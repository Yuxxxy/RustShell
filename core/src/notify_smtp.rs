//! SMTP 邮件配置与内容模板。
//!
//! **投递本身留待后续实现**（需求明确）：本模块交付配置模型、持久化、
//! 口令加密引用、模板渲染与预检，唯独不真的连 SMTP 服务器 ——
//! 那一步在 [`crate::alerts::notify::SmtpChannel::send`] 里以哨兵值返回。
//! 这样后续实现时只需替换那一个函数体。

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::path::PathBuf;

/// SMTP 连接加密方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SmtpSecurity {
    /// 明文（仅用于内网 relay；UI 上要给出明显提示）
    None,
    /// STARTTLS（默认；587）
    #[default]
    StartTls,
    /// 隐式 TLS（465）
    Tls,
}

impl SmtpSecurity {
    /// 该加密方式对应的默认端口。
    pub fn default_port(self) -> u16 {
        match self {
            Self::None => 25,
            Self::StartTls => 587,
            Self::Tls => 465,
        }
    }

    pub fn all() -> [SmtpSecurity; 3] {
        [Self::StartTls, Self::Tls, Self::None]
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::None => "None",
            Self::StartTls => "STARTTLS",
            Self::Tls => "TLS",
        }
    }

    /// UI 下拉用的**稳定**取值：不随语言变化，也不随 `label()` 的文案调整而变。
    ///
    /// 取值刻意与 serde 表示**逐字一致**（`start_tls` 而不是 `starttls`）：
    /// 设置面板存盘走 serde、下拉选中判定走 `key()`，两套写法一旦漂移，
    /// 表现是「界面显示 STARTTLS、文件里存着别的」，而且是静默的。
    /// 由 `serde_repr_matches_security_keys` 钉住这条一致性。
    pub fn key(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::StartTls => "start_tls",
            Self::Tls => "tls",
        }
    }

    /// `key()` 的逆变换。认不出来的一律回退到默认（STARTTLS）——
    /// 配置文件被手改坏时，宁可回到安全的默认，也不要静默变成明文发送。
    ///
    /// 同时接受历史写法 `starttls`（本函数第一版用的就是它）：那份配置已经可能
    /// 落在用户磁盘上了，读不回来就等于把用户的加密方式悄悄改掉。
    pub fn from_key(key: &str) -> Self {
        match key {
            "tls" => Self::Tls,
            "none" => Self::None,
            "starttls" | "start_tls" => Self::StartTls,
            _ => Self::StartTls,
        }
    }

    /// 界面显示名（i18n 键）。`label()` 是给日志和邮件头用的英文缩写，
    /// 用户面前要的是可翻译的文案，故两者分开。
    ///
    /// 注意：`i18n` 的 `g1`/`g2` 护栏是**静态扫源码**的 —— 经过这个函数转手
    /// 的键它们看不见。设置面板因此没有调用它，而是就地写出字面量；
    /// 这里保留是为了让 `notify_smtp` 自己有一条完整的「键 ↔ 文案」定义，
    /// 并由下面的用例钉住它与 `all()` 一致。
    pub fn display_key(self) -> &'static str {
        match self {
            Self::None => "明文（不加密）",
            Self::StartTls => "STARTTLS",
            Self::Tls => "SSL / TLS",
        }
    }
}

#[cfg(test)]
mod security_key_tests {
    use super::SmtpSecurity;

    /// `key()` / `from_key()` 必须互逆，且 `all()` 覆盖全部变体 ——
    /// 下拉里漏一个选项的表现是「用户选不到某个加密方式」。
    #[test]
    fn security_keys_round_trip() {
        assert_eq!(SmtpSecurity::all().len(), 3);
        for s in SmtpSecurity::all() {
            assert_eq!(SmtpSecurity::from_key(s.key()), s);
        }
        // 认不出来的取值回退到安全默认，而不是明文
        assert_eq!(SmtpSecurity::from_key("bogus"), SmtpSecurity::StartTls);
        assert_eq!(SmtpSecurity::from_key(""), SmtpSecurity::StartTls);
        // 历史写法（第一版 `key()`）必须还能读回来：那份值可能已经落盘。
        assert_eq!(SmtpSecurity::from_key("starttls"), SmtpSecurity::StartTls);
        // 三个变体互不相同，否则下拉会出现两个选中项的行为
        assert_ne!(
            SmtpSecurity::StartTls.key(),
            SmtpSecurity::Tls.key(),
            "STARTTLS 与 TLS 的下拉取值不能撞车"
        );
    }

