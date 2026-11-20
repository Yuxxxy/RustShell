//! 分屏域：标签拆分/取消拆分、窗格焦点与槽位修正。
//! 本模块为 `main_view` 的子模块，可直接访问 `MainView` 的私有字段。

use gpui::{
    div, prelude::FluentBuilder as _, px, AnyElement, ClickEvent, Context, InteractiveElement as _,
    IntoElement, MouseButton, ParentElement as _, SharedString, StatefulInteractiveElement as _,
    Styled as _, Window,
};
use gpui_kit::component::{
    button::{Button, ButtonVariants as _},
    h_flex,
    menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem},
    scroll::ScrollableElement as _,
    v_flex, Icon, IconName, Sizable as _,
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

/// 断连条那句整话：`连接已断开 · <原因> · <重连进展> · 重连失败：<错误>`。
///
/// 抽成纯函数只为一件事：**这段文案是「为什么没连上」的唯一载体**。
/// 渲染帧里读不到文字（`debug_bounds` 只给几何），源码里那几行 push_str
/// 也证明不了拼接顺序对不对（错误尾巴被前一段覆盖、或者压根没拼进去，
/// 都能编译通过）。2026-11-04 用户反馈「连接失败但无任何失败提示」之后就明白了：
/// 这条尾巴必须能被单测直接断言。
///
/// 四段的顺序是**因果顺序**：发生了什么（断开）→ 为什么断（远端关闭 / 网络中断）
/// → 现在在干什么（正在重连 / 等退避）→ 刚才那一次为什么没成（失败原因）。
fn dead_bar_message(
    reason: &str,
    retry: Option<&super::DeadBarRetry>,
    reconnecting: bool,
    retry_error: Option<&str>,
) -> String {
    let mut msg = rustshell_core::i18n::t("连接已断开").to_string();
    msg.push_str(" · ");
    msg.push_str(reason);
    // 重连进展那段有两种句式（F-4）：正在连 vs 在等退避。前者是「已经发出去了」，
    // 后者是「还要等一会儿」—— 用户对着一个不再刷新的画面时，这两句话正是他
    // 判断「我该等还是该自己点一下」的全部依据，所以次数必须写在脸上。
    match retry {
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
        None if reconnecting => {
            msg.push_str(" · ");
            msg.push_str(rustshell_core::i18n::t("正在重连…"));
        }
        None => {}
    }
    if let Some(err) = retry_error {
        msg.push_str(" · ");
        msg.push_str(rustshell_core::i18n::t("重连失败："));
        msg.push_str(err);
    }
    msg
}

impl MainView {
    // 分屏动作

    /// 拆分当前焦点标签到另一侧（向右 / 向下）。
    /// 决策交给 `split::plan_split` 纯函数，这里只负责把决策落到会话上。
    ///
    /// 快捷键（`Ctrl+Alt+R/D`）与「再按一次收起」类入口走这里：**已在分屏时只切方向**
    /// —— 按快捷键的意图就是「换个方向」，不该顺手挪动两格的标签。
    pub fn split_pane(&mut self, axis: SplitAxis, _w: &mut Window, cx: &mut Context<Self>) {
        let focused = self.tabs_state.active.clone();
        self.apply_split_plan(focused, axis, cx);
    }

    /// 以标签栏右键菜单里**点中的那枚标签**为准拆分到另一侧。
    ///
    /// 版面口径（`render_workspace` 里定的，别搞反）：`primary` 是第一格 —— 左右分屏的
    /// **左**、上下分屏的**上**；`secondary` 是第二格 —— **右 / 下**。
    ///
    /// **已分屏时：副格永远变成点中的那枚**（2026-11-17 运行反馈「下方已经有了就替换下方」
    /// + 「点上方那枚时要对调」）：
    /// * 点中的标签不在分屏里 ⇒ 它**替换副格**（原来占副格的标签回标签栏，不关会话），主格不动；
    /// * 点中的就是副格那枚 ⇒ 幂等，只切方向；
    /// * 点中的是主格那枚 ⇒ 两格**对调** —— 同一枚标签不能占两格，用户按的是
    ///   「把这枚拆到下面」，它必须离开主格，原来副格那枚升上来。
    ///
    /// **未分屏时**（拆出第一刀）：
    /// * 池里还有别的标签 ⇒ 点中的标签进副格，搭档去主格。搭档优先取「拆分前的焦点
    ///   标签」（正在看的那格不挪位），否则取池里第一个不是它的标签；
    /// * 池里只剩它自己 ⇒ 它留主格，新开一个同类型会话进副格（这时「往下拆」只能是
    ///   在它**下面**开一格，没有第二个标签可以挪）。
    pub fn split_pane_from(&mut self, target: &str, axis: SplitAxis, cx: &mut Context<Self>) {
        let pool: Vec<String> = self
            .tabs_state
            .tabs
            .iter()
            .map(|t| t.session.id.clone())
            .collect();
        if !pool.iter().any(|id| id == target) {
            // 标签在这一帧之前被关掉了（菜单是渲染期快照）：说清楚，别静默
            self.set_info(
                rustshell_core::i18n::t("没有可拆分的会话").to_string(),
                false,
            );
            cx.notify();
            return;
        }

        // 已分屏：主格不动，只替换副格（规则见 `core::split::anchor_split`）
        if let Some(sp) = self.tabs_state.split.as_mut() {
            let (primary, secondary) =
                rustshell_core::split::anchor_split(&sp.primary, &sp.secondary, target);
            sp.primary = primary;
            sp.secondary = secondary;
            sp.axis = axis;
            self.tabs_state.active = Some(target.to_string());
            self.tabs_state.pending_focus = true;
            cx.notify();
            return;
        }

        // 未分屏：先找搭档 —— 优先「拆分前的焦点标签」（正在看的那格不挪位），
        // 其次池里第一个不是它的标签。点中的标签自己当焦点时也要走这条，
        // 否则「A、B 两个标签、当前在 B、右键 B 向下拆」会把 B 留在上面。
        let partner = self
            .tabs_state
            .active
            .clone()
            .filter(|id| id != target && pool.iter().any(|p| p == id))
            .or_else(|| pool.iter().find(|id| *id != target).cloned());
        if let Some(primary) = partner {
            self.tabs_state.split = Some(SplitPane {
                axis,
                primary,
                secondary: target.to_string(),
            });
            self.tabs_state.active = Some(target.to_string());
            self.tabs_state.pending_focus = true;
            cx.notify();
            return;
        }

        // 池里只有它自己：它留主格，新开会话进副格
        self.apply_split_plan(Some(target.to_string()), axis, cx);
    }

