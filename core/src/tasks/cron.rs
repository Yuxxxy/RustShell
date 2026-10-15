//! cron 表达式解析与「下次触发时刻」计算。
//!
//! 支持标准 crontab 的 5 段子集（分 时 日 月 周），刻意**不**支持 `@daily` 之类宏、
//! 秒级字段、`L`/`W`/`#` 扩展 —— 定时任务的场景里这些都用不上，而每多一种语法
//! 就多一类解析分支要守。
//!
//! **时间基准一律是本机系统时区**（`chrono::Local`）：这是桌面软件，时区由操作系统
//! 自动给出（含夏令时规则），用户不需要、也不再提供手填时区；用户改系统时区，
//! 既有 cron 的语义跟着变 —— 这正是桌面软件该有的行为。
//! 夏令时跳变导致的「墙钟时刻不存在」在计算里有明确处理（跳过该分钟）。

use chrono::{Datelike, Local, NaiveDateTime, TimeZone as _, Timelike as _};
use serde::{Deserialize, Serialize};

/// 单段的最大取值（用于校验），与字段一一对应。
const FIELDS: [(&str, u32, u32); 5] = [
    ("分", 0, 59),
    ("时", 0, 23),
    ("日", 1, 31),
    ("月", 1, 12),
    ("周", 0, 6),
];

/// 解析失败的原因（给用户看的文案由 UI 层经 i18n 渲染）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CronError {
    /// 段数不是 5
    FieldCount(usize),
    /// 第 `ix` 段的取值越界（`value` 原样回显便于用户定位）
    OutOfRange { ix: usize, value: String },
    /// 第 `ix` 段语法错误
    BadSyntax { ix: usize, value: String },
    /// 范围起止颠倒
    ReversedRange { ix: usize, value: String },
    /// 步长不是正整数
    BadStep { ix: usize, value: String },
}

impl std::fmt::Display for CronError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FieldCount(n) => write!(f, "cron 需要 5 段（分 时 日 月 周），当前 {n} 段"),
            Self::OutOfRange { ix, value } => {
                let (name, lo, hi) = FIELDS[*ix];
                write!(f, "{name}段取值越界：{value}（允许 {lo}-{hi}）")
            }
            Self::BadSyntax { ix, value } => write!(f, "{}段语法错误：{value}", FIELDS[*ix].0),
            Self::ReversedRange { ix, value } => {
                write!(f, "{}段范围颠倒：{value}", FIELDS[*ix].0)
            }
            Self::BadStep { ix, value } => write!(f, "{}段步长无效：{value}", FIELDS[*ix].0),
        }
    }
}

impl std::error::Error for CronError {}

/// 一段展开后的取值集合（用 `Vec<bool>` 而不是 `HashSet`：域宽最大 60，
/// 位图足够小，且 `contains` 是 O(1) 无哈希开销 —— 下次触发计算会密集调用它）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct FieldSet {
    /// 下标 0 对应域的**最小值**（不是绝对值 0）
    bits: Vec<bool>,
    lo: u32,
    /// 该段是否为 `*`（或等价的「全覆盖」写法）。日/周取或的判定要用它。
    wildcard: bool,
}

impl FieldSet {
    fn contains(&self, v: u32) -> bool {
        v.checked_sub(self.lo)
            .and_then(|i| self.bits.get(i as usize))
            .copied()
            .unwrap_or(false)
    }

    fn len(&self) -> usize {
        self.bits.iter().filter(|b| **b).count()
    }
}

/// 解析后的 cron 计划。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronSchedule {
    minute: FieldSet,
    hour: FieldSet,
    day: FieldSet,
    month: FieldSet,
    /// 原始周字段：0-6（0 = 周日），7 已归一为 0
    weekday: FieldSet,
    /// 原始表达式（回显与 describe 用）
    expr: String,
}

