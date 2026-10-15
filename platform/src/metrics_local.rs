//! 本机系统指标采样（：本机 sysinfo，1s 采样 到 RingBuffer）
//!
//! 两条时间尺度：`Sample` 每轮都写（CPU / 内存 / 网络 / 磁盘走势），
//! `SystemOverview`（主机名 / IP / 运行时长 / 磁盘用量）变化慢，
//! 按 [`OVERVIEW_INTERVAL`] 低频刷新，避免每轮都扫卷与网卡。

use rustshell_core::metrics::{
    bytes_delta_to_mb_per_sec, DiskInfo, MetricsStore, Sample, SystemOverview,
};
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

/// 系统概况刷新间隔：磁盘用量/主机名这类信息秒级没有意义，扫太勤只是白花 IO
const OVERVIEW_INTERVAL: Duration = Duration::from_secs(15);

/// 启动本机采样线程（detach）。interval_ms 为共享原子值，设置面板可动态调频。
pub fn spawn_local_metrics(store: Arc<MetricsStore>, interval_ms: Arc<AtomicU64>) {
    std::thread::Builder::new()
        .name("rs-metrics-local".into())
        .spawn(move || {
            let mut sys = sysinfo::System::new();
            let mut nets = sysinfo::Networks::new_with_refreshed_list();
            let mut disks = sysinfo::Disks::new_with_refreshed_list();
            let mut prev_rx: u64 = 0;
            let mut prev_tx: u64 = 0;
            let mut prev_read: u64 = 0;
            let mut prev_written: u64 = 0;
            let mut prev_t: Option<Instant> = None;
            let mut next_overview = Instant::now();

            loop {
                sys.refresh_cpu_usage();
                sys.refresh_memory();
                nets.refresh(true);
                // 磁盘 IO 每轮都要新值。只刷 `io_usage` 这一项（不刷 kind / storage）：
                // kind 只认一次就够，storage 会多打一次 statvfs，而这是每秒都在跑的循环。
                // Windows 侧读 `IOCTL_DISK_PERFORMANCE`（打开卷句柄，微秒级），
                // Linux 侧读 `/proc/diskstats`，1Hz 下都不值得省。
                disks.refresh_specifics(false, sysinfo::DiskRefreshKind::nothing().with_io_usage());
                let (read_bytes, written_bytes) = total_disk_io(&disks);

                let now_t = Instant::now();
                let dt = prev_t
                    .map(|t| now_t.duration_since(t).as_secs_f32().max(0.001))
                    .unwrap_or(0.0);

                let mut rx = 0u64;
                let mut tx = 0u64;
                for data in nets.values() {
                    rx += data.total_received();
                    tx += data.total_transmitted();
                }
                // 四条速率走同一个算式（`bytes_delta_to_mb_per_sec`），
                // 与远端 `parse_proc_snapshot` 用的是同一把尺子。
                let (net_down, net_up) = if prev_t.is_some() {
                    (
                        bytes_delta_to_mb_per_sec(rx.saturating_sub(prev_rx), dt),
                        bytes_delta_to_mb_per_sec(tx.saturating_sub(prev_tx), dt),
                    )
                } else {
                    (0.0, 0.0)
                };
                let (disk_read_mb, disk_write_mb) = if prev_t.is_some() {
                    (
                        bytes_delta_to_mb_per_sec(read_bytes.saturating_sub(prev_read), dt),
                        bytes_delta_to_mb_per_sec(written_bytes.saturating_sub(prev_written), dt),
                    )
                } else {
                    (0.0, 0.0)
                };
                prev_rx = rx;
                prev_tx = tx;
                prev_read = read_bytes;
                prev_written = written_bytes;
                prev_t = Some(now_t);

                let mem_total = sys.total_memory();
                let mem_used = sys.used_memory();
                let swap_total = sys.total_swap();
                let swap_used = sys.used_swap();
                store.push(Sample {
                    cpu_percent: sys.global_cpu_usage().clamp(0.0, 100.0),
                    mem_percent: if mem_total > 0 {
                        100.0 * mem_used as f32 / mem_total as f32
                    } else {
                        0.0
                    },
                    swap_percent: if swap_total > 0 {
                        100.0 * swap_used as f32 / swap_total as f32
                    } else {
                        0.0
                    },
                    // 绝对值供界面显示「已用 / 上限」（内存与交换分区同口径）
                    mem_used,
                    mem_total,
                    swap_used,
                    swap_total,
                    net_up,
                    net_down,
                    disk_read_mb,
                    disk_write_mb,
                    timestamp: Sample::now(),
                });

                // 概况低频刷新（首轮立即刷新，界面上不会先空十几秒）
                if now_t >= next_overview {
                    next_overview = now_t + OVERVIEW_INTERVAL;
                    disks.refresh(true);
                    store.set_overview(SystemOverview {
                        hostname: sysinfo::System::host_name().unwrap_or_default(),
                        ip: primary_ip(&nets),
                        uptime_secs: sysinfo::System::uptime(),
                        disks: local_disks(&disks),
                        timestamp: Sample::now(),
                    });
                }

                let interval = Duration::from_millis(interval_ms.load(Ordering::Relaxed).max(200));
                std::thread::sleep(interval);
            }
        })
        .expect("spawn local metrics thread");
}

/// 本机系统 IP：优先取默认路由的出口地址（UDP `connect` 只做路由选择，不发包），
/// 无默认路由（离线/纯内网无网关）时退回网卡枚举，取第一个非回环、非链路本地 IPv4。
fn primary_ip(nets: &sysinfo::Networks) -> String {
    if let Some(ip) = outbound_ip() {
        return ip;
    }
    nets.values()
        .flat_map(|d| d.ip_networks())
        .map(|n| n.addr)
        .find(is_usable_ipv4)
        .map(|a| a.to_string())
        .unwrap_or_default()
}

