//! SSH 会话（russh 0.63）：连接 到 认证 到 PTY/Shell 信道 到 桥接为同步 Pipe 流。

use crate::connections::ConnectionProfile;
use crate::metrics::{DfColumns, DiskInfo, MetricsStore, Sample, SystemOverview};
use crate::proxy::ProxyConfig;
use crate::session::{
    build_session_pipes, InSink, OutSource, PipesSpec, ResizeSink, SessionHandle, SessionKind,
    SshResizeCoalescer,
};
use anyhow::Context as _;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
/// 全局 tokio 运行时（russh 需要；与 UI 线程解耦）。
///
/// ## worker 数为什么不能写死小值（2026-09-28 修 C-1）
///
/// 这里原先写死 `worker_threads(2)`，是全系统**最脆弱的单点**：所有 SSH
/// I/O 泵、远端指标轮询、SFTP 服务、端口转发、`exec_async` 全挤在这两条
/// worker 上。只要有一个任务进入「无挂起点」的长循环（或干脆是 CPU 密集），
/// 另外一条也很快被占满 ⇒ **整个 SSH 子系统停摆**：终端无输出、resize 不生效、
/// SFTP 卡死，而 UI 进程看起来还活着。
/// `ssh.rs` 里那段 2026-09-20 的事故注释（进程 273% 单核、UI 掉到 10fps）
/// 就是这条线上真实发生过的。
///
/// 现在取 `max(4, 逻辑核数 / 2)`：
/// - **下限 4**：保证「I/O 泵 / 指标轮询 / SFTP / 转发」四类任务各有喘息，
///   即便跑在单核虚拟机上也不会两条互等；
/// - **核数 / 2**：SSH 这类任务大部分时间在等网络，不需要占满全部核；
///   留一半给 UI 与本地终端渲染（它们在自己的线程上，但 CPU 是共享的）；
/// - 查询失败（受限环境返回 `Err`）时回落下限 4，绝不 panic。
pub fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(ssh_worker_threads())
            .enable_all()
            .build()
            .expect("build tokio runtime")
    })
}

/// SSH 运行时该起多少条 worker（见 [`runtime`] 的说明）。
///
/// 抽成独立函数是为了**可单测**：`runtime()` 是 `OnceLock`，一旦初始化就再也
/// 测不到别的核数分支，而「worker 数别退回 2」正是需要钉住的回归点。
pub fn ssh_worker_threads() -> usize {
    /// 下限：四类主要 SSH 任务（I/O 泵 / 指标 / SFTP / 转发）至少各有喘息。
    const MIN_WORKERS: usize = 4;
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(MIN_WORKERS);
    (cores / 2).max(MIN_WORKERS)
}

/// 主机密钥校验器（russh `Handler` 实现）。
///
/// 公开是因为 [`connect_only`] 的返回类型里带着它 —— 调用方（`exec`）需要拿到
/// `Handle` 去开自己的信道。字段一律私有：外部只该通过 [`HostKeyPolicy`] 影响行为，
/// 不该直接改校验逻辑。
pub struct ServerKeyVerifier {
    host: String,
    port: u16,
    /// 信任对话框等待用户决策时置位，establish 的连接超时据此顺延，避免把「等用户点击信任」误判为「握手超时」。
    trust_pending: Arc<AtomicBool>,
    /// 未知主机密钥的处置方式（见 [`HostKeyPolicy`]）。
    ///
    /// 这个字段是「无人值守」安全边界的关键：定时任务与巡检跑在后台，
    /// **没有人能回答信任对话框**，此时静默信任等于把中间人攻击的门打开。
    policy: HostKeyPolicy,
    /// 本次握手被拒的**可读原因**（unknown / changed / 证书类 / 用户取消）。
    ///
    /// 必须有这条旁路：russh 把四类拒绝一律折叠成 `Error::UnknownKey`（文本
    /// `Unknown server key`），只从错误串上根本分不出「主机密钥变了」和「从没见过
    /// 这台主机」。而这两件事的后果完全不同 —— 前者可能是中间人攻击。校验器知道
    /// 真相，就在拒绝的同一处把它记下来，建连失败时由 [`exec_friendly_err`] 取用。
    reject: Arc<Mutex<Option<String>>>,
}

/// 未知主机密钥的处置方式。
///
/// `known_hosts` 中**已有且一致**的密钥永远直接通过；本策略只决定
/// 「未知」这一种情形（不一致 = 疑似中间人，任何策略下都硬拒绝）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostKeyPolicy {
    /// 弹首次信任对话框等用户决策（交互式会话的默认行为）。
    AskUser,
    /// 直接拒绝并留下可读原因。
    ///
    /// 定时任务 / AI 巡检 / 批量 exec 必须用它：无人可答，静默信任不可接受。
    RejectUnknown,
}

/// 未知主机密钥被 [`HostKeyPolicy::RejectUnknown`] 拒绝时返回的错误前缀。
///
/// 单独提成常量是因为 UI 与告警引擎都要按它把「主机密钥未知」识别成
/// **巡检失败**（而不是「服务器异常」），拼错一个字符就会让分类失效。
pub const HOST_KEY_UNKNOWN_MARK: &str = "主机密钥未知";

/// 已知主机但**密钥变了**时的错误标记。
///
/// 与 [`HOST_KEY_UNKNOWN_MARK`] 分开是刻意的：两者在告警引擎里的分级不同
/// （变更 = 疑似中间人，Critical；未知 = 待信任，Warning），见
/// `runner::inspect::inspection_failure_finding`。
/// 文案里必须带「不一致」三个字 —— 那条分流就是按它判 Critical 的。
pub const HOST_KEY_CHANGED_MARK: &str = "主机密钥已变更";

/// 首次信任请求的成因 —— 决定对话框说什么、点「信任」后怎么写 known_hosts。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerKeyChange {
    /// 这台主机没记录过（或 known_hosts 读不到）：核对该指纹即可信任。
    FirstSeen,
    /// 记录过，但服务器现在给的是**另一把**密钥。
    ///
    /// 最常见的合法成因是「容器被删了重建 / 系统重装 / sshd 重新生成了 host key」——
    /// 只要镜像里的 host key 没有挂持久卷，每次重建都会变。但同样的现象也可能是
    /// 中间人攻击，所以绝不能自动接受，必须让人看着新旧指纹做决定。
    Changed {
        /// 旧记录（known_hosts 里那条）的 OpenSSH 风格指纹
        old_fingerprint: String,
        /// 旧记录的密钥类型，形如 "ssh-ed25519"
        old_key_type: String,
    },
}

/// 首次信任请求（连接线程 到 UI）：展示指纹等待用户决策
pub struct ServerKeyPrompt {
    pub host: String,
    pub port: u16,
    /// OpenSSH 风格指纹，形如 "SHA256:xxxx"
    pub fingerprint: String,
    /// 密钥类型，形如 "ssh-ed25519"
    pub key_type: String,
    /// 这次询问是「首次连接」还是「密钥变了」（UI 据此换标题、换按钮、加警告）
    pub kind: ServerKeyChange,
    /// 用户决策回传：true = 信任并加入 known_hosts
    pub reply: flume::Sender<bool>,
}

/// 信任请求通道（全局）：MainView 轮询接收并渲染对话框
pub fn trust_requests() -> &'static (
    flume::Sender<ServerKeyPrompt>,
    flume::Receiver<ServerKeyPrompt>,
) {
    static CH: std::sync::OnceLock<(
        flume::Sender<ServerKeyPrompt>,
        flume::Receiver<ServerKeyPrompt>,
    )> = std::sync::OnceLock::new();
    CH.get_or_init(flume::unbounded)
}

/// known_hosts 文件路径：<config>/rustshell/known_hosts（与 platform::paths 保持一致）
fn known_hosts_path() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("RUSTSHELL_KNOWN_HOSTS") {
        return std::path::PathBuf::from(p);
    }
    crate::config_dir::config_dir().join("known_hosts")
}

/// known_hosts 里的主机字段：标准端口写裸主机名，非标准端口写 `[host]:port`。
fn known_hosts_entry(host: &str, port: u16) -> String {
    if port == 22 {
        host.to_string()
    } else {
        format!("[{host}]:{port}")
    }
}

/// 将已信任的主机密钥以 OpenSSH 格式追加到 known_hosts
fn record_known_host(path: &std::path::Path, host: &str, port: u16, key: &russh::keys::PublicKey) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let host_entry = known_hosts_entry(host, port);
    if let Ok(line) = key.to_openssh() {
        use std::io::Write as _;
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            Ok(mut f) => {
                // Unix 上加固文件权限：known_hosts 仅属主可读写
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;
                    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
                }
                if let Err(e) = writeln!(f, "{host_entry} {line}") {
                    log::warn!("写入 known_hosts 失败: {e}");
                }
            }
            Err(e) => log::warn!("打开 known_hosts 失败: {e}"),
        }
    }
}

/// known_hosts 的「主机字段」是否指向 `entry`。
///
/// 认三种写法：裸主机名 / 逗号分隔的多个别名 / OpenSSH 的哈希形式（`|1|salt|hash`）。
/// 哈希形式无法反解，只能按「可能是它」处理 —— 但删除时还要**同时**命中旧密钥，
/// 所以放宽的代价只是可能多删一条同密钥别名，不会误伤别的密钥。
fn host_field_matches(field: &str, entry: &str) -> bool {
    field
        .split(',')
        .any(|part| part == entry || part.starts_with("|1|"))
}

/// 从 known_hosts **文本**里剔除「同一主机 + 指定旧密钥」的记录，其余行原样保留。
///
/// 抽成纯函数是为了可单测：这段逻辑一旦写错，用户点「更新密钥」后依然连不上
/// （旧行还在，每次校验都先撞上它），而现象与「没修」一模一样，极难排查。
///
/// - 注释行、空行、其它主机的行、**其它算法**的行一律保留；
/// - 兼容 `@cert-authority` / `@revoked` 前缀（字段整体右移一位）；
/// - 保留原文件的「末尾是否有换行」，不给下游留意外差异。
fn without_stale_key(
    content: &str,
    entry: &str,
    stale: &[russh::keys::PublicKey],
) -> String {
    if stale.is_empty() || content.is_empty() {
        return content.to_string();
    }
    let ends_with_newline = content.ends_with('\n');
    let mut out = String::with_capacity(content.len());
    for raw in content.lines() {
        // `lines()` 会吞掉 `\r`，这里显式去掉后再判断 —— 否则 Windows 上写回的行
        // 与解析用的行不一致。
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        let trimmed = line.trim();
        let toks: Vec<&str> = trimmed.split_whitespace().collect();
        let base = if toks.first().is_some_and(|t| t.starts_with('@')) {
            1
        } else {
            0
        };
        let stale_line = match (toks.get(base), toks.get(base + 2)) {
            (Some(hosts), Some(b64)) => {
                host_field_matches(hosts, entry)
                    && matches!(
                        russh::keys::parse_public_key_base64(b64),
                        Ok(k) if stale.iter().any(|s| *s == k)
                    )
            }
            _ => false,
        };
        if !stale_line {
            out.push_str(line);
            out.push('\n');
        }
    }
    if !ends_with_newline && out.ends_with('\n') {
        out.pop();
    }
    out
}

/// 用新密钥**替换**该主机同算法的旧记录（容器重建后重新信任时用）。
///
/// 只删「同主机 + 同算法」的行：不同算法的记录（例如还留着一条 `ssh-rsa`）
/// 不参与 `KeyChanged` 判定，留着无害，删了反而要重新信任一次。
fn replace_known_host(
    path: &std::path::Path,
    host: &str,
    port: u16,
    key: &russh::keys::PublicKey,
) {
    let entry = known_hosts_entry(host, port);
    // 旧记录走 russh 自己的匹配逻辑（含哈希主机名），不自己解析 —— 两套解析
    // 一旦漂移，就会出现「校验说变了、删除却没删到」的死循环。
    let stale: Vec<russh::keys::PublicKey> =
        russh::keys::known_hosts::known_host_keys_path(host, port, path)
            .unwrap_or_default()
            .into_iter()
            .filter(|(_, k)| k.algorithm() == key.algorithm())
            .map(|(_, k)| k)
            .collect();
    match std::fs::read_to_string(path) {
        Ok(content) => {
            let next = without_stale_key(&content, &entry, &stale);
            if next != content {
                if let Err(e) = std::fs::write(path, &next) {
                    log::warn!("重写 known_hosts 失败（{e}）：旧密钥残留会让下次连接继续被拒");
                }
            }
        }
        // 文件读不出来（不存在 = 首次，正常）就不改写，直接追加
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            log::warn!("读取 known_hosts 失败（{e}）：无法清理旧记录");
        }
        Err(_) => {}
    }
    // 新密钥本身可能已经在文件里（同一把密钥的重复行），别重复追加
    let already = key
        .to_openssh()
        .ok()
        .map(|line| format!("{entry} {line}"))
        .is_some_and(|want| {
            std::fs::read_to_string(path)
                .map(|c| c.lines().any(|l| l.trim_end() == want))
                .unwrap_or(false)
        });
    if !already {
        record_known_host(path, host, port, key);
    }
}

const TRUST_TIMEOUT: Duration = Duration::from_secs(120);

impl ServerKeyVerifier {
    /// 记下本次被拒的可读原因（供建连失败时拼用户文案）。
    fn note_reject(&self, reason: String) {
        if let Ok(mut slot) = self.reject.lock() {
            *slot = Some(reason);
        }
    }

