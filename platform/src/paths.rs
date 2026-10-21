//! 配置路径：`<config>/rustshell/`（Windows: %APPDATA%，Unix: ~/.config）

use std::path::PathBuf;

/// 配置根目录。
///
/// `RUSTSHELL_CONFIG_DIR` 可覆盖 —— 测试靠它把落盘产物（键位覆盖、设置…）写到
/// 临时目录，**不碰开发机真实配置**。手法与 `core::sftp_resume::resume_path`、
/// `core::ssh::known_hosts_path` 的同名环境变量一致。
///
/// ⚠️ 这是**进程级**环境变量：`cargo test` 会并行跑同文件内的多个用例，
/// 凡是「设了它之后还要跑别的用例」的测试都必须自己加静态锁串行
/// （见 `ui/src/main_view/ai_persist.rs` 的用例），否则互相偷对方的目录。
pub fn config_dir() -> PathBuf {
    if let Some(p) = std::env::var("RUSTSHELL_CONFIG_DIR")
        .ok()
        .filter(|p| !p.is_empty())
    {
        return PathBuf::from(p);
    }
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("rustshell")
}

pub fn settings_path() -> PathBuf {
    config_dir().join("settings.json")
}

pub fn connections_path() -> PathBuf {
    config_dir().join("connections.json")
}

pub fn quick_commands_path() -> PathBuf {
    config_dir().join("quick_commands.json")
}

pub fn ai_config_path() -> PathBuf {
    config_dir().join("ai_config.json")
}

/// AI 会话记录（多条会话 + 各自的消息）：与配置分开存 ——
/// 它是**数据**不是设置，坏掉时只该丢它自己，不该连端点/模型一起回退成默认。
pub fn ai_chats_path() -> PathBuf {
    config_dir().join("ai_chats.json")
}

/// 命令历史（按连接指纹分桶，见 `ui/src/main_view/completion_persist.rs`）：
/// 与配置分开存，理由同 `ai_chats_path` —— 它是**数据**。
pub fn cmd_history_path() -> PathBuf {
    config_dir().join("cmd_history.json")
}

/// 会话布局快照（F-9 会话恢复）：上次退出时的标签池顺序 / 分屏 / 激活位。
///
/// 与设置分开存：它是「这一刻开着什么」的**状态**，不是用户的偏好；
/// 坏了只该丢这一次的恢复能力，不该连设置一起回退成默认。
pub fn session_layout_path() -> PathBuf {
    config_dir().join("session_layout.json")
}

pub fn keymap_path() -> PathBuf {
    config_dir().join("keymap.json")
}

/// AI 审计日志：与运行日志同住 logs 目录（2026-09-17 反馈，此前散落在 config 根）。
pub fn agent_audit_path() -> PathBuf {
    logs_dir().join("agent-audit.log")
}

/// 定时任务定义。
pub fn tasks_path() -> PathBuf {
    config_dir().join("tasks.json")
}

/// 定时任务的运行记录目录（每任务一个 jsonl）。
pub fn task_runs_dir() -> PathBuf {
    config_dir().join("task-runs")
}

/// 告警规则与去重状态。
pub fn alert_rules_path() -> PathBuf {
    config_dir().join("alert_rules.json")
}

/// SMTP（邮件通知）配置。
pub fn smtp_path() -> PathBuf {
    config_dir().join("smtp.json")
}

/// 告警日志：与运行日志同住 logs 目录。
pub fn alerts_log_path() -> PathBuf {
    logs_dir().join("alerts.log")
}

/// 默认下载目录（定时下载任务与 agent 下载工具的落点）。
/// 优先系统下载目录，无则回退到用户主目录 —— 不落进配置目录。
pub fn default_download_dir() -> PathBuf {
    dirs::download_dir().or_else(dirs::home_dir).unwrap_or_else(config_dir)
}

/// 运行日志目录：`<config>/rustshell/logs/`
pub fn logs_dir() -> PathBuf {
    config_dir().join("logs")
}

/// 连接导出目录：导出对话框的初始位置，同时作为导入对话框的候选来源。
/// 用用户主目录，不落进配置目录。
pub fn connections_export_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(config_dir)
}

/// 确保目录存在；成功（含已存在）返回 true，失败返回 false（调用方据此提示，不 panic）。
pub fn ensure_dir(dir: &std::path::Path) -> bool {
    dir.is_dir() || std::fs::create_dir_all(dir).is_ok()
}

