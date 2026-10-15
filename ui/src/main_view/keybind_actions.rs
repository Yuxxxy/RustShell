//! 命令入口域：键位管理（录制/重绑定/恢复默认） + 命令面板（Command 双源消费、AI 直通条目）。 本模块为`main_view`的子模块，可直接访问`MainView`的私有字段。
//!
//! 2026-09-18 运行反馈「移除设置-快捷键-导出导入快捷键」：`export_keymap` /
//! `import_keymap` 两个方法随面板上的两枚按钮一并撤下。键位仍以
//! `<config>/keymap.json` 的用户覆盖形式持久化（`finish_key_record` 写入），
//! 只是不再提供「导出成 keymap-export.json / 再导回来」这条应用内通路 ——
//! 要迁移键位直接拷 `keymap.json` 即可，导出文件本就是它的副本。
//! `platform::keymap::{export_bindings, import_bindings}` 保留（有自身单测），
//! 只是应用内不再有调用点。

use gpui::{
    div, px, App, AppContext as _, Context, Focusable as _, InteractiveElement as _,
    IntoElement as _, MouseButton, ParentElement as _, Window,
};

use gpui_component::command::{Command, CommandGroup, CommandItem, CommandState};
use gpui_component::h_flex;

use rustshell_platform::paths;

use super::{
    ui_call, AiAssistant, CommandPalette, MainView, NewConnection, NewTab, OpenSettings,
    RestoreLastSession, ToggleSidebar, UiHub,
};

impl MainView {
    // 键位管理

    /// 结束录制态（丢弃拦截器与录制标记）
    pub(super) fn cancel_key_record(&mut self) {
        self.panel_ui.keymap_capture = None;
        self.panel_ui.keymap_recording = None;
    }

    /// 进入录制态：注册按键拦截器（在 action 绑定分发之前接管一切按键）
    pub(super) fn start_key_record(&mut self, action_id: &str, cx: &mut Context<Self>) {
        self.cancel_key_record();
        self.panel_ui.keymap_hint = None;
        self.panel_ui.keymap_recording = Some(action_id.to_string());
        self.panel_ui.keymap_capture = Some(cx.intercept_keystrokes(move |ev, _w, cx| {
            // 录制期吞掉所有按键：换算成 spec 后交回 MainView 决策
            cx.stop_propagation();
            let Some(spec) = crate::keybinds::keystroke_to_spec(&ev.keystroke) else {
                return; // 裸修饰键：继续等待组合
            };
            if let Some(hub) = cx.try_global::<UiHub>() {
                let main = hub.main.clone();
                let _ = main.update(cx, |m, cx| m.finish_key_record(spec, cx));
            }
        }));
        cx.notify();
    }

    /// 录制结束：Esc 取消；合法组合做冲突检测 到 持久化 到 运行时立即生效
    pub(super) fn finish_key_record(&mut self, spec: String, cx: &mut Context<Self>) {
        use rustshell_platform::keymap as km;
        self.panel_ui.keymap_capture = None; // 先停拦截，避免后续按键仍被吞
        let Some(action_id) = self.panel_ui.keymap_recording.take() else {
            return;
        };

        if spec == "escape" {
            cx.notify();
            return;
        }
        if !crate::keybinds::has_action_modifier(&spec) {
            self.panel_ui.keymap_hint = Some(rustshell_core::i18n::ts(
                format!(
                    "快捷键需包含 {} 或 Alt 修饰键",
                    rustshell_platform::keymap::mod_hint()
                ),
                format!(
                    "Shortcut must include the {} or Alt modifier",
                    rustshell_platform::keymap::mod_hint()
                ),
            ));
            cx.notify();
            return;
        }
        let new_resolved = km::resolve(&spec);
        if let Some(other) = self
            .panel_ui
            .keymap_rows
            .iter()
            .find(|r| r.action_id != action_id && km::resolve(&r.spec) == new_resolved)
        {
            self.panel_ui.keymap_hint = Some(rustshell_core::i18n::ts(
                format!("与「{}」的键位冲突，请换一个组合", other.label),
                format!("Conflicts with \"{}\"; choose another combo", other.label),
            ));
            cx.notify();
            return;
        }

        let old_resolved = self
            .panel_ui
            .keymap_rows
            .iter()
            .find(|r| r.action_id == action_id)
            .map(|r| km::resolve(&r.spec));

        let km_path = paths::keymap_path();
        let mut overrides = km::load_overrides(&km_path);
        overrides.insert(action_id.clone(), spec);
        if let Err(e) = km::save_overrides(&km_path, &overrides) {
            self.panel_ui.keymap_hint = Some(rustshell_core::i18n::ts(
                format!("保存失败：{e}"),
                format!("Save failed: {e}"),
            ));
            cx.notify();
            return;
        }
        // 运行时立即生效（旧键位解除、新键位注册）
        if old_resolved.as_deref() != Some(new_resolved.as_str()) {
            crate::keybinds::apply_override_live(cx, &action_id, &new_resolved);
        }
        self.panel_ui.keymap_rows = crate::keybinds::snapshot_rows();
        cx.notify();
    }