    /// 默认端口与 `key()` 一一对应，供设置面板「选加密方式」时提示端口。
    #[test]
    fn security_default_ports_match_the_keys() {
        assert_eq!(SmtpSecurity::None.default_port(), 25);
        assert_eq!(SmtpSecurity::StartTls.default_port(), 587);
        assert_eq!(SmtpSecurity::Tls.default_port(), 465);
    }

    /// 每个变体都要有显示名，且与 `key()` 一样覆盖 `all()` 的全部三个 ——
    /// 漏一个的表现是下拉里出现一项空白。
    #[test]
    fn every_security_variant_has_a_display_key() {
        for s in SmtpSecurity::all() {
            assert!(!s.display_key().is_empty(), "{s:?} 没有显示名");
            assert!(!s.key().is_empty(), "{s:?} 没有下拉取值");
        }
        assert_eq!(SmtpSecurity::None.display_key(), "明文（不加密）");
    }
}

/// 发件箱里的一个收件人。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailAddress {
    pub address: String,
    #[serde(default)]
    pub name: String,
}

impl MailAddress {
    pub fn new(address: impl Into<String>) -> Self {
        Self {
            address: address.into(),
            name: String::new(),
        }
    }

    /// `显示名 <地址>` 形态（无显示名时只有地址）。
    pub fn formatted(&self) -> String {
        if self.name.trim().is_empty() {
            self.address.clone()
        } else {
            format!("{} <{}>", self.name.trim(), self.address.trim())
        }
    }
}

/// SMTP 配置。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SmtpConfig {
    pub enabled: bool,
    pub host: String,
    pub port: u16,
    pub security: SmtpSecurity,
    pub username: String,
    /// 口令密文（`platform::secret::protect` 产物；明文永不落盘）
    pub password_cipher: Option<String>,
    pub from: MailAddress,
    pub to: Vec<MailAddress>,
    pub cc: Vec<MailAddress>,
    /// 主题前缀（默认 `[RustShell]`）
    pub subject_prefix: String,
    pub timeout_secs: u64,
    /// 失败重试次数（指数退避 5s / 30s / 120s）
    pub max_retries: u32,
}

/// 主题前缀默认值。
pub const DEFAULT_SUBJECT_PREFIX: &str = "[RustShell]";
/// 默认超时（秒）。
pub const DEFAULT_SMTP_TIMEOUT_SECS: u64 = 20;
/// 默认重试次数。
pub const DEFAULT_SMTP_RETRIES: u32 = 3;
/// 单封邮件正文的长度上限（字节）。
pub const MAX_MAIL_BODY_BYTES: usize = 64 * 1024;
/// 收件人（含抄送）数量上限。
pub const MAX_RECIPIENTS: usize = 50;
/// 口令密文在系统凭据存储里的键名。
///
/// 加密（设置面板保存）与解密（后续的投递实现）**必须**用同一个键，
/// 因此定义在这里而不是各自写字面量 —— 写歪一个字符的表现是「口令明明填过却认证失败」，
/// 且没有任何报错指向它。
pub const SECRET_KEY: &str = "smtp-password";

impl Default for SmtpConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            host: String::new(),
            port: SmtpSecurity::default().default_port(),
            security: SmtpSecurity::default(),
            username: String::new(),
            password_cipher: None,
            from: MailAddress::default(),
            to: Vec::new(),
            cc: Vec::new(),
            subject_prefix: DEFAULT_SUBJECT_PREFIX.to_string(),
            timeout_secs: DEFAULT_SMTP_TIMEOUT_SECS,
            max_retries: DEFAULT_SMTP_RETRIES,
        }
    }
}

impl SmtpConfig {
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(s) => match serde_json::from_str(&s) {
                Ok(v) => v,
                Err(_) => {
                    crate::configio::backup_broken_config(path, &s);
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        }
    }

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

    /// 是否已具备投递条件（启用 + 服务器 + 发件人 + 至少一个收件人）。
    ///
    /// **不看口令**：允许无认证的中继（内网 relay 是常见配置）。
    pub fn is_ready(&self) -> bool {
        self.enabled
            && !self.host.trim().is_empty()
            && self.port > 0
            && self.from.is_valid()
            && !self.to.is_empty()
    }

