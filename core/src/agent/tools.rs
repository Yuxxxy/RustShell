//! Agent 工具集：定义、JSON Schema、参数校验与目标解析。
//!
//! 工具数量刻意收敛为 **12 个**：工具越少，模型选错的概率越低。
//! 之所以从 7 个扩到 12 个，是因为巡检域里有几类信号「高频、有独立安全默认、
//! UI 摘要比裸命令更清晰」（`read_file` / `inspect_service` / `inspect_ports` /
//! `check_certificate` / `find_process`）—— 它们都能用 `run_command` 凑出来，
//! 但那样模型得自己拼命令、我们得自己猜风险级，且轨迹上只有一行裸命令。
//!
//! 本模块只做「把模型的意图翻译成可执行的结构化参数」，**不碰网络**：
//! 执行由 [`crate::agent`] 的 `ToolBackend` 负责，于是参数校验可以单测到底。

use crate::ai::Risk;
use crate::connections::{ConnectionProfile, ConnectionStore};
use serde_json::{json, Value};

/// 工具名常量（稳定标识，写入 prompt / 审计 / 轨迹，不可随意改名）。
pub const TOOL_LIST_TARGETS: &str = "list_targets";
pub const TOOL_INSPECT_LOAD: &str = "inspect_load";
pub const TOOL_INSPECT_LOG: &str = "inspect_log";
pub const TOOL_RUN_COMMAND: &str = "run_command";
pub const TOOL_CHECK_REACHABILITY: &str = "check_reachability";
pub const TOOL_DOWNLOAD_FILE: &str = "download_file";
pub const TOOL_PROPOSE_TASK: &str = "propose_task";
pub const TOOL_READ_FILE: &str = "read_file";
pub const TOOL_INSPECT_SERVICE: &str = "inspect_service";
pub const TOOL_INSPECT_PORTS: &str = "inspect_ports";
pub const TOOL_CHECK_CERTIFICATE: &str = "check_certificate";
pub const TOOL_FIND_PROCESS: &str = "find_process";

/// 单次调用能覆盖的目标数上限。
///
/// 超出就报错并建议分批：50 台机器的原始输出会把上下文直接打爆，
/// 而模型看到的第 51 台起全是截断的垃圾。
pub const MAX_TARGETS_PER_CALL: usize = 50;

/// 目标解析的上限（与 [`MAX_TARGETS_PER_CALL`] 同源）。
pub const MAX_TARGETS: usize = MAX_TARGETS_PER_CALL;

/// 工具执行超时的默认值与上下限（秒）。
pub const DEFAULT_TOOL_TIMEOUT_SECS: u64 = 15;
pub const MAX_TOOL_TIMEOUT_SECS: u64 = 120;

/// `inspect_log` 默认读取的尾部行数。
pub const DEFAULT_TAIL_LINES: u32 = 500;
pub const MAX_TAIL_LINES: u32 = 5000;

/// `read_file` 的读取窗口（行）。默认给一屏，上限挡住「把 10 万行的日志
/// 整个拉进上下文」—— 那既挤爆预算，也让模型淹在噪声里。
pub const DEFAULT_READ_LINES: u32 = 200;
pub const MAX_READ_LINES: u32 = 2000;

/// 单行内容的字符上限：压缩过的 js / 单行 JSON 动辄几十万字符，
/// 不裁的话一行就能吃掉整个回注预算。
pub const MAX_LINE_CHARS: usize = 400;

/// `inspect_service` 的单元名长度上限。
pub const MAX_UNIT_NAME_LEN: usize = 128;

/// `find_process` 的进程名长度上限。
pub const MAX_PROCESS_NAME_LEN: usize = 64;

/// `check_certificate` 默认探测的端口。
pub const DEFAULT_CERT_PORT: u16 = 443;

/// 证书到期的告警阈值（天）：≤[`CERT_WARN_DAYS`] 警告，≤[`CERT_CRIT_DAYS`] 严重。
///
/// 30 / 7 是运维惯例（Let's Encrypt 的续期窗口是 30 天，7 天是不换就该报警了）。
pub const CERT_WARN_DAYS: i64 = 30;
pub const CERT_CRIT_DAYS: i64 = 7;

/// 目标选择器：三种写法互斥，优先级 `profiles > group > tag`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetSelector {
    /// 显式连接 id 清单（模型从 `list_targets` 拿到的 id）
    Profiles(Vec<String>),
    /// 分组 id 或**分组名**（解析为「该分组及其全部子分组下的所有连接」）
    Group(String),
    /// 连接标签
    Tag(String),
    /// 什么都不给 = 全部连接（仅 `list_targets` 允许）
    All,
}

impl TargetSelector {
    /// 从工具参数里取选择器。
    pub fn from_args(v: &Value) -> Result<Self, String> {
        if let Some(arr) = v.get("targets").and_then(|t| t.as_array()) {
            let mut ids = Vec::new();
            for item in arr {
                let s = item
                    .as_str()
                    .ok_or_else(|| "targets 必须是字符串数组".to_string())?;
                if !s.trim().is_empty() {
                    ids.push(s.trim().to_string());
                }
            }
            if !ids.is_empty() {
                return Ok(Self::Profiles(ids));
            }
        }
        // `profile`（单数）是模型很自然的写法，一并接受
        if let Some(s) = v.get("profile").and_then(|t| t.as_str()) {
            if !s.trim().is_empty() {
                return Ok(Self::Profiles(vec![s.trim().to_string()]));
            }
        }
        if let Some(g) = v.get("group").and_then(|t| t.as_str()) {
            if !g.trim().is_empty() {
                return Ok(Self::Group(g.trim().to_string()));
            }
        }
        if let Some(t) = v.get("tag").and_then(|t| t.as_str()) {
            if !t.trim().is_empty() {
                return Ok(Self::Tag(t.trim().to_string()));
            }
        }
        Ok(Self::All)
    }

    pub fn describe(&self) -> String {
        use crate::i18n::ts;
        match self {
            Self::Profiles(v) => ts(
                format!("{} 个指定连接", v.len()),
                format!("{} connection(s)", v.len()),
            ),
            Self::Group(g) => ts(format!("分组 {g}"), format!("group {g}")),
            Self::Tag(t) => ts(format!("标签 {t}"), format!("tag {t}")),
            Self::All => ts("全部连接".into(), "all connections".into()),
        }
    }
}

/// 目标解析失败的原因（**必须**能被回注给模型，让它自己纠正）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    /// 分组名歧义（命中多个）
    AmbiguousGroup {
        name: String,
        candidates: Vec<String>,
    },
    /// 分组名不存在
    UnknownGroup { name: String },
    /// 标签没有任何连接
    UnknownTag { name: String },
    /// 显式 id 里有不存在的连接
    UnknownProfiles { ids: Vec<String> },
    /// 解析结果为空
    Empty { selector: String },
    /// 超过单次上限
    TooMany { found: usize, max: usize },
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use crate::i18n::ts;
        match self {
            Self::AmbiguousGroup { name, candidates } => write!(
                f,
                "{}",
                ts(
                    format!(
                        "分组名「{name}」不唯一，候选：{}。请用分组 id 或更精确的名字重试",
                        candidates.join("、")
                    ),
                    format!(
                        "Group name '{name}' is ambiguous: {}. Retry with a group id or a more specific name",
                        candidates.join(", ")
                    ),
                )
            ),
            Self::UnknownGroup { name } => write!(
                f,
                "{}",
                ts(
                    format!("找不到分组「{name}」。请先用 list_targets 查看可用分组"),
                    format!("No such group '{name}'. Call list_targets first")
                )
            ),
            Self::UnknownTag { name } => write!(
                f,
                "{}",
                ts(
                    format!("标签「{name}」下没有任何连接"),
                    format!("No connection carries the tag '{name}'")
                )
            ),
            Self::UnknownProfiles { ids } => write!(
                f,
                "{}",
                ts(
                    format!(
                        "这些连接 id 不存在：{}。请先用 list_targets 取到有效 id",
                        ids.join("、")
                    ),
                    format!(
                        "Unknown connection ids: {}. Call list_targets to get valid ids",
                        ids.join(", ")
                    )
                )
            ),
            Self::Empty { selector } => write!(
                f,
                "{}",
                ts(
                    format!("{selector} 下没有任何连接"),
                    format!("{selector} has no connections")
                )
            ),
            Self::TooMany { found, max } => write!(
                f,
                "{}",
                ts(
                    format!("目标过多（{found} 台，单次上限 {max} 台）。请分批处理"),
                    format!("Too many targets ({found}, limit {max}). Split the request")
                )
            ),
        }
    }
}

