//! OpenSSH `~/.ssh/config` 解析（F-1）：把 Host 别名转成可导入的连接候选。
//!
//! ## 为什么单独一个模块
//!
//! OpenSSH 配置的语义（块式作用域、通配与取反、**首次获得优先**、`Include` 就地展开）
//! 与「连接」这个领域模型并不一致：一次解析要产出的是**候选**，还要带上「为什么这条
//! 不能用」的原因给用户看。把它塞进 `connections.rs` 会让那个已经很长的文件再多一层
//! 无关概念，所以这里只做**解析 + 转导出体**两件事，落库完全复用既有导入管道
//! （`ConnectionStore::merge_import`），不另起一套写入路径。
//!
//! ## 与 OpenSSH 语义的对齐点
//!
//! - `Host` 开新块，其后的键值对归属该块，直到下一个 `Host`。
//! - **键取值 first-wins**：按文件顺序扫描，第一个「匹配该别名且定义了该键」的块的值生效。
//!   这正是 OpenSSH 的规则，也让 `Host *` 默认块与覆盖块都能自然工作。
//! - `HostName` 缺省 → 用别名当主机名；`User` 缺省 → 本机用户名（与 `ssh` 一致）。
//! - 只导入「具体别名」：`Host *` / `Host *.corp.com` 这类纯通配块不产生条目，只作为默认值来源。
//! - 一行多个别名（`Host a b`）取**第一个字面量**：它们是同一台机器的同义词，
//!   全都导入会得到若干条完全重复的连接（去重后又只剩一条），反而让用户以为漏了。

use crate::connections::{
    profile_key, ConnectionExport, ConnectionProfile, ConnectionStore, ExportedProfile,
    EXPORT_FORMAT, EXPORT_VERSION,
};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// 导入条目默认落入的分组名（便于用户一眼识别来源）。
pub const DEFAULT_IMPORT_GROUP: &str = "导入自 ~/.ssh/config";

/// `Include` 展开的最大深度（防环、防超深嵌套把启动拖死）。
pub const MAX_INCLUDE_DEPTH: usize = 8;

/// 我们真正关心的键（小写）。来源行号与「块是否有用」都按它判定。
const KEY_OF_INTEREST: [&str; 6] = [
    "hostname",
    "user",
    "port",
    "identityfile",
    "proxyjump",
    "proxycommand",
];

/// 一条从 OpenSSH config 解析出的候选连接。
///
/// 字段刻意保持「已是可直接用的形态」：`host` 一定非空、`port` 一定是合法值、
/// `identity_file` 一定已解析成绝对路径。把「缺省/回退」留在解析期做掉，
/// 预览与落库两侧就不必各自再猜一遍。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SshConfigEntry {
    /// 别名（`Host` 的第一个字面量）——用作连接名
    pub alias: String,
    /// 有效主机名（`HostName` 或别名）
    pub host: String,
    /// 用户名；`None` = 源文件没给且本机用户名也取不到
    pub user: Option<String>,
    pub port: u16,
    /// 已解析为可直接使用的路径（`~` 与相对路径都展开过）
    pub identity_file: Option<String>,
    pub proxy_jump: Option<String>,
    pub proxy_command: Option<String>,
    /// 来源行号（预览展示，便于用户回源文件核对）
    pub source_line: usize,
    /// 并入连接备注的提示（如代理暂不支持）
    pub notes: Vec<String>,
}

impl SshConfigEntry {
    /// 备注文本（多行用换行连接）。空备注返回空串。
    pub fn notes_text(&self) -> String {
        self.notes.join("\n")
    }
}

/// 解析结果：候选条目 + 全局警告（解析错误、被忽略的通配块、Include 展开情况等）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SshConfigParse {
    pub entries: Vec<SshConfigEntry>,
    pub warnings: Vec<String>,
}

