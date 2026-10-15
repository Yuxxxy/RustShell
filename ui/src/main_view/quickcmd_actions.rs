//! 快捷命令域：多窗口投递、增删改、分组与命令组织、目标记忆。
//!
//! 本模块为 `main_view` 的子模块，可直接访问 `MainView` 的私有字段。
//!
//! 执行路径不再做风险判定（`QuickCmd.confirm` / `assess_command` 已移除）。
//! `core/src/ai.rs` 的 `Risk` / `assess_command` 仍由 AI 面板使用，勿动。

use gpui::{Context, Focusable as _, SharedString, Window};
use gpui_component::{button::ButtonVariant, dialog::DialogButtonProps, WindowExt as _};

use super::MainView;
use crate::main_view::terminal_actions::SendOutcome;
use rustshell_core::session::SessionId;

impl MainView {
    // 目标解析

    /// 全部可参与投递的 Tab 候选（id, 标题, 是否可投递），按 `tabs` 顺序。
    ///
    /// 「可投递」判定：已连接 + 有终端视图。
    /// 状态判定复用 `TabEntry::is_live`，不另写一套。
    ///
    /// 修正：此处原多一条 `t.session.writer.is_some`，
    /// 但 `create_terminal_view`（`ui/src/terminal_view.rs`）里
    /// `let writer = session.writer.take()` 已把 `PipeWriter` move 进 `TerminalView`，
    /// `session.writer` 此后恒为 `None` → 每个活动 Tab 都被误判为不可投递，
    /// 这正是「命令点了完全没反应」的根因。真正的写入通道是
    /// `TerminalView::write_to_pty`（见 `send_lines_to_tabs`），故此处只看 `terminal`。
    ///
    /// 2026-09-21：判定从 `terminal.is_some()` 收口到 `is_live()`。断连态**刻意保留**
    /// 终端视图（画面留给人看），所以「有终端」不等于「能投递」—— 这里放过去的话，
    /// 快捷命令的目标清单会把一个已经死掉的会话列为可用，投递时被引擎静默丢弃，
    /// 用户看到的是「勾了、点了、什么都没发生」。
    pub fn quick_target_labels(&self) -> Vec<(String, String, bool)> {
        self.tabs_state
            .tabs
            .iter()
            .map(|t| {
                let ok = !t.is_connecting() && !t.is_disconnected() && t.is_live();
                (t.session.id.clone(), t.session.title.clone(), ok)
            })
            .collect()
    }

    /// 某命令在所属分组内的位置与分组长度（供右键菜单「上移/下移」置灰）
    pub fn quick_cmd_position(&self, folder_id: &str, cmd_id: &str) -> Option<(usize, usize)> {
        let f = self
            .quick_store
            .folders
            .iter()
            .find(|f| f.id == folder_id)?;
        let idx = f.commands.iter().position(|c| c.id == cmd_id)?;
        Some((idx, f.commands.len()))
    }

    /// 解析一条命令的实际投递目标：
    /// 1. 本次会话显式选择过 → 用显式选择
    /// 2. 否则命令有持久化 targets 且其中仍有可用 Tab → 用记忆（过滤不可用）
    /// 3. 否则 → 当前激活 Tab
    ///
    /// 返回 `(目标 id 列表, 是否发生降级)`：降级 = 记忆目标全部失效，
    /// 回退到当前 Tab（不是静默不发送）。
    ///
    /// 决策本身是纯函数（[`plan_targets`]），本方法只负责从 `self` 取三份输入，
    /// 以便优先级阶梯可被单测覆盖。
    fn resolve_targets(&self, cmd_id: &str) -> (Vec<String>, bool) {
        let sendable: Vec<String> = self
            .quick_target_labels()
            .into_iter()
            .filter(|(_, _, ok)| *ok)
            .map(|(id, _, _)| id)
            .collect();
        let remembered: Vec<String> = self
            .quick_store
            .find_cmd(cmd_id)
            .map(|c| c.targets.clone())
            .unwrap_or_default();
        let active: Vec<String> = self.tabs_state.active.clone().into_iter().collect();

        plan_targets(&self.ai_ui.quick_targets, &remembered, &sendable, &active)
    }

