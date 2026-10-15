//! AI Agent：多步工具循环的状态机。
//!
//! 职责边界（这一点决定了可测性）：
//! * **本模块**决定「下一步该干什么」——要不要继续、该回注什么、何时收尾，
//!   以及把工具调用与结果固化成一条可渲染的轨迹；
//! * **调用方**（core 的 `runner` 或 UI 层）负责真正发 LLM 请求、真正执行工具，
//!   并把结果喂回 [`AgentSession`]。
//!
//! 于是整个循环的决策逻辑都能用假 LLM + 假后端测穿，不需要网络、不需要 GPUI。

pub mod proposal;
pub mod tools;
pub mod trace;

pub use proposal::{describe as describe_proposal, parse_schedule, to_task as proposal_to_task};

pub use tools::{
    all_tools, call_summary, display_names, parse_args, resolve_targets, tool_schemas, MetricKind,
    ProposalKind, ResolveError, TargetSelector, Tool, ToolArgs, DEFAULT_TAIL_LINES,
    DEFAULT_TOOL_TIMEOUT_SECS, MAX_TARGETS, MAX_TARGETS_PER_CALL, MAX_TOOL_TIMEOUT_SECS,
};
pub use trace::{
    mark_stopped, summary_line, truncate_detail, truncate_to, TraceRow, TraceState,
    MAX_TRACE_DETAIL_BYTES, MAX_TRACE_DETAIL_RENDER_BYTES,
};

use crate::ai::{ChatMessage, Risk, ToolCall};

/// 单步最多接受的工具调用数。
pub const MAX_CALLS_PER_STEP: usize = 4;
/// 单次工具结果回注给模型的字节上限。
pub const MAX_TOOL_RESULT_BYTES: usize = 8 * 1024;
/// 参数校验失败后允许模型自纠的次数。
pub const MAX_ARG_RETRIES: u32 = 2;

/// Agent 的统一预算（v1.1 §3.3.1）。
///
/// **为什么需要统一预算而不是只看步数**：步数只能挡住「模型反复绕圈」，
/// 挡不住「一步之内扫 50 台机器、每台回 64 KiB 输出」这种把内存与上下文一次打爆的
/// 情形。各维度各自独立耗尽，任一触顶就进入可解释的终止状态 ——
/// 而不是让某一路悄悄吃掉全部资源。
///
/// 2026-09-22 运行反馈「最大工具步数移除，不做限制」：LLM 往返轮数不再设上限
/// （原 `max_steps` 维度连同设置项一并撤掉），其余维度仍作为资源护栏保留。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentBudget {
    /// 累计工具调用次数上限
    pub max_tool_calls: u32,
    /// 单次调用覆盖的目标数上限
    pub max_targets: usize,
    /// 整轮时长上限（秒）
    pub max_duration_secs: u64,
    /// 工具原始输出累计上限（字节）—— 与模型上下文限**分开**
    pub max_output_bytes: usize,
    /// 回注模型的消息序列累计上限（字节）
    pub max_context_bytes: usize,
    /// 同时在途的 SSH 连接数上限
    pub max_ssh_connections: usize,
}

impl Default for AgentBudget {
    fn default() -> Self {
        Self {
            max_tool_calls: 32,
            max_targets: MAX_TARGETS,
            max_duration_secs: 600,
            max_output_bytes: 512 * 1024,
            max_context_bytes: 128 * 1024,
            max_ssh_connections: 8,
        }
    }
}

impl AgentBudget {
    /// 各维度的可读上限（超预算时的提示文案用它说明是哪一项耗尽）。
    pub fn describe_limit(&self, dim: BudgetDim) -> String {
        use crate::i18n::ts;
        match dim {
            BudgetDim::ToolCalls => ts(
                format!("工具调用次数上限（{} 次）", self.max_tool_calls),
                format!("tool-call limit ({})", self.max_tool_calls),
            ),
            BudgetDim::Targets => ts(
                format!("目标数上限（{} 台）", self.max_targets),
                format!("target limit ({})", self.max_targets),
            ),
            BudgetDim::Duration => ts(
                format!("时长上限（{} 秒）", self.max_duration_secs),
                format!("duration limit ({}s)", self.max_duration_secs),
            ),
            BudgetDim::OutputBytes => ts(
                format!("输出体量上限（{} KiB）", self.max_output_bytes / 1024),
                format!("output-size limit ({} KiB)", self.max_output_bytes / 1024),
            ),
            BudgetDim::ContextBytes => ts(
                format!("上下文体量上限（{} KiB）", self.max_context_bytes / 1024),
                format!("context-size limit ({} KiB)", self.max_context_bytes / 1024),
            ),
            BudgetDim::SshConnections => ts(
                format!("SSH 连接数上限（{}）", self.max_ssh_connections),
                format!("SSH connection limit ({})", self.max_ssh_connections),
            ),
        }
    }
}

/// 预算维度（哪一项耗尽）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetDim {
    ToolCalls,
    Targets,
    Duration,
    OutputBytes,
    ContextBytes,
    SshConnections,
}

/// 预算计账：记录已消耗量并判断是否触顶。
///
/// 与 [`AgentSession`] 分开是为了让「预算」可以独立单测 —— 它的规则很简单，
/// 但一旦和循环状态机缠在一起就很难验证「到底哪一项先耗尽」。
#[derive(Debug, Clone)]
pub struct BudgetLedger {
    budget: AgentBudget,
    tool_calls: u32,
    output_bytes: usize,
    context_bytes: usize,
    started_at: u64,
    /// 同时在途的 SSH 连接数峰值（由执行侧上报）
    peak_connections: usize,
}

impl BudgetLedger {
    pub fn new(budget: AgentBudget, started_at: u64) -> Self {
        Self {
            budget,
            tool_calls: 0,
            output_bytes: 0,
            context_bytes: 0,
            started_at,
            peak_connections: 0,
        }
    }

    pub fn budget(&self) -> AgentBudget {
        self.budget
    }

    pub fn tool_calls(&self) -> u32 {
        self.tool_calls
    }

    pub fn output_bytes(&self) -> usize {
        self.output_bytes
    }

    /// 记 `n` 次工具调用与对应的原始输出体量。
    pub fn note_tool_calls(&mut self, n: u32, output_bytes: usize) {
        self.tool_calls = self.tool_calls.saturating_add(n);
        self.output_bytes = self.output_bytes.saturating_add(output_bytes);
    }

    /// 记一次回注模型的消息体量。
    pub fn note_context_bytes(&mut self, bytes: usize) {
        self.context_bytes = self.context_bytes.saturating_add(bytes);
    }

    /// 上报一次在途连接数（执行侧每开一批调一次）。
    pub fn note_connections(&mut self, live: usize) {
        self.peak_connections = self.peak_connections.max(live);
    }

