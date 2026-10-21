//! TerminalView 薄适配层：隔离 rustshell-terminal 的 API 变动。

use gpui::{px, App, AppContext as _, Context, Edges, Entity};
use rustshell_core::session::{ResizeSink, SessionHandle};
use rustshell_core::settings::CursorStyle as SettingsCursorStyle;
use rustshell_core::theme::TerminalPalette;
use rustshell_terminal::{ColorPalette, CursorShape, TerminalConfig, TerminalView};

/// Token 到 rustshell-terminal 调色板（4.1 节 theme_map）
pub fn to_color_palette(p: &TerminalPalette) -> ColorPalette {
    fn ch(v: u32) -> (u8, u8, u8) {
        ((v >> 16) as u8, (v >> 8) as u8, v as u8)
    }
    let bg = ch(p.background);
    let fg = ch(p.foreground);
    let cur = ch(p.cursor);
    let sel = ch(p.cursor); // 选区高亮用 accent（=cursor 色）+ 45% alpha
    let mut b = ColorPalette::builder()
        .background(bg.0, bg.1, bg.2)
        .foreground(fg.0, fg.1, fg.2)
        .cursor(cur.0, cur.1, cur.2)
        .selection(sel.0, sel.1, sel.2, 0.45);
    // builder 方法名固定，逐个显式调用
    let n = p.normal;
    b = b.black(ch(n[0]).0, ch(n[0]).1, ch(n[0]).2);
    b = b.red(ch(n[1]).0, ch(n[1]).1, ch(n[1]).2);
    b = b.green(ch(n[2]).0, ch(n[2]).1, ch(n[2]).2);
    b = b.yellow(ch(n[3]).0, ch(n[3]).1, ch(n[3]).2);
    b = b.blue(ch(n[4]).0, ch(n[4]).1, ch(n[4]).2);
    b = b.magenta(ch(n[5]).0, ch(n[5]).1, ch(n[5]).2);
    b = b.cyan(ch(n[6]).0, ch(n[6]).1, ch(n[6]).2);
    b = b.white(ch(n[7]).0, ch(n[7]).1, ch(n[7]).2);
    let h = p.bright;
    b = b.bright_black(ch(h[0]).0, ch(h[0]).1, ch(h[0]).2);
    b = b.bright_red(ch(h[1]).0, ch(h[1]).1, ch(h[1]).2);
    b = b.bright_green(ch(h[2]).0, ch(h[2]).1, ch(h[2]).2);
    b = b.bright_yellow(ch(h[3]).0, ch(h[3]).1, ch(h[3]).2);
    b = b.bright_blue(ch(h[4]).0, ch(h[4]).1, ch(h[4]).2);
    b = b.bright_magenta(ch(h[5]).0, ch(h[5]).1, ch(h[5]).2);
    b = b.bright_cyan(ch(h[6]).0, ch(h[6]).1, ch(h[6]).2);
    b = b.bright_white(ch(h[7]).0, ch(h[7]).1, ch(h[7]).2);
    b.build()
}

pub struct TerminalPaneConfig {
    pub font_family: String,
    pub font_size: f32,
    /// 行高倍数（设置项）：`1.0` = 紧贴，默认 `1.2`。
    /// 2026-09-28 起由设置驱动，此前硬编码在 `create_terminal_view` 里。
    pub line_height: f32,
    pub scrollback: usize,
    pub palette: TerminalPalette,
    /// T4-E1：PTY 输出字节的源编码 key（`"utf-8"`= 直通；其他经后台转码）。
    pub encoding: String,
    /// 终端默认底色的 alpha（1.0 = 不透明）。
    pub background_alpha: f32,
    /// 光标闪烁总开关（设置项）。关闭后常亮，定时器不启动。
    pub cursor_blink: bool,
    /// 光标闪烁半周期，毫秒（设置项）。
    pub cursor_blink_ms: u64,
    /// 默认光标形状（设置项）。远端 DECSCUSR 仍可临时覆盖。
    pub cursor_style: SettingsCursorStyle,
    /// 粘贴时把行尾统一为回车（设置项）：多行粘贴即逐行执行。
    pub paste_normalize_newlines: bool,
    /// 多行粘贴确认阈值（设置项，F-5）：`None` = 保护关闭；`Some(n)` = 行数超过 n
    /// 时先在终端里挂起、由宿主弹确认框。
    ///
    /// 2026-09-28 起这两个值由**同一个** `Settings::terminal_paste_behavior`
    /// 一次性算出（见 `PasteBehavior::normalize_newlines` / `protect_lines`）——
    /// 此前它们来自两个独立开关，能配出「先确认 + 逐行执行」这种自相矛盾的组合。
    pub paste_protect_lines: Option<u32>,
}

