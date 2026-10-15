//! AI 会话落盘（`<config>/rustshell/ai_chats.json`）：多条会话与各自的消息一起存。
//!
//! 需求（2026-09-23 运行反馈）：「AI对话需要保存，目前是软件关停就没了。」
//! 此前 `ai_chats` 只活在内存里，关窗即失忆。
//!
//! **为什么不给 `ChatMessage` 加存储用的 serde 实现**：它**已经有** serde
//! 实现，但那是**线上格式**（发给模型的请求体）—— `core/src/ai.rs` 里除
//! `role` / `content`（agent 循环再多 `tool_calls` / `tool_call_id`）外全部
//! `#[serde(skip)]`，并有护栏 `chat_message_wire_format_stays_role_and_content_only`
//! 钉着「请求体字段数不许变」。而落盘要的恰恰是被 skip 掉的那批**面板元数据**
//! （id / created_at / injected_lines / reasoning / trace / commands）：少了它们，
//! 恢复出来的对话没有轨迹、没有思考过程、消息 id 也要重排。所以这里另立一套
//! **存储格式**（`ChatsFile` 一族）与线上格式解耦 —— 以后改请求体不会悄悄改
//! 文件格式，反之亦然；两边靠显式转换对接，转换处顺手做两条「恢复时的规矩」：
//!
//! 1. **在途状态归零**：`Pending` / `Streaming` 是「这一刻还有请求在路上」的意思，
//!    进程都重启了，那个请求早没了。恢复成 `Cancelled`（面板显示「已停止」），
//!    比让它永远转圈或者假装已答完都诚实。
//! 2. **空占位丢掉**：进程死在首个增量之前的那种占位行（正文 / 思考 / 轨迹 /
//!    卡片全空），存下来只会变成一条空白气泡，不如不存。

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use serde::{Deserialize, Serialize};

use rustshell_core::agent::TraceRow;
use rustshell_core::ai::{AiCommandCard, ChatMessage, MsgState, Risk, Role};

use super::AiChat;

/// 文件格式版本。将来改结构时 +1：`load` 见到比自己新的版本会出声（见下），
/// 但不拒绝解析 —— 未知字段被 serde 忽略，已知字段照样能用上。
const VERSION: u32 = 1;

/// 流式期间两次落盘的最小间隔（毫秒）。
///
/// 一轮生成里每个增量都会改消息正文，逐条落盘等于每秒写几十次整份 JSON
/// （轨迹明细单条上限 64 KiB，几轮巡检就能把文件堆到 MB 级）。用户动作与
/// 收尾走的是「立即写」，不受这条节流限制。
const SAVE_MIN_GAP_MS: u64 = 3_000;

/// 启动时从磁盘读回来的会话现场。
pub(super) struct LoadedChats {
    pub chats: Vec<AiChat>,
    /// 当前会话 id（保证指向 `chats` 里存在的一条）
    pub current: u64,
    /// 会话 id 分配器（保证大于所有已存在的 id）
    pub next_chat_id: u64,
    /// `run_id → chat_id`（已剔除指向不存在会话的条目）
    pub run_chats: HashMap<String, u64>,
}

impl LoadedChats {
    /// 当前会话的消息条数。
    ///
    /// 恢复时得拿它初始化官方 `MessageScrollerState`：那份状态自带行数，
    /// 从 0 起而磁盘上有 N 条消息的话，虚拟列表会认为「一行都没有」——
    /// 表现就是「历史明明恢复了，面板却是空的」。
    pub(super) fn current_msgs_len(&self) -> usize {
        self.chats
            .iter()
            .find(|c| c.id == self.current)
            .map(|c| c.msgs.len())
            .unwrap_or(0)
    }
}

/// 生产用的会话文件路径；`None` = 本进程不该碰真实文件（见下）。
///
/// **测试构建下一律 `None`**：`MainView` 是几乎所有 UI 用例的脚手架，
/// 而测试调度器一推进时钟（`advance_clock`）`poll` 里的落盘就会真执行 ——
/// 砸的是开发机上**真实**的 `ai_chats.json`（跑一次用例就把聊天记录换成用例造的那条）。
/// 读也一样要绕开：否则「开局一条空白会话」这类既有断言会随开发机的聊天记录漂移。
/// 用例要测落盘就走 [`MainView::ai_chats_flush`] —— 它收路径参数。
fn production_path() -> Option<std::path::PathBuf> {
    if cfg!(test) {
        None
    } else {
        Some(rustshell_platform::paths::ai_chats_path())
    }
}