    /// 保存前校验：返回全部问题（不是遇到第一个就返回），
    /// 这样用户能一次看到所有要改的地方。
    pub fn validate(&self) -> Vec<String> {
        use crate::i18n::ts;
        let mut errs = Vec::new();
        if !self.enabled {
            return errs; // 未启用时不校验细节，允许先填一半
        }
        if self.host.trim().is_empty() {
            errs.push(ts(
                "请填写 SMTP 服务器".into(),
                "SMTP host is required".into(),
            ));
        }
        if self.port == 0 {
            errs.push(ts("端口无效".into(), "Invalid port".into()));
        }
        if !self.from.is_valid() {
            errs.push(ts(
                "发件人地址格式不正确".into(),
                "Invalid sender address".into(),
            ));
        }
        if self.to.is_empty() {
            errs.push(ts(
                "请至少填写一个收件人".into(),
                "At least one recipient is required".into(),
            ));
        }
        for a in self.to.iter().chain(self.cc.iter()) {
            if !a.is_valid() {
                errs.push(ts(
                    format!("收件人地址格式不正确：{}", a.address),
                    format!("Invalid recipient: {}", a.address),
                ));
            }
        }
        if self.to.len() + self.cc.len() > MAX_RECIPIENTS {
            errs.push(ts(
                format!("收件人过多（上限 {MAX_RECIPIENTS}）"),
                format!("Too many recipients (max {MAX_RECIPIENTS})"),
            ));
        }
        errs
    }

    /// 主题前缀（空则回退默认；不允许用户把前缀清成空串导致主题无法辨认来源）。
    pub fn effective_subject_prefix(&self) -> &str {
        let p = self.subject_prefix.trim();
        if p.is_empty() {
            DEFAULT_SUBJECT_PREFIX
        } else {
            p
        }
    }

    /// 隐藏口令后的展示形态（设置界面回读用；绝不回显明文）。
    pub fn has_password(&self) -> bool {
        self.password_cipher.is_some()
    }
}

impl MailAddress {
    /// 宽松校验：含 `@`、`@` 两边非空、域名里有点。
    ///
    /// 刻意不做 RFC 5322 完整校验：真要写全是一个几百行的状态机，
    /// 而拦错一个合法地址（如 `a+b@sub.domain.co`）比放过一个错地址更烦人。
    pub fn is_valid(&self) -> bool {
        let a = self.address.trim();
        let Some((local, domain)) = a.split_once('@') else {
            return false;
        };
        !local.is_empty()
            && !domain.is_empty()
            && domain.contains('.')
            && !domain.starts_with('.')
            && !domain.ends_with('.')
            && !a.contains(char::is_whitespace)
    }
}

/// 渲染邮件内容（主题 + 纯文本正文 + HTML 正文）。
///
/// 纯文本是**权威格式**（需求文档 §9.2）：HTML 只是它的渲染，
/// 任何一处不一致都以文本为准。
pub fn render_mail(cfg: &SmtpConfig, n: &crate::alerts::types::Notification) -> RenderedMail {
    let subject = format!("{} {}", cfg.effective_subject_prefix(), n.title);
    // 正文落盘/上送前脱敏：正文里可能带日志样本、命令行参数
    let body_text = limit_body(&crate::ai::redact(&n.body_text));
    let body_html = Some(text_to_html(&body_text));
    RenderedMail {
        from: cfg.from.clone(),
        to: cfg.to.clone(),
        cc: cfg.cc.clone(),
        subject,
        body_text,
        body_html,
    }
}

/// 渲染好的邮件。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RenderedMail {
    pub from: MailAddress,
    pub to: Vec<MailAddress>,
    pub cc: Vec<MailAddress>,
    pub subject: String,
    pub body_text: String,
    pub body_html: Option<String>,
}

/// 正文超限时截断（保留头部：结论与异常清单都在前面）。
fn limit_body(body: &str) -> String {
    if body.len() <= MAX_MAIL_BODY_BYTES {
        return body.to_string();
    }
    let mut cut = MAX_MAIL_BODY_BYTES;
    while cut > 0 && !body.is_char_boundary(cut) {
        cut -= 1;
    }
    format!(
        "{}\n\n{}",
        &body[..cut],
        crate::i18n::ts(
            "…（正文过长已截断，完整内容见运行记录）".to_string(),
            "…(truncated; see the run history for details)".to_string(),
        )
    )
}

/// 极简纯文本 -> HTML：转义 + 换行转 `<br>`，包一层 `<pre>` 保持对齐。
///
/// 不做 Markdown/富文本：巡检报告的排版价值在于「列对齐」，
/// `<pre>` 恰好保住它，而任何模板引擎都会把等宽对齐搞坏。
pub fn text_to_html(text: &str) -> String {
    let escaped = text
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    format!(
        "<html><body><pre style=\"font-family:monospace;white-space:pre-wrap\">{escaped}</pre></body></html>"
    )
}

