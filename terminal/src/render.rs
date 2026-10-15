//! Terminal rendering module.
//!
//! This module provides [`TerminalRenderer`], which handles efficient rendering of
//! terminal content using GPUI's text and drawing systems.
//!
//! # Rendering Pipeline
//!
//! The renderer processes the terminal grid in a small, fixed number of passes **per
//! row** — never per cell:
//!
//! ```text
//! Terminal Grid → Visual Pass → Draw Pass
//!                     │              │
//!                     ├─ resolve fg/bg + attrs once per cell (cached in `Visual`)
//!                     │              │
//!                     │              ├─ background rects (single streaming merge)
//!                     │              ├─ box-drawing geometry (vector paths)
//!                     │              ├─ batched text segments (one shape per segment)
//!                     │              └─ cursor
//! ```
//!
//! # Optimizations
//!
//! Everything below is measured against the previous implementation, which cloned
//! the whole row twice per frame, built a `HashSet` per row, and issued one
//! `shape_line` call **per character** (~`rows × cols` shaping calls per frame):
//!
//! 1. **One scratch buffer per frame, zero per-row allocations.** [`FrameScratch`]
//!    holds the row's resolved cell state and is cleared (not freed) between rows.
//!
//! 2. **Colors/attributes resolved once per cell.** The previous code called
//!    `cell_colors` up to four times per cell (box-drawing scan, horizontal span
//!    scan, vertical pass, text pass). Resolving once and caching in [`Visual`]
//!    removes the repeated palette lookups and the `Flags` bit tests that came
//!    with them.
//!
//! 3. **Batched text.** Adjacent cells with identical styling are concatenated into
//!    a single string and shaped **once per segment** instead of once per cell.
//!    `force_width = cell_width × span` makes GPUI pin every base glyph to its own
//!    cell (see `apply_force_width_to_layout` in gpui's text system), so batching
//!    cannot drift out of alignment for wide glyphs, font fallbacks, or zero-width
//!    combiner marks. Combined with GPUI's per-frame line-layout cache, a static
//!    screen re-shapes nothing at all on the following frame.
//!
//! 4. **No per-row `HashSet`.** "Was this column part of a horizontal box-drawing
//!    span?" is now a reusable `Vec<bool>` in [`FrameScratch`] (`flash`).
//!
//! 5. **Search spans are merged, not rescanned.** `search_spans` is walked once per
//!    row with a moving cursor instead of a full `Vec` filter per row.
//!
//! 6. **Solid backgrounds only.** An empty border is passed to `quad` directly —
//!    no throwaway `Edges` + `transparent_black` construction per rectangle.
//!
//! # Cell Dimensions
//!
//! Cell size is calculated from actual font metrics using the '│' character,
//! which spans the full cell height in properly designed terminal fonts:
//!
//! - **Width**: Measured from shaped '│' character
//! - **Height**: `(ascent + descent) × line_height_multiplier`
//!
//! The `line_height_multiplier` (default 1.0) can be adjusted to add extra
//! vertical space if needed for specific fonts.
//!
//! # Example
//!
//! ```ignore
//! use gpui::px;
//! use rustshell_terminal::{ColorPalette, TerminalRenderer};
//!
//! let renderer = TerminalRenderer::new(
//!     "JetBrains Mono".to_string(),
//!     px(14.0),
//!     1.0,  // line height multiplier
//!     ColorPalette::default(),
//! );
//! ```

use crate::box_drawing;
use crate::colors::ColorPalette;
use crate::event::GpuiEventProxy;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::Term;
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::vte::ansi::{Color, CursorShape};
use gpui::{
    App, Bounds, Edges, Font, FontFeatures, FontStyle, FontWeight, Hsla, Pixels, Point, Size,
    StrikethroughStyle, TextAlign, TextRun, UnderlineStyle, Window, px, quad, transparent_black,
};

/// 无边框（背景矩形 / 块光标填充用）。
///
/// `Edges<Pixels>` 不是 `const fn` 可构造的，故用函数返回；
/// 它只是 4 个 `Pixels::ZERO`，编译器会完全内联。
#[inline]
fn no_border() -> Edges<Pixels> {
    Edges {
        top: px(0.0),
        right: px(0.0),
        bottom: px(0.0),
        left: px(0.0),
    }
}

/// 直接用 `quad` 画一个纯色矩形（无描边）。
///
/// `quad` 是纯函数（只构造 `PaintQuad`），不触碰 `Window`，
/// 因此这里返回值而不是传出 `&mut Window`，调用点更难写错。
#[inline]
fn solid_quad(bounds: Bounds<Pixels>, color: Hsla) -> gpui::PaintQuad {
    quad(
        bounds,
        px(0.0),
        color,
        no_border(),
        transparent_black(),
        Default::default(),
    )
}

/// A batched run of text with consistent styling.
///
/// This struct groups adjacent terminal cells with identical visual attributes
/// to reduce the number of text rendering calls.
#[derive(Debug, Clone)]
pub struct BatchedTextRun {
    /// The text content to render
    pub text: String,

    /// Starting column position
    pub start_col: usize,

    /// Row position
    pub row: usize,

    /// Foreground color
    pub fg_color: Hsla,

    /// Background color
    pub bg_color: Hsla,

    /// Bold flag
    pub bold: bool,

    /// Italic flag
    pub italic: bool,

    /// Underline flag
    pub underline: bool,

    /// 双下划线（SGR 21）
    pub double_underline: bool,

    /// 删除线（SGR 9）
    pub strikethrough: bool,
}

/// 单元格视觉属性位（`u8` 位标志，存进 [`Visual`] 的 1 字节里）。
///
/// 不用 7 个 `bool` 字段：`Visual` 每个单元格一份，尺寸越小越好拷贝。
pub mod bit {
    /// 粗体（SGR 1）
    pub const BOLD: u8 = 1 << 0;
    /// 斜体（SGR 3）
    pub const ITALIC: u8 = 1 << 1;
    /// 下划线（SGR 4 及 ALL_UNDERLINES 各种变体）
    pub const UNDERLINE: u8 = 1 << 2;
    /// 双下划线（SGR 21）
    pub const DOUBLE_UNDERLINE: u8 = 1 << 3;
    /// 删除线（SGR 9）
    pub const STRIKETHROUGH: u8 = 1 << 4;
    /// 不显示字符但占位（SGR 8，密码输入）
    pub const HIDDEN: u8 = 1 << 5;
    /// 波浪下划线（SGR 4:3）
    pub const UNDERCURL: u8 = 1 << 6;
}

/// 单元格视觉属性：从 alacritty 的 `Flags` 位标志提取。
///
/// **这是属性解析的唯一收口**：`layout_row`、背景矩形、文本分段、box-drawing
/// 两条绘制路径都走它，避免同一种 SGR 属性在「背景矩形」与「文本绘制」处
/// 判定不一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CellAttrs {
    /// 粗体（SGR 1）
    pub bold: bool,
    /// 斜体（SGR 3）
    pub italic: bool,
    /// 下划线（SGR 4 及其变体）
    pub underline: bool,
    /// 双下划线（SGR 21）
    pub double_underline: bool,
    /// 删除线（SGR 9）
    pub strikethrough: bool,
    /// 不显示字符但占位（SGR 8，密码输入）
    pub hidden: bool,
    /// 波浪下划线（SGR 4:3，拼写错误提示样式）
    pub undercurl: bool,
}

impl CellAttrs {
    /// 从单元格位标志解析属性。
    pub fn from_cell(cell: &Cell) -> Self {
        Self::from_flags(cell.flags)
    }

    /// 从 `Flags` 直接解析（渲染热路径：避免构造临时 `Cell`）。
    pub fn from_flags(f: Flags) -> Self {
        Self {
            bold: f.contains(Flags::BOLD),
            italic: f.contains(Flags::ITALIC),
            // ALL_UNDERLINES 覆盖单/双/波浪/点线/虚线五种下划线，
            // 只查 UNDERLINE 会让 `ESC[4:3m` 之类的变体整条丢掉。
            underline: f.intersects(Flags::ALL_UNDERLINES),
            double_underline: f.contains(Flags::DOUBLE_UNDERLINE),
            strikethrough: f.contains(Flags::STRIKEOUT),
            hidden: f.contains(Flags::HIDDEN),
            undercurl: f.contains(Flags::UNDERCURL),
        }
    }

    /// 打包为 [`Visual`] 用的位标志。
    ///
    /// **不含**「宽字符」标志：分段时靠 `char_advance` 现算（见 `paint`），
    /// 位标志里再存一份就是两个真相来源。
    #[inline]
    pub fn bits(self) -> u8 {
        let mut b = 0u8;
        if self.bold {
            b |= bit::BOLD;
        }
        if self.italic {
            b |= bit::ITALIC;
        }
        if self.underline {
            b |= bit::UNDERLINE;
        }
        if self.double_underline {
            b |= bit::DOUBLE_UNDERLINE;
        }
        if self.strikethrough {
            b |= bit::STRIKETHROUGH;
        }
        if self.hidden {
            b |= bit::HIDDEN;
        }
        if self.undercurl {
            b |= bit::UNDERCURL;
        }
        b
    }
}

/// 单元格应当显示的字符。
///
/// - `HIDDEN`（SGR 8）统一返回空格：密码等场景**绝不能**把内容画到屏幕上；
/// - `'\0'`（alacritty 的空位）归一为空格，避免把 NUL 送去字体整形。
pub fn cell_char(cell: &Cell) -> char {
    if cell.flags.contains(Flags::HIDDEN) {
        return ' ';
    }
    if cell.c == '\0' { ' ' } else { cell.c }
}

