//! 主视图（3.1 节 整体布局）：TitleBar + ActivityBar + Sidebar + Tab/终端 + 快捷命令栏 + 状态栏。

// N14：对话框 / 信任流程 / 设置面板等「对话域」渲染拆分至子模块。
// 子模块可访问 MainView 的私有字段与方法（Rust 隐私：父模块私有项对子模块可见），
// 因此无需放宽字段可见性即可完成拆分。
mod dialogs;
// N24（P1-13）：按功能域拆分，会话 / 终端 / AI / 快捷命令 / 设置 / 命令入口。
// 子模块均为`impl MainView`的续块，共享同一实体与私有字段。
mod ai_actions;
// AI 会话落盘（2026-09-23「AI对话需要保存」）：存储格式 + 读写 + 落盘时机。
mod ai_persist;
mod connections;
// 会话布局落盘与恢复（F-9，规格 §1.9）：快照组装 / 节流落盘 / 启动恢复 / 退出钩子。
// 与 `ai_persist` 同一手法（指纹脏标记 + `poll` 节流 + 退出钩子 + `cfg(test)` 隔离）。
mod layout_persist;
// 退出钩子与 AI 会话那条并列挂在 `main_view` 根上：shell 层只认这一处入口，
// 不该知道布局落盘住在哪个子模块（模块本体保持私有）。
pub use layout_persist::ui_install_flush_session_layout_on_quit;
mod keybind_actions;
mod quickcmd_actions;
mod settings_actions;
// 2026-09-14：设置面板改用官方`Settings`组件装配（规格见
// docs/设置面板Settings组件重构-2026-09-14.md），装配逻辑独立成模块。
mod settings_panel;
// 「免确认白名单」配置框（设置 - Agent 页的「配置」按钮，2026-09-22 起由内联
// 列表改为弹窗）。弹窗要在设置面板之上再叠一层，故独立成模块单独走 `open_dialog`。
mod whitelist_dialog;
// 标签分屏（向右/向下拆分、取消分屏）与窗口背景图缓存。
// 决策逻辑在 core::split（纯函数），本模块负责落到会话与视图上。
mod split_actions;
// 命令补全浮层（C-1）：Ctrl+Space 唤起、边打字边过滤、↑↓ 选、Tab/Enter 采纳。
// 状态留在 `MainView::completion`，本模块只放渲染壳（见 `CompletionBarView` 文档）。
mod completion_bar;
// 引擎回调（跑在 `Context<TerminalView>` 里）回宿主的两个入口。
// 与 `layout_persist` 同一手法：模块本体私有，只把 shell / ui 层真正需要的
// 那两个符号提出来 —— 终端视图的装配方不该知道浮层的状态住在哪。
pub use completion_bar::{
    ui_call_completion_enter, ui_call_completion_key, ui_call_completion_refresh,
};
// 命令历史落盘 + 按连接指纹分桶（C-1 的遗留项 1 与 2）。
// 与 `ai_persist` 同一手法（指纹脏标记 + `poll` 节流 + 退出钩子 + `cfg(test)` 隔离）。
// 文档：[`completion_persist`] 模块头解释了「为什么两个遗留项必须一起做」。
mod completion_persist;
// 定时任务域：调度器启停、任务增删改、立即执行、运行记录读取。
pub mod task_actions;
mod terminal_actions;
// 粘贴保护的宿主半边（F-5）：弹确认框、兑现确认 / 取消（判定与写入在引擎里）。
mod paste_guard;

// 「掉线收场」的端到端护栏：要读 `tabs` / `palette_state` 这些私有字段，
// 所以只能住在 `main_view` 的子模块里。
//
// ★ 声明这里**不能**带 `#[cfg(test)]`：本仓所有扫源码的护栏都用
// `test_support::prod()` 取源，它从**第一个** `#[cfg(test)]` 起截断整份源码。
// 把它写在这儿，`prod(include_str!("main_view.rs"))` 就只剩最前面几行，
// 十几个「扫 main_view.rs」的护栏会集体假红（实测一次挂 16 个）。
// 故门禁放进去到子模块文件内部的 `#[cfg(test)] mod e2e`。
mod dead_session_e2e;

// 2026-10-12（开源评审 P0-4 刀一）：窗口外壳 `AppShell` 与四个「独立子视图」
// （`SidebarPanelView` / `AiChatPanelView` / `GroupNameDialogView` / `TermSearchBarView`）
// 一起搬进 `app_shell.rs`，外加脏传播归属机制 `ChildView` / `ViewDirty` / `ViewEpoch`。
//
// 为什么它们本来就不该住在 `main_view.rs`：每个都是**独立 Entity + 独立 impl Render**，
// 与 MainView 的协调逻辑（new / poll / render）只共享一个 `Entity<MainView>` 句柄，
// 没有任何共享局部变量或私有状态。留在同一文件只是物理聚集，让本体读起来像「一个巨物」。
//
// ★ `AppShell` 与四个视图保持 `pub`（`pub use` 转出去）：shell 层的窗口装配
// （`Root::new(AppShell)`）与各护栏测试的同构装配要用。
// ★ `ChildView` / `ViewDirty` / `ViewEpoch` 是 `pub(crate)` —— 它们只是 ui crate 内部的
// 脏传播机制，不该进公开 API。所以走 `pub(crate) use` 转出，crate 内 `super::ChildView`
// 这类调用点（`completion_bar.rs:37`、`connections.rs:15`、`task_actions.rs:185`）
// **一个字都不用改**。
mod app_shell;
pub use app_shell::{
    AiChatPanelView, AppShell, GroupNameDialogView, SidebarPanelView, TermSearchBarView,
};
pub(crate) use app_shell::{ChildView, ViewDirty, ViewEpoch};

// 2026-10-12（开源评审 P0-4 刀二）：`MainView` 的**领域模型**搬出本体 ——
// `tab_model`（标签 / 窗格 / 会话收场判定）、`domain`（AI 会话 / 传输条目 / 对话框模式）。
// 理由见两个文件的头部文档：它们是纯数据 + 纯判定，不依赖视图。
//
// 与刀一同理，外部调用点（`files_panel.rs:1670` 的 `main_view::TabEntry`、
// `connections.rs:15` 的 `super::TabEntry` / `super::DialogMode` / `super::RetryLedger`）
// **一个字都不用改**，全部由下面两行 `pub use` 原样转出。
mod domain;
mod tab_model;
// 2026-10-12（开源评审 P0-4 刀三）：`MainView` 原先 175 个平级字段按功能域归成
// 五个子结构（标签 / 连接树 / 面板框架 / AI+快捷指令 / 对话框与 IO）。
// 归属理由写在 `ui_state.rs` 各结构体的头部文档里。
//
// 改写由 `scripts/coverage/split_mainview_fields.py` 完成（可重跑，带覆盖完整性断言）。
mod ui_state;
pub use ui_state::{AiUiState, ConnUiState, DialogIoState, PanelUiState, TabsState};
// 公开面：`TabEntry` 等被 `files_panel.rs` 等模块按 `main_view::TabEntry` 引用；
// `DialogMode` / `chat_title_from` / `take_due_retries` / `display_tab_title`
// 原本就是 crate 内可见（`pub(crate)` 或私有），不进公开 API。
pub(crate) use domain::{chat_title_from, DialogMode};
pub use domain::{AgentRun, AiChat, TransferState};
pub use tab_model::{
    classify_session_end, DeadBarRetry, PaneSide, RetryLedger, SessionEnd, SplitPane, TabEntry,
};
pub(crate) use tab_model::{display_tab_title, take_due_retries};

use std::collections::HashSet;
use std::time::Duration;

use gpui::{
    div, px, App, AppContext as _, Context, ElementId, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, ObjectFit, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, StyledImage as _, Window,
};
use gpui_component::{
    h_flex,
    input::{EditorState, InputEvent, InputState, TextareaState},
    list::ListState,
    slider::{SliderEvent, SliderState},
    v_flex, Icon, IconName, Sizable as _, WindowExt as _,
};

use rustshell_core::connections::ConnectionStore;
use rustshell_core::metrics::MetricsStore;
use rustshell_core::quickcmd::QuickCommandStore;
use rustshell_core::session::{SessionId, SessionKind, SessionState};
use rustshell_core::settings::Settings;
use rustshell_core::split::SplitAxis;
use rustshell_core::ssh::ConnectResult;
use rustshell_platform::paths;

// Actions（7.2 节 M2 子集）

gpui::actions!(
    rustshell_ui,
    [
        NewTab,
        CloseTab,
        ToggleSidebar,
        TabNext,
        TabPrev,
        OpenSettings,
        CommandPalette,
        TermTab,
        TermShiftTab,
        SearchInTerminal,
        SplitRight,
        SplitDown,
        // F2：重命名连接树里选中的连接（2026-09-17 运行反馈）。
        RenameConnection,
        // 命令窗口的两条入口（2026-09-17 运行反馈「命令窗口新增【新增连接】【AI助手】」）：
        // 只在命令面板里派发，不绑默认键位。
        NewConnection,
        AiAssistant,
        // 2026-09-29 用户要求「取消恢复上次会话功能」：启动不再自动弹框，
        // 恢复改由命令面板里这一条**手动触发**（`NewConnection` 同一手法，
        // 只在命令面板派发，不绑默认键位）。
        RestoreLastSession
    ]
);

/// 带参数 Action：Ctrl+1~9 切换 Tab
#[derive(Clone, PartialEq, gpui::Action)]
#[action(namespace = rustshell_ui, no_json)]
pub struct SwitchTab {
    pub index: usize,
}

/// 自定义标题栏高度（px）。
///
/// 2026-09-18 运行反馈「最上面标题栏稍微降一点高度」：官方
/// `gpui_component::TitleBar` 默认 `TITLE_BAR_HEIGHT = 34px`（那是为「标题 +
/// 系统按钮」留的余量，本应用标题栏里只有一行品牌名与版本号，显得空）。压到
/// 30px —— 再矮于 28px 时 Windows 的最小化/最大化/关闭三枚 35px 宽按钮的可点
/// 高度会明显变小，手感变差，故只收 4px。
///
/// 2026-09-21 用户要求「标题栏和中间内容区的空白区域紧凑一点」：再收到 **28px**
/// —— 这正是上面那条下限，系统按钮的可点高度还剩 28px，不再继续收。
///
/// 官方渲染里 `.h(TITLE_BAR_HEIGHT)` 之后才 `refine_style(&self.style)`，而
/// `TitleBar` 实现了 `Styled`，所以调用点的 `.h(px(TITLE_BAR_HEIGHT))` 能覆盖它。
const TITLE_BAR_HEIGHT: f32 = 28.;

/// 标题栏品牌名字号（px）。
///
/// 2026-09-20 运行反馈「标题栏 RUSTSHELL 太大了，小一号」：品牌名原先跟着标题行基准
/// 字号（15px）走，比右侧版本号大得不成比例。压到 13px —— 缩一档而不是重新定一套
/// 比值，是因为它与版本号（10px）的**两级层次**要保持住，两个都动会让「谁主谁次」
/// 重新变得含糊。
///
/// 注意它**只**作用于品牌名：标题行那一层的 `.text_size(px(15.))` 不能一起降，
/// 那层同时决定右上角三枚系统按钮的图标尺寸（`Icon::render` 取继承来的字号）。
const TITLE_BAR_BRAND_SIZE: f32 = 13.;

/// 标题栏中段「命令入口」的宽度（px）。
///
/// 它长得像一个输入框、点一下就唤起命令面板（真正的输入由面板自己的输入框接管，
/// 见 `title_bar_command_entry` 的推导）。
const TITLE_BAR_CMD_W: f32 = 340.;

/// 标题栏命令入口的**视觉竖向微调**（px；正值 = 下移）。
///
/// 2026-10-09 运行反馈「搜索命令框往下一点」。命令入口在布局上**本来就是居中**的
/// （官方 `#bar` 的 `items_center`；`title_bar_command_entry_sits_below_the_bar_centre`
/// 量到：偏移归零时它的中心正好落在 14px = 标题栏中线）。偏上的是**观感** ——
/// 实机抓帧逐行取色（本机 1.5× DPI）：
/// - 标题栏可见带（`#0D1117` 从哪一行开始）= 物理 y 2..41；
/// - 命令入口 = y 5..34（30px = 20 逻辑 px）⇒ **上余 3px、下余 7px**（1:2.3）。
///
/// 差在哪：**可见带的上边界比布局盒的上边界低约 3 物理 px** —— 窗口最顶上那两行
/// 是系统画的窗口边框（浅色 `#B2B4B7`），不是应用内容。所以"布局居中"被削成
/// "屏幕上偏高"，这不是布局错了，改布局也治不好。
///
/// 取 **1px**（1.5× 下 = 1.5 物理 px）：命令入口中心移到可见带正中的 21.5 行 ⇒
/// 上余 5 行、下余 5 行（改前 3/7），把上面那点亏损补回来。**不取更大**：
/// 再往下就轮到"下余 < 上余"，从偏高翻成偏低。也**不能**用"看到偏上就补 2px"
/// 那种目测值 —— 实测亏损是 3 物理 px（1.5× 下 = 2 逻辑 px 量级），补多了就过头。
/// 换机器/换 Windows 版本（系统边框厚度、缩放不同）要**重新量一次**，判据就一句：
/// 可见带上下余量相等。
///
/// ★ 用相对偏移 `top` 而**不是** margin/padding：`div` 默认 `position: relative`，
/// `top` 只是一次绘制与命中的偏移、不参与布局 ⇒ 既不挤动左右两个 `flex_1` 配平块
/// （中段仍落在窗口几何中心，那是 `render` 刻意维持的性质），也不把 20px 的盒子撑高。
/// 反馈只针对这一个控件，影响面就钉在这一个盒子上。
///
/// 验收：`title_bar_command_entry_sits_below_the_bar_centre`（真渲染量几何、带变异
/// 验证）＋ 实机抓帧逐行取色（before/after 像素见 `.workbuddy/memory/` 当日日志）。
const TITLE_BAR_CMD_DY: f32 = 4.;

/// 标题栏中段的命令面板入口：一个「看起来是输入框」的按钮。
///
/// 为什么不放一个**真的** `TextInput`：真输入框要先拿到焦点才能打字，而焦点一旦
/// 落在标题栏，命令面板弹出来再抢焦点就是两个句柄打架（一个 `FocusId` 只能有一个
/// 认领者，见本仓「空主窗格快捷键」那条铁律）。做成按钮则只有一条路径：点它 →
/// 开面板 → 焦点自然归面板的输入框，用户「点一下再打字」的体感完全一致。
///
/// ★ `occlude()` 是这里能点得动的**唯一原因**，不能删：官方 `TitleBar` 把整条
/// `div#bar` 标成了 `WindowControlArea::Drag`，而 Windows 侧 `WM_NCHITTEST` 命中
/// 该区域就返回 `HTCAPTION` ⇒ 走系统非客户区拖动，应用**一个鼠标事件都收不到**。
/// gpui 的判定是「该窗口控制命中框的 id 是否在 `mouse_hit_test.ids` 里」，而
/// `hit_test` 会把光标下**所有**命中框都收进去（含祖先）⇒ 光靠「画在上层」没用。
/// `HitboxBehavior::BlockMouse` 是唯一能让 `hit_test` **提前 break** 的行为，
/// 祖先命中框因此不进 `ids`，Drag 也就不会被上报 —— 等于在拖动区里挖出一个
/// 可点击的洞。其余部分仍可拖动窗口。
fn title_bar_command_entry(cx: &mut Context<MainView>) -> impl IntoElement {
    use crate::theme::to_hsla as hex;

    let t = crate::theme::current(cx);
    div()
        .id("titlebar-command-entry")
        // 只给测试量几何用（正式构建里不产生任何东西）：本盒子没有 `TextInput`，
        // 只有一个 `id()`，而 `debug_bounds` **只认** `debug_selector`
        // ⇒ 不挂它，`title_bar_command_entry_sits_below_the_bar_centre` 就量不到。
        .debug_selector(|| "titlebar-command-entry".to_string())
        .occlude()
        .flex()
        .items_center()
        .gap_2()
        .flex_none()
        .h(px(20.))
        .w(px(TITLE_BAR_CMD_W))
        // 用户反馈「搜索命令框往下一点」的视觉微调，取值与推导见 `TITLE_BAR_CMD_DY`。
        // 相对偏移：不参与布局、不挤动兄弟节点，命中区跟着一起下移。
        .top(px(TITLE_BAR_CMD_DY))
        .px_2()
        .rounded(px(5.))
        .bg(hex(t.bg_base))
        .border_1()
        .border_color(hex(t.border_subtle))
        .cursor_pointer()
        .hover(|s| s.border_color(hex(t.accent)))
        .on_click(cx.listener(|m, _ev, w, cx| m.open_command_palette(w, cx)))
        .child(
            Icon::new(IconName::Search)
                .small()
                .text_color(hex(t.fg_muted)),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_size(px(11.))
                .text_color(hex(t.fg_muted))
                .text_ellipsis()
                .child(rustshell_core::i18n::t("搜索命令…")),
        )
        .child(
            div()
                .flex_none()
                .text_size(px(10.))
                .text_color(hex(t.fg_muted))
                .child("Ctrl+Shift+P"),
        )
}

/// 采样温度滑块的步长（2026-09-18 运行反馈「采样温度改为滑动轴 0.1 一个间隔」）。
///
/// 放在模块级而不是就地在 `SliderState::new()` 里写 `0.1`：这是**用户点名的口径**，
/// 抽成常量后 `temperature_slider_uses_a_tenth_step` 这条护栏才有东西可断言
/// （字面量散在装配代码里时，改成 0.05 也测不出来）。
const TEMPERATURE_STEP: f32 = 0.1;

/// 终端行高滑块的步长（2026-09-28）。
///
/// 0.05 是「手感」与「精细度」的折中：步长 0.1 时从 1.0 调到 1.2 只有两格，
/// 微调不到位；步长 0.01 又会让滑杆过密、拖起来像连续变量。抽成常量以便
/// 护栏断言（同 `TEMPERATURE_STEP` 的理由）。
const LINE_HEIGHT_STEP: f32 = 0.05;

/// 内容区卡片的圆角半径（px）。
///
/// 2026-09-18 运行反馈「参考 vscode 侧边栏和终端外圈线条变为圆角」：侧栏、终端区、
/// AI 对话面板各自成为一张圆角卡片（VS Code 1.9x 的同款观感）。
const CARD_RADIUS: f32 = 8.;

/// 卡片之间的间距 / 卡片与窗口边缘的间距（px）。
///
/// 与 `TITLE_BAR_HEIGHT` 同级放在模块作用域而不是就地在布局里写 `px(8.)`：
/// 「卡与卡之间」「卡与窗口边」必须**同源派生**（见 `render` 里的 `pad` 闭包），
/// 否则上下两个数字会各自漂移，出现「左边 8px、中间 6px」这种看不见但真实的不齐。
///
/// 2026-09-21 用户要求「左侧图标栏、标题栏和中间内容区的空白区域紧凑一点」：
/// 8 → 6。它是内容区**全部**留白的唯一来源（卡与卡、卡与窗口左右边缘、卡片上下
/// 与标题栏/窗口底边），改一处就是三处一起收；圆角与描边不动，观感只是「紧了一档」。
const CARD_GAP: f32 = 6.;

/// 「没有可投递会话」提示的存续时长（2026-09-18 运行反馈：该提示改为
/// **无需确认**的弹出消息）。
///
/// 取 4s 而不是命令栏瞬时提示的 2s：这条消息带一句原因
/// （「命令没有发出去：先新建本地终端，或连接一台服务器。」），
/// 要比「命令已不存在」那种一眼可读的短提示多留一点阅读时间。
/// 存续时长与提示内容一样是**用户可感知的行为**，所以抽成常量单独放这里。
const NO_SESSION_NOTICE_MS: u64 = 4000;

// 全局枢纽：菜单/子组件回调（拿不到 MainView 实体）经由 UiHub 回路由

pub struct UiHub {
    pub main: gpui::WeakEntity<MainView>,
}
impl gpui::Global for UiHub {}

/// 连接结果通道：SSH 连接线程 到 UI 轮询
pub fn connect_results() -> &'static (flume::Sender<ConnectResult>, flume::Receiver<ConnectResult>)
{
    use std::sync::OnceLock;
    static CH: OnceLock<(flume::Sender<ConnectResult>, flume::Receiver<ConnectResult>)> =
        OnceLock::new();
    CH.get_or_init(flume::unbounded)
}

/// 菜单回调 到 MainView 方法 的统一路由
pub fn ui_call<F>(id: &str, f: F, w: &mut Window, cx: &mut App)
where
    F: FnOnce(&mut MainView, &str, &mut Window, &mut Context<MainView>),
{
    let Some(hub) = cx.try_global::<UiHub>() else {
        return;
    };
    let main = hub.main.clone();
    let id = id.to_string();
    let _ = main.update(cx, |m, cx| f(m, &id, w, cx));
}

/// 非实体回调入口（供连接面板菜单复用）。`group_id` 是目标分组 id（`None` = 未分组）。
pub fn open_connect_dialog_for(group_id: Option<&str>, w: &mut Window, cx: &mut App) {
    let g = group_id.map(str::to_string);
    let Some(hub) = cx.try_global::<UiHub>() else {
        return;
    };
    let main = hub.main.clone();
    let _ = main.update(cx, |m, cx| m.open_connect_dialog(g, w, cx));
}

/// 「新建分组」入口（App 上下文）：连接树空白右键菜单与分组右键菜单共用。
///
/// `after` 是**锚点分组 id**：新分组建在它下面（同级），空白处右键与「新增分组」
/// 都不传（当时的「新建分组·同级插入」项已在 2026-09-20 撤下）；
/// `parent` 是**父分组 id**：分组右键的「新增分组」传入，新分组建到它里面。
/// 两者互斥（同传时以 `parent` 为准）；空白处右键全传 `None`，追加到末尾根级。
pub fn ui_call_new_group_dialog(
    after: Option<&str>,
    parent: Option<&str>,
    w: &mut Window,
    cx: &mut App,
) {
    let Some(hub) = cx.try_global::<UiHub>() else {
        return;
    };
    let main = hub.main.clone();
    let after = after.map(str::to_string);
    let parent = parent.map(str::to_string);
    if main
        .update(cx, |m, cx| m.open_new_group_dialog(after, parent, w, cx))
        .is_err()
    {
        log::warn!("「新建分组」未能路由到 MainView（实体已释放）");
    }
}

/// 「重命名分组」入口（App 上下文）：连接树分组右键菜单。
pub fn ui_call_rename_group_dialog(group_id: &str, w: &mut Window, cx: &mut App) {
    let Some(hub) = cx.try_global::<UiHub>() else {
        return;
    };
    let main = hub.main.clone();
    let gid = group_id.to_string();
    let _ = main.update(cx, |m, cx| m.open_rename_group_dialog(&gid, w, cx));
}

/// 「删除分组」入口（App 上下文）：连接树分组右键菜单。
///
/// 分组名与组内连接数在这里就**取值成快照**再进二次确认：弹层回调跑在弹层上下文里，
/// 拿不到 `MainView`（读 store 会 E0500），所以确认文案必须进弹层前定下来。
pub fn ui_call_confirm_remove_group(group_id: &str, w: &mut Window, cx: &mut App) {
    let Some(hub) = cx.try_global::<UiHub>() else {
        return;
    };
    let main = hub.main.clone();
    let gid = group_id.to_string();
    let _ = main.update(cx, |m, cx| {
        let Some(name) = m.conn_store.group_name(&gid).map(str::to_string) else {
            log::warn!("删除分组失败：分组已不存在 id={gid}");
            return;
        };
        // 嵌套分组：子分组里的连接也是「这个组的连接」，删除确认按整棵子树计数
        let members = m.conn_store.profile_ids_in_subtree(&gid).len();
        m.confirm_remove_group(&gid, &name, members, w, cx);
    });
}

/// 清空「待删除分组」记录。
///
/// `AlertDialog` 的 `on_close` 覆盖全部关闭路径（确认 / 取消 / ESC / 右上角 X），
/// 由它兜底调用 —— 否则用户用 ESC 关掉确认框后，`quick_pending_remove` 会永久
/// 停在 Some，下一次删除请求被首部守卫当成「已在确认中」挡掉。
pub fn ui_call_clear_quick_pending(cx: &mut App) {
    let Some(hub) = cx.try_global::<UiHub>() else {
        return;
    };
    let main = hub.main.clone();
    let _ = main.update(cx, |m, cx| {
        if m.ai_ui.quick_pending_remove.take().is_some() {
            cx.notify();
        }
    });
}

/// 连接某个分组下的全部连接（含全部子分组 —— 嵌套后「这个组里的连接」是整棵子树）。
/// `group_id` 是分组 id（不是名）。
pub fn connect_group(group_id: &str, w: &mut Window, cx: &mut App) {
    let g = group_id.to_string();
    let Some(hub) = cx.try_global::<UiHub>() else {
        return;
    };
    let main = hub.main.clone();
    let _ = main.update(cx, |m, cx| {
        let ids: Vec<String> = m.conn_store.profile_ids_in_subtree(&g);
        for id in ids {
            m.connect_profile(&id, w, cx);
        }
    });
}

pub fn copy_profile_summary(id: &str, cx: &mut App) {
    let Some(hub) = cx.try_global::<UiHub>() else {
        return;
    };
    let main = hub.main.clone();
    let _ = main.update(cx, |m, cx| {
        if let Some(p) = m.conn_store.profile(id) {
            let item = gpui::ClipboardItem::new_string(p.summary_line());
            cx.write_to_clipboard(item);
        }
    });
}

/// 连接树拖拽落点入口（App 上下文）：把连接移动到目标分组 **id**（`None` = 未分组）。
pub fn ui_call_move_connection(profile_id: &str, group_id: Option<&str>, cx: &mut App) {
    let Some(hub) = cx.try_global::<UiHub>() else {
        return;
    };
    let main = hub.main.clone();
    let pid = profile_id.to_string();
    let g = group_id.map(str::to_string);
    let _ = main.update(cx, |m, cx| {
        m.move_connection_to_group(&pid, g.as_deref(), cx);
    });
}

/// 连接树右键「上移 / 下移」入口（App 上下文）：连接在同一分组内挪一格。
///
/// `delta`：`-1` 上移、`+1` 下移。排序只改平铺次序，分组归属不动 ——
/// 跨组搬家仍走拖拽（`ui_call_move_connection`）。菜单项在首 / 末位本就置灰，
/// core 侧还会再挡一次越界，故这里不需要「失败提示」。
pub fn ui_call_move_connection_step(profile_id: &str, delta: i32, cx: &mut App) {
    let Some(hub) = cx.try_global::<UiHub>() else {
        return;
    };
    let main = hub.main.clone();
    let pid = profile_id.to_string();
    let _ = main.update(cx, |m, cx| m.move_connection_step(&pid, delta, cx));
}

/// 连接树右键「上移 / 下移」入口（App 上下文）：分组在同一层兄弟之间挪一格。
///
/// `delta`：`-1` 上移、`+1` 下移。只与同级兄弟换位（整棵子树一起搬），不换父 ——
/// 换父仍走拖拽（`ui_call_move_group`）。
pub fn ui_call_move_group_step(group_id: &str, delta: i32, cx: &mut App) {
    let Some(hub) = cx.try_global::<UiHub>() else {
        return;
    };
    let main = hub.main.clone();
    let gid = group_id.to_string();
    let _ = main.update(cx, |m, cx| m.move_group_step(&gid, delta, cx));
}

/// 「点框外失焦」的唯一实现：常驻输入框（快捷命令栏的「搜索命令」与「命令输入栏」、
/// 文件地址栏、连接树搜索）都挂它。
///
/// # 为什么不能只靠官方 `Input`
///
/// 官方输入框只在点到**另一个可聚焦元素**时才换焦点 —— 点终端、侧栏空白、树行都不
/// 会松手，于是光标一直闪、键盘输入继续被那个框吃掉（2026-09-18 用户要求
/// 「聚焦后，点击别的地方需要进行失焦」）。
///
/// 挂法是 `on_mouse_down_out`：gpui 在**捕获**阶段派发、且只在本元素边界**外**触发
/// （`gpui-pre/src/elements/div.rs`），所以点到别的输入框时那个框仍会在随后的冒泡里
/// 拿到焦点。
///
/// # 为什么先比对焦点句柄
///
/// 无条件 `window.blur(cx)` 会把**任何**框外点击都当成「松手」，包括发生在对话框
/// 里的点击（点「保存」按钮、点对话框空白）—— 那会把对话框正在编辑的输入框焦点
/// 一并收走。框外的点击到处都有，这里只收「本来就在这个框上」的焦点。
pub fn blur_input_on_outside_click(
    input: &gpui::Entity<InputState>,
    w: &mut gpui::Window,
    cx: &mut App,
) {
    if input.read(cx).focus_handle(cx).is_focused(w) {
        w.blur(cx);
    }
}

/// 连接树行右键（App 上下文）：作废库内部按**下标**记的「右击高亮」。
///
/// 官方 `Tree` 把 `right_clicked_ix` 画成行上一圈外框（`secondary_selected`），
/// 而它只在 `set_items` 或点**分组行**时才清 —— `gpui-base/src/tree.rs` 的
/// `toggle_expand` 对非分组行**提前 return**，于是右键一条**连接**之后那圈外框会
/// 一直留在那儿，再点别的连接也不消失（2026-09-18 用户反馈「连接选中框外线有时候
/// 一直停留在那，切换了也没用」）。
///
/// 这里把树的输入指纹置空 ⇒ 下一帧重建一次（`set_items` 随之清掉它，并按连接 id
/// 重新对齐真正的选中行）。代价是一次条目重建，只在右键时发生，菜单本身不受影响
/// （右键菜单挂的是容器 hitbox，不依赖树内部状态）。
pub fn ui_call_invalidate_conn_tree(cx: &mut App) {
    let Some(hub) = cx.try_global::<UiHub>() else {
        return;
    };
    let main = hub.main.clone();
    let _ = main.update(cx, |m, cx| m.invalidate_conn_tree(cx));
}

/// 把分组 `id` 的整棵子树挪到 `parent` 之下（`None` = 提到根级）。
///
/// 环形移动（挪进自己的后代）由 core 的 `move_group_under` 拒绝；
/// 拖拽把分组放到它自己的子分组行上时，这里要给用户可见的提示而不是静默失败。
pub fn ui_call_move_group(id: &str, parent: Option<&str>, w: &mut Window, cx: &mut App) {
    let Some(hub) = cx.try_global::<UiHub>() else {
        return;
    };
    let main = hub.main.clone();
    let gid = id.to_string();
    let p = parent.map(str::to_string);
    let _ = main.update(cx, |m, cx| {
        if m.conn_store.move_group_under(&gid, p.as_deref()) {
            if let Err(e) = m.conn_store.save(&paths::connections_path()) {
                m.set_quick_toast(
                    crate::notice::NoticeKind::Error,
                    &rustshell_core::i18n::ts(
                        format!("保存失败：{e}"),
                        format!("Save failed: {e}"),
                    ),
                    w,
                    cx,
                );
            }
            cx.notify();
        } else {
            m.set_quick_toast(
                crate::notice::NoticeKind::Warning,
                rustshell_core::i18n::t("不能把分组移到它自己或它的子分组里"),
                w,
                cx,
            );
        }
    });
}

