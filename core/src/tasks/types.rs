//! 定时任务与巡检结果的数据类型。
//!
//! 独立成模块是为了让 `tasks` / `runner` / `alerts` 三个使用者共享同一套类型，
//! 避免「任务记录里的 Finding」与「告警引擎里的 Finding」变成两个漂移的定义。

use serde::{Deserialize, Serialize};

// 目标

/// 任务（或 agent 工具调用）的目标集合。
///
/// 三种选法是**并集**，`exclude` 最后做差集。存的是 id 而不是名字：
/// 分组改名、连接改名都不会让任务目标失效。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TaskTarget {
    /// 分组 id（含其全部子分组）
    pub groups: Vec<String>,
    /// 显式连接 id
    pub profiles: Vec<String>,
    /// 连接标签（横切筛选）
    pub tags: Vec<String>,
    /// 从上述结果中排除的连接 id
    pub exclude: Vec<String>,
}

impl TaskTarget {
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty() && self.profiles.is_empty() && self.tags.is_empty()
    }

    /// 一句话描述（不含解析结果，仅描述选法本身）。
    pub fn describe(&self) -> String {
        use crate::i18n::ts;
        let mut parts: Vec<String> = Vec::new();
        if !self.groups.is_empty() {
            parts.push(ts(
                format!("{} 个分组", self.groups.len()),
                format!("{} group(s)", self.groups.len()),
            ));
        }
        if !self.profiles.is_empty() {
            parts.push(ts(
                format!("{} 个连接", self.profiles.len()),
                format!("{} connection(s)", self.profiles.len()),
            ));
        }
        if !self.tags.is_empty() {
            parts.push(ts(
                format!("标签 {}", self.tags.join("/")),
                format!("tag {}", self.tags.join("/")),
            ));
        }
        if !self.exclude.is_empty() {
            parts.push(ts(
                format!("排除 {}", self.exclude.len()),
                format!("exclude {}", self.exclude.len()),
            ));
        }
        if parts.is_empty() {
            return ts("未选择目标".into(), "no target".into());
        }
        parts.join(" · ")
    }
}

// 触发

/// 触发方式。输入最终收敛为任务上的 `next_run_at`（unix 秒），
/// 调度器只做 `now >= next_run_at` 这一个比较，不必知道触发是哪种形态。
///
/// 时区**不**作为任务字段：这是桌面软件，cron 一律跟随本机系统时区
/// （`chrono::Local`，含夏令时规则）——系统时区本身就是自动探测的，
/// 用户无需、也不应手填。旧文件里的 `tz` 字段由 serde 忽略。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Trigger {
    /// 每 N 秒（下限 [`MIN_INTERVAL_SECS`]）
    Interval { every_secs: u64 },
    /// cron 表达式（5 段，见 [`crate::tasks::cron`]），跟随本机时区
    Cron { expr: String },
}

/// interval 的下限：一分钟。再短就不是「定时任务」而是轮询了。
pub const MIN_INTERVAL_SECS: u64 = 60;

impl Trigger {
    /// 按计划（cron）触发，跟随本机时区。
    ///
    /// 构造器而不是让调用方写 `Trigger::Cron { expr: ..to_string() }`：
    /// 任务编辑器、面板模板、`propose_task` 多处都要用，散开写迟早有人漏掉
    /// `to_string`。
    pub fn cron(expr: &str) -> Self {
        Self::Cron {
            expr: expr.to_string(),
        }
    }

    /// 从 `from` 起的下一次触发时刻。
    pub fn next_after(&self, from: u64) -> Option<u64> {
        match self {
            Self::Interval { every_secs } => {
                let step = (*every_secs).max(MIN_INTERVAL_SECS);
                // 对齐到 step 的整数倍，避免同一任务因保存时刻不同而在「奇怪的秒数」上触发
                Some(from - from % step + step)
            }
            Self::Cron { expr } => crate::tasks::cron::parse_cron(expr)
                .ok()
                .and_then(|s| s.next_strict(from)),
        }
    }

