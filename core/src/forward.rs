//! 端口转发（F-3）：本地（`ssh -L`）/ 远端（`ssh -R`）/ 动态 SOCKS5（`ssh -D`）/
//! HTTP CONNECT 代理。
//!
//! 四条链路的共同形态都是「一条 TCP 连接 ↔ 一条 SSH 信道，双向搬字节」，
//! 差别只在**谁在监听**与**目标从哪来**：
//!
//! | 类型 | 谁监听 | 目标来源 | 建信道方式 |
//! |------|--------|----------|------------|
//! | Local | 本机 | 规则表（固定） | `channel_open_direct_tcpip` |
//! | Remote | 服务器 | 规则表（固定） | 服务端主动开，经 [`deliver_forwarded`] 投递 |
//! | Dynamic | 本机 | SOCKS5 握手（每连接） | `channel_open_direct_tcpip` |
//! | HttpConnect | 本机 | `CONNECT host:port` 请求行（每连接） | `channel_open_direct_tcpip` |
//!
//! # 生命周期不变量
//!
//! **界面上看得到隧道，就一定还有人在监听。** 两条收摊路径缺一不可：
//! 用户手动停（[`ForwardSlot::stop`]）与会话结束（转发任务自己看 [`ExitFlag`]）。
//! 少了后者，用户不关面板就能让一个本地端口在会话已死之后继续接受连接 —— 那时
//! 连进来的流量会开在一个已断开的 `Handle` 上，失败得又慢又难懂。
//!
//! # 为什么是 [`ForwardBoard`] 而不是「连接时一次性 spawn 完」
//!
//! 转发面板要能**显式**启停单条隧道（保存规则 ≠ 开始监听）。一次性 spawn 的
//! 形态在连接建立那一刻就把规则表烧死了，运行期既加不进也停不掉。板子持有
//! `Arc<Handle>`（非 Clone）与槽位表，UI 拿 `&SessionHandle` 就能启停 —— 不需要
//! 可变借用会话句柄，也不需要把 `Handle` 暴露给 UI。
//!
//! # 为什么槽位只暴露「状态快照」而不是 `&Mutex<ForwardStatus>`
//!
//! 状态是被 SSH 任务写的、UI 每帧读的。若把锁交出去，UI 侧一次不小心的跨帧持锁
//! 就会把转发任务的 `*status.lock() = ..` 堵住 —— 而那个赋值发生在 `accept` 循环里，
//! 堵住它就等于把整条隧道停掉。`status()` 返回 `clone` 出来的值，锁的作用域只在函数内。

use crate::connections::{ForwardKind, ForwardRule, ForwardValidateError};
use crate::session::ExitFlag;
use crate::ssh::ServerKeyVerifier;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

/// 单条转发的运行状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForwardStatus {
    /// 正在建立（监听还没起来 / `tcpip_forward` 还没回）
    Starting,
    /// 已建立
    Active,
    /// 建立失败，附带**面向用户**的原因
    Failed(String),
    /// 已被用户停掉，或会话已结束
    Stopped,
}

impl ForwardStatus {
    /// 稳定 ASCII 键：几何护栏 / 排序 / 设置项用，**不要**用 i18n 标签。
    pub fn key(&self) -> &'static str {
        match self {
            ForwardStatus::Starting => "starting",
            ForwardStatus::Active => "active",
            ForwardStatus::Failed(_) => "failed",
            ForwardStatus::Stopped => "stopped",
        }
    }

    pub fn is_active(&self) -> bool {
        matches!(self, ForwardStatus::Active)
    }
}

/// 一条隧道的运行期句柄（UI 持有，用于展示与手动启停）。
pub struct ForwardSlot {
    pub rule: ForwardRule,
    status: Arc<Mutex<ForwardStatus>>,
    /// 收摊信号。发送 `true` **或直接 drop** 都会让转发任务跳出循环做清理
    /// （`watch::Receiver::changed()` 在发送端 drop 后立刻返回 `Err`），
    /// 所以「会话没了」这条路径不需要额外的通知机制。
    stop_tx: Mutex<Option<tokio::sync::watch::Sender<bool>>>,
    stopped: AtomicBool,
}

impl ForwardSlot {
    /// 当前状态快照（见模块注释：故意不把锁交出去）。
    pub fn status(&self) -> ForwardStatus {
        self.status.lock().clone()
    }

    /// 是否已被停掉（含会话结束导致的自动停止）。
    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Relaxed)
    }

    /// 手动停掉这条隧道。幂等：重复调用只有第一次有效果。
    pub fn stop(&self) {
        if self.stopped.swap(true, Ordering::SeqCst) {
            return;
        }
        let tx = self.stop_tx.lock().take();
        if let Some(tx) = tx {
            let _ = tx.send(true);
        }
        *self.status.lock() = ForwardStatus::Stopped;
    }
}

impl Drop for ForwardSlot {
    fn drop(&mut self) {
        // 槽位被回收 = 会话没了（`SessionHandle` 释放）⇒ 必须顺手关掉监听。
        self.stop();
    }
}

/// 一个 SSH 会话的隧道板子：持有 `Handle` 与全部槽位，支持运行期启停。
///
/// 由 `SessionHandle::forwards` 持有（`Option<Arc<ForwardBoard>>`）：
/// 会话句柄一释放，板子跟着 drop，板子里的槽位再依次 drop ⇒ 监听关闭。
/// 这就是「隧道随拥有它的 SSH 标签页一起停止」的唯一落点 —— UI 侧不需要
/// （也不应该）在关标签时逐个补一遍 `stop`。
pub struct ForwardBoard {
    handle: Arc<russh::client::Handle<ServerKeyVerifier>>,
    exit: ExitFlag,
    slots: Mutex<Vec<Arc<ForwardSlot>>>,
}

impl ForwardBoard {
    pub fn new(handle: Arc<russh::client::Handle<ServerKeyVerifier>>, exit: ExitFlag) -> Arc<Self> {
        Arc::new(Self {
            handle,
            exit,
            slots: Mutex::new(Vec::new()),
        })
    }

    /// 当前在板子上的槽位快照（含已失败的，见 [`ForwardStatus`]）。
    ///
    /// 返回 `Arc` 的克隆而不是 `&`：UI 每帧都要读状态，而槽位可能在读与用之间
    /// 被 `stop` 从表里摘掉 —— 拿着克隆的话那一帧画的东西仍然自洽。
    pub fn list(&self) -> Vec<Arc<ForwardSlot>> {
        self.slots.lock().clone()
    }

