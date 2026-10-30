//! SFTP 文件服务（文件面板）：russh-sftp 客户端封装。

use russh::client::Handler;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use crate::sftp_resume::{plan_resume, resume_store, ResumeMark, ResumePlan, TransferDir};

/// 文件面板默认远端路径的**兜底值**：`/`（文件系统根）。
///
/// 2026-09-20 用户口径变更（第三次）：默认落点是**远端登录用户 home 的绝对路径**
/// —— 既不是本机家目录（`~` 在 UI 进程里必然展开错，见 [`expand_tilde`]），
/// 也不是文件系统根。真正的解析由 [`SftpCmd::ListHome`] 交给服务器完成。
///
/// 本常量因此只剩两个用途：
/// 1. 路径栏在首个 [`SftpEvent::Listing`] 到达前的**占位初值**（`MainView::new`）；
/// 2. home 解析失败时的**兜底落点** —— 宁可从根多按一次「上级」，
///    也不要停在一个来路不明的目录上。
///
/// 它**不再**是「默认路径」本身：谁要是把首扫写回本常量，默认落点就不再是
/// 服务器的家目录了（护栏 `files_panel_default_path_resolves_the_remote_home`）。
pub const DEFAULT_REMOTE_PATH: &str = "/";

/// 目录条目（UI 展示用）
#[derive(Debug, Clone)]
pub struct SftpEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    /// mtime（unix 秒）
    pub modified: u64,
    /// 权限位（`0o7777` 部分；含文件类型位的原始值请用 [`entry_mode`] 格式化）。
    ///
    /// `None` = 服务端没在 `SSH_FXP_NAME` 里带权限（协议允许省略）。这与
    /// 「权限是 `000`」是两件事，UI 必须能区分：前者显示空白，后者显示 `---------`。
    pub permissions: Option<u32>,
    /// 属主名（`SSH_FXP_NAME` 的 `user` 字段；服务端只给数字 id 时为 `None`）
    pub user: Option<String>,
    pub uid: Option<u32>,
    /// 属组名（同上）
    pub group: Option<String>,
    pub gid: Option<u32>,
}

impl SftpEntry {
    /// 只带基本字段的构造（测试与「只要名字/大小」的调用点用）：
    /// 权限与属主一律置为「服务端未提供」。
    pub fn new(name: impl Into<String>, is_dir: bool, size: u64, modified: u64) -> Self {
        Self {
            name: name.into(),
            is_dir,
            size,
            modified,
            permissions: None,
            user: None,
            uid: None,
            group: None,
            gid: None,
        }
    }
}

/// 权限位 → `ls -l` 风格的 **9 字符**串（`rwxr-xr-x`）。
///
/// 只取低 12 位里的三段权限（setuid/setgid/sticky 不在此列，与多数 SFTP 客户端
/// 的简化口径一致 —— 它们在这份列表里没有读者）。含文件类型位的原始
/// `permissions` 要先 `& 0o777`，本函数自己也会做这一步，所以传哪个都对。
pub fn format_mode(perms: u32) -> String {
    let bits = perms & 0o777;
    // 显式位表：一位一个掩码，读代码时不必心算移位
    const TABLE: [(u32, char); 9] = [
        (0o400, 'r'),
        (0o200, 'w'),
        (0o100, 'x'),
        (0o040, 'r'),
        (0o020, 'w'),
        (0o010, 'x'),
        (0o004, 'r'),
        (0o002, 'w'),
        (0o001, 'x'),
    ];
    TABLE
        .iter()
        .map(|(mask, ch)| if bits & mask != 0 { *ch } else { '-' })
        .collect()
}

/// 条目的权限展示串：**10 字符**，首字符是类型（`d` 目录 / `-` 普通文件）。
///
/// 服务端没给权限时返回**空串**（而不是 `----------`）：那代表「不知道」，
/// 画成 `---------` 会被读成「谁都不能访问」，比空白更容易误导。
pub fn entry_mode(entry: &SftpEntry) -> String {
    let Some(perms) = entry.permissions else {
        return String::new();
    };
    let mut s = String::with_capacity(10);
    s.push(if entry.is_dir { 'd' } else { '-' });
    s.push_str(&format_mode(perms));
    s
}

/// 属主展示串（**只有属主**，不含属组）。
///
/// 2026-10-30 用户要求「所有者/组取值与 shell-rs 一致」：那边「所有者」一列写的是
/// 用户名（截图里是 `testuser`），不是 `user:group`。列头叫「所有者」，塞两段进去
/// 既对不上列名，也在窄列里先被截掉一半。
///
/// 优先用服务端给的名字；只给数字 id 时退回 `uid`（`ls -n` 的效果）；都没有返回空串。
pub fn entry_owner(entry: &SftpEntry) -> String {
    entry
        .user
        .clone()
        .filter(|s| !s.is_empty())
        .or_else(|| entry.uid.map(|n| n.to_string()))
        .unwrap_or_default()
}

/// 远端路径拼接（SFTP 一律 `/` 分隔；目录已带尾斜杠时不重复补）。
pub fn join_remote(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// 远端文件的**所在目录**（POSIX 语义，只认 `/`）。
///
/// 用途单一：上传结束后请 UI 重扫「刚写进哪个目录」。与 UI 侧的
/// `sftp_tab::remote_parent` 是同一个语义，但**`..` 那一行用的是它自己的口径** ——
/// 这里不借 `std::path::Path`：`Path` 的分隔符跟着**本机**平台走，
/// 用它处理远端路径在 Windows 上会得到错误结果（`/home/u/a` 整串当文件名）。
pub fn remote_dir(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(ix) => trimmed[..ix].to_string(),
    }
}

/// 单个远端文件名的预校验结果（只覆盖**几乎必然失败**的输入）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameCheck {
    Empty,
    /// 含 `/`：`MKDIR`/`RENAME` 会把它当路径分隔符，建/改出来的东西不在用户以为的位置
    ContainsSlash,
    /// `.` / `..`：协议层不拦，落到服务端就是「改到父目录」这类越界操作
    Reserved,
}

/// 校验一个**名字**（不是路径）：非空、不含 `/`、不是 `.` / `..`。
///
/// 为什么要在本地先拦：`SftpCmd::Mkdir` / `Rename` 出错会让 `run_cmd` 返回 `Err`，
/// 而 `spawn_sftp_service` 对 `Err` 的处理是**丢掉整个 SFTP 会话**（它假设出错 =
/// 会话可能已失效）。用户敲错一个名字的代价因此是「面板断线重连」，
/// 远大于在本地拦一下。
///
/// 只做这三条：其余（重名、无权限、名字过长）只有服务端知道，报错本来就该来自它。
pub fn check_entry_name(name: &str) -> Result<(), NameCheck> {
    if name.is_empty() {
        return Err(NameCheck::Empty);
    }
    if name.contains('/') {
        return Err(NameCheck::ContainsSlash);
    }
    if name == "." || name == ".." {
        return Err(NameCheck::Reserved);
    }
    Ok(())
}

/// 把用户在文件面板路径栏里敲的内容规范成 SFTP 能接收的路径。
///
/// 支持三种写法（2026-09-17 运行反馈「默认为 home 路径，cd ~ 的路径」）：
/// - 直接路径：`/var/log`、`.`、`sub`
/// - `~` 开头：`~`、`~/logs` —— **原样透传**，由 SFTP 侧用远端家目录展开
///   （见 [`expand_tilde`]）。2026-09-20 改：以前在这里用 `dirs::home_dir()`
///   展开成本机家目录，Windows 上会把 `C:/Users/xxx` 送给 Linux 服务器。
/// - 带 `cd ` 前缀：`cd ~`、`cd /var/log` —— 用户习惯从终端里抄一条命令过来
///
/// 空串返回 `None`，调用方据此回退到当前目录。
pub fn normalize_remote_input(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    // `cd <path>`：剥掉命令前缀。`cd` 与路径之间允许任意空白。
    let stripped = trimmed
        .strip_prefix("cd")
        .map(str::trim_start)
        .unwrap_or(trimmed);
    // 单独一个 `cd`：终端语义即 `cd ~`（回自己的家目录），落到 `~` 让远端展开
    if stripped.is_empty() {
        return (trimmed == "cd").then(|| "~".to_string());
    }
    Some(stripped.to_string())
}

/// 把 `~` / `~/sub` 用**远端**家目录展开成绝对路径；其余形态原样返回。
///
/// `home` 由 [`remote_home`] 向服务器要来 —— 展开必须发生在能看见那台机器的一侧。
/// `~user` 不展开（要查远端 passwd，SFTP 协议不提供）：原样透传，由服务端报错。
pub fn expand_tilde(path: &str, home: &str) -> String {
    if path == "~" {
        home.to_string()
    } else if let Some(rest) = path.strip_prefix("~/") {
        join_remote(home, rest.trim_start_matches('/'))
    } else {
        path.to_string()
    }
}

/// 文件修改时间的展示串：今年内 `MM-DD HH:MM`，跨年 `YYYY-MM-DD`。
/// mtime 为 0（服务端没给）时返回空串，调用方据此不画这一列 —— 显示
/// `1970-01-01` 比不显示更糟。按本地时区换算（用户看到的是自己的时钟）。
pub fn format_mtime(unix_secs: u64) -> String {
    format_mtime_at(unix_secs, &chrono::Local::now())
}

/// [`format_mtime`] 的「现在由调用方给」版本：批量格式化一整个目录时，
/// `chrono::Local::now()` 只需要取**一次**。
///
/// 目录列表动辄上千行，逐行取系统时钟 + 逐行格式化年份是纯浪费
/// （《性能与帧率优化分析》P0-2 的第 ② 条放大项）。
pub fn format_mtime_at(unix_secs: u64, now: &chrono::DateTime<chrono::Local>) -> String {
    use chrono::TimeZone as _;
    if unix_secs == 0 {
        return String::new();
    }
    let Some(t) = chrono::Local.timestamp_opt(unix_secs as i64, 0).single() else {
        return String::new();
    };
    if t.format("%Y").to_string() == now.format("%Y").to_string() {
        t.format("%m-%d %H:%M").to_string()
    } else {
        t.format("%Y-%m-%d").to_string()
    }
}