/// 状态栏「复制服务器信息」：复制当前激活会话的连接摘要
pub fn ui_call_copy_server_info(cx: &mut App) {
    let Some(hub) = cx.try_global::<UiHub>() else {
        return;
    };
    let main = hub.main.clone();
    let _ = main.update(cx, |m, cx| {
        if let Some(tab) = m.active_tab() {
            let text = tab
                .session
                .profile_id
                .as_deref()
                .and_then(|pid| m.conn_store.profile(pid))
                .map(|p| p.summary_line())
                .unwrap_or_else(|| tab.session.title.clone());
            let item = gpui::ClipboardItem::new_string(text);
            cx.write_to_clipboard(item);
        }
    });
}

/// 状态栏「断开连接」：关闭当前激活会话 Tab
pub fn ui_call_disconnect_active(cx: &mut App) {
    let Some(hub) = cx.try_global::<UiHub>() else {
        return;
    };
    let main = hub.main.clone();
    let _ = main.update(cx, |m, cx| {
        if let Some(id) = m.tabs_state.active.clone() {
            m.close_tab_by_session(&id, cx);
        }
    });
}

/// 终端右键菜单「新建本地终端」
pub fn ui_call_new_local_shell(cx: &mut App) {
    let Some(hub) = cx.try_global::<UiHub>() else {
        return;
    };
    let main = hub.main.clone();
    let _ = main.update(cx, |m, cx| m.open_local_shell(cx));
}

/// 启动入口：请求首帧打开命令窗口（App 上下文，见 `MainView::pending_palette`）。
///
/// 2026-09-17 运行反馈「打开软件默认不打开本地 shell，打开命令窗口」：启动路径没有
/// `Window`，开浮层要 `window.focus`，所以这里只置标记，真正的打开落在 `render` 里。
pub fn ui_call_request_command_palette(cx: &mut App) {
    let Some(hub) = cx.try_global::<UiHub>() else {
        return;
    };
    let main = hub.main.clone();
    let _ = main.update(cx, |m, cx| m.request_command_palette(cx));
}

/// 启动入口（2026-09-28 改为按设置分派）。
///
/// 历史：2026-09-17 的运行反馈是「打开软件默认不打开本地 shell，打开命令窗口」，
/// 于是启动被**硬编码**成永远只开命令面板 —— 但那是用一个人的偏好覆盖所有人。
/// 一个**终端软件**启动后给一个终端，是最不需要解释的行为；power user 的第一诉求
/// 也正是「给我一个 shell」。现在按 [`StartupAction`] 分派，默认开本地终端，
/// 想要命令面板/什么都不做的人在设置里改一次（与 `session_restore` 同一条
/// 「改默认必须由用户按下」的口径）。
///
/// 启动路径拿不到 `Window`（开浮层要 `window.focus`），所以这里只置标记 / 发起
/// 无需窗口的动作，真正的打开落在各处的 `render` / 后台任务里。
pub fn ui_call_startup_action(action: rustshell_core::settings::StartupAction, cx: &mut App) {
    let Some(hub) = cx.try_global::<UiHub>() else {
        return;
    };
    let main = hub.main.clone();
    match action {
        rustshell_core::settings::StartupAction::LocalShell => {
            // 本地 Shell 的 spawn 在后台跑（ConPTY 冷启动实测可达数十秒），
            // 不需要 `Window`，故这里可以直接发起。
            let _ = main.update(cx, |m, cx| m.open_local_shell(cx));
        }
        rustshell_core::settings::StartupAction::CommandPalette => {
            let _ = main.update(cx, |m, cx| m.request_command_palette(cx));
        }
        rustshell_core::settings::StartupAction::RestoreSession => {
            // 不用做任何事：F-9 的 `layout_startup`（在 `MainView::new` 末尾）已经按
            // `session_restore` 三态处理过快照 —— 询问 / 直接恢复 / 不恢复。
            // 与 `LocalShell` 的差别只在「有没有快照」：恢复成功或弹了询问框时
            // 不该再开一个多余标签；一个标签都没有（无快照 / 用户明确不要）才补一个
            // 本地 Shell，而不是给一个空窗口。
            //
            // 注意此刻恢复是**异步**的（等 ConPTY 起来），所以「没有标签」不等于
            // 「恢复失败」—— 用 `layout_state.pending` / `prompt_open` 区分开。
            let needs_local = main
                .read_with(cx, |m, _| {
                    m.tabs_state.tabs.is_empty()
                        && m.tabs_state.layout_state.pending.is_none()
                        && !m.tabs_state.layout_state.prompt_open
                })
                .unwrap_or(false);
            if needs_local {
                let _ = main.update(cx, |m, cx| m.open_local_shell(cx));
            }
        }
        rustshell_core::settings::StartupAction::Nothing => {
            // 空窗口：不主动开任何东西。用户随时可 `Ctrl+T` / 命令面板。
        }
    }
}

// 竖栏右键菜单入口（，App 上下文）

pub fn ui_call_set_theme(mode: rustshell_core::theme::ThemeMode, cx: &mut App) {
    let Some(hub) = cx.try_global::<UiHub>() else {
        return;
    };
    let main = hub.main.clone();
    let _ = main.update(cx, |m, cx| m.set_theme_mode(mode, cx));
}

/// 状态栏/竖栏右键菜单「打开设置」入口。
pub fn ui_call_open_settings(w: &mut Window, cx: &mut App) {
    let Some(hub) = cx.try_global::<UiHub>() else {
        return;
    };
    let main = hub.main.clone();
    if main.update(cx, |m, cx| m.open_settings(w, cx)).is_err() {
        log::warn!("「打开设置」未能路由到 MainView（实体已释放）");
    }
}

pub fn ui_call_open_config(cx: &mut App) {
    let Some(hub) = cx.try_global::<UiHub>() else {
        return;
    };
    let main = hub.main.clone();
    let _ = main.update(cx, |m, cx| m.open_config_folder(cx));
}

// AI 面板辅助入口（气泡菜单 / 命令卡片，App 上下文）

pub fn ui_call_ai_whitelist(cmd: String, cx: &mut App) {
    let Some(hub) = cx.try_global::<UiHub>() else {
        return;
    };
    let main = hub.main.clone();
    let _ = main.update(cx, |m, cx| m.ai_add_whitelist(&cmd, cx));
}

pub fn ui_call_ai_regen(cx: &mut App) {
    let Some(hub) = cx.try_global::<UiHub>() else {
        return;
    };
    let main = hub.main.clone();
    let _ = main.update(cx, |m, cx| m.ai_regen(cx));
}

pub fn ui_call_ai_delete_msg(id: u64, cx: &mut App) {
    let Some(hub) = cx.try_global::<UiHub>() else {
        return;
    };
    let main = hub.main.clone();
    let _ = main.update(cx, |m, cx| m.ai_delete_msg(id, cx));
}

/// 挂「退出前把 AI 会话落盘」的钩子（shell 层启动时调一次）。
///
/// 为什么除了 `poll` 里的节流落盘还要这个钩子：流式期间最多每 3 秒才写一次，
/// 用户完全可能「刚看到答复就关窗」—— 那一轮的最后一段还在内存里。
/// `App::on_app_quit` 的处理器有 100ms 预算，AI 会话文件远小于这个量级。
///
/// 返回值就地 `detach()` 而不交给调用方：gpui 的 `Subscription` **一 drop 就退订**，
/// 调用处写成 `let _ = f(cx);` 就等于没挂上（而且不报错、不告警 —— 只有
/// 「关窗后还是丢历史」这一个现象）。把这件事封在函数里，调用方无从踩错。
pub fn ui_install_flush_ai_chats_on_quit(cx: &mut App) {
    cx.on_app_quit(|cx| {
        if let Some(hub) = cx.try_global::<UiHub>() {
            let main = hub.main.clone();
            // 两条数据线在同一个钩子里落盘。**都写**，一个失败不影响另一个：
            // AI 会话写不出来不该连带把命令历史也丢掉（反之亦然）。
            if main.update(cx, |m, _cx| m.ai_chats_flush_now()).is_err() {
                log::warn!("退出前落盘 AI 会话未路由到 MainView（实体已释放）");
            }
            if main.update(cx, |m, _cx| m.cmd_history_flush_now()).is_err() {
                log::warn!("退出前落盘命令历史未路由到 MainView（实体已释放）");
            }
        }
        async {}
    })
    .detach();
}

/// 命令历史桶装载的 `MainView` 侧入口（见 `completion_persist`）。
///
/// 在 `MainView::new` 里、**`layout_startup` 之后**调 —— 那一句可能已经建立了标签，
/// 而"当前会话那一桶"是按会话指纹取的（见 `current_history_key`）。
fn install_cmd_history_buckets(view: &mut MainView, loaded: completion_persist::LoadedHistory) {
    view.install_history_buckets(loaded);
}

/// 忽略某条回复上的某张命令卡片（按消息 id + 卡片下标定位，行索引会漂移）
pub fn ui_call_ai_dismiss_card(msg_id: u64, card_ix: usize, cx: &mut App) {
    let Some(hub) = cx.try_global::<UiHub>() else {
        return;
    };
    let main = hub.main.clone();
    let _ = main.update(cx, |m, cx| m.ai_card_dismiss(msg_id, card_ix, cx));
}

// 定时任务：菜单回调 → MainView。
//
// 与连接树/快捷命令同一手法（`ui_call_*` + `UiHub`）：菜单回调拿到的
// `&mut Window` 属于外层，用 `WeakEntity::update_in` 必定 Err 被静默吞掉
// （2026-09-10 N23 的根因），所以一律经全局 `UiHub` 路由回 `MainView`。

/// 立即执行任务。
pub fn ui_call_task_run_now(task_id: String, cx: &mut App) {
    let Some(hub) = cx.try_global::<UiHub>() else {
        return;
    };
    let main = hub.main.clone();
    let _ = main.update(cx, |m, cx| m.task_run_now(&task_id, cx));
}

/// 停止正在运行的任务。
pub fn ui_call_task_stop(task_id: String, cx: &mut App) {
    let Some(hub) = cx.try_global::<UiHub>() else {
        return;
    };
    let main = hub.main.clone();
    let _ = main.update(cx, |m, cx| m.task_stop(&task_id, cx));
}

/// 启用 / 停用任务。
pub fn ui_call_task_toggle(task_id: String, cx: &mut App) {
    let Some(hub) = cx.try_global::<UiHub>() else {
        return;
    };
    let main = hub.main.clone();
    let _ = main.update(cx, |m, cx| m.task_toggle_enabled(&task_id, cx));
}

/// 复制任务。
pub fn ui_call_task_duplicate(task_id: String, cx: &mut App) {
    let Some(hub) = cx.try_global::<UiHub>() else {
        return;
    };
    let main = hub.main.clone();
    let _ = main.update(cx, |m, cx| m.task_duplicate(&task_id, cx));
}

/// 删除任务。
pub fn ui_call_task_delete(task_id: String, cx: &mut App) {
    let Some(hub) = cx.try_global::<UiHub>() else {
        return;
    };
    let main = hub.main.clone();
    let _ = main.update(cx, |m, cx| m.task_delete(&task_id, cx));
}

/// 打开任务编辑对话框。需要 `Window`（`present_dialog` 要），故单独一条入口。
pub fn ui_call_task_edit(task_id: String, window: &mut Window, cx: &mut App) {
    let Some(hub) = cx.try_global::<UiHub>() else {
        return;
    };
    let main = hub.main.clone();
    let _ = main.update(cx, |m, cx| m.open_task_editor(&task_id, window, cx));
}

/// agent 确认卡片的两个动作（「执行」/「忽略」）。
pub fn ui_call_ai_confirm(approve: bool, cx: &mut App) {
    let Some(hub) = cx.try_global::<UiHub>() else {
        return;
    };
    let main = hub.main.clone();
    let _ = main.update(cx, |m, cx| m.ai_confirm(approve, cx));
}

// 主视图

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PanelKind {
    #[default]
    Dashboard,
    Connections,
    Terminals,
    Files,
    Ai,
    /// 定时任务与运维预警（左侧栏第 6 项）。
    Tasks,
}

impl PanelKind {
    pub const ALL: [PanelKind; 6] = [
        PanelKind::Dashboard,
        PanelKind::Connections,
        PanelKind::Terminals,
        PanelKind::Files,
        PanelKind::Ai,
        PanelKind::Tasks,
    ];

    pub fn label(self) -> &'static str {
        let zh = match self {
            PanelKind::Dashboard => rustshell_core::i18n::t("概览"),
            PanelKind::Connections => rustshell_core::i18n::t("连接"),
            PanelKind::Terminals => rustshell_core::i18n::t("终端"),
            PanelKind::Files => rustshell_core::i18n::t("文件"),
            PanelKind::Ai => "AI",
            PanelKind::Tasks => rustshell_core::i18n::t("定时"),
        };
        rustshell_core::i18n::t(zh)
    }

    /// 落盘用的**稳定键**：`Settings::sidebar_widths` 与面板组 keyed state 都以它为准。
    ///
    /// 刻意不复用 `label()` —— 那走 i18n，中文环境下是 `"连接"`、英文环境下是
    /// `"Connections"`；一旦拿它当键，用户切一次界面语言，拖出来的宽度就查不回来了。
    /// 同理这些字符串**不许改名**：改了等于把用户已存的宽度丢掉。
    pub fn key(self) -> &'static str {
        match self {
            PanelKind::Dashboard => "dashboard",
            PanelKind::Connections => "connections",
            PanelKind::Terminals => "terminals",
            PanelKind::Files => "files",
            PanelKind::Ai => "ai",
            PanelKind::Tasks => "tasks",
        }
    }
}

/// `main-split` 面板组的 id：**按「当前侧栏面板 + 哪几列在显示」分叉**。
///
/// 两个维度各有各的来历：面板维度来自 2026-09-20「每个侧边栏的宽度可以不同」，
/// 列可见性维度来自 2026-09-22 的终端全屏（推导见下面第二节）。
///
/// # 一、为什么要按面板分叉
///
/// 2026-09-20 用户要求「默认记住每个侧边栏的宽度，每个侧边栏的宽度可以不同」。
/// 关键事实（gpui-base `resizable/panel.rs` / `resizable/mod.rs`）：
///
/// - 面板组把各面板宽度存在 `window.use_keyed_state(self.id, ..)` 里，**id 就是键**；
/// - `resizable_panel().size(px(..))` 只是 `initial_size`，仅在
///   `ResizableState.panels[ix].size` 还是 `None` 时参与布局（`flex_basis(initial_size)`），
///   而 `update_panel_size` 在第一帧 prepaint 就把实测宽度写进 `panels[ix].size`；
/// - 于是**同一个 id 下改 `size(..)` 从第二帧起完全无效** —— 六个侧栏面板会永远
///   共用第一次拖出来的那个宽度。
///
/// 正解就是「换 key」：每个面板一个 keyed state，切换面板时新 state 的
/// `panels[0].size` 还是 `None` ⇒ 首帧就会用该面板自己记忆的宽度布局。
/// 这样不需要任何「程序化 resize」调用，也就没有 `Window` 时序问题。
///
/// 一个必须知道的**推论**：某个面板**正在显示**时改它的 `sidebar_widths` 值，
/// 画面不会当场跟着变 —— 那个 key 的 state 早已把宽度钉住了，要等离开该面板
/// （key 变了）再回来才生效。本应用里宽度只有「拖拽」一个来源，而拖拽本来就会
/// 同步改 state 本身，所以不构成问题；**但不要在渲染路径里「顺手」改宽度**，
/// 那种写法会静默失效。
///
/// # 二、为什么 key 里还要带「哪几列在显示」
///
/// 2026-09-22 加终端全屏时量出来的坑：面板组的 `panels: Vec<..>` / `sizes: Vec<..>`
/// 是**按位置**存的，而折叠侧栏、全屏都会让子节点个数变化。同一 id 下，
/// `sync_panels_count` 只会按个数截断/补位，**不会**跟着「少的是哪一列」走 ——
/// 于是侧栏一藏，终端就坐上了「第 0 块」的位子、继承侧栏记的宽度；AI 列同理继承
/// 终端的。更糟的是 gpui-base 在容器**宽度**变化时会按比例重算所有 slot
/// （`adjust_to_container_size`）：全屏时竖栏让位，容器从 1881 变 1920，那个已被
/// 张冠李戴的 slot 被当成「一块占满整宽的板子」重算成 1920；退出全屏时它被
/// 钳回侧栏上限 520 ⇒ 侧栏凭空变到最宽、终端被挤窄 130px（实测值）。
///
/// 让 key 带上**列数拓扑**就一次性断掉这条链：换拓扑 = 换 state，新 state 的
/// `panels[ix].size` 全是 `None` ⇒ 第一帧直接按 `resizable_panel().size(..)`
/// 布（侧栏=该面板记忆宽度、AI=记忆宽度、终端占满剩余），
/// 既不会有谁继承别人的宽度，也不会有脏值被比例重算。
/// 旧拓扑的 state 因为当帧没人访问，会被 `Window::finish` 丢弃，不会残留。
fn main_split_id(panel: PanelKind, sidebar_visible: bool, ai_visible: bool) -> ElementId {
    ElementId::Name(SharedString::from(format!(
        "main-split:{}:{}{}",
        panel.key(),
        if sidebar_visible { "s" } else { "-" },
        if ai_visible { "a" } else { "-" },
    )))
}

/// 内容区的「连接中 / 连接失败 / 已断开」占位卡片。
///
/// `bg` 由调用方经 `panel_bg` 算好传入：启用背景图时半透明透出背景图
/// （2026-09-18 反馈「背景图只替换了连接栏」—— 此前这里是硬编码不透明 `bg_base`）。
fn render_connect_placeholder(
    t: &rustshell_core::theme::ThemeTokens,
    bg: gpui::Hsla,
    title: &str,
    err: Option<&str>,
    started: Option<std::time::Instant>,
    kind: SessionKind,
    disconnected: bool,
) -> gpui::AnyElement {
    use crate::theme::to_hsla as hex;
    use gpui::Styled as _;

    let local = kind == SessionKind::Local;
    let failed = err.is_some();
    let (icon, tint, headline) = if failed {
        (
            IconName::TriangleAlert,
            hex(t.danger),
            rustshell_core::i18n::t("连接失败"),
        )
    } else if disconnected {
        // 断开态用「灰掉的本体图标」表达：本地 Shell / SSH 各自的身份图标 + muted 取色，
        // 不引入 danger 红（那是失败态），也不假装还在连接中。
        (
            if local {
                IconName::SquareTerminal
            } else {
                IconName::Globe
            },
            hex(t.fg_muted),
            rustshell_core::i18n::t("已断开"),
        )
    } else if local {
        (
            IconName::Loader,
            hex(t.accent),
            rustshell_core::i18n::t("正在启动本地 Shell…"),
        )
    } else {
        (
            IconName::Globe,
            hex(t.accent),
            rustshell_core::i18n::t("正在建立 SSH 连接…"),
        )
    };

    let mut card = v_flex()
        .size_full()
        .items_center()
        .justify_center()
        .gap_2()
        .bg(bg)
        .child(Icon::new(icon).text_color(tint))
        .child(
            div()
                .text_size(px(14.))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(hex(t.fg_primary))
                .child(SharedString::from(headline)),
        )
        .child(
            div()
                .text_size(px(12.))
                .text_color(hex(t.fg_muted))
                .child(SharedString::from(title.to_string())),
        );

    if let Some(e) = err {
        // 失败原因走官方`Alert`（error 变体）：此前是「自绘圆角卡 + danger 文字」，
        // 与全局错误条、对话框校验错误三处各画一套，配色/圆角/内边距互不相同。
        card = card.child(
            crate::notice::NoticeKind::Error
                .card("connect-failed-reason", SharedString::from(e.to_string()))
                .max_w(px(540.)),
        );
    } else if disconnected {
        // 断开后不显示「已等待 N 秒」，而是给出恢复入口（Tab 右键「连接」）
        card = card.child(div().text_size(px(11.5)).text_color(hex(t.fg_muted)).child(
            SharedString::from(rustshell_core::i18n::t("会话已终止，右键标签可重新连接")),
        ));
    } else if let Some(secs) = started.map(|s| s.elapsed().as_secs()) {
        card = card.child(div().text_size(px(11.5)).text_color(hex(t.fg_muted)).child(
            SharedString::from(rustshell_core::i18n::ts(
                format!("已等待 {secs} 秒"),
                format!("Waiting {secs}s"),
            )),
        ));
    }
    card.into_any_element()
}

/// 树节点 id（`grp:<分组 id>` / `prof:<连接 id>`）到分组 id。
///
/// 连接树的 `expanded_groups` 存的是**裸分组 id**，而 `TreeItem` 的节点 id 带 `grp:` 前缀；
/// `TreeEvent::Expanded/Collapsed` 带回的又是**节点 id**。不换算就会「收起写进 `grp:x`、
/// 展开查 `x`」——集合里恒有 `x`（启动时全部展开），于是分组看着永远展开、点节点也收不起来。
fn group_id_of_node(node_id: &str) -> Option<&str> {
    node_id.strip_prefix("grp:")
}

pub struct MainView {
    // 2026-10-12（开源评审 P0-4 刀三）：原先 175 个平级字段按功能域归入下面五个子结构。
    // 每个子结构的归属理由见 `ui_state.rs` 各自的头部文档。
    // 为什么归组：平铺时「哪些字段总是一起动」在字段名里完全看不出来，
    // 而连接相关状态要在一堆 `dlg_*` 之间翻才能找到。
    pub tabs_state: TabsState,
    pub conn_ui: ConnUiState,
    pub panel_ui: PanelUiState,
    pub ai_ui: AiUiState,
    pub io: DialogIoState,
    /// 终端全屏：竖栏 / 侧栏 / AI 对话列全部让位，终端卡片独占窗口内容区；
    /// 同时把窗口推入系统全屏（见 [`MainView::toggle_terminal_fullscreen`]）。
    ///
    /// 它**不改** `sidebar_collapsed` / `ai_chat_open`：那两个是用户自己的布局意图，
    /// 退出全屏时侧栏与 AI 列必须回到原样（见 render 里的 `sidebar_visible`）。
    pub terminal_fullscreen: bool,

    pub conn_store: ConnectionStore,

    pub quick_store: QuickCommandStore,

    pub settings: Settings,

    pub local_metrics: std::sync::Arc<MetricsStore>,

    dialog: Option<DialogMode>,

    /// 该提示的代次：连点时只有最后一次的定时器生效（同 `quick_toast_seq` 的口径）。
    ///
    /// 与 `quick_toast_seq` **分开**：两者寿命不同（4s / 2s），共用一个代次会让
    /// 短提示的定时器把长提示提前收走。
    pub no_session_notice_seq: u64,
}

impl Focusable for MainView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.panel_ui.focus.clone()
    }
}

