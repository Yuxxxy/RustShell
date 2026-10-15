//! 指标走势卡（概览侧栏用）。
//!
//! 单列窄幅布局（[`MetricsLayout::sidebar`]）。
//! （状态栏及其悬停总览卡已于 2026-09-17 按运行反馈移除。）
//! 图表 ElementId 由调用方给前缀拼出，多处同存时不会撞 id。
//!
//! 2026-09-18 运行反馈：走势图的 hover 数值改走 [`crate::plot_chart`] 的
//! 自绘图表 —— 官方 `AreaChart` 的提示行是 `format!("{}")` 硬编码，
//! 百分比图会显示成没有单位的 `12.4`、网络图会显示一长串 f64 尾数。

use gpui::{
    div, prelude::FluentBuilder as _, px, AnyElement, IntoElement, ParentElement as _,
    SharedString, Styled as _,
};
use gpui_component::{h_flex, v_flex};
use rustshell_core::i18n::{t as tr, ts};
use rustshell_core::metrics::{time_label, MetricsStore, Sample};
use std::sync::Arc;

use crate::plot_chart::{rate_format, Series, ValueAreaChart, FILL_ALPHA};

/// 双序列图（网络的上行/下行、磁盘的读/写）的填充不透明度：比单序列百分比图更淡，
/// 两条曲线下的面积叠在一起时不会糊成一片。
const DUAL_FILL_ALPHA: f32 = 0.10;

/// x 轴刻度间隔（约 1 分钟窗口 60 点 → 4 个刻度）
const TICK_MARGIN: usize = 15;

/// Chart x 轴取点数量（约 1 分钟窗口，采样 1s/失焦降频）
pub const CHART_POINTS: usize = 60;

/// 走势卡布局参数
#[derive(Clone, Copy)]
pub struct MetricsLayout {
    /// 每行列数
    pub columns: usize,
    /// 单图宽度（None = 撑满容器，窄侧栏用）
    pub plot_width: Option<f32>,
    pub plot_height: f32,
    /// 整卡宽度（None = 撑满容器）
    pub card_width: Option<f32>,
    /// 图表间距
    pub gap: f32,
}

impl MetricsLayout {
    /// 概览侧栏：单列，宽度跟着侧栏走
    pub fn sidebar() -> Self {
        Self {
            columns: 1,
            plot_width: None,
            plot_height: 64.0,
            card_width: None,
            gap: 10.0,
        }
    }
}

/// 最近 n 个样本（时间正序）
///
/// 走 `recent_into`（只读尾部 n 个槽位）而不是 `cpu.lock().iter()`：
/// 后者会**物化整条 300 点历史**再丢掉大部分，而本函数在渲染路径上
/// 每次重绘都会被调用（概览侧栏 + 状态栏悬停卡）。
pub fn recent(store: &MetricsStore, n: usize) -> Vec<Sample> {
    let mut out = Vec::with_capacity(n.clamp(1, CHART_POINTS));
    store.recent_into(n, &mut out);
    out
}

/// 「近 N 秒」窗口文案
pub fn window_label(len: usize) -> String {
    ts(format!("近 {len} 秒"), format!("last {len}s"))
}

/// 图表外框：固定宽（悬停卡）或撑满（侧栏）
fn plot_box(layout: MetricsLayout) -> gpui::Div {
    div()
        .h(px(layout.plot_height))
        .when_some(layout.plot_width, |d, w| d.w(px(w)))
        .when(layout.plot_width.is_none(), |d| d.w_full())
}

/// 单序列百分比走势图（无标题行）。`name` 是悬停提示里的序列名，
/// hover 数值按百分比两位小数呈现（`12.40%`）。
pub fn percent_series(
    id: String,
    hist: Vec<Sample>,
    color: gpui::Hsla,
    name: &'static str,
    sel: fn(&Sample) -> f64,
    layout: MetricsLayout,
) -> impl IntoElement {
    plot_box(layout).child(ValueAreaChart::new(
        id,
        hist,
        |s: &Sample| SharedString::from(time_label(s.timestamp)),
        vec![Series::new(
            sel,
            color,
            color.opacity(FILL_ALPHA),
            name,
            crate::plot_chart::percent_format,
        )],
        TICK_MARGIN,
    ))
}

/// 双序列速率图里的一条曲线（网络的上行 / 磁盘的读 …）
struct RateLine {
    color: gpui::Hsla,
    /// hover 提示行里的序列名（`上行` / `R`）
    name: &'static str,
    /// 表头当前值的前缀（`↑` / `R `）。
    /// 图上没有常驻图例，两条线谁是谁只能看这里 —— 前缀必须和 `name` 同义。
    prefix: &'static str,
    sel: fn(&Sample) -> f32,
}

