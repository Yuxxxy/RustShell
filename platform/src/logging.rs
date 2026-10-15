//! 运行日志（M4 诊断）：进程级文件日志 + panic 落盘。

use log::{LevelFilter, Log, Metadata, Record};
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// 每日一个日志目录（logs/yyyy-MM-dd/）之外的保留天数。
const KEEP_DAYS: u32 = 30;
const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;
const MAX_BACKUPS: usize = 3;

/// 已知高频噪声模块（按 target 子串匹配）。
const NOISY_TARGETS: &[&str] = &["window::a11y"];

/// 纯逻辑：该条日志是否属于「已知噪声、应静音」。
fn is_noisy(target: &str, level: log::Level) -> bool {
    if level <= log::Level::Warn {
        return false;
    }
    if std::env::var_os("RUSTSHELL_LOG_A11Y").is_some() {
        return false;
    }
    NOISY_TARGETS.iter().any(|n| target.contains(n))
}

/// 文件日志：落在 `logs/<yyyy-MM-dd>/rustshell.log`，跨天自动换目录。
/// 长驻进程跨零点后第一条日志触发目录创建与文件切换，无需重启。
struct FileLogger {
    /// logs 根目录（当日子目录由写入时按需推导）
    dir: PathBuf,
    /// 最近一次写入落在的日号（自 epoch 起，UTC+8 日界）；0 = 尚未写过
    last_day: AtomicU64,
    written: AtomicUsize,
}

impl FileLogger {
    /// 当前应写入的日志文件路径（随系统时间跨天滚动）。
    fn current_path(&self) -> (PathBuf, u64) {
        let now = crate::paths::now_unix();
        let day = (now as i64 + 8 * 3600).div_euclid(86_400) as u64;
        let name = crate::paths::day_folder_name(now);
        (self.dir.join(name).join("rustshell.log"), day)
    }
}

impl Log for FileLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= log::max_level() && !is_noisy(metadata.target(), metadata.level())
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let line = format!(
            "{} {:<5} [{}] {}\n",
            chrono_line(),
            record.level().as_str(),
            record.target(),
            record.args()
        );
        let (path, day) = self.current_path();
        // 跨天（或首写）：建目录 + 切文件。AtomicU64 换出旧值即可判断是否首写/换日。
        if self.last_day.swap(day, Ordering::Relaxed) != day {
            if let Some(dir) = path.parent() {
                let _ = fs::create_dir_all(dir);
            }
        }
        // 滚动检查（按写入条数粗略节流：每 256 条查一次大小）
        let n = self.written.fetch_add(1, Ordering::Relaxed);
        if n.is_multiple_of(256) {
            Self::rotate_if_needed(&path);
        }
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
            let _ = f.write_all(line.as_bytes());
        }
        // 调试场景镜像到 stderr
        if record.level() <= log::Level::Warn {
            let _ = std::io::stderr().write_all(line.as_bytes());
        }
    }

    fn flush(&self) {
        let (path, _) = self.current_path();
        if let Ok(mut f) = OpenOptions::new().append(true).open(&path) {
            let _ = f.flush();
        }
    }
}

impl FileLogger {
    fn rotate_if_needed(path: &std::path::Path) {
        let Ok(meta) = fs::metadata(path) else {
            return;
        };
        if meta.len() < MAX_LOG_BYTES {
            return;
        }
        // rustshell.log.{MAX-1} 来自 .{MAX-2} ...，最旧删除；rustshell.log 到 .1
        for i in (1..MAX_BACKUPS).rev() {
            let from = path.with_extension(format!("log.{i}"));
            let to = path.with_extension(format!("log.{}", i + 1));
            if from.exists() {
                let _ = fs::rename(&from, &to);
            }
        }
        let first = path.with_extension("log.1");
        if first.exists() {
            let _ = fs::remove_file(&first);
        }
        let _ = fs::rename(path, &first);
    }
}

/// 时间戳（本地时间 UTC+8，`yyyy-MM-dd HH:mm:ss.SSS`；零依赖手写，仅用于日志可读性）
fn chrono_line() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let millis = now.subsec_millis();
    // 与日志目录日界同源：固定 UTC+8 偏移，不引入 chrono
    let secs = now.as_secs() + 8 * 3600;
    let days = (secs / 86_400) as i64;
    let tod = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}.{millis:03}",
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60
    )
}

