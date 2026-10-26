//! 对话域渲染：连接对话框 / 快捷命令对话框 / SFTP 删除确认与「新建目录·重命名」/
//! 设置面板 / 首次信任（known_hosts）对话框。
//!
//! 注：快捷命令的「执行确认」对话框已按决策整体移除，
//! 快捷命令不再做风险判定，点击即发。
//!
//! 本模块为 `main_view` 的子模块，故可直接访问 `MainView` 的私有字段与方法，
//! 无需为其放宽可见性（Rust 隐私规则：父模块私有项对子模块可见）。

use gpui::{
    div, prelude::FluentBuilder as _, px, AppContext as _, Context, Entity,
    InteractiveElement as _, IntoElement, ParentElement as _, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window,
};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    dialog::{Dialog, DialogButtonProps},
    h_flex,
    input::{Editor, Input, InputState, Textarea, TextareaState},
    scroll::ScrollableElement as _,
    v_flex, Disableable as _, Icon, IconName, Sizable as _, WindowExt as _,
};

use rustshell_core::connections::ConnectionProfile;
use rustshell_platform::paths;

use super::{AuthMode, DialogMode, MainView};
use crate::terminal_view::TerminalPaneConfig;
use crate::theme::to_hsla as hex;

/// 设置面板拖拽的载荷类型：只作类型标识（`on_drag_move::<T>` 按 `T` 匹配），不承载数据。
pub(super) struct SettingsDrag;

/// 拖拽预览视图：面板本体跟着位移，光标下无需额外元素。
struct DragGhost;

impl gpui::Render for DragGhost {
    fn render(&mut self, _w: &mut Window, _cx: &mut Context<Self>) -> impl gpui::IntoElement {
        div()
    }
}

/// 远端名字预校验的失败原因 → 用户文案。
///
/// 映射放在 UI 侧而不是让 core 直接返回字符串：core 只负责「这个输入必然失败」
/// 这个判断（纯函数、可单测），怎么说给人听是界面的事（还要过 i18n 词条表）。
fn sftp_name_check_text(why: rustshell_core::sftp::NameCheck) -> &'static str {
    use rustshell_core::sftp::NameCheck;
    rustshell_core::i18n::t(match why {
        NameCheck::Empty => "名称不能为空",
        NameCheck::ContainsSlash => "名称不能包含斜杠",
        NameCheck::Reserved => "名称不能是 . 或 ..",
    })
}

impl MainView {
    pub(super) fn present_dialog(
        &mut self,
        mode: DialogMode,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 守卫读官方 dialog 状态，而不是业务标记`self.dialog`。
        // 官方 Dialog 默认 overlay_closable / close_button / keyboard 全开，
        // 点遮罩、点右上角 X、按 ESC 都会直接关闭；这些路径不会经过
        //`button_props.on_cancel`（那只覆盖「取消按钮」）。若用业务标记守卫，
        // 一旦用户用上述方式关闭过一次，标记就永久停在 Some(..)，此后
        // 设置/新建连接等所有对话框都被静默挡掉，即「设置按钮点了没反应」。
        if w.has_active_dialog(cx) {
            return;
        }
        self.dialog = Some(mode.clone());
        let view = cx.entity();
        w.open_dialog(cx, move |dialog, w, cx| {
            let mode = mode.clone();
            let view_on_close = view.clone();
            view.update(cx, |m, cx| m.render_dialog_by_mode(mode, dialog, w, cx))
                // 官方`on_close`在任意关闭路径后触发（ESC / 点遮罩 /
                // 右上角 X / 取消按钮），统一回收业务状态，杜绝对话框永久锁死。
                // 设置面板在这条路径上同样回滚快照（保存过则快照已清空）。
                .on_close(move |_ev, w, cx| {
                    view_on_close.update(cx, |m, cx| {
                        if matches!(m.dialog, Some(DialogMode::Settings)) {
                            m.revert_settings(w, cx);
                        }
                        // F-9：恢复提示被任何路径关掉（ESC / 点遮罩 / 右上角 X /
                        // 「不恢复」按钮）都等于「这次不恢复」—— 这里必须解除
                        // 「不许写快照」的封锁，否则它会一直挂到进程结束。
                        // 已点「恢复」时 `pending` 已被取走，是空操作。
                        if matches!(m.dialog, Some(DialogMode::RestoreSession)) {
                            m.layout_decline();
                        }
                        // F-5：粘贴确认被任何路径关掉（ESC / 点遮罩 / 右上角 X /
                        // 「取消」按钮）都等于「这次不粘」—— 必须把终端上挂起的
                        // 请求丢掉，否则它会一直挂着，下次渲染又弹一次框
                        //（「关了又回来」），而且一直攥着一份用户已经放弃的文本。
                        // 已点「粘贴」时挂起已被取走，是空操作。
                        if matches!(m.dialog, Some(DialogMode::PasteConfirm { .. })) {
                            m.paste_prompt_decline(cx);
                        }
                        m.dialog = None;
                    });
                })
        });
        cx.notify();
    }

    fn render_dialog_by_mode(
        &mut self,
        mode: DialogMode,
        dialog: Dialog,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) -> Dialog {
        match mode {
            DialogMode::New | DialogMode::Edit(_) => self.render_dialog(dialog, w, cx),
            DialogMode::NewGroup(_) | DialogMode::RenameConn(_) => {
                self.render_group_name_dialog(dialog, w, cx)
            }
            DialogMode::Settings => self.render_settings_dialog(dialog, w, cx),
            DialogMode::QcNew(_) | DialogMode::QcEdit(_, _) => self.render_qc_dialog(dialog, w, cx),
            DialogMode::SftpRemove { .. } => self.render_sftp_remove_dialog(dialog, w, cx),
            // 「新建目录 / 重命名」共用一套「收一个名字」的对话框正文
            // （`GroupNameDialogView`，与新建分组同一手法：输入框归正文视图自持，
            // 击键只重排正文而不是整窗口）
            DialogMode::SftpMkdir { .. } | DialogMode::SftpRename { .. } => {
                self.render_sftp_name_dialog(dialog, w, cx)
            }
            // 任务 id 要往下传：对话框根节点用它挂 `debug_selector`
            // （用例借此确认「点的是哪一条就开哪一条」），并在草稿与 id
            // 不一致时拒绝渲染，避免张冠李戴。
            DialogMode::TaskEdit(id) => self.render_task_dialog(dialog, &id, w, cx),
            // 上次会话恢复提示（F-9）：正文的两个数与勾选态都在 `layout_state` 里
            DialogMode::RestoreSession => self.render_restore_session_dialog(dialog, w, cx),
            // 粘贴确认（F-5）：正文现取自终端实体上的挂起请求（唯一真值）
            DialogMode::PasteConfirm { session } => {
                self.render_paste_confirm_dialog(dialog, &session, w, cx)
            }
        }
    }

    /// 打开 OpenSSH 导入预览层（F-1）。
    ///
    /// **直接走 `w.open_dialog` 而不是 `present_dialog`**：后者开头有
    /// `has_active_dialog` 守卫（挡的是「重复开同一扇窗」），而这里恰恰要允许
    /// 从设置面板**之上**再叠一层（设置 → 导入导出 → 从 OpenSSH 配置导入…）。
    /// 官方对话框层是栈：逐层渲染、只给最上层画遮罩、关闭只弹栈顶回上一层的焦点，
    /// 设置面板会原样留在下面（未保存的表单改动也就不会被「关窗」顺手回滚）。
    /// 与「免确认白名单」(`open_ai_whitelist_dialog`) 是同一手法。
    pub(super) fn present_openssh_import_dialog(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        self.conn_ui.openssh_dialog_open = true;
        let view = cx.entity();
        w.open_dialog(cx, move |dialog, w, cx| {
            let view = view.clone();
            let view_on_close = view.clone();
            view.update(cx, |m, cx| m.render_openssh_import_dialog(dialog, w, cx))
                // 覆盖全部关闭路径（取消按钮 / ESC / 点遮罩 / 右上角 X），
                // 业务标记与官方栈同步复位，避免「关了但标记还在」把后续
                // 「选择文件…」当成「层还开着」而只 notify 不重开。
                .on_close(move |_ev, _w, cx| {
                    view_on_close.update(cx, |m, _cx| m.conn_ui.openssh_dialog_open = false);
                })
        });
        cx.notify();
    }

    /// 收起 OpenSSH 预览层（导入完成 / 取消）：**只弹栈顶**。
    ///
    /// 不能用 `close_dialog_ui`：那个按 `self.dialog` 的单槽语义工作，会把下面的
    /// 设置面板（`Some(Settings)`）一并当成自己关掉、并在 `Some(Settings)` 时
    /// 回滚设置快照 —— 而用户只是从预览回到设置，什么都没取消。
    pub(super) fn close_openssh_dialog(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        w.close_dialog(cx);
        cx.notify();
    }

    /// OpenSSH `~/.ssh/config` 导入预览（F-1）。
    ///
    /// 弹层本身不持状态：条目与勾选位都在 `MainView::{openssh_plans, openssh_selected}`，
    /// 这里只按当前状态渲染。`重复` / `跳过` 项置灰且不可勾（勾了也不会被导入，
    /// 让它们可勾只会制造「我明明选了却没导进去」的困惑）。
    fn render_openssh_import_dialog(
        &mut self,
        dialog: Dialog,
        _w: &mut Window,
        cx: &mut Context<Self>,
    ) -> Dialog {
        use rustshell_core::openssh::{EntryStatus, SkipReason};
        let t = crate::theme::current(cx);

        let header = h_flex().gap_2().items_center().child(
            div()
                .text_size(px(14.))
                .font_weight(gpui::FontWeight::BOLD)
                .child(rustshell_core::i18n::t("从 OpenSSH 配置导入")),
        );

        let new_n = self
            .conn_ui
            .openssh_plans
            .iter()
            .filter(|p| p.status.is_new())
            .count();
        let dup_n = self
            .conn_ui
            .openssh_plans
            .iter()
            .filter(|p| matches!(p.status, EntryStatus::Duplicate))
            .count();
        let skip_n = self
            .conn_ui
            .openssh_plans
            .len()
            .saturating_sub(new_n + dup_n);
        let total = self.conn_ui.openssh_plans.len();

        let toolbar = h_flex()
            .justify_between()
            .items_center()
            .gap_2()
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(hex(t.fg_secondary))
                    .child(rustshell_core::i18n::ts(
                        format!("共 {total} 条：新增 {new_n} / 重复 {dup_n} / 跳过 {skip_n}"),
                        format!(
                            "{total} total: {new_n} new / {dup_n} duplicate / {skip_n} skipped"
                        ),
                    )),
            )
            .child(
                h_flex()
                    .gap_2()
                    // 换源：默认 `~/.ssh/config` 不存在、或用户想看别的文件（备份、
                    // 容器里拷出来的 config）时的出口。选完不重开层，只刷新本层列表。
                    .child(
                        Button::new("openssh-pick-file")
                            .small()
                            .outline()
                            .label(rustshell_core::i18n::t("选择文件…"))
                            .on_click(cx.listener(|m, _e, _w, cx| m.import_openssh_pick_file(cx))),
                    )
                    .child(
                        Button::new("openssh-sel-all")
                            .small()
                            .outline()
                            .label(rustshell_core::i18n::t("全选"))
                            .on_click(cx.listener(|m, _e, _w, cx| m.openssh_select_all(true, cx))),
                    )
                    .child(
                        Button::new("openssh-sel-none")
                            .small()
                            .outline()
                            .label(rustshell_core::i18n::t("全不选"))
                            .on_click(cx.listener(|m, _e, _w, cx| m.openssh_select_all(false, cx))),
                    ),
            );

        let mut list = v_flex().gap_1().max_h(px(300.)).overflow_y_scrollbar();
        if self.conn_ui.openssh_plans.is_empty() {
            list = list.child(
                div()
                    .py_4()
                    .text_size(px(12.))
                    .text_color(hex(t.fg_muted))
                    .child(rustshell_core::i18n::t("没有解析到任何主机")),
            );
        } else {
            for (ix, plan) in self.conn_ui.openssh_plans.iter().enumerate() {
                let is_new = plan.status.is_new();
                let checked = self
                    .conn_ui
                    .openssh_selected
                    .get(ix)
                    .copied()
                    .unwrap_or(false);
                let status = match &plan.status {
                    EntryStatus::New => rustshell_core::i18n::t("新增").to_string(),
                    EntryStatus::Duplicate => {
                        rustshell_core::i18n::t("重复，导入时跳过").to_string()
                    }
                    EntryStatus::Skipped(SkipReason::MissingHost) => {
                        rustshell_core::i18n::t("缺少主机").to_string()
                    }
                    EntryStatus::Skipped(SkipReason::MissingUser) => {
                        rustshell_core::i18n::t("缺少用户名").to_string()
                    }
                };
                let status_color = if is_new { t.accent } else { t.fg_muted };
                list = list.child(
                    h_flex()
                        .items_center()
                        .gap_2()
                        .py_1()
                        .child(
                            gpui_component::checkbox::Checkbox::new(SharedString::from(format!(
                                "openssh-row-{ix}"
                            )))
                            .xsmall()
                            .checked(checked)
                            .disabled(!is_new)
                            .on_click(cx.listener(
                                move |m, _checked: &bool, _w, cx| m.openssh_toggle(ix, cx),
                            )),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_size(px(12.))
                                .text_color(hex(if is_new { t.fg_primary } else { t.fg_muted }))
                                .child(plan.entry.alias.clone()),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_size(px(11.))
                                .text_color(hex(t.fg_muted))
                                .child(plan.summary_line()),
                        )
                        .child(
                            div()
                                .w(px(96.))
                                .text_size(px(11.))
                                .text_color(hex(status_color))
                                .child(status),
                        ),
                );
            }
        }

        let mut body = v_flex().gap_3().child(toolbar).child(list);
        if !self.conn_ui.openssh_warnings.is_empty() {
            let mut warn_col = v_flex().gap_1().child(
                div()
                    .text_size(px(11.))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(hex(t.fg_secondary))
                    .child(rustshell_core::i18n::t("解析提示")),
            );
            for msg in &self.conn_ui.openssh_warnings {
                warn_col = warn_col.child(
                    div()
                        .text_size(px(11.))
                        .text_color(hex(t.fg_muted))
                        .child(msg.clone()),
                );
            }
            body = body.child(warn_col);
        }
        if !self.conn_ui.openssh_source.is_empty() {
            body = body.child(
                div()
                    .text_size(px(10.))
                    .text_color(hex(t.fg_muted))
                    .child(self.conn_ui.openssh_source.clone()),
            );
        }

        let n = self.openssh_selected_new_count();
        let view = cx.entity();
        let view_ok = view.clone();
        let footer = h_flex()
            .gap_2()
            .justify_end()
            .pt_1()
            .child(
                Button::new("openssh-cancel")
                    .small()
                    .label(rustshell_core::i18n::t("取消"))
                    .on_click(cx.listener(|m, _e, w, cx| m.close_openssh_dialog(w, cx))),
            )
            .child(
                Button::new("openssh-import")
                    .small()
                    .primary()
                    .disabled(n == 0)
                    .label(rustshell_core::i18n::ts(
                        format!("导入选中 ({n})"),
                        format!("Import selected ({n})"),
                    ))
                    .on_click(cx.listener(|m, _e, w, cx| m.openssh_commit(w, cx))),
            );

