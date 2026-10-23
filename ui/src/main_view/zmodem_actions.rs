//! ZMODEM 的宿主侧：拖放上传入口、窗格里的传输浮层、`poll` 心跳。
//!
//! 协议本身全在引擎的字节泵里（`rustshell_terminal::ZmodemPump`，直接吃 PTY / SSH
//! 转码前的原始字节），所以本地 Shell、Telnet、SSH 共用同一套 `rz`/`sz`。这一层只做
//! 三件事：把拖进来的路径交给**这一块**终端、把泵里的状态画成一行、用 300ms 心跳
//! 驱动它的超时判定与结果提示消退。

use std::path::PathBuf;

use gpui::{
    div, prelude::FluentBuilder as _, px, AnyElement, App, Entity, InteractiveElement as _,
    IntoElement as _, ParentElement as _, SharedString, Styled as _, Window,
};
use gpui_component::{button::Button, h_flex, Sizable as _};
use rustshell_core::i18n::{is_en, t, ts};
use rustshell_terminal::{ZmodemNotice, ZmodemPhase, ZmodemStatus, ZmodemUploadError};

use super::{MainView, UiHub};
use crate::theme::to_hsla as hex;

/// 浮层里的进度条宽度：定宽，免得随传输速率抖动重排整个窗格。
const BAR_W: f32 = 72.;

impl MainView {
    /// 心跳：驱动泵内的超时与提示自动消退。任一终端状态变了就要重绘。
    pub(super) fn tick_zmodem(&self, cx: &mut App) -> bool {
        let mut changed = false;
        for term in self
            .tabs_state
            .tabs
            .iter()
            .filter_map(|t| t.terminal.clone())
        {
            changed |= term.update(cx, |v, _| v.zmodem_tick());
        }
        changed
    }

    /// 拖放到终端窗格 = 用 ZMODEM 把这些本地文件发给远端。
    ///
    /// 与文件面板的 SFTP 上传是两条独立的路：这里不要求 SSH，远端装了 `lrzsz` 就能收。
    pub(super) fn zmodem_drop(
        term: Entity<rustshell_terminal::TerminalView>,
        paths: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let result = term.read(cx).start_zmodem_upload(paths);
        // 提示条由主视图持有，取宿主的手法与 `files_panel::drop_uploads` 一致。
        let Some(hub) = cx.try_global::<UiHub>() else {
            return;
        };
        let main = hub.main.clone();
        let _ = main.update(cx, |m, cx| match result {
            Ok(start) if start.skipped_dirs > 0 => m.set_quick_toast(
                crate::notice::NoticeKind::Warning,
                &ts(
                    format!("暂不支持拖入文件夹（已跳过 {} 个）", start.skipped_dirs),
                    format!(
                        "Folders are not supported yet ({} skipped)",
                        start.skipped_dirs
                    ),
                ),
                window,
                cx,
            ),
            Ok(_) => {}
            Err(ZmodemUploadError::Busy) => m.set_quick_toast(
                crate::notice::NoticeKind::Warning,
                t("已有传输在进行"),
                window,
                cx,
            ),
            Err(ZmodemUploadError::NoFiles) => m.set_quick_toast(
                crate::notice::NoticeKind::Warning,
                t("没读到拖入的文件"),
                window,
                cx,
            ),
            Err(ZmodemUploadError::SessionClosed | ZmodemUploadError::WriteFailed) => m
                .set_quick_toast(
                    crate::notice::NoticeKind::Error,
                    t("会话已断开，无法发送"),
                    window,
                    cx,
                ),
        });
    }

