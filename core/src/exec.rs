//! 一次性远程命令执行（SSH exec 信道，**无 PTY**）。
//!
//! 为什么需要它：巡检与批量命令的诉求是「跑一条命令、拿输出、看退出码」。
//! 走交互式 PTY 会话有三个绕不过去的问题（见需求文档 §4.1）：
//! 1. PTY 会渲染提示符、回显、ANSI 与分页 —— 命中数会被提示符污染；
//! 2. 每台一个 `SessionHandle`（两个 tokio 泵 + 指标任务 + SFTP 服务），
//!    30 台就是 30 条常驻连接与 30 个采样循环；
//! 3. **没有退出码语义** —— 无法区分「命令没输出」与「命令失败」。
//!
//! 因此本模块直接开 exec 信道：跑完即断，逐台独立。

use crate::connections::ConnectionProfile;
use crate::proxy::ProxyConfig;
use crate::ssh::{connect_only, HostKeyPolicy, CONNECT_TIMEOUT_MAX_SECS, CONNECT_TIMEOUT_MIN_SECS};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 单台 stdout / stderr 各自的保留上限（字节）。
///
/// 超出的部分按「首尾各半 + 中间省略标记」截断：`cat` 一个大文件不该把内存打爆，
/// 而巡检恰恰需要同时看到开头（上下文）和结尾（最近的错误）。
pub const MAX_STREAM_BYTES: usize = 64 * 1024;

/// 批量执行的默认并发上限。
///
/// 取 4 而不是 8/16：运维机器的 SSH 服务端常有 `MaxStartups` 限制，
/// 一上来就把并发拉满很容易被服务端拒绝（表现为「莫名其妙连不上」）。
pub const DEFAULT_CONCURRENCY: usize = 4;
/// 并发上限的硬顶（任务里可调范围的上界）。
pub const MAX_CONCURRENCY: usize = 16;

/// 一次 exec 的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecResult {
    /// 远端退出码；超时或信道异常时为 `None`
    pub exit_code: Option<u32>,
    pub stdout: String,
    pub stderr: String,
    /// 是否因超时被中止
    pub timed_out: bool,
    /// 用户取消（批量停止）
    pub cancelled: bool,
    pub elapsed_ms: u64,
}

impl ExecResult {
    /// 命令是否成功（退出码 0）。超时/取消一律不算成功。
    pub fn is_ok(&self) -> bool {
        !self.timed_out && !self.cancelled && self.exit_code == Some(0)
    }

    /// 失败原因（用于运行记录的一行摘要）。
    pub fn failure_reason(&self) -> Option<String> {
        if self.cancelled {
            return Some(crate::i18n::ts("已取消".into(), "cancelled".into()));
        }
        if self.timed_out {
            return Some(crate::i18n::ts("命令超时".into(), "timed out".into()));
        }
        match self.exit_code {
            Some(0) => None,
            Some(c) => Some(crate::i18n::ts(format!("退出码 {c}"), format!("exit {c}"))),
            // 没有退出码时，stderr 往往是唯一的线索（连接失败、握手报错都写在那里），
            // 只回「未取得退出码」会让排障无从下手。
            None => Some(match first_stderr_line(&self.stderr) {
                Some(detail) => crate::i18n::ts(
                    format!("未取得退出码：{detail}"),
                    format!("no exit status: {detail}"),
                ),
                None => crate::i18n::ts("未取得退出码".into(), "no exit status".into()),
            }),
        }
    }

    /// 供审计 / 运行记录的一行摘要。
    pub fn summary(&self) -> String {
        match self.failure_reason() {
            None => crate::i18n::ts("成功".into(), "ok".into()),
            Some(r) => r,
        }
    }
}

/// 可注入的执行后端。
///
/// 定时任务与 agent 都只依赖这个 trait，于是：
/// - 生产环境注入 [`SshExec`]；
/// - 单测注入假后端，把「批量 / 超时 / 取消 / 审计」这些逻辑测穿，
///   而不需要一台真的 SSH 服务器。
pub trait ExecBackend: Send + Sync {
    fn exec(&self, profile: &ConnectionProfile, cmd: &str, timeout: Duration) -> ExecResult;

    /// 全局代理（F-10）的运行期形态。
    ///
    /// 放在这里而不是让调用方各带一份：SFTP 一次性下载等**旁路建连**与 exec 是
    /// 同一件事的两面（都要先到那台机器），而后端正是「怎么到达服务器」的载体。
    /// 假后端与直连场景取默认实现（`None`）—— 这也是默认值唯一该被用到的地方：
    /// 生产后端 [`SshExec`] 必须覆写它，否则「会话走代理、下载直连」会静默发生。
    fn global_proxy(&self) -> Option<&ProxyConfig> {
        None
    }
}

/// 生产后端：每次调用新建一条 SSH 连接，跑完即断。
///
/// `global_proxy`（F-10）是**全局**代理（运行期形态）；「本条连接该不该走代理」
/// 由 [`connect_only`] 按连接级三态合成 —— 后端只负责把全局值带过去。
/// 这里存的是值而不是 `Arc`：后端实例随一次运行创建，克隆一次的代价可以忽略，
/// 而共享引用会把「运行期配置」变成跨运行的可变状态。
#[derive(Debug, Default, Clone)]
pub struct SshExec {
    pub global_proxy: Option<ProxyConfig>,
}

