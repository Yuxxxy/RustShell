//! 主题系统：Token 定义 + 终端配色映射。禁止 UI/业务代码硬编码颜色，一律经由 Token 消费。

use serde::{Deserialize, Serialize};

/// 主题模式：本迭代仅 Dark 完整实现，枚举预留 Light/System
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ThemeMode {
    #[default]
    Dark,
    Light,
    System,
}

/// 视觉 Token（Dark Token 表）
#[derive(Debug, Clone, Copy)]
pub struct ThemeTokens {
    pub bg_base: u32,       // #0D1117 主背景
    pub bg_elevated: u32,   // #161B22 侧边栏/卡片
    pub bg_overlay: u32,    // #1C2128 悬浮层/标题栏
    pub border_subtle: u32, // #30363D 分割线
    pub fg_primary: u32,    // #E6EDF3 主文字
    pub fg_secondary: u32,  // #B6C2CF 次级文字（表单标签等，暗色高对比）
    pub fg_muted: u32,      // #8B949E 弱化文字（时间戳/计数等低强调信息）
    pub accent: u32,        // #58A6FF 激活态/链接
    pub success: u32,       // #3FB950
    pub purple: u32,        // #D2A8FF 内存指标
    pub warning: u32,       // # 网络/告警
    pub danger: u32,        // #F85149 阈值超限
    /// accent 底色上的前景色（on-accent 语义，随主题变化：Dark 深字 / Light 白字）
    pub on_accent: u32,
    /// danger 底色上的前景色（两种主题均为白字）
    pub on_danger: u32,
}

impl ThemeTokens {
    pub const fn dark() -> Self {
        Self {
            bg_base: 0x0D1117,
            bg_elevated: 0x161B22,
            bg_overlay: 0x1C2128,
            border_subtle: 0x30363D,
            fg_primary: 0xE6EDF3,
            fg_secondary: 0xB6C2CF,
            fg_muted: 0x8B949E,
            accent: 0x58A6FF,
            success: 0x3FB950,
            purple: 0xD2A8FF,
            warning: 0xD29922,
            danger: 0xF85149,
            // Dark：accent(#58A6FF) 为亮蓝，配深字对比度最佳；danger 保持白字
            on_accent: 0x0D1117,
            on_danger: 0xFFFFFF,
        }
    }

    pub fn light() -> Self {
        Self {
            bg_base: 0xFFFFFF,
            bg_elevated: 0xF6F8FA,
            bg_overlay: 0xFFFFFF,
            border_subtle: 0xD0D7DE,
            fg_primary: 0x1F2328,
            fg_secondary: 0x57606A,
            fg_muted: 0x656D76,
            accent: 0x0969DA,
            success: 0x1A7F37,
            purple: 0x8250DF,
            warning: 0x9A6700,
            danger: 0xCF222E,
            // Light：accent(#0969DA)/danger(#CF222E) 均为中深色，白字对比度达标
            on_accent: 0xFFFFFF,
            on_danger: 0xFFFFFF,
        }
    }

    /// 解析后的 Token：System 模式由 UI 层探测系统偏好后传入
    pub fn resolved(mode: ThemeMode, system_prefers_dark: bool) -> Self {
        match mode {
            ThemeMode::Dark => Self::dark(),
            ThemeMode::Light => Self::light(),
            ThemeMode::System => {
                if system_prefers_dark {
                    Self::dark()
                } else {
                    Self::light()
                }
            }
        }
    }
}

/// 终端 16 色 + 基础三色（Theme Token 到 终端 ANSI 配色）
#[derive(Debug, Clone, PartialEq)]
pub struct TerminalPalette {
    pub background: u32,
    pub foreground: u32,
    pub cursor: u32,
    pub normal: [u32; 8],
    pub bright: [u32; 8],
}

/// 配色方案 key（持久化用稳定英文串，不随语言变化）
pub const SCHEME_FOLLOW_THEME: &str = "follow-theme";
pub const SCHEME_CUSTOM: &str = "custom";

/// 可选配色方案：(key, 中文名, 英文名)。顺序即下拉展示顺序。
pub const TERMINAL_SCHEMES: &[(&str, &str, &str)] = &[
    (SCHEME_FOLLOW_THEME, "跟随主题", "Follow Theme"),
    ("dracula", "Dracula", "Dracula"),
    ("one-dark", "One Dark", "One Dark"),
    ("solarized-dark", "Solarized Dark", "Solarized Dark"),
    ("solarized-light", "Solarized Light", "Solarized Light"),
    ("gruvbox-dark", "Gruvbox Dark", "Gruvbox Dark"),
    ("nord", "Nord", "Nord"),
    (SCHEME_CUSTOM, "自定义", "Custom"),
];

