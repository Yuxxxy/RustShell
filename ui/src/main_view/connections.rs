//! 会话编排域：本地 Shell / SSH 连接生命周期、Tab 增删与激活、指标源路由。 本模块为`main_view`的子模块，可直接访问`MainView`的私有字段。

use gpui::{Context, Entity, SharedString, Window};
use gpui_component::{button::ButtonVariant, dialog::DialogButtonProps, WindowExt as _};

use rustshell_core::connections::ConnectionProfile;
use rustshell_core::metrics::MetricsStore;
use rustshell_core::session::{SessionHandle, SessionId, SessionKind, SessionState};
use rustshell_core::ssh::{connect_in_background, SshChannels};

use rustshell_platform::paths;

use crate::terminal_view::{create_terminal_view, TerminalPaneConfig};

use super::{connect_results, ChildView, DialogMode, MainView, RetryLedger, TabEntry, ViewDirty};

/// 「新建连接」表单里主机的默认值（2026-10-21 用户要求）。
///
/// 只在**新建**时填：编辑既有连接一律回填它自己的主机，绝不用这个值兜底 ——
/// 那会把「主机没填」这种错误悄悄变成「连到 localhost」。
const DEFAULT_HOST: &str = "localhost";

impl MainView {
    // 会话编排

    /// 打开本地 Shell。ConPTY spawn + PowerShell 冷启动在本机可能耗时数十秒 （实测 ~50s，杀软实时扫描为主因），必须在后台执行， 否则 UI 线程被阻塞表现为「点不动 / 打不开 / 界面卡死」。
    pub fn open_local_shell(&mut self, cx: &mut Context<Self>) {
        self.spawn_local_shell_into(None, cx);
    }

    /// 起一只本地 Shell：`into = None` 新开标签，`Some(id)` **就地替换**那个槽位。
    ///
    /// 「就地替换」这条路径是 F-9 会话恢复要的：快照里的本地标签先以占位形态
    /// 站在正确的位置上（见 `layout_begin_restore`），等 ConPTY 起来后再换成真会话
    /// —— 池顺序因此完全不变。反过来「等回来再插到快照下标」的话，窗口期里
    /// 标签栏就是错的，而用户此刻正看着它（冷启动以十秒计）。
    ///
    /// 返回 `false` = 这次没有真的发起：`local_starting` 挡住了（已有一次在跑），
    /// 或目标槽位已不在池里（用户把那个占位关掉了 —— 那样绝不能凭空把标签造回来）。
    pub(super) fn spawn_local_shell_into(
        &mut self,
        into: Option<SessionId>,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.conn_ui.local_starting {
            return false; // 启动中重复点击直接忽略
        }
        // 落点先查、再 spawn：查不到的必须**取消本次启动**，否则用户刚关掉的
        // 占位会以新标签的形式在池尾复活。
        if let Some(id) = into.as_deref() {
            if !self.tabs_state.tabs.iter().any(|t| t.session.id == id) {
                log::warn!("本地 Shell 落点已不在标签池里，取消本次启动: {id}");
                return false;
            }
        }
        // 用户已经着手补救（不管这次是不是重复点击），主窗口正中那条
        // 「没有可用会话」卡片就该收走，否则它会在终端起来后继续盖在画面上。
        self.ai_ui.quick_no_session_alert = false;
        self.conn_ui.local_starting = true;
        self.conn_ui.connect_notice =
            Some(rustshell_core::i18n::t("正在启动本地 Shell…").to_string());
        log::info!("请求启动本地 Shell（后台执行）");
        // 会话日志落点（F-7）在**同步段**算好再 move 进异步块：
        // 异步块里拿不到 `self`（settings 已被借走），而且路径一旦算晚，
        // 跨日那一刻会出现「文件在 A 目录、日志说的是 B 目录」。
        let log_path = self.session_log_target(None);
        cx.notify();

        cx.spawn(async move |this, cx| {
            let t0 = std::time::Instant::now();
            let res = cx
                .background_executor()
                .spawn(
                    async move { rustshell_core::session::spawn_local_session(80, 24, log_path) },
                )
                .await;
            log::info!(
                "本地 Shell spawn 耗时: {:?}（含 ConPTY 创建与进程启动）",
                t0.elapsed()
            );
            let _ = this.update(cx, move |m, cx| {
                m.conn_ui.local_starting = false;
                m.conn_ui.connect_notice = None;
                match res {
                    Ok(session) => m.local_shell_placed(into, session, cx),
                    Err(e) => m.local_shell_failed(into, e, cx),
                }
                cx.notify();
            });
        })
        .detach();
        true
    }

    /// 本地 Shell 起来了：新开标签，或就地替换指定槽位（F-9 恢复）。
    fn local_shell_placed(
        &mut self,
        into: Option<SessionId>,
        mut session: SessionHandle,
        cx: &mut Context<Self>,
    ) {
        let id = session.id.clone();
        log::info!("本地 Shell 已启动: tab={id} title={}", session.title);
        let Some(target) = into else {
            let terminal = self.mount_terminal(&mut session, cx);
            self.tabs_state.tabs.push(TabEntry {
                session,
                terminal: Some(terminal.0),
                scroll: Some(terminal.1),
                connect_error: None,
                connect_started: None,
                dead: None,
                retry: None,
            });
            self.tabs_state.active = Some(id.clone());
            self.tabs_state.pending_focus = true;
            // 分屏待建立标记（若存在）：本地 Shell 的创建是异步的，
            // 标记落在这里收口，新标签进副窗格。
            self.maybe_establish_split(&id, cx);
            self.layout_local_settled(cx);
            return;
        };
        let Some(ix) = self
            .tabs_state
            .tabs
            .iter()
            .position(|t| t.session.id == target)
        else {
            // spawn 期间占位被关掉了：这只 Shell 无处安放。直接丢弃 ——
            // `SessionHandle` 的 Drop 会终止子进程，不会留孤儿 PowerShell。
            log::warn!("本地 Shell 启动完成，但落点标签已关闭，丢弃该会话: {target}");
            self.layout_local_settled(cx);
            return;
        };
        let terminal = self.mount_terminal(&mut session, cx);
        self.tabs_state.tabs[ix].terminal = Some(terminal.0);
        self.tabs_state.tabs[ix].scroll = Some(terminal.1);
        self.tabs_state.tabs[ix].session = session;
        self.tabs_state.tabs[ix].connect_error = None;
        self.tabs_state.tabs[ix].connect_started = None;
        self.tabs_state.tabs[ix].dead = None;
        // 会话 id 换了（新会话由 core 生成），分屏槽位存的是旧 id
        self.remap_split_id(&target, &id);
        if self.tabs_state.active.as_deref() == Some(target.as_str()) {
            self.tabs_state.active = Some(id);
        }
        self.tabs_state.pending_focus = true;
        self.layout_local_settled(cx);
    }

    /// 本地 Shell 没起来。两条路径的落点不同（F-9）：
    /// - 新开标签（`into = None`）：全局错误条 —— 原行为；
    /// - 恢复槽位（`into = Some`）：该槽位退回**「已断开」占位** + 全局错误条，
    ///   其余槽位照常（规格 §1.9 边界态）。写成 `connect_error` 会把它变成
    ///   「连接失败」红卡，而这是本地终端：用户要的是「右键标签重开一个」。
    fn local_shell_failed(
        &mut self,
        into: Option<SessionId>,
        e: impl std::fmt::Display,
        cx: &mut Context<Self>,
    ) {
        log::error!("本地 Shell 启动失败: {e}");
        let reason = rustshell_core::i18n::t("本地 Shell 启动失败").to_string() + &format!(": {e}");
        match into {
            None => self.conn_ui.connect_error = Some(reason),
            Some(target) => {
                if let Some(tab) = self
                    .tabs_state
                    .tabs
                    .iter_mut()
                    .find(|t| t.session.id == target)
                {
                    tab.session.state = SessionState::Closed;
                    tab.connect_started = None;
                    tab.connect_error = None;
                }
                self.conn_ui.connect_error = Some(reason);
            }
        }
        self.layout_local_settled(cx);
    }

    /// 重开一个本地终端标签（标签右键「连接」）。
    ///
    /// 本地 Shell 没有连接参数，「连接」在这里的意思就是「照原样再起一只」，
    /// 落在原槽位上（状态、标题、池顺序全不变）。
    pub fn restart_local_tab(&mut self, id: &str, cx: &mut Context<Self>) {
        if !self.tabs_state.tabs.iter().any(|t| t.session.id == id) {
            return;
        }
        if self.conn_ui.local_starting {
            // 已有一次在跑：直接忽略等于「点了没反应」，给一条过程提示
            self.conn_ui.connect_notice =
                Some(rustshell_core::i18n::t("正在启动本地 Shell…").to_string());
            cx.notify();
            return;
        }
        self.spawn_local_shell_into(Some(id.to_string()), cx);
    }

    /// 会话 id 变了（就地替换 / 原地重连）：分屏槽位存的是旧 id，不跟着重映射的话
    /// 「重连成功」会凭空散掉一屏 —— `normalize_split` 只会把「已不存在的 id」修掉，
    /// 修的方式是**取消分屏**。
    pub(super) fn remap_split_id(&mut self, old: &str, new: &str) {
        if old == new {
            return;
        }
        if let Some(sp) = self.tabs_state.split.as_mut() {
            if sp.primary == old {
                sp.primary = new.to_string();
            }
            if sp.secondary == old {
                sp.secondary = new.to_string();
            }
        }
    }

