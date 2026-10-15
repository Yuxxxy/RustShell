//! AI Agent 基础设施：Provider 抽象 + OpenAI 兼容流式 HTTP 客户端 + 结果通道 + SafetyGuard + 命令提取。

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::io::BufRead as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

// 配置：base_url / model / API Key（DPAPI 加密存储）

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AiConfig {
    /// OpenAI 兼容端点（不含 /chat/completions），如 https://api.deepseek.com/v1
    pub base_url: String,
    pub model: String,
    /// API Key 密文（platform::secret::protect 产物；明文永不落盘）
    pub api_key_cipher: Option<String>,
    /// 采样温度（可配置；越低越确定，终端运维问答/命令生成场景不宜发散）。
    #[serde(default = "default_temperature")]
    pub temperature: f32,
    /// auto-approve 白名单：命中的命令免卡片确认直接执行。
    ///
    /// 每条规则自带开关（`enabled=false` 的条目不参与匹配，即「此命令白名单失效」）。
    /// 落盘形态兼容旧的字符串数组（见 [`de_auto_approve`]）。
    #[serde(default, deserialize_with = "de_auto_approve")]
    pub auto_approve: Vec<WhitelistRule>,
    /// 思考模式开关：开启时请求体附带 `thinking: {"type": "enabled"}`，
    /// 并把服务端返回的 `reasoning_content` 增量渲染成「思考过程」折叠块。
    #[serde(default)]
    pub thinking: bool,
    /// Agent（工具调用）总开关。关闭时退化为纯问答，请求体不带 `tools`。
    #[serde(default = "yes")]
    pub agent_enabled: bool,
    /// 是否允许状态变更类命令（Confirm 级）。
    ///
    /// 默认 **false**：agent 与定时任务默认只读。开启后交互式场景仍需用户在卡片上
    /// 确认；无人值守场景即使开启也不执行（见 `agent::judge`）。
    #[serde(default)]
    pub agent_allow_mutating: bool,
    /// 模型能力缓存（v1.1 §3.8.1）：避免每轮都靠一次 400 去发现「不支持 tools」。
    ///
    /// 落盘而不是只放内存：应用重启后不该重新撞一遍同样的 400。
    #[serde(default)]
    pub capabilities: Option<ModelCapabilities>,
    /// AI 调试模式：开启后把与模型的完整往返（请求体、SSE 原始行、工具调用与
    /// 结果、轨迹行）逐条写进运行日志 `logs/<date>/rustshell.log`（[`DEBUG_LOG_TARGET`]）。
    ///
    /// 默认关闭：请求体里带着系统提示与终端上下文，逐条落盘既占空间也不需要天天看，
    /// 只在排障（如「AI 回答不对 / 工具没执行」）时打开。
    #[serde(default)]
    pub debug_log: bool,
}

fn yes() -> bool {
    true
}

/// 免确认白名单的一条规则（设置 - Agent - 免确认白名单）。
///
/// 2026-09-22 运行反馈：白名单从「一串命令原文」升级为「带开关的规则表」——
/// 每条可单独启停（默认开启），并支持 `*` 通配符。旧配置仍能读进来。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WhitelistRule {
    /// 命令模式：不含 `*` 时按命令原文整串相等匹配；含 `*` 时 `*` 匹配任意字符序列。
    pub command: String,
    /// 是否生效。默认开启；关掉即此命令的白名单失效（回到每次确认）。
    #[serde(default = "yes")]
    pub enabled: bool,
}

impl WhitelistRule {
    /// 新建一条启用态的规则（新增入口的默认状态）。
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            enabled: true,
        }
    }
}

/// 白名单字段的反序列化：兼容两种落盘形态。
///
/// - 旧的 `["systemctl restart nginx"]`：字符串数组，一律读成**启用**态；
/// - 新的 `[{"command":"…","enabled":true}]`：规则对象数组。
///
/// 用 `untagged` 就地兼容，而不是给整个 `AiConfig` 加版本号：
/// 白名单是纯增量结构，多一个版本字段只会让「老库读得进来」多一个可能忘掉的分支。
fn de_auto_approve<'de, D>(de: D) -> Result<Vec<WhitelistRule>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        Legacy(String),
        Rule {
            command: String,
            #[serde(default = "yes")]
            enabled: bool,
        },
    }
    let raw = Vec::<Raw>::deserialize(de)?;
    Ok(raw
        .into_iter()
        .map(|r| match r {
            Raw::Legacy(command) => WhitelistRule::new(command),
            Raw::Rule { command, enabled } => WhitelistRule { command, enabled },
        })
        .collect())
}

/// 命令是否命中白名单（已关闭的规则与空白模式都不参与匹配）。
///
/// 匹配的是**整条命令**：`df -h` 命中不等于 `df -h && rm -rf /` 也命中 ——
/// 后者是另一条命令。要覆盖「同一类命令」请显式写通配符（`df -h*`），
/// 这也正是通配符存在的意义：让「放行一个前缀/模板」成为用户看得见的显式选择，
/// 而不是实现里默认的宽松包含。
pub fn whitelist_hit(rules: &[WhitelistRule], command: &str) -> bool {
    let cmd = command.trim();
    rules
        .iter()
        .filter(|r| r.enabled)
        .any(|r| !r.command.trim().is_empty() && wildcard_match(r.command.trim(), cmd))
}

/// 内置「只读命令」清单：这些命令 `assess_command` 一律判 `Risk::Safe`，
/// `agent::judge` 直接执行 —— **本来就不弹确认**，白名单加不加它们都一样。
///
/// 所以它不进配置、也不带开关（[`WhitelistRule`] 管的是状态变更命令）：
/// 白名单框把它只读地列出来，只为回答「哪些命令不用确认」这个问题。
/// 往里加命令前先想清楚 —— 这里写的是「不用确认」，混进一条变更命令，
/// 界面就等于给用户一份假承诺（测试 `builtin_readonly_commands_are_all_safe` 会拦下）。
pub const BUILTIN_READONLY_COMMANDS: &[&str] = &[
    "ls -la",
    "pwd",
    "whoami",
    "date",
    "uptime",
    "uname -a",
    "df -h",
    "du -sh *",
    "free -m",
    "ps aux",
    "top -bn1",
    "ss -lntup",
    "lsof -i",
    "cat *",
    "grep -*",
    "journalctl -n 100",
    "systemctl status *",
    "docker ps",
];

/// 通配匹配：`*` 匹配任意字符序列（含空串），其余字符逐字比较。
///
/// 迭代式实现（无递归、无正则依赖）：模式里只有 `*` 一种元字符，
/// 用「回溯指针」扫一遍即可；两条串都只有几百字符，最坏 O(n·m) 无所谓。
///
/// 公开是因为白名单框的搜索框要用**同一套**语义：在那里 `systemctl *`
/// 能筛出 `systemctl restart nginx`，否则「支持通配符」只对了白名单一半。
pub fn wildcard_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star: Option<usize> = None;
    let mut star_at = 0usize;
    while ti < t.len() {
        if pi < p.len() && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            star_at = ti;
            pi += 1;
        } else if let Some(s) = star {
            // 回退：让最近那个 `*` 多吃一个字符再试
            star_at += 1;
            ti = star_at;
            pi = s + 1;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

fn default_temperature() -> f32 {
    DEFAULT_TEMPERATURE
}

pub const TEMPERATURE_MIN: f32 = 0.0;
pub const TEMPERATURE_MAX: f32 = 2.0;

/// 某一项能力的探测结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityState {
    /// 还没试过（首次调用时才探测）
    #[default]
    Unknown,
    /// 明确可用
    Supported,
    /// 明确不可用（服务端返回了「不支持 tools」之类的错误）
    Unsupported,
}

impl CapabilityState {
    pub fn label(self) -> &'static str {
        use crate::i18n::t;
        match self {
            Self::Unknown => t("未知"),
            Self::Supported => t("支持"),
            Self::Unsupported => t("不支持"),
        }
    }
}

/// 采样温度默认值与合法区间（设置面板校验用；同时作为请求体兜底值）
pub const DEFAULT_TEMPERATURE: f32 = 0.3;
///
/// **为什么需要它**：原实现把「收到 400 就去掉 tools 重发」当作能力发现机制。
/// 那意味着**每一轮** agent 对话都要先撞一次 400 才知道这个模型不支持工具调用 ——
/// 用户每次提问都要多等一个失败的往返，日志里也全是同一个 400。
///
/// 缓存按 provider 指纹（端点 + 模型）隔离：用户切换端点或模型后应当重新探测，
/// 否则会把上一个模型的能力结论错误地套到新模型上。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelCapabilities {
    /// 缓存所属的 provider 指纹（`base_url|model`）。不匹配即视为失效。
    pub fingerprint: String,
    pub tool_calling: CapabilityState,
    pub streaming: CapabilityState,
    pub reasoning: CapabilityState,
}

impl ModelCapabilities {
    /// 由端点与模型算指纹。
    pub fn fingerprint_of(base_url: &str, model: &str) -> String {
        format!("{}|{}", base_url.trim().trim_end_matches('/'), model.trim())
    }

    /// 取当前 provider 的能力；指纹不匹配（换过端点/模型）时返回全 Unknown，
    /// **不沿用旧结论** —— 沿用会把上一个模型的能力错套到新模型上。
    pub fn for_provider(cfg: &AiConfig) -> Self {
        let want = Self::fingerprint_of(&cfg.base_url, &cfg.model);
        match &cfg.capabilities {
            Some(c) if c.fingerprint == want => c.clone(),
            _ => Self {
                fingerprint: want,
                ..Default::default()
            },
        }
    }

    /// 工具调用是否**明确**不可用（据此决定要不要发 `tools`）。
    pub fn tool_calling_unsupported(&self) -> bool {
        self.tool_calling == CapabilityState::Unsupported
    }

    /// 记录一次工具调用探测结果并写回配置。
    pub fn note_tool_calling(&mut self, supported: bool) {
        self.tool_calling = if supported {
            CapabilityState::Supported
        } else {
            CapabilityState::Unsupported
        };
    }
}

/// 判定一次错误是否属于「服务端不支持 tools」。
///
/// 判定要**窄**：只有明确提到 tools/tool_calls/function calling 的报文才算，
/// 否则一个无关的 400（比如上下文超长）会被误记成「不支持工具调用」，
/// 之后这个模型就再也用不上 agent 了 —— 那是比多撞一次 400 严重得多的回归。
pub fn is_tools_unsupported_error(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    let mentions_tools = m.contains("tool")
        || m.contains("function call")
        || m.contains("function_call")
        || m.contains("tool_calls");
    if !mentions_tools {
        return false;
    }
    // 常见的「不支持」措辞
    m.contains("not support")
        || m.contains("unsupported")
        || m.contains("does not support")
        || m.contains("unknown field")
        || m.contains("unrecognized")
        || m.contains("invalid parameter")
        || m.contains("no support")
}

/// 从配置里构造能力缓存（供 UI 展示「当前模型能力」）。
pub fn current_capabilities(cfg: &AiConfig) -> ModelCapabilities {
    ModelCapabilities::for_provider(cfg)
}
/// 上下文行数：注入上下文的终端最近行数（**不是配置项**）。
///
/// 2026-09-18 运行反馈「取消 AI 上下文限制配置」：这一项从设置面板撤下，值固定在这里。
/// 终端问答/命令生成场景看到最近 200 行已经足够；留一个可调数字只会诱使人往大调
/// （prompt 变长、回答变慢），却换不来更好的答案。
pub const CONTEXT_LINES: usize = 200;

