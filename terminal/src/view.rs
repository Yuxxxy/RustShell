//! Main terminal view component for GPUI.
//!
//! This module provides [`TerminalView`], the primary component for embedding terminals
//! in GPUI applications. It manages:
//!
//! - **I/O Streams**: Accepts arbitrary [`Read`]/[`Write`]
//!   streams, allowing integration with any PTY implementation
//! - **Event Handling**: Keyboard and mouse input, with configurable callbacks
//! - **Rendering**: Efficient canvas-based rendering via [`TerminalRenderer`]
//! - **Configuration**: Font, colors, dimensions, and padding via [`TerminalConfig`]
//!
//! # Architecture
//!
//! The terminal uses a push-based async I/O architecture:
//!
//! 1. A background thread reads bytes from the PTY stdout in 4KB chunks
//! 2. Bytes are sent through a [flume](https://docs.rs/flume) channel to an async task
//! 3. The async task processes bytes through the VTE parser and calls `cx.notify()`
//! 4. GPUI repaints the terminal with the updated grid
//!
//! This approach ensures the terminal only wakes when data arrives, avoiding polling.
//!
//! # Thread Safety
//!
//! - [`TerminalView`] itself is not `Send` (it contains GPUI handles)
//! - The stdin writer is wrapped in `Arc<parking_lot::Mutex<>>` for thread-safe writes
//! - Callbacks ([`ResizeCallback`], [`KeyHandler`]) must be `Send + Sync`
//!
//! # Example
//!
//! ```ignore
//! use gpui::{Context, Edges, px};
//! use rustshell_terminal::{ColorPalette, TerminalConfig, TerminalView};
//!
//! // In a GPUI window context:
//! let terminal = cx.new(|cx| {
//!     TerminalView::new(pty_writer, pty_reader, TerminalConfig::default(), cx)
//!         .with_resize_callback(move |cols, rows| {
//!             // Notify PTY of new dimensions
//!         })
//!         .with_exit_callback(|_, cx| {
//!             cx.quit();
//!         })
//! });
//!
//! // Focus the terminal to receive keyboard input
//! terminal.read(cx).focus_handle().focus(window);
//! ```

use crate::colors::ColorPalette;
use crate::event::{ColorLookup, GpuiEventProxy, TerminalEvent};
use crate::input::{
    is_text_input_keystroke, keystroke_to_bytes, needs_paste_confirm, normalize_paste,
    paste_newline_count,
};
use crate::render::{CursorPaint, FrameScratch, TerminalRenderer};
use crate::terminal::TerminalState;
use crate::zmodem::{ZmodemPump, ZmodemStatus, ZmodemUploadError, ZmodemUploadStart};
use alacritty_terminal::event::WindowSize;
use alacritty_terminal::vte::ansi::{CursorShape, Rgb as AnsiRgb};
use gpui::{Edges, *};
use std::cell::{Cell as StdCell, RefCell};
use std::io::{Read, Write};
use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// 网格尺寸「异常值已告警」计数：只记录前 3 条，避免每帧刷屏，
/// 同时保留多帧现场（首帧测量帧与后续复用帧可能表现不同）。
static GRID_DIM_WARNED: AtomicUsize = AtomicUsize::new(0);

const GRID_DIM_WARN_LIMIT: usize = 3;

fn grid_dim_warn_once(msg: String) {
    let n = GRID_DIM_WARNED.fetch_add(1, Ordering::Relaxed);
    if n < GRID_DIM_WARN_LIMIT {
        log::warn!("{}", msg);
    }
}

/// 由「可用像素 / 单元格像素」换算网格行列数的唯一收口。
///
/// 布局层给的可用空间**不保证有限**：布局缺陷（无限约束、0 约束）可能给出
/// `inf` / 天文数字；字体度量失败可能给出 `cell <= 0`。直接 `(a / c) as usize`
/// 会得到饱和巨值（`inf as usize == usize::MAX`），送进 alacritty
/// `Grid::resize` 就是 `capacity overflow` 闪退（2026-09-18 SSH 首帧实测，
/// backtrace 落在 `Storage::grow_visible_lines → Vec::resize_with`）。
///
/// 规则：非有限输入 / 除零 → 回退 `default`；结果 clamp 到 `[1, max]`。
/// 首次触发任一回退/截断时打一条 `warn` 记录原始值，供追布局根因。
fn safe_grid_dim(
    available: f32,
    cell: f32,
    font: f32,
    default: usize,
    max: usize,
    axis: &str,
    ctx: &str,
) -> usize {
    let bad = !available.is_finite() || !cell.is_finite() || cell <= 0.0;
    if bad {
        grid_dim_warn_once(format!(
            "terminal 网格尺寸计算输入异常（{axis}: available={available} cell={cell} font={font} {ctx}），回退默认 {default}"
        ));
        return default;
    }
    let n = (available / cell) as usize;
    if n < 1 {
        1
    } else if n > max {
        grid_dim_warn_once(format!(
            "terminal 网格尺寸超限（{axis}: available={available} cell={cell} font={font} → {n}），clamp 到 {max}"
        ));
        max
    } else {
        n
    }
}

/// 单列/单行的安全上限。真机 4K 宽度下 492 列封顶，1000 已远超物理可能；
/// 内存上界 1000×1000×sizeof(Cell) 约几十 MB，可接受。
const MAX_GRID_COLS: usize = 1000;
const MAX_GRID_ROWS: usize = 1000;
use std::sync::Arc;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// gpui `Hsla`（sRGB 语义）→ alacritty `Rgb`（8bit/通道）。
///
/// 用于应答 OSC 4/10/11 颜色查询：答错颜色不会崩，但答不上可能让查询方挂起。
fn hsla_to_ansi_rgb(c: gpui::Hsla) -> AnsiRgb {
    let rgba: gpui::Rgba = c.into();
    let q = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    AnsiRgb {
        r: q(rgba.r),
        g: q(rgba.g),
        b: q(rgba.b),
    }
}

/// 由终端调色板构造 OSC 颜色查询应答表。
///
/// 索引语义同 `NamedColor`：0–15 = ANSI/Bright，16–255 = 扩展色，
/// 256 = 前景，257 = 背景，258 = 光标。
fn color_lookup_from(palette: &ColorPalette) -> ColorLookup {
    let ansi: Vec<gpui::Hsla> = palette.ansi_colors().to_vec();
    let ext: Vec<gpui::Hsla> = palette.extended_colors().to_vec();
    let fg = palette.foreground();
    let bg = palette.background();
    let cursor = palette.cursor();
    Arc::new(move |idx: usize| match idx {
        0..=15 => Some(hsla_to_ansi_rgb(ansi[idx])),
        16..=255 => Some(hsla_to_ansi_rgb(ext[idx])),
        256 => Some(hsla_to_ansi_rgb(fg)),
        257 => Some(hsla_to_ansi_rgb(bg)),
        258 => Some(hsla_to_ansi_rgb(cursor)),
        _ => None,
    })
}

/// Configuration for terminal creation and runtime updates.
///
/// This struct defines the terminal's appearance and behavior, including
/// grid dimensions, font settings, scrollback buffer, and color scheme.
///
/// # Default Values
///
/// | Field | Default |
/// |-------|---------|
/// | `cols` | 80 |
/// | `rows` | 24 |
/// | `font_family` | "monospace" |
/// | `font_size` | 14px |
/// | `scrollback` | 10000 |
/// | `line_height_multiplier` | 1.0 |
/// | `padding` | 0px all sides |
/// | `colors` | Default palette |
///
/// # Example
///
/// ```ignore
/// use gpui::{Edges, px};
/// use rustshell_terminal::{ColorPalette, TerminalConfig};
///
/// let config = TerminalConfig {
///     cols: 120,
///     rows: 40,
///     font_family: "JetBrains Mono".into(),
///     font_size: px(13.0),
///     scrollback: 50000,
///     line_height_multiplier: 1.0,
///     padding: Edges::all(px(10.0)),
///     colors: ColorPalette::builder()
///         .background(0x1a, 0x1a, 0x1a)
///         .foreground(0xe0, 0xe0, 0xe0)
///         .build(),
/// };
/// ```
///
/// # Runtime Updates
///
/// Configuration can be updated at runtime via [`TerminalView::update_config`].
/// This is useful for implementing features like dynamic font sizing:
///
/// ```ignore
/// terminal.update(cx, |terminal, cx| {
///     let mut config = terminal.config().clone();
///     config.font_size += px(1.0);
///     terminal.update_config(config, cx);
/// });
/// ```
#[derive(Clone, Debug)]
pub struct TerminalConfig {
    /// Number of columns (character width) in the terminal
    pub cols: usize,

    /// Number of rows (lines) in the terminal
    pub rows: usize,

    /// Font family name (e.g., "Fira Code", "JetBrains Mono")
    pub font_family: String,

    /// Font size in pixels
    pub font_size: Pixels,

    /// Maximum number of scrollback lines to keep in history
    pub scrollback: usize,

    /// Multiplier for line height to accommodate tall glyphs (e.g., nerd fonts)
    /// Default is 1.0 (no extra height)
    pub line_height_multiplier: f32,

    /// Padding around the terminal content (top, right, bottom, left)
    /// The padding area renders with the terminal's background color
    pub padding: Edges<Pixels>,

    /// Color palette for terminal colors (16 ANSI colors, 256 extended colors,
    /// foreground, background, and cursor colors)
    pub colors: ColorPalette,

    /// ★ 本地扩展（RustShell）：PTY 输出字节的源编码 key（如 `"utf-8"` / `"gbk"`）。
    ///
    /// 空串或 `utf-8` → 零拷贝直通（默认行为）；
    /// 其他 → 在**后台 reader 线程**把字节转码为 UTF-8 后再送入 VTE 解析器
    /// （UI 线程不做转码，避免编码转换阻塞渲染）。
    ///
    /// `None` 由 `Encoding::for_label` 解析：未知 label 回退 UTF-8。
    pub encoding: Option<String>,

    /// 光标闪烁总开关。关闭后光标常亮（不启动闪烁定时器）。
    ///
    /// 注意：这是**用户侧**开关；终端侧还能用 DECSCUSR / `ESC[?12h|l`
    /// 单独要求或不要求闪烁，两者是「与」关系。
    pub cursor_blink: bool,

    /// 光标闪烁半周期（毫秒）。默认 530ms，与 xterm / VTE 一致。
    /// 相位每半个周期翻转一次，故完整明暗循环是它的两倍。
    pub cursor_blink_interval_ms: u64,

    /// 默认光标形状：块状 / 竖线 / 下划线（`Hidden` 会被回退为块状）。
    ///
    /// 2026-09-28 补：此前形状被硬编码为 `CursorShape::Block`（见
    /// [`TerminalState::with_scrollback`]），用户无控制权。这是**默认值**：
    /// 远端程序用 DECSCUSR（`ESC[2 q` 等）时仍会覆盖，与 `cursor_blink` 同口径。
    pub default_cursor_shape: CursorShape,

    /// 粘贴时是否把行尾统一规范为回车（`\r`）。
    ///
    /// 开启（默认）：粘贴多行文本会**逐行执行** —— 终端语义里回车才是「执行」，
    /// `\n` 只换行不回列，直接送出去会出现阶梯状错位。
    /// 关闭：原样透传，交给远端自己解释（少数需要保留原始字节的场景）。
    pub paste_normalize_newlines: bool,

    /// 多行粘贴的**确认阈值**（F-5）：`None` = 保护关闭（直接写）；
    /// `Some(n)` = 行数超过 n 时先把请求[挂起](TerminalView::pending_paste)等宿主确认。
    ///
    /// 用 `Option` 而不是「开关 + 阈值」两个字段，是**故意的**：两个字段意味着
    /// 「开关开了、阈值忘了传」这种半死状态在类型上合法，而它的症状是保护静默失效
    /// —— 恰好是这个功能最不该出的错。合成一个值之后，宿主只有一处换算
    /// （`connections.rs` 建会话、设置保存热更新），漏传就是编译错。
    ///
    /// 阈值语义见 [`needs_paste_confirm`]：默认 1 ⇒ 任何换行都要确认。
    /// 引擎默认 `None`（库不替宿主做产品决策；宿主 RustShell 默认 `Some(1)`）。
    pub paste_protect_lines: Option<u32>,

    /// ZMODEM（`sz` 下载 / 拖放 `rz` 上传）的落盘目录。
    ///
    /// `None` 时退回进程工作目录。宿主传用户「下载」目录 —— 引擎不依赖
    /// 平台路径库（那是宿主的职责），这里只收一个明确落点。
    pub zmodem_download_dir: Option<PathBuf>,
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self {
            cols: 80,
            rows: 24,
            font_family: "monospace".into(),
            font_size: px(14.0),
            scrollback: 10000,
            line_height_multiplier: 1.0,
            padding: Edges::all(px(0.0)),
            colors: ColorPalette::default(),
            encoding: None,
            // 默认开闪烁：主流终端的一致预期；不需要的用户可在设置里关
            cursor_blink: true,
            cursor_blink_interval_ms: 530,
            default_cursor_shape: CursorShape::Block,
            paste_normalize_newlines: true,
            // 引擎默认不开粘贴闸：这是宿主的产品决策（库不该替调用方定
            // 「粘贴什么要问用户」），宿主在建会话时显式传自己的设置
            paste_protect_lines: None,
            // ZMODEM 落点由宿主给（通常 = 系统下载目录）
            zmodem_download_dir: None,
        }
    }
}

/// 一次被粘贴闸拦下的粘贴请求：等宿主问过用户之后才写管道（F-5）。
///
/// 存的是**原始文本**（未做行尾归一 / bracketed 包裹）与原始的 `and_enter`：
/// 确认后走的仍是 [`TerminalView::paste_text`] 的完整通路，只是把「要不要问」
/// 这一问提前答掉了。存归一后的 payload 会在「远端 bracketed 模式在确认期间
/// 变了」时用错包裹方式。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PasteRequest {
    /// 原始粘贴文本（用户剪贴板里的那份，一个字节没动）
    pub text: String,
    /// 是否「粘贴并执行」（末尾补回车）
    pub and_enter: bool,
    /// 换行个数（CR / LF / CRLF 各算一个，见 [`paste_newline_count`]）——
    /// 宿主提示文案用它换算行数（[`PasteRequest::lines`]）。
    pub newlines: usize,
}

impl PasteRequest {
    /// 给人看的行数 = 换行数 + 1（与 [`needs_paste_confirm`] 的「行数」同口径）。
    pub fn lines(&self) -> usize {
        self.newlines + 1
    }
}

/// Callback type for PTY resize notifications.
///
/// This callback is invoked when the terminal grid dimensions change,
/// typically due to window resizing. The callback receives the new
/// column and row counts.
///
/// # Arguments
///
/// * `cols` - New number of columns (characters wide)
/// * `rows` - New number of rows (lines tall)
///
/// # Thread Safety
///
/// This callback must be `Send + Sync` as it may be called from the render thread.
///
/// # Example
///
/// ```ignore
/// use portable_pty::PtySize;
///
/// let pty = Arc::new(Mutex::new(pty_master));
/// let pty_clone = pty.clone();
///
/// terminal.with_resize_callback(move |cols, rows| {
///     pty_clone.lock().resize(PtySize {
///         cols: cols as u16,
///         rows: rows as u16,
///         pixel_width: 0,
///         pixel_height: 0,
///     }).ok();
/// });
/// ```
pub type ResizeCallback = Box<dyn Fn(usize, usize) + Send + Sync>;

/// Callback type for key event interception.
///
/// This callback is invoked before the terminal processes a key event,
/// allowing you to intercept and handle specific key combinations.
///
/// # Arguments
///
/// * `event` - The key down event from GPUI
///
/// # Returns
///
/// * `true` - Consume the event (terminal will not process it)
/// * `false` - Let the terminal handle the event normally
///
/// # Thread Safety
///
/// This callback must be `Send + Sync`.
///
/// # Example
///
/// ```ignore
/// terminal.with_key_handler(|event| {
///     let keystroke = &event.keystroke;
///
///     // Intercept Ctrl++ for font size increase
///     if keystroke.modifiers.control && (keystroke.key == "+" || keystroke.key == "=") {
///         // Handle font size increase
///         return true; // Consume the event
///     }
///
///     // Intercept Ctrl+- for font size decrease
///     if keystroke.modifiers.control && keystroke.key == "-" {
///         // Handle font size decrease
///         return true;
///     }
///
///     false // Let terminal handle all other keys
/// });
/// ```
pub type KeyHandler = Box<dyn Fn(&KeyDownEvent) -> bool + Send + Sync>;

/// Callback for terminal bell events.
///
/// This callback is invoked when the terminal bell is triggered (BEL character,
/// ASCII 0x07), allowing you to play a sound or show a visual indicator.
///
/// # Arguments
///
/// * `window` - The GPUI window
/// * `cx` - The context for the TerminalView
///
/// # Example
///
/// ```ignore
/// terminal.with_bell_callback(|window, cx| {
///     // Option 1: Visual bell (flash the window or show an indicator)
///     // Option 2: Play a sound
///     // Option 3: Notify the user via system notification
/// });
/// ```
pub type BellCallback = Box<dyn Fn(&mut Window, &mut Context<TerminalView>)>;

/// Callback for terminal title changes.
///
/// This callback is invoked when the terminal title changes via escape sequences
/// (OSC 0, OSC 2), allowing you to update the window or tab title.
///
/// # Arguments
///
/// * `window` - The GPUI window
/// * `cx` - The context for the TerminalView
/// * `title` - The new title string
///
/// # Example
///
/// ```ignore
/// terminal.with_title_callback(|window, cx, title| {
///     // Update the window title
///     // Or update a tab label in a tabbed interface
///     println!("Terminal title changed to: {}", title);
/// });
/// ```
pub type TitleCallback = Box<dyn Fn(&mut Window, &mut Context<TerminalView>, &str)>;

/// Callback for clipboard store requests.
///
/// This callback is invoked when the terminal wants to store data to the clipboard
/// via OSC 52 escape sequence. Applications like tmux and vim can use this to
/// copy text to the system clipboard.
///
/// # Arguments
///
/// * `window` - The GPUI window
/// * `cx` - The context for the TerminalView
/// * `text` - The text to store in the clipboard
///
/// # Example
///
/// ```ignore
/// terminal.with_clipboard_store_callback(|_window, cx, text| {
///     cx.write_to_clipboard(gpui::ClipboardItem::new_string(text.to_string()));
/// });
/// ```
pub type ClipboardStoreCallback = Box<dyn Fn(&mut Window, &mut Context<TerminalView>, &str)>;

/// Callback for OSC 52 clipboard **read** requests.
///
/// 返回值即剪贴板文本；`None` 表示宿主无法提供（例如剪贴板里不是文本）。
/// 序列拼装（base64、ST/BEL 收尾）由 alacritty 提供的格式化器完成，
/// 终端引擎自身**不接触系统剪贴板**，也不因此引入原生依赖。
pub type ClipboardLoadCallback =
    Box<dyn Fn(&mut Window, &mut Context<TerminalView>) -> Option<String> + 'static>;

/// Callback for shell integration 命令生命周期标记（`OSC 133` / `633`）。
///
/// 只在远端主动发这些序列时触发；不发就永远不触发，无副作用。
/// 参数是标记本体，终端网格**不做任何视觉改动** —— 渲染交给宿主。
pub type ShellMarkCallback =
    Box<dyn Fn(&mut Window, &mut Context<TerminalView>, crate::shell_integration::PromptMark)>;

/// Callback for `OSC 7` 工作目录变更。
///
/// ⚠️ 参数是**远端**路径。宿主若拿它去开本地文件对话框（如 SFTP 面板），
/// 必须先确认当前确实是一条远端会话 —— 本地会话里它才是本地路径。
pub type ShellCwdCallback = Box<dyn Fn(&mut Window, &mut Context<TerminalView>, &str)>;

/// Callback for `OSC 633;E` 命令行文本。
pub type ShellCommandLineCallback = Box<dyn Fn(&mut Window, &mut Context<TerminalView>, &str)>;