/// 解析 5 段 cron 表达式。
///
/// 语法（每段相同）：`*` | `n` | `a-b` | `a,b,c` | `*/n` | `a-b/n` | `a/n`。
pub fn parse_cron(expr: &str) -> Result<CronSchedule, CronError> {
    let parts: Vec<&str> = expr.split_whitespace().collect();
    if parts.len() != 5 {
        return Err(CronError::FieldCount(parts.len()));
    }
    let minute = parse_field(parts[0], 0)?;
    let hour = parse_field(parts[1], 1)?;
    let day = parse_field(parts[2], 2)?;
    let month = parse_field(parts[3], 3)?;
    // 周字段单独走：允许 7 表示周日（归一为 0）
    let weekday = parse_weekday(parts[4])?;
    Ok(CronSchedule {
        minute,
        hour,
        day,
        month,
        weekday,
        expr: parts.join(" "),
    })
}

fn parse_field(raw: &str, ix: usize) -> Result<FieldSet, CronError> {
    let (_, lo, hi) = FIELDS[ix];
    build_set(raw, ix, lo, hi, false)
}

/// 周字段：域是 0-6，但用户常写 7 表示周日；先归一化再走通用逻辑。
fn parse_weekday(raw: &str) -> Result<FieldSet, CronError> {
    let (_, lo, hi) = FIELDS[4];
    // 只对整体是纯数字的写法做 7 -> 0 归一（列表/范围里的 7 由 build_set 内部处理）
    build_set(raw, 4, lo, hi, true)
}

/// 通用段解析。`allow_sunday_seven` 为真时把值 7 视作 0（仅周字段）。
fn build_set(
    raw: &str,
    ix: usize,
    lo: u32,
    hi: u32,
    allow_sunday_seven: bool,
) -> Result<FieldSet, CronError> {
    let width = (hi - lo + 1) as usize;
    let mut bits = vec![false; width];
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(CronError::BadSyntax {
            ix,
            value: raw.into(),
        });
    }
    // 归一化取值：周字段的 7 -> 0，其余原样
    let norm = |v: u32| -> u32 {
        if allow_sunday_seven && v == 7 {
            0
        } else {
            v
        }
    };
    // 范围校验要在归一化**之前**（7 对周字段合法，对分钟段不合法）
    let check = |v: u32| -> Result<(), CronError> {
        let ok = if allow_sunday_seven {
            (lo..=7).contains(&v)
        } else {
            (lo..=hi).contains(&v)
        };
        if ok {
            Ok(())
        } else {
            Err(CronError::OutOfRange {
                ix,
                value: v.to_string(),
            })
        }
    };

    let mut wildcard = false;
    for item in raw.split(',') {
        let item = item.trim();
        if item.is_empty() {
            return Err(CronError::BadSyntax {
                ix,
                value: raw.into(),
            });
        }
        // 拆步长：`a-b/n` 或 `*/n`
        let (body, step) = match item.split_once('/') {
            Some((b, s)) => {
                let n: u32 = s.trim().parse().map_err(|_| CronError::BadStep {
                    ix,
                    value: item.into(),
                })?;
                if n == 0 {
                    return Err(CronError::BadStep {
                        ix,
                        value: item.into(),
                    });
                }
                (b.trim(), n)
            }
            None => (item, 1),
        };

        // 单点 / 通配 / 范围
        let (from, to) = if body == "*" {
            wildcard = true;
            (lo, hi)
        } else if let Some((a, b)) = body.split_once('-') {
            let a: u32 = a.trim().parse().map_err(|_| CronError::BadSyntax {
                ix,
                value: item.into(),
            })?;
            let b: u32 = b.trim().parse().map_err(|_| CronError::BadSyntax {
                ix,
                value: item.into(),
            })?;
            check(a)?;
            check(b)?;
            let (na, nb) = (norm(a), norm(b));
            if na > nb {
                return Err(CronError::ReversedRange {
                    ix,
                    value: item.into(),
                });
            }
            (na, nb)
        } else {
            let v: u32 = body.parse().map_err(|_| CronError::BadSyntax {
                ix,
                value: item.into(),
            })?;
            check(v)?;
            let v = norm(v);
            // `a/n`：标准 crontab 语义是 `a-max/n`（从 a 起每 n 一个），
            // 与 `*/n` 的区别只在起点。不带步长时就是单点。
            if step > 1 {
                (v, hi)
            } else {
                (v, v)
            }
        };

        let mut v = from;
        loop {
            let idx = (v - lo) as usize;
            if idx < bits.len() {
                bits[idx] = true;
            }
            match v.checked_add(step) {
                Some(next) if next <= to => v = next,
                _ => break,
            }
        }
    }

    let set = FieldSet { bits, lo, wildcard };
    // 全覆盖（如 `0-59`）在语义上等同 `*`：日/周取或判定必须按「无限制」处理，
    // 否则 `0 0 1 * 0-6` 会被当成「1 号且周日」而不是「1 号或任意周几」。
    let full = set.len() == width;
    Ok(FieldSet {
        bits: set.bits,
        lo: set.lo,
        wildcard: set.wildcard || full,
    })
}

