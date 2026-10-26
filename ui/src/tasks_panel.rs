//! 定时任务面板（左侧栏 `PanelKind::Tasks`）。
//!
//! 只读状态、只发动作：列表数据来自 `MainView::task_store` 与调度器的运行记录
//! 快照，所有改动都调 `main_view::task_actions` 里的方法。面板自己不做任何
//! 状态推导 —— 两处各推导一遍的代价是「界面显示的与落盘的不一致」。
//!
//! 圆角与底色由外层卡片壳（`MainView::render` 的 `card_shell`）统一提供，
//! 本面板只负责内容（与 `ai_history_panel` 同一约定）。

use gpui::{
    div, prelude::FluentBuilder as _, px, Context, InteractiveElement as _, IntoElement,
    ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _,
};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    h_flex,
    input::Input,
    menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem},
    scroll::ScrollableElement as _,
    v_flex, Icon, Sizable as _,
};

/// 全量 Lucide 目录（`gpui_component::IconName` 只是组件默认子集）。
use gpui_kit_assets::IconName as LucideIcon;

use rustshell_core::tasks::{RunRecord, RunStatus, TaskAction, TaskTarget};

use super::main_view::MainView;

/// 行圆角（比卡片壳的 8px 小一档：行是卡片内部的元素）。
const ROW_RADIUS: f32 = 6.;

/// 面板宽度（与其它侧栏面板一致）。实际宽度由外层 resizable 面板控制，
/// 这里只用于文档说明与内部布局的参考。
#[allow(dead_code)]
const PANEL_W: f32 = 240.;

/// 渲染任务面板。
pub fn render(m: &mut MainView, cx: &mut Context<MainView>) -> impl IntoElement {
    let t = crate::theme::current(cx);
    use crate::theme::to_hsla as hex;

    // 关键字现读现用（不缓存到视图上）：输入框的值就是唯一事实，
    // 两份状态一旦分叉，表现就是「打了字列表不动」或「清了字还过滤着」。
    let query = m.io.task_search.read(cx).value().trim().to_string();
    let ids = m.task_ids_matching(&query);
    let no_match = ids.is_empty() && !query.is_empty();
    let toast = m.io.task_toast.clone();
    let expanded = m.io.task_expanded.clone();

    v_flex()
        .id("tasks-panel")
        .w_full()
        .h_full()
        // 标题行（2026-10-21 用户要求「定时任务栏也要类似改动，+新增也放标题行」）：
        // 与连接树 / 聊天记录同一版式 —— 左标题、右加号、下边框。
        // 「新建 AI 巡检」从此在标题行上，搜索行只做搜索。
        .child(
            h_flex()
                .w_full()
                .items_center()
                .justify_between()
                .gap_2()
                .px_3()
                .py_2()
                .border_b_1()
                .border_color(hex(t.border_subtle))
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(hex(t.fg_muted))
                        .child(rustshell_core::i18n::t("定时任务")),
                )
                .child(
                    Button::new("task-new-ai")
                        .ghost()
                        .xsmall()
                        .flex_shrink_0()
                        .icon(LucideIcon::Plus)
                        .tooltip(rustshell_core::i18n::t("新建 AI 巡检"))
                        .on_click(cx.listener(|m, _e, w, cx| {
                            m.task_add_template(
                                TaskAction::AiInspect {
                                    target: TaskTarget::default(),
                                    prompt: String::new(),
                                    // 巡检项已撤下（查什么由模型按「AI 要求」决定）
                                    checks: Vec::new(),
                                    thresholds: None,
                                    log_path: None,
                                    log_level: Default::default(),
                                },
                                w,
                                cx,
                            );
                        })),
                ),
        )
        // 查询行：任务一多（几十条）就得能按名字找 —— 列表里只有名字，
        // 眼睛扫比打字慢得多。搜索框常驻（不藏进折叠区）：它有意义的
        // 前提是用户知道它在。
        //
        // 2026-10-21 用户要求「定时任务搜索栏左侧增加放大镜图标」：
        // 与连接树搜索框同一手法（`Input::prefix`，图标落在框内左侧、
        // 随输入框高度居中）。此前这里明确撤掉过放大镜，现在是加回来。
        .child(
            h_flex()
                .w_full()
                .items_center()
                .px_3()
                .py_2()
                .child(
                    // 包装层挂「点框外失焦」（与连接栏同一手法）：点列表行、
                    // 空白区这些不可聚焦的地方时，焦点也应当从搜索框松开。
                    div()
                        .flex_1()
                        .min_w_0()
                        .debug_selector(|| "task-search-box".to_string())
                        .on_mouse_down_out(cx.listener(|m, _e, w, cx| {
                            super::main_view::blur_input_on_outside_click(&m.io.task_search, w, cx)
                        }))
                        .child(
                            Input::new(&m.io.task_search)
                                .small()
                                .cleanable(true)
                                .prefix(
                                    // 外层 div 只为挂量取钩子（图标本身不带 debug_selector）。
                                    div()
                                        .debug_selector(|| "task-search-icon".to_string())
                                        .child(
                                            Icon::new(LucideIcon::Search)
                                                .xsmall()
                                                .text_color(hex(t.fg_muted)),
                                        ),
                                )
                                .w_full(),
                        ),
                ),
        )
        // 兜底提示：只留给**失败与异常**（提示出口见 `show_task_toast`）。
        // 「已保存 / 已开始执行 / 已启用」这类成功确认已按用户要求撤下 ——
        // 列表本身就把结果显示出来了，再来一行字只是噪音。
        .children(toast.map(|msg| {
            div()
                .mx_2()
                .mt_1()
                .px_2()
                .py_1()
                .rounded(px(ROW_RADIUS))
                .bg(hex(t.bg_elevated))
                .text_size(px(11.))
                .text_color(hex(t.warning))
                .child(SharedString::from(msg))
        }))
        // 任务列表
        .child(
            v_flex()
                .id("task-list")
                .flex_1()
                .min_h_0()
                .w_full()
                .gap_0p5()
                .py_1()
                .overflow_y_scrollbar()
                .when(ids.is_empty(), |d| {
                    if no_match {
                        // 搜不到 ≠ 没有任务：这里不给模板入口 —— 用户手上
                        // 明明有一堆任务，再劝他「新建一个」只会让人困惑。
                        return d.child(
                            v_flex()
                                .w_full()
                                .items_center()
                                .justify_center()
                                .py_8()
                                .child(
                                    div()
                                        .text_size(px(11.))
                                        .text_color(hex(t.fg_muted))
                                        .child(rustshell_core::i18n::t("没有匹配的任务")),
                                ),
                        );
                    }
                    // 空态：给模板入口，而不是一句「暂无数据」
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
                                    .text_color(hex(t.fg_muted))
                                    .child(rustshell_core::i18n::t("还没有定时任务")),
                            )
                            .child(template_button(cx)),
                    )
                })
                .children(ids.into_iter().map(|id| {
                    let is_expanded = expanded.as_deref() == Some(id.as_str());
                    task_row(m, &id, is_expanded, cx)
                })),
        )
}