    /// 清除某命令的用户覆盖，恢复默认键位
    pub(super) fn reset_key_binding(&mut self, action_id: &str, cx: &mut Context<Self>) {
        use rustshell_platform::keymap as km;
        let km_path = paths::keymap_path();
        let mut overrides = km::load_overrides(&km_path);
        if overrides.remove(action_id).is_none() {
            return;
        }
        if let Err(e) = km::save_overrides(&km_path, &overrides) {
            self.panel_ui.keymap_hint = Some(rustshell_core::i18n::ts(
                format!("保存失败：{e}"),
                format!("Save failed: {e}"),
            ));
            cx.notify();
            return;
        }
        if let Some((_, default_spec)) =
            km::DEFAULT_BINDINGS.iter().find(|(id, _)| *id == action_id)
        {
            crate::keybinds::apply_override_live(cx, action_id, &km::resolve(default_spec));
        }
        self.panel_ui.keymap_rows = crate::keybinds::snapshot_rows();
        self.panel_ui.keymap_hint = None;
        cx.notify();
    }

    // 命令面板

    /// 请求**下一帧**打开命令窗口（启动路径用；那里拿不到 `Window`）。
    ///
    /// 2026-09-17 运行反馈「打开软件默认不打开本地 shell，打开命令窗口」：
    /// 真正的打开动作在 `MainView::render` 里消费这个标记（要 `window.focus`）。
    pub fn request_command_palette(&mut self, cx: &mut Context<Self>) {
        self.panel_ui.pending_palette = true;
        cx.notify();
    }

