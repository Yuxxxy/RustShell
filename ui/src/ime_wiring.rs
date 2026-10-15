//! F-6 输入法（IME）通路的护栏（契约 / 行为 / 接线三层，仅测试构建编译）。
//!
//! # 被钉住的规矩
//!
//! 终端要能敲中文（日文 / 韩文同理），就得把「字符」从平台原样收下来；而字符在
//! 三个平台上**都不走 keydown**（Windows `WM_CHAR`、macOS `insertText:`、
//! X11 未被消费时的 `key_char`）。为此 `TerminalView` 实现了 `EntityInputHandler`，
//! 并在绘制期用 `Window::handle_input` 把它挂给平台。这一套的价值全在三件事上：
//!
//! 1. **每帧都挂、且只在聚焦时挂**：`handle_input` 只在 paint 期可调用，且只认
//!    「此刻聚焦」的视图。漏挂 = 输入法结果没有收件人（平台侧拿不到处理器就整条
//!    丢弃）。症状是「输入法没反应」，实机上不加日志看不出来 —— 只能靠端到端跑。
//! 2. **可打印键不被 keydown 消费**：平台上「应用消费了 keydown」正是文本通路的
//!    **抑制条件**（Windows 跳过 `TranslateMessage`、macOS 直接 return、X11 不再
//!    转发 `key_char`）。谁在放行分支里补一个 `stop_propagation()`，字符就永远
//!    到不了；反过来在分支里顺手写字节则是**双发**（用户看到双字）。
//! 3. **组合中的字绝不进 PTY**：预编辑串只画在光标处，上屏（`commit_text`）才写
//!    管道。否则拼音敲到一半就让 shell 先执行半截命令，而且「回车确认候选词」
//!    会同时给 shell 送一个 `\r`（命令当场执行）。
//!
//! # 三层各挡什么
//!
//! | 层 | 用例 | 挡的回归 |
//! |----|------|----------|
//! | 行为（真视图） | [`committing_text_writes_only_that_text`] | 上屏文本被当成粘贴（bracketed 包裹 / 换行规范化 / 补回车） |
//! | 行为（真视图） | [`preedit_stays_local_until_it_is_committed`] | 组合串漏进 PTY 或漏进网格；断连后文本通路仍写管道 |
//! | 端到端（真窗口） | [`a_printable_key_arrives_through_the_text_path_exactly_once`] | 处理器没装成（键全丢）/ 装在了没焦点的那一帧 / 可打印键被 keydown 双发 |
//! | 端到端（真窗口） | [`composing_keeps_the_keyboard_away_from_the_shell`] | 组合中按方向键 / 回车把字节送进 shell；断连后仍报「接受文本输入」 |
//! | 接线（扫源码） | [`the_input_handler_is_installed_during_paint`]、[`the_keyboard_gate_hands_printable_keys_to_the_text_path`]、[`the_ime_entry_points_land_on_the_terminal_semantics`] | 把 `handle_input` 挪出 paint 闭包、把 `accepts_text_input` 从 `alive` 上摘下来、让上屏走粘贴那条路 |
//!
//! 端到端那两条为什么必须真开窗口：`handle_input` 只在**绘制期**、且只在焦点落在
//! 本视图时才把处理器交给平台窗口。「源码里有那行调用」测不出「装在了没焦点的
//! 那一帧」，而后者就是「中文打不出字」本身。`Window::dispatch_keystroke` 的语义
//! 与三平台一致（先派发 keydown，**未被消费**才把 `key_char` 交给输入处理器），
//! 可以当平台替身来跑这条通路。
//!
//! 扫源码一律走 [`crate::test_support::prod`]（剥注释 + 切掉测试区）—— 针就写在
//! 本文件里，不过滤会自己命中自己（见 `test_support` 模块文档）。

use gpui::AppContext as _;

use crate::dead_session::feed_reader;
use crate::test_support::{prod, region};

// ---------------------------------------------------------------- 行为层

/// 记录写进 PTY 的每一个字节。
#[derive(Clone, Default)]
struct SpySink(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl SpySink {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap_or_else(|e| e.into_inner())).into_owned()
    }

    fn clear(&self) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clear();
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

