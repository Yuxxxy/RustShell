//! 通知摘要合并（digest）：把一次运行里产生的多条告警压成一条。
//!
//! **为什么需要它**：按「规则 + 目标 + 指纹」去重只解决了「同一台机器同一个原因
//! 只报一次」。三十台机器同时内存告警仍然是三十条通知 —— 用户收到的是三十封邮件，
//! 而他想知道的是「这三台北区机器都有问题」。
//!
//! 合并的时机是刻意延迟的：不是「攒够 N 条再发」，而是「攒 `digest_window_secs`
//! 秒再发」。理由是同一次运行的多条告警几乎同时产生，按时间窗口收口能让
//! **一次运行 = 一条通知**，而不是把同一批割成几段。
//!
//! 两条例外：
//! * `Severity::Critical` **不合并**，立即单发 —— 主机密钥不一致这类事故
//!   不该因为「攒一攒再发」而延迟；
//! * 窗口 ≤ 0 视为不合并（沿用既有逐条行为，便于排查）。

use super::types::{Notification, Severity};
use crate::tasks::NotifyPolicy;
use serde::{Deserialize, Serialize};

/// 一条被攒住的摘要。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingDigest {
    pub task_id: String,
    pub task_name: String,
    /// 窗口起点（第一条进来的时刻）
    pub started_at: u64,
    /// 到点时刻（`started_at + digest_window_secs`）
    pub flush_at: u64,
    pub window_secs: u64,
    /// 已攒住的通知（按到达顺序）
    pub items: Vec<Notification>,
}

impl PendingDigest {
    /// 是否到点该发了。
    pub fn is_due(&self, now: u64) -> bool {
        now >= self.flush_at
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// 合并成一条通知。
    ///
    /// 标题写清「几条」，正文逐条列出目标与摘要 —— 用户扫一眼就知道
    /// 是哪几台机器、分别是什么问题，而不必点开每一条。
    pub fn merge(&self, now: u64) -> Notification {
        use crate::i18n::ts;
        let n = self.items.len();
        // 取最严重的级别作为整条摘要的级别
        let severity = self
            .items
            .iter()
            .map(|i| i.severity)
            .max()
            .unwrap_or(Severity::Warning);
        let title = ts(
            format!("{}（合并 {n} 条）", self.task_name),
            format!("{} ({n} merged)", self.task_name),
        );
        let mut body = String::new();
        body.push_str(&ts(
            format!(
                "任务「{}」在 {} 秒窗口内产生 {n} 条预警，合并如下：\n",
                self.task_name, self.window_secs
            ),
            format!(
                "Task \"{}\" produced {n} alerts within a {}s window:\n",
                self.task_name, self.window_secs
            ),
        ));
        for (ix, item) in self.items.iter().enumerate() {
            // 单条正文里的首行是标题（如「[WARNING] 负载超阈值 · prod-03」），
            // 取它当列表项最可读；没有就退回整条。
            let first = item
                .title
                .lines()
                .next()
                .unwrap_or(item.title.as_str())
                .trim();
            body.push_str(&format!("{}. {first}\n", ix + 1));
            // 明细只给前若干行，避免合并后过长
            for line in item
                .body_text
                .lines()
                .filter(|l| !l.trim().is_empty())
                .take(3)
            {
                body.push_str(&format!("   {line}\n"));
            }
        }
        Notification {
            title,
            body_text: body,
            body_html: None,
            severity,
            task_name: Some(self.task_name.clone()),
            occurred_at: now,
            // 幂等键用窗口起点：同一窗口只发一条，重试也只会被通道去重一次
            dedup_key: format!("digest:{}:{}", self.task_id, self.started_at),
        }
    }
}

/// 把一批新通知按策略分配：该攒的攒、该直发的直发。
///
/// 返回 `(立即发送, 攒住的通知)`。
pub fn split(
    policy: &NotifyPolicy,
    incoming: Vec<Notification>,
) -> (Vec<Notification>, Vec<Notification>) {
    if policy.digest_window_secs == 0 {
        // 窗口为 0 = 关闭合并（既有逐条行为）
        return (incoming, Vec::new());
    }
    let mut immediate = Vec::new();
    let mut held = Vec::new();
    for n in incoming {
        // Critical 不合并：主机密钥不一致这类事故不该被窗口延迟
        if n.severity >= Severity::Critical {
            immediate.push(n);
        } else {
            held.push(n);
        }
    }
    (immediate, held)
}

/// 把新到的通知塞进摘要缓冲（新建或追加）。
pub fn push(
    pending: &mut Vec<PendingDigest>,
    task_id: &str,
    task_name: &str,
    policy: &NotifyPolicy,
    held: Vec<Notification>,
    now: u64,
) {
    if held.is_empty() {
        return;
    }
    if let Some(d) = pending.iter_mut().find(|d| d.task_id == task_id) {
        d.items.extend(held);
        return;
    }
    pending.push(PendingDigest {
        task_id: task_id.to_string(),
        task_name: task_name.to_string(),
        started_at: now,
        // 窗口从**第一条**进来的时刻算起，而不是每次追加都重置 ——
        // 重置的话持续产生告警的任务会把摘要无限期推迟。
        flush_at: now.saturating_add(policy.digest_window_secs.max(1)),
        window_secs: policy.digest_window_secs,
        items: held,
    });
}

/// 取出到点的摘要并从缓冲里移除（**不**打包，由调用方选通道）。
pub fn take_due(pending: &mut Vec<PendingDigest>, now: u64) -> Vec<PendingDigest> {
    let mut out = Vec::new();
    let mut keep = Vec::new();
    for d in pending.drain(..) {
        if d.is_due(now) {
            out.push(d);
        } else {
            keep.push(d);
        }
    }
    *pending = keep;
    out
}

/// 关闭 / 重启前把缓冲里所有摘要取出（不能丢：用户还没看到）。
pub fn take_all(pending: &mut Vec<PendingDigest>) -> Vec<PendingDigest> {
    std::mem::take(pending)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notif(sev: Severity, title: &str) -> Notification {
        Notification {
            title: format!("[{}] {title}", sev.tag()),
            body_text: "内存使用率 94%（9.4G / 10G），阈值 90%".into(),
            body_html: None,
            severity: sev,
            task_name: Some("早间巡检".into()),
            occurred_at: 1000,
            dedup_key: format!("k:{title}"),
        }
    }

    #[test]
    fn critical_never_waits_for_the_window() {
        let policy = NotifyPolicy {
            digest_window_secs: 300,
            ..Default::default()
        };
        let (now, held) = split(
            &policy,
            vec![
                notif(Severity::Warning, "负载超阈值 · prod-01"),
                notif(Severity::Critical, "主机密钥不一致 · prod-02"),
                notif(Severity::Warning, "日志异常 · prod-03"),
            ],
        );
        assert_eq!(now.len(), 1, "Critical 必须立即单发");
        assert_eq!(now[0].severity, Severity::Critical);
        assert_eq!(held.len(), 2, "两条 Warning 该被攒住");
    }

    #[test]
    fn zero_window_disables_digest() {
        let policy = NotifyPolicy {
            digest_window_secs: 0,
            ..Default::default()
        };
        let (now, held) = split(
            &policy,
            vec![notif(Severity::Warning, "a"), notif(Severity::Warning, "b")],
        );
        assert_eq!(now.len(), 2, "窗口 0 = 不合并（沿用逐条行为）");
        assert!(held.is_empty());
    }

    #[test]
    fn buffer_accumulates_into_one_and_flushes_after_the_window() {
        let policy = NotifyPolicy {
            digest_window_secs: 300,
            ..Default::default()
        };
        let mut pending = Vec::new();
        // 第一台到
        push(
            &mut pending,
            "t1",
            "早间巡检",
            &policy,
            vec![notif(Severity::Warning, "prod-01")],
            1_000,
        );
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].flush_at, 1_300, "窗口从第一条算起");

