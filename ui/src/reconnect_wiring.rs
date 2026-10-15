//! F-4 断线自动重连的护栏（行为 / 接线两层），仅测试构建编译。
//!
//! # 被钉住的五条规矩
//!
//! 1. **账本挂在标签上**，不是按 `profile_id` 记一张全局表（旧实现是
//!    `HashMap<profile_id, u32>`）：同一 profile 开两个标签会互相偷次数，
//!    关标签时那张表没人清 —— 重开一次接着上次用掉的算。本文件里
//!    `the_ledger_lives_on_the_tab` 就是钉这一条的（扫源码，`reconnect_attempts`
//!    这个名字不许再出现）。
//! 2. **掉线只排计划、不当场连**，而且**失败要接着排**：这一条正是 F-4 要修的
//!    缺陷 —— 旧实现每次掉线只重连一次，于是「最大重试次数 = 3」从来没被兑现过
//!    （设 3 也只连 1 次）。三次进入点是 `schedule_retry` / `fire_due_retries` /
//!    `advance_retry_after_failure`，`the_retry_loop_covers_first_attempt_and_continuation`
//!    钉它们都在 `poll` 这条链上。
//! 3. **用户动作作废账本**：右键「连接」/ 断连条上的「重新连接」/ 主动「断开」
//!    都必须清账。不清的后果有两种，direction 相反但都是坏事：用户在等退避时
//!    点了一下重连，账本还照自己的节奏再插一脚（重复连接）；用户主动断开后
//!    断连条还写着「等待重试」，让他以为它还会自己连回来。
//! 4. **连接结果必须落回正在重连的那个标签**（`awaits_connect_result`）：
//!    重连中的标签带着上一次的失败原因，用 `is_connecting()`（要求无失败原因）
//!    去找会漏掉它 —— 漏掉的后果是重连成功时新开一个标签，旧现场变孤儿。
//! 5. **次数与间隔只在全局设置里**，不进连接表单（连接级只有「开 / 关 / 跟随」
//!    这一档三态）：等多久是本机网络条件的产物，不是「这条连接」的属性。
//!
//! 纯函数层（退避序列、`should_retry` 的开关与预算判定）在
//! `core/src/reconnect.rs`；三态随连接走、导出 / 导入保真的镜像在
//! `core/src/connections.rs`。这里只钉界面这一侧的行为与接线。
//!
//! 扫源码一律走 [`crate::test_support::prod`]（剥注释 + 切测试区），
//! 否则针会命中本文件自己。

use crate::test_support::{prod, region};

use rustshell_core::session::{SessionHandle, SessionKind, SessionState};

use crate::main_view::{display_tab_title, take_due_retries, RetryLedger, TabEntry};

// ---------------------------------------------------------------- 造样本

/// 造一个「刚掉线、账本停在这一刻」的标签。`due_in_ms = 0` = 已经到点，
/// 正数 = 还要等那么久。
fn dead_tab_with_ledger(id: &str, attempts: u32, max: u32, due_in_ms: u64) -> TabEntry {
    let mut session = SessionHandle::pending(
        id.to_string(),
        SessionKind::Ssh,
        "机器".to_string(),
        Some(format!("p-{id}")),
    );
    // 掉线现场：状态收成 Closed、`dead` 立起来（终端视图本用例用不到）
    session.state = SessionState::Closed;
    TabEntry {
        session,
        terminal: None,
        scroll: None,
        connect_error: None,
        connect_started: None,
        dead: Some("连接已被远端关闭".to_string()),
        retry: Some(RetryLedger {
            attempts,
            max,
            due_at: Some(std::time::Instant::now() + std::time::Duration::from_millis(due_in_ms)),
        }),
    }
}

// ---------------------------------------------------------------- 行为层

