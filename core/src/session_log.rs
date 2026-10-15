//! 会话日志（F-7）：把终端输出**旁路**落盘。
//!
//! # 为什么接口只有「不返回 Result 的 write」
//!
//! 记录是**附加**能力：关掉它、写失败、磁盘满、路径不可写，都不能让终端少一个字节。
//! 所以这里把「日志出错」这件事**在 logger 内部消化**：
//! [`SessionLogger::write`] 返回 `()`，调用点没有错误可传播，
//! 也就不存在「顺手把日志错误升级成会话错误」的写法。写失败时 logger 自己降级
//! （置 `stopped` + 关文件 + `log::warn!`），之后所有 `write` 都是空操作。
//!
//! # 落点
//!
//! tee 点只有**一处**：`session.rs::build_session_pipes` 的输出泵线程。
//! SSH 与本地 Shell 的输出都要经过那里（SSH 的 `ChannelMsg` 先进 `out_tx`，
//! 由同一个线程转给终端），所以「两路各接一次」既不必要、也更容易漏接线。
//!
//! 路径的来源见 `platform::paths::session_log_path_now`：core 不依赖 platform
//! （方向是 platform → core），所以这里只收一个**已经算好的完整文件路径**。

use parking_lot::Mutex;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// 单个日志文件的大小上限（字节）；写满即轮转（见 [`SessionLogger::rotate`]）。
///
/// 8MB ≈ 十几万行终端输出，够一次会话的现场；再大就只是让「打开日志」变慢。
pub const MAX_LOG_BYTES: u64 = 8 * 1024 * 1024;

/// 会话是否记录日志：**连接级覆盖优先**，缺省跟全局设置。
///
/// 收口成一个纯函数：建会话的每条入口（SSH 连接 / 本地 Shell / 掉线重连）都得给出
/// 同一个答案。散在各处的话，早晚有一处忘了看连接级覆盖 ——
/// 表现是「这台机器开了没生效、那台机器关不掉」。
pub fn enabled_for(profile: Option<&crate::connections::ConnectionProfile>, global: bool) -> bool {
    match profile.and_then(|p| p.session_log) {
        Some(v) => v,
        None => global,
    }
}

/// 轮转后的备份路径：`X.log` → `X.log.1`（只留一份）。
///
/// 用追加后缀而不是改主名：一眼能看出它是 `X.log` 的前一段，
/// 且主名保持不变，用户记住的路径不用跟着变。
pub fn rotation_target(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(".1");
    PathBuf::from(s)
}

/// 在 `base` 旁边找一个没被占用的名字：`X.log`、`X-2.log`、`X-3.log`…
///
/// 撞名的现实场景：同一秒内开两个同名会话（或同一份配置连开两次）。
/// 撞上时**另起一个文件**而不是追加进已有文件 —— 把两次会话的输出缝进同一个文件，
/// 事后谁也说不清哪一段属于哪一次。
pub fn free_path(base: &Path) -> PathBuf {
    if !base.exists() {
        return base.to_path_buf();
    }
    let stem = base
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let ext = base.extension().map(|s| s.to_string_lossy().to_string());
    for n in 2..1000u32 {
        let name = match &ext {
            Some(e) => format!("{stem}-{n}.{e}"),
            None => format!("{stem}-{n}"),
        };
        let cand = base.with_file_name(name);
        if !cand.exists() {
            return cand;
        }
    }
    // 上千个同名文件：仍然返回基准名（追加进去），总比不记录好
    base.to_path_buf()
}

/// 一个会话的日志文件。
///
/// 直接写 `File`（不套 `BufWriter`）：①终端输出本来就是一整块一整块来的
/// （PTY 读 8KB、SSH 消息成批），缓冲收益有限；②没有缓冲就**没有「错误发生在 flush
/// 时」这一说** —— 每次 `write_all` 的结果就是最终结论，失败点唯一，
/// 降级判定不需要额外状态机。
pub struct SessionLogger {
    path: PathBuf,
    file: Option<std::fs::File>,
    written: u64,
    limit: u64,
    /// 降级原因。`Some` = 这个日志**已经不再写**（文件已关，后续 `write` 全是空操作）。
    stopped: Option<String>,
    /// 已轮转次数（现场读数用）。
    rotations: u32,
}