/// 单个字符的装饰（下划线 / 删除线）。`Default` = 无装饰。
#[derive(Clone, Debug, Default)]
pub struct CharDecoration {
    /// 下划线样式（含厚度与波浪标记）
    pub underline: Option<UnderlineStyle>,
    /// 删除线样式
    pub strikethrough: Option<StrikethroughStyle>,
}

/// 光标本帧是否绘制（纯函数，便于单测）。
///
/// 三个条件**缺一不可**：
/// 1. `show_cursor`：终端要求显示（`ESC[?25h` / `ESC[?25l`，vim、less 会隐藏）；
/// 2. 形状不是 `Hidden`（DECSCUSR 的 `CSI 0 q` 一类）；
/// 3. 远端未要求闪烁，或当前相位处于「亮」。
///
/// 旧实现无条件画方块 —— vim 隐藏光标时屏幕上仍挂着一个方块。
pub fn cursor_visible(
    show_cursor: bool,
    shape: CursorShape,
    blinking: bool,
    blink_on: bool,
) -> bool {
    show_cursor && shape != CursorShape::Hidden && (!blinking || blink_on)
}

/// 光标绘制参数：视图算好「可见性 + 形状」，渲染层只负责画。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorPaint {
    /// 形状（DECSCUSR `CSI n q`：块 / 下划线 / 竖线 / 空心块 / 隐藏）
    pub shape: CursorShape,
    /// 本帧是否绘制（已含 `ESC[?25h/l`、闪烁相位、焦点三者的综合判定）
    pub visible: bool,
}

impl Default for CursorPaint {
    fn default() -> Self {
        Self {
            shape: CursorShape::Block,
            visible: true,
        }
    }
}

/// 一处搜索命中的单元格区间（视口行 + 列闭区间）。
///
/// 由视图（`view.rs`）在 paint 之前算好并传进来，渲染器把它画在**字形之前**：
/// 与单元格底色同层，随后的字形绘制自然盖在上面，命中的文字因此保持可读。
/// 旧实现是在整块终端画完之后再叠一层半透明色块，色块压在字形上，
/// 命中的文字被糊住（2026-09-16 用户报「匹配到的文字被遮蔽」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaintSpan {
    /// 视口行（0 = 可视区首行）
    pub row: usize,
    /// 起始列（含）
    pub col_start: usize,
    /// 结束列（含）
    pub col_end: usize,
}

/// Background rectangle to paint.
///
/// Represents a rectangular region with a solid color background.
#[derive(Debug, Clone)]
pub struct BackgroundRect {
    /// Starting column position
    pub start_col: usize,

    /// Ending column position (exclusive)
    pub end_col: usize,

    /// Row position
    pub row: usize,

    /// Background color
    pub color: Hsla,
}

impl BackgroundRect {
    /// Check if this rectangle can be merged with another.
    ///
    /// Two rectangles can be merged if they:
    /// - Are on the same row
    /// - Have the same color
    /// - Are horizontally adjacent
    fn can_merge_with(&self, other: &Self) -> bool {
        self.row == other.row && self.color == other.color && self.end_col == other.start_col
    }
}

/// 一行的最大列数上限（防御性分配：终端列数不会超过这个量级，
/// 但 `FrameScratch` 的容量来自第一帧，必须先夹一下再 `resize`）。
const MAX_SCRATCH_COLUMNS: usize = 8192;

/// 单个单元格的解析结果（颜色 + 属性 + 显示字符 + 分段权重）。
///
/// **热路径核心结构**：一行只出现一份，背景 / box-drawing / 文本三段共用，
/// 于是 `Flags` 位测试与调色板解析在每帧每单元格只做一次。
///
/// `Copy` 且无指针：整行 `Vec<Visual>` 的 `extend_from_slice` 就是一次 `memcpy`。
#[derive(Debug, Clone, Copy)]
struct Visual {
    /// 要绘制的字符（`HIDDEN` / `'\0'` 已归一为空格）
    ch: char,
    /// 解析后的前景色
    fg: Hsla,
    /// 解析后的背景色
    bg: Hsla,
    /// [`bit`] 位标志
    bits: u8,
    /// 连续横向 box-drawing 权重（`get_horizontal_weight`）。
    ///
    /// 打包成 `u8` 而不是 `Option<LineWeight>`：`Visual` 因此保持 `Copy`
    /// 且无指针，同时省掉文本分段判定里对 `get_box_segments` 的重复查表。
    /// `u8::MAX` = 无横向线段；0/1/2 = Light/Heavy/Double。
    hweight: u8,
    /// 该单元格是否带零宽组合记号（`é` 的 U+0301 之类）。
    ///
    /// 只有为真时才回读网格取记号 —— 热路径上不值得为它扩大 `Visual`。
    has_zw: bool,
}

/// `Visual::hweight` 的哨兵值：该单元格不是「连续横向线段」。
const HWEIGHT_NONE: u8 = u8::MAX;

/// box-drawing 字符的宽度按 1 列处理（终端里它们永远是半角）。
#[inline]
fn pack_hweight(ch: char) -> u8 {
    match box_drawing::get_horizontal_weight(ch) {
        Some(box_drawing::LineWeight::Light) => 0,
        Some(box_drawing::LineWeight::Heavy) => 1,
        Some(box_drawing::LineWeight::Double) => 2,
        None => HWEIGHT_NONE,
    }
}

/// 把打包后的权重还原成 [`box_drawing::LineWeight`]。
#[inline]
fn unpack_hweight(v: u8) -> Option<box_drawing::LineWeight> {
    match v {
        0 => Some(box_drawing::LineWeight::Light),
        1 => Some(box_drawing::LineWeight::Heavy),
        2 => Some(box_drawing::LineWeight::Double),
        _ => None,
    }
}

/// 跨帧复用的渲染暂存区。
///
/// **方案：每帧零分配。** 旧实现每帧每行 `Vec<(usize, Cell)>`（克隆整行单元格）
/// 再 `layout_row` 两遍、最后丢掉一半结果；200×50 的窗口每帧克隆 10 000 个
/// `Cell`（每个 `Cell` 带 `Arc` 克隆）。现在整帧只有这三个缓冲区，
/// 且容量只增不减、`clear()` 不释放。
#[derive(Debug, Default)]
pub struct FrameScratch {
    /// 本行解析结果（长度 = 列数）
    visuals: Vec<Visual>,
    /// 「该列已由横向 box-drawing 线段画过」标志（长度 = 列数）
    flash: Vec<bool>,
    /// 文本分段拼装缓冲（每段清零复用）
    text: String,
}

impl FrameScratch {
    /// 清空并保留容量（每帧开头调用一次）。
    pub fn clear(&mut self) {
        self.text.clear();
    }

    /// 暂存区当前列容量（仅用于测试/诊断；稳态下等于首帧的网格列数）。
    pub fn capacity(&self) -> usize {
        self.visuals.capacity()
    }

    /// 按列数准备 `visuals` / `flash`，并在同一函数里借出（借用天然不重叠）。
    ///
    /// 只在长度不够时才 `resize`：稳态每帧的调用因此退化为一次长度比较 +
    /// 一次 `text` 清零，**一次分配都没有**。
    #[inline]
    fn prepare(&mut self, cols: usize) -> (&mut [Visual], &mut [bool]) {
        let cols = cols.min(MAX_SCRATCH_COLUMNS);
        if self.visuals.len() != cols {
            self.visuals.resize(
                cols,
                Visual {
                    ch: ' ',
                    fg: Hsla::default(),
                    bg: Hsla::default(),
                    bits: 0,
                    hweight: HWEIGHT_NONE,
                    has_zw: false,
                },
            );
        }
        if self.flash.len() != cols {
            self.flash.resize(cols, false);
        }
        let (visuals, _) = self.visuals.split_at_mut(cols);
        let (flash, _) = self.flash.split_at_mut(cols);
        (visuals, flash)
    }
}

/// 字符占几列。
///
/// 终端网格把东亚宽字符与 emoji 记为 2 列（alacritty 会在其后补一个
/// `WIDE_CHAR_SPACER`）。批量文本绘制必须知道每个字形推进几格：
/// 窄字形用 `force_width = cell_width` 钉到一列，宽字形用 `2 × cell_width`
/// 钉到两列 —— 混在一起传同一个 `cell_width` 会让宽字符只推进一格、
/// 整段文本从宽字符处开始左偏。
///
/// 零宽字符（组合记号、变体选择符）返回 0：它们跟随前一个基字形，
/// 不该独立成段，否则会被 `force_width` 钉到下一列。
#[inline]
fn char_advance(ch: char) -> u8 {
    use unicode_width::UnicodeWidthChar;
    match ch.width() {
        Some(2) => 2,
        None => 0,
        _ => 1,
    }
}

/// 由网格回读某单元格的零宽组合记号，追加到 `out`。
///
/// alacritty 把 `é`（`e` + U+0301）这类组合序列拆成「基字符 + `extra.zerowidth`」，
/// 旧实现逐字符绘制时这些记号被整条丢掉。批量绘制把它们拼回基字符之后，
/// 整形器就能组合出正确的字形。
///
/// `Visual` 只保存 `has_zw` 标志，真值在需要时回读 —— 组合记号极少出现，
/// 热路径上不值得为它扩大 `Visual`。
#[inline]
fn push_zero_width_marks(term: &Term<GpuiEventProxy>, line: Line, col: usize, out: &mut String) {
    let grid = term.grid();
    if let Some(marks) = grid[line][Column(col)].zerowidth() {
        for m in marks {
            out.push(*m);
        }
    }
}

