//! 定时任务：任务模型、cron 调度、持久化。
//!
//! 分层意图：本模块只管「任务是什么、什么时候该跑、跑完记了什么」；
//! **怎么跑**在 [`crate::runner`]，**异常怎么判**在 [`crate::alerts`]。

pub mod cron;
pub mod types;

pub use cron::{interval_to_cron, parse_cron, CronError, CronSchedule};
pub use types::{
    shell_quote, validate_remote_path, CheckKind, Finding, FindingKind, LoadThresholds, LogLevel,
    NotifyPolicy, RunRecord, RunSession, RunStatus, Severity, TargetOutcome, TaskAction,
    TaskTarget, Trigger, DEFAULT_EXEC_TIMEOUT_SECS, MAX_EXEC_TIMEOUT_SECS, MAX_PATH_LEN,
    MAX_PATTERN_LEN, MAX_PROMPT_CHARS, MIN_INTERVAL_SECS, SESSION_TRACE_DETAIL_BYTES,
};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// 任务文件 schema 版本。字段缺失按 1 处理（其余字段一律 `serde(default)`）。
pub const TASKS_SCHEMA_VERSION: u32 = 1;

/// 连续失败多少次后自动停用任务。
///
/// 理由：一个永远失败的任务（如目标机器已下线）会按计划持续产生通知，
/// 是纯粹的骚扰源。停用 + 一次性告知，把决定权交回用户。
pub const AUTO_DISABLE_AFTER_FAILS: u32 = 5;

/// 每次运行最多落盘的逐台明细条数（其余只留摘要）。
pub const MAX_TARGETS_LOGGED: usize = 20;

/// 单个任务保留的运行记录条数上限。
pub const MAX_RUNS_PER_TASK: usize = 500;

/// 名称长度上限（字）。
pub const MAX_TASK_NAME_CHARS: usize = 40;

/// 新建任务时预填的 cron 表达式（每天 09:00）。
///
/// 放在 core 而不是 UI 字面量：任务对话框、`propose_task` 草案、以及
/// 「老任务是 Interval 时换算成什么」三处都要用同一个默认值，
/// 三处各写一遍迟早会出现「对话框给的是 9 点、草案给的是 3 点」。
pub const DEFAULT_CRON: &str = "0 9 * * *";

/// 一个定时任务。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub trigger: Trigger,
    pub action: TaskAction,

    /// 由 `trigger` 派生并落盘：下次应触发时刻（unix 秒）。
    ///
    /// 落盘而不是每次启动重算，是为了能判断「休眠期间错过了多久」——
    /// 只有拿得到原计划的时刻，才谈得上「宽限内执行 / 超宽限跳过」。
    #[serde(default)]
    pub next_run_at: u64,
    /// 最近一次运行时刻（0 = 从未）
    #[serde(default)]
    pub last_run_at: u64,
    #[serde(default)]
    pub last_status: RunStatus,
    /// 连续失败次数（成功即清零）
    #[serde(default)]
    pub consecutive_failures: u32,
    /// 最近一次运行的一句话摘要（列表直接渲染，避免为每行去读 jsonl）
    #[serde(default)]
    pub last_summary: Option<String>,
    /// 通知策略覆盖（None = 用默认）
    #[serde(default)]
    pub notify: Option<NotifyPolicy>,
}

impl Task {
    pub fn new(name: impl Into<String>, trigger: Trigger, action: TaskAction, now: u64) -> Self {
        let trigger_clone = trigger.clone();
        Self {
            id: crate::connections::uuid_like(),
            name: name.into(),
            enabled: true,
            next_run_at: trigger_clone.next_after(now).unwrap_or(0),
            trigger,
            action,
            last_run_at: 0,
            last_status: RunStatus::Never,
            consecutive_failures: 0,
            last_summary: None,
            notify: None,
        }
    }

    /// 保存前校验（名称 + 触发 + 动作三层）。
    pub fn validate(&self) -> Result<(), String> {
        use crate::i18n::ts;
        let name = self.name.trim();
        if name.is_empty() {
            return Err(ts("请填写任务名称".into(), "task name required".into()));
        }
        if name.chars().count() > MAX_TASK_NAME_CHARS {
            return Err(ts(
                format!("名称过长（上限 {MAX_TASK_NAME_CHARS} 字）"),
                format!("name too long (max {MAX_TASK_NAME_CHARS})"),
            ));
        }
        self.trigger.validate()?;
        self.action.validate()?;
        Ok(())
    }

