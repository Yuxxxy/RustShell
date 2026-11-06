//! 版本更新：查（GitHub tag / release）→ 下载产物 → 落地安装。
//!
//! # 三种能力，责任分开
//!
//! - [`check_latest`]：只读。先问 `releases/latest`，拿不到（仓库只打了 tag 没建发布）
//!   就退回 `tags`，按版本号取最大的那个。判据永远是 **tag**，与发布页是谁建的无关。
//! - [`download`]：把对应平台的产物流式写到本地临时目录，并核对 GitHub 给的字节数
//!   —— 「下载完了」不能只看 `Ok(())`，半截文件也是 `Ok`。
//! - [`launch_windows_installer`]：Windows 的产物是 **NSIS 安装包**，不是主程序。
//!   不直接跑它 —— 拉起一个**隐藏的 PowerShell waiter**，等本进程（PID）真正退出
//!   之后才以 `/P /R` 跑安装器做覆盖安装。mac / linux 的 `.dmg` / `.deb` 则交给
//!   系统安装器（调用方拿到路径后 `open` 出去）。
//!
//! # 为什么不是「把下载到的 exe 复制到当前程序路径」
//!
//! 曾经这么干过，是 2026-11-04 那个「又卸载了、又没装上」事故的根因：下载的
//! `RustShell-<版本>-windows-x64.exe` 是 NSIS **安装器**，把它改名成 `rustshell.exe`
//! 后重启，跑起来的就是顶着主程序名字的安装器；旧卸载器按进程名找
//! `rustshell.exe`，确认「结束正在运行的 RustShell」时把安装器自己杀了。
//!
//! 另外，安装器也绝不能在自己的进程里覆盖自己正在运行的映像（Windows 锁写）。
//! 所以口径只有一条：**安装包永远留在下载目录，以独立进程运行**。
//!
//! # 为什么不是「点一下静默后台更新」
//!
//! 结束正在运行的进程是**不可逆**的一步，而且重启时机影响用户手上的活（终端会话、
//! 传输）。所以这里只提供「拉起安装器」这一原语，什么时候拉、拉完是否立刻重启，
//! 由 UI 那层按当前阶段决定（见 `ui/src/main_view/update_actions.rs`）。

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

use anyhow::{bail, Context as _};

/// 项目主页（「关于」页的 GitHub 入口）。
pub const REPO_URL: &str = "https://github.com/Yuxxxy/RustShell";

/// Releases 列表页：API 没给 `html_url` 时的兜底跳转目标。
pub const RELEASES_URL: &str = "https://github.com/Yuxxxy/RustShell/releases";

const LATEST_RELEASE_API: &str = "https://api.github.com/repos/Yuxxxy/RustShell/releases/latest";
const TAGS_API: &str = "https://api.github.com/repos/Yuxxxy/RustShell/tags?per_page=100";

/// 一次发布里的一个产物文件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asset {
    pub name: String,
    pub url: String,
    /// GitHub 报的字节数；0 表示没给（那就不校验大小）。
    pub size: u64,
}

/// 一次检查拿到的最新发布信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LatestRelease {
    /// 去掉前导 `v` 的版本号（如 `1.0.5`）。
    pub version: String,
    /// 发布页地址（正常情况下是该 tag 的页面）。
    pub url: String,
    /// 发布说明（Markdown 原文；只有 release 才有，tag 兜底时为空）。
    ///
    /// 2026-11-04 起 **UI 不展示**：用户要求更新对话框只给状态与主操作，
    /// 想看改了什么走「打开发布页」。字段保留是为了不改动 API 解析与用例。
    pub notes: String,
    pub assets: Vec<Asset>,
}

