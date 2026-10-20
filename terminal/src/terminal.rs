//! Terminal state management.
//!
//! This module provides [`TerminalState`], a thread-safe wrapper around alacritty's
//! [`Term`] structure. It manages the terminal
//! emulator state, including the character grid, cursor position, and VTE parser.
//!
//! # Architecture
//!
//! `TerminalState` wraps the alacritty terminal in `Arc<Mutex<>>` to allow safe
//! concurrent access from:
//!
//! - The async reader task (writing bytes to the terminal)
//! - The render thread (reading the grid for display)
//! - The main thread (handling resize events)
//!
//! # VTE Parsing
//!
//! The terminal uses alacritty's VTE parser to process byte streams. When bytes
//! arrive from the PTY, they are fed through the parser via [`process_bytes`],
//! which calls handler methods on the `Term` to update the grid:
//!
//! ```text
//! PTY bytes → VTE Parser → Term handlers → Grid updates
//!                          ├─ print()     (regular characters)
//!                          ├─ execute()   (control chars: BEL, BS, etc.)
//!                          ├─ esc_dispatch()  (escape sequences)
//!                          └─ csi_dispatch()  (CSI sequences: colors, cursor, etc.)
//! ```
//!
//! # Example
//!
//! ```
//! use std::sync::mpsc::channel;
//! use rustshell_terminal::event::GpuiEventProxy;
//! use rustshell_terminal::terminal::TerminalState;
//!
//! let (tx, rx) = channel();
//! let event_proxy = GpuiEventProxy::new(tx);
//! let mut terminal = TerminalState::new(80, 24, event_proxy);
//!
//! // Process some output (e.g., colored text)
//! terminal.process_bytes(b"\x1b[31mRed text\x1b[0m");
//! ```
//!
//! [`process_bytes`]: TerminalState::process_bytes

use crate::event::{GpuiEventProxy, TerminalEvent};
use crate::shell_integration::{ShellIntegration, parse_osc};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{
    CursorShape, CursorStyle, Handler, NamedPrivateMode, PrivateMode, Processor,
};
use parking_lot::Mutex;
use std::sync::Arc;

/// 未指定回滚历史时的默认行数（与 alacritty 默认保持一致）。
pub const DEFAULT_SCROLLING_HISTORY: usize = 10_000;

/// 回滚历史上限：settings.json 是可手改的，一个多打的 0 就能把内存吃光，
/// 这里做一次封顶，宁可静默截断也不让进程被撑爆。
pub const MAX_SCROLLING_HISTORY: usize = 1_000_000;

/// 把请求的回滚行数收进 `[0, MAX_SCROLLING_HISTORY]`。
fn clamped_scrolling_history(requested: usize) -> usize {
    requested.min(MAX_SCROLLING_HISTORY)
}

/// Simple dimensions implementation for terminal initialization.
struct TermDimensions {
    columns: usize,
    screen_lines: usize,
}

impl TermDimensions {
    fn new(columns: usize, screen_lines: usize) -> Self {
        Self {
            columns,
            screen_lines,
        }
    }
}

impl Dimensions for TermDimensions {
    fn total_lines(&self) -> usize {
        // For initial setup, total lines equals screen lines
        // The scrollback buffer will be managed by the Term itself
        self.screen_lines
    }

    fn screen_lines(&self) -> usize {
        self.screen_lines
    }

    fn columns(&self) -> usize {
        self.columns
    }

    fn last_column(&self) -> alacritty_terminal::index::Column {
        alacritty_terminal::index::Column(self.columns.saturating_sub(1))
    }
}

// ───────────────────────── Shell integration（OSC 133 / 633 / 7）感知层 ─────────────────────────
//
// 这一节实现"听懂远端 shell 说出来的语义"，供后续阶段（命令块、补全、跳转上一提示符）使用。
// 纯被动：不发字节、不改远端环境、不改网格渲染。

// Shell integration 的感知层（`OSC 133` / `633` / `7`）。
//
// # 为什么不能"顺手在 `Perform` 上截一下"
//
// 第一直觉是"包一层 `Perform`、覆写 `osc_dispatch`、认领 133/633/7、
// 其余转发给 `Handler`"。**这条路在 vte 0.15 上走不通**，原因在
// `vte-0.15.0/src/ansi.rs`：
//
// - `Handler` trait（行 495–732）里**根本没有 `osc_dispatch` 这个方法**。
//   它把 OSC 拆成了 `set_title` / `set_color` / `dynamic_color_sequence` /
//   `clipboard_store` / `clipboard_load` / `set_hyperlink` / `set_mouse_cursor_icon`
//   等专用方法。
// - 真正把 `OSC 4` 翻译成 `Handler::dynamic_color_sequence` 的，
//   是 `Processor` 内部的 `Performer`（`impl Perform for Performer`，行 1284），
//   而它的 OSC 分发**不经过 `Perform::osc_dispatch`** —— 是它自己直接调
//   `self.handler.xxx()`。
// - 兜底分支 `_ => unhandled(params)` 只打一条 `debug!` 日志，**没有任何回调**。
//
// 于是"覆写 `Perform::osc_dispatch` 再把不认领的重放给 `Handler`"这种写法
// 有两重坏处：被 vte 认领的 OSC（4/52/0/2…）**根本不会**在 `Perform::osc_dispatch`
// 上出现（所以那条转发是死代码），而不认领的重放到 `Handler` 上也没有接收方
// （因为 `Handler` 里没有 `osc_dispatch`）。
//
// # 实际做法：把 OSC 从字节流里摘出来，自己决定谁看
//
// [`TerminalState::dispatch`] 先用 [`split_osc_segments`] 把输入切成
// 「普通/转义段」与「整条 OSC 段」：
//
// | 段 | 去处 |
// |---|---|
// | 本层认领的 OSC（133 / 633 / 7） | 投 shell integration 事件，**不进 `Processor`** |
// | 其余一切（含**不认领的 OSC**） | 原样喂 `Processor::advance` |
//
// 第二步里"不认领的 OSC 也要喂回去"是**关键**：`OSC 4` 颜色查询、
// `OSC 52` 剪贴板、`OSC 0/2` 标题这些能力的**唯一入口**就是"让 vte 自己解析到它"
// （见上面第一条）。摘掉就等于静默废掉这些功能。
//
// 所以本层不做任何"转发"，只做"**从流里摘走我们那一小撮**"，
// 剩下的字节 vte 看到的与包装前完全一致。
//
// 相关回归测试：`event.rs` 的 `dsr_cursor_position_query_is_answered_on_pty`、
// `color_request_is_answered_on_pty`、`test_clipboard_load_keeps_alacritty_formatter`。
// 它们能过，就说明"其余 OSC 仍然原样抵达 vte"。
//
//
//

/// 一个投递单元：字节区间 + 它是什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SegmentKind {
    /// 非 OSC 字节（普通文字、CSI / ESC / DCS）。
    Plain,
    /// 一条**完整**的 OSC（首尾齐全）。
    Osc,
    /// 一条**没收尾**的 OSC（被字节边界截断），区间从引导符到输入末尾。
    OscUnterminated,
}

