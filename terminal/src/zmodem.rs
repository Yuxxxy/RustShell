//! ZMODEM（`rz` / `sz`）收发 + 拖放上传的落地层。
//!
//! 协议本身交给 `zmodem2`（调用方驱动、无 I/O）；本模块负责三件事：
//!
//! 1. **在字节流里认出 ZMODEM 会话**。远端 `sz`/`rz` 的第一帧是
//!    `**\x18B..`（ZHEX 报文头），认出之前所有字节照常交给 VTE 解析；
//!    认出之后到会话结束之前的字节全部喂给协议状态机 —— 它们不是文字，
//!    送进 VTE 会画出一屏乱码。识别跨块（一次 read 的边界可能切在 `**` 与
//!    `\x18` 之间），尾部前缀留在 `scan` 里等下一块拼接。
//! 2. **把协议动作落到真实的管道与磁盘上**：`WriteWire` 写 PTY、`ReadFile`
//!    读本地文件、`WriteFile` 写下载目录。全部在后台读线程上完成，不占 UI 线程。
//! 3. **拖放上传**：先向远端键入 `rz -be`，等它回 ZRINIT 再开始发文件。等它的
//!    这段时间里收到的文字（例如 `command not found`）照常上屏 —— 用户要能
//!    看见「远端根本没装 lrzsz」，而不是对着一屏没反应的终端猜。
//!
//! ★ 为什么不放在 UI 层：本地 PTY、SSH、将来的串口/Telnet 都是同一条字节流。
//!   把协议挂在字节流上，换传输不用再写一遍；挂在 UI 上则每加一种传输就得抄一次。
//!
//! ★ 传输期间键盘/粘贴/`write_to_pty` 一律封住（见 `TerminalView` 的几道闸）：
//!   ZMODEM 是二进制流，任何一个用户字节混进去都会让两端 CRC 对不上。

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use zmodem2::{Action, Event as ZmodemEvent, FileInfo, Position, Receiver, Sender};

use crate::event::PtyWriter;

/// ZMODEM 报文头起始符（`*`）。
const ZPAD: u8 = b'*';
/// ZMODEM 转义符（CAN）。
const ZDLE: u8 = 0x18;

/// 拖放后等远端 `rz` 起来的时限。
///
/// 超过它还没等到第一帧 ZRINIT，就认定远端没有 `rz`（或 shell 没执行）：
/// 继续等下去，用户每一次点终端都会被告知「传输中」，而画面里明明什么都没发生。
const UPLOAD_READY_TIMEOUT: Duration = Duration::from_secs(20);

/// 活动传输「多久没有收到任何字节」判死。
///
/// 链路断了但读端尚未 EOF（半开连接）时，状态机自己不会放弃；不判死就会
/// 一直吞掉后续输出（连 shell 提示符都不再显示），看起来像终端卡死。
const STALL_TIMEOUT: Duration = Duration::from_secs(60);

/// 传输结果提示的停留时间（UI 侧轮询 `status()` 自动过期）。
const NOTICE_TTL: Duration = Duration::from_secs(6);

/// 协议态下「暂时喂不进去」的字节上限。正常不会触发：状态机被驱动到 `Idle`
/// 之后一定吃得下；真堆到这里说明对端在发我们无法解析的东西，直接判失败，
/// 免得把整块内存耗光。
const MAX_PROTO_TAIL: usize = 8 * 1024;

/// 远端收文件的命令。`-b` = 二进制、`-e` = 转义控制字符（与 lrzsz 的常用组合一致）。
const REMOTE_RECEIVE_CMD: &[u8] = b"rz -be\r";

/// 会话结束后还能吞掉多少个协议填充字节（见 `Inner::trailer`）。
/// 封顶 8 字节：宁可少吞，也不能把用户的提示符吃掉。
const TRAILER_BUDGET: usize = 8;

/// 当前传输的阶段。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ZmodemPhase {
    /// 空闲（没有传输，也没有待启动的上传）。
    #[default]
    Idle,
    /// 已键入 `rz`，等远端第一帧（或等超时）。
    Waiting,
    /// 正在接收（`sz` 下载）。
    Downloading,
    /// 正在发送（拖放上传 / `rz`）。
    Uploading,
}

impl ZmodemPhase {
    pub fn is_active(self) -> bool {
        self != Self::Idle
    }
}

/// 一次传输的结果（UI 负责翻译成文案）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ZmodemNotice {
    /// 会话正常结束。`dir` 只有下载时有意义（上传为 `None`）。
    Completed { files: usize, dir: Option<String> },
    /// 用户取消，或对端中止。
    Cancelled,
    /// 拖放后远端一直没进入 ZMODEM（多半是没装 lrzsz / `rz` 不在 PATH 里）。
    RemoteNotReady,
    /// 长时间收不到数据，按链路中断处理。
    Stalled,
    /// 本地 I/O 或协议错误（原文给 UI 拼接，便于定位）。
    Failed(String),
}

/// 供 UI 渲染的传输快照。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ZmodemStatus {
    pub phase: ZmodemPhase,
    /// 当前文件名（下载用对端给的名字，上传用本地文件名）。
    pub file: Option<String>,
    /// 当前文件已处理的字节数。
    pub transferred: u64,
    /// 当前文件总字节数（对端没给 / 上传文件为空时为 `None`）。
    pub total: Option<u64>,
    /// 本次会话已经完成的文件数。
    pub files_done: usize,
    /// 本次会话计划发送的文件数（只有上传知道；下载为 `None`）。
    pub files_total: Option<usize>,
    /// 传输结束后的结果提示，`NOTICE_TTL` 后自动清空。
    pub notice: Option<ZmodemNotice>,
}

impl ZmodemStatus {
    pub fn is_active(&self) -> bool {
        self.phase.is_active()
    }
}

/// 拖放上传的启动结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ZmodemUploadStart {
    pub queued: usize,
    pub skipped_dirs: usize,
}

/// 拖放上传启动失败的原因（UI 负责翻译）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ZmodemUploadError {
    /// 已有传输 / 正在等待远端。
    Busy,
    /// 拖入的全是目录或不存在的路径。
    NoFiles,
    /// 会话已断，写不进管道。
    SessionClosed,
    /// 写管道失败。
    WriteFailed,
}

