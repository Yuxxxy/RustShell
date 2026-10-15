//! F-5 粘贴保护的护栏（契约 / 行为 / 接线三层），仅测试构建编译。
//!
//! # 被钉住的规矩
//!
//! 1. **含换行的粘贴不许直接进管道** —— 先挂起，等用户确认（`request_paste`）；
//! 2. **取消一个字节都不许写**（「这内容看着不对」是用户按取消的常见动机，
//!    此刻漏一个字节都可能已经执行了半条命令）；
//! 3. **确认后走的是完整粘贴通路**（行尾归一 + bracketed 包裹 + `and_enter`），
//!    不是把原始文本倒进管道；
//! 4. **三个入口全部过闸**：快捷键 / IME 剪贴板接口 / 右键菜单 ——
//!    少接一个，那条路就成了「别人会弹框、我不会」的旁路；
//! 5. **断连即作废**：挂在确认框上的请求不许活过会话（点确认写进死管道
//!    是想象中最坏的组合）。
//!
//! # 三层各挡什么
//!
//! | 层 | 用例 | 挡的回归 |
//! |----|------|----------|
//! | 契约（纯函数） | `terminal::input` 的 `paste_newline_count_*` / `paste_confirm_threshold_semantics` | 换行数数错（CRLF 数成两个 / 结尾换行漏数） |
//! | 契约（纯函数） | `main_view::paste_guard` 的预览用例 | 预览把「还有更多行」藏起来（用户拿它做决策） |
//! | 行为（真终端视图） | [`multi_line_paste_waits_for_confirmation_then_writes_once`] 等 | 闸不拦 / 取消漏写 / 确认不走完整通路 |
//! | 接线（扫源码） | [`every_paste_entry_routes_through_the_gate`] 等 | 有人把某个入口接回 `paste_text`（底层写入、不过闸） |
//!
//! 行为层为什么必须真建一个 `TerminalView`：这道闸的价值就是「引擎自己拦、
//! 不依赖宿主记得判」。只断言源码里有 `needs_paste_confirm` 是接线的活，
//! 测不出「闸装在 `paste_text` 里，于是确认之后又被拦一次（永远写不进去）」
//! —— 那正是把判定放错位置的另一种翻车法。

use crate::dead_session::{blocking_feed_receiver, feed_reader, BlockingReader, SpySink};
use crate::test_support::{prod, region};

// ---------------------------------------------------------------- 行为层

/// 在测试执行器上建一块终端视图，写端接 `sink`，返回视图。
///
/// 读端永不返回：粘贴闸的断言只关心**写**，读端一 EOF 会把视图卷进
/// 「进程已退出」那条支路（与 `dead_session` 的用例同一理由）。
fn view_with_spy(
    cx: &mut gpui::TestAppContext,
    sink: &SpySink,
    protect_lines: Option<u32>,
) -> gpui::Entity<rustshell_terminal::TerminalView> {
    use gpui::AppContext as _;
    cx.update(gpui_component::init);
    let sink = sink.clone();
    cx.update(|cx| {
        cx.new(|cx| {
            let config = rustshell_terminal::TerminalConfig {
                paste_protect_lines: protect_lines,
                ..Default::default()
            };
            rustshell_terminal::TerminalView::new(
                sink,
                BlockingReader(blocking_feed_receiver()),
                config,
                cx,
            )
        })
    })
}

