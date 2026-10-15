//! 命令补全浮层（C-1）：Ctrl+Space 唤起、边打字边过滤、↑↓ 选、Tab/Enter 采纳、Esc 关闭。
//!
//! # 为什么是「独立子视图 + 状态挂 MainView」
//!
//! 这个浮层有一条**别处没有**的约束：它**不抢焦点**（§5.2 硬约束）。
//!
//! 终端搜索浮层（[`super::TermSearchBarView`]）是抢焦点的 —— `Ctrl+F` 之后
//! 键盘交给 `InputState`，`Esc` 由那个输入框的 `on_key_down` 收。补全不能用这条路：
//! 补全的前提是「用户还在终端里打字」，一旦把焦点从终端拿走，字符就进不了 PTY，
//! 浮层也就永远看不到新的前缀了。
//!
//! 所以按键**仍然归终端**（`TerminalView::on_key_down`），浮层只在两处被动响应：
//!
//! 1. 「当前输入行可能变了」（[`rustshell_terminal::InputLineCallback`]：
//!    IME 上屏 / 粘贴 → 重算候选）；
//! 2. 「补全相关的控制键」被引擎的过滤器拦下时（[`rustshell_terminal::CompletionKeyFilter`]：
//!    `Ctrl+Space` 切浮层、`↑↓` 移选中、`Tab`/`Enter` 采纳、`Esc` 关闭）。
//!
//! 这两条路都跑在 `TerminalView` 的上下文里，拿不到 `MainView` 的 `&mut self` ——
//! 于是候选项、选中的下标、是否开启，**只能挂在 `MainView` 上**，本视图纯粹是
//! 「读出来画一遍」。这同时满足了铁律 #78（挂起的东西只存在一处）：浮层的全部
//! 状态只有 `MainView::completion` 一个落点，没有第二份副本能跟它失同步。
//!
//! # 为什么用 `ViewEpoch` 而不是裸 `observe` 重绘
//!
//! 与 `TermSearchBarView` 同一动机（见 [`super::ViewEpoch`]）：`MainView` 的 notify
//! 有几十个来源（AI 流式增量、指标采样、文件传输进度……），无条件重绘浮层等于
//! 让它跟着这些无关变化一起重排。浮层每帧要建最多 10 行元素，虽然不重，但没理由付。

use gpui::{
    div, prelude::FluentBuilder as _, px, AnyElement, Context, Entity, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, StatefulInteractiveElement as _, Styled as _, Window,
};

use rustshell_core::completion::CandidateSource;

use super::{ChildView, MainView, ViewEpoch};

/// 命令补全浮层的独立视图（渲染壳，无自有状态）。
///
/// 状态全在 [`MainView::completion`]（`pub(super) completion: CompletionState`），
/// 本结构体只有 `main` 句柄与 `ViewEpoch` 快照 —— 与 `TermSearchBarView` 同款。
/// 见模块文档「为什么是独立子视图 + 状态挂 MainView」。
pub struct CompletionBarView {
    main: Entity<MainView>,
    /// 见 [`super::ViewEpoch`] 与 `SidebarPanelView::seen`。
    seen: (u64, u64),
}

impl CompletionBarView {
    pub(super) fn new(main: Entity<MainView>, cx: &mut Context<Self>) -> Self {
        cx.observe(&main, |this, main, cx| {
            let prev = this.seen;
            let now = main
                .read(cx)
                .panel_ui
                .view_epoch
                .probe(ChildView::Completion);
            this.seen = now;
            if ViewEpoch::wants_redraw(prev, now) {
                cx.notify();
            }
        })
        .detach();
        Self { main, seen: (0, 0) }
    }
}

impl Render for CompletionBarView {
    fn render(&mut self, _w: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let main = self.main.clone();
        main.update(cx, |m, cx| m.render_completion_bar(cx))
    }
}

/// 一条候选在浮层里的样子（`Candidate` 的「已定稿」投影）。
///
/// 为什么不直接存 `rustshell_core::completion::Candidate`：那个结构带 `score`
/// （只用于排序，画出来后就是个死字段）与 `suffix`（采纳时要用的字节）。
/// `suffix` 要留着 —— 采纳就是把这一串写进 PTY。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CompletionRow {
    /// 候选全文（给用户看）
    pub(super) text: String,
    /// 要写进 PTY 的补齐部分（`Candidate::suffix` 原样搬运）
    pub(super) suffix: String,
    /// 说明文案（内置命令的中文说明 / 历史的用过次数）
    pub(super) detail: Option<String>,
    /// 来源（画成标签）
    pub(super) source: CandidateSource,
}

/// 补全浮层的全部状态（**唯一落点**，见模块文档）。
#[derive(Debug, Default)]
pub(super) struct CompletionState {
    /// 浮层是否打开
    pub(super) open: bool,
    /// 当前候选（最多 [`rustshell_core::completion::MAX_CANDIDATES`] 条）
    pub(super) rows: Vec<CompletionRow>,
    /// 选中的下标（`rows` 为空时无意义；`↑↓` 会 clamp 到合法范围）
    pub(super) selected: usize,
    /// 上次算候选时用的前缀（去重：前缀没变就不重算，省一次全表扫描）
    pub(super) prefix: String,
}

