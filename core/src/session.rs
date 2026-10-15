//! 会话模型：本地 Shell 与 SSH 会话的统一抽象。

use crate::stream::{PipeReader, PipeWriter};
use parking_lot::Mutex;
use portable_pty::{native_pty_system, Child, ChildKiller, CommandBuilder, MasterPty, PtySize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

pub type SessionId = String;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    Local,
    Ssh,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionState {
    Connecting,
    Connected,
    Closed,
}

#[derive(Debug, Clone)]
pub enum SessionEvent {
    /// 对端正常关闭
    Closed,
    /// 终端标题（OSC 序列转发）
    Title(String),
    /// 非致命错误
    Error(String),
}

/// 尺寸变化的目标端（本地 PTY master 或 SSH 控制通道）
#[derive(Clone)]
pub enum ResizeSink {
    Local(Arc<PtyResizer>),
    Ssh(Arc<SshResizeCoalescer>),
}

impl ResizeSink {
    /// 本函数运行在 UI 线程（终端 canvas 的尺寸回调，见
    /// `rustshell_terminal::view::TerminalView::render`）。
    ///
    /// **必须不阻塞**：拖拽分屏/窗口边缘时每帧都会调一次，而底层分别是
    /// `ResizePseudoConsole`（跨进程同步调用）与 SSH `window_change`（网络往返）。
    /// 两条路径都只做「入队 + 覆盖最新值」，真正的工作在后台线程/任务里做。
    pub fn resize(&self, cols: u16, rows: u16) {
        match self {
            ResizeSink::Local(resizer) => resizer.resize(cols, rows),
            ResizeSink::Ssh(coalescer) => coalescer.resize(cols as u32, rows as u32),
        }
    }
}

/// SSH 窗口尺寸变更的**合并槽**：UI 线程只写一个 `Option`，永不阻塞。
///
/// 为什么不能直接用有界 `flume::Sender::send`（2026-09 修复的缺陷）：
/// 原实现是 `flume::bounded(8)` + UI 线程 `tx.send(..)`。拖拽时每帧一次 resize，
/// 而消费端每次都要 await 一个 SSH `window_change` 网络往返（慢链路上百毫秒）；
/// 一旦队列积满 8 个，第 9 次 `send` 就在 **UI 线程上阻塞**，整个窗口冻结。
///
/// 现在的语义是「最新者胜」：远端尺寸只关心最终值，中间过程丢了没有损失。
/// 本地 PTY 走 [`PtyResizer`] 的 unbounded + drain 合并，是同一思路在同步侧的写法。
#[derive(Debug)]
pub struct SshResizeCoalescer {
    /// 待发送的最新尺寸（`None` = 没有新请求）
    slot: Mutex<Option<(u32, u32)>>,
    /// 唤醒信号：容量 1，只用来叫醒消费者，不承载数据
    wake: flume::Sender<()>,
}

impl SshResizeCoalescer {
    /// 建一对「槽 + 唤醒信号」。返回值喂给 [`ResizeSink::Ssh`]，
    /// 接收端交给消费循环（见 `ssh.rs` 的输入泵）。
    pub fn new() -> (Arc<Self>, flume::Receiver<()>) {
        let (wake, wake_rx) = flume::bounded::<()>(1);
        (
            Arc::new(Self {
                slot: Mutex::new(None),
                wake,
            }),
            wake_rx,
        )
    }

    /// UI 线程调用：只写入最新值并尝试唤醒，**永不阻塞**。
    pub fn resize(&self, cols: u32, rows: u32) {
        *self.slot.lock() = Some((cols, rows));
        // 容量 1：已有信号待消费时返回 `Full`，那是正常情况（消费者马上会来取）
        let _ = self.wake.try_send(());
    }

    /// 消费端调用：取走待发送的最新尺寸（`None` = 当前无请求）。
    pub fn take(&self) -> Option<(u32, u32)> {
        self.slot.lock().take()
    }

    /// 还有没有未取走的请求（供测试断言）。
    pub fn is_pending(&self) -> bool {
        self.slot.lock().is_some()
    }
}

/// 本地 PTY 尺寸变更的异步执行器。
pub struct PtyResizer {
    tx: flume::Sender<(u16, u16)>,
    master: Arc<AsyncMasterPty>,
    /// 已提交未完成的请求数（供测试/诊断等待排空）
    pending: Arc<std::sync::atomic::AtomicUsize>,
    /// 最近一次实际 resize 的耗时（微秒，供诊断）
    last_cost_us: Arc<std::sync::atomic::AtomicU64>,
}

impl PtyResizer {
    /// 启动执行线程。线程随进程存活（会话结束后自动退出）。
    pub fn spawn(master: Arc<AsyncMasterPty>) -> Arc<Self> {
        let (tx, rx) = flume::unbounded::<(u16, u16)>();
        let pending = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let last_cost_us = Arc::new(std::sync::atomic::AtomicU64::new(0));

        let worker_master = master.clone();
        let worker_pending = pending.clone();
        let worker_cost = last_cost_us.clone();
        let _ = std::thread::Builder::new()
            .name("rs-pty-resize".into())
            .spawn(move || {
                while let Ok(first) = rx.recv() {
                    // 合并积压：只执行队列里最后那个尺寸
                    let mut latest = first;
                    let mut drained = 0usize;
                    while let Ok(next) = rx.try_recv() {
                        latest = next;
                        drained += 1;
                    }
                    let t0 = std::time::Instant::now();
                    let _ = worker_master.resize(PtySize {
                        rows: latest.1,
                        cols: latest.0,
                        pixel_width: 0,
                        pixel_height: 0,
                    });
                    let cost = t0.elapsed();
                    worker_cost.store(cost.as_micros() as u64, Ordering::Relaxed);
                    if cost > std::time::Duration::from_secs(1) {
                        // 现场留证：说明 conhost 侧确实卡了很久，但 UI 线程不受影响
                        log::warn!(
                            "ConPTY resize 阻塞 {cost:?}（{}x{}）——已隔离在后台线程，UI 未冻结",
                            latest.0,
                            latest.1
                        );
                    }
                    worker_pending.fetch_sub(drained + 1, Ordering::Relaxed);
                }
            });

        Arc::new(Self {
            tx,
            master,
            pending,
            last_cost_us,
        })
    }

    /// UI 线程安全：只入队，不阻塞、不触碰任何句柄。
    pub fn resize(&self, cols: u16, rows: u16) {
        self.pending.fetch_add(1, Ordering::Relaxed);
        if self.tx.send((cols, rows)).is_err() {
            // 执行线程已退出（会话已关闭）
            self.pending.fetch_sub(1, Ordering::Relaxed);
        }
    }

    /// 仅供测试/诊断：等待队列排空（返回是否在超时前排空）。
    #[doc(hidden)]
    pub fn wait_idle(&self, timeout: std::time::Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            if self.pending.load(Ordering::Relaxed) == 0 {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        self.pending.load(Ordering::Relaxed) == 0
    }

    /// 仅供测试/诊断：读取 PTY 侧记录的当前尺寸。
    #[doc(hidden)]
    pub fn pty_size(&self) -> Option<(u16, u16)> {
        let size = self.master.get_size().ok()?;
        if size.cols == 0 && size.rows == 0 {
            None
        } else {
            Some((size.cols, size.rows))
        }
    }

    /// 仅供测试/诊断：最近一次 resize 的实际耗时。
    #[doc(hidden)]
    pub fn last_resize_cost(&self) -> std::time::Duration {
        std::time::Duration::from_micros(self.last_cost_us.load(Ordering::Relaxed))
    }
}

/// 本地 PTY master 的持有者，把 ConPTY 的阻塞式释放（`ClosePseudoConsole`）移出调用线程。
pub struct AsyncMasterPty {
    /// Drop 时用`Option::take`取走并交给后台线程，故为 Option。
    inner: Mutex<Option<Box<dyn MasterPty + Send>>>,
}

impl AsyncMasterPty {
    pub fn new(master: Box<dyn MasterPty + Send>) -> Self {
        Self {
            inner: Mutex::new(Some(master)),
        }
    }

    pub fn resize(&self, size: PtySize) -> anyhow::Result<()> {
        match self.inner.lock().as_ref() {
            Some(master) => master.resize(size),
            None => Ok(()),
        }
    }

    pub fn get_size(&self) -> anyhow::Result<PtySize> {
        match self.inner.lock().as_ref() {
            Some(master) => master.get_size(),
            None => Ok(PtySize {
                rows: 0,
                cols: 0,
                pixel_width: 0,
                pixel_height: 0,
            }),
        }
    }
}

impl Drop for AsyncMasterPty {
    fn drop(&mut self) {
        let taken = self.inner.lock().take();
        if let Some(master) = taken {
            // 释放可能阻塞数十秒~数分钟，必须离开调用线程（尤其是 UI 线程）。
            let _ = std::thread::Builder::new()
                .name("rs-pty-drop".into())
                .spawn(move || drop(master));
        }
    }
}

/// 共享退出标志：SessionHandle 释放时置位，各泵线程/任务据此退出
#[derive(Debug, Default, Clone)]
pub struct ExitFlag(pub Arc<AtomicBool>);

impl ExitFlag {
    pub fn raised(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// 供 UI 持有的会话句柄。writer/reader 在创建 rustshell-terminal 视图时被 move 走。
pub struct SessionHandle {
    pub id: SessionId,
    pub kind: SessionKind,
    pub title: String,
    pub profile_id: Option<String>,
    pub state: SessionState,
    pub writer: Option<PipeWriter>,
    pub reader: Option<PipeReader>,
    pub resize: ResizeSink,
    pub events_rx: flume::Receiver<SessionEvent>,
    pub exit: ExitFlag,
    /// 本会话专属指标存储（SSH 远端采样写入）
    pub metrics: Option<Arc<crate::metrics::MetricsStore>>,
    /// SFTP 命令通道（仅 SSH 会话；M5 文件面板）
    pub sftp_tx: Option<flume::Sender<crate::sftp::SftpCmd>>,
    /// 子进程杀手（仅本地会话）。关闭会话时用来终止 shell，否则每开一次终端就残留一个 PowerShell/cmd 进程。
    pub child_killer: Option<Box<dyn ChildKiller + Send + Sync>>,
    /// 端口转发隧道（仅 SSH 会话，F-3）。
    ///
    /// **放在 `SessionHandle` 上而不是 `MainView` 字段里**：隧道的生命周期必须
    /// 严格等于会话 —— 会话句柄一释放，槽位跟着析构，监听随之关闭。挂在 UI 上
    /// 就要额外维护「Tab 关了 / 会话断了要记得清隧道」，而漏清的表现是
    /// 「一个已经没人在看的本地端口继续接受连接」。
    pub forwards: Vec<Arc<crate::forward::ForwardSlot>>,
    /// 会话日志（F-7）。`None` = 本会话不记录（开关关着，或文件建不出来）。
    ///
    /// 和 `forwards` 同一个道理：**挂在句柄上**，句柄一释放文件就关。
    /// 这是**唯一**需要落盘的状态 —— 输出泵只管往里写，UI 若要展示「正在记录」
    /// 就读它（`path()` / `is_active()`），不另建一份「哪些会话在记录」的名单。
    pub log: Option<Arc<Mutex<crate::session_log::SessionLogger>>>,
}

impl Drop for SessionHandle {
    fn drop(&mut self) {
        self.exit.0.store(true, Ordering::Relaxed);
        // 终止本地 shell 子进程（SSH 会话无子进程，恒为 None）。
        // TerminateProcess 是异步且极快的，可留在当前线程（尽早让子进程消失）。
        // 注意：本函数在 UI 线程上被同步调用（关 Tab / 关窗口），因此这里
        // 不允许出现任何长阻塞操作。`resize`持有的 ConPTY master 在
        // 析构时会调用阻塞式`ClosePseudoConsole`，已由`AsyncMasterPty`
        // 把该动作转交后台线程，见其文档（实测可阻塞 187s）。
        if let Some(killer) = self.child_killer.as_mut() {
            let _ = killer.kill();
        }
        self.state = SessionState::Closed;
    }
}

impl SessionHandle {
    /// 构造一个「占位会话」：状态为`Connecting`，无 PTY、无通道。连接请求一发出，就立即在标签栏开出一个 Tab，让用户马上看到「正在连接...」。
    pub fn pending(
        id: SessionId,
        kind: SessionKind,
        title: String,
        profile_id: Option<String>,
    ) -> Self {
        // 事件通道：占位期不产生任何事件，发送端直接丢弃
        let (_no_event_tx, events_rx) = flume::unbounded::<SessionEvent>();
        // resize 槽：唤醒信号的接收端随之丢弃，so `resize()` 只写内存槽、永不阻塞
        let (dead_resize, _no_resize_rx) = SshResizeCoalescer::new();
        Self {
            id,
            kind,
            title,
            profile_id,
            state: SessionState::Connecting,
            writer: None,
            reader: None,
            resize: ResizeSink::Ssh(dead_resize),
            events_rx,
            exit: ExitFlag::default(),
            metrics: None,
            sftp_tx: None,
            child_killer: None,
            forwards: Vec::new(),
            log: None,
        }
    }
}

/// 本地 Shell 类型：zsh / bash / PowerShell。
fn default_shell_command() -> CommandBuilder {
    #[cfg(target_os = "windows")]
    {
        let mut candidates: Vec<std::path::PathBuf> = Vec::new();
        if let Some(sysroot) = std::env::var_os("SystemRoot") {
            let sysroot = std::path::PathBuf::from(sysroot);
            candidates.push(sysroot.join("System32\\WindowsPowerShell\\v1.0\\powershell.exe"));
            candidates.push(sysroot.join("System32\\cmd.exe"));
        }
        if let Some(path_var) = std::env::var_os("PATH") {
            for dir in std::env::split_paths(&path_var) {
                candidates.push(dir.join("pwsh.exe"));
                candidates.push(dir.join("powershell.exe"));
            }
        }

        let picked = candidates.iter().find(|p| p.is_file());
        if let Some(prog) = picked {
            log::info!("本地 Shell 选择: {}", prog.display());
            let mut cmd = CommandBuilder::new(prog);
            if prog.extension().is_some_and(|e| e == "exe")
                && prog
                    .file_name()
                    .is_some_and(|f| f.to_string_lossy().contains("powershell"))
            {
                cmd.arg("-NoLogo");
            }
            if let Some(home) = user_home_dir() {
                cmd.cwd(home);
            }
            return cmd;
        }
        log::warn!("未找到 powershell/cmd，回退裸 powershell.exe");
        let mut cmd = CommandBuilder::new("powershell.exe");
        cmd.arg("-NoLogo");
        if let Some(home) = user_home_dir() {
            cmd.cwd(home);
        }
        cmd
    }
    #[cfg(not(target_os = "windows"))]
    {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into());
        CommandBuilder::new(shell)
    }
}

/// 用户主目录（Windows: %USERPROFILE%；Unix: $HOME）
fn user_home_dir() -> Option<std::path::PathBuf> {
    std::env::var_os(if cfg!(target_os = "windows") {
        "USERPROFILE"
    } else {
        "HOME"
    })
    .map(std::path::PathBuf::from)
    .filter(|p| p.is_dir())
}

/// 本地 Shell 会话的标题（Windows 恒为 `PowerShell`，其它平台取 `$SHELL` 的末段）。
///
/// **导出给 UI**：会话日志的文件名（F-7）要用它，而标题是在这里定的 ——
/// 让 UI 自己再猜一遍必然分叉（本地会话的日志会写成别的名字）。
pub fn local_session_title() -> String {
    if cfg!(target_os = "windows") {
        "PowerShell".to_string()
    } else {
        std::env::var("SHELL")
            .map(|s| s.rsplit('/').next().unwrap_or("shell").to_string())
            .unwrap_or_else(|_| "shell".into())
    }
}

/// 启动本地 Shell 会话（rustshell-terminal 默认 PTY）
///
/// `log_path`：会话日志落点（F-7）。`None` = 不记录。**本地会话没有 profile**，
/// 所以开关只由全局设置决定，调用方算好路径传进来即可。
pub fn spawn_local_session(
    cols: u16,
    rows: u16,
    log_path: Option<std::path::PathBuf>,
) -> anyhow::Result<SessionHandle> {
    let t0 = std::time::Instant::now();
    let pty_system = native_pty_system();
    let pair = pty_system.openpty(PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    })?;
    let openpty_ms = t0.elapsed().as_millis();

    let mut cmd = default_shell_command();
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");

    let child: Box<dyn Child + Send + Sync> = match pair.slave.spawn_command(cmd) {
        Ok(c) => c,
        Err(e) => {
            log::error!("本地 Shell spawn 失败: {e}");
            return Err(anyhow::anyhow!("启动本地 Shell 失败: {e}"));
        }
    };
    // 在 child 被收割线程接管前先取出杀手句柄：
    // 会话 Drop 时用它终止 shell，避免「关 Tab 后 PowerShell 常驻」。
    let child_killer = child.clone_killer();
    log::info!(
        "本地 Shell spawn 分阶段耗时: openpty={openpty_ms}ms, spawn={}ms（之后为 shell 自身初始化）",
        t0.elapsed().as_millis()
    );
    // unix 下必须丢弃 slave，否则 master 读不到 EOF
    #[cfg(not(target_os = "windows"))]
    drop(pair.slave);

    let pty_writer = pair
        .master
        .take_writer()
        .map_err(|e| anyhow::anyhow!("take pty writer: {e}"))?;
    let pty_reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| anyhow::anyhow!("clone pty reader: {e}"))?;
    // master 交给异步释放器；resize 走 PtyResizer（ResizePseudoConsole 是跨进程同步调用，不能落 UI 线程）
    let master: Arc<AsyncMasterPty> = Arc::new(AsyncMasterPty::new(pair.master));
    let resizer = PtyResizer::spawn(master);

    let id = crate::connections::uuid_like();
    let title = local_session_title();

    build_session_pipes(PipesSpec {
        id,
        kind: SessionKind::Local,
        title,
        profile_id: None,
        resize: ResizeSink::Local(resizer),
        out_source: OutSource::Std(pty_reader),
        in_sink: InSink::Std(pty_writer),
        _child: Some(child),
        child_killer: Some(child_killer),
        // 建不出日志文件就降级为「不记录」，绝不让终端起不来（F-7 的硬要求）
        log: crate::session_log::open_if(log_path.as_deref()),
        // 本地会话的泵是 std 线程、本就靠 `exit` 退出，就地建旗标即可（C-2）。
        exit: None,
    })
}

/// 输出源：本地 PTY 的阻塞 Read 或 SSH 输出通道
pub enum OutSource {
    Std(Box<dyn std::io::Read + Send>),
    Chan(flume::Receiver<Vec<u8>>),
}

/// 输入目标：本地 PTY 的阻塞 Write 或 SSH 输入通道
pub enum InSink {
    Std(Box<dyn std::io::Write + Send>),
    Chan(flume::Sender<Vec<u8>>),
}

impl InSink {
    fn write_all(&mut self, data: &[u8]) -> std::io::Result<()> {
        match self {
            InSink::Std(w) => w.write_all(data),
            InSink::Chan(tx) => tx
                .send(data.to_vec())
                .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "chan closed")),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            InSink::Std(w) => w.flush(),
            InSink::Chan(_) => Ok(()),
        }
    }
}

