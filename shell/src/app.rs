//! Shell 层（2.1 节 应用装配）：无边框窗口、全局键位（7.2 节）、启动引导。

use gpui::{
    point, px, size, App, AppContext as _, Bounds, KeyBinding, WindowBounds, WindowKind,
    WindowOptions,
};
use gpui_component::TitleBar;
use rustshell_core::window_bounds::{pick_screen, ScreenRect, WindowBoundsRecord};

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

fn keybindings(cx: &gpui::App) -> Vec<KeyBinding> {
    use rustshell_platform::keymap as km;
    // M3：键位来自 默认表 + 用户覆盖（keymap.json），支持导入/导出
    // 绑定构建与运行时重绑定共用 rustshell_ui::keybinds::make_binding，避免两处漂移
    let mut bindings = Vec::new();
    for (id, keys) in km::effective(&rustshell_platform::paths::keymap_path()) {
        if let Some(b) = rustshell_ui::keybinds::make_binding(&id, &keys, cx) {
            bindings.push(b);
        }
    }
    for i in 1..=9usize {
        // 键位串是运行时拼出来的（`mod-1` / `cmd-1`…），不是 `'static` 字面量，
        // 所以和 `make_binding` 一样走 `KeyBinding::load`（见该函数的注释）。
        if let Ok(b) = KeyBinding::load(
            &km::bind(&i.to_string()),
            Box::new(SwitchTab { index: i - 1 }),
            None,
            false,
            None,
            cx,
        ) {
            bindings.push(b);
        }
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

/// 一块显示器的**可见区域**（逻辑像素）：任务栏 / Dock / 菜单栏已排除。
///
/// 用 `visible_bounds` 而不是 `bounds`：拿整块屏当可用区，窗口会被摆到任务栏底下，
/// 而 Windows 上那一条拖不动 —— 用户只能靠快捷键把窗口救回来。
fn screen_rect_of(display: &dyn gpui::PlatformDisplay) -> ScreenRect {
    let b = display.visible_bounds();
    ScreenRect::new(
        f32::from(b.origin.x),
        f32::from(b.origin.y),
        f32::from(b.size.width),
        f32::from(b.size.height),
    )
}

/// 启动时的窗口几何。
///
/// 有记录 ⇒ 还原（挑重叠最大的那块屏，收进它的可见区）。没有记录、或者全屏都
/// 不相交 ⇒ 「1400×900、屏幕 85%、居中」那套默认行为原样保留。
///
/// 为什么要两侧都 `fitted_to`：记录可能来自一块已经拔掉的外接屏。挑不到屏时退回
/// 主屏而不是最左边那块，装的是一台笔记本时两者相同，外接屏一拔就未必。
fn resolve_window_bounds(cx: &mut App, saved: Option<WindowBoundsRecord>) -> WindowBounds {
    if let Some(rec) = saved {
        let screens: Vec<ScreenRect> = cx
            .displays()
            .iter()
            .map(|d| screen_rect_of(d.as_ref()))
            .collect();
        let home = pick_screen(&screens, &rec).or_else(|| {
            cx.primary_display()
                .map(|d| screen_rect_of(d.as_ref()))
                .or_else(|| screens.first().copied())
        });
        if let Some(screen) = home {
            let fitted = rec.fitted_to(screen);
            let bounds = Bounds::new(
                point(px(fitted.x), px(fitted.y)),
                size(px(fitted.width), px(fitted.height)),
            );
            log::info!(
                "窗口几何：按上次记录恢复（{} 屏）。{:.0},{:.0} {:.0}x{:.0}{}",
                screens.len(),
                fitted.x,
                fitted.y,
                fitted.width,
                fitted.height,
                if fitted.maximized { "，最大化" } else { "" }
            );
            return if fitted.maximized {
                // 最大化态带的是**还原尺寸**：取消最大化时窗口回到这个大小，
                // 而不是变成系统随手给的一个尺寸。
                WindowBounds::Maximized(bounds)
            } else {
                WindowBounds::Windowed(bounds)
            };
        }
        // 一块屏都问不到（无头会话、远程桌面断开）：往下走默认分支，
        // 那条路用的是 `Bounds::centered`，它自己也不会崩。
    }

    let mut window_size = size(px(1400.0), px(900.0));
    if let Some(display) = cx.primary_display() {
        let display_size = display.bounds().size;
        window_size.width = window_size.width.min(display_size.width * 0.85);
        window_size.height = window_size.height.min(display_size.height * 0.85);
    }
    WindowBounds::Windowed(Bounds::centered(None, window_size, cx))
}

pub fn run() {
    // 运行日志最先初始化（panic hook 依赖它；启动打点见 logging::init）
    rustshell_platform::logging::init(log::LevelFilter::Info);
    // UI 线程现场自诊断（心跳 + 阶段打点）：窗口「未响应」时日志能直接定位阶段。
    // 见 ui/src/watchdog.rs 模块文档（：0xCFFFFFFF AppHangXProcB1）。
    rustshell_ui::watchdog::init();
    // 冷启动打点（M4 性能）：进程入口即记录，首帧渲染时打印耗时
    let _ = rustshell_ui::START.set(std::time::Instant::now());

    // ★ 单实例：堡垒机（JumpServer 等）把 `ssh://` / `sftp://` 交给「已注册的客户端」时，
    // 用户往往已经开着一个 RustShell。后启动的进程把 argv 转交给已在运行的那个，
    // 自己直接退出 —— 绝不开第二个窗口、也不让两个实例各写一份配置。
    //
    // 本进程自己的参数也走同一条通道（`forward` → `requests`）：UI 只有一条
    // 「把参数变成连接」的路径，首次启动与后续转交行为完全一致。
    let args: Vec<String> = std::env::args().skip(1).collect();
    let requests = match rustshell_platform::single_instance::claim(&args) {
        rustshell_platform::single_instance::InstanceRole::Forwarded => return,
        rustshell_platform::single_instance::InstanceRole::Primary { requests, forward } => {
            if !args.is_empty() {
                let _ = forward.try_send(args);
            }
            requests
        }
    };

    gpui_platform::application()
        .with_assets(AppAssets)
        .run(|cx: &mut App| {
            gpui_component::init(cx);
            // 启动参数通道要在开窗之前装好：首轮 `poll` 才看得到它。
            rustshell_ui::main_view::ui_install_launch_requests(cx, requests);
            // 主题由用户设置驱动（：settings.theme_mode）
            let settings = rustshell_core::settings::Settings::load(
                &rustshell_platform::paths::settings_path(),
            );
            rustshell_ui::theme::apply_theme_mode(cx, settings.theme_mode);
            // 输入框光标节奏跟随「终端光标闪烁」那条偏好（gpui-kit 0.9.0 的
            // `Theme::cursor_blink`）。必须排在 apply_theme_mode 之后 —— 见该函数注释。
            rustshell_ui::theme::apply_cursor_blink(
                cx,
                settings.terminal_cursor_blink,
                settings.terminal_cursor_blink_ms,
            );
            // 界面语言（M4 国际化：zh/en）
            rustshell_core::i18n::set_lang_str(&settings.language);

            cx.bind_keys(keybindings(cx));
            cx.activate(true);

            // 窗口几何：有上次的记录就还原，没有就走「1400×900、屏幕 85%、居中」。
            let window_bounds = resolve_window_bounds(cx, settings.window_bounds);

            let options = WindowOptions {
                window_bounds: Some(window_bounds),
                titlebar: Some(TitleBar::title_bar_options()),
                window_min_size: Some(gpui::Size {
                    // 与 `core::window_bounds` 的同一个常量：恢复时按它钳尺寸，
                    // 系统按它限制拖拽 —— 两处各写一个数，窗口会「自己变大/变小」。
                    width: px(rustshell_core::window_bounds::MIN_WINDOW_WIDTH),
                    height: px(rustshell_core::window_bounds::MIN_WINDOW_HEIGHT),
                }),
                kind: WindowKind::Normal,
                #[cfg(target_os = "linux")]
                window_decorations: Some(gpui::WindowDecorations::Client),
                ..Default::default()
            };

            let _handle = cx.open_window(options, |window, cx| {
                let view = cx.new(|cx| MainView::new(window, cx));
                // 打开软件就检查更新（2026-11-05 用户要求）。
                //
                // 挂在这里而不是等首帧 `render`：`render` 每次重绘都会走一遍，把网络
                // 动作放在渲染路径上、只靠一个闩兜着太脆；启动点只有这一处，语义直白。
                // `update_auto_check` 自带「每进程只发起一次」的闩，首帧 `render` 里
                // 那次兜底调用撞上它会直接返回，不会发两次请求。
                view.update(cx, |v, cx| v.update_auto_check(cx));
                // 外壳视图负责挂载官方 Dialog / Sheet 层。
                //`gpui_component::Root::render`并不挂这两层，必须由业务视图挂；
                // 且不能在 MainView 自身 render 里挂（render_dialog_layer 会同步执行
                // 对话框构建闭包，进而 update MainView 到 重入 panic）。
                let shell = cx.new(|_| AppShell::new(view));
                cx.new(|cx| gpui_component::Root::new(shell, window, cx))
            });

            // 退出前把 AI 会话落盘（2026-09-23「AI对话需要保存」）。
            // 订阅读的 `detach()` 封在函数内部，这里不必（也不能）再持返回值。
            rustshell_ui::main_view::ui_install_flush_ai_chats_on_quit(cx);

            // F-9 会话恢复：退出前把标签布局落盘（节流最长 2s，刚拖完分屏
            // 就关窗的那次变化还在内存里）。同样已在函数内部 `detach()`。
            rustshell_ui::main_view::ui_install_flush_session_layout_on_quit(cx);

            // 窗口几何（位置 / 尺寸 / 最大化，2026-11-03）：单独一个钩子，不并进上面
            // 那条 —— 两者写同一份 `settings.json` 的不同节，合起来「谁失败了」就看不出来。
            rustshell_ui::main_view::ui_install_flush_window_bounds_on_quit(cx);

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
    //! 全局键位装配。
    //!
    //! 2026-10-12 覆盖度盘点发现：本文件 225 行**零测试**。`keybindings` 决定
    //! 全局快捷键能不能用，是真逻辑。
    //!
    //! 注意 `KeyBinding` 的字段全是 `pub(crate)`（gpui-pre 0.3.5）⇒ 外部**读不到**
    //! 某条绑定的 action/keystrokes，只能断言「条数」与「构造不 panic」；
    //! 键位语义的真断言在 `ui/src/keybinds.rs` 与 `platform/src/keymap.rs` 的
    //! 单测里（各自能拿到公开字段），这里只钉**装配层不丢件**。

    /// 键位装配**不丢件**：默认表全量 + Ctrl+1..9 九个标签切换 + tab/shift-tab/f2。
    ///
    /// 条数 = 默认表条数 + 9 + 3。少一条就意味着某个快捷键静默失效
    /// （界面上还印着键帽，按下去没反应）。
    ///
    /// 2026-11-06：装配要 `&App` 了（动态键位走 `KeyBinding::load`，见
    /// `rustshell_ui::keybinds::make_binding` 的注释），所以从 `#[test]` 换成
    /// `#[gpui::test]` 拿一个 TestAppContext —— 断言口径一个字没变。
    #[gpui::test]
    fn keybindings_include_every_default_plus_the_extra_ones(cx: &mut gpui::TestAppContext) {
        use rustshell_platform::keymap as km;

        let bindings = cx.update(|cx| super::keybindings(cx));
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
    #[gpui::test]
    fn keybindings_are_repeatable(cx: &mut gpui::TestAppContext) {
        let (first, second) =
            cx.update(|cx| (super::keybindings(cx).len(), super::keybindings(cx).len()));
        assert_eq!(
            first, second,
            "两次装配条数不一致：{first} vs {second}（装配过程有隐藏状态？）"
        );
    }

    /// 启动必须**真的**拿设置里的记录去还原窗口，而且必须经过 `fitted_to`。
    ///
    /// 两条都是「漏了不会报错、只有用户重启才看得出来」的那种退化：
    /// 不读记录 ⇒ 每次都是 1400×900（功能等于没做）；不 clamp ⇒ 拔掉外接屏之后
    /// 窗口落在屏幕外，用户看到的是「程序启动了，可什么都没出现」。
    ///
    /// 为什么只能源码级：`resolve_window_bounds` 要 `&mut App` 与真实显示器列表，
    /// 单测里没有屏；钳制逻辑本身在 `core::window_bounds` 已由 7 条纯函数用例覆盖，
    /// 这里钉的只是**装配层没把它绕过**。
    #[test]
    fn startup_window_bounds_are_restored_through_the_clamp() {
        let code = include_str!("app.rs");
        assert!(
            code.contains("resolve_window_bounds(cx, settings.window_bounds)"),
            "启动没把 `settings.window_bounds` 交给 `resolve_window_bounds` —— \
             记了也不会还原"
        );
        assert!(
            code.contains("fitted_to(screen)"),
            "恢复路径绕过了 `fitted_to` —— 拔掉外接屏后窗口会落在看不见的地方"
        );
        assert!(
            code.contains("visible_bounds()"),
            "必须用 `visible_bounds`（排除任务栏），否则窗口会压到任务栏底下拖不出来"
        );
    }
}
