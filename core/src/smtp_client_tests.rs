//! 进程内假 SMTP 对端：把协议路径**逐字节**钉住。
//!
//! 为什么不连真实邮件服务器：
//! * 出网依赖会让测试在 CI 上不稳定；
//! * 「发得对不对」的本质是**发出的字节**，而真服务器只回一个 250 —— 断言不了
//!   `MAIL FROM` 的信封地址是不是漏了显示名、`DATA` 里的中文是不是被 base64 好了、
//!   `AUTH LOGIN` 的应答顺序对不对。
//!
//! 对端跑在 `TcpListener` 上，**不出网**（只 bind 127.0.0.1:0），并把收到的每一行
//! 都记进 `Arc<Mutex<Vec<String>>>` 供断言。

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

/// 假对端的剧本：按脚本回话，并记录收到的行。
pub struct FakeSmtp {
    pub port: u16,
    /// 收到的全部客户端行（逐行，已去 CRLF）
    pub received: Arc<Mutex<Vec<String>>>,
    /// `DATA` 之后收到的正文（已按协议解 dot-stuffing）
    pub data_body: Arc<Mutex<String>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl FakeSmtp {
    /// 起一个对端，`caps` 是 EHLO 之后的能力行（如 `["AUTH LOGIN PLAIN"]`）。
    ///
    /// `fail_at`：在第 N 条命令上回 `550`（1-based，含 EHLO）—— 用来测错误分支。
    pub fn start(caps: &[&str], fail_at: Option<usize>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定假 SMTP 端口失败");
        let port = listener.local_addr().expect("取端口失败").port();
        let received = Arc::new(Mutex::new(Vec::new()));
        let data_body = Arc::new(Mutex::new(String::new()));
        listener.set_nonblocking(true).expect("对端设为非阻塞失败");

        let r2 = received.clone();
        let d2 = data_body.clone();
        let caps: Vec<String> = caps.iter().map(|s| s.to_string()).collect();
        let handle = std::thread::spawn(move || {
            // ⚠️ `accept` 也要设超时：某些负例（连不上服务器的）客户端一次都不连，
            // 对端就永远停在 `accept` 上；测试线程 `join()` 它 = 死锁
            // （实测挂满 14 分钟）。故整条「等连接 + 读命令」路径都有上限。
            let Some(sock) = wait_for_client(&listener) else {
                return;
            };
            sock.set_nonblocking(false).ok();
            // 读超时只是**兜底**，不是节奏控制 —— 客户端跑完会 drop 连接，
            // `read_line` 会读到 Ok(0) 正常退出。
            // 取值必须**宽**（30s）：设成 2s 时，本机并行编译其它测试目标会抢走
            // CPU，服务端一个来回没走完就到点，线程退出把连接变成 RST，
            // 客户端读响应时拿到 os error 10053（实测 6 个用例全挂在这）。
            sock.set_read_timeout(Some(std::time::Duration::from_secs(30)))
                .ok();
            let mut w = sock.try_clone().expect("clone 失败");
            let mut reader = BufReader::new(sock);
            let mut idx = 0usize;
            let mut in_auth = false;
            let mut auth_round = 0usize;
            let mut in_data = false;
            let mut body = String::new();

            // 问候
            let _ = w.write_all(b"220 fake.smtp ESMTP ready\r\n");

            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) => {}
                    // 超时 / 连接断开 ⇒ 收工
                    Err(_) => break,
                }
                let line = line.trim_end_matches(['\r', '\n']).to_string();

                if in_data {
                    if line == "." {
                        in_data = false;
                        *d2.lock().unwrap() = std::mem::take(&mut body);
                        let _ = w.write_all(b"250 2.0.0 Ok: queued as FAKE12345\r\n");
                        continue;
                    }
                    // dot-stuffing 的逆运算：行首的一个点要还原掉
                    let un = line.strip_prefix('.').unwrap_or(&line);
                    body.push_str(un);
                    body.push_str("\r\n");
                    continue;
                }

                idx += 1;
                r2.lock().unwrap().push(line.clone());
                let upper = line.to_ascii_uppercase();

                // ⚠️ AUTH LOGIN 的**后续两步**收到的是 base64 串，不以 "AUTH" 开头
                // （用户名 base64 可能是 `dXNlckB4…`）。因此一旦进入认证流程，
                // 必须按**状态**继续推进，不能靠「这行像不像命令」来判断 ——
                // 否则第 2、3 步会落进默认分支被回 `250`，客户端等不到 334，
                // 认证路径的测试就永远失败（且失败得莫名其妙）。
                if in_auth {
                    // ⚠️ AUTH LOGIN 的后两步收到的是 base64 串（用户名/口令），
                    // **不以 "AUTH" 开头**，靠「这行像不像命令」判断会落进默认分支
                    // 被回 250，客户端等 334 时拿到 235 ⇒ 认证被误判为失败。
                    // 必须按状态推进。
                    auth_round += 1;
                    let _ = if auth_round == 1 {
                        w.write_all(b"334 UGFzc3dvcmQ6\r\n")
                    } else {
                        in_auth = false;
                        w.write_all(b"235 2.7.0 Authentication successful\r\n")
                    };
                    continue;
                }

                if let Some(n) = fail_at {
                    if idx == n {
                        let _ = w.write_all(b"550 5.7.1 rejected by policy\r\n");
                        continue;
                    }
                }

                if upper.starts_with("EHLO") {
                    // ⚠️ 最后一行必须是 `250 <text>`（空格结尾）—— 多行响应靠
                    // 第 4 字节区分续行(`-`)与收尾(` `)。caps 为空时若只发
                    // `250-fake.smtp`（连字符结尾），客户端会**一直等下一行**，
                    // 于是超时断连（症状与认证失败完全一样，极难查）。
                    let _ = w.write_all(b"250-fake.smtp greets you\r\n");
                    for c in caps.iter() {
                        let _ = w.write_all(format!("250-{c}\r\n").as_bytes());
                    }
                    let _ = w.write_all(b"250 8BITMIME\r\n");
                } else if upper.starts_with("HELO") {
                    let _ = w.write_all(b"250 fake.smtp\r\n");
                } else if upper.starts_with("AUTH LOGIN") {
                    // 进入认证流程：回 334 问用户名，随后由上面的 `in_auth`
                    // 分支按轮次回 334（问口令）/ 235（成功）。
                    in_auth = true;
                    let _ = w.write_all(b"334 VXNlcm5hbWU6\r\n");
                } else if upper.starts_with("AUTH PLAIN") {
                    let _ = w.write_all(b"235 2.7.0 Authentication successful\r\n");
                } else if upper.starts_with("MAIL FROM") || upper.starts_with("RCPT TO") {
                    let _ = w.write_all(b"250 2.1.0 Ok\r\n");
                } else if upper.starts_with("DATA") {
                    in_data = true;
                    let _ = w.write_all(b"354 End data with <CR><LF>.<CR><LF>\r\n");
                } else if upper.starts_with("QUIT") {
                    let _ = w.write_all(b"221 2.0.0 Bye\r\n");
                    break;
                } else {
                    let _ = w.write_all(b"250 2.0.0 Ok\r\n");
                }
            }
        });

        Self {
            port,
            received,
            data_body,
            handle: Some(handle),
        }
    }

    /// 等对端线程收尾（测试结束前调，避免用例先跑完时端口/线程悬挂）。
    pub fn join(&mut self) {
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }

    pub fn received(&self) -> Vec<String> {
        self.received.lock().unwrap().clone()
    }

    pub fn body(&self) -> String {
        self.data_body.lock().unwrap().clone()
    }
}