/// 网络图的两条线：上行（发）/ 下行（收）
fn net_lines(up: gpui::Hsla, down: gpui::Hsla) -> [RateLine; 2] {
    [
        RateLine {
            color: up,
            name: tr("上行"),
            prefix: "↑",
            sel: |s: &Sample| s.net_up,
        },
        RateLine {
            color: down,
            name: tr("下行"),
            prefix: "↓",
            sel: |s: &Sample| s.net_down,
        },
    ]
}

/// 磁盘图的两条线：R（读）/ W（写）。
///
/// 标签是英文单字母、不走 `tr()`（用户 2026-09-29 要求）：磁盘 IO 的通用记号，
/// 两种语言下同一个样子；单字母也不会把表头的「磁盘 (MB/s) · 近 60 秒」挤掉。
///
/// 用同一张图的两个序列，而不是两张单序列图：读写常差一个数量级（写日志、
/// 读大文件），拆成两张之后纵轴各自缩放，「读 200 写 2」和「读 2 写 200」
/// 会画得一模一样。
fn disk_lines(read: gpui::Hsla, write: gpui::Hsla) -> [RateLine; 2] {
    [
        RateLine {
            color: read,
            name: "R",
            prefix: "R ",
            sel: |s: &Sample| s.disk_read_mb,
        },
        RateLine {
            color: write,
            name: "W",
            prefix: "W ",
            sel: |s: &Sample| s.disk_write_mb,
        },
    ]
}

/// 双序列速率走势图（MB/s，无标题行）。
///
/// hover 数值保留两位小数 —— 速率本身是 f32 除法算出来的，
/// 原样打印会给出 `0.123456789` 这种把提示框撑变形的尾数。
fn dual_series(
    id: String,
    hist: Vec<Sample>,
    lines: [RateLine; 2],
    layout: MetricsLayout,
) -> impl IntoElement {
    let series = lines
        .into_iter()
        .map(|l| {
            // 拆开再进闭包：`sel` 是 `fn` 指针（Copy），整体 move 进来会让
            // 闭包多持有一个 `RateLine`，而这里只需要这两三个字段
            let RateLine {
                color, name, sel, ..
            } = l;
            Series::new(
                move |s: &Sample| sel(s) as f64,
                color,
                color.opacity(DUAL_FILL_ALPHA),
                name,
                rate_format,
            )
        })
        .collect();
    plot_box(layout).child(ValueAreaChart::new(
        id,
        hist,
        |s: &Sample| SharedString::from(time_label(s.timestamp)),
        series,
        TICK_MARGIN,
    ))
}

/// 双序列速率走势卡（表头 + 图表）：网络与磁盘共用。
/// 表头右侧给两个方向的当前值（`↑3.20` / `R 3.20`）。
fn rate_chart(
    id: String,
    hist: Vec<Sample>,
    title: &'static str,
    lines: [RateLine; 2],
    muted: gpui::Hsla,
    layout: MetricsLayout,
) -> impl IntoElement {
    let latest = hist.last().copied();
    let window = window_label(hist.len());
    let empty = hist.is_empty();
    let mut values = h_flex().gap_2();
    for line in &lines {
        let v = latest.map_or(0.0, |s| (line.sel)(&s));
        values = values.child(
            div()
                .text_color(line.color)
                .child(SharedString::from(format!("{}{v:.2}", line.prefix))),
        );
    }
    series_box(layout)
        .child(series_header(format!("{title} (MB/s)"), window, muted).child(values))
        .child(if empty {
            placeholder(muted, layout).into_any_element()
        } else {
            dual_series(id, hist, lines, layout).into_any_element()
        })
}

/// 无采样数据时的占位（高度与图表一致，避免卡片跳动）
pub fn placeholder(muted: gpui::Hsla, layout: MetricsLayout) -> impl IntoElement {
    plot_box(layout)
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(11.))
        .text_color(muted)
        .child(tr("暂无采样数据"))
}

/// 走势卡外壳：固定宽度（悬停卡）或撑满（侧栏）
fn series_box(layout: MetricsLayout) -> gpui::Div {
    v_flex()
        .min_w_0()
        .when_some(layout.plot_width, |d, w| d.w(px(w)).flex_none())
        .when(layout.plot_width.is_none(), |d| d.w_full())
}

/// 走势卡表头：`标题 · 近 N 秒` + 右侧当前值（由调用方继续 `.child()`）
fn series_header(title: String, window: String, muted: gpui::Hsla) -> gpui::Div {
    h_flex()
        .items_center()
        .justify_between()
        .text_size(px(11.))
        .text_color(muted)
        .child(SharedString::from(format!("{title} · {window}")))
}

/// 单序列走势卡的数据与配色（把「图表身份 + 取值 + 当前值文案」收成一份）
struct SeriesSpec {
    id: String,
    title: &'static str,
    color: gpui::Hsla,
    sel: fn(&Sample) -> f64,
    latest: String,
}