    /// 按规则 id 找运行中的槽位（`None` = 这条规则没在跑）。
    pub fn find(&self, rule_id: &str) -> Option<Arc<ForwardSlot>> {
        self.slots
            .lock()
            .iter()
            .find(|s| s.rule.id == rule_id)
            .cloned()
    }

    /// 启动一条规则。**同 id 已在跑时先停掉旧的那条**（规则被编辑过再启动，
    /// 旧监听必须让出端口，否则新的一条会以「端口已被占用」失败）。
    pub fn start(&self, rule: &ForwardRule) -> Arc<ForwardSlot> {
        self.stop(&rule.id);
        let slot = start_one(self.handle.clone(), rule.clone(), self.exit.clone());
        self.slots.lock().push(slot.clone());
        slot
    }

    /// 停掉一条规则并从板上摘下。返回 `false` = 本来就没在跑（幂等）。
    pub fn stop(&self, rule_id: &str) -> bool {
        let taken: Vec<Arc<ForwardSlot>> = {
            let mut slots = self.slots.lock();
            let (hit, keep): (Vec<_>, Vec<_>) = slots.drain(..).partition(|s| s.rule.id == rule_id);
            *slots = keep;
            hit
        };
        let found = !taken.is_empty();
        // 显式 stop：`taken` 可能还被 UI 持有 `Arc`，只靠 drop 收不了监听。
        for slot in taken {
            slot.stop();
        }
        found
    }

    /// 停掉全部（「全部停止」按钮 / 会话收摊前的兜底）。
    pub fn stop_all(&self) {
        let all: Vec<Arc<ForwardSlot>> = {
            let mut slots = self.slots.lock();
            std::mem::take(&mut *slots)
        };
        for slot in all {
            slot.stop();
        }
    }

    /// 板上还有几条（含失败态）。
    pub fn len(&self) -> usize {
        self.slots.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Drop for ForwardBoard {
    fn drop(&mut self) {
        self.stop_all();
    }
}

/// 启动一批规则（连接建立时的旧入口，保留给「规则本来就该随手连随起」的调用方）。
pub fn spawn_forwards(
    handle: Arc<russh::client::Handle<ServerKeyVerifier>>,
    rules: &[ForwardRule],
    exit: ExitFlag,
) -> Vec<Arc<ForwardSlot>> {
    rules
        .iter()
        .map(|rule| start_one(handle.clone(), rule.clone(), exit.clone()))
        .collect()
}

fn start_one(
    handle: Arc<russh::client::Handle<ServerKeyVerifier>>,
    rule: ForwardRule,
    exit: ExitFlag,
) -> Arc<ForwardSlot> {
    let status = Arc::new(Mutex::new(ForwardStatus::Starting));
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let slot = Arc::new(ForwardSlot {
        rule: rule.clone(),
        status: status.clone(),
        stop_tx: Mutex::new(Some(stop_tx)),
        stopped: AtomicBool::new(false),
    });

    if let Some(err) = rule.validate() {
        *status.lock() = ForwardStatus::Failed(validate_error_text(err).to_string());
        return slot;
    }

    match rule.kind {
        ForwardKind::Local => spawn_local(handle, rule, status, stop_rx, exit),
        ForwardKind::Dynamic => spawn_dynamic(handle, rule, status, stop_rx, exit),
        ForwardKind::Remote => spawn_remote(handle, rule, status, stop_rx, exit),
        ForwardKind::HttpConnect => spawn_http_connect(handle, rule, status, stop_rx, exit),
    }
    slot
}

/// 规则不可用的**面向用户**原因文案。
///
/// 公开是因为 UI 侧还有一条独立路径要报同一句话（连接对话框里「添加规则」的
/// 表单校验）：两处各写一份中文，就会在改文案时漂成两种说法。
pub fn validate_error_text(err: ForwardValidateError) -> &'static str {
    match err {
        ForwardValidateError::NoListenHost => crate::i18n::t("监听地址不能为空"),
        ForwardValidateError::NoListenPort => crate::i18n::t("监听端口不能为 0"),
        ForwardValidateError::NoTarget => crate::i18n::t("缺少目标地址或端口"),
    }
}

/// 把任务挂到全局运行时上（与 SSH 会话同一个 runtime —— 转发要复用同一条
/// `Handle`，跑到别的 runtime 上只会多一层线程而没有收益）。
///
/// 任务一律 **detach**：它们靠 [`ForwardSlot`] 的收摊信号与 [`ExitFlag`] 自己结束，
/// 不需要 JoinHandle。留一个没人 await 的句柄只会让「任务还在不在」有两种说法。
fn spawn_detached<F>(fut: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    drop(crate::ssh::runtime().spawn(fut));
}

/// 规则的目标两段。`Dynamic` 没有目标（握手时才知道），`None` 时调用方直接返回。
fn target_of(rule: &ForwardRule) -> Option<Target> {
    Some((rule.target_host.clone()?, rule.target_port?))
}

// ---------------------------------------------------------------- 本地 / 动态

/// 本地转发（`ssh -L`）：本机监听 → 每个连接开一条 `direct-tcpip` 到固定目标。
fn spawn_local(
    handle: Arc<russh::client::Handle<ServerKeyVerifier>>,
    rule: ForwardRule,
    status: Arc<Mutex<ForwardStatus>>,
    mut stop_rx: tokio::sync::watch::Receiver<bool>,
    exit: ExitFlag,
) {
    let bind = listen_addr(&rule);
    let Some(target) = target_of(&rule) else {
        return;
    };
    spawn_detached(async move {
        let listener = match bind_listener(&bind).await {
            Ok(l) => l,
            Err(e) => {
                *status.lock() = ForwardStatus::Failed(bind_error(&bind, &e));
                return;
            }
        };
        *status.lock() = ForwardStatus::Active;
        log::info!(
            "端口转发（本地）已启动: {bind} -> {}:{}",
            target.0,
            target.1
        );

        let mut conns = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                _ = stop_rx.changed() => break,
                _ = exit_raised(&exit) => break,
                Some(_) = conns.join_next(), if !conns.is_empty() => {}
                accepted = listener.accept() => match accepted {
                    Ok((stream, peer)) => {
                        let h = handle.clone();
                        let t = target.clone();
                        conns.spawn(async move { pipe_via_direct_tcpip(stream, h, t, peer).await });
                    }
                    Err(e) => log::warn!("端口转发 accept 失败: {e}"),
                },
            }
        }
        // 收摊：在途连接一起断掉。留着它们会让「已停止」的隧道继续搬流量。
        conns.shutdown().await;
        log::info!("端口转发（本地）已停止: {bind}");
    });
}