impl SshConfigParse {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// `load_config` 的结果：合并后的文本 + 解析结果 + 主文件读取失败原因。
#[derive(Debug, Clone, Default)]
pub struct SshConfigLoad {
    /// 展开 `Include` 之后的合并文本（诊断用）
    pub text: String,
    pub parse: SshConfigParse,
    /// 主文件读不到时的原因（UI 原地提示；`None` = 读到了）
    pub read_error: Option<String>,
}

/// `ssh_dir/config`。
pub fn config_path(ssh_dir: &Path) -> PathBuf {
    ssh_dir.join("config")
}

/// `<home>/.ssh`。
pub fn default_ssh_dir(home: &Path) -> PathBuf {
    home.join(".ssh")
}

/// 本机用户名（`ssh` 在 `User` 缺省时的回退口径）。
///
/// Windows 用 `USERNAME`，类 Unix 用 `USER` / `LOGNAME`。取不到返回 `None`
/// —— 宁可让这条标记「缺少用户名」，也不猜一个可能连错机器的名字。
pub fn local_username() -> Option<String> {
    ["USER", "USERNAME", "LOGNAME"]
        .iter()
        .find_map(|k| std::env::var(k).ok())
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// 用户主目录（Windows `%USERPROFILE%`，类 Unix `$HOME`）。
pub fn user_home_dir() -> Option<PathBuf> {
    std::env::var_os(if cfg!(target_os = "windows") {
        "USERPROFILE"
    } else {
        "HOME"
    })
    .map(PathBuf::from)
    .filter(|p| p.is_dir())
}

// ───────────────────────────── 解析 ─────────────────────────────

/// 一个 `Host` 块：模式表 + 键值对（保序，first-wins 靠保序实现）。
#[derive(Debug, Clone, Default)]
struct Block {
    patterns: Vec<String>,
    /// (键小写, 原始值, 行号)
    keys: Vec<(String, String, usize)>,
    /// 该块第一条键所在行（块起始行），用于条目来源行号
    line: usize,
}

impl Block {
    /// 块是否匹配某个具体别名：至少一个正向模式命中，且没有任何取反模式命中。
    ///
    /// 空模式表视为不匹配（`Host` 行没给模式是非法配置，不该悄悄变成 `*`）。
    fn matches(&self, name: &str) -> bool {
        if self.patterns.is_empty() {
            return false;
        }
        let mut positive = false;
        for pat in &self.patterns {
            match pat.strip_prefix('!') {
                Some(neg) => {
                    if glob_match(neg, name) {
                        return false;
                    }
                }
                None => {
                    if glob_match(pat, name) {
                        positive = true;
                    }
                }
            }
        }
        positive
    }

    /// 该块里某键的**第一个**值（OpenSSH first-wins；同块内重复也只认第一条）。
    fn first_of(&self, key: &str) -> Option<&str> {
        self.keys
            .iter()
            .find(|(k, _, _)| k == key)
            .map(|(_, v, _)| v.as_str())
    }

    /// 是否含字面量模式（不含 `*` / `?` / 前缀 `!`）
    fn literals(&self) -> Vec<String> {
        self.patterns
            .iter()
            .filter(|p| !p.starts_with('!'))
            .filter(|p| !p.contains('*') && !p.contains('?'))
            .filter(|p| !p.trim().is_empty())
            .cloned()
            .collect()
    }
}

/// 把行拆成 `(键小写, 值)`。支持 `Key Value` 与 `Key=Value` 两种写法；
/// 空行 / 注释 / 无值行返回 `None`。
fn split_kv(line: &str) -> Option<(String, String)> {
    let t = line.trim();
    if t.is_empty() || t.starts_with('#') {
        return None;
    }
    // 键 = 行首连续的「非空白且非 `=`」字符；值 = 其后（可选的 `=` 再）剩余部分。
    // 同时覆盖 `Key Value` / `Key=Value` / `Key = Value` 三种写法。
    let key_end = t
        .find(|c: char| c.is_whitespace() || c == '=')
        .unwrap_or(t.len());
    let (raw_key, rest) = t.split_at(key_end);
    let key = raw_key.trim().to_ascii_lowercase();
    if key.is_empty() {
        return None;
    }
    let rest = rest.trim_start();
    let raw_val = rest.strip_prefix('=').map(str::trim_start).unwrap_or(rest);
    // 行尾的行内注释：`#` 前有空白才算注释（值里出现 `#` 的写法罕见，保守处理）
    let val = match raw_val.find(" #") {
        Some(ix) => &raw_val[..ix],
        None => raw_val,
    };
    Some((key, val.trim().to_string()))
}

/// 把文本切成块。首个 `Host` 之前出现的键值对按 OpenSSH 语义归入 `Host *`
/// （全局默认），行号记为该键所在行。
fn split_blocks(text: &str) -> Vec<Block> {
    let mut blocks: Vec<Block> = Vec::new();
    let mut cur: Option<Block> = None;
    let mut global: Vec<(String, String, usize)> = Vec::new();

    for (i, raw) in text.lines().enumerate() {
        let line_no = i + 1;
        let Some((key, val)) = split_kv(raw) else {
            continue;
        };
        if key == "host" {
            if let Some(b) = cur.take() {
                blocks.push(b);
            }
            cur = Some(Block {
                patterns: val.split_whitespace().map(str::to_string).collect(),
                keys: Vec::new(),
                line: line_no,
            });
        } else if key == "include" {
            // Include 由 load_config 在读取阶段展开；纯文本解析时无文件可读，直接跳过。
            continue;
        } else if let Some(b) = cur.as_mut() {
            b.keys.push((key, val, line_no));
        } else {
            global.push((key, val, line_no));
        }
    }
    if let Some(b) = cur.take() {
        blocks.push(b);
    }

    // 全局默认块前置：它排在文件最前，first-wins 语义下正好是「兜底」
    if !global.is_empty() {
        let line = global.first().map(|(_, _, l)| *l).unwrap_or(1);
        blocks.insert(
            0,
            Block {
                patterns: vec!["*".to_string()],
                keys: global,
                line,
            },
        );
    }
    blocks
}

/// 解析 OpenSSH config 文本（**纯函数，不触盘**）。
///
/// - `ssh_dir`：`~/.ssh`，用于解析相对的 `IdentityFile`
/// - `home`：用户主目录，用于展开 `~`
/// - `user_default`：`User` 缺省时的回退（一般传 [`local_username`]）
pub fn parse_config(
    text: &str,
    ssh_dir: &Path,
    home: &Path,
    user_default: Option<&str>,
) -> SshConfigParse {
    let blocks = split_blocks(text);
    let mut warnings: Vec<String> = Vec::new();

    // 按出现顺序收集去重后的具体别名。
    // **每块只取第一个字面量**：一行多个别名（`Host a b`）是同义词，全收会得到
    // 若干条完全重复的连接；多块同名（覆盖块）只在第一次出现时收一次。
    let mut aliases: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for b in &blocks {
        if let Some(lit) = b.literals().into_iter().next() {
            let key = lit.to_ascii_lowercase();
            if seen.insert(key) {
                aliases.push(lit);
            }
        }
    }

    let mut entries: Vec<SshConfigEntry> = Vec::with_capacity(aliases.len());
    for alias in aliases {
        let entry = resolve_alias(&alias, &blocks, ssh_dir, home, user_default, &mut warnings);
        entries.push(entry);
    }

    if entries.is_empty() {
        // 有 Host 行却一条都没解析出来 ⇒ 大概率全是通配块，给一句可解释的提示
        let has_host_line = text
            .lines()
            .any(|l| split_kv(l).is_some_and(|(k, _)| k == "host"));
        if has_host_line {
            warnings.push("只发现通配 Host（如 `Host *`），没有可导入的具体主机".to_string());
        }
    }

    SshConfigParse { entries, warnings }
}

/// 对单个别名按 first-wins 解析出全部关注键，组装成条目。
fn resolve_alias(
    alias: &str,
    blocks: &[Block],
    ssh_dir: &Path,
    home: &Path,
    user_default: Option<&str>,
    warnings: &mut Vec<String>,
) -> SshConfigEntry {
    let mut hostname: Option<String> = None;
    let mut user: Option<String> = None;
    let mut port_raw: Option<(String, usize)> = None;
    let mut identity: Option<(String, usize)> = None;
    let mut jump: Option<String> = None;
    let mut proxy_cmd: Option<String> = None;
    let mut source_line: Option<usize> = None;

    for b in blocks {
        if !b.matches(alias) {
            continue;
        }
        // 来源行号 = 第一个「定义了我们关心的键」的匹配块的行
        if source_line.is_none() && KEY_OF_INTEREST.iter().any(|k| b.first_of(k).is_some()) {
            source_line = Some(b.line);
        }
        if hostname.is_none() {
            hostname = b.first_of("hostname").map(str::to_string);
        }
        if user.is_none() {
            user = b.first_of("user").map(str::to_string);
        }
        if port_raw.is_none() {
            if let Some(v) = b.first_of("port") {
                port_raw = Some((v.to_string(), b.line));
            }
        }
        if identity.is_none() {
            if let Some(v) = b.first_of("identityfile") {
                identity = Some((v.to_string(), b.line));
            }
        }
        if jump.is_none() {
            jump = b.first_of("proxyjump").map(str::to_string);
        }
        if proxy_cmd.is_none() {
            proxy_cmd = b.first_of("proxycommand").map(str::to_string);
        }
    }

    // 一个关注键都没定义的块（例如只有 ServerAliveInterval）：来源行退回该块自身行号
    let source_line = source_line
        .or_else(|| blocks.iter().find(|b| b.matches(alias)).map(|b| b.line))
        .unwrap_or(1);

    // 端口：非法值回退 22 并留警告（不静默吞掉用户的笔误）
    let port = match &port_raw {
        None => 22,
        Some((v, line)) => match v.trim().parse::<u16>() {
            Ok(p) if p > 0 => p,
            _ => {
                warnings.push(format!("`{alias}` 第 {line} 行的 Port={v} 非法，已回退 22"));
                22
            }
        },
    };

    let mut notes: Vec<String> = Vec::new();
    if let Some(j) = &jump {
        notes.push(format!("ProxyJump={j}（RustShell 暂不支持，已记入备注）"));
    }
    if let Some(c) = &proxy_cmd {
        notes.push(format!(
            "ProxyCommand={c}（RustShell 暂不支持，已记入备注）"
        ));
    }

    let user = user
        .map(|u| u.trim().to_string())
        .filter(|u| !u.is_empty())
        .or_else(|| user_default.map(str::to_string).filter(|u| !u.is_empty()));

    SshConfigEntry {
        alias: alias.to_string(),
        host: hostname
            .map(|h| h.trim().to_string())
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| alias.to_string()),
        user,
        port,
        identity_file: identity.map(|(v, _)| resolve_identity_path(&v, ssh_dir, home)),
        proxy_jump: jump,
        proxy_command: proxy_cmd,
        source_line,
        notes,
    }
}

/// `IdentityFile` 路径解析：`~/x` → home；相对路径 → `ssh_dir`；绝对路径原样。
///
/// 产出的路径**不带 `~`**：下游 `russh::keys::load_secret_key` 不做展开，
/// 留一个 `~` 进去等于让用户拿到一个「看起来对、实际打不开」的路径。
pub fn resolve_identity_path(raw: &str, ssh_dir: &Path, home: &Path) -> String {
    let p = raw.trim().trim_matches('"');
    if let Some(rest) = p.strip_prefix("~/").or_else(|| p.strip_prefix("~\\")) {
        return home.join(rest).to_string_lossy().to_string();
    }
    if p == "~" {
        return home.to_string_lossy().to_string();
    }
    let path = Path::new(p);
    if path.is_absolute() {
        return p.to_string();
    }
    ssh_dir.join(p).to_string_lossy().to_string()
}

// ───────────────────────── 转导出体 / 预览 ─────────────────────────

/// 把解析结果转成既有导入管道吃的导出体（复用 `merge_import` 的去重 / id 重排 / 分组登记）。
///
/// 只收录「可导入」条目（有主机且有用户名）；缺少用户名的条目由预览标记为跳过，
/// 不在这里悄悄放进去 —— 那会让 `merge_import` 把它当成一条连不上的记录再跳一次。
pub fn to_export(parse: &SshConfigParse, group_name: &str) -> ConnectionExport {
    let profiles: Vec<ExportedProfile> = parse
        .entries
        .iter()
        .filter(|e| !e.host.trim().is_empty() && e.user.is_some())
        .map(|e| ExportedProfile {
            name: e.alias.clone(),
            host: e.host.clone(),
            port: e.port,
            username: e.user.clone().unwrap_or_default(),
            key_path: e.identity_file.clone(),
            notes: e.notes_text(),
            group: Some(group_name.to_string()),
            tags: Vec::new(),
            // OpenSSH config 里没有端口转发的对应表示（`LocalForward` 等
            // 是配置项而不是连接属性），故导入体恒为空规则表。
            forwards: Vec::new(),
            // 会话日志（F-7）同理：它是**本机**的行为偏好，`ssh_config` 里
            // 没有任何对应表示，故导入体恒为「跟随全局」。
            session_log: None,
            // 代理（F-10）亦无对应表示：`ProxyJump` / `ProxyCommand` 是跳板机，
            // 与「走 HTTP/SOCKS 代理」不是一回事（只记进备注），故恒跟随全局。
            proxy: Default::default(),
            // 断线重连（F-4）同属本机偏好，`ssh_config` 里没有对应表示，恒跟随全局。
            auto_reconnect: None,
        })
        .collect();
    ConnectionExport {
        format: EXPORT_FORMAT.to_string(),
        version: EXPORT_VERSION,
        exported_at: 0,
        groups: vec![group_name.to_string()],
        profiles,
    }
}

/// 条目不可导入的原因。**结构化**而非中文串：core 不做 i18n，
/// 「怎么说给人听」由 UI 层经 `i18n::t` 决定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// 主机名为空（源文件既没给 HostName，别名也不可用）
    MissingHost,
    /// 源文件没给 User，且本机用户名也取不到
    MissingUser,
}