    /// 只在不比已有原因更具体时补一条。
    ///
    /// 「主机密钥已变更（旧指纹 → 新指纹）」比「用户取消了」信息量大得多，不能被后者盖掉 ——
    /// 用户点取消之后看到的最终报错，最该知道的是**为什么**被拒，而不是他刚做过的动作。
    fn note_reject_weak(&self, reason: String) {
        if let Ok(mut slot) = self.reject.lock() {
            if slot.is_none() {
                *slot = Some(reason);
            }
        }
    }

    /// known_hosts 里该主机、与新密钥**同算法**的旧记录（用于展示「旧指纹」）。
    ///
    /// 只按算法筛：`KeyChanged` 只在算法相同时才可能触发，因此这条就是
    /// 冲突的那一条。读不到就返回 `None`，调用方降级成 `<读取失败>`。
    fn recorded_key_of_same_alg(
        &self,
        path: &std::path::Path,
        new_key: &russh::keys::PublicKey,
    ) -> Option<russh::keys::PublicKey> {
        russh::keys::known_hosts::known_host_keys_path(&self.host, self.port, path)
            .ok()?
            .into_iter()
            .map(|(_, k)| k)
            .find(|k| k.algorithm() == new_key.algorithm())
    }

    /// 发送信任请求给 UI 并等待用户决策（120s 超时拒绝）。
    ///
    /// `change` 决定「信任」后怎么写 known_hosts：首次 → 追加；密钥变更 →
    /// **先清掉旧行再追加**（否则下次连接又撞上旧行）。
    async fn wait_user_trust(
        &self,
        key: &russh::keys::PublicKey,
        path: &std::path::Path,
        change: ServerKeyChange,
    ) -> bool {
        let (reply_tx, reply_rx) = flume::bounded(1);
        let replace = matches!(change, ServerKeyChange::Changed { .. });
        let prompt = ServerKeyPrompt {
            host: self.host.clone(),
            port: self.port,
            fingerprint: key.fingerprint(russh::keys::HashAlg::Sha256).to_string(),
            key_type: key.algorithm().to_string(),
            kind: change,
            reply: reply_tx,
        };
        log::info!("未知主机 {}:{}，等待用户信任决策", self.host, self.port);
        self.trust_pending.store(true, AtomicOrdering::SeqCst);
        let ok = if trust_requests().0.send(prompt).is_ok() {
            matches!(
                tokio::time::timeout(TRUST_TIMEOUT, reply_rx.recv_async()).await,
                Ok(Ok(true))
            )
        } else {
            false
        };
        self.trust_pending.store(false, AtomicOrdering::SeqCst);
        if ok {
            if replace {
                replace_known_host(path, &self.host, self.port, key);
            } else {
                record_known_host(path, &self.host, self.port, key);
            }
        } else {
            // 用户点了取消 / 对话框超时：也得说清楚是「没信任」而不是「网络不通」
            self.note_reject_weak(format!(
                "{}:{} 的主机密钥未获信任（已取消或等待超时），连接已中止",
                self.host, self.port
            ));
        }
        ok
    }
}

impl russh::client::Handler for ServerKeyVerifier {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &russh::keys::PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let key = match server_public_key {
            russh::keys::PublicKeyOrCertificate::PublicKey { key, .. } => key,
            russh::keys::PublicKeyOrCertificate::Certificate(_) => {
                // 安全红线：证书类主机密钥未经 TOFU 校验，
                // 无条件信任会绕过 known_hosts 防护 到 一律拒绝并留痕。
                log::error!(
                    "服务器 {}:{} 返回证书类主机密钥，未经 TOFU 校验，拒绝连接",
                    self.host,
                    self.port
                );
                self.note_reject(format!(
                    "{}:{} 返回的是证书类主机密钥，本客户端不做证书链校验，已拒绝连接",
                    self.host, self.port
                ));
                return Ok(false);
            }
        };

        let path = known_hosts_path();
        match russh::keys::check_known_hosts_path(&self.host, self.port, key, &path) {
            // 已信任且密钥一致
            Ok(true) => Ok(true),
            // 未知主机 -> 按策略处置
            Ok(false) => match self.policy {
                HostKeyPolicy::AskUser => {
                    Ok(self
                        .wait_user_trust(key, &path, ServerKeyChange::FirstSeen)
                        .await)
                }
                HostKeyPolicy::RejectUnknown => {
                    log::warn!(
                        "{}:{} 的主机密钥未知，无人值守策略下拒绝连接（需先手动连接一次完成信任）",
                        self.host,
                        self.port
                    );
                    self.note_reject(format!(
                        "{HOST_KEY_UNKNOWN_MARK}：{}:{} 不在 known_hosts 中。\
                         无人值守任务不会自动信任未知主机，请先在连接面板手动连接一次完成信任",
                        self.host, self.port
                    ));
                    Ok(false)
                }
            },
            // 密钥与记录不一致：可能是容器重建 / 系统重装，也可能是中间人攻击
            Err(russh::keys::Error::KeyChanged { line }) => {
                let old = self.recorded_key_of_same_alg(&path, key);
                let old_fp = old
                    .as_ref()
                    .map(|k| k.fingerprint(russh::keys::HashAlg::Sha256).to_string())
                    .unwrap_or_else(|| "<读取失败>".into());
                let old_type = old
                    .as_ref()
                    .map(|k| k.algorithm().to_string())
                    .unwrap_or_else(|| "<读取失败>".into());
                let new_fp = key.fingerprint(russh::keys::HashAlg::Sha256).to_string();
                let new_type = key.algorithm().to_string();
                log::error!(
                    "主机 {}:{} 的密钥与 known_hosts 第 {} 行记录不一致（旧 {old_type} {old_fp} → 新 {new_type} {new_fp}），拦截连接",
                    self.host,
                    self.port,
                    line + 1
                );
                // 文案里必须出现「不一致」：告警引擎按这三个字把它判为 Critical（疑似中间人）
                self.note_reject(format!(
                    "{HOST_KEY_CHANGED_MARK}：{}:{} 的服务器密钥与 known_hosts 记录不一致\
                     （旧 {old_type} {old_fp} → 新 {new_type} {new_fp}）。\
                     常见成因是服务器/容器被重建、系统重装或 sshd 重新生成了 host key；\
                     若并非你所为，请按中间人攻击处理",
                    self.host, self.port
                ));
                match self.policy {
                    // 交互式：让人看着新旧指纹决定，而不是丢一句 Unknown server key 了事
                    HostKeyPolicy::AskUser => Ok(self
                        .wait_user_trust(
                            key,
                            &path,
                            ServerKeyChange::Changed {
                                old_fingerprint: old_fp,
                                old_key_type: old_type,
                            },
                        )
                        .await),
                    // 无人值守：绝不自动接受变更
                    HostKeyPolicy::RejectUnknown => Ok(false),
                }
            }
            // 文件不可读等异常：按「未知主机」处理（同样受策略约束 ——
            // 无人值守下不能因为 known_hosts 读不到就降级为无条件信任）
            Err(e) => {
                log::warn!("known_hosts 校验异常（{e}）");
                match self.policy {
                    HostKeyPolicy::AskUser => Ok(self
                        .wait_user_trust(key, &path, ServerKeyChange::FirstSeen)
                        .await),
                    HostKeyPolicy::RejectUnknown => Ok(false),
                }
            }
        }
    }

    /// 服务端为**远端转发**（`ssh -R`）打开的信道：认领了才接受。
    ///
    /// ★ 默认实现是「照单全收，然后把信道丢掉」—— 那会在服务器上留下一个
    /// **黑洞端口**：TCP 握手成功、SSH 信道建立，然后没有任何字节被回过去，
    /// 连接一直挂到客户端超时。真机上表现为「浏览器转圈转到超时」，比直接
    /// 拒绝难查得多。所以这里必须显式检查有没有规则在等这条信道。
    async fn server_channel_open_forwarded_tcpip(
        &mut self,
        channel: russh::Channel<russh::client::Msg>,
        _connected_address: &str,
        connected_port: u32,
        originator_address: &str,
        _originator_port: u32,
        reply: russh::client::ChannelOpenHandle,
        _session: &mut russh::client::Session,
    ) -> Result<(), Self::Error> {
        if !crate::forward::has_remote_route(connected_port) {
            log::debug!("拒绝未认领的远端转发信道（端口 {connected_port}）");
            reply
                .reject(russh::ChannelOpenFailure::AdministrativelyProhibited)
                .await;
            return Ok(());
        }
        reply.accept().await;
        if !crate::forward::deliver_forwarded(
            connected_port,
            channel,
            originator_address.to_string(),
        ) {
            // 检查与投递之间规则被停掉了（用户点了「停止」）。信道已接受，
            // 直接丢弃即等于关掉它 —— 只是这一次连接的失败原因要留痕。
            log::warn!("远端转发信道无人认领（端口 {connected_port}），连接已丢弃");
        }
        Ok(())
    }
}

fn ssh_config() -> Arc<russh::client::Config> {
    let config = russh::client::Config {
        inactivity_timeout: None, // 交互式 shell 不做闲置断开
        keepalive_interval: Some(Duration::from_secs(30)),
        keepalive_max: 6,
        ..Default::default()
    };
    Arc::new(config)
}

/// 连接结果（连接线程 到 UI 轮询通道传递）。Err 携带 (profile_id, 错误描述)，供 UI 清理该 profile 的「连接中」标记。
pub type ConnectResult = Result<SessionHandle, (String, String)>;

/// 本次连接允许打开哪些通道。
///
/// 常规连接（`Full`）会同时用 shell、远端指标（每 5 秒一条 exec 通道）、
/// SFTP 子系统与端口转发。**部分堡垒机禁止一条连接上多开通道**，那种环境里
/// 多开一条 exec 就会整条被拒。外部链接（`ssh://` / `sftp://`）因此各只要一条：
/// 终端只要 shell，SFTP 只要 sftp 子系统。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SshChannels {
    /// 常规连接：shell + 指标 + SFTP + 端口转发（按配置）。
    #[default]
    Full,
    /// 只开 shell 一条（外部 `ssh://`）。没有 SFTP，也没有远端指标。
    ShellOnly,
    /// 只开 sftp 子系统一条（外部 `sftp://`）。没有终端，也没有远端指标。
    SftpOnly,
}

/// 同步入口：后台线程中执行异步连接，结果经通道返回给 UI 轮询。
///
/// `log_path`：会话日志落点（F-7）。`None` = 不记录。**调用方算好**（全局开关
/// 与连接级覆盖的优先级见 [`crate::session_log::enabled_for`]）——
/// core 不知道配置文件在哪，也没法自己拼出日期目录。
///
/// `sftp_max_concurrent`：SFTP 传输并发上限（F-13，1–4）。同样由调用方从设置
/// 传入 —— core 不读设置文件。改动只影响**本次**连接（SFTP 服务任务的信号量
/// 在建立时创建），所以设置项上写着「新建连接生效」。
///
/// `global_proxy`：全局代理（F-10）的运行期形态（明文口令已由宿主解出）。
/// 传**全局值**而不是「本条连接该走什么」：三态合成收口在 [`connect_only`]，
/// 调用点不必（也不该）各自解析 —— 见那边的注释。
pub fn connect_in_background(
    profile: ConnectionProfile,
    timeout: Duration,
    log_path: Option<std::path::PathBuf>,
    sftp_max_concurrent: u32,
    global_proxy: Option<ProxyConfig>,
    channels: SshChannels,
    result_tx: flume::Sender<ConnectResult>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name(format!("rs-ssh-{}", profile.name))
        .spawn(move || {
            log::info!(
                "SSH 连接线程启动: profile={} {}@{}:{} (超时 {}s)",
                profile.name,
                profile.username,
                profile.host,
                profile.port,
                timeout.as_secs()
            );
            let res = runtime()
                .block_on(establish(
                    &profile,
                    timeout,
                    log_path,
                    sftp_max_concurrent,
                    global_proxy.as_ref(),
                    channels,
                ))
                .map_err(|e| (profile.id.clone(), e));
            match &res {
                Ok(s) => log::info!("SSH 连接成功: tab={}", s.id),
                Err((_, e)) => log::warn!("SSH 连接结束（失败）: {e}"),
            }
            let _ = result_tx.send(res);
        })
        .expect("spawn ssh connect thread")
}

/// 「测试连接」的结果：成功给一句可直接展示的摘要，失败给原因。
pub type TestConnectResult = Result<String, String>;