/// 启动时装载会话：真实文件读不出来（或测试构建）就是一条空白会话。
pub(super) fn load_for_startup() -> LoadedChats {
    match production_path() {
        Some(path) => load(&path),
        None => fresh(),
    }
}

/// 一条空白会话 + 空映射：读不到 / 读坏了 / 文件是空的都落在这里
/// （不变量：`ai_chats` 恒非空，`ai_cur()` 的落点）。
fn fresh() -> LoadedChats {
    LoadedChats {
        chats: vec![AiChat::new(1, rustshell_core::connections::unix_now_secs())],
        current: 1,
        next_chat_id: 2,
        run_chats: HashMap::new(),
    }
}

/// 读会话记录。文件不存在 / 解析失败 / 内容为空都**不 panic 也不清空用户数据**：
/// 解析失败先把原文留底成 `<path>.broken-<ts>.bak`（与其它配置同一手法），再从头开始。
pub(super) fn load(path: &Path) -> LoadedChats {
    let Some(raw) = std::fs::read_to_string(path).ok() else {
        return fresh();
    };
    match serde_json::from_str::<ChatsFile>(&raw) {
        Ok(file) => {
            if file.version > VERSION {
                log::warn!(
                    "AI 会话文件版本 {} 比本程序认识的 {VERSION} 新，按已知字段尽力读取: {}",
                    file.version,
                    path.display()
                );
            }
            normalize(file)
        }
        Err(e) => {
            log::warn!(
                "AI 会话文件解析失败（{e}），已留底并从头开始: {}",
                path.display()
            );
            rustshell_core::configio::backup_broken_config(path, &raw);
            fresh()
        }
    }
}

