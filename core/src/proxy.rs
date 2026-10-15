//! 代理设置（F-10）：连接级三态 + 全局配置，以及 HTTP CONNECT / SOCKS5 的**客户端**握手。
//!
//! 这一节只解决一件事：**RustShell 到服务器的那条 TCP 路径**。
//! `russh::client::connect` 自己建 `TcpStream`（0.63.3 `client/mod.rs:1083`），
//! 所以有代理时不能调它 —— 改为先由本模块跟代理握手拿到一条流，再交给
//! `russh::client::connect_stream`（同文件 `:1102`）。SSH 协议本身对此一无所知。
//!
//! SOCKS5 的**服务端**（动态转发，`crate::forward`）早在仓内，本模块是同一份 RFC
//! 的镜像：那边解请求、回应答，这边发请求、解应答。两边都把编解码做成纯函数，
//! 理由也一样 —— 握手散在 async 流程里时，「少一个字节」和「版本不对」这两种错
//! 只能靠真连一个代理才能发现。

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------- 数据模型

/// 代理协议类型。落盘用稳定 ASCII（[`ProxyKind::key`]），**不要**用 i18n 标签：
/// 换显示语言或换文案都不该让老配置读不出来（与 `ForwardKind::key` 同一条）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProxyKind {
    #[default]
    Http,
    Socks5,
}

impl ProxyKind {
    pub fn key(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Socks5 => "socks5",
        }
    }

    /// 从 key 还原；认不出的一律当默认（手改坏配置不该让程序起不来）。
    pub fn from_key(key: &str) -> Self {
        match key {
            "socks5" => Self::Socks5,
            _ => Self::Http,
        }
    }

    /// 展示名。协议名是专有名词，两种语言都不译。
    pub fn label(self) -> &'static str {
        match self {
            Self::Http => "HTTP",
            Self::Socks5 => "SOCKS5",
        }
    }

    pub const ALL: [ProxyKind; 2] = [ProxyKind::Http, ProxyKind::Socks5];
}

/// 代理端口缺省值：HTTP 代理最常用的那一个。
///
/// 只是**输入框的初值**，不是「猜一个代理地址」—— 地址栏故意留空并在保存时必填，
/// 免得用户点开开关、地址栏被悄悄填上一个不存在的代理，然后所有连接一起失败。
pub const DEFAULT_PROXY_PORT: u16 = 8080;

/// 全局代理在系统凭据库里的键（非 Windows 走 keyring，Windows 忽略此值）。
/// 与 `notify_smtp::SECRET_KEY` 同一手法：抽成常量，免得两处各写一个字符串。
pub const GLOBAL_SECRET_KEY: &str = "proxy-global";

/// 连接级代理在系统凭据库里的键：按 profile id 区分，互不覆盖。
pub fn secret_key_for(profile_id: &str) -> String {
    format!("proxy-{profile_id}")
}

/// 一套代理配置。
///
/// 口令分两个字段，与 `ConnectionProfile` 的 `stored_password` / `password` 同一条纪律：
/// - [`Self::password_ref`] 是**落盘的那一份**（DPAPI 密文 / keyring 引用），永不回显进输入框；
/// - [`Self::password`] 是**运行期明文**（宿主 `unprotect` 解出后填入），永不序列化。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProxyConfig {
    #[serde(default)]
    pub kind: ProxyKind,
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password_ref: Option<String>,
    /// 运行期明文口令。**永不落盘** —— 与 `ConnectionProfile::password` 同理，
    /// 落盘的那一份只在 `password_ref` 里（且它是密文/引用，不是明文）。
    #[serde(default, skip_serializing)]
    pub password: Option<String>,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            kind: ProxyKind::Http,
            host: String::new(),
            port: DEFAULT_PROXY_PORT,
            username: None,
            password_ref: None,
            password: None,
        }
    }
}

impl ProxyConfig {
    /// `host:port`（错误文案与摘要用）。
    pub fn address(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// 「HTTP 127.0.0.1:8080」这种一行摘要（UI 只读投影与日志用）。
    pub fn summary(&self) -> String {
        format!("{} {}", self.kind.label(), self.address())
    }

    /// 需要认证时返回 `(用户名, 口令)`；没配用户名 = 不需要认证。
    ///
    /// 只配了用户名没配口令时返回空口令而不是 `None`：RFC1929 允许空口令，
    /// 而「用户名填了、口令忘了填」应该让代理去拒绝（用户看到 407/认证失败），
    /// 不该被我们悄悄降级成匿名 —— 那样报错点会跑到目标主机上去。
    pub fn credentials(&self) -> Option<(&str, &str)> {
        let user = self.username.as_deref()?.trim();
        if user.is_empty() {
            return None;
        }
        Some((user, self.password.as_deref().unwrap_or("")))
    }

    /// 解出运行期明文：把 `password_ref` 交给宿主注入的解密器，填进 `password`。
    ///
    /// 解密器由宿主给（`platform::secret::unprotect`）—— core 不依赖 platform，
    /// 与 `ConnectionStore::restore_passwords` 同一个手法。
    pub fn with_plaintext(&self, unprotect: impl Fn(&str) -> Option<String>) -> Self {
        let mut c = self.clone();
        if c.password.is_none() {
            c.password = c.password_ref.as_deref().and_then(unprotect);
        }
        c
    }

    /// 去空白：地址与用户名的首尾空白一律不算数（粘贴进来的值常带）。
    /// 返回是否发生了变化。
    pub fn normalize(&mut self) -> bool {
        let before = self.clone();
        self.host = self.host.trim().to_string();
        self.username = self
            .username
            .as_deref()
            .map(str::trim)
            .filter(|u| !u.is_empty())
            .map(str::to_string);
        *self != before
    }

    /// 校验：返回 `None` 表示可用来建连。
    ///
    /// 收口在数据层而不是对话框：手改 `settings.json` / `connections.json`、
    /// 导入配置两条路径都绕不过对话框（与 `ForwardRule::validate` 同理）。
    pub fn validate(&self) -> Option<ProxyValidateError> {
        if self.host.trim().is_empty() {
            return Some(ProxyValidateError::NoHost);
        }
        if self.port == 0 {
            return Some(ProxyValidateError::NoPort);
        }
        // 域名形式在 SOCKS5 里是「一个字节的长度」，HTTP 请求行也不该无限长。
        if self.host.trim().len() > SOCKS5_MAX_DOMAIN_LEN {
            return Some(ProxyValidateError::HostTooLong);
        }
        None
    }

    pub fn is_ready(&self) -> bool {
        self.validate().is_none()
    }
}

/// 代理配置不可用的原因（结构化：文案由调用方取，core 不产出本地化句子）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyValidateError {
    NoHost,
    NoPort,
    /// 地址超过 255 字节（SOCKS5 的域名长度是一个字节）
    HostTooLong,
}

/// 面向用户的不可用原因（与 `forward::validate_error_text` 同形）。
pub fn validate_error_text(err: ProxyValidateError) -> &'static str {
    match err {
        ProxyValidateError::NoHost => crate::i18n::t("请填写代理地址"),
        ProxyValidateError::NoPort => crate::i18n::t("代理端口需为 1–65535 的数字"),
        ProxyValidateError::HostTooLong => crate::i18n::t("代理地址过长（超过 255 字节）"),
    }
}

/// 连接级代理口径（三态）。
///
/// 为什么不是 `Option<ProxyConfig>`：`None` 表达不了「直连」与「跟随全局」的差别 ——
/// 前者是「这台机器明确不走代理」，后者是「看全局怎么配」。与 `ConnectionProfile::session_log`
/// 用 `Option<bool>` 而不是抄一份全局值是同一条教训：抄下来的那一刻就把全局冻进了配置。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProxyChoice {
    #[default]
    FollowGlobal,
    Direct,
    /// 本条连接自带的一套代理（含密文引用；导出走独立 DTO，不带口令）
    Custom(ProxyConfig),
}