/// 把字节流按「OSC 起点」切分，返回 `(start, end, kind)`。
///
/// # 判据
///
/// OSC 的引导符是 `ESC ]`（0x1B 0x5D）。终点按 xterm 规矩有两种收尾：
/// - `BEL`（0x07）；
/// - `ST` = `ESC \`（0x1B 0x5C）。
///
/// # 为什么要把「没收尾」单独标出来
///
/// 收尾与否决定了这段字节**能不能被判断**：一条完整的 OSC 我们可以立刻
/// 决定"认领 / 交给 vte"；一条被切断的 OSC 两种决定都做不了 ——
/// 认领会漏掉后半截的参数，交给 vte 又会让参数分隔符在跨块处错位
/// （vte 只在遇到 `;` 时推进参数下标）。所以调用方必须把它**攒起来**
/// （见 [`TerminalState::pending_osc`]），而不是就地处置。
fn split_osc_segments(bytes: &[u8]) -> Vec<(usize, usize, SegmentKind)> {
    /// `ESC ]`：OSC 引导符
    const OSC_INTRO: [u8; 2] = [0x1b, b']'];

    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        // 找下一个 OSC 起点
        let Some(rel) = bytes[i..].windows(2).position(|w| w == OSC_INTRO) else {
            out.push((i, bytes.len(), SegmentKind::Plain));
            break;
        };
        let intro = i + rel;

        // OSC 起点之前的普通字节自成一段
        if intro > i {
            out.push((i, intro, SegmentKind::Plain));
        }

        // 从 OSC 起点往后找收尾（BEL 或 ST）
        let mut end = None;
        let mut j = intro + 2;
        while j < bytes.len() {
            if bytes[j] == 0x07 {
                end = Some(j + 1); // 含 BEL
                break;
            }
            if bytes[j] == 0x1b && bytes.get(j + 1) == Some(&b'\\') {
                end = Some(j + 2); // 含 ST
                break;
            }
            j += 1;
        }

        match end {
            Some(e) => {
                out.push((intro, e, SegmentKind::Osc));
                i = e;
            }
            None => {
                // 剩下的全都是这半条 OSC
                out.push((intro, bytes.len(), SegmentKind::OscUnterminated));
                i = bytes.len();
            }
        }
    }
    out
}

/// 光标闪烁的两路来源，分开记。
///
/// # 为什么不能只看 `Term::cursor_style().blinking`
///
/// alacritty 把两路来源合成同一个 `blinking` 位：DECSET/DECRST 12
/// （`ESC[?12h|l`）与 DECSCUSR（`CSI n SP q`）。后写的那一路覆盖先写的，
/// 于是 curses 程序挂起/退出时发的 `cnorm`（`ESC[?12l ESC[?25h`）
/// 会把 DECSCUSR / 用户默认的「闪」一并清掉 —— 表现为 `top` 里 Ctrl+Z
/// 之后光标还在、却再也不闪。
///
/// foot#218 的修法：两路状态各记一份，**任一要求闪烁就闪**。
/// `cnorm` 只关掉 `decset` 这一路，另一路（DECSCUSR，未收到时即配置默认）
/// 仍为真，因此闪烁不会被一次 `?12l` 永久关死；而显式的稳定 DECSCUSR
/// （`CSI 2 q`）在 `decset` 为假时确实能让光标停闪。
#[derive(Debug)]
struct CursorBlinkTracker {
    /// `ESC[?12h`（true）/ `ESC[?12l`（false）。默认 false。
    decset: bool,
    /// 最近一次 DECSCUSR 的闪烁位；`CSI 0 q` 恢复为配置默认。
    decscusr: bool,
    /// 配置默认是否闪烁（构造时由默认光标样式决定）。
    default_blink: bool,
}

impl CursorBlinkTracker {
    fn new(default_blink: bool) -> Self {
        Self {
            decset: false,
            decscusr: default_blink,
            default_blink,
        }
    }

    fn blinking(&self) -> bool {
        self.decset || self.decscusr
    }

    fn is_blink_mode(mode: PrivateMode) -> bool {
        matches!(mode, PrivateMode::Named(NamedPrivateMode::BlinkingCursor))
    }
}

impl Handler for CursorBlinkTracker {
    fn set_private_mode(&mut self, mode: PrivateMode) {
        if Self::is_blink_mode(mode) {
            self.decset = true;
        }
    }

    fn unset_private_mode(&mut self, mode: PrivateMode) {
        if Self::is_blink_mode(mode) {
            self.decset = false;
        }
    }

    fn set_cursor_style(&mut self, style: Option<CursorStyle>) {
        // `None` 是 `CSI 0 q`：回到配置默认，而不是关掉闪烁。
        self.decscusr = style.map_or(self.default_blink, |s| s.blinking);
    }

    fn reset_state(&mut self) {
        self.decset = false;
        self.decscusr = self.default_blink;
    }
}

/// Thread-safe terminal state wrapper.
///
/// This struct wraps alacritty's [`Term`] in an
/// `Arc<parking_lot::Mutex<>>` to allow safe concurrent access from multiple threads.
/// It also manages the VTE parser for processing incoming bytes from the PTY.
///
/// # Thread Safety
///
/// The terminal state can be safely shared across threads:
///
/// - Use [`term_arc`](Self::term_arc) to get a cloned `Arc` for sharing
/// - Use [`with_term`](Self::with_term) for read access to the grid
/// - Use [`with_term_mut`](Self::with_term_mut) for write access
///
/// The mutex is held only for the duration of the closure, minimizing contention.
///
/// # Grid Access
///
/// The terminal grid is accessed through the `Term` structure:
///
/// ```ignore
/// terminal_state.with_term(|term| {
///     let grid = term.grid();
///     let cursor = grid.cursor.point;
///     let cell = &grid[cursor];
///     // Read cell content, colors, flags, etc.
/// });
/// ```
///
/// # Performance Notes
///
/// - `parking_lot::Mutex` is used for faster locking than `std::sync::Mutex`
/// - Lock contention is minimized by keeping critical sections short
/// - The VTE parser state is kept outside the mutex (only accessed from one thread)
///
/// # 线程与锁（本仓补充）
///
/// [`TerminalState`] 本身跑在**读线程 / UI 线程**上（见上文 Architecture 说明），
/// 所以它自己的字段没有锁、也不需要锁 —— 真正的共享点是内部那个
/// `Arc<Mutex<Term>>`（渲染线程要读网格）。
pub struct TerminalState {
    /// The underlying alacritty terminal emulator.
    term: Arc<Mutex<Term<GpuiEventProxy>>>,

    /// VTE parser for converting byte streams into terminal actions.
    parser: Processor,

    /// 与 [`parser`](Self::parser) 并行的第二路解析器：只喂给
    /// [`CursorBlinkTracker`]，不碰网格。
    ///
    /// 必须独立：同一个 `Processor` 不能把同一批字节解两遍（它是有状态的
    /// 状态机），而掩码层（`Term`）已经把两路闪烁来源合成一个位，
    /// 事后再拆不出来。
    blink_parser: Processor,

    /// 两路闪烁来源的独立状态，见 [`CursorBlinkTracker`]。
    blink_tracker: CursorBlinkTracker,

    /// 跨调用滞留的「半条 OSC」原始字节。
    ///
    /// PTY 按 4KB 投递，`ESC ]133;D;0` 被切成两块是常态。若把没收尾的半条
    /// 直接喂给内层 `Processor`，虽然字符本身不丢，但**参数分隔符会被 vte 吃掉**：
    /// `ESC ]133;D;` + `7 BEL` 在 vte 眼里是 `["133","D","7"]`… 而更多时候
    /// 半条的尾字节会与下一块拼出歧义（vte 的 OSC 参数只在遇到 `;` 时推进下标，
    /// 跨块时 `;` 已经过去了）。
    ///
    /// 所以这里自己留一份：**只要一段 OSC 没收尾，就攒起来不投**，
    /// 等下一块补齐收尾符后一次性认领或一次性交给内层。
    /// 这样"认领判断"看到的永远是**完整的一条**。
    ///
    /// 上限见 [`MAX_PENDING_OSC`]：远端可以发一条永远不收尾的 OSC，
    /// 无上限累积会把内存吃光。
    pending_osc: Vec<u8>,

