//! 告警判定与去重（纯逻辑，无 IO）。
//!
//! 每个函数都是可单测的纯函数：给定 finding 集合 + 规则 + 上一次的去重状态 +
//! 当前时刻，算出「该发哪些通知、当前在 firing 的有哪些、哪些已恢复」，并给出新的
//! 去重状态。**不碰文件、不发网络** —— 落盘与派发在调用方。

use super::notify::render_notification;
use super::types::*;
use std::collections::{HashMap, HashSet};

/// 判定上下文：一次运行产出的全部 finding。
#[derive(Debug, Clone)]
pub struct EvalInput<'a> {
    pub task_id: &'a str,
    pub task_name: &'a str,
    pub findings: &'a [Finding],
    pub now: u64,
}

/// 逐条判定 + 去重 + 恢复检测，返回本次要发的通知与新状态。
///
/// `dedup` 是**全量**状态（含其它任务/规则的条目）：本函数只更新与本次
/// `task_id` 相关的部分，其余原样保留 —— 否则跑一次 A 任务就会把 B 任务的
/// 抑制窗口清空。
///
/// `digest_policy` 是任务级的通知策略（v1.1 §8.6）：非 `Critical` 的通知
/// **不在本轮发出**，而是放进 `Evaluation::digest` 由调用方按窗口收口。
/// 传 `None` 即不合并（沿用逐条行为）。
pub fn evaluate(
    rules: &[AlertRule],
    dedup: &mut Vec<DedupEntry>,
    input: EvalInput<'_>,
    digest_policy: Option<&crate::tasks::NotifyPolicy>,
) -> Evaluation {
    let mut out = Evaluation::default();
    let mut immediate: Vec<(Notification, Vec<String>)> = Vec::new();

    // 判定开始**之前**就处于 firing 的条目。
    //
    // 恢复检测必须只看这个快照：本轮判定本身会新建条目（`firing = true`），
    // 若直接扫 `dedup`，那些「本轮刚命中、但规则把它归到了别的指纹」的条目
    // 会被当成「上次在响、这次没响」而立刻宣布恢复 —— 结果是同一条告警
    // 既报了一次告警又报了一次恢复。
    let was_firing: HashSet<String> = dedup
        .iter()
        .filter(|e| e.firing && e.task_id == input.task_id)
        .map(|e| e.key.clone())
        .collect();

    // 本次判定里「仍然命中」的 key 集合（用于识别恢复）
    let mut still_firing: HashSet<String> = HashSet::new();

    for rule in rules {
        if !rule.enabled || !rule.applies_to_task(input.task_id) {
            continue;
        }

        // 1) 聚合型条件：整批结果一起判
        if !rule.condition.is_per_target() {
            let fired = aggregate_fires(&rule.condition, input.findings);
            if fired {
                let fingerprint = aggregate_fingerprint(&rule.condition);
                let hits = aggregate_hits(&rule.condition, input.findings);
                let entry = bump(
                    dedup,
                    BumpSpec {
                        task_id: input.task_id,
                        rule,
                        profile_id: "",
                        profile_name: input.task_name,
                        fingerprint: &fingerprint,
                        message: &aggregate_message(&rule.condition, input.findings),
                        observed: aggregate_severity(&rule.condition, input.findings),
                        hits,
                        now: input.now,
                    },
                );
                still_firing.insert(entry.key.clone());
                out.firing.push(entry.clone());
                if let Some(n) = maybe_notify(rule, &entry, input.now, input.task_name) {
                    push_notification(&mut immediate, rule, n);
                }
            }
            continue;
        }

        // 2) 逐目标条件：把 finding 按 (profile, fingerprint) 归并后逐条判。
        //
        // 归并是必须的：同一次运行里同一台机器的同一规则可能产出多条 finding
        // （如两次采样都超阈），逐条报会变成 2 条通知，而用户只想知道「这台有问题」。
        let mut grouped: HashMap<(String, String), Vec<&Finding>> = HashMap::new();
        for f in input.findings {
            if f.rule_id != rule.id || !rule.applies_to_profile(&f.profile_id) {
                continue;
            }
            grouped
                .entry((f.profile_id.clone(), f.fingerprint.clone()))
                .or_default()
                .push(f);
        }

        for ((profile_id, fingerprint), group) in grouped {
            if !per_target_fires(&rule.condition, &group) {
                continue;
            }
            // 取最严重的一条作为这条告警的代表
            let worst = group.iter().max_by_key(|f| f.severity).expect("group 非空");
            let entry = bump(
                dedup,
                BumpSpec {
                    task_id: input.task_id,
                    rule,
                    profile_id: &profile_id,
                    profile_name: &worst.profile_name,
                    fingerprint: &fingerprint,
                    message: &worst.message,
                    observed: worst.severity,
                    hits: group.len() as u32,
                    now: input.now,
                },
            );
            still_firing.insert(entry.key.clone());
            out.firing.push(entry.clone());
            if let Some(n) = maybe_notify(rule, &entry, input.now, input.task_name) {
                push_notification(&mut immediate, rule, n);
            }
        }
    }

    // 3) 恢复检测：上一轮在 firing、这一轮没再命中 -> 清 firing 并（按需）发恢复通知。
    //
    // 两个过滤条件缺一不可：
    // * `was_firing`（判定前快照）—— 排除本轮新建的条目；
    // * `task_id` —— 排除别的任务的条目（同机被两个任务巡检时不得互相顶掉）。
    let mut recovered_keys: Vec<String> = Vec::new();
    for e in dedup.iter() {
        if !was_firing.contains(&e.key) {
            continue;
        }
        if still_firing.contains(&e.key) {
            continue;
        }
        let Some(rule) = rules
            .iter()
            .find(|r| r.id == e.rule_id && r.applies_to_task(input.task_id))
        else {
            continue;
        };
        recovered_keys.push(e.key.clone());
        let mut rec = e.clone();
        rec.firing = false;
        if rule.notify_recovery {
            let notif = render_notification(super::notify::NotificationDraft {
                rule,
                severity: rec.severity,
                title: &format!(
                    "{} {}",
                    crate::i18n::ts("已恢复：".into(), "Recovered: ".into()),
                    rec.profile_name
                ),
                body: &recovery_body(&rec, input.now),
                now: input.now,
                task_name: Some(input.task_name),
                dedup_key: &format!("{}|recovered", rec.key),
                target: Some(&rec.profile_name),
            });
            push_notification(&mut immediate, rule, notif);
        }
        out.recovered.push(rec);
    }

    // 已恢复的条目从状态里移除：保留它们只会让 dedup 越积越大，
    // 而「上次是什么时候响的」已经写进了运行记录与告警日志。
    if !recovered_keys.is_empty() {
        let drop: HashSet<&str> = recovered_keys.iter().map(|s| s.as_str()).collect();
        dedup.retain(|e| !drop.contains(e.key.as_str()));
    }

    // 分流（唯一一处）：Critical 与「未配置窗口」直发，其余攒进 digest。
    //
    // 放在最后统一做，而不是在每条 push 处判断：判断散开之后，新增一条通知路径时
    // 极容易漏掉 digest，而漏掉的表现是「大部分告警被合并了、偏偏这一类没有」——
    // 很难被发现。
    match digest_policy {
        Some(policy) => {
            let (now, held) = split_with_channels(policy, immediate);
            out.notifications = now.iter().map(|(n, _)| n.clone()).collect();
            out.channels = now.into_iter().map(|(_, c)| c).collect();
            out.digest = held;
        }
        None => {
            out.notifications = immediate.iter().map(|(n, _)| n.clone()).collect();
            out.channels = immediate.into_iter().map(|(_, c)| c).collect();
        }
    }

    out
}

