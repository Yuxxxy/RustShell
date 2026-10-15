//! 「免确认白名单」配置框（设置 - Agent 页右侧的「配置」按钮）。
//!
//! 白名单是「免确认直接执行」的**唯一**依据（`core::agent::judge`）：命中的状态
//! 变更命令直跑；未命中的一律先弹确认卡片；破坏类命令永不执行（加进来也没用）；
//! 批量执行与无人值守巡检一律不适用。
//!
//! 2026-09-22 运行反馈「右侧改为配置按钮，点击弹出框展示白名单列表」：条目原先
//! 内联铺在设置页上（列表 + 添加框），条目一多就把整页撑开；改成弹窗后页面只留
//! 一句说明，白名单本身有了搜索、逐条开关与删除的位置。
//!
//! 同一天的反馈「点开配置需要展示代码里默认白名单的命令」：代码里并不存在默认
//! 白名单命令集（`AiConfig::default().auto_approve` 一直是空的），用户想看的是
//! **本来就不弹确认的那批只读命令** —— 见 `core::ai::BUILTIN_READONLY_COMMANDS`，
//! 框里只读地列出来，不给开关（关掉只读命令不会改变任何行为）。
//!
//! 再一批反馈「改为 tab 方式，分为自定义白名单和内置白名单，默认展示自定义，
//! 名单用 table 组件形式可滚动」：两类名单各占一个页签（默认落在自定义页），
//! 条目多了在框内滚 —— 只读清单一多就不再和自定义条目挤在同一条流里。
//!
//! 2026-09-23「白名单命令改为 List 组件」：名单本体由官方 `Table` 换成官方
//! `List`。`Table` 是静态表格的语义（行数再多也整份渲染），`List` 才是长名单的
//! 正主：虚拟列表只渲染视口内的行、高度按内容自适应（`max_h` 之外的部分框内滚）、
//! 空态有官方钩子（`render_empty`）。
//!
//! 换 `Table`→`List` 的代价是**名单有状态了**：`ListState` 持有行缓存与滚动位置，
//! 必须跨帧复用（逐帧新建等于每帧把滚动位置清零）。所以 `ListState` 挂在
//! `MainView::ai_wl_list` 上长期持有，而本模块的 `WhitelistDelegate` 只持一份
//! **行快照** —— 对话框每帧把「当前页签 + 搜索过滤后的行」灌进去。名单的事实源
//! 仍只有一份（`ai_cfg.auto_approve` / `BUILTIN_READONLY_COMMANDS`），
//! `List` 里存的是它这一帧的投影，不是副本。
//!
//! 本模块是 `main_view` 的子模块，可直接访问 `MainView` 的私有字段与方法。

use gpui::{
    div, px, App, Context, Entity, InteractiveElement as _, IntoElement, ParentElement as _,
    SharedString, Styled as _, WeakEntity, Window,
};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    dialog::{Dialog, DialogButtonProps},
    h_flex,
    input::Input,
    list::{List, ListDelegate, ListItem, ListState},
    switch::Switch,
    tab::{Tab, TabBar},
    v_flex, IndexPath, Sizable as _, WindowExt as _,
};

use crate::theme::to_hsla as hex;

use super::MainView;

/// 页签下标：自定义白名单（默认页，`ai_wl_tab` 初始值就是它）。
const TAB_CUSTOM: usize = 0;
/// 页签下标：内置白名单（只读清单，只展示不可改）。
const TAB_BUILTIN: usize = 1;

/// 名单区最高高度：条目再多也在框内滚，不把对话框撑出屏幕。
///
/// 写成 `List::max_h(...)` 交给官方组件去落：`List` 会把它挪进内部的
/// `options.max_height`，落到虚拟列表的容器上并改走 `ListSizingBehavior::Infer`
/// （高度＝min(内容, 可用空间)）—— 条目少时贴合内容，条目多时封顶后框内滚。
const LIST_MAX_H: f32 = 260.;

impl MainView {
    /// 打开白名单配置框。
    ///
    /// 直接走 `w.open_dialog` 而**不是** `present_dialog`：后者开头就有
    /// `has_active_dialog` 守卫（挡的是「设置按钮点了没反应」那类重复开窗），
    /// 而这里恰恰要在设置面板**之上**再叠一层。官方对话框层是栈，关掉只弹栈顶，
    /// 设置面板仍留在下面。
    pub(super) fn open_ai_whitelist_dialog(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        // 每次打开都回到「自定义白名单」页：这是用户要配置的东西，内置清单只是
        // 参看（用户明确「默认展示自定义」）。留着上次退出时的页签，下次点开
        // 配置却停在只读清单上，看着像自己的白名单被清空了。
        self.ai_ui.ai_wl_tab = TAB_CUSTOM;
        let view = cx.entity();
        w.open_dialog(cx, move |dialog, w, cx| {
            let view = view.clone();
            view.update(cx, |m, cx| m.render_ai_whitelist_dialog(dialog, w, cx))
        });
        cx.notify();
    }