    /// Number of columns in the terminal.
    cols: usize,

    /// Number of rows (lines) in the terminal.
    rows: usize,
}

/// `pending_osc` 的上限。
///
/// 取 8 KiB：远大于任何正常的 OSC（vte 自己的原始缓冲上限是
/// `MAX_OSC_RAW = 1024`，连它的 8 倍都不到会显得比上游还严），
/// 又小到"恶意发一条永不收尾的 OSC"撑不爆内存。
///
/// 溢出时的处置见 [`TerminalState::dispatch`]：**直接丢掉**并复位到地面态。
/// 理由与 vte 一致 —— 一条超过 8KB 的 OSC 已经不是协议内的东西了，
/// 继续攒下去只会拖垮整个会话（vte 对超限 OSC 也是静默丢弃）。
const MAX_PENDING_OSC: usize = 8 * 1024;

impl TerminalState {
    /// Create a new terminal state with the given dimensions.
    ///
    /// # Arguments
    ///
    /// * `cols` - The number of columns (character width) of the terminal
    /// * `rows` - The number of rows (lines) of the terminal
    /// * `event_proxy` - The event proxy for forwarding terminal events to GPUI
    ///
    /// # Returns
    ///
    /// A new `TerminalState` instance.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::mpsc::channel;
    /// use rustshell_terminal::event::GpuiEventProxy;
    /// use rustshell_terminal::terminal::TerminalState;
    ///
    /// let (tx, rx) = channel();
    /// let event_proxy = GpuiEventProxy::new(tx);
    /// let terminal = TerminalState::new(80, 24, event_proxy);
    /// ```
    pub fn new(cols: usize, rows: usize, event_proxy: GpuiEventProxy) -> Self {
        Self::with_scrollback(cols, rows, DEFAULT_SCROLLING_HISTORY, event_proxy)
    }

    /// 与 [`new`](Self::new) 相同，但可指定回滚历史行数。
    ///
    /// `new` 走 alacritty 默认的 10000 行；宿主把用户的「回滚缓冲」设置传进来，
    /// 让该设置真正生效（此前它只落在结构体字段上，从不被消费）。
    pub fn with_scrollback(
        cols: usize,
        rows: usize,
        scrolling_history: usize,
        event_proxy: GpuiEventProxy,
    ) -> Self {
        Self::with_cursor_default(
            cols,
            rows,
            scrolling_history,
            CursorShape::Block,
            event_proxy,
        )
    }

    /// 与 [`with_scrollback`](Self::with_scrollback) 相同，但可指定**默认光标形状**。
    ///
    /// 2026-09-28 补：宿主新增「光标样式」设置（块状 / 竖线 / 下划线），此前这个值
    /// 被硬编码成 `CursorShape::Block`，用户对光标形状零控制权。
    ///
    /// 语义是**默认值**而非强制值：远端程序发 DECSCUSR（`ESC[1 q` / `ESC[5 q` …）
    /// 或 `ESC[?12h|l` 时仍会覆盖它 —— 用户设置是底色，程序请求是前景，
    /// 与 `terminal_cursor_blink` 同一条口径。
    pub fn with_cursor_default(
        cols: usize,
        rows: usize,
        scrolling_history: usize,
        default_cursor_shape: CursorShape,
        event_proxy: GpuiEventProxy,
    ) -> Self {
        // 终端配置：只覆写我们真正需要收口的项，其余沿用 alacritty 默认。
        // 默认光标样式必须是**闪烁**的 —— `CursorStyle::default()` 的
        // `blinking` 为 false，若照搬默认值，本地 shell 这类不发 DECSCUSR 的
        // 会话将永远不闪；远端发 `ESC[?12l` 或 DECSCUSR 时仍会覆盖这里。
        // 形状同理：`Hidden` 是非法输入（会让光标彻底消失），回退到块状。
        let shape = if matches!(default_cursor_shape, CursorShape::Hidden) {
            CursorShape::Block
        } else {
            default_cursor_shape
        };
        let config = Config {
            scrolling_history: clamped_scrolling_history(scrolling_history),
            default_cursor_style: CursorStyle {
                shape,
                blinking: true,
            },
            ..Config::default()
        };
        // DECSCUSR 未出现时，「另一路」闪烁意图就是这个默认值
        let default_blink = config.default_cursor_style.blinking;

        // Create dimensions for terminal initialization
        let dimensions = TermDimensions::new(cols, rows);

        // Create the terminal with the given configuration and dimensions
        let term = Term::new(config, &dimensions, event_proxy);

        // Create the VTE parser for processing incoming bytes
        let parser = Processor::new();

        Self {
            term: Arc::new(Mutex::new(term)),
            parser,
            blink_parser: Processor::new(),
            blink_tracker: CursorBlinkTracker::new(default_blink),
            pending_osc: Vec::new(),
            cols,
            rows,
        }
    }

    /// Process incoming bytes from the PTY.
    ///
    /// This method feeds the bytes through the VTE parser, which will call
    /// the appropriate handler methods on the terminal to update its state.
    ///
    /// # Arguments
    ///
    /// * `bytes` - The bytes received from the PTY
    ///
    /// # Examples
    ///
    /// ```
    /// # use std::sync::mpsc::channel;
    /// # use rustshell_terminal::event::GpuiEventProxy;
    /// # use rustshell_terminal::terminal::TerminalState;
    /// # let (tx, rx) = channel();
    /// # let event_proxy = GpuiEventProxy::new(tx);
    /// # let mut terminal = TerminalState::new(80, 24, event_proxy);
    /// // Process some output from the PTY
    /// terminal.process_bytes(b"Hello, world!\r\n");
    /// ```
    pub fn process_bytes(&mut self, bytes: &[u8]) {
        self.dispatch(bytes, None);
    }

    /// 同 [`process_bytes`](Self::process_bytes)，但把 shell integration 事件投给 `tx`。
    ///
    /// 生产路径用这一条（`TerminalView` 的读线程把 `TerminalEvent::PromptMark`
    /// 等转给视图）。与 `process_bytes` 分成两个入口，是为了让不需要事件的
    /// 纯解析路径（单测、无头场景）不必凭空造一个通道。
    pub fn process_bytes_with_events(
        &mut self,
        bytes: &[u8],
        tx: &std::sync::mpsc::Sender<TerminalEvent>,
    ) {
        self.dispatch(bytes, Some(tx));
    }

