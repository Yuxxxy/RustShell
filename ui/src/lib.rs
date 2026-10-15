pub mod activity_bar;
pub mod ai_history_panel;
pub mod ai_panel;
pub mod connections_panel;
pub mod dashboard_panel;
pub mod files_panel;
pub mod keybinds;
pub mod main_view;
pub mod metrics_card;
pub mod notice;
pub mod perf_probe;
pub mod plot_chart;
pub mod quick_commands_bar;
pub mod tab_bar;
pub mod tasks_panel;
pub mod terminal_scrollbar;
pub mod terminal_view;
pub mod terminals_panel;
pub mod theme;
pub mod watchdog;

/// 扫源码的护栏测试专用（剥注释 + 切测试模块），仅测试构建编译。
#[cfg(test)]
pub mod test_support;

/// 「会话掉线 → 终端原地转断连态」的护栏（契约 / 行为 / 接线三层），仅测试构建编译。
#[cfg(test)]
mod dead_session;

/// F-8 系统托盘的护栏（关窗守卫 / 唯一消费点 / 退出直连 / 悬停提示），仅测试构建编译。
#[cfg(test)]
mod tray_wiring;

/// F-6 输入法（IME）通路的护栏（契约 / 行为 / 接线三层），仅测试构建编译。
#[cfg(test)]
mod ime_wiring;

/// F-5 粘贴保护的护栏（契约 / 行为 / 接线三层），仅测试构建编译。
#[cfg(test)]
mod paste_wiring;

/// F-10 代理设置的护栏（契约 / 行为 / 接线三层），仅测试构建编译。
#[cfg(test)]
mod proxy_wiring;

/// F-4 断线自动重连的护栏（行为 / 接线两层），仅测试构建编译。
#[cfg(test)]
mod reconnect_wiring;

/// 冷启动打点（M4 性能）：进程起点 到 首帧渲染耗时
pub static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
pub static START_LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 首帧渲染时打印冷启动耗时（仅一次；stderr 可重定向到日志）
pub fn log_cold_start_once() {
    use std::sync::atomic::Ordering;
    if !START_LOGGED.swap(true, Ordering::Relaxed) {
        if let Some(t) = START.get() {
            eprintln!(
                "[perf] cold start -> first frame: {}ms",
                t.elapsed().as_millis()
            );
        }
    }
}