/// 建一个「真终端视图 + 只记字节的管道」，不接窗口。
///
/// 读端用 [`crate::dead_session::feed_reader`] 且**全程不投喂**：那条读线程会一直
/// 阻塞在 `recv` 上。换成 `std::io::empty()` 会立刻 EOF 并发出 `Event::Exit`，把
/// 视图卷进「进程已退出」那条支路 —— 测的就不是输入通路了（详见 `dead_session` 里
/// `BlockingReader` 的文档：读线程一旦结束，它 drop 发送端会从后台线程唤醒调度器，
/// 撞上 gpui::test 的「测试不确定」检查，报文长得像断言失败）。
fn spy_view(
    cx: &mut gpui::TestAppContext,
) -> (gpui::Entity<rustshell_terminal::TerminalView>, SpySink) {
    cx.update(gpui_component::init);

    let sink = SpySink::default();
    let (reader, _feed) = feed_reader();
    let sink_for_view = sink.clone();
    let view = cx.update(|cx| {
        cx.new(|cx| {
            rustshell_terminal::TerminalView::new(
                sink_for_view,
                reader,
                rustshell_terminal::TerminalConfig::default(),
                cx,
            )
        })
    });
    cx.run_until_parked();
    (view, sink)
}

/// 上屏的文本按**原样**落管道。
///
/// 与 [`TerminalView::paste_text`] 的三点差别都是刻意的（见 `commit_text` 文档）：
/// 不做换行规范化（这是敲出来的字，不是粘贴的剧本）、不套 bracketed paste
/// （`ESC[200~` 出现在 readline 的编辑缓冲里是纯噪声）、不补回车（要不要执行由
/// 用户自己按回车）。哪一条被「顺手统一成粘贴逻辑」，候选词上屏就会带出转义串
/// 或者直接执行。
#[gpui::test]
fn committing_text_writes_only_that_text(cx: &mut gpui::TestAppContext) {
    let (view, sink) = spy_view(cx);

    // ① 前提：活着的会话必须**真的**写得进去。没有这一步，下面的「恰好写了这些」
    //    可能只是因为管道压根不通（例如视图没建起来），那是假绿。
    view.update(cx, |v, cx| v.commit_text("首", cx));
    assert_eq!(
        sink.text(),
        "首",
        "前提不成立：文本通路写不进 PTY，后面的断言无意义"
    );

    // ② 多行 / 换行符原样：不许被归一成 CR（那是粘贴的待遇）
    sink.clear();
    view.update(cx, |v, cx| v.commit_text("a\nb", cx));
    assert_eq!(
        sink.text(),
        "a\nb",
        "上屏文本被做了换行规范化 —— 那是 `paste_text` 的语义，上屏是「用户敲出来的字」，\
         一个字节都不该被改写"
    );

    // ③ 不套 bracketed paste：包裹只属于「整段插入的文本」
    sink.clear();
    view.update(cx, |v, cx| v.commit_text("ls", cx));
    let out = sink.text();
    assert_eq!(out, "ls", "上屏文本被包裹了：{out:?}");

    // ④ 不补回车：上屏 ≠ 执行
    assert!(
        !out.ends_with('\r'),
        "上屏自动补了回车 —— 用户还没按回车，命令就被执行了"
    );

    // ⑤ 提交之后预编辑串必须清空（否则它会一直画在屏上）
    assert!(
        view.read_with(cx, |v, _| v.preedit().is_empty()),
        "提交后预编辑串还在，它会一直画在光标处"
    );
}

