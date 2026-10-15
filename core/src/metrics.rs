//! 状态栏监控：采样结构、定容环形缓冲（300 点）与共享存储。
//!
//! 两类数据分开存：`Sample` 是每秒变化的走势（CPU/内存/网络/磁盘），
//! `SystemOverview` 是低频变化的概况（主机名 / IP / 运行时长 / 磁盘用量）。

use chrono::TimeZone as _;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// 单次采样（本机 sysinfo 或远端 SSH 轮询归一化而来）
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Sample {
    pub cpu_percent: f32,
    pub mem_percent: f32,
    pub swap_percent: f32,
    /// 已用物理内存（字节）。状态栏显示「已用 / 上限」用；`#[serde(default)]` 保证旧的历史/缓存样本仍可反序列化。
    #[serde(default)]
    pub mem_used: u64,
    /// 物理内存上限（字节）
    #[serde(default)]
    pub mem_total: u64,
    /// 已用交换分区（字节）。与 `mem_used/mem_total` 对称 —— 交换分区只给百分比时
    /// 「用了 3.60%」无法判断是 4G 里的 3.6% 还是 64G 里的 3.6%。
    /// 未启用交换（`swap_total == 0`）时界面按「—」呈现，不显示 `0B/0B (0.00%)`。
    #[serde(default)]
    pub swap_used: u64,
    /// 交换分区上限（字节）；0 = 未启用交换或上限未知
    #[serde(default)]
    pub swap_total: u64,
    /// MB/s
    pub net_up: f32,
    /// MB/s
    pub net_down: f32,
    /// 磁盘读速率（MB/s，跨全部**整盘**合计）。
    ///
    /// 名字里带单位是本结构体的**刻意例外**（其余字段的单位只写在文档注释里）：
    /// 这两个值要从两条完全不同的链路拼出来 —— 远端 `/proc/diskstats` 给的是
    /// **扇区数**（要 ×512 才是字节），本机 `sysinfo` 给的是**字节数** ——
    /// 任一侧漏掉一次换算就是成百上千倍的偏差，而那种偏差画出来仍然是
    /// 「一条像样的曲线」，肉眼看不出来。单位写进名字，至少在 review 时能对上。
    #[serde(default)]
    pub disk_read_mb: f32,
    /// 磁盘写速率（MB/s，跨全部整盘合计）
    #[serde(default)]
    pub disk_write_mb: f32,
    pub timestamp: u64,
}

impl Sample {
    pub fn now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    /// 「已用 / 上限」紧凑文案，如`7.8/15.9G`；无上限数据时返回`—`。
    pub fn mem_used_over_total(&self) -> String {
        if self.mem_total == 0 {
            return "—".to_string();
        }
        format!(
            "{}/{}",
            format_bytes_short(self.mem_used),
            format_bytes_short(self.mem_total)
        )
    }

    /// 「已用 / 上限 (百分比)」，如`7.8/15.9G (49%)`
    pub fn mem_summary(&self) -> String {
        if self.mem_total == 0 {
            return "—".to_string();
        }
        format!("{} ({:.2}%)", self.mem_used_over_total(), self.mem_percent)
    }

    /// 交换分区「已用 / 上限」紧凑文案，如`512M/2G`；未启用交换时返回`—`。
    ///
    /// 与 [`Sample::mem_used_over_total`] 同口径：分子分母都用
    /// [`format_bytes_short`]，保证两行文案的排版宽度可比。
    pub fn swap_used_over_total(&self) -> String {
        if self.swap_total == 0 {
            return "—".to_string();
        }
        format!(
            "{}/{}",
            format_bytes_short(self.swap_used),
            format_bytes_short(self.swap_total)
        )
    }

    /// 交换分区「已用 / 上限 (百分比)」，如`512M/2G (25.00%)`；未启用交换时返回`—`。
    pub fn swap_summary(&self) -> String {
        if self.swap_total == 0 {
            return "—".to_string();
        }
        format!(
            "{} ({:.2}%)",
            self.swap_used_over_total(),
            self.swap_percent
        )
    }
}

