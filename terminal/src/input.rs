//! Keyboard input handling for the terminal emulator.
//!
//! This module provides [`keystroke_to_bytes`], which converts GPUI keyboard
//! events into terminal escape sequences that can be written to the PTY.
//!
//! # Key Mappings
//!
//! ## Special Keys
//!
//! | Key | Sequence | Notes |
//! |-----|----------|-------|
//! | Enter | `\r` (0x0D) | Carriage return |
//! | Escape | `\x1b` (0x1B) | ESC |
//! | Backspace | `\x7f` (0x7F) | DEL |
//! | Tab | `\t` (0x09) | Horizontal tab |
//! | Shift+Tab | `\x1b[Z` | Backtab |
//! | Space | ` ` (0x20) | Space |
//! | Ctrl+Space | `\x00` | NUL |
//!
//! ## Arrow Keys
//!
//! Arrow key sequences depend on application cursor mode:
//!
//! | Key | Normal Mode | App Cursor Mode |
//! |-----|-------------|-----------------|
//! | Up | `\x1b[A` | `\x1bOA` |
//! | Down | `\x1b[B` | `\x1bOB` |
//! | Right | `\x1b[C` | `\x1bOC` |
//! | Left | `\x1b[D` | `\x1bOD` |
//!
//! ## Navigation Keys
//!
//! | Key | Sequence |
//! |-----|----------|
//! | Home | `\x1b[H` |
//! | End | `\x1b[F` |
//! | PageUp | `\x1b[5~` |
//! | PageDown | `\x1b[6~` |
//! | Insert | `\x1b[2~` |
//! | Delete | `\x1b[3~` |
//!
//! ## Function Keys
//!
//! | Key | Sequence |
//! |-----|----------|
//! | F1-F4 | `\x1bOP` - `\x1bOS` |
//! | F5-F12 | `\x1b[15~` - `\x1b[24~` |
//!
//! ## Control Combinations
//!
//! Ctrl+A through Ctrl+Z map to ASCII control characters 0x01-0x1A:
//!
//! | Combination | Byte |
//! |-------------|------|
//! | Ctrl+A | 0x01 |
//! | Ctrl+C | 0x03 (interrupt) |
//! | Ctrl+D | 0x04 (EOF) |
//! | Ctrl+Z | 0x1A (suspend) |
//!
//! ## Alt Combinations
//!
//! Alt+key sends ESC followed by the key: `\x1b` + key
//!
//! # Terminal Mode Effects
//!
//! The [`TermMode`] flags affect key sequences:
//!
//! - **APP_CURSOR**: Changes arrow key sequences from CSI to SS3 format
//!
//! # Example
//!
//! ```
//! use gpui::Keystroke;
//! use alacritty_terminal::term::TermMode;
//! use rustshell_terminal::input::keystroke_to_bytes;
//!
//! // Enter key
//! let keystroke = Keystroke::parse("enter").unwrap();
//! assert_eq!(keystroke_to_bytes(&keystroke, TermMode::empty()), Some(b"\r".to_vec()));
//!
//! // Ctrl+C (interrupt)
//! let keystroke = Keystroke::parse("ctrl-c").unwrap();
//! assert_eq!(keystroke_to_bytes(&keystroke, TermMode::empty()), Some(vec![0x03]));
//! ```

use alacritty_terminal::term::TermMode;
use gpui::Keystroke;