/// 组合中的串**只画不写**：绝不进 PTY、也不进网格；断连后文本通路一起封死。
///
/// 「只画」这件事在无头测试里能看见的是后一半（网格里没有它）。前一半（真的画在
/// 光标处）由接线层的 [`the_input_handler_is_installed_during_paint`] 钉住。
#[gpui::test]
fn preedit_stays_local_until_it_is_committed(cx: &mut gpui::TestAppContext) {
    let (view, sink) = spy_view(cx);

    // ① 组合开始：记下来，但一个字节都不许走
    view.update(cx, |v, cx| v.set_preedit("nihao", cx));
    assert_eq!(
        view.read_with(cx, |v, _| v.preedit().to_string()),
        "nihao",
        "预编辑串没有被记下来 —— 用户看不到自己在打什么"
    );
    assert_eq!(
        sink.text(),
        "",
        "组合中的字进了 PTY —— shell 会先执行半截命令（拼音才敲到一半）"
    );
    assert!(
        !view.read_with(cx, |v, _| v.visible_text().contains("nihao")),
        "预编辑串被写进了网格：它只该由渲染层画在光标处，不该成为终端内容的一份"
    );

    // ② 上屏：清预编辑 + 原样写管道
    view.update(cx, |v, cx| v.commit_text("你好", cx));
    assert!(
        view.read_with(cx, |v, _| v.preedit().is_empty()),
        "上屏后预编辑串还在，它会继续画在光标处"
    );
    assert_eq!(sink.text(), "你好", "已确认的字没有上屏");

    // ③ 取消组合（Esc / 点别处）：丢掉，不上屏
    sink.clear();
    view.update(cx, |v, cx| {
        v.set_preedit("zaijian", cx);
        v.clear_preedit(cx);
    });
    assert_eq!(sink.text(), "", "取消组合不该往 PTY 写任何字节");
    assert!(
        view.read_with(cx, |v, _| v.preedit().is_empty()),
        "取消后预编辑串还在"
    );

    // ④ 断连：预编辑被清掉（留着就像一句没发出去的命令），两条文本入口一起封死
    view.update(cx, |v, cx| {
        v.set_preedit("x", cx);
        v.set_alive(false, cx);
    });
    assert!(
        view.read_with(cx, |v, _| v.preedit().is_empty()),
        "断连没有清预编辑串：它会一直画在断连现场上，像一句没发出去的命令"
    );
    let fence = sink.text().len();
    view.update(cx, |v, cx| {
        v.set_preedit("y", cx);
        v.commit_text("y", cx);
    });
    let after = sink.text();
    assert_eq!(
        after.len(),
        fence,
        "断连后文本通路仍在写 PTY —— 用户以为会话还活着。新增内容：{:?}",
        &after[fence..]
    );
}

// ---------------------------------------------------------- 端到端（真窗口）

/// 造一个「终端视图当根视图」的真窗口，并把焦点给它。
///
/// ★ 焦点要**先**给、再走一帧：`Window::handle_input` 只在绘制期、且只在
/// 「焦点此刻在本视图」时才把处理器交给平台。装在没焦点的那一帧 = 白装，
/// 而这一帧之后键盘就真的没反应了。
fn focused_terminal_window(
    cx: &mut gpui::TestAppContext,
    sink: SpySink,
) -> (
    gpui::Entity<rustshell_terminal::TerminalView>,
    &mut gpui::VisualTestContext,
) {
    let (reader, _feed) = feed_reader();
    let (view, vcx) = cx.add_window_view(move |_window, cx| {
        rustshell_terminal::TerminalView::new(
            sink,
            reader,
            rustshell_terminal::TerminalConfig::default(),
            cx,
        )
    });
    vcx.run_until_parked();

    vcx.update(|window, cx| {
        let handle = view.read(cx).focus_handle().clone();
        window.focus(&handle, cx);
    });
    vcx.run_until_parked();
    assert!(
        vcx.update(|window, cx| view.read(cx).focus_handle().is_focused(window)),
        "前提不成立：焦点没落在终端上 —— 处理器不会被装上，后面所有按键断言都会\
         以「什么都没收到」的方式假绿"
    );
    (view, vcx)
}

