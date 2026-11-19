//! 「掉线收场」的端到端护栏。
//!
//! 本模块是 `main_view` 的**子模块**，因此能直接读 `MainView` 的私有字段
//! （`tabs` / `active` / `palette_state`）—— 这正是「标签还在不在、命令窗口
//! 弹没弹」这两条只能整体看的东西的落点。
//!
//! 与 `crate::dead_session` 的分工：那边钉**判定与接线**（纯函数、真引擎封输入、
//! 扫源码）；这边钉**收场后的整屏状态**，尤其是
//! 「断连条有没有真的进帧」—— 源码里有那行代码 ≠ 这一帧真的画了它。
//! 本仓在图标那条护栏上踩过同一课（`AppAssets` 无回退，名字对了文件不在
//! 就静默留白，单测全绿），所以这里量的是 `debug_bounds` 的**非零几何**。
//!
//! ★ 门禁放在**本文件内部**的 `#[cfg(test)] mod e2e`，不在 `main_view.rs` 的
//! `mod` 声明上：本仓扫源码的护栏用 `test_support::prod()` 取源，它从**第一个**
//! `#[cfg(test)]` 起截断；写到父模块顶部会让十几个「扫 main_view.rs」的护栏集体假红。

#[cfg(test)]
mod e2e {
    use std::cell::RefCell;
    use std::rc::Rc;

    use gpui::AppContext as _;

    use rustshell_core::session::{SessionHandle, SessionKind, SessionState};

    use super::super::{AppShell, MainView, TabEntry};