impl CronSchedule {
    /// 原始表达式（归一化空白后）。
    pub fn expr(&self) -> &str {
        &self.expr
    }

    /// 某个**墙钟**时刻是否命中本计划。
    ///
    /// 判定只看年月日时分周，与时区无关 —— 时区只决定「墙钟 ↔ unix 秒」的换算，
    /// 这一步交给 [`Self::next_strict`]。这样 cron 语义与夏令时处理彻底解耦，
    /// 也让本函数能直接单测（不需要构造任何时区）。
    pub fn matches_wall(&self, t: NaiveDateTime) -> bool {
        if !self.minute.contains(t.minute()) || !self.hour.contains(t.hour()) {
            return false;
        }
        if !self.month.contains(t.month()) {
            return false;
        }
        let day_ok = self.day.contains(t.day());
        // chrono 的 num_days_from_sunday: 0 = 周日，与 cron 一致
        let wd = t.weekday().num_days_from_sunday();
        let wd_ok = self.weekday.contains(wd);

        // crontab 惯例：日与周**都受限**时取或；任一不受限则只看受限的那一个。
        match (self.day.wildcard, self.weekday.wildcard) {
            (true, true) => true,
            (true, false) => wd_ok,
            (false, true) => day_ok,
            (false, false) => day_ok || wd_ok,
        }
    }

    /// 严格大于 `from`（unix 秒）的下一个命中时刻，在**本机系统时区**里计算
    /// （桌面软件自动获取，含夏令时规则）。
    ///
    /// 逐分钟推进：跨度上限 [`MAX_SCAN_MINUTES`]（约 4 年），足够覆盖
    /// 「2 月 29 日」这类最长间隔。
    pub fn next_strict(&self, from: u64) -> Option<u64> {
        let start = i64::try_from(from).ok()?.checked_add(60)?;
        // 对齐到整分钟：cron 的最小粒度是分钟
        let start = start - (start.rem_euclid(60));
        let mut naive = wall_clock_at(start)?;
        for _ in 0..MAX_SCAN_MINUTES {
            if let Some((ts, _)) = hit_at(naive, self) {
                if ts >= 0 {
                    return Some(ts as u64);
                }
            }
            // 推进一分钟（墙钟），墙钟不存在的分钟由 `hit_at` 返回 None 自然跳过
            naive = naive.checked_add_signed(chrono::Duration::minutes(1))?;
        }
        None
    }

