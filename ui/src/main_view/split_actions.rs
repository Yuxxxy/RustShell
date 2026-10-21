//! 分屏域：标签拆分/取消拆分、窗格焦点与槽位修正。
//! 本模块为 `main_view` 的子模块，可直接访问 `MainView` 的私有字段。

use gpui::{
    div, prelude::FluentBuilder as _, px, AnyElement, ClickEvent, Context, InteractiveElement as _,
    IntoElement, MouseButton, ParentElement as _, SharedString, StatefulInteractiveElement as _,
    Styled as _, Window,
};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    h_flex,
    menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem},
    v_flex, IconName, Sizable as _,
};

use rustshell_core::session::SessionKind;
use rustshell_core::split::{SplitAxis, SplitFix, SplitPlan};

use super::{MainView, PaneSide, SplitPane};

use crate::theme::to_hsla as hex;

/// 断连条要渲染的一揽子数据。
///
/// 单独成结构体是因为 `render_term_pane` 里只能**先在不可变借用中取好**渲染数据
/// （后面建元素要 `&mut self`），塞进元组会让那个 `match` 变成一坨读不懂的东西。
struct DeadBarPlan {
    /// 断连原因（远端注销 / 网络中断 / 空闲超时…）
    reason: String,
    /// 正在原地重连
    reconnecting: bool,
    /// 自动重连的进展（F-4）：`Some` = 这条正走一套重试计划
    retry: Option<super::DeadBarRetry>,
    /// 重连失败原因（`Some` 时断连条追加一段尾巴）
    retry_error: Option<String>,
    /// 该会话有可重放的连接配置（临时会话为 `false`，不给「重新连接」按钮）
    can_reconnect: bool,
}

impl MainView {
    // 分屏动作

    /// 拆分当前焦点标签到另一侧（向右 / 向下）。
    /// 决策交给 `split::plan_split` 纯函数，这里只负责把决策落到会话上。
    pub fn split_pane(&mut self, axis: SplitAxis, _w: &mut Window, cx: &mut Context<Self>) {
        let pool: Vec<String> = self
            .tabs_state
            .tabs
            .iter()
            .map(|t| t.session.id.clone())
            .collect();
        let focused = self.tabs_state.active.clone();
        // 已在分屏中：本次拆分只切方向（池中标签一个都不动）
        let in_split = self.tabs_state.split.is_some();

        match rustshell_core::split::plan_split(focused.as_deref(), &pool, in_split, axis) {
            SplitPlan::Noop => {
                self.set_info(
                    rustshell_core::i18n::t("没有可拆分的会话").to_string(),
                    false,
                );
            }
            SplitPlan::SwitchAxis(axis) => {
                if let Some(sp) = self.tabs_state.split.as_mut() {
                    sp.axis = axis;
                }
            }
            SplitPlan::Show { secondary } => {
                let Some(primary) = focused else { return };
                // 池里已有另一个标签：直接把它放进副窗格，不新建会话、不改动任何标签
                self.tabs_state.split = Some(SplitPane {
                    axis,
                    primary,
                    secondary: secondary.clone(),
                });
                self.tabs_state.active = Some(secondary);
                self.tabs_state.pending_focus = true;
            }
            SplitPlan::NeedSession => self.split_with_new_session(axis, cx),
        }
        cx.notify();
    }

    /// 池中只有焦点标签时，新开一个同类型会话放进副窗格。
    /// 本地 Shell 的创建是异步的，此时只落标记，由 `maybe_establish_split` 在标签插入后收口。
    fn split_with_new_session(&mut self, axis: SplitAxis, cx: &mut Context<Self>) {
        let Some(primary) = self.tabs_state.active.clone() else {
            self.set_info(
                rustshell_core::i18n::t("没有可拆分的会话").to_string(),
                false,
            );
            return;
        };
        let Some(tab) = self
            .tabs_state
            .tabs
            .iter()
            .find(|t| t.session.id == primary)
        else {
            self.set_info(
                rustshell_core::i18n::t("没有可拆分的会话").to_string(),
                false,
            );
            return;
        };
        match (tab.session.kind, tab.session.profile_id.clone()) {
            (SessionKind::Ssh, Some(pid)) => {
                let Some(p) = self.conn_store.profile(&pid).cloned() else {
                    self.set_info(
                        rustshell_core::i18n::t("该会话的连接配置已被删除，无法重连").to_string(),
                        false,
                    );
                    return;
                };
                self.tabs_state.pending_split = Some((axis, primary));
                // 占位 Tab 同步入列，紧接着消费标记：分屏立即成立，不等下一轮轮询
                self.connect_profile_now(p, cx);
                self.consume_pending_split(cx);
            }
            (SessionKind::Local, _) => {
                if self.conn_ui.local_starting {
                    // 已有一个本地 Shell 在启动中：本次不会再产生新标签，
                    // 绝不落下标记（否则会在下次建标签时「突然分屏」）。
                    self.set_info(
                        rustshell_core::i18n::t("本地 Shell 正在启动，请稍后再拆分").to_string(),
                        false,
                    );
                    return;
                }
                self.tabs_state.pending_split = Some((axis, primary));
                self.open_local_shell(cx);
            }
            (SessionKind::Ssh, None) => {
                // 临时 SSH 会话没有可复制的目标，明确提示而不是静默无响应
                self.set_info(
                    rustshell_core::i18n::t("该会话未保存为连接配置，无法拆分").to_string(),
                    false,
                );
            }
        }
    }