pub(crate) struct PipesSpec {
    pub id: SessionId,
    pub kind: SessionKind,
    pub title: String,
    pub profile_id: Option<String>,
    pub resize: ResizeSink,
    pub out_source: OutSource,
    pub in_sink: InSink,
    pub _child: Option<Box<dyn Child + Send + Sync>>,
    /// 子进程杀手（本地会话用；SSH 传 None）
    pub child_killer: Option<Box<dyn ChildKiller + Send + Sync>>,
    /// 会话日志（F-7）。`None` = 不记录。由调用方按「全局开关 + 连接级覆盖」算好后传入；
    /// `build_session_pipes` 只负责把它交给输出泵线程**并**留在句柄上。
    pub log: Option<Arc<Mutex<crate::session_log::SessionLogger>>>,
    /// 外部预置的退出旗标（C-2，2026-09-28）。
    ///
    /// 由**调用方**（目前只有 SSH 的 `establish`）在起自己的 tokio 泵之前先建好，
    /// 好让那些泵也能观察同一个旗标。`None` = 这里自己建一个（本地会话的默认路径）。
    ///
    /// 为什么需要它：SSH 的输入/输出泵是 `tokio::spawn` 的 detached 任务，
    /// 它们的退出条件原本只挂在 `in_rx` / 输出信道关闭上 —— 而 `in_tx`
    /// 由 `SessionHandle.writer` 持有、会随 UI 侧 clone 一起活过会话本身，
    /// 于是输入泵可能在会话已死后长期空转，白占一条 worker（叠加 C-1 加速瘫痪）。
    /// 有了这个旗标，`establish` 里的泵可以 `select!` 上它，句柄一 drop 就退。
    pub exit: Option<ExitFlag>,
}

