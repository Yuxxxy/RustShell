//! 在线更新：检查 → 下载 → 安装 → 重启。
//!
//! # 提示放在哪（与 shell-rs 对齐）
//!
//! - **标题栏右侧一枚纯图标徽标**是唯一的主动提示：有新版本 / 已就绪 / 失败时
//!   才出现，其余时候什么都不显示（不打扰）。图标取 shell-rs 的 `CircleArrowUp`，
//!   版本 / 状态文字只在 hover 提示里。**点它就是「重启更新」**（2026-11-05）：
//!   有新版就后台下载、下完自动重启安装，一步到位。
//! - **设置 › 关于**那一行是常驻状态 + 手动入口（检查更新 / 下载并重启）。
//!
//! 这条分工是抄 shell-rs 的（它的 `UpdateBadge` 只在这三种状态下返回 `Some`，
//! 状态行在「关于」页）。为什么不让它弹模态框：更新不是用户当下要做的事，
//! 弹出来就是打断；而徽标放在标题栏，既不打断也丢不掉。
//! 对话框只显示状态和一个主操作，**不展示 tag 的发布说明**（2026-11-04 用户要求：
//! GitHub 的 Markdown 正文常是平台产物表格，塞进对话框既撑高又读不出重点）。
//!
//! # 三个阶段的分寸
//!
//! - **检查**：启动后延迟一次（[`MainView::update_auto_check`]）+ 关于页手动触发。
//!   自动检查失败**静默**（用户没问，不该被网络错误打扰），手动检查才报错。
//! - **下载**：用户点徽标（或关于页的按钮）那一刻立刻开始，**后台跑**、不阻塞 UI；
//!   用户没点就不下 —— 几十 MB 的流量该由用户决定。
//! - **安装**：Windows 的产物是 **NSIS 安装包**（不是主程序）。**下载完直接重启**，
//!   由 [`rustshell_core::update::launch_windows_installer`] 另起进程拉起覆盖安装，
//!   本程序随即退出 —— 2026-11-05 用户要求「更新提示按钮改为重启更新，后台下载完
//!   自动重启更新，不要做成两步」：不再有「先点下载、再点重启」这种中间态。
//!   **绝不把安装包复制/改名成 `rustshell.exe`** —— 那会让安装器
//!   顶着主程序名字运行，被旧卸载器按进程名当成 RustShell 杀掉（2026-11-04）。
//!   mac / linux 的 `.dmg` / `.deb` 交给系统安装器打开，本程序不碰 `/usr/bin`，
//!   也不自动重启（那两个平台「装没装」由系统那一步决定，重启不解决问题）。

use gpui::{Context, Window};

use rustshell_core::update;

use super::MainView;

/// 自动检查的延迟：等首屏、连接恢复这些更要紧的事先做完。
///
/// 8s → 2s（2026-11-05）：用户反馈「打开软件就该检查更新」。首屏在 300ms 内就画完了，
/// 8 秒纯属白等 —— 那段时间里用户往往已经切去干别的，徽标出现了也没人看见。
/// 检查本身在后台执行器上跑，不占 UI 线程，提前发起不会影响首屏帧率。
const AUTO_CHECK_DELAY: std::time::Duration = std::time::Duration::from_secs(2);

