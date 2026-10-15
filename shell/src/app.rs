//! Shell 层（2.1 节 应用装配）：无边框窗口、全局键位（7.2 节）、启动引导。

use gpui::{
    px, size, App, AppContext as _, Bounds, KeyBinding, WindowBounds, WindowKind, WindowOptions,
};
use gpui_component::TitleBar;

use rustshell_ui::main_view::{
    AppShell, MainView, RenameConnection, SwitchTab, TermShiftTab, TermTab,
};

#[derive(rust_embed::RustEmbed)]
#[folder = "assets"]
struct Embedded;

pub struct AppAssets;

impl gpui::AssetSource for AppAssets {
    fn load(&self, path: &str) -> anyhow::Result<Option<std::borrow::Cow<'static, [u8]>>> {
        Ok(Embedded::get(path).map(|f| f.data))
    }

    fn list(&self, path: &str) -> anyhow::Result<Vec<gpui::SharedString>> {
        let _ = path;
        Ok(Embedded::iter()
            .map(|p| p.as_ref().to_string().into())
            .collect())
    }
}

fn keybindings() -> Vec<KeyBinding> {
    use rustshell_platform::keymap as km;
    // M3：键位来自 默认表 + 用户覆盖（keymap.json），支持导入/导出
    // 绑定构建与运行时重绑定共用 rustshell_ui::keybinds::make_binding，避免两处漂移
    let mut bindings = Vec::new();
    for (id, keys) in km::effective(&rustshell_platform::paths::keymap_path()) {
        if let Some(b) = rustshell_ui::keybinds::make_binding(&id, &keys) {
            bindings.push(b);
        }
    }
    for i in 1..=9usize {
        bindings.push(KeyBinding::new(
            &km::bind(&i.to_string()),
            SwitchTab { index: i - 1 },
            None,
        ));
    }
    // Tab/Shift+Tab 终端补全：cx.bind_keys 在 gpui_component::init 之后调用，
    // 同深度后注册胜出（key_dispatch.rs），覆盖 Root 的焦点遍历绑定；
    // 处理器内判断焦点，终端持焦时写 PTY 并阻断，否则放行给默认焦点遍历。
    bindings.push(KeyBinding::new("tab", TermTab, None));
    bindings.push(KeyBinding::new("shift-tab", TermShiftTab, None));
    // F2：重命名选中连接（连接树）。固定键位而不进`keymap.json`默认表，
    // 是因为它只对连接树有意义，做成可重绑定项会与「全局快捷键」的语义混淆；
    // 终端的 F2 由处理器里的焦点判定让路（见 MainView 的 on_action）。
    bindings.push(KeyBinding::new("f2", RenameConnection, None));
    bindings
}