    /// 新建标签插入后消费待建立的拆分：新标签进副窗格，原标签留在主窗格。
    /// 幂等：标记不存在或条件不成立时什么都不做。
    pub(super) fn maybe_establish_split(&mut self, new_id: &str, cx: &mut Context<Self>) {
        let Some((axis, primary)) = self.tabs_state.pending_split.clone() else {
            return;
        };
        let pool: Vec<String> = self
            .tabs_state
            .tabs
            .iter()
            .map(|t| t.session.id.clone())
            .collect();
        if new_id == primary || !pool.iter().any(|id| id == new_id) || !pool.contains(&primary) {
            // 新标签没建立（创建失败，或被复用成了原有的连接中占位 Tab）：
            // 撤销标记并说明原因，绝不留一个「以后某次新建标签时突然分屏」的暗雷。
            self.tabs_state.pending_split = None;
            self.set_info(
                rustshell_core::i18n::t("拆分已取消（没有可用的新会话）").to_string(),
                false,
            );
            cx.notify();
            return;
        }
        self.tabs_state.pending_split = None;
        self.tabs_state.split = Some(SplitPane {
            axis,
            primary,
            secondary: new_id.to_string(),
        });
        self.tabs_state.active = Some(new_id.to_string());
        self.tabs_state.pending_focus = true;
        cx.notify();
    }

    /// 同步路径（新标签已成为 active）下的收口。
    fn consume_pending_split(&mut self, cx: &mut Context<Self>) {
        if self.tabs_state.pending_split.is_none() {
            return;
        }
        let Some(new_id) = self.tabs_state.active.clone() else {
            return;
        };
        self.maybe_establish_split(&new_id, cx);
    }

    /// 取消分屏：只清掉分屏态，两个会话都留在标签栏，不关闭任何会话。
    pub fn unsplit(&mut self, cx: &mut Context<Self>) {
        if self.tabs_state.split.take().is_some() {
            self.tabs_state.pending_focus = true;
        }
        cx.notify();
    }

    /// Tab 栏分屏按钮：未分屏则按给定方向拆分，已分屏则取消。
    /// `pub(crate)`：标签栏（`crate::tab_bar`）是`main_view`的兄弟模块，需要调用。
    ///
    /// 2026-09-17：标签栏上的两个分屏按钮已按运行反馈移除（拆分入口仍在标签右键菜单
    /// 与 `Ctrl+Alt+R` / `Ctrl+Alt+D`），因此本「一键切换」只剩这一个入口。保留它
    /// 是因为「同一个键再按一次 = 收起分屏」的语义仍成立，随时可以接回某个按钮。
    #[allow(dead_code)]
    pub(crate) fn toggle_split(&mut self, axis: SplitAxis, w: &mut Window, cx: &mut Context<Self>) {
        if self.tabs_state.split.is_some() {
            self.unsplit(cx);
        } else {
            self.split_pane(axis, w, cx);
        }
    }

    /// 点击窗格：把焦点移到该窗格（不阻断冒泡，终端划选与右键菜单不受影响）。
    pub(super) fn focus_pane(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.tabs_state.split.is_none() || self.tabs_state.active.as_deref() == Some(id) {
            return;
        }
        self.tabs_state.active = Some(id.to_string());
        cx.notify();
    }

    /// 修正分屏态不变量：槽位必须存在且互不相同，修正不成立就取消分屏。
    /// 增删标签后必须调用，否则会留下「窗格指向已关闭标签」的半残状态。
    pub(super) fn normalize_split(&mut self) {
        let pool: Vec<String> = self
            .tabs_state
            .tabs
            .iter()
            .map(|t| t.session.id.clone())
            .collect();
        let active_valid = self
            .tabs_state
            .active
            .as_ref()
            .map(|a| pool.contains(a))
            .unwrap_or(false);

        let Some(sp) = self.tabs_state.split.clone() else {
            if !active_valid {
                self.tabs_state.active = None;
            }
            if self.tabs_state.active.is_none() {
                self.tabs_state.active = self.tabs_state.tabs.last().map(|t| t.session.id.clone());
            }
            return;
        };

        match rustshell_core::split::fix_split(&sp.primary, &sp.secondary, &pool) {
            SplitFix::Keep => {}
            SplitFix::ReplaceSecondary { with } => {
                if let Some(sp) = self.tabs_state.split.as_mut() {
                    sp.secondary = with;
                }
            }
            SplitFix::PromoteSecondary { primary, secondary } => {
                if let Some(sp) = self.tabs_state.split.as_mut() {
                    sp.primary = primary;
                    sp.secondary = secondary;
                }
            }
            SplitFix::Unsplit => {
                self.tabs_state.split = None;
            }
        }

        if !active_valid {
            // 焦点标签被关掉了：交给仍存活的主窗格
            self.tabs_state.active = self.tabs_state.split.as_ref().map(|sp| sp.primary.clone());
        }
        if self.tabs_state.active.is_none() {
            self.tabs_state.active = self.tabs_state.tabs.last().map(|t| t.session.id.clone());
        }
    }

    /// 焦点窗格标识（未分屏时恒为主窗格）。
    pub(super) fn focused_side(&self) -> PaneSide {
        match &self.tabs_state.split {
            Some(sp) if self.tabs_state.active.as_deref() == Some(sp.secondary.as_str()) => {
                PaneSide::Secondary
            }
            _ => PaneSide::Primary,
        }
    }

