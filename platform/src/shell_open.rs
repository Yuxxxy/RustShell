//! 文件管理器打开/定位：三平台抽象，替代硬编码`explorer`。

use std::path::Path;
use std::process::Command;

/// 在系统文件管理器中显示`path`（文件或目录）。
pub fn reveal_in_file_manager(path: &Path) -> bool {
    if cfg!(target_os = "windows") {
        Command::new("explorer")
            .arg(format!("/select,{}", path.display()))
            .spawn()
            .is_ok()
    } else if cfg!(target_os = "macos") {
        Command::new("open").arg("-R").arg(path).spawn().is_ok()
    } else {
        let dir = path.parent().unwrap_or(path);
        Command::new("xdg-open").arg(dir).spawn().is_ok()
    }
}