/// 构建 Pipe 对并启动双向泵线程（Core 层不依赖任何 UI）
pub(crate) fn build_session_pipes(spec: PipesSpec) -> anyhow::Result<SessionHandle> {
    let (term_writer, input_rx) = crate::stream::writer_pipe();
    let (term_reader, output_tx) = crate::stream::reader_pipe();
    let (events_tx, events_rx) = flume::unbounded::<SessionEvent>();
    // 调用方预置优先（SSH 先建、好让自己的 tokio 泵也用同一个）；否则就地新建。
    let exit = spec.exit.unwrap_or_default();

    // 输出泵：源 到 终端
    let mut out_source = spec.out_source;
    let out_exit = exit.clone();
    // 会话日志的 **唯一** tee 点（F-7）：SSH 与本地 Shell 的输出都要经过这个线程
    // （SSH 的 `ChannelMsg` 先进 `out_tx`，再由这里转给终端），所以在这一处接就够了。
    // 两路各接一次不仅多余，还容易漏掉一条 —— 表现就是「这台机器有日志、那台没有」。
    let out_log = spec.log.clone();
    let out_join: JoinHandle<()> = std::thread::Builder::new()
        .name(format!("rs-out-{}", spec.id))
        .spawn(move || {
            loop {
                if out_exit.raised() {
                    break;
                }
                let chunk: Option<Vec<u8>> = match &mut out_source {
                    OutSource::Std(r) => {
                        let mut buf = [0u8; 8192];
                        match r.read(&mut buf) {
                            Ok(0) => None, // EOF
                            Ok(n) => Some(buf[..n].to_vec()),
                            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                            Err(_) => None,
                        }
                    }
                    OutSource::Chan(rx) => {
                        match rx.recv_timeout(std::time::Duration::from_millis(500)) {
                            Ok(v) => Some(v),
                            Err(flume::RecvTimeoutError::Timeout) => continue,
                            Err(flume::RecvTimeoutError::Disconnected) => None,
                        }
                    }
                };
                match chunk {
                    Some(v) => {
                        // 先落盘、再送终端：**顺序有意义**。终端侧销毁时下面会 break，
                        // 若把落盘放在后面，最后一段输出（往往正是「为什么断了」的现场）
                        // 就永远进不了日志。日志写失败由 logger 自己吞掉（见 session_log），
                        // 这里连错误都没有可传播的东西。
                        if let Some(lg) = &out_log {
                            lg.lock().write(&v);
                        }
                        if output_tx.send(v).is_err() {
                            break; // 终端已销毁
                        }
                    }
                    None => break, // EOF / 源关闭
                }
            }
            let _ = events_tx.send(SessionEvent::Closed);
        })?;

    // 输入泵：终端 到 目标
    let in_sink = spec.in_sink;
    let in_exit = exit.clone();
    let in_join: JoinHandle<()> = std::thread::Builder::new()
        .name(format!("rs-in-{}", spec.id))
        .spawn(move || {
            let mut sink = in_sink;
            loop {
                if in_exit.raised() {
                    break;
                }
                match input_rx.recv_timeout(std::time::Duration::from_millis(500)) {
                    Ok(data) => {
                        if sink.write_all(&data).is_err() {
                            break;
                        }
                        let _ = sink.flush();
                    }
                    Err(flume::RecvTimeoutError::Timeout) => continue,
                    Err(flume::RecvTimeoutError::Disconnected) => break,
                }
            }
        })?;

    // 子进程收割线程（防止僵尸进程）
    if let Some(mut child) = spec._child {
        std::thread::Builder::new()
            .name(format!("rs-wait-{}", spec.id))
            .spawn(move || {
                let _ = child.wait();
            })?;
    }

    // 线程 detach 自行运行（JoinHandle 丢弃即为 detach 语义）
    drop(out_join);
    drop(in_join);

    Ok(SessionHandle {
        id: spec.id,
        kind: spec.kind,
        title: spec.title,
        profile_id: spec.profile_id,
        state: SessionState::Connected,
        writer: Some(term_writer),
        reader: Some(term_reader),
        resize: spec.resize,
        events_rx,
        exit,
        metrics: None,
        sftp_tx: None,
        child_killer: spec.child_killer,
        forwards: Vec::new(),
        log: spec.log,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ★ 回归护栏（2026-09 修复「拖拽窗口时整窗冻结」）：
    /// UI 线程上的 `resize()` **在任何情况下都不能阻塞**。
    ///
    /// 旧实现是 `flume::bounded(8)` + UI 线程 `send`：消费端每次都要 await 一个
    /// SSH `window_change` 网络往返，队列积满后 `send` 就挂在 UI 线程上。
    /// 这里用「接收端完全不收」的极端情况验证新实现仍然是纯内存写入：
    /// 连续 10 000 次 resize 必须在毫秒级完成。
    #[test]
    fn ssh_resize_never_blocks_ui_thread() {
        let (coalescer, _rx) = SshResizeCoalescer::new();

        // 不消费接收端：唤醒信号容量 1 立刻满，后续 try_send 全部返回 Full
        let t0 = std::time::Instant::now();
        for i in 0..10_000u32 {
            coalescer.resize(80 + (i % 100), 24);
        }
        let elapsed = t0.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "10 000 次 resize 必须接近瞬时完成，实测 {elapsed:?}"
        );

        // 只有「最新值」被保留：中间过程可以丢，远端只关心最终窗口大小
        assert_eq!(coalescer.take(), Some((80 + 99, 24)));
        assert_eq!(coalescer.take(), None, "取走后不应残留");
        assert!(!coalescer.is_pending());
    }

    /// 唤醒信号被消费后，新请求必须能再次唤醒消费者 ——
    /// 否则拖拽停止后的最后一个尺寸会永远躺在槽里发不出去。
    #[test]
    fn ssh_resize_rewakes_after_drain() {
        let (coalescer, rx) = SshResizeCoalescer::new();

        coalescer.resize(100, 30);
        assert_eq!(coalescer.take(), Some((100, 30)));
        assert!(rx.try_recv().is_ok(), "首次请求必须发出唤醒信号");
        assert!(rx.try_recv().is_err(), "只发一个信号");

        // 消费端取走后，下一个请求要能重新唤醒
        coalescer.resize(120, 40);
        assert!(rx.try_recv().is_ok(), "槽被取空后必须能重新唤醒消费者");
        assert_eq!(coalescer.take(), Some((120, 40)));
    }

    /// 接收端被丢弃（会话结束）后，`resize()` 仍必须安全且不阻塞。
    #[test]
    fn ssh_resize_survives_dropped_receiver() {
        let (coalescer, rx) = SshResizeCoalescer::new();
        drop(rx);
        for _ in 0..1000 {
            coalescer.resize(80, 24);
        }
        assert!(coalescer.is_pending());
    }

    /// 占位会话（`pending`）的 resize 也必须不阻塞：它用的是同一套合并槽。
    #[test]
    fn pending_session_resize_is_non_blocking() {
        let handle = SessionHandle::pending("t".into(), SessionKind::Ssh, "title".into(), None);
        let t0 = std::time::Instant::now();
        for _ in 0..10_000 {
            handle.resize.resize(80, 24);
        }
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(1),
            "占位会话的 resize 也必须接近瞬时"
        );
    }

    /// 用 `OutSource::Chan` 造一个不碰 PTY 的会话（F-7 的 tee 用例用）。
    /// 返回句柄与「往输出源喂数据」的发送端。
    fn piped_session(
        id: &str,
        log: Option<Arc<Mutex<crate::session_log::SessionLogger>>>,
    ) -> (SessionHandle, flume::Sender<Vec<u8>>) {
        let (coalescer, _rx) = SshResizeCoalescer::new();
        let (feed_tx, feed_rx) = flume::bounded::<Vec<u8>>(8);
        let (in_tx, _in_rx) = flume::bounded::<Vec<u8>>(8);
        let session = build_session_pipes(PipesSpec {
            id: id.to_string(),
            kind: SessionKind::Local,
            title: "tee".into(),
            profile_id: None,
            resize: ResizeSink::Ssh(coalescer),
            out_source: OutSource::Chan(feed_rx),
            in_sink: InSink::Chan(in_tx),
            _child: None,
            child_killer: None,
            log,
            exit: None,
        })
        .expect("build pipes");
        (session, feed_tx)
    }

    /// 轮询等文件内容出现（输出泵是独立线程、且对 `Chan` 有 500ms 轮询节拍）。
    fn wait_for_file(path: &std::path::Path, want: usize) -> Vec<u8> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let got = std::fs::read(path).unwrap_or_default();
            if got.len() >= want || std::time::Instant::now() > deadline {
                return got;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// ★ F-7：输出泵是会话日志的**唯一** tee 点 —— 旁路落盘、原样落盘。
    ///
    /// 判据是「写进去的字节必须逐字节等于喂进去的」，因为记录的是**原始字节流**
    /// （含转义序列），任何「顺手解码/清洗」都会毁掉日志的取证价值。
    ///
    /// 注意本用例**不**断言「句柄释放后立刻封口」：输出泵与句柄共享同一个 logger
    /// （`Arc`），停止信号置位时泵可能正卡在 500ms 的 `recv_timeout` 里，
    /// 于是还会补写最后一笔。这是**想要**的行为 —— 会话收尾时的那段输出往往正是
    /// 「为什么断了」的现场，不该因为句柄先走而被丢掉。所以封口时机由泵决定，
    /// 而不是由句柄的生死决定。
    #[test]
    fn session_log_tees_the_output_pump_to_disk() {
        let dir = std::env::temp_dir().join("rustshell-session-log-tee");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("120000-tee.log");

        let log = crate::session_log::open_if(Some(&path)).expect("logger");
        let (session, feed) = piped_session("tee-1", Some(log));
        // 句柄上必须挂着 logger：UI 的「正在记录」只能读这里，不能另记一份
        assert!(session.log.is_some(), "句柄必须带着日志句柄");

        let payload = b"\x1b[32mok\x1b[0m \xe4\xb8\xad\xe6\x96\x87\r\n".to_vec();
        feed.send(payload.clone()).unwrap();
        let got = wait_for_file(&path, payload.len());
        assert_eq!(got, payload, "日志必须是喂进去的原始字节");

        // 分多笔写入是追加而不是覆盖
        feed.send(b"tail".to_vec()).unwrap();
        let got = wait_for_file(&path, payload.len() + 4);
        assert!(got.starts_with(&payload), "后一笔不得覆盖前一笔");
        assert_eq!(got, [payload.clone(), b"tail".to_vec()].concat());

        drop(session);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ★ F-7：关掉开关时**一个文件都不该产生**。
    /// 「不记录」必须是「没有文件」，而不是「文件一直是空的」——
    /// 空文件会让用户以为记录开了却没内容。
    #[test]
    fn session_log_off_produces_no_file() {
        let dir = std::env::temp_dir().join("rustshell-session-log-off");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let (session, feed) = piped_session("off-1", None);
        assert!(session.log.is_none());
        feed.send(b"hello".to_vec()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(700));
        drop(session);

        let leftovers: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().collect();
        assert!(
            leftovers.is_empty(),
            "关闭记录时不得产生文件：{:?}",
            leftovers.iter().map(|e| e.path()).collect::<Vec<_>>()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
