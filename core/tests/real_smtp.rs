//! 实机联调：对着**真实** SMTP 服务器发一封邮件。
//!
//! 平时不跑：需要真实凭据 + 出网，故标 `#[ignore]`，用
//! `cargo test -p rustshell-core -- --ignored --nocapture real_smtp` 触发。
//! 凭据从**环境变量**读，绝不写进源码（`SMTP_USER` / `SMTP_PASS`）。
//!
//! 为什么要有它：进程内假对端能钉住「发出的字节对不对」，但证不了
//! 「真实服务器收不收、真服务端的 TLS 握手能不能过、认证口令对不对」。
//! 这两类缺口只能由实机补 —— 与铁律「协议类功能优先用进程内假对端验协议路径，
//! 真服务连通性单独登记为遗留」是同一条思路的后一半。

use rustshell_core::notify_smtp::{MailAddress, RenderedMail, SmtpConfig, SmtpSecurity};

fn real_cfg() -> Option<SmtpConfig> {
    let user = std::env::var("SMTP_USER").ok()?;
    // 口令只用于确认「环境变量确实给了」，**不**进配置结构（那里只有密文字段）；
    // 明文由 `send_mail` 的独立参数接收，避免它有机会被序列化。
    std::env::var("SMTP_PASS").ok()?;
    let host = std::env::var("SMTP_HOST").unwrap_or_else(|_| "smtp.163.com".to_string());
    Some(SmtpConfig {
        enabled: true,
        host: host.clone(),
        port: std::env::var("SMTP_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(465),
        security: SmtpSecurity::Tls,
        username: user.clone(),
        // 明文口令由调用方注入（本测试直接传参，不经密文层）
        password_cipher: None,
        from: MailAddress {
            address: user.clone(),
            name: "RustShell 实机验证".into(),
        },
        to: vec![MailAddress::new(&user)],
        cc: vec![],
        subject_prefix: "[RustShell]".to_string(),
        timeout_secs: 20,
        max_retries: 1,
    })
}

#[test]
#[ignore = "需要真实 SMTP 凭据与出网；用 --ignored 手动触发"]
fn real_smtp_delivery_against_a_live_server() {
    let Some(cfg) = real_cfg() else {
        panic!("未设置 SMTP_USER / SMTP_PASS 环境变量");
    };
    let pass = std::env::var("SMTP_PASS").unwrap();
    assert!(
        cfg.validate().is_empty(),
        "配置应通过校验：{:?}",
        cfg.validate()
    );

    let mail = RenderedMail {
        from: cfg.from.clone(),
        to: cfg.to.clone(),
        cc: vec![],
        subject: "[CRIT] 实机联调测试：预警邮件投递".to_string(),
        body_text: "这是一封来自 RustShell 的实机联调测试邮件。\
                    如果你收到了它，说明 SMTP 投递链路（隐式 TLS + AUTH LOGIN + MIME 编码）完全可用。"
            .to_string(),
        body_html: None,
    };

    let report =
        rustshell_core::smtp_client::send_mail(&cfg, &mail, &pass, "").expect("实机投递应当成功");

    println!(
        "投递成功：收件人 {:?}，队列 id {:?}，是否加密 {}",
        report.recipients, report.queue_id, report.used_tls
    );
    assert_eq!(report.recipients.len(), 1);
    assert!(report.used_tls, "配置为隐式 TLS，必须报告已加密");
    assert!(!report.recipients.is_empty());
}