impl ExecBackend for SshExec {
    fn exec(&self, profile: &ConnectionProfile, cmd: &str, timeout: Duration) -> ExecResult {
        exec_on(profile, cmd, timeout, self.global_proxy.as_ref())
    }

    fn global_proxy(&self) -> Option<&ProxyConfig> {
        self.global_proxy.as_ref()
    }
}

/// `Arc<T>` 也是 `ExecBackend`。
///
/// 让调用方持有一份「可替换的执行后端」（生产 [`SshExec`]、测试假后端），
/// 同时能按值交给 [`exec_batch_with`] —— 那个函数要求 `'static` 的所有权，
/// 没有这个 impl 就得在每个使用点手工解引用再克隆一层 Arc。
impl<T: ExecBackend + ?Sized> ExecBackend for Arc<T> {
    fn exec(&self, profile: &ConnectionProfile, cmd: &str, timeout: Duration) -> ExecResult {
        (**self).exec(profile, cmd, timeout)
    }

    /// 必须显式转发：默认实现会返回 `None`，而 `Arc<SshExec>` 里的代理是**有的** ——
    /// 少这一行就是「下载绕开代理」，且不会有任何报错。
    fn global_proxy(&self) -> Option<&ProxyConfig> {
        (**self).global_proxy()
    }
}

/// 阻塞执行一次远程命令（内部 `block_on` 全局 tokio 运行时）。
///
/// 调用方必须在**后台线程**里调用（UI 线程禁止任何 SSH 阻塞操作）。
///
/// `global_proxy`：全局代理的运行期形态（F-10）。生产路径从设置里取
/// （[`SshExec`] 带着它）；测试与一次性脚本传 `None` 即直连。
///
/// **不写审计**：审计需要落盘路径，而本函数保持纯净以便复用与单测。
/// 生产路径走 [`exec_on_audited`]（批量执行内部已统一接线）。
pub fn exec_on(
    profile: &ConnectionProfile,
    cmd: &str,
    timeout: Duration,
    global_proxy: Option<&ProxyConfig>,
) -> ExecResult {
    exec_on_cancellable(profile, cmd, timeout, &AtomicBool::new(false), global_proxy)
}

/// 同上，但执行后写一条 `agent-audit.log`（AC-29）。
///
/// `kind` 用 `"exec"` / `"task_exec"` / `"agent_tool"` 区分来源，
/// 便于事后按来源筛查。**无论成功失败都写** —— 审计要的正是「做过什么」，
/// 而不是「成功过什么」。
pub fn exec_on_audited(
    profile: &ConnectionProfile,
    cmd: &str,
    timeout: Duration,
    audit_path: &std::path::PathBuf,
    kind: &str,
    global_proxy: Option<&ProxyConfig>,
) -> ExecResult {
    let res = exec_on(profile, cmd, timeout, global_proxy);
    audit_exec(audit_path, kind, profile, cmd, &res);
    res
}

/// 追加一条 exec 审计。
///
/// 风险级在这里**重算**而不是让调用方传：审计记录必须反映真实判定，
/// 而从调用方传入就多了一条「传错/传旧」的可能 —— 审计的价值全在可信。
pub fn audit_exec(
    audit_path: &std::path::PathBuf,
    kind: &str,
    profile: &ConnectionProfile,
    cmd: &str,
    res: &ExecResult,
) {
    let risk = crate::ai::assess_command(cmd);
    // `audit_log` 内部已做 `redact`；这里补上目标与退出码。
    // 命令本身可能含敏感参数，故走脱敏后的形态。
    let target = profile.summary_line();
    let outcome = res.exit_code.map(|c| c.to_string()).unwrap_or_else(|| {
        if res.timed_out {
            "timeout".into()
        } else if res.cancelled {
            "cancelled".into()
        } else {
            "-".into()
        }
    });
    // 复用既有 `audit_log` 的字段（kind/risk/command/executed），
    // 把目标与退出码拼进命令字段的可读前缀里，避免改动既有日志 schema
    // 而让下游解析脚本失效。
    let annotated = format!("[{target}] {cmd} ⇒ exit={outcome}");
    crate::ai::audit_log(audit_path, kind, &annotated, risk, res.is_ok());
}

/// 同 [`exec_on`]，额外接受取消旗标（批量停止时复用同一个旗标）。
pub fn exec_on_cancellable(
    profile: &ConnectionProfile,
    cmd: &str,
    timeout: Duration,
    cancel: &AtomicBool,
    global_proxy: Option<&ProxyConfig>,
) -> ExecResult {
    let start = Instant::now();
    let out =
        crate::ssh::runtime().block_on(exec_async(profile, cmd, timeout, cancel, global_proxy));
    let mut res = out;
    res.elapsed_ms = start.elapsed().as_millis() as u64;
    res
}