    /// `plan_split` 的落地：未分屏时挑搭档 / 新开会话；已分屏时只切方向。
    fn apply_split_plan(
        &mut self,
        focused: Option<String>,
        axis: SplitAxis,
        cx: &mut Context<Self>,
    ) {
        let pool: Vec<String> = self
            .tabs_state
            .tabs
            .iter()
            .map(|t| t.session.id.clone())
            .collect();
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
        // 2026-11-16 用户反馈「双层标题栏」：分屏时**不再**给每格画标题条。
        // 那条 24px 的条上只有四样东西 —— 标题、状态点、取消分屏按钮、焦点标记，
        // 前两样标签栏本来就有一份（同一个 `display_tab_title`、同一枚状态点），
        // 第三样挪回窗格右键菜单，只剩焦点标记是它独有的，改用**顶部 2px accent 横条**
        // 表达（`absolute` 叠在内容上方：不占行高，也不像边框那样吃掉 1px 内容宽度
        // 触发 PTY resize、切焦点时画面抖）。
        let accent = hex(crate::theme::current(cx).accent);
        let primary = {
            let body = self.render_term_pane(&sp.primary, cx);
            split_pane_shell(
                body,
                PaneSide::Primary,
                focused_side == PaneSide::Primary,
                accent,
            )
        };
        let secondary = {
            let body = self.render_term_pane(&sp.secondary, cx);
            split_pane_shell(
                body,
                PaneSide::Secondary,
                focused_side == PaneSide::Secondary,
                accent,
            )
        };

        // 两个面板都不给初始尺寸，各自 flex_grow_1，初始各占一半；
        // 拖拽由官方`ResizableState`接管并记住比例。两侧共用同一 id，
        // 切换方向时尺寸状态随之复用，不会被重置回 50/50。
        let group = match sp.axis {
            SplitAxis::Vertical => gpui_kit::component::resizable::h_resizable("term-split"),
            SplitAxis::Horizontal => gpui_kit::component::resizable::v_resizable("term-split"),
        };
        group
            .child(gpui_kit::component::resizable::resizable_panel().child(primary))
            .child(gpui_kit::component::resizable::resizable_panel().child(secondary))
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

        // 独立 SFTP 标签页（2026-10-26）：单通道会话没有 PTY，窗格里画的是
        // WinSCP 式双窗格，而不是「连接失败 / 正在连接」那张占位卡。
        // 判定放在取数据之前 —— 下面整段都是给终端窗格准备的。
        if self
            .tabs_state
            .tabs
            .iter()
            .find(|t| t.session.id == session_id)
            .is_some_and(|t| t.is_sftp_live())
        {
            return crate::sftp_tab::render(self, session_id, cx);
        }

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
                        tab.is_sftp_live(),
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
        // ZMODEM：拖放落点与传输浮层都要用终端实体，而下面的 `term` 会被移动进
        // 窗格子节点，所以在这里各取一份。
        let drop_term = term.clone();
        let zmodem = term
            .as_ref()
            .map(|t| (t.clone(), t.read(cx).zmodem_status()));
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
                |d, (title, err, started, kind, disconnected, sftp_only)| {
                    d.child(super::render_connect_placeholder(
                        &tok,
                        placeholder_bg,
                        super::ConnectPlaceholderState {
                            title: &title,
                            err: err.as_deref(),
                            started,
                            kind,
                            disconnected,
                            sftp_only,
                        },
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

        // ZMODEM 浮层挂在最后（断连条之上）：传输进行中它是这块窗格唯一会动的东西。
        if let Some((term_ent, status)) = zmodem.as_ref() {
            if let Some(bar) = Self::render_zmodem_bar(term_ent, status, cx) {
                pane = pane.child(bar);
            }
        }

        // 拖放上传的落点挂在**窗格根**上：gpui 的命中测试把整条命中路径上的祖先
        // hitbox 都算 hovered（同 `files_panel` 的 F-13），挂一次就覆盖整块终端 ——
        // 引擎的终端视图自己不认识文件拖放，它只负责把拖进来的路径交给字节泵。
        if let Some(term_ent) = drop_term {
            pane = pane
                .can_drop(|value, _, _| value.is::<gpui::ExternalPaths>())
                .drag_over::<gpui::ExternalPaths>(move |style, _paths, _w, _cx| {
                    style.bg(hex(tok.bg_elevated))
                })
                .on_drop::<gpui::ExternalPaths>(move |paths: &gpui::ExternalPaths, w, cx| {
                    MainView::zmodem_drop(term_ent.clone(), paths.paths().to_vec(), w, cx);
                });
        }

        // 「取消分屏」现在**只有**这里和标签右键菜单两个入口 —— 2026-11-16 按用户反馈
        // 删掉了每格顶部那条 24px 的窗格标题条（双层标题栏），原本挂在上面的按钮
        // 随之消失，于是把这一项放回窗格右键菜单（2026-09-22 曾撤过，那时有标题条按钮
        // 顶着；现在没有按钮了，右键是最近的位置）。未分屏时置灰。
        let in_split = self.tabs_state.split.is_some();
        pane.context_menu(move |menu: PopupMenu, _w, _cx| pane_context_menu(menu, in_split))
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
        // 「连接已断开 · 连接已被远端关闭 · 正在重连…（第 2/3 次） · 重连失败：认证失败」
        //
        // 文案拼装抽成 [`dead_bar_message`]（纯函数）：这段字符串是「连接为什么没连上」
        // 的唯一载体，只能靠单测钉住（渲染帧里读不到文字）。2026-11-04 用户反馈
        // 「连接失败但无任何失败提示」时，先要能证明这句尾巴确实拼进去了。
        let msg = dead_bar_message(
            &plan.reason,
            plan.retry.as_ref(),
            plan.reconnecting,
            plan.retry_error.as_deref(),
        );

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
    ///
    /// 2026-11-05 用户要求「设置-窗口-启动时打开 默认什么都不做，默认连接把之前
    /// 命令面板中的快速连接列表展示出来」：所以空态的最上面现在是一张**快速连接
    /// 表**（与命令面板同一个数据源 `command::quick_connect`：按最近访问倒序、
    /// 从未连过的按名字升序 —— 排序完全确定，不会每次打开都在跳），点一行就连那台
    /// 机器。原来的快捷键表退到下面，用一条分隔线隔开：先「连哪台机器」，
    /// 再「没有连接时还能做什么」。
    fn render_no_session(&self, cx: &mut Context<Self>) -> AnyElement {
        let tok = crate::theme::current(cx);
        let hints = crate::keybinds::empty_state_hints(&self.panel_ui.keymap_rows);
        // 与命令面板**同一份**数据：`quick_connect` 把连接配置摊成带 name /
        // `用户名@主机:端口` / 分组名的条目，空态直接照它的口径画。
        let connects = rustshell_core::command::quick_connect(&self.conn_store);

        let mut list = v_flex().w(px(400.)).gap(px(1.)).child(
            div()
                .px(px(10.))
                .pb(px(4.))
                .text_size(px(11.))
                .text_color(hex(tok.fg_muted))
                .child(rustshell_core::i18n::t("快速连接")),
        );

        if connects.is_empty() {
            // 一台机器都没存：给一句实话 + 唯一的下一步（别让人对着空表发呆）。
            list = list.child(
                h_flex()
                    .items_center()
                    .justify_between()
                    .gap_4()
                    .px(px(10.))
                    .py(px(6.))
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(hex(tok.fg_muted))
                            .child(rustshell_core::i18n::t("还没有保存的连接")),
                    )
                    .child(
                        Button::new("empty-new-connection")
                            .small()
                            .label(rustshell_core::i18n::t("新建连接"))
                            .on_click(cx.listener(|m, _: &ClickEvent, w, cx| {
                                m.open_connect_dialog(None, w, cx)
                            })),
                    ),
            );
        } else {
            // 连接很多时自己滚，不把下面的快捷键表顶出窗口。
            let mut rows = v_flex()
                .debug_selector(|| "empty-quick-connect".to_string())
                .w_full()
                .gap(px(1.))
                .max_h(px(320.));
            for entry in &connects {
                rows = rows.child(no_session_connect_row(entry, cx));
            }
            list = list.child(rows.overflow_y_scrollbar());
        }

        let mut hint_list = v_flex()
            .debug_selector(|| "empty-hints".to_string())
            .w_full()
            .gap(px(2.));
        // 2026-11-20 用户要求「（空窗格）新建本地终端上方增加新增连接」：
        // 它排在 `EMPTY_STATE_ACTIONS` 的**第一位**，并带 `Ctrl+N`（`mod-n`）——
        // 有了键位就能走统一循环，不必再单独插一行。
        for (id, (label, keys)) in crate::keybinds::EMPTY_STATE_ACTIONS.iter().zip(hints) {
            hint_list = hint_list.child(no_session_hint_row(id, label, keys, cx));
        }
        // 2026-09-22 用户要求「移除主窗格为空」：顶部那行「图标 + 主窗格为空」撤下。
        // 它说的是**用户已经知道的事**（窗格是空的 —— 一片空白本身就是证据），
        // 却占着垂直居中区最显眼的位置。现在这一屏的结构是「快速连接 → 分隔线 →
        // 快捷键表」，每一块都是「此刻能做的事」。
        list = list.child(
            div()
                .h(px(1.))
                .w_full()
                .my(px(8.))
                .bg(hex(tok.border_subtle)),
        );
        list = list.child(hint_list);

        div()
            .size_full()
            .min_w_0()
            .pt(px(20.))
            .bg(crate::theme::surface(1.0, tok.bg_base))
            // 2026-11-20 用户要求「这个调整位置：在界面左边三分之一」：
            // 用 1 : 2 两块空白把 400px 的内容推到左 1/3 处（此前是 `mx_auto` 居中）。
            //
            // 用 `flex_1` / `flex_2` 而不是固定像素：窗口一宽就按比例走，
            // 而写死 `ml(px(..))` 在大屏上会停在靠左的一小段。
            .child(
                h_flex()
                    .size_full()
                    .items_start()
                    .child(div().flex_grow(1.).min_w_0())
                    .child(list.flex_none())
                    .child(div().flex_grow(2.).min_w_0()),
            )
            .into_any_element()
    }
}

/// 空态快速连接的一行：名称 + `用户名@主机:端口` + 所属分组，点整行即连接。
///
/// 数据取自 [`rustshell_core::command::quick_connect`]，所以与命令面板里那一份
/// **完全同构**（同样是两行文字 + 右侧分组名）；差别只是这里铺成静态列表，
/// 不用再输入搜索词。
fn no_session_connect_row(
    entry: &rustshell_core::command::CommandEntry,
    cx: &mut Context<MainView>,
) -> AnyElement {
    let tok = crate::theme::current(cx);
    let rustshell_core::command::CommandOrigin::ConnectProfile { profile_id } = &entry.origin
    else {
        // 内置命令 / AI 直通不该出现在这张表里（`quick_connect` 只会产出连接条目）；
        // 真出现了就留空，别把「新增连接」点成「连接某个 id」。
        return div().into_any_element();
    };
    let id = profile_id.clone();
    h_flex()
        .id(SharedString::from(format!("empty-connect-{id}")))
        .w_full()
        .items_center()
        .gap_2()
        .px(px(10.))
        .py(px(5.))
        .rounded(px(6.))
        .hover(|s| s.bg(hex(tok.bg_overlay)))
        .on_click(cx.listener(move |m, _: &ClickEvent, w, cx| m.connect_profile(&id, w, cx)))
        .child(
            Icon::new(IconName::Globe)
                .small()
                .text_color(hex(tok.fg_muted)),
        )
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .child(
                    div()
                        .truncate()
                        .text_size(px(12.5))
                        .text_color(hex(tok.fg_primary))
                        .child(SharedString::from(entry.label.clone())),
                )
                .child(
                    div()
                        .truncate()
                        .text_size(px(10.5))
                        .text_color(hex(tok.fg_muted))
                        .child(SharedString::from(entry.detail.clone().unwrap_or_default())),
                ),
        )
        .children(entry.group.clone().map(|g| {
            div()
                .flex_shrink_0()
                .text_size(px(10.))
                .text_color(hex(tok.fg_muted))
                .child(SharedString::from(g))
        }))
        .into_any_element()
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
        );
    // 没有键位的行（「新增连接」）不画键帽：`display.split('+')` 对空串会产出一个
    // 空键帽（20×20 的空框），看起来像「这里本该有个键」。
    let row = if keys.is_empty() {
        row
    } else {
        row.child(hint_keycaps(&keys, tok))
    };

    match action_id {
        "NewTab" => row.on_click(cx.listener(|m, _: &ClickEvent, _w, cx| m.open_local_shell(cx))),
        "NewConnection" => {
            row.on_click(cx.listener(|m, _: &ClickEvent, w, cx| m.open_connect_dialog(None, w, cx)))
        }
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
/// 分屏时的一格：内容 + （焦点时）顶部 2px accent 横条。
///
/// 2026-11-16 用户反馈「双层标题栏」之后，窗格不再有自己的标题条 —— 标题与状态点
/// 标签栏里已经有同一份（同一个 `display_tab_title`、同一枚点），再画一遍只是白占
/// 24px 行高。这里只补那条**真正独有的**信息：这一格是不是焦点。
///
/// 横条用 `absolute` 叠在内容上：不占行高，也不像边框那样吃掉 1px 内容宽度
/// （那会触发终端列数重算 = PTY resize，切焦点时画面会抖）。
fn split_pane_shell(
    body: AnyElement,
    side: PaneSide,
    focused: bool,
    accent: gpui::Hsla,
) -> AnyElement {
    let shell = v_flex()
        .relative()
        .flex_1()
        .min_w_0()
        .min_h_0()
        // 几何类用例用它量「哪一格在上面 / 左边」。用**固定名字**（按槽位，不按会话
        // id）：`VisualTestContext::debug_bounds` 只收 `&'static str`。
        .debug_selector(move || match side {
            PaneSide::Primary => "pane-body-primary".to_string(),
            PaneSide::Secondary => "pane-body-secondary".to_string(),
        })
        .child(body);
    if !focused {
        return shell.into_any_element();
    }
    shell
        .child(
            div()
                .debug_selector(|| "pane-focus-bar".to_string())
                .absolute()
                .top_0()
                .left_0()
                .right_0()
                .h(px(2.))
                .bg(accent),
        )
        .into_any_element()
}

fn pane_context_menu(menu: PopupMenu, in_split: bool) -> PopupMenu {
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
        PopupMenuItem::new(rustshell_core::i18n::t("取消分屏"))
            .disabled(!in_split)
            .on_click(|_, _w, cx| {
                let Some(hub) = cx.try_global::<UiHub>() else {
                    return;
                };
                let main = hub.main.clone();
                let _ = main.update(cx, |m, cx| m.unsplit(cx));
            }),
    )
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

    /// 2026-11-20 用户要求「（空窗格）新建本地终端上方增加新增连接」，并给了 `Ctrl+N`。
    ///
    /// 位置由 `EMPTY_STATE_ACTIONS` 的**次序**决定（渲染就是按它铺的），键帽由
    /// `DEFAULT_BINDINGS` 里的 `mod-n` 决定 —— 两处都要钉，否则「排上去了但没键位」
    /// 会画出一行光秃秃的提示。
    #[test]
    fn the_empty_state_lists_new_connection_above_new_local_terminal() {
        assert_eq!(
            crate::keybinds::EMPTY_STATE_ACTIONS[0],
            "NewConnection",
            "空窗格第一行必须是「新增连接」（用户 2026-11-20 要求它排在新建终端上面）"
        );
        assert_eq!(
            crate::keybinds::EMPTY_STATE_ACTIONS[1],
            "NewTab",
            "第二行是「新建本地终端」—— 它是被让位的那一行，顺序别动"
        );
        let spec = rustshell_platform::keymap::DEFAULT_BINDINGS
            .iter()
            .find(|(id, _)| *id == "NewConnection")
            .map(|(_, spec)| *spec);
        assert_eq!(spec, Some("mod-n"), "新增连接的默认键位必须是 Ctrl+N");

        // 它必须有键位可画：`empty_state_hints` 取不到 spec 时会画成一枚空键帽
        let code = prod(include_str!("split_actions.rs"));
        assert!(
            code.contains("keys.is_empty()"),
            "无键位的行必须跳过键帽绘制（空键位串会画出一枚 20x20 的空框）"
        );
    }

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
        // 2026-11-16：「取消分屏」**回到了**这个菜单（见下一条用例）。删掉每格顶部
        // 那条窗格标题条之后，原本挂在它上面的按钮没有了，右键是最近的位置。
        for gone in ["向右拆分", "向下拆分", "新建本地终端"] {
            assert!(
                !menu.contains(&format!("\"{gone}")),
                "终端右键菜单里又出现了「{gone}」—— 该项已按运行反馈撤掉，\
                 真入口在 Tab 栏右键菜单 / 快捷键 / 命令面板，这里不再重复挂一份。"
            );
        }
    }

    /// 2026-11-16 删掉窗格标题条之后，「取消分屏」的唯一按钮入口没了 ⇒ 必须回到
    /// 窗格右键菜单，否则用户只能去标签右键菜单里找（或是蒙快捷键）。
    ///
    /// 两条一起守：菜单里有这一项，且它按分屏态置灰（未分屏时点了没意义）。
    #[test]
    fn the_pane_menu_brings_unsplit_back() {
        let menu = pane_menu();
        assert!(
            menu.contains("\"取消分屏\""),
            "窗格右键菜单少了「取消分屏」：标题条按钮已随「双层标题栏」一起删掉，\n\
             这里是最近的替代入口。"
        );
        assert!(
            menu.contains(".disabled(!in_split)"),
            "「取消分屏」必须按分屏态置灰（`disabled(!in_split)`），否则未分屏时是一个死按钮。"
        );
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
            code.contains("pane_context_menu(menu"),
            "`pane_context_menu` 没有挂到窗格上（`context_menu` 回调），改了等于没改。"
        );
    }