/// 「通知 + 它该走的通道」。两者必须成对流动，不能拆开（见 `push_notification`）。
type NotificationWithChannels = (Notification, Vec<String>);

/// 按 digest 策略分流「通知 + 通道」，保持两者逐项对齐。
///
/// 单独抽出来的理由：`digest::split` 只处理通知本体，而派发需要通道。
/// 若在调用处手工对齐，`notifications[i]` 与 `channels[i]` 很容易错位 ——
/// 而错位的表现是「告警发到了错误的通道」，排查成本极高。
fn split_with_channels(
    policy: &crate::tasks::NotifyPolicy,
    items: Vec<NotificationWithChannels>,
) -> (Vec<NotificationWithChannels>, Vec<NotificationWithChannels>) {
    if policy.digest_window_secs == 0 {
        return (items, Vec::new());
    }
    let mut immediate = Vec::new();
    let mut held = Vec::new();
    for (n, ch) in items {
        if n.severity >= super::types::Severity::Critical {
            immediate.push((n, ch));
        } else {
            held.push((n, ch));
        }
    }
    (immediate, held)
}

/// 收下一条通知（暂存），并把「该走哪些通道」一并记账。
///
/// 这是唯一同时知道「哪条规则触发的」与「该走哪些通道」的位置：
/// 通知渲染完只剩标题字符串，事后再反查规则一定丢信息。
///
/// 先收进 `immediate` 而不是直接写 `out`：判定结束后统一按 digest 策略分流
/// （该直发的进 `notifications`，该攒的进 `digest`）。分流点只有一处，
/// 就不会出现「某条路径忘了走 digest」。
fn push_notification(
    immediate: &mut Vec<(Notification, Vec<String>)>,
    rule: &AlertRule,
    n: Notification,
) {
    immediate.push((n, rule.channels.clone()));
}

/// [`bump`] 的入参：把「谁 + 什么」打成一个结构体。
///
/// 不这么做的话这个函数有 9 个参数，而其中连续三个都是 `&str`（规则名/目标名/
/// 指纹）—— 调用点一旦写错顺序，编译器**不会**报错，只会生成一个键值错乱的
/// 去重条目，表现为「告警莫名其妙不触发」。命名参数是这里唯一有效的护栏。
struct BumpSpec<'a> {
    task_id: &'a str,
    rule: &'a AlertRule,
    profile_id: &'a str,
    profile_name: &'a str,
    fingerprint: &'a str,
    message: &'a str,
    /// 命中 finding 的**实际**严重级（与规则基准级取较大者）
    observed: Severity,
    /// 本轮贡献的命中数
    hits: u32,
    now: u64,
}