    /// 人话描述（走 i18n）。
    ///
    /// 覆盖常见形态；无法归类的（如 `1,7,13 * * * *`）回退为原始表达式，
    /// 让用户至少能看到自己写了什么，而不是一句含糊的「自定义」。
    pub fn describe(&self) -> String {
        use crate::i18n::{t, ts};
        // 每天固定时刻：分时都是单点，日/月/周全通配
        if self.day.wildcard && self.month.wildcard && self.weekday.wildcard {
            if let (Some(m), Some(h)) = (single_of(&self.minute), single_of(&self.hour)) {
                return ts(
                    format!("每天 {:02}:{:02}", h, m),
                    format!("Daily at {:02}:{:02}", h, m),
                );
            }
            // 每 N 分钟（小时通配）
            if self.hour.wildcard {
                if let Some(desc) = every_minutes(&self.minute) {
                    return desc;
                }
            }
            // 每小时的第 M 分
            if let Some(m) = single_of(&self.minute) {
                return ts(format!("每小时第 {m} 分"), format!("Hourly at minute {m}"));
            }
        }
        // 每周固定星期与时刻
        if self.day.wildcard && self.month.wildcard && !self.weekday.wildcard {
            if let (Some(m), Some(h), Some(d)) = (
                single_of(&self.minute),
                single_of(&self.hour),
                single_of(&self.weekday),
            ) {
                let name = WEEKDAYS_ZH[d as usize % 7];
                let en = WEEKDAYS_EN[d as usize % 7];
                return ts(
                    format!("每周{name} {:02}:{:02}", h, m),
                    format!("Weekly on {en} at {:02}:{:02}", h, m),
                );
            }
        }
        // 每月固定日
        if !self.day.wildcard && self.month.wildcard && self.weekday.wildcard {
            if let (Some(m), Some(h), Some(d)) = (
                single_of(&self.minute),
                single_of(&self.hour),
                single_of(&self.day),
            ) {
                return ts(
                    format!("每月 {d} 日 {:02}:{:02}", h, m),
                    format!("Monthly on day {d} at {:02}:{:02}", h, m),
                );
            }
        }
        let _ = t("按计划");
        ts(
            format!("按计划（{}）", self.expr),
            format!("Cron ({})", self.expr),
        )
    }
}

/// 逐分钟扫描的上限（约 4 年）。`0 0 29 2 *` 需要跨越最长 4 年才命中一次。
const MAX_SCAN_MINUTES: u32 = 366 * 4 * 24 * 60;

const WEEKDAYS_ZH: [&str; 7] = ["日", "一", "二", "三", "四", "五", "六"];
const WEEKDAYS_EN: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

/// 域里只有一个取值时返回它（用于拼「每天 09:00」这类描述）。
fn single_of(set: &FieldSet) -> Option<u32> {
    if set.len() != 1 {
        return None;
    }
    set.bits.iter().position(|b| *b).map(|i| set.lo + i as u32)
}

/// 把「固定间隔」换算成**等价的 cron 表达式**。
///
/// 用途：任务编辑器只剩 cron 一种触发写法（按运行反馈收敛），而磁盘上可能
/// 还躺着 `Trigger::Interval` 的老任务。打开编辑器时把间隔换算成 cron，
/// 用户改完保存就自然迁到新写法 —— 而不是让「间隔」这个形态在 UI 上永久
/// 留一个后门（那正是这次要收敛掉的东西）。
///
/// 换算规则（只覆盖能被 cron **精确**表达的情形）：
/// * 分钟级且整除 60 的（1/2/3/4/5/6/10/12/15/20/30 分钟）→ `*/N * * * *`；
/// * 整小时且整除 24 的 → `0 */H * * *`；
/// * 整天的倍数 → `0 0 * * *`；
/// * **换算不了的一律回退 [`crate::tasks::DEFAULT_CRON`]**：宁可给一个用户
///   看得懂、改得动的默认值，也不要编一个语义不同的表达式 —— 那会让任务在
///   用户没察觉的情况下改变触发频率。
pub fn interval_to_cron(every_secs: u64) -> String {
    let mins = every_secs / 60;
    if mins > 0 && mins < 60 && 60 % mins == 0 {
        return format!("*/{mins} * * * *");
    }
    if mins >= 60 && mins.is_multiple_of(60) {
        let hours = mins / 60;
        if hours < 24 && 24 % hours == 0 {
            return format!("0 */{hours} * * *");
        }
        if hours.is_multiple_of(24) {
            return "0 0 * * *".to_string();
        }
    }
    crate::tasks::DEFAULT_CRON.to_string()
}

/// 分钟域是否是「每 N 分钟」形态，是则给出描述。
fn every_minutes(set: &FieldSet) -> Option<String> {
    use crate::i18n::ts;
    let hit: Vec<u32> = set
        .bits
        .iter()
        .enumerate()
        .filter(|(_, b)| **b)
        .map(|(i, _)| set.lo + i as u32)
        .collect();
    if hit.len() < 2 {
        return None;
    }
    let step = hit[1] - hit[0];
    if step == 0 || hit[0] != 0 {
        return None;
    }
    // 必须是等差且覆盖到域末
    let ok = hit.iter().enumerate().all(|(i, v)| *v == i as u32 * step)
        && *hit.last().unwrap() + step > 59;
    if !ok {
        return None;
    }
    Some(ts(
        format!("每 {step} 分钟"),
        format!("Every {step} minutes"),
    ))
}