    /// 保存前校验（UI 侧据此拒绝保存，而不是存一个到点必然失败的任务）。
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::Interval { every_secs } => {
                if *every_secs < MIN_INTERVAL_SECS {
                    Err(format!("间隔不能小于 {MIN_INTERVAL_SECS} 秒"))
                } else {
                    Ok(())
                }
            }
            Self::Cron { expr } => {
                crate::tasks::cron::parse_cron(expr).map_err(|e| e.to_string())?;
                Ok(())
            }
        }
    }

    /// 人话描述（列表标题上显示的「下次触发」）。
    pub fn describe(&self) -> String {
        use crate::i18n::ts;
        match self {
            Self::Interval { every_secs } => {
                if *every_secs % 3600 == 0 {
                    let h = every_secs / 3600;
                    ts(format!("每 {h} 小时"), format!("Every {h} h"))
                } else if *every_secs % 60 == 0 {
                    let m = every_secs / 60;
                    ts(format!("每 {m} 分钟"), format!("Every {m} min"))
                } else {
                    ts(
                        format!("每 {every_secs} 秒"),
                        format!("Every {every_secs} s"),
                    )
                }
            }
            Self::Cron { expr } => crate::tasks::cron::parse_cron(expr)
                .map(|s| s.describe())
                .unwrap_or_else(|_| ts("表达式无效".into(), "invalid cron".into())),
        }
    }
}

// 动作

/// 巡检项（决定给 AI 的工具白名单，缩小动作空间）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckKind {
    Load,
    Log,
    Reachability,
}

impl CheckKind {
    pub fn label(self) -> &'static str {
        use crate::i18n::t;
        match self {
            Self::Load => t("负载"),
            Self::Log => t("日志"),
            Self::Reachability => t("连通性"),
        }
    }
}

/// 日志异常级别（决定默认 grep 模式）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    #[default]
    Error,
    Warn,
    Fatal,
}

impl LogLevel {
    /// 默认匹配模式（ERE）。覆盖面优先：日志格式千差万别，宁可多报也不漏报。
    pub fn default_pattern(self) -> &'static str {
        match self {
            Self::Error => "(?i)\\b(error|err|fatal|panic|exception|traceback|critical)\\b",
            Self::Warn => "(?i)\\b(warn|warning)\\b",
            Self::Fatal => "(?i)\\b(fatal|panic|segfault|oom[- ]?kill)\\b",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Fatal => "fatal",
        }
    }

    pub fn all() -> [LogLevel; 3] {
        [Self::Error, Self::Warn, Self::Fatal]
    }
}

/// 负载阈值。默认值即 `docs/AI-Agent与定时任务需求文档` §3.5 表。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LoadThresholds {
    pub cpu_percent: f32,
    pub mem_percent: f32,
    pub swap_percent: f32,
    pub disk_percent: f32,
    pub load_per_core: f32,
}

impl Default for LoadThresholds {
    fn default() -> Self {
        Self {
            cpu_percent: 85.0,
            mem_percent: 90.0,
            swap_percent: 50.0,
            disk_percent: 90.0,
            load_per_core: 1.5,
        }
    }
}

/// 任务的四类动作。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TaskAction {
    /// 多选连接：到点把目标连上。
    ///
    /// `open_tabs = false`（默认）走**后台连接**：只建会话、不开 Tab ——
    /// 否则用户离开一晚回来会看到 20 个标签。
    MultiConnect {
        target: TaskTarget,
        #[serde(default)]
        open_tabs: bool,
        /// 任务结束后会话保留时长（秒）；0 = 立即断开
        #[serde(default)]
        keep_alive_secs: u64,
    },
    /// 在目标集合上执行同一条命令。
    ExecMulti {
        target: TaskTarget,
        command: String,
        #[serde(default = "default_exec_timeout")]
        timeout_secs: u64,
        /// 无人值守下是否允许状态变更命令（默认否，见安全规格 §7.4）
        #[serde(default)]
        allow_mutating: bool,
    },
    /// 从目标下载文件。
    Download {
        target: TaskTarget,
        remote_path: String,
        #[serde(default)]
        local_dir: Option<String>,
    },
    /// AI 巡检。
    AiInspect {
        target: TaskTarget,
        /// 补充要求（自然语言）。为空则只用内置提示词。
        #[serde(default)]
        prompt: String,
        #[serde(default)]
        checks: Vec<CheckKind>,
        #[serde(default)]
        thresholds: Option<LoadThresholds>,
        #[serde(default)]
        log_path: Option<String>,
        #[serde(default)]
        log_level: LogLevel,
    },
}