/// 把选择器解析为连接 id 清单。
///
/// 结果的**顺序必须稳定**（分组树先序 + 组内原序）：同一句指令两次运行选出
/// 同一批、同一个顺序，巡检结果的对比才有意义。
///
/// 歧义**不做猜测**：命中多个分组时返回错误并列出候选，让模型澄清 ——
/// 猜错的代价是「查了错误的机器却报告一切正常」，比报错严重得多。
pub fn resolve_targets(
    store: &ConnectionStore,
    selector: &TargetSelector,
) -> Result<Vec<String>, ResolveError> {
    let ids = match selector {
        TargetSelector::Profiles(list) => {
            let missing: Vec<String> = list
                .iter()
                .filter(|id| store.profile(id).is_none())
                .cloned()
                .collect();
            if !missing.is_empty() {
                return Err(ResolveError::UnknownProfiles { ids: missing });
            }
            // 保序去重
            let mut seen = std::collections::HashSet::new();
            list.iter()
                .filter(|id| seen.insert((*id).clone()))
                .cloned()
                .collect::<Vec<_>>()
        }
        TargetSelector::Group(name) => {
            let gid = match_group(store, name)?;
            store.profile_ids_in_subtree(&gid)
        }
        TargetSelector::Tag(tag) => {
            let ids = store.profile_ids_by_tag(tag);
            if ids.is_empty() {
                return Err(ResolveError::UnknownTag { name: tag.clone() });
            }
            ids
        }
        TargetSelector::All => store.all_profile_ids(),
    };

    if ids.is_empty() {
        return Err(ResolveError::Empty {
            selector: selector.describe(),
        });
    }
    if ids.len() > MAX_TARGETS {
        return Err(ResolveError::TooMany {
            found: ids.len(),
            max: MAX_TARGETS,
        });
    }
    Ok(ids)
}

/// 按名字找分组：id 精确优先，其次名字精确，最后不区分大小写包含。
fn match_group(store: &ConnectionStore, name: &str) -> Result<String, ResolveError> {
    let want = name.trim();
    // id 精确
    if store.group(want).is_some() {
        return Ok(want.to_string());
    }
    // 名字精确
    if let Some(id) = store.group_id_by_name(want) {
        return Ok(id);
    }
    // 不区分大小写包含
    let lower = want.to_lowercase();
    let hits: Vec<(String, String)> = store
        .groups
        .iter()
        .filter(|g| g.name.to_lowercase().contains(&lower))
        .map(|g| (g.id.clone(), g.name.clone()))
        .collect();
    match hits.len() {
        0 => Err(ResolveError::UnknownGroup {
            name: want.to_string(),
        }),
        1 => Ok(hits[0].0.clone()),
        _ => Err(ResolveError::AmbiguousGroup {
            name: want.to_string(),
            candidates: hits.into_iter().map(|(_, n)| n).collect(),
        }),
    }
}

// 参数结构

/// 已校验的工具调用参数。
#[derive(Debug, Clone, PartialEq)]
pub enum ToolArgs {
    ListTargets {
        selector: TargetSelector,
    },
    InspectLoad {
        selector: TargetSelector,
        /// 关注的指标（空 = 全部）
        metrics: Vec<MetricKind>,
        timeout_secs: u64,
    },
    InspectLog {
        selector: TargetSelector,
        path: String,
        tail_lines: u32,
        /// 显式 pattern（`None` = 按 level 派生）
        pattern: Option<String>,
        level: crate::tasks::LogLevel,
        timeout_secs: u64,
    },
    RunCommand {
        selector: TargetSelector,
        command: String,
        timeout_secs: u64,
        /// 静态风险级（实际执行前还会再算一次，取更严的一档）
        risk: Risk,
    },
    CheckReachability {
        selector: TargetSelector,
        port: Option<u16>,
        timeout_secs: u64,
    },
    DownloadFile {
        profile_id: String,
        remote_path: String,
        local_dir: Option<String>,
    },
    ProposeTask {
        name: String,
        schedule: String,
        action: ProposalKind,
        /// 目标选择器（落库时由用户确认后再解析）
        selector: TargetSelector,
        prompt: Option<String>,
        command: Option<String>,
    },
    ReadFile {
        selector: TargetSelector,
        path: String,
        /// 起始行（1-based）
        start_line: u32,
        /// 读多少行（含起始行）
        max_lines: u32,
        timeout_secs: u64,
    },
    InspectService {
        selector: TargetSelector,
        /// systemd 单元名（`nginx` 或 `nginx.service`）
        unit: String,
        timeout_secs: u64,
    },
    InspectPorts {
        selector: TargetSelector,
        /// 只看这一个监听端口（`None` = 全量清单）
        port: Option<u16>,
        timeout_secs: u64,
    },
    CheckCertificate {
        selector: TargetSelector,
        /// 从服务器本机连哪个地址（默认 127.0.0.1）
        host: String,
        port: u16,
        /// SNI（一个端口挂多个站点时必须给，否则拿到的是默认站点证书）
        server_name: Option<String>,
        timeout_secs: u64,
    },
    FindProcess {
        selector: TargetSelector,
        /// 按进程名找（`None` 表示按端口找）
        name: Option<String>,
        /// 按监听端口找
        port: Option<u16>,
        timeout_secs: u64,
    },
}

/// `inspect_load` 关注的指标。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricKind {
    Cpu,
    Mem,
    Swap,
    Disk,
    Net,
    Top,
}

impl MetricKind {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "cpu" | "load" => Some(Self::Cpu),
            "mem" | "memory" => Some(Self::Mem),
            "swap" => Some(Self::Swap),
            "disk" | "df" => Some(Self::Disk),
            "net" | "network" => Some(Self::Net),
            "top" | "process" | "processes" => Some(Self::Top),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Cpu => "cpu",
            Self::Mem => "mem",
            Self::Swap => "swap",
            Self::Disk => "disk",
            Self::Net => "net",
            Self::Top => "top",
        }
    }
}

/// `propose_task` 的动作类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProposalKind {
    InspectAi,
    ExecMulti,
    Download,
}

impl ProposalKind {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "inspect_ai" | "ai_inspect" | "inspect" => Some(Self::InspectAi),
            "exec_multi" | "exec" | "command" => Some(Self::ExecMulti),
            "download" => Some(Self::Download),
            _ => None,
        }
    }

    pub fn wire(self) -> &'static str {
        match self {
            Self::InspectAi => "inspect_ai",
            Self::ExecMulti => "exec_multi",
            Self::Download => "download",
        }
    }
}

/// 所有工具的 Schema 清单（请求体里 `tools` 字段的内容）。
pub fn tool_schemas() -> Vec<Value> {
    all_tools()
        .iter()
        .map(|t| {
            json!({
                "type": "function",
                "function": {
                    "name": t.name(),
                    "description": t.description(),
                    "parameters": t.parameters(),
                }
            })
        })
        .collect()
}

/// 全部工具（用于遍历 Schema / 文档 / 能力开关）。
pub fn all_tools() -> Vec<Tool> {
    vec![
        Tool::ListTargets,
        Tool::InspectLoad,
        Tool::InspectLog,
        Tool::ReadFile,
        Tool::InspectService,
        Tool::InspectPorts,
        Tool::CheckCertificate,
        Tool::FindProcess,
        Tool::RunCommand,
        Tool::CheckReachability,
        Tool::DownloadFile,
        Tool::ProposeTask,
    ]
}