/// 空态里的「AI 巡检」入口。
///
/// 模板只剩这一个：表单也只编辑 AI 巡检（运行反馈：定时任务 = cron + 目标 +
/// AI 要求 + 通知）。批量命令的模板曾留在这里，但表单撤下「命令」字段后，
/// 从它建出来的任务连命令都没处填 —— 留着等于给用户挖坑。
/// （既有批量命令任务仍保留其命令，见 `task_collect_draft`。）
fn template_button(cx: &mut Context<MainView>) -> impl IntoElement {
    let t = crate::theme::current(cx);
    use crate::theme::to_hsla as hex;
    Button::new("task-tpl-ai")
        .ghost()
        .xsmall()
        .label(rustshell_core::i18n::t("AI 巡检"))
        .text_color(hex(t.accent))
        .on_click(cx.listener(|m, _e, w, cx| {
            m.task_add_template(
                TaskAction::AiInspect {
                    target: TaskTarget::default(),
                    prompt: String::new(),
                    // 巡检项已撤下：查什么由模型按「AI 要求」决定
                    checks: Vec::new(),
                    thresholds: None,
                    log_path: None,
                    log_level: Default::default(),
                },
                w,
                cx,
            );
        }))
}

/// 一行任务。
fn task_row(
    m: &mut MainView,
    task_id: &str,
    expanded: bool,
    cx: &mut Context<MainView>,
) -> gpui::AnyElement {
    let t = crate::theme::current(cx);
    use crate::theme::to_hsla as hex;

    let Some(task) = m.io.task_store.get(task_id).cloned() else {
        // 任务在渲染过程中被删掉：这一帧直接跳过（下一帧列表会刷新）
        return div().into_any_element();
    };
    let running = m.task_running(task_id);
    let runs = if expanded {
        m.task_runs(task_id)
    } else {
        Vec::new()
    };
    // 先把要显示的值复制成纯值：下面的 `cx.listener(..)` 与右键菜单闭包
    // 都会可变借 `m`，渲染期继续持有 `task` 的借用会冲突
    // （与 `ai_history_panel` 同一手法）。
    let enabled = task.enabled;
    let task_name = task.name.clone();

    let row_id: SharedString = format!("task-{task_id}").into();
    let menu_id = task_id.to_string();
    let expand_id = task_id.to_string();
    let selector_id = task_id.to_string();
    let switch_id = task_id.to_string();

    let mut row = v_flex()
        .id(row_id)
        .w_full()
        .mx_1()
        .gap_0p5()
        .rounded(px(ROW_RADIUS))
        .hover(|s| s.bg(hex(t.bg_elevated)))
        // 右键：立即执行 / 启用停用 / 复制 / 删除
        .context_menu(move |menu: PopupMenu, _w, _cx| {
            let run = menu_id.clone();
            let toggle = menu_id.clone();
            let dup = menu_id.clone();
            let del = menu_id.clone();
            let stop = menu_id.clone();
            let edit = menu_id.clone();
            menu.item(
                PopupMenuItem::new(rustshell_core::i18n::t("立即执行"))
                    .icon(LucideIcon::Play)
                    .on_click(move |_, _w, cx| {
                        super::main_view::ui_call_task_run_now(run.clone(), cx);
                    }),
            )
            .item(
                PopupMenuItem::new(rustshell_core::i18n::t("停止"))
                    .icon(LucideIcon::CircleX)
                    .on_click(move |_, _w, cx| {
                        super::main_view::ui_call_task_stop(stop.clone(), cx);
                    }),
            )
            .item(
                PopupMenuItem::new(rustshell_core::i18n::t("编辑…"))
                    .icon(LucideIcon::Settings2)
                    .on_click(move |_, w, cx| {
                        super::main_view::ui_call_task_edit(edit.clone(), w, cx);
                    }),
            )
            .separator()
            .item(
                // 菜单跟随当前状态：已启用时给的是「停用」，反之亦然 ——
                // 写死一个词会让用户在「已经开着」时以为点了才开。
                PopupMenuItem::new(if enabled {
                    rustshell_core::i18n::t("停用")
                } else {
                    rustshell_core::i18n::t("启用")
                })
                .icon(if enabled {
                    LucideIcon::CircleX
                } else {
                    LucideIcon::Bell
                })
                .on_click(move |_, _w, cx| {
                    super::main_view::ui_call_task_toggle(toggle.clone(), cx);
                }),
            )
            .item(
                PopupMenuItem::new(rustshell_core::i18n::t("复制"))
                    .icon(LucideIcon::Copy)
                    .on_click(move |_, _w, cx| {
                        super::main_view::ui_call_task_duplicate(dup.clone(), cx);
                    }),
            )
            .separator()
            .item(
                PopupMenuItem::new(rustshell_core::i18n::t("删除"))
                    .icon(LucideIcon::Trash)
                    .on_click(move |_, _w, cx| {
                        super::main_view::ui_call_task_delete(del.clone(), cx);
                    }),
            )
        })
        // 主行：启停图标 + 标题 + 下次触发 + **单条任务的开关**
        .child(
            h_flex()
                .id(SharedString::from(format!("task-head-{task_id}")))
                // 测试钩子（release 下被 `#[cfg]` 摘掉）：用例要能确认这一行真的
                // 渲染出了非零高度。`debug_bounds` 只收 `&'static str`，
                // 而任务 id 是运行期的 —— 用例那边统一用 `t-a` / `t-b` 两个固定 id。
                .debug_selector(move || {
                    if selector_id.as_str() == "t-a" {
                        "task-head-t-a".to_string()
                    } else {
                        "task-head-t-b".to_string()
                    }
                })
                .w_full()
                .items_center()
                .gap_2()
                .px_2()
                .py_1()
                .child(
                    // 可点区域只包「名字」：开关有自己的点击语义，包在一起会让
                    // 「拨开关」顺手把运行记录也展开（或用 `stop_propagation` 去补，
                    // 那是在跟布局约定打架）。
                    //
                    // 行里只有名字与开关（运行反馈：其他都不显示）；运行状态、
                    // 会话入口在展开后的运行记录里看（那里有富余的横向空间）。
                    h_flex()
                        // `on_click` 挂在 `StatefulInteractiveElement` 上：没有 id 的
                        // 纯布局节点收不了点击（开关必须留在外面单独可点，见上）。
                        .id(SharedString::from(format!("task-head-click-{task_id}")))
                        .flex_1()
                        .min_w_0()
                        .items_center()
                        .gap_2()
                        .on_click(cx.listener(move |m, _e, _w, cx| {
                            m.task_toggle_expand(&expand_id, cx);
                        }))
                        // 任务图标：定时任务只有 AI 巡检一种（用户确认），
                        // 统一一个机器人图标 —— 不再按类型分发。
                        .child(
                            Icon::new(LucideIcon::Bot)
                                .xsmall()
                                .flex_shrink_0()
                                .text_color(if enabled {
                                    hex(t.accent)
                                } else {
                                    hex(t.fg_muted)
                                }),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_size(px(12.))
                                // 停用态弱化文字色：开关在右端，眼睛扫过名字时
                                // 也该一眼看出这条是停的。
                                .text_color(if enabled {
                                    hex(t.fg_primary)
                                } else {
                                    hex(t.fg_muted)
                                })
                                .child(SharedString::from(task_name.clone())),
                        ),
                )
                // 开关是**针对这一条任务**的（不是全局总闸）：到点会不会自动跑
                // 只看这里。用开关而不是图标按钮：按钮只能表达一个动作，
                // 开关能一眼看出「现在是开还是关」。
                .child(
                    gpui_component::switch::Switch::new(SharedString::from(format!(
                        "task-switch-{task_id}"
                    )))
                    .xsmall()
                    .checked(enabled)
                    .on_click(cx.listener(
                        move |m, checked: &bool, _w, cx| {
                            m.task_enable(&switch_id, *checked, cx);
                        },
                    )),
                ),
        )
        // 运行中的任务在名字后面缀一句（其余状态不再占位：行里只有名字与开关，
        // 但「正在跑」是等不得的反馈 —— 不显示的话用户会以为没触发）
        .when(running, |d| {
            d.child(
                div()
                    .px_2()
                    .pb_1()
                    .text_size(px(10.))
                    .text_color(hex(t.accent))
                    .child(rustshell_core::i18n::t("正在运行")),
            )
        });

    // 展开区：运行记录
    if expanded {
        let mut history = v_flex().w_full().px_2().pb_2().gap_0p5().child(
            div()
                .text_size(px(10.))
                .text_color(hex(t.fg_muted))
                .child(rustshell_core::i18n::t("运行记录")),
        );
        if runs.is_empty() {
            history = history.child(
                div()
                    .text_size(px(10.))
                    .text_color(hex(t.fg_muted))
                    .child(rustshell_core::i18n::t("还没有运行记录")),
            );
        } else {
            for rec in runs.iter().take(20) {
                history = history.child(run_row(rec, task_id, cx));
            }
        }
        row = row.child(history);
    }

    row.into_any_element()
}

