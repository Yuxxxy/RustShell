//! 巡检的**纯逻辑**部分：把探针输出变成可读结论与告警 finding。
//!
//! 这里不碰网络也不碰 SSH —— 输入是「命令 + 它的输出」，输出是
//! [`TargetReport`]。于是判定逻辑（阈值、异常 vs 巡检失败的分离、样本提取）
//! 可以完全靠单测覆盖，而执行侧只需要负责把命令跑出去。

use crate::agent::tools::{CERT_CRIT_DAYS, CERT_WARN_DAYS, MAX_LINE_CHARS};
use crate::alerts::{
    exit_fingerprint, log_fingerprint, metric_fingerprint, Finding, FindingKind, LoadThresholds,
    Severity, HOST_KEY_FP,
};
use crate::metrics::{DiskInfo, Sample, SystemOverview};
use crate::tasks::{LogLevel, TargetOutcome};
use serde::{Deserialize, Serialize};

/// 单条命令的输出（执行侧产出，判定侧消费）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub exit_code: Option<u32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
}

impl CommandOutput {
    pub fn ok(stdout: impl Into<String>) -> Self {
        Self {
            exit_code: Some(0),
            stdout: stdout.into(),
            stderr: String::new(),
            timed_out: false,
        }
    }

    /// 命令是否成功结束（退出码 0 且未超时）。
    pub fn succeeded(&self) -> bool {
        !self.timed_out && self.exit_code == Some(0)
    }

    /// 失败原因（成功返回 `None`）。
    pub fn failure(&self) -> Option<String> {
        if self.timed_out {
            return Some(crate::i18n::ts(
                "命令超时".into(),
                "command timed out".into(),
            ));
        }
        match self.exit_code {
            Some(0) => None,
            Some(c) => Some(crate::i18n::ts(format!("退出码 {c}"), format!("exit {c}"))),
            None => Some(crate::i18n::ts(
                "未取得退出码".into(),
                "no exit status".into(),
            )),
        }
    }
}

/// 一台机器的巡检结果。
#[derive(Debug, Clone, PartialEq)]
pub struct TargetReport {
    pub profile_id: String,
    pub profile_name: String,
    /// 连接是否成功
    pub connected: bool,
    /// 探针命令是否都成功
    pub exec_ok: bool,
    pub anomaly: bool,
    pub summary: String,
    pub detail: Option<String>,
    pub findings: Vec<Finding>,
}

impl TargetReport {
    /// 转成运行记录里的逐台结果。
    pub fn to_outcome(&self) -> TargetOutcome {
        TargetOutcome {
            profile_id: self.profile_id.clone(),
            profile_name: self.profile_name.clone(),
            connected: self.connected,
            exec_ok: self.exec_ok,
            anomaly: self.anomaly,
            summary: self.summary.clone(),
            detail: self.detail.clone(),
        }
    }

    /// 连接/命令层面的失败报告（「巡检失败」而不是「服务器异常」）。
    pub fn failure(profile_id: &str, profile_name: &str, connected: bool, reason: String) -> Self {
        Self {
            profile_id: profile_id.to_string(),
            profile_name: profile_name.to_string(),
            connected,
            exec_ok: false,
            anomaly: false,
            summary: reason.clone(),
            detail: None,
            findings: vec![inspection_failure_finding(
                profile_id,
                profile_name,
                &reason,
                connected,
            )],
        }
    }
}

/// 把「巡检本身失败」变成 finding。
///
/// **关键**：主机密钥类失败必须带上专属指纹，才能命中 `builtin.host_key*`
/// 那两条规则（Critical / 未知）。否则「疑似中间人」会被淹没在
/// 一堆普通连接失败里，用户根本注意不到。
pub fn inspection_failure_finding(
    profile_id: &str,
    profile_name: &str,
    reason: &str,
    connected: bool,
) -> Finding {
    let fingerprint = if reason.contains(crate::ssh::HOST_KEY_UNKNOWN_MARK) {
        crate::alerts::HOST_KEY_UNKNOWN_FP.to_string()
    } else if reason.contains("不一致") || reason.contains("mismatch") {
        HOST_KEY_FP.to_string()
    } else if !connected {
        "connect:failed".to_string()
    } else {
        "probe:failed".to_string()
    };
    // 主机密钥不一致是安全事故，不是普通失败
    let severity = if fingerprint == HOST_KEY_FP {
        Severity::Critical
    } else {
        Severity::Warning
    };
    Finding {
        kind: FindingKind::InspectionFailed,
        profile_id: profile_id.to_string(),
        profile_name: profile_name.to_string(),
        rule_id: "builtin.inspect_failed".to_string(),
        fingerprint,
        severity,
        message: reason.to_string(),
        evidence: None,
    }
}

/// 负载巡检项。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadCheck {
    Cpu,
    Mem,
    Swap,
    Disk,
    Load,
}

impl LoadCheck {
    pub fn label(self) -> &'static str {
        match self {
            Self::Cpu => "cpu",
            Self::Mem => "mem",
            Self::Swap => "swap",
            Self::Disk => "disk",
            Self::Load => "load",
        }
    }
}

/// 已解析出的负载指标（供展示与判定共用的中间形态）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LoadMetrics {
    pub sample: Option<Sample>,
    pub overview: Option<SystemOverview>,
    /// 1 分钟负载 / 核数
    pub load_per_core: Option<f32>,
    /// CPU 核数
    pub cores: Option<usize>,
}

/// 从两次 `/proc` 快照解析负载指标。
///
/// 传两次是为了让 `parse_proc_snapshot` 能算出 CPU 使用率（它按两次采样的
/// 计数器差值算）。只给一次时 CPU 百分比会是 0 —— 那比「没有数据」更误导人。
pub fn parse_load(probe_text: &str, prev: &mut crate::ssh::ProcSnapshot) -> LoadMetrics {
    let mut m = LoadMetrics {
        sample: crate::ssh::parse_proc_snapshot(probe_text, prev),
        ..Default::default()
    };
    let ov = crate::ssh::parse_system_overview(probe_text);
    if !ov.is_empty() {
        m.overview = Some(ov);
    }
    // 负载均值：`__LOADAVG__` 段（PROC_PROBE_CMD 未采集时用 /proc/loadavg 单独取）
    if let Some(l) = parse_loadavg(probe_text) {
        m.cores = cpu_cores(probe_text);
        if let Some(c) = m.cores.filter(|c| *c > 0) {
            m.load_per_core = Some(l / c as f32);
        }
    }
    m
}

/// 从探针输出里取 1 分钟负载均值（`__LOADAVG__` 段，或裸 `/proc/loadavg` 内容）。
fn parse_loadavg(text: &str) -> Option<f32> {
    // 取标记段的**首个非空行**：`echo __LOADAVG__` 之后紧跟换行，
    // 所以 `.lines().next()` 拿到的是空串，必须跳过空行。
    let body = text.split("__LOADAVG__").nth(1).unwrap_or(text);
    let line = body.lines().find(|l| !l.trim().is_empty())?;
    line.split_whitespace().next()?.parse::<f32>().ok()
}

/// 从 `/proc/stat` 的 `cpuN` 行数出核数。
fn cpu_cores(text: &str) -> Option<usize> {
    let n = text
        .lines()
        .filter(|l| {
            let t = l.trim_start();
            t.starts_with("cpu") && t.as_bytes().get(3).is_some_and(u8::is_ascii_digit)
        })
        .count();
    (n > 0).then_some(n)
}

/// 按阈值判定负载异常，产出 findings（并把可读结论写进 `summary_parts`）。
///
/// 返回 `(findings, 可读行)`。判定与文案在同一处完成：两处分开写的话，
/// 「报出来的异常」与「显示的数字」很容易对不上，而那种不一致会直接
/// 让用户怀疑整个巡檢结果的可靠性。
pub fn judge_load(
    metrics: &LoadMetrics,
    thresholds: &LoadThresholds,
    checks: &[LoadCheck],
) -> (Vec<Finding>, Vec<String>) {
    let mut findings = Vec::new();
    let mut lines = Vec::new();
    let wants = |c: LoadCheck| checks.is_empty() || checks.contains(&c);

    if let Some(s) = metrics.sample {
        if wants(LoadCheck::Cpu) {
            let pct = s.cpu_percent;
            lines.push(format!("CPU {pct:.0}%"));
            if pct > thresholds.cpu_percent {
                findings.push(metric_finding(
                    LoadCheck::Cpu,
                    &format!("CPU 使用率 {pct:.0}%，阈值 {:.0}%", thresholds.cpu_percent),
                    pct,
                    Severity::Warning,
                ));
            }
        }
        if wants(LoadCheck::Mem) {
            let pct = s.mem_percent;
            lines.push(format!(
                "内存 {pct:.0}%（{}）",
                if s.mem_total > 0 {
                    format!(
                        "{}/{}",
                        crate::metrics::format_bytes_short(s.mem_used),
                        crate::metrics::format_bytes_short(s.mem_total)
                    )
                } else {
                    "-".to_string()
                }
            ));
            if pct > thresholds.mem_percent {
                findings.push(metric_finding(
                    LoadCheck::Mem,
                    &format!("内存使用率 {pct:.0}%，阈值 {:.0}%", thresholds.mem_percent),
                    pct,
                    Severity::Critical,
                ));
            }
        }
        if wants(LoadCheck::Swap) && s.swap_total > 0 {
            let pct = s.swap_percent;
            lines.push(format!("Swap {pct:.0}%"));
            if pct > thresholds.swap_percent {
                findings.push(metric_finding(
                    LoadCheck::Swap,
                    &format!(
                        "Swap 使用率 {pct:.0}%，阈值 {:.0}%",
                        thresholds.swap_percent
                    ),
                    pct,
                    Severity::Warning,
                ));
            }
        }
    }

    if let Some(lpc) = metrics.load_per_core {
        if wants(LoadCheck::Load) {
            lines.push(format!("负载/核 {lpc:.2}"));
            if lpc > thresholds.load_per_core {
                findings.push(Finding {
                    kind: FindingKind::Anomaly,
                    profile_id: String::new(),
                    profile_name: String::new(),
                    rule_id: "builtin.load".into(),
                    fingerprint: metric_fingerprint("load", thresholds.load_per_core),
                    severity: Severity::Warning,
                    message: format!(
                        "1 分钟负载/核 {lpc:.2}，阈值 {:.2}",
                        thresholds.load_per_core
                    ),
                    evidence: metrics.cores.map(|c| format!("{c} 核")),
                });
            }
        }
    }

    if wants(LoadCheck::Disk) {
        if let Some(ov) = &metrics.overview {
            for d in ov.disks.iter().filter(|d| is_root_or_big(d)) {
                let pct = d.percent();
                lines.push(format!("{} {pct:.0}%", d.mount));
                if pct > thresholds.disk_percent {
                    findings.push(Finding {
                        kind: FindingKind::Anomaly,
                        profile_id: String::new(),
                        profile_name: String::new(),
                        rule_id: "builtin.load".into(),
                        fingerprint: metric_fingerprint("disk", thresholds.disk_percent),
                        severity: Severity::Critical,
                        message: format!(
                            "磁盘 {} 使用率 {pct:.0}%（{}），阈值 {:.0}%",
                            d.mount,
                            d.used_over_total(),
                            thresholds.disk_percent
                        ),
                        evidence: d.avail_text().map(|a| format!("可用 {a}")),
                    });
                }
            }
        }
    }
    (findings, lines)
}

/// 只对「根分区或大容量盘」告警：`/boot`、`/run` 这类小分区常年高位，
/// 每次都报只会把真正的磁盘快满淹没掉。
fn is_root_or_big(d: &DiskInfo) -> bool {
    d.mount == "/" || d.total >= 10 * 1024 * 1024 * 1024
}

fn metric_finding(check: LoadCheck, message: &str, threshold: f32, sev: Severity) -> Finding {
    Finding {
        kind: FindingKind::Anomaly,
        profile_id: String::new(),
        profile_name: String::new(),
        rule_id: "builtin.load".into(),
        fingerprint: metric_fingerprint(check.label(), threshold),
        severity: sev,
        message: message.to_string(),
        evidence: None,
    }
}

/// 日志巡检的探针命令。
///
/// 路径与 pattern 都过 [`crate::tasks::shell_quote`]：这是注入的唯一入口。
/// 三个标记（`__RS_HITS__` / `__RS_MISSING__` / `__RS_NOPERM__`）让判定侧
/// 能区分「没有异常」与「查不了」—— 后者是**巡检失败**，不是服务器异常。
pub fn log_probe_command(path: &str, pattern: &str, tail_lines: u32, sample_lines: u32) -> String {
    let p = crate::tasks::shell_quote(path);
    let pat = crate::tasks::shell_quote(pattern);
    let tail = tail_lines.clamp(1, crate::tasks::types::MAX_PATTERN_LEN as u32 * 10);
    let sample = sample_lines.clamp(1, 100);
    format!(
        "if [ ! -e {p} ]; then echo __RS_MISSING__; exit 3; fi; \
         if [ ! -r {p} ]; then echo __RS_NOPERM__; exit 4; fi; \
         n=$(tail -n {tail} {p} 2>/dev/null | grep -E -c {pat} || true); \
         echo \"__RS_HITS__${{n:-0}}\"; \
         tail -n {tail} {p} 2>/dev/null | grep -E {pat} | tail -n {sample}"
    )
}

