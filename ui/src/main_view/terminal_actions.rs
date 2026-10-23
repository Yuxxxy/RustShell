//! 终端交互域：PTY 写入、终端内搜索（Ctrl+F）、SFTP 事件收敛。
//!
//! 本模块为 `main_view` 的子模块，可直接访问 `MainView` 的私有字段。

use gpui::{
    div, px, Context, Focusable as _, InteractiveElement as _, IntoElement, ParentElement as _,
    Window,
};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    input::Input,
    Sizable as _,
};

use super::{MainView, TransferState};
use rustshell_core::session::SessionId;

/// 一次多目标投递的结果。
///
/// 分开记录 `skipped`（目标本身不可投递：连接中 / 已断开 / 无 PTY）与
/// `failed`（可投递但写入报错），便于提示里给出可操作的明细。
///
/// 注：`TerminalView::write_to_pty` 的签名返回 `()`（内部吞掉 `io::Error`），
/// 因此当前 `failed` 恒为空，保留该字段是为了不改变已有的汇总契约。
#[derive(Debug, Default, Clone, PartialEq)]
pub struct SendOutcome {
    /// 成功写入的会话（标题，按`tabs`顺序去重）
    pub sent: Vec<String>,
    /// 被跳过的会话及原因（标题，原因）
    pub skipped: Vec<(String, String)>,
    /// 写入失败的会话及原因摘要（标题，原因，最长 80 字符）
    pub failed: Vec<(String, String)>,
}

impl SendOutcome {
    pub fn sent_count(&self) -> usize {
        self.sent.len()
    }

    /// 可投递目标总数（成功 + 失败）；`skipped`不算在内（它本就不可投递）
    pub fn attempted(&self) -> usize {
        self.sent.len() + self.failed.len()
    }

    /// 是否全程无写入（用于区分「全部失败」与「部分成功」）
    pub fn all_failed(&self) -> bool {
        self.sent.is_empty() && !self.failed.is_empty()
    }
}

impl MainView {
    /// 把一条命令写入当前激活会话的 PTY（默认末尾回车，写完即执行）。
    ///
    /// 内部委托给 [`Self::send_lines_to_tabs`]（单目标），保证「单发」与
    /// 「多目标投递」走完全同一条写入路径，不产生行为分叉。
    pub fn send_to_active_pty(&mut self, cmd: &str, cx: &mut Context<Self>) {
        let ids: Vec<SessionId> = self.tabs_state.active.clone().into_iter().collect();
        let _ = self.send_lines_to_tabs(&ids, cmd, true, cx);
    }

    /// 多目标投递。
    ///
    /// - 按 `tabs` 顺序、`ids` 去重后逐个投递；
    /// - 每个目标逐行 `write_to_pty(line)`（行尾是否带回车见 `build_lines`）；
    /// - 逐目标错误隔离：单个目标的写失败只记入 `skipped`，绝不 `?` / `return`
    ///   短路，保证其它目标照常收到命令。
    ///
    /// ★ 必须经 `TerminalView::write_to_pty` 写入：`create_terminal_view` 里
    /// `let writer = session.writer.take()` 已把 `PipeWriter` move 进
    /// `TerminalView`，`tab.session.writer` 此后恒为 `None`。早期实现直接读
    /// `tab.session.writer` → 每个活动 Tab 都被判成「无 PTY」跳过，现象就是
    /// 「命令点了完全没反应」。键盘输入路径同样走 `write_to_pty`，两条路径
    /// 由此统一，不再有行为分叉。
    ///
    /// 空行会被跳过（避免连发多个回车）。`append_newline`：是否在脚本末尾追加
    /// 回车符。`false` 时末行不带 `\n`，命令只键入终端不执行。多行脚本的中间行
    /// 始终带 `\n`（否则会被拼成一行），本参数只影响最后一行。
    pub fn send_lines_to_tabs(
        &mut self,
        ids: &[SessionId],
        script: &str,
        append_newline: bool,
        cx: &mut Context<Self>,
    ) -> SendOutcome {
        // 去重且保持 tabs 原有顺序：以 tabs 为基准过滤，天然顺序正确、天然去重
        let mut targets: Vec<SessionId> = Vec::new();
        for t in self.tabs_state.tabs.iter() {
            if ids.iter().any(|id| id == &t.session.id) && !targets.contains(&t.session.id) {
                targets.push(t.session.id.clone());
            }
        }

        let lines = build_lines(script, append_newline);
        // 全空脚本：无可执行内容，直接返回（不写任何 PTY，也不误报成功）
        if lines.is_empty() {
            return SendOutcome::default();
        }

        let mut out = SendOutcome::default();
        for id in targets {
            let Some(tab) = self.tabs_state.tabs.iter_mut().find(|t| t.session.id == id) else {
                continue;
            };
            let title = tab.session.title.clone();
            // 目标状态判定复用 TabEntry 自身的方法
            let reason = if tab.is_connecting() {
                Some(rustshell_core::i18n::t("连接中"))
            } else if tab.is_disconnected() {
                Some(rustshell_core::i18n::t("已断开"))
            } else if tab.is_dead() {
                // 断连态：终端视图还在（画面留给人看），但输入已被引擎封死。
                // 这里必须给出**真实原因**，不能落到下面的「无 PTY」——
                // 用户看到「无 PTY」会以为程序坏了，实际是他的会话掉线了。
                Some(rustshell_core::i18n::t("连接已断开"))
            } else if tab.connect_error.is_some() {
                Some(rustshell_core::i18n::t("连接失败"))
            } else if tab.terminal.is_none() {
                // 无终端视图 = 没有可写入口（writer 已随终端视图创建而 move 走）
                Some(rustshell_core::i18n::t("无 PTY"))
            } else {
                None
            };
            let Some(reason) = reason else {
                let Some(term) = tab.terminal.clone() else {
                    out.skipped
                        .push((title, rustshell_core::i18n::t("无 PTY").to_string()));
                    continue;
                };
                // 注：`write_to_pty` 内部吞掉 `io::Error`（终端引擎签名返回 `()`），
                // 故以「写入调用是否成功抵达」为准，不再伪造失败明细。
                term.update(cx, |tv, _cx| {
                    for line in &lines {
                        tv.write_to_pty(line);
                    }
                });
                out.sent.push(title);
                continue;
            };
            out.skipped.push((title, reason.to_string()));
        }
        out
    }