/// Terminal renderer with font settings and cell dimensions.
///
/// This struct manages the rendering of terminal content, including text,
/// backgrounds, and cursor. It maintains font metrics and provides the
/// [`paint`](Self::paint) method for drawing the terminal grid.
///
/// # Font Metrics
///
/// Cell dimensions are calculated from actual font measurements via
/// [`measure_cell`](Self::measure_cell). This ensures accurate character
/// positioning regardless of the font used.
///
/// # Usage
///
/// The renderer is typically used internally by [`TerminalView`](crate::TerminalView),
/// but can also be used directly for custom rendering:
///
/// ```ignore
/// // Measure cell dimensions (call once per font change)
/// renderer.measure_cell(window);
///
/// // Paint the terminal grid
/// renderer.paint(bounds, padding, &term, cursor, &search_spans, &mut scratch, window, cx);
/// ```
///
/// # Performance
///
/// For optimal performance:
/// - Call `measure_cell` only when font settings change
/// - Keep one [`FrameScratch`] per terminal view and reuse it across frames
/// - The `paint` method is designed to be called every frame
#[derive(Clone)]
pub struct TerminalRenderer {
    /// Font family name (e.g., "Fira Code", "Menlo")
    pub font_family: String,

    /// Font size in pixels
    pub font_size: Pixels,

    /// Width of a single character cell
    pub cell_width: Pixels,

    /// Height of a single character cell (line height)
    pub cell_height: Pixels,

    /// Multiplier for line height to accommodate tall glyphs
    pub line_height_multiplier: f32,

    /// Color palette for resolving terminal colors
    pub palette: ColorPalette,
}

impl TerminalRenderer {
    /// Creates a new terminal renderer with the given font settings and color palette.
    ///
    /// # Arguments
    ///
    /// * `font_family` - The name of the font family to use
    /// * `font_size` - The font size in pixels
    /// * `line_height_multiplier` - Multiplier for line height (e.g., 1.2 for 20% extra)
    /// * `palette` - The color palette to use for terminal colors
    ///
    /// # Returns
    ///
    /// A new `TerminalRenderer` instance with default cell dimensions.
    ///
    /// # Examples
    ///
    /// ```
    /// use gpui::px;
    /// use rustshell_terminal::render::TerminalRenderer;
    /// use rustshell_terminal::ColorPalette;
    ///
    /// let renderer = TerminalRenderer::new("Fira Code".to_string(), px(14.0), 1.0, ColorPalette::default());
    /// ```
    pub fn new(
        font_family: String,
        font_size: Pixels,
        line_height_multiplier: f32,
        palette: ColorPalette,
    ) -> Self {
        // Default cell dimensions - will be measured on first paint
        // Using 0.6 as approximate em-width ratio for monospace fonts
        let cell_width = font_size * 0.6;
        let cell_height = font_size * 1.4; // Line height with some spacing

        Self {
            font_family,
            font_size,
            cell_width,
            cell_height,
            line_height_multiplier,
            palette,
        }
    }

    /// Measure cell dimensions based on actual font metrics.
    ///
    /// This method measures the actual width and height of characters
    /// using the GPUI text system. It uses the '│' (BOX DRAWINGS LIGHT VERTICAL)
    /// character which spans the full cell height in properly designed terminal fonts.
    ///
    /// # Arguments
    ///
    /// * `window` - The GPUI window for text system access
    pub fn measure_cell(&mut self, window: &mut Window) {
        // ⚠️ 必须与 `paint` 用**同一个**字体（family + weight + style + fallbacks），
        // 否则一旦哪天它改用 `.bold()` 之类，`FontId` 就变了、缓存键也对不上。
        let font = self.primary_font();
        let probe = "│";

        let text_run = TextRun {
            len: probe.len(),
            font,
            color: gpui::black(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };

        let shaped =
            window
                .text_system()
                .shape_line(probe.into(), self.font_size, &[text_run], None);

        // Get the width from the shaped line.
        //
        // 不做像素取整：文本分段的单元格对齐由 `force_width = cell_width`
        // 保证（见 `paint_segment`），取整反而会让「实测列宽」与「字形步进」
        // 用上两个略微不同的值。
        if shaped.width > px(0.0) {
            self.cell_width = shaped.width;
        }

        // Calculate height from ascent + descent with optional multiplier.
        // 先各自取整再相加，避免 ascent/descent 的小数部分把行高抬到下一像素。
        let line_height = shaped.ascent.ceil() + shaped.descent.ceil();
        if line_height > px(0.0) {
            self.cell_height = line_height * self.line_height_multiplier;
        }
    }

    /// 单元格最终前景/背景色：解析到主题调色板，再按 DIM / INVERSE 调整。
    ///
    /// **颜色语义的唯一收口**：`layout_row`、背景矩形与文本绘制都调用它。
    /// 此前两条路径各自 `palette.resolve`，新增属性必须改两遍，
    /// 漏一处就出现「背景反了、文字没反」的割裂。
    pub fn cell_colors(&self, cell: &Cell, colors: &Colors) -> (Hsla, Hsla) {
        self.colors_for(cell.fg, cell.bg, cell.flags, colors)
    }

    /// [`cell_colors`](Self::cell_colors) 的 `flags` 直通版本（热路径用）。
    #[inline]
    fn colors_for(&self, fg: Color, bg: Color, flags: Flags, colors: &Colors) -> (Hsla, Hsla) {
        let mut fg = self.palette.resolve(fg, colors);
        let mut bg = self.palette.resolve(bg, colors);

        // DIM（SGR 2）：前景降亮。系数与 colors.rs 的 DimForeground 命名色一致，
        // 让「dim 命名色」与「dim 属性」两种来源观感统一。
        if flags.contains(Flags::DIM) {
            fg.l *= 0.7;
        }

        // INVERSE（SGR 7）：前景/背景互换。
        // `ls --color` 的 setuid/粘滞位标记、htop 状态栏、vim 可视模式都靠它；
        // 此前完全不处理，这些界面会整片错色。
        if flags.contains(Flags::INVERSE) {
            std::mem::swap(&mut fg, &mut bg);
        }

        (fg, bg)
    }

    /// 基础字体（不含粗体/斜体），`measure_cell` 与文本绘制共用同一构造。
    fn primary_font(&self) -> Font {
        Font {
            family: self.font_family.clone().into(),
            features: FontFeatures::default(),
            fallbacks: None,
            weight: FontWeight::NORMAL,
            style: FontStyle::Normal,
        }
    }

    /// 当前字体（含粗体/斜体），三处文本绘制共用。
    fn font_for(&self, bold: bool, italic: bool) -> Font {
        // 常见情况（非粗非斜）直接复用基础字体构造，省两次 `SharedString` 构造
        if !bold && !italic {
            return self.primary_font();
        }
        Font {
            family: self.font_family.clone().into(),
            features: FontFeatures::default(),
            fallbacks: None,
            weight: if bold {
                FontWeight::BOLD
            } else {
                FontWeight::NORMAL
            },
            style: if italic {
                FontStyle::Italic
            } else {
                FontStyle::Normal
            },
        }
    }

    /// 由位标志构造**单字符** TextRun（块光标下的反色重绘用）。
    ///
    /// 文本批量绘制走 `paint_segment`；这条路径只在「光标压在字符上」时
    /// 每帧调用至多一次，不再承担逐字符绘制。
    fn decoration_for(&self, bits: u8, fg: Hsla) -> CharDecoration {
        CharDecoration {
            // 点线/虚线在 gpui 的 UnderlineStyle 里没有对应样式，退化为普通
            // 下划线 —— 保留「此处有下划线」的语义，而不是把整条属性丢掉。
            underline: (bits & bit::UNDERLINE != 0).then(|| UnderlineStyle {
                thickness: px(if bits & bit::DOUBLE_UNDERLINE != 0 {
                    2.0
                } else {
                    1.0
                }),
                color: Some(fg),
                wavy: bits & bit::UNDERCURL != 0,
            }),
            strikethrough: (bits & bit::STRIKETHROUGH != 0).then(|| StrikethroughStyle {
                thickness: px(1.0),
                color: Some(fg),
            }),
        }
    }

    /// 在指定位置绘制单个字符（仅块光标下的反色重绘使用）。
    #[allow(clippy::too_many_arguments)]
    fn paint_char_at(
        &self,
        ch: char,
        bits: u8,
        x: Pixels,
        y: Pixels,
        color: Hsla,
        window: &mut Window,
        cx: &mut App,
    ) {
        let font = self.font_for(bits & bit::BOLD != 0, bits & bit::ITALIC != 0);
        let decoration = self.decoration_for(bits, color);
        let text: gpui::SharedString = ch.to_string().into();
        let len = text.len();
        let run = TextRun {
            len,
            font,
            color,
            background_color: None,
            underline: decoration.underline,
            strikethrough: decoration.strikethrough,
        };
        // `force_width = cell_width`：与批量文本分段同一套单元格对齐规则
        let shaped =
            window
                .text_system()
                .shape_line(text, self.font_size, &[run], Some(self.cell_width));
        let _ = shaped.paint(
            Point { x, y },
            self.cell_height,
            TextAlign::Left,
            None,
            window,
            cx,
        );
    }

    /// Layout cells into batched text runs and background rects for a single row.
    ///
    /// This method processes a row of terminal cells and groups adjacent cells
    /// with identical styling into batched runs. It also collects background
    /// rectangles that need to be painted.
    ///
    /// Both `backgrounds` and `text_runs` are returned **contiguous and mutually
    /// disjoint**: a background rect never spans a `WIDE_CHAR_SPACER` (which keeps a
    /// stale cell), and a text run content string always has exactly
    /// `end_col - start_col` columns. That is what makes the batched paint path in
    /// [`paint`](Self::paint) able to draw each run at a single x offset with
    /// `force_width` cell alignment.
    ///
    /// # Arguments
    ///
    /// * `row` - The row number
    /// * `cells` - Iterator over (column, Cell) pairs
    /// * `colors` - Terminal color configuration
    ///
    /// # Returns
    ///
    /// A tuple of `(backgrounds, text_runs)`.
    pub fn layout_row(
        &self,
        row: usize,
        cells: impl Iterator<Item = (usize, Cell)>,
        colors: &Colors,
    ) -> (Vec<BackgroundRect>, Vec<BatchedTextRun>) {
        let mut backgrounds: Vec<BackgroundRect> = Vec::with_capacity(16);
        let mut text_runs: Vec<BatchedTextRun> = Vec::with_capacity(8);

        let mut run_text = String::new();
        let mut run_start = 0usize;
        let mut run_bits = 0u8;
        let mut run_fg = Hsla::default();
        let mut run_bg = Hsla::default();

        for (col, cell) in cells {
            let (fg, bg) = self.cell_colors(&cell, colors);
            let bits = CellAttrs::from_cell(&cell).bits();

            // 每格先各出一块背景，循环结束后统一合并 —— 「合并相邻同色块」
            // 的规则有单测锁死，这里只负责喂原始数据。
            // `WIDE_CHAR_SPACER` 也照出：它自己有底色，漏掉会让宽字符右半边
            // 与左半边异色时少刷一块。
            backgrounds.push(BackgroundRect {
                start_col: col,
                end_col: col + 1,
                row,
                color: bg,
            });

            // 宽字符占位格没有自己的字形，但它**占一列**：必须补一个空格段，
            // 否则后续文本的列跨度对不上（整段会被后续字形的自然步进顶偏），
            // 并且会与宽字符挤进同一段而丢掉列宽。
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                if !run_text.is_empty() {
                    text_runs.push(BatchedTextRun {
                        text: std::mem::take(&mut run_text),
                        start_col: run_start,
                        row,
                        fg_color: run_fg,
                        bg_color: run_bg,
                        bold: run_bits & bit::BOLD != 0,
                        italic: run_bits & bit::ITALIC != 0,
                        underline: run_bits & bit::UNDERLINE != 0,
                        double_underline: run_bits & bit::DOUBLE_UNDERLINE != 0,
                        strikethrough: run_bits & bit::STRIKETHROUGH != 0,
                    });
                }
                if cell_char(&cell) == ' ' {
                    // 占位格用**它自己的**属性出段：列宽与字体属性都不丢。
                    text_runs.push(BatchedTextRun {
                        text: " ".to_string(),
                        start_col: col,
                        row,
                        fg_color: fg,
                        bg_color: bg,
                        bold: bits & bit::BOLD != 0,
                        italic: bits & bit::ITALIC != 0,
                        underline: bits & bit::UNDERLINE != 0,
                        double_underline: bits & bit::DOUBLE_UNDERLINE != 0,
                        strikethrough: bits & bit::STRIKETHROUGH != 0,
                    });
                }
                run_start = col + 1;
                continue;
            }

            let ch = cell_char(&cell);

            // 属性或列连续性断了 → 收口当前段
            let contiguous = run_text.is_empty() || run_start + run_text.chars().count() == col;
            if !run_text.is_empty() && (bits != run_bits || fg != run_fg || !contiguous) {
                text_runs.push(BatchedTextRun {
                    text: std::mem::take(&mut run_text),
                    start_col: run_start,
                    row,
                    fg_color: run_fg,
                    bg_color: run_bg,
                    bold: run_bits & bit::BOLD != 0,
                    italic: run_bits & bit::ITALIC != 0,
                    underline: run_bits & bit::UNDERLINE != 0,
                    double_underline: run_bits & bit::DOUBLE_UNDERLINE != 0,
                    strikethrough: run_bits & bit::STRIKETHROUGH != 0,
                });
            }
            if run_text.is_empty() {
                run_start = col;
                run_bits = bits;
                run_fg = fg;
                run_bg = bg;
            }
            run_text.push(ch);
        }

        if !run_text.is_empty() {
            text_runs.push(BatchedTextRun {
                text: run_text,
                start_col: run_start,
                row,
                fg_color: run_fg,
                bg_color: run_bg,
                bold: run_bits & bit::BOLD != 0,
                italic: run_bits & bit::ITALIC != 0,
                underline: run_bits & bit::UNDERLINE != 0,
                double_underline: run_bits & bit::DOUBLE_UNDERLINE != 0,
                strikethrough: run_bits & bit::STRIKETHROUGH != 0,
            });
        }

        let backgrounds = self.merge_backgrounds(backgrounds);

        (backgrounds, text_runs)
    }

