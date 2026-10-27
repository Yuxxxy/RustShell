//! 独立 SFTP 标签页（2026-10-26 起，参考 since2006/shell-rs 的 SFTP 标签页）。
//!
//! 形态是 **WinSCP Commander 式双窗格**：左边本地磁盘、右边远端服务器，
//! 中间一条可拖拽的分隔线（官方 `h_resizable`，拖动改变两栏宽度并记住比例）。
//!
//! # 与左侧「文件」面板（`files_panel`）的关系
//!
//! 两者**不是同一份东西**：
//! - `files_panel` 是终端会话的附属（复用该会话的 SFTP 通道，跟着当前标签走）；
//! - 本标签页是一条**独立**的单通道 SFTP 会话（`SshChannels::SftpOnly`）。
//!   服务器上不需要 shell，堡垒机那种只放 SFTP 的环境也能用 —— 与 shell-rs 同一取舍。
//!
//! # 结构（2026-10-27 按用户反馈调整）
//!
//! ```text
//! ┌ [本地路径 输入框] ⟳ ┬ [远端路径 输入框] ⟳ ┐   ← 地址栏在栏内，分隔线顶到最上面
//! ├ 名称 ▾ │ 大小 ▾ │ 修改时间 ▾ ┬ 名称 ▾ │ 大小 ▾ │ 修改时间 ▾ │ 权限 │ 所有者 ┤
//! │ ...                          │ │ ...                                      │
//! ├──────────────── 传输队列（最近 4 条）────────────────────────────────────┤
//! ```
//!
//! - 两侧路径都是**可编辑输入框**（回车跳转、失焦还原、角是直角）；
//! - 地址栏与列头都**在各自那一栏里**：中间那条分隔线因此能一直顶到顶边
//!   （2026-10-27 用户要求「拖动线要延伸到地址栏」）；
//! - 每条地址栏**只有一个刷新按钮**，上传 / 下载图标已撤（同一轮用户要求），
//!   能力改挂右键菜单：本地「上传到远端」、远端「下载到本地」；
//! - 列头可点：名称 / 大小 / 修改时间 三列排序，再点同列翻转方向（两侧共用一套口径）；
//! - 远端右键：下载到本地 / 下载… / 打开 / 重命名 / 删除 / 新建文件 / 新建文件夹 / 复制路径；
//!   本地右键：上传到远端 / 新建文件 / 新建文件夹 / 重命名。
//!
//! # 2026-10-27 第三轮（用户四点要求）
//!
//! 1. **传输可取消**：底部队列每条还在跑的行右侧一枚 ✕（`sftp_cancel_transfer`）；
//!    点了立刻变灰显示「取消中…」，core 回报终态后落成「失败/完成」。
//! 2. **传完刷新远端**：`SftpCmd::Upload` 在 core 里发 `SftpEvent::Refresh`，
//!    本视图读的 `files_rows` 由 `poll` 重扫时更新（本文件不自己发 `List`）。
//! 3. **点别处失焦**：两条路径框各挂一层 `on_mouse_down_out`（点列表行也算「别处」），
//!    草稿随之还原成真实目录。
//! 4. **提示走消息框**：本视图不再画错误行（`hint_line` 只剩「正在读取…」这种状态字）。
//!
//! 圆角与底色由外层窗格提供，本视图只画内容。

use std::path::{Path, PathBuf};

use gpui::{
    div, prelude::FluentBuilder as _, px, AnyElement, Context, InteractiveElement as _,
    IntoElement, ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _,
};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    h_flex,
    input::Input,
    menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem},
    resizable::{h_resizable, resizable_panel},
    scroll::ScrollableElement as _,
    v_flex, Icon, Sizable as _,
};

/// 全量 Lucide 目录（`gpui_component::IconName` 只是默认子集）。
use gpui_kit_assets::IconName as LucideIcon;

use rustshell_core::sftp::SftpCmd;

use super::main_view::MainView;
use crate::theme::to_hsla as hex;

// ── 排序 ──────────────────────────────────────────────────────────────────

/// 可排序的列（用户要求：时间 / 名称 / 大小）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Name,
    Size,
    Time,
}

/// 排序口径：列 + 方向。两侧窗格**共用一套** —— 左边按大小排、右边按名字排，
/// 对照浏览时就没法比了。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SortSpec {
    pub key: SortKey,
    pub asc: bool,
}

impl Default for SortSpec {
    fn default() -> Self {
        Self {
            key: SortKey::Name,
            asc: true,
        }
    }
}

impl SortSpec {
    /// 点同一列翻转方向，点别的列换成那一列并从升序开始。
    pub fn toggled(self, key: SortKey) -> Self {
        if self.key == key {
            Self {
                key,
                asc: !self.asc,
            }
        } else {
            Self { key, asc: true }
        }
    }
}

/// 一行本地文件。
#[derive(Debug, Clone)]
pub struct LocalRow {
    pub name: SharedString,
    pub path: PathBuf,
    pub is_dir: bool,
    /// 大小展示串（目录为空串，与远端列表同口径）
    pub size: SharedString,
    /// 修改时间展示串
    pub mtime: SharedString,
    /// 原始字节数 / mtime 秒：**排序用**（展示串排序是错的：「9 KB」>「1 MB」）
    pub size_bytes: u64,
    pub mtime_secs: u64,
}

/// 读一个本地目录（不排序 —— 排序由渲染层按当前口径做）。
///
/// 失败返回 `(空列表, 原因)`：调用方把原因画在窗格里，而不是让它变成一张空目录
/// —— 「读不到」与「这里什么都没有」是两件事。
pub fn read_local_dir(path: &Path) -> (Vec<LocalRow>, Option<String>) {
    let mut rows: Vec<LocalRow> = Vec::new();
    let rd = match std::fs::read_dir(path) {
        Ok(rd) => rd,
        Err(e) => return (rows, Some(e.to_string())),
    };
    for ent in rd.flatten() {
        let Ok(meta) = ent.metadata() else { continue };
        let name = ent.file_name().to_string_lossy().to_string();
        let is_dir = meta.is_dir();
        let size_bytes = if is_dir { 0 } else { meta.len() };
        let mtime_secs = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        rows.push(LocalRow {
            name: SharedString::from(name),
            path: ent.path(),
            is_dir,
            size: if is_dir {
                SharedString::default()
            } else {
                SharedString::from(human_size(size_bytes))
            },
            mtime: SharedString::from(rustshell_core::sftp::format_mtime(mtime_secs)),
            size_bytes,
            mtime_secs,
        });
    }
    (rows, None)
}