/// 含换行的粘贴必须**先挂起**（管道零字节），确认后才整段写进去。
///
/// 三条断言缺一不可：挂起后没写（拦住了）、请求带着行数（提示有数可报）、
/// 确认后写出的是**归一后**的内容（不是原始文本）。
#[gpui::test]
fn multi_line_paste_waits_for_confirmation_then_writes_once(cx: &mut gpui::TestAppContext) {
    let sink = SpySink::default();
    let view = view_with_spy(cx, &sink, Some(1));
    cx.run_until_parked();

    view.update(cx, |v, cx| {
        v.request_paste("ls -l\r\nps aux\r\nwhoami", false, cx)
    });

    assert_eq!(
        sink.text(),
        "",
        "含换行的粘贴在确认前一个字节都不该进管道（这就是整道闸的意义）"
    );
    let pending = view.read_with(cx, |v, _| v.pending_paste().cloned());
    let pending = pending.expect("含换行的粘贴必须挂起等确认");
    assert_eq!(pending.lines(), 3, "3 行内容要如实报出 3 行");
    assert!(!pending.and_enter, "「粘贴」不带执行意图");

    view.update(cx, |v, cx| v.confirm_paste(cx));
    assert_eq!(
        sink.text(),
        "ls -l\rps aux\rwhoami",
        "确认后要写出**归一后**的内容（CRLF → CR），不是原始文本"
    );
    assert!(
        view.read_with(cx, |v, _| v.pending_paste().is_none()),
        "确认后挂起必须取走 —— 留着它，下一次渲染会再弹一次框"
    );

    // 幂等：连点两下确认不该写两份（挂起已被取走）
    view.update(cx, |v, cx| v.confirm_paste(cx));
    assert_eq!(
        sink.text(),
        "ls -l\rps aux\rwhoami",
        "第二次确认必须是空操作：重复确认会写第二份内容"
    );
}

/// 取消 = 零写入，且挂起被丢掉（否则下次渲染又把框弹回来）。
#[gpui::test]
fn cancelling_a_paste_writes_nothing(cx: &mut gpui::TestAppContext) {
    let sink = SpySink::default();
    let view = view_with_spy(cx, &sink, Some(1));
    cx.run_until_parked();

    view.update(cx, |v, cx| {
        v.request_paste("rm -rf /tmp/x\nrm -rf /tmp/y", false, cx)
    });
    view.update(cx, |v, cx| v.cancel_paste(cx));

    assert_eq!(sink.text(), "", "取消后终端必须零写入");
    assert!(
        view.read_with(cx, |v, _| v.pending_paste().is_none()),
        "取消后挂起必须丢掉，否则框会关了又回来"
    );

    // 也不许「确认」一个已经取消掉的请求
    view.update(cx, |v, cx| v.confirm_paste(cx));
    assert_eq!(sink.text(), "", "取消之后再确认也不许写出任何字节");
}

/// 单行粘贴不打扰：直接落管道，不挂起。
#[gpui::test]
fn single_line_paste_bypasses_the_prompt(cx: &mut gpui::TestAppContext) {
    let sink = SpySink::default();
    let view = view_with_spy(cx, &sink, Some(1));
    cx.run_until_parked();

    view.update(cx, |v, cx| v.request_paste("ls -l", false, cx));

    assert_eq!(sink.text(), "ls -l", "单行粘贴应当直接写进去");
    assert!(
        view.read_with(cx, |v, _| v.pending_paste().is_none()),
        "单行粘贴不该挂起（弹框打扰就是从这里开始的）"
    );
}

/// 阈值可配：`Some(2)` 时 1 个换行直通、2 个换行才拦；`None` 时整个闸关掉。
///
/// 判定式是「**换行数 ≥ 阈值**」（等价于「行数 > 阈值」，见规格 §1.5 施工展开 ②）：
/// 阈值比的是**换行**，不是行 —— 写测试时最容易在这儿把语义搞反。
#[gpui::test]
fn threshold_is_configurable_and_none_turns_the_gate_off(cx: &mut gpui::TestAppContext) {
    // Some(2)：2 行（1 个换行）直通
    let sink = SpySink::default();
    let view = view_with_spy(cx, &sink, Some(2));
    cx.run_until_parked();
    view.update(cx, |v, cx| v.request_paste("a\nb", false, cx));
    assert_eq!(sink.text(), "a\rb", "1 个换行 < 阈值 2，应当直通");
    assert!(view.read_with(cx, |v, _| v.pending_paste().is_none()));

    // Some(2)：3 行（2 个换行）达到阈值，要拦
    let sink2 = SpySink::default();
    let view2 = view_with_spy(cx, &sink2, Some(2));
    cx.run_until_parked();
    view2.update(cx, |v, cx| v.request_paste("a\nb\nc", false, cx));
    assert_eq!(sink2.text(), "", "2 个换行达到阈值 2，必须先确认");
    let pending = view2.read_with(cx, |v, _| v.pending_paste().cloned());
    assert_eq!(
        pending.expect("达到阈值必须挂起").lines(),
        3,
        "3 行内容要如实报出 3 行"
    );

    // None：保护关闭 ⇒ 多行也直接写
    let sink3 = SpySink::default();
    let view3 = view_with_spy(cx, &sink3, None);
    cx.run_until_parked();
    view3.update(cx, |v, cx| v.request_paste("a\nb\nc", false, cx));
    assert_eq!(sink3.text(), "a\rb\rc", "保护关闭时多行也直接写");
    assert!(view3.read_with(cx, |v, _| v.pending_paste().is_none()));
}

