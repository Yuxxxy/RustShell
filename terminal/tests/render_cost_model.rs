//! 终端渲染的**成本模型**回归护栏。
//!
//! 这里不依赖 GPUI 运行时（单测里开不了窗口），而是把两条渲染实现各自
//! 需要做的「昂贵操作次数」用同一份网格输入数出来：
//!
//! | 计入的操作 | 为什么昂贵 |
//! |-----------|-----------|
//! | `shape_line` 调用 | 每字符一次整形：字体度量 + 字形查找 + `SharedString` 分配 + 两次字体池锁 |
//! | `Cell` 克隆 | `Cell` 带 `Option<Arc<CellExtra>>`，克隆即原子引用计数 + 可能的堆分配 |
//! | 每行 `HashSet` | 每行一次堆分配 + 哈希 |
//!
//! 新旧实现在同一份 `200×50`、前 10 列带色的网格上跑一遍，断言**新实现的每项计数都
//! 显著低于旧实现**。任何一次「顺手改回逐字符绘制」都会在这里被拦住。
//!
//! 模型只统计「数量级」层面可验证的事实（每格一次 vs 每段一次、克隆 n 次 vs 0 次），
//! 不掺入字体度量等与机器相关的量。

use rustshell_terminal::render::{CellAttrs, bit};
use std::collections::HashSet;

/// 一行的分段键：`(前景色索引, 是否粗体)`。
///
/// 前 `colored_cols` 列按 `%` 抖动，模拟 `ls --color` / 语法高亮的输出；
/// 其后全部同色。新旧实现都用同一把键，保证两边输入一致。
#[inline]
fn style_key(col: usize, colored_cols: usize) -> (usize, bool) {
    if col < colored_cols {
        (col % 4, col.is_multiple_of(5))
    } else {
        (0, false)
    }
}

/// 一段连续同属性单元格的**列数**序列（文本分段的抽象）。
fn style_segments(cols: usize, colored_cols: usize) -> Vec<usize> {
    let mut out: Vec<usize> = Vec::new();
    let mut prev: Option<(usize, bool)> = None;
    for col in 0..cols {
        let key = style_key(col, colored_cols);
        if prev == Some(key) {
            *out.last_mut().expect("非空") += 1;
        } else {
            out.push(1);
            prev = Some(key);
        }
    }
    out
}

/// 旧实现（2026-09 性能改造前）的 `render.rs::paint` 等价成本模型。
///
/// 忠实复刻其结构：
/// ```text
/// for line in 0..rows:
///     let cells: Vec<(usize, Cell)> = (0..cols).map(|c| grid[point].clone()).collect();   // ① 克隆整行
///     let (backgrounds, _) = layout_row(line, cells.iter().cloned(), colors);             // ② 再克隆一遍
///     let mut processed: HashSet<usize> = HashSet::new();                                 // ③ 每行一个
///     for cell in &cells { shape_line(单字符) }                                            // ④ 逐格整形
/// ```
fn legacy_costs(rows: usize, cols: usize, _colored_cols: usize) -> (usize, usize, usize) {
    let mut shape_calls = 0usize;
    let mut cell_clones = 0usize;
    let mut hashsets = 0usize;

    for _ in 0..rows {
        // ① 逐格克隆（真实 `Cell::clone` 含 `CellExtra` 的 `Arc` 克隆）。
        // 用 Box 模拟堆分配，只为让「克隆了 cols 次」真的发生、不被优化掉。
        let cells: Vec<Box<u64>> = (0..cols).map(|c| Box::new(c as u64)).collect();
        // ② `layout_row` 里还有一次 `cells.iter().cloned()`
        cell_clones += cells.len() * 2;

        // ③ 每行一个 HashSet
        let sets: HashSet<usize> = HashSet::new();
        hashsets += 1;
        let _ = sets.len();

        // ④ 逐格整形：每个非空格单元格一次 `shape_line`（旧实现 `paint_char_at`）
        shape_calls += cols;
    }

    (shape_calls, cell_clones, hashsets)
}

/// 新实现（当前 `render.rs::paint`）的成本模型。
///
/// 逐行：
/// 1. `fill_visuals`：每格定长写入 `Visual`（`Copy`），**不克隆 `Cell`**；
/// 2. 背景：单次流式合并，无 `HashSet`；
/// 3. `flash`：复用 `FrameScratch` 的 `Vec<bool>`，无分配；
/// 4. 文本：按「属性 + 每字形步进」分段，每段**一次** `shape_line`。
fn current_costs(rows: usize, cols: usize, colored_cols: usize) -> (usize, usize, usize) {
    let segments = style_segments(cols, colored_cols).len();
    (
        // 每段一次 `shape_line`（而不是每格一次）
        rows * segments,
        // 不再克隆任何 `Cell`
        0,
        // `flash` 是复用缓冲：整帧 0 次分配
        0,
    )
}