    fn render_ai_whitelist_dialog(
        &mut self,
        dialog: Dialog,
        _w: &mut Window,
        cx: &mut Context<Self>,
    ) -> Dialog {
        let tok = crate::theme::current(cx);
        // 逐帧现读 `ai_cfg`（与设置面板同一口径）：开关改完 `cx.notify()`，
        // 官方对话框层会把 builder 再跑一遍，这里就拿到新值了。
        let rules = self.ai_ui.ai_cfg.auto_approve.clone();
        let search = self
            .ai_ui
            .ai_wl_search
            .read(cx)
            .value()
            .trim()
            .to_lowercase();
        let add_input = self.ai_ui.ai_cfg_whitelist.clone();
        let tab = self.ai_ui.ai_wl_tab;

        let header = div()
            .text_size(px(14.))
            .font_weight(gpui::FontWeight::BOLD)
            .child(rustshell_core::i18n::t("免确认白名单"));

        // 页签：切换只改 `ai_wl_tab` 一个下标，两份名单都在下面现算 ——
        // 页签不持有任何名单副本，加到一半切过去再切回来，内容仍是新的。
        let tabs = TabBar::new("ai-wl-tabs")
            // 与主标签栏同一变体（Segmented + Small）：本应用里「一组并排的
            // 同名分页」长什么样，用户在这两个地方看到的是同一个东西。
            .segmented()
            .small()
            .selected_index(tab)
            .child(Tab::new().label(rustshell_core::i18n::t("自定义白名单")))
            .child(Tab::new().label(rustshell_core::i18n::t("内置白名单")))
            .on_click(cx.listener(|m, ix: &usize, _w, cx| {
                m.ai_ui.ai_wl_tab = *ix;
                cx.notify();
            }));

        // 说明文案随页签切换：两页讲的是**两套不同**的规则。把「关闭即失效 /
        // 支持 * 通配符」一直贴在内置页上，只会让人以为那批命令也能关。
        let desc = if tab == TAB_BUILTIN {
            rustshell_core::i18n::t(
                "只读命令由安全策略直接执行，本来就无需确认；这里仅列出，不占用白名单。",
            )
        } else {
            rustshell_core::i18n::t(
                "命中的状态变更命令免确认直接执行；关闭某条即该命令失效；支持 * 通配符；批量执行不适用。",
            )
        };

        let mut body = v_flex()
            .w_full()
            .gap_2()
            .child(tabs)
            .child(
                div()
                    .w_full()
                    .text_size(px(11.))
                    .text_color(hex(tok.fg_muted))
                    .child(SharedString::from(desc)),
            )
            // 搜索框两页共用：它问的是「这份名单里有没有这一条」，
            // 换页签时留着查询词正好能接着比。
            .child(Input::new(&self.ai_ui.ai_wl_search).small());

        // 名单本体：当前页签的那一份，先算成**行快照**再交给官方 `List`。
        let (rows, empty_text): (Vec<WlRow>, &'static str) = if tab == TAB_BUILTIN {
            // 内置清单跟着搜索一起筛（搜 `df` 就是想看 `df -h` 在不在）。
            let rows = rustshell_core::ai::BUILTIN_READONLY_COMMANDS
                .iter()
                .filter(|c| wl_search_hit(&search, &c.to_lowercase()))
                .map(|c| WlRow::Builtin {
                    command: (*c).to_string(),
                })
                .collect();
            (rows, rustshell_core::i18n::t("没有匹配的命令"))
        } else {
            // 搜索只影响**显示**，不改变任何一条自身的开关状态。
            let rows = rules
                .iter()
                .filter(|r| wl_search_hit(&search, &r.command.to_lowercase()))
                .map(|r| WlRow::Custom {
                    command: r.command.clone(),
                    enabled: r.enabled,
                })
                .collect();
            let empty = if rules.is_empty() {
                rustshell_core::i18n::t(
                    "还没有自定义免确认命令：内置只读命令本来就无需确认，见「内置白名单」页。",
                )
            } else {
                rustshell_core::i18n::t("没有匹配的命令")
            };
            (rows, empty)
        };
        // 灌快照：`ListState` 跨帧复用（滚动位置与行缓存都在它手里），
        // 每帧把新的投影塞给委托体 —— 它自己不去读 `ai_cfg`，也就没有第二份
        // 状态需要同步。行数/内容变了时虚拟列表会自己把滚动位置钳回范围内。
        self.ai_ui
            .ai_wl_list
            .update(cx, |st, _cx| st.delegate_mut().set_rows(rows, empty_text));
        body = body.child(whitelist_list(&self.ai_ui.ai_wl_list));

        // 添加框只在自定义页签下出现：内置清单加不进来 —— 只读命令本来就不用
        // 确认，把它抄进白名单不会改变任何行为，给入口只会让人以为多做了什么。
        if tab == TAB_CUSTOM {
            // 新增：回车与「添加」走同一条路径（回车由对话框的 `Confirm` 动作送达
            // `on_ok`，见下方 `button_props`），去重后加完即清空。
            body = body.child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Input::new(&add_input).small()),
                    )
                    .child(
                        Button::new("ai-wl-add")
                            .small()
                            .outline()
                            .label(rustshell_core::i18n::t("添加"))
                            .on_click(cx.listener(|m, _e, w, cx| m.ai_wl_add(w, cx))),
                    ),
            );
        }

        let footer = h_flex().justify_end().pt_1().child(
            Button::new("ai-wl-close")
                .small()
                .primary()
                .label(rustshell_core::i18n::t("关闭"))
                .on_click(cx.listener(|m, _e, w, cx| m.close_ai_whitelist_dialog(w, cx))),
        );

        let view_ok = cx.entity();
        dialog
            .w(px(560.))
            .bg(hex(tok.bg_overlay))
            .title(header)
            .child(body.debug_selector(|| "ai-wl-dialog".to_string()))
            .footer(footer)
            .button_props(
                DialogButtonProps::default()
                    // Enter 走官方 `Confirm` 动作（输入框里回车也会 `propagate` 上来），
                    // 这里让它做**加一条**而不是关窗：输入完直接回车是最顺的手势。
                    // 返回 false 表示业务侧不关窗 —— 白名单还要继续加。
                    .on_ok(move |_, w, cx| {
                        view_ok.update(cx, |m, cx| m.ai_wl_add(w, cx));
                        false
                    })
                    // ESC / 右上角 X 只是关掉这一层，设置面板仍开着（栈式关闭）。
                    .on_cancel(|_, _, _| true),
            )
    }

    /// 把添加框里的命令加进白名单（去重、加完清空）。
    ///
    /// 只改 `ai_cfg`、不落盘：白名单是设置面板快照的一部分，落盘仍由面板的「保存」
    /// 负责，取消时整体回滚（与温度滑块同一口径）。命令卡片的「加入白名单」写的是
    /// 同一个字段，两条入口天然一致。
    pub(super) fn ai_wl_add(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        let cmd = self
            .ai_ui
            .ai_cfg_whitelist
            .read(cx)
            .value()
            .trim()
            .to_string();
        if cmd.is_empty() {
            return;
        }
        // 去重：同一条命令出现两次只会在列表里多出一个长得一模一样的行，
        // 而两行的开关可以互相矛盾 —— 那时「这条到底免不免确认」就说不清了。
        match self
            .ai_ui
            .ai_cfg
            .auto_approve
            .iter_mut()
            .find(|r| r.command == cmd)
        {
            // 已存在：顺手打开它。用户重新输入同一条命令的意图多半是「让它生效」，
            // 而不是「再加一条一模一样的」。
            Some(r) => r.enabled = true,
            None => self
                .ai_ui
                .ai_cfg
                .auto_approve
                .push(rustshell_core::ai::WhitelistRule::new(cmd)),
        }
        // 加完即清空：留着一串旧文本会让「刚才到底加进去没有」变成只能靠列表去认。
        self.ai_ui
            .ai_cfg_whitelist
            .update(cx, |st, cx| st.set_value(String::new(), w, cx));
        cx.notify();
    }

    /// 关闭白名单配置框：只弹栈顶那一层，设置面板留在下面继续编辑。
    fn close_ai_whitelist_dialog(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        w.close_dialog(cx);
        cx.notify();
    }
}

/// 白名单框搜索框的命中判定（`query` 已小写、已 trim，`cmd_lower` 已小写）。
///
/// 带 `*` 时走白名单**同一套**通配语义（用户敲 `systemctl *` 就是想要那批命令），
/// 不带 `*` 时按子串匹配 —— 敲 `systemctl` 就该看到所有含它的条目，
/// 若一律走通配，这种最自然的用法反而什么都筛不出来。
fn wl_search_hit(query: &str, cmd_lower: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    if query.contains('*') {
        rustshell_core::ai::wildcard_match(query, cmd_lower)
    } else {
        cmd_lower.contains(query)
    }
}