/// 设置层的光标形状 → 引擎层的 `CursorShape`。
///
/// 一一对应，无「HollowBlock」（空心块）档：那是 alacritty 给 `ESC[2 q` 的
/// 呈现方式，不是用户该在设置里挑的东西。
pub fn to_cursor_shape(s: SettingsCursorStyle) -> CursorShape {
    match s {
        SettingsCursorStyle::Block => CursorShape::Block,
        SettingsCursorStyle::Bar => CursorShape::Beam,
        SettingsCursorStyle::Underline => CursorShape::Underline,
    }
}

/// 调色板 + 底色 alpha：`alpha >= 1.0` 时与`to_color_palette`完全等价。
pub fn to_color_palette_a(p: &TerminalPalette, alpha: f32) -> ColorPalette {
    let c = to_color_palette(p);
    if alpha < 1.0 {
        c.with_background_alpha(alpha)
    } else {
        c
    }
}

/// 由会话句柄创建终端视图实体。writer/reader 从句柄中 move 出去。
pub fn create_terminal_view(
    session: &mut SessionHandle,
    cfg: &TerminalPaneConfig,
    resize_sink: ResizeSink,
    on_exit: impl Fn(&mut gpui::Window, &mut Context<TerminalView>) + 'static,
    cx: &mut App,
) -> Entity<TerminalView> {
    let writer = session.writer.take().expect("session writer already taken");
    let reader = session.reader.take().expect("session reader already taken");

    let config = TerminalConfig {
        font_family: cfg.font_family.clone(),
        font_size: px(cfg.font_size),
        cols: 80,
        rows: 24,
        scrollback: cfg.scrollback,
        // 设置项（2026-09-28 起由 `Settings::clamped_line_height` 提供；
        // 此前是硬编码 1.2，用户无处可调）
        line_height_multiplier: cfg.line_height,
        // 顶部收窄到 4px：终端背景与页签栏之间不再叠出一条过高的同色空白带；
        // 左右和底部保持 8px，给光标、划选与滚动条留边。
        padding: Edges {
            top: px(4.0),
            right: px(8.0),
            bottom: px(8.0),
            left: px(8.0),
        },
        colors: to_color_palette_a(&cfg.palette, cfg.background_alpha),
        // T4-E1：编码透传（引擎侧在后台 reader 线程按此转码）
        encoding: Some(cfg.encoding.clone()),
        // 光标闪烁与粘贴换行策略（设置项）
        cursor_blink: cfg.cursor_blink,
        cursor_blink_interval_ms: cfg.cursor_blink_ms,
        default_cursor_shape: to_cursor_shape(cfg.cursor_style),
        paste_normalize_newlines: cfg.paste_normalize_newlines,
        // 多行粘贴确认（由 `PasteBehavior` 一次性算出的两个值，见 TerminalPaneConfig 文档）
        paste_protect_lines: cfg.paste_protect_lines,
    };

    cx.new(|cx| {
        TerminalView::new(writer, reader, config, cx)
            .with_resize_callback(move |cols: usize, rows: usize| {
                // 本回调运行在 UI 线程（引擎 canvas 绘制闭包）。
                // 阶段打点用于事后定位「未响应」；`ResizeSink::Local`已保证
                // 只入队不阻塞（见 core/src/session.rs 的 PtyResizer）。
                crate::watchdog::enter(crate::watchdog::Phase::PtyResize);
                resize_sink.resize(cols as u16, rows as u16);
                crate::watchdog::leave();
            })
            .with_exit_callback(on_exit)
            // OSC 52：远端程序请求「把这段文本放进本地剪贴板」。
            // 系统剪贴板由宿主持有（引擎内不含剪贴板实现、无原生依赖）。
            .with_clipboard_store_callback(|_window, cx, text| {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(text.to_string()));
            })
            // OSC 52 读取：把本地剪贴板文本交给引擎，应答序列由 alacritty 的
            // 格式化器拼装后写回 PTY。非文本内容返回 None（不应答）。
            .with_clipboard_load_callback(|_window, cx| {
                cx.read_from_clipboard().and_then(|item| item.text())
            })
            // C-1 补全浮层：两处被动响应（浮层不抢焦点，按键仍归终端）。
            //
            // 三条回调的第一个参数都是引擎**借出的自己**（`&TerminalView`）——
            // 这三处兑现点全在 `TerminalView` 的租约里（`render` / `on_key_down`），
            // 宿主拿 `Entity<TerminalView>` 去 `read` / `update` 会直接 panic
            // （见 `rustshell_terminal::InputLineCallback` 的文档）。
            //
            // ① 「当前输入行可能变了」：IME 上屏（`commit_text`）与粘贴
            //    （`paste_text`）之后各报一次。浮层没开时宿主自己短路掉，不走实体调度。
            .with_input_line_callback(|tv, window, cx| {
                crate::main_view::ui_call_completion_refresh(tv, window, cx);
            })
            // ② 补全相关的控制键：引擎已把可打印键全部放行（否则浮层一开就打不了字），
            //    进到这里的只有控制键。浮层关着时宿主一律回 `PassThrough`。
            .with_completion_key_filter(|event, tv, _window, cx| {
                let k = &event.keystroke;
                crate::main_view::ui_call_completion_key(&k.key, k.modifiers.control, tv, cx)
                    // 宿主已经没了（窗口关闭中）：没有宿主就没有浮层，按键归终端
                    .unwrap_or(rustshell_terminal::CompletionKeyOutcome::PassThrough)
            })
            // ③ 「用户按了回车」：命令历史采集（C-1 的第 ② 号数据源）。
            //    这条路**不需要远端配 shell integration**，是所有会话都能走的那条。
            .with_enter_callback(|tv, window, cx| {
                crate::main_view::ui_call_completion_enter(tv, window, cx);
            })
    })
}