/// 字节数 → 便于阅读的短串（与远端列表的 `format_size` 同量级）。
pub fn human_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if b < KB {
        return format!("{bytes} B");
    }
    if b < KB * KB {
        return format!("{:.1} KB", b / KB);
    }
    if b < KB * KB * KB {
        return format!("{:.1} MB", b / (KB * KB));
    }
    format!("{:.2} GB", b / (KB * KB * KB))
}

/// 远端路径的上一级（POSIX 语义，与 `SftpCmd::List` 的路径口径一致）。
///
/// core 里没有现成的：远端路径只是字符串，`/` 是唯一分隔符（服务器不会是 Windows），
/// 所以这里不必借 `Path` —— 拿 `Path` 处理会受本机平台分隔符影响。
pub fn remote_parent(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(ix) => trimmed[..ix].to_string(),
    }
}

/// 本地路径的上一级（到根就停在根上）。
fn parent_of(path: &Path) -> PathBuf {
    path.parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| path.to_path_buf())
}

/// 去掉 Windows 的 verbatim 前缀（`\\?\D:\迅雷下载` → `D:\迅雷下载`）。
///
/// 2026-10-27 用户反馈「D 盘目录显示成 `\\?\D:\迅雷下载`」：来源是
/// `std::fs::canonicalize` —— 它在 Windows 上返回的就是 verbatim 形式。
/// 那个前缀本来是给 Win32 API 绕开 `MAX_PATH` 用的，摆进地址栏既难看又像路径坏了。
///
/// `\\?\UNC\server\share` 是网络路径的 verbatim 形式，必须还原成 `\\server\share`：
/// 直接砍掉 `\\?\` 会剩下 `UNC\server\share`，那是条根本不存在的路径。
pub fn clean_local_path(s: &str) -> String {
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        s.to_string()
    }
}

/// 本机 home 目录（Windows 用 `USERPROFILE`，POSIX 用 `HOME`）。
///
/// 都没有就退到当前工作目录 —— 本地窗格必须有个能读的起点，空串会让
/// `read_dir("")` 直接失败，表现为「第一次打开 SFTP 标签就是一片报错」。
pub fn home_dir() -> String {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(|p| PathBuf::from(p).to_string_lossy().to_string())
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|p| p.to_string_lossy().to_string())
        })
        .unwrap_or_else(|| "/".to_string())
}

// ── 列宽 ──────────────────────────────────────────────────────────────────

/// 四列定宽（`flex_shrink_0`），名称列吃剩下的空间 —— 窄窗格里宁可把名字截断，
/// 也不要把结构性的列挤没。
const COL_SIZE: f32 = 74.;
const COL_MTIME: f32 = 88.;
const COL_MODE: f32 = 84.;
const COL_OWNER: f32 = 68.;

/// 一行远端条目的右键菜单构造器。
///
/// 用 `Box<dyn Fn>` 而不是泛型参数：`..` 那一行传的是 `None`，泛型在 `None`
/// 上推断不出具体闭包类型（E0283）。
type EntryMenu = Box<
    dyn Fn(
        PopupMenu,
        &mut gpui::Window,
        &mut gpui::Context<'_, PopupMenu>,
    ) -> PopupMenu,
>;

