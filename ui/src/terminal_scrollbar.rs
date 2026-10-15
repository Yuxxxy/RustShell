//! 终端滚动条（T-2）：把 rustshell-terminal 的`display_offset`接到官方`Scrollbar`。
//!
//! 2026-09-17 第五轮运行反馈「鼠标移动滑动轴的时候不跟手」：拖拽目标从「等 300ms 轮询
//! 落地」改为「产生目标的下一帧就在渲染路径落地」—— 落在 `pending` 里的目标由
//! `main_view::split_actions::render_term_pane` 取走（见 [`TermScrollHandle::apply_pending_with`]
//! 的两个调用点）。滑块与终端内容因此同帧移动，而不是「滑块停住、几百毫秒后跳一下」。

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gpui::{
    div, px, AnyElement, Bounds, Entity, InteractiveElement as _, IntoElement, ParentElement as _,
    Pixels, Point, Size, Styled as _,
};
use gpui_component::scroll::{Scrollbar, ScrollbarHandle, ScrollbarMode};
use rustshell_terminal::{TermScrollMetrics, TerminalView};

/// 终端滚动句柄：把「像素偏移」与「终端 display_offset」双向换算。
#[derive(Clone)]
pub struct TermScrollHandle {
    term: Entity<TerminalView>,
    /// 最近一次渲染缓存的几何（拖拽期间避免反复读终端）
    metrics: Rc<Cell<TermScrollMetrics>>,
    /// 最近一次渲染的视口 bounds（官方 Scrollbar 用它算滑块比例与命中）
    bounds: Rc<Cell<Bounds<Pixels>>>,
    /// 待应用的 display_offset（`Some`= 有拖拽/点击产生的目标值）
    pending: Rc<RefCell<Option<usize>>>,
}

impl TermScrollHandle {
    pub fn new(term: Entity<TerminalView>) -> Self {
        Self {
            term,
            metrics: Rc::new(Cell::new(TermScrollMetrics::default())),
            bounds: Rc::new(Cell::new(Bounds::default())),
            pending: Rc::new(RefCell::new(None)),
        }
    }

    /// 刷新几何缓存（每帧渲染前调用；从终端读一次快照）。
    pub fn sync(&self, cx: &gpui::App) {
        self.metrics.set(self.term.read(cx).scroll_metrics());
    }

    /// 记录视口 bounds（渲染时由布局写入）。
    ///
    /// 注意：终端滚动条现在走官方 `Scrollbar::viewport_from_layout()`（见
    /// `main_view::split_actions::render_term_pane`），视口即滚动条自身的布局盒，
    /// 因此本方法目前**没有调用点**。保留它是因为官方 `ScrollbarHandle` 的语义
    /// 就是「由句柄提供视口」，一旦将来需要把视口与滚动条元素解耦（例如滚动条
    /// 不再铺满整列高度），调用点应当写在这里而不是再引一个字段。
    #[allow(dead_code)]
    pub fn set_bounds(&self, b: Bounds<Pixels>) {
        self.bounds.set(b);
    }

    pub fn metrics(&self) -> TermScrollMetrics {
        self.metrics.get()
    }

    /// 消费并应用待写入的偏移。
    ///
    /// 两个调用点（都在 `main_view`），共用同一份 `pending`：
    /// * **渲染路径**（`split_actions::render_term_pane`）—— 主路径。拖拽要跟手就必须
    ///   在产生目标的那一帧落地：`ScrollbarHandle::set_offset` 只有 `&self`，写不进终端
    ///   （`Entity::update` 要 `&mut App`），所以目标先暂存在这里，由渲染时唯一的
    ///   `&mut App` 钩子取走。见那里的「跟手」注释。
    /// * **轮询**（`MainView::poll`，300ms）—— 兜底：目标产生的窗格若一直不重绘
    ///   （标签页不在屏上 / 窗口被遮挡），偏移不会一直挂着。
    ///
    /// 返回是否真的产生了位移，供调用方决定要不要 `cx.notify()`。
    pub fn apply_pending_with(&self, cx: &mut gpui::App, term: &Entity<TerminalView>) -> bool {
        let Some(target) = self.pending.borrow_mut().take() else {
            return false;
        };
        let mut changed = false;
        term.update(cx, |tv, _| {
            let before = tv.scroll_metrics().display_offset;
            tv.set_display_offset(target);
            changed = tv.scroll_metrics().display_offset != before;
        });
        changed
    }
}

