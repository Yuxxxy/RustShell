//! 「会话掉线 → 终端原地转断连态」的三层护栏（仅测试构建编译）。
//!
//! # 被钉住的规矩
//!
//! 远端注销 / 空闲超时（服务端 `TMOUT`）/ 网络中断时：
//!
//! 1. **标签不关、命令窗口不弹、不显示「连接失败」占位卡** —— 用户看到的
//!    是他自己那一屏输出，不是一张陌生页面；
//! 2. 终端窗格底部叠一条断连条，写明「连接已断开」+ 原因 + 两个动作；
//! 3. **一个字节都不许再进 PTY**（键盘 / 粘贴 / `write_to_pty` 三条通路），
//!    光标停画、闪烁停表。
//! 4. **重新连上之后，原来那一屏内容一个字节都不许少**（2026-09-22 用户要求：
//!    「终端超时注销后重新连接，不要清除原连接的内容」）。复用**同一个**
//!    `TerminalView`，只把新会话的读/写端接上去 —— 换一块新屏等于把断连现场删掉。
//!
//! # 三层各挡什么
//!
//! | 层 | 用例 | 挡的回归 |
//! |----|------|----------|
//! | 契约（纯函数） | [`classify_session_end_*`] | 归类判错：SSH 掉线被当成「用户关标签」 |
//! | 行为（真终端视图） | [`engine_seals_every_input_path_once_the_session_is_dead`] | 断连后仍写入管道（光标还闪、粘贴还排队） |
//! | 行为（真终端视图） | [`reattaching_a_new_session_keeps_the_previous_screen`] 等四条 | 重连换了一块新屏 / 新 PTY 不知道网格尺寸 / 旧会话的终端模式被带过去 |
//! | 接线（扫源码） | [`dropping_a_session_keeps_the_tab_and_the_picture`] 等 | 有人把 `mark_session_dead` 改回 `close_tab_by_session`、把断连条挪出窗格、或在重连成功分支里换屏 |
//!
//! 行为那层为什么必须真建一个 `TerminalView`：这道闸的价值就在于「引擎自己拦，
//! 不依赖宿主记得判」。只断言源码里有 `if !self.alive` 是接线的活，测不出
//! 「闸装错了位置」（例如装在 `paste_from_clipboard` 而不是 `paste_text`，
//! 于是 AI 面板那条内部通路照样写）。
//!
//! 扫源码一律走 [`crate::test_support::prod`] —— 针就写在本文件里，不过滤
//! 测试区会自己命中自己（见 `test_support` 模块文档）。

use crate::test_support::{prod, region};

// ---------------------------------------------------------------- 契约层

/// SSH 掉线必须被归类为「留现场」。
///
/// 这一条判错的直接后果就是本次修的那个 bug：`close_tab_by_session` 被调用，
/// 标签消失，那一屏输出（连同断连原因）一起没了 —— 观感就是「跳到一张新页面」。
#[test]
fn classify_session_end_keeps_the_scene_for_a_dropped_ssh_session() {
    use rustshell_core::session::SessionKind;

    assert_eq!(
        crate::main_view::classify_session_end(
            rustshell_core::session::SessionState::Connected,
            SessionKind::Ssh,
        ),
        crate::main_view::SessionEnd::KeepScene,
        "SSH 会话在 Connected 状态下收到 Closed = 对端把连接关了，必须留现场"
    );
}

/// 用户主动关 / 主动断开时，输出泵收尾也会补发一个 `Closed`。
///
/// 那时 `state` 已被我们置成 `Closed`，必须**照旧关标签** —— 否则用户点了
/// 「关闭」却关不掉，标签会赖在标签栏上。
#[test]
fn classify_session_end_closes_the_tab_when_we_ended_it_ourselves() {
    use rustshell_core::session::{SessionKind, SessionState};

    for state in [SessionState::Closed, SessionState::Connecting] {
        for kind in [SessionKind::Ssh, SessionKind::Local] {
            assert_eq!(
                crate::main_view::classify_session_end(state.clone(), kind),
                crate::main_view::SessionEnd::CloseTab,
                "state={state:?} kind={kind:?}：不是「连着的时候被对端掐掉」，就该关标签"
            );
        }
    }
}