/// 输入法的剪贴板入口走**粘贴**语义（不是上屏）。
///
/// 为什么值得单开一条：`paste` 与 `replace_text_in_range` 在平台看来都是「一段文本
/// 进来了」，把 `paste` 接到 `commit_text` 上不会有任何编译错误、屏幕上直到用户
/// 粘一段多行脚本之前也看不出差别 —— 而那一刻 zsh / fish 会把它**逐行当命令立即
/// 执行**（少了 bracketed paste 的包裹）。行为上的判据取换行规范化：
/// `commit_text` 原样写、`paste_text` 把 LF 归一成 CR。
#[gpui::test]
fn the_ime_clipboard_entry_point_keeps_the_paste_semantics(cx: &mut gpui::TestAppContext) {
    use gpui::EntityInputHandler as _;

    let sink = SpySink::default();
    let (view, vcx) = focused_terminal_window(cx, sink.clone());

    vcx.update(|window, cx| {
        view.update(cx, |v, cx| {
            v.paste(
                gpui::ClipboardItem::new_string("a\nb".to_string()),
                window,
                cx,
            )
        });
    });
    assert_eq!(
        sink.text(),
        "a\rb",
        "输入法的剪贴板入口没有走粘贴语义：多行粘贴会被 shell 逐行当命令立即执行"
    );
}

/// 可打印键**恰好上屏一次**（走文本通路），控制键仍走 keydown。
///
/// 断言取**全等**而不是 `contains`，两个失效模式一起挡：
/// * keydown 顺手写字节 ⇒ `"aa"`（双发，用户看到双字）；
/// * keydown 放行时 `stop_propagation()` ⇒ `""`（平台把文本通路抑制掉，字符永远
///   到不了 —— 中文直接打不出字）。
#[gpui::test]
fn a_printable_key_arrives_through_the_text_path_exactly_once(cx: &mut gpui::TestAppContext) {
    let sink = SpySink::default();
    let (_view, vcx) = focused_terminal_window(cx, sink.clone());

    // ① 可打印字符：由平台投到文本通路（`dispatch_keystroke` 里那一步与三平台一致）
    vcx.simulate_keystrokes("a");
    assert_eq!(
        sink.text(),
        "a",
        "可打印键没有恰好上屏一次。空 = 被 keydown 消费掉了（文本通路被平台抑制）；\
         双份 = keydown 也写了一遍"
    );

    // ② 控制键（方向键）仍走 keydown：它的 `key_char` 为空，文本通路不参与
    vcx.simulate_keystrokes("left");
    assert_eq!(
        sink.text(),
        "a\x1b[D",
        "方向键没有走 keydown 出字节（`\\x1b[D` 是普通模式的 Left）"
    );

    // ③ 断连：键盘那道闸（`on_key_down` 开头）与文本那道闸（`commit_text` 开头）
    //    必须一起合上 —— `on_key_down` 对可打印键是放行的，只靠它拦不住
    vcx.update(|_w, cx| _view.update(cx, |v, cx| v.set_alive(false, cx)));
    vcx.simulate_keystrokes("a left");
    assert_eq!(
        sink.text(),
        "a\x1b[D",
        "断连之后仍有字节进管道：敲键/输入法看着有反应，实际排进一个永远不会返回的管道"
    );
}