    /// 把某个标签放进焦点窗格（标签栏点击的语义）。
    /// - 该标签已在某窗格 → 只把焦点移过去；
    /// - 不在窗格 → 替换焦点窗格的槽位，被换下来的标签仍留在标签栏（不丢）。
    pub(crate) fn place_in_focused_pane(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(sp) = self.tabs_state.split.clone() {
            if id != sp.primary && id != sp.secondary {
                match self.focused_side() {
                    PaneSide::Primary => {
                        if let Some(sp) = self.tabs_state.split.as_mut() {
                            sp.primary = id.to_string();
                        }
                    }
                    PaneSide::Secondary => {
                        if let Some(sp) = self.tabs_state.split.as_mut() {
                            sp.secondary = id.to_string();
                        }
                    }
                }
            }
        }
        self.tabs_state.active = Some(id.to_string());
        self.tabs_state.pending_focus = true;
        cx.notify();
    }

    /// 当前是否处于分屏态（供标签栏等外部视图决定按钮/菜单项可用性）。
    pub(crate) fn is_split(&self) -> bool {
        self.tabs_state.split.is_some()
    }

    /// 设置面板的统一提示入口：`ok = true` 走信息色，`false` 走错误色。
    pub(super) fn set_info(&mut self, text: String, ok: bool) {
        self.io.set_hint = Some(text);
        self.io.set_hint_ok = ok;
    }

    /// 失败提示的专用入口。
    ///
    /// 必须经这里、**不要直接写 `set_hint`**：`set_hint_ok` 是渲染时选`Alert`
    /// 变体的依据，而 `set_info` 可能刚把它置成 `true`（成功提示）。绕过本方法
    /// 直接赋值会让随后的校验失败继承上一次的「成功」，把错误画成绿色。
    pub(super) fn set_error(&mut self, text: String) {
        self.set_info(text, false);
    }

    // 窗格渲染

    /// 本帧是否存在「可聚焦的终端视图」。
    ///
    /// 终端视图（`terminal/src/view.rs`）是全应用**唯一**
    /// 对元素调 `track_focus` 的地方。它一旦不在帧里，窗口焦点句柄
    /// `self.panel_ui.focus` 就没有对应的派发节点：gpui 的
    /// `focus_node_id_in_rendered_frame`（`window.rs:6040`）查不到就**退化成
    /// 派发树根节点**，于是 `dispatch_path` 只剩根节点自身 —— 挂在根 div 上的
    /// `on_action`（命令面板、新建标签、切换侧栏……）一个都收不到。
    /// 现象即「主窗格为空、或标签还在连接中没有终端时，快捷键整体失灵」。
    ///
    /// 口径必须与 [`Self::render_workspace`] 的派发逻辑一致：分屏看两个窗格，
    /// 未分屏只看 `active`。
    pub(super) fn has_focusable_terminal(&self) -> bool {
        let has_term = |id: &str| {
            self.tabs_state
                .tabs
                .iter()
                .any(|t| t.session.id == id && t.terminal.is_some())
        };
        match self.tabs_state.split.as_ref() {
            Some(sp) => has_term(&sp.primary) || has_term(&sp.secondary),
            None => self.tabs_state.active.as_deref().is_some_and(has_term),
        }
    }

    /// 焦点锚点：一枚零尺寸、纯聚焦的透明节点，把 `self.panel_ui.focus` 注册进派发树，
    /// 让键盘动作的派发路径重新经过根 div。
    ///
    /// **只在没有可聚焦终端时挂**（见 [`Self::has_focusable_terminal`]）：
    /// 终端在场时它自己 `track_focus` 到自己的句柄，同一帧两个焦点认领者各自
    /// 触发 `a11y.set_focus` 就是 gpui 的
    /// `panic!("set_focus called more than once in a single frame")`（崩溃 N7）。
    /// 用「互斥」把这件事变成结构性不可能，而不是靠调用顺序碰运气。
    ///
    /// 零尺寸（`w(0).h(0)`）且无任何命中处理：不参与画面、不吃鼠标。
    /// `tab_stop(false)`：它是承载根级键盘动作的**容器**，不该成为 Tab 遍历
    /// 上的一站（否则空窗格按下 Tab 会先停在一个 0×0 的无标签节点上）。
    pub(super) fn render_focus_anchor(&self) -> AnyElement {
        div()
            .id("focus-anchor")
            .absolute()
            .top_0()
            .left_0()
            .w(px(0.))
            .h(px(0.))
            .tab_stop(false)
            .track_focus(&self.panel_ui.focus)
            .into_any_element()
    }

    /// 内容区：未分屏时是单个窗格；分屏时是两个可拖拽调整的窗格。
    pub(super) fn render_workspace(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(sp) = self.tabs_state.split.clone() else {
            return match self.tabs_state.active.clone() {
                Some(id) => self.render_term_pane(&id, cx),
                None => self.render_no_session(cx),
            };
        };

        let focused_side = self.focused_side();
        // 焦点窗格标记：`render_pane_header` 里再加两道强信号（顶部 accent 横条 +
        // 标题条底色提亮）。这里只做布局，不加边框 —— 边框会吃掉 1px 内容宽度，
        // 触发终端列数重算（PTY resize），切换焦点时画面会抖。
        let primary = {
            let header = self.render_pane_header(PaneSide::Primary, &sp.primary, focused_side, cx);
            let body = self.render_term_pane(&sp.primary, cx);
            v_flex()
                .flex_1()
                .min_w_0()
                .min_h_0()
                .child(header)
                .child(body)
                .into_any_element()
        };
        let secondary = {
            let header =
                self.render_pane_header(PaneSide::Secondary, &sp.secondary, focused_side, cx);
            let body = self.render_term_pane(&sp.secondary, cx);
            v_flex()
                .flex_1()
                .min_w_0()
                .min_h_0()
                .child(header)
                .child(body)
                .into_any_element()
        };

        // 两个面板都不给初始尺寸，各自 flex_grow_1，初始各占一半；
        // 拖拽由官方`ResizableState`接管并记住比例。两侧共用同一 id，
        // 切换方向时尺寸状态随之复用，不会被重置回 50/50。
        let group = match sp.axis {
            SplitAxis::Vertical => gpui_component::resizable::h_resizable("term-split"),
            SplitAxis::Horizontal => gpui_component::resizable::v_resizable("term-split"),
        };
        group
            .child(gpui_component::resizable::resizable_panel().child(primary))
            .child(gpui_component::resizable::resizable_panel().child(secondary))
            .into_any_element()
    }

