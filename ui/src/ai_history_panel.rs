//! 聊天记录面板：终端右侧对话正文对应的**会话目录**（左侧栏 `PanelKind::Ai`）。
//!
//! 2026-09-18 运行反馈「点击 AI 助手，聊天界面显示在终端右侧，原 AI 助手侧边栏改为
//! 聊天记录，支持删除，也是圆角」：这一列从「对话正文」改成了「会话目录」——
//! 正文搬到终端右侧（[`crate::ai_panel`]），左侧只剩三件事：
//!
//! 1. **新建一条**（头部 `+`，动作是 `MainView::ai_new_chat`）；
//! 2. **切到某条**（点行，`MainView::ai_switch_chat`）；
//! 3. **删掉某条**（行尾垃圾桶，`MainView::ai_delete_chat`）。
//!
//! 圆角与底色由外层卡片壳（`MainView::render` 的 `card_shell`）统一提供，
//! 本面板只负责内容 —— 自己再画一遍底色会在卡片圆角处露出一圈直角内衬。

use gpui::{
    div, prelude::FluentBuilder as _, px, Context, InteractiveElement as _, IntoElement,
    ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _,
};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    h_flex,
    scroll::ScrollableElement as _,
    v_flex, Disableable as _, Icon, Sizable as _,
};

/// 全量 Lucide 目录（`gpui_component::IconName` 只是组件默认子集，
/// 不含 `Trash` / `MessageSquare`；详见 `ai_panel.rs` 同一别名的说明）。
use gpui_kit_assets::IconName as LucideIcon;

use super::main_view::MainView;

/// N8：样式取色统一走主题工具函数（删除就地 hex 定义）
use crate::theme::to_hsla as hex;

/// 列表行圆角（比卡片壳的 8px 小一档：行是卡片**内部**的元素，
/// 与卡片同圆角会显得「行也是卡片」）。
const ROW_RADIUS: f32 = 6.;