    /// 2026-11-05 用户要求「启动时打开默认什么都不做」「默认连接把之前命令面板
    /// 中的快速连接列表展示出来」：空窗格必须直接给出**连接表**，而且与命令面板
    /// 用同一份数据源。
    ///
    /// 三条一起钉：
    /// ① 数据源是 `command::quick_connect`（另写一份手列表 ⇒ 两处口径迟早漂移，
    ///    比如一边按最近访问倒序、另一边按名字）；
    /// ② 点整行走 `connect_profile`（不是新开一个本地 Shell —— 空态的主角是「连哪台」）；
    /// ③ 连接表排在快捷键表**前面**（先给入口，再给「没有连接时还能做什么」）。
    #[test]
    fn the_empty_pane_lists_quick_connects_first() {
        let code = prod(include_str!("split_actions.rs"));
        let at = code
            .find("fn render_no_session(")
            .expect("找不到空窗格的渲染函数");
        let body = &code[at..];
        let end = body.find("\n}\n").expect("找不到空窗格函数结尾");
        let body = &body[..end];

        assert!(
            body.contains("rustshell_core::command::quick_connect(&self.conn_store)"),
            "空窗格必须用命令面板那份 `quick_connect`（排序与字段口径同源）"
        );
        let connects = body
            .find("no_session_connect_row")
            .expect("空窗格没有渲染连接行");
        let hints = body
            .find("no_session_hint_row")
            .expect("空窗格的快捷键表不见了");
        assert!(
            connects < hints,
            "连接表必须排在快捷键表前面：空窗格的第一诉求是「连哪台机器」"
        );

        let at = code
            .find("fn no_session_connect_row(")
            .expect("找不到空窗格的连接行");
        let row = &code[at..];
        assert!(
            row.contains("m.connect_profile(&id, w, cx)"),
            "点连接行必须走 `connect_profile`（连接那台机器），不是开本地终端"
        );
    }