    /// Merge adjacent background rects with same color.
    ///
    /// This optimization reduces the number of rectangles to paint by
    /// combining horizontally adjacent rectangles that share the same color.
    ///
    /// # Arguments
    ///
    /// * `rects` - Vector of background rectangles to merge
    ///
    /// # Returns
    ///
    /// A new vector with merged rectangles
    fn merge_backgrounds(&self, mut rects: Vec<BackgroundRect>) -> Vec<BackgroundRect> {
        if rects.is_empty() {
            return rects;
        }

        // 顺序扫描 + `into_iter`：旧实现用 `remove(0)` 取首元素，
        // 每行都要把整个 Vec 前移一遍（O(n²) 的隐性来源）。
        let mut merged: Vec<BackgroundRect> = Vec::with_capacity(rects.len());
        let mut iter = rects.drain(..);
        let mut current = iter.next().expect("rects 非空");

        for rect in iter {
            if current.can_merge_with(&rect) {
                current.end_col = rect.end_col;
            } else {
                merged.push(current);
                current = rect;
            }
        }

        merged.push(current);
        merged
    }

    /// 把整行单元格解析进 `visuals`（每格一次颜色/属性/bits 计算）。
    ///
    /// `out` 的长度已由 [`FrameScratch::prepare`] 对齐到网格列数；
    /// 万一网格比暂存区窄（极端防御），多出来的格子按「空背景 + 空格」处理，
    /// 否则会把上一帧的残留内容画出来。
    #[inline]
    fn fill_visuals(
        &self,
        term: &Term<GpuiEventProxy>,
        line: Line,
        colors: &Colors,
        out: &mut [Visual],
    ) {
        let grid = term.grid();
        let row = &grid[line];
        let n = out.len().min(row.len());
        for col in 0..n {
            let cell = &row[Column(col)];
            let flags = cell.flags;
            let (fg, bg) = self.colors_for(cell.fg, cell.bg, flags, colors);
            let bits = CellAttrs::from_flags(flags).bits();
            // HIDDEN（SGR 8）绝不能把内容画到屏幕上，`'\0'`（空位）也只是空格
            let ch = if bits & bit::HIDDEN != 0 || cell.c == '\0' {
                ' '
            } else {
                cell.c
            };
            out[col] = Visual {
                ch,
                fg,
                bg,
                bits,
                hweight: pack_hweight(ch),
                // 组合记号只在真的有的时候才回读网格
                has_zw: cell.extra.is_some(),
            };
        }
        for slot in out.iter_mut().skip(n) {
            *slot = Visual {
                ch: ' ',
                fg: Hsla::default(),
                bg: Hsla::default(),
                bits: 0,
                hweight: HWEIGHT_NONE,
                has_zw: false,
            };
        }
    }

