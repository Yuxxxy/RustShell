//! `TabsState`：标签栏与分屏窗格的全部跨帧状态。
//!
//! 为什么它该独立成一个结构：这一组字段描述的是「**有哪些标签、当前看哪个、
//! 分屏时谁在左边**」——它们的生命周期完全一致（都是随标签增减而变），
//! 且读写它们的代码也集中在 `tab_bar.rs` / `split_actions.rs` / `layout_persist.rs`。
//! 摊平成 `MainView` 的 175 个平级字段时，读者无法从字段名看出「哪些总是一起动」。
use std::collections::HashSet;
use std::path::PathBuf;

use gpui::{Entity, FocusHandle};
use gpui_component::{
    command::CommandState,
    input::{EditorState, InputState, TextareaState},
    list::ListState,
    slider::SliderState,
};

use super::app_shell::ViewEpoch;
use super::domain::{AiChat, TransferState};
use super::layout_persist;
use super::tab_model::{SplitPane, TabEntry};
use super::whitelist_dialog;
use super::{
    AgentRun, AiChatPanelView, GroupNameDialogView, PanelKind, SidebarPanelView, TermSearchBarView,
};
// `completion_bar` / `whitelist_dialog` / `settings_panel` 这些子模块在
// `main_view.rs` 里是**裸名可见**（同模块），搬进 ui_state 后必须写全路径。
use super::completion_bar;
use super::settings_panel;
use rustshell_core::connections::{ConnectionProfile, ForwardRule};
use rustshell_core::launch::LaunchKind;
use rustshell_core::session::SessionId;
use rustshell_core::split::SplitAxis;

pub struct TabsState {
    /// 会话布局落盘与恢复的全部跨帧状态（F-9，见 `layout_persist`）。
    ///
    /// 一个字段装下「指纹 / 脏标记 / 待恢复流程 / 提示框状态」：它们只被那一个
    /// 模块的少数几个方法读写，摊平成四个 `MainView` 字段只会让「恢复期间不许
    /// 写快照」这条约束散落在四处。
    pub(crate) layout_state: layout_persist::LayoutState,

    pub tabs: Vec<TabEntry>,

    pub active: Option<SessionId>,

    /// 标签栏横向滚动（标签多到放不下时用滚轮 / 触摸板移动，并让激活标签保持可见）。
    /// 见 [`crate::tab_bar::TabScroll`]：句柄必须跨帧存活，否则滚动位置每帧归零。
    pub tab_scroll: crate::tab_bar::TabScroll,

    /// 标签栏「按住空白处拖动平移」的上一次指针横坐标（窗口坐标 px；`None` = 尚未开始）。
    ///
    /// 平移按**逐次位移**累加（见 `MainView::drag_tab_bar_pan`），所以基准点必须跨帧
    /// 存活：拖拽开始时由 `on_drag` 的构造函数清零，之后每帧读上一帧的值算 `dx`。
    /// 逐帧新建的 `Rc<Cell>` 做不到这件事 —— 拖拽期间的每一帧都会重建闭包。
    pub tab_pan_x: Option<f32>,

    /// 每个标签的连接状态圆点（**独立子视图**），按会话 id 缓存。
    ///
    /// 圆点挂的是无限循环的呼吸动画。gpui 的动画调度器每 1/max_fps 秒做
    /// `cx.notify(window.current_view())`，而 `current_view()` 是「动画元素所属的视图」——
    /// 动画元素若直接建在 `MainView` 里，**整个窗口树就会永远以 20fps 重建**
    /// （标题栏 / 侧栏 / 标签栏 / 终端 / 快捷命令栏 / 状态栏，以及约 20 个监听器）。
    /// 摘成子视图后，`notify` 只重排这 6px 的圆点。
    ///
    /// 缓存按会话 id 键控并在标签关闭时清理（见 `prune_tab_dots`），
    /// 否则关掉又开同一会话会不断累积实体。
    pub tab_dots: std::collections::HashMap<SessionId, gpui::Entity<crate::tab_bar::TabStatusDot>>,

    /// 分屏态（None = 单窗格）。副窗格是另一个会话，不可能与主窗格共享同一 PTY。
    pub split: Option<SplitPane>,

    /// 拆分时若需要新建会话，先记下方向与原标签：新会话插入后由
    /// `maybe_establish_split` 消费。本地 Shell 是异步创建的，只有这条路径能知道新 id。
    pub pending_split: Option<(SplitAxis, SessionId)>,

    /// 下次 render 时把键盘焦点交给激活终端（无 window 的上下文先置标记）
    pub pending_focus: bool,
}

// `ConnUiState`：连接树与连接建立过程的状态。

// 左边是一棵可展开的分组树，右边是「哪些连接正在连 / 连失败了为什么」——
// 两者都是**连接管理**这一个功能域的投影，所以归在一起。

/// 一条外部链接（`ssh://` / `sftp://`）的临时连接。
///
/// **只活在内存里**：`ConnUiState::external` 是它唯一的家，标签一关就整条删除，
/// 口令也随之消失（见 [`rustshell_core::launch::EXTERNAL_PROFILE_PREFIX`]）。
/// 它不进 `ConnectionStore`，也就不会被连接树、最近访问或会话快照看见。
pub struct ExternalConn {
    /// 连接参数（口令在 `profile.password` 里，明文，仅内存）
    pub profile: ConnectionProfile,
    /// 这条链接要开什么：`Ssh` = 单通道 shell；`Sftp` = 单通道 sftp 子系统
    pub kind: LaunchKind,
    /// `sftp://` 的初始路径（`None` = 远端家目录）
    pub path: Option<String>,
}

pub struct ConnUiState {
    pub expanded_groups: HashSet<String>,

    /// 外部链接打开的临时连接，键 = `ext:` 开头的 profile id。
    pub external: std::collections::HashMap<String, ExternalConn>,

    /// 「这条连接只要 SFTP 通道」的意图（2026-10-26 右键「打开 SFTP」）。
    ///
    /// 与 `external` 的 `kind == Sftp` 是同一件事的两种来路：那边是**临时** profile
    /// （`sftp://` 链接），这边是**已保存**的连接 —— 不另造 profile，会话仍归原条目。
    ///
    /// 生命周期刻意做成「持久到用户正常连接它为止」：
    /// - 置位：`open_sftp`；
    /// - 清除：任何普通「连接」入口（`connect_profile`）—— 用户点正常连接就是要终端；
    /// - 重连不必看它：`reconnect_tab` 另有 `is_sftp_live()` 可从 tab 自身判定。
    pub sftp_only_profiles: HashSet<String>,

