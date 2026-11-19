//! 端口转发面板（左侧栏 `PanelKind::Forwards`，F-3）。
//!
//! 只读状态、只发动作：列表来自 [`MainView::forward_rows`]，启停 / 增删改一律
//! 调 `main_view::forward_actions` 里的方法。面板自己不做状态推导 —— 两处各推
//! 一遍的代价是「界面显示的与真正在监听的不一致」。
//!
//! # 版式（与连接树 / 定时任务同一套）
//!
//! 标题行左标题、右加号；下面直接是规则列表（**没有搜索行**：一个连接最多
//! 七八条规则，列表一屏就到底，加搜索框只会在空态时多一行没用完的空白）。
//!
//! 每行两行文字：
//!   * 第一行 —— 规则名 + 状态点 + 启停按钮 + 编辑按钮
//!   * 第二行 —— `主机 · 监听 → 目标`（弱化色，单行省略）
//!
//! 为什么不是截图里那种五列表格：侧栏 240px 放下 NAME / HOST / LISTENER /
//! DESTINATION / STATUS 五列，每列只剩四十来像素，等于全用省略号代替内容。
//! 两行式在同一宽度里能把「哪条规则、在哪台机器、监听什么、转到哪」都写全。

use gpui::{
    div, prelude::FluentBuilder as _, px, Context, InteractiveElement as _, IntoElement,
    ParentElement as _, SharedString, Styled as _,
};
use gpui_kit::component::{
    button::{Button, ButtonVariants as _},
    h_flex,
    menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem},
    v_flex, Sizable as _,
};

/// 全量 Lucide 目录（`gpui_kit::component::IconName` 只是默认子集）。
use gpui_kit::assets::IconName as LucideIcon;

use rustshell_core::i18n::t;

use super::main_view::forward_actions::ForwardRow;
use super::main_view::MainView;

/// 行圆角（比卡片壳小一档：行是卡片内部的元素，与任务面板同值）。
const ROW_RADIUS: f32 = 6.;
/// 第一行文字字号（侧栏主体信息）
const NAME_TEXT: f32 = 12.;
/// 第二行文字字号（弱化信息）
const META_TEXT: f32 = 11.;
/// 状态点直径：只有红 / 绿两种实心点（与标签栏、会话列表同一口径）。
const DOT: f32 = 6.;

/// 渲染端口转发面板。
pub fn render(m: &mut MainView, cx: &mut Context<MainView>) -> impl IntoElement {
    let tk = crate::theme::current(cx);
    use crate::theme::to_hsla as hex;

    // 先把行快照取成 owned 值：下面的 `cx.listener` 会可变借 `m`。
    let rows = m.forward_rows();
    let empty = rows.is_empty();

    v_flex()
        .id("forwards-panel")
        // 供用例量「面板真的长出来了」用（release 下 no-op）。
        .debug_selector(|| "forwards-panel".to_string())
        .w_full()
        .h_full()
        // 标题行：左标题、右加号（与连接树 / 聊天记录 / 定时任务同一版式）
        .child(
            h_flex()
                .w_full()
                .items_center()
                .justify_between()
                .gap_2()
                .px_3()
                .py_2()
                .border_b_1()
                .border_color(hex(tk.border_subtle))
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(hex(tk.fg_muted))
                        .child(t("端口转发")),
                )
                .child(
                    Button::new("fwd-new")
                        .ghost()
                        .xsmall()
                        .flex_shrink_0()
                        .icon(LucideIcon::Plus)
                        .tooltip(t("新建端口转发"))
                        .on_click(cx.listener(|m, _e, w, cx| {
                            // 空 profile_id = 「按当前活动标签所属连接自动挑一台」
                            m.open_forward_form("", None, w, cx);
                        })),
                ),
        )
        .child(
            v_flex()
                .id("fwd-list")
                .flex_1()
                .min_h_0()
                .w_full()
                .gap_0p5()
                .py_1()
                .when(empty, |d| {
                    d.child(
                        v_flex()
                            .w_full()
                            .items_center()
                            .justify_center()
                            .gap_2()
                            .py_8()
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(hex(tk.fg_muted))
                                    .child(t("还没有端口转发规则")),
                            )
                            .child(
                                Button::new("fwd-empty-new")
                                    .ghost()
                                    .xsmall()
                                    .label(t("新建规则"))
                                    .text_color(hex(tk.accent))
                                    .on_click(cx.listener(|m, _e, w, cx| {
                                        m.open_forward_form("", None, w, cx);
                                    })),
                            ),
                    )
                })
                .children(rows.iter().map(|row| forward_row(row, cx))),
        )
}