    /// 打开文件面板（竖栏点击）：对当前 SSH 会话发起首扫
    pub fn files_open(&mut self, cx: &mut Context<Self>) {
        let Some(tab) = self.active_tab() else { return };
        let Some(sftp_tx) = tab.session.sftp_tx.clone() else {
            return;
        };
        let sid = tab.session.id.clone();
        if self.io.files_loaded_session.as_deref() != Some(&sid) {
            self.io.files_loading = true;
            self.io.files_error = None;
            self.io.files_loaded_session = Some(sid);
            // 首扫落在**远端登录用户的家目录**（绝对路径）。这个路径 UI 侧算不出来
            // —— 它是本机进程，只有服务器知道自己的用户名与 home —— 所以发
            // `ListHome` 让 SFTP 侧 `canonicalize(".")` 问一次服务端，
            // 再经 `SftpEvent::Listing::path` 回报成 `/home/ubuntu` 这样的真实路径。
            let _ = sftp_tx.try_send(rustshell_core::sftp::SftpCmd::ListHome);
        }
        cx.notify();
    }

    /// 路径栏失焦：把草稿收回「当前真实目录」，并请求让输入框放弃焦点。
    ///
    /// 2026-09-17 运行反馈「输入路径后，点击别的地方需要有失去焦点」：
    /// 官方 `Input` 在点击别处时本就会失焦（`on_blur` → `InputEvent::Blur`），
    /// 但本应用此前没有订阅 `Blur`，于是**草稿一直留在框里**，看起来像「没失焦」；
    /// 同时焦点句柄有时仍被路径栏持有，下一次键盘输入会继续写进路径栏。
    /// 这里做两件事：值同步回当前目录 + 显式把焦点还给终端。
    pub(super) fn files_path_blur(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.io.files_path_typed && self.io.files_path_sync.is_none() {
            // 没在编辑、也没有待回填的目录：输入框本来就等于当前目录，什么都不用做
            return;
        }
        self.io.files_path_typed = false;
        let current = self.io.files_path.clone();
        self.io
            .files_path_input
            .update(cx, |s, cx| s.set_value(current, window, cx));
        // 不再顺手把焦点塞给终端：用户点到哪个控件，焦点就该落在哪个控件上。
        // 这里只负责「输入框内容与当前目录一致」，焦点由 gpui 的失焦链路自然交还。
        cx.notify();
    }

