//! 设置存储：三层覆盖中的 Defaults + User 层，JSON 原子写入。

use crate::theme::{default_terminal_font, TerminalPalette, ThemeMode, SCHEME_FOLLOW_THEME};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// 自定义终端配色：16 色 + 基础三色，颜色为 u32 0xRRGGBB。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CustomPalette {
    pub background: u32,
    pub foreground: u32,
    pub cursor: u32,
    pub normal: [u32; 8],
    pub bright: [u32; 8],
}

impl CustomPalette {
    /// 以给定基底调色板为初值（「恢复默认」/首次切换到 custom 时用）。
    pub fn from_palette(p: &TerminalPalette) -> Self {
        Self {
            background: p.background,
            foreground: p.foreground,
            cursor: p.cursor,
            normal: p.normal,
            bright: p.bright,
        }
    }

    pub fn to_palette(&self) -> TerminalPalette {
        TerminalPalette {
            background: self.background,
            foreground: self.foreground,
            cursor: self.cursor,
            normal: self.normal,
            bright: self.bright,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub theme_mode: ThemeMode,
    pub terminal_font_family: String,
    pub terminal_font_size: f32,
    /// 终端行高倍数：`1.0` = 紧贴，`1.2` = 默认，上限 2.0。
    ///
    /// 2026-09-28 补：此前这个值**硬编码在适配层**
    /// （`ui/src/terminal_view.rs` 的 `line_height_multiplier: 1.2`），
    /// 字号只改大小、不改疏密 —— 用户把字号调到 14 仍觉得「挤」却无处可调。
    /// 行高是终端可读性的第一旋钮（中文用户 / 高分屏 / 长时阅读差异极大），
    /// 引擎侧本来就支持（`terminal::TerminalConfig::line_height_multiplier`），
    /// 缺的只是把它接到设置上。
    ///
    /// 越界值由 [`Settings::clamped_line_height`] 钳制（`settings.json` 可手改）。
    pub terminal_line_height: f32,
    pub scrollback: usize,
    /// 本机采样间隔 ms（默认 1s，失焦时降频）。
    pub sample_interval_ms: u64,
    /// 界面语言（"zh" / "en"）。
    pub language: String,
    /// SSH 连接超时（秒）：握手 / 认证 / 建链各阶段的上限。
    pub connect_timeout_secs: u64,
    /// 会话意外断开（非用户主动关闭）后自动重连
    pub auto_reconnect: bool,
    /// 自动重连的最大尝试次数（达到后不再重试，等待用户手动连接）
    pub reconnect_max_retries: u32,
    /// 自动重连的**首个**重试间隔（秒）：第 n 次重试等 `interval × 2^(n-1)`，
    /// 封顶 `reconnect::BACKOFF_CAP_SECS`。越界值由 [`crate::reconnect::backoff_secs`]
    /// 统一钳制（`settings.json` 是用户可以手改的：写 0 会变成忙等重连）。
    ///
    /// 只做全局、不做连接级：等多久是本机网络条件的产物，不是「这条连接」的属性
    /// —— 与 `reconnect_max_retries` 同一条口径，见 `ConnectionProfile::auto_reconnect`。
    pub reconnect_interval_secs: u32,
    /// 侧栏**统一兜底**宽度 px：某个面板没单独记过宽度时用它。
    ///
    /// 0.2.0 及以前只有这一个字段 —— 六个面板共用同一个宽度。保留它作兜底，
    /// 升级用户原来的宽度就不会丢。
    pub sidebar_width: f32,
    /// 左侧栏**各面板各自的**宽度 px，键为 `ui::PanelKind::key()`。
    ///
    /// 2026-09-20 用户要求「默认记住每个侧边栏的宽度，每个侧边栏的宽度可以不同」：
    /// 键故意取**与界面语言无关**的标识（`"connections"` 而不是 `t("连接")` 的结果），
    /// 否则切一次语言，用户拖出来的宽度就查不回来了。
    ///
    /// `BTreeMap` 而非 `HashMap`：序列化顺序稳定 ⇒ `settings.json` 的 diff 可读、
    /// 测试能逐字比对。
    pub sidebar_widths: std::collections::BTreeMap<String, f32>,
    /// AI 对话面板（终端右侧）宽度 px：拖拽分界线时写回，重启恢复。
    pub ai_chat_width: f32,
    /// AI 对话面板是否展开。
    ///
    /// 2026-09-18 运行反馈「点击 AI 助手，聊天界面显示在终端右侧」：对话正文从
    /// 左侧栏搬到终端右侧的独立卡片后，它有了自己的开合状态 —— 左侧栏那一列只
    /// 负责「聊天记录」。落盘是为了重启后回到用户离开时的样子（关掉过就不再弹）。
    pub ai_chat_open: bool,
    /// 终端字符编码 key：见`theme::TERMINAL_ENCODINGS`，保存后对新终端生效。
    pub terminal_encoding: String,
    /// 终端配色方案 key：见`theme::TERMINAL_SCHEMES`。
    pub terminal_color_scheme: String,
    /// 自定义配色：仅`terminal_color_scheme == "custom"`时消费，为`None`时回退「跟随主题」。
    pub terminal_custom_palette: Option<CustomPalette>,
    /// 快捷命令栏是否展开：`false`= 40px 折叠胶囊条，`true`= 280px 展开分组列表。
    pub quick_bar_expanded: bool,
    /// 光标闪烁总开关。关闭后光标常亮（不启动闪烁定时器）。
    /// 注意这是**用户侧**开关，终端还能用 DECSCUSR / `ESC[?12h|l` 单独要求，
    /// 两者取「与」。
    ///
    /// 2026-09-17 起设置面板不再为它（与 `terminal_cursor_blink_ms`）提供控件：
    /// 这两个值只从这里的默认值与 `settings.json` 来，面板不再摆没人调的旋钮。
    pub terminal_cursor_blink: bool,
    /// 光标闪烁半周期（毫秒）。相位每半周期翻转一次，故完整明暗循环是它的两倍。
    /// 同上：仅配置文件可改，面板无控件。
    pub terminal_cursor_blink_ms: u64,
    /// 光标形状：块状 / 竖线 / 下划线。
    ///
    /// 2026-09-28 补：此前**完全没有这个设置**，用户对光标形状零控制权
    /// （终端程序能用 DECSCUSR 单方面要求，但用户改不了）。光标是终端里
    /// 用户盯着最久的像素，形状是纯个人习惯，属于终端软件标配。
    ///
    /// 这是**用户侧默认值**：远端程序用 DECSCUSR（`ESC[2 q` 等）时仍会临时覆盖，
    /// 与 `terminal_cursor_blink` 同一条口径（用户设置是底色，程序请求是前景）。
    pub terminal_cursor_style: CursorStyle,
    /// 多行粘贴行为（把原先两个语义交叉的开关收成一个三态，见 [`PasteBehavior`]）。
    ///
    /// 2026-09-28 收敛：原先是 `terminal_paste_normalize_newlines`（换行转回车，
    /// 后果是**逐行执行**）与 `terminal_paste_protect` + `terminal_paste_protect_lines`
    /// （弹确认框）两组独立开关，分属两个设置卡片、交互关系无处说明。用户很容易
    /// 「开了保护、又勾了换行转回车」，以为安全，实际是把确认框和逐行执行配成一对
    /// —— 一次误粘贴可能是 `rm -rf` 级别的后果。
    ///
    /// 旧字段迁移见 [`Settings::migrate_legacy_paste_flags`]（读盘时调用）。
    pub terminal_paste_behavior: PasteBehavior,
    /// 记录会话日志（F-7）：把终端输出旁路落盘到
    /// `<config>/rustshell/session-logs/<日期>/<时间>-<会话名>.log`。
    ///
    /// 这是**全局默认**；每条连接另有 `session_log: Option<bool>` 可覆盖
    /// （三态：跟全局 / 强制开 / 强制关）。优先级收口在
    /// [`crate::session_log::enabled_for`]。
    ///
    /// 默认 **false**：终端输出里常有口令、密钥、`export TOKEN=...`，
    /// 默认落盘等于默认留档，必须由用户显式打开。
    pub session_log_enabled: bool,
    /// 下载完成后校验（F-13）：单文件下载除了恒做的大小比对，再做一遍 SHA-256。
    ///
    /// 默认 **false**：校验要把远端文件**再读一遍**（不走 exec，见
    /// `sftp::hash_remote` 的取舍），大文件等于多花一倍读取时间。这是用户
    /// 按需打开的保险，不是默认负担。
    pub sftp_verify_downloads: bool,
    /// SFTP 并发传输数上限（F-13），区间 [1, 4]。
    ///
    /// **新建连接时生效**：信号量在 SFTP 服务任务建立时创建，改这一个值不会
    /// 影响已经连着的那条会话（设置面板的就地提示里写明了这一点）。
    pub sftp_max_concurrent: u32,
    /// 启动时遇到会话布局快照（F-9）怎么办。
    ///
    /// **2026-09-29 起本字段只作兼容保留，不再驱动任何启动行为**：用户要求
    /// 「取消恢复上次会话功能」，启动阶段一律不自动恢复、也不自动弹框 —— 恢复
    /// 改由命令面板「恢复上次会话」手动触发。字段与三档取值继续留在
    /// `settings.json` 里以免老配置读入报错（serde 兼容），但消费方已全部撤掉。
    /// 保留 `SessionRestoreMode` 的三态定义也是同一个理由：删了会让老配置文件
    /// 反序列化失败。
    pub session_restore: SessionRestoreMode,
    /// 全局代理（F-10）：`None` = 不用代理（老 `settings.json` 读进来就是这个姿态）。
    ///
    /// 与连接级的关系是「全局默认 + 三态覆盖」：每条连接的
    /// [`crate::connections::ConnectionProfile::proxy`] 可选跟随这里 / 明确直连 /
    /// 自带一套，合成只发生在 `ssh::connect_only` 一处。
    ///
    /// 口令只有 `password_ref`（DPAPI 密文 / keyring 引用）会落盘；运行期明文由
    /// 宿主解出（[`Settings::runtime_proxy`]）。
    pub proxy: Option<crate::proxy::ProxyConfig>,
    /// 启动动作：程序起来之后**替用户做什么**。
    ///
    /// 2026-09-28 补：此前硬编码为「打开命令面板」
    /// （`shell/src/app.rs` 的 `ui_call_request_command_palette`，并有回归测试
    /// `startup_opens_the_command_palette_instead_of_a_local_shell` 钉住）。
    /// 当时的运行反馈是「打开软件默认不打开本地 shell」—— 但把它**改成另一个
    /// 硬编码**（永远只开命令面板）等于用一个人的偏好覆盖所有人：power user
    /// 打开终端软件的第一诉求就是「给我一个 shell」，启动却总是先弹一个菜单。
    ///
    /// 默认取 [`StartupAction::LocalShell`]：一个**终端软件**启动后给一个终端，
    /// 是最不需要解释的行为；想要命令面板/恢复会话的人在设置里改一次即可
    /// （与 `session_restore` 同一条口径：改默认必须由用户按下）。
    pub startup_action: StartupAction,
}

/// 光标形状（用户侧默认值）。
///
/// 落盘 key 用稳定英文串（与界面语言解耦，同 `SessionRestoreMode::key` 的理由）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CursorStyle {
    /// 块状（终端传统默认）
    #[default]
    Block,
    /// 竖线
    Bar,
    /// 下划线
    Underline,
}

impl CursorStyle {
    /// 面板下拉的全部档位（顺序即展示顺序）。
    pub const ALL: [Self; 3] = [Self::Block, Self::Bar, Self::Underline];

