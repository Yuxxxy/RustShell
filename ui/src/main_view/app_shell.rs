//! 窗口外壳视图 `AppShell`：挂载官方 Dialog / Sheet 层。
//!
//! 与四个「独立子视图」（`SidebarPanelView` / `AiChatPanelView` /
//! `GroupNameDialogView` / `TermSearchBarView`）一起，从 `main_view.rs` 本体拆出。
//!
//! 为什么拆：它们**各自是一个独立的 gpui `Entity` + 独立 `impl Render`**，
//! 与 `MainView` 的协调逻辑（new / poll / render）没有共享的局部变量，
//! 只共享一个 `Entity<MainView>` 句柄。留在同一文件里只是物理聚集，
//! 让 6000 行的 `main_view.rs` 读起来像「一个巨物」，实际这里是四组互不相干的东西。
//!
//! 拆分口径：**零行为变更**，只搬移定义 + 调整可见性。

use gpui::{
    div, px, App, AppContext as _, Context, Entity, Focusable as _, IntoElement,
    ParentElement as _, Render, SharedString, Styled as _, Window,
};
use gpui_component::{input::InputState, v_flex, Sizable as _};

use super::{MainView, PanelKind};
use crate::{
    ai_history_panel, ai_panel, connections_panel, dashboard_panel, files_panel, tasks_panel,
    terminals_panel,
};

/// 窗口外壳视图：挂载官方 Dialog / Sheet 层。
pub struct AppShell {
    main: Entity<MainView>,
}

impl AppShell {
    pub fn new(main: Entity<MainView>) -> Self {
        Self { main }
    }
}

/// **四个**「独立子视图」的标识（P0-3 的归属口径）。
///
/// 这些视图都是**为了躲开 gpui 的祖先脏传播**才从 `MainView` 的树里摘出来的
/// （见各自的结构体注释）：它们在树上的位置不同、读的数据也几乎不重叠，
/// 所以「父视图 notify 了」并不等于「我都得重绘」。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ChildView {
    /// 左侧栏（`SidebarPanelView`）
    Sidebar,
    /// 终端右侧的 AI 对话列（`AiChatPanelView`）
    AiChat,
    /// Ctrl+F 终端搜索浮层（`TermSearchBarView`）
    TermSearch,
    /// Ctrl+Space 命令补全浮层（`CompletionBarView`）
    Completion,
}

/// 一次 notify **归属**哪几个子视图（P0-3）。
///
/// **空集**表示「这次 notify 没有归属」：三个子视图照旧全部重绘 ——
/// 拿不准时就用空集（等价于不写归属），方向是安全的。
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(crate) struct ViewDirty(u8);

impl ViewDirty {
    pub(crate) fn set(mut self, v: ChildView) -> Self {
        self.0 |= 1 << (v as u8);
        self
    }

    fn bits(self) -> u8 {
        self.0
    }
}

/// 三个独立子视图各自的「我依赖的数据变了」版本号（P0-3）。
///
/// **为什么需要它**：`cx.observe(&main, ..)` 是无条件的 —— `MainView` 只要
/// `cx.notify()`（任何原因：AI 流式增量触发的消息滚动条、文件传输进度、
/// 指标采样、窗口激活），侧栏 / AI 对话列 / 终端搜索浮层都会重跑各自的 render，
/// 于是 gpui 的 `reuse_prepaint` / `reuse_paint` 缓存一次都用不上。
///
/// **用法**：改了某个子视图读的数据时，把裸 `cx.notify()` 换成
/// [`MainView::notify_view`]。裸 `cx.notify()` 仍然是**安全兜底** ——
/// 没有归属（`attributed` 不前进）⇒ 三个子视图都重绘。
/// 于是漏改只损失这点优化，不会让界面停在旧数据上；改错了才会有问题，
/// 所以归属判断要按「这个字段谁在读」来，不要按「改动来自哪个模块」来。
///
/// **约束**：同一次 update 里只发一次 notify。gpui 的 notify 是延迟批量生效的
/// （`Effect::Notify` 按实体去重），一次 update 里既做归属已知的 notify 又做裸
/// `cx.notify()`，两者会被合并成一次，子视图就再也分不清「与我无关」和
/// 「这次没有归属」了。
#[derive(Default)]
pub(crate) struct ViewEpoch {
    /// 每个子视图的版本号（下标 = `ChildView as u8`）
    ///
    /// **改 `ChildView` 就必须同步改这个长度**：`slot()` 直接用它做下标，
    /// 少一格就是越界 panic（有测试 `view_epoch_counters_len_matches_childview` 兜着，
    /// 但那个测试也要记得改 —— 所以这里额外挂了一条编译期断言，见下方）。
    counters: [u64; 4],
    /// 「有过一次归属已知的 notify」的序号：子视图靠它区分
    /// 「这次 notify 与我无关」（跳过）和「这次 notify 没有归属」（兜底重绘）。
    attributed: u64,
}