/// Callback for 「当前输入行可能变了」。
///
/// **不是**"这一帧文本是什么"，而是"该重新去问一次"的通知 —— 参数里没有文本。
///
/// # 参数为什么是 `&TerminalView`（而不是让宿主自己 `read` 实体）
///
/// 本回调在 [`TerminalView::process_events`] 里兑现，而 `process_events` 是从
/// `render` 调进来的 —— 此刻本实体的**租约**（gpui `EntityMap::lease`）正握在
/// 引擎手里，实体被**从实体表移到栈上**。宿主若在回调里
/// `entity.read(cx)` / `entity.update(cx, ..)`，拿到的是
/// `cannot read rustshell_terminal::view::TerminalView while it is already being
/// updated` 的 **panic**（`entity_map.rs` 的 `double_lease_panic`）——
/// 是直接闪退，不是"读到旧值"这种软失败。
///
/// 所以这里不是"给通知不给内容"，而是**把引擎自己借出去**：宿主想读就
/// [`TerminalView::current_input_line`]，不想读就不读（浮层没开时那次
/// `with_term` 加锁 + 整行切片就省下了）—— 懒惰性与原设计一致，
/// 但不再依赖实体访问。三个回调（本回调 / [`EnterCallback`] /
/// [`CompletionKeyFilter`]）全部采用这一手法，理由相同。
///
/// 触发点（两处，缺一处就有"打字候选不更新 / 粘贴后候选是旧的"：
/// 对照铁律 #77「判定要贴着动作落点」）：
/// - [`TerminalView::commit_text`]（IME 上屏 / 平台直投字符）；
/// - [`TerminalView::paste_text`]（粘贴）。
///
/// 刻意**不**在 `on_key_down` 里触发：那里只处理控制键，可打印字符走文本通路
/// （见 `is_text_input_keystroke`），在按键里报变化会漏掉真正改行的那条路。
pub type InputLineCallback = Box<dyn Fn(&TerminalView, &mut Window, &mut Context<TerminalView>)>;

/// 「用户按了回车，命令即将执行」的通知回调。
///
/// # 为什么单独再给一个回调（而不是让宿主从 `InputLineCallback` 里自己判）
///
/// 上屏 / 粘贴与「按回车」是**两件事**：前者改的是"当前行长什么样"，
/// 后者是"这一行即将成为一条命令"。宿主拿后者的用途是**记进命令历史**
/// （C-1 的第 ② 号数据源），而历史的判据是"用户真的执行了"，不是"这行改了"。
///
/// 判据放在引擎里是因为**只有引擎知道这个键最终变成了什么字节**：
/// 回车在 `keystroke_to_bytes` 里是 `\r`，但小键盘回车、粘贴里的换行、
/// 以及"浮层开着的回车被补全吃掉"这三条路都从不同地方进来。宿主在外面
/// 按 `event.keystroke.key == "enter"` 重判一遍，必然会漏掉其中几条
/// （铁律 #77：判定要贴着动作落点）。
///
/// # 与 shell integration 的关系
///
/// 这是个**兜底**信号：远端配了 shell integration 时，`OSC 133;C`（用户回车）
/// 与 `OSC 633;E`（准确命令行）给的信息更全（见 [`Self::shell_command_line`]）。
/// 但绝大多数远端根本没配，所以历史采集不能只靠那条路 —— 本回调是那个
/// 「不需要远端配合」的版本。宿主应当两路并用、按优先级取更准的那个。
///
/// 触发时机：把回车字节写进 PTY **之后**（见 `enter_dirty` 的字段注释）。
/// 参数里的 `&TerminalView` 与 [`InputLineCallback`] **同款**（引擎借出自己）：
/// 宿主在回调里调 `current_input_line()` 读那一行，**不能**去 `read` 实体 ——
/// 此刻实体在租约里，读它必 panic（原因见 `InputLineCallback` 的文档）。
/// "要不要记"（行是空的、看起来像口令）全由宿主判。
pub type EnterCallback = Box<dyn Fn(&TerminalView, &mut Window, &mut Context<TerminalView>)>;

/// 补全浮层打开时，终端按键该往哪儿去。
///
/// 由宿主在 [`TerminalView::with_completion_key_filter`] 里给：引擎把每次按键
/// 交给它判，宿主回一个处置。
///
/// **为什么要有这个回调**（而不是让宿主自己装 `key_handler`）：
/// 补全浮层的按键语义与终端本身**深度耦合** —— 哪些键该被浮层吃掉（`↑↓` 选候选、
/// `Tab` 采纳、`Esc` 关闭）、哪些必须放行（可打印键要进文本通路、否则浮层一开
/// 就打不了字，见铁律 #63）。判据要读 `self.preedit` / `ALT_SCREEN` 这些
/// **只有引擎内部才知道**的状态，宿主在外面重写一遍必然走样。
///
/// # 参数里的 `&TerminalView`
///
/// 同 [`InputLineCallback`] 的理由：本过滤器在 `on_key_down` 里兑现，而
/// `on_key_down` 也在本实体的租约里 —— 宿主调用点**不能** `read` / `update`
/// 实体（必 panic）。引擎改为借出自己，过滤器靠它做两件事：
/// 读当前行判前缀（[`TerminalView::current_input_line`]）、采纳候选时把后缀
/// 写回 PTY（[`TerminalView::write_to_pty`]）。
///
/// 借出的是**共享**借用（`&`，不是 `&mut`）：这两个动作本身只需要 `&self`，
/// 于是过滤器拿不到 `&mut`，也就**改不了**引擎状态 —— 想改引擎状态的副作用
/// 必须走返回值（[`CompletionKeyOutcome`]），这是刻意留的一道约束。
pub type CompletionKeyFilter = Box<
    dyn Fn(
        &KeyDownEvent,
        &TerminalView,
        &mut Window,
        &mut Context<TerminalView>,
    ) -> CompletionKeyOutcome,
>;

/// 补全浮层对一次按键的处置（[`CompletionKeyFilter`] 的返回值）。
///
/// 返回的只是"这个键要不要被终端吞掉"。**宿主态**的副作用（浮层开关、选中项）
/// 由过滤器自己完成（它拿得到 `cx`，能经 `UiHub` 回到宿主实体）；
/// **引擎态**的副作用则只能由引擎按本返回值施加 —— 过滤器只借到
/// `&TerminalView`，改不了引擎状态（见 [`CompletionKeyFilter`] 的文档）。
/// 三个变体是让过滤器把**意图**写清楚（排障时一眼看出一串按键里哪一下是切浮层），
/// 引擎对后两者的处理**完全相同**。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionKeyOutcome {
    /// 浮层没开 / 这个键与浮层无关：终端照常处理，浮层不动。
    PassThrough,
    /// 浮层吃掉了这个键（效果已由过滤器自己施加）：终端不写字节、不派发。
    Consumed,
    /// 这一次按键是在**切换**浮层的开合（Ctrl+Space）。同样不写字节 ——
    /// Ctrl+Space 会被 `keystroke_to_bytes` 译成 `\x00`，写进 PTY 就是垃圾输入
    /// （远端 shell 收到 NUL）。
    ToggleRequested,
}

/// Callback for terminal exit events.
///
/// This callback is invoked when the terminal process exits (e.g., shell exits,
/// process terminates). This is detected when the PTY reader reaches EOF.
///
/// # Arguments
///
/// * `window` - The GPUI window
/// * `cx` - The context for the TerminalView
///
/// # Example
///
/// ```ignore
/// terminal.with_exit_callback(|window, cx| {
///     // Option 1: Quit the application
///     cx.quit();
///
///     // Option 2: Close this terminal tab/pane
///     // terminal_manager.close_terminal(terminal_id);
///
///     // Option 3: Show an exit message
///     // show_notification("Terminal exited");
/// });
/// ```
pub type ExitCallback = Box<dyn Fn(&mut Window, &mut Context<TerminalView>)>;

/// The main terminal view component for GPUI applications.
///
/// `TerminalView` is a GPUI entity that implements the [`Render`] trait,
/// providing a complete terminal emulator that can be embedded in any GPUI application.
///
/// # Responsibilities
///
/// - **Terminal State**: Manages the grid, cursor, and colors via [`TerminalState`]
/// - **I/O Streams**: Reads from PTY stdout and writes to PTY stdin
/// - **Event Handling**: Processes keyboard, mouse, and resize events
/// - **Rendering**: Paints text, backgrounds, and cursor via [`TerminalRenderer`]
/// - **Callbacks**: Dispatches events to user-provided callbacks
///
/// # Creating a Terminal
///
/// Use [`TerminalView::new`] within a GPUI entity context:
///
/// ```ignore
/// let terminal = cx.new(|cx| {
///     TerminalView::new(writer, reader, config, cx)
///         .with_resize_callback(resize_callback)
///         .with_exit_callback(|_, cx| cx.quit())
/// });
/// ```
///
/// # Focus
///
/// The terminal must be focused to receive keyboard input:
///
/// ```ignore
/// terminal.read(cx).focus_handle().focus(window);
/// ```
///
/// # Callbacks
///
/// Configure behavior through builder methods:
///
/// - [`with_resize_callback`](Self::with_resize_callback) - PTY size changes
/// - [`with_exit_callback`](Self::with_exit_callback) - Process exit
/// - [`with_key_handler`](Self::with_key_handler) - Key event interception
/// - [`with_bell_callback`](Self::with_bell_callback) - Terminal bell
/// - [`with_title_callback`](Self::with_title_callback) - Title changes
/// - [`with_clipboard_store_callback`](Self::with_clipboard_store_callback) - Clipboard writes
///
/// # Thread Safety
///
/// `TerminalView` is not `Send` as it contains GPUI handles. The stdin writer
/// is internally wrapped in `Arc<parking_lot::Mutex<>>` for safe concurrent access.
pub struct TerminalView {
    /// The terminal state managing the grid and VTE parser
    state: TerminalState,

    /// The renderer for drawing terminal content
    renderer: TerminalRenderer,

    /// Focus handle for keyboard event handling
    focus_handle: FocusHandle,

    /// Writer for sending input to the terminal process
    stdin_writer: Arc<parking_lot::Mutex<Box<dyn Write + Send>>>,

    /// ZMODEM 协议泵（`sz` 下载 / `rz` 上传 / 拖放上传）。
    ///
    /// 放在引擎里而不是宿主里：协议工作在**字节流**上（识别 `**\x18B`、
    /// 吃掉协议帧、把其余文字照常交给 VTE），而字节流恰好握在读线程手里。
    /// 读线程按**原始字节**喂它（早于编码转换），所以本地 PTY / SSH / 将来的
    /// 串口都自动继承同一套能力，不必各写一遍。
    zmodem: Arc<ZmodemPump>,

    /// Receiver for terminal events from the event proxy
    event_rx: mpsc::Receiver<TerminalEvent>,

    /// 同一事件通道的**发送端**，留在视图里供重挂（[`TerminalView::reattach`]）复用。
    ///
    /// 「会话结束」这个事件是输出泵任务经这条通道投给 `process_events` 的，
    /// 而 `process_events` 需要 `Window`（只有渲染期拿得到）⇒ 事件必须跨线程走通道，
    /// 不能直接调回调。重挂时读端任务要换一个，但它投 Exit 用的必须是**同一条**通道。
    exit_tx: mpsc::Sender<TerminalEvent>,

    /// 文本区像素尺寸（`CSI 14 t` 应答用），渲染时按实测刷新
    window_size: Arc<parking_lot::Mutex<WindowSize>>,

    /// Configuration used to create this terminal
    config: TerminalConfig,

    /// Async task that reads bytes and notifies the view (push-based)
    #[allow(dead_code)]
    _reader_task: Task<()>,

    /// Callback to notify the PTY about size changes
    resize_callback: Option<Arc<ResizeCallback>>,

    /// Optional callback to intercept key events before terminal processing
    key_handler: Option<Arc<KeyHandler>>,

    /// Callback for terminal bell events
    bell_callback: Option<BellCallback>,

    /// Callback for terminal title changes
    title_callback: Option<TitleCallback>,

    /// Callback for clipboard store requests
    clipboard_store_callback: Option<ClipboardStoreCallback>,

    /// Callback for terminal exit events
    exit_callback: Option<ExitCallback>,

    // ── shell integration（OSC 133 / 633 / 7）的感知状态 ──
    //
    // 这四样都**只存不渲染**：本阶段（C-0）的目标是"把语义拿到手"，
    // 视觉与交互留给后续阶段。存下来是为了让后续阶段（命令块、补全、
    // 跳转上一提示符）不必再改一遍引擎。
    /// 最近收到的命令生命周期标记（有上限，见 `process_events`）。
    shell_marks: Vec<crate::shell_integration::PromptMark>,

    /// 最近一次 `OSC 7` 上报的工作目录（**远端路径**）。
    shell_cwd: Option<String>,

    /// 最近一次 `OSC 633;E` 给出的准确命令行。
    shell_command_line: Option<String>,

    /// 命令生命周期标记回调。
    shell_mark_callback: Option<ShellMarkCallback>,

    /// 工作目录变更回调（远端路径）。
    shell_cwd_callback: Option<ShellCwdCallback>,

    /// 命令行变更回调（`OSC 633;E`）。
    shell_command_line_callback: Option<ShellCommandLineCallback>,

    /// 「当前输入行可能变了」的通知回调（见 [`InputLineCallback`]）。
    input_line_callback: Option<InputLineCallback>,

    /// 输入行变化待通知（两段式，见 [`Self::notify_input_line`]）。
    input_line_dirty: bool,

    /// 「用户按了回车（命令即将执行）」的通知回调（见 [`EnterCallback`]）。
    enter_callback: Option<EnterCallback>,

    /// 回车待通知（两段式，与 `input_line_dirty` 同一手法）。
    ///
    /// **顺序是硬约束**：必须在把 `\r` 写进 PTY **之后**才兑现 —— 兑现时宿主会去
    /// `current_input_line()` 读那一行，而"按回车"发生在远端回显之前，读到的
    /// 正是用户刚敲完的那一行。若在写入前读，读到的是同一份内容（网格更新在
    /// reader 侧、异步），但万一将来改成同步网格，顺序反了就会把命令读丢。
    /// 写在后面是**两种实现下都对**的那一侧。
    enter_dirty: bool,

    /// 补全浮层的按键过滤器（见 [`CompletionKeyFilter`]）。
    completion_key_filter: Option<CompletionKeyFilter>,

    /// shell integration 事件的**专用**通道发送端。
    ///
    /// 为什么不复用 `exit_tx` / `event_rx`：那条通道的发送端握在
    /// `GpuiEventProxy`（alacritty 事件代理）里，它的负载由终端引擎**自己**
    /// 决定；而 shell integration 事件是**字节解析**的产物，产生点在
    /// `Processor::advance` 的调用栈内（读端任务里），跟事件代理毫无关系。
    /// 硬塞进同一条通道要靠"把发送端传进 `TerminalState`"，那会让引擎
    /// 反向依赖视图的通道 —— 两者职责反而更乱了。
    ///
    /// 这条通道是**引擎到视图的专用上行口**：读端任务里
    /// `process_bytes_with_events` 投递，`process_events` 消费。
    /// 生命周期跟视图一致（不随 `reattach` 更换）—— 重挂换的是字节来源，
    /// 事件出口没有换的理由。
    shell_event_tx: mpsc::Sender<TerminalEvent>,

    /// [`shell_event_tx`](Self::shell_event_tx) 的接收端。
    shell_event_rx: mpsc::Receiver<TerminalEvent>,

    /// 终端内容区像素 bounds（canvas 首闭包写入，供鼠标坐标 → 单元格换算）
    content_bounds: Rc<RefCell<Bounds<Pixels>>>,

    /// 实测单元格尺寸——paint 时写入；鼠标换算必须用它，
    /// 而非 renderer 构造时的估算值（font_size*0.6/1.4），否则划选偏移。
    ///
    /// 用 `Cell`（不是 `RefCell`）：`CellMetrics` 是 `Copy`，热路径（鼠标移动、
    /// 滚动）只做一次无锁的读拷贝，不需要 `RefCell` 的借用计数与 panic 路径。
    ///
    /// ★ 必须包 `Rc`：`render()` 每帧把克隆交给 canvas paint 闭包。`Cell<T>`
    ///   的 `clone()` 是**值拷贝**——直接克隆 `StdCell` 会得到独立副本，闭包里
    ///   `set()` 写进的是副本，视图字段永远停在 `ZERO`；下一帧再克隆出 `(0,0)`
    ///   的新副本走「复用缓存」分支 → 除零 → 网格分配 `capacity overflow`
    ///   （2026-09-18 SSH 首帧闪退根因）。`Rc` 克隆才是共享同一份存储。
    cell_metrics: Rc<StdCell<CellMetrics>>,

    /// 方案 B：`measure_cell` 缓存的**生效字体指纹** `(family, size, scale_factor)`。
    ///
    /// paint 闭包每帧都会跑，而 `measure_cell` 要做一次 `shape_line('│')` +
    /// 字体度量——字体没变时这纯属浪费。只有指纹变化（或首次）才重新测量。
    /// 含 `scale_factor` 是因为 DPI 变化会改变实际字形度量。
    measured_for: Rc<RefCell<Option<(String, f32, f32)>>>,

    /// 渲染暂存区（跨帧复用，见 [`FrameScratch`]）。
    ///
    /// 用 `Arc<Mutex>` 而不是普通字段：canvas 的 paint 闭包要求 `Send + Sync`
    /// 且与视图的 `&mut self` 借用不共存，故用一个**每帧只锁一次**的旁路暂存区。
    render_scratch: Arc<parking_lot::Mutex<FrameScratch>>,

    /// 鼠标划选状态（视口行列坐标；None = 无选择）
    selection: Rc<RefCell<Option<crate::mouse::Selection>>>,

    /// 是否正在拖拽划选
    selecting: bool,

    /// 连击检测（双击选词/三击选行）：上次左键点击时间与位置
    last_click: Option<(std::time::Instant, gpui::Point<Pixels>)>,
    /// 当前连续点击次数（1=单击 2=双击 ≥3=三击+）
    click_count: usize,

    /// 搜索状态（Ctrl+F）：查询串 + 已编译正则；None = 无搜索
    search: Arc<parking_lot::Mutex<Option<SearchState>>>,

    /// 上次跳转命中的起始绝对行号（Enter 连续跳转的推进基准）
    search_last_hit: Arc<parking_lot::Mutex<Option<i32>>>,

    /// 视口命中区间缓存（键见 [`SearchKey`]）。None = 当前无搜索 / 已失效。
    ///
    /// `Arc<Mutex>`：canvas 的 paint 闭包要求 `Send + Sync`，且与视图的
    /// `&mut self` 借用不共存（同 [`TerminalView::render_scratch`]）。
    search_spans_cache: SearchSpansCache,

    /// 终端内容版本：每处理一批 PTY 字节 +1。命中区间缓存靠它判断「网格变了」。
    content_gen: Arc<std::sync::atomic::AtomicU64>,

    /// 光标闪烁相位：true = 本帧绘制光标
    blink_on: bool,

    /// 闪烁定时器。`None` 即停表（drop 一个 `Task` 就是取消它）。
    blink_task: Option<Task<()>>,

    /// 终端是否持有焦点。失焦时停表并保持光标常亮。
    has_focus: bool,

    /// 会话是否活着。`false` = 宿主已判定会话终止（SSH 掉线 / 远端注销 / 本地 shell 退出）。
    ///
    /// ★ 为什么要有这个开关，而不是「宿主别把事件转进来就行」：
    /// 断连之后终端视图**故意留在屏上**（画面是断连现场，用户要能读、能复制），
    /// 于是它仍然持有焦点、仍然收得到键盘事件、仍然被快捷命令 / AI 通过
    /// [`TerminalView::write_to_pty`] 写。这些都只有一个正确的收口 —— 引擎自己
    /// 在写入前拦一刀。散在宿主侧判定的话，任何一条新加的写入通路都会漏。
    ///
    /// 症状（没有这一刀时）：光标继续闪、敲键看着有反应、粘贴进来的命令排在
    /// 一个永远不会返回的管道里 —— 用户以为会话还活着。
    ///
    /// 2026-09-24 补上第四条通路：**输入法 / 平台的文本消息**（`WM_CHAR`、
    /// `insertText:`、X11 的 `key_char`）不经 `on_key_down`，它落
    /// [`TerminalView::commit_text`]，那一刀在那里；`accepts_text_input` 再关掉
    /// 输入法上下文，让断连的终端连候选窗都不弹。
    alive: bool,

