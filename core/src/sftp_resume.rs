//! SFTP 未完成传输的「偏移账本」（F-13 断点续传）。
//!
//! # 为什么落盘，而不是只放在服务任务内存里
//!
//! 最需要续传的场景恰恰是**断线**：网络断了、会话没了，用户重连回来再传一次。
//! 记录若只活在 `spawn_sftp_service` 那个任务里，连接一断就随任务一起消失 ——
//! 续传能力在使用它的场景里失效。落盘之后顺带换来「应用重启后仍可续传」。
//!
//! # 为什么 key 不做 canonicalize
//!
//! key 是 `(方向, 本地路径, 远端路径)`，两侧路径都**原样字符串**比对：
//! 服务侧记的 local 与发起时用的 `PathBuf` 同源，UI 侧查的 local 也是同一个来源，
//! 原样比即可命中。做归一（`canonicalize` / 大小写折叠）反而会把同一文件的不同写法
//! 拆成两条记录，或者把两个文件并成一条 —— 前者丢续传、后者更糟。
//!
//! # 节流
//!
//! 传输循环每 64 KiB 就会推进一次偏移，每次都落盘等于给传输加一路随机写。
//! 所以内存里的记录**始终是精确的**，落盘按 2s 节流（见 [`ResumeStore::record`]）；
//! 传输结束或失败时由调用方 [`ResumeStore::flush`] 立即收口。
//! 口径因此是「不重复写**已确认落盘**的部分」，最多重传最后 2s 那一窗的进度。

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// 记录文件的落盘节流间隔。
pub const FLUSH_EVERY: Duration = Duration::from_secs(2);
/// 记录条数上限；超出按 `updated` 淘汰最旧。
pub const MAX_MARKS: usize = 64;
/// 记录的保鲜期（秒）；更老的条目在加载/保存时丢弃。
pub const MAX_AGE_SECS: u64 = 7 * 24 * 3600;

/// 传输方向（决定「谁是目标」——守卫的判据按方向不同）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TransferDir {
    Download,
    Upload,
}

/// 一条未完成传输的记录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumeMark {
    pub dir: TransferDir,
    pub local: String,
    pub remote: String,
    /// 已确认落盘的字节偏移
    pub bytes: u64,
    /// 已知总量；0 = 未知（远端大小拿不到时）
    pub total: u64,
    /// 最后更新时间（unix 秒），用于过期与淘汰
    pub updated: u64,
}

impl ResumeMark {
    /// 记一条记录（时间戳由这里盖：调用方只管进度，不该关心过期口径）。
    pub fn new(
        dir: TransferDir,
        local: impl Into<String>,
        remote: impl Into<String>,
        bytes: u64,
        total: u64,
    ) -> Self {
        Self {
            dir,
            local: local.into(),
            remote: remote.into(),
            bytes,
            total,
            updated: now_secs(),
        }
    }
}

/// 续传判定结果。
///
/// `Restart` 带原因字符串：它会经 `SftpEvent::TransferNote` 显示在传输队列上 ——
/// 「为什么又是从头开始」必须是看得见的，否则用户只会觉得续传时灵时不灵。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumePlan {
    Restart(&'static str),
    Resume { from: u64 },
}

