//! SMTP 客户端：协议会话、STARTTLS / 隐式 TLS、认证与投递。
//!
//! 自建而非引第三方邮件库的理由：本项目的 SMTP 面很窄（一次一封、纯文本为主），
//! 而引入 `lettre` 会带进一整套它自己的传输抽象；更要紧的是**可测性** ——
//! 协议层要能用「进程内假对端」逐字节断言（见本模块的 `tests`），
//! 第三方库把 socket 封在内部，这条断言链就断了。
//!
//! ## 安全设计
//!
//! * **STARTTLS 失败必须致命**：明文 SMTP 上发预警邮件意味着告警内容（可能含主机名、
//!   磁盘路径、内部 IP）裸奔。配置写了 STARTTLS 却悄悄降级明文，等于用户以为加密了。
//!   故 [`SmtpSecurity::StartTls`] 下握手失败一律报错，不 fallback（铁律：无声失败是坑）。
//! * **`None` 是用户显式选择**：内网 relay 常见，UI 上要给出提示；此时不假装加密。
//! * 证书校验用 rustls 的**原生根证书集**，关闭「跳过校验」开关 —— 预警邮件走的是
//!   用户的真实邮箱，证书链校验失败就必须失败。
//!
//! ## 不做的事
//!
//! 不支持 `PLAIN` 以外的 SASL（OAuth2/XOAUTH2），不支持附件、不支持 DKIM 签名。
//! 够用即可：预警邮件是纯文本通知。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

use crate::notify_smtp::{RenderedMail, SmtpConfig, SmtpSecurity};

/// 单次投递的结果摘要（写进审计与 UI 提示）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryReport {
    /// 实际投递的收件人（含抄送）
    pub recipients: Vec<String>,
    /// 服务器返回的 `250` 里的队列 id（若有）
    pub queue_id: Option<String>,
    /// 走的加密方式（明文时是 `None`，日志里要如实写出来）
    pub used_tls: bool,
}

/// 投递失败。`Display` 直接给用户看，故文案要说清「哪一步失败」。
#[derive(Debug)]
pub enum SendError {
    Connect(String),
    Protocol(String),
    Tls(String),
    Auth(String),
    Rejected(String),
    Timeout,
}

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Connect(e) => write!(f, "连接 SMTP 服务器失败：{e}"),
            Self::Protocol(e) => write!(f, "SMTP 协议错误：{e}"),
            Self::Tls(e) => write!(f, "TLS 握手失败：{e}"),
            Self::Auth(e) => write!(f, "认证失败：{e}"),
            Self::Rejected(e) => write!(f, "服务器拒绝投递：{e}"),
            Self::Timeout => write!(f, "SMTP 超时"),
        }
    }
}

/// 口令解密器：由宿主注入（core 不依赖 platform，见 `Settings::runtime_proxy`）。
///
/// 要求 `Send + Sync`：邮件通道会被装进 `Arc<dyn NotifyChannel>` 跨线程共享，
/// 闭包不带这两个 bound 就装不进去。
pub type Unprotect = dyn Fn(&str) -> Option<String> + Send + Sync;

