//! F-8 系统托盘的三层护栏（仅测试构建编译）。
//!
//! # 被钉住的五条规矩
//!
//! 1. **关窗守卫必须能被三条「如实放行」的理由穿透** —— 托盘没建起来 / 开关没开 /
//!    窗口藏不起来，任一成立就照常关窗。守错的代价是「点了 X 什么都看不到」：
//!    窗口藏起来了，但托盘图标没挂上（或用户根本没开这个功能），只能去任务管理器。
//! 2. **托盘菜单动作只有一个消费点**（`MainView::poll`）。`muda` 把点击投进
//!    **进程级**通道，谁都能读；一旦多出一处消费者，「同一次点击被消费两次」
//!    这类问题在实机上表现为随机行为，靠读代码基本查不出来。
//! 3. **托盘「退出」直接退出进程**（2026-09-29 用户反馈「托盘右键退出失效，
//!    应该直接关掉进程，而不是界面提示」）：`ui` 里唯一的 `cx.quit()` 住在
//!    `poll` 的 `TrayAction::Quit` 臂里。原先「退出必须先过确认框」的
//!    `render_confirm_quit_dialog` 整条撤掉 —— 用户点了「退出」就得退出。
//! 4. **悬停提示必须带上断线数**（规格 §1.8「托盘提示断线状态」）—— 纯函数
//!    [`crate::main_view::tray_tooltip`] 直接单测，不必去问系统通知区。
//! 5. **窗口的收放只能走异步投递，且「抢前台」必须在 `render` 里兑现**（2026-10-10
//!    运行反馈「托盘右键的『显示主窗口』无效」）：同步的 `ShowWindow` 会在
//!    `poll`（= `Entity::update` 的借用里）就地派发 `WM_SHOWWINDOW`，gpui 的处理
//!    回调回来后构成重入借用 ⇒ panic ⇒ 收放中途夭折，界面上**毫无反馈**；
//!    而 `poll` 又拿不到 `Window`，「提到最前」只能由 `render` 两段式兑现。
//!    两条都钉在 [`tray_actions_are_consumed_in_exactly_one_place`] 里。
//!
//! 2026-10-10：托盘菜单撤掉「新建终端」（用户要求）。窗口收在托盘里时它把标签
//! 开在**看不见的地方**，点了没有反馈 —— 终端入口在快捷键与工具栏上，托盘只留
//! 「显示主窗口 / 退出」这两条与「窗口」直接相关的动作。
//!
//! 扫源码一律走 [`crate::test_support::prod`]（剥注释 + 切测试区），否则针会
//! 命中本文件自己（见 `test_support` 模块文档）。

use crate::test_support::prod;

// ---------------------------------------------------------------- 契约层

/// 悬停提示必须把「几个会话活着 / 几个掉线了」都摆出来。
///
/// 断言刻意**不查具体中文**：`i18n::LANG` 是进程级全局，别的用例（`metrics` /
/// `activity_bar` / `i18n` 自己的用例）会切英文，查文案会让这条护栏变成偶发红。
/// 只钉「数字确实进去了、断线数与活跃数各占一位、两者不会撞成同一个字符串」。
#[test]
fn the_tray_tooltip_carries_the_session_and_disconnect_counts() {
    let empty = crate::main_view::tray_tooltip(0, 0);
    assert!(
        empty.starts_with(rustshell_core::APP_NAME),
        "提示要以产品名开头（通知区里可能有好几个图标）：{empty}"
    );
    assert!(
        !empty.contains(|c: char| c.is_ascii_digit()),
        "没有会话时不该编出数字：{empty}"
    );

    let two_live = crate::main_view::tray_tooltip(2, 0);
    assert!(two_live.contains('2'), "活跃会话数没进提示：{two_live}");
    assert!(
        !two_live.contains('0'),
        "断线数为 0 时不该在提示里塞一个 0：{two_live}"
    );

    let two_live_one_dead = crate::main_view::tray_tooltip(2, 1);
    assert!(
        two_live_one_dead.contains('2') && two_live_one_dead.contains('1'),
        "活跃数与断线数都得进去：{two_live_one_dead}"
    );
    assert_ne!(
        two_live, two_live_one_dead,
        "「有断线」和「没有断线」的提示必须不一样 —— 规格 §1.8 要求托盘能提示断线状态。"
    );

    // 单数/复数不能靠拼接：1 个会话与 2 个会话的差异是整句级的。
    assert_ne!(
        crate::main_view::tray_tooltip(1, 0),
        crate::main_view::tray_tooltip(2, 0),
        "会话数不同的提示必须不同。"
    );
}