/// Convert a GPUI keystroke to terminal escape sequence bytes.
///
/// This function translates GPUI keyboard events into the appropriate byte sequences
/// expected by terminal applications. It handles special keys, control characters,
/// and application cursor mode.
///
/// # Arguments
///
/// * `keystroke` - The GPUI keystroke to convert
/// * `mode` - The current terminal mode (affects arrow key sequences)
///
/// # Returns
///
/// An optional vector of bytes representing the terminal escape sequence.
/// Returns `None` if the keystroke should not produce any output.
///
/// # Examples
///
/// ```
/// use gpui::Keystroke;
/// use alacritty_terminal::term::TermMode;
/// use rustshell_terminal::input::keystroke_to_bytes;
///
/// let keystroke = Keystroke::parse("enter").unwrap();
/// let bytes = keystroke_to_bytes(&keystroke, TermMode::empty());
/// assert_eq!(bytes, Some(b"\r".to_vec()));
/// ```
pub fn keystroke_to_bytes(keystroke: &Keystroke, mode: TermMode) -> Option<Vec<u8>> {
    // Handle special keys first
    match keystroke.key.as_str() {
        // Basic control characters
        "space" => {
            if keystroke.modifiers.control {
                return Some(b"\x00".to_vec()); // Ctrl+Space = NUL
            }
            return Some(b" ".to_vec());
        }
        "enter" => return Some(b"\r".to_vec()),
        "escape" => return Some(b"\x1b".to_vec()),
        "backspace" => return Some(b"\x7f".to_vec()),
        "tab" => {
            // Shift+Tab sends a different sequence
            if keystroke.modifiers.shift {
                return Some(b"\x1b[Z".to_vec());
            }
            return Some(b"\t".to_vec());
        }

        // Arrow keys - check APP_CURSOR mode
        "up" => {
            if mode.contains(TermMode::APP_CURSOR) {
                return Some(b"\x1bOA".to_vec());
            }
            return Some(b"\x1b[A".to_vec());
        }
        "down" => {
            if mode.contains(TermMode::APP_CURSOR) {
                return Some(b"\x1bOB".to_vec());
            }
            return Some(b"\x1b[B".to_vec());
        }
        "right" => {
            if mode.contains(TermMode::APP_CURSOR) {
                return Some(b"\x1bOC".to_vec());
            }
            return Some(b"\x1b[C".to_vec());
        }
        "left" => {
            if mode.contains(TermMode::APP_CURSOR) {
                return Some(b"\x1bOD".to_vec());
            }
            return Some(b"\x1b[D".to_vec());
        }

        // Navigation keys
        "home" => return Some(b"\x1b[H".to_vec()),
        "end" => return Some(b"\x1b[F".to_vec()),
        "pageup" => return Some(b"\x1b[5~".to_vec()),
        "pagedown" => return Some(b"\x1b[6~".to_vec()),
        "insert" => return Some(b"\x1b[2~".to_vec()),
        "delete" => return Some(b"\x1b[3~".to_vec()),

        // Function keys
        "f1" => return Some(b"\x1bOP".to_vec()),
        "f2" => return Some(b"\x1bOQ".to_vec()),
        "f3" => return Some(b"\x1bOR".to_vec()),
        "f4" => return Some(b"\x1bOS".to_vec()),
        "f5" => return Some(b"\x1b[15~".to_vec()),
        "f6" => return Some(b"\x1b[17~".to_vec()),
        "f7" => return Some(b"\x1b[18~".to_vec()),
        "f8" => return Some(b"\x1b[19~".to_vec()),
        "f9" => return Some(b"\x1b[20~".to_vec()),
        "f10" => return Some(b"\x1b[21~".to_vec()),
        "f11" => return Some(b"\x1b[23~".to_vec()),
        "f12" => return Some(b"\x1b[24~".to_vec()),

        _ => {}
    }

    // Handle Ctrl+key combinations
    if keystroke.modifiers.control {
        let key = keystroke.key.as_str();

        // Ctrl+A through Ctrl+Z map to 0x01 through 0x1a
        if key.len() == 1 {
            let ch = key.chars().next().unwrap();
            if ch.is_ascii_alphabetic() {
                // Convert to uppercase and then to control character
                let upper = ch.to_ascii_uppercase();
                let ctrl_char = (upper as u8) - b'@';
                return Some(vec![ctrl_char]);
            }

            // Special Ctrl combinations
            match ch {
                '[' => return Some(b"\x1b".to_vec()),  // Ctrl+[
                '\\' => return Some(b"\x1c".to_vec()), // Ctrl+\
                ']' => return Some(b"\x1d".to_vec()),  // Ctrl+]
                '^' => return Some(b"\x1e".to_vec()),  // Ctrl+^
                '_' => return Some(b"\x1f".to_vec()),  // Ctrl+_
                '?' => return Some(b"\x7f".to_vec()),  // Ctrl+?
                _ => {}
            }
        }
    }

    // Handle Alt+key combinations
    if keystroke.modifiers.alt {
        let key = keystroke.key.as_str();
        if key.len() == 1 {
            // Alt+key sends ESC followed by the key
            let ch = key.chars().next().unwrap();
            if ch.is_ascii() {
                let mut bytes = vec![b'\x1b'];
                bytes.push(ch as u8);
                return Some(bytes);
            }
        }
    }

    // Handle regular printable characters
    // Use key_char if available (contains the actual typed character with modifiers like Shift)
    if let Some(key_char) = &keystroke.key_char
        && !keystroke.modifiers.control
        && !keystroke.modifiers.alt
    {
        return Some(key_char.as_bytes().to_vec());
    }

    // Fallback to key for single characters
    let key = keystroke.key.as_str();
    if key.len() == 1 {
        let ch = key.chars().next().unwrap();
        if ch.is_ascii() && !keystroke.modifiers.control {
            // Handle shift modifier for uppercase
            let ch = if keystroke.modifiers.shift {
                ch.to_ascii_uppercase()
            } else {
                ch
            };
            return Some(vec![ch as u8]);
        }
        // For non-ASCII characters, encode as UTF-8
        if !keystroke.modifiers.control && !keystroke.modifiers.alt {
            return Some(key.as_bytes().to_vec());
        }
    }

    // If we get here, the keystroke doesn't produce any output
    None
}