impl Default for AiConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api.deepseek.com/v1".into(),
            model: "deepseek-flash".into(),
            api_key_cipher: None,
            temperature: DEFAULT_TEMPERATURE,
            auto_approve: Vec::new(),
            thinking: false,
            agent_enabled: true,
            agent_allow_mutating: false,
            capabilities: None,
            debug_log: false,
        }
    }
}

// AI 调试日志（配置里的 `debug_log` 开关 + 逐条落盘）

/// 调试日志在运行日志里的 target 名（`grep rustshell_ai_debug` 即可筛出）。
///
/// 用 INFO 级而不是 DEBUG 级：日志器的 `max_level` 是 Info，debug 级记录会被
/// `log` 门面在进入 logger **之前**丢掉（`level <= max_level()` 的判断在宏里），
/// 换 target 也拦不住。专属 target + Info 级既保住「只在开关打开时落盘」，
/// 又能让用户按一个词筛出全部 AI 往返。
pub const DEBUG_LOG_TARGET: &str = "rustshell_ai_debug";

/// 调试开关的进程级镜像（原子量，读侧零锁）。
///
/// 请求/工具循环深处（`stream_chat`、agent 驱动、`ChannelSink`）拿不到
/// `AiConfig`，而「开一条调试日志」不值得给每个函数再挂一个布尔参数 ——
/// 配置的 `load` / `save` 两个端点顺手同步这一个原子，就覆盖了全部真实状态
/// 迁移（启动加载、面板保存、设置面板保存）。
static DEBUG_LOG_ON: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 同步调试开关（[`AiConfig::load`] / [`AiConfig::save`] 调用；测试也用）。
pub fn set_debug_log(on: bool) {
    DEBUG_LOG_ON.store(on, Ordering::Relaxed);
}

/// 调试模式是否开启。
pub fn debug_log_on() -> bool {
    DEBUG_LOG_ON.load(Ordering::Relaxed)
}

/// 写一条 AI 调试日志。
///
/// 关闭时**零开销**：`f` 是惰性的，连 `format!` 都不会执行（消息体里有整段
/// 请求体与上下文，白格式化一次就是几百 KB 的无用分配）。
pub fn debug_log(f: impl FnOnce() -> String) {
    if debug_log_on() {
        log::info!(target: DEBUG_LOG_TARGET, "{}", f());
    }
}

impl AiConfig {
    pub fn load(path: &PathBuf) -> Self {
        let cfg = match std::fs::read_to_string(path) {
            Ok(s) => match serde_json::from_str(&s) {
                Ok(v) => v,
                Err(_) => {
                    // 解析失败先留底坏文件，再回退默认
                    crate::configio::backup_broken_config(path, &s);
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        };
        // 调试开关随配置走：加载即生效，不必等某处显式调用 `set_debug_log`
        set_debug_log(cfg.debug_log);
        cfg
    }

    pub fn save(&self, path: &PathBuf) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let json = serde_json::to_string_pretty(self)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, path)?;
        // 落盘即生效：设置面板「保存」后无需重启就开始记录
        set_debug_log(self.debug_log);
        Ok(())
    }

    pub fn is_configured(&self) -> bool {
        !self.base_url.trim().is_empty()
            && !self.model.trim().is_empty()
            && self.api_key_cipher.is_some()
    }
}

// 消息与通道

/// 消息角色；serde 线格式保持小写`"system" | "user" | "assistant"`，与改造前一致。
///
/// `Tool` 是 agent 工具循环专用：承载一次工具调用的结果（配合 `tool_call_id`）。
/// 它**不会**出现在面板消息列表里（面板只渲染 system/user/assistant），
/// 只存在于发给模型的消息序列中。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    #[default]
    System,
    User,
    Assistant,
    Tool,
}

impl Role {
    /// 面板里是否按「用户气泡」（右对齐）呈现
    pub fn is_user(self) -> bool {
        matches!(self, Role::User)
    }
}

/// 消息生命周期；仅内存态，`#[serde(skip)]`不上送服务端。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum MsgState {
    /// 已发出，等待首个增量
    Pending,
    /// 正在流式接收（renderer 追加打字机光标）
    Streaming,
    /// 正常结束
    #[default]
    Done,
    /// 失败（含原因：请求失败 / 限流 / 拦截等），面板据此渲染「重试」
    Failed(String),
    /// 用户中止
    Cancelled,
}

/// 命令卡片：模型回复里提取出的待确认命令，挂在消息上随消息参与虚拟列表行高测量。
#[derive(Debug, Clone, Default)]
pub struct AiCommandCard {
    pub cmd: String,
    pub risk: Risk,
    /// dismissed / executed 后不再显示
    pub done: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    /// 线上格式：`"system" | "user" | "assistant" | "tool"`
    pub role: Role,
    pub content: String,

    // 以下均为面板侧元数据，`#[serde(skip)]`保证请求体与改造前逐字节一致
    /// 稳定主键（单调自增）。gpui 元素 id 由它派生，避免按索引派生导致的复用错位。
    #[serde(skip)]
    pub id: u64,
    /// 生命周期状态（流式光标 / 失败重试的依据）
    #[serde(skip)]
    pub state: MsgState,
    /// unix 秒（时间线分组用）
    #[serde(skip)]
    pub created_at: u64,
    /// 本次请求注入的终端上下文行数（可追溯「AI 看到了什么」）
    #[serde(skip)]
    pub injected_lines: usize,
    /// 从本消息围栏提取的命令卡片（状态层填充，renderer 只读）
    #[serde(skip)]
    pub commands: Vec<AiCommandCard>,
    /// 思考过程（服务端 `reasoning_content` 增量；思考模式关闭时恒为空）
    #[serde(skip)]
    pub reasoning: String,

    // ---- agent 工具循环专用（普通问答消息上恒为空，且不参与序列化）----
    /// assistant 消息请求调用的工具（仅 agent 模式）
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub tool_calls: Vec<ToolCall>,
    /// `role = Tool` 的消息所回应的那次调用 id
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub tool_call_id: Option<String>,
    /// agent 工具轨迹（v1.1 §3.9）：挂在**发起这一轮巡检的 assistant 消息**上。
    ///
    /// 面板侧元数据，`#[serde(skip)]` —— 轨迹是给人看的呈现，不进请求体。
    /// 之所以挂在消息上而不是另建一张表：轨迹的生存期与那条回复完全一致，
    /// 分开存就要处理「消息被删了轨迹还留着」这类清理问题。
    #[serde(skip)]
    pub trace: Vec<crate::agent::TraceRow>,
}

impl ChatMessage {
    fn new(id: u64, role: Role, content: impl Into<String>) -> Self {
        Self {
            id,
            role,
            content: content.into(),
            state: MsgState::Done,
            created_at: unix_now_secs(),
            injected_lines: 0,
            commands: Vec::new(),
            reasoning: String::new(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            trace: Vec::new(),
        }
    }

    pub fn system(id: u64, content: impl Into<String>) -> Self {
        Self::new(id, Role::System, content)
    }

    /// 用户提问。`injected_lines`记录本次一并送给模型的终端上下文行数。
    pub fn user(id: u64, content: impl Into<String>, injected_lines: usize) -> Self {
        let mut m = Self::new(id, Role::User, content);
        m.injected_lines = injected_lines;
        m
    }

    /// 助手流式占位：正文由后续 Delta 逐段填充（此刻还没有内容）。
    pub fn assistant_streaming(id: u64) -> Self {
        let mut m = Self::new(id, Role::Assistant, String::new());
        m.state = MsgState::Streaming;
        m
    }

    /// 一次性助手消息（非流式路径 / 面板自己的提示行）。
    pub fn assistant(id: u64, content: impl Into<String>) -> Self {
        Self::new(id, Role::Assistant, content)
    }

    /// assistant 消息携带工具调用请求（agent 循环里模型说「我要调这几个工具」）。
    ///
    /// `content` 通常为空或一句说明；按 OpenAI 协议，带 `tool_calls` 的 assistant
    /// 消息**不必**有正文。
    pub fn assistant_tool_calls(id: u64, content: impl Into<String>, calls: Vec<ToolCall>) -> Self {
        let mut m = Self::new(id, Role::Assistant, content);
        m.tool_calls = calls;
        m
    }

    /// 一次工具调用的**结果**，回注给模型（`role = tool` + `tool_call_id`）。
    pub fn tool(id: u64, call_id: impl Into<String>, content: impl Into<String>) -> Self {
        let mut m = Self::new(id, Role::Tool, content);
        m.tool_call_id = Some(call_id.into());
        m
    }
}

/// 一次工具调用的请求（模型产出）。
///
/// **线格式**（OpenAI 协议，与结构体字段不同名）：
/// ```json
/// {"id":"call_1","type":"function","function":{"name":"inspect_load","arguments":"{...}"}}
/// ```
/// `arguments` 是 **JSON 文本**而不是嵌套对象 —— 协议如此，所以这里的
/// `arguments: String` 与自定义序列化都是刻意的，不能图省事改成 `Value`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    /// 调用 id（回注 `role=tool` 消息时要对上）
    pub id: String,
    pub name: String,
    /// 模型给的参数（**未校验**的原始 JSON 文本）。
    ///
    /// 保留为字符串而不是 `serde_json::Value`：模型可能给出语法错误的 JSON，
    /// 那种情况必须能被参数校验层当作「参数非法」报回去让它自纠，
    /// 而在这里 `from_str` 失败会变成一条无法解释的解析错误。
    pub arguments: String,
}

impl Serialize for ToolCall {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct as _;
        let mut st = s.serialize_struct("ToolCall", 3)?;
        st.serialize_field("id", &self.id)?;
        st.serialize_field("type", "function")?;
        st.serialize_field(
            "function",
            &serde_json::json!({"name": self.name, "arguments": self.arguments}),
        )?;
        st.end()
    }
}

impl<'de> Deserialize<'de> for ToolCall {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;
        let v = serde_json::Value::deserialize(d)?;
        let id = v["id"].as_str().unwrap_or_default().to_string();
        let f = &v["function"];
        let name = f["name"].as_str().unwrap_or_default().to_string();
        if name.is_empty() {
            return Err(D::Error::custom("tool_call 缺少 function.name"));
        }
        // arguments 可能是文本，也可能是网关直接给的对象
        let arguments = match &f["arguments"] {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Null => String::new(),
            other => other.to_string(),
        };
        Ok(Self {
            id,
            name,
            arguments,
        })
    }
}

impl ToolCall {
    /// 参数按 JSON 解析；失败给出可读原因（回注给模型自纠）。
    pub fn args(&self) -> Result<serde_json::Value, String> {
        let raw = self.arguments.trim();
        if raw.is_empty() {
            // 无参工具允许缺省为空对象
            return Ok(serde_json::json!({}));
        }
        serde_json::from_str(raw).map_err(|e| format!("参数不是合法 JSON: {e}"))
    }
}

