//! TEMP 性能探针（排查「拖动侧边栏掉帧」；结论落地后整文件删除）。
//!
//! 每 60 帧打一行 INFO 汇总，窗口内累计的耗时之和再除以帧数，避免逐帧刷日志
//! 本身污染测量（本文件只在排查期间存在，不进正式提交）。

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

pub static FRAMES: AtomicU64 = AtomicU64::new(0);
static SUM_US: AtomicU64 = AtomicU64::new(0);
static MAX_US: AtomicU64 = AtomicU64::new(0);

/// 相邻两帧 render 起点的间隔（真实帧间隔 = render 工作 + 布局/绘制 + 等 vsync）
static GAP_SUM_US: AtomicU64 = AtomicU64::new(0);
static GAP_MAX_US: AtomicU64 = AtomicU64::new(0);
/// 间隔超过 20ms 的帧数（60FPS 下一个 16.7ms 的预算，超过即肉眼可见的一顿）
static SLOW_FRAMES: AtomicU64 = AtomicU64::new(0);
static LAST_FRAME_START_US: AtomicU64 = AtomicU64::new(0);

/// 单调微秒时钟（0 用作哨兵：首帧没有「上一帧」）
fn mono_us() -> u64 {
    static EPOCH: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    let e = EPOCH.get_or_init(Instant::now);
    e.elapsed().as_micros() as u64
}

pub static TABBAR_US: AtomicU64 = AtomicU64::new(0);
pub static WORKSPACE_US: AtomicU64 = AtomicU64::new(0);
pub static QUICKBAR_US: AtomicU64 = AtomicU64::new(0);
pub static STATUSBAR_US: AtomicU64 = AtomicU64::new(0);
pub static ACTIVITY_US: AtomicU64 = AtomicU64::new(0);
pub static SIDEBAR_VIEW_US: AtomicU64 = AtomicU64::new(0);

pub fn add(slot: &AtomicU64, d: Duration) {
    slot.fetch_add(d.as_micros() as u64, Ordering::Relaxed);
}

/// 包住一次调用并计入 `slot`
pub fn timed<T>(slot: &AtomicU64, f: impl FnOnce() -> T) -> T {
    let t = Instant::now();
    let v = f();
    add(slot, t.elapsed());
    v
}

fn window(slot: &AtomicU64, frames: u64) -> u64 {
    slot.swap(0, Ordering::Relaxed) / frames.max(1)
}

/// `MainView::render` 收尾调用：累计本帧总耗时与帧间隔，每 60 帧打一行汇总。
pub fn frame(d: Duration) {
    let us = d.as_micros() as u64;
    SUM_US.fetch_add(us, Ordering::Relaxed);
    MAX_US.fetch_max(us, Ordering::Relaxed);

    let now = mono_us();
    let prev = LAST_FRAME_START_US.swap(now, Ordering::Relaxed);
    if prev != 0 {
        let gap = now.saturating_sub(prev);
        GAP_SUM_US.fetch_add(gap, Ordering::Relaxed);
        GAP_MAX_US.fetch_max(gap, Ordering::Relaxed);
        if gap > 20_000 {
            SLOW_FRAMES.fetch_add(1, Ordering::Relaxed);
        }
    }

    let n = FRAMES.fetch_add(1, Ordering::Relaxed) + 1;
    if !n.is_multiple_of(60) {
        return;
    }
    let (term_n, term_sum, term_max) = rustshell_terminal::perf::take3(
        &rustshell_terminal::perf::RESIZE_N,
        &rustshell_terminal::perf::RESIZE_SUM_US,
        &rustshell_terminal::perf::RESIZE_MAX_US,
    );
    let (send_n, send_sum, send_max) = rustshell_terminal::perf::take3(
        &rustshell_terminal::perf::SEND_N,
        &rustshell_terminal::perf::SEND_SUM_US,
        &rustshell_terminal::perf::SEND_MAX_US,
    );
    let (paint_n, paint_sum, paint_max) = rustshell_terminal::perf::take3(
        &rustshell_terminal::perf::PAINT_N,
        &rustshell_terminal::perf::PAINT_SUM_US,
        &rustshell_terminal::perf::PAINT_MAX_US,
    );
    let work_avg = window(&SUM_US, 60);
    let gap_avg = window(&GAP_SUM_US, 60);
    log::info!(
        "PERF render_avg_us={} render_max_us={} gap_avg_us={} gap_max_us={} \
         slow_frames={} rest_avg_us={} | tabbar={} workspace={} quickbar={} \
         activity={} sidebar={} | term_paint_n={} term_paint_avg_us={} term_paint_max_us={} \
         | term_resize_n={} term_resize_avg_us={} term_resize_max_us={} \
         pty_send_n={} pty_send_avg_us={} pty_send_max_us={}",
        work_avg,
        MAX_US.swap(0, Ordering::Relaxed),
        gap_avg,
        GAP_MAX_US.swap(0, Ordering::Relaxed),
        SLOW_FRAMES.swap(0, Ordering::Relaxed),
        gap_avg.saturating_sub(work_avg),
        window(&TABBAR_US, 60),
        window(&WORKSPACE_US, 60),
        window(&QUICKBAR_US, 60),
        window(&ACTIVITY_US, 60),
        window(&SIDEBAR_VIEW_US, 60),
        paint_n,
        paint_sum / paint_n.max(1),
        paint_max,
        term_n,
        term_sum / term_n.max(1),
        term_max,
        send_n,
        send_sum / send_n.max(1),
        send_max,
    );
}
