//! 预警（告警）系统的数据类型。
//!
//! 三段拆分（见需求文档 §8.1）：
//! ```text
//! ① 检测 Finding      ② 判定 AlertRule + 去重/抑制      ③ 通知 NotifyChannel
//!    巡检/agent 产出        是否该发、发什么、多久不再发       邮件 / 日志 / 桌面
//! ```
//! 本模块只定义这三段之间流动的类型；求值逻辑在 [`super::engine`]，
//! 通道与派发在 [`super::notify`]。

use serde::{Deserialize, Serialize};

pub use crate::tasks::types::{Finding, FindingKind, LoadThresholds, Severity};

/// 告警规则存储。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertStore {
    #[serde(default = "default_schema")]
    pub schema_version: u32,
    #[serde(default)]
    pub rules: Vec<AlertRule>,
    /// 去重状态。**必须落盘**：否则每次重启都把抑制窗口清零，
    /// 持续中的故障会在重启后立刻再报一遍（正是要避免的告警轰炸）。
    #[serde(default)]
    pub dedup: Vec<DedupEntry>,
}

fn default_schema() -> u32 {
    ALERTS_SCHEMA_VERSION
}

/// 告警文件 schema 版本。
pub const ALERTS_SCHEMA_VERSION: u32 = 1;

/// 默认抑制窗口（秒）：同一规则同一目标 30 分钟内只报一次。
pub const DEFAULT_SUPPRESS_SECS: u64 = 1800;

/// 内置规则的 id 前缀。带此前缀的规则不允许删除（只能停用）。
pub const BUILTIN_PREFIX: &str = "builtin.";

/// 主机密钥不一致规则的 id（该规则连停用都禁止：它对应的是疑似中间人攻击）。
pub const RULE_HOST_KEY: &str = "builtin.host_key";

/// 主机密钥未知规则的 id。与 [`RULE_HOST_KEY`] 分开：
/// 「未知」是**运维待办**（去手动信任一次就好），「不一致」是**安全事故**。
/// 两者严重级与处置动作完全不同，混在一条规则里会让用户分不清该紧张还是该照做。
pub const RULE_HOST_KEY_UNKNOWN: &str = "builtin.host_key_unknown";

impl Default for AlertStore {
    fn default() -> Self {
        Self {
            schema_version: ALERTS_SCHEMA_VERSION,
            rules: builtin_rules(),
            dedup: Vec::new(),
        }
    }
}

/// 内置规则（首次启动的种子）。
///
/// 用户可以停用与改阈值，但不能删除 —— 「告警规则全被删光」是比「告警太吵」
/// 严重得多的事故形态，而停用已经足够表达「我知道这条规则，暂时不想看」。
pub fn builtin_rules() -> Vec<AlertRule> {
    use crate::i18n::ts;
    vec![
        builtin(
            "builtin.load",
            ts("负载超阈值".into(), "Load over threshold".into()),
            AlertCondition::MetricThreshold(LoadThresholds::default()),
        ),
        builtin(
            "builtin.log",
            ts("日志异常".into(), "Log anomalies".into()),
            AlertCondition::LogAnomaly { min_hits: 1 },
        ),
        builtin(
            "builtin.inspect_failed",
            ts("巡检失败".into(), "Inspection failed".into()),
            AlertCondition::InspectionFailed { min_failed: 1 },
        ),
        // 主机密钥不一致：Critical 且不抑制、不发恢复通知（恢复通知在这里没有意义 ——
        // 密钥对齐是人为动作，用户自己知道）
        AlertRule {
            suppress_secs: 0,
            notify_recovery: false,
            ..builtin(
                RULE_HOST_KEY,
                ts("主机密钥不一致".into(), "Host key mismatch".into()),
                AlertCondition::HostKeyMismatch,
            )
        },
        builtin(
            RULE_HOST_KEY_UNKNOWN,
            ts("主机密钥未知".into(), "Unknown host key".into()),
            AlertCondition::HostKeyUnknown,
        ),
    ]
}

/// 一条告警规则。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AlertRule {
    pub id: String,
    pub name: String,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// 适用的任务 id（空 = 全部任务）
    #[serde(default)]
    pub task_ids: Vec<String>,
    /// 适用的连接 id（空 = 全部目标）
    #[serde(default)]
    pub target_profiles: Vec<String>,
    pub condition: AlertCondition,
    /// 触发后多久内不再重复通知；`0` = 不去重（调试用）
    #[serde(default = "default_suppress")]
    pub suppress_secs: u64,
    /// 恢复时是否发「已恢复」通知
    #[serde(default = "yes")]
    pub notify_recovery: bool,
    /// 基准严重级：**条目**的实际级别由命中的 finding 定（`bump` 里取两者较高者）。
    /// 这个字段只决定「规则本身听起来有多严重」，以及未指定时的兜底。
    pub severity: Severity,
    /// 通知通道 id（默认 log；`email` 依赖 SMTP 配置）
    #[serde(default = "default_channels")]
    pub channels: Vec<String>,
}