/// 登记主窗口的原生句柄（F-8）。
///
/// 「最小化到托盘」= 把窗口藏起来（`SW_HIDE`），而 gpui 没有暴露这个动作，
/// 只能拿原生句柄自己调 Win32（见 `rustshell_platform::window_ctl`）。
/// 句柄在这里取一次就够：窗口整场生命周期只有这一个，重复取反而会引入
/// 「谁持有最新的那份」这种没必要的状态。
fn register_main_window_handle(window: &gpui::Window) {
    #[cfg(windows)]
    {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        // 必须走**限定调用**：`Window` 自己有一个同名的固有方法
        // `window_handle() -> AnyWindowHandle`（那是 gpui 内部句柄），
        // 直接写 `window.window_handle()` 会选中它，拿不到原生 HWND。
        if let Ok(handle) = HasWindowHandle::window_handle(window) {
            if let RawWindowHandle::Win32(win32) = handle.as_raw() {
                rustshell_platform::window_ctl::set_main_hwnd(win32.hwnd.get());
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = window;
    }
}

/// 托盘菜单/提示的文案。
///
/// 语言在**安装时**定（`t()` 的结果是 `&'static str`，菜单项要 owned `String`）。
/// 也就是说运行中切换语言不会立刻改托盘菜单，要等下次启动 —— 托盘菜单是系统
/// 画的、不是 gpui 画的，为它做热切换要重建整个图标（会闪、还会丢通知区里的
/// 用户偏好），不值得。
fn tray_labels() -> rustshell_platform::tray::TrayLabels {
    use rustshell_core::i18n::t;
    rustshell_platform::tray::TrayLabels {
        // 提示文本的基名取 `APP_NAME`（产品名走常量，不进 i18n 词条表 ——
        // 它不翻译，而且 `t()` 的 key 必须有词条，否则 i18n 的 g1 门禁会报缺口）。
        // 会话数/断线数由 `ui` 侧在 poll 里补上（见 `main_view::tray_tooltip`）。
        tooltip: rustshell_core::APP_NAME.to_string(),
        show: t("显示主窗口").to_string(),
        // 2026-10-10：撤掉「新建终端」。终端入口在快捷键与工具栏上，
        // 托盘只留与「窗口」这件事直接相关的两条（见 `tray::ID_SHOW` 的文档）。
        quit: t("退出").to_string(),
    }
}

/// 关窗守卫（F-8）：开启「最小化到托盘」时，把关闭按钮的意思改成「收进托盘」。
///
/// 三条「如实放行关闭」的理由，缺一条都会让用户点了 X 之后**什么都看不到**：
/// ①托盘没建起来（收进去就再也叫不回来）；②开关没开（用户要的就是退出）；
/// ③窗口藏不起来（句柄没登记，藏了等于关不掉也找不回）。
///
/// 注意 `App::quit`（托盘菜单「退出」走的那条路）**不经过**这个回调 ——
/// 它直接 `PostQuitMessage`，所以退出路径不会被自己的守卫挡住。
fn wire_close_to_tray(window: &mut gpui::Window, cx: &mut App) {
    window.on_window_should_close(cx, |_window, _cx| {
        if !rustshell_platform::tray::is_ready() || !rustshell_platform::tray::close_to_tray() {
            return true;
        }
        // 藏成功 ⇒ 不许关（返回 false）；藏不起来 ⇒ 放行关闭。
        !rustshell_platform::window_ctl::set_visible(false)
    });
}

/// 装托盘图标（F-8）。失败**不静默** —— 设置面板会读到 `availability()` 并就地标注原因。
fn install_tray(settings: &rustshell_core::settings::Settings) {
    // 先把开关推给 platform：关窗守卫只有 `&mut App`，读不到设置快照。
    rustshell_platform::tray::set_close_to_tray(settings.close_to_tray);
    match rustshell_platform::tray::install(&tray_labels()) {
        Ok(()) => {}
        Err(reason) => {
            log::warn!("系统托盘不可用（{reason:?}）：关闭窗口仍按普通退出处理");
        }
    }
}

pub fn run() {
    // 运行日志最先初始化（panic hook 依赖它；启动打点见 logging::init）
    rustshell_platform::logging::init(log::LevelFilter::Info);
    // UI 线程现场自诊断（心跳 + 阶段打点）：窗口「未响应」时日志能直接定位阶段。
    // 见 ui/src/watchdog.rs 模块文档（：0xCFFFFFFF AppHangXProcB1）。
    rustshell_ui::watchdog::init();
    // 冷启动打点（M4 性能）：进程入口即记录，首帧渲染时打印耗时
    let _ = rustshell_ui::START.set(std::time::Instant::now());
    gpui_platform::application()
        .with_assets(AppAssets)
        .run(|cx: &mut App| {
            gpui_component::init(cx);
            // 主题由用户设置驱动（：settings.theme_mode）
            let settings = rustshell_core::settings::Settings::load(
                &rustshell_platform::paths::settings_path(),
            );
            rustshell_ui::theme::apply_theme_mode(cx, settings.theme_mode);
            // 界面语言（M4 国际化：zh/en）
            rustshell_core::i18n::set_lang_str(&settings.language);

            cx.bind_keys(keybindings());
            cx.activate(true);

            // 窗口尺寸：屏幕 85% 居中
            let mut window_size = size(px(1400.0), px(900.0));
            if let Some(display) = cx.primary_display() {
                let display_size = display.bounds().size;
                window_size.width = window_size.width.min(display_size.width * 0.85);
                window_size.height = window_size.height.min(display_size.height * 0.85);
            }
            let window_bounds = Bounds::centered(None, window_size, cx);

            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(window_bounds)),
                titlebar: Some(TitleBar::title_bar_options()),
                window_min_size: Some(gpui::Size {
                    width: px(720.),
                    height: px(480.),
                }),
                kind: WindowKind::Normal,
                #[cfg(target_os = "linux")]
                window_decorations: Some(gpui::WindowDecorations::Client),
                ..Default::default()
            };

            let _handle = cx.open_window(options, |window, cx| {
                // F-8：句柄与关窗守卫都必须在窗口建立时就位 —— 用户第一次点
                // 关闭按钮时它们就得已经生效，不能等某个后续初始化步骤。
                register_main_window_handle(window);
                wire_close_to_tray(window, cx);

                let view = cx.new(|cx| MainView::new(window, cx));
                // 外壳视图负责挂载官方 Dialog / Sheet 层。
                //`gpui_component::Root::render`并不挂这两层，必须由业务视图挂；
                // 且不能在 MainView 自身 render 里挂（render_dialog_layer 会同步执行
                // 对话框构建闭包，进而 update MainView 到 重入 panic）。
                let shell = cx.new(|_| AppShell::new(view));
                cx.new(|cx| gpui_component::Root::new(shell, window, cx))
            });

            // F-8 系统托盘：**必须在 `open_window` 之后**（托盘那个隐藏窗口要先有
            // 线程消息队列），且必须在消息循环**开始之前**（`run` 的这个初始化闭包
            // 正好就是那个窗口期 —— 循环一起来就没机会了，见 tray 模块文档）。
            install_tray(&settings);

            // 退出前把 AI 会话落盘（2026-09-23「AI对话需要保存」）。
            // 订阅读的 `detach()` 封在函数内部，这里不必（也不能）再持返回值。
            rustshell_ui::main_view::ui_install_flush_ai_chats_on_quit(cx);

            // F-9 会话恢复：退出前把标签布局落盘（节流最长 2s，刚拖完分屏
            // 就关窗的那次变化还在内存里）。同样已在函数内部 `detach()`。
            rustshell_ui::main_view::ui_install_flush_session_layout_on_quit(cx);

            // 启动动作（2026-09-28 起可配置，默认「打开本地终端」）。
            //
            // 历史：2026-09-17 的运行反馈是「打开软件默认不打开本地 shell，
            // 打开命令窗口」，于是启动被硬编码成永远只开命令面板 —— 那是用一个人
            // 的偏好覆盖所有人。现在按 `settings.startup_action` 分派：
            // 一个终端软件启动后给一个终端是最不需要解释的行为，想要命令面板 /
            // 什么都不做的人在设置里改一次（见 `StartupAction` 的文档）。
            //
            // 开浮层 / 建标签都要 `Window`，故这里只置标记或发起后台动作，
            // 真正的打开落在各自的首帧 `render` 里。
            rustshell_ui::main_view::ui_call_startup_action(settings.startup_action, cx);
            log::info!(
                "主窗口初始化完成（启动动作：{:?}）",
                settings.startup_action
            );
        });
}