    pub fn key(self) -> &'static str {
        match self {
            Self::Block => "block",
            Self::Bar => "bar",
            Self::Underline => "underline",
        }
    }

    /// (中文名, 英文名)
    pub fn labels(self) -> (&'static str, &'static str) {
        match self {
            Self::Block => ("块状", "Block"),
            Self::Bar => ("竖线", "Bar"),
            Self::Underline => ("下划线", "Underline"),
        }
    }

    /// 从 key 还原；认不出的一律当默认（手改坏设置不该让程序起不来）。
    pub fn from_key(key: &str) -> Self {
        match key {
            "bar" => Self::Bar,
            "underline" => Self::Underline,
            _ => Self::Block,
        }
    }
}

/// 多行粘贴行为：把原先两个语义交叉的开关收成一个三态。
///
/// 原设计的两个开关是 `terminal_paste_normalize_newlines`（换行转回车）与
/// `terminal_paste_protect`（弹确认框）。问题在于「换行转回车」的**后果**就是
/// 「逐行执行」，而这正是「粘贴保护」想拦的危险动作 —— 两个开关没有一处向用户
/// 解释它们的交互，用户很容易配出「确认框 + 逐行执行」这种自相矛盾又危险的组合。
///
/// 三态之后，「要不要逐行执行」与「要不要先问一句」变成**同一个问题的一次回答**，
/// 结构上不可能再配出矛盾组合。
///
/// 落盘用 **kebab-case**（`confirm-multi-line` / `always-confirm` / …），
/// 与 `PasteBehavior::key()` 的返回值逐字符相同：`Settings` 以
/// `serde_json::to_value` 落盘、UI 下拉以 `key()` 取值/回填，两边若不同源就会出现
/// 「下拉显示中文、存进 JSON 却是默认档」的静默不一致。
/// （不要用 `rename_all = "snake_case"`：它生成 `confirm_multi_line`，
/// 与 `key()` 的连字符形态对不上。）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PasteBehavior {
    /// 逐行执行，但**含换行时先确认**（默认）。
    ///
    /// 保留终端原本的语义（回车才是执行），同时用一次确认兜住「我不知道自己粘了什么」。
    /// 单行粘贴不打扰（无换行 ⇒ 不弹框）。
    #[default]
    ConfirmMultiLine,
    /// 一律先确认（含单行）。
    ///
    /// 最保守的一档：连单行粘贴也要问一次。适合剪贴板内容来源完全不可信的场景。
    AlwaysConfirm,
    /// 逐行执行，不确认（老行为，危险）。
    ///
    /// 粘贴即逐行执行，一次误粘贴可直接跑掉 `rm -rf`。默认值**绝不**取它。
    ExecuteDirectly,
    /// 原样粘贴，不逐行执行。
    ///
    /// 保留原始字节（`\n` 只换行不回列），交给远端自己解释 —— 少数需要保留
    /// 原始字节的场景用它（远端有 `bracketed paste` 时尤其正确）。不弹确认。
    Raw,
}