impl MainView {
    /// 启动后自动查一次更新（延迟 [`AUTO_CHECK_DELAY`]，失败静默）。
    ///
    /// **主触发点是应用启动**（`shell/src/app.rs` 里建完 `MainView` 立刻调一次），
    /// 2026-11-05 用户要求：「打开软件就调用更新检查，不是打开设置才查」。挂在启动点
    /// 而不是只挂首帧 `render`，是因为 `render` 每次重绘都会走一遍 —— 把一个网络动作
    /// 放在渲染路径上、只靠一个闩兜着太脆；启动点只有一处，语义也更直白。
    /// 首帧 `render` 里仍留一次调用当兜底（其它入口直接 `cx.new(MainView::new)` 时
    /// 也能查到），闩保证**一次进程只发一次请求**。
    ///
    /// 闩在 `update_auto_started` 上（不能用「阶段是 Idle」判断 —— 自动检查失败后
    /// 阶段本来就回到 Idle，那样每帧都会再发一次请求）。
    pub fn update_auto_check(&mut self, cx: &mut Context<Self>) {
        // 测试里不发真实请求：不少 UI 用例用 `advance_clock` 推进时钟（`dead_session`
        // 那套按 20ms 步进），推过 [`AUTO_CHECK_DELAY`] 就会真去请求 GitHub ——
        // CI 的成败就挂到网络上了。守卫用例只查源码结构（见本文件测试模块）。
        if cfg!(test) {
            return;
        }
        if self.io.update_auto_started {
            return;
        }
        self.io.update_auto_started = true;
        log::info!(
            "启动自动检查更新：{AUTO_CHECK_DELAY:?} 后发起（本机 v{}）",
            rustshell_core::APP_VERSION
        );
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
                        let newer = update::is_newer(&release.version, rustshell_core::APP_VERSION);
                        // 落一条日志：不写的话「启动到底查没查」在日志里完全无从判断
                        // （2026-11-05 用户反馈「只有打开设置才检查」时就是这么被误导的）。
                        log::info!(
                            "检查更新完成：远端 tag=v{}，本机 v{} ⇒ {}",
                            release.version,
                            rustshell_core::APP_VERSION,
                            if newer {
                                "有新版本"
                            } else {
                                "已是最新"
                            }
                        );
                        if newer {
                            m.io.update_release = Some(release);
                            m.io.update_phase = super::UpdatePhase::Available;
                        } else {
                            m.io.update_release = None;
                            m.io.update_phase = super::UpdatePhase::UpToDate;
                        }
                    }
                    Err(e) => {
                        log::warn!("检查更新失败（announce={announce}）：{e}");
                        // 自动检查失败不该给用户看网络错误：他根本没要求查。
                        m.io.update_phase = if announce {
                            super::UpdatePhase::Failed(super::MainView::update_fail_text(&e))
                        } else {
                            super::UpdatePhase::Idle
                        };
                    }
                }
                if announce {
                    // 手动检查 = 用户已经表达了「我要更新」的意图（2026-11-05 用户要求
                    // 「设置中检查更新后，如果有需要更新的直接跳出更新图标触发的内容」）。
                    // 所以查到新版就直接走徽标那条路：后台下载 → 下完自动重启。
                    // 自动检查（announce=false）不这么做 —— 那只是开机时的静默探测，
                    // 用户没表达任何意图，不能替他重启。
                    if matches!(m.io.update_phase, super::UpdatePhase::Available) {
                        log::info!("手动检查发现新版本，直接开始下载（完成后自动重启更新）");
                        m.start_update_download(cx);
                    } else {
                        let (line, trouble) = m.update_status_line();
                        m.set_info(line, !trouble);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn update_fail_text(e: &anyhow::Error) -> String {
        rustshell_core::i18n::ts(format!("更新失败：{e}"), format!("Update failed: {e}"))
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
                    format!("发现新版本 v{version}，点击后自动下载并重启更新"),
                    format!("v{version} available — click to download and restart"),
                ),
                false,
            )),
            super::UpdatePhase::Downloading => Some((
                rustshell_core::i18n::ts(
                    "正在下载更新…（完成后自动重启）".into(),
                    "Downloading… (restarts automatically)".into(),
                ),
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
                if cfg!(windows) {
                    // Windows 的「已就绪」是一瞬间的过渡态：下载完立刻自动重启安装
                    // （2026-11-05「不要做成两步」）。文案写成「文件已换好」那是老实现
                    // 的说法，会让人以为此刻程序目录里已经是新版本。
                    rustshell_core::i18n::ts(
                        format!("v{version} 已下载，正在重启以完成覆盖安装。"),
                        format!("v{version} downloaded — restarting to install."),
                    )
                } else {
                    rustshell_core::i18n::ts(
                        format!("v{version} 已就绪，请按系统安装器提示完成安装。"),
                        format!("v{version} is ready — finish in the system installer."),
                    )
                },
                false,
            ),
            super::UpdatePhase::Failed(why) => (why.clone(), true),
        }
    }

    /// 有可用版本 → 后台下载产物，**下载完直接重启安装**（Windows）。
    ///
    /// 找不到本平台的产物（例如刚出了个只打了 tag 的版本）不报「下载失败」，
    /// 而是打开发布页让用户自己挑 —— 那才是真实原因。
    ///
    /// ★ 不再需要 `Window`：整个流程只动状态与后台任务（原来那个 `window` 参数
    /// 从未被使用，`let _ = window;` 是它唯一的痕迹）。
    pub fn start_update_download(&mut self, cx: &mut Context<Self>) {
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
                        // 只下载，**不安装**。Windows 的 exe 是 NSIS 安装包，绝不能在
                        // 这里把它复制/改名成当前主程序（详见本文件顶部与
                        // core/src/update.rs 的说明）；安装由 `restart_for_update`
                        // 另起进程拉起。
                        update::download(&asset, &dest)?;
                        Ok::<_, anyhow::Error>(())
                    }
                })
                .await;
            let _ = this.update(cx, |m, cx| {
                match result {
                    Ok(()) => {
                        // Windows（`install_here` = 产物是 self-installing 的 `.exe`）：
                        // **下载完立刻重启**，不再停下来等用户点第二次
                        // （2026-11-05 用户要求「后台下载完自动重启更新，不要做成两步」）。
                        //
                        // mac / linux 的 `.dmg` / `.deb` 没有「自己装好」这一步：
                        // 先交给系统安装器打开，状态停在「已就绪」，用户装完自己重开 ——
                        // 在这里替用户重启只会把旧版本重新拉起来。
                        if !install_here {
                            rustshell_platform::shell_open::open_path(&dest);
                        }
                        m.io.update_artifact = Some(dest.clone());
                        m.io.update_phase = super::UpdatePhase::Ready;
                        if install_here {
                            log::info!("更新包下载完成，自动重启以完成安装");
                            m.restart_for_update(cx);
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
    }

    fn fail_update(&mut self, text: String, cx: &mut Context<Self>) {
        self.io.update_phase = super::UpdatePhase::Failed(text.clone());
        self.set_info(text, false);
        cx.notify();
    }

    /// 关于页 / 更新对话框的主操作。**一步到位**：有新版就开始下载（下载完自动重启）。
    pub fn update_primary_action(&mut self, cx: &mut Context<Self>) {
        let phase = self.io.update_phase.clone();
        match phase {
            super::UpdatePhase::Ready => self.restart_for_update(cx),
            super::UpdatePhase::Available => self.start_update_download(cx),
            // 失败过、但还记着那个发布版本：直接重试下载，别让用户先退回「检查」。
            super::UpdatePhase::Failed(_) if self.io.update_release.is_some() => {
                self.io.update_phase = super::UpdatePhase::Available;
                self.start_update_download(cx);
            }
            _ => self.check_for_updates(cx),
        }
    }

    /// 标题栏徽标的点击处理 —— 用户点的就是「重启更新」这几个字。
    ///
    /// 2026-11-05 用户要求「更新提示按钮改为重启更新，后台下载完自动重启更新，
    /// 不要做成两步」：所以这里没有「先弹框、再点立即更新」，
    /// 也没有「先下载、再点重启」——
    /// - 有新版本 → 直接后台下载，下完自动重启安装；
    /// - 已就绪（此前下载过）→ 立刻重启安装；
    /// - 正在下载 / 正在检查 → 什么都不做（这条路上再点没有意义）；
    /// - 失败 → 弹更新框说明原因（唯一还需要人读一眼的状态），框里的按钮重试。
    pub fn on_update_badge_click(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let phase = self.io.update_phase.clone();
        match phase {
            super::UpdatePhase::Available => self.start_update_download(cx),
            super::UpdatePhase::Ready => self.restart_for_update(cx),
            super::UpdatePhase::Downloading | super::UpdatePhase::Checking => {}
            super::UpdatePhase::Failed(_) => self.open_update_dialog(window, cx),
            // 没有待办版本（Idle / UpToDate）：点一下等于「重新查一次」。
            _ => self.check_for_updates(cx),
        }
    }

    /// 重启以完成更新：拉起安装器 / 新进程后退出自己。
    ///
    /// **Windows 走的是安装器，不是 `current_exe()`**：发布产物是 NSIS 安装包，
    /// 留在下载目录以独立进程运行（`/P /R` 覆盖安装 + 装完自动拉起主程序）。
    /// 若这里 `spawn(current_exe())`，而老版本又把安装包写成了 `rustshell.exe`，
    /// 跑起来的就是顶着主程序名字的安装器 —— 旧卸载器按进程名把它当成 RustShell
    /// 杀掉，结果是「又卸载了，又没安装上」。
    ///
    /// 顺序不能反：先 quit 会让新进程可能还没起来，用户看到「程序关了」；
    /// 反过来本进程也要尽快退出，把 `rustshell.exe` 的文件锁让给安装器。
    pub fn restart_for_update(&mut self, cx: &mut Context<Self>) {
        if cfg!(windows) {
            let Some(installer) = self.io.update_artifact.clone() else {
                self.set_info(
                    rustshell_core::i18n::ts(
                        "找不到已下载的安装包，请到发布页手动下载。".into(),
                        "Downloaded installer not found — get it from the release page.".into(),
                    ),
                    false,
                );
                cx.notify();
                return;
            };
            match update::launch_windows_installer(&installer) {
                Ok(()) => {
                    // 拉起成功才退出；失败时留在旧版本，把下载好的安装包路径
                    // 留在对话框里，用户可以自己双击完成更新。
                    cx.quit();
                    return;
                }
                Err(e) => {
                    self.set_info(
                        rustshell_core::i18n::ts(
                            format!("自动更新失败：{e}，请手动运行下载的安装包。"),
                            format!(
                                "Could not start the installer: {e}. Run the downloaded setup manually."
                            ),
                        ),
                        false,
                    );
                    cx.notify();
                }
            }
            return;
        }

        let Ok(exe) = std::env::current_exe() else {
            self.set_info(
                rustshell_core::i18n::ts(
                    "找不到程序路径，无法自动重启".into(),
                    "Cannot locate the executable; restart manually.".into(),
                ),
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
            code.contains("m.on_update_badge_click(w, cx)"),
            "点徽标必须直接进入「重启更新」（2026-11-05：不再先弹对话框）"
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

    /// 每次启动都检查，且每次只发起一次；**主触发点是应用启动**。
    ///
    /// 这里守的是那个**闩**：用「阶段是 Idle」判断会死循环 —— 自动检查失败后阶段
    /// 本来就回到 Idle，于是每帧都发一次请求。反过来，闩只存在进程内存里，
    /// `MainView::new` 每次都把它置回 false，所以**每个新进程都会重新查一次**。
    ///
    /// 2026-11-05 用户要求「打开软件就调用更新检查，不是打开设置才查」：
    /// 主触发点从「首帧 render」前移到 `shell/src/app.rs` 建窗口那一步。
    #[test]
    fn the_startup_check_fires_exactly_once() {
        // ② 主触发点：应用启动、建完 MainView 就调（早于任何 render）。
        let app = prod(include_str!("../../../shell/src/app.rs"));
        let boot = app
            .find("update_auto_check(cx)")
            .expect("应用启动时必须发起一次更新检查（2026-11-05 用户要求）");
        let shell = app
            .find("AppShell::new(view)")
            .expect("找不到外壳视图装配点");
        assert!(
            boot < shell,
            "更新检查要在建完 MainView 后立刻发起，不能等到别的时机"
        );

        // ① 兜底：首帧 render 也调一次（其它入口直接 `cx.new(MainView::new)` 时仍能查到）。
        let main = prod(include_str!("../main_view.rs"));
        assert!(
            main.contains("self.update_auto_check(cx)"),
            "首帧要留一次兜底调用"
        );
        assert!(
            main.contains("update_auto_started: false"),
            "闩不落盘：MainView 初始化必须是 false，否则下次启动就不检查了"
        );
        let actions = prod(include_str!("update_actions.rs"));
        assert!(
            actions.contains("if self.io.update_auto_started {"),
            "自动检查必须有「只发起一次」的闩（两处调用也不能发两次请求）"
        );
        assert!(
            actions.contains("background_executor().timer(AUTO_CHECK_DELAY)"),
            "自动检查要延迟：别和首屏抢网络"
        );
        // 延迟不许回到「等很久」：用户要的是打开软件就能看到有没有新版本。
        assert!(
            actions.contains("Duration::from_secs(2)"),
            "自动检查最多等 2 秒（曾经是 8 秒，用户根本等不到徽标）"
        );
        // 自动检查失败要静默（`announce == false`），手动检查才报错
        assert!(
            actions.contains("run_update_check(false, cx)"),
            "自动检查失败不该弹错误给用户（他没要求查）"
        );
        // 必须落日志：不然「启动到底查没查」在日志里完全看不出来。
        assert!(
            actions.contains("检查更新完成：远端 tag=v{}"),
            "检查结果要落日志，否则无法自证「启动就查了」"
        );
        assert!(actions.contains("启动自动检查更新"), "发起时也要落一条日志");
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

    /// 2026-11-05 用户要求「更新提示按钮改为重启更新，后台下载完自动重启更新，
    /// 不要做成两步」「设置中检查更新后，如果有需要更新的直接跳出更新图标触发的内容」。
    ///
    /// 三条一起守，少任何一条都会退回「两步」：
    /// ① 徽标点击 → 有新版就直接开始下载（不再先弹框）；
    /// ② 下载成功 → **自动** `restart_for_update`（Windows 上就是拉起 NSIS 覆盖安装）；
    /// ③ 手动检查（设置 › 关于那枚按钮）查到新版 → 自己开始下载，不留给人点第二次。
    ///
    /// 自动检查（`announce == false`，开机时那一次）**不许**走 ③ —— 用户没表达
    /// 「我要更新」，替他把机器重启了是另一种打扰。
    #[test]
    fn the_update_is_one_step_from_badge_to_restart() {
        let actions = prod(include_str!("update_actions.rs"));

        // ① 徽标点击：Available → 下载；Ready → 重启；Failed → 弹框看原因。
        let at = actions
            .find("pub fn on_update_badge_click")
            .expect("找不到徽标点击入口（标题栏那枚图标点了没反应）");
        let tail = &actions[at..];
        let body = &tail[..tail
            .find("pub fn open_update_dialog")
            .expect("找不到徽标入口的结束锚点")];
        for needle in [
            "super::UpdatePhase::Available => self.start_update_download(cx)",
            "super::UpdatePhase::Ready => self.restart_for_update(cx)",
            "super::UpdatePhase::Failed(_) => self.open_update_dialog(window, cx)",
        ] {
            assert!(
                body.contains(needle),
                "徽标入口缺一条分支 `{needle}` —— 点图标必须一步进入重启更新"
            );
        }

        // ② 下载完成即重启（只在产物是「自己会装」的 Windows exe 上）。
        let at = actions
            .find("pub fn start_update_download")
            .expect("找不到下载入口");
        let tail = &actions[at..];
        let body = &tail[..tail
            .find("fn fail_update")
            .expect("找不到下载入口的结束锚点")];
        assert!(
            body.contains("if install_here {") && body.contains("m.restart_for_update(cx)"),
            "下载完成必须自动重启安装（2026-11-05：不要做成两步）"
        );

        // ③ 手动检查发现新版 → 直接开始下载。
        assert!(
            actions.contains("手动检查发现新版本，直接开始下载"),
            "手动检查发现新版后要直接开始下载（日志是唯一的可观测证据）"
        );
        let at = actions.find("if announce {").expect("找不到手动检查分支");
        let tail = &actions[at..];
        let body = &tail[..tail
            .find("cx.notify();")
            .expect("找不到手动检查分支的结束锚点")];
        assert!(
            body.contains("m.start_update_download(cx)"),
            "设置里检查到新版必须直接开下（不留给用户点第二次）"
        );
        assert!(
            body.contains("matches!(m.io.update_phase, super::UpdatePhase::Available)"),
            "只有「有新版本」才自动下载 —— UpToDate / 失败 都不该触发"
        );
    }

    /// 2026-11-04 事故护栏：升级必须是**覆盖安装**，安装包不许变成主程序。
    ///
    /// 三条一起守：packager 用仓库里改过的 NSIS 模板；模板里没有「检测到旧版本 →
    /// 默认先卸载」那一页；客户端重启时拉起的是独立安装器而不是 `current_exe()`。
    /// 少任何一条，症状都是「旧版卸了、新版没装上」，而编译/单测全绿。
    #[test]
    fn windows_upgrade_is_an_overwrite_install() {
        let cfg = include_str!("../../../app/Cargo.toml");
        assert!(
            cfg.contains("template = \"assets/installer.nsi\""),
            "packager 必须指向仓库里 vendor 的 NSIS 模板，否则会退回上游「先卸载」行为"
        );

        // 模板里的注释会复述旧行为，断言只针对真正的 NSIS 代码行。
        let nsi_code = include_str!("../../../app/assets/installer.nsi")
            .lines()
            .filter(|line| !line.trim_start().starts_with(';'))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !nsi_code.contains("Page custom PageReinstall"),
            "不许再弹「先卸载再安装」的重装页"
        );
        assert!(
            !nsi_code.contains("ExecWait '$R1 /P _?=$4'"),
            "安装流程不许 ExecWait 旧卸载器（它会按进程名杀掉安装程序）"
        );
        assert!(
            nsi_code.contains("SetOverwrite on"),
            "升级路径必须显式覆盖文件"
        );
        assert!(
            nsi_code.contains("installer.old"),
            "老版本在线更新的残留（安装器顶着 rustshell.exe 运行）要能自愈"
        );
        // 2026-11-05 事故：安装器**自己**正顶着 rustshell.exe 运行时，Windows 锁住
        // 这个文件 ⇒ 写不进去、也改不动（实测 Rename 返回 FAIL），安装必然报
        // 「无法打开要写入的文件」。修法是两段合起来：
        //   A. .onInit 里把自身复制到临时目录、另起进程装、自己退出；
        //   B. Section Install 里写文件前先把目标改名让位（改不动就等一下重试）。
        assert!(
            nsi_code.contains("CopyFiles \"$EXEPATH\" \"$TEMP\\"),
            "安装器顶着主程序名字运行时，必须复制一份到临时目录另起进程安装"
        );
        assert!(
            nsi_code.contains("ExecShell \"open\""),
            "另起进程必须用 ExecShell（Exec/nsExec 会等子进程结束，和「本进程退出才能解锁」互相死等）"
        );
        assert!(
            nsi_code.contains("Rename \"$INSTDIR\\${MAINBINARYNAME}.exe\""),
            "写文件前必须先把目标改名让位，否则运行中的映像锁写"
        );
        assert!(
            nsi_code.contains("Sleep 500") && nsi_code.contains("make_room"),
            "改不动目标时要等一下重试（旧程序可能还没退干净）"
        );
        // 顺序：先杀进程、再让位改名、最后才写文件。
        let kill_at = nsi_code
            .find("!insertmacro CheckIfAppIsRunning")
            .expect("安装流程里没有「先关掉正在运行的程序」这一步");
        let room_at = nsi_code
            .find("make_room:")
            .expect("安装流程里没有「改名让位」这一步");
        assert!(
            kill_at < room_at,
            "必须先杀掉正在运行的程序，再改名让位 —— 反过来的话，程序已经被改名挪走（映像变成 .installer.old），再按进程名找就不一定找得到它，老版本会继续在跑，用户以为没升级"
        );
        assert!(
            nsi_code.contains("IfFileExists \"$INSTDIR\\${MAINBINARYNAME}.exe\""),
            "首次安装（目标文件压根不存在）要直接跳过让位循环，别白等 20 秒"
        );
        assert!(
            nsi_code.contains("IfErrors self_heal_copy_failed"),
            "复制不出副本时要退回正常安装流程，不能静默退出（用户会看到「什么都没发生」）"
        );

        let actions = prod(include_str!("update_actions.rs"));
        assert!(
            !actions.contains("replace_running_exe"),
            "不许把下载到的 NSIS 安装包复制/改名成当前主程序"
        );
        assert!(
            actions.contains("update::launch_windows_installer(&installer)"),
            "点重启时必须以独立进程拉起安装器"
        );
        // 只看 restart_for_update 的 Windows 分支（截到非 Windows 的重启逻辑之前），
        // 确保「拉起即退出」。不能直接全文找 cfg!(windows)：状态文案里也有一个。
        let at = actions
            .find("pub fn restart_for_update")
            .expect("找不到重启入口");
        let restart = &actions[at..];
        let win = &restart[..restart.find("let Ok(exe)").expect("找不到分支结尾")];
        assert!(
            win.contains("if cfg!(windows)") && win.contains("cx.quit()"),
            "拉起安装器后必须退出，把 rustshell.exe 的文件锁让出来"
        );
    }
}