/// 本地 Shell 跑完（用户敲了 `exit`）关标签，是各家的通行预期。
#[test]
fn classify_session_end_closes_the_tab_when_the_local_shell_exits() {
    use rustshell_core::session::{SessionKind, SessionState};

    assert_eq!(
        crate::main_view::classify_session_end(SessionState::Connected, SessionKind::Local),
        crate::main_view::SessionEnd::CloseTab,
        "本地 shell 正常退出不是「掉线」，留一屏死终端反而莫名其妙"
    );
}

// ---------------------------------------------------------------- 行为层

/// 记录写进 PTY 的每一个字节。
///
/// `pub(crate)`：F-5 的粘贴闸行为测试（`crate::paste_wiring`）复用这套手法 ——
/// 「拦住了没有」只能靠真实写入的字节说话，两处各造一套抄不出第二条更可信的路。
#[derive(Clone, Default)]
pub(crate) struct SpySink(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl SpySink {
    pub(crate) fn text(&self) -> String {
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

/// 永不返回的 reader：测试全程不投喂字节，输出泵那条后台线程就一直等着。
///
/// 换成 `std::io::empty()` 会让它立刻 EOF 并发出 `Event::Exit`，把刚建好的
/// 视图卷进「进程已退出」那条支路 —— 测的就不是输入闸了。
///
/// ★ 「永不返回」必须**真的**做到，不能只靠用例作用域里那个 `Sender`：
/// 读线程一旦从 `recv` 返回并结束，它会 drop 自己的 flume 发送端，那次 drop
/// 会**从后台线程**唤醒 GPUI 的异步任务，撞上 `gpui::test` 的
/// 「调度器只能在测试线程上活动」检查。症状极具误导性：报文长得像断言失败，
/// 实际所有断言都过了，只是收尾时被判「测试不确定」而 FAILED。
/// 所以发送端钉在静态里永不释放，线程就一直阻塞在这儿。
/// 读端发送端的**保活池**：每调一次 [`blocking_feed_receiver`] 就往里放一个。
///
/// ★ 池子必须收下**每一个** `tx`，不能只留第一个。早先这里是 `OnceLock<Sender>`：
/// `set` 只在第一次成功，后续调用拿到的 `tx` 会被 `set` **归还并当场 drop**——
/// 而每个 receiver 都是**独立**通道，于是从第二次调用起 `recv` 立刻返回
/// `Err(Disconnected)`、`read` 交出 `Ok(0)`（EOF），读线程随即结束并 drop 自己的
/// flume 发送端，从后台线程唤醒 GPUI 任务 ⇒ 撞上「测试不确定」检查。
/// 症状与「只留一个 sender」的经典踩法一模一样（报文像断言失败，其实断言全过），
/// 只是触发条件更隐蔽：**同一个测试里建第二块屏**，或两个用例并行时谁先谁后。
static BLOCKING_FEED: std::sync::Mutex<Vec<std::sync::mpsc::Sender<Vec<u8>>>> =
    std::sync::Mutex::new(Vec::new());

pub(crate) fn blocking_feed_receiver() -> std::sync::mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = std::sync::mpsc::channel();
    BLOCKING_FEED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(tx);
    rx
}

pub(crate) struct BlockingReader(pub(crate) std::sync::mpsc::Receiver<Vec<u8>>);

impl std::io::Read for BlockingReader {
    fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
        // 发送端由 `BLOCKING_FEED` 持有，`recv` 会一直阻塞
        // （保活池收下每一个 `tx`，见那边的说明：只留一个不够）
        match self.0.recv() {
            Ok(_) | Err(_) => Ok(0),
        }
    }
}

/// 断连之后，**所有**输入通路都必须一个字节都进不了管道。
#[gpui::test]
fn engine_seals_every_input_path_once_the_session_is_dead(cx: &mut gpui::TestAppContext) {
    use gpui::AppContext as _;

    cx.update(gpui_component::init);

    let sink = SpySink::default();
    let sink_for_view = sink.clone();
    let view = cx.update(|cx| {
        cx.new(|cx| {
            rustshell_terminal::TerminalView::new(
                sink_for_view,
                BlockingReader(blocking_feed_receiver()),
                rustshell_terminal::TerminalConfig::default(),
                cx,
            )
        })
    });
    cx.run_until_parked();

    // ① 前提：活着的会话必须**真的**写得进去。没有这一步，下面的负向断言
    //    可能只是因为管道压根不通（例如视图压根没建起来），那是假绿。
    view.update(cx, |v, _cx| v.write_to_pty("ALIVE\r"));
    assert!(
        sink.text().contains("ALIVE\r"),
        "前提不成立：活着的会话都写不进 PTY，后续断言无意义。实际收到：{:?}",
        sink.text()
    );

    // ② 断连（宿主在收到 SessionEvent::Closed 时做的事）
    view.update(cx, |v, cx| v.set_alive(false, cx));
    assert!(
        !cx.update(|cx| view.read(cx).is_alive()),
        "set_alive(false) 之后 is_alive() 必须为 false"
    );

    // ③ 三条通路逐条试一遍，管道长度必须一个字节都不涨
    let fence = sink.text().len();
    view.update(cx, |v, _cx| v.write_to_pty("KEYBOARD_SHOULD_NOT_APPEAR\r"));
    view.update(cx, |v, cx| {
        v.paste_text("PASTE_SHOULD_NOT_APPEAR", false, cx)
    });
    view.update(cx, |v, cx| {
        v.paste_text("PASTE_AND_RUN_SHOULD_NOT_APPEAR", true, cx)
    });
    cx.run_until_parked();

    let after = sink.text();
    assert_eq!(
        after.len(),
        fence,
        "断连后仍有字节进入 PTY —— 用户敲键/粘贴看着有反应，实际排进一个永远不会返回的管道。\n\
         闸门前长度 {fence}，闸门后 {}，新增内容：{:?}",
        after.len() - fence,
        &after[fence..]
    );

    // ④ 恢复：重连成功后必须**重新可写**（闸门是双向的，不能一关到底）
    view.update(cx, |v, cx| v.set_alive(true, cx));
    view.update(cx, |v, _cx| v.write_to_pty("REVIVED\r"));
    assert!(
        sink.text().contains("REVIVED\r"),
        "set_alive(true) 之后必须恢复可写，否则「重新连接」成功后终端是个哑巴：{:?}",
        sink.text()
    );
}

// ---------------------------------------------- 行为层：重连复用原来那块屏

/// 可投喂的读端 + 投喂句柄。
///
/// 为什么另起一套而不是复用上面的 `BlockingReader`：那条用例要证明的是「断连后一个
/// 字节都进不去」，读端**永不返回**才对；这里要证明的恰好相反 —— 旧会话的输出必须
/// 真的上屏、新会话的输出必须接得上，所以读端要可控。
pub(crate) struct FeedReader {
    rx: std::sync::mpsc::Receiver<Vec<u8>>,
    /// 一块没喂完的剩余（`read_stdout_blocking` 每次只要 4096 字节，投喂可以更长）
    carry: Vec<u8>,
}

impl std::io::Read for FeedReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.carry.is_empty() {
            match self.rx.recv() {
                Ok(chunk) => self.carry = chunk,
                // 到不了：发送端钉在 `FEED_KEEPALIVE` 里，永不释放
                Err(_) => return Ok(0),
            }
        }
        let n = self.carry.len().min(out.len());
        out[..n].copy_from_slice(&self.carry[..n]);
        self.carry.drain(..n);
        Ok(n)
    }
}