    /// 连接树官方状态实体（gpui-component Tree：展开/选中/键盘导航由组件持有）
    pub tree_state: gpui::Entity<gpui_component::tree::TreeState>,

    /// 上次推送给 `tree_state` 的连接树输入指纹（见 `connections_panel::conn_tree_key`）。
    ///
    /// 指纹没变就不重建树、不调 `TreeState::set_items` —— 那一步会清空树的选中态，
    /// 且每帧重建整棵 `Vec<TreeItem>` 纯属浪费。`None` = 尚未推送过。
    pub conn_tree_key: Option<String>,

    pub selected_profile: Option<String>,

    pub connecting_profiles: HashSet<String>,

    /// 连接行双击检测（平台 click_count 兜底：450ms 内同 id 二连击）
    pub last_click_profile: Option<(String, std::time::Instant)>,

    pub connect_error: Option<String>,

    /// 本地 Shell 启动中（后台 ConPTY spawn，期间 UI 保持响应并显示提示条）
    pub local_starting: bool,

    /// 非错误的过程提示（accent 色提示条，如「正在启动本地 Shell...」）
    pub connect_notice: Option<String>,

    /// OpenSSH 配置导入（F-1）：预览条目（含状态）与勾选位。
    ///
    /// `openssh_selected` 与 `openssh_plans` **同长同序**（按下标对应），
    /// 所以用两个平行容器，而不是把勾选态塞进 core 的 `EntryPlan` ——
    /// 勾选是界面态，不该污染领域模型。
    pub openssh_plans: Vec<rustshell_core::openssh::EntryPlan>,

    pub openssh_selected: Vec<bool>,

    /// 解析期警告（通配块、Include 未展开、非法端口……），预览里原样呈现。
    pub openssh_warnings: Vec<String>,

    /// 源文件路径（预览标题用；空 = 尚未解析过）。
    pub openssh_source: String,

    /// 「手动选 config 文件」的两段式回填：异步选文件回调拿不到 `Window`
    /// （弹预览对话框要 Window），故回调只置旗，`render` 里兑现。
    /// 载荷 = (config 路径, ssh_dir, home)。
    pub openssh_pick_sync: Option<(std::path::PathBuf, std::path::PathBuf, std::path::PathBuf)>,

    /// OpenSSH 预览层是否开着（F-1）。
    ///
    /// **不能记在 `DialogMode` 里**：那是个单槽标记，而预览要能叠在设置面板
    /// **之上**（官方对话框层是栈，与「免确认白名单」同一手法），叠层时写成
    /// `Some(ImportOpenSsh)` 会把下面的 `Some(Settings)` 冲掉 —— 关掉预览后
    /// 设置面板的渲染守卫与「取消即回滚」全部失准。故预览层自持一个布尔值，
    /// 由 `on_close`（覆盖 ESC / 遮罩 / 右上角 X / 按钮）复位。
    pub openssh_dialog_open: bool,

    /// 连接树搜索框（按 名称 / 主机 / 用户名 过滤）。
    /// 可见性决策在 core 的 `ConnectionStore::visible_tree`，此处只持有输入状态。
    /// 平级模块 `connections_panel` 也要渲染它，故 pub。
    pub conn_search: Entity<InputState>,

    /// 待决策的首次信任请求（M3 known_hosts 校验）
    pub pending_trust: Option<rustshell_core::ssh::ServerKeyPrompt>,

    /// 信任对话框是否已提交 Root 打开（防止逐帧重复 open_dialog）
    pub trust_dialog_open: bool,
}

// `PanelUiState`：窗口框架与各面板外壳的状态。

// 包括侧栏当前显示哪个面板、侧栏是否折叠、焦点句柄、脏传播版本号，
// 以及浮层类状态（终端搜索 / 命令补全 / 命令面板 / 键位录制 / 托盘旗）。

pub struct PanelUiState {
    pub focus: FocusHandle,

    pub panel: PanelKind,

    /// 侧栏面板的独立视图（见 `SidebarPanelView`：把面板从整棵树里摘出来，
    /// 避免输入/滚动把全窗口重排一遍）
    pub sidebar_panel: gpui::Entity<SidebarPanelView>,

    /// 三个独立子视图各自的「我依赖的数据变了」版本号（P0-3，见 [`ViewEpoch`]）。
    ///
    /// 只被 [`MainView::notify_view`] / `poll` 写、被三个子视图的 `observe` 回调读。
    pub(crate) view_epoch: ViewEpoch,

    pub sidebar_collapsed: bool,

    /// 本机指标采样间隔（ms，设置面板动态调频）
    pub metrics_interval: std::sync::Arc<std::sync::atomic::AtomicU64>,

    /// 命令面板状态实体（None = 关闭）
    pub palette_state: Option<Entity<CommandState>>,

    /// 启动后第一帧自动打开命令窗口（2026-09-17 运行反馈：启动不再开本地 shell）。
    ///
    /// 启动路径（`shell/src/app.rs`）拿不到 `Window`，而开浮层要 `window.focus` ——
    /// 于是那里只置这个标记，真正的打开动作落在 `render` 里（与 `pending_focus` 同一手法）。
    pub pending_palette: bool,

    /// 有待激活的窗口（外部链接转交给已在运行的实例时置位）。
    ///
    /// `poll` 拿不到 `Window`，而 `activate_window` 是 `Window` 的方法 ——
    /// 同样走「置旗 → 下一帧 render 兑现」这两段式。
    pub pending_activate_window: bool,

    /// 面板打开时的命令快照（内置命令 + 快速连接，两段拼接）。
    ///
    /// 渲染时按 `origin` 切成两个 group，confirm 按「分区 + 区内序号」回查
    /// （见`render_command_palette`）。
    pub palette_cmds: Vec<rustshell_core::command::CommandEntry>,

    /// 键位表格快照（M3 设置面板：录制重绑定 + 导入导出）
    pub keymap_rows: Vec<crate::keybinds::KeymapRow>,

