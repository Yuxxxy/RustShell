//! 概览面板：运行状态（主机名 / 系统 IP / 运行时长）
//! + 资源走势（CPU / 内存 / 交换 / 网络 / 磁盘读写）+ 存储用量（df 信息）。
//!
//! 数据取「当前会话」的指标存储（[`MainView::metrics_for_active`]），
//! 故本地 Shell 与 SSH 远端会话共用同一段渲染代码：远端概况由探测命令
//! （[`rustshell_core::ssh::PROC_PROBE_CMD`]）随走势一起回传，字段与本地一致。
//!
//! 2026-09-17 运行反馈：移除「会话」列表与「本地 Shell / 新建连接」快速动作 ——
//! 会话切换已由标签栏承担（同一份数据摆两处），新建入口在连接面板与终端面板各有一个。
//!
//! 2026-09-18 运行反馈：移除「运行状态」卡里的 CPU / 内存 / 交换三个指标环 ——
//! 与下方走势卡同源同值，环只重复一遍当前值。

use gpui::{
    div, px, Context, InteractiveElement as _, IntoElement, ParentElement as _, SharedString,
    Styled as _,
};
use gpui_component::{h_flex, scroll::ScrollableElement as _, v_flex};
use rustshell_core::i18n::t as tr;
use rustshell_core::metrics::{
    format_bytes_short, format_uptime, DiskInfo, MetricsStore, SystemOverview,
};
use rustshell_core::theme::ThemeTokens;

use super::main_view::MainView;
use crate::metrics_card::{self, MetricsLayout};
use crate::theme::to_hsla as hex;

/// 概览卡片外壳：卡片底色取 `bg_base`（在侧栏 `bg_elevated` 上形成一层）+ 细边框圆角
fn card(t: &ThemeTokens) -> gpui::Div {
    v_flex()
        .w_full()
        .gap_2()
        .rounded(px(10.))
        .p_3()
        .bg(hex(t.bg_base))
        .border_1()
        .border_color(hex(t.border_subtle))
}

/// 卡内分区标题
fn title(text: &'static str, t: &ThemeTokens) -> impl IntoElement {
    div()
        .text_size(px(11.))
        .text_color(hex(t.fg_muted))
        .child(text)
}

/// 左标签 + 右数值一行（系统 IP / 已运行）
fn info_row(label: &'static str, value: String, t: &ThemeTokens) -> impl IntoElement {
    h_flex()
        .items_center()
        .justify_between()
        .gap_2()
        .child(
            div()
                .flex_none()
                .text_size(px(11.))
                .text_color(hex(t.fg_muted))
                .child(label),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_size(px(11.))
                .text_color(hex(t.fg_primary))
                .text_ellipsis()
                .text_right()
                .child(value),
        )
}

/// 存储明细一行，口径对齐 `df -h`：第一行「挂载点 + Use%」，第二行
/// `Used / Size` 与 `Avail`。
///
/// 为什么拆两行：窄侧栏（最小 180px）里塞不下「挂载点 + 已用/总量 + 可用 + 占用率」，
/// 硬挤的结果是挂载点被省略号截断、四个数连成一片。拆开后第一行回答「哪个分区、
/// 用了多少」，第二行才是 `df -h` 的容量三列 —— 数字与用户手工执行 `df -h` 一致
/// （远端用 df 的原样文本，本机按字节格式化）。
fn disk_row(disk: &DiskInfo, t: &ThemeTokens) -> impl IntoElement {
    let pct = disk.percent().clamp(0.0, 100.0);
    let detail = match disk.avail_text() {
        Some(avail) => format!("{}  ·  {} {}", disk.used_over_total(), tr("可用"), avail),
        None => disk.used_over_total(),
    };
    v_flex()
        .gap(px(1.))
        .child(
            h_flex()
                .items_center()
                .justify_between()
                .gap_2()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(px(11.))
                        .text_color(hex(t.fg_primary))
                        .text_ellipsis()
                        .child(SharedString::from(disk.mount.clone())),
                )
                .child(
                    div()
                        .flex_none()
                        .text_size(px(10.))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(level_color(pct, t))
                        .child(SharedString::from(format!("{pct:.2}%"))),
                ),
        )
        .child(
            div()
                .text_size(px(10.))
                .text_color(hex(t.fg_muted))
                .text_ellipsis()
                .child(SharedString::from(detail)),
        )
}

/// 占用率 -> 语义色（>85 危险 / >60 警告 / 其余正常）
fn level_color(pct: f32, t: &ThemeTokens) -> gpui::Hsla {
    if pct > 85.0 {
        hex(t.danger)
    } else if pct > 60.0 {
        hex(t.warning)
    } else {
        hex(t.success)
    }
}

