// release 版**不带控制台窗口**：Windows 上 Rust 默认把二进制链成 console 子系统，
// 于是双击 exe 会先弹出一个黑框（标题是 exe 路径、内容是启动日志），
// 对 GUI 应用是纯噪音 —— 用户看到的第一个「界面」不该是个命令行窗口。
//
// 为什么用 `cfg_attr(not(debug_assertions))` 而不是无条件声明：
// debug 构建保留 console 子系统，`cargo run` 时启动日志、panic backtrace、
// `dbg!` 直接印在当前终端里，本地排障不必再去翻日志文件；只有 release 才切成 GUI。
//
// 日志不依赖控制台：`rustshell_platform::logging` 是进程级**文件** logger
// （`logs/<yyyy-MM-dd>/rustshell.log`），panic hook 也把 backtrace 落盘（见同文件），
// 所以去掉黑框只是丢掉 stdout 的那一份副本，不会丢诊断能力。
//
// 注意副作用：GUI 子系统下 stdout/stderr 与 stdin 都不可用（Windows 不再分配、
// 也不会继承调用方的控制台）。因此**不要**在 release 里依赖 `println!`/`stdin`：
// 需要输出走 `log::*`（落文件），需要交互走窗口。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    rustshell_shell::app::run();
}