/// 投喂句柄。
pub(crate) struct FeedHandle(std::sync::mpsc::Sender<Vec<u8>>);

impl FeedHandle {
    pub(crate) fn feed(&self, bytes: &[u8]) {
        let _ = self.0.send(bytes.to_vec());
    }
}

/// 所有投喂发送端的**保活池**。
///
/// 理由与 `BLOCKING_FEED` 完全相同：读线程一旦从 `recv` 返回并结束，它会 drop 自己的
/// flume 发送端，那次 drop 会从**后台线程**唤醒 GPUI 的异步任务，被 `gpui::test`
/// 判成「测试不确定」而 FAILED（报文长得像断言失败，其实断言全过）。
static FEED_KEEPALIVE: std::sync::Mutex<Vec<std::sync::mpsc::Sender<Vec<u8>>>> =
    std::sync::Mutex::new(Vec::new());

pub(crate) fn feed_reader() -> (FeedReader, FeedHandle) {
    let (tx, rx) = std::sync::mpsc::channel();
    FEED_KEEPALIVE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(tx.clone());
    (
        FeedReader {
            rx,
            carry: Vec::new(),
        },
        FeedHandle(tx),
    )
}

/// 打开 gpui 测试调度器的「允许真实 I/O」开关 —— 走**真线程**的用例必须先调它。
///
/// 本模块的重连用例是「真管道 + 真读线程」：`TerminalView` 的读端跑在
/// `std::thread::spawn` 里（`terminal/src/view.rs::read_stdout_blocking`），字节到达时
/// `flume` 会**从那一条线程**唤醒 GPUI 的合并任务。这在 `TestScheduler` 眼里正好是
/// 「测试不确定」的定义，于是收尾时报：
///
/// ```text
/// Detected activity on thread None ThreadId(N), but test scheduler is running on
/// Some("...") ThreadId(M). Your test is not deterministic.
/// ```
///
/// 报文极具误导性 —— 断言其实全过，栈顶指的是 `flume::Sender::send`，
/// 看着像「被测代码跨线程乱跑」，其实是**测试自己**引进了真线程。
///
/// gpui 为此留了官方开关：`TestScheduler::allow_parking()` 同时置位
/// `parking_allowed_once`，而 `assert_correct_thread()` 的第一行就是
/// `if state.parking_allowed_once { return; }` ⇒ 此后跨线程唤醒不再记账。
/// 它的原话语义就是「在一个其余部分确定性的测试里，混入真实的 I/O」
/// （见 `gpui_pre_scheduler::test_scheduler` 的 `Scheduler::block` 文档）。
///
/// 代价要说清：本用例里**真实的**时序不确定性也不再被抓。可接受 ——
/// 要证的是「重连没丢内容」，而内容本来就从真实线程来；真正需要确定性的部分
/// （网格有没有被清、模式位有没有复位、管道归属换没换）全在 `reattach` 的同步
/// 路径上，不依赖线程时序，断言本身仍然成立。
///
/// 别改成「先跑一遍看它红不红」：这条不是 flaky，是**必然**会红 ——
/// 只要读线程在任务已经 park 之后才把字节送进 flume。
pub(crate) fn allow_real_io(cx: &gpui::TestAppContext) {
    cx.executor().allow_parking();
}