/// 名单里的一行（**快照**，见 `WhitelistDelegate`）。
///
/// 与 `WhitelistDelegate::set_rows` 同为 `pub(super)`：行的构造只发生在
/// 本模块（对话框现算快照那一段），可见性对齐才不至于「方法能看见、行的类型看不见」。
pub(super) enum WlRow {
    /// 自定义条目：可开关、可删除（对应 `ai_cfg.auto_approve` 里的一员）。
    Custom { command: String, enabled: bool },
    /// 内置只读条目：只展示。
    Builtin { command: String },
}

/// 白名单名单的 `ListDelegate`：只持一份**行快照** + 一个回写用的弱引用。
///
/// 委托体里**不能**读写 `MainView`：`render_item` 是在 `ListState` 的 render 里
/// 跑的，此刻它正被 `state.update(...)` 锁着，回头再 `update` 它一次就是重入
/// panic（gpui 的实体锁不做重入）。所以行里的一切都取自快照；动作（开关 / 删除）
/// 走 `me.update(...)` 落到 `MainView` 上，由下一帧的快照体现回来 —— 名单的事实源
/// 始终是 `ai_cfg.auto_approve`，委托体里没有第二份可写状态。
///
/// `me` 是**弱**引用：`MainView` → `ListState` → 委托体 → `MainView` 是一条环，
/// 持强引用就是内存泄漏（与 `UiHub` 同一口径）。
pub(super) struct WhitelistDelegate {
    rows: Vec<WlRow>,
    empty_text: SharedString,
    me: WeakEntity<MainView>,
}

impl WhitelistDelegate {
    pub(super) fn new(me: WeakEntity<MainView>) -> Self {
        Self {
            rows: Vec::new(),
            empty_text: SharedString::default(),
            me,
        }
    }

    /// 重灌这一帧要显示的行（列表内容的事实源仍是 `ai_cfg`，这里只是投影）。
    ///
    /// `ListState` 不感知行内容变化，只认 `items_count` 与缓存的测量值，
    /// 所以每次都得整个换来 —— 追加式的增量更新一旦漏掉删除/过滤，
    /// 列表里就会留下一行已经不在白名单里的「幽灵命令」。
    pub(super) fn set_rows(&mut self, rows: Vec<WlRow>, empty_text: impl Into<SharedString>) {
        self.rows = rows;
        self.empty_text = empty_text.into();
    }
}

impl ListDelegate for WhitelistDelegate {
    type Item = ListItem;

    fn items_count(&self, _section: usize, _cx: &App) -> usize {
        self.rows.len()
    }

    /// 名单是「逐行开关 + 删除」的编辑面，没有「选中某一行」的语义：
    /// `ListState` 建的时候就关了 `selectable`（点了也行不成选中态）。
    /// 这个方法只是补上 trait 的要求。
    fn set_selected_index(
        &mut self,
        _ix: Option<IndexPath>,
        _w: &mut Window,
        _cx: &mut Context<ListState<Self>>,
    ) {
    }

    fn render_item(
        &mut self,
        ix: IndexPath,
        _w: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> Option<ListItem> {
        Some(match self.rows.get(ix.row)? {
            WlRow::Custom { command, enabled } => {
                wl_custom_row(ix.row, command, *enabled, &self.me, cx)
            }
            WlRow::Builtin { command } => wl_builtin_row(ix.row, command, cx),
        })
    }

    fn render_empty(
        &mut self,
        _w: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> impl IntoElement {
        // 空态要说话：列表空着而不解释，用户会以为是没加载出来。两种空态
        // （一条都没有 / 被搜索筛没了）的文案由 `set_rows` 带进来。
        let tok = crate::theme::current(cx);
        v_flex()
            .w_full()
            .px_3()
            .py_2()
            .text_size(px(11.))
            .text_color(hex(tok.fg_muted))
            .child(self.empty_text.clone())
    }
}

/// 名单本体：官方 `List`（虚拟列表 + 自带滚动条），最多 `LIST_MAX_H` 高。
///
/// 限高不做在自绘的外壳上、而是交给 `List::max_h(...)`：它会把上限转成内部虚拟
/// 列表的 `max_height` 并改走 `ListSizingBehavior::Infer`，条目少时贴合内容、
/// 条目多时封顶后框内滚。外层只留一个 `debug_selector` 钩子（`ai-wl-box`），
/// 给几何护栏量真实高度 —— 源码里写了 `max_h` 不等于它真的限住了，
/// 见 `the_list_is_height_capped_and_really_scrolls`。
fn whitelist_list(state: &Entity<ListState<WhitelistDelegate>>) -> impl IntoElement {
    v_flex()
        .w_full()
        .debug_selector(|| "ai-wl-box".to_string())
        .child(List::new(state).max_h(px(LIST_MAX_H)))
}

/// 自定义名单里的一行：命令原文 + 开关 + 删除。
///
/// 元素 id 用**下标**、改动用**值**：id 只要稳定不重复即可（命令原文里带引号
/// 或空格时拼进 id 是自找麻烦），而开关与删除按值定位 —— 渲染闭包捕获的下标
/// 在连点两下或别处改了白名单之后就失效了，改错/删错一行比改不掉严重得多。
fn wl_custom_row(
    ix: usize,
    cmd: &str,
    enabled: bool,
    me: &WeakEntity<MainView>,
    cx: &mut Context<ListState<WhitelistDelegate>>,
) -> ListItem {
    let tok = crate::theme::current(cx);
    let for_switch = cmd.to_string();
    let for_delete = cmd.to_string();
    ListItem::new(SharedString::from(format!("ai-wl-row-{ix}"))).child(
        h_flex()
            .w_full()
            .items_center()
            .gap_2()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(12.))
                    .text_ellipsis()
                    .text_color(if enabled {
                        hex(tok.fg_primary)
                    } else {
                        // 关掉的行压暗：一眼能看出「这条现在不生效」
                        hex(tok.fg_muted)
                    })
                    .child(SharedString::from(cmd.to_string())),
            )
            .child(
                // 外套一层只为了 `debug_selector`：`Switch` 不实现
                // `InteractiveElement`，`clicking_row_controls_edit_the_live_whitelist`
                // 要靠这个钩子量到开关的位置真点一次（release 下是空转）。
                div().debug_selector(|| format!("ai-wl-sw-{ix}")).child(
                    Switch::new(SharedString::from(format!("ai-wl-sw-{ix}")))
                        .small()
                        .checked(enabled)
                        .on_click({
                            let me = me.clone();
                            move |checked: &bool, _w, cx| {
                                let checked = *checked;
                                // 落点经 `me.update` 回 `MainView`：名单的事实源在
                                // `ai_cfg.auto_approve`，这里不另存一份。
                                let _ = me.update(cx, |m, cx| {
                                    if let Some(r) = m
                                        .ai_ui
                                        .ai_cfg
                                        .auto_approve
                                        .iter_mut()
                                        .find(|r| r.command == for_switch)
                                    {
                                        r.enabled = checked;
                                    }
                                    cx.notify();
                                });
                            }
                        }),
                ),
            )
            .child(
                Button::new(SharedString::from(format!("ai-wl-del-{ix}")))
                    .small()
                    .ghost()
                    .label(rustshell_core::i18n::t("删除"))
                    // 同开关：给点击测试留个量坐标的钩子。
                    .debug_selector(|| format!("ai-wl-del-{ix}"))
                    .on_click({
                        let me = me.clone();
                        move |_e, _w, cx| {
                            let _ = me.update(cx, |m, cx| {
                                m.ai_ui
                                    .ai_cfg
                                    .auto_approve
                                    .retain(|r| r.command != for_delete);
                                cx.notify();
                            });
                        }
                    }),
            ),
    )
}