/// 终端滚动条的覆盖层：铺满整个窗格、绝对定位，官方 `Scrollbar` 取自身布局盒作视口。
///
/// 四个不变量（缺一条就整条不见，或缺一条等于白画）：
///
/// 1. **自带视口** —— `viewport_from_layout()`。官方 `Scrollbar::prepaint` 第一步是
///    `resolved_viewport_bounds()`：拿到空 bounds 就把 content_mask 收成空矩形
///    （绘制被整条裁掉）、hitbox 建成零尺寸（连拖拽都点不中）。句柄侧的
///    `viewport_bounds()` 从未被写入过，所以这里必须让滚动条用自己的布局盒。
/// 2. **铺满窗格**（`inset_0()`，官方 `Scrollable` 的写法）—— 滚动条把**自身盒宽**
///    当容器宽，而竖直轨道固定 [`Scrollbar::width`]（16px）贴在盒右缘。盒子比轨道窄
///    时，轨道与滑块都会越出盒左缘被 content_mask 裁掉：盒子给 8px，滑块实际只剩
///    2px 可见。铺满窗格后轨道与滑块都完整落在窗格内。
/// 3. **不遮挡终端的鼠标事件** —— 覆盖层的 hitbox 是 `HitboxBehavior::Normal`
///    （官方 `insert_hitbox` 的第一个参数），语义是「不影响其它 hitbox 的命中」，
///    所以它不会吞掉终端的划选与右键。只有右缘那一条轨道会拦下滚轮与拖拽，
///    那正是滚动条该有的行为。
/// 4. **滑块取色要看得见** —— 见 [`crate::theme::scrollbar_thumb`]。前三条决定「画没画、
///    画在哪」，这一条决定「看不看得见」；官方浅色预设的滑块在白底上约 `(234,234,234)`，
///    画了也等于没画。
///
/// 挂载条件由调用方给（窗格有终端即可）：**不设闸门**，有没有东西可滚交给官方
/// `Scrollbar` 自己判（内容不超一屏时它连轨道都不往 states 里放）。
///
/// 对 `H` 泛化而不是写死 `TermScrollHandle`：这样护栏能用桩句柄渲染**这份真实写法**，
/// 直接量它画出了什么（见本模块 `overlay_paints_an_unclipped_thumb_at_the_edge`）。
///
/// 滑块三态取色取自 [`crate::theme::scrollbar_thumb`]（唯一入口）。不用官方预设色的
/// 原因见该函数：浅色预设的滑块在白底上几乎不可见，等于「有滚动条但看不见」。
pub fn overlay<H: ScrollbarHandle + Clone>(handle: &H, cx: &gpui::App) -> AnyElement {
    let (thumb, thumb_hover, thumb_active) = crate::theme::scrollbar_thumb(cx);
    div()
        .debug_selector(|| "term-scrollbar-layer".to_string())
        .absolute()
        .inset_0()
        .child(
            Scrollbar::vertical(handle)
                .id("term-scrollbar")
                // 常显：`Hover` 模式下命中盒只有 16px 宽的一条，用户不会恰好把
                // 鼠标停在那上面才知道有滚动条；`Scrolling` 模式闲置 2s 后又淡出，
                // 等于「内容很长却看不到滑动轴」。
                .mode(ScrollbarMode::Always)
                .viewport_from_layout()
                .styles(move |s| {
                    s.thumb(|t| t.bg(thumb))
                        .thumb_hover(|t| t.bg(thumb_hover))
                        .thumb_active(|t| t.bg(thumb_active))
                }),
        )
        .into_any_element()
}