impl TerminalPalette {
    /// GitHub Dark 风格终端配色，源自 Theme Token 体系
    pub fn dark() -> Self {
        Self {
            background: 0x0D1117,
            foreground: 0xE6EDF3,
            cursor: 0x58A6FF,
            normal: [
                0x484F58, // black
                0xFF7B72, // red
                0x3FB950, // green
                0xD29922, // yellow
                0x58A6FF, // blue
                0xBC8CFF, // magenta
                0x39C5CF, // cyan
                0xB1BAC4, // white
            ],
            bright: [
                0x6E7681, // bright black
                0xFFA198, // bright red
                0x56D364, // bright green
                0xE3B341, // bright yellow
                0x79C0FF, // bright blue
                0xD2A8FF, // bright magenta
                0x56D4DD, // bright cyan
                0xF0F6FC, // bright white
            ],
        }
    }

    // 已删除`of(mode)`：无调用点，且`of(System)`曾直接返回 light（潜伏 bug）。System 一律走`resolved(mode, system_prefers_dark)`显式解析。

    /// 解析后的 Token：System 模式由 UI 层探测系统偏好后传入
    pub fn resolved(mode: ThemeMode, system_prefers_dark: bool) -> Self {
        match mode {
            ThemeMode::Dark => Self::dark(),
            ThemeMode::Light => Self::light(),
            ThemeMode::System => {
                if system_prefers_dark {
                    Self::dark()
                } else {
                    Self::light()
                }
            }
        }
    }

    pub fn light() -> Self {
        Self {
            background: 0xFFFFFF,
            foreground: 0x1F2328,
            cursor: 0x0969DA,
            normal: [
                0x1F2328, 0xCF222E, 0x116329, 0x4D2D00, 0x0550AE, 0x8250DF, 0x1B7C83, 0x6E7781,
            ],
            bright: [
                0x57606A, 0xA40E26, 0x1A7F37, 0x633C01, 0x0969DA, 0x6639BA, 0x316D71, 0x8C959F,
            ],
        }
    }

    /// 内置配色预设。未知 key /`custom`/`follow-theme`一律返回`None`，由调用方走`resolved()`或`CustomPalette`解析，不 panic。
    pub fn preset(key: &str) -> Option<Self> {
        let p = match key {
            "dracula" => Self {
                background: 0x282A36,
                foreground: 0xF8F8F2,
                cursor: 0xF8F8F2,
                normal: [
                    0x21222C, 0xFF5555, 0x50FA7B, 0xF1FA8C, 0xBD93F9, 0xFF79C6, 0x8BE9FD, 0xF8F8F2,
                ],
                bright: [
                    0x6272A4, 0xFF6E6E, 0x69FF94, 0xFFFFA5, 0xD6ACFF, 0xFF92DF, 0xA4FFFF, 0xFFFFFF,
                ],
            },
            "one-dark" => Self {
                background: 0x282C34,
                foreground: 0xABB2BF,
                cursor: 0x528BFF,
                normal: [
                    0x282C34, 0xE06C75, 0x98C379, 0xE5C07B, 0x61AFEF, 0xC678DD, 0x56B6C2, 0xABB2BF,
                ],
                bright: [
                    0x5C6370, 0xE06C75, 0x98C379, 0xE5C07B, 0x61AFEF, 0xC678DD, 0x56B6C2, 0xFFFFFF,
                ],
            },
            "solarized-dark" => Self {
                background: 0x002B36,
                foreground: 0x839496,
                cursor: 0x93A1A1,
                normal: [
                    0x073642, 0xDC322F, 0x859900, 0xB58900, 0x268BD2, 0xD33682, 0x2AA198, 0xEEE8D5,
                ],
                bright: [
                    0x002B36, 0xCB4B16, 0x586E75, 0x657B83, 0x839496, 0x6C71C4, 0x93A1A1, 0xFDF6E3,
                ],
            },
            "solarized-light" => Self {
                background: 0xFDF6E3,
                foreground: 0x657B83,
                cursor: 0x586E75,
                normal: [
                    0x073642, 0xDC322F, 0x859900, 0xB58900, 0x268BD2, 0xD33682, 0x2AA198, 0xEEE8D5,
                ],
                bright: [
                    0x002B36, 0xCB4B16, 0x586E75, 0x657B83, 0x839496, 0x6C71C4, 0x93A1A1, 0xFDF6E3,
                ],
            },
            "gruvbox-dark" => Self {
                background: 0x282828,
                foreground: 0xEBDBB2,
                cursor: 0xEBDBB2,
                normal: [
                    0x282828, 0xCC241D, 0x98971A, 0xD79921, 0x458588, 0xB16286, 0x689D6A, 0xA89984,
                ],
                bright: [
                    0x928374, 0xFB4934, 0xB8BB26, 0xFABD2F, 0x83A598, 0xD3869B, 0x8EC07C, 0xEBDBB2,
                ],
            },
            "nord" => Self {
                background: 0x2E3440,
                foreground: 0xD8DEE9,
                cursor: 0xD8DEE9,
                normal: [
                    0x3B4252, 0xBF616A, 0xA3BE8C, 0xEBCB8B, 0x81A1C1, 0xB48EAD, 0x88C0D0, 0xE5E9F0,
                ],
                bright: [
                    0x4C566A, 0xBF616A, 0xA3BE8C, 0xEBCB8B, 0x81A1C1, 0xB48EAD, 0x8FBCBB, 0xECEFF4,
                ],
            },
            _ => return None,
        };
        Some(p)
    }
}

