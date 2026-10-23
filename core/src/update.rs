//! 版本更新检查：从 GitHub Releases 取最新版本号，与当前版本比较。
//!
//! 只做「查」不做「装」：发现新版本后由用户自己去发布页下载，
//! 应用不替换自身二进制，也不在后台自动发起请求。

use std::time::Duration;

use anyhow::Context as _;

/// 项目主页（「关于」页的 GitHub 入口）。
pub const REPO_URL: &str = "https://github.com/Yuxxxy/RustShell";

/// Releases 列表页：API 没给 `html_url` 时的兜底跳转目标。
pub const RELEASES_URL: &str = "https://github.com/Yuxxxy/RustShell/releases";

const LATEST_RELEASE_API: &str = "https://api.github.com/repos/Yuxxxy/RustShell/releases/latest";

/// 一次检查拿到的最新发布信息。
#[derive(Debug, Clone)]
pub struct LatestRelease {
    /// 去掉前导 `v` 的版本号（如 `1.0.5`）。
    pub version: String,
    /// 发布页地址（正常情况下是该 tag 的页面）。
    pub url: String,
}

/// 请求 GitHub 的最新 release。阻塞调用，调用方需放到后台线程。
pub fn check_latest() -> anyhow::Result<LatestRelease> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(20))
        .build();
    let resp = agent
        .get(LATEST_RELEASE_API)
        // GitHub API 要求带 User-Agent，缺了直接 403。
        .set(
            "User-Agent",
            concat!("RustShell/", env!("CARGO_PKG_VERSION")),
        )
        .set("Accept", "application/vnd.github+json")
        .call()
        .context("请求 GitHub 最新版本失败")?;
    let json: serde_json::Value = resp.into_json().context("解析 GitHub 响应失败")?;

    let tag = json
        .get("tag_name")
        .and_then(|v| v.as_str())
        .context("GitHub 响应里没有 tag_name")?;
    let url = json
        .get("html_url")
        .and_then(|v| v.as_str())
        .unwrap_or(RELEASES_URL);

    Ok(LatestRelease {
        version: tag.trim().trim_start_matches(['v', 'V']).to_string(),
        url: url.to_string(),
    })
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
    }

    #[test]
    fn malformed_versions_are_never_newer() {
        assert!(!is_newer("latest", "1.0.4"));
        assert!(!is_newer("", "1.0.4"));
        assert!(!is_newer("1.x.0", "1.0.4"));
        assert_eq!(REPO_URL, "https://github.com/Yuxxxy/RustShell");
    }
}