/// 解析收件人输入框（逗号 / 分号 / 换行分隔，支持 `名字 <地址>` 形态）。
///
/// 放在 core 而不是对话框里：导入配置与手改 `smtp.json` 都绕不过对话框。
pub fn parse_recipients(raw: &str) -> Vec<MailAddress> {
    let mut out = Vec::new();
    for part in raw.split([',', ';', '\n', '\r']) {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        // `名字 <a@b.com>` 形态
        if let Some((name, rest)) = part.split_once('<') {
            let addr = rest.trim_end_matches('>').trim();
            let name = name.trim().trim_matches('"').trim();
            out.push(MailAddress {
                address: addr.to_string(),
                name: name.to_string(),
            });
        } else {
            out.push(MailAddress::new(part));
        }
        if out.len() >= MAX_RECIPIENTS {
            break;
        }
    }
    out
}

/// 收件人列表 -> 输入框文本（与 [`parse_recipients`] 互逆，便于设置界面回显）。
pub fn recipients_to_text(list: &[MailAddress]) -> String {
    list.iter()
        .map(|a| a.formatted())
        .collect::<Vec<_>>()
        .join(", ")
}

/// SMTP 配置文件路径（由调用方传 config 目录，core 不依赖 platform 的路径解析）。
pub fn smtp_path(config_dir: &Path) -> PathBuf {
    config_dir.join("smtp.json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alerts::types::{Notification, Severity};

    fn cfg() -> SmtpConfig {
        SmtpConfig {
            enabled: true,
            host: "smtp.example.com".into(),
            port: 587,
            security: SmtpSecurity::StartTls,
            username: "bot".into(),
            password_cipher: None,
            from: MailAddress {
                address: "rustshell@example.com".into(),
                name: "RustShell".into(),
            },
            to: vec![MailAddress::new("ops@example.com")],
            cc: Vec::new(),
            subject_prefix: DEFAULT_SUBJECT_PREFIX.into(),
            timeout_secs: DEFAULT_SMTP_TIMEOUT_SECS,
            max_retries: DEFAULT_SMTP_RETRIES,
        }
    }

    fn notif() -> Notification {
        Notification {
            title: "[WARNING] 负载超阈值 · prod-03".into(),
            body_text: "内存使用率 94%（9.4G / 10G），阈值 90%\npassword=hunter2".into(),
            body_html: None,
            severity: Severity::Warning,
            task_name: Some("早间巡检".into()),
            occurred_at: 1_700_000_000,
            dedup_key: "k".into(),
        }
    }

    #[test]
    fn default_config_is_disabled_and_uses_587() {
        let c = SmtpConfig::default();
        assert!(
            !c.enabled,
            "默认不启用：用户没配 SMTP 就不该收到任何邮件相关提示"
        );
        assert!(!c.is_ready());
        assert_eq!(c.security, SmtpSecurity::StartTls);
        assert_eq!(c.port, 587);
        assert_eq!(c.effective_subject_prefix(), DEFAULT_SUBJECT_PREFIX);
        assert!(c.validate().is_empty(), "未启用时不校验细节");
    }

    #[test]
    fn security_default_ports() {
        assert_eq!(SmtpSecurity::StartTls.default_port(), 587);
        assert_eq!(SmtpSecurity::Tls.default_port(), 465);
        assert_eq!(SmtpSecurity::None.default_port(), 25);
    }

    #[test]
    fn is_ready_requires_host_from_and_recipient_but_not_password() {
        let mut c = cfg();
        assert!(c.is_ready());
        c.host = "  ".into();
        assert!(!c.is_ready());
        c = cfg();
        c.from = MailAddress::default();
        assert!(!c.is_ready());
        c = cfg();
        c.to.clear();
        assert!(!c.is_ready());
        // 无口令（内网 relay）仍算就绪
        c = cfg();
        assert!(c.password_cipher.is_none() && c.is_ready());
        // 未启用一律不就绪
        c.enabled = false;
        assert!(!c.is_ready());
    }

    #[test]
    fn validate_collects_all_problems_at_once() {
        let mut c = cfg();
        c.host = String::new();
        c.from = MailAddress::new("bad");
        c.to = vec![MailAddress::new("also-bad")];
        let errs = c.validate();
        assert!(errs.len() >= 3, "应一次列出全部问题：{errs:?}");
    }

    #[test]
    fn address_validation_is_lenient_but_catches_typos() {
        assert!(MailAddress::new("a@b.com").is_valid());
        assert!(MailAddress::new("a+b@sub.domain.co").is_valid());
        assert!(MailAddress::new("  x@y.z  ").is_valid());
        assert!(!MailAddress::new("no-at-sign").is_valid());
        assert!(!MailAddress::new("@b.com").is_valid());
        assert!(!MailAddress::new("a@b").is_valid(), "域名必须有点");
        assert!(!MailAddress::new("a@.com").is_valid());
        assert!(!MailAddress::new("a@b.").is_valid());
        assert!(!MailAddress::new("a b@c.com").is_valid());
    }

    #[test]
    fn render_mail_prefixes_subject_and_redacts_body() {
        let m = render_mail(&cfg(), &notif());
        assert!(
            m.subject.starts_with("[RustShell] [WARNING]"),
            "{}",
            m.subject
        );
        assert!(m.body_text.contains("内存使用率"));
        // 口令形态必须被脱敏
        assert!(
            !m.body_text.contains("hunter2"),
            "正文必须脱敏：{}",
            m.body_text
        );
        assert!(m.body_text.contains("***"));
        // HTML 版存在且转义了内容
        let html = m.body_html.as_ref().unwrap();
        assert!(html.contains("<pre"));
        assert!(html.contains("内存使用率"));
    }

    #[test]
    fn html_escapes_markup() {
        let html = text_to_html("a < b & c > d");
        assert!(html.contains("&lt;"));
        assert!(html.contains("&amp;"));
        assert!(html.contains("&gt;"));
        assert!(!html.contains("a < b"));
    }

    #[test]
    fn long_body_is_truncated_on_char_boundary() {
        let mut n = notif();
        n.body_text = "汉".repeat(MAX_MAIL_BODY_BYTES); // 3 字节/字，远超上限
        let m = render_mail(&cfg(), &n);
        assert!(m.body_text.len() < n.body_text.len());
        assert!(m.body_text.contains("截断") || m.body_text.contains("truncated"));
        assert!(!m.body_text.contains('\u{FFFD}'));
    }

    #[test]
    fn parse_and_render_recipients_roundtrip() {
        let list = parse_recipients("ops@example.com, 运维 <oncall@example.com>; dev@x.io\n");
        assert_eq!(list.len(), 3);
        assert_eq!(list[1].address, "oncall@example.com");
        assert_eq!(list[1].name, "运维");
        assert_eq!(list[1].formatted(), "运维 <oncall@example.com>");

        let text = recipients_to_text(&list);
        let back = parse_recipients(&text);
        assert_eq!(back, list, "解析与回显必须互逆");
    }

    #[test]
    fn parse_recipients_ignores_blank_and_caps_count() {
        assert!(parse_recipients("  ,  ;  ").is_empty());
        let many: String = (0..100).map(|i| format!("u{i}@x.com,")).collect::<String>();
        assert_eq!(parse_recipients(&many).len(), MAX_RECIPIENTS);
    }

    #[test]
    fn config_roundtrips_and_survives_broken_file() {
        let dir = std::env::temp_dir().join(format!("rs-smtp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = smtp_path(&dir);

        let c = cfg();
        c.save(&path).unwrap();
        let back = SmtpConfig::load(&path);
        assert_eq!(back, c);

        // 坏文件：留底 + 回退默认
        std::fs::write(&path, "{oops").unwrap();
        let broken = SmtpConfig::load(&path);
        assert!(!broken.enabled);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `SmtpSecurity` 的 serde 表示必须与 `key()` 一致。
    ///
    /// 设置面板用 `key()` 做下拉取值、`serde` 落到 `smtp.json`；两者一旦漂移，
    /// 表现是「界面显示 STARTTLS、文件里存着别的」，而且是静默的。
    #[test]
    fn serde_repr_matches_security_keys() {
        for s in SmtpSecurity::all() {
            let json = serde_json::to_string(&s).unwrap();
            assert_eq!(
                json,
                format!("\"{}\"", s.key()),
                "{s:?} 的 serde 表示与 key() 不一致"
            );
            let back: SmtpSecurity = serde_json::from_str(&json).unwrap();
            assert_eq!(back, s);
        }
    }

    #[test]
    fn legacy_config_without_new_fields_still_parses() {
        let s = r#"{"enabled":true,"host":"smtp.x.com","username":"u"}"#;
        let c: SmtpConfig = serde_json::from_str(s).unwrap();
        assert_eq!(c.host, "smtp.x.com");
        assert_eq!(c.port, 587, "缺端口回退默认");
        assert_eq!(c.security, SmtpSecurity::StartTls);
        assert_eq!(c.timeout_secs, DEFAULT_SMTP_TIMEOUT_SECS);
        assert_eq!(c.max_retries, DEFAULT_SMTP_RETRIES);
        assert_eq!(c.effective_subject_prefix(), DEFAULT_SUBJECT_PREFIX);
    }
}