    /// 正在录制新快捷键的 action id（None = 非录制态）
    pub keymap_recording: Option<String>,

    /// 录制期按键拦截器订阅（drop 即解除拦截）
    pub keymap_capture: Option<gpui::Subscription>,

    /// 键位操作提示/错误（冲突、保存失败、导入导出结果）
    pub keymap_hint: Option<String>,

    // 终端搜索（Ctrl+F）
    /// 搜索浮层是否打开
    pub term_search_open: bool,

    /// 上次已同步到终端的查询串（poll 去重）
    pub term_search_last: String,

    /// 搜索输入框
    pub term_search_input: Entity<InputState>,

    /// 终端搜索浮层的独立视图（见 `TermSearchBarView`）
    pub term_search_bar: gpui::Entity<TermSearchBarView>,

    /// 搜索串防抖任务：打字期间不逐键扫屏，停手 120ms 后才应用到终端
    pub term_search_debounce: Option<gpui::Task<()>>,

    // 命令补全（C-1）
    /// 补全浮层的**全部**状态（开合 / 候选 / 选中 / 前缀）。
    ///
    /// 用它自己的小结构体而不是在主结构里铺四个字段：这几个字段有共同的不变量
    /// （关掉就得一起清，见 `CompletionState::close`），凑在一起才不会漏清一两个。
    /// 它修改自 `TerminalView` 的上下文（引擎回调），所以必须是 `MainView` 上的字段
    /// —— 见 `completion_bar` 模块文档。
    pub(super) completion: completion_bar::CompletionState,

    /// 补全浮层的独立视图（见 `CompletionBarView`）
    pub completion_bar: gpui::Entity<completion_bar::CompletionBarView>,

    /// 本会话用过的命令历史（补全的第 ② 号数据源）。
    ///
    /// 这是**当前会话的那一份** —— 其它会话的历史在 `cmd_history_buckets` 里，
    /// 切标签时由 `switch_history_bucket` 搬运（见 `completion_persist` 模块文档：
    /// 为什么保留"当前那一份"这个单一字段，而不是让十几处调用各自算键）。
    ///
    /// 键取**连接指纹**（`host:port:username`）而非标签 id：标签 id 是进程内的，
    /// 重启后换主人（历史张冠李戴，比不存更糟）。
    pub completion_history: rustshell_core::cmdhistory::CommandHistory,

    /// 非当前会话的历史桶（切标签时换出到这儿）。落盘时与当前桶一起写。
    pub cmd_history_buckets:
        std::collections::BTreeMap<String, rustshell_core::cmdhistory::CommandHistory>,

    /// 历史有改动、还没落到 `cmd_history.json`（遗留项 1：历史跨重启保留）。
    pub cmd_history_dirty: bool,

    /// 最早允许落盘的时刻（unix 毫秒）。见 `completion_persist::cmd_history_touch`。
    pub cmd_history_save_at_ms: u64,

    /// 终端行高滑块（2026-09-28 新增设置项）。
    ///
    /// 与 `ai_temp_slider` 同一条理由：必须是**持久**字段，不能在设置面板里
    /// 逐帧新建 —— 拖动横跨很多帧，每帧重建状态会把手指按着的滑块拽回原位。
    /// 取值经 `cx.subscribe` 写进 `settings.terminal_line_height`
    /// （面板里那个数值标签每帧现读）。
    pub line_height_slider: Entity<SliderState>,
}

// `AiUiState`：AI 面板与快捷指令栏的状态。

// 两者放一起是因为它们共用同一套交互模式：**一串实体 + 一个 toast 槽 +
// 一个「正在忙」的取消句柄 + 一个序号分配器**。分开会让这套模式出现两遍。
// 维护时改一处忘另一处，是这类平行结构最常见的漂移来源。

pub struct AiUiState {
    pub quick_path: PathBuf,

    /// 快捷命令栏：本次会话的显式投递目标。
    pub quick_targets: Vec<SessionId>,

    /// 快捷命令栏展开态下的搜索框
    pub quick_search: Entity<InputState>,

    /// 快捷命令域瞬时提示（执行反馈 / 空态提示），2s 后由 [`MainView::clear_quick_toast`] 清空
    pub quick_toast: Option<String>,

    /// 瞬时提示的语义级别：决定它渲染成官方`Alert`的哪个变体。
    /// 「没有已连接的会话」是警告，「未执行 · 投递失败」是错误 到 此前
    /// 一律画黄色三角，故障被降级成了提醒。
    pub quick_toast_kind: crate::notice::NoticeKind,

    /// 「没有任何可用会话」提示是否正显示（2026-09-17 运行反馈 + 2026-09-18 改版）。
    ///
    /// 位置经历三版：① 命令栏里那枚 2s 浮层（位置贴着命令栏、寿命只有 2 秒）；
    /// ② 覆盖终端区正中的 `Alert` 卡片 + 「新建本地终端 / 知道了」按钮行
    /// （要用户点一下才收走）；③ **当前形态**：仍在终端区正中，但**没有按钮、
    /// 没有蒙版**，`NO_SESSION_NOTICE_MS` 后自己消失 —— 运行反馈要的是
    /// 「告诉我一声」，不是「停下来先点确认」。
    ///
    /// 清除时机：发出去一条命令、建好终端、到点自动消失（见 `show_no_session_alert`）。
    pub quick_no_session_alert: bool,

    /// 正在行内重命名的分组 id（；None = 非重命名态）
    pub quick_renaming: Option<String>,

    /// 行内重命名输入框（与`quick_renaming`配对使用）
    pub quick_rename_input: Entity<InputState>,

    /// 快捷命令栏当前选中的分组（Tab 页形态，2026-09-14 改）。
    pub quick_active_folder: Option<String>,

    /// 命令输入框的草稿（常驻输入框，独立于任何已保存命令）
    pub quick_compose_input: Entity<InputState>,

    /// 待确认删除的分组 id（QB-GRP-03）。
    /// 删除确认走官方 `AlertDialog` 浮动弹层，故本字段**不再驱动任何渲染**，
    /// 只用于记录「哪一个分组正在等确认」（关闭路径兜底清空）。
    pub quick_pending_remove: Option<String>,