/// 断连即作废：挂起的请求不许活过会话。
///
/// 「点确认把半截命令写进一个永远不返回的管道」是这组状态里最坏的组合：
/// 用户以为取消了、其实内容已经排进去了。
#[gpui::test]
fn dead_session_drops_the_pending_paste(cx: &mut gpui::TestAppContext) {
    let sink = SpySink::default();
    let view = view_with_spy(cx, &sink, Some(1));
    cx.run_until_parked();

    view.update(cx, |v, cx| v.request_paste("echo one\necho two", false, cx));
    assert!(view.read_with(cx, |v, _| v.pending_paste().is_some()));

    view.update(cx, |v, cx| v.set_alive(false, cx));
    assert!(
        view.read_with(cx, |v, _| v.pending_paste().is_none()),
        "断连后挂起必须被清掉（宿主据此收掉确认框）"
    );

    view.update(cx, |v, cx| v.confirm_paste(cx));
    assert_eq!(sink.text(), "", "对着死会话确认也不许写出任何字节");
}

/// 确认后走的是**完整粘贴通路**：远端开了 bracketed paste 就要带上包裹。
///
/// 这一条把「确认 = 用户答了『要不要粘』」钉死：答完之后怎么粘，仍由
/// `paste_text` 的语义决定 —— 确认框不改变包裹方式，也不吞掉 `and_enter`。
#[gpui::test]
fn confirmed_paste_still_goes_through_bracketed_wrapping(cx: &mut gpui::TestAppContext) {
    use gpui::AppContext as _;
    cx.update(gpui_component::init);
    crate::dead_session::allow_real_io(cx);

    let sink = SpySink::default();
    let sink_for_view = sink.clone();
    let (reader, feed) = feed_reader();
    let view = cx.update(|cx| {
        cx.new(|cx| {
            let config = rustshell_terminal::TerminalConfig {
                paste_protect_lines: Some(1),
                ..Default::default()
            };
            rustshell_terminal::TerminalView::new(sink_for_view, reader, config, cx)
        })
    });
    cx.run_until_parked();

    // 远端声明支持 bracketed paste（bash/zsh/fish/vim 的默认姿势）。
    // `?2004h` 与标记行走同一批字节 ⇒ 看到 ARMED 就说明模式开好了
    // —— 只 feed 模式字节没有可等的东西，`pump_until_parsed` 会立刻返回假绿。
    feed.feed(b"\x1b[?2004h\x1b[2mARMED\x1b[0m\r\n");
    crate::dead_session::pump_until_parsed(cx, "bracketed paste 模式生效", |cx| {
        cx.update(|cx| view.read(cx).visible_text().contains("ARMED"))
    });

    view.update(cx, |v, cx| v.request_paste("a\nb", true, cx));
    assert_eq!(sink.text(), "", "先拦");
    view.update(cx, |v, cx| v.confirm_paste(cx));
    assert_eq!(
        sink.text(),
        "\x1b[200~a\rb\x1b[201~\r",
        "确认后要走完整通路：bracketed 包裹 + 行尾归一 + and_enter 补的回车"
    );
}

// ---------------------------------------------------------------- 接线层

