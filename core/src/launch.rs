//! 外部链接启动参数：`ssh://` / `sftp://`（浏览器、Xshell、WinSCP、JumpServer 等堡垒机）。
//!
//! 认得的写法只有三类，其余一律忽略 —— 堡垒机的启动器会塞进一堆本机不认识的
//! 开关（`/hostkey=`、`-passive`、`/log=`……），为一个不认识的参数拒绝启动，
//! 表现就是「点了没反应」：
//!
//! * Xshell：`-url <uri>`（也接受 `-url=<uri>`）与 `-newtab`；
//! * WinSCP：`/sessionname=<名字>`（也接受 `-sessionname=`），会话名直接当标签标题；
//! * 裸 URI：`ssh://user:pass@host:22` / `sftp://user:pass@host:22/path`。
//!
//! 口令只从 URI 里取，**只进内存**（见 `EXTERNAL_PROFILE_PREFIX` 的说明）；
//! 这里不做任何落盘。

/// 临时（外部链接）连接的 `profile_id` 前缀。
///
/// 这类连接**不进**主机列表：`ConnectionStore` 里查不到这个 id，
/// 口令只存在 UI 的内存表里，标签一关就随表项一起消失。
/// 用前缀而不是 `profile_id: None`：连接结果的归队、断连条、分屏槽位修正
/// 都靠 profile id 认领占位 Tab（`None` 的 SSH 会话压根没有归队路径）。
pub const EXTERNAL_PROFILE_PREFIX: &str = "ext:";

/// 这条链接要开什么。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaunchKind {
    /// `ssh://`：只开一条 shell 通道的终端。
    Ssh,
    /// `sftp://`：只开一条 sftp 子系统通道的文件浏览。
    Sftp,
}

/// 解析出来的一条外部连接。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchTarget {
    pub kind: LaunchKind,
    pub host: String,
    pub port: u16,
    /// 空串 = 链接里没带用户名（连接时会以「缺用户名」失败，UI 不替用户猜）。
    pub username: String,
    pub password: Option<String>,
    /// `sftp://` 的初始路径（`/` 归一成 `None`）。
    pub path: Option<String>,
    /// `/sessionname=` / `-sessionname=`；标签标题用它。
    pub name: Option<String>,
    /// `-newtab`：**已经运行**的实例要新开一个标签，而不是复用当前标签。
    pub new_tab: bool,
}

/// 解析命令行（调用方先去掉 `argv[0]`）。没有链接参数时返回空表 —— 正常启动。
pub fn parse_args<I, S>(args: I) -> Vec<LaunchTarget>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut uris: Vec<String> = Vec::new();
    let mut name: Option<String> = None;
    let mut new_tab = false;
    let mut take_url = false;
    let mut args = args.into_iter();

    while let Some(arg) = args.next() {
        let a = arg.as_ref().trim();
        if a.is_empty() {
            continue;
        }
        if take_url {
            take_url = false;
            uris.push(a.to_string());
            continue;
        }
        if let Some(rest) = flag_value(a, &["-url"]) {
            if rest.is_empty() {
                take_url = true;
            } else {
                uris.push(rest.to_string());
            }
            continue;
        }
        if a.eq_ignore_ascii_case("-newtab") {
            new_tab = true;
            continue;
        }
        if let Some(rest) = flag_value(a, &["-sessionname", "/sessionname"]) {
            if rest.is_empty() {
                // `-sessionname 名字` 两个 token 的写法
                name = args.next().map(|s| s.as_ref().trim().to_string());
            } else {
                name = Some(rest.to_string());
            }
            continue;
        }
        if a.contains("://") {
            uris.push(a.to_string());
        }
        // 其余一概忽略：堡垒机启动器的私有开关不该让程序拒绝启动。
    }

    let mut targets: Vec<LaunchTarget> = uris.iter().filter_map(|u| parse_uri(u)).collect();
    // 会话名是「这一条启动」的属性，不是「每个 URL 各一份」：WinSCP 的
    // `/sessionname=` 可以写在 URL 后面，而 `-url` 与 `-newtab` 谁先谁后都可能。
    if let Some(name) = name.filter(|n| !n.trim().is_empty()) {
        if let Some(first) = targets.first_mut() {
            first.name = Some(name);
        }
    }
    for t in &mut targets {
        t.new_tab = new_tab;
    }
    targets
}