/// 测试连接：握手 + 认证，**成功后立刻断开** —— 不建 PTY、不开 exec 信道、不起采样泵。
///
/// 为什么不复用 [`connect_in_background`]：那条路径的产物是一个**会话**（标签、终端
/// 视图、指标泵），而测试要回答的只有一句话「这台机器现在能不能用这份凭据登进去」。
/// 每多做一步，就多一份必须收尾的状态 —— 而对话框里根本没有收尾的地方。
///
/// 返回 `Receiver` 而不是 `JoinHandle`：调用方（对话框）只需要一句话，
/// `flume::Receiver::recv_async()` 可以直接 `await`，不必再关心线程。
///
/// 未识别的主机密钥按**交互式**处理（[`HostKeyPolicy::AskUser`]，2026-10-21 用户要求
/// 「测试连接功能移除无人值守之类的限制」）。
///
/// 原先这里是 `RejectUnknown`，理由是「测试发生在对话框里，此刻没有人能回答指纹
/// 信任弹窗」。那条理由站不住：测试连接恰恰是**人正盯着对话框、点了按钮**的时刻 ——
/// 弹信任框正是他期待的下一步。用拒绝代替询问，用户得到的是「请先手动连接一次
/// 完成信任」这种绕路提示：想验证一份凭据，却被迫先建一个会话。
///
/// 安全边界没有放松：`AskUser` 只在 known_hosts **没有记录**时询问，密钥变更仍要
/// 用户看着新旧指纹确认（见 `ServerKeyChange`）；真正无人值守的路径（`exec` /
/// SFTP 服务任务 / 定时巡检）依旧走 `RejectUnknown`。
pub fn test_connection_in_background(
    profile: ConnectionProfile,
    timeout: Duration,
    global_proxy: Option<ProxyConfig>,
) -> flume::Receiver<TestConnectResult> {
    let (tx, rx) = flume::bounded(1);
    std::thread::Builder::new()
        .name("rs-ssh-test".into())
        .spawn(move || {
            log::info!(
                "SSH 连接测试: {}@{}:{}",
                profile.username,
                profile.host,
                profile.port
            );
            let timeout = timeout.clamp(
                Duration::from_secs(CONNECT_TIMEOUT_MIN_SECS),
                Duration::from_secs(CONNECT_TIMEOUT_MAX_SECS),
            );
            // `map` 的闭包把 handle 吃掉 —— 测试不需要这条连接活过本语句。
            let res = runtime()
                .block_on(connect_only(
                    &profile,
                    timeout,
                    HostKeyPolicy::AskUser,
                    global_proxy.as_ref(),
                ))
                .map(|_handle| {
                    crate::i18n::ts(
                        format!(
                            "连接成功：{}@{}:{}",
                            profile.username, profile.host, profile.port
                        ),
                        format!(
                            "Connected: {}@{}:{}",
                            profile.username, profile.host, profile.port
                        ),
                    )
                });
            match &res {
                Ok(msg) => log::info!("SSH 连接测试通过: {msg}"),
                Err(e) => log::warn!("SSH 连接测试失败: {e}"),
            }
            let _ = tx.send(res);
        })
        .expect("spawn ssh test thread");
    rx
}

/// 连接超时的可配置边界（秒），与 Settings 面板的校验保持一致
pub const CONNECT_TIMEOUT_MIN_SECS: u64 = 5;
pub const CONNECT_TIMEOUT_MAX_SECS: u64 = 300;
/// 信任等待场景下允许的总时长上限（120s 决策 + 握手余量）
const CONNECT_TOTAL_CAP: Duration = Duration::from_secs(180);
/// 认证 / 信道建立阶段的单步超时（上限；实际取 min(配置超时, 本值)）
const STEP_TIMEOUT: Duration = Duration::from_secs(15);

/// SSH 泵感知「会话已结束」（C-2）的轮询周期。
///
/// 泵的 `select!` 平时被 `in_rx` / `read_half` 唤醒，但会话结束（用户关 Tab、
/// 断连后不再重连）时这两路可能都不来消息 —— 需要一个定时分支兜底查看退出旗标。
/// 100ms 是「退出延迟」与「空转成本」的折中：最坏情况迟 100ms 退，而 tokio 的
/// 定时器是时间轮，空闲时几乎不耗 CPU。
const PUMP_EXIT_POLL: Duration = Duration::from_millis(100);

/// 握手 + 认证：拿到一个可用的 SSH 连接（**不含** PTY / shell / exec 信道）。
///
/// 抽出来是为了让两条路径共用同一套超时、known_hosts 校验与认证逻辑：
/// - [`establish`]：交互式会话（PTY + shell）；
/// - [`crate::exec::exec_on`]：一次性命令（exec 信道，无 PTY）。
///
/// 两处各写一遍的代价是「改了一处的超时、忘了另一处」，而这类漂移的后果
/// 正是历史上踩过的「半死服务端把线程永久挂起」（见 `docs/overview.md` 2026-09-09）。
///
/// 返回的 `Handle` 是 `pub` 的：exec 模块需要在它上面开 exec 信道。
///
/// `global_proxy`：全局代理（运行期形态，口令已解密）。**传全局值、由本函数合成**，
/// 而不是让调用点算好「本条连接走什么」：这条链上有四个调用点（会话 / 重连 /
/// 连接测试 / 任务与 agent 后端），每个都记得解析一次的话，漏一个就是
/// 「会话走代理、定时任务直连」这类最难发现的半生效。
pub async fn connect_only(
    profile: &ConnectionProfile,
    timeout: Duration,
    policy: HostKeyPolicy,
    global_proxy: Option<&ProxyConfig>,
) -> Result<russh::client::Handle<ServerKeyVerifier>, String> {
    let addr = (profile.host.as_str(), profile.port);
    // 单步（认证/信道）超时跟随配置但不超过默认上限，避免把「连接超时」调大后
    // 某个卡死的中间步骤也一起变得漫长。
    let step_timeout = timeout.min(STEP_TIMEOUT);

    // 三态合成（唯一一处）：跟随全局 / 明确直连 / 本条连接自带。
    let proxy = crate::proxy::resolve(global_proxy, &profile.proxy).cloned();

    // 握手兜底超时：服务端口「半死」（接受 TCP 但不回应 SSH banner）时，
    // russh 会无限等待，必须显式超时，否则 UI 永远停留在「连接中」。
    let trust_pending = Arc::new(AtomicBool::new(false));
    let phase = Arc::new(AtomicU8::new(Phase::PreStream as u8));
    // 校验器拒绝握手时会往这里写可读原因；russh 只回一句 `Unknown server key`，
    // 分不出「密钥变了」和「没见过这台主机」—— 那个区分在安全上很要紧。
    let reject: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let mut connect_fut = Box::pin(open_transport(
        addr,
        proxy.as_ref(),
        &phase,
        ServerKeyVerifier {
            host: profile.host.clone(),
            port: profile.port,
            trust_pending: trust_pending.clone(),
            policy,
            reject: reject.clone(),
        },
    ));
    let start = std::time::Instant::now();
    let mut deadline = timeout;

    let handle = loop {
        let wait = deadline.saturating_sub(start.elapsed());
        if wait.is_zero() {
            if trust_pending.load(AtomicOrdering::SeqCst) && start.elapsed() < CONNECT_TOTAL_CAP {
                // 用户还在看信任对话框：顺延后继续等
                deadline = start.elapsed() + TRUST_TIMEOUT + Duration::from_secs(10);
                continue;
            }
            break Err(handshake_timeout_msg(
                profile,
                deadline,
                policy,
                Phase::of(&phase),
                proxy.as_ref(),
            ));
        }
        tokio::select! {
            res = connect_fut.as_mut() => {
                // `open_transport` 已把底层错误串成可读的整句，这里只做
                // 「无人值守」那一层增强（与改造前 `with_context` 的位置等价）。
                break res.map_err(|raw| {
                    let reason = reject.lock().ok().and_then(|g| g.clone());
                    exec_friendly_err(profile, raw, policy, reason.as_deref())
                });
            }
            _ = tokio::time::sleep(wait) => {
                if trust_pending.load(AtomicOrdering::SeqCst) && start.elapsed() < CONNECT_TOTAL_CAP {
                    deadline = start.elapsed() + TRUST_TIMEOUT + Duration::from_secs(10);
                    continue;
                }
                break Err(handshake_timeout_msg(
                    profile,
                    deadline,
                    policy,
                    Phase::of(&phase),
                    proxy.as_ref(),
                ));
            }
        }
    };

    let mut handle = handle?;

    // 认证：密码优先，其次私钥（各步带超时，防止服务端半死拖挂）
    let authed = if let Some(pass) = &profile.password {
        match tokio::time::timeout(
            step_timeout,
            handle.authenticate_password(&profile.username, pass),
        )
        .await
        {
            Ok(r) => r.map_err(|e| format!("认证错误: {e}"))?,
            Err(_) => {
                return Err(format!(
                    "认证超时（{}s）：服务器无响应",
                    step_timeout.as_secs()
                ))
            }
        }
    } else if let Some(key_path) = &profile.key_path {
        let key = russh::keys::load_secret_key(key_path, None)
            .map_err(|e| format!("加载私钥 {key_path} 失败: {e}"))?;
        match tokio::time::timeout(
            step_timeout,
            handle.authenticate_publickey(
                &profile.username,
                russh::keys::PrivateKeyWithHashAlg::new(Arc::new(key), None),
            ),
        )
        .await
        {
            Ok(r) => r.map_err(|e| format!("认证错误: {e}"))?,
            Err(_) => {
                return Err(format!(
                    "认证超时（{}s）：服务器无响应",
                    step_timeout.as_secs()
                ))
            }
        }
    } else {
        return Err("未提供密码或私钥".into());
    };
    if !authed.success() {
        return Err(format!("{}@{} 认证失败", profile.username, profile.host));
    }

    Ok(handle)
}

/// 建连相位（只为超时文案分流）。
///
/// 「代理没回应」与「SSH 服务端没回应」在用户眼里都是「连不上」，但下一步动作
/// 完全不同（去查代理 vs 去查 sshd），所以相位必须能被超时文案读到。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// 还没拿到与目标主机的流：可能卡在与代理握手，也可能是直连的 TCP 建连
    PreStream = 0,
    /// 流已就绪，russh 正在做 SSH 握手
    SshHandshake = 1,
}

impl Phase {
    fn of(flag: &AtomicU8) -> Phase {
        if flag.load(AtomicOrdering::SeqCst) == Phase::SshHandshake as u8 {
            Phase::SshHandshake
        } else {
            Phase::PreStream
        }
    }
}

/// 建流 + SSH 握手（两条路产出同一个 `Handle`）。
///
/// 无代理：交给 `russh::client::connect`（它自己连 TCP），与改造前逐字一致。
/// 有代理：先跟代理握手拿到一条流，再交给 `russh::client::connect_stream` ——
/// russh 的 `connect` 就是「TcpStream::connect 之后调 connect_stream」，
/// 所以这里替换掉的只有「流从哪来」这一段。
///
/// 相位在**拿到流之后**翻到 `SshHandshake`：代理已通、SSH 没通，与代理本身不通
/// 是两种故障，不能在超时文案里混为一谈。整个函数跑在 `connect_only` 的 deadline
/// 里，不另设内层超时（两边同时到点会让文案不确定）。
async fn open_transport(
    addr: (&str, u16),
    proxy: Option<&ProxyConfig>,
    phase: &AtomicU8,
    handler: ServerKeyVerifier,
) -> Result<russh::client::Handle<ServerKeyVerifier>, String> {
    let (host, port) = addr;
    let Some(proxy) = proxy else {
        return russh::client::connect(ssh_config(), addr, handler)
            .await
            .with_context(|| format!("连接 {host}:{port} 失败"))
            .map_err(|e| format!("{e:#}"));
    };
    // 代理阶段的具体失败（连不上 / 407 / REP≠0）在 `open_tunnel` 里就带着
    // 代理地址报回来，不会被说成「SSH 握手失败」，所以这里不再包一层。
    let stream = crate::proxy::open_tunnel(proxy, host, port).await?;
    phase.store(Phase::SshHandshake as u8, AtomicOrdering::SeqCst);
    russh::client::connect_stream(ssh_config(), stream, handler)
        .await
        .with_context(|| format!("连接 {host}:{port} 失败"))
        .map_err(|e| format!("{e:#}"))
}

/// 握手超时的可读原因。无人值守时额外说明「可能是主机密钥未信任」——
/// 那种情况下服务端其实回话了，但被我们主动断开，用户看到的却是「超时」。
///
/// 相位在代理上时**换一句**：`proxy` 非空说明这条连接本该经代理，
/// 此时「服务器未完成 SSH 握手」是把用户往错的方向指。
fn handshake_timeout_msg(
    profile: &ConnectionProfile,
    deadline: Duration,
    policy: HostKeyPolicy,
    phase: Phase,
    proxy: Option<&ProxyConfig>,
) -> String {
    if let (Phase::PreStream, Some(proxy)) = (phase, proxy) {
        return crate::i18n::ts(
            format!(
                "连接代理 {} 超时（{} 秒）：代理未响应。请确认代理地址与端口正确、且代理进程正在运行",
                proxy.address(),
                deadline.as_secs()
            ),
            format!(
                "Timed out reaching proxy {} ({}s): the proxy did not respond",
                proxy.address(),
                deadline.as_secs()
            ),
        );
    }
    let base = format!(
        "连接 {}:{} 超时（{} 秒）：服务器未完成 SSH 握手。请确认该端口上运行的是可用的 SSH 服务",
        profile.host,
        profile.port,
        deadline.as_secs()
    );
    match policy {
        HostKeyPolicy::AskUser => base,
        HostKeyPolicy::RejectUnknown => {
            format!("{base}（若这是首次连接该主机，请先在连接面板手动连接一次以完成主机密钥信任）")
        }
    }
}

/// russh 对「服务器主机密钥被 handler 拒绝」的**固定文本**（`Error::UnknownKey`）。
///
/// 判据必须钉死在这句上。改造前用的是 `raw.contains("key")` —— 那会把
/// 「加载私钥失败」「认证 key 无效」之类全都说成主机密钥问题，把用户指向完全
/// 错误的排查方向（甚至让人以为要动 known_hosts）。
const HOST_KEY_REJECT_RAW: &str = "Unknown server key";

/// 连接错误文案的主机密钥增强：把「密钥未知 / 密钥变更 / 用户取消」说清楚，
/// 而不是让用户对着 `Unknown server key` 猜。
///
/// `reject_reason` 是校验器在拒绝那一刻写下的原因（见 [`ServerKeyVerifier::reject`]）：
/// 有这个就优先用它，因为只有校验器知道到底是「变了」还是「没见过」。
fn exec_friendly_err(
    profile: &ConnectionProfile,
    raw: String,
    policy: HostKeyPolicy,
    reject_reason: Option<&str>,
) -> String {
    match host_key_error_text(&profile.host, profile.port, &raw, policy, reject_reason) {
        Some(msg) => msg,
        None => raw,
    }
}