/// 引擎里三个入口全部过闸，且取消路径不碰管道。
///
/// 扫的是 `terminal/src/view.rs` 自己的源码：键绑与 IME 都在那儿，
/// 它们的接线错了宿主侧**看不出来**（宿主只看见「没弹框」）。
#[test]
fn every_paste_entry_routes_through_the_gate() {
    let code = prod(include_str!("../../terminal/src/view.rs"));

    // 快捷键：`paste_from_clipboard` 是键绑的落点，它必须转交 `request_paste`
    let from_clipboard = region(&code, "pub fn paste_from_clipboard", "pub fn request_paste");
    assert!(
        from_clipboard.contains("self.request_paste("),
        "Ctrl+Shift+V 的落点没有过闸：含换行的粘贴会绕过确认框直接执行"
    );

    // IME 的剪贴板接口：同样必须过闸（不是 paste_text / commit_text）
    let ime_paste = region(
        &code,
        "fn paste(&mut self, item",
        "fn replace_text_in_range",
    );
    assert!(
        ime_paste.contains("self.request_paste("),
        "输入法的剪贴板接口没有过闸：同一个内容用快捷键会问、用输入法却直接执行"
    );

    // 闸门自己：判定 + 挂起，且放行分支才落 `paste_text`
    let gate = region(&code, "pub fn request_paste", "pub fn pending_paste");
    assert!(
        gate.contains("needs_paste_confirm"),
        "`request_paste` 没做阈值判定 —— 闸成了摆设"
    );
    assert!(
        gate.contains("paste_pending = Some("),
        "`request_paste` 没把命中阈值的请求挂起来"
    );
    assert!(
        gate.contains("self.paste_text("),
        "`request_paste` 放行时没有落到底层写入 —— 单行粘贴会写不进去"
    );

    // 取消 = 零写入（反向信号：这里出现任何写入调用都是错的）
    let cancel = region(&code, "pub fn cancel_paste", "pub fn paste_text");
    assert!(
        !cancel.contains("paste_text") && !cancel.contains("write_to_pty"),
        "`cancel_paste` 里出现了写入调用 —— 取消一个字节都不许写"
    );

    // 断连清挂起
    let set_alive = region(&code, "pub fn set_alive(", "fn blink_should_run");
    assert!(
        set_alive.contains("self.paste_pending = None;"),
        "断连没有清掉挂起的粘贴请求：确认框会留在屏幕上，且能对着死会话点确认"
    );
}

/// 宿主侧：检测、弹框、两条回答、收框，四处缺一不可。
#[test]
fn host_prompts_and_clears_on_every_exit() {
    let view = prod(include_str!("main_view.rs"));
    let guard = prod(include_str!("main_view/paste_guard.rs"));
    let dialogs = prod(include_str!("main_view/dialogs.rs"));

    // 渲染期检测 + 收掉无效框（不走 poll —— 见 `paste_guard` 模块文档）
    assert!(
        view.contains("self.paste_prompt_prune(window, cx);")
            && view.contains("self.paste_prompt_session(cx)")
            && view.contains("DialogMode::PasteConfirm { session }"),
        "渲染期没有检测挂起的粘贴 / 没有弹确认框（或者没有先收掉无效框）"
    );
    // 检测必须直读终端实体（唯一真值），不是读某个缓存副本
    assert!(
        guard.contains("pending_paste()"),
        "宿主没有从终端实体读挂起请求 —— 断连后无法察觉请求已作废"
    );
    // 两个回答都要落到引擎
    assert!(
        guard.contains("confirm_paste(cx)"),
        "「粘贴」按钮没有落到引擎的 `confirm_paste`（挂了却写不进去）"
    );
    assert!(
        guard.contains("cancel_paste(cx)"),
        "「取消」没有落到引擎的 `cancel_paste`（挂起会一直留着）"
    );
    // 确认前先把视图切到那条会话：粘进看不见的后台标签 = 用户无从核对
    assert!(
        guard.contains("place_in_focused_pane"),
        "确认后没有切到目标会话 —— 粘贴写进了看不见的标签"
    );
    // 收框走 `close_dialog_ui`（清业务标记），不是裸 `w.close_dialog`
    // —— 后者不触发 `on_close`，会把单槽对话框永久锁死（铁律 #54 的邻居）
    assert!(
        guard.contains("close_dialog_ui"),
        "宿主收框没有走 `close_dialog_ui`：业务标记会停在 Some(PasteConfirm)"
    );

    // 对话框：标题 / 行数 / 预览 / 警告四件信息
    assert!(
        dialogs.contains("fn render_paste_confirm_dialog"),
        "确认框的渲染函数不见了（派发臂会指到空处）"
    );
    assert!(
        dialogs.contains("paste_prompt_body"),
        "确认框没有现取挂起请求的正文（行数与预览）"
    );
    // 每一条关闭路径都要把挂起丢掉：`present_dialog` 的 on_close 是兜底那一道
    assert!(
        dialogs.contains("m.paste_prompt_decline(cx);"),
        "`on_close` 没有兜底取消粘贴：Esc / 点遮罩 / 右上角 X 关掉之后，框会再弹回来"
    );
}