    pub fn connect_profile(&mut self, profile_id: &str, w: &mut Window, cx: &mut Context<Self>) {
        let Some(p) = self.lookup_profile(profile_id) else {
            return;
        };
        log::info!(
            "连接请求: {} ({}@{}:{})",
            p.name,
            p.username,
            p.host,
            p.port
        );
        if p.password.is_none() && p.key_path.is_none() {
            // 需要凭据：打开连接对话框（编辑模式 + 连接意图）
            self.conn_ui.selected_profile = Some(p.id.clone());
            self.open_connect_dialog_for_profile(&p, w, cx);
            // 意图必须在**打开之后**置位：打开动作自己会把它复位（那条路径也被
            // 「编辑」入口复用，而编辑不该顺带连接）。
            self.io.dlg_connect_after = true;
            return;
        }
        self.connect_profile_now(p, cx);
    }

    /// 无需`Window`的连接路径（供`poll`里的自动重连复用）。 先开 Tab，再连接： 连接中/失败/超时都有落点，用户不会「点了没反应」。
    ///
    /// 这里是「最近访问」的唯一写入点：命令窗口、连接面板、右键菜单、分屏、
    /// 自动重连全部经由本函数发起连接，不在这里记账就得在每个入口各写一遍。
    /// 同理，会话日志的落点也只在这里算一次（见 [`Self::session_log_target`]）。
    pub(super) fn connect_profile_now(&mut self, p: ConnectionProfile, cx: &mut Context<Self>) {
        // ★ C-4（2026-09-28）：**在途连接守卫** —— 同一个 profile 只允许一条连接线程。
        //
        // `connect_in_background` 返回的 `JoinHandle` 原先被直接丢弃（未绑定），
        // 每次调用都新起一条线程 + 一组 tokio 泵。而重连是**可以叠加**的：
        // 自动重连（`poll` 里的退避）与用户手动点「连接」可能同时发起；断连现场
        // 反复点重连也会连发。旧线程若因服务端半死长期挂在 `block_on` 里
        // （信任等待可顺延至 180s），就会与新线程并存 —— 两条线程都连上时产生
        // **孤儿会话**（连接占着资源却没有 Tab 指向它）。
        //
        // `connecting_profiles` 本来就是「哪些 profile 正在连」的账本，只是此前
        // 只用来渲染「连接中」提示。这里把它升级成真正的**互斥锁**：
        // `insert` 返回 `false` 说明已有在途连接，本次请求直接并到那一条上，
        // 不再另起线程（重复发起对用户表现为「点了没反应」，但比多一条线程安全）。
        if !self.conn_ui.connecting_profiles.insert(p.id.clone()) {
            log::info!(
                "忽略重复的连接请求（已有在途连接线程）: profile={} {}@{}:{}",
                p.name,
                p.username,
                p.host,
                p.port
            );
            return;
        }
        // 记账必须在 `open_connecting_tab` 之前：占位 Tab 的标题取自 p.name，
        // 与记账无关，但改完 `last_used_at` 后紧接一次落盘，若中间再 return 就漏记。
        if self
            .conn_store
            .touch_profile(&p.id, rustshell_core::connections::unix_now_secs())
        {
            // 落了盘才能让「最近访问」跨重启保留（FinalShell 手感）
            let _ = self.conn_store.save(&paths::connections_path());
        }
        let tab_id = self.open_connecting_tab(&p, cx);
        log::info!("占位 Tab 已打开: tab={tab_id}，开始后台连接");
        let timeout = std::time::Duration::from_secs(self.settings.connect_timeout_secs.clamp(
            rustshell_core::ssh::CONNECT_TIMEOUT_MIN_SECS,
            rustshell_core::ssh::CONNECT_TIMEOUT_MAX_SECS,
        ));
        // 外部链接（`ext:`）：单通道 + 不写会话日志。判定放在这里而不是各入口：
        // 「连接对话框补完凭据后再连」也必须守住同一条纪律，而它同样走本函数。
        let ephemeral = self.conn_ui.external.get(&p.id);
        let channels = match ephemeral.map(|e| e.kind) {
            Some(rustshell_core::launch::LaunchKind::Sftp) => SshChannels::SftpOnly,
            Some(rustshell_core::launch::LaunchKind::Ssh) => SshChannels::ShellOnly,
            None => SshChannels::Full,
        };
        let log_path = match ephemeral {
            Some(_) => None,
            None => self.session_log_target(Some(&p)),
        };
        connect_in_background(
            p,
            timeout,
            log_path,
            self.settings.clamped_sftp_max_concurrent(),
            self.runtime_global_proxy(),
            channels,
            connect_results().0.clone(),
        );
        cx.notify();
    }

    /// 本会话的日志落点（F-7）。`None` = 不记录；`profile = None` = 本地 Shell。
    ///
    /// 两件事收在一处：
    /// - **开不开**：交给 core 的纯函数 `session_log::enabled_for`
    ///   （连接级三态覆盖 > 全局默认）。每个建会话的入口共用同一份判定，
    ///   自动重连与原地重连也不必重新想一遍 —— 散着写早晚会出现
    ///   「重连之后就不记了」这种只在特定路径下复现的差异。
    /// - **写哪儿**：`platform::paths` 一次算好日期目录与时间前缀（同一个 `now`），
    ///   文件名的会话名一段由那边净化（Windows 非法字符、保留设备名都会咬人）。
    ///
    /// 关掉开关时**连路径都不算**：`session_log_path_now` 要读系统时间，
    /// 不该在每次开终端时白白做一次。
    pub(super) fn session_log_target(
        &self,
        profile: Option<&ConnectionProfile>,
    ) -> Option<std::path::PathBuf> {
        if !rustshell_core::session_log::enabled_for(profile, self.settings.session_log_enabled) {
            return None;
        }
        let title = match profile {
            Some(p) => p.name.clone(),
            None => rustshell_core::session::local_session_title(),
        };
        Some(paths::session_log_path_now(&title))
    }

    /// 「先开 Tab 再连接」：为 profile 立刻创建/复用一个`Connecting`占位 Tab。 若该 profile 已有一个仍在连接中的占位 Tab，则复用它（避免连续点击 开出多个 Tab），否则新建。返回 Tab 的 session id。
    pub(super) fn open_connecting_tab(
        &mut self,
        p: &ConnectionProfile,
        _cx: &mut Context<Self>,
    ) -> SessionId {
        if let Some(tab) =
            self.tabs_state.tabs.iter_mut().find(|t| {
                t.is_connecting() && t.session.profile_id.as_deref() == Some(p.id.as_str())
            })
        {
            tab.connect_started = Some(std::time::Instant::now());
            let id = tab.session.id.clone();
            self.tabs_state.active = Some(id.clone());
            return id;
        }
        let id = rustshell_core::connections::uuid_like();
        let session = SessionHandle::pending(
            id.clone(),
            SessionKind::Ssh,
            p.name.clone(),
            Some(p.id.clone()),
        );
        self.tabs_state.tabs.push(TabEntry {
            session,
            terminal: None,
            scroll: None,
            connect_error: None,
            connect_started: Some(std::time::Instant::now()),
            dead: None,
            retry: None,
        });
        self.tabs_state.active = Some(id.clone());
        self.tabs_state.pending_focus = true;
        id
    }

    pub fn remove_profile(&mut self, profile_id: &str, _w: &mut Window, cx: &mut Context<Self>) {
        self.conn_store.remove_profile(profile_id);
        let _ = self.conn_store.save(&paths::connections_path());
        cx.notify();
    }

    /// 复制连接：副本名称 = 原名 +`copy`，其余参数不变并紧跟原条目。 复制后选中副本，方便用户立即改名或连接。
    pub fn duplicate_connection(
        &mut self,
        profile_id: &str,
        _w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(new_id) = self.conn_store.duplicate_profile(profile_id) else {
            log::warn!("复制连接失败：找不到 profile_id={profile_id}");
            return;
        };
        let _ = self.conn_store.save(&paths::connections_path());
        self.conn_ui.selected_profile = Some(new_id);
        cx.notify();
    }

    // 重命名连接（右键菜单 / F2 共用入口）

    /// 打开「重命名连接」对话框：与「重命名分组」共用同一套正文视图
    /// （`GroupNameDialogView`，输入框归它自持），只换标题与落库目标。
    ///
    /// 2026-09-17 运行反馈「连接支持右键重命名和 F2 快捷键重命名」。
    /// 改名只动 `name`：`id` 不变，因此已打开的 Tab、指标来源、SFTP 会话引用全部照旧，
    /// 标签上显示的标题会在下一次 `poll` 时同步过去（见 `rename_connection_save`）。
    pub fn open_rename_connection_dialog(
        &mut self,
        profile_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(name) = self.conn_store.profile(profile_id).map(|p| p.name.clone()) else {
            log::warn!("重命名连接失败：连接已不存在 id={profile_id}");
            return;
        };
        self.io.group_name_body.update(cx, |v, cx| {
            v.set_label(rustshell_core::i18n::t("连接名称"));
            v.set_value(name, window, cx);
            v.open(cx);
        });
        self.io.dlg_error = None;
        self.conn_ui.selected_profile = Some(profile_id.to_string());
        self.present_dialog(DialogMode::RenameConn(profile_id.to_string()), window, cx);
    }