    /// 文件面板路径栏回车：按输入内容重扫目录。
    ///
    /// 支持绝对路径（`/var/log`、`C:\Users`）、相对路径（`.`、`sub`）、`~` 开头
    /// （`~`、`~/logs`）以及从终端抄过来的 `cd ~` / `cd /var/log` —— 归一化
    /// （只剥 `cd` 前缀、去空白）收在
    /// [`rustshell_core::sftp::normalize_remote_input`]（纯函数，可单测）。
    /// `~` **不在本机展开**（本机家目录送到远端是错的）：SFTP 侧先问服务器要
    /// 家目录（[`rustshell_core::sftp::expand_tilde`]），再 `canonicalize` 落成
    /// 绝对路径。
    pub(super) fn files_goto_typed_path(&mut self, cx: &mut Context<Self>) {
        let raw = self.io.files_path_input.read(cx).value().trim().to_string();
        // 本次跳转后重新接受回填
        self.io.files_path_typed = false;
        let Some(tx) = self.active_tab().and_then(|t| t.session.sftp_tx.clone()) else {
            return;
        };
        let target = rustshell_core::sftp::normalize_remote_input(&raw)
            .unwrap_or_else(|| self.io.files_path.clone());
        self.io.files_loading = true;
        self.io.files_error = None;
        let _ = tx.try_send(rustshell_core::sftp::SftpCmd::List { path: target });
        cx.notify();
    }

    /// 目录列表变化时重建文件面板的**行展示缓存**（`files_rows`）。
    ///
    /// 只在列表真的变了时跑一次（目录内容不变时这些字符串一个字都不会变）：
    /// 渲染期因此不再有 `format_mtime` / `join_path` / `format!` / `SharedString`
    /// 这些「每帧每行 6 次以上分配」（见《性能与帧率优化分析》P0-2）。
    ///
    /// `chrono::Local::now()` 由 `format_entry_mtimes` 在批量入口取一次 ——
    /// 逐行取系统时钟是这里最容易被忽略的隐性成本。
    pub(super) fn rebuild_files_rows(&mut self) {
        let mtimes = rustshell_core::sftp::format_entry_mtimes(&self.io.files_entries);
        let dir = self.io.files_path.clone();
        self.io.files_rows = std::rc::Rc::new(
            self.io
                .files_entries
                .iter()
                .zip(mtimes)
                .map(|(e, mtime)| crate::files_panel::FileRow::new(&dir, e, mtime))
                .collect(),
        );
    }

    /// 收敛 SFTP 事件（poll 调用）。
    ///
    /// 返回**本轮是否消费到了事件**，供 `MainView::poll` 决定要不要 `cx.notify()`：
    /// 没有在途传输时本函数每 300ms 只是对空通道做一次 `try_recv`，
    /// 不该顺带重绘整棵窗口树。
    pub(super) fn sftp_poll_events(&mut self, _cx: &mut Context<Self>) -> bool {
        use rustshell_core::sftp::{sftp_events, SftpEvent};
        let mut changed = false;
        while let Ok(ev) = sftp_events().1.try_recv() {
            changed = true;
            match ev {
                SftpEvent::Listing { path, entries } => {
                    // 用户正在路径栏手输时不回填，免得打断输入
                    if !self.io.files_path_typed {
                        self.io.files_path_sync = Some(path.clone());
                    }
                    self.io.files_path = path;
                    self.io.files_entries = entries;
                    self.rebuild_files_rows();
                    self.io.files_loading = false;
                    self.io.files_error = None;
                }
                SftpEvent::Error { message } => {
                    self.io.files_loading = false;
                    self.io.files_error = Some(message);
                }
                SftpEvent::TransferQueued {
                    transfer_id,
                    name,
                    total,
                } => {
                    self.io.files_transfers.push(TransferState {
                        id: transfer_id,
                        name,
                        bytes: 0,
                        total,
                        done: false,
                        ok: false,
                        note: None,
                    });
                }
                SftpEvent::Progress { transfer_id, bytes } => {
                    if let Some(tr) = self
                        .io
                        .files_transfers
                        .iter_mut()
                        .find(|t| t.id == transfer_id)
                    {
                        tr.bytes = bytes;
                        if tr.total == 0 {
                            tr.total = bytes * 2; // 占位避免除零（总量未知时进度条近似）
                        }
                    }
                }
                SftpEvent::TransferDone {
                    transfer_id,
                    ok,
                    message,
                } => {
                    if let Some(tr) = self
                        .io
                        .files_transfers
                        .iter_mut()
                        .find(|t| t.id == transfer_id)
                    {
                        tr.done = true;
                        tr.ok = ok;
                        if !ok {
                            tr.name = message;
                        }
                    }
                }
                SftpEvent::TransferMeta { transfer_id, total } => {
                    // P2-21：下载 total 由远端 stat 回填，进度条按百分比工作
                    if let Some(tr) = self
                        .io
                        .files_transfers
                        .iter_mut()
                        .find(|t| t.id == transfer_id)
                    {
                        tr.total = total;
                    }
                }
                SftpEvent::TransferNote { transfer_id, text } => {
                    if let Some(tr) = self
                        .io
                        .files_transfers
                        .iter_mut()
                        .find(|t| t.id == transfer_id)
                    {
                        tr.note = Some(text);
                    }
                }
                SftpEvent::Refresh { path } => {
                    // 操作完成后重扫该目录
                    if let Some(tab) = self.active_tab() {
                        if let Some(tx) = tab.session.sftp_tx.clone() {
                            let _ = tx.try_send(rustshell_core::sftp::SftpCmd::List { path });
                        }
                    }
                }
            }
        }
        changed
    }
    /// 键盘焦点是否在激活终端上。
    ///
    /// 全局快捷键（如 F2 重命名连接）需要据此让路：终端里的按键属于远端程序，
    /// 被应用抢走就成了「远端按了没反应」。
    pub(super) fn terminal_has_focus(&self, window: &Window, cx: &Context<Self>) -> bool {
        self.active_tab()
            .and_then(|t| t.terminal.as_ref())
            .map(|term| term.read(cx).focus_handle().is_focused(window))
            .unwrap_or(false)
    }