/// 一批条目的 mtime 展示串（顺序与入参一一对应）。
///
/// 存在的理由只有一个：把 `chrono::Local::now()` 从「每行一次」提到「每批一次」。
pub fn format_entry_mtimes(entries: &[SftpEntry]) -> Vec<String> {
    let now = chrono::Local::now();
    entries
        .iter()
        .map(|e| format_mtime_at(e.modified, &now))
        .collect()
}

/// UI 到 SFTP 任务命令
pub enum SftpCmd {
    List {
        path: String,
    },
    /// 首扫：落到**远端登录用户 home 的绝对路径**并列出（见 [`DEFAULT_REMOTE_PATH`]）。
    ///
    /// 为什么不是一个「算出 home 再发 `List`」的两步：home 只有服务器知道。
    /// 本命令让 SFTP 服务在**原会话**上 `canonicalize(".")` 问一次服务端的
    /// 当前工作目录 —— OpenSSH 启动 `sftp-server` 时就 chdir 到该用户的 home
    /// —— 拿到 `/home/ubuntu`、`/root` 这样的绝对路径，再作为
    /// [`SftpEvent::Listing::path`] 回报。UI 侧因此拿到的是可直接编辑的真实路径，
    /// 而不是 `~` 这种只有 shell 才认识的记号。
    ///
    /// 解析失败退回 [`DEFAULT_REMOTE_PATH`]：面板仍然可用，只是起点在根。
    ListHome,
    /// 下载：远端 到 本地（transfer_id 用于 UI 进度关联）
    Download {
        remote: String,
        local: PathBuf,
        transfer_id: u64,
        total: u64,
        /// 完成后校验（大小恒校验 + SHA-256）。由 UI **每次发起时现读设置**传进来 ——
        /// 服务任务在连接建立时就固定了参数，走那条路的话改了设置得重连才生效。
        verify: bool,
    },
    /// 下载整个目录（递归）：远端目录树 到 本地目录。
    /// 总量在服务端走完目录后经 `TransferMeta` 回填（UI 起手不知道有多少字节）。
    DownloadDir {
        remote: String,
        local: PathBuf,
        transfer_id: u64,
    },
    /// 上传：本地 到 远端
    Upload {
        local: PathBuf,
        remote: String,
        transfer_id: u64,
        total: u64,
    },
    Mkdir {
        path: String,
        refresh: String,
    },
    Remove {
        path: String,
        is_dir: bool,
        /// 目录是否递归删除（连同其下所有文件与子目录）。
        ///
        /// 为什么是一个显式开关而不是「目录一律递归」：非递归删除是**唯一**能删掉
        /// 单个空目录而不触碰其内容的口径，而 SFTP 协议只提供 `RMDIR`（要求空目录）。
        /// 递归由本层实现（协议没有 `rm -rf`），所以「删到什么程度」必须由调用方
        /// 明确说 —— 界面上那个「递归删除」勾选框就是它的唯一来源。
        recursive: bool,
        refresh: String,
    },
    Rename {
        from: String,
        to: String,
        refresh: String,
    },
    /// 取消一条传输（2026-10-27 用户要求「支持取消任务」）。
    ///
    /// 只带 `transfer_id`：取消是**针对某条在途传输**的指令，与服务端状态无关，
    /// 因此不需要会话、不需要路径，也不该被并发闸门挡在后面（见
    /// `spawn_sftp_service` 里对它的前置处理）。
    Cancel {
        transfer_id: u64,
    },
}

/// SFTP 任务 到 UI 事件
#[derive(Debug, Clone)]
pub enum SftpEvent {
    Listing {
        path: String,
        entries: Vec<SftpEntry>,
    },
    Error {
        message: String,
    },
    Progress {
        transfer_id: u64,
        bytes: u64,
    },
    /// 传输入队（UI 侧补一条进度条；由文件选择器后台任务发出）
    TransferQueued {
        transfer_id: u64,
        name: String,
        total: u64,
    },
    TransferDone {
        transfer_id: u64,
        ok: bool,
        message: String,
    },
    /// 传输元信息回填：下载 total 由远端 stat 补齐；UI 按 id 更新现有条目
    TransferMeta {
        transfer_id: u64,
        total: u64,
    },
    /// 传输条上的一行备注（续传生效 / 守卫不通过从头传 / 跳过了已传完的文件）。
    ///
    /// 用一条通用备注而不是 `Resumed` 专用事件：几种情形都要在队列条目上可见，
    /// 且目录下载的「跳过 N 个」这类统计以后也不必再加事件。
    TransferNote {
        transfer_id: u64,
        text: String,
    },
    /// 操作完成，请求刷新指定目录
    Refresh {
        path: String,
    },
}

pub fn sftp_events() -> &'static (flume::Sender<SftpEvent>, flume::Receiver<SftpEvent>) {
    static CH: OnceLock<(flume::Sender<SftpEvent>, flume::Receiver<SftpEvent>)> = OnceLock::new();
    CH.get_or_init(flume::unbounded)
}

fn next_transfer_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// 传输类命令：受并发信号量约束（其余命令即时串行执行，不被大文件传输饿死）。
fn is_transfer_cmd(cmd: &SftpCmd) -> bool {
    matches!(
        cmd,
        SftpCmd::Download { .. } | SftpCmd::Upload { .. } | SftpCmd::DownloadDir { .. }
    )
}

// ── 传输取消（2026-10-27）─────────────────────────────────────────────────

/// 用户点了「取消」之后，传输循环里抛出的中止原因。
///
/// 与真正的 IO 失败共用 `Err(String)` 通道是刻意的：取消也是「这次传输没成功」，
/// 走同一条路才会自动带上续传记账（半成品可续）、`TransferDone{ok:false}` 与队列收尾。
pub const CANCELLED: &str = "传输已取消";

/// 在途传输的取消旗标（`transfer_id` → 旗标）。
///
/// 为什么是**全局表**而不是给每条命令额外塞一个 `Arc<AtomicBool>`：取消是「另一条
/// 命令」，它必须能摸到**已经在跑**的那条传输 —— 命令通道只承载单向指令，没有回指
/// 在途任务的通道。旗标本身是 `Arc<AtomicBool>`：读写循环每个分片只做一次
/// `Relaxed` load，不必为了「用户有没有点取消」去抢一把全局锁。
///
/// `transfer_id` 由 UI 侧生成（[`new_transfer_id`]），全局唯一 ⇒ 不需要会话维度。
fn cancel_slots() -> &'static Mutex<HashMap<u64, Arc<AtomicBool>>> {
    static SLOTS: OnceLock<Mutex<HashMap<u64, Arc<AtomicBool>>>> = OnceLock::new();
    SLOTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 取（或建）某条传输的取消旗标。
///
/// 「或建」而不是「只取」是**预取消**的关键：`Cancel` 常常先于传输本体到达
/// （命令排在并发闸门后面还没开始跑），此时先把旗子插上并置位，等传输真正开工时
/// 取到的就是那面已经倒下的旗 —— 它在第一个分片前就退出。
fn cancel_slot(transfer_id: u64) -> Arc<AtomicBool> {
    let mut slots = cancel_slots()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    slots.entry(transfer_id).or_default().clone()
}

/// 请求取消某条传输（幂等：重复点只置一次位）。
///
/// 表里查不到也照样登记 —— 那就是上面说的「预取消」。代价是「取消一个从未开始的
/// `transfer_id`」会留下一条常量大小的空记录；取消是低频动作，不值得为它加一层
/// 过期清理。
pub fn request_cancel(transfer_id: u64) {
    cancel_slot(transfer_id).store(true, Ordering::Relaxed);
}

/// 传输收尾时把旗标收走（成功 / 失败 / 取消都走这里）。
fn forget_cancel(transfer_id: u64) {
    let mut slots = cancel_slots()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    slots.remove(&transfer_id);
}

/// SFTP 子系统会话的「取或建」单元：会话按需建立、失败即作废。
///
/// 并发传输（F-13）之后，会话不再能由单个任务独占了 —— 多个任务要拿到同一个
/// `Arc<SftpSession>`（`SftpSession` 的方法均 `&self`，请求按 id 多路复用），
/// 而「谁去开子系统」这件事必须收口，否则几条传输同时发现会话失效会各开一条。
struct SessionCell<H: Handler + 'static> {
    handle: std::sync::Arc<russh::client::Handle<H>>,
    cell: tokio::sync::Mutex<Option<std::sync::Arc<russh_sftp::client::SftpSession>>>,
}

impl<H: Handler + 'static> SessionCell<H> {
    fn new(handle: std::sync::Arc<russh::client::Handle<H>>) -> Self {
        Self {
            handle,
            cell: tokio::sync::Mutex::new(None),
        }
    }

    /// 取当前会话；没有（首次，或上次失败已作废）就地建立。
    ///
    /// 锁**跨越建链**是有意的：并发传输同时发现会话失效时，只该有一个去开子系统，
    /// 其余等它开完复用。每次命令多花一次加锁 + `Arc` 克隆，换的是不会开出多余信道。
    async fn get(&self) -> Result<std::sync::Arc<russh_sftp::client::SftpSession>, String> {
        let mut guard = self.cell.lock().await;
        if let Some(s) = guard.as_ref() {
            return Ok(s.clone());
        }
        let s = std::sync::Arc::new(open_sftp(&self.handle).await?);
        *guard = Some(s.clone());
        Ok(s)
    }

    /// 作废 `stale`：只有**当前存的就是它**才清空。
    ///
    /// 按指针比对而不是「一律清空」：另一个任务可能已经建好了新会话，
    /// 拿旧指针来清会把新会话一起丢掉（下一条命令白开一次子系统）。
    async fn invalidate(&self, stale: &std::sync::Arc<russh_sftp::client::SftpSession>) {
        let mut guard = self.cell.lock().await;
        if guard
            .as_ref()
            .is_some_and(|s| std::sync::Arc::ptr_eq(s, stale))
        {
            *guard = None;
        }
    }
}