/// 动态转发（`ssh -D`）：本机起 SOCKS5，目标由客户端**每个连接**在握手时指定。
fn spawn_dynamic(
    handle: Arc<russh::client::Handle<ServerKeyVerifier>>,
    rule: ForwardRule,
    status: Arc<Mutex<ForwardStatus>>,
    mut stop_rx: tokio::sync::watch::Receiver<bool>,
    exit: ExitFlag,
) {
    let bind = listen_addr(&rule);
    spawn_detached(async move {
        let listener = match bind_listener(&bind).await {
            Ok(l) => l,
            Err(e) => {
                *status.lock() = ForwardStatus::Failed(bind_error(&bind, &e));
                return;
            }
        };
        *status.lock() = ForwardStatus::Active;
        log::info!("端口转发（动态 SOCKS5）已启动: {bind}");

        let mut conns = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                _ = stop_rx.changed() => break,
                _ = exit_raised(&exit) => break,
                Some(_) = conns.join_next(), if !conns.is_empty() => {}
                accepted = listener.accept() => match accepted {
                    Ok((stream, peer)) => {
                        let h = handle.clone();
                        conns.spawn(async move { serve_socks5(stream, h, peer).await });
                    }
                    Err(e) => log::warn!("SOCKS5 accept 失败: {e}"),
                },
            }
        }
        conns.shutdown().await;
        log::info!("端口转发（动态 SOCKS5）已停止: {bind}");
    });
}

/// 一条隧道的目的端。
type Target = (String, u16);

async fn bind_listener(bind: &str) -> std::io::Result<tokio::net::TcpListener> {
    tokio::net::TcpListener::bind(bind).await
}

fn listen_addr(rule: &ForwardRule) -> String {
    format!("{}:{}", rule.listen_host, rule.listen_port)
}

fn bind_error(bind: &str, e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::AddrInUse => crate::i18n::ts(
            format!("端口已被占用：{bind}"),
            format!("Port already in use: {bind}"),
        ),
        std::io::ErrorKind::PermissionDenied => crate::i18n::ts(
            format!("无权监听 {bind}（1024 以下端口通常需要管理员权限）"),
            format!("Permission denied binding {bind} (ports below 1024 usually need admin)"),
        ),
        _ => crate::i18n::ts(
            format!("无法监听 {bind}：{e}"),
            format!("Cannot listen on {bind}: {e}"),
        ),
    }
}

/// 开一条 `direct-tcpip` 信道，把本端 TCP 与它双向对接。
async fn pipe_via_direct_tcpip(
    stream: tokio::net::TcpStream,
    handle: Arc<russh::client::Handle<ServerKeyVerifier>>,
    target: Target,
    peer: std::net::SocketAddr,
) {
    let (host, port) = target;
    let channel = match handle
        .channel_open_direct_tcpip(
            host.clone(),
            port as u32,
            peer.ip().to_string(),
            peer.port() as u32,
        )
        .await
    {
        Ok(c) => c,
        Err(e) => {
            // 目标不可达时服务端会拒绝开信道 —— 这条是用户「连了但没通」的**唯一**解释来源。
            log::warn!("转发到 {host}:{port} 失败: {e}");
            return;
        }
    };
    splice(stream, channel).await;
}

/// TCP ↔ SSH 信道双向搬运。
///
/// 用 `copy_bidirectional` 而不是手写两个 `copy`：它会把一端的 EOF 半关到另一端，
/// 手写版最常见的 bug 就是「上传完不收尾」—— 对方永远等不到 EOF，连接挂到超时。
async fn splice(tcp: tokio::net::TcpStream, channel: russh::Channel<russh::client::Msg>) {
    let mut local = tcp;
    let mut remote = channel.into_stream();
    match tokio::io::copy_bidirectional(&mut local, &mut remote).await {
        // 对端正常断开也会走到 Err（ConnectionReset / BrokenPipe），不值得 warn
        Ok((up, down)) => log::debug!("转发管道结束: 上行 {up}B 下行 {down}B"),
        Err(e) => log::debug!("转发管道结束: {e}"),
    }
}

// ------------------------------------------------------------------ 远端转发

/// 服务端为远端转发打开的入站信道。
struct ForwardedOpen {
    channel: russh::Channel<russh::client::Msg>,
    originator: String,
}

struct RemoteRoute {
    listen_host: String,
    listen_port: u16,
    tx: flume::Sender<ForwardedOpen>,
}

/// 远端转发的路由表（进程级）。
///
/// 为什么必须是**全局**的：`server_channel_open_forwarded_tcpip` 是 `Handler` 的
/// 方法，而 `Handler` 的实例活在 russh 的连接任务里 —— 那条任务拿不到任何
/// 「当前会话的转发规则」上下文。全局表是唯一能把「服务端开的信道」接回
/// 「哪条规则在等它」的桥。同端口只会有一条规则（注册时就挡掉重复）。
static REMOTE_ROUTES: LazyLock<Mutex<Vec<RemoteRoute>>> = LazyLock::new(|| Mutex::new(Vec::new()));

/// 注册一条远端转发路由。返回 `false` = 该端口已被另一条规则占用。
fn register_remote(host: &str, port: u16, tx: flume::Sender<ForwardedOpen>) -> bool {
    let mut routes = REMOTE_ROUTES.lock();
    if routes.iter().any(|r| r.listen_port == port) {
        return false;
    }
    routes.push(RemoteRoute {
        listen_host: host.to_string(),
        listen_port: port,
        tx,
    });
    true
}

fn unregister_remote(port: u16, host: &str) {
    let mut routes = REMOTE_ROUTES.lock();
    routes.retain(|r| !(r.listen_port == port && r.listen_host == host));
}