    /// 输入法预编辑串（还没上屏的组合文本）。
    ///
    /// 由 [`EntityInputHandler::replace_and_mark_text_in_range`] 写入，
    /// 提交 / 取消 / 断连 / 失焦时清空。**绝不进 PTY** —— 组合中的字符还没定，
    /// 送进去等于让 shell 先执行半截命令；它只画在光标处给人看
    /// （见 `render.rs::paint_preedit`）。
    ///
    /// 它同时是「键盘当前归输入法所有」的标志：非空时 [`TerminalView::on_key_down`]
    /// 一个字节都不发（否则组合中按回车确认候选词会给 shell 多送一个 `\r`）。
    preedit: String,

    /// 被粘贴闸拦下的待确认请求（F-5）：`Some` 时**一个字节都还没写**，
    /// 等宿主把它换成 [`TerminalView::confirm_paste`] 或 [`TerminalView::cancel_paste`]。
    ///
    /// 同一时刻最多一个：确认框是模态的，第二次粘贴在第一扇关掉前进不来
    /// （新的 `request_paste` 会**顶掉**旧的 —— 用户按了第二次，想要的是第二份
    /// 内容，把旧的留在框里等于让他确认一份自己已经放弃的文本）。
    paste_pending: Option<PasteRequest>,

    /// OSC 52 读取回调（宿主提供剪贴板文本）
    clipboard_load_callback: Option<ClipboardLoadCallback>,
}

/// 光标所在行的已输入文本（[`TerminalView::current_input_line`] 的产物）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputLine {
    /// 已输入文本（**已剥掉前导 prompt**，见 `strip_prompt`）。
    pub text: String,
    /// 光标在 `text` 内的**字符**下标（0 = 行首）。
    ///
    /// 用字符而非字节：补全要做前缀切片，而中文 / emoji 的字节偏移与视觉位置
    /// 完全对不上（一个汉字 3 字节、一个 emoji 4 字节）。`String::chars()` 的
    /// 下标才是用户看到的"第几个字"。
    pub cursor_col: usize,
    /// 光标行在**视口坐标系**里的行号（`Line(0)` = 视口首行）。
    ///
    /// 给宿主用于把浮层定位到光标行上用（C-1 暂不需要，C-2 行内提示要用）。
    pub viewport_row: usize,
    /// 被剥掉的 prompt 宽度（字符数）。
    ///
    /// 保留它有两个用途：① 宿主若要把补全结果画回**网格坐标**，需要加回这个偏移；
    /// ② 排障时能一眼看出"prompt 被识别成了几个字符"（H1 猜错时就是它不对）。
    pub prompt_cols: i32,
}

/// H1 启发式：把行首空白当作 prompt，切出后面的命令文本。
///
/// 返回 `(prompt 字符数, 命令文本)`。**prompt 宽度按"行首连续空白"算** ——
/// 这与"取第一个非空白字符之前的全部内容"等价，但实现上顺手得多。
///
/// 注意两个刻意的选择：
/// - `text` **不做 trim 右侧**：用户可能刚敲了一个空格，那是真实输入，
///   去掉会让补全前缀对不上（粘回 PTY 时少一个空格）。
/// - 返回的文本保留**行内的空白**（含多个空格）：命令中间的空格是用户打的。
///
/// 若整行都是空白，返回 `(整行宽度, 空串)` —— 此时"当前输入"就是空的，
/// 与"刚敲完回车、prompt 后面什么都没有"是同一个状态。
pub fn strip_prompt(line: &str) -> (i32, String) {
    let prefix_len = line.chars().take_while(|c| *c == ' ' || *c == '\t').count();
    // 剥掉的行尾空白**不**还原成原样：整行空白时，文本就是空串。
    let rest: String = line.chars().skip(prefix_len).collect();
    (prefix_len as i32, rest)
}

/// 编译后的搜索状态（正则可复用：search_next 无游标状态，origin 由调用方给）
pub struct SearchState {
    pub text: String,
    pub regex: alacritty_terminal::term::search::RegexSearch,
    /// 查询串版本号（每次 `set_search` 递增）。视口命中区间缓存（[`SearchKey`]）
    /// 用它判断「是否换了查询串」—— 比每帧重算字符串哈希便宜也更好推理。
    pub ver: u64,
}

/// 查询串版本号的全局发号器。
///
/// 用静态计数器而不是 `TerminalView` 字段：`set_search` 拿的是 `&self`
/// （多处共享调用），加字段就得改成 `&mut self`，把借用面铺开。
static SEARCH_QUERY_VER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 视口搜索命中区间的缓存键。
///
/// 四者全同 ⇒ 上一帧算出来的区间这一帧仍然有效，直接复用。
/// 这是「查找打开时终端卡顿」的第二道修复（第一道见
/// [`collect_search_spans`] 的 `scan_limit`）：命中区间算一次要扫视口，
/// 而渲染路径每帧都问一次 —— 终端静止时本可以一次都不算。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct SearchKey {
    /// 查询串版本（[`SearchState::ver`]）
    query: u64,
    /// 回滚偏移（滚动即失效）
    offset: i32,
    cols: usize,
    rows: usize,
    /// 终端内容版本（每批 PTY 字节 +1）
    content: u64,
}

/// 视口命中区间缓存（搜索区间 + 它对应的失效键）。
///
/// 抽成类型别名而不是把 `Arc<Mutex<Option<(..)>>>` 写在字段上：
/// 那串嵌套泛型已经超出 `clippy::type_complexity` 的阈值，而把它写全
/// 对读代码的人也没有任何额外信息 —— 「缓存了一份带键的区间」就是全部语义。
type SearchSpansCache = Arc<parking_lot::Mutex<Option<(SearchKey, Vec<crate::render::PaintSpan>)>>>;

/// 视口内的搜索命中区间（按行升序，行为视口行号）。
///
/// 交给渲染器画在字形**之下** —— 画在上面会把命中的文字糊住
/// （2026-09-16 用户报「匹配到的文字被遮蔽」）。
///
/// # 为什么每一帧都调它却不会卡
///
/// `Term::search_next` 的最后一个参数是**向后最多搜索多少行**（`max_lines`），
/// 传 `None` 等于「一直搜到 scrollback 末尾」。本函数跑在渲染路径上，
/// 视口 60 行 ⇒ 每帧最多 62 次调用，每次 `None` 都对整条历史（默认上万行）
/// 跑一遍正则 DFA —— 实测表现就是「一打开查找终端就卡死」。
///
/// 两道修复叠加：
/// 1. 每次调用只给「origin → 视口底部」的行数上限（`limit`），把单次扫描
///    从 O(全部历史) 压到 O(视口)，整体从 O(视口 × 历史) 压到 O(视口)；
/// 2. 调用方按 [`SearchKey`] 缓存结果，终端静止时每帧零成本。
fn collect_search_spans(
    term: &alacritty_terminal::term::Term<crate::event::GpuiEventProxy>,
    regex: &mut alacritty_terminal::term::search::RegexSearch,
) -> Vec<crate::render::PaintSpan> {
    use alacritty_terminal::grid::Dimensions as _;
    use alacritty_terminal::index::{Column, Direction, Line, Point, Side};

    let mut out = Vec::new();
    let d = term.grid().display_offset() as i32;
    let rows_i = term.screen_lines() as i32;
    let cols_i = term.columns() as i32;
    let bottom_abs = rows_i - 1 - d;

    let mut origin = Point::new(Line(-d), Column(0));
    // 上限：视口行数 + 少量余量（匹配可跨行），防异常正则卡死
    for _ in 0..(rows_i.max(4) + 2) {
        // 本次只需覆盖 origin → 视口底这一段（+2 行余量容纳跨行匹配）
        let limit = (bottom_abs - origin.line.0).max(1) as usize + 2;
        let Some(m) = term.search_next(regex, origin, Direction::Right, Side::Left, Some(limit))
        else {
            break;
        };
        let (s, e) = (*m.start(), *m.end());
        if s.line.0 > bottom_abs {
            break; // 已越过视口底部
        }
        for line in s.line.0..=e.line.0 {
            let v = line + d;
            if v < 0 || v >= rows_i {
                continue;
            }
            let cs = if line == s.line.0 { s.column.0 } else { 0 };
            let ce = if line == e.line.0 {
                e.column.0
            } else {
                (cols_i - 1).max(0) as usize
            };
            if ce < cs {
                continue;
            }
            out.push(crate::render::PaintSpan {
                row: v as usize,
                col_start: cs,
                col_end: ce,
            });
        }
        // 推进到匹配末尾之后（跨行时换行继续）
        origin = if e.column.0 + 1 >= term.columns() {
            Point::new(Line(e.line.0 + 1), Column(0))
        } else {
            Point::new(e.line, Column(e.column.0 + 1))
        };
    }
    out
}

/// ★ 本地扩展（RustShell）：实测单元格尺寸（paint 时写入）。
///
/// 用具名字段而不是 `(Pixels, Pixels)`：元组两个元素同类型，析构写反了编译器
/// 一声不吭。2026-09-17 的缺陷就是这么来的 —— `scroll_metrics` 把 `(宽, 高)`
/// 拆成了 `(高, _)`，`cell_height` 拿到字宽，滚动条滑块按错比例换算后被推出
/// 轨道之外整条裁掉（界面上表现为「有回滚内容却看不到滑动轴」）。
#[derive(Clone, Copy, Debug, PartialEq)]
struct CellMetrics {
    width: Pixels,
    height: Pixels,
}

impl CellMetrics {
    const ZERO: Self = Self {
        width: Pixels::ZERO,
        height: Pixels::ZERO,
    };
}

/// ★ 本地扩展（RustShell）：滚动几何快照（见 [`TerminalView::scroll_metrics`]）
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TermScrollMetrics {
    /// 可回滚的历史行数（0 = 未超过一屏 → 不显示滚动条）
    pub history_size: usize,
    /// 视口行数
    pub screen_lines: usize,
    /// 当前回滚偏移（0 = 紧跟输出底部）
    pub display_offset: usize,
    /// 单元格像素高
    pub cell_height: Pixels,
}

/// 双击选词的分隔符（除空白外额外排除的标点；单字符词也允许选中）
const WORD_SEPARATORS: &[char] = &[
    ' ', '\t', '\0', '"', '\'', '(', ')', '+', ',', ';', '<', '>', '[', ']', '{', '}', '|',
];

/// 连击判定窗口（ms）
const MULTI_CLICK_INTERVAL_MS: u128 = 500;

// ---- PTY 输出解析的两个预算（P0-1，见 docs/性能与帧率优化分析-不含AI-2026-09-19.md）----
//
// 放在模块级而不是 async 块里，是为了能被单测钉住相互关系：`MAX_PARSE_BYTES`
// 一旦被调到 ≥ `MAX_PENDING`，单批预算就等于没有（退回「一次吃下整个积压」），
// 而这件事在运行期完全看不出来 —— 只是又开始卡而已。

/// 帧窗口节流：两轮解析之间至少让出这么久
const FRAME_MS: u64 = 16;
/// `pending` 上限：超限时丢旧保新（终端本就该截断，不能无限增长）
const MAX_PENDING: usize = 1024 * 1024;
/// 单轮最多解析的字节数（= `process_bytes` 一次调用、一次 `term` 持锁的上限）。
///
/// 取 128KB：在「一次吃下整个积压」与「每 4KB 一块、让出过于频繁」之间取中。
/// 1MB 积压 ⇒ 8 轮，每轮之间让出一次，输出逐帧追赶、不丢内容。
pub(crate) const MAX_PARSE_BYTES: usize = 128 * 1024;
/// 单轮解析的**时间**兜底：字节上限之外，防的是「128KB 的病态内容」
/// （超长单行、极端转义序列）。超过就让出整个帧窗口。
const MAX_PARSE_US: u128 = 6_000;

impl TerminalView {
    /// Create a new terminal with provided I/O streams.
    ///
    /// This method initializes a new terminal emulator with the given stdin writer
    /// and stdout reader. It spawns a background task to read from stdout and
    /// process incoming bytes through the VTE parser.
    ///
    /// # Arguments
    ///
    /// * `stdin_writer` - Writer for sending input bytes to the terminal process
    /// * `stdout_reader` - Reader for receiving output bytes from the terminal process
    /// * `config` - Terminal configuration (dimensions, font, etc.)
    /// * `cx` - GPUI context for this view
    ///
    /// # Returns
    ///
    /// A new `TerminalView` instance ready to be rendered.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// // In a GPUI window context:
    /// let terminal = cx.new(|cx| {
    ///     TerminalView::new(stdin_writer, stdout_reader, TerminalConfig::default(), cx)
    /// });
    /// ```
    pub fn new<W, R>(
        stdin_writer: W,
        stdout_reader: R,
        config: TerminalConfig,
        cx: &mut Context<Self>,
    ) -> Self
    where
        W: Write + Send + 'static,
        R: Read + Send + 'static,
    {
        // Wrap stdin writer in Arc<Mutex> for thread-safe access
        //
        // ★ 必须**早于** event proxy 创建：事件代理要用同一份写端把 alacritty 的
        //   终端查询应答（`Event::PtyWrite` 等）写回 PTY。此前这行在 proxy 之后，
        //   proxy 无写端 → `Event::PtyWrite` 被丢弃 → PowerShell/ConPTY 启动时发出的
        //   `ESC[6n`（光标位置查询）永远收不到 `ESC[1;1R` 应答 → 阻塞、终端全黑。
        let stdin_writer = Arc::new(parking_lot::Mutex::new(
            Box::new(stdin_writer) as Box<dyn Write + Send>
        ));

        // ZMODEM 泵：与事件代理共享同一份写端（协议应答要直接写回 PTY）。
        // 落点目录由宿主给；没给就退回进程工作目录 —— 引擎不依赖平台路径库。
        let zmodem_dir = config
            .zmodem_download_dir
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        let zmodem = ZmodemPump::new(stdin_writer.clone(), zmodem_dir);

        // Create event channel for terminal events
        let (event_tx, event_rx) = mpsc::channel();

        // Clone event_tx for the reader task to send Exit event when PTY closes.
        // 这一份同时**留在视图字段里**（`exit_tx`）：重挂新会话时读端任务要换新的，
        // 但它投 `Exit` 必须走同一条通道（接收端 `event_rx` 换不了，也无需换）。
        let exit_event_tx = event_tx.clone();

        // shell integration 事件的专用通道（见字段注释）。
        // 与上面那条 alacritty 事件通道**并行**、互不干扰。
        let (shell_event_tx, shell_event_rx) = mpsc::channel();

        // 文本区像素尺寸（`CSI 14 t` 应答），渲染时按实测刷新
        let window_size = Arc::new(parking_lot::Mutex::new(WindowSize {
            num_lines: config.rows as u16,
            num_cols: config.cols as u16,
            cell_width: 0,
            cell_height: 0,
        }));

        // Create event proxy for alacritty（带 PTY 回写 + 调色板应答）
        let event_proxy = GpuiEventProxy::with_pty(
            event_tx,
            Some(stdin_writer.clone()),
            Some(color_lookup_from(&config.colors)),
            Some(window_size.clone()),
        );

        // Create terminal state
        let state = TerminalState::with_cursor_default(
            config.cols,
            config.rows,
            config.scrollback,
            config.default_cursor_shape,
            event_proxy,
        );

        // Create renderer with font settings and color palette
        let renderer = TerminalRenderer::new(
            config.font_family.clone(),
            config.font_size,
            config.line_height_multiplier,
            config.colors.clone(),
        );

        // Create focus handle
        let focus_handle = cx.focus_handle();

        // Create async channel for bytes (push-based notification)
        // Using flume instead of smol::channel because flume is executor-agnostic
        // and properly wakes GPUI's async executor when data arrives
        let (bytes_tx, bytes_rx) = flume::unbounded::<Vec<u8>>();

        // Spawn background thread to read from stdout
        // This thread sends bytes through the async channel
        let reader_encoding = config.encoding.clone();
        let reader_zmodem = zmodem.clone();
        thread::spawn(move || {
            Self::read_stdout_blocking(
                stdout_reader,
                bytes_tx,
                reader_encoding,
                Some(reader_zmodem),
            );
        });

        // Spawn async task that awaits on the channel and notifies the view
        //
        // ★ 方案 A：终端输出**节流合并**（见 docs/UI线程卡顿分析与优化方案.md §5-A）。
        //
        // 原实现每收到一个 4096B 块就立刻 `process_bytes`（VTE 解析 + 网格更新，持
        // `Mutex<Term>`）并 `cx.notify()`。洪流场景（大文件 cat / 日志流）下 UI 线程
        // 被「逐块解析 + 逐块重绘」占满，300ms 一次的 `MainView::poll` 心跳抢不到时间片
        // → 看门狗报「未响应」→ 系统判 AppHang。
        //
        // 现在改为**每帧窗口最多解析一次**：
        //   1. 先把通道里已排队的块全部 drain 进 `pending`（洪流时一次并掉全部积压）；
        //   2. 一次性 `process_bytes` + 一次 `cx.notify()`；
        //   3. 睡一个帧窗口（16ms），把时间片还给 `poll` 心跳。
        // 输出延迟增加 ≤16ms，交互无感；EOF（通道关闭）仍**立即**处理，不被窗口延迟。
        //
        // ★ 2026-09-21 补第二重预算（`MAX_PARSE_BYTES` / `MAX_PARSE_US`，见
        // docs/性能与帧率优化分析-不含AI-2026-09-19.md 的 P0-1）：
        // 上面那条节流只管「多久解析一次」，**没管「一次解析多少」**。
        // 洪流时一轮就能并到 `MAX_PENDING`（1MB），这 1MB 的 VTE 解析 + 网格改写
        // 全在 UI 线程上同步跑完，并且全程持有 `term` 锁 —— 单次阻塞时长因此与
        // 「积压量」成正比，而不是与「这一帧愿意花多少时间」成正比。
        // 现在每一轮只解析一块，剩余留在 `pending` 里下一轮继续：
        // 持锁窗口由「≤1MB」变成「≤MAX_PARSE_BYTES」，且每块之间都会 await 让出。
        // 常量在模块级，见文件下方的 `MAX_PARSE_BYTES`。
        // 视图字段还要留一份（`exit_tx`），故这里克隆一份给任务
        let reader_task =
            Self::spawn_reader(bytes_rx, exit_event_tx.clone(), shell_event_tx.clone(), cx);

        Self {
            state,
            renderer,
            focus_handle,
            stdin_writer,
            zmodem,
            event_rx,
            exit_tx: exit_event_tx,
            window_size,
            config,
            _reader_task: reader_task,
            resize_callback: None,
            key_handler: None,
            bell_callback: None,
            title_callback: None,
            clipboard_store_callback: None,
            clipboard_load_callback: None,
            exit_callback: None,
            shell_marks: Vec::new(),
            shell_cwd: None,
            shell_command_line: None,
            shell_mark_callback: None,
            shell_cwd_callback: None,
            shell_command_line_callback: None,
            input_line_callback: None,
            input_line_dirty: false,
            enter_callback: None,
            enter_dirty: false,
            completion_key_filter: None,
            shell_event_tx,
            shell_event_rx,
            content_bounds: Rc::new(RefCell::new(Bounds::default())),
            cell_metrics: Rc::new(StdCell::new(CellMetrics::ZERO)),
            measured_for: Rc::new(RefCell::new(None)),
            render_scratch: Arc::new(parking_lot::Mutex::new(FrameScratch::default())),
            selection: Rc::new(RefCell::new(None)),
            last_click: None,
            click_count: 1,
            selecting: false,
            search: Arc::new(parking_lot::Mutex::new(None)),
            search_last_hit: Arc::new(parking_lot::Mutex::new(None)),
            search_spans_cache: Arc::new(parking_lot::Mutex::new(None)),
            content_gen: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            blink_on: true,
            blink_task: None,
            has_focus: false,
            // 新建即活着：宿主在会话终止时才置 false（见 `set_alive`）。
            alive: true,
            preedit: String::new(),
            paste_pending: None,
            // 初始不启动定时器：焦点尚未进入，`has_focus` 为 false，
            // 由渲染期的焦点变化检测按需启动（避免构造期无谓 spawn）。
        }
    }