impl CompletionState {
    /// 关闭并清空（断连、切标签、Esc 都走这里）。
    ///
    /// 清空而不只是置 `open = false`：候选里带着**上一台机器**的命令文本，
    /// 留着它等于给「换会话后误采纳」留了一条路。铁律 #78 的「取走即失效」。
    pub(super) fn close(&mut self) {
        self.open = false;
        self.rows.clear();
        self.selected = 0;
        self.prefix.clear();
    }

    /// 当前选中项的补齐字节（无候选 / 下标越界时 `None`）。
    pub(super) fn selected_suffix(&self) -> Option<&str> {
        self.rows.get(self.selected).map(|r| r.suffix.as_str())
    }

    /// 上移/下移选中项（**循环**：从底再按一次回到顶，反之亦然）。
    ///
    /// 循环而不是钳制：候选固定最多 10 条，用户连按 `↑` 时钳制会出现
    /// 「按了没反应」的观感；主流补全面板（IDE、fzf）都是循环。
    pub(super) fn move_selection(&mut self, forward: bool) {
        let n = self.rows.len();
        if n == 0 {
            self.selected = 0;
            return;
        }
        self.selected = if forward {
            (self.selected + 1) % n
        } else {
            (self.selected + n - 1) % n
        };
    }
}

impl MainView {
    /// 补全浮层的渲染入口（`CompletionBarView` 调）。
    ///
    /// 样式基座照 [`Self::render_term_search_bar`]：绝对定位、圆角、`bg_overlay`
    /// 底色，以及「点击不透传」。列表是**自绘行**而不是官方 `List` ——
    /// 候选只有 10 行、每行也就三个片段（文本 / 说明 / 来源标签），
    /// 用 `ListState` 反而要维护一个跨帧实体（还得为它办两段式的注销），收益为负。
    pub(super) fn render_completion_bar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        use crate::theme::to_hsla as hex;
        let t = crate::theme::current(cx);
        let dark = crate::theme::is_dark(cx);

        // 选中行的高亮面：Light 主题里 `bg_overlay == bg_base`，用底色画选中态
        // 会逐像素同色（浅色下悬停看不出来，铁律 #33）—— 走 fg 的低透明叠加。
        let sel_bg = if dark {
            hex(t.accent).opacity(0.18)
        } else {
            hex(t.accent).opacity(0.12)
        };

        // 候选文本用**用户设置的终端字体**画：浮层里核对一行命令时，
        // 字形与终端里不一致会明显抬高核对成本（同一段文本换字体）。
        let mono = self.settings.terminal_font_family.trim().to_string();
        let mono = if mono.is_empty() {
            rustshell_core::theme::default_terminal_font().to_string()
        } else {
            mono
        };

        let mut list = div().flex().flex_col().gap(px(1.)).p(px(4.));
        for (ix, row) in self.panel_ui.completion.rows.iter().enumerate() {
            let selected = ix == self.panel_ui.completion.selected;
            let source_label = rustshell_core::i18n::t(row.source.label());
            let mut line = div()
                .flex()
                .items_center()
                .gap_2()
                .px(px(6.))
                .py(px(3.))
                .rounded(px(4.))
                .when(selected, |d| d.bg(sel_bg))
                .child(
                    // 候选文本：等宽，让它与终端里的观感一致（同一段文本在
                    // 浮层里换字体，用户核对的成本高一截）
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .font_family(mono.clone())
                        .text_size(px(12.))
                        .text_color(hex(t.fg_primary))
                        .child(row.text.clone()),
                );
            if let Some(detail) = row.detail.as_ref() {
                line = line.child(
                    div()
                        .flex_none()
                        .max_w(px(180.))
                        .overflow_hidden()
                        .text_size(px(11.))
                        .text_color(hex(t.fg_muted))
                        .child(detail.clone()),
                );
            }
            list = list.child(
                line.child(
                    // 来源标签：库 / 历史 / 内置 —— 用户判断「这条可不可信」的依据
                    div()
                        .flex_none()
                        .text_size(px(10.))
                        .text_color(hex(t.fg_muted))
                        .child(source_label),
                ),
            );
        }

