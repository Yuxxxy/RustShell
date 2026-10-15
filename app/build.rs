//! 构建脚本：把应用图标与版本信息嵌入 Windows 可执行文件。

fn main() {
    // 资源脚本只在 Windows 目标下有意义（跨平台构建时跳过）。
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os != "windows" {
        return;
    }

    println!("cargo:rerun-if-changed=assets/rustshell.rc");
    println!("cargo:rerun-if-changed=assets/rustshell.ico");

    // 路径相对 crate 根（rc.exe 的工作目录即 Cargo 传入的 manifest dir），
    // 与 gpui-pre 自身`resources/windows/gpui.rc`的写法一致。
    //`CompilationResult`是`#[must_use]`，需显式消费：
    // -`manifest_optional()`：`NotWindows`/`Ok`/`NotAttempted`（缺 rc.exe）均视为成功；
    // - 只有真正`Failed`才告警。图标是「体验项」，不该让缺 SDK 的机器整包构建失败。
    let result = embed_resource::compile("assets/rustshell.rc", embed_resource::NONE);
    match result.manifest_optional() {
        Ok(()) => {}
        Err(e) => println!("cargo:warning=嵌入 Windows 图标失败（将使用默认图标）：{e}"),
    }

    // 主线程栈预留从默认 1MB 提到 16MB（2026-09-29「新增连接闪退」的正式修复）。
    //
    // Windows 主线程的栈由 PE 头 `SizeOfStackReserve` 决定（默认 1MB）；gpui 的
    // 整条绘制链（WndProc → 绘制 → 分隔层 / 对话框构建闭包）都跑在主线程上，
    // debug 构建单帧占用大 —— 打开连接对话框时 1MB 栈恰好耗尽，触发保护页，
    // 进程以 "thread 'main' has overflowed its stack" 静默终止（栈溢出越过
    // panic hook，应用日志与 WER 里都不留痕迹，现场表现为「点了 + 就闪退」）。
    // 16MB 只是虚拟地址预留、按需提交，不增加实际内存占用。
    println!("cargo:rustc-link-arg-bins=/STACK:16777216");
}