        dialog
            .w(px(560.))
            .bg(hex(t.bg_overlay))
            .title(header)
            .child(body)
            .footer(footer)
            .button_props(
                DialogButtonProps::default()
                    .on_ok(move |_, w, cx| {
                        // Enter 走官方 `Confirm`，镜像「导入选中」按钮
                        view_ok.update(cx, |m, cx| m.openssh_commit(w, cx));
                        false
                    })
                    .on_cancel(move |_, w, cx| {
                        view.update(cx, |m, cx| m.close_openssh_dialog(w, cx));
                        true
                    }),
            )
    }

    /// 关闭对话框（取消 / 保存成功共用）。`pub(crate)`：重命名连接的落库路径在
    /// [`super::connections`] 子模块里，需要与分组改名走同一个关闭口径
    /// （设置面板的「取消即回滚」只在这里处理）。
    pub(crate) fn close_dialog_ui(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        // 设置面板的「取消」分支在此统一回滚到打开时的快照；
        // 保存路径会先把`set_snapshot`清空，因此不会误回滚已保存的修改。
        if matches!(self.dialog, Some(DialogMode::Settings)) {
            self.revert_settings(w, cx);
        }
        self.dialog = None;
        w.close_dialog(cx);
        cx.notify();
    }

    fn render_dialog(&mut self, dialog: Dialog, w: &mut Window, cx: &mut Context<Self>) -> Dialog {
        use gpui::Styled as _;
        let t = crate::theme::current(cx);
        let editing = matches!(self.dialog, Some(DialogMode::Edit(_)));

        // 「选择私钥文件」是异步的（`prompt_for_paths`），回调里只有 `AsyncApp` ——
        // 而写 `InputState` 需要 `Window`。按本仓既定的两段式兑现：回调置旗，
        // 这里消费。旗子只在真的选到文件时被置上（取消 = 什么都不做）。
        if let Some(picked) = self.io.dlg_key_pick.take() {
            self.io
                .dlg_key_path
                .update(cx, |s, cx| s.set_value(picked, w, cx));
        }

        // 标签在上、输入在下的字段列（现代表单布局；required 显示红色星号）
        let field = |label: &'static str, input: &Entity<InputState>, required: bool| {
            let label = rustshell_core::i18n::t(label);
            v_flex()
                .gap_1()
                .flex_1()
                .min_w_0()
                .child(
                    div()
                        .text_size(px(11.))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(hex(t.fg_secondary))
                        .child(if required {
                            SharedString::from(format!("{label} *"))
                        } else {
                            SharedString::from(label.to_string())
                        }),
                )
                .child(Input::new(input).w_full())
        };
        // 固定宽度的字段列（端口等短输入）
        let field_w = |label: &'static str, input: &Entity<InputState>, w: f32| {
            v_flex()
                .gap_1()
                .w(px(w))
                .flex_none()
                .child(
                    div()
                        .text_size(px(11.))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(hex(t.fg_secondary))
                        .child(SharedString::from(rustshell_core::i18n::t(label))),
                )
                .child(Input::new(input).w_full())
        };

        // 多行字段列（备注）：与 `field` 同一版式，控件换成官方 `Textarea`。
        // 高度不写死 —— `TextareaState` 建的时候用了 `auto_grow(2, 6)`（默认 2 行），
        // 由内容决定 2–6 行，这里再压一个高度只会和它打架。
        let field_area = |label: &'static str, input: &Entity<TextareaState>| {
            v_flex()
                .gap_1()
                .flex_1()
                .min_w_0()
                .child(
                    div()
                        .text_size(px(11.))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(hex(t.fg_secondary))
                        .child(SharedString::from(rustshell_core::i18n::t(label))),
                )
                .child(Textarea::new(input).w_full())
        };

        // 头部：图标 + 标题 + 副标题（官方 Dialog title 区）
        let header = h_flex()
            .items_center()
            .gap_3()
            .pb_3()
            .border_b_1()
            .border_color(hex(t.border_subtle))
            .child(
                div()
                    .w(px(34.))
                    .h(px(34.))
                    .rounded(px(8.))
                    .bg(hex(t.accent).opacity(0.14))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(Icon::new(IconName::SquareTerminal).text_color(hex(t.accent))),
            )
            .child(
                v_flex().child(
                    div()
                        .text_size(px(15.))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(hex(t.fg_primary))
                        // 注意：`t(..)` 只应套**一层**。此前写成
                        // `t(if editing { t("编辑连接") } else { t("新建连接") })`，
                        // 外层又对已译串再查一次表 —— 英文模式下 `t("New Connection")`
                        // 查不到词条，靠 `translate` 的 `_ => zh` 兜底才没出事。
                        // 直接按分支取一个 key，套一层翻译。
                        .child(rustshell_core::i18n::t(if editing {
                            "编辑连接"
                        } else {
                            "新建连接"
                        })),
                ),
            );
        // ---- 连接表单（必填 + 凭据 + 备注）----
        //
        // 2026-09-29 用户要求「隐藏新增连接-高级相关的内容，保持简单」：原先的
        // 「高级」页签与整页内容（端口转发 / 代理 / 会话日志 / 断线自动重连）
        // 不再渲染，对话框回到单页 —— 必填四项 + 凭据 + 私钥 + 备注，一屏看得完。
        //
        // 这些设置的**数据链路原样保留**：编辑时回填草稿、保存时原样写回，
        // 新建取默认。界面消失不等于把用户配过的隧道 / 代理静默清空
        // （清掉的话，用户要到「隧道怎么没了」那一刻才会发现）。
        //
        // 2026-10-21 用户再压一版（「增加认证方式，默认密码认证，主要目的是减少界面内容」）：
        // 版面按「目标 → 凭据」两段分组，中间用一块底色更暗的卡片隔开 ——
        //
        //   名称
        //   主机 | 端口
        //   ┌──────────── 凭据卡片（bg_base）────────────┐
        //   │ 用户名                                      │
        //   │ 认证方式：(•) 密码认证  ( ) 私钥认证        │
        //   │ 密码   ← 只有密码认证时出现                 │
        //   │ 私钥 + [选择…] ← 只有私钥认证时出现         │
        //   └─────────────────────────────────────────────┘
        //   备注（默认 2 行，可长到 6 行）
        //
        // 关键收益是**同时在场的东西变少**：密码与私钥不再各占一行（原先私钥那一块
        // 常驻，哪怕这条连接只用密码），一屏之内不再需要滚动。
        //
        // 分组归属同样不在表单里选（2026-09-23 用户要求默认不分组）：新建时取自
        // 上下文（分组右键「新建连接」），编辑时原样保留既有归属（改归属走连接树
        // 拖拽）。`dlg_group_id` 照旧是保存时的事实源，只是没有界面写它。
        let auth = self.io.dlg_auth;
        let key_now = self.io.dlg_key_path.read(cx).value().to_string();

        // 认证方式的两个选项：裸 `Radio`（圆圈）+ 整行可点。
        // 不用 `RadioGroup`：它是受控组，要额外维护下标状态；而这里只有两支，
        // 选中值就是 `dlg_auth` 本身（本文件既有的 `radio_row` 也是这个理由）。
        let auth_radio = |id: &'static str, label: &'static str, mode: AuthMode| {
            gpui_component::radio::Radio::new(id)
                .xsmall()
                .label(rustshell_core::i18n::t(label))
                .checked(auth == mode)
                .on_click(cx.listener(move |m, _checked, _w, cx| {
                    if m.io.dlg_auth != mode {
                        m.io.dlg_auth = mode;
                        // 切换方式**不清输入框**：用户可能只是想切过去看一眼再切回来，
                        // 清掉等于让他重敲一遍。各自的框只在保存时按选中侧取舍。
                        cx.notify();
                    }
                }))
        };
        let auth_row = v_flex()
            .gap_1()
            .child(
                div()
                    .text_size(px(11.))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(hex(t.fg_secondary))
                    .child(rustshell_core::i18n::t("认证方式")),
            )
            .child(
                h_flex()
                    .gap_4()
                    .child(auth_radio("dlg-auth-password", "密码认证", AuthMode::Password))
                    .child(auth_radio("dlg-auth-key", "私钥认证", AuthMode::Key)),
            );

        // 私钥路径那一行：输入框 + [选择…]（+ 有值时 [清除]）。只在私钥认证下渲染。
        let key_row = v_flex()
            .gap_1()
            .debug_selector(|| "dlg-key-row".to_string())
            .child(
                div()
                    .text_size(px(11.))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(hex(t.fg_secondary))
                    .child(rustshell_core::i18n::t("私钥文件")),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .w_full()
                    .child(Input::new(&self.io.dlg_key_path).w_full())
                    .child(
                        Button::new("dlg-key-browse")
                            .small()
                            .outline()
                            .flex_none()
                            .label(rustshell_core::i18n::t("选择…"))
                            .on_click(cx.listener(|_m, _e, _w, cx| {
                                // 系统「打开」对话框：结果经 `dlg_key_pick` 暂存，
                                // 由本函数开头兑现（异步回调里没有 `Window`）。
                                let rx = cx.prompt_for_paths(gpui::PathPromptOptions {
                                    files: true,
                                    directories: false,
                                    multiple: false,
                                    prompt: Some(rustshell_core::i18n::t("选择私钥文件").into()),
                                });
                                cx.spawn(async move |this, cx| {
                                    if let Ok(Ok(Some(paths))) = rx.await {
                                        if let Some(p) = paths.into_iter().next() {
                                            let _ = this.update(cx, |m, cx| {
                                                m.io.dlg_key_pick =
                                                    Some(p.to_string_lossy().to_string());
                                                cx.notify();
                                            });
                                        }
                                    }
                                })
                                .detach();
                            })),
                    )
                    .when(!key_now.is_empty(), |d| {
                        d.child(
                            Button::new("dlg-key-clear")
                                .small()
                                .ghost()
                                .flex_none()
                                .label(rustshell_core::i18n::t("清除"))
                                .on_click(cx.listener(|m, _e, w, cx| {
                                    m.io.dlg_key_path.update(cx, |s, cx| s.set_value("", w, cx));
                                })),
                        )
                    }),
            );

        // 凭据区（用户名 / 认证方式 / 只露被选中那一侧的凭据）。
        //
        // 2026-10-21 用户先要求「移除凭据卡片」，再要求「去掉分割线」：这块现在是
        // **纯表单流** —— 无底色、无边框、无线，只靠父容器的 `gap_3` 与上面几行拉开
        // 距离。别再加任何包裹（卡片 / 分隔线 / 分组框），用户已连续撤了两版。
        let credentials = v_flex()
            .gap_3()
            .debug_selector(|| "dlg-connect-credentials".to_string())
            // 2026-10-21 用户要求「认证方式挪到用户名上面」：先选「怎么登录」，
            // 再填「以谁登录」—— 这两行读起来是一条判断链，顺序颠倒时，
            // 用户会先把用户名敲进去，再回头发现下面还有一组必选项。
            .child(auth_row)
            .child(field(
                rustshell_core::i18n::t("用户名"),
                &self.io.dlg_user,
                true,
            ))
            .when(auth == AuthMode::Password, |d| {
                // 整块包一层只为了挂量取钩子（`debug_bounds` 只认 debug_selector）：
                // 「密码认证下私钥那一行确实不在场」这件事，只有量得出来才守得住。
                d.child(
                    v_flex()
                        .gap_1()
                        .debug_selector(|| "dlg-pass-row".to_string())
                        // 口令一律记住，「密码已加密保存」那句灰字按用户要求撤掉了
                        // （2026-10-21），保存行为本身不变。
                        .child(field(
                            rustshell_core::i18n::t("密码"),
                            &self.io.dlg_pass,
                            false,
                        )),
                )
            })
            .when(auth == AuthMode::Key, |d| d.child(key_row));

        let connection_page = v_flex()
            .pt_2()
            .gap_3()
            // 量出来的钩子：撤掉「分组」一栏之后，只靠源码断言证明不了表单还画得出来
            // （`connect_dialog_renders_without_the_group_field` 量它的高度）。
            .debug_selector(|| "dlg-connect-form".to_string())
            .child(field(
                rustshell_core::i18n::t("名称"),
                &self.io.dlg_name,
                true,
            ))
            .child(
                h_flex()
                    .items_start()
                    .gap_2()
                    .child(field(
                        rustshell_core::i18n::t("主机"),
                        &self.io.dlg_host,
                        true,
                    ))
                    .child(field_w(
                        rustshell_core::i18n::t("端口"),
                        &self.io.dlg_port,
                        96.,
                    )),
            )
            // 凭据整体成一块卡片：用户名 / 认证方式 / 只露被选中那一侧的凭据
            .child(credentials)
            // 备注（2026-09-18 用户要求）：多行 textarea，可选。
            // 留在表单最后：它虽然不影响能不能连上，但它是**用户自己写给自己**
            // 的那句话（用途 / 跳板机 / 到期日），归在「这条连接是什么」一侧读得通。
            .child(field_area("备注", &self.io.dlg_notes));

        // 校验错误 / 连接测试结果：跟在表单下方。
        //
        // 它们回答的是「保存为什么失败」「上次测通没有」—— 两条提示必须与表单
        // 同屏，用户不必去找。校验错误就地提示：走官方`Alert`（error 变体），
        // 字号压到与表单正文同级。
        let mut notices = v_flex().gap_2().pt_2();
        if let Some(msg) = self.io.dlg_error.clone() {
            notices = notices.child(
                crate::notice::NoticeKind::Error
                    .card("dlg-connect-error", SharedString::from(msg))
                    .xsmall()
                    .text_size(px(11.5)),
            );
        }
        // 连接测试结果：成功 success / 失败 error 两个变体。
        // 与校验错误是**两个独立的槽**：测试失败不该把「名称未填」那条提示顶掉，
        // 反过来也一样 —— 用户需要同时看到「哪个字段没填」与「上次测试结果」。
        if let Some((ok, msg)) = self.io.dlg_test.clone() {
            let kind = if ok {
                crate::notice::NoticeKind::Success
            } else {
                crate::notice::NoticeKind::Error
            };
            notices = notices.child(
                kind.card("dlg-connect-test", SharedString::from(msg))
                    .xsmall()
                    .text_size(px(11.5)),
            );
        }

        // 组装：表单 → 提示槽。
        let body = v_flex().child(connection_page).child(notices);

        // 底部操作区：左「连接测试」 右「取消 / 保存」。
        //
        // 2026-09-22 用户要求去掉「保存并连接」。它与「保存」在落库这件事上一模一样，
        // 只差保存后多连一次 —— 而「要不要连」在对话框关掉之后随时可以再决定
        // （连接列表就在旁边）。留着它，每次保存都得先分辨两个按钮的区别。
        // 取而代之的是「连接测试」：它回答的是对话框里**真正**需要答案的那个问题 ——
        // 「这组凭据现在能不能登进去」，而且什么都不落库、什么都不留下
        // （见 `MainView::test_connection`）。
        let footer = h_flex()
            .items_center()
            .gap_2()
            .pt_1()
            .child(
                Button::new("dlg-test")
                    .small()
                    .outline()
                    .disabled(self.io.dlg_testing)
                    .label(rustshell_core::i18n::t(if self.io.dlg_testing {
                        rustshell_core::i18n::t("测试中…")
                    } else {
                        rustshell_core::i18n::t("连接测试")
                    }))
                    .on_click(cx.listener(|m, _e, _w, cx| m.test_connection(cx))),
            )
            // 撑开中间的空白：左边是「辅助动作」（测试），右边是「离开对话框」
            // （取消 / 保存）。两类动作混在一起排会让人误点。
            .child(div().flex_1())
            .child(
                Button::new("dlg-cancel")
                    .small()
                    .label(rustshell_core::i18n::t("取消"))
                    .on_click(cx.listener(|m, _e, w, cx| {
                        m.close_dialog_ui(w, cx);
                    })),
            )
            .child(
                Button::new("dlg-save")
                    .small()
                    .primary()
                    .label(rustshell_core::i18n::t("保存"))
                    .on_click(cx.listener(|m, _e, w, cx| {
                        m.dialog_save(w, cx);
                    })),
            );

        let view = cx.entity();
        let view_ok = view.clone();
        dialog
            // 宽度回到 460（此前为「高级」页加宽到 520）：多出的 60px 是给端口
            // 转发那行四五个控件的，「高级」页隐藏后，简单表单用原生宽度即可。
            .w(px(460.))
            .bg(hex(t.bg_overlay))
            .title(header)
            .child(body)
            .footer(footer)
            .button_props(
                DialogButtonProps::default()
                    .on_ok(move |_, w, cx| {
                        // Enter 走官方 `Confirm` 动作，只认 `button_props.on_ok`；
                        // 自绘 footer 时它仍会被调用，不设就等于「Enter 关窗但没保存」。
                        view_ok.update(cx, |m, cx| m.dialog_save(w, cx));
                        // 业务侧负责关窗（校验失败要保持对话框打开），故返回 false。
                        false
                    })
                    .on_cancel(move |_, w, cx| {
                        view.update(cx, |m, cx| m.close_dialog_ui(w, cx));
                        true
                    }),
            )
    }

    /// 「名称」对话框：新建分组 / 重命名分组 / 重命名**连接**三处共用
    /// （都只是「填一个名字再落库」，没有第二个字段）。
    ///
    /// 分组侧入口是连接树的右键菜单：空白处 = 新建，分组节点上 = 建在该分组**下面**，
    /// 分组菜单的「重命名…」= 改名。连接侧入口是连接行右键菜单的「重命名…」与 F2
    /// （2026-09-17 运行反馈）。它取代了连接对话框里那个「填名字就顺带建组」
    /// 的输入框 —— 建组与建连接是两件事，不该共用一个字段。
    /// 校验失败就地以 error 卡片显示，窗口保持打开。
    fn render_group_name_dialog(
        &mut self,
        dialog: Dialog,
        _w: &mut Window,
        cx: &mut Context<Self>,
    ) -> Dialog {
        use gpui::Styled as _;
        let t = crate::theme::current(cx);
        // 三种语义：给分组改名 / 新建分组 / 给连接改名
        let renaming_group = matches!(self.dialog, Some(DialogMode::NewGroup(Some(_))));
        let renaming_conn = matches!(self.dialog, Some(DialogMode::RenameConn(_)));
        let editing = renaming_group || renaming_conn;

        let header = div()
            .text_size(px(14.))
            .font_weight(gpui::FontWeight::BOLD)
            .child(if renaming_conn {
                rustshell_core::i18n::t("重命名连接")
            } else if renaming_group {
                rustshell_core::i18n::t("重命名分组")
            } else {
                rustshell_core::i18n::t("新建分组")
            });

        let footer = h_flex()
            .items_center()
            .justify_end()
            .gap_2()
            .pt_1()
            .child(
                Button::new("dlg-group-cancel")
                    .small()
                    .label(rustshell_core::i18n::t("取消"))
                    .on_click(cx.listener(|m, _e, w, cx| {
                        m.close_dialog_ui(w, cx);
                    })),
            )
            .child(
                Button::new("dlg-group-create")
                    .small()
                    .primary()
                    .label(if editing {
                        rustshell_core::i18n::t("保存")
                    } else {
                        rustshell_core::i18n::t("创建")
                    })
                    .on_click(cx.listener(move |m, _e, w, cx| {
                        if renaming_conn {
                            m.rename_connection_save(w, cx);
                        } else {
                            m.group_name_save(w, cx);
                        }
                    })),
            );

        let view = cx.entity();
        let view_ok = view.clone();
        dialog
            .w(px(380.))
            .bg(hex(t.bg_overlay))
            .title(header)
            // 正文走独立视图（`GroupNameDialogView`）：名称框每敲一个字只重排正文，
            // 不再把整窗口重排一遍
            .child(self.io.group_name_body.clone())
            .footer(footer)
            .button_props(
                DialogButtonProps::default()
                    .on_ok(move |_, w, cx| {
                        // Enter 走官方 `Confirm` 动作，只认 `button_props.on_ok`：
                        // 自绘 footer 时它仍会被调用，不设就等于「Enter 关窗但没落库」。
                        view_ok.update(cx, |m, cx| {
                            if renaming_conn {
                                m.rename_connection_save(w, cx);
                            } else {
                                m.group_name_save(w, cx);
                            }
                        });
                        // 业务侧负责关窗（校验失败要保持对话框打开），故返回 false。
                        false
                    })
                    .on_cancel(move |_, w, cx| {
                        view.update(cx, |m, cx| m.close_dialog_ui(w, cx));
                        true
                    }),
            )
    }

    /// 落库分组名：新建（插到锚点分组下面）/ 改名（改既有分组名），成功即关窗。
    ///
    /// 校验（非空 / 重名 / 目标还在不在）与落盘失败都把原因写进 `dlg_error`
    /// 并**保持对话框打开**；只有真正落库成功才关窗 —— 否则会出现「点了创建，
    /// 什么都没发生」。改名不动分组 id，故展开态与连接归属都自动保持。
    fn group_name_save(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        // 输入框归正文视图所有（见 `GroupNameDialogView`），这里只取值
        let name = self.io.group_name_body.read(cx).value(cx);
        let renaming = match &self.dialog {
            Some(DialogMode::NewGroup(target)) => target.clone(),
            _ => None,
        };
        if name.is_empty() {
            self.io.dlg_error = Some(rustshell_core::i18n::t("分组名不能为空").to_string());
            cx.notify();
            return;
        }
        // 重名判定要排除「自己」：改名成原来的名字（哪怕只是改了大小写/空白）不该被自己挡掉
        let dup = self
            .conn_store
            .groups
            .iter()
            .any(|g| g.name == name && Some(g.id.as_str()) != renaming.as_deref());
        if dup {
            self.io.dlg_error = Some(rustshell_core::i18n::t("分组名已存在").to_string());
            cx.notify();
            return;
        }

        let created_id = match &renaming {
            Some(id) if self.conn_store.group(id).is_none() => {
                self.io.dlg_error = Some(rustshell_core::i18n::t("该分组已不存在").to_string());
                cx.notify();
                return;
            }
            Some(id) => {
                self.conn_store.rename_group(id, &name);
                None
            }
            // 新分组：选了父分组就建到里面（「连接组里还有组」），否则落在锚点分组
            // 下面（空白处右键进来的锚点为 None 到 追加到末尾根级）
            None => {
                if let Some(p) = self.io.dlg_group_parent.as_deref() {
                    self.conn_store.add_subgroup(name, p)
                } else {
                    Some(
                        self.conn_store
                            .add_group_after(name, self.io.dlg_group_after.as_deref()),
                    )
                }
            }
        };

        if let Err(e) = self.conn_store.save(&paths::connections_path()) {
            // 内存里已改过分组，落盘失败必须让用户看见，否则下次启动会「莫名少了分组」。
            self.io.dlg_error = Some(rustshell_core::i18n::ts(
                format!("保存失败：{e}"),
                format!("Save failed: {e}"),
            ));
            cx.notify();
            return;
        }
        // 新分组默认展开：建的是子分组时父分组也得展开，否则新组藏在收起的父下看不见。
        //
        // 这里**不再**把新分组的 id 记成连接对话框的默认归属（旧行为）：表单里已经
        // 没有分组一栏，记下来的归属在界面上完全不可见，接着点「新建连接」就会
        // 「莫名落进某个文件夹」。归属只留一条可见来路 —— 在分组上右键「新建连接」。
        if let Some(id) = created_id {
            if let Some(p) = self.io.dlg_group_parent.clone() {
                self.conn_ui.expanded_groups.insert(p);
            }
            self.conn_ui.expanded_groups.insert(id);
        }
        self.io.dlg_group_after = None;
        self.io.dlg_group_parent = None;
        self.io.dlg_error = None;
        self.dialog = None;
        w.close_dialog(cx);
        cx.notify();
    }

    fn dialog_save(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        let name = self.io.dlg_name.read(cx).value().trim().to_string();
        let host = self.io.dlg_host.read(cx).value().trim().to_string();
        let user = self.io.dlg_user.read(cx).value().trim().to_string();
        let pass = self.io.dlg_pass.read(cx).value().to_string();
        // 私钥路径：trim 两侧空白。`~` 不在这里展开 —— 远端/本机的家目录语义不同，
        // 展开由真正用它的那一侧（russh 的 `load_secret_key` 之前的路径处理）决定。
        let key_path = self.io.dlg_key_path.read(cx).value().trim().to_string();
        // 备注：trim 两端空白但**保留内部换行**（多行正是它存在的理由）。
        // 口令不 trim（见上），两者口径不同是有意的。
        let notes = self.io.dlg_notes.read(cx).value().trim().to_string();

        // 表单校验：必填项 + 端口范围（错误就地显示在对话框内）
        let port_text = self.io.dlg_port.read(cx).value().trim().to_string();
        let port = match port_text.parse::<u16>() {
            Ok(p) if p > 0 => p,
            _ if port_text.is_empty() => 22,
            _ => {
                self.io.dlg_error =
                    Some(rustshell_core::i18n::t("端口需为 1–65535 的数字").to_string());
                cx.notify();
                return;
            }
        };
        if name.is_empty() || host.is_empty() || user.is_empty() {
            self.io.dlg_error =
                Some(rustshell_core::i18n::t("名称、主机、用户名为必填项").to_string());
            cx.notify();
            return;
        }
        // 认证方式（2026-10-21）：选了私钥就**必须**有路径 —— 否则落库一条
        // 「两种凭据都没有」的连接，用户要到点连接时才被 `connect_only` 拒绝。
        let auth = self.io.dlg_auth;
        if auth == AuthMode::Key && key_path.is_empty() {
            self.io.dlg_error = Some(rustshell_core::i18n::t("请选择私钥文件").to_string());
            cx.notify();
            return;
        }
        self.io.dlg_error = None;
        // 归属分组不再是表单里选出来的：**新建**时它是打开对话框那一刻的上下文
        // （分组右键「新建连接」= 那个分组的 id，其余入口 = None），**编辑**时它是
        // 这条连接既有的归属（原样写回）。引用一律是 id，分组改名不会丢归属。
        let group_id = self.io.dlg_group_id.clone();
        // 端口转发规则（F-3）在这里定稿。**先克隆出来**再用：编辑分支持着
        // `conn_store.profiles` 的可变借用，逐处去 clone `self` 的字段会撞借用检查。
        let forwards = self.io.dlg_forwards.clone();
        // 会话日志覆盖（F-7）同理：`Option<bool>` 是 Copy，取值不会撞借用。
        let session_log = self.io.dlg_session_log;
        // 断线自动重连覆盖（F-4）同理
        let auto_reconnect = self.io.dlg_auto_reconnect;

        // 口令的加密 key 用 profile id（非 Windows keyring 需稳定键，Windows DPAPI 忽略）；
        // New 模式先生成 id，保证密文引用与 profile 一一对应。
        // 这里先 clone 模式而不 take：加密失败要就地报错、把对话框留在原地。
        let mode = self.dialog.clone();
        let secret_key = match &mode {
            Some(DialogMode::Edit(id)) => id.clone(),
            Some(DialogMode::New) => rustshell_core::connections::uuid_like(),
            _ => String::new(),
        };
        let new_id = matches!(&mode, Some(DialogMode::New)).then(|| secret_key.clone());
        let prev_stored = match &mode {
            Some(DialogMode::Edit(id)) => self
                .conn_store
                .profile(id)
                .and_then(|p| p.stored_password.clone()),
            _ => None,
        };
        // 口令一律记住（「记住密码」开关已取消）：填了密码就加密落库。
        //
        // 编辑时留空 = **不改口令**（保留原密文），而不是「清除口令」：留空可能只是
        // 因为系统凭据解不开（换机 / DPAPI 失效），此时抹掉原密文等于悄悄毁掉
        // 用户当初存下的口令。要让用户能主动清除，得另加入口，不该靠「留空」暗示。
        //
        // 私钥认证下**不加密、也不保留**任何口令：这一支的语义是「这条连接用钥匙」，
        // 留着旧密文会让下次启动的 `restore_passwords` 把口令解回 `profile.password`，
        // 于是 core 那条「密码优先」会拿它去登录 —— 用户选了私钥却被密码挡回来。
        let stored = if auth == AuthMode::Key {
            None
        } else if pass.is_empty() {
            prev_stored
        } else if secret_key.is_empty() {
            None
        } else {
            match rustshell_platform::secret::protect(&secret_key, &pass) {
                Ok(blob) => Some(blob),
                Err(e) => {
                    // 不能静默降级成「没记住」：用户会到下次启动才发现口令没了
                    self.io.dlg_error = Some(rustshell_core::i18n::ts(
                        format!("口令未能加密保存：{e}"),
                        format!("The password could not be saved encrypted: {e}"),
                    ));
                    cx.notify();
                    return;
                }
            }
        };
        // 代理（F-10）在这里定稿：档位 + 输入框 → `ProxyChoice`。
        //
        // 「口令留空 = 不改口令」只在**目标没变**时成立（`may_inherit_password`）：
        // 代理换了类型 / 地址 / 端口之后，旧口令属于旧代理，不能跟着走 ——
        // 那既是误导（用户以为已经填过了），也可能是把凭据送给了另一个代理。
        let proxy = {
            let mut cfg = rustshell_core::proxy::ProxyConfig::default();
            // 非「自定义」档位**不碰输入框**：草稿可能只填了一半（用户切过去看一眼
            // 又切回来），那半个草稿不该拦住保存 —— `from_parts` 本来就会丢弃它。
            if self.io.dlg_proxy_mode == rustshell_core::proxy::ProxyMode::Custom {
                cfg = self.dlg_proxy_draft(cx);
                if let Some(e) = cfg.validate() {
                    self.io.dlg_error =
                        Some(rustshell_core::proxy::validate_error_text(e).to_string());
                    cx.notify();
                    return;
                }
                let typed = self.io.dlg_proxy_pass.read(cx).value().to_string();
                if !typed.is_empty() {
                    let key = rustshell_core::proxy::secret_key_for(&secret_key);
                    match rustshell_platform::secret::protect(&key, &typed) {
                        Ok(blob) => {
                            cfg.password_ref = Some(blob);
                            cfg.password = Some(typed);
                        }
                        Err(e) => {
                            self.io.dlg_error = Some(rustshell_core::i18n::ts(
                                format!("代理口令未能加密保存：{e}"),
                                format!("The proxy password could not be saved encrypted: {e}"),
                            ));
                            cx.notify();
                            return;
                        }
                    }
                } else {
                    let origin = self
                        .io
                        .dlg_proxy_prev
                        .as_ref()
                        .map(rustshell_core::proxy::fingerprint);
                    if rustshell_core::proxy::may_inherit_password(origin.as_ref(), &cfg) {
                        if let Some(prev) = &self.io.dlg_proxy_prev {
                            cfg.password_ref = prev.password_ref.clone();
                            // 明文也一并沿用：只写回密文的话，本次会话里这条连接
                            // 会带着空口令去敲代理（要等下次启动重解才有口令）。
                            cfg.password = prev.password.clone();
                        }
                    }
                }
            }
            rustshell_core::proxy::ProxyChoice::from_parts(self.io.dlg_proxy_mode, cfg)
        };
        let mode = self.dialog.take();
        // 凭据定稿：**只留被选中那一侧**（见 `credential_sides` 的说明）。
        // 这样 `core::ssh::connect_only` 的「密码优先，其次私钥」永远只有一支有值，
        // 用户在单选框里选的是什么，实际就用什么。
        let (cred_password, cred_key_path) = credential_sides(auth, &pass, &key_path);
        // 「保存之后接着连」的意图：由连接入口留下的旗（见字段文档）。
        // 在这里一次性取走 —— 不管这次保存走哪条分支，旗都不该留到下一次打开。
        let connect_after = std::mem::take(&mut self.io.dlg_connect_after);
        let mut connect_id: Option<String> = None;

        match mode {
            Some(DialogMode::SftpRemove { .. }) => {
                unreachable!("SftpRemove 走 render_sftp_remove_dialog，不经此处")
            }
            Some(DialogMode::SftpMkdir { .. }) | Some(DialogMode::SftpRename { .. }) => {
                unreachable!("SFTP 建目录/重命名走 render_sftp_name_dialog，不经此处")
            }
            Some(DialogMode::Edit(id)) => {
                if let Some(p) = self.conn_store.profiles.iter_mut().find(|p| p.id == id) {
                    p.name = name;
                    p.host = host;
                    p.port = port;
                    p.username = user;
                    p.group_id = group_id;
                    p.notes = notes;
                    p.password = cred_password.clone();
                    p.stored_password = stored;
                    // 私钥同理由认证方式决定：密码认证下**清掉**（这条连接不再用它），
                    // 私钥认证下已校验非空。
                    p.key_path = cred_key_path.clone();
                    p.forwards = forwards.clone();
                    p.session_log = session_log;
                    p.auto_reconnect = auto_reconnect;
                    p.proxy = proxy.clone();
                }
                if let Err(e) = self.conn_store.save(&paths::connections_path()) {
                    log::warn!("编辑连接落盘失败: {e}");
                }
                if connect_after {
                    connect_id = Some(id);
                }
            }
            Some(DialogMode::New) => {
                let mut p = ConnectionProfile::new(name, host, user);
                if let Some(nid) = &new_id {
                    p.id = nid.clone(); // 与 secret_key 一致（keyring 引用对齐）
                }
                p.port = port;
                p.group_id = group_id;
                p.notes = notes;
                p.password = cred_password;
                p.stored_password = stored;
                p.key_path = cred_key_path;
                p.forwards = forwards.clone();
                p.session_log = session_log;
                p.auto_reconnect = auto_reconnect;
                p.proxy = proxy;
                let id = p.id.clone();
                self.conn_store.add_profile(p);
                // 新连接落在某个分组里时把那个分组展开：默认落在 `default` 时尤其要紧，
                // 否则「保存成功」之后树上看不到任何变化（条目在收起的分组里），
                // 用户会以为保存失败又建一条。
                if let Some(gid) = self
                    .conn_store
                    .profile(&id)
                    .and_then(|p| p.group_id.clone())
                {
                    self.conn_ui.expanded_groups.insert(gid);
                }
                if let Err(e) = self.conn_store.save(&paths::connections_path()) {
                    log::warn!("新建连接落盘失败: {e}");
                }
                if connect_after {
                    connect_id = Some(id);
                }
            }
            Some(DialogMode::QcNew(_)) | Some(DialogMode::QcEdit(_, _)) => {
                unreachable!("QC 对话框走 render_qc_dialog / qc_save，不经此处")
            }
            Some(DialogMode::NewGroup(_)) => {
                unreachable!("「分组名」走 render_group_name_dialog / group_name_save，不经此处")
            }
            Some(DialogMode::RenameConn(_)) => {
                unreachable!(
                    "「重命名连接」走 render_group_name_dialog / rename_connection_save，不经此处"
                )
            }
            Some(DialogMode::Settings) => {
                unreachable!("Settings 走 render_settings_dialog / settings_save，不经此处")
            }
            Some(DialogMode::TaskEdit(_)) => {
                unreachable!("任务编辑走 render_task_dialog / task_draft_save，不经此处")
            }
            // 恢复提示同样没有表单字段：两个按钮各自关窗，回车走 `on_ok`
            // （在 `render_restore_session_dialog` 里接的）。走到这里说明接线错了。
            Some(DialogMode::RestoreSession) => {
                unreachable!("会话恢复提示走 render_restore_session_dialog，不经此处")
            }
            // 粘贴确认（F-5）同理：没有表单字段，两个按钮各自关窗，回车走 `on_ok`
            // （在 `render_paste_confirm_dialog` 里接的）。走到这里说明接线错了。
            Some(DialogMode::PasteConfirm { .. }) => {
                unreachable!("粘贴确认走 render_paste_confirm_dialog，不经此处")
            }
            None => {}
        }
        // 保存成功：同步关闭 Root 层官方对话框（校验失败会在上方提前 return）
        w.close_dialog(cx);
        // 「保存后接着连」：只在**真的存下了凭据**时才连。否则 `connect_profile`
        // 会因为「既没有密码也没有私钥」把同一个对话框再打开一次 —— 用户看到的是
        // 「按了保存、窗口闪了一下又回来了」。
        if let Some(id) = connect_id {
            let ready = self
                .conn_store
                .profile(&id)
                .is_some_and(|p| p.password.is_some() || p.key_path.is_some());
            if ready {
                self.connect_profile(&id, w, cx);
            }
        }
        cx.notify();
    }

    /// 「连接测试」：用**表单此刻的内容**发起一次握手 + 认证，成功后立刻断开。
    ///
    /// 三条口径都是刻意的：
    /// 1. **不落库** —— 测试只是「先试试」，用户还没决定要保存。把它写进连接库
    ///    等于用一次试验篡改用户的列表（还会多出一条只用于测试的记录）。
    /// 2. **不碰任何会话状态** —— 不建标签、不占终端、不起指标泵。测试失败时屏幕上
    ///    不会留下任何残骸（这正是它与「保存并连接」的关键区别，见 core 的
    ///    `test_connection_in_background`）。
    /// 3. **原地显示结果** —— 结果落在对话框里的 `dlg_test` 槽，用户不用去别处找原因。
    ///
    /// 凭据取自表单；口令留空时（编辑既有连接）用**已存密文解出来的那把旧口令** ——
    /// 留空的语义是「不改口令」，用户此刻想测的正是那把口令。
    pub(super) fn test_connection(&mut self, cx: &mut Context<Self>) {
        if self.io.dlg_testing {
            return; // 已经在测：连点三次 = 三个后台线程同时敲同一台机器
        }
        let host = self.io.dlg_host.read(cx).value().trim().to_string();
        let user = self.io.dlg_user.read(cx).value().trim().to_string();
        let key_path = self.io.dlg_key_path.read(cx).value().trim().to_string();
        if host.is_empty() || user.is_empty() {
            self.io.dlg_test = Some((
                false,
                rustshell_core::i18n::t("主机与用户名为必填项").to_string(),
            ));
            cx.notify();
            return;
        }
        let port_text = self.io.dlg_port.read(cx).value().trim().to_string();
        let port = match port_text.parse::<u16>() {
            Ok(p) if p > 0 => p,
            _ if port_text.is_empty() => 22,
            _ => {
                self.io.dlg_test = Some((
                    false,
                    rustshell_core::i18n::t("端口需为 1–65535 的数字").to_string(),
                ));
                cx.notify();
                return;
            }
        };
        let typed_pass = self.io.dlg_pass.read(cx).value().to_string();
        let stored_pass = match self.dialog.as_ref() {
            Some(DialogMode::Edit(id)) => self
                .conn_store
                .profile(id)
                .and_then(|p| p.stored_password.as_deref())
                .and_then(rustshell_platform::secret::unprotect),
            _ => None,
        };
        // 认证方式决定试哪一侧（与 `dialog_save` 同一口径）：选了私钥就**不再**
        // 偷偷拿旧口令去试 —— 那样测通的是一条用户没选的路径。
        let (password, key_path) = match self.io.dlg_auth {
            AuthMode::Password => (
                if typed_pass.is_empty() {
                    stored_pass
                } else {
                    Some(typed_pass)
                },
                String::new(),
            ),
            AuthMode::Key => (None, key_path),
        };
        if self.io.dlg_auth == AuthMode::Key && key_path.is_empty() {
            self.io.dlg_test = Some((
                false,
                rustshell_core::i18n::t("请选择私钥文件").to_string(),
            ));
            cx.notify();
            return;
        }
        if password.is_none() && key_path.is_empty() {
            self.io.dlg_test = Some((
                false,
                rustshell_core::i18n::t("请先填写密码或私钥").to_string(),
            ));
            cx.notify();
            return;
        }

        // 探测用的 profile 是**临时构造**的：编辑时从库里取一份克隆（继承代理/标签等
        // 不影响认证的字段），新建时从零造一个。两者都只覆盖认证相关的四个字段。
        let mut probe = match self.dialog.as_ref() {
            Some(DialogMode::Edit(id)) => {
                // 极端情况下库里这条记录已经不在了（并发删除）：退回「从零造一个」，
                // 而不是让「连接测试」因为找不到记录直接停住。
                self.conn_store.profile(id).cloned().unwrap_or_else(|| {
                    ConnectionProfile::new(String::new(), host.clone(), user.clone())
                })
            }
            _ => ConnectionProfile::new(
                self.io.dlg_name.read(cx).value().trim().to_string(),
                host.clone(),
                user.clone(),
            ),
        };
        probe.host = host;
        probe.port = port;
        probe.username = user;
        probe.password = password;
        probe.key_path = if key_path.is_empty() {
            None
        } else {
            Some(key_path)
        };
        // 代理（F-10）：测的是**表单此刻**的代理草稿（用户问的就是「这套配置能不能连」），
        // 而不是库里存着的那套。「跟随全局」保持原样，由 core 拿传进去的全局值合成。
        probe.proxy = rustshell_core::proxy::ProxyChoice::from_parts(self.io.dlg_proxy_mode, {
            let mut cfg = self.dlg_proxy_draft(cx);
            // 口令：框里非空 = 新填的；空 = 「不改口令」——且只在目标未变时有效
            // （与 `dialog_save` 同一判定，测试与保存不该有两套口令语义）。
            let typed = self.io.dlg_proxy_pass.read(cx).value().to_string();
            if !typed.is_empty() {
                cfg.password = Some(typed);
            } else {
                let origin = self
                    .io
                    .dlg_proxy_prev
                    .as_ref()
                    .map(rustshell_core::proxy::fingerprint);
                if rustshell_core::proxy::may_inherit_password(origin.as_ref(), &cfg) {
                    cfg.password = self
                        .io
                        .dlg_proxy_prev
                        .as_ref()
                        .and_then(|p| p.password.clone());
                }
            }
            cfg
        });

        self.io.dlg_test = None; // 上一轮结果先清掉：新一次测试的过程才是主角
        self.io.dlg_testing = true;
        cx.notify();

        let timeout = std::time::Duration::from_secs(self.settings.connect_timeout_secs.clamp(
            rustshell_core::ssh::CONNECT_TIMEOUT_MIN_SECS,
            rustshell_core::ssh::CONNECT_TIMEOUT_MAX_SECS,
        ));
        let rx = rustshell_core::ssh::test_connection_in_background(
            probe,
            timeout,
            self.runtime_global_proxy(),
        );
        cx.spawn(async move |this, cx| {
            // 发送端被 drop（测试线程 panic）时 `recv_async` 返回 Err —— 不能就此静默，
            // 否则按钮会永远停在「测试中…」。
            let res = rx
                .recv_async()
                .await
                .unwrap_or_else(|_| Err(rustshell_core::i18n::t("测试线程异常结束").to_string()));
            let _ = this.update(cx, |m, cx| {
                m.io.dlg_testing = false;
                m.io.dlg_test = Some(match res {
                    Ok(msg) => (true, msg),
                    Err(e) => (false, e),
                });
                cx.notify();
            });
        })
        .detach();
    }

    /// 保存设置面板。
    ///
    /// 数值项已由官方 `SettingField::number_input` 的 min/max 钳制，故保存路径
    /// 只保留「终端字体名」这一项自由文本校验；非法则不保存、面板保持打开并
    /// 就地提示（空串不再表示「沿用原值」，字段已与设置双向绑定）。
    fn settings_save(&mut self, cx: &mut Context<Self>) {
        use std::sync::atomic::Ordering;

        let ff = self.settings.terminal_font_family.trim().to_string();
        if ff.is_empty() {
            self.set_error(rustshell_core::i18n::t("终端字体不能为空").to_string());
            cx.notify();
            return;
        }
        if ff.chars().count() > 64 || ff.chars().any(|c| c.is_control()) {
            self.set_error(
                rustshell_core::i18n::t("字体名称不合法（过长或含控制字符）").to_string(),
            );
            cx.notify();
            return;
        }
        self.settings.terminal_font_family = ff;

        // AI 配置
        // 与终端参数共用同一个「保存」动作，否则用户改完 AI 参数点保存却什么也没发生。
        // 先校验 AI，避免 AI 非法时终端参数已落盘造成的半保存状态。
        if let Err(e) = self.sync_ai_config(cx) {
            self.set_error(e);
            cx.notify();
            return;
        }
        // SMTP 同理：口令加密或校验失败都不该让别的参数「半保存」
        if let Err(e) = self.sync_smtp_config(cx) {
            self.set_error(e);
            cx.notify();
            return;
        }
        // 代理（F-10）同理：口令加密 / 地址校验失败都不落盘，面板保持打开就地提示
        if let Err(e) = self.sync_proxy_config(cx) {
            self.set_error(e);
            cx.notify();
            return;
        }
        // 保存失败不再静默：提示并保持面板打开
        if let Err(e) = self.settings.save(&paths::settings_path()) {
            self.set_error(rustshell_core::i18n::ts(
                format!("保存失败：{e}"),
                format!("Save failed: {e}"),
            ));
            cx.notify();
            return;
        }
        self.io.set_hint = None;
        // 已保存 到 关闭时不再回滚
        self.io.set_snapshot = None;

        // 即时应用：主题 / 指标采样频率 / 已打开终端的字体字号与配色
        crate::theme::apply_theme_mode(cx, self.settings.theme_mode);
        self.panel_ui
            .metrics_interval
            .store(self.settings.sample_interval_ms, Ordering::Relaxed);
        // T4-P3：配色不再只跟随主题，走「配色方案」解析（预设/自定义）
        let palette = self.settings.resolve_terminal_palette();
        let new_cfg = TerminalPaneConfig {
            font_family: self.settings.terminal_font_family.clone(),
            font_size: self.settings.terminal_font_size,
            line_height: self.settings.clamped_line_height(),
            scrollback: self.settings.scrollback,
            palette,
            encoding: self.settings.terminal_encoding.clone(),
            background_alpha: 1.0,
            cursor_blink: self.settings.terminal_cursor_blink,
            cursor_blink_ms: self.settings.terminal_cursor_blink_ms,
            cursor_style: self.settings.terminal_cursor_style,
            // 粘贴策略由 `PasteBehavior` 一个值算出两个引擎参数
            paste_normalize_newlines: self.settings.terminal_paste_behavior.normalize_newlines(),
            paste_protect_lines: self.settings.terminal_paste_behavior.protect_lines(),
        };
        for tab in &self.tabs_state.tabs {
            let Some(term) = tab.terminal.clone() else {
                continue;
            };
            term.update(cx, |tv, tcx| {
                let mut c = tv.config().clone();
                c.font_family = new_cfg.font_family.clone();
                c.font_size = px(new_cfg.font_size);
                c.scrollback = new_cfg.scrollback;
                // 行高（2026-09-28 新增设置项）：不涉及 PTY，可即时热更新
                c.line_height_multiplier = new_cfg.line_height;
                // 闪烁与粘贴策略同样即时生效（不涉及 PTY，可安全热更新）
                c.cursor_blink = new_cfg.cursor_blink;
                c.cursor_blink_interval_ms = new_cfg.cursor_blink_ms;
                // 光标形状：改的是 Term 的 default_cursor_style，已发过 DECSCUSR
                // 的会话仍以程序请求为准（与闪烁同一条「用户设置是底色」的口径）
                c.default_cursor_shape =
                    crate::terminal_view::to_cursor_shape(new_cfg.cursor_style);
                c.paste_normalize_newlines = new_cfg.paste_normalize_newlines;
                // F-5：面板上改完档位，「下一次粘贴」就按新值判 —— 不用重开会话
                c.paste_protect_lines = new_cfg.paste_protect_lines;
                c.colors = crate::terminal_view::to_color_palette_a(
                    &new_cfg.palette,
                    new_cfg.background_alpha,
                );
                // 编码只对新终端生效：已打开终端的转码状态不中途切换
                tv.update_config(c, tcx);
            });
        }

        self.dialog = None;
        self.cancel_key_record();
        cx.notify();
    }

    /// 设置面板：改用官方 `Settings` 组件族承载全部设置项。
    ///
    /// 页面 / 分组 / 字段的装配在 `settings_panel.rs`；这里只负责 Dialog 外壳、
    /// 校验提示与「保存 / 取消」两个动作。
    /// 渲染任务编辑对话框（§6.3）。
    ///
    /// 表单的每一项都在改 `MainView::task_draft` 的**副本**；点保存才写回
    /// `task_store`。取消时若这条是「新建出来的空壳」还会顺手删掉它 ——
    /// 否则列表里会留下一条用户没建完的任务。
    fn render_task_dialog(
        &mut self,
        dialog: Dialog,
        id: &str,
        _w: &mut Window,
        cx: &mut Context<Self>,
    ) -> Dialog {
        use gpui::Styled as _;
        let t = crate::theme::current(cx);
        // 对话框携带的 id 必须与草稿对上：`task_draft` 是单例字段，理论上
        // 「先开 A、再开 B」的后一次会把草稿换掉，而这个对话框是上一帧就注册进
        // 官方 Dialog 栈的 —— 不校验就会拿 A 的窗口去改 B 的任务。
        let draft_id = self.io.task_draft.as_ref().map(|d| d.id.clone());
        let stale = draft_id.as_deref() != Some(id);
        // `Dialog` 本身不是 `InteractiveElement`（没有 `debug_selector`），钩子挂在
        // 内容根节点上。与任务面板同一手法：`debug_bounds` 只收 `&'static str`，
        // 故把运行期 id 映射到 `t-a` / `t-b` 两个固定选择器（用例只用这两个 id）。
        // `debug_selector` 要 `'static` 闭包，故先把 id 归一到 `'static` 字面量。
        let sel_id: &'static str = if id == "t-a" { "t-a" } else { "t-b" };
        let task_dialog_selector = move || format!("task-dialog-{sel_id}");
        if stale {
            // 草稿已被另一次「编辑」换走：这个对话框只剩一个身份，不该继续
            // 拿它改别人的任务。渲染一句说明 + 关闭按钮，用户点掉即可。
            let view = cx.entity();
            return dialog
                .w(px(560.))
                .bg(hex(t.bg_overlay))
                .title(rustshell_core::i18n::t("任务"))
                .child(
                    div()
                        .debug_selector(task_dialog_selector)
                        .p_4()
                        .text_size(px(13.))
                        .text_color(hex(t.fg_secondary))
                        .child(rustshell_core::i18n::t(
                            "这条任务已在别处打开，请重新打开编辑",
                        )),
                )
                .button_props(
                    DialogButtonProps::default()
                        .on_ok(move |_, w, cx| {
                            view.update(cx, |m, cx| {
                                m.io.task_draft = None;
                                m.io.task_dlg_error = None;
                                m.dialog = None;
                                cx.notify();
                            });
                            w.close_dialog(cx);
                            false
                        })
                        .on_cancel(|_, w, cx| {
                            w.close_dialog(cx);
                            true
                        }),
                );
        }
        // 目标解析预览已在 UI 上撤下（「解析出 xx 台」功能取消）：
        // renderer 因此不再需要任何预计算，草稿只用于读回选择状态。
        self.task_collect_draft(cx);
        let error = self.io.task_dlg_error.clone();
        let kind = self.io.task_target_kind;
        let action_kind = self
            .io
            .task_draft
            .as_ref()
            .map(|d| match &d.action {
                rustshell_core::tasks::TaskAction::MultiConnect { .. } => 0u8,
                rustshell_core::tasks::TaskAction::ExecMulti { .. } => 1,
                rustshell_core::tasks::TaskAction::Download { .. } => 2,
                rustshell_core::tasks::TaskAction::AiInspect { .. } => 3,
            })
            .unwrap_or(0);
        let (notify_anomaly, notify_error, notify_ok) = self.io.dlg_task_notify;
        // 分组 / 连接的候选取自连接库的**快照**：渲染期不再借 `self.conn_store`
        // （下面 `pick_*` 的闭包要可变借 `self`，同时持有 `&self.conn_store` 会冲突）。
        let group_options: Vec<(String, String)> = self
            .conn_store
            .groups
            .iter()
            .map(|g| (g.id.clone(), g.name.clone()))
            .collect();
        let profile_options: Vec<(String, String)> = self
            .conn_store
            .profiles
            .iter()
            .map(|p| {
                (
                    p.id.clone(),
                    format!("{} ({}@{})", p.name, p.username, p.host),
                )
            })
            .collect();
        let group_sel = self.io.dlg_task_group_sel.clone();
        let profiles_sel = self.io.dlg_task_profiles_sel.clone();
        let no_groups = group_options.is_empty();
        let no_profiles = profile_options.is_empty();

        let field = |label: &'static str, input: &Entity<InputState>| {
            h_flex()
                .gap_2()
                .items_center()
                .child(
                    div()
                        .w(px(76.))
                        .flex_shrink_0()
                        .text_size(px(12.))
                        .text_color(hex(t.fg_secondary))
                        .child(SharedString::from(rustshell_core::i18n::t(label))),
                )
                .child(Input::new(input).small().w_full())
        };
        // 标题保留（用户要求）：只写「任务」——任务名回到表单里了，
        // 标题再抄一遍就是在同一个面板里重复两行同样的字。
        let header = div()
            .text_size(px(14.))
            .font_weight(gpui::FontWeight::BOLD)
            .child(rustshell_core::i18n::t("任务"));

        let body = v_flex()
            .gap_3()
            // 不再自己限高 + 滚动（2026-09-21 修）：外层 gpui-component 的 Dialog
            // 已经是「`max_h(窗口可用高度)` + `flex_1 + overflow_hidden` 视口 +
            // `size_full` 滚动体」的正规结构，这里再套一层
            // `max_h + overflow_y_scrollbar` 只会把内容先封死、还把滚动条画在
            // 一个永远滚不动的位置（同 `picker_box` 的注释）。
            // `flex_shrink_0`：让正文保持自然高度，视口装不下时是**滚动**而不是
            // 被压扁 —— 被压扁的字段会互相叠在一起。
            .flex_shrink_0()
            // 名称：列表行、AI 会话标题、通知里都用它当标识，用户得能改
            // （留空则保存时取 AI 要求首行，见 `task_collect_draft`）。
            .child(field(
                rustshell_core::i18n::t("名称"),
                &self.io.dlg_task_name,
            ))
            // 时间：五段式 cron（**只有这一种写法**，时区跟随本机）。
            // 表单五项：名称 / 时间 / 目标 / AI 要求 / 通知。
            .child(
                v_flex()
                    .gap_1()
                    .child(field(
                        rustshell_core::i18n::t("时间"),
                        &self.io.dlg_task_cron,
                    ))
                    .child(
                        div()
                            .pl(px(84.))
                            .text_size(px(10.))
                            .text_color(hex(t.fg_muted))
                            .child(rustshell_core::i18n::t(
                                "五段式：分 时 日 月 周，如 0 9 * * *（每天 09:00，跟随本机时区）",
                            )),
                    ),
            )
            // 目标：方式二选一（分组 / 连接），下面是**可选列表**
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(hex(t.fg_secondary))
                            .child(rustshell_core::i18n::t("目标")),
                    )
                    .child(
                        h_flex()
                            .gap_3()
                            .child(pick_target_radio(
                                "task-tg-group",
                                rustshell_core::i18n::t("分组"),
                                kind == 0,
                                TargetRoom::Group,
                                cx,
                            ))
                            .child(pick_target_radio(
                                "task-tg-profile",
                                rustshell_core::i18n::t("连接"),
                                kind == 1,
                                TargetRoom::Profiles,
                                cx,
                            )),
                    )
                    .child(if kind == 0 {
                        group_picker(&group_options, group_sel.as_deref(), no_groups, cx)
                    } else {
                        profile_picker(&profile_options, &profiles_sel, no_profiles, cx)
                    }),
            )
            // AI 巡检内容：只有一句「AI 要求」（巡检项已撤下，查什么由模型
            // 自己决定）。高度由文本域自己按内容生长（2–10 行，见 `auto_grow`），
            // 这里**不能**再给高度 —— 固定高度会把自动生长压死。
            .when(action_kind == 3, |d| {
                d.child(
                    v_flex()
                        .gap_1()
                        .child(
                            h_flex()
                                .gap_1()
                                .items_center()
                                .child(
                                    div()
                                        .text_size(px(12.))
                                        .text_color(hex(t.fg_secondary))
                                        .child(rustshell_core::i18n::t("AI 要求")),
                                )
                                // 「必输」标记（2026-09-22 用户要求「AI 要求必输」）：
                                // 真正的拦截在 core 的 `TaskAction::validate()`（保存 /
                                // 启用 / 开跑三条路都过它），这里只负责**在填之前**
                                // 就告诉用户它是必填的 —— 让人填完再被驳回是两种体验。
                                .child(
                                    div()
                                        .text_size(px(10.))
                                        .text_color(hex(t.warning))
                                        .child(rustshell_core::i18n::t("必输")),
                                ),
                        )
                        // `Textarea` 没有 `small()`（尺寸全由高度决定）。
                        .child(
                            gpui_component::input::Textarea::new(&self.io.dlg_task_prompt).w_full(),
                        ),
                )
            })
            // 既有任务若不是 AI 巡检：原样保留其动作，只用一句灰字说明 ——
            // 表单里没有它的字段，不写清楚会让人以为「命令/远端路径被删了」。
            .when(action_kind != 3, |d| {
                d.child(div().text_size(px(11.)).text_color(hex(t.fg_muted)).child(
                    rustshell_core::i18n::t(
                        "这条任务不是 AI 巡检；保存只更新时间、目标与通知，原动作保持不变。",
                    ),
                ))
            })
            // 通知策略
            .child(
                h_flex()
                    .gap_3()
                    .items_center()
                    .child(
                        div()
                            .w(px(76.))
                            .flex_shrink_0()
                            .text_size(px(12.))
                            .text_color(hex(t.fg_secondary))
                            .child(rustshell_core::i18n::t("通知")),
                    )
                    .child(notify_check(
                        "task-nt-anomaly",
                        rustshell_core::i18n::t("异常时"),
                        notify_anomaly,
                        0,
                        cx,
                    ))
                    .child(notify_check(
                        "task-nt-error",
                        rustshell_core::i18n::t("失败时"),
                        notify_error,
                        1,
                        cx,
                    ))
                    .child(notify_check(
                        "task-nt-ok",
                        rustshell_core::i18n::t("每次运行后"),
                        notify_ok,
                        2,
                        cx,
                    )),
            )
            // 校验错误：**文本**提示（§13.2 明令不得只有红框）
            .children(error.map(|e| {
                div()
                    .text_size(px(11.))
                    .text_color(hex(t.danger))
                    .child(SharedString::from(e))
            }));

        let footer = h_flex()
            .gap_2()
            .justify_end()
            .pt_1()
            .child(
                Button::new("task-dlg-cancel")
                    .small()
                    .label(rustshell_core::i18n::t("取消"))
                    .on_click(cx.listener(|m, _e, w, cx| m.task_draft_cancel(w, cx))),
            )
            // 「发一次 AI 测试」：把表单里的内容先落库，然后**真的跑一次** ——
            // 跑完在 AI 助手里留下会话，运行记录里也能回看（旧的「试运行」只做
            // 纯计算，用户看不到模型到底巡检出什么，已按运行反馈撤下）。
            // 只在 AI 巡检的任务上出现：非 AI 巡检的任务跑的不是「AI 测试」，
            // 按钮名会骗人。
            .when(action_kind == 3, |d| {
                d.child(
                    Button::new("task-dlg-ai-test")
                        .small()
                        .label(rustshell_core::i18n::t("发一次 AI 测试"))
                        .on_click(cx.listener(|m, _e, w, cx| m.task_send_ai_test(w, cx))),
                )
            })
            .child(
                Button::new("task-dlg-ok")
                    .small()
                    .primary()
                    .label(rustshell_core::i18n::t("保存"))
                    .on_click(cx.listener(|m, _e, w, cx| m.task_draft_save(w, cx))),
            );

        let view = cx.entity();
        dialog
            .w(px(560.))
            .bg(hex(t.bg_overlay))
            .title(header)
            .child(body.debug_selector(task_dialog_selector))
            .footer(footer)
            .button_props(
                DialogButtonProps::default()
                    .on_ok(move |_, w, cx| {
                        view.update(cx, |m, cx| m.task_draft_save(w, cx));
                        false
                    })
                    .on_cancel({
                        let view = cx.entity();
                        move |_, w, cx| {
                            view.update(cx, |m, cx| m.task_draft_cancel(w, cx));
                            true
                        }
                    }),
            )
    }

    fn render_settings_dialog(
        &mut self,
        dialog: Dialog,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) -> Dialog {
        use gpui::Styled as _;
        let t = crate::theme::current(cx);

        // 拖动：标题栏按下左键记起点，再由 gpui 的 **drag** 通道累加位移。
        //
        // 为什么不用标题栏自己的 `on_mouse_move`：它只在指针仍落在标题栏内时
        // 收事件，手一快一偏就丢事件，面板会「粘住」跟不上。
        // 为什么也不用 `Window::on_mouse_event`：那要求**绘制阶段**注册
        // （gpui `Invalidator::debug_assert_paint`），而对话框构建函数是在视图
        // `render` 里跑的，此时处于 prepaint 阶段，debug 构建下会直接 panic
        // （“this method can only be called during paint”）。
        // `on_drag_move` 是 gpui 为此提供的正当入口：只要拖拽由本元素发起，
        // 无论指针走到窗口何处，每个 move 事件都会回调（窗口级派发）。
        let mut header = h_flex()
            .id("settings-drag-handle")
            .w_full()
            .items_center()
            .gap_2()
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|m, ev: &gpui::MouseDownEvent, _w, cx| {
                    m.io.set_drag_from = Some((ev.position.x.as_f32(), ev.position.y.as_f32()));
                    cx.notify();
                }),
            )
            // 拖拽载荷只用于类型标识（`on_drag_move` 按 `T` 匹配），光标下不画预览。
            .on_drag(SettingsDrag, |_, _, _w, cx| cx.new(|_| DragGhost))
            .on_drag_move::<SettingsDrag>(cx.listener(
                |m, ev: &gpui::DragMoveEvent<SettingsDrag>, _w, cx| {
                    let (x, y) = (ev.event.position.x.as_f32(), ev.event.position.y.as_f32());
                    // 起点缺失（例如拖拽由非按下路径发起）时以本帧位置为基准，
                    // 位移从下一帧开始累计，不会把面板瞬间弹到别处。
                    let Some(from) = m.io.set_drag_from else {
                        m.io.set_drag_from = Some((x, y));
                        return;
                    };
                    m.io.set_drag_offset.0 += x - from.0;
                    m.io.set_drag_offset.1 += y - from.1;
                    m.io.set_drag_from = Some((x, y));
                    cx.notify();
                },
            ))
            .child(
                div()
                    .text_size(px(14.))
                    .font_weight(gpui::FontWeight::BOLD)
                    .child(rustshell_core::i18n::t("设置")),
            );
        // 抓手光标（本 fork 没有 `cursor_pointer()` 之类的 fluent 方法，
        // 直接写样式字段）
        header.style().mouse_cursor = Some(gpui::CursorStyle::OpenHand);

        // 官方 Settings：侧栏（页面导航 + 搜索）+ 右栏（GroupBox 分组 + 字段），
        // 字段值经捕获的`Entity<MainView>`双向读写，面板自身不持有业务状态。
        let settings = self.render_settings_pages(w, cx);

        // 官方根节点是`h_resizable`，依赖父容器给出确定高度，
        // 否则整块塌陷为 0（官方自测用例同样是`div.h(..)`包裹）。
        // 校验/保存提示置于面板之下、页脚之上，任何页面都可见。
        //
        // 提示走官方`Alert`，变体由 `set_hint_ok` 决定。该字段此前**只写不读**
        // （全仓无渲染路径消费它），于是这里的取色恒为 danger 到
        // 成功的保存提示也渲染成红色错误。现在收口在 `Alert` 上：
        // 成功=success、失败=error，级别由 `set_info(text, ok)` 声明。
        let hint_kind = if self.io.set_hint_ok {
            crate::notice::NoticeKind::Success
        } else {
            crate::notice::NoticeKind::Error
        };
        let hint = self.io.set_hint.clone().map(|hint| {
            hint_kind
                .card("settings-hint", SharedString::from(hint))
                .small()
                .text_size(px(11.5))
        });
        // 官方 Settings 的侧栏搜索框状态在组件内部（`SettingsState` 不可见），
        // 拿不到 `Entity<InputState>` 去挂 `blur_input_on_outside_click`。
        // 这里在整块设置主体上兜底：冒泡阶段若这次按下没有被任何可聚焦控件
        // 认领（`default_prevented` 仍为 false —— 点在按钮 / 页签 / 空白处），
        // 就把输入焦点收起来。点进任意输入框不会误伤：官方 Input 的
        // 「按下即聚焦」在冒泡里执行并 `prevent_default`，本处理器据此让路。
        let body = v_flex()
            .gap_2()
            .child(
                div()
                    .w_full()
                    .h(px(500.))
                    .debug_selector(|| "settings-body".to_string())
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(|_m, _ev, w, cx| {
                            if !w.default_prevented() && w.focused_input(cx).is_some() {
                                w.blur(cx);
                            }
                        }),
                    )
                    .child(settings),
            )
            .children(hint);

        let footer = h_flex()
            .gap_2()
            .justify_end()
            .pt_2()
            .child(
                Button::new("set-cancel")
                    .small()
                    .label(rustshell_core::i18n::t("取消"))
                    .on_click(cx.listener(|m, _e, w, cx| {
                        m.cancel_key_record();
                        // close_dialog_ui 内部会按 set_snapshot 回滚
                        m.close_dialog_ui(w, cx);
                    })),
            )
            .child(
                Button::new("set-save")
                    .small()
                    .primary()
                    .label(rustshell_core::i18n::t("保存"))
                    .on_click(cx.listener(|m, _e, w, cx| {
                        m.settings_save(cx);
                        if m.dialog.is_none() {
                            w.close_dialog(cx);
                        }
                    })),
            );

        // 位移钳制：保证面板至少留 KEEP 像素在窗口内。面板没有「重置位置」入口，
        // 一旦被拖出可视区就等于把这个对话框弄丢了（只能重启应用）。
        let (dx, dy) = {
            let vp = w.viewport_size();
            let (vw, vh) = (vp.width.as_f32(), vp.height.as_f32());
            // 面板宽与下方 `.w(px(940.))` 一致
            const PW: f32 = 940.;
            const KEEP: f32 = 96.;
            // 官方 Dialog 的默认落点：水平居中、垂直为视口高的 1/10
            let base_x = (vw - PW) / 2.;
            let base_y = vh / 10.;
            // 约束（面板左上角 = base + d）：
            //   -PW + KEEP <= base_x + dx <= vw - KEEP
            //           0 <= base_y + dy <= vh - KEEP
            let lo_x = -base_x - PW + KEEP;
            let hi_x = vw - KEEP - base_x;
            let lo_y = -base_y;
            let hi_y = vh - KEEP - base_y;
            // 用 max/min 而非 clamp：窗口极窄时区间可能反转，clamp 会 panic
            (
                self.io.set_drag_offset.0.max(lo_x).min(hi_x),
                self.io.set_drag_offset.1.max(lo_y).min(hi_y),
            )
        };

        let view = cx.entity();
        let view_ok = view.clone();
        dialog
            // 官方侧栏 180px 起 + 字段列，600px 过窄 到 940px
            .w(px(940.))
            .bg(hex(t.bg_overlay))
            // 拖动位移以**外边距**施加：官方 Dialog 的弹层自带 `left/top` 定位，
            // 外边距只平移整块面板，不会动到官方算出的居中落点。
            .ml(px(dx))
            .mt(px(dy))
            .title(header)
            .child(body)
            .footer(footer)
            .button_props(
                DialogButtonProps::default()
                    // Enter 走官方 `Confirm` 动作，只认 `button_props.on_ok`；
                    // 自绘 footer 时它仍会被调用，不设就等于「Enter 关窗但没保存」。
                    .on_ok(move |_, w, cx| {
                        view_ok.update(cx, |m, cx| {
                            m.settings_save(cx);
                            if m.dialog.is_none() {
                                w.close_dialog(cx);
                            }
                        });
                        // 业务侧负责关窗，故返回 false。
                        false
                    })
                    .on_cancel(move |_, w, cx| {
                        view.update(cx, |m, cx| {
                            m.cancel_key_record();
                            m.close_dialog_ui(w, cx);
                        });
                        true
                    }),
            )
    }

    fn qc_save(&mut self, cx: &mut Context<Self>) {
        let label = self.io.dlg_qc_label.read(cx).value().trim().to_string();
        let script = self.io.dlg_qc_script.read(cx).value().to_string();
        if label.is_empty() || script.trim().is_empty() {
            return;
        }
        // 末尾是否回车由对话框勾选项决定（取消对话框不会写库）
        let append_newline = self.io.dlg_qc_newline;
        let Some(mode) = self.dialog.take() else {
            return;
        };
        match mode {
            DialogMode::QcNew(fid) => {
                if let Some(f) = self.quick_store.folders.iter_mut().find(|f| f.id == fid) {
                    f.commands.push(rustshell_core::quickcmd::QuickCmd {
                        id: rustshell_core::connections::uuid_like(),
                        label,
                        script,
                        targets: Vec::new(),
                        append_newline,
                    });
                }
            }
            DialogMode::QcEdit(fid, cid) => {
                if let Some(f) = self.quick_store.folders.iter_mut().find(|f| f.id == fid) {
                    if let Some(c) = f.commands.iter_mut().find(|c| c.id == cid) {
                        c.label = label;
                        c.script = script;
                        c.append_newline = append_newline;
                    }
                }
            }
            _ => {}
        }
        let _ = self.quick_store.save(&self.ai_ui.quick_path);
        cx.notify();
    }

    fn render_qc_dialog(
        &mut self,
        dialog: Dialog,
        _w: &mut Window,
        cx: &mut Context<Self>,
    ) -> Dialog {
        use gpui::Styled as _;
        let t = crate::theme::current(cx);

        let field = |label: &'static str, input: &Entity<InputState>| {
            h_flex()
                .gap_2()
                .items_center()
                .child(
                    div()
                        .w(px(88.))
                        .flex_shrink_0()
                        .text_size(px(12.))
                        .text_color(hex(t.fg_secondary))
                        .child(SharedString::from(rustshell_core::i18n::t(label))),
                )
                .child(Input::new(input).small().w_full())
        };

        let header = div()
            .text_size(px(14.))
            .font_weight(gpui::FontWeight::BOLD)
            .child(rustshell_core::i18n::t("快捷命令"));

        let body = v_flex()
            .gap_3()
            .child(field(
                rustshell_core::i18n::t("名称"),
                &self.io.dlg_qc_label,
            ))
            // 命令正文改用官方 Editor（代码编辑器）：标签在上、编辑器在下，
            // 支持多行 shell 片段 + 行号 + 自动缩进
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_size(px(11.))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(hex(t.fg_secondary))
                            .child(rustshell_core::i18n::t("命令")),
                    )
                    .child(Editor::new(&self.io.dlg_qc_script).h(px(148.))),
            )
            // 「末尾添加回车」：勾上 = 写入后立即执行；取消勾选 = 只键入不执行
            // （留给用户继续补参数）。与「记住密码」同款：xsmall 复选框 + 11px 次级标签。
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .child(
                        gpui_component::checkbox::Checkbox::new("qc-dlg-newline")
                            .xsmall()
                            .checked(self.io.dlg_qc_newline)
                            .on_click(cx.listener(|m, checked: &bool, _w, cx| {
                                m.io.dlg_qc_newline = *checked;
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(hex(t.fg_secondary))
                            .child(rustshell_core::i18n::t(
                                "末尾添加回车（关闭则只键入不执行）",
                            )),
                    ),
            );

        let footer = h_flex()
            .gap_2()
            .justify_end()
            .pt_1()
            .child(
                Button::new("qc-dlg-cancel")
                    .small()
                    .label(rustshell_core::i18n::t("取消"))
                    .on_click(cx.listener(|m, _e, w, cx| {
                        m.close_dialog_ui(w, cx);
                    })),
            )
            .child(
                Button::new("qc-dlg-save")
                    .small()
                    .primary()
                    .label(rustshell_core::i18n::t("保存"))
                    .on_click(cx.listener(|m, _e, w, cx| {
                        m.qc_save(cx);
                        if m.dialog.is_none() {
                            w.close_dialog(cx);
                        }
                    })),
            );

        let view = cx.entity();
        let view_ok = view.clone();
        dialog
            .w(px(440.))
            .bg(hex(t.bg_overlay))
            .title(header)
            .child(body)
            .footer(footer)
            .button_props(
                DialogButtonProps::default()
                    .on_ok(move |_, w, cx| {
                        // Enter 走官方 `Confirm` 动作，只认 `button_props.on_ok`；镜像 footer
                        // 的「保存」动作。
                        view_ok.update(cx, |m, cx| {
                            m.qc_save(cx);
                            if m.dialog.is_none() {
                                w.close_dialog(cx);
                            }
                        });
                        // 业务侧负责关窗（校验失败要保持对话框打开），故返回 false。
                        false
                    })
                    .on_cancel(move |_, w, cx| {
                        view.update(cx, |m, cx| m.close_dialog_ui(w, cx));
                        true
                    }),
            )
    }

    pub fn confirm_sftp_remove(
        &mut self,
        path: String,
        is_dir: bool,
        refresh: String,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.present_dialog(
            DialogMode::SftpRemove {
                path,
                is_dir,
                refresh,
            },
            w,
            cx,
        );
    }

    fn render_sftp_remove_dialog(
        &mut self,
        dialog: Dialog,
        _w: &mut Window,
        cx: &mut Context<Self>,
    ) -> Dialog {
        use gpui::Styled as _;
        let t = crate::theme::current(cx);
        let Some(DialogMode::SftpRemove {
            path,
            is_dir,
            refresh,
        }) = self.dialog.clone()
        else {
            return dialog;
        };

        let header = h_flex()
            .gap_2()
            .items_center()
            .child(
                div()
                    .text_size(px(14.))
                    .font_weight(gpui::FontWeight::BOLD)
                    .child(if is_dir {
                        rustshell_core::i18n::t("删除目录")
                    } else {
                        rustshell_core::i18n::t("删除文件")
                    }),
            )
            .child(
                div()
                    .px_2()
                    .py_0p5()
                    .rounded(px(4.))
                    .text_size(px(11.))
                    .text_color(hex(t.danger))
                    .border_1()
                    .border_color(hex(t.danger))
                    .child(rustshell_core::i18n::t("不可恢复")),
            );

        let body = v_flex()
            .gap_2()
            .child(div().text_size(px(12.)).text_color(hex(t.fg_muted)).child(
                rustshell_core::i18n::t("确认删除以下远端路径？该操作不可恢复："),
            ))
            .child(
                div()
                    .w_full()
                    .rounded(px(6.))
                    .p_2()
                    .bg(hex(t.bg_base))
                    .border_1()
                    .border_color(hex(t.border_subtle))
                    .font_family(rustshell_core::theme::default_terminal_font())
                    .text_size(px(12.))
                    .text_ellipsis()
                    .child(SharedString::from(path.clone())),
            )
            .when(is_dir, |d| {
                d.child(
                    crate::notice::NoticeKind::Warning
                        .card(
                            "sftp-rm-note",
                            rustshell_core::i18n::t(
                                "这是目录：其中的所有文件与子目录都会被一并删除。",
                            ),
                        )
                        .xsmall()
                        .text_size(px(11.)),
                )
            });

        let path_ok = path.clone();
        let refresh_ok = refresh.clone();
        let footer = h_flex()
            .gap_2()
            .justify_end()
            .pt_1()
            .child(
                Button::new("sftp-rm-cancel")
                    .small()
                    .label(rustshell_core::i18n::t("取消"))
                    .on_click(cx.listener(|m, _e, w, cx| {
                        m.close_dialog_ui(w, cx);
                    })),
            )
            .child(
                Button::new("sftp-rm-confirm")
                    .small()
                    .primary()
                    .label(rustshell_core::i18n::t("确认删除"))
                    .on_click(cx.listener(move |m, _e, w, cx| {
                        m.close_dialog_ui(w, cx);
                        if let Some(tx) = m.active_tab().and_then(|t| t.session.sftp_tx.clone()) {
                            let _ = tx.try_send(rustshell_core::sftp::SftpCmd::Remove {
                                path: path.clone(),
                                is_dir,
                                // 目录删除一律递归（2026-09-22 用户要求）：上方的确认框
                                // 已经把「其中的所有文件与子目录都会被一并删除」写明，
                                // 到这里再退化成「只删空目录」会让确认框的承诺落空。
                                recursive: is_dir,
                                refresh: refresh.clone(),
                            });
                        }
                        cx.notify();
                    })),
            );

        let view = cx.entity();
        let view_ok = view.clone();
        dialog
            .w(px(440.))
            .bg(hex(t.bg_overlay))
            .title(header)
            .child(body)
            .footer(footer)
            .button_props(
                DialogButtonProps::default()
                    .on_ok(move |_, w, cx| {
                        // Enter 走官方 `Confirm` 动作，只认 `button_props.on_ok`；镜像 footer
                        // 的「确认删除」动作（先关窗再投递 SFTP 指令）。
                        view_ok.update(cx, |m, cx| {
                            m.close_dialog_ui(w, cx);
                            if let Some(tx) = m.active_tab().and_then(|t| t.session.sftp_tx.clone())
                            {
                                let _ = tx.try_send(rustshell_core::sftp::SftpCmd::Remove {
                                    path: path_ok.clone(),
                                    is_dir,
                                    recursive: is_dir,
                                    refresh: refresh_ok.clone(),
                                });
                            }
                            cx.notify();
                        });
                        // 业务侧负责关窗（校验失败要保持对话框打开），故返回 false。
                        false
                    })
                    .on_cancel(move |_, w, cx| {
                        view.update(cx, |m, cx| m.close_dialog_ui(w, cx));
                        true
                    }),
            )
    }

    /// 「恢复上次会话？」（F-9，规格 §1.9 交互模型）。
    ///
    /// 两件事必须在正文里说清楚，缺一件用户就可能做出与预期相反的选择：
    /// 1. **上次开着什么**（标签数 / 连接数 / 本地终端数）——「恢复」的含义；
    /// 2. **连接不会自动发起** —— 否则用户会以为按下「恢复」= 连上所有服务器
    ///    （规格明写「生产连接默认不自动发起」）。
    ///
    /// 2026-09-29：本框**不再在启动时自动弹出**（用户要求「取消恢复上次会话功能」），
    /// 只由用户手动触发（`MainView::layout_restore_now`）。随之撤掉「下次不再询问」
    /// 勾选框 —— 那个勾选的意义是「以后别自动问了」，而现在已经没有「自动问」这回事。
    fn render_restore_session_dialog(
        &mut self,
        dialog: Dialog,
        _w: &mut Window,
        cx: &mut Context<Self>,
    ) -> Dialog {
        use gpui::Styled as _;
        let theme = crate::theme::current(cx);
        // 没有待恢复的快照却渲染到这里（理论不可达）：按 0 个标签渲染即可，
        // 不必 panic —— 对话框渲染炸掉会连累整棵树。
        let (total, ssh) = self.restore_prompt_stats().unwrap_or((0, 0));
        let local = total.saturating_sub(ssh);

        let header = div()
            .text_size(px(14.))
            .font_weight(gpui::FontWeight::BOLD)
            .child(rustshell_core::i18n::t("恢复上次会话"));

        let detail = if local == 0 {
            rustshell_core::i18n::ts(
                format!("上次开着 {total} 个标签，其中 {ssh} 个连接。"),
                format!("Last time {total} tabs were open, {ssh} of them connections."),
            )
        } else {
            rustshell_core::i18n::ts(
                format!("上次开着 {total} 个标签，其中 {ssh} 个连接、{local} 个本地终端。"),
                format!(
                    "Last time {total} tabs were open: {ssh} connections and {local} local terminals."
                ),
            )
        };

        let body = v_flex()
            .gap_2()
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(hex(theme.fg_muted))
                    .child(SharedString::from(detail)),
            )
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(hex(theme.fg_secondary))
                    .child(rustshell_core::i18n::t(
                        "连接会恢复成「已断开」，不会自动发起；本地终端会照常重开。",
                    )),
            );

        let footer = h_flex()
            .gap_2()
            .justify_end()
            .pt_1()
            .child(
                Button::new("restore-decline")
                    .small()
                    .label(rustshell_core::i18n::t("不恢复"))
                    .on_click(cx.listener(|m, _e, w, cx| {
                        m.layout_decline();
                        m.close_dialog_ui(w, cx);
                    })),
            )
            .child(
                Button::new("restore-accept")
                    .small()
                    .primary()
                    .label(rustshell_core::i18n::t("恢复"))
                    .on_click(cx.listener(|m, _e, w, cx| m.layout_accept(w, cx))),
            );

        let view = cx.entity();
        dialog
            .w(px(460.))
            .bg(hex(theme.bg_overlay))
            .title(header)
            .child(body)
            .footer(footer)
            .button_props(
                DialogButtonProps::default()
                    // Enter 走官方 `Confirm` 动作，只认这里的 `on_ok` ——
                    // 自绘 `.footer()` 不会自动把按钮接上去（铁律 #2）。
                    .on_ok(move |_, w, cx| {
                        view.update(cx, |m, cx| m.layout_accept(w, cx));
                        // 业务侧负责关窗，故返回 false。
                        false
                    })
                    // Esc / 取消：与「不恢复」同义（用户把这件事推开了，就不该
                    // 由我们替他恢复）。**但不写设置** —— 2026-09-29 起本框只在用户
                    // 手动触发时出现，框上已无「下次不再询问」勾选框，也就没有任何
                    // 回写档位的路径；`on_close` 里那道兜底覆盖点遮罩 / 右上角 X 两条路径。
                    .on_cancel(|_, _w, _cx| true),
            )
    }

    /// 粘贴确认（F-5，规格 §1.5）：告诉用户「要粘几行、长什么样」，再让他决定。
    ///
    /// 正文里的行数与预览都**现取自终端实体上的挂起请求**（唯一真值），
    /// 不在对话框里存副本 —— 会话在弹框期间断连时引擎会清掉挂起，
    /// 渲染期 [`super::MainView::paste_prompt_prune`] 随即把框收掉。
    ///
    /// 三条信息缺一不可：**几行**（规模）、**长什么样**（是不是我刚复制的那段）、
    /// **会怎样**（逐行发送 = 命令会立即执行）。只给「确认粘贴吗？」的框，
    /// 用户唯一能学的姿势就是无脑回车 —— 那就等于没这个功能。
    fn render_paste_confirm_dialog(
        &mut self,
        dialog: Dialog,
        session: &str,
        _w: &mut Window,
        cx: &mut Context<Self>,
    ) -> Dialog {
        use gpui::Styled as _;
        let theme = crate::theme::current(cx);
        // 理论不可达的 `None`（挂起被清而框还没收）按「没有内容」渲染，不 panic ——
        // 对话框渲染炸掉会连累整棵树。正常路径下一帧 `paste_prompt_prune` 就收框。
        let (lines, preview) = self
            .paste_prompt_body(session, cx)
            .unwrap_or((0, String::new()));

        let header = div()
            .text_size(px(14.))
            .font_weight(gpui::FontWeight::BOLD)
            .child(rustshell_core::i18n::t("确认粘贴"));

        let body = v_flex()
            .gap_2()
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(hex(theme.fg_muted))
                    .child(SharedString::from(rustshell_core::i18n::ts(
                        format!("将粘贴 {lines} 行："),
                        format!("About to paste {lines} line(s):"),
                    ))),
            )
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(hex(theme.warning))
                    .child(rustshell_core::i18n::t(
                        "含换行的内容会被逐行发送，命令会立即执行。",
                    )),
            )
            .child(
                // 等宽 + 深底的预览块（与「删除远端路径」那块同一手法）：
                // 逐行画而不是把整段塞一个 `child` —— gpui 的文本不会自己
                // 把 `\n` 断成新行，塞进去会显示成一行带方框的长文本。
                v_flex()
                    .w_full()
                    .gap_0p5()
                    .rounded(px(6.))
                    .p_2()
                    .bg(hex(theme.bg_base))
                    .border_1()
                    .border_color(hex(theme.border_subtle))
                    .font_family(rustshell_core::theme::default_terminal_font())
                    .text_size(px(12.))
                    .children(preview.split('\n').map(|line| {
                        div()
                            .text_ellipsis()
                            .child(SharedString::from(line.to_string()))
                    })),
            );

        let footer = h_flex()
            .gap_2()
            .justify_end()
            .pt_1()
            .child(
                Button::new("paste-cancel")
                    .small()
                    .label(rustshell_core::i18n::t("取消"))
                    .on_click(cx.listener(|m, _e, w, cx| {
                        // 先丢掉挂起（零写入），再关框 —— 关框那条 `on_close`
                        // 也会兜底调一次 `paste_prompt_decline`，它是幂等的。
                        m.paste_prompt_decline(cx);
                        m.close_dialog_ui(w, cx);
                    })),
            )
            .child(
                Button::new("paste-confirm")
                    .small()
                    .primary()
                    .label(rustshell_core::i18n::t("粘贴"))
                    .on_click(cx.listener(|m, _e, w, cx| m.paste_prompt_accept(w, cx))),
            );

        let view = cx.entity();
        dialog
            .w(px(520.))
            .bg(hex(theme.bg_overlay))
            .title(header)
            .child(body)
            .footer(footer)
            .button_props(
                DialogButtonProps::default()
                    // Enter 走官方 `Confirm` 动作，只认这里的 `on_ok` ——
                    // 自绘 `.footer()` 不会自动把按钮接上去（铁律 #2）。
                    .on_ok(move |_, w, cx| {
                        view.update(cx, |m, cx| m.paste_prompt_accept(w, cx));
                        // 业务侧负责关窗，故返回 false。
                        false
                    })
                    // Esc / 取消：与「取消」同义（放弃这次粘贴）。`on_close`
                    // 里还有一道兜底，覆盖点遮罩 / 右上角 X 两条路径。
                    .on_cancel(|_, _w, _cx| true),
            )
    }

    /// 打开「新建目录」对话框（文件面板工具栏入口）。
    ///
    /// `base` 是**当前目录**：对话框只收名字，新目录的完整路径由落库时
    /// `join_remote(base, name)` 拼出 —— 父目录名因此存在 `DialogMode` 里，
    /// 而不是靠菜单闭包捕获：从点击到落库之间隔着一次事件循环，
    /// 只有枚举里的值是可读回的事实来源（与 `SftpRemove` 同一口径）。
    pub fn open_sftp_mkdir_dialog(
        &mut self,
        base: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 开框即清空名称框：与「新建分组」同一口径 —— 同一个正文视图也承载
        // 重命名，上一个框里留下的名字还在，用户按确定就会建成一个他没打过的目录。
        self.io.group_name_body.update(cx, |v, cx| {
            v.set_label(rustshell_core::i18n::t("目录名称"));
            v.set_value(String::new(), window, cx);
            v.open(cx);
        });
        self.io.dlg_error = None;
        self.present_dialog(DialogMode::SftpMkdir { base }, window, cx);
    }

    /// 打开「重命名」对话框：预填的是**名字**（不含路径），与文件管理器一致 ——
    /// 把整条路径塞进去，用户一按确定就会以为自己在移动文件。
    pub fn open_sftp_rename_dialog(
        &mut self,
        from: String,
        dir: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = from.rsplit('/').next().unwrap_or(&from).to_string();
        self.io.group_name_body.update(cx, |v, cx| {
            v.set_label(rustshell_core::i18n::t("新名称"));
            v.set_value(name, window, cx);
            v.open(cx);
        });
        self.io.dlg_error = None;
        self.present_dialog(DialogMode::SftpRename { from, dir }, window, cx);
    }

    /// 「新建目录 / 重命名」共用的对话框：只差标题与按钮文案，正文是同一个
    /// 「收一个名字」的表单。
    ///
    /// 正文复用 `GroupNameDialogView` 而不是各自造一个：那个视图存在的理由是
    /// **输入框归子视图所有**（挂在 `MainView` 上时每敲一个字都会把整窗口重排一次，
    /// 见该类型文档）；再造一份就要把那套手法再实现一遍。
    fn render_sftp_name_dialog(
        &mut self,
        dialog: Dialog,
        _w: &mut Window,
        cx: &mut Context<Self>,
    ) -> Dialog {
        let t = crate::theme::current(cx);
        let mkdir = matches!(self.dialog, Some(DialogMode::SftpMkdir { .. }));

        let header = div()
            .text_size(px(14.))
            .font_weight(gpui::FontWeight::BOLD)
            .child(if mkdir {
                rustshell_core::i18n::t("新建目录")
            } else {
                rustshell_core::i18n::t("重命名")
            });

        // 改名对象是谁：重命名时把原路径摆出来 ——「同名不同目录」是这一层最容易
        // 搞错的事，而搞错之后是不可恢复的。
        let renamed_from = match self.dialog.clone() {
            Some(DialogMode::SftpRename { from, .. }) => Some(from),
            _ => None,
        };

        let footer = h_flex()
            .items_center()
            .justify_end()
            .gap_2()
            .pt_1()
            .child(
                Button::new("sftp-name-cancel")
                    .small()
                    .label(rustshell_core::i18n::t("取消"))
                    .on_click(cx.listener(|m, _e, w, cx| m.close_dialog_ui(w, cx))),
            )
            .child(
                Button::new("sftp-name-ok")
                    .small()
                    .primary()
                    .label(if mkdir {
                        rustshell_core::i18n::t("创建")
                    } else {
                        rustshell_core::i18n::t("保存")
                    })
                    .on_click(cx.listener(move |m, _e, w, cx| {
                        if mkdir {
                            m.sftp_mkdir_save(w, cx);
                        } else {
                            m.sftp_rename_save(w, cx);
                        }
                    })),
            );

        let view = cx.entity();
        let view_ok = view.clone();
        dialog
            .w(px(420.))
            .bg(hex(t.bg_overlay))
            .title(header)
            .child(
                v_flex()
                    .gap_2()
                    .when_some(renamed_from, |d, from| {
                        d.child(
                            div()
                                .w_full()
                                .text_size(px(11.))
                                .text_color(hex(t.fg_muted))
                                .text_ellipsis()
                                .debug_selector(|| "sftp-rename-from".to_string())
                                .child(SharedString::from(from)),
                        )
                    })
                    .child(self.io.group_name_body.clone()),
            )
            .footer(footer)
            .button_props(
                DialogButtonProps::default()
                    .on_ok(move |_, w, cx| {
                        // Enter 走官方 `Confirm` 动作，只认 `button_props.on_ok`：
                        // 自绘 footer 时它仍会被调用，不设就等于「Enter 关窗但没落库」。
                        view_ok.update(cx, |m, cx| {
                            if mkdir {
                                m.sftp_mkdir_save(w, cx);
                            } else {
                                m.sftp_rename_save(w, cx);
                            }
                        });
                        // 业务侧负责关窗（校验失败要保持对话框打开），故返回 false。
                        false
                    })
                    .on_cancel(move |_, w, cx| {
                        view.update(cx, |m, cx| m.close_dialog_ui(w, cx));
                        true
                    }),
            )
    }

    /// 落库「新建目录」：校验名字 → 发 `SftpCmd::Mkdir` → 关窗。
    ///
    /// 校验不过就把原因写进 `dlg_error` 并**保持对话框打开**（与分组名对话框同
    /// 一口径）：关掉再开一次会让用户刚敲的名字消失，而错误的输入还在那里等着重敲。
    fn sftp_mkdir_save(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        let name = self.io.group_name_body.read(cx).value(cx);
        let Some(DialogMode::SftpMkdir { base }) = self.dialog.clone() else {
            return;
        };
        if let Err(why) = rustshell_core::sftp::check_entry_name(&name) {
            self.io.dlg_error = Some(sftp_name_check_text(why).to_string());
            cx.notify();
            return;
        }
        let Some(tx) = self.active_tab().and_then(|t| t.session.sftp_tx.clone()) else {
            // 面板开着的时候会话被关掉了：说清楚，而不是发一条没人接的命令。
            self.io.dlg_error = Some(rustshell_core::i18n::t("需要 SSH 会话").to_string());
            cx.notify();
            return;
        };
        let _ = tx.try_send(rustshell_core::sftp::SftpCmd::Mkdir {
            path: rustshell_core::sftp::join_remote(&base, &name),
            refresh: base,
        });
        self.close_dialog_ui(w, cx);
    }

    /// 落库「重命名」：校验名字 → 发 `SftpCmd::Rename` → 关窗。
    fn sftp_rename_save(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        let name = self.io.group_name_body.read(cx).value(cx);
        let Some(DialogMode::SftpRename { from, dir }) = self.dialog.clone() else {
            return;
        };
        if let Err(why) = rustshell_core::sftp::check_entry_name(&name) {
            self.io.dlg_error = Some(sftp_name_check_text(why).to_string());
            cx.notify();
            return;
        }
        let to = rustshell_core::sftp::join_remote(&dir, &name);
        if to == from {
            // 名字一个字都没改：直接关窗，不发命令。`RENAME` 到同一个路径在部分
            // 服务端会被判成「目标已存在」而失败，而那条失败路径会连带丢掉
            // 整个 SFTP 会话（见 `spawn_sftp_service` 对 `Err` 的处理）。
            self.close_dialog_ui(w, cx);
            return;
        }
        let Some(tx) = self.active_tab().and_then(|t| t.session.sftp_tx.clone()) else {
            self.io.dlg_error = Some(rustshell_core::i18n::t("需要 SSH 会话").to_string());
            cx.notify();
            return;
        };
        let _ = tx.try_send(rustshell_core::sftp::SftpCmd::Rename {
            from,
            to,
            refresh: dir,
        });
        self.close_dialog_ui(w, cx);
    }

    fn resolve_trust(&mut self, trust: bool, w: &mut Window, cx: &mut Context<Self>) {
        if let Some(req) = self.conn_ui.pending_trust.take() {
            let _ = req.reply.send(trust);
        }
        self.conn_ui.trust_dialog_open = false;
        w.close_dialog(cx);
        cx.notify();
    }

    /// 信任框被「关掉」（X / ESC / 点遮罩）而不是被按钮回答时的收尾。
    ///
    /// 必须当成**不信任**处理：连接线程此刻正 `await` 那个 `reply`，不发就等于让它
    /// 白等满 120s 的 `TRUST_TIMEOUT`，而屏幕上已经没有任何东西提示它还在等。
    ///
    /// **不要**在这里调 `w.close_dialog(cx)`：官方在 `on_close` 之后自己会 pop 栈顶，
    /// 再关一次就会把下面那层（例如正在测试连接的连接对话框）一起关掉。
    fn reject_trust_on_dismiss(&mut self) {
        if let Some(req) = self.conn_ui.pending_trust.take() {
            let _ = req.reply.send(false);
        }
        self.conn_ui.trust_dialog_open = false;
    }

    /// 打开「首次信任 / 密钥已变更」对话框。
    ///
    /// **不走 `present_dialog`**（那条路开头有 `has_active_dialog` 守卫，挡的是「重复开
    /// 同一扇窗」）：信任框必须能叠在**任何**业务对话框之上。最典型的就是 2026-10-21
    /// 用户要求的场景 ——「新建/编辑连接 → 测试连接」时连接对话框开着，而连接线程正卡在
    /// 主机密钥校验上等一个答案。官方对话框层是栈（逐层渲染、关闭只弹栈顶），
    /// 与 OpenSSH 导入层 / 免确认白名单同一手法。
    pub(super) fn present_trust_dialog(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        if self.conn_ui.pending_trust.is_none() {
            return;
        }
        let view = cx.entity();
        w.open_dialog(cx, move |dialog, w, cx| {
            view.update(cx, |m, cx| m.render_trust_dialog(dialog, w, cx))
        });
        cx.notify();
    }

    fn render_trust_dialog(
        &mut self,
        dialog: Dialog,
        _w: &mut Window,
        cx: &mut Context<Self>,
    ) -> Dialog {
        use gpui::Styled as _;
        let t = crate::theme::current(cx);
        let Some(req) = &self.conn_ui.pending_trust else {
            return dialog;
        };
        let host_label = format!("{}:{}", req.host, req.port);
        let fingerprint = req.fingerprint.clone();
        let key_type = req.key_type.clone();
        // 「首次连接」与「密钥已变更」是两件性质完全不同的事，文案必须分开：
        // 前者核对指纹即可；后者要先说清「变了」，并把**旧指纹**并排给出来 ——
        // 容器重建后重新信任，靠的就是这一屏（旧实现里这种情况直接扔一句
        // `Unknown server key`，界面上没有任何可操作入口）。
        let changed = match &req.kind {
            rustshell_core::ssh::ServerKeyChange::Changed {
                old_fingerprint,
                old_key_type,
            } => Some((old_fingerprint.clone(), old_key_type.clone())),
            rustshell_core::ssh::ServerKeyChange::FirstSeen => None,
        };

        let header = div()
            .text_size(px(14.))
            .font_weight(gpui::FontWeight::BOLD)
            .child(if changed.is_some() {
                rustshell_core::i18n::t("主机密钥已变更：请先核对指纹")
            } else {
                rustshell_core::i18n::t("首次连接：验证主机指纹")
            });

        let intro = if changed.is_some() {
            rustshell_core::i18n::ts(
                format!(
                    "主机 {host_label} 本次提供的服务器密钥，与上次连接时记录的不一致。\
                     容器被重建、系统重装、或 sshd 重新生成了 host key 都会如此；\
                     但也可能是中间人攻击。"
                ),
                format!(
                    "Host {host_label} presented a different server key than the one \
                     recorded last time. Rebuilt containers, reinstalled systems and \
                     regenerated sshd host keys all cause this — but so can a \
                     man-in-the-middle attack."
                ),
            )
        } else {
            rustshell_core::i18n::ts(
                format!("主机 {host_label} 的真实性无法确认，请核对以下指纹："),
                format!(
                    "Cannot verify the authenticity of host {host_label}; \
                     check the fingerprint below:"
                ),
            )
        };

        let mut body = v_flex()
            .gap_2()
            // 量取钩子：用例要证明「业务对话框开着时，信任框仍然叠得上来」
            // —— 那个场景正是「测试连接遇到的未知主机」。
            .debug_selector(|| "dlg-trust-body".to_string())
            .child(
            div()
                .text_size(px(12.))
                .text_color(hex(t.fg_muted))
                .child(SharedString::from(intro)),
        );
        if let Some((old_fp, old_type)) = &changed {
            body = body
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(hex(t.fg_muted))
                        .child(SharedString::from(rustshell_core::i18n::ts(
                            format!("旧指纹（known_hosts 记录，{old_type}）：{old_fp}"),
                            format!("Old fingerprint (known_hosts, {old_type}): {old_fp}"),
                        ))),
                )
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(hex(t.fg_primary))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .child(SharedString::from(rustshell_core::i18n::ts(
                            format!("新指纹（服务器本次提供，{key_type}）：{fingerprint}"),
                            format!("New fingerprint (presented now, {key_type}): {fingerprint}"),
                        ))),
                );
        } else {
            body = body
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(hex(t.fg_primary))
                        .child(SharedString::from(rustshell_core::i18n::ts(
                            format!("密钥类型：{key_type}"),
                            format!("Key type: {key_type}"),
                        ))),
                )
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(hex(t.fg_primary))
                        .child(SharedString::from(rustshell_core::i18n::ts(
                            format!("指纹：{fingerprint}"),
                            format!("Fingerprint: {fingerprint}"),
                        ))),
                );
        }
        body = body.child(
            div()
                .text_size(px(11.))
                .text_color(hex(t.fg_muted))
                .pt_1()
                .child(if changed.is_some() {
                    rustshell_core::i18n::t(
                        "确认后将以新密钥替换旧记录并继续连接。若无法确认这次变化来自你自己（重建容器 / 重装系统 / 换机器），请取消 —— 这可能是中间人攻击。",
                    )
                } else {
                    rustshell_core::i18n::t(
                        "信任后该密钥将加入白名单；若指纹与服务器管理员公布的不一致，请勿信任。",
                    )
                }),
        );

        let footer = h_flex()
            .gap_2()
            .justify_end()
            .pt_1()
            .child(
                Button::new("trust-reject")
                    .small()
                    .label(if changed.is_some() {
                        rustshell_core::i18n::t("取消连接")
                    } else {
                        rustshell_core::i18n::t("取消")
                    })
                    .on_click(cx.listener(|m, _e, w, cx| {
                        m.resolve_trust(false, w, cx);
                    })),
            )
            .child(
                Button::new("trust-accept")
                    .small()
                    .primary()
                    .label(if changed.is_some() {
                        rustshell_core::i18n::t("更新密钥并连接")
                    } else {
                        rustshell_core::i18n::t("信任并连接")
                    })
                    .on_click(cx.listener(|m, _e, w, cx| {
                        m.resolve_trust(true, w, cx);
                    })),
            );

        let view = cx.entity();
        let view_ok = view.clone();
        let view_close = view.clone();
        dialog
            .w(px(440.))
            .bg(hex(t.bg_overlay))
            .title(header)
            .child(body)
            .footer(footer)
            .button_props(
                DialogButtonProps::default()
                    .on_ok(move |_, w, cx| {
                        // Enter 走官方 `Confirm` 动作，只认 `button_props.on_ok`；
                        // 镜像 footer 的「信任并连接」。指纹确认这种不可逆选择同样不该被
                        // 一个静默关窗糊过去。
                        view_ok.update(cx, |m, cx| m.resolve_trust(true, w, cx));
                        // 业务侧负责关窗（校验失败要保持对话框打开），故返回 false。
                        false
                    })
                    .on_cancel(move |_, w, cx| {
                        view.update(cx, |m, cx| m.resolve_trust(false, w, cx));
                        true
                    }),
            )
            // X / ESC / 点遮罩：官方在 `on_close` 之后自己 pop 这一层，
            // 所以这里只发「不信任」并复位标记（见 `reject_trust_on_dismiss`）。
            .on_close(move |_ev, _w, cx| {
                view_close.update(cx, |m, _cx| m.reject_trust_on_dismiss());
            })
    }
}