/// 内置只读清单里的一行：**只有**命令原文，没有任何控件。
///
/// 这一页的行没有开关、也没有删除，更不会写进 `ai_config.json`：只读命令在
/// `agent::judge` 里走 `Risk::Safe` 直接执行，白名单加不加它们都一样 ——
/// 给一个「关掉」的开关、关掉却什么都不变，那是在骗人。
fn wl_builtin_row(
    ix: usize,
    cmd: &str,
    cx: &mut Context<ListState<WhitelistDelegate>>,
) -> ListItem {
    let tok = crate::theme::current(cx);
    ListItem::new(SharedString::from(format!("ai-wl-row-{ix}")))
        // `.disabled(true)` 不只是「不给点」：行文本会跟着压暗（`ListItem` 对
        // 不可选行用 muted 色），与自定义页里关掉的那条看上去同一种「不生效」。
        .disabled(true)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_size(px(12.))
                .text_ellipsis()
                .text_color(hex(tok.fg_muted))
                .child(SharedString::from(cmd.to_string())),
        )
}

/// 白名单配置框的护栏（设置 - Agent 页「配置」按钮弹出来的那一层）。
///
/// 内联在设置页上的编辑器已在 2026-09-22 撤掉，本框是白名单的**唯一**编辑入口，
/// 于是「列表能不能读、四个动作用户点不点得到」全押在这一个文件上。护栏按
/// 动作分条钉住，且一律读 `prod()` 后的生产源码（针写在测试里，不切会自己命中
/// 自己 —— 见 `test_support` 的模块文档）。
#[cfg(test)]
mod tests {
    use super::{wl_search_hit, WlRow, LIST_MAX_H, TAB_BUILTIN, TAB_CUSTOM};
    use crate::test_support::prod;
    use gpui::px;

    fn production_source() -> String {
        prod(include_str!("whitelist_dialog.rs"))
    }

    /// 生产源码里有没有这么一段代码（**两侧空白一律忽略**）。
    ///
    /// `prod()` 保留换行，而 rustfmt 对长文案与长链式调用会折行 —— `t("…")`
    /// 会变成 `t(\n "…",\n)`、`.push(x)` 会变成换行后的 `.push(\n x,\n)`，
    /// 逐字 `contains` 在折行后一律假红（本模块第一次跑就踩到三条）。
    /// 负向断言仍走裸 `contains`：`has` 更宽松，拿它做「不得出现」会变松。
    fn has(code: &str, snippet: &str) -> bool {
        fn flat(s: &str) -> String {
            s.chars().filter(|c| !c.is_whitespace()).collect()
        }
        flat(code).contains(&flat(snippet))
    }

    /// `has` 的 i18n 版：断言某句文案走了 `i18n::t(...)`，而不是裸字符串。
    ///
    /// 末端**收在引号处、不带 `)`**：长文案被 rustfmt 折行后是 `t(\n "…",\n)`，
    /// 压掉空白即 `t("…",)` —— 拿 `t("…")` 当针会差在一个逗号上（本模块踩过）。
    fn has_t(code: &str, key: &str) -> bool {
        has(code, &format!("t(\"{key}\""))
    }

    /// 入口与叠层：白名单框必须**叠在设置面板之上**开一层，关掉只弹栈顶。
    ///
    /// 走 `present_dialog` 就废了：它开头的 `has_active_dialog` 守卫会挡掉
    /// 「设置面板开着」这个唯一场景（表现为「点了配置没反应」）。
    #[test]
    fn dialog_stacks_on_top_of_the_settings_panel() {
        let code = production_source();
        let at = code
            .find("fn open_ai_whitelist_dialog")
            .expect("白名单框必须由这个方法打开");
        let tail = &code[at..];
        let open = &tail[..tail
            .find("fn render_ai_whitelist_dialog")
            .expect("打开方法之后应当是渲染方法")];
        assert!(
            has(open, "w.open_dialog("),
            "白名单框要走官方对话框栈（`WindowExt::open_dialog`），否则叠不到设置面板之上。"
        );
        assert!(
            !open.contains("present_dialog"),
            "白名单框不能走 `present_dialog`：它的 `has_active_dialog` 守卫会把\
             「设置面板开着时点配置」这个唯一入口挡掉。"
        );

        let close_at = code
            .find("fn close_ai_whitelist_dialog")
            .expect("必须有关闭方法");
        assert!(
            has(&code[close_at..], "w.close_dialog(cx)"),
            "关闭白名单框必须只弹栈顶一层（`w.close_dialog`），设置面板留在下面。"
        );
    }

    /// 读：逐帧现读 `ai_cfg.auto_approve`，并逐帧灌进 `List` 的委托体。
    ///
    /// 官方对话框层每帧重跑 builder，现读才能让「点开关 → 立即变色」成立；
    /// 若在打开时把列表拷进某个字段，改完就得手动重开一次框才看得到。
    ///
    /// 换成 `List` 之后这条多一半：名单有状态了（`ListState` 跨帧复用），
    /// 逐帧现读还不够 —— 还得每帧把现读的结果**灌进委托体**，否则列表停留在
    /// 第一次灌进去的那份快照上，点开关同样不变色。
    #[test]
    fn dialog_reads_the_live_whitelist_every_frame() {
        let code = production_source();
        assert!(
            has(&code, "self.ai_ui.ai_cfg.auto_approve.clone()"),
            "列表必须逐帧现读 `ai_cfg.auto_approve`，否则开关点了不变、加了看不到。"
        );
        assert!(
            has(&code, "st.delegate_mut().set_rows(rows, empty_text)"),
            "每帧必须把现算的行灌进 `List` 的委托体（`set_rows`）：`ListState` 跨帧\
             复用，不灌就永远停在打开时那一份快照上。"
        );
    }