/// 把执行器推到「投喂的字节真的解析完」。
///
/// 三件事缺一不可，少一件就会得到「看着像功能坏了」的假红：
///
/// 1. 读端是**真线程**：投喂之后它要过一会儿才把字节送进 flume ⇒ 必须轮询重试；
/// 2. 终端侧每解析完一轮会 `timer(16ms)` 睡一个帧窗口（输出节流），而
///    `TestAppContext::run_until_parked()` 只 `tick`、**不推虚拟时钟**
///    （`TestScheduler::run` 就是 `while step() {}`）⇒ 必须显式 `advance_clock`
///    把那 16ms 推过去，否则第二批字节永远排不上队；
/// 3. 推完时钟要再 `run_until_parked`，把因此变就绪的任务跑完。
///
/// 条件闭包收 `&mut TestAppContext`：它十有八九要 `cx.update(..)` 去读视图状态，
/// 而把 `cx` 直接捕获进闭包会和函数自己的 `&mut cx` 撞车。
pub(crate) fn pump_until_parsed(
    cx: &mut gpui::TestAppContext,
    what: &str,
    mut done: impl FnMut(&mut gpui::TestAppContext) -> bool,
) {
    for _ in 0..400 {
        cx.run_until_parked();
        if done(&mut *cx) {
            return;
        }
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(20));
        std::thread::sleep(std::time::Duration::from_millis(3));
    }
    panic!("等待「{what}」超时：投喂的字节始终没有上屏");
}