/// 列出目录内容：子目录在前、文件在后，各自按名称（不区分大小写）排序。
///
/// 返回 `(名称, 绝对路径, 是否目录)`；失败时返回空列表 + 可直接展示的原因，
/// 由调用方原地提示 —— 不 panic，也不静默给一个空目录冒充「目录里没东西」。
///
/// 以 `.` 开头的隐藏项**照列**：用户完全可能就是要选 `.ssh` 这类目录。
pub fn list_dir_entries(dir: &std::path::Path) -> (Vec<(String, PathBuf, bool)>, Option<String>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return (Vec::new(), Some(format!("无法读取目录：{}", dir.display())));
    };
    let mut dirs: Vec<(String, PathBuf)> = Vec::new();
    let mut files: Vec<(String, PathBuf)> = Vec::new();
    for entry in rd.flatten() {
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        let name = entry.file_name().to_string_lossy().to_string();
        if ft.is_dir() {
            dirs.push((name, entry.path()));
        } else if ft.is_file() {
            files.push((name, entry.path()));
        }
    }
    let by_name = |(n, _): &(String, PathBuf)| n.to_lowercase();
    dirs.sort_by_key(by_name);
    files.sort_by_key(by_name);

    let mut out: Vec<(String, PathBuf, bool)> = Vec::with_capacity(dirs.len() + files.len());
    out.extend(dirs.into_iter().map(|(n, p)| (n, p, true)));
    out.extend(files.into_iter().map(|(n, p)| (n, p, false)));
    (out, None)
}

/// 可切换的文件系统根。Windows 列出实际存在的盘符（选目录时必须能换盘，
/// 否则用户被锁死在系统盘里）；其它平台只有 `/`。
pub fn system_roots() -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        // 从 C 起：A:/B: 多为不存在的软驱，探测会卡住几秒。
        (b'C'..=b'Z')
            .map(|c| PathBuf::from(format!("{}:\\", c as char)))
            .filter(|p| p.is_dir())
            .collect()
    }
    #[cfg(not(windows))]
    {
        vec![PathBuf::from("/")]
    }
}

/// 当前 UTC+8 的 Unix 秒（与 logging.rs 的时间戳约定一致：日界按东八区）。
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Unix 秒 → 自 1970-01-01（UTC+8 日界）起的天数。
fn day_number(unix_secs: u64) -> i64 {
    (unix_secs as i64 + 8 * 3600).div_euclid(86_400)
}

/// 天数 → (年, 月, 日)。Howard Hinnant `civil_from_days` 算法，零依赖手写。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// (年, 月, 日) → 天数。Howard Hinnant `days_from_civil` 算法。
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400; // [0, 399]
    let mp = (if m > 2 { m - 3 } else { m + 9 }) as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// 当日日志子目录名（yyyy-MM-dd，UTC+8 日界）。
pub fn day_folder_name(unix_secs: u64) -> String {
    let (y, m, d) = civil_from_days(day_number(unix_secs));
    format!("{y:04}-{m:02}-{d:02}")
}

/// 解析 yyyy-MM-dd 目录名 → 天数；格式非法（含平铺文件名等）返回 None。
pub fn parse_day_folder(name: &str) -> Option<i64> {
    let b = name.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    if !(b[..4].iter().all(|c| c.is_ascii_digit())
        && b[5..7].iter().all(|c| c.is_ascii_digit())
        && b[8..10].iter().all(|c| c.is_ascii_digit()))
    {
        return None;
    }
    let y: i64 = name[..4].parse().ok()?;
    let m: u32 = name[5..7].parse().ok()?;
    let d: u32 = name[8..10].parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    Some(days_from_civil(y, m, d))
}

/// 当日运行日志文件：`logs/<yyyy-MM-dd>/rustshell.log`。
pub fn day_log_path(unix_secs: u64) -> PathBuf {
    logs_dir()
        .join(day_folder_name(unix_secs))
        .join("rustshell.log")
}

/// 运行日志文件路径（按当天日期落在 logs 的日期子目录里；跨天启动自动换目录）。
pub fn log_path() -> PathBuf {
    day_log_path(now_unix())
}