    // 执行（多目标投递）

    /// 执行快捷命令：解析目标 → 多目标投递 → 汇总反馈。
    ///
    /// v3 起不再有风险确认拦截，点击即发。
    ///
    /// 调整：用户要求去掉「已发送到 xxx」的成功提示，命令直达终端、效果本身
    /// 就是反馈，再弹条提示属于噪音。仅保留失败 / 无目标这类必须让用户知道的情况。
    pub fn execute_quick_cmd(&mut self, cmd_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(cmd) = self.quick_store.find_cmd(cmd_id).cloned() else {
            // 命令已被并发删除
            log::warn!("quick-cmd: 命令 {cmd_id} 已不存在");
            self.set_quick_toast(
                crate::notice::NoticeKind::Warning,
                rustshell_core::i18n::t("命令已不存在，列表已刷新"),
                window,
                cx,
            );
            return;
        };

        let (targets, _degraded) = self.resolve_targets(cmd_id);
        if targets.is_empty() {
            // 无激活会话：绝不无声失败。2026-09-17 运行反馈「提示要显示在主窗口中间」，
            // 2026-09-18 再改一条「不需要确认」⇒ 走唯一入口 `show_no_session_alert`
            // （居中的弹出消息，无按钮、会自动消失，见该函数）。
            self.show_no_session_alert(cx);
            return;
        }

        self.ai_ui.quick_no_session_alert = false;
        self.ai_ui.quick_last_cmd = Some(cmd_id.to_string());
        // 末尾是否回车由每条命令自己配置（`append_newline`）
        let outcome = self.send_lines_to_tabs(&targets, &cmd.script, cmd.append_newline, cx);

        // 审计：记录命令 id 到 目标会话 id 列表
        log::info!(
            "quick-cmd audit: cmd={} targets={:?} sent={} skipped={} failed={}",
            cmd_id,
            targets,
            outcome.sent.len(),
            outcome.skipped.len(),
            outcome.failed.len()
        );

        // 成功不提示（用户 2026-09-14 要求）；只有「一个都没发出去」才说明原因
        if outcome.sent.is_empty() {
            let detail = outcome
                .skipped
                .first()
                .or_else(|| outcome.failed.first())
                .map(|(title, why)| format!("{title}: {why}"))
                .unwrap_or_else(|| rustshell_core::i18n::t("目标没有可投递的窗口").to_string());
            let msg = format!("{} · {detail}", rustshell_core::i18n::t("未执行"));
            self.set_quick_toast(crate::notice::NoticeKind::Error, &msg, window, cx);
        }
    }

    /// 「发送到…」（右键菜单第 2 项）：把命令的目标设为本次会话的显式选择并立即执行。
    ///
    /// 与栏上选择器的区别：这条路径直接执行，不再要求用户二次点命令，
    /// 语义是「这条命令发往哪几个窗口（现在）」。选择结果记忆到该命令。
    pub fn open_quick_send_to(
        &mut self,
        cmd_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 把命令当前生效的目标载入显式选择位，用户可在菜单里增删后再执行
        let (mut cur, _) = self.resolve_targets(cmd_id);
        if cur.is_empty() {
            cur = self.tabs_state.active.clone().into_iter().collect();
        }
        if cur != self.ai_ui.quick_targets {
            self.ai_ui.quick_targets = cur;
            cx.notify();
        }
        // 该路径的服务语义：直接执行一次（菜单里的勾选会即时改变 targets 并重绘）
        self.execute_quick_cmd(cmd_id, window, cx);
    }