/// 续传判定（纯函数，无 IO）。
///
/// 守卫分两侧，判据由**谁是目标**决定：
/// - 目标侧（下载=本地文件 / 上传=远端文件）已写部分必须**恰好**等于偏移 ——
///   多了说明被别的写入碰过，少了说明半成品不完整，两种都不能接着往后写；
/// - 源侧（下载=远端 / 上传=本地）不得短于偏移，且总量已知时必须**没变过**。
///
/// 判据用 size 而不用 mtime：mtime 跨平台精度与时区口径不一（Windows 秒级、
/// Unix 纳秒），比对极易假阳性 —— 假阳性的代价是把能续的传输判成不能续。
/// 「同大小不同内容」这一漏网情形由可选校验（下载后 SHA-256）兜底。
pub fn plan_resume(mark: Option<&ResumeMark>, local_len: u64, remote_len: u64) -> ResumePlan {
    let Some(m) = mark else {
        return ResumePlan::Restart("无未完成记录");
    };
    if m.bytes == 0 {
        return ResumePlan::Restart("没有已传数据");
    }
    match m.dir {
        TransferDir::Download => {
            if local_len != m.bytes {
                return ResumePlan::Restart("本地文件已变化");
            }
            if remote_len < m.bytes {
                return ResumePlan::Restart("远端文件已变化");
            }
            if m.total > 0 && remote_len != m.total {
                return ResumePlan::Restart("远端文件已变化");
            }
        }
        TransferDir::Upload => {
            if remote_len != m.bytes {
                return ResumePlan::Restart("远端文件已变化");
            }
            if local_len < m.bytes {
                return ResumePlan::Restart("本地源文件已变化");
            }
            if m.total > 0 && local_len != m.total {
                return ResumePlan::Restart("本地源文件已变化");
            }
        }
    }
    ResumePlan::Resume { from: m.bytes }
}

/// 记录集合的清理：丢过期、按 `updated` 倒序、截到 [`MAX_MARKS`] 条。
///
/// 加载与保存都走它 —— 一处规则两处生效，不会出现「加载时清了、保存时又写回去」。
pub fn prune(mut marks: Vec<ResumeMark>, now: u64) -> Vec<ResumeMark> {
    marks.retain(|m| now.saturating_sub(m.updated) <= MAX_AGE_SECS);
    marks.sort_by_key(|m| std::cmp::Reverse(m.updated));
    marks.truncate(MAX_MARKS);
    marks
}

/// 记录文件的磁盘格式。
#[derive(Debug, Default, Serialize, Deserialize)]
struct StoreFile {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    marks: Vec<ResumeMark>,
}

type Key = (TransferDir, String, String);

/// key 构造：只在「记/查/删」三处用到，收成一个函数免得哪处漏了方向。
fn key_of(dir: TransferDir, local: &str, remote: &str) -> Key {
    (dir, local.to_string(), remote.to_string())
}

/// 记录存储：内存 map 为准，落盘为副本。
pub struct ResumeStore {
    path: PathBuf,
    inner: parking_lot::Mutex<Inner>,
}

struct Inner {
    marks: HashMap<Key, ResumeMark>,
    /// 首次访问才读盘：模块级单例不该在构造时就碰文件系统
    loaded: bool,
    dirty: bool,
    last_flush: Option<Instant>,
}