// ---------------------------------------------------------------- 接线层

/// 关窗守卫：三条放行理由 + 「藏成功才拦」。
#[test]
fn the_close_guard_still_lets_the_window_close_when_it_cannot_hide() {
    // 区域切到**下一个函数**（`install_tray`）为止 —— 不取调用点，否则后半截
    // 别的代码会喂饱断言（本仓踩过：区间终点取调用点等于没切）。
    let code = prod(include_str!("../../shell/src/app.rs"));
    let start = code
        .find("fn wire_close_to_tray")
        .expect("关窗守卫没了：托盘开了之后点 X 会直接退出，收不进托盘。");
    let end = code[start..]
        .find("fn install_tray")
        .expect("关窗守卫之后应当是 `install_tray`（换个名字就更新这条端点）");
    let guard = &code[start..start + end];

    for needle in [
        "on_window_should_close(",
        "rustshell_platform::tray::is_ready()",
        "rustshell_platform::tray::close_to_tray()",
    ] {
        assert!(
            guard.contains(needle),
            "关窗守卫缺 `{needle}`：托盘不可用 / 开关没开时，用户点 X 会看到窗口消失\n\
             但托盘图标根本不在 —— 只能去任务管理器杀进程。"
        );
    }
    // 关键的一行：**藏成功才拦**。写成无条件 `false` 就是「关不掉的窗口」。
    assert!(
        guard.contains("!rustshell_platform::window_ctl::set_visible(false)"),
        "关窗守卫必须以「藏成功 ⇒ 返回 false」收尾；写成别的形式（例如先 return false\n\
         再调 set_visible）会在句柄没登记时留下一个关不掉的窗口。"
    );
    // 守卫自己不许退出：它跑在窗口过程里，退出要 PostQuitMessage，语义完全不同。
    assert!(
        !guard.contains("quit()"),
        "关窗守卫里不许出现 quit()：那条路绕开关闭确认，用户点 X 会丢掉所有会话。"
    );
}

