//! 独立 SFTP 标签页的动作入口（视图在 [`crate::sftp_tab`]）。
//!
//! 这里只放「点了按钮之后做什么」：两件事都落到**已有的** `SftpCmd` 上 ——
//! 上传走 `files_panel::enqueue_uploads`（与文件面板的拖入/上传同一入口），
//! 下载发 `Download` / `DownloadDir`。续传账本、校验、并发闸门都在 core 的
//! `sftp_resume` / `spawn_sftp_service` 里，本层不重复实现。
//!
//! 传输条本身不在这里画：`TransferQueued` 事件统一由 `poll` 收进
//! `io.files_transfers`，SFTP 标签页底部队列读的就是那份数据。

use gpui::{Context, Window};

use rustshell_core::sftp::{new_transfer_id, SftpCmd, SftpEvent};

use super::MainView;

impl MainView {
    /// 路径栏回车：按输入内容重扫远端目录。
    ///
    /// 归一化收在 core 的 `normalize_remote_input`（剥 `cd ` 前缀、去空白）——
    /// 与文件面板路径栏**同一个纯函数**，两处行为不会分叉。`~` 不在本机展开：
    /// 家目录只有服务器知道，`List` 交给远端 `canonicalize` 解析。
    pub(crate) fn sftp_goto_typed_path(&mut self, cx: &mut Context<Self>) {
        let raw = self.io.sftp_path_input.read(cx).value().trim().to_string();
        // 本次跳转后重新接受回填
        self.io.sftp_path_typed = false;
        let Some(tx) = self.active_sftp_tx() else {
            return;
        };
        let target = rustshell_core::sftp::normalize_remote_input(&raw)
            .unwrap_or_else(|| self.io.files_path.clone());
        self.io.files_loading = true;
        let _ = tx.try_send(SftpCmd::List { path: target });
        cx.notify();
    }

    /// 路径栏失焦：把草稿收回当前真实目录（与文件面板同一口径）。
    pub(crate) fn sftp_path_blur(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        if !self.io.sftp_path_typed {
            return;
        }
        self.io.sftp_path_typed = false;
        let current = self.io.files_path.clone();
        self.io
            .sftp_path_input
            .update(cx, |s, cx| s.set_value(current, w, cx));
        cx.notify();
    }

    /// 本地窗格进目录：换路径 + 清缓存 + 让路径框跟上。
    ///
    /// 手输中的标记要清掉，否则「双击进目录」之后路径框还停在用户半截输入上
    /// （`render` 看到 `sftp_local_path_typed = true` 就不回填）。
    pub(crate) fn sftp_local_enter(&mut self, path: String, cx: &mut Context<Self>) {
        // 统一在这一处收口：无论来路是双击进来的 `PathBuf` 串、还是手输后
        // `canonicalize` 出来的 verbatim 形式，落进状态前都剥掉 `\\?\`。
        let path = crate::sftp_tab::clean_local_path(&path);
        self.io.sftp_local_path = path.clone();
        self.io.sftp_local_for = None;
        self.io.sftp_local_path_typed = false;
        self.io.sftp_local_path_sync = Some(path);
        cx.notify();
    }

    /// 本地路径框回车：跳到手输的目录（相对路径按当前目录解析）。
    pub(crate) fn sftp_local_goto_typed_path(&mut self, cx: &mut Context<Self>) {
        let raw = self.io.sftp_local_path_input.read(cx).value().trim().to_string();
        self.io.sftp_local_path_typed = false;
        if raw.is_empty() {
            return;
        }
        let cur = std::path::Path::new(&self.io.sftp_local_path);
        let target = std::path::Path::new(&raw);
        let resolved = if target.is_absolute() {
            target.to_path_buf()
        } else {
            cur.join(target)
        };
        // 归一化掉 `..` / `.`（否则同一目录会以两种字符串身份来回跳）。
        // `canonicalize` 在 Windows 上返回的形式带 `\\?\` 前缀，那一段由
        // `sftp_local_enter` 统一剥掉 —— 地址栏里不该出现 API 层面的写法。
        let resolved = resolved
            .canonicalize()
            .unwrap_or(resolved)
            .to_string_lossy()
            .to_string();
        self.sftp_local_enter(resolved, cx);
    }

    /// 本地路径框失焦：把草稿还原成当前真实目录（与远端路径框同一口径）。
    pub(crate) fn sftp_local_path_blur(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        if !self.io.sftp_local_path_typed {
            return;
        }
        self.io.sftp_local_path_typed = false;
        let current = self.io.sftp_local_path.clone();
        self.io
            .sftp_local_path_input
            .update(cx, |s, cx| s.set_value(current, w, cx));
        cx.notify();
    }

    /// 当前激活会话的 SFTP 命令通道（没有就是 `None`）。
    pub(crate) fn active_sftp_tx(&self) -> Option<flume::Sender<SftpCmd>> {
        self.tabs_state
            .active
            .as_deref()
            .and_then(|sid| self.tabs_state.tabs.iter().find(|t| t.session.id == sid))
            .and_then(|t| t.session.sftp_tx.clone())
    }