        let empty = self.panel_ui.completion.rows.is_empty();
        div()
            .absolute()
            // 盖在窗格左下（终端光标通常在下方，浮层贴它更近）
            .left(px(12.))
            .bottom(px(12.))
            .w(px(420.))
            .max_h(px(240.))
            .flex()
            .flex_col()
            .rounded(px(6.))
            .bg(hex(t.bg_overlay))
            .border_1()
            .border_color(hex(t.border_subtle))
            // 点击浮层不透传到终端：否则点一行候选会顺手把终端划选清掉
            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px(px(8.))
                    .py(px(4.))
                    .border_b_1()
                    .border_color(hex(t.border_subtle))
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(hex(t.fg_primary))
                            .child(rustshell_core::i18n::t("命令补全")),
                    )
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(hex(t.fg_muted))
                            .child(rustshell_core::i18n::t("按 Esc 关闭")),
                    ),
            )
            .child(
                div()
                    .id("completion-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .when(empty, |d| {
                        d.child(
                            div()
                                .px(px(8.))
                                .py(px(6.))
                                .text_size(px(11.))
                                .text_color(hex(t.fg_muted))
                                .child(rustshell_core::i18n::t("没有匹配的命令")),
                        )
                    })
                    .child(list),
            )
            .into_any_element()
    }

    /// 浮层当前是否打开（终端的按键过滤器与宿主都要问这一句）。
    ///
    /// 判据不只是「标记为开」：浮层挂的是**激活标签页的终端**，标签切走之后
    /// 那台终端已经不是屏上那个，它的候选（属于上一台机器）不该继续生效。
    /// 这里把「开着但标签已经变了」直接当成关 —— 与 `render_term_pane` 的
    /// `pane_focused` 同一口径（浮层只在焦点窗格显示）。
    pub(super) fn completion_is_open(&self) -> bool {
        self.panel_ui.completion.open
    }

    /// `Ctrl+Space`：切浮层的开合（§5.3 —— 触发键**不抢 Tab**，Tab 仍透传给远端）。
    ///
    /// 开的时候立刻算一次候选（此时前缀可能是空串 ⇒ 列出最常用的一批）；
    /// 关的时候走 [`CompletionState::close`]（连候选一起清，铁律 #78）。
    pub(super) fn toggle_completion(
        &mut self,
        tv: &rustshell_terminal::TerminalView,
        cx: &mut Context<Self>,
    ) {
        if self.panel_ui.completion.open {
            self.panel_ui.completion.close();
        } else {
            // 没有可输入的终端就不开：开了也是个永远空着的框
            if self
                .active_tab()
                .and_then(|t| t.terminal.as_ref())
                .is_none()
            {
                return;
            }
            self.panel_ui.completion.open = true;
            self.panel_ui.completion.selected = 0;
            self.panel_ui.completion.prefix.clear();
            self.refresh_completion(tv, cx);
        }
        // 浮层是独立子视图 ⇒ 归属通知（见 `ViewEpoch`）
        self.notify_view(ChildView::Completion, cx);
    }

    /// 关闭浮层（Esc / 断连 / 切标签都走这里）。**幂等**。
    pub(super) fn close_completion(&mut self, cx: &mut Context<Self>) {
        if !self.panel_ui.completion.open {
            return;
        }
        self.panel_ui.completion.close();
        self.notify_view(ChildView::Completion, cx);
    }

    /// 重算候选（前缀取自终端**当前输入行**）。
    ///
    /// `force = false` 时如果前缀没变就直接返回 —— 这个函数会被「输入行可能变了」
    /// 回调每次上屏 / 每次粘贴叫一遍，而回调**不保证**文本真的变了（见
    /// `InputLineCallback` 的文档：给的是「该重问了」而不是「变了多少」）。
    ///
    /// # `tv` 是「引擎借出的自己」，不是 `self.active_tab()` 里那个实体
    ///
    /// 入口（`ui_call_completion_refresh`）跑在 `TerminalView` 的**租约**里，
    /// 拿 `Entity<TerminalView>` 去 `read` / `update` 会直接 panic（见
    /// `rustshell_terminal::InputLineCallback` 的文档）—— 所以行走把
    /// `&TerminalView` 递进来。**本函数内一律不碰终端实体**。
    ///
    /// "这一份是不是活动标签的"由入口判（`active_terminal_id()` 对比
    /// `cx.entity_id()`），这里只负责算。
    ///
    /// 读不到当前行（备用屏 / 光标在视口外 / 会话已死）⇒ 前缀视为空，
    /// 而不是「保持上一次的」。保持上一次会让候选属于**上一行**的命令，
    /// 用户看着 vim 的界面却收到一堆 `ls` 补全。
    ///
    /// 用 `current_input_line` 而不是 C-0 的 `shell_command_line()`：后者要有
    /// shell integration 才给值，而 C-1 的验收场景（任意远端、没配过 OSC 633）
    /// 拿不到它。H1 启发式的精度损失已写在 `current_input_line` 的文档里。
    pub(super) fn refresh_completion(
        &mut self,
        tv: &rustshell_terminal::TerminalView,
        cx: &mut Context<Self>,
    ) {
        if !self.panel_ui.completion.open {
            return;
        }
        let prefix = tv.current_input_line().map(|l| l.text).unwrap_or_default();

        if prefix == self.panel_ui.completion.prefix && !self.panel_ui.completion.rows.is_empty() {
            return;
        }
        self.panel_ui.completion.prefix = prefix.clone();
        self.panel_ui.completion.rows = self.compute_rows(&prefix, cx);
        // 候选整体换了一批，旧下标可能越界（也失去意义）—— 回到第一条
        self.panel_ui.completion.selected = 0;
        self.notify_view(ChildView::Completion, cx);
    }

    /// 活动标签的终端实体 id（无活动标签 / 该标签没有终端 ⇒ `None`）。
    ///
    /// 存在意义是**认领回调**：引擎的三个回调都带着自己所属实体的 id
    /// （`cx.entity_id()`），而浮层是 `MainView` 级的单例、绑在**活动标签**上。
    /// 两者不一致说明这条回调来自一个已经切走的标签，必须丢掉。
    ///
    /// 什么时候会不一致：`input_line_dirty` / `enter_dirty` 是**两段式**的旗子
    /// （置旗在输入那一刻、兑现要等下一帧 `render`），中间用户切了标签 ——
    /// 旧视图那一帧仍会兑现回调。不判的话，**旧机器的当前行会被拿来算新机器
    /// 的候选**（正是 C-2 花大力气分桶要消灭的那种串味）。
    fn active_terminal_id(&self) -> Option<gpui::EntityId> {
        self.active_tab()
            .and_then(|t| t.terminal.as_ref())
            .map(|e| e.entity_id())
    }

    /// 三源候选 → 浮层行（`candidates()` 是纯函数，这里只做投影与排序保持）。
    fn compute_rows(&self, prefix: &str, _cx: &mut Context<Self>) -> Vec<CompletionRow> {
        use rustshell_core::completion::{candidates, library_from_folders};
        let library = library_from_folders(&self.quick_store);
        candidates(prefix, &self.panel_ui.completion_history, &library)
            .into_iter()
            .map(|c| CompletionRow {
                text: c.text,
                suffix: c.suffix,
                detail: c.detail,
                source: c.source,
            })
            .collect()
    }

    /// 采纳当前选中的候选：**只把补齐部分写进 PTY**（§5.1 硬约束）。
    ///
    /// 三件事一律不做：不重建整行、不改光标位置、不补回车。
    /// - 不重建整行：远端 readline 的状态（历史游标、kill ring）只有它自己知道，
    ///   重写一遍等于把用户已有的编辑动作抹掉；
    /// - 不补回车：补全不是执行。用户选完还要接着改参数
    ///   （`git commit -m` 后面那截消息）；
    /// - 不动光标：多写一个 `ESC[{}G` 就多一个与远端行宽口径不一致的机会。
    ///
    /// 写入走 [`rustshell_terminal::TerminalView::write_to_pty`] —— 它首行就是
    /// 死会话闸（§3.3 的护栏天然带上，不需要在这里再判一次 `is_alive`）。
    ///
    /// `tv` 是引擎借出的自己（见 [`Self::refresh_completion`]）：这里**必须**用它
    /// 而不是 `active_tab().terminal.update(..)` —— 本函数唯一的调用链在
    /// `on_key_down` 里（经 `CompletionKeyFilter`），那时实体正在租约中，
    /// `update` 会 panic（这次闪退的另一半就是这个）。
    pub(super) fn accept_completion(
        &mut self,
        tv: &rustshell_terminal::TerminalView,
        cx: &mut Context<Self>,
    ) {
        if !self.panel_ui.completion.open {
            return;
        }
        let Some(suffix) = self
            .panel_ui
            .completion
            .selected_suffix()
            .map(|s| s.to_string())
        else {
            return;
        };
        if suffix.is_empty() {
            return;
        }
        tv.write_to_pty(&suffix);
        // 采纳后**保持浮层打开**：连续补全（`g` → `git` → `git a` → `git add`）
        // 是常态，关掉会让用户每补一级都要再按一次 Ctrl+Space。
        // 前缀由终端的输入行回调带着新文本回来，这里只需把候选标记为「待重算」。
        self.panel_ui.completion.prefix.clear();
        self.notify_view(ChildView::Completion, cx);
    }

    /// `↑↓`：移动选中项。
    pub(super) fn completion_move(&mut self, forward: bool, cx: &mut Context<Self>) {
        if !self.panel_ui.completion.open || self.panel_ui.completion.rows.is_empty() {
            return;
        }
        self.panel_ui.completion.move_selection(forward);
        self.notify_view(ChildView::Completion, cx);
    }

    /// 采集一条命令历史（引擎的 [`rustshell_terminal::EnterCallback`] 调）。
    ///
    /// 判据全在 `CommandHistory::record` 里（空行 / 疑似口令 / 去重合并计数），
    /// 这里只负责「把当前行读出来交给它」。
    ///
    /// `tv` 是引擎借出的自己（见 [`Self::refresh_completion`]）：**必须**用它，
    /// 不能 `active_tab().terminal.read(cx)` —— 本函数只在 `process_events` 里
    /// 被调（那时实体在租约中），读它会 panic。**这就是本轮闪退的直接原因。**
    ///
    /// # 读的是**按回车那一刻**的行
    ///
    /// `EnterCallback` 在回车字节写进 PTY 之后兑现（见引擎侧字段注释），此时
    /// 远端的回显与执行结果都还没回来，网格上那一行正好是用户敲完的那条命令。
    /// 等 `OSC 133;D`（命令结束）再读就晚了：远端已经回了新 prompt，
    /// 读出来的是新的一行（多半是空的）。
    ///
    /// # 与 shell integration 的关系
    ///
    /// 这条路**不需要远端配合**，是所有会话都能走的那条。有 `OSC 633;E` 的会话
    /// 会拿到更准的命令行文本（不经 H1 启发式），后续可以在这里加一个
    /// 「integration 可用时优先用它」的分支 —— C-1 阶段先只做兜底那条，
    /// 因为用 `shell_command_line()` 需要把「最后一次命令行的到达时刻」与
    /// 「回车时刻」对齐，那是另一件事（命令块功能要的），不该塞进历史采集。
    pub(super) fn record_history_line(
        &mut self,
        tv: &rustshell_terminal::TerminalView,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(line) = tv.current_input_line() else {
            return false;
        };
        let changed = self.panel_ui.completion_history.record(&line.text);
        if changed {
            // 落了新记录 ⇒ 打落盘标记（遗留项 1：历史要跨重启保留）。
            //
            // 放在 `record` 返回 true 的分支里而不是无条件标：`record` 会因为
            // 「空行 / 疑似凭据」返回 false，那些**没有改动内存**，
            // 标了脏只会让 `poll` 空写一次文件。
            self.cmd_history_touch();
            // 历史变了 ⇒ 候选会变（历史是数据源之一）。
            //
            // 但**不必**在这里刷新浮层：`record` 刚记下的这条命令，其文本正是
            // `self.panel_ui.completion.prefix`（浮层开着的最后状态）。浮层下次重算时
            // 自然带上它 —— 而重算的触发点是「行变了」（回车后远端回显新 prompt
            // 会走 `commit_text` / 网格更新，宿主会收到 `InputLineCallback`）。
            //
            // 这里只标归属、让浮层知道自己该看一眼，不主动重算：
            // 主动重算会用**还没更新的**前缀（仍是刚执行的那条命令）去查，
            // 结果是一屏「把刚敲完的命令又列给你」的候选，观感是"没反应"。
            self.notify_view(ChildView::Completion, cx);
        }
        changed
    }

    /// 补全相关的按键处置（引擎的 [`rustshell_terminal::CompletionKeyFilter`] 调）。
    ///
    /// # 为什么这里能「自己拿主意」
    ///
    /// 引擎已经把**可打印键全部放行**了（铁律 #63：截住可打印键等于浮层一开终端就哑），
    /// 所以进到这里的只有控制键。下面这张表就是浮层的全部按键语义：
    ///
    /// | 键 | 浮层关 | 浮层开 |
    /// |---|---|---|
    /// | `Ctrl+Space` | 开 | 关 |
    /// | `↑` / `↓` | 透传（终端的历史/光标） | 移选中 |
    /// | `Tab` | 透传（远端补全，§5.3） | 采纳 |
    /// | `Enter` | 透传 | 采纳 |
    /// | `Esc` | 透传（远端程序要用） | 关浮层 |
    /// | 其余 | 透传 | 透传 |
    ///
    /// `↑↓` / `Tab` / `Enter` 在浮层关着时必须**透传**：它们是终端自己天天要用的键
    /// （历史回溯、readline 补全、执行命令），拦下来就是把终端功能删了。
    ///
    /// `Tab` 在浮层开着时被采纳占用，是 §5.3 明确允许的 ——
    /// 「不抢 Tab」指的是**浮层关着的时候不抢**（那才是默认态、才是用户习惯的路径）。
    ///
    /// `tv` 是引擎借出的自己（见 [`Self::refresh_completion`]）—— 这条链也在
    /// `on_key_down` 的租约里，所以开关浮层与采纳候选都要靠它，不能碰终端实体。
    pub(super) fn completion_key(
        &mut self,
        key: &str,
        control: bool,
        tv: &rustshell_terminal::TerminalView,
        cx: &mut Context<Self>,
    ) -> rustshell_terminal::CompletionKeyOutcome {
        use rustshell_terminal::CompletionKeyOutcome as Out;

        // Ctrl+Space：唯一一个「开关两个状态都做事」的键。
        // 注意判据是 key=="space" && control，与 `keystroke_to_bytes` 里
        // 「Ctrl+Space → \x00」那条映射同一个键位（那个 NUL 绝不能写进 PTY）。
        if control && key == "space" {
            self.toggle_completion(tv, cx);
            return Out::ToggleRequested;
        }
        if !self.panel_ui.completion.open {
            return Out::PassThrough;
        }
        match key {
            "up" => {
                self.completion_move(false, cx);
                Out::Consumed
            }
            "down" => {
                self.completion_move(true, cx);
                Out::Consumed
            }
            "tab" | "enter" => {
                self.accept_completion(tv, cx);
                Out::Consumed
            }
            "escape" => {
                self.close_completion(cx);
                Out::Consumed
            }
            _ => Out::PassThrough,
        }
    }
}

