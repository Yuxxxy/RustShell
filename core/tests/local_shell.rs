//! 本地 Shell 集成测试：真实走 ConPTY 链路，验证本地 PowerShell/cmd 能正常启动并执行命令。

use rustshell_core::session::{spawn_local_session, SessionHandle};
use std::io::{Read as _, Write as _};
use std::time::{Duration, Instant};

/// 启动「读 到 投递到 channel」的泵线程。
fn spawn_reader(session: &mut SessionHandle) -> std::sync::mpsc::Receiver<Vec<u8>> {
    let reader = session.reader.take().expect("reader 应存在");
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut reader = reader;
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
    rx
}

fn count_occurrences(haystack: &[u8], needle: &[u8]) -> usize {
    if needle.is_empty() || haystack.len() < needle.len() {
        return 0;
    }
    haystack
        .windows(needle.len())
        .filter(|w| *w == needle)
        .count()
}

/// 回归：终端只要按协议应答 DSR，本地 shell 必须能继续启动、接受输入并执行命令。
#[test]
fn local_shell_executes_command_when_dsr_is_answered() {
    let mut session = spawn_local_session(80, 24, None).expect("spawn_local_session 不应失败");
    let mut writer = session.writer.take().expect("writer 应存在");
    let rx = spawn_reader(&mut session);

    const MARKER: &[u8] = b"RSTEST";
    const DSR: &[u8] = b"\x1b[6n";

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut all: Vec<u8> = Vec::new();
    let mut dsr_replied_at: Option<Instant> = None;
    let mut command_sent = false;
    let mut echoed = 0usize;

    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(chunk) => all.extend_from_slice(&chunk),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }

        // 1. 终端语义：DSR 到 CPR 应答（修复后的 rustshell-terminal 会做这件事）
        if dsr_replied_at.is_none() && count_occurrences(&all, DSR) > 0 {
            writer
                .write_all(b"\x1b[1;1R")
                .expect("DSR 应答写回 PTY 失败");
            let _ = writer.flush();
            dsr_replied_at = Some(Instant::now());
        }

        // 2. 等 shell 起来后真的敲一条命令
        if let Some(t) = dsr_replied_at {
            if !command_sent && t.elapsed() > Duration::from_millis(1200) {
                writer
                    .write_all(b"echo RSTEST\r")
                    .expect("命令写回 PTY 失败");
                let _ = writer.flush();
                command_sent = true;
            }
        }

        // 3. 命令回显 + 执行结果 到 至少出现两次标记
        echoed = count_occurrences(&all, MARKER);
        if command_sent && echoed >= 2 {
            break;
        }
    }

    let text = String::from_utf8_lossy(&all).replace('\u{1b}', "<ESC>");
    assert!(
        dsr_replied_at.is_some(),
        "未观察到 DSR(`ESC[6n`) 查询；累计输出: {text}"
    );
    assert!(command_sent, "DSR 已应答但命令未能发出；累计输出: {text}");
    assert!(
        echoed >= 2,
        "命令 `echo RSTEST` 未见回显与执行结果（实际 {echoed} 次）——\
         终端在有正确 DSR 应答的情况下仍不可用；累计输出: {text}"
    );
    println!("本地 shell 命令往返成功（标记出现 {echoed} 次）");

    drop(session);
}