impl SessionLogger {
    /// 在 `base` 建日志（必要时换名避让），父目录不存在则创建。
    /// 失败返回**可直接展示的原因**，由调用方决定降级。
    pub fn open(base: &Path) -> Result<Self, String> {
        Self::open_with_limit(base, MAX_LOG_BYTES)
    }

    /// 同 [`Self::open`]，但可指定轮转阈值（单测用一个小阈值就能验轮转）。
    pub fn open_with_limit(base: &Path, limit: u64) -> Result<Self, String> {
        if let Some(parent) = base.parent() {
            if !parent.is_dir() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("创建日志目录失败（{}）：{e}", parent.display()))?;
            }
        }
        let path = free_path(base);
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| format!("创建日志文件失败（{}）：{e}", path.display()))?;
        // 以 append 打开 ⇒ 已有内容的长度就是「已写字节」，
        // 于是轮转阈值不会因为重开文件而重新计数（虽然正常路径下 `free_path`
        // 只会给出空文件，这条仍然是对的）。
        let written = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(Self {
            path,
            file: Some(file),
            written,
            limit,
            stopped: None,
            rotations: 0,
        })
    }

    /// 实际落盘路径（可能因撞名而与传入路径不同）。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 还在写吗。
    pub fn is_active(&self) -> bool {
        self.file.is_some() && self.stopped.is_none()
    }

    /// 降级原因（已停止记录时）。
    pub fn stopped_reason(&self) -> Option<&str> {
        self.stopped.as_deref()
    }

    /// 已轮转次数。
    pub fn rotations(&self) -> u32 {
        self.rotations
    }

    /// 追加一段**原始字节**（含转义序列，不做任何渲染/解码）。
    ///
    /// 返回 `()` 是刻意的：见模块头。写失败 → 置 `stopped`、关文件、`log::warn!`，
    /// 从此静默（不再每次失败都刷日志）。
    pub fn write(&mut self, data: &[u8]) {
        if data.is_empty() || self.stopped.is_some() {
            return;
        }
        if self.written >= self.limit {
            self.rotate();
            // 轮转失败会自行降级；失败后 file 已为 None，下面自然成为空操作
        }
        let Some(file) = self.file.as_mut() else {
            return;
        };
        if let Err(e) = file.write_all(data) {
            log::warn!(
                "会话日志写入失败，本会话已停止记录: path={} err={e}",
                self.path.display()
            );
            self.stopped = Some(e.to_string());
            self.file = None;
            return;
        }
        self.written += data.len() as u64;
    }

    /// 写满一轮就换文件：`X.log` → `X.log.1`，主名重新开始。
    ///
    /// Windows **不允许重命名打开中的文件** ⇒ 必须「先关、再改名、再开」，
    /// 顺序不能省。改名本身失败不算致命（只是没有备份），照旧继续往原文件追加；
    /// 只有「重新打开也失败」才降级 —— 那才是真的写不进去了。
    fn rotate(&mut self) {
        if let Some(mut f) = self.file.take() {
            let _ = f.flush();
        }
        let target = rotation_target(&self.path);
        let _ = std::fs::remove_file(&target); // 只保留一份备份（先删旧备份）
        let renamed = std::fs::rename(&self.path, &target).is_ok();
        self.written = 0;
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            Ok(f) => {
                self.file = Some(f);
                self.rotations += 1;
                if !renamed {
                    // 继续写同一个文件：本轮轮转白做，但数据没丢
                    log::warn!(
                        "会话日志轮转失败（改名未成功），继续追加原文件: {}",
                        self.path.display()
                    );
                }
            }
            Err(e) => {
                log::warn!(
                    "会话日志轮转后无法重开，本会话已停止记录: path={} err={e}",
                    self.path.display()
                );
                self.stopped = Some(e.to_string());
            }
        }
    }

    /// **单测专用**：模拟一次写入失败。
    ///
    /// 真实的写失败在本机很难稳定复现（Windows 上删不掉打开中的文件、权限改动也
    /// 撤不回已打开的句柄）。这里直接把「写入失败时 logger 做了什么」那一段的**结果**
    /// 摆出来（置 `stopped` + 关文件），用来钉住「降级之后 `write` 必须是空操作、
    /// 不得 panic、不得把文件重新打开」。
    #[cfg(test)]
    pub(crate) fn simulate_write_failure(&mut self) {
        self.stopped = Some("模拟写入失败".to_string());
        self.file = None;
    }
}