/// 命中/更新一条去重记录，返回更新后的条目。
///
/// `hits` 是**本次**这一轮贡献的命中数（同一目标同一指纹可能由多条 finding
/// 归并而来）。累加而不是每次 +1：否则「一台机器在同一次巡检里命中 2 条」
/// 与「连续两轮各命中 1 条」在汇总通知里长得一模一样，而两者的含义完全不同。
///
/// 条目级别取**规则基准级与 finding 实际级的较大者**。规则只是「这个条件下要报警」
/// 的声明，实际轻重由检测侧定；只取规则级的话，一条 `Warning` 基准的规则会把
/// Critical 的 finding 降级成 Warning，而下游（digest 窗口、通知前缀、邮件主题）
/// 全按条目级别分流 —— 表现为「内存 94% 的严重告警被当作普通告警攒进摘要」。
///
/// 首次命中时 `firing` 置真且 `last_notified = 0` —— 这样 [`maybe_notify`]
/// 的第一条分支必然成立，无需另设「是否首次」的标记。
fn bump(dedup: &mut Vec<DedupEntry>, spec: BumpSpec<'_>) -> DedupEntry {
    let BumpSpec {
        task_id,
        rule,
        profile_id,
        profile_name,
        fingerprint,
        message,
        observed,
        hits,
        now,
    } = spec;
    let severity = rule.severity.max(observed);
    let key = DedupEntry::key_of(task_id, &rule.id, profile_id, fingerprint);
    if let Some(e) = dedup.iter_mut().find(|e| e.key == key) {
        e.count = e.count.saturating_add(hits.max(1));
        e.severity = e.severity.max(severity);
        e.profile_name = profile_name.to_string();
        e.last_message = message.to_string();
        // 已经处于 firing 态的条目，这里**不**刷新 last_notified：
        // 否则每跑一次巡检都会把抑制窗口往后推，永远等不到汇总通知。
        if !e.firing {
            e.firing = true;
            e.first_seen = now;
            e.last_notified = 0;
        }
        return e.clone();
    }
    let entry = DedupEntry {
        key,
        task_id: task_id.to_string(),
        rule_id: rule.id.clone(),
        profile_id: profile_id.to_string(),
        profile_name: profile_name.to_string(),
        fingerprint: fingerprint.to_string(),
        last_message: message.to_string(),
        severity,
        first_seen: now,
        last_notified: 0,
        count: hits.max(1),
        firing: true,
    };
    dedup.push(entry.clone());
    entry
}

/// 决定这条 firing 记录此刻是否要发通知。
///
/// 三种情况发：
/// 1. 首次（`last_notified == 0`）；
/// 2. 距上次通知已超过抑制窗口（此时发的是**汇总**，带上持续时长与命中次数）；
/// 3. `Critical` 条目（抑制窗口恒为 0，但 `now > last_notified` 才发，
///    避免同一秒内重复判定产生两条）。
///
/// **抑制窗口按 entry 的实际严重级算**，不是按规则基准级：一条 Warning 规则
/// 完全可能产出 Critical 的条目（规则只是「这个条件下要报警」的声明，
/// 实际轻重由检测侧定），这时必须享受 Critical 的免抑制待遇。
///
/// 注意：本函数**只读**，真正把 `last_notified` 推进由 [`apply_notification`] 做。
/// 分开的原因是一次判定可能产生多条通知，而「已经决定要发」与「已经记在状态里」
/// 必须是同一处收口，否则并发上的重复发送无从追查。
fn maybe_notify(
    rule: &AlertRule,
    entry: &DedupEntry,
    now: u64,
    task_name: &str,
) -> Option<Notification> {
    let suppress = if entry.severity >= Severity::Critical {
        0
    } else {
        rule.suppress_secs
    };
    let due = if entry.last_notified == 0 {
        true
    } else {
        now.saturating_sub(entry.last_notified) >= suppress && suppress > 0
            // suppress == 0（Critical）：只在确实往前走了时间时再发
            || (suppress == 0 && now > entry.last_notified)
    };
    if !due {
        return None;
    }

    let first = entry.last_notified == 0;
    let title = if first {
        format!("{} · {}", rule.name, entry.profile_name)
    } else {
        format!(
            "{} · {}（{}）",
            rule.name,
            entry.profile_name,
            crate::i18n::ts("持续中".into(), "ongoing".into())
        )
    };
    let body = if first {
        // 首次通知给可读摘要（fingerprint 只是去重用的内部键）
        if entry.last_message.is_empty() {
            entry.fingerprint.clone()
        } else {
            entry.last_message.clone()
        }
    } else {
        let mins = now.saturating_sub(entry.first_seen) / 60;
        format!(
            "{}；{}",
            entry.last_message,
            crate::i18n::ts(
                format!("已持续 {mins} 分钟，共命中 {} 次", entry.count),
                format!("ongoing for {mins} min, {} hits", entry.count),
            )
        )
    };
    Some(render_notification(super::notify::NotificationDraft {
        rule,
        severity: entry.severity,
        title: &title,
        body: &body,
        now,
        task_name: Some(task_name),
        dedup_key: &entry.key,
        target: Some(&entry.profile_name),
    }))
}

/// 把「已发出通知」这一事实写回去重状态（推进 `last_notified`）。
///
/// **攒进 digest 的那批也要一并推进**：它们虽然还没送达用户，但「要不要通知」
/// 这个决策已经做完了。不推进的话，同一轮里既发了直发通知、又把攒住的那条
/// 留在「未通知」态，下一次判定会再把它当成「首次命中」——表现为同一条告警
/// 一会儿在摘要里、一会儿又单独冒出来。
pub fn apply_notification(
    dedup: &mut [DedupEntry],
    notifications: &[Notification],
    digested: &[Notification],
    now: u64,
) {
    let keys: HashSet<&str> = notifications
        .iter()
        .chain(digested.iter())
        .map(|n| n.dedup_key.as_str())
        .collect();
    for e in dedup.iter_mut() {
        if keys.contains(e.key.as_str()) {
            e.last_notified = now;
        }
    }
}

/// 聚合型条件命中时，参与判定的 finding 里最严重的那一级。
///
/// 与逐目标路径的 `worst.severity` 对称：规则基准级只是下限，
/// 实际轻重取检测侧给的级别。
fn aggregate_severity(cond: &AlertCondition, findings: &[Finding]) -> Severity {
    let kinds = condition_kinds(cond);
    findings
        .iter()
        .filter(|f| kinds.contains(&f.kind))
        .map(|f| f.severity)
        .max()
        .unwrap_or(Severity::Warning)
}