    /// 2026-10-30 「拆分的标签名称 / 状态点与标签栏对不上」那一类问题的**根治**。
    ///
    /// 当时的做法是让窗格标题条也走 `display_tab_title` + `is_healthy`，与标签栏同一份。
    /// 2026-11-16 用户反馈「双层标题栏」之后，窗格不再有标题条 —— **重复的那一份没了**，
    /// 「两处叫法不一致 / 两点颜色不一致」这一整类 bug 也就无从发生。
    ///
    /// 所以这里不再钉具体函数，改为钉**结构**：分屏窗格只留内容，不得再长出
    /// 自己的标题条（否则那句话又会被说第二遍）。
    #[test]
    fn split_panes_carry_no_second_title_bar() {
        let code = prod(include_str!("split_actions.rs"));
        assert!(
            !code.contains("render_pane_header"),
            "分屏窗格又长出了自己的标题条（`render_pane_header`）：标题与状态点标签栏里\n\
             已经有同一份，再画一遍就是用户报的「双层标题栏」。\n\
             焦点请用 `split_pane_shell` 的顶部 2px accent 横条表达。"
        );
        // 焦点标记必须还在（删标题条时最容易顺手把它一起删掉）
        assert!(
            code.contains("\"pane-focus-bar\""),
            "窗格失去了焦点标记：删标题条可以，但「哪一格是焦点」必须还有别的方式表达。"
        );
    }