/// 为已认证 SSH 会话启动 SFTP 常驻任务（Arc<Handle> 共享给指标任务，方法均为 &self）。
///
/// `max_concurrent`：传输类命令的并发上限（`1` = 既有串行语义）。由调用方从设置
/// 传入 —— core 不读设置文件，与 `log_path` 是同一种手法。改这个值不影响已建立的
/// 会话（信号量在这里创建，设置项描述里写明了「新建连接生效」）。
pub fn spawn_sftp_service<H: Handler + 'static>(
    handle: std::sync::Arc<russh::client::Handle<H>>,
    max_concurrent: u32,
) -> flume::Sender<SftpCmd> {
    let (tx, rx) = flume::bounded::<SftpCmd>(16);
    let limit = max_concurrent.clamp(
        crate::settings::SFTP_MAX_CONCURRENT_MIN,
        crate::settings::SFTP_MAX_CONCURRENT_MAX,
    );
    crate::ssh::runtime().spawn(async move {
        // 常驻复用 SFTP 子系统会话，避免每条命令都 channel_open + 子系统
        // 握手（目录频繁浏览时延迟明显）；失败即作废，下条命令自动重开
        let sessions = std::sync::Arc::new(SessionCell::new(handle));
        // 传输闸门：只给传输类命令发通行证。浏览/改名/删除不取许可 ⇒ 传输满载时
        // 面板依然可浏览（可用性优先于吞吐）。
        let gate = std::sync::Arc::new(tokio::sync::Semaphore::new(limit as usize));
        while let Ok(cmd) = rx.recv_async().await {
            // 取消**先于一切**处理：它不碰服务端，也就不该等并发闸门的通行证，
            // 更不该为了置一面旗去开一条 SFTP 子系统会话。传输满载（闸门放空）时
            // 这条命令照样即时生效 —— 否则「点了取消没反应」，而那一刻正是用户最想
            // 取消的时候。
            if let SftpCmd::Cancel { transfer_id } = &cmd {
                request_cancel(*transfer_id);
                continue;
            }
            let sessions = sessions.clone();
            if is_transfer_cmd(&cmd) {
                let gate = gate.clone();
                crate::ssh::runtime().spawn(async move {
                    // 拿不到许可就排队等（不丢命令）。信号量活到本任务结束，
                    // 期间不会被关闭，`acquire_owned` 不会返回 Err。
                    let Ok(_permit) = gate.acquire_owned().await else {
                        return;
                    };
                    let session = match sessions.get().await {
                        Ok(s) => s,
                        Err(e) => {
                            let _ = sftp_events().0.send(SftpEvent::Error { message: e });
                            return;
                        }
                    };
                    // 传输失败已在 `finish_transfer` 里探活过：会话活着 ⇒ 只报
                    // `TransferDone{ok:false}`（`Ok`），不牵连其他在途传输。
                    // 这里拿到 `Err` 才是子系统真死了 ⇒ 作废会话，下条命令重开。
                    if let Err(e) = run_cmd(&session, &cmd).await {
                        sessions.invalidate(&session).await;
                        let _ = sftp_events().0.send(SftpEvent::Error { message: e });
                    }
                });
            } else {
                let session = match sessions.get().await {
                    Ok(s) => s,
                    Err(e) => {
                        let _ = sftp_events().0.send(SftpEvent::Error { message: e });
                        continue;
                    }
                };
                if let Err(e) = run_cmd(&session, &cmd).await {
                    // 出错后会话可能已失效（断网/服务端关闭）：作废触发下条重开
                    sessions.invalidate(&session).await;
                    let _ = sftp_events().0.send(SftpEvent::Error { message: e });
                }
            }
        }
    });
    tx
}

async fn open_sftp<H: Handler + 'static>(
    handle: &russh::client::Handle<H>,
) -> Result<russh_sftp::client::SftpSession, String> {
    let channel = handle
        .channel_open_session()
        .await
        .map_err(|e| format!("打开信道失败: {e}"))?;
    channel
        .request_subsystem(true, "sftp")
        .await
        .map_err(|e| format!("请求 sftp 子系统失败: {e}"))?;
    russh_sftp::client::SftpSession::new(channel.into_stream())
        .await
        .map_err(|e| format!("SFTP 初始化失败: {e}"))
}

/// 远端登录用户的家目录**绝对路径**（拿不到即 `None`）。
///
/// `canonicalize(".")`：`.` 是 SFTP 会话的当前工作目录，OpenSSH 的 `sftp-server`
/// 启动时 chdir 到该用户的 home ⇒ `/home/ubuntu`、`/root`。服务端把 cwd 配到别处
/// （chroot、`-d`）时结论照样成立 —— 那正是 `.` 的真实位置。
/// 返回的不是绝对路径（畸形/代理实现）时也当拿不到：宁可退回根。
async fn remote_home(sftp: &russh_sftp::client::SftpSession) -> Option<String> {
    let home = sftp.canonicalize(".").await.ok()?;
    home.starts_with('/').then_some(home)
}

/// 列目录：先 `canonicalize` 落成绝对路径（相对路径、`.`、`..` 都靠它归一），
/// 再读目录并按「目录优先、同名不区分大小写」排序。
///
/// [`SftpCmd::List`] 与 [`SftpCmd::ListHome`] 共用 —— 过滤与排序只有一处实现。
async fn list_entries(
    sftp: &russh_sftp::client::SftpSession,
    path: &str,
) -> Result<(String, Vec<SftpEntry>), String> {
    let abs = sftp
        .canonicalize(path)
        .await
        .map_err(|e| format!("解析路径 {path} 失败: {e}"))?;
    let read = sftp
        .read_dir(&abs)
        .await
        .map_err(|e| format!("读取目录 {abs} 失败: {e}"))?;
    let mut entries: Vec<SftpEntry> = read
        .into_iter()
        .filter(|e| e.file_name() != "." && e.file_name() != "..")
        .map(|e| {
            let md = e.metadata();
            SftpEntry {
                name: e.file_name(),
                is_dir: matches!(e.file_type(), russh_sftp::protocol::FileType::Dir),
                size: md.size.unwrap_or(0),
                modified: md.mtime.map(|t| t as u64).unwrap_or(0),
                // 权限 / 属主是**可选**的：OpenSSH 会给全，但协议允许省略，
                // 每条都保留 `None` 语义，UI 才能区分「未知」与「000」。
                permissions: md.permissions,
                user: md.user.clone(),
                uid: md.uid,
                group: md.group.clone(),
                gid: md.gid,
            }
        })
        .collect();
    // 目录优先，同名按字典序
    entries.sort_by(|a, b| match (b.is_dir, a.is_dir) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
    });
    Ok((abs, entries))
}

/// 递归删除的**执行顺序**：先删文件，再删目录，且目录必须**从深到浅**。
///
/// 入参是深度优先**前序**扫描结果（父目录一定排在它的子项之前，见 `scan_tree`），
/// 出参是可直接顺序执行的 `(路径, 是否目录)` 列表。抽成纯函数是为了能单测顺序 ——
/// 顺序写错的表现是「非空目录删不掉」（`RMDIR` 撞上还没删空的目录），
/// 而那需要真连一台服务器、真造一棵树才能复现，单测里几乎不会有人去覆盖。
pub fn removal_order(scan: &[(String, bool)]) -> Vec<(String, bool)> {
    let mut files: Vec<String> = Vec::new();
    let mut dirs: Vec<String> = Vec::new();
    for (path, is_dir) in scan {
        if *is_dir {
            dirs.push(path.clone());
        } else {
            files.push(path.clone());
        }
    }
    // 前序的逆序 = 每个目录都在它的子目录之后出现（子先父后）
    dirs.reverse();
    files
        .into_iter()
        .map(|p| (p, false))
        .chain(dirs.into_iter().map(|p| (p, true)))
        .collect()
}