/// 到点的账本**推进一格**：`attempts + 1`、`due_at` 清空（＝「已在飞」），
/// 只返回该发起的那个标签。
///
/// 这一条是重试循环的心脏：`attempts` 不推进 ⇒ 每一轮都算「第 1 次」，
/// 预算永不耗尽（无限重连）；`due_at` 不清 ⇒ 下一轮 300ms 后又发一次
/// （一次退避变成一串连发）。
#[test]
fn a_due_ledger_advances_exactly_one_step() {
    let mut tabs = vec![
        dead_tab_with_ledger("a", 0, 3, 0),      // 已到点
        dead_tab_with_ledger("b", 1, 3, 60_000), // 还早
    ];
    // `now` 必须在**造完标签之后**取：标签里的到期时刻就是「造它的那一刻」，
    // 先取 `now` 会让它比 `now` 晚一点点，于是「已到点」那个也不算到点。
    let due = take_due_retries(&mut tabs, std::time::Instant::now());
    assert_eq!(due, vec!["a".to_string()], "只有到点的那个该发起");

    let a = tabs[0].retry.expect("账本还在（这一轮失败还要接着排）");
    assert_eq!(a.attempts, 1, "发出去之前就要把次数推进一格");
    assert!(
        a.due_at.is_none(),
        "已在飞 = `due_at` 清空，防同一轮重复点火"
    );

    let b = tabs[1].retry.expect("还早的那个账本必须原样留着");
    assert_eq!(b.attempts, 1, "没到点的不许推进次数");
    assert!(b.due_at.is_some(), "没到点的到期时刻不许动");
}

/// 已在飞（`due_at = None`）与没有账本的标签都不该被「到点」逻辑碰。
#[test]
fn an_in_flight_or_ledger_less_tab_is_left_alone() {
    let mut in_flight = dead_tab_with_ledger("fly", 1, 3, 0);
    in_flight.retry.as_mut().unwrap().due_at = None;
    let mut no_ledger = dead_tab_with_ledger("none", 0, 3, 0);
    no_ledger.retry = None;

    let mut tabs = vec![in_flight, no_ledger];
    assert!(
        take_due_retries(&mut tabs, std::time::Instant::now()).is_empty(),
        "「已在飞」再点一次火就是同一条连接发两次；没账本的更不该被自动连"
    );
    assert_eq!(tabs[0].retry.unwrap().attempts, 1, "已在飞的不许再加次数");
}

/// 展示态：等待中显示**将要**发的那一次，在飞时显示当前这一次，
/// 没账本 = 没有重连计划（用户手动重连那条路）。
#[test]
fn the_progress_shows_the_attempt_it_is_waiting_for() {
    // 等待退避：已发过 1 次，断连条该说「第 2/3 次」
    let waiting = dead_tab_with_ledger("a", 1, 3, 5_000);
    let p = waiting.retry_progress().expect("有账本就有进展可显示");
    assert_eq!((p.attempt, p.max, p.waiting), (2, 3, true));

    // 在飞：已发出 2 次，说的就是第 2 次
    let mut flying = dead_tab_with_ledger("b", 2, 3, 0);
    flying.retry.as_mut().unwrap().due_at = None;
    let p = flying.retry_progress().expect("在飞也有进展");
    assert_eq!((p.attempt, p.max, p.waiting), (2, 3, false));

    let mut none = dead_tab_with_ledger("c", 0, 3, 0);
    none.retry = None;
    assert!(none.retry_progress().is_none(), "没账本就没进展可谈");
}