    /// 重算下次触发的基准时刻。参数 `last` 是「从哪个时刻之后找下一次」。
    pub fn recompute_next(&mut self, from: u64) {
        self.next_run_at = self.trigger.next_after(from).unwrap_or(0);
    }

    /// 任务是否到点（不含 enabled / 重入判定，那些在调度器里）。
    pub fn is_due(&self, now: u64) -> bool {
        self.next_run_at > 0 && now >= self.next_run_at
    }

    /// 启停开关：停用时保留 `next_run_at`；重新启用时若已过期则重算到未来。
    pub fn set_enabled(&mut self, on: bool, now: u64) {
        self.enabled = on;
        if on {
            let expired = self.next_run_at == 0 || self.next_run_at <= now;
            if expired {
                self.recompute_next(now);
            }
        }
    }

    /// 通知策略（含默认）。
    pub fn notify_policy(&self) -> NotifyPolicy {
        self.notify.unwrap_or_default()
    }

    /// 列表副标题：动作 · 目标 · 规模。
    pub fn subtitle(&self, target_count: Option<usize>) -> String {
        let base = self.action.kind_label();
        let t = self.action.target().describe();
        match target_count {
            Some(n) => format!("{base} · {t} · {} 台", n),
            None => format!("{base} · {t}"),
        }
    }
}

/// 任务仓库（`tasks.json` 的内存形态）。
///
/// `PartialEq` 的用途是 UI 侧的「调度器改过状态了吗」判定：调度器与面板各持
/// 一份快照，只有比较出差异才重绘（否则 300ms 一轮的 `poll` 会不停重绘侧栏）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskStore {
    #[serde(default = "default_schema")]
    pub schema_version: u32,
    #[serde(default)]
    pub tasks: Vec<Task>,
}

fn default_schema() -> u32 {
    TASKS_SCHEMA_VERSION
}

impl Default for TaskStore {
    fn default() -> Self {
        Self {
            schema_version: TASKS_SCHEMA_VERSION,
            tasks: Vec::new(),
        }
    }
}