    /// 最近一次执行的命令 id：目标记忆要写回它
    pub quick_last_cmd: Option<String>,

    /// 瞬时提示代次：连点只保留最后一次（旧定时器发现代次已变即不再清空）
    pub quick_toast_seq: u64,

    // AI（.5：L1 问答 + L2 命令生成）
    /// AI 配置（端点/模型/密文密钥），持久化 ai_config.json
    pub ai_cfg: rustshell_core::ai::AiConfig,

    /// AI 会话列表（左侧栏「聊天记录」的数据源）。**不变量：恒非空**。
    ///
    /// 不变量由 `ai_delete_chat` 维持：删掉最后一条会立刻补一条空白会话 ——
    /// 「一条会话都没有」会让 `ai_cur()` 失去落点，而所有 AI 动作都从它出发。
    pub ai_chats: Vec<AiChat>,

    /// 当前会话 id（不变量：恒指向 `ai_chats` 里存在的一条）
    pub ai_chat_id: u64,

    /// 会话 id 分配器（单调自增；`0` 保留给「无 id」的哨兵）
    pub ai_next_chat_id: u64,

    /// 会话有改动、还没落到 `ai_chats.json`（2026-09-23「AI对话需要保存」）。
    ///
    /// 不落盘的内容不算「用户的东西」—— 关一次软件就没了。但也不能改一个字就写一次
    /// 整份 JSON（一轮生成里每个增量都会改正文），故标记与落盘分开：
    /// 标记随手打（`ai_persist::MainView` 侧那几个 `ai_chats_touch*`），
    /// 落盘统一在 `poll` 里按节流写。
    pub ai_chats_dirty: bool,

    /// 最早允许落盘的时刻（unix 毫秒；`0` = 立刻可写）。
    ///
    /// 只有流式增量会把它推后（见 `ai_chats_touch_streaming`）：用户动作与收尾
    /// 都设成 `0`，下个 tick 就写。
    pub ai_chats_save_at_ms: u64,

    /// 对话面板（终端右侧那一列）是否展开。与 `settings.ai_chat_open` 同步落盘。
    pub ai_chat_open: bool,

    /// 对话面板的独立视图（见 `AiChatPanelView`：与 `SidebarPanelView` 同一动机，
    /// 输入条每敲一个字只重排这一块，不牵动整棵窗口树）
    pub ai_chat_panel: gpui::Entity<AiChatPanelView>,

    /// 官方`MessageScroller`的滚动状态（row 数 / 尾部跟随 / 重测）。
    pub ai_scroller: Entity<gpui_component::message_scroller::MessageScrollerState>,

    pub ai_busy: bool,

    pub ai_error: Option<String>,

    pub ai_hint: Option<String>,

    pub ai_cfg_open: bool,

    /// 在途请求 id（None = 空闲；结果按 id 匹配，过期忽略）
    pub ai_pending: Option<u64>,

    /// 流式输出进行中（最后一条 ai_msgs 为在途 assistant 消息，Delta 继续拼接）
    pub ai_stream_open: bool,

    /// 思考模式开关（与 `ai_cfg.thinking` 同源；面板上的 Toggle 直接改它并落盘）
    pub ai_thinking: bool,

    /// 在途请求的取消旗标（None = 无在途请求；置位后线程发 `Cancelled` 收尾）
    pub ai_cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,

    /// 在途的**交互式 agent** 运行（v1.1 §3.9）。`None` = 本轮不是 agent 模式
    /// （纯问答，或模型不支持工具调用）。
    pub ai_agent: Option<AgentRun>,

    /// 已展开「思考过程」的消息 id 集（流式期间自动展开，收尾时自动折叠）
    pub ai_think_expanded: HashSet<u64>,

    /// 已展开「明细」的轨迹行（键 = `(消息 id, call_id)`）。
    ///
    /// 为什么不像「思考过程」那样只存消息 id：一条回复有多行轨迹，每行一份
    /// 明细，展开态必须落到**行**上。`call_id` 由 agent 侧生成且在一轮内唯一。
    pub ai_trace_expanded: HashSet<(u64, String)>,

    /// 定时任务运行 → 会话的对应表（`run_id` → `ai_chat_id`）。
    ///
    /// 存在的理由有两条：
    /// 1. **去重**：每个 tick 都会发现「刚跑完的那次」，没有这张表就会每秒给
    ///    同一次运行建一条会话；
    /// 2. **回跳**：用户点某次运行的「会话」时，要切回那条已经存在的会话，
    ///    而不是每点一次多一条。
    pub ai_run_chats: std::collections::HashMap<String, u64>,

    /// 调度器启动时刻（unix 秒）。只给**启动之后**跑完的运行自动建会话 ——
    /// 历史记录（含重启前跑的那批）在用户点「会话」时按需建，
    /// 否则一启动就往聊天记录里灌几十条陈年会话。
    pub ai_runs_baseline_at: u64,

    /// AI 输入框（**多行**）。
    ///
    /// 2026-09-20 运行反馈「AI助手 输入栏可以上下调整大小」：类型从 `InputState`（单行）
    /// 换成 `TextareaState`（多行）—— 单行输入框的高度写死一行，把输入栏拖高只会多出
    /// 一截空白，「调整大小」就没意义了。Enter 语义不变（Enter 发送、Shift+Enter 换行），
    /// 由 `submit_on_enter(true)` 与下面那条 `InputEvent::PressEnter` 订阅共同保证。
    pub ai_input: Entity<TextareaState>,

    /// 输入栏高度（2026-09-20「AI助手 输入栏可以上下调整大小」）。
    ///
    /// 由 AI 面板里那根可拖拽分界线写回（`ai_panel::render` 的 `on_resize`），
    /// 口径与 `settings.sidebar_width` / `ai_chat_width` 一致：拖拽过程中面板自己跟手，
    /// 松手才落到这里。**不落盘** —— 那是「这次看着舒服」的临时尺寸，
    /// 不像侧栏宽度那样值得写进 settings.json。
    pub ai_composer_h: f32,

    pub ai_cfg_base: Entity<InputState>,

    pub ai_cfg_model: Entity<InputState>,

    pub ai_cfg_key: Entity<InputState>,