/// 日志巡检结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogProbe {
    /// 命中 `hits` 行，附样本
    Hits { hits: u32, samples: Vec<String> },
    /// 日志文件不存在
    Missing,
    /// 日志文件不可读
    NoPermission,
    /// 其他失败（命令非零退出等）
    Failed(String),
}

/// 解析日志探针输出。
///
/// `level` 只用于失败文案的语境（命中数的级别标签在 finding 指纹里由调用方给），
/// 故此处不参与解析 —— 保留参数是为了让签名与 [`judge_log`] 对齐，调用点
/// 不必记住「哪个函数要级别、哪个不要」。
pub fn parse_log_probe(out: &CommandOutput, level: LogLevel, path: &str) -> LogProbe {
    let _ = level;
    if out.timed_out {
        return LogProbe::Failed(crate::i18n::ts(
            "日志检查超时".into(),
            "log check timed out".into(),
        ));
    }
    if out.stdout.contains("__RS_MISSING__") {
        return LogProbe::Missing;
    }
    if out.stdout.contains("__RS_NOPERM__") {
        return LogProbe::NoPermission;
    }
    let Some(hits) = out
        .stdout
        .lines()
        .find_map(|l| l.trim().strip_prefix("__RS_HITS__"))
        .and_then(|n| n.trim().parse::<u32>().ok())
    else {
        return LogProbe::Failed(match out.failure() {
            Some(r) => r,
            None => crate::i18n::ts(
                format!("无法解析日志检查结果（{path}）"),
                format!("unparsable log check result ({path})"),
            ),
        });
    };
    // 样本 = 标记行之后的所有非标记行
    let samples: Vec<String> = out
        .stdout
        .lines()
        .skip_while(|l| !l.trim().starts_with("__RS_HITS__"))
        .skip(1)
        .filter(|l| !l.trim().is_empty())
        .map(crate::ai::redact)
        .collect();
    LogProbe::Hits { hits, samples }
}

/// 把日志巡检结果变成结论 + findings。
///
/// 「路径不存在 / 无权限」在这里被明确降级成**巡检失败**（`InspectionFailed`），
/// 而不是异常 —— 这是需求文档 §8.3 的核心要求：把「查不了」报成「服务器有问题」
/// 会让运维照着错误的方向去排查。
pub fn judge_log(
    probe: &LogProbe,
    path: &str,
    level: LogLevel,
) -> (bool, String, Option<String>, Vec<Finding>) {
    match probe {
        LogProbe::Hits { hits: 0, .. } => (
            false,
            crate::i18n::ts(format!("{path} 无异常"), format!("{path}: no anomalies")),
            None,
            Vec::new(),
        ),
        LogProbe::Hits { hits, samples } => {
            let evidence = if samples.is_empty() {
                None
            } else {
                Some(samples.join("\n"))
            };
            let f = Finding {
                kind: FindingKind::Anomaly,
                profile_id: String::new(),
                profile_name: String::new(),
                rule_id: "builtin.log".into(),
                fingerprint: log_fingerprint(level.label(), *hits),
                severity: Severity::Warning,
                message: crate::i18n::ts(
                    format!("{path} 命中 {hits} 行异常（级别 {}）", level.label()),
                    format!("{path}: {hits} anomalous lines (level {})", level.label()),
                ),
                evidence: evidence.clone(),
            };
            (
                true,
                crate::i18n::ts(
                    format!("日志命中 {hits} 行"),
                    format!("{hits} log lines matched"),
                ),
                evidence,
                vec![f],
            )
        }
        LogProbe::Missing => inspection_failed_for(
            "builtin.inspect_failed",
            &crate::i18n::ts(
                format!("日志路径不存在：{path}"),
                format!("log path does not exist: {path}"),
            ),
            "log:missing",
        ),
        LogProbe::NoPermission => inspection_failed_for(
            "builtin.inspect_failed",
            &crate::i18n::ts(
                format!("日志不可读（权限不足）：{path}"),
                format!("log not readable (permission denied): {path}"),
            ),
            "log:noperm",
        ),
        LogProbe::Failed(e) => inspection_failed_for(
            "builtin.inspect_failed",
            &crate::i18n::ts(
                format!("日志检查失败：{e}"),
                format!("log check failed: {e}"),
            ),
            "log:failed",
        ),
    }
}

// ── 只读增强五件套 ──────────────────────────────────────────────
//
// `read_file` / `inspect_service` / `inspect_ports` / `check_certificate` /
// `find_process` 的探针与判定。形状与 `inspect_log` 一致：远端一条**内容固定**
// 的探针（模型给的字符串只出现在 `shell_quote` 包住的位置，或者干脆不进命令），
// 本地解析成结论与 finding —— 于是判定逻辑照样能脱离网络单测。
//
// 三条共同纪律：
// 1. 「查不了」与「没有」必须分开：工具缺失、路径不存在一律走**巡检失败**，
//    绝不静默变成「一切正常」；
// 2. 只读信息类工具（read_file / inspect_ports / find_process）**不发明阈值**：
//    它们只报告事实，异常与否由模型看数据后判断；
// 3. 明细里原样保留远端输出（脱敏后），模型看摘要、用户点开看证据。

/// `read_file` 的探针命令。
///
/// 三个标记把「看不了」与「是空的」分开：不存在 / 是目录 / 无权限都是巡检失败。
/// 路径过 [`crate::tasks::shell_quote`]，行窗口是调用方算的纯数字。
pub fn read_probe_command(path: &str, start_line: u32, max_lines: u32) -> String {
    let p = crate::tasks::shell_quote(path);
    let start = start_line.max(1);
    let end = start.saturating_add(max_lines.max(1)).saturating_sub(1);
    format!(
        "if [ ! -e {p} ]; then echo __RS_MISSING__; exit 3; fi; \
         if [ -d {p} ]; then echo __RS_ISDIR__; exit 5; fi; \
         if [ ! -r {p} ]; then echo __RS_NOPERM__; exit 4; fi; \
         t=$(wc -l < {p} 2>/dev/null | tr -d ' '); echo \"__RS_TOTAL__${{t:-0}}\"; \
         sed -n '{start},{end}p' {p} 2>/dev/null"
    )
}

/// `read_file` 的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadProbe {
    Lines {
        /// `wc -l` 口径的总行数（无尾换行的最后一行不计，可接受）
        total: u64,
        /// 本窗口内的行（已脱敏、超长行已裁）
        lines: Vec<String>,
        /// 因单行过长被裁的行数
        clipped: usize,
    },
    Missing,
    IsDirectory,
    NoPermission,
    Failed(String),
}

/// 解析 `read_file` 探针输出。
pub fn parse_read_probe(out: &CommandOutput) -> ReadProbe {
    if out.timed_out {
        return ReadProbe::Failed(crate::i18n::ts("读取超时".into(), "read timed out".into()));
    }
    if out.stdout.contains("__RS_MISSING__") {
        return ReadProbe::Missing;
    }
    if out.stdout.contains("__RS_ISDIR__") {
        return ReadProbe::IsDirectory;
    }
    if out.stdout.contains("__RS_NOPERM__") {
        return ReadProbe::NoPermission;
    }
    let Some(total) = out
        .stdout
        .lines()
        .find_map(|l| l.trim().strip_prefix("__RS_TOTAL__"))
        .and_then(|n| n.trim().parse::<u64>().ok())
    else {
        return ReadProbe::Failed(match out.failure() {
            Some(r) => crate::i18n::ts(format!("读取失败：{r}"), format!("read failed: {r}")),
            None => crate::i18n::ts("无法解析读取结果".into(), "unparsable read result".into()),
        });
    };
    let mut lines = Vec::new();
    let mut clipped = 0;
    for line in out
        .stdout
        .lines()
        .skip_while(|l| !l.trim().starts_with("__RS_TOTAL__"))
        .skip(1)
    {
        let (text, was_clipped) = clip_line(&crate::ai::redact(line));
        if was_clipped {
            clipped += 1;
        }
        lines.push(text);
    }
    ReadProbe::Lines {
        total,
        lines,
        clipped,
    }
}

/// 单行裁剪：压缩过的 js / 单行 JSON 一行就能吃掉整个回注预算。
fn clip_line(line: &str) -> (String, bool) {
    let n = line.chars().count();
    if n <= MAX_LINE_CHARS {
        return (line.to_string(), false);
    }
    let head: String = line.chars().take(MAX_LINE_CHARS).collect();
    (format!("{head} …（本行共 {n} 字符）"), true)
}

/// 读文件的结论。
pub fn judge_read(
    probe: &ReadProbe,
    path: &str,
    start_line: u32,
    max_lines: u32,
) -> (bool, String, Option<String>, Vec<Finding>) {
    use crate::i18n::ts;
    match probe {
        ReadProbe::Lines {
            total,
            lines,
            clipped,
        } => {
            let start = start_line.max(1);
            let end = start.saturating_add(max_lines.max(1)).saturating_sub(1);
            if lines.is_empty() {
                // 窗口落在文件末尾之外：说清「文件多大、你要的区间在哪」，
                // 否则模型只看到一句「空」然后重试同一个区间。
                return (
                    false,
                    ts(
                        format!("{path} 共 {total} 行，第 {start}-{end} 行没有内容"),
                        format!("{path} has {total} lines; lines {start}-{end} are empty"),
                    ),
                    None,
                    Vec::new(),
                );
            }
            let mut summary = ts(
                format!("{path} 第 {start}-{end} 行（共 {total} 行）"),
                format!("{path} lines {start}-{end} (of {total})"),
            );
            if *clipped > 0 {
                summary.push_str(&ts(
                    format!(" · {clipped} 行过长已截断"),
                    format!(" · {clipped} line(s) truncated"),
                ));
            }
            (false, summary, Some(lines.join("\n")), Vec::new())
        }
        ReadProbe::Missing => inspection_failed_for(
            "builtin.inspect_failed",
            &ts(
                format!("文件不存在：{path}"),
                format!("file does not exist: {path}"),
            ),
            "read:missing",
        ),
        ReadProbe::IsDirectory => inspection_failed_for(
            "builtin.inspect_failed",
            &ts(
                format!("{path} 是目录，不是文件"),
                format!("{path} is a directory, not a file"),
            ),
            "read:isdir",
        ),
        ReadProbe::NoPermission => inspection_failed_for(
            "builtin.inspect_failed",
            &ts(
                format!("文件不可读（权限不足）：{path}"),
                format!("file not readable (permission denied): {path}"),
            ),
            "read:noperm",
        ),
        ReadProbe::Failed(e) => inspection_failed_for(
            "builtin.inspect_failed",
            &ts(format!("读取失败：{e}"), format!("read failed: {e}")),
            "read:failed",
        ),
    }
}

/// `inspect_service` 的探针命令。
///
/// 用 `systemctl show` 而不是 `systemctl is-active`：`show` 一次给出
/// 激活态 + 子状态 + 开机自启 + 重启次数，且**机器可解析**（`key=value`），
/// 而 `is-active` 只有一行、退出码语义还得额外记一遍。
pub fn service_probe_command(unit: &str) -> String {
    let u = crate::tasks::shell_quote(unit);
    format!(
        "if ! command -v systemctl >/dev/null 2>&1; then echo __RS_NOSYSTEMD__; exit 0; fi; \
         systemctl show {u} -p Id,LoadState,ActiveState,SubState,UnitFileState,MainPID,\
         ExecMainStatus,NRestarts,ActiveEnterTimestamp --no-pager 2>/dev/null; \
         echo \"__RS_RC__$?\""
    )
}

/// `inspect_service` 的解析结果（`systemctl show` 的属性表）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ServiceProbe {
    pub props: Vec<(String, String)>,
}

impl ServiceProbe {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.props
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// 单元不存在（`LoadState=not-found`）——「查错了名字」而不是「服务没跑」。
    pub fn missing(&self) -> bool {
        self.get("LoadState") == Some("not-found")
    }

    /// 一句话状态（`active (running)`）。
    pub fn state_text(&self) -> String {
        match (self.get("ActiveState"), self.get("SubState")) {
            (Some(a), Some(s)) => format!("{a} ({s})"),
            (Some(a), None) => a.to_string(),
            _ => "unknown".to_string(),
        }
    }

    /// 开机自启状态（enabled / disabled / static / masked…，取不到为空串）。
    pub fn unit_file_state(&self) -> &str {
        self.get("UnitFileState").unwrap_or("")
    }

    /// 证据文本（原样属性表，`findings.evidence` 与轨迹明细共用）。
    pub fn evidence(&self) -> String {
        self.props
            .iter()
            .map(|(k, v)| format!("{k}={v}\n"))
            .collect()
    }
}