/// 单序列百分比走势卡（CPU/MEM/SWP 共用；含表头）
fn percent_chart(
    spec: SeriesSpec,
    hist: Vec<Sample>,
    muted: gpui::Hsla,
    layout: MetricsLayout,
) -> impl IntoElement {
    let window = window_label(hist.len());
    let empty = hist.is_empty();
    series_box(layout)
        .child(
            series_header(spec.title.to_string(), window, muted).child(
                div()
                    .text_color(spec.color)
                    .child(SharedString::from(spec.latest)),
            ),
        )
        .child(if empty {
            placeholder(muted, layout).into_any_element()
        } else {
            percent_series(spec.id, hist, spec.color, spec.title, spec.sel, layout)
                .into_any_element()
        })
}

/// 双序列网络走势卡（含表头与 ↑/↓ 当前值）
fn net_chart(
    id: String,
    hist: Vec<Sample>,
    up: gpui::Hsla,
    down: gpui::Hsla,
    muted: gpui::Hsla,
    layout: MetricsLayout,
) -> impl IntoElement {
    rate_chart(id, hist, tr("网络"), net_lines(up, down), muted, layout)
}

/// 双序列磁盘走势卡（含表头与 R/W 当前值）。
///
/// 取色刻意与网络卡错开（网络是 accent/success，磁盘是 accent/warning）：
/// 两张卡在侧栏里上下相邻，同色的话「这一条是网卡还是硬盘」要靠读标题才知道。
fn disk_chart(
    id: String,
    hist: Vec<Sample>,
    read: gpui::Hsla,
    write: gpui::Hsla,
    muted: gpui::Hsla,
    layout: MetricsLayout,
) -> impl IntoElement {
    rate_chart(id, hist, tr("磁盘"), disk_lines(read, write), muted, layout)
}

/// 五张走势卡：CPU 使用率 / 内存 / 交换分区 / 网络 / 磁盘。
/// 按 `layout.columns` 折行，故侧栏是单列五段。
pub fn metrics_grid(
    store: Arc<MetricsStore>,
    accent: gpui::Hsla,
    success: gpui::Hsla,
    warning: gpui::Hsla,
    muted: gpui::Hsla,
    layout: MetricsLayout,
    id_prefix: &'static str,
) -> impl IntoElement {
    let hist = recent(&store, CHART_POINTS);
    // 右上角数值文案：CPU 用百分比；内存与交换分区用「已用 / 上限（百分比）」——
    // 只给百分比时看不出「3.60% 的 4G」和「3.60% 的 64G」差多少（2026-09-18 运行反馈）。
    let pct = |f: fn(&Sample) -> f32| {
        hist.last()
            .map(|s| format!("{:.2}%", f(s)))
            .unwrap_or_else(|| "—".to_string())
    };
    let mem_label = hist
        .last()
        .map(Sample::mem_summary)
        .unwrap_or_else(|| "—".to_string());
    let swap_label = hist
        .last()
        .map(Sample::swap_summary)
        .unwrap_or_else(|| "—".to_string());

    let items: Vec<AnyElement> = vec![
        percent_chart(
            SeriesSpec {
                id: format!("{id_prefix}-cpu"),
                title: tr("CPU 使用率"),
                color: accent,
                sel: |s| s.cpu_percent as f64,
                latest: pct(|s| s.cpu_percent),
            },
            hist.clone(),
            muted,
            layout,
        )
        .into_any_element(),
        percent_chart(
            SeriesSpec {
                id: format!("{id_prefix}-mem"),
                title: tr("内存"),
                color: success,
                sel: |s| s.mem_percent as f64,
                latest: mem_label,
            },
            hist.clone(),
            muted,
            layout,
        )
        .into_any_element(),
        percent_chart(
            SeriesSpec {
                id: format!("{id_prefix}-swp"),
                title: tr("交换分区"),
                color: warning,
                sel: |s| s.swap_percent as f64,
                latest: swap_label,
            },
            hist.clone(),
            muted,
            layout,
        )
        .into_any_element(),
        net_chart(
            format!("{id_prefix}-net"),
            hist.clone(),
            accent,
            success,
            muted,
            layout,
        )
        .into_any_element(),
        // 磁盘读写在网络之后：两张速率图相邻，读者不用在 CPU/内存/交换三张
        // 百分比图之间来回跳。（2026-09-21 用户要求「概览增加 IO 波动图」。）
        disk_chart(
            format!("{id_prefix}-disk"),
            hist,
            accent,
            warning,
            muted,
            layout,
        )
        .into_any_element(),
    ];

    // 折行：按列数切成等长的行容器
    let mut rows: Vec<Vec<AnyElement>> = Vec::new();
    for el in items {
        if rows.last().is_none_or(|r| r.len() >= layout.columns) {
            rows.push(Vec::new());
        }
        rows.last_mut().expect("刚推入的行必然存在").push(el);
    }

    let mut grid = v_flex().gap(px(layout.gap));
    grid = match layout.card_width {
        Some(w) => grid.w(px(w)),
        None => grid.w_full().min_w_0(),
    };
    for row in rows {
        grid = grid.child(h_flex().items_start().gap(px(layout.gap)).children(row));
    }
    grid
}