/// 天数（自 Unix 纪元）→ (年, 月, 日)。Howard Hinnant 的 `civil_from_days`
/// 算法，纯整数运算覆盖闰年/世纪规则；仅服务 `chrono_line`。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// 初始化全局日志（进程内仅首次生效；重复调用为 no-op）。
pub fn init(level: LevelFilter) {
    // 30 天保留：启动时清理过期日期目录与旧版平铺日志（2026-09-17 反馈）
    let now = crate::paths::now_unix();
    let migrated = crate::paths::migrate_legacy_agent_audit();
    let pruned = crate::paths::prune_old_logs(now, KEEP_DAYS);

    let dir = crate::paths::logs_dir();
    let _ = fs::create_dir_all(&dir);
    let path = crate::paths::day_log_path(now);
    let logger = FileLogger {
        dir,
        last_day: AtomicU64::new(0),
        written: AtomicUsize::new(0),
    };
    let _ = log::set_boxed_logger(Box::new(logger));
    log::set_max_level(level);

    // panic hook：先落盘再走默认行为。路径在 panic 时按当天日期现取（跨天也不写错目录）。
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let msg = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown panic payload".into());
        let loc = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "unknown location".into());
        // 崩溃现场调用栈：容量溢出这类 panic 的「消息+行号」定位不了根因，
        // 必须带栈才能落到具体分配点（raw_vec 只暴露库内位置）。
        let bt = std::backtrace::Backtrace::force_capture().to_string();
        let entry = format!(
            "{} PANIC [panic] {} at {}\n--- backtrace ---\n{}\n--- backtrace end ---\n",
            chrono_line(),
            msg,
            loc,
            bt
        );
        if let Some(dir) = crate::paths::log_path().parent() {
            let _ = fs::create_dir_all(dir);
        }
        let _ = OpenOptions::new()
            .create(true)
            .append(true)
            .open(crate::paths::log_path())
            .and_then(|mut f| f.write_all(entry.as_bytes()));
        default_hook(info);
    }));

    let ver = env!("CARGO_PKG_VERSION");
    let os = std::env::consts::OS;
    let cwd = std::env::current_dir()
        .map(|d| d.display().to_string())
        .unwrap_or_else(|_| "?".into());
    log::info!(
        "RustShell v{ver} 启动：os={os} cwd={cwd} log={}",
        path.display()
    );
    if migrated {
        log::info!("已将旧版 config 根下的 agent-audit.log 迁入 logs 目录");
    }
    if !pruned.is_empty() {
        // 清理清单最多展示 8 条，避免启动日志被刷屏
        let shown: Vec<&String> = pruned.iter().take(8).collect();
        log::info!(
            "已清理 {} 项过期日志（保留 {KEEP_DAYS} 天）：{}{}",
            pruned.len(),
            shown
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            if pruned.len() > 8 { " …" } else { "" }
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chrono_line_format() {
        let s = chrono_line();
        // yyyy-MM-dd HH:mm:ss.SSS，共 23 字符
        assert_eq!(s.len(), 23, "got: {s}");
        let b = s.as_bytes();
        assert_eq!(b[4], b'-');
        assert_eq!(b[7], b'-');
        assert_eq!(b[10], b' ');
        assert_eq!(b[13], b':');
        assert_eq!(b[16], b':');
        assert_eq!(b[19], b'.');
    }

    /// 已知日期回归：2026-09-18 00:00:00 UTC 的 Unix 秒对应本地（UTC+8）2026-09-18 08:00。
    #[test]
    fn civil_from_days_known_dates() {
        let secs: u64 = 1789728000; // 2026-09-18T00:00:00Z
        let (y, m, d) = civil_from_days((secs / 86_400) as i64);
        assert_eq!((y, m, d), (2026, 9, 18));
        // 1970-01-01
        let (y, m, d) = civil_from_days(0);
        assert_eq!((y, m, d), (1970, 1, 1));
        // 闰年 2024-02-29：1709164800 / 86400 = 19782 天
        let (y, m, d) = civil_from_days(19_782);
        assert_eq!((y, m, d), (2024, 2, 29));
    }

    /// 回归护栏：仅静音 a11y 的 INFO，永不静音 WARN/ERROR。
    #[test]
    fn a11y_info_is_muted_but_warn_is_not() {
        use log::Level::{Error, Info, Warn};
        // 噪声目标：INFO 压掉
        assert!(is_noisy("gpui::window::a11y", Info));
        assert!(is_noisy("gpui_pre::window::a11y", Info));
        // 同样的目标：WARN / ERROR 一律放行
        assert!(!is_noisy("gpui::window::a11y", Warn));
        assert!(!is_noisy("gpui::window::a11y", Error));
        // 非噪声目标不受影响
        assert!(!is_noisy("rustshell_ui::main_view", Info));
        assert!(!is_noisy("rustshell_ui::watchdog", Warn));
    }
}