    /// Paint terminal content to the window.
    ///
    /// This is the main rendering method that draws the terminal grid,
    /// including backgrounds, text, and cursor.
    ///
    /// # Arguments
    ///
    /// * `bounds` - The bounding box to render within
    /// * `padding` - Padding around the terminal content
    /// * `term` - The terminal state
    /// * `cursor` - 光标绘制参数（形状 + 本帧可见性），由视图综合判定
    /// * `search_spans` - 搜索命中的单元格区间（视口行 + 列闭合区间）；画在**字形之前**
    /// * `preedit` - 输入法预编辑串（组合中、尚未上屏）；画在光标处，空串即不画
    /// * `scratch` - 跨帧复用的暂存区（见 [`FrameScratch`]）
    /// * `window` - The GPUI window
    /// * `cx` - The application context
    ///
    /// 形参 9 个（不含 `&self`），每个都是独立语义、没有天然聚合体，
    /// 故与同文件的 `paint_char_at` 一样显式豁免该 lint。
    #[allow(clippy::too_many_arguments)]
    pub fn paint(
        &self,
        bounds: Bounds<Pixels>,
        padding: Edges<Pixels>,
        term: &Term<GpuiEventProxy>,
        cursor: CursorPaint,
        search_spans: &[PaintSpan],
        preedit: &str,
        scratch: &mut FrameScratch,
        window: &mut Window,
        cx: &mut App,
    ) {
        // Get terminal dimensions
        let grid = term.grid();
        let num_lines = grid.screen_lines();
        let num_cols = grid.columns();
        let colors = term.colors();

        scratch.clear();
        // 先把文本拼装缓冲「借」到栈上（容量跨帧保留，收尾时放回）。
        // 必须在 `prepare` 之前拿走：`prepare` 会长期可变借用 `scratch` 的
        // visuals/flash 两个字段，之后再碰 `scratch.text` 借用检查器不认。
        let mut text_buf = std::mem::take(&mut scratch.text);
        // 两把借用来自同一次 `split_at_mut`，借用检查器认可它们互不重叠。
        let (visuals, flash) = scratch.prepare(num_cols);
        let ncols = visuals.len();

        // 历史滚回偏移（0 = 视口贴在实时屏底）。
        //
        // ⚠️ alacritty 的 `Line` 是**视口坐标**：`Line(0)` = 当前视口首行，负值向上进入
        // 历史（实测：3 行屏写入 A..F 后 `Line(-2..=2)` 分别为 C D E F ' '）。
        // 所以绘制视口第 i 行必须取 `Line(i - display_offset)`。
        // 漏减 offset 时，滚轮只改 `display_offset` 而画面纹丝不动 —— 这正是
        // 「终端内容无法滚动」的根因（2026-09-14 实测定位）。
        let display_offset = grid.display_offset() as i32;

        // Calculate default background color
        let default_bg = self.palette.resolve(
            Color::Named(alacritty_terminal::vte::ansi::NamedColor::Background),
            colors,
        );

        // Paint default background (covers full bounds including padding)
        window.paint_quad(solid_quad(bounds, default_bg));

        // 零列网格：后面所有逐列循环都以 `visuals[0]` 为起点，必须先挡在这里
        if ncols == 0 {
            text_buf.clear();
            let _ = std::mem::replace(&mut scratch.text, text_buf);
            return;
        }

        // Calculate origin offset (content starts after padding)
        let origin = Point {
            x: bounds.origin.x + padding.left,
            y: bounds.origin.y + padding.top,
        };

        // 文本基线的垂直校正：行高比「ascent+descent」多出来的部分上下均分。
        // 每帧算一次，而不是每个单元格算两次。
        let base_height = self.cell_height / self.line_height_multiplier;
        let vertical_offset = (self.cell_height - base_height) / 2.0;

        let cw: f32 = self.cell_width.into();
        let ch: f32 = self.cell_height.into();
        let ox: f32 = origin.x.into();
        let oy: f32 = origin.y.into();

        // 搜索命中区间：`view.rs` 已按行升序构建，用一个前移游标取代
        // 旧实现的 `search_spans.iter().filter(...)`（那是「每行扫全部命中」的 O(rows×hits)）。
        let mut span_cursor = 0usize;

        // Iterate over visible lines（视口行 i ↔ Line(i - display_offset)）
        for line_idx in 0..num_lines {
            let line = viewport_line(line_idx, display_offset);

            // ---------- 1. 解析本行（每格一次） ----------
            self.fill_visuals(term, line, colors, visuals);

            let y_base = px(oy + ch * (line_idx as f32));
            let cy = px(oy + ch * (line_idx as f32) + ch / 2.0);

            // ---------- 2. 背景（单次流式合并，零分配） ----------
            let mut cur_start = 0usize;
            let mut cur_color = visuals[0].bg;
            for (col, v) in visuals.iter().enumerate().skip(1) {
                let bg = v.bg;
                if bg != cur_color {
                    Self::paint_bg_run(
                        window, cur_color, default_bg, ox, y_base, cw, ch, cur_start, col,
                    );
                    cur_start = col;
                    cur_color = bg;
                }
            }
            Self::paint_bg_run(
                window, cur_color, default_bg, ox, y_base, cw, ch, cur_start, ncols,
            );

            // ---------- 3. 搜索命中底色（与单元格底色同层，画在字形之前） ----------
            // 这样命中的文字仍由紧随其后的字形绘制覆盖，清晰可读。
            while span_cursor < search_spans.len() && search_spans[span_cursor].row < line_idx {
                span_cursor += 1;
            }
            let mut sc = span_cursor;
            while sc < search_spans.len() && search_spans[sc].row == line_idx {
                let span = search_spans[sc];
                let col_start = span.col_start.min(ncols);
                let col_end = (span.col_end + 1).min(ncols);
                if col_end > col_start {
                    window.paint_quad(solid_quad(
                        Bounds {
                            origin: Point {
                                x: px(ox + cw * col_start as f32),
                                y: y_base,
                            },
                            size: Size {
                                width: px(cw * (col_end - col_start) as f32),
                                height: self.cell_height,
                            },
                        },
                        self.palette.search(),
                    ));
                }
                sc += 1;
            }

            // ---------- 4. box-drawing 横向线段 ----------
            // 连续同权重 + 同前景色的一段合成**一条**连续路径，消除格间缝隙；
            // 画过的列写进 `flash`，后续竖直分量只补上下两截。
            let mut col = 0usize;
            while col < ncols {
                let w = visuals[col].hweight;
                if w == HWEIGHT_NONE {
                    col += 1;
                    continue;
                }
                let fg = visuals[col].fg;
                let start = col;
                let mut end = col;
                col += 1;
                while col < ncols {
                    let v = &visuals[col];
                    if v.hweight == w && v.fg == fg {
                        end = col;
                        col += 1;
                    } else {
                        break;
                    }
                }
                let weight = unpack_hweight(w).expect("hweight 非 NONE 时必可还原");
                box_drawing::draw_horizontal_span(
                    px(ox + cw * start as f32),
                    px(ox + cw * (end + 1) as f32),
                    cy,
                    weight,
                    self.cell_width,
                    fg,
                    window,
                );
                for f in &mut flash[start..=end] {
                    *f = true;
                }
            }

            // ---------- 5. box-drawing 非横向部分 ----------
            for (idx, v) in visuals.iter().enumerate() {
                if v.hweight != HWEIGHT_NONE {
                    // 横向已画 → 只补竖直分量
                    if flash[idx] {
                        box_drawing::draw_vertical_components(
                            v.ch,
                            Bounds {
                                origin: Point {
                                    x: px(ox + cw * idx as f32),
                                    y: y_base,
                                },
                                size: Size {
                                    width: self.cell_width,
                                    height: self.cell_height,
                                },
                            },
                            v.fg,
                            self.cell_width,
                            window,
                        );
                    } else {
                        box_drawing::draw_box_character(
                            v.ch,
                            Bounds {
                                origin: Point {
                                    x: px(ox + cw * idx as f32),
                                    y: y_base,
                                },
                                size: Size {
                                    width: self.cell_width,
                                    height: self.cell_height,
                                },
                            },
                            v.fg,
                            self.cell_width,
                            window,
                        );
                    }
                }
                // 复位：下一行复用同一块 `flash`，逐格清零不额外分配
                flash[idx] = false;
            }

            // ---------- 6. 文本（按属性 + 字形步进分段，每段一次 shape_line） ----------
            //
            // 一段的约束：同一属性（颜色/粗斜/下划线）、连续列、**且每个字形推进
            // 相同的格数**。最后一条是宽字符的正确性关键 —— `force_width` 会按
            // 「每基字形推进多少」把所有字形钉到等距格线上，宽窄混排会让宽字符
            // 只推进一格，整段从宽字符处开始左偏。故宽字符单独成段、传 2 格宽度，
            // 零宽组合记号则绝不能单独成段。
            let mut seg_start = 0usize;
            loop {
                // 跳过不可用字形绘制的列：box-drawing（走矢量路径）与空列
                while seg_start < ncols
                    && (visuals[seg_start].hweight != HWEIGHT_NONE || visuals[seg_start].ch == ' ')
                {
                    seg_start += 1;
                }
                if seg_start >= ncols {
                    break;
                }
                let bits = visuals[seg_start].bits;
                let fg = visuals[seg_start].fg;
                let adv = char_advance(visuals[seg_start].ch).max(1);
                let mut end = seg_start + 1;
                while end < ncols {
                    let v = &visuals[end];
                    if v.hweight != HWEIGHT_NONE
                        || v.ch == ' '
                        || v.bits != bits
                        || v.fg != fg
                        || char_advance(v.ch).max(1) != adv
                    {
                        break;
                    }
                    end += 1;
                }
                text_buf.clear();
                // 只读借用 `visuals`：拼装完成后借用即结束，随后才落绘制。
                for (i, v) in visuals[seg_start..end].iter().enumerate() {
                    text_buf.push(v.ch);
                    // 零宽组合记号跟随基字形，不能触发 `force_width` 推进
                    if v.has_zw {
                        push_zero_width_marks(term, line, seg_start + i, &mut text_buf);
                    }
                }
                self.paint_segment(
                    // `SharedString::from(&str)` 会做一次拷贝，我们仅需避免
                    // 每段一次 `collect()` 的中间分配。
                    text_buf.as_str(),
                    bits,
                    fg,
                    px(ox + cw * seg_start as f32),
                    y_base + vertical_offset,
                    // 「每个基字形推进多少」= 本段的统一字宽
                    self.cell_width * adv as f32,
                    window,
                    cx,
                );
                seg_start = end;
            }
        }

        // Paint cursor。三个条件缺一不可：
        //   1. 视口贴在实时屏底 —— 光标坐标是**实时屏**坐标（`Line(0)` = 实时屏首行），
        //      视口一旦上翻 `display_offset` 行，照抄 `cursor.line` 会把它画到错行；
        //   2. `cursor.visible` —— 视图已综合 `ESC[?25h/l`（vim/less 会隐藏）、
        //      DECSCUSR 的闪烁标志、闪烁相位与焦点；
        //   3. 形状不为 `Hidden`。
        // 形状来自 DECSCUSR（`CSI n q`）：块 / 下划线 / 竖线 / 空心块。
        if display_offset == 0 && cursor.visible && cursor.shape != CursorShape::Hidden {
            let cursor_point = grid.cursor.point;
            let cursor_x = px(ox + cw * cursor_point.column.0 as f32);
            let cursor_y = px(oy + ch * cursor_point.line.0 as f32);

            let cursor_color = self.palette.resolve(
                Color::Named(alacritty_terminal::vte::ansi::NamedColor::Cursor),
                colors,
            );

            let cursor_bounds = Bounds {
                origin: Point {
                    x: cursor_x,
                    y: cursor_y,
                },
                size: Size {
                    width: self.cell_width,
                    height: self.cell_height,
                },
            };

            match cursor.shape {
                // 块光标：先铺满光标色，再用**该单元格的底色**把字符重画一遍。
                // 不重画的话字符会被色块盖住（这正是很多简易终端「光标处看不到字」的原因）。
                CursorShape::Block => {
                    window.paint_quad(solid_quad(cursor_bounds, cursor_color));
                    let cell = &grid[cursor_point];
                    let ch = cell_char(cell);
                    if ch != ' ' && !box_drawing::is_box_drawing_char(ch) {
                        let (_, bg_color) = self.cell_colors(cell, colors);
                        let bits = CellAttrs::from_cell(cell).bits();
                        self.paint_char_at(
                            ch,
                            bits,
                            cursor_x,
                            cursor_y + vertical_offset,
                            bg_color,
                            window,
                            cx,
                        );
                    }
                }
                // 空心块：只描边不填充，字符保持原色
                CursorShape::HollowBlock => {
                    let border = Edges {
                        top: px(1.0),
                        right: px(1.0),
                        bottom: px(1.0),
                        left: px(1.0),
                    };
                    window.paint_quad(quad(
                        cursor_bounds,
                        px(0.0),
                        transparent_black(),
                        border,
                        cursor_color,
                        Default::default(),
                    ));
                }
                // 竖线：宽取 1/8 单元格，至少 1px（否则小字号下会消失）
                CursorShape::Beam => {
                    let width = px((cw / 8.0).max(1.0));
                    window.paint_quad(solid_quad(
                        Bounds {
                            origin: Point {
                                x: cursor_x,
                                y: cursor_y,
                            },
                            size: Size {
                                width,
                                height: self.cell_height,
                            },
                        },
                        cursor_color,
                    ));
                }
                // 下划线：贴在单元格底部
                CursorShape::Underline => {
                    let height = px((ch / 8.0).max(1.0));
                    window.paint_quad(solid_quad(
                        Bounds {
                            origin: Point {
                                x: cursor_x,
                                y: cursor_y + self.cell_height - height,
                            },
                            size: Size {
                                width: self.cell_width,
                                height,
                            },
                        },
                        cursor_color,
                    ));
                }
                // 已在上方守卫中排除，此处仅为穷尽 match
                CursorShape::Hidden => {}
            }
        }

        // 输入法预编辑串：画在光标处，压在同名位置上（视图已让光标本帧不画）。
        //
        // 条件只有 `display_offset == 0`：这段字属于**实时屏**的光标位置，
        // 视口上翻历史时它既不该跟着历史滚，也不该画到别的行上。
        if display_offset == 0 && !preedit.is_empty() {
            let cursor_point = grid.cursor.point;
            self.paint_preedit(
                preedit,
                self.palette.resolve(
                    Color::Named(alacritty_terminal::vte::ansi::NamedColor::Foreground),
                    colors,
                ),
                default_bg,
                cursor_point.line.0.max(0) as usize,
                cursor_point.column.0,
                num_cols,
                origin,
                window,
                cx,
            );
        }

        // 归还文本缓冲（容量跨帧保留；`mem::swap` 不释放任何分配）
        text_buf.clear();
        let _ = std::mem::replace(&mut scratch.text, text_buf);
    }

