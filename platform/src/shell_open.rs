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

/// 用系统默认程序**打开**`path`（不是定位）。
///
/// 2026-10-28：更新流程要把下好的安装包交给系统 —— `.dmg` 挂载、`.deb` 进包管理器，
/// 这一步只能由系统决定。与 [`reveal_in_file_manager`] 分开是因为语义不同：
/// 那个是「让我看看它在哪」，这个是「现在就开始装」。
pub fn open_path(path: &Path) -> bool {
    if cfg!(target_os = "windows") {
        // 用 cmd 的 start：ShellExecute 会把 .exe 当程序跑、其它交给关联程序。
        Command::new("cmd")
            .args(["/C", "start", ""])
            .arg(path)
            .spawn()
            .is_ok()
    } else if cfg!(target_os = "macos") {
        Command::new("open").arg(path).spawn().is_ok()
    } else {
        Command::new("xdg-open").arg(path).spawn().is_ok()
    }
}