/// 一条规则行。
fn forward_row(row: &ForwardRow, cx: &mut Context<MainView>) -> gpui::AnyElement {
    let tk = crate::theme::current(cx);
    use crate::theme::to_hsla as hex;

    // 闭包要 owned 值（`cx.listener` 与右键菜单都可能在行快照之后才执行）
    let profile_id = row.profile_id.clone();
    let rule_id = row.rule.id.clone();
    let running = row.is_running();
    let active = row.is_active();
    let name = if row.rule.name.is_empty() {
        rustshell_core::connections::default_forward_name(row.rule.kind, row.rule.listen_port)
    } else {
        row.rule.name.clone()
    };
    let meta = format!("{} · {}", row.host, row.rule.summary());
    let status = row.status_label();
    let row_key: SharedString = format!("fwd-{rule_id}").into();

    // 右键菜单：启停 / 编辑 / 删除
    let menu_pid = profile_id.clone();
    let menu_rid = rule_id.clone();
    let menu_running = running;

    v_flex()
        .id(row_key)
        .w_full()
        .mx_1()
        .px_2()
        .py_1()
        .gap_0p5()
        .rounded(px(ROW_RADIUS))
        .hover(|s| s.bg(hex(tk.bg_elevated)))
        .context_menu(move |menu: PopupMenu, _w, _cx| {
            let start_pid = menu_pid.clone();
            let start_rid = menu_rid.clone();
            let stop_pid = menu_pid.clone();
            let stop_rid = menu_rid.clone();
            let edit_pid = menu_pid.clone();
            let edit_rid = menu_rid.clone();
            let del_pid = menu_pid.clone();
            let del_rid = menu_rid.clone();
            let toggle_item = if menu_running {
                PopupMenuItem::new(t("停止"))
                    .icon(LucideIcon::CircleX)
                    .on_click(move |_, _w, cx| {
                        super::main_view::ui_call_forward_stop(
                            stop_pid.clone(),
                            stop_rid.clone(),
                            cx,
                        );
                    })
            } else {
                PopupMenuItem::new(t("启动"))
                    .icon(LucideIcon::Play)
                    .on_click(move |_, _w, cx| {
                        super::main_view::ui_call_forward_start(
                            start_pid.clone(),
                            start_rid.clone(),
                            cx,
                        );
                    })
            };
            let edit_rid_opt = Some(edit_rid.clone());
            menu.item(toggle_item)
                .item(
                    PopupMenuItem::new(t("编辑…"))
                        .icon(LucideIcon::Settings2)
                        .on_click(move |_, w, cx| {
                            super::main_view::ui_call_forward_form(
                                edit_pid.clone(),
                                edit_rid_opt.clone(),
                                w,
                                cx,
                            );
                        }),
                )
                .separator()
                .item(
                    PopupMenuItem::new(t("删除"))
                        .icon(LucideIcon::Trash)
                        .on_click(move |_, _w, cx| {
                            super::main_view::ui_call_forward_remove(
                                del_pid.clone(),
                                del_rid.clone(),
                                cx,
                            );
                        }),
                )
        })
        // 第一行：名字 + 状态点 + 启停 + 编辑
        .child(
            h_flex()
                .w_full()
                .min_w_0()
                .items_center()
                .gap_1()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(px(NAME_TEXT))
                        .text_color(hex(tk.fg_primary))
                        // 单行 + 省略号：折行会把行撑高（与连接树同一条纪律）
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .child(SharedString::from(name)),
                )
                // 状态点：绿 = 正在监听，红 = 其余一切（未启动 / 启动中 / 失败）。
                // 「不画点」会让「这条规则存在但没跑」和「这条规则跑着」在扫视时
                // 完全一样 —— 面板最该一眼看出的就是这件事。
                .child(
                    div()
                        .flex_shrink_0()
                        .w(px(DOT))
                        .h(px(DOT))
                        .rounded_full()
                        .bg(if active {
                            hex(tk.success)
                        } else {
                            hex(tk.danger)
                        }),
                )
                .child(
                    Button::new(SharedString::from(format!("fwd-toggle-{rule_id}")))
                        .ghost()
                        .xsmall()
                        .flex_shrink_0()
                        .icon(if running {
                            LucideIcon::CircleX
                        } else {
                            LucideIcon::Play
                        })
                        .tooltip(if running { t("停止") } else { t("启动") })
                        .on_click({
                            let pid = profile_id.clone();
                            let rid = rule_id.clone();
                            cx.listener(move |m, _e, _w, cx| {
                                if running {
                                    m.forward_stop(&pid, &rid, cx);
                                } else {
                                    m.forward_start(&pid, &rid, cx);
                                }
                            })
                        }),
                )
                .child(
                    Button::new(SharedString::from(format!("fwd-edit-{rule_id}")))
                        .ghost()
                        .xsmall()
                        .flex_shrink_0()
                        .icon(LucideIcon::Settings2)
                        .tooltip(t("编辑…"))
                        .on_click({
                            let pid = profile_id.clone();
                            let rid = rule_id.clone();
                            cx.listener(move |m, _e, w, cx| {
                                m.open_forward_form(&pid, Some(&rid), w, cx);
                            })
                        }),
                ),
        )
        // 第二行：主机 · 监听 → 目标（弱化色）
        .child(
            div()
                .w_full()
                .min_w_0()
                .text_size(px(META_TEXT))
                .text_color(if active {
                    hex(tk.fg_secondary)
                } else {
                    hex(tk.fg_muted)
                })
                .whitespace_nowrap()
                .text_ellipsis()
                .child(SharedString::from(meta)),
        )
        // 失败原因单独一行：`bind` 失败（端口被占 / 无权限）只在状态里写着，
        // tooltip 里能看但要悬停才知道 —— 这条信息恰恰解释了「为什么没绿」。
        .when(
            matches!(
                row.status,
                Some(rustshell_core::forward::ForwardStatus::Failed(_))
            ),
            |d| {
                d.child(
                    div()
                        .w_full()
                        .min_w_0()
                        .text_size(px(META_TEXT))
                        .text_color(hex(tk.danger))
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .child(SharedString::from(status.clone())),
                )
            },
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use crate::test_support::prod;

    fn code() -> String {
        prod(include_str!("forward_panel.rs"))
    }

    /// 面板必须把「新建 / 启停 / 编辑」三件事都摆在行上。
    ///
    /// 少任何一样，用户就得去手改 `connections.json` —— 而这个面板存在的理由
    /// 正是「规则不用手改配置也能配」。
    #[test]
    fn panel_offers_create_start_stop_and_edit() {
        let c = code();
        for needle in [
            "fwd-new",
            "fwd-empty-new",
            "fwd-toggle-",
            "fwd-edit-",
            "forward_start",
            "forward_stop",
            "open_forward_form",
        ] {
            assert!(c.contains(needle), "端口转发面板缺了 `{needle}`");
        }
        assert!(
            c.contains("justify_between"),
            "标题行必须左标题、右加号（与连接树 / 定时任务同一版式）"
        );
    }

    /// 状态点只有**绿 / 红**两种实心色：绿 = 正在监听，红 = 其余一切
    /// （未启动 / 启动中 / 失败）。「不画点」也不许回来 —— 空着会让「这条规则
    /// 没跑」和「跑着」在扫视时完全一样。
    #[test]
    fn status_dot_is_only_green_or_red() {
        let c = code();
        assert!(
            c.contains("hex(tk.success)") && c.contains("hex(tk.danger)"),
            "状态点必须是绿 / 红两色实心点"
        );
        assert!(
            !c.contains("hex(tk.warning)"),
            "状态点出现了第三种颜色：只有「绿 = 可用 / 红 = 其余」两档"
        );
    }

    /// 「只绑回环地址」是产品口径：面板与表单都不许出现监听地址入口。
    #[test]
    fn the_panel_never_offers_a_listen_host_field() {
        let c = code();
        assert!(
            !c.contains("listen_host"),
            "面板里出现了监听地址相关代码 —— 产品口径是只绑 127.0.0.1"
        );
    }
}