/// 「距顶像素偏移」← 「display_offset」。
///
/// **符号是负的**：gpui 的 `ScrollHandle` 约定向下滚为负（`Div` 处置滚动时
/// `scroll_offset.y = scroll_offset.y.clamp(-scroll_max.y, px(0.))`，可滚区间落在
/// `[-max, 0]`），官方 `Scrollbar` 直接把这个值代进滑块位置公式。返回正值会让
/// 滑块被推到轨道上方之外并整条裁掉 —— 症状正是「有回滚内容、右缘却什么都没有」。
fn px_offset_from_display(display_offset: usize, history: usize, cell_height: f32) -> f32 {
    let from_top = history.saturating_sub(display_offset) as f32;
    -(from_top * cell_height)
}

/// 「display_offset」← 「距顶像素偏移」（负值）。
///
/// `None` = 这次输入不该产生位移（字高未知 / 没有历史行），调用方应保持原状。
/// 拖拽时鼠标可能跑到轨道外，所以先夹到 `[0, history]` 再换算。
fn display_offset_from_px(px_y: f32, history: usize, cell_height: f32) -> Option<usize> {
    if cell_height <= 0.0 || history == 0 {
        return None;
    }
    let from_top = ((-px_y) / cell_height).round().clamp(0.0, history as f32) as usize;
    Some(history.saturating_sub(from_top))
}

impl ScrollbarHandle for TermScrollHandle {
    fn viewport_bounds(&self) -> Bounds<Pixels> {
        self.bounds.get()
    }

    fn offset(&self) -> Point<Pixels> {
        let m = self.metrics.get();
        // display_offset: 0 = 底部；返回值: 0 = 顶部、向下滚为负（见上）。
        let y = px_offset_from_display(m.display_offset, m.history_size, f32::from(m.cell_height));
        Point::new(px(0.), px(y))
    }

    fn set_offset(&self, offset: Point<Pixels>) {
        // 只暂存目标：`set_offset` 拿不到 `&mut App`（见 `ScrollbarHandle` 的签名），
        // 真正写进终端由渲染路径在同一帧完成（`render_term_pane`，见那里的「跟手」注释）。
        let m = self.metrics.get();
        let px_y: f32 = offset.y.into();
        if let Some(target) = display_offset_from_px(px_y, m.history_size, f32::from(m.cell_height))
        {
            *self.pending.borrow_mut() = Some(target);
        }
    }

