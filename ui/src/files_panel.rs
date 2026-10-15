//! 文件面板（3.2 节 SFTP）：远端目录浏览 + 右键操作 + 上传/下载传输队列。

use gpui::{
    div, prelude::FluentBuilder as _, px, uniform_list, ClickEvent, Context, ExternalPaths,
    InteractiveElement as _, IntoElement, ParentElement as _, SharedString,
    StatefulInteractiveElement as _, Styled as _,
};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    input::Input,
    menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem},
    progress::Progress,
    v_flex, Icon, IconName, Sizable as _,
};

use rustshell_core::sftp::{new_transfer_id, SftpCmd};

use super::main_view::MainView;

/// N8：样式取色统一走主题工具函数（删除就地 hex 定义）
use crate::theme::to_hsla as hex;

/// 条目右侧元信息列宽（窄侧栏下也要给名称留出落脚处）。
///
/// 这几列是**定宽列**，不是「跟着文字走」：`uniform_list` 的每一行都要独立布局，
/// 只有定宽才能让「权限 / 属主 / 大小 / 修改时间」在整张表里各占一条竖线。
/// 目录行没有大小，但它照样占一个 [`META_SIZE_W`] 宽的空盒（见 `file_row`）——
/// 不是把两列合成一个宽块：合并之后目录行少一道 `gap`，两行的名称可用宽度
/// 就差了 8px，长文件名会在一类行里比另一类行早一个间距被省略号截断。
///
/// 宽度预算（2026-09-22 加权限与属主两列时算过一遍）：
/// 侧栏默认 [`FILES_WIDTH_DEFAULT`]，最小 `SIDEBAR_WIDTH_MIN`（180 —— 比下面
/// [`MIN_W_FOR_PERM_COLS`] 小得多）。行的固定开销见 [`ROW_FIXED_W`]，
/// 所以「面板宽 − 固定开销 = 文件名可用宽度」，四列每宽 1px 文件名就少 1px。
/// 各列取值按「10px 字号下刚好放得下」定：`-rw-r--r--` 有 10 个字符，
/// 但 10px 下九个权限位只有约 38px 宽，46 足够且与 `META_SIZE_W` 观感拉齐。
const META_SIZE_W: f32 = 44.;
const META_TIME_W: f32 = 64.;
/// 权限列（`-rw-r--r--` / `drwxr-xr-x`）
const META_MODE_W: f32 = 46.;
/// 属主列（`user:group`；`ubuntu:ubuntu` 这类长名字会走省略号）
const META_OWNER_W: f32 = 54.;

/// 行内 `gap_2` 的像素值。**必须与 `file_row` 里的 `.gap_2()` 一致** ——
/// 它参与 [`MIN_W_FOR_PERM_COLS`] 的算术，两处各写字面量就会各说各话。
const ROW_GAP_W: f32 = 8.;

/// 「图标 / 名称 / 大小 / 时间」这一版行的固定开销（**不含**权限与属主两列）：
/// `px_3` 两侧 24 + `ml_1`/`mr_1` 8 + 图标 14 + 大小 44 + 时间 64 + 三道 `gap_2` 24。
const ROW_FIXED_W: f32 = 24. + 8. + 14. + META_SIZE_W + META_TIME_W + 3. * ROW_GAP_W;

/// 权限 + 属主两列连同**各自前面那道** `gap_2`（两列插进来 = 多两个子节点 ⇒ 多两道 gap）。
const PERM_COLS_W: f32 = META_MODE_W + META_OWNER_W + 2. * ROW_GAP_W;

/// 文件名列至少要留出的宽度（12px 字号下约 12 个字符）。
const NAME_MIN_W: f32 = 72.;

/// 低于这个面板宽度就**不画**权限与属主两列（退回「图标/名称/大小/时间」四列版）。
///
/// 为什么要有这条：两列一共吃掉 [`PERM_COLS_W`] = 116px，而侧栏最小宽度是 180px
/// —— 无条件画的话，180~366px 这一段里文件名会被压到 0，再窄还会把右侧列顶出
/// 可视区（行的 `flex_none` 子节点不收缩，超出的部分被列表的 `overflow_hidden`
/// 裁掉）。列表的第一职责是看清名字，所以宁可少两列信息。
///
/// 阈值 = `178 + 116 + 72 = 366`：默认侧栏宽度 380 时两列是**画得出来**的
/// （文件名还剩 86px），从 366 往下才收起。
pub const MIN_W_FOR_PERM_COLS: f32 = ROW_FIXED_W + PERM_COLS_W + NAME_MIN_W;

/// [`crate::main_view`] 里「文件」面板的默认宽度。只用于文档与用例断言，
/// 真值在 `main_view`（见 `FILES_W`），这里取同一个数。
pub const FILES_WIDTH_DEFAULT: f32 = 380.;

/// 这个面板宽度够不够画权限 / 属主两列。
///
/// 抽成纯函数：阈值是**算出来的**（见 [`MIN_W_FOR_PERM_COLS`]），把它摊在
/// `render` 的调用现场就没法单测边界 —— 而这一条的边界正是「文件名会不会被挤没」，
/// 差了 1px 就是两种完全不同的界面。
pub fn room_for_perm_cols(panel_w: f32) -> bool {
    panel_w >= MIN_W_FOR_PERM_COLS
}

/// 面板宽度给到文件名列多少像素（按「画不画权限列」两档算）。
///
/// 与 [`room_for_perm_cols`] 一起构成这段布局的**全部算术**：调用方（和用例）
/// 想看「这个名字还能剩多宽」不必自己复述那两个常量。
pub fn name_column_width(panel_w: f32) -> f32 {
    let fixed = if room_for_perm_cols(panel_w) {
        ROW_FIXED_W + PERM_COLS_W
    } else {
        ROW_FIXED_W
    };
    (panel_w - fixed).max(0.)
}

fn fmt_size(n: u64) -> String {
    if n >= 1 << 30 {
        format!("{:.1}G", n as f64 / (1 << 30) as f64)
    } else if n >= 1 << 20 {
        format!("{:.1}M", n as f64 / (1 << 20) as f64)
    } else if n >= 1 << 10 {
        format!("{:.1}K", n as f64 / (1 << 10) as f64)
    } else {
        format!("{n}B")
    }
}

/// 文件条目 → `(图标资源路径, 取色)`。
///
/// 图标走 `Icon::default().path("icons/…")` **直接点名资源**，而不是 `IconName::X`：
/// 文件类型图标（`file-code` / `file-braces` …）并不在组件的 `IconName` 子集里，
/// 点名 `IconName::` 根本编译不过。代价是路径退化成**字符串**，编译器不会再替我们
/// 检查它是否真的随包发出 —— 所以 `every_icon_the_ui_asks_for_is_shipped` 已把
/// 这里的 `icons/*.svg` 字面量一并纳入检查（否则缺文件只表现为**静默留白**）。
///
/// 分类目标只有「一眼看出是哪一类」这一件事：**形状**负责区分大类，**颜色**只做
/// 次级线索，所以相近类别**有意**复用同一个色（图片/表格共用 success、音频与配置
/// 共用 purple），不为每一类都造一个新颜色 —— 那会让整列花色变嘈杂、反而更难扫读。
///
/// 取 `&ThemeTokens` 而不是返回色名：色值必须跟着主题走，写死 hex 会在浅色主题下
/// 变成一片糊字。
fn entry_icon(name: &str, t: &rustshell_core::theme::ThemeTokens) -> (&'static str, u32) {
    // 只认最后一段扩展名（`.tar.gz` 归到 `gz`，仍属归档，这正是想要的结果）。
    // 没有 `.` 的（`Makefile` / `.bashrc`）走默认，不硬猜。
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        // 源码
        "rs" | "py" | "js" | "mjs" | "cjs" | "jsx" | "ts" | "tsx" | "go" | "c" | "h" | "cc"
        | "cpp" | "cxx" | "hpp" | "hh" | "cs" | "java" | "kt" | "kts" | "swift" | "rb" | "php"
        | "lua" | "dart" | "scala" | "hs" | "zig" | "nim" | "pl" | "r" | "m" | "mm" | "vue"
        | "svelte" => ("icons/file-code.svg", t.accent),
        // 脚本
        "sh" | "bash" | "zsh" | "fish" | "ps1" | "psm1" | "bat" | "cmd" => {
            ("icons/file-terminal.svg", t.success)
        }
        // 配置（结构化文本）
        "json" | "jsonc" | "json5" | "yaml" | "yml" | "toml" | "ini" | "cfg" | "conf" | "env"
        | "properties" | "xml" | "plist" | "lock" => ("icons/file-braces.svg", t.purple),
        // 文档
        "md" | "markdown" | "rst" | "txt" | "log" | "pdf" | "doc" | "docx" | "odt" | "rtf"
        | "ppt" | "pptx" | "odp" | "tex" => ("icons/file-text.svg", t.fg_muted),
        // 表格
        "csv" | "tsv" | "xls" | "xlsx" | "ods" | "numbers" => {
            ("icons/file-spreadsheet.svg", t.success)
        }
        // 图片
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "svg" | "ico" | "tif" | "tiff"
        | "heic" | "avif" | "psd" => ("icons/file-image.svg", t.success),
        // 音频
        "mp3" | "wav" | "flac" | "ogg" | "m4a" | "aac" | "opus" | "wma" | "mid" => {
            ("icons/file-music.svg", t.purple)
        }
        // 视频
        "mp4" | "mkv" | "avi" | "mov" | "webm" | "flv" | "wmv" | "m4v" | "mpg" | "mpeg" => {
            ("icons/file-play.svg", t.accent)
        }
        // 归档
        "zip" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "zst" | "7z" | "rar" | "lz" | "lzma"
        | "cab" | "iso" => ("icons/file-archive.svg", t.warning),
        // 数据库 / 转储
        "db" | "sqlite" | "sqlite3" | "mdb" | "accdb" | "sql" | "dump" => {
            ("icons/database.svg", t.purple)
        }
        // 密钥 / 证书：这一类的误操作代价最高，用 danger 色把它顶出来
        "pem" | "key" | "crt" | "cer" | "pub" | "ppk" | "p12" | "pfx" | "keystore" | "jks"
        | "gpg" | "asc" => ("icons/file-key.svg", t.danger),
        // 字体
        "ttf" | "otf" | "woff" | "woff2" | "eot" => ("icons/file-type.svg", t.fg_secondary),
        // 可执行体 / 安装包
        "exe" | "msi" | "dll" | "so" | "dylib" | "bin" | "deb" | "rpm" | "apk" | "appimage"
        | "dmg" | "pkg" | "o" | "a" | "lib" | "jar" | "war" | "class" | "wasm" => {
            ("icons/file-box.svg", t.fg_secondary)
        }
        _ => ("icons/file.svg", t.fg_muted),
    }
}