/// 预览时单条候选的状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryStatus {
    /// 可导入（与既有连接不重复）
    New,
    /// 与既有连接重复（同 host+port+user），导入时跳过
    Duplicate,
    /// 不可导入，附原因
    Skipped(SkipReason),
}

impl EntryStatus {
    pub fn is_new(&self) -> bool {
        matches!(self, EntryStatus::New)
    }
}

/// 预览条目：候选 + 状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryPlan {
    pub entry: SshConfigEntry,
    pub status: EntryStatus,
}

impl EntryPlan {
    /// 源文件里的 `user@host:port` 一行摘要（用户名缺失时用 `-`）。
    pub fn summary_line(&self) -> String {
        format!(
            "{}@{}:{}",
            self.entry.user.as_deref().unwrap_or("-"),
            self.entry.host,
            self.entry.port
        )
    }
}

/// 对每个候选判定状态（**只读，不改库**）。
///
/// 判定口径与 `merge_import` 一致（`profile_key` = host+port+user 归一化），
/// 因此「预览说新增」的条目落库时一定新增，不会出现预览与结果对不上的情况。
pub fn plan_import(store: &ConnectionStore, parse: &SshConfigParse) -> Vec<EntryPlan> {
    let existing: HashSet<String> = store
        .profiles
        .iter()
        .map(|p| profile_key(&p.host, p.port, &p.username))
        .collect();

    parse
        .entries
        .iter()
        .map(|e| {
            let status = if e.host.trim().is_empty() {
                EntryStatus::Skipped(SkipReason::MissingHost)
            } else if e.user.as_deref().unwrap_or("").trim().is_empty() {
                EntryStatus::Skipped(SkipReason::MissingUser)
            } else if existing.contains(&profile_key(
                &e.host,
                e.port,
                e.user.as_deref().unwrap_or(""),
            )) {
                EntryStatus::Duplicate
            } else {
                EntryStatus::New
            };
            EntryPlan {
                entry: e.clone(),
                status,
            }
        })
        .collect()
}