impl Drop for FakeSmtp {
    fn drop(&mut self) {
        // 对端线程阻塞在 read_line：主动关掉写端让它读到 EOF 退出，
        // 否则测试进程末尾会挂一个永不退出的线程。
        let _ = TcpStream::connect(("127.0.0.1", self.port));
    }
}

/// 有上限地等一个客户端连上来。
///
/// 监听器设为非阻塞，这里轮询到**最多 3 秒**；等不到就返回 `None`
/// （对端线程安静退出，测试的 `join()` 立刻有回音，不会挂死）。
fn wait_for_client(listener: &TcpListener) -> Option<TcpStream> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while std::time::Instant::now() < deadline {
        match listener.accept() {
            Ok((s, _)) => return Some(s),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(_) => return None,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(port: u16) -> crate::notify_smtp::SmtpConfig {
        crate::notify_smtp::SmtpConfig {
            enabled: true,
            host: "127.0.0.1".to_string(),
            port,
            security: crate::notify_smtp::SmtpSecurity::None,
            from: crate::notify_smtp::MailAddress::new("rustshell@example.com"),
            to: vec![crate::notify_smtp::MailAddress::new("ops@example.com")],
            subject_prefix: "[RustShell]".to_string(),
            ..Default::default()
        }
    }

    fn mail() -> crate::notify_smtp::RenderedMail {
        crate::notify_smtp::RenderedMail {
            from: crate::notify_smtp::MailAddress::new("rustshell@example.com"),
            to: vec![crate::notify_smtp::MailAddress::new("ops@example.com")],
            cc: vec![],
            subject: "[CRIT] 磁盘空间不足".to_string(),
            body_text: "机器 web-01 的 / 分区已用 92%".to_string(),
            body_html: None,
        }
    }

    /// 完整明文投递：协议顺序 + 信封地址 + 收件人 + 正文。
    #[test]
    fn plaintext_delivery_speaks_correct_smtp() {
        let mut fake = FakeSmtp::start(&[], None);
        let report =
            crate::smtp_client::send_mail(&cfg(fake.port), &mail(), "", "").expect("投递应当成功");
        fake.join();

        let got = fake.received();
        // 协议顺序必须严格是 EHLO → MAIL → RCPT → DATA → .
        // 用 `position` 而不是写死下标：无认证的中继不会发 AUTH，
        // 硬编码下标会让「多了/少了一行」变成难懂的「这一行不对」。
        let at = |needle: &str| {
            got.iter()
                .position(|l| l.to_ascii_uppercase().starts_with(needle))
                .unwrap_or_else(|| panic!("没收到 `{needle}`，实际收到：{got:?}"))
        };
        let i_ehlo = at("EHLO");
        let i_mail = at("MAIL FROM");
        let i_rcpt = at("RCPT TO");
        let i_data = at("DATA");
        assert!(
            i_ehlo < i_mail && i_mail < i_rcpt && i_rcpt < i_data,
            "协议顺序不对（EHLO → MAIL → RCPT → DATA）：{got:?}"
        );
        assert_eq!(
            got[i_mail].to_ascii_uppercase(),
            "MAIL FROM:<RUSTSHELL@EXAMPLE.COM>",
            "MAIL FROM 应是纯信封地址（不能带显示名）"
        );
        assert_eq!(
            got[i_rcpt].to_ascii_uppercase(),
            "RCPT TO:<OPS@EXAMPLE.COM>"
        );
        assert_eq!(got[i_data].to_ascii_uppercase(), "DATA");
        // `.` 终止符由对端在 DATA 段内消费（不进 `received`），但它**必须存在**：
        // 对端能正常回 250 且拿到正文，就是「收到了终止符」的证据。
        assert!(
            !fake.body().is_empty(),
            "正文为空说明 DATA 段没收到内容（`.` 终止符可能没发出去）"
        );

        // 报告
        assert_eq!(report.recipients, vec!["ops@example.com".to_string()]);
        assert!(!report.used_tls, "明文传输不该标记 tls");
        assert_eq!(report.queue_id.as_deref(), Some("FAKE12345"));

        // 正文：中文主题必须 RFC 2047 编码，且 charset 显式声明
        let body = fake.body();
        assert!(
            body.contains("Subject: =?UTF-8?B?"),
            "中文主题未编码：{body}"
        );
        assert!(body.contains("charset=utf-8"), "缺 charset 声明：{body}");
        assert!(
            body.contains("Content-Transfer-Encoding: base64"),
            "中文正文未编码：{body}"
        );
    }

    /// 中文内容必须能**解码回原文** —— 编码对了但编错了同样收不到人看的邮件。
    #[test]
    fn chinese_content_survives_the_round_trip() {
        let mut fake = FakeSmtp::start(&[], None);
        crate::smtp_client::send_mail(&cfg(fake.port), &mail(), "", "").expect("投递应当成功");
        fake.join();

        let body = fake.body();
        // 抽出主题头并解码
        let subject_line = body
            .lines()
            .find(|l| l.starts_with("Subject: "))
            .expect("缺 Subject 头");
        let b64 = subject_line
            .trim_start_matches("Subject: =?UTF-8?B?")
            .trim_end_matches("?=");
        let decoded =
            String::from_utf8(crate::smtp_wire::base64_decode(b64).expect("主题 base64 解码失败"))
                .expect("主题解码后应是 UTF-8");
        assert_eq!(decoded, "[CRIT] 磁盘空间不足");

        // 正文段落同样解回来（跳过头部的空行，正文就在其后）
        let payload: String = body
            .lines()
            .skip_while(|l| !l.is_empty())
            .skip(1)
            .collect::<Vec<_>>()
            .join("");
        let plain = String::from_utf8(
            crate::smtp_wire::base64_decode(&payload).expect("正文 base64 解码失败"),
        )
        .expect("正文解码后应是 UTF-8");
        assert!(
            plain.contains("机器 web-01 的 / 分区已用 92%"),
            "正文解码后内容不对：{plain}"
        );
    }

    /// 认证：优先 AUTH LOGIN，且口令走 base64（不裸奔）。
    #[test]
    fn auth_login_is_preferred_when_supported() {
        let mut fake = FakeSmtp::start(&["AUTH LOGIN PLAIN"], None);
        let mut c = cfg(fake.port);
        c.username = "user@example.com".to_string();
        crate::smtp_client::send_mail(&c, &mail(), "s3cr3t", "").expect("投递应当成功");
        fake.join();

        let got = fake.received();
        let idx = got
            .iter()
            .position(|l| l.eq_ignore_ascii_case("AUTH LOGIN"))
            .expect("应当优先用 AUTH LOGIN");
        // 用户名与口令各自一行 base64，且都不得是明文
        assert!(
            got[idx + 1] != "user@example.com",
            "用户名不该明文发送：{:?}",
            got[idx + 1]
        );
        assert!(
            !got.iter().any(|l| l == "s3cr3t"),
            "口令绝不能出现在协议行里（明文或裸 base64 都不行）：{got:?}"
        );
        // base64("s3cr3t") = czNjcjN0
        assert!(
            got[idx + 2] == "czNjcjN0",
            "口令应答应为 base64：{:?}",
            got[idx + 2]
        );
    }

    /// 只支持 PLAIN 时退回 AUTH PLAIN。
    #[test]
    fn falls_back_to_auth_plain_when_login_unsupported() {
        let mut fake = FakeSmtp::start(&["AUTH PLAIN"], None);
        let mut c = cfg(fake.port);
        c.username = "u@e.com".to_string();
        crate::smtp_client::send_mail(&c, &mail(), "pw", "").expect("投递应当成功");
        fake.join();

        let got = fake.received();
        assert!(
            got.iter().any(|l| l.starts_with("AUTH PLAIN ")),
            "应当退回 AUTH PLAIN：{got:?}"
        );
        assert!(
            !got.iter().any(|l| l.eq_ignore_ascii_case("AUTH LOGIN")),
            "服务器不支持 LOGIN 时不该先试它：{got:?}"
        );
    }

    /// 无口令时不发 AUTH（无认证中继是合法配置，不能硬发 AUTH 导致被拒）。
    #[test]
    fn no_password_means_no_auth_attempt() {
        let mut fake = FakeSmtp::start(&["AUTH LOGIN"], None);
        crate::smtp_client::send_mail(&cfg(fake.port), &mail(), "", "").expect("投递应当成功");
        fake.join();

        assert!(
            !fake.received().iter().any(|l| l.starts_with("AUTH")),
            "无口令时不该尝试认证：{:?}",
            fake.received()
        );
    }

    /// 服务器拒绝时必须**报错**，不能假装成功。
    #[test]
    fn server_rejection_surfaces_as_error() {
        // 第 3 条命令（EHLO=1, MAIL=2, RCPT=3）被拒
        let mut fake = FakeSmtp::start(&[], Some(3));
        let r = crate::smtp_client::send_mail(&cfg(fake.port), &mail(), "", "");
        fake.join();
        let err = r.expect_err("被拒时必须返回错误");
        assert!(
            matches!(err, crate::smtp_client::SendError::Rejected(_)),
            "应是 Rejected，实际：{err:?}"
        );
        assert!(
            err.to_string().contains("rejected"),
            "错误文案要带服务器原因：{err}"
        );
    }

    /// 抄送也要发 RCPT TO（漏掉就等于静默丢抄送）。
    #[test]
    fn cc_recipients_get_their_own_rcpt_to() {
        let mut fake = FakeSmtp::start(&[], None);
        let c = cfg(fake.port);
        let mut m = mail();
        m.cc = vec![crate::notify_smtp::MailAddress::new("boss@example.com")];
        let report = crate::smtp_client::send_mail(&c, &m, "", "").expect("投递应当成功");
        fake.join();

        let got = fake.received();
        assert!(
            got.iter().any(|l| l
                .to_ascii_uppercase()
                .contains("RCPT TO:<BOSS@EXAMPLE.COM>")),
            "抄送漏了 RCPT TO：{got:?}"
        );
        assert_eq!(report.recipients.len(), 2, "报告应含收件人与抄送");
    }

    /// 没有收件人时必须拒发（不然 MAIL FROM 之后就 DATA，等于把邮件丢进黑洞）。
    #[test]
    fn empty_recipient_list_is_refused_before_sending() {
        let mut fake = FakeSmtp::start(&[], None);
        let c = cfg(fake.port);
        let mut m = mail();
        m.to.clear();
        let r = crate::smtp_client::send_mail(&c, &m, "", "");
        fake.join();
        assert!(r.is_err(), "没有收件人必须报错");
    }

    /// STARTTLS 是**必须**的：配置了它而服务器不支持时，不得静默降级明文。
    #[test]
    fn starttls_config_refuses_to_downgrade() {
        // 对端不宣告 STARTTLS
        let mut fake = FakeSmtp::start(&["8BITMIME"], None);
        let mut c = cfg(fake.port);
        c.security = crate::notify_smtp::SmtpSecurity::StartTls;
        let r = crate::smtp_client::send_mail(&c, &mail(), "", "");
        fake.join();

        let err = r.expect_err("服务器不支持 STARTTLS 时必须失败而不是明文发送");
        assert!(
            matches!(err, crate::smtp_client::SendError::Tls(_)),
            "应是 TLS 类错误，实际：{err:?}"
        );
        // 关键：不得已经发出了 MAIL FROM（有内容就等于泄了）
        assert!(
            !fake
                .received()
                .iter()
                .any(|l| l.to_ascii_uppercase().starts_with("MAIL FROM")),
            "降级失败前就把邮件内容发出去了：{:?}",
            fake.received()
        );
    }

    /// 连不上服务器时错误要说清是「连接」问题，且不得 panic。
    #[test]
    fn connection_failure_is_reported_cleanly() {
        // 先起一个假对端拿到端口，随即关掉 ⇒ 端口几乎必然连不上
        let mut fake = FakeSmtp::start(&[], None);
        let port = fake.port;
        fake.join();
        let c = cfg(port);
        let r = crate::smtp_client::send_mail(&c, &mail(), "", "");
        match r {
            Err(crate::smtp_client::SendError::Connect(_)) => {}
            other => panic!("应当是连接类错误，实际：{other:?}"),
        }
    }
}