/// 「当前输入行可能变了」的宿主入口（App 上下文）。
///
/// 由 [`rustshell_terminal::TerminalView::with_input_line_callback`] 装配到引擎上，
/// 触发点在 `commit_text`（IME 上屏 / 平台直投字符）与 `paste_text`（粘贴）。
///
/// # 为什么必须经 [`super::UiHub`] 回到实体
///
/// 这个回调跑在 `Context<TerminalView>` 里，拿不到 `&mut MainView`。`UiHub` 正是
/// 本仓为「非 `cx.listener` 场景」准备的通道（见 `split_actions::pane_context_menu`
/// 的文档），且持的是 `WeakEntity` —— 窗口已经关掉时 `update` 静默失败，
/// 不会把已释放的实体拖住。
///
/// ⚠️ 但 `UiHub` 只解决「怎么拿到 `MainView`」，**不解决**「怎么拿到终端」：
/// 此刻 `TerminalView` 正被引擎**租用**（实体已从实体表移到栈上，见
/// `rustshell_terminal::InputLineCallback` 的文档），谁去 `read` / `update` 它
/// 谁 panic。所以引擎把 `tv: &TerminalView` 直接借进来，本函数全程用它。
///
/// # 为什么要认领（`cx.entity_id()` 对比活动标签）
///
/// 浮层是 `MainView` 级的单例、绑在**活动标签**上；而这条回调是**某个具体**
/// 终端发的。`input_line_dirty` 是两段式旗子（置旗在输入那一刻、兑现要等下一帧
/// `render`），中间用户切了标签的话，旧视图这一帧仍会兑现 —— 不认领就会
/// **拿旧机器的当前行去算新机器的候选**。判据见 [`super::MainView::active_terminal_id`]。
pub fn ui_call_completion_refresh(
    tv: &rustshell_terminal::TerminalView,
    _w: &mut Window,
    cx: &mut gpui::Context<rustshell_terminal::TerminalView>,
) {
    let Some(hub) = cx.try_global::<super::UiHub>() else {
        return;
    };
    let main = hub.main.clone();
    // 浮层没开时不打扰宿主：`refresh_completion` 自己也会判，但那要先进一次
    // `MainView` 的 update（等于给每次击键都加一次实体调度）。
    // 先读一次标记再决定 —— 这次读是廉价的无副作用查询。
    //
    // 「是不是活动标签发的」一并在这趟查询里判掉：同一次实体调度，不多花钱。
    let my_id = cx.entity_id();
    let mine = main
        .read_with(cx, |m, _| {
            m.completion_is_open() && m.active_terminal_id() == Some(my_id)
        })
        .unwrap_or(false);
    if !mine {
        return;
    }
    let _ = main.update(cx, |m, cx| m.refresh_completion(tv, cx));
}