    fn content_size(&self) -> Size<Pixels> {
        let m = self.metrics.get();
        let ch = f32::from(m.cell_height);
        let total = (m.history_size + m.screen_lines) as f32 * ch;
        Size::new(px(0.), px(total))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::prod;
    use gpui::{Context, Quad, Render, Window};

    /// 生产代码正文（剥注释 + 切测试模块，见 `crate::test_support`）。
    fn production_source() -> String {
        prod(include_str!("terminal_scrollbar.rs"))
    }

    /// 探针用的字高（逻辑像素）。取整数只是为了让「视口行数 × 字高 = 视口高」
    /// 恰好成立，几何就不是近似的。
    const PROBE_CELL_H: f32 = 20.0;

    /// 桩滚动句柄：**几何全部由生产函数换算**（`px_offset_from_display` /
    /// `display_offset_from_px` / 与 `TermScrollHandle` 同形的 `content_size`），
    /// 用来在无头环境里量 [`overlay`] 真的画出了什么。
    ///
    /// 这里**不能**直接喂一个像素偏移：桩喂什么就画什么，「display_offset ↔ 像素」
    /// 那一段换算里的缺陷会从桩旁边溜过去 —— 2026-09-17 的 `offset` 符号写反正是
    /// 这种形态（当时把符号翻回去，桩驱动的绘制用例照样全绿，只有纯函数用例变红）。
    #[derive(Clone)]
    struct StubHandle {
        metrics: TermScrollMetrics,
        pending: std::rc::Rc<std::cell::RefCell<Option<usize>>>,
    }

    impl StubHandle {
        /// `history` 行回滚、视口 `screen` 行、回滚位置 `display_offset`（0 = 底部）。
        fn new(history: usize, screen: usize, cell_h: f32, display_offset: usize) -> Self {
            Self {
                metrics: TermScrollMetrics {
                    history_size: history,
                    screen_lines: screen,
                    display_offset,
                    cell_height: px(cell_h),
                },
                pending: Default::default(),
            }
        }
    }

    impl ScrollbarHandle for StubHandle {
        fn viewport_bounds(&self) -> Bounds<Pixels> {
            // 刻意留空：真实现（`TermScrollHandle`）的这个字段也从未被写入过，
            // 滚动条必须靠 `viewport_from_layout()` 自取布局盒才能有视口。
            Bounds::default()
        }
        fn offset(&self) -> Point<Pixels> {
            let m = self.metrics;
            Point::new(
                px(0.),
                px(px_offset_from_display(
                    m.display_offset,
                    m.history_size,
                    f32::from(m.cell_height),
                )),
            )
        }
        fn set_offset(&self, offset: Point<Pixels>) {
            let m = self.metrics;
            let px_y: f32 = offset.y.into();
            if let Some(target) =
                display_offset_from_px(px_y, m.history_size, f32::from(m.cell_height))
            {
                *self.pending.borrow_mut() = Some(target);
            }
        }
        fn content_size(&self) -> Size<Pixels> {
            let m = self.metrics;
            let ch = f32::from(m.cell_height);
            Size::new(px(0.), px((m.history_size + m.screen_lines) as f32 * ch))
        }
    }

    /// 探针视图：一个铺满窗口的容器 + 生产代码的滚动条覆盖层。
    ///
    /// 视口行数由窗口高现算（覆盖层铺满窗口，视口就是窗口），所以
    /// `视口行数 × 字高 ≈ 视口高`，官方那侧算出的 `max_offset = 内容高 - 视口高`
    /// 与句柄的取值范围一致。**「滚到底」（`display_offset = 0`）必须是必测态**：
    /// 滑块位置公式对符号极敏感，而滚动位置 0 处正负号同效，只有滚到底才露馅。
    struct OverlayProbe {
        history: usize,
        display_offset: usize,
    }

    impl Render for OverlayProbe {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let viewport: f32 = window.bounds().size.height.into();
            let screen = (viewport / PROBE_CELL_H).round().max(1.0) as usize;
            let handle = StubHandle::new(
                self.history,
                screen,
                PROBE_CELL_H,
                self.display_offset.min(self.history),
            );
            div().size_full().relative().child(overlay(&handle, cx))
        }
    }