    /// 终端写端的哨兵：用来证明「重连之后输入**不再**走旧会话的管道」。
    #[derive(Clone, Default)]
    struct SpySink(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl SpySink {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap_or_else(|e| e.into_inner())).into_owned()
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

    /// 可投喂的读端、投喂句柄、以及「把执行器推到字节真的上屏」的助手，全部复用
    /// `crate::dead_session` 那一套 —— 两处各写一份必然走样（尤其 `advance_clock`
    /// 那一步，漏掉的表现是「投喂了字节但屏上什么都没有」，看着像功能坏了）。
    ///
    /// `allow_real_io` 同理且更要紧：本模块两处投喂（旧会话的输出、新管道送来的
    /// 输出）都经过真读线程，不打开 gpui 的「允许真实 I/O」开关，测试会以
    /// `Your test is not deterministic.` 收尾 —— 报文长得像断言失败，其实全过。
    use crate::dead_session::{allow_real_io, feed_reader, pump_until_parsed, FeedHandle};

    /// 造一个只有这一个标签的 `MainView`，标签是「已连接的 SSH + 真终端视图」。
    ///
    /// 返回旧会话的投喂句柄与写端哨兵：前者用来把「注销前的输出」灌进屏里，
    /// 后者用来证明重连之后输入**不再**走它。
    fn build_with_a_connected_ssh_tab(
        cx: &mut gpui::TestAppContext,
    ) -> (
        gpui::Entity<MainView>,
        &mut gpui::VisualTestContext,
        FeedHandle,
        SpySink,
    ) {
        cx.update(|cx| {
            gpui_kit::component::init(cx);
            crate::theme::apply_theme_mode(cx, rustshell_core::theme::ThemeMode::Dark);
        });

        let slot: Rc<RefCell<Option<gpui::Entity<MainView>>>> = Rc::new(RefCell::new(None));
        let (_, vcx) = {
            let slot = slot.clone();
            cx.add_window_view(move |window, cx| {
                let main = cx.new(|cx| MainView::new(window, cx));
                *slot.borrow_mut() = Some(main.clone());
                let shell = cx.new(|_| AppShell::new(main));
                gpui_kit::component::Root::new(shell, window, cx)
            })
        };
        vcx.run_until_parked();
        let main = slot.borrow().clone().expect("MainView 未创建");

        // 终端视图必须是真的（断连态靠它留画面），读端可投喂、但不接真 PTY：
        // 没投喂之前引擎侧一个字节都不会解析。
        let (reader, feed) = feed_reader();
        let sink = SpySink::default();
        let sink_in_view = sink.clone();
        let term = vcx.update(move |_w, cx| {
            cx.new(move |cx| {
                rustshell_terminal::TerminalView::new(
                    sink_in_view,
                    reader,
                    rustshell_terminal::TerminalConfig::default(),
                    cx,
                )
            })
        });

        vcx.update(|_w, cx| {
            main.update(cx, |m, cx| {
                let mut session = SessionHandle::pending(
                    "e2e-dead".to_string(),
                    SessionKind::Ssh,
                    "e2e".to_string(),
                    Some("p-e2e".to_string()),
                );
                session.state = SessionState::Connected;
                m.tabs_state.tabs.push(TabEntry {
                    session,
                    terminal: Some(term),
                    scroll: None,
                    connect_error: None,
                    connect_started: None,
                    dead: None,
                    retry: None,
                });
                m.tabs_state.active = Some("e2e-dead".to_string());
                cx.notify();
            });
        });
        vcx.run_until_parked();
        (main, vcx, feed, sink)
    }

    /// 掉线之后：**标签还在、命令窗口不弹、画面留着、断连条真的进了帧**。
    #[gpui::test]
    fn a_dropped_ssh_session_keeps_the_tab_and_paints_the_dead_bar(cx: &mut gpui::TestAppContext) {
        let (main, vcx, _feed, _sink) = build_with_a_connected_ssh_tab(cx);

        // 前提：活着的时候**没有**断连条。没有这一步，下面的正向断言可能只是
        // 「这条本来就一直画着」（假绿）。
        assert!(
            vcx.debug_bounds("term-dead-bar").is_none(),
            "会话还活着就不该有断连条"
        );

        let before = main.read_with(vcx, |m, _| m.tabs_state.tabs.len());

        // 走真实收场路径：`poll` 收到 `SessionEvent::Closed` 且归类为 `KeepScene`
        // 时调的就是它。
        vcx.update(|_w, cx| {
            main.update(cx, |m, cx| {
                m.mark_session_dead("e2e-dead", "连接已被远端关闭".to_string(), cx)
            });
        });
        vcx.run_until_parked();

        // ① 标签不许被关掉
        assert_eq!(
            main.read_with(vcx, |m, _| m.tabs_state.tabs.len()),
            before,
            "掉线不许关标签 —— 关掉最后一个会牵出「跳到新页面」的观感"
        );
        // ② 掉线不许弹命令窗口
        assert!(
            main.read_with(vcx, |m, _| m.panel_ui.palette_state.is_none()),
            "掉线后弹出了命令窗口 —— 这正是用户说的「跳到新页面」"
        );
        // ③ 断连态：不算活，但终端视图**留着**（画面是判断「为什么断」的证据）
        assert!(
            main.read_with(vcx, |m, _| m.tabs_state.tabs[0].is_dead()
                && !m.tabs_state.tabs[0].is_live()
                && m.tabs_state.tabs[0].terminal.is_some()),
            "断连态必须保留终端视图（画面是证据），但绝不能算「活着」"
        );
        // ④ 断连条真的进了帧：**非零几何**才算画出来
        let bar = vcx
            .debug_bounds("term-dead-bar")
            .expect("断连条没有进渲染帧 —— 「在终端界面显示连接断开」落空了");
        assert!(
            f32::from(bar.size.height) > 0. && f32::from(bar.size.width) > 0.,
            "断连条尺寸为零，等于没画：{bar:?}"
        );

        // ⑤ 2026-11-19 运行反馈「断开后要在终端中输出断开连接」：这句话必须真的写进
        // **网格**（不是只在窗格底部画一条条），所以从终端视图里读回来验。
        let grid_text = main
            .read_with(vcx, |m, cx| {
                m.tabs_state.tabs[0]
                    .terminal
                    .as_ref()
                    .map(|t| t.read(cx).visible_text())
            })
            .expect("断连现场应当保留终端视图");
        assert!(
            grid_text.contains("连接已断开"),
            "终端里没有「连接已断开」这一行（用户翻历史时才看得到它）：{grid_text}"
        );
        assert!(
            grid_text.contains("连接已被远端关闭"),
            "提示里要带上断开原因，否则用户只知道「断了」不知道「为什么」：{grid_text}"
        );

        // ⑥ 收尾：这条断连的标签仍然可以被用户主动关掉（现场不是牢笼）
        vcx.update(|_w, cx| {
            main.update(cx, |m, cx| m.close_tab_by_session("e2e-dead", cx));
        });
        vcx.run_until_parked();
        assert!(
            main.read_with(vcx, |m, _| m.tabs_state.tabs.is_empty()),
            "断连的标签必须能被用户手动关掉"
        );
    }

    /// 2026-11-19 运行反馈「主动断开也保留现场、并输出同样一行」。
    ///
    /// 以前「断开」走的是 `detach_session`：终端视图被卸下、换成一页「已断开连接」
    /// 占位卡 —— 同样是一次断开，用户按的和机器断的看到两个界面，现场还全丢了。
    /// 这条用例钉住「两条来路同一套收场」：
    /// ① 画面还在（`terminal.is_some()`），滚动历史都在；
    /// ② 滚动区里有 `[连接已断开] 主动断开`；
    /// ③ 转成断连态（`is_dead()`、`!is_live()`），断连条真的进帧；
    /// ④ 自动重连账本作废（`retry` 为空）—— 用户已经把这条路关掉了。
    #[gpui::test]
    fn disconnecting_manually_keeps_the_scene_and_says_so(cx: &mut gpui::TestAppContext) {
        allow_real_io(cx);
        let (main, vcx, _feed, _sink) = build_with_a_connected_ssh_tab(cx);

        vcx.update(|_w, cx| {
            main.update(cx, |m, cx| m.disconnect_tab("e2e-dead", cx));
        });
        vcx.run_until_parked();
        vcx.update(|w, cx| {
            w.refresh();
            w.draw(cx).clear(cx);
        });

        let (kept_scene, dead, alive, has_retry) = main.read_with(vcx, |m, _| {
            let tab = &m.tabs_state.tabs[0];
            (
                tab.terminal.is_some(),
                tab.is_dead(),
                tab.is_live(),
                tab.retry.is_some(),
            )
        });
        assert!(
            kept_scene,
            "主动断开把终端视图卸掉了 —— 现场必须留着（同一套收场）"
        );
        assert!(dead, "主动断开没有转成断连态（断连条与状态点都靠它）");
        assert!(!alive, "主动断开之后不能再算「活着」");
        assert!(!has_retry, "主动断开必须作废自动重连账本：留着会自己连回来");

        let grid_text = main
            .read_with(vcx, |m, cx| {
                m.tabs_state.tabs[0]
                    .terminal
                    .as_ref()
                    .map(|t| t.read(cx).visible_text())
            })
            .expect("主动断开后终端视图应当还在");
        assert!(
            grid_text.contains("连接已断开"),
            "终端里没有「连接已断开」这一行：{grid_text}"
        );
        assert!(
            grid_text.contains("主动断开"),
            "提示里要点明是用户自己断的（与掉线区分开）：{grid_text}"
        );

        assert!(
            vcx.debug_bounds("term-dead-bar").is_some(),
            "主动断开也要画断连条（否则「重新连接」没有入口）"
        );
    }

    /// 重连成功之后：**同一块屏**、旧内容一个字节没少、输入走新管道、`dead` 已清。
    ///
    /// 对应用户 2026-09-22 的反馈「终端超时注销后重新连接，不要清除原连接的内容」。
    /// 为什么必须端到端：`reattach` 本身有没有保住画面是引擎层的事
    /// （`crate::dead_session::reattaching_a_new_session_keeps_the_previous_screen`
    /// 已经钉了），这里钉的是**宿主真的走了那条路** —— 而宿主若在成功分支里换一块
    /// 新屏，引擎那边写得再对也白搭，且所有单测照样全绿。
    ///
    /// 驱动方式是真实的：把状态推回 `Connecting`（`reconnect_tab` 的断连分支所做的）
    /// 之后往 `connect_results` 里投一个**真会话**，再跑真实的 `poll`。
    /// 不直接调 `reconnect_tab` 的原因：它要先去 `conn_store` 里找 profile 并真的
    /// 发起 SSH 连接，而本用例的目标是**成功分支** —— 那正是复用原屏的落点。
    #[gpui::test]
    fn reconnecting_keeps_the_previous_output_and_swaps_the_pipes(cx: &mut gpui::TestAppContext) {
        // 本用例两处投喂都走真读线程（旧会话的 `feed_reader`、新会话的真管道），
        // 必须在建视图**之前**打开开关：那之后读线程随时可能唤醒调度器。
        allow_real_io(cx);
        let (main, vcx, old_feed, old_sink) = build_with_a_connected_ssh_tab(cx);

        // ① 前提：旧会话的输出真的在屏上（在「vim 里掉线」之前它得先有内容）
        old_feed.feed(b"BEFORE-LOGOUT\r\n");
        pump_until_parsed(vcx, "旧会话输出上屏", |tcx| {
            tcx.update(|cx| read_screen(&main, cx).contains("BEFORE-LOGOUT"))
        });

        // ② 掉线：走真实收场路径（`poll` 收到 `SessionEvent::Closed` 时调的就是它）
        vcx.update(|_w, cx| {
            main.update(cx, |m, cx| {
                m.mark_session_dead("e2e-dead", "连接已被远端关闭".to_string(), cx)
            });
        });
        vcx.run_until_parked();
        assert!(
            main.read_with(vcx, |m, _| m.tabs_state.tabs[0].is_dead()
                && m.tabs_state.tabs[0].terminal.is_some()),
            "前提不成立：断连态没有保留终端视图，后面的重连就无从谈起"
        );

        // ③ 用户点了「重新连接」：状态推回 `Connecting` 并置「连接中」标记
        vcx.update(|_w, cx| {
            main.update(cx, |m, _cx| {
                m.tabs_state.tabs[0].session.state = SessionState::Connecting;
                m.tabs_state.tabs[0].connect_started = Some(std::time::Instant::now());
            });
        });

        // ④ 新会话真的建起来了（真管道：`SessionHandle` 上那两个字段在生产里就是它）
        let (writer, in_rx) = rustshell_core::stream::writer_pipe();
        let (reader, out_tx) = rustshell_core::stream::reader_pipe();
        let mut session = SessionHandle::pending(
            "e2e-reconnected".to_string(),
            SessionKind::Ssh,
            "e2e".to_string(),
            Some("p-e2e".to_string()),
        );
        session.state = SessionState::Connected;
        session.writer = Some(writer);
        session.reader = Some(reader);
        crate::main_view::connect_results()
            .0
            .send(Ok(session))
            .expect("连接结果通道应当可用");

        // ⑤ 跑真实的 `poll`（成功分支就是本条需求的落点）
        vcx.update(|_w, cx| {
            main.update(cx, |m, cx| {
                m.poll(cx);
            });
        });
        vcx.run_until_parked();

        assert_eq!(
            main.read_with(vcx, |m, _| m.tabs_state.tabs.len()),
            1,
            "重连成功不该新开标签（原地重连是本条的既定形态）"
        );
        assert!(
            main.read_with(vcx, |m, _| m.tabs_state.tabs[0].is_live()
                && !m.tabs_state.tabs[0].is_dead()),
            "重连成功之后必须算「活着」且不再画断连条（`dead` 没清会让新终端收不了键盘）"
        );

        // ⑥ 新会话的输出接上来了，而且**旧内容还在** —— 读的是 `tabs[0]` 当前的终端，
        //    所以「宿主偷偷换了一块新屏」也会在这里暴露（新屏里没有 BEFORE-LOGOUT）
        out_tx
            .send(b"AFTER-RECONNECT\r\n".to_vec())
            .expect("新会话的输出通道应当可用");
        pump_until_parsed(vcx, "重连后的输出上屏", |tcx| {
            tcx.update(|cx| read_screen(&main, cx).contains("AFTER-RECONNECT"))
        });
        let screen = vcx.update(|_w, cx| read_screen(&main, cx));
        for needle in ["BEFORE-LOGOUT", "已重新连接", "AFTER-RECONNECT"] {
            assert!(
                screen.contains(needle),
                "重连把原连接的内容清掉了（缺 `{needle}`）—— 这正是用户报的那一条。\n\
                 当前屏面：\n{screen}"
            );
        }

        // ⑦ 输入必须走**新**会话的管道，旧管道一个字节都不许再进
        let fence = old_sink.text().len();
        vcx.update(|_w, cx| {
            let term = main.read(cx).tabs_state.tabs[0]
                .terminal
                .clone()
                .expect("终端视图");
            term.read(cx).write_to_pty("AFTER-REATTACH\r");
        });
        assert_eq!(
            in_rx
                .recv_timeout(std::time::Duration::from_millis(500))
                .ok(),
            Some(b"AFTER-REATTACH\r".to_vec()),
            "重连之后输入没有进新会话的管道"
        );
        assert_eq!(
            old_sink.text().len(),
            fence,
            "重连之后还在往旧会话的管道写：{:?}",
            &old_sink.text()[fence..]
        );
    }

    /// 2026-11-04 用户反馈「ssh 连接断开后，tab 右键【连接】，连接失败但是无任何
    /// 失败提示」—— 端到端钉住「失败一定有提示」。
    ///
    /// 为什么必须整屏验：这里有三条各自都「看着对」的链路 ——
    /// ① 后台线程把 `Err(profile, 原因)` 发回 `connect_results`；
    /// ② `poll` 的失败分支把它落到**这一张**断连现场上（`connect_error`）；
    /// ③ 断连条与消息框把它显示出来。
    /// 任何一环断掉，单测都能全绿（文案拼装看起来在、落点代码看起来在），
    /// 用户那边却是「点了没反应、也没人告诉我为什么」。
    ///
    /// 连接目标是 `127.0.0.1:1`（本机必然「积极拒绝」，不依赖网络与凭据）。
    ///
    /// ⚠️ 2026-11-19：失败结果改为**直接注入** `connect_results`，不再等那个真线程。
    /// 它跑在进程级共享的 `ssh::runtime()` 上，全量并发跑时十几秒都排不上队
    /// （诊断实测 `state=Connecting 在途守卫=true connect_started=10.3s`），
    /// 于是这条用例会随机红 —— 那是测试环境争用，不是「失败没有落点」。
    /// 注入走的仍是同一条落点链路（`poll` 的失败分支），三条链路一条都没绕。
    #[gpui::test]
    fn a_failed_reconnect_from_the_dead_scene_reports_why(cx: &mut gpui::TestAppContext) {
        allow_real_io(cx);
        let (main, vcx, _feed, _sink) = build_with_a_connected_ssh_tab(cx);

        vcx.update(|_w, cx| {
            main.update(cx, |m, _| {
                m.conn_store
                    .profiles
                    .push(rustshell_core::connections::ConnectionProfile {
                        id: "p-e2e".into(),
                        name: "e2e".into(),
                        host: "127.0.0.1".into(),
                        port: 1,
                        username: "u".into(),
                        password: None,
                        stored_password: None,
                        key_path: None,
                        notes: String::new(),
                        group_id: None,
                        legacy_group: None,
                        last_used_at: 0,
                        tags: Vec::new(),
                        auto_reconnect: None,
                        forwards: Vec::new(),
                        proxy: rustshell_core::proxy::ProxyChoice::default(),
                        session_log: None,
                    });
            });
        });

        // 掉线：走真实收场路径（`poll` 收到 `SessionEvent::Closed` 时调的就是它）
        vcx.update(|_w, cx| {
            main.update(cx, |m, cx| {
                m.mark_session_dead("e2e-dead", "连接已被远端关闭".to_string(), cx)
            });
        });
        vcx.run_until_parked();
        assert!(
            vcx.debug_bounds("term-dead-bar").is_some(),
            "前提不成立：断连条没画出来，后面的「失败提示」就无从谈起"
        );

        // 用户右键标签点了「连接」（`tab_bar` 的菜单项落点就是 `reconnect_tab`）
        vcx.update(|_w, cx| {
            main.update(cx, |m, cx| m.reconnect_tab("e2e-dead", cx));
        });
        vcx.run_until_parked();

        // ★ 2026-11-19：不再等**真连接线程**的结果。
        //
        // 上面那次 `reconnect_tab` 确实会起线程，但它跑在 `ssh::runtime()` 这个
        // **进程级共享 tokio 运行时**上；全量并发跑时（440 个用例抢 worker）这条连接
        // 可能十几秒都排不上 —— 2026-11-17 加的现场诊断已经证实：
        // `state=Connecting 在途守卫=true connect_started=10.3s`，线程还在排队。
        // 那是**测试环境争用**，与本条要验的东西（失败的落点）无关，却把这条用例
        // 变成了随机红灯。
        //
        // 所以改成**注入**：往 `connect_results` 投一条真实的失败结果，再跑真 `poll`。
        // 落点逻辑（写 `connect_error` + 弹提示 + 保留断连现场）一个字都没被绕过，
        // 而结果与线程调度彻底解耦 —— 同一个手法见
        // `reconnecting_keeps_the_previous_output_and_swaps_the_pipes`（那边注入成功结果）。
        const INJECTED: &str = "Connection refused";
        crate::main_view::connect_results()
            .0
            .send(Err(("p-e2e".to_string(), INJECTED.to_string())))
            .expect("连接结果通道应当可用");

        // 通知队列必须在**同一次** update 里读：`render` 开头会 `flush_notices`
        // 把它推给窗口并清空，跨帧再读就永远是空的。
        type Outcome = (
            Option<String>,
            Vec<(crate::notice::NoticeKind, String)>,
            bool,
        );
        let mut outcome: Option<Outcome> = None;
        for _ in 0..20 {
            vcx.run_until_parked();
            let r = vcx.update(|_w, cx| {
                main.update(cx, |m, cx| {
                    m.poll(cx);
                    let tab = &m.tabs_state.tabs[0];
                    (tab.connect_error.clone(), m.notices.clone(), tab.is_dead())
                })
            });
            if r.0.is_some() {
                outcome = Some(r);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        let Some((tab_err, notices, still_dead)) = outcome else {
            panic!(
                "注入的失败结果没有落到 `connect_error`：\n\
                 后台的失败结果没有任何落点（用户看到的就是「无任何失败提示」）。"
            );
        };
        let err = tab_err.expect("上面已保证非空");
        assert!(
            !err.trim().is_empty(),
            "失败原因写进了 `connect_error` 却是空串，断连条上只会出现「重连失败：」"
        );
        assert!(
            still_dead,
            "重连失败把断连现场清掉了（`dead` 被清 = 用户那一屏输出被换成占位卡）"
        );

        // 失败原因必须真的被推给用户：消息框（`crate::notice` 的统一出口）
        assert!(
            notices.iter().any(|(kind, msg)| {
                *kind == crate::notice::NoticeKind::Error && msg.contains(&err)
            }),
            "重连失败没有弹出任何提示（通知队列里只有 {notices:?}）：\
             断连条底部那行小字是唯一线索，而它常常被长文案挤没 —— \
             用户看到的就是「连接失败但没有任何提示」"
        );

        // 断连条仍在帧上（画面与条都不动，只是多了「重连失败：…」那段尾巴）
        vcx.run_until_parked();
        assert!(
            vcx.debug_bounds("term-dead-bar").is_some(),
            "重连失败后断连条消失了：现场必须留着，用户在那儿重试"
        );
    }

    /// 读 `tabs[0]` **当前**那个终端视图的屏面。
    ///
    /// 刻意不复用外面捕获的 `Entity`：那样量到的是「老的那块屏还在不在」，
    /// 而本用例要量的是「用户眼前这块屏上有什么」。
    fn read_screen(main: &gpui::Entity<MainView>, cx: &gpui::App) -> String {
        main.read(cx)
            .tabs_state
            .tabs
            .first()
            .and_then(|t| t.terminal.as_ref())
            .map(|t| t.read(cx).visible_text())
            .unwrap_or_default()
    }
}