/// 流式响应里累积的 `tool_calls` 增量解析器。
///
/// OpenAI 兼容端点把一次工具调用**切成多个增量**下发（按 `index` 分组，
/// `function.arguments` 是逐段拼接的 JSON 片段）。但实践中至少存在两种变体：
/// 1. 标准碎片流（每段带 `index`，arguments 分段）；
/// 2. 部分网关一次性给完整对象（单段里 `id` / `name` / `arguments` 都齐）。
///
/// 这里对两种形态都容忍：按 `index` 归并，缺 `id`/`name` 的段只累加 arguments。
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ToolCallAccumulator {
    /// index -> (id, name, arguments)
    slots: std::collections::BTreeMap<u64, (String, String, String)>,
}

impl ToolCallAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// 吃进一个 `delta.tool_calls[]` 元素。
    pub fn push_delta(&mut self, v: &serde_json::Value) {
        // 缺失 index 时退化为 0：单次调用是最常见的形态，而丢包比错位好
        let ix = v["index"].as_u64().unwrap_or(0);
        let slot = self.slots.entry(ix).or_default();
        if let Some(id) = v["id"].as_str() {
            if !id.is_empty() {
                slot.0 = id.to_string();
            }
        }
        let f = &v["function"];
        if let Some(name) = f["name"].as_str() {
            if !name.is_empty() {
                // 名字也可能分段下发（少见），拼接而不是覆盖
                slot.1.push_str(name);
            }
        }
        if let Some(args) = f["arguments"].as_str() {
            slot.2.push_str(args);
        }
        // 有些网关把整个 function 对象塞在单段里而不是分段
        if let Some(obj) = v["function"].as_object() {
            if let Some(args_val) = obj.get("arguments") {
                if !args_val.is_string() {
                    if let Ok(s) = serde_json::to_string(args_val) {
                        if slot.2.is_empty() {
                            slot.2 = s;
                        }
                    }
                }
            }
        }
    }

    /// 收口为调用列表（按 index 升序，保证同一轮内顺序稳定）。
    ///
    /// 无 `id` 的调用补一个合成 id：回注 `role=tool` 时必须有 id 才能对上，
    /// 而服务端偶尔会省略（尤其是非流式兼容路径）。缺失时**不能**直接丢弃 ——
    /// 那会让模型看不到自己请求过的结果，进而反复重试同一个工具。
    pub fn finish(self) -> Vec<ToolCall> {
        self.slots
            .into_iter()
            .filter(|(_, (_, name, _))| !name.trim().is_empty())
            .map(|(ix, (id, name, arguments))| ToolCall {
                id: if id.trim().is_empty() {
                    format!("call_{ix}")
                } else {
                    id
                },
                name,
                arguments,
            })
            .collect()
    }
}

/// unix 秒（时间戳取不到时退化为 0，不 panic）
fn unix_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// AI 结果（请求线程 到 UI 轮询）
#[derive(Debug, Clone)]
pub enum AiOutcome {
    /// 流式增量（SSE delta；UI 侧拼接到在途 assistant 消息）
    Delta(String),
    /// 思考过程增量（`reasoning_content`；思考模式开启时才有）
    Reasoning(String),
    /// 本轮模型请求的工具调用（agent 模式；先于 `Reply` 发出）
    ToolCalls(Vec<ToolCall>),
    /// 服务端明确表示不支持工具调用（上层据此落盘能力缓存，避免每轮重复探测）
    ToolsUnsupported,
    /// 完整回复（流结束时全量文本，与 Delta 累计一致；非流式路径亦走此）
    Reply(String),
    /// 用户主动中止（UI 侧保留已收到的部分输出，标记为已取消）
    Cancelled,
    Error(String),
}

#[derive(Debug, Clone)]
pub struct AiResult {
    pub id: u64,
    pub outcome: AiOutcome,
}

pub fn ai_results() -> &'static (flume::Sender<AiResult>, flume::Receiver<AiResult>) {
    use std::sync::OnceLock;
    static CH: OnceLock<(flume::Sender<AiResult>, flume::Receiver<AiResult>)> = OnceLock::new();
    CH.get_or_init(flume::unbounded)
}

fn next_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

// Provider：单一端点，不做降级（2026-09-16 按用户要求移除备用端点）。

struct Provider {
    base_url: String,
    model: String,
    key: String,
    /// 采样温度（随设置面板变化）
    temperature: f32,
    /// 思考模式（请求体附带 `thinking` 字段）
    thinking: bool,
}

/// 由配置构造唯一 Provider。请求失败直接报错，不再尝试其它端点。
fn primary_provider(cfg: &AiConfig, api_key: &str) -> Provider {
    Provider {
        base_url: cfg.base_url.clone(),
        model: cfg.model.clone(),
        key: api_key.to_string(),
        temperature: cfg.temperature,
        thinking: cfg.thinking,
    }
}

/// 流式请求的三种收尾方式（取消与错误必须分开，UI 才有不同的文案与状态）。
enum StreamEnd {
    /// 正常结束：完整正文 + 本轮请求的工具调用（普通问答时为空）
    Done(String, Vec<ToolCall>),
    /// 用户中止（可能已有部分输出）
    Cancelled,
    /// 出错：是否已开始输出 + 可读原因
    Failed(bool, String),
}