    /// 「免确认白名单」的**添加**输入框（设置 - Agent 页的白名单配置框）。
    ///
    /// 只用于新增：白名单的事实源是 `ai_cfg.auto_approve`（与命令卡片的
    /// 「加入白名单」同一个字段），输入框加完即清空，故不参与 `prefill_ai_inputs`
    /// 回填、也不随快照回滚 —— 它本来就不持有任何配置。
    pub ai_cfg_whitelist: Entity<InputState>,

    /// 白名单配置框里的**搜索**框（2026-09-22「支持搜索命令」）。
    ///
    /// 与 `ai_cfg_whitelist` 一样是纯界面状态（筛选条件不是配置），
    /// 关掉弹窗即失效：不落盘、不进快照、`prefill_ai_inputs` 也不回填它。
    pub ai_wl_search: Entity<InputState>,

    /// 白名单配置框的两个页签（0 自定义 / 1 内置，见 `whitelist_dialog` 的
    /// `TAB_CUSTOM` / `TAB_BUILTIN`）。
    ///
    /// 与搜索框同一口径的纯界面状态，但**必须持久**在 `MainView` 上：页签下标
    /// 逐帧现读，若每帧重建就永远停在第一页，点了内置页也切不过去（与温度滑块
    /// 必须持久是同一类原因）。不落盘、不进快照。
    pub ai_wl_tab: usize,

    /// 白名单配置框的名单本体（官方 `List` 的状态，2026-09-23 起替代 `Table`）。
    ///
    /// **必须持久**：`ListState` 手里有行缓存与滚动位置，逐帧新建等于每帧把滚动
    /// 位置清零（用户刚滚到一半就被拽回顶部）。它与 `ai_wl_tab` 一样只是界面
    /// 状态 —— 名单的事实源始终是 `ai_cfg.auto_approve`，这里存的是它这一帧的
    /// 投影，由 `whitelist_dialog` 每帧灌进去（`WhitelistDelegate::set_rows`）。
    pub(super) ai_wl_list: Entity<ListState<whitelist_dialog::WhitelistDelegate>>,

    /// 采样温度的官方 `Slider` 状态（2026-09-18 运行反馈「改为滑动轴，0.1 一个间隔」）。
    ///
    /// 必须是 `MainView` 的**持久**字段，不能在设置面板里逐帧新建：
    /// `Slider` 的拖动过程（按下 → 移动 → 抬起）横跨很多帧，状态每帧重建就等于
    /// 每次重绘都把手指下按的滑块拽回原位。取值与落库经 `cx.subscribe`（见 `new`）。
    pub ai_temp_slider: Entity<SliderState>,
}

// `DialogIoState`：对话框表单、文件面板、定时任务与外部 IO 的状态。

/// 连接对话框里选中的**认证方式**（2026-10-21 用户要求：默认密码认证）。
///
/// 它是「草稿」而不是落盘字段：`ConnectionProfile` 只存凭据本身
/// （`password` / `stored_password` / `key_path`），「用哪一种」由**保存时只写一侧**
/// 来表达 —— 选中密码就清掉私钥路径、选中私钥就清掉密码与已存密文。
/// 这样 `core::ssh::connect_only` 那条「密码优先，其次私钥」的顺序永远只会命中一支，
/// 用户的选择就是最终行为，不会出现「我明明选了私钥，它却用密码登」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMode {
    /// 密码认证（默认）
    Password,
    /// 私钥认证
    Key,
}

impl AuthMode {
    /// 从已落盘的连接反推：**有私钥且没有密码** = 私钥认证，其余 = 密码认证。
    ///
    /// 两者都有的老数据按 core 的判定顺序显示成「密码认证」（密码确实会先被试），
    /// 保存时会把那条不被使用的私钥路径清掉 —— 界面上看到的就是最终落盘的。
    pub fn of_profile(p: &rustshell_core::connections::ConnectionProfile) -> Self {
        if p.key_path.is_some() && p.password.is_none() {
            Self::Key
        } else {
            Self::Password
        }
    }
}

// 这一组最大（66 个字段），但内聚性反而最强：它们**几乎全是 `InputState` /
// `TextareaState` 句柄与「对话框当前在填什么」的枚举**，只有 `dialogs`、
// `settings_panel`、`files_panel`、`task_actions` 这四组代码碰它们。
// 之前它们混在 `MainView` 里，害得「连接相关状态」要在一堆 `dlg_*` 之间找。

/// 更新流程的阶段（2026-10-28）。
///
/// 仿 shell-rs 的 `Phase`，但用**一个枚举**而不是几个 bool：`checking` /
/// `downloading` / `ready` 天然互斥，写成 bool 迟早出现「正在下载又说已就绪」
/// 这种自相矛盾的组合，而界面只会照着画。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum UpdatePhase {
    /// 还没查过，或自动检查静默失败（用户没主动问，不该被弹窗打扰）。
    #[default]
    Idle,
    /// 正在请求 GitHub。
    Checking,
    /// 已是最新版本。
    UpToDate,
    /// 有新版本可用，尚未下载。
    Available,
    /// 正在下载产物。
    Downloading,
    /// 已就绪：Windows 上文件已换好（重启生效）；mac / linux 已把安装包交给系统。
    Ready,
    /// 失败原因（一句话，直接给界面）。
    Failed(String),
}

impl UpdatePhase {
    /// 正在跑（检查 / 下载）：按钮置灰、徽标显示进度，避免连点。
    pub fn is_busy(&self) -> bool {
        matches!(self, Self::Checking | Self::Downloading)
    }
}

pub struct DialogIoState {
    pub dlg_name: Entity<InputState>,

    pub dlg_host: Entity<InputState>,

    pub dlg_port: Entity<InputState>,

    pub dlg_user: Entity<InputState>,

    /// 密码输入。非空即加密记住（「记住密码」开关已取消，见 `dialog_save`）
    pub dlg_pass: Entity<InputState>,

    /// 认证方式草稿（单选框；默认 [`AuthMode::Password`]）。
    ///
    /// 决定对话框里渲染哪一个凭据字段（密码框 / 私钥路径），也决定保存时
    /// **保留哪一侧、清掉哪一侧** —— 见 [`AuthMode`] 的说明。
    pub dlg_auth: AuthMode,