/// 内置规则用的规则构造器：条件里蕴含的严重级直接作为基准级。
///
/// 不能把主机密钥不一致这类条件配成 `Warning`：「不一致」是疑似中间人攻击，
/// 配成 Warning 会让用户在通知标题里看到 `[WARNING]`，第一眼就判断错了轻重。
fn builtin(id: &str, name: String, condition: AlertCondition) -> AlertRule {
    AlertRule {
        id: id.into(),
        name,
        enabled: true,
        task_ids: Vec::new(),
        target_profiles: Vec::new(),
        severity: condition.baseline_severity(),
        condition,
        suppress_secs: DEFAULT_SUPPRESS_SECS,
        notify_recovery: true,
        channels: default_channels(),
    }
}

fn yes() -> bool {
    true
}

fn default_suppress() -> u64 {
    DEFAULT_SUPPRESS_SECS
}

fn default_channels() -> Vec<String> {
    vec!["log".into()]
}

impl AlertRule {
    /// 是否为内置规则（不可删除）。
    pub fn is_builtin(&self) -> bool {
        self.id.starts_with(BUILTIN_PREFIX)
    }

    /// 是否允许停用。主机密钥规则不允许。
    pub fn can_disable(&self) -> bool {
        self.id != RULE_HOST_KEY
    }

    /// 是否作用于给定任务（`task_ids` 为空 = 全部）。
    pub fn applies_to_task(&self, task_id: &str) -> bool {
        self.task_ids.is_empty() || self.task_ids.iter().any(|t| t == task_id)
    }

    /// 是否作用于给定连接（`target_profiles` 为空 = 全部）。
    pub fn applies_to_profile(&self, profile_id: &str) -> bool {
        self.target_profiles.is_empty() || self.target_profiles.iter().any(|p| p == profile_id)
    }

    /// 生效的抑制窗口：`Critical` 一律不抑制。
    ///
    /// 理由见需求文档 §8.4：主机密钥不一致这类事件，宁可重复提醒，
    /// 也不能因为「半小时内已经报过」而被咽掉。
    pub fn effective_suppress(&self) -> u64 {
        if self.severity >= Severity::Critical {
            0
        } else {
            self.suppress_secs
        }
    }
}

/// 告警条件：判断一条 [`Finding`] 是否命中本规则。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AlertCondition {
    /// 任一负载指标超阈值（阈值由本条件携带，判定在检测侧完成）
    MetricThreshold(LoadThresholds),
    /// 日志命中异常行达到 `min_hits` 条
    LogAnomaly { min_hits: u32 },
    /// 巡检失败的目标数达到 `min_failed`
    InspectionFailed { min_failed: u32 },
    /// 命令退出码落在给定集合里（空 = 任意非零）
    ExitCodeNonZero { codes: Vec<u32> },
    /// 主机密钥**不一致**（疑似中间人，Critical）
    HostKeyMismatch,
    /// 主机密钥**未知**（无人值守下拒绝连接，需先手动完成首次信任）
    HostKeyUnknown,
    /// 全部子条件都命中
    All(Vec<AlertCondition>),
    /// 任一子条件命中
    Any(Vec<AlertCondition>),
}

impl AlertCondition {
    /// 条件**蕴含**的基准严重级。
    ///
    /// 规则上的 `severity` 是基准值，但「主机密钥不一致」这种条件本身就该是
    /// Critical —— 让规则作者每次手填一遍是纯粹的出错机会。这里给出默认值，
    /// 用户仍可在 UI 上覆盖（覆盖的是规则字段，不改这个函数）。
    pub fn baseline_severity(&self) -> Severity {
        match self {
            Self::HostKeyMismatch => Severity::Critical,
            Self::All(subs) | Self::Any(subs) => subs
                .iter()
                .map(|c| c.baseline_severity())
                .max()
                .unwrap_or(Severity::Warning),
            _ => Severity::Warning,
        }
    }

    /// 条件是否「按目标逐条判定」。
    ///
    /// 逐目标条件（负载/日志/退出码/主机密钥）匹配的是**单条** finding；
    /// 聚合条件（巡检失败数）匹配的是**整批**结果。两者在 [`super::engine`]
    /// 里的求值时机不同，这个区分必须显式，不能靠猜。
    pub fn is_per_target(&self) -> bool {
        match self {
            Self::MetricThreshold(_)
            | Self::LogAnomaly { .. }
            | Self::ExitCodeNonZero { .. }
            | Self::HostKeyMismatch
            | Self::HostKeyUnknown => true,
            Self::InspectionFailed { .. } => false,
            Self::All(v) | Self::Any(v) => v.iter().any(|c| c.is_per_target()),
        }
    }
}

