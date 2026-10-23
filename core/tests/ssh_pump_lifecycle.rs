//! 回归：反复开关 SSH 会话时，运行时里**不得累积**常驻任务。
//!
//! 2026-09-20 定位到的根因（用户反馈「只开一个终端 + AI 面板，CPU 却很高」）：
//! `russh` 的 `ChannelReadHalf::wait()` 在信道关闭后**每次调用都立刻返回
//! `None`**（底层是 `tokio::sync::mpsc::Receiver`，发送端析构后 `recv()` 立即
//! 返回 `None`）。旧代码把这种消息写成了 `_ => {}`，于是 `loop` 里再没有任何
//! await 点 —— 泵任务变成一个没有挂起的紧循环，把 tokio worker 钉在 100%。
//!
//! # 分工（三个层次，缺一不可）
//!
//! 1. `ssh::tests::pump_step` —— **纯函数契约**：`None` 必须判成 `Stop`。
//! 2. 本文件 —— **任务不泄漏**：反复开关后 `ssh::runtime()` 里的常驻任务
//!    必须回到基线。注意这一层的判据是「任务计数」，它有一条**时序旁路**：
//!    会话一 drop，`session.rs::build_session_pipes` 里的 std 桥线程
//!    （`rs-out-<id>`，每 500ms 查一次 exit 旗标）就会退出并顺手 drop `out_rx`，
//!    泵可能在一条残余数据上撞到 `out_tx.send` 失败而侥幸 break。所以它只能
//!    抓「泄漏」，抓不住「空转」。
//! 3. `platform/tests/ssh_pump_no_spin.rs` —— **运行时后果**：不 drop 会话、
//!    改让远端 shell 自己退出，用调度探针量 worker 有没有被钉死。那一条才是
//!    「泵真的在烧 CPU」的判据（把 `PumpStep::Stop => break` 注回成 `{}` 时，
//!    只有它能稳定报红）。

use rustshell_core::connections::ConnectionProfile;
use rustshell_core::session::SessionHandle;
use rustshell_core::ssh::{
    connect_in_background, runtime, trust_requests, ConnectResult, SshChannels,
};
use std::time::{Duration, Instant};

fn docker_enabled() -> bool {
    std::env::var("RUSTSHELL_DOCKER_TEST").is_ok()
}

/// 本文件里的用例共享两样**进程级**全局状态：`RUSTSHELL_KNOWN_HOSTS` 环境变量
/// 与 `ssh::trust_requests()` 的信任通道。cargo 默认并行跑同一个二进制里的用例，
/// 那会让「A 把 known_hosts 换掉、B 正等着信任弹窗」互相打架，所以这里显式串行化。
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial_guard() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

fn profile() -> ConnectionProfile {
    let mut p = ConnectionProfile::new("pump-lifecycle", "127.0.0.1", "testuser");
    p.port = 2222;
    p.password = Some("test1234".into());
    p
}

/// 连一个 SSH 会话（自动回应首次信任弹窗）。known_hosts 用调用方给的全新路径。
fn open_session() -> SessionHandle {
    let (tx, rx) = flume::unbounded();
    // 第三个参数是会话日志落点（F-7）：这些用例只关心泵的收场，不记录
    let handle = connect_in_background(
        profile(),
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

fn alive_tasks() -> usize {
    runtime().metrics().num_alive_tasks()
}

fn wait_until(mut cond: impl FnMut() -> bool, budget: Duration) -> bool {
    let t0 = Instant::now();
    while t0.elapsed() < budget {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    cond()
}

fn with_fresh_known_hosts(tag: &str) -> std::path::PathBuf {
    let kh = std::env::temp_dir().join(format!("rustshell-kh-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_file(&kh);
    std::env::set_var("RUSTSHELL_KNOWN_HOSTS", &kh);
    kh
}

/// 反复开关：任务数不得单调累积（历史缺陷：关一次泄漏一个永不退出的泵）。
#[test]
fn repeated_open_close_does_not_leak_pump_tasks() {
    if !docker_enabled() {
        eprintln!("跳过：未设置 RUSTSHELL_DOCKER_TEST=1");
        return;
    }
    let _serial = serial_guard();
    with_fresh_known_hosts("pump-leak");

    let baseline = alive_tasks();
    let mut live_after_each = Vec::new();
    for round in 0..3 {
        let session = open_session();
        assert!(
            wait_until(|| alive_tasks() > baseline, Duration::from_secs(10)),
            "第 {round} 轮：会话建立后任务数没增长（baseline={baseline}）"
        );
        drop(session);
        assert!(
            wait_until(|| alive_tasks() <= baseline, Duration::from_secs(10)),
            "第 {round} 轮：关闭后任务数未回落（baseline={baseline}，实测 {}）\
             —— 每开一次会话就漏一个不退出的泵",
            alive_tasks()
        );
        live_after_each.push(alive_tasks());
    }
    assert!(
        live_after_each.windows(2).all(|w| w[1] <= w[0]),
        "多次开关后任务数不降反升：{live_after_each:?}（baseline={baseline}）"
    );
}