/// `-url` / `-url=值` / `/sessionname=值` 三种写法统一取值。
fn flag_value<'a>(arg: &'a str, flags: &[&str]) -> Option<&'a str> {
    for flag in flags {
        if arg.eq_ignore_ascii_case(flag) {
            return Some("");
        }
        // 前缀按**字节**比较：`arg[..flag.len()]` 会切在多字节字符中间 panic
        // （链接内容不可信）。前缀全为 ASCII 时 `flag.len() + 1` 必然是边界。
        let bytes = arg.as_bytes();
        let want = flag.as_bytes();
        if bytes.len() > want.len()
            && bytes[..want.len()].eq_ignore_ascii_case(want)
            && bytes[want.len()] == b'='
        {
            return Some(&arg[flag.len() + 1..]);
        }
    }
    None
}

/// `scheme://[user[:pass]@]host[:port][/path]`。
///
/// 解析失败返回 `None`（不认识的 scheme / 空主机），调用方静默跳过：
/// 命令行里混进一条 `https://...` 不该让 `ssh://...` 也开不了。
fn parse_uri(raw: &str) -> Option<LaunchTarget> {
    let (scheme, rest) = raw.split_once("://")?;
    let kind = match scheme.to_ascii_lowercase().as_str() {
        "ssh" => LaunchKind::Ssh,
        "sftp" => LaunchKind::Sftp,
        _ => return None,
    };
    // 查询串与锚点不属于连接参数，切掉再解析 host。
    let rest = rest.split(['?', '#']).next().unwrap_or(rest);
    let (authority, path) = match rest.find('/') {
        Some(ix) => (&rest[..ix], Some(&rest[ix..])),
        None => (rest, None),
    };

    let (userinfo, hostport) = match authority.rsplit_once('@') {
        Some((user, host)) => (Some(user), host),
        None => (None, authority),
    };
    let (username, password) = match userinfo {
        Some(info) => match info.split_once(':') {
            Some((user, pass)) => (percent_decode(user), Some(percent_decode(pass))),
            None => (percent_decode(info), None),
        },
        None => (String::new(), None),
    };

    let (host, port) = if let Some(rest) = hostport.strip_prefix('[') {
        // IPv6：[::1]:2222
        let (host, tail) = rest.split_once(']')?;
        let port = tail
            .strip_prefix(':')
            .and_then(|p| p.parse::<u16>().ok())
            .unwrap_or(22);
        (host.to_string(), port)
    } else {
        // `host:abc` / `host:` 不是端口，整个 `host:abc` 当主机名
        // （宁可连不上，也别把地址改坏）。
        match hostport.rsplit_once(':') {
            Some((host, port)) if !port.is_empty() => match port.parse::<u16>() {
                Ok(port) => (host.to_string(), port),
                Err(_) => (hostport.to_string(), 22),
            },
            _ => (hostport.to_string(), 22),
        }
    };
    if host.is_empty() {
        return None;
    }

    let path = path
        .map(percent_decode)
        .filter(|p| !p.is_empty() && p != "/");
    Some(LaunchTarget {
        kind,
        host,
        port,
        username,
        password: password.filter(|p| !p.is_empty()),
        path,
        name: None,
        new_tab: false,
    })
}