/// 终端字体默认值（平台习惯）
pub fn default_terminal_font() -> String {
    if cfg!(target_os = "macos") {
        "Menlo".into()
    } else if cfg!(target_os = "windows") {
        "Consolas".into()
    } else {
        "monospace".into()
    }
}

/// 字体候选列表：按平台给可用等宽字体；`default_terminal_font()`必在首位。
///
/// 2026-09-28 按用户要求「再增加点字体」扩过一轮。补充的都是**中文 Windows 上
/// 真装得有的**等宽字体（`MS Gothic` 在日文包、`NSimSun` 是宋体等宽版、
/// `Sarasa Mono` 是社区常装的中日文等宽字体），不是把 `monospace` 这一族的
/// 别名抄一遍 —— 别名抄进来只会让下拉里出现几个「选了没变化」的重复项。
///
/// 仍然保留尾部的 `monospace` 兜底：它是 `font-family` 通用族，用户机器上
/// 一个候选都没装时至少还能落到系统等宽字体。
pub fn terminal_font_candidates() -> Vec<&'static str> {
    if cfg!(target_os = "macos") {
        vec![
            "Menlo",
            "Monaco",
            "SF Mono",
            "JetBrains Mono",
            "Fira Code",
            "Cascadia Mono",
            "Courier New",
            "monospace",
        ]
    } else if cfg!(target_os = "windows") {
        vec![
            "Consolas",
            "Cascadia Mono",
            "Cascadia Code",
            "JetBrains Mono",
            "Fira Code",
            "Source Code Pro",
            "IBM Plex Mono",
            "Lucida Console",
            "NSimSun",
            "MS Gothic",
            "SimSun-ExtB",
            "Courier New",
            "monospace",
        ]
    } else {
        vec![
            "monospace",
            "DejaVu Sans Mono",
            "Liberation Mono",
            "Noto Sans Mono",
            "JetBrains Mono",
            "Fira Code",
            "Ubuntu Mono",
            "Source Code Pro",
        ]
    }
}

// 终端字符编码

/// 可选编码：(key, 展示名)。key 为持久化稳定串。
pub const TERMINAL_ENCODINGS: &[(&str, &str)] = &[
    ("utf-8", "UTF-8"),
    ("gbk", "GBK"),
    ("gb18030", "GB18030"),
    ("big5", "Big5"),
    ("shift_jis", "Shift_JIS"),
    ("euc-kr", "EUC-KR"),
    ("iso-8859-1", "ISO-8859-1"),
];

/// key 到`encoding_rs::Encoding`；`utf-8`与未知 key 均返回 UTF-8（安全回退）。
pub fn encoding_for(key: &str) -> &'static encoding_rs::Encoding {
    match key {
        "gbk" => encoding_rs::GBK,
        "gb18030" => encoding_rs::GB18030,
        "big5" => encoding_rs::BIG5,
        "shift_jis" => encoding_rs::SHIFT_JIS,
        "euc-kr" => encoding_rs::EUC_KR,
        "iso-8859-1" => encoding_rs::WINDOWS_1252,
        _ => encoding_rs::UTF_8,
    }
}

/// 是否 UTF-8（决定是否走零拷贝直通）
pub fn is_utf8_encoding(key: &str) -> bool {
    key.eq_ignore_ascii_case("utf-8") || key.eq_ignore_ascii_case("utf8")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 未知 key 不 panic，返回 None（由调用方回退 follow-theme）。
    #[test]
    fn preset_unknown_key_returns_none() {
        assert!(TerminalPalette::preset("nope").is_none());
        assert!(TerminalPalette::preset(SCHEME_FOLLOW_THEME).is_none());
        assert!(TerminalPalette::preset(SCHEME_CUSTOM).is_none());
    }

    /// 每个声明的预设 key（排除 follow-theme/custom）都必须有实现，否则下拉选了等于没反应。
    #[test]
    fn every_declared_scheme_resolves() {
        for (key, _, _) in TERMINAL_SCHEMES {
            if *key == SCHEME_FOLLOW_THEME || *key == SCHEME_CUSTOM {
                continue;
            }
            assert!(
                TerminalPalette::preset(key).is_some(),
                "配色方案 {key} 在下拉中声明但无实现"
            );
        }
    }

    /// 平台默认字体必须在候选列表里，否则下拉的选中项会是空。
    #[test]
    fn default_font_is_a_candidate() {
        let f = default_terminal_font();
        assert!(
            terminal_font_candidates().contains(&f.as_str()),
            "默认字体 {f} 不在候选中"
        );
    }

    /// 编码回退：未知 key 到 UTF-8（不乱猜编码）。
    #[test]
    fn unknown_encoding_falls_back_to_utf8() {
        assert!(is_utf8_encoding("utf-8"));
        assert!(is_utf8_encoding("UTF-8"));
        assert!(!is_utf8_encoding("gbk"));
        assert_eq!(encoding_for("nope"), encoding_rs::UTF_8);
        assert_eq!(encoding_for("gbk"), encoding_rs::GBK);
    }
}