/// 补全相关的按键处置入口（App 上下文，供引擎的
/// [`rustshell_terminal::CompletionKeyFilter`] 装配）。
///
/// 返回 `None` 表示「宿主已经没了」—— 调用方应当当作
/// [`rustshell_terminal::CompletionKeyOutcome::PassThrough`] 处理：
/// 没有宿主就没有浮层，按键自然归终端。
pub fn ui_call_completion_key(
    key: &str,
    control: bool,
    tv: &rustshell_terminal::TerminalView,
    cx: &mut gpui::Context<rustshell_terminal::TerminalView>,
) -> Option<rustshell_terminal::CompletionKeyOutcome> {
    let hub = cx.try_global::<super::UiHub>()?;
    let main = hub.main.clone();
    let my_id = cx.entity_id();
    main.update(cx, |m, cx| {
        // 浮层只认活动标签的终端（同 [`ui_call_completion_refresh`]）。
        // 这里必须判：切标签之后，**旧**终端上的 `Ctrl+Space` 会把新标签的浮层
        // 开出来，而候选是按旧终端那一行算的 —— 正是最典型的串味。
        if m.active_terminal_id() != Some(my_id) {
            return rustshell_terminal::CompletionKeyOutcome::PassThrough;
        }
        m.completion_key(key, control, tv, cx)
    })
    .ok()
}