/// 协议状态机当前扮演的角色。
enum Mode {
    /// 没在传输：扫描字节流里的 ZMODEM 起始符。
    Idle,
    /// 已看到起始符、但还差几个字节才能分清这是 `rz` 的 ZRINIT 还是 `sz` 的
    /// ZRQINIT（只有「有待启动的上传」时才需要分）。
    Deciding,
    Receiving(Box<Receiver>),
    Sending(Box<Sender>),
}

struct RecvFile {
    /// 落盘中的临时文件（完成后重命名，失败/中止时删除）。
    part: PathBuf,
    /// 对端给的文件名（已清洗，不含路径）。
    name: String,
    file: File,
    written: u64,
}

struct SendFile {
    file: File,
    size: u64,
    name: String,
}

struct PendingUpload {
    files: VecDeque<PathBuf>,
    since: Instant,
}

struct Inner {
    mode: Mode,
    /// Idle 态尾部等待拼块的前缀（`*` / `**` / `**\x18`）。
    scan: Vec<u8>,
    /// Deciding 态攒下的起始符与后续字节。
    decide: Vec<u8>,
    /// 协议态下本轮喂不进去的字节，等下一批拼接再试。
    proto_tail: Vec<u8>,
    /// 会话刚结束时还能吞掉的协议填充字节数（对端的 `OO` 常和提示符分两批到）。
    trailer: usize,
    pending_upload: Option<PendingUpload>,
    download_dir: PathBuf,
    recv_file: Option<RecvFile>,
    send_file: Option<SendFile>,
    send_remaining: VecDeque<PathBuf>,
    files_done: usize,
    files_total: Option<usize>,
    status: ZmodemStatus,
    notice_at: Option<Instant>,
    last_activity: Instant,
    /// 临时文件序号：同一秒内连收多个文件也不会撞名字。
    seq: u64,
}

impl Inner {
    fn set_notice(&mut self, notice: ZmodemNotice) {
        self.status.phase = ZmodemPhase::Idle;
        self.status.file = None;
        self.status.total = None;
        self.status.transferred = 0;
        self.status.notice = Some(notice);
        self.notice_at = Some(Instant::now());
    }

    fn clear_progress(&mut self) {
        self.status.phase = ZmodemPhase::Idle;
        self.status.file = None;
        self.status.total = None;
        self.status.transferred = 0;
    }

    /// 关掉并删除正在写的半成品（中止 / 出错时）。
    fn cleanup_partial(&mut self) {
        if let Some(rf) = self.recv_file.take() {
            drop(rf.file);
            let _ = std::fs::remove_file(&rf.part);
        }
        self.send_file = None;
    }

    fn begin_recv(&mut self, info: &FileInfo<'_>) -> Result<(), String> {
        self.cleanup_partial();
        std::fs::create_dir_all(&self.download_dir)
            .map_err(|e| format!("无法创建下载目录 {}：{e}", self.download_dir.display()))?;
        let name = sanitize_file_name(info.name);
        let seq = self.seq;
        self.seq = self.seq.wrapping_add(1);
        let part = self
            .download_dir
            .join(format!(".rustshell-zmodem-{seq}.part"));
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&part)
            .map_err(|e| format!("无法创建临时文件 {}：{e}", part.display()))?;
        let total = info.size.map(|p| u64::from(p.get()));
        self.recv_file = Some(RecvFile {
            part,
            name: name.clone(),
            file,
            written: 0,
        });
        self.status.phase = ZmodemPhase::Downloading;
        self.status.file = Some(name);
        self.status.total = total;
        self.status.transferred = 0;
        Ok(())
    }

    fn finish_recv(&mut self) -> Result<(), String> {
        let Some(mut rf) = self.recv_file.take() else {
            return Ok(());
        };
        rf.file
            .flush()
            .map_err(|e| format!("落盘失败（{}）：{e}", rf.name))?;
        drop(rf.file);
        let mut final_path = unique_path(&self.download_dir, &rf.name);
        if std::fs::rename(&rf.part, &final_path).is_err() {
            // 目标可能在「查重名」与「重命名」之间被别的程序占了：换一个名字再试一次。
            final_path = unique_path(&self.download_dir, &rf.name);
            if let Err(e) = std::fs::rename(&rf.part, &final_path) {
                let _ = std::fs::remove_file(&rf.part);
                return Err(format!("无法保存 {}：{e}", rf.name));
            }
        }
        self.files_done += 1;
        self.status.transferred = rf.written;
        self.status.file = Some(rf.name);
        Ok(())
    }

    fn begin_send(&mut self, path: &Path) -> Result<(String, u64), String> {
        let meta = std::fs::metadata(path)
            .map_err(|e| format!("读不到本地文件 {}：{e}", path.display()))?;
        let size = meta.len();
        // zmodem2 的 Position 是 u32（协议本身 ZFILE 的尺寸字段也是 32 位）。
        if size > u64::from(u32::MAX) {
            return Err(format!("文件超过 ZMODEM 的 4 GB 上限：{}", path.display()));
        }
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "file".to_string());
        let file =
            File::open(path).map_err(|e| format!("打不开本地文件 {}：{e}", path.display()))?;
        self.send_file = Some(SendFile {
            file,
            size,
            name: name.clone(),
        });
        self.status.phase = ZmodemPhase::Uploading;
        self.status.file = Some(name.clone());
        self.status.total = Some(size);
        self.status.transferred = 0;
        Ok((name, size))
    }
}

/// 一条终端字节流上的 ZMODEM 收发器。
///
/// 由 `TerminalView` 持有，读线程负责 `feed`，UI 线程负责 `start_upload` /
/// `cancel` / `status`。所有状态都在一把锁里，动作（写管道、读文件、写文件）
/// 也在锁内完成 —— 交错的字节流本来就只允许一个消费者。
pub struct ZmodemPump {
    writer: PtyWriter,
    inner: Mutex<Inner>,
}