    /// 页签：两个页签、Segmented 变体、**默认落在自定义页**。
    ///
    /// 「默认页」有两处：`MainView::ai_wl_tab` 的初值，以及每次开框时的复位
    /// （`open_ai_whitelist_dialog` 里写回 `TAB_CUSTOM`）。常量与初值分家，
    /// 默认页就会悄悄变成内置页 —— 用户点开配置想看的是自己那份白名单，
    /// 停在只读清单上会以为它被清空了。
    #[test]
    fn tabs_split_custom_and_builtin_with_custom_by_default() {
        assert_eq!(TAB_CUSTOM, 0, "自定义页必须是第 0 个页签");
        assert_eq!(TAB_BUILTIN, 1, "内置页必须是第 1 个页签");

        let main = prod(include_str!("../main_view.rs"));
        assert!(
            has(&main, "ai_wl_tab: 0,"),
            "`MainView::ai_wl_tab` 的初值必须是 0（自定义白名单）。"
        );

        let code = production_source();
        assert!(
            has(&code, "TabBar::new(\"ai-wl-tabs\")"),
            "两个页签必须走官方 `TabBar`（自绘一套会在主题/尺寸上与主标签栏分家）。"
        );
        assert!(
            has(&code, ".segmented()"),
            "页签用 Segmented 变体：与主标签栏同一个样子。"
        );
        for label in ["自定义白名单", "内置白名单"] {
            assert!(
                has_t(&code, label),
                "页签少了「{label}」—— 两个页签的文案与用户嘴里的名字要一致。"
            );
        }
        assert!(
            has(&code, ".selected_index(tab)"),
            "页签的选中态必须跟着 `ai_wl_tab` 走，否则点了不换页。"
        );
        assert!(
            has(&code, "m.ai_ui.ai_wl_tab = *ix;"),
            "点击页签必须把下标写回 `ai_wl_tab`（纯界面状态，关掉弹窗即失效）。"
        );

        // 开框即复位到自定义页（切页签 → 关掉 → 再点开，仍应从自定义页开始）
        let at = code
            .find("fn open_ai_whitelist_dialog")
            .expect("白名单框必须由这个方法打开");
        let tail = &code[at..];
        let open = &tail[..tail
            .find("fn render_ai_whitelist_dialog")
            .expect("打开方法之后应当是渲染方法")];
        assert!(
            has(open, "self.ai_ui.ai_wl_tab = TAB_CUSTOM;"),
            "每次开框都要复位到自定义页：留着上次退出时的页签，用户下次点开配置\
             会停在只读清单上，看着像自己的白名单被清空了。"
        );
    }

    /// 名单本体：官方 `List` 组件（不是自绘表格），两份名单共用同一套渲染。
    ///
    /// 2026-09-23 用户要求「白名单命令改为 List 组件」：`Table` 是静态表格语义
    /// （行数再多也整份渲染），长名单该交给 `List` —— 虚拟列表 + 自带滚动条 +
    /// 官方空态钩子。这里连「不得再出现 `Table`」一起钉住：两种写法共存时，
    /// 改了一份忘了另一份是必然的。
    #[test]
    fn both_lists_render_with_the_official_list_component() {
        let code = production_source();
        for id in ["ai-wl-box", "ai-wl-add", "ai-wl-close"] {
            assert!(
                code.contains(id),
                "白名单框少了 `{id}`（名单盒子 / 添加按钮 / 关闭按钮）。"
            );
        }
        for needle in [
            "List::new(state)",
            "impl ListDelegate for WhitelistDelegate",
            "ListState<WhitelistDelegate>",
            "ListItem::new(",
            "fn render_item(",
            "fn render_empty(",
        ] {
            assert!(
                has(&code, needle),
                "名单必须用官方 `List` 组件渲染，这里缺 `{needle}`。"
            );
        }
        assert!(
            has(&code, "List::new(state).max_h(px(LIST_MAX_H))"),
            "限高要交给 `List::max_h(...)`（内部转 `Infer` 尺寸行为）：自绘外壳限高\
             只能裁掉内容，滚不动。"
        );
        // 旧实现（Table + 三层滚动外壳）必须清干净：留着 `Table` 的导入或自绘
        // 滚动层，下次改样式就会只改中一半。
        for gone in [
            "Table",
            "table::",
            "overflow_y_scrollbar",
            "scrollable_table",
            "ai-wl-rows",
        ] {
            assert!(
                !code.contains(gone),
                "名单已改用官方 `List`，源码里仍残留旧表格实现（命中 `{gone}`）。"
            );
        }

        for empty in [
            "还没有自定义免确认命令：内置只读命令本来就无需确认，见「内置白名单」页。",
            "没有匹配的命令",
        ] {
            assert!(
                has_t(&code, empty),
                "空态少了「{empty}」—— 列表空着而不说话，用户会以为是没加载出来。"
            );
        }
    }

    /// 动作一：搜索只筛**显示**，一个字都不改白名单。
    #[test]
    fn search_only_filters_the_visible_rows() {
        let code = production_source();
        let at = code
            .find("let (rows, empty_text): (Vec<WlRow>, &'static str) = if tab == TAB_BUILTIN {")
            .expect("必须有一段算出「这一帧要显示哪些行」的代码");
        let tail = &code[at..];
        let filter = &tail[..tail
            .find("self.ai_ui.ai_wl_list")
            .expect("行快照算完之后应当灌给 `List`")];
        assert!(
            has(filter, "wl_search_hit("),
            "搜索必须走 `wl_search_hit`（子串 + `*` 两种语义），不要另写一套。"
        );
        for write in ["iter_mut", "retain", "push(", "enabled =", "auto_approve"] {
            assert!(
                !filter.contains(write),
                "搜索是纯筛显示：这段只许读白名单，不得动任何一条的状态（命中 `{write}`）。"
            );
        }
        assert!(
            has(&code, "rustshell_core::ai::wildcard_match("),
            "搜索里的 `*` 必须复用 core 白名单那套通配匹配 —— 两处语义要一模一样。"
        );
        assert!(
            has(&code, "cmd_lower.contains(query)"),
            "不带 `*` 时按子串匹配：敲 `systemctl` 就该筛出所有含它的条目。"
        );
    }

    /// 搜索匹配本身：子串、`*`、空查询三种情形。
    #[test]
    fn search_hit_handles_substring_and_star() {
        assert!(wl_search_hit("", "rm -rf /tmp/x"), "空查询＝全部可见");
        assert!(wl_search_hit("restart", "systemctl restart nginx"));
        assert!(!wl_search_hit("stop", "systemctl restart nginx"));
        // 带 `*` 走通配（与白名单同语义）；不带 `*` 不会被当成通配
        assert!(wl_search_hit("systemctl *", "systemctl restart nginx"));
        assert!(!wl_search_hit("df *", "systemctl restart nginx"));
    }