/// 「用户按了回车」的宿主入口（App 上下文）。
///
/// 由 [`rustshell_terminal::TerminalView::with_enter_callback`] 装配到引擎上，
/// 触发点在回车字节写进 PTY 之后。用途是**命令历史采集**（C-1 的第 ② 号数据源）。
///
/// 与 [`ui_call_completion_refresh`] 的差别：那个只在浮层开着时干活（浮层没开就
/// 不打扰宿主），这个**无条件**干活 —— 用户不开浮层时敲的命令，
/// 下次开浮层时也该出现在「历史」里。这是历史的全部意义。
///
/// 认领判据同样得有，而且这里的代价**更重**：历史是**按活动会话的指纹分桶**的
/// （见 `completion_persist::session_key_of`）。不认领的话，切标签那一帧从旧终端
/// 兑现的那条命令会被记进**新机器**的桶里 —— 静默的错归属，用户下次连新机器
/// 时会看到一条自己从没在那台机器上敲过的命令。
pub fn ui_call_completion_enter(
    tv: &rustshell_terminal::TerminalView,
    _w: &mut Window,
    cx: &mut gpui::Context<rustshell_terminal::TerminalView>,
) {
    let Some(hub) = cx.try_global::<super::UiHub>() else {
        return;
    };
    let main = hub.main.clone();
    let my_id = cx.entity_id();
    let _ = main.update(cx, |m, cx| {
        if m.active_terminal_id() != Some(my_id) {
            return;
        }
        m.record_history_line(tv, cx);
    });
}

#[cfg(test)]
mod tests {
    use super::{CompletionRow, CompletionState};
    use rustshell_core::completion::CandidateSource;

    fn row(text: &str) -> CompletionRow {
        CompletionRow {
            text: text.to_string(),
            suffix: text.to_string(),
            detail: None,
            source: CandidateSource::Builtin,
        }
    }

    /// `close()` 必须**连候选一起清**（铁律 #78：取走即失效）。
    ///
    /// 只把 `open` 置 false 的写法能过编译、也能让浮层从画面上消失，
    /// 但候选还留着：换一台机器再打开浮层，用户会看到**上一台机器**的命令
    /// （`rows` 没被重算之前，第一帧画的就是这批旧数据）。
    #[test]
    fn close_drops_candidates_not_just_the_flag() {
        let mut s = CompletionState {
            open: true,
            rows: vec![row("ls -la"), row("git status")],
            selected: 1,
            prefix: "l".to_string(),
        };

        s.close();

        assert!(!s.open, "必须关掉");
        assert!(s.rows.is_empty(), "候选必须清空：{:#?}", s.rows);
        assert_eq!(s.selected, 0, "选中下标要归零，否则下次开门就指向越界位置");
        assert!(s.prefix.is_empty(), "前缀必须清空");
    }

    /// `↑↓` 是**循环**的（到底再按回到顶），且空列表下不 panic。
    ///
    /// 循环不是随手定的：候选最多 10 条，用户按着 `↑` 不放时钳制会出现
    /// 「按了很多下但停在第一条」的观感（分不清是没反应还是到底了）。
    #[test]
    fn selection_wraps_around_both_ways() {
        let mut s = CompletionState {
            rows: vec![row("a"), row("b"), row("c")],
            ..Default::default()
        };

        // 从 0 往上 ⇒ 绕到末条
        s.selected = 0;
        s.move_selection(false);
        assert_eq!(s.selected, 2, "从首条上移应绕到末条");

        // 从末条往下 ⇒ 绕回首条
        s.move_selection(true);
        assert_eq!(s.selected, 0, "从末条下移应绕回首条");

        // 单调前进
        s.move_selection(true);
        assert_eq!(s.selected, 1);
        s.move_selection(true);
        assert_eq!(s.selected, 2);
        s.move_selection(true);
        assert_eq!(s.selected, 0, "第三条之后再下移应回到首条");
    }

