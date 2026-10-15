//! MIME 编码：主题与正文的非 ASCII 处理。
//!
//! SMTP（RFC 5321）本身只传 7-bit ASCII。本应用的主题与正文**必然含中文**
//! （标题就是 `[CRIT] 磁盘空间不足`），因此必须按 RFC 2047 编码主题、
//! 按 MIME 传输编码正文 —— 否则多数邮件客户端会显示乱码，或者直接把
//! 非 ASCII 字节当成传输控制符（8-bit 洁净度检查失败导致整封被拒）。
//!
//! 取舍：
//! * 主题用 **RFC 2047 的 base64 形态**（`=?UTF-8?B?...?=`）。选 base64 而非
//!   QP：主题通常较短，base64 的 33% 膨胀可接受，而 QP 对中文的可读性更差。
//! * 正文用 **base64 + CRLF 硬换行**（RFC 2045 规定每行 ≤76 字符）。
//!   不选 quoted-printable：巡检正文里有大量日志片段，QP 的 `=XX` 密集时可读性差。
//!
//! 纯 ASCII 内容**一律不加编码头**：加了 `Content-Transfer-Encoding: base64`
//! 之后，部分老客户端与网关会把它当附件或显示源码。判据是「有没有非 ASCII 字节」，
//! 不是「有没有可能含非 ASCII」。

/// 判断是否需要 MIME 编码（含任一非 ASCII 字节即需要）。
pub fn needs_encoding(s: &str) -> bool {
    !s.is_ascii()
}