/// 把**新会话**的 I/O 接到既有终端视图上 —— 重连（远端注销 / 空闲超时 / 网络中断之后）
/// 的唯一落点。
///
/// 与 [`create_terminal_view`] 的分工只有一句话：那边建一块**新**屏，这边复用**原来那块**。
/// 断连现场（最后几行输出、报错文本、光标停在哪）留在原地，上方接一条分隔行，
/// 下面是新会话的提示符 —— 用户能自己对比「断在哪、重连后长什么样」。
/// 把画面换成一块干净的新屏，等于把证据删了。
///
/// `notice` 由调用方给文本（i18n 在 core，引擎与适配层都不掺文案）；传 `None` 只留空行。
pub fn reattach_terminal_view(
    view: &Entity<TerminalView>,
    session: &mut SessionHandle,
    notice: Option<&str>,
    cx: &mut App,
) {
    // writer/reader 与建新视图时同口径地从句柄里 move 走：句柄此后只剩「会话身份」，
    // 真实读写在终端视图手里（这也是 `SessionHandle` 上那两个字段总是 `None` 的原因）。
    let writer = session.writer.take().expect("session writer already taken");
    let reader = session.reader.take().expect("session reader already taken");
    let sink = session.resize.clone();
    view.update(cx, |v, cx| {
        v.reattach(
            writer,
            reader,
            // 尺寸回调改指**新**会话的 PTY：旧会话的 resize 槽已经跟着它一起死了。
            // 阶段打点与 `create_terminal_view` 里那份保持一致（同一段代码路径）。
            Some(Box::new(move |cols: usize, rows: usize| {
                crate::watchdog::enter(crate::watchdog::Phase::PtyResize);
                sink.resize(cols as u16, rows as u16);
                crate::watchdog::leave();
            })),
            notice,
            cx,
        );
    });
}

#[cfg(test)]
mod tests {
    use crate::test_support::prod;

