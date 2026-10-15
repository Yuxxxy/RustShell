//! [`TaskBackend`](super::TaskBackend) 的生产实现。
//!
//! 把四类动作接到真实通道上：SSH 非交互 exec（`crate::exec`）、一次性 SFTP
//! 下载（`crate::sftp::download_once`）、以及 AI 巡检（`crate::agent` 的循环）。
//!
//! **所有判定逻辑都不在这里**：阈值、异常 vs 巡检失败的区分、日志样本提取
//! 都在 [`super::inspect`]（纯函数、已被单测覆盖）。本模块只负责
//! 「把命令跑出去、把输出交给判定层、把结论装进 [`RunRecord`]」。

use super::inspect::{
    bind_findings, cert_probe_command, join_summary, judge_cert, judge_exit, judge_load, judge_log,
    judge_ports, judge_process, judge_read, judge_service, log_probe_command, parse_cert,
    parse_load, parse_log_probe, parse_ports_probe, parse_process_probe, parse_read_probe,
    parse_service_probe, ports_probe_command, process_probe_command, read_probe_command,
    service_probe_command, CommandOutput, LogProbe, TargetReport,
};
use super::{BackendOutcome, TaskBackend, TaskJob};
use crate::agent::{
    self, resolve_targets, Disposition, TargetSelector, Tool, ToolArgs, TraceRow, TraceState,
};
use crate::ai::{AiConfig, ChatMessage};
use crate::alerts::{AlertStore, Finding, FindingKind, LoadThresholds, Severity};
use crate::connections::{unix_now_secs, ConnectionProfile, ConnectionStore};
use crate::exec::{self, ExecBackend, ExecResult, SshExec, DEFAULT_CONCURRENCY};
use crate::proxy::ProxyConfig;
use crate::tasks::{
    CheckKind, LogLevel, RunRecord, RunSession, RunStatus, TargetOutcome, Task, TaskAction,
    TaskTarget,
};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// 单台机器的默认探测超时（秒）。命令本身是毫秒级，超时是给「卡死的机器」留的。
const PROBE_TIMEOUT_SECS: u64 = 15;
/// 日志巡检取回的样本行数上限（进 findings 的 evidence）。
const LOG_SAMPLE_LINES: u32 = 5;
/// AI 巡检的整体超时（秒）：工具步数与 LLM 往返都可能慢。
const AI_TIMEOUT_SECS: u64 = 300;
/// 交互式 `download_file` 的超时（秒）。比命令探测长得多 ——
/// 下载的对象可能是个几十兆的日志，而探测命令是毫秒级的。
const DOWNLOAD_TIMEOUT_SECS: u64 = 120;

/// 批量执行完成后的结果收集器（延迟到调用方显式 wait 时才阻塞）。
type BatchCollector = Box<dyn FnOnce() -> Vec<(String, ExecResult)>>;

/// 一次动作执行的结果：状态 + 逐台报告 + 备注 + 结论。
type ExecOutcome = (RunStatus, Vec<TargetReport>, Option<String>, Option<String>);

/// AI 巡检的结果：结论 + 全部 finding + 逐台摘要 + 工具轨迹。
///
/// 轨迹与结论一起返回（而不是只留在循环内部）：它要跟着运行记录落盘，
/// 用户点开某次运行的会话时看到的就是这几行 —— 没有轨迹，
/// 「这次到底查了什么」就只能靠结论的一句话猜。
type AiOutcomeBundle = (String, Vec<Finding>, Vec<(String, String)>, Vec<TraceRow>);

/// 进度可控的任务句柄。
pub struct BatchJob {
    cancel: Arc<AtomicBool>,
    done: Arc<AtomicUsize>,
    total: usize,
}

impl TaskJob for BatchJob {
    fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }
    fn progress(&self) -> (usize, usize) {
        (self.done.load(Ordering::Relaxed), self.total)
    }
}

/// Agent 的一步决策（由 LLM 返回给循环）。
#[derive(Debug, Clone)]
pub struct AgentTurn {
    pub content: String,
    pub tool_calls: Vec<crate::ai::ToolCall>,
}

/// Agent 循环的 LLM 侧。
///
/// 抽成 trait 是为了让巡检的**驱动逻辑**可测：测试注入一个按脚本回话的假 LLM，
/// 就能覆盖多步循环、参数自纠、Blocked 拦截等分支，不需要真的连模型。
pub trait AgentLlm: Send + Sync {
    fn turn(
        &self,
        messages: &[ChatMessage],
        tools: Vec<serde_json::Value>,
    ) -> Result<AgentTurn, String>;
}

/// 流式正文增量的转发目标（别名只为让 `StreamingLlm` 的字段类型可读，
/// 并在 clippy 的 `type_complexity` 下站得住）。
type DeltaSink = Arc<dyn Fn(&str) + Send + Sync>;

/// 基于现有 `ai.rs` 流式通道的 LLM 实现。
pub struct StreamingLlm {
    cfg: AiConfig,
    api_key: String,
    cancel: Arc<AtomicBool>,
    /// 增量正文的转发目标（交互式面板要「边收边显示」；无人值守巡检不需要，
    /// 传 `None` 就不会有任何额外开销）。
    on_delta: Option<DeltaSink>,
    /// **私有**结果通道。绝不能改用全局 `ai_results()`：那条通道的接收端是
    /// 竞争消费（flume 语义），AI 面板的轮询会与这里各抢一半增量 ——
    /// 表现是「回答缺字」「等不到终态」，且只在面板开着时才出现。
    tx: flume::Sender<crate::ai::AiResult>,
    rx: flume::Receiver<crate::ai::AiResult>,
}

impl StreamingLlm {
    pub fn new(cfg: AiConfig, api_key: String, cancel: Arc<AtomicBool>) -> Self {
        let (tx, rx) = flume::unbounded();
        Self {
            cfg,
            api_key,
            cancel,
            on_delta: None,
            tx,
            rx,
        }
    }

    /// 挂一个增量回调（`turn` 收到 `Delta` 时同步调用）。
    ///
    /// 用回调而不是让 `turn` 返回流，是为了不动 [`AgentLlm`] 的形状 ——
    /// 那个 trait 是「一问一答」的语义，改成流式会让无人值守巡检也跟着复杂。
    pub fn with_delta_sink(mut self, sink: Arc<dyn Fn(&str) + Send + Sync>) -> Self {
        self.on_delta = Some(sink);
        self
    }
}

impl AgentLlm for StreamingLlm {
    fn turn(
        &self,
        messages: &[ChatMessage],
        tools: Vec<serde_json::Value>,
    ) -> Result<AgentTurn, String> {
        // 调试模式：这一轮 Agent 的往返摘要（请求体与 SSE 原文由 `ai.rs` 逐条落盘，
        // 这里补的是「第几轮、带了多少上下文、模型回了什么工具」这条主线）。
        let tool_count = tools.len();
        let msg_count = messages.len();
        crate::ai::debug_log(|| {
            format!("[agent] → LLM 往返：messages={msg_count} tools={tool_count}")
        });
        let out = self.turn_inner(messages, tools);
        crate::ai::debug_log(|| match &out {
            Ok(t) => {
                let calls = if t.tool_calls.is_empty() {
                    "无".to_string()
                } else {
                    t.tool_calls
                        .iter()
                        .map(|c| format!("{}#{}", c.name, c.id))
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                format!(
                    "[agent] ← LLM 回答：正文 {} 字符 · 工具调用：{calls}",
                    t.content.chars().count()
                )
            }
            Err(e) => format!("[agent] ← LLM 失败：{e}"),
        });
        out
    }
}

impl StreamingLlm {
    fn turn_inner(
        &self,
        messages: &[ChatMessage],
        tools: Vec<serde_json::Value>,
    ) -> Result<AgentTurn, String> {
        let id = crate::ai::spawn_chat_with_tools_on(
            self.cfg.clone(),
            self.api_key.clone(),
            messages.to_vec(),
            tools,
            self.cancel.clone(),
            self.tx.clone(),
        );
        // 收集本次请求的全部结果，直到终态（Reply / Error / Cancelled）
        let rx = self.rx.clone();
        let mut content = String::new();
        let mut calls = Vec::new();
        // 读超时给足：SSE 单次往返可能较慢（与 ai.rs 的 agent 读超时同量级）
        let deadline = std::time::Instant::now() + Duration::from_secs(AI_TIMEOUT_SECS);
        loop {
            if std::time::Instant::now() > deadline {
                return Err("AI 请求超时".to_string());
            }
            match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(res) => {
                    if res.id != id {
                        continue; // 别的请求的结果
                    }
                    match res.outcome {
                        crate::ai::AiOutcome::Delta(d) => {
                            // 先把增量交出去再入账：面板要立刻看到字，
                            // 而 `content` 只是这一轮的汇总副本。
                            if let Some(sink) = &self.on_delta {
                                sink(&d);
                            }
                            content.push_str(&d);
                        }
                        crate::ai::AiOutcome::ToolCalls(c) => calls = c,
                        crate::ai::AiOutcome::Reply(full) => {
                            // 流式路径下 content 已累积；非流式路径下 full 才是全集
                            if content.is_empty() {
                                content = full;
                            }
                            return Ok(AgentTurn {
                                content,
                                tool_calls: calls,
                            });
                        }
                        crate::ai::AiOutcome::Reasoning(_) => {}
                        crate::ai::AiOutcome::Cancelled => {
                            return Err("已取消".to_string());
                        }
                        crate::ai::AiOutcome::ToolsUnsupported => {
                            // 能力结论由上层落盘（v1.1 §3.8.1）。这里只是不再继续
                            // 重试工具调用 —— 继续发只会再撞一次同样的 400。
                            return Err(crate::i18n::ts(
                                "当前模型不支持工具调用".into(),
                                "the current model does not support tool calling".into(),
                            ));
                        }
                        crate::ai::AiOutcome::Error(e) => return Err(e),
                    }
                }
                Err(flume::RecvTimeoutError::Timeout) => continue,
                Err(flume::RecvTimeoutError::Disconnected) => {
                    return Err("AI 结果通道已关闭".to_string())
                }
            }
        }
    }
}

/// 生产后端。
pub struct RealBackend {
    /// 连接库（**已恢复口令**：由调用方在构造前调 `restore_passwords`）
    conn: Arc<ConnectionStore>,
    alerts: Arc<parking_lot::Mutex<AlertStore>>,
    /// AI 配置与密钥（未配置 / 关闭 agent 时 AI 巡检退化为「跳过」）
    ai: Option<(AiConfig, String)>,
    download_dir: PathBuf,
    /// 审计日志落点（每次 exec / 下载都留痕，AC-29）
    audit_path: PathBuf,
    /// 可注入的执行后端（测试用假实现）
    exec: Arc<dyn ExecBackend>,
    /// 最近一次 AI 巡检的对话快照（结论之外的追溯线索）
    last_ai_turns: Arc<parking_lot::Mutex<Vec<String>>>,
    /// 本次运行留下的 AI 会话（巡检跑完写一次；调用方在收尾时取走）。
    ///
    /// 与 `last_ai_turns` 同一手法（后端实例在每次运行的线程里新建，故天然
    /// 是一次的桶）：改签名把会话从 `ai_reports` 传出来会牵动 `execute`
    /// 与 `TaskBackend::start` 两层，而它们对「会话」一无所知。
    last_ai_session: Arc<parking_lot::Mutex<Option<RunSession>>>,
}

impl RealBackend {
    /// `global_proxy`（F-10）：全局代理的运行期形态。跑任务的连接与交互式会话
    /// 走同一份设置 —— 定时任务无人值守，更不该悄悄绕开代理（那会让「代理没配好」
    /// 在手动连接时暴露、在后台却静默直连成功，最难排查）。
    pub fn new(
        conn: Arc<ConnectionStore>,
        alerts: Arc<parking_lot::Mutex<AlertStore>>,
        ai: Option<(AiConfig, String)>,
        download_dir: PathBuf,
        audit_path: PathBuf,
        global_proxy: Option<ProxyConfig>,
    ) -> Self {
        Self {
            conn,
            alerts,
            ai,
            download_dir,
            audit_path,
            exec: Arc::new(SshExec { global_proxy }),
            last_ai_turns: Arc::new(parking_lot::Mutex::new(Vec::new())),
            last_ai_session: Arc::new(parking_lot::Mutex::new(None)),
        }
    }

    /// 注入假执行后端（单测用）。
    pub fn with_exec(mut self, exec: Arc<dyn ExecBackend>) -> Self {
        self.exec = exec;
        self
    }

    /// 解析任务目标为具体连接。
    fn resolve(&self, target: &TaskTarget) -> Result<Vec<ConnectionProfile>, String> {
        let selector = if !target.profiles.is_empty() {
            TargetSelector::Profiles(target.profiles.clone())
        } else if !target.groups.is_empty() {
            // 多分组时逐个解析并合并（`resolve_targets` 只接受单个选择器）
            let mut ids: Vec<String> = Vec::new();
            for g in &target.groups {
                match resolve_targets(&self.conn, &TargetSelector::Group(g.clone())) {
                    Ok(mut v) => ids.append(&mut v),
                    Err(e) => return Err(e.to_string()),
                }
            }
            TargetSelector::Profiles(ids)
        } else if !target.tags.is_empty() {
            let mut ids: Vec<String> = Vec::new();
            for t in &target.tags {
                match resolve_targets(&self.conn, &TargetSelector::Tag(t.clone())) {
                    Ok(mut v) => ids.append(&mut v),
                    Err(e) => return Err(e.to_string()),
                }
            }
            TargetSelector::Profiles(ids)
        } else {
            TargetSelector::All
        };

        let ids = resolve_targets(&self.conn, &selector).map_err(|e| e.to_string())?;
        let mut out: Vec<ConnectionProfile> = Vec::new();
        let mut skipped: Vec<String> = Vec::new();
        for id in ids {
            if target.exclude.contains(&id) {
                continue;
            }
            match self.conn.profile(&id) {
                Some(p) => {
                    // 无人值守**不能**弹口令框：没存口令的连接只能跳过，
                    // 并把原因写进运行记录（否则用户只看到「少了一台」）。
                    if p.password.is_none() && p.key_path.is_none() {
                        skipped.push(p.name.clone());
                        continue;
                    }
                    out.push(p.clone());
                }
                None => skipped.push(id),
            }
        }
        if out.is_empty() {
            if skipped.is_empty() {
                return Err(crate::i18n::ts(
                    "目标为空（没有可执行的连接）".into(),
                    "no executable target".into(),
                ));
            }
            return Err(crate::i18n::ts(
                format!(
                    "没有可执行的连接：{} 未保存口令或已不存在",
                    skipped.join("、")
                ),
                format!("no executable connection: {}", skipped.join(", ")),
            ));
        }
        Ok(out)
    }

    /// 批量跑同一条命令，返回 `(目标数, 取结果)`。
    ///
    /// 「取结果」是刻意延迟的：并发调度一开始就返回，调用方可以选择先做别的事，
    /// 也可以立刻 `wait()`。结果按**目标原序**重排 —— 巡检报告的顺序必须稳定，
    /// 否则两次运行的对比会被完成顺序的抖动淹没。
    fn run_batch(
        &self,
        profiles: Vec<ConnectionProfile>,
        cmd: String,
        timeout: Duration,
        concurrency: usize,
        cancel: Arc<AtomicBool>,
    ) -> (usize, BatchCollector) {
        let order: Vec<String> = profiles.iter().map(|p| p.id.clone()).collect();
        let collected: Arc<parking_lot::Mutex<Vec<(String, ExecResult)>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let sink_collected = collected.clone();
        let handle = exec::exec_batch_with(
            self.exec.clone(),
            profiles,
            cmd,
            exec::BatchSpec::new(timeout, cancel)
                .with_concurrency(concurrency)
                .with_audit(self.audit_path.clone(), "task_exec"),
            move |pid, res| {
                sink_collected.lock().push((pid.to_string(), res.clone()));
            },
        );
        let total = handle.total;
        let wait = move || {
            handle.join();
            let mut out = collected.lock().clone();
            out.sort_by_key(|(pid, _)| order.iter().position(|o| o == pid).unwrap_or(usize::MAX));
            out
        };
        (total, Box::new(wait))
    }

    /// 连通性巡检（TCP 端口）：用 `exec` 之外的轻量方式 ——
    /// 直接尝试建连，不要求认证成功，因此不需要口令。
    fn reachability_reports(
        &self,
        profiles: &[ConnectionProfile],
        port: Option<u16>,
        timeout: Duration,
        cancel: Arc<AtomicBool>,
        progress: Arc<AtomicUsize>,
    ) -> Vec<TargetReport> {
        let mut reports = Vec::new();
        for p in profiles {
            if cancel.load(Ordering::SeqCst) {
                break;
            }
            let port = port.unwrap_or(p.port);
            let start = std::time::Instant::now();
            let addr = format!("{}:{}", p.host, port);
            let ok = std::net::TcpStream::connect_timeout(
                &match addr.parse() {
                    Ok(a) => a,
                    Err(_) => {
                        reports.push(TargetReport::failure(
                            &p.id,
                            &p.name,
                            false,
                            crate::i18n::ts(
                                format!("地址无法解析：{addr}"),
                                format!("unresolvable address: {addr}"),
                            ),
                        ));
                        progress.fetch_add(1, Ordering::SeqCst);
                        continue;
                    }
                },
                timeout.min(Duration::from_secs(10)),
            )
            .is_ok();
            let ms = start.elapsed().as_millis();
            progress.fetch_add(1, Ordering::SeqCst);
            if ok {
                reports.push(TargetReport {
                    profile_id: p.id.clone(),
                    profile_name: p.name.clone(),
                    connected: true,
                    exec_ok: true,
                    anomaly: false,
                    summary: crate::i18n::ts(
                        format!("{addr} 可达（{ms}ms）"),
                        format!("{addr} reachable ({ms}ms)"),
                    ),
                    detail: None,
                    findings: Vec::new(),
                });
            } else {
                reports.push(TargetReport::failure(
                    &p.id,
                    &p.name,
                    false,
                    crate::i18n::ts(format!("{addr} 不可达"), format!("{addr} unreachable")),
                ));
            }
        }
        reports
    }