    /// 断连条文案必须**带上失败原因**（2026-11-04 用户反馈
    /// 「ssh 连接断开后，tab 右键【连接】，连接失败但是无任何失败提示」）。
    ///
    /// 这一条只能靠纯函数钉：渲染帧里读不到文字，`debug_bounds` 只给几何 ——
    /// 文案里少拼一段时，条还在、高度还在、什么都测不出来。
    #[test]
    fn the_dead_bar_spells_out_why_the_reconnect_failed() {
        use super::dead_bar_message;
        use crate::main_view::DeadBarRetry;

        // ① 用户手动重连失败：没有账本、在连、带着失败原因
        let msg = dead_bar_message("连接已被远端关闭", None, true, Some("认证失败"));
        assert!(
            msg.contains("连接已断开"),
            "断连条必须先说清发生了什么：{msg}"
        );
        assert!(
            msg.contains("连接已被远端关闭"),
            "断连条要带上断连原因：{msg}"
        );
        assert!(
            msg.contains("重连失败：") && msg.contains("认证失败"),
            "用户手动重连失败后，断连条上必须出现失败原因 —— \
             这正是「连接失败但无任何提示」那一条反馈的落点：{msg}"
        );

        // ② 有账本时给进展（等退避 / 正在连），并且失败尾巴仍然在
        let waiting = DeadBarRetry {
            attempt: 2,
            max: 3,
            waiting: true,
        };
        let msg = dead_bar_message("连接已被远端关闭", Some(&waiting), false, Some("超时"));
        assert!(msg.contains("等待重试（第 2/3 次）"), "等退避的句式：{msg}");
        assert!(msg.contains("超时"), "失败原因不许被进展那段挤掉：{msg}");

        let flying = DeadBarRetry {
            attempt: 2,
            max: 3,
            waiting: false,
        };
        let msg = dead_bar_message("连接已被远端关闭", Some(&flying), true, None);
        assert!(msg.contains("正在重连…（第 2/3 次）"), "在连的句式：{msg}");
        assert!(
            !msg.contains("重连失败"),
            "没有失败原因时不许凭空写「重连失败」：{msg}"
        );

        // ③ 干断着（没人去连）：只有断开 + 原因，两段都不该多出来
        let msg = dead_bar_message("网络中断", None, false, None);
        assert_eq!(
            msg,
            format!("{} · 网络中断", rustshell_core::i18n::t("连接已断开")),
            "干断着的标签不该出现重连字样"
        );
    }