/// 该按键是否**归文本输入通路**（IME / `replace_text_in_range`）所有，
/// 而不是由 [`keystroke_to_bytes`] 从 keydown 直接翻译成字节。
///
/// # 为什么必须二选一
///
/// 装了 `EntityInputHandler` 之后，一个可打印字符可能从两条路进来：
///
/// 1. **keydown**（本模块）—— 平台在把按键交给输入法**之前**先同步派发一份；
/// 2. **文本通路** —— Windows 的 `TranslateMessage` → `WM_CHAR`、
///    macOS 的 `insertText:`、X11 的 `key_char` 转发。
///
/// 平台三家的规则是**同一条**：应用一旦在 keydown 阶段消费了按键
/// （macOS 的 `handled == YES`、X11 的 `!propagate`），文本通路就不再投递。
/// 换句话说两条路是**互斥**的，而终端两条都不做屏蔽 —— 会每个字符进两次管道。
///
/// 所以规则是：**可打印字符只归文本通路**。终端在 keydown 遇到它们时
/// 既不写字节、也不消费（不消费，平台的输入法/字符消息才走得下去）。
///
/// 反过来，控制字符与功能键（回车 / 退格 / Tab / Esc / 方向键 / Ctrl+字母 /
/// Alt+字母）走 keydown：它们要么被平台从文本通路里滤掉
/// （Windows `parse_char_message` 滤 `is_control`、X11 只认 `key_utf32 >= 32`），
/// 要么带着 Alt/Ctrl 修饰（X11 只转发「修饰键 ⊆ shift」的按键）。
///
/// `space` 要单列：它是唯一「key 不是单个字符、却是可打印字符」的按键
/// （key 为 `"space"`，字符是 `' '`）。
pub fn is_text_input_keystroke(keystroke: &Keystroke) -> bool {
    if keystroke.modifiers.control || keystroke.modifiers.alt || keystroke.modifiers.platform {
        return false;
    }
    keystroke.key == "space" || keystroke.key.chars().count() == 1
}