/// 深度优先**前序**走一遍远端目录树（父在子前，含根自身）。
///
/// 用显式工作栈而不是递归：路径深度不受调用栈限制（与 `DownloadDir` 同一手法）。
/// 走完才返回 —— 调用方（`rm -rf`）因此能在**动手前**知道规模，超限就整体放弃，
/// 不会留下删了一半的树。
async fn scan_tree(
    sftp: &russh_sftp::client::SftpSession,
    root: &str,
) -> Result<Vec<(String, bool)>, String> {
    /// 单次递归删除的规模上限。超过就整体中止 —— 删一半比不删更糟。
    const MAX_ENTRIES: usize = 20_000;
    let mut out: Vec<(String, bool)> = Vec::new();
    let mut stack: Vec<String> = vec![root.to_string()];
    // 超限即整体放弃（而不是边走边删）：留一棵删了一半的树比报错更糟。
    let guard = |n: usize| -> Result<(), String> {
        if n > MAX_ENTRIES {
            Err(format!(
                "目录过大（超过 {MAX_ENTRIES} 项），已中止且**未删除任何内容**"
            ))
        } else {
            Ok(())
        }
    };
    while let Some(dir) = stack.pop() {
        out.push((dir.clone(), true));
        guard(out.len())?;
        let read = sftp
            .read_dir(&dir)
            .await
            .map_err(|e| format!("读取目录 {dir} 失败: {e}"))?;
        for e in read {
            let name = e.file_name();
            if name == "." || name == ".." {
                continue;
            }
            let child = join_remote(&dir, &name);
            if matches!(e.file_type(), russh_sftp::protocol::FileType::Dir) {
                stack.push(child);
            } else {
                out.push((child, false));
            }
        }
        guard(out.len())?;
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// F-13：续传 / 校验 的传输原语
//
// 传输的失败处理与目录类命令相反：单个文件传不动**不是**会话故障
// （权限不够、远端空间满、源被改都可能），所以传输主体把错误收成
// `TransferDone{ok:false}`，只有探活也失败时才把 `Err` 交回上层去重建会话
// （见 `finish_transfer`）。
// ---------------------------------------------------------------------------

/// 传输分片大小（与既有读写循环一致）。
const CHUNK: usize = 64 * 1024;

/// 本地文件长度（不存在 / 取不到按 0）。
async fn local_len(path: &Path) -> u64 {
    tokio::fs::metadata(path)
        .await
        .map(|m| m.len())
        .unwrap_or(0)
}

/// 远端文件长度；`None` = 取不到（文件不存在或服务端不支持 stat）。
async fn remote_len(sftp: &russh_sftp::client::SftpSession, path: &str) -> Option<u64> {
    sftp.metadata(path).await.ok().and_then(|m| m.size)
}

/// 「已传 X%」文案；总量未知（0）时退回字节数。
fn progress_text(done: u64, total: u64) -> String {
    // `checked_div` 而不是 `if total > 0`：总量为 0（下载时远端 stat 拿不到大小、
    // 上传时本地文件为空）是常态，去零除要有一个**看得见**的出口。
    match done
        .min(total)
        .checked_mul(100)
        .and_then(|p| p.checked_div(total))
    {
        Some(pct) => format!("{pct}%"),
        None => format!("{done} 字节"),
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// 一段字节的 SHA-256 十六进制摘要（小写）。
pub fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

/// 远端文件的 SHA-256：走 SFTP 流式读，**不开 exec 信道**。
///
/// 远端一条 `sha256sum` 看着更省，但要引入「远端有没有这个命令（macOS 是 `shasum`）、
/// 路径怎么转义、shell 注入」一整类问题；流式读复用既有读循环，代码面小且可测。
/// 代价是大文件多读一遍远端 —— 校验本来就是 O(n)，且由用户显式开启。
async fn hash_remote(
    sftp: &russh_sftp::client::SftpSession,
    remote: &str,
) -> Result<String, String> {
    use tokio::io::AsyncReadExt;
    let mut rf = sftp
        .open(remote)
        .await
        .map_err(|e| format!("打开远端文件 {remote} 失败: {e}"))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = rf
            .read(&mut buf)
            .await
            .map_err(|e| format!("读取 {remote} 失败: {e}"))?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex(&h.finalize()))
}

/// 把本地文件 `[0, upto)` 读进哈希上下文。
///
/// 续传 + 校验同时开启时用：前半段是上一次传的，没经过这次下载循环，
/// 不补喂的话算出来的摘要必然对不上。
async fn hash_local_prefix(path: &Path, upto: u64) -> Result<Sha256, String> {
    use tokio::io::AsyncReadExt;
    let mut f = tokio::fs::File::open(path)
        .await
        .map_err(|e| format!("打开本地文件 {} 失败: {e}", path.display()))?;
    let mut h = Sha256::new();
    let mut left = upto;
    let mut buf = vec![0u8; CHUNK];
    while left > 0 {
        let want = left.min(CHUNK as u64) as usize;
        let n = f
            .read(&mut buf[..want])
            .await
            .map_err(|e| format!("读取本地文件 {} 失败: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        left -= n as u64;
    }
    Ok(h)
}

/// 打开本地目标文件：`from == 0` 截断重建；`from > 0` 保留已有内容并定位到偏移。
///
/// 续传**不能**用 `File::create`（它截断）—— 那正是「重复写」的来源。
async fn open_local_for_write(path: &Path, from: u64) -> Result<tokio::fs::File, String> {
    use tokio::io::AsyncSeekExt;
    if from == 0 {
        return tokio::fs::File::create(path)
            .await
            .map_err(|e| format!("创建本地文件 {} 失败: {e}", path.display()));
    }
    let mut f = tokio::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .await
        .map_err(|e| format!("打开本地文件 {} 失败: {e}", path.display()))?;
    f.seek(std::io::SeekFrom::Start(from))
        .await
        .map_err(|e| format!("定位本地文件 {} 到 {from} 失败: {e}", path.display()))?;
    Ok(f)
}

/// 打开远端目标文件（上传用）：`from == 0` 截断重建；`from > 0` 保留已有内容并定位。
///
/// 续传时不带 `TRUNCATE` —— 带上等于把已传的部分清掉，与「从头重传」无异。
async fn open_remote_for_write(
    sftp: &russh_sftp::client::SftpSession,
    remote: &str,
    from: u64,
) -> Result<russh_sftp::client::fs::File, String> {
    use russh_sftp::protocol::OpenFlags as F;
    use tokio::io::AsyncSeekExt;
    let flags = if from == 0 {
        F::CREATE | F::TRUNCATE | F::WRITE
    } else {
        F::CREATE | F::WRITE
    };
    let mut f = sftp
        .open_with_flags(remote, flags)
        .await
        .map_err(|e| format!("创建远端文件失败: {e}"))?;
    if from > 0 {
        f.seek(std::io::SeekFrom::Start(from))
            .await
            .map_err(|e| format!("定位远端文件到 {from} 失败: {e}"))?;
    }
    Ok(f)
}

/// 传输过程中的账本写入端（节流由 `sftp_resume::ResumeStore` 负责）。
struct MarkSink {
    dir: TransferDir,
    local: String,
    remote: String,
    total: u64,
}

impl MarkSink {
    fn new(dir: TransferDir, local: &Path, remote: &str, total: u64) -> Self {
        Self {
            dir,
            local: local.to_string_lossy().to_string(),
            remote: remote.to_string(),
            total,
        }
    }

    /// 进度推进（内存里始终精确，落盘按 2s 节流）
    fn progress(&self, bytes: u64) {
        resume_store().record(ResumeMark::new(
            self.dir,
            self.local.clone(),
            self.remote.clone(),
            bytes,
            self.total,
        ));
    }

    /// 传输成功：记录作废（半成品已不存在，留着只会让下次拿旧偏移去比长度）
    fn done(&self) {
        resume_store().forget(self.dir, &self.local, &self.remote);
    }

    /// 传输失败：把最后进度立刻落盘 —— 这条记录就是下次续传的起点
    fn failed(&self, bytes: u64) {
        self.progress(bytes);
        resume_store().flush();
    }
}

/// 续传决策的结果。
struct ResumeDecision {
    /// 起点偏移（0 = 从头）
    from: u64,
    /// 是否有过未完成记录。上传靠它决定「远端已存在」要不要拦：
    /// 有记录说明远端那份半成品是我们自己写的，再拦就把用户堵死了
    /// （删了可惜、传又传不了）。
    had_mark: bool,
    /// 给传输队列的一行备注；全新传输（从没有过记录）时为 `None` —— 那不需要说明。
    note: Option<String>,
}

/// 查记录 → 守卫 → 定起点。守卫的判据（谁是目标、谁必须恰好等于偏移）
/// 全在纯函数 [`crate::sftp_resume::plan_resume`] 里，这里只管 IO 与文案。
async fn decide_resume(
    dir: TransferDir,
    local: &Path,
    remote: &str,
    sftp: &russh_sftp::client::SftpSession,
) -> ResumeDecision {
    let local_s = local.to_string_lossy().to_string();
    let store = resume_store();
    let mark = store.find(dir, &local_s, remote);
    let l_len = local_len(local).await;
    let r_len = remote_len(sftp, remote).await.unwrap_or(0);
    match plan_resume(mark.as_ref(), l_len, r_len) {
        ResumePlan::Resume { from } => {
            let total = mark.as_ref().map(|m| m.total.max(r_len)).unwrap_or(r_len);
            ResumeDecision {
                from,
                had_mark: true,
                note: Some(format!("已从 {} 续传", progress_text(from, total))),
            }
        }
        ResumePlan::Restart(reason) => {
            if mark.is_some() {
                store.forget(dir, &local_s, remote);
                ResumeDecision {
                    from: 0,
                    had_mark: true,
                    note: Some(format!("已重新开始：{reason}")),
                }
            } else {
                ResumeDecision {
                    from: 0,
                    had_mark: false,
                    note: None,
                }
            }
        }
    }
}

/// 一次文件传输的上报上下文：进度事件 + 账本。
///
/// `base` 是**本批之前**已计入的字节（目录下载时是前面文件的总和；单文件下载为 0）：
/// 进度条要的是整批的连续进度，而账本要的是**本文件**的偏移 —— 两者差一个 base。
struct TransferCtx<'a> {
    transfer_id: u64,
    base: u64,
    tx: &'a flume::Sender<SftpEvent>,
    sink: Option<&'a MarkSink>,
    /// 取消旗标（[`cancel_slots`]）：每个分片前检查一次
    cancel: std::sync::Arc<AtomicBool>,
}

impl TransferCtx<'_> {
    /// 用户点了取消 ⇒ 中止本次传输。
    ///
    /// 检查点放在**每个分片之前**：64KB 一片，一次 `Relaxed` load 相对一次
    /// SFTP 往返可以忽略不计；粒度也够细 —— 取消后最多再多写一片。
    fn check_cancel(&self) -> Result<(), String> {
        if self.cancel.load(Ordering::Relaxed) {
            Err(CANCELLED.to_string())
        } else {
            Ok(())
        }
    }

    fn progress(&self, file_done: u64) {
        let _ = self.tx.send(SftpEvent::Progress {
            transfer_id: self.transfer_id,
            bytes: self.base + file_done,
        });
        if let Some(s) = self.sink {
            s.progress(file_done);
        }
    }
}

/// 远端 → 本地：从 `from` 处续写，边读边推进 `file_done`。
///
/// `hasher` 为 `Some` 时把**本次读到的**字节喂进去；续传起点之前的部分由调用方
/// 用 [`hash_local_prefix`] 预先补喂（那里不在这个循环的视野里）。
async fn copy_down(
    sftp: &russh_sftp::client::SftpSession,
    remote: &str,
    local: &Path,
    from: u64,
    file_done: &mut u64,
    ctx: &TransferCtx<'_>,
    hasher: &mut Option<Sha256>,
) -> Result<(), String> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
    let mut rf = sftp
        .open(remote)
        .await
        .map_err(|e| format!("打开远端文件 {remote} 失败: {e}"))?;
    if from > 0 {
        rf.seek(std::io::SeekFrom::Start(from))
            .await
            .map_err(|e| format!("定位远端文件 {remote} 到 {from} 失败: {e}"))?;
    }
    let mut lf = open_local_for_write(local, from).await?;
    let mut buf = vec![0u8; CHUNK];
    loop {
        // 每片先看取消：点了「取消」之后最多再写一片就停
        ctx.check_cancel()?;
        let n = rf
            .read(&mut buf)
            .await
            .map_err(|e| format!("读取 {remote} 失败: {e}"))?;
        if n == 0 {
            break;
        }
        lf.write_all(&buf[..n])
            .await
            .map_err(|e| format!("写入本地失败: {e}"))?;
        if let Some(h) = hasher.as_mut() {
            h.update(&buf[..n]);
        }
        *file_done += n as u64;
        ctx.progress(*file_done);
    }
    lf.flush()
        .await
        .map_err(|e| format!("写入本地 {} 失败: {e}", local.display()))?;
    Ok(())
}

/// 本地 → 远端：从 `from` 处续写，边读边推进 `file_done`。
async fn copy_up(
    sftp: &russh_sftp::client::SftpSession,
    local: &Path,
    remote: &str,
    from: u64,
    file_done: &mut u64,
    ctx: &TransferCtx<'_>,
) -> Result<(), String> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
    let mut lf = tokio::fs::File::open(local)
        .await
        .map_err(|e| format!("打开本地文件失败: {e}"))?;
    if from > 0 {
        lf.seek(std::io::SeekFrom::Start(from))
            .await
            .map_err(|e| format!("定位本地文件到 {from} 失败: {e}"))?;
    }
    let mut rf = open_remote_for_write(sftp, remote, from).await?;
    let mut buf = vec![0u8; CHUNK];
    loop {
        // 每片先看取消（与下载同一口径）
        ctx.check_cancel()?;
        let n = lf
            .read(&mut buf)
            .await
            .map_err(|e| format!("读取本地失败: {e}"))?;
        if n == 0 {
            break;
        }
        rf.write_all(&buf[..n])
            .await
            .map_err(|e| format!("写入远端失败: {e}"))?;
        *file_done += n as u64;
        ctx.progress(*file_done);
    }
    rf.flush().await.ok();
    rf.shutdown().await.ok();
    Ok(())
}

/// 下载一个文件到本地：续传 → 拷贝 → 大小校验（恒做）→ 可选 SHA-256。
///
/// `done` 是**本批累计**字节：函数返回时它已包含这个文件的全部字节（含续传起点），
/// 目录下载因此能拿到一条连续的总进度条。返回 `Ok(描述)` / `Err(原因)`，
/// 两者都经 [`finish_transfer`] 变成 `TransferDone`。
///
/// 形参 8 个（不含 `&self`，本函数也没有 `self`），每个都是独立语义：
/// `sftp` / `local` / `tx` 是三个不同层次的句柄，`done` 是出参，
/// 「远端路径 + 续传记录 + 总量提示 + 是否校验」四项各自都是单文件级的输入，
/// 没有天然聚合体。故与 `terminal::render::paint` 一样显式豁免该 lint。
#[allow(clippy::too_many_arguments)]
async fn download_one(
    sftp: &russh_sftp::client::SftpSession,
    remote: &str,
    local: &Path,
    transfer_id: u64,
    total_hint: u64,
    verify: bool,
    tx: &flume::Sender<SftpEvent>,
    cancel: &std::sync::Arc<AtomicBool>,
    done: &mut u64,
) -> Result<String, String> {
    // 总量：UI 起手不知道远端大小，这里 stat 回填（否则进度条恒 0%）
    let remote_size = remote_len(sftp, remote).await;
    let total = if total_hint == 0 {
        remote_size.unwrap_or(0)
    } else {
        total_hint
    };
    if total_hint == 0 {
        if let Some(size) = remote_size {
            let _ = tx.send(SftpEvent::TransferMeta {
                transfer_id,
                total: size,
            });
        }
    }

    let dec = decide_resume(TransferDir::Download, local, remote, sftp).await;
    if let Some(note) = dec.note {
        let _ = tx.send(SftpEvent::TransferNote {
            transfer_id,
            text: note,
        });
    }
    let sink = MarkSink::new(TransferDir::Download, local, remote, total);
    let ctx = TransferCtx {
        transfer_id,
        base: *done,
        tx,
        sink: Some(&sink),
        cancel: cancel.clone(),
    };
    let mut file_done = dec.from;
    if dec.from > 0 {
        // 起点偏移也是「这个文件已传的部分」：先记账，进度从它开始
        sink.progress(file_done);
        ctx.progress(file_done);
    }

    let mut hasher = if verify {
        Some(match dec.from {
            0 => Sha256::new(),
            from => hash_local_prefix(local, from).await?,
        })
    } else {
        None
    };

    if let Err(e) = copy_down(
        sftp,
        remote,
        local,
        dec.from,
        &mut file_done,
        &ctx,
        &mut hasher,
    )
    .await
    {
        sink.failed(file_done);
        return Err(e);
    }
    *done = ctx.base + file_done;

    // 大小校验：恒做 —— 零额外 IO（远端大小刚 stat 过，本地一次 stat 就有）
    if let Some(expected) = remote_size {
        let actual = local_len(local).await;
        if actual != expected {
            sink.failed(file_done);
            return Err(format!(
                "大小校验失败：本地 {actual} 字节 / 远端 {expected} 字节（文件已保留，可重传续传）"
            ));
        }
    }

    // 内容校验：仅在用户开启时（要把远端再读一遍，见规格的有意取舍）
    if let Some(h) = hasher {
        // 校验要把远端整份再读一遍：这一步也是「用户可能等不下去」的地方，
        // 先看一眼取消旗 —— 否则点了取消还得等一整轮哈希才收手。
        ctx.check_cancel()?;
        let local_hex = hex(&h.finalize());
        let remote_hex = hash_remote(sftp, remote).await?;
        if local_hex != remote_hex {
            // 内容不符：半成品不能拿来续传（续下去还是错的），记录必须作废
            sink.done();
            return Err(format!(
                "SHA-256 校验失败：本地 {}… / 远端 {}…（文件已保留，建议删除后重传）",
                &local_hex[..8.min(local_hex.len())],
                &remote_hex[..8.min(remote_hex.len())]
            ));
        }
    }

    sink.done();
    Ok(format!("下载完成 {file_done} bytes"))
}

/// 上传一个文件到远端（含续传）。返回 `Ok(描述)` / `Err(原因)`。
async fn upload_one(
    sftp: &russh_sftp::client::SftpSession,
    local: &Path,
    remote: &str,
    transfer_id: u64,
    total_hint: u64,
    tx: &flume::Sender<SftpEvent>,
    cancel: &std::sync::Arc<AtomicBool>,
) -> Result<String, String> {
    let local_size = local_len(local).await;
    let total = if total_hint == 0 {
        local_size
    } else {
        total_hint
    };
    let dec = decide_resume(TransferDir::Upload, local, remote, sftp).await;
    // 不静默覆盖远端已有文件（既有口径）。唯一例外：有未完成记录 ⇒ 远端那份半成品
    // 就是我们自己写的，此时拦截会把用户堵死在「远端已存在同名文件」上。
    if dec.from == 0 && !dec.had_mark && sftp.metadata(remote).await.is_ok() {
        return Err("上传取消：远端已存在同名文件（为防覆盖未写入，请先重命名）".into());
    }
    if let Some(note) = dec.note {
        let _ = tx.send(SftpEvent::TransferNote {
            transfer_id,
            text: note,
        });
    }
    let sink = MarkSink::new(TransferDir::Upload, local, remote, total);
    let ctx = TransferCtx {
        transfer_id,
        base: 0,
        tx,
        sink: Some(&sink),
        cancel: cancel.clone(),
    };
    let mut file_done = dec.from;
    if dec.from > 0 {
        sink.progress(file_done);
        ctx.progress(file_done);
    }
    match copy_up(sftp, local, remote, dec.from, &mut file_done, &ctx).await {
        Ok(()) => {
            sink.done();
            Ok(format!("上传完成 {file_done}/{total} bytes"))
        }
        Err(e) => {
            sink.failed(file_done);
            Err(e)
        }
    }
}

/// 传输收尾：失败**不一定**是会话故障 —— 先探活，活着就只报这一条失败。
///
/// 只有探活（`canonicalize(".")`，一次廉价往返）也失败时才把 `Err` 交回上层，
/// 由 `spawn_sftp_service` 丢弃会话（那是既有语义唯一仍然成立的场景）。
async fn finish_transfer(
    res: Result<String, String>,
    transfer_id: u64,
    tx: &flume::Sender<SftpEvent>,
    sftp: &russh_sftp::client::SftpSession,
) -> Result<(), String> {
    match res {
        Ok(message) => {
            let _ = tx.send(SftpEvent::TransferDone {
                transfer_id,
                ok: true,
                message,
            });
            Ok(())
        }
        Err(message) => {
            if sftp.canonicalize(".").await.is_err() {
                return Err(message);
            }
            let _ = tx.send(SftpEvent::TransferDone {
                transfer_id,
                ok: false,
                message,
            });
            Ok(())
        }
    }
}

/// 传输收尾：把取消旗标收走。
///
/// 用 `Drop` 而不是在每个 `return` 前手写一次：传输分支里有 `?`、有提前返回、
/// 有 `finish_transfer` 的两种出口 —— 漏一处的症状是「同一个 `transfer_id`
/// 下次一开工就是已取消状态」，很难从界面上看出来。
struct CancelGuard(u64);

impl Drop for CancelGuard {
    fn drop(&mut self) {
        forget_cancel(self.0);
    }
}

/// 旗标是否已放下（[`cancel_slot`] 的读端）。
fn is_cancelled(flag: &AtomicBool) -> bool {
    flag.load(Ordering::Relaxed)
}

async fn run_cmd(sftp: &russh_sftp::client::SftpSession, cmd: &SftpCmd) -> Result<(), String> {
    let tx = sftp_events().0.clone();
    match cmd {
        SftpCmd::List { path } => {
            // `~` 要落到远端家目录上展开；只在真出现 `~` 时才多问一次服务器
            let target = if path.starts_with('~') {
                match remote_home(sftp).await {
                    Some(home) => expand_tilde(path, &home),
                    None => path.clone(),
                }
            } else {
                path.clone()
            };
            let (abs, entries) = list_entries(sftp, &target).await?;
            let _ = tx.send(SftpEvent::Listing { path: abs, entries });
            Ok(())
        }
        SftpCmd::ListHome => {
            let home = remote_home(sftp)
                .await
                .unwrap_or_else(|| DEFAULT_REMOTE_PATH.to_string());
            let (abs, entries) = list_entries(sftp, &home).await?;
            let _ = tx.send(SftpEvent::Listing { path: abs, entries });
            Ok(())
        }
        SftpCmd::DownloadDir {
            remote,
            local,
            transfer_id,
        } => {
            let cancel = cancel_slot(*transfer_id);
            let _guard = CancelGuard(*transfer_id);
            // 1) 先把远端树走一遍：收集文件清单与总字节数。
            //    进度条要有百分比就必须先知道总量；用显式工作栈而非递归，
            //    路径深度不受调用栈限制。
            const MAX_ENTRIES: usize = 20_000;
            let mut dirs: Vec<String> = vec![remote.clone()];
            let mut files: Vec<(String, u64)> = Vec::new();
            while let Some(dir) = dirs.pop() {
                // 大目录的「走树」本身就要几秒：这一步也得能取消，
                // 否则用户点了取消还要等整棵扫完才开始响应。
                if is_cancelled(&cancel) {
                    return finish_transfer(Err(CANCELLED.to_string()), *transfer_id, &tx, sftp)
                        .await;
                }
                let read = sftp
                    .read_dir(&dir)
                    .await
                    .map_err(|e| format!("读取目录 {dir} 失败: {e}"))?;
                for e in read {
                    let name = e.file_name();
                    if name == "." || name == ".." {
                        continue;
                    }
                    if files.len() >= MAX_ENTRIES {
                        return Err(format!(
                            "目录过大（超过 {MAX_ENTRIES} 个文件），已中止以防误操作"
                        ));
                    }
                    let child = join_remote(&dir, &name);
                    if matches!(e.file_type(), russh_sftp::protocol::FileType::Dir) {
                        dirs.push(child);
                    } else {
                        files.push((child, e.metadata().size.unwrap_or(0)));
                    }
                }
            }
            let total: u64 = files.iter().map(|(_, size)| *size).sum();
            let _ = tx.send(SftpEvent::TransferMeta {
                transfer_id: *transfer_id,
                total,
            });

            // 2) 建本地根目录 + 逐个文件下载（远端相对路径 到 本地同构路径）
            tokio::fs::create_dir_all(&local)
                .await
                .map_err(|e| format!("创建本地目录失败: {e}"))?;
            let mut done: u64 = 0;
            let mut count: usize = 0;
            let mut skipped: usize = 0;
            let mut failed: Option<String> = None;
            for (remote_file, size) in &files {
                // 每个文件开始前看一眼：取消之后不再开下一个文件
                if is_cancelled(&cancel) {
                    failed = Some(CANCELLED.to_string());
                    break;
                }
                let rel = remote_file
                    .strip_prefix(remote.as_str())
                    .unwrap_or(remote_file)
                    .trim_start_matches('/');
                let local_file = local.join(rel.replace('\\', "/"));
                if let Some(parent) = local_file.parent() {
                    tokio::fs::create_dir_all(parent)
                        .await
                        .map_err(|e| format!("创建本地目录失败: {e}"))?;
                }
                // 文件级跳过：本地那份长度已与远端一致 ⇒ 就当它传完了，不再读一遍。
                // 只比长度不比内容（目录下载不做哈希，见规格的有意取舍）。
                if *size > 0 && local_len(&local_file).await == *size {
                    done += *size;
                    skipped += 1;
                    let _ = tx.send(SftpEvent::Progress {
                        transfer_id: *transfer_id,
                        bytes: done,
                    });
                    continue;
                }
                match download_one(
                    sftp,
                    remote_file,
                    &local_file,
                    *transfer_id,
                    *size,
                    false,
                    &tx,
                    &cancel,
                    &mut done,
                )
                .await
                {
                    Ok(_) => count += 1,
                    Err(e) => {
                        failed = Some(format!("{rel}: {e}"));
                        break;
                    }
                }
            }
            if skipped > 0 {
                let _ = tx.send(SftpEvent::TransferNote {
                    transfer_id: *transfer_id,
                    text: format!("跳过 {skipped} 个已传完的文件"),
                });
            }
            if let Some(e) = failed {
                return finish_transfer(Err(e), *transfer_id, &tx, sftp).await;
            }
            let _ = tx.send(SftpEvent::TransferDone {
                transfer_id: *transfer_id,
                ok: true,
                message: format!("下载完成 {done}/{total} bytes（{count} 个文件）"),
            });
            Ok(())
        }
        SftpCmd::Download {
            remote,
            local,
            transfer_id,
            total,
            verify,
        } => {
            let cancel = cancel_slot(*transfer_id);
            let _guard = CancelGuard(*transfer_id);
            let mut done: u64 = 0;
            let res = download_one(
                sftp,
                remote,
                local,
                *transfer_id,
                *total,
                *verify,
                &tx,
                &cancel,
                &mut done,
            )
            .await;
            finish_transfer(res, *transfer_id, &tx, sftp).await
        }
        SftpCmd::Upload {
            local,
            remote,
            transfer_id,
            total,
        } => {
            let cancel = cancel_slot(*transfer_id);
            let _guard = CancelGuard(*transfer_id);
            let res = upload_one(sftp, local, remote, *transfer_id, *total, &tx, &cancel).await;
            let res = finish_transfer(res, *transfer_id, &tx, sftp).await;
            // 远端目录在传完之后**一定**变了（新文件或多了一个半成品），
            // 请 UI 重扫一次 —— 用户要求「上传到远端后刷新远端路径」。
            // 只有会话还活着才发：死了的会话重扫只会再报一次错。
            if res.is_ok() {
                let _ = tx.send(SftpEvent::Refresh {
                    path: remote_dir(remote),
                });
            }
            res
        }
        SftpCmd::Mkdir { path, refresh } => {
            sftp.create_dir(path)
                .await
                .map_err(|e| format!("创建目录失败: {e}"))?;
            let _ = tx.send(SftpEvent::Refresh {
                path: refresh.clone(),
            });
            Ok(())
        }
        SftpCmd::Remove {
            path,
            is_dir,
            recursive,
            refresh,
        } => {
            if *is_dir && *recursive {
                // 递归删除由本层实现（SFTP 只有 `RMDIR`，要求目录已空）：
                // 先把整棵树走完拿到顺序，**再**动手 —— 走不完就什么都不删。
                let scan = scan_tree(sftp, path).await?;
                for (target, target_is_dir) in removal_order(&scan) {
                    let res = if target_is_dir {
                        sftp.remove_dir(&target).await
                    } else {
                        sftp.remove_file(&target).await
                    };
                    res.map_err(|e| format!("删除 {target} 失败: {e}"))?;
                }
            } else if *is_dir {
                sftp.remove_dir(path)
                    .await
                    .map_err(|e| format!("删除目录失败（需为空）: {e}"))?;
            } else {
                sftp.remove_file(path)
                    .await
                    .map_err(|e| format!("删除文件失败: {e}"))?;
            }
            let _ = tx.send(SftpEvent::Refresh {
                path: refresh.clone(),
            });
            Ok(())
        }
        SftpCmd::Rename { from, to, refresh } => {
            sftp.rename(from, to)
                .await
                .map_err(|e| format!("重命名失败: {e}"))?;
            let _ = tx.send(SftpEvent::Refresh {
                path: refresh.clone(),
            });
            Ok(())
        }
        SftpCmd::Cancel { transfer_id } => {
            // 正常路径上服务循环已经**前置**处理过这条命令（不碰会话、不等闸门）——
            // 走到这里说明有别的调用方直接跑了 `run_cmd`。置位是幂等的，再置一次
            // 也无害，所以这里不写成 `unreachable!()`：那会让一个本可自愈的分支
            // 变成 panic。
            request_cancel(*transfer_id);
            Ok(())
        }
    }
}

/// 生成新的传输 id（UI 侧发起传输时调用）
pub fn new_transfer_id() -> u64 {
    next_transfer_id()
}

/// 统计一次「一次性下载」的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OneShotDownload {
    /// 落盘路径
    pub local: PathBuf,
    pub bytes: u64,
}