    /// 单个窗格的内容：终端视图 / 连接占位卡 / 搜索浮层 / 滚动条 / 右键菜单。
    pub(super) fn render_term_pane(
        &mut self,
        session_id: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let tok = crate::theme::current(cx);
        let pane_focused = self.tabs_state.active.as_deref() == Some(session_id);

        // 先在不可变借用里取好渲染所需数据；后面构建浮层要`&mut self`
        let (term, placeholder, scroll, dead) = match self
            .tabs_state
            .tabs
            .iter()
            .find(|t| t.session.id == session_id)
        {
            Some(tab) => (
                tab.terminal.clone(),
                if tab.terminal.is_none() {
                    Some((
                        // 展示标题（不是 `session.title`）：配置已删除的占位要
                        // 说清「原来是哪一台、而且那台已经没了」（见 `display_tab_title`）
                        super::display_tab_title(tab, &self.conn_store),
                        tab.connect_error.clone(),
                        tab.connect_started,
                        tab.session.kind,
                        tab.is_disconnected(),
                    ))
                } else {
                    None
                },
                tab.scroll.clone(),
                // 断连条数据（**与占位卡互斥**：断连态一定有终端画面）
                tab.dead.as_ref().map(|reason| DeadBarPlan {
                    reason: reason.clone(),
                    reconnecting: tab.is_reconnecting(),
                    retry: tab.retry_progress(),
                    retry_error: tab.connect_error.clone(),
                    can_reconnect: tab.session.profile_id.is_some(),
                }),
            ),
            None => (None, None, None, None),
        };

        // T-2 拖拽跟手（2026-09-17 第五轮运行反馈「鼠标移动滑动轴的时候不跟手」）：
        // 把 `set_offset` 暂存的目标偏移**在这一帧**就落到终端上。
        //
        // 为什么必须放这里：`ScrollbarHandle::set_offset` 只拿得到 `&self`，写不进终端
        // （`Entity::update` 要 `&mut App`），帧内能拿到 `&mut App` 的钩子只有渲染。
        // 只留在 `MainView::poll`（300ms）里落地的话，鼠标每移动一次都要等最多 300ms
        // 才动 —— 症状正是「滑块停在原地，过一会儿跳一下」，也就是用户说的「不跟手」。
        //
        // 顺序不能反：**先落目标、再刷几何**。`sync` 读的是终端快照，反过来的话
        // `offset()` 拿到的还是旧偏移，滑块与内容仍然不同帧。
        if let (Some(handle), Some(term)) = (scroll.as_ref(), term.as_ref()) {
            handle.apply_pending_with(cx, term);
            // 每帧刷新几何缓存（读一次终端快照，与 visible_text 同量级）。
            // `sync` 与滚动条读的是同一份 `Rc<Cell<..>>`，所以缓存刷新必须发生在
            // 构建元素之前 —— 官方 `Scrollbar` 到 prepaint 阶段才去问 `content_size()`。
            handle.sync(cx);
        }

        // 搜索浮层只挂在焦点窗格：搜索目标本就是`active_tab()`对应的终端
        let search_bar = if pane_focused && self.panel_ui.term_search_open {
            // 浮层走独立视图（`TermSearchBarView`）：搜索框每敲一个字只重排它自己，
            // 不再把整个窗口重排一遍
            Some(self.panel_ui.term_search_bar.clone().into_any_element())
        } else {
            None
        };

        // 补全浮层同理只挂焦点窗格（候选属于**这台**终端的当前行）。
        //
        // 与搜索浮层在视觉上互不遮挡：搜索条贴右上，补全条贴左下。两者可以
        // 同时开着（Ctrl+F 找东西、Ctrl+Space 补命令），不做互斥 ——
        // 强行关掉一个会让用户「按下另一个键，前一个悄悄没了」。
        let completion_bar = if pane_focused && self.completion_is_open() {
            Some(self.panel_ui.completion_bar.clone().into_any_element())
        } else {
            None
        };

        let click_id = session_id.to_string();
        let right_id = click_id.clone();
        // 占位卡底色：主题纯色（alpha=1.0）
        let placeholder_bg = crate::theme::surface(1.0, tok.bg_base);
        let mut pane = div()
            .debug_selector(|| "term-pane".to_string())
            .relative()
            .flex_1()
            .min_h_0()
            // 点击/右击窗格即把焦点移过来（不 stop_propagation，终端的划选与右键菜单不受影响）
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |m, _e, _w, cx| m.focus_pane(&click_id, cx)),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |m, _e, _w, cx| m.focus_pane(&right_id, cx)),
            )
            .when_some(term, |d, term| d.child(term))
            .when_some(
                placeholder,
                |d, (title, err, started, kind, disconnected)| {
                    d.child(super::render_connect_placeholder(
                        &tok,
                        placeholder_bg,
                        &title,
                        err.as_deref(),
                        started,
                        kind,
                        disconnected,
                    ))
                },
            )
            .when_some(search_bar, |d, bar| d.child(bar))
            .when_some(completion_bar, |d, bar| d.child(bar));

        if let Some(handle) = scroll {
            // 几何缓存已在上面刷新（顺序：先落拖拽目标、再刷缓存）。
            // 挂载条件只有「窗格有终端」。**不再自带「内容是否超过一屏」的闸门**：
            // 官方 `Scrollbar` 自己会判（内容不超一屏时它连轨道都不进 states），
            // 两处各判一次就是两套口径，闸门判错时整条滚动条根本挂不上去，
            // 渲染器内部的判定连表态的机会都没有。2026-09-17 运行反馈
            // 「终端内容很长没有滑动轴」正是这道闸门在起作用。
            // 三个不变量（自带视口 / 铺满窗格 / 不吞鼠标）写在 `overlay` 的文档里，
            // 滑块取色同理走 `overlay` 内的唯一入口。
            pane = pane.child(crate::terminal_scrollbar::overlay(&handle, cx));
        }

        // 断连条画在**最后**（连滚动条之上）：断连态下它是这块窗格里唯一还能
        // 点的东西，层级上也该如此 —— 底部的「重新连接 / 关闭标签」不许被
        // 滚动条压在下面。
        if let Some(plan) = dead {
            pane = pane.child(Self::render_dead_bar(session_id, plan, cx));
        }

        pane.context_menu(|menu: PopupMenu, _w, _cx| pane_context_menu(menu))
            .into_any_element()
    }

    /// 断连条：会话掉线后在终端窗格**底部**就地叠的一条状态条。
    ///
    /// 为什么是「一条」而不是整屏遮罩：掉线现场是要给人读的 —— 最后几行输出、
    /// 远端注销时打的那句话、光标停在哪，都是判断「为什么断」的证据。整屏遮罩
    /// 会把证据盖掉；这里只吃掉底部两行高度，上面的输出一个字都不动。
    ///
    /// 三个必须同时成立的口径（缺一个这条就变成装饰品）：
    /// 1. **在终端窗格内**，不是新页面、不是弹窗 —— 用户的视线不用离开终端；
    /// 2. 显式说出「连接已断开」，并带上原因与（若在重连）重连进展；
    /// 3. `.occlude()`（`HitboxBehavior::BlockMouse`）**必须加**：gpui 的
    ///    `hit_test` 会把光标下的全部 hitbox 收进列表（含下方终端的），不加这一句，
    ///    点「重新连接」会**同时**把一次鼠标事件送给终端（触发划选/抢焦点）。
    ///    `.occlude()` 是唯一能让 hit_test 提前 break 的行为（同无边框标题栏挖洞）。
    ///
    /// 形态遵循 `ui/src/notice.rs` 的口径：官方 `Alert` **没有子节点槽**，
    /// 所以「提示 + 动作」只能是**两行兄弟节点**，不给 `Alert` 塞 `child`；
    /// 警告图标也归 `Alert` 所有（`no_panel_hand_rolls_a_warning_icon` 钉着），
    /// 这里不出手自绘三角。
    fn render_dead_bar(session_id: &str, plan: DeadBarPlan, cx: &mut Context<Self>) -> AnyElement {
        let tok = crate::theme::current(cx);

        // 一句整话交给 `Alert`，两段之间用间隔号连起来：
        // 「连接已断开 · 连接已被远端关闭 · 正在重连…（第 2/3 次）」
        //
        // 重连进展那段有两种句式（F-4）：正在连 vs 在等退避。前者是「已经发出去了」，
        // 后者是「还要等一会儿」—— 用户对着一个不再刷新的画面时，这两句话正是他
        // 判断「我该等还是该自己点一下」的全部依据，所以次数必须写在脸上。
        let mut msg = rustshell_core::i18n::t("连接已断开").to_string();
        msg.push_str(" · ");
        msg.push_str(&plan.reason);
        match plan.retry.as_ref() {
            Some(rt) if rt.waiting => {
                msg.push_str(" · ");
                msg.push_str(&rustshell_core::i18n::ts(
                    format!("等待重试（第 {}/{} 次）", rt.attempt, rt.max),
                    format!("Waiting to retry ({}/{})", rt.attempt, rt.max),
                ));
            }
            Some(rt) => {
                msg.push_str(" · ");
                msg.push_str(&rustshell_core::i18n::ts(
                    format!("正在重连…（第 {}/{} 次）", rt.attempt, rt.max),
                    format!("Reconnecting… ({}/{})", rt.attempt, rt.max),
                ));
            }
            // 没有账本还在连 = 用户手动点的「重新连接」，没有次数可言
            None if plan.reconnecting => {
                msg.push_str(" · ");
                msg.push_str(rustshell_core::i18n::t("正在重连…"));
            }
            None => {}
        }
        if let Some(err) = plan.retry_error.as_deref() {
            msg.push_str(" · ");
            msg.push_str(rustshell_core::i18n::t("重连失败："));
            msg.push_str(err);
        }

        let rid = session_id.to_string();
        let mut actions = h_flex().flex_shrink_0().gap_2();
        if plan.can_reconnect {
            actions = actions.child(
                Button::new("dead-reconnect")
                    .primary()
                    .xsmall()
                    .icon(IconName::RotateCw)
                    .label(rustshell_core::i18n::t("重新连接"))
                    .on_click(cx.listener(move |m, _e, _w, cx| m.reconnect_tab(&rid, cx))),
            );
        }
        let cid = session_id.to_string();
        actions = actions.child(
            Button::new("dead-close")
                .xsmall()
                .icon(IconName::Close)
                .label(rustshell_core::i18n::t("关闭标签"))
                .on_click(cx.listener(move |m, _e, _w, cx| m.close_tab_by_session(&cid, cx))),
        );

        v_flex()
            .id("term-dead-bar")
            .debug_selector(|| "term-dead-bar".to_string())
            .occlude()
            .flex_shrink_0()
            .w_full()
            .px_2()
            .py(px(4.))
            .gap_1()
            .border_t_1()
            .border_color(hex(tok.border_subtle))
            .child(
                crate::notice::NoticeKind::Error
                    .compact("term-dead-notice", SharedString::from(msg)),
            )
            .child(h_flex().w_full().justify_end().child(actions))
            .into_any_element()
    }

    /// 分屏时每个窗格顶部的标题条：状态点 + 标题 +（副窗格）取消分屏。
    fn render_pane_header(
        &mut self,
        side: PaneSide,
        id: &str,
        focused_side: PaneSide,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let tok = crate::theme::current(cx);
        let (title, status_color) = match self.tabs_state.tabs.iter().find(|t| t.session.id == id) {
            Some(tab) => {
                let title = if tab.session.title.trim().is_empty() {
                    rustshell_core::i18n::t("终端").to_string()
                } else {
                    tab.session.title.clone()
                };
                // 断连态必须是**红点**而不是绿点：终端视图在断连时刻意留着，
                // 用 `terminal.is_some()` 判会把一个已死的会话画成健康状态。
                // 判定与断连条、标签栏状态点共用 `TabEntry` 的三个谓词。
                let color = if tab.is_live() {
                    hex(tok.success)
                } else if tab.is_dead() {
                    hex(tok.danger)
                } else if tab.is_connecting() {
                    hex(tok.warning)
                } else {
                    hex(tok.fg_muted)
                };
                (title, color)
            }
            None => (String::new(), hex(tok.fg_muted)),
        };
        // 焦点窗格：底部一根 accent 横条 + 标题条底色提亮。
        // 2026-09-17 运行反馈：先去掉「主窗格 / 副窗格」字样（老气），
        // 焦点态就只剩「底色提亮 + 底部 2px 横条」这两处硬标记；
        // 横条只占 2px 高度，不引起终端列数变化。
        let focused = side == focused_side;
        // 底色：主题纯色（alpha=1.0）
        let focused_bg = crate::theme::surface(1.0, tok.bg_elevated);
        let unfocused_bg = crate::theme::surface(1.0, tok.bg_overlay);

        let mut bar = h_flex()
            .relative()
            .h(px(24.))
            .px_2()
            .gap_2()
            .flex_shrink_0()
            .items_center()
            .when(focused, |d| d.bg(focused_bg))
            .when(!focused, |d| d.bg(unfocused_bg))
            .border_b_1()
            .border_color(hex(tok.border_subtle))
            .child(
                div()
                    .w(px(6.))
                    .h(px(6.))
                    .flex_shrink_0()
                    .rounded_full()
                    .bg(status_color),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(11.5))
                    .text_color(if focused {
                        hex(tok.fg_primary)
                    } else {
                        hex(tok.fg_muted)
                    })
                    .text_ellipsis()
                    .child(SharedString::from(title)),
            );

        if side == PaneSide::Secondary {
            bar = bar.child(
                Button::new("unsplit-pane")
                    .ghost()
                    .xsmall()
                    .icon(IconName::PanelRightClose)
                    .tooltip(rustshell_core::i18n::t("取消分屏"))
                    .on_click(cx.listener(|m, _e, _w, cx| m.unsplit(cx))),
            );
        }
        if focused {
            bar = bar.child(
                // 进度的「底部 accent 横条」占满整条标题，与标签栏的选中标记同一语言。
                // 排在最后 到 画在内容之上；不参与布局（absolute），不会挤动标题与按钮。
                div()
                    .debug_selector(|| "pane-focus-bar".to_string())
                    .absolute()
                    .left_0()
                    .right_0()
                    .bottom_0()
                    .h(px(2.))
                    .bg(hex(tok.accent)),
            );
        }
        bar.into_any_element()
    }

    /// 无任何会话时的空态。
    ///
    /// 2026-09-18 反馈「背景图只替换了连接栏」：这里原是不透明 `bg_base`。
    /// 背景图功能已整体移除（2026-10-21），面板取主题纯色（alpha=1.0）。
    ///
    /// 2026-09-20 改版：原先的「图标 + 一句说明 + 一个按钮」换成
    /// 「动作名 …… 键帽」的快捷键表（对齐 VS Code 空窗格的做法）——
    /// 说明文字读一遍就忘，键位表能被**照着按**；而且每一行自己就是按钮
    /// （点即执行），等于把原来那一个按钮铺开成了「此刻所有能做的事」。
    /// 行数据见 [`crate::keybinds::empty_state_hints`]：只列此刻真的有效的动作，
    /// 键位取**生效**值（用户改过绑定就显示改后的）。
    fn render_no_session(&self, cx: &mut Context<Self>) -> AnyElement {
        let tok = crate::theme::current(cx);
        let hints = crate::keybinds::empty_state_hints(&self.panel_ui.keymap_rows);

        let mut list = v_flex()
            .debug_selector(|| "empty-hints".to_string())
            .w(px(340.))
            .gap(px(2.));
        for (id, (label, keys)) in crate::keybinds::EMPTY_STATE_ACTIONS.iter().zip(hints) {
            list = list.child(no_session_hint_row(id, label, keys, cx));
        }

        // 2026-09-22 用户要求「移除主窗格为空」：顶部那行「图标 + 主窗格为空」撤下。
        // 它说的是**用户已经知道的事**（窗格是空的 —— 一片空白本身就是证据），
        // 却占着垂直居中区最显眼的位置；下面那张快捷键表才是这个空态唯一的信息
        // （此刻能做什么、按什么键）。`gap_4` 随之作废：只剩一个子元素，没有间距可言。
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .bg(crate::theme::surface(1.0, tok.bg_base))
            .child(list)
            .into_any_element()
    }
}