/// 重连（远端注销 / 空闲超时之后重新连上）**必须复用原来那块屏**。
///
/// 本条对应用户 2026-09-22 的反馈：「终端超时注销后重新连接，不要清除原连接的内容」。
/// 挡的回归是「重连成功时换一块新屏」—— 那样用户只看到一条新提示符，
/// 断在哪、最后几行报了什么，全没了。
#[gpui::test]
fn reattaching_a_new_session_keeps_the_previous_screen(cx: &mut gpui::TestAppContext) {
    use gpui::AppContext as _;

    // 读端是真线程 ⇒ 投喂必然跨线程唤醒调度器（见 [`allow_real_io`]）
    allow_real_io(cx);
    cx.update(gpui_component::init);

    let old_sink = SpySink::default();
    let (old_reader, old_feed) = feed_reader();
    let view = cx.update(|cx| {
        cx.new(|cx| {
            rustshell_terminal::TerminalView::new(
                old_sink.clone(),
                old_reader,
                rustshell_terminal::TerminalConfig::default(),
                cx,
            )
        })
    });
    cx.run_until_parked();

    // ① 前提：旧会话的输出**真的**上屏了。少这一步，下面的「还在」可能只是
    //    「本来就什么都没画」—— 那是假绿。
    old_feed.feed(b"OLD-SESSION-ONE\r\nOLD-SESSION-TWO\r\n");
    pump_until_parsed(cx, "旧会话输出上屏", |cx| {
        cx.update(|cx| view.read(cx).visible_text().contains("OLD-SESSION-ONE"))
    });

    // ② 按真实顺序走：先断连（宿主 `mark_session_dead` 里的 `set_alive(false)`），
    //    再重连（`poll` 的成功分支调 `reattach`）。
    view.update(cx, |v, cx| v.set_alive(false, cx));
    let new_sink = SpySink::default();
    let (new_reader, new_feed) = feed_reader();
    view.update(cx, |v, cx| {
        v.reattach(new_sink.clone(), new_reader, None, Some("[已重新连接]"), cx)
    });
    cx.run_until_parked();

    // ③ 新会话的输出落在**同一块**屏上，旧内容一个字节都没少，接缝也在
    new_feed.feed(b"NEW-SESSION-ONE\r\n");
    pump_until_parsed(cx, "新会话输出上屏", |cx| {
        cx.update(|cx| view.read(cx).visible_text().contains("NEW-SESSION-ONE"))
    });
    let screen = cx.update(|cx| view.read(cx).visible_text());
    for needle in [
        "OLD-SESSION-ONE",
        "OLD-SESSION-TWO",
        "[已重新连接]",
        "NEW-SESSION-ONE",
    ] {
        assert!(
            screen.contains(needle),
            "重连把屏上的东西弄丢了（缺 `{needle}`）—— 这正是本条需求要挡的回归。\n\
             当前屏面：\n{screen}"
        );
    }

    // ④ 写端也必须换成新会话的：旧管道里多一个字节都说明没换干净
    let fence = old_sink.text().len();
    view.update(cx, |v, _cx| v.write_to_pty("AFTER-REATTACH\r"));
    assert!(
        new_sink.text().contains("AFTER-REATTACH\r"),
        "重挂之后输入没有写进**新**会话（新管道收到：{:?}）",
        new_sink.text()
    );
    assert_eq!(
        old_sink.text().len(),
        fence,
        "重挂之后还在往旧管道写：{:?}",
        &old_sink.text()[fence..]
    );
}

/// 重挂必须把**当前网格尺寸**补推给新 PTY。
///
/// 渲染期那条 resize 回调只在「测出的网格 ≠ 当前网格」时才发
/// （`view.rs` 的 `if cols != current_cols || rows != current_rows`），而重挂时面板
/// 大小没变 ⇒ 网格没变 ⇒ 回调**一次都不会发**。不补这一推，新 PTY 会一直停在自己
/// 的初始尺寸（80×24），换行位置从此全错 —— 症状是「重连后输出比窗口窄一截」，
/// 而屏幕上一切看着都正常。
#[gpui::test]
fn reattach_pushes_the_current_grid_size_to_the_new_pty(cx: &mut gpui::TestAppContext) {
    use gpui::AppContext as _;

    // 读端是真线程 ⇒ 投喂必然跨线程唤醒调度器（见 [`allow_real_io`]）
    allow_real_io(cx);
    cx.update(gpui_component::init);

    let (reader, _feed) = feed_reader();
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

    // 前提：先把网格改成非默认尺寸。无头测试里终端从不布局，网格就停在
    // `TerminalConfig` 的 80×24 —— 不先改，下面的断言分不出「推的是当前尺寸」
    // 还是「推的正好是默认值」。
    view.update(cx, |v, _cx| v.resize(132, 43));
    assert_eq!(
        cx.update(|cx| view.read(cx).dimensions()),
        (132, 43),
        "前提不成立：网格没被改成 132×43，后面的断言无意义"
    );

    let pushed = std::sync::Arc::new(std::sync::Mutex::new(Vec::<(usize, usize)>::new()));
    let pushed_cb = pushed.clone();
    let (reader, _feed) = feed_reader();
    view.update(cx, |v, cx| {
        v.reattach(
            SpySink::default(),
            reader,
            Some(Box::new(move |cols, rows| {
                pushed_cb
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push((cols, rows));
            })),
            None,
            cx,
        )
    });

    assert_eq!(
        pushed.lock().unwrap_or_else(|e| e.into_inner()).as_slice(),
        &[(132, 43)],
        "重挂没有把当前网格尺寸推给新 PTY —— 面板尺寸没变时渲染期那条回调一次都不会发，\
         新 PTY 会一直以为自己还是 80×24"
    );
}