/// 该端口上是否有人在等（`Handler` 用它决定接受还是拒绝）。
///
/// **只按端口匹配**：服务端回报的 `connected_address` 与请求时的写法常常不同
/// （请求 `localhost`、回报 `127.0.0.1` 是常态），按地址串匹配会让远端转发
/// 时通时不通 —— 而这种 bug 只在那台机器上复现。
pub fn has_remote_route(connected_port: u32) -> bool {
    let port = connected_port as u16;
    REMOTE_ROUTES.lock().iter().any(|r| r.listen_port == port)
}

/// 把一条服务端打开的信道投递给等它的规则。返回 `false` = 无人认领。
pub fn deliver_forwarded(
    connected_port: u32,
    channel: russh::Channel<russh::client::Msg>,
    originator: String,
) -> bool {
    let port = connected_port as u16;
    let tx = {
        let routes = REMOTE_ROUTES.lock();
        match routes.iter().find(|r| r.listen_port == port) {
            Some(r) => r.tx.clone(),
            None => return false,
        }
    };
    tx.send(ForwardedOpen {
        channel,
        originator,
    })
    .is_ok()
}

/// 远端转发（`ssh -R`）：请求服务端监听，再把服务端送来的连接转到目标。
fn spawn_remote(
    handle: Arc<russh::client::Handle<ServerKeyVerifier>>,
    rule: ForwardRule,
    status: Arc<Mutex<ForwardStatus>>,
    mut stop_rx: tokio::sync::watch::Receiver<bool>,
    exit: ExitFlag,
) {
    let host = rule.listen_host.clone();
    let port = rule.listen_port;
    let Some(target) = target_of(&rule) else {
        return;
    };
    spawn_detached(async move {
        let (tx, rx) = flume::unbounded::<ForwardedOpen>();
        if !register_remote(&host, port, tx) {
            *status.lock() = ForwardStatus::Failed(crate::i18n::ts(
                format!("监听端口 {port} 已被另一条远端转发规则占用"),
                format!("Listen port {port} is already claimed by another remote forward"),
            ));
            return;
        }

        // 服务端可能拒绝（`AllowTcpForwarding no`）或占用失败 —— 这时**必须**退掉
        // 路由注册，否则那条端口会永远认领得到信道却没有消费者。
        match handle.tcpip_forward(host.clone(), port as u32).await {
            Ok(_bound) => {}
            Err(e) => {
                unregister_remote(port, &host);
                *status.lock() = ForwardStatus::Failed(crate::i18n::ts(
                    format!("服务器拒绝远端转发 {host}:{port}：{e}"),
                    format!("Server refused remote forward {host}:{port}: {e}"),
                ));
                return;
            }
        }
        *status.lock() = ForwardStatus::Active;
        log::info!(
            "端口转发（远端）已启动: {host}:{port} -> {}:{}",
            target.0,
            target.1
        );

        let mut conns = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                _ = stop_rx.changed() => break,
                _ = exit_raised(&exit) => break,
                Some(_) = conns.join_next(), if !conns.is_empty() => {}
                incoming = rx.recv_async() => match incoming {
                    Ok(open) => {
                        let t = target.clone();
                        conns.spawn(async move { pipe_channel_to_target(open, t).await });
                    }
                    Err(_) => break,
                },
            }
        }
        conns.shutdown().await;
        unregister_remote(port, &host);
        // 会话还活着时停单条隧道，必须显式注销服务端监听 —— 否则服务器上那个
        // 端口会一直开着并接受连接，而本端已经没人认领这些连接了。
        let _ = handle.cancel_tcpip_forward(host.clone(), port as u32).await;
        log::info!("端口转发（远端）已停止: {host}:{port}");
    });
}

async fn pipe_channel_to_target(open: ForwardedOpen, target: Target) {
    let (host, port) = target;
    match tokio::net::TcpStream::connect((host.as_str(), port)).await {
        Ok(tcp) => splice(tcp, open.channel).await,
        Err(e) => {
            log::warn!(
                "远端转发：无法连接目标 {host}:{port}（来自 {}）：{e}",
                open.originator
            );
        }
    }
}

// --------------------------------------------------------------------- SOCKS5

/// SOCKS5 版本号（RFC1928）。
const SOCKS5_VERSION: u8 = 0x05;
/// 「无认证」方法码。
const SOCKS5_NO_AUTH: u8 = 0x00;
/// 「没有可接受的方法」方法码。
const SOCKS5_NO_ACCEPTABLE: u8 = 0xFF;
/// CONNECT 命令。
const SOCKS5_CMD_CONNECT: u8 = 0x01;

/// 解析 SOCKS5 问候（RFC1928 §3）：`VER NMETHODS METHODS...`
///
/// - `Ok(Some(n))` = 已收齐一个问候，消耗 `n` 字节
/// - `Ok(None)` = 字节还不够
/// - `Err(code)` = 该回的**应答码**（`0xFF` = 没有可接受的方法）
///
/// 做成纯函数是为了可单测：握手散在 async 流程里时，「少一个字节」和
/// 「版本不对」这两种错都只能靠真连一个 SOCKS 客户端才能发现。
pub fn decode_socks5_greeting(buf: &[u8]) -> Result<Option<usize>, u8> {
    let Some(&ver) = buf.first() else {
        return Ok(None);
    };
    if ver != SOCKS5_VERSION {
        return Err(SOCKS5_NO_ACCEPTABLE);
    }
    let Some(&n) = buf.get(1) else {
        return Ok(None);
    };
    let n = n as usize;
    if n == 0 {
        return Err(SOCKS5_NO_ACCEPTABLE);
    }
    if buf.len() < 2 + n {
        return Ok(None);
    }
    // 我们只支持「无认证」；客户端没提供就直接拒，而不是假装接受后
    // 按无认证处理 —— 那等于把「需要认证」的语义悄悄抹掉。
    if !buf[2..2 + n].contains(&SOCKS5_NO_AUTH) {
        return Err(SOCKS5_NO_ACCEPTABLE);
    }
    Ok(Some(2 + n))
}