fn parent_path(p: &str) -> String {
    let trimmed = p.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) => "/".into(),
        Some(i) => trimmed[..i].to_string(),
        None => "/".into(),
    }
}

fn join_path(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// 目录一行的**展示缓存**：目录列表变化时重建一次，渲染期只读不算。
///
/// 为什么要有这一层（《性能与帧率优化分析》P0-2）：原来的渲染路径对每一行现算
/// `format_mtime`（内含 `chrono::Local::now()`）、`join_path`、`format!("file-…")`、
/// 名称 `clone` 与两个 `SharedString` —— 每帧每行 6 次以上分配，而目录内容不变时
/// 这些字符串一个字都不会变。缓存之后每帧的成本只剩「取一次 `Rc`」。
///
/// 行高**必须**保持一致（`uniform_list` 只量第一行就按它排布其余全部）：所有行共用
/// 同一套 `px_3 / py_1 / gap_2` 与同一个图标尺寸，谁给某一类行加一行说明就会错位。
pub struct FileRow {
    /// 元素 id（`file-<完整路径>`），列表变化时拼一次
    pub id: SharedString,
    pub name: SharedString,
    /// 完整远端路径：点击进入目录 / 右键菜单的落点
    pub path: String,
    pub is_dir: bool,
    /// 大小展示串（目录为空串，右侧用占位保持列宽）
    pub size: SharedString,
    /// 修改时间展示串
    pub mtime: SharedString,
    /// 权限串（`-rw-r--r--` / `drwxr-xr-x`）
    ///
    /// 服务端没在 `SSH_FXP_NAME` 里带权限时是**空串**（不是 `----------`）：
    /// 「查不到」与「谁都不能访问」是两件事，画成后者会让人做出错误判断。
    pub mode: SharedString,
    /// 属主串（`user:group`，只有数字 id 时退成 `0:0`）；同上，未知即空串
    pub owner: SharedString,
}

impl FileRow {
    /// `mtime` 由调用方批量算好后传进来（见 `rustshell_core::sftp::format_entry_mtimes`）。
    pub fn new(dir: &str, e: &rustshell_core::sftp::SftpEntry, mtime: String) -> Self {
        let path = join_path(dir, &e.name);
        Self {
            id: SharedString::from(format!("file-{path}")),
            name: SharedString::from(e.name.clone()),
            is_dir: e.is_dir,
            size: if e.is_dir {
                SharedString::from("")
            } else {
                SharedString::from(fmt_size(e.size))
            },
            mtime: SharedString::from(mtime),
            // 权限与属主的格式化都收在 core（纯函数，那边有用例钉着 9 位等宽、
            // 数字 id 回退这些约定），UI 只负责把它放进一条定宽列里。
            mode: SharedString::from(rustshell_core::sftp::entry_mode(e)),
            owner: SharedString::from(rustshell_core::sftp::entry_owner(e)),
            path,
        }
    }
}

/// 目录列表里的一行（`uniform_list` 的 item 构造器；只对视口内的行调用）。
///
/// `show_perm` 决定要不要画权限 / 属主两列，由调用方按**面板当前宽度**算好传入
/// （见 [`MIN_W_FOR_PERM_COLS`]）。做成参数而不是在这里读宽度：行只能从 `&mut Window`
/// 拿到窗口尺寸，拿不到「这个列表自己被分到多少宽」—— 那是父容器的布局结果。
fn file_row(
    row: &FileRow,
    tx: &flume::Sender<SftpCmd>,
    t: &rustshell_core::theme::ThemeTokens,
    show_perm: bool,
    verify: bool,
) -> impl IntoElement {
    let is_dir = row.is_dir;
    let icon: gpui::AnyElement = if is_dir {
        Icon::new(IconName::FolderClosed)
            .small()
            .text_color(hex(t.warning))
            .into_any_element()
    } else {
        // 按扩展名分派图标（2026-09-20 反馈「文件树对不同文件类型展示不同
        // 图标」）：目录仍走 `IconName::FolderClosed`，文件走资源路径点名。
        let (icon_path, icon_color) = entry_icon(&row.name, t);
        Icon::default()
            .path(icon_path)
            .small()
            .text_color(hex(icon_color))
            .into_any_element()
    };

    let mut el = div()
        .id(row.id.clone())
        .flex()
        .items_center()
        .gap_2()
        .px_3()
        .py_1()
        .ml_1()
        .mr_1()
        .rounded(px(6.))
        // `w_full()` 是**这两列能对齐的前提**（2026-09-22 修「时间/大小字段错位」）：
        // 行根节点自己不声明宽度时按内容收缩，名称上的 `flex_1` 就没有剩余空间可吃，
        // `大小 / 时间` 会紧跟在名称后面 —— 每行的名称长度不同，两列的起点就跟着
        // 一行一个位置。声明 `w_full()` 之后名称吃掉整行剩余宽度，右侧两列被顶到
        // 行右缘，整张表共用同一条竖线。
        //
        // 实测（`meta_columns_line_up_regardless_of_name_length`）：
        //   有 `w_full()` → 文件行 / 目录行的时间列都在 x=284；
        //   去掉它       → 130 / 296（差 166px，就是截图里那种错位）。
        .w_full()
        .hover(|s| s.bg(hex(t.bg_overlay)))
        .on_click({
            // 目录点击进入；文件点击无动作
            let tx = tx.clone();
            let next = row.path.clone();
            move |_: &ClickEvent, _w, _cx| {
                if is_dir {
                    send_list(&tx, next.clone(), _cx);
                }
            }
        })
        .context_menu({
            let tx = tx.clone();
            let full = row.path.clone();
            let name = row.name.to_string();
            move |menu: PopupMenu, _w, _cx| {
                // 刷新目录由行自身的路径推出来（`parent_path`），不再逐帧克隆当前目录
                let dir = parent_path(&full);
                build_entry_menu(menu, &tx, &full, &dir, &name, is_dir, verify)
            }
        })
        .child(icon)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_size(px(12.))
                .text_color(hex(t.fg_primary))
                .text_ellipsis()
                .child(row.name.clone()),
        );
    // 右侧元信息：权限 + 属主 + 大小（目录留**同宽空盒**）+ 修改时间。
    //
    // 顺序是「描述这个条目的 → 描述它的数量/时间的」，所以两列新列紧跟在名称
    // 之后、大小之前。四列全是定宽 + `flex_none`：名称上的 `flex_1` 吃掉剩余
    // 宽度，四列因此被顶到行右缘、在整张表里各占一条竖线。
    //
    // 目录行放的是「和文件行 size 元素同宽的空盒」，不是「把两列合并成一个宽块」：
    // 合并块比「size + gap」少一道 `gap`，两行的名称可用宽度就此差 8px —— 于是
    // 长文件名在目录行会比文件行早一个间距被截断，而**列本身还是对齐的**，
    // 这种偏差在截图里看不出来、只在名称满格时才露头。
    //
    // 权限与属主**不做目录/文件占位区分**：两者对目录同样有意义（`drwxr-xr-x`），
    // 服务端不给时留下的也是同一个空盒。
    // 权限与属主**不做目录/文件占位区分**：两者对目录同样有意义（`drwxr-xr-x`），
    // 服务端不给时留下的也是同一个空盒。
    //
    // 两列整体受 `show_perm` 控制（窄侧栏下不画）：成对出现或成对消失，
    // 不会出现「有权限没属主」这种半截状态。
    if show_perm {
        el = el.child(
            div()
                .flex_none()
                .w(px(META_MODE_W))
                .text_size(px(10.))
                .text_color(hex(t.fg_muted))
                .text_ellipsis()
                // 供测试量「四列是否真的贴在同一条竖线上」；release 下是 no-op。
                .debug_selector(|| "file-mode".to_string())
                .child(row.mode.clone()),
        );
        el = el.child(
            div()
                .flex_none()
                .w(px(META_OWNER_W))
                .text_size(px(10.))
                .text_color(hex(t.fg_muted))
                .text_ellipsis()
                .debug_selector(|| "file-owner".to_string())
                .child(row.owner.clone()),
        );
    }
    el = el.child(if is_dir {
        div().flex_none().w(px(META_SIZE_W)).h(px(1.))
    } else {
        div()
            .flex_none()
            .w(px(META_SIZE_W))
            .text_size(px(10.))
            .text_color(hex(t.fg_muted))
            .text_ellipsis()
            // 供测试量「这一列是否真的贴在同一条竖线上」；release 下是 no-op。
            .debug_selector(|| "file-size".to_string())
            .child(row.size.clone())
    });
    el.child(
        div()
            .flex_none()
            .w(px(META_TIME_W))
            .text_size(px(10.))
            .text_color(hex(t.fg_muted))
            // 选择器按行的类别分开：护栏要同时量到「文件行的时间列」与「目录行的
            // 时间列」并断言两者 x 相等 —— 同一个选择器在一帧里被多个元素认领时，
            // `debug_bounds` 只会给其中一个，量不出「它们是否在同一条竖线上」。
            // release 下是 no-op。
            .debug_selector(move || {
                if is_dir {
                    "file-time-dir".to_string()
                } else {
                    "file-time-file".to_string()
                }
            })
            .child(row.mtime.clone()),
    )
}