/// 字节 到 紧凑容量文案（`1.5G`/`820M`/`0B`），保留一位小数并按需去掉`.0`
pub fn format_bytes_short(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let b = bytes as f64;
    let (v, unit) = if b >= GIB {
        (b / GIB, "G")
    } else if b >= MIB {
        (b / MIB, "M")
    } else if b >= KIB {
        (b / KIB, "K")
    } else {
        (b, "B")
    };
    // 整数值不显示多余小数位：16.0G 到 16G
    if (v - v.round()).abs() < 0.05 {
        format!("{:.0}{unit}", v.round())
    } else {
        format!("{v:.1}{unit}")
    }
}

/// `df -h` 的原样列（远端 SSH 会话才有；本机 `sysinfo` 不产出 df 文本）。
///
/// 保留原样文本、不把 `-h` 的数字反解回字节：`df -h` 的值本身已带单位与
/// 四舍五入（`59G` 可能是 58.6~59.4 GiB），反解后再被界面格式化一次，两次
/// 舍入叠加反而更容易和用户手工执行 `df -h` 看到的数字对不上。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DfColumns {
    /// `Size` 列（`59G`）
    pub size: String,
    /// `Used` 列（`4.7G`）
    pub used: String,
    /// `Avail` 列（`51G`）
    pub avail: String,
    /// `Use%` 列（`9%`）
    pub use_pct: String,
}

impl DfColumns {
    /// `Use%` 的数字部分；解析不出来返回 `None`（调用方回退到按字节算）
    pub fn use_percent(&self) -> Option<f32> {
        self.use_pct
            .trim()
            .trim_end_matches('%')
            .trim()
            .parse::<f32>()
            .ok()
    }
}

/// 单个文件系统的用量。本机由 sysinfo 卷信息归一化，远端由 `df -P -k` 归一化，
/// 两侧字段一致，故概览侧栏对本地/远端会话用同一段渲染代码。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DiskInfo {
    /// 设备名（`/dev/sda1`）或卷名（`Windows`）
    pub name: String,
    /// 挂载点 / 盘符（`/`、`/home`、`C:\`）
    pub mount: String,
    /// 总容量（字节）
    pub total: u64,
    /// 已用容量（字节）
    pub used: u64,
    /// 可用容量（字节）：df 的 Available 列 / 本机 sysinfo 的 available_space。
    /// ext4 有保留块，`total - used` 会比 df 的 Avail 大几个 G —— 要跟手工
    /// `df -h` 对得上就必须单独存这一列；未知（旧数据）为 0，按 `total-used` 兜底。
    #[serde(default)]
    pub avail: u64,
    /// `df -h` 原样列；`None` = 本机（无 df 文本），渲染按字节自行格式化
    #[serde(default)]
    pub df: Option<DfColumns>,
}

impl DiskInfo {
    /// 占用率（0..=100）。
    ///
    /// 有 `df -h` 原样列时**直接用 df 自己的 `Use%`**：df 是按
    /// `used / (used + avail)` 算的，而 `used / total` 在 ext4 这类带保留块
    /// 的文件系统上会低几个百分点 —— 用户拿手工 `df -h` 一对就发现对不上。
    pub fn percent(&self) -> f32 {
        if let Some(v) = self.df.as_ref().and_then(DfColumns::use_percent) {
            return v.clamp(0.0, 100.0);
        }
        if self.total == 0 {
            0.0
        } else {
            (100.0 * self.used as f64 / self.total as f64) as f32
        }
    }

    /// 「已用 / 总量」紧凑文案：有 df 文本时用 df 的原样值（`4.7G/59G`），
    /// 否则按字节格式化成 `12.3G/50G`
    pub fn used_over_total(&self) -> String {
        match &self.df {
            Some(d) => format!("{}/{}", d.used, d.size),
            None => format!(
                "{}/{}",
                format_bytes_short(self.used),
                format_bytes_short(self.total)
            ),
        }
    }

