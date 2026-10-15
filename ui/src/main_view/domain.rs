//! 跨功能域的**共享数据模型**：AI 会话与运行态、SFTP 传输队列条目、对话框模式。
//!
//! 2026-10-12（开源评审 P0-4 刀二）从 `main_view.rs` 本体搬出。
//!
//! 为什么它们该有自己的家：这几个类型的共同点是**被 MainView 的多个功能域
//! 共同读写**，谁都不该独占它们 ——
//!   * `AiChat` / `AgentRun`：AI 面板渲染，由 `ai_actions` 投递、`ai_persist` 落盘；
//!   * `TransferState`：文件面板渲染，由 `terminal_actions` 从 SFTP 事件流推进；
//!   * `DialogMode`：`dialogs` 的每个对话框分支都匹配它。
//!
//! 留在 `main_view.rs` 本体会诱使后来的代码把它们当成「MainView 自己的东西」，
//! 而实际上它们的生命周期彼此无关（AI 会话与 SFTP 传输互不相干）。
//!
//! `chat_title_from` 是纯函数（剥掉 `[当前终端上下文] … [用户]` 包装取首行），
//! 同样搬来 —— 它容易写错，而写错的后果是列表里每条会话都叫
//! 「[当前终端上下文]」。

/// 一条 AI 会话（左侧栏「聊天记录」列表里的一行）。
///
/// 2026-09-18 运行反馈「点击 AI 助手，聊天界面显示在终端右侧，原 AI 助手侧边栏
/// 改为聊天记录，支持删除」：对话从「全局只有一条」变成「可以有多条、能切换、
/// 能删除」。消息 id 分配器随会话走（每条会话各自从 1 起编号）——同一时刻只有
/// 当前会话渲染，而 `MessageScroller` 的状态由 `MainView` 持有并在切换时
/// `reset(len)`，所以 id 不必再拼会话前缀。
#[derive(Debug, Clone)]
pub struct AiChat {
    pub id: u64,
    /// 列表里显示的名字：取第一条提问截断；还没提问过时是「新对话」。
    pub title: String,
    pub msgs: Vec<rustshell_core::ai::ChatMessage>,
    /// 本会话的消息 id 分配器
    pub next_msg_id: u64,
    /// 最近一次改动时间（秒）——列表按它倒序，最近聊过的排最上面。
    pub updated_at: u64,
}

impl AiChat {
    pub(super) fn new(id: u64, now: u64) -> Self {
        Self {
            id,
            title: rustshell_core::i18n::t("新对话").to_string(),
            msgs: Vec::new(),
            next_msg_id: 1,
            updated_at: now,
        }
    }

    /// 还没聊过任何内容。用于「新建对话」不重复铺空会话、以及列表行的弱化显示。
    pub fn is_empty(&self) -> bool {
        self.msgs.is_empty()
    }
}

/// 在途的交互式 agent 运行（AI 面板的「多步工具巡检」）。
///
/// 为什么状态要单独一个结构而不是散在 `MainView` 上：agent 的一轮运行有
/// **明确的起止**（起：发问；止：`Done` 事件或用户点停止），散开写就要在每个
/// 分支里各自记得清理「等待确认的卡片」这类一次性状态，漏一个的表现是
/// 「上一轮的确认卡片挂在新一轮下面，点它执行的是旧命令」。
pub struct AgentRun {
    /// 后台线程 → UI 的事件通道（`Delta` / `Trace` / `Confirm` / `Done`）
    pub rx: flume::Receiver<rustshell_core::runner::AgentUiEvent>,
    /// UI → 后台线程的确认回话（`(call_id, decision)`）
    pub decisions: flume::Sender<(String, rustshell_core::runner::AgentDecision)>,
    /// 面板存活标记：置 false 后后台线程的确认等待立即按「忽略」返回，
    /// 不会因为用户关了面板而把一个线程挂在超时上。
    pub alive: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// 当前等待用户确认的请求（确认卡片的数据源；`None` = 没在等）
    pub pending: Option<rustshell_core::runner::ConfirmRequest>,
    /// 轨迹挂在哪条 assistant 消息上
    pub msg_id: u64,
    /// 是否已经创建了流式占位消息（`Trace` 可能先于第一个 `Delta` 到达）
    pub opened: bool,
}

impl AgentRun {
    /// 结束这次运行：让后台线程别再等确认，并丢弃未决卡片。
    pub(super) fn shutdown(&mut self) {
        self.alive
            .store(false, std::sync::atomic::Ordering::Relaxed);
        self.pending = None;
    }
}