/// 更新用的 HTTP agent（查与下载共用）。
///
/// # 为什么必须跟着代理走（2026-11-06 真机故障）
///
/// 用户机器上 `https://api.github.com/...` **直连 200 / 0.6s**，可
/// `https://github.com/.../releases/download/...` **直连 15s 超时** —— 两个都是
/// GitHub，但在国内网络里是两条不同的路。于是症状极具欺骗性：**能查到新版本、
/// 一点下载就「下载失败」**（弹窗里只有「下载 xxx 失败」，因为当时没展开错误链）。
///
/// 而浏览器能下，是因为浏览器走系统代理。ureq 默认**完全不读**系统代理，所以这里
/// 补两条（按优先级）：
/// ① 环境变量（`ALL_PROXY` / `HTTPS_PROXY` / `HTTP_PROXY`，ureq 自带
///    [`ureq::AgentBuilder::try_proxy_from_env`]）；
/// ② **Windows 系统代理**（「Internet 选项」里那一栏）。Clash / v2rayN 之类开
///    「系统代理」时**只改注册表、不设环境变量**，光靠 ① 读不到 —— 这正是本次
///    故障的场景。mac / linux 没有等价的通用读取口子，只保留 ①。
///
/// 只在环境变量**一个都没设**时才用系统代理：显式设了环境变量的（CI、容器、
/// 手工调试）应当以环境变量为准。
fn agent() -> ureq::Agent {
    let mut builder = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(30))
        .try_proxy_from_env(true);

    if !env_proxy_is_set() {
        if let Some(url) = system_proxy() {
            match ureq::Proxy::new(&url) {
                Ok(proxy) => {
                    log::info!("更新请求走系统代理 {url}");
                    builder = builder.proxy(proxy);
                }
                // 代理配置坏了不该让更新直接哑火：退回直连，失败时错误链里
                // 会带上真实的网络原因。
                Err(e) => log::warn!("系统代理 {url} 无法解析，改走直连：{e}"),
            }
        }
    }

    builder.build()
}

/// 环境里有没有任何一条代理变量（大小写都算）。
///
/// 大小写两种都要查：Windows 上 `HTTPS_PROXY` 与 `https_proxy` 常同时存在，
/// 而 `ureq` 读的是小写那几个。
fn env_proxy_is_set() -> bool {
    const KEYS: [&str; 6] = [
        "ALL_PROXY",
        "all_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
    ];
    KEYS.iter().any(|k| std::env::var_os(k).is_some())
}

/// 读 Windows「Internet 选项 › 局域网设置」里的代理，返回 `http://host:port`。
///
/// 非 Windows 恒 `None`（那边没有统一的系统代理读取接口）。
#[cfg(windows)]
fn system_proxy() -> Option<String> {
    use winreg::enums::{HKEY_CURRENT_USER, KEY_READ};

    let key = winreg::RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(
            r"Software\Microsoft\Windows\CurrentVersion\Internet Settings",
            KEY_READ,
        )
        .ok()?;
    // `ProxyEnable` 是 DWORD，0 = 用户关了系统代理。
    if key.get_value::<u32, _>("ProxyEnable").ok()? == 0 {
        return None;
    }
    let raw: String = key.get_value("ProxyServer").ok()?;
    parse_proxy_server(&raw)
}

#[cfg(not(windows))]
fn system_proxy() -> Option<String> {
    None
}

/// 把注册表里的 `ProxyServer` 归一成 ureq 能吃的 `scheme://host:port`。
///
/// 这个值的两种合法写法都要认（实测都出现过）：
///
/// - `127.0.0.1:7897` —— 一种协议一个地址，所有请求共用；
/// - `http=127.0.0.1:7897;https=127.0.0.1:7897;socks=...` —— 分协议列表。
///   分协议时优先级 **https > http > socks**：更新全是 https 请求。
///
/// 返回 `None` 表示「没有可用的 http(s) 代理」（空值、只有 `ftp=`、只有 `socks=` 等）。
fn parse_proxy_server(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }

    let pick = |scheme: &str| -> Option<&str> {
        raw.split(';')
            .filter_map(|part| part.split_once('='))
            .find(|(k, _)| k.trim().eq_ignore_ascii_case(scheme))
            .map(|(_, v)| v.trim())
    };

    let (host, scheme) = if raw.contains('=') {
        match (pick("https"), pick("http"), pick("socks")) {
            (Some(h), _, _) => (h, "http"),
            (_, Some(h), _) => (h, "http"),
            (_, _, Some(h)) => (h, "socks5"),
            _ => return None,
        }
    } else {
        (raw, "http")
    };

    let host = host
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_start_matches("socks5://")
        .trim_end_matches('/');
    if host.is_empty() {
        return None;
    }
    Some(format!("{scheme}://{host}"))
}

fn get_json(agent: &ureq::Agent, url: &str) -> anyhow::Result<serde_json::Value> {
    let resp = agent
        .get(url)
        // GitHub API 要求带 User-Agent，缺了直接 403。
        .set(
            "User-Agent",
            concat!("RustShell/", env!("CARGO_PKG_VERSION")),
        )
        .set("Accept", "application/vnd.github+json")
        .call()
        .with_context(|| format!("请求 {url} 失败"))?;
    resp.into_json().context("解析 GitHub 响应失败")
}

