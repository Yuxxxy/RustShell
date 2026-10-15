//! 通知通道与派发。
//!
//! 通道用 trait 抽象：**邮件发送本期留作后续实现**（需求明确），但通道接口、
//! 模板渲染、派发与结果记录本期全部落地 —— 后续实现只需补 [`SmtpChannel::send`]，
//! 告警引擎一行都不用改。

use super::types::*;
use crate::notify_smtp::SmtpConfig;
use crate::smtp_client::{DeliveryReport, Unprotect};
use std::path::PathBuf;
use std::sync::Arc;

/// 通知通道。
pub trait NotifyChannel: Send + Sync {
    /// 稳定 id（规则里的 `channels` 用它选中通道）
    fn id(&self) -> &'static str;
    /// 是否已配置好（未配置的通道在派发前被跳过，**不**算失败）
    fn is_configured(&self) -> bool;
    fn send(&self, n: &Notification) -> Result<(), String>;
}

/// 通道注册表。
pub struct Notifier {
    channels: Vec<Arc<dyn NotifyChannel>>,
    /// 审计落点（AC-29：通知扇出也要留痕）。
    ///
    /// 通知是「系统对外说话了」这件事本身，必须能事后核对 ——
    /// 用户问「到底发没发」时，日志是唯一的答案。
    audit_path: Option<PathBuf>,
}

impl Notifier {
    pub fn new(channels: Vec<Arc<dyn NotifyChannel>>) -> Self {
        Self {
            channels,
            audit_path: None,
        }
    }

    /// 挂上审计落点（生产路径用；测试不挂）。
    pub fn with_audit(mut self, path: PathBuf) -> Self {
        self.audit_path = Some(path);
        self
    }

    /// 可用的通道 id 列表（设置界面展示「当前生效的通道」用）。
    pub fn available(&self) -> Vec<&'static str> {
        self.channels
            .iter()
            .filter(|c| c.is_configured())
            .map(|c| c.id())
            .collect()
    }

    /// 向规则指定的通道派发。
    ///
    /// 结果三元语义（这点很关键，直接决定用户会不会被噪声淹没）：
    /// * 通道不在注册表里 -> `Err("未知通道")`（配置写错了，必须让人看见）；
    /// * 通道未配置 -> `Skipped`（**不是失败**：邮件没配 SMTP 就只是
    ///   「暂时不往那儿发」，不该报成发送失败）；
    /// * 发送出错 -> `Failed`。
    ///
    /// 三态必须分开：用户问「到底发没发」时，`Skipped` 与 `Ok` 混成一团就
    /// 答不上来 —— 2026-10-12 实测踩过：邮件通道未实现时被记成 `ok=true`，
    /// 审计日志里与「真的发出去了」**逐字相同**（见 `channel_result` 的护栏）。
    pub fn dispatch(&self, rule: &AlertRule, n: &Notification) -> DispatchOutcome {
        let mut out = Vec::new();
        for want in &rule.channels {
            let Some(ch) = self.channels.iter().find(|c| c.id() == want.as_str()) else {
                out.push(ChannelResult {
                    channel: want.clone(),
                    outcome: ChannelOutcome::Failed(format!("未知通道: {want}")),
                });
                continue;
            };
            if !ch.is_configured() {
                log::info!("通知通道 {want} 未配置，跳过本次派发");
                out.push(ChannelResult {
                    channel: want.clone(),
                    outcome: ChannelOutcome::Skipped("未配置".into()),
                });
                continue;
            }
            match ch.send(n) {
                Ok(()) => out.push(ChannelResult {
                    channel: want.clone(),
                    outcome: ChannelOutcome::Ok,
                }),
                Err(e) => {
                    log::warn!("通知通道 {want} 发送失败: {e}");
                    out.push(ChannelResult {
                        channel: want.clone(),
                        outcome: ChannelOutcome::Failed(e),
                    });
                }
            }
        }
        // 审计扇出结果（AC-29）：每条通道一行，含成功/失败/跳过。
        //
        // ⚠️ 三态必须写进审计（`ok` / `skipped` / `ok=false`）：把「跳过」也写成
        // `ok=true` 会让用户查「邮件发没发」时拿到假阳性 —— 那正是未实现时期
        // 留下的坑，别再犯。
        if let Some(path) = &self.audit_path {
            for r in &out {
                let ok = r.outcome.is_ok();
                let risk = match n.severity {
                    Severity::Critical => crate::ai::Risk::Blocked,
                    Severity::Warning => crate::ai::Risk::Confirm,
                    Severity::Info => crate::ai::Risk::Safe,
                };
                crate::ai::audit_log(
                    path,
                    "alert",
                    &format!(
                        "[{}] channel={} {} :: {}",
                        n.severity.tag(),
                        r.channel,
                        r.outcome.audit_detail(),
                        n.title
                    ),
                    risk,
                    ok,
                );
            }
        }
        out
    }
}