/// 会话标题：取提问正文（剥掉 `[当前终端上下文] … [用户]` 包装）首个非空行，
///
/// 单独抽成纯函数（放在文件级而不是 `impl MainView`）是为了能直接单测：
/// 标题剥包装这件事容易写错，而写错的后果是列表里每条都叫「[当前终端上下文]」。
pub fn chat_title_from(wrapped: &str) -> String {
    let body = wrapped
        .split_once("\n[用户]\n")
        .map(|(_, q)| q)
        .unwrap_or(wrapped);
    let line = body
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    if line.is_empty() {
        return rustshell_core::i18n::t("新对话").to_string();
    }
    let mut out: String = line.chars().take(CHAT_TITLE_MAX_CHARS).collect();
    if line.chars().count() > CHAT_TITLE_MAX_CHARS {
        out.push('…');
    }
    out
}

/// 会话标题的字数上限（超出截断加省略号）。列表行宽约 240px，24 个汉字已满一行。
const CHAT_TITLE_MAX_CHARS: usize = 24;

/// SFTP 传输队列条目（文件面板）
#[derive(Debug, Clone)]
pub struct TransferState {
    pub id: u64,
    pub name: String,
    pub bytes: u64,
    pub total: u64,
    pub done: bool,
    pub ok: bool,
    /// 传输条上的一行备注（F-13）：续传生效 / 守卫不通过从头传 / 跳过已传完的文件。
    ///
    /// 独立成字段而不是覆盖 `name`：名字是用户认这条传输的凭据（文件名/远端路径），
    /// 备注只是补充说明，两者要同时看得见。失败信息仍走 `name`（那是**替换**语义：
    /// 失败原因比文件名更重要）。
    pub note: Option<String>,
}

#[derive(Clone)]
pub(crate) enum DialogMode {
    /// 新建/编辑连接（Edit 内为 profile id）
    New,
    Edit(String),
    /// 在指定文件夹下新建快捷命令
    QcNew(String),
    /// 编辑快捷命令（folder_id, cmd_id）
    QcEdit(String, String),
    /// 分组名对话框：`None` = 新建（连接树右键菜单的入口），`Some(id)` = 把该分组改名。
    /// 两种用途只差一个名字，共用一个对话框。
    NewGroup(Option<String>),
    /// 重命名连接（连接树右键菜单 / F2）：内为 profile id。
    /// 与分组改名共用[`GroupNameDialogView`]，只换标题、字段名与落库目标。
    RenameConn(String),
    /// 设置面板
    Settings,
    /// SFTP 删除确认（：破坏性操作二次确认）
    SftpRemove {
        path: String,
        is_dir: bool,
        refresh: String,
    },
    /// SFTP 新建目录：`base` = 建在哪一层（名字由对话框收）。
    ///
    /// 与 [`DialogMode::SftpRemove`] 不同，父目录名**存在枚举里**而不是靠调用方
    /// 闭包捕获：落库（发 `SftpCmd::Mkdir`）发生在 `sftp_mkdir_save`，那时离点击
    /// 已经隔了一次事件循环，只有枚举里的值才是可读回的事实来源。
    SftpMkdir {
        base: String,
    },
    /// SFTP 重命名：`from` 原路径、`dir` 所在目录（新路径 = `dir` + 新名字）。
    SftpRename {
        from: String,
        dir: String,
    },
    /// 定时任务编辑：内为 task id。表单状态在 `MainView::task_draft` 与
    /// 一组 `dlg_task_*` 输入框里（与快捷命令对话框同一手法）。
    TaskEdit(String),
    /// 「恢复上次会话？」（F-9）。
    ///
    /// **2026-09-29 起只由用户手动触发**（命令面板「恢复上次会话」→
    /// `MainView::layout_restore_now`），启动不再自动弹这个框。
    ///
    /// 无参数：要展示的两个数（标签数 / 连接数）都在 `MainView::layout_state` 里，
    /// 快照本身也在那儿 —— 存进枚举只会多一份副本，而副本会与真值漂移
    /// （用户点「不恢复」之后那份副本还停在「有 5 个标签」）。
    RestoreSession,
    /// 粘贴确认（F-5）：内为**哪条会话**的终端在等确认。
    ///
    /// 挂起的请求（正文 / 行数 / `and_enter`）不在这里，它挂在终端实体上
    /// （`TerminalView::pending_paste`）—— 那是唯一真值：会话在弹框期间断连时，
    /// 引擎会把挂起清掉，宿主据此收框；枚举里再存一份副本就会与它漂移。
    PasteConfirm {
        session: rustshell_core::session::SessionId,
    },
}