/// 投递一封邮件。
///
/// `password` 由调用方解密后传入 —— core 拿不到 DPAPI/keyring，故**不**在这里
/// 调 `platform::secret`。留空串表示「不认证」（无认证中继是合法配置）。
pub fn send_mail(
    cfg: &SmtpConfig,
    mail: &RenderedMail,
    password: &str,
    unprotect_note: &str,
) -> Result<DeliveryReport, SendError> {
    let timeout = Duration::from_secs(cfg.timeout_secs.clamp(1, 300));
    let host = cfg.host.trim();
    if host.is_empty() {
        return Err(SendError::Connect("服务器地址为空".into()));
    }

    // 隐式 TLS：一上来就握手（465）。明文/STARTTLS 走普通 TCP。
    let mut conn = match cfg.security {
        SmtpSecurity::Tls => Conn::connect_implicit_tls(host, cfg.port, timeout)?,
        SmtpSecurity::None | SmtpSecurity::StartTls => {
            Conn::from_stream(Conn::connect_tcp(host, cfg.port, timeout)?)?
        }
    };

    let mut used_tls = cfg.security == SmtpSecurity::Tls;
    conn.expect_multi("220")?;

    // EHLO：先试 ESMTP，拿不到再退回 HELO（老服务器只认 HELO）。
    let caps = match conn.cmd_multi("EHLO rustshell.local") {
        Ok(lines) => lines,
        Err(_) => {
            conn.cmd("HELO rustshell.local")?;
            Vec::new()
        }
    };

    if cfg.security == SmtpSecurity::StartTls {
        // 服务端不支持 STARTTLS 时**不**静默降级 —— 那等于把内容明文发出去。
        if !caps
            .iter()
            .any(|l| capability(l).eq_ignore_ascii_case("STARTTLS"))
        {
            return Err(SendError::Tls(
                "服务器不支持 STARTTLS（配置为 STARTTLS，拒绝降级明文发送）".into(),
            ));
        }
        conn.cmd("STARTTLS")?;
        conn.upgrade_tls(host)?;
        used_tls = true;
        // RFC 3207：握手后**必须**重发 EHLO —— 握手前的能力表已作废。
        let caps = match conn.cmd_multi("EHLO rustshell.local") {
            Ok(lines) => lines,
            Err(_) => {
                conn.cmd("HELO rustshell.local")?;
                Vec::new()
            }
        };
        if auth_needed(&caps) && !password.is_empty() {
            authenticate(&mut conn, &caps, cfg, password)?;
        }
    } else if auth_needed(&caps) && !password.is_empty() {
        authenticate(&mut conn, &caps, cfg, password)?;
    }

    // 发件人。SMTP 的 MAIL FROM 用**信封地址**（纯地址、无显示名）。
    conn.cmd(&format!("MAIL FROM:<{}>", cfg.from.address.trim()))?;

    let mut recipients = Vec::new();
    for r in mail.to.iter().chain(mail.cc.iter()) {
        conn.cmd(&format!("RCPT TO:<{}>", r.address.trim()))?;
        recipients.push(r.address.trim().to_string());
    }
    if recipients.is_empty() {
        return Err(SendError::Protocol("没有收件人".into()));
    }

    conn.cmd("DATA")?;
    let date = rfc2822_date();
    // boundary 用时间戳 + 收件人数：同一封里唯一即可，不必上随机数发生器
    //（测试要能断言它，故取可预测的值）。
    let boundary = format!("rs-{}", crate::connections::unix_now_secs());
    let msg = crate::smtp_wire::build_message(
        &mail.subject,
        &mail.body_text,
        mail.body_html.as_deref(),
        &boundary,
        &date,
    );
    let stuffed = crate::smtp_wire::dot_stuff(&msg);
    conn.write_line(&stuffed)?;
    conn.write_line(".")?;

    let final_lines = conn.read_reply_multi()?;
    // 队列 id 取 `250 … queued as <ID>` 里的最后一段；没有「queued as」时
    // 退回整行的第三段（部分服务器只回 `250 2.0.0 <id>`）。
    let queue_id = final_lines.iter().find_map(|l| {
        if !l.starts_with("250") {
            return None;
        }
        l.split("queued as ")
            .nth(1)
            .map(|s| s.trim().to_string())
            .or_else(|| l.split_whitespace().nth(2).map(|s| s.to_string()))
    });
    if !final_lines.iter().any(|l| l.starts_with("250")) {
        let msg = final_lines.join("; ");
        return Err(SendError::Rejected(msg));
    }

    // QUIT 是礼仪而非必须，失败也不影响投递结果。
    let _ = conn.cmd("QUIT");
    let _ = unprotect_note;
    Ok(DeliveryReport {
        recipients,
        queue_id,
        used_tls,
    })
}