impl ZmodemPump {
    pub fn new(writer: PtyWriter, download_dir: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            writer,
            inner: Mutex::new(Inner {
                mode: Mode::Idle,
                scan: Vec::new(),
                decide: Vec::new(),
                proto_tail: Vec::new(),
                trailer: 0,
                pending_upload: None,
                download_dir,
                recv_file: None,
                send_file: None,
                send_remaining: VecDeque::new(),
                files_done: 0,
                files_total: None,
                status: ZmodemStatus::default(),
                notice_at: None,
                last_activity: Instant::now(),
                seq: 0,
            }),
        })
    }

    /// 喂入从传输读到的**原始字节**（转码之前），返回应交给 VTE 的字节。
    ///
    /// 必须先喂这里再转码：ZMODEM 是二进制协议，GBK→UTF-8 之类的转换会把
    /// 报文字节改掉，两端 CRC 立刻对不上。
    pub fn feed(&self, input: &[u8]) -> Vec<u8> {
        let mut st = self.inner.lock();
        st.last_activity = Instant::now();
        let mut out = Vec::new();
        let mut merged;
        let data: &[u8] = if st.proto_tail.is_empty() {
            input
        } else {
            merged = std::mem::take(&mut st.proto_tail);
            merged.extend_from_slice(input);
            &merged
        };
        if data.is_empty() {
            return out;
        }
        let mut input = std::mem::take(&mut st.scan);
        input.extend_from_slice(data);
        self.advance(&mut st, input, &mut out);
        out
    }

    /// 启动一次拖放上传：先键入 `rz -be`，等远端第一帧再真正发文件。
    pub fn start_upload(
        &self,
        paths: Vec<PathBuf>,
    ) -> Result<ZmodemUploadStart, ZmodemUploadError> {
        let mut st = self.inner.lock();
        if st.status.is_active() || !matches!(st.mode, Mode::Idle) {
            return Err(ZmodemUploadError::Busy);
        }
        let mut files = VecDeque::new();
        let mut skipped_dirs = 0;
        for p in paths {
            if p.is_dir() {
                skipped_dirs += 1;
            } else if p.is_file() {
                files.push_back(p);
            }
        }
        if files.is_empty() {
            return Err(ZmodemUploadError::NoFiles);
        }
        if !write_wire(&self.writer, REMOTE_RECEIVE_CMD) {
            return Err(ZmodemUploadError::WriteFailed);
        }
        let n = files.len();
        st.pending_upload = Some(PendingUpload {
            files,
            since: Instant::now(),
        });
        st.files_done = 0;
        st.files_total = Some(n);
        st.status = ZmodemStatus {
            phase: ZmodemPhase::Waiting,
            files_total: Some(n),
            ..ZmodemStatus::default()
        };
        st.notice_at = None;
        st.last_activity = Instant::now();
        Ok(ZmodemUploadStart {
            queued: n,
            skipped_dirs,
        })
    }

    /// 取消当前传输 / 待启动的上传。已发出取消序列，对端会一并收手。
    pub fn cancel(&self) {
        let mut st = self.inner.lock();
        self.cancel_locked(&mut st);
    }

    /// 重挂会话前复位（旧管道上的传输不能跟到新会话里）。
    pub fn reset(&self) {
        self.cancel();
        let mut st = self.inner.lock();
        st.scan.clear();
        st.decide.clear();
        st.proto_tail.clear();
        st.trailer = 0;
        st.pending_upload = None;
        st.status = ZmodemStatus::default();
        st.notice_at = None;
    }

    /// 超时判定（由宿主的轮询心跳驱动）。
    pub fn tick(&self) -> bool {
        let mut st = self.inner.lock();
        let mut changed = false;
        let pending_timeout = st
            .pending_upload
            .as_ref()
            .is_some_and(|p| p.since.elapsed() >= UPLOAD_READY_TIMEOUT);
        if pending_timeout {
            st.pending_upload = None;
            st.decide.clear();
            if matches!(st.mode, Mode::Idle | Mode::Deciding) {
                st.mode = Mode::Idle;
                st.set_notice(ZmodemNotice::RemoteNotReady);
                changed = true;
            }
        }
        let stalled = matches!(st.mode, Mode::Receiving(_) | Mode::Sending(_))
            && st.last_activity.elapsed() >= STALL_TIMEOUT;
        if stalled {
            self.cancel_locked(&mut st);
            st.set_notice(ZmodemNotice::Stalled);
            changed = true;
        }
        changed
    }

    pub fn is_active(&self) -> bool {
        let st = self.inner.lock();
        // 看 `mode` 而不只看 status：刚认出起始符、帧头还没收全（Deciding），
        // 或接收器已就位但还没开始第一个文件时，status.phase 仍是 Idle ——
        // 此刻输入闸门已经必须关上，否则用户敲一个键就污染了协议流。
        st.status.is_active() || !matches!(st.mode, Mode::Idle)
    }

    /// 当前状态快照。结束态提示超过 `NOTICE_TTL` 后在这里过期。
    pub fn status(&self) -> ZmodemStatus {
        let mut st = self.inner.lock();
        if st.notice_at.is_some_and(|t| t.elapsed() >= NOTICE_TTL) {
            st.status.notice = None;
            st.notice_at = None;
        }
        st.status.clone()
    }

    fn cancel_locked(&self, st: &mut Inner) {
        let was_active = st.status.is_active() || st.pending_upload.is_some();
        st.pending_upload = None;
        st.decide.clear();
        if matches!(st.mode, Mode::Idle) {
            st.cleanup_partial();
            if was_active {
                st.set_notice(ZmodemNotice::Cancelled);
            }
            return;
        }
        let mut mode = std::mem::replace(&mut st.mode, Mode::Idle);
        match &mut mode {
            Mode::Receiving(rx) => {
                let _ = rx.abort();
                let ended = self.run_receiver(st, rx);
                debug_assert!(ended, "abort 后接收状态机应当立即结束");
            }
            Mode::Sending(tx) => {
                tx.abort();
                let ended = self.run_sender(st, tx);
                debug_assert!(ended, "abort 后发送状态机应当立即结束");
            }
            _ => {}
        }
        st.cleanup_partial();
        if st.status.phase.is_active() {
            st.clear_progress();
        }
        if st.status.notice.is_none() {
            st.set_notice(ZmodemNotice::Cancelled);
        }
    }

    /// 把「扫字节流」与「跑协议」串起来。
    fn advance(&self, st: &mut Inner, mut input: Vec<u8>, out: &mut Vec<u8>) {
        // 本轮是否已经历过一次「submit 吃不下」；用来避免无输入可推进时死循环。
        let mut stalled_once = false;
        loop {
            match &mut st.mode {
                Mode::Idle => {
                    // 会话刚结束时，对端的收尾填充（`OO` + CR/LF/XON/XOFF/DLE）常常
                    // 和提示符分两批到达，所以用一个小额度**跨批**吞掉；一见到普通
                    // 字节就关窗，绝不停止把正常输出交给 VTE。
                    if st.trailer > 0 {
                        let n = input
                            .iter()
                            .take(st.trailer)
                            .take_while(|b| is_trailer_byte(**b))
                            .count();
                        st.trailer -= n;
                        input.drain(..n);
                        if input.is_empty() {
                            return;
                        }
                        st.trailer = 0;
                    }
                    let Some(ix) = find_start(&input) else {
                        let keep = partial_suffix_len(&input);
                        let split = input.len() - keep;
                        out.extend_from_slice(&input[..split]);
                        st.scan = input[split..].to_vec();
                        return;
                    };
                    out.extend_from_slice(&input[..ix]);
                    let tail = input.split_off(ix);
                    if st.pending_upload.is_some() {
                        // 有待启动的上传：先分清对端是在应答我们（ZRINIT）
                        // 还是自己要发文件（ZRQINIT）。
                        st.mode = Mode::Deciding;
                        st.decide = tail;
                        return self.advance_deciding(st, out);
                    }
                    st.mode = Mode::Receiving(Box::new(new_receiver()));
                    input = tail;
                }
                Mode::Deciding => return self.advance_deciding(st, out),
                Mode::Receiving(_) | Mode::Sending(_) => {
                    if !st.proto_tail.is_empty() {
                        let mut merged = std::mem::take(&mut st.proto_tail);
                        merged.append(&mut input);
                        input = merged;
                    }
                    // ★ 先把状态机推到“无事可做”再决定还要不要输入。
                    //   `submit_wire` 可能只是把应答排进发送队列就返回了；
                    //   若此时 input 恰好吃完，旧代码会在 drive 之前 return，
                    //   于是「对端等我们的应答、我们等对端的下一帧」→ 双方挂死。
                    if self.drive(st) {
                        // 会话结束：`drive` 已把 mode 收回 Idle 并开了收尾额度，
                        // 剩下的字节（`OO`、提示符、错误信息）交给 Idle 分支处理。
                        continue;
                    }
                    if input.is_empty() {
                        return;
                    }
                    match self.submit(st, &input) {
                        Ok(0) => {
                            // 再给一轮机会：上一次 submit 排出的应答可能正是
                            // 卡住解析的原因，drive 写出去之后就能继续吃。
                            if !stalled_once {
                                stalled_once = true;
                                continue;
                            }
                            if st.proto_tail.len() + input.len() > MAX_PROTO_TAIL {
                                self.fail(st, "协议数据无法解析（缓冲区已满）");
                                out.extend_from_slice(&input);
                                return;
                            }
                            st.proto_tail.extend_from_slice(&input);
                            return;
                        }
                        Ok(n) => {
                            stalled_once = false;
                            input.drain(..n);
                            // 继续循环：可能还有一段完整的报文 / 会话结束后的文字
                        }
                        Err(e) => {
                            self.fail(st, e);
                            out.extend_from_slice(&input);
                            return;
                        }
                    }
                }
            }
        }
    }

    /// Deciding 态：等够字节判断帧类型，再决定扮演发送方还是接收方。
    fn advance_deciding(&self, st: &mut Inner, out: &mut Vec<u8>) {
        let buf = std::mem::take(&mut st.decide);
        match frame_kind(&buf) {
            Some(1) => {
                // ZRINIT：远端 `rz` 就位 ⇒ 这次是我们上传。
                let pending = st.pending_upload.take();
                let Some(pending) = pending else {
                    st.mode = Mode::Receiving(Box::new(new_receiver()));
                    self.advance(st, buf, out);
                    return;
                };
                match start_sender(st, pending) {
                    Ok(sender) => {
                        st.mode = Mode::Sending(Box::new(sender));
                        let mut input = buf;
                        if self.drive(st) {
                            return self.advance(st, input, out);
                        }
                        match self.submit(st, &input) {
                            Ok(n) => {
                                input.drain(..n);
                                self.advance(st, input, out);
                            }
                            Err(e) => {
                                self.fail(st, e);
                                out.extend_from_slice(&input);
                            }
                        }
                    }
                    Err(e) => {
                        st.pending_upload = None;
                        self.fail(st, e);
                        out.extend_from_slice(&buf);
                    }
                }
            }
            Some(_) => {
                // ZRQINIT 等：对端要发文件（`sz`），还是下载。
                st.pending_upload = None;
                st.mode = Mode::Receiving(Box::new(new_receiver()));
                self.advance(st, buf, out);
            }
            None => {
                if buf.len() > 16 {
                    // 看起来像起始符却读不出帧类型：按普通输出处理，别把用户
                    // 的文字永远扣在缓冲区里。
                    st.decide.clear();
                    st.mode = Mode::Idle;
                    if st.pending_upload.take().is_some() {
                        st.set_notice(ZmodemNotice::Failed("无法识别的 ZMODEM 报文".to_string()));
                    }
                    out.extend_from_slice(&buf);
                    return;
                }
                st.decide = buf;
            }
        }
    }

    /// 驱动当前状态机到 `Idle`（把该写的写出去、该落盘的落盘）。返回会话是否结束。
    fn drive(&self, st: &mut Inner) -> bool {
        let mut mode = std::mem::replace(&mut st.mode, Mode::Idle);
        let ended = match &mut mode {
            Mode::Receiving(rx) => self.run_receiver(st, rx),
            Mode::Sending(tx) => self.run_sender(st, tx),
            _ => {
                st.mode = mode;
                return false;
            }
        };
        if !ended {
            st.mode = mode;
        } else {
            drop(mode);
            st.trailer = TRAILER_BUDGET;
            if st.status.phase.is_active() {
                st.clear_progress();
            }
        }
        ended
    }

    /// 把一批字节交给当前状态机，返回消费量。
    fn submit(&self, st: &mut Inner, input: &[u8]) -> Result<usize, String> {
        match &mut st.mode {
            Mode::Receiving(rx) => rx
                .submit_wire(input)
                .map_err(|e| format!("协议解析失败：{e}")),
            Mode::Sending(tx) => tx
                .submit_wire(input)
                .map_err(|e| format!("协议解析失败：{e}")),
            _ => Ok(0),
        }
    }

    fn run_receiver(&self, st: &mut Inner, rx: &mut Receiver) -> bool {
        // 会话结束事件到达时可能还有收尾字节（OO 等）挂在发送队列里：
        // 不能立刻返回，否则对端永远等不到应答、迟迟不结束。
        let mut session_done = false;
        loop {
            match rx.poll() {
                Action::WriteWire(buf) => {
                    let n = buf.len();
                    if !write_wire(&self.writer, buf) {
                        self.abort_receiver(rx);
                        self.fail(st, "写入终端失败");
                        return true;
                    }
                    rx.wire_written(n);
                }
                Action::WriteFile(buf) => {
                    let n = buf.len();
                    let written = match st.recv_file.as_mut() {
                        Some(rf) => match rf.file.write_all(buf) {
                            Ok(()) => {
                                rf.written += n as u64;
                                Ok(rf.written)
                            }
                            Err(e) => Err(format!("写入下载文件失败：{e}")),
                        },
                        None => Err("收到数据但没有待写的文件".to_string()),
                    };
                    match written {
                        Ok(written) => {
                            st.status.transferred = written;
                            if let Err(e) = rx.file_written(n) {
                                self.abort_receiver(rx);
                                self.fail(st, format!("协议状态错误：{e}"));
                                return true;
                            }
                        }
                        Err(e) => {
                            self.abort_receiver(rx);
                            self.fail(st, e);
                            return true;
                        }
                    }
                }
                Action::Event(ev) => match ev {
                    ZmodemEvent::FileStarted(info) => {
                        if let Err(e) = st.begin_recv(&info) {
                            self.abort_receiver(rx);
                            self.fail(st, e);
                            return true;
                        }
                    }
                    ZmodemEvent::FileCompleted => {
                        if let Err(e) = st.finish_recv() {
                            self.abort_receiver(rx);
                            self.fail(st, e);
                            return true;
                        }
                    }
                    ZmodemEvent::SessionCompleted => {
                        st.cleanup_partial();
                        let files = st.files_done;
                        let dir = Some(st.download_dir.display().to_string());
                        st.set_notice(ZmodemNotice::Completed { files, dir });
                        session_done = true;
                    }
                    ZmodemEvent::Aborted => {
                        st.cleanup_partial();
                        st.set_notice(ZmodemNotice::Cancelled);
                        session_done = true;
                    }
                    _ => {}
                },
                Action::Idle => return session_done,
                _ => {}
            }
        }
    }

    fn run_sender(&self, st: &mut Inner, tx: &mut Sender) -> bool {
        loop {
            match tx.poll() {
                Action::WriteWire(buf) => {
                    let n = buf.len();
                    if !write_wire(&self.writer, buf) {
                        tx.abort();
                        self.fail(st, "写入终端失败");
                        return true;
                    }
                    tx.wire_written(n);
                }
                Action::ReadFile { offset, max_len } => {
                    let result = st.send_file.as_mut().map_or_else(
                        || Err("发送器请求数据但没有打开文件".to_string()),
                        |sf| {
                            let start = u64::from(offset.get());
                            // `max_len` 是发送器能接受的上限，不是「必须凑满」。
                            // 开了 streaming window 后它可能大到几个 GB ——
                            // 照单分配会直接把进程撑爆；按剩余大小并限块返回。
                            let remaining = sf.size.saturating_sub(start);
                            let want = max_len.min(remaining as usize).min(64 * 1024);
                            if want == 0 {
                                return Err(format!("本地文件提前结束（{}）", sf.name));
                            }
                            sf.file
                                .seek(SeekFrom::Start(start))
                                .map_err(|e| format!("读取本地文件失败：{e}"))?;
                            let mut buf = vec![0u8; want];
                            let n = sf
                                .file
                                .read(&mut buf)
                                .map_err(|e| format!("读取本地文件失败：{e}"))?;
                            if n == 0 {
                                return Err(format!("本地文件提前结束（{}）", sf.name));
                            }
                            buf.truncate(n);
                            Ok((buf, start + n as u64))
                        },
                    );
                    match result {
                        Ok((data, transferred)) => {
                            if let Err(e) = tx.submit_file(&data) {
                                tx.abort();
                                self.fail(st, format!("协议状态错误：{e}"));
                                return true;
                            }
                            st.status.transferred = transferred;
                        }
                        Err(e) => {
                            tx.abort();
                            self.fail(st, e);
                            return true;
                        }
                    }
                }
                Action::Event(ev) => match ev {
                    ZmodemEvent::FileCompleted => {
                        st.files_done += 1;
                        st.send_file = None;
                        match st.send_remaining.pop_front() {
                            Some(next) => {
                                let started = st.begin_send(&next).and_then(|(name, size)| {
                                    tx.start_file(FileInfo::new(
                                        name.as_bytes(),
                                        Some(Position::new(size as u32)),
                                    ))
                                    .map_err(|e| format!("启动下一个文件失败：{e}"))
                                });
                                if let Err(e) = started {
                                    tx.abort();
                                    self.fail(st, e);
                                    return true;
                                }
                            }
                            None => {
                                if let Err(e) = tx.finish() {
                                    tx.abort();
                                    self.fail(st, format!("结束会话失败：{e}"));
                                    return true;
                                }
                            }
                        }
                    }
                    ZmodemEvent::SessionCompleted => {
                        st.send_file = None;
                        st.send_remaining.clear();
                        let files = st.files_done;
                        st.set_notice(ZmodemNotice::Completed { files, dir: None });
                        return true;
                    }
                    ZmodemEvent::Aborted => {
                        st.send_file = None;
                        st.send_remaining.clear();
                        st.set_notice(ZmodemNotice::Cancelled);
                        return true;
                    }
                    _ => {}
                },
                Action::Idle => return false,
                _ => {}
            }
        }
    }

    fn abort_receiver(&self, rx: &mut Receiver) {
        let _ = rx.abort();
        loop {
            match rx.poll() {
                Action::WriteWire(buf) => {
                    let n = buf.len();
                    if !write_wire(&self.writer, buf) {
                        return;
                    }
                    rx.wire_written(n);
                }
                Action::Idle => return,
                _ => {}
            }
        }
    }

    fn fail(&self, st: &mut Inner, detail: impl Into<String>) {
        st.cleanup_partial();
        st.send_remaining.clear();
        st.pending_upload = None;
        st.set_notice(ZmodemNotice::Failed(detail.into()));
    }
}