impl PasteBehavior {
    /// 面板下拉的全部档位（顺序即展示顺序：从最安全到最宽松）。
    pub const ALL: [Self; 4] = [
        Self::ConfirmMultiLine,
        Self::AlwaysConfirm,
        Self::ExecuteDirectly,
        Self::Raw,
    ];

    pub fn key(self) -> &'static str {
        match self {
            Self::ConfirmMultiLine => "confirm-multi-line",
            Self::AlwaysConfirm => "always-confirm",
            Self::ExecuteDirectly => "execute-directly",
            Self::Raw => "raw",
        }
    }

    pub fn from_key(key: &str) -> Self {
        match key {
            "always-confirm" => Self::AlwaysConfirm,
            "execute-directly" => Self::ExecuteDirectly,
            "raw" => Self::Raw,
            _ => Self::ConfirmMultiLine,
        }
    }

    /// 各档的展示名（中文，英文名见设置面板的 `tp` 对）。
    pub fn labels(self) -> (&'static str, &'static str) {
        match self {
            Self::ConfirmMultiLine => ("多行时先确认", "Confirm on multi-line"),
            Self::AlwaysConfirm => ("总是先确认", "Always confirm"),
            Self::ExecuteDirectly => ("直接执行（不推荐）", "Execute directly"),
            Self::Raw => ("原样粘贴", "Paste raw"),
        }
    }

    /// 引擎要的「粘贴时是否把行尾统一成回车」。
    /// `Raw` 档原样透传，其余三档都归一为回车（逐行执行语义）。
    pub fn normalize_newlines(self) -> bool {
        !matches!(self, Self::Raw)
    }

    /// 引擎要的「粘贴确认阈值」（`None` = 不确认）。
    ///
    /// 阈值口径与引擎一致：行数 = 换行数 + 1，**超过**阈值才问。
    /// - `ConfirmMultiLine` ⇒ `Some(1)`：单行（1 行）不问，任何换行都问；
    /// - `AlwaysConfirm` ⇒ `Some(0)`：一律问；
    /// - `ExecuteDirectly` / `Raw` ⇒ `None`：不问。
    pub fn protect_lines(self) -> Option<u32> {
        match self {
            Self::ConfirmMultiLine => Some(1),
            Self::AlwaysConfirm => Some(0),
            Self::ExecuteDirectly | Self::Raw => None,
        }
    }
}

/// 启动动作（见 [`Settings::startup_action`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StartupAction {
    /// 启动即开一个本地 Shell（默认）
    #[default]
    LocalShell,
    /// 启动打开命令面板
    CommandPalette,
    /// 启动尝试恢复上次的会话布局（F-9；无快照时退化为开本地 Shell）
    RestoreSession,
    /// 启动什么都不做，只给一个空窗口
    Nothing,
}

impl StartupAction {
    /// 面板下拉的全部档位。
    ///
    /// `panel_options()` 是给面板用的**子集**（不含 `RestoreSession`，理由见该函数）；
    /// 这个 `ALL` 覆盖全部四档，供测试与配置文件校验使用。
    pub const ALL: [Self; 4] = [
        Self::LocalShell,
        Self::CommandPalette,
        Self::RestoreSession,
        Self::Nothing,
    ];

    pub fn key(self) -> &'static str {
        match self {
            Self::LocalShell => "local-shell",
            Self::CommandPalette => "command-palette",
            Self::RestoreSession => "restore-session",
            Self::Nothing => "nothing",
        }
    }

    pub fn from_key(key: &str) -> Self {
        match key {
            "command-palette" => Self::CommandPalette,
            "restore-session" => Self::RestoreSession,
            "nothing" => Self::Nothing,
            _ => Self::LocalShell,
        }
    }

    /// 三态之外的第四档（`RestoreSession`）不在面板下拉里：它与
    /// [`Settings::session_restore`] 那套「要不要恢复、恢复时问不问」是同一件事的
    /// 两个入口，摆在一起会让人以为要配两次。面板只给三档，`restore-session`
    /// 只能手改 JSON（且必须同时把 `session_restore` 调成非 `never`）。
    pub fn panel_options() -> [Self; 3] {
        [Self::LocalShell, Self::CommandPalette, Self::Nothing]
    }

    pub fn labels(self) -> (&'static str, &'static str) {
        match self {
            Self::LocalShell => ("打开本地终端", "Open a local shell"),
            Self::CommandPalette => ("打开命令面板", "Open command palette"),
            Self::RestoreSession => ("恢复上次会话", "Restore last session"),
            Self::Nothing => ("什么也不做", "Do nothing"),
        }
    }
}

/// 启动时会话恢复的口径（F-9）。
///
/// **2026-09-29 起已成为兼容存根**：用户要求「取消恢复上次会话功能」后，启动
/// 阶段不再读这个值做任何决策（三档行为完全一致 —— 都是一动不动）。类型本身
/// 保留，只为了让老 `settings.json` 里的 `"session_restore": "always"` 之类
/// 仍能反序列化成功；删掉会让配置读入直接失败。
///
/// 历史设计（留着供理解三态的来历）：三态而不是布尔，因为布尔只能表达
/// 「问不问」，答不上「问完之后照哪个办」—— 而「不再问」必须能同时记住两个
/// 相反的回答（总是恢复 / 从不恢复）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionRestoreMode {
    /// 有快照就先问（默认）
    #[default]
    Ask,
    /// 有快照直接恢复，不问
    Always,
    /// 有快照也不恢复，不问
    Never,
}

impl SessionRestoreMode {
    /// 落盘用的稳定 key（与界面语言无关，见 `Settings::sidebar_widths` 的同一条理由）。
    pub fn key(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::Always => "always",
            Self::Never => "never",
        }
    }

    /// 从 key 还原；认不出的一律当默认（手改坏设置不该让程序起不来）。
    pub fn from_key(key: &str) -> Self {
        match key {
            "always" => Self::Always,
            "never" => Self::Never,
            _ => Self::Ask,
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme_mode: ThemeMode::Dark,
            terminal_font_family: default_terminal_font(),
            // 2026-09-18 用户要求「终端默认字体大小为 12 号」：与界面文字（见
            // `ui/src/theme.rs` 的全局字号）同一档，终端不再比 UI 大一档。
            terminal_font_size: 12.0,
            terminal_line_height: DEFAULT_TERMINAL_LINE_HEIGHT,
            scrollback: 10_000,
            sample_interval_ms: 1000,
            language: "zh".to_string(),
            connect_timeout_secs: 20,
            auto_reconnect: false,
            reconnect_max_retries: 3,
            reconnect_interval_secs: crate::reconnect::DEFAULT_INTERVAL_SECS,
            sidebar_width: 240.0,
            sidebar_widths: std::collections::BTreeMap::new(),
            ai_chat_width: DEFAULT_AI_CHAT_WIDTH,
            ai_chat_open: false,
            terminal_encoding: "utf-8".to_string(),
            terminal_color_scheme: SCHEME_FOLLOW_THEME.to_string(),
            terminal_custom_palette: None,
            quick_bar_expanded: true,
            terminal_cursor_blink: true,
            terminal_cursor_blink_ms: DEFAULT_CURSOR_BLINK_MS,
            terminal_cursor_style: CursorStyle::default(),
            terminal_paste_behavior: PasteBehavior::default(),
            session_log_enabled: false,
            sftp_verify_downloads: false,
            sftp_max_concurrent: DEFAULT_SFTP_MAX_CONCURRENT,
            session_restore: SessionRestoreMode::default(),
            proxy: None,
            startup_action: StartupAction::default(),
        }
    }
}