/// 展开区里的一行运行记录：**执行时间 / 状态 / 会话入口**（三列，2026-09-21 定）。
///
/// 概要（`RunRecord::one_line()`）已撤下：侧栏只有 240px，它截出来的是半句话，
/// 而「这次到底查了什么」点会话就能看全文。记录本身仍存着概要，别处要用。
///
/// 「会话」只在这次运行真的留下了会话痕迹时可点 —— 老记录（升级前跑的）与
/// 非 AI 巡检的任务都没有，此时显示 `—`：给一个点了没反应的入口比没有更糟。
fn run_row(rec: &RunRecord, task_id: &str, cx: &mut Context<MainView>) -> impl IntoElement {
    let t = crate::theme::current(cx);
    use crate::theme::to_hsla as hex;
    let color = match rec.status {
        RunStatus::Ok => t.success,
        RunStatus::Partial => t.warning,
        RunStatus::Failed => t.danger,
        _ => t.fg_muted,
    };
    let task_id = task_id.to_string();
    let run_id = rec.run_id.clone();

    h_flex()
        .w_full()
        .items_center()
        .gap_2()
        .text_size(px(10.))
        .child(
            div()
                .flex_shrink_0()
                .text_color(hex(t.fg_muted))
                .child(SharedString::from(rustshell_core::metrics::datetime_label(
                    rec.started_at,
                ))),
        )
        .child(
            div()
                .w(px(52.))
                .flex_shrink_0()
                .truncate()
                .text_color(hex(color))
                .child(SharedString::from(rec.status.label().to_string())),
        )
        // 弹性占位：把「会话」顶到右端。这一栏原先放 `one_line()` 概要，
        // 但侧栏只有 240px，概要截成半句话反而占掉了状态与会话的位置。
        .child(div().flex_1().min_w_0())
        .child(if rec.session.is_some() {
            // 点它 = 把这次巡检搬进 AI 助手（会切到 AI 面板并选中那条会话）
            div()
                .id(SharedString::from(format!("task-run-chat-{run_id}")))
                .flex_shrink_0()
                .px_1()
                .rounded(px(ROW_RADIUS))
                .text_color(hex(t.accent))
                .hover(|s| s.bg(hex(t.bg_elevated)))
                .on_click(cx.listener(move |m, _e, _w, cx| {
                    m.task_open_run_session(&task_id, &run_id, cx);
                }))
                .child(rustshell_core::i18n::t("会话"))
                .into_any_element()
        } else {
            div()
                .flex_shrink_0()
                .text_color(hex(t.fg_muted))
                .child(SharedString::from("—"))
                .into_any_element()
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::prod;

    /// 生产代码正文（剥注释 + 切测试模块）。
    fn code() -> String {
        prod(include_str!("tasks_panel.rs"))
    }

    /// 面板必须提供完整的一组动作：新建、单条启停、立即执行、删除、展开记录。
    /// 少一个都等于「用户没法用」，而这类缺失在编译期完全看不出来。
    #[test]
    fn panel_offers_the_full_set_of_task_actions() {
        let c = code();
        for needle in [
            "task_add_template",  // 新建（头部 + 空态模板）
            "task_enable",        // 单条任务的启停开关
            "task_run_now",       // 立即执行（右键菜单）
            "task_delete",        // 删除（右键菜单）
            "task_toggle_expand", // 展开运行记录
            "\"定时任务\"",       // 栏名
            "\"运行记录\"",       // 展开区的标题
        ] {
            assert!(c.contains(needle), "任务面板缺少 `{needle}`");
        }
    }

    /// 面板不得自带底色：底色与圆角由 `MainView` 的卡片壳统一提供，
    /// 自己再画一遍会在卡片圆角处露出一圈直角内衬（与 `ai_history_panel` 同一约定）。
    #[test]
    fn panel_does_not_paint_its_own_background() {
        let c = code();
        assert!(
            !c.contains(".bg(crate::theme::surface"),
            "面板不自带底色 —— 底色与圆角由 `MainView` 的卡片壳提供"
        );
    }

    /// 图标必须来自随包的 svg：写一个没有 svg 的名字**不会报错**，
    /// 只在界面上留一片空白。
    ///
    /// 右键菜单那组 + 任务行的机器人图标（只有 AI 巡检一种，统一一个图标）+
    /// 搜索框右侧的「新建」。放大镜已按用户要求撤下，`Search` 随之不再使用。
    #[test]
    fn panel_uses_shipped_icons_only() {
        let c = code();
        for icon in [
            "CircleX",
            "Play",
            "Bell",
            "Copy",
            "Trash",
            "Plus",
            "Settings2",
            "Bot",
        ] {
            assert!(
                c.contains(&format!("LucideIcon::{icon}")),
                "面板应当使用 `{icon}`"
            );
        }
        assert!(
            !c.contains("LucideIcon::Circle,") && !c.contains("LucideIcon::Circle)"),
            "`Circle` 没有对应 svg（用 CircleCheck / CircleX 代替）"
        );
    }

    /// 任务行里只有**图标 + 名字 + 开关**（2026-09-21 运行反馈）。
    ///
    /// 撤掉的东西在别处还有落点：下次触发在编辑对话框（时间）、上次结果在
    /// 展开后的运行记录里 —— 因此它们不是「丢了」，而是不再挤在列表行上。
    /// 图标只有**一个**（机器人）：定时任务只有 AI 巡检一种，按类型分发图标
    /// 是在给用户暗示一个不存在的分类。
    #[test]
    fn task_row_shows_only_the_name_and_switch() {
        let c = code();
        let at = c.find("fn task_row(").expect("找不到任务行");
        let end = c.find("fn run_row(").expect("找不到运行记录行");
        let row = &c[at..end];
        for gone in [
            "LucideIcon::CircleCheck", // 启停图标（状态由开关表达）
            "next_label",              // 下次触发时刻
            "subtitle",                // 动作 · 目标
            "status_line",             // 上次结果
        ] {
            assert!(
                !row.contains(gone),
                "任务行里还有 `{gone}`：行里只留名字与开关"
            );
        }
        for kept in [
            "task-switch-",
            "task-head-click-",
            "task_toggle_expand",
            "LucideIcon::Bot", // 唯一的任务图标（AI 巡检）
        ] {
            assert!(row.contains(kept), "任务行缺了 `{kept}`");
        }
        // 类型→图标的映射整体撤下（含函数与「批量命令」图标）
        for gone in ["fn task_icon(", "LucideIcon::SquareTerminal"] {
            assert!(!c.contains(gone), "类型图标相关代码 `{gone}` 还在");
        }
    }

    /// 面板必须有查询功能（用户要求）：搜索框 + 按名称过滤 + 「搜不到」的提示。
    ///
    /// 三条都不可省：没有输入框用户无从下手；输入框不接过滤（渲染时还在用
    /// 全量 id）就是「打了字没用」；搜不到时复用「还没有定时任务」会让用户
    /// 以为任务被删了。
    #[test]
    fn panel_has_a_name_search() {
        let c = code();
        for needle in [
            "Input::new(&m.io.task_search)", // 搜索框
            "task_ids_matching",             // 过滤（决策在 core 的纯函数里）
            "\"没有匹配的任务\"",            // 搜不到 ≠ 一条都没有
        ] {
            assert!(c.contains(needle), "任务面板的查询功能缺了 `{needle}`");
        }
        // 左侧放大镜（2026-10-21 用户要求「定时任务搜索栏左侧增加放大镜图标」）：
        // 与连接树同一手法走官方 `Input::prefix`，图标落在框内左侧。
        // 反向断言也留着：以前这里明确撤掉过放大镜，别又被人「顺手简化」掉。
        assert!(
            c.contains("LucideIcon::Search"),
            "搜索框左侧的放大镜不见了（用户 2026-10-21 要求加回来）。"
        );
        assert!(
            c.contains(".prefix("),
            "放大镜必须走 `Input::prefix` 落在框内左侧，而不是在外面拼一层 h_flex。"
        );
        // 「新建」在**标题行**（2026-10-26 用户要求「+新增也放标题行」，与连接树 /
        // 聊天记录同一版式）。因此它在源码里出现在搜索框**之前**，且全文件只有这一枚。
        assert_eq!(
            c.matches("task-new-ai").count(),
            1,
            "「新建 AI 巡检」入口必须只有一个（标题行那一枚）"
        );
        assert!(
            c.find("task-new-ai").unwrap() < c.find("Input::new(&m.io.task_search)").unwrap(),
            "新建按钮必须落在标题行（搜索行之上），而不是又挤回搜索框右侧"
        );
        assert!(
            c.contains("justify_between"),
            "标题行必须左标题、右加号（justify_between）"
        );
        // 过滤结果为空时必须走「没有匹配」那条分支，而不是照旧给模板入口
        assert!(
            c.contains("no_match"),
            "搜不到时没有单独的空态分支：会显示成「还没有定时任务」"
        );
    }

    /// 搜索框所在的侧栏**不得**被每秒重建（2026-09-28 用户反馈「打字失去焦点」）。
    ///
    /// 这不是本面板的代码，而是它**依赖的上游契约**，所以断言落在
    /// `task_actions.rs` 的 `tasks_tick` 上 —— 面板自己无从防住这件事。
    ///
    /// 失败链条（每次都由「裸 notify」重新点燃）：
    ///   1. `tasks_tick` 每秒发一次**无归属** `cx.notify()`；
    ///   2. `ViewEpoch::wants_redraw` 把「归属序号没前进」判成「兜底重绘」
    ///      ⇒ `SidebarPanelView` 整块重建；
    ///   3. 重建 ⇒ `Input::render` 重跑 ⇒ `gpui-component` 的
    ///      `sync_focused_input_registry` 里那次 `Root::try_update(.. cx.notify())`
    ///      再次触发；
    ///   4. 焦点在重排中重新登记，用户看到的是「打了两个字光标就没了」。
    ///
    /// 反向断言（`!contains("cx.notify()")`）是刻意的：这个函数里出现任何
    /// 无归属 notify 都会把上面那条链重新接上，哪怕旁边已经写了 `notify_view`。
    #[test]
    fn tasks_tick_attributes_its_notify_to_the_sidebar() {
        let src = prod(include_str!("main_view/task_actions.rs"));
        let at = src
            .find("pub fn tasks_tick(&mut self, cx: &mut Context<Self>) -> bool {")
            .expect("`tasks_tick` 不见了（定时任务的心跳）");
        // 只切到本函数的闭合大括号（行首 `}`）
        let end = src[at..]
            .find("\n    }")
            .expect("找不到 `tasks_tick` 的闭合大括号");
        let body = &src[at..at + end];

        assert!(
            body.contains("notify_view(super::ChildView::Sidebar, cx)"),
            "`tasks_tick` 必须把重绘归属点给侧栏（任务列表长在那里）：\
             不点名 ⇒ 空掩码 ⇒ `SidebarPanelView` 每秒整块重建 ⇒ \
             搜索框打字掉焦点。"
        );
        assert!(
            !body.contains("cx.notify()"),
            "`tasks_tick` 里不得出现**无归属**的 `cx.notify()`：\
             它就是「侧栏每秒重建」的点火器。要重绘侧栏请用 `notify_view(..)`。"
        );
    }

    /// 运行记录行只有**三列**：执行时间 / 状态 / 会话入口（2026-09-21 定）。
    ///
    /// 少了时间：用户分不清是哪一次跑；少了状态：看不出好坏；
    /// 「会话」若没有点击接线，用户看得到却打不开「这次到底查了什么」。
    /// 概要（`one_line`）必须**不在**这一行里：侧栏 240px，截出来只有半句话，
    /// 想看内容点会话即可。
    #[test]
    fn expanded_run_rows_show_time_status_and_session() {
        let c = code();
        let at = c.find("fn run_row(").expect("找不到运行记录行");
        // `code()` 已切掉测试模块，`run_row` 之后就是本文件生产代码的末尾
        let row = &c[at..];
        for needle in [
            "datetime_label",        // 执行时间
            "rec.status.label()",    // 状态
            "task-run-chat-",        // 会话入口的 id
            "task_open_run_session", // 点它 → 切到 AI 面板并选中这条会话
            "rec.session.is_some()", // 老记录（没有会话痕迹）不画点了没反应的入口
        ] {
            assert!(row.contains(needle), "运行记录行缺了 `{needle}`");
        }
        for gone in ["one_line", "rec.summary", "run_status_line"] {
            assert!(
                !row.contains(gone),
                "运行记录行里还有 `{gone}`：这一行只留时间、状态、会话三列"
            );
        }
    }

    /// 空态必须给出模板入口，而不是一句「暂无数据」。
    ///
    /// 只剩 **AI 巡检** 一个：定时任务已按运行反馈收敛成「cron + 目标 +
    /// AI 要求 + 通知」，批量下载与批量命令的模板都必须撤掉 —— 表单里没有
    /// 它们的字段，从模板建出来的任务用户根本没法把它配完整。
    #[test]
    fn empty_state_offers_templates() {
        let c = code();
        assert!(c.contains("task-tpl-ai"), "空态缺少「AI 巡检」入口");
        for gone in ["task-tpl-dl", "task-tpl-exec"] {
            assert!(
                !c.contains(gone),
                "空态仍有 `{gone}` 模板入口：该类型已撤下（表单里没有它的字段）"
            );
        }
        assert!(c.contains("\"还没有定时任务\""));
    }

    /// 启停开关必须是**每条任务一个**（用户明确要求：开关针对单条任务），
    /// 并且是「按目标态落定」而不是「取反」—— 开关控件自己知道目标值。
    #[test]
    fn switch_is_per_task_not_a_global_master_switch() {
        let c = code();
        assert!(
            c.contains("task-switch-"),
            "每行任务都要有属于自己的启停开关"
        );
        assert!(
            c.contains("m.task_enable(&switch_id, *checked, cx)"),
            "开关必须走 `MainView::task_enable` 并把勾选的目标态传进去"
        );
        // 全局总开关已撤下：面板上不该再出现它，也不该再读那份设置
        // （设置页的同名开关也已删除，两处保持一致的唯一办法是两处都没有）。
        assert!(
            !c.contains("task-master-switch") && !c.contains("m.settings.tasks_enabled"),
            "面板仍有全局总开关或仍读 `settings.tasks_enabled`"
        );
    }

    /// 真实装配（Root → AppShell → MainView）下，任务面板必须真的渲染出每一行。
    ///
    /// 只断言「有标题」是不够的：列表塌成 0 行、行高为 0、数据源接错，
    /// 都会让用户看到一片空白而测试照旧通过。这里造**两条**任务并量行高。
    #[gpui::test]
    fn panel_renders_one_row_per_task(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

        cx.update(|cx| {
            gpui_component::init(cx);
            crate::theme::apply_theme_mode(cx, rustshell_core::theme::ThemeMode::Dark);
        });

        let slot: std::rc::Rc<std::cell::RefCell<Option<gpui::Entity<MainView>>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let (_, vcx) = {
            let slot = slot.clone();
            cx.add_window_view(move |window, cx| {
                let main = cx.new(|cx| MainView::new(window, cx));
                *slot.borrow_mut() = Some(main.clone());
                let shell = cx.new(|_| crate::main_view::AppShell::new(main));
                gpui_component::Root::new(shell, window, cx)
            })
        };
        vcx.run_until_parked();
        let main = slot.borrow().clone().expect("MainView 未创建");

        main.update_in(&mut vcx.clone(), |m, _w, cx| {
            m.panel_ui.panel = crate::main_view::PanelKind::Tasks;
            m.panel_ui.sidebar_collapsed = false;
            // 两条任务：一条启用、一条停用（两种渲染分支都要走到）
            let now = rustshell_core::connections::unix_now_secs();
            let mut a = rustshell_core::tasks::Task::new(
                "早间巡检",
                rustshell_core::tasks::Trigger::Interval { every_secs: 3600 },
                rustshell_core::tasks::TaskAction::ExecMulti {
                    target: rustshell_core::tasks::TaskTarget {
                        profiles: vec!["p1".into()],
                        ..Default::default()
                    },
                    command: "df -h".into(),
                    timeout_secs: 15,
                    allow_mutating: false,
                },
                now,
            );
            a.id = "t-a".into();
            let mut b = rustshell_core::tasks::Task::new(
                "磁盘检查",
                rustshell_core::tasks::Trigger::Interval { every_secs: 3600 },
                rustshell_core::tasks::TaskAction::AiInspect {
                    target: rustshell_core::tasks::TaskTarget::default(),
                    prompt: String::new(),
                    checks: vec![rustshell_core::tasks::CheckKind::Load],
                    thresholds: None,
                    log_path: None,
                    log_level: Default::default(),
                },
                now,
            );
            b.id = "t-b".into();
            b.enabled = false;
            // 仓库先清空再播种：`MainView::new` 走的是
            // `TaskStore::load(&paths::tasks_path())`，读的是**这台机器上真实的**
            // `tasks.json`。开发机跑过应用、存过任务时，这个用例会带着用户的任务一起
            // 渲染，`len()` 就不是 2 了 —— 2026-09-20 实测报红（`left: 3, right: 2`，
            // 本机 `%APPDATA%\RustShell\tasks.json` 里躺着 1 条「新建 AI 巡检」）。
            // 换成空仓库，让断言只反映本用例造的两条，与运行环境无关。
            m.io.task_store = rustshell_core::tasks::TaskStore::default();
            m.io.task_store.add(a);
            m.io.task_store.add(b);
            // 前置条件就地断言：`read_with` 会**消费** `vcx`（`AppContext` 是按
            // 值实现的），放在后面会与 `debug_bounds` 争用同一个上下文。
            assert_eq!(m.io.task_store.len(), 2, "前置条件没成立：应当有两条任务");
            cx.notify();
        });
        vcx.run_until_parked();

        // selector 必须是字面量：`debug_bounds` 只收 `&'static str`
        for (ix, sel) in [(0, "task-head-t-a"), (1, "task-head-t-b")] {
            let row = vcx
                .debug_bounds(sel)
                .unwrap_or_else(|| panic!("任务列表第 {ix} 行没有渲染出来（列表塌了？）"));
            assert!(
                f32::from(row.size.height) > 0.,
                "任务列表第 {ix} 行高度为 0（行被 flex 挤没了？）"
            );
        }

        // 搜索真的过滤掉不匹配的行（「接了输入框但没接过滤」在源码里看不出来）
        main.update_in(&mut vcx.clone(), |m, window, cx| {
            m.io.task_search
                .update(cx, |s, cx| s.set_value("磁盘".to_string(), window, cx));
            cx.notify();
        });
        vcx.run_until_parked();
        assert!(
            vcx.debug_bounds("task-head-t-b").is_some(),
            "搜「磁盘」把匹配的那条也藏了"
        );
        assert!(
            vcx.debug_bounds("task-head-t-a").is_none(),
            "搜「磁盘」时「早间巡检」还在列表里：关键字没有被用来过滤"
        );

        // 清空关键字必须回到全量：过滤状态若粘住，用户会以为任务丢了
        main.update_in(&mut vcx.clone(), |m, window, cx| {
            m.io.task_search
                .update(cx, |s, cx| s.set_value(String::new(), window, cx));
            cx.notify();
        });
        vcx.run_until_parked();
        assert!(
            vcx.debug_bounds("task-head-t-a").is_some(),
            "清空搜索框后列表没有回到全量"
        );
    }
}