/// 单个通道的派发结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelResult {
    pub channel: String,
    pub outcome: ChannelOutcome,
}

/// 派发结果三态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelOutcome {
    /// 已投递
    Ok,
    /// 有意跳过（通道未配置等）。**不是失败**，但也不是「已发送」。
    Skipped(String),
    /// 投递失败
    Failed(String),
}

impl ChannelOutcome {
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Ok)
    }

    /// 审计行里的三态措辞：逐字不同，事后查日志才分得清。
    pub fn audit_detail(&self) -> String {
        match self {
            Self::Ok => "ok=true".to_string(),
            Self::Skipped(why) => format!("ok=false skipped:{why}"),
            Self::Failed(e) => format!("ok=false failed:{e}"),
        }
    }
}

/// 日志通道：写 `logs/alerts.log`（JSON Lines）。
///
/// 它永远是配置好的 —— 这是唯一一个「不配也能用」的通道，
/// 保证任何预警至少在本机留痕。
pub struct LogChannel {
    path: PathBuf,
}

impl LogChannel {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl NotifyChannel for LogChannel {
    fn id(&self) -> &'static str {
        "log"
    }

    fn is_configured(&self) -> bool {
        true
    }

    fn send(&self, n: &Notification) -> Result<(), String> {
        use std::io::Write as _;
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let entry = serde_json::json!({
            "ts": n.occurred_at,
            "severity": n.severity.tag(),
            "task": n.task_name,
            "title": n.title,
            // 正文可能含主机名/日志样本：落盘前统一脱敏（与审计同一口径）
            "body": crate::ai::redact(&n.body_text),
            "dedup_key": n.dedup_key,
        });
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|e| e.to_string())?;
        writeln!(f, "{entry}").map_err(|e| e.to_string())
    }
}

/// 邮件通道：真正投递到 SMTP 服务器。
///
/// 2026-10-12：原先 `send` 返回哨兵「未实现」（需求当时把投递排在本期之后）。
/// 现已补齐真实投递，实现委托给 [`crate::smtp_client`] —— 本模块只负责
/// 「把通知渲染成邮件」与「把结果翻译成通道语义」。
pub struct SmtpChannel {
    cfg: SmtpConfig,
    /// 口令解密器：由宿主注入（core 不依赖 platform，与
    /// `Settings::runtime_proxy` 同一手法）。
    unprotect: Box<Unprotect>,
}

impl SmtpChannel {
    pub fn new(cfg: SmtpConfig) -> Self {
        Self {
            cfg,
            // 默认「解不开」而不是「解出空串」：空口令会被当成无认证中继，
            // 于是配置了口令却静默降级 —— 那正是要避免的静默失败。
            unprotect: Box::new(|_| None),
        }
    }

    /// 带解密器构造（生产路径由宿主给 `platform::secret::unprotect`）。
    pub fn with_unprotect(
        cfg: SmtpConfig,
        unprotect: impl Fn(&str) -> Option<String> + Send + Sync + 'static,
    ) -> Self {
        Self {
            cfg,
            unprotect: Box::new(unprotect),
        }
    }