    /// Tab/Shift+Tab 送入终端（shell 补全）。仅当焦点在激活终端时写入 PTY 并阻断 传播；否则不消费，交还 gpui-component Root 的默认焦点遍历（Tab/TabPrev）。
    pub(super) fn send_tab_to_terminal(
        &mut self,
        bytes: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let focused = self.terminal_has_focus(window, cx);
        if focused {
            if let Some(t) = self.active_tab() {
                if let Some(term) = t.terminal.clone() {
                    term.update(cx, |tv, _| tv.write_to_pty(bytes));
                }
            }
            cx.stop_propagation();
        }
    }
    // 终端搜索（Ctrl+F）

    pub(super) fn open_term_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.active_tab().is_none() {
            return;
        }
        self.panel_ui.term_search_open = true;
        self.panel_ui.term_search_last.clear();
        let fh = self.panel_ui.term_search_input.read(cx).focus_handle(cx);
        window.focus(&fh, cx);
        cx.notify();
    }

    pub(super) fn close_term_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.panel_ui.term_search_open = false;
        self.panel_ui.term_search_last.clear();
        self.panel_ui
            .term_search_input
            .update(cx, |s, cx| s.set_value("", window, cx));
        if let Some(term) = self.active_tab().and_then(|t| t.terminal.clone()) {
            term.update(cx, |tv, _| {
                tv.set_search(None);
            });
        }
        // 焦点还给终端
        self.tabs_state.pending_focus = true;
        cx.notify();
    }

    /// 把输入框当前值同步到激活终端
    pub(super) fn sync_term_search(&mut self, cx: &mut Context<Self>) -> bool {
        let q = self
            .panel_ui
            .term_search_input
            .read(cx)
            .value()
            .trim()
            .to_string();
        if q == self.panel_ui.term_search_last {
            return false;
        }
        self.panel_ui.term_search_last = q.clone();
        if let Some(term) = self.active_tab().and_then(|t| t.terminal.clone()) {
            let opt = if q.is_empty() { None } else { Some(q) };
            term.update(cx, |tv, _| {
                tv.set_search(opt);
            });
        }
        true
    }

    /// 搜索串变更后的防抖应用（120ms 内的连续击键合并成一次）。
    ///
    /// 为什么要防抖：把查询串应用到终端意味着「编译正则 + 视口内逐屏扫描 +
    /// 终端重绘」，而终端重绘又会带上整棵 `MainView` 的重排 —— 逐键执行时
    /// 打字会一卡一卡（实测单帧可达数十毫秒）。停手后再应用，敲键盘这一侧
    /// 只重排搜索浮层本身。
    pub(super) fn schedule_term_search(&mut self, cx: &mut Context<Self>) {
        const DEBOUNCE_MS: u64 = 120;
        // 覆盖旧任务即取消旧定时器（gpui Task drop = cancel）
        self.panel_ui.term_search_debounce = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(DEBOUNCE_MS))
                .await;
            let _ = this.update(cx, |m, cx| {
                if m.sync_term_search(cx) {
                    // 高亮落在终端画面上，需要一帧重绘才看得见
                    cx.notify();
                }
            });
        }));
    }

    pub(super) fn term_search_jump(&mut self, forward: bool, cx: &mut Context<Self>) {
        self.sync_term_search(cx);
        if let Some(term) = self.active_tab().and_then(|t| t.terminal.clone()) {
            let _ = term.update(cx, |tv, _| tv.search_jump(forward));
        }
        cx.notify();
    }
    /// 终端搜索浮层（Ctrl+F）：输入 + 前后跳转 + 关闭；Esc 关闭
    pub(super) fn render_term_search_bar(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        use gpui::Styled as _;
        let t = crate::theme::current(cx);
        use crate::theme::to_hsla as hex;

        div()
            .absolute()
            .top(px(8.))
            .right(px(12.))
            .flex()
            .items_center()
            .gap_1()
            .p(px(6.))
            .rounded(px(6.))
            .bg(hex(t.bg_overlay))
            .border_1()
            .border_color(hex(t.border_subtle))
            // 点击面板不透传到终端，避免清掉划选。
            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
            // 点浮层以外的地方（终端画面、侧栏）时松手：浮层留着让用户接着跳转，
            // 但焦点不能一直锁在搜索框里。捕获阶段先失焦，终端若随后的冒泡里
            // 拿到焦点也不冲突。
            .on_mouse_down_out(cx.listener(|m, _e, w, cx| {
                super::blur_input_on_outside_click(&m.panel_ui.term_search_input, w, cx)
            }))
            .on_key_down(cx.listener(|m, ev: &gpui::KeyDownEvent, w, cx| {
                if ev.keystroke.key == "escape" {
                    m.close_term_search(w, cx);
                }
            }))
            .child(
                Input::new(&self.panel_ui.term_search_input)
                    .small()
                    .w(px(220.)),
            )
            .child(
                Button::new("search-prev")
                    .xsmall()
                    .ghost()
                    .label("↑")
                    .on_click(cx.listener(|m, _e, _w, cx| m.term_search_jump(false, cx))),
            )
            .child(
                Button::new("search-next")
                    .xsmall()
                    .ghost()
                    .label("↓")
                    .on_click(cx.listener(|m, _e, _w, cx| m.term_search_jump(true, cx))),
            )
            .child(
                Button::new("search-close")
                    .xsmall()
                    .ghost()
                    .label("×")
                    .on_click(cx.listener(|m, _e, w, cx| m.close_term_search(w, cx))),
            )
            .into_any_element()
    }
}