/// 百分号解码（URI 的 userinfo 段）。非法序列原样保留，不 panic。
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        // 只按**字节**判十六进制，不做字符串切片：`%` 后面跟一个多字节字符时
        // 切片会落在 UTF-8 字符中间直接 panic（链接内容不可信）。
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(args: &[&str]) -> LaunchTarget {
        let v = parse_args(args.iter().copied());
        assert_eq!(v.len(), 1, "应当解析出一条连接: {args:?}");
        v.into_iter().next().unwrap()
    }

    #[test]
    fn xshell_url_and_newtab() {
        let t = one(&["-url", "ssh://root:s3cret@10.0.0.7:2222", "-newtab"]);
        assert_eq!(t.kind, LaunchKind::Ssh);
        assert_eq!(t.host, "10.0.0.7");
        assert_eq!(t.port, 2222);
        assert_eq!(t.username, "root");
        assert_eq!(t.password.as_deref(), Some("s3cret"));
        assert!(t.new_tab);
        assert!(t.name.is_none());
    }

    #[test]
    fn winscp_bare_uri_and_sessionname() {
        let t = one(&[
            "sftp://ops:pw@jump.example.com:22/var/log",
            "/sessionname=JumpServer",
        ]);
        assert_eq!(t.kind, LaunchKind::Sftp);
        assert_eq!(t.host, "jump.example.com");
        assert_eq!(t.username, "ops");
        assert_eq!(t.password.as_deref(), Some("pw"));
        assert_eq!(t.path.as_deref(), Some("/var/log"));
        assert_eq!(t.name.as_deref(), Some("JumpServer"));
        assert!(!t.new_tab);
    }

    #[test]
    fn session_name_may_precede_the_uri() {
        let t = one(&["/sessionname=prod", "-url=ssh://u@h"]);
        assert_eq!(t.name.as_deref(), Some("prod"));
        assert_eq!(t.host, "h");
        assert_eq!(t.port, 22);
        assert_eq!(t.username, "u");
        assert!(t.password.is_none());
    }

    #[test]
    fn percent_encoded_password_is_decoded() {
        let t = one(&["ssh://u:p%40ss%3A1@h"]);
        assert_eq!(t.password.as_deref(), Some("p@ss:1"));
    }

    #[test]
    fn malformed_percent_sequences_survive_verbatim() {
        // `%` 后面是多字节字符：按字符串切片会切在 UTF-8 字符中间 —— 链接内容
        // 不可信，这里必须原样保留而不是 panic。
        let t = one(&["ssh://u:p%中@h"]);
        assert_eq!(t.password.as_deref(), Some("p%中"));
        let t = one(&["ssh://u:%zz%4@h"]);
        assert_eq!(t.password.as_deref(), Some("%zz%4"));
    }

    #[test]
    fn ipv6_host_and_default_port() {
        let t = one(&["ssh://[2001:db8::1]:2200"]);
        assert_eq!(t.host, "2001:db8::1");
        assert_eq!(t.port, 2200);
        let t = one(&["ssh://[::1]"]);
        assert_eq!(t.host, "::1");
        assert_eq!(t.port, 22);
    }

    #[test]
    fn unknown_flags_are_ignored_and_other_schemes_skipped() {
        let v = parse_args([
            "/hostkey=ssh-rsa",
            "-passive",
            "https://example.com",
            "-url",
            "sftp://u@h/",
            "/log=C:\\x.log",
        ]);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].kind, LaunchKind::Sftp);
        assert!(v[0].path.is_none(), "根路径应归一成 None");
    }

    #[test]
    fn no_uri_means_normal_startup() {
        assert!(parse_args(Vec::<String>::new()).is_empty());
        assert!(parse_args(["-newtab"]).is_empty());
    }

    #[test]
    fn multiple_urls_share_the_session_name_with_the_first() {
        let v = parse_args(["-url", "ssh://a@h1", "-url", "ssh://b@h2", "/sessionname=x"]);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].name.as_deref(), Some("x"));
        assert!(v[1].name.is_none());
    }
}