/// 柱状图轴标签用的短挂载点：过长时保留尾部（`…/docker`），
/// 否则窄侧栏里几根柱子的标签会互相叠在一起。
///
/// 存储柱状图已按运行反馈移除（2026-09-17），此函数保留是因为它已被单测
/// `short_mount_keeps_the_tail` 固定为「窄侧栏 + 多文件系统」的展示规则；
/// 一旦再接回图形化呈现，标签仍需按同一口径截断。
#[allow(dead_code)]
fn short_mount(mount: &str) -> String {
    const MAX: usize = 10;
    let chars: Vec<char> = mount.chars().collect();
    if chars.len() <= MAX {
        return mount.to_string();
    }
    let tail: Vec<char> = chars[chars.len() - (MAX - 1)..].to_vec();
    // 尽量从路径分隔符起头：`…ib/docker` 看着像错字，`…/docker` 才读得懂
    let start = tail
        .iter()
        .position(|c| *c == '/' || *c == '\\')
        .unwrap_or(0);
    format!("…{}", tail[start..].iter().collect::<String>())
}

/// 多盘合计文案：`120G/320G (38%)`
fn disk_totals_label(o: &SystemOverview) -> String {
    let (used, total) = o.disk_totals();
    if total == 0 {
        return "—".to_string();
    }
    let pct = 100.0 * used as f64 / total as f64;
    format!(
        "{}/{} ({pct:.2}%)",
        format_bytes_short(used),
        format_bytes_short(total)
    )
}