impl TaskStore {
    /// 从磁盘加载。文件不存在 → 空仓库（首次运行是常态，不是错误）；
    /// 解析失败 → 留底坏文件后回退空仓库（与 `AiConfig::load` 同口径）。
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(s) => {
                // 老文件里的 `kind:"manual"`（「仅手动」触发器已移除）会让整文件反序列化
                // 失败、把所有任务一起丢掉。先按 JSON 值重写这类任务，再正常解析。
                let prepared = migrate_legacy_json(&s).unwrap_or_else(|| s.clone());
                match serde_json::from_str::<Self>(&prepared) {
                    Ok(mut v) => {
                        v.schema_version = TASKS_SCHEMA_VERSION;
                        v.tasks.retain(|t| !t.id.trim().is_empty());
                        v
                    }
                    Err(_) => {
                        crate::configio::backup_broken_config(path, &s);
                        Self::default()
                    }
                }
            }
            Err(_) => Self::default(),
        }
    }

    /// 原子写（临时文件 + rename）。
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let json = serde_json::to_string_pretty(self)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    pub fn len(&self) -> usize {
        self.tasks.len()
    }

    pub fn get(&self, id: &str) -> Option<&Task> {
        self.tasks.iter().find(|t| t.id == id)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut Task> {
        self.tasks.iter_mut().find(|t| t.id == id)
    }

    /// 新增任务。返回新任务 id；**不**在这里校验（调用方先 `validate`，
    /// 因为校验失败的提示要落到对话框字段上）。
    pub fn add(&mut self, task: Task) -> String {
        let id = task.id.clone();
        self.tasks.push(task);
        id
    }

    /// 删除任务。返回是否存在。
    pub fn remove(&mut self, id: &str) -> bool {
        let before = self.tasks.len();
        self.tasks.retain(|t| t.id != id);
        self.tasks.len() != before
    }

    /// 复制一份（新 id、名字加后缀、`last_*` 归零、`next_run_at` 重算）。
    pub fn duplicate(&mut self, id: &str, now: u64) -> Option<String> {
        use crate::i18n::ts;
        let src = self.get(id)?.clone();
        let mut copy = src;
        copy.id = crate::connections::uuid_like();
        copy.name = ts(format!("{} 副本", copy.name), format!("{} copy", copy.name));
        copy.last_run_at = 0;
        copy.last_status = RunStatus::Never;
        copy.consecutive_failures = 0;
        copy.last_summary = None;
        copy.recompute_next(now);
        Some(self.add(copy))
    }

    /// 用编辑结果覆盖既有任务（保留运行状态，重算下次触发）。
    pub fn replace(&mut self, id: &str, mut next: Task, now: u64) -> bool {
        let Some(slot) = self.tasks.iter_mut().find(|t| t.id == id) else {
            return false;
        };
        // 运行状态属于「这个任务的历史」，编辑定义不该把它抹掉；
        // 但触发方式改了就必须重算下次时刻，否则会沿用旧计划的时刻。
        next.id = slot.id.clone();
        next.last_run_at = slot.last_run_at;
        next.last_status = slot.last_status;
        next.consecutive_failures = slot.consecutive_failures;
        next.last_summary = slot.last_summary.clone();
        next.recompute_next(now);
        *slot = next;
        true
    }

    /// 到点的任务（仅 `enabled`；重入与宽限判定在调度器）。
    pub fn due(&self, now: u64) -> Vec<String> {
        self.tasks
            .iter()
            .filter(|t| t.enabled && t.is_due(now))
            .map(|t| t.id.clone())
            .collect()
    }

    /// 最近一次运行时刻（用于「下次触发在列表里排最前」之类的展示排序）。
    pub fn next_fire_at(&self) -> Option<u64> {
        self.tasks
            .iter()
            .filter(|t| t.enabled && t.next_run_at > 0)
            .map(|t| t.next_run_at)
            .min()
    }

    /// 有未读异常的启停计数（竖栏徽标用：启用中的任务数）。
    pub fn enabled_count(&self) -> usize {
        self.tasks.iter().filter(|t| t.enabled).count()
    }

    /// 按「下次触发」升序的任务 id（手动触发的排最后，保持插入序稳定）。
    pub fn ordered_ids(&self) -> Vec<String> {
        let mut rows: Vec<(u64, usize, String)> = self
            .tasks
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let key = if t.enabled && t.next_run_at > 0 {
                    t.next_run_at
                } else {
                    u64::MAX
                };
                (key, i, t.id.clone())
            })
            .collect();
        rows.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        rows.into_iter().map(|(_, _, id)| id).collect()
    }

    /// 按名称过滤后的任务 id（面板搜索框用），保持 [`Self::ordered_ids`] 的次序。
    ///
    /// 关键字取**包含**匹配、大小写不敏感：任务名多为「早间巡检」这类短语，
    /// 要求全名相等等于没得搜。空关键字返回全部 —— 没打字时列表必须完整。
    pub fn ids_matching(&self, query: &str) -> Vec<String> {
        let q = query.trim().to_lowercase();
        self.ordered_ids()
            .into_iter()
            .filter(|id| {
                if q.is_empty() {
                    return true;
                }
                self.get(id)
                    .map(|t| t.name.to_lowercase().contains(&q))
                    .unwrap_or(false)
            })
            .collect()
    }
}