fn default_exec_timeout() -> u64 {
    DEFAULT_EXEC_TIMEOUT_SECS
}

/// 单台命令的默认超时（秒）。巡检不能让一台卡死的机器拖住整批。
pub const DEFAULT_EXEC_TIMEOUT_SECS: u64 = 15;
/// 单台命令的超时上限（秒）。
pub const MAX_EXEC_TIMEOUT_SECS: u64 = 120;

impl TaskAction {
    pub fn target(&self) -> &TaskTarget {
        match self {
            Self::MultiConnect { target, .. }
            | Self::ExecMulti { target, .. }
            | Self::Download { target, .. }
            | Self::AiInspect { target, .. } => target,
        }
    }

    pub fn target_mut(&mut self) -> &mut TaskTarget {
        match self {
            Self::MultiConnect { target, .. }
            | Self::ExecMulti { target, .. }
            | Self::Download { target, .. }
            | Self::AiInspect { target, .. } => target,
        }
    }

    /// 动作类别名（列表副标题、运行记录用）。
    pub fn kind_label(&self) -> &'static str {
        use crate::i18n::t;
        match self {
            Self::MultiConnect { .. } => t("批量连接"),
            Self::ExecMulti { .. } => t("批量执行命令"),
            Self::Download { .. } => t("批量下载"),
            Self::AiInspect { .. } => t("AI 巡检"),
        }
    }

    /// 保存前校验。返回 `Err` 即拒绝保存（UI 聚焦出错字段并提示）。
    pub fn validate(&self) -> Result<(), String> {
        use crate::i18n::ts;
        let t = self.target();
        if t.is_empty() {
            return Err(ts("请选择目标".into(), "target required".into()));
        }
        match self {
            Self::MultiConnect { .. } => Ok(()),
            Self::ExecMulti { command, .. } => {
                let cmd = command.trim();
                if cmd.is_empty() {
                    return Err(ts("命令不能为空".into(), "command required".into()));
                }
                if cmd.len() > MAX_COMMAND_LEN {
                    return Err(ts(
                        format!("命令过长（上限 {MAX_COMMAND_LEN} 字符）"),
                        format!("command too long (max {MAX_COMMAND_LEN})"),
                    ));
                }
                // Blocked 级命令不允许保存：存一个到点必然被拦的任务没有意义，
                // 用户在保存时就应该知道这条命令永远不会被执行。
                if crate::ai::assess_command(cmd) == crate::ai::Risk::Blocked {
                    return Err(ts(
                        "该命令属于不可逆破坏操作，已被安全策略永久拦截，无法用于定时任务".into(),
                        "This command is permanently blocked by the safety policy".into(),
                    ));
                }
                Ok(())
            }
            Self::Download { remote_path, .. } => {
                validate_remote_path(remote_path)?;
                Ok(())
            }
            Self::AiInspect {
                log_path, prompt, ..
            } => {
                // 「AI 要求」**必输**（2026-09-22 用户要求）。
                //
                // 为什么放在 core 而不是界面上：它是模型唯一的指令，空着的话到点
                // 只会得到一次「不知道要查什么」的巡检；而 `validate()` 恰好是
                // 保存 / 启用 / 开跑三条路径的**共同关口**（`task_draft_save`、
                // `task_toggle_enable`、`runner` 执行前各调一次），一处拦、三处生效。
                // 纯空白按空处理（粘贴出来的换行不算填了要求）。
                if prompt.trim().is_empty() {
                    return Err(ts(
                        "请填写 AI 要求".into(),
                        "AI requirement required".into(),
                    ));
                }
                if prompt.chars().count() > MAX_PROMPT_CHARS {
                    return Err(ts(
                        format!("补充要求过长（上限 {MAX_PROMPT_CHARS} 字）"),
                        format!("prompt too long (max {MAX_PROMPT_CHARS})"),
                    ));
                }
                // 日志路径已从编辑器撤下（用户反馈：日志路径没了）——**不再要求**
                // 必填：新版界面存出来的就是「空巡检项 + 无路径」，再拦就永远保存不上。
                // 但老任务里存着的那条路径仍要过安全校验（长度 / `..` / 伪文件系统），
                // 不能因为界面改版就放坏数据进无人值守的执行路径。
                if let Some(p) = log_path.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
                    validate_remote_path(p)?;
                }
                Ok(())
            }
        }
    }
}