    /// 画一帧，返回 (全部 quad, 窗口宽, 窗口高)，均为缩放后物理像素。
    ///
    /// 注意窗口 bounds 是**逻辑**像素（1920×1080），quad 是 2×DPI 的**物理**像素
    /// （3840×2160），所以比较前必须统一乘窗口自身的缩放因子 —— 直接拿逻辑宽去
    /// 比 quad 的 x，会把整条滚动条判成「不在右缘」。
    fn draw_probe(
        cx: &mut gpui::TestAppContext,
        history: usize,
        display_offset: usize,
    ) -> (Vec<Quad>, f32, f32) {
        // 滚动条取色要读全局主题（`theme::scrollbar_thumb`）；不装主题会在
        // prepaint 里 `no state of type ... Theme exists` 直接 panic。
        cx.update(|cx| {
            gpui_component::init(cx);
            crate::theme::apply_theme_mode(cx, rustshell_core::theme::ThemeMode::Dark);
        });
        let (_, cx) = cx.add_window_view(move |_, _| OverlayProbe {
            history,
            display_offset,
        });
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
        });
        cx.update(|window, _| {
            let wb = window.bounds();
            let scale = window.scale_factor();
            let width: f32 = wb.size.width.into();
            let height: f32 = wb.size.height.into();
            (window.painted_quads(), width * scale, height * scale)
        })
    }

    /// quad 的实质底色透明度；渐变等非纯色一律当作不透明（不会误判成「看不见」）。
    fn solid_alpha(q: &Quad) -> f32 {
        q.background.as_solid().map(|c| c.a).unwrap_or(1.0)
    }

    /// 「右缘那一条」= 落在窗口右侧、且明显比窗口窄的 quad。
    fn right_edge_strips(quads: &[Quad], window_width: f32) -> Vec<Quad> {
        quads
            .iter()
            .filter(|q| {
                let x = q.bounds.origin.x.as_f32();
                let w = q.bounds.size.width.as_f32();
                x > window_width * 0.75 && w < window_width * 0.25
            })
            .cloned()
            .collect()
    }

    /// 从右缘条带里挑出**滑块**：高度明显小于整屏高的那些。
    ///
    /// 轨道（含其 1px 描边）是整屏高，滑块按「视口 / 内容」比例缩短 —— 不区分这两者
    /// 就会把整屏高的轨道条也当成滑块，然后要求它的掩码包住 16px 轨道底色，那是错的
    /// 判据（2026-09-17 首次跑该用例就是这么假失败的：真滑块 x=3820 w=12 掩码
    /// 0..3840 完全没被裁，被逮住的是 `x=3808 w=32` 的轨道描边）。
    fn thumb_strips(strips: &[Quad], window_height: f32) -> Vec<&Quad> {
        strips
            .iter()
            .filter(|q| q.bounds.size.height.as_f32() < window_height * 0.9)
            .collect()
    }

    /// 回归（2026-09-17 运行反馈「终端内容很长没有滑动轴」）——**不读源码，直接量绘制**。
    ///
    /// 这是本模块唯一有牙的护栏：给它一份确定性几何，让它真的画一帧，然后检查
    /// 右缘到底有没有出现滑块。它同时守住三个不变量：
    ///
    /// 1. **必须有滑块** —— 内容（3000）远高于视口时，右缘必须出现一条窄 quad。
    ///    仅靠源码串断言是拦不住缺陷的：2026-09-17 那一版源码里 `viewport_from_layout`
    ///    / `ScrollbarMode::Always` / 覆盖层宽度全都在，画面上却什么都没有。
    /// 2. **滑块不得被裁** —— 覆盖层的 content_mask 必须完整包住滑块。上一版覆盖层
    ///    只有 8px 宽，而官方轨道固定 16px、滑块距轨道右缘 4px，于是滑块（6px）只剩
    ///    2px 落在掩码内：源码断言看不出这件事，量 quad 与 content_mask 可以。
    /// 3. **无内容时不许画** —— 内容（100）低于视口时右缘必须一条都没有，
    ///    否则就是「没有任何东西可滚却挂着一条滚动条」。
    #[gpui::test]
    fn overlay_paints_an_unclipped_thumb_at_the_edge(cx: &mut gpui::TestAppContext) {
        // 100 行回滚、停在最顶（display_offset = 全部历史行）：内容远高于视口。
        let (quads, window_width, window_height) = draw_probe(cx, 100, 100);
        let strips = right_edge_strips(&quads, window_width);
        assert!(
            !strips.is_empty(),
            "内容远高于视口时，滚动条覆盖层没有画出任何东西（窗口宽 {window_width}，\
             共 {} 个 quad）。",
            quads.len()
        );
        let thumbs = thumb_strips(&strips, window_height);
        assert!(
            !thumbs.is_empty(),
            "右缘只有整屏高的轨道、没有按比例缩短的滑块：官方 `Scrollbar` 判定「无内容可滚」\
             （100 行回滚却算出视口 ≥ 内容）。共 {} 条右缘条带。",
            strips.len()
        );
        for q in thumbs {
            let b = q.bounds;
            let m = q.content_mask.bounds;
            let thumb_left = b.origin.x.as_f32();
            let thumb_right = thumb_left + b.size.width.as_f32();
            let mask_left = m.origin.x.as_f32();
            let mask_right = mask_left + m.size.width.as_f32();
            assert!(
                mask_left <= thumb_left + 0.5 && mask_right >= thumb_right - 0.5,
                "滑块被 content_mask 裁掉：滑块 x={thumb_left:.1}..{thumb_right:.1}，\
                 掩码 x={mask_left:.1}..{mask_right:.1}。覆盖层必须铺满窗格\
                 （官方 `Scrollable` 的 `inset_0` 写法），否则轨道与滑块都越出盒缘被切断。"
            );
            assert!(
                thumb_right <= window_width + 0.5,
                "滑块画到了窗口右缘之外：滑块右沿 {thumb_right:.1} > 窗口宽 {window_width:.1}。"
            );
            // 「画了但看不见」也算缺陷：官方浅色预设的滑块铺在白底上约 (234,234,234)，
            // 用户两次反馈「滑动轴还是没有」都包含这一层。取色必须来自
            // `theme::scrollbar_thumb`（`fg_muted` 的 60%），实质透明度远高于此。
            let alpha = solid_alpha(q);
            assert!(
                alpha >= 0.4,
                "滑块实质透明度只有 {alpha:.2}，在底色上等于看不见 —— 取色必须走 \
                 `crate::theme::scrollbar_thumb`（唯一入口），不要沿用官方预设的淡色。"
            );
        }
    }

    /// 回归（2026-09-17 运行反馈「终端内容很长没有滑动轴」）——**滚到底**那一态必须量。
    ///
    /// 官方滑块位置公式是
    /// `thumb_start = -(scroll_position / (内容高 - 视口高) * (视口高 - 滑块长))`。
    /// `scroll_position` 取句柄的 `offset().y`，而 gpui 的约定是**向下滚为负**
    /// （`Div` 处置滚动时 `offset.y = offset.y.clamp(-scroll_max, 0)`，可滚区间落在
    /// `[-max, 0]`）。上一版 `offset()` 返回正值：滑到底时 `thumb_start` 被算成
    /// 正向超调，滑块整体落在轨道**上方**、被 content_mask 整条裁掉 —— 源码里
    /// `viewport_from_layout` / `ScrollbarMode::Always` / `inset_0` 一个都不少，
    /// 画面上却什么都没有。
    ///
    /// 而「滚动位置 0」（滑块恰好在顶）对符号不敏感，正负号都会画在顶部，
    /// 所以只测那一态是拦不住的 —— 本用例专测滚到底：滑块必须落在视口内且贴底。
    #[gpui::test]
    fn overlay_keeps_the_thumb_inside_the_track_at_full_scroll(cx: &mut gpui::TestAppContext) {
        // 100 行回滚、停在最底（display_offset = 0）—— 符号错时唯一露馅的一态。
        let (quads, window_width, window_height) = draw_probe(cx, 100, 0);
        let strips = right_edge_strips(&quads, window_width);
        let thumbs = thumb_strips(&strips, window_height);
        let thumb = *thumbs.first().expect(
            "滚到底时右缘找不到滑块：`offset().y` 的符号很可能写反了 —— 正值会把滑块\
             推到轨道上方之外，被 content_mask 整条裁掉。",
        );
        let top = thumb.bounds.origin.y.as_f32();
        let bottom = top + thumb.bounds.size.height.as_f32();
        assert!(
            top >= -2.0 && bottom <= window_height + 2.0,
            "滚到底时滑块跑到了视口之外：y={top:.1}..{bottom:.1}，视口高 {window_height:.1}。\
             gpui 约定向下滚 `offset.y` 为负，返回正值会把滑块推到轨道上方。"
        );
        assert!(
            bottom >= window_height * 0.9,
            "滚到底时滑块应当贴在轨道底端：滑块底 {bottom:.1}，视口高 {window_height:.1}。"
        );
        let alpha = solid_alpha(thumb);
        assert!(
            alpha >= 0.4,
            "滚到底时滑块实质透明度只有 {alpha:.2} —— 取色必须走 \
             `crate::theme::scrollbar_thumb`（唯一入口）。"
        );
    }

    /// 反向：内容不超一屏时右缘不得出现任何一条（官方 `Scrollbar` 自己会判）。
    #[gpui::test]
    fn overlay_paints_nothing_when_content_fits(cx: &mut gpui::TestAppContext) {
        // 0 行回滚：内容高 = 视口行数 × 字高 = 视口高，官方判「无可滚内容」。
        let (quads, window_width, _) = draw_probe(cx, 0, 0);
        let strips = right_edge_strips(&quads, window_width);
        assert!(
            strips.is_empty(),
            "内容未超一屏却画出了 {} 条右缘滚动条 —— 滚动条必须只在有东西可滚时出现。",
            strips.len()
        );
    }

    /// 结构侧：覆盖层必须自带视口且常显。判据是「官方那个能救命的开关在不在」——
    /// 行为侧的牙由上面两条绘制用例负责。
    #[test]
    fn overlay_contract_is_self_viewport_and_always_on() {
        let code = production_source();
        assert!(
            code.contains("viewport_from_layout"),
            "滚动条必须自带视口（`viewport_from_layout`）：句柄的 `viewport_bounds` \
             从未被写入，官方 Scrollbar 拿到空 bounds 会把整条裁掉。"
        );
        assert!(
            code.contains("ScrollbarMode::Always") && !code.contains("ScrollbarMode::Hover"),
            "滚动条必须常显；`Hover`/`Scrolling` 都要求用户先猜到那儿有滚动条。"
        );
        assert!(
            code.contains("inset_0"),
            "覆盖层必须铺满窗格（`inset_0`）—— 盒宽小于轨道宽度时滑块会被裁。"
        );
        assert!(
            code.contains("theme::scrollbar_thumb"),
            "滑块取色必须走 `crate::theme::scrollbar_thumb`（唯一入口）—— \
             官方预设色在浅色主题下淡到看不见，画了也等于没画。"
        );
    }

    /// 挂载侧：`split_actions.rs` 不得再自带「内容是否超过一屏」的预闸门。
    ///
    /// 闸门判错就是整条滚动条根本挂不上去，而官方渲染器内部的判定连表态的机会都没有；
    /// 两处各判一次，就是两套口径。判据取「该文件里不再出现 `overflow()` 这个调用」。
    #[test]
    fn pane_mounts_the_overlay_without_its_own_overflow_gate() {
        let pane = prod(include_str!("main_view/split_actions.rs"));
        assert!(
            pane.contains("crate::terminal_scrollbar::overlay("),
            "终端窗格必须挂上共享的滚动条覆盖层（`terminal_scrollbar::overlay`）。"
        );
        assert!(
            !pane.contains(".overflow()"),
            "终端窗格不得再自带「内容是否超过一屏」的闸门 —— 有没有东西可滚由官方 \
             `Scrollbar` 一处判定；两处口径迟早分叉。"
        );
    }

    /// 跟手（2026-09-17 第五轮运行反馈「鼠标移动滑动轴的时候不跟手」）：拖拽写下的
    /// 目标偏移必须在**渲染路径**落地，而且必须早于几何缓存刷新。
    ///
    /// 只做源码级顺序断言。真正的牙在运行时，而无头环境里既没有真实拖拽、也没有
    /// 300ms 轮询，硬测只会得到假绿；但「谁先谁后」这件事一旦被改回去，症状恰好是
    /// 用户看到的那一个 —— 滑块停住、内容隔几百毫秒跳一下，而所有绘制用例依然是绿的。
    ///
    /// 两个判据各挡一件事：
    /// * 没有 `apply_pending_with` ⇒ 目标只能等 `MainView::poll`（300ms）落地 = 不跟手；
    /// * 顺序写反 ⇒ `sync` 读到的还是旧快照，`offset()` 也是旧值 = 滑块与内容不同帧。
    #[test]
    fn pane_applies_the_drag_target_before_refreshing_geometry() {
        let pane = prod(include_str!("main_view/split_actions.rs"));
        let apply = pane.find("apply_pending_with").expect(
            "终端窗格渲染必须先落地拖拽产生的目标偏移（`apply_pending_with`）—— \
             否则拖拽要等 300ms 轮询才动，就是运行反馈的「不跟手」。",
        );
        let sync = pane
            .find("handle.sync(cx)")
            .expect("终端窗格渲染必须刷新滚动几何缓存（`handle.sync`）。");
        assert!(
            apply < sync,
            "顺序写反了：`sync` 必须读**落地之后**的终端快照（`apply_pending_with` 在前），\
             否则 `offset()` 还是旧值，滑块与终端内容不在同一帧移动。"
        );
    }

    /// 本模块自己不能把「视口」当成已经写好：`viewport_bounds` 必须诚实返回缓存值。
    #[test]
    fn viewport_bounds_reflects_whatever_was_cached() {
        let code = production_source();
        assert!(
            code.contains("fn viewport_bounds(&self) -> Bounds<Pixels>")
                && code.contains("self.bounds.get()"),
            "`viewport_bounds` 必须返回缓存值本身，不得凭空造一个矩形。"
        );
    }

    /// 换算公式的往返（直接测生产函数，不另抄一份公式，避免两边悄悄分叉）。
    #[test]
    fn offset_round_trips_top_and_bottom() {
        let ch = 16.0;
        let history = 100usize;
        // 顶部：距顶 0 像素 ⇔ display_offset = 全部历史行
        assert_eq!(display_offset_from_px(0.0, history, ch), Some(history));
        // 底部：距顶 = 历史总高（**带回负号**）⇔ display_offset = 0
        assert_eq!(
            display_offset_from_px(-(history as f32) * ch, history, ch),
            Some(0)
        );
        // 往返
        for d in [0usize, 1, 37, 99, 100] {
            let px_y = px_offset_from_display(d, history, ch);
            assert_eq!(display_offset_from_px(px_y, history, ch), Some(d));
        }
    }

    /// **符号约定**（2026-09-17 缺陷根因）：向下滚 `offset.y` 必须为负。
    ///
    /// 官方 `Scrollbar` 把这个值直接代进滑块位置公式，而 gpui 的 `Div` 维护的
    /// 滚动区间是 `[-max, 0]`；本模块若返回正值，滑块会被推到轨道上方之外。
    #[test]
    fn offset_is_negative_when_scrolled_down() {
        let ch = 16.0;
        let history = 100usize;
        // 顶部 = 0
        assert_eq!(px_offset_from_display(history, history, ch), 0.0);
        // 底部 = 最负
        assert_eq!(px_offset_from_display(0, history, ch), -(100.0 * ch));
        // 中间一律为负
        assert!(px_offset_from_display(50, history, ch) < 0.0);
    }

    /// 未溢出（history == 0）时任何输入都不得产生位移（AC-T2-1 前提）。
    #[test]
    fn no_history_stays_at_zero() {
        assert_eq!(display_offset_from_px(-500.0, 0, 16.0), None);
        assert_eq!(display_offset_from_px(0.0, 50, 0.0), None);
        assert_eq!(px_offset_from_display(0, 0, 16.0), 0.0);
    }

    /// 拖拽可能把鼠标拖到轨道之外：换算必须夹回合法区间，不得 panic / 越界。
    #[test]
    fn drag_beyond_the_track_clamps() {
        let ch = 16.0;
        let history = 100usize;
        assert_eq!(display_offset_from_px(-9_999.0, history, ch), Some(0));
        assert_eq!(display_offset_from_px(9_999.0, history, ch), Some(history));
    }
}