/// 空窗格提示表里的一行：左「动作名」右「键帽组」，整行可点（点即执行）。
///
/// 行为按动作 id 分派，与根节点 `on_action` 的同名分支落点一致；这里直接调
/// `MainView` 的方法，而不是派发同名 action —— 那需要动作在窗口级注册过，
/// 空窗格未必拿得到，直接调用则签名都在手边。
fn no_session_hint_row(
    action_id: &str,
    label: String,
    keys: String,
    cx: &mut Context<MainView>,
) -> AnyElement {
    let tok = crate::theme::current(cx);
    let row = h_flex()
        .id(SharedString::from(format!("empty-hint-{action_id}")))
        .w_full()
        .items_center()
        .justify_between()
        .gap_6()
        .px(px(10.))
        .py(px(5.))
        .rounded(px(6.))
        .hover(|s| s.bg(hex(tok.bg_overlay)))
        .child(
            div()
                .text_size(px(12.5))
                .text_color(hex(tok.fg_secondary))
                .child(SharedString::from(label)),
        )
        .child(hint_keycaps(&keys, tok));

    match action_id {
        "NewTab" => row.on_click(cx.listener(|m, _: &ClickEvent, _w, cx| m.open_local_shell(cx))),
        "CommandPalette" => {
            row.on_click(cx.listener(|m, _: &ClickEvent, w, cx| m.open_command_palette(w, cx)))
        }
        "OpenSettings" => {
            row.on_click(cx.listener(|m, _: &ClickEvent, w, cx| m.open_settings(w, cx)))
        }
        _ => row,
    }
    .into_any_element()
}

