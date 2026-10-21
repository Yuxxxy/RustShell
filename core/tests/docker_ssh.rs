//! Docker 真实 SSH 端到端测试。

use rustshell_core::connections::ConnectionProfile;
use rustshell_core::sftp::{new_transfer_id, sftp_events, SftpCmd, SftpEvent};
use rustshell_core::sftp_resume::{resume_store, ResumeMark, TransferDir};
use rustshell_core::ssh::{connect_in_background, trust_requests, ConnectResult, ServerKeyChange};
use std::time::Duration;

fn docker_enabled() -> bool {
    std::env::var("RUSTSHELL_DOCKER_TEST").is_ok()
}

/// 同一二进制里的 docker 用例都要读**全局** `trust_requests` 通道、又都改进程级
/// 环境变量（`RUSTSHELL_KNOWN_HOSTS` / `RUSTSHELL_SFTP_RESUME`），必须串行：
/// `cargo test` 默认并行跑同一文件里的用例，不串行就会互相偷走对方的信任询问。
static DOCKER_E2E_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn e2e_serial() -> std::sync::MutexGuard<'static, ()> {
    // 另一个用例 panic 会毒化这把锁，但后面的用例照样该跑（各用各的临时文件）
    DOCKER_E2E_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn profile() -> ConnectionProfile {
    let mut p = ConnectionProfile::new("docker-e2e", "127.0.0.1", "testuser");
    p.port = 2222;
    p.password = Some("test1234".into());
    p
}

fn connect_with_trust(p: &ConnectionProfile, trust: bool) -> ConnectResult {
    let (tx, rx) = flume::unbounded();
    let handle = connect_in_background(
        p.clone(),
        std::time::Duration::from_secs(20),
        None,
        1,
        None,
        tx,
    );
    // 等待可能的信任请求并决策
    if let Ok(prompt) = trust_requests().1.recv_timeout(Duration::from_secs(30)) {
        assert!(!prompt.fingerprint.is_empty(), "指纹不能为空");
        assert_eq!(prompt.host, "127.0.0.1");
        let _: Result<(), _> = prompt.reply.send(trust);
    }
    let res = rx.recv_timeout(Duration::from_secs(30)).expect("连接超时");
    handle.join().expect("连接线程 panic");
    res
}

/// 追加一条「主机已在 known_hosts 里」的连接：**不等**信任询问（不能等 ——
/// 没有询问时 `connect_with_trust` 会白等满 30s），但要盯住「不该有询问」这条。
fn connect_trusted(p: &ConnectionProfile, max_concurrent: u32) -> ConnectResult {
    let (tx, rx) = flume::unbounded();
    let handle = connect_in_background(
        p.clone(),
        Duration::from_secs(20),
        None,
        max_concurrent,
        None,
        tx,
    );
    match trust_requests().1.recv_timeout(Duration::from_secs(5)) {
        Err(flume::RecvTimeoutError::Timeout) => {}
        Ok(prompt) => panic!(
            "不该弹信任询问（这条连接的主机密钥与记录一致）：{}",
            prompt.fingerprint
        ),
        Err(e) => panic!("信任通道异常：{e}"),
    }
    let res = rx.recv_timeout(Duration::from_secs(30)).expect("连接超时");
    handle.join().expect("连接线程 panic");
    res
}

/// 连接并**捕获一次**信任询问：返回 (询问的 (新指纹, 成因), 连接结果)。
///
/// 与 `connect_with_trust` 的区别是它把 `kind` 也带出来 —— 「密钥变更」这条路径
/// 的关键正是「问的是哪一种」，只看连不连得上会把两种询问混为一谈。
fn connect_capturing_prompt(
    p: &ConnectionProfile,
    trust: bool,
) -> (Option<(String, ServerKeyChange)>, ConnectResult) {
    let (tx, rx) = flume::unbounded();
    let handle = connect_in_background(
        p.clone(),
        Duration::from_secs(20),
        None,
        1,
        None,
        tx,
    );
    let prompt = trust_requests()
        .1
        .recv_timeout(Duration::from_secs(20))
        .ok()
        .map(|pr| {
            let captured = (pr.fingerprint.clone(), pr.kind.clone());
            let _ = pr.reply.send(trust);
            captured
        });
    let res = rx.recv_timeout(Duration::from_secs(30)).expect("连接超时");
    handle.join().expect("连接线程 panic");
    (prompt, res)
}