/// 解析 `inspect_service` 探针输出（非 `key=value` 行一律忽略）。
pub fn parse_service_probe(out: &CommandOutput) -> Result<ServiceProbe, String> {
    if out.timed_out {
        return Err(crate::i18n::ts("查询超时".into(), "query timed out".into()));
    }
    if out.stdout.contains("__RS_NOSYSTEMD__") {
        return Err(crate::i18n::ts(
            "该主机没有 systemd（systemctl 不存在）".into(),
            "no systemd on this host (systemctl not found)".into(),
        ));
    }
    let props: Vec<(String, String)> = out
        .stdout
        .lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();
    if props.is_empty() {
        return Err(match out.failure() {
            Some(r) => crate::i18n::ts(
                format!("查询单元状态失败：{r}"),
                format!("could not query the unit: {r}"),
            ),
            None => crate::i18n::ts("查询单元状态失败".into(), "could not query the unit".into()),
        });
    }
    Ok(ServiceProbe { props })
}

/// 服务状态的结论。
///
/// **只有 `failed` 判成异常**：`inactive` 对一次性单元（oneshot / timer）是正常的，
/// 一律报异常会让每次巡检都刷假警报 ——「这个服务该不该在跑」得由模型看状态自己判。
pub fn judge_service(
    probe: Result<ServiceProbe, String>,
    unit: &str,
) -> (bool, String, Option<String>, Vec<Finding>) {
    use crate::i18n::ts;
    let probe = match probe {
        Ok(p) => p,
        Err(e) => {
            return inspection_failed_for(
                "builtin.inspect_failed",
                &ts(
                    format!("查询 {unit} 失败：{e}"),
                    format!("could not query {unit}: {e}"),
                ),
                "service:query-failed",
            )
        }
    };
    if probe.missing() {
        return inspection_failed_for(
            "builtin.inspect_failed",
            &ts(
                format!("单元不存在：{unit}"),
                format!("no such unit: {unit}"),
            ),
            "service:not-found",
        );
    }
    let state = probe.state_text();
    let enabled = probe.unit_file_state();
    let mut summary = ts(format!("{unit} {state}"), format!("{unit} {state}"));
    if !enabled.is_empty() {
        summary.push_str(&format!(" · {enabled}"));
    }
    let evidence = probe.evidence();

    if probe.get("ActiveState") == Some("failed") {
        let code = probe.get("ExecMainStatus").unwrap_or("-");
        let restarts = probe.get("NRestarts").unwrap_or("0");
        let f = Finding {
            kind: FindingKind::Anomaly,
            profile_id: String::new(),
            profile_name: String::new(),
            rule_id: "builtin.service".into(),
            fingerprint: "service:failed".into(),
            severity: Severity::Critical,
            message: ts(
                format!("{unit} 处于 failed 状态（退出码 {code}，已重启 {restarts} 次）"),
                format!("{unit} is in failed state (exit {code}, restarts {restarts})"),
            ),
            evidence: Some(evidence.clone()),
        };
        return (true, summary, Some(evidence), vec![f]);
    }
    (false, summary, Some(evidence), Vec::new())
}

/// `inspect_ports` 的探针命令。
///
/// **没有任何模型给的字符串进这条命令**（端口过滤在本地做）：最坏情况也只是
/// 「多列了几个端口」，而不会变成远端命令注入。
///
/// `ss -H -tlnp` 的 `-p` 在非 root 下只能看到自己的进程，这是内核的限制 ——
/// 拿不到进程名时解析结果会是 `None`，而不是编一个出来。
pub fn ports_probe_command() -> String {
    "if ! command -v ss >/dev/null 2>&1; then echo __RS_NOSS__; exit 0; fi; \
     echo __RS_LISTEN__; ss -H -tlnp 2>/dev/null | head -n 400; \
     echo __RS_ESTAB__; ss -H -tn state established 2>/dev/null | awk '{print $4}' \
     | sed 's/.*://' | sort | uniq -c | sort -rn | head -n 20"
        .to_string()
}

/// 一个监听套接字。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListenSocket {
    /// 本地地址（`0.0.0.0:80`）
    pub local: String,
    pub port: u16,
    /// 占用它的进程名（非 root 时可能看不到别人的进程）
    pub process: Option<String>,
    /// 原始行（用户点开明细要能看到完整信息）
    pub raw: String,
}

/// `inspect_ports` / `find_process` 的解析结果。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PortsProbe {
    pub listen: Vec<ListenSocket>,
    /// 连接数最多的本地端口（`(端口, 连接数)`，降序）
    pub estab: Vec<(u16, u32)>,
}

/// 解析监听套接字行。
///
/// 认「第 4 列是地址」而不是「找形如 host:port 的列」：`ss -H -tln` 的列顺序
/// 固定（State Recv-Q Send-Q Local Peer [Process]），而按形状找会把进程串里的
/// `pid=123` 也当成地址。
pub fn parse_listen_lines(lines: impl Iterator<Item = String>) -> Vec<ListenSocket> {
    let mut out = Vec::new();
    for line in lines {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 4 {
            continue;
        }
        let local = cols[3];
        let Some(port) = local.rsplit(':').next().and_then(|p| p.parse::<u16>().ok()) else {
            continue;
        };
        out.push(ListenSocket {
            local: local.to_string(),
            port,
            process: first_process_name(&line),
            raw: line.clone(),
        });
    }
    out
}

/// 从 `ss` 行里取第一个进程名（`users:(("nginx",pid=1,fd=3))` → `nginx`）。
fn first_process_name(line: &str) -> Option<String> {
    let rest = line.split("users:((").nth(1)?;
    let name = rest.split('"').nth(1)?.trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

/// 解析 `inspect_ports` 探针输出。
pub fn parse_ports_probe(out: &CommandOutput) -> Result<PortsProbe, String> {
    if out.timed_out {
        return Err(crate::i18n::ts("查询超时".into(), "query timed out".into()));
    }
    if out.stdout.contains("__RS_NOSS__") {
        return Err(crate::i18n::ts(
            "该主机没有 ss（iproute2）；可改用 run_command 跑 netstat".into(),
            "no ss (iproute2) on this host; use run_command with netstat instead".into(),
        ));
    }
    if !out.stdout.contains("__RS_LISTEN__") {
        return Err(match out.failure() {
            Some(r) => crate::i18n::ts(
                format!("查询端口失败：{r}"),
                format!("could not list ports: {r}"),
            ),
            None => crate::i18n::ts("无法解析端口清单".into(), "unparsable port list".into()),
        });
    }
    let listen = parse_listen_lines(
        out.stdout
            .lines()
            .skip_while(|l| !l.trim().starts_with("__RS_LISTEN__"))
            .skip(1)
            .take_while(|l| !l.trim().starts_with("__RS_ESTAB__"))
            .map(str::to_string),
    );
    let estab = parse_estab_lines(
        out.stdout
            .lines()
            .skip_while(|l| !l.trim().starts_with("__RS_ESTAB__"))
            .skip(1),
    );
    Ok(PortsProbe { listen, estab })
}

/// 解析「连接数 端口」两列（`awk | sort | uniq -c` 的产物）。
fn parse_estab_lines<'a>(lines: impl Iterator<Item = &'a str>) -> Vec<(u16, u32)> {
    let mut out = Vec::new();
    for l in lines {
        let mut it = l.split_whitespace();
        let (Some(n), Some(p)) = (it.next(), it.next()) else {
            continue;
        };
        let (Ok(n), Ok(p)) = (n.parse::<u32>(), p.parse::<u16>()) else {
            continue;
        };
        out.push((p, n));
    }
    out
}

/// 端口清单的结论。
///
/// 指名 `port` 时「没在监听」算一项异常：那是**用户点名的端口**，答案是「不在」
/// 时值得在轨迹上标出来（`check_reachability` 探到不可达也是走 finding 的）。
/// 不给 `port` 时只报事实，不发明阈值。
pub fn judge_ports(
    probe: Result<PortsProbe, String>,
    port: Option<u16>,
) -> (bool, String, Option<String>, Vec<Finding>) {
    use crate::i18n::ts;
    let probe = match probe {
        Ok(p) => p,
        Err(e) => return inspection_failed_for("builtin.inspect_failed", &e, "ports:query-failed"),
    };
    let mut evidence = String::new();
    for s in &probe.listen {
        evidence.push_str(&s.raw);
        evidence.push('\n');
    }
    for (p, n) in &probe.estab {
        evidence.push_str(&format!("{n} {p}\n"));
    }
    let evidence = Some(evidence).filter(|e| !e.trim().is_empty());

    match port {
        Some(want) => {
            let conns = probe
                .estab
                .iter()
                .find(|(p, _)| *p == want)
                .map(|(_, n)| *n)
                .unwrap_or(0);
            match probe.listen.iter().find(|s| s.port == want) {
                Some(s) => {
                    let who = s
                        .process
                        .clone()
                        .unwrap_or_else(|| ts("进程未知".into(), "process unknown".into()));
                    (
                        false,
                        ts(
                            format!("端口 {want} 正在监听（{who}，连接 {conns} 条）"),
                            format!("port {want} is listening ({who}, {conns} connections)"),
                        ),
                        evidence,
                        Vec::new(),
                    )
                }
                None => {
                    let f = Finding {
                        kind: FindingKind::Anomaly,
                        profile_id: String::new(),
                        profile_name: String::new(),
                        rule_id: "builtin.port".into(),
                        fingerprint: "port:not-listening".into(),
                        severity: Severity::Warning,
                        message: ts(
                            format!("{want} 端口没有监听"),
                            format!("nothing is listening on port {want}"),
                        ),
                        evidence: None,
                    };
                    (
                        true,
                        ts(
                            format!("{want} 未在监听"),
                            format!("port {want} is not listening"),
                        ),
                        evidence,
                        vec![f],
                    )
                }
            }
        }
        None => {
            let mut summary = ts(
                format!("{} 个监听端口", probe.listen.len()),
                format!("{} listening ports", probe.listen.len()),
            );
            if let Some((p, n)) = probe.estab.first() {
                summary.push_str(&ts(
                    format!(" · 连接最多：{p}（{n} 条）"),
                    format!(" · busiest: {p} ({n} connections)"),
                ));
            }
            (false, summary, evidence, Vec::new())
        }
    }
}

/// `check_certificate` 的探针命令。
///
/// 证书检查必须**在服务器上做**（`openssl s_client` 连 `127.0.0.1:port`）：
/// 本机没有 openssl，而且很多站点的证书只在服务器自己的网络里可见。
/// 主机名 / 端口 / SNI 都过 [`crate::tasks::shell_quote`]。
pub fn cert_probe_command(host: &str, port: u16, server_name: Option<&str>) -> String {
    let addr = crate::tasks::shell_quote(&format!("{host}:{port}"));
    let sni = match server_name {
        Some(s) => format!(" -servername {}", crate::tasks::shell_quote(s)),
        None => String::new(),
    };
    format!(
        "if ! command -v openssl >/dev/null 2>&1; then echo __RS_NOOPENSSL__; exit 0; fi; \
         echo | openssl s_client -connect {addr}{sni} 2>/dev/null \
         | openssl x509 -noout -subject -issuer -dates -fingerprint -sha256 2>/dev/null; \
         echo \"__RS_RC__$?\""
    )
}

/// 证书信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertInfo {
    pub subject: String,
    pub issuer: String,
    pub not_after: String,
    pub fingerprint: Option<String>,
    /// 距到期天数（负数 = 已过期）
    pub days_left: i64,
}

