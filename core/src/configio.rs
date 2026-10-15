//! 配置 I/O 公共辅助：JSON 配置加载失败时先复制损坏文件为`<path>.broken-<unix_ts>.bak`留底，再回退默认。

/// 解析失败时保留坏文件副本。返回是否成功留底（仅日志，不打断调用方）。
pub fn backup_broken_config(path: &std::path::Path, content: &str) {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let bak = path.with_extension(format!("broken-{ts}.bak"));
    match std::fs::write(&bak, content) {
        Ok(_) => log::warn!(
            "配置解析失败，已保留坏文件副本: {} → {}",
            path.display(),
            bak.display()
        ),
        Err(e) => log::warn!("配置解析失败，且留底失败（{e}）: {}", path.display()),
    }
}