/// 目标选择方式（单选）。**没有「标签」**：标签分类已按运行反馈撤下。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum TargetRoom {
    Group,
    Profiles,
}

/// 可选项列表的高度上限（超过就滚动）。
///
/// 196px（而不是更矮）：用户明确要求「加长一点」，一屏能看到更多连接才不用
/// 每次都滚。
const PICKER_MAX_H: f32 = 196.;

/// 可选项列表的统一外观：带边框、限高、可滚动。
///
/// 连接可能有几十上百条，不限高就会把对话框撑到屏幕外 —— 而对话框是自绘结构，
/// 撑出去的部分用户够不着。
///
/// **限高只能加在最外层**（2026-09-21 修）：`overflow_y_scrollbar` 会把被滚动的
/// 元素本身强制成 `h_auto + min_h_full`，同时把它的 `max_h` 原样复制到外层包装上。
/// 若把 `max_h` 加在滚动元素上，**内容和视口会双双封在 196 里** —— 内容恰好等于
/// 视口，滚动量为 0，滚动条永远不出现（用户实测「不能滚动」就是这个）。
/// 正确结构照抄 gpui-component 自己的 `MaxHeightParentTest`：限高容器 →
/// `flex_1 + overflow_hidden` 视口 → `size_full` 的滚动元素。
///
/// 行要先收集成 `Vec` 再进来：`overflow_y_scrollbar()` 返回的是
/// `Scrollable<Stateful<Div>>`，它**不是** `Div`，因此不能「先建盒子再逐行 child」。
fn picker_box(
    id: &'static str,
    t: &rustshell_core::theme::ThemeTokens,
    rows: Vec<gpui::AnyElement>,
) -> gpui::AnyElement {
    use gpui::Styled as _;
    v_flex()
        .w_full()
        .max_h(px(PICKER_MAX_H))
        // 测试钩子：用例量的是**这只盒子**（可见的那一层）的高度；内部的滚动
        // 元素在长列表下是「内容高度」（比盒子高），量它会量错。
        .debug_selector(move || format!("{id}-box"))
        .rounded(px(4.))
        .border_1()
        .border_color(hex(t.border_subtle))
        .bg(hex(t.bg_base))
        .child(
            // 视口槽 + 滚动体两级，照抄 gpui-component 自己的 `MaxHeightParentTest`：
            // 中间那层 `flex_1 + overflow_hidden` 让限高容器不必按内容撑高
            // （flex 子项的自动最小高度），滚动才会发生在这层里面。
            v_flex().flex_1().overflow_hidden().child(
                v_flex().flex_1().overflow_hidden().child(
                    v_flex()
                        .id(id)
                        .debug_selector(move || id.to_string())
                        .size_full()
                        .flex_col()
                        .gap_0p5()
                        .p_1()
                        .children(rows)
                        .overflow_y_scrollbar(),
                ),
            ),
        )
        .into_any_element()
}