/// 工具枚举。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    ListTargets,
    InspectLoad,
    InspectLog,
    ReadFile,
    InspectService,
    InspectPorts,
    CheckCertificate,
    FindProcess,
    RunCommand,
    CheckReachability,
    DownloadFile,
    ProposeTask,
}

impl Tool {
    pub fn name(self) -> &'static str {
        match self {
            Self::ListTargets => TOOL_LIST_TARGETS,
            Self::InspectLoad => TOOL_INSPECT_LOAD,
            Self::InspectLog => TOOL_INSPECT_LOG,
            Self::ReadFile => TOOL_READ_FILE,
            Self::InspectService => TOOL_INSPECT_SERVICE,
            Self::InspectPorts => TOOL_INSPECT_PORTS,
            Self::CheckCertificate => TOOL_CHECK_CERTIFICATE,
            Self::FindProcess => TOOL_FIND_PROCESS,
            Self::RunCommand => TOOL_RUN_COMMAND,
            Self::CheckReachability => TOOL_CHECK_REACHABILITY,
            Self::DownloadFile => TOOL_DOWNLOAD_FILE,
            Self::ProposeTask => TOOL_PROPOSE_TASK,
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        all_tools().into_iter().find(|t| t.name() == name.trim())
    }

    /// 工具说明（给模型看，英文更省 token 且模型理解更稳）。
    pub fn description(self) -> &'static str {
        match self {
            Self::ListTargets => {
                "List available connections and groups. Call this first to learn ids before targeting anything."
            }
            Self::InspectLoad => {
                "Read CPU / memory / swap / disk / network / top processes on one or more servers (read-only)."
            }
            Self::InspectLog => {
                "Check a log file on one or more servers for anomalous lines (read-only)."
            }
            Self::ReadFile => {
                "Read lines from a file on one or more servers (read-only). Use this to see what a config or file actually says; use inspect_log to scan a log for abnormal lines."
            }
            Self::InspectService => {
                "Check one systemd unit on one or more servers: active state, enabled/disabled, restart count (read-only). Call once per unit."
            }
            Self::InspectPorts => {
                "List listening TCP ports and the busiest local ports by connection count on one or more servers (read-only)."
            }
            Self::CheckCertificate => {
                "Check the TLS certificate served on a port: expiry date, days left, subject and issuer (read-only)."
            }
            Self::FindProcess => {
                "Find processes on one or more servers by process name or by the TCP port they listen on (read-only)."
            }
            Self::RunCommand => {
                "Run a shell command on the connections named in `targets` (omit `targets` = every connection). Read-only commands run immediately; state-changing commands ask the user first; destructive commands are refused."
            }
            Self::CheckReachability => {
                "TCP-connect to a port on one or more servers to check whether they are reachable."
            }
            Self::DownloadFile => {
                "Download a file from one server to the local download directory."
            }
            Self::ProposeTask => {
                "Propose a scheduled task draft. This does NOT create the task; the user must confirm it."
            }
        }
    }

    /// 是否为变更/执行类（决定是否需要确认卡片）。
    pub fn mutating(self) -> bool {
        matches!(
            self,
            Self::RunCommand | Self::DownloadFile | Self::ProposeTask
        )
    }
}

impl Tool {
    /// 参数的 JSON Schema。
    fn parameters(self) -> Value {
        // target 三件套在所有批量工具里重复出现，抽出来避免三处漂移
        let target_props = |extra: Value| -> Value {
            let mut base = json!({
                "targets": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Connection ids. Prefer this; get ids from list_targets."
                },
                "group": {
                    "type": "string",
                    "description": "Group id or group name; includes all subgroups."
                },
                "tag": {
                    "type": "string",
                    "description": "Connection tag."
                }
            });
            if let (Some(b), Some(e)) = (base.as_object_mut(), extra.as_object()) {
                for (k, v) in e {
                    b.insert(k.clone(), v.clone());
                }
            }
            Value::Object(base.as_object().cloned().unwrap_or_default())
        };

        match self {
            Self::ListTargets => json!({
                "type": "object",
                "properties": {
                    "group": {"type": "string", "description": "Optional group filter."},
                    "tag": {"type": "string", "description": "Optional tag filter."}
                }
            }),
            Self::InspectLoad => json!({
                "type": "object",
                "properties": target_props(json!({
                    "metrics": {
                        "type": "array",
                        "items": {"type": "string", "enum": ["cpu","mem","swap","disk","net","top"]},
                        "description": "Which metrics to report. Omit for all."
                    },
                    "timeout_secs": {"type": "integer", "description": "Per-host timeout, default 15."}
                }))
            }),
            Self::InspectLog => json!({
                "type": "object",
                "properties": target_props(json!({
                    "path": {"type": "string", "description": "Absolute path of the log file."},
                    "tail_lines": {"type": "integer", "description": "How many trailing lines to scan, default 500."},
                    "pattern": {"type": "string", "description": "Extended regex to match. Overrides level."},
                    "level": {"type": "string", "enum": ["error","warn","fatal"], "description": "Severity preset, default error."},
                    "timeout_secs": {"type": "integer"}
                })),
                "required": ["path"]
            }),
            Self::RunCommand => json!({
                "type": "object",
                "properties": target_props(json!({
                    "command": {"type": "string", "description": "Shell command to run."},
                    "timeout_secs": {"type": "integer"}
                })),
                "required": ["command"]
            }),
            Self::CheckReachability => json!({
                "type": "object",
                "properties": target_props(json!({
                    "port": {"type": "integer", "description": "TCP port to probe; defaults to the target's SSH port."},
                    "timeout_secs": {"type": "integer"}
                }))
            }),
            Self::DownloadFile => json!({
                "type": "object",
                "properties": {
                    "target": {"type": "string", "description": "Connection id."},
                    "remote_path": {"type": "string", "description": "Absolute path on the server."},
                    "local_dir": {"type": "string", "description": "Optional local directory."}
                },
                "required": ["target", "remote_path"]
            }),
            Self::ProposeTask => json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "Task name."},
                    "schedule": {"type": "string", "description": "Cron expression, e.g. '0 9 * * *'."},
                    "action": {"type": "string", "enum": ["inspect_ai","exec_multi","download"]},
                    "group": {"type": "string"},
                    "tag": {"type": "string"},
                    "targets": {"type": "array", "items": {"type": "string"}},
                    "prompt": {"type": "string", "description": "Required when action is 'inspect_ai': the standing instruction the inspection must follow (it is the only thing that tells the model what to look for). Also used by action 'download' to carry the remote file path. Ignored for 'exec_multi'."},
                    "command": {"type": "string", "description": "Command for exec_multi."}
                },
                "required": ["name", "schedule", "action"]
            }),
            Self::ReadFile => json!({
                "type": "object",
                "properties": target_props(json!({
                    "path": {"type": "string", "description": "Absolute path of the file to read."},
                    "start_line": {"type": "integer", "description": "1-based first line to read; default 1."},
                    "max_lines": {"type": "integer", "description": "How many lines to read, default 200, max 2000."},
                    "timeout_secs": {"type": "integer"}
                })),
                "required": ["path"]
            }),
            Self::InspectService => json!({
                "type": "object",
                "properties": target_props(json!({
                    "unit": {"type": "string", "description": "systemd unit name, e.g. 'nginx' or 'nginx.service'."},
                    "timeout_secs": {"type": "integer"}
                })),
                "required": ["unit"]
            }),
            Self::InspectPorts => json!({
                "type": "object",
                "properties": target_props(json!({
                    "port": {"type": "integer", "description": "Only report this listening port. Omit for the full list."},
                    "timeout_secs": {"type": "integer"}
                }))
            }),
            Self::CheckCertificate => json!({
                "type": "object",
                "properties": target_props(json!({
                    "port": {"type": "integer", "description": "TLS port to check; default 443."},
                    "host": {"type": "string", "description": "Address to connect to from the server itself; default 127.0.0.1."},
                    "server_name": {"type": "string", "description": "SNI name to send, e.g. 'example.com'. Give it when the port serves several sites, otherwise the default site's certificate is returned."},
                    "timeout_secs": {"type": "integer"}
                }))
            }),
            Self::FindProcess => json!({
                "type": "object",
                "properties": target_props(json!({
                    "name": {"type": "string", "description": "Process name to search for (matches the executable name, partial match allowed)."},
                    "port": {"type": "integer", "description": "Find the process listening on this TCP port."},
                    "timeout_secs": {"type": "integer"}
                }))
            }),
        }
    }
}