/// 粘贴文本规范化（纯函数，便于单测）。
///
/// - `normalize_newlines`：CRLF / LF 统一成 **CR**。终端语义里 CR 才是「回车执行」，
///   单独送 LF 只下移一行不回列，多行粘贴会阶梯状错位 ——
///   这正是「粘贴多行命令」看起来乱七八糟的根因。
/// - `bracketed`：远端声明支持 bracketed paste（`ESC[?2004h`）时，用
///   `ESC[200~` / `ESC[201~` 包裹整段，让 shell 把它当成**一次粘贴**。
///   没有这层包裹，zsh / fish 会把多行粘贴逐行当命令**立即执行**（危险）。
///
/// 注意：bracketed 分支**不**过滤正文里的 `ESC`。剪贴板是用户自己的数据，
/// 过滤会破坏「粘贴一段合法 ANSI 序列」这类正常用法；与 alacritty 行为一致。
pub fn normalize_paste(text: &str, bracketed: bool, normalize_newlines: bool) -> String {
    let body = if normalize_newlines {
        text.replace("\r\n", "\r").replace('\n', "\r")
    } else {
        text.to_string()
    };
    if bracketed {
        format!("\x1b[200~{body}\x1b[201~")
    } else {
        body
    }
}

/// 文本里「会执行」的换行个数（纯函数，便于单测）。
///
/// 三种写法按**一个**换行算：`\r`（终端里的回车 = 执行）、`\n`（readline 默认也绑
/// `accept-line`）、`\r\n`（Windows 剪贴板的行尾，两个字节一件事）。分开数会把
/// 「粘贴一份 CRLF 文本」的行数报成两倍，用户按提示核对时就会以为我们数错了。
///
/// 注意它统计的是**换行**，不是行数：没有换行的单行文本返回 `0`。
/// 展示用的行数由调用方算 `+1`（见 [`needs_paste_confirm`] 的文档）。
pub fn paste_newline_count(text: &str) -> usize {
    let b = text.as_bytes();
    let mut n = 0;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\r' => {
                n += 1;
                // CRLF 吃掉两个字节记一个：`\r\n` 是「一个行尾」，不是两个
                if b.get(i + 1) == Some(&b'\n') {
                    i += 2;
                    continue;
                }
            }
            b'\n' => n += 1,
            _ => {}
        }
        i += 1;
    }
    n
}