/// 条件关心的 finding 类别（聚合取严重级用）。
fn condition_kinds(cond: &AlertCondition) -> Vec<FindingKind> {
    match cond {
        AlertCondition::InspectionFailed { .. } => vec![FindingKind::InspectionFailed],
        AlertCondition::All(subs) | AlertCondition::Any(subs) => {
            subs.iter().flat_map(condition_kinds).collect()
        }
        _ => Vec::new(),
    }
}

/// 聚合型条件是否命中。
fn aggregate_fires(cond: &AlertCondition, findings: &[Finding]) -> bool {
    match cond {
        AlertCondition::InspectionFailed { min_failed } => {
            let n = findings
                .iter()
                .filter(|f| f.kind == FindingKind::InspectionFailed)
                .count();
            n >= (*min_failed).max(1) as usize
        }
        AlertCondition::All(subs) => subs.iter().all(|c| aggregate_fires(c, findings)),
        AlertCondition::Any(subs) => subs.iter().any(|c| aggregate_fires(c, findings)),
        // 逐目标条件不参与聚合判定
        _ => false,
    }
}

/// 聚合型条件在本轮贡献的命中数。
fn aggregate_hits(cond: &AlertCondition, findings: &[Finding]) -> u32 {
    match cond {
        AlertCondition::InspectionFailed { .. } => findings
            .iter()
            .filter(|f| f.kind == FindingKind::InspectionFailed)
            .count() as u32,
        AlertCondition::All(subs) | AlertCondition::Any(subs) => subs
            .iter()
            .map(|c| aggregate_hits(c, findings))
            .max()
            .unwrap_or(1),
        _ => 1,
    }
}

/// 聚合型条件的可读摘要（列出失败的目标名）。
fn aggregate_message(cond: &AlertCondition, findings: &[Finding]) -> String {
    let failed: Vec<&str> = findings
        .iter()
        .filter(|f| {
            (f.kind == FindingKind::InspectionFailed)
                && matches!(cond, AlertCondition::InspectionFailed { .. })
        })
        .map(|f| f.profile_name.as_str())
        .collect();
    if failed.is_empty() {
        return aggregate_fingerprint(cond);
    }
    format!(
        "{}: {}",
        crate::i18n::ts("巡检失败".into(), "inspection failed".into()),
        failed.join(", ")
    )
}

/// 聚合型条件的稳定指纹（用于去重键）。
fn aggregate_fingerprint(cond: &AlertCondition) -> String {
    match cond {
        AlertCondition::InspectionFailed { min_failed } => {
            format!("inspect_failed>={min_failed}")
        }
        AlertCondition::All(subs) => format!(
            "all({})",
            subs.iter()
                .map(aggregate_fingerprint)
                .collect::<Vec<_>>()
                .join(",")
        ),
        AlertCondition::Any(subs) => format!(
            "any({})",
            subs.iter()
                .map(aggregate_fingerprint)
                .collect::<Vec<_>>()
                .join(",")
        ),
        other => format!("{other:?}"),
    }
}

/// 逐目标条件是否命中（`group` 是同一目标同一指纹下的全部 finding）。
fn per_target_fires(cond: &AlertCondition, group: &[&Finding]) -> bool {
    match cond {
        AlertCondition::MetricThreshold(_) => group
            .iter()
            .any(|f| f.kind == FindingKind::Anomaly && f.fingerprint.starts_with("metric:")),
        AlertCondition::LogAnomaly { min_hits } => group.iter().any(|f| {
            f.kind == FindingKind::Anomaly
                && f.fingerprint.starts_with("log:")
                && hits_of(f) >= (*min_hits).max(1)
        }),
        AlertCondition::ExitCodeNonZero { codes } => group.iter().any(|f| {
            f.kind == FindingKind::InspectionFailed
                && match parse_exit_code(&f.fingerprint) {
                    Some(c) if codes.is_empty() => c != 0,
                    Some(c) => codes.contains(&c),
                    None => false,
                }
        }),
        AlertCondition::HostKeyMismatch => group
            .iter()
            .any(|f| f.kind == FindingKind::InspectionFailed && f.fingerprint == HOST_KEY_FP),
        AlertCondition::HostKeyUnknown => group.iter().any(|f| {
            f.kind == FindingKind::InspectionFailed && f.fingerprint == HOST_KEY_UNKNOWN_FP
        }),
        AlertCondition::All(subs) => subs.iter().all(|c| per_target_fires(c, group)),
        AlertCondition::Any(subs) => subs.iter().any(|c| per_target_fires(c, group)),
        // 聚合型条件不参与逐目标判定
        AlertCondition::InspectionFailed { .. } => false,
    }
}

/// 主机密钥不一致的指纹（疑似中间人：任何策略下都拒绝）。
pub const HOST_KEY_FP: &str = "hostkey:mismatch";

/// 主机密钥未知的指纹（无人值守下拒绝，需先手动完成首次信任）。
pub const HOST_KEY_UNKNOWN_FP: &str = "hostkey:unknown";

/// 日志 finding 的指纹形如 `log:error#23`（`#` 后是命中行数）；解析出命中数。
pub fn log_fingerprint(level: &str, hits: u32) -> String {
    format!("log:{level}#{hits}")
}

/// 指标 finding 的指纹形如 `metric:mem>90`；`metric:` 开头即视为指标类。
pub fn metric_fingerprint(metric: &str, threshold: f32) -> String {
    format!("metric:{metric}>{threshold}")
}

