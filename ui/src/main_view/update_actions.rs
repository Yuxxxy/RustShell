//! 在线更新：检查 → 下载 → 安装 → 重启。
//!
//! # 提示放在哪（与 shell-rs 对齐）
//!
//! - **标题栏右侧一枚徽标**是唯一的主动提示：有新版本 / 已就绪 / 失败时才出现，
//!   其余时候什么都不显示（不打扰）。点它开更新对话框。
//! - **设置 › 关于**那一行是常驻状态 + 手动入口（检查更新 / 立即更新 / 重启）。
//!
//! 这条分工是抄 shell-rs 的（它的 `UpdateBadge` 只在这三种状态下返回 `Some`，
//! 状态行在「关于」页）。为什么不让它弹模态框：更新不是用户当下要做的事，
//! 弹出来就是打断；而徽标放在标题栏，既不打断也丢不掉。
//!
//! # 三个阶段的分寸
//!
//! - **检查**：启动后延迟一次（[`MainView::update_auto_check`]）+ 关于页手动触发。
//!   自动检查失败**静默**（用户没问，不该被网络错误打扰），手动检查才报错。
//! - **下载**：只在用户点了「立即更新」之后发生，不后台偷跑 —— 几十 MB 的流量
//!   该由用户决定。
//! - **安装**：Windows 的产物是裸 exe，直接换掉当前程序（见
//!   [`rustshell_core::update::replace_running_exe`]）；mac / linux 的产物是
//!   `.dmg` / `.deb`，交给系统安装器打开，本程序不碰 `/usr/bin`。

use gpui::{Context, Window};

use rustshell_core::update;

use super::MainView;

/// 自动检查的延迟：等首屏、连接恢复这些更要紧的事先做完。
const AUTO_CHECK_DELAY: std::time::Duration = std::time::Duration::from_secs(8);