pub fn render(m: &mut MainView, cx: &mut Context<MainView>) -> impl IntoElement {
    let t = crate::theme::current(cx);
    // 校验开关每帧快照一次：菜单项在被点开时读到的是**当时**的设置值
    let verify = m.settings.sftp_verify_downloads;

    let mut root = v_flex()
        .id("files-panel")
        // 测试钩子：拖拽落点的 e2e 用例要拿面板的几何来派发 `FileDropEvent`
        // （`debug_bounds` 只认 `debug_selector`，`id` 不算）。
        .debug_selector(|| "files-panel".to_string())
        .w_full()
        .h_full()
        // 拖入落点（F-13）挂在**面板根**上：gpui 的命中测试把祖先 hitbox 也算 hovered
        // （`HitboxId::hit_test` 收的是整条命中路径），所以挂在根上就覆盖了列表、
        // 传输队列、空态 —— 不必给每个子元素各挂一次。
        // `.relative()` 是给下面那条悬浮提示做定位锚点。
        .relative()
        .can_drop(|value, _, _| value.is::<ExternalPaths>())
        .drag_over::<ExternalPaths>(move |style, _paths, _, _| {
            // 悬浮期整面板高亮：只压一层底色，**不加边框** —— 边框会占布局，
            // 悬停瞬间整块列表会跟着挪 1px（拖到自己想放的文件上时最难受）。
            style.bg(hex(t.bg_overlay))
        })
        .on_drop::<ExternalPaths>(|paths: &ExternalPaths, w, cx| {
            drop_uploads(paths.paths().to_vec(), w, cx);
        })
        // 滚动的只有下面的 `uniform_list`：整页滚动（原来的 `overflow_y_scrollbar`）
        // 会把标题栏与工具行一起滚走，而且与虚拟列表自己的滚动区嵌套，是双重滚动条。
        .child(
            // 标题 + 路径栏（路径栏可直接编辑：输入任意远端路径回车跳转）
            v_flex()
                .px_3()
                .py_2()
                .gap_1()
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(hex(t.fg_muted))
                        .child(rustshell_core::i18n::t("文件")),
                )
                .child(
                    // 点框外失焦（2026-09-18 用户要求「文件的地址栏，聚焦后点击别的地方
                    // 需要失焦」）：官方 `Input` 只在点到**另一个可聚焦元素**时才换焦点，
                    // 点侧栏/终端空白处不会松手。实现在 `main_view::blur_input_on_outside_click`
                    // （捕获阶段触发、只在焦点真在本框上时才收）。
                    // 失焦会触发 `files_path_blur`（草稿回退成当前路径），与既有订阅一致。
                    div()
                        .w_full()
                        .on_mouse_down_out(cx.listener(|m, _e, w, cx| {
                            super::main_view::blur_input_on_outside_click(
                                &m.io.files_path_input,
                                w,
                                cx,
                            )
                        }))
                        .child(Input::new(&m.io.files_path_input).small().w_full()),
                ),
        );

    // 当前激活会话是否支持 SFTP
    let Some(sftp_tx) = m.active_tab().and_then(|t| t.session.sftp_tx.clone()) else {
        root = root.child(
            v_flex()
                .px_3()
                .py_3()
                .gap_2()
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(hex(t.fg_muted))
                        .child(rustshell_core::i18n::t("需要 SSH 会话")),
                )
                .child(
                    Button::new("files-goto-conn")
                        .small()
                        .outline()
                        .label(rustshell_core::i18n::t("前往连接"))
                        .on_click(cx.listener(|m, _e, _w, cx| {
                            m.panel_ui.panel = super::main_view::PanelKind::Connections;
                            m.panel_ui.sidebar_collapsed = false;
                            cx.notify();
                        })),
                ),
        );
        return root;
    };

    // 工具行：上级 / 刷新 / 新建目录 / 上传
    let path_up = parent_path(&m.io.files_path);
    let refresh_path = m.io.files_path.clone();
    let mkdir_base = m.io.files_path.clone();
    let upload_dir = m.io.files_path.clone();
    root = root.child(
        div()
            .flex()
            .gap_1()
            .px_2()
            .pb_1()
            .child(
                Button::new("files-up")
                    .ghost()
                    .xsmall()
                    .icon(IconName::ArrowLeft)
                    .tooltip(rustshell_core::i18n::t("上级目录"))
                    .on_click({
                        let tx = sftp_tx.clone();
                        move |_, _w, _cx| send_list(&tx, path_up.clone(), _cx)
                    }),
            )
            .child(
                Button::new("files-refresh")
                    .ghost()
                    .xsmall()
                    .icon(IconName::Redo2)
                    .tooltip(rustshell_core::i18n::t("刷新"))
                    .on_click({
                        let tx = sftp_tx.clone();
                        move |_, _w, _cx| send_list(&tx, refresh_path.clone(), _cx)
                    }),
            )
            .child(
                Button::new("files-mkdir")
                    .ghost()
                    .xsmall()
                    .icon(IconName::FolderOpen)
                    .tooltip(rustshell_core::i18n::t("新建目录"))
                    .on_click({
                        let base = mkdir_base.clone();
                        move |_, w, cx| {
                            // 走对话框收名字（2026-09-22 用户要求）：此前是
                            // 「`新建目录-<时间戳>` 直接建，反正建完能改名」，
                            // 但改名入口此前也只是加个 `-renamed` 后缀 ——
                            // 两条路都不给用户输入的机会。
                            super::main_view::ui_call(
                                &base,
                                |m, dir, w, cx| m.open_sftp_mkdir_dialog(dir.to_string(), w, cx),
                                w,
                                cx,
                            );
                        }
                    }),
            )
            .child(
                Button::new("files-upload")
                    .ghost()
                    .xsmall()
                    .icon(IconName::ArrowUp)
                    .tooltip(rustshell_core::i18n::t("上传文件到当前目录"))
                    .on_click({
                        let tx = sftp_tx.clone();
                        move |_, _w, cx| prompt_upload(&upload_dir.clone(), &tx, cx)
                    }),
            ),
    );

    // 加载/错误提示
    if m.io.files_loading {
        root = root.child(
            div()
                .px_3()
                .py_1()
                .text_size(px(11.))
                .text_color(hex(t.fg_muted))
                .child(rustshell_core::i18n::t("加载中…")),
        );
    }
    if let Some(err) = &m.io.files_error {
        // 读目录失败等错误走官方`Alert`（error 变体）：此前只有一行红字，
        // 既没有图标也没有可辨认的边界，在文件列表里容易被当成普通行。
        root = root.child(
            div().px_1().py_1().child(
                crate::notice::NoticeKind::Error
                    .compact("files-error", SharedString::from(err.clone())),
            ),
        );
    }

    // 目录条目：**虚拟列表**（`uniform_list`）。
    //
    // 换掉原来的「v_flex + overflow_y_scrollbar 逐行 child」有两条收益
    // （《性能与帧率优化分析》P0-2）：
    // ① 只有视口内的行进入布局与绘制 —— 大目录下的帧时间从「与条目数成正比」
    //    变成「与视口行数成正比」；
    // ② 行内容全部来自 `files_rows` 缓存，渲染期不再做任何字符串计算，
    //    也不再 `clone` 整张目录表。
    //
    // 行高固定是这一条成立的前提（`uniform_list` 只量第一行）：所有行共用同一套
    // padding 与图标尺寸，**不要**给某类行单独加高度。
    let rows = m.io.files_rows.clone(); // Rc：O(1)，不是整表克隆
                                        // 权限/属主两列的画与不画：按**这个面板记下的宽度**判（见 `MIN_W_FOR_PERM_COLS`）。
                                        // 用记下的宽度而不是实测宽度，是因为行只能从 `&mut Window` 拿窗口尺寸；
                                        // 而侧栏宽度在拖拽结束时就写回了 `settings`，与渲染用的是同一个数。
    let show_perm = room_for_perm_cols(m.settings.sidebar_width_for(m.panel_ui.panel.key()));
    if rows.is_empty() {
        if !m.io.files_loading && m.io.files_error.is_none() {
            root = root.child(
                div()
                    .px_3()
                    .py_2()
                    .text_size(px(12.))
                    .text_color(hex(t.fg_muted))
                    .child(rustshell_core::i18n::t("空目录")),
            );
        }
    } else {
        let tx = sftp_tx.clone();
        root = root.child(
            // 外层给确定高度（`flex_1` + `min_h_0`）：`uniform_list` 默认的
            // `ListSizingBehavior::Auto` 要由样式定尺寸，尺寸不确定时它不会滚动。
            div().flex_1().min_h_0().w_full().overflow_hidden().child(
                uniform_list(
                    "files-list",
                    rows.len(),
                    move |range: std::ops::Range<usize>, _w, cx| {
                        // 主题每段只取一次（原来是每行取一次）
                        let t = crate::theme::current(cx);
                        range
                            .map(|ix| {
                                file_row(&rows[ix], &tx, &t, show_perm, verify).into_any_element()
                            })
                            .collect::<Vec<_>>()
                    },
                )
                .size_full(),
            ),
        );
    }

    // 传输队列
    let transfers = m.io.files_transfers.clone();
    if !transfers.is_empty() {
        root = root.child(
            div()
                .pt_2()
                .px_3()
                .text_size(px(11.))
                .text_color(hex(t.fg_muted))
                .child(rustshell_core::i18n::t("传输队列")),
        );
        for (idx, tr) in transfers.iter().enumerate() {
            let pct = if tr.total > 0 {
                (tr.bytes as f32 / tr.total as f32 * 100.0).min(100.0)
            } else {
                0.0
            };
            let color = if tr.done {
                if tr.ok {
                    hex(t.success)
                } else {
                    hex(t.danger)
                }
            } else {
                hex(t.accent)
            };
            root = root.child(
                v_flex()
                    .px_3()
                    .py_1()
                    .gap_0p5()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_ellipsis()
                                    .child(SharedString::from(tr.name.clone())),
                            )
                            .child(div().text_size(px(10.)).text_color(color).child(
                                SharedString::from(if tr.done {
                                    if tr.ok {
                                        rustshell_core::i18n::t("完成").to_string()
                                    } else {
                                        rustshell_core::i18n::t("失败").to_string()
                                    }
                                } else {
                                    format!("{pct:.2}%")
                                }),
                            )),
                    )
                    .child(
                        // 官方 Progress（审计表 #7：统一样式与确定/不确定态）
                        Progress::new(SharedString::from(format!("tr-progress-{idx}")))
                            .small()
                            .value(pct)
                            .color(color),
                    )
                    .when_some(tr.note.clone(), |el, note| {
                        // 续传/跳过这类备注（F-13）：单独一行小字，不挤占文件名与状态
                        el.child(
                            div()
                                .text_size(px(10.))
                                .text_color(hex(t.fg_muted))
                                .text_ellipsis()
                                .child(SharedString::from(note)),
                        )
                    }),
            );
        }
    }

    root = root.child(
        // 拖入悬浮提示（F-13）：绝对定位压在列表之上 —— 不用 `opacity(0)` 的空位
        // 占一行高度（那会让「拖一下」把整块列表顶下去）。有外部文件悬停时才显形。
        div()
            .id("files-drop-hint")
            .absolute()
            .bottom_2()
            .left_2()
            .right_2()
            .px_2()
            .py_1()
            .rounded(px(6.))
            .bg(hex(t.bg_overlay))
            .border_1()
            .border_color(hex(t.accent))
            .text_size(px(11.))
            .text_color(hex(t.fg_primary))
            .opacity(0.0)
            .drag_over::<ExternalPaths>(|style, _paths, _, _| style.opacity(1.0))
            .child(SharedString::from(rustshell_core::i18n::ts(
                format!("松开即上传到 {}", m.io.files_path),
                format!("Release to upload to {}", m.io.files_path),
            ))),
    );

    root
}