/// 解析 SOCKS5 请求（RFC1928 §4）：`VER CMD RSV ATYP DST.ADDR DST.PORT`
///
/// - `Ok(Some((n, host, port)))` = 可连接的目标，消耗 `n` 字节
/// - `Ok(None)` = 字节还不够
/// - `Err(code)` = 该回的应答码（`0x07` 不支持的命令 / `0x08` 不支持的地址类型）
pub fn decode_socks5_connect(buf: &[u8]) -> Result<Option<(usize, String, u16)>, u8> {
    if buf.len() < 4 {
        return Ok(None);
    }
    if buf[0] != SOCKS5_VERSION || buf[2] != 0x00 {
        return Err(0x01); // general failure
    }
    if buf[1] != SOCKS5_CMD_CONNECT {
        return Err(0x07); // command not supported
    }
    // 地址段的 (起始偏移, 长度)。域名形式在地址前多一个「长度」字节，所以起始
    // **不是**常数 4 —— 只记长度会让端口读到域名的字节上，连出来的目标变成
    // `xample.com:25961` 这种，比直接报错难查得多。
    let (host_start, addr_len) = match buf[3] {
        0x01 => {
            if buf.len() < 8 {
                return Ok(None);
            }
            (4usize, 4usize)
        }
        0x03 => {
            let n = buf[4] as usize;
            if n == 0 {
                return Err(0x01);
            }
            if buf.len() < 5 + n {
                return Ok(None);
            }
            (5usize, n)
        }
        0x04 => {
            if buf.len() < 20 {
                return Ok(None);
            }
            (4usize, 16usize)
        }
        _ => return Err(0x08), // address type not supported
    };
    let port_at = host_start + addr_len;
    if buf.len() < port_at + 2 {
        return Ok(None);
    }
    let host = match buf[3] {
        0x01 => format!("{}.{}.{}.{}", buf[4], buf[5], buf[6], buf[7]),
        0x03 => String::from_utf8_lossy(&buf[host_start..host_start + addr_len]).to_string(),
        _ => {
            let mut octets = [0u8; 16];
            octets.copy_from_slice(&buf[host_start..host_start + 16]);
            std::net::Ipv6Addr::from(octets).to_string()
        }
    };
    let port = u16::from_be_bytes([buf[port_at], buf[port_at + 1]]);
    if port == 0 {
        return Err(0x01);
    }
    Ok(Some((port_at + 2, host, port)))
}

/// SOCKS5 应答：`VER REP RSV ATYP BND.ADDR BND.PORT`。
///
/// BND 一律填 `0.0.0.0:0`：它对 CONNECT 无意义（那是 BIND 才关心的），
/// RFC1928 也允许客户端忽略，填真值反而要求我们去问 SSH 信道拿本地端点。
fn socks5_reply(code: u8) -> [u8; 10] {
    [SOCKS5_VERSION, code, 0x00, 0x01, 0, 0, 0, 0, 0, 0]
}

async fn serve_socks5(
    mut stream: tokio::net::TcpStream,
    handle: Arc<russh::client::Handle<ServerKeyVerifier>>,
    peer: std::net::SocketAddr,
) {
    use tokio::io::AsyncWriteExt as _;

    let target = match negotiate_socks5(&mut stream).await {
        Ok(Some(t)) => t,
        // 客户端半途走了 / 目标字段非法并已回过错码
        Ok(None) => return,
        Err(code) => {
            let _ = stream.write_all(&socks5_reply(code)).await;
            return;
        }
    };

    let (host, port) = target;
    match handle
        .channel_open_direct_tcpip(
            host.clone(),
            port as u32,
            peer.ip().to_string(),
            peer.port() as u32,
        )
        .await
    {
        Ok(channel) => {
            if stream.write_all(&socks5_reply(0x00)).await.is_err() {
                return;
            }
            splice(stream, channel).await;
        }
        Err(e) => {
            // 0x05 = connection refused：对 SOCKS 客户端来说，「目标连不上」
            // 与「隧道本身坏了」是两回事，必须分开报。
            log::warn!("SOCKS5 目标 {host}:{port} 不可达: {e}");
            let _ = stream.write_all(&socks5_reply(0x05)).await;
        }
    }
}

async fn negotiate_socks5(stream: &mut tokio::net::TcpStream) -> Result<Option<Target>, u8> {
    use tokio::io::AsyncWriteExt as _;

    let mut buf: Vec<u8> = Vec::with_capacity(64);
    loop {
        if let Some(n) = decode_socks5_greeting(&buf)? {
            buf.drain(..n);
            break;
        }
        if !read_more(stream, &mut buf, 512).await {
            return Ok(None);
        }
    }
    stream
        .write_all(&[SOCKS5_VERSION, SOCKS5_NO_AUTH])
        .await
        .map_err(|_| 0x01)?;

    loop {
        if let Some((_n, host, port)) = decode_socks5_connect(&buf)? {
            return Ok(Some((host, port)));
        }
        if !read_more(stream, &mut buf, 4096).await {
            return Ok(None);
        }
    }
}

/// 再读一段进缓冲；返回 `false` = 对端已关闭。
async fn read_more(stream: &mut tokio::net::TcpStream, buf: &mut Vec<u8>, cap: usize) -> bool {
    use tokio::io::AsyncReadExt as _;
    let mut chunk = [0u8; 512];
    match stream.read(&mut chunk).await {
        Ok(0) | Err(_) => false,
        Ok(n) => {
            buf.extend_from_slice(&chunk[..n]);
            // 防「永远说不完整」的恶意/坏客户端把内存撑爆：超过上限就当作失败。
            buf.len() <= cap
        }
    }
}

// --------------------------------------------------------------- HTTP CONNECT

/// 请求头上限：有意义的只有 `CONNECT host:port HTTP/1.x` 一行，8KB 已经极其宽松。
/// 超过它就不是代理请求（多半是端口上接了个别的协议），按 400 拒掉，
/// 而不是把内存读到爆。
const HTTP_HEAD_CAP: usize = 8 * 1024;

/// 解析 HTTP CONNECT 请求头（RFC 9110 §9.3.6）。
///
/// - `Ok(Some((n, host, port)))` = 已收齐请求头；`n` 是头部字节数，
///   **其后若还粘着客户端提前发来的数据，必须原样转给目标**（CONNECT 允许
///   客户端把应用数据与请求头一次发出，丢了它会让 HTTPS 握手卡住）
/// - `Ok(None)` = 还没读到 `\r\n\r\n`
/// - `Err(status)` = 该回的 HTTP 状态码：405 = 方法不是 CONNECT，400 = 请求行坏
///
/// 做成纯函数是为了可单测：握手埋在 async 里时，「头部没读全」与「方法不对」
/// 这两种错都只能靠真起一个代理客户端才发现。
pub fn decode_http_connect(buf: &[u8]) -> Result<Option<(usize, String, u16)>, u16> {
    let Some(end) = find_head_end(buf) else {
        return if buf.len() > HTTP_HEAD_CAP {
            Err(400)
        } else {
            Ok(None)
        };
    };
    let head = String::from_utf8_lossy(&buf[..end]);
    let line = head.lines().next().unwrap_or("").trim_end();
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");
    let version = parts.next().unwrap_or("");
    if method.is_empty() || !version.starts_with("HTTP/1.") {
        return Err(400);
    }
    if !method.eq_ignore_ascii_case("CONNECT") {
        // 405 而不是 400：`GET http://x/` 是合法 HTTP，只是我们不代理普通请求。
        return Err(405);
    }
    let Some((host, port)) = split_host_port(target) else {
        return Err(400);
    };
    if host.is_empty() || port == 0 {
        return Err(400);
    }
    Ok(Some((end, host, port)))
}