/// 命令长度上限（字符）。与 §7.2 的注入防护口径一致。
pub const MAX_COMMAND_LEN: usize = 8192;
/// 巡检补充要求的长度上限（字）。
pub const MAX_PROMPT_CHARS: usize = 500;
/// 远端路径长度上限（字符）。
pub const MAX_PATH_LEN: usize = 4096;
/// grep 模式长度上限（字符）。
pub const MAX_PATTERN_LEN: usize = 512;

/// 远端路径校验（`inspect_log` / `Download` 共用）。
///
/// 拦的是三类：过长、`..` 段（穿越）、以及 `/dev`、`/proc`、`/sys` 这些
/// 「不是日志也不该被批量读」的伪文件系统。
pub fn validate_remote_path(path: &str) -> Result<(), String> {
    use crate::i18n::ts;
    let p = path.trim();
    if p.is_empty() {
        return Err(ts("路径不能为空".into(), "path required".into()));
    }
    if p.chars().count() > MAX_PATH_LEN {
        return Err(ts(
            format!("路径过长（上限 {MAX_PATH_LEN} 字符）"),
            format!("path too long (max {MAX_PATH_LEN})"),
        ));
    }
    if p.split(['/', '\\']).any(|seg| seg == "..") {
        return Err(ts(
            "路径不能包含 .. （禁止目录穿越）".into(),
            "path must not contain ..".into(),
        ));
    }
    const DENY: [&str; 3] = ["/dev/", "/proc/", "/sys/"];
    if DENY.iter().any(|d| p.starts_with(d)) {
        return Err(ts(
            format!("不允许访问 {p}（伪文件系统）"),
            format!("access denied: {p}"),
        ));
    }
    Ok(())
}

/// 单条 shell 参数的安全引用：单引号包裹 + 内部单引号转义。
///
/// **禁止**把用户输入直接 `format!` 进远程命令串 —— 这是注入的唯一入口。
pub fn shell_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            // 结束单引号 -> 转义的单引号 -> 重新开始单引号
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

// 运行状态与结果

/// 一次运行的结果状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    #[default]
    Never,
    Ok,
    /// 部分目标失败（部分是「巡检失败」，不是「服务器异常」）
    Partial,
    Failed,
    /// 上一次仍在运行 / 目标为空 / 错过计划窗口
    Skipped,
    Cancelled,
}

impl RunStatus {
    pub fn label(self) -> &'static str {
        use crate::i18n::t;
        match self {
            Self::Never => t("从未运行"),
            Self::Ok => t("正常"),
            Self::Partial => t("部分失败"),
            Self::Failed => t("失败"),
            Self::Skipped => t("已跳过"),
            Self::Cancelled => t("已取消"),
        }
    }
}

/// finding 的类别。**「异常」与「巡检失败」必须分开**：
/// 把「连不上机器」报成「服务器异常」会让运维误判。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingKind {
    /// 服务器确实不正常（CPU 超阈、日志有 ERROR、磁盘将满）
    Anomaly,
    /// 我们没查到（连接失败、超时、无权限、路径不存在）
    InspectionFailed,
    /// 正常但有价值的信息（如「任务已恢复」）
    Info,
}