    /// 快捷键入口（F2）：重命名连接树里选中的连接；当前面板不是连接树时先切过去，
    /// 没有选中项时给出**窗口级可见提示**而不是静默无反应。
    ///
    /// 提示必须走 `connect_notice`（顶部悬浮条）而不是 `set_error`：后者写的是设置
    /// 面板的提示位，设置面板没打开时用户什么都看不到，表现就是「按 F2 没反应」。
    pub fn rename_selected_connection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !matches!(self.panel_ui.panel, super::PanelKind::Connections) {
            self.panel_ui.panel = super::PanelKind::Connections;
            self.panel_ui.sidebar_collapsed = false;
        }
        let Some(id) = self.conn_ui.selected_profile.clone() else {
            self.conn_ui.connect_notice =
                Some(rustshell_core::i18n::t("请先在连接树中选中一个连接").to_string());
            cx.notify();
            return;
        };
        if self.conn_store.profile(&id).is_none() {
            self.conn_ui.selected_profile = None;
            self.conn_ui.connect_notice =
                Some(rustshell_core::i18n::t("选中的连接已不存在").to_string());
            cx.notify();
            return;
        }
        // 开框前清掉上一轮的提示，免得旧提示压在新对话框上
        self.conn_ui.connect_notice = None;
        self.open_rename_connection_dialog(&id, window, cx);
    }

    /// 落库连接名：非空 / 不与该分组内其他连接重名才写入，失败保持对话框打开。
    ///
    /// 已打开的 Tab 标题同步改名：否则用户改完名字，标签栏还挂着旧标题，
    /// 会以为「没改成功」。
    pub(super) fn rename_connection_save(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        let name = self.io.group_name_body.read(cx).value(cx);
        let Some(DialogMode::RenameConn(id)) = self.dialog.clone() else {
            return;
        };
        if name.is_empty() {
            self.io.dlg_error = Some(rustshell_core::i18n::t("连接名称不能为空").to_string());
            cx.notify();
            return;
        }
        let Some(profile) = self.conn_store.profile(&id) else {
            self.io.dlg_error = Some(rustshell_core::i18n::t("该连接已不存在").to_string());
            cx.notify();
            return;
        };
        // 重名判定排除自己（改名成原名不该被自己挡掉），范围限定同一分组：
        // 树上同一层出现两个同名连接才是真正看不懂的情况。
        let group = profile.group_id.clone();
        let dup = self
            .conn_store
            .profiles
            .iter()
            .any(|p| p.id != id && p.name == name && p.group_id == group);
        if dup {
            self.io.dlg_error = Some(rustshell_core::i18n::t("同名连接已存在").to_string());
            cx.notify();
            return;
        }
        if !self.conn_store.rename_profile(&id, &name) {
            self.io.dlg_error = Some(rustshell_core::i18n::t("该连接已不存在").to_string());
            cx.notify();
            return;
        }
        if let Err(e) = self.conn_store.save(&paths::connections_path()) {
            log::warn!("连接改名落盘失败: {e}");
        }
        // 已打开的 Tab 同步标题（会话按 id 关联，改的只是显示名）
        for tab in self.tabs_state.tabs.iter_mut() {
            if tab.session.profile_id.as_deref() == Some(id.as_str()) {
                tab.session.title = name.clone();
            }
        }
        self.close_dialog_ui(w, cx);
    }

    /// 连接树拖拽落点：把连接移动到目标分组 **id**（`None` = 移出到「未分组」）。
    /// 目标 id 必须已在库中登记，core 侧对未知 id 直接拒绝（不产生孤儿分组），
    /// 故这里不再需要「按名兜底建组」那一步。
    pub fn move_connection_to_group(
        &mut self,
        profile_id: &str,
        group_id: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        if self.conn_store.move_profile_to_group(profile_id, group_id) {
            let _ = self.conn_store.save(&paths::connections_path());
            self.conn_ui.selected_profile = Some(profile_id.to_string());
            cx.notify();
        }
    }

    /// 连接树右键「上移 / 下移」：连接在**同一分组内**挪一格（`delta` = -1 上移、+1 下移）。
    ///
    /// 与 `move_connection_to_group`（拖拽换组）是两件事：这里只改次序，分组归属不动。
    /// 越界（已在首 / 末位）时菜单项本就置灰，core 侧再挡一次，故这里无事可做也不必提示。
    pub fn move_connection_step(&mut self, profile_id: &str, delta: i32, cx: &mut Context<Self>) {
        if self.conn_store.move_profile_step(profile_id, delta) {
            let _ = self.conn_store.save(&paths::connections_path());
            // 排序后选中它：树按新次序重建，选中态要跟着走到新位置，否则用户看不出动的是哪条
            self.conn_ui.selected_profile = Some(profile_id.to_string());
            cx.notify();
        }
    }

    /// 连接树右键「上移 / 下移」：分组在**同一层兄弟之间**挪一格（整棵子树一起搬）。
    ///
    /// 与 `move_group_under`（拖拽换父）是两件事：这里只与同级兄弟换位，不改 `parent`，
    /// 所以不存在成环风险，也不需要环校验。
    pub fn move_group_step(&mut self, group_id: &str, delta: i32, cx: &mut Context<Self>) {
        if self.conn_store.move_group_step(group_id, delta) {
            let _ = self.conn_store.save(&paths::connections_path());
            cx.notify();
        }
    }

    /// 让连接树**下一帧重建一次**：清掉库内部按下标记的选中态，并按连接 id
    /// 重新对齐真正的选中行（对齐发生在 `connections_panel::render` 的重建分支里）。
    ///
    /// 调用点：展开/折叠事件——`toggle_expand` 会整体位移条目下标，不重新对齐，
    /// 库的选中框就会停在**别的连接**上。
    pub fn invalidate_conn_tree(&mut self, cx: &mut Context<Self>) {
        self.conn_ui.conn_tree_key = None;
        cx.notify();
    }

    pub fn edit_connection(&mut self, profile_id: &str, w: &mut Window, cx: &mut Context<Self>) {
        if let Some(p) = self.conn_store.profile(profile_id).cloned() {
            self.open_connect_dialog_for_profile(&p, w, cx);
        }
    }

    pub(super) fn open_connect_dialog_for_profile(
        &mut self,
        p: &ConnectionProfile,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = p.name.clone();
        let host = p.host.clone();
        let port = p.port.to_string();
        let user = p.username.clone();
        let pass = p.password.clone().unwrap_or_default();
        let notes = p.notes.clone();
        // 私钥路径回填：明文，直接进输入框。空 = 这条连接不用私钥认证。
        let key_path = p.key_path.clone().unwrap_or_default();
        // 认证方式回填（2026-10-21）：由已落盘的凭据反推（见 `AuthMode::of_profile`），
        // 否则编辑一条私钥连接时对话框会显示「密码认证」，一保存就把钥匙路径抹掉。
        let auth = super::AuthMode::of_profile(p);
        self.io
            .dlg_name
            .update(cx, |s, cx| s.set_value(name, w, cx));
        self.io
            .dlg_host
            .update(cx, |s, cx| s.set_value(host, w, cx));
        self.io
            .dlg_port
            .update(cx, |s, cx| s.set_value(port, w, cx));
        self.io
            .dlg_user
            .update(cx, |s, cx| s.set_value(user, w, cx));
        self.io
            .dlg_pass
            .update(cx, |s, cx| s.set_value(pass, w, cx));
        self.io
            .dlg_key_path
            .update(cx, |s, cx| s.set_value(key_path, w, cx));
        self.io
            .dlg_notes
            .update(cx, |s, cx| s.set_value(notes, w, cx));
        // 认证方式：与上面那批同一个节奏 —— 不回填，编辑一条私钥连接就会
        // 默认成「密码认证」，保存时把 `key_path` 清掉。
        self.io.dlg_auth = auth;
        // 端口转发规则（F-3）：编辑时把既有规则**克隆**进草稿。
        // 克隆而非引用：对话框里改的是草稿，点「取消」不该动已落盘的那一份。
        // 「高级」界面已隐藏（2026-09-29，用户要求保持简单），这里照旧回填 ——
        // 保存时原样写回，界面消失不等于把用户配过的隧道清掉。
        self.io.dlg_forwards = p.forwards.clone();
        // 会话日志覆盖（F-7）：原样回填三态（`None` = 跟随全局）
        self.io.dlg_session_log = p.session_log;
        // 断线自动重连覆盖（F-4）同理：三态原样回填
        self.io.dlg_auto_reconnect = p.auto_reconnect;
        // 代理（F-10）：档位 + 自定义那套配置。口令回填**明文**（`p.proxy` 在启动时
        // 已由 `restore_passwords` 解出），与连接口令同一个口径；`dlg_proxy_prev`
        // 留一份原配置，「留空 = 不改口令」时要靠它把旧密文/旧明文写回去。
        self.io.dlg_proxy_mode = p.proxy.mode();
        let (prev, custom) = match &p.proxy {
            rustshell_core::proxy::ProxyChoice::Custom(c) => (Some(c.clone()), c.clone()),
            _ => (None, rustshell_core::proxy::ProxyConfig::default()),
        };
        self.io.dlg_proxy_kind = custom.kind;
        self.io
            .dlg_proxy_host
            .update(cx, |s, cx| s.set_value(custom.host.clone(), w, cx));
        self.io
            .dlg_proxy_port
            .update(cx, |s, cx| s.set_value(custom.port.to_string(), w, cx));
        self.io.dlg_proxy_user.update(cx, |s, cx| {
            s.set_value(custom.username.clone().unwrap_or_default(), w, cx)
        });
        self.io.dlg_proxy_pass.update(cx, |s, cx| {
            s.set_value(custom.password.clone().unwrap_or_default(), w, cx)
        });
        self.io.dlg_proxy_prev = prev;
        // 分组：回填既有归属 id（编辑时不动分组归属）
        self.io.dlg_group_id = p.group_id.clone();
        self.io.dlg_error = None;
        // 上一次测试的结果属于**上一条连接**：带进这一次就是误导。
        // 连接意图默认无 —— 只有 `connect_profile`（用户点了「连接」）会再把它置上。
        self.io.dlg_test = None;
        self.io.dlg_testing = false;
        self.io.dlg_connect_after = false;
        self.present_dialog(DialogMode::Edit(p.id.clone()), w, cx);
    }

    /// 打开「新建连接」对话框。`group_id` 是这次新建的**上下文归属**。
    ///
    /// 表单里没有分组一栏（2026-09-23 用户要求删掉、默认不分组），所以这个参数是
    /// 归属的**唯一**来路：只有「在某个分组上右键 → 新建连接」会带上它，其余入口
    /// （栏顶 `+`、树空白右键）都传 `None` —— 传 `None` 时若库里只有一个分组，
    /// 会落到那个分组里（见下文，2026-10-21「默认 default 文件夹」）。
    ///
    /// 新建与编辑共用同一批输入状态，所以这里必须把上一次（尤其是「编辑某条连接」）
    /// 留下的内容**全部清空** —— 不清就会把上一条的名称/主机/口令带进「新建」，
    /// 用户稍不留神就存出一条「看着是新建、其实是旧数据」的连接。
    /// 端口给默认值 22（空串在保存时同样落到 22，但不该让用户对着空框猜）。
    pub fn open_connect_dialog(
        &mut self,
        group_id: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 默认落点（2026-10-21）：初始化只建一个 `default` 文件夹，若还让新连接
        // 散在根级，那个文件夹就永远是空的 —— 用户看到的是「文件夹 + 一堆游离条目」。
        //
        // 只在**库里恰好只有一个分组**时自动归属：多分组下猜错归属，比让用户自己
        // 拖一下更烦；而「只有一个分组」时没有任何歧义。改名/删除都不影响这条判断
        // （不看名字，只看数量），所以把 default 改成「生产」也照样成立。
        let group_id = group_id.or_else(|| {
            (self.conn_store.groups.len() == 1).then(|| self.conn_store.groups[0].id.clone())
        });
        let default_proxy_port = rustshell_core::proxy::DEFAULT_PROXY_PORT.to_string();
        for (state, value) in [
            (&self.io.dlg_name, ""),
            // 主机给默认值 `localhost`（2026-10-21 用户要求）：连接库的绝大多数条目
            // 都是本机 / 本地端口转发（`127.0.0.1:2222` 这类），让它开箱就有个能直接
            // 改后缀的起点，比对着空框想「要不要填 IP」快。端口仍旧默认 22。
            (&self.io.dlg_host, DEFAULT_HOST),
            (&self.io.dlg_port, "22"),
            (&self.io.dlg_user, ""),
            (&self.io.dlg_pass, ""),
            // 私钥也在清空之列：上一条连接的私钥路径留在这里，用户新建时就会
            // 「莫名其妙连上了另一台机器的钥匙」—— 这条比口令更容易被忽略，
            // 因为它是明文、看着不像敏感信息。
            (&self.io.dlg_key_path, ""),
            // 代理（F-10）的四个框同样要清：留着上一条连接的自定义代理，
            // 用户一不留神就把新连接挂到别人的代理上（地址和用户名都还写着）。
            (&self.io.dlg_proxy_host, ""),
            (&self.io.dlg_proxy_port, default_proxy_port.as_str()),
            (&self.io.dlg_proxy_user, ""),
            (&self.io.dlg_proxy_pass, ""),
        ] {
            state.update(cx, |s, cx| s.set_value(value, window, cx));
        }
        // 规则草稿本身也必须清干净（它不在上面那张表的类型里）。
        self.io.dlg_forwards.clear();
        // 会话日志覆盖同理：不回零，新建的连接就会悄悄继承上一条的「强制不记录」
        self.io.dlg_session_log = None;
        // 断线自动重连覆盖同理：不回零，新建的连接会带着上一条的「从不重连」，
        // 而用户在表单上看不出这件事（那一行显示的是「从不重连」三个字，很容易被略过）
        self.io.dlg_auto_reconnect = None;
        // 代理档位与来源快照同属「草稿本体」，一并回零 —— 漏了它，新建会带着
        // 上一条的「直连 / 自定义」档位落库，而输入框里却是空的。
        self.io.dlg_proxy_mode = rustshell_core::proxy::ProxyMode::FollowGlobal;
        self.io.dlg_proxy_kind = rustshell_core::proxy::ProxyKind::Http;
        self.io.dlg_proxy_prev = None;
        // 备注不在这张表里：它是 `TextareaState`（多行），与上面五个 `InputState`
        // 不是同一个类型，混进同一个数组会被类型系统挡下（数组元素类型必须一致）。
        // 清空本身是硬要求，漏了它「新建」就会带着上一条连接的备注落库。
        self.io
            .dlg_notes
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.io.dlg_group_id = group_id;
        // 认证方式回默认（2026-10-21 用户要求「默认密码认证」）：不清就会带着
        // 上一条连接的「私钥认证」进新建框，而私钥那一栏此刻是空的。
        self.io.dlg_auth = super::AuthMode::Password;
        self.io.dlg_error = None;
        self.io.dlg_test = None;
        self.io.dlg_testing = false;
        self.io.dlg_connect_after = false; // 新建入口永不自动连接
        self.present_dialog(DialogMode::New, window, cx);
    }

    /// 把对话框里的代理输入框读成一份 `ProxyConfig`（**不含口令**）。
    ///
    /// 两个消费点共用这一处读法（连接测试的探测配置、保存时的定稿），免得两处
    /// 各判一遍端口、各 trim 一次。（原先还有第三个消费点 —— 渲染的「生效」
    /// 投影，随「高级」界面隐藏，2026-09-29。）
    /// 口令不在其中：它的定稿要区分「新填的」「沿用的」（见 `dialog_save`），
    /// 一个纯读函数表达不了这层意思。
    ///
    /// 端口：空 = 默认（与主机表单「空端口落到 22」同一口径）；非法文本落成 0，
    /// 交给 [`ProxyConfig::validate`] 报同一句「端口需为 1–65535」——
    /// 在这里悄悄回退成默认值会让 `abc` 变成 `8080`，用户永远不知道自己填错了。
    pub(super) fn dlg_proxy_draft(&self, cx: &gpui::App) -> rustshell_core::proxy::ProxyConfig {
        let port_text = self.io.dlg_proxy_port.read(cx).value().trim().to_string();
        let port = match port_text.parse::<u16>() {
            Ok(p) => p,
            Err(_) if port_text.is_empty() => rustshell_core::proxy::DEFAULT_PROXY_PORT,
            Err(_) => 0,
        };
        let mut cfg = rustshell_core::proxy::ProxyConfig {
            kind: self.io.dlg_proxy_kind,
            host: self.io.dlg_proxy_host.read(cx).value().trim().to_string(),
            port,
            username: Some(self.io.dlg_proxy_user.read(cx).value().trim().to_string())
                .filter(|u| !u.is_empty()),
            ..Default::default()
        };
        cfg.normalize();
        cfg
    }

    /// 打开「新建分组」对话框（连接树空白处 / 分组右键菜单共用的入口）。
    ///
    /// `after` 是锚点分组 id：在某个分组上右键进来的，新分组就落在它**下面**。
    ///
    /// 分组名输入刻意不放在「新建连接」对话框里：一次保存同时做「建分组」和
    /// 「建连接」两件事，用户分不清这次到底建了什么、建到哪去了。独立入口的
    /// 好处是建完就能把新分组在树上就地展开并选中 到 结果可见。
    pub fn open_new_group_dialog(
        &mut self,
        after: Option<String>,
        parent: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 开框即清空 + 首帧聚焦名称框（输入框由正文视图自带，见 `GroupNameDialogView`）。
        // 字段名同步设回「分组名称」——同一个视图也承载连接改名（「连接名称」），
        // 不设就会把上一次用过的标签留在框上。
        self.io.group_name_body.update(cx, |v, cx| {
            v.set_label(rustshell_core::i18n::t("分组名称"));
            v.set_value(String::new(), window, cx);
            v.open(cx);
        });
        // `after`（同级锚点）与 `parent`（建到里面）互斥；同传时以 parent 为准
        self.io.dlg_group_after = parent.is_none().then_some(after).flatten();
        self.io.dlg_group_parent = parent;
        self.io.dlg_error = None;
        self.present_dialog(DialogMode::NewGroup(None), window, cx);
    }

    /// 打开「重命名分组」对话框：与新建共用同一个表单，只换标题、按钮文字与校验口径。
    ///
    /// 改名**不动分组 id**，所以树上那个节点的展开态、以及连接的归属引用都原样保持。
    pub fn open_rename_group_dialog(
        &mut self,
        group_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(name) = self.conn_store.group_name(group_id).map(str::to_string) else {
            log::warn!("重命名分组失败：分组已不存在 id={group_id}");
            return;
        };
        self.io.group_name_body.update(cx, |v, cx| {
            v.set_label(rustshell_core::i18n::t("分组名称"));
            v.set_value(name, window, cx);
            v.open(cx);
        });
        self.io.dlg_error = None;
        self.present_dialog(DialogMode::NewGroup(Some(group_id.to_string())), window, cx);
    }

    // OpenSSH `~/.ssh/config` 导入（F-1）

    /// 入口：从默认 `~/.ssh/config` 发起导入（解析 → 预览）。
    ///
    /// 主文件读不到时不静默失败：给出一条可见提示，用户可改用设置里的「导入连接…」
    /// 或手选文件（`import_openssh_pick_file`）。
    pub fn import_openssh_config(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(home) = rustshell_core::openssh::user_home_dir() else {
            self.set_info(
                rustshell_core::i18n::t("未找到 OpenSSH 配置").to_string(),
                false,
            );
            cx.notify();
            return;
        };
        let ssh_dir = rustshell_core::openssh::default_ssh_dir(&home);
        let config = rustshell_core::openssh::config_path(&ssh_dir);
        self.load_openssh_config(config, ssh_dir, home, window, cx);
    }

    /// 解析指定 config 并建立预览（`read_error` 时提示并中止，不弹空对话框）。
    ///
    /// `pub(super)`：异步选文件的回填由父模块 `main_view` 的 `render` 兑现
    /// （弹对话框要 `Window`），跨层调用需要比 `fn` 更宽的可见性。
    pub(super) fn load_openssh_config(
        &mut self,
        config: std::path::PathBuf,
        ssh_dir: std::path::PathBuf,
        home: std::path::PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let user_default = rustshell_core::openssh::local_username();
        let load =
            rustshell_core::openssh::load_config(&config, &ssh_dir, &home, user_default.as_deref());
        if let Some(err) = load.read_error {
            log::warn!("OpenSSH 配置读取失败: {err}");
            self.set_info(
                format!("{}: {err}", rustshell_core::i18n::t("未找到 OpenSSH 配置")),
                false,
            );
            cx.notify();
            return;
        }
        let plans = rustshell_core::openssh::plan_import(&self.conn_store, &load.parse);
        // 默认只勾「新增」项：重复/跳过项勾了也不会被导入，默认不勾免得用户以为漏了东西
        self.conn_ui.openssh_selected = plans.iter().map(|p| p.status.is_new()).collect();
        self.conn_ui.openssh_plans = plans;
        self.conn_ui.openssh_warnings = load.parse.warnings.clone();
        self.conn_ui.openssh_source = config.to_string_lossy().to_string();
        log::info!(
            "OpenSSH 配置解析完成: {} 条候选（来源 {}）",
            self.conn_ui.openssh_plans.len(),
            self.conn_ui.openssh_source
        );
        // 预览层已经开着 = 用户在预览里点了「选择文件…」换源。官方对话框层每帧
        // 重跑 builder（`render` 里现读 `openssh_plans`），所以只要让视图脏一次
        // 列表就换新了；这里**不能**再调 `present_openssh_import_dialog` ——
        // 它会往栈上再推一层（同一个预览叠两份）。
        if self.conn_ui.openssh_dialog_open {
            cx.notify();
        } else {
            self.present_openssh_import_dialog(window, cx);
        }
    }

    /// 默认 config 缺失时的手动补救：系统「打开」对话框另选一个 config 文件。
    ///
    /// 选文件回调拿不到 `Window`，故只把载荷落到 `openssh_pick_sync`，
    /// 由 `render` 兑现弹预览（与本仓既有的两段式同一手法）。
    pub fn import_openssh_pick_file(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: None,
        });
        cx.spawn(async move |this, cx| {
            match rx.await {
                Ok(Ok(Some(files))) => {
                    let Some(file) = files.into_iter().next() else {
                        return;
                    };
                    let _ = this.update(cx, |m, cx| {
                        // 手选文件的父目录当作 ssh_dir：相对 IdentityFile 需要有个基准
                        let ssh_dir = file
                            .parent()
                            .map(|p| p.to_path_buf())
                            .unwrap_or_else(|| file.clone());
                        let home = rustshell_core::openssh::user_home_dir().unwrap_or_default();
                        m.conn_ui.openssh_pick_sync = Some((file, ssh_dir, home));
                        cx.notify();
                    });
                }
                // 用户取消：静默返回（主动弹一句「已取消」纯属噪音）
                Ok(Ok(None)) => {}
                // 等待结果被中断：窗口已关，没人可通知
                Err(_) => {}
                Ok(Err(e)) => {
                    log::error!("打开 OpenSSH 配置选择框失败: {e}");
                    let _ = this.update(cx, |m, cx| {
                        m.set_info(
                            format!("{}: {e}", rustshell_core::i18n::t("未找到 OpenSSH 配置")),
                            false,
                        );
                        cx.notify();
                    });
                }
            }
        })
        .detach();
    }

    /// 勾选 / 取消一条预览项。重复与跳过项不可选（勾了也不会被导入）。
    pub fn openssh_toggle(&mut self, ix: usize, cx: &mut Context<Self>) {
        if !self
            .conn_ui
            .openssh_plans
            .get(ix)
            .is_some_and(|p| p.status.is_new())
        {
            return;
        }
        if let Some(slot) = self.conn_ui.openssh_selected.get_mut(ix) {
            *slot = !*slot;
            cx.notify();
        }
    }

    /// 全选 / 全不选（只作用于「新增」项）。
    pub fn openssh_select_all(&mut self, on: bool, cx: &mut Context<Self>) {
        for (ix, slot) in self.conn_ui.openssh_selected.iter_mut().enumerate() {
            let new = self
                .conn_ui
                .openssh_plans
                .get(ix)
                .is_some_and(|p| p.status.is_new());
            *slot = on && new;
        }
        cx.notify();
    }

    /// 当前勾选且可导入的条数（按钮文案「导入选中 (N)」用）。
    pub fn openssh_selected_new_count(&self) -> usize {
        self.conn_ui
            .openssh_selected
            .iter()
            .enumerate()
            .filter(|(ix, on)| {
                **on && self
                    .conn_ui
                    .openssh_plans
                    .get(*ix)
                    .is_some_and(|p| p.status.is_new())
            })
            .count()
    }

    /// 落库：选中且状态为「新增」的条目经既有导入管道合并 + 原子写 + 展开新分组。
    ///
    /// 复用 `merge_import`（而非另写一套写入）保证：去重口径与预览一致、
    /// 不覆盖任何既有连接、导出体永不含口令。
    pub fn openssh_commit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let group = rustshell_core::openssh::DEFAULT_IMPORT_GROUP.to_string();
        let selected: Vec<usize> = self
            .conn_ui
            .openssh_selected
            .iter()
            .enumerate()
            .filter(|(_, on)| **on)
            .map(|(ix, _)| ix)
            .collect();
        let exp = rustshell_core::openssh::export_selected(
            &self.conn_ui.openssh_plans,
            &selected,
            &group,
        );
        if exp.profiles.is_empty() {
            self.set_info(
                rustshell_core::i18n::t("没有解析到任何主机").to_string(),
                false,
            );
            cx.notify();
            return;
        }
        // 记下既有分组 id：新登记的分组要顺手展开，否则连接导进来了却在收起的分组里
        let before: std::collections::HashSet<String> = self
            .conn_store
            .groups
            .iter()
            .map(|g| g.id.clone())
            .collect();
        let outcome = self.conn_store.merge_import(&exp);
        if outcome.is_empty() {
            self.set_info(
                rustshell_core::i18n::t("该文件里没有新的连接").to_string(),
                false,
            );
            self.close_openssh_dialog(window, cx);
            return;
        }
        if let Err(e) = self.conn_store.save(&paths::connections_path()) {
            log::error!("OpenSSH 导入后保存连接失败: {e}");
            self.set_info(
                format!("{}: {e}", rustshell_core::i18n::t("导出文件写入失败")),
                false,
            );
            cx.notify();
            return;
        }
        for g in &self.conn_store.groups {
            if !before.contains(&g.id) {
                self.conn_ui.expanded_groups.insert(g.id.clone());
            }
        }
        log::info!(
            "OpenSSH 导入完成: 新增 {} 个、跳过 {} 个、新增分组 {} 个（来源 {}）",
            outcome.added,
            outcome.skipped,
            outcome.groups_added,
            self.conn_ui.openssh_source
        );
        self.set_info(
            rustshell_core::i18n::ts(
                format!(
                    "已从 OpenSSH 导入 {} 个连接，跳过 {} 个重复",
                    outcome.added, outcome.skipped
                ),
                format!(
                    "Imported {} connection(s) from OpenSSH, skipped {} duplicate(s)",
                    outcome.added, outcome.skipped
                ),
            ),
            true,
        );
        self.close_openssh_dialog(window, cx);
    }

    /// 删除分组前的二次确认：官方 `AlertDialog` 浮动弹层（与快捷命令分组同一形态）。
    ///
    /// 组内连接**不会被删**，确认文案里要把「移到未分组」说清楚 —— 用户得先知道
    /// 数据不会丢，才敢点这个按钮。OK 按钮用 `Danger` 变体；取消 / ESC / 右上角 X
    /// 都视为放弃（`AlertDialog` 无待办状态，故不需要额外的 `on_close` 兜底）。
    pub fn confirm_remove_group(
        &mut self,
        group_id: &str,
        name: &str,
        members: usize,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 同一个 Root 再叠一层确认框对用户只是噪音；已有对话框就忽略本次请求。
        if w.has_active_dialog(cx) {
            return;
        }
        let title: SharedString = rustshell_core::i18n::t("删除分组").into();
        // 嵌套分组下子分组会上移一级，这层影响也要说清楚
        let has_subgroups = self
            .conn_store
            .groups
            .iter()
            .any(|g| g.parent.as_deref() == Some(group_id));
        let desc: SharedString = if members == 0 {
            rustshell_core::i18n::ts(
                if has_subgroups {
                    format!("删除分组「{name}」？该分组下没有连接，其中的子分组会上移一级。")
                } else {
                    format!("删除分组「{name}」？该分组下没有连接。")
                },
                if has_subgroups {
                    format!("Delete group \"{name}\"? It has no connections; its subgroups will move up one level.")
                } else {
                    format!("Delete group \"{name}\"? It has no connections.")
                },
            )
            .into()
        } else {
            let sub_note = if has_subgroups {
                rustshell_core::i18n::ts(
                    "，子分组会上移一级".to_string(),
                    "; its subgroups will move up one level".to_string(),
                )
            } else {
                String::new()
            };
            rustshell_core::i18n::ts(
                format!(
                    "删除分组「{name}」？其中的 {members} 个连接会移到「未分组」，连接本身不会被删除{sub_note}。"
                ),
                format!(
                    "Delete group \"{name}\"? Its {members} connection(s) will move to \
                     \"Ungrouped\"; the connections themselves are kept{sub_note}."
                ),
            )
            .into()
        };
        let ok_text: SharedString = rustshell_core::i18n::t("删除").into();
        let cancel_text: SharedString = rustshell_core::i18n::t("取消").into();
        let gid = group_id.to_string();

        w.open_alert_dialog(cx, move |alert, _w, _cx| {
            alert
                .title(title.clone())
                .description(desc.clone())
                .confirm()
                // 顺序要紧：`on_ok` 是在**已有** `button_props` 上改，
                // 排在 `.button_props(..)` 之前会被它整体覆盖（按钮文字与回调一起丢）。
                .button_props(
                    DialogButtonProps::default()
                        .ok_text(ok_text.clone())
                        .ok_variant(ButtonVariant::Danger)
                        .cancel_text(cancel_text.clone())
                        .show_cancel(true),
                )
                .on_ok({
                    let gid = gid.clone();
                    move |_, w, cx| {
                        crate::main_view::ui_call(&gid, MainView::remove_group_by_id, w, cx);
                        true
                    }
                })
        });
        cx.notify();
    }

    /// 真正删除分组（`AlertDialog` 的「删除」按钮）：组内连接回收到「未分组」。
    pub fn remove_group_by_id(&mut self, group_id: &str, _w: &mut Window, cx: &mut Context<Self>) {
        match self.conn_store.remove_group(group_id) {
            Some(_) => {
                // 分组没了：展开态与「待归属」的悬空 id 一并回收
                self.conn_ui.expanded_groups.remove(group_id);
                if self.io.dlg_group_id.as_deref() == Some(group_id) {
                    self.io.dlg_group_id = None;
                }
                if self.io.dlg_group_after.as_deref() == Some(group_id) {
                    self.io.dlg_group_after = None;
                }
                let _ = self.conn_store.save(&paths::connections_path());
            }
            None => log::warn!("删除分组失败：分组已不存在 id={group_id}"),
        }
        cx.notify();
    }

    pub(super) fn mount_terminal(
        &mut self,
        session: &mut SessionHandle,
        cx: &mut Context<Self>,
    ) -> (
        Entity<rustshell_terminal::TerminalView>,
        crate::terminal_scrollbar::TermScrollHandle,
    ) {
        let sink = session.resize.clone();
        // T4-P3：配色不再硬绑主题，由「配色方案」设置解析
        // （follow-theme / 预设 / 自定义三态，见 Settings::resolve_terminal_palette）。
        let pane_cfg = TerminalPaneConfig {
            font_family: self.settings.terminal_font_family.clone(),
            font_size: self.settings.terminal_font_size,
            // 行高（2026-09-28 新增设置项；越界值在读取侧钳制）
            line_height: self.settings.clamped_line_height(),
            scrollback: self.settings.scrollback,
            palette: self.settings.resolve_terminal_palette(),
            // T4-E1：字符编码（保存后新终端生效）
            encoding: self.settings.terminal_encoding.clone(),
            // 终端底色：不透明（alpha = 1.0）
            background_alpha: 1.0,
            cursor_blink: self.settings.terminal_cursor_blink,
            cursor_blink_ms: self.settings.terminal_cursor_blink_ms,
            cursor_style: self.settings.terminal_cursor_style,
            // 粘贴策略：两个引擎参数由**同一个** `PasteBehavior` 算出，
            // 结构上不可能再配出「逐行执行 + 弹确认」这种矛盾组合
            paste_normalize_newlines: self.settings.terminal_paste_behavior.normalize_newlines(),
            paste_protect_lines: self.settings.terminal_paste_behavior.protect_lines(),
        };
        let term = create_terminal_view(
            session,
            &pane_cfg,
            sink,
            |_w, _cx| {
                // 会话关闭事件由输出泵 SessionEvent::Closed 统一处理；
                // 此处终端进程退出也发一次事件由 poll 收敛。
            },
            cx,
        );
        let scroll = crate::terminal_scrollbar::TermScrollHandle::new(term.clone());
        (term, scroll)
    }
    pub fn close_tab_by_session(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(pos) = self.tabs_state.tabs.iter().position(|t| t.session.id == id) {
            let mut tab = self.tabs_state.tabs.remove(pos);
            // 外部链接的临时连接：标签一关，口令随表项一起从内存消失
            // （这是「链接里的密码只留在内存里、标签关掉就没了」的落点）。
            if let Some(pid) = tab
                .session
                .profile_id
                .clone()
                .filter(|p| p.starts_with(rustshell_core::launch::EXTERNAL_PROFILE_PREFIX))
            {
                self.conn_ui.external.remove(&pid);
            }
            // 终止会话（Drop 置位退出标志）
            tab.session.state = SessionState::Closed;
            // 这里在 UI 线程上同步析构会话：ConPTY 的阻塞式释放已由
            // AsyncMasterPty 转交后台线程，本阶段打点用于在
            // 现场日志里验证「关标签/关窗口不再造成未响应」。
            crate::watchdog::enter(crate::watchdog::Phase::SessionTeardown);
            drop(tab);
            crate::watchdog::leave();
            if self.tabs_state.active.as_deref() == Some(id) {
                self.tabs_state.active = self.tabs_state.tabs.last().map(|t| t.session.id.clone());
                self.tabs_state.pending_focus = true;
            }
            // 2026-09-29 用户反馈「关掉全部 tab 不再触发命令窗口显示」：原先这里
            // 会置「空了就弹命令窗口」的待办，已整条撤掉。清空后的画面交给空态
            // 本身（`split_actions::render_no_session` 的快捷键表），不再由程序
            // 替用户把命令窗口推上来。
            // 分屏槽位修正：被关掉的可能正是某个窗格的槽位。
            // 修不成立就取消分屏（标签都还在池子里，不丢会话），
            // 绝不留「窗格指向已关闭标签」的半残状态。
            self.normalize_split();
            cx.notify();
        }
    }

    pub fn close_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(tab) = self.tabs_state.tabs.get(index) {
            let id = tab.session.id.clone();
            self.close_tab_by_session(&id, cx);
        }
    }

    // Tab 右键菜单动作

    /// 断开指定 Tab 的会话，但保留 Tab（右键「断开」）。
    ///
    /// 与「关闭」的区别：这里只终止会话（放掉 PTY 写端 / 读端 / SFTP / 指标句柄，
    /// 卸下终端视图），`TabEntry` 本身留在标签栏，`state = Closed` 且无失败原因
    /// → `TabEntry::is_disconnected` 为真，内容区渲染「已断开连接」占位，
    /// 用户可再用右键「连接」原地重连。
    ///
    /// 阻塞点说明：`terminal = None` 会在 UI 线程析构终端视图与 `SessionHandle`
    /// 的 PTY 句柄，但 ConPTY 的阻塞式释放已由 `AsyncMasterPty` 转交后台线程，
    /// 因此这里只做「取下引用」，不做系统调用。
    pub fn disconnect_tab(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs_state.tabs.iter_mut().find(|t| t.session.id == id) else {
            return;
        };
        if tab.terminal.is_none() {
            return; // 本就未连接（连接中 / 已断开），无需处理
        }
        log::info!("断开会话（保留 Tab）: {id}");
        Self::detach_session(tab);
        tab.connect_error = None;
        tab.connect_started = None;
        // 用户主动断开 = 自动重连计划作废：留着它，断连条上的「等待重试」会
        // 让用户以为「它还会自己连回来」，而实际上他已经把这条路关掉了。
        tab.retry = None;
        cx.notify();
    }

    /// 把 Tab 转入「意外断连」态：**保留终端画面**，只封输入。
    ///
    /// 与 [`MainView::disconnect_tab`]（用户主动断开）的关键差别只有一条：
    /// 这里**不卸下终端视图**。掉线现场是要给人看的 —— 最后那几行输出、
    /// 报错信息、光标停在哪，都是判断「为什么断」的证据；把它换成一页
    /// 「已断开连接」的占位卡等于把证据删了。
    ///
    /// 三件事同时发生：
    /// 1. 终端引擎 `set_alive(false)` —— 键盘 / 粘贴 / `write_to_pty` 三条输入通路
    ///    在**引擎内部**堵死（不依赖 UI 层是否记得判），光标停画、闪烁停表、
    ///    点击不再抢焦点；画面、滚动、划选复制全部保留。
    /// 2. `dead = Some(原因)` —— 渲染层据此在窗格底部叠断连条，状态点转灰。
    /// 3. 会话句柄里与「连接」有关的通道放掉（writer/reader/SFTP/指标），
    ///    但 `TabEntry` 本身留在标签栏，`active` 指过来，用户看到的就是原来那一屏。
    ///
    /// 幂等：已在断连态时直接返回（`poll` 可能对同一 Tab 收到多个终止事件）。
    pub fn mark_session_dead(&mut self, id: &str, reason: String, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs_state.tabs.iter_mut().find(|t| t.session.id == id) else {
            return;
        };
        if tab.dead.is_some() {
            return;
        }
        let Some(term) = tab.terminal.clone() else {
            // 没有终端可留（连接中的占位 Tab 被对端掐掉）：退化为原来的失败落点
            tab.session.state = SessionState::Closed;
            tab.connect_started = None;
            if tab.connect_error.is_none() {
                tab.connect_error = Some(reason);
            }
            return;
        };
        log::info!("会话断连（保留画面）: tab={id} reason={reason}");
        // 引擎侧封输入 —— 必须先于 UI 侧改动，这样任何「同一帧里又收到一次输入」
        // 的竞争都已经被挡住。
        term.update(cx, |v, cx| v.set_alive(false, cx));
        tab.dead = Some(reason);
        tab.session.state = SessionState::Closed;
        tab.session.writer = None;
        tab.session.reader = None;
        tab.session.sftp_tx = None;
        tab.session.metrics = None;
        tab.connect_error = None;
        tab.connect_started = None;
        // 补全浮层跟着会话一起收（C-1）：候选是对着**上一台活着时的**当前行算出来的，
        // 会话一死它就没有再刷新的来源（`current_input_line` 对死会话返回 `None`），
        // 留着就是一块永远停在旧内容的框，用户点它还会尝试往死会话写字节。
        // 同理清掉历史缓存：这台机器的命令记录留给下一次连上时重新累积。
        if self.panel_ui.completion.open {
            self.panel_ui.completion.close();
        }
        self.panel_ui.completion_history.clear();
        // `terminal` 与 `scroll` **刻意不清**：断连现场仍要能滚动翻看历史输出。
        self.tabs_state.active = Some(id.to_string());
        // 归属通知：断连条画在窗格上、浮层收掉了，读这两样的子视图只有 Completion；
        // 而窗格本身（`render_term_pane`）在 `MainView` 的树里，走裸 notify 那条路。
        // 一次 update 只发一次 notify（`ViewEpoch` 的约束）—— 所以这里合并成一次。
        self.panel_ui
            .view_epoch
            .merge_scoped(ViewDirty::default().set(ChildView::Completion));
        cx.notify();
    }

    /// 掉线后给这个标签排上重连计划（F-4）：**不立刻连**，先等一个退避间隔。
    ///
    /// 为什么不在掉线当场重连：掉线往往伴随网络抖动（换网、Wi-Fi 重连、对端重启），
    /// 立刻撞上去多半白撞一次；退避序列把「第一次尝试」也纳入节奏，抖动过去了
    /// 反而更容易一次连上。
    ///
    /// 三个前提，缺一个就不排：临时会话没有可重放的目标；配置已被删除（自动重连
    /// 不该刷「无法重连」的提示，那是用户自己点时才该看到的）；开关与上限不批准。
    ///
    /// 上限在**这一刻**取快照存进账本（见 [`RetryLedger`]）：一套计划按一套规矩走完。
    pub(super) fn schedule_retry(&mut self, id: &str, pid: Option<&str>) {
        let Some(pid) = pid else {
            return; // 未保存为配置的临时会话：只留现场，无重放目标
        };
        let (choice, max) = match self.conn_store.profile(pid) {
            Some(p) => (p.auto_reconnect, self.settings.reconnect_max_retries),
            None => {
                log::info!("会话掉线，但连接配置已被删除，不再自动重连: profile={pid}");
                return;
            }
        };
        if !rustshell_core::reconnect::should_retry(choice, self.settings.auto_reconnect, 0, max) {
            log::info!("会话掉线，自动重连未启用 / 上限为 0，只留断连现场: profile={pid}");
            return;
        }
        let wait =
            rustshell_core::reconnect::backoff_secs(self.settings.reconnect_interval_secs, 1);
        let Some(tab) = self.tabs_state.tabs.iter_mut().find(|t| t.session.id == id) else {
            return;
        };
        tab.retry = Some(RetryLedger {
            attempts: 0,
            max,
            due_at: Some(std::time::Instant::now() + std::time::Duration::from_secs(wait)),
        });
        log::info!("会话掉线，{wait} 秒后自动重连（第 1/{max} 次）: profile={pid}");
    }

    /// 账本到点：把该试的那些重连真正发出去（F-4）。
    ///
    /// 跑在 `poll` 的 300ms 轮询里，不另起定时器 —— 轮询本来每轮都跑，到期判定
    /// 挂在这里等于零新增调度设施。判定与账本推进在
    /// [`super::take_due_retries`]（纯函数，可单测）；这里只负责点火。
    pub(super) fn fire_due_retries(&mut self, cx: &mut Context<Self>) {
        for id in super::take_due_retries(&mut self.tabs_state.tabs, std::time::Instant::now()) {
            log::info!("自动重连到期，发起尝试: tab={id}");
            self.start_reconnect(&id, cx);
        }
    }

    /// 原地重新连接指定 Tab（**用户动作**：右键「连接」/ 断连条上的「重新连接」）。
    ///
    /// 用户自己动手 = 自动重连的计划当场作废（`retry = None`）：接下来该由他接手。
    /// 不清账的话，账本会照自己的节奏在后台再插一脚 —— 用户看到的是「我刚点了重连，
    /// 它又自己重连了一次」，而次数也被算进了那一轮预算里。
    pub fn reconnect_tab(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(tab) = self.tabs_state.tabs.iter_mut().find(|t| t.session.id == id) {
            tab.retry = None;
        }
        self.start_reconnect(id, cx);
    }

    /// 真正发起一次原地重连。两个入口共用：[`MainView::reconnect_tab`]（用户动作）
    /// 与 [`MainView::fire_due_retries`]（账本到点）。**账本的处置不在这里** ——
    /// 用户动作要清账、自动重连必须留着，区别就在调用方那一句。
    fn start_reconnect(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(kind) = self
            .tabs_state
            .tabs
            .iter()
            .find(|t| t.session.id == id)
            .map(|t| t.session.kind)
        else {
            return;
        };
        if kind == SessionKind::Local {
            // 本地终端没有「连接」这一步（右键菜单里那一项对本地标签同样可用，
            // 见 `tab_menu_flags`）：照原样再起一只，落在原槽位上。
            return self.restart_local_tab(id, cx);
        }
        let Some(pid) = self
            .tabs_state
            .tabs
            .iter()
            .find(|t| t.session.id == id)
            .and_then(|t| t.session.profile_id.clone())
        else {
            return; // 未保存为配置的会话没有可重放的目标
        };
        let Some(p) = self.lookup_profile(&pid) else {
            self.conn_ui.connect_notice =
                Some(rustshell_core::i18n::t("该会话的连接配置已被删除，无法重连").to_string());
            cx.notify();
            return;
        };
        // 外部链接重连仍守「一条连接一个通道」：`sftp://` 只要 sftp、`ssh://` 只要 shell。
        let channels = match self.conn_ui.external.get(&pid).map(|e| e.kind) {
            Some(rustshell_core::launch::LaunchKind::Sftp) => SshChannels::SftpOnly,
            Some(rustshell_core::launch::LaunchKind::Ssh) => SshChannels::ShellOnly,
            None => SshChannels::Full,
        };
        let external = self.conn_ui.external.contains_key(&pid);

        let timeout = std::time::Duration::from_secs(self.settings.connect_timeout_secs.clamp(
            rustshell_core::ssh::CONNECT_TIMEOUT_MIN_SECS,
            rustshell_core::ssh::CONNECT_TIMEOUT_MAX_SECS,
        ));

        // ★ C-4（2026-09-28）：与 `connect_profile_now` 同一道在途守卫。
        //
        // 这条路径**尤其**需要它：断连现场的「重连」按钮可以被连点，而每次重连
        // 都是一条新线程 + 一组 tokio 泵。旧线程在半死服务端上可能挂很久
        // （信任等待可顺延至 180s），连点 N 次就有 N 条在途线程，全部成功时
        // 产生 N-1 个孤儿会话。守卫放在**改动 Tab 状态之前**，重复请求完全无副作用。
        if !self.conn_ui.connecting_profiles.insert(p.id.clone()) {
            log::info!("忽略重复的重连请求（已有在途连接线程）: tab={id} profile={pid}");
            return;
        }

        if let Some(tab) = self.tabs_state.tabs.iter_mut().find(|t| t.session.id == id) {
            if tab.is_dead() {
                // 断连现场：**终端画面留着**，只把状态推到 `Connecting`
                // —— 断连条据此改说「正在重连…」，用户盯着原来那一屏等结果，
                // 而不是被扔回一张「连接中」的空占位卡。
                // `dead` 留在原地：新会话真建立之前，输入必须一直是封死的。
                tab.session.state = SessionState::Connecting;
                tab.connect_started = Some(std::time::Instant::now());
            } else {
                Self::detach_session(tab); // 先放掉旧会话（已连接时相当于重连）
                tab.session.state = SessionState::Connecting;
                tab.connect_error = None;
                tab.connect_started = Some(std::time::Instant::now());
            }
            tab.session.title = p.name.clone();
        }
        self.tabs_state.active = Some(id.to_string());
        log::info!("原地重连 Tab: {id} profile={pid}");
        // 重连是**一次新的连接尝试**，因此也另起一个日志文件（文件名带时间前缀，
        // 按时间顺序读即可拼回整段现场）。不自作主张续写旧文件：续写会让
        // 「这一次到底连上没连上」变得说不清 —— 而重连场景恰恰最需要这个答案。
        // 外部链接例外：临时连接不在磁盘上留痕。
        let log_path = if external {
            None
        } else {
            self.session_log_target(Some(&p))
        };
        connect_in_background(
            p,
            timeout,
            log_path,
            self.settings.clamped_sftp_max_concurrent(),
            self.runtime_global_proxy(),
            channels,
            connect_results().0.clone(),
        );
        cx.notify();
    }

    /// 复制 Tab（右键「复制」）：本地 Shell 新开一个，SSH 按同一 profile 再连一个。 未保存成连接配置的临时 SSH 会话没有可复制的目标，给出提示而不是静默无响应。
    pub fn duplicate_tab(&mut self, id: &str, w: &mut Window, cx: &mut Context<Self>) {
        let Some((kind, pid)) = self
            .tabs_state
            .tabs
            .iter()
            .find(|t| t.session.id == id)
            .map(|t| (t.session.kind, t.session.profile_id.clone()))
        else {
            return;
        };
        match (kind, pid) {
            (SessionKind::Local, _) => self.open_local_shell(cx),
            (SessionKind::Ssh, Some(pid)) => self.connect_profile(&pid, w, cx),
            (SessionKind::Ssh, None) => {
                self.conn_ui.connect_notice =
                    Some(rustshell_core::i18n::t("该会话未保存为连接配置，无法复制").to_string());
                cx.notify();
            }
        }
    }

    /// 关闭除`keep_id`外的全部 Tab（右键「关闭其他」）。
    pub fn close_other_tabs(&mut self, keep_id: &str, cx: &mut Context<Self>) {
        if !self.tabs_state.tabs.iter().any(|t| t.session.id == keep_id) {
            return;
        }
        let doomed: Vec<String> = self
            .tabs_state
            .tabs
            .iter()
            .filter(|t| t.session.id != keep_id)
            .map(|t| t.session.id.clone())
            .collect();
        if doomed.is_empty() {
            return;
        }
        log::info!("关闭其他标签：保留 {keep_id}，关闭 {} 个", doomed.len());
        for id in doomed {
            self.close_tab_by_session(&id, cx);
        }
        // close_tab_by_session 会把 active 落到「最后一个 Tab」，此处纠正为保留项
        self.tabs_state.active = Some(keep_id.to_string());
        self.tabs_state.pending_focus = true;
        // 逐次 close_tab_by_session 已修正过分屏槽位，这里再走一遍兜底
        // （池子发生大幅变化后要保证槽位仍合法）
        self.normalize_split();
        cx.notify();
    }

    /// 关闭全部 Tab（右键「关闭全部」）。
    pub fn close_all_tabs(&mut self, cx: &mut Context<Self>) {
        let ids: Vec<String> = self
            .tabs_state
            .tabs
            .iter()
            .map(|t| t.session.id.clone())
            .collect();
        if ids.is_empty() {
            return;
        }
        log::info!("关闭全部标签：共 {} 个", ids.len());
        for id in ids {
            self.close_tab_by_session(&id, cx);
        }
    }

    /// 放掉一个 Tab 的会话资源（保留`TabEntry`）：PTY 写/读端、SFTP、指标句柄 与终端视图全部置空，`state`收敛为`Closed`。
    fn detach_session(tab: &mut TabEntry) {
        tab.session.state = SessionState::Closed;
        tab.session.writer = None;
        tab.session.reader = None;
        tab.session.sftp_tx = None;
        tab.session.metrics = None;
        tab.terminal = None;
        tab.scroll = None;
    }

    /// 窗口激活态决定本地采样间隔（失焦降频，聚焦恢复设置值）。
    pub fn sync_metrics_focus(&mut self, window: &Window) {
        use std::sync::atomic::Ordering;
        let target = if window.is_window_active() {
            self.settings.sample_interval_ms
        } else {
            5000
        };
        self.panel_ui
            .metrics_interval
            .store(target.max(200), Ordering::Relaxed);
    }

    /// 切换激活会话（Dashboard / 终端面板共用）
    pub fn activate_session(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.tabs_state.tabs.iter().any(|t| t.session.id == id) {
            // 先把**旧会话**的命令历史换出去（C-1 遗留项 2）。
            //
            // 位置在这儿而不是在 `switch_history_bucket` 内部算旧键：`self.tabs_state.active`
            // 一旦被覆盖，旧会话的身份就没了 —— 必须先记下来。
            //
            // 键要在 `active` 被改**之前**算，所以先算键、再改 active、最后换桶：
            let old_key = self.current_history_key();
            let switched = self.tabs_state.active.as_deref() != Some(id);
            self.tabs_state.active = Some(id.to_string());
            self.switch_history_bucket_to(&old_key);
            // 换台机器就把补全浮层收掉（`close_completion` 的文档早写了「切标签
            // 走这里」，实现却一直没接上）。
            //
            // 不收的后果是**跨机器串味**：候选是拿上一台机器的当前行算出来的，
            // 挂在新标签上等于「A 的命令列在 B 的框里」；更糟的是那段前缀会被
            // 当成新机器的输入继续参与补全。C-2 花大力气把**历史**按机器分桶，
            // 这里漏一个就是同一个洞的另一半。
            //
            // 只按「真的换了」判断（同一标签重复激活不收）：切标签时点已经选中的
            // 那一项也会走到这儿，收掉浮层是多余的惊扰。
            if switched && self.panel_ui.completion.open {
                self.panel_ui.completion.close();
                // 归属通知：浮层收掉了（与断连那条路同一手法，合并成一次 notify）。
                self.panel_ui
                    .view_epoch
                    .merge_scoped(ViewDirty::default().set(ChildView::Completion));
            }
            self.tabs_state.pending_focus = true;
            cx.notify();
        }
    }
    pub fn active_tab(&self) -> Option<&TabEntry> {
        let id = self.tabs_state.active.as_deref()?;
        self.tabs_state.tabs.iter().find(|t| t.session.id == id)
    }

    pub fn metrics_for_active(&self) -> &std::sync::Arc<MetricsStore> {
        match self.active_tab() {
            Some(t) if t.session.kind == rustshell_core::session::SessionKind::Ssh => {
                t.session.metrics.as_ref().unwrap_or(&self.local_metrics)
            }
            _ => &self.local_metrics,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::test_support::{prod, region};

    /// C-4 回归（2026-09-28）：**在途连接守卫**必须挂在两条发起连接的路上。
    ///
    /// `connect_in_background` 返回 `JoinHandle`，此前被直接丢弃（未绑定）——
    /// 每次调用都新起一条线程 + 一组 tokio 泵。重连可以叠加（自动重连的退避
    /// 与手动点「连接」、断连现场连点重连），旧线程若挂在半死服务端上
    /// （信任等待可顺延至 180s），就会与新线程并存；两条都连上时产生**孤儿会话**。
    ///
    /// 修法是把 `connecting_profiles` 从「用来渲染提示的账本」升级成真正的互斥锁：
    /// `insert` 返回 `false`（已在集合里）= 已有在途线程，本次请求直接返回。
    /// 这条护栏扫源码钉住两处调用点都在**发起线程之前**做了这个判断。
    #[test]
    fn both_connect_paths_guard_against_in_flight_threads() {
        let code = prod(include_str!("connections.rs"));

        // 两条路径各一次守卫：`connect_profile_now` 与 `reconnect_tab`。
        assert_eq!(
            code.matches("if !self.conn_ui.connecting_profiles.insert(")
                .count(),
            2,
            "应有 2 处在途守卫（connect_profile_now + reconnect_tab）——\
             少一处就会重开「同一 profile 两条连接线程」的孤儿会话窗口（C-4）"
        );

        // 守卫必须**早于**线程发起：在 connect_profile_now 里，守卫要在
        // `connect_in_background` 之前。
        let now_at = code
            .find("pub(super) fn connect_profile_now(")
            .expect("connect_profile_now 必须存在");
        let reconnect_at = code.find("pub fn reconnect_tab(").unwrap_or(code.len());
        let next_fn = code[now_at + 1..]
            .find("\n    /// ")
            .map(|i| now_at + 1 + i)
            .unwrap_or(reconnect_at);
        let body = &code[now_at..next_fn];
        let guard = body
            .find("if !self.conn_ui.connecting_profiles.insert(")
            .expect("connect_profile_now 里没有在途守卫（C-4）");
        let spawn = body
            .find("connect_in_background(")
            .expect("connect_profile_now 里必须发起后台连接");
        assert!(
            guard < spawn,
            "在途守卫必须在线程发起**之前**：放后面等于线程已经起了才发现该拦（C-4）"
        );

        // 反面护栏：守卫存在的前提是「成功路径也会清标记」。
        // 只看 MainView::poll 里必须清 —— 否则守卫会让该 profile 永远无法重连。
        let main = prod(include_str!("../main_view.rs"));
        assert!(
            main.contains("connecting_profiles.remove(&pid)")
                || main.contains("connecting_profiles.remove(&profile_id)"),
            "连接成功/失败路径必须清 `connecting_profiles` —— 不清的话在途守卫会\n\
             把该 profile 永久锁死（再也重连不上）"
        );
    }

    /// 「本地终端与 SSH 共用同一个终端挂载关口」（2026-10-09）。
    ///
    /// 需求侧的一句话是「AI 助手的展开内容支持鼠标选中，**支持本地终端**」——
    /// 要证的其实是「这套能力在本地 Shell 会话下同样成立」。它不是靠「我们没写
    /// SSH 专属判断」来讲的，而是靠**两条路共用同一个造视图的关口**：
    /// 本地 Shell（`local_shell_placed`）与 SSH（重连换屏 / 新标签）都经由
    /// `MainView::mount_terminal` 造出终端视图，而造视图的调用点**全仓只有一处**。
    ///
    /// 造视图只有一处 ⇒ 终端面板的行为（含 AI 面板的渲染路径 —— 它只读 `TraceRow`、
    /// 不看会话类型）对本地与 SSH **天然一致**。这条护栏钉住「关口只有一处」：
    /// 一旦有人给本地 Shell 另开一条造视图的路（比如复刻一份 `create_terminal_view`），
    /// 「支持本地终端」就重新变成「逐个功能去追」的问题。
    #[test]
    fn local_and_ssh_shells_share_the_same_terminal_mount_point() {
        let code = prod(include_str!("connections.rs"));

        // ① 本地 Shell 的两条落点（新开标签 / F-9 就地替换）都经由 `mount_terminal`。
        let local = region(
            code.as_str(),
            "fn local_shell_placed(",
            "fn local_shell_failed(",
        );
        assert_eq!(
            local.matches("mount_terminal(").count(),
            2,
            "本地 Shell 的两条落点（新开标签 / F-9 就地替换）都必须经由 `mount_terminal` —— \
             少一处说明有人给本地另开了一条造视图的路（会绕过 `TerminalPaneConfig` 解析）。"
        );

        // ② 造视图的关口只有一处：本文件里 `create_terminal_view(` 恰好一次调用。
        assert_eq!(
            code.matches("create_terminal_view(").count(),
            1,
            "视图构造必须只经由 `mount_terminal` 内的那一次 —— \
             长出第二处就是两套面板行为，本地与 SSH 会各走各的。"
        );

        // ③ 反面：SSH 侧（`main_view.rs`）**不自己造视图**，它也必须走 `mount_terminal`。
        let main = prod(include_str!("../main_view.rs"));
        assert!(
            !main.contains("create_terminal_view("),
            "SSH 侧不得直接造视图 —— 否则本地与 SSH 各有一套面板行为，\
             「支持本地终端」就变成逐个功能去追。"
        );
        assert!(
            main.matches("self.mount_terminal(").count() >= 1,
            "SSH 侧必须经由 `mount_terminal` 造视图（重连换屏 / 新标签）。"
        );
    }
}