#[cfg(test)]
mod tests {
    //! F-8 托盘文案 + 全局键位装配。
    //!
    //! 2026-10-12 覆盖度盘点发现：本文件 225 行**零测试**。这两处是真逻辑：
    //! `tray_labels` 决定用户看到的菜单文案（且带一条「托盘里不许放会改窗口内容
    //! 的入口」的业务约束），`keybindings` 决定全局快捷键能不能用。
    //!
    //! 注意 `KeyBinding` 的字段全是 `pub(crate)`（gpui-pre 0.3.5）⇒ 外部**读不到**
    //! 某条绑定的 action/keystrokes，只能断言「条数」与「构造不 panic」；
    //! 键位语义的真断言在 `ui/src/keybinds.rs` 与 `platform/src/keymap.rs` 的
    //! 单测里（各自能拿到公开字段），这里只钉**装配层不丢件**。

    /// 托盘只保留与「窗口/进程」直接相关的两条动作（2026-10-10 撤掉「新建终端」）。
    ///
    /// 这条是**业务约束**不是文案偏好：窗口正收在托盘里时，「新建终端」把新标签
    /// 开在看不见的地方，用户看到的是「点了没反应」。护栏盯的是
    /// `tray_labels()` 里**不出现**终端类入口的文案。
    #[test]
    fn tray_labels_carry_exactly_show_and_quit() {
        let labels = super::tray_labels();

        // 两条菜单项都必须有非空文案（空 = 托盘里出现一个无名项）
        assert!(!labels.show.is_empty(), "「显示主窗口」文案不能为空");
        assert!(!labels.quit.is_empty(), "「退出」文案不能为空");

        // 2026-10-10 撤掉「新建终端」：任何「新建/打开终端」类文案都不该再出现。
        for banned in ["新建终端", "新建本地", "New Terminal", "NewLocalShell"] {
            assert!(
                !labels.show.contains(banned) && !labels.quit.contains(banned),
                "托盘里不该再有「{banned}」—— 窗口正藏在托盘时它把结果放在看不见的地方：\
                 show={:?} quit={:?}",
                labels.show,
                labels.quit
            );
        }
    }