/// 渲染一条 SFTP 标签页（`session_id` 是这条单通道会话的 id）。
pub fn render(m: &mut MainView, session_id: &str, cx: &mut Context<MainView>) -> AnyElement {
    let t = crate::theme::current(cx);

    // 本地目录：按需读盘并缓存（渲染每帧都跑，`read_dir` 不能无脑重来）。
    let local_path = m.io.sftp_local_path.clone();
    if m.io.sftp_local_for.as_deref() != Some(local_path.as_str()) {
        let (rows, err) = read_local_dir(Path::new(&local_path));
        m.io.sftp_local_rows = rows;
        m.io.sftp_local_for = Some(local_path.clone());
        // 换了目录，旧的选中项就不属于这里了
        m.io.sftp_local_sel = None;
        // 读不到就说一句话（2026-10-27：不再在列表里画一行灰字 —— 那行字与目录行
        // 混在一起，既容易被当成条目，又会被后续刷新覆盖掉）。只在这里报，不每帧报。
        if let Some(err) = err {
            m.queue_notice(
                crate::notice::NoticeKind::Error,
                rustshell_core::i18n::ts(
                    format!("读取本地目录失败：{err}"),
                    format!("Failed to read local directory: {err}"),
                ),
            );
        }
    }

    let tx = m
        .tabs_state
        .tabs
        .iter()
        .find(|t| t.session.id == session_id)
        .and_then(|t| t.session.sftp_tx.clone());
    let remote_path = m.io.files_path.clone();
    let remote_rows = m.io.files_rows.clone();
    let remote_loading = m.io.files_loading;
    let local_sel = m.io.sftp_local_sel.clone();
    let remote_sel = m.io.sftp_remote_sel.clone();
    // 远端条目的右键菜单要现读设置（与文件面板同一口径）
    let verify = m.settings.sftp_verify_downloads;
    let sort = m.io.sftp_sort;

    // ---- 两条地址栏：**分别贴在各自那一栏的顶上** ----
    //
    // 2026-10-27 用户要求「拖动线要延伸到地址栏」：分隔线得与地址栏同高，
    // 所以地址栏不能再是一条横跨整宽的独立行 —— 它必须进各自那一栏里
    // （原来的布局是「整宽 head 行 + 下面才是两栏」，分隔线只能从 head 下沿开始）。
    //
    // 「两边地址栏都只保留一个刷新按钮，其他按钮移除」：上传 / 下载两枚图标撤下，
    // 能力改挂右键菜单（本地「上传到远端」、远端「下载到本地」）——
    // 入口从「先选行再点图标」变成「右键那一行」，少一层猜测，地址栏也腾出宽度给路径。
    let local_bar = h_flex()
        .w_full()
        .items_center()
        .gap_2()
        .px_2()
        .py_2()
        .border_b_1()
        .border_color(hex(t.border_subtle))
        .child(
            // 直角输入框（用户要求「不要圆角输入框」）：`Input` 的圆角来自主题
            // token，这里就地压到 0；两侧同款。
            //
            // 外层这层 `on_mouse_down_out` 是 2026-10-27 用户要求的「点文件之类的地方
            // 要失焦」：官方 `Input` 只在点到**另一个可聚焦元素**时才换焦点，
            // 点列表行（普通 div）不会松手 —— 焦点与草稿就都赖在路径框里。
            // `on_mouse_down_out` 只在「框外按下」时触发，所以挂在这个小包装上
            // （挂在整个标签页根上就永远不会触发：行也是「内部」）。
            div()
                .flex_1()
                .min_w_0()
                .on_mouse_down_out(cx.listener(|m, _e, w, cx| {
                    super::main_view::blur_input_on_outside_click(&m.io.sftp_local_path_input, w, cx)
                }))
                .child(
                    Input::new(&m.io.sftp_local_path_input)
                        .small()
                        .rounded(px(0.))
                        .w_full()
                        .prefix(
                            div().child(
                                Icon::new(LucideIcon::Folder)
                                    .xsmall()
                                    .text_color(hex(t.fg_muted)),
                            ),
                        ),
                ),
        )
        .child(
            Button::new("sftp-refresh-local")
                .ghost()
                .xsmall()
                .flex_shrink_0()
                .icon(LucideIcon::RotateCw)
                .tooltip(rustshell_core::i18n::t("刷新"))
                .on_click(cx.listener(|m, _e, _w, cx| {
                    // 本地没有「重新 List」这种命令：把行缓存作废，下一帧就重读盘。
                    m.io.sftp_local_for = None;
                    cx.notify();
                })),
        );

    let remote_bar = h_flex()
        .w_full()
        .items_center()
        .gap_2()
        .px_2()
        .py_2()
        .border_b_1()
        .border_color(hex(t.border_subtle))
        .child(
            // 与左栏同款：框外一按就松手（见上面 local_bar 的注释）
            div()
                .flex_1()
                .min_w_0()
                .on_mouse_down_out(cx.listener(|m, _e, w, cx| {
                    super::main_view::blur_input_on_outside_click(&m.io.sftp_path_input, w, cx)
                }))
                .child(
                    Input::new(&m.io.sftp_path_input)
                        .small()
                        .rounded(px(0.))
                        .w_full()
                        .prefix(
                            div().child(
                                Icon::new(LucideIcon::Globe)
                                    .xsmall()
                                    .text_color(hex(t.fg_muted)),
                            ),
                        ),
                ),
        )
        .child(
            Button::new("sftp-refresh-remote")
                .ghost()
                .xsmall()
                .flex_shrink_0()
                .icon(LucideIcon::RotateCw)
                .tooltip(rustshell_core::i18n::t("刷新"))
                .on_click(cx.listener(|m, _e, _w, cx| {
                    // 重扫当前目录。手输中的草稿要让位 —— 刷新就是「以真实目录为准」。
                    m.io.sftp_path_typed = false;
                    if let Some(tx) = m.active_sftp_tx() {
                        m.io.files_loading = true;
                        let _ = tx.try_send(SftpCmd::List {
                            path: m.io.files_path.clone(),
                        });
                    }
                    cx.notify();
                })),
        );

    // ---- 左：本地 ----
    let mut local_rows = v_flex()
        .id("sftp-local-list")
        .flex_1()
        .min_h_0()
        .w_full()
        .overflow_y_scrollbar();

    // 「..」永远在首行（根目录下也画，点了原地不动 —— 比让它消失更好找）
    let up = parent_of(Path::new(&local_path));
    local_rows = local_rows.child(local_row_of(
        "sftp-local-up".into(),
        LucideIcon::ArrowLeft,
        "..".into(),
        SharedString::default(),
        SharedString::default(),
        false,
        false,
        &t,
        cx.listener(move |m, _e, _w, cx| {
            m.sftp_local_enter(up.to_string_lossy().to_string(), cx);
        }),
        None,
    ));
    // 排序：用**引用**排（几百行时避免每帧 clone 整份行数据）
    let mut local_order: Vec<&LocalRow> = m.io.sftp_local_rows.iter().collect();
    sort_local(&mut local_order, sort);
    for row in local_order {
        let selected = local_sel.as_deref() == Some(row.path.to_string_lossy().as_ref());
        let path = row.path.clone();
        let path_str = path.to_string_lossy().to_string();
        let name = row.name.clone();
        let is_dir = row.is_dir;
        let dir_now = local_path.clone();
        let menu_dir = dir_now.clone();
        let menu_path = path_str.clone();
        let menu_name = name.to_string();
        let menu: Option<EntryMenu> = Some(Box::new(
            move |menu: PopupMenu,
                  _w: &mut gpui::Window,
                  _cx: &mut gpui::Context<'_, PopupMenu>| {
                local_entry_menu(menu, &menu_dir, &menu_path, &menu_name, is_dir)
            },
        ));
        local_rows = local_rows.child(local_row_of(
            SharedString::from(format!("sftp-local-{}", path_str)),
            if is_dir {
                LucideIcon::Folder
            } else {
                LucideIcon::File
            },
            name,
            row.size.clone(),
            row.mtime.clone(),
            is_dir,
            selected,
            &t,
            cx.listener(move |m, e: &gpui::ClickEvent, _w, cx| {
                // 双击目录 = 进入；单击 = 选中（上传按钮吃选中项）
                if is_dir && e.click_count() >= 2 {
                    m.sftp_local_enter(path_str.clone(), cx);
                } else {
                    m.io.sftp_local_sel = Some(path_str.clone());
                    cx.notify();
                }
            }),
            menu,
        ));
    }

    // ---- 右：远端 ----
    let mut remote_rows_el = v_flex()
        .id("sftp-remote-list")
        .flex_1()
        .min_h_0()
        .w_full()
        .overflow_y_scrollbar();
    if !remote_path.is_empty() {
        let parent = remote_parent(&remote_path);
        let tx_up = tx.clone();
        remote_rows_el = remote_rows_el.child(remote_row_of(
            "sftp-remote-up".into(),
            "..".into(),
            SharedString::default(),
            SharedString::default(),
            SharedString::default(),
            SharedString::default(),
            false,
            false,
            &t,
            cx.listener(move |m, _e, _w, cx| {
                if let Some(tx) = &tx_up {
                    let _ = tx.try_send(SftpCmd::List { path: parent.clone() });
                    m.io.files_loading = true;
                    m.io.sftp_path_typed = false;
                    cx.notify();
                }
            }),
            None,
        ));
    }
    let mut remote_order: Vec<&crate::files_panel::FileRow> = remote_rows.iter().collect();
    sort_remote(&mut remote_order, sort);
    for row in remote_order {
        let selected = remote_sel.as_deref() == Some(row.path.as_str());
        let path = row.path.clone();
        let name = row.name.clone();
        let is_dir = row.is_dir;
        let tx_open = tx.clone();
        let tx_menu = tx.clone();
        let dir_now = remote_path.clone();
        let menu_path = row.path.clone();
        let menu_name = row.name.to_string();
        let menu_dir = dir_now.clone();
        let menu: Option<EntryMenu> = tx_menu.map(|tx| {
            Box::new(
                move |menu: PopupMenu,
                      _w: &mut gpui::Window,
                      _cx: &mut gpui::Context<'_, PopupMenu>| {
                    // 远端菜单 = 「下载到本地」+ 文件面板那一份（下载 / 打开 / 重命名 /
                    // 删除 / 复制路径）+ 两条「新建」（文件面板的工具栏另有新建入口，
                    // 这里没有，而 SFTP 标签页只有右键这一条路）。
                    //
                    // 「下载到本地」放**第一条**：双窗格里这是最常按的一项。文件面板那条
                    // 「下载…」要你选保存位置 —— 这里左边正看着哪个目录就是想去的地方。
                    // 2026-10-27 地址栏撤掉下载图标后，它成了唯一入口。
                    let dl = menu_path.clone();
                    let menu = menu.item(
                        PopupMenuItem::new(rustshell_core::i18n::t("下载到本地")).on_click(
                            move |_, w, cx| {
                                let p = dl.clone();
                                super::main_view::ui_call(
                                    "",
                                    move |m, _id, _w, cx| m.sftp_download_path(p.clone(), cx),
                                    w,
                                    cx,
                                );
                            },
                        ),
                    );
                    let base_ok = crate::files_panel::build_entry_menu(
                        menu,
                        &tx,
                        &menu_path,
                        &dir_now,
                        &menu_name,
                        is_dir,
                        verify,
                    );
                    let mk_dir = menu_dir.clone();
                    let mk_file = menu_dir.clone();
                    base_ok.separator().item(
                        PopupMenuItem::new(rustshell_core::i18n::t("新建文件夹")).on_click(
                            move |_, w, cx| {
                                let base = mk_dir.clone();
                                super::main_view::ui_call(
                                    "",
                                    move |m, _id, w, cx| {
                                        m.open_sftp_mkdir_at(base.clone(), false, false, w, cx)
                                    },
                                    w,
                                    cx,
                                );
                            },
                        ),
                    )
                    .item(
                        PopupMenuItem::new(rustshell_core::i18n::t("新建文件")).on_click(
                            move |_, w, cx| {
                                let base = mk_file.clone();
                                super::main_view::ui_call(
                                    "",
                                    move |m, _id, w, cx| {
                                        m.open_sftp_mkdir_at(base.clone(), false, true, w, cx)
                                    },
                                    w,
                                    cx,
                                );
                            },
                        ),
                    )
                },
            ) as EntryMenu
        });
        remote_rows_el = remote_rows_el.child(remote_row_of(
            SharedString::from(format!("sftp-r-{}", row.path)),
            name,
            row.size.clone(),
            row.mtime.clone(),
            row.mode.clone(),
            row.owner.clone(),
            is_dir,
            selected,
            &t,
            cx.listener(move |m, e: &gpui::ClickEvent, _w, cx| {
                // 双击目录 = 进入；单击 = 选中（下载按钮吃选中项）
                if is_dir && e.click_count() >= 2 {
                    if let Some(tx) = &tx_open {
                        let _ = tx.try_send(SftpCmd::List { path: path.clone() });
                        m.io.files_loading = true;
                        m.io.sftp_path_typed = false;
                    }
                } else {
                    m.io.sftp_remote_sel = Some(path.clone());
                }
                cx.notify();
            }),
            menu,
        ));
    }
    if remote_loading {
        // 读取中仍然就地显示：它不是「提示」（没有说完就走的语义），
        // 而是一块**状态** —— 换成消息框反而每次进目录都闪一下。
        // 错误则一律走消息框（`SftpEvent::Error` 在 poll 里就已入队）。
        remote_rows_el = remote_rows_el.child(hint_line(
            rustshell_core::i18n::t("正在读取…").to_string(),
            &t,
        ));
    }

    // ---- 两栏 + 可拖拽分隔线 ----
    //
    // 官方 `h_resizable`：分隔线自带 hairline，hover 时变成可抓的胶囊。
    // 两栏都不给初始尺寸、各自 flex_grow_1 ⇒ 初始各占一半，拖过之后按比例记住。
    // `ResizablePanelGroup` 自己不是 `Styled`（没有 flex_1），用一个 div 吃掉剩余高度。
    //
    // **地址栏在栏内**：这是「分隔线延伸到地址栏」的实现方式 —— 分隔线的高度由
    // 两栏决定，栏顶包含地址栏，线自然就顶到最上面。
    let panes = div()
        .flex_1()
        .min_h_0()
        .w_full()
        .child(
            h_resizable("sftp-split")
                .child(
                    resizable_panel().child(
                        v_flex()
                            .h_full()
                            .w_full()
                            .child(local_bar)
                            .child(local_header(&t, sort, cx))
                            .child(local_rows),
                    ),
                )
                .child(
                    resizable_panel().child(
                        v_flex()
                            .h_full()
                            .w_full()
                            .child(remote_bar)
                            .child(remote_header(&t, sort, cx))
                            .child(remote_rows_el),
                    ),
                ),
        );

    // ---- 底部：传输队列（复用 core 的续传 / 校验，命令层本来就是同一套）----
    let queue = if m.io.files_transfers.is_empty() {
        None
    } else {
        let mut col = v_flex()
            .w_full()
            .gap_1()
            .px_3()
            .py_2()
            .border_t_1()
            .border_color(hex(t.border_subtle));
        // 只看最近几条：队列是「过程」，不是历史账本
        for tr in m.io.files_transfers.iter().rev().take(4) {
            let text = if tr.total > 0 {
                format!(
                    "{}  {} / {}",
                    tr.name,
                    human_size(tr.bytes),
                    human_size(tr.total)
                )
            } else {
                tr.name.clone()
            };
            let color = if tr.done && !tr.ok {
                t.danger
            } else if tr.done {
                t.success
            } else if tr.cancelling {
                t.fg_muted
            } else {
                t.fg_secondary
            };
            // 取消按钮只给「还在跑、且没在取消中」的行（2026-10-27 用户要求「支持取消任务」）。
            // 已结束的行放一枚按不动的 ✕ 只会让人误以为还能撤；取消中的行留着它则会连点。
            let id = tr.id;
            let show_cancel = !tr.done && !tr.cancelling;
            let note = if tr.cancelling && !tr.done {
                Some(rustshell_core::i18n::t("取消中…").to_string())
            } else {
                tr.note.clone()
            };
            col = col.child(
                h_flex()
                    .w_full()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(11.))
                            .text_color(hex(color))
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .child(SharedString::from(text)),
                    )
                    .when_some(note, |d, note| {
                        d.child(
                            div()
                                .flex_shrink_0()
                                .text_size(px(10.))
                                .text_color(hex(t.fg_muted))
                                .child(SharedString::from(note)),
                        )
                    })
                    .when(show_cancel, |d| {
                        d.child(
                            Button::new(SharedString::from(format!("sftp-cancel-{id}")))
                                .ghost()
                                .xsmall()
                                .flex_shrink_0()
                                .icon(LucideIcon::Close)
                                .tooltip(rustshell_core::i18n::t("取消任务"))
                                .on_click(cx.listener(move |m, _e, _w, cx| {
                                    m.sftp_cancel_transfer(id, cx);
                                })),
                        )
                    }),
            );
        }
        Some(col)
    };

    v_flex()
        .id("sftp-tab")
        .w_full()
        .h_full()
        .child(panes)
        .children(queue)
        .into_any_element()
}