pub fn render(m: &mut MainView, cx: &mut Context<MainView>) -> impl IntoElement {
    let t = crate::theme::current(cx);
    // 生成中既不能切也不能删：在途答复按「当前会话的最后一条消息」收口
    // （见 `MainView::ai_poll_results`），会话一换，增量就写进另一条里了。
    // 两个动作在后端各自也有一道拦截，这里置灰只是把原因摆到用户眼前。
    let busy = m.ai_ui.ai_busy;
    let current = m.ai_ui.ai_chat_id;

    // 行的渲染数据先快照成纯值：点击回调要 `'static`（跨帧存活），
    // 而 `ai_chats` 在回调里还会被改（切换 / 删除）。与 `terminals_panel`
    // 同一手法 —— 渲染期不再借 `m`，回调只认快照里那个 id。
    //
    // 排序也在快照时做：最近改动的排最上面。`updated_at` 是秒级精度，
    // 同一秒内改动的两条必须再有二级键（id 倒序），否则「刚发一条」
    // 之后两条会随机换位 —— 用户看到的就是列表自己在抖。
    let mut rows: Vec<(u64, u64, String, bool, bool)> = m
        .ai_ui
        .ai_chats
        .iter()
        .map(|c| {
            (
                c.updated_at,
                c.id,
                c.title.clone(),
                c.id == current,
                c.is_empty(),
            )
        })
        .collect();
    rows.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));

    // 「只剩一条空会话」时删除按钮无意义：`ai_delete_chat` 删完会立刻补一条空白
    // （不变量 `ai_chats` 恒非空），点下去界面纹丝不动 —— 那是比置灰更差的反馈。
    let sole_empty = rows.len() == 1 && rows[0].4;

    v_flex()
        .id("ai-history-panel")
        .w_full()
        .h_full()
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
                        .child(rustshell_core::i18n::t("聊天记录")),
                )
                .child(
                    Button::new("ai-history-new")
                        .ghost()
                        .xsmall()
                        .icon(LucideIcon::Plus)
                        .tooltip(rustshell_core::i18n::t("新建对话"))
                        .on_click(cx.listener(|m, _e, _w, cx| {
                            m.ai_new_chat(cx);
                        })),
                ),
        )
        .child(
            v_flex()
                .id("ai-history-list")
                .flex_1()
                .min_h_0()
                .w_full()
                .gap_0p5()
                .py_1()
                .overflow_y_scrollbar()
                .children(rows.into_iter().enumerate().map(
                    |(row_ix, (_updated, id, title, is_current, _empty))| {
                        h_flex()
                            .id(SharedString::from(format!("ai-chat-{id}")))
                            // 测试钩子（release 下被 `#[cfg]` 摘掉）：让用例能确认
                            // 「这一行确实被渲染出来了」。下标是**排序后**的位置，
                            // 不是会话 id —— `debug_bounds` 只认 `&'static str`，
                            // 用例那边写不出 `format!` 出来的 selector。
                            .debug_selector(move || format!("ai-chat-row-{row_ix}"))
                            .w_full()
                            .items_center()
                            .gap_2()
                            .mx_1()
                            .px_2()
                            .py_1()
                            .rounded(px(ROW_RADIUS))
                            .when(is_current, |d| d.bg(hex(t.bg_overlay)))
                            .hover(|s| s.bg(hex(t.bg_overlay)))
                            .on_click(cx.listener(move |m, _e, _w, cx| {
                                m.ai_switch_chat(id, cx);
                            }))
                            .child(Icon::new(LucideIcon::MessageSquare).xsmall().text_color(
                                if is_current {
                                    hex(t.accent)
                                } else {
                                    hex(t.fg_muted)
                                },
                            ))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(12.))
                                    .text_color(if is_current {
                                        hex(t.fg_primary)
                                    } else {
                                        hex(t.fg_muted)
                                    })
                                    .child(SharedString::from(title)),
                            )
                            .child(
                                Button::new(SharedString::from(format!("ai-chat-del-{id}")))
                                    .ghost()
                                    .xsmall()
                                    .icon(LucideIcon::Trash)
                                    .tooltip(rustshell_core::i18n::t("删除对话"))
                                    .disabled(busy || sole_empty)
                                    .on_click(cx.listener(move |m, _e, _w, cx| {
                                        m.ai_delete_chat(id, cx);
                                    })),
                            )
                    },
                )),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::prod;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// 生产代码正文（剥注释 + 切测试模块，见 `crate::test_support`）。
    fn production_source() -> String {
        prod(include_str!("ai_history_panel.rs"))
    }

    /// 回归（2026-09-18 运行反馈「原 AI 助手侧边栏改为聊天记录，支持删除」）：
    ///
    /// 这一列必须**同时**具备三个动作，缺一个都等于「用户没法用」：
    /// 新建（否则删到只剩一条就没法再开新会话）、切换（点行）、删除（行尾）。
    /// 另外两处「不该出现」的针：正文渲染的 `MessageScroller`（对话正文在终端右侧，
    /// 左侧再挂一份就是两棵树同时跟进同一条会话）与自带底色（圆角由卡片壳给）。
    #[test]
    fn history_panel_offers_new_switch_and_delete() {
        let code = production_source();
        for needle in [
            "\"ai-history-new\"", // 新建入口
            "ai_switch_chat",     // 点行切换
            "ai_delete_chat",     // 行尾删除
            "\"聊天记录\"",       // 栏名
        ] {
            assert!(code.contains(needle), "聊天记录面板缺少 `{needle}`。");
        }
        assert!(
            !code.contains("MessageScroller"),
            "对话正文只在终端右侧渲染（`ai_panel`）；左侧这一列是会话目录。"
        );
        assert!(
            !code.contains(".bg(crate::theme::surface"),
            "面板不自带底色 —— 底色与圆角由 `MainView` 的卡片壳提供，\
             叠两遍会在卡片圆角处露出一圈直角内衬。"
        );
    }

    /// 会话列表在真实装配（Root → AppShell → MainView）下必须逐条渲染出来。
    ///
    /// 只有一个会话时「列表没跑起来」和「列表跑起来了」在肉眼上都是「一行」，
    /// 所以这里刻意造**两条**，并断言两条行的 debug_bounds 都非零高度：
    /// 行高塌成 0、列表被 flex 挤没、或者快照排序把行丢了，都会在这里当场红。
    #[gpui::test]
    fn history_panel_renders_one_row_per_chat(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

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
                let shell = cx.new(|_| crate::main_view::AppShell::new(main));
                gpui_component::Root::new(shell, window, cx)
            })
        };
        vcx.run_until_parked();
        let main = slot.borrow().clone().expect("MainView 未创建");

        main.update_in(&mut vcx.clone(), |m, _w, cx| {
            m.panel_ui.panel = crate::main_view::PanelKind::Ai;
            m.panel_ui.sidebar_collapsed = false;
            // 第一条要「已有内容」，否则 `ai_new_chat` 判定当前会话为空白后
            // 直接复用它 —— 那样只会有一条会话，两条行的断言就落空了。
            m.ai_msgs_mut().push(rustshell_core::ai::ChatMessage::user(
                1,
                "[用户]\n磁盘满了吗",
                0,
            ));
            m.ai_ui.ai_scroller.update(cx, |s, cx| s.reset(1, cx));
            m.ai_new_chat(cx);
            cx.notify();
        });
        vcx.run_until_parked();

        assert_eq!(
            main.read_with(vcx, |m, _| m.ai_ui.ai_chats.len()),
            2,
            "前置条件没成立：应当有两条会话（一条有内容 + 一条新建）。"
        );
        // selector 必须写成字面量：`debug_bounds` 只收 `&'static str`
        // （见 gpui-pre `test_context.rs`），`format!` 出来的串传不进去。
        for (row_ix, sel) in [(0, "ai-chat-row-0"), (1, "ai-chat-row-1")] {
            let row = vcx
                .debug_bounds(sel)
                .unwrap_or_else(|| panic!("列表第 {row_ix} 行没有渲染出来（列表塌成 0 行？）"));
            assert!(
                f32::from(row.size.height) > 0.,
                "列表第 {row_ix} 行高度为 0（列表塌陷）"
            );
        }
    }
}
