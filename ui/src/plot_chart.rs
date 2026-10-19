//! 带「数值文案格式」的走势图。
//!
//! 为什么不直接用官方的 [`gpui_component::chart::AreaChart`]：
//! 它的 hover 行是 `format!("{}", value)` **硬编码**（`chart/area_chart.rs`
//! 的 `Chart::tooltip`），既不接受格式化闭包也没有导出可覆盖的钩子。于是：
//!
//! - CPU / 内存 / 交换这三张百分比图，hover 显示的是 `12.4`（没单位）；
//! - 网络图显示的是 `0.123456789`（f64 全精度，长到把提示框撑变形）。
//!
//! 修复路径有两条：把 gpui-component 整个 vendored 进仓库打补丁（几十个文件、
//! 每次升级都要重新同步），或者用官方**公开**的 [`Plot`] trait 自己拼一个
//! 同样外观、但每条序列自带 `format` 的图。这里选后者 —— `plot` 模块
//! （`shape::Area` / `Grid` / `PlotAxis` / `tooltip::*` / `scale::*`）都是 `pub`，
//! 绘制原语是现成的，本文件只负责把它们按官方 `AreaChart` 的顺序串起来，
//! 并在 tooltip 里换成调用方给的格式化闭包。
//!
//! `#[derive(IntoPlot)]` 由 gpui-component 公开导出（`plot::IntoPlot`），
//! 它生成 `IntoElement` / `Element` 样板：布局、命中盒、tooltip 悬停与
//! 跨帧游标状态。本文件只需实现 [`Plot`] 的四个方法。

use std::rc::Rc;

use gpui::{
    point, px, AnyElement, App, Bounds, ElementId, Hsla, IntoElement, Pixels, SharedString,
    TextAlign, Window,
};
use gpui_component::plot::{
    scale::{Scale, ScaleLinear, ScalePoint},
    shape::Area,
    tooltip::{CrossLine, Dot, Tooltip, TooltipState},
    AxisText, Grid, IntoPlot, Plot, PlotAxis,
};
use gpui_component::ActiveTheme as _;

/// x 轴刻度区占掉的底部高度（px）。
///
/// 0.7.0 起 `plot::AXIS_GAP`（常量 18.）被**弃用**，官方口径改为
/// `plot::axis_gutter(刻度字号)` = `字号 + TEXT_GAP × 4` —— 留白由标签字号算出来，
/// 图表若画更大的刻度就传自己的字号，否则刻度会被裁掉。
/// 本组件的刻度走默认字号（[`gpui_component::plot::label::TEXT_SIZE`]，10px：
/// 10 + 2×4 = 18），与旧常量取值完全相同，所以这次替换不改变任何一个像素。
fn axis_gutter() -> f32 {
    gpui_component::plot::axis_gutter(px(gpui_component::plot::label::TEXT_SIZE))
}

/// 一条序列：怎么取值、怎么配色、hover 里叫什么、**数值怎么写成文案**。
pub struct Series<T> {
    /// 纵轴取值（图上的高度按它算）
    pub value: Rc<dyn Fn(&T) -> f64>,
    /// 描边色（也是 hover 行的色块）
    pub stroke: Hsla,
    /// 区域填充色
    pub fill: Hsla,
    /// hover 行里的序列名（上行 / 下行 / CPU 使用率 …）
    pub name: SharedString,
    /// hover 行里的数值文案。**本组件存在的唯一理由**：
    /// 百分比图给 `|v| format!("{v:.2}%")`，速率图给 `|v| format!("{v:.2}")`。
    pub format: Rc<dyn Fn(f64) -> String>,
}

impl<T> Series<T> {
    pub fn new(
        value: impl Fn(&T) -> f64 + 'static,
        stroke: Hsla,
        fill: Hsla,
        name: impl Into<SharedString>,
        format: impl Fn(f64) -> String + 'static,
    ) -> Self {
        Self {
            value: Rc::new(value),
            stroke,
            fill,
            name: name.into(),
            format: Rc::new(format),
        }
    }
}

/// 百分比序列（`12.40%`）。CPU / 内存 / 交换三张图都用它。
pub fn percent_format(v: f64) -> String {
    format!("{v:.2}%")
}