    /// 当前是否触顶；返回**第一个**耗尽的维度。
    ///
    /// 顺序是刻意的：先查「硬性资源」（时长 / 输出 / 上下文），再查「次数」。
    /// 反过来会让「已经跑爆内存但步数还够」的情形被判成「步数未超」而继续跑。
    pub fn exhausted(&self, now: u64) -> Option<BudgetDim> {
        if now.saturating_sub(self.started_at) >= self.budget.max_duration_secs {
            return Some(BudgetDim::Duration);
        }
        if self.output_bytes >= self.budget.max_output_bytes {
            return Some(BudgetDim::OutputBytes);
        }
        if self.context_bytes >= self.budget.max_context_bytes {
            return Some(BudgetDim::ContextBytes);
        }
        if self.tool_calls >= self.budget.max_tool_calls {
            return Some(BudgetDim::ToolCalls);
        }
        if self.peak_connections > self.budget.max_ssh_connections {
            return Some(BudgetDim::SshConnections);
        }
        None
    }

    /// 一次调用是否超出单次目标数上限。
    pub fn targets_exceeded(&self, targets: usize) -> bool {
        targets > self.budget.max_targets
    }
}

/// 工具可见性策略（v1.1 §3.6.1）。
///
/// **可见性优先于执行时拦截**：把一个工具暴露给模型再拒绝它，模型会反复尝试
/// 并浪费步数；从 `tools[]` 里拿掉才是干净的。但执行层**仍然**要再判一次 ——
/// 这是双层防线，不是二选一（模型可能凭记忆调用一个不在清单里的工具名）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolPolicy {
    /// 允许变更类工具（`run_command` 判为 Confirm、`download_file`、`propose_task`）
    pub allow_mutation: bool,
    /// 允许下载类工具
    pub allow_download: bool,
    /// 允许网络类工具（`check_reachability` / `check_certificate`）
    pub allow_network: bool,
    /// 允许执行类工具（`run_command`）
    pub allow_exec: bool,
}

impl Default for ToolPolicy {
    fn default() -> Self {
        Self {
            allow_mutation: false,
            allow_download: false,
            allow_network: true,
            allow_exec: true,
        }
    }
}

impl ToolPolicy {
    /// 交互式问答：全量工具（执行时再按风险确认）。
    pub fn interactive() -> Self {
        Self {
            allow_mutation: true,
            allow_download: true,
            allow_network: true,
            allow_exec: true,
        }
    }

    /// AI 定时巡检：只读清单 + `run_command`（v1.1 §3.6.1 表）。
    ///
    /// `run_command` 保留：巡检里「看看磁盘占用」是常见需求，而它执行时会再过
    /// `assess_command`，Confirm/Blocked 一律拦下。真正去掉的是下载与任务草案 ——
    /// 它们要么写本地磁盘、要么产生任务，不属于「巡检」。
    pub fn inspect_only() -> Self {
        Self {
            allow_mutation: false,
            allow_download: false,
            allow_network: true,
            allow_exec: true,
        }
    }

    /// Dry Run：只做参数解析与风险评估，不执行任何东西。
    pub fn dry_run() -> Self {
        Self {
            allow_mutation: false,
            allow_download: false,
            allow_network: false,
            allow_exec: false,
        }
    }

    /// 该策略下模型能看到的工具。
    pub fn allowed_tools(&self) -> Vec<Tool> {
        all_tools()
            .into_iter()
            .filter(|t| self.allows(*t))
            .collect()
    }

    /// 单个工具是否可见。
    pub fn allows(&self, tool: Tool) -> bool {
        match tool {
            // 只读巡检类：任何策略下都给（它们只读远端状态，不改变任何东西）
            Tool::ListTargets
            | Tool::InspectLoad
            | Tool::InspectLog
            | Tool::ReadFile
            | Tool::InspectService
            | Tool::InspectPorts
            | Tool::FindProcess => true,
            // 两者都要**主动发起连接**（前者从本机连目标，后者让服务器连自己或
            // 指定地址），所以随「允许网络类工具」收放 —— Dry Run 下应当彻底不联网。
            Tool::CheckReachability | Tool::CheckCertificate => self.allow_network,
            Tool::RunCommand => self.allow_exec,
            Tool::DownloadFile => self.allow_download,
            Tool::ProposeTask => self.allow_mutation,
        }
    }

    /// 被策略挡下的工具名（配置界面与轨迹上说明「为什么它没出现」）。
    pub fn blocked_tools(&self) -> Vec<&'static str> {
        all_tools()
            .into_iter()
            .filter(|t| !self.allows(*t))
            .map(|t| t.name())
            .collect()
    }
}

/// 用策略过滤后的工具 Schema 清单（**发给 provider 的那一份**）。
///
/// 与 [`tool_schemas`] 的区别就是「可见性」：这里只出策略允许的工具。
pub fn tool_schemas_for(policy: &ToolPolicy) -> Vec<serde_json::Value> {
    tool_schemas()
        .into_iter()
        .filter(|s| {
            let name = s["function"]["name"].as_str().unwrap_or_default();
            Tool::from_name(name)
                .map(|t| policy.allows(t))
                .unwrap_or(true)
        })
        .collect()
}

/// 一步里单次工具调用的结果。
#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutcome {
    pub call_id: String,
    pub tool: Tool,
    /// 人类可读的调用摘要（轨迹行显示用；renderer 不做解析）
    pub summary: String,
    pub status: ToolStatus,
    /// 回注给模型的文本（已截断、已脱敏）
    pub result_text: String,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolStatus {
    /// 已规划但尚未执行（`record_outcome` 会覆盖它）
    Pending,
    /// 参数校验通过并已执行
    Ok,
    /// 执行出错（连接失败、命令非零退出、超时…）
    Failed(String),
    /// 参数非法（模型自纠的依据）
    BadArgs(String),
    /// 风险拦截（Blocked：不可逆破坏）
    Blocked(String),
    /// 等待用户确认（Confirm 级、批量执行、或白名单不适用）
    AwaitingConfirm(String),
    /// 用户忽略/拒绝
    Rejected,
    /// 策略禁用（如 `agent_allow_mutating = false` 时的 Confirm 级工具）
    Disabled(String),
}

impl ToolStatus {
    /// 轨迹行上的一句话状态。
    pub fn label(&self) -> String {
        use crate::i18n::ts;
        match self {
            Self::Pending => ts("待执行".into(), "pending".into()),
            Self::Ok => ts("完成".into(), "done".into()),
            Self::Failed(e) => ts(format!("失败：{e}"), format!("failed: {e}")),
            Self::BadArgs(e) => ts(format!("参数错误：{e}"), format!("bad args: {e}")),
            Self::Blocked(r) => ts(format!("已拦截：{r}"), format!("blocked: {r}")),
            Self::AwaitingConfirm(_) => ts("等待确认".into(), "awaiting confirmation".into()),
            Self::Rejected => ts("已忽略".into(), "dismissed".into()),
            Self::Disabled(r) => ts(format!("已禁用：{r}"), format!("disabled: {r}")),
        }
    }

