//! F-5 粘贴保护的**宿主半边**：发现挂起的粘贴、弹确认框、兑现两个回答。
//!
//! # 分工：判定在引擎、弹框在宿主
//!
//! 「这次粘贴要不要问」在引擎里判（`TerminalView::request_paste`），因为三个用户
//! 入口里有两个根本不出引擎 —— Ctrl+Shift+V 的键绑与输入法的剪贴板接口都长在
//! `TerminalView` 上，闸装宿主就等于最常用的那条路绕过（详见规格 §1.5 施工展开）。
//! 本模块只负责**问**：谁在等确认、正文长什么样、用户点了哪个按钮。
//!
//! # 为什么检测不经过 poll
//!
//! 挂起的请求是终端实体上的**可读事实**，渲染期直接扫 `tabs` 读它即可。
//! 走 poll（300ms 一轮）置旗的代价是：弹框最多晚到一轮 —— 而粘贴是手起刀落
//! 的一瞬，用户等不到框会以为「没生效」，再按一次 Ctrl+Shift+V。
//! 每帧扫十来个 `Entity::read` 的开销可以忽略（读的是内存里的一个 `Option`）。
//!
//! 正文同理**不存副本**：唯一真值是终端上的挂起请求，这里每次都现取。
//! 会话在弹框期间断连时引擎会把挂起清掉（`set_alive(false)`），
//! [`MainView::paste_prompt_prune`] 据此把框收掉 —— 存副本就做不到这件事。

use gpui::{App, Context, Window};
use rustshell_core::session::SessionId;

use super::{DialogMode, MainView};

/// 预览最多展示几行、每行最多几个字符（超出以 `…` 收尾，末尾另给一行 `…`）。
///
/// 3 行足够让用户认出「这是不是我刚复制的那段」；给满屏预览等于把确认框
/// 变成长文阅读器，反而没人看。
const PREVIEW_LINES: usize = 3;
const PREVIEW_COLS: usize = 72;

impl MainView {
    /// 有终端挂着待确认的粘贴吗？返回**哪条会话**（`DialogMode::PasteConfirm` 的载荷）。
    ///
    /// 按 `tabs` 顺序取第一个 —— 同一帧里最多只会有一条在等（确认框是模态的，
    /// 但用户可能在框弹出前的极短窗口里连按两次，第二次会顶掉第一次的挂起，
    /// 所以「同时多个挂起」在引擎里也不可能存在）。
    pub(super) fn paste_prompt_session(&self, cx: &App) -> Option<SessionId> {
        self.tabs_state.tabs.iter().find_map(|t| {
            let term = t.terminal.as_ref()?;
            term.read(cx)
                .pending_paste()
                .is_some()
                .then(|| t.session.id.clone())
        })
    }

    /// 目标会话当前挂起的粘贴正文：`(行数, 预览文本)`。
    ///
    /// 返 `None` = 这条会话已经不挂着了（断连 / 别处答过 / 标签已关）——
    /// 调用方按「没有可确认的内容」处理，而不是拿一份旧数据渲染。
    pub(super) fn paste_prompt_body(&self, session: &str, cx: &App) -> Option<(usize, String)> {
        let req = self.paste_prompt_request(session, cx)?;
        Some((
            req.lines(),
            paste_preview(&req.text, PREVIEW_LINES, PREVIEW_COLS),
        ))
    }

    /// 挂起的原始请求（内部用；`Clone` 出来是为了不把 `Entity` 的读借用
    /// 拖到调用方后续的 `&mut self` 操作上）。
    fn paste_prompt_request(
        &self,
        session: &str,
        cx: &App,
    ) -> Option<rustshell_terminal::PasteRequest> {
        let term = self
            .tabs_state
            .tabs
            .iter()
            .find(|t| t.session.id == session)?
            .terminal
            .as_ref()?;
        term.read(cx).pending_paste().cloned()
    }

    /// 渲染期维护：确认框指着的那次粘贴已经没了（会话断连 / 标签被关）⇒ 收掉框。
    ///
    /// 不收的后果是「对着一件不存在的事按确认」：正文取不到、确认无处可写，
    /// 用户对着一个空壳框做决定。只有确认框会被这里收 —— 其它对话框的
    /// 生命周期与粘贴请求无关。
    ///
    /// 关闭走 [`MainView::close_dialog_ui`]（它会一并清 `self.dialog`）：
    /// 程序化 `w.close_dialog` **不会**触发 `on_close`，只关官方栈会让业务标记
    /// 永久停在 `Some(PasteConfirm)`，此后所有对话框都被单槽守卫挡掉。
    pub(super) fn paste_prompt_prune(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        let stale = match self.dialog.as_ref() {
            Some(DialogMode::PasteConfirm { session }) => {
                self.paste_prompt_request(session, cx).is_none()
            }
            _ => false,
        };
        if stale {
            self.close_dialog_ui(w, cx);
        }
    }