/// 把脚本切成「待写入 PTY 的行」，并按配置决定末行是否带回车（纯函数，便于单测）。
///
/// 规则：
/// - 剔除纯空白行（避免连发多个回车）；
/// - 中间行始终以 `\n` 结尾，否则多行脚本会被终端拼成一行；
/// - 末行的 `\n` 由 `append_newline` 决定：`true` = 写完即回车执行，
///   `false` = 只把命令键入终端，留给用户继续补参数；
/// - 脚本全为空白 → 返回空 `Vec`（调用方据此「什么都不做」，且不误报成功）。
fn build_lines(script: &str, append_newline: bool) -> Vec<String> {
    let parts: Vec<&str> = script.lines().filter(|l| !l.trim().is_empty()).collect();
    if parts.is_empty() {
        return Vec::new();
    }
    let last = parts.len() - 1;
    parts
        .iter()
        .enumerate()
        .map(|(i, l)| {
            if i == last && !append_newline {
                (*l).to_string()
            } else {
                format!("{l}\n")
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::build_lines;

    #[test]
    fn single_line_with_newline() {
        assert_eq!(build_lines("ls -la", true), vec!["ls -la\n".to_string()]);
    }

    #[test]
    fn single_line_without_newline() {
        assert_eq!(build_lines("ls -la", false), vec!["ls -la".to_string()]);
    }

    /// 多行脚本：中间行必须带`\n`，只有末行受开关影响
    #[test]
    fn multiline_only_last_line_affected() {
        assert_eq!(
            build_lines("cd /tmp\nls -la", false),
            vec!["cd /tmp\n".to_string(), "ls -la".to_string()]
        );
        assert_eq!(
            build_lines("cd /tmp\nls -la", true),
            vec!["cd /tmp\n".to_string(), "ls -la\n".to_string()]
        );
    }

    /// 空行被剔除，且末行仍是「最后一条非空行」（不是因为尾部空行而误判）
    #[test]
    fn blank_lines_dropped() {
        assert_eq!(
            build_lines("  \n\nls\n   \n", false),
            vec!["ls".to_string()]
        );
    }

    /// 全空白 到 空结果（调用方据此不写 PTY、不报成功）
    #[test]
    fn all_blank_is_empty() {
        assert!(build_lines("   \n\t\n  ", true).is_empty());
        assert!(build_lines("", false).is_empty());
    }
}