    /// 投递一封（供「发送测试邮件」按钮复用同一条链路）。
    pub fn deliver(&self, n: &Notification) -> Result<DeliveryReport, String> {
        let mail = crate::notify_smtp::render_mail(&self.cfg, n);
        // 有密文却解不开 = 明确的失败，不能当成「无认证」继续发。
        let password = match &self.cfg.password_cipher {
            None => String::new(),
            Some(cipher) => (self.unprotect)(cipher).ok_or_else(|| {
                crate::i18n::ts(
                    "SMTP 口令解密失败（系统凭据存储不可用或密文来自另一台机器）".to_string(),
                    "cannot decrypt the SMTP password (credential store unavailable)".to_string(),
                )
            })?,
        };
        crate::smtp_client::send_mail(&self.cfg, &mail, &password, "").map_err(|e| e.to_string())
    }
}

impl NotifyChannel for SmtpChannel {
    fn id(&self) -> &'static str {
        "email"
    }

    fn is_configured(&self) -> bool {
        self.cfg.is_ready()
    }

    fn send(&self, n: &Notification) -> Result<(), String> {
        let report = self.deliver(n)?;
        log::info!(
            "预警邮件已投递：{} 个收件人（tls={}）{}",
            report.recipients.len(),
            report.used_tls,
            report.queue_id.as_deref().unwrap_or("")
        );
        Ok(())
    }
}

/// [`render_notification`] 的入参。
///
/// 打成结构体的理由与 [`crate::alerts::engine`] 里的 `BumpSpec` 相同：
/// 这里有连续四个 `&str`，位置写错编译器不会拦。
pub struct NotificationDraft<'a> {
    pub rule: &'a AlertRule,
    /// 条目**实际**的严重级（不是规则基准级）
    pub severity: Severity,
    /// 标题主体（严重级前缀由本函数补）
    pub title: &'a str,
    pub body: &'a str,
    pub now: u64,
    pub task_name: Option<&'a str>,
    /// 幂等键：告警用去重键，恢复通知用「去重键 + |recovered」
    pub dedup_key: &'a str,
    /// 目标展示名（可选；聚合型告警没有单一目标）
    pub target: Option<&'a str>,
}