async fn exec_async(
    profile: &ConnectionProfile,
    cmd: &str,
    timeout: Duration,
    cancel: &AtomicBool,
    global_proxy: Option<&ProxyConfig>,
) -> ExecResult {
    if cancel.load(Ordering::Relaxed) {
        return ExecResult {
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
            timed_out: false,
            cancelled: true,
            elapsed_ms: 0,
        };
    }

    // 连接超时按配置（与交互式会话同一个设置项），命令超时按调用方给的。
    let connect_timeout = Duration::from_secs(
        timeout
            .as_secs()
            .clamp(CONNECT_TIMEOUT_MIN_SECS, CONNECT_TIMEOUT_MAX_SECS),
    );

    // 无人值守：未知主机密钥直接拒绝（没有人能回答信任对话框）
    let handle = match connect_only(
        profile,
        connect_timeout,
        HostKeyPolicy::RejectUnknown,
        global_proxy,
    )
    .await
    {
        Ok(h) => h,
        Err(e) => return failure(e),
    };

    let channel = match handle.channel_open_session().await {
        Ok(c) => c,
        Err(e) => return failure(format!("打开 exec 信道失败: {e}")),
    };

    let mut read_half = match channel.exec(true, cmd).await {
        Ok(()) => channel.split().0,
        Err(e) => return failure(format!("执行命令失败: {e}")),
    };

    // 收包循环放到独立任务里，主协程只做「等它完成 / 到点超时 / 被取消」三选一。
    //
    // 这样写的理由：收包本身是「读到 Close 为止」的不定长过程，把它和超时、
    // 取消混在一个 select 里，会让「取消后还剩多少包没收」变得难以推理。
    // 拆开之后，超时与取消都只做一件事 —— 放弃等待并关掉信道。
    let (tx, rx) = tokio::sync::oneshot::channel::<Drained>();
    tokio::spawn(async move {
        let mut d = Drained::default();
        while let Some(msg) = read_half.wait().await {
            if !absorb(&mut d, msg) {
                break;
            }
        }
        let _ = tx.send(d);
    });

    let mut drained = Drained::default();
    let mut timed_out = false;
    let mut cancelled = false;
    // 取消旗标是 AtomicBool（没有唤醒源），所以用 100ms 轮询兜底；
    // 这点开销相对一次 SSH 往返可以忽略。
    let poll = Duration::from_millis(100);
    tokio::select! {
        got = rx => {
            if let Ok(d) = got {
                drained = d;
            }
        }
        _ = tokio::time::sleep(timeout) => {
            timed_out = true;
        }
        _ = wait_cancelled(cancel, poll) => {
            cancelled = true;
        }
    }

    ExecResult {
        exit_code: if timed_out || cancelled {
            None
        } else {
            drained.exit_code
        },
        stdout: decode_stream(&drained.stdout),
        stderr: decode_stream(&drained.stderr),
        timed_out,
        cancelled,
        elapsed_ms: 0,
    }
}

/// 收包任务交回的结果。
#[derive(Default)]
struct Drained {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    exit_code: Option<u32>,
}

/// 把一条信道消息并入收集结果；返回 `false` 表示可以结束收包循环。
///
/// **不能拿 `Eof` 当结束标志**：RFC 4254 并未保证 `exit-status` 先于 `EOF`
/// 到达，OpenSSH 实测就存在「EOF 先到、exit-status 后到」的次序。早前这里
/// 在 `Eof | Close` 上 break，把随后的 `ExitStatus` 一起丢掉了，退出码变
/// `None`，每次巡检都被误判为「未取得退出码」——这正是 inspect_load
/// 连续失败的根因。现在只有真正的 `Close` 才终止。
fn absorb(d: &mut Drained, msg: russh::ChannelMsg) -> bool {
    match msg {
        russh::ChannelMsg::Data { data } => {
            if d.stdout.len() < MAX_STREAM_BYTES {
                d.stdout.extend_from_slice(&data);
            }
        }
        russh::ChannelMsg::ExtendedData { data, .. } => {
            if d.stderr.len() < MAX_STREAM_BYTES {
                d.stderr.extend_from_slice(&data);
            }
        }
        russh::ChannelMsg::ExitStatus { exit_status } => {
            d.exit_code = Some(exit_status);
        }
        russh::ChannelMsg::Close => return false,
        _ => {}
    }
    true
}

/// 每 `poll` 检查一次取消旗标，置位即返回。
///
/// 内置兜底上限（[`DEFAULT_EXEC_TIMEOUT_SECS`] + 余量）：即使调用方给了
/// 一个荒谬的长超时，这条 future 也不会永远挂着 —— 一个永不完成的 select
/// 分支本身不会出错，但会让「取消」在极端情形下失去意义。
async fn wait_cancelled(cancel: &AtomicBool, poll: Duration) {
    for _ in 0..CANCEL_POLL_ROUNDS {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        tokio::time::sleep(poll).await;
    }
    // 兜底：等满轮次后仍然把控制权交回 select（此时必然已超时或已收完）
    std::future::pending::<()>().await
}

/// 取消轮询的轮数上限（100ms × 1200 = 2 分钟）。
const CANCEL_POLL_ROUNDS: u32 = 1200;

fn failure(msg: String) -> ExecResult {
    ExecResult {
        exit_code: None,
        stdout: String::new(),
        stderr: msg,
        timed_out: false,
        cancelled: false,
        elapsed_ms: 0,
    }
}

/// stderr 的首个非空行（截到 120 字符），用作失败摘要的补充线索。
fn first_stderr_line(stderr: &str) -> Option<String> {
    stderr
        .lines()
        .map(|l| l.trim())
        .find(|l| !l.is_empty())
        .map(|l| l.chars().take(120).collect())
}

