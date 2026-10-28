//! 版本更新：查（GitHub tag / release）→ 下载产物 → 落地安装。
//!
//! # 三种能力，责任分开
//!
//! - [`check_latest`]：只读。先问 `releases/latest`，拿不到（仓库只打了 tag 没建发布）
//!   就退回 `tags`，按版本号取最大的那个。判据永远是 **tag**，与发布页是谁建的无关。
//! - [`download`]：把对应平台的产物流式写到本地临时目录，并核对 GitHub 给的字节数
//!   —— 「下载完了」不能只看 `Ok(())`，半截文件也是 `Ok`。
//! - [`replace_running_exe`]：Windows 上直接换掉当前可执行文件（先把运行中的自己
//!   改名，再放新的，失败回滚）；mac / linux 的产物是 `.dmg` / `.deb`，不由本模块
//!   安装，交给系统安装器（调用方拿到路径后 `open` 出去）。
//!
//! # 为什么不是「点一下静默后台更新」
//!
//! 换掉正在运行的进程是**不可逆**的一步，而且退出码、重启时机都影响用户手上的活
//! （终端会话、传输）。所以这里只提供「换文件」这一原语，什么时候换、换完是否立刻
//! 重启，由 UI 那层按当前阶段决定（见 `ui/src/main_view/update_actions.rs`）。

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

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
    pub notes: String,
    pub assets: Vec<Asset>,
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(30))
        .build()
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

/// 产物扩展名会不会由我们**自己**替换（只有 Windows 的裸 exe 是）。
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
        bail!(
            "下载不完整（期望 {} 字节，实际 {got} 字节）",
            asset.size
        );
    }
    Ok(())
}

// ── 安装 ──────────────────────────────────────────────────────────────────

/// 用下载好的 exe 换掉**当前正在运行**的可执行文件。
///
/// Windows 不允许覆盖运行中的 exe，但**允许改名**，所以顺序是：
/// ① 新文件先落到同目录（跨卷 rename 会失败，copy 一次最稳）
/// ② 把当前 exe 改名成 `.old`（此刻进程还在跑改名后的那个文件）
/// ③ 把新文件改名成正式名；失败就把 `.old` 改回来 —— 不允许出现「两个都没有」
///
/// `.old` **不删**：当前进程的映像还是它，删不掉（Windows 会锁住）。下次启动时
/// [`cleanup_stale_backup`] 会顺手清掉。
pub fn replace_running_exe(downloaded: &Path) -> anyhow::Result<()> {
    let exe = std::env::current_exe().context("取不到当前可执行文件路径")?;
    let dir = exe.parent().context("可执行文件没有父目录")?.to_path_buf();
    let name = exe
        .file_name()
        .context("可执行文件没有文件名")?
        .to_string_lossy()
        .to_string();
    let staged = dir.join(format!("{name}.new"));
    let backup = dir.join(format!("{name}.old"));

    std::fs::copy(downloaded, &staged).context("把新版本复制到程序目录失败")?;
    let _ = std::fs::remove_file(&backup);
    std::fs::rename(&exe, &backup).context("无法把当前程序改名（可能没有写权限）")?;
    if let Err(e) = std::fs::rename(&staged, &exe) {
        // 回滚：宁可停在旧版本，也不能留下一个没有主程序目录
        let _ = std::fs::rename(&backup, &exe);
        let _ = std::fs::remove_file(&staged);
        return Err(e).context("把新版本放到程序位置失败（已回滚到原版本）");
    }
    Ok(())
}

/// 启动时清掉上一次更新留下的 `<exe>.old`。删不掉（还被占用）就忽略。
pub fn cleanup_stale_backup() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let name = exe
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    if name.is_empty() {
        return;
    }
    if let Some(dir) = exe.parent() {
        let _ = std::fs::remove_file(dir.join(format!("{name}.old")));
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
        assert!(installs_itself("exe"), "Windows 的裸 exe 由我们自己替换");
        assert!(!installs_itself("deb"), "deb 交给系统安装器");
        assert!(!installs_itself("dmg"));
    }
}