    /// 可用容量文案：有 df 文本时原样给（`51G`），否则按字节算；
    /// 总量未知（本机拿不到、远端解析失败）时返回 `None`，界面不编造数字
    pub fn avail_text(&self) -> Option<String> {
        if let Some(d) = &self.df {
            return Some(d.avail.clone());
        }
        (self.total > 0).then(|| {
            if self.avail > 0 {
                format_bytes_short(self.avail)
            } else {
                format_bytes_short(self.total.saturating_sub(self.used))
            }
        })
    }
}

/// 系统概况：主机名 / IP / 运行时长 / 磁盘用量。
/// 变化慢（秒级采样里每十几秒刷新一次即可），故与 `Sample` 分开存放。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SystemOverview {
    /// 主机名（本地 `sysinfo`，远端 `hostname`）
    pub hostname: String,
    /// 系统 IP（本地默认路由出口地址，远端 `hostname -I` / `ip -o -4 addr`）
    pub ip: String,
    /// 系统运行时长（秒）
    pub uptime_secs: u64,
    /// 文件系统用量（已过滤伪文件系统，按总容量降序）
    pub disks: Vec<DiskInfo>,
    /// 最近一次刷新时刻（Unix 秒）
    pub timestamp: u64,
}

impl SystemOverview {
    /// 一次都还没采到时为 true（渲染走「暂无数据」分支，而不是画一排 0）
    pub fn is_empty(&self) -> bool {
        self.hostname.is_empty()
            && self.ip.is_empty()
            && self.uptime_secs == 0
            && self.disks.is_empty()
    }

    /// 全部文件系统的容量合计（已用, 总量）
    pub fn disk_totals(&self) -> (u64, u64) {
        self.disks.iter().fold((0u64, 0u64), |(u, t), d| {
            (u.saturating_add(d.used), t.saturating_add(d.total))
        })
    }
}

/// 字节计数差值 到 MB/s（四条速率链路共用的唯一算式）。
///
/// 调用方：远端 `parse_proc_snapshot` 的网络/磁盘、本机采样的网络/磁盘。
/// 抽出来不是为了少写两行 —— 「除以 1024²」这一步一旦在某处漏掉，得到的是一条
/// 数值巨大但**看着仍然像数据**的曲线，只有拿它跟 `df`/`iftop` 对才看得出错。
/// 纯函数 + 单测是唯一挡得住这种错误的东西。
///
/// - `delta` 用 `saturating_sub` 出来的差值（计数回绕/设备热插拔都可能让它为 0）
/// - `dt_secs` 必须 > 0 且有限；同一秒内采两次或时钟回拨时给 0，不产生 `inf`/`NaN`
pub fn bytes_delta_to_mb_per_sec(delta: u64, dt_secs: f32) -> f32 {
    if !dt_secs.is_finite() || dt_secs <= 0.0 {
        return 0.0;
    }
    delta as f32 / dt_secs / (1024.0 * 1024.0)
}

/// MB/s 到 紧凑速率文案（`0 B/s` / `60 B/s` / `1.2 MB/s`）。
/// 低速流量不足 1KB 时保留字节单位，避免阈值以下全被压成`0 B/s`。
pub fn format_rate(mb_per_sec: f32) -> String {
    let bytes = (mb_per_sec.max(0.0) as f64) * 1024.0 * 1024.0;
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    if bytes < KIB {
        format!("{bytes:.0} B/s")
    } else if bytes < MIB {
        format!("{:.1} KB/s", bytes / KIB)
    } else if bytes < GIB {
        format!("{:.1} MB/s", bytes / MIB)
    } else {
        format!("{:.2} GB/s", bytes / GIB)
    }
}

/// 运行时长文案：`0天1时57分49秒` / `0d 1h 57m 49s`（天为 0 时仍保留，位数稳定不跳宽）
pub fn format_uptime(secs: u64) -> String {
    let days = secs / 86_400;
    let hours = (secs % 86_400) / 3_600;
    let mins = (secs % 3_600) / 60;
    let rem = secs % 60;
    crate::i18n::ts(
        format!("{days}天{hours}时{mins}分{rem}秒"),
        format!("{days}d {hours}h {mins}m {rem}s"),
    )
}

/// 定容环形缓冲（RingBuffer<Sample> 300 点）
#[derive(Debug)]
pub struct RingBuffer<T> {
    data: Vec<Option<T>>,
    head: usize,
    len: usize,
    cap: usize,
}