    /// 私钥路径输入（2026-09-22 用户要求「支持私钥连接」）。
    ///
    /// 2026-10-21 起它只在认证方式选「私钥认证」时出现：core 的认证顺序仍是
    /// 「密码优先，其次私钥」（`core/src/ssh.rs::connect_only`），但保存时只会
    /// 留下被选中那一侧的凭据，顺序在单侧有值时不可能产生歧义。
    pub dlg_key_path: Entity<InputState>,

    /// 系统「打开」对话框选中的私钥路径（一次性暂存）。
    ///
    /// 为什么要有这一格：写 `InputState` 需要 `&mut Window`，而
    /// `prompt_for_paths` 的等待是异步的（回调里只有 `AsyncApp`）。
    /// 按本仓既定的两段式处理 —— 回调里只置旗，`render_dialog` 兑现。
    pub dlg_key_pick: Option<String>,

    /// 连接测试结果（None = 没测过）：(是否成功, 文案)。
    /// 成功走 success 变体、失败走 error 变体，两个 Alert 共用同一个字段以免
    /// 出现「上一条失败提示还挂在屏幕上」的错觉。
    pub dlg_test: Option<(bool, String)>,

    /// 测试进行中：按钮转「测试中…」并挡住重复点击（一次测试要握手+认证，
    /// 连点三次就是三个后台线程在敲同一台机器）。
    pub dlg_testing: bool,

    /// 备注输入（2026-09-18 新增）。用官方 `TextareaState`（多行）而不是 `InputState`：
    /// 备注天然是多行自由文本（用途 / 跳板机 / 到期日），单行框里写不下也看不见全貌。
    pub dlg_notes: Entity<TextareaState>,

    /// 连接对话框里**待保存**的端口转发规则（F-3）。
    ///
    /// 与 `conn_store` 里那份的关系是「草稿 vs 事实」：本字段只在对话框开着时
    /// 有意义，保存时整体写回 `ConnectionProfile::forwards`。取消不动已落盘的规则
    /// —— 与表单里其它字段同一口径（改到一半就走人不该留下半套配置）。
    ///
    /// 2026-09-29 起「高级」界面隐藏（用户要求「保持简单」）：字段与保存路径都
    /// 原样保留 —— 编辑时回填、保存时原样写回（**不能**在这条链路上清掉用户配过
    /// 的隧道），新建取默认。下面的会话日志 / 代理 / 自动重连同此：界面没有入口，
    /// 但数据仍要原值往返，界面消失 ≠ 静默清空。
    pub dlg_forwards: Vec<ForwardRule>,

    /// 连接对话框里**待保存**的会话日志覆盖（F-7）。三态：
    /// `None` = 跟随全局设置，`Some(true)` = 强制记录，`Some(false)` = 强制不记录。
    ///
    /// 与 `dlg_forwards` 同属「草稿」：只在对话框开着时有意义，保存时整体写回
    /// `ConnectionProfile::session_log`；取消不动已落盘的值。
    pub dlg_session_log: Option<bool>,

    /// 连接对话框里**待保存**的断线自动重连覆盖（F-4）。三态同 `dlg_session_log`：
    /// `None` = 跟随全局（`Settings::auto_reconnect`），`Some(true)` = 总是重连，
    /// `Some(false)` = 从不重连。
    ///
    /// 重试**次数与间隔不在这里**：那是本机网络条件的产物，留在设置里
    /// （见 `ConnectionProfile::auto_reconnect` 的说明）。
    pub dlg_auto_reconnect: Option<bool>,

    // ---- 代理（F-10）----
    /// 连接对话框里**待保存**的代理档位：跟随全局 / 直连 / 自定义。
    ///
    /// 与 `dlg_session_log` 同属「草稿」：只在对话框开着时有意义，保存时与下面
    /// 五个输入框合成 `ProxyChoice` 写回 `ConnectionProfile::proxy`。
    pub dlg_proxy_mode: rustshell_core::proxy::ProxyMode,

    /// 自定义档位下的代理类型（HTTP / SOCKS5），循环按钮切换。
    pub dlg_proxy_kind: rustshell_core::proxy::ProxyKind,

    pub dlg_proxy_host: Entity<InputState>,

    pub dlg_proxy_port: Entity<InputState>,

    pub dlg_proxy_user: Entity<InputState>,

    /// 自定义代理的口令草稿。**明文**：与对话框里的连接口令同一条口径
    /// （编辑时回填已解出的明文）。
    pub dlg_proxy_pass: Entity<InputState>,

    /// 打开对话框那一刻**这条连接的自定义代理**（`None` = 当时没有）。
    ///
    /// 两个用途，都围绕「口令留空 = 不改口令」这一条口径：
    /// ① 目标指纹由它算出 —— 代理目标变了就不许沿用（`core::proxy::may_inherit_password`）；
    /// ② 判定通过时，旧密文与旧明文从这里**原样写回**（只写回密文的话，本次会话里
    /// 这条连接会带着空口令去敲代理，直到下次启动重解）。
    pub dlg_proxy_prev: Option<rustshell_core::proxy::ProxyConfig>,

    /// 连接对话框里选中的**既有**分组 id（None = 未分组）。引用走 id 而非名字：
    /// 分组改名后这条归属仍然成立。
    pub dlg_group_id: Option<String>,

    /// 分组名对话框的输入框由 `GroupNameDialogView` 自带（见该类型注释：
    /// 输入框留在 `MainView` 里会让每次击键都重排整窗口）。
    /// 新建分组的**锚点**分组 id：新分组插在该分组下面（`None` = 追加到末尾）。
    /// 只在新建时有意义 —— 改名改的是既有分组，没有位置问题。
    pub dlg_group_after: Option<String>,

    /// 新建**子分组**的目标父分组 id（2026-09-17「连接组里还有组」）。
    /// 与 `dlg_group_after` 互斥使用：锚点 = 同级新建，父分组 = 建到里面。
    pub dlg_group_parent: Option<String>,

    /// 连接对话框表单校验错误（None = 无）
    pub dlg_error: Option<String>,

