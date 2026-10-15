# rustshell-terminal

RustShell 自有终端引擎：用 [alacritty_terminal](https://docs.rs/alacritty_terminal) 做 VTE 解析、
用 [GPUI](https://gpui.rs) 做渲染的终端组件。

## 能力

- ANSI 转义序列（VTE）完整解析
- 16 色 / 256 色 / 24 位真彩
- 粗体、斜体、下划线（含双下划线与波浪变体）、删除线、暗淡、**反显**、隐藏
- **光标**：块 / 空心块 / 竖线 / 下划线四种形状（DECSCUSR），**默认闪烁块光标**
  （本地开关 + 远端 `ESC[?12h|l` 共同决定），失焦常亮并停表
- 键盘输入（含 application cursor mode，vim / tmux 兼容）
- **粘贴**：一次性整段写入、行尾转回车（多行逐行执行）、bracketed paste（`ESC[?2004h`）
- 鼠标划选 / 双击选词 / 三击选行，选中即复制
- 滚回（scrollback）与滚回几何 API（`scroll_metrics` / `set_display_offset`）
- 搜索高亮与命中跳转
- OSC 52 剪贴板读写（经宿主实现，引擎内不含剪贴板依赖）
- 框线字符跨单元格连绘
- PTY 输出按指定字符编码转码（GBK / Big5 / Shift_JIS 等，后台线程 + 跨块补全）
- 推模式异步 I/O；接受任意 `Read` / `Write` 流，不绑定特定 PTY 库

## 用法

```rust
use rustshell_terminal::{ColorPalette, TerminalConfig, TerminalView};

let terminal = cx.new(|cx| {
    TerminalView::new(pty_writer, pty_reader, TerminalConfig::default(), cx)
        .with_resize_callback(|cols, rows| {
            // 通知 PTY 新的行列数
        })
        .with_exit_callback(|_window, cx| {
            cx.quit();
        })
        .with_clipboard_store_callback(|_window, cx, text| {
            // OSC 52 写：转交宿主剪贴板
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(text.to_string()));
        })
        .with_clipboard_load_callback(|_window, cx| {
            // OSC 52 读：由引擎用 alacritty 的格式化器拼应答
            cx.read_from_clipboard().and_then(|item| item.text())
        })
});
```

## 配置

```rust
let colors = ColorPalette::builder()
    .background(0x0D, 0x11, 0x17)
    .foreground(0xE6, 0xED, 0xF3)
    .cursor(0x58, 0xA6, 0xFF)
    .black(0x48, 0x4F, 0x58)
    // ... 其余 15 个 ANSI 色
    .build();

let config = TerminalConfig {
    font_family: "JetBrains Mono".into(),
    font_size: px(14.0),
    cols: 80,
    rows: 24,
    scrollback: 10_000,
    line_height_multiplier: 1.2,
    padding: Edges::all(px(8.0)),
    colors,
    encoding: Some("utf-8".into()),
    // 光标闪烁：本地开关 + 半周期（毫秒）
    cursor_blink: true,
    cursor_blink_interval_ms: 530,
    // 粘贴时把 CRLF / LF 统一成 CR（多行粘贴逐行执行）
    paste_normalize_newlines: true,
};
```

`update_config` 可热更新字号 / 配色 / 闪烁 / 粘贴策略；**编码不热切**（转码状态在后台读线程上，
中途切换会让跨块字节流错位）。

## 与宿主的分工

| 职责 | 归属 |
|---|---|
| VTE 解析、网格与滚回、渲染、键位到字节、粘贴规范化 | 本 crate |
| 系统剪贴板（OSC 52 读写） | 宿主（经 `with_clipboard_*_callback`） |
| PTY 尺寸变更（resize） | 宿主（经 `with_resize_callback`） |
| 字符编码选择 | 宿主（`TerminalConfig::encoding`） |

引擎**不**直接触碰系统剪贴板，也不因此引入原生依赖（`arboard` 已移除）。

## License

本 crate 以 **Apache-2.0** 发布，与仓库其余部分一致（见仓库根 `LICENSE`）。

它源自 [zortax/gpui-terminal](https://github.com/zortax/gpui-terminal)（MIT OR Apache-2.0
双许可）。上游为双许可，本 crate 选择其中的 **Apache-2.0** 分支分发；上游的 MIT 版权声明与
Apache-2.0 全文均已随本 crate 原样保留（`LICENSE-MIT` / `LICENSE-APACHE`），未作修改。