    /// 用户按下「粘贴」：把视图切到那条会话，再让引擎把挂起的请求写进去。
    ///
    /// 先切会话再写，理由与「断连留现场」同源：粘贴是**可见的副作用**，
    /// 写进一个看不见的后台标签，用户既不知道粘到哪了、也无从核对粘了什么。
    /// 切换用标签栏点击那条路（`place_in_focused_pane`）——分屏时它会把
    /// 目标放进焦点窗格，非分屏时等价于切标签。
    pub(super) fn paste_prompt_accept(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.paste_dialog_session() else {
            return;
        };
        // 会话可能在弹框期间被关掉：那时挂起也没了，`confirm_paste` 会是无处可写的
        // 空操作，这里提前判掉是为了不把「切换到一个已经不存在的标签」当成功。
        if let Some(term) = self
            .tabs_state
            .tabs
            .iter()
            .find(|t| t.session.id == session)
            .and_then(|t| t.terminal.clone())
        {
            self.place_in_focused_pane(&session, cx);
            term.update(cx, |tv, cx| tv.confirm_paste(cx));
        }
        self.close_dialog_ui(w, cx);
    }

    /// 用户放弃这次粘贴（「取消」按钮 / Esc / 点遮罩 / 右上角 X）。
    ///
    /// **零写入**：把终端上挂起的请求丢掉就完事 —— 引擎的 `cancel_paste`
    /// 是这趟的唯一副作用，它不碰管道。
    ///
    /// 由 [`super::MainView::present_dialog`] 的 `on_close` 统一兜底调用
    /// （覆盖 Esc / 遮罩 / X 三条路），按钮那条路先调这里再关框。
    /// 幂等：挂起已被取走（用户点了确认）时是空操作。
    pub(super) fn paste_prompt_decline(&mut self, cx: &mut Context<Self>) {
        if let Some(session) = self.paste_dialog_session() {
            self.paste_prompt_cancel_session(&session, cx);
        }
    }

    /// 当前确认框指的是哪条会话（走 `self.dialog`，非确认框时返 `None`）。
    fn paste_dialog_session(&self) -> Option<SessionId> {
        match self.dialog.as_ref() {
            Some(DialogMode::PasteConfirm { session }) => Some(session.clone()),
            _ => None,
        }
    }

    /// 丢掉指定会话上挂起的粘贴请求（引擎侧的零写入取消）。
    fn paste_prompt_cancel_session(&mut self, session: &str, cx: &mut Context<Self>) {
        if let Some(term) = self
            .tabs_state
            .tabs
            .iter()
            .find(|t| t.session.id == session)
            .and_then(|t| t.terminal.clone())
        {
            term.update(cx, |tv, cx| tv.cancel_paste(cx));
        }
    }
}

/// 预览文本（纯函数）：最多 `max_lines` 行、每行最多 `max_cols` 个字符。
///
/// 行尾按三种写法切（`CRLF` / `CR` / `LF`）—— 粘贴内容可能来自任何平台，
/// 只认 `\n` 会把一整份 macOS/Windows 文本显示成一行。
/// 截断的痕迹必须留着（行尾 `…`、被砍掉的行另起一行 `…`）：
/// 预览少给几行可以，**看着像全部**不行 —— 用户是拿它当决策依据的。
///
/// 按 `char` 数截而不是字节数：按字节切会把中文/emoji 切成半个字符。
pub(super) fn paste_preview(text: &str, max_lines: usize, max_cols: usize) -> String {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let all: Vec<&str> = normalized.split('\n').collect();
    let mut out = String::new();
    for (i, line) in all.iter().take(max_lines).enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let mut chars = line.chars();
        out.extend(chars.by_ref().take(max_cols));
        if chars.next().is_some() {
            out.push('…');
        }
    }
    if all.len() > max_lines {
        out.push_str("\n…");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 预览的行尾切分：三种写法都要认，`CRLF` 不能切出一个空行。
    #[test]
    fn preview_splits_every_newline_flavor() {
        for text in ["a\nb\nc", "a\r\nb\r\nc", "a\rb\rc"] {
            assert_eq!(
                paste_preview(text, 5, 10),
                "a\nb\nc",
                "{text:?} 的三种行尾都应切成三行"
            );
        }
    }

    /// 超出 max_lines 的行被砍掉，并**留下** `…` 痕迹。
    #[test]
    fn preview_marks_the_lines_it_dropped() {
        assert_eq!(paste_preview("a\nb\nc\nd", 3, 10), "a\nb\nc\n…");
        // 正好放得下就不加尾巴（别把「刚好 3 行」显示成「还有更多」）
        assert_eq!(paste_preview("a\nb\nc", 3, 10), "a\nb\nc");
    }

    /// 超长的一行按字符截断并加 `…`：中文不能被按字节切碎。
    #[test]
    fn preview_truncates_long_lines_by_chars() {
        assert_eq!(paste_preview("abcdef", 3, 4), "abcd…");
        assert_eq!(paste_preview("中文中文中文", 3, 4), "中文中文…");
        // 只有一个字符的余量：不能被吞掉
        assert_eq!(paste_preview("abcde", 3, 4), "abcd…");
        // 恰好等于上限，无省略号
        assert_eq!(paste_preview("abcd", 3, 4), "abcd");
    }

    /// 空行要如实显示（粘贴脚本里的空行是结构的一部分）。
    #[test]
    fn preview_keeps_blank_lines() {
        assert_eq!(paste_preview("a\n\nb", 5, 10), "a\n\nb");
        // 结尾换行 = 结尾空行：真实存在，照实显示
        assert_eq!(paste_preview("a\n", 5, 10), "a\n");
    }
}