    /// 动作二：逐条开关按**值**定位、只改 `enabled`，且落点经弱引用回 `MainView`。
    ///
    /// 按下标定位会在连点两下或别处改过白名单之后错行 —— 改错一行比改不掉严重。
    /// 弱引用那条是给内存上的：`MainView` → `ListState` → 委托体 → `MainView`
    /// 是一条环，持强引用就是泄漏（行闭包每帧重建，泄漏会随帧数涨）。
    #[test]
    fn switch_toggles_by_value_not_by_index() {
        let code = production_source();
        assert!(
            has(&code, "r.enabled = checked;"),
            "开关必须把勾选态写回规则本体（`r.enabled = checked`）。"
        );
        assert!(
            has(&code, "find(|r| r.command == for_switch)"),
            "开关必须按**命令原文**定位那一条，不能拿渲染时的下标去索引。"
        );
        assert!(
            has(&code, "me: WeakEntity<MainView>"),
            "委托体只能持 `MainView` 的**弱**引用：强引用会让 \
             `MainView → ListState → 委托体 → MainView` 成环，泄漏一份视图。"
        );
        assert!(
            has(&code, "me.update(cx,"),
            "行内动作必须经 `me.update(...)` 回主视图（委托体里没有第二份可写状态）。"
        );
    }

    /// 动作三：新增 —— 去重、默认开启、加完清空；且只在自定义页出现。
    #[test]
    fn add_appends_dedupes_and_clears_the_input() {
        let code = production_source();
        assert!(
            has(&code, "auto_approve.push("),
            "新增必须写进 `ai_cfg.auto_approve`（否则保存时什么都不落盘）。"
        );
        assert!(
            has(&code, "WhitelistRule::new(cmd)"),
            "新增必须走 `WhitelistRule::new` —— 默认开启就写在这一个地方（core 侧有测试钉着）。"
        );
        assert!(
            has(&code, "r.enabled = true"),
            "重新输入已存在的条目时应当**开启**它：用户多半是想让它生效，不是想再加一条一样的。"
        );
        assert!(
            has(&code, "set_value(String::new()"),
            "加完必须清空输入框，否则「刚才到底加进去没有」只能靠列表去认。"
        );

        // 添加框只在自定义页签下渲染：内置清单加不进来。
        let at = code.find("if tab == TAB_CUSTOM {").expect(
            "添加框必须由页签门控（`if tab == TAB_CUSTOM`）—— 内置页摆一个添加框，\
             等于告诉用户「这批只读命令也能加进白名单」",
        );
        let tail = &code[at..];
        let add = &tail[..tail
            .find(".debug_selector(")
            .expect("添加框之后应当收尾进对话框")];
        assert!(
            has(add, "ai-wl-add"),
            "「添加」按钮必须在自定义页签那一支里渲染。"
        );
    }

    /// 动作四：删除按值 `retain`，以及「回车＝加一条」。
    #[test]
    fn delete_and_enter_paths_are_wired() {
        let code = production_source();
        assert!(
            has(&code, "auto_approve.retain("),
            "删除必须能删掉条目，否则加错了只能去手改 ai_config.json。"
        );
        assert!(
            has(&code, "r.command != for_delete"),
            "删除必须按**命令原文**定位，不能按下标（同开关那条）。"
        );

        // Enter 走官方 `Confirm` 动作；这里让它做「加一条」并返回 false 不关窗 ——
        // 输入完直接回车是最顺的手势，加一条就关窗等于每加一条都要重开一次。
        let at = code
            .find("DialogButtonProps::default()")
            .expect("白名单框必须给对话框配上按钮行为");
        let props = &code[at..];
        assert!(
            has(props, "m.ai_wl_add(w, cx)); false"),
            "回车（`on_ok`）必须走**和「添加」按钮同一条**路径（`ai_wl_add`），\
             且返回 false 不关窗 —— 白名单往往要连着加几条，加一条就关窗等于每加一条都要重开。"
        );
        assert!(
            props.contains(".on_cancel("),
            "ESC / 右上角 X 要走取消：只关这一层，设置面板继续编辑。"
        );
    }

    /// 说明文案必须点到「通配符」，且只贴在自定义页上。
    ///
    /// 白名单命中的是**整条**命令，用户不可能从界面上猜出 `*` 这件事；
    /// 这句话是唯一的说明书。内置页讲的是另一套规则（那批命令压根不用确认），
    /// 文案就得跟着页签换 —— 两页共用一句会把「关闭即失效」也扣到只读命令头上。
    #[test]
    fn the_description_follows_the_active_tab() {
        let code = production_source();
        assert!(
            has_t(
                &code,
                "命中的状态变更命令免确认直接执行；关闭某条即该命令失效；支持 * 通配符；批量执行不适用。"
            ),
            "白名单框必须把「整条命中、关闭即失效、支持 *、批量不适用」四件事讲清楚。"
        );
        assert!(
            has(&code, "let desc = if tab == TAB_BUILTIN {"),
            "说明文案必须按页签分岔（`if tab == TAB_BUILTIN`）：两页讲的是两套规则。"
        );
    }

    /// 内置只读清单：要展示，但**只展示**，且独占一页。
    ///
    /// 代码里本来就没有默认白名单命令集（`AiConfig::default().auto_approve` 是空的），
    /// 而只读命令在 `judge` 里走 `Risk::Safe` 直接执行 —— 用户想看的「代码里的默认
    /// 白名单」就是这一批。给它开关会骗人：关掉一条只读命令，行为不会有任何变化。
    #[test]
    fn builtin_readonly_commands_are_listed_without_controls() {
        let code = production_source();
        assert!(
            has(&code, "rustshell_core::ai::BUILTIN_READONLY_COMMANDS"),
            "白名单框必须展示代码里的内置只读命令清单（用户点开配置就是想看到这批）。"
        );
        assert!(
            has(&code, "wl_search_hit(&search, &c.to_lowercase())"),
            "内置清单也要跟着搜索框筛：搜 `df` 却看不到 `df -h` 会让人以为它不在。"
        );
        assert!(
            has(&code, "wl_builtin_row(ix.row, command, cx)"),
            "内置清单必须走 `wl_builtin_row`（护栏要能只盯那一段的渲染体）。"
        );

        // 只读区渲染体：不得出现任何控件，也不得读写 ai_cfg，更没有回写通道
        // （切片末端用**函数签名**：`prod()` 会把注释剥掉，拿文档注释当锚点会切到文件尾）
        let at = code
            .find("fn wl_builtin_row(")
            .expect("内置清单必须是独立的一段（否则护栏没法只盯它）");
        let tail = &code[at..];
        let body = &tail[..tail.find("\nfn ").unwrap_or(tail.len())];
        // 内置页的说明文案是**页签那一支**里的常量（不在行函数里），
        // 所以这条针扎在 `let desc = if tab == TAB_BUILTIN {` 到 `} else {` 之间：
        // 只断言「这句在文件里」会让它哪天被挪到自定义页也照样绿。
        let desc_at = code
            .find("let desc = if tab == TAB_BUILTIN {")
            .expect("说明文案必须按页签分岔");
        let desc_tail = &code[desc_at..];
        let builtin_desc = &desc_tail[..desc_tail
            .find("} else {")
            .expect("分岔必须有 else 支（自定义页那句）")];
        assert!(
            has_t(
                builtin_desc,
                "只读命令由安全策略直接执行，本来就无需确认；这里仅列出，不占用白名单。"
            ),
            "内置页必须讲清「为什么不给开关」—— 只展示、不占用白名单。"
        );
        assert!(
            has(body, "ListItem::new(") && has(body, ".disabled(true)"),
            "内置清单也要用官方 `ListItem` 渲染，并显式 `disabled`（不给点、文本压暗）。"
        );
        for forbidden in [
            "Switch::new",
            ".retain(",
            "ai-wl-del",
            "ai-wl-add",
            "auto_approve",
            "WhitelistRule",
            "WeakEntity",
            "on_click",
        ] {
            assert!(
                !body.contains(forbidden),
                "内置只读命令不可关、不可删、不进配置（命中 `{forbidden}`）：\
                 关掉一条只读命令不会有任何行为变化，开关只是骗人。"
            );
        }
    }