    /// 记录写进 PTY 的全部字节（与 `ime_wiring` / `dead_session` 的 `SpySink` 同款）。
    #[derive(Clone, Default)]
    struct SpySink(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl SpySink {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap_or_else(|e| e.into_inner())).to_string()
        }
    }

    impl std::io::Write for SpySink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// 永不返回的 reader。
    ///
    /// 见 `ui/src/dead_session.rs::blocking_feed_receiver` 的说明：换
    /// `std::io::empty()` 会立刻 EOF，把刚建好的视图卷进「进程已退出」支路，
    /// 而且读线程一旦结束就会**从后台线程**唤醒 GPUI 任务，撞上 `gpui::test`
    /// 的「调度器只能在测试线程上活动」检查 —— 症状是「所有断言都过了却 FAILED」。
    /// 发送端钉在静态里永不释放，读线程就一直阻塞。
    static BLOCKING_FEED: std::sync::Mutex<Vec<std::sync::mpsc::Sender<Vec<u8>>>> =
        std::sync::Mutex::new(Vec::new());

    struct BlockingReader(std::sync::mpsc::Receiver<Vec<u8>>);

    impl std::io::Read for BlockingReader {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            let _ = self.0.recv();
            Ok(0)
        }
    }

    fn blocking_reader() -> BlockingReader {
        let (tx, rx) = std::sync::mpsc::channel();
        // 池子必须收下**每一个** `tx`（`OnceLock` 只留第一个 = 从第二次调用起静默 EOF）
        BLOCKING_FEED
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(tx);
        BlockingReader(rx)
    }

    /// 造一个「终端视图当根视图」的真窗口并把焦点给它（同 `ime_wiring` 的手法：
    /// 焦点要**先**给、再走一帧，否则处理器装不上，按键断言会以「什么都没收到」假绿）。
    ///
    /// `filter` 在 `TerminalView::new()` 时链上：`with_completion_key_filter` 是
    /// **消费型 builder**（`self` 进、`Self` 出），只在建视图那一刻能装 ——
    /// 这与生产路径完全一致（`create_terminal_view` 也是在那里链的）。
    fn focused_terminal_window_with_filter(
        cx: &mut gpui::TestAppContext,
        sink: SpySink,
        filter: impl Fn(
                &gpui::KeyDownEvent,
                &rustshell_terminal::TerminalView,
                &mut gpui::Window,
                &mut gpui::Context<rustshell_terminal::TerminalView>,
            ) -> rustshell_terminal::CompletionKeyOutcome
            + 'static,
    ) -> (
        gpui::Entity<rustshell_terminal::TerminalView>,
        &mut gpui::VisualTestContext,
    ) {
        let (view, vcx) = cx.add_window_view(move |_window, cx| {
            rustshell_terminal::TerminalView::new(
                sink,
                blocking_reader(),
                rustshell_terminal::TerminalConfig::default(),
                cx,
            )
            .with_completion_key_filter(filter)
        });
        vcx.run_until_parked();
        vcx.update(|window, cx| {
            let handle = view.read(cx).focus_handle().clone();
            window.focus(&handle, cx);
        });
        vcx.run_until_parked();
        assert!(
            vcx.update(|window, cx| view.read(cx).focus_handle().is_focused(window)),
            "前提不成立：焦点没落在终端上 —— 后面所有按键断言都会以「什么都没收到」假绿"
        );
        (view, vcx)
    }

    /// ★ C-1 的**核心契约**：补全浮层的三种处置下，哪些键会变成字节写进 PTY。
    ///
    /// # 这条用例挡的是什么
    ///
    /// 补全浮层把 `↑↓ / Tab / Enter / Esc / Ctrl+Space` 从终端手里拿走了。
    /// 拿走的代价是：**任何一处漏放行，终端就少一个键**，而这类回归在
    /// 「编译通过 + 单测全绿」下完全不可见 —— 要等用户实机按到那个键才发现。
    /// 三条最危险的：
    ///
    /// 1. **`Ctrl+Space` 绝不能写 `\x00`**。`keystroke_to_bytes` 把
    ///    Ctrl+Space 译成 NUL（那是终端协议里的正确映射），浮层开着时若忘了
    ///    拦，远端 shell 每开一次浮层就收到一个垃圾字节。
    /// 2. **浮层关着时 `Tab` / `↑` / `Esc` 必须原样透传**。它们是终端天天要用的
    ///    键（远端补全、历史回溯、vim 退出）；§5.3 的「不抢 Tab」指的正是这一态。
    /// 3. **浮层开着时可打印键仍要放行**。截住它等于「浮层一开终端就打不了字」
    ///    （铁律 #63）—— 而打字正是过滤候选的输入源。
    ///
    /// 断言取**全等**而不是 `contains`：`contains` 挡不住「多写了一个字节」
    /// （双发），而那正是第 1 条要防的。
    #[gpui::test]
    fn the_completion_filter_decides_exactly_which_keys_reach_the_pty(
        cx: &mut gpui::TestAppContext,
    ) {
        use rustshell_terminal::CompletionKeyOutcome as Out;
        use std::sync::atomic::{AtomicBool, Ordering};

        let sink = SpySink::default();

        // 用 `AtomicBool` 当「浮层开着吗」：真实的判据在 `MainView::completion` 上，
        // 但这里测的是**引擎与过滤器的契约**（谁被拦、谁被放行），
        // 拉真 `MainView` 进来会引入一大堆与本题无关的依赖。
        let open = std::sync::Arc::new(AtomicBool::new(false));
        let open_for_filter = open.clone();
        let (_view, vcx) =
            focused_terminal_window_with_filter(cx, sink.clone(), move |event, _tv, _w, _cx| {
                let k = &event.keystroke;
                if k.modifiers.control && k.key == "space" {
                    // Ctrl+Space **无条件**切浮层（两种状态都做事）
                    let now = !open_for_filter.load(Ordering::Relaxed);
                    open_for_filter.store(now, Ordering::Relaxed);
                    return Out::ToggleRequested;
                }
                if !open_for_filter.load(Ordering::Relaxed) {
                    return Out::PassThrough;
                }
                match k.key.as_str() {
                    "up" | "down" | "tab" | "enter" | "escape" => Out::Consumed,
                    _ => Out::PassThrough,
                }
            });

        // ① 浮层关着：终端该收的键一个不少。
        //
        //    这是「不抢 Tab」的可执行版本 —— `tab` 必须变成 `\t`（远端补全靠它），
        //    `up` 必须变成光标上移序列，`escape` 必须变成 `\x1b`（vim 靠它）。
        vcx.simulate_keystrokes("tab");
        assert_eq!(
            sink.text(),
            "\t",
            "浮层关着时 Tab 没有透传：远端 shell 补全收不到键（§5.3 的硬约束）"
        );
        vcx.simulate_keystrokes("escape");
        assert_eq!(sink.text(), "\t\x1b", "浮层关着时 Esc 没有透传");
        vcx.simulate_keystrokes("up");
        assert_eq!(
            sink.text(),
            "\t\x1b\x1b[A",
            "浮层关着时 ↑ 没有透传：远端历史回溯失效"
        );

        // ② 开浮层：Ctrl+Space 被吃掉，**且一个字节都不进管道**。
        //
        //    ★ 这一条是本用例存在的首要理由：没有它，`\x00` 会静默流到远端。
        vcx.simulate_keystrokes("ctrl-space");
        assert_eq!(
            sink.text(),
            "\t\x1b\x1b[A",
            "Ctrl+Space 写了字节（`\\x00`）进 PTY：远端 shell 收到垃圾输入。\
             预期是「开浮层」这个动作本身**零字节**"
        );
        assert!(
            open.load(Ordering::Relaxed),
            "Ctrl+Space 没有把浮层切到打开态"
        );

        // ③ 浮层开着：被拿走的键一个字节都不写。
        vcx.simulate_keystrokes("tab");
        vcx.simulate_keystrokes("up");
        vcx.simulate_keystrokes("enter");
        vcx.simulate_keystrokes("escape");
        assert_eq!(
            sink.text(),
            "\t\x1b\x1b[A",
            "浮层开着时 ↑↓/Tab/Enter/Esc 仍被写进了 PTY：这些键该被浮层吃掉"
        );

        // ④ 浮层开着：**可打印键必须照常进去**（铁律 #63）。
        //
        //    截住它 = 浮层一开终端就打不了字，而打字正是过滤候选的输入源。
        vcx.simulate_keystrokes("g");
        assert_eq!(
            sink.text(),
            "\t\x1b\x1b[Ag",
            "浮层开着时可打印键被拦住了：用户没法继续打字，候选也就永远不会变"
        );

        // ⑤ 再按一次 Ctrl+Space：关浮层，依然零字节。
        vcx.simulate_keystrokes("ctrl-space");
        assert_eq!(sink.text(), "\t\x1b\x1b[Ag", "关浮层那一下写了字节进 PTY");
        assert!(
            !open.load(Ordering::Relaxed),
            "Ctrl+Space 没有把浮层切回关闭态"
        );

        // ⑥ 关掉之后终端恢复原样：Tab 又能用了。
        //    没有这一步，「关浮层」和「浮层一直开着」这两种实现都过得了 ①②③④。
        vcx.simulate_keystrokes("tab");
        assert_eq!(
            sink.text(),
            "\t\x1b\x1b[Ag\t",
            "关掉浮层后 Tab 没有恢复透传：浮层关不干净"
        );
    }

    /// C-1 接线护栏：补全浮层的**三条引擎回调**都必须在 `create_terminal_view`
    /// 里装上，且都接到 `main_view` 的宿入口上。
    ///
    /// # 为什么必须扫源码而不是跑单测
    ///
    /// 这三条接线是「装配」而不是「逻辑」：漏装任何一条，**编译照样通过**，
    /// 单测（测的是 `candidates` / `CompletionState` 这些纯逻辑）也照样绿 ——
    /// 症状要等到用户实机操作时才显形，而且很隐蔽：
    ///
    /// * 漏 `with_input_line_callback` ⇒ 打字时候选不更新（浮层像死的）；
    /// * 漏 `with_completion_key_filter` ⇒ Ctrl+Space 没反应，且 `\x00` 被写进 PTY
    ///   （远端 shell 收到一个 NUL，`bash` 下会响铃或什么都不做，极难归因）；
    /// * 漏 `with_enter_callback` ⇒ 命令历史永远是空的，而「历史」这一栏
    ///   在浮层里只表现为"就是不出现"，不像报错那样显眼。
    ///
    /// 铁律 #51：「已完成」清单不是证据，必须回代码找**拦截点本体** ——
    /// 本用例找的就是那三个装配调用点。
    #[test]
    fn completion_callbacks_are_wired_in_create_terminal_view() {
        let src = prod(include_str!("terminal_view.rs"));

        // 断言钉在 `create_terminal_view` 的函数体里（而不是整文件：
        // `reattach_terminal_view` 也在同文件，整文件级 `contains` 会让
        // 「只装在 reattach 上」也算通过 —— 而那条路只在**重连**时走，
        // 常规路径（首次连接）根本没有回调）。
        //
        // 终点取**下一个函数的签名**而不是 `"\n}\n"`：后者会停在函数体里
        // 第一个嵌套块的收尾处（`cx.new(|cx| { ... })` 那个闭包），
        // 切片只剩前半截 —— 三条回调恰好都装在那半个闭包里，
        // 护栏会以「找不到」的形式假红（铁律 #67 的同一类坑）。
        let body = crate::test_support::region(
            &src,
            "pub fn create_terminal_view(",
            "\npub fn reattach_terminal_view(",
        );

        for (needle, why) in [
            (
                ".with_input_line_callback(",
                "打字 / 粘贴后浮层收不到「行变了」，候选永远停在打开那一刻",
            ),
            (
                ".with_completion_key_filter(",
                "Ctrl+Space 不会开浮层，且 NUL(`\\x00`) 会被写进 PTY",
            ),
            (
                ".with_enter_callback(",
                "命令历史永远采不到，浮层的「历史」栏恒为空",
            ),
        ] {
            assert!(
                body.contains(needle),
                "`create_terminal_view` 里少了 {needle}：{why}。\n片段：{body}"
            );
        }

        // 再钉一层：三条都必须落在 `main_view::ui_call_*` 上（而不是就地写一个
        // 空闭包）。`{}` 这种空实现能过上面那条断言，但浮层一个字都不会收到。
        for needle in [
            "ui_call_completion_refresh(",
            "ui_call_completion_key(",
            "ui_call_completion_enter(",
        ] {
            assert!(
                body.contains(needle),
                "`create_terminal_view` 里的回调没有接到 {needle} 上（空实现能过编译）：\n{body}"
            );
        }
    }

    /// 新建终端**不许凭空长出 scrollback**。
    ///
    /// 终端是 80×24 建出来的（宿主 `create_terminal_view` 的口径），首帧 canvas 会把
    /// 它撑到窗格尺寸。alacritty 的 `Grid::grow_lines` 在「没有历史」时把空白加在
    /// 下方、在有历史时把内容拉到底部 —— 所以首帧那次撑高**如果**往 scrollback 里
    /// 塞进空白行，后果是双份的：
    ///
    /// 1. 滚动条在一个什么都没打印过的终端上就出现（`history_size > 0` 正是它的
    ///    显示条件，见 `TermScrollMetrics::history_size` 的文档）；
    /// 2. 之后每次窗口变高都会按 `min(history, 新增行数)` 把内容整体往下拖 ——
    ///    这正是 2026-10-09「默认态提示行到不了底、全屏可以」的候选机制之一。
    ///
    /// 实测 alacritty 0.25.1 不会（撑高只动可见区，`decrease_scroll_limit` 把那
    /// 一截的额度收掉了 ⇒ `history_size` 保持 0）。这条护栏钉住这个前提：换引擎
    /// 版本或改 fit 顺序时若退化，这里立刻红。
    #[gpui::test]
    fn a_fresh_terminal_does_not_grow_phantom_scrollback(cx: &mut gpui::TestAppContext) {
        let sink = SpySink::default();
        let (view, vcx) = focused_terminal_window_with_filter(cx, sink, |_, _, _, _| {
            rustshell_terminal::CompletionKeyOutcome::PassThrough
        });

        // 首帧：canvas 的 fit 就发生在 paint 里
        vcx.update(|window, cx| {
            window.draw(cx).clear(cx);
        });
        let m = vcx.update(|_w, cx| view.read(cx).scroll_metrics());

        // 前提：首帧**确实**把 80×24 撑到了窗格尺寸（没撑高就测不到这次 fit 的副作用）
        assert!(
            m.screen_lines > 24,
            "前提不成立：首帧没有把 80×24 撑到窗格尺寸（screen_lines={}），\
             下面的断言会因为「压根没发生这次撑高」而假绿。",
            m.screen_lines
        );
        assert_eq!(
            m.history_size, 0,
            "一个什么都没打印过的终端长出了 {} 行 scrollback（screen_lines={}）：\
             滚动条会凭空出现在空终端上，而且之后每次窗口变高都会把内容往下拖。",
            m.history_size, m.screen_lines
        );
        assert_eq!(m.display_offset, 0, "空终端不该处于回滚状态");
    }

    /// 一次「网格落点」采样：`(视口行数, 历史行数, MARKER 在第几行, MARKER 下方还有几行空白)`。
    ///
    /// `visible_text()` 给的是**整个视口**（空行也在里面，只有行尾空白被修掉），
    /// 所以「MARKER 在第几行」就是「提示行离顶边多远」，`text_lines − marker − 1`
    /// 就是「提示行离底边多远」。
    fn grid_landing(
        view: &gpui::Entity<rustshell_terminal::TerminalView>,
        cx: &mut gpui::TestAppContext,
    ) -> (usize, usize, i32, i32) {
        let (text, m) = cx.update(|cx| {
            let v = view.read(cx);
            (v.visible_text(), v.scroll_metrics())
        });
        let lines: Vec<&str> = text.lines().collect();
        let marker = lines
            .iter()
            .position(|l| l.contains("MARKER"))
            .expect("视口里找不到 MARKER —— 前提不成立，后面的行号断言无意义")
            as i32;
        (
            m.screen_lines,
            m.history_size,
            marker,
            lines.len() as i32 - marker - 1,
        )
    }

    /// 撑高网格**只会把空白加在内容下方**；有历史时才把历史拉回视口。
    ///
    /// # 这条护栏回答的是用户 2026-10-09 的追问
    ///
    /// 「为什么默认状态的终端输入栏到不了底部，全屏可以？」
    ///
    /// 实测（真终端、真投喂、alacritty 0.25.1，量 `screen_lines` / `history_size` /
    /// MARKER 那行的行号）：
    ///
    /// ```text
    /// A1  45 行 / 1 行内容（无历史）  screen=45 hist=0  marker_at=0   下方空白=43
    /// A2  撑到 78 行                 screen=78 hist=0  marker_at=0   下方空白=76
    /// A3  缩回 45 行                 screen=45 hist=0  marker_at=0   下方空白=43
    /// A4  再撑到 78 行               screen=78 hist=0  marker_at=0   下方空白=76
    /// B1  45 行 / 61 行输出（历史）  screen=45 hist=17 marker_at=43  下方空白=0
    /// B2  撑到 78 行                 screen=78 hist=0  marker_at=60  下方空白=16
    /// ```
    ///
    /// （A4 = A2，说明「切过去再切回来」是幂等的；下面只断言 A1/A2/A3/B1/B2 五处，
    /// 表里的 A4 是测量时的原始记录，留着是为了以后比对引擎行为。）
    ///
    /// 读法（**这就是「到不了底」的全部机制**）：
    ///
    /// * **没有 scrollback 时，撑高只往下加空白行**（A1→A2：`marker_at` 不动，
    ///   下方空白 43→76 = 43 + 33 新增行）。alacritty 的原话是 "Once scrollback is
    ///   exhausted, new lines are simply added to the bottom of the screen"
    ///   （`grid/resize.rs::grow_lines` 的文档）。屏幕比打印内容高多少，
    ///   提示行就离底边多远 —— 这是**终端本来的行为**，不是布局坏了。
    /// * **窗口越大，这段空白越长**（A1 43 行 → A2 76 行；实机 1400×900 与
    ///   1920×1080 之间差 15 行左右）。所以「全屏时提示行更靠底」在物理上不成立，
    ///   除非屏幕上本来就打印了那么多行。
    /// * **有历史时才把内容往下拉**（B1→B2：`marker_at` 43→60 = 43 + `min(17, 33)`，
    ///   历史同时被拉空 ⇒ `hist` 17→0）—— 因为可回滚的那部分重新进了视口，
    ///   整屏看起来「满了」，用户在读的是 backlog，不是「提示行贴到底边」。
    /// * **缩矮不留幽灵历史**（A2→A3：`hist` 仍是 0）：`shrink_lines` 把它旋出去的那
    ///   一截交回额度，所以空终端来回全屏不会凭空长出滚动条。
    ///
    /// 谁改动了「撑高之后内容停在哪一行」（换引擎、改 fit 顺序、把余量折进内边距），
    /// 这里立刻红。
    #[gpui::test]
    fn growing_the_grid_only_ever_adds_blank_screen_below_the_content(
        cx: &mut gpui::TestAppContext,
    ) {
        use crate::dead_session::{
            allow_real_io, feed_reader, pump_until_parsed, FeedHandle, SpySink,
        };
        use gpui::AppContext as _;

        // 读端是真线程（投喂句柄每场景一套）⇒ 必然跨线程唤醒调度器
        allow_real_io(cx);
        cx.update(gpui_component::init);

        // 建一块 80×`rows` 的屏（窗口态 / 全屏态的实测行数就落在这个量级）
        let build = |cx: &mut gpui::TestAppContext,
                     rows: usize|
         -> (gpui::Entity<rustshell_terminal::TerminalView>, FeedHandle) {
            let (reader, feed) = feed_reader();
            let view = cx.update(|cx| {
                cx.new(|cx| {
                    rustshell_terminal::TerminalView::new(
                        SpySink::default(),
                        reader,
                        rustshell_terminal::TerminalConfig::default(),
                        cx,
                    )
                })
            });
            view.update(cx, |v, _cx| v.resize(80, rows));
            cx.run_until_parked();
            (view, feed)
        };
        let wait = |view: &gpui::Entity<rustshell_terminal::TerminalView>,
                    cx: &mut gpui::TestAppContext| {
            pump_until_parsed(cx, "MARKER 上屏", |cx| {
                cx.update(|cx| view.read(cx).visible_text().contains("MARKER"))
            });
        };

        // ---- A：内容比屏幕短（刚连上、只有提示符）---------------------------------
        // 窗口态 45 行 —— 建屏尺寸就取这个，全屏撑到 78
        let (view, feed) = build(cx, 45);
        feed.feed(b"MARKER\r\n");
        wait(&view, cx);

        let a1 = grid_landing(&view, cx);
        assert_eq!(
            (a1.0, a1.1, a1.2),
            (45, 0, 0),
            "前提不成立：45 行屏、1 行内容应当 screen=45 / hist=0 / marker 在第 0 行，实测 {a1:?}"
        );
        assert_eq!(
            a1.3, 43,
            "45 行屏就 1 行内容，marker 下方应当正好 43 行空白（= 45 − 1 − 1），实测 {a1:?}"
        );

        view.update(cx, |v, _cx| v.resize(80, 78));
        cx.run_until_parked();
        let a2 = grid_landing(&view, cx);
        assert_eq!(
            a2.0, 78,
            "撑高没有生效（screen_lines={}）—— 前提不成立",
            a2.0
        );
        assert_eq!(
            a2.2, 0,
            "无历史时撑高把内容挪动了：marker 从第 0 行跑到第 {} 行。\
             引擎的口径是「scrollback 用尽后，新行只加在下方」。",
            a2.2
        );
        assert_eq!(
            a2.3 - a1.3,
            33,
            "撑高 33 行之后，marker 下方的空白只该**正好**多出 33 行（{} → {}）。\
             差值不对说明留白被别处吃掉了或漏了。",
            a1.3,
            a2.3
        );
        assert_eq!(
            a2.1, 0,
            "无历史时撑高凭空长出了 {} 行 scrollback —— 空终端会出现滚动条，\
             而且之后每次变高都会按 min(历史, 新增行数) 把内容往下拖。",
            a2.1
        );

        // 缩回窗口态：不留幽灵历史（否则「来回切一次全屏」就在空终端上留一条滚动条）
        view.update(cx, |v, _cx| v.resize(80, 45));
        cx.run_until_parked();
        let a3 = grid_landing(&view, cx);
        assert_eq!(
            a3, a1,
            "缩回窗口态之后网格落点回不到原样（窗口态 {a1:?} / 缩回后 {a3:?}）——\
             全屏来回切一次就改变了内容落点或留下了历史"
        );

        // ---- B：内容比屏幕长（有 scrollback 可回滚）-------------------------------
        let (view, feed) = build(cx, 45);
        let mut payload = String::new();
        for i in 0..60 {
            payload.push_str(&format!("L{i:02}\r\n"));
        }
        payload.push_str("MARKER\r\n");
        feed.feed(payload.as_bytes());
        wait(&view, cx);

        let b1 = grid_landing(&view, cx);
        assert_eq!(
            (b1.0, b1.2, b1.3),
            (45, 43, 0),
            "前提不成立：61 行输出灌进 45 行的屏，提示行应当**正好贴在底边**\
             （视口文本 44 行、marker 在末行 index 43、下方 0 行空白），实测 {b1:?}"
        );
        assert_eq!(
            b1.1, 17,
            "61 行输出灌进 45 行的屏，应当正好回滚掉 17 行，实测 {}（前提不成立）",
            b1.1
        );

        view.update(cx, |v, _cx| v.resize(80, 78));
        cx.run_until_parked();
        let b2 = grid_landing(&view, cx);
        assert_eq!(
            b2.2,
            b1.2 + b1.1 as i32,
            "有历史时撑高，提示行必须**正好**下移 `min(历史, 新增行数)` 行\
             （{} + min({}, 33) = {}），实测下移到第 {} 行。\
             这是「全屏之后屏幕看起来满了」的唯一来源：历史被拉回视口。",
            b1.2,
            b1.1,
            b1.2 + b1.1 as i32,
            b2.2
        );
        assert_eq!(
            b2.1, 0,
            "历史被拉回视口之后 `history_size` 应当归零，实测 {} —— \
             残留的话滚动条位置会与用户的观感对不上。",
            b2.1
        );
    }
}