impl<T: Copy> RingBuffer<T> {
    pub fn new(cap: usize) -> Self {
        Self {
            data: (0..cap).map(|_| None).collect(),
            head: 0,
            len: 0,
            cap,
        }
    }

    pub fn push(&mut self, v: T) {
        self.data[self.head] = Some(v);
        self.head = (self.head + 1) % self.cap;
        self.len = (self.len + 1).min(self.cap);
    }

    /// 最新一个样本：**O(1)**，不构造任何中间容器。
    ///
    /// 旧实现是 `iter().last()`，即「先 `collect` 出整条 300 点历史，
    /// 再丢掉 299 个」。本方法每秒被渲染路径调用多次（见 `MetricsStore::latest`）。
    #[inline]
    pub fn last(&self) -> Option<T> {
        if self.len == 0 {
            return None;
        }
        let idx = (self.head + self.cap - 1) % self.cap;
        self.data[idx]
    }

    /// 最近 `n` 个样本（时间正序），写进调用方给的缓冲（清空后复用）。
    ///
    /// 只读 `min(n, len)` 个槽位，而不是先物化整条历史再 `skip`。
    /// 缓冲由调用方持有，稳态下**零分配**。
    pub fn last_n_into(&self, n: usize, out: &mut Vec<T>) {
        out.clear();
        let take = n.min(self.len);
        if take == 0 {
            return;
        }
        let start = (self.head + self.cap - take) % self.cap;
        out.reserve(take);
        for i in 0..take {
            if let Some(v) = self.data[(start + i) % self.cap] {
                out.push(v);
            }
        }
    }

