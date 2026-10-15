//! 预警（告警）系统。
//!
//! 分层：
//! * [`types`]：规则 / 条件 / 去重记录 / 通知的数据类型；
//! * [`engine`]：判定 + 去重 + 恢复检测（**纯逻辑**，无 IO）；
//! * [`notify`]：通道抽象与派发（日志通道 + 邮件通道桩）。
//!
//! 持久化（`alert_rules.json`）在本模块的 [`AlertStore`] 扩展里，
//! 与 `tasks` 保持同一套「原子写 + 坏文件留底」的口径。

pub mod digest;
pub mod engine;
pub mod notify;
pub mod types;

pub use digest::PendingDigest;
pub use engine::{
    apply_notification, evaluate, exit_fingerprint, log_fingerprint, metric_fingerprint, EvalInput,
    HOST_KEY_FP, HOST_KEY_UNKNOWN_FP,
};
pub use notify::{LogChannel, Notifier, NotifyChannel, SmtpChannel};
pub use types::{
    builtin_rules, AlertCondition, AlertRule, AlertStore, DedupEntry, Evaluation, Finding,
    FindingKind, LoadThresholds, Notification, Severity, ALERTS_SCHEMA_VERSION, BUILTIN_PREFIX,
    DEFAULT_SUPPRESS_SECS, RULE_HOST_KEY, RULE_HOST_KEY_UNKNOWN,
};

use anyhow::Result;
use std::path::Path;

impl AlertStore {
    /// 加载。文件不存在 -> 内置规则种子（首次运行要有东西可用）；
    /// 解析失败 -> 留底坏文件并把内置规则补回来。
    ///
    /// 注意「补回内置规则」这一步：如果只是回退默认，用户自定义的规则会全丢；
    /// 但如果不补内置规则，一台曾经坏过配置的机器就永远不再有基础告警 ——
    /// 两种都不可接受，所以这里做的是「空规则集 -> 补内置」的**增量**修复。
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(s) => match serde_json::from_str::<Self>(&s) {
                Ok(mut v) => {
                    v.schema_version = ALERTS_SCHEMA_VERSION;
                    v.ensure_builtins();
                    v
                }
                Err(_) => {
                    crate::configio::backup_broken_config(path, &s);
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        }
    }

    /// 原子写。
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

    /// 把缺失的内置规则补回来（保留用户已经改过的同名规则）。
    pub fn ensure_builtins(&mut self) {
        for b in builtin_rules() {
            if !self.rules.iter().any(|r| r.id == b.id) {
                self.rules.push(b);
            }
        }
    }

    pub fn rule(&self, id: &str) -> Option<&AlertRule> {
        self.rules.iter().find(|r| r.id == id)
    }

    pub fn rule_mut(&mut self, id: &str) -> Option<&mut AlertRule> {
        self.rules.iter_mut().find(|r| r.id == id)
    }

    /// 删除规则。内置规则（`builtin.` 前缀）不允许删除 —— 返回 false。
    pub fn remove_rule(&mut self, id: &str) -> bool {
        let Some(r) = self.rule(id) else {
            return false;
        };
        if r.is_builtin() {
            return false;
        }
        self.rules.retain(|r| r.id != id);
        true
    }

    /// 新增或覆盖一条规则（按 id）。返回是否为替换。
    pub fn upsert_rule(&mut self, rule: AlertRule) -> bool {
        match self.rules.iter_mut().find(|r| r.id == rule.id) {
            Some(slot) => {
                *slot = rule;
                true
            }
            None => {
                self.rules.push(rule);
                false
            }
        }
    }

    /// 当前处于 firing 的条目（UI 的「当前异常」列表）。
    pub fn firing(&self) -> Vec<&DedupEntry> {
        let mut v: Vec<&DedupEntry> = self.dedup.iter().filter(|e| e.firing).collect();
        // 严重的排前面：用户先看 Critical
        v.sort_by(|a, b| {
            b.severity
                .cmp(&a.severity)
                .then(b.first_seen.cmp(&a.first_seen))
        });
        v
    }