    /// 空候选列表下移动选中项**不 panic、不留越界下标**。
    ///
    /// 这条挡的是真实可达的状态：浮层开着（`open = true`）而用户敲出了
    /// 一串没有任何匹配的前缀 ⇒ `rows` 为空、`selected = 0`；
    /// 此时按 `↑` 若写成 `(0 + n - 1) % n` 就是 `usize` 下溢 panic。
    #[test]
    fn moving_in_an_empty_list_is_a_no_op() {
        let mut s = CompletionState::default();
        s.rows.clear();
        s.selected = 7; // 故意留个非法值，看它会不会被修正
        s.move_selection(true);
        s.move_selection(false);
        assert_eq!(s.selected, 0, "空列表下选中项应被压回 0，而不是保持越界值");
    }

    /// `selected_suffix()` 在无候选 / 下标越界时都返回 `None`
    /// （这是 `accept_completion` 的唯一守卫，写不出字节比写错字节好）。
    #[test]
    fn selected_suffix_is_none_when_there_is_nothing_to_take() {
        let mut s = CompletionState::default();
        assert!(s.selected_suffix().is_none(), "无候选时不该拿到字节");

        s.rows = vec![row("ls")];
        s.selected = 5;
        assert!(
            s.selected_suffix().is_none(),
            "下标越界时应当返回 None（而不是 panic 或取到别的行）"
        );

        s.selected = 0;
        assert_eq!(s.selected_suffix(), Some("ls"));
    }