/// 会话结束后紧跟在线路上的收尾字节：对端回给我们的 `OO`（over-and-out），
/// 以及 lrzsz 习惯在帧后补的 CR/LF、XON/XOFF、DLE。它们不是文字，照原样上屏
/// 就是提示符前凭空多出的两个 `O`。
fn is_trailer_byte(b: u8) -> bool {
    matches!(b, b'O' | b'\r' | b'\n' | 0x11 | 0x13 | 0x18)
}

fn new_receiver() -> Receiver {
    // 传输都是可靠、带流控的（PTY / TCP / SSH）：通告非停等 I/O，
    // 让对端连续发数据，否则每个 1KB 子包都要等一个往返。
    Receiver::with_flow_control(0, true).unwrap_or_else(|_| Receiver::new().expect("初始接收器"))
}

fn start_sender(st: &mut Inner, pending: PendingUpload) -> Result<Sender, String> {
    let mut sender = Sender::new().map_err(|e| format!("初始化发送器失败：{e}"))?;
    // 可靠链路上把窗口开到最大：每次等待 ACK 都是一个完整往返。
    sender.set_streaming_window(usize::MAX);
    // `Sender::new()` 会先排一帧 ZRQINIT（“发送方主动叫醒接收方”）。
    // 但拖放上传是被远端的 ZRINIT 唤醒的 —— `rz` 的接收端已经就位并主动
    // 打了招呼，此时再发 ZRQINIT 会让部分实现（包括 zmodem2 的 Receiver）
    // 把它当成失序帧，后面的 ZFILE 随之丢掉。与 `sz` 的行为对齐：把这一帧
    // 从发送队列里取走丢弃，再直接处理已经收到的 ZRINIT。
    let init_len = match sender.poll() {
        Action::WriteWire(buf) => buf.len(),
        _ => 0,
    };
    if init_len > 0 {
        sender.wire_written(init_len);
    }
    let mut files = pending.files;
    let n = files.len();
    st.files_done = 0;
    st.files_total = Some(n);
    let Some(first) = files.pop_front() else {
        return Err("没有可上传的文件".to_string());
    };
    let (name, size) = st.begin_send(&first)?;
    sender
        .start_file(FileInfo::new(
            name.as_bytes(),
            Some(Position::new(size as u32)),
        ))
        .map_err(|e| format!("启动上传失败：{e}"))?;
    st.send_remaining = files;
    Ok(sender)
}