/// 老任务文件的预解析迁移：把已移除的「仅手动」触发器改写成默认 cron（并停用）。
///
/// 为什么必须做：`Trigger::Manual` 变体删掉后，`kind:"manual"` 会让**整文件**反序列化
/// 失败 —— 所有任务一起丢。改写后保持「永不自动触发」的语义：默认表达式 + `enabled:false`。
/// 旧文件里的 `tz` 字段不用动，serde 默认忽略未知字段。
///
/// 返回 `None` 表示文件里没有可迁移的内容（或压根不是预期结构），
/// 调用方直接拿原文解析，失败时照常留底。
fn migrate_legacy_json(s: &str) -> Option<String> {
    let mut v: serde_json::Value = serde_json::from_str(s).ok()?;
    let tasks = v.get_mut("tasks")?.as_array_mut()?;
    let mut touched = false;
    for t in tasks.iter_mut() {
        let is_manual = t
            .get("trigger")
            .and_then(|tr| tr.get("kind"))
            .and_then(|k| k.as_str())
            == Some("manual");
        if !is_manual {
            continue;
        }
        if let Some(trigger) = t.get_mut("trigger") {
            *trigger = serde_json::json!({ "kind": "cron", "expr": DEFAULT_CRON });
        }
        if let Some(obj) = t.as_object_mut() {
            obj.insert("enabled".into(), serde_json::json!(false));
        }
        touched = true;
    }
    touched.then(|| serde_json::to_string(&v).ok()).flatten()
}

// 运行记录存储

/// 运行记录：内存里持每个任务的最近若干条，历史从 `task-runs/<id>.jsonl` 按需读。
#[derive(Debug, Default)]
pub struct RunLogStore {
    /// task_id -> 最近记录（新的在前）
    recent: std::collections::HashMap<String, Vec<RunRecord>>,
}

/// 内存里每个任务保留的条数（渲染只用到最近 20 条）。
const RECENT_IN_MEMORY: usize = 20;

impl RunLogStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// 追加一条记录到内存与磁盘。磁盘写失败只记日志，不影响任务结果。
    pub fn push(&mut self, dir: &Path, rec: RunRecord) {
        if let Err(e) = append_record(dir, &rec) {
            log::warn!("运行记录落盘失败 task={}: {e}", rec.task_id);
        }
        let slot = self.recent.entry(rec.task_id.clone()).or_default();
        slot.insert(0, rec);
        slot.truncate(RECENT_IN_MEMORY);
    }

    pub fn recent(&self, task_id: &str) -> &[RunRecord] {
        self.recent
            .get(task_id)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// 最近一条（列表行「上次结果」用；数据源优先用内存，避免为每行读盘）。
    pub fn latest(&self, task_id: &str) -> Option<&RunRecord> {
        self.recent.get(task_id).and_then(|v| v.first())
    }

    /// 从磁盘回填最近记录（应用启动时调用一次）。
    pub fn warm(&mut self, dir: &Path, task_id: &str) {
        let path = run_log_path(dir, task_id);
        let Ok(text) = std::fs::read_to_string(&path) else {
            return;
        };
        let mut recs: Vec<RunRecord> = Vec::new();
        for line in text.lines().rev().take(RECENT_IN_MEMORY) {
            if let Ok(r) = serde_json::from_str::<RunRecord>(line) {
                recs.push(r);
            }
        }
        if !recs.is_empty() {
            self.recent.insert(task_id.to_string(), recs);
        }
    }

    /// 任务被删除时清理内存态（磁盘记录保留，便于事后追溯）。
    pub fn forget(&mut self, task_id: &str) {
        self.recent.remove(task_id);
    }
}

/// 任务运行记录文件路径。
pub fn run_log_path(dir: &Path, task_id: &str) -> PathBuf {
    dir.join(format!("{task_id}.jsonl"))
}

/// 追加一条运行记录（JSON Lines），并在超出上限时裁剪。
pub fn append_record(dir: &Path, rec: &RunRecord) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let path = run_log_path(dir, &rec.task_id);
    let line = serde_json::to_string(rec)?;
    {
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        writeln!(f, "{line}")?;
    }
    trim_records(&path, MAX_RUNS_PER_TASK);
    Ok(())
}

/// 记录条数超过 `keep` 时重写文件（保留最后 `keep` 行）。
///
/// 每次追加都数一遍行数：文件在百行量级，读一遍的成本远低于「永不清剪」
/// 带来的磁盘增长；这也是唯一能保证「坏行不会让后续解析全崩」的写法。
fn trim_records(path: &Path, keep: usize) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.len() <= keep {
        return;
    }
    let tail = &lines[lines.len() - keep..];
    let tmp = path.with_extension("jsonl.tmp");
    if std::fs::write(&tmp, format!("{}\n", tail.join("\n"))).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