impl ResumeStore {
    /// 打开（或延迟打开）一个记录文件。测试与 e2e 都走这里传自己的临时路径。
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            inner: parking_lot::Mutex::new(Inner {
                marks: HashMap::new(),
                loaded: false,
                dirty: false,
                last_flush: None,
            }),
        }
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// 记录/更新一条未完成传输。落盘按 [`FLUSH_EVERY`] 节流。
    pub fn record(&self, mark: ResumeMark) {
        let mut g = self.inner.lock();
        self.ensure_loaded(&mut g);
        g.marks
            .insert(key_of(mark.dir, &mark.local, &mark.remote), mark);
        g.dirty = true;
        if g.last_flush.is_none_or(|t| t.elapsed() >= FLUSH_EVERY) {
            self.write_locked(&mut g);
        }
    }

    /// 丢弃一条记录（传输成功、或已判定从头重传）。立即落盘：这是语义事件，不是进度。
    pub fn forget(&self, dir: TransferDir, local: &str, remote: &str) {
        let mut g = self.inner.lock();
        self.ensure_loaded(&mut g);
        if g.marks.remove(&key_of(dir, local, remote)).is_some() {
            g.dirty = true;
            self.write_locked(&mut g);
        }
    }

    /// 查一条记录（UI 在下载落点决策时用）。
    pub fn find(&self, dir: TransferDir, local: &str, remote: &str) -> Option<ResumeMark> {
        let mut g = self.inner.lock();
        self.ensure_loaded(&mut g);
        g.marks.get(&key_of(dir, local, remote)).cloned()
    }

    /// 立即落盘（传输结束/失败时调用，把节流窗口里的进度收口）。
    pub fn flush(&self) {
        let mut g = self.inner.lock();
        self.ensure_loaded(&mut g);
        if g.dirty {
            self.write_locked(&mut g);
        }
    }

    /// 当前记录快照（`updated` 倒序，含未落盘的最新值）。
    pub fn snapshot(&self) -> Vec<ResumeMark> {
        let mut g = self.inner.lock();
        self.ensure_loaded(&mut g);
        let now = now_secs();
        prune(g.marks.values().cloned().collect(), now)
    }

    fn ensure_loaded(&self, g: &mut Inner) {
        if g.loaded {
            return;
        }
        g.loaded = true;
        let raw = match std::fs::read_to_string(&self.path) {
            Ok(s) => s,
            Err(_) => return, // 首次运行：没有文件是常态
        };
        match serde_json::from_str::<StoreFile>(&raw) {
            Ok(f) => {
                let now = now_secs();
                for m in prune(f.marks, now) {
                    g.marks.insert(key_of(m.dir, &m.local, &m.remote), m);
                }
            }
            Err(e) => {
                // 坏文件不打断任何事：续传退化成「没有记录」，下载/上传照常。
                log::warn!(
                    "SFTP 续传记录解析失败（按空处理）: {} ({e})",
                    self.path.display()
                );
                crate::configio::backup_broken_config(&self.path, &raw);
            }
        }
    }

    /// 写盘（调用方持锁）。失败只记日志：记录写不进去不能拖累传输本身。
    fn write_locked(&self, g: &mut Inner) {
        let now = now_secs();
        let marks = prune(g.marks.values().cloned().collect(), now);
        // 内存里同步这份清理结果：否则被淘汰的条目会一直活着，下次又写回去
        g.marks = marks
            .iter()
            .map(|m| (key_of(m.dir, &m.local, &m.remote), m.clone()))
            .collect();
        let f = StoreFile { version: 1, marks };
        match write_atomic(&self.path, &f) {
            Ok(_) => {
                g.dirty = false;
                g.last_flush = Some(Instant::now());
            }
            Err(e) => log::warn!("SFTP 续传记录落盘失败: {} ({e})", self.path.display()),
        }
    }
}

fn write_atomic(path: &std::path::Path, f: &StoreFile) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_string_pretty(f).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 全过程只有这一份记录（与 `sftp_events` 同一手法：跨模块共享的运行时状态）。
pub fn resume_store() -> &'static ResumeStore {
    static STORE: OnceLock<ResumeStore> = OnceLock::new();
    STORE.get_or_init(|| ResumeStore::new(resume_path()))
}

/// 记录文件路径：`<config>/rustshell/sftp_resume.json`。
/// `RUSTSHELL_SFTP_RESUME` 可覆盖 —— 测试与 Docker e2e 靠它把记录落在临时目录，
/// 不碰开发机真实配置（同 `ssh.rs::known_hosts_path` 的手法）。
pub fn resume_path() -> PathBuf {
    resolve_path(std::env::var("RUSTSHELL_SFTP_RESUME").ok())
}