impl MainView {
    /// 启动后自动查一次更新（延迟 [`AUTO_CHECK_DELAY`]，失败静默）。
    ///
    /// 由 `render` 首帧调用：`cx.new(|cx| …)` 期间拿不到自身实体，也就没法 spawn。
    /// 闩在 `update_auto_started` 上（不能用「阶段是 Idle」判断 —— 自动检查失败后
    /// 阶段本来就回到 Idle，那样每帧都会再发一次请求）。
    pub fn update_auto_check(&mut self, cx: &mut Context<Self>) {
        if self.io.update_auto_started {
            return;
        }
        self.io.update_auto_started = true;
        // 顺手清掉上次更新留下的 `<exe>.old`：那时它还被旧进程锁着删不掉。
        update::cleanup_stale_backup();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(AUTO_CHECK_DELAY).await;
            let _ = this.update(cx, |m, cx| {
                // 用户已经手动查过（或正在查）就别再发一次请求。
                if matches!(m.io.update_phase, super::UpdatePhase::Idle) {
                    m.run_update_check(false, cx);
                }
            });
        })
        .detach();
    }

    /// 「关于」页的「检查更新」按钮。
    pub fn check_for_updates(&mut self, cx: &mut Context<Self>) {
        self.run_update_check(true, cx);
    }

    /// 检查本体。`announce = false` 时失败不弹提示（自动检查的路径）。
    fn run_update_check(&mut self, announce: bool, cx: &mut Context<Self>) {
        if self.io.update_phase == super::UpdatePhase::Downloading {
            return;
        }
        self.io.update_phase = super::UpdatePhase::Checking;
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async { update::check_latest() })
                .await;
            let _ = this.update(cx, |m, cx| {
                match result {
                    Ok(release) => {
                        if update::is_newer(&release.version, rustshell_core::APP_VERSION) {
                            m.io.update_release = Some(release);
                            m.io.update_phase = super::UpdatePhase::Available;
                        } else {
                            m.io.update_release = None;
                            m.io.update_phase = super::UpdatePhase::UpToDate;
                        }
                    }
                    Err(e) => {
                        // 自动检查失败不该给用户看网络错误：他根本没要求查。
                        m.io.update_phase = if announce {
                            super::UpdatePhase::Failed(super::MainView::update_fail_text(&e))
                        } else {
                            super::UpdatePhase::Idle
                        };
                    }
                }
                if announce {
                    let (line, trouble) = m.update_status_line();
                    m.set_info(line, !trouble);
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn update_fail_text(e: &anyhow::Error) -> String {
        rustshell_core::i18n::ts(
            format!("更新失败：{e}"),
            format!("Update failed: {e}"),
        )
    }

    /// 标题栏徽标：(文案, 是否是坏消息)。`None` = 不显示。
    ///
    /// 只在三种状态下出现，与 shell-rs 的 `UpdateBadge::badge()` 同口径：
    /// 有新版本 / 已就绪待重启 / 失败。正在检查或已是最新时**不占位置**。
    pub fn update_badge(&self) -> Option<(String, bool)> {
        let version = self
            .io
            .update_release
            .as_ref()
            .map(|r| r.version.clone())
            .unwrap_or_default();
        match &self.io.update_phase {
            super::UpdatePhase::Available => Some((
                rustshell_core::i18n::ts(
                    format!("新版本 v{version}"),
                    format!("v{version} available"),
                ),
                false,
            )),
            super::UpdatePhase::Downloading => Some((
                rustshell_core::i18n::ts("正在下载更新…".into(), "Downloading…".into()),
                false,
            )),
            super::UpdatePhase::Ready => Some((
                rustshell_core::i18n::ts("重启以更新".into(), "Restart to update".into()),
                false,
            )),
            super::UpdatePhase::Failed(_) => Some((
                rustshell_core::i18n::ts("更新失败".into(), "Update failed".into()),
                true,
            )),
            _ => None,
        }
    }

    /// 「关于」页那一行的状态文案 + 是否是坏消息（决定红/灰）。
    pub fn update_status_line(&self) -> (String, bool) {
        let version = self
            .io
            .update_release
            .as_ref()
            .map(|r| r.version.clone())
            .unwrap_or_default();
        let current = rustshell_core::APP_VERSION;
        match &self.io.update_phase {
            super::UpdatePhase::Idle => (
                rustshell_core::i18n::ts(
                    format!("当前版本 v{current}。"),
                    format!("You are on v{current}."),
                ),
                false,
            ),
            super::UpdatePhase::Checking => (
                rustshell_core::i18n::ts("正在检查更新…".into(), "Checking for updates…".into()),
                false,
            ),
            super::UpdatePhase::UpToDate => (
                rustshell_core::i18n::ts(
                    format!("已是最新版本（v{current}）。"),
                    format!("You are up to date (v{current})."),
                ),
                false,
            ),
            super::UpdatePhase::Available => (
                rustshell_core::i18n::ts(
                    format!("发现新版本 v{version}（当前 v{current}）。"),
                    format!("v{version} is available (you are on v{current})."),
                ),
                false,
            ),
            super::UpdatePhase::Downloading => (
                rustshell_core::i18n::ts(
                    format!("正在下载 v{version}…"),
                    format!("Downloading v{version}…"),
                ),
                false,
            ),
            super::UpdatePhase::Ready => (
                rustshell_core::i18n::ts(
                    format!("v{version} 已就绪，重启后生效。"),
                    format!("v{version} is ready — restart to finish."),
                ),
                false,
            ),
            super::UpdatePhase::Failed(why) => (why.clone(), true),
        }
    }

    /// 有可用版本 → 下载产物并装好。
    ///
    /// 找不到本平台的产物（例如刚出了个只打了 tag 的版本）不报「下载失败」，
    /// 而是打开发布页让用户自己挑 —— 那才是真实原因。
    pub fn start_update_download(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(release) = self.io.update_release.clone() else {
            return;
        };
        if matches!(self.io.update_phase, super::UpdatePhase::Downloading) {
            return;
        }
        let Some((triplet, ext)) = update::target_triplet() else {
            self.fail_update(
                rustshell_core::i18n::ts(
                    "当前平台没有可用的安装包，请到发布页手动下载。".into(),
                    "No package for this platform — download it from the release page.".into(),
                ),
                cx,
            );
            cx.open_url(&release.url);
            return;
        };
        let Some(asset) = update::pick_asset(&release.assets, triplet, ext).cloned() else {
            self.fail_update(
                rustshell_core::i18n::ts(
                    "这个版本没有发布安装包，请在发布页手动下载。".into(),
                    "This release has no package attached — download it from the release page."
                        .into(),
                ),
                cx,
            );
            cx.open_url(&release.url);
            return;
        };

        self.io.update_phase = super::UpdatePhase::Downloading;
        cx.notify();
        let dest = update::download_dir().join(&asset.name);
        let install_here = update::installs_itself(ext);

        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn({
                    let dest = dest.clone();
                    let asset = asset.clone();
                    async move {
                        update::download(&asset, &dest)?;
                        // Windows：直接把当前程序换掉（失败会回滚）。
                        if install_here {
                            update::replace_running_exe(&dest)?;
                        }
                        Ok::<_, anyhow::Error>(())
                    }
                })
                .await;
            let _ = this.update(cx, |m, cx| {
                match result {
                    Ok(()) => {
                        if install_here {
                            m.io.update_artifact = Some(dest.clone());
                            m.io.update_phase = super::UpdatePhase::Ready;
                        } else {
                            // .dmg / .deb：交给系统安装器，装不装由系统那一步决定。
                            rustshell_platform::shell_open::open_path(&dest);
                            m.io.update_artifact = Some(dest.clone());
                            m.io.update_phase = super::UpdatePhase::Ready;
                        }
                    }
                    Err(e) => {
                        let text = MainView::update_fail_text(&e);
                        m.io.update_phase = super::UpdatePhase::Failed(text.clone());
                        m.set_info(text, false);
                    }
                }
                cx.notify();
            });
        })
        .detach();
        let _ = window;
    }

    fn fail_update(&mut self, text: String, cx: &mut Context<Self>) {
        self.io.update_phase = super::UpdatePhase::Failed(text.clone());
        self.set_info(text, false);
        cx.notify();
    }

    /// 徽标 / 关于页的「立即更新」都走这里。
    pub fn update_primary_action(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.io.update_phase {
            super::UpdatePhase::Ready => self.restart_for_update(cx),
            super::UpdatePhase::Available => self.start_update_download(window, cx),
            _ => self.check_for_updates(cx),
        }
    }

    /// 重启以完成更新：拉起新进程后退出自己。
    ///
    /// 顺序不能反：先 quit 会让新进程可能还没起来，用户看到「程序关了」。
    /// 新进程的路径就是 `current_exe()`（Windows 上旧映像已经改名成 `.old`，
    /// 那个文件此刻归新进程所有，本进程退出后才能被清掉）。
    pub fn restart_for_update(&mut self, cx: &mut Context<Self>) {
        let Ok(exe) = std::env::current_exe() else {
            self.set_info(
                rustshell_core::i18n::ts("找不到程序路径，无法自动重启".into(), "Cannot locate the executable; restart manually.".into()),
                false,
            );
            cx.notify();
            return;
        };
        match std::process::Command::new(&exe).spawn() {
            Ok(_) => cx.quit(),
            Err(e) => {
                self.set_info(
                    rustshell_core::i18n::ts(
                        format!("自动重启失败：{e}，请手动重新打开。"),
                        format!("Could not restart automatically: {e}. Reopen it manually."),
                    ),
                    false,
                );
                cx.notify();
            }
        }
    }

    /// 徽标点击 / 关于页的「详情」：打开更新对话框。
    pub fn open_update_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.present_dialog(super::DialogMode::Update, window, cx);
    }
}