    /// 「保存之后接着连」的意图（2026-09-22）。
    ///
    /// 由来：`connect_profile` 发现这条连接既没有密码也没有私钥时，会把编辑对话框
    /// 打开让用户补凭据 —— 用户的原始意图始终是「连接」，补凭据只是必经步骤。
    /// 以前这件事由「保存并连接」按钮承担；按钮按用户要求撤掉之后，意图改由本字段
    /// 承载，否则那条路径会停在列表里，用户得再点一次连接。
    /// 只有**从连接入口进来**才会置位；「编辑」入口不置位。
    pub dlg_connect_after: bool,

    /// 设置面板的拖动位移（px，相对 Root 算出的默认位置）。
    /// 官方 `Dialog` 只把面板放在「水平居中 + 视口高度 1/10」处，自身不提供拖动；
    /// 这里用标题栏当抓手，把位移经 `margin` 叠加到那个默认位置上。
    pub set_drag_offset: (f32, f32),

    /// 拖动起点（鼠标按下时的窗口坐标；None = 未在拖动）
    pub set_drag_from: Option<(f32, f32)>,

    pub dlg_qc_label: Entity<InputState>,

    /// 命令正文：官方 Editor（代码编辑器，带行号/缩进/多行）而不是单行 Input
    pub dlg_qc_script: Entity<EditorState>,

    /// 命令对话框的「末尾添加回车」勾选态（v4）。
    pub dlg_qc_newline: bool,

    /// 设置面板校验/保存提示（None = 无）
    pub set_hint: Option<String>,

    /// 提示语义：true = 成功/信息（accent 色），false = 校验错误（danger 色）
    pub set_hint_ok: bool,

    /// 更新流程走到哪一步（2026-10-28，仿 shell-rs 的 `Phase`）。
    ///
    /// 只用一个枚举而不是「若干个 bool」：`checking` / `downloading` / `ready`
    /// 互相排斥，写成 bool 迟早出现「同时在下载又说已就绪」的脏状态。
    pub update_phase: UpdatePhase,

    /// 最近一次检查得到的最新发布；None = 还没查过或上次失败。
    pub update_release: Option<rustshell_core::update::LatestRelease>,

    /// 启动时的自动检查是否已经发起过。
    ///
    /// 不能用 `update_phase == Idle` 代替：自动检查**失败**之后阶段会回到 Idle，
    /// 那样每帧都会再发一次请求（渲染路径里跑定时任务，CPU 与网络都会被打满）。
    pub update_auto_started: bool,

    /// 已下载到本地的产物（`.exe` / `.deb` / `.dmg`）。
    ///
    /// 与 `update_phase` 分开存：`Ready` 需要它在，而失败时也要能告诉用户
    /// 「刚才存到哪儿了」（半截文件在 [`rustshell_core::update::download`] 里已删）。
    pub update_artifact: Option<std::path::PathBuf>,

    /// 设置面板打开时的快照（SP-SAVE-01）：取消 / ESC / 点遮罩 / 右上角 X 一律回滚，让「取消」名副其实（此前只有主题等少数字段能回滚）。
    pub(super) set_snapshot: Option<settings_panel::SettingSnapshot>,

    // 文件面板（M5 SFTP）
    /// 当前浏览的远端目录
    pub files_path: String,

    /// 路径栏（自定义路径）：显示当前目录，回车即跳转
    pub files_path_input: Entity<InputState>,

    /// 用户正在路径栏里手输（此时不要用目录列表回填，免得打断输入）
    pub files_path_typed: bool,

    /// 待回填到路径栏的目录（轮询无 window，回填动作延到下一帧 render）
    pub files_path_sync: Option<String>,

    pub files_entries: Vec<rustshell_core::sftp::SftpEntry>,

    /// 文件面板的**行展示缓存**（目录列表变化时重建一次，见
    /// [`MainView::rebuild_files_rows`]）。
    ///
    /// 用 `Rc<Vec<_>>` 而不是每帧从 `files_entries` 现算：虚拟列表的 item 闭包是
    /// `'static`，只能捕获所有权，`Rc` 克隆是 O(1)，不会变成「每帧整表深拷贝」。
    pub files_rows: std::rc::Rc<Vec<crate::files_panel::FileRow>>,

    pub files_loading: bool,

    /// 传输队列。**没有** `files_error` 了（2026-10-27）：目录读取失败、上传被拒、
    /// 传输失败这些提示一律走消息框（`crate::notice`），不在列表里画一行字。
    pub files_transfers: Vec<TransferState>,

    /// 已加载过目录的会话 id（切换会话后重扫）
    pub files_loaded_session: Option<String>,

    // ---- 独立 SFTP 标签页（2026-10-26）----
    /// 本地窗格当前目录（绝对路径）。初始 = 本机 home。
    pub sftp_local_path: String,
    /// 本地窗格的行缓存；`sftp_local_for` 记着它是哪个目录的，换了目录才重读。
    pub sftp_local_rows: Vec<crate::sftp_tab::LocalRow>,
    /// 行缓存对应的目录（`None` = 需要重读）
    pub sftp_local_for: Option<String>,
    /// 本地窗格里被单击选中的项（「上传」吃它）
    pub sftp_local_sel: Option<String>,
    /// 远端窗格里被单击选中的项（「下载」吃它）
    pub sftp_remote_sel: Option<String>,

    /// SFTP 标签页顶部的远端路径输入框：可编辑，回车跳转（2026-10-26 用户要求）。
    pub sftp_path_input: Entity<InputState>,
    /// 待回填的远端目录（`SftpEvent::Listing` 在 `poll` 里到达，没有 `Window`；
    /// 下一帧由 `render` 落进输入框）
    pub sftp_path_sync: Option<String>,
    /// 用户正在这个框里手输 —— 此时不回填，免得把半截输入顶掉
    pub sftp_path_typed: bool,

