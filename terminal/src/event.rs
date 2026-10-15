//! Event handling for the terminal emulator.
//!
//! This module bridges alacritty's event system with GPUI by providing
//! [`GpuiEventProxy`], which implements alacritty's [`EventListener`] trait
//! and forwards relevant events through a channel.
//!
//! # Event Flow
//!
//! ```text
//! alacritty Term → GpuiEventProxy → mpsc channel → TerminalView
//!                        │
//!                        └─ Translates Event enum to TerminalEvent
//! ```
//!
//! # Supported Events
//!
//! | Alacritty Event | TerminalEvent | Description |
//! |-----------------|---------------|-------------|
//! | `Event::Wakeup` | `Wakeup` | Terminal has new content |
//! | `Event::Bell` | `Bell` | BEL character received |
//! | `Event::Title(_)` | `Title(String)` | Title escape sequence (OSC 0/2) |
//! | `Event::ClipboardStore(_, _)` | `ClipboardStore(String)` | Copy request (OSC 52) |
//! | `Event::ClipboardLoad(_, _)` | `ClipboardLoad(formatter)` | Paste request（应答由宿主写回 PTY） |
//! | `Event::CursorBlinkingChange` | `CursorBlinkingChange` | 光标闪烁状态变化（DECSCUSR / `ESC[?12h|l`） |
//! | `Event::Exit` | `Exit` | Terminal exited |
//! | `Event::ChildExit(_)` | `Exit` | Child process exited |
//! | `Event::ResetTitle` | `Title("")` | Reset to empty title |
//! | `Event::PtyWrite(_)` | —（直接写回 PTY） | 终端查询应答（DSR/DA/DECRQM/尺寸） |
//! | `Event::ColorRequest(_, _)` | —（直接写回 PTY） | OSC 4/10/11 颜色查询应答 |
//! | `Event::TextAreaSizeRequest(_)` | —（直接写回 PTY） | `CSI 14 t` 像素尺寸查询应答 |
//!
//! ⚠️ **`PtyWrite` 必须由宿主写回 PTY，不能丢弃**：alacritty 只负责“算出应答内容”，
//! 它本身没有 PTY 写端。丢弃 `PtyWrite` 会让 `ESC[6n`（DSR，光标位置查询）等
//! 应答永远送不到子进程 —— 例如 PowerShell/ConPTY 启动时会发 `ESC[6n` 并
//! **阻塞等待** `ESC[{行};{列}R`，于是终端永久空白（2026-09-10 运行反馈：
//! 「powershell 终端无效」）。同理 `ColorRequest` / `TextAreaSizeRequest`
//! 也需要宿主应答，否则查询方可能挂起。
//!
//! # Example
//!
//! ```
//! use std::sync::mpsc::channel;
//! use rustshell_terminal::event::{GpuiEventProxy, TerminalEvent};
//!
//! let (tx, rx) = channel();
//! let proxy = GpuiEventProxy::new(tx);
//!
//! // The proxy is passed to alacritty's Term and will forward events
//! // Events can be received on the other end of the channel
//! ```
//!
//! [`EventListener`]: alacritty_terminal::event::EventListener

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::vte::ansi::Rgb;
use parking_lot::Mutex;
use std::io::Write as _;
use std::sync::Arc;
use std::sync::mpsc::Sender;

/// PTY 回写句柄：与终端 stdin writer 是同一份 `Arc`。
pub type PtyWriter = Arc<Mutex<Box<dyn std::io::Write + Send>>>;

/// 颜色查询应答：索引语义同 `NamedColor`
/// （0–15 = ANSI / Bright，256 = 前景，257 = 背景，258 = 光标）。
pub type ColorLookup = Arc<dyn Fn(usize) -> Option<Rgb> + Send + Sync>;

/// OSC 52 读取应答格式化器，由 alacritty 提供。
/// 宿主只需给出剪贴板文本，序列拼装（base64、BEL/ST 收尾等）交给它。
pub type ClipboardLoadFormatter = Arc<dyn Fn(&str) -> String + Sync + Send + 'static>;

/// Events emitted by the terminal that the GPUI application cares about.
///
/// This enum represents a subset of alacritty's events that are relevant
/// for the GPUI terminal emulator implementation.
#[derive(Clone)]
pub enum TerminalEvent {
    /// The terminal has new content to display and needs a redraw.
    Wakeup,

    /// The terminal bell was triggered (visual or audible alert).
    Bell,