    /// 起一个「读端 → 网格」的合并任务。
    ///
    /// 从 [`TerminalView::new`] 抽出来是为了让 [`TerminalView::reattach`] 复用**同一份**
    /// 节流逻辑：重挂新会话换的只是字节来源，解析节奏、积压上限、让出策略都必须一模一样。
    ///
    /// 契约（三条都由调用方保证）：
    /// - `bytes_rx` 的发送端由**后台读线程**持有（见 [`TerminalView::read_stdout_blocking`]）；
    /// - 通道关闭 = PTY 结束 ⇒ 抽干积压后投一次 `TerminalEvent::Exit`（会话结束的回执，
    ///   视图不直接调回调，因为 `exit_callback` 需要 `Window`）；
    /// - 返回的 `Task` 一 drop 即取消 ⇒ 重挂时旧任务连同它的节流定时器一起作废。
    ///
    /// `shell_event_tx` 是 shell integration 事件的出口（见 [`TerminalView`] 字段注释）。
    /// 它跟着**视图**走而不是跟着任务走：任务是每次重挂都要换的，出口不换。
    fn spawn_reader(
        bytes_rx: flume::Receiver<Vec<u8>>,
        exit_event_tx: mpsc::Sender<TerminalEvent>,
        shell_event_tx: mpsc::Sender<TerminalEvent>,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            let mut pending: Vec<u8> = Vec::new();
            let mut closed = false;

            loop {
                // 无积压时才阻塞等待；有积压直接进入合并 + 解析
                if pending.is_empty() {
                    match bytes_rx.recv_async().await {
                        Ok(bytes) => pending = bytes,
                        Err(_) => closed = true,
                    }
                }

                // 把通道里已排队的块一次性并入（`try_recv` 清空积压）
                if !closed {
                    while let Ok(more) = bytes_rx.try_recv() {
                        let total = pending.len() + more.len();
                        if total > MAX_PENDING {
                            let drop_n = (total - MAX_PENDING).min(pending.len());
                            pending.drain(0..drop_n);
                        }
                        pending.extend_from_slice(&more);
                        if pending.len() >= MAX_PENDING {
                            break;
                        }
                    }
                }

                let had_bytes = !pending.is_empty();
                // 这一轮实际解析了多久（决定让出多少时间片）
                let mut parse_us: u128 = 0;
                if had_bytes {
                    // 只取一块：余下的留在 `pending` 里，下一轮接着解析（顺序不变）。
                    // VTE 解析器是**状态机**：在任意字节边界切开，与「分多次收到小
                    // 块」完全等价 —— PTY 本来就按 4KB 分块投递，这里并不引入新语义。
                    let n = pending.len().min(MAX_PARSE_BYTES);
                    let bytes: Vec<u8> = pending.drain(..n).collect();
                    // 解析前先把出口克隆出来：闭包要 `FnOnce` + 不借用 `view`，
                    // 而 `shell_event_tx` 是 `Send` 的克隆体，正好可以 move 进去。
                    let shell_tx = shell_event_tx.clone();
                    let started = std::time::Instant::now();
                    let result = this.update(cx, |view: &mut Self, cx: &mut Context<Self>| {
                        // ★ 走 `_with_events`：shell integration 的 OSC（133/633/7）
                        //   就是在这一次字节解析里被认出来的，必须带着出口进去，
                        //   否则事件在 `ShellIntegrationForwarder` 里被静默丢掉
                        //   （`tx: None` 分支），表现为"远端明明发了、视图一无所知"。
                        view.state.process_bytes_with_events(&bytes, &shell_tx);
                        // 网格内容变了 ⇒ 视口命中区间缓存失效（见 `SearchKey`）
                        view.content_gen
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        // shell integration 事件走的是**另一条**通道，`cx.notify()`
                        // 只保证下一帧跑 `render`，而事件要等 `process_events` 来抽 ——
                        // 所以这里不能只依赖 notify：必须紧接着自己抽一次，
                        // 否则同一个 PromptMark 要等到下一次 notify 才被看见，
                        // 表现为"补全浮层慢半拍/偶尔不出现"。
                        view.drain_shell_events();
                        cx.notify();
                    });
                    parse_us = started.elapsed().as_micros();
                    if result.is_err() {
                        // View was dropped, exit
                        break;
                    }
                }

                // 通道已关闭（PTY 结束）⇒ 积压抽干后立即投递 Exit，不进入节流窗口。
                // 「抽干后才投递」是 P0-1 带出来的新要求：一轮只解析一块之后，
                // 关通道那一刻 `pending` 里往往还剩着尾部输出（EOF 前的最后一块
                // 常常就是提示符），照旧直接 break 会把它丢掉。
                if closed && pending.is_empty() {
                    let _ = exit_event_tx.send(TerminalEvent::Exit);
                    let _ = this.update(cx, |_view, cx: &mut Context<Self>| {
                        cx.notify();
                    });
                    break;
                }

                // 帧窗口节流：只在真的处理过数据后才等待，空闲时保持零开销。
                // 仍有积压时只做一次**让出**（1ms），不睡满整帧 —— 否则 1MB 积压要
                // 8×16ms 才追得上，用户看到的是「输出慢半拍」。
                if had_bytes {
                    let yield_ms = if !pending.is_empty() && parse_us < MAX_PARSE_US {
                        1
                    } else {
                        FRAME_MS
                    };
                    cx.background_executor()
                        .timer(Duration::from_millis(yield_ms))
                        .await;
                }
            }
        })
    }

    /// 把**新会话**的 I/O 接到本视图上：屏上的东西一个字节都不动，只换脚下的管道。
    ///
    /// 这是「重连（远端注销 / 空闲超时 / 网络中断之后）」的引擎侧落点，
    /// 与 [`TerminalView::new`] 的分工是全部的差别所在：
    ///
    /// | | `new` | `reattach` |
    /// |---|---|---|
    /// | 网格 / 回滚缓冲 | 空 | **原样保留** |
    /// | 划选、搜索、滚动位置 | 无 | **原样保留** |
    /// | 写端 / 读端 | 入参 | 入参（旧的作废） |
    ///
    /// 为什么必须复用同一块屏而不是「新建一块再把文字倒进去」：终端内容不是纯文本 ——
    /// 颜色、属性、宽字符占位、光标处在一个被覆盖过的行中间，这些状态**没有**可回灌的
    /// 表示形式；把网格重建一遍必然失真，而「保留原内容」这件事恰恰是用户能一眼看穿的。
    ///
    /// 重挂要做的四件事（少一件都会留下一个只在实机出现的坑）：
    ///
    /// 1. **换写端**：只换 `Box<dyn Write>`，**不换那把 `Arc<Mutex>`** —— alacritty 的
    ///    事件代理（`OSC 52` 应答、`CSI 14 t` 光标位置应答）持的是同一份 `Arc` 克隆，
    ///    据此才能直接写进**新** PTY。
    /// 2. **换读端**：新线程 + 新合并任务（旧任务一 drop 即取消）。
    /// 3. **换尺寸回调**：尺寸回调指回新会话的 PTY；并**主动推一次当前网格尺寸** ——
    ///    渲染期那条回调只在「测出的网格 ≠ 当前网格」时才发，而重挂时网格**没变**
    ///    （面板大小没动），不补这一推，新 PTY 会一直以为自己还是 80×24，
    ///    换行位置从此全错（症状是「重连后输出比窗口窄一截」）。
    /// 4. **复位旧会话留下的终端模式**：见 [`Self::reset_modes_after_session_change`]。
    ///
    /// `notice` 是给用户看的分隔行（宿主按自己的语言给文本，引擎不掺 i18n）；
    /// 传 `None` 就只留一行空行。分隔行**必须在复位之后**写 —— 复位里有「退回主屏」，
    /// 写在前面会被一起丢在备用屏上。
    ///
    /// 会话生命期也跟着重开：内部会调 `set_alive(true)`，把断连时封死的三条输入通路
    /// 一起打开（输入闸门与画布是同一块屏的两个状态，不能只开一个）。
    pub fn reattach<W, R>(
        &mut self,
        stdin_writer: W,
        stdout_reader: R,
        resize_callback: Option<ResizeCallback>,
        notice: Option<&str>,
        cx: &mut Context<Self>,
    ) where
        W: Write + Send + 'static,
        R: Read + Send + 'static,
    {
        // ---- 1) 写端：原地换 Box，保留那份 Arc（事件代理与这里共享）----
        *self.stdin_writer.lock() = Box::new(stdin_writer);

        // ---- 2) 读端：新线程 + 新合并任务 ----
        // 旧会话上的 ZMODEM 已在断连时中止；这里再复位一次，确保新会话从干净状态开始
        // （泵与新读线程共用，不复位会把旧传输的半截状态带进新连接）。
        self.zmodem.reset();
        let (bytes_tx, bytes_rx) = flume::unbounded::<Vec<u8>>();
        let encoding = self.config.encoding.clone();
        let zmodem = self.zmodem.clone();
        thread::spawn(move || {
            Self::read_stdout_blocking(stdout_reader, bytes_tx, encoding, Some(zmodem));
        });
        // 直接赋值即「旧任务被 drop」⇒ 它的节流定时器与残留 `pending` 一并作废。
        // 顺序要紧：先把新任务装上，再谈别的（否则此间到达的字节会积在无人消费的通道里）。
        self._reader_task = Self::spawn_reader(
            bytes_rx,
            self.exit_tx.clone(),
            self.shell_event_tx.clone(),
            cx,
        );

        // ---- 3) 尺寸回调改指新会话，并把当前网格尺寸补推一次 ----
        self.resize_callback = resize_callback.map(Arc::new);
        if let Some(callback) = self.resize_callback.as_ref() {
            let (cols, rows) = (self.state.cols(), self.state.rows());
            callback(cols, rows);
        }

        // ---- 4) 复位旧模式 + 分隔行 + 重开会话生命期 ----
        self.reset_modes_after_session_change();
        match notice {
            Some(text) => {
                // 前后各一个换行：旧会话最后一行可能没换行（被掐在行中间），
                // 新 shell 的首个提示符也不会自带前导换行 —— 两头都得我们给。
                self.state
                    .process_bytes(format!("\r\n{text}\r\n").as_bytes());
            }
            None => self.state.process_bytes(b"\r\n"),
        }
        self.content_gen
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.set_alive(true, cx);
        cx.notify();
    }

    /// 清掉「旧会话开的、新会话没开」的终端模式。
    ///
    /// 这些位住在 `Term` 里，而**新会话对它一无所知**：它以为自己是接管了一块干净的屏。
    /// 不清理的话，症状全是「重连之后怪怪的」这类没有栈可查的东西：
    ///
    /// | 残留 | 症状 |
    /// |---|---|
    /// | 备用屏（vim / less / htop 里掉线） | 新 shell 的输出全画进那块「上层还留着 vim」的屏幕 |
    /// | 鼠标上报（`?1000/1002/1003`） | 点一下就往新 shell 里灌一串转义序列 |
    /// | `?1006` SGR 鼠标 / `?1015` urxvt 鼠标 | 同上（编码格式还不同） |
    /// | `?2004` 粘贴包裹 | 粘贴多行命令被新 shell 当成「一次性粘贴」拒收或误解析 |
    /// | `?1`（DECCKM）应用光标键 | 方向键发出 `ESC O A` 而不是 `ESC [ A` |
    /// | `?1004` 焦点事件 | 切窗口时往新 shell 投一串 `ESC[I`/`ESC[O` |
    ///
    /// 只发「关闭」序列，不发任何清屏/复位过的序列（`RIS`/`clear` 一类会让本条需求
    /// 「不要清除原连接的内容」直接落空）—— 这些序列不动网格里的一个字。
    fn reset_modes_after_session_change(&mut self) {
        // 备用屏要单独判：`?1049l` 落在主屏上会白切一次缓冲（光标与保存的位置对不上）
        if self
            .state
            .mode()
            .contains(alacritty_terminal::term::TermMode::ALT_SCREEN)
        {
            self.state.process_bytes(b"\x1b[?1049l");
        }
        self.state.process_bytes(
            // 鼠标上报三连 + SGR/urxvt 鼠标编码
            b"\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1015l\
              \x1b[?1004l\x1b[?2004l\x1b[?1l\x1b>\x1b[0m\x1b[?25h",
        );
    }

    /// Set a callback to be invoked when the terminal is resized.
    ///
    /// This callback should resize the underlying PTY to match the new dimensions.
    /// The callback receives (cols, rows) as arguments.
    ///
    /// # Arguments
    ///
    /// * `callback` - A function that will be called with (cols, rows) on resize
    pub fn with_resize_callback(
        mut self,
        callback: impl Fn(usize, usize) + Send + Sync + 'static,
    ) -> Self {
        self.resize_callback = Some(Arc::new(Box::new(callback)));
        self
    }

    /// Set a callback to intercept key events before terminal processing.
    ///
    /// The callback receives the key event and should return `true` to consume
    /// the event (prevent the terminal from processing it), or `false` to allow
    /// normal terminal processing.
    ///
    /// # Arguments
    ///
    /// * `handler` - A function that receives key events and returns whether to consume them
    ///
    /// # Example
    ///
    /// ```ignore
    /// terminal.with_key_handler(|event| {
    ///     // Handle Ctrl++ to increase font size
    ///     if event.keystroke.modifiers.control && event.keystroke.key == "+" {
    ///         // Handle the event
    ///         return true; // Consume the event
    ///     }
    ///     false // Let terminal handle it
    /// })
    /// ```
    pub fn with_key_handler(
        mut self,
        handler: impl Fn(&KeyDownEvent) -> bool + Send + Sync + 'static,
    ) -> Self {
        self.key_handler = Some(Arc::new(Box::new(handler)));
        self
    }

    /// Set a callback to be invoked when the terminal bell is triggered.
    ///
    /// The callback receives a mutable reference to the window and context,
    /// allowing you to play a sound or show a visual indicator.
    ///
    /// # Arguments
    ///
    /// * `callback` - A function that will be called when the bell is triggered
    ///
    /// # Example
    ///
    /// ```ignore
    /// terminal.with_bell_callback(|window, cx| {
    ///     // Play a sound or flash the screen
    /// })
    /// ```
    pub fn with_bell_callback(
        mut self,
        callback: impl Fn(&mut Window, &mut Context<TerminalView>) + 'static,
    ) -> Self {
        self.bell_callback = Some(Box::new(callback));
        self
    }

    /// Set a callback to be invoked when the terminal title changes.
    ///
    /// The callback receives a mutable reference to the window and context,
    /// along with the new title string.
    ///
    /// # Arguments
    ///
    /// * `callback` - A function that will be called with the new title
    ///
    /// # Example
    ///
    /// ```ignore
    /// terminal.with_title_callback(|window, cx, title| {
    ///     // Update window title or tab title
    /// })
    /// ```
    pub fn with_title_callback(
        mut self,
        callback: impl Fn(&mut Window, &mut Context<TerminalView>, &str) + 'static,
    ) -> Self {
        self.title_callback = Some(Box::new(callback));
        self
    }

    /// Set a callback to be invoked when the terminal wants to store data to the clipboard.
    ///
    /// The callback receives a mutable reference to the window and context,
    /// along with the text to store. This is typically triggered by OSC 52 escape sequences.
    ///
    /// # Arguments
    ///
    /// * `callback` - A function that will be called with the text to store
    ///
    /// # Example
    ///
    /// ```ignore
    /// terminal.with_clipboard_store_callback(|window, cx, text| {
    ///     // Store text to system clipboard
    /// })
    /// ```
    pub fn with_clipboard_store_callback(
        mut self,
        callback: impl Fn(&mut Window, &mut Context<TerminalView>, &str) + 'static,
    ) -> Self {
        self.clipboard_store_callback = Some(Box::new(callback));
        self
    }

    /// Set a callback that supplies the system clipboard text for OSC 52 reads.
    ///
    /// 远端程序用 `OSC 52 ; c ; ?` 请求读剪贴板时调用；返回 `None` 表示不提供。
    /// 应答序列由 alacritty 的格式化器拼装后写回 PTY（见 `ClipboardLoad` 事件）。
    ///
    /// # Example
    ///
    /// ```ignore
    /// terminal.with_clipboard_load_callback(|_window, cx| {
    ///     cx.read_from_clipboard().and_then(|item| item.text())
    /// })
    /// ```
    pub fn with_clipboard_load_callback(
        mut self,
        callback: impl Fn(&mut Window, &mut Context<TerminalView>) -> Option<String> + 'static,
    ) -> Self {
        self.clipboard_load_callback = Some(Box::new(callback));
        self
    }

    /// 订阅 shell integration 的命令生命周期标记（`OSC 133` / `633`）。
    ///
    /// 只在远端主动发这些序列时触发。本回调与网格渲染**完全解耦** ——
    /// 终端画面不会因为远端发了这些序列而有任何变化。
    pub fn with_shell_mark_callback(
        mut self,
        callback: impl Fn(&mut Window, &mut Context<TerminalView>, crate::shell_integration::PromptMark)
        + 'static,
    ) -> Self {
        self.shell_mark_callback = Some(Box::new(callback));
        self
    }

    /// 订阅 `OSC 7` 工作目录变更。⚠️ 参数是**远端**路径。
    pub fn with_shell_cwd_callback(
        mut self,
        callback: impl Fn(&mut Window, &mut Context<TerminalView>, &str) + 'static,
    ) -> Self {
        self.shell_cwd_callback = Some(Box::new(callback));
        self
    }

    /// 订阅 `OSC 633;E` 给出的准确命令行文本。
    pub fn with_shell_command_line_callback(
        mut self,
        callback: impl Fn(&mut Window, &mut Context<TerminalView>, &str) + 'static,
    ) -> Self {
        self.shell_command_line_callback = Some(Box::new(callback));
        self
    }

    /// 订阅「当前输入行可能变了」（见 [`InputLineCallback`]）。
    ///
    /// 宿主在回调里按需调用 `tv.current_input_line()` 取当前行 —— 参数里的
    /// `&TerminalView` 就是引擎借出去的那一份，**不要**再去 `read` 实体。
    /// 注：本回调只报"可能变了"，**不保证**取到的文本与上一次不同 ——
    /// 去重是宿主的事（浮层只需重算候选、比对查询串）。
    pub fn with_input_line_callback(
        mut self,
        callback: impl Fn(&TerminalView, &mut Window, &mut Context<TerminalView>) + 'static,
    ) -> Self {
        self.input_line_callback = Some(Box::new(callback));
        self
    }

    /// 订阅「用户按了回车」（见 [`EnterCallback`]）。
    ///
    /// 宿主在回调里按需调用 `tv.current_input_line()` 取那一行，判要不要
    /// 记进命令历史。与 [`Self::with_input_line_callback`] 的分工：
    /// 那个报"行变了"（驱动候选刷新），这个报"行被执行了"（驱动历史采集）。
    pub fn with_enter_callback(
        mut self,
        callback: impl Fn(&TerminalView, &mut Window, &mut Context<TerminalView>) + 'static,
    ) -> Self {
        self.enter_callback = Some(Box::new(callback));
        self
    }

    /// 装补全浮层的按键过滤器（见 [`CompletionKeyFilter`]）。
    ///
    /// 与 [`Self::with_key_handler`] 的分工：那个是**应用级**的按键钩子（在 IME
    /// 之后、字节翻译之前，返回 `bool` 表示是否消费），本过滤器**只**判
    /// 补全浮层相关的键，且判据里要用引擎自己的状态（`preedit` / 备用屏）。
    /// 两者可以同时装；本过滤器在 `key_handler` **之前**判 —— 浮层开着时
    /// 它优先，关着时一律 [`CompletionKeyOutcome::PassThrough`]（等于不存在）。
    pub fn with_completion_key_filter(
        mut self,
        filter: impl Fn(
            &KeyDownEvent,
            &TerminalView,
            &mut Window,
            &mut Context<TerminalView>,
        ) -> CompletionKeyOutcome
        + 'static,
    ) -> Self {
        self.completion_key_filter = Some(Box::new(filter));
        self
    }

    /// 最近收到的命令生命周期标记（只读快照）。
    pub fn shell_marks(&self) -> &[crate::shell_integration::PromptMark] {
        &self.shell_marks
    }

    /// 最近一次 `OSC 7` 上报的工作目录（**远端**路径）。
    pub fn shell_cwd(&self) -> Option<&str> {
        self.shell_cwd.as_deref()
    }

    /// 最近一次 `OSC 633;E` 给出的命令行文本。
    ///
    /// 与"从网格里推断当前行"是**两条独立的路**：这一条只在远端支持
    /// `633;E` 时才有值，但拿到就是精确的；那一条永远可用但要猜 prompt 边界。
    /// 后续阶段应当优先用它。
    pub fn shell_command_line(&self) -> Option<&str> {
        self.shell_command_line.as_deref()
    }

    /// 该会话是否正在进行 shell integration（收到过任何一条相关序列）。
    ///
    /// 用途：宿主可以据此决定"要不要展示依赖精确 prompt 边界的功能"。
    /// 返回 `false` **不代表出错**，只代表远端没发 —— 大量环境本来就不发。
    pub fn shell_integration_active(&self) -> bool {
        !self.shell_marks.is_empty()
            || self.shell_cwd.is_some()
            || self.shell_command_line.is_some()
    }

    /// 取光标所在行的**已输入文本**（C-1/C-2 的上下文来源）。
    ///
    /// # 返回
    ///
    /// `None` 表示**现在不适合补全**，调用方必须据此不弹浮层。三种情况：
    /// - 会话已死（断连态的终端是只读的，补全写不进去）；
    /// - 备用屏（`ALT_SCREEN`：vim / less / htop 里"当前行"没有补全语义）；
    /// - 光标**不在视口内**（用户正翻着历史看）：此时行文本读出来也不是
    ///   用户眼前的那个位置，拿它算候选只会给出风马牛不相及的结果。
    ///
    /// # prompt 边界（本层唯一需要"猜"的地方）
    ///
    /// 返回的 `text` 已经**剥掉前导 prompt**，用的是启发式 H1（调研文档 §3.2）：
    /// 取行首第一个非空白字符之前的全部内容为 prompt。绝大多数
    /// `user@host:~$ ` 形态下成立。
    ///
    /// H1 会失效的场景（多行 prompt、带换行的 `PS1`、`zsh` RPROMPT）在 C-0 之后有
    /// **更准的路**：`OSC 633;E` 直接给命令行文本（见 [`Self::shell_command_line`]）。
    /// 调用方应当**优先**用那条，本方法只作兜底 —— 所以它给的文本是"够用"而非"精确"。
    ///
    /// # 与远端 readline 的关系
    ///
    /// 本方法是**纯读**：不改网格、不动光标、不发字节。补全把候选写回去走的是
    /// [`Self::write_to_pty`]（同死会话闸）。读与写分开，是为了让"看得见"这件事
    /// 无论用不用得上都不产生副作用。
    pub fn current_input_line(&self) -> Option<InputLine> {
        use alacritty_terminal::grid::Dimensions as _;
        use alacritty_terminal::index::{Column, Line, Point};

        if !self.alive {
            return None;
        }
        if self
            .state
            .mode()
            .contains(alacritty_terminal::term::TermMode::ALT_SCREEN)
        {
            return None;
        }

        self.state.with_term(|term| {
            let grid = term.grid();
            let offset = grid.display_offset() as i32;
            let cursor = grid.cursor.point;
            // 光标行在**视口坐标系**里的位置：`Line(0)` = 视口首行，
            // 与 `visible_text` / `selected_text` 同一坐标系（见那两处）。
            let viewport_row = cursor.line.0 + offset;
            let rows = grid.screen_lines() as i32;
            // 光标滚出视口（翻历史看的时候）：见文档的第三种 None
            if viewport_row < 0 || viewport_row >= rows {
                return None;
            }

            let vp_line = Line(viewport_row);
            let line_text = term.bounds_to_string(
                Point::new(vp_line, Column(0)),
                Point::new(vp_line, grid.last_column()),
            );
            let (prompt_cols, text) = strip_prompt(&line_text);
            // 光标列也要跟着减掉 prompt 宽度（否则补全的前缀会多出几个字符）
            let cursor_col = (cursor.column.0 as i32 - prompt_cols).max(0) as usize;
            let cursor_col = cursor_col.min(text.chars().count());

            Some(InputLine {
                text,
                cursor_col,
                viewport_row: viewport_row as usize,
                prompt_cols,
            })
        })
    }

    /// 会话是否还活着（宿主判定的会话生命期，不是「能不能连上」）。
    pub fn is_alive(&self) -> bool {
        self.alive
    }

    /// 置会话存活标记。宿主在会话终止（掉线 / 远端注销 / shell 退出）时置 `false`。
    ///
    /// 置 `false` 之后：键盘 / 粘贴 / 文本输入（IME）/ [`TerminalView::write_to_pty`]
    /// 四条输入通路全部静默丢弃，输入法上下文也随之关闭（[`Self::accepts_text_input`]），
    /// 光标不再绘制、闪烁定时器停表，点击也不再抢焦点 ——
    /// **画面、划选、复制、滚动全部保留**（断连现场是要给人看的）。
    ///
    /// 只在真的变化时才重绘与重置闪烁，避免宿主每帧无脑调用引起重绘。
    pub fn set_alive(&mut self, alive: bool, cx: &mut Context<Self>) {
        if self.alive == alive {
            return;
        }
        self.alive = alive;
        // 断连即停表并让光标停在「不画」那一相上（`render` 里也按 alive 兜了一刀，
        // 两处都判是因为定时器任务与渲染帧不同步）
        self.reset_cursor_blink(cx);
        if !alive {
            self.blink_on = false;
            // 会话没了，协议传输也不可能继续：复位泵，别让它在断连现场挂着一个
            // 永远等不到对端的状态（重连时 `reattach` 还会再复位一次）。
            self.zmodem.reset();
            // 组合到一半掉线：预编辑串留着也没人能提交它了，清掉。
            // 不清理的话它会一直画在断连现场的画面上，像一句没发出去的命令。
            self.preedit.clear();
            // 挂在确认框上的粘贴请求同理作废：宿主据此收框（挂起消失 = 没得可确认），
            // 也杜绝「对着死会话点确认」写出半截命令的想象。
            self.paste_pending = None;
        }
        cx.notify();
    }

    /// 通知宿主「当前输入行可能变了」（见 [`InputLineCallback`]）。
    ///
    /// **只置旗，不调回调**：`commit_text` / `paste_text` 手里没有 `Window`
    /// （回调签名要它），而多绕一层把 `Window` 传进来会把这两条**热路径**的
    /// 签名全铺开。兑现点放在 `process_events`（渲染期，那里正好有 `Window`）——
    /// 这是铁律 #28 的两段式：需要 `Window` 的延迟动作先置旗、`render` 兑现。
    ///
    /// 死会话不置旗：宿主此刻正在收断连现场（浮层该被清掉，而不是被刷新）。
    fn notify_input_line(&mut self, cx: &mut Context<Self>) {
        if !self.alive {
            return;
        }
        if self.input_line_callback.is_some() && !self.input_line_dirty {
            self.input_line_dirty = true;
            cx.notify();
        }
    }

    /// 当前是否应当运行闪烁定时器：用户开关 + 终端要求 + 持有焦点。
    ///
    /// 三者缺一即停表 —— 后台窗口每 530ms 空转重绘没有意义。
    /// 会话已死时同样停表（见 [`TerminalView::set_alive`]）。
    ///
    /// 「终端要求」每帧现读 [`TerminalState::cursor_blinking`]，不缓存：
    /// DECSET 12 与 DECSCUSR 两路来源分开记，读的是合并后的结果。
    fn blink_should_run(&self) -> bool {
        self.alive && self.config.cursor_blink && self.has_focus && self.state.cursor_blinking()
    }

    /// 重置闪烁相位并（重新）按需启动定时器。
    ///
    /// 相位归零后光标立刻可见，所以输入、滚动、焦点切换之后不会出现
    /// 「刚敲完键光标是黑的」这种观感问题 —— 主流终端也是这个行为。
    fn reset_cursor_blink(&mut self, cx: &mut Context<Self>) {
        // drop 旧 `Task` 即取消旧定时器
        self.blink_task = None;
        self.blink_on = true;

        if !self.blink_should_run() {
            cx.notify();
            return;
        }

        // 下限 50ms：防止配置成 0 变成满负载重绘
        let interval = Duration::from_millis(self.config.cursor_blink_interval_ms.clamp(50, 5000));
        self.blink_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(interval).await;
                let keep = this
                    .update(cx, |view, cx| {
                        // 条件失效（失焦 / 终端要求停止 / 用户关闭）时收表并常亮
                        if !view.blink_should_run() {
                            view.blink_on = true;
                            return false;
                        }
                        view.blink_on = !view.blink_on;
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !keep {
                    break;
                }
            }
        }));
        cx.notify();
    }

    /// Current cursor shape (empty for the default path).
    pub fn cursor_shape(&self) -> CursorShape {
        self.state.with_term(|t| t.cursor_style().shape)
    }

    /// 轻量重置闪烁相位：只把光标拉回「亮」，不重建定时器。
    ///
    /// 用于输入 / 滚动 / 点击这类**高频**路径 —— 重建 `Task` 有分配开销，
    /// 而且相位已是「亮」时连重绘都不需要（直接返回）。
    fn touch_cursor_blink(&mut self, cx: &mut Context<Self>) {
        if self.blink_on {
            return;
        }
        self.blink_on = true;
        cx.notify();
    }

    /// 把系统剪贴板内容粘贴到终端（Ctrl+Shift+V / Cmd+V / Shift+Insert 的落点）。
    ///
    /// 只负责「读剪贴板」这一半，读到就交给 [`Self::request_paste`] 过闸：
    /// 多行内容会先挂起等宿主确认，而不是直接写 —— 见那里的文档。
    ///
    /// `and_enter` 为 true 时末尾补一个回车，即「粘贴并执行」。
    pub fn paste_from_clipboard(&mut self, and_enter: bool, cx: &mut Context<Self>) {
        // 会话已死时连剪贴板都不读：省一次系统调用，也让「断连后粘贴」彻底无声
        if !self.alive {
            return;
        }
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        self.request_paste(&text, and_enter, cx);
    }

    /// 粘贴闸（F-5）：判断这次粘贴要不要先问用户，并把两个答案各自兑现。
    ///
    /// **这是所有用户粘贴入口的唯一通路**（Ctrl+Shift+V / Cmd+V / Shift+Insert、
    /// 输入法的剪贴板接口、UI 右键菜单），[`Self::paste_text`] 只是它放行之后的
    /// 底层写入。为什么闸设在引擎而不是宿主 UI：三个入口里有两个根本不出引擎
    /// （键绑与 `EntityInputHandler::paste`），装在外面等于最常用的那条路直接绕过
    /// —— 门禁全绿、功能静默失效。
    ///
    /// 判定用 [`needs_paste_confirm`]：换行数 `≥` 阈值（默认 1）即拦。被拦下的
    /// 请求**一个字节都不写**，挂在 [`Self::pending_paste`] 上等宿主弹框；
    /// 宿主答什么由 [`Self::confirm_paste`] / [`Self::cancel_paste`] 兑现。
    pub fn request_paste(&mut self, text: &str, and_enter: bool, cx: &mut Context<Self>) {
        if text.is_empty() {
            return;
        }
        // 会话已死：与 `paste_text` 同一道闸，但拦在更前面 —— 死会话连「待确认」
        // 都不该出现（宿主会拿它弹框，弹出来点确认也只会写进一个死管道）。
        if !self.alive {
            return;
        }
        // ZMODEM 传输中：粘贴会作为协议数据写进管道，把正在传的帧撕碎。
        // 拦在闸前（连「待确认」都不出现），符合「传输期间键盘不落管道」的约定。
        if self.zmodem.is_active() {
            return;
        }
        let newlines = paste_newline_count(text);
        let ask = self
            .config
            .paste_protect_lines
            .is_some_and(|lines| needs_paste_confirm(text, lines));
        if ask {
            // 顶掉旧请求（见字段文档）：用户按了第二次，要确认的是第二份内容
            self.paste_pending = Some(PasteRequest {
                text: text.to_string(),
                and_enter,
                newlines,
            });
            cx.notify();
            return;
        }
        self.paste_text(text, and_enter, cx);
    }

    /// 当前挂起的待确认粘贴请求（宿主用来弹确认框 / 读行数与正文）。
    pub fn pending_paste(&self) -> Option<&PasteRequest> {
        self.paste_pending.as_ref()
    }

    /// 用户在确认框里点了「粘贴」：把挂起的请求真正写进管道。
    ///
    /// 走的是 [`Self::paste_text`] 的完整通路（行尾归一 + bracketed 包裹），
    /// **不是**直接写原始文本 —— 确认的是「要不要粘」，不是「怎么粘」。
    /// 没有挂起时是空操作（幂等：连点两下确认不会写两份）。
    pub fn confirm_paste(&mut self, cx: &mut Context<Self>) {
        let Some(req) = self.paste_pending.take() else {
            return;
        };
        self.paste_text(&req.text, req.and_enter, cx);
    }

    /// 用户取消了这次粘贴（按钮 / Esc / 点遮罩 / 关框）：**零写入**丢掉的。
    ///
    /// 「取消 ⇒ 终端一个字节都不动」是规格的硬要求：用户按下取消的动机往往
    /// 正是「这内容看着不对」，此刻漏一个字节都可能已经执行了半条命令。
    pub fn cancel_paste(&mut self, cx: &mut Context<Self>) {
        if self.paste_pending.take().is_some() {
            cx.notify();
        }
    }

    /// 粘贴给定文本 —— **底层写入，不过闸**。
    ///
    /// 用户入口一律走 [`Self::request_paste`]；本函数的两个合法调用者是
    /// `request_paste`（未达阈值 / 保护关闭）与 `confirm_paste`（用户已确认）。
    ///
    /// 「整段一次性 write」的三件事都在这里，它们是粘贴语义的落点：
    /// - **不逐字符、不逐行写** —— 逐段写会让 ConPTY 在两次写之间插入回显，
    ///   远端 shell 也可能把一条长命令当成多次输入；
    /// - 行尾规范为回车 ⇒ 粘贴多行脚本时**逐行执行**；
    /// - 远端若开启 bracketed paste（`ESC[?2004h`，bash/zsh/fish/vim 默认开），
    ///   按 `ESC[200~ .. ESC[201~` 包裹整段。这是「一次性粘贴」的协议保证：
    ///   没有它，zsh/fish 会把多行粘贴**逐行当命令立即执行**（危险）。
    pub fn paste_text(&mut self, text: &str, and_enter: bool, cx: &mut Context<Self>) {
        if text.is_empty() {
            return;
        }
        // 会话已死：粘贴同样不落管道（与键盘、`write_to_pty` 同一道闸）
        if !self.alive {
            return;
        }
        // ZMODEM 传输中：文本会混进二进制协议流，必须静默丢弃。
        if self.zmodem.is_active() {
            return;
        }
        let bracketed = self
            .state
            .mode()
            .contains(alacritty_terminal::term::TermMode::BRACKETED_PASTE);
        let mut payload = normalize_paste(text, bracketed, self.config.paste_normalize_newlines);
        if and_enter {
            payload.push('\r');
        }

        {
            let mut writer = self.stdin_writer.lock();
            let _ = writer.write_all(payload.as_bytes());
            let _ = writer.flush();
        }

        // 与键入一致：粘完滚到实时屏底并让光标立即可见
        self.state
            .with_term_mut(|term| term.scroll_display(alacritty_terminal::grid::Scroll::Bottom));
        self.touch_cursor_blink(cx);
        self.notify_input_line(cx);
    }

    /// 当前输入法预编辑串（空 = 没有正在组合的输入）。
    pub fn preedit(&self) -> &str {
        &self.preedit
    }

    /// 记下输入法给的预编辑串（组合中，尚未上屏）。
    ///
    /// 只存不写：这段文字**绝不进 PTY**，只由渲染层画在光标处。
    /// 断连后不再接受新组合（`alive` 那道闸），已存在的串由 [`Self::set_alive`] 清掉。
    pub fn set_preedit(&mut self, text: &str, cx: &mut Context<Self>) {
        if !self.alive {
            return;
        }
        if self.preedit == text {
            return;
        }
        self.preedit.clear();
        self.preedit.push_str(text);
        cx.notify();
    }

    /// 清掉预编辑串（组合取消 / 平台收尾）。
    pub fn clear_preedit(&mut self, cx: &mut Context<Self>) {
        if self.preedit.is_empty() {
            return;
        }
        self.preedit.clear();
        cx.notify();
    }

    /// 提交一段**已确认**的文本到 PTY（IME 上屏的落点）。
    ///
    /// 与 [`TerminalView::paste_text`] 的三点不同，都是刻意的：
    /// - **不做换行规范化** —— 这是用户敲出来的字，不是粘贴的剧本；
    /// - **不套 bracketed paste** —— 那层包裹是给「一段要整体插入的文本」用的，
    ///   套在一个候选词上只会让 `ESC[200~` 出现在 readline 的编辑缓冲里；
    /// - **不补回车** —— 提交即上屏，要不要执行由用户自己按回车。
    ///
    /// 送完与键入同样收尾：滚回实时屏底、光标立刻可见。
    pub fn commit_text(&mut self, text: &str, cx: &mut Context<Self>) {
        // 会话已死：文本通路与键盘、粘贴同一道闸
        if !self.alive {
            return;
        }
        // ZMODEM 传输中：IME 上屏 / 平台文本消息同样不许混进协议流。
        if self.zmodem.is_active() {
            return;
        }
        // 先清预编辑：无论这段文本是不是空（输入法取消组合时会提交空串），
        // 组合都已经结束了。
        let had_preedit = !self.preedit.is_empty();
        self.preedit.clear();
        if text.is_empty() {
            if had_preedit {
                cx.notify();
            }
            return;
        }

        {
            let mut writer = self.stdin_writer.lock();
            let _ = writer.write_all(text.as_bytes());
            let _ = writer.flush();
        }
        // 与 `on_key_down` 一致：键入时若正在查看历史滚回，自动滚回底部（普通屏）
        if !self
            .state
            .mode()
            .contains(alacritty_terminal::term::TermMode::ALT_SCREEN)
        {
            self.state.with_term_mut(|term| {
                term.scroll_display(alacritty_terminal::grid::Scroll::Bottom)
            });
        }
        self.touch_cursor_blink(cx);
        cx.notify();
        self.notify_input_line(cx);
    }

    /// 光标所在单元格的窗口像素 bounds（IME 候选窗的定位依据）。
    ///
    /// 度量与鼠标换算（[`TerminalView::event_to_cell`]）同源：paint 实测尺寸优先，
    /// 没渲染过才现场补测；原点 = 内容区原点 + padding。
    ///
    /// 用**实时屏**坐标（`Line(0)` = 实时屏首行）并钳到视口内：视口上翻历史时
    /// 光标不在屏上，把候选窗放到窗口外的坐标没有意义。
    fn cursor_cell_bounds(
        &mut self,
        element_bounds: Bounds<Pixels>,
        window: &mut Window,
    ) -> Bounds<Pixels> {
        use alacritty_terminal::grid::Dimensions as _;

        let CellMetrics {
            width: cw,
            height: ch,
        } = self.cell_metrics.get();
        let (cw, ch) = if cw <= Pixels::ZERO || ch <= Pixels::ZERO {
            self.renderer.measure_cell(window);
            (self.renderer.cell_width, self.renderer.cell_height)
        } else {
            (cw, ch)
        };

        let (point, rows, cols) = self.state.with_term(|term| {
            (
                term.grid().cursor.point,
                term.screen_lines(),
                term.columns(),
            )
        });
        let row = point.line.0.clamp(0, rows as i32 - 1) as f32;
        let col = point.column.0.min(cols.saturating_sub(1)) as f32;

        let padding = self.config.padding;
        Bounds::new(
            gpui::point(
                element_bounds.origin.x + padding.left + cw * col,
                element_bounds.origin.y + padding.top + ch * row,
            ),
            gpui::size(cw, ch),
        )
    }

    /// Set a callback to be invoked when the terminal process exits.
    ///
    /// The callback receives a mutable reference to the window and context,
    /// allowing you to close the terminal view or show an exit message.
    ///
    /// # Arguments
    ///
    /// * `callback` - A function that will be called when the process exits
    ///
    /// # Example
    ///
    /// ```ignore
    /// terminal.with_exit_callback(|window, cx| {
    ///     // Close the terminal tab or show exit message
    /// })
    /// ```
    pub fn with_exit_callback(
        mut self,
        callback: impl Fn(&mut Window, &mut Context<TerminalView>) + 'static,
    ) -> Self {
        self.exit_callback = Some(Box::new(callback));
        self
    }

    /// Background thread that reads from stdout.
    ///
    /// This function runs in a background thread, continuously reading bytes
    /// from the stdout reader and sending them through the async channel.
    /// The async channel allows the main async task to be woken up immediately
    /// when data arrives (push-based).
    ///
    /// ★ 本地扩展（RustShell）：`encoding` 非 UTF-8 时在此**后台线程**做转码。
    /// 多字节字符可能跨 read 块被截断，故保留未消费尾部（≤3 字节）与下一块拼接
    /// 后再解码——否则 4 字节的 GB18030 字符被切成两半会变乱码（T4-E4）。
    fn read_stdout_blocking<R: Read + Send + 'static>(
        mut stdout_reader: R,
        bytes_tx: flume::Sender<Vec<u8>>,
        encoding: Option<String>,
        zmodem: Option<Arc<ZmodemPump>>,
    ) {
        let mut buffer = [0u8; 4096];

        // 非 UTF-8 才走转码路径；UTF-8/未指定保持零拷贝直通
        let encoder = encoding
            .as_deref()
            .filter(|e| !e.is_empty() && !e.eq_ignore_ascii_case("utf-8"))
            .and_then(|label| encoding_rs::Encoding::for_label(label.as_bytes()));
        // 转码路径下的「未消费尾部」缓冲（跨块拼接用）
        let mut carry: Vec<u8> = Vec::new();

        loop {
            match stdout_reader.read(&mut buffer) {
                Ok(0) => {
                    // EOF - channel will be dropped, signaling completion
                    break;
                }
                Ok(n) => {
                    // ★ ZMODEM 必须看**原始字节**（早于转码）：协议帧是二进制，
                    //   按 GB18030 转一遍就毁了。泵只扣留协议帧与疑似起始符，
                    //   普通文字原样返回 ⇒ 下面照旧走编码转换。
                    let filtered;
                    let chunk: &[u8] = match zmodem.as_ref() {
                        Some(pump) => {
                            filtered = pump.feed(&buffer[..n]);
                            if filtered.is_empty() {
                                continue;
                            }
                            &filtered
                        }
                        None => &buffer[..n],
                    };
                    let out: Vec<u8> = if let Some(enc) = encoder {
                        // 拼接上次未消费尾部
                        let mut input = std::mem::take(&mut carry);
                        input.extend_from_slice(chunk);
                        let (cow, _enc_used, malformed) = enc.decode(&input);
                        if malformed {
                            // 尾部有截断的多字节序列：按 UTF-8 最大 4 字节保留末尾
                            // 保留最后 min(3, len) 字节即可覆盖 2/3/4 字节序列的截断
                            let keep = input.len().min(3);
                            carry = input[input.len() - keep..].to_vec();
                            // 对「可完整解码」的前缀重新解码（去掉尾部残片）
                            let head = &input[..input.len() - keep];
                            enc.decode(head).0.into_owned().into_bytes()
                        } else {
                            // 完全解码成功；但末尾若刚好是某个多字节序列的开头，
                            // decode 会把它当 malformed 或替换字符，故这里仅在有
                            // malformed 时才保留——无 malformed 即无截断风险。
                            cow.into_owned().into_bytes()
                        }
                    } else {
                        chunk.to_vec()
                    };
                    if out.is_empty() {
                        continue;
                    }
                    if bytes_tx.send(out).is_err() {
                        break; // Channel closed
                    }
                }
                Err(_) => {
                    // Read error
                    break;
                }
            }
        }
    }

    /// Handle keyboard input events.
    ///
    /// Converts GPUI keystrokes to terminal escape sequences and writes them
    /// to the stdin writer. If a key handler is set and returns true, the event
    /// is consumed and not sent to the terminal.
    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        // 会话已死：一个键都不许进管道，也**不阻断传播** —— 断连的终端不该再吞掉
        // 全局快捷键（Ctrl+T / Ctrl+W 等），用户此刻最需要的是「换个地方干活」。
        if !self.alive {
            return;
        }

        // ZMODEM 传输中：键盘字节会直接落进协议流，把二进制帧撕碎。
        // 只留一个出口 —— Esc 主动中止；其余按键（含全局快捷键）一律吞掉，
        // 否则用户按 Ctrl+C 想停传输，反而先污染了数据。
        if self.zmodem.is_active() {
            let k = &event.keystroke;
            if k.key == "escape"
                && !k.modifiers.control
                && !k.modifiers.alt
                && !k.modifiers.platform
                && !k.modifiers.shift
            {
                self.zmodem.cancel();
                cx.notify();
            }
            cx.stop_propagation();
            return;
        }

        // 输入法正在组合：键盘此刻归 IME 所有，一个字节都不许进管道。
        //
        // 平台**先**把 keydown 递到应用，之后才轮到输入法处理它
        // （Windows 的 IME 处理在 `TranslateMessage` 阶段，即 keydown 之后；
        // macOS 在组合期间也会把按键先交 `input_context`）。所以组合中的每一次
        // 按键都会经过这里：不挡的话，按回车「确认候选词」会同时给 shell 送一个
        // `\r`（命令直接执行），按退格删的是 shell 里的字符。
        //
        // 与下一道闸分工不同：这里挡的是**控制键**（回车/退格/Esc），
        // 可打印键本来就由文本通路负责，见 `is_text_input_keystroke`。
        if !self.preedit.is_empty() {
            return;
        }

        // 补全浮层的按键过滤（C-1）。
        //
        // 位置很讲究，两条闸之后、文本通路之前：
        // - 在 `preedit` 闸**之后**：输入法组合中的按键整键让位（铁律 #64），
        //   浮层绝不抢"回车确认候选词"那一下；
        // - 在可打印键放行**之前**：浮层的 `↑↓` / `Tab` / `Esc` 都是控制键，
        //   必须在这里被截住，否则会作为光标键 / 制表符写进 PTY；
        // - 但**可打印键一律放行**：浮层开着时用户还要继续打字（那正是过滤
        //   候选的输入源），截住它等于浮层一开终端就哑了（铁律 #63）。
        if let Some(ref filter) = self.completion_key_filter {
            let is_printable =
                event.prefer_character_input || is_text_input_keystroke(&event.keystroke);
            // 可打印键永远不进过滤器：见上面第三条。
            if !is_printable {
                // 借出 `&self`（见 `CompletionKeyFilter` 的文档）：`this` 与
                // `filter` 是两个**共享**借用，所以能同时在手 —— 这也正是
                // 过滤器只拿得到 `&TerminalView` 的机制原因。
                let this: &TerminalView = self;
                match filter(event, this, _window, cx) {
                    CompletionKeyOutcome::PassThrough => {}
                    CompletionKeyOutcome::Consumed => {
                        cx.stop_propagation();
                        return;
                    }
                    CompletionKeyOutcome::ToggleRequested => {
                        // 切换浮层：Ctrl+Space 会被译成 `\x00`，绝不能把它写进 PTY
                        // （远端 shell 收到 NUL 是垃圾输入）。
                        cx.stop_propagation();
                        return;
                    }
                }
            }
        }

        // Check if key handler wants to consume this event
        if let Some(ref handler) = self.key_handler
            && handler(event)
        {
            cx.stop_propagation(); // Event consumed by handler
            return;
        }

        // 粘贴键位：Ctrl+Shift+V / Cmd+V（主流终端）与 Shift+Insert（老牌终端）。
        // **不劫持裸 Ctrl+V** —— shell 里它是「按字面插入下一个字符」（readline
        // 的 quoted-insert），劫持会破坏 readline 习惯；需要它的用户可在设置里开。
        {
            let k = &event.keystroke;
            let paste = (k.key == "v"
                && (k.modifiers.platform || (k.modifiers.control && k.modifiers.shift)))
                || (k.key == "insert" && k.modifiers.shift);
            if paste {
                self.paste_from_clipboard(false, cx);
                cx.stop_propagation();
                return;
            }
        }

        // 可打印字符归文本输入通路（IME / WM_CHAR / `insertText:` / `key_char`），
        // 从这里放行 —— 详情见 `is_text_input_keystroke`：装了输入处理器之后，
        // 不消费按键就不代表没人管，平台会把字符再投一次到文本通路。
        //
        // ★ 这里**不能** `stop_propagation()`：平台上「消费了 keydown」正是
        //   文本通路的抑制条件（Windows 跳过 `TranslateMessage`、macOS 直接
        //   return），一消费字符就再也到不了了。
        //
        // `prefer_character_input`（Windows 专有）要一并放行：那是 AltGr / 死键
        // 的提示 —— 这个键的可打印结果只能从 WM_CHAR 拿到，key_char 不可信。
        if event.prefer_character_input || is_text_input_keystroke(&event.keystroke) {
            return;
        }

        if let Some(bytes) = keystroke_to_bytes(&event.keystroke, self.state.mode()) {
            // 键入时若正在查看历史滚回，自动滚回底部（普通屏）
            if !self
                .state
                .mode()
                .contains(alacritty_terminal::term::TermMode::ALT_SCREEN)
            {
                self.state.with_term_mut(|term| {
                    term.scroll_display(alacritty_terminal::grid::Scroll::Bottom)
                });
            }
            let mut writer = self.stdin_writer.lock();
            let _ = writer.write_all(&bytes);
            let _ = writer.flush();
            drop(writer);

            // 「用户按了回车」的两段式置旗（见 `enter_dirty`）。
            //
            // 判据是**最终写出去的字节**恰好是 `\r`，不是 `event.keystroke.key == "enter"`
            // —— 小键盘回车、以及将来可能出现的其它映射都会走到这里，
            // 而按 key 名字判会漏（见 `EnterCallback` 的文档）。
            //
            // 位置在 `writer` drop 之后：顺序见 `enter_dirty` 的字段注释。
            if bytes == b"\r" && self.enter_callback.is_some() && !self.enter_dirty {
                self.enter_dirty = true;
                cx.notify();
            }

            // 输入后光标立即回到「亮」相位，避免「刚敲完键光标却是黑的」
            self.touch_cursor_blink(cx);

            // Tab/Shift+Tab 会被 GPUI 焦点导航（focus traversal）捕获并移走焦点，
            // 终端必须显式阻断传播，否则 shell 的 Tab 补全收不到键。
            // 其余按键不阻断，保证全局快捷键（Ctrl+T/W/B 等）仍然生效；
            // Ctrl+Tab / Ctrl+Shift+Tab 也放行，留给全局标签切换。
            let k = &event.keystroke;
            let is_plain_tab = k.key == "tab" && !k.modifiers.control;
            if is_plain_tab {
                cx.stop_propagation();
            }
        }
    }

    /// Handle mouse down events.
    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Request focus when clicking the terminal.
        // 会话已死时不抢焦点：焦点环与插入点是「可以打字」的信号，断连的终端
        // 只提供「读」——划选复制仍然照走（下面几行不看聚焦）。
        if self.alive {
            window.focus(&self.focus_handle, cx);
        }

        if event.button == MouseButton::Left {
            // 连击检测：时间窗口内且位置接近（±1.5 cell）则递增，否则重新计数
            let CellMetrics {
                width: cw,
                height: ch,
            } = self.cell_metrics.get();
            let now = std::time::Instant::now();
            self.click_count = match self.last_click {
                Some((t, pos))
                    if now.duration_since(t).as_millis() <= MULTI_CLICK_INTERVAL_MS
                        && (event.position.x - pos.x).abs() <= cw * 1.5
                        && (event.position.y - pos.y).abs() <= ch * 1.5 =>
                {
                    (self.click_count + 1).min(3)
                }
                _ => 1,
            };
            self.last_click = Some((now, event.position));

            let cell = self.event_to_cell(&event.position, window);
            // 单击=逐字符划选；双击=选词；三击=选行（选词/选行立即扩展并复制）
            let kind = crate::mouse::selection_type_from_clicks(self.click_count);
            let sel = self.expand_selection(cell, kind);
            *self.selection.borrow_mut() = Some(sel);
            self.selecting = true;
            self.touch_cursor_blink(cx);
            cx.notify();
        }
    }

    /// 把视口单元格坐标按选择类型扩展为完整 Selection。
    ///
    /// - Simple：原样（起点=终点，拖拽时由 move 扩展）
    /// - Word：扩展到同行相邻的非分隔字符段（双击选词）
    /// - Line：扩展到整行（三击选行）
    fn expand_selection(
        &self,
        cell: alacritty_terminal::index::Point<alacritty_terminal::index::Line>,
        kind: crate::mouse::SelectionType,
    ) -> crate::mouse::Selection {
        use alacritty_terminal::grid::Dimensions as _;
        use alacritty_terminal::index::{Column, Line, Point};

        let rows = self.state.with_term(|t| t.screen_lines()) as i32;
        let row = cell.line.0.clamp(0, rows.saturating_sub(1));

        if kind == crate::mouse::SelectionType::Line {
            let cols = self.state.with_term(|t| t.columns());
            return crate::mouse::Selection::new(
                Point::new(Line(row), Column(0)),
                Point::new(Line(row), Column(cols.saturating_sub(1))),
                kind,
            );
        }
        if kind == crate::mouse::SelectionType::Simple {
            return crate::mouse::Selection::new(cell, cell, kind);
        }

        // Word：在目标行内扫描非分隔字符段边界
        self.state.with_term(|term| {
            let cols = term.columns();
            let col = cell.column.0.min(cols.saturating_sub(1));
            let off = term.grid().display_offset() as i32;
            let alac_line = Line(row - off);
            let char_at = |c: usize| term.grid()[alac_line][Column(c)].c;
            let is_word = |c: char| !WORD_SEPARATORS.contains(&c);

            if !is_word(char_at(col)) {
                // 点在分隔符上：退化为单字符选择（不扩展）
                return crate::mouse::Selection::new(
                    Point::new(Line(row), Column(col)),
                    Point::new(Line(row), Column(col)),
                    kind,
                );
            }
            let mut s = col;
            while s > 0 && is_word(char_at(s - 1)) {
                s -= 1;
            }
            let mut e = col;
            while e + 1 < cols && is_word(char_at(e + 1)) {
                e += 1;
            }
            crate::mouse::Selection::new(
                Point::new(Line(row), Column(s)),
                Point::new(Line(row), Column(e)),
                kind,
            )
        })
    }

    /// Handle mouse up events.
    fn on_mouse_up(&mut self, _event: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if !self.selecting {
            return;
        }
        self.selecting = false;

        // 单击（起点=终点）：清除选择，不复制
        let collapsed = {
            let sel = self.selection.borrow();
            sel.as_ref().map(|s| s.start == s.end).unwrap_or(true)
        };
        if collapsed {
            *self.selection.borrow_mut() = None;
            cx.notify();
            return;
        }

        // 划选结束：提取文本并复制到系统剪贴板
        if let Some(text) = self.selected_text()
            && !text.is_empty()
        {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
        }
        cx.notify();
    }

    /// Handle mouse move events.
    fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.selecting {
            return;
        }
        let cell = self.event_to_cell(&event.position, window);
        if let Some(sel) = self.selection.borrow_mut().as_mut() {
            sel.end = cell;
            cx.notify();
        }
    }

    /// 把窗口像素坐标换算为视口单元格坐标（含边界钳制）。
    fn event_to_cell(
        &mut self,
        position: &gpui::Point<Pixels>,
        window: &mut Window,
    ) -> alacritty_terminal::index::Point<alacritty_terminal::index::Line> {
        use alacritty_terminal::grid::Dimensions as _;
        use alacritty_terminal::index::{Column, Line};

        // 单元格尺寸用 paint 时实测的共享值；从未渲染过（零值）才现场补测
        let CellMetrics {
            width: cw,
            height: ch,
        } = self.cell_metrics.get();
        let (cw, ch) = if cw <= Pixels::ZERO || ch <= Pixels::ZERO {
            self.renderer.measure_cell(window);
            (self.renderer.cell_width, self.renderer.cell_height)
        } else {
            (cw, ch)
        };
        let bounds = *self.content_bounds.borrow();
        let padding = self.config.padding;
        let origin = gpui::point(
            bounds.origin.x + padding.left,
            bounds.origin.y + padding.top,
        );
        let cell = crate::mouse::pixel_to_cell(*position, origin, cw, ch);

        // 钳制到当前网格范围
        let (cols, rows) = self
            .state
            .with_term(|term| (term.columns(), term.screen_lines()));
        let row = cell.line.0.clamp(0, rows as i32 - 1);
        let col = cell.column.0.min(cols.saturating_sub(1));
        alacritty_terminal::index::Point::new(Line(row), Column(col))
    }

    /// Handle scroll events.
    ///
    /// 普通缓冲：滚动 display_offset 查看历史滚回（Scroll::Delta 正值=向上翻历史）；
    /// 备用屏（vim/htop 等）：不滚动，改发方向键序列（APP_CURSOR 时用 SS3 形式）。
    fn on_scroll(
        &mut self,
        event: &ScrollWheelEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use crate::mouse::pixels_to_scroll_lines;
        use alacritty_terminal::grid::Scroll;
        use alacritty_terminal::term::TermMode;
        use std::io::Write;

        // 行高：paint 实测优先，未渲染过回退 renderer 估算
        let ch = {
            let CellMetrics { height: ch, .. } = self.cell_metrics.get();
            if ch <= Pixels::ZERO {
                self.renderer.cell_height
            } else {
                ch
            }
        };
        let lines = match event.delta {
            gpui::ScrollDelta::Pixels(delta) => pixels_to_scroll_lines(delta.y, ch),
            gpui::ScrollDelta::Lines(delta) => delta.y.round().clamp(-10.0, 10.0) as i32,
        };
        if lines == 0 {
            return;
        }

        let alt_screen = self.state.mode().contains(TermMode::ALT_SCREEN);
        if alt_screen {
            // 备用屏：滚轮 → 方向键（正值=向上=光标上移）
            let up = lines > 0;
            let seq = if self.state.mode().contains(TermMode::APP_CURSOR) {
                if up { "\x1bOA" } else { "\x1bOB" }
            } else if up {
                "\x1b[A"
            } else {
                "\x1b[B"
            };
            let mut w = self.stdin_writer.lock();
            for _ in 0..lines.unsigned_abs() {
                let _ = w.write_all(seq.as_bytes());
            }
            let _ = w.flush();
        } else {
            self.state
                .with_term_mut(|term| term.scroll_display(Scroll::Delta(lines)));
        }
        // 滚回底部时光标重新出现在视口内，让它立刻可见
        self.touch_cursor_blink(cx);
        cx.notify();
    }

    /// Process pending terminal events.
    ///
    /// This method drains all available events from the event receiver
    /// and handles them appropriately. Note: bytes are processed in the
    /// async reader task, not here.
    /// 把 shell integration 专用通道里的事件全部并进视图状态（不触发回调）。
    ///
    /// 与 [`process_events`](Self::process_events) 的分工：那边还要 `Window`
    /// （要转发给宿主回调），而这里**只要视图自己**。读端任务的解析闭包里
    /// 拿不到 `Window`，所以必须有一个"无 Window"的入口先落状态，
    /// 否则事件要等到下一次 `process_events` 才被看见（见 `spawn_reader` 注释）。
    ///
    /// 幂等：通道抽干就返回，重复调用无害。
    fn drain_shell_events(&mut self) {
        while let Ok(event) = self.shell_event_rx.try_recv() {
            match event {
                TerminalEvent::PromptMark(mark) => {
                    self.shell_marks.push(mark);
                    // 只保留最近若干条：本向量是"最近发生了什么"，
                    // 不是历史档案。无上限递增会让一个长跑会话缓慢吃内存。
                    const MAX_KEPT_MARKS: usize = 64;
                    if self.shell_marks.len() > MAX_KEPT_MARKS {
                        let drop_n = self.shell_marks.len() - MAX_KEPT_MARKS;
                        self.shell_marks.drain(..drop_n);
                    }
                }
                TerminalEvent::Cwd(cwd) => self.shell_cwd = Some(cwd),
                TerminalEvent::CommandLine(line) => self.shell_command_line = Some(line),
                // 这条通道**只承载** shell integration 三种事件（引擎侧只投这三种）；
                // 其余变体说明投递方接错了通道 —— 静默忽略。
                _ => {}
            }
        }
    }

    fn process_events(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // shell integration 事件走**专用通道**（见字段 `shell_event_tx` 注释）。
        //
        // 状态本身在读端任务里就已经落了（`drain_shell_events`），这里再抽一次
        // 只为拿到**回调**时机：回调要 `Window`，而读端任务拿不到。
        // 先落状态再给回调，顺序上"宿主读 `shell_marks()` 时已经能看到这条"。
        self.drain_shell_events();

        // 「输入行可能变了」的两段式兑现（见 `notify_input_line`）：
        // 旗子在 `commit_text` / `paste_text` 里置，这里拿 `Window` 调回调。
        if self.input_line_dirty {
            self.input_line_dirty = false;
            // 把 `&self` **借给**回调（见 `InputLineCallback` 的文档）：
            // 这一行跑在 `render` 里，本实体的租约正握在引擎手里，
            // 宿主若去 `read` / `update` 实体就是 panic。
            //
            // `this` 与 `this.input_line_callback` 都是**共享**借用 ⇒
            // 不必把闭包 `take()` 出来再放回去（`&mut` 才需要那套手法）。
            let this: &TerminalView = self;
            if let Some(callback) = this.input_line_callback.as_ref() {
                callback(this, window, cx);
            }
        }

        // 「用户按了回车」的两段式兑现（见 `EnterCallback`）。
        //
        // 必须排在 `input_line_dirty` 之后：上面那次是「行可能变了」，这次是
        // 「行被执行了」，而宿主在后者里读的那一行**正是**前者刚通知过的那份内容。
        // 反过来的话，浮层会先收到「命令已执行」（此时可能在算新候选），
        // 再收到「行变了」—— 中间那一帧候选属于一条已经跑掉的命令。
        if self.enter_dirty {
            self.enter_dirty = false;
            // 同 `input_line_dirty`：借出 `&self` 而不是让宿主读实体。
            let this: &TerminalView = self;
            if let Some(callback) = this.enter_callback.as_ref() {
                callback(this, window, cx);
            }
        }

        // Process terminal events (from alacritty event proxy)
        while let Ok(event) = self.event_rx.try_recv() {
            match event {
                TerminalEvent::Wakeup => {
                    // Terminal has new content - already handled by async task
                }
                TerminalEvent::Bell => {
                    if let Some(ref callback) = self.bell_callback {
                        callback(window, cx);
                    }
                }
                TerminalEvent::Title(title) => {
                    if let Some(ref callback) = self.title_callback {
                        callback(window, cx, &title);
                    }
                }
                TerminalEvent::ClipboardStore(text) => {
                    if let Some(ref callback) = self.clipboard_store_callback {
                        callback(window, cx, &text);
                    }
                }
                TerminalEvent::ClipboardLoad(formatter) => {
                    // ★ OSC 52 读取：向宿主取剪贴板文本，**用 alacritty 给的格式化器**
                    //   拼出应答序列后写回 PTY。旧实现留了个 TODO，读取请求石沉大海。
                    let text = self
                        .clipboard_load_callback
                        .as_ref()
                        .and_then(|callback| callback(window, cx));
                    if let Some(text) = text {
                        let reply = formatter(&text);
                        let mut writer = self.stdin_writer.lock();
                        let _ = writer.write_all(reply.as_bytes());
                        let _ = writer.flush();
                    }
                }
                TerminalEvent::CursorBlinkingChange => {
                    // DECSCUSR / `ESC[?12h|l` 改了闪烁要求：按最新状态重启
                    // 或停掉定时器。这里不比较「有没有变」——两路来源合并后
                    // 一个事件可能并不改变最终结果，重置相位只是让光标多亮
                    // 一下，代价可忽略。
                    self.reset_cursor_blink(cx);
                }
                TerminalEvent::PromptMark(mark) => {
                    // 理论上不该走到这里：shell integration 事件由**专用通道**
                    // 投递（见 `shell_event_tx`），在函数开头已消费完。
                    // 保留这个分支有两个理由：
                    //   1. `TerminalEvent` 是全 crate 共享的枚举，`match` 必须穷尽；
                    //   2. 万一将来有人把 shell 事件从 alacritty 代理那条路投进来，
                    //      这里也能兜住（同样"只存不渲染"），而不是被静默丢弃。
                    //
                    // shell integration（C-0）：**只感知、不渲染** —— 本层还没有调用方
                    // （命令块 / 补全 / 跳转上一提示符都要等后续阶段），
                    // 而"先画再改语义"是要返工的。把状态记下来就够了。
                    self.shell_marks.push(mark);
                    const MAX_KEPT_MARKS: usize = 64;
                    if self.shell_marks.len() > MAX_KEPT_MARKS {
                        let drop_n = self.shell_marks.len() - MAX_KEPT_MARKS;
                        self.shell_marks.drain(..drop_n);
                    }
                    if let Some(ref callback) = self.shell_mark_callback {
                        callback(window, cx, mark);
                    }
                }
                TerminalEvent::Cwd(cwd) => {
                    // `OSC 7`：远端上报的工作目录。**这是远端路径**，
                    // 不能拿去开本地文件对话框（宿主侧要自己分辨）。
                    self.shell_cwd = Some(cwd.clone());
                    if let Some(ref callback) = self.shell_cwd_callback {
                        callback(window, cx, &cwd);
                    }
                }
                TerminalEvent::CommandLine(line) => {
                    // `OSC 633;E`：当前命令行的准确文本。
                    self.shell_command_line = Some(line.clone());
                    if let Some(ref callback) = self.shell_command_line_callback {
                        callback(window, cx, &line);
                    }
                }
                TerminalEvent::Exit => {
                    if let Some(ref callback) = self.exit_callback {
                        callback(window, cx);
                    }
                }
            }
        }
    }

    /// Get the current terminal dimensions.
    ///
    /// # Returns
    ///
    /// A tuple of (columns, rows).
    pub fn dimensions(&self) -> (usize, usize) {
        (self.state.cols(), self.state.rows())
    }

    /// ★ 本地扩展（RustShell）：滚动几何快照，供外部（UI 层）绘制滚动条。
    ///
    /// - `history_size`：可回滚的历史行数（0 = 内容未超过一屏 → 不显示滚动条）
    /// - `screen_lines`：视口行数
    /// - `display_offset`：当前回滚偏移（0 = 紧跟输出底部）
    /// - `cell_height`：单元格像素高（换算偏移 ↔ 像素用）
    pub fn scroll_metrics(&self) -> TermScrollMetrics {
        use alacritty_terminal::grid::Dimensions as _;
        // `cell_metrics` 的次序是 `(宽, 高)`（见本文件 1793 行赋值 `(cell_width,
        // cell_height)`，以及 1199 / 1339 / 1379 行的 `let (cw, ch)` 用法）。
        // 这里曾误拆成 `(cell_h, _)`，于是 `cell_height` 拿到的是**字宽**：
        // `content_size` 与 `offset` 双双按错比例换算，滚动条滑块被推到轨道之外。
        let CellMetrics { height: cell_h, .. } = self.cell_metrics.get();
        let cell_height = if cell_h <= Pixels::ZERO {
            self.renderer.cell_height
        } else {
            cell_h
        };
        self.state.with_term(|term| {
            let total = term.total_lines();
            let screen = term.screen_lines();
            let history = total.saturating_sub(screen);
            let offset = term.grid().display_offset().min(history);
            TermScrollMetrics {
                history_size: history,
                screen_lines: screen,
                display_offset: offset,
                cell_height,
            }
        })
    }

    /// ★ 本地扩展（RustShell）：设置回滚偏移（0 = 底部，`history_size` = 最顶）。
    ///
    /// 只改内存偏移，**不触碰 PTY**（不触发 resize），故可在 UI 线程安全调用。
    /// 传入值按 `[0, history_size]` 钳制。
    pub fn set_display_offset(&self, offset: usize) {
        use alacritty_terminal::grid::{Dimensions as _, Scroll};
        self.state.with_term_mut(|term| {
            let history = term.total_lines().saturating_sub(term.screen_lines());
            let target = offset.min(history) as i32;
            let cur = term.grid().display_offset() as i32;
            let delta = target - cur;
            if delta != 0 {
                term.scroll_display(Scroll::Delta(delta));
            }
        });
    }

    /// 把回滚偏移设为 0（紧跟输出底部）。终端已有此逻辑，此处仅供外部调用。
    pub fn scroll_to_bottom(&self) {
        self.set_display_offset(0);
    }

    /// Resize the terminal to new dimensions.
    ///
    /// This method should be called when the terminal view size changes.
    /// It updates the internal grid and notifies the terminal process of the new size.
    ///
    /// # Arguments
    ///
    /// * `cols` - New number of columns
    /// * `rows` - New number of rows
    pub fn resize(&mut self, cols: usize, rows: usize) {
        self.state.resize(cols, rows);
    }

    /// Write raw bytes to the PTY stdin (paste / send control characters).
    ///
    /// 会话已死（[`TerminalView::set_alive`] 为 `false`）时**静默丢弃**：这一刀是
    /// 「断连后不可输入」的最终防线 —— 快捷命令、AI 面板、菜单「粘贴并执行」
    /// 全都经由本函数写入，挡在这里等于把所有通路一次盖住。
    pub fn write_to_pty(&self, text: &str) {
        if !self.alive {
            return;
        }
        // ZMODEM 传输中：快捷命令 / AI / 菜单的写入同样不许污染协议流。
        if self.zmodem.is_active() {
            return;
        }
        use std::io::Write;
        let mut w = self.stdin_writer.lock();
        let _ = w.write_all(text.as_bytes());
        let _ = w.flush();
    }

    /// ZMODEM 当前状态快照（宿主据此画传输浮层）。
    pub fn zmodem_status(&self) -> ZmodemStatus {
        self.zmodem.status()
    }

    /// 是否有 ZMODEM 传输在进行（或正在等待远端 `rz` 就位）。
    pub fn zmodem_active(&self) -> bool {
        self.zmodem.is_active()
    }

    /// 驱动超时 / 通知过期，返回状态是否变化（宿主轮询时调用）。
    pub fn zmodem_tick(&self) -> bool {
        self.zmodem.tick()
    }

    /// 拖放上传：向远端发 `rz -be` 并排队这些本地文件。
    ///
    /// 真正的握手在字节泵里完成 —— 等远端 `rz` 发出 ZRINIT 后才开始发送，
    /// 期间状态是 [`ZmodemPhase::Waiting`](crate::zmodem::ZmodemPhase::Waiting)。
    pub fn start_zmodem_upload(
        &self,
        paths: Vec<PathBuf>,
    ) -> Result<ZmodemUploadStart, ZmodemUploadError> {
        self.zmodem.start_upload(paths)
    }

    /// 主动中止当前传输（Esc / 浮层取消按钮）。
    pub fn cancel_zmodem(&self) {
        self.zmodem.cancel();
    }

    /// 设置搜索串（None 清除）。正则编译失败视为无效（返回 false 且不生效）。
    pub fn set_search(&self, query: Option<String>) -> bool {
        use std::sync::atomic::Ordering;
        let mut guard = self.search.lock();
        *guard = query.and_then(|text| {
            alacritty_terminal::term::search::RegexSearch::new(&text)
                .ok()
                .map(|regex| SearchState {
                    text,
                    regex,
                    // 换查询串即换版本号 ⇒ 命中区间缓存整块失效
                    ver: SEARCH_QUERY_VER.fetch_add(1, Ordering::Relaxed) + 1,
                })
        });
        *self.search_last_hit.lock() = None;
        *self.search_spans_cache.lock() = None;
        guard.is_some()
    }

    /// 跳转到下一个/上一个匹配（滚动视口使匹配行尽量贴顶）。命中返回 true。
    pub fn search_jump(&self, forward: bool) -> bool {
        use alacritty_terminal::grid::Dimensions as _;
        use alacritty_terminal::index::{Column, Direction, Line, Point, Side};

        self.state.with_term_mut(|term| {
            let mut guard = self.search.lock();
            let Some(q) = guard.as_mut() else {
                return false;
            };
            let d = term.grid().display_offset() as i32;
            let rows = term.screen_lines() as i32;
            let last_col = term.columns().saturating_sub(1);

            let top_abs = Line(-d);
            let bottom_abs = Line(rows - 1 - d);

            // 推进基准：上次命中行；无则从视口顶/底开始（视口内优先逐个跳）
            let last = *self.search_last_hit.lock();
            let (origin, dir) = if forward {
                match last {
                    Some(line) => (Point::new(Line(line), Column(last_col)), Direction::Right),
                    None => (Point::new(top_abs, Column(0)), Direction::Right),
                }
            } else {
                match last {
                    Some(line) => (Point::new(Line(line), Column(0)), Direction::Left),
                    None => (Point::new(bottom_abs, Column(last_col)), Direction::Left),
                }
            };

            // 先从推进点向后找；找不到则回绕（从顶部重新开始）
            let m = term
                .search_next(&mut q.regex, origin, dir, Side::Left, None)
                .or_else(|| {
                    let wrap_origin = if forward {
                        Point::new(top_abs, Column(0))
                    } else {
                        Point::new(bottom_abs, Column(last_col))
                    };
                    term.search_next(&mut q.regex, wrap_origin, dir, Side::Left, None)
                });

            let Some(m) = m else { return false };
            let hit = if forward { *m.start() } else { *m.end() };
            *self.search_last_hit.lock() = Some(hit.line.0);

            // 滚动使匹配行贴视口顶：d_target = -abs_line（abs = viewport - d）
            let d_target = (-hit.line.0).clamp(0, term.total_lines() as i32 - rows) as usize;
            let delta = d_target as i32 - d;
            if delta != 0 {
                use alacritty_terminal::grid::Scroll;
                term.scroll_display(Scroll::Delta(delta));
            }
            true
        })
    }

    /// Get the text currently visible in the viewport (for copy support).
    ///
    /// Includes trailing whitespace trimming per line via `bounds_to_string`.
    pub fn visible_text(&self) -> String {
        use alacritty_terminal::grid::Dimensions as _;
        use alacritty_terminal::index::{Column, Line, Point};
        self.state.with_term(|term| {
            let grid = term.grid();
            let off = grid.display_offset() as i32;
            let start = Point::new(Line(-off), Column(0));
            let end = Point::new(
                Line(grid.screen_lines() as i32 - 1 - off),
                grid.last_column(),
            );
            term.bounds_to_string(start, end)
        })
    }

    /// 取当前鼠标划选的文本；无有效选择时返回 None。
    ///
    /// 坐标约定：Selection 内为视口行列（Line 0 = 视口首行），
    /// 转换为 alacritty 视口坐标 `Line(row - display_offset)` 后交给 bounds_to_string。
    pub fn selected_text(&self) -> Option<String> {
        use alacritty_terminal::index::{Line, Point};

        let sel = self.selection.borrow().clone()?;
        if sel.start == sel.end {
            return None;
        }
        self.state.with_term(|term| {
            let off = term.grid().display_offset() as i32;
            let mut a = Point::new(Line(sel.start.line.0 - off), sel.start.column);
            let mut b = Point::new(Line(sel.end.line.0 - off), sel.end.column);
            if a > b {
                std::mem::swap(&mut a, &mut b);
            }
            let text = term.bounds_to_string(a, b);
            if text.is_empty() { None } else { Some(text) }
        })
    }

    /// 清除当前划选状态（供外部在点击别处或新输出时调用）。
    pub fn clear_selection(&self) {
        *self.selection.borrow_mut() = None;
    }

    /// Get the current terminal configuration.
    ///
    /// # Returns
    ///
    /// A reference to the current configuration.
    pub fn config(&self) -> &TerminalConfig {
        &self.config
    }

    /// Get the focus handle for this terminal view.
    ///
    /// # Returns
    ///
    /// A reference to the focus handle.
    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus_handle
    }

    /// Update the terminal configuration.
    ///
    /// This method updates the terminal's configuration, including font settings,
    /// padding, and color palette. Changes take effect on the next render.
    ///
    /// # Arguments
    ///
    /// * `config` - The new configuration to apply
    /// * `cx` - The context for triggering a repaint
    pub fn update_config(&mut self, config: TerminalConfig, cx: &mut Context<Self>) {
        // Update renderer with new font settings and palette
        self.renderer.font_family = config.font_family.clone();
        self.renderer.font_size = config.font_size;
        self.renderer.line_height_multiplier = config.line_height_multiplier;
        self.renderer.palette = config.colors.clone();

        // Store the new config
        self.config = config;

        // 闪烁开关 / 周期可能变了：按新配置重建定时器
        self.reset_cursor_blink(cx);

        // Trigger a repaint - cell dimensions will be recalculated via measure_cell()
        cx.notify();
    }

    /// Calculate terminal dimensions from pixel bounds and cell size.
    ///
    /// Helper method to determine how many columns and rows fit in the given bounds.
    #[allow(dead_code)]
    fn calculate_dimensions(&self, bounds: Bounds<Pixels>) -> (usize, usize) {
        let width_f32: f32 = bounds.size.width.into();
        let height_f32: f32 = bounds.size.height.into();
        let cell_width_f32: f32 = self.renderer.cell_width.into();
        let cell_height_f32: f32 = self.renderer.cell_height.into();

        let cols = safe_grid_dim(
            width_f32,
            cell_width_f32,
            f32::from(self.renderer.font_size),
            80,
            MAX_GRID_COLS,
            "cols",
            "calc_dims",
        );
        let rows = safe_grid_dim(
            height_f32,
            cell_height_f32,
            f32::from(self.renderer.font_size),
            24,
            MAX_GRID_ROWS,
            "rows",
            "calc_dims",
        );
        (cols, rows)
    }
}