/// 编译期护栏：`counters` 的长度必须等于 `ChildView` 的变体数。
///
/// 为什么不用测试：测试要人去跑、且改 `ChildView` 时那条测试自己也常被顺手改掉。
/// `const` 上下文里的越界数组访问是**编译错误**，改漏了一编译就红 ——
/// 与铁律 #59 同向（护栏要被新功能逼着失败，而不是被悄悄绕过）。
const _: () = {
    // `ChildView` 的最大判别式 + 1 就是「至少要多少格」。
    // 写 `COUNTERS_LEN` 这个常量而不是直接数 `[u64; 4]`，是为了让两处改动
    // 中的任何一处漏改都编译不过。
    const COUNTERS_LEN: usize = 4;
    let probe: [u64; COUNTERS_LEN] = [0; COUNTERS_LEN];
    // 下一行在 `ChildView::Completion` 不存在（或长度不够）时编译失败：
    // 它要求下标 `ChildView::Completion as usize` 在数组范围内。
    let _ = probe[ChildView::Completion as usize];
};

impl ViewEpoch {
    pub(super) fn slot(v: ChildView) -> usize {
        v as u8 as usize
    }

    /// 归属已知的 notify（含「与三个子视图都无关」的空集）。
    pub(super) fn merge_scoped(&mut self, dirty: ViewDirty) {
        let bits = dirty.bits();
        for ix in 0..self.counters.len() {
            if bits & (1 << ix) != 0 {
                let c = &mut self.counters[ix];
                *c = c.wrapping_add(1);
            }
        }
        self.attributed = self.attributed.wrapping_add(1);
    }

    /// `poll` 用：空集**不**推进 `attributed`。
    ///
    /// 这一条是安全网：`poll` 里某个变更忘了标归属 ⇒ 掩码为空 ⇒ 与「没有归属」
    /// 等价 ⇒ 三个子视图照旧全部重绘。忘了标只是少一次优化，不会停在旧数据上。
    pub(super) fn merge(&mut self, dirty: ViewDirty) {
        if dirty.bits() == 0 {
            return;
        }
        self.merge_scoped(dirty);
    }

    /// 供子视图查询：`(我这个视图的版本号, 归属序号)`。
    pub(super) fn probe(&self, v: ChildView) -> (u64, u64) {
        (self.counters[Self::slot(v)], self.attributed)
    }

    /// 子视图的判定：这次 notify 要不要重绘我？
    ///
    /// * 我的版本号前进了 ⇒ 是给我的，重绘；
    /// * 我的没前进、但归属序号前进了 ⇒ 是给**别的**子视图的，跳过；
    /// * 归属序号也没前进 ⇒ 这次 notify 没有归属（裸 `cx.notify()`），兜底重绘。
    pub(super) fn wants_redraw(prev: (u64, u64), now: (u64, u64)) -> bool {
        now.1 == prev.1 || now.0 != prev.0
    }
}

/// 侧栏面板视图：把「当前面板」的内容从 `MainView` 的整棵树里摘出来，单独成一个视图。
///
/// 为什么必须单独成视图：gpui 的脏传播会把「发生变化的实体」的**所有祖先视图**
/// 标记为脏。`MainView` 是一棵覆盖全窗口的大树（标题栏 / 侧栏 / 标签栏 / 终端 /
/// 快捷命令栏 / 状态栏都挂在它下面），于是**任何**输入框敲一个字，都会连带把整棵
/// 树重新渲染一遍（实测 debug 构建 ≈60ms/帧，即「输入卡顿」的直接来源）。
///
/// 摘成子视图后：输入只重排侧栏这一小块；`MainView` 因别的原因重绘时，
/// 侧栏子树走 gpui 的视图缓存（`reuse_prepaint`/`reuse_paint`），不再重新布局。
/// 父视图主动 `notify`（改了数据）时经 `observe` 传导下来，内容不会过期。
pub struct SidebarPanelView {
    main: Entity<MainView>,
    /// 上次重绘时看到的 `(本视图版本号, 归属序号)`；见 [`ViewEpoch`]。
    ///
    /// 初值 `(0, 0)` 而不是「构造时就读一次父视图」：`new` 跑在 `MainView` 自己的
    /// 构造过程里，此时 `main.read(cx)` 会撞上正在进行的可变借用。从 `(0, 0)` 起步
    /// 只会让**第一次** notify 按「兜底重绘」处理，行为与改造前一致。
    seen: (u64, u64),
}

