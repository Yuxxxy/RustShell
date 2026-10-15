//! 回归：会话释放（`SessionHandle::drop`）必须在毫秒级返回，绝不阻塞调用线程。

use rustshell_core::session::{spawn_local_session, SessionHandle};
use std::io::Write as _;
use std::time::{Duration, Instant};

/// 阈值：UI 线程上超过该时长即会被 Windows 判定为「未响应」。
const DROP_BUDGET: Duration = Duration::from_secs(3);

/// 启动本地会话；`answer_dsr`为真时替 PowerShell 应答光标位置查询
fn spawn_and_settle(answer_dsr: bool) -> SessionHandle {
    let mut s = spawn_local_session(80, 24, None).expect("spawn local session");
    std::thread::sleep(Duration::from_millis(1500));
    if answer_dsr {
        if let Some(w) = s.writer.as_mut() {
            let _ = w.write_all(b"\x1b[1;1R");
            let _ = w.flush();
        }
        std::thread::sleep(Duration::from_millis(800));
    }
    s
}

fn measure_drop(tag: &str, session: SessionHandle) {
    let t0 = Instant::now();
    drop(session);
    let dt = t0.elapsed();
    println!("[teardown] {tag}: drop = {dt:?}");
    assert!(
        dt < DROP_BUDGET,
        "{tag}: SessionHandle::drop 阻塞 {dt:?}（>{DROP_BUDGET:?}）\
         —— 在 UI 线程上会表现为窗口「未响应」"
    );
}

/// 场景 A：空闲会话（终端已出提示符）。
#[test]
fn drop_idle_session_is_prompt() {
    measure_drop("A 空闲", spawn_and_settle(true));
}

/// 场景 B：刚产生大量输出（刷屏）后立刻关闭。
#[test]
fn drop_busy_session_is_prompt() {
    let mut s = spawn_and_settle(true);
    if let Some(w) = s.writer.as_mut() {
        let _ = w.write_all(b"1..200000 | ForEach-Object { \"line $_\" }\r\n");
        let _ = w.flush();
    }
    std::thread::sleep(Duration::from_millis(1200));
    measure_drop("B 高输出", s);
}

/// 场景 C（关键回归）：子进程仍在启动阶段（DSR 未应答）时关闭会话。
#[test]
fn drop_session_while_shell_is_booting_is_prompt() {
    measure_drop("C 子进程启动中", spawn_and_settle(false));
}

/// 连续开关不累积阻塞（历史缺陷：复测期间 PowerShell 进程表堆积）。
#[test]
fn repeated_open_close_stays_prompt() {
    for i in 0..3 {
        let t0 = Instant::now();
        drop(spawn_and_settle(true));
        let dt = t0.elapsed();
        println!("[teardown] 第 {i} 轮（含 2.3s 启动等待）耗时 {dt:?}");
    }
}