/// 拖入落点（App 上下文）：把拖进来的本地文件入队上传到**当前**远端目录。
///
/// 落点目录与发送端都在这条回调里**现取**，不在渲染期快照：拖拽可能悬停好几秒，
/// 期间用户完全可能换了目录或切了 Tab —— 快照会让文件落到「拖进来那一刻」的目录，
/// 那是用户看不见的旧状态。
fn drop_uploads(paths: Vec<std::path::PathBuf>, w: &mut gpui::Window, cx: &mut gpui::App) {
    use super::main_view::UiHub;
    let Some(hub) = cx.try_global::<UiHub>() else {
        return;
    };
    let main = hub.main.clone();
    let _ = main.update(cx, |m, cx| {
        let tx = m.active_tab().and_then(|t| t.session.sftp_tx.clone());
        let Some(tx) = tx else {
            // 不做「静默忽略」：拖进来没反应，用户只会以为拖拽坏了
            m.set_quick_toast(
                crate::notice::NoticeKind::Warning,
                rustshell_core::i18n::t("需要 SSH 会话"),
                w,
                cx,
            );
            return;
        };
        let dir = m.io.files_path.clone();
        let (queued, skipped_dirs) = enqueue_uploads(&paths, &dir, &tx);
        if skipped_dirs > 0 {
            m.set_quick_toast(
                crate::notice::NoticeKind::Warning,
                &rustshell_core::i18n::ts(
                    format!("暂不支持拖入文件夹（已跳过 {skipped_dirs} 个）"),
                    format!("Folders are not supported yet ({skipped_dirs} skipped)"),
                ),
                w,
                cx,
            );
        } else if queued == 0 {
            m.set_quick_toast(
                crate::notice::NoticeKind::Warning,
                rustshell_core::i18n::t("没读到拖入的文件"),
                w,
                cx,
            );
        }
    });
}

/// 把一组本地路径逐个入队上传到 `dir`（拖入与「上传」按钮共用同一条路径）。
///
/// 返回 `(入队数, 跳过的目录数)`：目录 v1 不支持递归上传，调用方据此给可见提示 ——
/// 「静默跳过」会让用户以为已经传过了。
fn enqueue_uploads(
    paths: &[std::path::PathBuf],
    dir: &str,
    tx: &flume::Sender<SftpCmd>,
) -> (usize, usize) {
    let mut queued = 0;
    let mut skipped_dirs = 0;
    for f in paths {
        if f.is_dir() {
            skipped_dirs += 1;
            continue;
        }
        let Some(name) = f.file_name().map(|s| s.to_string_lossy().to_string()) else {
            continue;
        };
        let total = std::fs::metadata(f).map(|m| m.len()).unwrap_or(0);
        let id = new_transfer_id();
        let _ = rustshell_core::sftp::sftp_events().0.send(
            rustshell_core::sftp::SftpEvent::TransferQueued {
                transfer_id: id,
                name: rustshell_core::i18n::ts(format!("上传 {name}"), format!("Upload {name}")),
                total,
            },
        );
        let _ = tx.try_send(SftpCmd::Upload {
            local: f.clone(),
            remote: join_path(dir, &name),
            transfer_id: id,
            total,
        });
        queued += 1;
    }
    (queued, skipped_dirs)
}

/// 单个条目的右键菜单（闭包内即用即克隆，保持 Fn 兼容）。
fn build_entry_menu(
    menu: PopupMenu,
    tx: &flume::Sender<SftpCmd>,
    full: &str,
    dir: &str,
    name: &str,
    is_dir: bool,
    verify: bool,
) -> PopupMenu {
    let tx_c = tx.clone();
    let full_c = full.to_string();
    let menu = menu
        .when(!is_dir, |m_| {
            m_.item(
                PopupMenuItem::new(rustshell_core::i18n::t("下载…")).on_click(move |_, _w, cx| {
                    prompt_download(full_c.clone(), verify, &tx_c.clone(), cx);
                }),
            )
        })
        .when(is_dir, |m_| {
            m_.item(
                PopupMenuItem::new(rustshell_core::i18n::t("打开")).on_click({
                    let tx = tx.clone();
                    let next = full.to_string();
                    move |_, _w, cx| send_list(&tx, next.clone(), cx)
                }),
            )
            // 目录下载：递归拉取整棵树
            .item(
                PopupMenuItem::new(rustshell_core::i18n::t("下载文件夹…")).on_click({
                    let tx = tx.clone();
                    let next = full.to_string();
                    move |_, _w, cx| prompt_download_dir(next.clone(), &tx, cx)
                }),
            )
        })
        .separator()
        .item(
            PopupMenuItem::new(rustshell_core::i18n::t("重命名…")).on_click({
                let from = full.to_string();
                let parent = dir.to_string();
                move |_, w, cx| {
                    // 收用户输入的新名字（2026-09-22 用户要求）。此前是追一个
                    // `-renamed` 后缀，用户拿到的名字里带着工具的实现细节。
                    super::main_view::ui_call(
                        &from,
                        |m, from, w, cx| {
                            m.open_sftp_rename_dialog(from.to_string(), parent.clone(), w, cx)
                        },
                        w,
                        cx,
                    );
                }
            }),
        )
        .item(
            PopupMenuItem::new(rustshell_core::i18n::t("删除")).on_click({
                let full = full.to_string();
                let dir = dir.to_string();
                move |_, w, cx| {
                    // P2-14：破坏性操作先弹确认对话框（官方 Dialog），确认后才发 Remove
                    super::main_view::ui_call(
                        &full,
                        |m, full, w, cx| {
                            m.confirm_sftp_remove(full.to_string(), is_dir, dir.clone(), w, cx);
                        },
                        w,
                        cx,
                    );
                }
            }),
        )
        .separator();
    // 复制类动作放最后：复制只动剪贴板，不碰远端
    let name_c = name.to_string();
    let full_path_c = full.to_string();
    menu.item(
        PopupMenuItem::new(rustshell_core::i18n::t("复制文件名")).on_click(move |_, _w, cx| {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(name_c.clone()));
        }),
    )
    .item(
        PopupMenuItem::new(rustshell_core::i18n::t("复制文件路径")).on_click(move |_, _w, cx| {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(full_path_c.clone()));
        }),
    )
}

// 辅助