/// 后台线程执行流式 chat/completions，增量经通道回 UI。线程体包`catch_unwind`，保证 panic 时也向 UI 补发 Error，避免状态卡死。
///
/// `cancel` 由 UI 持有并置位；置位后线程在下一个 SSE 行处收尾并发 `Cancelled`。
pub fn spawn_chat(
    cfg: AiConfig,
    api_key: String,
    messages: Vec<ChatMessage>,
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> u64 {
    spawn_chat_with_tools(cfg, api_key, messages, Vec::new(), cancel)
}

/// 同 [`spawn_chat`]，但附带工具清单（agent 模式）。
///
/// `tools` 为空即与 [`spawn_chat`] 完全等价（请求体不带 `tools` 字段）。
pub fn spawn_chat_with_tools(
    cfg: AiConfig,
    api_key: String,
    messages: Vec<ChatMessage>,
    tools: Vec<serde_json::Value>,
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> u64 {
    let tx = ai_results().0.clone();
    spawn_chat_with_tools_on(cfg, api_key, messages, tools, cancel, tx)
}

/// 同 [`spawn_chat_with_tools`]，但结果发到调用方给的通道（接收端由调用方持有）。
///
/// 为什么必须有这一版：`flume` 的接收端是**竞争消费**（一条消息只交给一个
/// 接收者）。agent 的 LLM 往返（`StreamingLlm`）与 AI 面板的轮询若共用全局通道，
/// 两边会各拿走一半 SSE 增量 —— 表现是「回答缺字」甚至「永远等不到终态」，
/// 而且只在「面板开着的同时跑巡检」时出现，极难复现。给 agent 一条私有通道，
/// 竞争就消失了。
pub fn spawn_chat_with_tools_on(
    cfg: AiConfig,
    api_key: String,
    messages: Vec<ChatMessage>,
    tools: Vec<serde_json::Value>,
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    tx: flume::Sender<AiResult>,
) -> u64 {
    let id = next_id();
    let tx_thread = tx.clone(); // 线程内使用；外层留 tx 供 spawn 失败兜底
    let spawn = std::thread::Builder::new()
        .name(format!("ai-chat-{id}"))
        .spawn(move || {
            use std::sync::atomic::Ordering;
            // done：终端结果（Reply/Error/Cancelled）是否已发出；panic 兜底据此补发
            let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let tx_inner = tx_thread.clone();
            let done_inner = done.clone();
            let body = std::panic::AssertUnwindSafe(move || {
                chat_chain(ChatChainCtx {
                    cfg: &cfg,
                    api_key: &api_key,
                    messages: &messages,
                    tools: &tools,
                    id,
                    tx: &tx_inner,
                    done: &done_inner,
                    cancel: &cancel,
                });
            });
            let panicked = std::panic::catch_unwind(body).is_err();
            if panicked || !done.load(Ordering::Relaxed) {
                let msg = if panicked {
                    "AI 请求线程异常终止".to_string()
                } else {
                    "AI 请求未返回结果".to_string()
                };
                let _ = tx_thread.send(AiResult {
                    id,
                    outcome: AiOutcome::Error(msg),
                });
            }
        });
    if spawn.is_err() {
        // 线程创建失败也必须收尾 UI 状态
        let _ = tx.send(AiResult {
            id,
            outcome: AiOutcome::Error("AI 请求线程创建失败".into()),
        });
    }
    id
}

/// [`chat_chain`] 的入参。
///
/// 打成结构体而不是摊成 8 个参数：其中 `tx` / `done` / `cancel` 都是
/// `&Arc` 形态的通道/旗标，位置写错编译器只会报类型不符或干脆静默错配，
/// 而这类错配的表现是「流式结果发到别处」—— 极难查。
struct ChatChainCtx<'a> {
    cfg: &'a AiConfig,
    api_key: &'a str,
    messages: &'a [ChatMessage],
    tools: &'a [serde_json::Value],
    id: u64,
    tx: &'a flume::Sender<AiResult>,
    done: &'a std::sync::atomic::AtomicBool,
    cancel: &'a std::sync::atomic::AtomicBool,
}

/// 单端点请求执行体（从 spawn_chat 拆出以便 catch_unwind 包裹）。
fn chat_chain(ctx: ChatChainCtx<'_>) {
    use std::sync::atomic::Ordering;
    let ChatChainCtx {
        cfg,
        api_key,
        messages,
        tools,
        id,
        tx,
        done,
        cancel,
    } = ctx;

    // 发送终端结果（Reply / Cancelled / Error）并置位完成标记
    let finish = |outcome: AiOutcome| {
        done.store(true, Ordering::Relaxed);
        let _ = tx.send(AiResult { id, outcome });
    };

    let provider = primary_provider(cfg, api_key);
    match stream_chat(&provider, messages, tools, id, tx, cancel) {
        StreamEnd::Done(full, tool_calls) => {
            // 工具调用走独立事件：面板据此进入 agent 循环，
            // 而正文照旧作为 Reply 收尾（两条路径互不干扰）。
            if !tool_calls.is_empty() {
                let _ = tx.send(AiResult {
                    id,
                    outcome: AiOutcome::ToolCalls(tool_calls),
                });
            }
            finish(AiOutcome::Reply(full))
        }
        StreamEnd::Cancelled => finish(AiOutcome::Cancelled),
        StreamEnd::Failed(started, err) => {
            if started {
                // 已输出一半断流：重试会重复输出，直接报错
                finish(AiOutcome::Error(err));
            } else {
                // 明确「不支持 tools」时上报能力结论，让上层落盘缓存 ——
                // 否则同一模型每轮都要先撞一次 400（v1.1 §3.8.1）。
                if !tools.is_empty() && is_tools_unsupported_error(&err) {
                    let _ = tx.send(AiResult {
                        id,
                        outcome: AiOutcome::ToolsUnsupported,
                    });
                }
                // 无备用端点：连接未建立即失败，直接把可读原因交回 UI
                finish(AiOutcome::Error(format!(
                    "端点不可用：{err}（请检查端点配置与网络后重试）"
                )));
            }
        }
    }
}

/// 流式请求单个 Provider。
///
/// `tools` 非空时请求体附带 `tools` 字段，并解析响应里的 `tool_calls` 增量。
/// 传空切片即退化为与改造前**逐字节一致**的纯问答请求。
fn stream_chat(
    p: &Provider,
    messages: &[ChatMessage],
    tools: &[serde_json::Value],
    id: u64,
    tx: &flume::Sender<AiResult>,
    cancel: &std::sync::atomic::AtomicBool,
) -> StreamEnd {
    use std::sync::atomic::Ordering;

    let url = format!("{}/chat/completions", p.base_url.trim_end_matches('/'));
    // temperature 来自 AiConfig（设置面板可调）；越界值钳到合法区间后再发，
    // 避免被服务端 400 拒绝（用户手改配置文件也不会打挂请求）。
    let temperature = if p.temperature.is_finite() {
        p.temperature.clamp(TEMPERATURE_MIN, TEMPERATURE_MAX)
    } else {
        DEFAULT_TEMPERATURE
    };
    let mut body = ureq::json!({
        "model": p.model,
        "messages": messages,
        "temperature": temperature,
        "stream": true,
    });
    // 工具清单：**仅非空时**附带。空数组也别发 —— 部分网关见到 `tools: []`
    // 会直接 400，而纯问答路径必须保持与改造前一致。
    if !tools.is_empty() {
        body["tools"] = serde_json::Value::Array(tools.to_vec());
        body["tool_choice"] = ureq::json!("auto");
    }
    // 思考模式：仅开启时附带，关闭时请求体与改造前逐字节一致
    //（OpenAI 兼容端点对未知字段容忍度不一，不开启就不发）。
    if p.thinking {
        body["thinking"] = ureq::json!({ "type": "enabled" });
    }

    // 主 Provider 10s 连接超时；读超时限制两次增量间最大间隔
    // （timeout_connect 属 Agent 级配置，Request 级只有 timeout/timeout_read）
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(10))
        .timeout_read(std::time::Duration::from_secs(120))
        .build();
    // 调试模式：请求体整包落盘（含系统提示与全部消息）——「AI 看到了什么」
    // 是排障第一问，少一段都解释不了它的回答。
    debug_log(|| {
        format!(
            "#{} → POST {url} · model={} · thinking={} · messages={} · tools={}\n{}",
            id,
            p.model,
            p.thinking,
            messages.len(),
            tools.len(),
            serde_json::to_string(&body).unwrap_or_else(|e| format!("<请求体序列化失败: {e}>"))
        )
    });
    let resp = match agent
        .post(&url)
        .set("Authorization", &format!("Bearer {}", p.key))
        .set("Accept", "text/event-stream")
        .send_json(body)
    {
        Ok(resp) => resp,
        Err(e) => {
            let msg = match e {
                ureq::Error::Status(code, resp) => {
                    let text = resp.into_string().unwrap_or_default();
                    serde_json::from_str::<serde_json::Value>(&text)
                        .ok()
                        .and_then(|v| v["error"]["message"].as_str().map(|s| s.to_string()))
                        .unwrap_or_else(|| format!("HTTP {code}"))
                }
                other => format!("{other}"),
            };
            debug_log(|| format!("#{id} ← 请求失败：{msg}"));
            return StreamEnd::Failed(false, msg);
        }
    };
    debug_log(|| format!("#{id} ← HTTP {} 已建立流", resp.status()));

    let reader = std::io::BufReader::new(resp.into_reader());
    let mut full = String::new();
    // 只产出思考过程、正文为空也是合法结果（推理模型被中止/仅思考即结束）
    let mut saw_reasoning = false;
    // 工具调用增量累积器（无工具时恒为空，收尾返回空列表）
    let mut tool_acc = ToolCallAccumulator::new();
    for line in reader.lines() {
        if cancel.load(Ordering::Relaxed) {
            debug_log(|| format!("#{id} ← 用户取消（已收 {} 字符）", full.chars().count()));
            return StreamEnd::Cancelled;
        }
        let line = match line {
            Ok(line) => line,
            Err(e) => {
                let msg = format!("流读取中断: {e}");
                debug_log(|| format!("#{id} ← {msg}"));
                return StreamEnd::Failed(true, msg);
            }
        };
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data.is_empty() {
            continue;
        }
        // 原始 SSE 行逐条落盘（含 [DONE]）——增量顺序、reasoning 与 tool_calls
        // 的到达时机只有原文能还原。
        debug_log(|| format!("#{id} ← SSE {data}"));
        if data == "[DONE]" {
            break;
        }
        let v: serde_json::Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(_) => continue, // 忽略非 JSON 心跳/注释行
        };
        let delta = &v["choices"][0]["delta"];
        // 思考过程：DeepSeek/Qwen 系为 `reasoning_content`，部分网关用 `reasoning`。
        // 思考模式关闭时不接受该增量（口径与请求一致：没开就不展示）。
        if p.thinking {
            for key in ["reasoning_content", "reasoning"] {
                if let Some(reason) = delta[key].as_str() {
                    if !reason.is_empty() {
                        saw_reasoning = true;
                        let _ = tx.send(AiResult {
                            id,
                            outcome: AiOutcome::Reasoning(reason.to_string()),
                        });
                    }
                }
            }
        }
        if let Some(content) = delta["content"].as_str() {
            if !content.is_empty() {
                full.push_str(content);
                let _ = tx.send(AiResult {
                    id,
                    outcome: AiOutcome::Delta(content.to_string()),
                });
            }
        }
        // 工具调用增量：本函数只累积，收尾时随 StreamEnd 一起交回调用方。
        // 不在流中逐个下发 —— 半截的 arguments 是非法 JSON，UI 无从渲染。
        if let Some(calls) = delta["tool_calls"].as_array() {
            for c in calls {
                tool_acc.push_delta(c);
            }
        }
    }
    let tool_calls = tool_acc.finish();
    // 收尾汇总：正文与工具调用各一条（正文可能在增量日志里已逐条出现，
    // 这里给的是**拼装后**的最终文本，复制即可用）。
    debug_log(|| {
        let calls = if tool_calls.is_empty() {
            "无".to_string()
        } else {
            tool_calls
                .iter()
                .map(|c| format!("{}#{} args={}", c.name, c.id, c.arguments))
                .collect::<Vec<_>>()
                .join("; ")
        };
        format!(
            "#{id} ← 收流完成 · 正文 {} 字符 · 工具调用：{calls}\n[正文]\n{full}",
            full.chars().count()
        )
    });
    // 「正文与思考都为空」原本是错误，但**带工具调用**时正文为空完全正常：
    // 模型这一轮只说「我要调工具」。把它当错误会让 agent 第一步就断掉。
    if full.is_empty() && !saw_reasoning && tool_calls.is_empty() {
        return StreamEnd::Failed(false, "响应为空".into());
    }
    StreamEnd::Done(full, tool_calls)
}

// 审计日志：agent-audit.log 全量工具调用，JSON Lines 追加写

/// 追加一条审计记录。kind: "ai_cmd"（AI 卡片）/ "quick_cmd"（快捷命令确认执行）。
pub fn audit_log(path: &PathBuf, kind: &str, cmd: &str, risk: Risk, executed: bool) {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let risk_str = match risk {
        Risk::Safe => "safe",
        Risk::Confirm => "confirm",
        Risk::Blocked => "blocked",
    };
    // 命令可能携带敏感参数，落盘前脱敏
    let entry = serde_json::json!({
        "ts": ts,
        "kind": kind,
        "risk": risk_str,
        "command": redact(cmd),
        "executed": executed,
    });
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    use std::io::Write as _;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "{entry}");
    }
}

// 系统提示词（L1 问答 + L2 命令生成共用）

pub fn system_prompt() -> String {
    "你是 RustShell 内置的终端 AI 助手，帮助用户在服务器上完成任务。规则：\n\
     1. 用简体中文，回答简洁直接，优先给结论。\n\
     2. 上下文里给了「当前终端」时，它就是用户此刻在看的机器：「这个终端 / 这台机器 / 这里」都指它。\n\
     3. 上下文里已有的指标（内存、CPU 等）足够回答就直接答，不要为已有的事实再跑一次工具；\n\
     4. 需要执行命令或查更多数据时直接用工具（工具可用时不要再给一段让用户自己粘贴的命令）；\n\
        默认目标就是「当前终端」的连接 id —— 把它放进 `targets`，不要为此反问用户。\n\
        当前终端是本地终端时工具够不到本机，如实说明即可。\n\
     5. 用户提供了「当前终端上下文」时，基于它回答；不要编造上下文之外的信息。\n\
     6. 当用户想执行任务，或给出命令更有帮助时，在回答末尾用 ```bash 围栏给出恰好一条可直接执行的命令（命令本身不要加注释）。\n\
     7. 危险操作（删除、重启、格式化等）必须在文字里明确警告。"
        .to_string()
}

/// 「当前终端」画像：提问时用户正看着的那个终端。
///
/// 为什么要注入它：Agent 的工具全是「对连接（SSH）执行」，而模型看不出用户
/// 此刻在哪个标签上 —— 「这个终端的内存是多少」于是要么先 `list_targets` 猜一台，
/// 要么反过来问用户是哪台。把当前标签的事实（连同最近一次采样的实时指标）
/// 直接放进上下文：已有指标的问题一步就能答，要用工具时也有现成的连接 id。
#[derive(Debug, Clone, PartialEq)]
pub struct TerminalSnapshot {
    /// 标签标题（连接名 / 「PowerShell」）
    pub title: String,
    pub kind: crate::session::SessionKind,
    /// SSH 会话所属的连接 id（模型调工具时当 `targets` 用；本地终端为 `None`）
    pub profile_id: Option<String>,
    /// `user@host:port`（本地终端为 `None`）
    pub target: Option<String>,
    pub state: crate::session::SessionState,
    /// 最近一次采样（本地 1 s / 远端 5 s 一次；可能还没采到）
    pub metrics: Option<crate::metrics::Sample>,
    /// 系统概况（主机名 / 运行时长 / 磁盘；15 s 刷新一次）
    pub overview: Option<std::sync::Arc<crate::metrics::SystemOverview>>,
}