/// 粘贴保护阈值的默认值与区间（F-5）。
///
/// 默认 **1**：单行不打扰，任何换行都确认 —— 含「单行 + 结尾换行」，
/// 那一个回车就是「执行」，正是最该拦的一刀。
pub const DEFAULT_PASTE_PROTECT_LINES: u32 = 1;
pub const PASTE_PROTECT_LINES_MIN: u32 = 0;
/// 上限 100：再往上等于关掉闸。
///
/// 2026-09-28 起面板不再暴露阈值控件（改用 [`PasteBehavior`] 四档），这两个常量
/// 只服务**旧配置迁移**（把老的 `terminal_paste_protect_lines` 读进来时不 panic）。
pub const PASTE_PROTECT_LINES_MAX: u32 = 100;

/// 终端行高倍数的默认值与区间。
///
/// 默认 **1.2**：与终端区域前身（适配层硬编码）逐像素一致，升级不改观感。
/// 下限 1.0（再小会切字形），上限 2.0（再大一行只剩空白）。
pub const DEFAULT_TERMINAL_LINE_HEIGHT: f32 = 1.2;
pub const TERMINAL_LINE_HEIGHT_MIN: f32 = 1.0;
pub const TERMINAL_LINE_HEIGHT_MAX: f32 = 2.0;

/// SFTP 并发传输数的默认值（F-13）。
///
/// 默认 **1** = 与历史行为一致（一条传输跑完再跑下一条）：并发是用户显式
/// 选择的加速手段，不该靠升级悄悄改变传输顺序与带宽占用。
pub const DEFAULT_SFTP_MAX_CONCURRENT: u32 = 1;
pub const SFTP_MAX_CONCURRENT_MIN: u32 = 1;
pub const SFTP_MAX_CONCURRENT_MAX: u32 = 4;

/// 侧栏宽度的合法区间；拖动与加载都按此钳制。
///
/// 不按面板分档：`PanelKind::Ai` 自 2026-09-18 起放的是**聊天记录列表**（窄列），
/// 与其余面板同档即可 —— 原先那条 `SIDEBAR_WIDTH_MIN_AI = 300.0` 已无生产调用点
/// （仅 `clamped_sidebar_width(true)` 分支引用，而该分支从没被调用过），本次一并删除。
pub const SIDEBAR_WIDTH_MIN: f32 = 180.0;
pub const SIDEBAR_WIDTH_MAX: f32 = 520.0;

/// AI 对话面板（终端右侧）的默认宽度与合法区间；拖动与加载都按此钳制。
///
/// 下限比侧栏高：面板里是消息流 + 输入条 + 命令卡片，窄于 320px 时中文正文一行
/// 只剩十几个字，读起来比滚动更累。
pub const DEFAULT_AI_CHAT_WIDTH: f32 = 400.0;
pub const AI_CHAT_WIDTH_MIN: f32 = 320.0;
pub const AI_CHAT_WIDTH_MAX: f32 = 720.0;

/// 光标闪烁半周期（毫秒）默认值。
/// 530ms 与 xterm / VTE 一致。
///
/// 2026-09-17 用户要求「移除设置中光标闪烁、闪烁半周期配置，改为默认配置 json 中的
/// 默认设置」：面板上的两个控件撤下后，**这里**（以及可手改的 `settings.json`）
/// 就是它们的唯一来源，故只留默认值常量；原先给面板用的 `CURSOR_BLINK_MS_MIN/MAX`
/// 区间常量随之删除 —— 留着就是「没人消费的配置」。
/// 越界保护仍在消费端：终端引擎在启用闪烁时把半周期钳到 50 到 5000ms
/// （见 `rustshell-terminal/src/view.rs`），手改 JSON 成 0 也不会变成满负载重绘。
pub const DEFAULT_CURSOR_BLINK_MS: u64 = 530;

impl Settings {
    /// 解析当前应生效的终端配色，按 custom 到 预设 到 跟随主题 的优先级回退，不 panic。
    pub fn resolve_terminal_palette(&self) -> TerminalPalette {
        let key = self.terminal_color_scheme.as_str();
        if key == crate::theme::SCHEME_CUSTOM {
            if let Some(cp) = &self.terminal_custom_palette {
                return cp.to_palette();
            }
            return TerminalPalette::resolved(self.theme_mode);
        }
        TerminalPalette::preset(key).unwrap_or_else(|| TerminalPalette::resolved(self.theme_mode))
    }

    /// 某个左侧栏面板该用的宽度（钳制到合法区间）。
    ///
    /// 取值口径：**先查该面板自己记的宽度**（`sidebar_widths`），没记过才回退到
    /// 统一兜底 `sidebar_width`。两边都过一遍 `clamp` —— `settings.json` 是用户
    /// 可以手改的，写成 0 会把侧栏压没、写成 9999 会把终端挤没。
    pub fn sidebar_width_for(&self, panel: &str) -> f32 {
        self.sidebar_widths
            .get(panel)
            .copied()
            .unwrap_or(self.sidebar_width)
            .clamp(SIDEBAR_WIDTH_MIN, SIDEBAR_WIDTH_MAX)
    }

    /// 记住某个左侧栏面板的宽度（越界值先钳制再存，手改 JSON 也写不出离谱值）。
    ///
    /// 只写**该面板**那一项：别的面板的宽度不受影响 —— 这正是用户要的
    /// 「每个侧边栏的宽度可以不同」。统一兜底 `sidebar_width` 不跟着改，
    /// 免得「拖过一次连接栏」把还没访问过的面板也一起搬走。
    pub fn remember_sidebar_width(&mut self, panel: &str, width: f32) {
        let w = width.clamp(SIDEBAR_WIDTH_MIN, SIDEBAR_WIDTH_MAX);
        self.sidebar_widths.insert(panel.to_string(), w);
    }

    /// AI 对话面板宽度（钳制到合法区间；手改 settings.json 也不会把终端挤没）
    pub fn clamped_ai_chat_width(&self) -> f32 {
        self.ai_chat_width
            .clamp(AI_CHAT_WIDTH_MIN, AI_CHAT_WIDTH_MAX)
    }