/// 读取某个任务的历史记录（最多 `limit` 条，新的在前）。
pub fn read_records(dir: &Path, task_id: &str, limit: usize) -> Vec<RunRecord> {
    let path = run_log_path(dir, task_id);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let mut out: Vec<RunRecord> = Vec::new();
    for line in text.lines().rev() {
        if out.len() >= limit {
            break;
        }
        if let Ok(r) = serde_json::from_str::<RunRecord>(line) {
            out.push(r);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use types::{CheckKind as _CheckKind, LogLevel};

    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "rs-tasks-{tag}-{}-{}",
            std::process::id(),
            unique_suffix()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// 单调递增的测试目录后缀。
    ///
    /// **不能**用秒级时间戳：同一秒内重跑（或两个用例并行）会拿到同名目录，
    /// 上一条留下的文件会被读回来，断言随之莫名其妙地失败。
    fn unique_suffix() -> u64 {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst) as u64
            ^ (std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0))
    }

    fn sample_action() -> TaskAction {
        TaskAction::ExecMulti {
            target: TaskTarget {
                groups: vec!["g1".into()],
                ..Default::default()
            },
            command: "df -h".into(),
            timeout_secs: 15,
            allow_mutating: false,
        }
    }

    #[test]
    fn new_task_gets_a_future_next_run() {
        let now = 1_700_000_000u64;
        let t = Task::new(
            "磁盘检查",
            Trigger::Interval { every_secs: 3600 },
            sample_action(),
            now,
        );
        assert!(t.enabled);
        assert!(t.next_run_at > now, "新建任务的下次触发必须在未来");
        assert_eq!(t.next_run_at % 3600, 0, "interval 要对齐整数倍");
        assert_eq!(t.last_status, RunStatus::Never);
        assert!(t.validate().is_ok());
    }

    #[test]
    fn validate_reports_name_and_action_problems() {
        let now = 1000;
        let mut t = Task::new(
            "  ",
            Trigger::Interval { every_secs: 60 },
            sample_action(),
            now,
        );
        assert!(t.validate().is_err(), "空名必须被拒");
        t.name = "x".repeat(MAX_TASK_NAME_CHARS + 1);
        assert!(t.validate().is_err(), "超长名必须被拒");
        t.name = "ok".into();
        t.trigger = Trigger::cron("bad");
        assert!(t.validate().is_err(), "坏 cron 必须被拒");
        t.trigger = Trigger::Interval { every_secs: 60 };
        t.action = TaskAction::MultiConnect {
            target: TaskTarget::default(),
            open_tabs: false,
            keep_alive_secs: 0,
        };
        assert!(t.validate().is_err(), "空目标必须被拒");
    }

    #[test]
    fn enable_toggle_recomputes_only_when_expired() {
        let now = 1_700_000_000u64;
        let mut t = Task::new(
            "t",
            Trigger::Interval { every_secs: 3600 },
            sample_action(),
            now,
        );
        let planned = t.next_run_at;
        assert!(planned > now);

        // 停用：保留原计划时刻（重新启用时才好判断是否过期）
        t.set_enabled(false, now);
        assert_eq!(t.next_run_at, planned);

        // 未过期重新启用：不动计划
        t.set_enabled(true, now);
        assert_eq!(t.next_run_at, planned);

        // 已过期重新启用：重算到未来
        t.set_enabled(false, now);
        let after = planned + 10;
        t.set_enabled(true, after);
        assert!(t.next_run_at > after, "过期的计划必须重算到未来");
    }

    #[test]
    fn store_add_remove_duplicate_roundtrip() {
        let now = 1_700_000_000u64;
        let mut store = TaskStore::default();
        let t = Task::new(
            "prod 巡检",
            Trigger::Interval { every_secs: 600 },
            sample_action(),
            now,
        );
        let id = store.add(t);
        assert_eq!(store.len(), 1);

        let dup = store.duplicate(&id, now).unwrap();
        assert_ne!(dup, id);
        assert_eq!(store.len(), 2);
        assert_eq!(store.get(&dup).unwrap().last_status, RunStatus::Never);
        assert!(store.get(&dup).unwrap().name.contains("副本"));

        assert!(store.remove(&id));
        assert!(store.get(&id).is_none());
        assert!(!store.remove("nope"));
    }

    #[test]
    fn replace_keeps_history_but_recomputes_schedule() {
        let now = 1_700_000_000u64;
        let mut store = TaskStore::default();
        let mut t = Task::new(
            "t",
            Trigger::Interval { every_secs: 3600 },
            sample_action(),
            now,
        );
        t.last_run_at = now - 10;
        t.last_status = RunStatus::Partial;
        t.consecutive_failures = 2;
        t.last_summary = Some("2 台异常".into());
        let id = store.add(t);

        let edited = Task::new("改名了", Trigger::cron("0 9 * * *"), sample_action(), now);
        assert!(store.replace(&id, edited, now));
        let got = store.get(&id).unwrap();
        assert_eq!(got.name, "改名了");
        // 历史保留
        assert_eq!(got.last_status, RunStatus::Partial);
        assert_eq!(got.consecutive_failures, 2);
        assert_eq!(got.last_summary.as_deref(), Some("2 台异常"));
        // 但触发方式换了 -> 下次时刻必须按新表达式重算
        assert!(got.next_run_at > now);
        assert!(!store.replace(
            "nope",
            Task::new(
                "x",
                Trigger::Interval { every_secs: 60 },
                sample_action(),
                now
            ),
            now
        ));
    }

    #[test]
    fn due_lists_only_enabled_and_reached_tasks() {
        let now = 1_700_000_000u64;
        let mut store = TaskStore::default();
        let mut a = Task::new(
            "a",
            Trigger::Interval { every_secs: 3600 },
            sample_action(),
            now,
        );
        a.next_run_at = now - 1; // 已到点
        let a_id = store.add(a);

        let mut b = Task::new(
            "b",
            Trigger::Interval { every_secs: 3600 },
            sample_action(),
            now,
        );
        b.next_run_at = now - 1;
        b.enabled = false;
        store.add(b);

        let mut c = Task::new(
            "c",
            Trigger::Interval { every_secs: 3600 },
            sample_action(),
            now,
        );
        c.next_run_at = now + 100;
        store.add(c);

        assert_eq!(store.due(now), vec![a_id]);
        assert_eq!(store.enabled_count(), 2);
    }

    #[test]
    fn ordered_ids_put_disabled_last() {
        let now = 1_700_000_000u64;
        let mut store = TaskStore::default();
        let mut later = Task::new(
            "later",
            Trigger::Interval { every_secs: 3600 },
            sample_action(),
            now,
        );
        later.next_run_at = now + 7200;
        let later_id = store.add(later);
        let mut sooner = Task::new(
            "sooner",
            Trigger::Interval { every_secs: 3600 },
            sample_action(),
            now,
        );
        sooner.next_run_at = now + 60;
        let sooner_id = store.add(sooner);
        // 停用的任务即使「下次时刻」更早也必须排最后
        let mut off = Task::new(
            "off",
            Trigger::Interval { every_secs: 3600 },
            sample_action(),
            now,
        );
        off.enabled = false;
        off.next_run_at = now + 30;
        let off_id = store.add(off);

        assert_eq!(store.ordered_ids(), vec![sooner_id, later_id, off_id]);
    }

    /// 面板搜索：空关键字给全部、按名称包含匹配、大小写不敏感、无匹配给空。
    #[test]
    fn ids_matching_filters_by_name() {
        let now = 1_700_000_000u64;
        let mut store = TaskStore::default();
        let disk = store.add(Task::new(
            "磁盘巡检",
            Trigger::Interval { every_secs: 3600 },
            sample_action(),
            now,
        ));
        let load = store.add(Task::new(
            "Disk Load Check",
            Trigger::Interval { every_secs: 3600 },
            sample_action(),
            now,
        ));
        let all = store.ordered_ids();

        // 没打字 = 不过滤（不是「什么都没匹配」）
        assert_eq!(store.ids_matching(""), all);
        assert_eq!(store.ids_matching("   "), all);
        // 中文按包含匹配
        assert_eq!(store.ids_matching("磁盘"), vec![disk.clone()]);
        // 英文大小写不敏感
        assert_eq!(store.ids_matching("disk"), vec![load.clone()]);
        assert_eq!(store.ids_matching("DISK LOAD"), vec![load]);
        // 搜不到就是空（面板据此给「没有匹配的任务」，而不是把全部显示出来）
        assert!(store.ids_matching("不存在").is_empty());
    }

    #[test]
    fn store_persists_and_survives_broken_file() {
        let dir = tmp_dir("store");
        let path = dir.join("tasks.json");
        let now = 1_700_000_000u64;

        let mut store = TaskStore::default();
        store.add(Task::new(
            "持久化",
            Trigger::cron("*/30 * * * *"),
            sample_action(),
            now,
        ));
        store.save(&path).unwrap();

        let back = TaskStore::load(&path);
        assert_eq!(back.len(), 1);
        assert_eq!(back.tasks[0].name, "持久化");
        assert_eq!(back.schema_version, TASKS_SCHEMA_VERSION);
        assert!(back.tasks[0].next_run_at > now);

        // 坏文件：留底 + 回退空仓库，不 panic
        std::fs::write(&path, "{not json").unwrap();
        let broken = TaskStore::load(&path);
        assert!(broken.is_empty());
        assert!(dir.join("tasks.json.broken").exists() || broken.is_empty());

        // 不存在的文件 -> 空仓库
        assert!(TaskStore::load(&dir.join("nope.json")).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 老文件里的「仅手动」任务（`kind:"manual"`）必须被**迁移**成默认 cron 且停用 ——
    /// 不迁移的话整文件反序列化失败，用户的任务会被整个丢进 `.broken` 备份。
    /// 同一文件里的正常任务照常加载，老 `tz` 字段被 serde 忽略。
    #[test]
    fn load_migrates_legacy_manual_trigger_and_ignores_tz() {
        let dir = tmp_dir("legacy");
        let path = dir.join("tasks.json");
        let now = 1_700_000_000u64;

        let mut store = TaskStore::default();
        store.add(Task::new(
            "老的手动任务",
            Trigger::cron(DEFAULT_CRON),
            sample_action(),
            now,
        ));
        store.add(Task::new(
            "带时区的老 cron",
            Trigger::cron("0 8 * * *"),
            sample_action(),
            now,
        ));
        let (manual_id, cron_id) = (store.tasks[0].id.clone(), store.tasks[1].id.clone());
        // 手工改回老形态：第一条 manual（带老 tz 字段），第二条 cron 带 tz
        let mut v = serde_json::to_value(&store).unwrap();
        let tasks = v.get_mut("tasks").unwrap().as_array_mut().unwrap();
        tasks[0]["trigger"] = serde_json::json!({ "kind": "manual", "tz": "UTC+08:00" });
        tasks[0]["enabled"] = serde_json::json!(true);
        tasks[1]["trigger"]["tz"] = serde_json::json!("Asia/Shanghai");
        std::fs::write(&path, serde_json::to_string(&v).unwrap()).unwrap();

        let back = TaskStore::load(&path);
        assert_eq!(back.len(), 2, "不能因 manual 整文件丢");
        let m = back.get(&manual_id).unwrap();
        assert_eq!(m.trigger, Trigger::cron(DEFAULT_CRON));
        assert!(!m.enabled, "迁移出来的任务必须停用（保持「永不自动触发」）");
        let c = back.get(&cron_id).unwrap();
        assert_eq!(c.trigger, Trigger::cron("0 8 * * *"));
        assert!(c.enabled, "正常任务不受迁移影响");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_log_appends_warms_and_trims() {
        let dir = tmp_dir("runs");
        let mut mem = RunLogStore::new();
        let mk = |i: u64| RunRecord {
            run_id: format!("r{i}"),
            task_id: "t1".into(),
            task_name: "巡检".into(),
            started_at: i,
            finished_at: i + 1,
            status: RunStatus::Ok,
            targets: Vec::new(),
            machine: Vec::new(),
            conclusion: None,
            findings: Vec::new(),
            note: None,
            session: None,
        };
        for i in 0..5 {
            mem.push(&dir, mk(i));
        }
        // 内存里新的在前
        assert_eq!(mem.recent("t1").len(), 5);
        assert_eq!(mem.latest("t1").unwrap().run_id, "r4");

        // 磁盘可回读
        let from_disk = read_records(&dir, "t1", 3);
        assert_eq!(from_disk.len(), 3);
        assert_eq!(from_disk[0].run_id, "r4");

        // warm 回填
        let mut fresh = RunLogStore::new();
        fresh.warm(&dir, "t1");
        assert_eq!(fresh.recent("t1").len(), 5);

        // forget 清内存
        fresh.forget("t1");
        assert!(fresh.recent("t1").is_empty());

        // 裁剪：写入超过上限后文件行数不超过上限
        for i in 0..(MAX_RUNS_PER_TASK + 20) {
            append_record(&dir, &mk(i as u64)).unwrap();
        }
        let text = std::fs::read_to_string(run_log_path(&dir, "t1")).unwrap();
        let lines = text.lines().filter(|l| !l.trim().is_empty()).count();
        assert!(lines <= MAX_RUNS_PER_TASK, "lines={lines}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 会话痕迹必须能过一遍 jsonl：这是「重启后点开某次运行的会话还能看到
    /// 那次查了什么」的**唯一**依据。写不进去或读不回来，界面上都表现为
    /// 「会话打不开」，而这类故障在内存态测试里完全看不出来。
    #[test]
    fn run_record_session_survives_the_jsonl_round_trip() {
        use crate::agent::{TraceRow, TraceState};
        let dir = tmp_dir("run-session");
        let mut rec = RunRecord {
            run_id: "r1".into(),
            task_id: "t1".into(),
            task_name: "夜间巡检".into(),
            started_at: 10,
            finished_at: 20,
            status: RunStatus::Partial,
            targets: Vec::new(),
            machine: Vec::new(),
            conclusion: Some("发现 1 台内存偏高".into()),
            findings: Vec::new(),
            note: None,
            session: None,
        };
        let mut row = TraceRow::new(1, "c1", "inspect_load");
        row.summary = "prod-01".into();
        row.state = TraceState::Anomaly;
        row.note = "发现异常".into();
        row.elapsed_ms = 812;
        row.detail = "内存 94%".into();
        rec.session = Some(RunSession {
            prompt: "检查内存".into(),
            conclusion: "发现 1 台内存偏高".into(),
            trace: vec![row],
        });

        append_record(&dir, &rec).unwrap();
        let back = read_records(&dir, "t1", 1);
        assert_eq!(back.len(), 1, "记录没读回来");
        let sess = back[0].session.as_ref().expect("会话字段丢了");
        assert_eq!(sess.prompt, "检查内存");
        assert_eq!(sess.trace.len(), 1);
        assert_eq!(sess.trace[0].state, TraceState::Anomaly, "轨迹状态丢了");
        assert_eq!(sess.trace[0].detail, "内存 94%", "轨迹明细丢了");

        // 老记录（没有 session 字段）必须照旧能读 —— 字段是后加的。
        let legacy = std::fs::read_to_string(run_log_path(&dir, "t1")).unwrap();
        let stripped: String = legacy
            .lines()
            .map(|l| match l.find(",\"session\":") {
                Some(ix) => l[..ix].to_string() + "}",
                None => l.to_string(),
            })
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(run_log_path(&dir, "t1"), stripped).unwrap();
        let old = read_records(&dir, "t1", 1);
        assert_eq!(old.len(), 1, "老记录读不出来了");
        assert!(old[0].session.is_none(), "缺字段应当按 None 读");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn subtitle_mentions_action_and_target() {
        let t = Task::new(
            "t",
            Trigger::cron("0 9 * * *"),
            TaskAction::AiInspect {
                target: TaskTarget {
                    groups: vec!["g1".into()],
                    ..Default::default()
                },
                prompt: String::new(),
                checks: vec![_CheckKind::Load],
                thresholds: None,
                log_path: None,
                log_level: LogLevel::Error,
            },
            0,
        );
        let s = t.subtitle(Some(3));
        assert!(s.contains('3'), "s={s}");
        assert!(!t.action.target().is_empty());
    }
}