    /// 两条入口的共同实现：按 OSC 边界切分后逐段投递。
    ///
    /// # 每一段的去处
    ///
    /// 1. **本层认领的 OSC**（133 / 633 / 7）⇒ 投事件，**这一段就到此为止**
    ///    （vte 本来也不认它，喂进去只会打一条 `debug!` 日志）。
    /// 2. **其余段**（普通字节、CSI / ESC / DCS、**以及不认领的 OSC**）
    ///    ⇒ 原样喂内层 `Processor`。
    ///
    /// 第 2 条里"不认领的 OSC 也喂进去"是**关键**：vte 的 OSC 动作分发
    /// （`OSC 4` 颜色查询、`OSC 52` 剪贴板、`OSC 0/2` 标题…）是在它自己的
    /// `Performer::osc_dispatch` 里做的，**不经过 `Perform` trait** ——
    /// 也就是说这些能力的唯一入口就是"让 vte 解析到这条 OSC"。
    /// 屏蔽掉它们，剪贴板与颜色查询会集体失效且没有任何报错
    /// （回归测试：`event.rs` 里的 `dsr_*` / `color_request_*` / `clipboard_*`）。
    ///
    /// 内层拿到的是**同一批字节、同样的顺序**（只是被切成几段、认领的那几条被摘掉），
    /// 而 `Processor` 是跨调用有状态的状态机 ⇒ 对网格的结果逐字节等价。
    fn dispatch(&mut self, bytes: &[u8], tx: Option<&std::sync::mpsc::Sender<TerminalEvent>>) {
        let mut term = self.term.lock();

        // 先处理"上一轮滞留的半条 OSC"：与这块的开头拼起来再看。
        //
        // 拼之前不能直接把 `bytes` 一整块喂给内层 —— 那样滞留的半条就白留了。
        // 做法：把滞留内容与本次输入**先接上**，再走同一条切分逻辑。
        // 拼接成本只发生在真的有半条待续时（正常情况是零），
        // 且半条上限 8KB ⇒ 一次 copy 的代价可忽略。
        let borrowed;
        let input: &[u8] = if self.pending_osc.is_empty() {
            bytes
        } else {
            let mut joined = std::mem::take(&mut self.pending_osc);
            joined.extend_from_slice(bytes);
            borrowed = joined;
            &borrowed
        };

        for (start, end, kind) in split_osc_segments(input) {
            let seg = &input[start..end];

            match kind {
                SegmentKind::Plain => {
                    // 闪烁跟踪器只吃普通/转义段。OSC 里不可能出现
                    // `?12h|l` 或 `CSI n q`，而「半条 OSC」在下一轮会被
                    // 与新增字节拼起来重放 —— 跟踪器若也跟着看，就会把
                    // 已经看过的半条再看一遍，状态机与主解析器错位。
                    self.blink_parser.advance(&mut self.blink_tracker, seg);
                    self.parser.advance(&mut *term, seg);
                }
                SegmentKind::Osc => {
                    // 完整的一条：认领就到此为止，不认领就原样交给 vte
                    // （`OSC 4`/`52`/`0`… 的唯一入口就是让 vte 解析到它）。
                    if !self.try_claim_osc(seg, tx) {
                        self.parser.advance(&mut *term, seg);
                    }
                }
                SegmentKind::OscUnterminated => {
                    // 半条：攒起来，等下一块补齐。**不喂内层**（理由见
                    // `split_osc_segments` 与 `pending_osc` 的文档）。
                    // 注意它是循环里的最后一段（后面没字节了），直接 break 即可。
                    if seg.len() > MAX_PENDING_OSC {
                        // 超限 ⇒ 丢掉并复位。这与 vte 对超长 OSC 的处置一致：
                        // 协议内不存在这么大的 OSC，继续攒只会拖垮会话。
                        // 但要**补喂一个 CAN（0x18）给内层**：让 vte 的解析器
                        // 从"OSC 字符串中间"退回地面态，否则它会一直以为
                        // 自己还在 OSC 里，后续所有字节都被吃掉。
                        self.parser.advance(&mut *term, &[0x18]);
                    } else {
                        self.pending_osc.extend_from_slice(seg);
                    }
                    break;
                }
            }
        }
    }

    /// 试着把一段完整的 OSC 认领下来（投事件）。
    ///
    /// 返回 `true` 表示"已认领、不要再交给内层"。
    ///
    /// 参数切分口径**逐条对齐 vte**（见 `vte-0.15.0/src/lib.rs` 的
    /// `osc_dispatch`）：去掉 `ESC ]` 引导符与收尾符后，按 `;` 切；
    /// **空段被跳过**（vte 只在遇到分隔符时推进参数下标）。这一条极重要：
    /// `OSC 133;D;;host` 切出来是 `["133","D","host"]` 而不是带空串的四段，
    /// 若不照着做，退出码解析会与真实 vte 环境下不一致
    /// （详见 `shell_integration::parse_osc` 的文档）。
    fn try_claim_osc(
        &self,
        seg: &[u8],
        tx: Option<&std::sync::mpsc::Sender<TerminalEvent>>,
    ) -> bool {
        // 去掉 `ESC ]`
        let Some(body) = seg.strip_prefix(b"\x1b]") else {
            return false;
        };
        // 去掉收尾：BEL 或 ST
        let body = if let Some(stripped) = body.strip_suffix(b"\x07") {
            stripped
        } else if let Some(stripped) = body.strip_suffix(b"\x1b\\") {
            stripped
        } else {
            // 没有收尾 ⇒ 半条 OSC。不认领（也不投递）：交给内层按状态机累积。
            return false;
        };

        let params: Vec<&[u8]> = body
            .split(|b| *b == b';')
            .filter(|s| !s.is_empty())
            .collect();
        let Some(parsed) = parse_osc(&params) else {
            return false;
        };

        if let Some(tx) = tx {
            let event = match parsed {
                ShellIntegration::Mark(m) => TerminalEvent::PromptMark(m),
                ShellIntegration::Cwd(cwd) => TerminalEvent::Cwd(cwd),
                ShellIntegration::CommandLine(line) => TerminalEvent::CommandLine(line),
            };
            let _ = tx.send(event);
        }
        true
    }

    /// Resize the terminal to new dimensions.
    ///
    /// This method updates the terminal's internal grid to match the new size.
    /// It should be called when the terminal view is resized.
    ///
    /// # Arguments
    ///
    /// * `cols` - The new number of columns
    /// * `rows` - The new number of rows
    ///
    /// # Examples
    ///
    /// ```
    /// # use std::sync::mpsc::channel;
    /// # use rustshell_terminal::event::GpuiEventProxy;
    /// # use rustshell_terminal::terminal::TerminalState;
    /// # let (tx, rx) = channel();
    /// # let event_proxy = GpuiEventProxy::new(tx);
    /// # let mut terminal = TerminalState::new(80, 24, event_proxy);
    /// // Resize to 120x30
    /// terminal.resize(120, 30);
    /// ```
    pub fn resize(&mut self, cols: usize, rows: usize) {
        self.cols = cols;
        self.rows = rows;

        let mut term = self.term.lock();

        // Create dimensions for the resize
        let dimensions = TermDimensions::new(cols, rows);

        // Resize the terminal
        term.resize(dimensions);
    }

    /// Get the current terminal mode.
    ///
    /// The terminal mode affects how certain key sequences are interpreted,
    /// particularly arrow keys in application cursor mode.
    ///
    /// # Returns
    ///
    /// The current `TermMode` flags.
    ///
    /// # Examples
    ///
    /// ```
    /// # use std::sync::mpsc::channel;
    /// # use rustshell_terminal::event::GpuiEventProxy;
    /// # use rustshell_terminal::terminal::TerminalState;
    /// # let (tx, rx) = channel();
    /// # let event_proxy = GpuiEventProxy::new(tx);
    /// # let terminal = TerminalState::new(80, 24, event_proxy);
    /// use alacritty_terminal::term::TermMode;
    ///
    /// let mode = terminal.mode();
    /// if mode.contains(TermMode::APP_CURSOR) {
    ///     println!("Application cursor mode is enabled");
    /// }
    /// ```
    pub fn mode(&self) -> TermMode {
        let term = self.term.lock();
        *term.mode()
    }