/// 空列表时的提示（比一片空白有用：用户会以为「列表加载失败」）。
fn picker_empty(t: &rustshell_core::theme::ThemeTokens) -> gpui::AnyElement {
    use gpui::Styled as _;
    div()
        .px_1()
        .py_2()
        .text_size(px(11.))
        .text_color(hex(t.fg_muted))
        .child(SharedString::from(rustshell_core::i18n::t(
            "还没有可选项，请先在连接面板里建好分组或连接",
        )))
        .into_any_element()
}

/// 一行可选项：`id` / 显示名 / 是否勾选 / 点击回调。
fn pick_row(
    kind: &'static str,
    id: String,
    label: String,
    on: bool,
    cx: &mut Context<MainView>,
    toggle: fn(&mut MainView, String),
) -> gpui::AnyElement {
    use gpui::Styled as _;
    let t = crate::theme::current(cx);
    let row_id: SharedString = format!("task-pick-{kind}-{id}").into();
    let click_id = id.clone();
    h_flex()
        .id(row_id)
        .w_full()
        .min_w_0()
        // 不许压缩：flex 子项默认 `flex_shrink: 1`，行会被压扁到盒子高度里，
        // 于是列表永远「装得下」、滚动条永远不出现 —— 用户实测滚不动就是这个。
        .flex_shrink_0()
        .gap_2()
        .items_center()
        .px_1()
        .py_0p5()
        .rounded(px(3.))
        .cursor_pointer()
        .hover(|s| s.bg(hex(t.bg_overlay)))
        // 测试钩子：`debug_bounds` 只收 `&'static str`，故按「种类 + 是否勾选」
        // 给固定选择器，用例据此确认列表真的渲染出了行、勾选状态也对。
        .debug_selector(move || format!("task-pick-{kind}-{}", if on { "on" } else { "off" }))
        .child(
            gpui_component::checkbox::Checkbox::new(SharedString::from(format!(
                "task-pick-box-{kind}-{id}"
            )))
            .xsmall()
            .checked(on),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(px(11.))
                .text_color(hex(if on { t.fg_primary } else { t.fg_secondary }))
                .child(SharedString::from(label)),
        )
        .on_click(cx.listener(move |m, _e, _w, cx| {
            toggle(m, click_id.clone());
            cx.notify();
        }))
        .into_any_element()
}