/// `\r\n\r\n` 之后的位置（= 请求头的字节数）。
fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
}

/// 拆 `host:port` / `[::1]:port`。
///
/// CONNECT **必须**带端口（不带就没有目标），所以缺端口一律返回 `None`；
/// 裸 IPv6（含多个冒号却无方括号）同样是语法错误。
fn split_host_port(target: &str) -> Option<(String, u16)> {
    if let Some(rest) = target.strip_prefix('[') {
        let (host, tail) = rest.split_once(']')?;
        let port = tail.strip_prefix(':')?;
        return Some((host.to_string(), port.parse().ok()?));
    }
    let (host, port) = target.rsplit_once(':')?;
    if host.contains(':') {
        return None;
    }
    Some((host.to_string(), port.parse().ok()?))
}

/// 出错时回给客户端的**最短**响应：带 `Connection: close` 让客户端别等复用。
fn http_error_reply(code: u16) -> String {
    let reason = match code {
        400 => "Bad Request",
        405 => "Method Not Allowed",
        502 => "Bad Gateway",
        _ => "Error",
    };
    format!("HTTP/1.1 {code} {reason}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n")
}

/// HTTP CONNECT 代理：本机监听，目标由每个连接的请求行给出。
fn spawn_http_connect(
    handle: Arc<russh::client::Handle<ServerKeyVerifier>>,
    rule: ForwardRule,
    status: Arc<Mutex<ForwardStatus>>,
    mut stop_rx: tokio::sync::watch::Receiver<bool>,
    exit: ExitFlag,
) {
    let bind = listen_addr(&rule);
    spawn_detached(async move {
        let listener = match bind_listener(&bind).await {
            Ok(l) => l,
            Err(e) => {
                *status.lock() = ForwardStatus::Failed(bind_error(&bind, &e));
                return;
            }
        };
        *status.lock() = ForwardStatus::Active;
        log::info!("端口转发（HTTP CONNECT）已启动: {bind}");

        let mut conns = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                _ = stop_rx.changed() => break,
                _ = exit_raised(&exit) => break,
                Some(_) = conns.join_next(), if !conns.is_empty() => {}
                accepted = listener.accept() => match accepted {
                    Ok((stream, peer)) => {
                        let h = handle.clone();
                        conns.spawn(async move { serve_http_connect(stream, h, peer).await });
                    }
                    Err(e) => log::warn!("HTTP CONNECT accept 失败: {e}"),
                },
            }
        }
        conns.shutdown().await;
        log::info!("端口转发（HTTP CONNECT）已停止: {bind}");
    });
}

async fn serve_http_connect(
    mut stream: tokio::net::TcpStream,
    handle: Arc<russh::client::Handle<ServerKeyVerifier>>,
    peer: std::net::SocketAddr,
) {
    use tokio::io::AsyncWriteExt as _;

    let mut buf: Vec<u8> = Vec::with_capacity(256);
    let (host, port, leftover) = loop {
        match decode_http_connect(&buf) {
            Ok(Some((n, host, port))) => break (host, port, buf.split_off(n)),
            Ok(None) => {
                if !read_more(&mut stream, &mut buf, HTTP_HEAD_CAP).await {
                    return;
                }
            }
            Err(code) => {
                let _ = stream.write_all(http_error_reply(code).as_bytes()).await;
                return;
            }
        }
    };

    match handle
        .channel_open_direct_tcpip(
            host.clone(),
            port as u32,
            peer.ip().to_string(),
            peer.port() as u32,
        )
        .await
    {
        Ok(channel) => {
            if stream
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .await
                .is_err()
            {
                return;
            }
            splice_with_prefix(stream, channel, leftover).await;
        }
        Err(e) => {
            // 502 = 上游不可达（与「代理本身坏了」区分开，客户端据此提示才有意义）
            log::warn!("HTTP CONNECT 目标 {host}:{port} 不可达: {e}");
            let _ = stream.write_all(http_error_reply(502).as_bytes()).await;
        }
    }
}

/// 与 [`splice`] 同一件事，但先把客户端提前发来的字节灌进信道。
async fn splice_with_prefix(
    tcp: tokio::net::TcpStream,
    channel: russh::Channel<russh::client::Msg>,
    prefix: Vec<u8>,
) {
    use tokio::io::AsyncWriteExt as _;
    let mut local = tcp;
    let mut remote = channel.into_stream();
    if !prefix.is_empty() && remote.write_all(&prefix).await.is_err() {
        return;
    }
    match tokio::io::copy_bidirectional(&mut local, &mut remote).await {
        Ok((up, down)) => log::debug!("HTTP CONNECT 管道结束: 上行 {up}B 下行 {down}B"),
        Err(e) => log::debug!("HTTP CONNECT 管道结束: {e}"),
    }
}

// ------------------------------------------------------------------- 会话退出