/// 严重级（决定通知前缀与 UI 颜色）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Warning,
    Critical,
}

impl Severity {
    pub fn label(self) -> &'static str {
        use crate::i18n::t;
        match self {
            Self::Info => t("提示"),
            Self::Warning => t("警告"),
            Self::Critical => t("严重"),
        }
    }

    /// 邮件主题里的英文标签（避免非 ASCII 主题被网关改写）。
    pub fn tag(self) -> &'static str {
        match self {
            Self::Info => "INFO",
            Self::Warning => "WARNING",
            Self::Critical => "CRITICAL",
        }
    }
}

/// 一条检测结论。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Finding {
    pub kind: FindingKind,
    pub profile_id: String,
    pub profile_name: String,
    /// 产生它的规则 id（去重与抑制按它分组）
    pub rule_id: String,
    /// 指纹：同规则 + 同目标 + 同原因才去重（如 `mem>90`、`log:error`）
    pub fingerprint: String,
    pub severity: Severity,
    /// 一句话描述（已脱敏）
    pub message: String,
    /// 证据（指标值 / 日志样本，已脱敏截断）
    #[serde(default)]
    pub evidence: Option<String>,
}

/// 单个目标在一次运行里的结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TargetOutcome {
    pub profile_id: String,
    pub profile_name: String,
    pub connected: bool,
    pub exec_ok: bool,
    /// 该目标是否被判为异常（与「巡检失败」严格区分）
    pub anomaly: bool,
    /// 一行摘要（已脱敏）
    pub summary: String,
    /// 展开可见的原始输出（截断后）
    #[serde(default)]
    pub detail: Option<String>,
}

/// 通知策略（任务级覆盖默认）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NotifyPolicy {
    /// 正常也通知（默认关：那是告警疲劳的来源）
    pub on_ok: bool,
    pub on_anomaly: bool,
    pub on_error: bool,
    /// 合并窗口：同一任务窗口内的多条告警合并成一条通知
    pub digest_window_secs: u64,
}

impl Default for NotifyPolicy {
    fn default() -> Self {
        Self {
            on_ok: false,
            on_anomaly: true,
            on_error: true,
            digest_window_secs: 300,
        }
    }
}

/// 一次运行留下的 **AI 会话痕迹**（`AiInspect` 专有）。
///
/// 为什么跟着运行记录一起落盘：用户点开某次运行的「会话」要能看到
/// 「这次到底查了什么、模型说了什么、跑了哪些工具」——而这些在进程存活期内
/// 只存在于 agent 循环的内存里。不落盘的代价是重启后这一切蒸发，
/// 而定时任务恰恰是「跑的时候人不在」的场景。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RunSession {
    /// 任务里那条「AI 要求」的原文（会话里作为用户提问回显）。
    pub prompt: String,
    /// 巡检结论（模型最后一段回复）。
    pub conclusion: String,
    /// 工具轨迹（每步一行，明细已按 [`SESSION_TRACE_DETAIL_BYTES`] 截断）。
    pub trace: Vec<crate::agent::TraceRow>,
}

impl RunSession {
    /// 内容全空（旧记录 / 未配置 AI 时）：不值得为它建一条会话。
    pub fn is_empty(&self) -> bool {
        self.prompt.trim().is_empty() && self.conclusion.trim().is_empty() && self.trace.is_empty()
    }
}

/// 会话里保留的轨迹明细上限（字节/行）。
///
/// 比 UI 的存储口径（[`crate::agent::trace::MAX_TRACE_DETAIL_BYTES`]，64 KiB）
/// 紧得多：运行记录是 **jsonl 追加**且每次运行都写一条，一条记录里挂上
/// 几十个 64 KiB 明细，翻看历史会变成读几十兆文本。8 KiB 与「送渲染」口径
/// 同量级，足够看清那几台机器的异常段。
pub const SESSION_TRACE_DETAIL_BYTES: usize = 8 * 1024;