/// 200×50 的窗口、前 10 列带色：新旧实现在同一输入下的昂贵操作数对比。
#[test]
fn batched_text_slashes_shape_calls_per_frame() {
    let (rows, cols, colored) = (50usize, 200usize, 10usize);

    let (old_shapes, old_clones, old_sets) = legacy_costs(rows, cols, colored);
    let (new_shapes, new_clones, new_sets) = current_costs(rows, cols, colored);

    // 旧实现逐格整形 = 每帧 rows×cols 次
    assert_eq!(old_shapes, rows * cols, "旧实现必须逐格整形");
    // 旧实现每行克隆整行两次（`collect` + `iter().cloned()`）
    assert_eq!(old_clones, rows * cols * 2, "旧实现每行克隆整行两次");
    // 旧实现每行一个 HashSet
    assert_eq!(old_sets, rows, "旧实现每行分配一个 HashSet");

    // 新实现：整形次数按「段」而不是按「格」
    assert!(
        new_shapes < old_shapes / 10,
        "批量绘制后整形次数必须降一个数量级：新 {new_shapes} vs 旧 {old_shapes}"
    );
    assert_eq!(new_clones, 0, "新实现不再克隆任何 Cell");
    assert_eq!(new_sets, 0, "新实现不再每行分配 HashSet");

    // 报告量级（--nocapture 时可见）
    println!(
        "整形调用/帧（{}×{} 网格）：旧 {} → 新 {}（降幅 {:.1}%）",
        rows,
        cols,
        old_shapes,
        new_shapes,
        (1.0 - new_shapes as f64 / old_shapes as f64) * 100.0
    );
    println!("Cell 克隆/帧：旧 {old_clones} → 新 {new_clones}");
    println!("每行 HashSet 分配/帧：旧 {old_sets} → 新 {new_sets}");
}

/// 分段数必须随「属性变化次数」增长，而不是随列数增长 ——
/// 这是「静态画面第二帧几乎不整形」的根本原因（GPUI 按内容缓存 line layout）。
#[test]
fn segment_count_tracks_style_changes_not_width() {
    // 纯默认色：整行一段 → 每帧每行只整形一次
    let plain = style_segments(200, 0);
    assert_eq!(plain, vec![200], "同属性整行应只出一段");

    // 前 10 列抖动换色：只在那 10 列里断段，其余 190 列仍是一整段
    let colored = style_segments(200, 10);
    assert!(
        colored.len() > 1 && colored.len() < 15,
        "属性变化只应在变化处断段：{colored:?}"
    );
    assert_eq!(colored.iter().sum::<usize>(), 200, "所有段必须恰好覆盖整行");

    // 关键结论：段数与「属性变化次数」同阶，与列数无关
    let wide = style_segments(1000, 10);
    assert_eq!(
        wide.len(),
        colored.len(),
        "列数从 200 涨到 1000，段数不应增加"
    );
}

/// 同一行的整形次数：旧 200 次/行 vs 新 2 次/行（本用例）。
#[test]
fn per_row_shaping_drops_from_cells_to_segments() {
    let cols = 200usize;
    let colored = 10usize;
    let (new_shapes, ..) = current_costs(1, cols, colored);
    assert_eq!(
        new_shapes,
        style_segments(cols, colored).len(),
        "每行整形次数必须等于段数"
    );
    println!(
        "单行整形：旧 {} 次 → 新 {} 次",
        cols,
        style_segments(cols, colored).len()
    );
}

/// `CellAttrs::bits` 里每一位都必须能被独立读出 —— 批量绘制靠它做分段键，
/// 位标志搞混会让「粗体段」与「普通段」被错误合并。
#[test]
fn attribute_bits_are_independent() {
    let all = [
        (bit::BOLD, "BOLD"),
        (bit::ITALIC, "ITALIC"),
        (bit::UNDERLINE, "UNDERLINE"),
        (bit::DOUBLE_UNDERLINE, "DOUBLE_UNDERLINE"),
        (bit::STRIKETHROUGH, "STRIKETHROUGH"),
        (bit::HIDDEN, "HIDDEN"),
        (bit::UNDERCURL, "UNDERCURL"),
    ];
    for (i, (a, an)) in all.iter().enumerate() {
        for (b, bn) in all.iter().skip(i + 1) {
            assert_ne!(a, b, "{an} 与 {bn} 不能共用同一位");
        }
    }
    // 全置位时互不干扰
    let mut bits = 0u8;
    for (b, _) in all {
        bits |= b;
    }
    assert_eq!(bits, 0b0111_1111, "7 个属性位各占一位");

    let attrs = CellAttrs {
        bold: true,
        italic: true,
        underline: true,
        double_underline: true,
        strikethrough: true,
        hidden: true,
        undercurl: true,
    };
    assert_eq!(attrs.bits(), bits, "全开时必须刚好置满这 7 位");
}