/// 把 `"Ctrl+Shift+P"` 画成 `[Ctrl] + [Shift] + [P]` —— 一个按键一枚键帽。
///
/// 分隔符 `+` 单独画一枚弱化字，是为了让「Ctrl 和 Shift 是一起按的」这件事
/// 不被人读成「三个并列的键」（VS Code 空窗格同款表达）。
fn hint_keycaps(display: &str, tok: rustshell_core::theme::ThemeTokens) -> AnyElement {
    let mut row = h_flex().items_center().gap(px(4.)).flex_shrink_0();
    for (ix, part) in display.split('+').enumerate() {
        if ix > 0 {
            row = row.child(
                div()
                    .text_size(px(10.))
                    .text_color(hex(tok.fg_muted))
                    .child("+"),
            );
        }
        row = row.child(hint_keycap(part.to_string(), tok));
    }
    row.into_any_element()
}

/// 一枚键帽。底色取 `bg_elevated`：深色主题下它比 `bg_base` 略亮、浅色主题下比
/// `bg_base` 略暗，两种主题都是「比底纸浮起来一点」的正确方向（若用 `bg_overlay`，
/// 浅色主题里它与 `bg_base` 同为 `#FFFFFF`，键帽会在白底上凭空消失）。
/// 再加 `border_subtle` 描边，保证键帽轮廓清晰。
fn hint_keycap(label: String, tok: rustshell_core::theme::ThemeTokens) -> AnyElement {
    div()
        .flex()
        .items_center()
        .justify_center()
        .h(px(20.))
        .min_w(px(20.))
        .px(px(6.))
        .rounded(px(4.))
        .bg(hex(tok.bg_elevated))
        .border_1()
        .border_color(hex(tok.border_subtle))
        .text_size(px(11.))
        .text_color(hex(tok.fg_secondary))
        .child(SharedString::from(label))
        .into_any_element()
}