    /// 执行一批命令并判定退出码。
    fn exec_reports(
        &self,
        profiles: &[ConnectionProfile],
        cmd: &str,
        timeout: Duration,
        allow_mutating: bool,
        cancel: Arc<AtomicBool>,
        progress: Arc<AtomicUsize>,
    ) -> Result<Vec<TargetReport>, String> {
        // 无人值守的安全闸：与 agent 共用 `assess_command`
        let risk = crate::ai::assess_command(cmd);
        match risk {
            crate::ai::Risk::Blocked => {
                return Err(crate::i18n::ts(
                    "该命令属于不可逆破坏操作，已被安全策略永久拦截".into(),
                    "blocked by the safety policy".into(),
                ))
            }
            crate::ai::Risk::Confirm if !allow_mutating => {
                return Err(crate::i18n::ts(
                    "无人值守不允许执行状态变更命令（可在任务里显式开启）".into(),
                    "state-changing commands are not allowed unattended".into(),
                ))
            }
            _ => {}
        }
        let order: Vec<(String, String)> = profiles
            .iter()
            .map(|p| (p.id.clone(), p.name.clone()))
            .collect();
        let (total, wait) = self.run_batch(
            profiles.to_vec(),
            cmd.to_string(),
            timeout,
            DEFAULT_CONCURRENCY,
            cancel,
        );
        let results = wait();
        progress.fetch_add(total, Ordering::SeqCst);
        let mut reports = Vec::new();
        for (pid, res) in results {
            let name = order
                .iter()
                .find(|(id, _)| id == &pid)
                .map(|(_, n)| n.clone())
                .unwrap_or_else(|| pid.clone());
            let out = CommandOutput {
                exit_code: res.exit_code,
                stdout: res.stdout.clone(),
                stderr: res.stderr.clone(),
                timed_out: res.timed_out,
            };
            let (mut anomaly, summary, mut findings) = judge_exit(&out, &[]);
            bind_findings(&mut findings, &pid, &name);
            // 命令层面的失败（连不上 / 超时）也算「巡检失败」，要能被 on_error 规则覆盖
            if !res.is_ok() && findings.is_empty() {
                let reason = res
                    .failure_reason()
                    .unwrap_or_else(|| crate::i18n::ts("命令失败".into(), "command failed".into()));
                // 命令失败归到「巡检失败」，不该被当成服务器异常
                anomaly = false;
                findings.push(super::inspect::inspection_failure_finding(
                    &pid, &name, &reason, true,
                ));
            }
            let mut detail = res.stdout.clone();
            if !res.stderr.trim().is_empty() {
                detail.push_str("\n--- stderr ---\n");
                detail.push_str(&res.stderr);
            }
            reports.push(TargetReport {
                profile_id: pid,
                profile_name: name,
                connected: true,
                exec_ok: res.is_ok(),
                anomaly,
                summary,
                detail: Some(detail),
                findings,
            });
        }
        Ok(reports)
    }

    /// 下载：逐台拉一个文件。
    fn download_reports(
        &self,
        profiles: &[ConnectionProfile],
        remote: &str,
        local_dir: Option<&str>,
        cancel: Arc<AtomicBool>,
        progress: Arc<AtomicUsize>,
    ) -> Vec<TargetReport> {
        let base = local_dir
            .map(PathBuf::from)
            .unwrap_or_else(|| self.download_dir.clone());
        let mut reports = Vec::new();
        for p in profiles {
            if cancel.load(Ordering::SeqCst) {
                break;
            }
            // 本地文件名带上连接名与时间，避免多台机器下同一个文件名互相覆盖
            let fname = remote
                .rsplit('/')
                .next()
                .filter(|s| !s.is_empty())
                .unwrap_or("download");
            let stamp = crate::metrics::time_label(unix_now_secs()).replace(':', "");
            let local = base.join(format!("{}-{}-{}", p.name, stamp, fname));
            let res = crate::sftp::download_once(
                p,
                remote,
                &local,
                Duration::from_secs(60),
                self.exec.global_proxy(),
            );
            progress.fetch_add(1, Ordering::SeqCst);
            match res {
                Ok(d) => reports.push(TargetReport {
                    profile_id: p.id.clone(),
                    profile_name: p.name.clone(),
                    connected: true,
                    exec_ok: true,
                    anomaly: false,
                    summary: crate::i18n::ts(
                        format!("已下载 {} 字节到 {}", d.bytes, d.local.display()),
                        format!("downloaded {} bytes to {}", d.bytes, d.local.display()),
                    ),
                    detail: Some(d.local.display().to_string()),
                    findings: Vec::new(),
                }),
                Err(e) => reports.push(TargetReport::failure(&p.id, &p.name, true, e)),
            }
        }
        reports
    }

    /// AI 巡检：走 agent 多步循环，工具只读、无人确认。
    fn ai_reports(
        &self,
        profiles: &[ConnectionProfile],
        action: &TaskAction,
        cancel: Arc<AtomicBool>,
        progress: Arc<AtomicUsize>,
    ) -> Vec<TargetReport> {
        let TaskAction::AiInspect {
            prompt,
            checks,
            thresholds,
            log_path,
            log_level,
            ..
        } = action
        else {
            return Vec::new();
        };
        let Some((cfg, key)) = self.ai.clone() else {
            return profiles
                .iter()
                .map(|p| {
                    TargetReport::failure(
                        &p.id,
                        &p.name,
                        false,
                        crate::i18n::ts(
                            "未配置 AI 服务，无法执行 AI 巡检".into(),
                            "AI service is not configured".into(),
                        ),
                    )
                })
                .collect();
        };
        if !cfg.agent_enabled {
            return profiles
                .iter()
                .map(|p| {
                    TargetReport::failure(
                        &p.id,
                        &p.name,
                        false,
                        crate::i18n::ts(
                            "工具调用（Agent）已在设置中关闭".into(),
                            "Agent mode is disabled in settings".into(),
                        ),
                    )
                })
                .collect();
        }

        let names: Vec<String> = profiles.iter().map(|p| p.name.clone()).collect();
        let thresholds = thresholds.unwrap_or_default();
        // checks **原样**带进 plan：空 = 界面撤下巡检项后的新任务，提示词里走
        // 「按 AI 要求自己选工具」那一段。这里不能再补「负载 + 日志」默认值 ——
        // 那会替用户指定一项没有日志路径的「日志巡检」，模型要么编路径要么空转。
        let plan = AiPlan {
            names,
            checks: checks.clone(),
            thresholds,
            log_path: log_path.clone(),
            log_level: *log_level,
            extra: prompt.clone(),
        };
        // 能力缓存（v1.1 §3.8.1）：已知不支持工具调用就直接别发 `tools`，
        // 省掉每轮一次注定失败的往返。只在 Unknown 时才让请求去探测。
        if crate::ai::ModelCapabilities::for_provider(&cfg).tool_calling_unsupported() {
            return profiles
                .iter()
                .map(|p| {
                    TargetReport::failure(
                        &p.id,
                        &p.name,
                        false,
                        crate::i18n::ts(
                            "当前模型已确认不支持工具调用，AI 巡检无法进行（可在设置中更换模型）"
                                .into(),
                            "the current model is known not to support tool calling".into(),
                        ),
                    )
                })
                .collect();
        }
        let llm = StreamingLlm::new(cfg.clone(), key, cancel.clone());
        let tools = AiToolRuntime {
            conn: self.conn.clone(),
            backend: self.exec.clone(),
            audit_path: self.audit_path.clone(),
            download_dir: self.download_dir.clone(),
            profiles: profiles.to_vec(),
            thresholds,
            cancel: cancel.clone(),
        };
        // 进度由工具执行路径统一累加：AI 巡检的目标数是「本次覆盖的连接数」，
        // 与逐台命令不同，故这里只在收尾时把总数推进一次。
        let _ = progress;
        match run_ai_inspection(&llm, &tools, &plan) {
            Ok((conclusion, findings, per_target, trace)) => {
                self.last_ai_turns.lock().push(conclusion.clone());
                // 会话痕迹留给收尾的一方（`RealBackend::start` 里取走并写进运行记录）：
                // 「每次执行都能在 AI 聊天记录里看到这次巡检」靠的就是这一份。
                // 明细在这里按持久化口径（8 KiB）截断 —— 落盘的是留证，不是全量日志。
                let session = RunSession {
                    prompt: prompt.clone(),
                    conclusion: conclusion.clone(),
                    trace: trace
                        .into_iter()
                        .map(|mut r| {
                            r.detail = crate::agent::trace::truncate_to(
                                &r.detail,
                                crate::tasks::SESSION_TRACE_DETAIL_BYTES,
                            );
                            r
                        })
                        .collect(),
                };
                // 全空（模型一句话没说、一个工具没调）时不记会话：空会话只会
                // 在聊天记录里多一条打不开任何东西的「新对话」。
                *self.last_ai_session.lock() = (!session.is_empty()).then_some(session);
                let mut reports: Vec<TargetReport> = Vec::new();
                for p in profiles {
                    let findings_for: Vec<Finding> = findings
                        .iter()
                        .filter(|f| f.profile_id == p.id)
                        .cloned()
                        .collect();
                    let anomaly = findings_for.iter().any(|f| f.kind == FindingKind::Anomaly);
                    let failed = findings_for
                        .iter()
                        .any(|f| f.kind == FindingKind::InspectionFailed);
                    let summary = per_target
                        .iter()
                        .find(|(id, _)| id == &p.id)
                        .map(|(_, s)| s.clone())
                        .unwrap_or_else(|| crate::i18n::ts("已完成".into(), "done".into()));
                    reports.push(TargetReport {
                        profile_id: p.id.clone(),
                        profile_name: p.name.clone(),
                        connected: true,
                        exec_ok: !failed,
                        anomaly,
                        summary,
                        detail: Some(conclusion.clone()),
                        findings: findings_for,
                    });
                }
                reports
            }
            Err(e) => {
                // AI 整体失败：所有目标都记「巡检失败」，并把原因带上
                profiles
                    .iter()
                    .map(|p| TargetReport::failure(&p.id, &p.name, false, e.clone()))
                    .collect()
            }
        }
    }
}

/// AI 巡检的输入计划（从任务动作里摘出来，便于驱动函数保持纯参数）。
struct AiPlan {
    names: Vec<String>,
    checks: Vec<CheckKind>,
    thresholds: LoadThresholds,
    log_path: Option<String>,
    log_level: LogLevel,
    extra: String,
}

impl TaskBackend for RealBackend {
    fn start(&self, task: Task) -> (Arc<dyn TaskJob>, flume::Receiver<BackendOutcome>) {
        let (tx, rx) = flume::bounded(1);
        let cancel = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicUsize::new(0));
        let job: Arc<dyn TaskJob> = Arc::new(BatchJob {
            cancel: cancel.clone(),
            done: done.clone(),
            total: 0,
        });

        // 解析目标与动作，构造执行闭包 —— 全部在后台线程里做，
        // UI 线程不得触碰 SSH。
        let action = task.action.clone();
        let conn = self.conn.clone();
        let alerts = self.alerts.clone();
        let ai = self.ai.clone();
        let download_dir = self.download_dir.clone();
        let audit_path = self.audit_path.clone();
        let exec = self.exec.clone();
        let task_name = task.name.clone();
        let task_id = task.id.clone();
        let progress = done.clone();
        let cancel_bg = cancel.clone();

        std::thread::Builder::new()
            .name(format!("rs-task-run-{task_id}"))
            .spawn(move || {
                let started_at = unix_now_secs();
                let backend = RealBackend {
                    conn,
                    alerts,
                    ai,
                    download_dir,
                    audit_path,
                    exec,
                    last_ai_turns: Arc::new(parking_lot::Mutex::new(Vec::new())),
                    last_ai_session: Arc::new(parking_lot::Mutex::new(None)),
                };
                let (status, reports, note, conclusion) =
                    match backend.execute(&action, &progress, &cancel_bg) {
                        Ok(v) => v,
                        Err(e) => (RunStatus::Failed, Vec::new(), Some(e), None),
                    };
                let findings: Vec<Finding> =
                    reports.iter().flat_map(|r| r.findings.clone()).collect();
                let targets: Vec<TargetOutcome> = reports.iter().map(|r| r.to_outcome()).collect();
                // 机器侧完整结果单独留一份进运行记录（v1.1 §2.2.3）：
                // 模型上下文只有 8 KiB，本地留证不该受那个预算约束。
                let machine = super::inspect::machine_results(&reports);
                let finished_at = unix_now_secs();
                // AI 巡检留下的会话（其余动作恒为 None）。`execute` 的四元组里
                // 没有它 —— 会话只在 AI 路径产生，而 `execute` 对动作类型一无所知。
                let session = backend.last_ai_session.lock().take();
                // 结论：AI 巡检的结论在会话里，这里补进记录（列表与会话都直接可读）。
                let conclusion =
                    conclusion.or_else(|| session.as_ref().map(|s| s.conclusion.clone()));
                let rec = RunRecord {
                    run_id: crate::connections::uuid_like(),
                    task_id,
                    task_name,
                    started_at,
                    finished_at,
                    status,
                    targets,
                    machine,
                    conclusion,
                    findings,
                    note,
                    session,
                };
                let _ = tx.send(BackendOutcome { record: rec });
            })
            .expect("spawn task runner");

        (job, rx)
    }
}

impl RealBackend {
    /// 执行动作，返回 `(状态, 逐台报告, 备注, 结论)`。
    fn execute(
        &self,
        action: &TaskAction,
        progress: &Arc<AtomicUsize>,
        cancel: &Arc<AtomicBool>,
    ) -> Result<ExecOutcome, String> {
        let target = action.target();
        let profiles = self.resolve(target)?;
        let reports = match action {
            TaskAction::MultiConnect {
                keep_alive_secs, ..
            } => {
                // 「连接」本身用一次极轻的只读命令验证可达性：
                // 真正的会话建立要走 PTY/Tab，定时场景不需要。
                let r = self.reachability_reports(
                    &profiles,
                    None,
                    Duration::from_secs(PROBE_TIMEOUT_SECS),
                    cancel.clone(),
                    progress.clone(),
                );
                // keep_alive 现在不实现常驻（见需求文档 §17 Q-8 的处置）
                let _ = keep_alive_secs;
                r
            }
            TaskAction::ExecMulti {
                command,
                timeout_secs,
                allow_mutating,
                ..
            } => self.exec_reports(
                &profiles,
                command,
                Duration::from_secs((*timeout_secs).clamp(1, crate::tasks::MAX_EXEC_TIMEOUT_SECS)),
                *allow_mutating,
                cancel.clone(),
                progress.clone(),
            )?,
            TaskAction::Download {
                remote_path,
                local_dir,
                ..
            } => self.download_reports(
                &profiles,
                remote_path,
                local_dir.as_deref(),
                cancel.clone(),
                progress.clone(),
            ),
            TaskAction::AiInspect { .. } => {
                self.ai_reports(&profiles, action, cancel.clone(), progress.clone())
            }
        };

        // 汇总状态：全部成功 = Ok；有异常或部分失败 = Partial；全失败 = Failed
        let failed = reports.iter().filter(|r| !r.exec_ok).count();
        let status = if reports.is_empty() {
            RunStatus::Skipped
        } else if failed == reports.len() {
            RunStatus::Failed
        } else if failed > 0
            || reports.iter().any(|r| r.anomaly)
            || reports
                .iter()
                .flat_map(|r| &r.findings)
                .any(|f| f.kind == FindingKind::Anomaly)
        {
            RunStatus::Partial
        } else {
            RunStatus::Ok
        };
        let note = if cancel.load(Ordering::SeqCst) {
            Some(crate::i18n::ts("已取消".into(), "cancelled".into()))
        } else {
            None
        };
        let status = if note.is_some() {
            RunStatus::Cancelled
        } else {
            status
        };
        Ok((status, reports, note, None))
    }
}

// AI 巡检的驱动

/// 工具执行运行时（agent 工具箱在真实通道上的实现）。
pub(crate) struct AiToolRuntime {
    conn: Arc<ConnectionStore>,
    backend: Arc<dyn ExecBackend>,
    /// 审计落点（agent 工具调用与任务执行共用一份日志）
    audit_path: PathBuf,
    /// `download_file` 的落地目录（交互式下载用；巡检路径不碰它）
    download_dir: PathBuf,
    profiles: Vec<ConnectionProfile>,
    thresholds: LoadThresholds,
    cancel: Arc<AtomicBool>,
}

impl AiToolRuntime {
    fn find_profile(&self, id: &str) -> Option<&ConnectionProfile> {
        self.profiles.iter().find(|p| p.id == id)
    }
}

/// 跑一次 AI 巡检。
///
/// 这是 agent 循环的**驱动**（状态机在 `crate::agent::AgentSession`）：
/// 每一步问 LLM -> 执行只读工具 -> 回注结果 -> 直到模型不再要工具或到步数上限。
///
/// 无人值守下的工具裁决：只读工具直接跑；任何需要确认的（含批量 `run_command`）
/// 一律回注「无人值守不允许」，**不执行**。这样 AI 巡检天然是只读的，
/// 与需求文档 §7.4 的边界一致。
fn run_ai_inspection(
    llm: &dyn AgentLlm,
    rt: &AiToolRuntime,
    plan: &AiPlan,
) -> Result<AiOutcomeBundle, String> {
    let system = build_inspect_prompt(plan);
    // 巡检上下文用**只读策略**（v1.1 §3.6.1）：下载与任务草案连 `tools[]`
    // 都不出现 —— 暴露了再拒绝只会让模型浪费步数反复尝试。
    // 执行层仍会再过一次 `judge`（双层防线，见 `execute_tool_call`）。
    let policy = agent::ToolPolicy::inspect_only();
    let budget = agent::AgentBudget {
        max_targets: rt.profiles.len().max(1),
        ..agent::AgentBudget::default()
    };
    let mut session = agent::AgentSession::with_budget(system, budget, policy);
    session.push_user(crate::i18n::ts(
        "请开始巡检".into(),
        "Start the inspection".into(),
    ));

    let mut all_findings: Vec<Finding> = Vec::new();
    let mut per_target: Vec<(String, String)> = Vec::new();
    // 与交互式路径同样记一份轨迹：这份要跟着运行记录落盘，
    // 用户事后点开这次运行的会话时看到的就是它（见 `RunSession`）。
    let mut rows: Vec<TraceRow> = Vec::new();
    let mut conclusion = String::new();

    loop {
        match session.next_action() {
            agent::NextAction::RequestLlm => {
                session.begin_llm_round();
                let turn = llm.turn(&session.transcript, session.tools_for_provider())?;
                if turn.tool_calls.is_empty() {
                    conclusion = turn.content.clone();
                    session.record_reply(&turn.content);
                    break;
                }
                let Some(step) = session.record_tool_calls(&turn.content, turn.tool_calls.clone())
                else {
                    conclusion = turn.content.clone();
                    session.record_reply(&turn.content);
                    break;
                };
                for (ix, call) in turn.tool_calls.iter().enumerate() {
                    let planned = step.outcomes.get(ix);
                    // 超出单步上限的调用：状态机已就地判死，这里直接收口
                    if planned
                        .map(|o| !matches!(o.status, agent::ToolStatus::Pending))
                        .unwrap_or(true)
                    {
                        if let Some(o) = planned {
                            session.record_outcome(o.clone());
                            // 判死的调用也要留一行轨迹：「模型尝试过什么」同样属于
                            // 这次运行的事实，缺了它用户只会看到结论里没提这件事。
                            let mut row =
                                TraceRow::new(step.index, o.call_id.clone(), call.name.clone());
                            row.summary = summary_of(rt, call);
                            row.state = TraceState::of(&o.status, false);
                            row.note = o.status.label();
                            upsert_trace(&mut rows, &row, &NullSink);
                        }
                        continue;
                    }
                    let started = std::time::Instant::now();
                    let mut row = TraceRow::new(step.index, call.id.clone(), call.name.clone());
                    row.summary = summary_of(rt, call);
                    row.state = TraceState::Running;
                    row.note = TraceState::Running.label();
                    upsert_trace(&mut rows, &row, &NullSink);

                    let (status, text, findings) = execute_tool_call(rt, call, &all_findings);
                    let elapsed = started.elapsed().as_millis() as u64;
                    // 明细要在 `findings` / `text` 被移走之前算出来，
                    // 否则这一行只剩工具名，点开什么都看不到。
                    let detail = detail_of(call, &findings, &text);
                    let anomaly = !findings.is_empty();
                    let note = trace_note(&status, findings.len());
                    all_findings.extend(findings);
                    session.record_outcome(agent::ToolOutcome {
                        call_id: call.id.clone(),
                        tool: Tool::from_name(&call.name).unwrap_or(Tool::ListTargets),
                        summary: summary_of(rt, call),
                        status: status.clone(),
                        result_text: text,
                        elapsed_ms: elapsed,
                    });
                    settle_row(
                        &mut rows,
                        &NullSink,
                        &call.id,
                        TraceState::of(&status, anomaly),
                        note,
                        elapsed,
                        detail,
                    );
                }
            }
            agent::NextAction::AwaitConfirm => {
                // 无人值守不该走到这里（裁决阶段已把 Confirm 变成 Disabled）；
                // 真的走到就明确收尾，而不是挂在那里等一个不会来的确认。
                session.close_pending_calls("无人值守场景不允许该操作");
                conclusion = crate::i18n::ts(
                    "巡检尝试了需要确认的操作，已中止".into(),
                    "inspection tried a confirm-required operation; aborted".into(),
                );
                break;
            }
            agent::NextAction::Finish(reason) => {
                if let Some(note) = reason.note() {
                    conclusion = if conclusion.is_empty() {
                        note
                    } else {
                        format!("{conclusion}\n{note}")
                    };
                }
                break;
            }
        }
    }

    // 逐台摘要取 findings 里最严重的一条
    for p in &rt.profiles {
        let f = all_findings.iter().find(|f| f.profile_id == p.id);
        per_target.push((
            p.id.clone(),
            match f {
                Some(f) => f.message.clone(),
                None => crate::i18n::ts("未发现异常".into(), "no anomaly found".into()),
            },
        ));
    }
    Ok((conclusion, all_findings, per_target, rows))
}

