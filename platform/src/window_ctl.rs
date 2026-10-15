//! 主窗口的原生可见性控制（F-8 系统托盘）。
//!
//! 「最小化到托盘」不是 `minimize_window()` —— 那个只是最小化到任务栏，
//! 窗口还在任务栏上占一格。真正要的是把窗口**藏起来**（`SW_HIDE`），
//! 任务栏按钮随之消失，只剩下通知区的托盘图标。
//!
//! gpui 没有暴露隐藏/显示窗口的接口（只有 `remove_window` 与 `minimize_window`），
//! 所以这里绕过它直接调 Win32。句柄由 shell 层在创建窗口时登记一次
//! （[`set_main_hwnd`]），此后 UI 侧（`poll` 里消费托盘事件时）不必再持有 `Window`
//! 就能收放窗口 —— 这正是铁律 #28「需要 `Window` 的延迟动作一律两段式」想要避免的局面。
//!
//! 关于 gpui 的内部状态：收放触发的 `WM_SHOWWINDOW` 会被 gpui 自己收到
//! （主线程消息循环派发全部本线程窗口的消息），它的 `is_visible` 会跟着更新，
//! 所以不会出现「gpui 以为还显示着、把帧白画在隐藏窗口上」的浪费。
//!
//! ## 为什么不许同步调 `ShowWindow`（2026-10-10 运行反馈）
//!
//! 用户报「托盘右键的『显示主窗口』点了没反应」。日志里那三次点击各配一条
//! `ERROR [] RefCell already borrowed` —— 这不是巧合：
//!
//! * `ShowWindow` 是**同步**的：它就地给窗口过程派发 `WM_SHOWWINDOW`；
//! * gpui 的 `WM_SHOWWINDOW` 处理会回调进 gpui 自己的状态（请求出帧、通知
//!   可见性观察者，见 `gpui-pre-windows` 的 `handle_window_visibility_changed`）；
//! * 而调用点（`MainView::poll` 消费托盘菜单）正处在 `Entity::update` 的借用里
//!   ⇒ 那次回调成了**重入借用**，gpui 立刻 panic（`RefCell already borrowed`，
//!   被 gpui 的窗口过程兜住、只留在日志里），`ShowWindow` 于中途夭折。
//!   症状就是「点了毫无反应」，而且**完全不报错到界面上**。
//!
//! 对照实验也在同一份日志里：隐藏方向的调用从来不 panic —— 它是在窗口过程
//! （`WM_CLOSE` → 关窗守卫）里执行的，那一刻 gpui 没有借用任何人。
//!
//! 所以本模块**一律走 `ShowWindowAsync`**：它只把请求投进窗口线程的消息队列，
//! 真正的可见性变更发生在**消息循环里**（此时 gpui 没有任何借用），既不会重入、
//! 也不会被吞。gpui 自己在 `WindowsWindow::activate()` 里用的也是异步版本
//! （`ShowWindowAsync(SW_RESTORE)`）。

use std::sync::atomic::{AtomicIsize, Ordering};

/// 主窗口的原生句柄（`HWND` 的 `isize` 值）。0 = 尚未登记。
static MAIN_HWND: AtomicIsize = AtomicIsize::new(0);

/// 登记主窗口句柄。
///
/// 由 `shell` 层在 `cx.open_window` 之后立刻调用一次。之后 [`set_visible`] 就能工作。
pub fn set_main_hwnd(hwnd: isize) {
    MAIN_HWND.store(hwnd, Ordering::Release);
}

/// 已登记的主窗口句柄；未登记（含显式复位成 0）时为 `None`。
pub fn main_hwnd() -> Option<isize> {
    let hwnd = MAIN_HWND.load(Ordering::Acquire);
    (hwnd != 0).then_some(hwnd)
}

/// 收放主窗口。`visible = false` 即「藏进托盘」。
///
/// 返回**是否已经把请求交给系统**（不是「已经发生了」—— 见下面为什么是异步的）。
/// 句柄还没登记 / 句柄已经失效 / 系统拒绝投递 / 当前平台没实现时返回 `false`，
/// 调用方据此决定要不要退回「普通关闭」（规格 §1.8 边界态要求可见降级，不许静默失败）。
///
/// 抢前台**不在这里做**：`SetForegroundWindow` 同样是同步重入的，而「显示主窗口」
/// 真正把窗口提到最前的那一步交给 gpui 自己的 `Window::activate_window()`
/// （它内部 spawn 到 executor，天然不重入；见模块文档与 `ui` 侧的两段式消费）。
#[cfg(windows)]
pub fn set_visible(visible: bool) -> bool {
    use std::ffi::c_void;

    // SW_HIDE = 0、SW_SHOW = 5、SW_RESTORE = 9（winuser.h）。
    const SW_HIDE: i32 = 0;
    const SW_SHOW: i32 = 5;
    const SW_RESTORE: i32 = 9;

    #[link(name = "user32")]
    extern "system" {
        fn ShowWindowAsync(hwnd: *mut c_void, cmd: i32) -> i32;
        fn IsWindow(hwnd: *mut c_void) -> i32;
        fn IsIconic(hwnd: *mut c_void) -> i32;
    }

    let Some(hwnd) = main_hwnd() else {
        return false;
    };
    let hwnd = hwnd as *mut c_void;
    unsafe {
        // 句柄失效（窗口被销毁过）时如实上报 `false`：关窗守卫靠它决定
        // 「藏不起来就照常关」，报 true 会让用户得到一个关不掉的窗口。
        if IsWindow(hwnd) == 0 {
            return false;
        }
        let cmd = if visible {
            // 最小化的窗口给 `SW_SHOW` 只会「显示成一个最小化的窗口」（屏幕上
            // 还是什么都没有），必须 `SW_RESTORE`；**只有**最小化时才用它，
            // 否则会把用户的最大化状态一并抹平。gpui 的 `activate()` 同此口径。
            if IsIconic(hwnd) != 0 {
                SW_RESTORE
            } else {
                SW_SHOW
            }
        } else {
            SW_HIDE
        };
        // 异步：只投递，不同步派发 `WM_SHOWWINDOW`（原因见模块文档）。
        ShowWindowAsync(hwnd, cmd) != 0
    }
}

/// 非 Windows 平台没有 Win32，直接报「做不到」，由调用方降级为普通关闭。
#[cfg(not(windows))]
pub fn set_visible(_visible: bool) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_window_handle_round_trips_and_can_be_reset() {
        // 复位一次，保证用例起点确定（同进程内只有本条用例写这个静态量）。
        set_main_hwnd(0);
        assert_eq!(main_hwnd(), None, "未登记时不该给出句柄");

        set_main_hwnd(0x0000_1234_ABCD);
        assert_eq!(main_hwnd(), Some(0x0000_1234_ABCD));

        set_main_hwnd(0);
        assert_eq!(main_hwnd(), None, "0 是「未登记」的哨兵值，不是合法句柄");
    }

    #[test]
    fn hiding_without_a_registered_window_reports_failure() {
        set_main_hwnd(0);
        // 无论哪个平台，没句柄就必须如实返回 false —— UI 靠它决定降级路径，
        // 「装作藏好了」会让用户点了关闭之后窗口还在、又没有托盘可找回来。
        assert!(!set_visible(false));
    }
}