/// 组合中键盘归输入法：一个字节都不进 shell；断连后连输入法上下文都要关掉。
#[gpui::test]
fn composing_keeps_the_keyboard_away_from_the_shell(cx: &mut gpui::TestAppContext) {
    use gpui::EntityInputHandler as _;

    let sink = SpySink::default();
    let (view, vcx) = focused_terminal_window(cx, sink.clone());

    // ① 组合开始：走**平台真正调的那两个入口**（`replace_and_mark_text_in_range` /
    //    `replace_text_in_range`），而不是内部的 `set_preedit` / `commit_text` ——
    //    否则「入口接错方法」这类错（组合走成上屏、上屏走成粘贴）只有扫源码那条
    //    护栏看得见，行为层一条都拦不住。
    vcx.update(|window, cx| {
        view.update(cx, |v, cx| {
            v.replace_and_mark_text_in_range(None, "ni", None, window, cx);
            assert!(
                v.marked_text_range(window, cx).is_some(),
                "组合中 `marked_text_range` 必须给出区间：macOS 靠它把后续按键先递给输入法\
                 （否则组合中按方向键会被终端吃掉）"
            );
        });
    });
    assert_eq!(
        view.read_with(vcx, |v, _| v.preedit().to_string()),
        "ni",
        "输入法报来的组合串没有被记下来 —— 用户看不到自己在打什么"
    );

    // ② 组合中按方向键 = 翻候选页，绝不能给 shell 送 `\x1b[A`。
    //    平台**先**把 keydown 递给应用、之后才轮到输入法，所以这一下一定会经过
    //    `on_key_down`（那道 preedit 守卫就是为它装的）。
    vcx.simulate_keystrokes("up");
    assert_eq!(
        sink.text(),
        "",
        "组合中按方向键把字节送进了 shell —— 用户翻候选词，shell 收到一串转义"
    );

    // ③ 上屏：平台调 `replace_text_in_range`，已确认的字原样写出、预编辑清空
    vcx.update(|window, cx| {
        view.update(cx, |v, cx| v.replace_text_in_range(None, "你", window, cx))
    });
    assert_eq!(sink.text(), "你", "上屏的字没有进管道");
    assert!(
        view.read_with(vcx, |v, _| v.preedit().is_empty()),
        "上屏后预编辑串还在"
    );
    assert!(
        vcx.update(|window, cx| view.update(cx, |v, cx| v.marked_text_range(window, cx).is_none())),
        "上屏后仍报「正在组合」：macOS 会把后续按键一直先交给输入法，终端再也收不到键"
    );

    // ④ 存活时接受文本输入，断连后必须翻成 false —— 平台据此关掉输入法上下文
    //    （Windows 的 `ImmAssociateContextEx(..., 0)`：断连的终端连候选窗都不该弹）
    assert!(
        vcx.update(|window, cx| view.update(cx, |v, cx| v.accepts_text_input(window, cx))),
        "活着的会话报「不接受文本输入」—— 输入法直接打不出字"
    );
    vcx.update(|_w, cx| view.update(cx, |v, cx| v.set_alive(false, cx)));
    assert!(
        !vcx.update(|window, cx| view.update(cx, |v, cx| v.accepts_text_input(window, cx))),
        "断连后仍报「接受文本输入」：输入法照旧弹候选窗，而字符全被闸掉 —— \
         用户打半天一个字都不出来"
    );
}

// ---------------------------------------------------------------- 接线层

/// 输入处理器必须在**绘制期**装（装的是本视图的实体 + canvas 的 bounds）。
#[test]
fn the_input_handler_is_installed_during_paint() {
    let view = prod(include_str!("../../terminal/src/view.rs"));

    // 断言钉在 canvas 的 paint 闭包里（`__paint_t0` 起、闭包结尾 `.size_full(),` 止）：
    // 整文件级 `contains` 会让「这行出现在 prepaint / `render()` 里」也算通过 ——
    // 而 `handle_input` 在 paint 之外调用是会被 `debug_assert_paint` 直接打回的。
    let paint = region(&view, "let __paint_t0", ".size_full(),");
    assert!(
        paint.contains("window.handle_input("),
        "paint 闭包里没有注册输入处理器 —— 输入法 / 平台的字符消息没有收件人，中文打不出字"
    );
    assert!(
        paint.contains("&focus_handle"),
        "没把焦点句柄交给 `handle_input`：它只在焦点在本视图时才把处理器交给平台窗口，\
         而「本视图」正是靠这个句柄判的"
    );
    assert!(
        paint.contains("ElementInputHandler::new(bounds, this.clone())"),
        "输入处理器没带上 canvas 的 `bounds`：候选窗定位（`bounds_for_range`）拿不到原点，\
         输入法候选框会飘到窗口左上角"
    );

    // 预编辑串要一路传到渲染器（画不出来 = 用户看不到自己在打什么）
    let draw_call = region(&view, "renderer.paint(", ".size_full(),");
    assert!(
        draw_call.contains("&preedit"),
        "预编辑串没有传给渲染器 —— 候选字不上屏"
    );

    // 组合期间不画光标：预编辑就画在光标那一格上，叠一个光标会把第一个候选字糊住
    let cursor = region(&view, "let cursor = {", "let search_spans");
    assert!(
        cursor.contains("preedit.is_empty()"),
        "组合期间仍在画光标：它会把第一个候选字糊住"
    );

    // 渲染侧的落点
    let render = prod(include_str!("../../terminal/src/render.rs"));
    let call = region(
        &render,
        "if display_offset == 0 && !preedit.is_empty()",
        "fn paint_preedit",
    );
    assert!(
        call.contains("self.paint_preedit("),
        "预编辑串没有被画出来 —— F-6 的可见部分（候选字带下划线显示在光标处）落空了"
    );
    let body = region(&render, "fn paint_preedit", "let mut seg = String::new();");
    assert!(
        body.contains("if preedit.is_empty()"),
        "`paint_preedit` 少了空串守卫"
    );
    assert!(
        body.contains("solid_quad("),
        "`paint_preedit` 没有先铺底色：底下的终端内容会与候选字叠字"
    );
    assert!(
        body.contains("char_advance(ch)"),
        "`paint_preedit` 没有按字宽分段：一个 CJK 字占 2 格，混进 1 格宽的段里会被\
         `force_width` 挤扁"
    );
}