/// 执行一次工具调用（无人值守策略）。
fn execute_tool_call(
    rt: &AiToolRuntime,
    call: &crate::ai::ToolCall,
    _seen: &[Finding],
) -> (agent::ToolStatus, String, Vec<Finding>) {
    execute_tool_call_with(
        rt,
        call,
        agent::GuardContext {
            allow_mutating: false,
            interactive: false,
            target_count: 0,
            whitelisted: false,
        },
        // 无人值守没有任何人可以确认：任何 Confirm 一律按「拒绝」收口。
        // 注意 `judge` 在 `interactive = false` 时已经把 Confirm 变成 `Disabled`，
        // 这个闭包只是兜底 —— 走到它就说明裁决规则被改出了漏洞。
        &mut |_req| AgentDecision::Deny,
        // 无人值守没有面板可通知；草案工具在巡检策略下连 `tools[]` 都不出现，
        // 走到 `propose` 的路径不存在。
        &NullSink,
    )
}

/// 什么都不做的 sink：给无人值守路径兜底（它没有任何 UI 可通知）。
struct NullSink;

impl AgentSink for NullSink {
    fn delta(&self, _text: &str) {}
    fn trace(&self, _row: &TraceRow) {}
    fn confirm(&self, _req: &ConfirmRequest) -> AgentDecision {
        AgentDecision::Deny
    }
    fn propose(&self, _task: &Task) {}
    fn done(&self, _conclusion: &str, _reason: &agent::FinishReason) {}
}

/// 交互式 agent 与 UI 之间的**确认请求**（确认卡片的数据源）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmRequest {
    pub step: u8,
    pub call_id: String,
    /// 稳定的工具名（卡片据此选图标/文案）
    pub tool: String,
    /// 人类可读的调用摘要（目标 + 命令首行）
    pub summary: String,
    /// 为什么要确认（来自 `judge` 的可读理由）
    pub reason: String,
    /// 本次调用覆盖的目标数（卡片写「将在 N 台上执行」）
    pub target_count: usize,
    /// 要执行的命令原文（非命令类工具为空）—— 卡片上要原样展示
    pub command: String,
    /// 白名单是否可用于本次调用（批量 >1 台时为 false）
    pub whitelistable: bool,
}

/// 用户对确认卡片的决定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentDecision {
    /// 执行
    Approve,
    /// 忽略（回注「用户已忽略」，模型换别的手段或就此收尾）
    Deny,
}

/// 交互式 agent 的**驱动接口**。
///
/// 抽成 trait（而不是直接传通道）是为了让整个循环能脱 UI 单测：
/// 测试注入一个「脚本化 sink」，就能覆盖确认挂起、拒绝、停止、步数上限等分支，
/// 不需要 GPUI、不需要网络（AC-47 的同一思路）。
pub trait AgentSink: Send + Sync {
    /// 模型的一段正文增量（流式）。
    fn delta(&self, text: &str);
    /// 轨迹行新增或更新（同一个 `call_id` 只会更新，不会追加新行）。
    fn trace(&self, row: &TraceRow);
    /// 阻塞等待用户决策。实现方负责超时与「窗口已关」的兜底（一律当 Deny）。
    fn confirm(&self, req: &ConfirmRequest) -> AgentDecision;
    /// 用户已确认的任务草案。**入库由实现方负责** —— 循环拿不到任务库，
    /// 硬塞进来会让这个纯执行层依赖 UI 的持久化状态。
    fn propose(&self, task: &Task);
    /// 整轮结束（正常收尾、预算耗尽、被停止、出错都走这里）。
    fn done(&self, conclusion: &str, reason: &agent::FinishReason);
}

/// 交互式 agent 的输入。
pub struct InteractiveSpec {
    pub cfg: AiConfig,
    pub api_key: String,
    pub system: String,
    pub user: String,
    /// 本会话已有的消息（多轮对话要带上，否则模型只看得到最后一句）。
    ///
    /// 只喂 `user` / `assistant`；`system` 由 `spec.system` 单独给出 ——
    /// 两处都给会让 transcript 里出现两条 system 消息，而 OpenAI 协议下
    /// 第二条会被当成普通上下文，表现得像「AI 忘了自己的角色」。
    pub history: Vec<ChatMessage>,
    pub profiles: Vec<ConnectionProfile>,
    pub thresholds: LoadThresholds,
    /// 审计落点（与任务执行、快捷命令共用一份）
    pub audit_path: PathBuf,
    /// `download_file` 的落地目录
    pub download_dir: PathBuf,
    /// 命令白名单（`ai_cfg.auto_approve`）；**批量场景不生效**（见 `judge`）
    pub whitelist: Vec<crate::ai::WhitelistRule>,
    /// 允许状态变更类操作（对应设置里的 `agent_allow_mutating`）
    pub allow_mutating: bool,
    pub cancel: Arc<AtomicBool>,
}

/// 跑一轮**交互式** agent（AI 面板的问答）。
///
/// 与 [`run_ai_inspection`] 共用工具执行与安全裁决，差异只有三点：
/// 1. 有人在场 ⇒ `GuardContext.interactive = true`，Confirm 走确认卡片而不是直接禁用；
/// 2. 每一步的轨迹经 `sink` 外发（面板要边跑边显示）；
/// 3. 工具可见性按 `allow_mutating` 收放（关掉时退化为巡检那套只读清单）。
pub(crate) fn run_interactive_agent(
    spec: &InteractiveSpec,
    rt: &AiToolRuntime,
    llm: &dyn AgentLlm,
    sink: &dyn AgentSink,
) -> Result<(), String> {
    use std::sync::atomic::Ordering;

    // 设置里关掉「允许状态变更命令」时，把变更类工具**从 tools[] 里拿掉**：
    // 暴露了再拒绝只会让模型反复尝试并烧掉步数（§3.6.1 可见性优先）。
    let policy = if spec.allow_mutating {
        agent::ToolPolicy::interactive()
    } else {
        agent::ToolPolicy::inspect_only()
    };
    let budget = agent::AgentBudget {
        max_targets: rt.profiles.len().max(1),
        ..agent::AgentBudget::default()
    };
    let mut session = agent::AgentSession::with_budget(spec.system.clone(), budget, policy);
    // 先铺历史（多轮对话），再放本轮提问 —— 顺序反了模型就会以为自己在回答上一句。
    for m in &spec.history {
        if m.role == crate::ai::Role::System {
            continue; // system 已由 `spec.system` 给出，不重复
        }
        session.transcript.push(m.clone());
    }
    session.push_user(spec.user.clone());

    let mut rows: Vec<TraceRow> = Vec::new();
    let mut conclusion = String::new();
    // 循环的收尾原因（决定 `sink.done` 的文案）
    let mut finish = agent::FinishReason::NoMoreTools;

    loop {
        if spec.cancel.load(Ordering::Relaxed) {
            session.close_pending_calls(&crate::i18n::ts("已停止".into(), "stopped".into()));
            mark_rows_stopped(&mut rows, sink);
            finish = agent::FinishReason::Cancelled;
            break;
        }

        match session.next_action() {
            agent::NextAction::RequestLlm => {
                session.begin_llm_round();
                let turn = match llm.turn(&session.transcript, session.tools_for_provider()) {
                    Ok(t) => t,
                    Err(e) => {
                        // 取消与出错必须分开：用户点了停止却看到「出错」会以为程序坏了。
                        finish = if spec.cancel.load(Ordering::Relaxed) {
                            agent::FinishReason::Cancelled
                        } else {
                            agent::FinishReason::Error(e)
                        };
                        mark_rows_stopped(&mut rows, sink);
                        break;
                    }
                };
                if turn.tool_calls.is_empty() {
                    conclusion = turn.content.clone();
                    session.record_reply(&turn.content);
                    break;
                }
                let Some(step) = session.record_tool_calls(&turn.content, turn.tool_calls.clone())
                else {
                    conclusion = turn.content.clone();
                    session.record_reply(&turn.content);
                    break;
                };
                // 先把这一轮**所有**调用的轨迹行摆出来（状态 Running / 超限就地判死），
                // 再逐条执行。反过来的话，第一条命令跑 15 秒期间用户什么都看不到。
                for (ix, call) in turn.tool_calls.iter().enumerate() {
                    let planned = step.outcomes.get(ix);
                    let (state, note) = match planned.map(|o| &o.status) {
                        Some(agent::ToolStatus::BadArgs(r)) => (TraceState::BadArgs, r.clone()),
                        _ => (TraceState::Running, TraceState::Running.label()),
                    };
                    let mut row = TraceRow::new(step.index, call.id.clone(), call.name.clone());
                    row.summary = summary_of(rt, call);
                    row.state = state;
                    row.note = note;
                    upsert_trace(&mut rows, &row, sink);
                }
                for (ix, call) in turn.tool_calls.iter().enumerate() {
                    let planned = step.outcomes.get(ix);
                    // 超出单步上限 / 未知工具：状态机已就地判死，收口即可
                    if planned
                        .map(|o| !matches!(o.status, agent::ToolStatus::Pending))
                        .unwrap_or(true)
                    {
                        if let Some(o) = planned {
                            let status = o.status.clone();
                            session.record_outcome(o.clone());
                            settle_row(
                                &mut rows,
                                sink,
                                &o.call_id,
                                TraceState::of(&status, false),
                                status.label(),
                                0,
                                o.result_text.clone(),
                            );
                        }
                        continue;
                    }
                    let planned = planned.expect("上面已判过 Some/Pending");
                    let started = std::time::Instant::now();
                    // 目标数与白名单在这里按参数重算：`judge` 内部还会再算一次并
                    // 覆盖，但白名单要拿命令原文去比，只有这一层拿得到。
                    let _ = planned;
                    let (status, text, findings) = execute_tool_call_with(
                        rt,
                        call,
                        agent::GuardContext {
                            allow_mutating: spec.allow_mutating,
                            interactive: true,
                            target_count: target_count_of_json(rt, call),
                            whitelisted: command_of(call)
                                .is_some_and(|c| crate::ai::whitelist_hit(&spec.whitelist, &c)),
                        },
                        &mut |req| {
                            // 卡片出现前先把轨迹行切到「等待确认」——
                            // 否则用户看到的是一行「执行中…」停住不动。
                            settle_row(
                                &mut rows,
                                sink,
                                &req.call_id,
                                TraceState::Waiting,
                                TraceState::Waiting.label(),
                                0,
                                String::new(),
                            );
                            sink.confirm(req)
                        },
                        sink,
                    );
                    let elapsed = started.elapsed().as_millis() as u64;
                    let anomaly = !findings.is_empty();
                    let state = TraceState::of(&status, anomaly);
                    // 明细要在 `text` 被 move 进 transcript 之前算出来 ——
                    // 它是界面上唯一能看到「哪几台、什么毛病、跑了什么」的地方（§3.9）。
                    let detail = detail_of(call, &findings, &text);
                    session.record_outcome(agent::ToolOutcome {
                        call_id: call.id.clone(),
                        tool: Tool::from_name(&call.name).unwrap_or(Tool::ListTargets),
                        summary: summary_of(rt, call),
                        status: status.clone(),
                        result_text: text,
                        elapsed_ms: elapsed,
                    });
                    settle_row(
                        &mut rows,
                        sink,
                        &call.id,
                        state,
                        trace_note(&status, findings.len()),
                        elapsed,
                        detail,
                    );
                }
            }
            agent::NextAction::AwaitConfirm => {
                // 走到这里说明有调用被留成了 `AwaitingConfirm` 而没人处理。
                // 明确收口而不是挂住：挂住的表现是界面永远停在「等待确认」。
                session.close_pending_calls(&crate::i18n::ts(
                    "等待确认的调用没有被处理，已跳过".into(),
                    "a confirm-required call was left unresolved; skipped".into(),
                ));
                mark_rows_stopped(&mut rows, sink);
                finish = agent::FinishReason::BadArgs;
                break;
            }
            agent::NextAction::Finish(reason) => {
                if let Some(note) = reason.note() {
                    conclusion = if conclusion.is_empty() {
                        note
                    } else {
                        format!("{conclusion}\n{note}")
                    };
                }
                finish = reason;
                break;
            }
        }
    }

    sink.done(&conclusion, &finish);
    Ok(())
}

/// 交互式 agent 通过通道外发的**进度事件**。
///
/// 用事件而不是直接回调 UI：agent 跑在后台线程上，而 GPUI 的实体只能在主线程
/// 更新 —— 通道是这两者之间唯一安全的桥。
#[derive(Debug, Clone, PartialEq)]
pub enum AgentUiEvent {
    /// 模型正文增量
    Delta(String),
    /// 轨迹行新增或更新
    Trace(Box<TraceRow>),
    /// 需要用户确认（`decisions` 通道上等回话）
    Confirm(Box<ConfirmRequest>),
    /// 用户已确认的任务草案（面板据此入库；见 `AgentSink::propose`）
    TaskProposed(Box<Task>),
    /// 整轮结束
    Done {
        conclusion: String,
        reason: agent::FinishReason,
    },
}

/// 通道版 [`AgentSink`]：给 UI 用。
///
/// 确认的等待有**硬超时**（[`CONFIRM_TIMEOUT_SECS`]）：用户关掉窗口、切走、
/// 或者干脆不理，后台线程不能永远挂着 —— 挂着的线程持有 SSH 连接与预算。
/// 超时按「忽略」处理并继续，而不是报错终止（用户可能只是去泡了杯咖啡）。
pub struct ChannelSink {
    tx: flume::Sender<AgentUiEvent>,
    decisions: flume::Receiver<(String, AgentDecision)>,
    /// 面板是否还活着；置 false 后确认一律按「忽略」立即返回
    alive: Arc<AtomicBool>,
}

/// 等待用户确认的硬超时（秒）。
pub const CONFIRM_TIMEOUT_SECS: u64 = 300;

impl ChannelSink {
    pub fn new(
        tx: flume::Sender<AgentUiEvent>,
        decisions: flume::Receiver<(String, AgentDecision)>,
        alive: Arc<AtomicBool>,
    ) -> Self {
        Self {
            tx,
            decisions,
            alive,
        }
    }

    /// 卡片已在面板上，等用户回话（超时/面板关闭都按「忽略」）。
    fn confirm_wait(&self, req: &ConfirmRequest) -> AgentDecision {
        if self
            .tx
            .send(AgentUiEvent::Confirm(Box::new(req.clone())))
            .is_err()
        {
            // 接收端没了（面板已关）：没有人在看这张卡片，按忽略处理。
            return AgentDecision::Deny;
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(CONFIRM_TIMEOUT_SECS);
        loop {
            if !self.alive.load(Ordering::Relaxed) {
                return AgentDecision::Deny;
            }
            match self.decisions.recv_timeout(Duration::from_millis(200)) {
                Ok((id, d)) if id == req.call_id => return d,
                // 别的调用的回话（同一轮多张卡片）：丢掉，继续等自己那张
                Ok(_) => continue,
                Err(flume::RecvTimeoutError::Timeout) => {
                    if std::time::Instant::now() > deadline {
                        return AgentDecision::Deny;
                    }
                }
                // 发送端全没了：不可能再有回话
                Err(flume::RecvTimeoutError::Disconnected) => return AgentDecision::Deny,
            }
        }
    }
}

impl AgentSink for ChannelSink {
    fn delta(&self, text: &str) {
        let _ = self.tx.send(AgentUiEvent::Delta(text.to_string()));
    }

    fn trace(&self, row: &TraceRow) {
        let _ = self.tx.send(AgentUiEvent::Trace(Box::new(row.clone())));
    }

    fn confirm(&self, req: &ConfirmRequest) -> AgentDecision {
        crate::ai::debug_log(|| {
            format!(
                "[confirm] 请求确认：{}\n  摘要：{}\n  原因：{}\n  命令：{}\n  目标数：{}",
                req.tool, req.summary, req.reason, req.command, req.target_count
            )
        });
        let decision = self.confirm_wait(req);
        crate::ai::debug_log(|| format!("[confirm] {} 的用户决定：{decision:?}", req.tool));
        decision
    }

    fn done(&self, conclusion: &str, reason: &agent::FinishReason) {
        let _ = self.tx.send(AgentUiEvent::Done {
            conclusion: conclusion.to_string(),
            reason: reason.clone(),
        });
    }