    /// 本地窗格的路径输入框（2026-10-27 用户要求「左右两边路径都可以手动改动」）
    pub sftp_local_path_input: Entity<InputState>,
    /// 本地路径框的待回填值（同 `sftp_path_sync`：poll 里排队、render 里落进输入框）
    pub sftp_local_path_sync: Option<String>,
    /// 用户正在本地路径框里手输
    pub sftp_local_path_typed: bool,
    /// **本地**窗格的排序口径（点左半边的列头切换）。
    ///
    /// 2026-10-30 用户要求「排序只需要触发对应窗口」：此前两侧共用一份，点左边列头
    /// 会把右边也一起改掉 —— 两个目录各看各的，排序没有理由联动。
    pub sftp_sort_local: crate::sftp_tab::SortSpec,
    /// **远端**窗格的排序口径（点右半边的列头切换）。
    pub sftp_sort_remote: crate::sftp_tab::SortSpec,
    /// 本地窗格的列宽：`[名称, 大小, 修改时间]`（类型是最后一列，吃剩余空间）。
    pub sftp_cols_local: [f32; 3],
    /// 远端窗格的列宽：`[名称, 大小, 修改时间, 类型, 权限]`（所有者吃剩余空间）。
    pub sftp_cols_remote: [f32; 5],
    /// 正在拖的列宽：`(起点指针 x, 哪一侧, 第几列)`。`None` = 没在拖。
    pub sftp_col_drag: Option<(f32, crate::sftp_tab::SftpSide, usize)>,

    /// 分组名对话框正文视图（自带名称输入框，见 `GroupNameDialogView`）
    pub group_name_body: Entity<GroupNameDialogView>,

    // 定时任务与预警
    /// 任务定义（`tasks.json`）。渲染只读它，改动经 `task_actions` 统一落盘。
    pub task_store: rustshell_core::tasks::TaskStore,

    /// 告警规则与去重状态（`alert_rules.json`）。
    pub alert_store: rustshell_core::alerts::AlertStore,

    /// SMTP 配置（`smtp.json`）。2026-10-12 起投递已实现（`smtp_client`），
    /// 面板上的「发送测试邮件」按钮可用。
    pub smtp_cfg: rustshell_core::notify_smtp::SmtpConfig,

    /// 测试邮件正在发送中：按钮据此置灰，避免连点发出多封。
    pub smtp_testing: bool,

    /// SMTP 口令输入框。与 `ai_cfg_key` 同一口径：密文不可逆故**不回显**，
    /// 留空即沿用已保存的口令；输入框掩码显示。
    pub smtp_cfg_pass: Entity<InputState>,

    /// 全局代理（F-10）的口令输入框。与 `smtp_cfg_pass` 同一条纪律：密文不可逆故
    /// **不回显**，留空即沿用已保存的口令；输入框掩码显示。
    pub proxy_cfg_pass: Entity<InputState>,

    /// 打开设置面板那一刻的全局代理（`None` = 当时没配）。
    ///
    /// 与对话框的 `dlg_proxy_prev` 同一用途：「留空 = 不改口令」只在**代理目标没变**
    /// 时成立（`core::proxy::may_inherit_password`）；判定通过时旧密文与旧明文都从这里
    /// 写回。面板里的行是**双向绑定**的（改一下就在 `settings.proxy` 上落了痕），
    /// 没有这份快照就无从知道用户改没改目标。
    pub proxy_cfg_prev: Option<rustshell_core::proxy::ProxyConfig>,

    /// 调度器（常驻 tick 线程 + 运行记录）。`None` = 尚未启动（未配置时懒建）。
    pub scheduler: Option<std::sync::Arc<rustshell_core::runner::Scheduler>>,

    /// 任务面板里展开查看运行记录的任务 id（`None` = 全部收起）。
    ///
    /// 只放一个 id：侧栏只有 240px，同时展开两条会让列表变成一团。
    pub task_expanded: Option<String>,

    /// 任务面板的搜索框（按任务名过滤列表）。空串 = 不过滤。
    ///
    /// 状态挂在视图上：任务一多（几十条）就得能查，而输入中途的每个字都要
    /// 保留 —— 逐帧重建的输入框会丢光标与草稿（与 `conn_search` 同一约定）。
    pub task_search: Entity<InputState>,

    /// 任务面板的瞬时提示（执行反馈 / 校验失败），2s 后清除。
    pub task_toast: Option<String>,

    /// 提示代次：连点时只有最后一次的定时器生效（与 `quick_toast_seq` 同口径）。
    pub task_toast_seq: u64,

    /// 上次调度 tick 的 unix 秒（`poll` 是 300ms 一轮，调度按秒节流）。
    pub tasks_last_tick: u64,

    /// 任务编辑对话框的草稿（`None` = 未在编辑）。
    ///
    /// 草稿与落库分离：对话框里点「取消」要**一个字都不改**地回到原样。
    /// 直接编辑 `task_store` 里的那条，取消就变成了「改回去」，而中间任何一次
    /// 误触都会留下痕迹。
    pub task_draft: Option<rustshell_core::tasks::Task>,

    /// 草稿的目标选择方式（`0` = 分组，`1` = 连接）。**没有「标签」**：
    /// 标签分类已按运行反馈撤下（`TaskTarget::tags` 只为兼容既有任务保留）。
    pub task_target_kind: u8,

    /// 任务名称（**用户可填**）。留空时才回退到「AI 要求」首行 ——
    /// 名称回到表单里，是因为列表行、AI 会话标题、通知里都拿它当标识，
    /// 用户得有个正式的地方改它。
    pub dlg_task_name: Entity<InputState>,

    pub dlg_task_cron: Entity<InputState>,

    /// 当前编辑的是否为**新建的空壳**（`task_add_template` 刚建的）。
    /// 取消时据此删除空壳 —— 只有明确知道「这是我刚建的」，删掉才不冤。
    pub dlg_task_is_new: bool,

    /// 选中的分组 id（单选）。`None` = 没选。
    ///
    /// 存 **id** 不存名字：分组改名后任务不该丢目标（与 `TaskTarget::groups` 同一口径）。
    /// 用选择而不是文本输入 —— 让用户手打分组名，打错的代价是「任务跑空了」，
    /// 而列表里点一下不可能打错。
    pub dlg_task_group_sel: Option<String>,

    /// 选中的连接 id（多选）。
    pub dlg_task_profiles_sel: Vec<String>,

    /// 「AI 要求」（**多行** textarea，2–10 行随内容自适应高度）。
    pub dlg_task_prompt: Entity<gpui_component::input::TextareaState>,

    /// 草稿通知策略的勾选（异常 / 失败 / 每次运行后）。
    pub dlg_task_notify: (bool, bool, bool),

    /// 对话框内的校验错误（文本，不是只有红框 —— 见 §13.2）。
    pub task_dlg_error: Option<String>,
}