#[test]
fn docker_ssh_e2e_trust_and_reconnect() {
    if !docker_enabled() {
        eprintln!("跳过：未设置 RUSTSHELL_DOCKER_TEST=1");
        return;
    }
    let _serial = e2e_serial();
    // 每次测试用全新 known_hosts
    let kh = std::env::temp_dir().join(format!("rustshell-kh-e2e-{}", std::process::id()));
    let _ = std::fs::remove_file(&kh);
    std::env::set_var("RUSTSHELL_KNOWN_HOSTS", &kh);

    // 1) 首连：未知主机 到 信任 到 连接成功
    let res = connect_with_trust(&profile(), true);
    assert!(res.is_ok(), "信任后首连应成功：{:?}", res.err());

    // known_hosts 应记录 [127.0.0.1]:2222
    let content = std::fs::read_to_string(&kh).expect("known_hosts 应已写入");
    assert!(
        content.contains("[127.0.0.1]:2222"),
        "known_hosts 缺少主机条目：{content}"
    );

    // 2) 重连：已信任 到 无 prompt 到 连接成功
    let res = connect_trusted(&profile(), 1);
    assert!(res.is_ok(), "重连应成功且无询问：{:?}", res.err());

    // 3) 密钥变更：known_hosts 里的记录换成另一把合法但**不同**的 ed25519。
    //
    // 这正是「docker 容器被删了重建」的形态：镜像里的 sshd host key 没挂持久卷，
    // 重建即重新生成 —— 账号口令都没变，客户端却撞上 KeyChanged。
    // 改造前这里**直接硬拒**（russh 只回一句 `Unknown server key`，界面上没有任何
    // 可操作入口）；现在必须先弹「已变更」询问，由人核对新旧指纹后决定。
    let bogus_key = "AAAAC3NzaC1lZDI1NTE5AAAAIAWyypbD1OxgqHPphLSN8o1fo71hm8Qg7jtKgGaTmw8l";
    std::fs::write(
        &kh,
        format!("[127.0.0.1]:2222 ssh-ed25519 {bogus_key}\n"),
    )
    .expect("写入伪造 known_hosts");

    // 3a) 取消 → 不建立连接，且不得偷偷改写记录
    let (prompt, res) = connect_capturing_prompt(&profile(), false);
    let (new_fp, kind) = prompt.expect("密钥变更必须弹询问，而不是硬拒");
    let (old_fp, old_type) = match &kind {
        ServerKeyChange::Changed {
            old_fingerprint,
            old_key_type,
        } => (old_fingerprint.clone(), old_key_type.clone()),
        other => panic!("询问成因应为 Changed（密钥变更），实际 {other:?}"),
    };
    assert_eq!(old_type, "ssh-ed25519");
    assert!(!old_fp.is_empty(), "变更询问必须带上旧指纹供核对");
    assert_ne!(old_fp, new_fp, "旧指纹与服务器本次提供的指纹不该相同");
    assert!(res.is_err(), "用户取消后不得建立连接");
    eprintln!("密钥变更（取消）信息：{:?}", res.err());
    assert!(
        std::fs::read_to_string(&kh).unwrap().contains(bogus_key),
        "取消时不得改写 known_hosts"
    );

    // 3b) 确认更新 → 连上，且旧行被**替换**（并存会让下次连接继续撞旧行）
    let (prompt2, res) = connect_capturing_prompt(&profile(), true);
    let (new_fp2, kind2) = prompt2.expect("确认前仍应再次询问（上次取消了）");
    assert_eq!(kind2, kind, "同一场景两次询问的成因必须一致");
    assert_eq!(new_fp2, new_fp);
    assert!(res.is_ok(), "确认更新密钥后应能连上：{:?}", res.err());
    let content = std::fs::read_to_string(&kh).expect("known_hosts 应存在");
    assert_eq!(
        content.lines().count(),
        1,
        "旧记录必须被替换而不是追加并存：{content}"
    );
    assert!(!content.contains(bogus_key), "旧密钥必须已删除：{content}");

    // 3c) 再连一次 → 无询问（更新后的信任已落盘）
    let res = connect_trusted(&profile(), 1);
    assert!(res.is_ok(), "更新密钥后重连应无需询问：{:?}", res.err());

    let _ = std::fs::remove_file(&kh);
    std::env::remove_var("RUSTSHELL_KNOWN_HOSTS");
}