/// 失败后续排：预算还有 ⇒ 按**下一个**退避值排下一次；预算用尽 ⇒ 清账，
/// 并把最终错误留在 `connect_error` 上（断连条那句尾巴）。
///
/// `choice` / `global` 是**实时**读的：用户在重试途中把自动重连关掉，
/// 下一次失败就必须停 —— 这个停止键不能等到重启才生效。`max` 用快照，
/// 所以下面第三段用「快照 3、实时预算也 3」之外没法区分的地方不再展开。
#[test]
fn a_failed_attempt_schedules_the_next_one_until_the_budget_runs_out() {
    // 第一段：预算还剩（已发 1 次 / 上限 3）⇒ 排第 2 次，等 2×间隔
    let mut tab = dead_tab_with_ledger("a", 1, 3, 0);
    tab.retry.as_mut().unwrap().due_at = None;
    let wait = tab
        .advance_retry_after_failure(None, true, 5)
        .expect("预算还有就该排下一次");
    assert_eq!(wait, 10, "第 2 次重试等 2×间隔（退避序列在 core）");
    let r = tab.retry.expect("排了下一次，账本当然还在");
    assert_eq!(r.attempts, 1, "排下一次不等于又发了一次");
    assert_eq!(
        r.attempts + 1,
        tab.retry_progress().unwrap().attempt,
        "断连条接下来该说「第 2/3 次」"
    );
    assert!(r.due_at.is_some(), "排好下一次就必须有到期时刻");
    assert!(
        tab.connect_error.is_none(),
        "最终错误由 `poll` 的失败分支负责挂上，这里不越权"
    );

    // 第二段：预算用尽（已发 3 次 / 上限 3）⇒ 清账 + 最终错误留着
    let mut done = dead_tab_with_ledger("b", 3, 3, 0);
    done.retry.as_mut().unwrap().due_at = None;
    done.connect_error = Some("连接超时".to_string());
    assert!(
        done.advance_retry_after_failure(None, true, 5).is_none(),
        "上限用尽就不许再排"
    );
    assert!(
        done.retry.is_none(),
        "用尽即清账（不许留一个会再开火的计划）"
    );
    assert_eq!(
        done.connect_error.as_deref(),
        Some("连接超时"),
        "次数用尽后，最终那条错误必须留在断连条上给用户看"
    );

    // 第三段：停止键 —— 全局关掉、连接级也没强制开 ⇒ 立刻停
    let mut stopped = dead_tab_with_ledger("c", 1, 3, 0);
    stopped.retry.as_mut().unwrap().due_at = None;
    assert!(
        stopped
            .advance_retry_after_failure(None, false, 5)
            .is_none(),
        "关掉自动重连之后还要接着排 = 这个开关是假的"
    );
    assert!(stopped.retry.is_none());

    // 第四段：连接级「总是重连」能顶住全局的关（三态的意义）
    let mut forced = dead_tab_with_ledger("d", 1, 3, 0);
    forced.retry.as_mut().unwrap().due_at = None;
    assert!(
        forced
            .advance_retry_after_failure(Some(true), false, 5)
            .is_some(),
        "连接级「总是重连」在全局关掉时仍要重试（这正是三态存在的理由）"
    );
}

/// 「等一次连接结果」的判定必须把**断连现场上正在重连**算进来。
///
/// 漏掉它的症状只在重连后出现：新标签凭空多一个（重连成功那条分支找不到
/// 落点，走了「新建」兜底），而断连现场带着旧画面留在标签栏里。
#[test]
fn a_reconnecting_dead_tab_awaits_the_connect_result() {
    let mut tab = dead_tab_with_ledger("a", 1, 3, 5_000);
    // 还带着上一次的失败原因（断连条要显示它）—— 这正是 `is_connecting()` 会漏掉的形态
    tab.connect_error = Some("连接超时".to_string());
    assert!(
        !tab.is_connecting(),
        "前提：带失败原因的标签按 `is_connecting` 判定就是「不在等」"
    );

    tab.session.state = SessionState::Connecting;
    assert!(
        tab.awaits_connect_result(),
        "断连现场上正在重连，连接结果必须落回它 —— 否则重连成功会新开一个标签"
    );

    // 还活着的占位（首次连接）同样算在等
    let mut fresh = dead_tab_with_ledger("b", 0, 3, 0);
    fresh.dead = None;
    fresh.session.state = SessionState::Connecting;
    assert!(fresh.awaits_connect_result(), "首次连接的占位当然在等");

    // 只是断连、没人去连：不在等（连接结果不该往它身上落）
    let idle = dead_tab_with_ledger("c", 0, 3, 0);
    assert!(
        !idle.awaits_connect_result(),
        "干断着、没在重连的标签不该被当成「在等连接结果」"
    );
}

/// 标签标题在**这一轮真的在连**时带次数；等退避时不带（说「重连中」是假话）。
#[test]
fn the_tab_title_marks_reconnecting_but_not_waiting() {
    let store = rustshell_core::connections::ConnectionStore::default();
    let _ = &store; // 标题里的「连接配置已删除」后缀与本条无关

    let mut flying = dead_tab_with_ledger("a", 2, 3, 0);
    flying.retry.as_mut().unwrap().due_at = None;
    flying.session.state = SessionState::Connecting;
    let title = display_tab_title(&flying, &store);
    assert!(
        title.contains("2/3"),
        "重连中的标签必须在标签栏上看出「第 2/3 次」，实际：{title}"
    );

    let waiting = dead_tab_with_ledger("b", 2, 3, 5_000);
    let title = display_tab_title(&waiting, &store);
    assert!(
        !title.contains("2/3"),
        "退避等待期间不许说「重连中」——那一句归断连条说，实际：{title}"
    );

    // 用户手动重连（无账本）：说「正在连」但没有次数
    let mut manual = dead_tab_with_ledger("c", 2, 3, 0);
    manual.retry = None;
    manual.session.state = SessionState::Connecting;
    let title = display_tab_title(&manual, &store);
    assert!(
        !title.contains("2/3"),
        "手动重连没有账本，不该凭空出现次数，实际：{title}"
    );
}