    /// 托盘提示文本的基名是产品名常量（不是 i18n 词条）。
    ///
    /// `t()` 的 key 必须在词条表里，否则 i18n 的 g1 门禁报缺口 ⇒ 产品名走常量。
    /// 这条钉住「别把它改成 t(APP_NAME)」这种写法。
    #[test]
    fn tray_tooltip_is_the_product_name_constant() {
        let labels = super::tray_labels();
        assert_eq!(
            labels.tooltip,
            rustshell_core::APP_NAME.to_string(),
            "托盘提示的基名应当就是 APP_NAME（会话数/断线数由 ui 侧在 poll 里补）"
        );
    }

    /// 键位装配**不丢件**：默认表全量 + Ctrl+1..9 九个标签切换 + tab/shift-tab/f2。
    ///
    /// 条数 = 默认表条数 + 9 + 3。少一条就意味着某个快捷键静默失效
    /// （界面上还印着键帽，按下去没反应）。
    #[test]
    fn keybindings_include_every_default_plus_the_extra_ones() {
        use rustshell_platform::keymap as km;

        let bindings = super::keybindings();
        let expected = km::DEFAULT_BINDINGS.len() + 9 + 3;
        assert_eq!(
            bindings.len(),
            expected,
            "键位条数不对：默认表 {} 条 + 标签切换 9 条 + tab/shift-tab/f2 三条 = {expected}，\
             实际 {}。少一条 = 某个快捷键静默失效（界面上还印着键帽，按下去没反应）。",
            km::DEFAULT_BINDINGS.len(),
            bindings.len()
        );
    }

    /// 重复装配不 panic、不产生重复条数（`run()` 与热重绑路径都可能再调一次）。
    #[test]
    fn keybindings_are_repeatable() {
        let first = super::keybindings().len();
        let second = super::keybindings().len();
        assert_eq!(
            first, second,
            "两次装配条数不一致：{first} vs {second}（装配过程有隐藏状态？）"
        );
    }
}