/// 解析证书输出。
///
/// `now` 由调用方给：测试要能钉住「还有几天」，而不是让用例依赖运行时的钟。
pub fn parse_cert(
    out: &CommandOutput,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<CertInfo, String> {
    if out.timed_out {
        return Err(crate::i18n::ts(
            "证书检查超时".into(),
            "certificate check timed out".into(),
        ));
    }
    if out.stdout.contains("__RS_NOOPENSSL__") {
        return Err(crate::i18n::ts(
            "该主机没有 openssl，无法检查证书".into(),
            "no openssl on this host; cannot check the certificate".into(),
        ));
    }
    let mut subject = String::new();
    let mut issuer = String::new();
    let mut not_after = String::new();
    let mut fingerprint = None;
    for line in out.stdout.lines() {
        let line = line.trim();
        if let Some(v) = line.strip_prefix("subject=") {
            subject = v.trim().to_string();
        } else if let Some(v) = line.strip_prefix("issuer=") {
            issuer = v.trim().to_string();
        } else if let Some(v) = line.strip_prefix("notAfter=") {
            not_after = v.trim().to_string();
        } else if let Some(v) = line.split_once("Fingerprint=").map(|(_, v)| v) {
            fingerprint = Some(v.trim().to_string());
        }
    }
    if not_after.is_empty() {
        return Err(match out.failure() {
            Some(r) => crate::i18n::ts(
                format!("无法获取证书（端口未监听或不是 TLS）：{r}"),
                format!("could not fetch a certificate (port closed or not TLS): {r}"),
            ),
            None => crate::i18n::ts(
                "无法获取证书（端口未监听或不是 TLS）".into(),
                "could not fetch a certificate (port closed or not TLS)".into(),
            ),
        });
    }
    let expiry = parse_openssl_time(&not_after).ok_or_else(|| {
        crate::i18n::ts(
            format!("无法解析证书到期时间：{not_after}"),
            format!("unparsable certificate expiry: {not_after}"),
        )
    })?;
    let days_left = (expiry - now.naive_utc()).num_days();
    Ok(CertInfo {
        subject,
        issuer,
        not_after,
        fingerprint,
        days_left,
    })
}

/// `openssl x509 -dates` 的时间格式：`Mar 15 12:00:00 2027 GMT`。
fn parse_openssl_time(s: &str) -> Option<chrono::NaiveDateTime> {
    let s = s.trim();
    ["%b %e %H:%M:%S %Y GMT", "%b %d %H:%M:%S %Y GMT"]
        .iter()
        .find_map(|f| chrono::NaiveDateTime::parse_from_str(s, f).ok())
}

/// 证书到期的结论。
pub fn judge_cert(
    probe: Result<CertInfo, String>,
    host: &str,
    port: u16,
) -> (bool, String, Option<String>, Vec<Finding>) {
    use crate::i18n::ts;
    let info = match probe {
        Ok(i) => i,
        Err(e) => {
            return inspection_failed_for(
                "builtin.inspect_failed",
                &ts(
                    format!("{host}:{port} 证书检查失败：{e}"),
                    format!("certificate check on {host}:{port} failed: {e}"),
                ),
                "cert:query-failed",
            )
        }
    };
    let subject = if info.subject.is_empty() {
        ts("(主体未知)".into(), "(unknown subject)".into())
    } else {
        info.subject.clone()
    };
    let mut evidence = format!(
        "subject={}\nissuer={}\nnotAfter={}\n",
        info.subject, info.issuer, info.not_after
    );
    if let Some(fp) = &info.fingerprint {
        evidence.push_str(&format!("SHA256 Fingerprint={fp}\n"));
    }
    let days = info.days_left;
    let (sev, msg) = if days < 0 {
        (
            Some(Severity::Critical),
            ts(
                format!("证书已过期 {} 天", -days),
                format!("certificate expired {} day(s) ago", -days),
            ),
        )
    } else if days <= CERT_CRIT_DAYS {
        (
            Some(Severity::Critical),
            ts(
                format!("证书 {days} 天后到期"),
                format!("certificate expires in {days} day(s)"),
            ),
        )
    } else if days <= CERT_WARN_DAYS {
        (
            Some(Severity::Warning),
            ts(
                format!("证书 {days} 天后到期"),
                format!("certificate expires in {days} day(s)"),
            ),
        )
    } else {
        (None, String::new())
    };
    let summary = ts(
        format!("{host}:{port} 证书 {days} 天后到期（{subject}）"),
        format!("{host}:{port} certificate expires in {days} day(s) ({subject})"),
    );
    match sev {
        Some(sev) => {
            let f = Finding {
                kind: FindingKind::Anomaly,
                profile_id: String::new(),
                profile_name: String::new(),
                rule_id: "builtin.cert".into(),
                fingerprint: if days < 0 {
                    "cert:expired".into()
                } else {
                    "cert:expiring".into()
                },
                severity: sev,
                message: msg,
                evidence: Some(evidence.clone()),
            };
            (true, summary, Some(evidence), vec![f])
        }
        None => (false, summary, Some(evidence), Vec::new()),
    }
}

/// 把进程名转成「字面量」ERE：`pgrep` 没有 `-F`，`|`、`*` 这类元字符会改变
/// 匹配范围。逐个转义比白名单宽容（用户可能真要找 `a.out` 里的那个点）。
pub fn escape_ere(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(
            c,
            '\\' | '.' | '+' | '*' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '^' | '$' | '|'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// `find_process` 的探针命令。
///
/// 两条分支各自检查工具是否存在：`pgrep` / `ss` 缺失时**明确报出来**，
/// 因为静默返回空会被读成「没有这个进程」，而那是两码事。
pub fn process_probe_command(name: Option<&str>, port: Option<u16>) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(n) = name {
        let pat = crate::tasks::shell_quote(&escape_ere(n));
        parts.push(format!(
            "if ! command -v pgrep >/dev/null 2>&1; then echo __RS_NOPGREP__; \
             else ps -o pid,ppid,pcpu,pmem,etime,stat,args \
             -p $(pgrep -- {pat} 2>/dev/null | tr '\\n' ' ') 2>/dev/null; fi"
        ));
    }
    if port.is_some() {
        parts.push(
            "if ! command -v ss >/dev/null 2>&1; then echo __RS_NOSS__; \
             else ss -H -tlnp 2>/dev/null | head -n 400; fi"
                .to_string(),
        );
    }
    format!("echo __RS_BEGIN__; {}", parts.join("; echo __RS_SPLIT__; "))
}

/// `find_process` 的解析结果。
///
/// 两个字段都是 `Option`：`Some` = 查了（可能结果为空 = 没匹配），
/// `None` = 这一段没查成（工具缺失）——「查不了」与「没有」必须分开。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProcessProbe {
    pub procs: Option<Vec<String>>,
    pub listeners: Option<Vec<ListenSocket>>,
    /// 工具缺失的说明（pgrep / ss）
    pub missing_tools: Vec<String>,
}

/// 解析 `find_process` 探针输出。
///
/// `want_name` / `want_port` 决定第几段是什么 —— 探针按这两个开关拼段，
/// 解析必须用同一份开关，**不能靠「段里像什么」来猜**。
pub fn parse_process_probe(
    out: &CommandOutput,
    want_name: bool,
    want_port: bool,
) -> Result<ProcessProbe, String> {
    if out.timed_out {
        return Err(crate::i18n::ts("查询超时".into(), "query timed out".into()));
    }
    let Some(body) = out.stdout.split("__RS_BEGIN__").nth(1) else {
        return Err(match out.failure() {
            Some(r) => crate::i18n::ts(
                format!("查询进程失败：{r}"),
                format!("could not look up the process: {r}"),
            ),
            None => crate::i18n::ts(
                "无法解析进程查询结果".into(),
                "unparsable process result".into(),
            ),
        });
    };
    let sections: Vec<&str> = body.split("__RS_SPLIT__").collect();
    let mut probe = ProcessProbe::default();
    let mut it = sections.iter();
    if want_name {
        let sec = it.next().copied().unwrap_or("");
        if sec.contains("__RS_NOPGREP__") {
            probe.missing_tools.push(crate::i18n::ts(
                "pgrep 不存在".into(),
                "pgrep not found".into(),
            ));
        } else {
            probe.procs = Some(
                sec.lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .filter(|l| !l.starts_with("PID"))
                    .map(crate::ai::redact)
                    .collect(),
            );
        }
    }
    if want_port {
        let sec = it.next().copied().unwrap_or("");
        if sec.contains("__RS_NOSS__") {
            probe
                .missing_tools
                .push(crate::i18n::ts("ss 不存在".into(), "ss not found".into()));
        } else {
            probe.listeners = Some(parse_listen_lines(sec.lines().map(str::to_string)));
        }
    }
    Ok(probe)
}

/// 进程查询的结论（只读信息类：不发明阈值，只报事实）。
pub fn judge_process(
    probe: Result<ProcessProbe, String>,
    name: Option<&str>,
    port: Option<u16>,
) -> (bool, String, Option<String>, Vec<Finding>) {
    use crate::i18n::ts;
    let probe = match probe {
        Ok(p) => p,
        Err(e) => {
            return inspection_failed_for("builtin.inspect_failed", &e, "process:query-failed")
        }
    };
    // 两段都没查成 = 这次调用什么也没查到：巡检失败，别让模型以为「没有」
    if probe.procs.is_none() && probe.listeners.is_none() {
        let msg = ts(
            format!("无法查询进程：{}", probe.missing_tools.join("、")),
            format!(
                "cannot look up processes: {}",
                probe.missing_tools.join(", ")
            ),
        );
        return inspection_failed_for("builtin.inspect_failed", &msg, "process:no-tool");
    }

    let mut parts: Vec<String> = Vec::new();
    let mut evidence = String::new();
    let mut findings: Vec<Finding> = Vec::new();

    if let Some(want) = name {
        match &probe.procs {
            Some(rows) if rows.is_empty() => {
                parts.push(ts(
                    format!("没有名为 {want} 的进程"),
                    format!("no process named {want}"),
                ));
            }
            Some(rows) => {
                parts.push(ts(
                    format!("{want}：{} 个匹配", rows.len()),
                    format!("{want}: {} match(es)", rows.len()),
                ));
                for r in rows {
                    evidence.push_str(r);
                    evidence.push('\n');
                }
            }
            // 工具缺失（上面已保证不是「两段都没查成」，所以这里只可能是单边缺失）
            None => {
                let msg = ts(
                    format!("pgrep 不存在，无法按名查询 {want}"),
                    format!("pgrep not found; cannot search for {want}"),
                );
                findings.push(Finding {
                    kind: FindingKind::InspectionFailed,
                    profile_id: String::new(),
                    profile_name: String::new(),
                    rule_id: "builtin.inspect_failed".into(),
                    fingerprint: "process:no-pgrep".into(),
                    severity: Severity::Warning,
                    message: msg.clone(),
                    evidence: None,
                });
                parts.push(msg);
            }
        }
    }
    if let Some(want) = port {
        match &probe.listeners {
            Some(socks) => match socks.iter().find(|s| s.port == want) {
                Some(s) => {
                    let who = s
                        .process
                        .clone()
                        .unwrap_or_else(|| ts("进程未知".into(), "process unknown".into()));
                    parts.push(ts(
                        format!("端口 {want} 由 {who} 监听"),
                        format!("port {want} is held by {who}"),
                    ));
                    evidence.push_str(&s.raw);
                    evidence.push('\n');
                }
                None => parts.push(ts(
                    format!("没有进程监听 {want}"),
                    format!("nothing is listening on {want}"),
                )),
            },
            None => {
                let msg = ts(
                    format!("ss 不存在，无法按端口 {want} 查询"),
                    format!("ss not found; cannot search by port {want}"),
                );
                findings.push(Finding {
                    kind: FindingKind::InspectionFailed,
                    profile_id: String::new(),
                    profile_name: String::new(),
                    rule_id: "builtin.inspect_failed".into(),
                    fingerprint: "process:no-ss".into(),
                    severity: Severity::Warning,
                    message: msg.clone(),
                    evidence: None,
                });
                parts.push(msg);
            }
        }
    }
    let evidence = Some(evidence).filter(|e| !e.trim().is_empty());
    (false, join_summary(&parts), evidence, findings)
}

/// 构造一条「巡检失败」的结论（`anomaly = false`）。
fn inspection_failed_for(
    rule_id: &str,
    message: &str,
    fingerprint: &str,
) -> (bool, String, Option<String>, Vec<Finding>) {
    (
        false,
        message.to_string(),
        None,
        vec![Finding {
            kind: FindingKind::InspectionFailed,
            profile_id: String::new(),
            profile_name: String::new(),
            rule_id: rule_id.to_string(),
            fingerprint: fingerprint.to_string(),
            severity: Severity::Warning,
            message: message.to_string(),
            evidence: None,
        }],
    )
}

/// 判定退出码异常（`run_command` 巡检用）。
pub fn judge_exit(out: &CommandOutput, watched: &[u32]) -> (bool, String, Vec<Finding>) {
    let Some(code) = out.exit_code else {
        if out.timed_out {
            let (_, msg, _, f) = inspection_failed_for(
                "builtin.inspect_failed",
                &crate::i18n::ts("命令超时".into(), "command timed out".into()),
                "probe:timeout",
            );
            return (false, msg, f);
        }
        return (false, String::new(), Vec::new());
    };
    let watched_hit = if watched.is_empty() {
        code != 0
    } else {
        watched.contains(&code)
    };
    if !watched_hit {
        return (
            false,
            crate::i18n::ts(format!("退出码 {code}"), format!("exit {code}")),
            Vec::new(),
        );
    }
    (
        true,
        crate::i18n::ts(format!("退出码 {code}"), format!("exit {code}")),
        vec![Finding {
            kind: FindingKind::Anomaly,
            profile_id: String::new(),
            profile_name: String::new(),
            rule_id: "builtin.inspect_failed".into(),
            fingerprint: exit_fingerprint(code),
            severity: Severity::Warning,
            message: crate::i18n::ts(
                format!("命令以非预期退出码结束：{code}"),
                format!("command exited with unexpected code {code}"),
            ),
            evidence: None,
        }],
    )
}

/// 把 finding 的目标信息补齐（判定函数不知道自己在给谁判）。
pub fn bind_findings(findings: &mut [Finding], profile_id: &str, profile_name: &str) {
    for f in findings.iter_mut() {
        f.profile_id = profile_id.to_string();
        f.profile_name = profile_name.to_string();
    }
}

/// 拼接一台机器的可读摘要。
pub fn join_summary(parts: &[String]) -> String {
    if parts.is_empty() {
        crate::i18n::ts("巡检完成，无异常".into(), "inspection ok".into())
    } else {
        parts.join(" · ")
    }
}

/// 轨迹行的「明细」文本：**先列异常，再附原始输出**（v1.1 §3.9「点开可见」）。
///
/// 为什么异常必须排在原始输出**前面**：原始输出往往是每台几十行，用户点开折叠块
/// 的第一眼应该看到「哪台机器、什么毛病」，而不是从 `cpu 12 0 0 ...` 里自己找。
/// 界面上那一行只写「发现 11 项异常」，明细是唯一能看到**是哪 11 项**的地方 ——
/// 所以这里不能只塞原始输出，更不能（曾经）塞空串。
///
/// 返回的文本可能很长，由 [`crate::agent::truncate_detail`] 在渲染前截断。
pub fn trace_detail(findings: &[Finding], raw: &str) -> String {
    let mut out = String::new();
    if !findings.is_empty() {
        out.push_str(&crate::i18n::ts(
            format!("异常明细（{} 项）：", findings.len()),
            format!("Anomalies ({}):", findings.len()),
        ));
        out.push('\n');
        for f in findings {
            // 目标名 + 严重级 + 一句话原因。展示名可能为空（如 `run_command` 的
            // 退出码 finding 由上层 `bind_findings` 补齐；万一没补上就退回 id）。
            let who = if f.profile_name.trim().is_empty() {
                f.profile_id.as_str()
            } else {
                f.profile_name.as_str()
            };
            out.push_str(&format!(
                "· [{}] {} — {}\n",
                f.severity.label(),
                who,
                f.message
            ));
            if let Some(ev) = f
                .evidence
                .as_deref()
                .map(str::trim)
                .filter(|e| !e.is_empty())
            {
                // 证据按行缩进，避免与下一条 finding 的 `·` 混淆
                for line in ev.lines().take(5) {
                    out.push_str("    ");
                    out.push_str(line);
                    out.push('\n');
                }
            }
        }
    }
    let raw = raw.trim();
    if !raw.is_empty() {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&crate::i18n::ts("原始输出：".into(), "Raw output:".into()));
        out.push('\n');
        out.push_str(raw);
    }
    out
}