fn send_list(tx: &flume::Sender<SftpCmd>, path: String, cx: &mut gpui::App) {
    let _ = tx.try_send(SftpCmd::List { path });
    let Some(hub) = cx.try_global::<super::main_view::UiHub>() else {
        return;
    };
    let main = hub.main.clone();
    let _ = main.update(cx, |m, cx| {
        m.io.files_loading = true;
        m.io.files_error = None;
        cx.notify();
    });
}

/// 上传：系统文件选择器（后台等待，不阻塞 UI） 到 传输队列 到 上传到当前远端目录
fn prompt_upload(dir: &str, tx: &flume::Sender<SftpCmd>, cx: &mut gpui::App) {
    let dir = dir.to_string();
    let tx = tx.clone();
    let rx = cx.prompt_for_paths(gpui::PathPromptOptions {
        files: true,
        directories: false,
        multiple: true,
        prompt: Some(rustshell_core::i18n::t("选择要上传的文件").into()),
    });
    cx.background_executor()
        .spawn(async move {
            if let Ok(Ok(Some(paths))) = rx.await {
                // 与拖入共用同一条入队路径；选择器已过滤掉目录，跳过数为 0
                let _ = enqueue_uploads(&paths, &dir, &tx);
            }
        })
        .detach();
}

/// 下载：系统保存对话框（后台等待） 到 传输队列 到 下载远端文件
///
/// `verify` 由调用方**当次现读设置**传进来（而不是在 SFTP 服务建立时固定）：
/// 校验开关是用户按文件决定的 —— 小文件想校验、大文件不想，改完设置不必重连。
fn prompt_download(
    remote_full: String,
    verify: bool,
    tx: &flume::Sender<SftpCmd>,
    cx: &mut gpui::App,
) {
    let name = remote_full.rsplit('/').next().unwrap_or("file").to_string();
    let default_dir = std::env::var("USERPROFILE")
        .map(|h| std::path::PathBuf::from(h).join("Downloads"))
        .unwrap_or_else(|_| std::path::PathBuf::from("."));
    let tx = tx.clone();
    let rx = cx.prompt_for_new_path(&default_dir, Some(&name));
    cx.background_executor()
        .spawn(async move {
            if let Ok(Ok(Some(local))) = rx.await {
                let mut local = local;
                // F-13 续传的落点配合：这个 `(远端, 本地)` 有未完成记录 ⇒ **复用原路径**。
                // 一旦按 P2-14 追加序号换了落点，记录 key 就失配（记录认的是原路径），
                // 半成品立刻变成无人认领的孤儿文件，续传再也接不上。
                let mid_flight = rustshell_core::sftp_resume::resume_store()
                    .find(
                        rustshell_core::sftp_resume::TransferDir::Download,
                        &local.to_string_lossy(),
                        &remote_full,
                    )
                    .is_some();
                // P2-14：不静默覆盖本地同名文件，自动追加序号（续传中的文件除外）
                if !mid_flight && local.exists() {
                    let dir = local.parent().map(|p| p.to_path_buf()).unwrap_or_default();
                    let stem = local
                        .file_stem()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_else(|| "file".into());
                    let ext = local
                        .extension()
                        .map(|s| format!(".{}", s.to_string_lossy()))
                        .unwrap_or_default();
                    let mut n = 1;
                    loop {
                        let cand = dir.join(format!("{stem} ({n}){ext}"));
                        if !cand.exists() {
                            local = cand;
                            break;
                        }
                        n += 1;
                    }
                }
                let save_name = local
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| name.clone());
                let total = 0; // 远端大小未知，由 SFTP 服务 stat 回填（TransferMeta）
                let id = new_transfer_id();
                let _ = rustshell_core::sftp::sftp_events().0.send(
                    rustshell_core::sftp::SftpEvent::TransferQueued {
                        transfer_id: id,
                        name: rustshell_core::i18n::ts(
                            format!("下载 {save_name}"),
                            format!("Download {save_name}"),
                        ),
                        total,
                    },
                );
                let _ = tx.try_send(SftpCmd::Download {
                    remote: remote_full,
                    local,
                    transfer_id: id,
                    total,
                    verify,
                });
            }
        })
        .detach();
}