    /// 终端行高倍数（钳制到合法区间；手改 settings.json 也不会把字形切掉或撑出大片空白）
    pub fn clamped_line_height(&self) -> f32 {
        if !self.terminal_line_height.is_finite() {
            return DEFAULT_TERMINAL_LINE_HEIGHT;
        }
        self.terminal_line_height
            .clamp(TERMINAL_LINE_HEIGHT_MIN, TERMINAL_LINE_HEIGHT_MAX)
    }

    /// SFTP 并发传输数（钳制到合法区间；手改 settings.json 也拉不出 0 或 99）
    pub fn clamped_sftp_max_concurrent(&self) -> u32 {
        self.sftp_max_concurrent
            .clamp(SFTP_MAX_CONCURRENT_MIN, SFTP_MAX_CONCURRENT_MAX)
    }

    /// 全局代理的**运行期形态**（F-10）：把 `password_ref` 交给宿主注入的解密器，
    /// 解出明文口令。
    ///
    /// 解密器由宿主给（`rustshell_platform::secret::unprotect`）—— core 不依赖
    /// platform，与 `ConnectionStore::restore_passwords` 同一个手法。
    ///
    /// **每次建连现算**而不是在启动时解一次存起来：用户可能在设置面板里改了口令，
    /// 缓存一份就等于让「改了设置、新连接还是旧口令」这种故障有了藏身处。
    pub fn runtime_proxy(
        &self,
        unprotect: impl Fn(&str) -> Option<String>,
    ) -> Option<crate::proxy::ProxyConfig> {
        self.proxy.as_ref().map(|p| p.with_plaintext(unprotect))
    }

    pub fn load(path: &PathBuf) -> Self {
        match std::fs::read_to_string(path) {
            Ok(s) => Self::load_from_optional_str(&s, || {
                // 解析失败先留底坏文件，再回退默认
                crate::configio::backup_broken_config(path, &s);
            }),
            Err(_) => Self::default(),
        }
    }

    /// 从 JSON 文本解析（带旧字段迁移）；文本非法时调 `on_broken` 留底后回退默认。
    ///
    /// 抽出来是为了让「旧配置迁移」这条升级路径**能被单测直接覆盖** ——
    /// 走 [`Settings::load`] 的话，测试得先往磁盘写文件（含临时目录、
    /// 清理、Windows 文件占用等噪声），而迁移逻辑本身与磁盘无关。
    fn load_from_optional_str(raw: &str, on_broken: impl FnOnce()) -> Self {
        match serde_json::from_str::<Settings>(raw) {
            Ok(mut v) => {
                // 旧版把「粘贴行为」拆成三个字段，语义交叉且能配出危险组合。
                // 迁移只在**读盘**这一处发生，读完之后运行期只有
                // `terminal_paste_behavior` 一个真值来源。
                v.migrate_legacy_paste_flags(raw);
                v
            }
            Err(_) => {
                on_broken();
                Self::default()
            }
        }
    }

    /// 测试用便捷入口：非法文本静默回退默认（不留底）。
    #[cfg(test)]
    fn load_from_str(raw: &str) -> Self {
        Self::load_from_optional_str(raw, || {})
    }

    /// 旧 `settings.json` 的粘贴三字段 → [`PasteBehavior`] 的迁移。
    ///
    /// 判据是**原始 JSON 文本里有没有旧字段**（`s` 是读进来的原文），而不是
    /// 反序列化后的值：新格式的 `terminal_paste_behavior` 有 `serde(default)`，
    /// 旧文件里没有它，于是「有没有旧字段」必须回到文本上问，否则无法区分
    /// 「老用户没配过粘贴」与「新用户显式选了默认档」。
    ///
    /// 迁移表（旧 normalize / 旧 protect / 旧阈值 → 新档位）：
    ///
    /// | normalize | protect | lines | 新档位 |
    /// |---|---|---|---|
    /// | true | true | ≥2 | `ConfirmMultiLine`（收紧到「任何换行都问」是刻意的：安全优先） |
    /// | true | true | 0/1 | `ConfirmMultiLine` |
    /// | true | false | — | `ExecuteDirectly` |
    /// | false | any | — | `Raw` |
    ///
    /// 若用户已经写过新字段（`terminal_paste_behavior` 在原文里出现），
    /// **一律不动** —— 迁移不能覆盖用户的新选择。
    fn migrate_legacy_paste_flags(&mut self, raw: &str) {
        if raw.contains("terminal_paste_behavior") {
            return; // 新格式：不迁移
        }
        let has_legacy = raw.contains("terminal_paste_normalize_newlines")
            || raw.contains("terminal_paste_protect");
        if !has_legacy {
            // 老到连粘贴设置都没有的配置：保持默认档（多行时先确认）
            return;
        }
        let normalize = raw.contains("\"terminal_paste_normalize_newlines\": true")
            || raw.contains("\"terminal_paste_normalize_newlines\":true");
        let protect = raw.contains("\"terminal_paste_protect\": true")
            || raw.contains("\"terminal_paste_protect\":true");
        self.terminal_paste_behavior = match (normalize, protect) {
            (false, _) => PasteBehavior::Raw,
            (true, false) => PasteBehavior::ExecuteDirectly,
            (true, true) => PasteBehavior::ConfirmMultiLine,
        };
    }

    pub fn save(&self, path: &PathBuf) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let json = serde_json::to_string_pretty(self)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{SCHEME_CUSTOM, SCHEME_FOLLOW_THEME};

    /// 2026-09-18 用户要求「终端默认字体大小为 12 号」。
    ///
    /// 只守**默认值**（新装 / 重置后的取值）；已落盘的 `settings.json` 是用户
    /// 自己的选择，不在这里迁移 —— 设置面板那侧另有一条「重置值必须与默认值同源」
    /// 的对齐测试（`ui/src/main_view/settings_panel.rs`）。
    #[test]
    fn default_terminal_font_size_is_12() {
        assert_eq!(Settings::default().terminal_font_size, 12.0);
    }