/// 会话退出标志的异步等待。
///
/// 轮询（200ms）而不是条件变量：`ExitFlag` 是个裸 `AtomicBool`，它被置位的
/// 时机是 `SessionHandle::drop`（UI 线程），那里不能做任何可能阻塞的事 ——
/// 为了一个通知再去加一套 channel，收益只有 200ms 的延迟。
async fn exit_raised(exit: &ExitFlag) {
    while !exit.raised() {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connections::{ForwardKind, ForwardRule};

    /// 问候：够一个完整问候才返回长度，缺一个字节就不许「猜」。
    #[test]
    fn socks5_greeting_needs_every_method_byte() {
        assert_eq!(decode_socks5_greeting(&[]), Ok(None));
        assert_eq!(decode_socks5_greeting(&[0x05]), Ok(None));
        // 声明 2 个方法但只给了 1 个 —— 必须继续等，不能按 1 个算
        assert_eq!(decode_socks5_greeting(&[0x05, 0x02, 0x00]), Ok(None));
        assert_eq!(
            decode_socks5_greeting(&[0x05, 0x02, 0x00, 0x02]),
            Ok(Some(4))
        );
        // 多给的字节是下一个报文的，不能算进本次消耗
        assert_eq!(
            decode_socks5_greeting(&[0x05, 0x01, 0x00, 0xAA, 0xBB]),
            Ok(Some(3))
        );
    }

    #[test]
    fn socks5_greeting_rejects_unsupported_and_malformed() {
        // 版本不对
        assert_eq!(decode_socks5_greeting(&[0x04, 0x01, 0x00]), Err(0xFF));
        // 一个方法都不提供
        assert_eq!(decode_socks5_greeting(&[0x05, 0x00]), Err(0xFF));
        // 只提供「用户名口令认证」——我们没实现，必须拒而不是当成无认证放过去
        assert_eq!(decode_socks5_greeting(&[0x05, 0x01, 0x02]), Err(0xFF));
    }

    #[test]
    fn socks5_connect_parses_all_three_address_forms() {
        // IPv4 + 端口 80
        let ipv4 = [0x05, 0x01, 0x00, 0x01, 10, 0, 0, 7, 0x00, 0x50];
        assert_eq!(
            decode_socks5_connect(&ipv4),
            Ok(Some((10, "10.0.0.7".to_string(), 80)))
        );
        // 域名（带长度前缀）
        let mut domain = vec![0x05, 0x01, 0x00, 0x03, 0x0B];
        domain.extend_from_slice(b"example.com");
        domain.extend_from_slice(&443u16.to_be_bytes());
        assert_eq!(
            decode_socks5_connect(&domain),
            Ok(Some((18, "example.com".to_string(), 443)))
        );
        // IPv6
        let mut ipv6 = vec![0x05, 0x01, 0x00, 0x04];
        ipv6.extend_from_slice(&std::net::Ipv6Addr::LOCALHOST.octets());
        ipv6.extend_from_slice(&8080u16.to_be_bytes());
        assert_eq!(
            decode_socks5_connect(&ipv6),
            Ok(Some((22, "::1".to_string(), 8080)))
        );
    }

    #[test]
    fn socks5_connect_waits_for_full_payload_then_rejects_bad_commands() {
        let full = [0x05, 0x01, 0x00, 0x01, 10, 0, 0, 7, 0x00, 0x50];
        // 逐字节喂：任何一个前缀都不该被当成「读完了」
        for cut in 0..full.len() {
            assert_eq!(
                decode_socks5_connect(&full[..cut]),
                Ok(None),
                "截断到 {cut} 字节时不应给出结果"
            );
        }
        // BIND(0x02) 我们不支持 —— 必须回「命令不支持」而不是当成 CONNECT
        let bind = [0x05, 0x02, 0x00, 0x01, 10, 0, 0, 7, 0x00, 0x50];
        assert_eq!(decode_socks5_connect(&bind), Err(0x07));
        // 地址类型不认识
        let weird = [0x05, 0x01, 0x00, 0x09, 10, 0, 0, 7, 0x00, 0x50];
        assert_eq!(decode_socks5_connect(&weird), Err(0x08));
        // 端口 0 不是一个可连的目标
        let zero_port = [0x05, 0x01, 0x00, 0x01, 10, 0, 0, 7, 0x00, 0x00];
        assert_eq!(decode_socks5_connect(&zero_port), Err(0x01));
    }

    #[test]
    fn forward_rule_validation_covers_the_four_kinds() {
        let mut local = ForwardRule::new(ForwardKind::Local);
        local.listen_port = 8080;
        // 本地转发没有目标 → 不可用（这条规则真跑起来会连到空主机名）
        assert_eq!(local.validate(), Some(ForwardValidateError::NoTarget));
        local.target_host = Some("db.internal".into());
        local.target_port = Some(5432);
        assert_eq!(local.validate(), None);

        // 端口 0 = 「让系统选」，但转发规则里没人回填真实端口 ⇒ 一律判非法
        let mut bad_port = local.clone();
        bad_port.listen_port = 0;
        assert_eq!(
            bad_port.validate(),
            Some(ForwardValidateError::NoListenPort)
        );

        let mut no_host = local.clone();
        no_host.listen_host = "   ".into();
        assert_eq!(no_host.validate(), Some(ForwardValidateError::NoListenHost));

        // 动态转发不需要目标
        let mut dynamic = ForwardRule::new(ForwardKind::Dynamic);
        dynamic.listen_port = 1080;
        assert_eq!(dynamic.validate(), None);

        // HTTP CONNECT 同理：目标在请求行里，规则不需要它
        let mut http = ForwardRule::new(ForwardKind::HttpConnect);
        http.listen_port = 8888;
        assert_eq!(http.validate(), None);
    }

    /// CONNECT 的请求头必须**逐字节**收齐才算收到：任何前缀都不许被当成
    /// 「已经能建连了」（少读一个 `\r\n` 就会把请求头残片当应用数据发进隧道）。
    #[test]
    fn http_connect_waits_for_the_full_head_then_parses_the_target() {
        let req = b"CONNECT db.internal:5432 HTTP/1.1\r\nHost: db.internal:5432\r\n\r\n";
        for cut in 0..req.len() {
            assert_eq!(
                decode_http_connect(&req[..cut]),
                Ok(None),
                "截断到 {cut} 字节时不应给出结果"
            );
        }
        assert_eq!(
            decode_http_connect(req),
            Ok(Some((req.len(), "db.internal".to_string(), 5432)))
        );
    }

    #[test]
    fn http_connect_rejects_non_connect_and_malformed_targets() {
        // 普通代理请求我们不代理 —— 405 而不是 400：那是一个合法 HTTP 请求
        assert_eq!(
            decode_http_connect(b"GET http://example.com/ HTTP/1.1\r\n\r\n"),
            Err(405)
        );
        // 缺端口 = 没有目标（CONNECT 必须带）
        assert_eq!(
            decode_http_connect(b"CONNECT db.internal HTTP/1.1\r\n\r\n"),
            Err(400)
        );
        // 端口 0 不是可连的目标
        assert_eq!(
            decode_http_connect(b"CONNECT db.internal:0 HTTP/1.1\r\n\r\n"),
            Err(400)
        );
        // 主机为空
        assert_eq!(
            decode_http_connect(b"CONNECT :8080 HTTP/1.1\r\n\r\n"),
            Err(400)
        );
        // 缺协议版本
        assert_eq!(
            decode_http_connect(b"CONNECT db.internal:5432\r\n\r\n"),
            Err(400)
        );
        // 裸 IPv6（多个冒号又没有方括号）是语法错误，不许猜
        assert_eq!(
            decode_http_connect(b"CONNECT ::1:443 HTTP/1.1\r\n\r\n"),
            Err(400)
        );
    }

    /// 方括号 IPv6 能用，且**粘在请求头后面的应用数据不能丢**（CONNECT 允许
    /// 客户端把 TLS hello 与请求头一次发出）。
    #[test]
    fn http_connect_handles_ipv6_and_reports_head_length_for_the_leftover() {
        let mut req = b"CONNECT [::1]:443 HTTP/1.1\r\n\r\n".to_vec();
        let head = req.len();
        req.extend_from_slice(&[0x16, 0x03, 0x01]);
        assert_eq!(
            decode_http_connect(&req),
            Ok(Some((head, "::1".to_string(), 443)))
        );
    }

    /// 头部超过上限还不完整时按 400 拒掉 —— 端口上接的很可能不是 HTTP。
    #[test]
    fn http_connect_gives_up_on_an_oversized_head() {
        let junk = vec![b'A'; HTTP_HEAD_CAP + 1];
        assert_eq!(decode_http_connect(&junk), Err(400));
    }

    #[test]
    fn normalize_fills_defaults_and_drops_dynamic_target() {
        let mut dynamic = ForwardRule::new(ForwardKind::Dynamic);
        dynamic.listen_host = "  ".into();
        dynamic.target_host = Some("leftover".into());
        dynamic.target_port = Some(1);
        dynamic.id = String::new();
        assert!(dynamic.normalize());
        assert_eq!(dynamic.listen_host, "127.0.0.1");
        assert_eq!(dynamic.target_host, None, "SOCKS5 的目标由握手给出");
        assert_eq!(dynamic.target_port, None);
        assert!(!dynamic.id.is_empty(), "空 id 必须补上");
        // 再归一化一次不该有变化（幂等）
        assert!(!dynamic.normalize());
    }

    #[test]
    fn listen_host_defaults_to_loopback_not_all_interfaces() {
        let rule = ForwardRule::new(ForwardKind::Local);
        assert_eq!(rule.listen_host, "127.0.0.1");
    }

    /// 列表里的 NAME 列必须总有可读内容：老配置没有 `name` 键，空名由
    /// 类型 + 端口补出来；用户起的名字只去空白，不被覆盖。
    #[test]
    fn normalize_gives_every_rule_a_readable_name() {
        let mut http = ForwardRule::new(ForwardKind::HttpConnect);
        http.listen_port = 8888;
        http.normalize();
        assert_eq!(http.name, "http connect 8888");

        // 端口还没填时只给类型名：列表里出现「local 0」只会让人以为是 bug
        let mut pending = ForwardRule::new(ForwardKind::Local);
        pending.normalize();
        assert_eq!(pending.name, "local");

        let mut named = ForwardRule::new(ForwardKind::Local);
        named.listen_port = 5432;
        named.name = "  postgres  ".into();
        named.normalize();
        assert_eq!(named.name, "postgres");
    }

    /// 老 `connections.json` 里没有 `forwards` 键，必须照常读得进来。
    #[test]
    fn profile_without_forwards_key_still_loads() {
        let json = r#"{
            "id": "p1", "name": "n", "host": "h", "port": 22, "username": "u",
            "notes": "", "last_used_at": 0
        }"#;
        let p: crate::connections::ConnectionProfile =
            serde_json::from_str(json).expect("老库必须能读");
        assert!(p.forwards.is_empty());
    }

    #[test]
    fn remote_routes_claim_by_port_not_by_address_spelling() {
        // 端口取一个本测试专用的号，避免与并行跑的用例互踩（路由表是进程级静态）
        let port = 45_231u16;
        let (tx, rx) = flume::unbounded::<ForwardedOpen>();
        assert!(register_remote("localhost", port, tx));
        assert!(has_remote_route(port as u32));
        // 服务端回报的地址与请求时的写法常常不同 —— 只按端口认领
        assert!(has_remote_route(port as u32));

        unregister_remote(port, "localhost");
        assert!(!has_remote_route(port as u32));
        drop(rx);
    }

    #[test]
    fn duplicate_remote_port_is_refused() {
        let port = 45_232u16;
        let (tx1, _rx1) = flume::unbounded::<ForwardedOpen>();
        let (tx2, _rx2) = flume::unbounded::<ForwardedOpen>();
        assert!(register_remote("127.0.0.1", port, tx1));
        assert!(
            !register_remote("127.0.0.1", port, tx2),
            "同一端口只能有一条远端转发规则，否则信道投递给谁是不确定的"
        );
        unregister_remote(port, "127.0.0.1");
        assert!(!has_remote_route(port as u32));
    }

    /// 停止是幂等的，并且会让等待方收到信号（接收端 drop 也算）。
    #[test]
    fn stopping_a_slot_is_idempotent_and_signals_waiters() {
        let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        let slot = ForwardSlot {
            rule: ForwardRule::new(ForwardKind::Local),
            status: Arc::new(Mutex::new(ForwardStatus::Active)),
            stop_tx: Mutex::new(Some(stop_tx)),
            stopped: AtomicBool::new(false),
        };
        assert!(slot.status().is_active());
        slot.stop();
        assert_eq!(slot.status(), ForwardStatus::Stopped);
        assert!(slot.is_stopped());
        // `stop_tx` 已被取走，第二次调用不能 panic
        slot.stop();
        assert_eq!(slot.status(), ForwardStatus::Stopped);
        // 发送端已被取走并 drop —— 对 `changed()` 而言这同样是「立刻返回」，
        // 监听循环正是靠这一点在 `stop_tx` 随槽位一起消失时收摊。
        let signalled = stop_rx.has_changed().unwrap_or(true);
        assert!(signalled, "停止必须让等待方立刻醒过来");
    }
}