    /// 按时间顺序返回有效样本
    pub fn iter(&self) -> Vec<T> {
        let start = if self.len < self.cap { 0 } else { self.head };
        (0..self.len)
            .filter_map(|i| self.data[(start + i) % self.cap])
            .collect()
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// 共享指标存储：采样线程写入，UI 线程读取
#[derive(Debug)]
pub struct MetricsStore {
    pub cpu: Mutex<RingBuffer<Sample>>,
    /// 系统概况（低频刷新）。采样线程与远端探测任务都写这里，UI 只读。
    ///
    /// 用 `Arc` + 写时替换（copy-on-write），让「每帧取一份」从**深拷贝**
    /// （`Vec<DiskInfo>` + 每个挂载点 6 个 `String`，10 个挂载点约 70 次分配）
    /// 降为一次 `Arc` 引用计数自增。写入侧 15s 才发生一次，替换成本可忽略。
    overview: Mutex<Arc<SystemOverview>>,
}

impl Default for MetricsStore {
    fn default() -> Self {
        Self::new()
    }
}

impl MetricsStore {
    pub fn new() -> Self {
        Self {
            cpu: Mutex::new(RingBuffer::new(300)),
            overview: Mutex::new(Arc::new(SystemOverview::default())),
        }
    }

    pub fn push(&self, s: Sample) {
        self.cpu.lock().push(s);
    }

    /// 最新样本：O(1)，不再为了取尾部而复制整条历史。
    pub fn latest(&self) -> Option<Sample> {
        self.cpu.lock().last()
    }

    /// 最近 `n` 个样本写进调用方的缓冲（稳态零分配）。
    ///
    /// 渲染路径应当用这个而不是 `cpu.lock().iter()`：后者会物化整条历史。
    pub fn recent_into(&self, n: usize, out: &mut Vec<Sample>) {
        self.cpu.lock().last_n_into(n, out);
    }

    pub fn set_overview(&self, o: SystemOverview) {
        *self.overview.lock() = Arc::new(o);
    }

    /// 概况快照（**廉价**：只加一次引用计数，不做深拷贝）。
    ///
    /// 渲染路径应当优先用这个；[`overview`](Self::overview) 保留给需要
    /// 独立所有权的调用点。
    pub fn overview_arc(&self) -> Arc<SystemOverview> {
        self.overview.lock().clone()
    }

    /// 概况的独立副本（旧 API，语义不变；深拷贝只在真的需要所有权时用）。
    pub fn overview(&self) -> SystemOverview {
        // `Arc<T>::clone` 后解引用再克隆：读锁在最内层就释放，
        // 不会把深拷贝的时间算进持锁窗口。
        (*self.overview.lock()).as_ref().clone()
    }
}

/// 采样时间戳 到 本地时间 HH:MM:SS（Chart x 轴标签用）
pub fn time_label(ts: u64) -> String {
    chrono::Local
        .timestamp_opt(ts as i64, 0)
        .single()
        .map(|t| t.format("%H:%M:%S").to_string())
        .unwrap_or_else(|| "--:--:--".into())
}

/// 日期 + 时刻（`09-21 14:03`，本机时区）。
///
/// 给「跨天要能读」的场景用：运行记录与会话标题只写 `HH:MM:SS` 的话，
/// 隔天再看就分不清是今天早上还是昨天早上跑的。
pub fn datetime_label(ts: u64) -> String {
    chrono::Local
        .timestamp_opt(ts as i64, 0)
        .single()
        .map(|t| t.format("%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "-- --:--".into())
}

/// 块状走势字符（▁▂▃▄▅▆▇█），用于状态栏 CPU/MEM 迷你图
///
/// `values.len() < width` 时按等距重采样**重复取值**（而不是越界）：
/// 旧写法 `values[i * len / width]` 在 `len < width` 时会算出下标 `== len`
/// 而直接 panic（例：`values.len() = 1, width = 8` → `i = 1` 时下标为 1）。
pub fn sparkline(values: &[f32], width: usize, max: f32) -> String {
    const BLOCKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    if values.is_empty() || width == 0 {
        return String::new();
    }
    // `max <= 0`（或 NaN）时无法归一化：统一给最低档，避免整串随机高度
    let scale = if max.is_finite() && max > 0.0 {
        max
    } else {
        f32::INFINITY
    };
    // 等距抽样到 width 个桶；下标必须夹在 `0..values.len()`
    let last = values.len() - 1;
    let mut out = String::with_capacity(width * 3);
    for i in 0..width {
        let idx = (i * values.len() / width).min(last);
        let v = values[idx];
        let level = if scale.is_infinite() {
            0
        } else {
            ((v.clamp(0.0, scale) / scale) * 7.0).round() as usize
        };
        out.push(BLOCKS[level.min(7)]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_buffer_wraps() {
        let mut rb: RingBuffer<u32> = RingBuffer::new(3);
        for i in 1..=5 {
            rb.push(i);
        }
        assert_eq!(rb.iter(), vec![3, 4, 5]);
    }

    #[test]
    fn sparkline_levels() {
        assert_eq!(sparkline(&[0.0], 1, 100.0), "▁");
        assert_eq!(sparkline(&[100.0], 1, 100.0), "█");
        assert_eq!(sparkline(&[50.0], 1, 100.0), "▅");
        assert_eq!(sparkline(&[], 3, 100.0), "");
        assert_eq!(sparkline(&[1.0, 2.0], 0, 100.0), "");
    }

    /// ★ 回归护栏：样本数少于宽度时**不得越界 panic**。
    ///
    /// 旧写法 `values[i * len / width]` 在 `len < width` 时算出下标 `== len`：
    /// `sparkline(&[0.0], 8, 100.0)` 会在 `i = 1` 处直接下标越界。
    #[test]
    fn sparkline_handles_fewer_values_than_width() {
        // 1 个样本铺满 8 格
        assert_eq!(sparkline(&[100.0], 8, 100.0), "████████");
        // 2 个样本铺满 5 格：等距重复取值
        assert_eq!(sparkline(&[0.0, 100.0], 5, 100.0), "▁▁▁██");
        // 3 个样本铺满 10 格
        let s = sparkline(&[0.0, 50.0, 100.0], 10, 100.0);
        assert_eq!(s.chars().count(), 10);
    }

    /// `max <= 0` / NaN 时不能算出 NaN 档位（`as usize` 会把 NaN 变 0，
    /// 但那属于「碰巧对」；这里显式约定为最低档）。
    #[test]
    fn sparkline_survives_zero_and_nan_max() {
        assert_eq!(sparkline(&[10.0, 20.0], 4, 0.0), "▁▁▁▁");
        assert_eq!(sparkline(&[10.0, 20.0], 4, f32::NAN), "▁▁▁▁");
        assert_eq!(sparkline(&[10.0, 20.0], 4, -5.0), "▁▁▁▁");
        // 超出上限的样本被夹住，不会算出 >7 的档位
        assert_eq!(sparkline(&[0.0, 1000.0], 2, 100.0), "▁█");
    }

    /// 环形缓冲的 O(1) 尾读 / 最近 n 个：与 `iter()` 逐点比对，且跨环回一致。
    #[test]
    fn ring_buffer_tail_accessors_match_iter() {
        let mut rb: RingBuffer<u32> = RingBuffer::new(4);
        assert_eq!(rb.last(), None, "空缓冲没有最新样本");

        for i in 1..=4u32 {
            rb.push(i);
            assert_eq!(rb.last(), Some(i));
            let full = rb.iter();
            let mut buf = Vec::new();
            rb.last_n_into(10, &mut buf);
            assert_eq!(buf, full, "不足 n 个时应给全部有效样本");
        }

        // 绕环：1..=4 之后推 5、6，环里只剩 [3,4,5,6]
        rb.push(5);
        rb.push(6);
        assert_eq!(rb.last(), Some(6));
        assert_eq!(rb.iter(), vec![3, 4, 5, 6]);

        let mut buf = vec![999]; // 预置脏数据，验证会被清空
        rb.last_n_into(2, &mut buf);
        assert_eq!(buf, vec![5, 6], "应给出时间正序的最后 2 个");
        rb.last_n_into(0, &mut buf);
        assert!(buf.is_empty());
    }

    /// 每帧读尾部不能物化整条历史：`latest()` 与 `recent_into` 的语义
    /// 必须与旧 `iter().last()` / `iter().skip()` 完全一致。
    #[test]
    fn store_tail_reads_match_full_history() {
        let store = MetricsStore::new();
        assert_eq!(store.latest(), None);

        for i in 0..10u64 {
            store.push(Sample {
                cpu_percent: i as f32,
                timestamp: i,
                ..Default::default()
            });
        }
        assert_eq!(store.latest().expect("有样本").timestamp, 9);

        let full = store.cpu.lock().iter();
        for n in [1usize, 3, 300] {
            let mut buf = Vec::new();
            store.recent_into(n, &mut buf);
            let expect: Vec<Sample> = full[full.len().saturating_sub(n)..].to_vec();
            assert_eq!(buf, expect, "recent_into({n}) 必须等价于尾部切片");
        }
    }

    /// 概况快照必须**共享**同一份数据（`Arc`），不是每帧深拷贝。
    #[test]
    fn overview_snapshot_shares_data() {
        let store = MetricsStore::new();
        store.set_overview(SystemOverview {
            hostname: "box".into(),
            ..Default::default()
        });

        let a = store.overview_arc();
        let b = store.overview_arc();
        assert!(Arc::ptr_eq(&a, &b), "同一份概况必须共享，不得深拷贝");
        assert_eq!(a.hostname, "box");

        // 写入新概况后旧快照不变（值语义），新快照指向新数据
        store.set_overview(SystemOverview {
            hostname: "other".into(),
            ..Default::default()
        });
        assert_eq!(a.hostname, "box", "旧快照不应被就地改写");
        assert_eq!(store.overview_arc().hostname, "other");
        assert!(!Arc::ptr_eq(&a, &store.overview_arc()));
    }

    #[test]
    fn bytes_short_formatting() {
        assert_eq!(format_bytes_short(0), "0B");
        assert_eq!(format_bytes_short(512), "512B");
        assert_eq!(format_bytes_short(1024), "1K");
        assert_eq!(format_bytes_short(1536), "1.5K");
        assert_eq!(format_bytes_short(16 * 1024 * 1024 * 1024), "16G");
        assert_eq!(format_bytes_short(8_589_934_592), "8G");
        // 100 MiB = 104857600
        assert_eq!(format_bytes_short(104_857_600), "100M");
    }

    #[test]
    fn mem_summary_handles_missing_total() {
        let mut s = Sample::default();
        assert_eq!(s.mem_summary(), "—", "上限未知时不应显示 0/0");

        s.mem_total = 16 * 1024 * 1024 * 1024;
        s.mem_used = 8 * 1024 * 1024 * 1024;
        s.mem_percent = 50.0;
        assert_eq!(s.mem_used_over_total(), "8G/16G");
        assert_eq!(s.mem_summary(), "8G/16G (50.00%)");
    }

    /// Sample 增字段后仍能反序列化旧 JSON（缺 mem_used/mem_total）
    #[test]
    fn sample_deserializes_without_new_fields() {
        let old = r#"{"cpu_percent":1.0,"mem_percent":2.0,"swap_percent":3.0,
                      "net_up":0.1,"net_down":0.2,"timestamp":123}"#;
        let s: Sample = serde_json::from_str(old).expect("旧样本应可解析");
        assert_eq!(s.mem_used, 0);
        assert_eq!(s.mem_total, 0);
        assert_eq!(s.swap_used, 0);
        assert_eq!(s.swap_total, 0);
        assert_eq!(s.mem_percent, 2.0);
        // 磁盘速率是 2026-09-21 加的：旧的历史/缓存样本里没有这两个键，
        // 解析必须成功且落在 0（图上表现为「那段时间没采到」，而不是错位的数）
        assert_eq!(s.disk_read_mb, 0.0);
        assert_eq!(s.disk_write_mb, 0.0);
    }

    /// 速率换算的唯一算式：`delta / dt / 1024²`，且 `dt` 非法时给 0 而不是 `inf`/`NaN`。
    ///
    /// 挡的是「漏了那一步换算」：漏掉除以 1024² 会得到 1048576 倍的数，
    /// 图上仍是一条曲线，只能靠这条断言拦。
    #[test]
    fn bytes_delta_becomes_mb_per_sec_with_the_1024_factor() {
        const MIB: u64 = 1024 * 1024;
        assert_eq!(bytes_delta_to_mb_per_sec(0, 1.0), 0.0);
        assert_eq!(bytes_delta_to_mb_per_sec(MIB, 1.0), 1.0);
        assert_eq!(bytes_delta_to_mb_per_sec(MIB * 10, 2.0), 5.0);
        // 亚秒间隔（本机高频采样）也要算对，不能被截成整数秒
        assert_eq!(bytes_delta_to_mb_per_sec(MIB, 0.5), 2.0);
        // 反证：漏掉 /1024² 会得到 1048576.0
        assert_ne!(bytes_delta_to_mb_per_sec(MIB, 1.0), MIB as f32);

        // dt 非法 → 0（不得产生 inf / NaN）
        for bad in [0.0f32, -1.0, f32::NAN, f32::INFINITY] {
            let v = bytes_delta_to_mb_per_sec(MIB, bad);
            assert_eq!(v, 0.0, "dt={bad} 应给 0，实际 {v}");
        }
    }

    /// 交换分区文案与内存同口径：未启用交换时给占位符，不做「0B/0B (0.00%)」
    /// 这种看起来像数据但毫无信息量的输出。
    #[test]
    fn swap_summary_matches_memory_wording() {
        let mut s = Sample::default();
        assert_eq!(s.swap_used_over_total(), "—", "未启用交换时不应显示 0B/0B");
        assert_eq!(s.swap_summary(), "—");

        s.swap_total = 2 * 1024 * 1024 * 1024;
        s.swap_used = 512 * 1024 * 1024;
        s.swap_percent = 25.0;
        assert_eq!(s.swap_used_over_total(), "512M/2G");
        assert_eq!(s.swap_summary(), "512M/2G (25.00%)");
    }

    #[test]
    fn rate_formatting_keeps_unit_scale() {
        assert_eq!(format_rate(0.0), "0 B/s");
        // 60 B/s 这类低速流量不能被压成 0（图上「↓ 60 B/s」）
        assert_eq!(format_rate(60.0 / (1024.0 * 1024.0)), "60 B/s");
        assert_eq!(format_rate(1.0), "1.0 MB/s");
        assert_eq!(format_rate(0.5), "512.0 KB/s");
        assert_eq!(format_rate(2048.0), "2.00 GB/s");
        // 负值（时钟回拨/计数回绕）按 0 处理
        assert_eq!(format_rate(-1.0), "0 B/s");
    }

    #[test]
    fn uptime_formatting_is_stable() {
        let _g = crate::i18n::test_support::lang_guard();
        crate::i18n::set_lang(crate::i18n::Lang::Zh);
        assert_eq!(format_uptime(0), "0天0时0分0秒");
        assert_eq!(format_uptime(6_829), "0天1时53分49秒");
        assert_eq!(format_uptime(90_061), "1天1时1分1秒");
        crate::i18n::set_lang(crate::i18n::Lang::En);
        assert_eq!(format_uptime(6_829), "0d 1h 53m 49s");
        crate::i18n::set_lang(crate::i18n::Lang::Zh);
    }

    #[test]
    fn disk_percent_and_summary() {
        let mut d = DiskInfo::default();
        assert_eq!(d.percent(), 0.0, "总量未知时不得除零");
        assert_eq!(d.used_over_total(), "0B/0B");
        assert_eq!(d.avail_text(), None, "总量未知时不得编造可用量");

        d.total = 50 * 1024 * 1024 * 1024;
        d.used = 12 * 1024 * 1024 * 1024;
        assert_eq!(d.used_over_total(), "12G/50G");
        assert!((d.percent() - 24.0).abs() < 0.01);
        // 本机路径（无 df 文本）：可用量按「总量 - 已用」算
        assert_eq!(d.avail_text().as_deref(), Some("38G"));
    }

    /// 带 `df -h` 原样文本时，展示与占用率全部以 df 为准（远端 SSH 路径）
    #[test]
    fn disk_with_df_text_matches_df_h() {
        let d = DiskInfo {
            name: "/dev/vda1".into(),
            mount: "/".into(),
            total: 59 * 1024 * 1024 * 1024,
            used: 4_700_000_000,
            df: Some(DfColumns {
                size: "59G".into(),
                used: "4.7G".into(),
                avail: "51G".into(),
                use_pct: "9%".into(),
            }),
            ..Default::default()
        };
        assert_eq!(d.used_over_total(), "4.7G/59G", "容量用 df 的原样文本");
        assert_eq!(d.avail_text().as_deref(), Some("51G"));
        assert_eq!(d.percent(), 9.0, "占用率用 df 的 Use%");
    }

    /// `Use%` 超出 0..=100（远端输出异常）时钳住，避免弧线越界
    #[test]
    fn df_use_percent_is_clamped() {
        let d = DiskInfo {
            df: Some(DfColumns {
                use_pct: "155%".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(d.percent(), 100.0);
    }

    #[test]
    fn overview_reports_empty_and_totals() {
        let mut o = SystemOverview::default();
        assert!(o.is_empty());
        assert_eq!(o.disk_totals(), (0, 0));

        o.hostname = "web-01".into();
        assert!(!o.is_empty());

        o.disks = vec![
            DiskInfo {
                name: "/dev/sda1".into(),
                mount: "/".into(),
                total: 100,
                used: 40,
                ..Default::default()
            },
            DiskInfo {
                name: "/dev/sdb1".into(),
                mount: "/data".into(),
                total: 200,
                used: 10,
                ..Default::default()
            },
        ];
        assert_eq!(o.disk_totals(), (50, 300));
    }

    /// 概况经共享存储往返：写入即可被 UI 读到（跨线程的唯一契约）
    #[test]
    fn store_round_trips_overview() {
        let store = MetricsStore::new();
        assert!(store.overview().is_empty());
        store.set_overview(SystemOverview {
            hostname: "box".into(),
            ip: "10.0.0.2".into(),
            uptime_secs: 7,
            ..Default::default()
        });
        let got = store.overview();
        assert_eq!(got.hostname, "box");
        assert_eq!(got.ip, "10.0.0.2");
        assert_eq!(got.uptime_secs, 7);
    }
}