impl ProxyChoice {
    /// 档位（不含配置本体）。
    pub fn mode(&self) -> ProxyMode {
        match self {
            Self::FollowGlobal => ProxyMode::FollowGlobal,
            Self::Direct => ProxyMode::Direct,
            Self::Custom(_) => ProxyMode::Custom,
        }
    }

    /// 落盘时不写 `follow_global`：默认档位占了绝大多数连接，写出来只是噪声。
    pub fn is_follow_global(&self) -> bool {
        matches!(self, Self::FollowGlobal)
    }

    /// 由「档位 + 自定义配置」合成（UI 保存时用；非自定义档位丢弃配置本体）。
    pub fn from_parts(mode: ProxyMode, custom: ProxyConfig) -> Self {
        match mode {
            ProxyMode::FollowGlobal => Self::FollowGlobal,
            ProxyMode::Direct => Self::Direct,
            ProxyMode::Custom => Self::Custom(custom),
        }
    }

    /// 转导出镜像：`Custom` 的载荷换成**没有口令字段**的 [`ExportedProxy`]。
    pub fn to_export(&self) -> ExportedProxyChoice {
        match self {
            Self::FollowGlobal => ExportedProxyChoice::FollowGlobal,
            Self::Direct => ExportedProxyChoice::Direct,
            Self::Custom(c) => ExportedProxyChoice::Custom(ExportedProxy::of(c)),
        }
    }

    /// 从导出镜像还原（`Custom` 的口令必然是空的，导入后需重填）。
    pub fn from_export(exported: ExportedProxyChoice) -> Self {
        match exported {
            ExportedProxyChoice::FollowGlobal => Self::FollowGlobal,
            ExportedProxyChoice::Direct => Self::Direct,
            ExportedProxyChoice::Custom(p) => Self::Custom(p.to_config()),
        }
    }
}

/// 导出体里的代理：**没有口令字段**。
///
/// 与 `ExportedProfile` 排除 `stored_password` 是同一条理由（`core/src/connections.rs`
/// 的原话：必须「结构性排除，而不是靠调用方自觉」）：密文是机器绑定的，换机解不开，
/// 带出去只有泄漏面没有价值。所以这里不是「把口令抹掉再序列化 `ProxyConfig`」，
/// 而是换一个**根本装不下凭据的类型** —— 将来给 `ProxyConfig` 加凭据类字段时，
/// 导出体不会跟着自动带上。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ExportedProxy {
    #[serde(default)]
    pub kind: ProxyKind,
    pub host: String,
    pub port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
}

impl ExportedProxy {
    /// 从完整配置生成导出镜像（只取「怎么连上代理」这一半）。
    pub fn of(cfg: &ProxyConfig) -> Self {
        Self {
            kind: cfg.kind,
            host: cfg.host.clone(),
            port: cfg.port,
            username: cfg.username.clone(),
        }
    }

    /// 还原成配置：口令留空（导入后由用户在对话框里重填）。
    pub fn to_config(&self) -> ProxyConfig {
        ProxyConfig {
            kind: self.kind,
            host: self.host.clone(),
            port: self.port,
            username: self.username.clone(),
            password_ref: None,
            password: None,
        }
    }
}

/// 导出体里的三态口径：与 [`ProxyChoice`] 同形，只把 `Custom` 的载荷换成镜像。
///
/// 保形（而不是压成一个 `Option<ExportedProxy>`）是为了不丢「明确直连」这一档 ——
/// 那是用户对某台机器**刻意**的选择，导入后不该悄悄退回「跟随全局」。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportedProxyChoice {
    #[default]
    FollowGlobal,
    Direct,
    Custom(ExportedProxy),
}

impl ExportedProxyChoice {
    /// 默认档位不写键（与 `ConnectionProfile::proxy` 同一个口径）。
    pub fn is_follow_global(&self) -> bool {
        matches!(self, Self::FollowGlobal)
    }
}

/// 连接级代理的**档位**（三态，不含配置本体）。
///
/// 与 `ProxyChoice` 分开是给 UI 用的：对话框里配置本体在五个输入框里（草稿），
/// 档位是其中唯一需要「循环点击」的那一格；保存时两者合成 `ProxyChoice`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProxyMode {
    #[default]
    FollowGlobal,
    Direct,
    Custom,
}

impl ProxyMode {
    pub const ALL: [ProxyMode; 3] = [
        ProxyMode::FollowGlobal,
        ProxyMode::Direct,
        ProxyMode::Custom,
    ];

    /// 循环到下一档（跟随全局 → 直连 → 自定义 → 跟随全局）。
    ///
    /// 用循环而不是下拉：三档一次点完就能看全，下拉要点两下、还会把当前值藏起来
    /// （与 `dlg_fw_cycle_kind` 同一个取舍）。
    pub fn next(self) -> Self {
        let all = Self::ALL;
        let ix = all.iter().position(|m| *m == self).unwrap_or(0);
        all[(ix + 1) % all.len()]
    }

    /// 稳定 ASCII 键（落盘不进这一层，留给 UI 的词条映射与用例断言用）。
    pub fn key(self) -> &'static str {
        match self {
            Self::FollowGlobal => "follow_global",
            Self::Direct => "direct",
            Self::Custom => "custom",
        }
    }
}

/// 自定义代理的**目标指纹**：类型 + 去空白后的地址 + 端口。
///
/// 只服务一个判定：[`may_inherit_password`]。口令是**发给某一个代理**的凭据，
/// 目标一换它就与新的目标无关了。
pub fn fingerprint(cfg: &ProxyConfig) -> (ProxyKind, String, u16) {
    (cfg.kind, cfg.host.trim().to_string(), cfg.port)
}

/// 保存时能不能沿用旧口令（两个表单入口**唯一**的一处判定）。
///
/// 「口令留空 = 不改口令」是编辑口令的既定口径（与连接口令同，见 `dialog_save`），
/// 但它有一个前提：改的还是**同一个代理**。代理目标变了还把旧密文留着，等于把
/// A 代理的凭据发给 B 代理 —— 轻则误导（用户以为已经填过了），重则泄漏。
/// `origin` 是打开表单那一刻的[`fingerprint`]；`None`（新建）一律不沿用。
pub fn may_inherit_password(
    origin: Option<&(ProxyKind, String, u16)>,
    current: &ProxyConfig,
) -> bool {
    origin.is_some_and(|o| o == &fingerprint(current))
}

/// 合成出「本条连接真正要走的代理」。
///
/// **收口在这里，不交给调用点**：这条链上有四个连接入口（会话 / 重连 / 连接测试 /
/// 定时任务与 agent），每个都自己解析一次的话，漏一个就是「会话走代理、定时任务直连」
/// 这种最难发现的半生效。传全局值给引擎、由引擎合成，新增入口自动继承正确行为。
pub fn resolve<'a>(
    global: Option<&'a ProxyConfig>,
    choice: &'a ProxyChoice,
) -> Option<&'a ProxyConfig> {
    match choice {
        ProxyChoice::FollowGlobal => global,
        ProxyChoice::Direct => None,
        ProxyChoice::Custom(c) => Some(c),
    }
}

// ------------------------------------------------------- 握手（编解码纯函数）

// ---- HTTP CONNECT ----