/// 本地排序：目录永远在前，同类型内按 `spec`。
///
/// 「目录优先」不跟着方向翻转：降序里把目录甩到最下面，用户就得滚过几百个文件
/// 才找得着路，而浏览文件时目录正是最常点的。
fn sort_local(rows: &mut Vec<&LocalRow>, spec: SortSpec) {
    rows.sort_by(|a, b| {
        b.is_dir.cmp(&a.is_dir).then_with(|| {
            let ord = match spec.key {
                SortKey::Name => a
                    .name
                    .to_lowercase()
                    .cmp(&b.name.to_lowercase())
                    .then_with(|| a.name.cmp(&b.name)),
                SortKey::Size => a.size_bytes.cmp(&b.size_bytes),
                SortKey::Time => a.mtime_secs.cmp(&b.mtime_secs),
            };
            if spec.asc {
                ord
            } else {
                ord.reverse()
            }
        })
    });
}

/// 远端排序：与本地同一口径（目录优先 + 三列 + 方向）。
fn sort_remote(rows: &mut Vec<&crate::files_panel::FileRow>, spec: SortSpec) {
    rows.sort_by(|a, b| {
        b.is_dir.cmp(&a.is_dir).then_with(|| {
            let ord = match spec.key {
                SortKey::Name => a
                    .name
                    .to_lowercase()
                    .cmp(&b.name.to_lowercase())
                    .then_with(|| a.name.cmp(&b.name)),
                SortKey::Size => a.size_bytes.cmp(&b.size_bytes),
                SortKey::Time => a.mtime_secs.cmp(&b.mtime_secs),
            };
            if spec.asc {
                ord
            } else {
                ord.reverse()
            }
        })
    });
}