    /// 是否属于「执行成功」。
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Ok)
    }
}

/// 一个工具步（同一轮模型请求里的全部调用）。
#[derive(Debug, Clone, PartialEq)]
pub struct AgentStep {
    /// 从 1 开始的步号
    pub index: u8,
    pub outcomes: Vec<ToolOutcome>,
    /// 尚未收口的调用 id（等待确认、被截断、或还没轮到执行）。
    ///
    /// 必须显式记录而不是靠「`result_text` 为空」推断：等待确认的调用
    /// 完全可以已经带着一段说明文字，而 OpenAI 协议要求 assistant 消息里
    /// 每个 `tool_call_id` 最终都有 `role=tool` 应答 —— 漏一个下一次请求就 400。
    pub pending_calls: Vec<String>,
}

impl AgentStep {
    /// 这一行在轨迹上的汇总（如「3/3 完成 · 1.8s」）。
    pub fn summary(&self) -> String {
        use crate::i18n::ts;
        if self.has_pending_confirm() {
            let n = self.outcomes.len();
            return ts(
                format!("等待确认（{n} 次调用）"),
                format!("awaiting confirmation ({n} calls)"),
            );
        }
        let ok = self.outcomes.iter().filter(|o| o.status.is_ok()).count();
        let total = self.outcomes.len();
        let ms: u64 = self.outcomes.iter().map(|o| o.elapsed_ms).sum();
        let secs = ms as f64 / 1000.0;
        ts(
            format!("{ok}/{total} 完成 · {secs:.1}s"),
            format!("{ok}/{total} done · {secs:.1}s"),
        )
    }

    pub fn has_pending_confirm(&self) -> bool {
        self.outcomes
            .iter()
            .any(|o| matches!(o.status, ToolStatus::AwaitingConfirm(_)))
    }
}

/// 循环的下一步决策。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NextAction {
    /// 发一次 LLM 请求（带工具清单）
    RequestLlm,
    /// 等用户确认后继续（这一轮不再发请求）
    AwaitConfirm,
    /// 收尾：给出最终结论（或说明为何停下）
    Finish(FinishReason),
}

/// 为什么收尾。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinishReason {
    /// 模型不再请求工具 —— 正常结束，正文就是结论
    NoMoreTools,
    /// 预算耗尽（v1.1 §3.3.1：任一维度触顶都要可解释地终止）
    BudgetExhausted { dim: BudgetDim, detail: String },
    /// 用户点了停止
    Cancelled,
    /// 参数反复非法（模型没能自纠）
    BadArgs,
    /// 出错
    Error(String),
}

impl FinishReason {
    /// 收尾时追加在对话末尾的一句说明（让用户知道为什么停了）。
    pub fn note(&self) -> Option<String> {
        use crate::i18n::ts;
        match self {
            Self::NoMoreTools => None,
            Self::BudgetExhausted { dim, detail } => Some(ts(
                format!("已达上限，已停止：{detail}（维度 {dim:?}）"),
                format!("Limit reached, stopped: {detail} (dimension {dim:?})"),
            )),
            Self::Cancelled => Some(ts("已停止".into(), "Stopped".into())),
            Self::BadArgs => Some(ts(
                "工具参数反复非法，已停止。请换一种说法再试".into(),
                "Tool arguments kept failing to parse; stopped".into(),
            )),
            Self::Error(e) => Some(ts(format!("出错：{e}"), format!("Error: {e}"))),
        }
    }

    /// 是否为「正常结束」。
    pub fn is_normal(&self) -> bool {
        matches!(self, Self::NoMoreTools)
    }
}

/// 一次 agent 会话（多步循环的全部状态）。
///
/// `transcript` 既是要发给模型的消息序列，也是 UI 轨迹的数据源：
/// 合成一份而不是两处维护，是为了避免「界面显示的步骤」与「模型看到的上下文」
/// 出现分歧 —— 那种分歧会让用户按界面上的步骤去质问模型，而模型根本没见过它。
#[derive(Debug, Clone)]
pub struct AgentSession {
    pub transcript: Vec<ChatMessage>,
    pub steps: Vec<AgentStep>,
    /// 已发出的 LLM 请求次数（= 步号来源）
    pub llm_rounds: u8,
    /// 参数非法累计次数（跨步累计；达到 [`MAX_ARG_RETRIES`] 即收尾）
    arg_errors: u32,
    /// 下一个消息 id（`ChatMessage::id` 在主 transcript 里不使用，留 0 即可；
    /// 这里保留分配器是为了将来把 transcript 直接喂给面板渲染）
    next_id: u64,
    /// 统一预算计账（v1.1 §3.3.1）
    ledger: BudgetLedger,
    /// 工具可见性策略（决定哪一份 tools 清单发给 provider）
    policy: ToolPolicy,
}

impl AgentSession {
    /// 新建会话（默认预算 + 交互式工具策略）。
    pub fn new(system_prompt: impl Into<String>) -> Self {
        Self::with_budget(
            system_prompt,
            AgentBudget::default(),
            ToolPolicy::interactive(),
        )
    }

    /// 新建会话（完整预算 + 工具策略）。
    pub fn with_budget(
        system_prompt: impl Into<String>,
        budget: AgentBudget,
        policy: ToolPolicy,
    ) -> Self {
        Self {
            transcript: vec![ChatMessage::system(0, system_prompt)],
            steps: Vec::new(),
            llm_rounds: 0,
            arg_errors: 0,
            next_id: 1,
            ledger: BudgetLedger::new(budget, crate::connections::unix_now_secs()),
            policy,
        }
    }

    pub fn budget_ledger(&self) -> &BudgetLedger {
        &self.ledger
    }

    pub fn budget_ledger_mut(&mut self) -> &mut BudgetLedger {
        &mut self.ledger
    }

    pub fn policy(&self) -> &ToolPolicy {
        &self.policy
    }

    /// 本次会话该发给 provider 的 tools 清单（已按策略过滤）。
    pub fn tools_for_provider(&self) -> Vec<serde_json::Value> {
        tool_schemas_for(&self.policy)
    }

    /// 追加一条用户消息（首轮指令）。
    pub fn push_user(&mut self, content: impl Into<String>) -> u64 {
        let id = self.take_id();
        self.transcript.push(ChatMessage::user(id, content, 0));
        id
    }

    /// 追加一条助手消息（非工具路径，如面板自己的提示行）。
    pub fn push_assistant_note(&mut self, content: impl Into<String>) -> u64 {
        let id = self.take_id();
        self.transcript.push(ChatMessage::assistant(id, content));
        id
    }

    fn take_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// 记录一次 LLM 请求已发出（决定下一步之前必须先调它）。
    pub fn begin_llm_round(&mut self) {
        self.llm_rounds = self.llm_rounds.saturating_add(1);
    }