// ───────────────────────── Include 展开（IO） ─────────────────────────

/// 读取 `<ssh_dir>/config` 并解析，`Include` 指令就地展开。
///
/// 读不到主文件不算致命：返回 `read_error`，由 UI 决定是提示还是让用户另选文件。
pub fn load_config(
    config: &Path,
    ssh_dir: &Path,
    home: &Path,
    user_default: Option<&str>,
) -> SshConfigLoad {
    let mut out = String::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut seen: Vec<PathBuf> = Vec::new();

    if let Err(e) = read_with_includes(
        config,
        ssh_dir,
        home,
        MAX_INCLUDE_DEPTH,
        &mut seen,
        &mut out,
        &mut warnings,
    ) {
        return SshConfigLoad {
            text: String::new(),
            parse: SshConfigParse::default(),
            read_error: Some(e),
        };
    }

    let mut parse = parse_config(&out, ssh_dir, home, user_default);
    // Include 展开期的警告排在解析警告之前（更接近「文件层面」的问题）
    warnings.append(&mut parse.warnings);
    parse.warnings = warnings;
    SshConfigLoad {
        text: out,
        parse,
        read_error: None,
    }
}

/// 递归读文件、展开 `Include`，把内容追加到 `out`。返回主文件读取错误信息。
fn read_with_includes(
    path: &Path,
    ssh_dir: &Path,
    home: &Path,
    depth: usize,
    seen: &mut Vec<PathBuf>,
    out: &mut String,
    warnings: &mut Vec<String>,
) -> Result<(), String> {
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if seen.contains(&canonical) {
        warnings.push(format!("Include 出现环，已跳过：{}", path.display()));
        return Ok(());
    }
    if depth == 0 {
        warnings.push(format!(
            "Include 嵌套超过 {MAX_INCLUDE_DEPTH} 层，已停止展开：{}",
            path.display()
        ));
        return Ok(());
    }
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("读取 {} 失败：{e}", path.display()))?;
    seen.push(canonical);

    for line in text.lines() {
        match split_kv(line) {
            Some((k, v)) if k == "include" => {
                let targets = expand_include_paths(&v, ssh_dir, home);
                if targets.is_empty() {
                    warnings.push(format!("Include 未匹配到文件：{v}"));
                }
                for t in targets {
                    // 子文件的读失败只警告、不中断整体解析
                    if let Err(e) =
                        read_with_includes(&t, ssh_dir, home, depth - 1, seen, out, warnings)
                    {
                        warnings.push(e);
                    }
                }
            }
            _ => {
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    Ok(())
}

/// 把 `Include` 的值展开成实际文件列表：`~` 与相对路径先规范化，再对末段做 glob。
fn expand_include_paths(value: &str, ssh_dir: &Path, home: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for token in value.split_whitespace() {
        let expanded = if let Some(rest) = token
            .strip_prefix("~/")
            .or_else(|| token.strip_prefix("~\\"))
        {
            home.join(rest)
        } else {
            let p = Path::new(token);
            if p.is_absolute() {
                p.to_path_buf()
            } else {
                ssh_dir.join(p)
            }
        };
        let name = expanded
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        if !name.contains('*') && !name.contains('?') {
            if expanded.is_file() {
                out.push(expanded);
            }
            continue;
        }
        // 末段含通配：列父目录按 glob 匹配（Include 的常见用法就是 `*.conf`）
        let dir = expanded.parent().unwrap_or(ssh_dir).to_path_buf();
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut matched: Vec<PathBuf> = rd
            .flatten()
            .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
            .filter(|e| glob_match(&name, &e.file_name().to_string_lossy()))
            .map(|e| e.path())
            .collect();
        matched.sort(); // 展开次序必须稳定，否则导入结果不可复现
        out.extend(matched);
    }
    out
}

// ───────────────────────────── glob ─────────────────────────────

/// OpenSSH 风格通配匹配：`*` 任意串、`?` 单字符，**大小写不敏感**（OpenSSH Host 亦然）。
///
/// 经典「回溯 + 星号标记」写法：遇不匹配且前面有 `*` 时，让该 `*` 多吃一个字符重试。
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().flat_map(|c| c.to_lowercase()).collect();
    let t: Vec<char> = text.chars().flat_map(|c| c.to_lowercase()).collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark) = (usize::MAX, 0usize);

    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = pi;
            mark = ti;
            pi += 1;
        } else if star != usize::MAX {
            pi = star + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// 供 UI 复用的「导入可用条目」计数（预览按钮文案 `导入选中 (N)` 用）。
pub fn importable_count(plans: &[EntryPlan]) -> usize {
    plans.iter().filter(|p| p.status.is_new()).count()
}

/// 从选中集合构造导出体（UI 勾选后调用；只收状态为 `New` 的条目）。
pub fn export_selected(
    plans: &[EntryPlan],
    selected: &[usize],
    group_name: &str,
) -> ConnectionExport {
    let profiles: Vec<ExportedProfile> = selected
        .iter()
        .filter_map(|ix| plans.get(*ix))
        .filter(|p| p.status.is_new())
        .map(|p| ExportedProfile {
            name: p.entry.alias.clone(),
            host: p.entry.host.clone(),
            port: p.entry.port,
            username: p.entry.user.clone().unwrap_or_default(),
            key_path: p.entry.identity_file.clone(),
            notes: p.entry.notes_text(),
            group: Some(group_name.to_string()),
            tags: Vec::new(),
            forwards: Vec::new(),
            // 会话日志（F-7）同理：它是**本机**的行为偏好，`ssh_config` 里
            // 没有任何对应表示，故导入体恒为「跟随全局」。
            session_log: None,
            // 代理（F-10）同理（见 `to_export` 的说明）。
            proxy: Default::default(),
            // 断线重连（F-4）同理（见 `to_export` 的说明）。
            auto_reconnect: None,
        })
        .collect();
    ConnectionExport {
        format: EXPORT_FORMAT.to_string(),
        version: EXPORT_VERSION,
        exported_at: 0,
        groups: vec![group_name.to_string()],
        profiles,
    }
}

/// 便捷：把某个连接的备注/主机拼成一行（UI 列表悬停提示用）。
pub fn profile_hint(p: &ConnectionProfile) -> String {
    if p.notes.trim().is_empty() {
        p.summary_line()
    } else {
        format!("{}\n{}", p.summary_line(), p.notes.trim())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn ssh_dir() -> PathBuf {
        PathBuf::from("/home/u/.ssh")
    }
    fn home() -> PathBuf {
        PathBuf::from("/home/u")
    }

    /// glob：星号跨段、问号单字符、大小写不敏感、锚定整串。
    #[test]
    fn glob_matching_rules() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("web*", "web1"));
        assert!(glob_match("*.corp.com", "db.corp.com"));
        assert!(glob_match("db?.corp.com", "db1.corp.com"));
        assert!(!glob_match("db?.corp.com", "db12.corp.com"));
        assert!(glob_match("WEB1", "web1"), "Host 匹配应大小写不敏感");
        assert!(!glob_match("web", "web1"), "必须是整串匹配");
        assert!(glob_match("a*b*c", "aXXbYYc"));
        // 退化为纯字面量
        assert!(glob_match("prod-01", "prod-01"));
        assert!(!glob_match("prod-01", "prod-02"));
    }

    /// 基本块：HostName/User/Port/IdentityFile 逐项解析。
    #[test]
    fn parses_basic_block() {
        let text = "\
Host prod-01
  HostName 10.0.0.1
  User root
  Port 2222
  IdentityFile ~/.ssh/id_ed25519
";
        let p = parse_config(text, &ssh_dir(), &home(), None);
        assert_eq!(p.entries.len(), 1);
        let e = &p.entries[0];
        assert_eq!(e.alias, "prod-01");
        assert_eq!(e.host, "10.0.0.1");
        assert_eq!(e.user.as_deref(), Some("root"));
        assert_eq!(e.port, 2222);
        let want = home().join(".ssh/id_ed25519").to_string_lossy().to_string();
        assert_eq!(e.identity_file.as_deref(), Some(want.as_str()));
    }

    /// 缺省回退：HostName 用别名、User 用本机用户名、Port=22。
    #[test]
    fn defaults_fall_back_correctly() {
        let text = "Host bare\n";
        let p = parse_config(text, &ssh_dir(), &home(), Some("alice"));
        let e = &p.entries[0];
        assert_eq!(e.host, "bare", "无 HostName 时用别名当主机");
        assert_eq!(e.user.as_deref(), Some("alice"), "无 User 时用本机用户名");
        assert_eq!(e.port, 22);

        // 本机用户名也取不到 → user 为空（后续被标记跳过）
        let p2 = parse_config(text, &ssh_dir(), &home(), None);
        assert_eq!(p2.entries[0].user, None);
    }

    /// `Host *` 默认块：不产生条目，但其值作为兜底参与解析（first-wins）。
    #[test]
    fn wildcard_block_only_supplies_defaults() {
        let text = "\
Host prod-01
  HostName 10.0.0.1

Host *
  User deploy
  Port 2200
";
        let p = parse_config(text, &ssh_dir(), &home(), Some("alice"));
        assert_eq!(p.entries.len(), 1, "通配块不该变成连接");
        let e = &p.entries[0];
        assert_eq!(e.user.as_deref(), Some("deploy"), "应吃到 Host * 的 User");
        assert_eq!(e.port, 2200, "应吃到 Host * 的 Port");
        assert_eq!(e.host, "10.0.0.1");
    }

    /// first-wins（OpenSSH 语义）：**先出现的匹配块获胜**。
    ///
    /// 这正是 `man ssh_config` 的规则（「more host-specific declarations should be given
    /// near the beginning of the file, and general defaults at the end」）——所以官方推荐
    /// 的写法是「具体块在前、`Host *` 兜底在后」，而不是反过来。
    #[test]
    fn first_matching_block_wins() {
        // 具体块在前 → 具体块的值生效
        let text = "\
Host prod-01
  HostName 10.0.0.1
  User root

Host *
  User deploy
";
        let p = parse_config(text, &ssh_dir(), &home(), None);
        assert_eq!(
            p.entries[0].user.as_deref(),
            Some("root"),
            "具体块在前应获胜"
        );

        // `Host *` 在前 → 它先定义 User，按 first-wins 由它获胜（与 OpenSSH 一致）
        let text2 = "\
Host *
  User deploy

Host prod-01
  HostName 10.0.0.1
  User root
";
        let p2 = parse_config(text2, &ssh_dir(), &home(), None);
        let e = p2.entries.iter().find(|e| e.alias == "prod-01").unwrap();
        assert_eq!(
            e.user.as_deref(),
            Some("deploy"),
            "`Host *` 在前时先定义者获胜（first-wins）"
        );
        assert_eq!(
            e.host, "10.0.0.1",
            "HostName 只有具体块定义，仍应取到（未被前面的块抢走）"
        );
    }

    /// 首个 Host 之前的键值对按全局默认（等价 `Host *`）处理。
    #[test]
    fn leading_options_apply_as_global_defaults() {
        let text = "\
User globaluser
Port 2022

Host prod-01
  HostName 10.0.0.1
";
        let p = parse_config(text, &ssh_dir(), &home(), None);
        let e = &p.entries[0];
        assert_eq!(e.user.as_deref(), Some("globaluser"));
        assert_eq!(e.port, 2022);
    }

    /// 一行多别名：只取第一个字面量（同义词，全导会得到重复连接）。
    #[test]
    fn multi_alias_takes_first_literal() {
        let text = "Host web1 web2 web3\n  HostName 10.0.0.9\n";
        let p = parse_config(text, &ssh_dir(), &home(), Some("u"));
        assert_eq!(p.entries.len(), 1);
        assert_eq!(p.entries[0].alias, "web1");
    }

    /// 取反模式：`!` 命中的别名整块不生效。
    #[test]
    fn negated_pattern_excludes_alias() {
        let text = "\
Host *.corp.com !secret.corp.com
  User corp

Host secret.corp.com
  User secret-user
";
        let p = parse_config(text, &ssh_dir(), &home(), None);
        // 两个具体别名
        let names: Vec<&str> = p.entries.iter().map(|e| e.alias.as_str()).collect();
        assert!(names.contains(&"secret.corp.com"));
        // secret 不吃 *.corp.com 那块（取反排除），只吃自己的块
        let secret = p
            .entries
            .iter()
            .find(|e| e.alias == "secret.corp.com")
            .unwrap();
        assert_eq!(secret.user.as_deref(), Some("secret-user"));
    }

    /// 同名覆盖块：first-wins 合并（HostName 来自前块、Port 来自后块）。
    #[test]
    fn duplicate_alias_blocks_merge_first_wins() {
        let text = "\
Host foo
  HostName 1.2.3.4

Host foo
  Port 2222
";
        let p = parse_config(text, &ssh_dir(), &home(), Some("u"));
        assert_eq!(p.entries.len(), 1, "同名覆盖块只产出一条");
        let e = &p.entries[0];
        assert_eq!(e.host, "1.2.3.4");
        assert_eq!(e.port, 2222);
    }

    /// 非法 Port：回退 22 并留警告（不静默吞掉笔误）。
    #[test]
    fn invalid_port_falls_back_with_warning() {
        let text = "Host a\n  Port 70000\n";
        let p = parse_config(text, &ssh_dir(), &home(), Some("u"));
        assert_eq!(p.entries[0].port, 22);
        assert!(
            p.warnings.iter().any(|w| w.contains("Port=70000")),
            "非法端口必须留警告：{:?}",
            p.warnings
        );
    }

    /// IdentityFile 三种形态：`~/` 展开、相对按 ssh_dir、绝对原样。
    #[test]
    fn identity_file_path_resolution() {
        // 期望值用 `join` 构造，断言与平台分隔符无关
        assert_eq!(
            resolve_identity_path("~/.ssh/id_rsa", &ssh_dir(), &home()),
            home().join(".ssh/id_rsa").to_string_lossy().to_string()
        );
        assert_eq!(
            resolve_identity_path("id_ed25519", &ssh_dir(), &home()),
            ssh_dir().join("id_ed25519").to_string_lossy().to_string()
        );
        // 绝对路径原样保留（用平台自身的绝对路径：Windows 上 `/etc/...` 并不算绝对）
        let abs = std::env::temp_dir().join("key");
        let abs_s = abs.to_string_lossy().to_string();
        assert_eq!(resolve_identity_path(&abs_s, &ssh_dir(), &home()), abs_s);
        // 带引号的写法也要认
        assert_eq!(
            resolve_identity_path("\"~/.ssh/id_x\"", &ssh_dir(), &home()),
            home().join(".ssh/id_x").to_string_lossy().to_string()
        );
    }

    /// ProxyJump / ProxyCommand 记入备注并在预览可见。
    #[test]
    fn proxy_directives_are_recorded_as_notes() {
        let text = "\
Host behind
  HostName 10.0.0.5
  ProxyJump bastion
  ProxyCommand ssh -W %h:%p jump
";
        let p = parse_config(text, &ssh_dir(), &home(), Some("u"));
        let e = &p.entries[0];
        assert_eq!(e.proxy_jump.as_deref(), Some("bastion"));
        let notes = e.notes_text();
        assert!(
            notes.contains("ProxyJump=bastion"),
            "备注应含 ProxyJump：{notes}"
        );
        assert!(
            notes.contains("ProxyCommand="),
            "备注应含 ProxyCommand：{notes}"
        );
    }

    /// 注释、空行、`Key=Value` 写法、行内注释都要正确处理。
    #[test]
    fn comments_and_equals_form() {
        let text = "\
# 这是注释
Host a

  HostName=10.0.0.1   # 行内注释
  user = bob

# 尾注释
";
        let p = parse_config(text, &ssh_dir(), &home(), None);
        assert_eq!(p.entries.len(), 1);
        let e = &p.entries[0];
        assert_eq!(e.host, "10.0.0.1");
        assert_eq!(e.user.as_deref(), Some("bob"), "`user = bob` 也要认");
    }

    /// 全通配配置 → 无条目 + 明确警告。
    #[test]
    fn only_wildcards_yields_warning_and_no_entries() {
        let text = "Host *\n  User deploy\n";
        let p = parse_config(text, &ssh_dir(), &home(), None);
        assert!(p.entries.is_empty());
        assert!(
            p.warnings.iter().any(|w| w.contains("通配")),
            "应提示「只发现通配 Host」：{:?}",
            p.warnings
        );
    }

    /// to_export：只收可用条目，分组名与备注正确带出。
    #[test]
    fn to_export_only_includes_importable() {
        let text = "\
Host ok
  HostName 10.0.0.1
  User root

Host no-user
  HostName 10.0.0.2
";
        let p = parse_config(text, &ssh_dir(), &home(), None);
        let exp = to_export(&p, DEFAULT_IMPORT_GROUP);
        assert_eq!(exp.profiles.len(), 1, "缺用户名的条目不进导出体");
        assert_eq!(exp.profiles[0].name, "ok");
        assert_eq!(exp.groups, vec![DEFAULT_IMPORT_GROUP.to_string()]);
        assert_eq!(exp.profiles[0].group.as_deref(), Some(DEFAULT_IMPORT_GROUP));
    }

    /// 预览状态：新增 / 重复 / 跳过（缺用户名），与 merge_import 口径一致。
    #[test]
    fn plan_import_classifies_entries() {
        let mut store = ConnectionStore::default();
        let mut existing = ConnectionProfile::new("prod-01", "10.0.0.1", "root");
        existing.port = 2222;
        store.add_profile(existing);

        let text = "\
Host prod-01
  HostName 10.0.0.1
  User root
  Port 2222

Host fresh
  HostName 10.0.0.9
  User dev

Host no-user
  HostName 10.0.0.8
";
        let p = parse_config(text, &ssh_dir(), &home(), None);
        let plans = plan_import(&store, &p);
        assert_eq!(plans.len(), 3);
        let find = |n: &str| plans.iter().find(|x| x.entry.alias == n).unwrap();
        assert_eq!(find("prod-01").status, EntryStatus::Duplicate);
        assert_eq!(find("fresh").status, EntryStatus::New);
        assert!(matches!(find("no-user").status, EntryStatus::Skipped(_)));
        assert_eq!(importable_count(&plans), 1);
    }

    /// 预览与落库一致：plan 标 New 的条目，merge_import 后确实 added。
    #[test]
    fn plan_and_merge_agree() {
        let mut store = ConnectionStore::default();
        store.add_profile(ConnectionProfile::new("dup", "10.0.0.1", "root"));
        let text = "\
Host dup
  HostName 10.0.0.1
  User root

Host fresh
  HostName 10.0.0.9
  User dev
";
        let p = parse_config(text, &ssh_dir(), &home(), None);
        let plans = plan_import(&store, &p);
        let new_ix: Vec<usize> = plans
            .iter()
            .enumerate()
            .filter(|(_, pl)| pl.status.is_new())
            .map(|(i, _)| i)
            .collect();
        let exp = export_selected(&plans, &new_ix, DEFAULT_IMPORT_GROUP);
        let out = store.merge_import(&exp);
        assert_eq!(out.added, new_ix.len(), "预览说新增的必须真的新增");
        assert_eq!(out.added, 1);
    }

    /// Include 就地展开：子文件里的主机也被解析出来，且父文件顺序保留。
    #[test]
    fn include_expands_in_place() {
        let base = std::env::temp_dir().join("rustshell-openssh-test-include");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(
            base.join("extra.conf"),
            "Host from-include\n  HostName 10.0.0.7\n  User inc\n",
        )
        .unwrap();
        let cfg = base.join("config");
        std::fs::write(
            &cfg,
            "Host first\n  HostName 10.0.0.6\n  User a\n\nInclude extra.conf\n",
        )
        .unwrap();

        let load = load_config(&cfg, &base, &base, Some("u"));
        assert!(load.read_error.is_none(), "应读到主文件");
        let names: Vec<&str> = load
            .parse
            .entries
            .iter()
            .map(|e| e.alias.as_str())
            .collect();
        assert!(names.contains(&"first"));
        assert!(
            names.contains(&"from-include"),
            "Include 内容应被展开：{names:?}"
        );
        let inc = load
            .parse
            .entries
            .iter()
            .find(|e| e.alias == "from-include")
            .unwrap();
        assert_eq!(inc.host, "10.0.0.7");

        let _ = std::fs::remove_dir_all(&base);
    }

    /// Include 环：不挂死、记警告、其余照常解析。
    #[test]
    fn include_cycle_is_guarded() {
        let base = std::env::temp_dir().join("rustshell-openssh-test-cycle");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let a = base.join("a.conf");
        let b = base.join("b.conf");
        std::fs::write(&a, "Host ha\n  HostName 10.0.0.1\nInclude b.conf\n").unwrap();
        std::fs::write(&b, "Host hb\n  HostName 10.0.0.2\nInclude a.conf\n").unwrap();

        let load = load_config(&a, &base, &base, Some("u"));
        assert!(load.read_error.is_none());
        let names: Vec<&str> = load
            .parse
            .entries
            .iter()
            .map(|e| e.alias.as_str())
            .collect();
        assert!(names.contains(&"ha") && names.contains(&"hb"));
        assert!(
            load.parse.warnings.iter().any(|w| w.contains("环")),
            "应记录环警告：{:?}",
            load.parse.warnings
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// 主文件不存在的调用方行为：给出原因而不是空结果冒充成功。
    #[test]
    fn missing_config_reports_read_error() {
        let missing = std::env::temp_dir()
            .join("rustshell-openssh-does-not-exist")
            .join("config");
        let load = load_config(&missing, &ssh_dir(), &home(), None);
        assert!(load.read_error.is_some(), "读不到主文件必须给出原因");
        assert!(load.parse.entries.is_empty());
    }
}