    pub fn open_command_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // 两段快照：内置命令 + 快速连接（按最近访问倒序）。
        //
        // 快捷命令**不在这里**：命令窗口只做「跑内置动作」与「连一台机器」两件事，
        // 快捷命令有它自己的入口（底部快捷命令栏）。
        self.panel_ui.palette_cmds = rustshell_core::command::builtin_commands();
        self.panel_ui
            .palette_cmds
            .extend(rustshell_core::command::quick_connect(&self.conn_store));
        let state = cx.new(|cx| CommandState::new(window, cx));
        let fh = state.read(cx).focus_handle(cx);
        window.focus(&fh, cx);
        self.panel_ui.palette_state = Some(state);
        cx.notify();
    }
    /// 命令面板查询回调：输入`ai <描述>`时注入直通条目。
    ///
    /// 条目的搜索词就是完整查询词：过滤规则是「标签或任一搜索词包含查询串」，
    /// 而查询串（`ai 帮我看看磁盘`）**不是**标签（`问 AI：帮我看看磁盘`）的
    /// 子串，不补这一条就会「注入完立刻被过滤掉」，用户永远看不到这个条目。
    pub fn palette_on_query(&mut self, query: &str, _w: &mut Window, cx: &mut Context<Self>) {
        use rustshell_core::command::{CommandEntry, CommandOrigin};
        let q = query.trim_start();
        // 前缀判定**两条路径必须同一口径**。原先写的是
        // `q.starts_with("ai ") || q.eq_ignore_ascii_case("ai")`：前者大小写敏感、
        // 后者不敏感 ⇒ 输 `AI 帮我看看磁盘` 不命中、输 `ai` 命中，
        // 同一个功能两套标准（2026-10-12 覆盖度盘点发现，此前零测试）。
        // 现在统一成「大小写不敏感的前缀 + 至多一个分隔空白」。
        let (is_ai, rest) = Self::parse_ai_query(q, self.panel_ui.palette_state.is_some());
        // 无论是否命中都先移除旧条目
        self.panel_ui
            .palette_cmds
            .retain(|c| !matches!(c.origin, CommandOrigin::AiPassthrough { .. }));
        if is_ai {
            let label = if rest.is_empty() {
                rustshell_core::i18n::t("问 AI").to_string()
            } else {
                rustshell_core::i18n::ts(format!("问 AI：{rest}"), format!("Ask AI: {rest}"))
            };
            self.panel_ui.palette_cmds.push(CommandEntry {
                label,
                i18n_key: None,
                default_shortcut: None,
                action_id: None,
                keywords: vec![q.to_string()],
                // AI 直通条目没有连接信息
                detail: None,
                group: None,
                origin: CommandOrigin::AiPassthrough {
                    text: rest.to_string(),
                },
            });
        }
        cx.notify();
    }

    /// 判定一条面板查询词是不是「问 AI」，并取出描述正文。
    ///
    /// 口径（纯函数，可直接单测）：
    /// * 前缀 `ai` **大小写不敏感**（`ai` / `AI` / `Ai` 一视同仁）；
    /// * 前缀之后要么**到头**、要么跟一个 ASCII 空白（空格 / Tab / 换行），
    ///   空白之后的才是描述正文（两侧空白都吃掉）；
    /// * `ai看看` 这种**中间没空白**的**不算** —— 它多半是别的命令的前缀
    ///   （`aixxx`），抢走会让用户再也搜不到那条命令；这条与旧实现一致，
    ///   本次只修大小写口径，不改这条边界；
    /// * 面板没打开时一律不注入（`palette_state` 为空 ⇒ 没人看得见这条）。
    ///
    /// 切勿把字节切片 `&q[2..]` 之外的下标算法搬回调用点：`ai` 之后若直接跟非 ASCII
    /// 字符，按字节推进的偏移会落在字符中间而 panic（中文描述必须走 char 边界）。
    /// 这里返回 `&str` 切片，调用方再 `to_string()`。
    fn parse_ai_query(q: &str, palette_open: bool) -> (bool, &str) {
        if !palette_open {
            return (false, "");
        }
        // 前缀判定在**字节层**做，且绝不切片：`&q[..q.len().min(2)]` 这类写法在
        // 「首字符是多字节」时（`中` 占 3 字节，`min(2)` 取到下标 2）会 panic ——
        // 2026-10-12 本用例自己抓到了这一点（`ai_prefix_slice_is_panic_free_for_short_inputs`）。
        // 先看字节就不用切，天然不越界；首字节非 ASCII 字母时也不可能命中 `ai`。
        let b = q.as_bytes();
        if b.len() < 2 || !b[0].eq_ignore_ascii_case(&b'a') || !b[1].eq_ignore_ascii_case(&b'i') {
            return (false, "");
        }
        // 前两字节已确认是 ASCII ⇒ 下标 2 必落在 char 边界上，这一下切片是安全的。
        let after = &q[2..];
        match after.as_bytes().first() {
            Some(b' ') | Some(b'\t') | Some(b'\n') | Some(b'\r') => (true, after.trim_start()),
            None => (true, ""),
            _ => (false, ""),
        }
    }

    pub(super) fn close_palette_global(cx: &mut App) {
        if let Some(hub) = cx.try_global::<UiHub>() {
            let main = hub.main.clone();
            let _ = main.update(cx, |m, cx| {
                m.panel_ui.palette_state = None;
                // 2026-09-20 运行反馈「主窗格为空，命令面板快捷键呼不出来」：
                // 命令面板是**抢焦点**的浮层（`open_command_palette` 里 `window.focus(&fh)`），
                // 关掉它时必须把焦点还回去。否则窗口焦点停在 `CommandState` 的句柄上，
                // 而那个节点已经不在本帧元素树里 ⇒ gpui 的派发路径退化成派发树根节点，
                // 根 div 上的 `on_action` 一个都收不到（Ctrl+Shift+P 按了没反应）。
                // 置 `pending_focus` 让 `render` 用统一口径收敛：还有终端就还给终端，
                // 空窗格就还给视图级句柄 `self.panel_ui.focus`（那枚 0×0 焦点锚点，见
                // `render_focus_anchor`）。两段式是必须的 —— 这个 App 上下文里没有 Window。
                m.tabs_state.pending_focus = true;
                cx.notify();
            });
        }
    }
    /// 执行一条非内置命令条目（AI 直通 到 交给 AI；快速连接 到 发起连接）。
    ///
    /// 内置条目不经过这里：它们挂的是真实 Action，由 `Command` 组件在 confirm 时
    /// 直接派发到根处理器（见 `render_command_palette`）。
    pub fn execute_command(
        &mut self,
        entry: rustshell_core::command::CommandEntry,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use rustshell_core::command::CommandOrigin;
        match entry.origin {
            CommandOrigin::AiPassthrough { text } => self.ai_passthrough(&text, w, cx),
            CommandOrigin::ConnectProfile { profile_id } => {
                self.connect_profile(&profile_id, w, cx)
            }
            CommandOrigin::Builtin => match entry.action_id {
                Some("ToggleSidebar") => {
                    self.panel_ui.sidebar_collapsed = !self.panel_ui.sidebar_collapsed;
                }
                Some("OpenSettings") => self.open_settings(w, cx),
                _ => {}
            },
        }
        cx.notify();
    }
    pub(super) fn render_command_palette(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        use gpui::Styled as _;
        let Some(state) = self.panel_ui.palette_state.clone() else {
            return div().into_any_element();
        };
        // items 与 confirm 回调共用同一份分区快照。
        //
        // 分区顺序必须与 `Command` 的 `IndexPath` 坐标系一致：显式 group 的
        // `section` 是**组的注册顺序**（gpui-component 的 `command/state.rs` 里
        // `section_ix = group_ix + usize::from(has_ungrouped_items)`；这里只挂
        // group，故 0 / 1），`row` 是组内序号。confirm 回调拿到的就是这对坐标 ——
        // 渲染顺序与回查顺序一旦分叉，点「快速连接」里的机器会去执行内置命令。
        //
        // 2026-09-17 用户要求「快速连接组排到最前面」：快速连接 = 第 0 组。
        let mut group_items: [Vec<rustshell_core::command::CommandEntry>; 2] =
            [Vec::new(), Vec::new()];
        for c in &self.panel_ui.palette_cmds {
            let is_connect = matches!(
                c.origin,
                rustshell_core::command::CommandOrigin::ConnectProfile { .. }
            );
            group_items[usize::from(!is_connect)].push(c.clone());
        }

        let items_of = |list: &[rustshell_core::command::CommandEntry]| -> Vec<CommandItem> {
            list.iter()
                .map(|c| {
                    let label = c.label.clone();
                    let mut it = CommandItem::new().label(label.clone());
                    if let Some(sc) = c.default_shortcut {
                        it = it.keywords([sc]);
                    }
                    // 标签之外的搜索词（连接的主机名 / 用户名、AI 直通的整条查询词）
                    if !c.keywords.is_empty() {
                        it = it.keywords(c.keywords.clone());
                    }
                    // 内置命令挂真实 Action：confirm 时由组件自动派发到根处理器
                    if let Some(id) = c.action_id {
                        it = it.action(match id {
                            "ToggleSidebar" => Box::new(ToggleSidebar),
                            "OpenSettings" => Box::new(OpenSettings),
                            // 2026-09-17「命令窗口新增【新增连接】【AI助手】」
                            "NewConnection" => Box::new(NewConnection),
                            "AiAssistant" => Box::new(AiAssistant),
                            // 2026-09-29「取消恢复上次会话功能」：启动不再自动弹框，
                            // 面板里这条是恢复功能的唯一入口。
                            "RestoreLastSession" => Box::new(RestoreLastSession),
                            // 2026-09-20「新增连接下面增加新增本地 Shell」：面板里这条
                            // 与 Ctrl+T 落到**同一个** `NewTab` 处理器，不另建动作。
                            // 2026-09-17 撤下的「终端内搜索」不在面板了，它仍走
                            // Ctrl+F / 终端右键菜单，故这里的分支一并撤掉。
                            "NewLocalShell" => Box::new(NewTab),
                            _ => Box::new(CommandPalette),
                        });
                    }
                    // 2026-09-17 用户要求「快速连接里的连接要带 ip 端口 名称 组名」：
                    // 连接条目换成自定义行 —— 主行 = 连接名，次行 = `用户名@主机:端口`，
                    // 右端挂所属分组名。
                    // 官方文档：自定义行**整体替换**默认行（含图标），所以这里不再重复
                    // 画图标；行高/内边距/选中底色仍由官方行外壳负责，文字颜色一律
                    // **继承**行样式（选中行底色是 accent，写死 `fg_primary` 在选中态
                    // 会变成低对比），次行只用 `opacity` 压暗。
                    if let Some(detail) = c.detail.clone() {
                        let group = c.group.clone();
                        it =
                            it.child(move |_window, _cx| {
                                h_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .gap_2()
                                    .items_center()
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .flex()
                                            .flex_col()
                                            .child(div().truncate().child(label.clone()))
                                            .child(
                                                div()
                                                    .truncate()
                                                    .text_xs()
                                                    .opacity(0.7)
                                                    .child(detail.clone()),
                                            ),
                                    )
                                    .children(group.clone().map(|g| {
                                        div().flex_shrink_0().text_xs().opacity(0.7).child(g)
                                    }))
                            });
                    }
                    it
                })
                .collect()
        };
        let connect_group = CommandGroup::new()
            .label(rustshell_core::i18n::t("快速连接"))
            .items(items_of(&group_items[0]));
        let command_group = CommandGroup::new()
            .label(rustshell_core::i18n::t("命令"))
            .items(items_of(&group_items[1]));

        div()
            .id("palette-scrim")
            .absolute()
            .inset_0()
            // 遮罩随主题派生（Dark 35% / Light 20%），不再硬编码 black 0.35
            .bg(crate::theme::scrim(cx))
            .flex()
            .items_start()
            .justify_center()
            // 遮罩可点击关闭（对齐官方 Dialog 的 overlay_closable 行为）。
            // 此前遮罩既无 id 也无命中处理：点击会穿透到下方终端并夺走焦点，
            // 之后 ESC 再也到不了 Command，命令面板只能靠重启关掉。
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|m, _e, w, cx| {
                    // 与 `on_cancel` 同一口径：点遮罩关掉面板，同样要把焦点还回去。
                    // 少了这一步，关面板之后焦点停在已不在树里的 `CommandState` 句柄上，
                    // 之后按 Ctrl+Shift+P 再也呼不出面板（2026-09-20 运行反馈）。
                    if !m.has_focusable_terminal() {
                        let fh = m.panel_ui.focus.clone();
                        w.focus(&fh, cx);
                    }
                    m.panel_ui.palette_state = None;
                    m.tabs_state.pending_focus = true;
                    cx.notify();
                }),
            )
            .child(
                div()
                    .mt_24()
                    // 2026-09-18 用户要求加宽：560 → 680（快速连接行有次行 ip:端口，太窄会截断）
                    .w(px(680.))
                    // 面板内部点击不冒泡到遮罩（避免误关）
                    .on_mouse_down(MouseButton::Left, |_e, _w, cx| cx.stop_propagation())
                    .child(
                        Command::new(&state)
                            // 组序即 `IndexPath.section` 的坐标（见上面 group_items）
                            .group(connect_group)
                            .group(command_group)
                            // 2026-09-17 用户要求「加长快捷命令高度」：官方默认上限
                            // 18.75rem（300px）只够十来行，而连接行是两行文字。
                            .max_h(px(520.))
                            .placeholder(rustshell_core::i18n::t("搜索命令…（Ctrl + Shift + P）"))
                            .on_query(|query, w, cx| {
                                // AI 直通：ai <描述> 动态注入条目。
                                ui_call(
                                    "",
                                    move |m, _id, w, cx| {
                                        m.palette_on_query(query, w, cx);
                                    },
                                    w,
                                    cx,
                                );
                            })
                            .on_cancel({
                                // 焦点立即交还：`close_palette_global` 只能置旗（它是
                                // App 上下文，没有 Window），而这里正好拿得到 `&mut Window`。
                                // 空窗格 / 还在连接（没有可聚焦终端）时直接落回视图级句柄
                                // —— 那枚锚点本帧就在树里；有终端时交给下一帧的
                                // `pending_focus` 收敛到终端（同样的口径）。
                                let fh = self.panel_ui.focus.clone();
                                let has_term = self.has_focusable_terminal();
                                move |w, cx| {
                                    if !has_term {
                                        w.focus(&fh, cx);
                                    }
                                    Self::close_palette_global(cx);
                                }
                            })
                            .on_confirm(move |path, w, cx| {
                                // 按「分区 + 区内序号」回查（见上面 group_items 的坐标系说明）
                                let entry = group_items
                                    .get(path.section)
                                    .and_then(|s| s.get(path.row))
                                    .cloned();
                                if let Some(entry) = entry {
                                    let is_builtin = entry.action_id.is_some();
                                    ui_call(
                                        "",
                                        move |m, _id, w, cx| {
                                            // 内置命令的 action 已由组件派发，这里只处理其余条目
                                            if !is_builtin {
                                                m.execute_command(entry, w, cx);
                                            }
                                        },
                                        w,
                                        cx,
                                    );
                                }
                                Self::close_palette_global(cx);
                            }),
                    ),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::MainView;

    // ---------------------------------------------------------------------
    // F-24 / 命令面板：AI 直通判定（`parse_ai_query`）
    //
    // 2026-10-12 覆盖度盘点发现：这条链路此前**零测试**，而判定式写的是
    // `q.starts_with("ai ") || q.eq_ignore_ascii_case("ai")` —— 前者大小写敏感、
    // 后者不敏感 ⇒ 输 `AI 帮我看看磁盘` 不命中、输 `ai` 命中，同一功能两套标准。
    // 纯函数判定，没有理由不测。
    // ---------------------------------------------------------------------

    /// 大小写必须**一视同仁**：这是本次修复的正题。
    #[test]
    fn ai_prefix_is_case_insensitive() {
        for q in [
            "ai 帮我看看磁盘",
            "AI 帮我看看磁盘",
            "Ai 帮我看看磁盘",
            "aI 帮我看看磁盘",
        ] {
            let (is_ai, rest) = MainView::parse_ai_query(q, true);
            assert!(is_ai, "「{q}」应命中 AI 直通（大小写不该影响判定）");
            assert_eq!(rest, "帮我看看磁盘", "「{q}」的正文解析错了");
        }
    }

    /// 面板没打开时不注入（`palette_state` 为空 ⇒ 没人看得见这条）。
    #[test]
    fn ai_prefix_needs_an_open_palette() {
        let (is_ai, _) = MainView::parse_ai_query("ai 看看", false);
        assert!(!is_ai, "面板未打开时不该注入 AI 条目");
        // 前提自证：同样的词在面板打开时确实命中，否则本用例是假绿。
        assert!(MainView::parse_ai_query("ai 看看", true).0);
    }

    /// 光键 `ai`（无正文）命中且正文为空 ⇒ 标签退化成「问 AI」。
    #[test]
    fn bare_ai_keyword_is_accepted() {
        for q in ["ai", "AI", "Ai"] {
            let (is_ai, rest) = MainView::parse_ai_query(q, true);
            assert!(is_ai, "「{q}」应命中");
            assert_eq!(rest, "", "「{q}」没有正文");
        }
    }

    /// `ai` 后面直接跟非空白字符 ⇒ **不命中**。
    ///
    /// 这类多半是别的命令的前缀（`aixxx` / `aiblah`），抢走会让用户再也搜不到
    /// 那条命令。这条边界与旧实现一致，本次只修大小写，不改它。
    #[test]
    fn ai_must_be_followed_by_whitespace_or_end() {
        for q in ["aixxx", "aiblah", "aid", "air"] {
            let (is_ai, _) = MainView::parse_ai_query(q, true);
            assert!(!is_ai, "「{q}」不该被当成 AI 直通（会抢走别的命令）");
        }
    }

    /// 非 ASCII 正文必须走 char 边界 —— 旧实现里的 `q[2..]` 在
    /// 「`ai` + 中文」上是对的，但一旦有人把偏移写错就是 panic。
    /// 这里正面钉住：`ai` 后直接跟中文时，正文原样完整取回。
    #[test]
    fn ai_body_with_non_ascii_never_panics() {
        let (is_ai, rest) = MainView::parse_ai_query("ai 磁盘满了帮我看看", true);
        assert!(is_ai);
        assert_eq!(rest, "磁盘满了帮我看看");

        // 分隔空白本身也要吃掉（多个空白 / Tab / 换行都算分隔）。
        for q in ["ai   多余空格", "ai\t制表符", "ai\n换行"] {
            let (is_ai, rest) = MainView::parse_ai_query(q, true);
            assert!(is_ai, "「{q:?}」应命中");
            assert_eq!(rest, q[2..].trim(), "「{q:?}」的正文应去掉分隔空白");
        }
    }

    /// 非 `ai` 开头的普通查询一律不命中（别把正常搜索劫持成 AI 直通）。
    #[test]
    fn ordinary_queries_are_not_ai() {
        for q in ["", "a", "s", "nett", "新建连接", "1.2.3.4", "  "] {
            let (is_ai, _) = MainView::parse_ai_query(q.trim_start(), true);
            assert!(!is_ai, "「{q:?}」不该命中 AI 直通");
        }
    }

    /// 前缀的字节切片不许 panic：`q` 短于 2 字节（空串 / 单字符）时也要安全返回。
    ///
    /// 旧写法 `q[2..]` 在这些输入上会 panic，这条用例把它钉死。
    #[test]
    fn ai_prefix_slice_is_panic_free_for_short_inputs() {
        for q in ["", " ", "a", "A", "中"] {
            // 不 panic 即通过；再钉一条语义：除非恰好是 `ai`，否则不命中。
            let (is_ai, rest) = MainView::parse_ai_query(q, true);
            assert!(rest.is_empty(), "「{q:?}」不该带出正文");
            assert_eq!(
                is_ai,
                q.eq_ignore_ascii_case("ai"),
                "「{q:?}」的判定语义不对"
            );
        }
    }

    // ---------------------------------------------------------------------
    // 键位录制收尾（`finish_key_record`）的五条分支
    //
    // 2026-10-12 覆盖度盘点发现：本文件整体零测试，而这五条分支全是**纯逻辑**
    //（有明确的 if/早返回），没有任何「UI 不好测」的借口。冲突检测一旦失效，
    // 用户按 Ctrl+Shift+P 会命中别的命令，且**没有任何报错**。
    // ---------------------------------------------------------------------

    /// 进程级串行锁 + 临时配置目录：`finish_key_record` 会写 `paths::keymap_path()`，
    /// 而本轮为此给 `paths::config_dir()` 加了 `RUSTSHELL_CONFIG_DIR` 覆盖。
    ///
    /// 锁与目录都走 `crate::test_support::config_env_guard` —— **crate 级唯一一把**
    /// （2026-10-12 踩过：各文件各挂一把 `static Mutex` 时，`settings_actions`
    /// 会并行覆盖掉本文件刚设的目录，间歇性失败而单跑永远绿）。
    fn km_env_guard(name: &str) -> (crate::test_support::EnvRestore, std::path::PathBuf) {
        let (guard, dir) = crate::test_support::config_env_guard(name);
        let km_path = dir.join("keymap.json");
        (guard, km_path)
    }

    /// 在真窗口里造一个 `MainView`，并把它的实体交出来（`MainView::new` 要 Window）。
    ///
    /// ⚠️ `gpui_component::init` **必须在建窗口之前**跑：`MainView::new` 与
    /// `Root::new` 在构造期就取 `theme::Theme` 全局态（`cx.theme::<Theme>()`），
    /// 晚一步就 panic「no state of type gpui_component::theme::Theme exists」。
    fn main_view_entity(cx: &mut gpui::TestAppContext) -> gpui::Entity<MainView> {
        use gpui::AppContext as _;
        use std::cell::RefCell;
        use std::rc::Rc;

        cx.update(gpui_component::init);

        let slot: Rc<RefCell<Option<gpui::Entity<MainView>>>> = Rc::new(RefCell::new(None));
        let slot2 = slot.clone();
        cx.add_window_view(move |window, cx| {
            let main = cx.new(|cx| MainView::new(window, cx));
            *slot2.borrow_mut() = Some(main.clone());
            let shell = cx.new(|_| crate::main_view::AppShell::new(main));
            gpui_component::Root::new(shell, window, cx)
        });
        let out = slot.borrow().clone().expect("MainView 实体没拿到");
        out
    }

    /// 分支 1：`escape` = 取消录制，**不落盘、不提示**。
    #[gpui::test]
    fn finish_key_record_escape_cancels_without_writing(cx: &mut gpui::TestAppContext) {
        let (_guard, km_path) = km_env_guard("escape");
        let main = main_view_entity(cx);

        main.update(cx, |m, cx| {
            // 直接置录制态，省掉 `start_key_record` 的按键拦截器（那部分
            // 属 `keystroke_to_spec`，在 `keybinds.rs` 里已单独测过）。
            m.panel_ui.keymap_recording = Some("NewTab".to_string());
            m.finish_key_record("escape".to_string(), cx);

            assert!(
                !km_path.exists(),
                "Esc 取消不该落盘（不该凭空造出 keymap.json）"
            );
            assert!(
                m.panel_ui.keymap_hint.is_none(),
                "Esc 是「用户主动取消」，不该弹任何提示：{:?}",
                m.panel_ui.keymap_hint
            );
            // 录制标记已被 take 掉（否则下一次 finish 会记到旧的 action 上）
            assert!(m.panel_ui.keymap_recording.is_none(), "录制态没收掉");
        });
    }

    /// 分支 2：无 Ctrl/Alt 修饰键 = 拒绝，给出提示，**不落盘**。
    #[gpui::test]
    fn finish_key_record_rejects_bare_key_with_a_hint(cx: &mut gpui::TestAppContext) {
        let (_guard, km_path) = km_env_guard("bare");
        let main = main_view_entity(cx);

        main.update(cx, |m, cx| {
            m.panel_ui.keymap_recording = Some("NewTab".to_string());
            // 裸键：没有修饰键
            m.finish_key_record("k".to_string(), cx);

            assert!(
                !km_path.exists(),
                "裸键不该落盘（keymap.json 不该被建出来）"
            );
            let hint = m.panel_ui.keymap_hint.as_deref().unwrap_or_else(|| {
                panic!("裸键必须给出提示（用户按了键却毫无反应 = 以为功能坏了）")
            });
            assert!(
                hint.contains("修饰键") || hint.contains("modifier"),
                "提示应当说明「需要修饰键」，实际：{hint}"
            );
        });
    }

    /// 分支 3：与别的 action 冲突 = 拒绝，提示里**点名冲突对象**，不落盘。
    #[gpui::test]
    fn finish_key_record_rejects_conflict_and_names_it(cx: &mut gpui::TestAppContext) {
        let (_guard, km_path) = km_env_guard("conflict");
        let main = main_view_entity(cx);

        main.update(cx, |m, cx| {
            // 取一条真实存在的默认键位去撞它 —— 用 `snapshot_rows()` 保证
            // 撞的是**真键位表**里的组合，而不是随便编一个字符串。
            let rows = crate::keybinds::snapshot_rows();
            let victim = rows
                .iter()
                .find(|r| r.action_id == "NewTab")
                .expect("默认键位表里必须有 NewTab");
            let taken = rustshell_platform::keymap::resolve(&victim.spec);

            m.panel_ui.keymap_recording = Some("ToggleSidebar".to_string());
            m.finish_key_record(taken.clone(), cx);

            assert!(!km_path.exists(), "冲突时不该落盘");
            let hint = m.panel_ui.keymap_hint.as_deref().unwrap_or_else(|| {
                panic!("冲突必须提示（静默拒绝 = 用户反复按同一个键位却没人告诉他为什么）")
            });
            assert!(
                hint.contains("冲突") || hint.contains("Conflict"),
                "提示应当说明冲突，实际：{hint}"
            );
            // 提示里要点名是哪个动作冲突，否则用户不知道该换哪个
            assert!(
                hint.contains(&victim.label) || hint.contains("NewTab"),
                "提示必须点名冲突对象「{}」，实际：{hint}",
                victim.label
            );
        });
    }

    /// 分支 5：合法且不冲突 = 落盘 + 录制态清空 + 键位表刷新为新组合。
    #[gpui::test]
    fn finish_key_record_persists_a_valid_combo(cx: &mut gpui::TestAppContext) {
        let (_guard, km_path) = km_env_guard("persist");
        let main = main_view_entity(cx);

        main.update(cx, |m, cx| {
            let before = crate::keybinds::snapshot_rows();
            let target = before
                .iter()
                .find(|r| r.action_id == "NewTab")
                .expect("默认键位表里必须有 NewTab")
                .spec
                .clone();

            m.panel_ui.keymap_recording = Some("NewTab".to_string());
            m.finish_key_record(target.clone(), cx);

            assert!(
                km_path.exists(),
                "合法组合必须落盘到 keymap.json（路径：{}）",
                km_path.display()
            );
            // 落盘内容里应当能找到这条覆盖
            let raw = std::fs::read_to_string(&km_path).expect("keymap.json 读不出来");
            assert!(
                raw.contains("NewTab"),
                "keymap.json 里应有 NewTab 的覆盖记录，实际内容：{raw}"
            );
            assert!(
                m.panel_ui.keymap_hint.is_none(),
                "成功路径不该留提示：{:?}",
                m.panel_ui.keymap_hint
            );
            assert!(m.panel_ui.keymap_recording.is_none(), "成功后录制态要清掉");
            // 键位表快照已刷新为新组合，且标记 overridden
            let after = crate::keybinds::snapshot_rows();
            let row = after
                .iter()
                .find(|r| r.action_id == "NewTab")
                .expect("刷新后仍应有 NewTab");
            assert_eq!(row.spec, target, "键位表没刷新成新组合");
            assert!(row.overridden, "落过盘的组合应标记 overridden");
        });
    }
}