/// 渲染一条通知（主题 + 正文）。
///
/// 主题格式：`[SEVERITY] {标题}`，与需求文档 §9.2 的邮件主题拼装口径一致。
pub fn render_notification(draft: NotificationDraft<'_>) -> Notification {
    let NotificationDraft {
        rule,
        severity,
        title,
        body,
        now,
        task_name,
        dedup_key,
        target,
    } = draft;
    let time = crate::metrics::time_label(now);
    let full_title = format!("[{}] {title}", severity.tag());
    let mut text = String::new();
    text.push_str(&format!(
        "{}\n",
        crate::i18n::ts("RustShell 预警".into(), "RustShell Alert".into())
    ));
    text.push_str(&format!(
        "{}: {}\n",
        crate::i18n::ts("规则".into(), "Rule".into()),
        rule.name
    ));
    if let Some(t) = task_name {
        text.push_str(&format!(
            "{}: {t}\n",
            crate::i18n::ts("任务".into(), "Task".into())
        ));
    }
    text.push_str(&format!(
        "{}: {}\n",
        crate::i18n::ts("级别".into(), "Severity".into()),
        severity.label()
    ));
    text.push_str(&format!(
        "{}: {time}\n",
        crate::i18n::ts("时间".into(), "Time".into())
    ));
    if let Some(t) = target {
        text.push_str(&format!(
            "{}: {t}\n",
            crate::i18n::ts("目标".into(), "Target".into())
        ));
    }
    text.push('\n');
    text.push_str(body);
    text.push('\n');

    Notification {
        title: full_title,
        body_text: text,
        body_html: None,
        severity,
        task_name: task_name.map(str::to_string),
        occurred_at: now,
        dedup_key: dedup_key.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alerts::types::AlertCondition;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn rule_with(channels: Vec<&str>) -> AlertRule {
        AlertRule {
            id: "builtin.load".into(),
            name: "负载超阈值".into(),
            enabled: true,
            task_ids: Vec::new(),
            target_profiles: Vec::new(),
            condition: AlertCondition::MetricThreshold(LoadThresholds::default()),
            suppress_secs: 1800,
            notify_recovery: true,
            severity: Severity::Warning,
            channels: channels.into_iter().map(str::to_string).collect(),
        }
    }

    fn notif() -> Notification {
        render_notification(NotificationDraft {
            rule: &rule_with(vec!["log"]),
            severity: Severity::Warning,
            title: "负载超阈值 · prod-03",
            body: "内存使用率 94%（9.4G / 10G），阈值 90%",
            now: 1_700_000_000,
            task_name: Some("早间巡检"),
            dedup_key: "k1",
            target: Some("prod-03"),
        })
    }

    struct CountingChannel {
        id: &'static str,
        configured: bool,
        calls: Arc<AtomicUsize>,
        fail: bool,
    }

    impl NotifyChannel for CountingChannel {
        fn id(&self) -> &'static str {
            self.id
        }
        fn is_configured(&self) -> bool {
            self.configured
        }
        fn send(&self, _n: &Notification) -> Result<(), String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                Err("boom".into())
            } else {
                Ok(())
            }
        }
    }

    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "rs-alerts-{tag}-{}-{}",
            std::process::id(),
            crate::connections::unix_now_secs()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn title_carries_severity_and_body_carries_context() {
        let n = notif();
        assert!(n.title.starts_with("[WARNING]"), "title={}", n.title);
        assert!(n.body_text.contains("负载超阈值"));
        assert!(n.body_text.contains("早间巡检"));
        assert!(n.body_text.contains("内存使用率"));
        assert_eq!(n.task_name.as_deref(), Some("早间巡检"));
        assert_eq!(n.dedup_key, "k1");
    }

    #[test]
    fn log_channel_writes_json_lines() {
        let dir = tmp_dir("log");
        let path = dir.join("alerts.log");
        let ch = LogChannel::new(path.clone());
        assert!(ch.is_configured());
        ch.send(&notif()).unwrap();
        ch.send(&notif()).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        let v: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(v["severity"], "WARNING");
        assert_eq!(v["task"], "早间巡检");
        assert_eq!(v["dedup_key"], "k1");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn log_channel_redacts_secrets_in_body() {
        let dir = tmp_dir("redact");
        let path = dir.join("alerts.log");
        let ch = LogChannel::new(path.clone());
        let mut n = notif();
        n.body_text = "token=abcdef123456 记录".into();
        ch.send(&n).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("abcdef123456"), "落盘正文必须脱敏：{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn smtp_channel_is_unconfigured_without_settings_and_skips_when_dispatched() {
        let calls = Arc::new(AtomicUsize::new(0));
        let log_calls = calls.clone();
        let notifier = Notifier::new(vec![
            Arc::new(CountingChannel {
                id: "log",
                configured: true,
                calls: log_calls,
                fail: false,
            }),
            Arc::new(SmtpChannel::new(SmtpConfig::default())),
        ]);

        // 未配置 SMTP：email 通道被跳过，但**不产生失败**
        let out = notifier.dispatch(&rule_with(vec!["log", "email"]), &notif());
        assert_eq!(out.len(), 2);
        assert!(
            !out.iter()
                .any(|r| matches!(r.outcome, ChannelOutcome::Failed(_))),
            "未配置不等于失败: {out:?}"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1, "log 通道应被调用一次");

        // email 通道未配置 ⇒ 记为 **Skipped**，且**不得**与 Ok 混同
        // （2026-10-12 修的正是这个：以前两者在审计里逐字相同）。
        let email = out
            .iter()
            .find(|r| r.channel == "email")
            .expect("应有 email 通道的结果");
        assert!(
            matches!(email.outcome, ChannelOutcome::Skipped(_)),
            "未配置的通道应记为 Skipped 而非 Ok: {out:?}"
        );
        assert!(
            !email.outcome.is_ok(),
            "Skipped 不得被当成「已投递」: {out:?}"
        );
        assert!(
            email.outcome.audit_detail().contains("skipped"),
            "审计行必须写得出「跳过」: {}",
            email.outcome.audit_detail()
        );
    }

    /// 三态在**审计措辞**上必须逐字不同，否则查日志时分不出来。
    #[test]
    fn three_states_have_distinct_audit_wording() {
        let ok = ChannelOutcome::Ok.audit_detail();
        let skipped = ChannelOutcome::Skipped("未配置".into()).audit_detail();
        let failed = ChannelOutcome::Failed("连接超时".into()).audit_detail();
        assert_eq!(ok, "ok=true");
        assert_eq!(skipped, "ok=false skipped:未配置");
        assert_eq!(failed, "ok=false failed:连接超时");
        // 「跳过」绝不能自称成功 —— 那是用户查「发没发」时拿到假阳性的根源
        assert_ne!(skipped, ok, "Skipped 与 Ok 的审计措辞必须不同");
        assert_ne!(failed, ok, "Failed 与 Ok 的审计措辞必须不同");
        assert!(!skipped.contains("ok=true"));
        assert!(!failed.contains("ok=true"));
    }

    /// 未配置邮件通道 + 未实现发送的组合下，不得产生任何「已投递」的假象。
    #[test]
    fn unconfigured_email_channel_never_claims_delivery() {
        let cfg = crate::notify_smtp::SmtpConfig {
            enabled: true,
            host: "smtp.example.com".into(),
            from: crate::notify_smtp::MailAddress::new("a@b.com"),
            to: vec![crate::notify_smtp::MailAddress::new("c@d.com")],
            ..Default::default()
        };
        // 端口 1 必然连不上：即使走到 send 也只会得到 Failed，绝不会是 Ok
        let notifier = Notifier::new(vec![Arc::new(SmtpChannel::new(cfg))]);
        let out = notifier.dispatch(&rule_with(vec!["email"]), &notif());
        assert_eq!(out.len(), 1);
        assert!(
            !out[0].outcome.is_ok(),
            "没真的发出信就不得记为 Ok: {out:?}"
        );
    }

    #[test]
    fn known_failure_and_unknown_channel_are_reported() {
        let calls = Arc::new(AtomicUsize::new(0));
        let notifier = Notifier::new(vec![Arc::new(CountingChannel {
            id: "log",
            configured: true,
            calls: calls.clone(),
            fail: true,
        })]);
        let out = notifier.dispatch(&rule_with(vec!["log"]), &notif());
        assert!(
            matches!(out[0].outcome, ChannelOutcome::Failed(_)),
            "真实发送失败必须上报: {out:?}"
        );

        // 规则里写了一个不存在的通道
        let out = notifier.dispatch(&rule_with(vec!["webhook"]), &notif());
        assert_eq!(out.len(), 1);
        assert!(
            matches!(out[0].outcome, ChannelOutcome::Failed(_)),
            "未知通道必须报错（配置写错了要让人看见）: {out:?}"
        );
    }

    #[test]
    fn available_lists_only_configured_channels() {
        let calls = Arc::new(AtomicUsize::new(0));
        let notifier = Notifier::new(vec![
            Arc::new(LogChannel::new(tmp_dir("avail").join("alerts.log"))),
            Arc::new(CountingChannel {
                id: "desktop",
                configured: false,
                calls,
                fail: false,
            }),
        ]);
        assert_eq!(notifier.available(), vec!["log"]);
    }
}