/// 分组选择器（**单选**）：点一行即选中，再点取消。
fn group_picker(
    options: &[(String, String)],
    selected: Option<&str>,
    empty: bool,
    cx: &mut Context<MainView>,
) -> gpui::AnyElement {
    let t = crate::theme::current(cx);
    if empty {
        return picker_box("task-pick-groups", &t, vec![picker_empty(&t)]);
    }
    let mut rows = Vec::new();
    for (id, name) in options {
        let on = selected == Some(id.as_str());
        rows.push(pick_row(
            "group",
            id.clone(),
            name.clone(),
            on,
            cx,
            |m, id| {
                // 再点一次取消：用户需要一条「不选任何分组」的退路
                m.io.dlg_task_group_sel = if m.io.dlg_task_group_sel.as_deref() == Some(id.as_str())
                {
                    None
                } else {
                    Some(id)
                };
            },
        ));
    }
    picker_box("task-pick-groups", &t, rows)
}

/// 连接选择器（**多选**）。
fn profile_picker(
    options: &[(String, String)],
    selected: &[String],
    empty: bool,
    cx: &mut Context<MainView>,
) -> gpui::AnyElement {
    let t = crate::theme::current(cx);
    if empty {
        return picker_box("task-pick-profiles", &t, vec![picker_empty(&t)]);
    }
    let mut rows = Vec::new();
    for (id, name) in options {
        let on = selected.contains(id);
        rows.push(pick_row(
            "profile",
            id.clone(),
            name.clone(),
            on,
            cx,
            |m, id| {
                if let Some(pos) = m.io.dlg_task_profiles_sel.iter().position(|x| *x == id) {
                    m.io.dlg_task_profiles_sel.remove(pos);
                } else {
                    m.io.dlg_task_profiles_sel.push(id);
                }
            },
        ));
    }
    picker_box("task-pick-profiles", &t, rows)
}

/// 认证方式 → 两个凭据字段的落盘取值（**纯函数，可单测**）。
///
/// 只留被选中那一侧：
/// - 密码认证：`key_path` 置空（这条连接不再用钥匙）；
/// - 私钥认证：`password` 置空 —— 否则 `core::ssh::connect_only` 的「密码优先」
///   会拿旧口令去登录，用户选了私钥却被密码挡回来。
///
/// 抽成纯函数是因为它的错法是**静默**的：两边都留着照样能连上，只是用的不是
/// 用户选的那条路径，谁都不会发现。
fn credential_sides(
    auth: AuthMode,
    pass: &str,
    key_path: &str,
) -> (Option<String>, Option<String>) {
    match auth {
        AuthMode::Password => (
            (!pass.is_empty()).then(|| pass.to_string()),
            None,
        ),
        AuthMode::Key => (
            None,
            (!key_path.is_empty()).then(|| key_path.to_string()),
        ),
    }
}

