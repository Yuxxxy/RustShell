//! 实机探针：本机磁盘读写计数**真的会动**吗？
//!
//! 为什么需要它：本机磁盘 IO 走 `sysinfo` 的卷性能计数（Windows 是
//! `IOCTL_DISK_PERFORMANCE`，Linux 是 `/proc/diskstats`）。如果某个平台上
//! 这个计数拿不到（权限不足、卷不吃这个 IOCTL、sysinfo 版本行为变化），
//! 症状是**静默恒 0** —— 概览里那张「磁盘」图永远是一条贴地的直线，
//! 不报错、不告警，而单元测试（合成样本）永远发现不了。
//!
//! 默认 `#[ignore]`：它会真的往临时目录写 16 MiB 并同步刷盘，
//! 属于「要动手跑」的验证，不该混进常规 `cargo test`。
//!
//! 跑法：
//! ```text
//! cargo test -p rustshell-platform --test disk_io_probe -- --ignored --nocapture
//! ```
//! 期望看到 `delta_write` 明显大于 0（≥ 16 MiB，因为写的就是 16 MiB，
//! 文件系统元数据可能再加一点）。若 `delta_write == 0` ⇒ 该环境下本机
//! 磁盘 IO 采集不可用，概览的磁盘图会恒为 0，需换采集手段。

use std::io::Write as _;
use std::time::Duration;

/// 全部卷的累计读 / 写字节
fn totals(disks: &sysinfo::Disks) -> (u64, u64) {
    disks.list().iter().fold((0u64, 0u64), |(r, w), d| {
        let u = d.usage();
        (
            r.saturating_add(u.total_read_bytes),
            w.saturating_add(u.total_written_bytes),
        )
    })
}

/// 只刷 IO 计数（与生产采样循环 `platform/src/metrics_local.rs` 同一条路径）
fn refresh_io(disks: &mut sysinfo::Disks) {
    disks.refresh_specifics(false, sysinfo::DiskRefreshKind::nothing().with_io_usage());
}

#[test]
#[ignore = "实机探针：会写 16MiB 到临时目录，需手动 --ignored 触发"]
fn local_disk_io_counters_actually_move() {
    const WRITE_MIB: usize = 16;
    let mut disks = sysinfo::Disks::new_with_refreshed_list();
    let before = totals(&disks);

    let path = std::env::temp_dir().join("rustshell-disk-io-probe.bin");
    {
        let mut f = std::fs::File::create(&path).expect("创建探针文件");
        f.write_all(&vec![0x5Au8; WRITE_MIB * 1024 * 1024])
            .expect("写探针文件");
        // 必须刷盘：否则数据还躺在页缓存里，卷性能计数不会动
        f.sync_all().expect("刷盘");
    }
    std::thread::sleep(Duration::from_millis(300));
    refresh_io(&mut disks);
    let after = totals(&disks);
    let _ = std::fs::remove_file(&path);

    let delta_read = after.0.saturating_sub(before.0);
    let delta_write = after.1.saturating_sub(before.1);
    println!(
        "PROBE volumes={} before={before:?} after={after:?} \
         delta_read={delta_read} delta_write={delta_write}",
        disks.list().len()
    );
    for d in disks.list() {
        let u = d.usage();
        println!(
            "  volume={:?} mount={:?} total_read={} total_written={}",
            d.name(),
            d.mount_point(),
            u.total_read_bytes,
            u.total_written_bytes
        );
    }

    assert!(
        !disks.list().is_empty(),
        "一个卷都枚举不到 ⇒ 磁盘用量卡也该是空的，先查 sysinfo 列表"
    );
    assert!(
        delta_write >= (WRITE_MIB * 1024 * 1024) as u64,
        "写了 {WRITE_MIB}MiB 但卷的写计数只涨了 {delta_write} 字节 ⇒ \
         本环境拿不到磁盘 IO 计数，概览的磁盘图会恒为 0"
    );
}
