//! 配置根目录（core 侧）：与 `platform::paths::config_dir()` 同一套口径。
//!
//! core 不依赖 platform，此前各文件各自用 `dirs::config_dir()` 拼路径，既重复又
//! 不一致。统一收口到这里：`RUSTSHELL_CONFIG_DIR` 可覆盖（测试靠它把落盘产物写到
//! 临时目录，不碰开发机真实配置）。

use std::path::PathBuf;

/// 配置根目录：`<config>/rustshell/`（Windows: %APPDATA%，Unix: ~/.config）。
pub fn config_dir() -> PathBuf {
    if let Some(p) = std::env::var("RUSTSHELL_CONFIG_DIR")
        .ok()
        .filter(|p| !p.is_empty())
    {
        return PathBuf::from(p);
    }
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("rustshell")
}