    /// 切换某个 Tab 的勾选
    pub fn toggle_quick_target(&mut self, session_id: &str, cx: &mut Context<Self>) {
        if self.ai_ui.quick_targets.iter().any(|t| t == session_id) {
            self.ai_ui.quick_targets.retain(|t| t != session_id);
        } else {
            self.ai_ui.quick_targets.push(session_id.to_string());
        }
        // 勾选即刻写回命令记忆：目标是「命令的属性」，
        // 不是全局开关，不同命令的自然目标本就不同（如「重启服务」发全部、
        // 「看日志」发单个），所以绑到命令上而不是全局。
        self.sync_target_memory();
        cx.notify();
    }

    /// 「全选已连接」：只勾选可投递 Tab
    pub fn select_all_quick_targets(&mut self, cx: &mut Context<Self>) {
        self.ai_ui.quick_targets = self
            .quick_target_labels()
            .into_iter()
            .filter(|(_, _, ok)| *ok)
            .map(|(id, _, _)| id)
            .collect();
        self.sync_target_memory();
        cx.notify();
    }

    /// 「仅当前 Tab」：清空勾选，回到默认
    pub fn clear_quick_targets(&mut self, cx: &mut Context<Self>) {
        if self.ai_ui.quick_targets.is_empty() {
            return;
        }
        self.ai_ui.quick_targets.clear();
        // 清空 = 命令回到「发当前 Tab」，同样要写回记忆
        self.sync_target_memory();
        cx.notify();
    }

    /// 目标记忆同步：把当前勾选写回最近一次操作过的命令。
    ///
    /// 只写回一条命令：用户勾选的语义是「我接下来要发的这条命令的目标」，
    /// 批量改所有命令的记忆会造成意外广播。
    pub fn sync_target_memory(&mut self) {
        let Some(cmd_id) = self.ai_ui.quick_last_cmd.clone() else {
            return;
        };
        let want = self.ai_ui.quick_targets.clone();
        if self.quick_store.set_cmd_targets(&cmd_id, want) {
            let _ = self.quick_store.save(&self.ai_ui.quick_path);
        }
    }

    // 栏形态

    pub fn set_quick_bar_expanded(
        &mut self,
        expanded: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.settings.quick_bar_expanded == expanded {
            return;
        }
        self.settings.quick_bar_expanded = expanded;
        // 收起时清空搜索，避免下次展开残留过滤条件
        if !expanded {
            self.ai_ui
                .quick_search
                .update(cx, |s, cx| s.set_value("", window, cx));
        }
        let _ = self
            .settings
            .save(&rustshell_platform::paths::settings_path());
        cx.notify();
    }

    // 分组 Tab 页（2026-09-14：命令组由纵向列表改为 Tab 页）

    /// 当前选中的分组 id（渲染期纠偏：未选过 / 选中的已被删除 → 回第一个分组）。
    ///
    /// 这是纯读，不改 `self`；真正的纠偏由 `set_quick_active_folder` 完成，
    /// 避免在渲染里产生副作用。
    pub fn quick_active_folder_resolved(&self) -> Option<String> {
        let folders = &self.quick_store.folders;
        match &self.ai_ui.quick_active_folder {
            Some(id) if folders.iter().any(|f| &f.id == id) => Some(id.clone()),
            _ => folders.first().map(|f| f.id.clone()),
        }
    }

    /// 切换当前分组 Tab
    pub fn set_quick_active_folder(&mut self, folder_id: &str, cx: &mut Context<Self>) {
        if self.ai_ui.quick_active_folder.as_deref() == Some(folder_id) {
            return;
        }
        self.ai_ui.quick_active_folder = Some(folder_id.to_string());
        cx.notify();
    }

    // 命令输入框（2026-09-15：常驻形态，不再有「编辑态」开关）