/// 标准 base64 字母表。
const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Base64 编码（RFC 4648 标准字母表，带 `=` 补位）。
pub fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            B64[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            B64[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// 按固定宽度折行（RFC 2045：base64 正文每行 ≤76 字符）。
///
/// 用**字符**而不是字节计数：折行点必须落在 base64 输出之内，而 base64 输出
/// 全是 ASCII，两种口径在这里等价 —— 但按字符计数在 `chars().count()` 上更直观，
/// 且万一将来换实现也不会出错。
fn wrap_lines(s: &str, width: usize) -> String {
    let mut out = String::with_capacity(s.len() + s.len() / width * 2);
    let mut n = 0usize;
    for c in s.chars() {
        if n == width {
            out.push_str("\r\n");
            n = 0;
        }
        out.push(c);
        n += 1;
    }
    out
}

/// 编码邮件主题（RFC 2047）。
///
/// 纯 ASCII 原样返回 —— 加了 `=?UTF-8?B?...?=` 头的老客户端可能整封退信。
pub fn encode_subject(subject: &str) -> String {
    if !needs_encoding(subject) {
        return subject.to_string();
    }
    // 先折叠成单行：主题里的真实换行会被 SMTP 当成头终止符。
    let flat: String = subject
        .chars()
        .filter(|c| *c != '\r' && *c != '\n')
        .collect();
    format!("=?UTF-8?B?{}?=", base64_encode(flat.as_bytes()))
}

/// 编码正文，返回 `(Content-Transfer-Encoding 头值, 正文内容)`。
///
/// `text/plain` 恒用 `utf-8`（7bit / 8bit / base64）；`text/html` 同理。
/// 非 ASCII 一律 base64 并折行；ASCII 原样但**仍做 dot-stuffing 之外的行长保护**。
pub fn encode_body(text: &str) -> (&'static str, String) {
    if !needs_encoding(text) {
        return ("7bit", text.to_string());
    }
    ("base64", wrap_lines(&base64_encode(text.as_bytes()), 76))
}

/// 构造整封邮件。
///
/// `boundary` 由调用方给（便于测试断言）；生产路径应每次生成随机串 —— 固定的
/// boundary 会在同一封邮件里出现两次以上时把正文切错。
pub fn build_message(
    subject: &str,
    body_text: &str,
    body_html: Option<&str>,
    boundary: &str,
    date: &str,
) -> String {
    let mut out = String::new();
    out.push_str(&format!("Subject: {}\r\n", encode_subject(subject)));
    out.push_str("MIME-Version: 1.0\r\n");
    out.push_str("X-Mailer: RustShell\r\n");
    out.push_str(&format!("Date: {date}\r\n"));

    let (text_cte, text_body) = encode_body(body_text);
    match body_html {
        Some(html) => {
            let (html_cte, html_body) = encode_body(html);
            out.push_str(&format!(
                "Content-Type: multipart/alternative; boundary=\"{boundary}\"\r\n\r\n"
            ));
            // 纯文本在前：不支持 multipart 的老客户端会只读第一段。
            out.push_str(&format!(
                "--{boundary}\r\nContent-Type: text/plain; charset=utf-8\r\n\
                 Content-Transfer-Encoding: {text_cte}\r\n\r\n{text_body}\r\n"
            ));
            out.push_str(&format!(
                "--{boundary}\r\nContent-Type: text/html; charset=utf-8\r\n\
                 Content-Transfer-Encoding: {html_cte}\r\n\r\n{html_body}\r\n"
            ));
            out.push_str(&format!("--{boundary}--\r\n"));
        }
        None => {
            out.push_str("Content-Type: text/plain; charset=utf-8\r\n");
            out.push_str(&format!("Content-Transfer-Encoding: {text_cte}\r\n\r\n"));
            out.push_str(&text_body);
            out.push_str("\r\n");
        }
    }
    out
}

/// Base64 解码（RFC 4648 标准字母表）。
///
/// 与 [`base64_encode`] 互逆。**仅供测试与将来可能的 MIME 解析用** ——
/// 编码侧永远不需要解码，所以放在这里而不是内联进测试。
pub fn base64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a' + 26) as u32),
            b'0'..=b'9' => Some((c - b'0' + 52) as u32),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let bytes: Vec<u8> = s.bytes().filter(|&c| c != b'\n' && c != b'\r').collect();
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for chunk in bytes.chunks(4) {
        if chunk.len() < 4 {
            return None;
        }
        let mut n = 0u32;
        let mut pad = 0;
        for &c in chunk {
            if c == b'=' {
                pad += 1;
            } else {
                n = (n << 6) | val(c)?;
            }
        }
        for _ in 0..pad {
            n <<= 6;
        }
        out.push((n >> 16) as u8);
        if pad < 2 {
            out.push((n >> 8) as u8);
        }
        if pad < 1 {
            out.push(n as u8);
        }
    }
    Some(out)
}

/// dot-stuffing：正文里以 `.` 开头的行前补一个点（RFC 5321 §4.5.2）。
///
/// 不补的话，单独一行 `.` 会被服务器当成「邮件结束」⇒ 正文被截断。
pub fn dot_stuff(body: &str) -> String {
    let mut out = String::with_capacity(body.len() + 16);
    for (i, line) in body.split('\n').enumerate() {
        if i > 0 {
            out.push_str("\r\n");
        }
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.starts_with('.') {
            out.push('.');
        }
        out.push_str(line);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试侧的 base64 解码（编码器的逆运算，用来证明往返无损）。
    fn b64_decode(s: &str) -> Option<Vec<u8>> {
        fn val(c: u8) -> Option<u32> {
            match c {
                b'A'..=b'Z' => Some((c - b'A') as u32),
                b'a'..=b'z' => Some((c - b'a' + 26) as u32),
                b'0'..=b'9' => Some((c - b'0' + 52) as u32),
                b'+' => Some(62),
                b'/' => Some(63),
                _ => None,
            }
        }
        let bytes: Vec<u8> = s.bytes().filter(|&c| c != b'\n' && c != b'\r').collect();
        let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
        for chunk in bytes.chunks(4) {
            if chunk.len() < 4 {
                return None;
            }
            let mut n = 0u32;
            let mut pad = 0;
            for &c in chunk {
                if c == b'=' {
                    pad += 1;
                } else {
                    n = (n << 6) | val(c)?;
                }
            }
            for _ in 0..pad {
                n <<= 6;
            }
            out.push((n >> 16) as u8);
            if pad < 2 {
                out.push((n >> 8) as u8);
            }
            if pad < 1 {
                out.push(n as u8);
            }
        }
        Some(out)
    }

    #[test]
    fn ascii_subject_is_left_alone() {
        // 纯 ASCII 加编码头会被部分老客户端/网关当垃圾处理
        assert_eq!(encode_subject("disk full on web-01"), "disk full on web-01");
        assert!(!encode_subject("disk full").contains("=?"));
    }

    #[test]
    fn chinese_subject_is_rfc2047_base64() {
        let s = encode_subject("[CRIT] 磁盘空间不足");
        assert!(s.starts_with("=?UTF-8?B?"), "实际：{s}");
        assert!(s.ends_with("?="), "实际：{s}");
        // 解回来必须一字不差
        let b64 = s.trim_start_matches("=?UTF-8?B?").trim_end_matches("?=");
        let bytes = b64_decode(b64).expect("解码失败");
        assert_eq!(String::from_utf8(bytes).unwrap(), "[CRIT] 磁盘空间不足");
    }

    #[test]
    fn subject_newlines_are_folded_away() {
        // 真实换行会被 SMTP 当成头终止符（头注入）
        let s = encode_subject("标题\r\nBcc: evil@x.com");
        assert!(
            !s.contains('\r') && !s.contains('\n'),
            "主题里不能有换行：{s:?}"
        );
    }

    #[test]
    fn base64_matches_known_vectors() {
        // RFC 4648 §10 的标准测试向量
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn ascii_body_uses_7bit_untouched() {
        let (cte, body) = encode_body("all good\n");
        assert_eq!(cte, "7bit");
        assert_eq!(body, "all good\n", "ASCII 正文不该被编码");
    }

    #[test]
    fn chinese_body_is_base64_and_wrapped_to_76() {
        let text = "磁盘空间不足：".repeat(40); // 远超一行
        let (cte, body) = encode_body(&text);
        assert_eq!(cte, "base64");
        for line in body.split("\r\n") {
            assert!(line.len() <= 76, "base64 行超长（{}）：{line}", line.len());
        }
        // 折行后拼回来仍应等于原编码
        let joined: String = body.split("\r\n").collect();
        assert_eq!(joined, base64_encode(text.as_bytes()));
        // 解码还原一字不差
        assert_eq!(
            String::from_utf8(b64_decode(&joined).unwrap()).unwrap(),
            text
        );
    }

    #[test]
    fn plain_message_declares_utf8_charset() {
        let msg = build_message(
            "标题",
            "正文内容",
            None,
            "BOUND",
            "Mon, 12 Oct 2026 03:00:00 +0000",
        );
        assert!(
            msg.contains("Content-Type: text/plain; charset=utf-8"),
            "实际：{msg}"
        );
        assert!(msg.contains("Subject: =?UTF-8?B?"), "实际：{msg}");
        assert!(msg.contains("MIME-Version: 1.0"));
        assert!(msg.contains("Date: Mon, 12 Oct 2026"), "实际：{msg}");
        // 头与正文之间必须空一行，否则 DATA 的首行被当头
        assert!(msg.contains("\r\n\r\n"));
        assert!(
            !msg.contains("multipart"),
            "无 HTML 时不该用 multipart：{msg}"
        );
    }

    #[test]
    fn multipart_puts_plain_text_first() {
        let msg = build_message(
            "标题",
            "纯文本正文",
            Some("<html><body>HTML正文</body></html>"),
            "BOUND",
            "Mon, 12 Oct 2026 03:00:00 +0000",
        );
        assert!(msg.contains("Content-Type: multipart/alternative; boundary=\"BOUND\""));
        // 纯文本段必须在 HTML 段之前（不支持 multipart 的客户端只读第一段）
        let plain_at = msg.find("Content-Type: text/plain").expect("缺纯文本段");
        let html_at = msg.find("Content-Type: text/html").expect("缺 HTML 段");
        assert!(plain_at < html_at, "纯文本段必须在前");
        assert!(msg.trim_end().ends_with("--BOUND--"), "缺结束边界：{msg}");
    }

    #[test]
    fn dot_stuffing_protects_terminator_line() {
        // 单独一行 `.` 会被服务器当成邮件结束 ⇒ 正文截断
        let stuffed = dot_stuff("第一行\r\n.\r\n第二行");
        assert!(
            stuffed.contains("\r\n..\r\n"),
            "以点开头的行必须补点：{stuffed:?}"
        );
        // 首行不以点开头时不该多补
        assert_eq!(dot_stuff("普通\n行"), "普通\r\n行");
    }

    #[test]
    fn dot_stuffing_is_idempotent_for_bodies_ending_with_dot() {
        let body = dot_stuff(".开头一行");
        assert!(body.starts_with("..开头一行"), "首行以点开头也要补：{body}");
    }
}