/// 把画像渲染成注入块（纯函数，便于单测）。
///
/// 只写**事实**，不做建议 —— 该怎么用这些事实由 `system_prompt` 的规则说。
/// 指标只在**已连接**时给出：断开后存储里还留着上一次的样本，照抄会让
/// 模型拿旧数字回答「现在」的问题。
fn terminal_block(snap: &TerminalSnapshot) -> String {
    use crate::i18n::tp;
    use crate::session::{SessionKind, SessionState};

    let sep = tp("：", ": ");
    let delim = tp("；", "; ");
    let kind = match snap.kind {
        SessionKind::Ssh => "SSH",
        SessionKind::Local => tp("本地", "local"),
    };
    let state = match snap.state {
        SessionState::Connecting => tp("连接中", "connecting"),
        SessionState::Connected => tp("已连接", "connected"),
        SessionState::Closed => tp("已断开", "closed"),
    };
    let head = [
        format!("{}{sep}{}", tp("标签", "title"), snap.title),
        format!("{}{sep}{kind}", tp("类型", "type")),
        format!("{}{sep}{state}", tp("状态", "state")),
    ]
    .join(delim);

    let mut out = format!("{}\n{head}", tp("[当前终端]", "[current terminal]"));
    match (snap.profile_id.as_deref(), snap.target.as_deref()) {
        (Some(id), target) => {
            out.push_str(&format!(
                "\n{}{sep}{id}（{}）",
                tp("连接 id", "connection id"),
                tp(
                    "调工具时把它放进 targets",
                    "pass it in `targets` when calling tools"
                ),
            ));
            if let Some(t) = target.filter(|t| !t.trim().is_empty()) {
                out.push_str(&format!("\n{}{sep}{t}", tp("地址", "address")));
            }
        }
        (None, _) => out.push_str(&format!(
            "\n{}",
            tp(
                "本机终端：Agent 的工具只能在「连接」里的服务器上执行，够不到本机",
                "local machine terminal: agent tools run on saved connections (servers), not here",
            )
        )),
    }
    if snap.state == SessionState::Connected {
        if let Some(line) = metrics_line(snap.metrics.as_ref(), snap.overview.as_deref()) {
            out.push_str(&format!(
                "\n{}{sep}{line}（{}）",
                tp("指标", "metrics"),
                tp("最近一次采样", "latest sample"),
            ));
        }
    }
    out
}

/// 一行实时指标：内存 / CPU / 交换 / 主机 / 运行时长 / 磁盘合计。
///
/// 缺的那几项**跳过**而不是写「—」：注入给模型的是事实，不是界面 ——
/// 「交换 —」会让模型以为交换是零，而实际是没启用或没采到。
fn metrics_line(
    sample: Option<&crate::metrics::Sample>,
    overview: Option<&crate::metrics::SystemOverview>,
) -> Option<String> {
    use crate::i18n::{tp, ts};
    use crate::metrics::format_uptime;

    let mut parts: Vec<String> = Vec::new();
    if let Some(s) = sample {
        parts.push(format!("{}{}", tp("内存 ", "memory "), s.mem_summary()));
        parts.push(format!("CPU {:.1}%", s.cpu_percent));
        let swap = s.swap_summary();
        if swap != "—" {
            parts.push(format!("{}{swap}", tp("交换 ", "swap ")));
        }
    }
    if let Some(o) = overview {
        if !o.hostname.trim().is_empty() {
            parts.push(format!("{}{}", tp("主机 ", "host "), o.hostname));
        }
        if o.uptime_secs > 0 {
            let up = format_uptime(o.uptime_secs);
            parts.push(ts(format!("已运行 {up}"), format!("uptime {up}")));
        }
        let (used, total) = o.disk_totals();
        if total > 0 {
            let d = format!(
                "{}/{}",
                crate::metrics::format_bytes_short(used),
                crate::metrics::format_bytes_short(total)
            );
            parts.push(format!("{}{d}", tp("磁盘 ", "disk ")));
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(tp("；", "; ")))
    }
}

/// 提问包装：当前终端事实 + 终端最近输出 + 用户原话。
///
/// `[用户]` 这一行是**协议**：会话标题（`ui::chat_title_from`）与「重新生成」
/// 都按它切分，改这一行要连它们一起改。
pub fn with_context(
    query: &str,
    terminal: Option<&TerminalSnapshot>,
    terminal_tail: Option<&str>,
) -> String {
    let mut out = String::new();
    if let Some(snap) = terminal {
        out.push_str(&terminal_block(snap));
        out.push_str("\n\n");
    }
    if let Some(ctx) = terminal_tail.filter(|c| !c.trim().is_empty()) {
        out.push_str("[当前终端上下文（最近输出，可能截断）]\n");
        out.push_str(ctx);
        out.push_str("\n\n");
    }
    out.push_str("[用户]\n");
    out.push_str(query);
    out
}

/// 截取终端可见文本尾部：最多`max_lines`行、总量 <= 8 KiB。
pub fn truncate_tail(text: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(max_lines);
    let mut tail: Vec<String> = lines[start..].iter().map(|s| redact(s)).collect();
    // 总量限制
    const MAX_BYTES: usize = 8 * 1024;
    let mut total = tail.iter().map(|s| s.len() + 1).sum::<usize>();
    while total > MAX_BYTES && tail.len() > 1 {
        total -= tail.remove(0).len() + 1;
    }
    tail.join("\n")
}

/// 脱敏（零依赖手写）：password/token/secret/api-key 赋值形态 → `xxx=***`；
/// 常见密钥字面量（sk-...、ghp_...、AKIA...）→ `***`。
///
/// 赋值形态用逐字符 ASCII 小写得到字符索引再换算字节边界，
/// 避免非 ASCII 输入展开后字节偏移错位泄漏敏感值。
pub fn redact(line: &str) -> String {
    const MARKS: &[&str] = &[
        "password", "passwd", "token", "secret", "api_key", "api-key", "apikey",
    ];

    let chars: Vec<char> = line.chars().collect();
    // 逐字符 ASCII 小写：长度与 chars 严格一致，索引可安全互换
    let lower: Vec<char> = chars.iter().map(|c| c.to_ascii_lowercase()).collect();
    let mut out = line.to_string();

    // 第一步：赋值形态（在任一标记词之后找 = / :，屏蔽其后的第一个值）
    let mut mark_start: Option<usize> = None;
    'outer: for i in 0..lower.len() {
        for m in MARKS {
            let mc: Vec<char> = m.chars().collect();
            if i + mc.len() <= lower.len() && lower[i..i + mc.len()] == mc[..] {
                mark_start = Some(i);
                break 'outer;
            }
        }
    }
    if let Some(pos) = mark_start {
        if let Some(eq) = lower[pos..].iter().position(|c| *c == '=' || *c == ':') {
            let val_start = pos + eq + 1;
            let skip = lower[val_start..]
                .iter()
                .take_while(|c| c.is_whitespace())
                .count();
            let val_start = val_start + skip;
            if val_start < chars.len() {
                let end = chars[val_start..]
                    .iter()
                    .position(|c| c.is_whitespace())
                    .map(|i| val_start + i)
                    .unwrap_or(chars.len());
                // 字符索引 到 字节偏移（保证 char boundary）
                let b_start: usize = chars[..val_start].iter().map(|c| c.len_utf8()).sum();
                let b_end: usize = chars[..end].iter().map(|c| c.len_utf8()).sum();
                out = format!("{}***{}", &line[..b_start], &line[b_end..]);
            }
        }
    }

    // 第二步：密钥字面量
    let mut fin = String::with_capacity(out.len());
    let mut rest = out.as_str();
    while !rest.is_empty() {
        let hit = ["sk-", "ghp_", "AKIA"]
            .iter()
            .find(|p| rest.starts_with(**p));
        match hit {
            Some(p) => {
                let min_len = if *p == "AKIA" { 20 } else { 23 };
                let token_end = rest
                    .char_indices()
                    .map(|(idx, c)| idx + c.len_utf8())
                    .find(|&idx| {
                        idx >= p.len()
                            && rest
                                .as_bytes()
                                .get(idx)
                                .is_none_or(|b| !b.is_ascii_alphanumeric())
                    })
                    .unwrap_or(rest.len());
                if token_end >= min_len {
                    fin.push_str("***");
                    rest = &rest[token_end..];
                } else {
                    fin.push_str(&rest[..p.len()]);
                    rest = &rest[p.len()..];
                }
            }
            None => {
                let take = rest
                    .find(['s', 'g', 'A'])
                    .map(|i| i.max(1))
                    .unwrap_or(rest.len());
                fin.push_str(&rest[..take]);
                rest = &rest[take..];
            }
        }
    }
    fin
}

// L2：命令提取 + SafetyGuard（风险分级）

/// 从回复中提取首个```bash / ```sh /``` 围栏命令块
pub fn extract_command(reply: &str) -> Option<String> {
    let mut in_block = false;
    let mut buf = String::new();
    for line in reply.lines() {
        let t = line.trim();
        if !in_block {
            if t.starts_with("```") {
                let tag = t.trim_start_matches('`').trim();
                if t == "```" || matches!(tag, "bash" | "sh" | "shell" | "console" | "zsh") {
                    in_block = true;
                    buf.clear();
                }
            }
        } else if t.starts_with("```") {
            let cmd = buf.trim();
            if cmd.is_empty() {
                in_block = false;
                buf.clear();
                continue;
            }
            // 去掉常见的提示符前缀
            let cleaned: Vec<&str> = cmd
                .lines()
                .map(|l| {
                    let l = l.trim();
                    l.strip_prefix("$ ").unwrap_or(l)
                })
                .collect();
            return Some(cleaned.join("\n"));
        } else {
            buf.push_str(line);
            buf.push('\n');
        }
    }
    None
}

/// 风险分级：Safe 直跑 / Confirm 确认 / Blocked 拒绝
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Risk {
    #[default]
    Safe,
    Confirm,
    Blocked,
}

