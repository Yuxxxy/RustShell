//! 终端会话面板：活动会话列表 + 关闭动作，点击切换激活 Tab。

use gpui::{
    div, prelude::FluentBuilder as _, px, ClickEvent, Context, InteractiveElement as _,
    IntoElement, ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _,
};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    h_flex,
    menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem},
    scroll::ScrollableElement as _,
    v_flex, Icon, IconName, Sizable as _,
};
use rustshell_core::session::{SessionKind, SessionState};

use super::main_view::MainView;

/// N8：样式取色统一走主题工具函数（删除就地 hex 定义）
use crate::theme::to_hsla as hex;

pub fn render(m: &mut MainView, cx: &mut Context<MainView>) -> impl IntoElement {
    let t = crate::theme::current(cx);

    let mut root = v_flex()
        .id("terminals-panel")
        .w_full()
        .h_full()
        .overflow_y_scrollbar()
        .child(
            div()
                .px_3()
                .py_2()
                .text_size(px(11.))
                .text_color(hex(t.fg_muted))
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(rustshell_core::i18n::t("终端"))
                        .child(SharedString::from(format!("{}", m.tabs_state.tabs.len()))),
                ),
        );

    let rows: Vec<(String, String, bool, bool, SessionKind)> = m
        .tabs_state
        .tabs
        .iter()
        .map(|t| {
            (
                t.session.id.clone(),
                t.session.title.clone(),
                t.session.state == SessionState::Connected,
                m.tabs_state.active.as_deref() == Some(&t.session.id.clone()),
                t.session.kind,
            )
        })
        .collect();

    for (id, title, connected, is_active, kind) in rows {
        let kind_label = match kind {
            SessionKind::Local => rustshell_core::i18n::t("本地"),
            SessionKind::Ssh => "SSH",
        };
        let id_click = id.clone();
        let id_menu = id.clone();
        root = root.child(
            div()
                .id(SharedString::from(format!("term-{id}")))
                .flex()
                .items_center()
                .gap_2()
                .px_3()
                .py_1()
                .ml_1()
                .mr_1()
                .rounded(px(6.))
                // 卡片底色是 `bg_base`：激活 / hover 都取 `bg_elevated`，
                // 浅色下 `bg_overlay` 与底色同为纯白（2026-10-22 内外分层调整）。
                .when(is_active, |d| d.bg(hex(t.bg_elevated)))
                .hover(|s| s.bg(hex(t.bg_elevated)))
                .on_click(move |_: &ClickEvent, _w, cx| {
                    let id = id_click.clone();
                    let Some(hub) = cx.try_global::<super::main_view::UiHub>() else {
                        return;
                    };
                    let main = hub.main.clone();
                    let _ = main.update(cx, |m, cx| m.activate_session(&id, cx));
                })
                .context_menu(move |menu: PopupMenu, _w, _cx| {
                    let id = id_menu.clone();
                    menu.item(
                        PopupMenuItem::new(rustshell_core::i18n::t("关闭")).on_click(
                            move |_, _w, cx| {
                                let id = id.clone();
                                let Some(hub) = cx.try_global::<super::main_view::UiHub>() else {
                                    return;
                                };
                                let main = hub.main.clone();
                                let _ = main.update(cx, |m, cx| {
                                    if let Some(idx) =
                                        m.tabs_state.tabs.iter().position(|t| t.session.id == id)
                                    {
                                        m.close_tab(idx, cx);
                                    }
                                });
                            },
                        ),
                    )
                })
                .child(if connected {
                    div()
                        .w(px(6.))
                        .h(px(6.))
                        .flex_shrink_0()
                        .rounded_full()
                        .bg(hex(t.success))
                } else {
                    div()
                        .w(px(6.))
                        .h(px(6.))
                        .flex_shrink_0()
                        .rounded_full()
                        .border_1()
                        .border_color(hex(t.fg_muted))
                })
                .child(
                    Icon::new(IconName::SquareTerminal)
                        .small()
                        .text_color(hex(t.fg_muted)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(px(12.))
                        .text_color(if is_active {
                            hex(t.fg_primary)
                        } else {
                            hex(t.fg_muted)
                        })
                        .text_ellipsis()
                        .child(SharedString::from(title)),
                )
                .child(
                    div()
                        .text_size(px(10.))
                        .text_color(hex(t.fg_muted))
                        .child(kind_label),
                )
                // 所有会话均提供关闭按钮（防误关交由「固定 Tab」机制）
                .child(
                    div().child(
                        Button::new(SharedString::from(format!("term-close-{id}")))
                            .ghost()
                            .xsmall()
                            .icon(IconName::Close)
                            .tooltip(rustshell_core::i18n::t("关闭"))
                            .on_click(cx.listener(move |m, _e, _w, cx| {
                                if let Some(idx) =
                                    m.tabs_state.tabs.iter().position(|t| t.session.id == id)
                                {
                                    m.close_tab(idx, cx);
                                }
                            })),
                    ),
                ),
        );
    }

    if m.tabs_state.tabs.is_empty() {
        root = root.child(
            div()
                .px_3()
                .py_2()
                .text_size(px(12.))
                .text_color(hex(t.fg_muted))
                .child(rustshell_core::i18n::t("暂无活动会话")),
        );
    }

    // 底部动作
    root.child(
        div().px_3().py_2().mt_auto().child(
            Button::new("term-new-local")
                .small()
                .outline()
                .icon(IconName::Plus)
                .label(rustshell_core::i18n::t("本地 Shell"))
                .on_click(cx.listener(|m, _e, _w, cx| m.open_local_shell(cx))),
        ),
    )
}