/// 服务端是否要求认证。
fn auth_needed(caps: &[String]) -> bool {
    caps.iter().any(|l| {
        let cap = capability(l);
        let cap = cap.to_ascii_uppercase();
        cap.starts_with("AUTH") && (cap.contains("PLAIN") || cap.contains("LOGIN"))
    })
}

/// 从一行 `250 AUTH LOGIN PLAIN` 里取出**能力关键字**（去掉回复码与分隔符）。
///
/// ⚠️ 必须剥掉 `250-` / `250 ` 前缀：EHLO 的每一行能力都带回复码，
/// 直接对整行做 `starts_with("AUTH")` 永远不成立 ⇒ 认证被静默跳过
/// （2026-10-12 由假对端测试当场抓出：服务端宣告了 AUTH，客户端却不认证）。
fn capability(line: &str) -> &str {
    let rest = line.get(3..).unwrap_or("");
    rest.trim_start_matches(['-', ' ']).trim()
}

/// 认证：优先 `AUTH LOGIN`（几乎所有服务器都支持），退回 `AUTH PLAIN`。
fn authenticate(
    conn: &mut Conn,
    caps: &[String],
    cfg: &SmtpConfig,
    password: &str,
) -> Result<(), SendError> {
    let user = cfg.username.trim();
    if user.is_empty() {
        return Err(SendError::Auth("配置了口令但用户名为空".into()));
    }
    let supports = |mech: &str| {
        caps.iter()
            .any(|l| capability(l).to_ascii_uppercase().contains(mech))
    };
    let b64 = crate::smtp_wire::base64_encode;

    if supports("LOGIN") {
        // AUTH LOGIN 是多步握手，不能走 `cmd()`（第一步回复是 334 而非成功码）
        conn.write_line("AUTH LOGIN")?;
        conn.expect_multi("334")?;
        conn.write_line(&b64(user.as_bytes()))?;
        conn.expect_multi("334")?;
        conn.write_line(&b64(password.as_bytes()))?;
        let reply = conn.read_reply_multi()?;
        if !reply.iter().any(|l| l.starts_with("235")) {
            return Err(SendError::Auth(reply.join("; ")));
        }
        return Ok(());
    }

    if supports("PLAIN") {
        // PLAIN 的载荷是 authzid\0authcid\0passwd，全用明文口令。
        let mut payload = String::new();
        payload.push('\0');
        payload.push_str(user);
        payload.push('\0');
        payload.push_str(password);
        let line = conn.cmd_multi(&format!("AUTH PLAIN {}", b64(payload.as_bytes())))?;
        if !line.iter().any(|l| l.starts_with("235")) {
            return Err(SendError::Auth(line.join("; ")));
        }
        return Ok(());
    }

    Err(SendError::Auth(
        "服务器只支持本程序未实现的认证机制（仅支持 LOGIN / PLAIN）".into(),
    ))
}

/// RFC 2822 日期头（`Date: Mon, 12 Oct 2026 03:00:00 +0000`）。
fn rfc2822_date() -> String {
    chrono::Utc::now()
        .format("%a, %d %b %Y %H:%M:%S +0000")
        .to_string()
}

// ---------------------------------------------------------------------------
// 连接：裸 TCP 或 TLS；STARTTLS 后可原地升级
// ---------------------------------------------------------------------------

/// 传输层：明文 TCP 或 TLS。
///
/// 两者对上层是同一套 `Read`/`Write`，但**类型不同** ⇒ 必须用枚举而不是 trait
/// object：SMTP 的读写是逐行的小 I/O，动态分发没有收益，而枚举让
/// 「STARTTLS 后原地换掉传输」这件事保持直白。
///
/// TLS 变体**装箱**（clippy `large_enum_variant`）：`StreamOwned` 带整个
/// `ClientConnection`（~1KB 状态），不装的话每个 `Conn` 都要按最大的那个变体留空间，
/// 而明文连接（本仓最常见的内网 relay 场景）白白占着 1KB。
enum Transport {
    Plain(TcpStream),
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>),
}