/// 命令风险评估：黑名单（强破坏） 到 Blocked；敏感动词 到 Confirm；其余 Safe。
pub fn assess_command(cmd: &str) -> Risk {
    let lower = cmd.to_lowercase();

    // Blocked：不可逆的强破坏
    const BLOCK: &[&str] = &[
        "mkfs",
        "dd if=",
        ":(){",
        "fork()",
        "> /dev/sd",
        "of=/dev/sd",
        " shred /dev",
        "wipefs",
        "chkdsk /f",
        "diskpart",
        "cipher /w",
    ];
    if BLOCK.iter().any(|p| lower.contains(p)) {
        return Risk::Blocked;
    }
    // 根目录 / 递归强删（白名单路径前排除：如 rm -rf ./build）
    if (lower.contains("rm -rf") || lower.contains("rm -fr"))
        && (lower.contains(" / ")
            || lower.trim_end().ends_with(" /")
            || lower.contains(" rm -rf /*")
            || lower.contains("rm -rf ~")
            || lower.contains("c:\\"))
    {
        return Risk::Blocked;
    }

    // Confirm：影响系统状态/服务/数据的操作
    const CONFIRM: &[&str] = &[
        "rm ",
        "rmdir",
        "del ",
        "shutdown",
        "reboot",
        "halt",
        "poweroff",
        "systemctl stop",
        "systemctl restart",
        "systemctl disable",
        "service ",
        "kill ",
        "killall",
        "pkill",
        "truncate",
        "> /dev/",
        "mv ",
        "chmod",
        "chown",
        "userdel",
        "dropdb",
        "drop table",
        "git push --force",
        "git reset --hard",
        "apt remove",
        "apt purge",
        "yum remove",
        "dnf remove",
        "pacman -r",
        "docker rm",
        "docker rmi",
        "docker system prune",
        "format ",
        "reg delete",
        "taskkill",
    ];
    if CONFIRM.iter().any(|p| lower.contains(p)) {
        return Risk::Confirm;
    }

    Risk::Safe
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_tail_limits_lines_and_redacts() {
        let text = (0..500)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n")
            + "\npassword=hunter2\nsk-abcdefghijklmnopqrstuvwxyz012345";
        let out = truncate_tail(&text, 100);
        let lines: Vec<&str> = out.lines().collect();
        // 尾部 100 行 = 98 行日志 + 2 行附加（附加行包含在内）
        assert_eq!(lines.len(), 100);
        assert!(out.contains("password=***"));
        assert!(out.contains("***"));
        assert!(!out.contains("hunter2"));
    }

    #[test]
    fn extract_command_parses_fenced_block() {
        let reply =
            "先清理缓存：\n```bash\n$ rm -rf ./target/debug/incremental\n```\n然后重新构建。";
        let cmd = extract_command(reply).unwrap();
        assert_eq!(cmd, "rm -rf ./target/debug/incremental");
    }

    #[test]
    fn extract_command_none_without_fence() {
        assert!(extract_command("直接运行 ls -la 就好").is_none());
    }

    #[test]
    fn assess_blocks_destructive() {
        assert_eq!(assess_command("mkfs.ext4 /dev/sda1"), Risk::Blocked);
        assert_eq!(assess_command("dd if=/dev/zero of=/dev/sda"), Risk::Blocked);
        assert_eq!(assess_command("rm -rf /"), Risk::Blocked);
        assert_eq!(assess_command("rm -rf ~/data"), Risk::Blocked);
    }

    #[test]
    fn assess_confirms_or_allows() {
        assert_eq!(assess_command("rm -rf ./build"), Risk::Confirm);
        assert_eq!(assess_command("systemctl restart nginx"), Risk::Confirm);
        assert_eq!(assess_command("kill -9 1234"), Risk::Confirm);
        assert_eq!(assess_command("ls -la"), Risk::Safe);
        assert_eq!(assess_command("df -h"), Risk::Safe);
        assert_eq!(assess_command("grep ERROR app.log | tail -20"), Risk::Safe);
    }

    /// 内置「只读命令」清单必须**真的只读**。
    ///
    /// 界面把这一区标成「本来就无需确认」—— 往里混一条 `systemctl restart *`
    /// 就等于向用户承诺「这条不用确认」，而执行时照样弹卡片，界面上说的话就假了。
    #[test]
    fn builtin_readonly_commands_are_all_safe() {
        assert!(
            !BUILTIN_READONLY_COMMANDS.is_empty(),
            "内置清单空了，白名单框那一区就只剩个空标题"
        );
        for cmd in BUILTIN_READONLY_COMMANDS {
            assert_eq!(
                assess_command(cmd),
                Risk::Safe,
                "内置只读清单里混进了会改状态/被拦的命令：{cmd}"
            );
        }
        let mut sorted: Vec<&str> = BUILTIN_READONLY_COMMANDS.to_vec();
        sorted.sort_unstable();
        let before = sorted.len();
        sorted.dedup();
        assert_eq!(before, sorted.len(), "内置只读清单里有重复项");
    }

    #[test]
    fn primary_provider_reflects_config() {
        let cfg = AiConfig {
            base_url: "http://localhost:11434/v1".into(),
            model: "llama3.1".into(),
            temperature: 0.5,
            ..AiConfig::default()
        };
        let p = primary_provider(&cfg, "primary-key");
        // 可观测行为：唯一 Provider 直接搬运配置，不存在第二个端点
        assert_eq!(p.base_url, "http://localhost:11434/v1");
        assert_eq!(p.model, "llama3.1");
        assert_eq!(p.key, "primary-key");
        assert_eq!(p.temperature, 0.5);
        // 思考模式默认关（不改请求体），显式开启后才随 Provider 下发
        assert!(!p.thinking);
        assert!(!AiConfig::default().thinking);
        let on = primary_provider(
            &AiConfig {
                thinking: true,
                ..cfg
            },
            "k",
        );
        assert!(on.thinking);
    }

    /// 思考过程是面板元数据：不得随请求体上送（与 id/state 同一条护栏）。
    #[test]
    fn reasoning_is_not_serialized_to_the_wire() {
        let mut m = ChatMessage::assistant(9, "答案");
        m.reasoning = "先看上下文，再给结论".into();
        let v = serde_json::to_value(&m).expect("serialize");
        assert_eq!(v.as_object().expect("object").len(), 2, "{v}");
        assert!(v.get("reasoning").is_none());
        // 内存态仍可读（渲染「思考过程」折叠块用）
        assert_eq!(m.reasoning, "先看上下文，再给结论");
    }

    // v1.1 §3.8.1：模型能力缓存

    /// 能力必须按 provider 指纹隔离：换过端点/模型后**不能**沿用旧结论，
    /// 否则会把上一个模型「不支持工具调用」错误地套到新模型上。
    #[test]
    fn capability_cache_is_scoped_to_the_provider() {
        let mut cfg = AiConfig {
            base_url: "https://a.example/v1".into(),
            model: "m1".into(),
            ..Default::default()
        };
        assert_eq!(
            ModelCapabilities::for_provider(&cfg).tool_calling,
            CapabilityState::Unknown,
            "没探测过就是未知"
        );

        // 记一次「不支持」
        let mut caps = ModelCapabilities::for_provider(&cfg);
        caps.note_tool_calling(false);
        cfg.capabilities = Some(caps);
        assert!(ModelCapabilities::for_provider(&cfg).tool_calling_unsupported());

        // 换模型 -> 结论失效（回到 Unknown，而不是继续说不支持）
        let other = AiConfig {
            model: "m2".into(),
            ..cfg.clone()
        };
        assert_eq!(
            ModelCapabilities::for_provider(&other).tool_calling,
            CapabilityState::Unknown,
            "换模型后必须重新探测"
        );

        // 换端点同理
        let other_url = AiConfig {
            base_url: "https://b.example/v1".into(),
            ..cfg.clone()
        };
        assert_eq!(
            ModelCapabilities::for_provider(&other_url).tool_calling,
            CapabilityState::Unknown
        );

        // 末尾斜杠不应让指纹失效（同一端点写成两种形态是常见的）
        let slash = AiConfig {
            base_url: "https://a.example/v1/".into(),
            ..cfg.clone()
        };
        assert!(
            ModelCapabilities::for_provider(&slash).tool_calling_unsupported(),
            "末尾斜杠不该被当成另一个端点"
        );
    }

    /// 判定「不支持 tools」必须**窄**：无关的 400 被误记会让这个模型
    /// 永远用不上 agent —— 比多撞一次 400 严重得多。
    #[test]
    fn tools_unsupported_detection_is_narrow() {
        // 明确提到 tools 且是「不支持」措辞
        assert!(is_tools_unsupported_error(
            "Invalid parameter: tools is not supported by this model"
        ));
        assert!(is_tools_unsupported_error("unknown field `tools`"));
        assert!(is_tools_unsupported_error("TOOL_CALLS: unsupported"));

        // 提到 tools 但**不是**不支持（如参数写错的普通 400）——不认，避免误记
        assert!(!is_tools_unsupported_error(
            "tools[0].function.name is empty"
        ));
        // 完全没提 tools（如上下文超长）——不认
        assert!(!is_tools_unsupported_error(
            "This model's maximum context length is 8192 tokens"
        ));
        assert!(!is_tools_unsupported_error("rate limit exceeded"));
    }

    /// 能力状态要落盘并可回读（重启后不该重新撞一遍 400）。
    #[test]
    fn capabilities_round_trip_through_config_json() {
        let cfg = AiConfig {
            capabilities: Some(ModelCapabilities {
                fingerprint: ModelCapabilities::fingerprint_of("https://x/v1", "m"),
                tool_calling: CapabilityState::Unsupported,
                streaming: CapabilityState::Supported,
                reasoning: CapabilityState::Unknown,
            }),
            ..Default::default()
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let back: AiConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.capabilities, cfg.capabilities);

        // 老配置没有 capabilities 字段 -> 补 None，不报错
        let legacy: AiConfig =
            serde_json::from_str(r#"{"model":"deepseek-flash"}"#).expect("旧配置必须仍可解析");
        assert!(legacy.capabilities.is_none());
        assert_eq!(
            ModelCapabilities::for_provider(&legacy).tool_calling,
            CapabilityState::Unknown
        );
    }

    /// 思考模式的旧配置兼容：老 `ai_config.json` 无 `thinking` 字段时回退 false。
    #[test]
    fn legacy_config_without_thinking_defaults_to_off() {
        let cfg: AiConfig = serde_json::from_str(
            r#"{"base_url":"https://api.deepseek.com/v1","model":"deepseek-flash"}"#,
        )
        .expect("旧配置必须仍可解析");
        assert!(!cfg.thinking);
        assert_eq!(cfg.temperature, DEFAULT_TEMPERATURE);
    }

    /// 上下文行数已不是配置项（2026-09-18「取消 AI 上下文限制配置」）：
    /// 老配置文件里残留的 `context_lines` 必须被**静默忽略**，不能因为多了一个键就解析失败。
    #[test]
    fn legacy_context_lines_key_is_ignored() {
        let cfg: AiConfig =
            serde_json::from_str(r#"{"model":"deepseek-flash","context_lines":2000}"#)
                .expect("老配置里的 context_lines 不该让解析失败");
        assert_eq!(cfg.model, "deepseek-flash");
        assert_eq!(CONTEXT_LINES, 200);
    }

    /// 「最大工具步数」已不是配置项（2026-09-22 移除，不做限制）：
    /// 老配置文件里残留的 `agent_max_steps` 同样要被静默忽略。
    #[test]
    fn legacy_agent_max_steps_key_is_ignored() {
        let cfg: AiConfig =
            serde_json::from_str(r#"{"model":"deepseek-flash","agent_max_steps":16}"#)
                .expect("老配置里的 agent_max_steps 不该让解析失败");
        assert_eq!(cfg.model, "deepseek-flash");
    }

    /// 白名单匹配：整串相等、通配符、逐条启停三种语义。
    #[test]
    fn whitelist_match_supports_exact_and_wildcards() {
        let rules = vec![
            WhitelistRule::new("systemctl restart nginx"),
            WhitelistRule::new("df -h"),
        ];
        assert!(whitelist_hit(&rules, "df -h"));
        assert!(whitelist_hit(&rules, "  df -h  "), "两侧空白应被忽略");
        // 整串匹配而不是子串/前缀：多一段尾巴就是另一条命令
        assert!(!whitelist_hit(&rules, "df -h && rm -rf /"));
        assert!(!whitelist_hit(&rules, "echo df -h"));

        let star = vec![WhitelistRule::new("systemctl restart *")];
        assert!(whitelist_hit(&star, "systemctl restart nginx"));
        assert!(whitelist_hit(&star, "systemctl restart nginx -q"));
        assert!(!whitelist_hit(&star, "systemctl stop nginx"));
        // 模式里的空格是**字面量**：`*` 能吃空串，但那个空格不能凭空消失。
        // 要连「不带参数」一起放行就写 `restart*`（星号直接贴着字面量）。
        assert!(
            !whitelist_hit(&star, "systemctl restart"),
            "空格必须逐字对上 —— 匹配是精确的，不给「顺便放行」留口子"
        );
        let tight = vec![WhitelistRule::new("systemctl restart*")];
        assert!(whitelist_hit(&tight, "systemctl restart"), "`*` 可匹配空串");
        assert!(whitelist_hit(&tight, "systemctl restart nginx"));

        // `*` 出现在中间：两段都要按原文对上
        let mid = vec![WhitelistRule::new("docker * --help")];
        assert!(whitelist_hit(&mid, "docker ps -a --help"));
        assert!(!whitelist_hit(&mid, "docker ps -a --help-x"));
        assert!(!whitelist_hit(&mid, "docker ps -a --version"));

        // 关闭的规则不参与匹配（关掉 = 此命令白名单失效）
        let off = vec![WhitelistRule {
            command: "df -h".into(),
            enabled: false,
        }];
        assert!(!whitelist_hit(&off, "df -h"));
        // 空白模式不匹配任何命令（避免空规则放行一切）
        let blank = vec![WhitelistRule::new("   ")];
        assert!(!whitelist_hit(&blank, ""));
    }

    /// 老白名单形态（字符串数组）读成「启用态」规则；新形态原样回读。
    #[test]
    fn whitelist_accepts_legacy_string_array_and_round_trips() {
        let legacy: AiConfig = serde_json::from_str(
            r#"{"model":"m","auto_approve":["df -h","systemctl restart nginx"]}"#,
        )
        .expect("旧白名单必须仍可解析");
        assert_eq!(legacy.auto_approve.len(), 2);
        assert!(
            legacy.auto_approve.iter().all(|r| r.enabled),
            "老条目一律读成启用态"
        );
        assert!(whitelist_hit(&legacy.auto_approve, "df -h"));

        let cfg = AiConfig {
            auto_approve: vec![WhitelistRule {
                command: "df *".into(),
                enabled: false,
            }],
            ..Default::default()
        };
        let back: AiConfig = serde_json::from_str(&serde_json::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(back.auto_approve, cfg.auto_approve);
        assert!(
            !whitelist_hit(&back.auto_approve, "df -h"),
            "关闭态必须能落盘回读（否则重启就悄悄复活了）"
        );
    }

    /// 调试开关是**进程级**的，两个用例都会掰它，必须串行跑。
    static DEBUG_FLAG_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn lock_debug_flag_tests() -> std::sync::MutexGuard<'static, ()> {
        DEBUG_FLAG_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// 调试开关随配置走：`save` / `load` 同步进程级镜像（面板保存后立即生效，
    /// 不需要重启），且关闭时闭包**零执行** —— 消息体里是整包请求与上下文，
    /// 「格式化了再丢弃」白烧几百 KB 分配。
    #[test]
    fn debug_log_flag_follows_config_and_is_lazy() {
        let _guard = lock_debug_flag_tests();
        // 老配置无 `debug_log` 字段 -> 回退 false
        let legacy: AiConfig = serde_json::from_str(r#"{"model":"m"}"#).unwrap();
        assert!(!legacy.debug_log, "老配置必须回退到关闭");

        let dir = std::env::temp_dir().join(format!("rs-debug-flag-{}", std::process::id()));
        let path = dir.join("ai_config.json");
        let _ = std::fs::remove_file(&path);

        set_debug_log(false);
        let ran = std::cell::Cell::new(false);
        debug_log(|| {
            ran.set(true);
            "不该被格式化".to_string()
        });
        assert!(!ran.get(), "关闭时闭包不得执行（惰性求值）");

        let mut cfg = AiConfig {
            debug_log: true,
            ..Default::default()
        };
        cfg.save(&path).unwrap();
        assert!(debug_log_on(), "保存后开关应立即生效（无需重启）");

        let back = AiConfig::load(&path);
        assert!(back.debug_log, "debug_log 必须能落盘回读");
        assert!(debug_log_on(), "加载同样要同步镜像");

        cfg.debug_log = false;
        cfg.save(&path).unwrap();
        assert!(!debug_log_on(), "再关掉也要立即生效");

        set_debug_log(false); // 不给后续用例留状态
        let _ = std::fs::remove_file(&path);
    }

    /// 调试日志必须真的**到达日志器**，且是专属 target + INFO 级。
    ///
    /// 为什么专门测这个：应用把日志器的 `max_level` 设成 Info，若有人把实现改回
    /// `log::debug!`，记录会在 `log` 门面层被静默丢掉 —— 开关开着却什么都看不到，
    /// 属于最难排查的一类故障。
    #[test]
    fn debug_log_reaches_the_logger_with_its_own_target() {
        let _guard = lock_debug_flag_tests();

        struct Collector;
        struct Rec {
            target: String,
            level: log::Level,
            msg: String,
        }
        static SEEN: std::sync::OnceLock<std::sync::Mutex<Vec<Rec>>> = std::sync::OnceLock::new();
        impl log::Log for Collector {
            fn enabled(&self, _m: &log::Metadata<'_>) -> bool {
                true
            }
            fn log(&self, r: &log::Record<'_>) {
                if let Some(seen) = SEEN.get() {
                    seen.lock().unwrap().push(Rec {
                        target: r.target().to_string(),
                        level: r.level(),
                        msg: r.args().to_string(),
                    });
                }
            }
            fn flush(&self) {}
        }
        static INIT: std::sync::Once = std::sync::Once::new();
        INIT.call_once(|| {
            let _ = SEEN.set(std::sync::Mutex::new(Vec::new()));
            let _ = log::set_boxed_logger(Box::new(Collector));
            // 与应用同口径（shell/src/app.rs）：Info 级
            log::set_max_level(log::LevelFilter::Info);
        });
        let seen = || SEEN.get().expect("收集器已初始化");
        seen().lock().unwrap().clear();

        set_debug_log(false);
        debug_log(|| "关闭时不该出现".to_string());
        assert!(
            seen().lock().unwrap().is_empty(),
            "关闭时不得产生任何日志记录"
        );

        set_debug_log(true);
        debug_log(|| "调试探针-MARKER".to_string());
        set_debug_log(false);
        let rec = {
            let guard = seen().lock().unwrap();
            guard
                .iter()
                .find(|r| r.msg.contains("调试探针-MARKER"))
                .map(|r| (r.target.clone(), r.level))
        }
        .expect("开启时调试日志必须到达日志器（改成 debug 级会被 max_level=Info 丢掉）");
        assert_eq!(rec.0, DEBUG_LOG_TARGET);
        assert_eq!(rec.1, log::Level::Info);
    }

    #[test]
    fn audit_log_appends_json_lines() {
        let dir = std::env::temp_dir().join(format!("rs-audit-test-{}", std::process::id()));
        let path = dir.join("agent-audit.log");
        let _ = std::fs::remove_file(&path);
        audit_log(&path, "ai_cmd", "ls -la", Risk::Safe, true);
        audit_log(&path, "quick_cmd", "rm -rf /", Risk::Blocked, false);
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        let v: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(v["risk"], "blocked");
        assert_eq!(v["executed"], false);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn context_wrapping() {
        assert_eq!(
            with_context("帮我看看", None, Some("err line")),
            "[当前终端上下文（最近输出，可能截断）]\nerr line\n\n[用户]\n帮我看看"
        );
        assert_eq!(with_context("hi", None, Some("   ")), "[用户]\nhi");
        assert_eq!(with_context("hi", None, None), "[用户]\nhi");
    }

    /// 造一个「已连接的 SSH 会话」画像（样本与系统概况都齐）。
    fn ssh_snapshot() -> TerminalSnapshot {
        use crate::metrics::{Sample, SystemOverview};
        TerminalSnapshot {
            title: "生产 1".into(),
            kind: crate::session::SessionKind::Ssh,
            profile_id: Some("p-1".into()),
            target: Some("root@10.0.0.5:22".into()),
            state: crate::session::SessionState::Connected,
            metrics: Some(Sample {
                mem_used: 3 * 1024 * 1024 * 1024,
                mem_total: 8 * 1024 * 1024 * 1024,
                mem_percent: 37.5,
                cpu_percent: 12.3,
                ..Sample::default()
            }),
            overview: Some(std::sync::Arc::new(SystemOverview {
                hostname: "web-1".into(),
                uptime_secs: 3 * 86_400 + 2 * 3_600 + 5 * 60,
                ..SystemOverview::default()
            })),
        }
    }

    /// 用户点名的场景：「当前终端的内存是多少」必须**有事实可依**。
    #[test]
    fn the_current_terminal_is_injected_as_facts() {
        let snap = ssh_snapshot();
        let out = with_context("当前终端的内存是多少", Some(&snap), None);
        assert!(out.contains("内存 3G/8G (37.50%)"), "{out}");
        assert!(out.contains("CPU 12.3%"), "{out}");
        assert!(out.contains("连接 id：p-1"), "{out}");
        assert!(out.contains("root@10.0.0.5:22"), "{out}");
        assert!(out.contains("主机 web-1"), "{out}");
        assert!(out.contains("已运行 3天2时5分0秒"), "{out}");
        // 用户原话排在最后，`[用户]` 分隔行不能少（会话标题与「重新生成」按它切）
        assert!(out.ends_with("[用户]\n当前终端的内存是多少"), "{out}");
        // 终端最近输出与画像各自独立：两段事实都要在
        let both = with_context("看看", Some(&snap), Some("err line"));
        assert!(both.contains("内存 3G/8G"), "{both}");
        assert!(
            both.contains("[当前终端上下文（最近输出，可能截断）]\nerr line"),
            "{both}"
        );
    }

    /// 断开 / 本地：不许编造事实。
    #[test]
    fn a_disconnected_or_local_terminal_injects_no_fake_facts() {
        // 断开：存储里还留着上一次的样本，照抄会让模型拿旧数字答「现在」
        let mut snap = ssh_snapshot();
        snap.state = crate::session::SessionState::Closed;
        let out = with_context("内存多少", Some(&snap), None);
        assert!(out.contains("已断开"), "{out}");
        assert!(
            !out.contains("指标") && !out.contains("3G/8G"),
            "断开后不该再给指标：{out}"
        );

        // 本地：没有连接 id 可给，且要说明工具够不到本机（本地指标照样可用）
        let mut snap = ssh_snapshot();
        snap.kind = crate::session::SessionKind::Local;
        snap.profile_id = None;
        snap.target = None;
        snap.title = "PowerShell".into();
        let out = with_context("内存多少", Some(&snap), None);
        assert!(!out.contains("连接 id"), "{out}");
        assert!(out.contains("类型：本地"), "{out}");
        assert!(out.contains("够不到本机"), "{out}");
        assert!(out.contains("内存 3G/8G"), "本地也有指标可用：{out}");
    }

    /// 没采到的那部分**跳过**，不写「—」：注入的是事实，不是界面。
    #[test]
    fn the_metrics_line_omits_what_it_does_not_know() {
        let bare = TerminalSnapshot {
            title: "t".into(),
            kind: crate::session::SessionKind::Ssh,
            profile_id: Some("p".into()),
            target: None,
            state: crate::session::SessionState::Connected,
            metrics: None,
            overview: None,
        };
        let out = with_context("q", Some(&bare), None);
        assert!(!out.contains("指标"), "什么都没有时不该出现指标行：{out}");

        // 只有概况（刚连上、还没到第一次采样）
        let mut snap = bare.clone();
        snap.overview = ssh_snapshot().overview;
        let out = with_context("q", Some(&snap), None);
        assert!(out.contains("主机 web-1"), "{out}");
        assert!(!out.contains("内存"), "{out}");
        assert!(!out.contains("—"), "不写占位符：{out}");

        // 只有样本（远端概况 15 s 才刷新一次）
        let mut snap = bare;
        snap.metrics = ssh_snapshot().metrics;
        let out = with_context("q", Some(&snap), None);
        assert!(out.contains("内存 3G/8G"), "{out}");
        assert!(!out.contains("主机"), "{out}");
    }

    // 回归：redact 非 ASCII 输入

    #[test]
    fn redact_non_ascii_no_panic_and_redacts() {
        // 'İ'.to_lowercase() 展开为 2 个字符（字节数变化），
        // 旧实现以 lower 的字节偏移切原文会 panic 或错位
        let line = "İ帯域 password=hunter2 привет";
        let out = redact(line);
        assert!(!out.contains("hunter2"), "out={out}");
        assert!(out.contains("***"));
        assert!(out.contains("İ帯域"), "非 ASCII 前缀应保留: out={out}");
    }

    #[test]
    fn redact_multibyte_before_marker_keeps_prefix() {
        let line = "日本語テキスト token: abc123 中文尾部";
        let out = redact(line);
        assert!(!out.contains("abc123"), "out={out}");
        assert!(out.contains("日本語テキスト"), "out={out}");
    }

    #[test]
    fn redact_plain_lines_unchanged() {
        assert_eq!(redact("ls -la /tmp"), "ls -la /tmp");
        assert_eq!(redact("中文日志行，无敏感词"), "中文日志行，无敏感词");
    }

    /// 最关键的护栏：新增的 id / state / created_at / injected_lines / commands
    #[test]
    fn chat_message_wire_format_stays_role_and_content_only() {
        let mut m = ChatMessage::user(42, "hello", 200);
        m.commands.push(AiCommandCard {
            cmd: "rm -rf /".into(),
            risk: Risk::Blocked,
            done: true,
        });
        let v = serde_json::to_value(&m).expect("serialize");
        let obj = v.as_object().expect("object");
        assert_eq!(obj.len(), 2, "请求体字段数变了（元数据泄漏）：{v}");
        assert_eq!(obj["role"], "user");
        assert_eq!(obj["content"], "hello");
    }

    /// 工具消息的 wire 格式：**只在这两种消息上**多出 `tool_calls` / `tool_call_id`。
    ///
    /// 这条护栏与上一条是一对：上一条保证普通消息零泄漏，这一条保证
    /// agent 循环需要的两个字段确实发得出去（漏了会导致模型收不到工具结果，
    /// 表现为「同一轮反复请求同一个工具」）。
    #[test]
    fn tool_messages_carry_calls_and_ids_on_the_wire() {
        // assistant 带工具调用
        let calls = vec![ToolCall {
            id: "call_1".into(),
            name: "inspect_load".into(),
            arguments: r#"{"group":"prod"}"#.into(),
        }];
        let a = ChatMessage::assistant_tool_calls(1, "", calls.clone());
        let v = serde_json::to_value(&a).unwrap();
        let obj = v.as_object().unwrap();
        assert_eq!(
            obj.len(),
            3,
            "assistant 工具消息应为 role/content/tool_calls：{v}"
        );
        assert_eq!(obj["role"], "assistant");
        assert!(
            obj.get("tool_call_id").is_none(),
            "assistant 不该带 tool_call_id"
        );
        assert_eq!(v["tool_calls"][0]["id"], "call_1");
        assert_eq!(v["tool_calls"][0]["function"]["name"], "inspect_load");
        assert_eq!(
            v["tool_calls"][0]["function"]["arguments"],
            r#"{"group":"prod"}"#
        );

        // 工具结果
        let t = ChatMessage::tool(2, "call_1", "cpu 42%");
        let v = serde_json::to_value(&t).unwrap();
        let obj = v.as_object().unwrap();
        assert_eq!(obj.len(), 3, "工具结果应为 role/content/tool_call_id：{v}");
        assert_eq!(obj["role"], "tool");
        assert_eq!(obj["tool_call_id"], "call_1");
        assert!(obj.get("tool_calls").is_none(), "工具结果不该带 tool_calls");

        // 普通 assistant 消息仍然只有两个字段
        let plain = ChatMessage::assistant(3, "结论");
        assert_eq!(
            serde_json::to_value(&plain)
                .unwrap()
                .as_object()
                .unwrap()
                .len(),
            2
        );
    }

    /// `Role::Tool` 的 serde 线格式。
    #[test]
    fn tool_role_serializes_lowercase() {
        assert_eq!(
            serde_json::to_value(ChatMessage::tool(1, "c", "x")).unwrap()["role"],
            "tool"
        );
    }

    /// 流式 `tool_calls` 增量：标准碎片流（按 index 分段拼接）。
    #[test]
    fn tool_call_accumulator_joins_fragmented_arguments() {
        let mut acc = ToolCallAccumulator::new();
        // OpenAI 实际下发的形态：第一段给 id/name，后续段只给 arguments 片段
        acc.push_delta(&ureq::json!({
            "index": 0,
            "id": "call_abc",
            "function": {"name": "inspect_log", "arguments": ""}
        }));
        acc.push_delta(&ureq::json!({
            "index": 0,
            "function": {"arguments": "{\"path\":\"/var/log/"}
        }));
        acc.push_delta(&ureq::json!({
            "index": 0,
            "function": {"arguments": "app.log\"}"}
        }));
        let calls = acc.finish();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_abc");
        assert_eq!(calls[0].name, "inspect_log");
        assert_eq!(calls[0].arguments, r#"{"path":"/var/log/app.log"}"#);
        assert_eq!(calls[0].args().unwrap()["path"], "/var/log/app.log");
    }

    /// 流式 `tool_calls` 增量：并行多次调用（index 交错），顺序必须稳定。
    #[test]
    fn tool_call_accumulator_keeps_parallel_calls_separate_and_ordered() {
        let mut acc = ToolCallAccumulator::new();
        acc.push_delta(&ureq::json!({
            "index": 1, "id": "b", "function": {"name": "second", "arguments": "{}"}
        }));
        acc.push_delta(&ureq::json!({
            "index": 0, "id": "a", "function": {"name": "first", "arguments": "{}"}
        }));
        let calls = acc.finish();
        assert_eq!(calls.len(), 2);
        // 按 index 升序，与下发顺序无关
        assert_eq!(calls[0].name, "first");
        assert_eq!(calls[1].name, "second");
    }

    /// 部分网关一次性给完整对象（非碎片）；也要能收下。
    #[test]
    fn tool_call_accumulator_accepts_single_shot_payload() {
        let mut acc = ToolCallAccumulator::new();
        acc.push_delta(&ureq::json!({
            "index": 0,
            "id": "call_x",
            "function": {"name": "inspect_load", "arguments": {"group": "prod", "timeout_secs": 30}}
        }));
        let calls = acc.finish();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "inspect_load");
        // 对象形态的参数被序列化成 JSON 文本，仍能解析
        let args = calls[0].args().unwrap();
        assert_eq!(args["group"], "prod");
        assert_eq!(args["timeout_secs"], 30);
    }

    /// 没有 id / 没有 index 的退化形态：补合成 id、归到 0 号槽。
    #[test]
    fn tool_call_accumulator_tolerates_missing_ids() {
        let mut acc = ToolCallAccumulator::new();
        acc.push_delta(&ureq::json!({"function": {"name": "t", "arguments": "{}"}}));
        let calls = acc.finish();
        assert_eq!(calls.len(), 1, "缺 id 不能把调用整个丢掉");
        assert_eq!(calls[0].id, "call_0");
        assert_eq!(calls[0].name, "t");
    }

    /// 空增量 / 无名增量不应产出调用。
    #[test]
    fn tool_call_accumulator_ignores_empty_and_nameless() {
        let mut acc = ToolCallAccumulator::new();
        assert!(acc.is_empty());
        // 只给 arguments 不给 name：无法执行，丢弃
        acc.push_delta(&ureq::json!({"index": 0, "function": {"arguments": "{}"}}));
        assert!(acc.finish().is_empty());

        let mut acc = ToolCallAccumulator::new();
        acc.push_delta(&ureq::json!({"index": 0, "function": {"name": "  ", "arguments": "{}"}}));
        assert!(acc.finish().is_empty());
    }

    /// 无参工具：arguments 为空串时视为空对象，而不是解析错误。
    #[test]
    fn tool_call_with_empty_arguments_parses_as_empty_object() {
        let c = ToolCall {
            id: "x".into(),
            name: "list_targets".into(),
            arguments: String::new(),
        };
        assert_eq!(c.args().unwrap(), serde_json::json!({}));
        // 语法错的 JSON 要给出可读原因（回注给模型自纠）
        let bad = ToolCall {
            id: "x".into(),
            name: "t".into(),
            arguments: "{not json".into(),
        };
        let err = bad.args().unwrap_err();
        assert!(err.contains("JSON"), "err={err}");
    }

    /// 流式响应只在 `tools` 非空时附带工具字段 —— 纯问答路径必须无变化。
    #[test]
    fn request_body_has_no_tool_fields_for_plain_chat() {
        // 这条断言针对 stream_chat 的构造逻辑：空 tools 不写 tools/tool_choice。
        // 用一份等价的最小构造复现分支条件即可，无需真的发请求。
        let tools: Vec<serde_json::Value> = Vec::new();
        let mut body = serde_json::json!({"model": "m", "messages": [], "stream": true});
        if !tools.is_empty() {
            body["tools"] = serde_json::Value::Array(tools.clone());
            body["tool_choice"] = serde_json::json!("auto");
        }
        assert!(body.get("tools").is_none());
        assert!(body.get("tool_choice").is_none());

        let tools = vec![serde_json::json!({"type": "function"})];
        if !tools.is_empty() {
            body["tools"] = serde_json::Value::Array(tools);
            body["tool_choice"] = serde_json::json!("auto");
        }
        assert!(body.get("tools").is_some());
        assert!(body.get("tool_choice").is_some());
    }

    /// role 的 serde 线格式须仍为小写。
    #[test]
    fn role_serializes_lowercase() {
        assert_eq!(
            serde_json::to_value(ChatMessage::system(0, "s")).unwrap()["role"],
            "system"
        );
        assert_eq!(
            serde_json::to_value(ChatMessage::user(1, "u", 0)).unwrap()["role"],
            "user"
        );
        assert_eq!(
            serde_json::to_value(ChatMessage::assistant(2, "a")).unwrap()["role"],
            "assistant"
        );
    }

    /// 流式占位消息：空白正文 +`Streaming`状态（renderer 据此追加打字机光标）。
    #[test]
    fn assistant_streaming_starts_empty_and_streaming() {
        let m = ChatMessage::assistant_streaming(7);
        assert!(m.content.is_empty());
        assert_eq!(m.state, MsgState::Streaming);
        assert!(!m.role.is_user());
    }

    /// 用户消息记录本此注入的上下文行数，且默认状态为 Done（不显示光标）。
    #[test]
    fn user_message_records_injected_lines() {
        let m = ChatMessage::user(3, "q", 200);
        assert_eq!(m.injected_lines, 200);
        assert_eq!(m.state, MsgState::Done);
        assert!(m.role.is_user());
        assert!(m.commands.is_empty());
        // created_at 取系统时间，不应为 0（取不到才退化为 0）
        assert!(m.created_at > 1_600_000_000, "created_at={}", m.created_at);
    }
}
