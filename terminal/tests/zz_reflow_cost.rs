//! 临时基准：量化「侧边栏拖动时终端重排」的开销（测完即删）。
//!
//! 拖动侧栏 → 终端窗格宽度每帧变一点 → 列数变化 → `Term::resize` 带 reflow
//! 把整个回滚缓冲重排一遍。本文件用真实 `TerminalState` 量一下这一步多少钱。

use rustshell_terminal::event::GpuiEventProxy;
use rustshell_terminal::terminal::TerminalState;

fn fill(term: &mut TerminalState, lines: usize) {
    // 每行 100 个字符 + 换行：制造「需要换行重排」的真实内容
    let mut chunk = String::new();
    for i in 0..lines {
        chunk.push_str(&format!("{i:06} {}", "x".repeat(90)));
        chunk.push_str("\r\n");
    }
    term.process_bytes(chunk.as_bytes());
}

#[test]
fn measure_reflow_cost() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut term = TerminalState::with_scrollback(120, 40, 10_000, GpuiEventProxy::new(tx));

    fill(&mut term, 12_000);
    println!("--- 滚动缓冲已填满（10000 行上限）---");

    // 1) 单次列数变化的成本（drag 中每变一格列就会走一次）
    let mut samples = Vec::new();
    for step in 0..12 {
        let cols = 120 - step;
        let t0 = std::time::Instant::now();
        term.resize(cols, 40);
        samples.push(t0.elapsed().as_micros());
    }
    println!("单次 resize(reflow) 耗时(us): {samples:?}");
    let avg: u128 = samples.iter().sum::<u128>() / samples.len() as u128;
    println!("平均 {avg} us");

    // 2) 只改行数（不换列）—— 对照组
    let t0 = std::time::Instant::now();
    term.resize(108, 41);
    println!("仅改行数 resize: {} us", t0.elapsed().as_micros());

    // 3) 列数变回原值
    let t0 = std::time::Instant::now();
    term.resize(120, 41);
    println!("列数回摆 resize: {} us", t0.elapsed().as_micros());
}