/// 一次运行的完整记录（追加写 `task-runs/<task_id>.jsonl`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunRecord {
    pub run_id: String,
    pub task_id: String,
    pub task_name: String,
    pub started_at: u64,
    pub finished_at: u64,
    pub status: RunStatus,
    pub targets: Vec<TargetOutcome>,
    /// 机器侧**完整**结构化结果（v1.1 §2.2.3）。
    ///
    /// 与回注模型的那份分开存：模型上下文只有 8 KiB，而本地留证几乎不花钱。
    /// 混成一个字段的代价必然是「为省上下文而丢掉本地证据」。
    #[serde(default)]
    pub machine: Vec<crate::runner::inspect::MachineResult>,
    /// 巡检结论（`AiInspect` 专有）
    #[serde(default)]
    pub conclusion: Option<String>,
    /// 异常清单（触发预警的依据）
    #[serde(default)]
    pub findings: Vec<Finding>,
    /// 跳过 / 失败的原因（状态为 Skipped / Failed 时有值）
    #[serde(default)]
    pub note: Option<String>,
    /// 本次运行的 AI 会话痕迹（`AiInspect` 专有；老记录里没有这个字段）。
    #[serde(default)]
    pub session: Option<RunSession>,
}

impl RunRecord {
    pub fn elapsed_ms(&self) -> u64 {
        self.finished_at.saturating_sub(self.started_at) * 1000
    }

    pub fn anomaly_count(&self) -> usize {
        self.findings
            .iter()
            .filter(|f| f.kind == FindingKind::Anomaly)
            .count()
    }

    pub fn failed_target_count(&self) -> usize {
        self.targets.iter().filter(|t| !t.exec_ok).count()
    }