        // 第二、三台在同一窗口内到：追加而不是新建
        push(
            &mut pending,
            "t1",
            "早间巡检",
            &policy,
            vec![
                notif(Severity::Warning, "prod-02"),
                notif(Severity::Critical, "prod-03"),
            ],
            1_100,
        );
        assert_eq!(pending.len(), 1, "同一任务只该有一条摘要");
        assert_eq!(pending[0].len(), 3);
        assert_eq!(
            pending[0].flush_at, 1_300,
            "追加**不得**把窗口往后推（否则持续告警会无限期推迟）"
        );

        // 未到点：取不出来
        assert!(take_due(&mut pending, 1_299).is_empty());
        assert_eq!(pending.len(), 1);

        // 到点：取出并清空
        let due = take_due(&mut pending, 1_300);
        assert_eq!(due.len(), 1);
        assert!(pending.is_empty(), "取走后缓冲必须清空，否则会重复发");
    }

    #[test]
    fn different_tasks_keep_separate_digests() {
        let policy = NotifyPolicy {
            digest_window_secs: 60,
            ..Default::default()
        };
        let mut pending = Vec::new();
        push(
            &mut pending,
            "t1",
            "任务一",
            &policy,
            vec![notif(Severity::Warning, "a")],
            100,
        );
        push(
            &mut pending,
            "t2",
            "任务二",
            &policy,
            vec![notif(Severity::Warning, "b")],
            100,
        );
        assert_eq!(pending.len(), 2, "不同任务各自成条");
        let due = take_due(&mut pending, 200);
        assert_eq!(due.len(), 2);
    }

    #[test]
    fn merge_produces_one_readable_notification() {
        let policy = NotifyPolicy {
            digest_window_secs: 300,
            ..Default::default()
        };
        let mut pending = Vec::new();
        push(
            &mut pending,
            "t1",
            "早间巡检",
            &policy,
            vec![
                notif(Severity::Warning, "prod-01"),
                notif(Severity::Critical, "prod-02"),
            ],
            1_000,
        );
        let merged = pending[0].merge(1_300);
        assert!(
            merged.title.contains("早间巡检") && merged.title.contains('2'),
            "标题要写清任务与条数：{}",
            merged.title
        );
        // 级别取最严重的那条
        assert_eq!(merged.severity, Severity::Critical);
        // 正文逐条列出，用户不用点开每一条
        assert!(merged.body_text.contains("prod-01"));
        assert!(merged.body_text.contains("prod-02"));
        assert!(merged.body_text.contains("内存使用率"), "要带明细");
        // 幂等键按窗口起点，重试不会重复发
        assert_eq!(merged.dedup_key, "digest:t1:1000");
        assert_eq!(merged.task_name.as_deref(), Some("早间巡检"));
    }

    /// 关闭 / 重启前必须能把缓冲取空，否则用户的告警会**永远**不出现。
    #[test]
    fn take_all_drains_the_buffer() {
        let policy = NotifyPolicy {
            digest_window_secs: 300,
            ..Default::default()
        };
        let mut pending = Vec::new();
        push(
            &mut pending,
            "t1",
            "x",
            &policy,
            vec![notif(Severity::Warning, "a")],
            10,
        );
        push(
            &mut pending,
            "t2",
            "y",
            &policy,
            vec![notif(Severity::Warning, "b")],
            10,
        );
        let all = take_all(&mut pending);
        assert_eq!(all.len(), 2);
        assert!(pending.is_empty());
    }
}