    /// 窗格右下角的传输浮层：进度（当前文件 + 已传/总量 + 第几个文件）或结束提示。
    ///
    /// `None` = 此刻无事可报（空闲，且没有待消退的结果提示），窗格一个字都不多画。
    pub(super) fn render_zmodem_bar(
        term: &Entity<rustshell_terminal::TerminalView>,
        status: &ZmodemStatus,
        cx: &mut App,
    ) -> Option<AnyElement> {
        let line = Self::zmodem_line(status)?;
        let tok = crate::theme::current(cx);
        let active = status.phase.is_active();
        let percent = match status.total {
            Some(total) if total > 0 => (status.transferred as f32 / total as f32).clamp(0.0, 1.0),
            _ => 0.0,
        };
        let ent = term.clone();
        Some(
            h_flex()
                .id("term-zmodem-bar")
                .debug_selector(|| "term-zmodem-bar".to_string())
                .absolute()
                .right_2()
                .bottom_2()
                // 浮层自己吃掉鼠标：漏给下面的终端会打断划选（与断连条同一口径）
                .occlude()
                .gap_2()
                .items_center()
                .px_2()
                .py_1()
                .rounded(px(6.))
                .bg(hex(tok.bg_overlay))
                .border_1()
                .border_color(hex(tok.border_subtle))
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(hex(tok.fg_primary))
                        .child(SharedString::from(line)),
                )
                .when(active && status.total.is_some(), |d| {
                    d.child(
                        div()
                            .w(px(BAR_W))
                            .h(px(3.))
                            .rounded(px(2.))
                            .bg(hex(tok.border_subtle))
                            .child(
                                div()
                                    .h_full()
                                    .rounded(px(2.))
                                    .bg(hex(tok.accent))
                                    .w(px(BAR_W * percent)),
                            ),
                    )
                })
                .when(active, |d| {
                    d.child(
                        Button::new("zmodem-cancel")
                            .xsmall()
                            .label(t("取消"))
                            .on_click(move |_e, _w, cx| {
                                let _ = ent.update(cx, |v, _| v.cancel_zmodem());
                            }),
                    )
                })
                .into_any_element(),
        )
    }

    /// 浮层那一行文案。引擎只给阶段与字节数，文案在这里按当前语言拼。
    fn zmodem_line(status: &ZmodemStatus) -> Option<String> {
        if let Some(notice) = status.notice.as_ref() {
            return Some(match notice {
                ZmodemNotice::Completed { files, dir } => match (dir.as_deref(), is_en()) {
                    (Some(dir), true) => format!("{files} file(s) saved to {dir}"),
                    (Some(dir), false) => format!("已保存到 {dir}（{files} 个文件）"),
                    (None, true) => format!("{files} file(s) transferred"),
                    (None, false) => format!("已发送 {files} 个文件"),
                },
                ZmodemNotice::Cancelled => t("传输已取消").to_string(),
                ZmodemNotice::RemoteNotReady => ts(
                    "远端没有可用的 rz（已超时）".into(),
                    "No usable rz on the remote (timed out)".into(),
                ),
                ZmodemNotice::Stalled => ts(
                    "传输长时间无响应，已中止".into(),
                    "Transfer stalled, aborted".into(),
                ),
                // 引擎原文（协议 / 本地 I/O 细节）：直译只会丢掉定位线索
                ZmodemNotice::Failed(e) => {
                    ts(format!("传输失败：{e}"), format!("Transfer failed: {e}"))
                }
            });
        }
        match status.phase {
            ZmodemPhase::Idle => None,
            ZmodemPhase::Waiting => Some(ts(
                "正在等待远端 rz…".into(),
                "Waiting for rz on the remote…".into(),
            )),
            p => {
                let verb = match p {
                    ZmodemPhase::Downloading => t("下载"),
                    ZmodemPhase::Uploading => t("上传"),
                    _ => t("传输"),
                };
                let mut line = format!(
                    "{verb} {}",
                    status.file.clone().unwrap_or_else(|| "…".to_string())
                );
                if let Some(total) = status.total {
                    line.push_str(&format!(
                        " · {}/{}",
                        crate::files_panel::fmt_size(status.transferred),
                        crate::files_panel::fmt_size(total)
                    ));
                }
                if let Some(files_total) = status.files_total {
                    line.push_str(&ts(
                        format!("（文件 {}/{}）", status.files_done + 1, files_total),
                        format!(" (file {}/{})", status.files_done + 1, files_total),
                    ));
                }
                Some(line)
            }
        }
    }
}