/// 输入法通路（IME）：预编辑与提交的落点。
///
/// # 为什么终端必须自己实现它
///
/// 平台的字符消息不走 keydown：Windows 走 `WM_CHAR`、macOS 走 `insertText:`、
/// X11 在按键未被消费时转发 `key_char`。没有 [`EntityInputHandler`] 的实现，
/// 这些字符**没有收件人**（lib.rs 里 `handle_char_msg` 拿不到输入处理器就整条丢弃），
/// 后果是两件事一起坏：
/// - 中文 / 日文 / 韩文输入法**打不出字**（组合结果是走 `insertText:` 提交的）；
/// - 关掉输入法时，连 ASCII 都进不来（macOS 上普通字符也只从 `insertText:` 走）。
///
/// # 与键盘路径的分工
///
/// 这里的每个方法都只做「文本」这一件事：可打印字符由平台投到这里，
/// 控制字符与功能键仍走 [`TerminalView::on_key_down`]。两边**互斥**的规则见
/// [`crate::input::is_text_input_keystroke`] —— 那边负责放行，这边负责落笔。
///
/// # 预编辑为什么不能直接上屏
///
/// 组合中的字还没定（拼音敲到一半、候选词翻页中），进 PTY 就是让 shell 先执行
/// 半截命令。所以 [`Self::replace_and_mark_text_in_range`] 只把串记进
/// [`TerminalView::preedit`] 画在光标处，等 [`Self::replace_text_in_range`]
/// （上屏）才真正写管道。
impl EntityInputHandler for TerminalView {
    /// 终端没有「可编辑文档」这个概念：整屏内容属于远端程序，不是本地的文本缓冲区。
    ///
    /// 返回 `None` 而不是把网格拼成字符串 —— 让输入法以为能替换终端内容，
    /// 就等于给了它一条改写屏幕的错路。（代价：候选词拿不到上下文，
    /// 对拼音/注音这类「靠上下文学习」的输入法略有损失。）
    fn text_for_range(
        &mut self,
        _range_utf16: Range<usize>,
        _adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        None
    }