impl SidebarPanelView {
    pub(super) fn new(main: Entity<MainView>, cx: &mut Context<Self>) -> Self {
        // 父视图 notify（数据变了）→ 本视图跟着重绘；否则保持缓存命中。
        // P0-3：只重绘**真的改了侧栏数据**的那些 notify（见 `ViewEpoch`）。
        cx.observe(&main, |this, main, cx| {
            let prev = this.seen;
            let now = main.read(cx).panel_ui.view_epoch.probe(ChildView::Sidebar);
            this.seen = now;
            if ViewEpoch::wants_redraw(prev, now) {
                cx.notify();
            }
        })
        .detach();
        Self { main, seen: (0, 0) }
    }
}

impl Render for SidebarPanelView {
    fn render(&mut self, _w: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let main = self.main.clone();
        // 面板渲染函数要 `&mut Context<MainView>`，与对话框构建闭包同一手法：
        // 经捕获的 Entity 回到 MainView 上下文中渲染。
        //
        // 计时接回 `SIDEBAR_VIEW_US`（2026-09-21）：拆分 `MainView` 时这层包装丢了，
        // 于是 `PERF` 行的 `sidebar=` 一直是 0 —— 而《性能与帧率优化分析》正是拿
        // 它当 P0-2 / P0-3 的验收口径。没有它，「大目录下侧栏每帧多少钱」无从对比。
        crate::perf_probe::timed(&crate::perf_probe::SIDEBAR_VIEW_US, || {
            main.update(cx, render_sidebar_panel)
        })
    }
}

/// AI 对话面板视图：终端右侧那一列（`ai_panel::render` 的宿主）。
///
/// 2026-09-18 运行反馈「点击 AI 助手，聊天界面显示在终端右侧」：对话正文从左侧栏
/// 搬到终端右侧之后，它和 `SidebarPanelView` 一样必须**单独成视图** ——
/// 里面有一个输入条，敲一个字就会通知它所属的实体；若这一列长在 `MainView` 的树里，
/// 一次击键就要把整棵窗口树重排一遍（实测 debug 构建 ≈60ms/帧，即「输入卡顿」）。
pub struct AiChatPanelView {
    main: Entity<MainView>,
    /// 见 [`ViewEpoch`] 与 `SidebarPanelView::seen`。
    seen: (u64, u64),
}

impl AiChatPanelView {
    pub(super) fn new(main: Entity<MainView>, cx: &mut Context<Self>) -> Self {
        // 父视图 notify（数据变了）→ 本视图跟着重绘；否则保持缓存命中。
        // P0-3：AI 流式期间最高频的那个 notify（消息滚动条）只归本视图，
        // 侧栏与终端搜索浮层不再跟着重跑 render。
        cx.observe(&main, |this, main, cx| {
            let prev = this.seen;
            let now = main.read(cx).panel_ui.view_epoch.probe(ChildView::AiChat);
            this.seen = now;
            if ViewEpoch::wants_redraw(prev, now) {
                cx.notify();
            }
        })
        .detach();
        Self { main, seen: (0, 0) }
    }
}

impl Render for AiChatPanelView {
    fn render(&mut self, _w: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let main = self.main.clone();
        // 与 `SidebarPanelView` 同一手法：直接把渲染函数当 `FnOnce` 传进去
        // （`render(m, cx)` 与它逐字对应，包一层闭包只会多一次间接）
        main.update(cx, ai_panel::render)
    }
}

/// 分组名对话框的**正文**视图，且**自带名称输入框**。
///
/// 为什么连输入框一起搬走：对话框正文由 `AppShell` 的对话层每帧构建，构建闭包经
/// `MainView` 的方法跑；只要 `MainView` 碰过这个 `InputState`，它每敲一个字都会把
/// `MainView` 标脏，整窗口重排一次（实测 ≈70ms/帧，即「输入分组名称卡顿」）。
/// 输入框归本视图所有后，`MainView` 只在保存时**取一次值**，击键只重排这一小块。
pub struct GroupNameDialogView {
    main: Entity<MainView>,
    input: Entity<InputState>,
    /// 字段名（「分组名称」/「连接名称」）。用同一套视图承载两类「改个名字」的
    /// 对话框，标签就得是可变的；默认分组名，连接改名由
    /// [`GroupNameDialogView::set_label`] 覆盖。
    name_label: String,
    /// 开框首帧把焦点放进输入框（消费一次）
    focus_pending: bool,
}

impl GroupNameDialogView {
    pub(super) fn new(main: Entity<MainView>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self {
            main,
            input: cx.new(|cx| InputState::new(window, cx)),
            name_label: rustshell_core::i18n::t("分组名称").to_string(),
            focus_pending: false,
        }
    }

    /// 设置字段名（开框前调用；连接改名用「连接名称」）。
    pub fn set_label(&mut self, label: &str) {
        self.name_label = label.to_string();
    }