/// 这次粘贴要不要先问用户（纯函数，便于单测）。
///
/// `protect_lines` 是「允许**直接落管道**的最大行数」（行数 = 换行数 + 1）：
/// 换行数 `≥ protect_lines` 即要确认，等价于「行数 > protect_lines」。
///
/// 默认值 1 的语义因此是**任何换行都要问**（单行无换行静默直通）；
/// `0` = 一律确认；调大即放松。`0` 与「关闭保护」的区别由调用方表达
/// （`TerminalConfig::paste_protect_lines` 的 `None` = 整个闸关掉，
/// 连问都不问），本函数只管「开了闸、阈值是多少」。
pub fn needs_paste_confirm(text: &str, protect_lines: u32) -> bool {
    paste_newline_count(text) >= protect_lines as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_enter_key() {
        let keystroke = Keystroke::parse("enter").unwrap();
        let bytes = keystroke_to_bytes(&keystroke, TermMode::empty());
        assert_eq!(bytes, Some(b"\r".to_vec()));
    }

    #[test]
    fn test_escape_key() {
        let keystroke = Keystroke::parse("escape").unwrap();
        let bytes = keystroke_to_bytes(&keystroke, TermMode::empty());
        assert_eq!(bytes, Some(b"\x1b".to_vec()));
    }

    #[test]
    fn test_backspace_key() {
        let keystroke = Keystroke::parse("backspace").unwrap();
        let bytes = keystroke_to_bytes(&keystroke, TermMode::empty());
        assert_eq!(bytes, Some(b"\x7f".to_vec()));
    }

    #[test]
    fn test_tab_key() {
        let keystroke = Keystroke::parse("tab").unwrap();
        let bytes = keystroke_to_bytes(&keystroke, TermMode::empty());
        assert_eq!(bytes, Some(b"\t".to_vec()));
    }

    #[test]
    fn test_shift_tab() {
        let keystroke = Keystroke::parse("shift-tab").unwrap();
        let bytes = keystroke_to_bytes(&keystroke, TermMode::empty());
        assert_eq!(bytes, Some(b"\x1b[Z".to_vec()));
    }

    #[test]
    fn test_arrow_keys_normal_mode() {
        let mode = TermMode::empty();

        let up = Keystroke::parse("up").unwrap();
        assert_eq!(keystroke_to_bytes(&up, mode), Some(b"\x1b[A".to_vec()));

        let down = Keystroke::parse("down").unwrap();
        assert_eq!(keystroke_to_bytes(&down, mode), Some(b"\x1b[B".to_vec()));

        let right = Keystroke::parse("right").unwrap();
        assert_eq!(keystroke_to_bytes(&right, mode), Some(b"\x1b[C".to_vec()));

        let left = Keystroke::parse("left").unwrap();
        assert_eq!(keystroke_to_bytes(&left, mode), Some(b"\x1b[D".to_vec()));
    }

    #[test]
    fn test_arrow_keys_app_cursor_mode() {
        let mode = TermMode::APP_CURSOR;

        let up = Keystroke::parse("up").unwrap();
        assert_eq!(keystroke_to_bytes(&up, mode), Some(b"\x1bOA".to_vec()));

        let down = Keystroke::parse("down").unwrap();
        assert_eq!(keystroke_to_bytes(&down, mode), Some(b"\x1bOB".to_vec()));

        let right = Keystroke::parse("right").unwrap();
        assert_eq!(keystroke_to_bytes(&right, mode), Some(b"\x1bOC".to_vec()));

        let left = Keystroke::parse("left").unwrap();
        assert_eq!(keystroke_to_bytes(&left, mode), Some(b"\x1bOD".to_vec()));
    }

    #[test]
    fn test_navigation_keys() {
        let mode = TermMode::empty();

        let home = Keystroke::parse("home").unwrap();
        assert_eq!(keystroke_to_bytes(&home, mode), Some(b"\x1b[H".to_vec()));

        let end = Keystroke::parse("end").unwrap();
        assert_eq!(keystroke_to_bytes(&end, mode), Some(b"\x1b[F".to_vec()));

        let pageup = Keystroke::parse("pageup").unwrap();
        assert_eq!(keystroke_to_bytes(&pageup, mode), Some(b"\x1b[5~".to_vec()));

        let pagedown = Keystroke::parse("pagedown").unwrap();
        assert_eq!(
            keystroke_to_bytes(&pagedown, mode),
            Some(b"\x1b[6~".to_vec())
        );

        let insert = Keystroke::parse("insert").unwrap();
        assert_eq!(keystroke_to_bytes(&insert, mode), Some(b"\x1b[2~".to_vec()));

        let delete = Keystroke::parse("delete").unwrap();
        assert_eq!(keystroke_to_bytes(&delete, mode), Some(b"\x1b[3~".to_vec()));
    }

    #[test]
    fn test_function_keys() {
        let mode = TermMode::empty();

        let f1 = Keystroke::parse("f1").unwrap();
        assert_eq!(keystroke_to_bytes(&f1, mode), Some(b"\x1bOP".to_vec()));

        let f2 = Keystroke::parse("f2").unwrap();
        assert_eq!(keystroke_to_bytes(&f2, mode), Some(b"\x1bOQ".to_vec()));

        let f5 = Keystroke::parse("f5").unwrap();
        assert_eq!(keystroke_to_bytes(&f5, mode), Some(b"\x1b[15~".to_vec()));

        let f12 = Keystroke::parse("f12").unwrap();
        assert_eq!(keystroke_to_bytes(&f12, mode), Some(b"\x1b[24~".to_vec()));
    }

    #[test]
    fn test_ctrl_combinations() {
        let mode = TermMode::empty();

        // Ctrl+A = 0x01
        let ctrl_a = Keystroke::parse("ctrl-a").unwrap();
        assert_eq!(keystroke_to_bytes(&ctrl_a, mode), Some(vec![0x01]));

        // Ctrl+C = 0x03
        let ctrl_c = Keystroke::parse("ctrl-c").unwrap();
        assert_eq!(keystroke_to_bytes(&ctrl_c, mode), Some(vec![0x03]));

        // Ctrl+Z = 0x1a
        let ctrl_z = Keystroke::parse("ctrl-z").unwrap();
        assert_eq!(keystroke_to_bytes(&ctrl_z, mode), Some(vec![0x1a]));

        // Ctrl+Space = 0x00
        let ctrl_space = Keystroke::parse("ctrl-space").unwrap();
        assert_eq!(keystroke_to_bytes(&ctrl_space, mode), Some(vec![0x00]));
    }

    #[test]
    fn test_alt_combinations() {
        let mode = TermMode::empty();

        // Alt+a sends ESC followed by 'a'
        let alt_a = Keystroke::parse("alt-a").unwrap();
        assert_eq!(keystroke_to_bytes(&alt_a, mode), Some(b"\x1ba".to_vec()));

        // Alt+x sends ESC followed by 'x'
        let alt_x = Keystroke::parse("alt-x").unwrap();
        assert_eq!(keystroke_to_bytes(&alt_x, mode), Some(b"\x1bx".to_vec()));
    }

    #[test]
    fn test_regular_characters() {
        let mode = TermMode::empty();

        let a = Keystroke::parse("a").unwrap();
        assert_eq!(keystroke_to_bytes(&a, mode), Some(b"a".to_vec()));

        let z = Keystroke::parse("z").unwrap();
        assert_eq!(keystroke_to_bytes(&z, mode), Some(b"z".to_vec()));

        let zero = Keystroke::parse("0").unwrap();
        assert_eq!(keystroke_to_bytes(&zero, mode), Some(b"0".to_vec()));
    }

    #[test]
    fn paste_normalizes_crlf_and_lf_to_cr() {
        // 多行粘贴必须逐行回车执行：LF / CRLF 都归一为 CR
        assert_eq!(normalize_paste("a\nb", false, true), "a\rb");
        assert_eq!(normalize_paste("a\r\nb", false, true), "a\rb");
        assert_eq!(normalize_paste("a\r\nb\nc", false, true), "a\rb\rc");
    }

    #[test]
    fn paste_without_normalization_keeps_bytes() {
        assert_eq!(normalize_paste("a\nb", false, false), "a\nb");
    }

    #[test]
    fn paste_bracketed_wraps_whole_payload_once() {
        // 整段一个包裹（开头一次、结尾一次），而不是每行一个
        let out = normalize_paste("ls -l\nps aux", true, true);
        assert_eq!(out, "\x1b[200~ls -l\rps aux\x1b[201~");
        assert_eq!(out.matches("\x1b[200~").count(), 1);
        assert_eq!(out.matches("\x1b[201~").count(), 1);
    }

    #[test]
    fn paste_trailing_newline_becomes_trailing_cr() {
        // 末尾换行 = 粘贴后直接执行最后一条命令
        assert_eq!(normalize_paste("echo hi\n", false, true), "echo hi\r");
    }

    /// 换行计数：CR / LF / CRLF 都是**一个**换行，CRLF 不数成两个。
    ///
    /// 数错的方向都有害：数少了该拦的不拦（一份 CRLF 脚本被当成两行以内），
    /// 数多了把单行文本当成多行（每次粘贴都弹框，用户学会无脑回车）。
    #[test]
    fn paste_newline_count_treats_cr_lf_crlf_as_one_each() {
        assert_eq!(paste_newline_count("ls -l"), 0);
        assert_eq!(paste_newline_count(""), 0);
        assert_eq!(paste_newline_count("a\nb"), 1);
        assert_eq!(paste_newline_count("a\rb"), 1);
        assert_eq!(
            paste_newline_count("a\r\nb"),
            1,
            "CRLF 是一个行尾，不是两个"
        );
        assert_eq!(paste_newline_count("a\r\nb\nc"), 2);
        assert_eq!(paste_newline_count("a\r\nb\r\nc"), 2);
        assert_eq!(paste_newline_count("echo hi\n"), 1, "结尾换行也要数");
        assert_eq!(paste_newline_count("\n\n"), 2);
        assert_eq!(
            paste_newline_count("\r\n\r\n"),
            2,
            "空行之间的 CRLF 各算一个"
        );
        // 非换行的字节不能被误认（UTF-8 多字节序列里没有 0x0D/0x0A，但别靠这个巧合）
        assert_eq!(paste_newline_count("中文🎉 emoji"), 0);
    }

    /// 阈值判定：默认 1 ⇒ 任何换行都要问，单行静默；0 ⇒ 一律问；调大即放松。
    #[test]
    fn paste_confirm_threshold_semantics() {
        // 默认阈值 1：单行无换行不打扰，含换行（哪怕只是结尾那一个）都要问
        assert!(!needs_paste_confirm("ls -l", 1));
        assert!(needs_paste_confirm("echo hi\n", 1));
        assert!(needs_paste_confirm("a\nb", 1));
        assert!(needs_paste_confirm("a\r\nb", 1), "CRLF 也算一次换行");

        // 0 = 一律确认（连单行也问）
        assert!(needs_paste_confirm("ls -l", 0));

        // 调大 = 放松到「超过 n 行才问」
        assert!(!needs_paste_confirm("a\nb", 2), "2 行、阈值 2 ⇒ 直通");
        assert!(needs_paste_confirm("a\nb\nc", 2), "3 行、阈值 2 ⇒ 要问");
        assert!(!needs_paste_confirm("a\nb\nc", 3));

        // 极端：阈值取满 u32 ⇒ 等于关掉（但这是「开了闸」的表达，语义与 None 不同）
        assert!(!needs_paste_confirm("a\nb\nc\nd", u32::MAX));
    }

    #[test]
    fn test_space_key() {
        let mode = TermMode::empty();

        let space = Keystroke::parse("space").unwrap();
        assert_eq!(keystroke_to_bytes(&space, mode), Some(b" ".to_vec()));
    }

    /// 该走文本通路的按键：可打印字符（含空格），不带 Ctrl/Alt/Cmd。
    ///
    /// 漏掉任何一个，键盘那条路就会和 `WM_CHAR` / `insertText:` / `key_char`
    /// 一起把同一个字符送两遍；多算一个，那个键就永远进不了管道。
    #[test]
    fn text_input_keystroke_covers_exactly_the_printable_keys() {
        // 走文本通路：字母、数字、符号、空格
        for spec in ["a", "z", "0", "9", "/", "space", "shift-a", "shift-space"] {
            let k = Keystroke::parse(spec).unwrap();
            assert!(
                is_text_input_keystroke(&k),
                "{spec} 是可打印字符，归文本通路"
            );
        }

        // 走 keydown：控制字符 / 功能键 / 带 Ctrl 或 Alt 或 Cmd 的组合
        for spec in [
            "enter",
            "escape",
            "backspace",
            "tab",
            "shift-tab",
            "up",
            "f5",
            "insert",
            "ctrl-a",
            "ctrl-space",
            "alt-a",
            "cmd-a",
            "shift-insert",
        ] {
            let k = Keystroke::parse(spec).unwrap();
            assert!(
                !is_text_input_keystroke(&k),
                "{spec} 只能从 keydown 走，误判会让它进不了管道"
            );
        }
    }
}
