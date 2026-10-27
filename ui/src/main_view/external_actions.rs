//! 外部链接域：`ssh://` / `sftp://` 打开的**临时**连接。
//!
//! 三条硬约束，全部收在这个模块的入口上：
//! 1. **不进主机列表**：`ConnectionProfile` 只塞进 `ConnUiState::external`，
//!    从不写 `ConnectionStore` / `connections.json`；
//! 2. **口令只在内存里**：链接里的口令随 profile 存在那张表里，标签关闭时
//!    `close_tab_by_session` 删表项，口令随之消失；
//! 3. **一条连接一个通道**：`ssh://` 只要 shell、`sftp://` 只要 sftp 子系统
//!    （见 [`rustshell_core::ssh::SshChannels`]），不允许多开通道的堡垒机也能用。
//!
//! 会话快照（F-9）同样排除 `ext:` 开头的 id（见 `layout_persist::layout_snapshot`）：
//! 临时连接连「连哪台」都不该跨重启保留。

use gpui::{App, Context};

use rustshell_core::connections::ConnectionProfile;
use rustshell_core::launch::{parse_args, LaunchTarget, EXTERNAL_PROFILE_PREFIX};

use super::ui_state::ExternalConn;
use super::{MainView, PanelKind};

/// 单实例转交过来的启动参数通道（主实例持接收端）。
pub struct LaunchRequests(pub flume::Receiver<Vec<String>>);

impl gpui::Global for LaunchRequests {}

/// 装进全局：`shell/src/app.rs` 在开窗之前调用一次。
pub fn ui_install_launch_requests(cx: &mut App, requests: flume::Receiver<Vec<String>>) {
    cx.set_global(LaunchRequests(requests));
}

impl MainView {
    /// 连接树 / 外部链接共用的 profile 查找：已保存的优先，其次临时的。
    pub(super) fn lookup_profile(&self, id: &str) -> Option<ConnectionProfile> {
        self.conn_store
            .profile(id)
            .cloned()
            .or_else(|| self.conn_ui.external.get(id).map(|e| e.profile.clone()))
    }

    /// 消费单实例转交的启动参数（`poll` 每轮调用）。
    ///
    /// 解析失败 / 不认识的参数由 `parse_args` 静默忽略：堡垒机启动器会塞一堆
    /// 本机不认识的开关，为一个陌生参数拒绝启动的表现就是「点了没反应」。
    pub(super) fn drain_launch_requests(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(requests) = cx.try_global::<LaunchRequests>().map(|g| g.0.clone()) else {
            return false;
        };
        let mut targets: Vec<LaunchTarget> = Vec::new();
        let mut got_request = false;
        while let Ok(args) = requests.try_recv() {
            got_request = true;
            targets.extend(parse_args(args));
        }
        if !got_request {
            return false;
        }
        for target in targets {
            self.connect_external(target, cx);
        }
        // 视窗可能被别的程序盖着：把本窗口提到前面（`poll` 没有 Window，下一帧兑现）。
        // 即使这一轮没有可解析的链接（例如重复双击 exe、只带堡垒机私有开关），
        // 「用户刚刚点了这个程序」也应当得到回应。
        self.panel_ui.pending_activate_window = true;
        true
    }

    /// 把一条外部链接变成临时连接 + 占位 Tab，然后后台连。
    fn connect_external(&mut self, target: LaunchTarget, cx: &mut Context<Self>) {
        let name = target.name.clone().unwrap_or_else(|| {
            if target.username.is_empty() {
                target.host.clone()
            } else {
                format!("{}@{}", target.username, target.host)
            }
        });
        let mut profile = ConnectionProfile::new(name, target.host.clone(), target.username);
        profile.id = format!(
            "{EXTERNAL_PROFILE_PREFIX}{}",
            rustshell_core::connections::uuid_like()
        );
        profile.port = target.port;
        // 口令原样留在这个 profile 里：唯一的副本，随表项/标签一起消失。
        profile.password = target.password;
        let pid = profile.id.clone();
        self.conn_ui.external.insert(
            pid.clone(),
            ExternalConn {
                profile: profile.clone(),
                kind: target.kind,
                path: target.path,
            },
        );
        log::info!(
            "外部链接连接: {} ({}@{}:{}) kind={:?}",
            profile.name,
            profile.username,
            profile.host,
            profile.port,
            target.kind
        );
        // 与正常连接共用同一条收口：占位 Tab、在途守卫、单通道 / 不写日志的判定
        // 都在 `connect_profile_now` 里（外部链接的身份由刚插入的表项决定）。
        self.connect_profile_now(profile, cx);
    }

    /// 单通道 SFTP 会话连上后的落地：切到文件面板并列出目标目录（缺省 = 家目录）。
    ///
    /// 两条来路共用它：
    /// - 外部 `sftp://` 链接（`path` = 链接里的路径）；
    /// - 2026-10-26 起：连接树右键「打开 SFTP」对**已保存**的连接发起的那条
    ///   （`path` = `None`，从家目录开始）。
    ///
    /// 用 `active_tab()` 取通道：调用点都刚把目标 Tab 设为激活（见
    /// `connect_results` 的 `sftp_only` 分支与 [`super::MainView::open_sftp`]），
    /// 所以这里不需要再传 session id。
    pub(super) fn external_sftp_first_scan(
        &mut self,
        path: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let Some((sid, tx)) = self.active_tab().and_then(|t| {
            t.session
                .sftp_tx
                .clone()
                .map(|tx| (t.session.id.clone(), tx))
        }) else {
            return;
        };
        self.io.files_loading = true;
        self.io.files_error = None;
        self.io.files_loaded_session = Some(sid);
        self.panel_ui.panel = PanelKind::Files;
        self.panel_ui.sidebar_collapsed = false;
        let _ = match path.filter(|p| !p.is_empty()) {
            Some(path) => tx.try_send(rustshell_core::sftp::SftpCmd::List { path }),
            None => tx.try_send(rustshell_core::sftp::SftpCmd::ListHome),
        };
        cx.notify();
    }
}