    /// 旧 settings.json（无新字段）反序列化后补齐默认值。
    #[test]
    fn legacy_settings_gain_new_defaults() {
        // 模拟旧版本写出的 JSON（只有历史字段）
        let legacy = r#"{
            "theme_mode": "Dark",
            "terminal_font_family": "Consolas",
            "terminal_font_size": 14.0,
            "scrollback": 5000,
            "sample_interval_ms": 1000,
            "language": "zh",
            "connect_timeout_secs": 20,
            "auto_reconnect": false,
            "reconnect_max_retries": 3
        }"#;
        let s: Settings = serde_json::from_str(legacy).expect("旧配置必须能反序列化");
        assert_eq!(s.terminal_font_family, "Consolas");
        assert_eq!(s.sidebar_width, 240.0);
        assert!(
            s.sidebar_widths.is_empty(),
            "旧配置没有 `sidebar_widths`：应为空表，各面板统一走 `sidebar_width` 兜底"
        );
        assert_eq!(s.terminal_encoding, "utf-8");
        assert_eq!(s.terminal_color_scheme, SCHEME_FOLLOW_THEME);
        assert!(s.terminal_custom_palette.is_none());
        // 快捷命令栏默认展开
        assert!(s.quick_bar_expanded);
        // 旧配置没有代理（F-10）：默认「不用代理」，不是「配了一套空的」
        assert!(s.proxy.is_none());
        // 旧配置没有重连间隔（F-4）：取默认值，而不是 0（0 会变成忙等重连）
        assert_eq!(
            s.reconnect_interval_secs,
            crate::reconnect::DEFAULT_INTERVAL_SECS
        );
        // 行为等同 follow-theme
        assert_eq!(s.resolve_terminal_palette(), TerminalPalette::dark());
    }

    /// 全局代理（F-10）：口令引用解成运行期明文；解不开时留空而不是整条丢掉。
    #[test]
    fn runtime_proxy_decrypts_the_ref_and_keeps_the_route() {
        let mut s = Settings::default();
        assert!(s.runtime_proxy(|_| Some("x".into())).is_none());

        s.proxy = Some(crate::proxy::ProxyConfig {
            host: "proxy.lan".into(),
            port: 3128,
            username: Some("u".into()),
            password_ref: Some("cipher:G".into()),
            ..Default::default()
        });
        let rt = s
            .runtime_proxy(|blob| (blob == "cipher:G").then(|| "pw".to_string()))
            .expect("配了代理就该有运行期形态");
        assert_eq!(rt.password.as_deref(), Some("pw"));
        assert_eq!(rt.host, "proxy.lan");

        // 解不开（换机器 / 凭据库不可用）：地址与用户名留着，口令为空 ——
        // 让代理去拒绝，比假装「没配代理」而**静默直连**诚实
        let broken = s.runtime_proxy(|_| None).expect("配置本体仍应返回");
        assert_eq!(broken.password, None);
        assert_eq!(broken.username.as_deref(), Some("u"));
        // 解密不该改动设置本体（每次建连现算，不缓存）
        assert_eq!(s.proxy.as_ref().unwrap().password, None);
    }

    /// custom 但无自定义数据 到 回退跟随主题，不 panic。
    #[test]
    fn custom_without_palette_falls_back_to_theme() {
        // 默认主题是 Dark：custom 无数据时与 follow-theme 等价（都是 dark）
        let s = Settings {
            terminal_color_scheme: SCHEME_CUSTOM.to_string(),
            terminal_custom_palette: None,
            ..Default::default()
        };
        assert_eq!(
            s.resolve_terminal_palette(),
            TerminalPalette::resolved(s.theme_mode)
        );
        assert_eq!(s.resolve_terminal_palette(), TerminalPalette::dark());

        // 主题设为 Light 时必须跟着走浅色，而不是钉死深色
        let s = Settings {
            theme_mode: ThemeMode::Light,
            terminal_color_scheme: SCHEME_CUSTOM.to_string(),
            terminal_custom_palette: None,
            ..Default::default()
        };
        assert_eq!(s.resolve_terminal_palette(), TerminalPalette::light());
    }

    /// 预设生效 + 自定义优先于预设键。
    #[test]
    fn preset_and_custom_resolution() {
        let s = Settings {
            terminal_color_scheme: "dracula".to_string(),
            ..Default::default()
        };
        assert_eq!(
            s.resolve_terminal_palette(),
            TerminalPalette::preset("dracula").unwrap()
        );

        let s = Settings {
            terminal_color_scheme: SCHEME_CUSTOM.to_string(),
            terminal_custom_palette: Some(CustomPalette {
                background: 0x010203,
                foreground: 0x040506,
                cursor: 0x070809,
                normal: [0; 8],
                bright: [0; 8],
            }),
            ..Default::default()
        };
        let p = s.resolve_terminal_palette();
        assert_eq!(p.background, 0x010203);
    }

    /// 未知配色 key 回退跟随主题。
    #[test]
    fn unknown_scheme_falls_back_to_theme() {
        let s = Settings {
            terminal_color_scheme: "no-such-scheme".to_string(),
            ..Default::default()
        };
        assert_eq!(s.resolve_terminal_palette(), TerminalPalette::dark());
    }

    /// 侧栏宽度：**按面板分别记**，互不干扰，且都过钳制。
    ///
    /// 2026-09-20 用户要求「默认记住每个侧边栏的宽度，每个侧边栏的宽度可以不同」。
    #[test]
    fn sidebar_width_is_remembered_per_panel() {
        let mut s = Settings::default();

        // ① 没记过的面板 → 统一兜底（升级用户原来的宽度不会丢）
        assert_eq!(s.sidebar_width_for("connections"), 240.0);
        assert_eq!(s.sidebar_width_for("files"), 240.0);

        s.sidebar_width = 300.0;
        assert_eq!(
            s.sidebar_width_for("files"),
            300.0,
            "旧版只有一个 sidebar_width：升级后所有面板都要继承它"
        );

        // ② 两个面板记成不同宽度 ⇒ 各读各的
        s.remember_sidebar_width("connections", 200.0);
        s.remember_sidebar_width("files", 460.0);
        assert_eq!(s.sidebar_width_for("connections"), 200.0);
        assert_eq!(s.sidebar_width_for("files"), 460.0);

        // ③ 改一个不影响另一个，也不动兜底值
        s.remember_sidebar_width("connections", 260.0);
        assert_eq!(s.sidebar_width_for("connections"), 260.0);
        assert_eq!(s.sidebar_width_for("files"), 460.0);
        assert_eq!(
            s.sidebar_width, 300.0,
            "兜底值不得被拖拽搬走 —— 否则没访问过的面板会跟着跳"
        );

        // ④ 越界值在写入侧就钳掉（手改 JSON 写 0 / 9999 都写不出离谱宽度）
        s.remember_sidebar_width("tasks", 0.0);
        s.remember_sidebar_width("ai", 9999.0);
        assert_eq!(s.sidebar_width_for("tasks"), SIDEBAR_WIDTH_MIN);
        assert_eq!(s.sidebar_width_for("ai"), SIDEBAR_WIDTH_MAX);
        // 兜底值同样过钳制（旧配置里可能就是个越界值）
        let s = Settings {
            sidebar_width: 9999.0,
            ..Default::default()
        };
        assert_eq!(s.sidebar_width_for("dashboard"), SIDEBAR_WIDTH_MAX);
    }

    /// 各面板宽度必须真的落进 `settings.json` 并读得回来（重启后不白拖）。
    #[test]
    fn sidebar_widths_round_trip_through_json() {
        let mut s = Settings::default();
        s.remember_sidebar_width("connections", 210.0);
        s.remember_sidebar_width("tasks", 380.0);

        let json = serde_json::to_string_pretty(&s).expect("序列化");
        let back: Settings = serde_json::from_str(&json).expect("反序列化");
        assert_eq!(back.sidebar_width_for("connections"), 210.0);
        assert_eq!(back.sidebar_width_for("tasks"), 380.0);
        // 未记过的面板仍走兜底
        assert_eq!(back.sidebar_width_for("files"), 240.0);
    }

    /// AI 对话面板宽度钳制 + 默认收起。
    ///
    /// 面板宽度是可以手改 `settings.json` 的：写成 0 会把终端挤成 0 宽（整个
    /// 工作区看着「不见了」），写成 5000 同样如此。加载侧兜住，等价于「用户怎么
    /// 改都不会把界面弄坏」。
    #[test]
    fn ai_chat_width_clamped_and_collapsed_by_default() {
        let mut s = Settings::default();
        assert_eq!(s.ai_chat_width, DEFAULT_AI_CHAT_WIDTH);
        assert!(
            !s.ai_chat_open,
            "AI 对话面板默认必须收起 —— 未配置 AI 服务的用户不该一开软件就多出一列空面板"
        );

        s.ai_chat_width = 0.0;
        assert_eq!(s.clamped_ai_chat_width(), AI_CHAT_WIDTH_MIN);
        s.ai_chat_width = 9999.0;
        assert_eq!(s.clamped_ai_chat_width(), AI_CHAT_WIDTH_MAX);
        s.ai_chat_width = DEFAULT_AI_CHAT_WIDTH;
        assert_eq!(s.clamped_ai_chat_width(), DEFAULT_AI_CHAT_WIDTH);
    }

    /// 会话恢复默认「询问」，且三态 key 往返稳定。
    ///
    /// 默认必须是 `Ask` 而不是「直接恢复」：启动时凭一份文件替用户开一堆标签，
    /// 是升级就能改变的行为 —— 那类改动必须由用户按下按钮才算数。
    #[test]
    fn session_restore_defaults_to_ask_and_round_trips_its_key() {
        assert_eq!(Settings::default().session_restore, SessionRestoreMode::Ask);
        for m in [
            SessionRestoreMode::Ask,
            SessionRestoreMode::Always,
            SessionRestoreMode::Never,
        ] {
            assert_eq!(SessionRestoreMode::from_key(m.key()), m);
        }
        // 认不出的 key 落到默认（手改坏设置不该让程序起不来）
        assert_eq!(
            SessionRestoreMode::from_key("whatever"),
            SessionRestoreMode::Ask
        );
    }

    /// 旧 `settings.json`（没有 `session_restore` 字段）读进来必须是 `Ask`。
    #[test]
    fn legacy_settings_gain_ask_for_session_restore() {
        let legacy = r#"{"theme_mode":"Dark","language":"zh"}"#;
        let s: Settings = serde_json::from_str(legacy).expect("旧设置要能读");
        assert_eq!(s.session_restore, SessionRestoreMode::Ask);
    }

    /// 2026-10-20 用户要求「主题深色模式移除跟随系统」：旧的 `"System"` 必须还能读，
    /// 并落到深色。枚举少一个变体不能让整份 `Settings` 反序列化失败 → 回退默认，
    /// 那会把用户的语言、连接超时等其它配置一起清掉。
    #[test]
    fn legacy_system_theme_reads_as_dark() {
        let legacy = r#"{"theme_mode":"System","language":"en"}"#;
        let s = Settings::load_from_str(legacy);
        assert_eq!(s.theme_mode, ThemeMode::Dark);
        assert_eq!(s.language, "en", "主题字段的兼容不能连累其它设置");
    }

    /// 2026-09-28 新增三项终端外观设置的默认值。
    ///
    /// 行高默认必须与适配层原来的硬编码（1.2）一致 —— 升级不改观感，
    /// 否则老用户升一次版本终端行距会莫名变化。
    #[test]
    fn new_terminal_appearance_defaults() {
        let s = Settings::default();
        assert_eq!(s.terminal_line_height, 1.2, "行高默认必须等于原硬编码值");
        assert_eq!(s.terminal_cursor_style, CursorStyle::Block);
        assert_eq!(s.terminal_paste_behavior, PasteBehavior::ConfirmMultiLine);
        assert_eq!(s.startup_action, StartupAction::LocalShell);
    }

    /// 行高钳制：`settings.json` 是用户可手改的，NaN / 越界都不能把终端画坏。
    #[test]
    fn line_height_is_clamped() {
        // 被测的是 `clamped_line_height()` 这个纯函数，故意不走 `Settings::default()`
        // + 逐次赋值（那会触发 clippy 的 `field_reassign_with_default`，而且每个
        // 用例只关心一个字段）：这里只需要一个「行高 = x」的设置值。
        let at = |h: f32| Settings {
            terminal_line_height: h,
            ..Default::default()
        };
        assert_eq!(
            at(0.1).clamped_line_height(),
            TERMINAL_LINE_HEIGHT_MIN,
            "低于下限要抬到下限"
        );
        assert_eq!(
            at(99.0).clamped_line_height(),
            TERMINAL_LINE_HEIGHT_MAX,
            "高于上限要压到上限"
        );
        assert_eq!(
            at(f32::NAN).clamped_line_height(),
            DEFAULT_TERMINAL_LINE_HEIGHT,
            "NaN 必须回落到默认值，而不是让 clamp 传播出 NaN"
        );
        assert_eq!(at(1.5).clamped_line_height(), 1.5, "区间内原样返回");
    }

    /// `PasteBehavior` 四档到引擎两参数的换算（这是两个开关收成一个之后
    /// **唯一**的换算点，也是最容易配错的地方）。
    ///
    /// 重点守两件事：① `ConfirmMultiLine` 必须「单行不问、多行问」；
    /// ② 任何一档都不能同时给出「逐行执行 + 不确认」这种危险组合
    /// —— 那正是旧设计能配出来、本次要消灭的东西。
    #[test]
    fn paste_behavior_maps_to_engine_arguments() {
        // 多行时先确认：单行（1 行）不问，多行问 ⇒ 阈值 1
        assert!(PasteBehavior::ConfirmMultiLine.normalize_newlines());
        assert_eq!(PasteBehavior::ConfirmMultiLine.protect_lines(), Some(1));

        // 总是确认：阈值 0 ⇒ 连单行也问
        assert!(PasteBehavior::AlwaysConfirm.normalize_newlines());
        assert_eq!(PasteBehavior::AlwaysConfirm.protect_lines(), Some(0));

        // 直接执行：不问（危险档，但不再与"保护"开关矛盾 —— 用户是显式选的）
        assert!(PasteBehavior::ExecuteDirectly.normalize_newlines());
        assert_eq!(PasteBehavior::ExecuteDirectly.protect_lines(), None);

        // 原样粘贴：不归一、不问
        assert!(!PasteBehavior::Raw.normalize_newlines());
        assert_eq!(PasteBehavior::Raw.protect_lines(), None);

        // 危险组合「逐行执行且不确认」只由 ExecuteDirectly 一档表达，
        // 且它不可能是默认值。
        assert_ne!(PasteBehavior::default(), PasteBehavior::ExecuteDirectly);
    }

    /// `PasteBehavior` / `CursorStyle` / `StartupAction` 的 key 往返稳定，
    /// 且认不出的 key 落默认（手改坏配置不该让程序起不来）。
    #[test]
    fn enum_keys_round_trip_and_fall_back() {
        for b in [
            PasteBehavior::ConfirmMultiLine,
            PasteBehavior::AlwaysConfirm,
            PasteBehavior::ExecuteDirectly,
            PasteBehavior::Raw,
        ] {
            assert_eq!(PasteBehavior::from_key(b.key()), b);
        }
        assert_eq!(
            PasteBehavior::from_key("nonsense"),
            PasteBehavior::ConfirmMultiLine
        );

        for c in [CursorStyle::Block, CursorStyle::Bar, CursorStyle::Underline] {
            assert_eq!(CursorStyle::from_key(c.key()), c);
        }
        assert_eq!(CursorStyle::from_key("nonsense"), CursorStyle::Block);

        for a in [
            StartupAction::LocalShell,
            StartupAction::CommandPalette,
            StartupAction::RestoreSession,
            StartupAction::Nothing,
        ] {
            assert_eq!(StartupAction::from_key(a.key()), a);
        }
        assert_eq!(
            StartupAction::from_key("nonsense"),
            StartupAction::LocalShell
        );
        // 面板只给三档：`restore-session` 必须**不在**里面（避免与
        // `session_restore` 摆成两个入口）
        assert!(!StartupAction::panel_options().contains(&StartupAction::RestoreSession));
    }

    /// 旧配置迁移：三个旧粘贴字段 → 一个新档位。
    ///
    /// 这是升级路径的唯一正确性来源，必须逐组合钉住。
    #[test]
    fn legacy_paste_flags_migrate_to_one_behavior() {
        // ① 旧默认（normalize=true + protect=true）⇒ 多行时先确认
        let legacy = r#"{
            "terminal_paste_normalize_newlines": true,
            "terminal_paste_protect": true,
            "terminal_paste_protect_lines": 3
        }"#;
        let s = Settings::load_from_str(legacy);
        assert_eq!(s.terminal_paste_behavior, PasteBehavior::ConfirmMultiLine);

        // ② 用户关掉了保护 ⇒ 直接执行（保留用户原来"不弹框"的意图）
        let legacy = r#"{
            "terminal_paste_normalize_newlines": true,
            "terminal_paste_protect": false
        }"#;
        let s = Settings::load_from_str(legacy);
        assert_eq!(s.terminal_paste_behavior, PasteBehavior::ExecuteDirectly);

        // ③ 用户关掉了归一（要保留原始字节）⇒ 原样粘贴
        let legacy = r#"{
            "terminal_paste_normalize_newlines": false,
            "terminal_paste_protect": true
        }"#;
        let s = Settings::load_from_str(legacy);
        assert_eq!(s.terminal_paste_behavior, PasteBehavior::Raw);

        // ④ 已经是新格式：不迁移，尊重用户的新选择。
        //
        //    这里必须把「新字段作数」与「旧字段不再作数」一起验：新档位故意选
        //    `always-confirm`，而**默认档恰好也是** `ConfirmMultiLine` ——
        //    若只钉 `ConfirmMultiLine`，把迁移逻辑整个删掉用例照样绿（默认值替
        //    它挡了枪）。旧字段的组合本会迁移成 `ExecuteDirectly`，只要迁移被
        //    误触发，这里就会读到 `ExecuteDirectly`（用户改完设置又变回去，
        //    且没有任何提示）。
        let modern = r#"{
            "terminal_paste_normalize_newlines": true,
            "terminal_paste_protect": false,
            "terminal_paste_behavior": "always-confirm"
        }"#;
        let s = Settings::load_from_str(modern);
        assert_eq!(
            s.terminal_paste_behavior,
            PasteBehavior::AlwaysConfirm,
            "已有新字段时迁移不得覆盖用户的显式选择"
        );

        // ④b 反向对照：去掉新字段、留同样的旧字段，就必须迁移成 `ExecuteDirectly`。
        //     有这一条才能证明 ④ 绿是因为「没迁移」，而不是因为「迁移逻辑根本不存在」。
        let same_but_legacy = r#"{
            "terminal_paste_normalize_newlines": true,
            "terminal_paste_protect": false
        }"#;
        let s = Settings::load_from_str(same_but_legacy);
        assert_eq!(
            s.terminal_paste_behavior,
            PasteBehavior::ExecuteDirectly,
            "对照组失败：同样的旧字段在缺新字段时**应该**迁移 —— \
             否则 ④ 只是被默认档蒙混过关。"
        );

        // ⑤ 老到连粘贴设置都没有：保持默认档
        let ancient = r#"{"theme_mode":"Dark"}"#;
        let s = Settings::load_from_str(ancient);
        assert_eq!(s.terminal_paste_behavior, PasteBehavior::ConfirmMultiLine);
    }

    /// 落盘字面量必须与 `key()` **逐字符相同**：UI 下拉用 `key()` 取值/回填，
    /// 落盘走 `serde` 的 `rename_all` —— 两套命名一旦分叉，症状是
    /// 「下拉里选中的档位存进 JSON 后变回默认档」，且读回来时**不报任何错**
    ///（serde 的 `_ => 默认` 兜底把不认识的值静默吃掉）。
    ///
    /// 2026-09-28 抓到的实例：`rename_all = "snake_case"` 生成
    /// `confirm_multi_line`，而 `key()` 返回 `confirm-multi-line` ——
    /// 用户选的「多行时先确认」存盘后再读回来会掉进默认档，选「一律先确认」则
    /// **静默降级**成默认档。函数签名是 `pub fn from_key`，但真正决定落盘名字的
    /// 是 serde 属性，两者之间原本没有任何测试把它们绑在一起。
    #[test]
    fn paste_behavior_wire_name_matches_panel_key() {
        for b in PasteBehavior::ALL {
            let json = serde_json::to_string(&b).unwrap();
            let wire = json.trim_matches('"');
            assert_eq!(
                wire,
                b.key(),
                "`PasteBehavior::{b:?}` 的落盘名与面板 `key()` 不一致：\
                 面板选它之后存盘会被 serde 静默吃成默认档。"
            );
            // 反向：落盘名必须能原样读回来（`from_key` 是 UI 的入口，
            // serde 的入口在这里；两个入口都得认同一个名字）
            let back: PasteBehavior = serde_json::from_str(&json).unwrap();
            assert_eq!(back, b, "`{wire}` 写出去读不回来（serde 属性改坏了？）");
        }
        // 对照组：确认这组数字**不是**恒等式 —— `PasteBehavior::ALL` 必须真的
        // 覆盖四个不同档位，否则上面的循环等于只测了一遍默认档。
        assert_eq!(PasteBehavior::ALL.len(), 4);
        let mut names: Vec<&str> = PasteBehavior::ALL.iter().map(|b| b.key()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), 4, "四档的 key() 必须两两不同，否则下拉会撞车");
    }

    /// 旧 `settings.json` 里没有新字段时补齐默认值（升级不 panic、不丢其他值）。
    #[test]
    fn legacy_settings_gain_new_terminal_defaults() {
        let legacy = r#"{
            "theme_mode": "Dark",
            "terminal_font_family": "Consolas",
            "terminal_font_size": 14.0,
            "scrollback": 5000,
            "language": "zh"
        }"#;
        let s = Settings::load_from_str(legacy);
        assert_eq!(s.terminal_font_family, "Consolas");
        assert_eq!(s.terminal_line_height, DEFAULT_TERMINAL_LINE_HEIGHT);
        assert_eq!(s.terminal_cursor_style, CursorStyle::Block);
        assert_eq!(s.startup_action, StartupAction::LocalShell);
    }
}