/// 托盘菜单动作只在 `MainView::poll` 里被取走。
#[test]
fn tray_actions_are_consumed_in_exactly_one_place() {
    let hits = grep_sources("tray::drain_actions()");
    assert_eq!(
        hits.len(),
        1,
        "`tray::drain_actions()` 必须**只有一处**调用（唯一消费点），实际命中：{hits:?}\n\
         `muda` 的事件通道是**进程级**的：多一处消费者就会出现「同一次点击被消费两次」，\n\
         而症状在实机上是随机的，靠读代码基本查不出来。"
    );
    assert!(
        hits[0].0.ends_with("ui/src/main_view.rs"),
        "唯一消费点必须在 `MainView::poll`（主线的事件节拍）里，实际在：{}",
        hits[0].0
    );

    // 消费点必须落在 poll 的体内：放到别处（例如某个只在窗口获得焦点时才跑的
    // 回调）会让「点托盘菜单没反应」变成间歇性的。
    let code = prod(include_str!("main_view.rs"));
    let poll_at = code
        .find("fn poll(&mut self, cx: &mut Context<Self>) -> bool {")
        .expect("poll 没了");
    let poll_end = code[poll_at..]
        .find("fn render_no_session_notice")
        .expect("poll 之后应当是 `render_no_session_notice`（换个名字就更新这条端点）");
    let poll = &code[poll_at..poll_at + poll_end];
    assert!(
        poll.contains("rustshell_platform::tray::drain_actions()"),
        "托盘动作不该挪出 `poll`：它是唯一一个「每 300ms 必然跑一次」的节拍，\n\
         挪到事件回调里就会漏掉「窗口不可见时点的菜单」。"
    );

    // 两个动作都必须有落点，一个都不许被 `_ => {}` 吞掉。
    for action in ["TrayAction::ShowWindow", "TrayAction::Quit"] {
        assert!(
            poll.contains(action),
            "托盘动作 `{action}` 没有落点（被 `_ => {{}}` 吞了？）：菜单项点了没反应。"
        );
    }

    // 2026-10-10：托盘不再有「新建终端」——窗口收在托盘里时它把标签开在**看不见
    // 的地方**，用户点完毫无反馈。撤干净的三条：动作变体、id 常量、开标签的调用。
    for needle in ["TrayAction::NewTerminal", "ID_NEW_TERM"] {
        let hits = grep_sources(needle);
        assert!(
            hits.is_empty(),
            "托盘「新建终端」没撤干净：全仓还留着 `{needle}`，命中 {hits:?}。\n\
             留着它，菜单项就能被一行代码复活，而复活它的人看不到这段历史。"
        );
    }
    assert!(
        !poll.contains("open_local_shell"),
        "托盘动作里又出现了开标签的调用：窗口正收在托盘里时，它只会把终端开在\n\
         用户看不见的地方（点了没有任何反馈）。终端入口在快捷键与工具栏上。"
    );

    // 「显示主窗口」的**前半段**：收放窗口只能异步投递。
    //
    // 同步的 `ShowWindow` 会就地给窗口过程派发 `WM_SHOWWINDOW`，而本调用点处在
    // `Entity::update` 的借用里 ⇒ gpui 的回调构成重入借用 ⇒ 它直接 panic、
    // `ShowWindow` 中途夭折，界面上毫无反馈（2026-10-10 实测：点三次，日志里
    // 三条 `ERROR [] RefCell already borrowed`，窗口一次都没出现）。
    let wctl = prod(include_str!("../../platform/src/window_ctl.rs"));
    assert!(
        wctl.contains("ShowWindowAsync("),
        "窗口收放没有走异步版本：同步的 `ShowWindow` 会在 `poll` 的借用里重入\n\
         gpui（原因见 `window_ctl` 的模块文档），表现是「点了没反应」。"
    );
    assert!(
        !wctl.contains("ShowWindow("),
        "`window_ctl` 里出现了同步的 `ShowWindow(`：它会在 gpui 的借用里重入。\n\
         一律用 `ShowWindowAsync`（gpui 自己的 `activate()` 也是这么做的）。"
    );

    // 「显示主窗口」的**后半段**：抢前台要 `Window`，只能在 `render` 里兑现
    // （铁律 #28 的两段式）。旗只置一次、也只消费一次 —— 乱置就会「莫名抢前台」。
    assert!(
        poll.contains("tray_show_pending = true;"),
        "托盘「显示主窗口」没有把「抢前台」这一步留给 `render`：`poll` 拿不到 `Window`。"
    );
    let render_at = code
        .find("impl Render for MainView {")
        .expect("`MainView` 的 Render 实现没了");
    // 终点取**代码**锚点（`prod()` 剥过注释，这里不能拿文档注释当端点）。
    //
    // ★ 2026-10-12（开源评审 P0-4 刀一）：端点曾是 `pub struct AppShell {`
    // （`MainView` 的 render 之后紧跟着它）。`AppShell` 与四个子视图搬进
    // `main_view/app_shell.rs` 之后，`MainView` 的 render 成了本文件**最后**
    // 一个实现块。
    //
    // 于是不再需要另找终点：`prod()` 已经把从第一个 `#[cfg(test)]` 起的内容
    // 全部切掉（见 `test_support::prod`），所以 `code` 到此就是「生产代码的末尾」。
    // 直接切到串尾。
    //
    // 为什么不能改成「对原始源码取到文件末尾」：那样会把整个测试模块
    // （几千行）当成 render 的正文，这条断言就退化成
    // 「整个文件里有没有这两行」—— 覆盖面被**悄悄放大**，比端点失效更难发现。
    let render = &code[render_at..];
    assert!(
        render.contains("window.activate_window();")
            && render.contains("tray_show_pending = false;"),
        "`render` 没有兑现托盘「显示主窗口」的后半段：`Window::activate_window()`\n\
         （它内部 spawn 到 executor，所以不会重入）+ 把旗清掉，两者缺一不可。"
    );
    // 「退出」直接退出进程（2026-09-29 用户反馈「托盘右键退出失效，应该直接
    // 关掉进程，而不是界面提示」）：不许再退回「置旗 → 弹确认框」的两段式。
    assert!(
        poll.contains("rustshell_platform::tray::TrayAction::Quit => cx.quit(),"),
        "托盘「退出」必须在 poll 的 Quit 臂里直接 `cx.quit()`：\n\
         用户点了「退出」就得退出，不能在界面上再拦一道。"
    );
    assert!(
        !poll.contains("present_dialog"),
        "poll 里不许开对话框（它只有 `&mut Context`，拿不到 `Window`）：\n\
         「开框」这一步必须留给 `render`（铁律 #28 的两段式）。"
    );
}