    /// The terminal title has changed.
    Title(String),

    /// The terminal wants to store data to the clipboard.
    ClipboardStore(String),

    /// The terminal wants to load data from the clipboard.
    ///
    /// 携带 alacritty 的应答格式化器：宿主给出文本，格式由 alacritty 决定。
    ClipboardLoad(ClipboardLoadFormatter),

    /// 光标闪烁状态发生变化（DECSCUSR、`ESC[?12h` / `ESC[?12l`）。
    CursorBlinkingChange,

    /// shell integration：命令生命周期标记（`OSC 133` / `633`）。
    ///
    /// 仅在远端（或用户自己的 shell 配置）**主动发出**这些序列时才有事件；
    /// 不发就永远收不到，不会有任何副作用。用途见 [`crate::shell_integration`]。
    PromptMark(crate::shell_integration::PromptMark),

    /// shell integration：当前工作目录变更（`OSC 7`）。路径是**远端**路径。
    Cwd(String),

    /// shell integration：当前命令行的准确文本（`OSC 633;E`）。
    ///
    /// 比"从网格里推断当前行"可靠得多 —— 它不依赖 prompt 边界猜测。
    CommandLine(String),

    /// The terminal process has exited.
    Exit,
}

// `ClipboardLoad` 携带 `Arc<dyn Fn ..>`（不实现 Debug），故手写 Debug，
// 保留其余变体的结构化输出，避免为一个闭包丢掉整个枚举的 Debug。
impl std::fmt::Debug for TerminalEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Wakeup => f.write_str("Wakeup"),
            Self::Bell => f.write_str("Bell"),
            Self::Title(t) => f.debug_tuple("Title").field(t).finish(),
            Self::ClipboardStore(t) => f
                .debug_tuple("ClipboardStore")
                .field(&format_args!("{} bytes", t.len()))
                .finish(),
            Self::ClipboardLoad(_) => f.write_str("ClipboardLoad(<formatter>)"),
            Self::CursorBlinkingChange => f.write_str("CursorBlinkingChange"),
            Self::PromptMark(m) => f.debug_tuple("PromptMark").field(m).finish(),
            Self::Cwd(p) => f.debug_tuple("Cwd").field(p).finish(),
            Self::CommandLine(l) => f.debug_tuple("CommandLine").field(l).finish(),
            Self::Exit => f.write_str("Exit"),
        }
    }
}

/// An event proxy that implements alacritty's EventListener trait.
///
/// This struct forwards relevant terminal events to a channel that can be
/// consumed by the GPUI application on the main thread.
pub struct GpuiEventProxy {
    /// Channel sender for forwarding events to the GPUI application.
    tx: Sender<TerminalEvent>,

    /// PTY 回写句柄。`None` 时丢弃应答（仅用于不关心终端应答的场景/测试）。
    pty_writer: Option<PtyWriter>,

    /// 颜色查询应答表（OSC 4/10/11）。
    color_lookup: Option<ColorLookup>,

    /// 文本区像素尺寸（`CSI 14 t` 应答用），渲染时刷新。
    window_size: Option<Arc<Mutex<WindowSize>>>,
}

impl GpuiEventProxy {
    /// Creates a new event proxy with the given channel sender.
    ///
    /// # Arguments
    ///
    /// * `tx` - The channel sender to forward events through
    ///
    /// # Returns
    ///
    /// A new GpuiEventProxy instance
    ///
    /// # Examples
    ///
    /// ```
    /// use std::sync::mpsc::channel;
    /// use rustshell_terminal::event::GpuiEventProxy;
    ///
    /// let (tx, rx) = channel();
    /// let proxy = GpuiEventProxy::new(tx);
    /// ```
    pub fn new(tx: Sender<TerminalEvent>) -> Self {
        Self::with_pty(tx, None, None, None)
    }

    /// 带 PTY 回写能力的构造器（生产路径）。
    ///
    /// `pty_writer` 必须是终端 stdin 的同一份 `Arc`（见 `TerminalView::new`），
    /// 这样 DSR/DA/颜色/尺寸查询的应答才能送回子进程。
    pub fn with_pty(
        tx: Sender<TerminalEvent>,
        pty_writer: Option<PtyWriter>,
        color_lookup: Option<ColorLookup>,
        window_size: Option<Arc<Mutex<WindowSize>>>,
    ) -> Self {
        Self {
            tx,
            pty_writer,
            color_lookup,
            window_size,
        }
    }