/// 默认路由出口 IPv4（失败返回 None）
fn outbound_ip() -> Option<String> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    // 不设超时也不发包：UDP connect 只写路由表查询结果
    sock.connect("8.8.8.8:80").ok()?;
    let addr = sock.local_addr().ok()?.ip();
    match addr {
        IpAddr::V4(v4) if !v4.is_unspecified() => Some(v4.to_string()),
        _ => None,
    }
}

/// 可用作「系统 IP」展示的地址：IPv4、非回环、非 169.254 链路本地
fn is_usable_ipv4(addr: &IpAddr) -> bool {
    match addr {
        IpAddr::V4(v4) => !v4.is_loopback() && !v4.is_link_local() && !v4.is_unspecified(),
        IpAddr::V6(_) => false,
    }
}

/// 本机文件系统用量（过滤容量为 0 的卷，按总容量降序）
fn local_disks(disks: &sysinfo::Disks) -> Vec<DiskInfo> {
    let mut list: Vec<DiskInfo> = disks
        .list()
        .iter()
        .filter(|d| d.total_space() > 0)
        .map(|d| DiskInfo {
            name: d.name().to_string_lossy().to_string(),
            mount: d.mount_point().to_string_lossy().to_string(),
            total: d.total_space(),
            used: d.total_space().saturating_sub(d.available_space()),
            // sysinfo 的可用字节数：本机没有 df 文本，可用量直接按它格式化，
            // 与远端「df 的 Available 列」同语义（不经过 total-used 那次减法）。
            avail: d.available_space(),
            // 本机走 sysinfo，没有 `df -h` 文本：展示列留空，
            // 概览存储卡会退回「按字节格式化」那条渲染路径。
            df: None,
        })
        .collect();
    list.sort_by_key(|d| std::cmp::Reverse(d.total));
    list
}

/// 全部卷的**累计**读 / 写字节（概览「磁盘」走势图的本机数据源）。
fn total_disk_io(disks: &sysinfo::Disks) -> (u64, u64) {
    sum_disk_usage(disks.list().iter().map(sysinfo::Disk::usage))
}

/// 把若干卷用量折成 `(累计读, 累计写)`。
///
/// 拆成纯函数是为了**可断言** —— 这里唯一的坑是读错字段：`DiskUsage` 同时给了
/// `read_bytes`（**自上次刷新以来**的增量）与 `total_read_bytes`（自开机累计）。
/// 拿增量去「自己算速率」会得到一个随刷新节奏变化的口径，而它在图上仍然是一条
/// 正常的曲线。用合成样本把字段选择钉死。
///
/// 本机按**卷**求和（Windows 是 `C:` / `D:`，Linux 是各挂载点的分区）：
/// 同一块物理盘上多个卷的读写互不重叠，合起来就是整盘的量，不会重复计数。
fn sum_disk_usage(usages: impl Iterator<Item = sysinfo::DiskUsage>) -> (u64, u64) {
    usages.fold((0u64, 0u64), |(r, w), u| {
        (
            r.saturating_add(u.total_read_bytes),
            w.saturating_add(u.total_written_bytes),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_and_link_local_are_not_system_ips() {
        assert!(is_usable_ipv4(&"192.168.1.10".parse().unwrap()));
        assert!(is_usable_ipv4(&"10.0.0.2".parse().unwrap()));
        assert!(!is_usable_ipv4(&"127.0.0.1".parse().unwrap()));
        assert!(!is_usable_ipv4(&"169.254.3.4".parse().unwrap()));
        assert!(!is_usable_ipv4(&"0.0.0.0".parse().unwrap()));
        assert!(!is_usable_ipv4(&"::1".parse().unwrap()));
    }

    /// 磁盘合计必须取**累计**字段（`total_*`），不是「自上次刷新以来」的增量字段。
    ///
    /// 合成样本里两个字段刻意取不同值：若实现改成读增量，
    /// 第一个断言就会从 1000+2000 变成 10+20。
    #[test]
    fn disk_io_sums_cumulative_not_delta_fields() {
        let usages = vec![
            sysinfo::DiskUsage {
                read_bytes: 10,
                total_read_bytes: 1000,
                written_bytes: 20,
                total_written_bytes: 2000,
            },
            sysinfo::DiskUsage {
                read_bytes: 11,
                total_read_bytes: 500,
                written_bytes: 21,
                total_written_bytes: 300,
            },
        ];
        assert_eq!(sum_disk_usage(usages.into_iter()), (1500, 2300));

        // 空列表（首次列表还没刷出来）不得 panic，给 0
        assert_eq!(sum_disk_usage(std::iter::empty()), (0, 0));
    }

    /// 计数回绕 / 设备热插拔导致累计值变小：差值用 saturating_sub，不得回绕成天文数字
    #[test]
    fn disk_io_delta_survives_counter_going_backwards() {
        assert_eq!(bytes_delta_to_mb_per_sec(0u64.saturating_sub(5), 1.0), 0.0);
        let cur = 100u64;
        let prev = 5_000u64; // 上一次的值反而更大（换了块盘）
        let delta = cur.saturating_sub(prev);
        assert_eq!(delta, 0, "差值必须被夹在 0，不能回绕成 u64::MAX");
        assert_eq!(bytes_delta_to_mb_per_sec(delta, 1.0), 0.0);
    }
}