/// 代理响应头的大小上限（含）。
///
/// 必须有一个上限：坏掉/恶意的代理可以用「永不结束的头部」把我们的内存撑爆，
/// 而外层超时只能保证「早晚会放弃」，不能保证放弃之前没吃掉几百 MB。
pub const HTTP_HEAD_MAX: usize = 8 * 1024;

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Base64 编码（只为 `Proxy-Authorization: Basic` 服务）。
///
/// 自实现避免新增依赖 —— 与 `platform::secret.rs` 里那份同样的取舍；
/// 标准字母表 + `=` 填充，37 行以内不值得引一个 crate。
fn b64_encode(data: &[u8]) -> String {
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

/// HTTP CONNECT 请求（RFC 9110 §9.3.6）。
///
/// `Host` 与请求行都写 `host:port` 原文（IPv6 字面量由调用方带方括号，那是 URI 的规定）；
/// 有凭据时补 `Proxy-Authorization: Basic`。末尾空行不能省 —— 少了它代理会一直
/// 等着「还有没有头字段」，表现就是「代理不响应」。
pub fn http_connect_request(host: &str, port: u16, credentials: Option<(&str, &str)>) -> String {
    let target = format!("{host}:{port}");
    let mut req = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n");
    if let Some((user, pass)) = credentials {
        req.push_str(&format!(
            "Proxy-Authorization: Basic {}\r\n",
            b64_encode(format!("{user}:{pass}").as_bytes())
        ));
    }
    req.push_str("\r\n");
    req
}

/// 从响应缓冲里读出状态码。
///
/// - `Ok(Some(code))` = 头部已完整（收到了空行），`code` 是状态码
/// - `Ok(None)` = 字节还不够
/// - `Err(原因)` = 不是 HTTP 响应 / 头部超过 [`HTTP_HEAD_MAX`]
pub fn parse_http_connect_status(buf: &[u8]) -> Result<Option<u16>, String> {
    let Some(head_end) = find_head_end(buf) else {
        if buf.len() > HTTP_HEAD_MAX {
            return Err(crate::i18n::ts(
                format!("代理响应头超过 {HTTP_HEAD_MAX} 字节仍未见结束空行"),
                format!(
                    "Proxy response headers exceeded {HTTP_HEAD_MAX} bytes without a blank line"
                ),
            ));
        }
        return Ok(None);
    };
    let head = &buf[..head_end];
    let line_end = head
        .windows(2)
        .position(|w| w == b"\r\n")
        .unwrap_or(head.len());
    let line = String::from_utf8_lossy(&head[..line_end]);
    let mut parts = line.split(' ');
    let version = parts.next().unwrap_or("");
    let code = parts
        .next()
        .and_then(|c| c.parse::<u16>().ok())
        .filter(|_| version.starts_with("HTTP/"));
    match code {
        Some(c) => Ok(Some(c)),
        None => Err(crate::i18n::ts(
            format!("代理返回的不是 HTTP 响应：{line}"),
            format!("Proxy did not answer with HTTP: {line}"),
        )),
    }
}

/// 缓冲区里第一个 `\r\n\r\n` 的**位置**（不含它本身）。
fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

// ---- SOCKS5（RFC1928 / RFC1929）----

/// SOCKS5 版本号（与 `crate::forward` 同值；那边是服务端）。
const SOCKS5_VERSION: u8 = 0x05;
/// 「无认证」方法码。
const SOCKS5_METHOD_NO_AUTH: u8 = 0x00;
/// 「用户名/口令」方法码（RFC1929）。
const SOCKS5_METHOD_USERPASS: u8 = 0x02;
/// 「没有可接受的方法」方法码。
const SOCKS5_METHOD_NONE_ACCEPTABLE: u8 = 0xFF;
/// CONNECT 命令。
const SOCKS5_CMD_CONNECT: u8 = 0x01;
const SOCKS5_ATYP_IPV4: u8 = 0x01;
const SOCKS5_ATYP_DOMAIN: u8 = 0x03;
const SOCKS5_ATYP_IPV6: u8 = 0x04;
/// 域名长度上限：RFC1928 的 `LEN` 只有一个字节。
pub const SOCKS5_MAX_DOMAIN_LEN: usize = 255;

/// 客户端问候：`VER NMETHODS METHODS...`。
///
/// 配了口令就同时提供「用户名/口令」与「无认证」两种，由代理选（有些代理对
/// 内网来源本来就不要求认证，我们没理由替它决定）；没配口令只提供「无认证」——
/// 报一个自己根本没有的方法，代理选了我们也没法答。
pub fn socks5_greeting(with_credentials: bool) -> Vec<u8> {
    if with_credentials {
        vec![
            SOCKS5_VERSION,
            2,
            SOCKS5_METHOD_NO_AUTH,
            SOCKS5_METHOD_USERPASS,
        ]
    } else {
        vec![SOCKS5_VERSION, 1, SOCKS5_METHOD_NO_AUTH]
    }
}

/// 代理选中的认证方法：`VER METHOD`。
///
/// - `Ok(Some(method))` = 收到了选择
/// - `Ok(None)` = 字节还不够
/// - `Err(原因)` = 版本不对（多半是连错了端口，比如把 HTTP 代理配成了 SOCKS5）
pub fn socks5_chosen_method(buf: &[u8]) -> Result<Option<u8>, String> {
    let Some(&ver) = buf.first() else {
        return Ok(None);
    };
    if ver != SOCKS5_VERSION {
        return Err(crate::i18n::ts(
            format!("代理握手失败：SOCKS5 版本不匹配（收到 0x{ver:02x}）"),
            format!("SOCKS5 handshake failed: unexpected version 0x{ver:02x}"),
        ));
    }
    Ok(buf.get(1).copied())
}

/// RFC1929 用户名/口令认证请求：`VER ULEN UNAME PLEN PASSWD`。
///
/// 长度按**字节**算（一个汉字三字节），超 255 直接报错而不是截断 ——
/// 截断会送去一个「长得像但不是」的凭据，代理只会回认证失败，用户永远查不出为什么。
pub fn socks5_userpass_request(user: &str, pass: &str) -> Result<Vec<u8>, String> {
    let u = user.as_bytes();
    let p = pass.as_bytes();
    if u.len() > 255 {
        return Err(crate::i18n::ts(
            "代理用户名过长（超过 255 字节）".to_string(),
            "Proxy username too long (> 255 bytes)".to_string(),
        ));
    }
    if p.len() > 255 {
        return Err(crate::i18n::ts(
            "代理口令过长（超过 255 字节）".to_string(),
            "Proxy password too long (> 255 bytes)".to_string(),
        ));
    }
    let mut out = Vec::with_capacity(3 + u.len() + p.len());
    out.push(0x01); // 子协商版本号，RFC1929 固定为 1（不是 SOCKS 的 5）
    out.push(u.len() as u8);
    out.extend_from_slice(u);
    out.push(p.len() as u8);
    out.extend_from_slice(p);
    Ok(out)
}

/// RFC1929 认证应答：`VER STATUS`。
///
/// - `Ok(Some(true))` = 通过；`Ok(Some(false))` = 代理拒绝（用户名或口令不对）
/// - `Ok(None)` = 字节还不够
pub fn socks5_userpass_reply(buf: &[u8]) -> Result<Option<bool>, String> {
    let Some(&ver) = buf.first() else {
        return Ok(None);
    };
    if ver != 0x01 {
        return Err(crate::i18n::ts(
            format!("代理认证应答版本异常（0x{ver:02x}）"),
            format!("Unexpected SOCKS5 auth reply version (0x{ver:02x})"),
        ));
    }
    Ok(buf.get(1).map(|&st| st == 0x00))
}

/// CONNECT 请求：`VER CMD RSV ATYP DST.ADDR DST.PORT`。
///
/// 地址类型按目标自动判：IPv4 / IPv6 字面量走 `0x01` / `0x04`（省一次域名解析，
/// 也避开「代理自己解析不了这个域名」），其余一律域名形式 `0x03`。
pub fn socks5_connect_request(host: &str, port: u16) -> Result<Vec<u8>, String> {
    // `[::1]` 这种带方括号的写法是 URI 的习惯，SOCKS5 要的是 16 字节原文。
    let bare = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    let mut out = vec![SOCKS5_VERSION, SOCKS5_CMD_CONNECT, 0x00];
    if let Ok(v4) = bare.parse::<std::net::Ipv4Addr>() {
        out.push(SOCKS5_ATYP_IPV4);
        out.extend_from_slice(&v4.octets());
    } else if let Ok(v6) = bare.parse::<std::net::Ipv6Addr>() {
        out.push(SOCKS5_ATYP_IPV6);
        out.extend_from_slice(&v6.octets());
    } else {
        let name = bare.as_bytes();
        if name.is_empty() || name.len() > SOCKS5_MAX_DOMAIN_LEN {
            return Err(crate::i18n::ts(
                format!("目标地址无法表示为 SOCKS5 域名（{} 字节）", name.len()),
                format!(
                    "Target host is not a valid SOCKS5 domain ({} bytes)",
                    name.len()
                ),
            ));
        }
        out.push(SOCKS5_ATYP_DOMAIN);
        out.push(name.len() as u8);
        out.extend_from_slice(name);
    }
    out.extend_from_slice(&port.to_be_bytes());
    Ok(out)
}

/// CONNECT 应答：`VER REP RSV ATYP BND.ADDR BND.PORT`（BND 是变长的）。
///
/// - `Ok(Some((consumed, rep)))` = 应答完整，`consumed` 是它占的字节数
/// - `Ok(None)` = 字节还不够
/// - `Err(原因)` = 版本/地址类型异常
///
/// 变长这一段必须按 `ATYP` 走完再判「够不够」：只读前 4 字节就返回成功的话，
/// 后面那截 BND 会被当成下一次握手的字节（本模块里没有下一次，但缓存里会留下
/// 垃圾 —— 而 SSH 协议马上就跑在这条流上，多一个字节都是灾难）。
pub fn socks5_connect_reply(buf: &[u8]) -> Result<Option<(usize, u8)>, String> {
    if buf.len() < 4 {
        return Ok(None);
    }
    if buf[0] != SOCKS5_VERSION {
        return Err(crate::i18n::ts(
            format!("代理应答版本异常（0x{:02x}）", buf[0]),
            format!("Unexpected SOCKS5 reply version (0x{:02x})", buf[0]),
        ));
    }
    let (addr_len, addr_at) = match buf[3] {
        SOCKS5_ATYP_IPV4 => (4usize, 4usize),
        SOCKS5_ATYP_IPV6 => (16usize, 4usize),
        SOCKS5_ATYP_DOMAIN => {
            let n = buf[4] as usize;
            (n, 5usize)
        }
        other => {
            return Err(crate::i18n::ts(
                format!("代理应答的地址类型不支持（0x{other:02x}）"),
                format!("Unsupported address type in SOCKS5 reply (0x{other:02x})"),
            ))
        }
    };
    let end = addr_at + addr_len + 2;
    if buf.len() < end {
        return Ok(None);
    }
    Ok(Some((end, buf[1])))
}

/// `REP` 到人话（RFC1928 §6 的错误码表）。
pub fn socks5_rep_text(rep: u8) -> &'static str {
    match rep {
        0x01 => crate::i18n::t("一般性失败"),
        0x02 => crate::i18n::t("代理不允许该连接"),
        0x03 => crate::i18n::t("网络不可达"),
        0x04 => crate::i18n::t("主机不可达"),
        0x05 => crate::i18n::t("连接被拒"),
        0x06 => crate::i18n::t("TTL 超时"),
        0x07 => crate::i18n::t("代理不支持 CONNECT 命令"),
        0x08 => crate::i18n::t("代理不支持该地址类型"),
        _ => crate::i18n::t("未知错误"),
    }
}