#[cfg(test)]
mod tests {
    use crate::test_support::prod;

    /// 更新徽标必须挂在**标题栏**里 —— 这是用户点名要对齐 shell-rs 的位置。
    ///
    /// 顺带钉住「平时不显示」：`update_badge()` 只在有新版 / 已就绪 / 失败三种状态下
    /// 返回 `Some`，其余落到 `_ => None`。写成「总显示」不会编译失败，只会让标题栏
    /// 常年挂着一枚没有内容的按钮。
    #[test]
    fn the_update_badge_hangs_in_the_title_bar() {
        let code = prod(include_str!("../main_view.rs"));
        let bar = code
            .find("gpui_component::TitleBar::new()")
            .expect("找不到标题栏构造");
        let badge = code
            .find("update_badge_entry(self, cx)")
            .expect("标题栏里没有更新徽标");
        assert!(bar < badge, "更新徽标必须挂在标题栏的子树里");
        assert!(
            code.contains("titlebar-update-badge"),
            "徽标要挂 `debug_selector`：用例靠它量位置"
        );
        assert!(
            code.contains("m.open_update_dialog(w, cx)"),
            "点徽标必须打开更新对话框（不是直接下载）"
        );

        let actions = prod(include_str!("update_actions.rs"));
        for needle in [
            "super::UpdatePhase::Available",
            "super::UpdatePhase::Ready",
            "super::UpdatePhase::Failed(_)",
        ] {
            assert!(
                actions.contains(needle),
                "徽标缺状态 `{needle}` —— 三种状态（有新版 / 已就绪 / 失败）都要能提示"
            );
        }
        assert!(
            actions.contains("_ => None"),
            "其余阶段必须不显示徽标（常年挂着=噪声）"
        );
    }