/// 列头（本地）：名称 / 大小 / 修改时间，三列都可点排序。
fn local_header(
    t: &rustshell_core::theme::ThemeTokens,
    sort: SortSpec,
    cx: &mut Context<MainView>,
) -> AnyElement {
    h_flex()
        .w_full()
        .items_center()
        .gap_2()
        .mx_2()
        .px_2()
        .py_1()
        .border_b_1()
        .border_color(hex(t.border_subtle))
        .child(head_cell(rustshell_core::i18n::t("名称"), None, Some(SortKey::Name), sort, t, cx))
        .child(head_cell(
            rustshell_core::i18n::t("大小"),
            Some(COL_SIZE),
            Some(SortKey::Size),
            sort,
            t,
            cx,
        ))
        .child(head_cell(
            rustshell_core::i18n::t("修改时间"),
            Some(COL_MTIME),
            Some(SortKey::Time),
            sort,
            t,
            cx,
        ))
        .into_any_element()
}

/// 列头（远端）：名称 / 大小 / 修改时间 / 权限 / 所有者。
///
/// 权限与所有者不参与排序（不点）；前三列点击切换排序。
fn remote_header(
    t: &rustshell_core::theme::ThemeTokens,
    sort: SortSpec,
    cx: &mut Context<MainView>,
) -> AnyElement {
    h_flex()
        .w_full()
        .items_center()
        .gap_2()
        .mx_2()
        .px_2()
        .py_1()
        .border_b_1()
        .border_color(hex(t.border_subtle))
        .child(head_cell(rustshell_core::i18n::t("名称"), None, Some(SortKey::Name), sort, t, cx))
        .child(head_cell(
            rustshell_core::i18n::t("大小"),
            Some(COL_SIZE),
            Some(SortKey::Size),
            sort,
            t,
            cx,
        ))
        .child(head_cell(
            rustshell_core::i18n::t("修改时间"),
            Some(COL_MTIME),
            Some(SortKey::Time),
            sort,
            t,
            cx,
        ))
        .child(head_cell(rustshell_core::i18n::t("权限"), Some(COL_MODE), None, sort, t, cx))
        .child(head_cell(
            rustshell_core::i18n::t("所有者"),
            Some(COL_OWNER),
            None,
            sort,
            t,
            cx,
        ))
        .into_any_element()
}

/// 一个列头格子：`width = None` 表示吃剩余空间（名称列）。
///
/// `sort_key = Some(..)` 时这格可点：同一列再点翻转方向，当前排序列显示 ▲/▼。
fn head_cell(
    label: &'static str,
    width: Option<f32>,
    sort_key: Option<SortKey>,
    sort: SortSpec,
    t: &rustshell_core::theme::ThemeTokens,
    cx: &mut Context<MainView>,
) -> AnyElement {
    let active = sort_key.is_some_and(|k| k == sort.key);
    let text = if active {
        format!("{label} {}", if sort.asc { "▲" } else { "▼" })
    } else {
        label.to_string()
    };
    let cell = h_flex()
        .items_center()
        .gap_1()
        .text_size(px(10.))
        .text_color(hex(if active { t.fg_primary } else { t.fg_muted }))
        .whitespace_nowrap()
        .child(SharedString::from(text));
    let cell = match width {
        Some(w) => cell.w(px(w)).flex_shrink_0(),
        None => cell.flex_1().min_w_0(),
    };
    // 可点的列头会变成 `Stateful<Div>`（`.id(..)` 的返回类型），与不可点的
    // 那个分支不是同一类型 —— 所以在这里各收各的 `AnyElement`。
    match sort_key {
        Some(key) => cell
            .id(SharedString::from(format!("sftp-sort-{key:?}-{active}")))
            .cursor_pointer()
            .on_click(cx.listener(move |m, _e, _w, cx| {
                m.io.sftp_sort = m.io.sftp_sort.toggled(key);
                cx.notify();
            }))
            .into_any_element(),
        None => cell.into_any_element(),
    }
}

