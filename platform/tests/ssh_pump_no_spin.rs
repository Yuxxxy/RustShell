//! 回归：远端 shell 退出（信道关闭）之后，SSH 输出泵必须停下来 —— 不能变成
//! 一个只烧 CPU 的紧循环。
//!
//! # 为什么判据是「进程 CPU」而不是「调度是否停摆」或「任务计数」
//!
//! 2026-09-20 定位到的根因：`read_half.wait()` 在信道关闭后每次都立刻返回
//! `None`（底层是 `tokio::sync::mpsc::Receiver`），旧代码写成 `_ => {}` ⇒ 泵
//! 任务变成一个**没有真实 await 点**的紧循环，把 tokio worker 钉在 100%
//! （用户侧表现：「只开着终端和 AI 面板，CPU 却单核 70%+」）。
//!
//! 试过并**否掉**的两个判据（都实测抓不到注入回去的缺陷）：
//!
//! 1. **任务计数**（`runtime().metrics().num_alive_tasks()`）：会话一 drop，
//!    `session.rs::build_session_pipes` 里那个 std 桥线程（`rs-out-<id>`，每
//!    500ms 查一次 exit 旗标）就会退出并顺手 drop `out_rx`，泵可能在一条残余
//!    数据上撞到 `out_tx.send` 失败而侥幸 break。
//! 2. **调度探针**（在运行时里挂个每 5ms 醒一次的 tick 任务，看它会不会挨饿）：
//!    **tokio 有协作式调度额度（coop budget，每任务每轮 128 次）** ——
//!    `mpsc::recv()` 在额度耗尽时会返回 `Poll::Pending` 并唤醒任务，于是那个
//!    「紧」循环其实会周期性 yield，同 worker 上的其他任务照样能跑。实测两条
//!    worker 都被空转泵占住时，探针 tick 速率**完全正常**。
//!
//! 所以唯一诚实的判据是**CPU 本身**：本测试**不 drop 会话**（桥线程与 `out_rx`
//! 都还活着，泵不可能靠 `send` 失败逃掉），改让远端 shell 自己 `exit`，然后量
//! 进程 CPU 的单核占用率。修复后 ≈0；`PumpStep::Stop => break` 被写坏时，两个泵
//! 会把两条 worker 烧到 ~200%。
//!
//! 与 `ssh::tests::pump_step` 的分工：那个单测钉**纯函数契约**（`None` 必须判成
//! `Stop`），本测试钉**运行时后果**。缺一不可：把 `PumpStep::Stop => break` 改成
//! `{}`，纯函数单测照样绿。

use rustshell_core::connections::ConnectionProfile;
use rustshell_core::session::SessionHandle;
use rustshell_core::ssh::{connect_in_background, trust_requests, ConnectResult, SshChannels};
use std::io::Write;
use std::time::{Duration, Instant};
use sysinfo::{Pid, ProcessesToUpdate, System};

fn docker_enabled() -> bool {
    std::env::var("RUSTSHELL_DOCKER_TEST").is_ok()
}

fn profile(name: &str) -> ConnectionProfile {
    let mut p = ConnectionProfile::new(name, "127.0.0.1", "testuser");
    p.port = 2222;
    p.password = Some("test1234".into());
    p
}

/// 连一个 SSH 会话（自动回应首次信任弹窗）。
fn open_session(name: &str) -> SessionHandle {
    let (tx, rx) = flume::unbounded();
    let handle = connect_in_background(
        profile(name),
        Duration::from_secs(20),
        None,
        1,
        None,
        SshChannels::Full,
        tx,
    );
    if let Ok(prompt) = trust_requests().1.recv_timeout(Duration::from_secs(30)) {
        assert!(!prompt.fingerprint.is_empty(), "指纹不能为空");
        let _: Result<(), _> = prompt.reply.send(true);
    }
    let res: ConnectResult = rx.recv_timeout(Duration::from_secs(30)).expect("连接超时");
    handle.join().expect("连接线程 panic");
    res.expect("连接应成功")
}

/// 本进程当前累计的 CPU 时间（毫秒，跨所有线程）。
fn process_cpu_ms(sys: &mut System, pid: Pid) -> u64 {
    sys.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
    sys.process(pid)
        .map(|p| p.accumulated_cpu_time())
        .unwrap_or(0)
}

/// 在给定窗口内，本进程平均占用了多少个核（1.0 = 一个核跑满）。
fn cpu_cores_used(window: Duration) -> f64 {
    let mut sys = System::new();
    let pid = Pid::from_u32(std::process::id());
    let t0 = Instant::now();
    let c0 = process_cpu_ms(&mut sys, pid);
    std::thread::sleep(window);
    let c1 = process_cpu_ms(&mut sys, pid);
    let elapsed_ms = t0.elapsed().as_millis().max(1) as f64;
    c1.saturating_sub(c0) as f64 / elapsed_ms
}

#[test]
fn remote_shell_exit_does_not_leave_the_output_pump_burning_cpu() {
    if !docker_enabled() {
        eprintln!("跳过：未设置 RUSTSHELL_DOCKER_TEST=1");
        return;
    }
    // 每次测试用全新 known_hosts，保证会走一次信任弹窗（我们自动应答）。
    let kh = std::env::temp_dir().join(format!("rustshell-kh-spin-{}", std::process::id()));
    let _ = std::fs::remove_file(&kh);
    std::env::set_var("RUSTSHELL_KNOWN_HOSTS", &kh);

    // 两个会话：`ssh::runtime()` 只配了 `worker_threads(2)`，两个泵同时空转就能
    // 把两条 worker 全烧满 —— 正是用户报的那个状态。
    let mut a = open_session("spin-a");
    let mut b = open_session("spin-b");
    let mut wa = a.writer.take().expect("会话应带输入端");
    let mut wb = b.writer.take().expect("会话应带输入端");

    // 先让两个 shell 起稳，再量基线占用。
    std::thread::sleep(Duration::from_secs(1));
    let base = cpu_cores_used(Duration::from_secs(2));
    assert!(
        base < 1.0,
        "基线 CPU 已达 {base:.2} 核 —— 会话刚建好就已经在烧 CPU，本测试失去意义"
    );

    // 让两个远端 shell 退出。注意**不 drop 会话**：桥线程与 `out_rx` 都还活着，
    // 所以泵不可能靠 `out_tx.send` 失败逃掉，只能正确识别「信道已关闭」。
    wa.write_all(b"exit\r\n").expect("写入 a 的 exit");
    wa.flush().expect("flush a");
    wb.write_all(b"exit\r\n").expect("写入 b 的 exit");
    wb.flush().expect("flush b");

    // 给远端结束会话、russh 关信道留出时间；此后若泵还在，它就是纯空转。
    std::thread::sleep(Duration::from_millis(1500));
    let after = cpu_cores_used(Duration::from_secs(3));

    assert!(
        after < 0.5,
        "远端 shell 已退出，但进程仍在烧 {after:.2} 个核（基线 {base:.2}）——\
         说明输出泵在 `wait()` 返回 `None` 之后没有 break，成了没有真实 await 点的\
         紧循环，把 tokio worker 钉在 100%。用户侧表现就是「只开着终端和 AI 面板\
         却 CPU 很高」。注意 tokio 的协作式调度会让它周期性 yield，所以它**不会**\
         饿死同 worker 的其他任务 —— 只有 CPU 占用能把这个缺陷照出来。"
    );
}
