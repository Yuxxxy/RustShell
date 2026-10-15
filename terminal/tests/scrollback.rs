//! 滚回（scrollback）回归测试。
//!
//! 背景：用户反馈「终端内容无法滚动」。根因是 `render::paint` 绘制视口第 i 行时
//! 用 `Line(i)` 直接索引 alacritty 网格，漏掉了 `display_offset` —— 而 `Line`
//! 是**视口坐标**（`Line(0)` = 当前视口首行），于是滚轮只改内部偏移、画面恒定不变。
//!
//! 本测试同时锁死两件事：
//! 1. alacritty `Line` 的实际语义（防止有人凭直觉改公式）；
//! 2. `render::viewport_line` 的换算公式（paint 必须走这个函数）。

use std::sync::mpsc::channel;

use alacritty_terminal::grid::Scroll;
use alacritty_terminal::index::{Column, Point};
use rustshell_terminal::event::GpuiEventProxy;
use rustshell_terminal::render::viewport_line;
use rustshell_terminal::terminal::TerminalState;

/// 按「视口行」读取每行首字符（与 `render::paint` 同一套换算）
fn viewport_first_chars(t: &TerminalState, rows: usize) -> Vec<char> {
    let off = t.with_term(|term| term.grid().display_offset() as i32);
    t.with_term(|term| {
        let g = term.grid();
        (0..rows)
            .map(|i| g[Point::new(viewport_line(i, off), Column(0))].c)
            .collect()
    })
}

fn setup() -> TerminalState {
    let (tx, _rx) = channel();
    // 3 行屏 × 8 列；写 6 行 A..F，因此 A/B/C 落入历史
    let mut t = TerminalState::new(8, 3, GpuiEventProxy::new(tx));
    for ch in ['A', 'B', 'C', 'D', 'E', 'F'] {
        t.process_bytes(format!("{ch}\r\n").as_bytes());
    }
    t
}

/// 贴底时视口 = 实时屏最后 3 行（E / F / 空行）
#[test]
fn viewport_at_bottom_shows_live_screen() {
    let t = setup();
    assert_eq!(t.with_term(|x| x.grid().display_offset()), 0);
    assert_eq!(viewport_first_chars(&t, 3), vec!['E', 'F', ' ']);
}

/// 上翻 2 行后视口整体上移 2 行 —— 这正是修复前恒为 ['E','F',' '] 的地方
#[test]
fn viewport_scrolls_up_into_history() {
    let t = setup();
    t.with_term_mut(|term| term.scroll_display(Scroll::Delta(2)));

    assert_eq!(t.with_term(|x| x.grid().display_offset()), 2);
    assert_eq!(
        viewport_first_chars(&t, 3),
        vec!['C', 'D', 'E'],
        "上翻 2 行后视口首行应为历史中的 C；若仍是 E 说明 paint 漏加了 display_offset"
    );
}

/// 再滚回底部应恢复贴合实时屏
#[test]
fn scroll_back_to_bottom_restores_live_screen() {
    let t = setup();
    t.with_term_mut(|term| term.scroll_display(Scroll::Delta(2)));
    t.with_term_mut(|term| term.scroll_display(Scroll::Delta(-2)));

    assert_eq!(t.with_term(|x| x.grid().display_offset()), 0);
    assert_eq!(viewport_first_chars(&t, 3), vec!['E', 'F', ' ']);
}

/// `Scroll::Delta` 的上界是历史行数，不会溢出到不存在的内容
#[test]
fn scroll_up_clamps_to_history_size() {
    let t = setup();
    t.with_term_mut(|term| term.scroll_display(Scroll::Delta(9999)));
    let off = t.with_term(|x| x.grid().display_offset());

    // 再滚一次不应继续增加 ⇒ 已被历史长度钳制
    t.with_term_mut(|term| term.scroll_display(Scroll::Delta(9999)));
    assert_eq!(t.with_term(|x| x.grid().display_offset()), off);

    // 视口首行落在最老的一条历史行上
    assert_eq!(viewport_first_chars(&t, 3)[0], 'A');
}

/// 新输出到达时，已上翻的视口应保持静止（不把用户拽回底部）
#[test]
fn scrolled_viewport_stays_put_on_new_output() {
    let mut t = setup();
    t.with_term_mut(|term| term.scroll_display(Scroll::Delta(2)));
    let before = viewport_first_chars(&t, 3);

    t.process_bytes(b"G\r\n");

    assert_eq!(
        viewport_first_chars(&t, 3),
        before,
        "上翻阅读历史时，新输出不应推动视口"
    );
}