// ---------------------------------------------------------------- 接线层

/// 账本挂在标签上：全仓不许再出现按 profile 记的那张表。
#[test]
fn the_ledger_lives_on_the_tab() {
    for (name, src) in [
        ("main_view.rs", prod(include_str!("main_view.rs"))),
        (
            "main_view/connections.rs",
            prod(include_str!("main_view/connections.rs")),
        ),
        ("tab_bar.rs", prod(include_str!("tab_bar.rs"))),
    ] {
        assert!(
            !src.contains("reconnect_attempts"),
            "{name} 里还有按 profile 记的重连计数表：\n\
             同一 profile 的两个标签会互相偷次数，关标签时也没人清它。\n\
             账本必须挂在 `TabEntry::retry` 上（标签的生命周期就是账本的生命周期）。"
        );
    }
    // ★ 2026-10-12（开源评审 P0-4 刀二）：`TabEntry` 已从 `main_view.rs` 搬进
    // `main_view/tab_model.rs`，所以这条断言的源码源必须跟着走。
    // 护栏的源不跟着搬移走，就会退化成「断言 main_view.rs 里没有某个东西」——
    // 而那里已经根本不该有（断言恒真，形同虚设）。
    let tab_model = prod(include_str!("main_view/tab_model.rs"));
    assert!(
        tab_model.contains("pub retry: Option<RetryLedger>,"),
        "`TabEntry` 必须带 `retry` 字段（F-4 账本的唯一落点）"
    );
}

/// 重试循环的三个进入点都在 `poll` 这条链上，且第一次尝试也走退避。
#[test]
fn the_retry_loop_covers_first_attempt_and_continuation() {
    let main = prod(include_str!("main_view.rs"));
    let session = prod(include_str!("main_view/connections.rs"));

    // ① 掉线：排计划（不是当场 `reconnect_tab`）
    let drop_loop = region(
        &main,
        "for (id, pid, reason) in dropped {",
        "self.fire_due_retries(cx);",
    );
    assert!(
        drop_loop.contains("self.schedule_retry(&id, pid.as_deref());"),
        "掉线处理必须走 `schedule_retry`（排一个带退避的计划），\n\
         而不是当场 `reconnect_tab` —— 当场连既没有退避，也是「只连一次」的老毛病。"
    );
    assert!(
        !drop_loop.contains("self.reconnect_tab("),
        "掉线处理里不许直接调 `reconnect_tab`：那条路是**用户动作**（会清账），\n\
         从掉线分支调它等于计划刚排上就被自己作废。"
    );

    // ② 到点：`poll` 里推进
    assert!(
        main.contains("self.fire_due_retries(cx);"),
        "`poll` 必须调用 `fire_due_retries`：到期判定挂在 300ms 轮询上，\n\
         不调它 = 计划排了永远没人点火。"
    );
    let fire = region(
        &session,
        "pub(super) fn fire_due_retries",
        "pub fn reconnect_tab(&mut self, id: &str",
    );
    assert!(
        fire.contains("take_due_retries("),
        "点火必须走 `take_due_retries`（推进账本的唯一落点，且可单测）"
    );

    // ③ 失败续排：`poll` 的失败分支必须推进账本
    assert!(
        main.contains("advance_retry_after_failure("),
        "连接失败分支必须调用 `advance_retry_after_failure` ——\n\
         少了它，重试永远停在第一次（这正是 F-4 要修的缺陷：\n\
         「最大重试次数」设了没人兑现）。"
    );

    // ④ 退避与判定都来自 core 的纯函数，界面不许自己算
    let schedule = region(
        &session,
        "pub(super) fn schedule_retry",
        "pub(super) fn fire_due_retries",
    );
    assert!(
        schedule.contains("rustshell_core::reconnect::should_retry")
            && schedule.contains("rustshell_core::reconnect::backoff_secs"),
        "「排不排」与「等多久」都必须问 core：界面各算一份，\n\
         两处迟早在边界上分叉（一处按 5 秒、一处按 10 秒）。"
    );
    assert!(
        schedule.contains("p.auto_reconnect"),
        "排计划必须读**连接级三态** `p.auto_reconnect`：只读全局开关，\n\
         连接表单上那一行就是摆设（用户选了「从不重连」也照连）。"
    );
    assert!(
        main.contains("self.settings.reconnect_interval_secs"),
        "间隔必须从设置里读（挂死在代码里的间隔改不动）。"
    );
}

