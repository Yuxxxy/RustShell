//! 回归：本地 PTY 尺寸变更不得阻塞调用线程。

use rustshell_core::session::{spawn_local_session, ResizeSink, SessionHandle};
use std::io::Read;
use std::time::{Duration, Instant};

/// 后台持续排空 reader，精确模拟 rustshell-terminal 的`read_stdout_blocking`线程，
fn drain_in_background(s: &mut SessionHandle) {
    if let Some(mut r) = s.reader.take() {
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match r.read(&mut buf) {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        });
    }
}

fn local_resizer(s: &SessionHandle) -> std::sync::Arc<rustshell_core::session::PtyResizer> {
    match &s.resize {
        ResizeSink::Local(r) => r.clone(),
        ResizeSink::Ssh(_) => panic!("本地会话不应使用 Ssh 尺寸通道"),
    }
}

/// UI 线程可见的调用耗时应是「入队」量级（微秒~毫秒），
const CALL_BUDGET: Duration = Duration::from_millis(100);

/// 尺寸必须真正生效（异步执行，但不可丢失）
#[test]
fn resize_applies_to_pty_without_blocking_caller() {
    let mut s = spawn_local_session(80, 24, None).expect("spawn local session");
    drain_in_background(&mut s);
    std::thread::sleep(Duration::from_millis(600));
    let resizer = local_resizer(&s);

    let t0 = Instant::now();
    s.resize.resize(100, 30);
    let call_cost = t0.elapsed();
    assert!(
        call_cost < CALL_BUDGET,
        "resize 入队耗时 {call_cost:?} 超过 {CALL_BUDGET:?} —— UI 线程会被拖住"
    );

    assert!(
        resizer.wait_idle(Duration::from_secs(5)),
        "resize 请求未在 5s 内执行完（后台线程可能被 conhost 卡住或丢失）"
    );
    assert_eq!(
        resizer.pty_size(),
        Some((100, 30)),
        "resize 未真正应用到 PTY"
    );
}

/// 连续拖拽窗口：入队必须始终瞬时；且最终生效的应是最后一次尺寸。
#[test]
fn burst_resize_coalesces_to_latest() {
    let mut s = spawn_local_session(80, 24, None).expect("spawn local session");
    drain_in_background(&mut s);
    std::thread::sleep(Duration::from_millis(400));
    let resizer = local_resizer(&s);

    let t0 = Instant::now();
    for i in 0..40u16 {
        s.resize.resize(90 + i, 28 + i);
    }
    let total = t0.elapsed();
    assert!(
        total < CALL_BUDGET,
        "40 次 resize 入队共耗时 {total:?}，超过 {CALL_BUDGET:?}"
    );

    assert!(resizer.wait_idle(Duration::from_secs(5)), "尺寸队列未排空");
    assert_eq!(
        resizer.pty_size(),
        Some((129, 67)),
        "合并后应生效最后一次尺寸（90+39, 28+39）"
    );
}

/// 最危险的时刻：子进程仍停在启动阻塞态（DSR 未应答）。
#[test]
fn resize_while_child_is_starting_does_not_block_caller() {
    let mut s = spawn_local_session(80, 24, None).expect("spawn local session");
    drain_in_background(&mut s);
    // 不应答 DSR 到 PowerShell 停在启动阻塞态
    std::thread::sleep(Duration::from_millis(800));
    let resizer = local_resizer(&s);

    let t0 = Instant::now();
    s.resize.resize(120, 40);
    let call_cost = t0.elapsed();
    assert!(
        call_cost < CALL_BUDGET,
        "子进程启动阶段 resize 入队耗时 {call_cost:?} —— UI 线程会被拖住"
    );
    assert!(resizer.wait_idle(Duration::from_secs(5)), "尺寸队列未排空");
}