    /// 等对话框**进场动画真的跑完**，再返回 `sel` 此刻的窗口坐标。
    ///
    /// 这是本仓测试里唯一能把「渲染坐标」与「点击命中」对齐的口径，原因是三件事叠在一起：
    ///
    /// 1. 进场动画（官方 `Dialog` 的 `ANIMATION_DURATION` = 250ms 缓动）让对话框的位置
    ///    随**时间**变；
    /// 2. `run_until_parked()` 只 `tick` 调度器、**不推虚拟时钟**（`TestScheduler::run`
    ///    就是 `while step() {}`，与 `dead_session` 里那条注释同源）⇒ 动画在测试里可以
    ///    冻在任意一帧：「连续两次读数一样」看起来**正像**已经稳定，其实只是时钟没走。
    ///    老的 `settled`（只比连续两帧）就栽在这里 —— 它第一次比较就相等并返回；
    /// 3. `debug_bounds` 给的是**上一帧画出来的**坐标，而 `simulate_click` 派发前会按
    ///    **当前**动画时刻重排 ⇒ 点击落在一个已经挪走的盒子上。
    ///
    /// 症状极具欺骗性：控件明明渲染出来了、`debug_bounds` 量得到、点了却毫无反应
    /// （`clicking_row_controls_edits_the_live_whitelist` 最初的红就是这个，与
    /// 「行内动作没接上主视图」长得一模一样）。实测该用例里盒子停在 y=129、真实位置
    /// 已是 y=237，差 108px —— 点击自然落空。
    ///
    /// 所以这里是「**推一次时钟 + 强制出一帧**，位置仍不变」才算稳定：
    /// `advance_clock` 推进动画，`window.draw(cx)` 让它落到 `debug_bounds` 上。
    ///
    /// 另一条用例 `the_list_is_height_capped_and_really_scrolls` 为了绕开同一问题，
    /// 只比「内容相对盒子」的位移；有了本函数，绝对坐标也可用了。
    fn settled_after_entrance(
        vcx: &mut gpui::VisualTestContext,
        sel: &'static str,
    ) -> gpui::Bounds<gpui::Pixels> {
        let mut prev = None;
        for _ in 0..80 {
            vcx.executor()
                .advance_clock(std::time::Duration::from_millis(25));
            vcx.update(|window, cx| {
                window.draw(cx).clear(cx);
            });
            vcx.run_until_parked();
            let now = vcx
                .debug_bounds(sel)
                .unwrap_or_else(|| panic!("`{sel}` 没渲染出来"));
            if prev == Some(now.origin) {
                return now;
            }
            prev = Some(now.origin);
        }
        panic!("`{sel}` 推了 2s 虚拟时钟还在动：它挂的动画不是有限时长的？");
    }

    /// 行内动作的**点击链路**：委托体里渲染出来的开关与删除，真能改到白名单。
    ///
    /// 换 `List` 之后唯一「源码看着对、跑起来未必对」的地方就是这里：行控件不再由
    /// `MainView` 直接渲染（老实现是 `cx.listener` 那条熟路），而是在 `List` 的委托体
    /// 里现搭，动作只能经**弱引用**回主视图。源码断言看不出这条链路通不通，所以这里
    /// 真点一次：开关翻的必须是**那一条**、删除删的也必须是**那一条**，别的条目一动
    /// 不许动（按下标定位的典型症状正是「点第 2 条改了第 1 条」）。
    #[gpui::test]
    fn clicking_row_controls_edits_the_live_whitelist(cx: &mut gpui::TestAppContext) {
        use crate::main_view::dialogs::tests::build_main;

        let (main, vcx) = build_main(cx);
        main.update_in(&mut vcx.clone(), |m, window, cx| {
            m.ai_ui.ai_cfg.auto_approve = vec![
                rustshell_core::ai::WhitelistRule::new("servicectl restart alpha"),
                rustshell_core::ai::WhitelistRule::new("servicectl restart beta"),
            ];
            // 第 2 条先关掉：点它前后都有对照。
            m.ai_ui.ai_cfg.auto_approve[1].enabled = false;
            m.open_ai_whitelist_dialog(window, cx);
        });
        vcx.run_until_parked();

        // 量坐标前必须先把**进场动画**推完（口径见 [`settled_after_entrance`]）：
        // 否则量到的是「上一帧画出来的」位置，而 `simulate_click` 会按**当前**动画
        // 时刻重排，点击就落在一个已经挪走的盒子上。
        let states = |vcx: &mut gpui::VisualTestContext| -> (bool, bool) {
            main.read_with(vcx, |m, _| {
                (
                    m.ai_ui.ai_cfg.auto_approve[0].enabled,
                    m.ai_ui.ai_cfg.auto_approve[1].enabled,
                )
            })
        };

        // ① 点第 2 条的开关（它是关着的那条）：只有它翻过来
        let sw = settled_after_entrance(vcx, "ai-wl-sw-1");
        vcx.simulate_click(sw.center(), Default::default());
        vcx.run_until_parked();
        assert_eq!(
            states(vcx),
            (true, true),
            "点第 2 条的开关之后状态不对：要么没落回 `ai_cfg.auto_approve`（行内动作\
             没接到主视图上），要么落到第 1 条上了（按下标定位的错行）。"
        );

        // ② 再点一次翻回去：开关是双态的，不是「点一下就锁成打开」
        let sw = settled_after_entrance(vcx, "ai-wl-sw-1");
        vcx.simulate_click(sw.center(), Default::default());
        vcx.run_until_parked();
        assert_eq!(states(vcx), (true, false), "开关只能打开、关不回去？");

        // ③ 点第 2 条的删除：那条没了、第 1 条还在
        let del = settled_after_entrance(vcx, "ai-wl-del-1");
        vcx.simulate_click(del.center(), Default::default());
        vcx.run_until_parked();
        let left = main.read_with(vcx, |m, _| {
            m.ai_ui
                .ai_cfg
                .auto_approve
                .iter()
                .map(|r| r.command.clone())
                .collect::<Vec<_>>()
        });
        assert_eq!(
            left,
            vec!["servicectl restart alpha".to_string()],
            "删除删错了行：剩下的白名单不对。"
        );

        // ④ 删完那一行也得从列表里消失：委托体的行快照必须逐帧跟着 `ai_cfg` 走，
        //    否则界面上会留下一条已经不在白名单里的「幽灵命令」（点它还报错）。
        assert!(
            vcx.debug_bounds("ai-wl-del-1").is_none(),
            "删除后列表里还留着那一行：灌给 `List` 的行快照没跟着刷新。"
        );
    }