/// [`exec_friendly_err`] 的**纯函数内核**（不依赖 `ConnectionProfile`，可单测）。
///
/// 返回 `None` = 这条错误与主机密钥无关，调用方原样透出。
fn host_key_error_text(
    host: &str,
    port: u16,
    raw: &str,
    policy: HostKeyPolicy,
    reject_reason: Option<&str>,
) -> Option<String> {
    if !raw.contains(HOST_KEY_REJECT_RAW) {
        return None;
    }
    if let Some(reason) = reject_reason {
        return Some(format!("{reason}（底层原因：{raw}）"));
    }
    Some(match policy {
        // 握手被别的路径掐断（超时、代理断开）时不该硬套主机密钥文案
        HostKeyPolicy::AskUser => format!(
            "连接 {host}:{port} 失败：服务器主机密钥未通过校验，请重试（原因：{raw}）"
        ),
        HostKeyPolicy::RejectUnknown => format!(
            "{HOST_KEY_UNKNOWN_MARK}：{host}:{port}。无人值守场景不会自动信任未知主机，请先手动连接一次完成信任（原因：{raw}）"
        ),
    })
}

/// 输出泵对**一条**信道消息的处置。
#[derive(Debug, PartialEq, Eq)]
enum PumpStep {
    /// 有内容要转发到终端（stdout 与 stderr 都走这里）
    Forward(Vec<u8>),
    /// 控制类消息（ExitStatus / Eof / WindowAdjusted / …）：忽略，继续等
    Ignore,
    /// 结束泵
    Stop,
}

/// 消息 → 处置（**纯函数，可单测**）。
///
/// # 为什么把它从 `match` 里提出来
///
/// 这条链路上最危险的那一支（`None`）的正确处置**在单测里根本跑不出来**：
/// 要等信道真的被关闭才发生；而错了也不 panic、不打日志、进程照常工作，
/// 只是**静默烧掉一条 CPU 核**。2026-09-20 就是这么烧起来的 ——
/// 两条 tokio worker 各 `Running` @100%，进程合计 273% 单核（16 核 → 17.1%），
/// 同时 UI 线程 4~8s 无心跳、窗口掉到 10fps。
///
/// 提成纯函数后，「`None` ⇒ **必须 Stop**」从「藏在 match 里的隐式约定」
/// 变成一条可执行、会红、能精确报错的契约
/// （见 `ssh_output_pump_stops_when_the_channel_closes`）。
///
/// 机理：russh 0.63 的 `ChannelReadHalf::wait()` 底下是
/// `tokio::sync::mpsc::Receiver<ChannelMsg>`，其文档明写
/// *"returns `None` if the channel has been closed"*，而 `mpsc::recv()`
/// 在通道关闭后**每次调用都立刻返回 `None`**。所以这一支一旦不停，
/// 调用方的 `loop` 里就再也没有任何 await 点 —— 变成**没有挂起点的紧循环**。
///
/// 严重性来自运行时的容量：全局运行时此前只有 `worker_threads(2)`（见 [`runtime`]；
/// 2026-09-28 已改为 `max(4, 核数/2)`，但**紧循环本身仍是错的**，任何容量下都该停）。
/// 最坏情况下**两个会话被关掉或掉线，两条 worker 就双双钉死**，此后该运行时上的一切
/// SSH I/O（shell 输出 / resize / 远端指标 / SFTP）一起挨饿。
fn pump_step(msg: Option<&russh::ChannelMsg>) -> PumpStep {
    match msg {
        // ★ 信道已关闭。**绝不要把它并进 `Some(_)` 或通配 `_` 分支。**
        None => PumpStep::Stop,
        Some(russh::ChannelMsg::Data { data }) => PumpStep::Forward(data.to_vec()),
        // stderr 必须一起送到终端，否则远端报错在界面上凭空消失。
        Some(russh::ChannelMsg::ExtendedData { data, .. }) => PumpStep::Forward(data.to_vec()),
        // 收到 Close 才是真正的结束；ExitStatus / Eof 之后还可能跟一条 Close，
        // 早停会丢掉收尾输出（例如某些 shell 的退出提示）。
        Some(russh::ChannelMsg::ExitStatus { .. }) => PumpStep::Ignore,
        Some(russh::ChannelMsg::Eof) => PumpStep::Ignore,
        Some(russh::ChannelMsg::Close) => PumpStep::Stop,
        // `ChannelMsg` 是 `#[non_exhaustive]`，将来新增的变体默认忽略。
        Some(_) => PumpStep::Ignore,
    }
}

/// SSH 泵「该不该退出」的判据（C-2，2026-09-28）。
///
/// 两条泵的退出条件有两类来源，这里合成一处、可单测：
/// - **信道层**：`in_rx` 关闭 / 输出信道发不出去 / `pump_step` 给出 `Stop`；
/// - **会话层**：`SessionHandle::drop` 置位的 [`crate::session::ExitFlag`]。
///
/// 第二类是本轮补的。原先两只泵只看信道层，而信道层的 `in_tx` 由
/// `SessionHandle.writer` 持有 —— 那个 clone 会随 UI 侧（终端视图、断连现场的
/// reattach）活过会话本身。结果是：会话早就结束了，输入泵却还在
/// `select!` 上等一个永远不关的信道，白占一条 worker；叠加 worker 数偏少（C-1），
/// 几条僵尸泵就足以让整个 SSH 子系统饥饿。
///
/// 参数取 `exit_raised: bool` 而不是 `&ExitFlag`，是为了让判据完全无副作用、
/// 可在普通单测里直接调用（不必起 tokio 运行时）。
pub(crate) fn pump_should_stop(exit_raised: bool) -> bool {
    exit_raised
}