// ---------------------------------------------------------------------------
// F-13 验收要点 3：中断续传**不重写**已确认的字节（真实 SFTP 通道）
// ---------------------------------------------------------------------------

/// 问服务器要 SFTP 子系统当前目录（= 登录用户的 home）。
///
/// 不猜 `/home/testuser`：linuxserver 这个镜像的 `testuser` home 是 `/config`
/// —— 「home 只有服务器知道」正是 `ListHome` 存在的理由。
fn remote_home_of(tx: &flume::Sender<SftpCmd>, rx: &flume::Receiver<SftpEvent>) -> String {
    tx.send(SftpCmd::ListHome).expect("发送 ListHome");
    loop {
        match rx.recv_timeout(Duration::from_secs(20)) {
            Ok(SftpEvent::Listing { path, .. }) => return path,
            Ok(SftpEvent::Error { message }) => panic!("ListHome 失败：{message}"),
            Ok(_) => {}
            Err(e) => panic!("等 ListHome 响应失败：{e}"),
        }
    }
}

/// 一次传输在事件通道上的全部表现。
struct TransferRun {
    ok: bool,
    message: String,
    notes: Vec<String>,
    /// 本传输报出的**第一个** `Progress` 值：续传时它必须等于续传起点
    /// （首个值 = 0 就是从头写了一遍，验收要点 3 的「证伪」项）
    first_progress: Option<u64>,
}

/// 发一条传输命令，只认**它自己**的 id 的事件（全局通道上可能夹着别的命令的动静）。
fn run_transfer(
    tx: &flume::Sender<SftpCmd>,
    rx: &flume::Receiver<SftpEvent>,
    cmd: SftpCmd,
    transfer_id: u64,
) -> TransferRun {
    tx.send(cmd).expect("发送传输命令");
    let mut run = TransferRun {
        ok: false,
        message: String::new(),
        notes: Vec::new(),
        first_progress: None,
    };
    loop {
        match rx.recv_timeout(Duration::from_secs(120)) {
            Ok(SftpEvent::TransferNote {
                transfer_id: id,
                text,
            }) if id == transfer_id => run.notes.push(text),
            Ok(SftpEvent::Progress {
                transfer_id: id,
                bytes,
            }) if id == transfer_id => {
                run.first_progress.get_or_insert(bytes);
            }
            Ok(SftpEvent::TransferDone {
                transfer_id: id,
                ok,
                message,
            }) if id == transfer_id => {
                run.ok = ok;
                run.message = message;
                return run;
            }
            Ok(_) => {}
            Err(e) => panic!("等 transfer #{transfer_id} 完成失败：{e}"),
        }
    }
}

/// 远端 `sha256sum` 复核（独立于 SFTP 写路径的第二种读法）。
///
/// 用 exec 信道跑命令而不是比对 SFTP 回读结果：SFTP 回读走的是我们自己刚写的那条
/// 路径，若写偏移有偏差、两边正好一致地错，回读比对是发现不了的。
fn remote_sha256(remote: &str) -> String {
    let res = rustshell_core::exec::exec_on(
        &profile(),
        &format!("sha256sum '{}'", remote.replace('\'', "")),
        Duration::from_secs(30),
        None,
    );
    assert!(
        res.is_ok(),
        "远端 sha256sum 失败：exit={:?} stderr={}",
        res.exit_code,
        res.stderr
    );
    res.stdout
        .split_whitespace()
        .next()
        .expect("sha256sum 没有输出摘要")
        .to_string()
}

/// 逐个字节比对，失败时只报**首个差异偏移** —— 200 KB 的 `assert_eq!` 打印出来没法看。
fn assert_same_bytes(got: &[u8], expect: &[u8], keep_prefix: u64, what: &str) {
    assert_eq!(got.len(), expect.len(), "{what}：长度不对");
    if let Some(at) = got.iter().zip(expect).position(|(a, b)| a != b) {
        panic!(
            "{what}：首个不同字节在偏移 {at}（前 {keep_prefix} 字节是已确认的部分，{at} < {keep_prefix} 说明它被重写了）"
        );
    }
}