/// 在备用屏里掉线（vim / less / htop）之后再重连：要**退回主屏**。
///
/// 不退回的话，新 shell 的输出会画进那块「上层还留着 vim」的备用屏 ——
/// 用户看到的是一屏谁也不认识的内容，而这一切没有任何报错。
#[gpui::test]
fn reattaching_leaves_the_alternate_screen_behind(cx: &mut gpui::TestAppContext) {
    use gpui::AppContext as _;

    // 读端是真线程 ⇒ 投喂必然跨线程唤醒调度器（见 [`allow_real_io`]）
    allow_real_io(cx);
    cx.update(gpui_component::init);

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

    // 主屏写一行 → 进备用屏 → 备用屏再写一行（模拟「在 vim 里掉线」）
    feed.feed(b"PRIMARY-LINE\r\n\x1b[?1049hALT-SCREEN-LINE\r\n");
    pump_until_parsed(cx, "旧的备用屏内容上屏", |cx| {
        cx.update(|cx| view.read(cx).visible_text().contains("ALT-SCREEN-LINE"))
    });

    let (reader, _feed) = feed_reader();
    view.update(cx, |v, cx| {
        v.reattach(SpySink::default(), reader, None, None, cx)
    });
    cx.run_until_parked();

    let screen = cx.update(|cx| view.read(cx).visible_text());
    assert!(
        screen.contains("PRIMARY-LINE"),
        "没有退回主屏：备用屏一撤就什么都没了。\n当前屏面：\n{screen}"
    );
    assert!(
        !screen.contains("ALT-SCREEN-LINE"),
        "重连之后还留在备用屏上 —— 新 shell 的输出会画进那块「上层还留着 vim」的屏。\n\
         当前屏面：\n{screen}"
    );
}

/// 旧会话开过的输入上报模式（这里以「粘贴包裹」`?2004` 为代表）不许带到新会话。
///
/// 这些位住在 `Term` 里，新会话对它一无所知：`?2004` 留着，粘贴多行命令就会被
/// 新 shell 当成「一次性粘贴」；鼠标上报留着，点一下就往新 shell 里灌一串转义序列。
#[gpui::test]
fn reattaching_drops_the_old_sessions_paste_wrapping(cx: &mut gpui::TestAppContext) {
    use gpui::AppContext as _;

    // 读端是真线程 ⇒ 投喂必然跨线程唤醒调度器（见 [`allow_real_io`]）
    allow_real_io(cx);
    cx.update(gpui_component::init);

    let old_sink = SpySink::default();
    let (reader, feed) = feed_reader();
    let view = cx.update(|cx| {
        cx.new(|cx| {
            rustshell_terminal::TerminalView::new(
                old_sink.clone(),
                reader,
                rustshell_terminal::TerminalConfig::default(),
                cx,
            )
        })
    });

    // `?2004h` 与标记行走同一批字节 ⇒ 看到 ARMED 就说明模式已经开好了
    feed.feed(b"\x1b[?2004h\x1b[2mARMED\x1b[0m\r\n");
    pump_until_parsed(cx, "旧会话的粘贴包裹模式生效", |cx| {
        cx.update(|cx| view.read(cx).visible_text().contains("ARMED"))
    });

    // 前提：此刻粘贴**确实是**包裹式。没有这一步，下面的负向断言可能只是因为
    // 这个模式压根没开过（假绿）。
    view.update(cx, |v, cx| v.paste_text("PASTE-OLD", false, cx));
    assert!(
        old_sink.text().contains("\x1b[200~PASTE-OLD\x1b[201~"),
        "前提不成立：旧会话的粘贴没有被包裹，模式压根没生效。实际发往旧管道：{:?}",
        old_sink.text()
    );

    let new_sink = SpySink::default();
    let (reader, _feed) = feed_reader();
    view.update(cx, |v, cx| {
        v.reattach(new_sink.clone(), reader, None, None, cx)
    });
    cx.run_until_parked();

    view.update(cx, |v, cx| v.paste_text("PASTE-NEW", false, cx));
    let to_new = new_sink.text();
    assert!(
        to_new.contains("PASTE-NEW"),
        "重挂之后粘贴没有送到新会话：{to_new:?}"
    );
    assert!(
        !to_new.contains("\x1b[200~"),
        "旧会话的粘贴包裹模式被带到了新会话 —— 新 shell 并没有要求过它，\
         而它会让多行粘贴被当成一次性粘贴"
    );
}