    /// 把命令输入框的草稿发送到全部终端（所有可投递 Tab）。
    ///
    /// 与「发送到当前」的区别：这是用户显式要求广播，不走目标记忆。
    pub fn send_compose_to_all_tabs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let script = self.ai_ui.quick_compose_input.read(cx).value().to_string();
        if script.trim().is_empty() {
            return;
        }
        let ids = self.all_sendable_ids();
        if ids.is_empty() {
            // 同上：提示落在主窗口正中，且不需要确认
            self.show_no_session_alert(cx);
            return;
        }
        self.ai_ui.quick_no_session_alert = false;
        let outcome = self.send_lines_to_tabs(&ids, &script, true, cx);
        self.after_broadcast(&script, &outcome, window, cx);
    }

    /// 全部当前终端的可投递 Tab id（= 所有已连接会话）。
    ///
    /// 语义上「全部终端」与「全部 Tab」在实现上一致（可投递 Tab 必然有终端视图），
    /// 保留两个入口是为了让 UI 文案与用户心智对应。
    pub fn all_sendable_ids(&self) -> Vec<SessionId> {
        self.quick_target_labels()
            .into_iter()
            .filter(|(_, _, ok)| *ok)
            .map(|(id, _, _)| id)
            .collect()
    }

    /// 广播后的收尾：清空草稿、聚焦复位、必要提示（成功静默）。
    fn after_broadcast(
        &mut self,
        script: &str,
        outcome: &SendOutcome,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        log::info!(
            "quick-cmd broadcast: len={} sent={} skipped={} failed={}",
            script.lines().count(),
            outcome.sent.len(),
            outcome.skipped.len(),
            outcome.failed.len()
        );
        if outcome.sent.is_empty() {
            let detail = outcome
                .skipped
                .first()
                .or_else(|| outcome.failed.first())
                .map(|(title, why)| format!("{title}: {why}"))
                .unwrap_or_else(|| rustshell_core::i18n::t("目标没有可投递的窗口").to_string());
            let msg = format!("{} · {detail}", rustshell_core::i18n::t("未执行"));
            self.set_quick_toast(crate::notice::NoticeKind::Error, &msg, window, cx);
            return;
        }
        // 发送成功：清空草稿（输入框常驻，无形态切换；不弹「已发送到...」）
        self.ai_ui
            .quick_compose_input
            .update(cx, |s, cx| s.set_value("", window, cx));
        cx.notify();
    }

    // 分组动作

    /// 新建分组：创建后立即进入行内重命名态
    pub fn add_quick_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let prefix = rustshell_core::i18n::t("新分组");
        let id = self.quick_store.add_folder(prefix);
        let _ = self.quick_store.save(&self.ai_ui.quick_path);
        self.begin_folder_rename(&id, prefix, window, cx);
    }

    /// 进入分组的行内重命名态
    pub fn begin_folder_rename(
        &mut self,
        folder_id: &str,
        current: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.ai_ui.quick_renaming = Some(folder_id.to_string());
        self.ai_ui
            .quick_rename_input
            .update(cx, |s, cx| s.set_value(current, window, cx));
        let fh = self.ai_ui.quick_rename_input.read(cx).focus_handle(cx);
        window.focus(&fh, cx);
        cx.notify();
    }

    /// 提交行内重命名（Enter / 失焦）：空名回退原名。
    ///
    /// 修复：原实现直接把 `trim` 后的空串交给 `rename_folder`，而 core 只在
    /// 「新旧同名」时返回 false，空串会被真的写进去 → 分组变成一个没有名字的 Tab。
    /// 这里显式拦截空名：视为取消，保留原名。
    pub fn commit_folder_rename(&mut self, cx: &mut Context<Self>) {
        let Some(fid) = self.ai_ui.quick_renaming.take() else {
            return;
        };
        let name = self
            .ai_ui
            .quick_rename_input
            .read(cx)
            .value()
            .trim()
            .to_string();
        if name.is_empty() {
            // 空名 = 取消（保留原名），不写盘、不改数据
            cx.notify();
            return;
        }
        if self.quick_store.rename_folder(&fid, &name) {
            let _ = self.quick_store.save(&self.ai_ui.quick_path);
        }
        cx.notify();
    }

    /// 取消行内重命名（Esc）
    pub fn cancel_folder_rename(&mut self, cx: &mut Context<Self>) {
        if self.ai_ui.quick_renaming.take().is_some() {
            cx.notify();
        }
    }

    /// 删除分组的二次确认：走官方 `AlertDialog` 浮动弹层，不占命令栏空间。
    ///
    /// 历史两版都有缺陷：最早只 `set_quick_toast(..)` —— toast 没有按钮且 2s 后
    /// 自动消失，现象就是「右键删除分组无效」；后来改成在命令栏里常驻一条确认条 ——
    /// 能删了，但把命令列表整体顶下去。现在统一成弹层，并与其它不可逆动作同一形态：
    /// OK 按钮用 `Danger` 变体，取消 / ESC / 右上角 X 都视为放弃。
    pub fn confirm_remove_quick_folder(
        &mut self,
        folder_id: &str,
        name: &str,
        count: usize,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 同一个 Root 再叠一层确认框对用户只是噪音；已有对话框就忽略本次请求。
        if w.has_active_dialog(cx) {
            return;
        }
        self.ai_ui.quick_pending_remove = Some(folder_id.to_string());

        let title: SharedString = rustshell_core::i18n::t("删除分组").into();
        let desc: SharedString = rustshell_core::i18n::ts(
            format!("删除分组「{name}」？该分组下的 {count} 条命令会一并删除。"),
            format!("Delete group \"{name}\"? Its {count} command(s) will be removed too."),
        )
        .into();
        let ok_text: SharedString = rustshell_core::i18n::t("删除").into();
        let cancel_text: SharedString = rustshell_core::i18n::t("取消").into();
        let fid = folder_id.to_string();

        w.open_alert_dialog(cx, move |alert, _w, _cx| {
            alert
                .title(title.clone())
                .description(desc.clone())
                .confirm()
                // 顺序要紧：`on_ok` / `on_close` 都是在**已有** `button_props` 上改，
                // 排在 `.button_props(..)` 之前会被它整体覆盖（按钮文字与回调一起丢）。
                .button_props(
                    DialogButtonProps::default()
                        .ok_text(ok_text.clone())
                        .ok_variant(ButtonVariant::Danger)
                        .cancel_text(cancel_text.clone())
                        .show_cancel(true),
                )
                .on_ok({
                    let fid = fid.clone();
                    move |_, w, cx| {
                        crate::main_view::ui_call(&fid, MainView::remove_quick_folder_by_id, w, cx);
                        true
                    }
                })
                .on_close(|_, _w, cx| crate::main_view::ui_call_clear_quick_pending(cx))
        });
        cx.notify();
    }

    /// 真正删除分组（`AlertDialog` 的「删除」按钮）。
    ///
    /// 无论删除是否命中都要清掉待删记录：否则用户再点一次删除会被
    /// `confirm_remove_quick_folder` 的首部守卫挡掉，看起来像菜单坏了。
    pub fn remove_quick_folder_by_id(
        &mut self,
        folder_id: &str,
        _w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.quick_store.remove_folder(folder_id) {
            let _ = self.quick_store.save(&self.ai_ui.quick_path);
        }
        self.ai_ui.quick_pending_remove = None;
        cx.notify();
    }

    /// 复制分组（含命令）
    pub fn duplicate_quick_folder(&mut self, folder_id: &str, cx: &mut Context<Self>) {
        let suffix = rustshell_core::i18n::ts(" 副本".to_string(), " copy".to_string());
        if self
            .quick_store
            .duplicate_folder(folder_id, &suffix)
            .is_some()
        {
            let _ = self.quick_store.save(&self.ai_ui.quick_path);
        }
        cx.notify();
    }

    // 注：分组「展开/收起」的 UI 入口已于 随「命令组改 Tab 页」移除
    // （Tab 页形态下每个分组天然只显示当前一个，展开/收起语义不再存在），
    // 故`toggle_quick_folder`/`set_all_quick_folders`一并删除。
    //`CommandFolder.collapsed`字段与 core 的`set_folder_collapsed`/`set_all_collapsed`
    // 保留：它们仍被 core 单测覆盖且承担旧文件兼容（删字段要再动 schema）。

    /// 分组上移 / 下移（右键菜单，delta = -1/+1）
    pub fn move_quick_folder_step(&mut self, folder_id: &str, delta: i32, cx: &mut Context<Self>) {
        let Some(idx) = self
            .quick_store
            .folders
            .iter()
            .position(|f| f.id == folder_id)
        else {
            return;
        };
        let target = idx as i64 + delta as i64;
        if target < 0 || target >= self.quick_store.folders.len() as i64 {
            return;
        }
        let to_id = self.quick_store.folders[target as usize].id.clone();
        if self.quick_store.move_folder(folder_id, &to_id) {
            let _ = self.quick_store.save(&self.ai_ui.quick_path);
            cx.notify();
        }
    }

    /// 分组拖拽重排：from 分组移到 to 分组的位置并持久化
    pub fn reorder_quick_folder(&mut self, from_id: &str, to_id: &str, cx: &mut Context<Self>) {
        if self.quick_store.move_folder(from_id, to_id) {
            let _ = self.quick_store.save(&self.ai_ui.quick_path);
            cx.notify();
        }
    }

    // 命令动作

    /// 新建命令：落到当前展开的分组，否则首个分组；无分组时先建一个
    pub fn quick_new_command_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let fid = self
            .quick_store
            .folders
            .iter()
            .find(|f| !f.collapsed)
            .or_else(|| self.quick_store.folders.first())
            .map(|f| f.id.clone());
        match fid {
            Some(fid) => self.open_qc_new(fid, window, cx),
            None => {
                let id = self
                    .quick_store
                    .add_folder(rustshell_core::i18n::t("新分组"));
                let _ = self.quick_store.save(&self.ai_ui.quick_path);
                self.open_qc_new(id, window, cx);
            }
        }
    }

    /// 复制命令
    pub fn duplicate_quick_cmd(&mut self, cmd_id: &str, cx: &mut Context<Self>) {
        let suffix = rustshell_core::i18n::ts(" 副本".to_string(), " copy".to_string());
        if self.quick_store.duplicate_cmd(cmd_id, &suffix).is_some() {
            let _ = self.quick_store.save(&self.ai_ui.quick_path);
        }
        cx.notify();
    }

    /// 删除命令
    pub fn delete_quick_cmd(&mut self, cmd_id: &str, cx: &mut Context<Self>) {
        if self.quick_store.remove_cmd(cmd_id) {
            let _ = self.quick_store.save(&self.ai_ui.quick_path);
        }
        cx.notify();
    }

    /// 命令跨组移动
    pub fn move_quick_cmd_to(&mut self, cmd_id: &str, to_folder_id: &str, cx: &mut Context<Self>) {
        if self.quick_store.move_cmd_to(cmd_id, to_folder_id) {
            let _ = self.quick_store.save(&self.ai_ui.quick_path);
            cx.notify();
        }
    }

    /// 分组内命令上移/下移（delta = -1/+1）并持久化
    pub fn move_quick_cmd(
        &mut self,
        folder_id: &str,
        cmd_id: &str,
        delta: i32,
        cx: &mut Context<Self>,
    ) {
        if self.quick_store.move_command(folder_id, cmd_id, delta) {
            let _ = self.quick_store.save(&self.ai_ui.quick_path);
            cx.notify();
        }
    }

    /// 打开「设置 到 快捷命令」页
    pub fn open_quickcmd_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_settings(window, cx);
    }

    // 编辑器（保留原行为，删掉 confirm）

    pub fn open_qc_new(&mut self, folder_id: String, window: &mut Window, cx: &mut Context<Self>) {
        self.io
            .dlg_qc_label
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.io
            .dlg_qc_script
            .update(cx, |s, cx| s.set_value("", window, cx));
        // 新建默认「末尾回车」= 写完即执行
        self.io.dlg_qc_newline = true;
        self.present_dialog(DialogMode::QcNew(folder_id), window, cx);
    }

    pub fn open_qc_edit(
        &mut self,
        folder_id: String,
        cmd_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(c) = self.quick_store.find_cmd(&cmd_id).cloned() else {
            return;
        };
        self.io
            .dlg_qc_label
            .update(cx, |s, cx| s.set_value(&c.label, window, cx));
        self.io
            .dlg_qc_script
            .update(cx, |s, cx| s.set_value(&c.script, window, cx));
        // 回填「末尾回车」勾选态
        self.io.dlg_qc_newline = c.append_newline;
        self.present_dialog(DialogMode::QcEdit(folder_id, cmd_id), window, cx);
    }

    // 瞬时提示

    /// 弹出「没有可投递会话」提示 —— **唯一入口**（居中、无按钮、到点自清）。
    ///
    /// 两条发送路径（发单条命令 / 广播草稿）都只调这一个函数：形态与寿命的口径
    /// 只有一处，改版时不会漏掉某条路径。此前两处各自 `quick_no_session_alert = true`
    /// + `cx.notify()`，想加「自动消失」就得在两处各贴一遍定时器。
    ///
    /// 记录代次（token）而不是直接 `cx.spawn` 一个定时器：连点时只有最后一次的
    /// 定时器生效，先到的不清后到的。
    pub fn show_no_session_alert(&mut self, cx: &mut Context<Self>) {
        self.ai_ui.quick_no_session_alert = true;
        let token = self.no_session_notice_seq.wrapping_add(1);
        self.no_session_notice_seq = token;
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(
                    super::NO_SESSION_NOTICE_MS,
                ))
                .await;
            let _ = this.update(cx, |m, cx| {
                if m.no_session_notice_seq == token {
                    m.ai_ui.quick_no_session_alert = false;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// 设置快捷命令域瞬时提示并安排 2s 后自动清除。
    ///
    /// `kind` 决定提示条的语义级别（警告 / 错误），渲染时经官方`Alert`取色 到
    /// 调用点必须显式选，不允许「都当警告」：投递失败是错误，不是提醒。
    pub fn set_quick_toast(
        &mut self,
        kind: crate::notice::NoticeKind,
        msg: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.ai_ui.quick_toast = Some(msg.to_string());
        self.ai_ui.quick_toast_kind = kind;
        let token = self.ai_ui.quick_toast_seq.wrapping_add(1);
        self.ai_ui.quick_toast_seq = token;
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(2000))
                .await;
            let _ = this.update(cx, |m, cx| {
                // 只有仍是同一次提示才清（连点时后到的提示不会被先到的清掉）
                if m.ai_ui.quick_toast_seq == token {
                    m.ai_ui.quick_toast = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }
}

//`DialogMode`与`EditorState`由本模块的编辑器动作使用
use super::DialogMode;

// 投递目标决策（纯函数，供 resolve_targets 调用 + 单测覆盖）

/// 三级优先级的目标决策。
///
/// 输入：
/// - `explicit`：本次会话显式勾选的目标
/// - `remembered`：命令持久化的 `targets`（上次发过的窗口）
/// - `sendable`：当前可投递的 Tab id 集合
/// - `active`：当前激活 Tab（可能为空 = 无会话）
///
/// 输出 `(目标列表, 是否降级)`：
/// 1. `explicit` 中有可用项 → 用它
/// 2. `remembered` 中有可用项 → 用它
/// 3. `remembered` 非空但全部失效 → 降级到 `active`，标记 `true`
/// 4. 否则 → `active`
///
/// 关键：第 3 步绝不返回空列表，记忆失效时宁可改发当前窗口并提示，
/// 也不静默不发送。
fn plan_targets(
    explicit: &[String],
    remembered: &[String],
    sendable: &[String],
    active: &[String],
) -> (Vec<String>, bool) {
    let keep_usable = |ids: &[String]| -> Vec<String> {
        ids.iter()
            .filter(|id| sendable.contains(id))
            .cloned()
            .collect()
    };

    // 1. 显式选择
    if !explicit.is_empty() {
        let hit = keep_usable(explicit);
        if !hit.is_empty() {
            return (hit, false);
        }
        // 显式选择全部失效 到 继续往下尝试记忆 / 当前 Tab
    }

    // 2. 记忆
    if !remembered.is_empty() {
        let hit = keep_usable(remembered);
        if !hit.is_empty() {
            return (hit, false);
        }
        // 3. 记忆全部失效 到 降级到当前 Tab
        return (active.to_vec(), true);
    }

    // 4. 当前激活 Tab
    (active.to_vec(), false)
}

#[cfg(test)]
mod tests {
    use super::plan_targets;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    /// 无显式选择、无记忆 到 用当前 Tab，不降级
    #[test]
    fn falls_back_to_active_tab() {
        let (got, degraded) = plan_targets(&[], &[], &s(&["a", "b"]), &s(&["a"]));
        assert_eq!(got, s(&["a"]));
        assert!(!degraded);
    }

    /// 显式选择优先于记忆。
    #[test]
    fn explicit_beats_remembered() {
        let (got, degraded) = plan_targets(&s(&["b"]), &s(&["a"]), &s(&["a", "b"]), &s(&["a"]));
        assert_eq!(got, s(&["b"]));
        assert!(!degraded);
    }

    /// 显式选择里混有已失效项 到 只保留可用项，不算降级
    #[test]
    fn explicit_filters_dead_targets() {
        let (got, degraded) = plan_targets(&s(&["dead", "b"]), &[], &s(&["a", "b"]), &s(&["a"]));
        assert_eq!(got, s(&["b"]));
        assert!(!degraded);
    }

    /// 显式选择全失效 → 退到记忆（记忆可用）→ 用记忆且不标记降级
    #[test]
    fn dead_explicit_falls_through_to_remembered() {
        let (got, degraded) = plan_targets(&s(&["dead"]), &s(&["b"]), &s(&["a", "b"]), &s(&["a"]));
        assert_eq!(got, s(&["b"]));
        assert!(!degraded);
    }

    /// 记忆生效：过滤掉已不可用的目标，只发仍可用的那些
    #[test]
    fn remembered_filters_unavailable() {
        let (got, degraded) = plan_targets(&[], &s(&["a", "dead"]), &s(&["a", "b"]), &s(&["a"]));
        assert_eq!(got, s(&["a"]));
        assert!(!degraded);
    }

    /// 核心边界：记忆全部失效 到 降级到当前 Tab 并标记降级，绝不返回空
    #[test]
    fn all_remembered_dead_degrades_to_active_with_flag() {
        let (got, degraded) = plan_targets(&[], &s(&["x", "y"]), &s(&["a"]), &s(&["a"]));
        assert_eq!(got, s(&["a"]), "记忆全失效时必须降级到当前 Tab，不能为空");
        assert!(degraded, "必须标记降级，以便提示用户改发了当前窗口");
    }

    /// 记忆全失效且当前无会话 到 空列表（由调用方提示「当前没有已连接的会话」）
    #[test]
    fn all_dead_and_no_active_yields_empty() {
        let (got, degraded) = plan_targets(&[], &s(&["x"]), &s(&["a"]), &[]);
        assert!(got.is_empty());
        assert!(degraded);
    }

    /// 多目标：显式选择多个可用项 到 全部保留且保持顺序
    #[test]
    fn keeps_multiple_targets_in_order() {
        let (got, degraded) =
            plan_targets(&s(&["b", "a", "c"]), &[], &s(&["a", "b", "c"]), &s(&["a"]));
        assert_eq!(got, s(&["b", "a", "c"]));
        assert!(!degraded);
    }
}