/// 用户动作 / 断开都要清账；真正发起连接的那个函数不许碰账本。
#[test]
fn only_user_actions_invalidate_the_ledger() {
    let session = prod(include_str!("main_view/connections.rs"));

    let user = region(
        &session,
        "pub fn reconnect_tab(&mut self, id: &str",
        "fn start_reconnect(&mut self, id: &str",
    );
    assert!(
        user.contains("tab.retry = None;"),
        "用户点「重新连接」必须清账：不清的话账本会照自己的节奏在后台再插一脚\n\
         （用户看到的是「我刚点了重连，它又自己连了一次」）。"
    );

    let start = region(
        &session,
        "fn start_reconnect(&mut self, id: &str",
        "pub fn duplicate_tab(",
    );
    assert!(
        !start.contains("retry = None"),
        "`start_reconnect` 是自动重连与用户动作**共用**的发起函数：\n\
         在这里清账等于自动重连排完计划就自己作废（永远只连一次）。"
    );

    let disconnect = region(
        &session,
        "pub fn disconnect_tab(&mut self, id: &str",
        "pub fn mark_session_dead",
    );
    assert!(
        disconnect.contains("tab.retry = None;"),
        "用户主动「断开」必须清账：留着它，断连条上会写着「等待重试」，\n\
         让用户以为它还会自己连回来 —— 而他已经把这条路关掉了。"
    );

    // 连上之后账本作废：成功分支在 `poll`（父模块）里清，失败分支的续排
    // 由 `advance_retry_after_failure` 自己收尾。
    let main = prod(include_str!("main_view.rs"));
    assert!(
        main.contains("tabs[ix].retry = None;"),
        "重连成功必须清账（`tabs[ix].retry = None`）：不清的话下次掉线\n\
         会接着上一轮用掉的次数算，断连条显示 3/3 而实际一次都没试。"
    );
}

/// 连接结果的两个落点都必须用 `awaits_connect_result`。
#[test]
fn connect_results_land_on_the_reconnecting_tab() {
    let main = prod(include_str!("main_view.rs"));
    assert_eq!(
        main.matches("t.awaits_connect_result()").count(),
        2,
        "成功与失败两条落点都要用 `awaits_connect_result`：\n\
         用 `is_connecting` 会漏掉「带着上次失败原因的重连标签」，\n\
         症状是重连成功反而新开一个标签、旧断连现场变孤儿。"
    );
}

/// 设置面板：间隔行绑定 `settings.reconnect_interval_secs`，且过 `clamp_interval`。
#[test]
fn the_settings_panel_binds_the_interval_to_settings() {
    let panel = prod(include_str!("main_view/settings_panel.rs"));
    assert!(
        panel.contains("settings.reconnect_interval_secs"),
        "设置面板必须直接读写 `settings.reconnect_interval_secs`（写成一份 UI 私有状态，\n\
         面板上改得动、落盘的却不是它）。"
    );
    assert!(
        panel.contains("rustshell_core::reconnect::clamp_interval("),
        "写回间隔必须过 `clamp_interval`：手改 settings.json 写出 0 秒\n\
         就是忙等重连（每秒撞一次，对端会当你在扫端口）。"
    );
    assert!(
        panel.contains("DEFAULT_RETRY_INTERVAL_SECS")
            && panel.contains("rustshell_core::reconnect::DEFAULT_INTERVAL_SECS"),
        "「重置」按钮的默认值必须与 core 的默认同源，否则点重置会重置成 0 秒。"
    );
}