async fn establish(
    profile: &ConnectionProfile,
    timeout: Duration,
    log_path: Option<std::path::PathBuf>,
    sftp_max_concurrent: u32,
    global_proxy: Option<&ProxyConfig>,
    channels: SshChannels,
) -> Result<SessionHandle, String> {
    // 交互式会话：未知主机走首次信任对话框
    let handle = connect_only(profile, timeout, HostKeyPolicy::AskUser, global_proxy).await?;
    let step_timeout = timeout.min(STEP_TIMEOUT);

    // 会话日志（F-7）：在**开信道之前**就把文件准备好 —— 越早开始记录，
    // 越不容易漏掉「连上了但 shell 初始化就出错」这一段最需要留档的输出。
    // 建不出来会降级为 None（只 warn），不阻断连接。
    let log = crate::session_log::open_if(log_path.as_deref());

    // ★ 单通道 SFTP（外部 `sftp://` 链接）：整条连接只开 sftp 子系统一条通道。
    //
    // 堡垒机（JumpServer 等）常按「每条连接允许几条通道」限流，多开一条 shell
    // 就整条被拒。这里连数据管道都不建 —— 没有终端，也就没有可画的 PTY。
    if channels == SshChannels::SftpOnly {
        let mut session = SessionHandle::pending(
            crate::connections::uuid_like(),
            SessionKind::Ssh,
            profile.name.clone(),
            Some(profile.id.clone()),
        );
        session.state = crate::session::SessionState::Connected;
        session.log = log;
        session.sftp_tx = Some(crate::sftp::spawn_sftp_service(
            std::sync::Arc::new(handle),
            sftp_max_concurrent,
        ));
        return Ok(session);
    }

    // 打开 shell 信道
    let channel = match tokio::time::timeout(step_timeout, handle.channel_open_session()).await {
        Ok(r) => r.map_err(|e| format!("打开信道失败: {e}"))?,
        Err(_) => {
            return Err(format!(
                "打开信道超时（{}s）：服务器无响应",
                step_timeout.as_secs()
            ))
        }
    };
    match tokio::time::timeout(
        step_timeout,
        channel.request_pty(true, "xterm-256color", 80, 24, 0, 0, &[]),
    )
    .await
    {
        Ok(r) => r.map_err(|e| format!("请求 PTY 失败: {e}"))?,
        Err(_) => return Err(format!("请求 PTY 超时（{}s）", step_timeout.as_secs())),
    }
    match tokio::time::timeout(step_timeout, channel.request_shell(true)).await {
        Ok(r) => r.map_err(|e| format!("请求 shell 失败: {e}"))?,
        Err(_) => return Err(format!("请求 shell 超时（{}s）", step_timeout.as_secs())),
    }

    let (mut read_half, write_half) = channel.split();

    // 控制通道：resize。
    //
    // ★ 不再用有界 `flume::Sender::send` 直连 UI 线程 —— 那是「拖拽窗口/分屏时整窗冻结」
    //   的根因：拖拽时每帧一次 resize，而消费端每次都要 await 一个 SSH
    //   `window_change` 网络往返；队列一旦积满，UI 线程上的 `send` 就会阻塞。
    //   现在 UI 线程只往合并槽写「最新尺寸」（[`SshResizeCoalescer`]），
    //   唤醒信号容量 1 且只做通知，真正的发送留在这个 tokio 任务里。
    let (resize_coalescer, resize_wake) = SshResizeCoalescer::new();
    // 数据通道桥：russh 异步半 与 同步 Pipe
    let (in_tx, in_rx) = flume::bounded::<Vec<u8>>(1024);
    let (out_tx, out_rx) = flume::bounded::<Vec<u8>>(1024);

    // ★ C-2（2026-09-28）：泵的退出旗标**在这里先建**，再交给下面两个 tokio 泵
    //   和 `build_session_pipes` 共用同一份。原先两个泵是裸 `tokio::spawn`，退出
    //   条件只有「`in_rx` 关闭 / `out_tx` 发送失败」，而 `in_tx` 由
    //   `SessionHandle.writer` 持有 —— 那个 clone 会随 UI 侧（终端视图、
    //   断连现场 reattach）活过会话本身。会话死了输入泵却还活着空转，白占一条
    //   worker；叠加 C-1（worker 少）就是「整个 SSH 子系统停摆」的加速器。
    //   把旗标接到 `select!` 上，`SessionHandle::drop` 一置位泵就退。
    let pump_exit = crate::session::ExitFlag::default();

    // 输入泵（tokio）：Pipe receiver 到 write_half
    let resize_for_pump = resize_coalescer.clone();
    let in_exit = pump_exit.clone();
    tokio::spawn(async move {
        loop {
            // 每轮先查一次：会话已结束时立刻退，不再等 `select!` 的分支被唤醒。
            if pump_should_stop(in_exit.raised()) {
                break;
            }
            tokio::select! {
                // 兜底唤醒：`in_rx` / `resize_wake` 都静默时也要能察觉 exit 置位。
                // 100ms 只是「最坏情况下的退出延迟」，不产生可感知的 CPU 开销
                // （现代 tokio 定时器是时间轮，空转成本可忽略）。
                _ = tokio::time::sleep(PUMP_EXIT_POLL) => {
                    if pump_should_stop(in_exit.raised()) {
                        break;
                    }
                }
                r = in_rx.recv_async() => {
                    match r {
                        Ok(data) => {
                            if write_half.data_bytes(data).await.is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
                r = resize_wake.recv_async() => {
                    if r.is_err() {
                        break;
                    }
                    // 取走「最新者胜」的尺寸：中间被覆盖的值不必逐个发送，
                    // 远端只关心最终窗口大小。
                    if let Some((cols, rows)) = resize_for_pump.take() {
                        let _ = write_half.window_change(cols, rows, 0, 0).await;
                    }
                }
            }
        }
    });

    // 输出泵（tokio）：read_half 到 Pipe sender
    //
    // 消息的处置全部收在 [`pump_step`] 里（那是一个可单测的纯函数）。这里只负责
    // 把三种处置落到副作用上 —— 尤其是 `Stop` **必须真的 break**：
    // 它既要结束这个任务，又要落下去关掉 `out_tx`（终端侧由此收到 Closed 事件）。
    //
    // 同 C-2：`read_half.wait()` 在服务端半死时可能长时间不返回，也挂上 exit 旗标
    // （它本来是抗「无挂起点紧循环」的，但 `wait()` 本身卡住是另一回事）。
    let out_exit = pump_exit.clone();
    tokio::spawn(async move {
        loop {
            if pump_should_stop(out_exit.raised()) {
                break;
            }
            tokio::select! {
                _ = tokio::time::sleep(PUMP_EXIT_POLL) => {
                    if pump_should_stop(out_exit.raised()) {
                        break;
                    }
                }
                msg = read_half.wait() => {
                    match pump_step(msg.as_ref()) {
                        PumpStep::Forward(data) => {
                            // 发送失败 = 终端侧已销毁，泵没有继续存在的理由。
                            if out_tx.send(data).is_err() {
                                break;
                            }
                        }
                        PumpStep::Ignore => {}
                        PumpStep::Stop => break,
                    }
                }
            }
        }
        drop(out_tx); // 关闭 到 输出泵线程发 Closed 事件
    });

    let session = build_session_pipes(PipesSpec {
        id: crate::connections::uuid_like(),
        kind: SessionKind::Ssh,
        title: profile.name.clone(),
        profile_id: Some(profile.id.clone()),
        resize: ResizeSink::Ssh(resize_coalescer),
        out_source: OutSource::Chan(out_rx),
        in_sink: InSink::Chan(in_tx),
        _child: None,
        child_killer: None,
        log,
        // 与上面两个 tokio 泵共用同一份退出旗标：`SessionHandle::drop` 一置位，
        // 三个泵一起收。
        exit: Some(pump_exit),
    })
    .map_err(|e| e.to_string())?;

    // 远端指标采样（远端 SSH 轮询）。Handle 由常驻任务持有，
    // SessionHandle 退出标志置位后任务结束 到 Handle 释放 到 连接关闭。
    // Handle 非 Clone：包一层 Arc 共享给指标与 SFTP 两个常驻任务（方法均为 &self）
    let handle = Arc::new(handle);
    let mut session = session;
    if channels == SshChannels::Full {
        let metrics = Arc::new(MetricsStore::new());
        spawn_remote_metrics(
            handle.clone(),
            metrics.clone(),
            session.exit.clone(),
            profile.host.clone(),
        );
        session.metrics = Some(metrics);
        // 端口转发（F-3）：规则随连接建立自动启动。**先于 SFTP** 起 ——
        // 两者都要 `handle`，而 SFTP 服务把 handle 吃掉了，转发只能排在它前面。
        session.forwards =
            crate::forward::spawn_forwards(handle.clone(), &profile.forwards, session.exit.clone());
        if !session.forwards.is_empty() {
            log::info!("已启动 {} 条端口转发规则", session.forwards.len());
        }
    }
    if channels == SshChannels::ShellOnly {
        // 单通道（外部 `ssh://`）：既没有指标任务也没有 SFTP 服务持有 Handle，
        // 而 Handle 就是连接本身 —— 这里必须留一个守夜的，否则函数一返回
        // 连接就被关掉，终端刚连上就断。
        let exit = session.exit.clone();
        runtime().spawn(async move {
            let _keep = handle;
            while !exit.raised() {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
    } else {
        // SFTP 常驻服务：复用一条 sftp 子系统信道（失败才重开），命令在其上多路复用。
        session.sftp_tx = Some(crate::sftp::spawn_sftp_service(handle, sftp_max_concurrent));
    }
    Ok(session)
}

/// 远端指标常驻任务：每 5s 打开独立 exec 信道拉取 /proc 快照（远端 SSH 轮询）
fn spawn_remote_metrics<H: russh::client::Handler + 'static>(
    handle: std::sync::Arc<russh::client::Handle<H>>,
    store: Arc<MetricsStore>,
    exit: crate::session::ExitFlag,
    host: String,
) {
    runtime().spawn(async move {
        let mut prev = ProcSnapshot::default();
        loop {
            if exit.raised() {
                break;
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
            if exit.raised() {
                break;
            }

            let probe = async {
                let ch = handle.channel_open_session().await.ok()?;
                ch.exec(true, PROC_PROBE_CMD).await.ok()?;
                let (mut read_half, _write) = ch.split();
                let mut text = String::new();
                while let Some(msg) = read_half.wait().await {
                    match msg {
                        russh::ChannelMsg::Data { data } => {
                            text.push_str(&String::from_utf8_lossy(&data))
                        }
                        russh::ChannelMsg::Close => break,
                        _ => {}
                    }
                }
                Some(text)
            };

            match probe.await {
                Some(text) => {
                    if let Some(sample) = parse_proc_snapshot(&text, &mut prev) {
                        store.push(sample);
                    }
                    // 概况段（主机名 / IP / 运行时长 / df）与走势段同一次 exec 拉回，
                    // 解析失败只影响概况：走势照旧显示，不给「半张空卡」。
                    let overview = parse_system_overview(&text);
                    if !overview.is_empty() {
                        store.set_overview(overview);
                    }
                }
                None => {
                    log::debug!("remote metrics probe failed on {host}");
                    break;
                }
            }
        }
    });
}

/// 远端探测命令（Linux 服务器；一次 exec 拉全量）。
/// 各段以 `__X__` 标记分隔，解析侧按标记切分：新增段不会影响既有段的位置。
///
/// 磁盘取两遍：`__DF__` 的 `df -P -k` 给**精确字节**（合计、排序、阈值判断
/// 都要它），`__DFH__` 的 `df -h -P` 给**原样展示文本**（见 [`parse_df_h`]）。
/// `df` 本身的耗时是毫秒级，多跑一遍远比重算/反解数字划算。
///
/// `__LOADAVG__` 专供巡检的「负载/核」判定（见 `runner::inspect`）：
/// `/proc/loadavg` 第一列是 1 分钟均值，除以核数才是有意义的相对压力。
///
/// `__DISKSTATS__` 给磁盘读写速率（概览的「磁盘」走势图）。放在**最后**：
/// `df -h` 的输出是最后一段既有内容，把新段接在它后面，任何既有段的切分都不受影响。
pub const PROC_PROBE_CMD: &str =
    "cat /proc/stat /proc/meminfo; echo __NETDEV__; cat /proc/net/dev; \
     echo __HOST__; hostname 2>/dev/null; \
     echo __IP__; hostname -I 2>/dev/null; ip -o -4 addr show 2>/dev/null; \
     echo __UPTIME__; cat /proc/uptime 2>/dev/null; \
     echo __LOADAVG__; cat /proc/loadavg 2>/dev/null; \
     echo __DF__; df -P -k 2>/dev/null; \
     echo __DFH__; df -h -P 2>/dev/null; \
     echo __DISKSTATS__; cat /proc/diskstats 2>/dev/null";

/// 按 `__X__` 标记把探测输出切成段：返回 (段标记, 段正文)。
/// 无标记的行归入上一段；首个标记之前的内容归入空标记段。
fn probe_sections(text: &str) -> Vec<(&str, &str)> {
    let mut out: Vec<(&str, &str)> = Vec::new();
    let mut cur: &str = "";
    let mut start = 0usize;
    let mut pos = 0usize;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim();
        if trimmed.starts_with("__") && trimmed.ends_with("__") && trimmed.len() > 4 {
            out.push((cur, &text[start..pos]));
            cur = trimmed;
            start = pos + line.len();
        }
        pos += line.len();
    }
    out.push((cur, &text[start..]));
    out
}

/// 取指定标记段的正文（无该段时返回空串）
fn section<'a>(sections: &[(&'a str, &'a str)], marker: &str) -> &'a str {
    sections
        .iter()
        .find(|(m, _)| *m == marker)
        .map(|(_, body)| *body)
        .unwrap_or("")
}

/// 解析探测输出里的系统概况（主机名 / IP / 运行时长 / 磁盘用量）
pub fn parse_system_overview(text: &str) -> SystemOverview {
    let sections = probe_sections(text);
    // 字节口径（`-k`）是主数据：合计 / 排序 / 阈值都按它算
    let mut disks = parse_df(section(&sections, "__DF__"));
    // 展示口径（`-h`）按挂载点贴到主数据上：两段来自同一台机器的相邻两次 df，
    // 挂载点必然一致。缺 `df -h` 的机器（BSD 的 `-P` 组合差异等）只是没有原样文本，
    // 渲染会退回按字节格式化，不会「整卡空白」。
    for (mount, cols) in parse_df_h(section(&sections, "__DFH__")) {
        if let Some(d) = disks.iter_mut().find(|d| d.mount == mount) {
            d.df = Some(cols);
        }
    }
    SystemOverview {
        hostname: section(&sections, "__HOST__").trim().to_string(),
        ip: parse_ip(section(&sections, "__IP__")),
        uptime_secs: parse_uptime(section(&sections, "__UPTIME__")),
        disks,
        timestamp: Sample::now(),
    }
}

/// 运行时长：`/proc/uptime` 首字段为秒（可带小数）
fn parse_uptime(text: &str) -> u64 {
    text.split_whitespace()
        .next()
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|v| *v >= 0.0)
        .map(|v| v as u64)
        .unwrap_or(0)
}

/// 系统 IP：`hostname -I` 给裸地址，`ip -o -4 addr show` 给 `inet 1.2.3.4/24`，
/// 两种输出混在同一段里，这里按「第一个可用 IPv4 记号」取，不依赖命令是否成功。
fn parse_ip(text: &str) -> String {
    text.split_whitespace()
        .map(|tok| tok.split('/').next().unwrap_or(tok))
        .filter_map(|tok| tok.parse::<std::net::Ipv4Addr>().ok())
        .find(|v4| !v4.is_loopback() && !v4.is_link_local() && !v4.is_unspecified())
        .map(|v4| v4.to_string())
        .unwrap_or_default()
}

/// 伪文件系统（不该出现在「存储」卡里：容量没有意义或与真实磁盘重复计数）
const PSEUDO_FS: &[&str] = &[
    "tmpfs",
    "devtmpfs",
    "devpts",
    "proc",
    "sysfs",
    "cgroup",
    "cgroup2",
    "overlay",
    "none",
    "shm",
    "udev",
    "ramfs",
    "squashfs",
    "efivarfs",
    "debugfs",
    "tracefs",
    "securityfs",
    "pstore",
    "bpf",
    "autofs",
    "mqueue",
    "hugetlbfs",
    "configfs",
    "fusectl",
    "binfmt_misc",
    "nsfs",
];

/// 挂载点前缀白名单之外的黑名单：容器/快照/虚拟目录
const PSEUDO_MOUNT_PREFIXES: &[&str] = &[
    "/snap",
    "/sys",
    "/proc",
    "/dev",
    "/run",
    "/var/lib/docker",
    "/var/lib/kubelet",
    "/var/snap",
    "/boot/efi",
];

/// 该 `df` 行是否值得展示
fn is_real_filesystem(device: &str, mount: &str) -> bool {
    if PSEUDO_FS.contains(&device) || device.is_empty() {
        return false;
    }
    if device.starts_with("/dev/loop") || device.starts_with("/dev/sr") {
        return false;
    }
    !PSEUDO_MOUNT_PREFIXES
        .iter()
        .any(|p| mount == *p || mount.starts_with(&format!("{p}/")))
}

/// 解析 `df -P -k`：`Filesystem 1024-blocks Used Available Capacity Mounted on`。
/// 用 `-k`（1024 字节块）而不是 `-h`：`-h` 的数值带单位、会四舍五入，
/// 解析出来只能再猜回去；容量在 Rust 侧统一格式化成紧凑文案。
pub fn parse_df(text: &str) -> Vec<DiskInfo> {
    let mut out = Vec::new();
    for line in text.lines() {
        let cols: Vec<&str> = line.split_whitespace().collect();
        // 表头、截断行、意外输出一律跳过（列数不足 6 就没法定位挂载点）
        if cols.len() < 6 {
            continue;
        }
        let (Ok(total_kb), Ok(used_kb)) = (cols[1].parse::<u64>(), cols[2].parse::<u64>()) else {
            continue;
        };
        // 挂载点可能含空格：从第 6 列起重新拼回
        let mount = cols[5..].join(" ");
        if !is_real_filesystem(cols[0], &mount) || total_kb == 0 {
            continue;
        }
        out.push(DiskInfo {
            name: cols[0].to_string(),
            mount,
            total: total_kb.saturating_mul(1024),
            used: used_kb.saturating_mul(1024),
            // Available 列原样留字节：ext4 保留块会让 total-used 比 df 的 Avail 大
            avail: cols[3].parse::<u64>().unwrap_or(0).saturating_mul(1024),
            // 展示列由同一段探测里的 `df -h` 段补上（见 `parse_system_overview`）
            df: None,
        });
    }
    // 大容量在前：窄侧栏里最该先看到根分区/数据盘
    out.sort_by_key(|d| std::cmp::Reverse(d.total));
    out
}

/// 解析 `df -h -P` 的展示列：`Filesystem Size Used Avail Use% Mounted on`。
///
/// 返回「挂载点 -> 原样四列」，由 [`parse_system_overview`] 贴到 [`parse_df`]
/// 出来的磁盘上。用 `-P`（POSIX 输出）保证「一个文件系统一行」——不带 `-P` 时
/// 长设备名会折行，折行后的续行看起来像新的一行，会把挂载点解析错。
///
/// 这里只取文本、不反解字节：反解要在单位与四舍五入上再猜一次，猜错反而
/// 让界面上的数字和用户手工 `df -h` 对不上（精确字节另有 `-k` 那一遍负责）。
pub fn parse_df_h(text: &str) -> Vec<(String, DfColumns)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let cols: Vec<&str> = line.split_whitespace().collect();
        // 表头、截断行、意外输出一律跳过（列数不足 6 就没法定位挂载点）。
        // 表头守卫不能省：`parse_df` 靠「数字列解析失败」自然滤掉表头，
        // 这里全是文本列，`Filesystem … Mounted on` 恰好 6 列会被当成数据行。
        if cols.len() < 6 || cols[0] == "Filesystem" {
            continue;
        }
        // 挂载点可能含空格：从第 6 列起重新拼回
        let mount = cols[5..].join(" ");
        if !is_real_filesystem(cols[0], &mount) {
            continue;
        }
        out.push((
            mount,
            DfColumns {
                size: cols[1].to_string(),
                used: cols[2].to_string(),
                avail: cols[3].to_string(),
                use_pct: cols[4].to_string(),
            },
        ));
    }
    out
}

/// 解析 /proc/stat + /proc/meminfo + /proc/net/dev + /proc/diskstats 文本为 Sample
/// （需两次采样做差）
pub fn parse_proc_snapshot(text: &str, prev: &mut ProcSnapshot) -> Option<Sample> {
    let mut cpu_total = 0u64;
    let mut cpu_idle = 0u64;
    let mut mem_total_kb = 0u64;
    let mut mem_avail_kb = 0u64;
    let mut swap_total_kb = 0u64;
    let mut swap_free_kb = 0u64;
    let mut rx_bytes = 0u64;
    let mut tx_bytes = 0u64;
    let mut disk_read_bytes = 0u64;
    let mut disk_write_bytes = 0u64;

    // 段切分与概况解析共用同一套标记：走势只读 `__NETDEV__` 之前的系统段与网络段，
    // 后面新增的段（主机名 / df...）不会混进 `/proc/net/dev` 的行解析。
    let sections = probe_sections(text);
    let sys_part = section(&sections, "");
    let net_part = section(&sections, "__NETDEV__");
    let disk_part = section(&sections, "__DISKSTATS__");

    for line in sys_part.lines() {
        if let Some(cpu_rest) = line.strip_prefix("cpu ") {
            let v: Vec<u64> = cpu_rest
                .split_whitespace()
                .filter_map(|x| x.parse().ok())
                .collect();
            if v.len() >= 4 {
                cpu_total = v.iter().sum::<u64>().max(1);
                cpu_idle = v[3] + v.get(4).copied().unwrap_or(0); // idle + iowait
            }
        } else if let Some((k, v)) = line.split_once(':') {
            let val: u64 = v
                .split_whitespace()
                .next()
                .unwrap_or("0")
                .parse()
                .unwrap_or(0);
            match k.trim() {
                "MemTotal" => mem_total_kb = val,
                "MemAvailable" => mem_avail_kb = val,
                "SwapTotal" => swap_total_kb = val,
                "SwapFree" => swap_free_kb = val,
                _ => {}
            }
        }
    }

    for line in net_part.lines() {
        // /proc/net/dev 数据行: " eth0: rx_bytes packets ... tx_bytes ..."
        let Some((iface, rest)) = line.split_once(':') else {
            continue;
        };
        let iface = iface.trim();
        if iface == "lo" || iface.contains('|') || iface == "face" {
            continue;
        }
        let v: Vec<u64> = rest
            .split_whitespace()
            .filter_map(|x| x.parse().ok())
            .collect();
        if v.len() >= 9 {
            rx_bytes += v[0];
            tx_bytes += v[8];
        }
    }

    // /proc/diskstats 数据行（内核 4.18+ 共 20 列）：
    //   major minor name reads_completed reads_merged sectors_read ms_reading
    //   writes_completed writes_merged sectors_written ms_writing ...
    // 下标按 0 起算 ⇒ 名字在 v[2]、读扇区在 v[5]、写扇区在 v[9]。
    // 扇区恒为 512 字节（内核 ABI 定义，不随物理扇区大小变），这里就地换成字节，
    // 与网络段一样在 `ProcSnapshot` 里存字节，差分才在同一把尺子上。
    for line in disk_part.lines() {
        let v: Vec<&str> = line.split_whitespace().collect();
        // 少于一列到 v[9] 就取不到写扇区：老内核（4.18 之前）只有 14 列且没有
        // 写计数，截断行也可能是被信号打断的半个输出。取不到就整行不要。
        if v.len() < 10 || !is_whole_disk(v[2]) {
            continue;
        }
        disk_read_bytes = disk_read_bytes.saturating_add(
            v[5].parse::<u64>()
                .unwrap_or(0)
                .saturating_mul(SECTOR_BYTES),
        );
        disk_write_bytes = disk_write_bytes.saturating_add(
            v[9].parse::<u64>()
                .unwrap_or(0)
                .saturating_mul(SECTOR_BYTES),
        );
    }

    // 两次采样做差：CPU 使用率与网络 / 磁盘速率
    let d_total = cpu_total.saturating_sub(prev.cpu_total);
    let d_idle = cpu_idle.saturating_sub(prev.cpu_idle);
    let cpu_percent = if prev.cpu_total > 0 && d_total > 0 {
        100.0 * (1.0 - d_idle as f32 / d_total as f32)
    } else {
        0.0
    };
    let now = Sample::now();
    let dt = prev.ts.map(|t| now.saturating_sub(t).max(1)).unwrap_or(0);
    let has_prev = prev.ts.is_some() && dt > 0;
    let rate = |cur: u64, before: u64| {
        if has_prev {
            crate::metrics::bytes_delta_to_mb_per_sec(cur.saturating_sub(before), dt as f32)
        } else {
            0.0
        }
    };
    let (net_up, net_down) = (rate(tx_bytes, prev.tx_bytes), rate(rx_bytes, prev.rx_bytes));
    let (disk_read_mb, disk_write_mb) = (
        rate(disk_read_bytes, prev.disk_read_bytes),
        rate(disk_write_bytes, prev.disk_write_bytes),
    );

    *prev = ProcSnapshot {
        cpu_total,
        cpu_idle,
        rx_bytes,
        tx_bytes,
        disk_read_bytes,
        disk_write_bytes,
        ts: Some(now),
    };

    let mem_used = mem_total_kb.saturating_sub(mem_avail_kb);
    Some(Sample {
        cpu_percent: cpu_percent.clamp(0.0, 100.0),
        mem_percent: if mem_total_kb > 0 {
            100.0 * mem_used as f32 / mem_total_kb as f32
        } else {
            0.0
        },
        swap_percent: if swap_total_kb > 0 {
            100.0 * (swap_total_kb - swap_free_kb) as f32 / swap_total_kb as f32
        } else {
            0.0
        },
        // 远端 /proc/meminfo 以 KB 计，统一换算成字节
        mem_used: mem_used.saturating_mul(1024),
        mem_total: mem_total_kb.saturating_mul(1024),
        swap_used: swap_total_kb
            .saturating_sub(swap_free_kb)
            .saturating_mul(1024),
        swap_total: swap_total_kb.saturating_mul(1024),
        net_up,
        net_down,
        disk_read_mb,
        disk_write_mb,
        timestamp: now,
    })
}

/// 内核 ABI 定义的扇区大小：`/proc/diskstats` 的读写字数**恒**以 512 字节为单位，
/// 与磁盘物理扇区（512e/4Kn）无关。误用 4096 会让速率差 8 倍。
const SECTOR_BYTES: u64 = 512;

/// `/proc/diskstats` 的某一行是否代表**整块磁盘**（而不是分区或伪设备）。
///
/// 为什么必须过滤：`sda` 与 `sda1` 在核心里是**同一份 IO 的两处记账** ——
/// 分区的读写完全落在整盘里。全加一遍正好把速率翻倍，而且翻倍后的曲线依然
/// 「长得对」，只有当用户拿 `iostat` 对才看得出来。
///
/// 规则：
/// - 以数字结尾 = 分区（`sda1`、`nvme0n1p2`、`mmcblk0p1`）⇒ 跳过。
///   这一条顺带滤掉了 `loop0` / `ram0` / `dm-0` / `md0` / `sr0` 这些伪设备与
///   堆叠设备 —— 其中 `dm-*`（LVM）的 IO 同样落在底层整盘里，也必须排除。
/// - 例外：NVMe 与 eMMC 的**整盘名本身以数字结尾**（`nvme0n1`、`mmcblk0`），
///   它们的分区反而带 `p`（`nvme0n1p2`）⇒ 单独认出来。
fn is_whole_disk(name: &str) -> bool {
    if is_nvme_whole(name) || is_mmcblk_whole(name) {
        return true;
    }
    !name.ends_with(|c: char| c.is_ascii_digit())
}

/// `nvme0n1`（整盘）与 `nvme0n1p2`（分区）之别：前者是 `<数字>n<数字>`，后者多一段 `p<数字>`
fn is_nvme_whole(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("nvme") else {
        return false;
    };
    let Some((ns, ctrl)) = rest.split_once('n') else {
        return false;
    };
    is_digits(ns) && is_digits(ctrl)
}

/// `mmcblk0`（整盘）与 `mmcblk0p1`（分区）之别
fn is_mmcblk_whole(name: &str) -> bool {
    name.strip_prefix("mmcblk").is_some_and(is_digits)
}

/// 非空且全为 ASCII 数字
fn is_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

#[derive(Debug, Default, Clone, Copy)]
pub struct ProcSnapshot {
    pub cpu_total: u64,
    pub cpu_idle: u64,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    /// 全部整盘读/写字节累计（已从扇区换算）
    pub disk_read_bytes: u64,
    pub disk_write_bytes: u64,
    pub ts: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// C-1 回归（2026-09-28）：SSH 运行时**不得**再退回 2 条 worker。
    ///
    /// 这条断言是纯函数级的，所以不受 `OnceLock` 影响 —— 一旦有人把
    /// `worker_threads(2)` 写回去（那是历史上真实事故的配置），这里立刻红。
    /// 下限 4 是硬线：四类主要 SSH 任务（I/O 泵 / 指标 / SFTP / 转发）必须各有喘息，
    /// 否则一个任务卡住就能把整个 SSH 子系统拖瘫。
    #[test]
    fn ssh_runtime_is_not_pinned_to_two_workers() {
        let workers = ssh_worker_threads();
        assert!(
            workers >= 4,
            "SSH 运行时 worker 数必须 >= 4，实际 {workers} —— 退回 2 会重现 \
             2026-09-20 的「整个 SSH 子系统停摆」（见 `runtime()` 的文档）"
        );
        // 单核/受限环境（`available_parallelism` 失败）也必须落到下限，而不是 0/1
        assert!(
            workers <= 512,
            "worker 数 {workers} 明显异常（核数解析出错？）"
        );
    }

    /// C-2 回归（2026-09-28）：泵必须**能**因会话结束而退出。
    ///
    /// 原先两只 tokio 泵只认信道层信号：输入泵看 `in_rx` 关闭，而 `in_tx`
    /// 由 `SessionHandle.writer` 持有、会随 UI 侧 clone 活过会话本身 ——
    /// 于是会话死了泵还活着，白占 worker。
    #[test]
    fn ssh_pumps_observe_the_session_exit_flag() {
        assert!(
            !pump_should_stop(false),
            "会话还活着时，泵不得自行退出（提前退出 = 终端收不到后续输出）"
        );
        assert!(
            pump_should_stop(true),
            "`ExitFlag` 置位（会话句柄已释放）后，泵必须退出 —— 否则僵尸泵会占满 worker（C-2）"
        );
    }

    /// C-2 的**接线**护栏：`select!` 里必须真的有 exit 那一路。
    ///
    /// 纯判据对了还不够 —— 有人把 `select!` 里那个 sleep 分支删掉，泵就又回到
    /// 「只在信道关闭时才醒」的老样子，而 `pump_should_stop` 的单测照样绿。
    /// 所以这里扫源码，钉住「两条泵都挂了 exit 检查」。
    #[test]
    fn ssh_pumps_are_wired_to_the_exit_flag() {
        // core 侧没有 ui 的 `prod()` 取源器：按本文件既有先例，用
        // `split("#[cfg(test)]")` 切掉测试区即可（针写在这里，不切就自证）。
        let src = include_str!("ssh.rs");
        let code = src.split("#[cfg(test)]").next().unwrap_or(src);

        // 泵的退出旗标必须是 establish 里先建、再传进 PipesSpec 的那一份
        assert!(
            code.contains("let pump_exit = crate::session::ExitFlag::default();"),
            "输入泵的退出旗标没有在 establish 里预建"
        );
        assert!(
            code.contains("exit: Some(pump_exit),"),
            "预建的退出旗标没有传给 `build_session_pipes`：句柄 drop 时置位的就不是泵看的那一份"
        );
        // 两条泵都得查：`pump_should_stop(` 在生产代码里出现 **5** 次 ——
        // 1 次是函数定义，4 次是调用（两条泵 × 「循环开头 + 定时分支」）。
        assert_eq!(
            code.matches("pump_should_stop(").count(),
            5,
            "生产代码里应有 1 处定义 + 4 处调用（两条泵各「循环开头 + 定时分支」）；\
             数量不对说明有泵漏接了退出旗标（C-2 回归）"
        );
        // 兜底唤醒必须存在，否则信道静默时察觉不到 exit
        assert!(
            code.contains("tokio::time::sleep(PUMP_EXIT_POLL)"),
            "缺少定时兜底唤醒：信道静默时泵永远不会察觉 exit 置位"
        );
    }

    /// ★ 行为护栏（2026-09-20 性能修复）：`wait()` 返回 `None`（信道已关闭）时
    /// **必须结束泵**，不能只是「忽略」。
    ///
    /// 这是本轮 CPU 事故的正中靶心。它之所以难防，是因为错了**没有任何症状**：
    /// 不 panic、不打日志、进程照常工作，只是静默烧掉 CPU。而它又必须在信道真的
    /// 关闭之后才发生 —— 于是只能把「消息 → 处置」提成纯函数 [`pump_step`]，
    /// 把这条契约变成可精确断言的东西。
    ///
    /// 机理：russh 0.63 的 `ChannelReadHalf::wait()` 底下是
    /// `tokio::sync::mpsc::Receiver<ChannelMsg>`，其文档明写 *"returns `None` if
    /// the channel has been closed"*，而 `mpsc::recv()` 在通道关闭后**每次调用
    /// 都立刻返回 `None`**。所以这一支只要不停，调用方的 `loop` 里就再也没有
    /// await 点 —— 变成没有挂起点的紧循环，把一条 tokio worker 钉死在 100% 单核。
    ///
    /// 实机指纹（2026-09-20 采样，16 逻辑核）：两条线程各 `Running` @100.9%/99.7%，
    /// 进程合计 273% 单核 = 整机 17.1%（与任务管理器 16% 对上）；同时 UI 线程
    /// 4~8s 无心跳、`poll` 慢帧 1.536s、窗口只有 10fps。
    /// 复现代价极低：连上再关掉两个会话即可（旧配置全局运行时只有
    /// `worker_threads(2)`；2026-09-28 已改为 `max(4, 核数/2)`，见 [`runtime`]）。
    #[test]
    fn ssh_output_pump_stops_when_the_channel_closes() {
        use russh::ChannelMsg;

        assert_eq!(
            pump_step(None),
            PumpStep::Stop,
            "`wait()` 返回 `None`（信道已关闭）时必须结束泵。吞掉它 = 没有 await 点的紧循环 \
             = 一条 tokio worker 被钉在 100%；本仓全局运行时只有 worker_threads(2)，\
             两个会话关掉就烧穿整个 SSH 运行时。"
        );
        assert_eq!(
            pump_step(Some(&ChannelMsg::Close)),
            PumpStep::Stop,
            "收到 Close 必须结束泵"
        );
        // ExitStatus / Eof 之后通常还跟一条 Close：早停会丢掉收尾输出，所以只忽略。
        assert_eq!(
            pump_step(Some(&ChannelMsg::ExitStatus { exit_status: 0 })),
            PumpStep::Ignore
        );
        assert_eq!(pump_step(Some(&ChannelMsg::Eof)), PumpStep::Ignore);
        // stdout 与 stderr 都必须转发（stderr 吞掉 = 远端报错在界面上凭空消失）。
        assert_eq!(
            pump_step(Some(&ChannelMsg::Data {
                data: Default::default()
            })),
            PumpStep::Forward(Vec::new())
        );
        assert_eq!(
            pump_step(Some(&ChannelMsg::ExtendedData {
                data: Default::default(),
                ext: 1
            })),
            PumpStep::Forward(Vec::new())
        );
    }

    /// 接线护栏：上面那条行为用例只证明 `pump_step` 的语义正确，还得证明输出泵
    /// **真的在用它**、且 `Stop` 真的被落成 `break`（漏掉这段，`pump_step` 就只是
    /// 一个没人调用的漂亮函数）。
    ///
    /// 判据按**行**取，不用裸 `contains` —— 裸 `contains("_ =>")` 会被注释里引用的
    /// 代码本身误伤（core 侧没有 ui 的 `prod()` 取源器，这里不剥注释）。
    #[test]
    fn ssh_output_pump_wires_the_stop_step_to_break() {
        let src = include_str!("ssh.rs");
        // 只看生产代码：本测试自己写着这些针，不去掉就等于自证。
        let prod = src.split("#[cfg(test)]").next().unwrap_or(src);
        let at = prod.find("// 输出泵").expect("输出泵必须存在");
        let rest = &prod[at..];
        // 只截到本泵的收尾 `drop(out_tx)`：后面还有别的任务。
        let body = &rest[..rest
            .find("drop(out_tx)")
            .expect("输出泵必须以 drop(out_tx) 收尾")];

        assert!(
            body.contains("pump_step("),
            "SSH 输出泵没有走 `pump_step(..)`：消息处置必须收在那个可单测的纯函数里。"
        );
        assert!(
            body.contains("PumpStep::Stop => break"),
            "SSH 输出泵没有把 `PumpStep::Stop` 落成 `break` —— 泵不会停，\
             信道关闭后就是没有 await 点的紧循环。"
        );
        let offenders: Vec<String> = body
            .lines()
            .enumerate()
            .filter_map(|(i, line)| {
                let t = line.trim_start();
                (t.starts_with("_ =>") || t.starts_with("_=>"))
                    .then(|| format!("第 {} 行：{}", i + 1, t.trim_end()))
            })
            .collect();
        assert!(
            offenders.is_empty(),
            "SSH 输出泵出现了 {} 处通配分支 `_ =>`：`PumpStep` 只有三种处置，通配支会把 \
             `Stop` 一起吞掉（正是上述紧循环的成因）。\n{}",
            offenders.len(),
            offenders.join("\n")
        );
    }

    const SNAPSHOT_1: &str = "cpu  100 0 100 700 0 0 0 0 0 0\n\
        MemTotal: 16000000 kB\n\
        MemAvailable: 6400000 kB\n\
        SwapTotal: 2000000 kB\n\
        SwapFree: 2000000 kB\n\
        __NETDEV__\n\
        Inter-|   Receive                                                |  Transmit\n\
        face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n\
        eth0: 1000000    100    0    0    0     0          0         0  2000000    100    0    0    0     0       0          0\n\
        lo: 999999999 9999    0    0    0     0          0         0  999999999 9999    0    0    0     0       0          0\n";

    const SNAPSHOT_2: &str = "cpu  900 0 100 0 0 0 0 0 0 0\n\
        MemTotal: 16000000 kB\n\
        MemAvailable: 3200000 kB\n\
        SwapTotal: 2000000 kB\n\
        SwapFree: 1000000 kB\n\
        __NETDEV__\n\
        Inter-|   Receive                                                |  Transmit\n\
        face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n\
        eth0: 4000000    100    0    0    0     0          0         0  5000000    100    0    0    0     0       0          0\n\
        lo: 999999999 9999    0    0    0     0          0         0  999999999 9999    0    0    0     0       0          0\n";

    #[test]
    fn parse_proc_snapshot_two_samples() {
        let mut prev = ProcSnapshot::default();
        let s1 = parse_proc_snapshot(SNAPSHOT_1, &mut prev).unwrap();
        assert_eq!(s1.mem_percent, 60.0);
        assert_eq!(s1.swap_percent, 0.0);
        assert_eq!(s1.cpu_percent, 0.0, "首帧无差分基线");
        // 交换分区容量：KB → 字节，与内存同一换算口径
        assert_eq!(s1.swap_total, 2_000_000 * 1024);
        assert_eq!(s1.swap_used, 0);

        // 模拟 1s 间隔
        prev.ts = Some(Sample::now() - 1);
        let s2 = parse_proc_snapshot(SNAPSHOT_2, &mut prev).unwrap();
        assert!(s2.cpu_percent > 99.0, "cpu={}", s2.cpu_percent);
        assert_eq!(s2.mem_percent, 80.0);
        assert_eq!(s2.swap_percent, 50.0);
        assert_eq!(s2.swap_total, 2_000_000 * 1024);
        assert_eq!(s2.swap_used, 1_000_000 * 1024);
        assert_eq!(s2.swap_used_over_total(), "976.6M/1.9G");
        assert_eq!(s2.swap_summary(), "976.6M/1.9G (50.00%)");
        assert!(
            s2.net_up > 0.0 && s2.net_down > 0.0,
            "net={:?}",
            (s2.net_up, s2.net_down)
        );
    }

    /// 远端概况段：主机名 / IP / 运行时长 / df（含伪文件系统过滤 + `df -h` 展示列）
    const SNAPSHOT_FULL: &str = "cpu  100 0 100 700 0 0 0 0 0 0\n\
        MemTotal: 16000000 kB\n\
        MemAvailable: 6400000 kB\n\
        __NETDEV__\n\
        eth0: 1000000    100    0    0    0     0          0         0  2000000    100    0    0    0     0       0          0\n\
        __HOST__\n\
        web-01\n\
        __IP__\n\
        192.168.1.20 10.0.0.5 \n\
        2: eth0    inet 192.168.1.20/24 brd 192.168.1.255 scope global eth0\n\
        1: lo    inet 127.0.0.1/8 scope host lo\n\
        __UPTIME__\n\
        6829.42 24000.10\n\
        __DF__\n\
        Filesystem     1024-blocks    Used Available Capacity Mounted on\n\
        /dev/sda1         51474528 8123456  40723456      17% /\n\
        tmpfs              8134656       0   8134656       0% /dev/shm\n\
        /dev/sdb1       1030812480 500000000 500000000  50% /data\n\
        /dev/loop3          129024  129024         0     100% /snap/core/1\n\
        __DFH__\n\
        Filesystem      Size  Used Avail Use% Mounted on\n\
        /dev/sda1        49G  7.8G   39G  17% /\n\
        tmpfs           7.8G     0  7.8G   0% /dev/shm\n\
        /dev/sdb1       983G  477G  477G  50% /data\n\
        /dev/loop3      126M  126M     0 100% /snap/core/1\n";

    #[test]
    fn parse_system_overview_reads_every_section() {
        let o = parse_system_overview(SNAPSHOT_FULL);
        assert_eq!(o.hostname, "web-01");
        assert_eq!(o.ip, "192.168.1.20", "回环地址不得被当成系统 IP");
        assert_eq!(o.uptime_secs, 6829);
        // tmpfs 与 /snap 下的 loop 设备都要被过滤掉
        assert_eq!(o.disks.len(), 2, "disks={:?}", o.disks);
        // 大容量在前
        assert_eq!(o.disks[0].mount, "/data");
        assert_eq!(o.disks[1].mount, "/");
        // 字节口径仍来自 `df -P -k`（合计 / 排序 / 阈值都按它算）
        assert_eq!(o.disks[1].total, 51_474_528 * 1024);
        assert_eq!(o.disks[1].used, 8_123_456 * 1024);
        // 展示口径来自 `df -h`：容量与占用率都用 df 的原样文本
        assert_eq!(o.disks[1].used_over_total(), "7.8G/49G");
        assert_eq!(o.disks[1].avail_text().as_deref(), Some("39G"));
    }

    /// `df -h` 的 `Use%` 必须盖过 `used / total`。
    ///
    /// df 是按 `used / (used + avail)` 算的；ext4 有 5% 保留块，两个口径会差
    /// 几个百分点（本样例：df 给 17%，`used/total` 只有 15.8%）。用户拿手工
    /// `df -h` 一对就能看出差别，所以有 df 文本时必须优先用 df 的那个数。
    #[test]
    fn df_h_use_percent_wins_over_used_over_total() {
        let o = parse_system_overview(SNAPSHOT_FULL);
        let root = o.disks.iter().find(|d| d.mount == "/").expect("根分区");
        assert_eq!(root.percent(), 17.0);
        // 反证：按字节算只有 15.8%，若实现回退到 used/total，这里会得到 15.8
        let by_bytes = 100.0 * root.used as f64 / root.total as f64;
        assert!(
            (by_bytes - 15.78).abs() < 0.05,
            "样例本身要能体现两个口径的差异，实际 {by_bytes}"
        );
    }

    /// 没有 `__DFH__` 段（老版本探测输出 / BSD 的 `-P` 组合差异）时不得整卡空白：
    /// 退回按字节格式化，且 `df` 字段留空。
    #[test]
    fn parse_overview_without_df_h_falls_back_to_bytes() {
        let text = "__HOST__\nweb-01\n__DF__\n\
                    Filesystem 1024-blocks Used Available Capacity Mounted on\n\
                    /dev/sda1 51474528 8123456 40723456 16% /\n";
        let o = parse_system_overview(text);
        assert_eq!(o.disks.len(), 1);
        assert!(o.disks[0].df.is_none(), "缺 __DFH__ 时不该编造原样列");
        assert_eq!(o.disks[0].used_over_total(), "7.7G/49.1G");
        assert_eq!(o.disks[0].avail_text().as_deref(), Some("38.8G"));
    }

    /// `df -h -P` 的展示列：挂载点含空格时也要能对上（同一台机器两次 df 的
    /// 挂载点写法一致，所以拼回方式必须与 `parse_df` 同口径）
    #[test]
    fn parse_df_h_keeps_mount_points_with_spaces() {
        let cols = parse_df_h(
            "Filesystem Size Used Avail Use% Mounted on\n\
             /dev/sda1 1.0T 500G 500G 50% /mnt/my data\n",
        );
        assert_eq!(cols.len(), 1);
        assert_eq!(cols[0].0, "/mnt/my data");
        assert_eq!(cols[0].1.size, "1.0T");
        assert_eq!(cols[0].1.used, "500G");
        assert_eq!(cols[0].1.avail, "500G");
        assert_eq!(cols[0].1.use_percent(), Some(50.0));
    }

    /// `df -h` 里的伪文件系统同样要过滤（否则 tmpfs 会以「0% 的 7.8G」混进存储卡）
    #[test]
    fn parse_df_h_filters_pseudo_filesystems() {
        let cols = parse_df_h(
            "Filesystem Size Used Avail Use% Mounted on\n\
             tmpfs 7.8G 0 7.8G 0% /dev/shm\n\
             /dev/sda1 49G 7.8G 39G 17% /\n",
        );
        assert_eq!(cols.len(), 1);
        assert_eq!(cols[0].0, "/");
    }

    /// `Use%` 文本非法（本地化输出 / 空列）时不 panic：返回 None，调用方回退按字节算
    #[test]
    fn df_use_percent_tolerates_junk() {
        use crate::metrics::DfColumns;
        let junk = DfColumns {
            use_pct: "-".into(),
            ..Default::default()
        };
        assert_eq!(junk.use_percent(), None);
        let disk = DiskInfo {
            total: 100,
            used: 25,
            df: Some(junk),
            ..Default::default()
        };
        assert_eq!(disk.percent(), 25.0, "解析不出 Use% 时应回退 used/total");
    }

    /// 老的探测输出（只有两段）仍要能解析出走势，概况段缺省为空
    #[test]
    fn parse_proc_snapshot_tolerates_missing_new_sections() {
        let mut prev = ProcSnapshot::default();
        assert!(parse_proc_snapshot(SNAPSHOT_1, &mut prev).is_some());
        assert!(parse_system_overview(SNAPSHOT_1).is_empty());
    }

    /// 两次采样的 `/proc/diskstats`（含分区、loop、dm-0 等**不该被计入**的行）。
    ///
    /// 数字是刻意配的：整盘 `sda` 的增量 = 4096 读扇区 / 2048 写扇区，
    /// 恰好 2 MiB / 1 MiB；而 `sda1`（分区）、`loop0`、`dm-0` 的增量**都不为零**，
    /// 一旦过滤失效，算出来的速率会明显偏大 ⇒ 断言有牙。
    ///
    /// 行首的 `\x20` 是**故意**的：真实 `/proc/diskstats` 的 major/minor 列右对齐、
    /// 前面有空格，行尾的 `\` 续行又会吃掉下一行的缩进，所以用显式转义把那个空格
    /// 留下来 —— 顺带保证实现不能用 `split(' ')` 蒙混过关。
    const SNAPSHOT_DISK_1: &str = "cpu  100 0 100 700 0 0 0 0 0 0\n\
        MemTotal: 16000000 kB\n\
        MemAvailable: 6400000 kB\n\
        __NETDEV__\n\
        eth0: 1000000    100    0    0    0     0          0         0  2000000    100    0    0    0     0       0          0\n\
        __DISKSTATS__\n\
        \x20  8       0 sda            1000   10   2048   50    500    5   1024   20   0   30   70   0 0 0 0\n\
        \x20  8       1 sda1           1000   10   2048   50    500    5   1024   20   0   30   70   0 0 0 0\n\
        \x20259       0 nvme0n1         200    2      0    5    100    1      0    3   0    8   12   0 0 0 0\n\
        \x20259       1 nvme0n1p1       200    2      0    5    100    1      0    3   0    8   12   0 0 0 0\n\
        \x20  7       0 loop0           500    0   9999    0      0    0      0    0   0    0    0   0 0 0 0\n\
        \x20253       0 dm-0            300    0   8888   10      0    0      0    0   0    0    0   0 0 0 0\n";

    const SNAPSHOT_DISK_2: &str = "cpu  100 0 100 700 0 0 0 0 0 0\n\
        MemTotal: 16000000 kB\n\
        MemAvailable: 6400000 kB\n\
        __NETDEV__\n\
        eth0: 1000000    100    0    0    0     0          0         0  2000000    100    0    0    0     0       0          0\n\
        __DISKSTATS__\n\
        \x20  8       0 sda            3000   10   6144   50   1500    5   3072   20   0   30   70   0 0 0 0\n\
        \x20  8       1 sda1           3000   10   6144   50   1500    5   3072   20   0   30   70   0 0 0 0\n\
        \x20259       0 nvme0n1         200    2      0    5    100    1      0    3   0    8   12   0 0 0 0\n\
        \x20259       1 nvme0n1p1       200    2      0    5    100    1      0    3   0    8   12   0 0 0 0\n\
        \x20  7       0 loop0          1500    0  19999    0      0    0      0    0   0    0    0   0 0 0 0\n\
        \x20253       0 dm-0            300    0  18888   10      0    0      0    0   0    0    0   0 0 0 0\n";

    /// `/proc/diskstats` 只算**整盘**：分区（`sda1`）是整盘 IO 的第二处记账，
    /// 全加一遍会把速率翻倍；`loop*` / `dm-*` 这类伪设备与堆叠设备同理。
    #[test]
    fn parse_proc_snapshot_counts_whole_disks_only() {
        let mut prev = ProcSnapshot::default();
        let s1 = parse_proc_snapshot(SNAPSHOT_DISK_1, &mut prev).unwrap();
        assert_eq!(s1.disk_read_mb, 0.0, "首帧没有差分基线");
        assert_eq!(s1.disk_write_mb, 0.0, "首帧没有差分基线");

        // 模拟 1s 间隔
        prev.ts = Some(Sample::now() - 1);
        let s2 = parse_proc_snapshot(SNAPSHOT_DISK_2, &mut prev).unwrap();
        // sda 增量 4096 读扇区 ×512B = 2 MiB / 1s；2048 写扇区 ×512B = 1 MiB / 1s
        assert_eq!(s2.disk_read_mb, 2.0, "扇区 → 字节 → MB 的换算链断了");
        assert_eq!(s2.disk_write_mb, 1.0);
        // 反证：把 sda1 / loop0 / dm-0 一起算进来的话，读数会明显大于 2.0
        assert!(
            s2.disk_read_mb < 2.5,
            "分区或伪设备被计入了合计，disk_read_mb={}",
            s2.disk_read_mb
        );
    }

    /// 没有 `__DISKSTATS__` 段（老版本探测输出，或内核不提供 /proc/diskstats）时，
    /// 磁盘速率必须是 0 而不是乱数 —— 图上表现为「这段时间没采到」。
    #[test]
    fn parse_proc_snapshot_without_diskstats_reports_zero() {
        let mut prev = ProcSnapshot::default();
        parse_proc_snapshot(SNAPSHOT_1, &mut prev).unwrap();
        prev.ts = Some(Sample::now() - 1);
        let s = parse_proc_snapshot(SNAPSHOT_2, &mut prev).unwrap();
        assert_eq!(s.disk_read_mb, 0.0);
        assert_eq!(s.disk_write_mb, 0.0);
        // 网络段不受影响（新段是**追加**，没有挤掉既有段）
        assert!(s.net_up > 0.0 && s.net_down > 0.0);
    }

    /// 整盘判定：分区、伪设备、LVM 都要排除；NVMe / eMMC 的整盘名本身就带数字，
    /// 不能被「以数字结尾即分区」这条规则误伤。
    #[test]
    fn whole_disk_filter_handles_nvme_and_partitions() {
        for whole in ["sda", "sdb", "vda", "xvda", "hda", "nvme0n1", "mmcblk0"] {
            assert!(is_whole_disk(whole), "{whole} 是整盘");
        }
        for part in [
            "sda1",
            "sda15",
            "nvme0n1p2",
            "mmcblk0p1",
            "loop0",
            "ram0",
            "dm-0",
            "md0",
            "sr0",
            "zram0",
        ] {
            assert!(!is_whole_disk(part), "{part} 不该被当成整盘");
        }
    }

    /// `hostname -I` 不可用时只靠 `ip -o -4 addr show` 也能取到 IP
    #[test]
    fn parse_ip_falls_back_to_ip_addr_output() {
        let text = "__IP__\n1: lo    inet 127.0.0.1/8 scope host lo\n\
                    2: eth0    inet 10.1.2.3/16 brd 10.1.255.255 scope global eth0\n";
        assert_eq!(parse_system_overview(text).ip, "10.1.2.3");
    }

    /// 表头、截断行与非法数值不得被当成文件系统
    #[test]
    fn parse_df_skips_noise() {
        let text = "Filesystem 1024-blocks Used Available Capacity Mounted on\n\
                    /dev/sda1 100 50 50 50% /\n\
                    bad line\n\
                    /dev/sdb1 xx 50 50 50% /mnt\n";
        let disks = parse_df(text);
        assert_eq!(disks.len(), 1);
        assert_eq!(disks[0].mount, "/");
    }

    /// 挂载点含空格（`df -P` 允许）时按第 6 列起拼回
    #[test]
    fn parse_df_keeps_mount_points_with_spaces() {
        let disks = parse_df(
            "Filesystem 1024-blocks Used Available Capacity Mounted on\n\
             /dev/sda1 1024 512 512 50% /mnt/my data\n",
        );
        assert_eq!(disks.len(), 1);
        assert_eq!(disks[0].mount, "/mnt/my data");
    }

    /// 接线护栏（2026-10-21 用户要求「测试连接功能移除无人值守之类的限制」）：
    /// 测试连接必须走 `AskUser`，不能再用无人值守的硬拒。
    ///
    /// 为什么是源码断言而不是端到端：`test_connection_in_background` 的真实路径要
    /// 一台「known_hosts 里没有记录」的 sshd（docker 夹具），而这条契约一旦被改回去
    /// **不会**报错、只会让用户看到「请先手动连接一次」——正是用户报的那个限制。
    #[test]
    fn connection_test_asks_the_user_about_unknown_host_keys() {
        let src = include_str!("ssh.rs");
        let code = src.split("#[cfg(test)]").next().unwrap_or(src);
        let at = code
            .find("pub fn test_connection_in_background(")
            .expect("找不到连接测试入口");
        let body = &code[at..];
        let end = body.find("\n}").expect("找不到连接测试的结尾");
        let body = &body[..end];
        assert!(
            body.contains("HostKeyPolicy::AskUser"),
            "连接测试又退回不询问主机密钥了：人在对话框前点「测试连接」，\
             未知主机应该弹信任框，而不是让他先去手动连一次。"
        );
        assert!(
            !body.contains("HostKeyPolicy::RejectUnknown"),
            "连接测试仍带着无人值守策略 —— 正是用户要求移除的那条限制。"
        );
    }

    // ---- 主机密钥变更（容器重建）相关 --------------------------------------
    //
    // 现场：服务器上的 docker 容器被删了重建，账号口令都没变，直连却报
    // 「Unknown server key」。根因是镜像里的 sshd host key 没挂持久卷，重建后
    // 重新生成 ⇒ known_hosts 里的旧记录与新密钥冲突 ⇒ russh 回
    // `Error::UnknownKey`。改造前这条路径**只写日志、界面无入口**，用户只能去
    // 手改 known_hosts。这些用例钉住「能重新信任」与「旧记录必须被替换」。

    /// russh 自带的两个合法 ed25519 公钥字面量（来自其 known_hosts 单测），
    /// 用作「旧密钥 / 新密钥」。
    const ED_A: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIJdD7y3aLq454yWBdwLWbieU1ebz9/cu7/QEXn9OIeZJ";
    const ED_B: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIA6rWI3G1sz07DnfFlrouTcysQlj2P+jpNSOEWD9OJ3X";

    fn key(b64: &str) -> russh::keys::PublicKey {
        russh::keys::parse_public_key_base64(b64).expect("测试用公钥字面量必须可解析")
    }

    /// 只删「同一主机 + 同一把旧密钥」的行；注释、其它主机、其它密钥一律保留。
    #[test]
    fn without_stale_key_drops_only_the_conflicting_record() {
        let content = format!(
            "# managed by rustshell\n\
             [10.0.0.5]:2222 ssh-ed25519 {ED_A}\n\
             10.0.0.5 ssh-ed25519 {ED_A}\n\
             other.example ssh-ed25519 {ED_A}\n\
             [10.0.0.5]:2222 ssh-ed25519 {ED_B}\n"
        );
        let out = without_stale_key(&content, "[10.0.0.5]:2222", &[key(ED_A)]);

        assert!(
            !out.contains(&format!("[10.0.0.5]:2222 ssh-ed25519 {ED_A}")),
            "冲突行必须被删掉，否则下次连接还会撞上它（用户点多少次「更新」都没用）"
        );
        assert!(out.contains("# managed by rustshell"), "注释行不该被删");
        assert!(
            out.contains(&format!("10.0.0.5 ssh-ed25519 {ED_A}")),
            "同一 IP 但**端口不同**（裸主机名 = 22）是另一条记录，必须保留"
        );
        assert!(
            out.contains(&format!("other.example ssh-ed25519 {ED_A}")),
            "别的主机即便用同一把密钥也必须保留"
        );
        assert!(
            out.contains(&format!("[10.0.0.5]:2222 ssh-ed25519 {ED_B}")),
            "同主机其它密钥（不同算法/不同值）不参与本次冲突，必须保留"
        );
    }

    /// 端到端：写文件 → russh 判定 KeyChanged → `replace_known_host` → russh 判定通过，
    /// 且旧行**不残留**（这是「更新密钥后仍连不上」那个死循环的回归点）。
    #[test]
    fn replace_known_host_makes_the_next_check_pass() {
        let dir = std::env::temp_dir().join(format!("rs-kh-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("known_hosts");
        std::fs::write(&path, format!("[h.example]:2200 ssh-ed25519 {ED_A}\n")).unwrap();

        let (old, new) = (key(ED_A), key(ED_B));
        assert_ne!(old, new, "测试前提：新旧密钥必须不同");
        assert!(
            matches!(
                russh::keys::check_known_hosts_path("h.example", 2200, &new, &path),
                Err(russh::keys::Error::KeyChanged { .. })
            ),
            "前提：新旧密钥不同，russh 必须报 KeyChanged"
        );

        replace_known_host(&path, "h.example", 2200, &new);
        assert!(
            russh::keys::check_known_hosts_path("h.example", 2200, &new, &path).unwrap(),
            "更新后再校验必须通过 —— 否则用户「点了信任」还是连不上"
        );
        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            after.lines().count(),
            1,
            "旧记录必须被**替换**而不是追加并存（否则永远先命中旧行）：{after}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 主机密钥类失败必须能区分「变了」与「没见过」，且不能把无关的 key 错误算进来。
    #[test]
    fn host_key_error_text_distinguishes_changed_from_unknown() {
        let raw = format!("连接 h:22 失败: {HOST_KEY_REJECT_RAW}");
        let changed = format!("{HOST_KEY_CHANGED_MARK}：h:22 的服务器密钥与 known_hosts 记录不一致");
        let got = host_key_error_text("h", 22, &raw, HostKeyPolicy::RejectUnknown, Some(&changed))
            .expect("主机密钥类错误必须被接管");
        assert!(
            got.contains(HOST_KEY_CHANGED_MARK) && got.contains("不一致"),
            "变更类文案要带标记与「不一致」（告警引擎按它判 Critical）：{got}"
        );
        assert!(!got.contains(HOST_KEY_UNKNOWN_MARK), "变更不能说成「未知」：{got}");

        // 有未知标记（无 reject 原因）时走未知分支
        let unknown = host_key_error_text("h", 22, &raw, HostKeyPolicy::RejectUnknown, None)
            .expect("主机密钥类错误必须被接管");
        assert!(unknown.contains(HOST_KEY_UNKNOWN_MARK), "{unknown}");

        // 改造前的 `raw.contains("key")` 会把这些误判成主机密钥问题，指向错误排查方向
        for unrelated in [
            "加载私钥 /tmp/id_ed25519 失败: key is encrypted",
            "认证错误: no key accepted",
            "连接 h:22 失败: Connection refused",
        ] {
            assert!(
                host_key_error_text("h", 22, unrelated, HostKeyPolicy::RejectUnknown, None).is_none(),
                "与主机密钥无关的错误不该被改写：{unrelated}"
            );
        }
    }
}