    /// 2026-11-16 两轮运行反馈（「往下拆分 / 往右拆分」= **右键的这枚标签挪到下面 /
    /// 右面那一格**）——接线层。
    ///
    /// 纯决策（槽位怎么排）由 `core::split::anchor_split` 的用例钉；这里钉的是
    /// 「UI 把点中的 id 喂对了、且它落到正确的格」。四条一次跑完（同一条时间线演进）：
    /// ① 未分屏点 gamma（当前焦点是 alpha）→ alpha 留主格、gamma 进副格；
    /// ② 已分屏点第三个标签 beta → beta 进副格、主格不动；
    /// ③ 已分屏点主格里那枚 alpha → 两格**对调**（它去下格，原下格那枚升上来）；
    /// ④ 未分屏点**当前焦点**自己 → 它留主格、池里挑搭档进副格（「拆自己」）。
    ///
    /// 标签是 `SessionHandle::pending` 造的占位会话（不碰 PTY）。
    #[gpui::test]
    fn split_from_puts_the_clicked_tab_in_the_second_pane(cx: &mut gpui::TestAppContext) {
        use crate::main_view::dialogs::tests::build_main;
        use crate::main_view::TabEntry;
        use rustshell_core::session::{SessionHandle, SessionKind};
        use rustshell_core::split::SplitAxis;

        let (main, vcx) = build_main(cx);
        main.update_in(&mut vcx.clone(), |m, _w, cx| {
            let mut ids = Vec::new();
            for title in ["alpha", "beta", "gamma"] {
                let session = SessionHandle::pending(
                    rustshell_core::connections::uuid_like(),
                    SessionKind::Local,
                    title.to_string(),
                    None,
                );
                ids.push(session.id.clone());
                m.tabs_state.tabs.push(TabEntry {
                    session,
                    terminal: None,
                    scroll: None,
                    connect_error: None,
                    connect_started: None,
                    dead: None,
                    retry: None,
                });
            }
            m.tabs_state.active = Some(ids[0].clone());

            // ① 未分屏 + 点的是别的标签：它去下格，当前在看的那格留下
            m.split_pane_from(&ids[2], SplitAxis::Horizontal, cx);
            let sp = m.tabs_state.split.as_ref().expect("应当开了分屏");
            assert_eq!(sp.primary, ids[0], "当前在看的那格不该被换走");
            assert_eq!(sp.secondary, ids[2], "点中的标签必须落在下/右那一格");
            assert_eq!(sp.axis, SplitAxis::Horizontal, "方向必须是本次点的那一个");

            // ② 已分屏 + 点分屏之外的标签：进副格，主格不动
            m.split_pane_from(&ids[1], SplitAxis::Vertical, cx);
            let sp = m.tabs_state.split.as_ref().expect("分屏态不该被取消");
            assert_eq!(sp.primary, ids[0], "主格（正在看的那格）不该动");
            assert_eq!(sp.secondary, ids[1], "第三个标签必须进副格");
            assert_eq!(sp.axis, SplitAxis::Vertical);

            // ③ 已分屏 + 点主格里那枚标签：两格**对调** —— 它必须离开主格才能落到副格，
            //    原来副格那枚升上来（2026-11-17 用户明确要求「点上方那枚要对调」）
            m.split_pane_from(&ids[0], SplitAxis::Horizontal, cx);
            let sp = m.tabs_state.split.as_ref().expect("分屏态不该被取消");
            assert_eq!(sp.secondary, ids[0], "点主格那枚必须把它送到副格（下/右）");
            assert_eq!(sp.primary, ids[1], "原来副格那枚升到主格");
            assert_eq!(sp.axis, SplitAxis::Horizontal);

            // ④ 未分屏 + 点的是当前焦点自己：**照样**进副格（这条就是 A/B 两标签的场景，
            //    上一版在这里把它留在了主格 —— 用户报的「B 应该到下面」正是这条）
            m.unsplit(cx);
            m.tabs_state.active = Some(ids[2].clone());
            m.split_pane_from(&ids[2], SplitAxis::Horizontal, cx);
            let sp = m.tabs_state.split.as_ref().expect("点自己也要开分屏");
            assert_eq!(
                sp.secondary, ids[2],
                "点中的标签永远在副格（下/右），点自己也不例外"
            );
            assert_eq!(sp.primary, ids[0], "搭档取池里第一个别的标签，留在主格");
        });
    }