/// 可打印键必须**不写字节、不消费**地让给文本通路；组合中一键不发。
#[test]
fn the_keyboard_gate_hands_printable_keys_to_the_text_path() {
    let view = prod(include_str!("../../terminal/src/view.rs"));

    // 两道闸门都必须在 `keystroke_to_bytes` **之前** —— 写在之后等于白写
    let keydown = region(&view, "fn on_key_down", "keystroke_to_bytes(");
    assert!(
        keydown.contains("if !self.preedit.is_empty()"),
        "少了「组合中不写字节」那道闸：按回车确认候选词会同时给 shell 送一个 `\\r`\
         （命令当场执行），按退格删的是 shell 里的字符"
    );
    assert!(
        keydown.contains(
            "if event.prefer_character_input || is_text_input_keystroke(&event.keystroke)"
        ),
        "少了可打印键的放行闸：装了输入处理器之后，不消费按键不代表没人管 —— \
         平台会把字符再投一次到文本通路，两边都收就成了双发"
    );

    // 放行那一段**绝不能**消费按键：平台上「应用消费了 keydown」正是文本通路的
    // 抑制条件（Windows 跳过 `TranslateMessage`、macOS 直接 return、X11 不再发
    // `key_char`）。这里 `stop_propagation()` 一下，中文就永远打不出来。
    let pass = region(
        &view,
        "if event.prefer_character_input || is_text_input_keystroke",
        "keystroke_to_bytes(",
    );
    assert!(
        !pass.contains("stop_propagation"),
        "可打印键在放行分支里 `stop_propagation()` 了 —— 平台会因此不再投递文本，\
         字符永远到不了终端（症状：中文打不出字，而英文也一起没了）"
    );
}