/// 单选行：`RadioGroup` 太重（只有两个选项且要跟草稿状态走），
/// 用 xsmall `Checkbox` + 整行点击实现单选。
fn radio_row(
    id: &'static str,
    label: &'static str,
    on: bool,
    cx: &mut Context<MainView>,
) -> (gpui::AnyElement, gpui::AnyElement) {
    let t = crate::theme::current(cx);
    use crate::theme::to_hsla as hex;
    // 局部引入：本文件的其它渲染函数同样这么做（`gap_1` / `text_size` 来自 `Styled`）
    use gpui::Styled as _;
    let boxed = gpui_component::checkbox::Checkbox::new(SharedString::from(format!("{id}-box")))
        .xsmall()
        .checked(on)
        .into_any_element();
    let text = div()
        .text_size(px(11.))
        .text_color(hex(if on { t.fg_primary } else { t.fg_secondary }))
        .child(SharedString::from(label))
        .into_any_element();
    let _ = cx;
    (boxed, text)
}

pub(super) fn pick_target_radio(
    id: &'static str,
    label: &'static str,
    on: bool,
    room: TargetRoom,
    cx: &mut Context<MainView>,
) -> impl IntoElement {
    let (boxed, text) = radio_row(id, label, on, cx);
    h_flex()
        .id(id)
        .gap_1()
        .items_center()
        .cursor_pointer()
        .child(boxed)
        .child(text)
        .on_click(cx.listener(move |m, _e, _w, cx| {
            m.io.task_target_kind = match room {
                TargetRoom::Group => 0,
                TargetRoom::Profiles => 1,
            };
            // 切换方式时**清掉另一种方式的目标**：不清的话「先选了分组、
            // 又切到连接」会留下两边都有值，而 `task_collect_draft` 只取一边，
            // 用户看到自己选过的东西「没生效」。
            match room {
                TargetRoom::Group => m.io.dlg_task_profiles_sel.clear(),
                TargetRoom::Profiles => m.io.dlg_task_group_sel = None,
            }
            cx.notify();
        }))
}

/// 通知策略的一个复选框（`slot` 对应 `dlg_task_notify` 的三元组下标）。
pub(super) fn notify_check(
    id: &'static str,
    label: &'static str,
    on: bool,
    slot: u8,
    cx: &mut Context<MainView>,
) -> impl IntoElement {
    let (boxed, text) = radio_row(id, label, on, cx);
    h_flex()
        .id(id)
        .gap_1()
        .items_center()
        .cursor_pointer()
        .child(boxed)
        .child(text)
        .on_click(cx.listener(move |m, _e, _w, cx| {
            let target = match slot {
                0 => &mut m.io.dlg_task_notify.0,
                1 => &mut m.io.dlg_task_notify.1,
                _ => &mut m.io.dlg_task_notify.2,
            };
            *target = !*target;
            cx.notify();
        }))
}

impl MainView {
    /// 「发一次 AI 测试」：表单内容先落库，再**真的跑一次**。
    ///
    /// 与旧「试运行」的区别（用户反馈，2026-09-21）：旧按钮只做纯计算
    /// （解析目标 / 风险级 / 并发），用户看不到模型到底巡检出什么；现在直接发
    /// 一次真实运行 —— 跑完会在 AI 助手里留下会话、运行记录里留下一条，
    /// 「这条要求写得好不好」当场就能看结果。
    ///
    /// 触发前先按表单落库（调度器读的是它自己那份 store），等于「保存并立即
    /// 执行一次」；对话框随之关闭，用户才看得见 AI 面板里跑出来的会话。
    /// 校验失败则**保持对话框打开**（错误显示在正文的错误位）。
    pub(super) fn task_send_ai_test(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.task_collect_draft(cx);
        let Some(draft) = self.io.task_draft.clone() else {
            return;
        };
        if let Err(e) = draft.validate() {
            self.io.task_dlg_error = Some(e);
            cx.notify();
            return;
        }
        // 目标解析必须非空：一个连接都没选到，测试就跑不出任何结果
        let preview = rustshell_core::runner::dry_run(&draft, &self.conn_store);
        if preview.targets.is_empty() {
            self.io.task_dlg_error = Some(rustshell_core::i18n::ts(
                "没有选到任何连接，请在上面的列表里勾选分组或连接".into(),
                "no connection selected; pick a group or connection above".into(),
            ));
            cx.notify();
            return;
        }
        let id = draft.id.clone();
        let now = rustshell_core::connections::unix_now_secs();
        self.task_store_update(|s| {
            if s.get(&id).is_some() {
                s.replace(&id, draft, now);
            } else {
                s.add(draft);
            }
        });
        self.io.dlg_task_is_new = false;
        let sched = self.scheduler();
        let backend = self.task_backend(&sched);
        if let Err(e) = sched.run_now(&id, &backend as &dyn rustshell_core::runner::TaskBackend) {
            // 极少见（任务刚被删 / 应用正在退出）：留在对话框里说明原因
            self.io.task_dlg_error = Some(e);
            cx.notify();
            return;
        }
        self.io.task_draft = None;
        self.io.task_dlg_error = None;
        self.dialog = None;
        window.close_dialog(cx);
        cx.notify();
    }
}