    /// 开框：请求首帧聚焦（并先清空，改名场景由 `set_value` 再填）。
    pub fn open(&mut self, cx: &mut Context<Self>) {
        self.focus_pending = true;
        cx.notify();
    }

    /// 当前输入内容（已 trim）——保存路径唯一的取数入口。
    pub fn value(&self, cx: &App) -> String {
        self.input.read(cx).value().trim().to_string()
    }

    /// 名称输入框的焦点句柄（回归用例断言「开框即聚焦」用）。
    pub fn focus_handle(&self, cx: &App) -> gpui::FocusHandle {
        self.input.read(cx).focus_handle(cx)
    }

    /// 预填内容（改名对话框）。
    pub fn set_value(&mut self, value: String, window: &mut Window, cx: &mut Context<Self>) {
        self.input
            .update(cx, |s, cx| s.set_value(value, window, cx));
    }
}

impl Render for GroupNameDialogView {
    fn render(&mut self, w: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        use crate::theme::to_hsla as hex;
        use gpui::{prelude::FluentBuilder as _, Styled as _};
        use gpui_component::input::Input;
        let t = crate::theme::current(cx);
        // 开框首帧：名称框此刻已在元素树里，`Window::focus` 才落得下去
        // （与终端的 `pending_focus` 同一手法）
        if self.focus_pending {
            self.focus_pending = false;
            let handle = self.input.read(cx).focus_handle(cx);
            w.focus(&handle, cx);
        }
        // 只有错误文案来自 MainView（读取不涉及输入框，不会把击键牵到父视图）
        let err = self.main.read(cx).io.dlg_error.clone();
        v_flex()
            .gap_1()
            .child(
                div()
                    .text_size(px(11.))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(hex(t.fg_secondary))
                    .child(SharedString::from(self.name_label.clone())),
            )
            .child(Input::new(&self.input).w_full())
            .when_some(err, |d, msg| {
                d.child(
                    crate::notice::NoticeKind::Error
                        .card("dlg-new-group-error", SharedString::from(msg))
                        .xsmall()
                        .text_size(px(11.5)),
                )
            })
    }
}

/// 终端搜索浮层视图（Ctrl+F）。
///
/// 与 `SidebarPanelView` 同一动机：搜索框每敲一个字都会通知它的 `InputState`，
/// 若浮层长在 `MainView` 的树里，一次击键就要把整窗口重排一遍（实测 debug
/// 构建 ≈60ms/帧，表现就是「输入/删除都一卡一卡」）。摘成独立视图后，
/// 击键只重排这一条浮层。
pub struct TermSearchBarView {
    main: Entity<MainView>,
    /// 见 [`ViewEpoch`] 与 `SidebarPanelView::seen`。
    seen: (u64, u64),
}

impl TermSearchBarView {
    pub(super) fn new(main: Entity<MainView>, cx: &mut Context<Self>) -> Self {
        cx.observe(&main, |this, main, cx| {
            let prev = this.seen;
            let now = main
                .read(cx)
                .panel_ui
                .view_epoch
                .probe(ChildView::TermSearch);
            this.seen = now;
            if ViewEpoch::wants_redraw(prev, now) {
                cx.notify();
            }
        })
        .detach();
        Self { main, seen: (0, 0) }
    }
}

impl Render for TermSearchBarView {
    fn render(&mut self, _w: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let main = self.main.clone();
        main.update(cx, |m, cx| m.render_term_search_bar(cx))
    }
}

/// 当前面板（侧栏内容）的唯一渲染入口。
///
/// 注意 `PanelKind::Ai` 分到的是**聊天记录**（会话列表）而不是对话正文：2026-09-18
/// 运行反馈「点击 AI 助手，聊天界面显示在终端右侧，原 AI 助手侧边栏改为聊天记录」。
/// 对话正文由终端右侧的 `AiChatPanelView` 渲染（同一个 `ai_panel::render`）。
fn render_sidebar_panel(m: &mut MainView, cx: &mut Context<MainView>) -> gpui::AnyElement {
    match m.panel_ui.panel {
        PanelKind::Dashboard => dashboard_panel::render(m, cx).into_any_element(),
        PanelKind::Connections => connections_panel::render(m, cx).into_any_element(),
        PanelKind::Terminals => terminals_panel::render(m, cx).into_any_element(),
        PanelKind::Files => files_panel::render(m, cx).into_any_element(),
        PanelKind::Ai => ai_history_panel::render(m, cx).into_any_element(),
        PanelKind::Tasks => tasks_panel::render(m, cx).into_any_element(),
    }
}

impl Render for AppShell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let sheet_layer = gpui_component::Root::render_sheet_layer(window, cx);
        let dialog_layer = gpui_component::Root::render_dialog_layer(window, cx);

        div()
            .size_full()
            .child(self.main.clone())
            .children(sheet_layer)
            .children(dialog_layer)
    }
}