/// 设置面板与配置链路必须消费粘贴行为设置（否则就是「死配置」）。
///
/// 与 `main_view.rs` 里那条「光标闪烁是配置项、面板为它摆控件」的护栏**互补**：
/// 粘贴闸的行为是**面板管**的（「多行粘贴行为」是唯一旋钮，必须伸手可调），
/// 且必须一路透传到引擎 —— 面板管了但没透传，就是换了个位置的死配置。
///
/// 2026-09-28 收敛：原先面板上摆着 `normalize_newlines` 与
/// `paste_protect` / `paste_protect_lines` 三只控件，能配出「先确认 + 逐行执行」
/// 这种自相矛盾又危险的组合。现在三个旧字段合并成单一
/// `terminal_paste_behavior: PasteBehavior`，两个引擎参数由它**同一个**枚举算出。
///
/// 合成点（一个设置字段 → 引擎的两个参数）去空白比对：那两行会被
/// `cargo fmt` 折成多段，按原样拼针等于把断言绑死在当前折行方式上。
#[test]
fn paste_behavior_settings_reach_the_engine() {
    let panel = prod(include_str!("main_view/settings_panel.rs"));
    assert!(
        panel.contains("settings.terminal_paste_behavior"),
        "设置面板没有消费 `terminal_paste_behavior`（下拉是死配置）"
    );
    // 三个旧字段不得再回到面板：它们能各自取值，正是矛盾组合的来源
    for gone in [
        "settings.terminal_paste_normalize_newlines",
        "settings.terminal_paste_protect = val",
        "settings.terminal_paste_protect_lines = val",
    ] {
        assert!(
            !panel.contains(gone),
            "粘贴行为已收敛为单一 `PasteBehavior`，面板不得再摆旧控件：{gone}"
        );
    }

    let squeezed = |s: &str| -> String { s.chars().filter(|c| !c.is_whitespace()).collect() };
    // 一个枚举 → 引擎的两个参数：换算点只有那两处（建会话 / 设置热更新）
    let compose = [
        "paste_normalize_newlines:self.settings.terminal_paste_behavior.normalize_newlines()",
        "paste_protect_lines:self.settings.terminal_paste_behavior.protect_lines()",
    ];

    let connect = prod(include_str!("main_view/connections.rs"));
    let connect_squeezed = squeezed(&connect);
    for expr in compose {
        assert!(
            connect_squeezed.contains(expr),
            "建会话时没有把 `terminal_paste_behavior` 换算成引擎参数（缺：{expr}）。\n\
             两个引擎参数必须都由同一个枚举算出 —— 各自取值就等于把\
             「先确认 + 逐行执行」的矛盾组合重新引回来。"
        );
    }
    let dialogs = prod(include_str!("main_view/dialogs.rs"));
    let dialogs_squeezed = squeezed(&dialogs);
    for expr in compose {
        assert!(
            dialogs_squeezed.contains(expr),
            "设置热更新时没有重新换算引擎参数：改完粘贴行为要重开会话才生效（缺：{expr}）"
        );
    }

    // `TerminalPaneConfig` → `TerminalConfig` 的透传
    let view = prod(include_str!("terminal_view.rs"));
    assert!(
        view.contains("paste_protect_lines: cfg.paste_protect_lines"),
        "`TerminalPaneConfig` 的 `paste_protect_lines` 没有透传进引擎配置"
    );
}