    /// 画输入法预编辑串（组合中的字，还没上屏）。
    ///
    /// 视觉上就是「终端里的拼写检查」：先铺一层默认底色把底下的内容让开，
    /// 再用默认前景色写上带下划线的候选字 —— 下划线是**没上屏**的标志
    /// （上屏后 shell 会自己回显一份，那时没有下划线）。
    ///
    /// 起点是光标所在格，向右逐格排；写到右边界就截断（组合串通常只有几个字，
    /// 截掉尾巴也比折到下一行更不容易看错）。按**字宽分段**：一个 CJK 字占 2 格，
    /// 混进 1 格宽的段里会被 `force_width` 挤扁（同 [`Self::paint_segment`] 的规则）。
    #[allow(clippy::too_many_arguments)]
    fn paint_preedit(
        &self,
        preedit: &str,
        fg: Hsla,
        bg: Hsla,
        row: usize,
        col: usize,
        max_cols: usize,
        origin: Point<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) {
        if preedit.is_empty() || max_cols == 0 || col >= max_cols {
            return;
        }
        let cw: f32 = self.cell_width.into();
        // 与 `paint` 里同一套基线校正：行高比字形高出来的部分上下均分
        let base_height = self.cell_height / self.line_height_multiplier;
        let vertical_offset = (self.cell_height - base_height) / 2.0;
        let y = origin.y + self.cell_height * row as f32 + vertical_offset;

        // 先把整段待选字的宽度量出来（超出右边界的不算），一次铺掉底色
        let mut cols_used = 0usize;
        for ch in preedit.chars() {
            let adv = char_advance(ch).max(1) as usize;
            if col + cols_used + adv > max_cols {
                break;
            }
            cols_used += adv;
        }
        if cols_used == 0 {
            return;
        }
        window.paint_quad(solid_quad(
            Bounds {
                origin: Point {
                    x: origin.x + self.cell_width * col as f32,
                    y: origin.y + self.cell_height * row as f32,
                },
                size: Size {
                    width: px(cw * cols_used as f32),
                    height: self.cell_height,
                },
            },
            bg,
        ));

        // 按字宽分段画：`paint_segment` 的 `force_width` 是整段统一的
        let mut seg = String::new();
        let mut seg_col = col;
        let mut seg_adv = 0usize;
        let mut cur_col = col;
        for ch in preedit.chars() {
            let adv = char_advance(ch).max(1) as usize;
            if cur_col + adv > max_cols {
                break;
            }
            if seg.is_empty() {
                seg_col = cur_col;
                seg_adv = adv;
            } else if adv != seg_adv {
                self.paint_segment(
                    &seg,
                    bit::UNDERLINE,
                    fg,
                    origin.x + self.cell_width * seg_col as f32,
                    y,
                    px(cw * seg_adv as f32),
                    window,
                    cx,
                );
                seg.clear();
                seg_col = cur_col;
                seg_adv = adv;
            }
            seg.push(ch);
            cur_col += adv;
        }
        if !seg.is_empty() {
            self.paint_segment(
                &seg,
                bit::UNDERLINE,
                fg,
                origin.x + self.cell_width * seg_col as f32,
                y,
                px(cw * seg_adv as f32),
                window,
                cx,
            );
        }
    }

    /// 画一段背景：默认底色跳过（整块 bounds 已铺垫），其余只画非透明段。
    #[allow(clippy::too_many_arguments)]
    #[inline]
    fn paint_bg_run(
        window: &mut Window,
        color: Hsla,
        default_bg: Hsla,
        ox: f32,
        y: Pixels,
        cw: f32,
        ch: f32,
        start_col: usize,
        end_col: usize,
    ) {
        if end_col <= start_col || color == default_bg {
            return;
        }
        window.paint_quad(solid_quad(
            Bounds {
                origin: Point {
                    x: px(ox + cw * start_col as f32),
                    y,
                },
                size: Size {
                    width: px(cw * (end_col - start_col) as f32),
                    height: px(ch),
                },
            },
            color,
        ));
    }

    /// 批量绘制一段同属性文本：**一次** `shape_line` + 一次 `paint`。
    ///
    /// `force_width = cell_width` 让 GPUI 把每个基字形钉到自己的格子上
    /// （见 gpui `apply_force_width_to_layout`），因此分段批量绘制不会
    /// 出现逐字步进累计偏移，也不会因字体回退而错列。
    #[allow(clippy::too_many_arguments)]
    fn paint_segment(
        &self,
        text: &str,
        bits: u8,
        fg: Hsla,
        x: Pixels,
        y: Pixels,
        force_width: Pixels,
        window: &mut Window,
        cx: &mut App,
    ) {
        if text.is_empty() {
            return;
        }
        let font = self.font_for(bits & bit::BOLD != 0, bits & bit::ITALIC != 0);
        let decoration = self.decoration_for(bits, fg);
        let text: gpui::SharedString = text.into();
        let len = text.len();
        let run = TextRun {
            len,
            font,
            color: fg,
            background_color: None,
            underline: decoration.underline,
            strikethrough: decoration.strikethrough,
        };
        let shaped =
            window
                .text_system()
                .shape_line(text, self.font_size, &[run], Some(force_width));
        let _ = shaped.paint(
            Point { x, y },
            self.cell_height,
            TextAlign::Left,
            None,
            window,
            cx,
        );
    }
}