/// 下载方向的一条续传腿：远端放**整份**源、本地放前 `from` 字节的半成品 + 一条
/// 同偏移的记录，断言只有后半段被写进本地、远端源分毫未动。
///
/// 抽成函数是为了让「并发 1 / 2 两档」跑**同一条**腿（验收要点 6 要的是两档功能
/// 一致，不是各测各的）。返回远端文件路径，由调用方清理。
#[allow(clippy::too_many_arguments)]
fn download_resume_leg(
    tx: &flume::Sender<SftpCmd>,
    rx: &flume::Receiver<SftpEvent>,
    home: &str,
    work: &std::path::Path,
    src: &std::path::Path,
    payload: &[u8],
    from: u64,
    tag: &str,
) -> String {
    let remote = format!(
        "{home}/rustshell-resume-down-{}-{tag}.bin",
        std::process::id()
    );
    let id = new_transfer_id();
    let seed = run_transfer(
        tx,
        rx,
        SftpCmd::Upload {
            local: src.to_path_buf(),
            remote: remote.clone(),
            transfer_id: id,
            total: payload.len() as u64,
        },
        id,
    );
    assert!(seed.ok, "[{tag}] 预置远端源文件失败：{}", seed.message);

    // 本地预置半成品：前 from 字节是 0xcc（本地侧「已传完」的那一段）
    let dst = work.join(format!("dst-{tag}.bin"));
    std::fs::write(&dst, vec![0xccu8; from as usize]).expect("写本地半成品");
    resume_store().record(ResumeMark::new(
        TransferDir::Download,
        dst.to_string_lossy().to_string(),
        remote.clone(),
        from,
        payload.len() as u64,
    ));
    resume_store().flush();

    let id = new_transfer_id();
    let down = run_transfer(
        tx,
        rx,
        SftpCmd::Download {
            remote: remote.clone(),
            local: dst.clone(),
            transfer_id: id,
            total: 0,
            verify: false,
        },
        id,
    );
    assert!(down.ok, "[{tag}] 续传下载失败：{}", down.message);
    assert!(
        down.notes.iter().any(|n| n.contains("续传")),
        "[{tag}] 下载续传必须给队列一条备注，实际：{:?}",
        down.notes
    );
    assert_eq!(
        down.first_progress,
        Some(from),
        "[{tag}] 续传下载报出的第一个进度必须是续传起点（0 说明它从头写了一遍）"
    );
    let mut expect = vec![0xccu8; from as usize];
    expect.extend_from_slice(&payload[from as usize..]);
    assert_same_bytes(
        &std::fs::read(&dst).expect("读下载结果"),
        &expect,
        from,
        &format!("[{tag}] 下载续传"),
    );
    // 下载侧的「回读」就是远端本身：远端源文件必须原样没被动过（续传只读远端）
    assert_eq!(
        remote_sha256(&remote),
        rustshell_core::sftp::sha256_hex(payload),
        "[{tag}] 续传下载不该改动远端源文件"
    );
    remote
}