// ------------------------------------------------------------ 握手（异步）

/// 经代理与 `host:port` 建立一条流。
///
/// **不在这里设超时**：调用方（`ssh::connect_only`）把整段握手包在它自己的 deadline 里，
/// 到点按相位报「代理未响应」而不是「SSH 握手超时」—— 相位标记见 `ssh.rs`。
/// 内层再套一层超时只会让「谁先到点」变成竞态，文案随之不确定。
pub(crate) async fn open_tunnel(
    proxy: &ProxyConfig,
    host: &str,
    port: u16,
) -> Result<tokio::net::TcpStream, String> {
    if let Some(e) = proxy.validate() {
        return Err(crate::i18n::ts(
            format!("代理配置不完整：{}", validate_error_text(e)),
            format!("Incomplete proxy configuration: {}", validate_error_text(e)),
        ));
    }
    let mut stream = tokio::net::TcpStream::connect((proxy.host.trim(), proxy.port))
        .await
        .map_err(|e| {
            crate::i18n::ts(
                format!("代理 {} 不可达：{e}", proxy.address()),
                format!("Proxy {} unreachable: {e}", proxy.address()),
            )
        })?;
    match proxy.kind {
        ProxyKind::Http => http_connect(&mut stream, proxy, host, port).await?,
        ProxyKind::Socks5 => socks5_connect(&mut stream, proxy, host, port).await?,
    }
    Ok(stream)
}

/// HTTP CONNECT：写请求 → 读到头部结束 → 判状态码。
async fn http_connect(
    stream: &mut tokio::net::TcpStream,
    proxy: &ProxyConfig,
    host: &str,
    port: u16,
) -> Result<(), String> {
    use tokio::io::AsyncWriteExt as _;

    let req = http_connect_request(host, port, proxy.credentials());
    stream
        .write_all(req.as_bytes())
        .await
        .map_err(|e| proxy_io_err(&proxy.address(), e))?;
    let mut buf: Vec<u8> = Vec::with_capacity(256);
    loop {
        match parse_http_connect_status(&buf)? {
            Some(status) if (200..300).contains(&status) => return Ok(()),
            Some(status) => return Err(http_reject_msg(proxy, host, port, status)),
            None => {
                if !read_more(stream, &mut buf).await {
                    return Err(proxy_closed_msg(&proxy.address()));
                }
            }
        }
    }
}

/// 407 单独说清楚：代理**要认证**和代理**拒绝放行**是两件事，
/// 前者用户去填用户名/口令就能解决，后者得去查代理策略。
fn http_reject_msg(proxy: &ProxyConfig, host: &str, port: u16, status: u16) -> String {
    let addr = proxy.address();
    if status == 407 {
        return crate::i18n::ts(
            format!("代理 {addr} 要求认证（407）：请在设置或连接里填写代理用户名与口令"),
            format!(
                "Proxy {addr} requires authentication (407): provide a proxy username and password"
            ),
        );
    }
    crate::i18n::ts(
        format!("代理 {addr} 拒绝连接 {host}:{port}（HTTP {status}）"),
        format!("Proxy {addr} refused CONNECT {host}:{port} (HTTP {status})"),
    )
}