    /// 规则改动后清理不再适用的去重状态（如规则被删/换了作用域）。
    ///
    /// 不做这一步的话，删掉一条规则后它的 firing 条目会永远留在列表里，
    /// 用户既看不到对应规则也无法让它恢复。
    pub fn prune_dedup(&mut self) {
        let live: std::collections::HashSet<String> =
            self.rules.iter().map(|r| r.id.clone()).collect();
        self.dedup.retain(|e| live.contains(&e.rule_id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "rs-alertstore-{tag}-{}-{}",
            std::process::id(),
            unique_suffix()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// 单调递增的测试目录后缀（秒级时间戳在同一秒内重跑会撞名）。
    fn unique_suffix() -> u64 {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst) as u64
            ^ (std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0))
    }

    #[test]
    fn default_store_seeds_builtin_rules() {
        let s = AlertStore::default();
        assert_eq!(s.rules.len(), builtin_rules().len());
        assert!(s.rule(RULE_HOST_KEY).is_some());
    }

    #[test]
    fn load_missing_file_seeds_builtins() {
        let dir = tmp_dir("missing");
        let s = AlertStore::load(&dir.join("nope.json"));
        assert_eq!(s.rules.len(), builtin_rules().len());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_repairs_missing_builtins_without_losing_user_rules() {
        let dir = tmp_dir("repair");
        let path = dir.join("alert_rules.json");
        // 用户自己删过 或 手写了一份只有自定义规则的文件
        let raw = r#"{"schema_version":1,"rules":[{"id":"my.rule","name":"我的规则","condition":{"kind":"host_key_mismatch"},"severity":"warning"}]}"#;
        std::fs::write(&path, raw).unwrap();

        let s = AlertStore::load(&path);
        assert!(s.rule("my.rule").is_some(), "用户规则不得丢");
        assert!(s.rule("builtin.load").is_some(), "内置规则必须补回");
        assert!(s.rule(RULE_HOST_KEY).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn broken_file_is_backed_up_and_input_is_truncated() {
        let dir = tmp_dir("broken");
        let path = dir.join("alert_rules.json");
        std::fs::write(&path, "{{{").unwrap();
        let s = AlertStore::load(&path);
        assert!(s.rule(RULE_HOST_KEY).is_some());
        // 坏文件被留底
        let backups: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains("broken"))
            .collect();
        assert!(!backups.is_empty(), "坏文件必须留底");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn builtin_rules_cannot_be_removed_but_user_rules_can() {
        let mut s = AlertStore::default();
        assert!(!s.remove_rule("builtin.load"), "内置规则不可删");
        assert!(!s.remove_rule(RULE_HOST_KEY));
        assert!(s.rule("builtin.load").is_some());

        let r = AlertRule {
            id: "user.1".into(),
            name: "自定义".into(),
            enabled: true,
            task_ids: Vec::new(),
            target_profiles: Vec::new(),
            condition: AlertCondition::LogAnomaly { min_hits: 1 },
            suppress_secs: 60,
            notify_recovery: true,
            severity: Severity::Info,
            channels: vec!["log".into()],
        };
        assert!(!s.upsert_rule(r.clone()), "新增应返回 false");
        assert!(s.upsert_rule(r), "覆盖应返回 true");
        assert!(s.remove_rule("user.1"));
        assert!(s.rule("user.1").is_none());
        assert!(!s.remove_rule("nope"));
    }

    #[test]
    fn firing_list_is_sorted_by_severity() {
        let mut s = AlertStore::default();
        let mk = |key: &str, sev: Severity, first: u64| DedupEntry {
            key: key.into(),
            task_id: "t1".into(),
            rule_id: "builtin.load".into(),
            profile_id: "p".into(),
            profile_name: "p".into(),
            fingerprint: "f".into(),
            last_message: String::new(),
            severity: sev,
            first_seen: first,
            last_notified: first,
            count: 1,
            firing: true,
        };
        s.dedup = vec![
            mk("a", Severity::Warning, 100),
            mk("b", Severity::Critical, 50),
            mk("c", Severity::Info, 200),
        ];
        let ids: Vec<&str> = s.firing().iter().map(|e| e.key.as_str()).collect();
        assert_eq!(ids, vec!["b", "a", "c"], "Critical 必须排最前");
    }

    #[test]
    fn prune_dedup_drops_entries_of_deleted_rules() {
        let mut s = AlertStore::default();
        s.dedup.push(DedupEntry {
            key: "k".into(),
            task_id: "t1".into(),
            rule_id: "gone.rule".into(),
            profile_id: "p".into(),
            profile_name: "p".into(),
            fingerprint: "f".into(),
            last_message: String::new(),
            severity: Severity::Warning,
            first_seen: 1,
            last_notified: 1,
            count: 1,
            firing: true,
        });
        s.prune_dedup();
        assert!(s.dedup.is_empty(), "规则没了，它的 firing 条目也必须清掉");
    }

    #[test]
    fn store_persists_dedup_across_restart() {
        let dir = tmp_dir("persist");
        let path = dir.join("alert_rules.json");
        let mut s = AlertStore::default();
        s.dedup.push(DedupEntry {
            key: "k".into(),
            task_id: "t1".into(),
            rule_id: "builtin.load".into(),
            profile_id: "p".into(),
            profile_name: "p".into(),
            fingerprint: "metric:mem>90".into(),
            last_message: "内存 94%".into(),
            severity: Severity::Warning,
            first_seen: 10,
            last_notified: 10,
            count: 3,
            firing: true,
        });
        s.save(&path).unwrap();

        let back = AlertStore::load(&path);
        assert_eq!(back.dedup.len(), 1, "抑制窗口必须跨重启保持");
        assert_eq!(back.dedup[0].last_notified, 10);
        assert_eq!(back.dedup[0].count, 3);
        assert!(back.dedup[0].firing);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