/// 速率序列（`0.12`，两位小数）。网络上下行用它。
pub fn rate_format(v: f64) -> String {
    format!("{v:.2}")
}

/// 区域填充的不透明度（与官方 `AreaChart` 默认观感一致）
pub const FILL_ALPHA: f32 = 0.12;

/// 单纵轴折线 / 面积图，hover 数值可格式化。
///
/// 与官方 `AreaChart` 的差异只有一处：tooltip 的数值行走 [`Series::format`]。
/// 网格、坐标轴、区域填充、十字线与数据点都按官方同一套原语绘制。
#[derive(IntoPlot)]
pub struct ValueAreaChart<T>
where
    T: 'static,
{
    data: Vec<T>,
    x: Rc<dyn Fn(&T) -> SharedString>,
    series: Vec<Series<T>>,
    /// x 轴每几个点标一个刻度（60 点窗口用 15，即 4 个刻度）
    tick_margin: usize,
    id: ElementId,
}

impl<T: 'static> ValueAreaChart<T> {
    pub fn new(
        id: impl Into<ElementId>,
        data: Vec<T>,
        x: impl Fn(&T) -> SharedString + 'static,
        series: Vec<Series<T>>,
        tick_margin: usize,
    ) -> Self {
        Self {
            data,
            x: Rc::new(x),
            series,
            tick_margin: tick_margin.max(1),
            id: id.into(),
        }
    }

    /// x 轴与 y 轴的比例尺。`paint` 与 `tooltip_state` 共用一份，
    /// 保证「画出来的点」和「悬停命中的点」严格对得上。
    ///
    /// 与官方一致：y 的 domain 里**强制带上 0**（`.chain(Some(0.))`），
    /// 否则 3%~5% 的曲线会被拉伸成满屏噪声；上边留 10px 顶部余量。
    fn scales(
        &self,
        bounds: Bounds<Pixels>,
    ) -> Option<(ScalePoint<SharedString>, ScaleLinear<f64>)> {
        if self.data.is_empty() || self.series.is_empty() {
            return None;
        }
        let width = bounds.size.width.as_f32();
        let height = bounds.size.height.as_f32() - axis_gutter();

        let x_of = self.x.clone();
        // gpui-component 0.7：Plot 原语下沉到 gpui-base，scale 的 range 参数从
        // `Vec<f32>` 收紧成 `[f32; 2]`（`plot::scale::ScalePoint::new` 签名 `range: [f32; 2]`）。
        let x = ScalePoint::new(
            self.data.iter().map(|v| x_of(v)).collect::<Vec<_>>(),
            [0., width],
        );
        let domain = self
            .data
            .iter()
            .flat_map(|v| self.series.iter().map(|s| (s.value)(v)))
            .chain(Some(0.0))
            .collect::<Vec<_>>();
        let y = ScaleLinear::new(domain, [height, 10.]);
        Some((x, y))
    }

    /// x 轴刻度文案：首尾贴边对齐、中间居中（官方 `build_point_x_labels` 的
    /// 同款规则；它在 gpui-component 里是 `pub(crate)`，外部用不了）。
    fn x_labels(&self, x_scale: &ScalePoint<SharedString>, color: Hsla) -> Vec<AxisText> {
        let data_len = self.data.len();
        let x_of = self.x.clone();
        self.data
            .iter()
            .enumerate()
            .filter_map(|(i, d)| {
                if (i + 1) % self.tick_margin != 0 {
                    return None;
                }
                let label = x_of(d);
                x_scale.tick(&label).map(|tick| {
                    let align = match i {
                        0 if data_len == 1 => TextAlign::Center,
                        0 => TextAlign::Left,
                        i if i == data_len - 1 => TextAlign::Right,
                        _ => TextAlign::Center,
                    };
                    AxisText::new(label, tick, color).align(align)
                })
            })
            .collect()
    }
}