/// 输入法的每个落点落在**终端语义**上（上屏 ≠ 粘贴、组合只画不写）。
#[test]
fn the_ime_entry_points_land_on_the_terminal_semantics() {
    let view = prod(include_str!("../../terminal/src/view.rs"));

    let handler = region(
        &view,
        "impl EntityInputHandler for TerminalView",
        "impl Render for TerminalView",
    );
    for needle in [
        "fn text_for_range",
        "fn selected_text_range",
        "fn marked_text_range",
        "fn unmark_text",
        "fn paste(",
        "fn replace_text_in_range",
        "fn replace_and_mark_text_in_range",
        "fn bounds_for_range",
        "fn character_index_for_point",
        "fn accepts_text_input",
    ] {
        assert!(
            handler.contains(needle),
            "`EntityInputHandler` 少了 `{needle}`：平台那一边的字符消息会掉在空处\
             （默认实现是「什么都不做」，不报错、不打日志）"
        );
    }

    // 终端不是「可编辑文档」：不许把网格拼成字符串交给输入法
    let text_for_range = region(&view, "fn text_for_range", "fn selected_text_range");
    assert!(
        text_for_range.contains("None") && !text_for_range.contains("visible_text"),
        "`text_for_range` 把屏幕内容当成可替换的文本交出去了 —— 那等于给输入法一条\
         改写远端程序画面的错路"
    );

    // 上屏 → `commit_text`（原样写管道）；组合 → `set_preedit`（只画不写）
    let commit = region(
        &view,
        "fn replace_text_in_range",
        "fn replace_and_mark_text_in_range",
    );
    assert!(
        commit.contains("commit_text("),
        "`replace_text_in_range` 没有落到 `commit_text`：输入法上屏的字写不进管道"
    );
    let mark = region(
        &view,
        "fn replace_and_mark_text_in_range",
        "fn bounds_for_range",
    );
    assert!(
        mark.contains("set_preedit("),
        "`replace_and_mark_text_in_range` 没有只记不写 —— 组合中的字会直接进 PTY"
    );

    // 剪贴板走 `request_paste`（粘贴闸门：bracketed paste 包裹 + 换行规范化 +
    // 多行先确认），**不是**上屏那条路。
    //
    // 2026-09-28（F-5）口径收紧：入口从 `paste_text` 换成 `request_paste`。
    // 反向断言同时加上「不得直连 `paste_text`」—— 那是确认后的底层写入，
    // 从这里直连会让输入法成为绕过粘贴确认框的旁路。
    let paste = region(&view, "fn paste(", "fn replace_text_in_range");
    assert!(
        paste.contains("request_paste("),
        "输入法的剪贴板接口没有落到 `request_paste`（粘贴闸门）"
    );
    assert!(
        !paste.contains("paste_text("),
        "输入法的剪贴板接口直连了 `paste_text`：含换行的粘贴会绕过确认框"
    );
    assert!(
        !paste.contains("commit_text"),
        "输入法的剪贴板接口走上了屏那条路：多行粘贴会失去 bracketed paste 包裹，\
         zsh / fish 会把它逐行当命令**立即执行**"
    );

    // 取消组合 → 丢掉，不上屏
    let unmark = region(&view, "fn unmark_text", "fn paste(");
    assert!(
        unmark.contains("clear_preedit("),
        "`unmark_text` 没有丢掉预编辑串：取消组合后它还画在屏上，且会拦住后续按键"
    );

    // 标记区间是 macOS / X11 判断「正在组合」的依据
    let marked = region(&view, "fn marked_text_range", "fn unmark_text");
    assert!(
        marked.contains("self.preedit.is_empty()")
            && marked.contains("Some(")
            && marked.contains("None"),
        "`marked_text_range` 不再按预编辑串是否为空来作答：组合期间 macOS 就不会把后续\
         按键先交给输入法（方向键会被终端吃掉）"
    );

    // 候选窗定位：一律给光标那一格
    let bounds = region(&view, "fn bounds_for_range", "fn character_index_for_point");
    assert!(
        bounds.contains("cursor_cell_bounds("),
        "`bounds_for_range` 没有给光标格：输入法候选窗会飘在窗口外或左上角"
    );

    // 断连 = 不接受文本输入（平台据此关掉输入法上下文）
    let accepts = region(&view, "fn accepts_text_input", "impl Render");
    assert!(
        accepts.contains("self.alive"),
        "`accepts_text_input` 不再绑在会话存活上：断连的终端照样弹输入法，\
         而字符全被 `commit_text` 的闸门吃掉"
    );

    // `commit_text` 的闸门与形态
    let commit_text = region(&view, "pub fn commit_text", "fn cursor_cell_bounds");
    assert!(
        commit_text.contains("if !self.alive"),
        "`commit_text` 少了断连闸门：文本通路绕过了 `on_key_down` 那一刀，\
         断连后输入法提交的字照样排进永不返回的管道"
    );
    assert!(
        !commit_text.contains("normalize_paste"),
        "`commit_text` 用上了粘贴的规范化 —— 上屏的文本不是粘贴的剧本，\
         一个字节都不该被改写"
    );
    assert!(
        commit_text.contains("text.as_bytes()"),
        "`commit_text` 没有把文本原样写进管道"
    );

    // 预编辑串的三处清理：断连、失焦、取消
    let set_alive = region(&view, "pub fn set_alive", "fn blink_should_run");
    assert!(
        set_alive.contains("self.preedit.clear()"),
        "`set_alive(false)` 没有清预编辑串：它会一直画在断连现场上"
    );
    let render_fn = region(
        &view,
        "fn render(",
        "let state_arc = self.state.term_arc();",
    );
    assert!(
        render_fn.contains("self.preedit.clear()"),
        "失焦没有清预编辑串：平台此时已经结束了这次组合，留着它不仅画在屏上，\
         下次聚焦还会拦住所有按键（`on_key_down` 的 preedit 守卫）"
    );
}
