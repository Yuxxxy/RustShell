//! TEMP 性能探针（排查「拖动侧边栏掉帧」；结论落地后整文件删除）。
//!
//! 终端 crate 不依赖 `log`，故这里只累加原子计数，由 ui 侧统一打印。

use std::sync::atomic::{AtomicU64, Ordering};

pub static RESIZE_N: AtomicU64 = AtomicU64::new(0);
pub static RESIZE_SUM_US: AtomicU64 = AtomicU64::new(0);
pub static RESIZE_MAX_US: AtomicU64 = AtomicU64::new(0);
pub static PAINT_N: AtomicU64 = AtomicU64::new(0);
pub static PAINT_SUM_US: AtomicU64 = AtomicU64::new(0);
pub static PAINT_MAX_US: AtomicU64 = AtomicU64::new(0);
pub static SEND_N: AtomicU64 = AtomicU64::new(0);
pub static SEND_SUM_US: AtomicU64 = AtomicU64::new(0);
pub static SEND_MAX_US: AtomicU64 = AtomicU64::new(0);

pub fn add_us(slot: &AtomicU64, us: u64) {
    slot.fetch_add(us, Ordering::Relaxed);
}

pub fn bump(slot: &AtomicU64) {
    slot.fetch_add(1, Ordering::Relaxed);
}

pub fn update_max(slot: &AtomicU64, v: u64) {
    slot.fetch_max(v, Ordering::Relaxed);
}

/// (计数, 总耗时us, 峰值us) 并清零窗口
pub fn take3(n: &AtomicU64, sum: &AtomicU64, max: &AtomicU64) -> (u64, u64, u64) {
    (
        n.swap(0, Ordering::Relaxed),
        sum.swap(0, Ordering::Relaxed),
        max.swap(0, Ordering::Relaxed),
    )
}