    /// 模型这一轮给了最终正文（没有工具调用）。
    ///
    /// 记进 transcript 后返回 [`FinishReason::NoMoreTools`]。
    pub fn record_reply(&mut self, content: &str) -> FinishReason {
        let id = self.take_id();
        self.transcript
            .push(ChatMessage::assistant(id, content.to_string()));
        FinishReason::NoMoreTools
    }

    /// 模型这一轮请求了工具调用。
    ///
    /// 一个调用都不合法（超出单步上限的会被截掉）时返回 `None`；
    /// 截掉的调用会以「超出单步上限」的失败结果回注，让模型知道发生了什么。
    pub fn record_tool_calls(&mut self, content: &str, calls: Vec<ToolCall>) -> Option<AgentStep> {
        if calls.is_empty() {
            return None;
        }
        // assistant 消息必须原样带上模型请求的**全部**调用（含将被截掉的）：
        // OpenAI 协议要求后续每条 role=tool 消息都能在 assistant 消息里找到对应 id，
        // 少一条就会 400。被截掉的调用在下一条消息里以失败结果收口。
        let id = self.take_id();
        self.transcript.push(ChatMessage::assistant_tool_calls(
            id,
            content.to_string(),
            calls.clone(),
        ));

        let index = self.llm_rounds.max(1);
        // 超出单步上限的调用**就地判死**：它们的应答必须立刻产生，
        // 而原因要写清「为什么没执行」—— 否则模型会反复重发同一批调用。
        let outcomes: Vec<ToolOutcome> = calls
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let over_limit = i >= MAX_CALLS_PER_STEP;
                let reason = format!("单步调用数超过上限（{MAX_CALLS_PER_STEP}），本次未执行");
                ToolOutcome {
                    call_id: c.id.clone(),
                    tool: Tool::from_name(&c.name).unwrap_or(Tool::ListTargets),
                    summary: c.name.clone(),
                    status: if over_limit {
                        ToolStatus::BadArgs(reason.clone())
                    } else {
                        ToolStatus::Pending
                    },
                    result_text: if over_limit {
                        format!("未执行：{reason}")
                    } else {
                        String::new()
                    },
                    elapsed_ms: 0,
                }
            })
            .collect();
        // 待收口 = 全部调用（含就地判死的）。判死的那些不经过后端执行，
        // 由 `close_pending_calls` 或驱动方直接 `record_outcome` 收口 ——
        // 关键是**每个 call_id 最终都必须有 role=tool 应答**。
        let pending_calls: Vec<String> = outcomes.iter().map(|o| o.call_id.clone()).collect();
        self.steps.push(AgentStep {
            index,
            outcomes: outcomes.clone(),
            pending_calls,
        });
        Some(AgentStep {
            index,
            outcomes,
            pending_calls: Vec::new(),
        })
    }

    /// 记录一次工具执行结果，并把它作为 `role=tool` 消息回注给模型。
    pub fn record_outcome(&mut self, outcome: ToolOutcome) {
        // 1) 回注给模型（截断到上下文体量上限）
        let text = limit_result(&outcome.result_text);
        // 预算计账：原始输出体量按**回注前**的完整长度算，
        // 否则「截断到 8 KiB」会让输出体量永远涨不上去，预算形同虚设。
        self.ledger.note_tool_calls(1, outcome.result_text.len());
        self.ledger.note_context_bytes(text.len());
        let id = self.take_id();
        self.transcript
            .push(ChatMessage::tool(id, outcome.call_id.clone(), text));
        if matches!(outcome.status, ToolStatus::BadArgs(_)) {
            self.arg_errors = self.arg_errors.saturating_add(1);
        }
        // 2) 写进轨迹（UI 数据源）并把它从待收口集合里摘掉
        if let Some(step) = self.steps.last_mut() {
            step.pending_calls.retain(|c| c != &outcome.call_id);
            match step
                .outcomes
                .iter_mut()
                .find(|o| o.call_id == outcome.call_id)
            {
                Some(slot) => *slot = outcome,
                None => step.outcomes.push(outcome),
            }
        } else {
            // 没有对应步（理论上不会发生）：开一步兜住，别把结果丢掉
            self.steps.push(AgentStep {
                index: self.llm_rounds.max(1),
                outcomes: vec![outcome],
                pending_calls: Vec::new(),
            });
        }
    }

    /// 把当前步里**所有未收口**的调用补上终态（如被截断的那些、被拒绝的确认）。
    ///
    /// 必须做这一步：OpenAI 协议要求 assistant 消息里的每个 `tool_call_id`
    /// 都有后续的 `role=tool` 应答，否则下一次请求直接 400，
    /// 而用户看到的是「AI 突然报错了」，无从理解。
    pub fn close_pending_calls(&mut self, reason: &str) {
        // 先克隆出待收口清单：后面要 `&mut self` 逐条回注
        let pending: Vec<(String, Tool, String)> = self
            .steps
            .last()
            .map(|s| {
                s.pending_calls
                    .iter()
                    .filter_map(|cid| {
                        s.outcomes
                            .iter()
                            .find(|o| &o.call_id == cid)
                            .map(|o| (o.call_id.clone(), o.tool, o.summary.clone()))
                    })
                    .collect()
            })
            .unwrap_or_default();
        for (call_id, tool, summary) in pending {
            self.record_outcome(ToolOutcome {
                call_id,
                tool,
                summary,
                status: ToolStatus::Failed(reason.to_string()),
                result_text: format!("未执行：{reason}"),
                elapsed_ms: 0,
            });
        }
    }

    /// 决定下一步。
    pub fn next_action(&self) -> NextAction {
        // 有等待确认的调用：停下来等人，不发请求也不收尾
        if self
            .steps
            .last()
            .map(|s| s.has_pending_confirm())
            .unwrap_or(false)
        {
            return NextAction::AwaitConfirm;
        }
        if self.arg_errors >= MAX_ARG_RETRIES {
            return NextAction::Finish(FinishReason::BadArgs);
        }
        // 统一预算维度：任一触顶都收尾（2026-09-22 起不含步数 —— 轮数不设限）
        let now = crate::connections::unix_now_secs();
        if let Some(dim) = self.ledger.exhausted(now) {
            return NextAction::Finish(FinishReason::BudgetExhausted {
                dim,
                detail: self.ledger.budget().describe_limit(dim),
            });
        }
        NextAction::RequestLlm
    }

    /// 轨迹上所有工具调用的条数（验收用：agent 至少要调过工具）。
    pub fn tool_call_count(&self) -> usize {
        self.steps.iter().map(|s| s.outcomes.len()).sum()
    }

    /// 本轮是否还有等待用户确认的调用。
    pub fn awaiting_confirm(&self) -> bool {
        self.steps
            .last()
            .map(|s| s.has_pending_confirm())
            .unwrap_or(false)
    }

    /// 等待确认的那批调用的展示名（卡片上要写清「将在 N 台上执行」）。
    pub fn pending_confirm_labels(&self) -> Vec<String> {
        self.steps
            .last()
            .map(|s| {
                s.outcomes
                    .iter()
                    .filter(|o| matches!(o.status, ToolStatus::AwaitingConfirm(_)))
                    .map(|o| o.summary.clone())
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// 回注文本截断（首尾各半 + 省略标记，按字符边界）。
pub fn limit_result(text: &str) -> String {
    limit_result_with(text, MAX_TOOL_RESULT_BYTES)
}

/// 同 [`limit_result`]，但上限可指定。
///
/// 聚合类工具（如巡检）要按台数把总预算摊到每一台上，否则单台的输出
/// 会吃光整个上下文，后面的台数只剩个「已省略」。
pub fn limit_result_with(text: &str, max_bytes: usize) -> String {
    let max_bytes = max_bytes.max(1);
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let head = max_bytes / 2;
    let tail = max_bytes - head;
    let hs = floor_boundary(text, head);
    let ts = ceil_boundary(text, text.len() - tail);
    let omitted = text.len().saturating_sub(hs + (text.len() - ts));
    format!(
        "{}\n… (省略 {omitted} 字节) …\n{}",
        &text[..hs],
        &text[ts..]
    )
}

fn floor_boundary(s: &str, mut i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_boundary(s: &str, mut i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// 判定一次工具调用该如何处置（风险与策略的唯一裁决点）。
///
/// 这是 agent 与定时任务**共用**的安全闸门，所以放进本模块而不是各自实现：
/// 两处各写一遍的代价是「一处收紧了、另一处忘了」，而收紧的那处才是无效的。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Disposition {
    /// 直接执行
    Run,
    /// 需要用户确认（附理由，卡片上显示）
    Confirm(String),
    /// 拒绝执行（不可逆破坏）
    Blocked(String),
    /// 策略禁用（如未开启状态变更）
    Disabled(String),
}

/// 裁决参数。
#[derive(Debug, Clone, Copy)]
pub struct GuardContext {
    /// 是否允许状态变更类命令（`AiConfig::agent_allow_mutating` / 任务的同名开关）
    pub allow_mutating: bool,
    /// 是否有人在场可以确认（定时任务 = false）
    pub interactive: bool,
    /// 本次调用覆盖的目标数（批量时永远要确认，白名单不适用）
    pub target_count: usize,
    /// 该命令是否在白名单里
    pub whitelisted: bool,
}

/// 裁决一次工具调用。
///
/// 规则（见需求文档 §7.4）：
/// 1. `Blocked` 风险 —— 永远拒绝，白名单也不能放行；
/// 2. `Confirm` 风险 —— 需要有人确认；无人值守下默认拒绝（除非显式开启且…）；
/// 3. 只读工具 —— 直接执行；
/// 4. 白名单 —— **只对单目标生效**；批量（>1 台）永不走白名单。
pub fn judge(tool: Tool, args: &ToolArgs, ctx: GuardContext) -> Disposition {
    use crate::i18n::ts;

    // 巡检类只读工具：永远直接执行
    if !tool.mutating() {
        return Disposition::Run;
    }

    // 命令类：先看风险级
    let risk = match args {
        ToolArgs::RunCommand { risk, .. } => *risk,
        // 下载与任务草案固定按 Confirm 处理
        _ => Risk::Confirm,
    };

    match risk {
        Risk::Blocked => Disposition::Blocked(ts(
            "该命令属于不可逆破坏操作，已被安全策略永久拦截".into(),
            "This command is irreversibly destructive and permanently blocked".into(),
        )),
        Risk::Confirm => {
            // 单目标 + 白名单 + 有人在场：直接跑（保持既有白名单手感）
            if ctx.whitelisted && ctx.target_count <= 1 && ctx.interactive {
                return Disposition::Run;
            }
            // 先判「无人值守」再判开关：两个原因对应的**处置动作**不同 ——
            // 没人可确认时却提示「去开启允许状态变更命令」是误导（开了也还是没人确认）。
            if !ctx.interactive {
                return Disposition::Disabled(ts(
                    "无人值守场景不允许执行状态变更命令（定时任务与 AI 巡检默认只读）".into(),
                    "state-changing commands are not allowed unattended".into(),
                ));
            }
            if !ctx.allow_mutating {
                return Disposition::Disabled(ts(
                    "状态变更命令未启用（可在设置中开启「允许状态变更命令」）".into(),
                    "State-changing commands are disabled in settings".into(),
                ));
            }
            if ctx.target_count > 1 {
                return Disposition::Confirm(ts(
                    format!(
                        "将在 {} 台上执行（批量执行不使用白名单，必须每次确认）",
                        ctx.target_count
                    ),
                    format!(
                        "Will run on {} hosts (batch runs are never whitelisted)",
                        ctx.target_count
                    ),
                ));
            }
            Disposition::Confirm(ts(
                "该操作会改变服务器状态，请确认".into(),
                "This changes server state; please confirm".into(),
            ))
        }
        Risk::Safe => {
            // 下载与任务草案虽然不改远端状态，但**会写本地磁盘 / 产生任务**，
            // 属于「用户应该看见」的动作：与状态变更命令同一档处置。
            // 它们不是命令，没有「风险级」可谈，所以走这里的显式分支。
            if args_is_download_or_proposal(args) {
                if !ctx.allow_mutating {
                    return Disposition::Disabled(ts(
                        "该操作未启用（可在设置中开启「允许状态变更命令」）".into(),
                        "This operation is disabled in settings".into(),
                    ));
                }
                if !ctx.interactive {
                    return Disposition::Disabled(ts(
                        "无人值守场景不允许该操作".into(),
                        "This operation is not allowed unattended".into(),
                    ));
                }
                return Disposition::Confirm(ts(
                    "该操作会写入本地或创建任务草案，请确认".into(),
                    "This writes locally or drafts a task; please confirm".into(),
                ));
            }
            Disposition::Run
        }
    }
}

fn args_is_download_or_proposal(args: &ToolArgs) -> bool {
    matches!(
        args,
        ToolArgs::DownloadFile { .. } | ToolArgs::ProposeTask { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> AgentSession {
        AgentSession::new("你是巡检助手")
    }

    fn call(id: &str, name: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: "{}".into(),
        }
    }

    fn outcome(id: &str, status: ToolStatus) -> ToolOutcome {
        ToolOutcome {
            call_id: id.into(),
            tool: Tool::InspectLoad,
            summary: "inspect_load prod".into(),
            status,
            result_text: "cpu 42%".into(),
            elapsed_ms: 120,
        }
    }

    #[test]
    fn session_starts_by_requesting_the_llm() {
        let mut s = session();
        s.push_user("查一下生产组");
        assert_eq!(s.next_action(), NextAction::RequestLlm);
        assert_eq!(s.tool_call_count(), 0);
        // transcript 是 system + user
        assert_eq!(s.transcript.len(), 2);
        assert_eq!(s.transcript[0].role, crate::ai::Role::System);
    }

    #[test]
    fn reply_finishes_the_loop_without_tools() {
        let mut s = session();
        s.push_user("你好");
        s.begin_llm_round();
        let reason = s.record_reply("你好，有什么可以帮你");
        assert_eq!(reason, FinishReason::NoMoreTools);
        assert!(reason.is_normal());
        assert!(reason.note().is_none());
        assert_eq!(s.next_action(), NextAction::RequestLlm, "轮次还没到上限");
    }

    #[test]
    fn tool_calls_create_a_step_and_are_fed_back() {
        let mut s = session();
        s.push_user("查负载");
        s.begin_llm_round();
        let step = s
            .record_tool_calls("我来看看", vec![call("c1", "inspect_load")])
            .expect("应当产生一步");
        assert_eq!(step.index, 1);
        assert_eq!(step.outcomes.len(), 1);
        // assistant 消息带上工具调用
        let last = s.transcript.last().unwrap();
        assert_eq!(last.role, crate::ai::Role::Assistant);
        assert_eq!(last.tool_calls.len(), 1);

        s.record_outcome(outcome("c1", ToolStatus::Ok));
        // 结果以 role=tool 回注，且 id 对得上
        let tool_msg = s.transcript.last().unwrap();
        assert_eq!(tool_msg.role, crate::ai::Role::Tool);
        assert_eq!(tool_msg.tool_call_id.as_deref(), Some("c1"));
        assert_eq!(tool_msg.content, "cpu 42%");
        assert_eq!(s.tool_call_count(), 1);
        assert!(s.steps[0].outcomes[0].status.is_ok());
    }

    #[test]
    fn calls_beyond_the_per_step_cap_are_capped_but_answered() {
        let mut s = session();
        s.begin_llm_round();
        let calls: Vec<ToolCall> = (0..6)
            .map(|i| call(&format!("c{i}"), "inspect_load"))
            .collect();
        let step = s.record_tool_calls("", calls).unwrap();
        assert_eq!(
            step.outcomes.len(),
            6,
            "全部调用都要在 assistant 消息里（协议要求）"
        );
        // 只有超出上限的那 2 个是就地判死的；前 4 个仍是待执行
        let bad = step
            .outcomes
            .iter()
            .filter(|o| matches!(o.status, ToolStatus::BadArgs(_)))
            .count();
        assert_eq!(bad, 2, "超限的 2 个应就地判死");
        let pending = step
            .outcomes
            .iter()
            .filter(|o| o.status == ToolStatus::Pending)
            .count();
        assert_eq!(pending, 4, "前 4 个应处于待执行");

        // 逐条收口：给前 4 个正常结果，后 2 个保留超限状态
        for i in 0..4 {
            s.record_outcome(outcome(&format!("c{i}"), ToolStatus::Ok));
        }
        // 未收口的补上（含被截断的两个）
        s.close_pending_calls("超出单步上限");
        // 每个 call_id 都必须有 role=tool 应答，否则下一次请求会 400
        for i in 0..6 {
            let id = format!("c{i}");
            assert!(
                s.transcript.iter().any(|m| m.role == crate::ai::Role::Tool
                    && m.tool_call_id.as_deref() == Some(id.as_str())),
                "{id} 没有对应的 tool 应答"
            );
        }
    }

    #[test]
    fn close_pending_calls_answers_every_unfinished_call() {
        let mut s = session();
        s.begin_llm_round();
        s.record_tool_calls(
            "",
            vec![call("a", "inspect_load"), call("b", "inspect_log")],
        )
        .unwrap();
        s.record_outcome(outcome("a", ToolStatus::Ok));
        s.close_pending_calls("被截断");
        assert!(s
            .transcript
            .iter()
            .any(|m| m.tool_call_id.as_deref() == Some("b") && m.role == crate::ai::Role::Tool));
        // 已收口的不会被改
        assert_eq!(
            s.transcript
                .iter()
                .filter(|m| m.role == crate::ai::Role::Tool)
                .count(),
            2
        );
    }

    /// 2026-09-22「最大工具步数移除，不做限制」：LLM 往返轮数不再有上限
    /// （旧默认 8 步、硬顶 16 步）。跑到 20 轮仍应继续发请求。
    #[test]
    fn llm_rounds_are_no_longer_capped() {
        let mut s = session();
        s.push_user("继续查");
        for i in 0..20 {
            s.begin_llm_round();
            s.record_tool_calls("", vec![call(&format!("c{i}"), "inspect_load")])
                .unwrap();
            s.record_outcome(outcome(&format!("c{i}"), ToolStatus::Ok));
        }
        assert_eq!(
            s.next_action(),
            NextAction::RequestLlm,
            "轮数不该再有闸门（只有资源类预算能拦下它）"
        );
    }

    #[test]
    fn awaiting_confirm_pauses_the_loop() {
        let mut s = session();
        s.begin_llm_round();
        s.record_tool_calls("", vec![call("c1", "run_command")])
            .unwrap();
        s.record_outcome(outcome(
            "c1",
            ToolStatus::AwaitingConfirm("将在 3 台上执行".into()),
        ));
        assert!(s.awaiting_confirm());
        assert_eq!(s.next_action(), NextAction::AwaitConfirm);
        assert_eq!(
            s.pending_confirm_labels(),
            vec!["inspect_load prod".to_string()]
        );
        assert_eq!(s.steps[0].summary(), {
            use crate::i18n::ts;
            ts(
                format!("等待确认（{} 次调用）", 1),
                format!("awaiting confirmation ({})", 1),
            )
        });
    }

    #[test]
    fn resolving_the_confirm_resumes_the_loop() {
        let mut s = session();
        s.begin_llm_round();
        s.record_tool_calls("", vec![call("c1", "run_command")])
            .unwrap();
        s.record_outcome(outcome("c1", ToolStatus::AwaitingConfirm("确认".into())));
        assert_eq!(s.next_action(), NextAction::AwaitConfirm);
        // 用户确认后写入终态，循环恢复
        s.record_outcome(outcome("c1", ToolStatus::Ok));
        assert!(!s.awaiting_confirm());
        assert_eq!(s.next_action(), NextAction::RequestLlm);
    }

    #[test]
    fn repeated_bad_args_stop_the_loop() {
        let mut s = session();
        for i in 0..MAX_ARG_RETRIES {
            s.begin_llm_round();
            s.record_tool_calls("", vec![call(&format!("c{i}"), "inspect_log")])
                .unwrap();
            s.record_outcome(outcome(
                &format!("c{i}"),
                ToolStatus::BadArgs("缺少必填参数 path".into()),
            ));
        }
        match s.next_action() {
            NextAction::Finish(FinishReason::BadArgs) => {}
            other => panic!("参数反复非法应收尾：{other:?}"),
        }
        // 成功一步会把计数清掉（模型自纠成功）
        let mut s2 = session();
        s2.begin_llm_round();
        s2.record_tool_calls("", vec![call("a", "inspect_log")])
            .unwrap();
        s2.record_outcome(outcome("a", ToolStatus::BadArgs("x".into())));
        s2.begin_llm_round();
        s2.record_tool_calls("", vec![call("b", "inspect_log")])
            .unwrap();
        assert_eq!(s2.next_action(), NextAction::RequestLlm);
    }

    #[test]
    fn empty_tool_call_list_is_not_a_step() {
        let mut s = session();
        s.begin_llm_round();
        assert!(s.record_tool_calls("只是文字", vec![]).is_none());
        assert!(s.steps.is_empty());
    }

    #[test]
    fn tool_result_is_truncated_within_context_budget() {
        let big = "汉".repeat(MAX_TOOL_RESULT_BYTES); // 3 字节/字，远超上限
        let out = limit_result(&big);
        assert!(out.len() < big.len());
        assert!(out.contains("省略"));
        assert!(!out.contains('\u{FFFD}'), "截断必须落在字符边界");
        // 小文本不动
        assert_eq!(limit_result("ok"), "ok");
    }

    #[test]
    fn step_summary_counts_successes() {
        let step = AgentStep {
            index: 1,
            outcomes: vec![
                outcome("a", ToolStatus::Ok),
                outcome("b", ToolStatus::Ok),
                outcome("c", ToolStatus::Failed("boom".into())),
            ],
            pending_calls: Vec::new(),
        };
        let s = step.summary();
        assert!(s.contains("2/3"), "summary={s}");
        assert!(!step.has_pending_confirm());
    }

    // 安全裁决

    /// v1.1 §3.3.1 / AC-41：七个预算维度任一耗尽都要安全终止。
    #[test]
    fn every_budget_dimension_terminates_the_loop() {
        // 用「把某一维上限压到极小」的方式逐个触发，而不是造真实负载 ——
        // 这样用例不依赖时序与数据量，跑得快也不会偶发。
        // 别名只是为了让 clippy 的 type_complexity 闭嘴：三元组本身语义清楚
        type BudgetCase = (AgentBudget, fn(&mut BudgetLedger), BudgetDim);
        let cases: Vec<BudgetCase> = vec![
            (
                AgentBudget {
                    max_tool_calls: 1,
                    ..Default::default()
                },
                |l| l.note_tool_calls(2, 0),
                BudgetDim::ToolCalls,
            ),
            (
                AgentBudget {
                    max_output_bytes: 10,
                    ..Default::default()
                },
                |l| l.note_tool_calls(1, 11),
                BudgetDim::OutputBytes,
            ),
            (
                AgentBudget {
                    max_context_bytes: 10,
                    ..Default::default()
                },
                |l| l.note_context_bytes(11),
                BudgetDim::ContextBytes,
            ),
            (
                AgentBudget {
                    max_ssh_connections: 1,
                    ..Default::default()
                },
                |l| l.note_connections(2),
                BudgetDim::SshConnections,
            ),
        ];
        for (budget, consume, want) in cases {
            let mut l = BudgetLedger::new(budget, 1_000);
            consume(&mut l);
            assert_eq!(l.exhausted(1_000), Some(want), "该维度耗尽必须被识别出来");
            // 提示文案要说明是哪一项、上限多少
            let note = FinishReason::BudgetExhausted {
                dim: want,
                detail: budget.describe_limit(want),
            }
            .note()
            .expect("超预算收尾必须给出说明");
            assert!(!note.is_empty());
        }

        // 时长维度按「开始时刻 + 上限」判，与数据量无关
        let l = BudgetLedger::new(
            AgentBudget {
                max_duration_secs: 60,
                ..Default::default()
            },
            1_000,
        );
        assert_eq!(l.exhausted(1_059), None, "未到时长上限不该终止");
        assert_eq!(l.exhausted(1_060), Some(BudgetDim::Duration));

        // 未超任何维度时不该终止
        let mut l = BudgetLedger::new(AgentBudget::default(), 1_000);
        l.note_tool_calls(1, 100);
        assert_eq!(l.exhausted(1_000), None);
    }

    /// 预算耗尽时 `next_action` 必须收尾，而不是继续发请求。
    #[test]
    fn session_stops_when_the_budget_is_exhausted() {
        let budget = AgentBudget {
            max_output_bytes: 8,
            ..AgentBudget::default()
        };
        let mut s = AgentSession::with_budget("p", budget, ToolPolicy::interactive());
        s.begin_llm_round();
        s.record_tool_calls("", vec![call("c1", "inspect_load")])
            .unwrap();
        s.record_outcome(ToolOutcome {
            call_id: "c1".into(),
            tool: Tool::InspectLoad,
            summary: "x".into(),
            status: ToolStatus::Ok,
            // 回注会被截断到 8 KiB，但**输出体量**按原始长度记账 ——
            // 否则「截断」会让这条预算永远涨不上去（这正是要防的）。
            result_text: "y".repeat(64),
            elapsed_ms: 1,
        });
        match s.next_action() {
            NextAction::Finish(FinishReason::BudgetExhausted { dim, .. }) => {
                assert_eq!(dim, BudgetDim::OutputBytes)
            }
            other => panic!("超输出预算必须收尾：{other:?}"),
        }
    }

    /// AC-41 的另一半：目标数上限。
    #[test]
    fn targets_budget_is_reported_separately() {
        let l = BudgetLedger::new(AgentBudget::default(), 0);
        assert!(!l.targets_exceeded(MAX_TARGETS));
        assert!(l.targets_exceeded(MAX_TARGETS + 1));
        let small = BudgetLedger::new(
            AgentBudget {
                max_targets: 2,
                ..Default::default()
            },
            0,
        );
        assert!(small.targets_exceeded(3));
    }

    /// v1.1 §3.6.1 / AC-43：**可见性**过滤 —— 禁止的工具不出现在 `tools[]` 里。
    #[test]
    fn tool_policy_filters_the_provider_tool_list() {
        // 交互式：十二个工具全给
        let all = tool_schemas_for(&ToolPolicy::interactive());
        assert_eq!(all.len(), 12);

        // AI 巡检：不给下载与任务草案
        let inspect = tool_schemas_for(&ToolPolicy::inspect_only());
        let names: Vec<&str> = inspect
            .iter()
            .map(|s| s["function"]["name"].as_str().unwrap_or_default())
            .collect();
        assert!(names.contains(&"list_targets"));
        assert!(names.contains(&"inspect_load"));
        assert!(names.contains(&"inspect_log"));
        assert!(names.contains(&"check_reachability"));
        // 巡检域新增的只读工具在无人值守下也必须给（证书到期、服务状态
        // 恰恰是定时巡检最该看的东西）
        for t in [
            "read_file",
            "inspect_service",
            "inspect_ports",
            "check_certificate",
            "find_process",
        ] {
            assert!(names.contains(&t), "巡检上下文缺 {t}");
        }
        assert!(
            !names.contains(&"download_file"),
            "巡检上下文不该把下载工具暴露给模型"
        );
        assert!(!names.contains(&"propose_task"));
        assert_eq!(inspect.len(), 10, "只读九件套 + run_command");

        // Dry Run：只给纯读状态、不连网也不执行的那些
        let dry = tool_schemas_for(&ToolPolicy::dry_run());
        assert_eq!(dry.len(), 7, "Dry Run 只保留七个纯只读检查工具");
        let dry_names: Vec<&str> = dry
            .iter()
            .map(|s| s["function"]["name"].as_str().unwrap_or_default())
            .collect();
        assert!(
            !dry_names.contains(&"check_reachability") && !dry_names.contains(&"check_certificate"),
            "Dry Run 不该连网：{dry_names:?}"
        );

        // 被挡下的工具要能列出来（UI 说明「为什么它没出现」）
        let blocked = ToolPolicy::inspect_only().blocked_tools();
        assert!(blocked.contains(&"download_file"));
        assert!(blocked.contains(&"propose_task"));

        // 默认策略：只读优先（不允许变更）
        let d = ToolPolicy::default();
        assert!(!d.allows(Tool::RunCommand) || d.allow_exec);
        assert!(!d.allows(Tool::DownloadFile));
        assert!(!d.allows(Tool::ProposeTask));

        // 会话暴露的清单与策略一致
        let s = AgentSession::with_budget("p", AgentBudget::default(), ToolPolicy::inspect_only());
        assert_eq!(s.tools_for_provider().len(), 10);
    }

    fn guard(allow_mutating: bool, interactive: bool, targets: usize, wl: bool) -> GuardContext {
        GuardContext {
            allow_mutating,
            interactive,
            target_count: targets,
            whitelisted: wl,
        }
    }

    fn cmd_args(cmd: &str) -> ToolArgs {
        ToolArgs::RunCommand {
            selector: TargetSelector::Group("g".into()),
            command: cmd.into(),
            timeout_secs: 15,
            risk: crate::ai::assess_command(cmd),
        }
    }

    fn load_args() -> ToolArgs {
        ToolArgs::InspectLoad {
            selector: TargetSelector::Group("g".into()),
            metrics: Vec::new(),
            timeout_secs: 15,
        }
    }

    #[test]
    fn read_only_tools_always_run() {
        for tool in [
            Tool::ListTargets,
            Tool::InspectLoad,
            Tool::InspectLog,
            Tool::CheckReachability,
        ] {
            assert_eq!(
                judge(tool, &load_args(), guard(false, false, 30, false)),
                Disposition::Run,
                "{tool:?} 是只读工具，必须直接执行"
            );
        }
    }

    #[test]
    fn blocked_commands_are_never_allowed_even_whitelisted() {
        for whitelisted in [false, true] {
            for allow in [false, true] {
                let d = judge(
                    Tool::RunCommand,
                    &cmd_args("rm -rf /"),
                    guard(allow, true, 1, whitelisted),
                );
                assert!(
                    matches!(d, Disposition::Blocked(_)),
                    "破坏类命令必须永久拦截（whitelisted={whitelisted}, allow={allow}）：{d:?}"
                );
            }
        }
    }

    #[test]
    fn confirm_commands_need_a_human_and_are_disabled_unattended() {
        let args = cmd_args("systemctl restart nginx");
        // 无人值守：一律禁用，即使开了 allow_mutating
        let d = judge(Tool::RunCommand, &args, guard(true, false, 1, false));
        assert!(matches!(d, Disposition::Disabled(_)), "{d:?}");
        // 有人在场但没开开关：禁用
        let d = judge(Tool::RunCommand, &args, guard(false, true, 1, false));
        assert!(matches!(d, Disposition::Disabled(_)), "{d:?}");
        // 有人在场 + 开关打开：需要确认
        let d = judge(Tool::RunCommand, &args, guard(true, true, 1, false));
        assert!(matches!(d, Disposition::Confirm(_)), "{d:?}");
    }

    #[test]
    fn whitelist_only_applies_to_single_target_interactive_runs() {
        let args = cmd_args("systemctl restart nginx");
        // 单目标 + 白名单 + 有人在场：直接跑
        assert_eq!(
            judge(Tool::RunCommand, &args, guard(true, true, 1, true)),
            Disposition::Run
        );
        // 批量：白名单不生效，必须确认
        let d = judge(Tool::RunCommand, &args, guard(true, true, 10, true));
        match d {
            Disposition::Confirm(reason) => {
                assert!(reason.contains("10"), "卡片要说清范围：{reason}")
            }
            other => panic!("批量不得走白名单：{other:?}"),
        }
        // 无人值守 + 白名单：仍然禁用（白名单不适用于无人场景）
        let d = judge(Tool::RunCommand, &args, guard(true, false, 1, true));
        assert!(matches!(d, Disposition::Disabled(_)), "{d:?}");
    }

    #[test]
    fn safe_command_runs_directly() {
        assert_eq!(
            judge(
                Tool::RunCommand,
                &cmd_args("df -h"),
                guard(false, false, 30, false)
            ),
            Disposition::Run
        );
    }

    #[test]
    fn download_and_proposal_require_confirmation_and_are_disabled_unattended() {
        let dl = ToolArgs::DownloadFile {
            profile_id: "p1".into(),
            remote_path: "/var/log/a.log".into(),
            local_dir: None,
        };
        let prop = ToolArgs::ProposeTask {
            name: "t".into(),
            schedule: "0 9 * * *".into(),
            action: ProposalKind::InspectAi,
            selector: TargetSelector::Group("g".into()),
            prompt: None,
            command: None,
        };
        // 下载与草案：与状态变更命令同一档 —— 未开启时禁用、开启且有人在场才确认
        assert!(matches!(
            judge(Tool::DownloadFile, &dl, guard(true, true, 1, false)),
            Disposition::Confirm(_)
        ));
        assert!(matches!(
            judge(Tool::DownloadFile, &dl, guard(false, true, 1, false)),
            Disposition::Disabled(_)
        ));
        assert!(matches!(
            judge(Tool::DownloadFile, &dl, guard(true, false, 1, false)),
            Disposition::Disabled(_)
        ));
        assert!(matches!(
            judge(Tool::ProposeTask, &prop, guard(true, true, 1, false)),
            Disposition::Confirm(_)
        ));
        assert!(matches!(
            judge(Tool::ProposeTask, &prop, guard(false, false, 1, false)),
            Disposition::Disabled(_)
        ));
    }
}