/// 概览侧栏：运行状态 / 资源走势 / 存储三张卡。
///
/// 2026-09-18 运行反馈「取消三个圆饼图」：原先「运行状态」卡里有一排
/// CPU / 内存 / 交换的 `ProgressCircle` 指标环，与下方「资源走势」卡的
/// 三张折线图是**同一份数据的两种画法**（环只给当前值、走势给趋势，
/// 而当前值本来就在走势卡右上角写着）。侧栏宽度只有 180~320px，
/// 删掉环之后这一卡只剩主机名 / IP / 运行时长，信息密度反而更高。
pub fn render(m: &mut MainView, cx: &mut Context<MainView>) -> impl IntoElement {
    let t = crate::theme::current(cx);
    // 当前会话的指标（本地 Shell 用本机采样，SSH 用远端轮询），与状态栏悬停卡同源
    let store: std::sync::Arc<MetricsStore> = m.metrics_for_active().clone();
    // `overview_arc`：只加一次引用计数。旧写法每次重绘都深拷贝整个
    // `SystemOverview`（`Vec<DiskInfo>` + 每个挂载点 6 个 `String`），
    // 而这份数据 15 秒才刷新一次。
    let overview = store.overview_arc();

    let muted = hex(t.fg_muted);
    let (accent, success, warning) = (hex(t.accent), hex(t.success), hex(t.warning));

    let mut root = v_flex()
        .id("dashboard")
        .w_full()
        .h_full()
        .overflow_y_scrollbar()
        .px_2()
        .py_3()
        .gap_3();

    // ---- 运行状态 ----
    let hostname = if overview.hostname.is_empty() {
        "—".to_string()
    } else {
        overview.hostname.clone()
    };
    let ip = if overview.ip.is_empty() {
        "—".to_string()
    } else {
        overview.ip.clone()
    };
    let uptime = if overview.is_empty() {
        "—".to_string()
    } else {
        format_uptime(overview.uptime_secs)
    };
    // 首行 = **服务器名称**（当前会话的连接名 / 本地 Shell 的会话名）。
    //
    // 2026-09-18 用户要求「概览第一行应该是服务器名称？反正不是状态」：这一卡里的
    // 主机名 / IP / 运行时长全是**某一台机器**的读数，第一眼要能确认「这是哪台」，
    // 而不是先读到「运行状态」这种不含信息的标签。
    let server_name = m
        .active_tab()
        .map(|tab| tab.session.title.clone())
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| "—".to_string());

    let mut status_card = card(&t);
    status_card = status_card.child(
        div()
            .w_full()
            .min_w_0()
            .text_size(px(12.))
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .text_color(hex(t.fg_primary))
            .text_ellipsis()
            .child(SharedString::from(server_name)),
    );
    status_card = status_card.child(div().h(px(1.)).w_full().bg(hex(t.border_subtle)));
    // 远端主机名单独一行：连接名是**用户起的名字**，主机名是机器自报的 ——
    // 两者不一致时（改过名、跳板机）正是需要对照着看的地方。
    status_card = status_card.child(info_row(tr("主机名"), hostname, &t));
    status_card = status_card.child(info_row(tr("系统 IP"), ip, &t));
    status_card = status_card.child(info_row(tr("已运行"), uptime, &t));
    root = root.child(status_card);

    // ---- 资源走势（CPU / 内存 / 交换 / 网络 / 磁盘，网络图与磁盘图都在此）----
    //
    // 2026-09-17 运行反馈「概览，删除单独的网络，替换为资源走势」：原先「网络」是一张
    // 独立卡片（上下行数字 + 走势图），而下方「资源走势」里又画了一遍网络图 ——
    // 同一份数据在侧栏出现两次。现在只保留走势卡里的那一张，并把它提到原网络卡的位置
    // （紧跟「运行状态」，一眼能看到）。
    root = root.child(
        card(&t)
            .child(title(tr("资源走势"), &t))
            .child(metrics_card::metrics_grid(
                store.clone(),
                accent,
                success,
                warning,
                muted,
                MetricsLayout::sidebar(),
                "dash-chart",
            )),
    );

    // ---- 存储（df 信息：逐个文件系统给用量 + 占用率）----
    //
    // 2026-09-17 运行反馈「存储移除柱状图」：BarChart 在窄侧栏里只能放下三两根柱子，
    // 标签还会互相挤压，信息量反而不如下面的明细行。明细行（挂载点 + 已用/总量 +
    // 占用率）保留，并作为唯一的存储呈现。
    let mut disk_card = card(&t);
    disk_card = disk_card.child(
        h_flex()
            .items_center()
            .justify_between()
            .gap_2()
            .child(title(tr("存储"), &t))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(10.))
                    .text_color(muted)
                    .text_ellipsis()
                    .text_right()
                    .child(SharedString::from(disk_totals_label(&overview))),
            ),
    );
    if overview.disks.is_empty() {
        disk_card = disk_card.child(
            div()
                .text_size(px(11.))
                .text_color(muted)
                .child(tr("暂无磁盘信息")),
        );
    } else {
        // 明细行给精确数字（柱状图已按要求移除）
        for disk in &overview.disks {
            disk_card = disk_card.child(disk_row(disk, &t));
        }
    }
    root = root.child(disk_card);

    // 2026-09-17 运行反馈：原先这里还有「会话」列表卡与「本地 Shell / 新建连接」
    // 两个快速动作按钮。会话列表与标签栏是同一份数据的两个视图（概览侧栏窄，
    // 标题常被省略号截断，切换会话反而不好认）；两个新建入口又分别在终端面板
    // 与连接面板各有一个。两块一并移除，概览只剩「看指标」这一件事。
    root
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::prod;

    /// 柱状图标签：短挂载点原样，长挂载点保留尾部并按路径分隔符起头
    /// （窄侧栏里几根柱子的标签太长就会互相叠在一起）
    #[test]
    fn short_mount_keeps_the_tail() {
        assert_eq!(short_mount("/"), "/");
        assert_eq!(short_mount("/data"), "/data");
        assert_eq!(short_mount("/var/lib/docker"), "…/docker");
        assert_eq!(short_mount("/data/backup/yearly"), "…/yearly");
        // 按字符切：多字节路径不得从中间劈开（劈开就是乱码）
        assert_eq!(short_mount("/数据目录/备份资料/年度归档"), "…/年度归档");
    }

    /// 合计文案：没有磁盘数据时给占位符，不显示 `0B/0B (0%)`
    #[test]
    fn disk_totals_label_handles_missing_data() {
        assert_eq!(disk_totals_label(&SystemOverview::default()), "—");
        let o = SystemOverview {
            disks: vec![DiskInfo {
                total: 1000,
                used: 250,
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(disk_totals_label(&o), "250B/1000B (25.00%)");
    }

    /// 回归（2026-09-18 运行反馈「概览，取消三个圆饼图」）：
    ///
    /// 三个指标环与下方走势卡读的是同一份 `Sample`，环只复述一遍当前值
    /// （而当前值本来就写在走势卡右上角）。整体移除后不得在概览里复活：
    /// 断言用剥过注释的生产正文，注释里提到组件名不算数。
    #[test]
    fn dashboard_has_no_progress_rings() {
        let code = production_source();
        for needle in ["ProgressCircle", "RingKind", "dash-ring-", "fn ring("] {
            assert!(
                !code.contains(needle),
                "概览仍残留指标环（`{needle}`）：当前值由走势卡右上角承担。"
            );
        }
        // 反向信号：环删了，但概况卡的其余信息必须还在
        for keep in ["tr(\"主机名\")", "tr(\"系统 IP\")", "tr(\"已运行\")"] {
            assert!(code.contains(keep), "概况卡少了内容：{keep}");
        }
        // 2026-09-18 用户要求「概览第一行应该是服务器名称？反正不是状态」：
        // 首行是当前会话名（`session.title`），不再是「运行状态」这类标签。
        assert!(
            code.contains("tab.session.title.clone()"),
            "概况卡首行必须取当前会话名（服务器名称），而不是状态标签。"
        );
        assert!(
            !code.contains("tr(\"运行状态\")"),
            "「运行状态」标签已按用户要求从首行撤下（词条表也已同步删除）。"
        );
    }

    /// 生产代码正文（剥注释 + 切测试模块，见 `crate::test_support`）：
    /// 本组断言针对「侧栏摆了什么」，注释里提到旧卡片名不算数。
    fn production_source() -> String {
        prod(include_str!("dashboard_panel.rs"))
    }

    /// 回归（2026-09-17 运行反馈「概览，删除单独的网络，替换为资源走势」）：
    ///
    /// 1) 不再有独立的「网络」卡片（`net_card` / `net_series` / `format_rate`）；
    /// 2) 资源走势卡排在「存储」之前 —— 它顶替的正是原来网络卡的位置；
    /// 3) 网络数据仍由资源走势卡里的那张图承载，不是被整体删掉。
    #[test]
    fn dashboard_has_no_standalone_network_card() {
        let code = production_source();
        for needle in [
            "net_card",
            "net_series",
            "format_rate",
            "dash-net-card-chart",
        ] {
            assert!(
                !code.contains(needle),
                "概览面板仍残留独立网络卡（`{needle}`）：网络只该出现在资源走势卡里。"
            );
        }
        assert!(
            code.contains("metrics_grid"),
            "资源走势必须仍在（它同时承载 CPU / 内存 / 交换 / 网络 / 磁盘五张图）。"
        );
        // 顺序：资源走势卡要在存储卡之前
        let trend = code
            .find("tr(\"资源走势\")")
            .expect("找不到资源走势卡（结构锚点）");
        let disk = code.find("tr(\"存储\")").expect("找不到存储卡（结构锚点）");
        assert!(
            trend < disk,
            "资源走势卡应排在存储卡之前（它顶替了被删掉的网络卡的位置）。"
        );
    }

    /// 回归（2026-09-17 运行反馈「存储移除柱状图」）：
    /// 存储只保留明细行，`BarChart` 与它的挂载点标签处理都不该再出现在渲染路径里。
    #[test]
    fn dashboard_storage_has_no_bar_chart() {
        let code = production_source();
        for needle in ["BarChart", "disk_chart", "dash-disk-chart"] {
            assert!(
                !code.contains(needle),
                "存储卡仍残留柱状图（`{needle}`）；存储只保留明细行。"
            );
        }
        assert!(
            code.contains("disk_row"),
            "存储明细行必须还在 —— 去掉柱状图不等于去掉存储信息。"
        );
    }

    /// 回归（2026-09-17 运行反馈「概览去除本地Shell和新建连接，移除会话模块」）：
    ///
    /// 会话列表与标签栏是同一份数据的两个视图，快速动作按钮也与终端面板 /
    /// 连接面板的入口重复。这三样都不得在概览里复活 —— 概览只负责「看指标」。
    /// （注释已剥：本组断言针对代码，注释里提到旧卡片名不算数。）
    #[test]
    fn dashboard_has_no_session_list_and_no_quick_actions() {
        let code = production_source();
        for needle in [
            "sess_card",
            "dash-tab-",
            "dash-local",
            "dash-connect",
            "activate_session",
        ] {
            assert!(
                !code.contains(needle),
                "概览仍残留已移除的「会话 / 快速动作」（`{needle}`）：\n\
                 会话切换由标签栏承担，新建入口在终端面板与连接面板。"
            );
        }
        // 反向信号：指标三卡必须仍在，删掉的是入口不是内容
        // （首行改为服务器名后，概况卡的锚点从「运行状态」换成它的三行信息）
        for keep in [
            "tr(\"主机名\")",
            "tr(\"系统 IP\")",
            "tr(\"已运行\")",
            "tr(\"资源走势\")",
            "tr(\"存储\")",
        ] {
            assert!(code.contains(keep), "概览少了核心卡片：{keep}");
        }
    }
}