/// 视口行号 → alacritty `Line`。
///
/// 这是**滚回功能唯一的坐标转换点**，抽成纯函数以便单测锁死公式。
///
/// alacritty 的 `Line` 是视口坐标：`Line(0)` = 当前视口首行，负值向上进入历史。
/// `display_offset` 表示视口相对实时屏底上翻了多少行，所以：
///
/// ```text
/// 视口第 0 行  ==  Line(-display_offset)
/// 视口第 n 行  ==  Line(n - display_offset)
/// ```
///
/// 漏掉 `- display_offset` 时，滚轮只改内部偏移而**画面不变**，
/// 用户看到的就是「终端内容无法滚动」。
#[inline]
pub fn viewport_line(row: usize, display_offset: i32) -> Line {
    Line(row as i32 - display_offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_renderer_creation() {
        let renderer = TerminalRenderer::new(
            "Fira Code".to_string(),
            px(14.0),
            1.0,
            ColorPalette::default(),
        );
        assert_eq!(renderer.font_family, "Fira Code");
        assert_eq!(renderer.font_size, px(14.0));
        assert_eq!(renderer.line_height_multiplier, 1.0);
    }

    #[test]
    fn test_background_rect_merge() {
        let black = Hsla::black();

        let rect1 = BackgroundRect {
            start_col: 0,
            end_col: 5,
            row: 0,
            color: black,
        };

        let rect2 = BackgroundRect {
            start_col: 5,
            end_col: 10,
            row: 0,
            color: black,
        };

        assert!(rect1.can_merge_with(&rect2));

        let rect3 = BackgroundRect {
            start_col: 5,
            end_col: 10,
            row: 1,
            color: black,
        };

        assert!(!rect1.can_merge_with(&rect3));
    }

    #[test]
    fn test_merge_backgrounds() {
        let renderer = TerminalRenderer::new(
            "monospace".to_string(),
            px(14.0),
            1.0,
            ColorPalette::default(),
        );
        let black = Hsla::black();

        let rects = vec![
            BackgroundRect {
                start_col: 0,
                end_col: 5,
                row: 0,
                color: black,
            },
            BackgroundRect {
                start_col: 5,
                end_col: 10,
                row: 0,
                color: black,
            },
        ];

        let merged = renderer.merge_backgrounds(rects);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].start_col, 0);
        assert_eq!(merged[0].end_col, 10);
    }

    /// 回归护栏：滚回时视口行必须随 `display_offset` 上移。
    ///
    /// 变异的写法是 `Line(row as i32)`（丢掉 `- display_offset`）——此时滚轮
    /// 只改内部偏移、画面恒定不变，即「终端内容无法滚动」。
    #[test]
    fn viewport_line_tracks_display_offset() {
        // 贴底：视口行号即内容行号
        assert_eq!(viewport_line(0, 0), Line(0));
        assert_eq!(viewport_line(7, 0), Line(7));

        // 上翻 3 行：视口首行变成「实时屏上方第 3 行」
        assert_eq!(viewport_line(0, 3), Line(-3));
        assert_eq!(viewport_line(2, 3), Line(-1));
        assert_eq!(viewport_line(3, 3), Line(0));

        // 关键不等式：上翻后一定不能仍等于贴底时的取值
        assert_ne!(viewport_line(0, 3), viewport_line(0, 0));
    }
    // ---------- 单元格属性 / 光标（2026-09-15 本地增强） ----------

    fn test_renderer() -> TerminalRenderer {
        TerminalRenderer::new("monospace".into(), px(14.0), 1.0, ColorPalette::default())
    }

    /// 反显（SGR 7）必须交换前景/背景。
    ///
    /// `ls --color` 的 setuid/粘滞位、htop 状态栏、vim 可视模式都靠它；
    /// 缺失时表现为「这些界面整片错色」。
    #[test]
    fn inverse_flag_swaps_cell_colors() {
        let r = test_renderer();
        let colors = Colors::default();

        let mut cell = Cell {
            c: 'x',
            ..Default::default()
        };
        let plain = r.cell_colors(&cell, &colors);

        cell.flags.insert(Flags::INVERSE);
        let inverted = r.cell_colors(&cell, &colors);

        assert_eq!(plain.0, inverted.1, "反显后前景应等于原背景");
        assert_eq!(plain.1, inverted.0, "反显后背景应等于原前景");
        assert_ne!(plain.0, plain.1, "前提：默认前景与背景本就不同");
    }

    /// DIM（SGR 2）只压暗前景，不动背景。
    #[test]
    fn dim_flag_darkens_foreground_only() {
        let r = test_renderer();
        let colors = Colors::default();

        let mut cell = Cell {
            c: 'x',
            ..Default::default()
        };
        let plain = r.cell_colors(&cell, &colors);

        cell.flags.insert(Flags::DIM);
        let dimmed = r.cell_colors(&cell, &colors);

        assert!(dimmed.0.l < plain.0.l, "前景亮度必须下降");
        assert_eq!(dimmed.1, plain.1, "背景不受 DIM 影响");
    }

    /// HIDDEN（SGR 8）绝不能让字符出现在屏幕上（密码输入）。
    #[test]
    fn hidden_cell_renders_as_space() {
        let mut cell = Cell {
            c: 'p',
            ..Default::default()
        };
        cell.flags.insert(Flags::HIDDEN);
        assert_eq!(cell_char(&cell), ' ');
    }

    /// 空位（`'\0'`）归一为空格，且非隐藏字符原样返回。
    #[test]
    fn empty_cell_normalizes_to_space() {
        let mut cell = Cell {
            c: '\0',
            ..Default::default()
        };
        assert_eq!(cell_char(&cell), ' ');
        cell.c = 'a';
        assert_eq!(cell_char(&cell), 'a');
    }

    /// 下划线变体（波浪 / 双下划线）都必须被认作「有下划线」。
    ///
    /// 只查 `Flags::UNDERLINE` 会让 `ESC[4:3m`（波浪）之类整条丢掉。
    #[test]
    fn cell_attrs_covers_underline_variants() {
        let cases = [
            (Flags::UNDERLINE, false, false),
            (Flags::DOUBLE_UNDERLINE, true, false),
            (Flags::UNDERCURL, false, true),
        ];
        for (flag, double, curl) in cases {
            let mut cell = Cell::default();
            cell.flags.insert(flag);
            let a = CellAttrs::from_cell(&cell);
            assert!(a.underline, "变体 {flag:?} 必须算作有下划线");
            assert_eq!(a.double_underline, double, "双下划线标记 {flag:?}");
            assert_eq!(a.undercurl, curl, "波浪标记 {flag:?}");
        }
    }

    /// 删除线与粗斜体解析（此前 `strikethrough` 恒为 None）。
    #[test]
    fn cell_attrs_reads_bold_italic_strikethrough() {
        let mut cell = Cell::default();
        cell.flags
            .insert(Flags::BOLD | Flags::ITALIC | Flags::STRIKEOUT);
        let a = CellAttrs::from_cell(&cell);
        assert!(a.bold && a.italic && a.strikethrough);
        assert!(!a.hidden);
    }

    /// 光标可见性三条条件：终端显示开关、形状、闪烁相位。
    #[test]
    fn cursor_visibility_requires_all_three_conditions() {
        use CursorShape::{Beam, Block, Hidden};

        // 全部满足 → 可见
        assert!(cursor_visible(true, Block, false, true));
        // 远端要求闪烁时跟随相位
        assert!(cursor_visible(true, Block, true, true));
        assert!(
            !cursor_visible(true, Block, true, false),
            "闪烁暗相位必须隐藏"
        );
        // `ESC[?25l` 隐藏光标（vim / less）
        assert!(
            !cursor_visible(false, Block, false, true),
            "SHOW_CURSOR=false 必须隐藏"
        );
        // DECSCUSR 的 Hidden 形状
        assert!(
            !cursor_visible(true, Hidden, false, true),
            "Hidden 形状必须隐藏"
        );
        // 非块形状只要可见就算可见
        assert!(cursor_visible(true, Beam, false, true));
    }

    /// 光标默认参数：可见 + 块形状（视图未指定时的安全默认）。
    #[test]
    fn cursor_paint_default_is_visible_block() {
        let c = CursorPaint::default();
        assert!(c.visible);
        assert_eq!(c.shape, CursorShape::Block);
    }

    // ---------- 渲染分段 / 暂存区（2026-09 性能改造护栏） ----------

    /// `pack_hweight` / `unpack_hweight` 必须互为逆运算：
    /// 水平连续字符（`─`）可往返，其余一律 `NONE` 且还原为 `None`。
    ///
    /// 注意 `┼`（十字）**有**横向线段，故不在「无横向权重」之列 ——
    /// 它的左右两段都是 Light，`get_horizontal_weight` 会返回 Light。
    #[test]
    fn hweight_roundtrip() {
        use box_drawing::LineWeight;
        for (ch, expect) in [('─', LineWeight::Light), ('━', LineWeight::Heavy)] {
            let packed = pack_hweight(ch);
            assert_ne!(packed, HWEIGHT_NONE, "{ch} 应有横向权重");
            assert_eq!(unpack_hweight(packed), Some(expect));
        }
        for ch in ['│', '┌', 'a', ' ', '\0'] {
            assert_eq!(pack_hweight(ch), HWEIGHT_NONE, "{ch} 不应有横向权重");
            assert_eq!(unpack_hweight(HWEIGHT_NONE), None);
        }
        // 十字：横向线段存在，必须被识别（否则整行 ┼ 会退化成逐格矢量绘制）
        assert_eq!(unpack_hweight(pack_hweight('┼')), Some(LineWeight::Light));
    }

    /// `CellAttrs::bits` 表达的属性集合必须能被逐位读回。
    #[test]
    fn cell_attrs_bits_roundtrip() {
        let mut cell = Cell::default();
        cell.flags.insert(
            Flags::BOLD | Flags::ITALIC | Flags::UNDERLINE | Flags::STRIKEOUT | Flags::UNDERCURL,
        );
        let bits = CellAttrs::from_cell(&cell).bits();
        assert_ne!(bits & bit::BOLD, 0);
        assert_ne!(bits & bit::ITALIC, 0);
        assert_ne!(bits & bit::UNDERLINE, 0);
        assert_ne!(bits & bit::UNDERCURL, 0);
        assert_ne!(bits & bit::STRIKETHROUGH, 0);
        assert_eq!(bits & bit::DOUBLE_UNDERLINE, 0, "单下划线不应置双下划线位");
        assert_eq!(bits & bit::HIDDEN, 0);
    }

    /// `layout_row` 必须产出**连续且互不重叠**的背景矩形与文本 run。
    ///
    /// 这是批量绘制路径的前提：`paint` 用 `force_width` 一次对齐一整段，
    /// 一旦 run 的列跨度与字符串长度不符，长行末尾就会累积可见偏移。
    #[test]
    fn layout_row_is_contiguous() {
        let r = test_renderer();
        let colors = Colors::default();

        let mut cells: Vec<(usize, Cell)> = Vec::new();
        for col in 0..10usize {
            let mut cell = Cell {
                c: char::from(b'a' + col as u8),
                ..Default::default()
            };
            if col == 5 {
                cell.flags.insert(Flags::BOLD);
            }
            if col == 7 {
                // 前景换色 → 必须断段（分段键含前景色）
                cell.fg = Color::Indexed(1);
            }
            cells.push((col, cell));
        }

        let (bgs, runs) = r.layout_row(0, cells.into_iter(), &colors);

        // 背景：连续覆盖 0..10，无空洞、无重叠
        let mut expect = 0usize;
        for bg in &bgs {
            assert_eq!(bg.start_col, expect, "背景矩形必须首尾相接");
            assert!(bg.end_col > bg.start_col);
            expect = bg.end_col;
        }
        assert_eq!(expect, 10, "背景必须覆盖整行");

        // 文本段：升序、互不重叠、每段字符串宽度等于列跨度，且**并集覆盖整行**。
        // 「列跨度 == 字符数」是批量绘制路径的硬前提：绘制时用
        // `force_width × 每字形列数` 一次对齐一整段，一旦不符，长行末尾会累积偏移。
        let mut covered = 0usize;
        let mut prev_end = 0usize;
        for run in &runs {
            assert!(
                run.start_col >= prev_end,
                "文本段必须升序且不重叠：{:?}",
                runs
            );
            let cols = run.text.chars().count();
            assert_eq!(cols, run.text.len(), "本用例全是 ASCII");
            covered += cols;
            prev_end = run.start_col + cols;
        }
        assert_eq!(covered, 10, "文本段必须覆盖整行：{:?}", runs);

        // 属性变化处必须断开（粗体在 col 5，索引色在 col 7）
        assert!(runs.len() >= 3, "属性变化应产生至少 3 段：{:?}", runs);
        assert_eq!(runs[0].start_col, 0);
        assert!(!runs[0].bold);
        assert!(
            runs.iter().any(|r| r.bold && r.start_col == 5),
            "粗体段必须从 col 5 开始：{:?}",
            runs
        );
        assert!(
            runs.iter().any(|r| r.start_col == 7),
            "换前景色处必须断段：{:?}",
            runs
        );
    }

    /// 宽字符占位格（`WIDE_CHAR_SPACER`）必须保留列宽：文本段要含一个空格，
    /// 且不能把左右两段错误地粘成一段（否则后续字形整体左移一格）。
    #[test]
    fn layout_row_pads_wide_char_spacer() {
        let r = test_renderer();
        let colors = Colors::default();

        let mut wide = Cell {
            c: '中',
            ..Default::default()
        };
        wide.flags.insert(Flags::WIDE_CHAR);
        let mut spacer = Cell {
            c: ' ',
            ..Default::default()
        };
        spacer.flags.insert(Flags::WIDE_CHAR_SPACER);

        let cells = vec![
            (
                0,
                Cell {
                    c: 'a',
                    ..Default::default()
                },
            ),
            (1, wide),
            (2, spacer),
            (
                3,
                Cell {
                    c: 'b',
                    ..Default::default()
                },
            ),
        ];
        let (_bgs, runs) = r.layout_row(0, cells.into_iter(), &colors);

        // 拼接后必须仍然是 4 列：'a' '中' ' ' 'b'
        let joined: String = runs.iter().map(|r| r.text.clone()).collect();
        assert_eq!(joined.chars().count(), 4, "拼接后列数必须与网格一致");
        assert_eq!(joined, "a中 b");
        // 第二段（'b'）必须从第 3 列开始，而不是紧贴 '中'
        let b_run = runs.iter().find(|r| r.text.contains('b')).expect("应有 b");
        assert_eq!(b_run.start_col, 3, "宽字符后的文本必须从第 3 列开始");
    }

    /// 背景色必须覆盖 `WIDE_CHAR_SPACER` 自己的底色：
    /// 旧实现整格 `continue`，宽字符右半边异色时会漏刷一块。
    #[test]
    fn layout_row_paints_spacer_background() {
        let r = test_renderer();
        let colors = Colors::default();

        let mut spacer = Cell {
            c: ' ',
            ..Default::default()
        };
        spacer.flags.insert(Flags::WIDE_CHAR_SPACER);
        spacer.bg = Color::Indexed(2);

        let cells = vec![
            (
                0,
                Cell {
                    c: 'a',
                    ..Default::default()
                },
            ),
            (1, spacer),
        ];
        let (bgs, _runs) = r.layout_row(0, cells.into_iter(), &colors);
        let special: Vec<_> = bgs
            .iter()
            .filter(|b| b.color != ColorPalette::default().background())
            .collect();
        assert_eq!(special.len(), 1, "占位格的异色底必须单独出一块");
        assert_eq!((special[0].start_col, special[0].end_col), (1, 2));
    }

    /// 暂存区必须**复用容量**而不是每帧重新分配：
    /// `prepare` 两次相同列数时容量不变（这正是「每帧零分配」的护栏）。
    #[test]
    fn scratch_reuses_capacity() {
        let mut s = FrameScratch::default();
        let (visuals, flash) = s.prepare(200);
        let (len_v, len_f) = (visuals.len(), flash.len());
        let cap = s.capacity();
        assert!(cap >= 200);
        assert_eq!(len_v, 200);
        assert_eq!(len_f, 200);

        s.clear();
        let (visuals, flash) = s.prepare(200);
        let (len_v, len_f) = (visuals.len(), flash.len());
        assert_eq!(s.capacity(), cap, "容量必须被复用");
        assert_eq!(len_v, 200);
        assert_eq!(len_f, 200);

        // 更小的列数不能把容量抖掉
        let (visuals, _) = s.prepare(80);
        let len_v = visuals.len();
        assert_eq!(s.capacity(), cap);
        assert_eq!(len_v, 80);

        // 文本缓冲同样跨帧保留
        s.text.push_str(&"x".repeat(4096));
        let text_cap = s.text.capacity();
        s.clear();
        assert_eq!(s.text.capacity(), text_cap, "文本缓冲容量必须保留");
        assert!(s.text.is_empty());
    }

    /// `prepare` 必须夹住异常列数，避免坏尺寸导致一次性巨额分配。
    #[test]
    fn scratch_column_count_is_clamped() {
        let mut s = FrameScratch::default();
        let (visuals, flash) = s.prepare(usize::MAX);
        assert_eq!(visuals.len(), MAX_SCRATCH_COLUMNS);
        assert_eq!(flash.len(), MAX_SCRATCH_COLUMNS);
        assert_eq!(s.visuals.len(), MAX_SCRATCH_COLUMNS);
        assert_eq!(s.flash.len(), MAX_SCRATCH_COLUMNS);
    }

    /// `prepare` 返回的两把借用必须长度一致（逐列一一对应），
    /// 否则 `flash[idx]` 与 `visuals[idx]` 会错位。
    #[test]
    fn prepare_returns_aligned_slices() {
        let mut s = FrameScratch::default();
        let (visuals, flash) = s.prepare(37);
        assert_eq!(visuals.len(), 37);
        assert_eq!(flash.len(), 37);
    }

    /// 非横向 box-drawing（如 '│'）不应被 `hweight` 标记，否则会被误当横向线段。
    #[test]
    fn vertical_box_char_is_not_horizontal() {
        let v = Visual {
            ch: '│',
            fg: Hsla::default(),
            bg: Hsla::default(),
            bits: 0,
            hweight: pack_hweight('│'),
            has_zw: false,
        };
        assert_eq!(v.hweight, HWEIGHT_NONE);
    }
}