    fn propose(&self, task: &Task) {
        let _ = self
            .tx
            .send(AgentUiEvent::TaskProposed(Box::new(task.clone())));
    }
}

/// 起一个后台线程跑交互式 agent，进度经 [`AgentUiEvent`] 外发。
///
/// 返回线程句柄（调用方一般不 join：面板关闭时靠 `alive` 与 `cancel` 收尾）。
///
/// 为什么开线程而不是 `cx.spawn`：工具执行是**阻塞**的（SSH exec、SFTP 下载），
/// 放在 gpui 的异步执行器上会把整个界面拖住 —— 那正是「AI 一巡检，界面就卡死」
/// 的成因。通道 + 线程让面板始终可交互（包括点「停止」）。
pub fn spawn_interactive_agent(
    spec: InteractiveSpec,
    conn: Arc<ConnectionStore>,
    exec: Arc<dyn ExecBackend>,
    tx: flume::Sender<AgentUiEvent>,
    decisions: flume::Receiver<(String, AgentDecision)>,
    alive: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    let rt = AiToolRuntime {
        conn,
        backend: exec,
        audit_path: spec.audit_path.clone(),
        download_dir: spec.download_dir.clone(),
        profiles: spec.profiles.clone(),
        thresholds: spec.thresholds,
        cancel: spec.cancel.clone(),
    };
    std::thread::Builder::new()
        .name("rs-agent-interactive".to_string())
        .spawn(move || {
            let sink = ChannelSink::new(tx.clone(), decisions, alive);
            let llm = {
                let deltas = tx.clone();
                StreamingLlm::new(spec.cfg.clone(), spec.api_key.clone(), spec.cancel.clone())
                    .with_delta_sink(Arc::new(move |d: &str| {
                        let _ = deltas.send(AgentUiEvent::Delta(d.to_string()));
                    }))
            };
            // 错误也要收尾：面板若等不到 `Done` 就会永远停在「运行中」。
            if let Err(e) = run_interactive_agent(&spec, &rt, &llm, &sink) {
                sink.done(
                    &crate::i18n::ts(format!("出错：{e}"), format!("Error: {e}")),
                    &agent::FinishReason::Error(e),
                );
            }
        })
        .unwrap_or_else(|e| panic!("无法启动 agent 线程：{e}"))
}

/// 把一条轨迹行写进列表并发给 sink（同一 `call_id` 只更新，不追加）。
fn upsert_trace(rows: &mut Vec<TraceRow>, row: &TraceRow, sink: &dyn AgentSink) {
    let ix = agent::trace::upsert(rows, row.clone());
    if let Some(r) = rows.get(ix) {
        debug_trace(r);
        sink.trace(r);
    }
}

/// 就地更新一行的状态并发给 sink。
#[allow(clippy::too_many_arguments)]
fn settle_row(
    rows: &mut [TraceRow],
    sink: &dyn AgentSink,
    call_id: &str,
    state: TraceState,
    note: impl Into<String>,
    elapsed_ms: u64,
    detail: String,
) {
    let Some(r) = rows.iter_mut().find(|r| r.call_id == call_id) else {
        return;
    };
    r.settle(state, note, elapsed_ms);
    if !detail.is_empty() {
        r.detail = detail;
    }
    debug_trace(r);
    sink.trace(r);
}

/// 轨迹行落一条调试日志（明细只记长度：正文在 `settle_row` 的 detail 里，
/// 可能有几十 KB，日志里留全量只会把主线淹掉）。
fn debug_trace(r: &TraceRow) {
    crate::ai::debug_log(|| {
        format!(
            "[trace] {} {} · {} · {} · 明细 {} 字符 · {}ms",
            r.tool,
            r.state.tag(),
            r.summary,
            r.note,
            r.detail.chars().count(),
            r.elapsed_ms
        )
    });
}

/// 调试日志里长文本的截断上限（工具输出动辄几十 KB）。
const DEBUG_TEXT_MAX: usize = 4000;

/// 截断到 [`DEBUG_TEXT_MAX`]，并附上原始长度。
fn debug_text(s: &str) -> String {
    let total = s.chars().count();
    if total <= DEBUG_TEXT_MAX {
        return s.to_string();
    }
    let mut out: String = s.chars().take(DEBUG_TEXT_MAX).collect();
    out.push_str(&format!("\n…（截断，共 {total} 字符）"));
    out
}

/// 裁决结果的一句话描述（调试日志用）。
fn disposition_debug(d: &Disposition) -> String {
    match d {
        Disposition::Run => "放行".to_string(),
        Disposition::Confirm(r) => format!("需确认：{r}"),
        Disposition::Blocked(r) => format!("已拦截：{r}"),
        Disposition::Disabled(r) => format!("已禁用：{r}"),
    }
}

/// 把未收口的行标记为已停止并发给 sink（AC-7：部分轨迹保留并标记）。
fn mark_rows_stopped(rows: &mut [TraceRow], sink: &dyn AgentSink) {
    let before: Vec<(String, TraceState)> =
        rows.iter().map(|r| (r.call_id.clone(), r.state)).collect();
    agent::mark_stopped(rows);
    for r in rows.iter() {
        if before
            .iter()
            .any(|(id, st)| id == &r.call_id && *st != r.state)
        {
            debug_trace(r);
            sink.trace(r);
        }
    }
}

/// 一次调用的参数摘要。
///
/// 目标名只取**这次调用真正要连的**那几台：早先这里传的是整个连接库的展示名，
/// 于是 `run_command` 选中 2 台也会在轨迹上把 30 台连接印满一行
/// （2026-09-22 运行反馈「不要展示所有连接，只展示需要连的链接」）。
fn summary_of(rt: &AiToolRuntime, call: &crate::ai::ToolCall) -> String {
    let Ok(json) = call.args() else {
        return call.name.clone();
    };
    let Some(tool) = Tool::from_name(&call.name) else {
        return call.name.clone();
    };
    match agent::parse_args(tool, &json) {
        Ok(args) => {
            let names = target_names_of(rt, &args);
            agent::call_summary(agent::tools::SummaryInput {
                tool,
                args: &args,
                names: &names,
            })
        }
        Err(_) => call.name.clone(),
    }
}

/// 本次调用涉及的目标展示名。
///
/// 只对 `TargetSelector::Profiles` 有用（`call_summary` 仅在该分支 join 这些名字，
/// 分组/标签/全部走各自的 `describe()`）。解析不出来的 id **原样保留**：
/// 那正是「模型点错了名」的证据，比空白摘要可读。
fn target_names_of(rt: &AiToolRuntime, args: &ToolArgs) -> Vec<String> {
    let selector = match args {
        ToolArgs::ListTargets { selector }
        | ToolArgs::InspectLoad { selector, .. }
        | ToolArgs::InspectLog { selector, .. }
        | ToolArgs::ReadFile { selector, .. }
        | ToolArgs::InspectService { selector, .. }
        | ToolArgs::InspectPorts { selector, .. }
        | ToolArgs::CheckCertificate { selector, .. }
        | ToolArgs::FindProcess { selector, .. }
        | ToolArgs::RunCommand { selector, .. }
        | ToolArgs::CheckReachability { selector, .. } => selector,
        ToolArgs::DownloadFile { profile_id, .. } => {
            return vec![profile_name_or_id(rt, profile_id)];
        }
        ToolArgs::ProposeTask { .. } => return Vec::new(),
    };
    match selector {
        TargetSelector::Profiles(ids) => ids
            .iter()
            .map(|id| profile_name_or_id(rt, id))
            .collect::<Vec<_>>(),
        // 其余选择器由 `call_summary` 自己描述（分组名/标签名/「全部连接」）
        _ => Vec::new(),
    }
}

/// 轨迹与确认卡片里显示名字比显示 id 可读；查不到时退回 id。
fn profile_name_or_id(rt: &AiToolRuntime, id: &str) -> String {
    rt.find_profile(id)
        .map(|p| p.name.clone())
        .unwrap_or_else(|| id.to_string())
}

/// 该调用参数里声明的目标数（白名单/批量判定用；解析不出来按 0）。
fn target_count_of_json(rt: &AiToolRuntime, call: &crate::ai::ToolCall) -> usize {
    let Ok(json) = call.args() else { return 0 };
    let Some(tool) = Tool::from_name(&call.name) else {
        return 0;
    };
    match agent::parse_args(tool, &json) {
        Ok(args) => {
            let _ = rt;
            target_count_of(rt, &args)
        }
        Err(_) => 0,
    }
}

/// 命令原文（白名单比对用；非命令类工具为 `None`）。
fn command_of(call: &crate::ai::ToolCall) -> Option<String> {
    let json = call.args().ok()?;
    json["command"].as_str().map(str::to_string)
}

/// 轨迹明细正文：命令类调用先亮出**实际执行的命令**，再是异常与原始输出。
///
/// 一行 `run_command` 的摘要在界面上只够放命令首行（见 `call_summary`），
/// 展开明细此前又只有远端回显 —— 用户点开一行「执行命令」却看不到到底跑了什么
/// （2026-09-22 运行反馈）。`propose_task` 的参数里也有 `command`，但它只是草案
/// 内容、并没有执行，故按 [`Tool::RunCommand`] 精确判定而不是「有 command 就加」。
fn detail_of(call: &crate::ai::ToolCall, findings: &[Finding], raw: &str) -> String {
    let body = super::inspect::trace_detail(findings, raw);
    if Tool::from_name(&call.name) != Some(Tool::RunCommand) {
        return body;
    }
    // 多行命令原样保留：`sh -c` 里换行是语义的一部分，压成一行会看不出来。
    let Some(cmd) = command_of(call).filter(|c| !c.trim().is_empty()) else {
        return body;
    };
    let head = crate::i18n::ts(format!("执行的命令：\n{cmd}"), format!("Command:\n{cmd}"));
    if body.trim().is_empty() {
        head
    } else {
        format!("{head}\n\n{body}")
    }
}

/// 轨迹行上那句结果说明：状态文案 + 发现数（用户一眼能看出「查到了什么」）。
///
/// 有异常时**不再缀「完成」**：`run_command` 非零退出在 `ToolStatus` 上是 `Ok`
/// （命令确实跑完了），但界面上写「完成 · 发现 11 项异常」会让用户以为那 11 项
/// 只是顺带提一句。这里直接以异常数开头，配合 warning 色。
fn trace_note(status: &agent::ToolStatus, findings: usize) -> String {
    if matches!(status, agent::ToolStatus::Ok) && findings > 0 {
        return crate::i18n::ts(
            format!("发现 {findings} 项异常（点开看明细）"),
            format!("{findings} issue(s) — expand for details"),
        );
    }
    status.label()
}

/// 执行一次工具调用（策略参数化）。
///
/// `guard` 给出裁决上下文（交互式 / 无人值守、目标数、是否命中白名单）；
/// `decide` 只在裁决结果是 `Confirm` 时被调用，由调用方决定「谁来确认」。
///
/// 本体在 [`execute_tool_call_inner`]；这里只负责给调试日志补一条统一的
/// 「结果」收尾行 —— 提前返回的分支多（参数错、禁用、拦截、用户忽略），
/// 逐个加日志既啰嗦又容易漏，包一层最稳。
fn execute_tool_call_with(
    rt: &AiToolRuntime,
    call: &crate::ai::ToolCall,
    guard: agent::GuardContext,
    decide: &mut dyn FnMut(&ConfirmRequest) -> AgentDecision,
    sink: &dyn AgentSink,
) -> (agent::ToolStatus, String, Vec<Finding>) {
    let out = execute_tool_call_inner(rt, call, guard, decide, sink);
    crate::ai::debug_log(|| {
        format!(
            "[tool] {}#{} 结果：{} · 回注 {} 字符\n{}",
            call.name,
            call.id,
            out.0.label(),
            out.1.chars().count(),
            debug_text(&out.1)
        )
    });
    out
}

fn execute_tool_call_inner(
    rt: &AiToolRuntime,
    call: &crate::ai::ToolCall,
    guard: agent::GuardContext,
    decide: &mut dyn FnMut(&ConfirmRequest) -> AgentDecision,
    sink: &dyn AgentSink,
) -> (agent::ToolStatus, String, Vec<Finding>) {
    let Some(tool) = Tool::from_name(&call.name) else {
        return (
            agent::ToolStatus::BadArgs(format!("未知工具：{}", call.name)),
            format!("未知工具：{}", call.name),
            Vec::new(),
        );
    };
    let args_json = match call.args() {
        Ok(v) => v,
        Err(e) => return (agent::ToolStatus::BadArgs(e.clone()), e, Vec::new()),
    };
    let args = match agent::parse_args(tool, &args_json) {
        Ok(a) => a,
        Err(e) => return (agent::ToolStatus::BadArgs(e.clone()), e, Vec::new()),
    };
    let target_count = target_count_of(rt, &args);
    crate::ai::debug_log(|| {
        format!(
            "[tool] → 调用 {}#{} · 目标数 {target_count} · 参数 {}",
            call.name,
            call.id,
            debug_text(&args_json.to_string())
        )
    });
    let verdict = agent::judge(
        tool,
        &args,
        agent::GuardContext {
            target_count,
            ..guard
        },
    );
    crate::ai::debug_log(|| {
        format!(
            "[tool] {}#{} 裁决：{}",
            call.name,
            call.id,
            disposition_debug(&verdict)
        )
    });
    match verdict {
        Disposition::Run => {}
        Disposition::Disabled(r) => return (agent::ToolStatus::Disabled(r.clone()), r, Vec::new()),
        Disposition::Blocked(r) => return (agent::ToolStatus::Blocked(r.clone()), r, Vec::new()),
        Disposition::Confirm(r) => {
            if !guard.interactive {
                // 无人值守不弹确认：明确回注原因，让模型换只读手段
                let msg = format!("{}（无人值守场景不允许，请改用只读检查）", r);
                return (agent::ToolStatus::Disabled(msg.clone()), msg, Vec::new());
            }
            let req = ConfirmRequest {
                step: 0,
                call_id: call.id.clone(),
                tool: call.name.clone(),
                summary: summary_of(rt, call),
                reason: r,
                target_count,
                command: command_of(call).unwrap_or_default(),
                // 批量永不走白名单（§7.4）——卡片据此隐藏「加入白名单」动作
                whitelistable: target_count <= 1,
            };
            match decide(&req) {
                AgentDecision::Approve => {}
                AgentDecision::Deny => {
                    let msg = crate::i18n::ts(
                        "用户已忽略该操作".into(),
                        "the user dismissed this operation".into(),
                    );
                    return (agent::ToolStatus::Rejected, msg, Vec::new());
                }
            }
        }
    }

    execute_approved_tool(rt, &args, &args_json, sink)
}

/// 已通过裁决的工具执行体（`execute_tool_call_with` 的后半段）。
///
/// 拆出来是为了让「裁决」与「执行」各自可读：裁决只决定**能不能**跑，
/// 这里是**怎么跑**。两者混在一个函数里时，安全规则的改动很容易被执行的
/// 分支细节淹没。
fn execute_approved_tool(
    rt: &AiToolRuntime,
    args: &ToolArgs,
    args_json: &serde_json::Value,
    sink: &dyn AgentSink,
) -> (agent::ToolStatus, String, Vec<Finding>) {
    match args {
        ToolArgs::ListTargets { .. } => {
            let lines: Vec<String> = rt
                .profiles
                .iter()
                .map(|p| format!("{} ({}) {}@{}", p.name, p.id, p.username, p.host))
                .collect();
            let text = lines.join("\n");
            (agent::ToolStatus::Ok, text, Vec::new())
        }
        ToolArgs::InspectLoad {
            selector,
            timeout_secs,
            ..
        } => {
            let Some(profiles) = select_profiles(rt, selector) else {
                let e = "目标无法解析".to_string();
                return (agent::ToolStatus::BadArgs(e.clone()), e, Vec::new());
            };
            let reports = run_probe_load(rt, &profiles, Duration::from_secs(*timeout_secs));
            let findings = collect_findings(&reports);
            let text = render_reports(&reports, super::inspect::OkDetail::NamesOnly);
            (agent::ToolStatus::Ok, text, findings)
        }
        ToolArgs::InspectLog {
            selector,
            path,
            level,
            tail_lines,
            pattern,
            timeout_secs,
        } => {
            let Some(profiles) = select_profiles(rt, selector) else {
                let e = "目标无法解析".to_string();
                return (agent::ToolStatus::BadArgs(e.clone()), e, Vec::new());
            };
            let pat = pattern
                .clone()
                .unwrap_or_else(|| level.default_pattern().to_string());
            let cmd = log_probe_command(path, &pat, *tail_lines, LOG_SAMPLE_LINES);
            // `path` 与 `level` 都是借用值，闭包要跨线程 -> 必须先复制成自有值
            let path_for_build = path.clone();
            let level = *level;
            let reports = run_probe(
                rt.backend.clone(),
                rt.cancel.clone(),
                rt.audit_path.clone(),
                &profiles,
                cmd,
                Duration::from_secs(*timeout_secs),
                move |pid, name, res| {
                    let out = CommandOutput {
                        exit_code: res.exit_code,
                        stdout: res.stdout.clone(),
                        stderr: res.stderr.clone(),
                        timed_out: res.timed_out,
                    };
                    let probe = parse_log_probe(&out, level, &path_for_build);
                    let (anomaly, summary, evidence, mut findings) =
                        judge_log(&probe, &path_for_build, level);
                    bind_findings(&mut findings, pid, name);
                    TargetReport {
                        profile_id: pid.to_string(),
                        profile_name: name.to_string(),
                        connected: true,
                        exec_ok: !matches!(probe, LogProbe::Failed(_)),
                        anomaly,
                        summary,
                        detail: evidence,
                        findings,
                    }
                },
            );
            let findings = collect_findings(&reports);
            let text = render_reports(&reports, super::inspect::OkDetail::NamesOnly);
            (agent::ToolStatus::Ok, text, findings)
        }
        ToolArgs::CheckReachability {
            selector,
            port,
            timeout_secs,
        } => {
            let Some(profiles) = select_profiles(rt, selector) else {
                let e = "目标无法解析".to_string();
                return (agent::ToolStatus::BadArgs(e.clone()), e, Vec::new());
            };
            let mut lines = Vec::new();
            let mut findings = Vec::new();
            for p in &profiles {
                // 给了 port 就探那个端口。忽略它、永远探 profile 自己的 SSH 端口，
                // 模型问「80 通不通」只会拿到一句 `127.0.0.1:2222 ok`，
                // 然后把这当成 80 的结论回报给用户。
                let target_port = port.unwrap_or(p.port);
                let addr = format!("{}:{}", p.host, target_port);
                let started = std::time::Instant::now();
                let ok = addr
                    .parse()
                    .ok()
                    .and_then(|a| {
                        std::net::TcpStream::connect_timeout(
                            &a,
                            Duration::from_secs((*timeout_secs).min(10)),
                        )
                        .ok()
                    })
                    .is_some();
                let elapsed_ms = started.elapsed().as_millis();
                lines.push(format!(
                    "{} {} {} {elapsed_ms}ms",
                    p.name,
                    addr,
                    if ok { "ok" } else { "unreachable" }
                ));
                if !ok {
                    let reason =
                        crate::i18n::ts(format!("{addr} 不可达"), format!("{addr} unreachable"));
                    findings.push(super::inspect::inspection_failure_finding(
                        &p.id, &p.name, &reason, false,
                    ));
                }
            }
            (agent::ToolStatus::Ok, lines.join("\n"), findings)
        }
        ToolArgs::RunCommand {
            selector,
            timeout_secs,
            ..
        } => {
            // Safe 级直接到这里；Confirm 级要经上面的确认卡片批准后才到。
            let Some(profiles) = select_profiles(rt, selector) else {
                let e = "目标无法解析".to_string();
                return (agent::ToolStatus::BadArgs(e.clone()), e, Vec::new());
            };
            let cmd = match args_json["command"].as_str() {
                Some(c) => c.to_string(),
                None => {
                    let e = "缺少 command".to_string();
                    return (agent::ToolStatus::BadArgs(e.clone()), e, Vec::new());
                }
            };
            let reports = run_probe(
                rt.backend.clone(),
                rt.cancel.clone(),
                rt.audit_path.clone(),
                &profiles,
                cmd,
                Duration::from_secs(*timeout_secs),
                |pid, name, res| {
                    let out = CommandOutput {
                        exit_code: res.exit_code,
                        stdout: res.stdout.clone(),
                        stderr: res.stderr.clone(),
                        timed_out: res.timed_out,
                    };
                    let (mut anomaly, summary, mut findings) = judge_exit(&out, &[]);
                    bind_findings(&mut findings, pid, name);
                    if !res.is_ok() && findings.is_empty() {
                        anomaly = false;
                        findings.push(super::inspect::inspection_failure_finding(
                            pid,
                            name,
                            &res.failure_reason().unwrap_or_default(),
                            true,
                        ));
                    }
                    TargetReport {
                        profile_id: pid.to_string(),
                        profile_name: name.to_string(),
                        connected: true,
                        exec_ok: res.is_ok(),
                        anomaly,
                        summary,
                        detail: Some(command_evidence(&out)),
                        findings,
                    }
                },
            );
            let findings = collect_findings(&reports);
            // Full：模型的结论就建立在这些输出上（「哪些端口在监听」之类），
            // 只回一句「N 台正常」等于没给证据。
            let text = render_reports(&reports, super::inspect::OkDetail::Full);
            (agent::ToolStatus::Ok, text, findings)
        }
        ToolArgs::DownloadFile {
            profile_id,
            remote_path,
            local_dir,
        } => {
            let Some(p) = rt.find_profile(profile_id) else {
                let e = format!("找不到连接：{profile_id}");
                return (agent::ToolStatus::BadArgs(e.clone()), e, Vec::new());
            };
            if let Err(e) = crate::tasks::types::validate_remote_path(remote_path) {
                let e = e.to_string();
                return (agent::ToolStatus::BadArgs(e.clone()), e, Vec::new());
            }
            let dir = local_dir
                .clone()
                .map(PathBuf::from)
                .unwrap_or_else(|| rt.download_dir.clone());
            // 远端文件名取最后一段；空/以 `/` 结尾的路径由上面的校验拦下。
            let file_name = remote_path
                .rsplit('/')
                .find(|s| !s.is_empty())
                .unwrap_or("download.bin");
            let local = dir.join(file_name);
            match crate::sftp::download_once(
                p,
                remote_path,
                &local,
                Duration::from_secs(DOWNLOAD_TIMEOUT_SECS),
                rt.backend.global_proxy(),
            ) {
                Ok(d) => {
                    let bytes = d.bytes;
                    let text = crate::i18n::ts(
                        format!("已下载 {bytes} 字节到 {}", local.display()),
                        format!("downloaded {bytes} bytes to {}", local.display()),
                    );
                    (agent::ToolStatus::Ok, text, Vec::new())
                }
                Err(e) => {
                    let msg =
                        crate::i18n::ts(format!("下载失败：{e}"), format!("download failed: {e}"));
                    (agent::ToolStatus::Failed(msg.clone()), msg, Vec::new())
                }
            }
        }
        ToolArgs::ReadFile {
            selector,
            path,
            start_line,
            max_lines,
            timeout_secs,
        } => {
            let Some(profiles) = select_profiles(rt, selector) else {
                let e = "目标无法解析".to_string();
                return (agent::ToolStatus::BadArgs(e.clone()), e, Vec::new());
            };
            let cmd = read_probe_command(path, *start_line, *max_lines);
            let path = path.clone();
            let (start, max) = (*start_line, *max_lines);
            let reports = run_probe_readonly(
                rt,
                &profiles,
                cmd,
                Duration::from_secs(*timeout_secs),
                move |out| {
                    let probe = parse_read_probe(out);
                    judge_read(&probe, &path, start, max)
                },
            );
            finish_readonly(reports)
        }
        ToolArgs::InspectService {
            selector,
            unit,
            timeout_secs,
        } => {
            let Some(profiles) = select_profiles(rt, selector) else {
                let e = "目标无法解析".to_string();
                return (agent::ToolStatus::BadArgs(e.clone()), e, Vec::new());
            };
            let cmd = service_probe_command(unit);
            let unit = unit.clone();
            let reports = run_probe_readonly(
                rt,
                &profiles,
                cmd,
                Duration::from_secs(*timeout_secs),
                move |out| judge_service(parse_service_probe(out), &unit),
            );
            finish_readonly(reports)
        }
        ToolArgs::InspectPorts {
            selector,
            port,
            timeout_secs,
        } => {
            let Some(profiles) = select_profiles(rt, selector) else {
                let e = "目标无法解析".to_string();
                return (agent::ToolStatus::BadArgs(e.clone()), e, Vec::new());
            };
            let want = *port;
            let reports = run_probe_readonly(
                rt,
                &profiles,
                ports_probe_command(),
                Duration::from_secs(*timeout_secs),
                move |out| judge_ports(parse_ports_probe(out), want),
            );
            finish_readonly(reports)
        }
        ToolArgs::CheckCertificate {
            selector,
            host,
            port,
            server_name,
            timeout_secs,
        } => {
            let Some(profiles) = select_profiles(rt, selector) else {
                let e = "目标无法解析".to_string();
                return (agent::ToolStatus::BadArgs(e.clone()), e, Vec::new());
            };
            let cmd = cert_probe_command(host, *port, server_name.as_deref());
            let host = host.clone();
            let want = *port;
            let reports = run_probe_readonly(
                rt,
                &profiles,
                cmd,
                Duration::from_secs(*timeout_secs),
                move |out| judge_cert(parse_cert(out, chrono::Utc::now()), &host, want),
            );
            finish_readonly(reports)
        }
        ToolArgs::FindProcess {
            selector,
            name,
            port,
            timeout_secs,
        } => {
            let Some(profiles) = select_profiles(rt, selector) else {
                let e = "目标无法解析".to_string();
                return (agent::ToolStatus::BadArgs(e.clone()), e, Vec::new());
            };
            let cmd = process_probe_command(name.as_deref(), *port);
            let want_name = name.clone();
            let want_port = *port;
            let reports = run_probe_readonly(
                rt,
                &profiles,
                cmd,
                Duration::from_secs(*timeout_secs),
                move |out| {
                    let probe = parse_process_probe(out, want_name.is_some(), want_port.is_some());
                    judge_process(probe, want_name.as_deref(), want_port)
                },
            );
            finish_readonly(reports)
        }
        ToolArgs::ProposeTask { .. } => {
            // 到这里说明**用户已经在确认卡片上点了「执行」**（`judge` 把
            // `propose_task` 固定判为 Confirm）。所以这里真的落一条任务草案，
            // 而不是只回注一句「打算建什么」—— 后者会让模型对用户宣称
            // 「已为你创建」，而任务列表里什么都没有。
            let now = crate::connections::unix_now_secs();
            match agent::proposal_to_task(args, rt.conn.as_ref(), now) {
                Ok(task) => {
                    let preview = agent::describe_proposal(args, rt.conn.as_ref());
                    // 建出来的任务默认**停用**（见 `proposal::to_task`）：
                    // 用户点的是「允许建这条草案」，不是「让它现在就开始跑」。
                    sink.propose(&task);
                    let text = crate::i18n::ts(
                        format!("已创建任务草案（默认停用，可在定时任务面板启用）：{preview}"),
                        format!(
                            "task draft created (disabled by default; enable it in the tasks panel): {preview}"
                        ),
                    );
                    (agent::ToolStatus::Ok, text, Vec::new())
                }
                Err(e) => {
                    let msg = crate::i18n::ts(
                        format!("任务草案无法创建：{e}"),
                        format!("could not create the task draft: {e}"),
                    );
                    // 草案建不出来属于**参数问题**而不是执行失败：
                    // 回注成因让模型自己改参数重试，比直接终止有用。
                    (agent::ToolStatus::BadArgs(msg.clone()), msg, Vec::new())
                }
            }
        }
    }
}

/// 计算一次调用涉及多少台目标（白名单/批量的判定要用）。
fn target_count_of(rt: &AiToolRuntime, args: &ToolArgs) -> usize {
    match args {
        ToolArgs::ListTargets { .. } => 0,
        ToolArgs::DownloadFile { .. } | ToolArgs::ProposeTask { .. } => 1,
        ToolArgs::InspectLoad { selector, .. }
        | ToolArgs::InspectLog { selector, .. }
        | ToolArgs::ReadFile { selector, .. }
        | ToolArgs::InspectService { selector, .. }
        | ToolArgs::InspectPorts { selector, .. }
        | ToolArgs::CheckCertificate { selector, .. }
        | ToolArgs::FindProcess { selector, .. }
        | ToolArgs::RunCommand { selector, .. }
        | ToolArgs::CheckReachability { selector, .. } => {
            select_profiles(rt, selector).map(|p| p.len()).unwrap_or(0)
        }
    }
}

/// 把选择器解析为运行时可执行的连接（限定在本次巡检的目标集合内）。
fn select_profiles(
    rt: &AiToolRuntime,
    selector: &TargetSelector,
) -> Option<Vec<ConnectionProfile>> {
    match selector {
        TargetSelector::All => Some(rt.profiles.clone()),
        TargetSelector::Profiles(ids) => Some(
            ids.iter()
                .filter_map(|id| rt.find_profile(id).cloned())
                .collect(),
        ),
        TargetSelector::Group(g) => {
            let ids = resolve_targets(&rt.conn, &TargetSelector::Group(g.clone())).ok()?;
            Some(
                ids.iter()
                    .filter_map(|id| rt.find_profile(id).cloned())
                    .collect(),
            )
        }
        TargetSelector::Tag(t) => {
            let ids = resolve_targets(&rt.conn, &TargetSelector::Tag(t.clone())).ok()?;
            Some(
                ids.iter()
                    .filter_map(|id| rt.find_profile(id).cloned())
                    .collect(),
            )
        }
    }
}

/// 负载巡检的命令：**连采两次**。
///
/// `/proc/stat` 的 CPU 是累计计数器，必须有两次采样做差才能算使用率
/// （见 `ssh::parse_proc_snapshot`）。只采一次的话 `cpu_percent` 恒为 0，
/// 于是「CPU 超阈值」这条判定**永远不会触发** —— 而且是静默失效，
/// 报告里只会显示「CPU 0%」。
const PROC_PROBE_TWICE: &str = "cat /proc/stat /proc/meminfo; \
     echo __NETSPLIT__; sleep 1; \
     cat /proc/stat /proc/meminfo; echo __NETDEV__; cat /proc/net/dev; \
     echo __HOST__; hostname 2>/dev/null; \
     echo __IP__; hostname -I 2>/dev/null; ip -o -4 addr show 2>/dev/null; \
     echo __UPTIME__; cat /proc/uptime 2>/dev/null; \
     echo __LOADAVG__; cat /proc/loadavg 2>/dev/null; \
     echo __DF__; df -P -k 2>/dev/null; \
     echo __DFH__; df -h -P 2>/dev/null";

/// 把连采两次的输出切成「第一份快照」与「第二份快照」。
///
/// 第一份用于给 `parse_proc_snapshot` 垫底，第二份（连同前者）才是有效采样。
fn split_double_probe(text: &str) -> (String, String) {
    match text.split_once("__NETSPLIT__") {
        Some((_first, rest)) => {
            // 第二份的结尾保留全部附带段（loadavg / df 在最后一份里）
            (text.to_string(), rest.to_string())
        }
        None => (text.to_string(), text.to_string()),
    }
}

/// 跑一次负载探针并判定。
fn run_probe_load(
    rt: &AiToolRuntime,
    profiles: &[ConnectionProfile],
    timeout: Duration,
) -> Vec<TargetReport> {
    // 阈值先复制成纯值：闭包要跨线程，捕获 `rt` 会让借用活过函数体
    let thresholds = rt.thresholds;
    run_probe(
        rt.backend.clone(),
        rt.cancel.clone(),
        rt.audit_path.clone(),
        profiles,
        PROC_PROBE_TWICE.to_string(),
        timeout,
        move |pid, name, res| {
            if !res.is_ok() {
                return TargetReport::failure(
                    pid,
                    name,
                    true,
                    res.failure_reason().unwrap_or_default(),
                );
            }
            // 两次采样做差才算得出 CPU；顺序不能反（差值为负会被 saturating 成 0）
            let (first, second) = split_double_probe(&res.stdout);
            let mut prev = crate::ssh::ProcSnapshot::default();
            let _ = parse_load(&first, &mut prev);
            let metrics = parse_load(&second, &mut prev);
            let (mut findings, lines) = judge_load(&metrics, &thresholds, &[]);
            bind_findings(&mut findings, pid, name);
            let anomaly = findings.iter().any(|f| f.kind == FindingKind::Anomaly);
            TargetReport {
                profile_id: pid.to_string(),
                profile_name: name.to_string(),
                connected: true,
                exec_ok: true,
                anomaly,
                summary: join_summary(&lines),
                detail: Some(res.stdout.clone()),
                findings,
            }
        },
    )
}

/// 只读五件套共用的探针流水线：构造 `CommandOutput` → 判定 → 补齐目标名 → 报告。
///
/// 五个工具的命令与判定各不相同，但这段胶水完全一样。收在一处是因为漏一处
/// `bind_findings` 的表现是「findings 全都没有目标名」，而那种 bug 只在
/// 多目标且真出异常时才露头。
fn run_probe_readonly(
    rt: &AiToolRuntime,
    profiles: &[ConnectionProfile],
    cmd: String,
    timeout: Duration,
    build: impl Fn(&CommandOutput) -> (bool, String, Option<String>, Vec<Finding>)
        + Send
        + Sync
        + 'static,
) -> Vec<TargetReport> {
    run_probe(
        rt.backend.clone(),
        rt.cancel.clone(),
        rt.audit_path.clone(),
        profiles,
        cmd,
        timeout,
        move |pid, name, res| {
            let out = CommandOutput {
                exit_code: res.exit_code,
                stdout: res.stdout.clone(),
                stderr: res.stderr.clone(),
                timed_out: res.timed_out,
            };
            let (anomaly, summary, evidence, mut findings) = build(&out);
            bind_findings(&mut findings, pid, name);
            // 「巡检失败」的 finding 就是 exec_ok=false 的判据：聚合时它决定这台
            // 进「失败」还是「正常」那一组（见 `inspect::aggregate_for_model`）
            let exec_ok = !findings
                .iter()
                .any(|f| f.kind == FindingKind::InspectionFailed);
            let detail = evidence.or_else(|| {
                let raw = out.stdout.trim();
                (!raw.is_empty()).then(|| out.stdout.clone())
            });
            TargetReport {
                profile_id: pid.to_string(),
                profile_name: name.to_string(),
                connected: true,
                exec_ok,
                anomaly,
                summary,
                detail,
                findings,
            }
        },
    )
}

/// 只读五件套的统一收尾。
///
/// 一律 `OkDetail::Full`：这五个工具的输出**本身就是答案**（文件内容、服务状态、
/// 端口清单），折成「N 台正常」等于把要查的东西丢掉。
fn finish_readonly(reports: Vec<TargetReport>) -> (agent::ToolStatus, String, Vec<Finding>) {
    let findings = collect_findings(&reports);
    let text = render_reports(&reports, super::inspect::OkDetail::Full);
    (agent::ToolStatus::Ok, text, findings)
}

/// 并发跑一条命令，逐台交给 `build` 生成报告。
///
/// 只接 `backend` 与 `cancel`（而不是整个 [`AiToolRuntime`]）：
/// `build` 闭包要跨越工作线程，捕获 `rt` 会让借用活过函数体；
/// 传入它真正需要的两样东西，闭包就能干净地 `move`。
fn run_probe(
    backend: Arc<dyn ExecBackend>,
    cancel: Arc<AtomicBool>,
    audit_path: PathBuf,
    profiles: &[ConnectionProfile],
    cmd: String,
    timeout: Duration,
    build: impl Fn(&str, &str, &ExecResult) -> TargetReport + Send + Sync + 'static,
) -> Vec<TargetReport> {
    if profiles.is_empty() {
        return Vec::new();
    }
    let order: Vec<String> = profiles.iter().map(|p| p.id.clone()).collect();
    let collected: Arc<parking_lot::Mutex<Vec<TargetReport>>> =
        Arc::new(parking_lot::Mutex::new(Vec::new()));
    let sink_target = collected.clone();
    let names: Vec<(String, String)> = profiles
        .iter()
        .map(|p| (p.id.clone(), p.name.clone()))
        .collect();
    let handle = exec::exec_batch_with(
        backend,
        profiles.to_vec(),
        cmd,
        exec::BatchSpec::new(timeout, cancel)
            .with_concurrency(DEFAULT_CONCURRENCY)
            .with_audit(audit_path, "agent_tool"),
        move |pid, res| {
            let name = names
                .iter()
                .find(|(id, _)| id == pid)
                .map(|(_, n)| n.as_str())
                .unwrap_or(pid);
            sink_target.lock().push(build(pid, name, res));
        },
    );
    handle.join();
    let mut out = collected.lock().clone();
    out.sort_by_key(|r| {
        order
            .iter()
            .position(|o| o == &r.profile_id)
            .unwrap_or(usize::MAX)
    });
    out
}

fn collect_findings(reports: &[TargetReport]) -> Vec<Finding> {
    reports.iter().flat_map(|r| r.findings.clone()).collect()
}

/// 把逐台报告渲染成给模型看的文本。
///
/// **走聚合而不是全量拼接**（v1.1 §2.2.3）：50 台各 4 KiB 会被上下文预算首尾截断，
/// 中间那几十台（可能正是问题机器）会被静默丢掉。聚合后模型看到的是
/// 「异常 3 台 + 摘要 + 正常 N 台名单」（`ok_detail` 决定正常机器是否也逐台展开）。
fn render_reports(reports: &[TargetReport], ok_detail: super::inspect::OkDetail) -> String {
    super::inspect::aggregate_for_model(reports, ok_detail).render()
}

/// `run_command` 每台的证据文本：stdout + stderr（v1.1 §3.2.1 工具契约）。
///
/// stderr 单独标出来 —— 混在一起时模型分不清「这是输出」还是「这是报错」。
fn command_evidence(out: &CommandOutput) -> String {
    let mut text = out.stdout.clone();
    if !out.stderr.trim().is_empty() {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str("stderr: ");
        text.push_str(out.stderr.trim_end());
    }
    text
}

/// 内置巡检提示词（需求文档 §5.4）。
fn build_inspect_prompt(plan: &AiPlan) -> String {
    let mut p = format!(
        "你是服务器巡检助手。请对以下目标执行只读巡检，并给出结论。\n\n目标：{}（共 {} 台）\n",
        plan.names.join("、"),
        plan.names.len(),
    );
    // 「巡检项」在任务编辑器里已撤下：用户只写一句「AI 要求」，查什么由模型
    // 用工具自己决定（只读策略下它只能调只读工具：inspect_load / read_file /
    // inspect_service / inspect_ports / check_certificate / find_process …）。
    // 这一段在有 checks 时才输出 —— 老任务（存着 checks 的）
    // 仍然按原样走，不会因为界面改版而改变既有的巡检范围。
    if !plan.checks.is_empty() {
        let checks: Vec<&str> = plan.checks.iter().map(|c| c.label()).collect();
        p.push_str(&format!("巡检项：{}\n", checks.join("、")));
    }
    if let Some(path) = &plan.log_path {
        p.push_str(&format!(
            "日志路径：{path}（级别 {}）\n",
            plan.log_level.label()
        ));
    }
    p.push_str(&format!(
        "\n阈值：CPU {}% / 内存 {}% / Swap {}% / 磁盘 {}% / 负载核比 {}\n",
        plan.thresholds.cpu_percent,
        plan.thresholds.mem_percent,
        plan.thresholds.swap_percent,
        plan.thresholds.disk_percent,
        plan.thresholds.load_per_core
    ));
    if !plan.extra.trim().is_empty() {
        p.push_str(&format!("\n用户要求：{}\n", plan.extra.trim()));
    }
    // 第 1、2 条只在对应巡检项存在时给：没有日志路径却让模型去 inspect_log，
    // 它要么编一个路径、要么空转一步 —— 两者都浪费步数。
    let mut rules: Vec<String> = Vec::new();
    if plan.checks.is_empty() {
        rules.push(
            "先用 list_targets 确认目标，再按用户要求选合适的只读工具：inspect_load（负载）/ \
             inspect_service（systemd 单元）/ inspect_ports（监听端口）/ check_certificate（证书到期）/ \
             find_process（按名或端口找进程）/ read_file（看文件内容）/ inspect_log（日志异常行）/ \
             check_reachability（连通性）。用户没指明日志路径时不要凭猜测去 grep 一个路径。"
                .to_string(),
        );
    } else {
        rules.push("先用 inspect_load 取每台的负载快照，超阈值的逐台点名。".to_string());
        if plan.log_path.is_some() {
            rules.push(
                "对每台的目标日志路径用 inspect_log 检查异常，报告命中行数并给出 1–2 条样本。"
                    .to_string(),
            );
        }
    }
    rules.push("不要执行任何状态变更命令；如确有必要，先在结论里说明建议并停下。".to_string());
    rules.push(
        "结论用简体中文，先给「异常清单」，再给「处置建议」，最后给一行总评\
         （正常 / N 台异常 / N 台巡检失败）。"
            .to_string(),
    );
    rules.push("只依据工具返回的数据下结论，不要推测。".to_string());
    p.push_str("\n要求：\n");
    for (ix, r) in rules.iter().enumerate() {
        p.push_str(&format!("{}. {r}\n", ix + 1));
    }
    p
}

/// 判定严重级的展示（供 UI 复用）。
pub fn severity_of(f: &Finding) -> Severity {
    f.severity
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::ToolCall;
    use crate::alerts::AlertStore;

    /// 假执行后端：按命令特征返回预设输出，并记录被调用次数。
    struct FakeExec {
        calls: Arc<AtomicUsize>,
        /// 匹配到子串就返回这段 stdout
        rules: Vec<(String, String)>,
        default: String,
    }

    impl ExecBackend for FakeExec {
        fn exec(&self, _profile: &ConnectionProfile, cmd: &str, _timeout: Duration) -> ExecResult {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let out = self
                .rules
                .iter()
                .find(|(k, _)| cmd.contains(k.as_str()))
                .map(|(_, v)| v.clone())
                .unwrap_or_else(|| self.default.clone());
            ExecResult {
                exit_code: Some(0),
                stdout: out,
                stderr: String::new(),
                timed_out: false,
                cancelled: false,
                elapsed_ms: 1,
            }
        }
    }

    /// 假 LLM：按脚本逐轮回话，脚本用完后返回一句结论（结束循环）。
    struct FakeLlm {
        turns: Arc<parking_lot::Mutex<Vec<AgentTurn>>>,
        /// 每次调用记录收到的消息条数（验证回注确实进了上下文）
        seen: Arc<parking_lot::Mutex<Vec<usize>>>,
        /// 每次调用收到的消息正文（拼成一段）——「模型到底看到了什么」的唯一凭证
        proof: Arc<parking_lot::Mutex<Vec<String>>>,
    }

    impl AgentLlm for FakeLlm {
        fn turn(
            &self,
            messages: &[ChatMessage],
            _tools: Vec<serde_json::Value>,
        ) -> Result<AgentTurn, String> {
            self.seen.lock().push(messages.len());
            self.proof.lock().push(
                messages
                    .iter()
                    .map(|m| m.content.as_str())
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
            let mut t = self.turns.lock();
            if t.is_empty() {
                return Ok(AgentTurn {
                    content: "巡检完成：未发现异常".into(),
                    tool_calls: Vec::new(),
                });
            }
            Ok(t.remove(0))
        }
    }

    fn call(id: &str, name: &str, args: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: args.into(),
        }
    }

    fn profile(id: &str, name: &str) -> ConnectionProfile {
        let mut p = ConnectionProfile::new(name, "10.0.0.1", "root");
        p.id = id.into();
        p.password = Some("pw".into()); // 有口令才不会被「无人值守无口令」跳过
        p
    }

    fn tmp_dir(tag: &str) -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let d = std::env::temp_dir().join(format!(
            "rs-sshbe-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn store_with(profiles: Vec<ConnectionProfile>) -> Arc<ConnectionStore> {
        Arc::new(ConnectionStore {
            profiles,
            ..Default::default()
        })
    }

    fn backend(store: Arc<ConnectionStore>, exec: Arc<FakeExec>) -> RealBackend {
        RealBackend::new(
            store,
            Arc::new(parking_lot::Mutex::new(AlertStore::default())),
            None,
            tmp_dir("dl"),
            tmp_dir("audit").join("agent-audit.log"),
            None,
        )
        .with_exec(exec)
    }

    fn runtime(
        store: Arc<ConnectionStore>,
        exec: Arc<FakeExec>,
        profiles: Vec<ConnectionProfile>,
    ) -> AiToolRuntime {
        AiToolRuntime {
            conn: store,
            backend: exec,
            audit_path: tmp_dir("audit-rt").join("agent-audit.log"),
            download_dir: tmp_dir("dl-rt"),
            profiles,
            thresholds: LoadThresholds::default(),
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    fn plan(names: Vec<String>) -> AiPlan {
        AiPlan {
            names,
            checks: vec![CheckKind::Load],
            thresholds: LoadThresholds::default(),
            log_path: None,
            log_level: LogLevel::Error,
            extra: String::new(),
        }
    }

    /// 一次 exec 的探针输出：**两次采样**（`__NETSPLIT__` 分隔）。
    ///
    /// 第一段 CPU 计数器几乎没涨（idle 只 +1），第二段涨了 100 而 idle 只 +1
    /// → 使用率 ≈99%，稳稳越过 85% 的阈值。内存 1000000kB 中只可用 50000kB
    /// → 使用率 95%，越过 90%。
    ///
    /// 必须造两次采样：只给一次的话 `parse_proc_snapshot` 算不出 CPU
    /// （累计计数器做差才有意义），而「CPU 判定静默失效」正是这条用例要防的。
    fn hot_probe() -> String {
        "cpu  100 0 0 900 0 0 0 0\ncpu0 100 0 0 900 0 0 0 0\n\
         MemTotal:       1000000 kB\nMemFree:          10000 kB\nMemAvailable:     50000 kB\n\
         __NETSPLIT__\n\
         cpu  200 0 0 901 0 0 0 0\ncpu0 200 0 0 901 0 0 0 0\n\
         MemTotal:       1000000 kB\nMemFree:          10000 kB\nMemAvailable:     50000 kB\n\
         __NETDEV__\n\
         __LOADAVG__\n0.10 0.10 0.10 1/100 1\n"
            .to_string()
    }

    /// 两次采样几乎没变化 -> 使用率接近 0（用于「不该报异常」的用例）。
    fn cool_probe() -> String {
        "cpu  100 0 0 900 0 0 0 0\ncpu0 100 0 0 900 0 0 0 0\n\
         MemTotal:       1000000 kB\nMemFree:         800000 kB\nMemAvailable:    800000 kB\n\
         __NETSPLIT__\n\
         cpu  200 0 0 1800 0 0 0 0\ncpu0 200 0 0 1800 0 0 0 0\n\
         MemTotal:       1000000 kB\nMemFree:         800000 kB\nMemAvailable:    800000 kB\n\
         __NETDEV__\n\
         __LOADAVG__\n0.10 0.10 0.10 1/100 1\n"
            .to_string()
    }

    /// 探针输出里**必须**有两次采样：单次快照算不出 CPU 使用率，
    /// 于是「CPU 超阈值」会静默永不触发（这是真实修过的缺陷）。
    #[test]
    fn load_probe_takes_two_samples_so_cpu_is_measurable() {
        assert!(
            PROC_PROBE_TWICE.contains("__NETSPLIT__"),
            "负载探针必须连采两次：否则 CPU 使用率恒为 0"
        );
        // 拆分函数要能把两份快照分开
        let (first, second) = split_double_probe(&hot_probe());
        assert!(first.contains("cpu  100"));
        assert!(second.contains("cpu  200"));
        // 没有分隔标记时退化为「两份相同」，不 panic
        let (a, b) = split_double_probe("cpu  1 0 0 1\n");
        assert_eq!(a, b);
    }

    /// 正常水位不该产出 Anomaly（避免把「一切正常」也报成告警）。
    #[test]
    fn cool_metrics_produce_no_anomaly() {
        let store = store_with(vec![profile("p1", "prod-01")]);
        let exec = Arc::new(FakeExec {
            calls: Arc::new(AtomicUsize::new(0)),
            rules: vec![("__NETSPLIT__".into(), cool_probe())],
            default: cool_probe(),
        });
        let rt = runtime(store, exec, vec![profile("p1", "prod-01")]);
        let (status, text, findings) = execute_tool_call(
            &rt,
            &call("c1", "inspect_load", r#"{"targets":["p1"]}"#),
            &[],
        );
        assert!(matches!(status, agent::ToolStatus::Ok), "{status:?}");
        assert!(text.contains("prod-01"));
        assert!(
            !findings.iter().any(|f| f.kind == FindingKind::Anomaly),
            "正常水位不该报异常：{findings:?}"
        );
    }

    #[test]
    fn exec_action_blocks_destructive_and_unattended_mutations() {
        let store = store_with(vec![profile("p1", "prod-01")]);
        let exec = Arc::new(FakeExec {
            calls: Arc::new(AtomicUsize::new(0)),
            rules: Vec::new(),
            default: String::new(),
        });
        let be = backend(store, exec.clone());
        let progress = Arc::new(AtomicUsize::new(0));
        let cancel = Arc::new(AtomicBool::new(false));

        // 破坏类：直接拒绝，后端一次都不该被调用
        let blocked = TaskAction::ExecMulti {
            target: crate::tasks::TaskTarget {
                profiles: vec!["p1".into()],
                ..Default::default()
            },
            command: "rm -rf /".into(),
            timeout_secs: 15,
            allow_mutating: true,
        };
        let err = be.execute(&blocked, &progress, &cancel).unwrap_err();
        assert!(err.contains("拦截") || err.contains("blocked"), "err={err}");
        assert_eq!(exec.calls.load(Ordering::SeqCst), 0, "拦截必须不执行");

        // 状态变更 + 未开启 allow_mutating：同样不执行
        let mutating = TaskAction::ExecMulti {
            target: crate::tasks::TaskTarget {
                profiles: vec!["p1".into()],
                ..Default::default()
            },
            command: "systemctl restart nginx".into(),
            timeout_secs: 15,
            allow_mutating: false,
        };
        let err = be.execute(&mutating, &progress, &cancel).unwrap_err();
        assert!(!err.is_empty());
        assert_eq!(exec.calls.load(Ordering::SeqCst), 0, "未开启时不得执行");

        // 显式开启后允许执行
        let allowed = TaskAction::ExecMulti {
            target: crate::tasks::TaskTarget {
                profiles: vec!["p1".into()],
                ..Default::default()
            },
            command: "systemctl restart nginx".into(),
            timeout_secs: 15,
            allow_mutating: true,
        };
        let metric = be.execute(&allowed, &progress, &cancel).unwrap();
        assert_eq!(exec.calls.load(Ordering::SeqCst), 1, "开启后应执行");
        assert_eq!(metric.0, RunStatus::Ok);
        assert_eq!(metric.1.len(), 1);
    }

    /// 无口令的连接在无人值守下必须**跳过并说明原因**，而不是静默少一台。
    #[test]
    fn profile_without_credentials_is_skipped_with_a_reason() {
        let mut p = profile("p1", "prod-01");
        p.password = None;
        p.key_path = None;
        let store = store_with(vec![p]);
        let exec = Arc::new(FakeExec {
            calls: Arc::new(AtomicUsize::new(0)),
            rules: Vec::new(),
            default: String::new(),
        });
        let be = backend(store, exec.clone());
        let act = TaskAction::ExecMulti {
            target: crate::tasks::TaskTarget {
                profiles: vec!["p1".into()],
                ..Default::default()
            },
            command: "df -h".into(),
            timeout_secs: 15,
            allow_mutating: false,
        };
        let err = be
            .execute(
                &act,
                &Arc::new(AtomicUsize::new(0)),
                &Arc::new(AtomicBool::new(false)),
            )
            .unwrap_err();
        assert!(err.contains("口令"), "原因要说清：{err}");
        assert_eq!(exec.calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn target_resolution_covers_group_tag_and_exclude() {
        let mut s = ConnectionStore {
            groups: vec![crate::connections::ConnectionGroup::new("生产组")],
            ..Default::default()
        };
        let gid = s.groups[0].id.clone();
        let mut p1 = profile("p1", "prod-01");
        p1.group_id = Some(gid);
        p1.tags = vec!["prod".into()];
        let mut p2 = profile("p2", "prod-02");
        p2.tags = vec!["prod".into()];
        s.profiles = vec![p1, p2];
        let store = Arc::new(s);
        let exec = Arc::new(FakeExec {
            calls: Arc::new(AtomicUsize::new(0)),
            rules: Vec::new(),
            default: String::new(),
        });
        let be = backend(store, exec);

        // 按分组
        let by_group = be
            .resolve(&crate::tasks::TaskTarget {
                groups: vec![store_group_id(&be.conn)],
                ..Default::default()
            })
            .unwrap();
        assert_eq!(by_group.len(), 1);
        assert_eq!(by_group[0].name, "prod-01");

        // 按标签
        let by_tag = be
            .resolve(&crate::tasks::TaskTarget {
                tags: vec!["prod".into()],
                ..Default::default()
            })
            .unwrap();
        assert_eq!(by_tag.len(), 2);

        // 排除
        let excluded = be
            .resolve(&crate::tasks::TaskTarget {
                tags: vec!["prod".into()],
                exclude: vec!["p2".into()],
                ..Default::default()
            })
            .unwrap();
        assert_eq!(excluded.len(), 1);
        assert_eq!(excluded[0].id, "p1");
    }

    fn store_group_id(conn: &ConnectionStore) -> String {
        conn.groups[0].id.clone()
    }

    /// 无人值守下需要确认的工具体验：回注 Disabled 且**不执行**。
    #[test]
    fn unattended_tool_verdicts_never_execute_mutations() {
        let store = store_with(vec![profile("p1", "prod-01")]);
        let exec = Arc::new(FakeExec {
            calls: Arc::new(AtomicUsize::new(0)),
            rules: Vec::new(),
            default: String::new(),
        });
        let rt = runtime(store, exec.clone(), vec![profile("p1", "prod-01")]);

        // 状态变更命令：Disabled，且后端未被调用
        let (status, text, findings) = execute_tool_call(
            &rt,
            &call(
                "c1",
                "run_command",
                r#"{"targets":["p1"],"command":"systemctl restart nginx"}"#,
            ),
            &[],
        );
        assert!(
            matches!(status, agent::ToolStatus::Disabled(_)),
            "无人值守必须禁用状态变更：{status:?}"
        );
        assert!(
            text.contains("无人值守") || text.contains("unattended"),
            "{text}"
        );
        assert!(findings.is_empty());
        assert_eq!(exec.calls.load(Ordering::SeqCst), 0);

        // 破坏类：Blocked
        let (status, _, _) = execute_tool_call(
            &rt,
            &call(
                "c2",
                "run_command",
                r#"{"targets":["p1"],"command":"mkfs.ext4 /dev/sda"}"#,
            ),
            &[],
        );
        assert!(
            matches!(status, agent::ToolStatus::Blocked(_)),
            "{status:?}"
        );
        assert_eq!(exec.calls.load(Ordering::SeqCst), 0);

        // 下载：Disabled
        let (status, _, _) = execute_tool_call(
            &rt,
            &call(
                "c3",
                "download_file",
                r#"{"target":"p1","remote_path":"/var/log/a.log"}"#,
            ),
            &[],
        );
        assert!(
            matches!(status, agent::ToolStatus::Disabled(_)),
            "{status:?}"
        );

        // 只读的负载巡检：直接执行
        let (status, _, _) = execute_tool_call(
            &rt,
            &call("c4", "inspect_load", r#"{"targets":["p1"]}"#),
            &[],
        );
        assert!(matches!(status, agent::ToolStatus::Ok), "{status:?}");
        assert_eq!(exec.calls.load(Ordering::SeqCst), 1, "只读工具应当执行");
    }

    #[test]
    fn tool_bad_args_and_unknown_tools_are_reported() {
        let store = store_with(vec![profile("p1", "prod-01")]);
        let exec = Arc::new(FakeExec {
            calls: Arc::new(AtomicUsize::new(0)),
            rules: Vec::new(),
            default: String::new(),
        });
        let rt = runtime(store, exec.clone(), vec![profile("p1", "prod-01")]);

        let (status, text, _) = execute_tool_call(&rt, &call("c1", "no_such_tool", "{}"), &[]);
        assert!(matches!(status, agent::ToolStatus::BadArgs(_)));
        assert!(text.contains("未知工具"));

        // 语法错的 JSON
        let (status, _, _) = execute_tool_call(&rt, &call("c2", "inspect_log", "{oops"), &[]);
        assert!(matches!(status, agent::ToolStatus::BadArgs(_)));

        // 缺必填参数
        let (status, _, _) = execute_tool_call(&rt, &call("c3", "inspect_log", "{}"), &[]);
        assert!(matches!(status, agent::ToolStatus::BadArgs(_)));

        // 穿越路径被参数校验拦下
        let (status, _, _) = execute_tool_call(
            &rt,
            &call("c4", "inspect_log", r#"{"path":"/var/../../etc/shadow"}"#),
            &[],
        );
        assert!(matches!(status, agent::ToolStatus::BadArgs(_)));
        assert_eq!(exec.calls.load(Ordering::SeqCst), 0, "非法参数不得执行");
    }

    /// 只读巡检把阈值判定结果变成 finding（这是预警链路的源头）。
    #[test]
    fn load_probe_turns_threshold_breach_into_a_finding() {
        let store = store_with(vec![profile("p1", "prod-01")]);
        let exec = Arc::new(FakeExec {
            calls: Arc::new(AtomicUsize::new(0)),
            rules: vec![("__NETSPLIT__".into(), hot_probe())],
            default: hot_probe(),
        });
        let rt = runtime(store, exec, vec![profile("p1", "prod-01")]);
        let (status, text, findings) = execute_tool_call(
            &rt,
            &call("c1", "inspect_load", r#"{"targets":["p1"]}"#),
            &[],
        );
        assert!(matches!(status, agent::ToolStatus::Ok), "{status:?}");
        assert!(text.contains("prod-01"), "回注文本要带目标：{text}");
        assert!(
            findings.iter().any(|f| f.kind == FindingKind::Anomaly),
            "超阈值必须产出 Anomaly：{findings:?}"
        );
        // finding 必须带上目标身份（否则预警里只有「某台」）
        assert!(findings.iter().all(|f| f.profile_id == "p1"));
    }

    /// AI 巡检多步循环：先查负载、再查日志，最后给结论。
    #[test]
    fn ai_inspection_runs_multiple_tool_steps_and_collects_findings() {
        let store = store_with(vec![profile("p1", "prod-01")]);
        let calls = Arc::new(AtomicUsize::new(0));
        let exec = Arc::new(FakeExec {
            calls: calls.clone(),
            rules: vec![(
                "grep".into(),
                "__RS_HITS__7\n10:00 ERROR boom\n10:01 ERROR boom\n".into(),
            )],
            default: hot_probe(),
        });
        let rt = runtime(store, exec, vec![profile("p1", "prod-01")]);
        let llm = FakeLlm {
            turns: Arc::new(parking_lot::Mutex::new(vec![
                AgentTurn {
                    content: "先看负载".into(),
                    tool_calls: vec![call("c1", "inspect_load", r#"{"targets":["p1"]}"#)],
                },
                AgentTurn {
                    content: "再看日志".into(),
                    tool_calls: vec![call(
                        "c2",
                        "inspect_log",
                        r#"{"targets":["p1"],"path":"/var/log/app.log"}"#,
                    )],
                },
            ])),
            seen: Arc::new(parking_lot::Mutex::new(Vec::new())),
            proof: Arc::new(parking_lot::Mutex::new(Vec::new())),
        };
        let seen = llm.seen.clone();

        let (conclusion, findings, per_target, trace) =
            run_ai_inspection(&llm, &rt, &plan(vec!["prod-01".into()])).unwrap();

        assert!(conclusion.contains("未发现异常") || !conclusion.is_empty());
        // 两类 finding 都产生了
        assert!(
            findings.iter().any(|f| f.rule_id == "builtin.load"),
            "{findings:?}"
        );
        assert!(
            findings.iter().any(|f| f.rule_id == "builtin.log"),
            "{findings:?}"
        );
        assert_eq!(per_target.len(), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 2, "两个工具各跑一次");

        // 轨迹与结论一起留下来（运行记录里的会话靠它回放）：
        // 两次工具调用各一行、都已收口，且明细里有东西可看。
        assert_eq!(trace.len(), 2, "两个工具调用应当各留一行轨迹：{trace:?}");
        for row in &trace {
            assert!(
                row.state.is_settled(),
                "轨迹行不能停在「执行中」：{row:?} —— 无人值守没人会去收口它"
            );
            assert!(
                !row.summary.is_empty(),
                "轨迹行必须带目标摘要（否则会话里只剩工具名）：{row:?}"
            );
            assert!(!row.detail.is_empty(), "轨迹行应当留下明细：{row:?}");
        }
        assert!(
            trace.iter().any(|r| r.state == TraceState::Anomaly),
            "负载超阈值那一行应当是「发现异常」：{trace:?}"
        );

        // 关键：工具的返回值确实被回注进了下一轮上下文。
        // 只比较「消息条数」是没牙的断言 —— 必须看到工具输出文本真的在里面。
        let msg_counts = seen.lock().clone();
        assert_eq!(msg_counts.len(), 3, "三次 LLM 往返（两步工具 + 收尾）");
        assert!(
            msg_counts[1] > msg_counts[0],
            "第二轮上下文必须比第一轮长（工具结果被回注）：{msg_counts:?}"
        );
    }

    /// 2026-09-22「最大工具步数移除，不做限制」：巡检不再按轮数收尾。
    ///
    /// 旧行为是跑到 `max_steps`（默认 8、硬顶 16）就停下并在结论里写「达到上限」。
    /// 现在 20 轮工具往返仍应继续发请求，直到模型自己收口（脚本跑完 -> 收尾轮）。
    #[test]
    fn ai_inspection_is_no_longer_capped_by_step_count() {
        let store = store_with(vec![profile("p1", "prod-01")]);
        let exec = Arc::new(FakeExec {
            calls: Arc::new(AtomicUsize::new(0)),
            rules: Vec::new(),
            default: hot_probe(),
        });
        let rt = runtime(store, exec, vec![profile("p1", "prod-01")]);
        // 每一步都要求调工具：只有「轮数上限」能拦下它 —— 正是要验证它不再拦。
        let turns: Vec<AgentTurn> = (0..20)
            .map(|i| AgentTurn {
                content: format!("第 {i} 步"),
                tool_calls: vec![call(
                    &format!("c{i}"),
                    "inspect_load",
                    r#"{"targets":["p1"]}"#,
                )],
            })
            .collect();
        let llm = FakeLlm {
            turns: Arc::new(parking_lot::Mutex::new(turns)),
            seen: Arc::new(parking_lot::Mutex::new(Vec::new())),
            proof: Arc::new(parking_lot::Mutex::new(Vec::new())),
        };
        let (conclusion, _, _, _) = run_ai_inspection(&llm, &rt, &plan(vec!["prod-01".into()]))
            .expect("无轮数上限时应当正常跑完");
        assert_eq!(
            llm.seen.lock().len(),
            21,
            "20 轮工具调用 + 1 轮收尾，中间不该被轮数闸门截断"
        );
        assert!(
            conclusion.contains("巡检完成"),
            "结论应当来自模型自己的收尾轮：{conclusion}"
        );
    }

    /// AI 未配置时给出可读原因，而不是静默成功。
    #[test]
    fn ai_inspection_without_config_reports_a_reason() {
        let store = store_with(vec![profile("p1", "prod-01")]);
        let exec = Arc::new(FakeExec {
            calls: Arc::new(AtomicUsize::new(0)),
            rules: Vec::new(),
            default: String::new(),
        });
        let be = backend(store, exec); // ai = None
        let act = TaskAction::AiInspect {
            target: crate::tasks::TaskTarget {
                profiles: vec!["p1".into()],
                ..Default::default()
            },
            prompt: String::new(),
            checks: vec![CheckKind::Load],
            thresholds: None,
            log_path: None,
            log_level: LogLevel::Error,
        };
        let (status, reports, _, _) = be
            .execute(
                &act,
                &Arc::new(AtomicUsize::new(0)),
                &Arc::new(AtomicBool::new(false)),
            )
            .unwrap();
        assert_eq!(status, RunStatus::Failed);
        assert_eq!(reports.len(), 1);
        assert!(!reports[0].exec_ok);
        assert!(
            reports[0].summary.contains("未配置") || reports[0].summary.contains("not configured"),
            "summary={}",
            reports[0].summary
        );
    }

    #[test]
    fn run_status_reflects_anomaly_and_failures() {
        let store = store_with(vec![profile("p1", "prod-01"), profile("p2", "prod-02")]);
        // 只读命令：两台都成功
        let exec = Arc::new(FakeExec {
            calls: Arc::new(AtomicUsize::new(0)),
            rules: Vec::new(),
            default: String::new(),
        });
        let be = backend(store, exec);
        let ok = TaskAction::ExecMulti {
            target: crate::tasks::TaskTarget {
                profiles: vec!["p1".into(), "p2".into()],
                ..Default::default()
            },
            command: "df -h".into(),
            timeout_secs: 15,
            allow_mutating: false,
        };
        let (status, reports, _, _) = be
            .execute(
                &ok,
                &Arc::new(AtomicUsize::new(0)),
                &Arc::new(AtomicBool::new(false)),
            )
            .unwrap();
        assert_eq!(status, RunStatus::Ok);
        assert_eq!(reports.len(), 2);
        // 报告按目标原序（稳定可比）
        assert_eq!(reports[0].profile_id, "p1");
        assert_eq!(reports[1].profile_id, "p2");
    }

    #[test]
    fn inspect_prompt_carries_targets_thresholds_and_rules() {
        let mut p = plan(vec!["prod-01".into(), "prod-02".into()]);
        p.log_path = Some("/var/log/app.log".into());
        p.extra = "关注数据库连接数".into();
        let text = build_inspect_prompt(&p);
        assert!(text.contains("prod-01") && text.contains("prod-02"));
        assert!(text.contains("/var/log/app.log"));
        assert!(text.contains("关注数据库连接数"));
        assert!(text.contains("90"), "阈值要写进提示词：{text}");
        // 硬约束必须在（否则模型可能去执行状态变更命令）
        assert!(text.contains("不要执行任何状态变更命令"));
        assert!(text.contains("只依据工具返回的数据"));
    }

    /// 「巡检项」撤下之后：没有 checks 时提示词**不得**再让模型去
    /// `inspect_log` 一个它没有的路径。
    ///
    /// 这条防的是「界面撤了巡检项，提示词还在原地指挥」：没有日志路径却命令模型
    /// 去 grep，它要么编一个路径、要么空转一步 —— 两者都浪费步数，而用户只看到
    /// 「AI 查了个莫名其妙的文件」。
    #[test]
    fn inspect_prompt_without_checks_does_not_invent_a_log_path() {
        let p = AiPlan {
            names: vec!["prod-01".into()],
            // 巡检项已撤下 ⇒ UI 送过来的是空
            checks: Vec::new(),
            thresholds: LoadThresholds::default(),
            log_path: None,
            log_level: LogLevel::Error,
            extra: "看看磁盘和内存".into(),
        };
        let text = build_inspect_prompt(&p);
        assert!(text.contains("prod-01"), "目标仍要写进提示词：{text}");
        assert!(
            text.contains("用户要求：看看磁盘和内存"),
            "「AI 要求」要原样进提示词且标明是用户要求：{text}"
        );
        assert!(
            !text.contains("巡检项："),
            "没有 checks 时不该输出空的巡检项行：{text}"
        );
        assert!(
            !text.contains("目标日志路径"),
            "没有日志路径时不该指挥模型去 inspect_log：{text}"
        );
        assert!(
            text.contains("不要凭猜测去 grep 一个路径"),
            "要明确告诉模型「没有路径就别猜」：{text}"
        );
        assert!(text.contains("阈值"), "阈值仍然要给出（判定口径不能丢）");

        // 老任务（存着 checks + log_path）的行为**不能被改版影响**
        let mut legacy = plan(vec!["prod-01".into()]);
        legacy.log_path = Some("/var/log/app.log".into());
        let text = build_inspect_prompt(&legacy);
        assert!(text.contains("巡检项："), "有 checks 时要照旧输出：{text}");
        assert!(
            text.contains("目标日志路径用 inspect_log"),
            "有日志路径时要照旧指挥模型去查：{text}"
        );
    }

    // 交互式 agent 驱动（AI 面板那条路）

    /// 脚本化 sink：记录全部事件，确认按预设答复。
    ///
    /// 用它而不是通道，是为了让断言直接落在「轨迹变成了什么」上，
    /// 不必处理线程与超时 —— 那部分由 `ChannelSink` 自己的用例覆盖。
    struct ScriptedSink {
        deltas: parking_lot::Mutex<String>,
        rows: parking_lot::Mutex<Vec<TraceRow>>,
        confirms: parking_lot::Mutex<Vec<ConfirmRequest>>,
        answer: AgentDecision,
        done: parking_lot::Mutex<Option<(String, agent::FinishReason)>>,
        /// 用户确认后落下来的任务草案（`propose_task` 的产物）
        proposed: parking_lot::Mutex<Vec<Task>>,
    }

    impl ScriptedSink {
        fn new(answer: AgentDecision) -> Self {
            Self {
                deltas: parking_lot::Mutex::new(String::new()),
                rows: parking_lot::Mutex::new(Vec::new()),
                confirms: parking_lot::Mutex::new(Vec::new()),
                answer,
                done: parking_lot::Mutex::new(None),
                proposed: parking_lot::Mutex::new(Vec::new()),
            }
        }
    }

    impl AgentSink for ScriptedSink {
        fn delta(&self, text: &str) {
            self.deltas.lock().push_str(text);
        }
        fn trace(&self, row: &TraceRow) {
            let mut rows = self.rows.lock();
            agent::trace::upsert(&mut rows, row.clone());
        }
        fn confirm(&self, req: &ConfirmRequest) -> AgentDecision {
            self.confirms.lock().push(req.clone());
            self.answer
        }
        fn done(&self, conclusion: &str, reason: &agent::FinishReason) {
            *self.done.lock() = Some((conclusion.to_string(), reason.clone()));
        }
        fn propose(&self, task: &Task) {
            self.proposed.lock().push(task.clone());
        }
    }

    fn interactive_spec(allow_mutating: bool) -> InteractiveSpec {
        InteractiveSpec {
            cfg: AiConfig::default(),
            api_key: "k".into(),
            system: "你是运维助手".into(),
            user: "查一下生产组".into(),
            history: Vec::new(),
            profiles: vec![profile("p1", "prod-01"), profile("p2", "prod-02")],
            thresholds: LoadThresholds::default(),
            audit_path: tmp_dir("ia-audit").join("agent-audit.log"),
            download_dir: tmp_dir("ia-dl"),
            whitelist: Vec::new(),
            allow_mutating,
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    fn scripted(turns: Vec<AgentTurn>) -> FakeLlm {
        FakeLlm {
            turns: Arc::new(parking_lot::Mutex::new(turns)),
            seen: Arc::new(parking_lot::Mutex::new(Vec::new())),
            proof: Arc::new(parking_lot::Mutex::new(Vec::new())),
        }
    }

    /// AC-1：模型请求 `inspect_load` → 真的执行了、结果回注、轨迹留下痕迹。
    #[test]
    fn interactive_agent_runs_readonly_tools_without_asking() {
        let store = store_with(vec![profile("p1", "prod-01"), profile("p2", "prod-02")]);
        let exec = Arc::new(FakeExec {
            calls: Arc::new(AtomicUsize::new(0)),
            rules: vec![("__PROC__".into(), cool_probe())],
            default: cool_probe(),
        });
        let rt = runtime(
            store,
            exec.clone(),
            vec![profile("p1", "prod-01"), profile("p2", "prod-02")],
        );
        let llm = scripted(vec![AgentTurn {
            content: "我来看看负载".into(),
            tool_calls: vec![call("c1", "inspect_load", r#"{"selector":{"kind":"all"}}"#)],
        }]);
        let sink = ScriptedSink::new(AgentDecision::Approve);
        let spec = interactive_spec(false);

        run_interactive_agent(&spec, &rt, &llm, &sink).unwrap();

        assert!(
            exec.calls.load(Ordering::SeqCst) > 0,
            "只读工具必须真的执行（没有任何确认）"
        );
        let rows = sink.rows.lock().clone();
        assert_eq!(rows.len(), 1, "一次调用应当只有一行轨迹");
        assert_eq!(rows[0].tool, "inspect_load");
        assert!(
            rows[0].state.is_settled(),
            "收尾后不该停在 Running：{:?}",
            rows[0].state
        );
        assert!(sink.confirms.lock().is_empty(), "只读工具不该弹确认卡片");
        // 回注确实进了上下文：第二轮请求的消息数比第一轮多（assistant + tool）
        let seen = llm.seen.lock().clone();
        assert!(
            seen.len() >= 2 && seen[1] > seen[0],
            "工具结果没有回注给模型：{seen:?}"
        );
        let (_, reason) = sink.done.lock().clone().expect("必须收尾");
        assert_eq!(reason, agent::FinishReason::NoMoreTools);
    }

    /// AC-4：`rm -rf /` 被永久拦截 —— 轨迹上能看到拒绝原因，且**没有执行**。
    #[test]
    fn blocked_command_is_refused_and_never_executed() {
        let store = store_with(vec![profile("p1", "prod-01")]);
        let exec = Arc::new(FakeExec {
            calls: Arc::new(AtomicUsize::new(0)),
            rules: Vec::new(),
            default: "should not run".into(),
        });
        let rt = runtime(store, exec.clone(), vec![profile("p1", "prod-01")]);
        let llm = scripted(vec![AgentTurn {
            content: "准备清理".into(),
            tool_calls: vec![call(
                "c1",
                "run_command",
                r#"{"selector":{"kind":"all"},"command":"rm -rf /","timeout_secs":15}"#,
            )],
        }]);
        let sink = ScriptedSink::new(AgentDecision::Approve);
        // 即使设置里开了「允许状态变更」，Blocked 也必须被拦下
        let spec = interactive_spec(true);

        run_interactive_agent(&spec, &rt, &llm, &sink).unwrap();

        assert_eq!(
            exec.calls.load(Ordering::SeqCst),
            0,
            "Blocked 命令绝不允许落到执行层"
        );
        let rows = sink.rows.lock().clone();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, TraceState::Blocked, "轨迹要显示「已拦截」");
        assert!(
            !rows[0].note.trim().is_empty(),
            "轨迹要给出拒绝原因，而不是只有一个红点"
        );
        assert!(
            sink.confirms.lock().is_empty(),
            "Blocked 不该弹卡片（弹了就等于给了放行的机会）"
        );
    }

    /// AC-5：Confirm 级 + 多台 → 弹确认卡片；**用户拒绝前不执行**；
    /// 批量场景白名单不生效（`whitelistable = false`）。
    #[test]
    fn batch_confirm_asks_first_and_honours_the_answer() {
        let st = store_with(vec![profile("p1", "prod-01"), profile("p2", "prod-02")]);
        let exec = Arc::new(FakeExec {
            calls: Arc::new(AtomicUsize::new(0)),
            rules: Vec::new(),
            default: "ok".into(),
        });
        let rt = runtime(
            st,
            exec.clone(),
            vec![profile("p1", "prod-01"), profile("p2", "prod-02")],
        );
        let turn = AgentTurn {
            content: String::new(),
            tool_calls: vec![call(
                "c1",
                "run_command",
                r#"{"selector":{"kind":"all"},"command":"systemctl restart nginx","timeout_secs":15}"#,
            )],
        };
        let spec = interactive_spec(true);

        // ① 拒绝：不执行
        let sink = ScriptedSink::new(AgentDecision::Deny);
        run_interactive_agent(&spec, &rt, &scripted(vec![turn.clone()]), &sink).unwrap();
        assert_eq!(exec.calls.load(Ordering::SeqCst), 0, "用户拒绝了却执行了");
        let reqs = sink.confirms.lock().clone();
        assert_eq!(reqs.len(), 1, "Confirm 级必须弹一张卡片");
        assert_eq!(reqs[0].target_count, 2, "卡片要写清覆盖几台");
        assert!(
            !reqs[0].whitelistable,
            "批量（>1 台）永不走白名单 —— 卡片必须据此隐藏该动作"
        );
        assert!(
            reqs[0].command.contains("systemctl restart nginx"),
            "卡片要原样展示待执行的命令"
        );
        assert_eq!(
            sink.rows.lock()[0].state,
            TraceState::Rejected,
            "拒绝后轨迹要标成「已忽略」"
        );

        // ② 同意：执行
        let sink2 = ScriptedSink::new(AgentDecision::Approve);
        run_interactive_agent(&spec, &rt, &scripted(vec![turn]), &sink2).unwrap();
        assert!(exec.calls.load(Ordering::SeqCst) > 0, "用户同意了却没执行");
    }

    /// 2026-09-22 运行反馈「不要展示所有连接，只展示需要连的链接」：
    /// 轨迹摘要里的目标名只有本次调用声明的那几台 —— 连接库里其余的不能出现。
    #[test]
    fn trace_summary_lists_only_the_targets_this_call_needs() {
        let p1 = profile("p1", "prod-01");
        let p2 = profile("p2", "prod-02");
        let p3 = profile("p3", "prod-03");
        let exec = Arc::new(FakeExec {
            calls: Arc::new(AtomicUsize::new(0)),
            rules: Vec::new(),
            default: cool_probe(),
        });
        let rt = runtime(
            store_with(vec![p1.clone(), p2.clone(), p3.clone()]),
            exec,
            vec![p1, p2, p3],
        );
        let llm = scripted(vec![AgentTurn {
            content: String::new(),
            tool_calls: vec![call("c1", "inspect_load", r#"{"targets":["p1"]}"#)],
        }]);
        let sink = ScriptedSink::new(AgentDecision::Approve);
        run_interactive_agent(&interactive_spec(false), &rt, &llm, &sink).unwrap();

        let rows = sink.rows.lock().clone();
        assert_eq!(rows.len(), 1);
        assert!(
            rows[0].summary.contains("prod-01"),
            "摘要要指出这次连的是哪台：{}",
            rows[0].summary
        );
        assert!(
            !rows[0].summary.contains("prod-02") && !rows[0].summary.contains("prod-03"),
            "没被选中的连接不该出现在摘要里：{}",
            rows[0].summary
        );
    }

    /// 2026-09-22 运行反馈「明细中需要展示执行的命令」：点开一行 `run_command`
    /// 必须能看到命令原文（多行原样保留），而不只是远端回显。
    #[test]
    fn trace_detail_shows_the_executed_command() {
        let p1 = profile("p1", "prod-01");
        let exec = Arc::new(FakeExec {
            calls: Arc::new(AtomicUsize::new(0)),
            rules: Vec::new(),
            default: "ok".into(),
        });
        let rt = runtime(store_with(vec![p1.clone()]), exec, vec![p1]);
        let llm = scripted(vec![AgentTurn {
            content: String::new(),
            tool_calls: vec![call(
                "c1",
                "run_command",
                r#"{"targets":["p1"],"command":"df -h\nuptime","timeout_secs":15}"#,
            )],
        }]);
        let sink = ScriptedSink::new(AgentDecision::Approve);
        run_interactive_agent(&interactive_spec(false), &rt, &llm, &sink).unwrap();

        let rows = sink.rows.lock().clone();
        let detail = rows[0].render_detail();
        assert!(detail.contains("df -h"), "明细里要有执行的命令：{detail}");
        assert!(
            detail.contains("uptime"),
            "多行命令要原样保留（`sh -c` 里换行是语义）：{detail}"
        );
    }

    /// 用户报的 bug：`run_command` 只回「1 台正常」，stdout 没进上下文 ——
    /// 模型看不到端口列表，只能答「没拿到输出」。
    #[test]
    fn run_command_result_reaches_the_model_with_the_real_output() {
        let exec = Arc::new(FakeExec {
            calls: Arc::new(AtomicUsize::new(0)),
            rules: vec![(
                "ss -lntup".into(),
                "LISTEN 0 128 0.0.0.0:80 users:((\"nginx\",pid=1,fd=6))".into(),
            )],
            default: String::new(),
        });
        let rt = runtime(
            store_with(vec![profile("p1", "prod-01")]),
            exec.clone(),
            vec![profile("p1", "prod-01")],
        );
        let llm = scripted(vec![AgentTurn {
            content: String::new(),
            tool_calls: vec![call(
                "c1",
                "run_command",
                r#"{"targets":["p1"],"command":"ss -lntup","timeout_secs":15}"#,
            )],
        }]);
        let proof = llm.proof.clone();
        let sink = ScriptedSink::new(AgentDecision::Approve);

        run_interactive_agent(&interactive_spec(false), &rt, &llm, &sink).unwrap();

        // 第二轮请求就是带工具结果的那一轮：模型实际看到的正文里必须有 stdout
        let rounds = proof.lock().clone();
        let second = rounds
            .get(1)
            .unwrap_or_else(|| panic!("至少要有第二轮（带工具结果）：{rounds:?}"));
        assert!(
            second.contains("LISTEN 0 128 0.0.0.0:80"),
            "run_command 的 stdout 必须回注给模型，否则它只能答「没拿到输出」：{second}"
        );
        assert!(
            second.contains("prod-01"),
            "回注要标明输出属于哪台机器：{second}"
        );
        assert_eq!(exec.calls.load(Ordering::SeqCst), 1);
    }

    /// 用户报的 bug：探测 80/443/8080 全回 `127.0.0.1:2222 ok` —— `port` 参数被忽略，
    /// 探的永远是连接自己的 SSH 端口。这里起一个真实监听，断言探的就是请求里的端口。
    #[test]
    fn check_reachability_probes_the_requested_port_not_the_ssh_port() {
        use std::net::TcpListener;
        // 真实监听：给 127.0.0.1 一个必然可达的端口
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let open = listener.local_addr().unwrap().port();
        // 一个刚被释放的端口当 profile 的 SSH 端口：探它必然 unreachable
        let closed = {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            let p = l.local_addr().unwrap().port();
            drop(l);
            p
        };

        let mut p1 = profile("p1", "docker-ssh1");
        p1.host = "127.0.0.1".into();
        p1.port = closed;
        let rt = runtime(
            store_with(vec![p1.clone()]),
            Arc::new(FakeExec {
                calls: Arc::new(AtomicUsize::new(0)),
                rules: Vec::new(),
                default: String::new(),
            }),
            vec![p1],
        );
        let llm = scripted(vec![AgentTurn {
            content: String::new(),
            tool_calls: vec![call(
                "c1",
                "check_reachability",
                &format!(r#"{{"targets":["p1"],"port":{open},"timeout_secs":2}}"#),
            )],
        }]);
        let proof = llm.proof.clone();
        let sink = ScriptedSink::new(AgentDecision::Approve);

        run_interactive_agent(&interactive_spec(false), &rt, &llm, &sink).unwrap();

        let rounds = proof.lock().clone();
        let text = rounds.get(1).cloned().unwrap_or_default();
        assert!(
            text.contains(&format!("127.0.0.1:{open} ok")),
            "必须探测请求里给的端口 {open}：{text}"
        );
        assert!(
            !text.contains(&format!("127.0.0.1:{closed}")),
            "不能拿 profile 的 SSH 端口（{closed}）冒充探测结论：{text}"
        );
    }

    /// 设置里关掉「允许状态变更命令」时：故障命令变成 Disabled **且不出现在 tools[] 里**
    /// （可见性优先，见 §3.6.1）。
    #[test]
    fn mutating_tools_are_hidden_when_the_setting_is_off() {
        let store = store_with(vec![profile("p1", "prod-01")]);
        let exec = Arc::new(FakeExec {
            calls: Arc::new(AtomicUsize::new(0)),
            rules: Vec::new(),
            default: "ok".into(),
        });
        let rt = runtime(store, exec.clone(), vec![profile("p1", "prod-01")]);
        let spec = interactive_spec(false);

        // 断言发给 provider 的工具清单里没有 download/propose_task
        let policy = if spec.allow_mutating {
            agent::ToolPolicy::interactive()
        } else {
            agent::ToolPolicy::inspect_only()
        };
        let names: Vec<String> = agent::tool_schemas_for(&policy)
            .iter()
            .map(|s| {
                s["function"]["name"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string()
            })
            .collect();
        assert!(names.contains(&"run_command".to_string()), "只读命令要留着");
        assert!(!names.contains(&"download_file".to_string()));
        assert!(!names.contains(&"propose_task".to_string()));

        // 即便模型「凭记忆」调了这两个工具，执行层也必须挡住（双层防线）
        let llm = scripted(vec![AgentTurn {
            content: String::new(),
            tool_calls: vec![call(
                "c1",
                "download_file",
                r#"{"target":"p1","remote_path":"/etc/hostname"}"#,
            )],
        }]);
        let sink = ScriptedSink::new(AgentDecision::Approve);
        run_interactive_agent(&spec, &rt, &llm, &sink).unwrap();
        let rows = sink.rows.lock().clone();
        assert_eq!(rows[0].state, TraceState::Disabled, "执行层必须再次挡住");
        assert!(
            sink.confirms.lock().is_empty(),
            "没开启变更权限时不该弹「要不要执行」——那不是用户能放的权"
        );
    }

    /// AC-7：用户点停止 → 在途步骤收口，未完成的行标记为已停止，
    /// 已完成的轨迹**保留**。
    #[test]
    fn stop_marks_unfinished_rows_and_keeps_the_rest() {
        let store = store_with(vec![profile("p1", "prod-01")]);
        let exec = Arc::new(FakeExec {
            calls: Arc::new(AtomicUsize::new(0)),
            rules: vec![("__PROC__".into(), cool_probe())],
            default: cool_probe(),
        });
        let rt = runtime(store, exec, vec![profile("p1", "prod-01")]);
        // 第一轮正常跑一个只读工具；第二轮前把 cancel 置位
        let cancel = Arc::new(AtomicBool::new(false));
        let mut spec = interactive_spec(false);
        spec.cancel = cancel.clone();
        let llm = scripted(vec![
            AgentTurn {
                content: String::new(),
                tool_calls: vec![call("c1", "inspect_load", r#"{"selector":{"kind":"all"}}"#)],
            },
            AgentTurn {
                content: String::new(),
                tool_calls: vec![call("c2", "inspect_load", r#"{"selector":{"kind":"all"}}"#)],
            },
        ]);
        // 第二轮开始前置位：用 llm 的 `seen` 长度判断已经问过一轮
        let seen = llm.seen.clone();
        let cancel2 = cancel.clone();
        std::thread::spawn(move || {
            for _ in 0..200 {
                if !seen.lock().is_empty() {
                    cancel2.store(true, Ordering::SeqCst);
                    return;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        let sink = ScriptedSink::new(AgentDecision::Approve);
        run_interactive_agent(&spec, &rt, &llm, &sink).unwrap();

        let rows = sink.rows.lock().clone();
        assert!(!rows.is_empty(), "停止后已收到的轨迹必须保留（不能清空）");
        assert_eq!(
            sink.done.lock().clone().unwrap().1,
            agent::FinishReason::Cancelled,
            "停止要收尾成「已取消」，不是「出错」"
        );
        // 每一条都必须收口：否则界面会留一行永远转圈的「执行中」
        for r in &rows {
            assert!(r.state.is_settled(), "停止后仍有未收口的行：{r:?}");
        }
    }

    /// 2026-09-22「最大工具步数移除，不做限制」：交互式循环同样不再按轮数收尾。
    ///
    /// 脚本给 10 轮工具调用（超过旧默认上限 8），全部都要真的跑完；
    /// 收尾由模型自己的结束轮给出，而不是被某个步数闸门截断。
    #[test]
    fn interactive_loop_is_no_longer_capped_by_step_count() {
        let store = store_with(vec![profile("p1", "prod-01")]);
        let exec = Arc::new(FakeExec {
            calls: Arc::new(AtomicUsize::new(0)),
            rules: vec![("__PROC__".into(), cool_probe())],
            default: cool_probe(),
        });
        let rt = runtime(store, exec, vec![profile("p1", "prod-01")]);
        let turns: Vec<AgentTurn> = (0..10)
            .map(|i| AgentTurn {
                content: String::new(),
                tool_calls: vec![call(
                    &format!("c{i}"),
                    "inspect_load",
                    r#"{"selector":{"kind":"all"}}"#,
                )],
            })
            .collect();
        let spec = interactive_spec(false);
        let sink = ScriptedSink::new(AgentDecision::Approve);
        run_interactive_agent(&spec, &rt, &scripted(turns), &sink).unwrap();

        assert_eq!(
            sink.rows.lock().len(),
            10,
            "10 轮工具调用必须全部执行（旧默认 8 步会截断成 8 行）"
        );
        let (conclusion, reason) = sink.done.lock().clone().unwrap();
        assert_eq!(
            reason,
            agent::FinishReason::NoMoreTools,
            "收尾原因应当是模型自己结束，而不是步数上限"
        );
        assert!(!conclusion.is_empty(), "收尾要给用户一句解释：{conclusion}");
    }

    /// LLM 出错也要走到 `done`：否则面板会永远停在「巡检中」。
    #[test]
    fn llm_error_still_reports_done() {
        struct FailingLlm;
        impl AgentLlm for FailingLlm {
            fn turn(
                &self,
                _messages: &[ChatMessage],
                _tools: Vec<serde_json::Value>,
            ) -> Result<AgentTurn, String> {
                Err("连接被重置".into())
            }
        }
        let store = store_with(vec![profile("p1", "prod-01")]);
        let exec = Arc::new(FakeExec {
            calls: Arc::new(AtomicUsize::new(0)),
            rules: Vec::new(),
            default: String::new(),
        });
        let rt = runtime(store, exec, vec![profile("p1", "prod-01")]);
        let sink = ScriptedSink::new(AgentDecision::Approve);
        run_interactive_agent(&interactive_spec(false), &rt, &FailingLlm, &sink).unwrap();
        let reported = sink.done.lock().clone();
        match reported {
            Some((_, agent::FinishReason::Error(e))) => assert!(e.contains("连接被重置")),
            other => panic!("出错必须收尾成 Error：{other:?}"),
        }
    }

    /// `propose_task` 在用户点「执行」后必须**真的落一条任务草案**（默认停用）。
    ///
    /// 这条用例防的是「模型告诉用户『已为你创建』，而任务列表里什么都没有」——
    /// 那种失败对用户来说与「AI 在撒谎」没有区别。
    #[test]
    fn approved_proposal_creates_a_real_disabled_task() {
        let st = store_with(vec![profile("p1", "prod-01"), profile("p2", "prod-02")]);
        let exec = Arc::new(FakeExec {
            calls: Arc::new(AtomicUsize::new(0)),
            rules: Vec::new(),
            default: String::new(),
        });
        let rt = runtime(
            st,
            exec,
            vec![profile("p1", "prod-01"), profile("p2", "prod-02")],
        );
        let turn = AgentTurn {
            content: String::new(),
            tool_calls: vec![call(
                "c1",
                "propose_task",
                r#"{"name":"夜间巡检","schedule":"0 3 * * *","action":"inspect_ai","targets":["p1","p2"],"prompt":"看看负载与磁盘占用"}"#,
            )],
        };
        let spec = interactive_spec(true);

        // ① 用户同意 → 草案落进 sink
        let sink = ScriptedSink::new(AgentDecision::Approve);
        run_interactive_agent(&spec, &rt, &scripted(vec![turn.clone()]), &sink).unwrap();
        let tasks = sink.proposed.lock().clone();
        assert_eq!(tasks.len(), 1, "确认后必须真的产出一条任务草案");
        assert_eq!(tasks[0].name, "夜间巡检");
        assert!(!tasks[0].enabled, "草案默认停用，等用户去面板启用");
        assert_eq!(
            tasks[0].action.target().profiles.len(),
            2,
            "目标要落成具体 id（两台）"
        );
        // 轨迹上是「完成」，不是「等待确认」（不然用户以为还卡着）
        assert!(sink.rows.lock()[0].state.is_settled());

        // ② 用户拒绝 → 一条也不落
        let sink2 = ScriptedSink::new(AgentDecision::Deny);
        run_interactive_agent(&spec, &rt, &scripted(vec![turn]), &sink2).unwrap();
        assert!(sink2.proposed.lock().is_empty(), "用户拒绝了却仍然建了任务");
    }

    /// 界面上那一行只写「发现 N 项异常」，**明细是唯一能看到是哪几项的地方**。
    ///
    /// 这条回归针对一个真实缺陷：接线第一版给 `settle_row` 传的 `detail` 是空串，
    /// 于是「明细」折叠块永远没有内容 —— 界面一切正常，用户却无从知道
    /// 11 项异常分别是什么、在哪台上。
    #[test]
    fn trace_row_carries_the_anomaly_detail() {
        let store = store_with(vec![profile("p1", "prod-01"), profile("p2", "prod-02")]);
        // 两台都热：CPU/内存都越阈值 ⇒ 一定产生 finding
        let exec = Arc::new(FakeExec {
            calls: Arc::new(AtomicUsize::new(0)),
            rules: vec![("__PROC__".into(), hot_probe())],
            default: hot_probe(),
        });
        let rt = runtime(
            store,
            exec,
            vec![profile("p1", "prod-01"), profile("p2", "prod-02")],
        );
        let llm = scripted(vec![AgentTurn {
            content: String::new(),
            tool_calls: vec![call("c1", "inspect_load", r#"{"selector":{"kind":"all"}}"#)],
        }]);
        let sink = ScriptedSink::new(AgentDecision::Approve);
        run_interactive_agent(&interactive_spec(false), &rt, &llm, &sink).unwrap();

        let rows = sink.rows.lock().clone();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(
            row.state,
            TraceState::Anomaly,
            "有 finding 就该走 warning 态"
        );
        assert!(
            row.note.contains("异常"),
            "行内文案要说明发现了异常：{}",
            row.note
        );
        let detail = row.truncated_detail();
        assert!(
            !detail.trim().is_empty(),
            "轨迹明细为空 —— 用户点开折叠块看不到任何异常内容"
        );
        assert!(
            detail.contains("异常明细"),
            "明细要先列出异常项：\n{detail}"
        );
        assert!(
            detail.contains("prod-01"),
            "明细要指明是哪台机器：\n{detail}"
        );
        // 原始输出也要在（用户可能想看原始行）
        assert!(detail.contains("MemTotal"), "明细要带原始输出：\n{detail}");
    }

    /// 通道版 sink：确认回话按 `call_id` 匹配，别的调用的回话不能顶替。
    #[test]
    fn channel_sink_matches_decisions_by_call_id() {
        let (tx, rx) = flume::unbounded::<AgentUiEvent>();
        let (dtx, drx) = flume::unbounded::<(String, AgentDecision)>();
        let sink = ChannelSink::new(tx, drx, Arc::new(AtomicBool::new(true)));
        let req = ConfirmRequest {
            step: 1,
            call_id: "want".into(),
            tool: "run_command".into(),
            summary: "prod-01 · ls".into(),
            reason: "会改变状态".into(),
            target_count: 1,
            command: "ls".into(),
            whitelistable: true,
        };
        // 回话线程：先塞一个**别的**调用的决定，再塞正确的
        let worker = std::thread::spawn(move || {
            // 等卡片事件出现（说明 sink 已经在等回话）
            let _ev = rx.recv().expect("应当收到确认事件");
            dtx.send(("someone-else".into(), AgentDecision::Approve))
                .unwrap();
            dtx.send(("want".into(), AgentDecision::Deny)).unwrap();
            // 后续事件（done）也收掉，避免 channel 悬空
            while rx.recv().is_ok() {}
        });
        assert_eq!(
            sink.confirm(&req),
            AgentDecision::Deny,
            "必须按 call_id 取自己的回话，而不是「先到先得」"
        );
        sink.done("结论", &agent::FinishReason::NoMoreTools);
        drop(sink);
        worker.join().unwrap();
    }

    /// 面板已关（`alive = false`）：确认立即按忽略返回，绝不永远阻塞。
    #[test]
    fn channel_sink_denies_immediately_when_the_panel_is_gone() {
        let (tx, rx) = flume::unbounded::<AgentUiEvent>();
        let (_dtx, drx) = flume::unbounded::<(String, AgentDecision)>();
        let sink = ChannelSink::new(tx, drx, Arc::new(AtomicBool::new(false)));
        let req = ConfirmRequest {
            step: 1,
            call_id: "c1".into(),
            tool: "run_command".into(),
            summary: String::new(),
            reason: String::new(),
            target_count: 1,
            command: "ls".into(),
            whitelistable: true,
        };
        let started = std::time::Instant::now();
        assert_eq!(sink.confirm(&req), AgentDecision::Deny);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "面板已关时必须立刻返回，而不是等满超时"
        );
        assert!(matches!(rx.try_recv(), Ok(AgentUiEvent::Confirm(_))));
    }
}