/// 本地列表的一行：图标 + 名称 / 大小 / 修改时间。
fn local_row_of(
    id: SharedString,
    icon: LucideIcon,
    name: SharedString,
    size: SharedString,
    mtime: SharedString,
    is_dir: bool,
    selected: bool,
    t: &rustshell_core::theme::ThemeTokens,
    on_click: impl Fn(&gpui::ClickEvent, &mut gpui::Window, &mut gpui::App) + 'static,
    menu: Option<EntryMenu>,
) -> AnyElement {
    let row = row_base(id, selected, t)
        .child(icon_of(icon, is_dir, t))
        .child(name_cell(name, t))
        .child(fixed_cell(size, COL_SIZE, t))
        .child(fixed_cell(mtime, COL_MTIME, t))
        .on_click(on_click);
    match menu {
        Some(build) => row.context_menu(build).into_any_element(),
        None => row.into_any_element(),
    }
}

/// 远端列表的一行：图标 + 名称 / 大小 / 修改时间 / 权限 / 所有者，
/// 并挂该条目的右键菜单（下载 / 重命名 / 删除 / 新建… / 复制路径）。
fn remote_row_of(
    id: SharedString,
    name: SharedString,
    size: SharedString,
    mtime: SharedString,
    mode: SharedString,
    owner: SharedString,
    is_dir: bool,
    selected: bool,
    t: &rustshell_core::theme::ThemeTokens,
    on_click: impl Fn(&gpui::ClickEvent, &mut gpui::Window, &mut gpui::App) + 'static,
    menu: Option<EntryMenu>,
) -> AnyElement {
    let row = row_base(id, selected, t)
        .child(icon_of(
            if is_dir {
                LucideIcon::Folder
            } else {
                LucideIcon::File
            },
            is_dir,
            t,
        ))
        .child(name_cell(name, t))
        .child(fixed_cell(size, COL_SIZE, t))
        .child(fixed_cell(mtime, COL_MTIME, t))
        .child(fixed_cell(mode, COL_MODE, t))
        .child(fixed_cell(owner, COL_OWNER, t))
        .on_click(on_click);
    // `context_menu` 返回的是**另一种元素类型**（`ContextMenu<Stateful<Div>>`），
    // 没法塞进同一条链继续 `.child(..)`，所以在这里就把它收成 `AnyElement`。
    match menu {
        Some(build) => row.context_menu(build).into_any_element(),
        None => row.into_any_element(),
    }
}

/// 行的公共底座（内边距 / 圆角 / 选中与 hover 两档底色 / 手型光标）。
///
/// 选中与 hover 用**两个深浅**（与连接树同一口径，见 `ui/src/theme.rs`）：
/// 同色时「鼠标停在哪」与「哪条被选中」分不出来，而这一列的操作是按选中项发起的。
fn row_base(
    id: SharedString,
    selected: bool,
    t: &rustshell_core::theme::ThemeTokens,
) -> gpui::Stateful<gpui::Div> {
    h_flex()
        .id(id)
        .w_full()
        .items_center()
        .gap_2()
        .mx_2()
        .px_2()
        .py_1()
        .rounded(px(6.))
        .cursor_pointer()
        .when(selected, |d| d.bg(hex(t.accent).opacity(0.18)))
        .hover(|s| s.bg(hex(t.accent).opacity(0.08)))
}

fn icon_of(icon: LucideIcon, is_dir: bool, t: &rustshell_core::theme::ThemeTokens) -> AnyElement {
    Icon::new(icon)
        .xsmall()
        .text_color(hex(if is_dir { t.warning } else { t.fg_muted }))
        .into_any_element()
}

/// 名称列：吃剩余空间，单行 + 省略号（撑高行会压到相邻行）。
fn name_cell(name: SharedString, t: &rustshell_core::theme::ThemeTokens) -> AnyElement {
    div()
        .flex_1()
        .min_w_0()
        .text_size(px(12.))
        .text_color(hex(t.fg_primary))
        .whitespace_nowrap()
        .text_ellipsis()
        .child(name)
        .into_any_element()
}

/// 定宽列（大小 / 时间 / 权限 / 所有者）：不参与伸缩，窄窗格里宁可挤名称。
fn fixed_cell(text: SharedString, width: f32, t: &rustshell_core::theme::ThemeTokens) -> AnyElement {
    div()
        .w(px(width))
        .flex_shrink_0()
        .text_size(px(10.))
        .text_color(hex(t.fg_muted))
        .whitespace_nowrap()
        .text_ellipsis()
        .child(text)
        .into_any_element()
}

/// 一行灰色提示（读取中 / 读不到）。
fn hint_line(text: String, t: &rustshell_core::theme::ThemeTokens) -> AnyElement {
    div()
        .px_3()
        .py_1()
        .text_size(px(11.))
        .text_color(hex(t.fg_muted))
        .child(SharedString::from(text))
        .into_any_element()
}