impl Transport {
    fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        match self {
            Self::Plain(s) => s.write_all(buf),
            Self::Tls(t) => t.write_all(buf),
        }
    }
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Plain(s) => s.read(buf),
            Self::Tls(t) => t.read(buf),
        }
    }
    /// 升级：把明文 `TcpStream` 换成 TLS 流。
    ///
    /// 必须在**没有缓冲残留**时调用 —— 调用方负责重建 `BufReader`。
    fn into_tls(self, host: &str) -> Result<Transport, SendError> {
        match self {
            Self::Plain(s) => {
                let conn = tls_connect(host)?;
                Ok(Self::Tls(Box::new(rustls::StreamOwned::new(conn, s))))
            }
            // 已加密的连接没有「再加密」的道理
            Self::Tls(_) => Err(SendError::Tls("连接已经是 TLS".into())),
        }
    }
}

/// `BufReader<Transport>` 要求 `Transport: Read` —— 手写实现转发给两个变体。
impl Read for Transport {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        Transport::read(self, buf)
    }
}

/// SMTP 连接（行协议 + 多行响应解析）。
struct Conn {
    reader: BufReader<Transport>,
}

impl Conn {
    fn from_stream(stream: TcpStream) -> Result<Self, SendError> {
        Ok(Self {
            reader: BufReader::new(Transport::Plain(stream)),
        })
    }

    fn connect_tcp(host: &str, port: u16, timeout: Duration) -> Result<TcpStream, SendError> {
        let addr = resolve(host, port)?;
        let stream = TcpStream::connect_timeout(&addr, timeout)
            .map_err(|e| SendError::Connect(e.to_string()))?;
        stream.set_read_timeout(Some(timeout)).ok();
        stream.set_write_timeout(Some(timeout)).ok();
        stream.set_nodelay(true).ok();
        Ok(stream)
    }

    /// 隐式 TLS（465）：连上就握手。
    fn connect_implicit_tls(host: &str, port: u16, timeout: Duration) -> Result<Self, SendError> {
        let stream = Self::connect_tcp(host, port, timeout)?;
        let t = Transport::Plain(stream).into_tls(host)?;
        Ok(Self {
            reader: BufReader::new(t),
        })
    }

    /// STARTTLS 之后原地升级。
    ///
    /// ⚠️ 必须**重建 `BufReader`**：旧 reader 可能已经缓存了握手前的明文响应，
    /// 留在缓冲里会让 TLS 握手后的第一个字节读到脏数据（表现为「随机字节」或
    /// 直接协议错乱）。`into_inner()` 把传输层拿回来时也顺手丢掉缓存。
    fn upgrade_tls(&mut self, host: &str) -> Result<(), SendError> {
        let transport = std::mem::replace(
            &mut self.reader,
            BufReader::new(Transport::Plain(placeholder_stream()?)),
        )
        .into_inner();
        let t = transport.into_tls(host)?;
        self.reader = BufReader::new(t);
        Ok(())
    }

    /// 读完服务器的多行响应（以 `NNN ` 收尾，`NNN-` 是续行）。
    fn read_reply_multi(&mut self) -> Result<Vec<String>, SendError> {
        let mut lines = Vec::new();
        loop {
            let mut line = String::new();
            let n = self
                .reader
                .read_line(&mut line)
                .map_err(|e| map_io(e, "读取响应"))?;
            if n == 0 {
                return Err(SendError::Protocol("连接被服务器关闭".into()));
            }
            let t = line.trim_end_matches(['\r', '\n']).to_string();
            lines.push(t.clone());
            // 第 4 字节是空格表示这条是最后一行；`-` 表示还有后续行。
            if t.len() >= 4 && t.as_bytes()[3] == b' ' {
                return Ok(lines);
            }
            if lines.len() > 64 {
                return Err(SendError::Protocol("响应行数异常".into()));
            }
        }
    }

    fn write_line(&mut self, line: &str) -> Result<(), SendError> {
        // SMTP 是 CRLF 结尾；用 \n 的话部分服务器会一直等后半行。
        self.reader
            .get_mut()
            .write_all(format!("{line}\r\n").as_bytes())
            .map_err(|e| map_io(e, "写入命令"))
    }