    /// 名单再长也必须**限高且真的能滚**（2026-09-22 用户要求「可滚动」，
    /// 2026-09-23 换成官方 `List` 后这条更要盯 —— `List` 的滚动是本组件内部的
    /// 虚拟列表，自绘外壳完全看不见它滚没滚）。
    ///
    /// 源码断言只能证明「写了 `max_h`」，证明不了它真的限住了、更证明不了滚得动。
    /// 这里造 20 条白名单，量盒子的**真实高度**（限高），再真滚一次滚轮看
    /// `ListState` 的滚动偏移有没有动（滚动条背后就是它）。切页签那一步再回头
    /// 读委托体里的行，确认换的是**内容**而不是同一份快照。
    ///
    /// 几何探针复用 `dialogs` 的 `build_main`：对话层（`Root` + `AppShell`）
    /// 必须真挂上，`open_dialog` 叠出来的那一层才有人渲染。
    #[gpui::test]
    fn the_list_is_height_capped_and_really_scrolls(cx: &mut gpui::TestAppContext) {
        use crate::main_view::dialogs::tests::build_main;

        let (main, vcx) = build_main(cx);
        let many = |n: usize| -> Vec<rustshell_core::ai::WhitelistRule> {
            (0..n)
                .map(|i| {
                    rustshell_core::ai::WhitelistRule::new(format!("servicectl restart app-{i:02}"))
                })
                .collect()
        };
        main.update_in(&mut vcx.clone(), |m, window, cx| {
            m.ai_ui.ai_cfg.auto_approve = many(20);
            m.open_ai_whitelist_dialog(window, cx);
        });
        vcx.run_until_parked();

        let box_h = |vcx: &mut gpui::VisualTestContext| -> f32 {
            f32::from(
                vcx.debug_bounds("ai-wl-box")
                    .expect("名单盒子没渲染出来（`ai-wl-box`）")
                    .size
                    .height,
            )
        };
        // 滚动偏移直接读 `ListState` 手里的滚动句柄：这才是「用户能不能滚」的
        // 事实源（内容相对盒子的位移只是它的结果，盒子进场动画期间还会自己漂）。
        let scroll_y = |vcx: &mut gpui::VisualTestContext| -> f32 {
            main.read_with(vcx, |m, cx| {
                f32::from(m.ai_ui.ai_wl_list.read(cx).scroll_handle().offset().y)
            })
        };
        let rows_are_builtin = |vcx: &mut gpui::VisualTestContext| -> Vec<bool> {
            main.read_with(vcx, |m, cx| {
                m.ai_ui
                    .ai_wl_list
                    .read(cx)
                    .delegate()
                    .rows
                    .iter()
                    .map(|r| matches!(r, WlRow::Builtin { .. }))
                    .collect()
            })
        };

        // ① 限高：20 条撑不进 260px
        let h = box_h(vcx);
        assert!(
            h <= LIST_MAX_H + 2.,
            "20 条白名单把名单撑到 {h}px（上限 {LIST_MAX_H}）：`List` 没有限高，\
             条目一多就会把对话框顶出屏幕"
        );
        assert!(h > 60., "名单只有 {h}px 高：列表塌了，一行都点不到");

        // ② 真能滚：滚轮打在盒子中心，`ListState` 的滚动偏移必须动。
        let boxed = vcx.debug_bounds("ai-wl-box").expect("名单盒子没渲染出来");
        let off0 = scroll_y(vcx);
        assert_eq!(off0, 0., "刚打开时名单不该是滚过的状态（偏移为 {off0}）");
        vcx.simulate_event(gpui::ScrollWheelEvent {
            position: boxed.center(),
            delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(-60.))),
            ..Default::default()
        });
        vcx.run_until_parked();
        let off1 = scroll_y(vcx);
        assert!(
            off1 < off0 - 10.,
            "滚轮打了 60px，名单只滚了 {:.1}px（{off0:.1} → {off1:.1}）：\
             用户滚不动这份白名单，等于看不见后面的条目",
            off0 - off1
        );

        // ③ 页签换的是**内容**：切到内置页后，委托体里装的应当整批变成只读命令
        //    （不是自定义页那 20 条）。空的自定义页反过来要贴合内容 ——
        //    上限不能变成固定高度，否则中间会空一大块。
        main.update_in(&mut vcx.clone(), |m, _w, cx| {
            m.ai_ui.ai_cfg.auto_approve.clear();
            m.ai_ui.ai_wl_tab = TAB_BUILTIN;
            cx.notify();
        });
        vcx.run_until_parked();
        let builtin = rows_are_builtin(vcx);
        assert_eq!(
            builtin.len(),
            rustshell_core::ai::BUILTIN_READONLY_COMMANDS.len(),
            "内置页的名单行数不对：委托体里装的可能还是自定义页那一份"
        );
        assert!(
            builtin.iter().all(|is_builtin| *is_builtin),
            "内置页里混进了自定义条目：页签切了、灌进 `List` 的行没跟着换。"
        );
        let builtin_h = box_h(vcx);
        assert!(
            builtin_h > 60. && builtin_h <= LIST_MAX_H + 2.,
            "内置页的盒子 {builtin_h}px：既不该塌（清单有 {} 条），也不该超过上限",
            rustshell_core::ai::BUILTIN_READONLY_COMMANDS.len()
        );

        main.update_in(&mut vcx.clone(), |m, _w, cx| {
            m.ai_ui.ai_wl_tab = TAB_CUSTOM;
            cx.notify();
        });
        vcx.run_until_parked();
        assert!(
            rows_are_builtin(vcx).is_empty(),
            "自定义页清空后委托体里还留着行：空态应当走 `render_empty`，不是照旧渲染。"
        );
        let empty_h = box_h(vcx);
        assert!(
            empty_h < LIST_MAX_H - 20.,
            "空白的自定义页盒子也有 {empty_h}px 高：上限被当成了固定高度，中间会空一大块"
        );
    }
}