// 结果分层（v1.1 §2.2.3）

/// 单台机器的**完整**结构化结果。
///
/// 与「回注给模型的文本」严格分开，因为两者的预算根本不是一回事：
/// 模型上下文只有 8 KiB（多了会挤掉别的内容、还更贵），而本机存一份原始输出
/// 几乎不花钱。把两者混成一个字段，结果必然是「为省上下文而丢掉本地证据」
/// 或「为留证据而把上下文撑爆」。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MachineResult {
    pub target_id: String,
    pub target_name: String,
    /// 探针命令本身的输出（已脱敏，**未**按模型预算裁剪）
    #[serde(default)]
    pub raw: Option<String>,
    /// 结论摘要（与 `TargetOutcome::summary` 同源）
    pub summary: String,
    pub elapsed_ms: u64,
}

impl MachineResult {
    pub fn new(target_id: &str, target_name: &str, summary: &str, raw: Option<String>) -> Self {
        Self {
            target_id: target_id.to_string(),
            target_name: target_name.to_string(),
            raw,
            summary: summary.to_string(),
            elapsed_ms: 0,
        }
    }
}

/// 回注给模型的文本（v1.1 §2.2.3）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResultForModel {
    pub summary: String,
    pub evidence: Option<String>,
    pub truncated: bool,
}

impl ToolResultForModel {
    /// 拼成一段文本（`AgentSession` 还会再做一次 8 KiB 兜底截断）。
    pub fn render(&self) -> String {
        match &self.evidence {
            Some(e) if !e.trim().is_empty() => format!("{}\n{}", self.summary, e),
            _ => self.summary.clone(),
        }
    }
}

/// 聚合后**逐台展开**的目标数上限。
///
/// 超出部分不进模型上下文，而是折叠成一行「其余 N 台见运行记录」——
/// 见 [`aggregate_for_model`] 的说明。
pub const MODEL_DETAIL_TARGETS: usize = 10;

/// 单次回注给模型的明细文本上限（与 `agent::MAX_TOOL_RESULT_BYTES` 同量级）。
pub const MAX_MODEL_TEXT_BYTES: usize = 8 * 1024;

/// 单台明细摊到的最低字节数：台数多到把预算摊没时，兜底保证「看得见开头结尾」。
pub const MIN_MODEL_HOST_BYTES: usize = 256;

/// 每台的固定开销（`== 名称 (id) ==` 与摘要两行）——先扣掉再摊明细预算，
/// 否则台数一多，这些标题行自己就会把总量顶过 [`MAX_MODEL_TEXT_BYTES`]。
const HOST_OVERHEAD_BYTES: usize = 128;

/// 正常机器的明细策略（见 [`aggregate_for_model`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OkDetail {
    /// 正常机器只给一行名单。巡检类工具用：结论落在异常项上，
    /// 正常机器的原始输出只是噪声（50 台各 4 KiB 就是 200 KiB）。
    NamesOnly,
    /// 正常机器也逐台展开「退出码 + stdout/stderr」。`run_command` 用：
    /// 模型的结论就建立在这些输出上 —— 只回一句「N 台正常」等于什么都没给，
    /// 用户问「监听端口有哪些」时模型只能凭空编。
    Full,
}

/// 把逐台报告聚合成「回注给模型」的文本。
///
/// **为什么不直接把每台的完整输出拼起来**：50 台各 4 KiB 就是 200 KiB，远超上下文
/// 预算；一旦超了，现在的兜底做法是首尾截断 —— 中间那 40 多台（恰恰可能包含真正的
/// 异常）会被**静默丢掉**。用户看到的是「AI 说一切正常」，可它根本没看到问题机器。
///
/// 聚合规则：
/// 1. 异常与巡检失败的机器排在最前并**逐台详列**；
/// 2. 正常机器按 `ok_detail` 处理：[`OkDetail::NamesOnly`] 折成一行名单，
///    [`OkDetail::Full`] 则与异常机器一样逐台展开（上限同样是
///    [`MODEL_DETAIL_TARGETS`] 台，超出部分并入「其余 N 台」）；
/// 3. 每台明细按 [`MAX_MODEL_TEXT_BYTES`] 摊分预算后首尾截断，
///    免得头一台的输出吃光整个上下文；
/// 4. 超过上限时末尾**明确写出**「其余 N 台见运行记录」，
///    而不是让模型把「没看到」当成「不存在」。
pub fn aggregate_for_model(reports: &[TargetReport], ok_detail: OkDetail) -> ToolResultForModel {
    if reports.is_empty() {
        return ToolResultForModel {
            summary: crate::i18n::ts("没有目标被检查".into(), "no target was checked".into()),
            evidence: None,
            truncated: false,
        };
    }
    let bad: Vec<&TargetReport> = reports.iter().filter(|r| r.anomaly || !r.exec_ok).collect();
    let ok: Vec<&TargetReport> = reports.iter().filter(|r| !r.anomaly && r.exec_ok).collect();

    let total = reports.len();
    let anomaly = reports.iter().filter(|r| r.anomaly).count();
    let failed = reports.iter().filter(|r| !r.exec_ok).count();
    let summary = crate::i18n::ts(
        format!(
            "共 {total} 台：{anomaly} 台异常，{failed} 台巡检失败，{} 台正常",
            ok.len()
        ),
        format!(
            "{total} hosts: {anomaly} anomaly, {failed} failed, {} ok",
            ok.len()
        ),
    );

    let mut lines: Vec<String> = Vec::new();
    let bad_shown = bad.len().min(MODEL_DETAIL_TARGETS);
    let ok_shown = match ok_detail {
        OkDetail::NamesOnly => 0,
        OkDetail::Full => ok.len().min(MODEL_DETAIL_TARGETS - bad_shown),
    };
    let expanded = bad_shown + ok_shown;
    let per_host = MAX_MODEL_TEXT_BYTES
        .saturating_sub(expanded * HOST_OVERHEAD_BYTES)
        .checked_div(expanded)
        .map(|per| per.max(MIN_MODEL_HOST_BYTES))
        .unwrap_or(MAX_MODEL_TEXT_BYTES);

    let host_block = |r: &TargetReport| {
        let mut block = vec![
            format!("== {} ({}) ==", r.profile_name, r.profile_id),
            r.summary.clone(),
        ];
        if let Some(d) = &r.detail {
            block.push(crate::agent::limit_result_with(d, per_host));
        }
        block
    };

    for r in bad.iter().take(bad_shown) {
        lines.extend(host_block(r));
    }
    let hidden_bad = bad.len().saturating_sub(bad_shown);
    if hidden_bad > 0 {
        lines.push(crate::i18n::ts(
            format!("…其余 {hidden_bad} 台异常/失败未展开，详见本地运行记录"),
            format!("…{hidden_bad} more anomalous/failed hosts omitted; see the local run record"),
        ));
    }

    match ok_detail {
        OkDetail::Full => {
            for r in ok.iter().take(ok_shown) {
                lines.extend(host_block(r));
            }
            let hidden_ok = ok.len().saturating_sub(ok_shown);
            if hidden_ok > 0 {
                lines.push(crate::i18n::ts(
                    format!("…其余 {hidden_ok} 台正常机器未展开，详见本地运行记录"),
                    format!("…{hidden_ok} more healthy hosts omitted; see the local run record"),
                ));
            }
        }
        OkDetail::NamesOnly => {
            if !ok.is_empty() {
                let names: Vec<&str> = ok.iter().map(|r| r.profile_name.as_str()).collect();
                lines.push(crate::i18n::ts(
                    format!("正常（{} 台）：{}", names.len(), names.join("、")),
                    format!("OK ({}): {}", names.len(), names.join(", ")),
                ));
            }
        }
    }

    let rendered = lines.join("\n");
    // 模型侧还有一道 8 KiB 硬顶：这里如实报告是否被裁过，
    // 免得模型把「没看到」当成「不存在」。
    let truncated = rendered.len() > MAX_MODEL_TEXT_BYTES;
    ToolResultForModel {
        summary,
        evidence: Some(rendered),
        truncated,
    }
}