/// 验收要点 4：**校验失败要可见**，且该传输的记录必须作废。
///
/// 怎么在没有故障网络的前提下造出「本地与远端不一致」：本地放一段**内容不对**的
/// 半成品（0xcc 前缀）并记一条同偏移的账 ⇒ 续传只补后半段，大小校验当然过
/// （补完正好等长），而「本地前缀摘出来的哈希」与「远端整份的哈希」必然不同
/// —— 这正是校验存在的理由：半成品是别的东西留下的。返回远端路径供清理。
#[allow(clippy::too_many_arguments)]
fn verify_mismatch_leg(
    tx: &flume::Sender<SftpCmd>,
    rx: &flume::Receiver<SftpEvent>,
    home: &str,
    work: &std::path::Path,
    src: &std::path::Path,
    payload: &[u8],
    from: u64,
) -> String {
    let remote = format!("{home}/rustshell-verify-{}.bin", std::process::id());
    let id = new_transfer_id();
    let seed = run_transfer(
        tx,
        rx,
        SftpCmd::Upload {
            local: src.to_path_buf(),
            remote: remote.clone(),
            transfer_id: id,
            total: payload.len() as u64,
        },
        id,
    );
    assert!(seed.ok, "预置远端源文件失败：{}", seed.message);

    let dst = work.join("dst-verify.bin");
    std::fs::write(&dst, vec![0xccu8; from as usize]).expect("写本地半成品");
    resume_store().record(ResumeMark::new(
        TransferDir::Download,
        dst.to_string_lossy().to_string(),
        remote.clone(),
        from,
        payload.len() as u64,
    ));
    resume_store().flush();

    let id = new_transfer_id();
    let run = run_transfer(
        tx,
        rx,
        SftpCmd::Download {
            remote: remote.clone(),
            local: dst.clone(),
            transfer_id: id,
            total: 0,
            verify: true,
        },
        id,
    );
    assert!(!run.ok, "内容不符必须报失败，实际：{}", run.message);
    assert!(
        run.message.contains("SHA-256"),
        "失败原因要指明是校验不符，实际：{}",
        run.message
    );
    // 消息里的两个摘要前缀必须分别是**本地期望**与**远端实际**的（expected/actual）
    let mut local_content = vec![0xccu8; from as usize];
    local_content.extend_from_slice(&payload[from as usize..]);
    let expect_local = rustshell_core::sftp::sha256_hex(&local_content);
    let expect_remote = rustshell_core::sftp::sha256_hex(payload);
    assert_ne!(
        &expect_local[..8],
        &expect_remote[..8],
        "两个摘要前缀一样的话这条用例证明不了任何事"
    );
    assert!(
        run.message.contains(&expect_local[..8]) && run.message.contains(&expect_remote[..8]),
        "消息要同时给出两侧摘要前缀（本地 {expect_local:?} / 远端 {expect_remote:?}），实际：{}",
        run.message
    );
    assert_eq!(
        std::fs::metadata(&dst).map(|m| m.len()).unwrap_or(0),
        payload.len() as u64,
        "校验失败的文件应当保留（用户可自行删除/重传）"
    );
    assert!(
        resume_store()
            .find(TransferDir::Download, &dst.to_string_lossy(), &remote)
            .is_none(),
        "内容不符的半成品不能拿来续传：记录必须作废"
    );
    remote
}