/// 清理旧日志：删除 `dir` 下早于保留期的 yyyy-MM-dd 子目录，以及每日目录制
/// 之前遗留在 logs 根下的平铺 `rustshell.log(.N)`。返回被删除的条目名。
/// 逐项失败静默跳过，绝不 panic；agent-audit.log 等其它文件不受影响。
pub fn prune_old_logs_in(dir: &std::path::Path, now_secs: u64, keep_days: u32) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let cutoff = day_number(now_secs) - keep_days as i64;
    let mut removed = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let path = entry.path();
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir {
            // 日期子目录：超出保留期整目录删除
            if let Some(day) = parse_day_folder(&name) {
                if day < cutoff && std::fs::remove_dir_all(&path).is_ok() {
                    removed.push(name);
                }
            }
        } else {
            // 平铺旧日志（每日目录制之前的布局）：rustshell.log 与 rustshell.log.N
            let is_flat_log = name == "rustshell.log"
                || name
                    .strip_prefix("rustshell.log.")
                    .map(|s| !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit()))
                    .unwrap_or(false);
            if is_flat_log && std::fs::remove_file(&path).is_ok() {
                removed.push(name);
            }
        }
    }
    removed
}

/// 清理真实 logs 目录（启动时调用一次）。返回被删除的条目名，供启动日志记录。
pub fn prune_old_logs(now_secs: u64, keep_days: u32) -> Vec<String> {
    prune_old_logs_in(&logs_dir(), now_secs, keep_days)
}

/// 旧布局迁移：config 根下的 agent-audit.log 搬进 logs 目录（2026-09-17 起
/// 审计日志与运行日志同住 logs）。仅当日标不存在时移动，绝不覆盖新审计日志。
/// 返回是否发生了移动。`_in` 变体供单测注入目录。
pub fn migrate_legacy_agent_audit_in(config: &std::path::Path, logs: &std::path::Path) -> bool {
    let legacy = config.join("agent-audit.log");
    if !legacy.is_file() {
        return false;
    }
    let target = logs.join("agent-audit.log");
    if target.exists() {
        return false;
    }
    std::fs::rename(&legacy, &target).is_ok()
}

/// 对真实目录执行旧审计日志迁移。
pub fn migrate_legacy_agent_audit() -> bool {
    migrate_legacy_agent_audit_in(&config_dir(), &logs_dir())
}

// ---------------- 会话日志（F-7） ----------------

/// 会话日志根目录：`<config>/rustshell/session-logs/`。
///
/// 与运行日志 `logs/` **分开**：运行日志记的是「程序说了什么」，会话日志记的是
/// 「远端说了什么」。混在一起会让排查应用问题时，得在几百 MB 的终端输出里翻自己的行。
pub fn session_logs_dir() -> PathBuf {
    config_dir().join("session-logs")
}

/// 会话日志的当日子目录（`yyyy-MM-dd`，UTC+8 日界 —— 与运行日志同一套换算）。
pub fn session_log_day_dir(unix_secs: u64) -> PathBuf {
    session_logs_dir().join(day_folder_name(unix_secs))
}

/// UTC+8 的 `HHMMSS`（会话日志文件名的时间前缀）。
pub fn time_prefix(unix_secs: u64) -> String {
    let t = (unix_secs + 8 * 3600) % 86_400;
    format!("{:02}{:02}{:02}", t / 3600, (t % 3600) / 60, t % 60)
}

/// 会话日志文件名里「会话名」一段的最大字符数。
const MAX_LOG_NAME_CHARS: usize = 40;