/// 托盘「退出」直接退出进程：唯一的一条 `cx.quit()` 就在 poll 的 Quit 臂里。
///
/// 历史：2026-09-29 之前这里守的是反面（「退出必须先过确认框」，规格 §1.8）。
/// 用户反馈托盘右键退出失效、要求直接关掉进程 —— 确认框整条撤掉，护栏随之
/// 掉头：现在要钉的是「点一下真的退出，中间不许再横一道界面」。
#[test]
fn the_tray_quit_action_quits_the_process_directly() {
    // 全仓（core/ui/shell/platform）**只有一条**退出路径：退出是「点一下就该发生」
    // 的动作，多一条路径就多一种「点哪个才真退」的解释。
    let hits = grep_sources("cx.quit()");
    assert_eq!(
        hits.len(),
        1,
        "`ui` 里不该有多条退出路径：已知唯一一条是 poll 的 `TrayAction::Quit` 臂。\n\
         实际命中：{hits:?}"
    );
    assert!(
        hits[0].0.ends_with("ui/src/main_view.rs"),
        "唯一一条 `cx.quit()` 必须在 `main_view.rs`（poll 的 Quit 臂），实际在：{}",
        hits[0].0
    );

    // 它必须落在 poll 体内、且就在 Quit 臂上：挪到别处（例如某个只在窗口获得
    // 焦点时才跑的回调）会让「点托盘退出没反应」变成间歇性的。
    let code = prod(include_str!("main_view.rs"));
    let poll_at = code
        .find("fn poll(&mut self, cx: &mut Context<Self>) -> bool {")
        .expect("poll 没了");
    let poll_end = code[poll_at..]
        .find("fn render_no_session_notice")
        .expect("poll 之后应当是 `render_no_session_notice`（换个名字就更新这条端点）");
    let poll = &code[poll_at..poll_at + poll_end];
    assert!(
        poll.contains("rustshell_platform::tray::TrayAction::Quit => cx.quit(),"),
        "托盘「退出」没有直接落在 poll 的 Quit 臂上。"
    );
    assert!(
        !poll.contains("pending_quit_confirm"),
        "poll 里又出现了退出确认的待办旗：整条撤掉的机制别以别的名字复活。"
    );

    // 确认框的函数与枚举变体都要一起撤 —— 只删一处会留下「渲染不到的死代码」。
    let dialogs = prod(include_str!("main_view/dialogs.rs"));
    assert!(
        !dialogs.contains("render_confirm_quit_dialog"),
        "退出确认框还在 `dialogs.rs` 里：它的唯一入口（托盘退出）已经改直连，\n\
         这个函数现在谁也走不到，留着只会让后来者以为还有一条确认路径。"
    );
    assert!(
        !code.contains("DialogMode::ConfirmQuit"),
        "`DialogMode::ConfirmQuit` 变体还在：确认框的接线没撤干净。"
    );
}