    /// 用户报告的最小复现（2026-11-16 第三轮）：
    /// 「同时存在 A B 两个 tab，我在 tab B 上右键选择向下拆分，则 **tab B 拆分到下方**」。
    ///
    /// 上一版对「右键的就是当前焦点」留了特例（自己留主格），于是 B 留在上面、A 被换走下去
    /// —— 两个标签时这个特例几乎每次都会命中。这里把「焦点就在 B」这个最坏情形钉死。
    #[gpui::test]
    fn splitting_down_a_two_tab_set_puts_the_clicked_tab_below(cx: &mut gpui::TestAppContext) {
        use crate::main_view::TabEntry;
        use rustshell_core::session::{SessionHandle, SessionKind};
        use rustshell_core::split::SplitAxis;

        let (main, vcx) = crate::main_view::dialogs::tests::build_main(cx);
        let mut ids = Vec::new();
        main.update_in(&mut vcx.clone(), |m, _w, cx| {
            for title in ["A", "B"] {
                let session = SessionHandle::pending(
                    rustshell_core::connections::uuid_like(),
                    SessionKind::Local,
                    title.to_string(),
                    None,
                );
                ids.push(session.id.clone());
                m.tabs_state.tabs.push(TabEntry {
                    session,
                    terminal: None,
                    scroll: None,
                    connect_error: None,
                    connect_started: None,
                    dead: None,
                    retry: None,
                });
            }
            // 最坏情形：当前焦点就在 B（被右键的那枚）
            m.tabs_state.active = Some(ids[1].clone());

            m.split_pane_from(&ids[1], SplitAxis::Horizontal, cx);

            let sp = m.tabs_state.split.as_ref().expect("应当开了分屏");
            assert_eq!(sp.primary, ids[0], "A 必须留在上方");
            assert_eq!(sp.secondary, ids[1], "右键的 B 必须落到下方");
            assert_eq!(sp.axis, SplitAxis::Horizontal);
        });
    }