/// 请求 GitHub 的最新发布。阻塞调用，调用方需放到后台线程。
///
/// 先 `releases/latest`；它 404（只打了 tag、没建 release）时退回 `tags`，
/// 取版本号最大的那条。两条路都失败才报错 —— 只打 tag 的发版方式一样能收到更新提示。
pub fn check_latest() -> anyhow::Result<LatestRelease> {
    let agent = agent();
    if let Ok(json) = get_json(&agent, LATEST_RELEASE_API) {
        if let Some(release) = parse_release(&json) {
            return Ok(release);
        }
    }
    let tags = get_json(&agent, TAGS_API).context("读不到 GitHub 标签")?;
    parse_newest_tag(&tags).context("GitHub 上既没有发布也没有版本标签")
}

/// 从 `releases/latest` 的 JSON 里取版本 / 页面 / 说明 / 产物。
fn parse_release(json: &serde_json::Value) -> Option<LatestRelease> {
    let tag = json.get("tag_name").and_then(|v| v.as_str())?;
    let url = json
        .get("html_url")
        .and_then(|v| v.as_str())
        .unwrap_or(RELEASES_URL);
    let notes = json
        .get("body")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let assets = json
        .get("assets")
        .and_then(|v| v.as_array())
        .map(|list| {
            list.iter()
                .filter_map(|a| {
                    Some(Asset {
                        name: a.get("name")?.as_str()?.to_string(),
                        url: a.get("browser_download_url")?.as_str()?.to_string(),
                        size: a.get("size").and_then(|s| s.as_u64()).unwrap_or(0),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Some(LatestRelease {
        version: normalize_tag(tag),
        url: url.to_string(),
        notes,
        assets,
    })
}

/// 从 `tags` 列表里挑版本号最大的那个（没有 release 时的兜底）。
fn parse_newest_tag(json: &serde_json::Value) -> Option<LatestRelease> {
    let list = json.as_array()?;
    let mut best: Option<String> = None;
    for entry in list {
        let Some(name) = entry.get("name").and_then(|v| v.as_str()) else {
            continue;
        };
        let version = normalize_tag(name);
        if version_parts(&version).is_none() {
            continue;
        }
        let better = match &best {
            Some(cur) => is_newer(&version, cur),
            None => true,
        };
        if better {
            best = Some(version);
        }
    }
    let version = best?;
    Some(LatestRelease {
        // tag 页面就是那棵树的页面；产物在 release 上，所以给 releases 列表更实用。
        url: RELEASES_URL.to_string(),
        version,
        notes: String::new(),
        // tag 没有 assets：调用方据此走「打开发布页」，而不是硬编一个猜出来的下载地址。
        assets: Vec::new(),
    })
}

/// `v1.2.3` → `1.2.3`。
fn normalize_tag(tag: &str) -> String {
    tag.trim().trim_start_matches(['v', 'V']).to_string()
}

/// GitHub 上的 `latest` 是否比当前版本新。解析不了的一律视为「不新」，
/// 避免把格式异常的数据渲染成「有新版本」。
pub fn is_newer(latest: &str, current: &str) -> bool {
    match (version_parts(latest), version_parts(current)) {
        (Some(latest), Some(current)) => latest > current,
        _ => false,
    }
}

/// 取 `major.minor.patch`；容忍前导 `v` 与预发布后缀（`1.2.3-beta.1`）。
fn version_parts(version: &str) -> Option<(u64, u64, u64)> {
    let core = version.trim().trim_start_matches(['v', 'V']);
    let core = core.split(['-', '+']).next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    Some((major, minor, patch))
}

// ── 产物选择 ──────────────────────────────────────────────────────────────

/// 当前平台对应哪个产物：`(平台-架构, 扩展名)`。
///
/// 与 `release.yml` 的矩阵逐字对齐（产物名 = `RustShell-<版本>-<平台>-<架构>.<ext>`，
/// 见 `Resolve version` 的 `PKG=` 一行）。**不要在这里自创名字**：名字对不上时
/// 下载会 404，而界面只会说「下载失败」，排查起来毫无线索。
pub fn target_triplet() -> Option<(&'static str, &'static str)> {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    match (os, arch) {
        ("windows", "x86_64") => Some(("windows-x64", "exe")),
        ("linux", "x86_64") => Some(("linux-x64", "deb")),
        ("linux", "aarch64") => Some(("linux-arm64", "deb")),
        // macOS 只有通用包（release.yml 用 lipo 合过），与机器架构无关。
        ("macos", _) => Some(("macos-universal", "dmg")),
        _ => None,
    }
}

/// 从产物列表里挑出当前平台要的那个。
///
/// 按**后缀**匹配而不是整名相等：文件名里的版本号写法不受本仓库控制
/// （`1.0.6` / `v1.0.6` 取决于打包脚本），而 `-windows-x64.exe` 这一段是稳定的。
pub fn pick_asset<'a>(assets: &'a [Asset], triplet: &str, ext: &str) -> Option<&'a Asset> {
    let suffix = format!("-{triplet}.{ext}");
    assets.iter().find(|a| a.name.ends_with(&suffix))
}

/// Windows 的 `.exe` 是 NSIS 安装包，安装动作要由我们自己（带 `/P /R`）拉起，
/// 而不是交给 `open` 用系统默认方式打开、更不能当裸可执行文件复制。
///
/// 注意：这里的「自己安装」**不再**意味着替换当前进程的文件 —— 见模块顶部说明。
pub fn installs_itself(ext: &str) -> bool {
    ext == "exe"
}

// ── 下载 ──────────────────────────────────────────────────────────────────

/// 把一个产物流式下载到 `dest`，并核对字节数。
///
/// 半截文件同样是 `Ok(())` —— 所以拿 GitHub 给的 `size` 做一次硬核对，
/// 不一致就报错删文件，免得把坏包交给安装步骤。
pub fn download(asset: &Asset, dest: &Path) -> anyhow::Result<()> {
    let agent = agent();
    let resp = agent
        .get(&asset.url)
        .set(
            "User-Agent",
            concat!("RustShell/", env!("CARGO_PKG_VERSION")),
        )
        .call()
        .with_context(|| format!("下载 {} 失败", asset.name))?;
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).context("建下载目录失败")?;
    }
    let mut reader = resp.into_reader();
    let mut file = std::fs::File::create(dest).context("创建下载文件失败")?;
    let mut buf = vec![0u8; 64 * 1024];
    let mut got: u64 = 0;
    loop {
        let n = reader.read(&mut buf).context("读下载流失败")?;
        if n == 0 {
            break;
        }
        std::io::Write::write_all(&mut file, &buf[..n]).context("写下载文件失败")?;
        got += n as u64;
    }
    drop(file);
    if asset.size > 0 && got != asset.size {
        let _ = std::fs::remove_file(dest);
        bail!("下载不完整（期望 {} 字节，实际 {got} 字节）", asset.size);
    }
    Ok(())
}

// ── 安装 ──────────────────────────────────────────────────────────────────

/// Windows 上「运行中的程序不能被覆盖」。shell-rs（`since2006/shell-rs`）处理同一个
/// 问题的办法是：**不让安装器去杀进程**，而是另起一个**隐藏的 PowerShell 等着** ——
/// 等本进程退出后跑安装器，装完（按需）再把程序拉起来。
///
/// 这样做的好处是安装器跑起来时本进程已经没了，目标 exe 没有任何人占着，覆盖写
/// 就是一次干净的写；也不会出现「安装器正在杀程序、程序正在退」那种谁先谁后不
/// 确定的竞态。
///
/// 脚本里**不拼任何路径**：路径全部走**环境变量**（shell-rs 的原话是 "values come
/// in through the environment, not the script's text, so no path needs quoting"），
/// 所以路径里有空格或引号也不会出事。
///
/// `/P` passive：只显示进度，跳过所有页面（尤其不会再弹「检测到旧版本」的卸载询问
/// 页），进程检查在 passive 下也不弹确认框。
/// `/R`：安装成功后由安装器 `Exec` 新主程序，用户视角就是「重启到新版本」。
/// 这里**不再由 waiter 负责拉起**——安装器已经带 `/R` 了，两个都做会起两个实例。
///
/// 刻意**不用 `Start-Process`**：它要枚举环境变量拼字典，而开发机 / CI 上常有
/// 大小写重复的键（`HTTPS_PROXY` 与 `https_proxy`），一撞上就抛
/// 「已添加项。字典中的关键字:HTTPS_PROXY」—— 更新当场哑火。调用运算符 `&`
/// 完全不碰环境字典，而且实测对 GUI 安装器 0.1s 就返回（不等它跑完，也就不会
/// 因为安装器的 `/R` 拉起新主程序、而把 waiter 拖成一条常驻进程）。
const WAITER_SCRIPT: &str = r#"$ErrorActionPreference = 'SilentlyContinue'
$waitFor = [int]$env:RSSHELL_UPDATE_PID
while (Get-Process -Id $waitFor) { Start-Sleep -Milliseconds 100 }
& $env:RSSHELL_UPDATE_SETUP /P /R
"#;

/// 拉起下载好的 Windows NSIS 安装包，做**覆盖安装**。调用方随后应退出本进程。
///
/// 历史上这里是把下载到的 exe 复制成当前程序文件（`replace_running_exe`）。**不能
/// 再这么做**：发布产物 `RustShell-<版本>-windows-x64.exe` 是 NSIS 安装器，不是主
/// 程序。把它写成 `rustshell.exe` 再重启，安装器就顶着主程序的名字运行，旧卸载器
/// 按进程名 `rustshell.exe` 找「正在运行的 RustShell」时会把安装器自己杀掉 ——
/// 于是「旧版已卸载、新版没装上」（2026-11-04）。
///
/// 首选 [`spawn_update_waiter`]（shell-rs 那一套）；万一拉不起来（比如机器上没有
/// PowerShell），退回「直接拉起安装器 + 本进程立刻退出」的老路 —— NSIS 模板里
/// 仍然有「杀运行中程序」和「让位重试」兜底。
pub fn launch_windows_installer(installer: &Path) -> anyhow::Result<()> {
    match spawn_update_waiter(installer) {
        Ok(()) => Ok(()),
        Err(e) => {
            log::warn!("拉起更新 waiter 失败（{e}），退回直接拉起安装器");
            std::process::Command::new(installer)
                .args(["/P", "/R"])
                .spawn()
                .with_context(|| format!("拉起安装包 {} 失败", installer.display()))?;
            Ok(())
        }
    }
}

/// 隐藏的 PowerShell waiter：等本进程（PID）真正退出后，再跑安装器。
///
/// 只 `spawn` 不 `wait`：waiter 是独立进程，本进程要立刻退出把 exe 的锁让出来；
/// waiter 自己会盯着 PID，直到本进程真的没了才动手。
fn spawn_update_waiter(installer: &Path) -> anyhow::Result<()> {
    let system_root =
        std::env::var_os("SystemRoot").unwrap_or_else(|| std::ffi::OsString::from(r"C:\Windows"));
    let powershell =
        PathBuf::from(system_root).join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
    let mut cmd = std::process::Command::new(powershell);
    cmd.args([
        "-NoProfile",
        "-NonInteractive",
        "-ExecutionPolicy",
        "Bypass",
        "-WindowStyle",
        "Hidden",
        "-Command",
        WAITER_SCRIPT,
    ])
    .env("RSSHELL_UPDATE_PID", std::process::id().to_string())
    .env("RSSHELL_UPDATE_SETUP", installer);
    // CREATE_NO_WINDOW：RustShell 是 GUI 程序，直接 spawn 控制台程序会闪一个黑框。
    // `-WindowStyle Hidden` 是 PowerShell 启动**之后**再隐藏（仍会闪一下），
    // CREATE_NO_WINDOW 才是从根上不创建那个控制台。
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000);
    cmd.spawn().with_context(|| "拉起更新 waiter 失败")?;
    Ok(())
}

/// 启动时清掉历史更新留下的残留；删不掉（还被占用）就忽略，下次再试。
///
/// - `<exe>.old`：老版本 `replace_running_exe` 留下的旧主程序。
/// - `<stem>.installer.old`：NSIS 模板「让位」时挪开的旧文件（见模板 Section Install）。
/// - `<临时目录>/<stem>-setup.exe`：安装器「自己顶着主程序名字运行」时复制出来的
///   那份副本（见模板 .onInit 的自愈 A）。
pub fn cleanup_stale_backup() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let Some(dir) = exe.parent() else {
        return;
    };
    let name = exe
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    if !name.is_empty() {
        let _ = std::fs::remove_file(dir.join(format!("{name}.old")));
    }
    let stem = exe
        .file_stem()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    if !stem.is_empty() {
        let _ = std::fs::remove_file(dir.join(format!("{stem}.installer.old")));
        // 自愈 A 的副本落在**系统临时目录**（NSIS 的 `$TEMP` 就是它）；下载目录
        // 那一条是给「将来把副本挪进下载目录」留的，删不到就算了。
        let _ = std::fs::remove_file(download_dir().join(format!("{stem}-setup.exe")));
        let _ = std::fs::remove_file(std::env::temp_dir().join(format!("{stem}-setup.exe")));
    }
}