    /// 插入点是**光标**，不是选区：报一个零长选区。
    ///
    /// 平台拿它去问 [`Self::bounds_for_range`]，从而把候选窗摆到光标旁边。
    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: 0..0,
            reversed: false,
        })
    }

    /// 预编辑串的 UTF-16 区间。
    ///
    /// ★ 这个返回值有两个消费者，缺一不可：
    /// - macOS 用它判断 `is_composing`，组合期间把后续按键**先递给输入法**
    ///   （否则组合中按方向键会被终端吃掉）；
    /// - macOS / X11 用它判断「有没有未上屏的标记文本」。
    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        if self.preedit.is_empty() {
            None
        } else {
            Some(0..self.preedit.encode_utf16().count())
        }
    }

    /// 组合被取消（Esc / 点别处）：丢掉预编辑，**不上屏**。
    fn unmark_text(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.clear_preedit(cx);
    }

    /// 向输入法提供剪贴板内容。
    ///
    /// 覆盖默认实现（默认会走 [`Self::replace_text_in_range`]，即"当成键盘敲的字"）：
    /// 剪贴板是**粘贴**，要走 bracketed paste 包裹与换行规范化 ——
    /// 否则多行粘贴会被 zsh / fish 逐行当命令立即执行。
    ///
    /// 与快捷键那条路同走 [`Self::request_paste`]（过粘贴闸）：输入法若走
    /// `paste_text` 直通，同一个内容用 Ctrl+Shift+V 会问、用输入法却直接执行。
    fn paste(&mut self, item: ClipboardItem, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = item.text() {
            self.request_paste(&text, false, cx);
        }
    }

    /// 上屏：一段**已确认**的文字（输入法候选落定、或平台直投的字符）。
    fn replace_text_in_range(
        &mut self,
        _range_utf16: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.commit_text(text, cx);
    }

    /// 组合中：记下预编辑串，**只画不写**。
    fn replace_and_mark_text_in_range(
        &mut self,
        _range_utf16: Option<Range<usize>>,
        new_text: &str,
        _new_selected_range_utf16: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_preedit(new_text, cx);
    }

    /// 候选窗该摆哪：忽略区间，一律给**光标所在单元格**。
    ///
    /// 参数 `element_bounds` 由 GPUI 在注册输入处理器时带进来（canvas 的 bounds），
    /// 正是坐标换算需要的原点，不用再去读 [`TerminalView::content_bounds`]。
    fn bounds_for_range(
        &mut self,
        _range_utf16: Range<usize>,
        element_bounds: Bounds<Pixels>,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        Some(self.cursor_cell_bounds(element_bounds, window))
    }

    /// 点 → 文本下标：终端没有这种映射（屏幕上是远端程序画的东西，不是文本模型）。
    fn character_index_for_point(
        &mut self,
        _point: gpui::Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }

    /// 是否接受文本输入 —— 这一项**绑在会话存活上**。
    ///
    /// 返回 `false` 时平台会关掉这条通路的输入法上下文（Windows 的
    /// `ImmAssociateContextEx(..., 0)`）：断连的终端连输入法都不该弹出来。
    /// 注：这只管"输入法要不要激活"，字符消息本身仍会进来，所以
    /// [`Self::replace_text_in_range`] 里那道 `alive` 闸门不能省。
    fn accepts_text_input(&self, _window: &mut Window, _cx: &mut Context<Self>) -> bool {
        self.alive
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Process any pending events
        self.process_events(window, cx);

        // 焦点变化：闪烁只在聚焦时驱动。失焦时停表并常亮 ——
        // 后台窗口每 530ms 空转重绘毫无意义（省电），观感也与主流终端一致。
        let focused = self.focus_handle.is_focused(window);
        if focused != self.has_focus {
            self.has_focus = focused;
            self.reset_cursor_blink(cx);
            // 失焦即弃组合：平台此时已经结束了这次组合（Windows 会在失焦时提交，
            // macOS 直接取消），留着这串会一直画在屏幕上；更糟的是下次聚焦时
            // 它会拦住所有按键（`on_key_down` 的 preedit 守卫）。
            // 直接清字段、不 `notify` —— 本帧就是重绘。
            if !focused {
                self.preedit.clear();
            }
        }

        // Get terminal state and renderer for rendering
        let state_arc = self.state.term_arc();
        let resize_callback = self.resize_callback.clone();
        let window_size = self.window_size.clone();
        let padding = self.config.padding;
        let content_bounds = self.content_bounds.clone();
        let selection = self.selection.clone();
        let search = self.search.clone();
        let search_spans_cache = self.search_spans_cache.clone();
        let content_gen = self.content_gen.clone();
        let measured_for = self.measured_for.clone();
        let render_scratch = self.render_scratch.clone();
        // 实测单元格尺寸（`Cell<CellMetrics>` 是 Copy 读，无需借用检查开销）
        let cell_metrics = self.cell_metrics.clone();
        // 字体指纹（不含每帧变化的实测尺寸）
        let font_family = self.renderer.font_family.clone();
        let font_size_value = f32::from(self.renderer.font_size);
        let line_height_multiplier = self.renderer.line_height_multiplier;
        let base_palette = self.renderer.palette.clone();
        // 闪烁相位与「终端是否要求闪烁」都是 Copy 值，直接捕获进 canvas 闭包。
        // 后者不能用 `term.cursor_style().blinking`：curses 的 `cnorm`
        // （`ESC[?12l`）会把 DECSCUSR / 默认的闪烁意图一并抹掉。
        let blink_on = self.blink_on;
        let cursor_blinking = self.state.cursor_blinking();
        // 会话存活标记（同上：Copy 值进闭包，断连后连光标都不画）
        let alive = self.alive;
        // 输入法预编辑串：`paint` 要画它、光标可见性要避让它，一并进闭包
        let preedit = self.preedit.clone();
        // 注册输入处理器（IME 通路）要的两样：实体句柄 + 焦点句柄
        let this = cx.entity();
        let focus_handle = self.focus_handle.clone();

        div()
            .size_full()
            .bg(self.renderer.palette.background())
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::on_key_down))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_scroll_wheel(cx.listener(Self::on_scroll))
            .child(
                canvas(
                    move |bounds, _window, _cx| {
                        // 记录内容区 bounds，供鼠标事件做坐标换算
                        *content_bounds.borrow_mut() = bounds;
                        bounds
                    },
                    move |bounds, _, window, cx| {
                        let __paint_t0 = std::time::Instant::now();
                        use alacritty_terminal::grid::Dimensions;

                        // ★ 方案 B：`measure_cell` 移出每帧路径。
                        //
                        // 原来每帧都 `measure_cell(window)`（一次 `shape_line('│')` +
                        // 字体度量），而字体族/字号/缩放没变时结果必然相同 —— 纯浪费。
                        // 现在用「字体指纹」做缓存键，只有指纹变化或首次才重测。
                        //
                        // 同时：**不再每帧克隆整个 renderer**（含 256 项扩展调色板），
                        // 只在闭包内重建一个「实测尺寸已填好的」临时 renderer。
                        let fingerprint =
                            (font_family.clone(), font_size_value, window.scale_factor());
                        let needs_measure = measured_for.borrow().as_ref() != Some(&fingerprint);

                        let renderer = {
                            let mut r = TerminalRenderer::new(
                                font_family,
                                px(font_size_value),
                                line_height_multiplier,
                                base_palette,
                            );
                            if needs_measure {
                                r.measure_cell(window);
                                *measured_for.borrow_mut() = Some(fingerprint);
                                // 把实测尺寸写给鼠标换算用（构造值是估算，会偏移）
                                cell_metrics.set(CellMetrics {
                                    width: r.cell_width,
                                    height: r.cell_height,
                                });
                            } else {
                                // 复用上次实测尺寸（鼠标换算 / 布局同源，必须一致）
                                let CellMetrics {
                                    width: cw,
                                    height: ch,
                                } = cell_metrics.get();
                                r.cell_width = cw;
                                r.cell_height = ch;
                            }
                            r
                        };

                        // Calculate available space after padding
                        let available_width: f32 =
                            (bounds.size.width - padding.left - padding.right).into();
                        let available_height: f32 =
                            (bounds.size.height - padding.top - padding.bottom).into();
                        let cell_width_f32: f32 = renderer.cell_width.into();
                        let cell_height_f32: f32 = renderer.cell_height.into();

                        // 诊断上下文：只在异常告警时打印，正常帧开销仅一次小分配
                        let cached_m = cell_metrics.get();
                        let dim_ctx = format!(
                            "needs_measure={needs_measure} cached=({},{})",
                            <f32 as From<Pixels>>::from(cached_m.width),
                            <f32 as From<Pixels>>::from(cached_m.height),
                        );
                        let cols = safe_grid_dim(
                            available_width,
                            cell_width_f32,
                            font_size_value,
                            80,
                            MAX_GRID_COLS,
                            "cols",
                            &dim_ctx,
                        );
                        let rows = safe_grid_dim(
                            available_height,
                            cell_height_f32,
                            font_size_value,
                            24,
                            MAX_GRID_ROWS,
                            "rows",
                            &dim_ctx,
                        );

                        // Helper struct implementing Dimensions for resize
                        struct TermSize {
                            cols: usize,
                            rows: usize,
                        }
                        impl Dimensions for TermSize {
                            fn total_lines(&self) -> usize {
                                self.rows
                            }
                            fn screen_lines(&self) -> usize {
                                self.rows
                            }
                            fn columns(&self) -> usize {
                                self.cols
                            }
                            fn last_column(&self) -> alacritty_terminal::index::Column {
                                alacritty_terminal::index::Column(self.cols.saturating_sub(1))
                            }
                            fn bottommost_line(&self) -> alacritty_terminal::index::Line {
                                alacritty_terminal::index::Line(self.rows as i32 - 1)
                            }
                            fn topmost_line(&self) -> alacritty_terminal::index::Line {
                                alacritty_terminal::index::Line(0)
                            }
                        }

                        // Resize terminal if dimensions changed
                        let mut term = state_arc.lock();
                        let current_cols = term.columns();
                        let current_rows = term.screen_lines();
                        if cols != current_cols || rows != current_rows {
                            // ★ 顺序约束：必须**先释放 term 锁**再通知 PTY。
                            //   resize 回调底层是 ConPTY 的 `ResizePseudoConsole`
                            //   —— 跨进程同步调用（ConDrv → conhost.exe），可能长时间
                            //   不返回。持锁调用会连带把 `process_bytes`（字节解析，
                            //   同锁）和整条渲染路径拖死，放大成整窗「未响应」。
                            drop(term);
                            // Notify the PTY about the resize
                            if let Some(ref callback) = resize_callback {
                                let __t = std::time::Instant::now();
                                callback(cols, rows);
                                let __us = __t.elapsed().as_micros() as u64;
                                crate::perf::bump(&crate::perf::SEND_N);
                                crate::perf::add_us(&crate::perf::SEND_SUM_US, __us);
                                crate::perf::update_max(&crate::perf::SEND_MAX_US, __us);
                            }
                            term = state_arc.lock();
                            let __t = std::time::Instant::now();
                            term.resize(TermSize { cols, rows });
                            let __us = __t.elapsed().as_micros() as u64;
                            crate::perf::bump(&crate::perf::RESIZE_N);
                            crate::perf::add_us(&crate::perf::RESIZE_SUM_US, __us);
                            crate::perf::update_max(&crate::perf::RESIZE_MAX_US, __us);
                        }

                        // 刷新 `CSI 14 t` 应答用的文本区像素尺寸
                        {
                            let mut ws = window_size.lock();
                            ws.num_lines = rows as u16;
                            ws.num_cols = cols as u16;
                            ws.cell_width = cell_width_f32.round().max(0.0) as u16;
                            ws.cell_height = cell_height_f32.round().max(0.0) as u16;
                        }

                        // 光标绘制参数：形状取自 DECSCUSR，可见性由三件事决定 ——
                        // `ESC[?25h/l`（vim/less 会隐藏）、闪烁要求的实时值、闪烁相位。
                        // 闪烁要求用视图每帧现读的 `cursor_blinking`（两路来源
                        // 合并后的结果），而不是 `term.cursor_style().blinking`。
                        //
                        // 会话已死时一律不画：闪动的光标是「可以打字」的承诺，
                        // 而断连的终端收不了任何输入。
                        let cursor = {
                            let style = term.cursor_style();
                            CursorPaint {
                                shape: style.shape,
                                // 组合中不画光标：预编辑串正画在那个格子上，
                                // 再叠一个光标会把第一个候选字糊住；主流终端
                                // 与输入框在组合期间也都是「只显示待选字」。
                                visible: alive
                                    && preedit.is_empty()
                                    && crate::render::cursor_visible(
                                        term.mode().contains(
                                            alacritty_terminal::term::TermMode::SHOW_CURSOR,
                                        ),
                                        style.shape,
                                        cursor_blinking,
                                        blink_on,
                                    ),
                            }
                        };

                        // 搜索命中区间（Ctrl+F：高亮视口内全部匹配）。
                        //
                        // 在画终端**之前**算好并交给渲染器，由它画在字形之下 ——
                        // 旧实现是画完终端再叠色块，色块压在字形上、把命中的文字
                        // 糊住（2026-09-16 用户报「匹配到的文字被遮蔽」）。
                        //
                        // 2026-09-18：结果按 [`SearchKey`] 缓存，扫描范围限定在视口
                        // （见 `collect_search_spans`）。此前每帧都把整条 scrollback
                        // 正则扫一遍，"查找"一打开终端就卡死。
                        let search_spans: Vec<crate::render::PaintSpan> = {
                            let mut out = Vec::new();
                            let mut sg = search.lock();
                            if let Some(q) = sg.as_mut() {
                                let key = SearchKey {
                                    query: q.ver,
                                    offset: term.grid().display_offset() as i32,
                                    cols: term.columns(),
                                    rows: term.screen_lines(),
                                    content: content_gen.load(std::sync::atomic::Ordering::Relaxed),
                                };
                                let mut cache = search_spans_cache.lock();
                                match cache.as_ref() {
                                    Some((cached, spans)) if *cached == key => {
                                        out.clone_from(spans);
                                    }
                                    _ => {
                                        out = collect_search_spans(&term, &mut q.regex);
                                        *cache = Some((key, out.clone()));
                                    }
                                }
                            } else {
                                // 无搜索：顺手把缓存丢掉，别让上一次的命中在下次
                                // 打开查找时「先闪一下旧结果」
                                *search_spans_cache.lock() = None;
                            }
                            out
                        };

                        // 注册文本输入处理器 —— 输入法 / 平台字符消息的收件人
                        // （见 `impl EntityInputHandler for TerminalView`）。
                        //
                        // ★ 必须在**绘制期**装：`Window::handle_input` 只在 paint
                        // 阶段可调用，且只在焦点在本视图时生效；每帧装一次是它的
                        // 既定用法（gpui 每帧从 `next_frame.input_handlers` 里取
                        // 最后一份交给平台窗口）。这也是断连后不该出现输入法的原因 ——
                        // `accepts_text_input()` 返回 false 时平台会关掉 IME 上下文。
                        //
                        // 装在 canvas 的 paint 闭包里（而不是 `render()` 里）是因为
                        // 这里的 `bounds` 正是候选窗定位要用的原点。
                        window.handle_input(
                            &focus_handle,
                            ElementInputHandler::new(bounds, this.clone()),
                            cx,
                        );

                        // Paint the terminal with measured dimensions
                        renderer.paint(
                            bounds,
                            padding,
                            &term,
                            cursor,
                            &search_spans,
                            &preedit,
                            &mut render_scratch.lock(),
                            window,
                            cx,
                        );

                        // 画划选高亮（逐行矩形，覆盖所选单元格范围）
                        let sel = selection.borrow().clone();
                        if let Some(sel) = sel {
                            let (start, end) = if sel.start <= sel.end {
                                (sel.start, sel.end)
                            } else {
                                (sel.end, sel.start)
                            };
                            let cols = term.columns();
                            let cw: f32 = renderer.cell_width.into();
                            let ch: f32 = renderer.cell_height.into();
                            let ox: f32 = (bounds.origin.x + padding.left).into();
                            let oy: f32 = (bounds.origin.y + padding.top).into();
                            let highlight = renderer.palette.selection();
                            for row in start.line.0..=end.line.0 {
                                let col_start = if row == start.line.0 {
                                    start.column.0
                                } else {
                                    0
                                };
                                let col_end = if row == end.line.0 {
                                    end.column.0
                                } else {
                                    cols.saturating_sub(1)
                                };
                                if col_end < col_start {
                                    continue;
                                }
                                let rect = gpui::Bounds::new(
                                    gpui::Point::new(
                                        px(ox + col_start as f32 * cw),
                                        px(oy + row as f32 * ch),
                                    ),
                                    gpui::Size::new(
                                        px((col_end - col_start + 1) as f32 * cw),
                                        px(ch),
                                    ),
                                );
                                window.paint_quad(gpui::fill(rect, highlight));
                            }
                        }

                        let __us = __paint_t0.elapsed().as_micros() as u64;
                        crate::perf::bump(&crate::perf::PAINT_N);
                        crate::perf::add_us(&crate::perf::PAINT_SUM_US, __us);
                        crate::perf::update_max(&crate::perf::PAINT_MAX_US, __us);
                    },
                )
                .size_full(),
            )
    }
}