impl MainView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        // ── 1. `UiHub` 全局句柄：引擎回调（跑在 `Context<TerminalView>` 里）回宿主的唯一入口。
        //    必须在任何实体创建**之前**就位 —— 子视图的构造里会立刻用它。
        let hub = UiHub {
            main: cx.entity().downgrade(),
        };
        cx.set_global(hub);

        // ── 2. 连接对话框 / 代理 / 快捷指令 的输入框实体（`DialogIoState` 的 `dlg_*` / `quick_*` 部分）。
        let dlg_name = cx.new(|cx| {
            InputState::new(window, cx).placeholder(rustshell_core::i18n::t("名称，如 prod-01"))
        });
        let dlg_host = cx.new(|cx| {
            InputState::new(window, cx).placeholder(rustshell_core::i18n::t("主机名 / IP"))
        });
        let dlg_port = cx.new(|cx| InputState::new(window, cx).placeholder("22"));
        let dlg_user =
            cx.new(|cx| InputState::new(window, cx).placeholder(rustshell_core::i18n::t("用户名")));
        // 口令输入：**掩码显示**（2026-09-18 运行反馈「连接-编辑，密码需要 *** 展示」）。
        // `masked(true)` 只影响绘制，`value()` 仍回真实文本（见 gpui-base
        // `InputBaseState::value`：直接返回 `self.text`），所以「编辑时回填原口令、
        // 留空即不改口令」这条既有语义一字不变；变化只是编辑对话框不再把口令
        // 明文摊在屏幕上（此前打开编辑框 = 屏幕共享时口令被读走）。
        let dlg_pass = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(rustshell_core::i18n::t("密码（留空则连接时询问）"))
                .masked(true)
        });
        // 私钥路径（明文，不是口令）：`~/.ssh/id_ed25519` 这类路径要能一眼看全，
        // 掩码显示只会让人没法确认自己选的是哪把钥匙。
        let dlg_key_path = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(rustshell_core::i18n::t("私钥路径，如 ~/.ssh/id_ed25519"))
        });
        // 备注：官方多行 Textarea（`TextareaState` 自带的模式就是「多行纯文本」，
        // 不需要额外开关）。初值 3 行、随内容长到 6 行 —— 备注常是一小段话，
        // 固定 3 行会让长备注只能滚动着看，自动生长则不至于把对话框撑爆。
        let dlg_notes = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder(rustshell_core::i18n::t(
                    "备注（可选，如用途、跳板机、到期日）",
                ))
                .auto_grow(3, 6)
        });
        // 代理（F-10）的四个输入框：与上面那批 dlg_* 同一条创建管线。
        // 口令掩码显示（与连接口令同一个处理）。
        let dlg_proxy_host = cx.new(|cx| InputState::new(window, cx).placeholder("127.0.0.1"));
        let dlg_proxy_port = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(rustshell_core::proxy::DEFAULT_PROXY_PORT.to_string())
        });
        let dlg_proxy_user = cx.new(|cx| {
            InputState::new(window, cx).placeholder(rustshell_core::i18n::t("代理用户名（可选）"))
        });
        let dlg_proxy_pass = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(rustshell_core::i18n::t("代理口令（可选）"))
                .masked(true)
        });
        // 分组只在下拉里选既有分组（引用分组 id）。新建/改名分组挪出本对话框，
        // 走连接树右键菜单 到 `DialogMode::NewGroup`：
        // 同一条输入管线里既选归属又建分组，会让人分不清「这次保存到底建了什么」。
        // 分组名输入框由 `GroupNameDialogView` 自带（见该类型注释）。
        let dlg_qc_label = cx.new(|cx| {
            InputState::new(window, cx).placeholder(rustshell_core::i18n::t("命令名称，如 build"))
        });
        let dlg_qc_script = cx.new(|cx| {
            // 官方 EditorState：代码编辑器（行号 / 自动缩进 / 多行 / 内置搜索）。
            // 不设 language，本工作区未启用 gpui-component 的 tree-sitter features，
            // 无高亮可用；普通纯文本编辑行为不受影响。
            EditorState::new(window, cx)
                .placeholder(rustshell_core::i18n::t("命令内容，如 make -j4（支持多行）"))
        });
        // 快捷命令栏（v3）：展开态搜索框与分组行内重命名输入框
        let conn_search = cx.new(|cx| {
            InputState::new(window, cx).placeholder(rustshell_core::i18n::t("搜索连接…"))
        });
        let quick_search = cx.new(|cx| {
            InputState::new(window, cx).placeholder(rustshell_core::i18n::t("搜索命令…"))
        });
        let quick_rename_input = cx.new(|cx| InputState::new(window, cx).placeholder(""));
        // 命令输入框：独立草稿，写完后可发全部 Tab / 全部终端
        let quick_compose_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(rustshell_core::i18n::t("输入命令，回车发送…"))
        });

        // 分组行内重命名输入（2026-09-14 修复「右键重命名无效」）：
        // 与`quick_renaming`配对，状态置位后在分组 Tab 位置渲染本输入框（见
        //`quick_commands_bar::render_expanded`），这里负责**提交时机**：
        // Enter 提交、失焦提交（空名在`commit_folder_rename`里视为取消）。
        cx.subscribe_in(
            &quick_rename_input,
            window,
            |m: &mut Self, _input, ev: &InputEvent, _w, cx| {
                if matches!(ev, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                    m.commit_folder_rename(cx);
                }
            },
        )
        .detach();

        // 命令输入框回车发送（2026-09-15 用户要求）：输入框常驻，
        // 回车即把草稿发到全部可投递终端（与右侧「发送到全部终端」按钮同一路径）。
        // 失焦不做任何处理 —— 输入框是常驻形态，不存在「切回搜索命令」的外观切换。
        cx.subscribe_in(
            &quick_compose_input,
            window,
            |m: &mut Self, _input, ev: &InputEvent, w, cx| {
                if matches!(ev, InputEvent::PressEnter { .. }) {
                    m.send_compose_to_all_tabs(w, cx);
                }
            },
        )
        .detach();

        // 2026-09-14：终端/性能/连接/AI 温度与上下文行数等数值项改由官方
        //`Settings`的`SettingField::number_input`直接读写`settings`/`ai_cfg`，
        // 原先 8 个仅供设置面板用的 InputState 字段（set_font_family 等）已删除。

        // ── 3. 启动时从磁盘恢复的配置与历史：AI 配置 / AI 会话 / 命令历史桶。
        //    读不到一律回落到「空白会话 / 空桶」，本函数后续初始化一字不变。
        let ai_cfg = rustshell_core::ai::AiConfig::load(&paths::ai_config_path());
        // 思考模式开关是配置的一部分（面板 Toggle 直接读写并落盘），开局与配置同源。
        let ai_thinking = ai_cfg.thinking;
        // AI 会话：从 `ai_chats.json` 恢复（2026-09-23「AI对话需要保存」）。
        // 读不到 / 读坏了都回落到「一条空白会话」，本函数接下来的初始化一字不变。
        let ai_history = ai_persist::load_for_startup();
        // 命令历史：从 `cmd_history.json` 恢复（遗留项 1），按连接指纹分桶（遗留项 2）。
        // ⚠️ 真正的装载在 `layout_startup` 之后（`install_cmd_history_buckets`）——
        // 那时才知道"当前会话"是哪一个。这里只把 map 取出来，其余留给那一步。
        let cmd_history_buckets = completion_persist::load_for_startup().buckets;
        // 官方 MessageScroller 的滚动状态（每次 ai_msgs 增删改都成对更新）。
        // 观察它是为了「跳到最新」按钮的显隐切换能触发面板重绘（组件内部只在滚动
        // 处理器里 notify 自己，父视图不会自动跟着重绘）。
        //
        // 行数**必须**用恢复回来的条数：这份状态自带虚拟列表的行数，
        // 从 0 起而磁盘上有 N 条消息的话，面板会认为一行都没有（历史恢复了却看不见）。
        // ── 4. AI 面板的输入框与滚动状态（`AiUiState` 的 `ai_*`）。
        //    `MessageScroller` 的行数**必须**用恢复回来的条数，从 0 起会让
        let ai_scroller = cx.new(|cx| {
            gpui_component::message_scroller::MessageScrollerState::new(
                ai_history.current_msgs_len(),
                cx,
            )
        });
        // 归属：只有 AI 对话列读消息滚动条状态。这是 AI 流式期间**最高频**的
        // notify 来源（每个增量都要 remeasure），所以它是 P0-3 收益最大的一处。
        cx.observe(&ai_scroller, |m: &mut Self, _scroller, cx| {
            m.notify_view(ChildView::AiChat, cx);
        })
        .detach();
        let ai_cfg_base = cx.new(|cx| {
            InputState::new(window, cx).placeholder(rustshell_core::i18n::t(
                "API 端点，如 https://api.deepseek.com/v1",
            ))
        });
        ai_cfg_base.update(cx, |s, cx| s.set_value(ai_cfg.base_url.clone(), window, cx));
        let ai_cfg_model = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(rustshell_core::i18n::t("模型名，如 deepseek-flash"))
        });
        ai_cfg_model.update(cx, |s, cx| s.set_value(ai_cfg.model.clone(), window, cx));
        let ai_cfg_key = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(rustshell_core::i18n::t("API Key（加密保存，不回显）"))
                // 掩码显示（2026-09-18 运行反馈「api key 也改为 *** 展示」）：
                // 与连接密码同一手法 —— 只影响绘制，`value()` 仍返回真实文本，
                // 因此「输入 → 加密落盘」这条通路一字未改，变化只是屏幕上不再
                // 明文摊着 Key（截图 / 屏幕共享 / 演示时最容易漏出去的东西）。
                .masked(true)
        });
        // 「免确认白名单」的添加框（设置 - Agent 页的白名单配置框）。与上面三个 AI
        // 输入框不同，它不承载配置值：加完就清空，白名单本体在 `ai_cfg.auto_approve` 里。
        let ai_cfg_whitelist = cx.new(|cx| {
            InputState::new(window, cx).placeholder(rustshell_core::i18n::t(
                "输入完整命令，如 systemctl restart nginx",
            ))
        });
        // 白名单配置框的搜索框（2026-09-22「支持搜索命令」）：纯界面筛选条件。
        let ai_wl_search = cx
            .new(|cx| InputState::new(window, cx).placeholder(rustshell_core::i18n::t("搜索命令")));
        // 白名单配置框的名单本体（2026-09-23「改为 List 组件」）。`ListState` 必须
        // **跨帧复用**：滚动位置与行缓存都在它手里，逐帧新建等于用户每滚一格就被
        // 拽回顶部。委托体只拿一个**弱**引用回主视图 —— 强引用会成环
        // （`MainView` → `ListState` → 委托体 → `MainView`）。
        let wl_list_owner = cx.entity().downgrade();
        let ai_wl_list = cx.new(|cx| {
            ListState::new(
                whitelist_dialog::WhitelistDelegate::new(wl_list_owner),
                window,
                cx,
            )
            // 名单是「逐行开关 + 删除」的编辑面，没有「选中某一行」的语义。
            .selectable(false)
        });
        // AI 输入框：**多行**（2026-09-20 运行反馈「AI助手 输入栏可以上下调整大小」）。
        //
        // 从 `InputState` 换成 `TextareaState` 是这次改动的前提：单行输入框的高度写死
        // 一行，把输入栏拖高只会多出一截空白。`submit_on_enter(true)` 保住原有 Enter
        // 语义 —— **Enter 发送、Shift+Enter 换行**（gpui-base 的
        // `InputEvent::PressEnter { shift }`，订阅见下面那条 `subscribe_in`）。
        let ai_input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder(rustshell_core::i18n::t("问 AI…（自动附终端上下文）"))
                .submit_on_enter(true)
        });
        // 采样温度滑块（2026-09-18 运行反馈「采样温度改为滑动轴，0.1 一个间隔」）。
        // 状态建在这里而不是设置面板里逐帧新建：拖动是**跨多帧的连续手势**，
        // 每帧重建状态等于每次重绘都把手按着的滑块拽回初值（见 `ai_temp_slider` 字段注释）。
        let ai_temp_slider = cx.new(|_| {
            SliderState::new()
                .min(rustshell_core::ai::TEMPERATURE_MIN)
                .max(rustshell_core::ai::TEMPERATURE_MAX)
                .step(TEMPERATURE_STEP)
                .default_value(ai_cfg.temperature.clamp(
                    rustshell_core::ai::TEMPERATURE_MIN,
                    rustshell_core::ai::TEMPERATURE_MAX,
                ))
        });
        // 拖动即写 `ai_cfg.temperature`，与旧数字框语义完全一致：面板上改的就是配置里的值，
        // 落盘仍由设置面板的「保存」决定，取消则整体回滚到快照（`revert_settings` 顺带把
        // 滑块位置也拨回去，见 `prefill_ai_inputs`）。
        //
        // 听 `Change` 而不是 `Release`：滑块旁边那个数值标签要**跟着手指走**，
        // 只在松手时更新会让人以为控件坏了。写值幂等，连续同值帧不会触发任何额外工作。
        cx.subscribe(
            &ai_temp_slider,
            |m, _state: gpui::Entity<SliderState>, ev: &SliderEvent, cx| {
                let SliderEvent::Change(v) = ev else {
                    return; // Release 与 Change 同值，不必再走一遍
                };
                let next = v.end().clamp(
                    rustshell_core::ai::TEMPERATURE_MIN,
                    rustshell_core::ai::TEMPERATURE_MAX,
                );
                if (next - m.ai_ui.ai_cfg.temperature).abs() > f32::EPSILON {
                    m.ai_ui.ai_cfg.temperature = next;
                    cx.notify();
                }
            },
        )
        .detach();
        // ── 5. 其余各面板的输入框：终端搜索 / 任务搜索 / 文件路径 / SMTP 口令 / 代理口令 /
        //    任务编辑对话框（分属 `PanelUiState` 与 `DialogIoState`）。
        let term_search_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(rustshell_core::i18n::t("搜索终端…"))
        });
        // 任务面板搜索框（按名称过滤）。与连接树搜索框同一约定：
        // 过滤在渲染时读当前值，订阅 Change 只是为了让每一键都重绘。
        let task_search = cx.new(|cx| {
            InputState::new(window, cx).placeholder(rustshell_core::i18n::t("搜索任务…"))
        });
        cx.subscribe_in(
            &task_search,
            window,
            |m: &mut Self, _input, ev: &InputEvent, _w, cx| {
                if matches!(ev, InputEvent::Change) {
                    // 归属：任务列表在侧栏
                    m.notify_view(ChildView::Sidebar, cx);
                }
            },
        )
        .detach();
        // 文件面板路径栏：可直接输入远端绝对路径（如 /var/log）回车跳转。
        // 初值取 `DEFAULT_REMOTE_PATH`（= 兜底 `/`）：这只是首个 `Listing` 到达前的
        // 占位 —— 真正的默认落点是**远端登录用户的家目录**，面板一打开就被首扫
        // 回报的绝对路径覆盖（`SftpCmd::ListHome` 在 `terminal_actions.rs`）。
        // 这里**必须**走常量：口径已在同一天内反转过两次（`/` → `~` → `/` → 家目录），
        // 就地写死一个字面量正是让两处（字段初值 / 输入框回填）分叉的原因 ——
        // 上一次改口径就漏掉了这一行，面板打开的瞬间又跳回家目录。
        // 空串会让输入框看起来「没值」，所以不能给 ""。
        let files_path_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(rustshell_core::i18n::ts(
                "远端路径（回车跳转，~ = 家目录）".to_string(),
                "Remote path (Enter to go, ~ = home)".to_string(),
            ))
        });
        files_path_input.update(cx, |s, cx| {
            s.set_value(
                rustshell_core::sftp::DEFAULT_REMOTE_PATH.to_string(),
                window,
                cx,
            )
        });

        // 连接树搜索框：每次输入都重算可见树（过滤决策在 core 的纯函数里），
        // 不订阅就只有等下一次别的重绘才会生效 —— 表现为「打了字树不动」。
        cx.subscribe_in(
            &conn_search,
            window,
            |m: &mut Self, _input, ev: &InputEvent, _w, cx| {
                if matches!(ev, InputEvent::Change) {
                    // 归属：连接树在侧栏
                    m.notify_view(ChildView::Sidebar, cx);
                }
            },
        )
        .detach();

        // 终端搜索输入：Change 走防抖（打字期间不逐键扫屏），Enter/Shift+Enter 立即跳转
        cx.subscribe_in(
            &term_search_input,
            window,
            |m: &mut Self, _input, ev: &InputEvent, w, cx| match ev {
                InputEvent::Change => {
                    // 归属：搜索结果只有浮层自己读
                    m.schedule_term_search(cx);
                    m.notify_view(ChildView::TermSearch, cx);
                }
                InputEvent::PressEnter { shift, .. } => {
                    m.term_search_jump(!shift, cx);
                    let _ = w;
                }
                _ => {}
            },
        )
        .detach();

        // AI 输入框：回车即发送（与「发送」按钮同一入口）。
        // 此前只能点按钮，键盘用户停在输入框里回车没有任何反应。
        cx.subscribe_in(
            &ai_input,
            window,
            |m: &mut Self, _input, ev: &InputEvent, w, cx| {
                if matches!(ev, InputEvent::PressEnter { .. }) {
                    m.ai_send(w, cx);
                }
            },
        )
        .detach();

        // 文件面板路径栏：手输（Change）时先不打回填（别打断输入），
        // 回车即按输入的路径重扫目录；失焦则把输入框收回「当前目录」——
        // 2026-09-17 运行反馈「输入路径后，点击别的地方需要有失去焦点」：
        // 输入框原先一直攥着焦点（也不会把草稿还原），点别的面板后焦点仍停在路径栏，
        // 键盘输入继续往里灌。失焦时同步回当前真实目录，界面与状态就不会分叉。
        cx.subscribe_in(
            &files_path_input,
            window,
            |m: &mut Self, _input, ev: &InputEvent, w, cx| match ev {
                InputEvent::Change => {
                    m.io.files_path_typed = true;
                }
                InputEvent::PressEnter { .. } => m.files_goto_typed_path(cx),
                InputEvent::Blur => m.files_path_blur(w, cx),
                _ => {}
            },
        )
        .detach();

        // 失焦降频（，10 节 监控开销 <1% CPU）：窗口失活时本地采样降至 5s，激活恢复
        cx.observe_window_activation(window, |m, window, cx| {
            m.sync_metrics_focus(window);
            cx.notify();
        })
        .detach();

        // ── 6. 从磁盘读的应用级仓库与设置（连接表 / 快捷指令表 / 设置快照 / 指标缓冲）。
        //    这些**不进**任何 UI 子结构（刀三的 `ROOT` 归组）——它们是只读仓库，不是交互状态。
        let conn_path = paths::connections_path();
        let mut conn_store = ConnectionStore::load(&conn_path);
        // 恢复已保存的口令（DPAPI 密文 到 运行时明文）
        conn_store.restore_passwords(rustshell_platform::secret::unprotect);

        // 定时任务 / 告警规则 / SMTP：与 settings 同一套「load 失败回退默认」口径。
        // 口令恢复必须**在**读连接之后立刻做（上面的 restore_passwords）——
        // 定时任务的无人值守连接依赖它，缺了就会全部报「未保存口令」。
        let task_store = rustshell_core::tasks::TaskStore::load(&paths::tasks_path());
        let alert_store = rustshell_core::alerts::AlertStore::load(&paths::alert_rules_path());
        let smtp_cfg = rustshell_core::notify_smtp::SmtpConfig::load(&paths::smtp_path());
        let smtp_cfg_pass = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(rustshell_core::i18n::t("SMTP 口令（加密保存，不回显）"))
                .masked(true)
        });
        // 全局代理（F-10）的口令框：与 SMTP 同一口径（掩码 + 不回显）。
        // 面板里**不得**出现 `proxy.password_ref`（源码护栏见 settings_panel 的测试）。
        let proxy_cfg_pass = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(rustshell_core::i18n::t("代理口令（加密保存，不回显）"))
                .masked(true)
        });

        // 任务编辑对话框的输入框。与快捷命令对话框同一批构造方式：
        // 状态挂在 `MainView` 上（逐帧重建会丢草稿与光标）。
        // 字段只剩两个输入：cron 时间（单行）与「AI 要求」（多行）。
        let dlg_task_name = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(rustshell_core::i18n::t("任务名称（留空则取 AI 要求首行）"))
        });
        let dlg_task_cron = cx.new(|cx| InputState::new(window, cx).placeholder("0 9 * * *"));
        // 提示里点明「首行会成为任务名」：名称字段已撤下，用户得知道
        // 列表里那行字是从哪儿来的。
        // 高度交给组件按内容自适应（2–10 行）：用户明确不要拖拽手柄那份实现。
        // 注意别在渲染处再给固定高度 —— 两者会打架。
        let dlg_task_prompt = cx.new(|cx| {
            gpui_component::input::TextareaState::new(window, cx)
                .auto_grow(2, 10)
                .placeholder(rustshell_core::i18n::t("用一句话说明要巡检什么"))
        });

        let quick_path = paths::quick_commands_path();
        let quick_store = QuickCommandStore::load(&quick_path);
        let settings = Settings::load(&paths::settings_path());
        // 终端行高滑块（2026-09-28）：与采样温度同一个模式 —— 状态持久、
        // 拖动即写 `settings.terminal_line_height`、落盘仍由「保存」决定。
        // 行高改动要**即时推给已开的终端**（`apply_terminal_palette`），否则拖动时
        // 数值在变、画面不动，用户会以为没生效。
        //
        // 必须建在 `settings` 载入**之后**：滑块的起始位置就是用户上次存的行高。
        // ── 7. 通用滑块 / 连接树状态（`PanelUiState` 的 `line_height_slider` / `tree_state`）。
        let line_height_slider = cx.new(|_| {
            SliderState::new()
                .min(rustshell_core::settings::TERMINAL_LINE_HEIGHT_MIN)
                .max(rustshell_core::settings::TERMINAL_LINE_HEIGHT_MAX)
                .step(LINE_HEIGHT_STEP)
                .default_value(settings.clamped_line_height())
        });
        cx.subscribe(
            &line_height_slider,
            |m, _state: gpui::Entity<SliderState>, ev: &SliderEvent, cx| {
                let SliderEvent::Change(v) = ev else {
                    return; // Release 与 Change 同值，不必再走一遍
                };
                let next = v.end().clamp(
                    rustshell_core::settings::TERMINAL_LINE_HEIGHT_MIN,
                    rustshell_core::settings::TERMINAL_LINE_HEIGHT_MAX,
                );
                if (next - m.settings.terminal_line_height).abs() > f32::EPSILON {
                    m.settings.terminal_line_height = next;
                    // 拖动过程中就把行高推给已开的终端（不涉及 PTY，安全热更新）
                    m.apply_terminal_palette(cx);
                }
            },
        )
        .detach();
        // AI 对话面板的开合是持久状态。`settings` 稍后会被整体 move 进 `Self`，
        // 所以先取出来（字段与 settings 里的同名，直接引用会借用已 move 的结构）。
        let ai_chat_open_wanted = settings.ai_chat_open;

        let local_metrics = std::sync::Arc::new(MetricsStore::new());
        let metrics_interval = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(
            settings.sample_interval_ms,
        ));
        rustshell_platform::metrics_local::spawn_local_metrics(
            local_metrics.clone(),
            metrics_interval.clone(),
        );

        // 连接树官方状态实体：展开/折叠事件回流到 expanded_groups（供持久化与重建）。
        // 事件携带的是**节点 id**（`grp:<分组 id>`），而集合存的是裸分组 id，
        // 故必须先过 `group_id_of_node` 换算，理由见该函数注释。
        let tree_state = cx.new(|cx| gpui_component::tree::TreeState::new(cx));
        cx.subscribe(
            &tree_state,
            |m: &mut Self, _ts, ev: &gpui_component::tree::TreeEvent, cx| {
                match ev {
                    gpui_component::tree::TreeEvent::Expanded(id) => {
                        if let Some(gid) = group_id_of_node(id) {
                            m.conn_ui.expanded_groups.insert(gid.to_string());
                        }
                    }
                    gpui_component::tree::TreeEvent::Collapsed(id) => {
                        if let Some(gid) = group_id_of_node(id) {
                            m.conn_ui.expanded_groups.remove(gid);
                        }
                    }
                }
                // 展开/折叠会让可见条目**整体位移**，而库的选中态是按下标存的
                // （`TreeState::selected_ix`）：不重建并按连接 id 对齐，选中框就会
                // 停在位移后落到同一个下标上的**另一条连接**上（2026-09-18 反馈
                // 「选中框外线有时候一直停留在那，切换了也没用」的另一半原因）。
                // 重建落在下一帧 render（`set_items` 不能在树自己的事件回调里重入）。
                m.invalidate_conn_tree(cx);
            },
        )
        .detach();

        // 侧栏面板视图要在 MainView 实体建好之后创建（它 observe 父视图）
        let me = cx.entity();
        let sidebar_panel = cx.new(|cx| SidebarPanelView::new(me.clone(), cx));
        let ai_chat_panel = cx.new(|cx| AiChatPanelView::new(me.clone(), cx));
        let term_search_bar = cx.new(|cx| TermSearchBarView::new(me.clone(), cx));
        let completion_bar = cx.new(|cx| completion_bar::CompletionBarView::new(me.clone(), cx));
        let group_name_body = {
            let me = me.clone();
            cx.new(|cx| GroupNameDialogView::new(me, window, cx))
        };

        // ── 8. ★ 结构体字面量：把上面创建的全部实体按刀三的归组装进五个子结构。
        //    这里之后不再创建实体，只有「收尾」：命令历史桶装载、轮询任务、聚焦。
        let mut view = Self {
            tabs_state: TabsState {
                // 开局一律干净：待恢复流程 / 脏标记 / 提示框状态都由
                // `layout_startup`（本函数末尾）按快照与设置决定。
                layout_state: layout_persist::LayoutState::default(),
                tabs: Vec::new(),
                active: None,
                tab_scroll: crate::tab_bar::TabScroll::new(gpui::ScrollHandle::new()),
                tab_pan_x: None,
                tab_dots: std::collections::HashMap::new(),
                // 分屏态：启动时未分屏；`pending_split` 是「本地 Shell 异步创建期间
                // 待建立的分屏」标记，启动时必然为空。
                split: None,
                pending_split: None,
                pending_focus: false,
            },
            conn_ui: ConnUiState {
                expanded_groups: HashSet::new(),
                conn_tree_key: None,
                tree_state,
                selected_profile: None,
                connecting_profiles: HashSet::new(),
                last_click_profile: None,
                connect_error: None,
                local_starting: false,
                connect_notice: None,
                openssh_plans: Vec::new(),
                openssh_selected: Vec::new(),
                openssh_warnings: Vec::new(),
                openssh_source: String::new(),
                openssh_pick_sync: None,
                openssh_dialog_open: false,
                conn_search,
                pending_trust: None,
                trust_dialog_open: false,
            },
            panel_ui: PanelUiState {
                focus: cx.focus_handle(),
                panel: PanelKind::Connections,
                sidebar_panel,
                view_epoch: ViewEpoch::default(),
                sidebar_collapsed: false,
                bg_source: None,
                metrics_interval,
                palette_state: None,
                pending_palette: false,
                palette_cmds: Vec::new(),
                // 启动就建一份键位快照：空窗格的「快捷键提示」表要显示**生效**键位
                // （含用户在设置里改过的绑定），而空态是应用刚起来、最可能被看到的画面，
                // 不能等「用户打开设置」触发 `snapshot_rows()` 才有值。一次小文件读，
                // 冷启动代价可忽略；后续重绑定路径（keybind_actions）会自行刷新。
                keymap_rows: crate::keybinds::snapshot_rows(),
                keymap_recording: None,
                keymap_capture: None,
                keymap_hint: None,
                term_search_open: false,
                term_search_last: String::new(),
                term_search_input,
                term_search_bar,
                term_search_debounce: None,
                // 开局浮层必然是关的：会话都还没有，没有「当前行」可补
                completion: completion_bar::CompletionState::default(),
                completion_bar,
                // 历史从 `cmd_history.json` 恢复（遗留项 1），并按连接指纹分桶（遗留项 2）。
                // 装载时把**当前会话**那一桶取出来放进 `completion_history`；
                // 不这么做的话启动后第一条命令从一个空桶开始记，磁盘上那份要等切标签才被看见。
                // ⚠️ 这里给的是**空的**占位：真正的桶要在 `layout_startup` 之后装
                // （那时才知道当前会话是哪一个，见 `install_cmd_history_buckets`）。
                completion_history: rustshell_core::cmdhistory::CommandHistory::new(),
                cmd_history_buckets,
                // 刚读进来的东西不是「改动」，开局一律干净
                cmd_history_dirty: false,
                cmd_history_save_at_ms: 0,
                line_height_slider,
            },
            ai_ui: AiUiState {
                quick_path,
                quick_targets: Vec::new(),
                quick_search,
                quick_toast: None,
                quick_toast_kind: crate::notice::NoticeKind::Warning,
                quick_no_session_alert: false,
                quick_renaming: None,
                quick_rename_input,
                quick_active_folder: None,
                quick_compose_input,
                quick_pending_remove: None,
                quick_last_cmd: None,
                quick_toast_seq: 0,
                ai_cfg,
                // 会话来自 `ai_chats.json`（没聊过 / 读不到时就是一条空白会话：
                // 不变量「列表恒非空」由 `ai_persist::load` 兜住），id 从 1 起、0 是哨兵
                ai_chats: ai_history.chats,
                ai_chat_id: ai_history.current,
                ai_next_chat_id: ai_history.next_chat_id,
                // 刚读进来的东西不是「改动」，开局一律干净
                ai_chats_dirty: false,
                ai_chats_save_at_ms: 0,
                ai_chat_open: ai_chat_open_wanted,
                ai_chat_panel,
                ai_scroller,
                ai_busy: false,
                ai_error: None,
                ai_hint: None,
                ai_cfg_open: false,
                ai_pending: None,
                ai_stream_open: false,
                ai_thinking,
                ai_cancel: None,
                ai_agent: None,
                ai_think_expanded: HashSet::new(),
                ai_trace_expanded: HashSet::new(),
                // 定时任务的运行 → 会话对应表也随会话一起存：不存的话，重启后
                // 点一次旧运行的「会话」就会**再建一条**同名会话（见 `ai_persist::normalize`
                // 的悬空剔除）。
                ai_run_chats: ai_history.run_chats,
                // 调度器还没起（懒建）：基线在 `scheduler()` 里落到真正的启动时刻
                ai_runs_baseline_at: 0,
                ai_input,
                // 输入栏高度：默认约三行文本（面板里那根分界线可拖拽改它）
                ai_composer_h: crate::ai_panel::AI_COMPOSER_H,
                ai_cfg_base,
                ai_cfg_model,
                ai_cfg_key,
                ai_cfg_whitelist,
                ai_wl_search,
                ai_wl_tab: 0,
                ai_wl_list,
                ai_temp_slider,
            },
            io: DialogIoState {
                dlg_name,
                dlg_host,
                dlg_port,
                dlg_user,
                dlg_pass,
                dlg_notes,
                dlg_forwards: Vec::new(),
                dlg_session_log: None,
                dlg_auto_reconnect: None,
                dlg_proxy_mode: rustshell_core::proxy::ProxyMode::FollowGlobal,
                dlg_proxy_kind: rustshell_core::proxy::ProxyKind::Http,
                dlg_proxy_host,
                dlg_proxy_port,
                dlg_proxy_user,
                dlg_proxy_pass,
                dlg_proxy_prev: None,
                dlg_key_path,
                dlg_key_pick: None,
                dlg_test: None,
                dlg_testing: false,
                dlg_group_id: None,
                dlg_group_after: None,
                dlg_group_parent: None,
                dlg_error: None,
                dlg_connect_after: false,
                set_drag_offset: (0.0, 0.0),
                set_drag_from: None,
                dlg_qc_label,
                dlg_qc_script,
                dlg_qc_newline: true,
                set_hint: None,
                set_hint_ok: false,
                set_snapshot: None,
                files_path: rustshell_core::sftp::DEFAULT_REMOTE_PATH.into(),
                files_path_input,
                files_path_typed: false,
                files_path_sync: None,
                files_entries: Vec::new(),
                files_rows: std::rc::Rc::new(Vec::new()),
                files_loading: false,
                files_error: None,
                files_transfers: Vec::new(),
                files_loaded_session: None,
                group_name_body,
                // 定时任务：定义与运行记录从磁盘读，不在这里起调度线程 ——
                // 调度线程首次显示任务面板（或立即执行）时才懒建，避免只想用终端的
                // 用户白白多一个每秒醒一次的线程。
                task_store,
                alert_store,
                smtp_cfg,
                smtp_testing: false,
                smtp_cfg_pass,
                proxy_cfg_pass,
                proxy_cfg_prev: None,
                scheduler: None,
                task_expanded: None,
                task_search,
                task_toast: None,
                task_toast_seq: 0,
                tasks_last_tick: 0,
                task_draft: None,
                task_target_kind: 0,
                dlg_task_name,
                dlg_task_cron,
                dlg_task_is_new: false,
                dlg_task_group_sel: None,
                dlg_task_profiles_sel: Vec::new(),
                dlg_task_prompt,
                dlg_task_notify: (true, true, false),
                task_dlg_error: None,
            },
            terminal_fullscreen: false,
            conn_store,
            quick_store,
            settings,
            local_metrics,
            dialog: None,
            no_session_notice_seq: 0,
        };
        // 展开态存的是分组 id（与 TreeItem 节点 id 同源）
        for g in view.conn_store.groups.clone() {
            view.conn_ui.expanded_groups.insert(g.id);
        }
        // 背景图缓存预热：渲染路径只读缓存，文件系统检查集中在这里
        // （以及设置面板打开/切换背景/手动刷新时）。
        view.refresh_background();

        // F-9 会话恢复：2026-09-29 起**启动不再自动恢复、也不再自动弹框**
        // （用户要求「取消恢复上次会话功能」）。这里只调 `layout_startup` 读一眼
        // 快照并记日志，快照**不设 `pending`** —— 于是第一轮 poll 就会拿当前
        // （空）布局把它覆盖掉，与原先 `Never` 档位一致。恢复改由用户手动触发
        // （`layout_restore_now`）。
        view.layout_startup(cx);

        // 命令历史桶装载（遗留项 1 + 2）：必须在 `layout_startup` **之后** ——
        // 那一句可能已经建立了标签（并 resolve 出会话指纹），
        // 而"当前会话那一桶"正是按这个键取的。
        // 放在 `Self { .. }` 里会恒落到本机桶：那时 `active` 还是 `None`。
        let startup_buckets = std::mem::take(&mut view.panel_ui.cmd_history_buckets);
        install_cmd_history_buckets(
            &mut view,
            completion_persist::LoadedHistory {
                buckets: startup_buckets,
            },
        );

        // 轮询任务：会话事件 / 连接结果 / 状态刷新
        //
        // ★ `poll` 返回「本轮是否真的改了状态」，只有改了才 `cx.notify()`。
        // 旧实现无条件 `cx.notify()`：`MainView` 是一棵覆盖全窗口的大树
        // （标题栏 / 侧栏 / 标签栏 / 终端 / 快捷命令栏 / 状态栏），于是**空闲时**
        // 也每秒重建 3.33 次整棵树，永远不会真正静止。
        // 心跳（watchdog::beat）仍在 `poll` 里每轮执行，与「是否重绘」解耦。
        cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(300))
                .await;
            if this
                .update(cx, |m, cx| {
                    if m.poll(cx) {
                        cx.notify();
                    }
                })
                .is_err()
            {
                break;
            }
        })
        .detach();

        window.focus(&view.panel_ui.focus, cx);
        view
    }

    /// 归属已知的 notify（P0-3）：只有点名的子视图会重绘。
    ///
    /// 语义与约束见 [`ViewEpoch`] —— 简单说：**改了谁读的数据就点谁**；
    /// 拿不准就用裸 `cx.notify()`（那是安全兜底，三个子视图都会重绘）。
    /// 这个方法**自己发 notify**，不要在同一个 update 里再补一次裸 `cx.notify()`。
    pub(super) fn notify_view(&mut self, view: ChildView, cx: &mut Context<Self>) {
        self.panel_ui
            .view_epoch
            .merge_scoped(ViewDirty::default().set(view));
        cx.notify();
    }

    /// 轮询一轮：会话事件 / 连接结果 / 状态刷新。
    ///
    /// 返回**本轮是否真的改变了需要重绘的状态**。调用方据此决定要不要 `cx.notify()`：
    /// 旧实现无条件重绘，空闲时也会每秒重建 3.33 次整棵窗口树。
    ///
    /// 心跳（`watchdog::beat`）与阶段打点**不受本返回值影响**，仍然每轮执行 ——
    /// 看门狗要的是「UI 线程还活着」，与「要不要重绘」是两件事。
    fn poll(&mut self, cx: &mut Context<Self>) -> bool {
        // 本轮是否产生了需要重绘的变化
        let mut changed = false;
        // 本轮变更**归属**哪几个独立子视图（P0-3，见 `ViewEpoch`）。
        //
        // 规则：能确定归属就 `set(...)`；确定不了就**不标**（空掩码 ⇒ 没有归属
        // ⇒ 三个子视图全部重绘，与改造前一致）。漏标不会出错，标错才会 ——
        // 所以这里一律按「这个字段谁在读」判断，不按改动来源判断。
        let mut dirty = ViewDirty::default();
        // 心跳 + 阶段打点：UI 线程一旦被卡住，看门狗能直接报出「停在哪个阶段、卡了多久」
        crate::watchdog::beat();
        crate::watchdog::enter(crate::watchdog::Phase::Poll);
        let poll_start = std::time::Instant::now();
        // 0) T-2：消费滚动条拖拽产生的待应用偏移。
        // 主路径在渲染里（`split_actions::render_term_pane`，见那里的「跟手」注释）：
        // 拖拽要跟手就必须在产生目标的那一帧落地。这里只兜底 —— 目标所属的标签页
        // 不在屏上（不重绘）时，偏移不会一直挂着，等它下次上屏就已经是最终位置。
        // `ScrollbarHandle::set_offset`只给`&self`（无`&mut App`），故拖拽时
        // 只暂存目标值，这里（持有`&mut Context`）才真正写进终端。
        for tab in &self.tabs_state.tabs {
            let Some(handle) = tab.scroll.clone() else {
                continue;
            };
            let Some(term) = tab.terminal.clone() else {
                continue;
            };
            let applied = handle.apply_pending_with(cx, &term);
            if applied {
                changed = true;
                // 归属不标：这条兜底路径只服务「不在屏上的标签页」，
                // 三个子视图读的东西一个都没变，但标错方向的代价更大 ——
                // 空掩码会退化成「三个都重绘」，正是改造前的行为。
            }
        }
        // 1) 连接结果
        while let Ok(res) = connect_results().1.try_recv() {
            changed = true;
            match res {
                Ok(mut session) => {
                    let id = session.id.clone();
                    // ★ C-4（2026-09-28）：**成功路径也必须清「连接中」标记**。
                    //
                    // 此前只有失败分支（下面 `Err` 那支）清，成功分支不清 —— 于是
                    // 连上以后 `connecting_profiles` 里永远留着这个 profile：
                    // ① 侧栏那颗「连接中」转圈从此不灭；
                    // ② 加了 C-4 的在途守卫之后，这个 profile 再也无法重连
                    //    （守卫看到「已有在途」就静默 return）。
                    // 标记的语义是「有一条连接线程在跑」，不是「连接成功过」。
                    if let Some(pid) = session.profile_id.clone() {
                        self.conn_ui.connecting_profiles.remove(&pid);
                    }
                    log::info!("SSH 会话建立: tab={id} profile={:?}", session.profile_id);
                    // 把先前开出的占位 Tab「升级」成真实会话；找不到则新建（兜底）
                    //
                    // 找的是 `awaits_connect_result` 而不是 `is_connecting`：断连现场的
                    // 重连**带着上一次的失败原因**（断连条要显示它），用 `is_connecting`
                    // 会漏掉它，于是重连成功反而新开一个标签、旧现场变孤儿。
                    let slot = session.profile_id.as_deref().and_then(|p| {
                        self.tabs_state.tabs.iter().position(|t| {
                            t.awaits_connect_result() && t.session.profile_id.as_deref() == Some(p)
                        })
                    });
                    // 落点是否已经有一块屏？有 = 这是「断连现场」上的重连 ⇒ 复用它。
                    // 走到这里还带着终端视图的只有一条路径：`mark_session_dead` 之后
                    // `reconnect_tab` 把状态推回 `Connecting`（主动断开那条会把终端卸下，
                    // 所以落在 `None` 上，见 `detach_session`）。
                    let reuse_ix = slot.filter(|ix| self.tabs_state.tabs[*ix].terminal.is_some());
                    match slot {
                        Some(ix) => {
                            // 原地重连：Tab 身份（下标 / 位置 / 是否分屏）不变，
                            // 只换掉会话与脚下的管道。`dead` 必须清掉 —— 它同时是
                            // 「输入封死」与「画断连条」的开关，留着会让重连成功的
                            // 新终端依旧收不了键盘。
                            let old_id = self.tabs_state.tabs[ix].session.id.clone();
                            let new_id = session.id.clone();
                            if reuse_ix.is_some() {
                                // ★ 复用「原连接的那块屏」（2026-09-22 用户要求：
                                //   「超时注销后重新连接，不要清除原连接的内容」）。
                                //
                                // 网格、回滚缓冲、划选、搜索高亮、滚动位置全留在原地，
                                // 只把新会话的读/写端接上去，并在断点处写一行分隔行。
                                //
                                // `scroll` 也**不用换**：`TermScrollHandle` 持有的是这个
                                // `Entity`，视图还是同一个，句柄自然还指得对。
                                let view = self.tabs_state.tabs[ix]
                                    .terminal
                                    .clone()
                                    .expect("reuse_ix 已保证该 Tab 有终端视图");
                                // 分隔行是本层给网格的原始字节（引擎只负责喂给 VTE），
                                // 故 `\x1b[2m`（SGR 2 = 降亮）也由本层写：它让这行与
                                // 远端输出一眼可分，而不会被误当成服务器说的话。
                                let notice = format!(
                                    "\x1b[2m[{}]\x1b[0m",
                                    rustshell_core::i18n::t("已重新连接")
                                );
                                crate::terminal_view::reattach_terminal_view(
                                    &view,
                                    &mut session,
                                    Some(&notice),
                                    cx,
                                );
                            } else {
                                let (terminal, scroll) = self.mount_terminal(&mut session, cx);
                                self.tabs_state.tabs[ix].terminal = Some(terminal);
                                self.tabs_state.tabs[ix].scroll = Some(scroll);
                            }
                            self.tabs_state.tabs[ix].session = session;
                            self.tabs_state.tabs[ix].connect_error = None;
                            self.tabs_state.tabs[ix].connect_started = None;
                            self.tabs_state.tabs[ix].dead = None;
                            // 连上了：重连账本作废。清账是一句断言 —— 这套账只服务
                            // 「往回连」，连上了就没有下一次了（`attempts` 留着没人读，
                            // 但留着会让断连条在下次掉线时显示错误的上一次次数）。
                            self.tabs_state.tabs[ix].retry = None;
                            // 分屏槽位存的是**旧的 session id**，不跟着重映射的话
                            // 重连成功会凭空散掉一屏（`normalize_split` 只会把
                            // 「已不存在的 id」修掉，修的方式是取消分屏）。
                            if old_id != new_id {
                                if let Some(sp) = self.tabs_state.split.as_mut() {
                                    if sp.primary == old_id {
                                        sp.primary = new_id.clone();
                                    }
                                    if sp.secondary == old_id {
                                        sp.secondary = new_id.clone();
                                    }
                                }
                            }
                            self.tabs_state.active = Some(new_id);
                        }
                        None => {
                            let (terminal, scroll) = self.mount_terminal(&mut session, cx);
                            self.tabs_state.tabs.push(TabEntry {
                                session,
                                terminal: Some(terminal),
                                scroll: Some(scroll),
                                connect_error: None,
                                connect_started: None,
                                dead: None,
                                retry: None,
                            });
                            self.tabs_state.active = Some(id);
                        }
                    }
                    self.tabs_state.pending_focus = true;
                }
                Err((profile_id, e)) => {
                    // 评审 P1-4：失败路径显式收尾，清除该 profile 的「连接中」标记
                    self.conn_ui.connecting_profiles.remove(&profile_id);
                    log::warn!("SSH 连接失败: profile={profile_id} err={e}");
                    // 把错误写进对应 Tab（超时/拒绝/认证失败都在这里落地），
                    // 而不是只丢到一个全局提示里，用户需要知道「哪个连接、为什么失败」。
                    let slot = self.tabs_state.tabs.iter_mut().find(|t| {
                        t.awaits_connect_result()
                            && t.session.profile_id.as_deref() == Some(profile_id.as_str())
                    });
                    match slot {
                        Some(tab) => {
                            tab.connect_started = None;
                            tab.session.state = SessionState::Closed;
                            // 断连态下的重连失败：**画面与断连条一个字都不动**，
                            // 只把失败原因挂到 `connect_error` 上，让断连条多显示
                            // 一段「重连失败：…」的尾巴。
                            //
                            // 这里绝不能清掉 `dead` —— 清掉就等于把用户那一屏输出
                            // 换成「连接失败」占位卡（`dead` 同时是输入封死与断连条
                            // 渲染的开关），正是本次要消灭的「跳页面」。
                            // 占位卡只在 `terminal.is_none()` 时才会选上，与这里无关。
                            tab.connect_error = Some(e);
                            // 账本还有预算就排下一次（退避），用尽就清账并在断连条上
                            // 留下最终原因。判定读**实时**的开关与连接级三态：
                            // 用户在重试途中把自动重连关掉，就是这个停止键生效的时刻。
                            let had_ledger = tab.retry.is_some();
                            let choice = self
                                .conn_store
                                .profile(profile_id.as_str())
                                .and_then(|p| p.auto_reconnect);
                            let next = tab.advance_retry_after_failure(
                                choice,
                                self.settings.auto_reconnect,
                                self.settings.reconnect_interval_secs,
                            );
                            match (next, tab.retry_progress()) {
                                (Some(wait), Some(p)) => log::info!(
                                    "重连失败，{wait} 秒后第 {}/{} 次重试: profile={profile_id}",
                                    p.attempt,
                                    p.max
                                ),
                                _ if had_ledger => {
                                    log::warn!("自动重连已用尽，停止重试: profile={profile_id}")
                                }
                                _ => {}
                            }
                            let id = tab.session.id.clone();
                            self.tabs_state.active = Some(id);
                        }
                        None => self.conn_ui.connect_error = Some(e),
                    }
                }
            }
        }
        // 清理“连接中”标记（按 profile 有无已建立的 tab；占位 Tab 不算）
        let active_profiles: HashSet<String> = self
            .tabs_state
            .tabs
            .iter()
            .filter(|t| !t.is_connecting())
            .filter_map(|t| t.session.profile_id.clone())
            .collect();
        self.conn_ui
            .connecting_profiles
            .retain(|id| !active_profiles.contains(id));

        // 2) 首次信任请求（M3 known_hosts）
        while self.conn_ui.pending_trust.is_none() {
            match rustshell_core::ssh::trust_requests().1.try_recv() {
                Ok(req) => {
                    self.conn_ui.pending_trust = Some(req);
                    changed = true;
                }
                Err(_) => break,
            }
        }

        // 2.5) AI 结果（M3.5）
        // 归属：AI 对话列（流式正文、轨迹、错误提示）+ 侧栏（AI 面板里的
        // 「回答中」小标与消息计数读 `ai_busy`）。侧栏只在**起止**两帧需要它，
        // 但那两帧也走这条路径，所以不能只点 AI 对话列。
        if self.ai_poll_results(cx) {
            changed = true;
            dirty = dirty.set(ChildView::AiChat).set(ChildView::Sidebar);
        }
        // 归属：只有文件面板（侧栏）读 `files_*`
        if self.sftp_poll_events(cx) {
            changed = true;
            dirty = dirty.set(ChildView::Sidebar);
        }
        // 定时任务调度（内部按秒节流；未起调度器时是零开销的一次 None 判断）。
        //
        // 归属由 `tasks_tick` 自己点名（`ChildView::Sidebar`）并直接发 notify ——
        // 这里**不再**把它并进 `dirty`：本函数末尾会用 `dirty` 统一发一次
        // 归属 notify，而 `tasks_tick` 已经发过了，再并一次就是同一帧两次
        // `cx.notify()`（第二次是空转，但仍会把侧栏的版本号再推一格）。
        //
        // 2026-09-28 修：此前这里是 `changed |=`，且 `tasks_tick` 内部用裸
        // `cx.notify()` —— 空掩码 ⇒ `wants_redraw` 判定为「兜底重绘」⇒
        // **侧栏每秒被整块重建一次**，任务面板的搜索框因此打字掉焦点
        // （详见 `tasks_tick` 的文档）。
        changed |= self.tasks_tick(cx);

        // 3) 会话事件
        // 三种结局要分清楚：
        // - **用户主动关**（`state` 已被置成 `Closed`）→ 关标签；
        // - **本地 Shell 退出**（用户敲了 exit）→ 关标签（Windows Terminal 同款预期）；
        // - **SSH 掉线**（远端注销 / 空闲超时 / 网络中断）→ **原地转断连态**：
        //   标签不关、命令窗口不弹、不显示「连接失败」占位卡，只把终端画面留住
        //   并在窗格底部叠一条断连条 + 封死输入。
        //   2026-09-21 运行反馈：此前这一支走的是 `close_tab_by_session`，标签一关
        //   就是一页空窗格，观感就是「跳到一张新页面」。
        let mut to_close: Vec<SessionId> = Vec::new();
        let mut dropped: Vec<(SessionId, Option<String>, String)> = Vec::new();
        for tab in &self.tabs_state.tabs {
            while let Ok(ev) = tab.session.events_rx.try_recv() {
                changed = true;
                match ev {
                    rustshell_core::session::SessionEvent::Closed => {
                        // 归类口径见 `classify_session_end` 的文档（它同时是被单测
                        // 钉住的那一处判定）
                        match classify_session_end(tab.session.state.clone(), tab.session.kind) {
                            SessionEnd::KeepScene => dropped.push((
                                tab.session.id.clone(),
                                tab.session.profile_id.clone(),
                                rustshell_core::i18n::t("连接已被远端关闭").to_string(),
                            )),
                            SessionEnd::CloseTab => to_close.push(tab.session.id.clone()),
                        }
                    }
                    rustshell_core::session::SessionEvent::Error(e) => {
                        self.conn_ui.connect_error = Some(e);
                    }
                    _ => {}
                }
            }
        }
        for id in to_close {
            changed = true;
            log::info!("会话关闭: tab={id}");
            self.close_tab_by_session(&id, cx);
        }
        // 掉线处理：先落地断连现场，再（按配置）给这个标签排上重连计划。
        //
        // 「排计划」而不是「当场连一次」：旧实现只有这一次尝试，失败了不会再排
        // —— 于是「最大重试次数」这个设置从来没被兑现过（设 3 也只连一次）。
        // 现在第一次也走退避：掉线往往伴随网络抖动，立刻重连常常白撞一次。
        for (id, pid, reason) in dropped {
            changed = true;
            self.mark_session_dead(&id, reason, cx);
            self.schedule_retry(&id, pid.as_deref());
        }
        // 账本到点：把该试的那些真正发出去。
        self.fire_due_retries(cx);

        // 4) AI 会话落盘（2026-09-23「AI对话需要保存」）。有改动才写，流式期间按
        // 3 秒节流（见 `ai_persist`）；真正把「关窗不丢」钉死的是退出钩子
        // （`ui_install_flush_ai_chats_on_quit`），这里负责的是「别等关窗」。
        //
        // 刻意**不参与 `changed`**：写文件不改任何被渲染的东西，为它重绘整棵树
        // 等于把「空闲时真正静止」这条性能约束又放开了（poll 每 300ms 一轮）。
        self.ai_chats_flush_if_due();

        // 4.2) 命令历史落盘（C-1 遗留项 1）。同样**刻意不参与 `changed`** ——
        // 写文件不改任何被渲染的东西。节流 1 秒（见 `completion_persist`）：
        // 比 AI 那边短得多，因为历史每次只改一条记录、而用户敲命令是「一条接一条」，
        // 等 3 秒会让刚敲完的那条有极大概率还没写进去 —— 而那正是最可能被立刻重用的。
        self.cmd_history_flush_if_due();

        // 4.5) F-9 会话布局落盘 / 恢复推进。
        //
        // 与 AI 会话同一手法：指纹脏标记（当前布局 vs 上次写的）+ 节流，
        // 刻意**不参与 `changed`** —— 它不改任何被渲染的东西；恢复期间它负责
        // 逐个把本地 Shell 排上队（那次 spawn 自己会 notify），且**一律不写快照**。
        self.layout_poll(cx);

        // 卡顿观测：单次 poll 超过 100ms 记入运行日志（M4 诊断）
        let elapsed = poll_start.elapsed();
        if elapsed > std::time::Duration::from_millis(100) {
            log::warn!("poll 慢帧: {:?}", elapsed);
        }
        // 归属登记必须与 `changed` 同步：只登记不 notify 会让这次的归属**留到**
        // 下一次无关的 notify 上，把本该重绘的视图挡掉（见 `ViewEpoch` 的约束）。
        if changed {
            self.panel_ui.view_epoch.merge(dirty);
        }
        crate::watchdog::leave();
        changed
    }
    /// 「没有任何可用会话」提示：主窗格顶部居中的**无需确认**的弹出消息。
    ///
    /// 位置：仍在主窗口里显眼处（2026-09-17 运行反馈「不要贴在命令栏上」），
    /// 但**不是正中** —— 正中被空态自己占着（见下面覆盖层的注释）。
    /// 交互形态按 2026-09-18 运行反馈改成「不需要确认」：没有按钮、没有蒙版，
    /// 到点自己消失（定时器排在该提示的唯一入口 `MainView::show_no_session_alert`）。
    ///
    /// 三处刻意的取舍：
    /// - **不给按钮**：原来那两枚（「知道了」/「新建本地终端」）等于逼用户先点一下
    ///   才能继续。用户真正要的答案是「命令为什么没出去」，那是一句话，不是一道选择题。
    /// - **不铺蒙版**：蒙版会把整屏压暗、看着像模态框 —— 与「不需要确认」自相矛盾。
    ///   卡片自身有不透明底色 + 描边，压不压暗都读得清。
    /// - **不挂任何鼠标监听**：这个覆盖层与卡片都没有 `.id()`/`.on_click`，
    ///   于是 gpui 不会为它们建 hitbox（`Div::should_insert_hitbox`），
    ///   点击原样落到下面的终端上 —— 提示归提示，手不停。
    ///
    /// 形态遵循 `ui/src/notice.rs` 的口径：官方 `Alert` **没有子节点槽**，
    /// 所以「标题 + 说明」只能是两个兄弟节点，不给 `Alert` 塞 `child`。
    fn render_no_session_notice(cx: &mut Context<Self>) -> gpui::AnyElement {
        use crate::theme::to_hsla as hex;

        let t = crate::theme::current(cx);
        // 卡片：两行（警告行 + 一句原因），**没有第三行按钮**。
        // debug_selector 保留 `no-session-alert` 前缀：它仍是同一条提示，
        // 只是形态从「要确认」变成「不需要确认」。
        let card = v_flex()
            .debug_selector(|| "no-session-alert".to_string())
            .w(px(420.))
            .gap_2()
            .p_4()
            .rounded_lg()
            .bg(hex(t.bg_overlay))
            .border_1()
            .border_color(hex(t.border_subtle))
            .child(crate::notice::NoticeKind::Warning.card(
                "no-session-alert-title",
                rustshell_core::i18n::t("当前没有已连接的会话"),
            ))
            .child(
                div()
                    .text_size(px(11.5))
                    .text_color(hex(t.fg_secondary))
                    .child(SharedString::from(rustshell_core::i18n::t(
                        "命令没有发出去：先新建本地终端，或连接一台服务器。",
                    ))),
            );

        // 覆盖层：水平居中、贴顶一条 —— 不是「正中」。
        //
        // 位置改动的原因：**正中被空态自己占了**。没有任何会话时，主窗格正中是
        // `split_actions::render_no_session` 那张空态（图标 + 「主窗格为空」+
        // 「新建本地终端」按钮），而这条提示恰恰最常在这种时刻出现；两者都居中
        // 就会叠在一起，把那个**可点的按钮**盖住 4 秒。
        //
        // 贴顶后仍然在「主窗口里一眼看得见」的位置（2026-09-17 反馈要的就是
        // 「别贴在命令栏上、太偏太短」），同时不遮空态按钮、不遮终端正文中段。
        //
        // 覆盖层不铺底色、不挂监听（见函数头说明）：看得见、点得穿。
        div()
            .debug_selector(|| "no-session-alert-overlay".to_string())
            .absolute()
            .inset_0()
            .flex()
            .items_start()
            .justify_center()
            .pt_3()
            .child(card)
            .into_any_element()
    }

    /// 终端全屏开关（标签栏右侧那枚图标）。
    ///
    /// 两件事一起做：**窗口**进系统全屏（`toggle_fullscreen`），同时把竖栏 / 侧栏 /
    /// AI 对话列从布局里摘掉（见 render 里的 `sidebar_visible` / `ai_visible`），
    /// 终端卡片独占内容区。用户按的是「我要整个屏幕都是终端」，只做其中一件都不到位。
    ///
    /// 状态由 `terminal_fullscreen` 自己持（而不是每帧读 `window.is_fullscreen()`）：
    /// 平台层的全屏切换是**异步**的（gpui-windows 里 spawn 到 executor 才改窗口），
    /// 按 `is_fullscreen()` 供状态会让图标与布局晚一帧才动、某些平台甚至永远不动。
    pub fn toggle_terminal_fullscreen(&mut self, window: &Window, cx: &mut Context<Self>) {
        self.terminal_fullscreen = !self.terminal_fullscreen;
        window.toggle_fullscreen();
        cx.notify();
    }
}