    /// 把应答字节写回 PTY（无写端时静默丢弃）。
    fn write_pty(&self, bytes: &[u8]) {
        let Some(writer) = self.pty_writer.as_ref() else {
            return;
        };
        let mut w = writer.lock();
        let _ = w.write_all(bytes);
        let _ = w.flush();
    }

    /// Sends a terminal event through the channel.
    ///
    /// If the channel is disconnected, this method will silently drop the event.
    /// This can happen if the GPUI application has been shut down.
    fn send(&self, event: TerminalEvent) {
        // Ignore send errors - they just mean the receiver has been dropped
        let _ = self.tx.send(event);
    }
}

impl EventListener for GpuiEventProxy {
    /// Handles events from the alacritty terminal.
    ///
    /// This method is called by alacritty when terminal events occur.
    /// It translates alacritty's Event enum to our TerminalEvent enum
    /// and forwards relevant events through the channel.
    fn send_event(&self, event: Event) {
        match event {
            Event::Wakeup => {
                self.send(TerminalEvent::Wakeup);
            }
            Event::Bell => {
                self.send(TerminalEvent::Bell);
            }
            Event::Title(title) => {
                self.send(TerminalEvent::Title(title));
            }
            Event::ClipboardStore(_clipboard_type, data) => {
                // For simplicity, we ignore the clipboard type and just store the data
                self.send(TerminalEvent::ClipboardStore(data));
            }
            Event::ClipboardLoad(_clipboard_type, format) => {
                // ★ 保留 alacritty 的应答格式化器：宿主读到剪贴板文本后由它拼出
                //   OSC 52 应答，再写回 PTY。旧实现连 format 一起丢弃，
                //   终端侧 `OSC 52 ; ?` 读取请求永远得不到应答。
                self.send(TerminalEvent::ClipboardLoad(format));
            }
            Event::Exit => {
                self.send(TerminalEvent::Exit);
            }
            // Ignore events we don't care about
            Event::MouseCursorDirty => {}
            Event::PtyWrite(data) => {
                // ★ 必须写回 PTY：DSR(`ESC[6n`) 光标位置查询、DA 设备属性、
                // DECRQM 模式查询、`CSI 18 t` 字符尺寸查询等都靠这条通路应答。
                // 丢弃会导致子进程（如 PowerShell/ConPTY 启动流程）永久阻塞等待。
                self.write_pty(data.as_bytes());
            }
            Event::ColorRequest(index, format) => {
                // OSC 4/10/11 颜色查询：用调色板算出 RGB 后按 alacritty 给的格式应答。
                if let Some(rgb) = self.color_lookup.as_ref().and_then(|f| f(index)) {
                    self.write_pty(format(rgb).as_bytes());
                }
            }
            Event::TextAreaSizeRequest(format) => {
                // `CSI 14 t`：查询文本区像素尺寸。
                if let Some(ws) = self.window_size.as_ref() {
                    let ws = *ws.lock();
                    self.write_pty(format(ws).as_bytes());
                }
            }
            Event::CursorBlinkingChange => {
                // ★ 不再丢弃：`CSI n q`(DECSCUSR) 与 `ESC[?12h|l` 都会触发它，
                //   视图据此决定自身闪烁定时器是否驱动。
                self.send(TerminalEvent::CursorBlinkingChange);
            }
            Event::ResetTitle => {
                // Reset title to default - we can treat this as an empty title
                self.send(TerminalEvent::Title(String::new()));
            }
            Event::ChildExit(_exit_code) => {
                // Child process exited - treat this as a terminal exit
                self.send(TerminalEvent::Exit);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;

    #[test]
    fn test_event_proxy_creation() {
        let (tx, _rx) = channel();
        let _proxy = GpuiEventProxy::new(tx);
    }

    #[test]
    fn test_wakeup_event() {
        let (tx, rx) = channel();
        let proxy = GpuiEventProxy::new(tx);

        proxy.send_event(Event::Wakeup);

        let event = rx.recv().unwrap();
        assert!(matches!(event, TerminalEvent::Wakeup));
    }

    #[test]
    fn test_bell_event() {
        let (tx, rx) = channel();
        let proxy = GpuiEventProxy::new(tx);

        proxy.send_event(Event::Bell);

        let event = rx.recv().unwrap();
        assert!(matches!(event, TerminalEvent::Bell));
    }

    #[test]
    fn test_title_event() {
        let (tx, rx) = channel();
        let proxy = GpuiEventProxy::new(tx);

        proxy.send_event(Event::Title("Test Title".to_string()));

        let event = rx.recv().unwrap();
        match event {
            TerminalEvent::Title(title) => assert_eq!(title, "Test Title"),
            _ => panic!("Expected Title event"),
        }
    }

    #[test]
    fn test_clipboard_store_event() {
        use alacritty_terminal::term::ClipboardType;

        let (tx, rx) = channel();
        let proxy = GpuiEventProxy::new(tx);

        proxy.send_event(Event::ClipboardStore(
            ClipboardType::Clipboard,
            "clipboard data".to_string(),
        ));

        let event = rx.recv().unwrap();
        match event {
            TerminalEvent::ClipboardStore(data) => assert_eq!(data, "clipboard data"),
            _ => panic!("Expected ClipboardStore event"),
        }
    }

    #[test]
    fn test_clipboard_load_event() {
        use alacritty_terminal::term::ClipboardType;
        use std::sync::Arc;

        let (tx, rx) = channel();
        let proxy = GpuiEventProxy::new(tx);

        // ClipboardLoad requires a callback function
        let callback = Arc::new(|s: &str| s.to_string());
        proxy.send_event(Event::ClipboardLoad(ClipboardType::Clipboard, callback));

        let event = rx.recv().unwrap();
        assert!(matches!(event, TerminalEvent::ClipboardLoad(_)));
    }

    #[test]
    fn test_exit_event() {
        let (tx, rx) = channel();
        let proxy = GpuiEventProxy::new(tx);

        proxy.send_event(Event::Exit);

        let event = rx.recv().unwrap();
        assert!(matches!(event, TerminalEvent::Exit));
    }

    #[test]
    fn test_reset_title_event() {
        let (tx, rx) = channel();
        let proxy = GpuiEventProxy::new(tx);

        proxy.send_event(Event::ResetTitle);

        let event = rx.recv().unwrap();
        match event {
            TerminalEvent::Title(title) => assert!(title.is_empty()),
            _ => panic!("Expected Title event"),
        }
    }

    #[test]
    fn test_ignored_events() {
        let (tx, rx) = channel();
        let proxy = GpuiEventProxy::new(tx);

        // These events should be ignored and not sent through the channel
        proxy.send_event(Event::MouseCursorDirty);

        // The channel should be empty
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn test_cursor_blinking_change_is_forwarded() {
        // 光标闪烁状态**必须**转发：DECSCUSR(`CSI n q`) 与 `ESC[?12h|l` 都靠它，
        // 视图据此启停闪烁定时器。旧实现把它和 MouseCursorDirty 一起忽略。
        let (tx, rx) = channel();
        let proxy = GpuiEventProxy::new(tx);

        proxy.send_event(Event::CursorBlinkingChange);

        let event = rx.recv().unwrap();
        assert!(matches!(event, TerminalEvent::CursorBlinkingChange));
    }

    #[test]
    fn test_clipboard_load_keeps_alacritty_formatter() {
        // OSC 52 读取请求必须把 alacritty 给的**应答格式化器**带给宿主，
        // 否则宿主只有文本、拼不出 OSC 52 应答（旧实现直接丢弃 format）。
        use alacritty_terminal::term::ClipboardType;

        let (tx, rx) = channel();
        let proxy = GpuiEventProxy::new(tx);

        let formatter = Arc::new(|s: &str| format!("]52;c;{s}"));
        proxy.send_event(Event::ClipboardLoad(ClipboardType::Clipboard, formatter));

        match rx.recv().unwrap() {
            TerminalEvent::ClipboardLoad(fmt) => {
                assert_eq!(fmt("payload"), "]52;c;payload");
            }
            other => panic!("期望 ClipboardLoad，得到 {other:?}"),
        }
    }

    #[test]
    fn test_disconnected_channel() {
        let (tx, rx) = channel();
        let proxy = GpuiEventProxy::new(tx);

        // Drop the receiver to disconnect the channel
        drop(rx);

        // Sending should not panic even though the channel is disconnected
        proxy.send_event(Event::Wakeup);
    }

    // ---------- 回归：PTY 应答通路（2026-09-10「powershell 终端无效」） ----------
    //
    // 缺陷：`Event::PtyWrite` 被当作「alacritty 内部处理」丢掉，但 alacritty 没有
    // PTY 写端，应答必须由宿主送回。PowerShell/ConPTY 启动时先发 `ESC[6n` 并阻塞
    // 等待 `ESC[{行};{列}R`，应答丢失 → 永远不打印提示符 → 终端全黑。

    /// 把写入收集到共享缓冲的测试用 `Write`。
    #[derive(Clone)]
    struct CollectSink(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for CollectSink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn collect_proxy() -> (GpuiEventProxy, Arc<Mutex<Vec<u8>>>) {
        let (tx, _rx) = channel();
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer: PtyWriter = Arc::new(Mutex::new(Box::new(CollectSink(buf.clone()))));
        (GpuiEventProxy::with_pty(tx, Some(writer), None, None), buf)
    }

    /// ★ 核心回归：`ESC[6n`（DSR 光标位置查询）必须回写 `ESC[1;1R`。
    #[test]
    fn dsr_cursor_position_query_is_answered_on_pty() {
        use crate::terminal::TerminalState;
        let (proxy, buf) = collect_proxy();
        let mut term = TerminalState::new(80, 24, proxy);

        term.process_bytes(b"\x1b[6n");

        let got = String::from_utf8_lossy(&buf.lock()).to_string();
        assert_eq!(
            got, "\x1b[1;1R",
            "DSR 应答没有写回 PTY —— PtyWrite 通路断裂（PowerShell 会因此永久阻塞）"
        );
    }

    /// `ESC[c`（DA 主设备属性查询）必须回写 `ESC[?6c`。
    #[test]
    fn primary_device_attributes_query_is_answered_on_pty() {
        use crate::terminal::TerminalState;
        let (proxy, buf) = collect_proxy();
        let mut term = TerminalState::new(80, 24, proxy);

        term.process_bytes(b"\x1b[c");

        let got = String::from_utf8_lossy(&buf.lock()).to_string();
        assert_eq!(got, "\x1b[?6c", "DA 应答没有写回 PTY");
    }

    /// OSC 颜色查询应答（索引 256 = 前景色）。
    #[test]
    fn color_request_is_answered_on_pty() {
        let (tx, _rx) = channel();
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer: PtyWriter = Arc::new(Mutex::new(Box::new(CollectSink(buf.clone()))));
        let lookup: ColorLookup = Arc::new(|idx| {
            (idx == 256).then_some(Rgb {
                r: 0x11,
                g: 0x22,
                b: 0x33,
            })
        });
        let proxy = GpuiEventProxy::with_pty(tx, Some(writer), Some(lookup), None);

        proxy.send_event(Event::ColorRequest(
            256,
            Arc::new(|rgb: Rgb| {
                format!("\x1b]10;rgb:{:02x}/{:02x}/{:02x}\x07", rgb.r, rgb.g, rgb.b)
            }),
        ));

        let got = String::from_utf8_lossy(&buf.lock()).to_string();
        assert_eq!(got, "\x1b]10;rgb:11/22/33\x07", "颜色查询应答没有写回 PTY");
    }

    /// `CSI 14 t` 文本区像素尺寸查询应答。
    #[test]
    fn text_area_size_request_is_answered_on_pty() {
        let (tx, _rx) = channel();
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer: PtyWriter = Arc::new(Mutex::new(Box::new(CollectSink(buf.clone()))));
        let ws = Arc::new(Mutex::new(WindowSize {
            num_lines: 24,
            num_cols: 80,
            cell_width: 7,
            cell_height: 19,
        }));
        let proxy = GpuiEventProxy::with_pty(tx, Some(writer), None, Some(ws));

        proxy.send_event(Event::TextAreaSizeRequest(Arc::new(|ws: WindowSize| {
            format!(
                "\x1b[4;{};{}t",
                ws.num_lines * ws.cell_height,
                ws.num_cols * ws.cell_width
            )
        })));

        let got = String::from_utf8_lossy(&buf.lock()).to_string();
        assert_eq!(got, "\x1b[4;456;560t", "尺寸查询应答没有写回 PTY");
    }

    /// 应答通路未装配（`new`）时必须静默丢弃，不得 panic。
    #[test]
    fn pty_write_without_writer_is_ignored() {
        let (tx, _rx) = channel();
        let proxy = GpuiEventProxy::new(tx);
        proxy.send_event(Event::PtyWrite("\x1b[1;1R".to_string()));
    }
}