/// 去重状态的一条记录。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DedupEntry {
    /// `task_id` + `rule_id` + `profile_id` + `fingerprint`（见 [`Self::key_of`]）
    pub key: String,
    /// 产生它的任务 id（恢复检测按它划定「本次判定管得着谁」）
    #[serde(default)]
    pub task_id: String,
    pub rule_id: String,
    pub profile_id: String,
    pub profile_name: String,
    pub fingerprint: String,
    /// 最近一次命中的可读摘要（已脱敏）。通知正文用它而不是指纹：
    /// 用户看到「内存使用率 94%（9.4G/10G），阈值 90%」才有意义。
    #[serde(default)]
    pub last_message: String,
    pub severity: Severity,
    /// 首次命中时刻
    pub first_seen: u64,
    /// 最近一次**发出通知**的时刻
    pub last_notified: u64,
    /// 累计命中次数（汇总通知里报「持续 N 分钟，共命中 M 次」）
    pub count: u32,
    /// 是否处于「已通知、尚未恢复」态
    pub firing: bool,
}

impl DedupEntry {
    /// 去重键。
    ///
    /// **带任务维度**是这个键的关键：同一个目标被两个任务巡检（比如「早间巡检」
    /// 与「磁盘专项」都盯着一台机器）时，两条任务各自的抑制窗口与恢复语义必须
    /// 互不干扰。不带任务维度的键会让 A 任务的恢复把 B 任务尚在 firing 的状态
    /// 一起清掉 —— 那正是「告警忽有忽无」这类难查问题的来源。
    pub fn key_of(task_id: &str, rule_id: &str, profile_id: &str, fingerprint: &str) -> String {
        format!("{task_id}|{rule_id}|{profile_id}|{fingerprint}")
    }
}

/// 一条待发送的通知。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Notification {
    pub title: String,
    pub body_text: String,
    #[serde(default)]
    pub body_html: Option<String>,
    pub severity: Severity,
    #[serde(default)]
    pub task_name: Option<String>,
    pub occurred_at: u64,
    /// 幂等键：同一 key 的重复发送应被通道侧去重（邮件重试时用）
    pub dedup_key: String,
}

/// 通道派发结果：每个通道一条 [`ChannelResult`]。
///
/// 2026-10-12：由 `Vec<(String, Result<(), String>)>` 改为三态结构体。
/// 原形态只有 `Ok(())` / `Err(_)` 两值，于是「有意跳过（通道未配置）」被迫
/// 记成 `Ok(())`，与「真的投递成功」在审计日志里**逐字相同** —— 用户查
/// 「邮件到底发没发」时拿到假阳性。
pub type DispatchOutcome = Vec<crate::alerts::notify::ChannelResult>;

/// 一次判定循环的结果。
#[derive(Debug, Default)]
pub struct Evaluation {
    /// 需要发出的通知
    pub notifications: Vec<Notification>,
    /// 与 `notifications` **逐项对齐**的通道列表（`channels[i]` 是第 i 条通知该走的通道）。
    ///
    /// 通道必须与通知成对带出：通知渲染完只剩标题字符串，事后靠标题反查规则
    /// 会在规则改名后失联，退化成默认通道又会让用户配的通道静默失效。
    pub channels: Vec<Vec<String>>,
    /// 该攒进摘要窗口的通知 + 对应通道（v1.1 §8.6 digest）。
    ///
    /// 与 `notifications` 分开而不是混在一起：攒住的通知**现在不发**，
    /// 调用方要按任务级窗口收口，混在一起就需要调用方再判一次
    /// 「这条到底该不该等」—— 那正是把策略漏在引擎之外。
    pub digest: Vec<(Notification, Vec<String>)>,
    /// 命中的告警（无论是否因抑制未发通知，UI 都要能列出当前异常）
    pub firing: Vec<DedupEntry>,
    /// 本次判定中恢复的条目
    pub recovered: Vec<DedupEntry>,
}

impl Evaluation {
    /// 一条通知该走哪些通道（越界或缺失时回退到默认通道）。
    pub fn channels_of(&self, ix: usize) -> Vec<String> {
        self.channels
            .get(ix)
            .cloned()
            .unwrap_or_else(|| vec!["log".into()])
    }