#[test]
fn docker_sftp_resume_does_not_rewrite_confirmed_bytes() {
    if !docker_enabled() {
        eprintln!("跳过：未设置 RUSTSHELL_DOCKER_TEST=1");
        return;
    }
    let _serial = e2e_serial();

    let work = std::env::temp_dir().join(format!("rustshell-resume-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).expect("建临时目录");
    let kh = work.join("known_hosts");
    std::env::set_var("RUSTSHELL_KNOWN_HOSTS", &kh);
    // 记录落在临时目录：别碰开发机真实的 sftp_resume.json
    std::env::set_var("RUSTSHELL_SFTP_RESUME", work.join("resume.json"));

    let session = connect_with_trust(&profile(), true).expect("连接失败");
    let tx = session.sftp_tx.clone().expect("SFTP 常驻服务未启动");
    let rx = &sftp_events().1;
    let home = remote_home_of(&tx, rx);
    eprintln!("远端 home = {home}");

    // 源文件内容：周期 251（质数，不与 0xbb/0xcc 前缀周期对齐，误判不了）
    const LEN: usize = 200_000;
    // 已确认的偏移：**不是** CHUNK(64 KiB) 的整数倍 —— 否则「从块边界接上」的
    // 实现偶然也能过，测不出 seek 偏移对不对。
    const FROM: u64 = 131_072 + 4_096 + 7;
    let payload: Vec<u8> = (0..LEN).map(|i| (i % 251) as u8).collect();
    let src = work.join("src.bin");
    std::fs::write(&src, &payload).expect("写本地源文件");
    let src_s = src.to_string_lossy().to_string();
    let pid = std::process::id();

    // ---------- 方向一：上传续传 ----------
    let remote_up = format!("{home}/rustshell-resume-up-{pid}.bin");
    // 远端预置半成品：前 FROM 字节是 0xbb。续传若失效（从头重写），这半段会被
    // `payload` 覆盖 ⇒ 下面的比对在偏移 0 就红。
    let partial = work.join("partial.bin");
    std::fs::write(&partial, vec![0xbbu8; FROM as usize]).expect("写半成品");
    let id = new_transfer_id();
    let pre = run_transfer(
        &tx,
        rx,
        SftpCmd::Upload {
            local: partial,
            remote: remote_up.clone(),
            transfer_id: id,
            total: FROM,
        },
        id,
    );
    assert!(pre.ok, "预置远端半成品失败：{}", pre.message);

    // 账本记「前 FROM 字节已传」，并落盘一次（走真实的重启口径，而不是只在内存里）
    resume_store().record(ResumeMark::new(
        TransferDir::Upload,
        src_s.clone(),
        remote_up.clone(),
        FROM,
        LEN as u64,
    ));
    resume_store().flush();

    let id = new_transfer_id();
    let up = run_transfer(
        &tx,
        rx,
        SftpCmd::Upload {
            local: src.clone(),
            remote: remote_up.clone(),
            transfer_id: id,
            // total 交 0：让服务端自己 stat 本地补总量（UI 的目录上传就是这么发的）
            total: 0,
        },
        id,
    );
    assert!(up.ok, "续传上传失败：{}", up.message);
    assert!(
        up.notes.iter().any(|n| n.contains("续传")),
        "续传必须给队列一条备注（否则用户看不出它续上了），实际：{:?}",
        up.notes
    );
    assert_eq!(
        up.first_progress,
        Some(FROM),
        "续传上传报出的第一个进度必须是续传起点（0 说明它从头写了一遍）"
    );
    assert!(
        resume_store()
            .find(TransferDir::Upload, &src_s, &remote_up)
            .is_none(),
        "成功之后不该还留着续传记录（半成品已不存在）"
    );

    // 回读远端文件：前 FROM 字节必须还是 0xbb（没被重写），其余等于源文件
    let back = work.join("back-up.bin");
    let id = new_transfer_id();
    let read_back = run_transfer(
        &tx,
        rx,
        SftpCmd::Download {
            remote: remote_up.clone(),
            local: back.clone(),
            transfer_id: id,
            total: 0,
            verify: false,
        },
        id,
    );
    assert!(read_back.ok, "回读远端文件失败：{}", read_back.message);
    let mut expect = vec![0xbbu8; FROM as usize];
    expect.extend_from_slice(&payload[FROM as usize..]);
    assert_same_bytes(
        &std::fs::read(&back).expect("读回读结果"),
        &expect,
        FROM,
        "上传续传",
    );
    // 独立复核：服务器自己算的摘要（exec 信道）必须与本地拼出的期望内容一致
    assert_eq!(
        remote_sha256(&remote_up),
        rustshell_core::sftp::sha256_hex(&expect),
        "远端 sha256sum 与期望内容不符"
    );

    // ---------- 方向二：下载续传（并发 1 档） ----------
    let mut created = vec![remote_up.clone()];
    created.push(download_resume_leg(
        &tx, rx, &home, &work, &src, &payload, FROM, "conc1",
    ));

    // ---------- 验收要点 6：并发 2 档跑同一条腿，结果必须一样 ----------
    // 连接时传的 max_concurrent 只影响信号量（新建连接生效），续传行为与它无关 ——
    // 这条腿就是拿来证明这一点的。
    let session2 = connect_trusted(&profile(), 2).expect("并发 2 档连接失败");
    let tx2 = session2.sftp_tx.clone().expect("SFTP 常驻服务未启动");
    created.push(download_resume_leg(
        &tx2, rx, &home, &work, &src, &payload, FROM, "conc2",
    ));

    // ---------- 验收要点 4：校验失败可见 + 记录作废 ----------
    created.push(verify_mismatch_leg(
        &tx, rx, &home, &work, &src, &payload, FROM,
    ));

    // 收尾：删掉远端这次建的文件（容器是共享的，别留垃圾）。
    // 等足额 Refresh 回执而不是 sleep —— 删干净了才结束。
    for path in &created {
        tx.send(SftpCmd::Remove {
            path: path.clone(),
            is_dir: false,
            recursive: false,
            refresh: home.clone(),
        })
        .expect("发送 Remove");
    }
    let mut refreshed = 0;
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while refreshed < created.len() && std::time::Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(SftpEvent::Refresh { path }) if path == home => refreshed += 1,
            Ok(SftpEvent::Error { message }) => panic!("清理远端文件失败：{message}"),
            Ok(_) => {}
            Err(e) => panic!("等清理回执失败：{e}"),
        }
    }
    assert_eq!(refreshed, created.len(), "远端临时文件没有删干净");

    let _ = std::fs::remove_dir_all(&work);
    std::env::remove_var("RUSTSHELL_KNOWN_HOSTS");
    std::env::remove_var("RUSTSHELL_SFTP_RESUME");
}