    /// 发一条命令，校验回复码。
    ///
    /// 期望码按**命令动词**取（`DATA` 期待 354 而不是 250）—— 用命令自身的
    /// 助记符去比对回复行会把这层语义弄丢。
    fn cmd(&mut self, line: &str) -> Result<Vec<String>, SendError> {
        let verb = line
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_uppercase();
        let want: u16 = match verb.as_str() {
            "EHLO" | "HELO" | "MAIL" | "RCPT" => 250,
            "DATA" => 354,
            "QUIT" => 221,
            // AUTH LOGIN / AUTH PLAIN 都是「先问再答」的多步握手，**不能**用
            // `cmd()`：它的第一步回复是 334（继续提示），不是最终结果。
            // 真正的成功码由 `authenticate` 在收到应答后单独校验。
            "AUTH" => 334,
            other => return Err(SendError::Protocol(format!("未知命令 {other}"))),
        };
        self.write_line(line)?;
        let reply = self.read_reply_multi()?;
        if reply.iter().any(|l| l.starts_with(&want.to_string())) {
            Ok(reply)
        } else {
            Err(SendError::Rejected(reply.join("; ")))
        }
    }

    /// 发命令并返回**全部**响应行（EHLO 的能力表要用）。
    fn cmd_multi(&mut self, line: &str) -> Result<Vec<String>, SendError> {
        self.write_line(line)?;
        self.read_reply_multi()
    }

    fn expect_multi(&mut self, code: &str) -> Result<Vec<String>, SendError> {
        let reply = self.read_reply_multi()?;
        if reply.iter().any(|l| l.starts_with(code)) {
            Ok(reply)
        } else {
            Err(SendError::Rejected(reply.join("; ")))
        }
    }
}

/// 仅用于「换出 transport 时临时占位」的无效流。
///
/// 它会在同一函数里立刻被替换掉（`upgrade_tls` 的下一行就是重建 reader），
/// 所以这里用 `panic!` 而**不是**真的去建一个连接 —— 建连接会让
/// 「升级失败」变成「连了个没用的第二台机器」。
fn placeholder_stream() -> Result<TcpStream, SendError> {
    Err(SendError::Protocol("内部状态错误：传输层被意外取走".into()))
}

fn map_io(e: std::io::Error, what: &str) -> SendError {
    match e.kind() {
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => SendError::Timeout,
        _ => SendError::Protocol(format!("{what}失败：{e}")),
    }
}

/// `host:port` → socket 地址（支持 IP 字面量与域名）。
fn resolve(host: &str, port: u16) -> Result<std::net::SocketAddr, SendError> {
    use std::net::ToSocketAddrs;
    (host, port)
        .to_socket_addrs()
        .map_err(|e| SendError::Connect(format!("无法解析 {host}:{e}")))?
        .next()
        .ok_or_else(|| SendError::Connect(format!("{host} 未解析到任何地址")))
}

/// TLS 客户端：系统根证书集 + 完整证书链校验。
///
/// **不提供**「跳过校验」开关：预警邮件走用户的真实邮箱，证书链不对就必须失败。
/// （配置里也没有这个选项，故代码里也不留后门。）
fn tls_connect(host: &str) -> Result<rustls::ClientConnection, SendError> {
    let mut roots = rustls::RootCertStore::empty();
    let loaded = rustls_native_certs::load_native_certs();
    let (added, ignored) = roots.add_parsable_certificates(loaded.certs);
    if added == 0 {
        return Err(SendError::Tls(format!(
            "系统根证书为空（读取到 {added} 张，忽略 {ignored} 张），无法校验服务器证书"
        )));
    }
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let server_name = rustls::pki_types::ServerName::try_from(host.to_string())
        .map_err(|_| SendError::Tls(format!("{host} 不是合法的主机名（无法作为 SNI）")))?;
    rustls::ClientConnection::new(Arc::new(config), server_name)
        .map_err(|e| SendError::Tls(e.to_string()))
}