    /// 自动检查只能发起一次，且从 render 首帧触发。
    ///
    /// 这里守的是那个**闩**：用「阶段是 Idle」判断会死循环 —— 自动检查失败后阶段
    /// 本来就回到 Idle，于是每帧都发一次请求。
    #[test]
    fn the_startup_check_fires_exactly_once() {
        let main = prod(include_str!("../main_view.rs"));
        assert!(
            main.contains("self.update_auto_check(cx)"),
            "首帧必须发起自动检查"
        );
        let actions = prod(include_str!("update_actions.rs"));
        assert!(
            actions.contains("if self.io.update_auto_started {"),
            "自动检查必须有「只发起一次」的闩"
        );
        assert!(
            actions.contains("background_executor().timer(AUTO_CHECK_DELAY)"),
            "自动检查要延迟：别和首屏抢网络"
        );
        // 自动检查失败要静默（`announce == false`），手动检查才报错
        assert!(
            actions.contains("run_update_check(false, cx)"),
            "自动检查失败不该弹错误给用户（他没要求查）"
        );
    }

    /// 「从 OpenSSH 导入」现在必须**先弹系统选文件对话框**。
    ///
    /// 2026-10-28 用户要求：不再自动去读 `~/.ssh/config`。守两点：
    /// 入口里没有「默认路径」那套（`default_ssh_dir` / `config_path`），
    /// 而是 `prompt_for_paths`。
    #[test]
    fn openssh_import_asks_for_a_file_first() {
        let code = prod(include_str!("connections.rs"));
        assert!(
            code.contains("fn import_openssh_pick_file"),
            "导入入口不见了"
        );
        let at = code
            .find("fn import_openssh_pick_file")
            .expect("找不到导入入口");
        let body = &code[at..];
        let end = body.find("\n    }\n").expect("找不到函数结尾");
        let body = &body[..end];
        assert!(
            body.contains("prompt_for_paths"),
            "导入必须先弹系统选文件对话框"
        );
        assert!(
            !body.contains("default_ssh_dir") && !body.contains("config_path"),
            "导入入口不该再自己拼默认路径（`~/.ssh/config`）"
        );
    }
}