/// 校验并归一化一次工具调用的参数。
///
/// 返回 `Err(String)` 即「参数非法」，调用方把这段文字**原样回注**给模型
/// 让它自纠（最多重试 2 次）—— 所以错误文案要说清「哪里不对、正确的写法」。
pub fn parse_args(tool: Tool, args: &Value) -> Result<ToolArgs, String> {
    let selector = TargetSelector::from_args(args)?;
    let timeout = parse_timeout(args)?;

    match tool {
        Tool::ListTargets => Ok(ToolArgs::ListTargets { selector }),
        Tool::InspectLoad => {
            let mut metrics = Vec::new();
            if let Some(arr) = args.get("metrics").and_then(|m| m.as_array()) {
                for m in arr {
                    let s = m.as_str().ok_or("metrics 必须是字符串数组")?;
                    match MetricKind::parse(s) {
                        Some(k) => metrics.push(k),
                        None => {
                            return Err(format!(
                                "未知指标「{s}」；可用值：cpu/mem/swap/disk/net/top"
                            ))
                        }
                    }
                }
            }
            Ok(ToolArgs::InspectLoad {
                selector,
                metrics,
                timeout_secs: timeout,
            })
        }
        Tool::InspectLog => {
            let path = args
                .get("path")
                .and_then(|p| p.as_str())
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .ok_or("缺少必填参数 path")?
                .to_string();
            // 路径校验与定时任务共用同一份实现（`..` 穿越、伪文件系统、长度）
            crate::tasks::validate_remote_path(&path)?;

            let tail_lines = match args.get("tail_lines").and_then(|t| t.as_u64()) {
                Some(n) => (n as u32).clamp(1, MAX_TAIL_LINES),
                None => DEFAULT_TAIL_LINES,
            };
            let level = match args.get("level").and_then(|l| l.as_str()) {
                Some(s) => match s.trim().to_ascii_lowercase().as_str() {
                    "error" => crate::tasks::LogLevel::Error,
                    "warn" | "warning" => crate::tasks::LogLevel::Warn,
                    "fatal" => crate::tasks::LogLevel::Fatal,
                    other => return Err(format!("未知级别「{other}」；可用值：error/warn/fatal")),
                },
                None => crate::tasks::LogLevel::Error,
            };
            let pattern = match args.get("pattern").and_then(|p| p.as_str()) {
                Some(p) if !p.trim().is_empty() => {
                    let p = p.trim();
                    if p.chars().count() > crate::tasks::MAX_PATTERN_LEN {
                        return Err(format!(
                            "pattern 过长（上限 {} 字符）",
                            crate::tasks::MAX_PATTERN_LEN
                        ));
                    }
                    Some(p.to_string())
                }
                _ => None,
            };
            Ok(ToolArgs::InspectLog {
                selector,
                path,
                tail_lines,
                pattern,
                level,
                timeout_secs: timeout,
            })
        }
        Tool::RunCommand => {
            let command = args
                .get("command")
                .and_then(|c| c.as_str())
                .map(str::trim)
                .filter(|c| !c.is_empty())
                .ok_or("缺少必填参数 command")?
                .to_string();
            if command.chars().count() > crate::tasks::types::MAX_COMMAND_LEN {
                return Err(format!(
                    "command 过长（上限 {} 字符）",
                    crate::tasks::types::MAX_COMMAND_LEN
                ));
            }
            // 风险级在这里算一次（用于回注与卡片），执行前还会再算一次
            let risk = crate::ai::assess_command(&command);
            Ok(ToolArgs::RunCommand {
                selector,
                command,
                timeout_secs: timeout,
                risk,
            })
        }
        Tool::CheckReachability => {
            let port = match args.get("port").and_then(|p| p.as_u64()) {
                Some(p) if (1..=65535).contains(&p) => Some(p as u16),
                Some(p) => return Err(format!("端口越界：{p}（1-65535）")),
                None => None,
            };
            Ok(ToolArgs::CheckReachability {
                selector,
                port,
                timeout_secs: timeout,
            })
        }
        Tool::DownloadFile => {
            let profile_id = args
                .get("target")
                .and_then(|t| t.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or("缺少必填参数 target")?
                .to_string();
            let remote_path = args
                .get("remote_path")
                .and_then(|t| t.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or("缺少必填参数 remote_path")?
                .to_string();
            crate::tasks::validate_remote_path(&remote_path)?;
            let local_dir = args
                .get("local_dir")
                .and_then(|t| t.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            Ok(ToolArgs::DownloadFile {
                profile_id,
                remote_path,
                local_dir,
            })
        }
        Tool::ProposeTask => {
            let name = args
                .get("name")
                .and_then(|t| t.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or("缺少必填参数 name")?
                .to_string();
            let schedule = args
                .get("schedule")
                .and_then(|t| t.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or("缺少必填参数 schedule")?
                .to_string();
            // 草案里的 cron 必须当场校验：让模型现在就改正，
            // 而不是等用户点了「确认」才发现表达式是错的
            crate::tasks::parse_cron(&schedule)
                .map_err(|e| format!("schedule 不是合法 cron：{e}"))?;
            let action = args
                .get("action")
                .and_then(|t| t.as_str())
                .and_then(ProposalKind::parse)
                .ok_or("action 必须是 inspect_ai / exec_multi / download 之一")?;
            let prompt = args
                .get("prompt")
                .and_then(|t| t.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            let command = args
                .get("command")
                .and_then(|t| t.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            if action == ProposalKind::ExecMulti && command.is_none() {
                return Err("action=exec_multi 时必须给 command".into());
            }
            if selector == TargetSelector::All {
                return Err("请指定 group / tag / targets 之一".into());
            }
            Ok(ToolArgs::ProposeTask {
                name,
                schedule,
                action,
                selector,
                prompt,
                command,
            })
        }
        Tool::ReadFile => {
            let path = args
                .get("path")
                .and_then(|p| p.as_str())
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .ok_or("缺少必填参数 path")?
                .to_string();
            // 与 inspect_log / download_file 共用同一份路径校验（`..` 穿越、伪文件系统、长度）
            crate::tasks::validate_remote_path(&path)?;
            let start_line = match args.get("start_line").and_then(|v| v.as_u64()) {
                Some(0) => return Err("start_line 从 1 开始计数，不能是 0".into()),
                Some(n) => n.min(u32::MAX as u64) as u32,
                None => 1,
            };
            let max_lines = match args.get("max_lines").and_then(|v| v.as_u64()) {
                Some(0) => return Err("max_lines 必须大于 0".into()),
                Some(n) => n.min(MAX_READ_LINES as u64) as u32,
                None => DEFAULT_READ_LINES,
            };
            Ok(ToolArgs::ReadFile {
                selector,
                path,
                start_line,
                max_lines,
                timeout_secs: timeout,
            })
        }
        Tool::InspectService => {
            let unit = args
                .get("unit")
                .and_then(|u| u.as_str())
                .map(str::trim)
                .filter(|u| !u.is_empty())
                .ok_or("缺少必填参数 unit（如 nginx 或 nginx.service）")?
                .to_string();
            validate_unit_name(&unit)?;
            Ok(ToolArgs::InspectService {
                selector,
                unit,
                timeout_secs: timeout,
            })
        }
        Tool::InspectPorts => {
            let port = parse_optional_port(args.get("port"), "port")?;
            Ok(ToolArgs::InspectPorts {
                selector,
                port,
                timeout_secs: timeout,
            })
        }
        Tool::CheckCertificate => {
            let port = match parse_optional_port(args.get("port"), "port")? {
                Some(p) => p,
                None => DEFAULT_CERT_PORT,
            };
            let host = args
                .get("host")
                .and_then(|h| h.as_str())
                .map(str::trim)
                .filter(|h| !h.is_empty())
                .unwrap_or("127.0.0.1")
                .to_string();
            validate_probe_host(&host, "host")?;
            let server_name = match args
                .get("server_name")
                .and_then(|s| s.as_str())
                .map(str::trim)
            {
                Some(s) if !s.is_empty() => {
                    validate_probe_host(s, "server_name")?;
                    Some(s.to_string())
                }
                _ => None,
            };
            Ok(ToolArgs::CheckCertificate {
                selector,
                host,
                port,
                server_name,
                timeout_secs: timeout,
            })
        }
        Tool::FindProcess => {
            let name = match args.get("name").and_then(|n| n.as_str()).map(str::trim) {
                Some(n) if !n.is_empty() => {
                    validate_process_name(n)?;
                    Some(n.to_string())
                }
                _ => None,
            };
            let port = parse_optional_port(args.get("port"), "port")?;
            if name.is_none() && port.is_none() {
                return Err("请给 name 或 port 之一（按进程名找 / 按监听端口找）".into());
            }
            Ok(ToolArgs::FindProcess {
                selector,
                name,
                port,
                timeout_secs: timeout,
            })
        }
    }
}

/// 可选端口参数（`1..=65535`；缺省 / 空值 = `None`）。
fn parse_optional_port(v: Option<&Value>, field: &str) -> Result<Option<u16>, String> {
    match v.and_then(|p| p.as_u64()) {
        Some(p) if (1..=65535).contains(&p) => Ok(Some(p as u16)),
        Some(p) => Err(format!("{field} 越界：{p}（1-65535）")),
        None => Ok(None),
    }
}

/// 远端地址（主机名 / IP / `[IPv6]`）的校验：证书探针会把它拼进远端命令，
/// 字符集收紧到「地址合法字符」，再由执行侧过 `shell_quote` 形成第二道防线。
fn validate_probe_host(s: &str, field: &str) -> Result<(), String> {
    if s.chars().count() > 253 {
        return Err(format!("{field} 过长（上限 253 字符）"));
    }
    if !s
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || ".-_:[]".contains(c))
    {
        return Err(format!(
            "{field}「{s}」含非法字符；只允许字母数字与 . - _ : [ ]"
        ));
    }
    Ok(())
}

/// systemd 单元名校验。
///
/// 单元名没有「转义」一说（`systemctl` 不认识引号里的转义序列），所以这里
/// 按字符集白名单收紧：合法字符之外的输入直接判错让模型改，而不是拼进命令里赌。
fn validate_unit_name(s: &str) -> Result<(), String> {
    let n = s.chars().count();
    if n == 0 || n > MAX_UNIT_NAME_LEN {
        return Err(format!("unit 长度须在 1-{MAX_UNIT_NAME_LEN} 之间"));
    }
    if s.starts_with('-')
        || !s
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._@:\\-".contains(c))
    {
        return Err(format!("unit「{s}」含非法字符；示例：nginx.service"));
    }
    Ok(())
}

/// 进程名校验：它会作为 `pgrep` 的模式串，故只留普通进程名用得上的字符。
fn validate_process_name(s: &str) -> Result<(), String> {
    let n = s.chars().count();
    if n == 0 || n > MAX_PROCESS_NAME_LEN {
        return Err(format!("name 长度须在 1-{MAX_PROCESS_NAME_LEN} 之间"));
    }
    if !s
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "._@:+-".contains(c))
    {
        return Err(format!("name「{s}」含非法字符；请用进程名（如 nginx）"));
    }
    Ok(())
}

fn parse_timeout(args: &Value) -> Result<u64, String> {
    match args.get("timeout_secs").and_then(|t| t.as_u64()) {
        Some(0) => Err("timeout_secs 必须大于 0".into()),
        Some(n) if n > MAX_TOOL_TIMEOUT_SECS => Err(format!(
            "timeout_secs 过大（上限 {MAX_TOOL_TIMEOUT_SECS} 秒）"
        )),
        Some(n) => Ok(n),
        None => Ok(DEFAULT_TOOL_TIMEOUT_SECS),
    }
}

/// [`call_summary`] 的入参。
///
/// 打成结构体是因为这里会同时出现「工具名」「调用摘要」等一堆 `&str`，
/// 位置写错编译器不报错，只会让轨迹上显示错的东西。
#[derive(Debug, Clone, Copy)]
pub struct SummaryInput<'a> {
    /// 工具名（当前仅用于将来按工具细化摘要；参数里已含足够信息）
    pub tool: Tool,
    pub args: &'a ToolArgs,
    /// 目标连接的展示名（`TargetSelector::Profiles` 时用它代替 id）
    pub names: &'a [String],
}

/// 工具轨迹上显示的一行摘要（UI 直接渲染，renderer 不做解析）。
pub fn call_summary(input: SummaryInput<'_>) -> String {
    let SummaryInput {
        tool: _,
        args,
        names,
    } = input;
    let target_text = match args {
        ToolArgs::ListTargets { selector }
        | ToolArgs::InspectLoad { selector, .. }
        | ToolArgs::InspectLog { selector, .. }
        | ToolArgs::ReadFile { selector, .. }
        | ToolArgs::InspectService { selector, .. }
        | ToolArgs::InspectPorts { selector, .. }
        | ToolArgs::CheckCertificate { selector, .. }
        | ToolArgs::FindProcess { selector, .. }
        | ToolArgs::RunCommand { selector, .. }
        | ToolArgs::CheckReachability { selector, .. } => match selector {
            TargetSelector::Profiles(_) => names.join(", "),
            other => other.describe(),
        },
        ToolArgs::DownloadFile { profile_id, .. } => profile_id.clone(),
        ToolArgs::ProposeTask { selector, .. } => selector.describe(),
    };
    match args {
        ToolArgs::InspectLog { path, .. } | ToolArgs::ReadFile { path, .. } => {
            let head = format!("{target_text}:{path}");
            // 只有窗口不是默认那一屏时才缀上范围（默认情况下缀了反而是噪声）
            match args {
                ToolArgs::ReadFile {
                    start_line,
                    max_lines,
                    ..
                } if *start_line != 1 || *max_lines != DEFAULT_READ_LINES => {
                    format!("{head} · L{start_line}+{max_lines}")
                }
                _ => head,
            }
        }
        ToolArgs::InspectService { unit, .. } => format!("{target_text} · {unit}"),
        ToolArgs::CheckCertificate { host, port, .. } => format!("{target_text} · {host}:{port}"),
        ToolArgs::InspectPorts { port: Some(p), .. } => format!("{target_text} · :{p}"),
        ToolArgs::InspectPorts { port: None, .. } => target_text,
        ToolArgs::FindProcess { name, port, .. } => {
            let mut parts = vec![target_text];
            if let Some(n) = name {
                parts.push(n.clone());
            }
            if let Some(p) = port {
                parts.push(format!(":{p}"));
            }
            parts.join(" · ")
        }
        ToolArgs::RunCommand { command, .. } => {
            let one_line = command.lines().next().unwrap_or("").trim();
            format!("{target_text} · {one_line}")
        }
        _ => target_text,
    }
    .trim()
    .to_string()
}

/// 用连接 id 查出展示名（轨迹与运行记录里显示名字比显示 id 可读）。
pub fn display_names(store: &ConnectionStore, ids: &[String]) -> Vec<String> {
    ids.iter()
        .map(|id| {
            store
                .profile(id)
                .map(|p: &ConnectionProfile| p.name.clone())
                .unwrap_or_else(|| id.clone())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connections::{ConnectionGroup, ConnectionStore};

    fn store() -> ConnectionStore {
        let mut s = ConnectionStore {
            groups: vec![
                ConnectionGroup::new("生产组"),
                ConnectionGroup::new("生产组-华东"),
                ConnectionGroup::new("测试组"),
            ],
            ..Default::default()
        };
        // 建立父子关系：华东挂到生产组下
        s.groups[1].parent = Some(s.groups[0].id.clone());
        let g_prod = s.groups[0].id.clone();
        let g_east = s.groups[1].id.clone();
        let g_test = s.groups[2].id.clone();

        let mut p1 = ConnectionProfile::new("prod-01", "10.0.0.1", "root");
        p1.group_id = Some(g_prod.clone());
        p1.tags = vec!["prod".into()];
        let mut p2 = ConnectionProfile::new("prod-02", "10.0.0.2", "root");
        p2.group_id = Some(g_east.clone());
        p2.tags = vec!["prod".into(), "db".into()];
        let mut p3 = ConnectionProfile::new("test-01", "10.0.1.1", "dev");
        p3.group_id = Some(g_test);
        s.profiles = vec![p1, p2, p3];
        s
    }

    fn id_of(s: &ConnectionStore, name: &str) -> String {
        s.profiles
            .iter()
            .find(|p| p.name == name)
            .expect("测试数据里必须有该连接")
            .id
            .clone()
    }

    #[test]
    fn group_selector_includes_subgroups_and_is_ordered() {
        let s = store();
        let ids = resolve_targets(&s, &TargetSelector::Group("生产组".into())).unwrap();
        // 含子分组：prod-01（生产组）+ prod-02（生产组-华东）
        assert_eq!(ids.len(), 2, "分组必须含子分组：{ids:?}");
        assert_eq!(ids[0], id_of(&s, "prod-01"));
        assert_eq!(ids[1], id_of(&s, "prod-02"));
        // 顺序稳定：再解析一次结果相同
        assert_eq!(
            ids,
            resolve_targets(&s, &TargetSelector::Group("生产组".into())).unwrap()
        );
    }

    #[test]
    fn group_can_be_addressed_by_id_exact_name_or_fuzzy() {
        let s = store();
        let by_name = resolve_targets(&s, &TargetSelector::Group("测试组".into())).unwrap();
        assert_eq!(by_name.len(), 1);

        let gid = s.groups[2].id.clone();
        let by_id = resolve_targets(&s, &TargetSelector::Group(gid)).unwrap();
        assert_eq!(by_id, by_name, "按 id 与按名必须等价");

        // 模糊匹配唯一命中
        let fuzzy = resolve_targets(&s, &TargetSelector::Group("测试".into())).unwrap();
        assert_eq!(fuzzy, by_name);
    }

    #[test]
    fn ambiguous_group_name_is_an_error_not_a_guess() {
        // 造一个**真正**的歧义：两个分组名都包含查询串，且都不与查询串精确相等
        // （精确名优先，所以「精确命中」永远不算歧义 —— 那是用户的本意）。
        let mut s = ConnectionStore {
            groups: vec![
                ConnectionGroup::new("prod组"),
                ConnectionGroup::new("prod组-华东"),
            ],
            ..Default::default()
        };
        // 连接必须真的落在分组里：`profiles_in_group` 按 `group_id` 找，
        // 建了分组却把连接留在未分组状态是这份夹具最初踩的坑。
        let gid = s.groups[0].id.clone();
        let mut p1 = ConnectionProfile::new("p1", "h1", "u");
        p1.group_id = Some(gid);
        s.profiles.push(p1);

        // "prod" 同时包含于两个分组名
        let err = resolve_targets(&s, &TargetSelector::Group("prod".into()));
        match err {
            Err(ResolveError::AmbiguousGroup { name, candidates }) => {
                assert_eq!(name, "prod");
                assert_eq!(candidates.len(), 2, "候选要列全：{candidates:?}");
            }
            other => panic!("歧义必须报错并给候选，而不是猜一个：{other:?}"),
        }

        // 精确名不是歧义
        let ids = resolve_targets(&s, &TargetSelector::Group("prod组".into())).unwrap();
        assert_eq!(ids.len(), 1, "精确命中该分组下的连接");
    }

    #[test]
    fn unknown_group_tag_and_profile_errors_are_actionable() {
        let s = store();
        let e = resolve_targets(&s, &TargetSelector::Group("不存在的组".into())).unwrap_err();
        assert!(matches!(e, ResolveError::UnknownGroup { .. }));
        assert!(
            e.to_string().contains("list_targets"),
            "错误文案要告诉模型下一步怎么做：{e}"
        );

        let e = resolve_targets(&s, &TargetSelector::Tag("nope".into())).unwrap_err();
        assert!(matches!(e, ResolveError::UnknownTag { .. }));

        let e = resolve_targets(
            &s,
            &TargetSelector::Profiles(vec!["ghost".into(), "phantom".into()]),
        )
        .unwrap_err();
        assert!(matches!(e, ResolveError::UnknownProfiles { .. }));
        assert!(e.to_string().contains("ghost"));
    }

    #[test]
    fn tag_selector_filters_across_groups() {
        let s = store();
        let prod = resolve_targets(&s, &TargetSelector::Tag("prod".into())).unwrap();
        assert_eq!(prod.len(), 2, "标签是横切筛选，跨分组");
        let db = resolve_targets(&s, &TargetSelector::Tag("db".into())).unwrap();
        assert_eq!(db.len(), 1);
        assert_eq!(db[0], id_of(&s, "prod-02"));
        // 大小写不敏感
        assert_eq!(
            resolve_targets(&s, &TargetSelector::Tag("PROD".into())).unwrap(),
            prod
        );
    }

    #[test]
    fn all_selector_covers_everything_and_errors_when_store_empty() {
        let s = store();
        assert_eq!(resolve_targets(&s, &TargetSelector::All).unwrap().len(), 3);
        let empty = ConnectionStore::default();
        let e = resolve_targets(&empty, &TargetSelector::All).unwrap_err();
        assert!(
            matches!(e, ResolveError::Empty { .. }),
            "空集合要报错而不是静默成功"
        );
    }

    #[test]
    fn target_count_is_capped() {
        let mut s = ConnectionStore::default();
        for i in 0..(MAX_TARGETS + 5) {
            s.profiles
                .push(ConnectionProfile::new(format!("p{i}"), "h", "u"));
        }
        let e = resolve_targets(&s, &TargetSelector::All).unwrap_err();
        assert!(
            matches!(e, ResolveError::TooMany { .. }),
            "超过上限必须报错并建议分批：{e:?}"
        );
    }

    #[test]
    fn selector_priority_prefers_explicit_ids() {
        // targets 优先于 group（同时给出时以显式 id 为准）
        let v = json!({"targets": ["a"], "group": "生产组"});
        assert_eq!(
            TargetSelector::from_args(&v).unwrap(),
            TargetSelector::Profiles(vec!["a".into()])
        );
        // 单数 profile 也接受
        let v = json!({"profile": "b"});
        assert_eq!(
            TargetSelector::from_args(&v).unwrap(),
            TargetSelector::Profiles(vec!["b".into()])
        );
        // 空数组不算「给了 targets」
        let v = json!({"targets": [], "tag": "prod"});
        assert_eq!(
            TargetSelector::from_args(&v).unwrap(),
            TargetSelector::Tag("prod".into())
        );
        // 都没有 = All
        assert_eq!(
            TargetSelector::from_args(&json!({})).unwrap(),
            TargetSelector::All
        );
    }

    #[test]
    fn inspect_log_parses_and_validates() {
        let t = Tool::InspectLog;
        let ok = parse_args(
            t,
            &json!({"group": "生产组", "path": "/var/log/app.log", "level": "warn", "tail_lines": 200}),
        )
        .unwrap();
        match ok {
            ToolArgs::InspectLog {
                path,
                tail_lines,
                level,
                pattern,
                ..
            } => {
                assert_eq!(path, "/var/log/app.log");
                assert_eq!(tail_lines, 200);
                assert_eq!(level, crate::tasks::LogLevel::Warn);
                assert!(pattern.is_none());
            }
            other => panic!("解析结果不对：{other:?}"),
        }

        // 缺 path
        assert!(parse_args(t, &json!({"group": "g"}))
            .unwrap_err()
            .contains("path"));
        // 穿越路径
        assert!(parse_args(t, &json!({"path": "/var/../../etc/shadow"}))
            .unwrap_err()
            .contains(".."));
        // 伪文件系统
        assert!(parse_args(t, &json!({"path": "/proc/self/environ"})).is_err());
        // 未知 level
        assert!(parse_args(t, &json!({"path": "/a.log", "level": "info"}))
            .unwrap_err()
            .contains("error/warn/fatal"));
        // tail_lines 越界被钳制（而不是报错：这是可容忍的过度请求）
        match parse_args(t, &json!({"path": "/a.log", "tail_lines": 999999})).unwrap() {
            ToolArgs::InspectLog { tail_lines, .. } => assert_eq!(tail_lines, MAX_TAIL_LINES),
            other => panic!("{other:?}"),
        }
        // pattern 显式给出优先，且长度受限
        match parse_args(
            t,
            &json!({"path": "/a.log", "pattern": "status=5\\d\\d", "level": "fatal"}),
        )
        .unwrap()
        {
            ToolArgs::InspectLog { pattern, .. } => {
                assert_eq!(pattern.as_deref(), Some("status=5\\d\\d"))
            }
            other => panic!("{other:?}"),
        }
        let long = "x".repeat(crate::tasks::MAX_PATTERN_LEN + 1);
        assert!(parse_args(t, &json!({"path": "/a.log", "pattern": long})).is_err());
    }

    #[test]
    fn run_command_carries_risk_and_rejects_empty() {
        let t = Tool::RunCommand;
        match parse_args(t, &json!({"group": "g", "command": "df -h"})).unwrap() {
            ToolArgs::RunCommand { risk, .. } => assert_eq!(risk, Risk::Safe),
            other => panic!("{other:?}"),
        }
        match parse_args(
            t,
            &json!({"group": "g", "command": "systemctl restart nginx"}),
        )
        .unwrap()
        {
            ToolArgs::RunCommand { risk, .. } => assert_eq!(risk, Risk::Confirm),
            other => panic!("{other:?}"),
        }
        match parse_args(t, &json!({"group": "g", "command": "mkfs.ext4 /dev/sda1"})).unwrap() {
            ToolArgs::RunCommand { risk, .. } => assert_eq!(risk, Risk::Blocked),
            other => panic!("{other:?}"),
        }
        assert!(parse_args(t, &json!({"command": "  "})).is_err());
        assert!(parse_args(t, &json!({"group": "g"})).is_err());
    }

    #[test]
    fn timeout_bounds_are_enforced() {
        assert!(parse_args(Tool::InspectLoad, &json!({"group": "g", "timeout_secs": 0})).is_err());
        assert!(parse_args(
            Tool::InspectLoad,
            &json!({"group": "g", "timeout_secs": MAX_TOOL_TIMEOUT_SECS + 1})
        )
        .is_err());
        match parse_args(Tool::InspectLoad, &json!({"group": "g"})).unwrap() {
            ToolArgs::InspectLoad { timeout_secs, .. } => {
                assert_eq!(timeout_secs, DEFAULT_TOOL_TIMEOUT_SECS)
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn metrics_list_is_validated() {
        match parse_args(
            Tool::InspectLoad,
            &json!({"group": "g", "metrics": ["cpu", "memory", "df"]}),
        )
        .unwrap()
        {
            ToolArgs::InspectLoad { metrics, .. } => assert_eq!(
                metrics,
                vec![MetricKind::Cpu, MetricKind::Mem, MetricKind::Disk]
            ),
            other => panic!("{other:?}"),
        }
        let e = parse_args(
            Tool::InspectLoad,
            &json!({"group": "g", "metrics": ["gpu"]}),
        )
        .unwrap_err();
        assert!(e.contains("cpu/mem/swap/disk/net/top"), "err={e}");
    }

    #[test]
    fn propose_task_validates_cron_and_action() {
        let t = Tool::ProposeTask;
        let ok = parse_args(
            t,
            &json!({"name": "早间巡检", "schedule": "0 9 * * *", "action": "inspect_ai", "group": "生产组"}),
        );
        assert!(ok.is_ok(), "{ok:?}");

        // 坏 cron 必须当场拒绝（否则用户点了确认才发现）
        let e = parse_args(
            t,
            &json!({"name": "x", "schedule": "99 * * * *", "action": "inspect_ai", "group": "g"}),
        )
        .unwrap_err();
        assert!(e.contains("cron"), "err={e}");

        // exec_multi 缺 command
        assert!(parse_args(
            t,
            &json!({"name": "x", "schedule": "0 9 * * *", "action": "exec_multi", "group": "g"})
        )
        .is_err());

        // 未知 action
        assert!(parse_args(
            t,
            &json!({"name": "x", "schedule": "0 9 * * *", "action": "reboot", "group": "g"})
        )
        .is_err());

        // 不给目标
        assert!(parse_args(
            t,
            &json!({"name": "x", "schedule": "0 9 * * *", "action": "inspect_ai"})
        )
        .is_err());

        // download 不需要 command
        assert!(parse_args(
            t,
            &json!({"name": "x", "schedule": "0 9 * * *", "action": "download", "tag": "prod"})
        )
        .is_ok());
    }

    #[test]
    fn download_requires_target_and_valid_path() {
        let t = Tool::DownloadFile;
        assert!(parse_args(t, &json!({"target": "p1", "remote_path": "/var/log/a.log"})).is_ok());
        assert!(parse_args(t, &json!({"remote_path": "/a.log"})).is_err());
        assert!(parse_args(t, &json!({"target": "p1"})).is_err());
        assert!(parse_args(t, &json!({"target": "p1", "remote_path": "/dev/sda"})).is_err());
    }

    #[test]
    fn schemas_are_wellformed_and_cover_every_tool() {
        let schemas = tool_schemas();
        assert_eq!(schemas.len(), 12, "工具数量变了要同步更新文档与画像");
        for s in &schemas {
            assert_eq!(s["type"], "function");
            let f = &s["function"];
            let name = f["name"].as_str().expect("每个工具都要有 name");
            assert!(Tool::from_name(name).is_some(), "name 必须能反查：{name}");
            assert!(!f["description"].as_str().unwrap_or("").is_empty());
            let params = &f["parameters"];
            assert_eq!(params["type"], "object", "{name} 的参数必须是 object");
            assert!(params["properties"].is_object(), "{name} 缺 properties");
        }
    }

    #[test]
    fn mutating_flags_match_the_safety_spec() {
        // 只有这三个是变更/执行类；巡检类全部只读
        assert!(Tool::RunCommand.mutating());
        assert!(Tool::DownloadFile.mutating());
        assert!(Tool::ProposeTask.mutating());
        for t in [
            Tool::ListTargets,
            Tool::InspectLoad,
            Tool::InspectLog,
            Tool::ReadFile,
            Tool::InspectService,
            Tool::InspectPorts,
            Tool::CheckCertificate,
            Tool::FindProcess,
            Tool::CheckReachability,
        ] {
            assert!(!t.mutating(), "{:?} 应当是只读的", t);
        }
    }

    /// 五个新工具的参数校验：**必须**在进远端之前拦下非法值。
    ///
    /// 逐个盯的是「拼进远端命令的那个串」——路径穿越、单元名与进程名里的
    /// 元字符，都是注入面的入口，漏一个就不是「模型调皮」而是远端执行任意命令。
    #[test]
    fn new_tools_reject_hostile_arguments() {
        // read_file：与 inspect_log / download_file 同一份路径校验
        assert!(parse_args(
            Tool::ReadFile,
            &json!({"targets": ["p1"], "path": "/etc/nginx/nginx.conf"})
        )
        .is_ok());
        assert!(
            parse_args(Tool::ReadFile, &json!({"targets": ["p1"]})).is_err(),
            "缺 path"
        );
        assert!(
            parse_args(
                Tool::ReadFile,
                &json!({"targets": ["p1"], "path": "/etc/../../dev/sda"})
            )
            .is_err(),
            "`..` 穿越必须在参数层就被拦下"
        );
        // 带 `;` 的路径不是 `validate_remote_path` 的职责（它只管穿越与伪文件系统）。
        // 注入防线在命令拼装层：要么当时拒掉，要么[必须]被整体 quote 成单个词。
        match parse_args(
            Tool::ReadFile,
            &json!({"targets": ["p1"], "path": "/etc/passwd; rm -rf /"}),
        ) {
            Err(_) => {}
            Ok(ToolArgs::ReadFile { path, .. }) => {
                let cmd = crate::runner::inspect::read_probe_command(&path, 1, 10);
                assert!(
                    cmd.contains("'/etc/passwd; rm -rf /'"),
                    "分号不能逃出单引号：{cmd}"
                );
            }
            Ok(other) => panic!("解析成了 {other:?}"),
        }
        // 行窗口：默认一屏，越界（0）当场报错而不是静默改成 1
        let args = parse_args(
            Tool::ReadFile,
            &json!({"targets": ["p1"], "path": "/etc/hosts"}),
        )
        .expect("默认窗口应当合法");
        match args {
            ToolArgs::ReadFile {
                start_line,
                max_lines,
                ..
            } => {
                assert_eq!(start_line, 1);
                assert_eq!(max_lines, DEFAULT_READ_LINES);
            }
            other => panic!("解析成了 {other:?}"),
        }
        assert!(parse_args(
            Tool::ReadFile,
            &json!({"targets": ["p1"], "path": "/etc/hosts", "start_line": 0})
        )
        .is_err());
        assert!(parse_args(
            Tool::ReadFile,
            &json!({"targets": ["p1"], "path": "/etc/hosts", "max_lines": 0})
        )
        .is_err());

        // inspect_service：单元名里的元字符与选项前缀
        assert!(parse_args(
            Tool::InspectService,
            &json!({"targets": ["p1"], "unit": "nginx.service"})
        )
        .is_ok());
        assert!(parse_args(
            Tool::InspectService,
            &json!({"targets": ["p1"], "unit": "nginx"})
        )
        .is_ok());
        assert!(parse_args(
            Tool::InspectService,
            &json!({"targets": ["p1"], "unit": "nginx; rm -rf /"})
        )
        .is_err());
        assert!(parse_args(
            Tool::InspectService,
            &json!({"targets": ["p1"], "unit": "--all"})
        )
        .is_err());
        assert!(parse_args(Tool::InspectService, &json!({"targets": ["p1"]})).is_err());

        // check_certificate：默认端口与默认连接地址，SNI 可选
        let args = parse_args(Tool::CheckCertificate, &json!({"targets": ["p1"]}))
            .expect("只给目标也应当合法");
        match args {
            ToolArgs::CheckCertificate {
                host,
                port,
                server_name,
                ..
            } => {
                assert_eq!(port, DEFAULT_CERT_PORT);
                assert_eq!(host, "127.0.0.1");
                assert!(server_name.is_none());
            }
            other => panic!("解析成了 {other:?}"),
        }
        assert!(parse_args(
            Tool::CheckCertificate,
            &json!({"targets": ["p1"], "port": 70000})
        )
        .is_err());
        assert!(parse_args(
            Tool::CheckCertificate,
            &json!({"targets": ["p1"], "server_name": "a.com; id"})
        )
        .is_err());

        // inspect_ports：端口过滤可选，越界报错
        assert!(parse_args(Tool::InspectPorts, &json!({"targets": ["p1"]})).is_ok());
        assert!(parse_args(Tool::InspectPorts, &json!({"targets": ["p1"], "port": 80})).is_ok());
        assert!(parse_args(Tool::InspectPorts, &json!({"targets": ["p1"], "port": 0})).is_err());

        // find_process：name / port 至少给一个
        assert!(parse_args(Tool::FindProcess, &json!({"targets": ["p1"]})).is_err());
        assert!(parse_args(
            Tool::FindProcess,
            &json!({"targets": ["p1"], "name": "sshd"})
        )
        .is_ok());
        assert!(parse_args(Tool::FindProcess, &json!({"targets": ["p1"], "port": 22})).is_ok());
        assert!(
            parse_args(
                Tool::FindProcess,
                &json!({"targets": ["p1"], "name": "sshd|rm"})
            )
            .is_err(),
            "进程名会进 pgrep 的模式串，`|` 这类元字符必须被挡下"
        );
        assert!(parse_args(
            Tool::FindProcess,
            &json!({"targets": ["p1"], "name": "sshd", "port": 22})
        )
        .is_ok());
    }

    /// 新工具的轨迹摘要：把「查了谁、查什么」写清楚（renderer 不做解析）。
    #[test]
    fn new_tool_summaries_name_what_is_inspected() {
        let s = store();
        let names = display_names(&s, &[id_of(&s, "prod-01")]);
        let one = |args: &ToolArgs| {
            call_summary(SummaryInput {
                tool: Tool::ReadFile,
                args,
                names: &names,
            })
        };

        let args = ToolArgs::ReadFile {
            selector: TargetSelector::Profiles(vec!["prod-01".into()]),
            path: "/etc/nginx/nginx.conf".into(),
            start_line: 1,
            max_lines: DEFAULT_READ_LINES,
            timeout_secs: 15,
        };
        let sum = one(&args);
        assert!(
            sum.contains("prod-01") && sum.contains("/etc/nginx/nginx.conf"),
            "{sum}"
        );
        assert!(!sum.contains("L1"), "默认窗口不该缀范围：{sum}");

        let ranged = ToolArgs::ReadFile {
            selector: TargetSelector::Profiles(vec!["prod-01".into()]),
            path: "/etc/nginx/nginx.conf".into(),
            start_line: 51,
            max_lines: 50,
            timeout_secs: 15,
        };
        assert!(one(&ranged).contains("L51+50"), "{}", one(&ranged));

        let svc = ToolArgs::InspectService {
            selector: TargetSelector::Profiles(vec!["prod-01".into()]),
            unit: "nginx.service".into(),
            timeout_secs: 15,
        };
        let sum = call_summary(SummaryInput {
            tool: Tool::InspectService,
            args: &svc,
            names: &names,
        });
        assert!(
            sum.contains("prod-01") && sum.contains("nginx.service"),
            "{sum}"
        );

        let proc = ToolArgs::FindProcess {
            selector: TargetSelector::Profiles(vec!["prod-01".into()]),
            name: Some("sshd".into()),
            port: Some(22),
            timeout_secs: 15,
        };
        let sum = call_summary(SummaryInput {
            tool: Tool::FindProcess,
            args: &proc,
            names: &names,
        });
        assert!(sum.contains("sshd") && sum.contains(":22"), "{sum}");

        let cert = ToolArgs::CheckCertificate {
            selector: TargetSelector::Profiles(vec!["prod-01".into()]),
            host: "127.0.0.1".into(),
            port: 443,
            server_name: None,
            timeout_secs: 15,
        };
        let sum = call_summary(SummaryInput {
            tool: Tool::CheckCertificate,
            args: &cert,
            names: &names,
        });
        assert!(sum.contains("127.0.0.1:443"), "{sum}");
    }

    #[test]
    fn call_summary_is_human_readable() {
        let s = store();
        let args = ToolArgs::RunCommand {
            selector: TargetSelector::Group("生产组".into()),
            command: "df -h\n多余的行不该出现".into(),
            timeout_secs: 15,
            risk: Risk::Safe,
        };
        let sum = call_summary(SummaryInput {
            tool: Tool::RunCommand,
            args: &args,
            names: &[],
        });
        assert!(sum.contains("生产组"), "sum={sum}");
        assert!(sum.contains("df -h"));
        assert!(!sum.contains("多余"), "只取首行：{sum}");

        let args = ToolArgs::InspectLog {
            selector: TargetSelector::Profiles(vec!["a".into()]),
            path: "/var/log/app.log".into(),
            tail_lines: 100,
            pattern: None,
            level: crate::tasks::LogLevel::Error,
            timeout_secs: 15,
        };
        let names = display_names(&s, &[id_of(&s, "prod-01")]);
        let sum = call_summary(SummaryInput {
            tool: Tool::InspectLog,
            args: &args,
            names: &names,
        });
        assert!(
            sum.contains("prod-01") && sum.contains("/var/log/app.log"),
            "sum={sum}"
        );
    }

    #[test]
    fn display_names_falls_back_to_id_for_unknown() {
        let s = store();
        let names = display_names(&s, &["ghost".to_string()]);
        assert_eq!(names, vec!["ghost"]);
    }
}