/// 退出码 finding 的指纹形如 `exit:127`。
pub fn exit_fingerprint(code: u32) -> String {
    format!("exit:{code}")
}

fn hits_of(f: &Finding) -> u32 {
    f.fingerprint
        .rsplit_once('#')
        .and_then(|(_, n)| n.parse().ok())
        .unwrap_or(0)
}

fn parse_exit_code(fp: &str) -> Option<u32> {
    fp.strip_prefix("exit:")?.parse().ok()
}

fn recovery_body(entry: &DedupEntry, now: u64) -> String {
    let mins = now.saturating_sub(entry.first_seen) / 60;
    crate::i18n::ts(
        format!(
            "{}（{}）已恢复正常；本次异常持续 {mins} 分钟，共命中 {} 次",
            entry.profile_name, entry.fingerprint, entry.count
        ),
        format!(
            "{} ({}) is back to normal; the issue lasted {mins} min with {} hits",
            entry.profile_name, entry.fingerprint, entry.count
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finding(rule: &str, profile: &str, fp: &str, kind: FindingKind) -> Finding {
        Finding {
            kind,
            profile_id: profile.into(),
            profile_name: profile.into(),
            rule_id: rule.into(),
            fingerprint: fp.into(),
            severity: Severity::Warning,
            message: fp.into(),
            evidence: None,
        }
    }

    fn rule(id: &str, cond: AlertCondition) -> AlertRule {
        AlertRule {
            id: id.into(),
            name: id.into(),
            enabled: true,
            task_ids: Vec::new(),
            target_profiles: Vec::new(),
            // 与生产代码同一口径：基准级取自条件本身
            //（否则「主机密钥不一致」的用例会因为测试构造器写死 Warning 而永远红）
            severity: cond.baseline_severity(),
            condition: cond,
            suppress_secs: 1800,
            notify_recovery: true,
            channels: vec!["log".into()],
        }
    }

    fn input<'a>(findings: &'a [Finding], now: u64) -> EvalInput<'a> {
        EvalInput {
            task_id: "t1",
            task_name: "早间巡检",
            findings,
            now,
        }
    }

    #[test]
    fn first_hit_notifies_once() {
        let mut dedup = Vec::new();
        let rules = vec![rule(
            "builtin.load",
            AlertCondition::MetricThreshold(LoadThresholds::default()),
        )];
        let f = vec![finding(
            "builtin.load",
            "p1",
            &metric_fingerprint("mem", 90.0),
            FindingKind::Anomaly,
        )];
        let ev = evaluate(&rules, &mut dedup, input(&f, 1000), None);
        assert_eq!(ev.notifications.len(), 1, "首次命中必须通知");
        assert_eq!(ev.firing.len(), 1);
        assert!(dedup[0].firing);

        // 记录已通知事实
        apply_notification(&mut dedup, &ev.notifications, &[], 1000);
        assert_eq!(dedup[0].last_notified, 1000);
    }

    #[test]
    fn repeated_hits_are_suppressed_within_window() {
        let mut dedup = Vec::new();
        let rules = vec![rule(
            "builtin.load",
            AlertCondition::MetricThreshold(LoadThresholds::default()),
        )];
        let f = vec![finding(
            "builtin.load",
            "p1",
            &metric_fingerprint("mem", 90.0),
            FindingKind::Anomaly,
        )];

        let ev = evaluate(&rules, &mut dedup, input(&f, 1000), None);
        apply_notification(&mut dedup, &ev.notifications, &[], 1000);

        // 窗口内再判 3 次：不通知，但仍在 firing 且计数累加
        for t in [1100, 1200, 1300] {
            let ev = evaluate(&rules, &mut dedup, input(&f, t), None);
            assert!(
                ev.notifications.is_empty(),
                "抑制窗口内不得重复通知（t={t}）"
            );
            assert_eq!(ev.firing.len(), 1, "firing 状态要保留，UI 才能列出当前异常");
        }
        assert_eq!(dedup[0].count, 4, "命中次数应累加到 4");
        assert_eq!(dedup[0].last_notified, 1000, "抑制窗口不得被反复命中推后");
    }

    #[test]
    fn digest_fires_after_window_expires() {
        let mut dedup = Vec::new();
        let rules = vec![rule(
            "builtin.load",
            AlertCondition::MetricThreshold(LoadThresholds::default()),
        )];
        let f = vec![finding(
            "builtin.load",
            "p1",
            &metric_fingerprint("mem", 90.0),
            FindingKind::Anomaly,
        )];
        let ev = evaluate(&rules, &mut dedup, input(&f, 0), None);
        apply_notification(&mut dedup, &ev.notifications, &[], 0);

        // 超过抑制窗口（1800s）：发汇总，正文带上持续时间与次数
        let ev = evaluate(&rules, &mut dedup, input(&f, 1900), None);
        assert_eq!(ev.notifications.len(), 1, "窗口结束必须发一条汇总");
        assert!(
            ev.notifications[0].body_text.contains("31")
                || ev.notifications[0].body_text.contains("持续"),
            "汇总正文应说明持续时间：{}",
            ev.notifications[0].body_text
        );
    }

    #[test]
    fn critical_bypasses_suppression_window() {
        let mut rules = vec![rule(
            "builtin.load",
            AlertCondition::MetricThreshold(LoadThresholds::default()),
        )];
        rules[0].severity = Severity::Critical;
        rules[0].suppress_secs = 3600; // 显式给了长窗口
        let mut dedup = Vec::new();
        let f = vec![finding(
            "builtin.load",
            "p1",
            &metric_fingerprint("mem", 90.0),
            FindingKind::Anomaly,
        )];
        let ev = evaluate(&rules, &mut dedup, input(&f, 100), None);
        apply_notification(&mut dedup, &ev.notifications, &[], 100);

        // 时间往前走了就该再报，哪怕远未到 suppress_secs
        let ev = evaluate(&rules, &mut dedup, input(&f, 200), None);
        assert_eq!(ev.notifications.len(), 1, "Critical 不受抑制窗口约束");
    }

    #[test]
    fn same_timestamp_does_not_double_notify() {
        let mut rules = vec![rule(
            "builtin.load",
            AlertCondition::MetricThreshold(LoadThresholds::default()),
        )];
        rules[0].severity = Severity::Critical;
        let mut dedup = Vec::new();
        let f = vec![finding(
            "builtin.load",
            "p1",
            &metric_fingerprint("mem", 90.0),
            FindingKind::Anomaly,
        )];
        let ev = evaluate(&rules, &mut dedup, input(&f, 500), None);
        apply_notification(&mut dedup, &ev.notifications, &[], 500);
        // 同一秒再判：不得再发
        let ev = evaluate(&rules, &mut dedup, input(&f, 500), None);
        assert!(ev.notifications.is_empty());
    }

    #[test]
    fn recovery_notifies_and_clears_state() {
        let mut dedup = Vec::new();
        let rules = vec![rule(
            "builtin.load",
            AlertCondition::MetricThreshold(LoadThresholds::default()),
        )];
        let f = vec![finding(
            "builtin.load",
            "p1",
            &metric_fingerprint("mem", 90.0),
            FindingKind::Anomaly,
        )];
        let ev = evaluate(&rules, &mut dedup, input(&f, 1000), None);
        apply_notification(&mut dedup, &ev.notifications, &[], 1000);
        assert_eq!(dedup.len(), 1);

        // 下一轮没有 finding -> 恢复
        let ev = evaluate(&rules, &mut dedup, input(&[], 2000), None);
        assert_eq!(ev.recovered.len(), 1);
        assert_eq!(ev.notifications.len(), 1, "应发恢复通知");
        assert!(ev.notifications[0].dedup_key.ends_with("|recovered"));
        assert!(dedup.is_empty(), "恢复后条目应从状态里移除");
    }

    #[test]
    fn recovery_silent_when_disabled() {
        let mut rules = vec![rule(
            "builtin.load",
            AlertCondition::MetricThreshold(LoadThresholds::default()),
        )];
        rules[0].notify_recovery = false;
        let mut dedup = Vec::new();
        let f = vec![finding(
            "builtin.load",
            "p1",
            &metric_fingerprint("mem", 90.0),
            FindingKind::Anomaly,
        )];
        let ev = evaluate(&rules, &mut dedup, input(&f, 1000), None);
        apply_notification(&mut dedup, &ev.notifications, &[], 1000);

        let ev = evaluate(&rules, &mut dedup, input(&[], 2000), None);
        assert_eq!(ev.recovered.len(), 1);
        assert!(ev.notifications.is_empty(), "关闭恢复通知后不得发");
    }

    #[test]
    fn different_fingerprint_is_a_new_alert() {
        let mut dedup = Vec::new();
        let rules = vec![rule(
            "builtin.load",
            AlertCondition::MetricThreshold(LoadThresholds::default()),
        )];
        let f1 = vec![finding(
            "builtin.load",
            "p1",
            &metric_fingerprint("mem", 90.0),
            FindingKind::Anomaly,
        )];
        let ev = evaluate(&rules, &mut dedup, input(&f1, 1000), None);
        apply_notification(&mut dedup, &ev.notifications, &[], 1000);

        // 指纹变了（内存 -> 磁盘）：新指纹必须享有自己的抑制窗口，
        // 不能被**另一条**已 firing 告警的窗口压住。
        //
        // 注意这里同时给出了内存（持续中）与磁盘（新）两条：这才对应真实的巡检
        // 一轮里「老问题还在、新问题又来了」，也正是「新告警被旧窗口吞掉」这个
        // bug 会显露的场景。
        let f2 = vec![
            finding(
                "builtin.load",
                "p1",
                &metric_fingerprint("mem", 90.0),
                FindingKind::Anomaly,
            ),
            finding(
                "builtin.load",
                "p1",
                &metric_fingerprint("disk", 90.0),
                FindingKind::Anomaly,
            ),
        ];
        let ev = evaluate(&rules, &mut dedup, input(&f2, 1100), None);
        assert_eq!(ev.notifications.len(), 1, "只应通知新的那条（磁盘）");
        assert!(
            ev.notifications[0].body_text.contains("磁盘")
                || ev.notifications[0].body_text.contains("disk"),
            "新告警的内容应是磁盘指标：{}",
            ev.notifications[0].body_text
        );
        assert!(ev.recovered.is_empty(), "内存那条仍在命中，不该判恢复");
        assert_eq!(dedup.len(), 2, "两条指纹各自一条状态");
    }

    #[test]
    fn same_target_repeated_findings_collapse_into_one_notification() {
        let mut dedup = Vec::new();
        let rules = vec![rule(
            "builtin.load",
            AlertCondition::MetricThreshold(LoadThresholds::default()),
        )];
        // 同一台机器同一指纹两条（两次采样都超阈）
        let f = vec![
            finding(
                "builtin.load",
                "p1",
                &metric_fingerprint("mem", 90.0),
                FindingKind::Anomaly,
            ),
            finding(
                "builtin.load",
                "p1",
                &metric_fingerprint("mem", 90.0),
                FindingKind::Anomaly,
            ),
        ];
        let ev = evaluate(&rules, &mut dedup, input(&f, 1000), None);
        assert_eq!(ev.notifications.len(), 1, "同一目标同一原因只报一条");
        assert_eq!(dedup.len(), 1);
        assert_eq!(dedup[0].count, 2, "两条命中共计为 2");
    }

    #[test]
    fn multiple_targets_produce_one_notification_each() {
        let mut dedup = Vec::new();
        let rules = vec![rule(
            "builtin.load",
            AlertCondition::MetricThreshold(LoadThresholds::default()),
        )];
        let f = vec![
            finding(
                "builtin.load",
                "p1",
                &metric_fingerprint("mem", 90.0),
                FindingKind::Anomaly,
            ),
            finding(
                "builtin.load",
                "p2",
                &metric_fingerprint("mem", 90.0),
                FindingKind::Anomaly,
            ),
        ];
        let ev = evaluate(&rules, &mut dedup, input(&f, 1000), None);
        assert_eq!(ev.notifications.len(), 2, "两台机器各自一条");
        assert_eq!(dedup.len(), 2);
    }

    #[test]
    fn log_condition_respects_min_hits() {
        let mut dedup = Vec::new();
        let rules = vec![rule(
            "builtin.log",
            AlertCondition::LogAnomaly { min_hits: 5 },
        )];
        // 只命中 3 条：不触发
        let f = vec![finding(
            "builtin.log",
            "p1",
            &log_fingerprint("error", 3),
            FindingKind::Anomaly,
        )];
        let ev = evaluate(&rules, &mut dedup, input(&f, 1000), None);
        assert!(ev.notifications.is_empty(), "未达 min_hits 不该报");

        // 命中 23 条：触发
        let f = vec![finding(
            "builtin.log",
            "p1",
            &log_fingerprint("error", 23),
            FindingKind::Anomaly,
        )];
        let ev = evaluate(&rules, &mut dedup, input(&f, 1000), None);
        assert_eq!(ev.notifications.len(), 1);
    }

    #[test]
    fn inspection_failed_condition_is_aggregate() {
        let mut dedup = Vec::new();
        let rules = vec![rule(
            "builtin.inspect_failed",
            AlertCondition::InspectionFailed { min_failed: 2 },
        )];
        // 只有 1 台失败：不触发
        let one = vec![finding(
            "builtin.inspect_failed",
            "p1",
            "connect:timeout",
            FindingKind::InspectionFailed,
        )];
        let ev = evaluate(&rules, &mut dedup, input(&one, 1000), None);
        assert!(ev.notifications.is_empty());

        // 2 台失败：触发，且只产生 1 条（聚合型）
        let two = vec![
            finding(
                "builtin.inspect_failed",
                "p1",
                "connect:timeout",
                FindingKind::InspectionFailed,
            ),
            finding(
                "builtin.inspect_failed",
                "p2",
                "connect:refused",
                FindingKind::InspectionFailed,
            ),
        ];
        let ev = evaluate(&rules, &mut dedup, input(&two, 1000), None);
        assert_eq!(ev.notifications.len(), 1, "聚合条件只发一条");
        assert_eq!(ev.firing.len(), 1);
    }

    #[test]
    fn host_key_conditions_match_only_their_own_fingerprints() {
        // 「不一致」与「未知」是两条规则、两个指纹，绝不能互相命中：
        // 前者是安全事故（Critical），后者是运维待办（Warning）。
        //
        // 每个场景各用一份全新的去重状态：一次判定里「本轮没命中」= 恢复，
        // 共用状态会让后一个场景顺带把前一个场景的告警判成恢复，
        // 那是**正确行为**但会干扰本用例想验证的匹配关系。
        let rules = vec![
            rule(RULE_HOST_KEY, AlertCondition::HostKeyMismatch),
            rule("builtin.host_key_unknown", AlertCondition::HostKeyUnknown),
        ];

        // 普通连接失败：两条都不命中
        let mut dedup = Vec::new();
        let other = vec![finding(
            "builtin.inspect_failed",
            "p1",
            "connect:timeout",
            FindingKind::InspectionFailed,
        )];
        let ev = evaluate(&rules, &mut dedup, input(&other, 1000), None);
        assert!(
            ev.notifications.is_empty(),
            "普通连接失败不该被当成主机密钥问题"
        );

        // 未知主机密钥 -> 只命中「未知」那条
        let mut dedup = Vec::new();
        let unknown = vec![finding(
            "builtin.host_key_unknown",
            "p1",
            HOST_KEY_UNKNOWN_FP,
            FindingKind::InspectionFailed,
        )];
        let ev = evaluate(&rules, &mut dedup, input(&unknown, 1000), None);
        assert_eq!(ev.notifications.len(), 1);
        assert_eq!(ev.notifications[0].severity, Severity::Warning);

        // 密钥不一致 -> 只命中「不一致」那条，且必须是 Critical
        let mut dedup = Vec::new();
        let mismatch = vec![finding(
            RULE_HOST_KEY,
            "p2",
            HOST_KEY_FP,
            FindingKind::InspectionFailed,
        )];
        let ev = evaluate(&rules, &mut dedup, input(&mismatch, 1000), None);
        assert_eq!(ev.notifications.len(), 1);
        assert_eq!(
            ev.notifications[0].severity,
            Severity::Critical,
            "主机密钥不一致必须按 Critical 通知"
        );
    }

    /// 恢复通知与告警通知不会在同一轮混发：
    /// 本轮新命中的条目不得被同轮的恢复扫描「顺手宣布恢复」。
    #[test]
    fn fresh_hit_is_not_declared_recovered_in_the_same_pass() {
        let rules = vec![rule(
            "builtin.load",
            AlertCondition::MetricThreshold(LoadThresholds::default()),
        )];
        let mut dedup = Vec::new();
        // 第一轮：只有内存告警
        let mem = vec![finding(
            "builtin.load",
            "p1",
            &metric_fingerprint("mem", 90.0),
            FindingKind::Anomaly,
        )];
        let ev = evaluate(&rules, &mut dedup, input(&mem, 1000), None);
        assert_eq!(ev.notifications.len(), 1);
        apply_notification(&mut dedup, &ev.notifications, &[], 1000);

        // 第二轮：同一台机器换了指纹（内存好了、磁盘满了）。
        // 正确行为 = 1 条新告警 + 1 条恢复（恢复是被允许的），
        // 但**绝不能**是同一条内存告警既报又恢复。
        let disk = vec![finding(
            "builtin.load",
            "p1",
            &metric_fingerprint("disk", 90.0),
            FindingKind::Anomaly,
        )];
        let ev = evaluate(&rules, &mut dedup, input(&disk, 1100), None);
        let kinds: Vec<bool> = ev
            .notifications
            .iter()
            .map(|n| n.dedup_key.ends_with("|recovered"))
            .collect();
        assert!(kinds.contains(&false), "必须发出新告警: {kinds:?}");
        assert_eq!(ev.recovered.len(), 1, "旧指纹应判为恢复");
        assert_eq!(
            ev.notifications.len(),
            2,
            "一条新告警 + 一条恢复，不得多也不得少"
        );
    }

    #[test]
    fn disabled_rule_and_scoped_rule_are_skipped() {
        let mut dedup = Vec::new();
        let mut off = rule(
            "builtin.load",
            AlertCondition::MetricThreshold(LoadThresholds::default()),
        );
        off.enabled = false;
        let f = vec![finding(
            "builtin.load",
            "p1",
            &metric_fingerprint("mem", 90.0),
            FindingKind::Anomaly,
        )];
        let ev = evaluate(&[off], &mut dedup, input(&f, 1000), None);
        assert!(ev.notifications.is_empty(), "停用的规则不得触发");

        // 规则只作用于 t2，而本次是 t1
        let mut scoped = rule(
            "builtin.load",
            AlertCondition::MetricThreshold(LoadThresholds::default()),
        );
        scoped.task_ids = vec!["t2".into()];
        let ev = evaluate(&[scoped], &mut dedup, input(&f, 1000), None);
        assert!(ev.notifications.is_empty(), "作用域外的任务不得触发");

        // 规则只作用于 p2
        let mut by_profile = rule(
            "builtin.load",
            AlertCondition::MetricThreshold(LoadThresholds::default()),
        );
        by_profile.target_profiles = vec!["p2".into()];
        let ev = evaluate(&[by_profile], &mut dedup, input(&f, 1000), None);
        assert!(ev.notifications.is_empty(), "作用域外的目标不得触发");
    }

    #[test]
    fn evaluate_does_not_touch_other_tasks_state() {
        // 预置一条属于 t2 的 firing 记录
        let other_key = DedupEntry::key_of("t2", "builtin.load", "px", "metric:mem>90");
        let mut dedup = vec![DedupEntry {
            key: other_key.clone(),
            task_id: "t2".into(),
            rule_id: "builtin.load".into(),
            profile_id: "px".into(),
            profile_name: "px".into(),
            fingerprint: "metric:mem>90".into(),
            last_message: "内存 94%".into(),
            severity: Severity::Warning,
            first_seen: 10,
            last_notified: 10,
            count: 1,
            firing: true,
        }];
        // 另一条属于 t1（本次判定的任务），故意不给 finding -> 应被判定为恢复
        let mine_key = DedupEntry::key_of("t1", "builtin.load", "p1", "metric:mem>90");
        dedup.push(DedupEntry {
            key: mine_key.clone(),
            task_id: "t1".into(),
            rule_id: "builtin.load".into(),
            profile_id: "p1".into(),
            profile_name: "p1".into(),
            fingerprint: "metric:mem>90".into(),
            last_message: "内存 94%".into(),
            severity: Severity::Warning,
            first_seen: 10,
            last_notified: 10,
            count: 1,
            firing: true,
        });

        let rules = vec![rule(
            "builtin.load",
            AlertCondition::MetricThreshold(LoadThresholds::default()),
        )];
        // t1 什么都没命中
        let ev = evaluate(&rules, &mut dedup, input(&[], 999), None);

        assert_eq!(ev.recovered.len(), 1, "本次任务那条应判为恢复");
        assert_eq!(ev.recovered[0].key, mine_key);
        assert_eq!(dedup.len(), 1, "不属于本次判定的条目必须原样保留");
        assert_eq!(dedup[0].key, other_key);
        assert!(dedup[0].firing, "t2 的 firing 状态不得被 t1 的判定清掉");
    }

    #[test]
    fn notification_carries_severity_prefix_and_task_name() {
        let mut dedup = Vec::new();
        let mut r = rule(
            "builtin.load",
            AlertCondition::MetricThreshold(LoadThresholds::default()),
        );
        r.severity = Severity::Critical;
        let f = vec![finding(
            "builtin.load",
            "prod-03",
            &metric_fingerprint("mem", 90.0),
            FindingKind::Anomaly,
        )];
        let ev = evaluate(&[r], &mut dedup, input(&f, 1_700_000_000), None);
        let n = &ev.notifications[0];
        assert!(n.title.starts_with("[CRITICAL]"), "title={}", n.title);
        assert_eq!(n.task_name.as_deref(), Some("早间巡检"));
        assert!(n.body_text.contains("prod-03") || n.title.contains("prod-03"));
    }
}