/// 终端区右键菜单。菜单项一律作用于「当前焦点窗格」——右键已通过 `focus_pane` 把焦点移过来。
/// 菜单闭包跑在 `Context<PopupMenu>` 里，拿不到 `MainView`，故统一经 `UiHub` 回到实体。
///
/// 2026-09-22 运行反馈：撤掉「向右拆分 / 向下拆分 / 取消分屏 / 新建本地终端」四项。
/// 前三个仍有键盘快捷键（mod-alt-r / mod-alt-d）与 Tab 栏右键菜单、副窗格标题条按钮；
/// 新建本地终端仍有 mod-t、命令面板与终端面板按钮 —— 这里只是不再重复挂一份。
fn pane_context_menu(menu: PopupMenu) -> PopupMenu {
    use super::{ui_call_disconnect_active, UiHub};

    menu.item(
        PopupMenuItem::new(rustshell_core::i18n::ts(
            format!("查找 ({})", crate::keybinds::display_spec("mod-f")),
            format!("Find ({})", crate::keybinds::display_spec("mod-f")),
        ))
        .on_click(|_, w, cx| {
            let Some(hub) = cx.try_global::<UiHub>() else {
                return;
            };
            let main = hub.main.clone();
            let _ = main.update(cx, |m, cx| {
                if m.active_tab().and_then(|t| t.terminal.as_ref()).is_some() {
                    m.open_term_search(w, cx);
                }
            });
        }),
    )
    .item(
        PopupMenuItem::new(rustshell_core::i18n::t("复制")).on_click(|_, _w, cx| {
            let Some(hub) = cx.try_global::<UiHub>() else {
                return;
            };
            let main = hub.main.clone();
            let _ = main.update(cx, |m, cx| {
                if let Some(term) = m.active_tab().and_then(|t| t.terminal.as_ref()) {
                    // 优先取鼠标划选内容，否则回退到整个视口文本
                    let term = term.read(cx);
                    let text = term.selected_text().unwrap_or_else(|| term.visible_text());
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
                }
            });
        }),
    )
    .item(
        PopupMenuItem::new(rustshell_core::i18n::t("粘贴")).on_click(|_, _w, cx| {
            let Some(hub) = cx.try_global::<UiHub>() else {
                return;
            };
            let main = hub.main.clone();
            let _ = main.update(cx, |m, cx| {
                if let Some(text) = cx.read_from_clipboard().and_then(|i| i.text()) {
                    if let Some(term) = m.active_tab().and_then(|t| t.terminal.clone()) {
                        // 走引擎的粘贴**闸门**（而不是直接 write_to_pty / paste_text）：
                        // 行尾转回车 + bracketed paste 包裹，与 Ctrl+Shift+V 同语义；
                        // 含换行时先挂起、由本视图弹确认框（F-5）。
                        term.update(cx, |tv, cx| tv.request_paste(&text, false, cx));
                    }
                }
            });
        }),
    )
    .item(
        PopupMenuItem::new(rustshell_core::i18n::t("粘贴并执行")).on_click(|_, _w, cx| {
            let Some(hub) = cx.try_global::<UiHub>() else {
                return;
            };
            let main = hub.main.clone();
            let _ = main.update(cx, |m, cx| {
                if let Some(text) = cx.read_from_clipboard().and_then(|i| i.text()) {
                    if let Some(term) = m.active_tab().and_then(|t| t.terminal.clone()) {
                        // 末尾补一个回车：多行一次性粘进去后直接逐行执行。
                        // 同样过闸：这条路的「立即执行」意图更强，多行更该先给人看一眼。
                        term.update(cx, |tv, cx| tv.request_paste(&text, true, cx));
                    }
                }
            });
        }),
    )
    .item(
        PopupMenuItem::new(rustshell_core::i18n::ts(
            format!("清屏 ({})", crate::keybinds::display_spec("mod-l")),
            format!("Clear ({})", crate::keybinds::display_spec("mod-l")),
        ))
        .on_click(|_, _w, cx| {
            let Some(hub) = cx.try_global::<UiHub>() else {
                return;
            };
            let main = hub.main.clone();
            let _ = main.update(cx, |m, cx| {
                if let Some(term) = m.active_tab().and_then(|t| t.terminal.clone()) {
                    term.update(cx, |tv, _| {
                        tv.write_to_pty("\x0c");
                    });
                }
            });
        }),
    )
    .separator()
    .item(
        PopupMenuItem::new(rustshell_core::i18n::t("关闭当前标签")).on_click(|_, _w, cx| {
            ui_call_disconnect_active(cx);
        }),
    )
}