/// 内容区卡片外壳：圆角 + 裁切 + 1px 描边 + 底色。
///
/// `overflow_hidden` 与圆角是**成对**的：卡片里最外层的东西（终端画布、Tab 栏底色、
/// 侧栏面板底色）都是方的，不裁的话四个角会把圆角盖回去，看起来像「圆角没生效」。
///
/// 注意它只该包在 `ResizablePanel` 的**内层**：`ResizablePanel` 自己的拖拽手柄是
/// absolute 定位在面板左缘外侧 4px，直接给面板加 `overflow_hidden` 会裁掉命中区
/// （见 gpui-base `resizable/panel.rs` 的 "Reserved styles" 注释）。
fn card_shell(bg: gpui::Hsla, border: gpui::Hsla) -> gpui::Div {
    div()
        .w_full()
        .h_full()
        .rounded(px(CARD_RADIUS))
        .overflow_hidden()
        .border_1()
        .border_color(border)
        .bg(bg)
}

impl Render for MainView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::log_cold_start_once();
        // 阶段打点：render 会同步构建整棵视图树（终端 canvas 的绘制闭包在 gpui
        // 内部绘制阶段执行，另有 PtyResize 打点）。卡住时看门狗会报出本阶段。
        crate::watchdog::enter(crate::watchdog::Phase::Render);
        let __perf_t0 = std::time::Instant::now();
        let t = crate::theme::current(cx);
        use crate::theme::to_hsla as hex;

        // 文件面板路径栏回填：轮询侧拿不到 window，只能在这里写输入框。
        //
        // `InputState::set_value` 内部把 `emit_events` 置 false（见
        // gpui-base `input/base/state.rs`），因此回填**不会**触发一次
        // `InputEvent::Change`；紧随其后清掉 `files_path_typed` 是显式复位，
        // 表示「输入框内容不再是用户手输的草稿」，与 Change 订阅无关。
        if let Some(path) = self.io.files_path_sync.take() {
            self.io
                .files_path_input
                .update(cx, |s, cx| s.set_value(path, window, cx));
            self.io.files_path_typed = false;
        }

        // OpenSSH 配置「手动选文件」的回填：异步选文件回调拿不到 Window，
        // 走两段式 —— 回调只置旗，这里兑现（弹预览对话框需要 Window）。
        if let Some((config, ssh_dir, home)) = self.conn_ui.openssh_pick_sync.take() {
            self.load_openssh_config(config, ssh_dir, home, window, cx);
        }

        // 焦点收敛：Tab 切换/新建后把键盘焦点交给激活终端。
        // 对话框开着时不许收：弹框期间到达的异步事件（命令面板关闭、SSH 连上、
        // 标签页操作）都会置这个旗，若照常执行，焦点会被拽出模态框、落到背后的
        // 终端上 —— 表现为「设置面板里打字，字却进了终端」。旗子不消费，等框关掉
        // 后的下一帧再收敛。
        if self.tabs_state.pending_focus && !window.has_active_dialog(cx) {
            match self.active_tab().and_then(|t| t.terminal.as_ref()) {
                Some(term) => {
                    let h = term.read(cx).focus_handle().clone();
                    window.focus(&h, cx);
                }
                // 没有终端可聚焦（空窗格 / 还在连接 / 刚关掉最后一个标签）：
                // 把窗口焦点收回视图级句柄。不收的话焦点会停在**已经不在
                // 本帧元素树里**的旧句柄上，键盘动作的派发路径退化成只剩
                // 派发树根节点，根 div 上的 `on_action` 全部失灵。
                // 收回来之后由 `render_focus_anchor` 负责把它注册进派发树。
                None => window.focus(&self.panel_ui.focus, cx),
            }
            self.tabs_state.pending_focus = false;
        }

        // F-9 会话恢复询问框：`MainView::new` 只置了旗（那一刻窗口根视图还不是
        // 官方 `Root`，`present_dialog` 里的 `has_active_dialog` 一读就 panic），
        // 真正的开框落在这里 —— 能跑到 `render` 就说明 `Root` 已经装上了。
        //
        // 守卫与 `present_dialog` 自己的一致（有别的框开着就先不弹），
        // 且**只在真的还没弹过**时消费（`prompt_open` 已由启动段置上，
        // 这里不再重复判定它，避免「置旗的那段改了语义、这里悄悄跟着变」）。
        if self.tabs_state.layout_state.prompt_arm {
            self.tabs_state.layout_state.prompt_arm = false;
            // `pending` 还在 = 用户还没答过（答过之后要么恢复、要么作废，两条路都会
            // `take()` 掉它）。没有它就没有「恢复什么」这个前提，框不该弹。
            if self.tabs_state.layout_state.pending.is_some() && !window.has_active_dialog(cx) {
                self.present_dialog(DialogMode::RestoreSession, window, cx);
            }
        }

        // 启动即打开命令窗口（2026-09-17 运行反馈「默认不打开本地 shell，打开命令窗口」）。
        // 必须落在这里：开浮层要 `window.focus`，而启动路径（`shell/src/app.rs`）里没有 Window。
        // 只消费一次；`open_command_palette` 内部取的是**当前**连接快照，正是启动时想要的那份。
        if self.panel_ui.pending_palette {
            self.panel_ui.pending_palette = false;
            // F-9：这一帧要弹「恢复上次会话？」时不叠命令窗口 —— 两个浮层同帧
            // 抢焦点只会打架，而且用户还没决定要不要恢复，此刻的「最近连接」
            // 也不是他要的答案。待办**就地作废**（不延后到恢复框关掉之后）。
            if !self.tabs_state.layout_state.prompt_open {
                self.open_command_palette(window, cx);
            }
        }

        // 粘贴确认框（F-5）。两个方向都在这里，顺序要紧：
        //
        // 1) **先收掉已经无效的框**：弹框期间会话断了 / 标签被关了，挂起就没有了，
        //    框留着只会让人对着一件不存在的事按确认。
        // 2) 再看有没有人挂着待确认的粘贴 —— 检测直读终端实体（不经过 poll）：
        //    粘贴是手起刀落的一瞬，等下一轮轮询（300ms）就可能让人以为「没生效」
        //    而再粘一次。见 `paste_guard` 的模块文档。
        //
        // 有别的对话框开着时不弹（`present_dialog` 自己也有同一道守卫）：
        // 不消费、不丢弃 —— 挂起还在，等那扇关掉之后的下一帧照常弹出来。
        self.paste_prompt_prune(window, cx);
        if !window.has_active_dialog(cx) {
            if let Some(session) = self.paste_prompt_session(cx) {
                self.present_dialog(DialogMode::PasteConfirm { session }, window, cx);
            }
        }

        // 2026-09-29 用户反馈「关掉全部 tab 不再触发命令窗口显示」：原先「关掉最后
        // 一个标签 → 自动弹出命令窗口」的待办机制（`palette_when_empty`）整条撤掉。
        // 标签清空后的画面交给空态本身（`split_actions::render_no_session`：
        // 一张「动作名 …… 键帽」的快捷键表，每行点即执行），不再由程序替用户
        // 把命令窗口推上来。启动即开命令窗口是另一回事（见上面的 `pending_palette`，
        // 由「启动动作」设置决定），不在此列。

        // 内容区（终端 / 占位卡 / 搜索浮层 / 滚动条 / 右键菜单）统一由
        // `render_workspace` 产出：未分屏是单个窗格，分屏时是两个可拖拽窗格。
        // 数据在窗格内部按各自 session 取，这里不再预先择出「激活终端」——
        // 分屏后「激活」只是焦点窗格，不等于屏幕上唯一的终端。

        // （崩溃）：根节点不再`track_focus(&self.panel_ui.focus)`。
        //`MainView`已实现`Focusable`（`focus_handle()`返回`self.panel_ui.focus`），
        // gpui 会在窗口获得焦点时把视图级焦点设到`self.panel_ui.focus`。若根 div 再对
        // 同一个 handle 调`track_focus`，同一帧内就有两个元素（根 div 与
        // 激活终端的`div().track_focus(&term.focus_handle)`，见
        //`terminal/src/view.rs`）各自判定`focus_handle.is_focused(window)`
        // 为真 到 两次`window.a11y.set_focus(node_id)`。
        // gpui 对该调用有硬契约（`window/a11y.rs`：`set_focus`每帧只能调一次），
        // 且`DispatchTree::focusable_node_ids`以`FocusId`为键、后写覆盖
        // （`key_dispatch.rs:223`），同一个`FocusId`被两个元素认领时，
        // 两个元素都会看到`is_focused == true`，于是 debug 构建
        //`panic!("set_focus called more than once in a single frame")`，
        // 随之触发 gpui 内部`RefCell already borrowed`次生 panic。
        // 现象即「终端 tab 页看不到了，后台报错程序崩溃」。
        // 视图级焦点由`Focusable`承担即可，键派发仍由`key_context`与
        // 各元素自身的`track_focus`覆盖；根节点无需重复认领同一个 handle。
        let mut root = v_flex()
            .id("rustshell-root")
            .relative()
            .size_full()
            .bg(self.panel_bg(t.bg_base))
            .text_color(hex(t.fg_primary))
            .text_size(px(13.))
            .on_action(cx.listener(|m, _: &NewTab, _w, cx| {
                m.open_local_shell(cx);
            }))
            .on_action(cx.listener(|m, _: &CloseTab, _w, cx| {
                if let Some(id) = m.tabs_state.active.clone() {
                    m.close_tab_by_session(&id, cx);
                }
            }))
            .on_action(cx.listener(|m, _: &ToggleSidebar, _w, cx| {
                m.panel_ui.sidebar_collapsed = !m.panel_ui.sidebar_collapsed;
                cx.notify();
            }))
            .on_action(cx.listener(|m, _: &OpenSettings, w, cx| {
                m.open_settings(w, cx);
            }))
            .on_action(cx.listener(|m, _: &CommandPalette, w, cx| {
                m.open_command_palette(w, cx);
            }))
            .on_action(cx.listener(|m, _: &NewConnection, w, cx| {
                // 命令窗口的「新增连接」：与竖栏 / 连接树空白处右键同一个入口。
                m.open_connect_dialog(None, w, cx);
            }))
            .on_action(cx.listener(|m, _: &AiAssistant, w, cx| {
                // 命令窗口的「AI助手」：切到 AI 面板（与 `ai <描述>` 直通同一落点）。
                m.ai_passthrough("", w, cx);
            }))
            .on_action(cx.listener(|m, _: &RestoreLastSession, _w, cx| {
                // 命令窗口的「恢复上次会话」：2026-09-29 起恢复功能的唯一入口
                // （启动不再自动弹框）。只置旗，真正的开框落在 render 里由
                // `prompt_arm` 兑现 —— 与手动触发前的历史口径一致。
                m.layout_restore_now();
                cx.notify();
            }))
            .on_action(cx.listener(|m, a: &SwitchTab, _w, cx| {
                if let Some(tab) = m.tabs_state.tabs.get(a.index) {
                    m.tabs_state.active = Some(tab.session.id.clone());
                    m.tabs_state.pending_focus = true;
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|m, _: &TabNext, _w, cx| {
                if !m.tabs_state.tabs.is_empty() {
                    let cur = m
                        .tabs_state
                        .active
                        .as_deref()
                        .and_then(|id| m.tabs_state.tabs.iter().position(|t| t.session.id == id))
                        .map(|i| (i + 1) % m.tabs_state.tabs.len())
                        .unwrap_or(0);
                    m.tabs_state.active = Some(m.tabs_state.tabs[cur].session.id.clone());
                    m.tabs_state.pending_focus = true;
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|m, _: &TabPrev, _w, cx| {
                if !m.tabs_state.tabs.is_empty() {
                    let n = m.tabs_state.tabs.len();
                    let cur = m
                        .tabs_state
                        .active
                        .as_deref()
                        .and_then(|id| m.tabs_state.tabs.iter().position(|t| t.session.id == id))
                        .unwrap_or(0);
                    m.tabs_state.active =
                        Some(m.tabs_state.tabs[(cur + n - 1) % n].session.id.clone());
                    m.tabs_state.pending_focus = true;
                    cx.notify();
                }
            }))
            // Tab/Shift+Tab 终端补全：绑定在 gpui-component Root 之后注册（后注册胜出），
            // 焦点在终端时写 PTY 并阻断；否则放行给 Root 的默认焦点遍历
            .on_action(cx.listener(|m, _: &TermTab, w, cx| {
                m.send_tab_to_terminal("\t", w, cx);
            }))
            .on_action(cx.listener(|m, _: &TermShiftTab, w, cx| {
                m.send_tab_to_terminal("\x1b[Z", w, cx);
            }))
            .on_action(cx.listener(|m, _: &SearchInTerminal, w, cx| {
                m.open_term_search(w, cx);
            }))
            // 标签分屏：向右 = 左右分屏（副窗格在右侧），向下 = 上下分屏。
            // 决策（已有分屏则只切方向 / 复用池中标签 / 需要新建会话）在 core::split。
            .on_action(cx.listener(|m, _: &SplitRight, w, cx| {
                m.split_pane(SplitAxis::Vertical, w, cx);
            }))
            .on_action(cx.listener(|m, _: &SplitDown, w, cx| {
                m.split_pane(SplitAxis::Horizontal, w, cx);
            }))
            // F2：重命名选中连接。全局绑定会先于终端的按键处理命中，所以在处理器里
            // 判断「焦点是否在终端」——在终端里按 F2 应当原样送给远端程序（如 mc、
            // 或用户自己绑的快捷键），而不是抢来开重命名框。
            .on_action(cx.listener(|m, _: &RenameConnection, w, cx| {
                if m.terminal_has_focus(w, cx) {
                    return;
                }
                m.rename_selected_connection(w, cx);
            }));

        // 窗口背景图：绝对定位铺满，作为第一个子节点 —— 后续所有面板盖在其上。
        // `Panel 底色 * panel_opacity` 透出这张图（见`MainView::panel_bg`）。
        // 图源路径已由`refresh_background`校验过（渲染路径不做文件系统 IO）；
        // 覆盖层用纯黑按`background_dim`配 alpha，避免亮图把面板文字冲淡。
        if let Some(bg) = self.panel_ui.bg_source.clone() {
            root = root
                .child(
                    div()
                        .absolute()
                        .inset_0()
                        .child(gpui::img(bg).object_fit(ObjectFit::Cover)),
                )
                .child(
                    div()
                        .absolute()
                        .inset_0()
                        .bg(gpui::rgba(self.settings.clamped_background_dim())),
                );
        }

        // 过程提示条（本地 Shell 启动中）：accent 底色，随启动完成自动消失
        if self.conn_ui.connect_error.is_none() {
            if let Some(notice) = self.conn_ui.connect_notice.clone() {
                root = root.child(
                    div()
                        .absolute()
                        .top(px(40.))
                        .left_0()
                        .w_full()
                        .flex()
                        .justify_center()
                        .px_4()
                        .child(
                            h_flex()
                                .gap_2()
                                .items_center()
                                .px_3()
                                .py_1()
                                .bg(hex(t.accent))
                                .rounded(px(6.))
                                .child(
                                    Icon::new(IconName::Loader)
                                        .small()
                                        .text_color(crate::theme::to_hsla(t.on_accent)),
                                )
                                .child(
                                    div()
                                        .text_size(px(12.))
                                        .text_color(crate::theme::to_hsla(t.on_accent))
                                        .child(SharedString::from(notice)),
                                ),
                        ),
                );
            }
        }

        // 全局错误提示条（连接超时/认证失败/本地 Shell 启动失败统一出口）：
        // 顶部居中悬浮。内容交给官方`Alert`（error 变体 = 本应用 danger 语义色），
        // 关闭按钮由 `Alert::on_close` 自带，不再手拼「实底 + 白字 + 关闭按钮」。
        // 旧实现的实底白字在浅色主题下与页面其它提示风格割裂，且 `on_danger`
        // 前景要手工取色；官方 Alert 用「同色淡底 + 同色文字 + 同色描边」，
        // 明暗两套主题都自适应。
        if let Some(err) = self.conn_ui.connect_error.clone() {
            let me = cx.entity();
            root = root.child(
                div()
                    .absolute()
                    .top(px(40.))
                    .left_0()
                    .w_full()
                    .flex()
                    .justify_center()
                    .px_4()
                    .child(
                        div().w_full().max_w(px(560.)).child(
                            crate::notice::NoticeKind::Error
                                .card("global-connect-error", SharedString::from(err))
                                .on_close(move |_ev, _w, cx| {
                                    me.update(cx, |m, cx| {
                                        m.conn_ui.connect_error = None;
                                        cx.notify();
                                    });
                                }),
                        ),
                    ),
            );
        }

        // 标题栏。`TitleBar` 官方默认取主题 bar 底色（不透明），会盖住背景图顶部；
        // 改经 `panel_bg` 透出（2026-09-18 反馈「背景图只替换了连接栏」）。
        // 高度收一档见 `TITLE_BAR_HEIGHT`；品牌名后补版本号（2026-09-18 反馈
        // 「增加版本显示」）——版本号取 `core::APP_VERSION`（= 工作区 Cargo.toml 的
        // `workspace.package.version`），不写死字面量，发版时不会忘记同步。
        root = root.child(
            gpui_component::TitleBar::new()
                .h(px(TITLE_BAR_HEIGHT))
                // 隐藏标题栏下边框（2026-10-09 用户要求「隐藏标题栏的下方的线」）。
                //
                // 这条线**不是本应用画的**：官方 `TitleBar::render` 链上自带
                // `.border_b_1().border_color(cx.theme().title_bar_border)`
                // （gpui-component-0.6.4/src/title_bar.rs），横贯整个窗口宽度。
                //
                // 覆盖机理与紧邻的 `.h(..)` 完全一致：官方在
                // `.h(..).border_b_1()....refine_style(&self.style)` 之后才把调用点的
                // `StyleRefinement` 并进来，`Some(px(0.))` 会盖掉官方那个 1px；
                // 宽度归零后 `is_border_visible()` 直接跳过这条边的绘制（不是画成透明）。
                // 本应用标题栏底色与下方内容同为 `bg_base`，分界交给留白即可。
                .border_b_0()
                .bg(self.panel_bg(t.bg_base))
                // 标题行**整体**不参与全局字号收档（用户要求「标题行不用动」）。
                // 官方 `TitleBar` 的子节点（品牌名、右上角三枚控制图标）都按**继承**的
                // 字号绘制 —— `Icon::render` 取的就是 `window.text_style().font_size`
                // —— 所以钉在这一层比逐个孩子写字号更稳。
                .text_size(px(15.))
                .child(
                    h_flex()
                        .w_full()
                        .items_center()
                        .child(
                            // 左侧「品牌名 + 版本号」占 `flex_1`，与右侧等宽空占位配平 ⇒
                            // 中段的命令入口落在窗口几何中心，而不是被左侧文字挤偏。
                            h_flex()
                                .flex_1()
                                .min_w_0()
                                .gap_2()
                                .items_center()
                                .child(
                                    div()
                                        .text_size(px(TITLE_BAR_BRAND_SIZE))
                                        .text_color(hex(t.accent))
                                        .font_weight(gpui::FontWeight::BOLD)
                                        .child("RustShell"),
                                )
                                .child(
                                    // 版本号弱化为次要色：它是「查版本」时才看的元信息，
                                    // 与品牌名同色会抢视线。
                                    div().text_size(px(10.)).text_color(hex(t.fg_muted)).child(
                                        SharedString::from(format!(
                                            "v{}",
                                            rustshell_core::APP_VERSION
                                        )),
                                    ),
                                ),
                        )
                        .child(title_bar_command_entry(cx))
                        .child(div().flex_1()),
                ),
        );

        // 中段：竖栏 | 右列。右列 =「侧栏 | 终端 | AI 对话」内容行（快捷命令栏在终端列内）。
        //
        // 2026-09-18 运行反馈「参考 vscode 侧边栏和终端外圈线条变为圆角」：
        // 三块面板从「通栏 + 一条竖分割线」改为**独立的圆角卡片**，卡片之间与卡片与
        // 窗口边缘之间留出 `CARD_GAP` 的空隙。
        //
        // 空隙由**面板自己的 padding** 产生，而不是外层容器 padding：
        // `ResizablePanel` 的拖拽手柄是 absolute 定位（gpui-base
        // `resizable/resize_handle.rs`，`left: -4px; w: 1px; px: 4px`），外层
        // padding 会把面板连同手柄一起挤窄；面板 padding 则把手柄留在空隙内，
        // 命中区反而更大。左右如何分配这 `CARD_GAP` 见下面 `pad` 处的推导。
        let content = {
            // 侧栏的宽度上下限。这里不再按面板类型分档：`PanelKind::Ai` 从 2026-09-18
            // 起放的是**聊天记录列表**（窄列），不再是需要 300px 才不挤的消息流，
            // 所以统一用 `SIDEBAR_WIDTH_MIN` 即可。
            //
            // 宽度**按面板分别取**（2026-09-20 用户要求「每个侧边栏的宽度可以不同」）：
            // 取 `self.panel_ui.panel` 自己记的宽度，没记过才回退到统一兜底值。
            let w_px = px(self.settings.sidebar_width_for(self.panel_ui.panel.key()));
            let min_w = px(rustshell_core::settings::SIDEBAR_WIDTH_MIN);
            let max_w = px(rustshell_core::settings::SIDEBAR_WIDTH_MAX);
            let ai_w_px = px(self.settings.clamped_ai_chat_width());
            let ai_min_w = px(rustshell_core::settings::AI_CHAT_WIDTH_MIN);
            let ai_max_w = px(rustshell_core::settings::AI_CHAT_WIDTH_MAX);
            // 面板底色统一经`panel_bg`取：未启用背景图时 alpha = 1.0，与改动前
            // 逐像素一致；启用后按`panel_opacity`透出窗口背景图。底色只画在卡片壳上，
            // 内层面板不再自带背景（否则 alpha < 1 时会叠两遍，透出度与设置不符）。
            let sidebar_bg = self.panel_bg(t.bg_elevated);
            let pane_bg = self.panel_bg(t.bg_base);
            let card_border = hex(t.border_subtle);

            // 终端列内容（Tab 栏 + 终端 + 快捷命令栏 + 无会话提示）。
            // `size_full` 而不是 `flex_1`：它现在的父节点是卡片壳（普通块容器），
            // 不再是 flex 行，`flex_1` 在块流里不起作用（高度会塌成内容高）。
            let mut pane = v_flex().size_full().min_w_0().min_h_0();
            // Tab 栏
            pane = pane.child(crate::perf_probe::timed(
                &crate::perf_probe::TABBAR_US,
                || self.render_tab_bar(cx),
            ));
            // 终端内容区：内容 / 占位卡 / 搜索浮层 / 滚动条 / 右键菜单，
            // 统一由`render_workspace`派发（未分屏 = 单窗格；分屏 = 两个
            // 可拖拽窗格）。数据按各自 session 现场取，故这里不再预先
            // 择出「激活终端」—— 分屏后「激活」只是焦点窗格。
            pane = pane.child(crate::perf_probe::timed(
                &crate::perf_probe::WORKSPACE_US,
                || self.render_workspace(cx),
            ));
            // 快捷命令栏（2026-09-15 用户要求「与终端区同宽」）：
            // 它原先挂在`right_col`下，横跨「侧栏 + 终端区」；
            // 移进`pane`后宽度由右面板决定，与终端区严格对齐，
            // 且侧栏折叠/拖宽时跟随，不需要任何宽度同步代码。
            pane = pane.child(crate::perf_probe::timed(
                &crate::perf_probe::QUICKBAR_US,
                || super::quick_commands_bar::render(self, cx),
            ));
            // 「没有可用会话」提示：覆盖在终端区正中（2026-09-17 运行反馈
            // 「显示在主窗口中间」；2026-09-18 起改为无需确认的弹出消息，
            // 见 `render_no_session_notice`）。排在最后 ⇒ 画在终端内容之上；
            // `absolute` 不参与 `pane` 的 v_flex 布局，不会挤动标签栏与命令栏。
            if self.ai_ui.quick_no_session_alert {
                pane = pane.child(Self::render_no_session_notice(cx));
            }

            // T5-S3：拖拽结束才写盘（`on_resize`只在 mouse up 触发一次），
            // 拖拽过程中只改内存，避免每帧 fsync。
            let me_resize = cx.entity();

            // 面板顺序：侧栏（可折叠） | 终端 | AI 对话（可收起）。
            //
            // 留白**左右各分一半**（`CARD_GAP * 0.5`），因为拖拽手柄那条 1px
            // 分界线恰好画在「面板左缘」这条线上，不在它外侧：
            // `ResizablePanel` 的手柄来自 `resize_handle()`（gpui-base
            // `resizable/resize_handle.rs`），样式是
            // `absolute; left: -4px; w: 1px; px: 4px`（`HANDLE_PADDING = 4px`），
            // 而 gpui 默认 `box_sizing: border-box` ⇒ `-4px` 的外移被 4px 内边距
            // 抵掉，可见的 1px 线**净落在面板左缘**。
            //
            // 2026-09-18 实机取像素验证过「留白全记在左卡 `pr`、右卡不设 `pl`」的
            // 版本是错的：那条线正好压在右卡片自己的 1px 描边上（分界线 `#262626`
            // 盖住终端卡左描边 `#30363D`），看上去像卡片缺了一条边。
            // 对半分之后：线在空隙正中（离左右卡片各 4px）；卡间恒为 `CARD_GAP`，
            // 首块补 `pl`、末块补 `pr`，卡与窗口左右边缘也恒为 `CARD_GAP`。
            // 终端全屏时侧栏与 AI 列都让位（但**不改**那两个开关本身：
            // `sidebar_collapsed` / `ai_chat_open` 是用户自己的布局意图，
            // 退出全屏要原样还回去）。
            let fullscreen = self.terminal_fullscreen;
            let sidebar_visible = !self.panel_ui.sidebar_collapsed && !fullscreen;
            let ai_visible = self.ai_ui.ai_chat_open && !fullscreen;
            let lead = usize::from(sidebar_visible);
            let total = lead + 1 + usize::from(ai_visible);
            let pad = |ix: usize| -> (f32, f32) {
                let half = CARD_GAP * 0.5;
                (
                    if ix == 0 { CARD_GAP } else { half },
                    if ix + 1 == total { CARD_GAP } else { half },
                )
            };

            // 面板组 id 按「当前侧栏面板 + 哪几列在显示」分叉，两件都是必需的：
            //
            // - 按面板分叉：「切面板」= 换一套 `panels[ix].size`，新面板首帧就用
            //   自己记的宽度。用同一个 id 的话 `resizable_panel().size(..)` 从第二帧
            //   起被忽略，六个面板会共用第一次拖出来的宽度；
            // - 按可见列分叉：面板组的 slot 是**按位置**存的，列数一变就会张冠李戴
            //   （详见 `main_split_id` 的推导）。
            let panel_key = self.panel_ui.panel.key();
            let split_id = main_split_id(self.panel_ui.panel, sidebar_visible, ai_visible);
            let mut group = gpui_component::resizable::h_resizable(split_id)
                // 分隔线**不画**（2026-09-21 用户要求「隐藏终端界面与左右两侧相间的
                // 线条」）。三条竖边界（侧栏|终端、终端|AI 对话）走的就是这套手柄。
                //
                // 关键：`with_handle_appearance` 换掉的**只是画出来的那一笔** ——
                // 命中区、`col-resize` 光标、拖拽逻辑全都留在 gpui-base 的
                // `ResizeHandle` 里（`resize_handle.rs`：`appearance` 只接管那个
                // `.child(..)` 元素，见其 `unwrap_or_else` 内建分支）。
                // 所以隐藏线**不会**让分界线变得拖不动，只是不再有视觉提示 ——
                // 现在由卡片之间 `CARD_GAP` 的留白承担分界（三块本来就是独立圆角卡片）。
                //
                // 契约提醒：renderer 返回 `Some` 才是「顶掉内建线」，返回 `None`
                // 等于没关。这里给一个零尺寸空 div。
                .with_handle_appearance(std::rc::Rc::new(
                    |_ctx, _w: &mut Window, _cx: &mut App| Some(div().into_any_element()),
                ));
            if sidebar_visible {
                let (pl, pr) = pad(0);
                group = group.child(
                    gpui_component::resizable::resizable_panel()
                        .size(w_px)
                        .size_range(min_w..max_w)
                        .flex_none()
                        .pl(px(pl))
                        .pr(px(pr))
                        .py(px(CARD_GAP))
                        .child(
                            card_shell(sidebar_bg, card_border)
                                // 供测试量「这个面板的侧栏实际多宽」用；release 下是 no-op。
                                .debug_selector(|| "sidebar-card".to_string())
                                .child(self.panel_ui.sidebar_panel.clone()),
                        ),
                );
            }
            {
                let (pl, pr) = pad(lead);
                group = group.child(
                    gpui_component::resizable::resizable_panel()
                        .pl(px(pl))
                        .pr(px(pr))
                        .py(px(CARD_GAP))
                        .child(
                            card_shell(pane_bg, card_border)
                                // 同 `sidebar-card`：供测试量「终端卡片还在不在 / 多宽」。
                                .debug_selector(|| "terminal-card".to_string())
                                .child(pane),
                        ),
                );
            }
            if ai_visible {
                let (pl, pr) = pad(lead + 1);
                group = group.child(
                    gpui_component::resizable::resizable_panel()
                        .size(ai_w_px)
                        .size_range(ai_min_w..ai_max_w)
                        .flex_none()
                        .pl(px(pl))
                        .pr(px(pr))
                        .py(px(CARD_GAP))
                        .child(
                            card_shell(sidebar_bg, card_border)
                                .debug_selector(|| "ai-chat-card".to_string())
                                .child(self.ai_ui.ai_chat_panel.clone()),
                        ),
                );
            }

            group
                .on_resize(move |state, _w, cx| {
                    // 拖某一根分界线只会改动它相邻的两块，另外一块的宽度原样回写
                    // （写回同一个值是无副作用的），所以这里可以把两块**定宽面板**
                    // 的宽度都取一遍，不必知道用户拖的是哪一根。
                    // 侧栏折叠时第 0 块是终端列 —— 那条分支必须跳过，否则会把终端
                    // 宽度当成侧栏宽度存进设置里。
                    //
                    // 侧栏宽度**只记到当前面板名下**（`sidebar_widths[panel_key]`）：
                    // 这是「每个侧边栏的宽度可以不同」的落点，写别的面板名下就等于
                    // 又把它们锁在一起了。`panel_key` 取自本帧渲染的面板，与
                    // `state`（同一面板的 keyed state）天然配对。
                    let sidebar_w = sidebar_visible
                        .then(|| state.read(cx).sizes().first().copied())
                        .flatten();
                    let ai_w = ai_visible
                        .then(|| state.read(cx).sizes().last().copied())
                        .flatten();
                    me_resize.update(cx, |m, cx| {
                        if let Some(w) = sidebar_w {
                            m.settings.remember_sidebar_width(panel_key, f32::from(w));
                        }
                        if let Some(w) = ai_w {
                            m.settings.ai_chat_width = f32::from(w);
                        }
                        let _ = m.settings.save(&paths::settings_path());
                        cx.notify();
                    });
                })
                .into_any_element()
        };

        // 内容行：把`content`放进`h_flex`，折叠/展开两态统一走这里。
        // （快捷命令栏已下移到终端区列内，见上面`pane`的构造处；
        //   底部状态栏 2026-09-17 运行反馈要求移除：指标看概览侧栏即可。）
        let right_col = v_flex()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .child(h_flex().flex_1().min_h_0().items_stretch().child(content));

        // 竖栏（活动栏）在全屏时整条不画：它四个入口指向的都是此刻被藏起来的侧栏
        // 面板，留着只会是四条点了没反应的图标。
        let mut body = h_flex().flex_1().min_h_0().items_stretch();
        if !self.terminal_fullscreen {
            body = body.child(crate::perf_probe::timed(
                &crate::perf_probe::ACTIVITY_US,
                || super::activity_bar::render(self, cx),
            ));
        }
        root = root.child(body.child(right_col));

        // 焦点锚点：本帧没有任何可聚焦终端时补一枚（口径见 `has_focusable_terminal`）。
        //
        // 少了它，`self.panel_ui.focus` 不在派发树里，gpui 会把派发目标退化成派发树根节点，
        // 根 div 上的 `on_action` 全部收不到按键 —— 用户看到的就是
        // 「主窗格为空时命令面板快捷键按了没反应」。
        if !self.has_focusable_terminal() {
            root = root.child(self.render_focus_anchor());
        }

        // 首次信任对话框：poll 无 window，经下一帧在事件上下文中经 Root 层打开
        // （官方 Dialog：焦点陷阱/ESC/遮罩关闭由组件托管）
        if self.conn_ui.pending_trust.is_some() && !self.conn_ui.trust_dialog_open {
            self.conn_ui.trust_dialog_open = true;
            let view = cx.entity();
            window.on_next_frame(move |w, cx| {
                view.update(cx, |m, cx| m.present_trust_dialog(w, cx));
            });
        }

        // 命令面板（浮层，M3 Command 双源）
        if self.panel_ui.palette_state.is_some() {
            root = root.child(self.render_command_palette(cx));
        }

        crate::perf_probe::frame(__perf_t0.elapsed());
        crate::watchdog::leave();
        root
    }
}

#[cfg(test)]
mod tests {
    use crate::test_support::prod;

    /// 回归（2026-10-09 用户要求「隐藏标题栏的下方的线」）。
    ///
    /// 那条线**不是本文件画的**：官方 `TitleBar` 的渲染链上自带
    /// `.border_b_1().border_color(theme.title_bar_border)`
    /// （gpui-component-0.6.4/src/title_bar.rs），横贯整个窗口宽度。
    /// 本应用标题栏底色（`bg_base`）与下方内容同色，分界交给留白即可，
    /// 故在调用点用 `.border_b_0()` 覆盖：官方在链尾才
    /// `refine_style(&self.style)`，调用点写进去的 `Some(px(0.))` 会盖掉官方
    /// 那个 1px（与紧邻的 `.h(..)` 覆盖官方默认高 34px 是**同一条机理**），
    /// 宽度归零后官方 `is_border_visible()` 会整条跳过这条边的绘制。
    ///
    /// 为什么钉构造链、而不是量几何：官方 `TitleBar` 只挂了 `.id("title-bar")`、
    /// **没有** `debug_selector`，而 `debug_bounds` 只认后者（`id()` 不算）⇒
    /// 测试里量不到它的外框高度，只能退到源码结构锚点。
    #[test]
    fn title_bar_bottom_border_is_hidden() {
        let code = prod(include_str!("main_view.rs"));
        let chain = crate::test_support::region(
            &code,
            "gpui_component::TitleBar::new()",
            r#".child("RustShell")"#,
        );
        assert!(
            chain.contains(".border_b_0()"),
            "标题栏下边框没关掉：官方 `TitleBar` 自带一条 1px 下边框，\
             不覆盖就会在标题栏下方留一条横线（用户 2026-10-09 要求隐藏）"
        );
        assert!(!chain.contains(".border_b_1()"), "标题栏下边框又被画回来了");
    }

    /// 回归（2026-10-09 用户要求「搜索命令框往下一点」，见 `TITLE_BAR_CMD_DY`）。
    ///
    /// 钉**行为**而不是常量字面量：真渲染一帧后量命令入口的竖向中心，它必须落在
    /// 「标题栏中线 + `TITLE_BAR_CMD_DY`」上。只钉源码锚点（"链上有 `.top(..)`"）
    /// 不够 —— 相对偏移要**真生效**才算数，而它完全可能被上游抵消（父容器的
    /// `items_center` 把外框摆回中间、或盒子被包进另一层时 `top` 落在了外层），
    /// 那时源码看着没变、用户看到的位置却回去了。
    ///
    /// 容差 ±0.75：官方 `TitleBar` 那条 1px 下边框算在 28px 之内还是之外
    /// （`h()` 的盒模型语义）会让"中线"差 0.5px，两种解释都必须过。
    /// 注意**不能**改用 `prod()` 扫源码：`prod` 从 `#[cfg(test)]` 起截断，
    /// 而这段断言与 `debug_bounds` 都活在测试模块里。
    #[gpui::test]
    fn title_bar_command_entry_sits_below_the_bar_centre(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;
        use std::cell::RefCell;
        use std::rc::Rc;

        use super::{AppShell, MainView, TITLE_BAR_CMD_DY, TITLE_BAR_HEIGHT};

        cx.update(|cx| {
            gpui_component::init(cx);
            crate::theme::apply_theme_mode(cx, rustshell_core::theme::ThemeMode::Dark);
        });

        let slot: Rc<RefCell<Option<gpui::Entity<MainView>>>> = Rc::new(RefCell::new(None));
        let (_, vcx) = {
            let slot = slot.clone();
            cx.add_window_view(move |window, cx| {
                let main = cx.new(|cx| MainView::new(window, cx));
                *slot.borrow_mut() = Some(main.clone());
                let shell = cx.new(|_| AppShell::new(main));
                gpui_component::Root::new(shell, window, cx)
            })
        };
        vcx.run_until_parked();

        let b = vcx.debug_bounds("titlebar-command-entry").expect(
            "标题栏里量不到命令入口 —— 缺 `titlebar-command-entry` 的 `debug_selector`？\
             （`debug_bounds` 只认它，`.id()` 不算）",
        );
        let centre: f32 = (b.origin.y + b.size.height / 2.0).into();
        let want = TITLE_BAR_HEIGHT / 2.0 + TITLE_BAR_CMD_DY;
        assert!(
            (centre - want).abs() <= 0.75,
            "命令入口的竖向中心在 {centre}px，期望 ≈{want}px（标题栏中线 {}px + 下移 {TITLE_BAR_CMD_DY}px）—— \
             偏回 14px 附近说明那处 `top` 没生效或被上游对齐抵消了",
            TITLE_BAR_HEIGHT / 2.0
        );
    }

    /// 行为护栏（2026-09-21 用户要求）：
    /// 1. 「最左侧图标栏宽度减小五分之一」—— 真渲染出来必须是 38.4px；
    /// 2. 图标**次序固定**为 `PanelKind::ALL`（同日「这个排序回退」）。
    ///
    /// 为什么不止步于源码扫描：`.w(px(RAIL_W))` 在场照样可能被下游抵消
    /// （父容器定宽、flex 压缩）；而「次序固定」本身就是行为断言 —— 源码里
    /// 遍历固定表，仍可能被别处的排序/重建打乱。所以量真几何：栏宽量
    /// `activity-rail`，次序量每枚图标自己的 `rail-<panel-key>` 纵坐标，
    /// 并且**切换面板前后各量一次**，两次必须一致。
    #[gpui::test]
    fn activity_rail_is_narrower_and_keeps_a_fixed_order(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;
        use std::cell::RefCell;
        use std::rc::Rc;

        use super::{AppShell, MainView, PanelKind};

        /// 图标栏自上而下的**固定**次序（= `PanelKind::ALL` 的 key 次序）。
        const RAIL_KEYS: [&str; 6] = [
            "rail-dashboard",
            "rail-connections",
            "rail-terminals",
            "rail-files",
            "rail-ai",
            "rail-tasks",
        ];

        cx.update(|cx| {
            gpui_component::init(cx);
            crate::theme::apply_theme_mode(cx, rustshell_core::theme::ThemeMode::Dark);
        });

        let slot: Rc<RefCell<Option<gpui::Entity<MainView>>>> = Rc::new(RefCell::new(None));
        let (_, vcx) = {
            let slot = slot.clone();
            cx.add_window_view(move |window, cx| {
                let main = cx.new(|cx| MainView::new(window, cx));
                *slot.borrow_mut() = Some(main.clone());
                let shell = cx.new(|_| AppShell::new(main));
                gpui_component::Root::new(shell, window, cx)
            })
        };
        vcx.run_until_parked();
        let main = slot.borrow().clone().expect("MainView 未创建");

        // ① 栏宽：48px 的 4/5
        let rail_w: f32 = vcx
            .debug_bounds("activity-rail")
            .expect("找不到图标栏（`activity-rail` debug_selector）")
            .size
            .width
            .into();
        assert!(
            (rail_w - 38.4).abs() <= 1.0,
            "图标栏实宽 {rail_w}px，期望 38.4px（原 48px 的 4/5）"
        );

        // ② 次序：六枚图标的纵坐标必须与 `RAIL_KEYS` 同序（自上而下严格递增）
        let ys = |vcx: &mut gpui::VisualTestContext| -> Vec<f32> {
            RAIL_KEYS
                .iter()
                .map(|sel| {
                    vcx.debug_bounds(sel)
                        .unwrap_or_else(|| panic!("图标栏里找不到 `{sel}`"))
                        .origin
                        .y
                        .into()
                })
                .collect()
        };
        let before = ys(vcx);
        for (i, w) in before.windows(2).enumerate() {
            assert!(
                w[0] < w[1],
                "第 {i} 格应排在它后面那格之上 —— 图标次序必须等于 `PanelKind::ALL`，\
                 实际纵坐标 {before:?}"
            );
        }

        // ③ 切一块面板：图标位置必须**一动不动**。这正是「排序回退」要钉的东西 ——
        //    按最近访问重排时，刚切过去的面板会跳到第一格，用户再也记不住
        //    「第几格是哪块面板」。
        vcx.update(|_w, cx| {
            main.update(cx, |m, cx| {
                m.panel_ui.sidebar_collapsed = false;
                m.panel_ui.panel = PanelKind::Ai;
                cx.notify();
            })
        });
        vcx.run_until_parked();
        let after = ys(vcx);
        for (i, (a, b)) in before.iter().zip(after.iter()).enumerate() {
            assert!(
                (a - b).abs() <= 0.5,
                "切换面板后第 {i} 格图标的纵坐标从 {a} 变成 {b} —— 图标栏又按最近访问\
                 重排了（用户 2026-09-21 要求回退）。"
            );
        }
    }

    /// 回归（2026-09-21 用户要求「隐藏终端界面与左右两侧相间的线条」）。
    ///
    /// 面板组必须接过手柄外观，并且 renderer 要返回 `Some(<空元素>)`。
    /// 返回 `None` 时 gpui-base 的 `ResizeHandle` 会走 `unwrap_or_else` 分支
    /// 画回**内建那条线**（见 `resize_handle.rs`），用户看到的就是原来那条竖线。
    ///
    /// 注意只换「画出来的那一笔」：命中区 / `col-resize` 光标 / 拖拽都在库内，
    /// 所以隐藏线之后分界线照样能拖 —— 别把它当成「顺带禁用了拖拽」的改动。
    #[test]
    fn main_split_paints_no_divider_between_panels() {
        let code = prod(include_str!("main_view.rs"));
        let at = code
            .find(".with_handle_appearance(")
            .expect("面板组必须用 `with_handle_appearance` 接管分隔线外观（用户要求隐藏它）");
        let seg = &code[at..(at + 400).min(code.len())];
        assert!(
            seg.contains("Some(div().into_any_element())"),
            "手柄 renderer 必须返回 `Some(<空元素>)`；返回 `None` 等于没关，\
             内建那条 1px 竖线会照原样画回来。片段：{seg}"
        );
    }

    /// 命令窗口的每一条内置命令都必须**接到底**：面板里的 `action_id` 有两处映射，
    /// 少一处就是「点了没反应」——
    /// 1. `keybind_actions` 里 `action_id → Action` 的映射（漏了会静默落到
    ///    `_ => Box::new(CommandPalette)`，确认时打开的是命令面板自己）；
    /// 2. 根节点上的 `on_action` 处理器（漏了 Action 派发出去没人接）。
    ///
    /// 2026-09-17 运行反馈「命令窗口新增【新增连接】【AI助手】」：这两条正是新加的，
    /// 两处都要在，所以这条护栏顺带把「新加一条命令要改哪些地方」写死在用例里。
    ///
    /// 2026-09-20：第 1 步的映射**允许改名** —— `NewLocalShell` 派发到 `NewTab`
    /// （见 `keybind_actions` 的注释），面板里的 id 与 `Action` 类型不必同名。
    /// 所以这里先抠出面板真正 `Box::new(..)` 的那个动作，再去根节点找它的处理器：
    /// 断言的是「**被派发的那个 Action** 有人接」，而不是「id 与类型同名」。
    /// 曾经写成 `main.contains("&{id},")`：`NewLocalShell` 一进面板就报红，
    /// 而实际上面板派发的是 `NewTab`、处理器一直都在。
    #[test]
    fn every_palette_command_is_wired_to_its_action() {
        let main = prod(include_str!("main_view.rs"));
        let palette = prod(include_str!("main_view/keybind_actions.rs"));
        for cmd in rustshell_core::command::builtin_commands() {
            let id = cmd.action_id.expect("内置命令必须带 action_id");
            let dispatched = dispatched_action(&palette, id).unwrap_or_else(|| {
                panic!(
                    "命令「{}」({id}) 没在命令面板的 action 映射里 —— 确认时会被静默当成\
                     「打开命令面板」。",
                    cmd.label
                )
            });
            assert!(
                main.contains(&format!("&{dispatched},")),
                "命令「{}」({id}) 派发到 `{dispatched}`，但根节点上没有它的 `on_action` \
                 —— 面板派发的 Action 没有人接。",
                cmd.label
            );
        }
    }

    /// 从面板源码里抠出 `"<id>" => Box::new(<Action>)` 中的 `<Action>`。
    ///
    /// 面板按**字符串 id** 分发，一条 id 允许指到别的动作（`NewLocalShell` → `NewTab`），
    /// 所以护栏不能假定「id 就是 Action 名」，得看源码里真正 `Box::new` 了什么 ——
    /// 否则「复用已有处理器」这种正当写法会被当成「没接线」。
    /// 返回 `None` = 这条 id 压根不在映射表里（调用方据此报「会被静默当成打开面板」）。
    fn dispatched_action(palette: &str, id: &str) -> Option<String> {
        let needle = format!("\"{id}\" => Box::new(");
        let rest = &palette[palette.find(&needle)? + needle.len()..];
        let name = rest[..rest.find(')')?].trim();
        (!name.is_empty()).then(|| name.to_string())
    }

    /// 回归（2026-09-28 改写）：
    /// 启动动作必须**由设置驱动**，不得再硬编码成「永远只开命令面板」。
    ///
    /// 历史：2026-09-17 的运行反馈是「打开软件默认不打开本地 shell，打开命令窗口」，
    /// 于是启动被写死为永远只开命令面板 —— 那是用一个人的偏好覆盖所有人。
    /// 一个终端软件启动后给一个终端是最不需要解释的行为，现在默认
    /// `StartupAction::LocalShell`，其余档位在设置里选。
    ///
    /// 扫的是 `shell/src/app.rs`（启动顺序在本仓的这一处），钉住两条：
    /// ① 启动走**分派函数**（而不是某一条写死的入口）；② 那个分派按设置取值。
    #[test]
    fn startup_action_is_driven_by_settings() {
        let code = prod(include_str!("../../shell/src/app.rs"));
        assert!(
            code.contains("ui_call_startup_action"),
            "启动路径必须走 `ui_call_startup_action` 分派 —— 写死成某一条入口\
             （如只开命令面板）就是用一个人的偏好覆盖所有人。"
        );
        assert!(
            code.contains("settings.startup_action"),
            "启动分派必须把用户设置传进去，否则分派函数只能吃一个固定值。"
        );

        // 分派函数本身：四个档位都必须有明确落点，且默认档是「开本地终端」。
        let dispatch = prod(include_str!("main_view.rs"));
        for branch in [
            "StartupAction::LocalShell",
            "StartupAction::CommandPalette",
            "StartupAction::RestoreSession",
            "StartupAction::Nothing",
        ] {
            assert!(
                dispatch.contains(branch),
                "启动分派漏了 {branch} 这一档 —— 选了它的用户会得到一个空窗口。"
            );
        }
        // 默认值：终端软件启动后给一个终端
        assert_eq!(
            rustshell_core::settings::StartupAction::default(),
            rustshell_core::settings::StartupAction::LocalShell
        );
    }

    /// 回归护栏（2026-09-20 用户要求「默认记住每个侧边栏的宽度，每个侧边栏的宽度可以不同」）。
    ///
    /// 守的是**三条链路必须同时成立**，缺一条这个需求就等于没做：
    ///
    /// 1. 宽度按面板取（`sidebar_width_for(self.panel_ui.panel.key())`）—— 取错就还是共用；
    /// 2. 面板组 id 按面板分叉（`main_split_id`）—— gpui-base 的面板组把宽度存在
    ///    `use_keyed_state(id)` 里，同 id 下 `resizable_panel().size(..)` 只首帧生效，
    ///    换了面板宽度也不会跟着换（静默失效，不报错）；
    /// 3. 拖拽后写回**当前面板**名下（`remember_sidebar_width(panel_key, ..)`）——
    ///    写别的名字等于又把六个面板锁在一起。
    ///
    /// 2026-09-22 加终端全屏时实测出第四条：id 里还得带**哪几列在显示**。
    /// 面板组的 slot 按位置存，列数一变就张冠李戴，容器变宽时还被按比例重算，
    /// 退出全屏时侧栏被撑到上限、终端被挤窄（推导见 `main_split_id`）。
    /// 这条的行为面由 `terminal_fullscreen_hides_the_chrome_and_puts_it_back` 量实宽。
    #[test]
    fn sidebar_width_is_keyed_per_panel() {
        let code = prod(include_str!("main_view.rs"));

        assert!(
            code.contains(
                "fn main_split_id(panel: PanelKind, sidebar_visible: bool, ai_visible: bool) -> ElementId"
            ),
            "面板组 id 必须按「侧栏面板 + 哪几列在显示」分叉（`main_split_id`）：只按面板\n\
             分叉的话，换面板共用宽度（`size(..)` 只在首帧生效）、换列数张冠李戴。"
        );
        // 三个占位符：`{}` 面板 key + 两个可见位。少一个就说明又退回「按列数不变」
        // 的假设了（占位符个数与实参对不上会编译失败，所以这条查得住）。
        assert!(
            code.contains(r#""main-split:{}:{}{}""#),
            "面板组 id 必须把面板 key 与两个列可见性都写进去 —— id 就是 `use_keyed_state` 的键。"
        );
        assert!(
            code.contains("main_split_id(self.panel_ui.panel, sidebar_visible, ai_visible)"),
            "建面板组时必须把**本帧**的两个列可见性传进 `main_split_id`：传常量或传\n\
             `self.panel_ui.sidebar_collapsed`（不含全屏）都会让全屏进出落在同一个 state 上。"
        );
        assert!(
            code.contains("sidebar_width_for(self.panel_ui.panel.key())"),
            "侧栏宽度必须**按当前面板**取（`sidebar_width_for(self.panel_ui.panel.key())`）。"
        );
        assert!(
            code.contains("let panel_key = self.panel_ui.panel.key();"),
            "写回用的 `panel_key` 必须取自本帧的 `self.panel_ui.panel` —— 写死或取错面板\n\
             等于把六个面板的宽度又锁在一起。"
        );
        assert!(
            code.contains("remember_sidebar_width(panel_key"),
            "拖拽结束必须把宽度写回**当前面板**名下（`remember_sidebar_width(panel_key, ..)`）。"
        );

        // 反面：旧写法（六个面板共用一个宽度）不得回来。
        assert!(
            !code.contains("m.settings.sidebar_width ="),
            "不得再把侧栏宽度写进共用的 `settings.sidebar_width`：那是「所有面板同一个\n\
             宽度」的旧行为，`Remember` 到统一兜底字段上等于把面板又锁在一起。"
        );
    }

    /// `PanelKind::key()` 是**落盘键**：必须两两不同、且不随界面语言变化。
    ///
    /// 这条不是形式主义 —— `sidebar_widths` 的键会用 `"connections"` 这样的字面量写进
    /// `settings.json`；若有人图省事改成复用自己的 `label()`，中文环境下键就是 `"连接"`、
    /// 英文环境下是 `"Connections"`，用户切一次语言，拖出来的宽度就全查不回来了。
    #[test]
    fn panel_keys_are_distinct_and_language_independent() {
        use super::PanelKind;

        let keys: Vec<&str> = PanelKind::ALL.iter().map(|p| p.key()).collect();
        let mut sorted = keys.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), keys.len(), "面板 key 有重复：{keys:?}");

        for p in PanelKind::ALL {
            let k = p.key();
            assert!(
                !k.is_empty() && k.is_ascii(),
                "面板 key 必须是 ASCII 稳定标识（落盘用），实际 {k:?}"
            );
        }

        // 中文环境下 `label()` 出中文、`key()` 必须仍出 ASCII —— 两者不许同源。
        assert_ne!(
            PanelKind::Connections.key(),
            PanelKind::Connections.label(),
            "面板 key 不得与 i18n 的 label 同源：label 会随界面语言变，拿它当\n\
             `settings.json` 的键 ⇒ 切一次语言就查不回用户拖出来的宽度。"
        );
        assert_eq!(PanelKind::Connections.key(), "connections");
    }

    /// 行为护栏：同一窗口里，两个侧栏面板各自记着不同宽度 ⇒ 切面板必须真的换宽度。
    ///
    /// 上面的结构护栏只能证明「代码长这样」，证明不了「切面板时宽度真的换了」——
    /// 面板组缓存 keyed state 这一层是**静默失效**的（不报错、不 panic、单测全绿），
    /// 所以这里用 `debug_bounds` 量侧栏卡片的**实宽**。
    #[gpui::test]
    fn switching_sidebar_panels_swaps_the_measured_width(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;
        use std::cell::RefCell;
        use std::rc::Rc;

        use super::{AppShell, MainView, PanelKind};

        cx.update(|cx| {
            gpui_component::init(cx);
            crate::theme::apply_theme_mode(cx, rustshell_core::theme::ThemeMode::Dark);
        });

        let slot: Rc<RefCell<Option<gpui::Entity<MainView>>>> = Rc::new(RefCell::new(None));
        // 面板组会**延迟一帧**结算（容器尺寸变化时 `window.defer` 补一次 notify），
        // 量宽前必须 `run_until_parked()` 让它稳定下来，否则量到的是中间态。
        //
        // `add_window_view` 返回的第二个元素是 **`&mut VisualTestContext`**（不是 owned），
        // 所以下面一律按「可变引用」用它，不要再叠 `&mut`。
        let (_, vcx) = {
            let slot = slot.clone();
            cx.add_window_view(move |window, cx| {
                let main = cx.new(|cx| MainView::new(window, cx));
                *slot.borrow_mut() = Some(main.clone());
                let shell = cx.new(|_| AppShell::new(main));
                gpui_component::Root::new(shell, window, cx)
            })
        };
        vcx.run_until_parked();
        let main = slot.borrow().clone().expect("MainView 未创建");

        // 两个面板的**已记忆宽度**（都在 `SIDEBAR_WIDTH_MIN..SIDEBAR_WIDTH_MAX` 内）。
        //
        // 刻意选两个**都不是启动面板**的面板：启动面板是 `Connections`，它的 keyed
        // state 在首帧就已经按当时的宽度钉住了（这正是 `resizable_panel().size(..)`
        // 只在首帧生效的直接后果，见 `main_split_id`），拿它量会量到旧值。
        // 兜底值 `sidebar_width` 刻意设成第三个不相干的数字：两个面板都必须用它
        // **自己**记的那份，而不是回落到兜底。
        const FILES_W: f32 = 380.0;
        const TASKS_W: f32 = 200.0;
        const FALLBACK_W: f32 = 240.0;
        vcx.update(|_w, cx| {
            main.update(cx, |m, cx| {
                m.panel_ui.sidebar_collapsed = false;
                m.ai_ui.ai_chat_open = false;
                m.settings.sidebar_width = FALLBACK_W;
                m.settings.sidebar_widths.clear();
                m.settings
                    .remember_sidebar_width(PanelKind::Files.key(), FILES_W);
                m.settings
                    .remember_sidebar_width(PanelKind::Tasks.key(), TASKS_W);
                m.panel_ui.panel = PanelKind::Files;
                cx.notify();
            })
        });
        vcx.run_until_parked();

        let card_w = |vcx: &mut gpui::VisualTestContext| -> f32 {
            vcx.debug_bounds("sidebar-card")
                .expect("找不到侧栏卡片（`sidebar-card` debug_selector）")
                .size
                .width
                .into()
        };

        // 卡片宽度 = 面板宽度 − 面板左右留白（首块 pl=CARD_GAP、pr=CARD_GAP*0.5，
        // 见 render 里的 `pad`）。两边同减同一个常数 ⇒ 差值就是两个面板宽度之差。
        const CARD_INSET: f32 = super::CARD_GAP * 1.5;

        let w_files = card_w(vcx);

        // 切到「定时」—— 宽度必须换成它自己记的 200，而不是沿用「文件」的 380。
        vcx.update(|_w, cx| {
            main.update(cx, |m, cx| {
                m.panel_ui.panel = PanelKind::Tasks;
                cx.notify();
            })
        });
        vcx.run_until_parked();
        let w_tasks = card_w(vcx);

        let delta = w_files - w_tasks;
        assert!(
            (delta - (FILES_W - TASKS_W)).abs() <= 3.0,
            "切侧栏面板后宽度没有跟着换：files={w_files:?} tasks={w_tasks:?}\n\
             （差 {delta:?}，期望 {:?}）。多半是面板组仍共用一个 keyed state ——\n\
             `resizable_panel().size(..)` 只在首帧生效，之后再改也不参与布局。",
            FILES_W - TASKS_W
        );

        // 绝对值也要对：各自用的必须是**自己**记的那份，而不是统一兜底值。
        assert!(
            (w_files - (FILES_W - CARD_INSET)).abs() <= 3.0,
            "「文件」面板宽度不是它自己记的 {FILES_W:?}（实宽 {w_files:?}，\
             统一兜底值 {FALLBACK_W:?} 对应的卡片宽是 {:?}）",
            FALLBACK_W - CARD_INSET
        );
        assert!(
            (w_tasks - (TASKS_W - CARD_INSET)).abs() <= 3.0,
            "「定时」面板宽度不是它自己记的 {TASKS_W:?}（实宽 {w_tasks:?}）"
        );

        // 切回来必须回到原来那个宽度（每个面板各记各的，不互相污染）。
        vcx.update(|_w, cx| {
            main.update(cx, |m, cx| {
                m.panel_ui.panel = PanelKind::Files;
                cx.notify();
            })
        });
        vcx.run_until_parked();
        let w_back = card_w(vcx);
        assert!(
            (w_back - w_files).abs() <= 1.0,
            "切回「文件」后宽度没有回到 {w_files:?}，实际 {w_back:?} ——\n\
             两个面板的记忆互相污染了。"
        );
    }

    /// 终端全屏：全屏时竖栏 / 侧栏 / AI 对话列整列不画，终端卡片铺满；
    /// 退出后**原样**还回来 —— 三列都回来，宽度也回到基线。
    ///
    /// 「宽度回到基线」这一条是 2026-09-22 实机量出来的坑：面板组的 slot 是按位置
    /// 存的，列数一变就张冠李戴，gpui-base 又会按容器宽度比例重算这些 slot
    /// （全屏时竖栏让位、容器变宽），于是退出全屏时侧栏被撑到上限、终端被挤窄。
    /// 推导与修法见 `main_split_id`。这里用实宽把它钉住。
    ///
    /// 末段是另一条独立的重点：全屏只许改 `terminal_fullscreen`，不许顺手动
    /// `sidebar_collapsed` / `ai_chat_open`。那两个是用户自己的布局意图 ——
    /// 被全屏改写的话，用户退出时会发现侧栏或 AI 列凭空消失，且找不到是哪一步弄丢的。
    #[gpui::test]
    fn terminal_fullscreen_hides_the_chrome_and_puts_it_back(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;
        use std::cell::RefCell;
        use std::rc::Rc;

        use super::{AppShell, MainView};

        cx.update(|cx| {
            gpui_component::init(cx);
            crate::theme::apply_theme_mode(cx, rustshell_core::theme::ThemeMode::Dark);
        });

        let slot: Rc<RefCell<Option<gpui::Entity<MainView>>>> = Rc::new(RefCell::new(None));
        let (_, vcx) = {
            let slot = slot.clone();
            cx.add_window_view(move |window, cx| {
                let main = cx.new(|cx| MainView::new(window, cx));
                *slot.borrow_mut() = Some(main.clone());
                let shell = cx.new(|_| AppShell::new(main));
                gpui_component::Root::new(shell, window, cx)
            })
        };
        vcx.run_until_parked();
        let main = slot.borrow().clone().expect("MainView 未创建");

        // 基线：侧栏与 AI 对话列都开着（全屏要藏的就是这两列 + 竖栏）。
        vcx.update(|_w, cx| {
            main.update(cx, |m, cx| {
                m.panel_ui.sidebar_collapsed = false;
                m.ai_ui.ai_chat_open = true;
                cx.notify();
            })
        });
        vcx.run_until_parked();

        let present =
            |vcx: &mut gpui::VisualTestContext, sel: &'static str| vcx.debug_bounds(sel).is_some();
        let w_of = |vcx: &mut gpui::VisualTestContext, sel: &'static str| -> f32 {
            vcx.debug_bounds(sel)
                .unwrap_or_else(|| panic!("找不到 {sel}（debug_selector）"))
                .size
                .width
                .into()
        };

        assert!(present(vcx, "sidebar-card"), "基线：侧栏当在");
        assert!(present(vcx, "ai-chat-card"), "基线：AI 对话列当在");
        assert!(present(vcx, "activity-rail"), "基线：竖栏当在");
        let term_normal = w_of(vcx, "terminal-card");
        let sidebar_normal = w_of(vcx, "sidebar-card");
        let ai_normal = w_of(vcx, "ai-chat-card");

        // 进全屏
        vcx.update(|w, cx| main.update(cx, |m, cx| m.toggle_terminal_fullscreen(w, cx)));
        vcx.run_until_parked();

        assert!(!present(vcx, "sidebar-card"), "全屏下侧栏应当整列不画");
        assert!(
            !present(vcx, "ai-chat-card"),
            "全屏下 AI 对话列应当整列不画"
        );
        assert!(!present(vcx, "activity-rail"), "全屏下竖栏应当整条不画");
        let term_full = w_of(vcx, "terminal-card");
        assert!(
            term_full > term_normal + 100.0,
            "全屏下终端卡片没有铺满：{term_normal:?} → {term_full:?}"
        );
        assert!(
            present(vcx, "toggle-terminal-fullscreen"),
            "全屏下那枚图标必须还在 —— 它是唯一的鼠标出口"
        );

        // 退出全屏：三列原样回来，宽度回到基线
        vcx.update(|w, cx| main.update(cx, |m, cx| m.toggle_terminal_fullscreen(w, cx)));
        vcx.run_until_parked();

        assert!(present(vcx, "sidebar-card"), "退出全屏后侧栏没回来");
        assert!(present(vcx, "ai-chat-card"), "退出全屏后 AI 对话列没回来");
        assert!(present(vcx, "activity-rail"), "退出全屏后竖栏没回来");
        let term_back = w_of(vcx, "terminal-card");
        assert!(
            (term_back - term_normal).abs() <= 1.0,
            "退出全屏后终端宽度没回到基线：{term_normal:?} → {term_back:?}"
        );
        assert!(
            (w_of(vcx, "sidebar-card") - sidebar_normal).abs() <= 1.0,
            "退出全屏后侧栏宽度没回到基线：{sidebar_normal:?} → {:?}",
            w_of(vcx, "sidebar-card")
        );
        assert!(
            (w_of(vcx, "ai-chat-card") - ai_normal).abs() <= 1.0,
            "退出全屏后 AI 对话列宽度没回到基线：{ai_normal:?} → {:?}",
            w_of(vcx, "ai-chat-card")
        );

        // 同一类坑的第二条入口：折叠侧栏也是「换拓扑」，往返一次同样不许改宽度。
        // （全屏那一段的坑就是从这里复制出来的，见 `main_split_id` 的推导。）
        for collapsed in [true, false] {
            vcx.update(|_w, cx| {
                main.update(cx, |m, cx| {
                    m.panel_ui.sidebar_collapsed = collapsed;
                    cx.notify();
                })
            });
            vcx.run_until_parked();
        }
        assert!(
            (w_of(vcx, "sidebar-card") - sidebar_normal).abs() <= 1.0,
            "折叠再展开侧栏后宽度没回到基线：{sidebar_normal:?} → {:?}",
            w_of(vcx, "sidebar-card")
        );
        assert!(
            (w_of(vcx, "terminal-card") - term_normal).abs() <= 1.0,
            "折叠再展开侧栏后终端宽度没回到基线：{term_normal:?} → {:?}",
            w_of(vcx, "terminal-card")
        );

        // 用户自己的布局意图没被全屏改写（`terminal_fullscreen` 只负责全屏本身）
        vcx.update(|_w, cx| {
            main.update(cx, |m, _cx| {
                assert!(!m.terminal_fullscreen, "退出后全屏标记当复位");
                assert!(!m.panel_ui.sidebar_collapsed, "全屏不该顺手折叠侧栏");
                assert!(m.ai_ui.ai_chat_open, "全屏不该顺手关掉 AI 对话列");
            })
        });
    }

    /// 回归护栏（N7 崩溃）：`MainView`的视图级焦点 handle 只能有一个认领者。
    #[test]
    fn view_focus_handle_is_claimed_by_exactly_one_element() {
        let code = prod(include_str!("main_view.rs"));

        // 结构锚点：Render 实现 到 内容区构建（T-5 引入的`let content = {`）。
        let render_at = code
            .find("impl Render for MainView")
            .expect("找不到 Render 实现");
        let body = &code[render_at..];
        let end = body
            .find("let content = {")
            .expect("找不到内容区构建起点（T-5 结构锚点）");
        let root_build = &body[..end];

        // 1) 根节点不得认领`self.panel_ui.focus`。
        // 只看代码（注释已剥）：`self.panel_ui.focus`出现在 track_focus 参数位即为违规。
        //`self.panel_ui.focus`仍可出现在别处（如`Focusable::focus_handle`返回它）。
        let bad = root_build
            .match_indices("track_focus")
            .any(|(i, _)| root_build[i..].contains("track_focus(&self.panel_ui.focus)"));
        assert!(
            !bad,
            "根节点不得 track_focus 到 self.panel_ui.focus：MainView 已实现 Focusable，\n\
             同一 FocusId 被两个元素认领会在 debug + a11y active 时触发 gpui 的\n\
             panic!(\"set_focus called more than once in a single frame\")（崩溃 N7）。"
        );

        // 2) 内容列必须真的位于`ResizablePanelGroup`的某个面板内，
        // 否则内容被挤出面板组 到 宽度塌成 0 到 「终端 tab 页看不到」。
        //
        // 2026-09-18 卡片化（圆角）之后 `pane` 不再直接挂在面板上，而是包在卡片壳
        // 里（`card_shell(..).child(pane)`），所以断言从「逐字面量相等」放宽成
        // **结构位置**：从面板组起点到 `.child(pane)` 之间必须出现过
        // `resizable_panel()`。这样既放过卡片壳这一层包装，又仍然拦住原本要防的
        // 写法 —— 把 `pane` 追加成面板组**之后**的兄弟节点。
        // 2026-09-20：面板组 id 改为按侧栏面板分叉（`main_split_id(self.panel_ui.panel, ..)`，
        // 见该函数注释），锚点随之从字面量 `h_resizable("main-split")` 放宽成
        // `h_resizable(` —— 本文件里只有面板组这一处用 `h_resizable`（终端分屏那处
        // 在 `main_view/split_actions.rs`），仍然唯一定位到面板组起点。
        let resizable_at = body.find("h_resizable(").expect("找不到 main-split 面板组");
        let after = &body[resizable_at..];
        let pane_at = after.find(".child(pane)").expect(
            "内容列 pane 没有被任何面板接管（面板组里找不到 `.child(pane)`）：\n\
             它会退回成面板组的兄弟节点，右面板空置、内容塌成 0 宽（崩溃 N7 / 偏差 D-7）。",
        );
        assert!(
            after[..pane_at].contains("resizable_panel()"),
            "内容列 pane 必须是 ResizablePanelGroup 内某个面板的子节点：\n\
             该面板组自成一个 flex 容器，不会接收调用方在其后追加的兄弟节点；\n\
             写成兄弟节点会让右面板空置、内容塌成 0 宽（崩溃 N7 / 偏差 D-7）。"
        );
        assert!(
            after.contains(".pl(px(pl))") && after.contains(".pr(px(pr))"),
            "面板必须自带左右留白（卡间空隙由**面板 padding** 产生，而不是外层容器 padding）：\n\
             外层 padding 会把面板连同它 absolute 定位的手柄一起挤窄。"
        );
        // 留白必须左右**对半**分：可见的 1px 分界线画在面板左缘这条线上
        // （`left: -4px` 的外移被手里那 4px 内边距抵掉），把整段空隙记在左卡
        // `pr` 上会让分界线压在右卡的描边上 —— 实机像素证据见上面 `pad` 的注释。
        assert!(
            body.contains("let half = CARD_GAP * 0.5;"),
            "卡间空隙必须左右各分一半（`CARD_GAP * 0.5`）：分界线落在面板左缘，\n\
             全记在左卡 `pr` 会让它盖住右卡片的 1px 描边（实机取像素 `#262626`\n\
             压住 `#30363D`），看起来像卡片缺了一条边。"
        );

        // 3) 快捷命令栏必须落在`pane`（终端区列）内 —— 用户要求「与终端区同宽」。
        //`pane`在`h_resizable`之前构建，故「面板组起点之后不得再出现命令栏」，
        // 出现在那里只可能是回落成了`right_col`的子节点（横跨侧栏 + 终端区）。
        // （perf_probe::timed 包装只包住耗时统计，调用点字面量允许带包装。）
        assert!(
            body.contains("quick_commands_bar::render"),
            "快捷命令栏必须渲染在 pane（终端区列）里，否则无法与终端区同宽。"
        );
        assert!(
            !after.contains("quick_commands_bar::render"),
            "快捷命令栏不得挂在 right_col 下：right_col 位于 ResizablePanelGroup 之外，\n\
             会把命令栏横跨到侧栏下方，与「和终端区保持同一宽度」相冲突。"
        );
    }

    /// 回归护栏（2026-09-17 运行反馈「主窗格为空界面，命令窗口快捷键无法使用」）。
    ///
    /// 根因不在按键绑定，而在**派发路径**：终端视图是全应用唯一对元素调
    /// `track_focus` 的地方。没有终端时 `window.focus` 指向的 `self.panel_ui.focus`
    /// 在派发树里没有节点，gpui 的 `focus_node_id_in_rendered_frame` 退化成
    /// 派发树根节点，`dispatch_path` 只剩根节点自身 —— 根 div 上的 `on_action`
    /// 一个都收不到。修法是补一枚焦点锚点，且必须与终端**互斥**（同帧两个
    /// 认领者 = gpui 的 `set_focus called more than once` panic，崩溃 N7）。
    #[test]
    fn focus_anchor_keeps_root_actions_reachable_without_a_terminal() {
        let split = prod(include_str!("main_view/split_actions.rs"));
        let render = prod(include_str!("main_view.rs"));

        // 1) 锚点真的把 self.panel_ui.focus 注册进派发树。
        assert!(
            split.contains("track_focus(&self.panel_ui.focus)"),
            "焦点锚点必须把 self.panel_ui.focus 注册进派发树，否则空窗格/连接态下\n\
             根 div 的 on_action（命令面板等）全部收不到按键。"
        );

        // 2) 锚点零尺寸、无命中处理：不参与画面，也不吃鼠标。
        assert!(
            split.contains(r#".id("focus-anchor")"#) && split.contains(".w(px(0.))"),
            "焦点锚点必须是零尺寸、无命中处理的透明节点（不吃鼠标、不改布局）。"
        );

        // 3) 互斥：只在「本帧没有可聚焦终端」时挂。
        assert!(
            render.contains("if !self.has_focusable_terminal() {"),
            "锚点必须挂在 !has_focusable_terminal() 分支下 —— 常驻挂载会让它与终端\n\
             同帧各自 a11y.set_focus，触发 debug panic（崩溃 N7）。"
        );

        // 4) 判定口径必须覆盖 render_workspace 的全部派发分支（分屏两窗格）。
        assert!(
            split.contains("has_term(&sp.primary) || has_term(&sp.secondary)"),
            "has_focusable_terminal 必须覆盖分屏的两个窗格，否则分屏空态会漏判。"
        );

        // 5) 焦点收敛的「无终端」分支必须把焦点收回视图句柄，
        //    否则焦点会停在已不在帧里的旧句柄上（关掉最后一个标签即复现）。
        assert!(
            render.contains("None => window.focus(&self.panel_ui.focus, cx),"),
            "pending_focus 的无终端分支必须把窗口焦点收回 self.panel_ui.focus。"
        );
    }

    /// 回归（2026-09-20 运行反馈「主窗格为空，命令面板快捷键呼不出来」）。
    ///
    /// 与上面那条是**同一根因的第二半**：锚点只保证 `self.panel_ui.focus` 在树里，可要是窗口焦点
    /// 停在别处（命令面板是抢焦点的浮层，`open_command_palette` 里 `window.focus(&fh)`），
    /// 派发路径照样退化成根节点。用户按 Ctrl+Shift+P 呼出面板、Esc 关掉，
    /// 焦点若留在已经不在帧里的 `CommandState` 句柄上，之后所有根级快捷键全部失灵 ——
    /// 这正是「主窗格为空时按了没反应」的复现路径。
    ///
    /// 所以关闭面板的**两条**路径都必须归还焦点：`on_cancel`（Esc）与遮罩点击。
    #[test]
    fn closing_the_palette_hands_focus_back_to_the_dispatch_tree() {
        let palette = prod(include_str!("main_view/keybind_actions.rs"));
        assert!(
            palette.contains("m.tabs_state.pending_focus = true;"),
            "关闭命令面板必须置 `pending_focus`：下一帧由 render 统一把焦点收敛回\n\
             终端（有终端）或视图级句柄（空窗格），否则根级快捷键会集体失灵。"
        );
        assert!(
            palette.contains("w.focus(&fh, cx);"),
            "Esc / 遮罩两条关闭路径都要立刻归还焦点（App 上下文里拿不到 Window，\n\
             所以只能在 `on_cancel` 与点击回调里各还一次）。"
        );
        assert!(
            !palette.contains("on_cancel(|_w, cx| Self::close_palette_global(cx))"),
            "`on_cancel` 不得再丢掉那个 `&mut Window` —— 拿到它才谈得上立即归还焦点。"
        );
    }

    /// 运行期复现（2026-09-20 运行反馈「修复主窗格为空 命令面板快捷键呼不出来」）。
    ///
    /// 上面那条用例扫的是源码，只能证明「该写的代码在场」；这条是**真按键**：
    /// 断言「关掉面板之后再按 Ctrl+Shift+P，面板真的又出现了」。两者缺一不可 ——
    /// 源码在、派发路径不通，用户看到的仍然是「按了没反应」。
    ///
    /// 复现路径与用户一致：启动请求打开面板（`pending_palette` 两段式）→ 空主窗格
    /// （一个可聚焦终端都没有）→ Esc 关掉 → 再按 Ctrl+Shift+P。修好之前最后一步
    /// 毫无反应：关面板时焦点停在**已经不在本帧元素树里**的 `CommandState` 句柄上，
    /// gpui 找不到派发目标就退化成派发树根节点，而根 div（`rustshell-root`）只是它的
    /// 子节点 —— 根上的 `on_action(CommandPalette)` 一个都收不到。
    #[gpui::test]
    fn palette_shortcut_works_again_after_closing_it_on_an_empty_pane(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::AppContext as _;
        use std::cell::RefCell;
        use std::rc::Rc;

        use super::{AppShell, MainView, TermShiftTab, TermTab};

        cx.update(|cx| {
            gpui_component::init(cx);
            crate::theme::apply_theme_mode(cx, rustshell_core::theme::ThemeMode::Dark);
            // 与 `shell/src/app.rs` 同口径注册键位：不注册的话这条用例测的是一张空
            // 键位表，「按键没反应」恒真 —— 那就成了一个永远会通过（或永远会失败）
            // 的假断言。这里用默认表而不是 `effective()`，避免被本机 keymap.json 影响。
            let mut bindings: Vec<gpui::KeyBinding> = rustshell_platform::keymap::DEFAULT_BINDINGS
                .iter()
                .filter_map(|(id, spec)| {
                    crate::keybinds::make_binding(id, &rustshell_platform::keymap::resolve(spec))
                })
                .collect();
            bindings.push(gpui::KeyBinding::new("tab", TermTab, None));
            bindings.push(gpui::KeyBinding::new("shift-tab", TermShiftTab, None));
            cx.bind_keys(bindings);
        });

        let slot: Rc<RefCell<Option<gpui::Entity<MainView>>>> = Rc::new(RefCell::new(None));
        let (_, vcx) = {
            let slot = slot.clone();
            cx.add_window_view(move |window, cx| {
                let main = cx.new(|cx| MainView::new(window, cx));
                *slot.borrow_mut() = Some(main.clone());
                let shell = cx.new(|_| AppShell::new(main));
                gpui_component::Root::new(shell, window, cx)
            })
        };
        vcx.run_until_parked();
        let main = slot.borrow().clone().expect("MainView 未创建");

        let palette_open = |vcx: &mut gpui::VisualTestContext| {
            vcx.update(|_w, cx| main.read(cx).panel_ui.palette_state.is_some())
        };

        // 1) 启动路径：只置旗，下一帧 render 兑现（`shell/src/app.rs` 同一条路径）。
        vcx.update(|_w, cx| {
            main.update(cx, |m, cx| {
                m.panel_ui.pending_palette = true;
                cx.notify();
            })
        });
        vcx.run_until_parked();
        assert!(palette_open(vcx), "启动路径没能打开命令面板");

        // 2) 前提：这是空主窗格（没有可聚焦终端，`self.panel_ui.focus` 锚点本帧在树里）。
        let empty = vcx.update(|_w, cx| !main.read(cx).has_focusable_terminal());
        assert!(empty, "本用例的前提是空主窗格，但此刻有可聚焦终端");

        // 3) Esc 关掉 —— 走的是 `Command` 组件 → `on_cancel` 那条真实路径。
        vcx.simulate_keystrokes("escape");
        vcx.run_until_parked();
        assert!(
            !palette_open(vcx),
            "Esc 没能关掉命令面板：断言链路的前提没成立（Command 的 Cancel 没派发出来）"
        );

        // 4) 关键断言：关掉之后再按快捷键，必须还能呼出来。
        vcx.simulate_keystrokes("ctrl-shift-p");
        vcx.run_until_parked();
        assert!(
            palette_open(vcx),
            "主窗格为空时，关掉命令面板之后 Ctrl+Shift+P 再也呼不出来 —— 焦点没还给\n\
             视图级句柄，根 div 上的 `on_action` 收不到键（2026-09-20 运行反馈）。"
        );
    }

    /// 回归护栏：命令输入框是**常驻**输入框，不再有「假输入外观 / 编辑态」两态切换。
    #[test]
    fn quick_bar_compose_is_always_on_and_enter_sends() {
        let code = prod(include_str!("main_view.rs"));

        // 1) 两态开关必须彻底移除（字段与 setter 都已删）。
        assert!(
            !code.contains("quick_compose_open"),
            "命令输入框必须常驻：不得恢复 quick_compose_open 这套「点击即编辑」的\n\
             两态切换 —— 它正是「失焦后外观切回搜索命令」「回车无处可发」的根源。"
        );

        // 2) 回车转发必须在：订阅里调用 send_compose_to_all_tabs（与按钮同一条路径）。
        assert!(
            code.contains("subscribe_in") && code.contains("send_compose_to_all_tabs(w, cx)"),
            "命令输入框必须订阅 InputEvent::PressEnter 并走 send_compose_to_all_tabs，\n\
             否则回车发送会静默无效。"
        );
    }

    /// 终端引擎接线护栏：本轮新增的三类能力必须在 UI 侧真的接上。
    ///
    /// 这几条断言防的是「引擎改了、宿主没接」——症状是功能静默失效：
    /// 引擎里 CLI 测试全绿，用户那边却什么都没有。
    #[test]
    fn terminal_wiring_covers_cursor_paste_and_osc52() {
        let code = prod(include_str!("terminal_view.rs"));

        // OSC 52 读**和**写都要接：旧实现只接了写，读请求在引擎里被 TODO 吞掉，
        // 表现为「远端程序读剪贴板永远拿到空」。
        assert!(
            code.contains("with_clipboard_store_callback"),
            "OSC 52 写入回调缺失：远端拷贝到本地剪贴板会失效。"
        );
        assert!(
            code.contains("with_clipboard_load_callback"),
            "OSC 52 读取回调缺失：远端读本地剪贴板会一直得不到应答。"
        );

        // 闪烁与粘贴策略必须从设置透传到引擎配置（否则就是「死配置」）
        assert!(
            code.contains("cursor_blink: cfg.cursor_blink"),
            "光标闪烁开关未透传到引擎配置。"
        );
        assert!(
            code.contains("cursor_blink_interval_ms: cfg.cursor_blink_ms"),
            "光标闪烁周期未透传到引擎配置。"
        );
        assert!(
            code.contains("paste_normalize_newlines: cfg.paste_normalize_newlines"),
            "粘贴换行策略未透传到引擎配置。"
        );
    }

    /// 设置面板与「终端字段」的消费边界（2026-10-19 改写）。
    ///
    /// 2026-10-19 用户要求精简面板：光标闪烁 / 回滚行数 / 光标样式 /
    /// 多行粘贴行为 / 采样间隔的**控件**撤下，但设置字段与运行时行为全部保留
    /// （老配置里的值继续生效）。于是本护栏分两层：
    ///   1. 面板**不再**提供这些控件；
    ///   2. 开终端 / 热更新 / 适配层仍把字段透传进引擎 —— 否则字段就真的死了。
    ///
    /// 行高仍留在面板（用户没有点名撤它），且必须绑在持久滑块上。
    #[test]
    fn terminal_settings_are_panel_owned_and_forwarded() {
        let panel = prod(include_str!("main_view/settings_panel.rs"));

        // ① 2026-10-19 撤下的控件不得再出现在面板里（字段本身保留在 settings.json）
        for gone in [
            "settings.terminal_cursor_blink",
            "settings.terminal_cursor_style",
            "settings.terminal_paste_behavior",
            "settings.scrollback",
            "settings.sample_interval_ms",
        ] {
            assert!(
                !panel.contains(gone),
                "设置面板不得再摆 `{gone}` 的控件（2026-10-19 用户要求撤下）；\
                 字段仍保留在配置文件里继续驱动终端。"
            );
        }
        // 行高用的是持久滑块实体（不是每帧新建的数字框）
        assert!(
            panel.contains("line_height_slider"),
            "行高控件必须绑在持久的 `line_height_slider` 上 —— 每帧新建 SliderState \
             会把用户手指按着的滑块拽回原位。"
        );

        // ② 开终端的路径必须把新字段透传进引擎配置
        let connect = prod(include_str!("main_view/connections.rs"));
        for field in [
            "cursor_blink: self.settings.terminal_cursor_blink",
            "cursor_blink_ms: self.settings.terminal_cursor_blink_ms",
            "cursor_style: self.settings.terminal_cursor_style",
            "line_height: self.settings.clamped_line_height()",
        ] {
            assert!(
                connect.contains(field),
                "开终端时必须把设置透传进引擎（{field}），\n\
                 否则这个字段就成了面板之外的第二个「死配置」。"
            );
        }

        // ②b 启动动作必须**面板可改**，否则「默认打开本地终端」只是换了个
        //     新的硬编码 —— 用户想回到命令面板只能手改 settings.json。
        assert!(
            panel.contains("settings.startup_action"),
            "启动动作没有面板控件：默认值改了，用户却改不动（新的硬编码）。"
        );
        assert!(
            panel.contains("StartupAction::panel_options()"),
            "启动动作下拉必须走 `panel_options()`：`RestoreSession` 不在面板里\
             （它是「启动时会话恢复」那项的另一说法，摆一起会让人以为要配两次）。"
        );

        // ③ 粘贴：两个引擎参数必须由**同一个** `PasteBehavior` 算出。
        //
        // 这正是本次收敛要守住的东西 —— 只要它们还能各自取值，
        // 「先确认 + 逐行执行」这种矛盾组合就还有可能被配出来。
        let squeezed: String = connect.chars().filter(|c| !c.is_whitespace()).collect();
        for expr in [
            "paste_normalize_newlines:self.settings.terminal_paste_behavior.normalize_newlines()",
            "paste_protect_lines:self.settings.terminal_paste_behavior.protect_lines()",
        ] {
            assert!(
                squeezed.contains(expr),
                "开终端时粘贴的两个引擎参数必须都由 `terminal_paste_behavior` 算出\
                 （缺：{expr}）。\n\
                 这两行是「用户设置」与「引擎行为」之间唯一的换算点，分成两个来源\
                 就等于把矛盾组合的风险重新引回来。"
            );
        }

        // ④ 适配层必须把行高与光标形状真的写进引擎 config
        let adapter = prod(include_str!("terminal_view.rs"));
        assert!(
            adapter.contains("line_height_multiplier: cfg.line_height"),
            "适配层没有把行高写进引擎 config（设置里的行高不会生效）。"
        );
        assert!(
            adapter.contains("default_cursor_shape: to_cursor_shape(cfg.cursor_style)"),
            "适配层没有把光标形状写进引擎 config（配置文件里的光标样式不会生效）。"
        );
    }

    /// 终端引擎不得再自带系统剪贴板实现：剪贴板归宿主。
    ///
    /// 反向信号很明确 —— 重新引入 `arboard` 会让 Linux 构建重新背上
    /// wayland-data-control 原生依赖，并且出现两份剪贴板实现。
    #[test]
    fn terminal_engine_has_no_native_clipboard_dependency() {
        let manifest = include_str!("../../terminal/Cargo.toml");
        assert!(
            !manifest.contains("arboard"),
            "终端引擎不应重新引入 arboard：系统剪贴板是宿主的职责。"
        );
        // 被扫的是 Rust 源码 ⇒ 走 `prod()`：注释里提到 `pub mod clipboard`
        // 不算数，外部 crate 的测试模块也不该被算进「引擎自带了什么」。
        // （上一行是 `Cargo.toml`，不是源码，没有注释语法问题，保持原样。）
        let engine = prod(include_str!("../../terminal/src/lib.rs"));
        assert!(
            !engine.contains("pub mod clipboard"),
            "终端引擎不应再自带剪贴板模块。"
        );
    }

    /// 终端右键菜单的「粘贴」必须走引擎的粘贴通路。
    ///
    /// 真实踩过的坑：菜单里直接 `write_to_pty(&text)` 原样透传，于是菜单粘贴与
    /// Ctrl+Shift+V 语义分叉 —— 菜单路径不做 CRLF 到 CR 的归一，也不做 bracketed
    /// paste 包裹，多行粘贴会在远端 staircase 错位甚至被逐行立即执行。
    #[test]
    fn terminal_menu_paste_routes_through_engine() {
        let code = prod(include_str!("main_view/split_actions.rs"));

        assert!(code.contains(r#"t("粘贴")"#), "终端菜单缺少「粘贴」项。");
        assert!(
            code.contains(r#"t("粘贴并执行")"#),
            "终端菜单缺少「粘贴并执行」项（`paste_text` 的 and_enter 参数会变成死参数）。"
        );
        // F-5（2026-09-28）：入口从 `paste_text` 升到 `request_paste` ——
        // 后者是**过粘贴闸**的那扇门（含换行先挂起等确认），`paste_text` 降为
        // 确认后的底层写入。断言随实现一起收紧：菜单必须走闸门。
        assert!(
            code.contains("tv.request_paste(&text, false, cx)"),
            "菜单「粘贴」绕过了引擎粘贴闸门（行尾归一 / bracketed paste / 多行确认都会丢失）。"
        );
        assert!(
            code.contains("tv.request_paste(&text, true, cx)"),
            "菜单「粘贴并执行」未接到引擎的 and_enter 通路。"
        );
        assert!(
            !code.contains("write_to_pty(&text)"),
            "菜单不得再直接 write_to_pty 透传剪贴板文本：粘贴语义必须收口在引擎里。"
        );
        assert!(
            !code.contains("paste_text("),
            "菜单不得直接调 `paste_text`（底层写入、不过闸）：含换行的粘贴会绕过确认框。"
        );
    }

    /// 回归护栏：连接树的树节点 id / 展开态 / 分组查询必须按分组 **id**，
    /// 而不是分组名。按名走会在「分组改名」或「两个同名分组」时丢展开态、
    /// 甚至把连接拖到错误的分组上。
    #[test]
    fn connection_tree_keys_groups_by_id() {
        let code = prod(include_str!("connections_panel.rs"));

        assert!(
            code.contains(r#"format!("grp:{}", g.id)"#),
            "树节点 id 必须由分组 id 拼出（`grp:{{id}}`）。"
        );
        assert!(
            !code.contains(r#"format!("grp:{group}")"#),
            "不得再按分组**名**拼树节点 id：改名后展开态会丢，同名分组会撞键。"
        );
        // 「显示哪些连接」的决策已整体下沉到 core 的 `visible_tree`
        // （其内部按分组 id 取成员，见 `connections.rs` 的 `profiles_in_group(&g.id)`），
        // 渲染层只消费它的返回值。
        assert!(
            code.contains("visible_tree(&query)"),
            "连接树的可见性决策必须经 core 的 `visible_tree`（内部按分组 id 取成员）。"
        );
        assert!(
            !code.contains("expanded_groups.contains(group)")
                && !code.contains("expanded_groups.contains(&group.name)"),
            "展开态集合存的是分组 id，不得再拿分组名去查。"
        );

        // 用户要求：取消树顶部的「服务器」标识，让分组/连接直接作最上层
        assert!(
            !code.contains(r#"child(t("服务器"))"#),
            "连接树顶部不得再出现「服务器」标识块。"
        );

        // 同一处：MainView 初始化展开态也必须喂 id
        let main = prod(include_str!("main_view.rs"));
        assert!(
            main.contains("view.conn_ui.expanded_groups.insert(g.id)"),
            "初始展开态必须写入分组 id（`expanded_groups.insert(g.id)`）。"
        );
    }

    /// 回归护栏：新建/编辑连接对话框的分组是**下拉选择**（引用分组 id），
    /// 不再是一个自由文本输入框。自由文本会因错字/空格攒出近似重名分组。
    ///
    /// 2026-09-16 用户要求：把「新建分组」从连接对话框里取消，只留下拉选既有分组。
    /// 新建分组改由连接树空白右键菜单进入（`DialogMode::NewGroup`），故这里同时
    /// 回归（2026-09-17 运行反馈「连接支持右键重命名和 F2 快捷键重命名」）：
    ///
    /// 一条断言串起三处，漏任何一处这个功能都是「看起来有、按了没反应」：
    ///   1) 右键菜单里有重命名项（`connections_panel`）；
    ///   2) 树选中态 + F2 有真正的落点（`MainView::rename_selected_connection`）；
    ///   3) F2 真的绑到了该 action（`shell` 的注册处）。
    #[test]
    fn connection_rename_is_wired_from_menu_and_f2() {
        let panel = prod(include_str!("connections_panel.rs"));
        assert!(
            panel.contains("open_rename_connection_dialog"),
            "连接行右键菜单缺少「重命名…」入口。"
        );
        assert!(
            panel.contains("display_spec(\"f2\")"),
            "右键菜单的「重命名…」应标出 F2，否则没人知道有这个快捷键。"
        );

        let connections = prod(include_str!("main_view/connections.rs"));
        assert!(
            connections.contains("rename_selected_connection"),
            "F2 的落点 `rename_selected_connection` 不存在。"
        );
        assert!(
            connections.contains("rename_connection_save"),
            "重命名对话框缺少落库路径（`rename_connection_save`）。"
        );
        assert!(
            connections.contains("rename_profile"),
            "落库必须走 `ConnectionStore::rename_profile`（只改展示名，不动 id）。"
        );

        // 标签栏上已打开的 Tab 要跟着改名，否则用户改完还看到旧标题
        assert!(
            connections.contains("tab.session.title = name.clone()"),
            "改名后必须同步已打开 Tab 的标题。"
        );

        // F2 绑定必须真的注册（否则 action 永远收不到）
        let shell = prod(include_str!("../../shell/src/app.rs"));
        assert!(
            shell.contains("KeyBinding::new(\"f2\", RenameConnection, None)"),
            "F2 未注册到 `RenameConnection`，快捷键按了不会有反应。"
        );
    }

    /// 回归（2026-09-17 运行反馈「文件：路径栏默认为 home 路径，cd ~ 的路径；
    /// 输入路径后点击别的地方需要失去焦点」；2026-09-20 口径定为「远端家目录」）。
    ///
    /// 这条护栏当天被反转过两次（`/` ↔ `~`）。所以它守的**不是某一个具体路径**，
    /// 而是「UI 侧那两处初值同源」—— 换口径只改常量与本节注释，
    /// 别在字段初始化处再写死一个字面量。
    #[test]
    fn files_panel_default_path_has_a_single_source_and_blurs() {
        let main = prod(include_str!("main_view.rs"));
        assert!(
            main.contains("files_path: rustshell_core::sftp::DEFAULT_REMOTE_PATH.into()"),
            "文件面板初始路径必须取自 `core::sftp::DEFAULT_REMOTE_PATH`，\
             不得在字段初始化处硬编码路径字面量。"
        );
        // 输入框回填也必须走常量：上一次改口径（`/` → `~`）只改了字段初值，
        // 漏掉了这一行 `set_value("~")`，于是字段是 `/`、输入框显示 `~`。
        // 断言去空白后比对，免得被 `cargo fmt` 的折行方式绑死。
        let squeezed: String = main.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(
            squeezed.contains("s.set_value(rustshell_core::sftp::DEFAULT_REMOTE_PATH.to_string()"),
            "路径栏回填初值必须走 `DEFAULT_REMOTE_PATH`，否则字段与输入框会各说各话。"
        );
        assert!(
            !main.contains("files_path: \"/\".into()")
                && !main.contains("files_path: \"~\".into()"),
            "路径栏默认值又变成就地字面量了 —— 口径只能有一个来源。"
        );

        // 订阅在 `main_view.rs`（`MainView::new` 里装配输入事件），
        // 落点 `files_path_blur` 在 `terminal_actions.rs` —— 两处都要在。
        assert!(
            main.contains("InputEvent::Blur"),
            "路径栏必须订阅失焦事件，否则输入框会一直攥着焦点与草稿。"
        );
        let actions = prod(include_str!("main_view/terminal_actions.rs"));
        assert!(
            actions.contains("files_path_blur"),
            "失焦落点 `files_path_blur` 不存在。"
        );
        assert!(
            actions.contains("normalize_remote_input"),
            "回车跳转必须走 `normalize_remote_input`（支持 `~` / `cd ~`）。"
        );
    }

    /// 回归（2026-09-18 用户要求「搜索命令和输入命令栏和文件的地址栏，聚焦后，
    /// 点击别的地方需要进行失焦」）：四个常驻输入框都要挂「点框外失焦」，
    /// 且实现必须是 [`blur_input_on_outside_click`]（只在焦点真在本框上时才收）。
    ///
    /// 为什么不能只靠官方 `Input`：它只在点到**另一个可聚焦元素**时才换焦点 ——
    /// 点终端 / 侧栏空白处不会松手，光标一直闪、键盘输入继续被那个框吃掉。
    /// 挂法是 `on_mouse_down_out`（**捕获**阶段、仅在本元素边界外触发），
    /// 所以点到别的输入框时，仍然是那个框在随后的冒泡里拿到焦点。
    #[test]
    fn text_inputs_release_focus_when_clicking_outside() {
        let quick = prod(include_str!("quick_commands_bar.rs"));
        let files = prod(include_str!("files_panel.rs"));
        let conns = prod(include_str!("connections_panel.rs"));

        // 每个输入框各挂一枚，且挂在**自己**的包装层上
        // ★ 锚点只取**字段引用**那一段（`&m.ai_ui.quick_search`），不取整条调用 ——
        //   rustfmt 按行宽把 `blur_input_on_outside_click(&m.ai_ui.quick_search, w, cx)`
        //   折成了多行（`connections_panel.rs:354` 就是一行的，`files_panel.rs:453`
        //   是三行的），整条调用串在源码里**并不总是**存在 ⇒ 断言恒假。
        //   「这些框挂了失焦处理」由 `.contains("blur_input_on_outside_click(")` 单管，
        //   这里只管「**哪个**输入框挂了」。
        for (src, input) in [
            (&quick, "&m.ai_ui.quick_search"),
            (&quick, "&m.ai_ui.quick_compose_input"),
            (&conns, "&m.conn_ui.conn_search"),
        ] {
            assert!(
                src.contains(input),
                "输入框没有挂「点框外失焦」：{input}（聚焦后点别处不会松手）。"
            );
        }
        // 文件地址栏那处被 rustfmt 折成多行，故按参数名比对
        assert!(
            files.contains("blur_input_on_outside_click(")
                && files.contains("&m.io.files_path_input"),
            "文件地址栏没有挂「点框外失焦」。"
        );

        // 实现在 `main_view.rs`：只收「本来就在这个框上」的焦点
        let main = prod(include_str!("main_view.rs"));
        for needle in [
            "pub fn blur_input_on_outside_click(",
            "if input.read(cx).focus_handle(cx).is_focused(w) {",
            "w.blur(cx);",
        ] {
            assert!(
                main.contains(needle),
                "失焦实现缺少 `{needle}`：无条件 blur 会把对话框/终端的焦点也一起收走。"
            );
        }
    }

    /// 文件侧边栏默认路径的当前口径（2026-09-20 用户要求）：
    /// **远端登录用户 home 的绝对路径**，由 SFTP 侧向服务器解析后回报。
    ///
    /// 与上一条分开写：上一条守结构（初值同源），本条守口径（首扫走 home 解析）。
    /// 前两次口径变化（`/` ↔ `~`）都只动了常量值 —— 而这次动的**不是常量**：
    /// UI 是本机进程，「服务器上那个用户的 home」它根本算不出来，只能问服务器。
    /// 所以锚点从「常量等于什么」挪到「首扫发什么命令」：谁要是把首扫写回
    /// `List { path: <某个字面量> }`，默认落点就与本机或写死的值绑死，
    /// 不再是服务器上的家目录。
    #[test]
    fn files_panel_default_path_resolves_the_remote_home() {
        // 兜底仍是根：拿不到 home 时宁可落在 `/` —— 多按一次「上级」，
        // 也不要把用户扔到一个来路不明的目录里。
        assert_eq!(
            rustshell_core::sftp::DEFAULT_REMOTE_PATH,
            "/",
            "`DEFAULT_REMOTE_PATH` 现在是 home 解析失败时的兜底，应为文件系统根 `/`。"
        );
        let actions = prod(include_str!("main_view/terminal_actions.rs"));
        assert!(
            actions.contains("SftpCmd::ListHome"),
            "首扫必须发 `SftpCmd::ListHome`：远端家目录只有服务器知道，\
             UI 侧给不出这个路径。"
        );
        let squeezed: String = actions.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(
            !squeezed.contains("DEFAULT_REMOTE_PATH"),
            "首扫不该再引用兜底常量 —— 它现在只用于路径栏占位初值与解析失败兜底。"
        );
    }

    /// 回归（2026-09-17 运行反馈「主窗口和副窗格切换只有文字颜色变化不太明显」）：
    /// 焦点窗格必须有跨整条标题的高亮标记，不能只改「主窗格 / 副窗格」四个字的颜色。
    #[test]
    fn focused_pane_has_a_visible_marker() {
        let code = prod(include_str!("main_view/split_actions.rs"));
        assert!(
            code.contains("pane-focus-bar"),
            "焦点窗格缺少可见标记条（`pane-focus-bar`）。"
        );
        assert!(
            code.contains("bg_elevated"),
            "焦点窗格的标题条底色必须与另一侧不同。"
        );

        let main = prod(include_str!("main_view.rs"));
        let split = prod(include_str!("main_view/split_actions.rs"));
        let actions = prod(include_str!("main_view/terminal_actions.rs"));
        // F2 与拆分都必须先判断焦点是否在终端：抢了终端的按键就是「远端按了没反应」
        assert!(
            main.contains("terminal_has_focus"),
            "全局按键处理器必须先判断终端焦点（`terminal_has_focus`），否则会抢走终端的按键。"
        );
        assert!(
            actions.contains("fn terminal_has_focus"),
            "`terminal_has_focus` 的实现在 `terminal_actions.rs` 里。"
        );
        // 拆分的落点仍在（标签栏按钮移除了，但菜单与快捷键还得走同一条路）
        assert!(
            split.contains("split_pane"),
            "拆分入口（`split_pane`）不得随标签栏按钮一起消失。"
        );
    }

    /// 连接表单**不再有分组一栏**（2026-09-23 用户要求「新增、修改连接面板的
    /// 【分组】按钮删掉，默认不分组」）。
    ///
    /// 三件事缺一不可，任何一条失守都对应一种真实故障：
    ///   1) 表单正文里没有分组控件 —— 控件还在就说明「删掉」没落实；
    ///   2) 保存**仍读** `dlg_group_id` —— 归属不是「不写了」，只是不再从表单里来
    ///      （分组右键「新建连接」传进来的 id / 编辑回填的原归属）。把它一并删掉，
    ///      「在分组上右键新建连接」会静默失效，编辑还会把连接挤出原分组；
    ///   3) 没有任何**界面写** `dlg_group_id` 的地方 —— 「新建分组后悄悄记下默认
    ///      归属」那条就是典型：写进去的归属在表单里完全看不见，用户接着点
    ///      「新建连接」就莫名落进某个文件夹。
    #[test]
    fn connect_dialog_has_no_group_field() {
        let code = prod(include_str!("main_view/dialogs.rs"));

        // 锚点必须夹住 `render_dialog` 本身，不能拿「`fn dialog_save` 之前的一切」
        // 当代理 —— 「分组名」对话框（`render_group_name_dialog`）与 `group_name_save`
        // 就排在它后面/附近，它们合法地读写 `dlg_group_name`，用旧锚点会误判。
        let start = code
            .find("fn render_dialog(")
            .expect("找不到 render_dialog（结构锚点）");
        let end = code
            .find("fn render_group_name_dialog(")
            .expect("找不到 render_group_name_dialog（结构锚点）");
        let form = &code[start..end];
        assert!(
            start < end,
            "锚点顺序反了：render_dialog 应当排在分组名对话框前面"
        );
        for gone in [
            "dlg-group-pick",
            "dropdown_menu",
            "dlg_group",
            "dlg_group_id",
            "未分组",
            "group_name_body",
            "dlg_group_name",
            "ensure_group_by_name",
        ] {
            assert!(
                !form.contains(gone),
                "连接表单里不得再出现分组（命中 `{gone}`）：分组一栏已按用户要求删除，\
                 归属只来自上下文（分组右键「新建连接」）与编辑回填。"
            );
        }

        // 保存路径仍以 `dlg_group_id` 为事实源；且**没有**界面再写它。
        assert!(
            code.contains("let group_id = self.io.dlg_group_id.clone()"),
            "保存时必须仍写回 `dlg_group_id`（否则归属从哪来的都不算数了）。"
        );
        assert!(
            !code.contains("dlg_group_id = Some("),
            "不得再有「悄悄设定默认归属」的写入（新建分组后记下 id 那种）：\
             表单里看不见的归属就是让用户莫名落进某个文件夹。"
        );

        // 上下文来路要在：分组右键「新建连接」= `open_connect_dialog(Some(id))`。
        let conns = prod(include_str!("main_view/connections.rs"));
        assert!(
            conns.contains("group_id: Option<String>")
                && conns.contains("dlg_group_id = group_id;"),
            "新建连接对话框必须仍接收上下文的预选分组（`open_connect_dialog` 的 `group_id`）。"
        );
        // 编辑时回填既有归属（改归属走连接树拖拽，不走表单）
        assert!(
            conns.contains("dlg_group_id = p.group_id.clone();"),
            "编辑连接时必须回填既有归属，否则一编辑就把连接挤出原分组。"
        );

        // 分组名对话框视图照旧（输入框由该视图自带：留在 MainView 里会让每次击键重排整窗口）
        //
        // ★ 刀三后 `dlg_group_id` / `group_name_body` 的**声明**在 `ui_state.rs` 里
        //   （`MainView` 只留 12 个字段），所以这条断言扫的是 ui_state 而不是 main_view。
        //   断言源不跟着搬移走，就会退化成「断言 main_view.rs 里没有某个东西」——
        //   而那里已经根本不该有（断言恒真，形同虚设）。
        let state = prod(include_str!("main_view/ui_state.rs"));
        assert!(
            state.contains("dlg_group_id: Option<String>")
                && state.contains("group_name_body: Entity<GroupNameDialogView>"),
            "`DialogIoState` 必须持有 `dlg_group_id`（上下文/回填的归属）与 \
             分组名对话框视图（`group_name_body`）。"
        );
    }

    /// 回归护栏：连接导入/导出必须走**系统原生文件对话框**。
    ///
    /// 2026-09-15 用户要求：点「导入」直接挑文件、点「导出」直接挑位置并确认，
    /// 设置页里不再摆「导出目录 / 导出文件名 / 浏览 / 打开导出目录」这一排中间控件。
    /// 同时目标路径**只许来自对话框返回值** —— 自行拼「固定目录 + 自动文件名」
    /// 会让文件落在用户没看的地方。
    #[test]
    fn conn_import_export_use_native_path_dialogs() {
        let code = prod(include_str!("main_view/settings_actions.rs"));

        assert!(
            code.contains("prompt_for_new_path"),
            "导出必须走 `cx.prompt_for_new_path`（系统「另存为」对话框）。"
        );
        assert!(
            code.contains("prompt_for_paths"),
            "导入必须走 `cx.prompt_for_paths`（系统「打开」对话框）。"
        );
        assert!(
            !code.contains("conn_browse") && !code.contains("resolve_export_target"),
            "内联文件浏览器与「目录 + 文件名」拼接已废弃：路径只来自对话框返回值。"
        );
        assert!(
            !code.contains("dir.join(format!("),
            "不得再自行拼「固定目录 + 自动文件名」作为导出目标。"
        );

        // 设置页不许再残留需要用户手填路径的中间控件
        let panel = prod(include_str!("main_view/settings_panel.rs"));
        for needle in [
            "conn-export-dir-browse",
            "conn-import-file-browse",
            "conn-browse-pick-dir",
            "打开默认导出目录",
        ] {
            assert!(
                !panel.contains(needle),
                "设置-连接管理页残留了 `{needle}`，用户会被要求自己填路径。"
            );
        }
        // 一键导入 / 一键导出两个入口必须在
        for needle in ["conn-export", "conn-import"] {
            assert!(
                panel.contains(needle),
                "设置-连接管理页缺少 `{needle}` 按钮，功能就没了入口。"
            );
        }

        // 「连接设置」页已并入「连接管理」：超时/重连三项必须还在同一页里
        assert!(
            !panel.contains("SettingPage::new(t(\"连接设置\"))"),
            "「连接设置」页应已并入「连接管理」，不该再单独占一页。"
        );
        for needle in ["连接超时（秒）", "断线自动重连", "最大重试次数"] {
            assert!(
                panel.contains(needle),
                "迁移后「连接管理」页缺少 `{needle}`，设置项被搬丢了。"
            );
        }
    }

    /// 回归护栏：连接树的搜索过滤只许走 core 的纯函数 `visible_tree`。
    /// 面板里自己重写一套 contains 判定，两处实现必然会分叉
    /// （一个改了口径，另一个没跟）。
    #[test]
    fn connection_tree_search_uses_core_filter() {
        let panel = prod(include_str!("connections_panel.rs"));
        assert!(
            panel.contains("visible_tree"),
            "连接树必须用 `ConnectionStore::visible_tree` 决定可见内容。"
        );
        assert!(
            panel.contains("SearchQuery::parse"),
            "关键词归一化必须走 `SearchQuery::parse`（trim + 小写只该在那一处做）。"
        );
        assert!(
            panel.contains("未找到匹配的连接"),
            "搜索无结果必须给可见提示；空树会让用户以为连接被删了。"
        );
    }

    /// 回归护栏：连接树展开/收起事件带的是**节点 id**（`grp:<分组 id>`），
    /// 而 `expanded_groups` 存的是裸分组 id。不换算就会出现「收起写进 `grp:x`、
    /// 展开查 `x`」的错位 —— 集合里恒有 `x`（启动时全部展开），
    /// 现象就是分组看着永远展开、点节点也收不起来。
    #[test]
    fn tree_expand_events_are_keyed_by_group_id() {
        assert_eq!(super::group_id_of_node("grp:abc"), Some("abc"));
        assert_eq!(
            super::group_id_of_node("prof:abc"),
            None,
            "连接行不是分组，不该动展开态"
        );
        assert_eq!(super::group_id_of_node("abc"), None, "没有前缀的一律不认");
        assert_eq!(
            super::group_id_of_node("grp:"),
            Some(""),
            "退化输入不得 panic"
        );

        let main = prod(include_str!("main_view.rs"));
        assert!(
            main.contains("group_id_of_node(id)"),
            "展开/收起事件必须经 `group_id_of_node` 换算成裸分组 id 再写 `expanded_groups`。"
        );
        assert!(
            !main.contains("m.conn_ui.expanded_groups.insert(id.to_string())"),
            "不得把节点 id（`grp:<id>`）直接写进 `expanded_groups`：渲染侧按裸分组 id 查，永远查不到。"
        );
    }

    /// 无头确定性验证：**点分组行确实会触发展开/折叠**，且 `TreeEvent` 带回的是
    /// **带 `grp:` 前缀的节点 id**。一条测试钉住两件事：
    ///   1) 组件侧的「点击到折叠」链路（`TreeState::render` 行上的 `on_mouse_down(Left)`
    ///      到 `toggle_expand`）真的在跑 —— 所以「点不动」的根因只能在键空间上；
    ///   2) 应用侧收到事件后**必须**经 `group_id_of_node` 才能落进 `expanded_groups` 的键空间。
    ///
    /// 用 `#[gpui::test]` + `simulate_click`（真实命中分发）而不是截图判色：结论不受主题、
    /// 字号、DPI 影响。
    #[gpui::test]
    fn folder_row_click_emits_prefixed_collapse_event(cx: &mut gpui::TestAppContext) {
        use gpui::{
            div, point, AppContext as _, InteractiveElement as _, IntoElement, ParentElement as _,
            Render, Styled as _, Window,
        };
        use gpui_component::{
            list::ListItem,
            tree::{Tree as TreeView, TreeEvent, TreeItem, TreeState},
        };
        use std::cell::RefCell;
        use std::rc::Rc;

        /// 探针视图：一棵与生产同形态的树（分组一帧、每帧按外部状态回填 `expanded`）
        struct Probe {
            ts: gpui::Entity<TreeState>,
        }

        impl Render for Probe {
            fn render(
                &mut self,
                _w: &mut Window,
                cx: &mut gpui::Context<Self>,
            ) -> impl IntoElement {
                let items = vec![TreeItem::new("grp:g1", "生产组")
                    .expanded(true)
                    .child(TreeItem::new("prof:p1", "prod-01"))];
                self.ts.update(cx, |ts, cx| ts.set_items(items, cx));

                div()
                    .w(gpui::px(240.))
                    .h(gpui::px(160.))
                    .child(TreeView::new(&self.ts, |_ix, entry, _selected, _w, _cx| {
                        let row = ListItem::new(entry.item().id.clone());
                        // `debug_bounds` 只认 `&'static str`，故 selector 必须是静态字面量
                        if &*entry.item().id == "grp:g1" {
                            row.debug_selector(|| "probe-grp-row".to_string())
                        } else {
                            row
                        }
                    }))
            }
        }

        cx.update(|cx| {
            gpui_component::init(cx);
            crate::theme::apply_theme_mode(cx, rustshell_core::theme::ThemeMode::Dark);
        });

        let events: Rc<RefCell<Vec<TreeEvent>>> = Rc::new(RefCell::new(Vec::new()));
        let (_, vcx) = cx.add_window_view({
            let events = events.clone();
            move |_w, cx| {
                let ts = cx.new(|cx| TreeState::new(cx));
                let sink = events.clone();
                cx.subscribe(&ts, move |_probe, _ts, ev: &TreeEvent, _cx| {
                    sink.borrow_mut().push(ev.clone());
                })
                .detach();
                Probe { ts }
            }
        });
        vcx.run_until_parked();

        let row = vcx
            .debug_bounds("probe-grp-row")
            .expect("分组行未渲染，测不出点击行为");
        let pos = point(
            row.origin.x + row.size.width / 2.0,
            row.origin.y + row.size.height / 2.0,
        );
        vcx.simulate_click(pos, Default::default());
        vcx.run_until_parked();

        let got = events.borrow().clone();
        assert_eq!(
            got.len(),
            1,
            "点一次分组行应恰好触发一次展开态变化，实际：{got:?}"
        );
        let node_id = match &got[0] {
            TreeEvent::Collapsed(id) => id.to_string(),
            other => panic!("点已展开的分组应发出 Collapsed，实际：{other:?}"),
        };
        assert_eq!(
            node_id, "grp:g1",
            "事件带回的是**节点 id**（带 `grp:` 前缀）"
        );

        // 这才是 `expanded_groups` 的键空间：不换算就永远对不上
        let key = super::group_id_of_node(&node_id).expect("分组事件必须能换算成裸分组 id");
        assert_eq!(key, "g1");
        assert_ne!(node_id.as_str(), key, "事件 id 与业务键不同源 ⇒ 必须换算");
    }

    /// 回归（2026-09-17「提示别贴在命令栏上」+ 2026-09-18「改为不需要【确认】的
    /// 弹出消息」两条运行反馈）：
    ///
    /// 形态从「居中 + 两个按钮 + 蒙版」变成「**贴顶居中的弹出消息**：无按钮、
    /// 无蒙版、到点自动消失」。本用例钉住四件事：
    ///   1) 卡片与覆盖层仍在（`no-session-alert` / `no-session-alert-overlay`），
    ///      覆盖层是绝对定位、水平居中、贴顶（`items_start`）—— 回到 `items_center`
    ///      就会和空态那张卡叠在一起，把空态自己的「新建本地终端」按钮盖住；
    ///   2) **没有按钮**：两枚旧按钮的 id（`-dismiss` / `-newterm`）不得再出现
    ///      —— 用户要的是一条消息，不是一道选择题；
    ///   3) 覆盖层不铺底色（不蒙版）：出现 `.bg(` 就退化回模态观感；
    ///   4) 触发点走唯一入口 `show_no_session_alert`，两条发送路径都调它，
    ///      不再各自置标记（否则「自动消失」会在某条路径上缺失）。
    #[test]
    fn no_session_prompt_is_a_buttonless_notice() {
        let main = prod(include_str!("main_view.rs"));
        for needle in ["no-session-alert-title", "no-session-alert-overlay"] {
            assert!(
                main.contains(needle),
                "主视图里缺少 `{needle}`；这条提示应挂在主窗格上、位于显眼位置。"
            );
        }
        for gone in ["no-session-alert-dismiss", "no-session-alert-newterm"] {
            assert!(
                !main.contains(gone),
                "「没有可用会话」提示又长出了按钮（`{gone}`）—— 用户要求这条提示\
                 不需要确认：它只是一条消息，用户看完继续干活。"
            );
        }
        // 覆盖层：绝对定位 + 贴顶 + 水平居中，且**不铺底色**（不蒙版）。
        // 断言都收在覆盖层这一段代码里，避免「全文件里恰好有 items_start()」这种假通过。
        let layer = main
            .split("no-session-alert-overlay")
            .nth(1)
            .expect("找不到覆盖层");
        let layer_tail = &layer[..layer.find("into_any_element").unwrap_or(layer.len())];
        for layout in [
            "absolute()",
            "inset_0()",
            "items_start()",
            "justify_center()",
        ] {
            assert!(
                layer_tail.contains(layout),
                "弹出消息的覆盖层缺少 `{layout}`：它要铺满主窗格、贴顶、水平居中。"
            );
        }
        assert!(
            !layer_tail.contains("items_center()"),
            "覆盖层又回到垂直正中了 —— 那里是空态卡片的位置（图标 + 「主窗格为空」+ \
             「新建本地终端」按钮），提示会把它盖住 4 秒。"
        );
        assert!(
            !layer_tail.contains(".bg("),
            "覆盖层又铺了蒙版 —— 「不需要确认」的提示不该把整屏压暗。"
        );

        // 触发点：两条「没有可用会话」的出口都必须走唯一入口
        let actions = prod(include_str!("main_view/quickcmd_actions.rs"));
        assert_eq!(
            actions.matches("self.show_no_session_alert(cx)").count(),
            2,
            "「快捷命令 / 广播草稿」两条发送路径都要经 `show_no_session_alert`（居中 + 自动消失）。"
        );
        assert_eq!(
            actions.matches("quick_no_session_alert = true").count(),
            1,
            "置标记只允许出现在唯一入口 `show_no_session_alert` 内部；\
             各触发点一律调它 —— 直接置标记会让「自动消失」只挂在其中一条路径上。"
        );
        assert!(
            actions.contains("NO_SESSION_NOTICE_MS"),
            "自动消失的时长必须用 `NO_SESSION_NOTICE_MS` 常量（与提示内容同级的可感知行为）。"
        );
        assert!(
            !actions.contains("当前没有已连接的会话"),
            "「没有可用会话」的文案与渲染都归主视图那张居中卡片，命令栏域不该再插一脚。"
        );
    }

    /// P0-3 的裁决逻辑：**只有**「归属已知且不是给我的」才跳过重绘。
    ///
    /// 这三条边界各对应一类真实场景，写反任何一条都会出问题：
    /// * 归我 → 必须重绘（不然改了数据界面不动）；
    /// * 归别人 → 跳过（这才是省下来的那部分）；
    /// * **没有归属 → 必须兜底重绘**（裸 `cx.notify()`；漏标归属时的安全网）。
    #[test]
    fn view_epoch_only_skips_other_views_notifies() {
        use super::{ChildView, ViewDirty, ViewEpoch};

        let mut epoch = ViewEpoch::default();
        let sidebar_now = |e: &ViewEpoch| e.probe(ChildView::Sidebar);

        // 没有归属：`attributed` 不前进 ⇒ 任何子视图都重绘
        let prev = sidebar_now(&epoch);
        assert!(
            ViewEpoch::wants_redraw(prev, sidebar_now(&epoch)),
            "没有归属的 notify 必须兜底重绘"
        );

        // 归属 AI 对话列：侧栏跳过
        epoch.merge_scoped(ViewDirty::default().set(ChildView::AiChat));
        let prev = sidebar_now(&epoch);
        epoch.merge_scoped(ViewDirty::default().set(ChildView::AiChat));
        assert!(
            !ViewEpoch::wants_redraw(prev, sidebar_now(&epoch)),
            "归属别人的 notify 不该让侧栏重绘"
        );

        // 归属侧栏：必须重绘
        let prev = sidebar_now(&epoch);
        epoch.merge_scoped(ViewDirty::default().set(ChildView::Sidebar));
        assert!(
            ViewEpoch::wants_redraw(prev, sidebar_now(&epoch)),
            "归属自己的 notify 必须重绘"
        );

        // `poll` 的空掩码 = 没有归属（漏标归属的安全网）：`attributed` 不前进
        let before = epoch.probe(ChildView::AiChat).1;
        epoch.merge(ViewDirty::default());
        assert_eq!(
            epoch.probe(ChildView::AiChat).1,
            before,
            "空掩码不该被当成一次归属已知的 notify —— 否则漏标归属会挡住本该重绘的视图"
        );
    }

    /// 三个独立子视图必须走**带条件**的 observe（P0-3）。
    ///
    /// 钉源码而不是只靠上面的纯逻辑测试：`ViewEpoch` 写得再对，只要某处又把
    /// 原来的 `cx.observe(&main, |_, _, cx| cx.notify())` 贴回去，三个子树的
    /// prepaint 缓存就又全废了 —— 而且完全不影响功能，只悄悄变慢。
    ///
    /// ★ 扫的是 `app_shell.rs` 不是 `main_view.rs`：三个子视图与 `ViewEpoch`
    /// 已于 2026-10-12（开源评审 P0-4 刀一）搬进那个子模块。护栏的「源码源」
    /// 必须跟着搬移走，否则它会变成「断言 main_view.rs 里没有某个东西」——
    /// 而那里已经根本不该有（断言恒真，形同虚设）。
    #[test]
    fn child_views_gate_the_parent_observe() {
        let code = prod(include_str!("main_view/app_shell.rs"));
        assert!(
            !code.contains("cx.observe(&main, |_, _, cx| cx.notify())"),
            "又出现了无条件 observe 父视图：`MainView` 的任何一次 notify 都会让该子树\
             重跑 render（P0-3）。改用带归属的 `notify_view` + `ViewEpoch`。"
        );
        // ★ 数 `probe(ChildView::` 而不是 `view_epoch.probe(ChildView::`：
        //   刀三后链变长，rustfmt 把 `TermSearch` 那处折成了
        //   `.view_epoch` / `.probe(ChildView::TermSearch)` 两行 ⇒ 带前缀的整串恒不存在，
        //   断言恒假。而 `probe(ChildView::` 这一段在三种折行形态下都成立。
        //   逐个点名会得到「4 个子视图」而不是 3 个（`CompletionBarView` 的 probe
        //   在 `completion_bar.rs` 里，不在本文件），所以这里按**总数**断言。
        assert_eq!(
            code.matches("probe(ChildView::").count(),
            3,
            "三个独立子视图（侧栏 / AI 对话列 / 终端搜索浮层）都要按自己的归属判定重绘。"
        );
    }

    /// AI 会话落盘的**接线**（2026-09-23「AI对话需要保存」）。
    ///
    /// 落盘格式本身由 `ai_persist` 的用例管（往返 / 坏文件 / 计数器修复），
    /// 这里只钉三处**少一处就静默失效**的接线 —— 它们都不会报编译错，表现一律是
    /// 「功能看着有，历史照样丢」：
    ///   1) 启动时读 `ai_chats.json`；
    ///   2) 恢复回来的条数**喂给滚动状态**（喂 0 的话虚拟列表认为一行都没有 ——
    ///      数据在内存里，面板却是空的，用户只会说「保存没生效」）；
    ///   3) `poll` 里每轮到点写。
    #[test]
    fn ai_chat_history_is_wired_to_disk() {
        let code = prod(include_str!("main_view.rs"));

        // 切片端点取**代码形态**（不是注释）：`prod()` 先剥注释，用注释当锚点必 panic
        let new_start = code
            .find("pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {")
            .expect("找不到 MainView::new（结构锚点）");
        let poll_at = code
            .find("fn poll(&mut self, cx: &mut Context<Self>) -> bool {")
            .expect("找不到 poll（结构锚点）");
        let new_src = &code[new_start..poll_at];
        assert!(
            new_src.contains("ai_persist::load_for_startup()"),
            "启动没有从磁盘恢复 AI 会话：关一次软件就失忆（用户反馈的原始现象）。"
        );
        assert!(
            new_src.contains("ai_history.current_msgs_len()"),
            "滚动状态的行数没有用恢复回来的条数：历史恢复进内存了，面板却按「一行都没有」渲染。"
        );
        assert!(
            new_src.contains("ai_chats: ai_history.chats"),
            "恢复出来的会话没有进 MainView。"
        );

        let poll_src = &code[poll_at..];
        assert!(
            poll_src.contains("self.ai_chats_flush_if_due();"),
            "poll 里没有落盘调用：会话永远只活在内存里。"
        );

        // 落盘真正写哪个文件，以及「测试构建绕开真实文件」这条隔离
        let persist = prod(include_str!("main_view/ai_persist.rs"));
        assert!(
            persist.contains("rustshell_platform::paths::ai_chats_path()"),
            "落盘路径不再是 `ai_chats.json`（`paths::ai_chats_path()`）了。"
        );
        assert!(
            persist.contains("cfg!(test)"),
            "测试构建必须绕开真实会话文件：`MainView` 是用例脚手架，测试调度器一推时钟\
             `poll` 就会落盘 —— 直接写开发机的 `ai_chats.json` 等于跑一次用例抹一次聊天记录。"
        );

        // 退出钩子：函数在、且**自己 detach**（`Subscription` 一 drop 就退订，
        // 把 detach 交给调用方的写法一旦写错就是「钩子根本没挂上」且无人察觉）
        let quit_start = code
            .find("pub fn ui_install_flush_ai_chats_on_quit")
            .unwrap_or_else(|| {
                panic!("退出前落盘的钩子没了：流式期间最多每 3 秒写一次，最后一段会丢。")
            });
        // 只切到**本函数**的闭合大括号，不切到文件尾：文件后半截到处是别的
        // `.detach()`，切到尾巴的话钩子自己没 detach 也照样能喂饱下面的断言。
        //
        // 结束符取 `"\n}\n"`（换行 + 行首单括号 + 换行）而不是 `"\n}"`：
        // 本函数的收尾正是「单独一行 `}`，后面跟下一段代码」，所以 `"\n}\n"` 能
        // 精确落在它的闭合括号上；而 `"\n}"` 只是「行首 `}`」，会命中的是**第一个**
        // 行首闭合括号 —— 此刻恰好也是同一个位置，但那是巧合：函数体里任何一处
        // （现在或将来）被格式化成行首 `}`，切片就会提前截断，`.detach()` 落到
        // 切片外 ⇒ 断言恒红。用更紧的锚点，等于顺手排掉这颗雷。
        //
        // 2026-09-28 的真实事故：这个 `"\n}\n"` 一度**panic**，根因不是锚点选错，
        // 而是**行尾** —— 本仓在 Windows 上按 CRLF 检出，`include_str!` 拿到的是
        // `\r\n`，于是 `\n}\n` 永远找不到（`\r\n}\r\n` 里不存在 `\n}\n`），而 CI
        // 用 LF 检出反而是绿的。修在唯一的取源入口 `prod()`：先把 `\r\n` 归一成
        // `\n`，所有扫源码的护栏从此与检出配置无关；护栏自身不必再迁就行尾。
        let body_end = code[quit_start..]
            .find("\n}\n")
            .expect("找不到退出钩子的闭合大括号");
        let quit_src = &code[quit_start..quit_start + body_end];
        assert!(
            quit_src.contains("on_app_quit(") && quit_src.contains(".detach()"),
            "退出钩子必须就地 `on_app_quit(...).detach()`：`Subscription` 一 drop 就退订，\
             把返回值交给调用方就是在赌对方记得 detach。"
        );
    }

    /// 命令历史按连接指纹分桶落盘的**接线**（2026-10-08「继续做遗留项123」的遗留 1+2）。
    ///
    /// 与 `ai_chat_history_is_wired_to_disk` 同构 —— 落盘格式由
    /// `completion_persist` 的 14 条用例管（往返 / 丢空桶 / 坏文件 / 序号修复 /
    /// 切桶互换），这里只钉**四处少一处就静默失效**的接线：
    ///   1) 启动时读 `cmd_history.json`（在 `layout_startup` 之后装载，见下）；
    ///   2) `poll` 里每轮到点写；
    ///   3) 路径是 `paths::cmd_history_path()`，且测试构建绕开真实文件；
    ///   4) 退出钩子**两条数据线都在**（AI 与命令历史各自 flush，互不影响）。
    #[test]
    fn cmd_history_is_wired_to_disk() {
        let code = prod(include_str!("main_view.rs"));

        let new_start = code
            .find("pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {")
            .expect("找不到 MainView::new（结构锚点）");
        let poll_at = code
            .find("fn poll(&mut self, cx: &mut Context<Self>) -> bool {")
            .expect("找不到 poll（结构锚点）");
        let new_src = &code[new_start..poll_at];
        assert!(
            new_src.contains("completion_persist::load_for_startup()"),
            "启动没有从磁盘恢复命令历史：每重启一次补全就失忆，\"多会话分桶\" 也无从谈起。"
        );
        // 装载点必须在 `layout_startup` **之后**：该函数会建立标签并 resolve 出会话
        // 指纹，而「当前会话那一桶」正是按这个键取的。放进 `Self { .. }` 会恒落到
        // 本机桶（那时 `active` 还是 `None`）—— 症状是「远端机器的历史看着像本地命令」。
        let startup_at = new_src
            .find("view.layout_startup(cx);")
            .expect("找不到 `layout_startup` 调用（装载顺序锚点）");
        let install_at = new_src
            .find("install_cmd_history_buckets(")
            .expect("启动时没有把读到的分桶装进 MainView：文件读了但没人用，等于没读。");
        assert!(
            install_at > startup_at,
            "分桶装载必须在 `layout_startup` 之后：放在它前面时 `active` 还是 `None`，\
             当前会话那一桶恒落在本机桶，远端历史会被归错机器。"
        );

        let poll_src = &code[poll_at..];
        assert!(
            poll_src.contains("self.cmd_history_flush_if_due();"),
            "poll 里没有落盘调用：命令历史永远只活在内存里，关一次软件就丢。"
        );

        let persist = prod(include_str!("main_view/completion_persist.rs"));
        assert!(
            persist.contains("rustshell_platform::paths::cmd_history_path()"),
            "落盘路径不再是 `cmd_history.json`（`paths::cmd_history_path()`）了。"
        );
        assert!(
            persist.contains("cfg!(test)"),
            "测试构建必须绕开真实历史文件：`MainView` 是用例脚手架，测试调度器一推时钟\
             `poll` 就会落盘 —— 直接写开发机的 `cmd_history.json` 等于跑一次用例抹一次历史。"
        );

        let quit_start = code
            .find("pub fn ui_install_flush_ai_chats_on_quit")
            .unwrap_or_else(|| panic!("退出前落盘的钩子没了：节流窗内的最后几条会丢。"));
        let body_end = code[quit_start..]
            .find("\n}\n")
            .expect("找不到退出钩子的闭合大括号");
        let quit_src = &code[quit_start..quit_start + body_end];
        // 两条数据线**写进同一个钩子**，且各自独立：漏掉任一条的表现都是「另一条好好的」，
        // 极难从现象反推 —— 所以这里两个都钉。
        assert!(
            quit_src.contains("ai_chats_flush_now()")
                && quit_src.contains("cmd_history_flush_now()"),
            "退出钩子必须同时 flush AI 会话与命令历史：两条数据线各自节流，\
             少一条就少一类数据，且失败现象与另一条无关、看不出是同一个钩子的问题。"
        );
    }

    /// 回归护栏（2026-10-09 运行反馈「默认状态的终端输入栏无法到达底部，全屏可以」）：
    /// **终端全屏只改横向布局，纵向几何逐像素不变。**
    ///
    /// # 用户的观察为什么容易被读成「全屏时终端列更高」
    ///
    /// 实测（挂**真终端**、同一窗口尺寸，量的是 `terminal-card` 与 `term-pane`
    /// 的上下边）四种量在两种状态下全部相同：
    ///
    /// ```text
    /// windowed  : win_h=1080  card=[34,1074]  pane=[75,1033]  rows=78  cell_h=12
    /// fullscreen: win_h=1080  card=[34,1074]  pane=[75,1033]  rows=78  cell_h=12
    /// ```
    ///
    /// 卡片底边恒为 `窗口底边 − CARD_GAP`，`term-pane` 底边恒在卡片底边之上 41px
    /// （快捷命令栏 40px + 1px 描边）—— **两态一模一样**，终端网格行数也一样。
    ///
    /// 所以「到不了底」不可能来自布局差异，真正的两个来源是：
    ///
    /// 1. **整数行余量**：行数 = `floor((窗格高 − 上下内边距) / 行高)`，除不尽的
    ///    余数（`0 ≤ r < 行高`）整段留在网格下方，**随窗口尺寸变**。本机口径
    ///    （字号 12 / 行高 1.2 → 单元格 ≈ 16.8px）下：窗口态 1400×900 留 ≈ 6px、
    ///    全屏 1920×1080 留 ≈ 1px，折进 8px 下内边距后是 14px vs 9px
    ///    —— 量级太小，不足以解释肉眼可见的差异。
    /// 2. **内容落点**：提示行落在网格第几行由 alacritty `Grid::grow_lines` 决定
    ///    （有 scrollback 就把内容/光标拉到底部；scrollback 为空则把空白加在
    ///    下方、内容留在原处）。它取决于「变高那一刻有没有历史」，与全屏标记无关。
    ///
    /// 本护栏钉住第 1、2 条的**前提**：纵向几何与全屏无关。将来谁在全屏分支里
    /// 顺手改了终端列的高度、或让终端卡片的高度受内容影响，这里立刻红。
    #[gpui::test]
    fn terminal_fullscreen_keeps_the_column_geometry_identical(cx: &mut gpui::TestAppContext) {
        use super::{AppShell, MainView, TabEntry};
        use crate::dead_session::{blocking_feed_receiver, BlockingReader, SpySink};
        use gpui::AppContext as _;
        use std::cell::RefCell;
        use std::rc::Rc;

        cx.update(|cx| {
            gpui_component::init(cx);
            crate::theme::apply_theme_mode(cx, rustshell_core::theme::ThemeMode::Dark);
        });

        // 真终端：与宿主 `create_terminal_view` 同口径（80×24 起、字号 12、行高 1.2、
        // 内边距顶 4 / 其余 8）—— 用真终端而不是占位卡，是为了连「终端卡片高度是否受
        // 内容影响」一起钉住（占位卡不参与 flex 撑高，测不出这一类回归）。
        let term = cx.update(|cx| {
            let cfg = rustshell_terminal::TerminalConfig {
                cols: 80,
                rows: 24,
                font_size: gpui::px(12.),
                line_height_multiplier: 1.2,
                padding: gpui::Edges {
                    top: gpui::px(4.),
                    right: gpui::px(8.),
                    bottom: gpui::px(8.),
                    left: gpui::px(8.),
                },
                ..rustshell_terminal::TerminalConfig::default()
            };
            cx.new(|cx| {
                rustshell_terminal::TerminalView::new(
                    SpySink::default(),
                    BlockingReader(blocking_feed_receiver()),
                    cfg,
                    cx,
                )
            })
        });

        let slot: Rc<RefCell<Option<gpui::Entity<MainView>>>> = Rc::new(RefCell::new(None));
        let (_, vcx) = {
            let slot = slot.clone();
            cx.add_window_view(move |window, cx| {
                let main = cx.new(|cx| MainView::new(window, cx));
                *slot.borrow_mut() = Some(main.clone());
                let shell = cx.new(|_| AppShell::new(main));
                gpui_component::Root::new(shell, window, cx)
            })
        };
        vcx.run_until_parked();
        let main = slot.borrow().clone().expect("MainView 未创建");
        // 基线：侧栏与 AI 对话列都开着（全屏要藏的就是这两列 + 竖栏）
        vcx.update(|_w, cx| {
            main.update(cx, |m, cx| {
                m.panel_ui.sidebar_collapsed = false;
                m.ai_ui.ai_chat_open = true;
                cx.notify();
            })
        });
        vcx.run_until_parked();

        let sid = "geom-regression";
        main.update_in(vcx, |m, _w, _cx| {
            m.tabs_state.tabs.push(TabEntry {
                session: rustshell_core::session::SessionHandle::pending(
                    sid.to_string(),
                    rustshell_core::session::SessionKind::Ssh,
                    "geom".into(),
                    None,
                ),
                terminal: Some(term.clone()),
                scroll: None,
                connect_error: None,
                connect_started: None,
                dead: None,
                retry: None,
            });
            m.tabs_state.active = Some(sid.to_string());
        });
        vcx.run_until_parked();

        // (窗口底边, 卡片上边, 卡片下边, 窗格上边, 窗格下边, 网格行数)
        let geo = |vcx: &mut gpui::VisualTestContext| -> (f32, f32, f32, f32, f32, usize) {
            // **必须画两帧**（gpui-fast 的 retained 渲染，2026-10-19 升级后）：
            // 第一帧只把新子树（这里是刚 push 进去的标签 → `render_workspace` →
            // `term-pane`）建进布局与缓存，`Div::paint` 还没轮到它 —— 此时
            // `debug_bounds` 查不到 `term-pane`（只有卡片壳与标签栏在）。
            // 第二帧复用缓存、把它画进 scene，`debug_bounds` 才拿得到。
            // 这是**引擎的渲染节奏**，与本用例要钉的「纵向几何与全屏无关」无关：
            // 两帧之间没有用户输入，也没有任何状态变化。
            for _ in 0..2 {
                vcx.update(|w, cx| {
                    w.draw(cx).clear(cx);
                });
            }
            let win_h: f32 = vcx.update(|w, _cx| w.bounds().size.height).into();
            let edge = |vcx: &mut gpui::VisualTestContext, sel: &'static str| -> (f32, f32) {
                let b = vcx
                    .debug_bounds(sel)
                    .unwrap_or_else(|| panic!("找不到 {sel}（debug_selector）"));
                let top: f32 = b.origin.y.into();
                let h: f32 = b.size.height.into();
                (top, top + h)
            };
            let card = edge(vcx, "terminal-card");
            let pane = edge(vcx, "term-pane");
            let rows = vcx.update(|_w, cx| term.read(cx).scroll_metrics().screen_lines);
            (win_h, card.0, card.1, pane.0, pane.1, rows)
        };

        let before = geo(vcx);
        vcx.update(|w, cx| main.update(cx, |m, cx| m.toggle_terminal_fullscreen(w, cx)));
        vcx.run_until_parked();
        let after = geo(vcx);

        assert_eq!(
            before, after,
            "全屏改写了终端列的**纵向**几何。全屏只许让位横向的列（竖栏/侧栏/AI 列），\
             卡片与窗格的上下边必须逐像素不变。\n窗口态：{before:?}\n全屏态：{after:?}"
        );
        // 下界的口径：卡片底边 = 窗口底边 − CARD_GAP（卡片空隙由面板自己的 padding 产生）
        let card_gap = super::CARD_GAP;
        assert!(
            (before.0 - before.2 - card_gap).abs() < 0.5,
            "终端卡片没贴到「窗口底边 − CARD_GAP」：win_h={} card_bottom={}（gap={}）",
            before.0,
            before.2,
            before.0 - before.2
        );
        // 窗格底边必须**高于**卡片底边：下面那条 40px 的快捷命令栏 + 1px 描边还在
        assert!(
            before.2 - before.4 > 38.0,
            "终端卡片底边到窗格底边只剩 {}px —— 快捷命令栏（40px + 1px 描边）被挤掉了，\
             用户看到的正是「终端输入栏离底边还差一截」。",
            before.2 - before.4
        );
    }
}