impl Drop for SessionLogger {
    fn drop(&mut self) {
        if let Some(mut f) = self.file.take() {
            let _ = f.flush();
        }
    }
}

/// 按目标路径建 logger；`None` 表示本会话不记录。
///
/// **任何失败都不阻断会话**（F-7 的硬要求）：日志是附加能力，建不出来就
/// `log::warn!` 一句然后返回 `None`，终端照常工作。用户可以在「打开日志目录」里
/// 看到「开关开了但没文件」，日志里也有原因可查。
pub fn open_if(target: Option<&Path>) -> Option<Arc<Mutex<SessionLogger>>> {
    let target = target?;
    match SessionLogger::open(target) {
        Ok(l) => {
            log::info!("会话日志已开启: {}", l.path().display());
            Some(Arc::new(Mutex::new(l)))
        }
        Err(e) => {
            log::warn!("会话日志未能开启（本会话不记录）: {e}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rustshell-session-log-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 连接级覆盖优先于全局；没有覆盖时跟全局走。
    #[test]
    fn enabled_for_prefers_the_connection_override() {
        let mut p = crate::connections::ConnectionProfile::new("t", "h", "u");
        assert!(enabled_for(Some(&p), true), "无覆盖 → 跟全局开");
        assert!(!enabled_for(Some(&p), false), "无覆盖 → 跟全局关");
        p.session_log = Some(false);
        assert!(
            !enabled_for(Some(&p), true),
            "连接级关必须压过全局开（否则「这台机器别记」表达不出来）"
        );
        p.session_log = Some(true);
        assert!(enabled_for(Some(&p), false), "连接级开必须压过全局关");
        // 本地 Shell 没有 profile：只有全局
        assert!(enabled_for(None, true));
        assert!(!enabled_for(None, false));
    }

    /// 轮转备份路径就是原路径加 `.1`。
    #[test]
    fn rotation_target_appends_a_dot_one() {
        let p = PathBuf::from("/tmp/a/123456-x.log");
        assert_eq!(rotation_target(&p), PathBuf::from("/tmp/a/123456-x.log.1"));
    }

    /// 撞名时另起 `-2`、`-3`…；未被占用时原样返回。
    #[test]
    fn free_path_avoids_clobbering_an_existing_log() {
        let dir = tmp("free");
        let base = dir.join("120000-web.log");
        assert_eq!(free_path(&base), base, "没占用就用原名");

        std::fs::write(&base, "first").unwrap();
        let second = free_path(&base);
        assert_eq!(second, dir.join("120000-web-2.log"));

        std::fs::write(&second, "second").unwrap();
        assert_eq!(free_path(&base), dir.join("120000-web-3.log"));

        // 已有的文件绝不能被截断或追加（各自独立）
        assert_eq!(std::fs::read_to_string(&base).unwrap(), "first");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 写进去的是**原始字节**：转义序列、非 UTF-8 都原样落盘，
    /// 读回来必须与写进去的逐字节相同。
    #[test]
    fn write_persists_raw_bytes_verbatim() {
        let dir = tmp("raw");
        let base = dir.join("120000-raw.log");
        let mut lg = SessionLogger::open(&base).unwrap();
        assert!(lg.is_active());
        assert_eq!(lg.path(), base);

        // 彩色输出 + 无效 UTF-8 + CR/LF，全都不能被「顺手清洗」
        let payload: Vec<u8> = b"\x1b[31mred\x1b[0m \xff\xfe\r\n$ ".to_vec();
        lg.write(&payload);
        lg.write(b"second");
        lg.write(b""); // 空切片是空操作
        drop(lg);

        assert_eq!(std::fs::read(&base).unwrap(), {
            let mut v = payload.clone();
            v.extend_from_slice(b"second");
            v
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 追加语义：父目录会被创建；打开是**追加**模式，绝不截断既有内容。
    #[test]
    fn open_creates_the_parent_dir_and_never_truncates() {
        let dir = tmp("append");
        let base = dir.join("nested").join("120000-a.log");
        {
            let mut lg = SessionLogger::open(&base).unwrap();
            lg.write(b"AAA");
        }
        assert!(base.is_file(), "父目录应当被创建出来");
        // 底层文件必须是 append：手动再追加一段，前一段仍在
        {
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&base)
                .unwrap();
            f.write_all(b"BBB").unwrap();
        }
        assert_eq!(std::fs::read_to_string(&base).unwrap(), "AAABBB");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 占用即避让：目标名被一个**目录**占着时，不覆盖、不失败，另起 `-2`。
    /// （`free_path` 只看「这个名字被占了吗」，不问占用者是文件还是目录 ——
    /// 对日志来说两者的结论一样：换个名字继续写。）
    #[test]
    fn a_directory_in_the_way_is_avoided_not_overwritten() {
        let dir = tmp("dirclash");
        let base = dir.join("120000-x.log");
        std::fs::create_dir_all(&base).unwrap();

        let mut lg = SessionLogger::open(&base).unwrap();
        assert_eq!(lg.path(), dir.join("120000-x-2.log"));
        lg.write(b"ok");
        drop(lg);

        assert!(base.is_dir(), "原目录必须原样保留");
        assert_eq!(
            std::fs::read_to_string(dir.join("120000-x-2.log")).unwrap(),
            "ok"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 写满阈值即轮转：主名被清空重写、备份里有前一段，`rotations` 计数。
    #[test]
    fn reaching_the_limit_rotates_the_file() {
        let dir = tmp("rotate");
        let base = dir.join("120000-rot.log");
        let mut lg = SessionLogger::open_with_limit(&base, 16).unwrap();

        lg.write(b"0123456789ABCDEF"); // 正好写满 16
        lg.write(b"next-chunk"); // 触发轮转，这一段进新文件
        assert_eq!(lg.rotations(), 1, "写满后必须轮转过一次");

        drop(lg);
        assert_eq!(std::fs::read_to_string(&base).unwrap(), "next-chunk");
        assert_eq!(
            std::fs::read_to_string(rotation_target(&base)).unwrap(),
            "0123456789ABCDEF",
            "前一段必须完整躺在备份里"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 轮转两次：备份只留**最近**一次，不会无限堆积。
    #[test]
    fn rotating_twice_keeps_only_one_backup() {
        let dir = tmp("rotate2");
        let base = dir.join("120000-r2.log");
        let mut lg = SessionLogger::open_with_limit(&base, 4).unwrap();
        lg.write(b"AAAA"); // written=4
        lg.write(b"BBBB"); // 满 → 轮转（备份 AAAA），本轮写 BBBB
        lg.write(b"CCCC"); // 满 → 轮转（备份 BBBB 覆盖 AAAA），本轮写 CCCC
        lg.write(b"DDDD"); // 满 → 轮转（备份 CCCC），本轮写 DDDD
        assert_eq!(lg.rotations(), 3);
        drop(lg);
        assert_eq!(
            std::fs::read_to_string(rotation_target(&base)).unwrap(),
            "CCCC"
        );
        assert_eq!(std::fs::read_to_string(&base).unwrap(), "DDDD");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 降级之后 `write` 必须是空操作：不 panic、不重开文件、不产生新内容。
    /// 这就是「日志写不进去不能影响终端」在引擎侧的落点 ——
    /// 调用点拿到的是 `()`，连传播错误的机会都没有。
    #[test]
    fn a_stopped_logger_swallows_every_later_write() {
        let dir = tmp("stopped");
        let base = dir.join("120000-st.log");
        let mut lg = SessionLogger::open(&base).unwrap();
        lg.write(b"before");
        lg.simulate_write_failure();
        assert!(!lg.is_active());
        assert!(
            lg.stopped_reason().is_some(),
            "降级必须留原因，便于现场排查"
        );
        lg.write(b"after"); // 不得 panic、不得恢复写入
        assert_eq!(lg.stopped_reason(), Some("模拟写入失败"));
        drop(lg);
        assert_eq!(std::fs::read_to_string(&base).unwrap(), "before");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 目标为 `None` = 不记录（关闭开关时**一个文件都不该产生**）。
    #[test]
    fn open_if_skips_everything_when_the_target_is_none() {
        assert!(open_if(None).is_none());
    }

    /// 建不出来时返回 `None`（而不是 Err 让调用方二选一）：日志失败**不得**
    /// 给会话制造第二条错误路径。用「父路径是一个文件」制造必然失败
    /// （`create_dir_all` 在文件下面建目录必然报错）。
    #[test]
    fn open_if_degrades_to_none_when_the_path_is_unusable() {
        let dir = tmp("unusable");
        let blocker = dir.join("blocker");
        std::fs::write(&blocker, "x").unwrap();
        assert!(
            open_if(Some(&blocker.join("sub").join("a.log"))).is_none(),
            "父路径是文件时不可能建出日志，必须降级为 None"
        );
        assert!(open_if(None).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `open_if` 成功时给出可用的 logger，且路径就是给定路径。
    #[test]
    fn open_if_returns_a_writable_logger() {
        let dir = tmp("ok");
        let base = dir.join("120000-ok.log");
        let lg = open_if(Some(&base)).expect("应当能建出来");
        lg.lock().write(b"hi");
        assert_eq!(std::fs::read_to_string(&base).unwrap(), "hi");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 生产源码（切到第一个 `#[cfg(test)]` 之前）。
    fn prod_session_rs() -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/session.rs");
        let src = std::fs::read_to_string(&path).expect("读得到 core/src/session.rs");
        // 与 `ui::test_support::prod` 同一口径：测试模块之后的内容不算生产代码，
        // 否则断言里的字面量会「自己匹配自己」，护栏永远绿。
        match src.find("#[cfg(test)]") {
            Some(ix) => src[..ix].to_string(),
            None => src,
        }
    }

    /// ★ F-7 接线护栏：输出泵是**唯一**的 tee 点，且顺序必须是「先落盘、再送终端」。
    ///
    /// 两条都是极容易被「顺手改掉」的约定，而改错的后果都不会在编译期暴露：
    /// - **唯一**：SSH 与本地 Shell 的输出都经过 `session.rs` 的输出泵线程
    ///   （SSH 的消息先进 `out_tx`，再由该线程转给终端）。哪天有人觉得
    ///   「SSH 那条更直接，在 `ssh.rs` 里也写一次吧」，结果就是同一段输出记两遍。
    /// - **顺序**：终端侧销毁时泵会 `break`。把落盘放在 `send` 之后，
    ///   最后一段输出（往往正是「为什么断了」的现场）就永远进不了日志。
    #[test]
    fn the_output_pump_is_the_only_tee_point_and_logs_before_forwarding() {
        let prod = prod_session_rs();
        let tee = "lg.lock().write(&v);";
        // 转发点用调用名做锚，**不**写死整条表达式：`send(v)` 改成 `send(v.clone())`
        // 之类的合法重构不该把这条护栏打红（反向验证时实测踩到过这一点）。
        let forward = "output_tx.send(";

        assert_eq!(
            prod.matches(tee).count(),
            1,
            "会话日志的写入点必须**只有一处**（输出泵）。多出来的那一处会让同一段输出落盘两遍；\
             一处都没有则是「开关开了但没日志」"
        );
        let tee_at = prod.find(tee).expect("找不到 tee 点");
        let fwd_at = prod
            .find(forward)
            .expect("输出泵里找不到 `output_tx.send(` —— 终端转发本身出了问题");
        assert!(
            tee_at < fwd_at,
            "必须先落盘再送终端：终端销毁时泵会立刻 break，反过来的话最后一段输出（往往正是「为什么断了」的现场）会丢"
        );
    }
}