    /// 终端当前是否**要求**光标闪烁（两路来源任一为真）。
    ///
    /// 这是判断闪烁的唯一正确入口：`Term::cursor_style().blinking` 把
    /// DECSET 12 与 DECSCUSR 合成了一个位，curses 的 `cnorm`
    /// （`ESC[?12l`）会把另一路的「闪」一起抹掉（见 [`CursorBlinkTracker`]）。
    ///
    /// 返回的只是**终端侧**的意愿；用户设置、焦点、会话存活仍由视图叠加。
    pub fn cursor_blinking(&self) -> bool {
        self.blink_tracker.blinking()
    }

    /// Execute a function with read access to the terminal.
    ///
    /// This method provides safe read access to the underlying `Term` structure.
    /// The terminal is locked for the duration of the function call.
    ///
    /// # Arguments
    ///
    /// * `f` - A function that takes a reference to the `Term` and returns a value
    ///
    /// # Returns
    ///
    /// The value returned by the function `f`.
    ///
    /// # Examples
    ///
    /// ```
    /// # use std::sync::mpsc::channel;
    /// # use rustshell_terminal::event::GpuiEventProxy;
    /// # use rustshell_terminal::terminal::TerminalState;
    /// # let (tx, rx) = channel();
    /// # let event_proxy = GpuiEventProxy::new(tx);
    /// # let terminal = TerminalState::new(80, 24, event_proxy);
    /// let cursor_pos = terminal.with_term(|term| {
    ///     term.grid().cursor.point
    /// });
    /// ```
    pub fn with_term<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&Term<GpuiEventProxy>) -> R,
    {
        let term = self.term.lock();
        f(&term)
    }

    /// Execute a function with mutable access to the terminal.
    ///
    /// This method provides safe write access to the underlying `Term` structure.
    /// The terminal is locked for the duration of the function call.
    ///
    /// # Arguments
    ///
    /// * `f` - A function that takes a mutable reference to the `Term` and returns a value
    ///
    /// # Returns
    ///
    /// The value returned by the function `f`.
    ///
    /// # Examples
    ///
    /// ```
    /// # use std::sync::mpsc::channel;
    /// # use rustshell_terminal::event::GpuiEventProxy;
    /// # use rustshell_terminal::terminal::TerminalState;
    /// # let (tx, rx) = channel();
    /// # let event_proxy = GpuiEventProxy::new(tx);
    /// # let terminal = TerminalState::new(80, 24, event_proxy);
    /// terminal.with_term_mut(|term| {
    ///     // Perform some mutation on the term
    ///     term.scroll_display(alacritty_terminal::grid::Scroll::Delta(5));
    /// });
    /// ```
    pub fn with_term_mut<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut Term<GpuiEventProxy>) -> R,
    {
        let mut term = self.term.lock();
        f(&mut term)
    }

    /// Get the number of columns in the terminal.
    ///
    /// # Returns
    ///
    /// The current number of columns.
    pub fn cols(&self) -> usize {
        self.cols
    }

    /// Get the number of rows in the terminal.
    ///
    /// # Returns
    ///
    /// The current number of rows.
    pub fn rows(&self) -> usize {
        self.rows
    }

    /// Get a cloned reference to the underlying terminal Arc.
    ///
    /// This allows sharing the terminal state across multiple threads or components.
    ///
    /// # Returns
    ///
    /// A cloned `Arc<Mutex<Term<GpuiEventProxy>>>`.
    pub fn term_arc(&self) -> Arc<Mutex<Term<GpuiEventProxy>>> {
        Arc::clone(&self.term)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;

    #[test]
    fn test_terminal_creation() {
        let (tx, _rx) = channel();
        let event_proxy = GpuiEventProxy::new(tx);
        let terminal = TerminalState::new(80, 24, event_proxy);

        assert_eq!(terminal.cols(), 80);
        assert_eq!(terminal.rows(), 24);
    }

    #[test]
    fn test_process_bytes() {
        let (tx, _rx) = channel();
        let event_proxy = GpuiEventProxy::new(tx);
        let mut terminal = TerminalState::new(80, 24, event_proxy);

        // Process some text
        terminal.process_bytes(b"Hello, world!");

        // Verify the text was written to the grid
        terminal.with_term(|term| {
            let grid = term.grid();
            // The text should be at the cursor position
            // We can't easily test the exact content without more complex grid inspection
            assert!(grid.columns() == 80);
        });
    }

    /// 单批解析的上限必须**真的**小于积压上限（P0-1）。
    ///
    /// 这两个常量一旦相等，单批预算就等于没写：又回到「一次吃下整个积压」，
    /// 而运行期完全看不出来，只是 UI 线程又开始被解析占满。
    #[test]
    fn parse_budget_is_smaller_than_the_backlog_cap() {
        // 收进 `const {}`：这两条是**编译期**不变式（纯常量比较），
        // 让不合规的常量组合直接编译失败，而不是等跑测试才发现。
        // （`clippy::assertions_on_constants` 亦要求这样写。）
        const {
            assert!(
                crate::view::MAX_PARSE_BYTES < 1024 * 1024,
                "单轮解析上限必须远小于 1MB 的积压上限"
            );
            // 也不能小到比 PTY 的读块（4KB）还碎：那样每轮解析不完一块，
            // 让出次数会随输出量线性增长。
            assert!(
                crate::view::MAX_PARSE_BYTES >= 16 * 1024,
                "单轮解析上限不该小于几个 PTY 读块"
            );
        }
    }

    /// P0-1 的正确性前提：VTE 解析是**状态机**，在任意字节边界切开与一次喂完等价。
    ///
    /// 分帧续解析（`view.rs`：一轮只解析 `MAX_PARSE_BYTES`，余下留在 `pending`）
    /// 之所以安全全靠这一条 —— 一旦解析层哪天改成「按次缓冲」的实现，
    /// 这里会立刻红，而不是等用户看到乱码。切点覆盖中文的多字节 UTF-8 序列内部
    /// 与 CSI 转义序列内部，这两处正是「切开就出乱码」的高发区。
    #[test]
    fn stream_survives_being_split_at_any_byte_boundary() {
        use alacritty_terminal::index::{Column, Line};
        let stream = "line1\r\n中文截断测试 abc\r\n\x1b[31mred\x1b[0m\x08\x08XY\r\n尾行"
            .as_bytes()
            .to_vec();

        let grid_text = |chunks: &[&[u8]]| -> Vec<String> {
            let (tx, _rx) = channel();
            let mut terminal = TerminalState::new(24, 8, GpuiEventProxy::new(tx));
            for c in chunks {
                terminal.process_bytes(c);
            }
            terminal.with_term(|term| {
                let grid = term.grid();
                (0..grid.screen_lines())
                    .map(|l| {
                        let row: String = (0..grid.columns())
                            .map(|c| {
                                let ch = grid[Line(l as i32)][Column(c)].c;
                                if ch == '\0' { ' ' } else { ch }
                            })
                            .collect();
                        row.trim_end().to_string()
                    })
                    .collect()
            })
        };

        let whole = grid_text(&[&stream]);
        // 夹具自身要真的画出了东西（宽字符的第二个格子是占位，所以按单字判）
        assert!(
            whole.iter().any(|l| l.contains('中')) && whole.iter().any(|l| l.contains('尾')),
            "夹具自身没画出来：{whole:?}"
        );
        for at in 0..=stream.len() {
            assert_eq!(
                grid_text(&[&stream[..at], &stream[at..]]),
                whole,
                "在字节 {at} 处切开后网格与整块喂入不一致"
            );
        }
    }

    #[test]
    fn test_resize() {
        let (tx, _rx) = channel();
        let event_proxy = GpuiEventProxy::new(tx);
        let mut terminal = TerminalState::new(80, 24, event_proxy);

        terminal.resize(120, 30);

        assert_eq!(terminal.cols(), 120);
        assert_eq!(terminal.rows(), 30);

        terminal.with_term(|term| {
            let grid = term.grid();
            assert_eq!(grid.columns(), 120);
            assert_eq!(grid.screen_lines(), 30);
        });
    }

    #[test]
    fn test_mode() {
        let (tx, _rx) = channel();
        let event_proxy = GpuiEventProxy::new(tx);
        let terminal = TerminalState::new(80, 24, event_proxy);

        let mode = terminal.mode();
        // Mode should be a valid TermMode value (just verify we can get it)
        let _bits = mode.bits();
    }

    #[test]
    fn test_with_term() {
        let (tx, _rx) = channel();
        let event_proxy = GpuiEventProxy::new(tx);
        let terminal = TerminalState::new(80, 24, event_proxy);

        let cols = terminal.with_term(|term| term.grid().columns());
        assert_eq!(cols, 80);
    }

    #[test]
    fn test_with_term_mut() {
        let (tx, _rx) = channel();
        let event_proxy = GpuiEventProxy::new(tx);
        let terminal = TerminalState::new(80, 24, event_proxy);

        terminal.with_term_mut(|term| {
            // Just verify we can get mutable access
            let _grid = term.grid_mut();
        });
    }

    #[test]
    fn test_term_arc() {
        let (tx, _rx) = channel();
        let event_proxy = GpuiEventProxy::new(tx);
        let terminal = TerminalState::new(80, 24, event_proxy);

        let arc1 = terminal.term_arc();
        let arc2 = terminal.term_arc();

        // Both Arcs should point to the same terminal
        assert!(Arc::ptr_eq(&arc1, &arc2));
    }

    #[test]
    fn default_cursor_style_blinks() {
        // 本地 shell 不发 DECSCUSR，默认样式必须自带闪烁，
        // 否则「光标闪烁」这个需求在本地会话上永远不会生效。
        let (tx, _rx) = channel();
        let terminal = TerminalState::new(80, 24, GpuiEventProxy::new(tx));

        let style = terminal.with_term(|t| t.cursor_style());
        assert!(style.blinking, "默认光标样式必须要求闪烁");
        assert_eq!(style.shape, CursorShape::Block);
        assert!(terminal.cursor_blinking());
    }

    #[test]
    fn remote_decscusr_can_disable_blinking() {
        // 远端发 DECSCUSR 时以远端为准，不能被默认值锁死。
        let (tx, _rx) = channel();
        let mut terminal = TerminalState::new(80, 24, GpuiEventProxy::new(tx));

        // ESC [ 2 SP q = 稳定（不闪）块光标
        terminal.process_bytes(b"\x1b[2 q");

        let style = terminal.with_term(|t| t.cursor_style());
        assert!(!style.blinking, "DECSCUSR 应能关掉闪烁");
        assert!(
            !terminal.cursor_blinking(),
            "显式的稳定 DECSCUSR 必须真的停闪"
        );
    }

    /// `CSI 0 q` 是「回到配置默认」，不是「关掉闪烁」。
    #[test]
    fn decscusr_zero_restores_default_blinking() {
        let (tx, _rx) = channel();
        let mut terminal = TerminalState::new(80, 24, GpuiEventProxy::new(tx));

        terminal.process_bytes(b"\x1b[2 q");
        assert!(!terminal.cursor_blinking());

        terminal.process_bytes(b"\x1b[0 q");
        assert!(
            terminal.cursor_blinking(),
            "CSI 0 q 应恢复配置默认（闪烁），而不是保持上一次的稳定样式"
        );
    }

    /// ★ 回归（2026-10-20 用户报）：SSH 里跑 `top`，Ctrl+Z 挂起后光标不再闪烁。
    ///
    /// 根因：curses 程序恢复终端时发 terminfo 的 `cnorm`
    /// （`ESC[?12l ESC[?25h`）。`?12l` 会把 alacritty 的
    /// `CursorStyle.blinking` 清掉，而视图此前只看这一个位 ⇒ 闪烁被永久
    /// 关掉（光标还在，只是不闪）。
    ///
    /// 修法（foot#218）：DECSET 12 与 DECSCUSR 的闪烁意图分开记，
    /// 任一要求闪烁就闪。`cnorm` 只关掉前者，配置默认 / DECSCUSR 那一路
    /// 仍为真 ⇒ 光标继续闪。
    #[test]
    fn cnorm_does_not_stop_blinking() {
        let (tx, _rx) = channel();
        let mut terminal = TerminalState::new(80, 24, GpuiEventProxy::new(tx));

        // top 挂起 / 退出时恢复光标：cnorm = ?12l + ?25h
        terminal.process_bytes(b"\x1b[?12l\x1b[?25h");

        // alacritty 自己的单一位确实被关掉了（这就是旧实现的依据）
        assert!(
            !terminal.with_term(|t| t.cursor_style().blinking),
            "夹具前提：?12l 会清掉 alacritty 的 blinking 位"
        );
        assert!(
            terminal.cursor_blinking(),
            "cnorm 的 ?12l 不得把 DECSCUSR/默认的闪烁意图一并关掉"
        );
    }

    /// `?12h` 能重新打开闪烁；之后的 `?12l` 只退回到 DECSCUSR 的意图。
    #[test]
    fn decset_12_combines_with_decscusr() {
        let (tx, _rx) = channel();
        let mut terminal = TerminalState::new(80, 24, GpuiEventProxy::new(tx));

        terminal.process_bytes(b"\x1b[2 q"); // 稳定块
        assert!(!terminal.cursor_blinking());

        terminal.process_bytes(b"\x1b[?12h"); // 显式要求闪
        assert!(terminal.cursor_blinking(), "?12h 应能覆盖稳定 DECSCUSR");

        terminal.process_bytes(b"\x1b[?12l");
        assert!(
            !terminal.cursor_blinking(),
            "?12l 只关掉 DECSET 那一路，DECSCUSR 仍是稳定 ⇒ 不闪"
        );
    }

    /// 跟踪器必须和主解析器一样跨字节边界有状态（PTY 按 4KB 投递）。
    #[test]
    fn blink_tracking_survives_split_sequences() {
        let (tx, _rx) = channel();
        let mut terminal = TerminalState::new(80, 24, GpuiEventProxy::new(tx));

        terminal.process_bytes(b"\x1b[?1");
        terminal.process_bytes(b"2l");
        assert!(terminal.cursor_blinking(), "跨块 ?12l 不应误杀闪烁");

        terminal.process_bytes(b"\x1b[1");
        terminal.process_bytes(b" q");
        assert!(terminal.cursor_blinking(), "跨块 CSI 1 q 应被识别为闪烁");

        terminal.process_bytes(b"\x1b[");
        terminal.process_bytes(b"2 q");
        assert!(!terminal.cursor_blinking(), "跨块 CSI 2 q 应被识别为稳定");
    }

    #[test]
    fn scrollback_setting_reaches_alacritty_config() {
        // 「回滚行数」设置此前只落在字段上、从不进 alacritty 配置。
        // 这里用可观测行为验证：打印远超一屏的行数后，网格总行数应等于
        // 屏高 + 配置的回滚行数。
        let (tx, _rx) = channel();
        let mut terminal = TerminalState::with_scrollback(80, 24, 50, GpuiEventProxy::new(tx));

        for i in 0..500 {
            terminal.process_bytes(format!("line {i}\r\n").as_bytes());
        }

        let total = terminal.with_term(|t| t.grid().total_lines());
        assert_eq!(total, 24 + 50, "回滚行数应被 alacritty 采纳");
    }

    #[test]
    fn zero_scrollback_keeps_no_history() {
        let (tx, _rx) = channel();
        let mut terminal = TerminalState::with_scrollback(80, 24, 0, GpuiEventProxy::new(tx));

        for i in 0..500 {
            terminal.process_bytes(format!("line {i}\r\n").as_bytes());
        }

        let total = terminal.with_term(|t| t.grid().total_lines());
        assert_eq!(total, 24, "回滚为 0 时不应留下历史");
    }

    #[test]
    fn scrollback_is_capped() {
        assert_eq!(clamped_scrolling_history(usize::MAX), MAX_SCROLLING_HISTORY);
        assert_eq!(
            clamped_scrolling_history(MAX_SCROLLING_HISTORY + 1),
            MAX_SCROLLING_HISTORY
        );
        assert_eq!(clamped_scrolling_history(1234), 1234);
    }

    // ─────────────────────── Shell integration（C-0）回归 ───────────────────────
    //
    // 这一组测试要证明三件事，缺一不可：
    //  1. 认领的 OSC（133/633/7）**真的**被投递出来（否则整个 C-0 是空的）；
    //  2. 认领的 OSC **不再**到达网格（否则会往屏上画垃圾）；
    //  3. 没认领的 OSC 与普通字节**逐字节等价**地抵达 vte（否则就废掉了
    //     OSC 4/52/0/2 这些既有能力 —— 这是本方案最大的风险面）。

    /// 收集网格全部可见文本，用于"包装前后网格是否一致"的比对。
    fn grid_text(terminal: &TerminalState) -> Vec<String> {
        use alacritty_terminal::index::{Column, Line};
        terminal.with_term(|term| {
            let grid = term.grid();
            (0..grid.screen_lines())
                .map(|l| {
                    let row: String = (0..grid.columns())
                        .map(|c| {
                            let ch = grid[Line(l as i32)][Column(c)].c;
                            if ch == '\0' { ' ' } else { ch }
                        })
                        .collect();
                    row.trim_end().to_string()
                })
                .collect()
        })
    }

    /// ① 认领的 OSC 真的投递出来；② 不进网格。
    #[test]
    fn shell_integration_marks_are_emitted_and_not_drawn() {
        let (tx, _rx) = channel();
        let mut terminal = TerminalState::new(80, 24, GpuiEventProxy::new(tx));
        let (shell_tx, shell_rx) = channel();

        terminal.process_bytes_with_events(
            b"\x1b]133;A\x07$ \x1b]133;B\x07ls -la\x1b]133;C\x07\x1b]133;D;0\x07",
            &shell_tx,
        );

        let got: Vec<TerminalEvent> = shell_rx.try_iter().collect();
        assert_eq!(got.len(), 4, "四条 133 标记应全部投递，实得 {got:?}");
        assert!(matches!(
            got[0],
            TerminalEvent::PromptMark(crate::shell_integration::PromptMark::PromptStart)
        ));
        assert!(matches!(
            got[1],
            TerminalEvent::PromptMark(crate::shell_integration::PromptMark::CommandStart)
        ));
        assert!(matches!(
            got[2],
            TerminalEvent::PromptMark(crate::shell_integration::PromptMark::CommandExecuted)
        ));
        assert!(matches!(
            got[3],
            TerminalEvent::PromptMark(crate::shell_integration::PromptMark::CommandFinished {
                exit_code: Some(0)
            })
        ));

        // 网格里只应有 `$ ` 与 `ls -la`，不该有 OSC 的任何残渣。
        let text = grid_text(&terminal).join("\n");
        assert!(
            text.contains("$ ls -la"),
            "普通字节应正常落到网格，实得 {text:?}"
        );
        assert!(
            !text.contains("133"),
            "OSC 133 不该被画到网格上，实得 {text:?}"
        );
    }

    /// OSC 7（cwd）与 OSC 633;E（命令行）。
    #[test]
    fn shell_integration_cwd_and_command_line() {
        let (tx, _rx) = channel();
        let mut terminal = TerminalState::new(80, 24, GpuiEventProxy::new(tx));
        let (shell_tx, shell_rx) = channel();

        terminal.process_bytes_with_events(
            b"\x1b]7;file://host/var/log\x07\x1b]633;E;git status\x1b\\",
            &shell_tx,
        );

        let got: Vec<TerminalEvent> = shell_rx.try_iter().collect();
        assert_eq!(got.len(), 2, "实得 {got:?}");
        match &got[0] {
            TerminalEvent::Cwd(p) => assert_eq!(p, "/var/log"),
            other => panic!("期望 Cwd，得到 {other:?}"),
        }
        match &got[1] {
            TerminalEvent::CommandLine(l) => assert_eq!(l, "git status"),
            other => panic!("期望 CommandLine，得到 {other:?}"),
        }
    }

    /// ★ 最高价值的一条：**装不装这一层，网格结果必须一模一样**。
    ///
    /// 这是整个 C-0 方案的安全底线。字节流里混了：普通文字、中文（多字节 UTF-8）、
    /// CSI 颜色、认领的 OSC 133、**不认领的 OSC 8 超链接 / OSC 0 标题**、
    /// 以及一个 OSC 与 CSI 紧挨着的情况。
    ///
    /// 基准是"手工把认领的 OSC 摘掉的同一份字节"——认领的 OSC 本来就不在
    /// vte 的产出里（它只打日志），所以摘掉之后两者应当**逐行相同**。
    #[test]
    fn claimed_osc_removal_leaves_the_grid_byte_identical() {
        // 注意必须按**字节**摘，不能用 `push(bytes[i] as char)` ——
        // 那会把中文 UTF-8 的每个字节单独当 char 塞回去，把夹具本身写坏。
        fn strip_claimed(bytes: &[u8]) -> Vec<u8> {
            let mut out = Vec::new();
            let mut i = 0;
            while i < bytes.len() {
                if bytes[i..].starts_with(b"\x1b]133;") {
                    // 跳到 BEL（本夹具里全是 BEL 收尾）
                    let mut j = i + 2;
                    while j < bytes.len() && bytes[j] != 0x07 {
                        j += 1;
                    }
                    i = (j + 1).min(bytes.len());
                    continue;
                }
                out.push(bytes[i]);
                i += 1;
            }
            out
        }

        // `\x1b]133;A\x07` 是**认领**的；`\x1b]0;t\x07` 与
        // `\x1b]8;;https://x\x07` 是**不认领**的（必须留给 vte）。
        let full: &str = "\x1b]133;A\x07$ \x1b[31m\u{7ea2}\x1b[0m \x1b]0;title\x07\x1b]8;;https://x\x07link\x1b]133;C\x07\r\nsecond line\x1b]133;D;0\x07";

        let (tx, _rx) = channel();
        let mut with_integration = TerminalState::new(80, 24, GpuiEventProxy::new(tx));
        let (shell_tx, shell_rx) = channel();
        with_integration.process_bytes_with_events(full.as_bytes(), &shell_tx);

        // 认领的 3 条确实被摘走了（不是被吞掉又重复投递）。
        // 本夹具含 A / C / D 三条。
        assert_eq!(shell_rx.try_iter().count(), 3, "应有 3 条 133 事件");

        // 基准：把认领的 OSC 摘掉后，喂给一个"干净"的终端
        let (tx2, _rx2) = channel();
        let mut baseline = TerminalState::new(80, 24, GpuiEventProxy::new(tx2));
        baseline.process_bytes(&strip_claimed(full.as_bytes()));

        assert_eq!(
            grid_text(&with_integration),
            grid_text(&baseline),
            "装/不装 shell integration 感知层，网格必须逐行相同"
        );

        // 网格里必须真的画出了东西（防止两边都是空屏 ⇒ 假绿）
        let text = grid_text(&with_integration).join("\n");
        assert!(text.contains('\u{7ea2}'), "夹具自身没画出东西：{text:?}");
        assert!(text.contains("second line"), "夹具自身没画出东西：{text:?}");
    }

    /// `OSC 9`（无人认领）必须原样穿过本层 —— 它该走 vte 的 `unhandled` 路径，
    /// **不该**被误判成 shell integration 而被静默摘掉。
    #[test]
    fn unclaimed_osc_still_reaches_vte() {
        let (tx, _rx) = channel();
        let mut terminal = TerminalState::new(80, 24, GpuiEventProxy::new(tx));
        let (shell_tx, shell_rx) = channel();

        // OSC 9 通知 + 之后的正文
        terminal.process_bytes_with_events("\x1b]9;hello\x07正文".as_bytes(), &shell_tx);

        assert_eq!(
            shell_rx.try_iter().count(),
            0,
            "OSC 9 不是 shell integration，不该产生事件"
        );
        let text = grid_text(&terminal).join("\n");
        // 宽字符（中文）在网格里占两格，第二个格子是**占位空格**
        // ⇒ 不能断言连续字符串，得把空格折掉再比（与 `render.rs` 的口径一致）。
        let squeezed: String = text.chars().filter(|c| *c != ' ').collect();
        assert!(
            squeezed.contains("正文"),
            "OSC 9 之后的正文应正常绘制：{text:?}"
        );
        // OSC 9 的内容本身不该被画出来（vte 会把它当 unhandled 丢弃）
        assert!(
            !text.contains("hello"),
            "OSC 9 的内容不该被画到屏上：{text:?}"
        );
    }

    /// 半条 OSC（没收尾）不能被认领，也不能丢字符。
    ///
    /// PTY 按 4KB 投递，`ESC ]133;D;0` 的后半截出现在下一块里是常态。
    /// 若第一块就把这半条当"认领失败"丢掉，退出码就永远收不到。
    #[test]
    fn split_osc_across_chunks_is_still_claimed() {
        let (tx, _rx) = channel();
        let mut terminal = TerminalState::new(80, 24, GpuiEventProxy::new(tx));
        let (shell_tx, shell_rx) = channel();

        // 在 OSC 中间切开：第一块没收尾，第二块补上
        terminal.process_bytes_with_events(b"\x1b]133;D;", &shell_tx);
        assert_eq!(
            shell_rx.try_iter().count(),
            0,
            "半条 OSC 不该产生事件（也没有收尾符）"
        );

        terminal.process_bytes_with_events(b"7\x07", &shell_tx);
        let got: Vec<TerminalEvent> = shell_rx.try_iter().collect();
        assert_eq!(got.len(), 1, "跨块补齐后应投出一条，实得 {got:?}");
        assert!(matches!(
            got[0],
            TerminalEvent::PromptMark(crate::shell_integration::PromptMark::CommandFinished {
                exit_code: Some(7)
            })
        ));
        // 补齐之后滞留缓冲必须清空 —— 否则下一条 OSC 会被接上这条的残渣。
        assert!(terminal.pending_osc.is_empty(), "认领完成后滞留缓冲应清空");
    }

    /// 超长的未收尾 OSC 不能把内存吃光，也不能把解析器永久钉在 OSC 里。
    ///
    /// 远端可以一直发 `ESC ]` 然后永不给收尾符。两条要求：
    ///  1. `pending_osc` 有上限（内存不涨）；
    ///  2. 超限时要**让 vte 退回地面态**（补一个 CAN），
    ///     否则它以为自己在 OSC 里、后续所有字节被静默吞掉。
    #[test]
    fn runaway_unterminated_osc_is_dropped_and_resets_parser() {
        let (tx, _rx) = channel();
        let mut terminal = TerminalState::new(80, 24, GpuiEventProxy::new(tx));
        let (shell_tx, shell_rx) = channel();

        // 先发一条超过上限的未收尾 OSC（引导符 + 上限+1 的填充）
        let mut flood = Vec::from(&b"\x1b]999;"[..]);
        flood.extend(std::iter::repeat_n(b'A', MAX_PENDING_OSC + 1));
        terminal.process_bytes_with_events(&flood, &shell_tx);

        assert!(
            terminal.pending_osc.is_empty(),
            "超限的一批不该被留下，实得 {} 字节",
            terminal.pending_osc.len()
        );
        assert_eq!(shell_rx.try_iter().count(), 0, "它不是 shell integration");

        // 解析器必须已回到地面态：随后的普通文字要能正常画出来
        terminal.process_bytes_with_events(b"after\x07", &shell_tx);
        let text = grid_text(&terminal).join("\n");
        assert!(
            text.contains("after"),
            "超限 OSC 之后解析器应已复位，正文应正常绘制：{text:?}"
        );
    }

    /// 分段函数自身的边界：无 OSC / 纯 OSC / 首尾都是 OSC / 未收尾。
    #[test]
    fn osc_segmenter_handles_edges() {
        use SegmentKind::{Osc, OscUnterminated, Plain};

        // 没有 OSC ⇒ 整块一段
        assert_eq!(split_osc_segments(b"hello"), vec![(0, 5, Plain)]);
        // 空输入 ⇒ 无段
        assert_eq!(split_osc_segments(b""), Vec::new());
        // 纯 OSC（BEL 收尾）：`\x1b]133;A\x07` 共 8 字节
        let osc = b"\x1b]133;A\x07";
        assert_eq!(osc.len(), 8, "夹具长度假设");
        assert_eq!(split_osc_segments(osc), vec![(0, 8, Osc)]);
        // 首尾都是 OSC，中间夹普通字节
        let mixed = b"\x1b]1;x\x07mid\x1b]2;y\x07";
        let segs = split_osc_segments(mixed);
        assert_eq!(segs.len(), 3, "实得 {segs:?}");
        assert_eq!(segs[0].2, Osc);
        assert_eq!(segs[1].2, Plain);
        assert_eq!(segs[2].2, Osc);
        assert_eq!(&mixed[segs[1].0..segs[1].1], b"mid");
        // ST 收尾（`ESC \`）也要认
        let st = b"\x1b]7;file:///a\x1b\\";
        assert_eq!(split_osc_segments(st), vec![(0, st.len(), Osc)]);
        // 未收尾 ⇒ 单独标出来（调用方要攒起来，不能就地处置）
        let partial = b"\x1b]133;D;0";
        assert_eq!(
            split_osc_segments(partial),
            vec![(0, partial.len(), OscUnterminated)]
        );
        // 半条 OSC 前面还有普通字节时，两段都要在
        let lead = b"abc\x1b]133;D";
        assert_eq!(
            split_osc_segments(lead),
            vec![(0, 3, Plain), (3, lead.len(), OscUnterminated)]
        );
    }
}