/// 托盘必须在 `open_window` 之后、消息循环开始之前装好。
#[test]
fn the_tray_is_installed_after_the_window_and_before_the_message_loop() {
    let code = prod(include_str!("../../shell/src/app.rs"));
    let open_at = code.find("cx.open_window(").expect("主窗口建立没了");
    let install_at = code
        .find("install_tray(&settings)")
        .expect("托盘安装没了：图标永远不出现。");
    assert!(
        open_at < install_at,
        "`install_tray` 必须在 `cx.open_window` **之后**：托盘要自建一个隐藏窗口，\n\
         而窗口创建要求本线程已经有消息队列（gpui 的 `run` 初始化闭包正好在这个窗口期）。"
    );

    // 句柄登记与关窗守卫都必须在 open_window 的闭包**开头** ——
    // 用户第一次点关闭按钮时它们就得已经生效，不能等某个后续初始化步骤。
    let closure_region = &code[open_at..install_at];
    for needle in [
        "register_main_window_handle(window);",
        "wire_close_to_tray(window, cx);",
    ] {
        assert!(
            closure_region.contains(needle),
            "`{needle}` 必须在 `cx.open_window` 的闭包里就地调用。"
        );
    }
    // 窗格视图之后才有意义（句柄是给窗口用的），但顺序上必须在开窗闭包内即可。
    assert!(
        closure_region.find("register_main_window_handle(window);")
            < closure_region.find("let view = cx.new(|cx| MainView::new(window, cx));"),
        "句柄登记必须在建 `MainView` 之前：间隔期用户点 X，守卫读到的是未登记的句柄。"
    );
}

/// 「关闭按钮的行为」这一位必须在**三处**都推给 platform。
///
/// 它是「立即生效」的那一类设置（关窗守卫读的是 platform 里的原子位，不是设置
/// 快照）：开关点击、保存、取消回滚三处只要漏一处，就会出现「面板上写着开、
/// 点 X 却直接退出」这种自相矛盾的状态。
#[test]
fn the_close_to_tray_switch_is_pushed_on_every_path() {
    let panel = prod(include_str!("main_view/settings_panel.rs"));
    assert!(
        panel.contains("m.sync_close_to_tray();"),
        "设置面板的开关必须在改动时就推给 platform（守卫读的是 platform 的原子位）。"
    );

    let actions = prod(include_str!("main_view/settings_actions.rs"));
    // 区间取**下一个函数**而不是「闭合大括号」：本文件里既有 `pub(super) fn` 也有
    // `pub fn`，缩进不统一，靠 `\n    }\n` 定位会在缩进一变就 panic（实测踩过）。
    let revert_at = actions
        .find("pub(super) fn revert_settings")
        .expect("`revert_settings` 没了");
    let revert_end = actions[revert_at..]
        .find("pub fn prefill_ai_inputs")
        .expect("`revert_settings` 之后应当是 `prefill_ai_inputs`（换名就更新这条端点）");
    assert!(
        actions[revert_at..revert_at + revert_end].contains("self.sync_close_to_tray();"),
        "取消（回滚快照）也必须把这一位推回去：否则「改了开关 → 点取消」之后，\n\
         面板显示的是旧值，而关窗行为已经是新值了。"
    );
}

// ---------------------------------------------------------------- 工具

/// 在四个 crate（core / ui / shell / platform）的 `src` 里数某个字符串出现了几次。
///
/// 返回 `(相对路径, 行号)`。两件事必须做对：
///   * **先 `prod()` 再 `contains`** —— 否则注释里的举例会算成命中；
///   * **跳过本文件自己** —— 本文件的断言里就写着那些「针」（`cx.quit()` 等），
///     而本文件是被 `#[cfg(test)] mod` 声明进来的，文件内**没有** `#[cfg(test)]`
///     可供 `prod()` 截断 ⇒ 不跳过就会自我命中（负向断言恒失败）。
///     用 `file!()` 而不是写死文件名：将来改名/挪目录都不会静默失效。
fn grep_sources(needle: &str) -> Vec<(String, usize)> {
    let me = file!().replace('\\', "/");
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("ui 目录必须有父目录（= 仓库根）")
        .to_path_buf();
    let mut files = Vec::new();
    for c in ["core", "ui", "shell", "platform"] {
        collect_rs(&root.join(c).join("src"), &mut files);
    }
    let mut hits = Vec::new();
    for f in files {
        let Ok(text) = std::fs::read_to_string(&f) else {
            continue;
        };
        let rel = f
            .strip_prefix(&root)
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|_| f.display().to_string());
        if rel.ends_with(&me) {
            continue;
        }
        let code = prod(&text);
        for (ix, _) in code.match_indices(needle) {
            hits.push((rel.clone(), code[..ix].matches('\n').count() + 1));
        }
    }
    hits
}

fn collect_rs(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect_rs(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}