#[cfg(test)]
pub(in crate::main_view) mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// 搭一个与生产同构的窗口：Root → AppShell → MainView（对话层由 AppShell 挂）。
    ///
    /// `pub(in crate::main_view)` 供同级的 `whitelist_dialog` 复用：白名单框也是
    /// 「叠在设置面板之上的一层对话」，要量它的真实几何就得有同一套窗口。
    pub(in crate::main_view) fn build_main(
        cx: &mut gpui::TestAppContext,
    ) -> (Entity<MainView>, &mut gpui::VisualTestContext) {
        use gpui::AppContext as _;
        cx.update(|cx| {
            gpui_component::init(cx);
            crate::theme::apply_theme_mode(cx, rustshell_core::theme::ThemeMode::Dark);
        });
        let slot: Rc<RefCell<Option<Entity<MainView>>>> = Rc::new(RefCell::new(None));
        let (_, vcx) = {
            let slot = slot.clone();
            cx.add_window_view(move |window, cx| {
                let main = cx.new(|cx| MainView::new(window, cx));
                *slot.borrow_mut() = Some(main.clone());
                let shell = cx.new(|_| super::super::AppShell::new(main));
                gpui_component::Root::new(shell, window, cx)
            })
        };
        vcx.run_until_parked();
        let main = slot.borrow().clone().expect("MainView 未创建");
        (main, vcx)
    }

    /// 回归：设置面板的拖动把手曾在**视图 render 里**调用 `Window::on_mouse_event`
    /// 注册窗口级鼠标监听。gpui 对此有硬约束——该方法只能出现在绘制阶段
    /// （`Invalidator::debug_assert_paint`），而视图 render 跑在 prepaint 阶段，
    /// 于是 debug 构建一打开设置面板就 panic：
    /// `this method can only be called during paint`，进程随即整体退出。
    ///
    /// 本用例把「打开设置面板」的完整渲染真跑一遍。断言点不是像素，而是
    /// 「这一帧渲染没有 panic」+「对话层确实激活」，因此对主题、DPI、字号都不敏感；
    /// 反过来，若把窗口级监听挪回 render，这里会立刻红。
    #[gpui::test]
    fn settings_dialog_renders_without_paint_phase_panic(cx: &mut gpui::TestAppContext) {
        use gpui_component::WindowExt as _;
        let (main, vcx) = build_main(cx);

        // 打开设置面板：panic 发生在紧随其后的那一帧（对话层渲染里跑构建闭包）
        main.update_in(vcx, |m, window, cx| {
            m.present_dialog(DialogMode::Settings, window, cx);
        });
        vcx.run_until_parked();

        vcx.update(|window, cx| {
            assert!(
                window.has_active_dialog(cx),
                "设置面板没有进入对话层，本用例会退化成空断言"
            );
        });
    }

    /// 连接表单撤掉「分组」一栏之后**仍然画得出来**，且归属照旧跟着上下文走。
    ///
    /// 结构护栏（`main_view.rs` 的 `connect_dialog_has_no_group_field`）只看源码：
    /// 删多一行 `.child(..)` 会不会把整个 chain 弄塌、归属还被不被写进 `dlg_group_id`，
    /// 只有真开一次对话框才知道。归属的断言刻意走公开入口（`Some("g-x")` / `None`），
    /// 不依赖本机 `connections.json` 里有没有那个分组 —— 表单里已经没有按 id 查名字
    /// 的渲染路径，未知 id 也画得出来。
    #[gpui::test]
    fn connect_dialog_renders_without_the_group_field(cx: &mut gpui::TestAppContext) {
        use gpui_component::WindowExt as _;
        let (main, vcx) = build_main(cx);

        main.update_in(vcx, |m, window, cx| {
            m.open_connect_dialog(Some("g-x".to_string()), window, cx);
        });
        vcx.run_until_parked();
        vcx.update(|window, cx| {
            assert!(
                window.has_active_dialog(cx),
                "新建连接对话框没打开，本用例会退化成空断言"
            );
        });
        let form = vcx
            .debug_bounds("dlg-connect-form")
            .expect("连接表单没渲染出来（整条 chain 是不是被删塌了？）");
        assert!(
            f32::from(form.size.height) > 0.,
            "连接表单高度为 0 —— 表单塌了"
        );
        assert_eq!(
            main.read_with(vcx, |m, _| m.io.dlg_group_id.clone()),
            Some("g-x".to_string()),
            "分组右键「新建连接」带上来的归属没有落到 `dlg_group_id`：\
             表单里已经没有分组一栏，这条入参是归属的**唯一**来路。"
        );

        // 其余入口（栏顶 + / 树空白右键）传 None：**库里只有一个分组时落到它**，
        // 否则未分组（2026-10-21「初始化只建一个 default 文件夹」）。
        // 先把那个统一入口的两种情况都穷举出来，免得这条规则只靠源码注释自证。
        // 先关掉上一层：`present_dialog` 开头有 `has_active_dialog` 守卫，
        // 不关就再开等于什么都没发生（断言会假绿）。
        main.update_in(vcx, |m, window, cx| {
            m.close_dialog_ui(window, cx);
        });
        vcx.run_until_parked();

        // ① 库里一个分组都没有 → 未分组（不能凭空造一个 id 出来）
        main.update_in(vcx, |m, window, cx| {
            m.conn_store.groups.clear();
            m.open_connect_dialog(None, window, cx);
        });
        vcx.run_until_parked();
        assert_eq!(
            main.read_with(vcx, |m, _| m.io.dlg_group_id.clone()),
            None,
            "没有任何分组时，新建连接必须未分组。"
        );

        // ② 恰好一个分组 → 落到它（这就是 `default` 的行为）
        main.update_in(vcx, |m, window, cx| {
            m.close_dialog_ui(window, cx);
        });
        vcx.run_until_parked();
        let only = main.update_in(vcx, |m, window, cx| {
            m.conn_store
                .groups
                .push(rustshell_core::connections::ConnectionGroup::new("default"));
            let id = m.conn_store.groups[0].id.clone();
            m.open_connect_dialog(None, window, cx);
            id
        });
        vcx.run_until_parked();
        assert_eq!(
            main.read_with(vcx, |m, _| m.io.dlg_group_id.clone()),
            Some(only.clone()),
            "库里只有一个分组时，新建连接应落到它（默认 default 文件夹的唯一作用）。"
        );

        // ③ 两个及以上 → 不猜，回到未分组
        main.update_in(vcx, |m, window, cx| {
            m.close_dialog_ui(window, cx);
        });
        vcx.run_until_parked();
        main.update_in(vcx, |m, window, cx| {
            m.conn_store
                .groups
                .push(rustshell_core::connections::ConnectionGroup::new("另一个组"));
            m.open_connect_dialog(None, window, cx);
        });
        vcx.run_until_parked();
        assert_eq!(
            main.read_with(vcx, |m, _| m.io.dlg_group_id.clone()),
            None,
            "多分组下自动归属就是猜，猜错比不猜更烦 —— 必须保持未分组。"
        );
    }

    /// 2026-09-29 用户要求「隐藏新增连接-高级相关的内容，保持简单」：连接对话框
    /// 是**单页简单表单** —— 没有页签栏，也没有端口转发 / 代理 / 会话日志 /
    /// 断线自动重连这些「高级」内容。
    ///
    /// 契约分两半，缺一不可：
    /// ① 简单表单在屏、可量高（不能「隐藏」到把表单一起删塌）；
    /// ② 页签栏与高级页的钩子在渲染树里量不到（新建、编辑两条入口都验）。
    ///
    /// 变异验证：把页签栏或高级页加回 `render_dialog`，② 会红。
    #[gpui::test]
    fn the_connect_dialog_is_a_single_simple_form(cx: &mut gpui::TestAppContext) {
        use gpui_component::WindowExt as _;
        let (main, vcx) = build_main(cx);

        main.update_in(vcx, |m, window, cx| {
            m.open_connect_dialog(None, window, cx);
        });
        vcx.run_until_parked();

        vcx.update(|window, cx| {
            assert!(
                window.has_active_dialog(cx),
                "新建连接对话框没打开，本用例会退化成空断言"
            );
        });

        // ① 简单表单在屏、可量高
        let form = vcx
            .debug_bounds("dlg-connect-form")
            .expect("连接表单没渲染出来（整条 chain 是不是被删塌了？）");
        assert!(
            f32::from(form.size.height) > 0.,
            "连接表单高度为 0 —— 表单塌了"
        );
        // ② 页签栏 / 高级页不在渲染树里：钩子量得到就意味着「隐藏」没落实
        for gone in ["dlg-connect-tabs", "dlg-connect-advanced"] {
            assert!(
                vcx.debug_bounds(gone).is_none(),
                "`{gone}` 又出现在对话框里 —— 「高级」内容要隐藏得干干净净。"
            );
        }

        // 编辑入口共用同一个渲染函数，顺手也验一次（别只在「新建」上成立）
        let p = profile_with_proxy();
        main.update_in(vcx, |m, window, cx| m.close_dialog_ui(window, cx));
        vcx.run_until_parked();
        main.update_in(vcx, |m, window, cx| {
            m.open_connect_dialog_for_profile(&p, window, cx)
        });
        vcx.run_until_parked();
        assert!(
            vcx.debug_bounds("dlg-connect-form").is_some()
                && vcx.debug_bounds("dlg-connect-advanced").is_none(),
            "编辑连接也必须是同一张简单表单（新建与编辑共用 `render_dialog`）"
        );
    }

    /// 2026-10-21 用户要求「增加认证方式，默认密码认证，主要目的是减少界面内容」：
    /// 密码框与私钥行**互斥在场**，且默认只有密码认证。
    ///
    /// 量取钩子（`dlg-pass-row` / `dlg-key-row`）而不是数源码里的 `.child(..)`：
    /// 只有真渲染一次才知道那一支到底进没进树。
    #[gpui::test]
    fn auth_mode_swaps_the_credential_field(cx: &mut gpui::TestAppContext) {
        use gpui_component::WindowExt as _;
        let (main, vcx) = build_main(cx);

        main.update_in(vcx, |m, window, cx| m.open_connect_dialog(None, window, cx));
        vcx.run_until_parked();
        assert!(
            vcx.update(|window, cx| window.has_active_dialog(cx)),
            "新建连接对话框没打开，本用例会退化成空断言"
        );

        // 默认：密码认证 —— 私钥那一行不该在场
        assert_eq!(
            main.read_with(vcx, |m, _| m.io.dlg_auth),
            AuthMode::Password,
            "新建连接默认必须是密码认证"
        );
        assert!(
            vcx.debug_bounds("dlg-pass-row").is_some(),
            "密码认证下密码框必须在场"
        );
        assert!(
            vcx.debug_bounds("dlg-key-row").is_none(),
            "密码认证下不该出现私钥行 —— 「减少界面内容」要的就是它不占地方"
        );
        assert!(
            vcx.debug_bounds("dlg-connect-credentials").is_some(),
            "凭据卡片（用户名 + 认证方式）不该被分支漏掉"
        );

        // 切到私钥认证：两行的在场状态必须对调
        main.update_in(vcx, |m, _w, cx| {
            m.io.dlg_auth = AuthMode::Key;
            cx.notify();
        });
        vcx.run_until_parked();
        assert!(
            vcx.debug_bounds("dlg-key-row").is_some(),
            "切到私钥认证后私钥行必须出现"
        );
        assert!(
            vcx.debug_bounds("dlg-pass-row").is_none(),
            "切到私钥认证后密码框必须让位"
        );
    }

    /// 认证方式的落盘语义：**只留被选中那一侧**，另一侧清空。
    ///
    /// 这条错法是静默的 —— 两边都留着照样连得上，只是走的不是用户选的那条路。
    #[test]
    fn credential_sides_keeps_only_the_selected_one() {
        assert_eq!(
            credential_sides(AuthMode::Password, "pw", "/home/u/.ssh/id_ed25519"),
            (Some("pw".to_string()), None),
            "密码认证必须清掉私钥路径（否则用户以为自己在用钥匙）"
        );
        assert_eq!(
            credential_sides(AuthMode::Key, "pw", "/home/u/.ssh/id_ed25519"),
            (None, Some("/home/u/.ssh/id_ed25519".to_string())),
            "私钥认证必须清掉口令（否则 core 的「密码优先」会拿它去登录）"
        );
        assert_eq!(
            credential_sides(AuthMode::Password, "", "/k"),
            (None, None),
            "空口令就是空口令，不要凭空捏一个 Some(\"\")"
        );
    }

    /// 编辑既有连接时的认证方式反推：有私钥且没有口令 = 私钥认证。
    ///
    /// 反推错了后果很重：对话框会显示「密码认证」，用户一保存就把钥匙路径清掉了。
    #[test]
    fn auth_mode_is_recovered_from_the_stored_credentials() {
        let mut p = ConnectionProfile::new("k", "h", "u");
        assert_eq!(
            AuthMode::of_profile(&p),
            AuthMode::Password,
            "什么都没有时是默认的密码认证"
        );
        p.key_path = Some("/k".into());
        assert_eq!(
            AuthMode::of_profile(&p),
            AuthMode::Key,
            "只有私钥 = 私钥认证（否则一编辑一保存就把钥匙丢了）"
        );
        p.password = Some("pw".into());
        assert_eq!(
            AuthMode::of_profile(&p),
            AuthMode::Password,
            "两者都有时按 core 的实际顺序显示为密码认证（密码确实会先被试）"
        );
    }

    /// 2026-10-21 用户要求「测试连接功能移除无人值守之类的限制」：信任框必须能
    /// **叠在业务对话框之上**。
    ///
    /// 「测试连接」正是这个组合：连接对话框开着，后台线程卡在主机密钥校验上等答复。
    /// 旧实现里 `present_trust_dialog` 有 `has_active_dialog` 守卫，会把这一层直接
    /// 挡掉（而 `trust_dialog_open` 已置位 ⇒ 再也不重试），结果是连接线程白等满
    /// 120s，用户什么也看不到。判据是几何：信任框的 body 量得到。
    #[gpui::test]
    fn the_trust_dialog_stacks_on_top_of_the_connect_dialog(cx: &mut gpui::TestAppContext) {
        use gpui_component::WindowExt as _;
        let (main, vcx) = build_main(cx);

        // 业务层：新建连接对话框（测试连接就在它里面）
        main.update_in(vcx, |m, window, cx| m.open_connect_dialog(None, window, cx));
        vcx.run_until_parked();
        assert!(
            vcx.update(|window, cx| window.has_active_dialog(cx)),
            "前提丢了：连接对话框没开出来"
        );

        // 模拟连接线程送来一条「未知主机」信任请求，并走上屏路径。
        //
        // 这里直接调 `present_trust_dialog`（而不是等 `render` 的 `on_next_frame`
        // 触发）：本用例要守的是「业务对话框开着时这一层还叠不叠得上来」——
        // 旧实现的 `has_active_dialog` 守卫就在这个函数里，直接调用同样会被挡。
        let (tx, _rx) = flume::bounded(1);
        main.update_in(vcx, |m, window, cx| {
            m.conn_ui.pending_trust = Some(rustshell_core::ssh::ServerKeyPrompt {
                host: "10.0.0.1".into(),
                port: 22,
                fingerprint: "SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into(),
                key_type: "ssh-ed25519".into(),
                kind: rustshell_core::ssh::ServerKeyChange::FirstSeen,
                reply: tx,
            });
            m.present_trust_dialog(window, cx);
        });
        vcx.run_until_parked();

        assert!(
            vcx.debug_bounds("dlg-trust-body").is_some(),
            "信任框没叠上来 —— 连接测试遇到未知主机就只能干等（用户要求移除的限制）。"
        );
        // 业务对话框仍在栈里：关掉信任框之后还要回到它
        assert!(
            vcx.debug_bounds("dlg-connect-form").is_some(),
            "叠信任框时把下面的连接对话框挤掉了 —— 关掉信任框就回不去了。"
        );
    }

    /// 2026-10-21 用户要求「新增连接，主机字段，默认：localhost」。
    ///
    /// 两条一起守：① 新建时主机是 `localhost`、端口仍是 22；
    /// ② **重开一次**不能把上一条敲过的主机带出来（默认值必须是「每次新建的起点」，
    /// 不是「上次留下什么就是什么」—— 后者正是清空逻辑要防的那种串味）。
    #[gpui::test]
    fn new_connection_defaults_the_host_to_localhost(cx: &mut gpui::TestAppContext) {
        let (main, vcx) = build_main(cx);

        main.update_in(vcx, |m, window, cx| m.open_connect_dialog(None, window, cx));
        vcx.run_until_parked();

        let read = |vcx: &mut gpui::VisualTestContext| {
            main.read_with(vcx, |m, cx| {
                (
                    m.io.dlg_host.read(cx).value().to_string(),
                    m.io.dlg_port.read(cx).value().to_string(),
                )
            })
        };
        assert_eq!(
            read(vcx),
            ("localhost".to_string(), "22".to_string()),
            "新建连接的主机默认值必须是 localhost（端口仍默认 22）"
        );

        // 改成别的主机 → 关掉 → 再新建：必须回到 localhost，而不是留着 10.0.0.1
        main.update_in(vcx, |m, window, cx| {
            m.io.dlg_host
                .update(cx, |s, cx| s.set_value("10.0.0.1", window, cx));
            m.close_dialog_ui(window, cx);
        });
        vcx.run_until_parked();
        main.update_in(vcx, |m, window, cx| m.open_connect_dialog(None, window, cx));
        vcx.run_until_parked();
        assert_eq!(
            read(vcx).0,
            "localhost",
            "重开新建框时主机又带出了上一条的值 —— 默认值应当每次都重新落上"
        );
    }

    /// 回归：新建分组对话框开框即聚焦名称输入框（用户不必先点一下输入框）。
    #[gpui::test]
    fn new_group_dialog_focuses_name_input(cx: &mut gpui::TestAppContext) {
        use gpui_component::WindowExt as _;
        let (main, vcx) = build_main(cx);

        main.update_in(vcx, |m, window, cx| {
            m.open_new_group_dialog(None, None, window, cx);
        });
        vcx.run_until_parked();

        vcx.update(|window, cx| {
            assert!(window.has_active_dialog(cx), "新建分组对话框没打开");
        });
        main.update_in(vcx, |m, w, cx| {
            let body = m.io.group_name_body.clone();
            let handle = body.read(cx).focus_handle(cx);
            assert!(
                handle.is_focused(w),
                "开框后焦点不在分组名输入框上（用户还得先点一下才能打字）"
            );
        });
    }

    // ---------- 代理草稿（F-10） ----------

    /// 造一条带自定义代理的连接（口令与密文都填上，模拟 `restore_passwords` 解过的状态）。
    fn profile_with_proxy() -> rustshell_core::connections::ConnectionProfile {
        use rustshell_core::proxy::{ProxyChoice, ProxyConfig, ProxyKind};
        rustshell_core::connections::ConnectionProfile {
            id: "p-proxy".into(),
            name: "带代理的机器".into(),
            host: "10.0.0.9".into(),
            port: 22,
            username: "root".into(),
            password: None,
            stored_password: None,
            key_path: None,
            notes: String::new(),
            group_id: None,
            legacy_group: None,
            last_used_at: 0,
            tags: Vec::new(),
            forwards: Vec::new(),
            session_log: None,
            proxy: ProxyChoice::Custom(ProxyConfig {
                kind: ProxyKind::Socks5,
                host: "proxy.corp.lan".into(),
                port: 1080,
                username: Some("proxy-user".into()),
                password_ref: Some("dpapi-blob".into()),
                password: Some("proxy-pw".into()),
            }),
            auto_reconnect: None,
        }
    }

    /// ★ F-10：编辑**原样回填**、新建**整份清零**（档位 / 类型 / 地址 / 端口 / 用户名 /
    /// 口令 / 来源快照，一个都不能漏）。
    ///
    /// 「新建带出上一条连接的代理」是这条链上最贵的一类事故：地址和用户名都还在，
    /// 用户看不出这条连接挂着别人的代理 —— 一直到它开始借别人的出口出去才发现。
    /// 空口令的「沿用」更要紧：`dlg_proxy_prev` 没清，`may_inherit_password` 就会
    /// 拿着上一条连接的密文当自己的来源。
    #[gpui::test]
    fn the_new_dialog_wipes_the_proxy_draft_and_edit_restores_it(cx: &mut gpui::TestAppContext) {
        use gpui_component::WindowExt as _;
        use rustshell_core::proxy::{ProxyKind, ProxyMode};

        let (main, vcx) = build_main(cx);

        // ① 编辑：四个输入框 + 档位 + 来源快照全都要回填上
        let p = profile_with_proxy();
        main.update_in(vcx, |m, window, cx| {
            m.open_connect_dialog_for_profile(&p, window, cx);
        });
        vcx.run_until_parked();
        vcx.update(|window, cx| {
            assert!(
                window.has_active_dialog(cx),
                "编辑对话框没打开，下面的断言会假绿"
            );
        });
        let (mode, kind, pass, has_prev) = main.read_with(vcx, |m, cx| {
            (
                m.io.dlg_proxy_mode,
                m.io.dlg_proxy_kind,
                m.io.dlg_proxy_pass.read(cx).value().to_string(),
                m.io.dlg_proxy_prev.is_some(),
            )
        });
        assert_eq!(mode, ProxyMode::Custom, "档位没有回填成这条连接自己的口径");
        assert_eq!(kind, ProxyKind::Socks5, "代理类型没有回填");
        let draft = main.read_with(vcx, |m, cx| m.dlg_proxy_draft(cx));
        assert_eq!(
            (draft.host.as_str(), draft.port, draft.username.as_deref()),
            ("proxy.corp.lan", 1080, Some("proxy-user")),
            "地址 / 端口 / 用户名没有回填 —— 用户打开对话框看到的是别人的代理"
        );
        assert_eq!(
            pass, "proxy-pw",
            "口令要回填**明文**（`restore_passwords` 已解出），与连接口令同一口径"
        );
        assert!(
            has_prev,
            "`dlg_proxy_prev` 必须留一份原配置：`may_inherit_password` 全靠它判「目标变没变」"
        );

        // ② 新建：同一批输入状态，整份草稿必须清零
        main.update_in(vcx, |m, window, cx| m.close_dialog_ui(window, cx));
        vcx.run_until_parked();
        main.update_in(vcx, |m, window, cx| m.open_connect_dialog(None, window, cx));
        vcx.run_until_parked();
        vcx.update(|window, cx| {
            assert!(
                window.has_active_dialog(cx),
                "关掉再开就没开出来（够不着的守卫？），下面的清零断言会假绿"
            );
        });
        let (mode, kind, host, port, user, pass, has_prev) = main.read_with(vcx, |m, cx| {
            (
                m.io.dlg_proxy_mode,
                m.io.dlg_proxy_kind,
                m.io.dlg_proxy_host.read(cx).value().to_string(),
                m.io.dlg_proxy_port.read(cx).value().to_string(),
                m.io.dlg_proxy_user.read(cx).value().to_string(),
                m.io.dlg_proxy_pass.read(cx).value().to_string(),
                m.io.dlg_proxy_prev.is_some(),
            )
        });
        assert_eq!(
            mode,
            ProxyMode::FollowGlobal,
            "新建必须从「跟随全局」起步：留在「自定义」上会存出一条指向空地址的代理"
        );
        assert_eq!(kind, ProxyKind::Http, "代理类型要回到默认（HTTP）");
        assert_eq!(
            host, "",
            "上一条连接的代理地址留在「新建」里 = 新连接悄悄挂到别人的代理上"
        );
        assert_eq!(
            port,
            rustshell_core::proxy::DEFAULT_PROXY_PORT.to_string(),
            "端口要给初值（与主机表单「空端口落到 22」同一口径）"
        );
        assert_eq!(user, "", "代理用户名没清：地址与用户名都还写着上一条的");
        assert_eq!(
            pass, "",
            "代理口令没清：明文留在这里，等于把上一条的凭据送给新连接"
        );
        assert!(
            !has_prev,
            "`dlg_proxy_prev` 留着 = 新建连接有一个「来源」，留空就会沿用上一条的密文"
        );
    }

    /// F-4：「高级」三态行已按用户要求隐藏（2026-09-29），但**状态仍在走完整条
    /// 链路**：编辑回填进草稿、新建清回默认 —— 隐藏不等于清空。
    ///
    /// 界面上看不见它之后这条链更容易被顺手「清理」掉：真清了，用户一保存就把
    /// 「从不重连」这类表态静默抹成「跟随全局」，而对话框里已经没有任何地方
    /// 能让他发现这件事。
    ///
    /// 变异验证：删掉 `open_connect_dialog_for_profile` 里的
    /// `self.io.dlg_auto_reconnect = p.auto_reconnect;`，本用例会红。
    #[gpui::test]
    fn the_hidden_auto_reconnect_state_still_round_trips(cx: &mut gpui::TestAppContext) {
        let (main, vcx) = build_main(cx);

        // ① 编辑：这条连接的表态要原样带到草稿上（随后保存时原样写回）
        let mut p = profile_with_proxy();
        p.auto_reconnect = Some(false);
        main.update_in(vcx, |m, window, cx| {
            m.open_connect_dialog_for_profile(&p, window, cx)
        });
        vcx.run_until_parked();
        assert_eq!(
            main.read_with(vcx, |m, _| m.io.dlg_auto_reconnect),
            Some(false),
            "编辑对话框没有回填连接级三态 —— 用户一保存就把它抹成「跟随全局」"
        );

        // ② 新建：不回零的话，新连接会带着上一条的「从不重连」落库
        main.update_in(vcx, |m, window, cx| m.close_dialog_ui(window, cx));
        vcx.run_until_parked();
        main.update_in(vcx, |m, window, cx| m.open_connect_dialog(None, window, cx));
        vcx.run_until_parked();
        assert_eq!(
            main.read_with(vcx, |m, _| m.io.dlg_auto_reconnect),
            None,
            "新建没有把三态回零：新连接会带着上一条的「从不重连」落库，\n\
             而界面上已经没有那行字，用户更无从察觉"
        );
    }

    /// 2026-09-29「隐藏新增连接-高级相关的内容」：界面撤了，**保存路径不许跟着撤**。
    ///
    /// 编辑一条配过隧道 / 设过会话日志的连接，一保存把它们清空，用户要到很久
    /// 以后（隧道没起来 / 日志没落盘）才会发现 —— 而对话框里已经没地方看它们。
    /// 编辑与新建两条分支各要写回一次，这里把「两处」钉死。
    #[test]
    fn the_hidden_advanced_settings_are_still_written_back_on_save() {
        let code = crate::test_support::prod(include_str!("dialogs.rs"));
        assert_eq!(
            code.matches("p.forwards = forwards.clone();").count(),
            2,
            "编辑与新建两条分支都要写回端口转发规则（界面隐藏 ≠ 清空已落盘的规则）。"
        );
        assert_eq!(
            code.matches("p.session_log = session_log;").count(),
            2,
            "编辑与新建两条分支都要写回会话日志三态（隐藏后更要小心：界面上看不见它）。"
        );
    }

    /// F-10：`dlg_proxy_draft` 是两个消费点**唯一**的读法，端口口径钉在这里。
    ///
    /// 两条口径各挡一类事故：空端口 = 默认（用户清了框不该存出一条端口 0 的配置）；
    /// 非法 / 越界文本落成 0 交给 `validate` 报错 —— 在这里悄悄回退成默认值，
    /// `abc` 会变成 `8080`，用户永远不知道自己填错了。
    #[gpui::test]
    fn the_proxy_draft_parses_the_port_in_exactly_one_way(cx: &mut gpui::TestAppContext) {
        use rustshell_core::proxy::{validate_error_text, DEFAULT_PROXY_PORT};

        let (main, vcx) = build_main(cx);
        main.update_in(vcx, |m, window, cx| m.open_connect_dialog(None, window, cx));
        vcx.run_until_parked();

        // 像用户一样经 `InputState` 写值（直接改字段测不到 trim / 解析）
        let set = |vcx: &mut gpui::VisualTestContext, host: &str, port: &str, user: &str| {
            main.update_in(vcx, |m, window, cx| {
                m.io.dlg_proxy_host
                    .update(cx, |s, cx| s.set_value(host, window, cx));
                m.io.dlg_proxy_port
                    .update(cx, |s, cx| s.set_value(port, window, cx));
                m.io.dlg_proxy_user
                    .update(cx, |s, cx| s.set_value(user, window, cx));
            });
        };
        let draft =
            |vcx: &mut gpui::VisualTestContext| main.read_with(vcx, |m, cx| m.dlg_proxy_draft(cx));

        // ① 首尾空白收口 + 空端口 = 默认 + 空白用户名 = 匿名
        set(vcx, "  10.0.0.1  ", "", "   ");
        let d = draft(vcx);
        assert_eq!(d.host, "10.0.0.1", "地址首尾空白要在读出处收口");
        assert_eq!(d.port, DEFAULT_PROXY_PORT, "空端口 = 默认端口");
        assert_eq!(d.username, None, "空白用户名等于没填（匿名代理）");

        // ② 非法文本：不是回退成默认值，而是落 0 让 validate 报同一句话
        set(vcx, "10.0.0.1", "abc", "");
        let d = draft(vcx);
        assert_eq!(
            d.port, 0,
            "非法端口文本必须落成 0 交给 validate，不许悄悄变默认值"
        );
        assert_eq!(
            d.validate().map(validate_error_text),
            Some(rustshell_core::i18n::t("代理端口需为 1–65535 的数字")),
            "非法端口的报错文案必须与 core 的校验同一句"
        );

        // ③ 越界同理（`70000` 不能被截断成某个合法端口）
        set(vcx, "10.0.0.1", "70000", "");
        assert_eq!(draft(vcx).port, 0, "越界端口同样是「非法」而不是截断");

        // ④ 合法值原样进配置，且此时不再有校验错误
        set(vcx, "10.0.0.1", " 3128 ", "u1");
        let d = draft(vcx);
        assert_eq!((d.port, d.username.as_deref()), (3128, Some("u1")));
        assert_eq!(d.validate(), None, "填完整了就不该再有校验错误");
    }

    /// 回归（2026-09-17 运行反馈「打开软件默认不打开本地 shell，打开命令窗口」）：
    /// 启动请求（`request_command_palette`）必须真的在下一帧把命令窗口开出来 ——
    /// 它走的是「标记 → render 里消费」这条路（开浮层要 `Window`，启动路径没有），
    /// 只在 `render` 里接对线，用户才看得到。
    ///
    /// 顺带钉住面板内容：启动那一刻列表里必须已经有「新增连接」「AI助手」两条入口
    /// （启动不再自动开终端，用户就靠这两条 + 快速连接开工）。
    #[gpui::test]
    fn startup_request_opens_the_command_palette(cx: &mut gpui::TestAppContext) {
        let (main, vcx) = build_main(cx);
        assert!(
            main.read_with(vcx, |m, _| m.panel_ui.palette_state.is_none()),
            "初始不该已经开着命令窗口（本用例的前提）"
        );

        main.update(vcx, |m, cx| m.request_command_palette(cx));
        vcx.run_until_parked();

        let (open, labels) = main.read_with(vcx, |m, _| {
            (
                m.panel_ui.palette_state.is_some(),
                m.panel_ui
                    .palette_cmds
                    .iter()
                    .map(|c| c.label.clone())
                    .collect::<Vec<_>>(),
            )
        });
        assert!(open, "启动请求没有把命令窗口开出来");
        for want in ["新增连接", "AI助手"] {
            assert!(
                labels.iter().any(|l| l == want),
                "命令窗口里缺少启动入口「{want}」：实际列表 {labels:?}"
            );
        }
        assert!(
            !labels.iter().any(|l| l == "命令面板"),
            "命令窗口里不该再列「命令面板」自己：实际列表 {labels:?}"
        );
    }

    /// 回归（2026-09-29 用户要求「关掉全部 tab 不再触发命令窗口显示」）：
    /// 关掉最后一个标签**不再**自动弹命令窗口 —— 原先的 `palette_when_empty`
    /// 两段式（`close_tab_by_session` 置旗 → `render` 判空兑现，2026-09-18 加的）
    /// 整条撤掉。
    ///
    /// 撤掉的理由，写给将来想把它加回来的人：标签清空后的画面已经有自己的落点
    /// （`split_actions::render_no_session` 的空态：一张可照着按的快捷键表），
    /// 程序再替用户把命令窗口推上来，用户每关一次标签就得先关掉一个面板。
    /// 启动即开命令窗口是另一条路（`pending_palette`，由「启动动作」设置决定），
    /// 不受此影响。
    ///
    /// 两面都钉 + 一条源码负断言（防止「旗」以别的名字复活）。
    #[gpui::test]
    fn closing_the_last_tab_no_longer_opens_the_command_palette(cx: &mut gpui::TestAppContext) {
        use rustshell_core::session::{SessionHandle, SessionKind};

        let (main, vcx) = build_main(cx);
        assert!(
            main.read_with(vcx, |m, _| m.panel_ui.palette_state.is_none()),
            "初始不该已经开着命令窗口（本用例的前提）"
        );

        // 造两个「连接中」标签：不需要真 PTY —— 终端为 `None` 的标签走连接态
        // 占位渲染，这正是关标签所需的全部前置条件。
        main.update(vcx, |m, cx| {
            for id in ["t1", "t2"] {
                m.tabs_state.tabs.push(crate::main_view::TabEntry {
                    session: SessionHandle::pending(id.into(), SessionKind::Ssh, id.into(), None),
                    terminal: None,
                    scroll: None,
                    connect_error: None,
                    connect_started: Some(std::time::Instant::now()),
                    dead: None,
                    retry: None,
                });
            }
            m.tabs_state.active = Some("t1".into());
            cx.notify();
        });
        vcx.run_until_parked();

        // ① 关掉一半：还有标签在，不弹（本来就不该）。
        main.update(vcx, |m, cx| m.close_tab_by_session("t1", cx));
        vcx.run_until_parked();
        assert!(
            main.read_with(vcx, |m, _| m.panel_ui.palette_state.is_none()),
            "还有标签在就弹了命令窗口 —— 多标签用户每关一个都会平白多一个面板"
        );

        // ② 关掉最后一个：也不弹（本次改动的本体）。
        main.update(vcx, |m, cx| m.close_tab_by_session("t2", cx));
        vcx.run_until_parked();
        assert!(
            main.read_with(vcx, |m, _| m.tabs_state.tabs.is_empty()),
            "前提破坏：第二个标签没被关掉，下面的断言会变成假阴性"
        );
        assert!(
            main.read_with(vcx, |m, _| m.panel_ui.palette_state.is_none()),
            "关掉最后一个标签后又自动弹出了命令窗口（2026-09-29 起要求：不弹，\
             清空后的画面交给空态自己的快捷键表）"
        );

        // ③ 源码里不许再有这条机制的任何形态（字段 / 置旗 / 兑现块一起撤）。
        for (name, src) in [
            (
                "main_view.rs",
                crate::test_support::prod(include_str!("../main_view.rs")),
            ),
            (
                "connections.rs",
                crate::test_support::prod(include_str!("connections.rs")),
            ),
        ] {
            assert!(
                !src.contains("palette_when_empty"),
                "{name} 里还残留着「关掉全部 tab 自动弹命令窗口」的机制：\
                 撤行为要把旗与兑现块一起撤，只删一处会留下一个永远不触发的死字段。"
            );
        }
    }

    /// 回归（2026-09-17 运行反馈「重命名连接分组 enter 确认但分组名称没保存成功」）：
    ///
    /// 官方 `Dialog` 自绘 `.footer(..)` 时**不画** OK/Cancel 按钮，但 `Confirm` /
    /// `Cancel` 两个动作**仍会**调用 `button_props.on_ok` / `on_cancel`（Enter 派发
    /// 的正是 `Confirm`）。只设 `on_cancel` 而不设 `on_ok` 的写法会落到默认实现
    /// `|_| true` —— 于是 Enter 表现为「窗口关了，名字一个字都没存」。
    ///
    /// 本用例把这种「漏接线」写法钉死：任何一处 `on_cancel` 直接接在
    /// `DialogButtonProps::default()` 后面都算缺陷；同时正断言六个自绘 footer 的
    /// 对话框都挂了 `on_ok`。
    #[test]
    fn every_dialog_wires_enter_to_its_own_action() {
        let code = crate::test_support::prod(include_str!("dialogs.rs"));
        assert!(
            !code.contains("DialogButtonProps::default().on_cancel("),
            "有对话框只设了 `on_cancel` 没设 `on_ok`：Enter（官方 `Confirm` 动作）会直接\
             关窗，用户以为已经保存了。"
        );
        let wired = code.matches(".on_ok(move |_, w, cx| {").count();
        assert!(
            wired >= 6,
            "自绘 footer 的对话框少了 `on_ok` 接线（只找到 {wired} 处，应至少 6 处）"
        );
    }

    /// 任务编辑对话框必须**真的为被点的那一条**渲染出来，并且预填它的字段。
    ///
    /// 只断言「有对话框」是不够的：`task_draft` 是单例字段，若打开 `b` 时复用了
    /// 上一条 `a` 的草稿，窗口照样弹出来、也照样有输入框，但用户改的是 `a`。
    /// 这里用挂在内容根节点上的 `debug_selector`（`task-dialog-t-a/b`）分别打开
    /// 两条任务，既量到「框开出来了」，也用时间 / AI 要求两个输入框的内容钉住
    /// 「载入的是对的那条」。
    #[gpui::test]
    fn task_edit_dialog_opens_for_the_clicked_task(cx: &mut gpui::TestAppContext) {
        use gpui_component::WindowExt as _;
        let (main, vcx) = build_main(cx);

        let now = rustshell_core::connections::unix_now_secs();
        let mk = |id: &str, prompt: &str, every_secs: u64| {
            let mut t = rustshell_core::tasks::Task::new(
                id,
                rustshell_core::tasks::Trigger::Interval { every_secs },
                rustshell_core::tasks::TaskAction::AiInspect {
                    target: rustshell_core::tasks::TaskTarget {
                        profiles: vec!["p1".into()],
                        ..Default::default()
                    },
                    prompt: prompt.into(),
                    checks: Vec::new(),
                    thresholds: None,
                    log_path: None,
                    log_level: Default::default(),
                },
                now,
            );
            t.id = id.into();
            t
        };

        main.update_in(&mut vcx.clone(), |m, _w, _cx| {
            // 与 `panel_renders_one_row_per_task` 同一理由：`MainView::new` 读的是
            // 这台机器上真实的 `tasks.json`，不清空会把用户的任务一起带进来。
            m.io.task_store = rustshell_core::tasks::TaskStore::default();
            m.io.task_store.add(mk("t-a", "看看磁盘还剩多少", 3600));
            m.io.task_store.add(mk("t-b", "看看负载高不高", 7200));
        });

        for (id, sel, cron, prompt) in [
            ("t-a", "task-dialog-t-a", "0 */1 * * *", "看看磁盘还剩多少"),
            ("t-b", "task-dialog-t-b", "0 */2 * * *", "看看负载高不高"),
        ] {
            main.update_in(&mut vcx.clone(), |m, window, cx| {
                m.open_task_editor(id, window, cx);
            });
            vcx.run_until_parked();

            vcx.update(|window, cx| {
                assert!(window.has_active_dialog(cx), "任务编辑对话框没打开（{id}）");
            });
            let bounds = vcx
                .debug_bounds(sel)
                .unwrap_or_else(|| panic!("任务编辑对话框内容没有渲染出来（{id} 未开？）"));
            assert!(
                f32::from(bounds.size.height) > 0.,
                "任务编辑对话框内容高度为 0（{id}）"
            );
            main.update_in(&mut vcx.clone(), |m, _w, cx| {
                let got_cron = m.io.dlg_task_cron.read(cx).value().trim().to_string();
                assert_eq!(
                    got_cron, cron,
                    "对话框预填的是别的任务的时间：改的会是另一条（期望 {cron}，实际 {got_cron}）"
                );
                let got_prompt = m.io.dlg_task_prompt.read(cx).value().trim().to_string();
                assert_eq!(
                    got_prompt, prompt,
                    "对话框预填的是别的任务的 AI 要求（期望 {prompt}，实际 {got_prompt}）"
                );
                let got_name = m.io.dlg_task_name.read(cx).value().trim().to_string();
                assert_eq!(
                    got_name, id,
                    "名称框没有预填被点那条任务的名字（期望 {id}，实际 {got_name}）"
                );
            });

            // 关掉，为下一条让路（`present_dialog` 见有对话框在就静默 return）
            vcx.update(|window, cx| {
                window.close_dialog(cx);
            });
            main.update_in(&mut vcx.clone(), |m, _w, cx| {
                m.io.task_draft = None;
                m.dialog = None;
                cx.notify();
            });
            vcx.run_until_parked();
        }
    }

    /// OpenSSH 预览层是**叠层**：它要能开在设置面板之上（设置 → 导入导出 →
    /// 从 OpenSSH 配置导入…），所以必须直开 `w.open_dialog`、关闭时只弹栈顶。
    ///
    /// 挡的回归是「顺手统一成 `present_dialog` / `close_dialog_ui`」这对看似更规整的
    /// 组合 —— 官方 `Root` 的对话框层本来就是栈（`active_dialogs: Vec<_>`，逐层渲染、
    /// 只给最上层画遮罩、关闭只弹栈顶并回上一层的焦点），而 `present_dialog` 开头的
    /// `has_active_dialog` 守卫会**直接吞掉**这次打开：设置面板里那枚按钮变成
    /// 「点了没反应」；`close_dialog_ui` 则按单槽语义工作，会把下面的设置面板
    /// （`self.dialog == Some(Settings)`）连同未保存的快照一起回滚。
    ///
    /// 同一个坑「免确认白名单」已经踩过一次（`open_ai_whitelist_dialog` 也是直开），
    /// 所以这条断言同时护住两个入口。
    #[test]
    fn openssh_preview_stacks_instead_of_taking_the_single_dialog_slot() {
        let code = crate::test_support::prod(include_str!("dialogs.rs"));
        // `prod()` 已经切掉测试模块并抹掉注释，所以下面的正/负断言不会自我命中。
        let body = |needle: &str| -> String {
            let lines: Vec<&str> = code.lines().collect();
            let at = lines
                .iter()
                .position(|l| l.contains(needle))
                .unwrap_or_else(|| panic!("找不到 `{needle}`"));
            let end = lines[at + 1..]
                .iter()
                .position(|l| l.starts_with("    fn ") || l.starts_with("    pub"))
                .map_or(lines.len(), |i| at + 1 + i);
            lines[at..end].join("\n")
        };

        let open = body("fn present_openssh_import_dialog");
        assert!(
            open.contains("w.open_dialog("),
            "预览层必须直开 `w.open_dialog`：走 `present_dialog` 会被它的 \
             `has_active_dialog` 守卫吞掉，设置面板里那枚按钮就成了「点了没反应」。"
        );
        assert!(
            !open.contains("present_dialog(DialogMode"),
            "预览层不能用 `present_dialog`（单槽 + 有守卫），它要能叠在设置面板之上。"
        );
        assert!(
            open.contains("openssh_dialog_open = true"),
            "打开时必须置上 `openssh_dialog_open`：否则「选择文件…」换源会再叠一层预览。"
        );
        assert!(
            open.contains(".on_close("),
            "`on_close` 要覆盖 ESC / 点遮罩 / 右上角 X / 取消按钮四条关闭路径，\
             把业务标记与官方栈一起复位。"
        );

        let close = body("fn close_openssh_dialog");
        assert!(
            close.contains("w.close_dialog("),
            "关闭预览只能 `w.close_dialog`（弹栈顶），下面那层设置面板要原样留着。"
        );
        assert!(
            !close.contains("close_dialog_ui(") && !close.contains("self.dialog = None"),
            "关闭预览不能用 `close_dialog_ui`：它按单槽语义清 `self.dialog`，\
             还会在 `Some(Settings)` 时把设置面板的快照一起回滚。"
        );
    }

    /// 任务编辑对话框的正文**不再自己限高 + 滚动**（2026-09-21 修）。
    ///
    /// 外层 gpui-component 的 Dialog 已经是「`max_h(窗口可用高度)` + `flex_1 +
    /// overflow_hidden` 视口 + `size_full` 滚动体」的正规结构，正文再套一层
    /// `max_h + overflow_y_scrollbar` 只会把内容先封死（内容恰好等于视口，
    /// 滚动量为 0）—— 用户实测的「目标列表不能滚动」正是这个。
    /// 正文自身要 `flex_shrink_0`：保持自然高度，视口装不下时是**滚动**而不是
    /// 被压扁（压扁的字段会互相叠在一起）。
    #[test]
    fn task_dialog_body_leaves_scrolling_to_the_dialog() {
        let code = crate::test_support::prod(include_str!("dialogs.rs"));
        assert!(
            !code.contains(".max_h(px(520.))"),
            "正文又给自己限了高：外层 Dialog 的滚动体会因此永远不触发"
        );
        // 只在任务对话框的正文里查：本文件还有好几个对话框，它们同样有 `let body`
        let dialog_at = code
            .find("fn render_task_dialog")
            .expect("找不到任务对话框");
        let at = dialog_at
            + code[dialog_at..]
                .find("let body = v_flex()")
                .expect("找不到任务对话框正文");
        // 链上到第一个 `.child(` 为止：中间是一段说明为什么不再自己滚动的注释
        let end = code[at..]
            .find(".child(")
            .map(|i| at + i)
            .unwrap_or(code.len());
        let chain = &code[at..end];
        assert!(
            chain.contains(".flex_shrink_0()"),
            "正文缺 `flex_shrink_0`：flex 会把它压扁进对话框高度，滚动条永远不出现"
        );
    }

    /// 任务编辑对话框的字段形态（2026-09-20 运行反馈的一整批要求）。
    ///
    /// 这些要求大多只能静态钉住（它们描述的是「界面长什么样」），但每一条都对应
    /// 一个用户能直接看到的结果，所以逐条断言比「看起来改了」可靠：
    /// 触发只剩 cron、目标可选、下载/标签/巡检项撤下、AI 要求、textarea、
    /// 不再有「解析出 xx 台」、「发一次 AI 测试」按钮名正确。
    #[test]
    fn task_dialog_follows_the_2026_09_20_rework() {
        let code = crate::test_support::prod(include_str!("dialogs.rs"));

        // ① 时间：只有 cron 一种写法 —— 三种形态的单选必须消失
        for gone in ["task-tk-every", "task-tk-cron", "task-tk-manual"] {
            assert!(
                !code.contains(gone),
                "触发方式单选 `{gone}` 仍在：已收敛成 cron 表达式一种"
            );
        }
        assert!(
            code.contains("rustshell_core::i18n::t(\"时间\")"),
            "时间要直接给 cron 表达式输入（字段名就叫「时间」）"
        );

        // ② 目标可选：不能再有「手打分组名/连接 id」的输入框；
        //    排除功能整体撤下（2026-09-21 追加：用户要求取消排除）
        for gone in [
            "\"分组名或 id\"",
            "\"连接 id，逗号分隔\"",
            "\"排除的连接 id\"",
            "task-pick-exclude",
            "fn exclude_picker(",
            "dlg_task_exclude_sel",
        ] {
            assert!(
                !code.contains(gone),
                "`{gone}` 仍在：目标必须是**可选列表**且不含排除功能"
            );
        }
        for needle in ["task-pick-groups", "task-pick-profiles", "fn pick_row("] {
            assert!(code.contains(needle), "目标选择器缺了 `{needle}`");
        }
        // 长列表必须真的能滚：行不许被压扁（`flex_shrink_0`），盒子要有高度上限。
        // 切片按**字符边界**收敛：源码里有中文，按字节切会切进多字节字符里 panic。
        let slice = |from: usize| {
            let mut end = (from + 1600).min(code.len());
            while end < code.len() && !code.is_char_boundary(end) {
                end += 1;
            }
            &code[from..end]
        };
        let picker = slice(code.find("fn picker_box(").expect("找不到选择器盒子"));
        assert!(
            picker.contains(".max_h(px(PICKER_MAX_H))")
                && picker.contains(".overflow_hidden()")
                && picker.contains(".overflow_y_scrollbar()"),
            "选择器盒子必须「限高 + 视口 + 可滚动」：几十条连接时用户要能滚到底"
        );
        let row = slice(code.find("fn pick_row(").expect("找不到选择器行"));
        assert!(
            row.contains(".flex_shrink_0()"),
            "选择器行缺 `flex_shrink_0`：flex 会把行压扁进盒子高度，滚动条永远不出现"
        );

        // ③ 标签分类撤下
        assert!(
            !code.contains("task-tg-tag") && !code.contains("TargetRoom::Tags"),
            "「标签」目标方式仍在：标签分类已撤下"
        );

        // ④ 批量下载撤下
        assert!(
            !code.contains("task-tpl-dl") && !code.contains("\"远端路径\""),
            "下载任务的入口/字段仍在：批量下载已撤下新建"
        );

        // ⑤ 巡检项撤下
        assert!(
            !code.contains("task-check-") && !code.contains("\"巡检项\""),
            "「巡检项」选择仍在：已撤下（查什么由模型按 AI 要求决定）"
        );

        // ⑥ 补充要求 → AI 要求
        assert!(
            code.contains("rustshell_core::i18n::t(\"AI 要求\")"),
            "「补充要求」应更名为「AI 要求」"
        );
        assert!(!code.contains("\"补充要求\""), "旧的「补充要求」文案仍在");

        // ⑦ 多行输入：AI 要求走 Textarea（命令框已随表单简化撤下）
        assert!(
            code.contains("Textarea::new(&self.io.dlg_task_prompt)"),
            "AI 要求必须是多行 `Textarea`"
        );

        // ⑧ 「解析出 xx 台」预览撤下
        assert!(
            !code.contains("task_refresh_preview") && !code.contains("task_preview"),
            "目标解析预览仍在：该功能已取消"
        );

        // ⑨ 「发一次 AI 测试」按钮（用户要求：旧的「试运行」只做纯计算，撤下）
        assert!(
            code.contains("Button::new(\"task-dlg-ai-test\")")
                && code.contains("rustshell_core::i18n::t(\"发一次 AI 测试\")"),
            "「发一次 AI 测试」按钮的 id 与文案没对上"
        );
        assert!(
            !code.contains("task-dlg-dry-run") && !code.contains("task_run_dry_run"),
            "旧的「试运行」按钮还在：已改为「发一次 AI 测试」（真的跑一次）"
        );
        // 任务对话框 footer 的三颗按钮（AI 测试只在 AI 巡检任务上出现），
        // 且**没有**第二颗「保存」
        for id in ["task-dlg-cancel", "task-dlg-ai-test", "task-dlg-ok"] {
            assert!(code.contains(id), "footer 缺少 `{id}`");
        }
        assert!(
            !code.contains("Button::new(\"task-dlg-save\")"),
            "footer 里还留着第二颗「保存」（`task-dlg-save`）—— 那正是原来的撞名缺陷"
        );
    }

    /// 目标列表再长也必须**限高且真的能滚**（2026-09-21 追加要求）。
    ///
    /// 只有源码断言（`.max_h` 在不在）不够：「限高」与「限高 + 滚动」是两码事 ——
    /// 2026-09-21 的第一次修就只做到前一半（`max_h` 加在滚动元素自己身上，
    /// 内容与视口双双封在 196 里），用户实测「不能滚动、没有滚动条」。
    /// 这里造 12 个分组，既量盒子的真实高度（限高），也**真滚一次**
    /// （滚轮事件打在选择器中心，看内容有没有跟着位移）。
    #[gpui::test]
    fn long_target_list_is_height_capped_and_scrolls(cx: &mut gpui::TestAppContext) {
        let (main, vcx) = build_main(cx);
        let now = rustshell_core::connections::unix_now_secs();
        let mut t = rustshell_core::tasks::Task::new(
            "t-a",
            rustshell_core::tasks::Trigger::Cron {
                expr: "0 9 * * *".into(),
            },
            rustshell_core::tasks::TaskAction::AiInspect {
                target: rustshell_core::tasks::TaskTarget::default(),
                prompt: "看看磁盘".into(),
                checks: Vec::new(),
                thresholds: None,
                log_path: None,
                log_level: Default::default(),
            },
            now,
        );
        t.id = "t-a".into();
        let groups: Vec<rustshell_core::connections::ConnectionGroup> = (0..12)
            .map(|i| rustshell_core::connections::ConnectionGroup {
                id: format!("g{i}"),
                name: format!("分组 {i}"),
                parent: None,
            })
            .collect();
        main.update_in(&mut vcx.clone(), |m, window, cx| {
            m.io.task_store = rustshell_core::tasks::TaskStore::default();
            m.io.task_store.add(t);
            m.conn_store.groups = groups;
            m.open_task_editor("t-a", window, cx);
        });
        vcx.run_until_parked();

        // ① 限高：可见的那一层（`-box`）不超过上限
        let b = vcx
            .debug_bounds("task-pick-groups-box")
            .expect("分组选择器没有渲染出来");
        let h = f32::from(b.size.height);
        assert!(
            h <= PICKER_MAX_H + 2.,
            "12 条分组把选择器撑到 {h}px：没有限高，用户会在对话框里看不到底"
        );
        assert!(h > 40., "选择器只有 {h}px 高：列表塌了，一条都点不到");

        // ② 真的能滚：滚轮打在选择器中心，内部内容必须相对盒子位移。
        //    比对的是「内容相对盒子」的偏移，不是窗口绝对坐标 —— 对话框进场
        //    动画还在跑，绝对坐标会自己漂。
        let rel = |vcx: &mut gpui::VisualTestContext| {
            let content = vcx
                .debug_bounds("task-pick-groups")
                .expect("选择器内容层没有渲染出来")
                .origin
                .y;
            let boxed = vcx
                .debug_bounds("task-pick-groups-box")
                .expect("分组选择器没有渲染出来")
                .origin
                .y;
            f32::from(content - boxed)
        };
        let rel0 = rel(vcx);
        let center = b.center();
        vcx.simulate_event(gpui::ScrollWheelEvent {
            position: center,
            delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(-60.))),
            ..Default::default()
        });
        vcx.run_until_parked();
        let rel1 = rel(vcx);
        assert!(
            rel1 < rel0 - 10.,
            "滚轮打了 60px，选择器内容相对盒子只动了 {:.1}px（{rel0:.1} → {rel1:.1}）：\
             用户滚不动这份列表，等于选不到后面的连接",
            rel0 - rel1
        );
    }

    /// 目标列表短的时候盒子要贴合内容（2026-09-21）。
    ///
    /// 上限 196 不能变成固定高度：两个分组时盒子若也是 196，中间会空一大块，
    /// 用户会以为「列表加载中」。
    #[gpui::test]
    fn short_target_list_is_snug(cx: &mut gpui::TestAppContext) {
        let (main, vcx) = build_main(cx);
        let now = rustshell_core::connections::unix_now_secs();
        let mut t = rustshell_core::tasks::Task::new(
            "t-a",
            rustshell_core::tasks::Trigger::Cron {
                expr: "0 9 * * *".into(),
            },
            rustshell_core::tasks::TaskAction::AiInspect {
                target: rustshell_core::tasks::TaskTarget::default(),
                prompt: "看看磁盘".into(),
                checks: Vec::new(),
                thresholds: None,
                log_path: None,
                log_level: Default::default(),
            },
            now,
        );
        t.id = "t-a".into();
        let groups: Vec<rustshell_core::connections::ConnectionGroup> = (0..2)
            .map(|i| rustshell_core::connections::ConnectionGroup {
                id: format!("g{i}"),
                name: format!("分组 {i}"),
                parent: None,
            })
            .collect();
        main.update_in(&mut vcx.clone(), |m, window, cx| {
            m.io.task_store = rustshell_core::tasks::TaskStore::default();
            m.io.task_store.add(t);
            m.conn_store.groups = groups;
            m.open_task_editor("t-a", window, cx);
        });
        vcx.run_until_parked();

        let b = vcx
            .debug_bounds("task-pick-groups-box")
            .expect("分组选择器没有渲染出来");
        let h = f32::from(b.size.height);
        assert!(
            h < PICKER_MAX_H - 20.,
            "两条分组的盒子也有 {h}px 高：上限被当成了固定高度，中间会空一大块"
        );
        assert!(h > 20., "两条分组的盒子只有 {h}px 高：行没渲染出来");
    }

    /// 「发一次 AI 测试」必须**真的发一次**：先按表单落库，再立即执行
    /// （2026-09-21 用户要求：旧「试运行」只做纯计算，撤下）。
    ///
    /// 三件事都得钉住：按钮只在 AI 巡检任务上出现（非 AI 任务跑的不是「AI 测试」，
    /// 按钮名会骗人）、先把表单内容落库（调度器读的是它自己那份 store，不落库
    /// 跑的还是旧内容）、再 `run_now` 发出去。
    #[test]
    fn ai_test_button_commits_then_runs() {
        let code = crate::test_support::prod(include_str!("dialogs.rs"));
        let at = code
            .find("fn task_send_ai_test")
            .expect("找不到「发一次 AI 测试」的实现");
        // `prod()` 切掉了测试模块，这个函数之后就是本文件生产代码的末尾
        let body = &code[at..];
        for needle in [
            "task_collect_draft",  // ① 从表单取草稿
            "dry_run",             // ② 目标为空就不发（否则必然白跑一趟）
            "task_store_update",   // ③ 落库：调度器读的是它自己那份 store
            "sched.run_now(&id",   // ④ 真的发一次
            "window.close_dialog", // ⑤ 关掉对话框，用户才看得见 AI 面板里的会话
        ] {
            assert!(body.contains(needle), "「发一次 AI 测试」缺了 `{needle}`");
        }
        // 按钮只在 AI 巡检任务上出现：`task-dlg-ai-test` 往前一小段必须是
        // `action_kind == 3` 的分支
        let btn = code
            .find("task-dlg-ai-test")
            .expect("footer 里没有 AI 测试按钮");
        let mut from = btn.saturating_sub(400);
        while from > 0 && !code.is_char_boundary(from) {
            from -= 1;
        }
        assert!(
            code[from..btn].contains("action_kind == 3"),
            "AI 测试按钮没有按任务类型收口：非 AI 巡检的任务上点它就是骗人"
        );
    }

    /// 「发一次 AI 测试」在没选到目标时**不发**：错误留在对话框里、对话框不关。
    ///
    /// 这条守住的是「别把一条跑不出结果的任务发出去」——那会让用户干等一场
    /// 结果为空（甚至没有会话）的巡检。
    #[gpui::test]
    fn ai_test_refuses_when_nothing_is_selected(cx: &mut gpui::TestAppContext) {
        let (main, vcx) = build_main(cx);
        let now = rustshell_core::connections::unix_now_secs();
        let mut t = rustshell_core::tasks::Task::new(
            "t-a",
            rustshell_core::tasks::Trigger::Cron {
                expr: "0 9 * * *".into(),
            },
            rustshell_core::tasks::TaskAction::AiInspect {
                target: rustshell_core::tasks::TaskTarget::default(),
                prompt: "看看磁盘".into(),
                checks: Vec::new(),
                thresholds: None,
                log_path: None,
                log_level: Default::default(),
            },
            now,
        );
        t.id = "t-a".into();
        main.update_in(&mut vcx.clone(), |m, window, cx| {
            m.io.task_store = rustshell_core::tasks::TaskStore::default();
            m.io.task_store.add(t);
            // 连接库里一个分组/连接都没有：目标解析必然为空
            m.conn_store.groups.clear();
            m.conn_store.profiles.clear();
            m.open_task_editor("t-a", window, cx);
        });
        vcx.run_until_parked();

        main.update_in(&mut vcx.clone(), |m, window, cx| {
            m.task_send_ai_test(window, cx);
        });
        vcx.run_until_parked();

        main.update_in(&mut vcx.clone(), |m, _w, _cx| {
            assert!(
                m.io.task_dlg_error.is_some(),
                "一个目标都没选到，测试却发出去了：用户会等一场必然为空的结果"
            );
            assert!(
                m.dialog.is_some(),
                "测试没有发出去，对话框不该关（错误要留在对话框里给用户看）"
            );
        });
    }

    /// 任务表单（新增与编辑共用）：**五项** —— 名称 / 时间 / 目标 / AI 要求 / 通知，
    /// 且「AI 要求」的高度随内容自适应。
    ///
    /// 名称字段是 2026-09-21 追加要求「编辑/新增还是要增加个任务名称字段」加回来的：
    /// 列表行、AI 会话标题、通知里都拿名字当标识，用户得有地方改它。
    #[test]
    fn task_dialog_carries_the_five_fields() {
        let code = crate::test_support::prod(include_str!("dialogs.rs"));
        // 只在任务对话框的正文里查：本文件还有连接对话框，它同样有「名称」字段
        let at = code
            .find("fn render_task_dialog")
            .expect("找不到任务对话框");
        let end = code
            .find("fn render_settings_dialog")
            .expect("找不到设置面板");
        let form = &code[at..end];

        // ① 撤下的字段：表单里不该再有它们
        for gone in [
            "i18n::t(\"命令\")",
            "i18n::t(\"触发\")",
            "i18n::t(\"表达式\")",
        ] {
            assert!(
                !form.contains(gone),
                "任务表单里还有 `{gone}`：表单只剩 名称/时间/目标/AI 要求/通知 五项"
            );
        }
        // ② 五项都在（名称接的是 `dlg_task_name` 这个输入框，不是只画个标签）
        //
        // ★ 第一条只钉字段引用 `&self.io.dlg_task_name`，不钉 `t("名称"), &self.io.dlg_task_name`
        //   这样的「标签 + 字段」整串 —— rustfmt 按行宽把它折成了两行
        //   （`t("名称"),` / `&self.io.dlg_task_name,`），整串在源码里恒不存在 ⇒ 断言恒假。
        //   标签本身由下面四条断言单独管，合成一条反而让两处格式变动都弄红它。
        for kept in [
            "&self.io.dlg_task_name",
            "i18n::t(\"时间\")",
            "i18n::t(\"目标\")",
            "i18n::t(\"AI 要求\")",
            "i18n::t(\"通知\")",
        ] {
            assert!(form.contains(kept), "任务表单缺了 `{kept}`");
        }
        // ③ 「AI 要求」的高度**不许**在渲染处写死：自动生长
        //    （`auto_grow(2, 10)`，在 `main_view.rs` 建 state 时设）只在此处生效，
        //    再给一个固定高度会把它压死 —— 表现为「打了十几行也还是四行高」。
        let textarea = form
            .find("Textarea::new(&self.io.dlg_task_prompt)")
            .expect("任务表单里没有「AI 要求」文本域");
        let mut chain_end = (textarea + 120).min(form.len());
        while chain_end < form.len() && !form.is_char_boundary(chain_end) {
            chain_end += 1;
        }
        let chain = &form[textarea..chain_end];
        assert!(
            !chain.contains(".h(px("),
            "「AI 要求」文本域被写死了高度：会压死 `auto_grow` 的自动生长"
        );
        // ④ 拖拽把手那一套已按用户要求整体撤下（改为 `auto_grow`），一件都不留
        for gone in [
            "task-prompt-grip",
            "TaskPromptResize",
            "dlg_task_prompt_h",
            "dlg_task_prompt_drag_from",
            "TASK_PROMPT_H_",
        ] {
            assert!(
                !form.contains(gone) && !code.contains(gone),
                "拾掇「拖动改高度」留下的残件 `{gone}` 还在：已改为 auto_grow"
            );
        }
        // ⑤ 标题保留（用户明确要求）——只写「任务」：任务名回到表单里了，
        //    标题再抄一遍是同一个面板里重复两行同样的字。
        assert!(
            form.contains(".title(header)") && form.contains("i18n::t(\"任务\")"),
            "任务对话框的标题被撤了：用户要求保留标题"
        );
        // ⑥ 既有任务若不是 AI 巡检：给一句「原动作保留」的说明，
        //    否则用户会以为自己的命令 / 远端路径被这个面板删掉了。
        assert!(
            form.contains("这条任务不是 AI 巡检；保存只更新时间、目标与通知，原动作保持不变。"),
            "非 AI 巡检的既有任务缺少说明文案"
        );
    }

    /// 打开设置面板时**不**自动聚焦搜索框（2026-10-20 用户要求取消自动聚焦）；
    /// 但用户主动进入搜索框之后，轮询心跳 / 视图通知 / Root 重建 / IME 上屏 /
    /// 过滤重排都不许再把焦点抢走（此前修过的回归）。
    #[gpui::test]
    fn settings_search_keeps_focus_only_after_the_user_focuses_it(cx: &mut gpui::TestAppContext) {
        use gpui_component::WindowExt as _;
        let (main, vcx) = build_main(cx);
        main.update_in(vcx, |m, window, cx| {
            m.present_dialog(DialogMode::Settings, window, cx);
        });
        vcx.run_until_parked();
        // 测试没有平台帧循环：手动投递一次 `on_next_frame`（真实运行由平台帧驱动）
        vcx.update(|w, cx| {
            w.simulate_next_frame(cx);
        });
        vcx.run_until_parked();

        vcx.update(|w, cx| {
            assert!(
                w.has_active_dialog(cx),
                "设置面板没有打开，本用例会退化成空断言"
            );
            assert!(
                w.focused_input(cx).is_none(),
                "设置面板打开后自动聚焦了搜索框 —— 用户已要求取消自动聚焦"
            );
        });

        // 用户主动进入搜索框（真实路径是点击 / Tab；这里用 focus_next 代劳）
        vcx.update(|w, cx| {
            w.focus_next(cx);
        });
        vcx.run_until_parked();
        assert_eq!(
            vcx.update(|w, cx| w.focused_input(cx).map(|s| s.value(cx).to_string())),
            Some("".to_string()),
            "focus_next 没有落到搜索框上（第一个 tab stop）"
        );

        vcx.simulate_keystrokes("abc");
        vcx.run_until_parked();
        assert_eq!(
            vcx.update(|w, cx| w.focused_input(cx).map(|s| s.value(cx).to_string())),
            Some("abc".to_string()),
            "搜索框获得焦点后打字没生效"
        );

        // 真实运行里有 300ms 的 poll 心跳：推进时钟让它在打字期间跑起来
        for i in 0..6 {
            vcx.executor()
                .advance_clock(std::time::Duration::from_millis(300));
            vcx.run_until_parked();
            assert!(
                vcx.update(|w, cx| w.focused_input(cx).is_some()),
                "poll 心跳第 {i} 次把焦点从搜索框上抢走了"
            );
            vcx.simulate_keystrokes("d");
            vcx.run_until_parked();
        }

        // 通知落地会通知 WindowState，触发 Root 重绘（对话框构建闭包重跑）
        main.update_in(vcx, |_m, _w, cx| cx.notify());
        vcx.run_until_parked();
        assert!(
            vcx.update(|w, cx| w.focused_input(cx).is_some()),
            "MainView 通知后搜索框丢了焦点"
        );
        vcx.update(|w, cx| {
            w.push_notification(
                gpui_component::notification::Notification::info("probe"),
                cx,
            );
        });
        vcx.run_until_parked();
        assert!(
            vcx.update(|w, cx| w.focused_input(cx).is_some()),
            "Root 重建后搜索框丢了焦点"
        );

        // IME 上屏 + 过滤后的侧栏重排：焦点都得留在同一个输入框上
        vcx.simulate_input("主题");
        vcx.run_until_parked();
        let value = vcx.update(|w, cx| w.focused_input(cx).map(|s| s.value(cx).to_string()));
        assert!(
            value.as_deref().is_some_and(|v| v.contains("主题")),
            "IME 上屏后焦点丢了或文字没进搜索框：{value:?}"
        );
        vcx.simulate_keystrokes("x");
        vcx.run_until_parked();
        assert!(
            vcx.update(|w, cx| w.focused_input(cx).is_some()),
            "搜索过滤重排后焦点丢了"
        );
    }

    /// 弹框期间到达的「焦点收敛」请求（命令面板关闭 / SSH 连上 / 标签操作）
    /// 不许把焦点从对话框里拽走；请求要留到框关掉之后再兑现。
    #[gpui::test]
    fn pending_focus_waits_until_the_dialog_closes(cx: &mut gpui::TestAppContext) {
        use gpui_component::WindowExt as _;
        let (main, vcx) = build_main(cx);
        main.update_in(vcx, |m, window, cx| {
            m.present_dialog(DialogMode::Settings, window, cx);
        });
        vcx.run_until_parked();
        // 开框后焦点落在对话框外壳上；记下来，用来判断收敛请求有没有把焦点拽走。
        let dialog_focus = vcx
            .update(|w, cx| w.focused(cx))
            .expect("设置面板打开后窗口没有焦点");

        // 命令面板「设置」条目确认后就是这样：先开框，再关闭面板并置收敛旗
        main.update_in(vcx, |m, _w, cx| {
            m.tabs_state.pending_focus = true;
            cx.notify();
        });
        vcx.run_until_parked();
        vcx.update(|w, cx| {
            assert!(w.has_active_dialog(cx), "设置面板被意外关掉了");
            assert_eq!(
                w.focused(cx),
                Some(dialog_focus.clone()),
                "弹框期间仍未消费的收敛请求把焦点从对话框里抢走了"
            );
            assert!(
                main.read(cx).tabs_state.pending_focus,
                "对话框还开着就把收敛请求消费掉了：关框后没人负责收焦点"
            );
        });

        // 关框后照常兑现：没有终端可聚焦时收回视图级句柄
        vcx.update(|w, cx| w.close_dialog(cx));
        vcx.run_until_parked();
        vcx.update(|w, cx| {
            assert!(!w.has_active_dialog(cx));
            assert!(
                !main.read(cx).tabs_state.pending_focus,
                "框关掉后收敛请求仍未执行"
            );
            assert!(
                main.read(cx).panel_ui.focus.is_focused(w),
                "没有终端时，关框后的收敛没有把焦点收回视图句柄"
            );
        });
    }
}