    /// 把通知与通道配对（不做任何字符串解析）。
    pub fn paired(&self) -> Vec<(&Notification, Vec<String>)> {
        self.notifications
            .iter()
            .enumerate()
            .map(|(i, n)| (n, self.channels_of(i)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_rules_cover_the_four_documented_cases() {
        let rules = builtin_rules();
        let ids: Vec<&str> = rules.iter().map(|r| r.id.as_str()).collect();
        assert!(ids.contains(&"builtin.load"));
        assert!(ids.contains(&"builtin.log"));
        assert!(ids.contains(&"builtin.inspect_failed"));
        assert!(ids.contains(&RULE_HOST_KEY));
        assert!(rules.iter().all(|r| r.is_builtin()));
    }

    #[test]
    fn host_key_rule_cannot_be_disabled_and_never_suppresses() {
        let rules = builtin_rules();
        let hk = rules.iter().find(|r| r.id == RULE_HOST_KEY).unwrap();
        assert!(!hk.can_disable(), "主机密钥规则不得可停用");
        assert_eq!(hk.severity, Severity::Critical);
        assert_eq!(hk.effective_suppress(), 0, "Critical 不受抑制窗口限制");
        // 非主机密钥规则可以停用
        let other = rules.iter().find(|r| r.id == "builtin.load").unwrap();
        assert!(other.can_disable());
        assert_eq!(other.effective_suppress(), DEFAULT_SUPPRESS_SECS);
    }

    #[test]
    fn empty_filters_mean_apply_to_everything() {
        let r = AlertRule {
            id: "x".into(),
            name: "x".into(),
            enabled: true,
            task_ids: Vec::new(),
            target_profiles: Vec::new(),
            condition: AlertCondition::HostKeyMismatch,
            suppress_secs: 0,
            notify_recovery: true,
            severity: Severity::Warning,
            channels: vec!["log".into()],
        };
        assert!(r.applies_to_task("any"));
        assert!(r.applies_to_profile("any"));

        let mut scoped = r.clone();
        scoped.task_ids = vec!["t1".into()];
        scoped.target_profiles = vec!["p1".into()];
        assert!(scoped.applies_to_task("t1"));
        assert!(!scoped.applies_to_task("t2"));
        assert!(scoped.applies_to_profile("p1"));
        assert!(!scoped.applies_to_profile("p2"));
    }

    #[test]
    fn condition_per_target_classification() {
        assert!(AlertCondition::MetricThreshold(LoadThresholds::default()).is_per_target());
        assert!(AlertCondition::LogAnomaly { min_hits: 1 }.is_per_target());
        assert!(!AlertCondition::InspectionFailed { min_failed: 1 }.is_per_target());
        // 组合条件：含任一逐目标子条件即按逐目标处理
        assert!(AlertCondition::Any(vec![
            AlertCondition::InspectionFailed { min_failed: 1 },
            AlertCondition::HostKeyMismatch,
        ])
        .is_per_target());
        assert!(
            !AlertCondition::All(vec![AlertCondition::InspectionFailed { min_failed: 2 }])
                .is_per_target()
        );
    }

    #[test]
    fn dedup_key_is_stable_and_specific() {
        let a = DedupEntry::key_of("t1", "rule", "p1", "mem>90");
        let b = DedupEntry::key_of("t1", "rule", "p1", "mem>90");
        assert_eq!(a, b);
        assert_ne!(a, DedupEntry::key_of("t1", "rule", "p2", "mem>90"));
        assert_ne!(a, DedupEntry::key_of("t1", "rule", "p1", "disk>90"));
        assert_ne!(a, DedupEntry::key_of("t1", "rule2", "p1", "mem>90"));
        // 任务维度必须参与键：同一台机器被两个任务巡检时抑制状态互不干扰
        assert_ne!(
            a,
            DedupEntry::key_of("t2", "rule", "p1", "mem>90"),
            "去重键必须带任务维度"
        );
    }

    #[test]
    fn alert_store_roundtrips_and_backfills_defaults() {
        let store = AlertStore::default();
        let json = serde_json::to_string(&store).unwrap();
        let back: AlertStore = serde_json::from_str(&json).unwrap();
        assert_eq!(back.rules.len(), store.rules.len());
        assert_eq!(back.schema_version, ALERTS_SCHEMA_VERSION);

        // 旧文件（只有 rules，没有 dedup / schema_version）
        let legacy = r#"{"rules":[]}"#;
        let parsed: AlertStore = serde_json::from_str(legacy).unwrap();
        assert!(parsed.rules.is_empty());
        assert!(parsed.dedup.is_empty());
        assert_eq!(parsed.schema_version, ALERTS_SCHEMA_VERSION);

        // 规则缺字段 -> 补默认
        let partial = r#"{"rules":[{"id":"r","name":"n","condition":{"kind":"host_key_mismatch"},"severity":"warning"}]}"#;
        let p: AlertStore = serde_json::from_str(partial).unwrap();
        let r = &p.rules[0];
        assert!(r.enabled);
        assert_eq!(r.suppress_secs, DEFAULT_SUPPRESS_SECS);
        assert!(r.notify_recovery);
        assert_eq!(r.channels, vec!["log".to_string()]);
    }
}