/// 连接表单：三态行按用户要求**隐藏**（2026-09-29「隐藏新增连接-高级相关的内容」），
/// 但回填 / 清态 / 保存两个分支原样保留 —— 隐藏不等于清空。
#[test]
fn the_hidden_three_state_override_still_round_trips() {
    let session = prod(include_str!("main_view/connections.rs"));
    let dialogs = prod(include_str!("main_view/dialogs.rs"));

    // 界面已撤：循环按钮与三态行都不该还在渲染代码里
    assert!(
        !dialogs.contains(r#"Button::new("dlg-auto-reconnect")"#)
            && !dialogs.contains("dlg_auto_reconnect_cycle"),
        "「断线自动重连」三态行已随「高级」界面隐藏：\n\
         对话框里不该再有它的按钮或循环函数（留着会让重开「高级」时状态两处来源）。"
    );

    // 编辑回填
    let edit = region(
        &session,
        "pub(super) fn open_connect_dialog_for_profile",
        "dlg_group_id = p.group_id.clone();",
    );
    assert!(
        edit.contains("dlg_auto_reconnect = p.auto_reconnect;"),
        "编辑必须回填这条连接的三态，否则一保存就把它抹成「跟随全局」。"
    );

    // 新建清态
    let fresh = region(&session, "pub fn open_connect_dialog(", "dlg_notes");
    assert!(
        fresh.contains("dlg_auto_reconnect = None;"),
        "新建必须把三态回零：不回零，新建的连接会带着上一条的「从不重连」落库，\n\
         而界面上已经没有那行字，用户更无从察觉。"
    );

    // 保存两个分支
    assert_eq!(
        dialogs
            .matches("p.auto_reconnect = auto_reconnect;")
            .count(),
        2,
        "编辑与新建两条分支都要写回三态（少一条，那一半的连接存下去就丢了设置）。"
    );

    // 次数与间隔**不进**连接表单（连接属性 vs 本机习惯的分界）
    assert!(
        !dialogs.contains("reconnect_max_retries") && !dialogs.contains("reconnect_interval_secs"),
        "重试次数与间隔不许出现在连接表单里：\n\
         它们是本机网络条件的产物（换台机器该重新判断），不是「这条连接」的属性。"
    );
}

/// 断连条的两种句式都在（「正在重连…（第 n/m 次）」/「等待重试（第 n/m 次）」）。
#[test]
fn the_dead_bar_says_whether_it_is_connecting_or_waiting() {
    let split = prod(include_str!("main_view/split_actions.rs"));
    assert!(
        split.contains("等待重试（第 {}/{} 次）") && split.contains("正在重连…（第 {}/{} 次）"),
        "断连条要把「已经发出去了」和「还要等一会儿」分开说：\n\
         用户对着一个不再刷新的画面时，这两句话是他判断「该等我该自己点」的全部依据。"
    );
    assert!(
        split.contains("retry: tab.retry_progress(),"),
        "断连条的数据必须来自 `retry_progress()`（唯一读法），\n\
         否则次数与上限各算一遍，两处迟早对不上。"
    );
    // 计划里不许出现「界面自己算退避」的痕迹：退避只有一处（core 的纯函数）。
    // 界面各算一份，两处迟早在边界上分叉（一处按 5 秒、一处按 10 秒）。
    let session = prod(include_str!("main_view/connections.rs"));
    for (name, src) in [("split_actions.rs", &split), ("connections.rs", &session)] {
        assert!(
            !src.contains("BACKOFF_CAP_SECS"),
            "{name} 在自己算退避封顶 —— 退避只有一处（`core::reconnect::backoff_secs`）。"
        );
    }
}

/// `MainView` 上不许再留重连相关的平行状态（账本只有 `TabEntry::retry` 一处）。
#[test]
fn the_view_keeps_no_parallel_retry_state() {
    let main = prod(include_str!("main_view.rs"));
    for needle in ["reconnect_attempts", "retry_due_at", "reconnect_timer"] {
        assert!(
            !main.contains(needle),
            "`main_view.rs` 里出现了 `{needle}`：\n\
             重连的进度只许记在标签的账本上（第二份状态一定会与它分叉）。"
        );
    }
    // ★ 2026-10-12（刀二）：`take_due_retries` 与 `TabEntry` 一起搬进了
    // `main_view/tab_model.rs`，断言源随之改指（见本文件另一处同名说明）。
    let tab_model = prod(include_str!("main_view/tab_model.rs"));
    assert!(
        tab_model.contains("pub(crate) fn take_due_retries("),
        "到期扫描必须留在 `take_due_retries`（可传入 `now` 的纯函数）——\n\
         塞回 `poll` 里就只能靠真时钟等，等于不可测。"
    );
}