// Tests are omitted due to macro expansion issues with the test attribute
// in this configuration. Integration tests can be added separately.

#[cfg(test)]
mod tests {
    use super::strip_prompt;

    // ───────────────────────── H1 prompt 边界（纯函数） ─────────────────────────

    /// 典型 `user@host:~$ ` 形态：行首到第一个非空白之间的都算 prompt。
    #[test]
    fn strip_prompt_takes_leading_whitespace_as_the_prompt() {
        let (cols, text) = strip_prompt("user@host:~$ ls -la");
        // 这里 prompt 与命令**连在一起**没有空白分隔 —— H1 只认行首空白，
        // 所以这种「prompt 末尾无空格」的形态它切不开，文本是整行。
        assert_eq!(text, "user@host:~$ ls -la");
        assert_eq!(cols, 0);
    }

    #[test]
    fn strip_prompt_handles_the_common_spaced_prompt() {
        // 真实终端里 prompt 后面总有一个空格，且网格里 prompt 之前没有空白 ——
        // 这让 H1 能切开。构造一个"prompt 是空白 + 命令"的场景：
        let (cols, text) = strip_prompt("   ls -la");
        assert_eq!(cols, 3);
        assert_eq!(text, "ls -la");
    }

    #[test]
    fn strip_prompt_keeps_inner_and_trailing_spaces() {
        // 命令中间的多个空格、以及末尾刚敲的空格，都是用户真实输入，不能吃掉
        let (_, text) = strip_prompt("  git commit  -m 'x' ");
        assert_eq!(text, "git commit  -m 'x' ");
    }

    #[test]
    fn strip_prompt_on_blank_line_yields_empty_text() {
        let (cols, text) = strip_prompt("        ");
        assert_eq!(cols, 8);
        assert!(text.is_empty(), "整行空白 = 还没有输入");
    }

    #[test]
    fn strip_prompt_on_empty_line() {
        assert_eq!(strip_prompt(""), (0, String::new()));
    }

    #[test]
    fn strip_prompt_counts_tabs_as_prompt_width() {
        let (cols, text) = strip_prompt("\t\tls");
        assert_eq!(cols, 2);
        assert_eq!(text, "ls");
    }
}