fn resolve_path(env: Option<String>) -> PathBuf {
    if let Some(p) = env.filter(|p| !p.is_empty()) {
        return PathBuf::from(p);
    }
    crate::config_dir::config_dir().join("sftp_resume.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(dir: TransferDir, bytes: u64, total: u64) -> ResumeMark {
        ResumeMark {
            dir,
            local: "C:\\dl\\big.iso".into(),
            remote: "/srv/big.iso".into(),
            bytes,
            total,
            // 必须是「现在」：过期判定是 7 天，写死的历史时间戳会被 prune 直接丢掉
            updated: now_secs(),
        }
    }

    fn dl(bytes: u64, total: u64) -> ResumeMark {
        mk(TransferDir::Download, bytes, total)
    }

    fn ul(bytes: u64, total: u64) -> ResumeMark {
        mk(TransferDir::Upload, bytes, total)
    }

    fn tmp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rustshell-sftp-resume-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 无记录 → 从头（且给出原因）。
    #[test]
    fn no_mark_restarts() {
        assert_eq!(plan_resume(None, 0, 0), ResumePlan::Restart("无未完成记录"));
        assert_eq!(
            plan_resume(Some(&dl(0, 100)), 0, 100),
            ResumePlan::Restart("没有已传数据")
        );
    }

    /// 下载：目标侧（本地）必须恰好等于偏移。
    #[test]
    fn download_requires_the_local_part_to_match_exactly() {
        let m = dl(40, 100);
        assert_eq!(
            plan_resume(Some(&m), 40, 100),
            ResumePlan::Resume { from: 40 }
        );
        assert_eq!(
            plan_resume(Some(&m), 39, 100),
            ResumePlan::Restart("本地文件已变化"),
            "本地半成品短了说明被别的写入碰过，不能接着往后写"
        );
        assert_eq!(
            plan_resume(Some(&m), 41, 100),
            ResumePlan::Restart("本地文件已变化")
        );
        assert_eq!(
            plan_resume(Some(&m), 0, 100),
            ResumePlan::Restart("本地文件已变化"),
            "本地半成品被删 → 从头"
        );
    }

    /// 下载：源侧（远端）不得短于偏移，且总量已知时不能变。
    #[test]
    fn download_checks_the_remote_source() {
        let m = dl(40, 100);
        assert_eq!(
            plan_resume(Some(&m), 40, 99),
            ResumePlan::Restart("远端文件已变化"),
            "远端变短：偏移已在文件末尾之外"
        );
        assert_eq!(
            plan_resume(Some(&m), 40, 120),
            ResumePlan::Restart("远端文件已变化"),
            "远端被换成长度不同的另一份"
        );
        // total 未知（0）时不比总量，只比「不短于偏移」
        assert_eq!(
            plan_resume(Some(&dl(40, 0)), 40, 120),
            ResumePlan::Resume { from: 40 }
        );
    }

    /// 上传：目标侧是**远端**，本地是源。
    #[test]
    fn upload_swaps_which_side_must_match() {
        let m = ul(40, 100);
        assert_eq!(
            plan_resume(Some(&m), 100, 40),
            ResumePlan::Resume { from: 40 }
        );
        assert_eq!(
            plan_resume(Some(&m), 100, 39),
            ResumePlan::Restart("远端文件已变化"),
            "远端半成品短了 → 从头（远端那份不可信了）"
        );
        assert_eq!(
            plan_resume(Some(&m), 100, 41),
            ResumePlan::Restart("远端文件已变化")
        );
        assert_eq!(
            plan_resume(Some(&m), 39, 40),
            ResumePlan::Restart("本地源文件已变化"),
            "本地源被改短 → 没有可续的源数据"
        );
        assert_eq!(
            plan_resume(Some(&m), 120, 40),
            ResumePlan::Restart("本地源文件已变化"),
            "本地源大小变了（total 已知时按它判）→ 从头"
        );
    }

    /// 清理：丢过期、按时间倒序、截到上限。
    #[test]
    fn prune_drops_expired_and_caps_the_count() {
        let now = now_secs();
        let mut marks: Vec<ResumeMark> = (0..(MAX_MARKS as u64 + 5))
            .map(|i| ResumeMark {
                updated: now - i,
                ..dl(i + 1, 1000)
            })
            .collect();
        marks.push(ResumeMark {
            updated: now - MAX_AGE_SECS - 1,
            ..dl(7, 1000)
        });
        let out = prune(marks, now);
        assert_eq!(out.len(), MAX_MARKS, "超出上限按 updated 淘汰最旧");
        assert!(out[0].updated >= out[out.len() - 1].updated, "最新的在前");
        assert!(
            out.iter().all(|m| now - m.updated <= MAX_AGE_SECS),
            "过期条目一条都不留"
        );
    }

    /// 记录往返：落盘后由另一个 store 读回，内容一致。
    #[test]
    fn marks_survive_a_round_trip() {
        let dir = tmp("round-trip");
        let path = dir.join("sftp_resume.json");
        let a = ResumeStore::new(path.clone());
        a.record(dl(1234, 9999));
        a.record(mk(TransferDir::Upload, 7, 8));
        a.flush();

        let b = ResumeStore::new(path);
        let back = b.snapshot();
        assert_eq!(back.len(), 2);
        let up = b
            .find(TransferDir::Upload, "C:\\dl\\big.iso", "/srv/big.iso")
            .expect("上传记录要能读回");
        assert_eq!(up.bytes, 7);
        let down = b
            .find(TransferDir::Download, "C:\\dl\\big.iso", "/srv/big.iso")
            .expect("下载记录要能读回");
        assert_eq!(down.bytes, 1234);
        assert_eq!(down.total, 9999);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 落盘是**节流**的：2s 内第二次记录不写盘，`flush` 才收口。
    #[test]
    fn writes_are_throttled_between_flushes() {
        let dir = tmp("throttle");
        let path = dir.join("sftp_resume.json");
        let s = ResumeStore::new(path.clone());
        s.record(dl(64, 4096)); // 第一条立即落盘（新传输不该只在内存里）
        let first = std::fs::read_to_string(&path).unwrap();
        s.record(dl(128, 4096));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            first,
            "2s 内的进度推进不该写盘"
        );
        s.flush();
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains("128"), "flush 必须把最新偏移收口: {after}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 传完 / 判定重传后记录必须消失（否则下次会拿着旧偏移去比长度）。
    #[test]
    fn forget_removes_and_persists() {
        let dir = tmp("forget");
        let path = dir.join("sftp_resume.json");
        let s = ResumeStore::new(path.clone());
        s.record(dl(64, 4096));
        assert!(s
            .find(TransferDir::Download, "C:\\dl\\big.iso", "/srv/big.iso")
            .is_some());
        s.forget(TransferDir::Download, "C:\\dl\\big.iso", "/srv/big.iso");
        assert!(s
            .find(TransferDir::Download, "C:\\dl\\big.iso", "/srv/big.iso")
            .is_none());
        let b = ResumeStore::new(path);
        assert!(b.snapshot().is_empty(), "删除也要落盘，不能只改内存");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 方向不同的同名路径是两条记录（下载半成品与上传半成品互不干扰）。
    #[test]
    fn direction_is_part_of_the_key() {
        let dir = tmp("direction");
        let path = dir.join("sftp_resume.json");
        let s = ResumeStore::new(path);
        s.record(dl(10, 100));
        s.record(ul(20, 100));
        assert_eq!(
            s.find(TransferDir::Download, "C:\\dl\\big.iso", "/srv/big.iso")
                .unwrap()
                .bytes,
            10
        );
        assert_eq!(
            s.find(TransferDir::Upload, "C:\\dl\\big.iso", "/srv/big.iso")
                .unwrap()
                .bytes,
            20
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 坏记录文件：当作空 + 留底，绝不 panic、也不影响调用方。
    #[test]
    fn broken_file_degrades_to_empty_with_a_backup() {
        let dir = tmp("broken");
        let path = dir.join("sftp_resume.json");
        std::fs::write(&path, "{ this is not json").unwrap();
        let s = ResumeStore::new(path.clone());
        assert!(s.snapshot().is_empty());
        let backups: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.contains("broken-"))
            .collect();
        assert_eq!(backups.len(), 1, "坏文件要留底: {backups:?}");
        // 留底之后照常可用
        s.record(dl(5, 50));
        s.flush();
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .contains("\"bytes\": 5"));
        let _ = std::fs::remove_dir_all(&dir);
    }

/// 路径解析：环境变量优先，其次配置目录。
    #[test]
    fn resume_path_prefers_the_env_override() {
        assert_eq!(
            resolve_path(Some("D:\\tmp\\r.json".into())),
            PathBuf::from("D:\\tmp\\r.json")
        );
        assert_eq!(
            resolve_path(None),
            crate::config_dir::config_dir().join("sftp_resume.json")
        );
        // 空环境变量视为未设置
        assert_eq!(
            resolve_path(Some(String::new())),
            crate::config_dir::config_dir().join("sftp_resume.json")
        );
    }
}