/// 一次性下载单个远端文件（定时任务 / agent 工具用）。
///
/// 与 [`spawn_sftp_service`] 的区别：那条路是**常驻**服务（复用子系统会话、
/// 进度走全局事件通道 `sftp_events`），面向文件面板的交互式浏览；
/// 这条是「连一次、下一个文件、断开」，结果直接返回给调用方 ——
/// 定时任务没有 UI 去消费全局事件，让它往那个通道里发进度只会丢失。
///
/// 每次调用新建连接：定时下载是低频动作，不值得为此维护连接池。
pub fn download_once(
    profile: &crate::connections::ConnectionProfile,
    remote: &str,
    local: &std::path::Path,
    timeout: std::time::Duration,
    global_proxy: Option<&crate::proxy::ProxyConfig>,
) -> Result<OneShotDownload, String> {
    crate::ssh::runtime().block_on(async {
        let handle = crate::ssh::connect_only(
            profile,
            timeout,
            crate::ssh::HostKeyPolicy::RejectUnknown,
            global_proxy,
        )
        .await?;
        let sftp = open_sftp(&handle).await?;
        if let Some(dir) = local.parent() {
            tokio::fs::create_dir_all(dir)
                .await
                .map_err(|e| format!("创建本地目录 {} 失败: {e}", dir.display()))?;
        }
        // 定时任务的一次性下载**不进续传账本**（它没有「重发同一传输」的 UI 入口），
        // 校验口径也保持原样：直接整文件覆盖，不做大小/哈希校验。
        // 进度通道只是原语的形参要求：接收端**丢掉**，之后每次 `send` 立即返回
        // `Err(Disconnected)` 而不阻塞 —— 留着接收端反而会让通道塞满后把下载卡死。
        let (tx, rx) = flume::bounded::<SftpEvent>(1);
        drop(rx);
        let mut done = 0u64;
        let mut hasher: Option<Sha256> = None;
        let ctx = TransferCtx {
            transfer_id: 0,
            base: 0,
            tx: &tx,
            sink: None,
            // 一次性下载（定时任务那条路）没有「取消」入口：给一面独立的、
            // 永不倒的旗 —— 不进登记表，免得留一条 id=0 的死记录
            cancel: Arc::new(AtomicBool::new(false)),
        };
        copy_down(&sftp, remote, local, 0, &mut done, &ctx, &mut hasher).await?;
        let _ = sftp.close().await;
        Ok(OneShotDownload {
            local: local.to_path_buf(),
            bytes: done,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 校验用的摘要必须是**标准** SHA-256（F-13）：
    /// 拿 NIST 的已知向量比对，而不是「自己算一遍存成期望值」——
    /// 后者只能证明实现没变，证明不了它算的是 SHA-256。
    #[test]
    fn sha256_matches_the_standard_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // 分块喂入 == 一次性喂入（远端/本地都是流式读，这条是流式路径的等价性保证）
        let data: Vec<u8> = (0u8..=255).cycle().take(200_000).collect();
        let mut h = Sha256::new();
        for chunk in data.chunks(7_919) {
            h.update(chunk);
        }
        assert_eq!(hex(&h.finalize()), sha256_hex(&data));
    }

    /// 字节转十六进制：小写、定宽两位（`0x0f` 不能写成 `f`）。
    #[test]
    fn hex_is_lowercase_and_zero_padded() {
        assert_eq!(hex(&[0x00, 0x0f, 0xa0, 0xff]), "000fa0ff");
        assert_eq!(hex(&[]), "");
    }

    /// 「已传 X%」文案：总量已知给百分比，未知退回字节数（不显示 0% 骗人）。
    #[test]
    fn progress_text_prefers_percentage_and_falls_back_to_bytes() {
        assert_eq!(progress_text(0, 0), "0 字节");
        assert_eq!(progress_text(1_536, 0), "1536 字节");
        assert_eq!(progress_text(512, 2_048), "25%");
        // 越界（进度超过总量）钳到 100%，不出现 250% 这种数字
        assert_eq!(progress_text(1_000, 400), "100%");
    }

    /// 并发闸门的分类：只有传输类命令受信号量约束，浏览 / 改名 / 删除必须不受限
    /// —— 大文件传输占满闸门时面板仍要能浏览（可用性优先于吞吐）。
    #[test]
    fn only_transfer_commands_are_gated() {
        let gated = [
            SftpCmd::Download {
                remote: "/r".into(),
                local: PathBuf::from("l"),
                transfer_id: 1,
                total: 0,
                verify: false,
            },
            SftpCmd::Upload {
                local: PathBuf::from("l"),
                remote: "/r".into(),
                transfer_id: 1,
                total: 0,
            },
            SftpCmd::DownloadDir {
                remote: "/r".into(),
                local: PathBuf::from("l"),
                transfer_id: 1,
            },
        ];
        for (i, cmd) in gated.iter().enumerate() {
            assert!(is_transfer_cmd(cmd), "第 {i} 条传输类命令没被闸门算上");
        }
        let free = [
            SftpCmd::List { path: "/".into() },
            SftpCmd::ListHome,
            SftpCmd::Mkdir {
                path: "/d".into(),
                refresh: "/".into(),
            },
            SftpCmd::Remove {
                path: "/f".into(),
                is_dir: false,
                recursive: false,
                refresh: "/".into(),
            },
            SftpCmd::Rename {
                from: "/a".into(),
                to: "/b".into(),
                refresh: "/".into(),
            },
            // 取消**绝不能**被闸门挡住：传输满载正是用户最想取消的那一刻，
            // 若它排在闸门后面，点下去要等前一条传完才生效（观感 = 没反应）。
            SftpCmd::Cancel { transfer_id: 1 },
        ];
        for (i, cmd) in free.iter().enumerate() {
            assert!(!is_transfer_cmd(cmd), "第 {i} 条非传输命令被闸门挡住了");
        }
    }

    /// 取消旗标：登记 → 置位 → 收走，三步都要落地。
    ///
    /// 这条是「点了取消到底有没有传到传输循环」的机制检查 —— 断掉任何一环的表现
    /// 都是「按钮点了没反应」，而在界面上完全看不出是哪一环断的。
    #[test]
    fn cancel_flags_are_registered_set_and_forgotten() {
        let id = new_transfer_id();
        let flag = cancel_slot(id);
        assert!(!is_cancelled(&flag), "新登记的旗标必须是立着的");

        request_cancel(id);
        assert!(
            is_cancelled(&flag),
            "取消请求没有落到传输循环正在读的那面旗上"
        );

        forget_cancel(id);
        let fresh = cancel_slot(id);
        assert!(
            !is_cancelled(&fresh),
            "收尾之后旗标必须复位 —— 否则这条 transfer_id 下次一开工就自杀"
        );
        forget_cancel(id);
    }

    /// **预取消**：命令先到、传输后开工，仍要立刻退出。
    ///
    /// 这是并发闸门后面的那条路径（`max_concurrent=1` 时几乎每次都会走）：
    /// 若旗标只在传输开工时才建，先到的 `Cancel` 会落进虚空。
    #[test]
    fn a_cancel_that_arrives_before_the_transfer_still_wins() {
        let id = new_transfer_id();
        request_cancel(id); // 传输还没开工
        let flag = cancel_slot(id); // 开工时取旗
        assert!(is_cancelled(&flag), "先到的取消必须在开工时生效");
        forget_cancel(id);
    }

    /// 远端文件的所在目录（上传后重扫用的就是它）：POSIX 口径，只认 `/`。
    #[test]
    fn remote_dir_is_posix_and_stops_at_root() {
        assert_eq!(remote_dir("/home/ubuntu/a.txt"), "/home/ubuntu");
        assert_eq!(remote_dir("/home/ubuntu/"), "/home");
        assert_eq!(remote_dir("/a"), "/");
        assert_eq!(remote_dir("/"), "/");
        assert_eq!(remote_dir("a.txt"), "/", "没有目录部分时退回根");
    }

    /// 接线护栏（2026-10-27 用户要求「上传到远端后需要刷新远端路径」）。
    ///
    /// 单测测不到「传完之后 UI 有没有重扫」：这条链路是
    /// `Upload 分支 到 SftpEvent::Refresh 到 UI 的 List`，中间任何一环断掉都不会
    /// 报错，只是列表静默地停在旧内容上（新传的文件看不见）。所以读源码钉住它。
    #[test]
    fn upload_asks_the_ui_to_rescan_the_remote_directory() {
        let src = include_str!("sftp.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap_or(src);
        let at = prod.find("let res = upload_one(").expect("找不到上传分支");
        let tail = &prod[at..at + 900.min(prod.len() - at)];
        assert!(
            tail.contains("SftpEvent::Refresh"),
            "上传分支没有发 `Refresh` —— 传完远端列表不会刷新"
        );
        assert!(
            tail.contains("remote_dir(remote)"),
            "刷新请求必须指向**文件所在目录**（`remote_dir`），而不是文件本身"
        );
    }

    /// 接线护栏：取消命令必须在**拿会话与闸门之前**被处理。
    ///
    /// 放错位置不会报错，只会让「传输满载时点取消」排队等前一条传完 ——
    /// 那正是用户最需要它生效的时刻。
    #[test]
    fn cancel_is_handled_before_the_session_and_the_gate() {
        let src = include_str!("sftp.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap_or(src);
        let cancel_at = prod
            .find("if let SftpCmd::Cancel { transfer_id } = &cmd")
            .expect("服务循环里没有前置处理取消命令");
        let session_at = prod[cancel_at..]
            .find("let sessions = sessions.clone();")
            .map(|i| cancel_at + i)
            .expect("找不到取会话的位置");
        assert!(
            cancel_at < session_at,
            "取消命令被排在「取会话 / 等闸门」之后 —— 满载时点取消会没反应"
        );
    }

    /// 目录下载的本地落点换算：远端相对路径 到 本地同构路径（`/` 转平台分隔符）。
    #[test]
    fn download_dir_relative_paths_stay_under_the_target() {
        let root = "/srv/app";
        let cases = [
            ("/srv/app/a.txt", "a.txt"),
            ("/srv/app/logs/today.log", "logs/today.log"),
            ("/srv/app/a/b/c.bin", "a/b/c.bin"),
        ];
        for (remote_file, expect) in cases {
            let rel = remote_file
                .strip_prefix(root)
                .unwrap_or(remote_file)
                .trim_start_matches('/');
            assert_eq!(rel.replace('\\', "/"), expect, "remote={remote_file}");
            // 拼出来的本地路径必须始终落在目标目录内（不能因 `..` 逃逸）
            let local = std::path::PathBuf::from("/tmp/dst").join(rel.replace('\\', "/"));
            assert!(local.starts_with("/tmp/dst") || local.starts_with("\\tmp\\dst"));
        }
    }

    /// 修改时间展示：0（服务端没给）不显示；正常值给出可读串。
    #[test]
    fn format_mtime_is_empty_for_unknown_and_readable_otherwise() {
        assert!(format_mtime(0).is_empty());
        // 2021-09-13 12:26:40 UTC 附近：必须给出非空且含数字的串
        let s = format_mtime(1_631_536_000);
        assert!(!s.is_empty(), "正常时间戳不该是空串");
        assert!(s.chars().any(|c| c.is_ascii_digit()), "s={s}");
    }

    /// 批量版必须与逐行版**逐字一致** —— 列表里的时间列不该因为「顺手优化了一下」
    /// 而在某个跨年时间戳上悄悄换格式（`format_mtime` 至今仍是别的调用点的入口）。
    #[test]
    fn batch_mtimes_match_the_single_entry_version() {
        let stamp = |modified| SftpEntry::new("x", false, 0, modified);
        // 本年内、跨年、以及「服务端没给」（0）三种形态都要覆盖
        let now = chrono::Local::now();
        let recent = now.timestamp() as u64;
        let entries = vec![stamp(0), stamp(recent), stamp(1_631_536_000)];
        let batch = format_entry_mtimes(&entries);
        assert_eq!(batch.len(), entries.len(), "必须一一对应，少一个就错列");
        for (e, got) in entries.iter().zip(&batch) {
            assert_eq!(*got, format_mtime(e.modified), "modified={}", e.modified);
        }
    }

    /// 远端路径拼接：目录带/不带尾斜杠都不产生 `//`。
    #[test]
    fn join_remote_both_shapes() {
        assert_eq!(join_remote("/var", "log"), "/var/log");
        assert_eq!(join_remote("/var/", "log"), "/var/log");
    }

    /// 路径栏输入归一化（2026-09-17 运行反馈「默认为 home 路径，cd ~ 的路径」）。
    ///
    /// 2026-09-20 改：`~` 不再在本机展开 —— 本函数只负责「剥 `cd` / 去空白」，
    /// `~` 原样交给 SFTP 侧用远端家目录展开（[`expand_tilde`]）。所以这里对
    /// `~` 形态断言的是**保留 `~`**，与运行环境无关。
    #[test]
    fn normalize_remote_input_handles_shell_style_paths() {
        // 绝对路径与相对路径原样通过
        assert_eq!(
            normalize_remote_input("/var/log").as_deref(),
            Some("/var/log")
        );
        assert_eq!(
            normalize_remote_input("  ./sub  ").as_deref(),
            Some("./sub")
        );
        // `cd <path>`：剥掉命令前缀（用户在终端里敲的写法，直接抄进路径栏）
        assert_eq!(
            normalize_remote_input("cd /var/log").as_deref(),
            Some("/var/log")
        );
        assert_eq!(normalize_remote_input("cd   ./x").as_deref(), Some("./x"));

        // `~` 形态：原样透传，交给远端展开（本机不知道那台机器的家目录）
        assert_eq!(normalize_remote_input("~").as_deref(), Some("~"));
        assert_eq!(normalize_remote_input("cd ~").as_deref(), Some("~"));
        assert_eq!(normalize_remote_input("~/logs").as_deref(), Some("~/logs"));
        assert_eq!(
            normalize_remote_input("cd ~/logs").as_deref(),
            Some("~/logs")
        );

        // 空串：交给调用方回退（`cd` 单独一条 = 回家目录）
        assert_eq!(normalize_remote_input("   "), None);
        assert_eq!(normalize_remote_input("cd").as_deref(), Some("~"));
    }

    /// `~` 用**远端**家目录展开（2026-09-20 用户口径「默认路径 = 服务器用户的
    /// home 绝对路径」的配套修复）：展开结果必须是那台机器上的路径。
    ///
    /// 反向用例同样重要：非 `~` 形态一个字节都不能动，否则 `/var/log`
    /// 之类的绝对路径会被拼到家目录后面。
    #[test]
    fn tilde_expands_against_the_remote_home() {
        let home = "/home/ubuntu";
        assert_eq!(expand_tilde("~", home), "/home/ubuntu");
        assert_eq!(expand_tilde("~/logs", home), "/home/ubuntu/logs");
        assert_eq!(expand_tilde("~/a/b", home), "/home/ubuntu/a/b");
        // 家目录带尾斜杠也不产生 `//`
        assert_eq!(expand_tilde("~/logs", "/home/ubuntu/"), "/home/ubuntu/logs");
        // root 的家目录不是 `/home/root`：展开结果由传入的 home 决定
        assert_eq!(expand_tilde("~", "/root"), "/root");
        assert_eq!(expand_tilde("~/x", "/root"), "/root/x");

        for p in ["/var/log", "./x", "sub", "", "x~y", "/srv/~/logs"] {
            assert_eq!(expand_tilde(p, home), p, "非 `~` 形态不该被改动：{p}");
        }
    }

    /// 接线护栏（2026-09-20）：`~` 只能在**远端**展开。
    ///
    /// 上一条用例覆盖了纯函数 [`expand_tilde`]，但单测测不到**接线** —— 而接错线
    /// 的表现恰恰是用户看得见的那个 bug：「`~` 又变成本机家目录」。所以这里直接读
    /// 源码，把必须同时成立的三件事钉住：首扫走 home 解析、`List` 里的 `~` 走
    /// `remote_home` + `expand_tilde`、本机家目录不再参与。
    #[test]
    fn tilde_is_expanded_on_the_remote_side() {
        let src = include_str!("sftp.rs");
        // 只看生产代码：本测试自身也写着这些字面量，不去掉就等于自证
        let prod = src.split("#[cfg(test)]").next().unwrap_or(src);

        assert!(
            prod.contains("SftpCmd::ListHome =>"),
            "缺少 `ListHome` 分支：文件面板首扫拿不到远端家目录。"
        );
        assert!(
            prod.contains("path.starts_with('~')") && prod.contains("expand_tilde(path, &home)"),
            "`List` 没把 `~` 交给远端展开 —— 它会退回本机家目录（另一台机器的路径）。"
        );
        assert!(
            !prod.contains("fn home_dir"),
            "核心侧不该再有取本机家目录的函数：`~` 属于服务器，不属于跑 UI 的这台机器。"
        );
    }

    /// 权限位 → `rwxr-xr-x`（2026-09-22 用户要求「文件栏列表增加权限和用户组」）。
    ///
    /// 9 位等宽是这一列**能当定宽列用**的前提：长度随权限值变化（`0o644` 与
    /// `0o777` 若按「跳过全零位」的写法则长度不同），列就会跟着每行抖。
    #[test]
    fn mode_string_is_always_nine_chars() {
        let cases: [(u32, &str); 7] = [
            (0o755, "rwxr-xr-x"),
            (0o644, "rw-r--r--"),
            (0o600, "rw-------"),
            (0o777, "rwxrwxrwx"),
            (0o000, "---------"),
            // 高位（setuid/setgid/sticky）与文件类型位都不进这 9 位
            (0o4755, "rwxr-xr-x"),
            (0o100644, "rw-r--r--"),
        ];
        for (bits, want) in cases {
            let got = format_mode(bits);
            assert_eq!(got, want, "{bits:o} 应当显示成 {want}");
            assert_eq!(got.chars().count(), 9, "长度必须恒为 9：{got}");
        }
    }

    /// `entry_mode` 的「未知」与「无权限」必须分得开。
    ///
    /// 服务端省略 `permissions`（协议允许）时给**空串**；真给 `0o000` 时给
    /// `----------`（十个字符 = 文件类型位 + 九位权限）。两者若都画成
    /// `----------`，用户会把「查不到」读成「这文件谁都动不了」，进而做出错误判断。
    #[test]
    fn unknown_permissions_stay_blank() {
        let unknown = SftpEntry::new("a", false, 0, 0);
        assert_eq!(entry_mode(&unknown), "");

        let denied = SftpEntry {
            permissions: Some(0),
            ..SftpEntry::new("a", false, 0, 0)
        };
        assert_eq!(entry_mode(&denied), "----------", "首字符是文件类型位 `-`");
        assert_eq!(
            entry_mode(&denied).chars().count(),
            10,
            "权限列要定宽，展示串必须恒为 10 个字符"
        );

        let dir = SftpEntry {
            permissions: Some(0o755),
            ..SftpEntry::new("d", true, 4096, 0)
        };
        assert_eq!(entry_mode(&dir), "drwxr-xr-x", "首字符要区分目录与文件");
    }

    /// 属主串：优先名字，其次数字 id，缺一侧用 `?`，全缺空串。
    #[test]
    fn owner_string_falls_back_to_numeric_ids() {
        let with_names = SftpEntry {
            user: Some("root".into()),
            group: Some("root".into()),
            ..SftpEntry::new("a", false, 0, 0)
        };
        assert_eq!(entry_owner(&with_names), "root");

        let ids_only = SftpEntry {
            uid: Some(0),
            gid: Some(0),
            ..SftpEntry::new("a", false, 0, 0)
        };
        assert_eq!(
            entry_owner(&ids_only),
            "0",
            "只有数字 id 时退回 `ls -n` 的 uid（不再拼 `uid:gid`）"
        );

        let half = SftpEntry {
            user: Some("ubuntu".into()),
            gid: Some(1000),
            ..SftpEntry::new("a", false, 0, 0)
        };
        assert_eq!(entry_owner(&half), "ubuntu");

        let unknown = SftpEntry::new("a", false, 0, 0);
        assert_eq!(entry_owner(&unknown), "", "两者都没有时留白，不画 `?:?`");
    }

    /// 递归删除的**顺序**：文件先走完，目录从深到浅。
    ///
    /// 入参按 `scan_tree` 的前序形态构造（父目录先于其子项出现）。顺序写错的表现是
    /// 「非空目录删不掉」—— `RMDIR` 撞上还装着东西的目录 —— 而这需要真连一台
    /// 服务器、真造一棵两层树才会暴露，单测里最容易漏的就是它。
    #[test]
    fn removal_order_deletes_children_before_parents() {
        let scan: Vec<(String, bool)> = vec![
            ("/a".to_string(), true),
            ("/a/keep.txt".to_string(), false),
            ("/a/b".to_string(), true),
            ("/a/b/c.txt".to_string(), false),
            ("/a/b/deep".to_string(), true),
            ("/a/b/deep/d.txt".to_string(), false),
        ];
        let order = removal_order(&scan);
        let pos = |p: &str| {
            order
                .iter()
                .position(|(t, _)| t == p)
                .unwrap_or_else(|| panic!("{p} 不在删除计划里"))
        };
        // 所有文件都排在所有目录之前
        let first_dir_ix = order.iter().position(|(_, d)| *d).expect("应当有目录");
        let last_file_ix = order.iter().rposition(|(_, d)| !*d).expect("应当有文件");
        assert!(
            last_file_ix < first_dir_ix,
            "文件必须先删完再删目录，否则空目录判定会被自己的子文件挡住"
        );
        // 子目录先于父目录，祖父最后
        assert!(pos("/a/b/deep") < pos("/a/b"), "/a/b/deep 必须先于 /a/b");
        assert!(pos("/a/b") < pos("/a"), "/a/b 必须先于 /a");
        assert_eq!(order.len(), scan.len(), "计划里不能漏项也不能重复");
    }

    /// 文件名预校验的三条硬规则（2026-09-22：新建目录 / 重命名改成收用户输入）。
    ///
    /// 这三条的共同点是「提交上去必然失败，而失败的代价是整个 SFTP 会话被丢掉」
    /// （见 `spawn_sftp_service` 对 `Err` 的处理）—— 所以在本地拦。
    /// 反过来说，其余判断（重名、权限、长度）**刻意不在这里**：那些只有服务端知道，
    /// 假装自己能判会给用户错误的安全感。
    #[test]
    fn entry_name_rejects_only_the_hopeless_cases() {
        assert_eq!(check_entry_name(""), Err(NameCheck::Empty));
        assert_eq!(check_entry_name("a/b"), Err(NameCheck::ContainsSlash));
        assert_eq!(check_entry_name("."), Err(NameCheck::Reserved));
        assert_eq!(check_entry_name(".."), Err(NameCheck::Reserved));

        // 合法的名字，包括一些看着奇怪但服务端接受的
        for ok in ["logs", "a b.txt", ".bashrc", "中文目录", "..a", "a..b", "-"] {
            assert_eq!(check_entry_name(ok), Ok(()), "`{ok}` 应当被放行");
        }
    }
}