/// 会话名 → 文件名安全片段。
///
/// 会话名是**用户输入**：可能含 Windows 非法字符（`\ / : * ? " < > |`）、控制字符、
/// 结尾的点或空格（Windows 会静默吃掉，于是「写进去的名字」与磁盘上的名字不一致），
/// 也可能整串都是非法字符，或者撞上 `CON` / `NUL` 这类保留设备名。任一条漏过，
/// 建文件就会失败 —— 而失败的表现是「开关开了却没有日志」，属于最难查的那类故障。
/// 所以这里一律收敛成一个**可预测且一定合法**的片段。
///
/// 保留设备名的处理**不分平台**：让 Windows 与 Unix 生成同一个名字，
/// 比按平台分叉更好预测（Unix 上多加一个下划线无害）。
pub fn sanitize_file_segment(name: &str) -> String {
    let mut out = String::new();
    for ch in name.chars() {
        let bad =
            ch.is_control() || matches!(ch, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|');
        let mapped = if bad { '_' } else { ch };
        // 连续非法字符折叠成一个下划线（`a:::b` → `a_b`，不是 `a___b`）
        if mapped == '_' && out.ends_with('_') {
            continue;
        }
        out.push(mapped);
    }
    let mut s: String = out
        .trim_matches([' ', '.'])
        .chars()
        .take(MAX_LOG_NAME_CHARS)
        .collect();
    // 截断又可能留下结尾的点/空格
    s = s.trim_matches([' ', '.']).to_string();
    // 全非法（`///` → `_`）或全空：这类名字说明不出任何东西，回退到一个固定词，
    // 而不是留一个 `_.log` 让人猜这是哪台机器。
    if s.is_empty() || s.chars().all(|c| c == '_') {
        return "session".to_string();
    }
    if is_reserved_device_name(&s) {
        s.insert(0, '_');
    }
    s
}

/// Windows 保留设备名（不区分大小写；带扩展名时 `Path::file_stem` 已把扩展名摘掉）。
fn is_reserved_device_name(s: &str) -> bool {
    let up = s.to_ascii_uppercase();
    matches!(
        up.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || (up.len() == 4
        && (up.starts_with("COM") || up.starts_with("LPT"))
        && matches!(up.as_bytes()[3], b'1'..=b'9'))
}

/// 本次会话的日志文件路径：`session-logs/<yyyy-MM-dd>/<HHMMSS>-<会话名>.log`。
///
/// **一次算好**（同一个 `now`）而不是留给调用方自己拼：日期目录与时间前缀分两次
/// 取时间，会在跨日那一刻落到两个目录里。
pub fn session_log_path_now(session_title: &str) -> PathBuf {
    let now = now_unix();
    session_log_day_dir(now).join(format!(
        "{}-{}.log",
        time_prefix(now),
        sanitize_file_segment(session_title)
    ))
}

/// 全部落盘文件的统一清单：配置、数据、日志一次列全。
///
/// 给「备份/迁移/排查」一个单一入口 —— 各处拼路径的调用点太多，想打包带走或
/// 检查落盘位置时不该逐个翻代码。返回的是**路径**不是文件内容；日志目录/会话日志
/// 是**目录**（含按天子目录），清单里给的是它们的根目录。
pub fn paths_inventory() -> Vec<(String, PathBuf)> {
    vec![
        ("settings".into(), settings_path()),
        ("connections".into(), connections_path()),
        ("quick_commands".into(), quick_commands_path()),
        ("ai_config".into(), ai_config_path()),
        ("ai_chats".into(), ai_chats_path()),
        ("cmd_history".into(), cmd_history_path()),
        ("session_layout".into(), session_layout_path()),
        ("keymap".into(), keymap_path()),
        ("tasks".into(), tasks_path()),
        ("alert_rules".into(), alert_rules_path()),
        ("smtp".into(), smtp_path()),
        ("agent_audit_log".into(), agent_audit_path()),
        ("alerts_log".into(), alerts_log_path()),
        ("run_logs_dir".into(), logs_dir()),
        ("session_logs_dir".into(), session_logs_dir()),
        ("task_runs_dir".into(), task_runs_dir()),
        ("export_dir".into(), connections_export_dir()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 列目录：目录在前、文件在后，各自按名排序；不存在时给出原因而不是空列表。
    #[test]
    fn list_dir_entries_orders_dirs_first_and_reports_errors() {
        let base = std::env::temp_dir().join("rustshell-paths-test-list");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("zzz_dir")).unwrap();
        std::fs::create_dir_all(base.join("aaa_dir")).unwrap();
        std::fs::write(base.join("bbb.txt"), "x").unwrap();
        std::fs::write(base.join("aab.txt"), "x").unwrap();

        let (entries, err) = list_dir_entries(&base);
        assert!(err.is_none(), "已存在的目录不该报错：{err:?}");
        let names: Vec<&str> = entries.iter().map(|(n, _, _)| n.as_str()).collect();
        assert_eq!(names, vec!["aaa_dir", "zzz_dir", "aab.txt", "bbb.txt"]);
        assert!(entries[0].2 && entries[1].2, "前两个必须是目录");
        assert!(!entries[2].2 && !entries[3].2, "后两个必须是文件");

        let (missing, err) = list_dir_entries(&base.join("不存在"));
        assert!(missing.is_empty());
        assert!(err.is_some(), "读不到的目录必须给出原因，不能假装是空目录");

        let _ = std::fs::remove_dir_all(&base);
    }

    /// 根列表至少要包含当前工作目录所在盘（否则选目录时无法换盘）。
    #[test]
    fn system_roots_is_not_empty() {
        assert!(!system_roots().is_empty());
    }

    /// 日期换算基准点：epoch 落在 1970-01-01；已知时刻换算不得漂移。
    #[test]
    fn day_folder_name_known_dates() {
        // epoch（+8h 仍是 1970-01-01）
        assert_eq!(day_folder_name(0), "1970-01-01");
        // 2023-11-14 22:13:20 UTC → +8h = 2023-11-15 06:13
        assert_eq!(day_folder_name(1_700_000_000), "2023-11-15");
        // 2026-09-17 00:00:00 UTC+8 前后：UTC 前一天 16:00 即换日
        let utc_2026_09_16_16 = 1_789_574_400u64; // 2026-09-16 16:00:00 UTC
        assert_eq!(day_folder_name(utc_2026_09_16_16), "2026-09-17");
        assert_eq!(day_folder_name(utc_2026_09_16_16 - 1), "2026-09-16");
    }

    /// 目录名解析与日期换算互为逆运算；非法名一律拒绝（防止把平铺文件当目录删）。
    #[test]
    fn parse_day_folder_round_trips_and_rejects_garbage() {
        for secs in [0u64, 1_700_000_000, 1_789_574_400, 4_102_444_800] {
            let name = day_folder_name(secs);
            let day = day_number(secs);
            assert_eq!(parse_day_folder(&name), Some(day), "round trip: {name}");
        }
        for bad in [
            "rustshell.log",
            "agent-audit.log",
            "2026-9-17",
            "2026/09/17",
            "2026-13-01",
            "2026-00-10",
            "2026-09-32",
            "abcd-09-17",
            "20260917",
            "2026-09-17x",
        ] {
            assert_eq!(parse_day_folder(bad), None, "应拒绝：{bad}");
        }
    }

    /// 保留期清理：过期日期目录整删、当日目录保留、平铺 rustshell.log* 删除、
    /// agent-audit.log 等无关文件不受影响。
    #[test]
    fn prune_old_logs_removes_expired_dirs_and_flat_files() {
        let base = std::env::temp_dir().join("rustshell-paths-test-prune");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("2020-01-01")).unwrap();
        std::fs::create_dir_all(base.join("1999-12-31")).unwrap();
        let today = day_folder_name(1_789_574_400); // 2026-09-17（UTC+8）
        std::fs::create_dir_all(base.join(&today)).unwrap();
        std::fs::write(base.join("rustshell.log"), "x").unwrap();
        std::fs::write(base.join("rustshell.log.2"), "x").unwrap();
        std::fs::write(base.join("agent-audit.log"), "x").unwrap();
        std::fs::write(base.join("not-a-date-dir.txt"), "x").unwrap();

        let mut removed = prune_old_logs_in(&base, 1_789_574_400, 30);
        removed.sort();
        assert_eq!(
            removed,
            vec![
                "1999-12-31".to_string(),
                "2020-01-01".to_string(),
                "rustshell.log".to_string(),
                "rustshell.log.2".to_string(),
            ]
        );
        // 当日目录与无关文件必须幸存
        assert!(base.join(&today).is_dir());
        assert!(base.join("agent-audit.log").is_file());
        assert!(base.join("not-a-date-dir.txt").is_file());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// 旧审计日志迁移：config 根的 agent-audit.log 搬进 logs；logs 已有则不动。
    #[test]
    fn migrate_legacy_agent_audit_moves_once_only() {
        let cfg = std::env::temp_dir().join("rustshell-paths-test-audit-cfg");
        let logs = std::env::temp_dir().join("rustshell-paths-test-audit-logs");
        let _ = std::fs::remove_dir_all(&cfg);
        let _ = std::fs::remove_dir_all(&logs);
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::create_dir_all(&logs).unwrap();

        // 无旧文件：不动
        assert!(!migrate_legacy_agent_audit_in(&cfg, &logs));

        // 有旧文件：搬走
        std::fs::write(cfg.join("agent-audit.log"), "legacy").unwrap();
        assert!(migrate_legacy_agent_audit_in(&cfg, &logs));
        assert!(!cfg.join("agent-audit.log").exists());
        assert_eq!(
            std::fs::read_to_string(logs.join("agent-audit.log")).unwrap(),
            "legacy"
        );

        // logs 已有同名文件：绝不覆盖
        std::fs::write(cfg.join("agent-audit.log"), "second").unwrap();
        assert!(!migrate_legacy_agent_audit_in(&cfg, &logs));
        assert!(cfg.join("agent-audit.log").is_file());
        assert_eq!(
            std::fs::read_to_string(logs.join("agent-audit.log")).unwrap(),
            "legacy"
        );
        let _ = std::fs::remove_dir_all(&cfg);
        let _ = std::fs::remove_dir_all(&logs);
    }

    /// UTC+8 时间前缀：跨日、跨小时、跨分都要对得上（0 秒与 59 秒是边界）。
    #[test]
    fn time_prefix_is_utc_plus_8() {
        // epoch（+8h）= 1970-01-01 08:00:00
        assert_eq!(time_prefix(0), "080000");
        // 2023-11-14 22:13:20 UTC → +8h = 2023-11-15 06:13:20
        assert_eq!(time_prefix(1_700_000_000), "061320");
        // 2026-09-16 16:00:00 UTC → 次日 00:00:00（与 day_folder_name 的日界一致）
        assert_eq!(time_prefix(1_789_574_400), "000000");
        assert_eq!(time_prefix(1_789_574_399), "235959");
    }

    /// 会话名净化：非法字符、控制字符、首尾点空格、空串、超长、保留设备名全覆盖。
    /// 判据不只是「看起来干净」，而是**能真的当文件名用**。
    #[test]
    fn sanitize_file_segment_always_yields_a_usable_name() {
        assert_eq!(sanitize_file_segment("我的服务器"), "我的服务器");
        assert_eq!(sanitize_file_segment("web:8080"), "web_8080");
        assert_eq!(
            sanitize_file_segment("a:::b"),
            "a_b",
            "连续非法字符折叠成一个"
        );
        assert_eq!(sanitize_file_segment("a/b\\c*d?e"), "a_b_c_d_e");
        assert_eq!(sanitize_file_segment("x\u{0}y"), "x_y", "控制字符");
        assert_eq!(sanitize_file_segment("trail."), "trail");
        assert_eq!(sanitize_file_segment("trail "), "trail");
        assert_eq!(sanitize_file_segment("  .lead"), "lead");
        assert_eq!(sanitize_file_segment(""), "session");
        assert_eq!(sanitize_file_segment("..."), "session");
        assert_eq!(sanitize_file_segment("///"), "session");
        // 保留设备名加前缀（Windows 上不加就建不出文件）
        assert_eq!(sanitize_file_segment("CON"), "_CON");
        assert_eq!(sanitize_file_segment("com1"), "_com1");
        assert_eq!(sanitize_file_segment("LPT9"), "_LPT9");
        assert_eq!(sanitize_file_segment("COM0"), "COM0", "COM0 不是保留名");
        // 超长截断到上限，且截断后不得残留结尾的点
        let long = sanitize_file_segment(&format!("{}.", "长".repeat(60)));
        assert_eq!(long.chars().count(), MAX_LOG_NAME_CHARS);
        assert!(!long.ends_with('.'));
        // 净化结果必须真的能落成文件
        let dir = std::env::temp_dir().join("rustshell-paths-test-sanitize");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for raw in ["CON", "web:8080", "我的服务器", "...", ""] {
            let p = dir.join(format!("{}.log", sanitize_file_segment(raw)));
            assert!(
                std::fs::write(&p, "x").is_ok(),
                "净化后仍建不出文件：{raw} → {}",
                p.display()
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 日志路径版式：`session-logs/<日期>/<HHMMSS>-<净化名>.log`，
    /// 且同一次调用里的日期目录与时间前缀来自同一个时刻。
    #[test]
    fn session_log_path_now_lays_out_day_dir_and_time_prefix() {
        let p = session_log_path_now("prod:web");
        assert_eq!(p.extension().and_then(|e| e.to_str()), Some("log"));
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        assert_eq!(name.len(), 6 + 1 + "prod_web".len() + 4, "{name}");
        assert!(name.ends_with("-prod_web.log"), "{name}");
        assert!(name[..6].bytes().all(|c| c.is_ascii_digit()), "{name}");
        let day = p.parent().unwrap();
        assert_eq!(day.parent().unwrap(), session_logs_dir());
        assert!(
            parse_day_folder(&day.file_name().unwrap().to_string_lossy()).is_some(),
            "日期目录名必须能被 parse_day_folder 认出来：{}",
            day.display()
        );
    }
}