/// 下载整个目录（递归）：选一个本地落点 到 传输队列 到 `SftpCmd::DownloadDir`。
///
/// 总量在服务端走完远端目录后经 `TransferMeta` 回填，所以这里 total 先给 0。
fn prompt_download_dir(remote_dir: String, tx: &flume::Sender<SftpCmd>, cx: &mut gpui::App) {
    let name = remote_dir
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("folder")
        .to_string();
    let default_dir = std::env::var("USERPROFILE")
        .map(|h| std::path::PathBuf::from(h).join("Downloads"))
        .unwrap_or_else(|_| std::path::PathBuf::from("."));
    let tx = tx.clone();
    let rx = cx.prompt_for_new_path(&default_dir, Some(&name));
    cx.background_executor()
        .spawn(async move {
            if let Ok(Ok(Some(local))) = rx.await {
                // 与单文件下载同一策略：不静默合并进已有目录，追加序号另开一个
                let mut local = local;
                if local.exists() {
                    let parent = local.parent().map(|p| p.to_path_buf()).unwrap_or_default();
                    let stem = local
                        .file_name()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_else(|| name.clone());
                    let mut n = 1;
                    loop {
                        let cand = parent.join(format!("{stem} ({n})"));
                        if !cand.exists() {
                            local = cand;
                            break;
                        }
                        n += 1;
                    }
                }
                let save_name = local
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| name.clone());
                let id = new_transfer_id();
                let _ = rustshell_core::sftp::sftp_events().0.send(
                    rustshell_core::sftp::SftpEvent::TransferQueued {
                        transfer_id: id,
                        name: rustshell_core::i18n::ts(
                            format!("下载文件夹 {save_name}"),
                            format!("Download folder {save_name}"),
                        ),
                        total: 0,
                    },
                );
                let _ = tx.try_send(SftpCmd::DownloadDir {
                    remote: remote_dir,
                    local,
                    transfer_id: id,
                });
            }
        })
        .detach();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::prod;
    use rustshell_core::theme::ThemeTokens;

    /// 每个大类都得落到**各自的**图标上，且兜底分支不能吞掉任何已登记的类型。
    ///
    /// 守的是「加了新扩展名、却写错/漏写分支」这类错：`entry_icon` 是纯函数，写错
    /// 不会 panic，只会安静地退化成通用文件图标 —— 界面上表现为「这一类没有图标」，
    /// 而编译器与其余用例都不会响。所以这里按**类别**各取一个代表逐个钉死。
    #[test]
    fn file_types_map_to_distinct_icons() {
        let t = ThemeTokens::dark();
        let cases = [
            ("a.rs", "icons/file-code.svg"),
            ("a.sh", "icons/file-terminal.svg"),
            ("a.json", "icons/file-braces.svg"),
            ("a.md", "icons/file-text.svg"),
            ("a.csv", "icons/file-spreadsheet.svg"),
            ("a.png", "icons/file-image.svg"),
            ("a.mp3", "icons/file-music.svg"),
            ("a.mp4", "icons/file-play.svg"),
            ("a.zip", "icons/file-archive.svg"),
            ("a.db", "icons/database.svg"),
            ("a.pem", "icons/file-key.svg"),
            ("a.ttf", "icons/file-type.svg"),
            ("a.exe", "icons/file-box.svg"),
            // 未登记类型与无扩展名都退回通用图标
            ("a.unknown", "icons/file.svg"),
            ("noext", "icons/file.svg"),
        ];
        for (name, want) in cases {
            let (got, _) = entry_icon(name, &t);
            assert_eq!(got, want, "`{name}` 应当用 {want}");
        }
    }

    /// 分类必须是**扩展名大小写无关**的：远端目录里 `README.MD`、`Photo.JPG` 很常见，
    /// 按原样匹配会让这些文件掉进兜底、和一堆陌生格式看起来完全一样。
    #[test]
    fn file_type_lookup_ignores_extension_case() {
        let t = ThemeTokens::dark();
        assert_eq!(entry_icon("README.MD", &t).0, "icons/file-text.svg");
        assert_eq!(entry_icon("Photo.JPG", &t).0, "icons/file-image.svg");
        assert_eq!(entry_icon("Data.CSV", &t).0, "icons/file-spreadsheet.svg");
    }

    /// 多点扩展名取**最后一段**：`.tar.gz` 归到 `gz`（仍是归档），正是想要的语义；
    /// 若哪天改成取第一段，这里会立刻红。
    #[test]
    fn multi_part_extension_uses_the_last_segment() {
        let t = ThemeTokens::dark();
        assert_eq!(entry_icon("pkg.tar.gz", &t).0, "icons/file-archive.svg");
        // `.ts` 是源码而不是视频，`a.d.ts` 也不该因为「取最后一段」而漏掉
        assert_eq!(entry_icon("a.d.ts", &t).0, "icons/file-code.svg");
    }

    /// 行展示缓存的内容口径（P0-2）：目录不留大小串（右侧用占位保持列宽），
    /// mtime 用的是**批量算好的那一份**而不是现算的。
    #[test]
    fn file_row_caches_display_strings() {
        // 目录这一份带上权限与属主：2026-09-22 起这两列也进 `FileRow`，
        // 顺手把「core 的格式化结果真的被搬进了行缓存」钉住（而不是渲染期现算）。
        let dir = rustshell_core::sftp::SftpEntry {
            permissions: Some(0o755),
            user: Some("root".into()),
            group: Some("root".into()),
            ..rustshell_core::sftp::SftpEntry::new("sub", true, 4096, 1_631_536_000)
        };
        let row = FileRow::new("/var", &dir, "09-13 12:26".into());
        assert_eq!(
            row.id.as_ref(),
            "file-/var/sub",
            "元素 id 要带完整路径才唯一"
        );
        assert_eq!(row.name.as_ref(), "sub");
        assert!(row.is_dir);
        assert_eq!(row.size.as_ref(), "", "目录不显示大小");
        assert_eq!(row.mtime.as_ref(), "09-13 12:26");
        assert_eq!(row.mode.as_ref(), "drwxr-xr-x", "目录的权限串首字符是 d");
        assert_eq!(row.owner.as_ref(), "root:root");

        let file = rustshell_core::sftp::SftpEntry {
            permissions: Some(0o644),
            ..rustshell_core::sftp::SftpEntry::new("a.log", false, 2048, 1_631_536_000)
        };
        let row = FileRow::new("/var", &file, "09-13 12:26".into());
        assert_eq!(row.id.as_ref(), "file-/var/a.log");
        assert_eq!(row.size.as_ref(), "2.0K");
        assert_eq!(row.mode.as_ref(), "-rw-r--r--");
        // 服务端不给权限/属主时留空串（不是 `----------`）：列宽由 `flex_none`
        // + 定宽守住，内容为空照样占位，两行不会错开。
        assert_eq!(row.owner.as_ref(), "");
        // 目录带尾斜杠时不能拼出 `//`
        assert_eq!(
            FileRow::new("/var/", &file, String::new()).id.as_ref(),
            "file-/var/a.log"
        );
    }

    /// P0-2 的**行为**护栏：虚拟列表真的只布局视口内的行。
    ///
    /// 用 5000 行渲染一次，量「列表上方标记」与「下方标记」的距离：虚拟化生效时
    /// 它等于容器高度；一旦哪天 `uniform_list` 被套进自适应高度的父容器
    /// （或又换回「v_flex 逐行 child」），这个距离会变成 5000 × 行高 ——
    /// 而功能上**完全看不出来**，只是大目录下每帧又开始布局上万行。
    #[gpui::test]
    fn file_list_only_lays_out_the_viewport(cx: &mut gpui::TestAppContext) {
        use gpui::{Context, Render, Window};

        struct ListProbe {
            rows: std::rc::Rc<Vec<FileRow>>,
            tx: flume::Sender<SftpCmd>,
        }

        impl Render for ListProbe {
            fn render(&mut self, _w: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
                let rows = self.rows.clone();
                let tx = self.tx.clone();
                v_flex()
                    .w(px(240.))
                    .h(px(400.))
                    .child(div().debug_selector(|| "probe-top".to_string()).h(px(1.)))
                    .child(
                        div().flex_1().min_h_0().w_full().overflow_hidden().child(
                            uniform_list(
                                "probe-list",
                                rows.len(),
                                move |range: std::ops::Range<usize>, _w, cx| {
                                    let t = crate::theme::current(cx);
                                    range
                                        .map(|ix| {
                                            file_row(&rows[ix], &tx, &t, true, false)
                                                .into_any_element()
                                        })
                                        .collect::<Vec<_>>()
                                },
                            )
                            .size_full(),
                        ),
                    )
                    .child(
                        div()
                            .debug_selector(|| "probe-bottom".to_string())
                            .h(px(1.)),
                    )
            }
        }

        cx.update(|cx| {
            gpui_component::init(cx);
            crate::theme::apply_theme_mode(cx, rustshell_core::theme::ThemeMode::Dark);
        });
        let (tx, _rx) = flume::unbounded::<SftpCmd>();
        let rows: std::rc::Rc<Vec<FileRow>> = std::rc::Rc::new(
            (0..5000)
                .map(|i| {
                    FileRow::new(
                        "/usr/bin",
                        &rustshell_core::sftp::SftpEntry::new(
                            format!("file-{i}"),
                            false,
                            i as u64,
                            1_631_536_000,
                        ),
                        "09-13 12:26".into(),
                    )
                })
                .collect(),
        );
        let (_, vcx) = cx.add_window_view(|_w, _cx| ListProbe {
            rows: rows.clone(),
            tx: tx.clone(),
        });
        vcx.run_until_parked();

        let top: f32 = vcx
            .debug_bounds("probe-top")
            .expect("找不到列表上方标记")
            .origin
            .y
            .into();
        let bottom: f32 = vcx
            .debug_bounds("probe-bottom")
            .expect("找不到列表下方标记")
            .origin
            .y
            .into();
        let gap = bottom - top;
        assert!(
            (gap - 398.0).abs() < 8.0,
            "虚拟列表把容器撑开了：上下标记相距 {gap}px，期望约 398px。\
             5000 行的总高是十万像素级 —— 出现这个数字说明行又被全量布局了。"
        );
    }

    /// 列对齐的**几何**护栏（2026-09-22 用户反馈「文件栏时间大小字段错位」）。
    ///
    /// 缺陷形态：行根节点没有 `w_full()` ⇒ 每行按内容收缩 ⇒ 名称上的 `flex_1`
    /// 吃不到剩余空间 ⇒「权限 / 属主 / 大小 / 时间」四列紧跟在名称后面、
    /// **每行一个位置**（截图里就是文件行的时间列与目录行的时间列错开）。
    /// 整件事布局合法、不报错、不 panic，只有把两类行的时间列各量一次才看得见。
    ///
    /// 面板宽度取**默认侧栏宽度**、`show_perm` 取 [`room_for_perm_cols`] 的结果：
    /// 于是这条用例同时守着「默认宽度下权限/属主两列真的画得出来」——
    /// 否则「增加权限和用户组」在默认界面上等于没做，而纯函数用例是发现不了的。
    ///
    /// 反向验证：①去掉 `file_row` 根节点的 `.w_full()`（两类行的时间列不再相等）；
    /// ②去掉权限列的 `.w(px(META_MODE_W))`（四列的间隔链对不上）。
    #[gpui::test]
    fn meta_columns_line_up_regardless_of_name_length(cx: &mut gpui::TestAppContext) {
        use gpui::{Context, Render, Window};

        const PANEL_W: f32 = FILES_WIDTH_DEFAULT;

        struct AlignProbe {
            rows: std::rc::Rc<Vec<FileRow>>,
            tx: flume::Sender<SftpCmd>,
            show_perm: bool,
        }

        impl Render for AlignProbe {
            fn render(&mut self, _w: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
                let rows = self.rows.clone();
                let tx = self.tx.clone();
                let show_perm = self.show_perm;
                v_flex().w(px(PANEL_W)).h(px(120.)).child(
                    uniform_list(
                        "align-list",
                        rows.len(),
                        move |range: std::ops::Range<usize>, _w, cx| {
                            let t = crate::theme::current(cx);
                            range
                                .map(|ix| {
                                    file_row(&rows[ix], &tx, &t, show_perm, false)
                                        .into_any_element()
                                })
                                .collect::<Vec<_>>()
                        },
                    )
                    .size_full(),
                )
            }
        }

        assert!(
            room_for_perm_cols(PANEL_W),
            "默认侧栏宽度({PANEL_W})下画不出权限/属主两列：阈值 `MIN_W_FOR_PERM_COLS`\
             ({MIN_W_FOR_PERM_COLS}) 定得比它高了 —— 那样默认界面上这两列永远不会出现。"
        );

        cx.update(|cx| {
            gpui_component::init(cx);
            crate::theme::apply_theme_mode(cx, rustshell_core::theme::ThemeMode::Dark);
        });
        let (tx, _rx) = flume::unbounded::<SftpCmd>();
        // 一行短名文件 + 一行长名目录：名称宽度差得越远，列错位越容易被量出来。
        // 两行都带上权限与属主 —— 这两列正是本次新增的定宽列，必须一起参与对齐断言。
        let rows: std::rc::Rc<Vec<FileRow>> = std::rc::Rc::new(vec![
            FileRow::new(
                "/root",
                &rustshell_core::sftp::SftpEntry {
                    permissions: Some(0o644),
                    user: Some("root".into()),
                    group: Some("root".into()),
                    ..rustshell_core::sftp::SftpEntry::new("a.txt", false, 0, 1_631_536_000)
                },
                "09-20 17:01".into(),
            ),
            FileRow::new(
                "/root",
                &rustshell_core::sftp::SftpEntry {
                    permissions: Some(0o755),
                    user: Some("root".into()),
                    group: Some("root".into()),
                    ..rustshell_core::sftp::SftpEntry::new(
                        "a-really-long-directory-name",
                        true,
                        4096,
                        1_631_536_000,
                    )
                },
                "09-20 17:01".into(),
            ),
        ]);
        let (_, vcx) = cx.add_window_view(|_w, _cx| AlignProbe {
            rows: rows.clone(),
            tx: tx.clone(),
            show_perm: room_for_perm_cols(PANEL_W),
        });
        vcx.run_until_parked();

        let file_time: f32 = vcx
            .debug_bounds("file-time-file")
            .expect("文件行的时间列没渲染出来")
            .origin
            .x
            .into();
        let dir_time: f32 = vcx
            .debug_bounds("file-time-dir")
            .expect("目录行的时间列没渲染出来")
            .origin
            .x
            .into();
        assert!(
            (file_time - dir_time).abs() < 0.5,
            "文件行与目录行的时间列不在同一条竖线上：文件行 x={file_time}，目录行 x={dir_time}。\
             这两列是定宽列，位置只该由面板宽度决定，不该跟着名称长度跑。"
        );
        let file_size: f32 = vcx
            .debug_bounds("file-size")
            .expect("文件行的大小列没渲染出来")
            .origin
            .x
            .into();
        // 两列相邻且顺序是「大小 在 时间 左边」，间隔就是行内的 `gap_2`（8px）。
        // 这条守的是 `file_row` 里那个决定：「目录行留同宽空盒，而不是把两列合并成
        // 一个宽块」—— 合并块会少一道 gap，两行名称的可用宽度随之差 8px。
        assert!(
            (file_time - file_size - (META_SIZE_W + 8.0)).abs() < 0.5,
            "「大小」列与「时间」列应紧邻（间隔 = 行内 gap_2 = 8px）：实测 size x={file_size}、\
             time x={file_time}，差 {}，期望 {}。",
            file_time - file_size,
            META_SIZE_W + 8.0
        );
        // 反向信号：两者也不是「一起漂到左边」。时间列的右缘必须贴住行的右缘，
        // 右边剩下的那点空白只能是行的内边距 + 外边距。
        //
        // 这里刻意**不写死**那个空白的具体像素（`px_3` + `mr_1` 会随样式调），
        // 要守的是「时间列由面板右缘定位、而不是由名称长度定位」这件事：
        // 缺陷形态下短名的文件行时间列会落在面板左半区，右边距大于面板的一半。
        let right_inset = PANEL_W - (file_time + META_TIME_W);
        assert!(
            (0.0..40.0).contains(&right_inset),
            "时间列没有贴住行的右缘：右缘还剩 {right_inset}px 空白（实测 x={file_time}）。\
             行根节点大概又按内容收缩了（缺 `w_full()`）。"
        );

        // 权限与属主两列（2026-09-22 新增）同样由右缘定位：从时间列往左一路量，
        // 每一跳都应当正好是「上一列的定宽常量 + 一道 gap_2（8px）」。
        // 这条同时钉住两件事：**列宽常量与实际渲染一致**（改了常量忘了改断言会红），
        // 以及**四列是同一条链上的定宽块**（任何一列变成内容宽度，这里立刻对不上）。
        let file_owner: f32 = vcx
            .debug_bounds("file-owner")
            .expect("文件行的属主列没渲染出来")
            .origin
            .x
            .into();
        let file_mode: f32 = vcx
            .debug_bounds("file-mode")
            .expect("文件行的权限列没渲染出来")
            .origin
            .x
            .into();
        assert!(
            (file_size - file_owner - (META_OWNER_W + 8.0)).abs() < 0.5,
            "「属主」列与「大小」列之间不是「定宽 + gap」：实测差 {}，期望 {}。",
            file_size - file_owner,
            META_OWNER_W + 8.0
        );
        assert!(
            (file_owner - file_mode - (META_MODE_W + 8.0)).abs() < 0.5,
            "「权限」列与「属主」列之间不是「定宽 + gap」：实测差 {}，期望 {}。",
            file_owner - file_mode,
            META_MODE_W + 8.0
        );
        // 光有「间隔等于常量 + gap」还不够：整条链一起缩放（甚至整行被拉伸）时，
        // 间隔仍是常量值，断言照样通过。所以再量一次**实际渲染宽度**——它必须等于
        // 常量本身，才说明这两列真的是定宽列、而不是碰巧排成了那个间距。
        let mode_w: f32 = vcx
            .debug_bounds("file-mode")
            .expect("文件行的权限列没渲染出来")
            .size
            .width
            .into();
        assert!(
            (mode_w - META_MODE_W).abs() < 0.5,
            "「权限」列的实际宽度 {mode_w} 与 `META_MODE_W`({META_MODE_W}) 不一致：\
             这一列大概被内容撑开或压缩了，权限串会跟着名称长度抖动。"
        );
        let owner_w: f32 = vcx
            .debug_bounds("file-owner")
            .expect("文件行的属主列没渲染出来")
            .size
            .width
            .into();
        assert!(
            (owner_w - META_OWNER_W).abs() < 0.5,
            "「属主」列的实际宽度 {owner_w} 与 `META_OWNER_W`({META_OWNER_W}) 不一致。"
        );
    }

    /// 接线护栏（2026-09-22 用户要求「新增目录需要弹出框输入目录名称，重命名也是」）。
    ///
    /// 纯函数测不到这条：`check_entry_name` 再严谨，只要入口还在**直接发命令**，
    /// 用户就依旧拿不到输入框 —— 而「目录建出来了」这件事在界面上与「用户自己起的
    /// 名字」长得一模一样，不报错、不报红。
    ///
    /// 断言写成 `m.open_sftp_mkdir_dialog(` 而不是裸方法名，是**反向验证逼出来的**：
    /// 把方法改名成 `…_v2` 时，裸名字的 `contains` 仍为真（前缀命中），护栏是哑的。
    /// 加上 `(` 之后才真的指「这里发生了一次调用」。同理，下面用
    /// 「面板里不许出现 `SftpCmd::Mkdir` / `SftpCmd::Rename`」来钉住
    /// **不许绕过对话框自己造命令**，而不是去数旧函数名 —— 名字可以换，行为不行。
    #[test]
    fn mkdir_and_rename_entries_go_through_a_name_dialog() {
        let panel = prod(include_str!("files_panel.rs"));
        let dialogs = prod(include_str!("main_view/dialogs.rs"));

        assert_eq!(
            panel.matches("m.open_sftp_mkdir_dialog(").count(),
            1,
            "「新建目录」必须（且只能）有一个入口，走收名字的对话框。"
        );
        assert_eq!(
            panel.matches("m.open_sftp_rename_dialog(").count(),
            1,
            "「重命名」必须（且只能）有一个入口，走收名字的对话框。"
        );
        assert!(
            !panel.contains("SftpCmd::Mkdir") && !panel.contains("SftpCmd::Rename"),
            "文件面板不该自己发 Mkdir / Rename：这两条命令必须由对话框收完名字后再发，\
             面板里出现构造点就意味着有个入口绕过了用户输入。"
        );
        assert!(
            !panel.contains("fn prompt_mkdir") && !panel.contains("fn prompt_rename"),
            "自动命名入口（prompt_mkdir / prompt_rename）不该复活：它们绕过用户输入。"
        );
        assert_eq!(
            dialogs.matches("check_entry_name(&name)").count(),
            2,
            "建目录与重命名都必须在发命令前先校验名字（两条路各一次）。"
        );
        assert!(
            dialogs.contains("fn sftp_mkdir_save(") && dialogs.contains("fn sftp_rename_save("),
            "对话框的落库函数不见了：按钮会变成「点了没反应」。"
        );
    }

    /// 接线护栏：「支持递归删除文件目录」不能只在 core 里成立。
    ///
    /// core 的 `removal_order` 有用例，但**发命令时传没传 `recursive: true`**
    /// 是另一回事。传 false 时 core 会退回 `RMDIR`（要求空目录），表现是
    /// 「非空目录删不掉」—— 功能看着在、实际没生效，且这句 flag 在界面上看不见。
    #[test]
    fn folder_delete_asks_the_core_for_a_recursive_remove() {
        let dialogs = prod(include_str!("main_view/dialogs.rs"));
        assert!(
            dialogs.contains("recursive: is_dir"),
            "删除目录时没有请求递归：core 会退回「只删空目录」，非空目录删不动。"
        );
        assert!(
            !dialogs.contains("recursive: false"),
            "界面侧不该出现「只删空目录」的删除路径（那是 core 的能力，不是这次的入口）。"
        );
        assert!(
            dialogs.contains("其中的所有文件与子目录都会被一并删除"),
            "确认框必须**提前**说明目录是连同内容一起删的 —— 递归删除的同意书。"
        );
    }

    /// 窄侧栏下要**收起**权限/属主两列（2026-09-22 随这两列一起加的兜底）。
    ///
    /// 为什么要收：两列一共吃掉 [`PERM_COLS_W`]（116px），而侧栏最小宽度只有
    /// `SIDEBAR_WIDTH_MIN`（180px）。无条件画的话，窄侧栏里文件名会被压到 0，
    /// 再窄还会把右侧列顶出可视区（`flex_none` 的子节点不收缩，超出部分被列表的
    /// `overflow_hidden` 裁掉）—— 列表的第一职责是看清名字，宁可少两列信息。
    ///
    /// 边界与「渲染结果」两条都要钉：阈值算错了纯函数用例看得见，但
    /// 「阈值说要收、行里却照样画」只有真渲染一次才暴露。
    #[gpui::test]
    fn perm_columns_are_dropped_when_the_panel_is_narrow(cx: &mut gpui::TestAppContext) {
        use gpui::{Context, Render, Window};

        // ---- 判定与算术 ----
        assert!(
            !room_for_perm_cols(MIN_W_FOR_PERM_COLS - 1.0),
            "差 1px 就该收起"
        );
        assert!(room_for_perm_cols(MIN_W_FOR_PERM_COLS));
        assert!(
            room_for_perm_cols(FILES_WIDTH_DEFAULT),
            "默认侧栏宽度下必须画得出来，否则这两列在默认界面上永远不会出现"
        );
        assert!(
            !room_for_perm_cols(rustshell_core::settings::SIDEBAR_WIDTH_MIN),
            "侧栏最小宽度下不该还挂着两列 116px 的信息"
        );
        // 契约：只要决定要画两列，文件名就必须还剩 `NAME_MIN_W` 的落脚处。
        // 这条正是阈值存在的理由 —— 它把「信息更多」与「名字看得清」的取舍写死了。
        for w in [0., 120., 180., 300., 365., 366., 380., 520.] {
            if room_for_perm_cols(w) {
                let name_w = name_column_width(w);
                assert!(
                    name_w >= NAME_MIN_W,
                    "面板宽 {w} 决定画权限列，却只给文件名剩 {name_w}px（下限 {NAME_MIN_W}）"
                );
            }
        }

        // ---- 渲染结果 ----
        struct NarrowProbe {
            rows: std::rc::Rc<Vec<FileRow>>,
            tx: flume::Sender<SftpCmd>,
            show_perm: bool,
        }
        impl Render for NarrowProbe {
            fn render(&mut self, _w: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
                let rows = self.rows.clone();
                let tx = self.tx.clone();
                let show_perm = self.show_perm;
                v_flex().w(px(FILES_WIDTH_DEFAULT)).h(px(120.)).child(
                    uniform_list(
                        "narrow-list",
                        rows.len(),
                        move |range: std::ops::Range<usize>, _w, cx| {
                            let t = crate::theme::current(cx);
                            range
                                .map(|ix| {
                                    file_row(&rows[ix], &tx, &t, show_perm, false)
                                        .into_any_element()
                                })
                                .collect::<Vec<_>>()
                        },
                    )
                    .size_full(),
                )
            }
        }

        cx.update(|cx| {
            gpui_component::init(cx);
            crate::theme::apply_theme_mode(cx, rustshell_core::theme::ThemeMode::Dark);
        });
        let (tx, _rx) = flume::unbounded::<SftpCmd>();
        let rows: std::rc::Rc<Vec<FileRow>> = std::rc::Rc::new(vec![FileRow::new(
            "/root",
            &rustshell_core::sftp::SftpEntry {
                permissions: Some(0o644),
                ..rustshell_core::sftp::SftpEntry::new("a.txt", false, 0, 1_631_536_000)
            },
            "09-20 17:01".into(),
        )]);

        let (_, narrow) = cx.add_window_view(|_w, _cx| NarrowProbe {
            rows: rows.clone(),
            tx: tx.clone(),
            show_perm: false,
        });
        narrow.run_until_parked();
        assert!(
            narrow.debug_bounds("file-mode").is_none()
                && narrow.debug_bounds("file-owner").is_none(),
            "收起时权限/属主两列不该出现在元素树里"
        );
        assert!(
            narrow.debug_bounds("file-size").is_some()
                && narrow.debug_bounds("file-time-file").is_some(),
            "收起的只是新增的两列：大小与时间必须还在"
        );

        let (_, wide) = cx.add_window_view(|_w, _cx| NarrowProbe {
            rows: rows.clone(),
            tx: tx.clone(),
            show_perm: true,
        });
        wide.run_until_parked();
        assert!(
            wide.debug_bounds("file-mode").is_some(),
            "打开时权限列必须真的画出来"
        );
    }

    /// F-13 验收要点 5（拖拽）：拖进来的本地文件**直接**变成上传命令，落点是当前远端目录。
    ///
    /// 为什么必须端到端：落点靠面板根上的 `can_drop` / `drag_over` / `on_drop` 三件套，
    /// 「挂对了没有」只有真派发一次平台级 `FileDropEvent` 才看得见 —— 挂到某个更小的
    /// 子盒子上、或者干脆漏挂，用户侧都是「拖进来什么都没发生」，而把 `enqueue_uploads`
    /// 单独拿来调永远发现不了。
    ///
    /// 反向验证：①删掉根上的 `.on_drop::<ExternalPaths>` ⇒ 一条命令都没有；
    /// ②把 `drop_uploads` 里的 `dir` 换成常量 ⇒ 落点目录断言红；
    /// ③删掉 `enqueue_uploads` 里 `f.is_dir()` 那段跳过 ⇒「只该有一条命令」红。
    #[gpui::test]
    fn dropping_local_files_uploads_into_the_current_directory(cx: &mut gpui::TestAppContext) {
        use crate::main_view::{AppShell, MainView, PanelKind, TabEntry};
        use gpui::{AppContext as _, FileDropEvent};
        use rustshell_core::session::{SessionHandle, SessionKind, SessionState};

        // 提示文案按当前语言取，而语言在测试进程里是全局的（别的用例可能刚切过 EN）：
        // 本用例断言中文那份，先自己钉住。
        rustshell_core::i18n::set_lang(rustshell_core::i18n::Lang::Zh);

        cx.update(|cx| {
            gpui_component::init(cx);
            crate::theme::apply_theme_mode(cx, rustshell_core::theme::ThemeMode::Dark);
        });

        // 与生产同构的窗口：Root → AppShell → MainView（同 `main_view::dead_session_e2e`）。
        let slot: std::rc::Rc<std::cell::RefCell<Option<gpui::Entity<MainView>>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let (_, vcx) = {
            let slot = slot.clone();
            cx.add_window_view(move |window, cx| {
                let main = cx.new(|cx| MainView::new(window, cx));
                *slot.borrow_mut() = Some(main.clone());
                let shell = cx.new(|_| AppShell::new(main));
                gpui_component::Root::new(shell, window, cx)
            })
        };
        vcx.run_until_parked();
        let main = slot.borrow().clone().expect("MainView 未创建");

        // 假会话：只填 SFTP 发送端。本用例量的是「拖进来的路径变成了什么命令」，
        // 不需要真连接；但没有这个发送端，落点会走「需要 SSH 会话」那条分支。
        let (tx, rx) = flume::unbounded::<SftpCmd>();
        vcx.update(|_w, cx| {
            main.update(cx, |m, cx| {
                let mut session = SessionHandle::pending(
                    "drop-e2e".to_string(),
                    SessionKind::Ssh,
                    "drop".to_string(),
                    None,
                );
                session.state = SessionState::Connected;
                session.sftp_tx = Some(tx);
                m.tabs_state.tabs.push(TabEntry {
                    session,
                    terminal: None,
                    scroll: None,
                    connect_error: None,
                    connect_started: None,
                    dead: None,
                    // 假的传输面板 e2e 标签：不挂重连账本（它根本没有会话）
                    retry: None,
                });
                m.tabs_state.active = Some("drop-e2e".to_string());
                m.panel_ui.panel = PanelKind::Files;
                m.io.files_path = "/srv/app".to_string();
                cx.notify();
            });
        });
        vcx.run_until_parked();

        // 量坐标前强制出一帧：`debug_bounds` 与命中测试读的都是**上一帧**的盒表，
        // 光 `run_until_parked` 不保证「刚设成文件面板」已经落到帧上。
        vcx.update(|window, cx| {
            window.draw(cx).clear(cx);
        });
        vcx.run_until_parked();
        let panel = vcx
            .debug_bounds("files-panel")
            .expect("文件面板没有进渲染帧");
        assert!(
            f32::from(panel.size.width) > 0. && f32::from(panel.size.height) > 0.,
            "文件面板尺寸为零，落点等于不存在：{panel:?}"
        );
        let at = panel.center();

        // 真本地路径：一个文件 + 一个目录。目录 v1 不支持递归，必须跳过并给提示。
        let base = std::env::temp_dir().join(format!("rustshell-drop-e2e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("sub")).expect("建临时目录");
        let file = base.join("hello.txt");
        std::fs::write(&file, b"hello").expect("写临时文件");
        let folder = base.join("sub");

        let drop_now = |vcx: &mut gpui::VisualTestContext,
                        at: gpui::Point<gpui::Pixels>,
                        paths: Vec<std::path::PathBuf>| {
            vcx.simulate_event(FileDropEvent::Entered {
                position: at,
                paths: ExternalPaths(paths.into()),
            });
            vcx.run_until_parked();
            vcx.simulate_event(FileDropEvent::Submit { position: at });
            vcx.run_until_parked();
        };
        drop_now(vcx, at, vec![file.clone(), folder.clone()]);

        // ① 目录被跳过、文件变成一条上传命令，落点是**当前**面板目录
        let mut cmds = Vec::new();
        while let Ok(c) = rx.try_recv() {
            cmds.push(c);
        }
        assert_eq!(
            cmds.len(),
            1,
            "只该有一条上传命令（目录必须跳过），实际 {} 条",
            cmds.len()
        );
        match &cmds[0] {
            SftpCmd::Upload {
                local,
                remote,
                total,
                ..
            } => {
                assert_eq!(local, &file, "上传的不是拖进来的那个本地文件");
                assert_eq!(
                    remote, "/srv/app/hello.txt",
                    "落点目录必须是面板**当前**的远端目录"
                );
                assert_eq!(*total, 5, "总字节数应当取本地文件长度");
            }
            _ => panic!("拖进来的文件没有变成 `SftpCmd::Upload`"),
        }

        // ② 跳过的目录要给出可见提示（静默跳过会让人以为已经传过去了）
        let toast = main.read_with(vcx, |m, _| m.ai_ui.quick_toast.clone());
        assert!(
            toast.as_deref().is_some_and(|s| s.contains("拖入文件夹")),
            "拖入目录必须给提示，实际提示：{toast:?}"
        );

        // ③ 没有 SFTP 会话时同样不许静默忽略：面板照收拖拽，但要说清为什么没传
        vcx.update(|_w, cx| {
            main.update(cx, |m, cx| {
                m.tabs_state.tabs[0].session.sftp_tx = None;
                cx.notify();
            });
        });
        vcx.run_until_parked();
        drop_now(vcx, at, vec![file.clone()]);
        assert!(rx.try_recv().is_err(), "没有 SFTP 会话时不该发出上传命令");
        let toast = main.read_with(vcx, |m, _| m.ai_ui.quick_toast.clone());
        assert!(
            toast
                .as_deref()
                .is_some_and(|s| s.contains("需要 SSH 会话")),
            "没有会话时拖入文件必须给提示（静默忽略会被当成「拖拽坏了」），\
             实际提示：{toast:?}"
        );

        let _ = std::fs::remove_dir_all(&base);
    }
}