fn write_wire(writer: &PtyWriter, bytes: &[u8]) -> bool {
    let mut w = writer.lock();
    w.write_all(bytes).and_then(|()| w.flush()).is_ok()
}

/// 在字节流里找一个 ZMODEM 报文起始符。
///
/// ZHEX（握手帧都用它）是 `**\x18B`；ZBIN / ZBIN32 只有单个 `*`：
/// `*\x18A` / `*\x18C`。两种都认，免得换个对端实现就失联。
fn find_start(buf: &[u8]) -> Option<usize> {
    if buf.len() < 3 {
        return None;
    }
    let mut i = 0;
    while i + 2 < buf.len() {
        if buf[i] == ZPAD {
            if buf[i + 1] == ZDLE && matches!(buf[i + 2], b'A' | b'C') {
                return Some(i);
            }
            if i + 3 < buf.len() && buf[i + 1] == ZPAD && buf[i + 2] == ZDLE && buf[i + 3] == b'B' {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

/// 缓冲区尾部有多少字节可能是「被切断的起始符」。
///
/// 只保留这些字节，其余立刻交给 VTE —— 否则每读一块都要把整块扣住等下一块，
/// 输出会出现「一块一块」的延迟。
fn partial_suffix_len(buf: &[u8]) -> usize {
    let n = buf.len();
    if n >= 3 && &buf[n - 3..] == b"**\x18" {
        return 3;
    }
    if n >= 2 && (&buf[n - 2..] == b"**" || &buf[n - 2..] == b"*\x18") {
        return 2;
    }
    if n >= 1 && buf[n - 1] == ZPAD {
        return 1;
    }
    0
}

/// `buf` 以起始符开头时，解析出帧类型；字节还不够时返回 `None`。
fn frame_kind(buf: &[u8]) -> Option<u8> {
    if buf.len() >= 4 && buf[0] == ZPAD && buf[1] == ZPAD && buf[2] == ZDLE && buf[3] == b'B' {
        if buf.len() < 6 {
            return None;
        }
        let hi = hex_value(buf[4])?;
        let lo = hex_value(buf[5])?;
        return Some(hi * 16 + lo);
    }
    if buf.len() >= 4 && buf[0] == ZPAD && buf[1] == ZDLE && matches!(buf[2], b'A' | b'C') {
        let mut frame = buf[3];
        if frame == ZDLE {
            // 转义过的帧类型：`ZDLE` 后跟 `原值 XOR 0x40`
            if buf.len() < 5 {
                return None;
            }
            frame = buf[4] ^ 0x40;
        }
        return Some(frame);
    }
    None
}

fn hex_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// 把对端给的文件名压成一个安全的本地文件名。
///
/// ZMODEM 的 ZFILE 里是任意字节，且**可以带路径**（恶意或损坏的对端能塞
/// `../../.ssh/authorized_keys`）。这里只取最后一段，再去掉 Windows 非法字符，
/// 保证落点一定在下载目录内。
fn sanitize_file_name(raw: &[u8]) -> String {
    let lossy = String::from_utf8_lossy(raw);
    let base = lossy.rsplit(['/', '\\']).next().unwrap_or("").trim();
    let mut out: String = base
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*') {
                '_'
            } else {
                c
            }
        })
        .take(180)
        .collect();
    out = out.trim_end_matches(['.', ' ']).to_string();
    if out.is_empty() || out == "." || out == ".." {
        out = "download".to_string();
    }
    out
}

/// 在 `dir` 里给 `name` 找一个不覆盖既有文件的名字。
fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let path = Path::new(name);
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "download".to_string());
    let ext = path
        .extension()
        .map(|s| format!(".{}", s.to_string_lossy()))
        .unwrap_or_default();
    for n in 1..=9999 {
        let candidate = dir.join(format!("{stem} ({n}){ext}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    dir.join(format!("{stem}-{}{ext}", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    static TEST_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let seq = TEST_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "rustshell-zmodem-{}-{tag}-{seq}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("创建测试目录");
            Self(dir)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// 记录写进 PTY 的字节。
    #[derive(Clone, Default)]
    struct SpyWriter(Arc<Mutex<Vec<u8>>>);

    impl SpyWriter {
        fn take(&self) -> Vec<u8> {
            std::mem::take(&mut self.0.lock())
        }
    }

    impl Write for SpyWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn pump(dir: &TempDir, spy: &SpyWriter) -> Arc<ZmodemPump> {
        let writer: Box<dyn Write + Send> = Box::new(spy.clone());
        ZmodemPump::new(Arc::new(Mutex::new(writer)), dir.path().to_path_buf())
    }

    /// 普通输出原样透传；起始符被切在两次 read 之间也能认出来。
    #[test]
    fn text_passes_through_and_split_headers_are_detected() {
        let dir = TempDir::new("split");
        let spy = SpyWriter::default();
        let pump = pump(&dir, &spy);

        assert_eq!(pump.feed(b"hello\r\n"), b"hello\r\n");
        // `**` 是潜在起始符的前缀，先扣住，其余照常上屏
        assert_eq!(pump.feed(b"tail **"), b"tail ");
        assert!(!pump.is_active());
        // 下一块补上 `\x18B`：识别成 ZMODEM，缓冲区里的内容不再进 VTE
        assert!(pump.feed(b"\x18B").is_empty());
        assert!(pump.is_active());
    }

    /// 把发送器的待办（写线 / 读文件 / 事件）跑空。
    /// 返回是否真的做了事 —— `submit_wire` 返回 0 时先跑它再决定放弃。
    fn drive_sender(
        sender: &mut Sender,
        payload: &[u8],
        to_pump: &mut Vec<u8>,
        completed: &mut bool,
    ) -> bool {
        let mut acted = false;
        loop {
            match sender.poll() {
                Action::WriteWire(bytes) => {
                    let n = bytes.len();
                    to_pump.extend_from_slice(bytes);
                    sender.wire_written(n);
                    acted = true;
                }
                Action::ReadFile { offset, max_len } => {
                    let start = (offset.get() as usize).min(payload.len());
                    let end = start.saturating_add(max_len).min(payload.len());
                    sender
                        .submit_file(&payload[start..end])
                        .expect("submit file");
                    acted = true;
                }
                Action::Event(ZmodemEvent::SessionCompleted) => {
                    *completed = true;
                    acted = true;
                }
                Action::Idle => break,
                _ => {}
            }
        }
        acted
    }

    /// 把接收器的待办跑空（上传测试里的「远端 rz」）。
    fn drive_receiver(
        remote: &mut Receiver,
        to_pump: &mut Vec<u8>,
        received: &mut Vec<u8>,
        got_file: &mut bool,
        done: &mut bool,
    ) -> bool {
        let mut acted = false;
        loop {
            match remote.poll() {
                Action::WriteWire(bytes) => {
                    let n = bytes.len();
                    to_pump.extend_from_slice(bytes);
                    remote.wire_written(n);
                    acted = true;
                }
                Action::WriteFile(bytes) => {
                    let chunk = bytes.to_vec();
                    received.extend_from_slice(&chunk);
                    remote.file_written(chunk.len()).expect("file written");
                    acted = true;
                }
                Action::Event(ZmodemEvent::FileCompleted) => {
                    *got_file = true;
                    acted = true;
                }
                Action::Event(ZmodemEvent::SessionCompleted) => {
                    *done = true;
                    acted = true;
                }
                Action::Idle => break,
                _ => {}
            }
        }
        acted
    }

    /// 下载全流程：`sz` 侧用 zmodem2 的 Sender 真跑一遍握手与数据，
    /// 落盘的文件内容必须与源文件逐字节一致。
    #[test]
    fn download_round_trip_saves_the_file() {
        let dir = TempDir::new("download");
        let spy = SpyWriter::default();
        let pump = pump(&dir, &spy);

        let payload: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
        let mut sender = Sender::new().expect("sender");
        sender
            .start_file(FileInfo::new(
                b"hello.bin",
                Some(Position::new(payload.len() as u32)),
            ))
            .expect("start_file");
        // 单文件传输：现在就请求收尾（ZFIN 会在本文件 ZEOF 被确认后发出）。
        sender.finish().expect("finish");

        let mut to_pump: Vec<u8> = Vec::new();
        let mut out = Vec::new();
        // 发送端暂时吃不下的字节（它要先吐出自己的应答才能继续）留在这里，
        // 下一轮接着交 —— 直接丢掉就等于撕掉正在传输的数据。
        let mut pending: Vec<u8> = Vec::new();
        let mut completed = false;
        let mut idle_rounds = 0;
        for _ in 0..10_000 {
            let mut acted = false;
            acted |= drive_sender(&mut sender, &payload, &mut to_pump, &mut completed);
            if !to_pump.is_empty() {
                out.extend_from_slice(&pump.feed(&to_pump));
                to_pump.clear();
            }
            let before = pending.len();
            pending.extend(spy.take());
            acted |= pending.len() != before;
            while !pending.is_empty() {
                let n = sender.submit_wire(&pending).expect("submit wire");
                if n == 0 {
                    break;
                }
                pending.drain(..n);
                acted = true;
            }
            if completed && !pump.is_active() {
                break;
            }
            idle_rounds = if acted { 0 } else { idle_rounds + 1 };
            if idle_rounds > 3 {
                break;
            }
        }
        assert!(completed, "下载会话没有正常结束");
        assert!(!pump.is_active(), "下载结束后状态机仍在活动");
        // 收尾的 `OO` 与 CR/LF 填充属于协议字节，不该出现在终端上。
        assert!(out.is_empty(), "协议字节漏进了终端输出：{out:02x?}");

        let saved = dir.path().join("hello.bin");
        let got = std::fs::read(&saved).expect("下载文件不存在");
        assert_eq!(got, payload, "落盘内容与源文件不一致");
        assert!(matches!(
            pump.status().notice,
            Some(ZmodemNotice::Completed { files: 1, .. })
        ));
        // 传输结束后远端提示符照常上屏（收尾扣留只针对那一小段协议填充）。
        assert_eq!(pump.feed(b"user@host:~$ "), b"user@host:~$ ");
    }

    /// 上传全流程：拖放 → 键入 `rz -be` → 远端（zmodem2 Receiver）收到完整文件。
    #[test]
    fn upload_round_trip_streams_the_file() {
        let dir = TempDir::new("upload");
        let spy = SpyWriter::default();
        let pump = pump(&dir, &spy);

        let payload: Vec<u8> = (0..3000u32).map(|i| (i % 239) as u8).collect();
        let local = dir.path().join("send.bin");
        std::fs::write(&local, &payload).expect("写本地文件");
        let start = pump.start_upload(vec![local]).expect("start_upload 应成功");
        assert_eq!(start.queued, 1);
        assert_eq!(pump.status().phase, ZmodemPhase::Waiting);

        let mut remote = Receiver::with_flow_control(0, true).expect("receiver");
        let mut received = Vec::new();
        let mut got_file = false;

        // 远端 `rz` 先发 ZRINIT；pump 看到它才启动发送端
        let replies = spy.take();
        assert_eq!(replies, b"rz -be\r");
        let mut to_pump = Vec::new();
        let mut pending: Vec<u8> = Vec::new();
        let mut done = false;
        let mut idle_rounds = 0;
        for _ in 0..10_000 {
            let mut acted = false;
            acted |= drive_receiver(
                &mut remote,
                &mut to_pump,
                &mut received,
                &mut got_file,
                &mut done,
            );
            if !to_pump.is_empty() {
                pump.feed(&to_pump);
                to_pump.clear();
            }
            let before = pending.len();
            pending.extend(spy.take());
            acted |= pending.len() != before;
            while !pending.is_empty() {
                let n = remote.submit_wire(&pending).expect("submit wire");
                if n == 0 {
                    break;
                }
                pending.drain(..n);
                acted = true;
            }
            if done && !pump.is_active() {
                break;
            }
            idle_rounds = if acted { 0 } else { idle_rounds + 1 };
            if idle_rounds > 3 {
                break;
            }
        }
        assert!(done, "上传会话没有正常结束");
        assert!(got_file, "远端没有收到完整的文件");
        assert_eq!(received, payload, "远端收到的内容与本地文件不一致");
        assert!(matches!(
            pump.status().notice,
            Some(ZmodemNotice::Completed {
                files: 1,
                dir: None
            })
        ));
    }

    /// 远端迟迟不进入 ZMODEM（没装 rz）→ 超时收尾，且期间文字正常上屏。
    #[test]
    fn pending_upload_times_out_when_remote_never_start() {
        let dir = TempDir::new("timeout");
        let spy = SpyWriter::default();
        let pump = pump(&dir, &spy);
        let local = dir.path().join("x.txt");
        std::fs::write(&local, b"x").expect("写文件");
        pump.start_upload(vec![local]).expect("start_upload");
        assert!(pump.is_active());

        // 远端报错是普通文字，必须照常显示
        assert_eq!(
            pump.feed(b"bash: rz: command not found\r\n"),
            b"bash: rz: command not found\r\n"
        );

        {
            let mut st = pump.inner.lock();
            st.pending_upload.as_mut().expect("pending").since =
                Instant::now() - UPLOAD_READY_TIMEOUT - Duration::from_secs(1);
        }
        assert!(pump.tick());
        assert!(!pump.is_active());
        assert_eq!(pump.status().notice, Some(ZmodemNotice::RemoteNotReady));
    }

    /// 取消活动传输：不发文件、立即回到空闲，并给出取消提示。
    #[test]
    fn cancel_stops_an_active_transfer() {
        let dir = TempDir::new("cancel");
        let spy = SpyWriter::default();
        let pump = pump(&dir, &spy);
        let local = dir.path().join("x.txt");
        std::fs::write(&local, b"x").expect("写文件");
        pump.start_upload(vec![local]).expect("start_upload");
        pump.cancel();
        assert!(!pump.is_active());
        assert_eq!(pump.status().notice, Some(ZmodemNotice::Cancelled));
    }

    /// ZFILE 里的路径必须被压平，不能写到下载目录之外。
    #[test]
    fn file_names_are_sanitized_into_the_download_dir() {
        for (raw, want) in [
            ("../../.ssh/authorized_keys", "authorized_keys"),
            ("C:\\Windows\\evil.exe", "evil.exe"),
            ("a<b>c:d.txt", "a_b_c_d.txt"),
            ("", "download"),
            ("..", "download"),
        ] {
            assert_eq!(sanitize_file_name(raw.as_bytes()), want, "raw={raw:?}");
        }
        let dir = TempDir::new("names");
        std::fs::write(dir.path().join("a.txt"), b"x").unwrap();
        assert_eq!(
            unique_path(dir.path(), "a.txt"),
            dir.path().join("a (1).txt")
        );
    }

    #[test]
    fn zhex_frame_numbers_are_decoded() {
        assert_eq!(frame_kind(b"**\x18B00"), Some(0));
        assert_eq!(frame_kind(b"**\x18B01"), Some(1));
        assert_eq!(frame_kind(b"**\x18B0A"), Some(10));
        assert_eq!(frame_kind(b"**\x18B0"), None);
        assert_eq!(frame_kind(b"*\x18A\x01"), Some(1));
    }
}