impl<T: 'static> Plot for ValueAreaChart<T> {
    fn paint(&mut self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
        let Some((x, y)) = self.scales(bounds) else {
            return;
        };
        let height = bounds.size.height.as_f32() - axis_gutter();

        // 1. x 轴线 + 刻度文案
        PlotAxis::new()
            .stroke(cx.theme().border)
            .x(height)
            .x_label(self.x_labels(&x, cx.theme().muted_foreground))
            .paint(&bounds, window, cx);

        // 2. 横向虚线网格（四等分）
        // `Grid::y` 收的是 `impl IntoIterator<Item = impl Into<Pixels>>`：0.7 里直接喂
        // 迭代器即可（旧版要先 `collect::<Vec<_>>()`，新签名下那样写反而推不出元素类型）。
        Grid::new()
            .y((0..=3).map(|i| height * i as f32 / 4.0))
            .stroke(cx.theme().border)
            .dash_array(&[px(4.), px(2.)])
            .paint(&bounds, window);

        // 3. 面积 + 折线（每条序列一条）
        for s in &self.series {
            let x = x.clone();
            let y = y.clone();
            let x_of = self.x.clone();
            let value = s.value.clone();
            Area::new()
                .data(self.data.iter())
                .x(move |d| x.tick(&x_of(d)))
                .y0(height)
                .y1(move |d| y.tick(&(value)(d)))
                .stroke(s.stroke)
                .fill(s.fill)
                .paint(&bounds, window);
        }
    }

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn tooltip_state(
        &self,
        position: gpui::Point<Pixels>,
        bounds: Bounds<Pixels>,
        _cx: &App,
    ) -> Option<TooltipState> {
        let (x, y) = self.scales(bounds)?;
        // 悬停到 x 轴刻度区不弹提示（官方同款守卫）
        if position.y.as_f32() > bounds.size.height.as_f32() - axis_gutter() {
            return None;
        }

        // 0.7 把 `ScalePoint::least_index` 改名为 `Scale::nearest_index`
        // （`plot/scale/point.rs`：语义不变，仍是「最近的点索引」，越界钳到末位）。
        let index = x.nearest_index(position.x.as_f32());
        let d = self.data.get(index)?;
        let x_of = self.x.clone();
        let x_tick = x.tick(&x_of(d))?;

        let dots = self
            .series
            .iter()
            .filter_map(|s| Some(point(px(x_tick), px(y.tick(&(s.value)(d))?))))
            .collect();

        Some(TooltipState::new(
            index,
            point(px(x_tick), position.y),
            dots,
        ))
    }

    fn tooltip(
        &self,
        state: &TooltipState,
        cursor: gpui::Point<Pixels>,
        bounds: Bounds<Pixels>,
        _window: &mut Window,
        cx: &mut App,
    ) -> Option<AnyElement> {
        let x_of = self.x.clone();
        let d = self.data.get(state.index)?;
        let dot_stroke = cx.theme().background;

        let mut tooltip = Tooltip::new(cursor, bounds.size)
            .gap(px(8.))
            .cross_line(
                CrossLine::new(state.cross_line)
                    .height(bounds.size.height.as_f32() - axis_gutter()),
            )
            .dots(state.dots.iter().enumerate().filter_map(|(i, p)| {
                self.series
                    .get(i)
                    .map(|s| Dot::new(*p).stroke(dot_stroke).fill(s.stroke))
            }))
            .title(x_of(d));

        // 与官方唯一的实质差异：数值走序列自带的 `format`
        for s in &self.series {
            tooltip = tooltip.row(s.stroke, s.name.clone(), (s.format)((s.value)(d)));
        }

        Some(tooltip.into_any_element())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// hover 文案：百分比带 `%` 两位小数，速率两位小数不含单位。
    /// 这两条是运行反馈（2026-09-18）直接要求的呈现，别被「看起来更聪明」
    /// 的动态精度改回去。
    #[test]
    fn hover_formats_are_pinned() {
        assert_eq!(percent_format(12.4), "12.40%");
        assert_eq!(percent_format(0.0), "0.00%");
        assert_eq!(percent_format(100.0), "100.00%");
        // 网络速率：两位小数，0 也要写成 0.00（空图不显示裸 0）
        assert_eq!(rate_format(0.0), "0.00");
        assert_eq!(rate_format(0.123_456_789), "0.12");
        assert_eq!(rate_format(12.345), "12.35");
    }
}