/// SOCKS5：问候 → （必要时）认证 → CONNECT → 应答。
///
/// 每一步之间都会 `buf.clear()`：SOCKS5 是严格的一问一答，服务端的下一段应答
/// 只能在我们发出下一段请求之后才可能到达，所以「把上一段的残留字节丢掉」是安全的。
/// 反过来说，如果哪天真出现了流水线（不该有），这里清掉的字节会变成握手失败 ——
/// 而不是更糟的「悄悄错位」。
async fn socks5_connect(
    stream: &mut tokio::net::TcpStream,
    proxy: &ProxyConfig,
    host: &str,
    port: u16,
) -> Result<(), String> {
    use tokio::io::AsyncWriteExt as _;

    let addr = proxy.address();
    let creds = proxy.credentials();
    stream
        .write_all(&socks5_greeting(creds.is_some()))
        .await
        .map_err(|e| proxy_io_err(&addr, e))?;
    let mut buf: Vec<u8> = Vec::with_capacity(128);

    // ① 方法选择
    let method = loop {
        if let Some(m) = socks5_chosen_method(&buf)? {
            break m;
        }
        if !read_more(stream, &mut buf).await {
            return Err(proxy_closed_msg(&addr));
        }
    };
    match method {
        SOCKS5_METHOD_NO_AUTH => {}
        SOCKS5_METHOD_USERPASS => {
            let (user, pass) = creds.ok_or_else(|| {
                crate::i18n::ts(
                    format!("代理 {addr} 要求用户名/口令认证，但未配置代理凭据"),
                    format!("Proxy {addr} requires username/password authentication, but no credentials are set"),
                )
            })?;
            let req = socks5_userpass_request(user, pass)?;
            buf.clear();
            stream
                .write_all(&req)
                .await
                .map_err(|e| proxy_io_err(&addr, e))?;
            loop {
                match socks5_userpass_reply(&buf)? {
                    Some(true) => break,
                    Some(false) => {
                        return Err(crate::i18n::ts(
                            format!("代理 {addr} 认证失败：用户名或口令不正确"),
                            format!(
                                "Proxy {addr} authentication failed: wrong username or password"
                            ),
                        ))
                    }
                    None => {
                        if !read_more(stream, &mut buf).await {
                            return Err(proxy_closed_msg(&addr));
                        }
                    }
                }
            }
        }
        SOCKS5_METHOD_NONE_ACCEPTABLE => {
            return Err(crate::i18n::ts(
                format!("代理 {addr} 拒绝了所有可用的认证方式"),
                format!("Proxy {addr} rejected every offered authentication method"),
            ))
        }
        other => {
            return Err(crate::i18n::ts(
                format!("代理 {addr} 选择了不支持的认证方式（0x{other:02x}）"),
                format!("Proxy {addr} chose an unsupported authentication method (0x{other:02x})"),
            ))
        }
    }

    // ② CONNECT
    let req = socks5_connect_request(host, port)?;
    buf.clear();
    stream
        .write_all(&req)
        .await
        .map_err(|e| proxy_io_err(&addr, e))?;
    loop {
        match socks5_connect_reply(&buf)? {
            Some((_consumed, 0x00)) => return Ok(()),
            Some((_consumed, rep)) => {
                return Err(crate::i18n::ts(
                    format!(
                        "代理 {addr} 拒绝连接 {host}:{port}：{}（REP={rep}）",
                        socks5_rep_text(rep)
                    ),
                    format!(
                        "Proxy {addr} refused CONNECT {host}:{port}: {} (REP={rep})",
                        socks5_rep_text(rep)
                    ),
                ))
            }
            None => {
                if !read_more(stream, &mut buf).await {
                    return Err(proxy_closed_msg(&addr));
                }
            }
        }
    }
}

/// 再读一段进缓冲；返回 `false` = 对端已关闭 / 读失败。
async fn read_more(stream: &mut tokio::net::TcpStream, buf: &mut Vec<u8>) -> bool {
    use tokio::io::AsyncReadExt as _;
    let mut chunk = [0u8; 512];
    match stream.read(&mut chunk).await {
        Ok(0) | Err(_) => false,
        Ok(n) => {
            buf.extend_from_slice(&chunk[..n]);
            true
        }
    }
}

fn proxy_io_err(addr: &str, e: std::io::Error) -> String {
    crate::i18n::ts(
        format!("与代理 {addr} 通信失败：{e}"),
        format!("Failed to talk to proxy {addr}: {e}"),
    )
}