/// 给定 unix 秒，取它在本机系统时区里的**墙钟**。
fn wall_clock_at(unix: i64) -> Option<NaiveDateTime> {
    Local
        .timestamp_opt(unix, 0)
        .single()
        .map(|dt| dt.naive_local())
}

/// 某个墙钟时刻在本机系统时区里对应的 `(unix 秒, 墙钟)`；该墙钟不存在时返回 `None`。
///
/// 夏令时「往前跳」时某些墙钟不存在（如 02:30 跳到 03:00）：`Local` 会返回
/// `LocalResult::None`，调用方据此把该分钟当作「不存在」跳过 —— 而不是静默
/// 触发一个用户没指定的时刻。
///
/// 夏令时「往后退」导致同一墙钟出现两次时取**较早**那个：同一墙钟不会被触发两次，
/// 因为调用方是「严格大于上次触发时刻」地推进。
fn hit_at(naive: NaiveDateTime, s: &CronSchedule) -> Option<(i64, NaiveDateTime)> {
    if !s.matches_wall(naive) {
        return None;
    }
    match Local.from_local_datetime(&naive) {
        chrono::LocalResult::Single(dt) | chrono::LocalResult::Ambiguous(dt, _) => {
            Some((dt.timestamp(), naive))
        }
        chrono::LocalResult::None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 辅助：从本地墙钟构造 unix 秒（测试里只验证「计算出的时刻落在期望的墙钟上」，
    /// 不硬编码具体时间戳 —— 那会让测试依赖运行机器的时区）。
    fn local(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> u64 {
        Local
            .with_ymd_and_hms(y, mo, d, h, mi, 0)
            .single()
            .expect("测试用的墙钟时刻必须存在")
            .timestamp() as u64
    }

    fn wall(ts: u64) -> (u32, u32, u32, u32) {
        let t = Local.timestamp_opt(ts as i64, 0).single().unwrap();
        (t.month(), t.day(), t.hour(), t.minute())
    }

    /// 间隔 → cron 的换算：能**精确**表达的要精确，表达不了的回退默认值。
    ///
    /// 「表达不了就回退」是这条的关键：编一个语义不同的表达式会让任务在用户
    /// 没察觉的情况下改变触发频率，比回退默认值严重得多。
    #[test]
    fn interval_converts_to_cron_or_falls_back() {
        // 分钟级：整除 60 的都能精确表达
        assert_eq!(interval_to_cron(60), "*/1 * * * *");
        assert_eq!(interval_to_cron(900), "*/15 * * * *");
        assert_eq!(interval_to_cron(1800), "*/30 * * * *");
        // 小时级：整除 24 的
        assert_eq!(interval_to_cron(3600), "0 */1 * * *");
        assert_eq!(interval_to_cron(6 * 3600), "0 */6 * * *");
        assert_eq!(interval_to_cron(12 * 3600), "0 */12 * * *");
        // 整天
        assert_eq!(interval_to_cron(24 * 3600), "0 0 * * *");
        assert_eq!(interval_to_cron(48 * 3600), "0 0 * * *");
        // 表达不了的（7 分钟、5 小时、90 分钟、秒级）一律回退默认值
        for bad in [7 * 60, 5 * 3600, 90 * 60, 0, 30] {
            assert_eq!(
                interval_to_cron(bad),
                crate::tasks::DEFAULT_CRON,
                "{bad}s 应当回退默认表达式"
            );
        }
        // 换出来的表达式必须**真的能被解析**（否则用户一保存就撞校验错误）
        for secs in [60u64, 900, 1800, 3600, 6 * 3600, 12 * 3600, 24 * 3600] {
            let expr = interval_to_cron(secs);
            assert!(
                parse_cron(&expr).is_ok(),
                "{secs}s 换算出的 `{expr}` 不是合法 cron"
            );
        }
        // 默认值本身也必须是合法 cron
        assert!(parse_cron(crate::tasks::DEFAULT_CRON).is_ok());
    }

    #[test]
    fn parses_field_count_and_syntax_errors() {
        assert_eq!(parse_cron("* * * *").unwrap_err(), CronError::FieldCount(4));
        assert_eq!(
            parse_cron("* * * * * *").unwrap_err(),
            CronError::FieldCount(6)
        );
        assert!(matches!(
            parse_cron("99 * * * *").unwrap_err(),
            CronError::OutOfRange { ix: 0, .. }
        ));
        assert!(matches!(
            parse_cron("* * 32 * *").unwrap_err(),
            CronError::OutOfRange { ix: 2, .. }
        ));
        assert!(matches!(
            parse_cron("* * * 13 *").unwrap_err(),
            CronError::OutOfRange { ix: 3, .. }
        ));
        assert!(matches!(
            parse_cron("1-5/0 * * * *").unwrap_err(),
            CronError::BadStep { ix: 0, .. }
        ));
        assert!(matches!(
            parse_cron("10-5 * * * *").unwrap_err(),
            CronError::ReversedRange { ix: 0, .. }
        ));
        assert!(matches!(
            parse_cron("abc * * * *").unwrap_err(),
            CronError::BadSyntax { ix: 0, .. }
        ));
        // 周字段允许 7 表示周日，但 8 不行
        assert!(parse_cron("* * * * 7").is_ok());
        assert!(parse_cron("* * * * 8").is_err());
    }

    #[test]
    fn every_fifteen_minutes_hits_expected_wall_clock() {
        let s = parse_cron("*/15 * * * *").unwrap();
        let base = local(2026, 9, 19, 10, 1);
        // 10:01 之后第一个 */15 是 10:15
        let next = s.next_strict(base).unwrap();
        assert_eq!(wall(next), (9, 19, 10, 15));
        // 再前进一个：10:30
        let next2 = s.next_strict(next).unwrap();
        assert_eq!(wall(next2), (9, 19, 10, 30));
        // 恰好落在命中点上时，next 必须是**严格大于**
        let on_hit = local(2026, 9, 19, 10, 15);
        assert_eq!(wall(s.next_strict(on_hit).unwrap()), (9, 19, 10, 30));
    }

    #[test]
    fn daily_nine_am_rolls_to_next_day() {
        let s = parse_cron("0 9 * * *").unwrap();
        let before = local(2026, 9, 19, 8, 59);
        assert_eq!(wall(s.next_strict(before).unwrap()), (9, 19, 9, 0));
        let after = local(2026, 9, 19, 9, 0);
        assert_eq!(wall(s.next_strict(after).unwrap()), (9, 20, 9, 0));
    }

    #[test]
    fn day_and_weekday_are_ored_when_both_restricted() {
        // crontab 惯例：`0 3 1 * 1` = 每月 1 号 **或** 每周一，03:00
        let s = parse_cron("0 3 1 * 1").unwrap();
        // 2026-09-19 是周六 → 下一个命中应是 09-21（周一）
        let base = local(2026, 9, 19, 0, 0);
        let t = Local
            .timestamp_opt(s.next_strict(base).unwrap() as i64, 0)
            .single()
            .unwrap();
        assert_eq!(t.day(), 21, "应当命中 9/21（周一）");
        assert_eq!(t.hour(), 3);

        // 只受限其一：`0 3 1 * *` 只看「1 号」
        let only_day = parse_cron("0 3 1 * *").unwrap();
        let t2 = Local
            .timestamp_opt(only_day.next_strict(base).unwrap() as i64, 0)
            .single()
            .unwrap();
        assert_eq!((t2.month(), t2.day()), (10, 1));
    }

    #[test]
    fn full_range_is_treated_as_unrestricted() {
        // `0-6` 是「任意周几」，等价 `*`；不能因为「非字面 *」就当成受限
        let s = parse_cron("0 3 1 * 0-6").unwrap();
        let base = local(2026, 9, 19, 0, 0);
        let t = Local
            .timestamp_opt(s.next_strict(base).unwrap() as i64, 0)
            .single()
            .unwrap();
        assert_eq!(
            (t.month(), t.day()),
            (10, 1),
            "0-6 等同通配，应等到下一个 1 号而不是「1 号且周日」"
        );
    }

    #[test]
    fn leap_day_expression_is_reachable_and_bounded() {
        // 2/29 最长要跨 4 年才命中：扫描上限必须够
        let s = parse_cron("0 0 29 2 *").unwrap();
        let base = local(2026, 3, 1, 0, 0);
        let next = s.next_strict(base).expect("2028-02-29 是可达的");
        let t = Local.timestamp_opt(next as i64, 0).single().unwrap();
        assert_eq!((t.year(), t.month(), t.day()), (2028, 2, 29));
        assert_eq!((t.hour(), t.minute()), (0, 0));
    }

    #[test]
    fn impossible_expression_returns_none() {
        // 2 月没有 30 号：合法语法但永不命中
        let s = parse_cron("0 0 30 2 *").unwrap();
        assert_eq!(s.next_strict(local(2026, 1, 1, 0, 0)), None);
    }

    #[test]
    fn weekdays_and_ranges() {
        // 工作日 09:30
        let s = parse_cron("30 9 * * 1-5").unwrap();
        // 2026-09-19 是周六 → 下一个是周一 09-21
        let t = Local
            .timestamp_opt(s.next_strict(local(2026, 9, 19, 12, 0)).unwrap() as i64, 0)
            .single()
            .unwrap();
        assert_eq!((t.month(), t.day(), t.hour(), t.minute()), (9, 21, 9, 30));

        // 周日也能用 7 写
        let sun7 = parse_cron("0 0 * * 7").unwrap();
        let sun0 = parse_cron("0 0 * * 0").unwrap();
        let base = local(2026, 9, 19, 0, 0);
        assert_eq!(sun7.next_strict(base), sun0.next_strict(base));
    }

    #[test]
    fn describe_covers_common_shapes() {
        // 描述文案走 i18n；测试只断言「包含关键数字」，不锁死中英文
        let d = parse_cron("0 9 * * *").unwrap().describe();
        assert!(d.contains("09") && d.contains("00"), "d={d}");

        let every = parse_cron("*/15 * * * *").unwrap().describe();
        assert!(every.contains("15"), "every={every}");

        let weekly = parse_cron("30 3 * * 1").unwrap().describe();
        assert!(
            weekly.contains("03") && weekly.contains("30"),
            "weekly={weekly}"
        );

        let monthly = parse_cron("0 2 15 * *").unwrap().describe();
        assert!(
            monthly.contains("15") && monthly.contains("02"),
            "monthly={monthly}"
        );

        // 无法归类 → 回退原表达式，且原表达式必须出现在描述里
        let odd = parse_cron("1,7,13 * * * *").unwrap().describe();
        assert!(odd.contains("1,7,13"), "odd={odd}");
    }

    #[test]
    fn month_field_is_honoured() {
        // 每年 6 月 1 日 00:00
        let s = parse_cron("0 0 1 6 *").unwrap();
        let t = Local
            .timestamp_opt(s.next_strict(local(2026, 9, 19, 0, 0)).unwrap() as i64, 0)
            .single()
            .unwrap();
        assert_eq!((t.year(), t.month(), t.day()), (2027, 6, 1));
    }

    #[test]
    fn advance_is_monotonic_across_dst_boundaries() {
        // 不做时区相关的精确断言（不同平台时区不同），只要求：
        // 连续推进 5000 次必须严格递增、不 panic、不回到过去。
        let s = parse_cron("*/10 * * * *").unwrap();
        let mut cur = local(2026, 3, 1, 0, 0);
        for _ in 0..5000 {
            let Some(next) = s.next_strict(cur) else {
                panic!("*/10 必须总能找到下一次");
            };
            assert!(next > cur, "next={next} cur={cur}");
            cur = next;
        }
        assert!(cur > local(2026, 3, 1, 0, 0));
    }
}