    /// 列表行的一句话摘要。
    pub fn one_line(&self) -> String {
        use crate::i18n::ts;
        let anomaly = self.anomaly_count();
        let failed = self.failed_target_count();
        match self.status {
            RunStatus::Skipped | RunStatus::Failed | RunStatus::Cancelled => self
                .note
                .clone()
                .unwrap_or_else(|| self.status.label().to_string()),
            _ => {
                let mut parts = Vec::new();
                if anomaly > 0 {
                    parts.push(ts(
                        format!("{anomaly} 台异常"),
                        format!("{anomaly} anomaly"),
                    ));
                }
                if failed > 0 {
                    parts.push(ts(
                        format!("{failed} 台巡检失败"),
                        format!("{failed} failed"),
                    ));
                }
                if parts.is_empty() {
                    ts(
                        format!("{} 台正常", self.targets.len()),
                        format!("{} ok", self.targets.len()),
                    )
                } else {
                    parts.join(" · ")
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote("/var/log/app.log"), "'/var/log/app.log'");
        // 内部单引号必须被拆成 '...'\''...'，否则可以逃出引号注入
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
        assert_eq!(shell_quote("'; rm -rf / #"), "''\\''; rm -rf / #'");
        // 空串也要有引号（`''` 是合法的空参数）
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn remote_path_rejects_traversal_and_pseudo_fs() {
        assert!(validate_remote_path("/var/log/app.log").is_ok());
        assert!(validate_remote_path("").is_err());
        assert!(validate_remote_path("/var/log/../../etc/shadow").is_err());
        assert!(validate_remote_path("/dev/sda1").is_err());
        assert!(validate_remote_path("/proc/self/environ").is_err());
        assert!(validate_remote_path("/sys/kernel").is_err());
        // 只是名字里含 dev 的路径不该被误伤
        assert!(validate_remote_path("/var/log/device.log").is_ok());
    }

    #[test]
    fn exec_action_rejects_blocked_command_at_save_time() {
        let mut a = TaskAction::ExecMulti {
            target: TaskTarget {
                profiles: vec!["p1".into()],
                ..Default::default()
            },
            command: "rm -rf /".into(),
            timeout_secs: 15,
            allow_mutating: false,
        };
        assert!(a.validate().is_err(), "Blocked 命令不得被保存为定时任务");

        // Confirm 级没问题（到点按策略决定是否执行），空命令不行
        if let TaskAction::ExecMulti { command, .. } = &mut a {
            *command = "systemctl restart nginx".into();
        }
        assert!(a.validate().is_ok());
        if let TaskAction::ExecMulti { command, .. } = &mut a {
            *command = "   ".into();
        }
        assert!(a.validate().is_err());
    }

    /// 日志路径**不是必填**（2026-09-21 用户反馈）：编辑器里已经没有这个字段，
    /// 新版界面存出来的形态（空巡检项 + 无路径）必须能保存 —— 旧校验会以
    /// 「已勾选日志巡检，请填写日志路径」把新建的 AI 巡检任务全部挡在保存之外。
    /// 但**存着**的路径仍要过安全校验：不能因为界面改版就放坏数据进执行路径。
    #[test]
    fn ai_inspect_does_not_require_a_log_path() {
        let base = |checks: Vec<CheckKind>, log_path: Option<String>| TaskAction::AiInspect {
            target: TaskTarget {
                groups: vec!["g1".into()],
                ..Default::default()
            },
            // 本用例只谈日志路径：AI 要求给一句合法的，别被 2026-09-22 的
            // 「要求必输」规则带偏（那条规则有自己的用例）
            prompt: "看看日志里有没有异常".into(),
            checks,
            thresholds: None,
            log_path,
            log_level: LogLevel::Error,
        };
        // 新版界面存出来的形态：空 checks + 无路径 -> 必须通过
        assert!(base(vec![], None).validate().is_ok());
        // 老任务勾了日志但没路径 -> 也不再拦（路径本就没地方补）
        assert!(base(vec![CheckKind::Log], None).validate().is_ok());
        assert!(base(vec![CheckKind::Log], Some("  ".into()))
            .validate()
            .is_ok());
        // 只查负载不填日志 -> 通过
        assert!(base(vec![CheckKind::Load], None).validate().is_ok());
        // 填了合法路径 -> 通过
        assert!(base(vec![], Some("/var/log/a.log".into()))
            .validate()
            .is_ok());
        // 路径本身非法照旧要拦：伪文件系统与穿越
        assert!(base(vec![CheckKind::Log], Some("/proc/uptime".into()))
            .validate()
            .is_err());
        assert!(base(vec![], Some("/var/log/../etc/passwd".into()))
            .validate()
            .is_err());
    }

    /// 「AI 要求」必输（2026-09-22 用户要求）。
    ///
    /// 拦在 `validate()` 上是为了**一处拦、三处生效**：保存（`task_draft_save`）、
    /// 启用（`task_toggle_enable`）、开跑（`runner` 执行前）三条路径都会调它。
    /// 纯空白同样算空 —— 从别处粘贴出来的换行不能当成「填了要求」。
    #[test]
    fn ai_inspect_requires_a_prompt() {
        let base = |prompt: &str| TaskAction::AiInspect {
            target: TaskTarget {
                groups: vec!["g1".into()],
                ..Default::default()
            },
            prompt: prompt.into(),
            checks: Vec::new(),
            thresholds: None,
            log_path: None,
            log_level: LogLevel::Error,
        };
        assert!(base("").validate().is_err(), "空 AI 要求不得被保存");
        assert!(
            base("  \n\t ").validate().is_err(),
            "纯空白等于空：换行 / 空格都不算填了要求"
        );
        assert!(base("看看磁盘有没有满").validate().is_ok());

        // 长度上限口径不变：按**字符**数算（CJK 一个字算一个），边界值本身合法
        assert!(base(&"字".repeat(MAX_PROMPT_CHARS)).validate().is_ok());
        assert!(base(&"字".repeat(MAX_PROMPT_CHARS + 1)).validate().is_err());
    }

    #[test]
    fn target_must_not_be_empty() {
        let a = TaskAction::MultiConnect {
            target: TaskTarget::default(),
            open_tabs: false,
            keep_alive_secs: 0,
        };
        assert!(a.validate().is_err());
        // 只有 exclude 也不算有目标
        let a = TaskAction::MultiConnect {
            target: TaskTarget {
                exclude: vec!["p1".into()],
                ..Default::default()
            },
            open_tabs: false,
            keep_alive_secs: 0,
        };
        assert!(a.validate().is_err());
    }

    #[test]
    fn trigger_next_after_is_aligned_and_strict() {
        // interval 对齐到整数倍
        let t = Trigger::Interval { every_secs: 900 };
        let next = t.next_after(1000).unwrap();
        assert_eq!(next, 1800);
        assert_eq!(next, next - next % 900);

        // 下限保护：10 秒的 interval 会被抬到 60 秒
        assert!(Trigger::Interval { every_secs: 10 }.validate().is_err());
        let t = Trigger::Interval { every_secs: 10 };
        let next = t.next_after(60).unwrap();
        assert_eq!(next, 120);
    }

    #[test]
    fn trigger_validate_rejects_bad_cron() {
        assert!(Trigger::Cron {
            expr: "99 * * * *".into(),
        }
        .validate()
        .is_err());
        assert!(Trigger::Cron {
            expr: "0 9 * * *".into(),
        }
        .validate()
        .is_ok());
    }

    /// 旧文件兼容：旧版本 cron 触发里的 `tz` 字段要被安全丢弃（serde 忽略
    /// 未知字段）；`manual` 的迁移在 `TaskStore::load`（要同步把 `enabled`
    /// 置 false，否则「仅手动」任务迁移后会真的到点自动跑）。
    /// 这里钉住 serde 层的宽容度。
    #[test]
    fn legacy_tz_field_is_ignored_on_trigger() {
        let t: Trigger =
            serde_json::from_str(r#"{"kind":"cron","expr":"0 9 * * *","tz":"UTC+08:00"}"#).unwrap();
        assert_eq!(t, Trigger::cron("0 9 * * *"));
    }

    #[test]
    fn notify_policy_defaults_avoid_alert_fatigue() {
        let p = NotifyPolicy::default();
        assert!(!p.on_ok, "正常也通知 = 告警疲劳");
        assert!(p.on_anomaly);
        assert!(p.on_error);
        assert_eq!(p.digest_window_secs, 300);
    }

    #[test]
    fn run_record_keeps_anomaly_and_failure_apart() {
        let rec = RunRecord {
            run_id: "r1".into(),
            task_id: "t1".into(),
            task_name: "巡检".into(),
            started_at: 10,
            finished_at: 12,
            status: RunStatus::Partial,
            machine: Vec::new(),
            targets: vec![
                TargetOutcome {
                    profile_id: "p1".into(),
                    profile_name: "prod-01".into(),
                    connected: true,
                    exec_ok: false,
                    anomaly: false,
                    summary: "连接超时".into(),
                    detail: None,
                },
                TargetOutcome {
                    profile_id: "p2".into(),
                    profile_name: "prod-02".into(),
                    connected: true,
                    exec_ok: true,
                    anomaly: true,
                    summary: "内存 94%".into(),
                    detail: None,
                },
            ],
            conclusion: None,
            findings: vec![Finding {
                kind: FindingKind::Anomaly,
                profile_id: "p2".into(),
                profile_name: "prod-02".into(),
                rule_id: "builtin.load".into(),
                fingerprint: "mem>90".into(),
                severity: Severity::Critical,
                message: "内存 94%".into(),
                evidence: None,
            }],
            note: None,
            session: None,
        };
        assert_eq!(rec.anomaly_count(), 1);
        assert_eq!(rec.failed_target_count(), 1);
        assert_eq!(rec.elapsed_ms(), 2000);
        let line = rec.one_line();
        assert!(line.contains('1'), "one_line={line}");
    }

    #[test]
    fn load_level_patterns_are_nonempty_and_distinct() {
        for l in LogLevel::all() {
            assert!(!l.default_pattern().is_empty());
        }
        assert_ne!(
            LogLevel::Error.default_pattern(),
            LogLevel::Warn.default_pattern()
        );
    }
}