// ---------------------------------------------------------------- 接线层

/// 掉线处理必须**留现场**，绝不许再关标签。
#[test]
fn dropping_a_session_keeps_the_tab_and_the_picture() {
    let main = prod(include_str!("main_view.rs"));

    let landing = region(
        &main,
        "for (id, pid, reason) in dropped",
        "let elapsed = poll_start",
    );
    assert!(
        landing.contains("mark_session_dead"),
        "掉线落点必须先转断连态（保留终端画面 + 封输入）"
    );
    assert!(
        !landing.contains("close_tab_by_session"),
        "掉线落点里出现了 close_tab_by_session —— 标签一关，那一屏输出（连同断连\n\
         原因）就没了，观感就是用户说的「跳到新页面」。留现场是本条的**全部**意义。"
    );

    // 断连态的三个开关都在 `TabEntry` 上，缺一个就会漏渲染
    //
    // ★ 2026-10-12（开源评审 P0-4 刀二）：`TabEntry` 已搬进
    // `main_view/tab_model.rs`，断言源随之改指。护栏的源不跟着搬移走，
    // 就会退化成「断言一个已经不该有这段代码的文件里没有它」——恒真、形同虚设。
    let tab_model = prod(include_str!("main_view/tab_model.rs"));
    for needle in ["pub fn is_live", "pub fn is_dead", "pub fn is_reconnecting"] {
        assert!(
            tab_model.contains(needle),
            "`TabEntry` 少了 `{needle}`：状态判定会被各调用点各写一套，\
             症状是「断了但状态点是绿的」这类只在实机才看得见的错"
        );
    }

    // 重连成功必须把 `dead` 清掉，否则新终端依旧收不了键盘
    let success = region(&main, "match slot {", "pending_focus = true;");
    assert!(
        success.contains("dead = None"),
        "原地重连成功后没有清 `dead` —— 新会话会被当成断连态，输入一直是死的"
    );
}

/// 断连条必须在**终端窗格内**，并且挡住鼠标。
#[test]
fn the_dead_bar_lives_inside_the_terminal_pane_and_blocks_the_mouse() {
    let split = prod(include_str!("main_view/split_actions.rs"));

    // 终点取**函数定义**而不是调用点：`Self::render_dead_bar` 第一次出现
    // 正是那行调用本身，切在那里会把要断言的调用排除在切片之外（踩过一次）。
    let pane = region(&split, "fn render_term_pane", "fn render_dead_bar");
    assert!(
        pane.contains(".child(Self::render_dead_bar(session_id, plan, cx))"),
        "断连条没有挂进终端窗格 —— 「在终端界面显示连接断开」就落不了地"
    );
    // 占位卡与断连条互斥：断连态一定有终端画面，走了占位卡就说明画面被换掉了
    assert!(
        pane.contains("if tab.terminal.is_none()") && pane.contains("tab.dead.as_ref().map("),
        "断连条与占位卡的选取条件被改乱了（断连态绝不能走「连接失败」占位卡）"
    );

    let bar = region(&split, "fn render_dead_bar", "fn render_pane_header");
    for needle in [
        // 钉住四个口径：挡住鼠标、说明断连、给出两个动作
        ".occlude()",
        "t(\"连接已断开\")",
        "t(\"重新连接\")",
        "t(\"关闭标签\")",
    ] {
        assert!(
            bar.contains(needle),
            "断连条里少了 `{needle}`。\
             `.occlude()` 尤其不能省：gpui 的 hit_test 会收下光标下的全部 hitbox，\
             不挡的话点「重新连接」会同时把一次鼠标事件送给终端。"
        );
    }
    assert!(
        bar.contains("NoticeKind::Error"),
        "断连条必须走 `crate::notice` 的官方 `Alert`（`no_panel_hand_rolls_a_warning_icon` \
         钉着「警告/错误提示不得自绘图标」），不要回到「三角图标 + 手拼取色」。"
    );
    assert!(
        !bar.contains("Icon::new("),
        "断连条里出现了自绘图标 —— 警告/错误图标归官方 `Alert` 所有，走 `NoticeKind`。"
    );
}