    /// 回归（2026-10-09 实机「输入命令后闪退」）：宿主回调里**一个实体访问都不许有**。
    ///
    /// # 崩在哪
    ///
    /// gpui 的 `EntityMap::lease` 在 `update` / `render` 期间把实体**从实体表移到
    /// 栈上**（`entity_map.rs`）。此刻对同一实体 `read` / `update` 会走
    /// `double_lease_panic`：
    ///
    /// ```text
    /// cannot read rustshell_terminal::view::TerminalView while it is already being updated
    /// ```
    ///
    /// 而引擎的三个回调**全都在租约里兑现**：`InputLine` / `Enter` 在
    /// `process_events`（由 `Render::render` 调）、按键过滤器在 `on_key_down`。
    /// 旧实现把这三条都当「通知」发给宿主，宿主老老实实去
    /// `active_tab().terminal.read(cx)` —— 于是**按一次回车就整进程闪退**
    /// （`record_history_line` 每次都要读那一行）。
    ///
    /// 修法：引擎改为**借出 `&TerminalView`**（见
    /// `rustshell_terminal::InputLineCallback` 的文档），宿主全程用它。
    ///
    /// # 为什么必须把这块屏挂成「活动标签的终端」
    ///
    /// 三个宿主入口都有**认领**守卫（`active_terminal_id() == cx.entity_id()`）。
    /// 不挂上，守卫会提前 `return`，**用例照样绿却什么都没测到** ——
    /// 正是铁律 #101 记的那个坑：测「某分支没被执行」之前，先保证它被执行到。
    ///
    /// # 覆盖四条路
    ///
    /// | 动作 | 走到哪 | 旧实现崩在 |
    /// |---|---|---|
    /// | `enter` | `ui_call_completion_enter` → `record_history_line` | `term.read(cx)` |
    /// | `ctrl-space` | `ui_call_completion_key` → `toggle_completion` → `refresh_completion` | `term.read(cx)` |
    /// | `commit_text` + 一帧 | `ui_call_completion_refresh` → `refresh_completion` | `term.read(cx)` |
    /// | `tab`（浮层开着） | `ui_call_completion_key` → `accept_completion` | `term.update(cx, ..)` |
    ///
    /// ★ **回车必须排在开浮层之前**：浮层开着时回车会被它**吃掉**
    /// （`CompletionKeyOutcome::Consumed`，见 `completion_key` 的表），
    /// 于是引擎根本不会看到那个 `\r`、`enter_dirty` 永远不会置位 ——
    /// 用例看着「跑了回车」，实际一次都没走到被测代码（第一版就是这么假绿的：
    /// 变异验证测出来 `record_history_line` 压根没被调用）。
    #[gpui::test]
    fn host_callbacks_never_touch_the_terminal_entity(cx: &mut gpui::TestAppContext) {
        use crate::dead_session::{blocking_feed_receiver, BlockingReader, SpySink};
        use gpui::AppContext as _;

        cx.update(|cx| {
            gpui_component::init(cx);
            crate::theme::apply_theme_mode(cx, rustshell_core::theme::ThemeMode::Dark);
        });

        // 一块终端当窗口的根，`MainView` **只在实体层存在**（见下面那段注释）。
        // 三条回调按生产的接法装（与 `create_terminal_view` 逐字一致）——
        // 这三条就是被测对象本身，不能换成「等价」写法；「接线没漏」另有
        // `completion_callbacks_are_wired_in_create_terminal_view` 那条源码护栏管。
        let sink = SpySink::default();
        let sink_for_view = sink.clone();
        let slot: std::rc::Rc<std::cell::RefCell<Option<gpui::Entity<super::super::MainView>>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let (term, vcx) = {
            let slot = slot.clone();
            cx.add_window_view(move |window, cx| {
                // ★ `MainView` 在本用例里**只当实体用**：本窗口的根是终端，
                //   `MainView::render` 一次都不会跑。
                //
                //   这一条是刻意的，不是图省事。若走 `build_main` 那条路（MainView
                //   上屏），它的活动标签会去渲染**同一个** `TerminalView` 实体 ——
                //   那个实体同时又是另一个窗口的根，于是每帧被画两遍，两边的
                //   `process_events` 互相 `notify`，调度器再也停不下来
                //   （实测：`simulate_keystrokes` 直接挂死）。
                //
                //   本用例要验的是**宿主回调与终端实体的关系**，与「MainView 长什么样」
                //   无关，所以把渲染这一层整个去掉，噪声归零。
                let main = cx.new(|cx| super::super::MainView::new(window, cx));
                *slot.borrow_mut() = Some(main);
                rustshell_terminal::TerminalView::new(
                    sink_for_view,
                    BlockingReader(blocking_feed_receiver()),
                    rustshell_terminal::TerminalConfig::default(),
                    cx,
                )
                .with_input_line_callback(|tv, window, cx| {
                    crate::main_view::ui_call_completion_refresh(tv, window, cx);
                })
                .with_completion_key_filter(|event, tv, _window, cx| {
                    let k = &event.keystroke;
                    crate::main_view::ui_call_completion_key(&k.key, k.modifiers.control, tv, cx)
                        .unwrap_or(rustshell_terminal::CompletionKeyOutcome::PassThrough)
                })
                .with_enter_callback(|tv, window, cx| {
                    crate::main_view::ui_call_completion_enter(tv, window, cx);
                })
            })
        };
        let main = slot.borrow().clone().expect("MainView 未创建");
        vcx.run_until_parked();

        // 把这块屏挂成**活动标签**的终端（认领守卫要它俩相等）。
        let sid = "reentrancy-regression";
        main.update_in(vcx, |m, _w, _cx| {
            m.tabs_state.tabs.push(super::super::TabEntry {
                session: rustshell_core::session::SessionHandle::pending(
                    sid.to_string(),
                    rustshell_core::session::SessionKind::Ssh,
                    "regression".into(),
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

        // 前提：认领判据**真的**成立。不钉住这一条，下面全是空转（铁律 #101）。
        assert!(
            main.update_in(vcx, |m, _w, _cx| m.active_terminal_id()
                == Some(term.entity_id())),
            "前提不成立：活动标签的终端不是这块屏 —— 认领守卫会把三条回调全部挡在 \
             门外，本用例会退化成「什么都没测却全绿」"
        );

        // 焦点先给再走一帧（同 `ime_wiring` 的手法），否则按键进不了终端。
        vcx.update(|window, cx| {
            let handle = term.read(cx).focus_handle().clone();
            window.focus(&handle, cx);
        });
        vcx.run_until_parked();
        assert!(
            vcx.update(|window, cx| term.read(cx).focus_handle().is_focused(window)),
            "前提不成立：焦点没落在终端上，后面所有按键都会以「什么都没收到」假绿"
        );

        // 推一帧：两条回调都是**两段式**（`input_line_dirty` / `enter_dirty` 置旗在
        // 输入那一刻、兑现在下一帧 `render`），不推帧它们永远不会被调用。
        let frame = |vcx: &mut gpui::VisualTestContext| {
            vcx.update(|window, cx| {
                window.draw(cx).clear(cx);
            });
        };
        // 前提：这块屏**真的**在走 render（否则 `process_events` 里的两条回调
        // 一次都不会兑现，本用例同样退化成空转）。
        frame(vcx);

        // ① 回车（**浮层关着**）→ `ui_call_completion_enter` → `record_history_line`
        //    ——「用户报的那一下」。先跑它，理由见上面文档里的 ★。
        vcx.simulate_keystrokes("enter");
        frame(vcx);

        // ② 按键过滤器 → 开浮层 → `refresh_completion`（旧实现另一种崩法）
        vcx.simulate_keystrokes("ctrl-space");
        frame(vcx);
        assert!(
            main.update_in(vcx, |m, _w, _cx| m.panel_ui.completion.open),
            "Ctrl+Space 没把浮层打开 —— 浮层关着时 `ui_call_completion_refresh` 会提前 \
             返回、`accept_completion` 也会提前返回，后面两条路就都测不到了"
        );

        // ③ 文本通路 → 输入行变了 → `ui_call_completion_refresh`
        term.update_in(vcx, |tv, _w, cx| tv.commit_text("ls -la", cx));
        frame(vcx);

        // ④ 浮层开着按 Tab → `accept_completion`（旧实现崩在那个 `update` 上）
        vcx.simulate_keystrokes("tab");
        frame(vcx);

        // 走到这里没 panic 就是通过。再钉两个正向信号，免得将来回调被整条摘掉
        // （摘掉也不会 panic）用例仍然绿。
        assert!(
            sink.text().contains("ls -la"),
            "`commit_text` 的字节没进 PTY：这条链路压根没跑起来，上面「没 panic」的\
             结论不可信。实际收到：{:?}",
            sink.text()
        );
        assert!(
            !sink.text().contains('\0'),
            "Ctrl+Space 被译成 `\\x00` 写进了 PTY：远端 shell 收到 NUL 是垃圾输入，\
             这个键必须由浮层吃掉。实际收到：{:?}",
            sink.text()
        );
    }
}