fn proxy_closed_msg(addr: &str) -> String {
    crate::i18n::ts(
        format!("代理 {addr} 在握手完成前关闭了连接"),
        format!("Proxy {addr} closed the connection during the handshake"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(kind: ProxyKind, host: &str, port: u16) -> ProxyConfig {
        ProxyConfig {
            kind,
            host: host.into(),
            port,
            ..Default::default()
        }
    }

    fn with_creds(mut c: ProxyConfig, user: &str, pass: &str) -> ProxyConfig {
        c.username = Some(user.into());
        c.password = Some(pass.into());
        c
    }

    // ---- 模型 ----

    #[test]
    fn kind_keys_round_trip_and_unknown_falls_back() {
        for k in ProxyKind::ALL {
            assert_eq!(ProxyKind::from_key(k.key()), k);
        }
        assert_eq!(ProxyKind::from_key("socks"), ProxyKind::Http);
        assert_eq!(ProxyKind::default(), ProxyKind::Http);
    }

    #[test]
    fn normalize_trims_and_drops_empty_username() {
        let mut c = ProxyConfig {
            host: "  proxy.lan ".into(),
            port: 3128,
            username: Some("  ".into()),
            ..Default::default()
        };
        assert!(c.normalize());
        assert_eq!(c.host, "proxy.lan");
        assert_eq!(c.username, None);
        assert!(!c.normalize(), "第二次归一化不该再有变化");
    }

    #[test]
    fn validate_rejects_empty_host_zero_port_and_over_long_host() {
        assert_eq!(
            cfg(ProxyKind::Http, "  ", 8080).validate(),
            Some(ProxyValidateError::NoHost)
        );
        assert_eq!(
            cfg(ProxyKind::Http, "10.0.0.1", 0).validate(),
            Some(ProxyValidateError::NoPort)
        );
        let long = "a".repeat(SOCKS5_MAX_DOMAIN_LEN + 1);
        assert_eq!(
            cfg(ProxyKind::Http, &long, 8080).validate(),
            Some(ProxyValidateError::HostTooLong)
        );
        assert!(cfg(ProxyKind::Http, "10.0.0.1", 8080).is_ready());
    }

    /// 口令只走引用落盘：明文 `password` 既不进 JSON，也要能从引用解出来。
    #[test]
    fn plaintext_never_lands_in_json_and_comes_back_from_the_ref() {
        let mut c = with_creds(cfg(ProxyKind::Http, "p.lan", 8080), "u", "s3cret");
        c.password_ref = Some("dpapi-blob".into());
        let json = serde_json::to_string(&c).unwrap();
        assert!(
            !json.contains("s3cret"),
            "运行期明文不该出现在序列化结果里：{json}"
        );
        assert!(json.contains("dpapi-blob"));

        c.password = None;
        let back = c.with_plaintext(|stored| (stored == "dpapi-blob").then(|| "s3cret".into()));
        assert_eq!(back.password.as_deref(), Some("s3cret"));
        // 已经有明文时不覆盖（宿主要能传一份已解好的）
        let keep = back.with_plaintext(|_| Some("别的".into()));
        assert_eq!(keep.password.as_deref(), Some("s3cret"));
    }

    #[test]
    fn credentials_need_a_non_empty_username() {
        assert_eq!(cfg(ProxyKind::Http, "p.lan", 8080).credentials(), None);
        let mut c = cfg(ProxyKind::Http, "p.lan", 8080);
        c.username = Some(" ".into());
        assert_eq!(c.credentials(), None);
        // 有用户名没口令 ⇒ 空口令（交给代理去拒绝），不静默降级成匿名
        c.username = Some("u".into());
        assert_eq!(c.credentials(), Some(("u", "")));
    }

    // ---- 三态合成 ----

    #[test]
    fn resolve_covers_every_combination() {
        let global = cfg(ProxyKind::Http, "g.lan", 8080);
        let custom = cfg(ProxyKind::Socks5, "c.lan", 1080);
        assert_eq!(
            resolve(Some(&global), &ProxyChoice::FollowGlobal),
            Some(&global)
        );
        assert_eq!(resolve(None, &ProxyChoice::FollowGlobal), None);
        assert_eq!(resolve(Some(&global), &ProxyChoice::Direct), None);
        assert_eq!(
            resolve(Some(&global), &ProxyChoice::Custom(custom.clone())),
            Some(&custom)
        );
        assert_eq!(resolve(None, &ProxyChoice::Direct), None);
    }

    #[test]
    fn choice_defaults_to_follow_global_and_round_trips() {
        assert_eq!(ProxyChoice::default(), ProxyChoice::FollowGlobal);
        let custom = ProxyChoice::Custom(cfg(ProxyKind::Socks5, "c.lan", 1080));
        assert_eq!(custom.mode(), ProxyMode::Custom);
        assert_eq!(ProxyChoice::Direct.mode(), ProxyMode::Direct);
        // 非自定义档位丢弃配置本体，不把用户填过的地址偷偷留下
        assert_eq!(
            ProxyChoice::from_parts(ProxyMode::FollowGlobal, cfg(ProxyKind::Http, "x", 1)),
            ProxyChoice::FollowGlobal
        );
        assert_eq!(
            ProxyChoice::from_parts(ProxyMode::Custom, cfg(ProxyKind::Http, "x", 1)),
            ProxyChoice::Custom(cfg(ProxyKind::Http, "x", 1))
        );
    }

    #[test]
    fn mode_cycles_through_all_three() {
        assert_eq!(ProxyMode::FollowGlobal.next(), ProxyMode::Direct);
        assert_eq!(ProxyMode::Direct.next(), ProxyMode::Custom);
        assert_eq!(ProxyMode::Custom.next(), ProxyMode::FollowGlobal);
        assert_eq!(ProxyMode::ALL.len(), 3);
    }

    #[test]
    fn secret_keys_are_stable_and_per_profile() {
        assert_eq!(GLOBAL_SECRET_KEY, "proxy-global");
        assert_eq!(secret_key_for("abc"), "proxy-abc");
        assert_ne!(secret_key_for("abc"), secret_key_for("def"));
    }

    /// 口令沿用判定：目标没变才允许，改了类型 / 地址 / 端口任一都不许。
    #[test]
    fn password_inherits_only_while_the_target_is_unchanged() {
        let base = cfg(ProxyKind::Http, "10.0.0.1", 3128);
        let origin = fingerprint(&base);
        assert!(may_inherit_password(Some(&origin), &base));
        // 新建：没有来源，不沿用（免得把别的表单留下的密文当自己的）
        assert!(!may_inherit_password(None, &base));

        let mut host = base.clone();
        host.host = "10.0.0.2".into();
        assert!(!may_inherit_password(Some(&origin), &host));

        let mut port = base.clone();
        port.port = 8080;
        assert!(!may_inherit_password(Some(&origin), &port));

        let mut kind = base.clone();
        kind.kind = ProxyKind::Socks5;
        assert!(!may_inherit_password(Some(&origin), &kind));

        // 口令**不属于**指纹：改了口令还是同一个代理，沿用判定不受影响
        let mut pass = base.clone();
        pass.password = Some("typed".into());
        assert!(may_inherit_password(Some(&origin), &pass));

        // 地址两侧空白不算变化（保存前会 normalize，先 trim 再比才不误判）
        let mut padded = base.clone();
        padded.host = "  10.0.0.1 ".into();
        assert!(may_inherit_password(Some(&origin), &padded));
    }

    // ---- HTTP CONNECT ----

    #[test]
    fn http_request_carries_target_and_ends_with_a_blank_line() {
        let req = http_connect_request("example.com", 22, None);
        assert_eq!(
            req,
            "CONNECT example.com:22 HTTP/1.1\r\nHost: example.com:22\r\n\r\n"
        );
    }

    #[test]
    fn http_request_adds_basic_auth_only_when_credentials_are_given() {
        let req = http_connect_request("10.0.0.9", 22, Some(("user", "pass")));
        // dXNlcjpwYXNz 是 "user:pass" 的 base64（标准字母表）
        assert!(
            req.contains("Proxy-Authorization: Basic dXNlcjpwYXNz"),
            "{req}"
        );
        assert!(!http_connect_request("h", 1, None).contains("Proxy-Authorization"));
    }

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(b64_encode(b""), "");
        assert_eq!(b64_encode(b"a"), "YQ==");
        assert_eq!(b64_encode(b"ab"), "YWI=");
        assert_eq!(b64_encode(b"abc"), "YWJj");
        // 非 ASCII（口令里出现中文）按 UTF-8 字节编码
        assert_eq!(b64_encode("中".as_bytes()), "5Lit");
    }

    #[test]
    fn http_status_waits_for_the_blank_line_then_reads_the_code() {
        assert_eq!(parse_http_connect_status(b""), Ok(None));
        assert_eq!(parse_http_connect_status(b"HTTP/1.1 200 OK\r\n"), Ok(None));
        assert_eq!(
            parse_http_connect_status(b"HTTP/1.1 200 Connection established\r\nX: 1\r\n\r\n"),
            Ok(Some(200))
        );
        assert_eq!(
            parse_http_connect_status(b"HTTP/1.0 407 Proxy Authentication Required\r\n\r\n"),
            Ok(Some(407))
        );
    }

    #[test]
    fn http_status_rejects_non_http_and_over_long_heads() {
        let err = parse_http_connect_status(b"\x05\x02\x00\x02\r\n\r\n").unwrap_err();
        assert!(err.contains("不是 HTTP 响应"), "{err}");
        let mut huge = vec![b'x'; HTTP_HEAD_MAX + 1];
        huge.extend_from_slice(b"\r\n\r\n"); // 就算后面终于有结束符，也已经在限额之外
        assert!(parse_http_connect_status(&huge).is_err());
        // 恰好等于上限且没有结束符 ⇒ 还没超，继续等
        assert_eq!(
            parse_http_connect_status(&vec![b'x'; HTTP_HEAD_MAX]),
            Ok(None)
        );
    }

    // ---- SOCKS5 ----

    #[test]
    fn greeting_offers_auth_only_when_we_have_credentials() {
        assert_eq!(socks5_greeting(false), vec![0x05, 0x01, 0x00]);
        assert_eq!(socks5_greeting(true), vec![0x05, 0x02, 0x00, 0x02]);
    }

    #[test]
    fn chosen_method_waits_for_both_bytes_and_rejects_wrong_version() {
        assert_eq!(socks5_chosen_method(b""), Ok(None));
        assert_eq!(socks5_chosen_method(b"\x05"), Ok(None));
        assert_eq!(socks5_chosen_method(b"\x05\x02"), Ok(Some(0x02)));
        assert_eq!(socks5_chosen_method(b"\x05\xff"), Ok(Some(0xff)));
        let err = socks5_chosen_method(b"\x04\x00").unwrap_err();
        assert!(err.contains("版本不匹配"), "{err}");
    }

    #[test]
    fn userpass_request_encodes_lengths_in_bytes() {
        let req = socks5_userpass_request("u", "p").unwrap();
        assert_eq!(req, vec![0x01, 1, b'u', 1, b'p']);
        // 一个汉字三字节：长度按字节算，不是字符数
        let zh = socks5_userpass_request("中", "").unwrap();
        assert_eq!(zh, vec![0x01, 3, 0xe4, 0xb8, 0xad, 0]);
    }

    #[test]
    fn userpass_request_refuses_over_long_fields() {
        let long = "a".repeat(256);
        assert!(socks5_userpass_request(&long, "p").is_err());
        assert!(socks5_userpass_request("u", &long).is_err());
    }

    #[test]
    fn userpass_reply_maps_status_to_bool() {
        assert_eq!(socks5_userpass_reply(b""), Ok(None));
        assert_eq!(socks5_userpass_reply(b"\x01\x00"), Ok(Some(true)));
        assert_eq!(socks5_userpass_reply(b"\x01\x01"), Ok(Some(false)));
        assert!(socks5_userpass_reply(b"\x02\x00").is_err());
    }

    #[test]
    fn connect_request_picks_the_address_form() {
        // IPv4 字面量走 0x01：省一次域名解析
        let v4 = socks5_connect_request("10.0.0.9", 22).unwrap();
        assert_eq!(&v4[..4], &[0x05, 0x01, 0x00, SOCKS5_ATYP_IPV4]);
        assert_eq!(&v4[4..8], &[10, 0, 0, 9]);
        assert_eq!(&v4[8..], &22u16.to_be_bytes());

        // IPv6（带方括号的 URI 写法也要认）
        let v6 = socks5_connect_request("[::1]", 22).unwrap();
        assert_eq!(v6[3], SOCKS5_ATYP_IPV6);
        assert_eq!(v6.len(), 4 + 16 + 2);

        // 域名
        let d = socks5_connect_request("example.com", 443).unwrap();
        assert_eq!(d[3], SOCKS5_ATYP_DOMAIN);
        assert_eq!(d[4] as usize, "example.com".len());
        assert_eq!(&d[5..5 + 11], b"example.com");

        // 空 / 超长域名：报错而不是截断
        assert!(socks5_connect_request("", 22).is_err());
        assert!(socks5_connect_request(&"a".repeat(256), 22).is_err());
    }

    #[test]
    fn connect_reply_walks_the_variable_address_then_reports_rep() {
        assert_eq!(socks5_connect_reply(b"\x05\x00\x00"), Ok(None));
        // IPv4 形式：4 + 4 + 2 = 10 字节
        assert_eq!(
            socks5_connect_reply(&[5, 0, 0, 1, 0, 0, 0, 0, 0x1f, 0x90]),
            Ok(Some((10, 0)))
        );
        // 只到一半 ⇒ 还不够（不能提前判成功，否则残字节会流到 SSH 协议里）
        assert_eq!(socks5_connect_reply(&[5, 0, 0, 1, 0, 0]), Ok(None));
        // 域名形式：4 + 1 + n + 2
        assert_eq!(
            socks5_connect_reply(&[5, 0x05, 0, 3, 2, b'h', b'i', 0, 80]),
            Ok(Some((9, 5)))
        );
        // 错误码照样要收齐再返回
        assert_eq!(socks5_connect_reply(&[5, 1, 0, 4]), Ok(None));
        assert_eq!(
            socks5_connect_reply(
                &[5, 7, 0, 4]
                    .iter()
                    .copied()
                    .chain([0u8; 18])
                    .collect::<Vec<_>>()
            ),
            Ok(Some((22, 7)))
        );
        assert!(socks5_connect_reply(&[5, 0, 0, 9, 0]).is_err());
    }

    #[test]
    fn rep_codes_have_human_text() {
        for rep in 1..=8u8 {
            assert!(!socks5_rep_text(rep).is_empty());
        }
        assert_ne!(socks5_rep_text(5), socks5_rep_text(4));
    }

    // ---- 握手（进程内假代理）----
    //
    // 假代理绑在 `127.0.0.1:0`（端口由内核挑），全程不出网。这里要验的是
    // **我们发出去的字节**与**我们怎么读应答**；真代理的兼容性属于实机核验
    // （见规格 §1.10「验收要点」）。

    /// 起一个假代理并把它的端口填进配置；`JoinHandle` 里是它**收到**的请求字节。
    async fn fake_proxy<F, Fut>(
        kind: ProxyKind,
        serve: F,
    ) -> (ProxyConfig, tokio::task::JoinHandle<Vec<u8>>)
    where
        F: FnOnce(tokio::net::TcpStream) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Vec<u8>> + Send,
    {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("绑定假代理");
        let port = listener.local_addr().expect("本地地址").port();
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("假代理 accept");
            serve(stream).await
        });
        (cfg(kind, "127.0.0.1", port), task)
    }

    /// 逐字节读到 `\r\n\r\n`（含）为止。HTTP 请求头专用 —— 不用 `read` 是因为
    /// 头与「隧道里偷跑过来的字节」可能落在同一个包里，多读一个字节都会让后续断言错位。
    async fn read_head(s: &mut tokio::net::TcpStream) -> Vec<u8> {
        use tokio::io::AsyncReadExt as _;
        let mut out = Vec::new();
        let mut b = [0u8; 1];
        while !out.ends_with(b"\r\n\r\n") {
            if s.read(&mut b).await.expect("读假代理请求头") == 0 {
                break;
            }
            out.push(b[0]);
        }
        out
    }

    #[test]
    fn http_connect_tunnel_reports_200_and_carries_bytes_both_ways() {
        crate::ssh::runtime().block_on(async {
            use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
            let (base, task) = fake_proxy(ProxyKind::Http, |mut s| async move {
                let head = read_head(&mut s).await;
                s.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                    .await
                    .unwrap();
                // 回显：把隧道里来的字节原样送回去，证明 200 之后通道真的通了
                let mut echo = [0u8; 7];
                s.read_exact(&mut echo).await.unwrap();
                s.write_all(&echo).await.unwrap();
                head
            })
            .await;
            let proxy = with_creds(base, "u", "p");
            let mut stream = open_tunnel(&proxy, "example.com", 22)
                .await
                .expect("200 之后应建立隧道");
            stream.write_all(b"SSH-2.0").await.unwrap();
            let mut back = [0u8; 7];
            stream.read_exact(&mut back).await.unwrap();
            assert_eq!(&back, b"SSH-2.0", "200 之后的字节必须原样穿过隧道");

            let sent = String::from_utf8(task.await.unwrap()).unwrap();
            assert!(
                sent.starts_with("CONNECT example.com:22 HTTP/1.1\r\n"),
                "请求行不对：{sent:?}"
            );
            assert!(sent.contains("Host: example.com:22\r\n"), "{sent:?}");
            assert!(
                sent.contains("Proxy-Authorization: Basic dTpw\r\n"),
                "凭据必须按 Basic 编码送出：{sent:?}"
            );
            assert!(
                sent.ends_with("\r\n\r\n"),
                "末尾空行不能省，否则代理会一直等头字段：{sent:?}"
            );
        });
    }

    #[test]
    fn socks5_with_credentials_walks_greeting_auth_and_connect() {
        crate::ssh::runtime().block_on(async {
            use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
            let (base, task) = fake_proxy(ProxyKind::Socks5, |mut s| async move {
                let mut seen = Vec::new();
                // ① 问候：配了口令就同时报「无认证」与「用户名/口令」，由代理挑
                let mut g = [0u8; 4];
                s.read_exact(&mut g).await.unwrap();
                seen.extend_from_slice(&g);
                assert_eq!(g, [0x05, 0x02, 0x00, 0x02]);
                s.write_all(&[0x05, 0x02]).await.unwrap();
                // ② RFC1929 认证
                let mut hdr = [0u8; 2];
                s.read_exact(&mut hdr).await.unwrap();
                let ulen = hdr[1] as usize;
                let mut ubuf = vec![0u8; ulen + 1];
                s.read_exact(&mut ubuf).await.unwrap();
                let plen = ubuf[ulen] as usize;
                let mut pbuf = vec![0u8; plen];
                s.read_exact(&mut pbuf).await.unwrap();
                seen.extend_from_slice(&hdr);
                seen.extend_from_slice(&ubuf);
                seen.extend_from_slice(&pbuf);
                s.write_all(&[0x01, 0x00]).await.unwrap();
                // ③ CONNECT（域名形式）
                let mut c = [0u8; 5];
                s.read_exact(&mut c).await.unwrap();
                let n = c[4] as usize;
                let mut rest = vec![0u8; n + 2];
                s.read_exact(&mut rest).await.unwrap();
                seen.extend_from_slice(&c);
                seen.extend_from_slice(&rest);
                s.write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
                    .await
                    .unwrap();
                seen
            })
            .await;
            let proxy = with_creds(base, "u", "p");
            let _stream = open_tunnel(&proxy, "example.com", 22)
                .await
                .expect("三段握手都应通过");

            let seen = task.await.unwrap();
            assert_eq!(&seen[0..4], &[0x05, 0x02, 0x00, 0x02], "问候");
            assert_eq!(&seen[4..6], &[0x01, 0x01], "认证请求头 = VER(1) + ULEN");
            assert_eq!(&seen[6..7], b"u");
            assert_eq!(&seen[7..8], &[0x01], "PLEN");
            assert_eq!(&seen[8..9], b"p");
            let c = &seen[9..];
            assert_eq!(c[0], 0x05, "VER");
            assert_eq!(c[1], 0x01, "CMD = CONNECT");
            assert_eq!(c[3], 0x03, "域名走 ATYP=0x03");
            assert_eq!(c[4] as usize, "example.com".len());
            assert_eq!(&c[5..16], b"example.com");
            assert_eq!(&c[16..18], &22u16.to_be_bytes(), "端口按网络序");
        });
    }

    #[test]
    fn http_407_and_502_get_different_messages() {
        crate::ssh::runtime().block_on(async {
            use tokio::io::AsyncWriteExt as _;
            let (auth, _t) = fake_proxy(ProxyKind::Http, |mut s| async move {
                let head = read_head(&mut s).await;
                s.write_all(
                    b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic\r\n\r\n",
                )
                .await
                .unwrap();
                head
            })
            .await;
            let err = open_tunnel(&auth, "10.0.0.9", 22).await.unwrap_err();
            assert!(err.contains("407"), "{err}");
            assert!(
                err.contains("要求认证") && err.contains("用户名"),
                "407 要指出「去填凭据」，而不是只说一句被拒：{err}"
            );

            // 非 407 走另一条文案：那是代理策略问题，让用户去填口令是误导
            let (gateway, _t2) = fake_proxy(ProxyKind::Http, |mut s| async move {
                let head = read_head(&mut s).await;
                s.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n").await.unwrap();
                head
            })
            .await;
            let err = open_tunnel(&gateway, "10.0.0.9", 22).await.unwrap_err();
            assert!(err.contains("502"), "{err}");
            assert!(!err.contains("要求认证"), "502 不是认证问题：{err}");
        });
    }

    #[test]
    fn socks5_rejections_have_distinct_messages() {
        crate::ssh::runtime().block_on(async {
            use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

            // 代理回 0xFF：它一个方法都不接受
            let (none, _t) = fake_proxy(ProxyKind::Socks5, |mut s| async move {
                let mut g = [0u8; 3];
                s.read_exact(&mut g).await.unwrap();
                s.write_all(&[0x05, 0xff]).await.unwrap();
                Vec::new()
            })
            .await;
            let err = open_tunnel(&none, "example.com", 22).await.unwrap_err();
            assert!(err.contains("拒绝了所有可用的认证方式"), "{err}");

            // 代理选了「用户名/口令」而我们没配：不能硬着头皮发一段空凭据
            let (wants_auth, _t2) = fake_proxy(ProxyKind::Socks5, |mut s| async move {
                let mut g = [0u8; 3];
                s.read_exact(&mut g).await.unwrap();
                s.write_all(&[0x05, 0x02]).await.unwrap();
                Vec::new()
            })
            .await;
            let err = open_tunnel(&wants_auth, "example.com", 22)
                .await
                .unwrap_err();
            assert!(err.contains("未配置代理凭据"), "{err}");

            // 凭据不对：代理回 STATUS=1
            let (bad_creds, _t3) = fake_proxy(ProxyKind::Socks5, |mut s| async move {
                let mut g = [0u8; 4];
                s.read_exact(&mut g).await.unwrap();
                s.write_all(&[0x05, 0x02]).await.unwrap();
                let mut hdr = [0u8; 2];
                s.read_exact(&mut hdr).await.unwrap();
                let mut rest = vec![0u8; hdr[1] as usize + 1];
                s.read_exact(&mut rest).await.unwrap();
                let mut pw = vec![0u8; *rest.last().unwrap() as usize];
                s.read_exact(&mut pw).await.unwrap();
                s.write_all(&[0x01, 0x01]).await.unwrap();
                Vec::new()
            })
            .await;
            let err = open_tunnel(&with_creds(bad_creds, "u", "p"), "example.com", 22)
                .await
                .unwrap_err();
            assert!(err.contains("认证失败"), "{err}");
        });
    }

    #[test]
    fn socks5_rep_5_says_connection_refused() {
        crate::ssh::runtime().block_on(async {
            use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
            let (base, _t) = fake_proxy(ProxyKind::Socks5, |mut s| async move {
                let mut g = [0u8; 3];
                s.read_exact(&mut g).await.unwrap();
                s.write_all(&[0x05, 0x00]).await.unwrap();
                let mut c = [0u8; 5];
                s.read_exact(&mut c).await.unwrap();
                let mut rest = vec![0u8; c[4] as usize + 2];
                s.read_exact(&mut rest).await.unwrap();
                s.write_all(&[0x05, 0x05, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
                    .await
                    .unwrap();
                Vec::new()
            })
            .await;
            let err = open_tunnel(&base, "example.com", 22).await.unwrap_err();
            assert!(err.contains("连接被拒"), "REP=5 要有人话：{err}");
            assert!(err.contains("REP=5"), "错误码要留着，便于比对 RFC：{err}");
        });
    }

    /// 头部迟迟不给结束空行：报「超过上限」而不是无限等下去。
    #[test]
    fn http_head_over_the_cap_errors_instead_of_waiting_forever() {
        crate::ssh::runtime().block_on(async {
            use tokio::io::AsyncWriteExt as _;
            let (base, _t) = fake_proxy(ProxyKind::Http, |mut s| async move {
                let _ = read_head(&mut s).await;
                // 没有 CRLF、也没有空行：只会一直涨
                let filler = vec![b'X'; HTTP_HEAD_MAX + 64];
                let _ = s.write_all(&filler).await;
                Vec::new()
            })
            .await;
            let err = open_tunnel(&base, "example.com", 22).await.unwrap_err();
            assert!(err.contains("字节"), "要说清楚是头部超限：{err}");
        });
    }

    #[test]
    fn unreachable_proxy_reports_its_address() {
        crate::ssh::runtime().block_on(async {
            // 先绑一个端口再立刻放掉：测试期间不会有监听者
            let port = {
                let l = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                    .await
                    .unwrap();
                l.local_addr().unwrap().port()
            };
            let proxy = cfg(ProxyKind::Http, "127.0.0.1", port);
            let err = open_tunnel(&proxy, "example.com", 22).await.unwrap_err();
            assert!(err.contains("不可达"), "{err}");
            assert!(
                err.contains(&port.to_string()),
                "报错要带上代理地址，否则用户不知道是哪一个：{err}"
            );
        });
    }

    /// 配置不全时连尝试都不尝试 —— 直接说清缺什么。
    #[test]
    fn incomplete_config_fails_before_opening_a_socket() {
        crate::ssh::runtime().block_on(async {
            let proxy = cfg(ProxyKind::Http, "   ", 8080);
            let err = open_tunnel(&proxy, "example.com", 22).await.unwrap_err();
            assert!(err.contains("代理配置不完整"), "{err}");
            assert!(err.contains("代理地址"), "{err}");
        });
    }
}