/// 落盘。写入方式与其它配置一致：先写同目录 `.tmp` 再 `rename` 原子替换 ——
/// 中途掉电也不会留下半份 JSON（备份留底那条路只服务「坏文件」，不负责抢救半个文件）。
pub(super) fn save(
    path: &Path,
    chats: &[AiChat],
    current: u64,
    next_chat_id: u64,
    run_chats: &HashMap<String, u64>,
) -> anyhow::Result<()> {
    let file = ChatsFile {
        version: VERSION,
        current,
        next_chat_id,
        // BTreeMap：同一次会话存两次能得到逐字节相同的文件，改了什么一眼能看出来
        run_chats: run_chats.iter().map(|(k, v)| (k.clone(), *v)).collect(),
        chats: chats.iter().map(chat_to_rec).collect(),
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // 紧凑 JSON 而不是 pretty：这份文件是**记录**不是手改的配置，
    // 轨迹明细动辄几十 KB，缩进会让体积接近翻倍。
    let json = serde_json::to_string(&file)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// 把读回来的内容整理成能直接接管的不变量形态。
fn normalize(file: ChatsFile) -> LoadedChats {
    let chats: Vec<AiChat> = file.chats.into_iter().map(rec_to_chat).collect();
    if chats.is_empty() {
        // 文件在、但一条会话都没有（用户从没聊过，或全被删光过）：
        // 与「读不到」同一条出路，顺便把不可能再被指到的映射丢掉
        return fresh();
    }
    // 当前会话：文件里记的那条要是已经不存在（外部改过 / 被上面判空丢弃），
    // 落到「最近改动」的那条 —— 与 `ai_delete_chat` 删掉当前会话时的落点同一口径。
    let current = if chats.iter().any(|c| c.id == file.current) {
        file.current
    } else {
        chats
            .iter()
            .max_by_key(|c| c.updated_at)
            .map(|c| c.id)
            .unwrap_or(chats[0].id)
    };
    // 悬空映射剔除：一次定时任务运行留下的 `run_id → chat_id` 只有在会话还在时
    // 才有意义。留着它，用户点那次运行的「会话」会跳进一条不存在的会话 ——
    // 表现就是「每点一次就多出一条一模一样的会话」。顺带也保证这张表不会
    // 无限长大（`ai_attach_run_session` 只增不删）。
    let known: std::collections::HashSet<u64> = chats.iter().map(|c| c.id).collect();
    let run_chats = file
        .run_chats
        .into_iter()
        .filter(|(_, id)| known.contains(id))
        .collect();
    // 分配器必须大于所有已存在的 id，否则新建会话会撞上恢复回来的旧会话
    let next_chat_id = file
        .next_chat_id
        .max(chats.iter().map(|c| c.id).max().unwrap_or(0) + 1);
    LoadedChats {
        chats,
        current,
        next_chat_id,
        run_chats,
    }
}

// ---- 存储格式（与线上格式解耦，见模块文档） ----------------------------------

#[derive(Serialize, Deserialize)]
#[serde(default)]
struct ChatsFile {
    version: u32,
    current: u64,
    next_chat_id: u64,
    run_chats: BTreeMap<String, u64>,
    chats: Vec<ChatRec>,
}

impl Default for ChatsFile {
    fn default() -> Self {
        Self {
            version: VERSION,
            current: 0,
            next_chat_id: 1,
            run_chats: BTreeMap::new(),
            chats: Vec::new(),
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(default)]
struct ChatRec {
    id: u64,
    title: String,
    updated_at: u64,
    next_msg_id: u64,
    msgs: Vec<MsgRec>,
}

impl Default for ChatRec {
    fn default() -> Self {
        Self {
            id: 0,
            title: String::new(),
            updated_at: 0,
            next_msg_id: 1,
            msgs: Vec::new(),
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(default)]
struct MsgRec {
    id: u64,
    /// 角色直接复用 `core::ai::Role`：它在线上格式里就是这个含义，
    /// 没有第二套词汇的必要（serde 线格式同样是小写）。
    role: Role,
    content: String,
    state: StateRec,
    created_at: u64,
    injected_lines: usize,
    reasoning: String,
    trace: Vec<TraceRow>,
    commands: Vec<CardRec>,
}

impl Default for MsgRec {
    fn default() -> Self {
        Self {
            id: 0,
            role: Role::Assistant,
            content: String::new(),
            state: StateRec::default(),
            created_at: 0,
            injected_lines: 0,
            reasoning: String::new(),
            trace: Vec::new(),
            commands: Vec::new(),
        }
    }
}

/// 消息的**终态**。
///
/// 刻意不是 `MsgState` 的镜像：`Pending` / `Streaming` 在这里**根本无法表示** ——
/// 「还在路上」是进程内的事实，能存进文件就说明它已经是假的。类型上排除掉，
/// 就不必指望每个读文件的地方都记得归一化。
#[derive(Serialize, Deserialize, Default, Debug, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum StateRec {
    #[default]
    Done,
    Failed {
        reason: String,
    },
    Cancelled,
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
struct CardRec {
    cmd: String,
    risk: RiskRec,
    done: bool,
}

#[derive(Serialize, Deserialize, Default, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum RiskRec {
    #[default]
    Safe,
    Confirm,
    Blocked,
}

impl RiskRec {
    fn from_risk(r: Risk) -> Self {
        match r {
            Risk::Safe => Self::Safe,
            Risk::Confirm => Self::Confirm,
            Risk::Blocked => Self::Blocked,
        }
    }

    fn to_risk(self) -> Risk {
        match self {
            Self::Safe => Risk::Safe,
            Self::Confirm => Risk::Confirm,
            Self::Blocked => Risk::Blocked,
        }
    }
}

// ---- 转换 --------------------------------------------------------------------

fn chat_to_rec(chat: &AiChat) -> ChatRec {
    ChatRec {
        id: chat.id,
        title: chat.title.clone(),
        updated_at: chat.updated_at,
        next_msg_id: chat.next_msg_id,
        msgs: chat.msgs.iter().filter_map(msg_to_rec).collect(),
    }
}

/// 消息 → 记录。返回 `None` = 这条不值得存（见模块文档第 2 条）。
fn msg_to_rec(msg: &ChatMessage) -> Option<MsgRec> {
    // 会话消息里不该出现工具调用痕迹：`Tool` 结果只活在 agent 的私有
    // transcript 里（见 `ai_poll_results` 的 `ToolCalls` 分支 —— 它连结果都不落
    // 进会话）。真出现了说明有人改了通路，那时这份文件需要跟着扩字段 ——
    // 先出声，别静默丢。
    debug_assert!(
        msg.tool_calls.is_empty() && msg.tool_call_id.is_none(),
        "会话消息不该携带工具调用字段（ai_persist 的存储格式需要跟着扩）"
    );
    let interrupted = matches!(msg.state, MsgState::Pending | MsgState::Streaming);
    if interrupted
        && msg.content.is_empty()
        && msg.reasoning.is_empty()
        && msg.trace.is_empty()
        && msg.commands.is_empty()
    {
        return None;
    }
    Some(MsgRec {
        id: msg.id,
        role: msg.role,
        content: msg.content.clone(),
        state: if interrupted {
            StateRec::Cancelled
        } else {
            match &msg.state {
                MsgState::Failed(reason) => StateRec::Failed {
                    reason: reason.clone(),
                },
                MsgState::Cancelled => StateRec::Cancelled,
                _ => StateRec::Done,
            }
        },
        created_at: msg.created_at,
        injected_lines: msg.injected_lines,
        reasoning: msg.reasoning.clone(),
        trace: msg.trace.clone(),
        commands: msg
            .commands
            .iter()
            .map(|c| CardRec {
                cmd: c.cmd.clone(),
                risk: RiskRec::from_risk(c.risk),
                done: c.done,
            })
            .collect(),
    })
}

fn rec_to_chat(rec: ChatRec) -> AiChat {
    let msgs: Vec<ChatMessage> = rec.msgs.into_iter().map(rec_to_msg).collect();
    // 消息 id 分配器必须越过已恢复的最大 id：撞上会让 gpui 元素 id 复用错位
    // （面板按 id 派生元素 id），也会让「按 id 定位某条消息」指错行。
    let next_msg_id = rec
        .next_msg_id
        .max(msgs.iter().map(|m| m.id).max().unwrap_or(0) + 1);
    AiChat {
        id: rec.id,
        title: rec.title,
        msgs,
        next_msg_id,
        updated_at: rec.updated_at,
    }
}

fn rec_to_msg(rec: MsgRec) -> ChatMessage {
    // 走公开构造器拿到一条形态正确的消息，再逐项覆盖 —— `ChatMessage::new` 是
    // core 私有，而落盘要控制的正是那几个（id / state / created_at）字段。
    let mut msg = match rec.role {
        Role::User => ChatMessage::user(rec.id, rec.content, rec.injected_lines),
        Role::Assistant => ChatMessage::assistant(rec.id, rec.content),
        // 会话里不该有这两种（见 `msg_to_rec` 的 debug_assert）：真读了进来也
        // 按助手消息收下，至少正文不丢。
        Role::System | Role::Tool => ChatMessage::assistant(rec.id, rec.content),
    };
    msg.created_at = rec.created_at;
    msg.state = match rec.state {
        StateRec::Done => MsgState::Done,
        StateRec::Failed { reason } => MsgState::Failed(reason),
        StateRec::Cancelled => MsgState::Cancelled,
    };
    msg.reasoning = rec.reasoning;
    msg.trace = rec.trace;
    msg.commands = rec
        .commands
        .into_iter()
        .map(|c| AiCommandCard {
            cmd: c.cmd,
            risk: c.risk.to_risk(),
            done: c.done,
        })
        .collect();
    msg
}

// ---- 落盘时机（MainView 侧） --------------------------------------------------

impl super::MainView {
    /// 标记会话有改动，**下个 tick 就写**（`poll` 每 300ms 一轮）。
    ///
    /// 用在用户动作与收尾上：这些时刻之后用户随时可能关窗，多等一个节流周期
    /// 就可能把刚发生的事丢在内存里。
    pub(super) fn ai_chats_touch(&mut self) {
        self.ai_ui.ai_chats_dirty = true;
        self.ai_ui.ai_chats_save_at_ms = 0;
    }

    /// 流式增量用的标记：同样「有改动」，但按 [`SAVE_MIN_GAP_MS`] 节流。
    ///
    /// 节流窗口只在「干净 → 脏」那一次记录：一轮生成里第一个增量定下时刻，
    /// 之后每 3 秒落一次盘，直到这轮结束。
    pub(super) fn ai_chats_touch_streaming(&mut self) {
        if !self.ai_ui.ai_chats_dirty {
            self.ai_ui.ai_chats_dirty = true;
            self.ai_ui.ai_chats_save_at_ms = now_ms().saturating_add(SAVE_MIN_GAP_MS);
        }
    }

    /// 到点就落盘（`poll` 每轮调用）。返回是否真的写了 —— 调用方据此决定要不要
    /// 记慢帧日志，**不要**据此 `cx.notify()`：写文件不改任何被渲染的东西。
    pub(super) fn ai_chats_flush_if_due(&mut self) -> bool {
        match production_path() {
            Some(path) => self.ai_chats_flush(&path, false),
            None => false,
        }
    }

    /// 立即落盘（退出钩子调它）。没改动时是空操作 —— 退出时不该为
    /// 「什么都没发生过」重写一遍文件。
    ///
    /// ⚠️ 用例**别调它**：它写的是开发机真实的会话文件，会把真实聊天记录换掉。
    /// 测落盘请用 `ai_chats_flush(&临时路径, true)`。
    pub fn ai_chats_flush_now(&mut self) -> bool {
        match production_path() {
            Some(path) => self.ai_chats_flush(&path, true),
            None => false,
        }
    }

    /// 落盘本体。`path` 是参数而不是写死：用例要把它导到临时目录
    /// （写开发机真实的 AI 会话文件是**破坏性**的）。
    pub(super) fn ai_chats_flush(&mut self, path: &Path, force: bool) -> bool {
        if !self.ai_ui.ai_chats_dirty {
            return false;
        }
        let now = now_ms();
        if !force && now < self.ai_ui.ai_chats_save_at_ms {
            return false;
        }
        match save(
            path,
            &self.ai_ui.ai_chats,
            self.ai_ui.ai_chat_id,
            self.ai_ui.ai_next_chat_id,
            &self.ai_ui.ai_run_chats,
        ) {
            Ok(()) => {
                self.ai_ui.ai_chats_dirty = false;
                true
            }
            Err(e) => {
                // 写失败（磁盘满 / 目录只读）就保持脏，下一轮再来：
                // 静默把历史丢掉，比多试几次糟得多。
                self.ai_ui.ai_chats_save_at_ms = now.saturating_add(SAVE_MIN_GAP_MS);
                log::warn!("AI 会话落盘失败（下轮重试）: {e}");
                false
            }
        }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustshell_core::agent::TraceState;

    /// 每个用例一个独立目录（落盘用例真写文件，共用目录会互相踩）。
    fn tmp_path(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("rustshell-ai-chats-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        dir.join("ai_chats.json")
    }

    /// 一条「什么都有」的会话：正文 + 思考 + 轨迹 + 卡片 + 上下文行数 + 时间戳。
    fn sample_chat() -> AiChat {
        let mut user = ChatMessage::user(1, "磁盘满了吗？", 200);
        user.created_at = 1_700_000_000;
        let mut reply = ChatMessage::assistant(2, "看下 /var 的用量");
        reply.created_at = 1_700_000_005;
        reply.reasoning = "先看 df -h 再说".into();
        reply.commands.push(AiCommandCard {
            cmd: "df -h".into(),
            risk: Risk::Safe,
            done: true,
        });
        reply.commands.push(AiCommandCard {
            cmd: "systemctl restart nginx".into(),
            risk: Risk::Confirm,
            done: false,
        });
        reply.trace.push(TraceRow {
            step: 1,
            call_id: "call_1".into(),
            tool: "inspect_load".into(),
            summary: "prod-*".into(),
            state: TraceState::Anomaly,
            note: "cpu 92%".into(),
            elapsed_ms: 812,
            detail: "load: 9.1".into(),
        });
        AiChat {
            id: 3,
            title: "磁盘满了吗？".into(),
            msgs: vec![user, reply],
            next_msg_id: 7,
            updated_at: 1_700_000_010,
        }
    }

    /// 面板元数据一个都不能少：这份文件的**全部意义**就是恢复出与关窗前一模一样的
    /// 对话。少了轨迹 → 巡检明细没了；少了 reasoning → 思考过程没了；少了 id →
    /// 元素 id 复用错位、按 id 定位消息指错行。
    #[test]
    fn round_trip_keeps_every_panel_metadata_field() {
        let path = tmp_path("round-trip");
        let chat = sample_chat();
        let mut run = HashMap::new();
        run.insert("run-1".to_string(), chat.id);
        save(&path, std::slice::from_ref(&chat), chat.id, 9, &run).expect("落盘");

        let loaded = load(&path);
        assert_eq!(loaded.chats.len(), 1);
        assert_eq!(loaded.current, 3);
        assert_eq!(loaded.next_chat_id, 9);
        assert_eq!(loaded.run_chats.get("run-1"), Some(&3));
        assert_eq!(loaded.current_msgs_len(), 2, "行数要与消息条数一致");
        let got = &loaded.chats[0];
        assert_eq!(got.title, "磁盘满了吗？");
        assert_eq!(got.updated_at, 1_700_000_010);
        assert_eq!(got.next_msg_id, 7);

        let u = &got.msgs[0];
        assert_eq!(u.id, 1);
        assert_eq!(u.role, Role::User);
        assert_eq!(u.content, "磁盘满了吗？");
        assert_eq!(u.created_at, 1_700_000_000);
        assert_eq!(u.injected_lines, 200);
        assert_eq!(u.state, MsgState::Done);

        let a = &got.msgs[1];
        assert_eq!(a.id, 2);
        assert_eq!(a.role, Role::Assistant);
        assert_eq!(a.created_at, 1_700_000_005);
        assert_eq!(a.reasoning, "先看 df -h 再说");
        assert_eq!(a.commands.len(), 2);
        assert_eq!(a.commands[1].cmd, "systemctl restart nginx");
        assert_eq!(a.commands[1].risk, Risk::Confirm);
        assert!(!a.commands[1].done, "未确认的卡片要原样回来（还能点执行）");
        assert!(a.commands[0].done, "已执行的卡片回来也要是已完成态");
        assert_eq!(a.trace.len(), 1);
        assert_eq!(a.trace[0].state, TraceState::Anomaly);
        assert_eq!(a.trace[0].detail, "load: 9.1");
        assert_eq!(a.trace[0].elapsed_ms, 812);
    }

    /// 在途状态归零：重启后没有请求在路上，`Streaming` 存进文件就是个假象。
    /// 同时空占位（首个增量之前就死了的那种）不该变成一条空白气泡。
    #[test]
    fn in_flight_messages_are_settled_and_empty_placeholders_dropped() {
        let path = tmp_path("in-flight");
        let mut chat = AiChat::new(1, 1_700_000_000);
        // ① 空占位：正文 / 思考 / 轨迹 / 卡片全空 → 不存
        chat.msgs.push(ChatMessage::assistant_streaming(1));
        // ② 半截答复：有正文 → 存，状态归成「已停止」
        let mut half = ChatMessage::assistant(2, "先看 df");
        half.state = MsgState::Streaming;
        chat.msgs.push(half);
        // ③ Pending 但已经有思考过程 → 也算「有话要说」，存
        let mut pending = ChatMessage::assistant(3, "");
        pending.state = MsgState::Pending;
        pending.reasoning = "想了想".into();
        chat.msgs.push(pending);
        chat.next_msg_id = 4;
        save(&path, std::slice::from_ref(&chat), 1, 2, &HashMap::new()).expect("落盘");

        let loaded = load(&path);
        let msgs = &loaded.chats[0].msgs;
        assert_eq!(msgs.len(), 2, "空占位不该落盘: {msgs:?}");
        assert_eq!(msgs[0].content, "先看 df");
        assert_eq!(msgs[0].state, MsgState::Cancelled);
        assert_eq!(msgs[1].state, MsgState::Cancelled);
        assert_eq!(msgs[1].reasoning, "想了想");
    }

    /// 失败原因要活着回来：面板靠它渲染「重试」与原因文案。
    #[test]
    fn failure_reason_survives_the_round_trip() {
        let path = tmp_path("failed");
        let mut chat = AiChat::new(1, 1_700_000_000);
        let mut m = ChatMessage::assistant(1, "写到一半");
        m.state = MsgState::Failed("HTTP 429：请求过于频繁".into());
        chat.msgs.push(m);
        chat.next_msg_id = 2;
        save(&path, std::slice::from_ref(&chat), 1, 2, &HashMap::new()).expect("落盘");

        let loaded = load(&path);
        assert_eq!(
            loaded.chats[0].msgs[0].state,
            MsgState::Failed("HTTP 429：请求过于频繁".into())
        );
    }

    /// 没有文件（第一次用 / 用户删过）：一条空白会话，正常开局。
    #[test]
    fn missing_file_yields_exactly_one_empty_chat() {
        let path = tmp_path("missing");
        let _ = std::fs::remove_file(&path);
        let loaded = load(&path);
        assert_eq!(loaded.chats.len(), 1);
        assert!(loaded.chats[0].is_empty());
        assert_eq!(loaded.current, loaded.chats[0].id);
        assert!(loaded.next_chat_id > loaded.chats[0].id);
        assert_eq!(loaded.current_msgs_len(), 0);
    }

    /// 坏文件：留底 + 从头开始，绝不 panic（`ai_cur()` 的落点必须存在）。
    #[test]
    fn corrupt_file_is_backed_up_and_starts_fresh() {
        let path = tmp_path("corrupt");
        std::fs::write(&path, "{ 这不是 JSON").expect("写坏文件");
        let loaded = load(&path);
        assert_eq!(loaded.chats.len(), 1);
        assert!(loaded.chats[0].is_empty());
        let names: Vec<String> = std::fs::read_dir(path.parent().unwrap())
            .expect("读目录")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert!(
            names.iter().any(|n| n.contains("broken")),
            "坏文件没留底，用户无从查「会话为什么全没了」: {names:?}"
        );
    }

    /// 文件在、但一条会话都没有：与「读不到」同一条出路。
    #[test]
    fn a_file_with_no_chats_falls_back_to_one_empty_chat() {
        let path = tmp_path("no-chats");
        std::fs::write(
            &path,
            r#"{"version":1,"current":7,"next_chat_id":9,"chats":[]}"#,
        )
        .expect("写文件");
        let loaded = load(&path);
        assert_eq!(loaded.chats.len(), 1, "不变量：ai_chats 恒非空");
        assert_eq!(loaded.current, loaded.chats[0].id);
        assert!(loaded.run_chats.is_empty(), "没有会话，映射只能全丢");
    }

    /// 外部改过 / 老版本留下的文件里，current 可能指向不存在的会话；
    /// 两个分配器也可能落后于已有的 id —— 都得修好再接管，否则新会话/新消息
    /// 会撞上恢复回来的旧 id。
    #[test]
    fn counters_and_current_are_repaired_on_load() {
        let path = tmp_path("repair");
        let older = AiChat::new(5, 1_000);
        let mut newer = AiChat::new(6, 2_000);
        newer.msgs.push(ChatMessage::assistant(4, "x"));
        newer.next_msg_id = 1; // 落后于最大消息 id(4)
        let mut run = HashMap::new();
        run.insert("run-alive".to_string(), 6u64);
        run.insert("run-dangling".to_string(), 99u64); // 指向不存在的会话

        // 故意传坏：current 指向 99（不存在）、next_chat_id 只到 2（比 5/6 都小）
        save(&path, &[older, newer], 99, 2, &run).expect("落盘");

        let loaded = load(&path);
        assert_eq!(loaded.current, 6, "current 失效时落到最近改动的那条");
        assert_eq!(loaded.next_chat_id, 7, "会话分配器要越过最大 id");
        assert_eq!(
            loaded.chats[1].next_msg_id, 5,
            "消息分配器要越过最大消息 id"
        );
        assert_eq!(loaded.run_chats.len(), 1);
        assert_eq!(loaded.run_chats.get("run-alive"), Some(&6));
    }

    /// 版本号比本程序新：未知字段被 serde 忽略，已知字段照读。
    /// 直接拒绝解析等于「升级一次软件，旧版本的会话全没了」。
    #[test]
    fn a_newer_file_version_still_loads_the_known_fields() {
        let path = tmp_path("newer");
        std::fs::write(
            &path,
            r#"{"version":99,"current":1,"next_chat_id":2,"future_field":{"a":1},
                "chats":[{"id":1,"title":"未来","updated_at":5,"next_msg_id":2,
                "msgs":[{"id":1,"role":"user","content":"还在吗","state":{"kind":"done"}}]}]}"#,
        )
        .expect("写文件");
        let loaded = load(&path);
        assert_eq!(loaded.chats.len(), 1);
        assert_eq!(loaded.chats[0].title, "未来");
        assert_eq!(loaded.chats[0].msgs[0].content, "还在吗");
        assert_eq!(loaded.chats[0].msgs[0].role, Role::User);
    }

    /// 真相：会话是不是真从 `MainView` 里写出去、又读得回来。
    /// 上一条覆盖「格式对不对」，这条覆盖「接线通不通」。
    #[gpui::test]
    fn the_live_view_writes_its_chats_and_reads_them_back(cx: &mut gpui::TestAppContext) {
        let (main, vcx) = crate::main_view::dialogs::tests::build_main(cx);
        let path = tmp_path("live-view");
        main.update_in(vcx, |m, _w, _cx| {
            let id = m.next_ai_msg_id();
            m.ai_msgs_mut()
                .push(ChatMessage::user(id, "重启后还在吗", 0));
            // 与 `ai_send_wrapped` 同款：首条提问会顺手把标题从「新对话」改成提问
            m.ai_cur_mut().title = "重启后还在吗".into();
            assert!(m.ai_ui.ai_chats_dirty, "改过消息却没打落盘标记");
            // 注意：这里**必须**传临时路径。`ai_chats_flush_now()` 写的是开发机
            // 真实的 `ai_chats.json`，在用例里调它会抹掉真实会话记录。
            assert!(m.ai_chats_flush(&path, true), "有改动却没写");
            assert!(!m.ai_chats_flush(&path, true), "没改动还重写了一次文件");
        });

        let loaded = load(&path);
        assert_eq!(loaded.chats.len(), 1);
        assert_eq!(loaded.chats[0].title, "重启后还在吗");
        assert_eq!(loaded.chats[0].msgs.len(), 1);
        assert_eq!(loaded.chats[0].msgs[0].role, Role::User);
        assert_eq!(loaded.chats[0].msgs[0].content, "重启后还在吗");
        assert_eq!(loaded.current_msgs_len(), 1);
    }

    /// 用户动作立刻写，流式增量按节流等 —— 这条节流是「一轮生成别把整份 JSON
    /// 写几十遍」的唯一保证，写反了就是磁盘与 UI 线程一起受罪。
    #[gpui::test]
    fn streaming_changes_are_throttled_while_user_actions_write_right_away(
        cx: &mut gpui::TestAppContext,
    ) {
        let (main, vcx) = crate::main_view::dialogs::tests::build_main(cx);
        let path = tmp_path("throttle");
        let _ = std::fs::remove_file(&path);
        main.update_in(vcx, |m, _w, _cx| {
            let id = m.next_ai_msg_id();
            m.ai_msgs_mut().push(ChatMessage::assistant(id, "增量"));
            assert!(m.ai_ui.ai_chats_dirty);
            assert!(
                !m.ai_chats_flush(&path, false),
                "流式标记（`ai_cur_mut` 那条路）不该立刻落盘"
            );
            assert!(!path.exists(), "节流窗口内不该有文件");
            m.ai_chats_touch(); // 用户动作 / 收尾
            assert!(
                m.ai_chats_flush(&path, false),
                "用户动作必须下个 tick 就写，不能等节流"
            );
        });
    }
}