/// 批量执行句柄。
pub struct ExecBatchHandle {
    pub cancel: Arc<AtomicBool>,
    /// 已完成的目标数（轮询用）
    pub done: Arc<AtomicUsize>,
    pub total: usize,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl ExecBatchHandle {
    /// 请求停止：不再派发新目标，在途目标在下一次轮询点收尾。
    pub fn stop(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    pub fn done_count(&self) -> usize {
        self.done.load(Ordering::Relaxed)
    }

    pub fn is_finished(&self) -> bool {
        self.done_count() >= self.total
    }

    /// 等待全部 worker 自然结束（**不要**在 UI 线程调用）。
    ///
    /// 这里**不能**顺手置位 `cancel`：那会让 worker 一启动就退出，
    /// 表现为「批量跑完了但一个结果都没有」。要中断请显式调 [`Self::stop`]。
    pub fn join(self) {
        for t in self.threads {
            let _ = t.join();
        }
    }
}

/// 后果回调：每台完成时调用一次 `(profile_id, result)`。
pub type BatchSink = Arc<dyn Fn(&str, &ExecResult) + Send + Sync>;

/// 批量的调度参数（并发、超时、取消、审计）。
///
/// 打成结构体而不是摊开：摊平后函数有 9 个入参，而其中
/// `concurrency: usize` 与 `timeout: Duration` 位置写错编译器**不会**报错 ——
/// 只会让批量以错误的并发跑，或用一个荒谬的超时把任务挂住。
#[derive(Clone)]
pub struct BatchSpec {
    pub timeout: Duration,
    pub concurrency: usize,
    pub cancel: Arc<AtomicBool>,
    /// 审计落点（`None` = 不写；测试用）
    pub audit_path: Option<std::path::PathBuf>,
    /// 审计来源标签（`task_exec` / `agent_tool` / `exec`）
    pub audit_kind: &'static str,
}

impl BatchSpec {
    /// 默认：给定超时与取消旗标，并发取 [`DEFAULT_CONCURRENCY`]，不写审计。
    pub fn new(timeout: Duration, cancel: Arc<AtomicBool>) -> Self {
        Self {
            timeout,
            concurrency: DEFAULT_CONCURRENCY,
            cancel,
            audit_path: None,
            audit_kind: "exec",
        }
    }

    pub fn with_concurrency(mut self, n: usize) -> Self {
        self.concurrency = n;
        self
    }

    pub fn with_audit(mut self, path: std::path::PathBuf, kind: &'static str) -> Self {
        self.audit_path = Some(path);
        self.audit_kind = kind;
        self
    }
}

/// 在一组连接上执行同一条命令。
///
/// - 消费 `Vec<ConnectionProfile>`：口令等敏感字段用完即随本函数结束释放；
/// - `sink` 每台一调，让 UI 能**边跑边出**结果，而不是等整批跑完；
/// - `spec.audit_path` 非空时**每台**都留一条审计（含失败与取消）。
///   批量执行是最需要审计的场景 —— 一次点错就是几十台机器。
pub fn exec_batch_with<B, F>(
    backend: B,
    profiles: Vec<ConnectionProfile>,
    cmd: String,
    spec: BatchSpec,
    sink: F,
) -> ExecBatchHandle
where
    B: ExecBackend + 'static,
    F: Fn(&str, &ExecResult) + Send + Sync + 'static,
{
    let BatchSpec {
        timeout,
        concurrency,
        cancel,
        audit_path,
        audit_kind,
    } = spec;
    let total = profiles.len();
    let workers = concurrency.clamp(1, MAX_CONCURRENCY).min(total.max(1));
    // 队列里带原始下标：worker 取任务不保证顺序，但结果回调是按完成顺序的，
    // 下标留在这里是为了将来需要「按原序重排结果」时有依据。
    let queue = Arc::new(parking_lot::Mutex::new(
        profiles.into_iter().enumerate().collect::<Vec<_>>(),
    ));
    let done = Arc::new(AtomicUsize::new(0));
    let sink: BatchSink = Arc::new(sink);
    // 后端共享给所有 worker（`exec` 只取 `&self`，故用 Arc）
    let backend = Arc::new(backend);
    let audit_path: Option<Arc<std::path::PathBuf>> = audit_path.map(Arc::new);
    let mut threads = Vec::with_capacity(workers);

    for _ in 0..workers {
        let queue = queue.clone();
        let done = done.clone();
        let cancel = cancel.clone();
        let sink = sink.clone();
        let backend = backend.clone();
        let audit_path = audit_path.clone();
        let cmd = cmd.clone();
        threads.push(
            std::thread::Builder::new()
                .name("rs-exec-batch".into())
                .spawn(move || loop {
                    if cancel.load(Ordering::SeqCst) {
                        break;
                    }
                    let Some((_, profile)) = queue.lock().pop() else {
                        break;
                    };
                    let pid = profile.id.clone();
                    let res = if cancel.load(Ordering::SeqCst) {
                        ExecResult {
                            exit_code: None,
                            stdout: String::new(),
                            stderr: String::new(),
                            timed_out: false,
                            cancelled: true,
                            elapsed_ms: 0,
                        }
                    } else {
                        backend.exec(&profile, &cmd, timeout)
                    };
                    // 审计在 sink 之前：即使 sink 内部 panic，留痕也已经落盘。
                    if let Some(p) = &audit_path {
                        audit_exec(p, audit_kind, &profile, &cmd, &res);
                    }
                    // ★ 顺序是**计数在前、回调在后**（2026-09-28 修 C-3）。
                    //
                    // 反过来（先 `sink` 后 `fetch_add`）有一个致命窗口：`sink` 是
                    // **调用方提供的闭包**（UI 侧更新进度条），它一旦 panic，这个
                    // worker 线程当场终止，这一台的计数**永远不会 +1**。而
                    // `is_finished()`（判据 `done >= total`）据此判定「还没跑完」
                    // ⇒ 「批量执行」进度条永远停在 `< 100%`，剩余目标也再没人消费
                    // （若 panic 发生在最后一个并发槽里，剩下的都在队列里烂掉）。
                    //
                    // 先计数：即便回调炸了，这一台在**调度账本**上也算已收尾 ——
                    // 批量一定能跑到终态，UI 不会假死。
                    done.fetch_add(1, Ordering::SeqCst);
                    // 回调本身仍要防 panic 掀掉整条 worker：`catch_unwind` 兜住并
                    // 记日志。`sink` 是 `Arc<dyn Fn + Send + Sync>`，跨 `catch_unwind`
                    // 需要 `UnwindSafe` —— 闭包捕获的是 UI 的 `Entity`/句柄，用
                    // `AssertUnwindSafe` 明确承担「本次 panic 后不再复用该闭包的内部
                    // 状态」这一约定（回调只做通知，无需要求它自身可恢复）。
                    let sink_guard = std::panic::AssertUnwindSafe(|| sink(&pid, &res));
                    if std::panic::catch_unwind(sink_guard).is_err() {
                        log::error!(
                            "批量执行结果回调 panic（已忽略，计数照常推进）: \
                             target={pid} kind={audit_kind}"
                        );
                    }
                })
                .expect("spawn exec batch worker"),
        );
    }

    ExecBatchHandle {
        cancel,
        done,
        total,
        threads,
    }
}

/// 生产便捷入口：默认 SSH 后端。
///
/// `global_proxy` 与 [`exec_batch_with`] 的后端同一个口径：显式传入，
/// 让「这批命令走不走代理」在签名上就看得见。core 不读设置文件。
pub fn exec_batch<F>(
    profiles: Vec<ConnectionProfile>,
    cmd: String,
    spec: BatchSpec,
    global_proxy: Option<ProxyConfig>,
    sink: F,
) -> ExecBatchHandle
where
    F: Fn(&str, &ExecResult) + Send + Sync + 'static,
{
    exec_batch_with(SshExec { global_proxy }, profiles, cmd, spec, sink)
}

// 纯函数：截断与解码

/// 按 `max` 字节截断，超出时首尾各半并插入省略标记。
///
/// 切点会退到 UTF-8 字符边界：直接按字节切会把一个汉字劈成两半，
/// 结果是替换字符（`\u{FFFD}`）出现在本该可读的日志里。
pub fn truncate_stream(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let head_len = max / 2;
    let tail_len = max - head_len;
    let head_end = floor_char_boundary(s, head_len);
    let tail_start = ceil_char_boundary(s, s.len() - tail_len);
    let omitted = s.len().saturating_sub(head_end + (s.len() - tail_start));
    format!(
        "{}\n… (省略 {omitted} 字节) …\n{}",
        &s[..head_end],
        &s[tail_start..]
    )
}

/// 把字节流按终端编码解成文本，并按 [`MAX_STREAM_BYTES`] 截断。
///
/// 解码失败按 lossy 处理（替换字符）：巡检的输出只要能读，不能因为个别字节
/// 非法就让整段结果变成错误。
pub fn decode_stream(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    truncate_stream(&text, MAX_STREAM_BYTES)
}

/// 最大的不超过 `i` 的字符边界。
fn floor_char_boundary(s: &str, i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    let mut k = i;
    while k > 0 && !s.is_char_boundary(k) {
        k -= 1;
    }
    k
}

/// 最小的不小于 `i` 的字符边界。
fn ceil_char_boundary(s: &str, i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    let mut k = i;
    while k < s.len() && !s.is_char_boundary(k) {
        k += 1;
    }
    k
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::AtomicUsize;

    /// 测试用的批量入口：审计默认关闭。
    ///
    /// **刻意与生产函数同名**，在测试模块内遮蔽它 —— 生产签名新增了
    /// 「审计路径 + 来源」两个参数，而既有用例都不关心审计（审计本身由
    /// `audit_exec_writes_one_line_per_target` 单独覆盖）。不遮蔽的话，
    /// 每加一个参数都要改一遍全部用例，而那种改动没有任何验证价值。
    fn exec_batch_with(
        backend: impl ExecBackend + 'static,
        profiles: Vec<ConnectionProfile>,
        cmd: String,
        timeout: Duration,
        concurrency: usize,
        cancel: Arc<AtomicBool>,
        sink: impl Fn(&str, &ExecResult) + Send + Sync + 'static,
    ) -> ExecBatchHandle {
        super::exec_batch_with(
            backend,
            profiles,
            cmd,
            BatchSpec::new(timeout, cancel).with_concurrency(concurrency),
            sink,
        )
    }

    fn profile(id: &str) -> ConnectionProfile {
        ConnectionProfile {
            id: id.into(),
            name: id.into(),
            host: format!("{id}.example.com"),
            port: 22,
            username: "ops".into(),
            password: None,
            stored_password: None,
            key_path: None,
            notes: String::new(),
            group_id: None,
            legacy_group: None,
            last_used_at: 0,
            tags: Vec::new(),
            forwards: Vec::new(),
            // 会话日志（F-7）：测试构造的连接不记录
            session_log: None,
            // 代理（F-10）：跟随全局（用例里也没有全局代理）
            proxy: Default::default(),
            // 断线重连（F-4）：跟随全局
            auto_reconnect: None,
        }
    }

    /// 假后端：记录每台被调用的次数与命令，按 id 给出预设结果。
    struct FakeBackend {
        /// profile_id -> 退出码
        codes: HashMap<String, Option<u32>>,
        calls: Arc<parking_lot::Mutex<Vec<String>>>,
        /// 每台耗时（用来验证并发确实并行）
        delay: Duration,
    }

    impl ExecBackend for FakeBackend {
        fn exec(&self, profile: &ConnectionProfile, cmd: &str, _timeout: Duration) -> ExecResult {
            self.calls.lock().push(format!("{}:{cmd}", profile.id));
            if !self.delay.is_zero() {
                std::thread::sleep(self.delay);
            }
            let code = self.codes.get(&profile.id).copied().flatten();
            ExecResult {
                exit_code: code,
                stdout: format!("out-{}", profile.id),
                stderr: String::new(),
                timed_out: false,
                cancelled: false,
                elapsed_ms: 1,
            }
        }
    }

    #[test]
    fn truncate_is_char_boundary_safe() {
        // 全是 3 字节汉字：按字节硬切会切出半个字符
        let s = "汉".repeat(100);
        let out = truncate_stream(&s, 30);
        assert!(out.contains("省略"), "out={out}");
        // 结果必须是合法 UTF-8（Rust 里 String 天然如此），且不含替换字符
        assert!(!out.contains('\u{FFFD}'), "截断切坏了字符边界: {out}");
        // 短串原样返回
        assert_eq!(truncate_stream("abc", 10), "abc");
        // 恰好等于上限时不动它
        assert_eq!(truncate_stream("abcde", 5), "abcde");
    }

    #[test]
    fn decode_stream_is_lossy_and_bounded() {
        // 非法字节不该让整段失败
        let mut bytes = vec![b'a'];
        bytes.push(0xFF);
        bytes.push(0xFE);
        let out = decode_stream(&bytes);
        assert!(out.starts_with('a'));
        // 超长输入被截断
        let big = vec![b'x'; MAX_STREAM_BYTES + 5000];
        let out = decode_stream(&big);
        assert!(out.len() < big.len());
        assert!(out.contains("省略"));
    }

    #[test]
    fn exec_result_classifies_outcomes() {
        let ok = ExecResult {
            exit_code: Some(0),
            stdout: "x".into(),
            stderr: String::new(),
            timed_out: false,
            cancelled: false,
            elapsed_ms: 3,
        };
        assert!(ok.is_ok());
        assert!(ok.failure_reason().is_none());

        let bad = ExecResult {
            exit_code: Some(1),
            ..ok.clone()
        };
        assert!(!bad.is_ok());
        assert!(bad.failure_reason().unwrap().contains('1'));

        let to = ExecResult {
            exit_code: None,
            timed_out: true,
            ..ok.clone()
        };
        assert!(!to.is_ok());
        assert!(to.failure_reason().is_some());

        let cx = ExecResult {
            exit_code: None,
            cancelled: true,
            ..ok.clone()
        };
        assert!(!cx.is_ok());
        assert!(cx.failure_reason().is_some());
    }

    /// 回归：OpenSSH 可能先发 EOF 再发 exit-status（RFC 4254 未规定次序）。
    ///
    /// 早前收包循环把 `Eof` 当成结束标志，EOF 之后的 `ExitStatus` 被丢弃，
    /// 退出码变 `None`，巡检被判「未取得退出码」——即 inspect_load 连续
    /// 失败的根因。这里按真实到达次序喂消息，退出码必须活下来。
    #[test]
    fn absorb_keeps_exit_status_that_arrives_after_eof() {
        let mut d = Drained::default();
        let keep = absorb(
            &mut d,
            russh::ChannelMsg::Data {
                data: b"load 0.1".to_vec().into(),
            },
        );
        assert!(keep, "Data 之后循环必须继续");
        let keep = absorb(&mut d, russh::ChannelMsg::Eof);
        assert!(keep, "Eof 不是结束标志，后续还有 ExitStatus");
        let keep = absorb(&mut d, russh::ChannelMsg::ExitStatus { exit_status: 3 });
        assert!(keep);
        assert_eq!(d.exit_code, Some(3), "EOF 之后的退出码不能被丢掉");
        assert_eq!(d.stdout, b"load 0.1");
        let keep = absorb(&mut d, russh::ChannelMsg::Close);
        assert!(!keep, "只有 Close 才结束收包");
    }

    #[test]
    fn failure_reason_carries_stderr_hint_when_exit_code_missing() {
        let res = ExecResult {
            exit_code: None,
            stdout: String::new(),
            stderr: "连接失败: connection refused\nmore detail".into(),
            timed_out: false,
            cancelled: false,
            elapsed_ms: 1,
        };
        let reason = res.failure_reason().unwrap();
        assert!(reason.contains("connection refused"), "reason={reason}");
        assert!(!reason.contains("more detail"), "只取首行，reason={reason}");

        // 纯空白 stderr 时退回原来的统一下文案
        let bare = ExecResult {
            stderr: "  \n\n".into(),
            ..res
        };
        let reason = bare.failure_reason().unwrap();
        assert!(!reason.contains('：'), "无线索时不给空提示: {reason}");
    }

    #[test]
    fn batch_runs_every_target_and_reports_progress() {
        let calls = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let backend = FakeBackend {
            codes: HashMap::from([
                ("p1".to_string(), Some(0)),
                ("p2".to_string(), Some(0)),
                ("p3".to_string(), Some(0)),
            ]),
            calls: calls.clone(),
            delay: Duration::ZERO,
        };
        let seen: Arc<parking_lot::Mutex<Vec<String>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let seen2 = seen.clone();
        let handle = exec_batch_with(
            backend,
            vec![profile("p1"), profile("p2"), profile("p3")],
            "df -h".into(),
            Duration::from_secs(5),
            2,
            Arc::new(AtomicBool::new(false)),
            move |pid, res| {
                assert!(res.is_ok());
                seen2.lock().push(pid.to_string());
            },
        );
        assert_eq!(handle.total, 3);
        handle.join();

        let mut got = seen.lock().clone();
        got.sort();
        assert_eq!(got, vec!["p1", "p2", "p3"], "每台都必须被回调一次");
        assert_eq!(calls.lock().len(), 3);
        assert!(calls.lock().iter().all(|c| c.ends_with(":df -h")));
    }

    #[test]
    fn batch_reports_per_target_failure_without_aborting_others() {
        let backend = FakeBackend {
            codes: HashMap::from([
                ("p1".to_string(), Some(0)),
                ("p2".to_string(), Some(127)),
                ("p3".to_string(), None),
            ]),
            calls: Arc::new(parking_lot::Mutex::new(Vec::new())),
            delay: Duration::ZERO,
        };
        let results: Arc<parking_lot::Mutex<HashMap<String, String>>> =
            Arc::new(parking_lot::Mutex::new(HashMap::new()));
        let r2 = results.clone();
        let handle = exec_batch_with(
            backend,
            vec![profile("p1"), profile("p2"), profile("p3")],
            "cmd".into(),
            Duration::from_secs(5),
            3,
            Arc::new(AtomicBool::new(false)),
            move |pid, res| {
                r2.lock().insert(pid.to_string(), res.summary());
            },
        );
        handle.join();
        let r = results.lock().clone();
        assert_eq!(r.len(), 3, "一台失败不能吞掉其它台的结果");
        assert!(r["p2"].contains("127"));
        assert_eq!(r["p1"], crate::i18n::ts("成功".into(), "ok".into()));
    }

    #[test]
    fn cancel_before_start_skips_all_without_calling_backend() {
        let calls = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let backend = FakeBackend {
            codes: HashMap::new(),
            calls: calls.clone(),
            delay: Duration::ZERO,
        };
        let cancel = Arc::new(AtomicBool::new(true)); // 先取消
        let count = Arc::new(AtomicUsize::new(0));
        let c2 = count.clone();
        let handle = exec_batch_with(
            backend,
            vec![profile("p1"), profile("p2")],
            "cmd".into(),
            Duration::from_secs(5),
            2,
            cancel,
            move |_pid, _res| {
                c2.fetch_add(1, Ordering::SeqCst);
            },
        );
        handle.join();
        assert_eq!(calls.lock().len(), 0, "已取消的批量不得再发起任何连接");
        assert_eq!(count.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn concurrency_is_clamped_and_work_is_parallel() {
        let per_target = Duration::from_millis(120);
        let n = 8;
        let profiles: Vec<_> = (0..n).map(|i| profile(&format!("p{i}"))).collect();
        let backend = FakeBackend {
            codes: HashMap::new(),
            calls: Arc::new(parking_lot::Mutex::new(Vec::new())),
            delay: per_target,
        };
        let t0 = Instant::now();
        let handle = exec_batch_with(
            backend,
            profiles,
            "sleep 1".into(),
            Duration::from_secs(30),
            // 远超硬顶：应由 MAX_CONCURRENCY 收口
            MAX_CONCURRENCY + 99,
            Arc::new(AtomicBool::new(false)),
            |_p, _r| {},
        );
        handle.join();
        let elapsed = t0.elapsed();
        // 8 台 × 120ms，并发 16（>8）→ 约 120ms 而非 960ms。
        // 只断言「明显快于串行」，避免依赖机器负载的精确阈值。
        assert!(elapsed < per_target * n / 2, "并发没有生效：{elapsed:?}");
    }

    /// C-3（2026-09-28）：回调 panic **不得**让批量卡在「永远跑不完」。
    ///
    /// 原实现是 `sink(...)` 在 `done.fetch_add(1)` **之前**。`sink` 是调用方
    /// （UI 侧）提供的闭包，一旦它 panic，这个 worker 线程当场终止，这一台的
    /// 计数永远不会 +1；而 `is_finished()` 判据是 `done >= total`，于是
    /// 「批量执行」的进度条永远到不了 100%，后续目标也无人消费。
    ///
    /// 现在顺序倒过来（先计数、后回调）且回调包了 `catch_unwind`：即便回调每次都
    /// panic，`done` 也必须正好等于 `total`，`is_finished()` 必须为真。
    #[test]
    fn a_panicking_sink_still_advances_the_batch_to_finished() {
        let n = 6;
        let profiles: Vec<_> = (0..n).map(|i| profile(&format!("p{i}"))).collect();
        let backend = FakeBackend {
            codes: HashMap::new(),
            calls: Arc::new(parking_lot::Mutex::new(Vec::new())),
            delay: Duration::ZERO,
        };
        let handle = exec_batch_with(
            backend,
            profiles,
            "cmd".into(),
            Duration::from_secs(5),
            2,
            Arc::new(AtomicBool::new(false)),
            // 回调**每次都炸**：正是 C-3 描述的 UI 侧 panic 场景。
            |_pid, _res| panic!("模拟 UI 回调 panic"),
        );
        // `join()` 会消费 `handle`，而计数在 `Arc<AtomicUsize>` 上 —— 先留一份引用，
        // 等 worker 全部收尾后再读，才是「终态」的计数。
        let done = handle.done.clone();
        let total = handle.total;
        handle.join();
        let finished = done.load(Ordering::SeqCst);
        assert_eq!(
            finished, n,
            "回调 panic 后计数没有推进到 total：进度条会永远停在 <100%（C-3 回归）"
        );
        assert!(
            finished >= total,
            "`is_finished()` 判据（`done >= total`）必须为真，否则批量永远跑不完、UI 假死"
        );
    }

    /// AC-29：**每一次** exec 都要在 `agent-audit.log` 留痕，含失败与拦截。
    ///
    /// 审计的价值全在「可信」：漏一条的实现和没有审计在事后核对时一样无用。
    #[test]
    fn audit_exec_writes_one_line_per_target() {
        let dir = std::env::temp_dir().join(format!(
            "rs-exec-audit-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("agent-audit.log");

        // 三台：两台成功、一台失败 —— 三条都要留痕
        let backend = FakeBackend {
            codes: HashMap::from([
                ("p1".to_string(), Some(0)),
                ("p2".to_string(), Some(1)),
                ("p3".to_string(), Some(0)),
            ]),
            calls: Arc::new(parking_lot::Mutex::new(Vec::new())),
            delay: Duration::ZERO,
        };
        let handle = super::exec_batch_with(
            backend,
            vec![profile("p1"), profile("p2"), profile("p3")],
            "df -h".into(),
            // 串行（并发 1）：日志行序稳定，便于断言
            BatchSpec::new(Duration::from_secs(5), Arc::new(AtomicBool::new(false)))
                .with_concurrency(1)
                .with_audit(path.clone(), "task_exec"),
            |_p, _r| {},
        );
        handle.join();

        let text = std::fs::read_to_string(&path).expect("审计必须落盘");
        let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(lines.len(), 3, "三台各一条：{text}");
        // 每条都带目标与退出码，且 kind / risk 正确
        for l in &lines {
            let v: serde_json::Value = serde_json::from_str(l).unwrap();
            assert_eq!(v["kind"], "task_exec");
            assert_eq!(v["risk"], "safe", "df -h 是 Safe 级");
            assert!(
                v["command"].as_str().unwrap().contains("df -h"),
                "命令要留痕：{l}"
            );
            assert!(
                v["command"].as_str().unwrap().contains('.'),
                "目标（user@host:port）要能追溯到是哪台机器：{l}"
            );
        }
        // 失败的哪一条、成功的哪两条
        let ok: Vec<bool> = lines
            .iter()
            .map(|l| {
                serde_json::from_str::<serde_json::Value>(l).unwrap()["executed"]
                    .as_bool()
                    .unwrap()
            })
            .collect();
        assert_eq!(ok.iter().filter(|b| **b).count(), 2, "两条成功：{ok:?}");

        // 危险命令的风险级要如实记录（而不是一律写 safe）
        let p = profile("p1");
        audit_exec(
            &path,
            "task_exec",
            &p,
            "rm -rf /",
            &ExecResult {
                exit_code: None,
                stdout: String::new(),
                stderr: "拦截".into(),
                timed_out: false,
                cancelled: false,
                elapsed_ms: 0,
            },
        );
        let text = std::fs::read_to_string(&path).unwrap();
        let last = text.lines().last().unwrap();
        let v: serde_json::Value = serde_json::from_str(last).unwrap();
        assert_eq!(v["risk"], "blocked", "破坏类命令必须记成 blocked");
        assert_eq!(v["executed"], false, "未执行要如实记录：{last}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_target_list_finishes_immediately() {
        let backend = FakeBackend {
            codes: HashMap::new(),
            calls: Arc::new(parking_lot::Mutex::new(Vec::new())),
            delay: Duration::ZERO,
        };
        let handle = exec_batch_with(
            backend,
            Vec::new(),
            "cmd".into(),
            Duration::from_secs(1),
            4,
            Arc::new(AtomicBool::new(false)),
            |_p, _r| panic!("空目标不该有回调"),
        );
        assert_eq!(handle.total, 0);
        assert!(handle.is_finished());
        handle.join();
    }
}