    /// 版面口径的**几何事实**：`primary` 是第一格 —— 上下分屏在上、左右分屏在左。
    ///
    /// 这是「右键拆分往哪一侧走」的地基：上一条用例断言的是**槽位**，槽位到屏幕的
    /// 映射只写在这里。两者若搞反，症状就是用户报的「选了往下拆分，它却跑到上面」。
    #[gpui::test]
    fn the_primary_pane_is_the_top_and_left_one(cx: &mut gpui::TestAppContext) {
        use crate::main_view::TabEntry;
        use rustshell_core::session::{SessionHandle, SessionKind};
        use rustshell_core::split::SplitAxis;

        let (main, vcx) = crate::main_view::dialogs::tests::build_main(cx);
        let mut ids = Vec::new();
        main.update_in(&mut vcx.clone(), |m, _w, cx| {
            for title in ["alpha", "beta"] {
                let session = SessionHandle::pending(
                    rustshell_core::connections::uuid_like(),
                    SessionKind::Local,
                    title.to_string(),
                    None,
                );
                ids.push(session.id.clone());
                m.tabs_state.tabs.push(TabEntry {
                    session,
                    terminal: None,
                    scroll: None,
                    connect_error: None,
                    connect_started: None,
                    dead: None,
                    retry: None,
                });
            }
            m.tabs_state.active = Some(ids[0].clone());
            // 点 beta 往下拆 → alpha 留主格、beta 落副格
            m.split_pane_from(&ids[1], SplitAxis::Horizontal, cx);
        });

        // 先标脏再出帧：复用帧里的 `debug_bounds` 是旧几何（见项目记忆里的复用帧坑）
        let (primary, secondary) = {
            vcx.run_until_parked();
            vcx.update(|w, cx| {
                w.refresh();
                w.draw(cx).clear(cx);
            });
            let p = vcx
                .debug_bounds("pane-body-primary")
                .expect("主窗格内容区没渲染出来");
            let s = vcx
                .debug_bounds("pane-body-secondary")
                .expect("副窗格内容区没渲染出来");
            (p, s)
        };
        assert!(
            primary.origin.y < secondary.origin.y,
            "上下分屏时 primary 必须在上（primary.y={:?} / secondary.y={:?}）",
            primary.origin.y,
            secondary.origin.y
        );

        // 再点主格那枚 alpha 选「向右拆分」：对调 + 换方向 → beta 去左、alpha 去右
        main.update_in(&mut vcx.clone(), |m, _w, cx| {
            m.split_pane_from(&ids[0], SplitAxis::Vertical, cx);
        });
        vcx.run_until_parked();
        vcx.update(|w, cx| {
            w.refresh();
            w.draw(cx).clear(cx);
        });
        let primary = vcx
            .debug_bounds("pane-body-primary")
            .expect("主窗格内容区没渲染出来");
        let secondary = vcx
            .debug_bounds("pane-body-secondary")
            .expect("副窗格内容区没渲染出来");
        assert!(
            primary.origin.x < secondary.origin.x,
            "左右分屏时 primary 必须在左（primary.x={:?} / secondary.x={:?}）",
            primary.origin.x,
            secondary.origin.x
        );
    }
}