/// 断连态在**标签栏**必须显示成「未连接」，且不能是绿点。
#[test]
fn a_dead_tab_looks_dead_in_the_tab_bar() {
    let tab_bar = prod(include_str!("tab_bar.rs"));

    assert!(
        tab_bar.contains("TabStatus::of(tab.is_live(), tab.is_connecting(), tab.is_dead())"),
        "标签栏状态点没有按 `is_live()` 判定：断连态刻意保留终端视图，\
         用 `terminal.is_some()` 会把一个已死的会话画成健康的绿点"
    );
    assert!(
        tab_bar.contains("Self::Dead => (hex(t.danger), false)"),
        "断连态在标签栏应当是一个空心红点（`Dead` 变体）"
    );
    assert!(
        tab_bar.contains("connected: tab.is_live()"),
        "Tab 右键菜单的「连接 / 断开」二选一必须问 `is_live()`，\
         否则断连的标签给出的动作是「断开」（点了没反应）而不是「重新连接」"
    );
}

/// 快捷命令的目标清单里不许出现断连的会话。
#[test]
fn a_dead_tab_is_not_a_quick_command_target() {
    let quick = prod(include_str!("main_view/quickcmd_actions.rs"));

    assert!(
        quick.contains("t.is_live()"),
        "快捷命令的「可投递」判定必须收口到 `is_live()`：\
         断连态保留终端视图，放过去的话目标清单会把死会话列为可用，\
         投递被引擎静默丢弃 —— 用户看到的是「勾了、点了、什么都没发生」"
    );
}

/// 重连成功必须**复用**原来那块屏 —— 不许在成功分支里换一块新的。
///
/// 行为层（[`reattaching_a_new_session_keeps_the_previous_screen`]）证明引擎重挂能留住
/// 画面；这一条证明**宿主真的走了那条路**。两者缺一不可：宿主若在成功分支里
/// `mount_terminal` 一块新屏，引擎那边写得再对也没人调用它，而所有单测照样全绿。
#[test]
fn reconnecting_reuses_the_screen_it_left_behind() {
    let main = prod(include_str!("main_view.rs"));
    // 起点必须取 `reuse_ix` 那一行：它就是「该槽位是否已经有一块屏」的判据本身，
    // 从 `match slot {` 起头会把它切在片外，下面那条断言便永远找不到
    // `terminal.is_some()`（切片里根本没有这一行）。
    let success = region(
        &main,
        "let reuse_ix = slot.filter(",
        "pending_focus = true;",
    );

    assert_eq!(
        success.matches("reattach_terminal_view(").count(),
        1,
        "重连成功分支里没有调用「重挂到原来那块屏」的路径（或调了不止一处）：\
         用户要求「重新连接不要清除原连接的内容」，落点就是这一处"
    );
    assert!(
        success.contains("terminal.is_some()"),
        "复用与否没有按「该槽位是否已经有一块屏」判定 —— 断连现场会被换成一块空屏"
    );

    // 复用分支里不许再建/再换终端视图
    let reuse = region(success, "if reuse_ix.is_some()", "} else {");
    assert!(
        !reuse.contains("mount_terminal"),
        "复用分支里又建了一块新屏：原连接的内容会被清掉，正是本条要挡的回归"
    );
    assert!(
        !reuse.contains("terminal = Some("),
        "复用分支里替换了终端视图 —— 只有**同一个 Entity** 才谈得上保留网格与回滚缓冲"
    );

    // 接缝：用户得看得出「这里重连过」，否则新旧会话的输出连成一片、分不清哪句是谁说的
    assert!(
        success.contains("已重新连接"),
        "重连之后屏上没有任何接缝标记"
    );
}