/// 把逐台报告转成机器侧完整结果（进运行记录）。
pub fn machine_results(reports: &[TargetReport]) -> Vec<MachineResult> {
    reports
        .iter()
        .map(|r| MachineResult::new(&r.profile_id, &r.profile_name, &r.summary, r.detail.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alerts::HOST_KEY_UNKNOWN_FP;

    /// 轨迹明细必须**先列异常、再附原始输出**，且带上目标名与严重级。
    ///
    /// 这条回归的由来：界面上那一行只写「发现 11 项异常」，明细是唯一能看到
    /// **是哪 11 项**的地方。第一版接线时 `detail` 传的是空串，于是用户点开折叠块
    /// 什么都没有 —— 而界面上一切正常。
    #[test]
    fn trace_detail_lists_anomalies_before_raw_output() {
        let f = |name: &str, sev: Severity, msg: &str, ev: Option<&str>| Finding {
            kind: FindingKind::Anomaly,
            profile_id: format!("p-{name}"),
            profile_name: name.to_string(),
            rule_id: "builtin.mem".into(),
            fingerprint: "mem>90".into(),
            severity: sev,
            message: msg.to_string(),
            evidence: ev.map(str::to_string),
        };
        let findings = vec![
            f(
                "prod-01",
                Severity::Critical,
                "内存 94%（9.4G/10G）",
                Some("MemTotal: 10G"),
            ),
            f("prod-02", Severity::Warning, "CPU 88%", None),
        ];
        let raw = "host prod-01\ncpu 12 0 0 88\n";
        let d = trace_detail(&findings, raw);

        // 异常在前
        let head = d.find("内存 94%").expect("异常必须出现在明细里");
        let tail = d.find("原始输出").expect("原始输出也要保留");
        assert!(head < tail, "异常必须排在原始输出之前：\n{d}");
        // 目标名与严重级都要在（否则用户不知道是哪台、多严重）
        assert!(d.contains("prod-01") && d.contains("prod-02"), "{d}");
        assert!(d.contains(Severity::Critical.label()), "{d}");
        assert!(d.contains(Severity::Warning.label()), "{d}");
        assert!(d.contains("2 项"), "要报出条数：{d}");
        // 证据缩进在对应 finding 下面
        assert!(d.contains("MemTotal: 10G"), "{d}");
        // 原始输出仍在（用户可能要看原始行）
        assert!(d.contains("cpu 12 0 0 88"), "{d}");
    }

    /// 没有异常时明细只给原始输出；两者都没有时是空串（界面据此不渲染折叠块）。
    #[test]
    fn trace_detail_degrades_cleanly() {
        let d = trace_detail(&[], "cpu 1 2 3");
        assert!(!d.contains("异常明细"), "没有异常就不该有异常段：{d}");
        assert!(d.contains("cpu 1 2 3"), "{d}");
        assert_eq!(trace_detail(&[], "   "), "");
        assert_eq!(trace_detail(&[], ""), "");
        // 只有异常、没有原始输出也成立
        let one = vec![Finding {
            kind: FindingKind::Anomaly,
            profile_id: "p".into(),
            profile_name: String::new(),
            rule_id: "r".into(),
            fingerprint: "f".into(),
            severity: Severity::Info,
            message: "提示级".into(),
            evidence: None,
        }];
        let d = trace_detail(&one, "");
        assert!(d.contains("提示级"), "{d}");
        // 目标名为空时退回 id，不留一个空的「[] —」
        assert!(d.contains('p'), "目标名为空时要退回 id：{d}");
    }

    fn thresholds() -> LoadThresholds {
        LoadThresholds::default()
    }

    fn sample(cpu: f32, mem: f32, swap: f32) -> Sample {
        Sample {
            cpu_percent: cpu,
            mem_percent: mem,
            swap_percent: swap,
            mem_used: 9_400_000_000,
            mem_total: 10_000_000_000,
            swap_used: 0,
            swap_total: 0,
            net_up: 0.0,
            net_down: 0.0,
            disk_read_mb: 0.0,
            disk_write_mb: 0.0,
            timestamp: 0,
        }
    }

    fn disk(mount: &str, used: u64, total: u64) -> DiskInfo {
        DiskInfo {
            name: "sda1".into(),
            mount: mount.into(),
            total,
            used,
            avail: total - used,
            df: None,
        }
    }

    const GIB: u64 = 1024 * 1024 * 1024;

    #[test]
    fn command_output_classifies_outcomes() {
        let ok = CommandOutput::ok("hi");
        assert!(ok.succeeded());
        assert!(ok.failure().is_none());

        let bad = CommandOutput {
            exit_code: Some(1),
            ..ok.clone()
        };
        assert!(!bad.succeeded());
        assert!(bad.failure().is_some());

        let to = CommandOutput {
            exit_code: None,
            timed_out: true,
            ..ok.clone()
        };
        assert!(!to.succeeded());
        assert!(to.failure().is_some());
    }

    #[test]
    fn host_key_failures_get_their_own_fingerprints() {
        // 未知主机密钥 -> 专属指纹（命中 Warning 的 host_key_unknown 规则）
        let f = inspection_failure_finding(
            "p1",
            "prod-01",
            &format!("{}：去手动信任一次", crate::ssh::HOST_KEY_UNKNOWN_MARK),
            false,
        );
        assert_eq!(f.fingerprint, HOST_KEY_UNKNOWN_FP);
        assert_eq!(f.severity, Severity::Warning);

        // 密钥不一致 -> Critical（疑似中间人）
        let f = inspection_failure_finding("p1", "prod-01", "主机密钥与记录不一致", false);
        assert_eq!(f.fingerprint, HOST_KEY_FP);
        assert_eq!(
            f.severity,
            Severity::Critical,
            "密钥不一致必须按 Critical 报，否则会被普通连接失败淹没"
        );

        // 普通连接失败
        let f = inspection_failure_finding("p1", "prod-01", "SSH 连接超时", false);
        assert_eq!(f.fingerprint, "connect:failed");
        assert_eq!(f.kind, FindingKind::InspectionFailed);
    }

    #[test]
    fn target_report_failure_is_not_an_anomaly() {
        let r = TargetReport::failure("p1", "prod-01", false, "连接超时".into());
        assert!(!r.anomaly, "巡检失败不是「服务器异常」");
        assert!(!r.exec_ok);
        assert!(!r.connected);
        assert_eq!(r.findings.len(), 1);
        assert_eq!(r.findings[0].kind, FindingKind::InspectionFailed);
        // 转成运行记录后仍然保持这个区分
        let o = r.to_outcome();
        assert!(!o.anomaly && !o.exec_ok);
    }

    #[test]
    fn load_within_thresholds_produces_no_findings() {
        let m = LoadMetrics {
            sample: Some(sample(10.0, 20.0, 0.0)),
            overview: Some(SystemOverview {
                disks: vec![disk("/", 10 * GIB, 100 * GIB)],
                ..Default::default()
            }),
            load_per_core: Some(0.3),
            cores: Some(4),
        };
        let (f, lines) = judge_load(&m, &thresholds(), &[]);
        assert!(f.is_empty(), "不该报异常：{f:?}");
        assert!(lines.iter().any(|l| l.contains("CPU")));
        assert!(
            lines.iter().any(|l| l.contains("/")),
            "磁盘行要出现：{lines:?}"
        );
    }

    #[test]
    fn load_over_thresholds_reports_with_severity() {
        let m = LoadMetrics {
            sample: Some(sample(95.0, 94.0, 0.0)),
            overview: Some(SystemOverview {
                disks: vec![disk("/", 95 * GIB, 100 * GIB)],
                ..Default::default()
            }),
            load_per_core: Some(3.0),
            cores: Some(4),
        };
        let (f, _) = judge_load(&m, &thresholds(), &[]);
        let fps: Vec<&str> = f.iter().map(|x| x.fingerprint.as_str()).collect();
        assert!(fps.iter().any(|s| s.contains("cpu")), "{fps:?}");
        assert!(fps.iter().any(|s| s.contains("mem")), "{fps:?}");
        assert!(fps.iter().any(|s| s.contains("disk")), "{fps:?}");
        assert!(fps.iter().any(|s| s.contains("load")), "{fps:?}");
        // 内存与磁盘是 Critical
        let mem = f.iter().find(|x| x.fingerprint.contains("mem")).unwrap();
        assert_eq!(mem.severity, Severity::Critical);
    }

    #[test]
    fn checks_filter_which_metrics_are_reported() {
        let m = LoadMetrics {
            sample: Some(sample(95.0, 94.0, 0.0)),
            ..Default::default()
        };
        let (f, lines) = judge_load(&m, &thresholds(), &[LoadCheck::Cpu]);
        assert_eq!(f.len(), 1, "只勾了 CPU 就只报 CPU：{f:?}");
        assert!(f[0].fingerprint.contains("cpu"));
        assert!(lines.iter().all(|l| !l.contains("内存")), "{lines:?}");
    }

    #[test]
    fn small_partitions_do_not_trigger_disk_alerts() {
        // /boot 常年高位，报它只会淹没真正的磁盘快满
        let m = LoadMetrics {
            overview: Some(SystemOverview {
                disks: vec![disk("/boot", 480 * 1024 * 1024, 512 * 1024 * 1024)],
                ..Default::default()
            }),
            ..Default::default()
        };
        let (f, _) = judge_load(&m, &thresholds(), &[]);
        assert!(f.is_empty(), "/boot 不该触发磁盘告警：{f:?}");
    }

    #[test]
    fn swap_is_skipped_when_not_enabled() {
        // swap_total = 0（未启用交换）：不该因为 swap_percent 报了而告警
        let m = LoadMetrics {
            sample: Some(sample(1.0, 1.0, 99.0)),
            ..Default::default()
        };
        let (f, lines) = judge_load(&m, &thresholds(), &[]);
        assert!(f.is_empty(), "{f:?}");
        assert!(!lines.iter().any(|l| l.contains("Swap")), "{lines:?}");
    }

    #[test]
    fn custom_thresholds_are_honoured() {
        let m = LoadMetrics {
            sample: Some(sample(50.0, 50.0, 0.0)),
            ..Default::default()
        };
        let strict = LoadThresholds {
            cpu_percent: 40.0,
            mem_percent: 40.0,
            ..thresholds()
        };
        let (f, _) = judge_load(&m, &strict, &[]);
        assert_eq!(f.len(), 2, "更严的阈值应当报出来：{f:?}");
    }

    #[test]
    fn probe_command_quotes_paths_and_patterns() {
        let cmd = log_probe_command("/var/log/app.log", "error|warn", 500, 5);
        assert!(cmd.contains("'/var/log/app.log'"));
        assert!(cmd.contains("__RS_MISSING__"));
        assert!(cmd.contains("__RS_NOPERM__"));
        assert!(cmd.contains("__RS_HITS__"));
        assert!(cmd.contains("tail -n 500"));
        // 注入尝试：每个单引号都必须被 `'\''` 转义。
        // 判据是**转义后的结果**本身，而不是「输出里不含危险子串」——
        // 那段载荷本来就会出现在转义后的字符串里，只是被包在引号内当字面量。
        let evil = log_probe_command("/tmp/a'; rm -rf / #", "x", 10, 1);
        assert!(evil.contains("'\\''"), "单引号必须被转义：{evil}");
        assert!(
            evil.contains("'/tmp/a'\\''; rm -rf / #'"),
            "转义结果应是「闭合引号 + 转义的单引号 + 重开引号」：{evil}"
        );
    }

    #[test]
    fn parse_log_probe_reads_hits_and_samples() {
        let out = CommandOutput::ok(
            "__RS_HITS__23\n10:12:31 ERROR db timeout\n10:12:33 ERROR db timeout\n",
        );
        match parse_log_probe(&out, LogLevel::Error, "/var/log/app.log") {
            LogProbe::Hits { hits, samples } => {
                assert_eq!(hits, 23);
                assert_eq!(samples.len(), 2);
                assert!(samples[0].contains("ERROR"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn parse_log_probe_detects_missing_and_noperm() {
        let missing = CommandOutput {
            exit_code: Some(3),
            stdout: "__RS_MISSING__\n".into(),
            stderr: String::new(),
            timed_out: false,
        };
        assert_eq!(
            parse_log_probe(&missing, LogLevel::Error, "/x.log"),
            LogProbe::Missing
        );

        let noperm = CommandOutput {
            exit_code: Some(4),
            stdout: "__RS_NOPERM__\n".into(),
            stderr: String::new(),
            timed_out: false,
        };
        assert_eq!(
            parse_log_probe(&noperm, LogLevel::Error, "/x.log"),
            LogProbe::NoPermission
        );
    }

    #[test]
    fn parse_log_probe_reports_failure_when_no_marker() {
        let out = CommandOutput {
            exit_code: Some(1),
            stdout: "grep: not found".into(),
            stderr: String::new(),
            timed_out: false,
        };
        match parse_log_probe(&out, LogLevel::Error, "/x.log") {
            LogProbe::Failed(e) => assert!(!e.is_empty()),
            other => panic!("{other:?}"),
        }
        // 超时优先判为失败
        let to = CommandOutput {
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
            timed_out: true,
        };
        assert!(matches!(
            parse_log_probe(&to, LogLevel::Error, "/x.log"),
            LogProbe::Failed(_)
        ));
    }

    #[test]
    fn log_hits_produce_anomaly_with_samples() {
        let probe = LogProbe::Hits {
            hits: 3,
            samples: vec!["10:00 ERROR boom".into()],
        };
        let (anomaly, summary, evidence, findings) =
            judge_log(&probe, "/var/log/app.log", LogLevel::Error);
        assert!(anomaly);
        assert!(summary.contains('3'), "{summary}");
        assert!(evidence.unwrap().contains("boom"));
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].kind, FindingKind::Anomaly);
        assert!(findings[0].fingerprint.starts_with("log:error#3"));
        assert_eq!(findings[0].rule_id, "builtin.log");
    }

    #[test]
    fn zero_hits_is_clean() {
        let probe = LogProbe::Hits {
            hits: 0,
            samples: Vec::new(),
        };
        let (anomaly, _, evidence, findings) =
            judge_log(&probe, "/var/log/app.log", LogLevel::Error);
        assert!(!anomaly);
        assert!(evidence.is_none());
        assert!(findings.is_empty());
    }

    /// 关键行为：查不了 ≠ 服务器异常。
    #[test]
    fn missing_or_unreadable_log_is_inspection_failure_not_anomaly() {
        for probe in [
            LogProbe::Missing,
            LogProbe::NoPermission,
            LogProbe::Failed("boom".into()),
        ] {
            let (anomaly, summary, _, findings) =
                judge_log(&probe, "/var/log/app.log", LogLevel::Error);
            assert!(!anomaly, "查不了不是异常：{probe:?}");
            assert!(!summary.is_empty());
            assert_eq!(findings.len(), 1);
            assert_eq!(
                findings[0].kind,
                FindingKind::InspectionFailed,
                "{probe:?} 必须是巡检失败"
            );
        }
    }

    #[test]
    fn exit_code_judgement_respects_watch_list() {
        let out = CommandOutput {
            exit_code: Some(2),
            stdout: String::new(),
            stderr: String::new(),
            timed_out: false,
        };
        // 空清单 = 任意非零都算
        let (bad, _, f) = judge_exit(&out, &[]);
        assert!(bad && f.len() == 1);
        // 只盯 1 或 3：退出码 2 不算
        let (bad, _, f) = judge_exit(&out, &[1, 3]);
        assert!(!bad && f.is_empty());
        // 命中清单
        let (bad, _, f) = judge_exit(&out, &[2]);
        assert!(bad && f[0].fingerprint == "exit:2");

        // 退出码 0 永不告警
        let (bad, _, f) = judge_exit(&CommandOutput::ok(""), &[]);
        assert!(!bad && f.is_empty());
    }

    #[test]
    fn bind_findings_fills_target_identity() {
        let mut f = vec![metric_finding(
            LoadCheck::Mem,
            "内存高",
            90.0,
            Severity::Critical,
        )];
        bind_findings(&mut f, "p1", "prod-01");
        assert_eq!(f[0].profile_id, "p1");
        assert_eq!(f[0].profile_name, "prod-01");
    }

    /// v1.1 §2.2.3 / AC-39：聚合后**异常机器一定进上下文**，
    /// 正常机器被折成一行，并且本地完整结果照旧保留。
    ///
    /// 这条用例是整次评审里最重要的回归：原实现（全量拼接 + 首尾截断）在
    /// 50 台规模下会把中间的异常机器静默丢掉，而模型与用户都以为「一切正常」。
    #[test]
    fn aggregation_keeps_anomalies_and_folds_normal_hosts() {
        let mk = |i: usize, anomaly: bool, failed: bool, detail_pad: usize| TargetReport {
            profile_id: format!("p{i}"),
            profile_name: format!("host-{i:02}"),
            connected: true,
            exec_ok: !failed,
            anomaly,
            summary: if anomaly {
                format!("内存 94%（第 {i} 台）")
            } else if failed {
                format!("连接超时（第 {i} 台）")
            } else {
                "正常".into()
            },
            detail: Some("x".repeat(detail_pad)),
            findings: Vec::new(),
        };

        // 50 台：第 3、第 40 台异常（一台靠前、一台靠后），第 49 台巡检失败
        let reports: Vec<TargetReport> = (0..50)
            .map(|i| mk(i, i == 3 || i == 40, i == 49, 4096))
            .collect();
        assert_eq!(reports.len(), 50);
        let agg = aggregate_for_model(&reports, OkDetail::NamesOnly);
        let text = agg.render();

        // 摘要如实报数
        assert!(text.contains("共 50 台"), "{text}");
        assert!(text.contains("2 台异常"), "{text}");
        assert!(text.contains("1 台巡检失败"), "{text}");

        // **两台异常都在**（含靠后的第 40 台）——这正是首尾截断会丢掉的
        assert!(text.contains("host-03"), "靠前的异常必须进上下文：{text}");
        assert!(
            text.contains("host-40"),
            "靠后的异常同样必须进上下文（首尾截断会丢掉它）：{text}"
        );
        assert!(text.contains("host-49"), "巡检失败的机器也必须进上下文");

        // 正常机器没有明细（只有一个名单行）
        assert!(text.contains("正常（47 台）"), "{text}");
        assert!(
            !text.contains("== host-00"),
            "正常机器不该逐台展开（那是被省下来的上下文）"
        );

        // 本地完整结果照旧保留：机器侧不受模型预算约束
        let machine = machine_results(&reports);
        assert_eq!(machine.len(), 50);
        assert!(
            machine
                .iter()
                .all(|m| m.raw.as_ref().map(|r| r.len()).unwrap_or(0) == 4096),
            "机器侧原始输出必须完整保留（不因模型上下文预算而被裁）"
        );

        // 异常数超过逐台上限时，明文说明「其余见运行记录」，而不是静默截断
        let many: Vec<TargetReport> = (0..30).map(|i| mk(i, true, false, 100)).collect();
        let agg = aggregate_for_model(&many, OkDetail::NamesOnly);
        let text = agg.render();
        assert!(
            text.contains(&format!("其余 {} 台", 30 - MODEL_DETAIL_TARGETS)),
            "超出上限要明写「其余 N 台」：{text}"
        );
        assert!(text.contains("未展开"));
    }

    /// `run_command` 的回注必须带上 stdout：模型就是靠它回答
    /// 「有哪些端口在监听」这类问题的，只回一句「N 台正常」等于什么都没给。
    #[test]
    fn run_command_aggregation_carries_stdout_for_healthy_hosts() {
        let mk = |i: usize, anomaly: bool, detail: &str| TargetReport {
            profile_id: format!("p{i}"),
            profile_name: format!("host-{i:02}"),
            connected: true,
            exec_ok: true,
            anomaly,
            summary: if anomaly {
                "退出码 0（阈值超限）".into()
            } else {
                "退出码 0".into()
            },
            detail: Some(detail.to_string()),
            findings: Vec::new(),
        };

        // 单台正常机器：stdout 必须原样进上下文（这是用户报的那个场景）
        let one = vec![mk(0, false, "LISTEN 0 128 0.0.0.0:80 users:(nginx)")];
        let agg = aggregate_for_model(&one, OkDetail::Full);
        let text = agg.render();
        assert!(
            text.contains("LISTEN 0 128 0.0.0.0:80"),
            "正常机器的 stdout 必须回注模型：{text}"
        );
        assert!(text.contains("== host-00"), "{text}");
        assert!(!agg.truncated);

        // 与巡检一致：异常排前、正常在后，两类都逐台展开
        let mixed = vec![
            mk(0, false, "PORT-OK-A"),
            mk(1, true, "PORT-ANOMALY"),
            mk(2, false, "PORT-OK-B"),
        ];
        let text = aggregate_for_model(&mixed, OkDetail::Full).render();
        for needle in ["PORT-OK-A", "PORT-ANOMALY", "PORT-OK-B"] {
            assert!(text.contains(needle), "{needle} 必须进上下文：{text}");
        }
        let bad_at = text.find("PORT-ANOMALY").unwrap();
        let ok_at = text.find("PORT-OK-A").unwrap();
        assert!(bad_at < ok_at, "异常机器要排在正常机器之前：{text}");

        // 上限仍是 MODEL_DETAIL_TARGETS 台：超出部分明写「其余 N 台」，不静默丢
        let many: Vec<TargetReport> = (0..25).map(|i| mk(i, false, "PORT")).collect();
        let agg = aggregate_for_model(&many, OkDetail::Full);
        let text = agg.render();
        assert!(
            text.contains(&format!("其余 {} 台", 25 - MODEL_DETAIL_TARGETS)),
            "正常机器超上限也要明写「其余 N 台」：{text}"
        );
        assert!(text.contains("未展开"), "{text}");

        // 每台按台数摊预算：单台输出再大也不能把后面的台挤掉
        let fat: Vec<TargetReport> = (0..3)
            .map(|i| mk(i, false, &format!("PORT-{i}-{}", "y".repeat(20_000))))
            .collect();
        let agg = aggregate_for_model(&fat, OkDetail::Full);
        let text = agg.render();
        for i in 0..3 {
            assert!(
                text.contains(&format!("PORT-{i}-")),
                "第 {i} 台的开头必须留下（单台输出不得吃光预算）：{text}"
            );
        }
        assert!(text.contains("省略"), "超预算的单台要带省略标记：{text}");
        assert!(!agg.truncated, "摊分后整体不该再触发 8 KiB 兜底：{text}");
    }

    #[test]
    fn aggregation_handles_empty_and_all_ok() {
        let empty = aggregate_for_model(&[], OkDetail::NamesOnly);
        assert!(empty.evidence.is_none());
        assert!(!empty.truncated);

        let ok: Vec<TargetReport> = (0..3)
            .map(|i| TargetReport {
                profile_id: format!("p{i}"),
                profile_name: format!("h{i}"),
                connected: true,
                exec_ok: true,
                anomaly: false,
                summary: "正常".into(),
                detail: None,
                findings: Vec::new(),
            })
            .collect();
        let agg = aggregate_for_model(&ok, OkDetail::NamesOnly);
        let text = agg.render();
        assert!(text.contains("0 台异常"));
        assert!(text.contains("正常（3 台）"));
        assert!(!agg.truncated);
    }

    #[test]
    fn loadavg_and_cores_parsing() {
        // 与 `PROC_PROBE_CMD` 的实际输出一致：loadavg 在独立段里，**头部**是均值
        let text = "cpu0 1 2\ncpu1 1 2\ncpu2 1 2\n__LOADAVG__\n1.20 0.90 0.70 1/100 1234\n";
        assert_eq!(cpu_cores(text), Some(3));
        assert_eq!(parse_loadavg(text), Some(1.20));
        // 裸 /proc/loadavg（无标记）也能读
        assert_eq!(parse_loadavg("0.50 0.40 0.30\n"), Some(0.50));
        // 读不到就给 None，不要编 0.0
        assert_eq!(parse_loadavg("no numbers here"), None);
    }

    /// 探测命令必须真的采集 `/proc/loadavg`：否则「负载/核」永远是未知，
    /// 阈值再配也不会触发 —— 而这条链路在单测里不容易被发现。
    #[test]
    fn probe_command_collects_loadavg() {
        assert!(
            crate::ssh::PROC_PROBE_CMD.contains("__LOADAVG__"),
            "探测命令缺 __LOADAVG__ 段，负载/核判定将永远拿不到数据"
        );
        assert!(crate::ssh::PROC_PROBE_CMD.contains("/proc/loadavg"));
    }

    // ── 只读增强五件套 ──────────────────────────────────────────

    fn out_of(stdout: &str) -> CommandOutput {
        CommandOutput::ok(stdout)
    }

    fn out_failed(stdout: &str, code: u32, stderr: &str) -> CommandOutput {
        CommandOutput {
            exit_code: Some(code),
            stdout: stdout.to_string(),
            stderr: stderr.to_string(),
            timed_out: false,
        }
    }

    // read_file

    /// 路径要过 `shell_quote`，行窗口是我们自己算的数字。
    #[test]
    fn read_probe_command_quotes_the_path_and_windows_the_lines() {
        let cmd = read_probe_command("/var/log/my app.log", 11, 20);
        assert!(cmd.contains("'/var/log/my app.log'"), "{cmd}");
        assert!(cmd.contains("sed -n '11,30p'"), "行窗口算错了：{cmd}");
        // 单引号本身要被转义成 `'\''`，否则路径里一个引号就能闭合并续命令
        let evil = read_probe_command("/tmp/a'b; rm -rf /", 1, 5);
        assert!(!evil.contains("a'b"), "单引号没被转义：{evil}");
        assert!(evil.contains("__RS_MISSING__"), "缺不存在标记");
    }

    #[test]
    fn read_file_reports_content_and_window_edges() {
        let out = out_of("__RS_TOTAL__12\nline one\nline two\n");
        match parse_read_probe(&out) {
            ReadProbe::Lines {
                total,
                lines,
                clipped,
                ..
            } => {
                assert_eq!(total, 12);
                assert_eq!(lines, vec!["line one", "line two"]);
                assert_eq!(clipped, 0);
            }
            other => panic!("解析成了 {other:?}"),
        }
        let (anomaly, summary, evidence, findings) =
            judge_read(&parse_read_probe(&out), "/etc/x.conf", 1, 200);
        assert!(!anomaly, "读文件是信息类，不该自己判异常");
        assert!(findings.is_empty());
        assert!(
            summary.contains("/etc/x.conf") && summary.contains("12"),
            "{summary}"
        );
        assert!(evidence.unwrap().contains("line two"));

        // 窗口落到文件末尾之外：说清文件多大，别让模型以为文件是空的
        let empty = out_of("__RS_TOTAL__3\n");
        let (anomaly, summary, evidence, findings) =
            judge_read(&parse_read_probe(&empty), "/etc/x.conf", 50, 10);
        assert!(!anomaly);
        assert!(findings.is_empty());
        assert!(summary.contains('3') && summary.contains("50"), "{summary}");
        assert!(evidence.is_none());
    }

    /// 「看不了」必须是**巡检失败**（§8.3），而不是「文件里没东西」。
    #[test]
    fn read_file_turns_missing_and_unreadable_into_inspection_failure() {
        for (text, code) in [
            ("__RS_MISSING__\n", 3u32),
            ("__RS_NOPERM__\n", 4),
            ("__RS_ISDIR__\n", 5),
        ] {
            let probe = parse_read_probe(&out_failed(text, code, ""));
            let (anomaly, summary, _, findings) = judge_read(&probe, "/etc/x", 1, 10);
            assert!(!anomaly, "查不了 ≠ 服务器异常：{summary}");
            assert_eq!(findings.len(), 1, "{summary}");
            assert_eq!(findings[0].kind, FindingKind::InspectionFailed);
            assert!(!summary.is_empty());
        }
    }

    /// 超长行要裁（压缩 js 一行就能吃掉整个回注预算），且裁了要说出来。
    #[test]
    fn read_file_clips_absurdly_long_lines() {
        let long = "x".repeat(MAX_LINE_CHARS * 2);
        let out = out_of(&format!("__RS_TOTAL__1\n{long}\n"));
        match parse_read_probe(&out) {
            ReadProbe::Lines { lines, clipped, .. } => {
                assert_eq!(clipped, 1);
                assert!(lines[0].len() < long.len());
                assert!(
                    lines[0].contains("本行共"),
                    "裁了要说明：{}",
                    lines[0].len()
                );
            }
            other => panic!("解析成了 {other:?}"),
        }
        let (_, summary, _, _) = judge_read(&parse_read_probe(&out), "/a.js", 1, 200);
        assert!(summary.contains("截断"), "摘要要说明有行被裁：{summary}");
    }

    /// 文件内容里常带密码，明细与回注都要脱敏。
    #[test]
    fn read_file_redacts_secrets_in_the_content() {
        let out = out_of("__RS_TOTAL__1\npassword=hunter2\n");
        let (_, _, evidence, _) = judge_read(&parse_read_probe(&out), "/etc/app.conf", 1, 10);
        let ev = evidence.expect("有内容就该有明细");
        assert!(!ev.contains("hunter2"), "明文密码进了明细：{ev}");
    }

    // inspect_service

    #[test]
    fn service_probe_command_quotes_the_unit() {
        let cmd = service_probe_command("nginx.service");
        assert!(cmd.contains("systemctl show 'nginx.service'"), "{cmd}");
        assert!(cmd.contains("__RS_NOSYSTEMD__"), "缺「没有 systemd」标记");
        assert!(cmd.contains("ActiveState"), "要取激活态");
    }

    fn service_out(state: &str, sub: &str, load: &str) -> CommandOutput {
        out_of(&format!(
            "Id=nginx.service\nLoadState={load}\nActiveState={state}\nSubState={sub}\n\
             UnitFileState=enabled\nMainPID=1234\nExecMainStatus=1\nNRestarts=3\n\
             ActiveEnterTimestamp=Mon 2026-09-21 10:00:00 CST\n__RS_RC__0\n"
        ))
    }

    /// `failed` 判异常（Critical），`inactive` **不判** —— 一次性单元本来就是
    /// inactive，一律报警会让每次巡检都刷假警报。
    #[test]
    fn service_failed_is_an_anomaly_but_inactive_is_not() {
        let failed = judge_service(
            parse_service_probe(&service_out("failed", "failed", "loaded")),
            "nginx",
        );
        assert!(failed.0, "failed 必须判成异常");
        assert_eq!(failed.3.len(), 1);
        assert_eq!(failed.3[0].severity, Severity::Critical);
        assert!(
            failed.3[0].message.contains("退出码 1"),
            "{}",
            failed.3[0].message
        );
        assert!(failed.1.contains("failed"), "{}", failed.1);

        let dead = judge_service(
            parse_service_probe(&service_out("inactive", "dead", "loaded")),
            "oneshot-job",
        );
        assert!(!dead.0, "inactive 不该判异常：{}", dead.1);
        assert!(dead.3.is_empty());
        assert!(dead.1.contains("inactive (dead)"), "{}", dead.1);
        assert!(
            dead.1.contains("enabled"),
            "自启动状态要一起给出：{}",
            dead.1
        );
    }

    /// 单元不存在是**巡检失败**（名字写错了），不是「服务挂了」。
    #[test]
    fn unknown_unit_is_an_inspection_failure() {
        let probe = parse_service_probe(&service_out("inactive", "dead", "not-found"));
        let (anomaly, summary, _, findings) = judge_service(probe, "nginxx");
        assert!(!anomaly);
        assert_eq!(findings[0].kind, FindingKind::InspectionFailed);
        assert!(summary.contains("nginxx"), "{summary}");
    }

    /// 没有 systemd / 查询失败都不能静默成「没查到」。
    #[test]
    fn missing_systemd_is_reported_not_swallowed() {
        let err = parse_service_probe(&out_of("__RS_NOSYSTEMD__\n")).unwrap_err();
        assert!(err.contains("systemd"), "{err}");
        let (_, msg, _, findings) = judge_service(Err(err), "nginx");
        assert_eq!(findings[0].kind, FindingKind::InspectionFailed);
        assert!(msg.contains("nginx"), "{msg}");
    }

    // inspect_ports

    #[test]
    fn ports_probe_command_carries_no_model_input() {
        let cmd = ports_probe_command();
        assert!(cmd.contains("ss -H -tlnp"));
        assert!(cmd.contains("__RS_LISTEN__") && cmd.contains("__RS_ESTAB__"));
        assert!(cmd.contains("__RS_NOSS__"), "缺「没有 ss」标记");
    }

    #[test]
    fn ports_parsing_reads_address_process_and_connection_counts() {
        let out = out_of(
            "__RS_LISTEN__\n\
             LISTEN 0 128 0.0.0.0:22 0.0.0.0:* users:((\"sshd\",pid=900,fd=3))\n\
             LISTEN 0 511 127.0.0.1:8080 0.0.0.0:* users:((\"nginx\",pid=1234,fd=6))\n\
             __RS_ESTAB__\n   42 8080\n    7 22\n",
        );
        let probe = parse_ports_probe(&out).expect("应当解析成功");
        assert_eq!(probe.listen.len(), 2);
        assert_eq!(probe.listen[0].port, 22);
        assert_eq!(probe.listen[0].process.as_deref(), Some("sshd"));
        assert_eq!(probe.listen[1].local, "127.0.0.1:8080");
        assert_eq!(probe.estab, vec![(8080, 42), (22, 7)]);

        // 不给端口：只报事实
        let (anomaly, summary, _, findings) = judge_ports(Ok(probe.clone()), None);
        assert!(!anomaly, "全量清单不该自己判异常");
        assert!(findings.is_empty());
        assert!(summary.contains('2'), "{summary}");
        assert!(summary.contains("8080"), "要说连接数最多的端口：{summary}");
    }

    /// 指名端口而它没在监听 → 一项异常（这是用户问的那件事的答案）。
    #[test]
    fn ports_named_port_not_listening_is_flagged() {
        let probe = parse_ports_probe(&out_of(
            "__RS_LISTEN__\nLISTEN 0 128 0.0.0.0:22 0.0.0.0:* users:((\"sshd\",pid=900,fd=3))\n\
             __RS_ESTAB__\n",
        ))
        .unwrap();
        let (anomaly, summary, _, findings) = judge_ports(Ok(probe.clone()), Some(80));
        assert!(anomaly, "指名的端口没在监听要标出来");
        assert_eq!(findings[0].fingerprint, "port:not-listening");
        assert!(summary.contains("80"), "{summary}");

        let (anomaly, summary, _, findings) = judge_ports(Ok(probe), Some(22));
        assert!(!anomaly);
        assert!(findings.is_empty());
        assert!(summary.contains("sshd"), "要指出谁在监听：{summary}");
    }

    /// 从进程串里取不到进程名时不许编 —— 非 root 看不到别人的进程是常态。
    #[test]
    fn ports_without_process_info_do_not_invent_a_name() {
        let probe = parse_ports_probe(&out_of(
            "__RS_LISTEN__\nLISTEN 0 128 0.0.0.0:80 0.0.0.0:*\n__RS_ESTAB__\n",
        ))
        .unwrap();
        assert_eq!(probe.listen[0].process, None);
        let (_, summary, _, _) = judge_ports(Ok(probe), Some(80));
        assert!(!summary.contains("80 正在监听（, "), "{summary}");
    }

    // check_certificate

    #[test]
    fn cert_probe_command_quotes_host_and_sni() {
        let cmd = cert_probe_command("127.0.0.1", 443, Some("example.com"));
        assert!(cmd.contains("'127.0.0.1:443'"), "{cmd}");
        assert!(cmd.contains("-servername 'example.com'"), "{cmd}");
        assert!(cmd.contains("__RS_NOOPENSSL__"), "缺「没有 openssl」标记");
        // 不给 SNI 时不该凭空加一个
        assert!(!cert_probe_command("10.0.0.1", 8443, None).contains("-servername"));
    }

    /// 到期天数是**本地算**的（远端 `date` 各版本方言太多），故拿固定时钟钉住。
    #[test]
    fn cert_days_left_is_computed_from_the_expiry_line() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-09-22T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let out = out_of(
            "subject=CN = example.com\nissuer=C = US, O = Let's Encrypt, CN = R3\n\
             notBefore=Aug 1 00:00:00 2026 GMT\nnotAfter=Oct 22 00:00:00 2026 GMT\n\
             SHA256 Fingerprint=AB:CD\n__RS_RC__0\n",
        );
        let info = parse_cert(&out, now).expect("应当解析成功");
        assert_eq!(info.days_left, 30, "10-22 减 09-22 是 30 天");
        assert!(info.subject.contains("example.com"));
        assert_eq!(info.fingerprint.as_deref(), Some("AB:CD"));

        // 30 天整 → 警告（阈值是 ≤30）
        let (anomaly, summary, _, findings) = judge_cert(Ok(info), "127.0.0.1", 443);
        assert!(anomaly);
        assert_eq!(findings[0].severity, Severity::Warning);
        assert!(summary.contains("30"), "{summary}");
    }

    /// 已过期 / 7 天内 → Critical；半年有效期 → 什么都不报。
    #[test]
    fn cert_expiry_severity_follows_the_thresholds() {
        let now = chrono::Utc::now();
        let mk = |days: i64| CertInfo {
            subject: "CN = a.com".into(),
            issuer: "CN = CA".into(),
            not_after: "x".into(),
            fingerprint: None,
            days_left: days,
        };
        assert_eq!(
            judge_cert(Ok(mk(-3)), "h", 443).3[0].severity,
            Severity::Critical
        );
        assert_eq!(
            judge_cert(Ok(mk(5)), "h", 443).3[0].severity,
            Severity::Critical
        );
        assert_eq!(
            judge_cert(Ok(mk(20)), "h", 443).3[0].severity,
            Severity::Warning
        );
        let fine = judge_cert(Ok(mk(200)), "h", 443);
        assert!(
            !fine.0 && fine.3.is_empty(),
            "半年有效期不该报警：{}",
            fine.1
        );
        let _ = now;
    }

    /// 端口不通 / 不是 TLS：是**巡检失败**，不是「证书有问题」。
    #[test]
    fn cert_connection_failure_is_an_inspection_failure() {
        let err =
            parse_cert(&out_failed("", 1, "connect refused"), chrono::Utc::now()).unwrap_err();
        assert!(err.contains("无法获取证书"), "{err}");
        let (anomaly, _, _, findings) = judge_cert(Err(err), "127.0.0.1", 443);
        assert!(!anomaly);
        assert_eq!(findings[0].kind, FindingKind::InspectionFailed);
    }

    // find_process

    /// 进程名会进 `pgrep` 的模式串：元字符要转义，工具缺失要报出来。
    #[test]
    fn process_probe_command_escapes_the_pattern_and_checks_tools() {
        let cmd = process_probe_command(Some("nginx"), None);
        assert!(cmd.contains("pgrep -- 'nginx'"), "{cmd}");
        assert!(cmd.contains("__RS_NOPGREP__"), "缺「没有 pgrep」标记");
        // ERE 元字符逐个转义：`a|b` 不能被当成「a 或 b」
        let esc = process_probe_command(Some("a|b"), None);
        assert!(esc.contains("'a\\|b'"), "元字符没转义：{esc}");
        // 按端口找时不带进程名，且走监听清单
        let by_port = process_probe_command(None, Some(80));
        assert!(by_port.contains("ss -H -tlnp"), "{by_port}");
        assert!(!by_port.contains("pgrep"), "{by_port}");
    }

    #[test]
    fn find_process_reports_matches_and_absence_by_name() {
        let out = out_of(
            "__RS_BEGIN__\n  PID  PPID %CPU %MEM     ELAPSED STAT COMMAND\n\
             1234     1  0.0  0.1       10:00 Ss   nginx: master process\n\
             __RS_SPLIT__\n",
        );
        let probe = parse_process_probe(&out, true, false).unwrap();
        let rows = probe.procs.clone().unwrap();
        assert_eq!(rows.len(), 1, "表头要被丢掉：{rows:?}");
        let (_, summary, evidence, findings) = judge_process(Ok(probe), Some("nginx"), None);
        assert!(summary.contains("1 个匹配"), "{summary}");
        assert!(findings.is_empty());
        assert!(evidence.unwrap().contains("master process"));

        // 没找到：是答案，不是异常
        let none =
            parse_process_probe(&out_of("__RS_BEGIN__\n__RS_SPLIT__\n"), true, false).unwrap();
        let (anomaly, summary, _, findings) = judge_process(Ok(none), Some("redis"), None);
        assert!(!anomaly);
        assert!(findings.is_empty());
        assert!(summary.contains("redis"), "{summary}");
    }

    #[test]
    fn find_process_finds_the_owner_of_a_port() {
        let out = out_of(
            "__RS_BEGIN__\n\
             LISTEN 0 511 127.0.0.1:8080 0.0.0.0:* users:((\"nginx\",pid=1234,fd=6))\n",
        );
        let probe = parse_process_probe(&out, false, true).unwrap();
        let (_, summary, _, findings) = judge_process(Ok(probe), None, Some(8080));
        assert!(summary.contains("nginx"), "{summary}");
        assert!(findings.is_empty());

        let probe = parse_process_probe(&out, false, true).unwrap();
        let (_, summary, _, _) = judge_process(Ok(probe), None, Some(9999));
        assert!(summary.contains("9999"), "{summary}");
    }

    /// **关键**：工具缺失不能被读成「没有这个进程」。
    #[test]
    fn find_process_distinguishes_tool_missing_from_no_match() {
        let out = out_of("__RS_BEGIN__\n__RS_NOPGREP__\n__RS_SPLIT__\n");
        let probe = parse_process_probe(&out, true, false).unwrap();
        assert!(probe.procs.is_none(), "工具缺失时不该给出空匹配");
        assert_eq!(probe.missing_tools.len(), 1);
        let (anomaly, summary, _, findings) = judge_process(Ok(probe), Some("nginx"), None);
        assert!(!anomaly);
        assert_eq!(findings[0].kind, FindingKind::InspectionFailed);
        assert!(summary.contains("pgrep"), "{summary}");

        // 两段都没查成 → 整次调用就是巡检失败
        let both = parse_process_probe(
            &out_of("__RS_BEGIN__\n__RS_NOPGREP__\n__RS_SPLIT__\n__RS_NOSS__\n"),
            true,
            true,
        )
        .unwrap();
        let (_, _, _, findings) = judge_process(Ok(both), Some("nginx"), Some(80));
        assert_eq!(findings[0].fingerprint, "process:no-tool");
    }
}