/// 终端右键菜单的护栏（2026-09-22 运行反馈：撤掉四项）。
///
/// 断言只切在 `pane_context_menu` 这一个函数里：那四项在**别处**仍是合法入口
/// （Tab 栏右键菜单、`mod-alt-r` / `mod-alt-d`、命令面板、副窗格标题条），
/// 拿整份源码 `contains` 会把「别的入口还在」误判成没删干净。
#[cfg(test)]
mod tests {
    use crate::test_support::prod;

    /// `pane_context_menu` 的函数体（从函数定义起到文件末尾 = 生产源码结尾）。
    ///
    /// 端点用**代码**不用注释：`prod()` 会先剥注释，拿注释里的话去找位置会 panic。
    fn pane_menu() -> String {
        let code = prod(include_str!("split_actions.rs"));
        let at = code
            .find("fn pane_context_menu")
            .expect("终端右键菜单必须存在");
        code[at..].to_string()
    }

    /// 撤掉的四项不得回到终端右键菜单里。
    ///
    /// 针取「开引号 + 关键词」，两种写法都拦得住：`t("向右拆分")` 与
    /// `ts(format!("向右拆分 ({})", …))`（后者在针里是 `"向右拆分 ({})"`，
    /// 不带收尾引号才匹配得上）。
    #[test]
    fn the_pane_menu_drops_the_four_items() {
        let menu = pane_menu();
        for gone in ["向右拆分", "向下拆分", "取消分屏", "新建本地终端"] {
            assert!(
                !menu.contains(&format!("\"{gone}")),
                "终端右键菜单里又出现了「{gone}」—— 该项已按运行反馈撤掉，\
                 真入口在 Tab 栏右键菜单 / 快捷键 / 命令面板，这里不再重复挂一份。"
            );
        }
    }

    /// 没撤的项一个都不能少：菜单是「右键才看得见」的地方，删错了没人会发现。
    #[test]
    fn the_pane_menu_keeps_the_remaining_items() {
        let menu = pane_menu();
        for keep in ["查找", "复制", "粘贴", "粘贴并执行", "清屏", "关闭当前标签"]
        {
            assert!(
                menu.contains(&format!("\"{keep}")),
                "终端右键菜单少了「{keep}」——撤四项时顺手删多了。"
            );
        }
        // 接线：菜单必须真的挂到窗格上（`context_menu` 的构建回调走本函数）
        let code = prod(include_str!("split_actions.rs"));
        assert!(
            code.contains("pane_context_menu(menu)"),
            "`pane_context_menu` 没有挂到窗格上（`context_menu` 回调），改了等于没改。"
        );
    }
}