    /// 本地某个文件 → 远端当前目录。
    ///
    /// 2026-10-27 起入口是**本地行右键菜单**（地址栏的上传图标按用户要求撤下）。
    /// 显式收路径而不是读 `sftp_local_sel`：右键点的那一行不一定等于上次单击选中的
    /// 那一行，读选中项会在「右键 A、删了 B」这种操作上错得毫无提示。
    ///
    /// 目录上传 v1 不做（与文件面板的 `enqueue_uploads` 同一口径）：静默跳过会让人
    /// 以为传过了，所以给一条**消息框**说明 —— 不再往远端列表里塞一行红字
    /// （2026-10-27 用户要求「提示不要在原来的位置显示」）。
    pub(crate) fn sftp_upload_path(&mut self, local: String, cx: &mut Context<Self>) {
        let Some(tx) = self.active_sftp_tx() else {
            self.queue_notice(
                crate::notice::NoticeKind::Warning,
                rustshell_core::i18n::t("需要 SSH 会话"),
            );
            return;
        };
        let path = std::path::PathBuf::from(&local);
        if path.is_dir() {
            self.queue_notice(
                crate::notice::NoticeKind::Warning,
                rustshell_core::i18n::t("暂不支持上传整个文件夹，请展开后逐个上传"),
            );
            return;
        }
        let dir = self.io.files_path.clone();
        let (queued, _skipped) = crate::files_panel::enqueue_uploads(&[path], &dir, &tx);
        if queued == 0 {
            self.queue_notice(
                crate::notice::NoticeKind::Warning,
                rustshell_core::i18n::t("没有可上传的文件"),
            );
        }
        cx.notify();
    }

    /// 远端某个条目 → 本地窗格的当前目录。
    ///
    /// 入口同样是右键菜单（远端行）。与文件面板的「下载…」不同：那条会让你选保存
    /// 到哪，这条直接落在左窗格正看着的目录里 —— 双窗格的意义就在这儿。
    ///
    /// 目录走 `DownloadDir`（core 支持递归），文件走 `Download`。
    /// `total` 起手给 0：远端大小由 `TransferMeta` 回填，猜一个反而会让进度条倒退。
    pub(crate) fn sftp_download_path(&mut self, remote: String, cx: &mut Context<Self>) {
        let Some(tx) = self.active_sftp_tx() else {
            return;
        };
        let dir = self.io.sftp_local_path.clone();
        let name = remote
            .rsplit('/')
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or("download")
            .to_string();
        let local = std::path::Path::new(&dir).join(&name);
        let is_dir = self
            .io
            .files_rows
            .iter()
            .find(|r| r.path == remote && r.is_dir)
            .is_some();
        let id = new_transfer_id();
        let label = if is_dir {
            rustshell_core::i18n::ts(
                format!("下载文件夹 {name}"),
                format!("Download folder {name}"),
            )
        } else {
            rustshell_core::i18n::ts(format!("下载 {name}"), format!("Download {name}"))
        };
        let _ = rustshell_core::sftp::sftp_events().0.send(SftpEvent::TransferQueued {
            transfer_id: id,
            name: label,
            total: 0,
        });
        let sent = if is_dir {
            tx.try_send(SftpCmd::DownloadDir {
                remote,
                local,
                transfer_id: id,
            })
        } else {
            tx.try_send(SftpCmd::Download {
                remote,
                local,
                transfer_id: id,
                total: 0,
                // 与文件面板同一口径：校验开关每次发起时现读设置
                verify: self.settings.sftp_verify_downloads,
            })
        };
        if sent.is_err() {
            self.queue_notice(
                crate::notice::NoticeKind::Error,
                rustshell_core::i18n::t("传输命令没发出去"),
            );
        }
        cx.notify();
    }

    /// 取消一条在途传输（传输队列行上的 ✕）。
    ///
    /// 命令只带 `transfer_id`：core 侧的登记表负责找到正在跑的那条命令，
    /// 因此这里**不需要**判断它排到没有（排队中的那条会在开工的第一个分片前退出，
    /// 见 core 的 `cancel_slot` 注释「预取消」）。
    pub(crate) fn sftp_cancel_transfer(&mut self, transfer_id: u64, cx: &mut Context<Self>) {
        let Some(tx) = self.active_sftp_tx() else {
            return;
        };
        if tx.try_send(SftpCmd::Cancel { transfer_id }).is_err() {
            self.queue_notice(
                crate::notice::NoticeKind::Warning,
                rustshell_core::i18n::t("取消请求没发出去，传输可能已经结束"),
            );
        } else if let Some(tr) = self
            .io
            .files_transfers
            .iter_mut()
            .find(|t| t.id == transfer_id)
        {
            // 立刻给反馈：排队中的那条要等闸门放行才会真正退出（见 `TransferState::cancelling`）
            tr.cancelling = true;
        }
        cx.notify();
    }
}