/// 本地条目的右键菜单：上传到远端 / 新建文件夹 / 新建文件 / 重命名。
///
/// 「上传到远端」是 2026-10-27 地址栏撤掉上传图标后的**唯一**入口。
/// 只对文件出现：目录上传 v1 不做（`sftp_upload_path` 会明确报一句，但没必要
/// 摆一个点了必然失败的东西）。
///
/// 本地不做「删除」：删本地文件是不可逆的（本应用没有回收站接口），误点一次就没了 ——
/// 要删请用系统的文件管理器。
fn local_entry_menu(menu: PopupMenu, dir: &str, full: &str, name: &str, is_dir: bool) -> PopupMenu {
    let dir_mk = dir.to_string();
    let dir_file = dir.to_string();
    let from = full.to_string();
    let dir_ren = dir.to_string();
    let upload = full.to_string();
    let has_name = !name.is_empty() && !name.starts_with("..");
    menu.when(has_name && !is_dir, |m_| {
        m_.item(
            PopupMenuItem::new(rustshell_core::i18n::t("上传到远端")).on_click(move |_, w, cx| {
                let path = upload.clone();
                super::main_view::ui_call(
                    "",
                    move |m, _id, _w, cx| m.sftp_upload_path(path.clone(), cx),
                    w,
                    cx,
                );
            }),
        )
        .separator()
    })
    .item(
        PopupMenuItem::new(rustshell_core::i18n::t("新建文件夹")).on_click(move |_, w, cx| {
            let base = dir_mk.clone();
            super::main_view::ui_call(
                "",
                move |m, _id, w, cx| m.open_sftp_mkdir_at(base.clone(), true, false, w, cx),
                w,
                cx,
            );
        }),
    )
    .item(
        PopupMenuItem::new(rustshell_core::i18n::t("新建文件")).on_click(move |_, w, cx| {
            let base = dir_file.clone();
            super::main_view::ui_call(
                "",
                move |m, _id, w, cx| m.open_sftp_mkdir_at(base.clone(), true, true, w, cx),
                w,
                cx,
            );
        }),
    )
    .when(has_name, |m_| {
        m_.separator().item(
            PopupMenuItem::new(rustshell_core::i18n::t("重命名…")).on_click(move |_, w, cx| {
                let from = from.clone();
                let dir = dir_ren.clone();
                super::main_view::ui_call(
                    "",
                    move |m, _id, w, cx| {
                        m.open_sftp_rename_at(from.clone(), dir.clone(), true, w, cx)
                    },
                    w,
                    cx,
                );
            }),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 远端路径的上一级：POSIX 口径，只认 `/`。
    ///
    /// 这条纯函数是「`..` 往哪走」的唯一来源 —— 判错的表现是「点了上一级没反应」
    /// 或「跳到别的目录」，在界面上很难一眼看出来。
    #[test]
    fn remote_parent_is_posix_and_stops_at_root() {
        assert_eq!(remote_parent("/home/ubuntu"), "/home");
        assert_eq!(remote_parent("/home"), "/");
        assert_eq!(remote_parent("/"), "/", "根目录再上一级仍是根（原地不动）");
        assert_eq!(remote_parent("/home/ubuntu/"), "/home", "尾斜杠要忽略");
        assert_eq!(remote_parent("relative"), "/", "相对路径没有父目录时退回根");
    }

    /// 大小展示：三段进位，且不出现「1024.0 KB」这种跨档的写法。
    #[test]
    fn human_size_switches_units_at_the_right_points() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(1024), "1.0 KB");
        assert_eq!(human_size(1024 * 1024), "1.0 MB");
        assert_eq!(human_size(3 * 1024 * 1024 * 1024), "3.00 GB");
    }

    /// 排序口径：点同一列翻转方向，点别的列换列并从升序开始。
    #[test]
    fn sort_spec_toggles_direction_per_column() {
        let s = SortSpec::default();
        assert_eq!((s.key, s.asc), (SortKey::Name, true), "默认按名称升序");
        let s = s.toggled(SortKey::Name);
        assert!(!s.asc, "同一列再点一次 = 反向");
        let s = s.toggled(SortKey::Size);
        assert_eq!((s.key, s.asc), (SortKey::Size, true), "换列必须回到升序");
    }

    /// `\\?\` verbatim 前缀必须剥掉（2026-10-27 用户反馈「D 盘显示成 `\\?\D:\迅雷下载`」）。
    ///
    /// 它来自 `canonicalize`，只该出现在 Win32 API 的参数里；摆在地址栏里
    /// 既难看又像路径坏了。网络路径要还原成 `\\server\share` —— 砍成
    /// `UNC\server\share` 是另一条不存在的路径。
    #[test]
    fn clean_local_path_strips_the_windows_verbatim_prefix() {
        assert_eq!(clean_local_path(r"\\?\D:\迅雷下载"), r"D:\迅雷下载");
        assert_eq!(clean_local_path(r"\\?\C:\Users\x"), r"C:\Users\x");
        assert_eq!(
            clean_local_path(r"\\?\UNC\server\share"),
            r"\\server\share",
            "网络路径的 verbatim 形式要还原成 UNC，而不是留下 `UNC\\…`"
        );
        assert_eq!(clean_local_path(r"D:\迅雷下载"), r"D:\迅雷下载", "普通路径不动");
        assert_eq!(clean_local_path("/home/u"), "/home/u", "POSIX 路径不动");
    }

    /// 2026-10-27 第二轮：地址栏进栏内（分隔线顶到顶边）+ 每条地址栏只留刷新。
    ///
    /// 三条都是**布局层**的约定，破一条界面照样能跑：
    /// 地址栏若摆回整宽行，分隔线就只从它下沿开始（用户点名要改的那个）；
    /// 刷新按钮少一个，那一侧就没法重扫了；上传 / 下载图标若回来，
    /// 地址栏又会去跟路径抢宽度（能力已经挂在右键菜单上）。
    #[test]
    fn the_address_bars_live_inside_their_panes_with_one_refresh_each() {
        let code = crate::test_support::prod(include_str!("sftp_tab.rs"));

        // ① 两条地址栏都挂在**栏内**：`..child(local_bar)` 出现在两栏结构里
        assert!(
            code.contains(".child(local_bar)") && code.contains(".child(remote_bar)"),
            "地址栏必须在各自那一栏的第一个位置 —— 摆在栏外就成了一条横跨整宽的行，\
             中间那条分隔线只能从它下沿开始"
        );
        // 且地址栏排在列头之前（顺序即版式：地址栏在最上面）
        let local_bar = code.find(".child(local_bar)").unwrap();
        let local_header = code.find(".child(local_header(&t, sort, cx))").unwrap();
        let remote_bar = code.find(".child(remote_bar)").unwrap();
        let remote_header = code.find(".child(remote_header(&t, sort, cx))").unwrap();
        assert!(
            local_bar < local_header && remote_bar < remote_header,
            "地址栏必须在列头之上"
        );

        // ② 两侧各一枚刷新，且各自有独立 id（同一个 id 会让两枚按钮抢同一份状态）
        assert!(
            code.contains(r#"Button::new("sftp-refresh-local")"#)
                && code.contains(r#"Button::new("sftp-refresh-remote")"#),
            "两侧地址栏各要有自己的刷新按钮"
        );
        assert_eq!(
            code.matches(".icon(LucideIcon::RotateCw)").count(),
            2,
            "刷新图标应当恰好两枚（每侧一枚）"
        );

        // ③ 上传 / 下载图标已撤，但能力仍在右键菜单里
        assert!(
            !code.contains("sftp-upload\"") && !code.contains("sftp-download\""),
            "地址栏的上传 / 下载图标已按用户要求撤下"
        );
        assert!(
            code.contains(r#"t("上传到远端")"#) && code.contains(r#"t("下载到本地")"#),
            "撤图标不等于撤能力：上传 / 下载必须仍在右键菜单里"
        );
    }

    /// 2026-10-27 第三轮用户要求（四条，各自独立、少一条界面照样跑但不合要求）：
    /// 传输行能取消、点列表失焦、提示走消息框。
    ///
    /// 「上传后刷新远端」那一条在 core 侧（`upload_asks_the_ui_to_rescan_the_remote_directory`）
    /// —— 事件是 core 发的，UI 只负责按 Refresh 重扫。
    #[test]
    fn the_sftp_tab_wires_cancel_blur_and_toasts() {
        let code = crate::test_support::prod(include_str!("sftp_tab.rs"));

        // ① 取消：队列行的 ✕ 落到 `sftp_cancel_transfer`，且只在运行中的行上出现
        assert!(
            code.contains("m.sftp_cancel_transfer(id, cx)"),
            "传输队列没有取消入口 —— 用户要求「支持取消任务」"
        );
        assert!(
            code.contains(r#"t("取消任务")"#),
            "取消按钮缺 tooltip（一枚孤零零的 ✕ 看不出是干什么的）"
        );
        assert!(
            code.contains("let show_cancel = !tr.done && !tr.cancelling;"),
            "取消按钮的显示条件不对：已结束的行不该还能点，取消中的行不该能连点"
        );

        // ② 失焦：两个路径框各挂一处「框外按下就松手」
        assert_eq!(
            code.matches(".on_mouse_down_out(").count(),
            2,
            "两个路径框都必须在框外按下时失焦（用户要求「点击文件之类的地方要失焦」）"
        );
        assert!(
            code.contains("blur_input_on_outside_click(&m.io.sftp_path_input")
                && code.contains("blur_input_on_outside_click(&m.io.sftp_local_path_input"),
            "失焦必须走既有的 `blur_input_on_outside_click`（它只在焦点真在本框上时才收）"
        );

        // ③ 提示：列表里不再画错误行
        assert!(
            !code.contains("hint_line(err, &t)"),
            "文件列表里不得再有「一行灰字/红字」式的提示 —— 一律走消息框"
        );
        assert!(
            code.contains("m.queue_notice(") && code.contains("NoticeKind::Error"),
            "本地目录读取失败必须弹消息框（否则用户只看到一张空列表）"
        );
    }

    /// 新开 SFTP 标签时，左侧地址栏必须是本机 home，而不是空白框。
    ///
    /// 2026-10-27 用户反馈「新打开 sftp，左侧的本地路径不显示」：`InputState` 建出来
    /// 就是空串，而回填（`sftp_local_path_sync`）只在「进目录」时才排队 ——
    /// 首次打开没有任何人写它。
    #[test]
    fn the_local_path_box_starts_at_the_home_directory() {
        let code = crate::test_support::prod(include_str!("main_view.rs"));
        assert!(
            code.contains("InputState::new(window, cx).default_value(sftp_home.clone())"),
            "本地路径框没有初值 —— 新开标签时左边地址栏会是空的"
        );
        assert!(
            code.contains("sftp_local_path: sftp_home"),
            "状态里的本地路径与输入框初值必须同源（各自调一次 home_dir 只是碰巧相等）"
        );
    }

    /// 挂载点：窗格渲染必须在**取终端数据之前**把 SFTP 标签页分出去。
    ///
    /// 顺序反了不会编译失败、也不会 panic —— 表现是 SFTP 标签页里画出一张
    /// 「连接中」占位卡（`terminal.is_none()` 那条分支），双窗格整个不见。
    #[test]
    fn the_sftp_tab_is_dispatched_before_the_terminal_path() {
        let code = crate::test_support::prod(include_str!("main_view/split_actions.rs"));
        let at = code.find("fn render_term_pane(").expect("找不到窗格渲染函数");
        let body = &code[at..];
        let sftp = body
            .find("crate::sftp_tab::render(")
            .expect("窗格渲染没有分派到 SFTP 标签页 —— 右键「打开 SFTP」会落成一张占位卡");
        let term = body
            .find("tab.terminal.clone()")
            .expect("找不到终端数据取值点");
        assert!(
            sftp < term,
            "SFTP 的分派必须排在取终端数据之前，否则双窗格被占位卡顶掉"
        );
    }

    /// 2026-10-27 用户提的一整套结构性要求（少一条界面就退化，但都不会报错）：
    /// 两端路径可编辑且直角、中间可拖拽、列头带「所有者」、本地有修改时间、
    /// 三列可排序、右键有新建/重命名。
    #[test]
    fn the_sftp_tab_meets_the_2026_10_27_layout_contract() {
        let code = crate::test_support::prod(include_str!("sftp_tab.rs"));

        // ① 两端路径都是输入框，且都是直角
        assert!(
            code.contains("Input::new(&m.io.sftp_local_path_input)"),
            "本地路径不是可编辑输入框 —— 用户要求左右两边都能手改"
        );
        assert!(
            code.contains("Input::new(&m.io.sftp_path_input)"),
            "远端路径不是可编辑输入框"
        );
        assert_eq!(
            code.matches(".rounded(px(0.))").count(),
            2,
            "两个路径框都必须是直角（`.rounded(px(0.))`）"
        );

        // ② 中间可拖拽
        assert!(
            code.contains("h_resizable(") && code.contains("resizable_panel()"),
            "两栏之间必须是可拖拽的官方分隔线，而不是一条写死的 div"
        );

        // ③ 列头文案
        assert!(
            code.contains("t(\"所有者\")"),
            "远端列头应写「所有者」（用户 2026-10-27 要求改掉「属主」）"
        );
        assert!(
            code.contains("t(\"修改时间\")"),
            "列头必须包含修改时间（本地也要）"
        );

        // ④ 三列可排序
        for key in ["SortKey::Name", "SortKey::Size", "SortKey::Time"] {
            assert!(
                code.contains(key),
                "列头少了可排序列 {key} —— 用户要求按时间 / 名称 / 大小排序"
            );
        }

        // ⑤ 右键新建 / 重命名
        assert!(
            code.contains("t(\"新建文件夹\")") && code.contains("t(\"新建文件\")"),
            "右键菜单缺「新建文件夹 / 新建文件」"
        );
        assert!(
            code.contains("open_sftp_rename_at("),
            "右键菜单缺本地重命名入口"
        );
    }
}