/// 下载目录：`<系统临时目录>/rustshell-update/`。
pub fn download_dir() -> PathBuf {
    std::env::temp_dir().join("rustshell-update")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_numeric_versions_not_strings() {
        assert!(is_newer("1.0.5", "1.0.4"));
        assert!(is_newer("1.0.10", "1.0.9"), "按数字比较，不能按字符串");
        assert!(is_newer("1.1.0", "1.0.99"));
        assert!(is_newer("2.0.0", "1.99.99"));
        assert!(!is_newer("1.0.4", "1.0.4"));
        assert!(!is_newer("1.0.3", "1.0.4"));
    }

    #[test]
    fn tolerates_tag_prefix_and_prerelease_suffix() {
        assert!(is_newer("v1.0.5", "1.0.4"));
        assert!(is_newer("1.0.5-beta.1", "1.0.4"));
        assert!(is_newer("1.1", "1.0.9"));
        assert!(!is_newer("v1.0.4", "V1.0.4"));
        assert_eq!(normalize_tag("v1.0.5"), "1.0.5");
        assert_eq!(normalize_tag("V1.0.5"), "1.0.5");
    }

    #[test]
    fn malformed_versions_are_never_newer() {
        assert!(!is_newer("latest", "1.0.4"));
        assert!(!is_newer("", "1.0.4"));
        assert!(!is_newer("1.x.0", "1.0.4"));
        assert_eq!(REPO_URL, "https://github.com/Yuxxxy/RustShell");
    }

    /// 产物按**后缀**挑：别的平台（尤其别的架构）绝不能选中。
    #[test]
    fn picks_the_asset_for_this_platform_only() {
        let assets = vec![
            Asset {
                name: "RustShell-1.0.6-windows-x64.exe".into(),
                url: "u-win".into(),
                size: 10,
            },
            Asset {
                name: "RustShell-1.0.6-linux-x64.deb".into(),
                url: "u-lin".into(),
                size: 20,
            },
            Asset {
                name: "RustShell-1.0.6-linux-arm64.deb".into(),
                url: "u-arm".into(),
                size: 30,
            },
            Asset {
                name: "RustShell-1.0.6-macos-universal.dmg".into(),
                url: "u-mac".into(),
                size: 40,
            },
        ];
        assert_eq!(
            pick_asset(&assets, "windows-x64", "exe").map(|a| a.url.as_str()),
            Some("u-win")
        );
        assert_eq!(
            pick_asset(&assets, "linux-arm64", "deb").map(|a| a.url.as_str()),
            Some("u-arm"),
            "arm64 不能选中 x64 的包"
        );
        assert_eq!(
            pick_asset(&assets, "macos-universal", "dmg").map(|a| a.url.as_str()),
            Some("u-mac")
        );
        assert!(
            pick_asset(&assets, "windows-arm64", "exe").is_none(),
            "没出过的平台必须返回 None（界面据此走「打开发布页」）"
        );
        assert!(pick_asset(&[], "linux-x64", "deb").is_none());
    }

    /// 每个受支持的平台组合都要能落到一个 (triplet, ext) 上，且与 release.yml 拼法一致。
    #[test]
    fn the_triplet_matches_the_release_matrix() {
        // 这里只断言「本机能算出一个组合」，跨平台值由上面的 pick_asset 用例覆盖。
        if let Some((triplet, ext)) = target_triplet() {
            assert!(
                triplet.contains('-'),
                "triplet 形如 windows-x64 / linux-arm64，实际 {triplet:?}"
            );
            assert!(
                ["exe", "deb", "dmg"].contains(&ext),
                "扩展名只可能是 exe / deb / dmg，实际 {ext:?}"
            );
        }
        assert!(
            installs_itself("exe"),
            "Windows 的 .exe 是 NSIS 安装包，必须由我们自己带参数拉起"
        );
        assert!(!installs_itself("deb"), "deb 交给系统安装器");
        assert!(!installs_itself("dmg"));
    }

    /// 钉死「安装包不再被复制成主程序」+「更新交给 waiter 顺序执行」这两条口径。
    ///
    /// 这是一个**源码护栏**：把 `replace_running_exe` 加回来、或漏掉 `/P` / `/R`、
    /// 或去掉「等本进程退出再装」的顺序，编译与运行都不会立刻报错，只会在用户点
    /// 「安装并重启」时表现为「旧版卸了、新版没装」「装完不重启」或
    /// 「无法打开要写入的文件」（2026-11-05 实测：老客户端把安装包复制成主程序名
    /// 运行，安装器改不动自己正在运行的映像，Windows 锁写 ⇒ 升级必失败）。
    /// `/P` 尤其关键：没有它，安装器会弹上游那张「检测到旧版本 → 是否卸载」的页面。
    #[test]
    fn windows_update_only_launches_the_installer() {
        // include_str! 读到的是磁盘原始字节：Windows 检出是 CRLF，先把换行归一，
        // 否则下面用 "\n}\n" 找函数结尾会静默失败（本仓踩过这个坑）。
        let code = include_str!("update.rs").replace("\r\n", "\n");
        // 针要拼出来：整段写成一个字面量会扫到本用例自己（负向断言恒失败）。
        let forbidden = ["pub fn ", "replace_running_exe("].concat();
        assert!(
            !code.contains(&forbidden),
            "不许再把下载到的安装包复制/改名成当前主程序"
        );
        let at = code
            .find("pub fn launch_windows_installer")
            .expect("找不到拉起安装器的入口");
        let body = &code[at..];
        let end = body.find("\n}\n").expect("找不到函数结尾");
        let body = &body[..end];
        assert!(
            body.contains("spawn_update_waiter"),
            "必须走 shell-rs 那一套：让独立进程等本进程退出后再装"
        );
        assert!(
            !body.contains("std::fs::copy") && !body.contains("std::fs::rename"),
            "安装器必须留在下载目录，不能往程序目录写文件"
        );

        // waiter 脚本本体：等 PID 消失 → 跑安装器 → 安装器带 /P /R。
        assert!(
            WAITER_SCRIPT.contains("Get-Process -Id $waitFor"),
            "waiter 必须等本进程真的退出再去装（运行中的 exe 不能被覆盖）"
        );
        assert!(
            WAITER_SCRIPT.contains("RSSHELL_UPDATE_SETUP"),
            "安装器路径必须走环境变量，不能拼进脚本文本（路径里可能有空格/引号）"
        );
        assert!(
            WAITER_SCRIPT.contains("/P /R"),
            "必须 passive（跳过卸载询问页）且装完拉起新主程序"
        );
        assert!(
            !WAITER_SCRIPT.contains("Start-Process"),
            "不能用 Start-Process：它要枚举环境变量拼字典，大小写重复的键（HTTPS_PROXY / https_proxy）会让它当场抛错，更新直接哑火"
        );
        assert!(
            !WAITER_SCRIPT.contains("RSSHELL_UPDATE_RELAUNCH"),
            "拉起新主程序由安装器的 /R 负责；waiter 再拉一次会起两个实例"
        );
    }

    /// 钉死 waiter 的接线：环境变量名与调用方式都不能漂。
    ///
    /// 这几行是「Rust 侧」与「PowerShell 侧」之间的唯一接口，改名不改脚本就会
    /// 静默退化成「装不上」—— 没有编译错误，也没有运行时错误。
    #[test]
    fn the_waiter_gets_its_inputs_through_the_environment() {
        let code = include_str!("update.rs").replace("\r\n", "\n");
        let at = code
            .find("fn spawn_update_waiter")
            .expect("找不到 waiter 的实现");
        let body = &code[at..];
        let end = body.find("\n}\n").expect("找不到函数结尾");
        let body = &body[..end];
        for needle in [
            "RSSHELL_UPDATE_PID",
            "RSSHELL_UPDATE_SETUP",
            "std::process::id()",
            "-WindowStyle",
            "Hidden",
        ] {
            assert!(body.contains(needle), "waiter 缺少 {needle}");
        }
        // 脚本文本里不许出现任何字面路径：路径只许从环境变量来，否则带空格或
        // 引号的安装目录会把命令行拆坏（`` `&` 后面不带引号也一样安全``）。
        assert!(
            !WAITER_SCRIPT.contains(":\\") && !WAITER_SCRIPT.contains(":/"),
            "waiter 脚本里不许出现字面路径，路径必须走环境变量"
        );
        // 不能闪控制台黑框：RustShell 是 GUI 程序，spawn 控制台程序默认会弹窗。
        assert!(
            body.contains("creation_flags") && body.contains("0x0800_0000"),
            "拉起 PowerShell 必须带 CREATE_NO_WINDOW，否则用户会看到一闪而过的黑框"
        );
        // `-Wait` 会连安装器 /R 拉起的新主程序一起等 ⇒ 隐藏的 PowerShell 常驻不退。
        assert!(
            !WAITER_SCRIPT.contains("-Wait"),
            "waiter 不能用 -Wait：它会连安装器 /R 拉起的新主程序一起等，每次更新都留下一条常驻的隐藏 PowerShell"
        );
    }

    /// 2026-11-06 真机故障：**能查到新版本、一点下载就失败**。
    ///
    /// 根因是 ureq 默认不读系统代理：`api.github.com` 直连通、`github.com` 直连超时，
    /// 而浏览器走系统代理所以能下。这里钉住那条修法：agent 必须
    /// ① 读环境变量代理；② 在 Windows 上补读注册表里的系统代理。
    #[test]
    fn the_update_agent_follows_the_system_proxy() {
        let code = include_str!("update.rs").replace("\r\n", "\n");
        let at = code.find("fn agent()").expect("找不到 agent()");
        let body = &code[at..];
        let end = body.find("\n}\n").expect("找不到 agent() 结尾");
        let body = &body[..end];

        assert!(
            body.contains(".try_proxy_from_env(true)"),
            "agent 必须读环境变量代理（ALL_PROXY / HTTPS_PROXY），否则 CI 与容器里下不动"
        );
        assert!(
            body.contains("system_proxy()"),
            "agent 必须补读 Windows 系统代理：Clash 这类客户端开「系统代理」只改注册表，\
             不设环境变量，光靠 try_proxy_from_env 读不到（2026-11-06 就是栽在这）"
        );
        assert!(
            body.contains("env_proxy_is_set()"),
            "显式设了环境变量时应以环境变量为准，不能被系统代理覆盖"
        );
        assert!(
            body.contains("log::info!") || body.contains("log::warn!"),
            "用了哪条代理必须留日志，否则下次「下载失败」又只能靠猜"
        );
    }

    /// 真机联网用例（`--ignored` 才跑）：查最新版 → 挑本平台产物 → 真下载。
    ///
    /// 这条刻意**不进默认测试集**（CI 是离线的，且发版产物随时会变），
    /// 它的用途是「改完网络相关代码后，在本机证明一次端到端能通」：
    ///
    /// ```text
    /// env -u HTTPS_PROXY -u https_proxy -u HTTP_PROXY -u http_proxy \
    ///   cargo test -p rustshell-core --lib real_download -- --ignored --nocapture
    /// ```
    ///
    /// 上面显式 unset 环境变量是为了强制走**系统代理**那条路 —— 2026-11-06 的
    /// 「能查不能下」正是栽在这里：查走 api.github.com 直连通，下载走
    /// github.com/…/download 直连超时。
    #[test]
    #[ignore = "联网：需要能访问 GitHub（走系统代理）"]
    fn real_download_works_end_to_end() {
        let latest = check_latest().expect("查不到最新版（网络或代理不通）");
        println!("最新版 = {}", latest.version);
        let (triplet, ext) = target_triplet().expect("本平台没有产物");
        let asset = pick_asset(&latest.assets, triplet, ext)
            .unwrap_or_else(|| panic!("{} 没有 {} 的产物", latest.version, triplet));
        println!("产物 = {} ({} 字节)", asset.name, asset.size);

        let dest = std::env::temp_dir().join(&asset.name);
        download(asset, &dest).expect("下载失败");
        let got = std::fs::metadata(&dest).expect("下载后读不到文件").len();
        assert!(
            got > 1024 * 1024,
            "下到的文件太小（{got} 字节），不像一个安装包"
        );
        println!("落盘 = {} 字节 → {}", got, dest.display());
        let _ = std::fs::remove_file(&dest);
    }

    /// 注册表里 `ProxyServer` 的两种写法都要认。
    #[test]
    fn parses_both_shapes_of_proxy_server() {
        // ① 单一地址：所有协议共用
        assert_eq!(
            parse_proxy_server("127.0.0.1:7897").as_deref(),
            Some("http://127.0.0.1:7897")
        );
        // ② 分协议列表：更新全是 https，优先 https
        assert_eq!(
            parse_proxy_server("http=10.0.0.1:8080;https=10.0.0.1:8443").as_deref(),
            Some("http://10.0.0.1:8443")
        );
        // 只给了 http 的也能用
        assert_eq!(
            parse_proxy_server("http=10.0.0.1:8080").as_deref(),
            Some("http://10.0.0.1:8080")
        );
        // 只有 socks：转成 socks5
        assert_eq!(
            parse_proxy_server("socks=127.0.0.1:1080").as_deref(),
            Some("socks5://127.0.0.1:1080")
        );
        // 用户手抄进去带了 scheme / 结尾斜杠：别二次拼成 http://http://
        assert_eq!(
            parse_proxy_server(" http://192.168.1.2:8080/ ").as_deref(),
            Some("http://192.168.1.2:8080")
        );
        // 没有可用代理的各种写法
        assert_eq!(parse_proxy_server(""), None);
        assert_eq!(parse_proxy_server("   "), None);
        assert_eq!(parse_proxy_server("ftp=1.2.3.4:21"), None);
        assert_eq!(parse_proxy_server("https=  "), None);
    }
}
