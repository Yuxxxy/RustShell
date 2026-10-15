//! 系统托盘（F-8）。
//!
//! ## 形态
//!
//! 托盘图标建在 **gpui 的主线程**上，不是独立线程。理由是把两边的消息循环对上：
//! gpui 在 Windows 上的 `run()` 是标准的 `GetMessageW(None) + TranslateMessage +
//! DispatchMessageW` 循环（`gpui-pre-windows` 的 `platform.rs`），`None` 意味着
//! **本线程全部窗口**的消息都会被派发 —— 托盘图标自己那个隐藏窗口的 `WndProc`
//! 因此照样能收到回调。相反，另起线程就得自己再写一个消息泵，还要面对
//! 「菜单回调里拿不到 `App`」的跨线程问题。规格 §1.8 里写的「独立线程 + 通道」
//! 是备选方案，这里按「主线程建 + 全局事件通道 + `poll` 消费」落地。
//!
//! 唯一要守住的是：`install()` 必须在消息循环**开始之前**调用（即 gpui 的
//! `run` 初始化闭包内），否则创建窗口这一步没人派发消息。
//!
//! ## 菜单事件怎么进 UI
//!
//! `muda` 把菜单点击投进一个进程级通道（[`drain_actions`] 里的 `MenuEvent::receiver`），
//! 所以回调里**不需要**访问 `App`：UI 侧在既有的 300ms `poll` 循环里取一次即可。
//! 这也让「托盘动作的唯一消费点」是个可护栏化的静态事实。
//!
//! ## 图标
//!
//! 直接用**应用图标**（`app/assets/rustshell.ico`，桌面快捷方式/任务栏显示的那份），
//! 见 [`app_icon`]。以前这里是程序化画的另一套图形，两处并存必然走样。
//!
//! ## 悬停提示为什么不交给 `TrayIcon::set_tooltip`
//!
//! 图标用固定 GUID 注册（`with_guid`），身份在通知区里落在 `guidItem` 上。
//! 而 `tray-icon` 0.25.1 的 `TrayIcon::set_tooltip` 组装 `NIM_MODIFY` 时**漏了**
//! `NIF_GUID`（同一文件里 `set_icon` / `set_tray_visible` 都调了 `apply_guid`，
//! 只有它没调）⇒ 通知区按 `(hWnd, uID)` 找这个图标 ⇒ 找不到 ⇒ 每次都 `E_FAIL`。
//! 症状是「提示永远停在建图标那一刻的初始值」，而且每轮推送都在日志里刷一行
//! `托盘提示更新失败`。所以提示由本模块**自己发通告**（`notify_icon` 子模块），
//! 身份仍然只用 GUID —— 既不退回 `uID` 身份（会丢用户「固定在任务栏」的偏好、
//! 也防不住幽灵图标），也不重建图标（会闪）。
//!
//! ## 非 Windows
//!
//! 依赖本身只在 Windows 目标启用（见 `platform/Cargo.toml`），本模块在其它平台上
//! 退化为「不可用 + 原因」。[`TrayUnavailable::Platform`] 会被设置面板原样标注 ——
//! 规格 §1.8 的边界态要求「托盘不可用时降级为普通关闭并标注」，不许静默。

/// 托盘菜单项 id（稳定 ASCII，落进 `muda` 的 `MenuId`；不要用文案当 id，
/// 换个界面语言就认不出来了）。
///
/// 2026-10-10：撤掉「新建终端」。用户在托盘（窗口多半正藏在托盘里）点了它，
/// 新标签页开在**看不见的窗口**里 —— 一个「点了没有任何反馈」的入口不是功能，
/// 是坑。终端入口仍在快捷键（`NewLocalShell`）与工具栏上，托盘只留
/// 「显示主窗口 / 退出」两条**语义与托盘本身相关**的动作。
pub const ID_SHOW: &str = "rustshell.tray.show";
pub const ID_QUIT: &str = "rustshell.tray.quit";

/// 托盘菜单动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayAction {
    /// 把主窗口从托盘里放回来（并抢前台）。
    ShowWindow,
    /// 退出进程。
    ///
    /// 2026-09-29 用户反馈「托盘右键退出失效，应该直接关掉进程，而不是界面提示」
    /// —— 原先「退出先过确认框」那条整条撤掉，这个变体现在就是**直连 `App::quit`**
    /// （落点在 `ui/src/main_view.rs` 的 `poll`，护栏
    /// `the_tray_quit_action_quits_the_process_directly` 盯着它）。
    Quit,
}

/// 托盘菜单项 id → 动作。认不出来的 id 一律忽略（同一进程里可能还有别人的菜单）。
pub fn action_of(menu_id: &str) -> Option<TrayAction> {
    match menu_id {
        ID_SHOW => Some(TrayAction::ShowWindow),
        ID_QUIT => Some(TrayAction::Quit),
        _ => None,
    }
}

/// 托盘建不起来的原因（是原因**类别**，不是给人看的句子 ——
/// 文案走 i18n 词条表，见 [`TrayAvailability::Unavailable`] 的消费方）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayUnavailable {
    /// 本平台没实现（当前只有 Windows 后端）。
    Platform,
    /// 系统拒绝创建（通知区不可用、被组策略禁止等）。细节只进日志。
    System,
}

/// 托盘可用性快照。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayAvailability {
    /// 图标已挂上，菜单可用。
    Ready,
    /// 没挂上，附原因。
    Unavailable(TrayUnavailable),
}

/// 托盘菜单/提示的文案。由 `ui` 侧用 `i18n::t()` 取好再传进来，
/// 本模块不碰文案表（否则 core 的 i18n 门禁 g2 会把词条算成孤立）。
#[derive(Debug, Clone)]
pub struct TrayLabels {
    pub tooltip: String,
    pub show: String,
    pub quit: String,
}

// ---------------------------------------------------------------------------
// 图标：直接用应用图标（app/assets/rustshell.ico）
// ---------------------------------------------------------------------------

/// 应用图标文件。桌面快捷方式、任务栏、资源管理器显示的都是这一份 ——
/// 托盘以前自己画一套（圆角蓝底 + 白色 `>`），结果与桌面图标是两副面孔；
/// 用户要的是「一处改、处处一致」，所以这里把同一个文件编进来。
///
/// 通知区图标**不读应用主题**这条仍然成立：图标自带深色圆角底，
/// 用户应用浅色、任务栏深色时也看得见（不是靠取主题色保证的）。
const APP_ICON_ICO: &[u8] = include_bytes!("../../app/assets/rustshell.ico");

/// 从应用 `.ico` 取一帧，解成直通道（非预乘）alpha 的 RGBA8，返回 `(宽, 高, 像素)`。
///
/// 取帧规则：**不小于 `want` 的最小一帧**优先，没有更大的就退到最大的那帧。
/// 通知区把小图缩下去还能看，把 16px 放大到 32px 就是糊的。
/// 文件坏 / 一帧都认不出来 → `None`，调用方按「托盘不可用」处理
/// （规格 §1.8：不静默 —— 本仓在图标这条线上踩过「名字对了文件不在，
/// 单测全绿、界面留白」的坑）。
fn app_icon(want: u32) -> Option<(u32, u32, Vec<u8>)> {
    decode_ico(APP_ICON_ICO, want)
}

/// 在 ICO 容器里挑帧并解码（`bytes` 抽成参数，好让「文件坏」这一路可单测）。
fn decode_ico(bytes: &[u8], want: u32) -> Option<(u32, u32, Vec<u8>)> {
    // ICONDIR：reserved(2) + type(2) + count(2)。type 必须是 1（图标，不是光标）。
    let head = bytes.get(0..6)?;
    if head[..4] != [0, 0, 1, 0] {
        return None;
    }
    let count = u16::from_le_bytes([head[4], head[5]]) as usize;

    let mut pick: Option<(u32, u32, &[u8])> = None;
    for i in 0..count {
        // ICONDIRENTRY（16 字节）：宽 / 高 / 色数 / 保留 + planes(2) + bpp(2)
        // + 字节数(4) + 偏移(4)。条目里的宽高**不看**（256 会被编码成 0），
        // 真尺寸以 DIB 头为准。
        let Some(entry) = bytes.get(6 + i * 16..6 + i * 16 + 16) else {
            break; // 目录自己就被截断了，后面的条目也不存在
        };
        let size = u32::from_le_bytes([entry[8], entry[9], entry[10], entry[11]]) as usize;
        let offset = u32::from_le_bytes([entry[12], entry[13], entry[14], entry[15]]) as usize;
        let Some(frame) = bytes.get(offset..offset.saturating_add(size)) else {
            continue; // 帧落在文件外：断掉的条目，换下一帧
        };
        let Some((w, h)) = dib_size(frame) else {
            continue; // 不是 32bpp 未压缩 DIB（如 PNG 帧）——本仓的 .ico 里没有这种
        };
        let better = match pick {
            None => true,
            Some((cur, _, _)) => match (cur >= want, w >= want) {
                (true, true) => w < cur,
                (false, true) => true,
                (true, false) => false,
                (false, false) => w > cur,
            },
        };
        if better {
            pick = Some((w, h, frame));
        }
    }
    let (w, h, frame) = pick?;
    Some((w, h, dib_to_rgba(frame, w, h)))
}

/// 32bpp 未压缩 DIB 帧的 `(宽, 高)`；其余形态一律 `None`。
///
/// BITMAPINFOHEADER 前 20 字节：biSize(4) / biWidth(4) / biHeight(4) /
/// biPlanes(2) / biBitCount(2) / biCompression(4)。图标帧的 `biHeight`
/// 是「像素行 + AND 掩码行」的**两倍**，缺了这份二倍关系就说明不是图标帧
/// （或文件被截断），宁可拒绝也别按一半高度解出一张错位的图。
fn dib_size(frame: &[u8]) -> Option<(u32, u32)> {
    let head = frame.get(0..20)?;
    let bi_size = u32::from_le_bytes([head[0], head[1], head[2], head[3]]);
    let w = i32::from_le_bytes([head[4], head[5], head[6], head[7]]);
    let h2 = i32::from_le_bytes([head[8], head[9], head[10], head[11]]);
    let planes = u16::from_le_bytes([head[12], head[13]]);
    let bits = u16::from_le_bytes([head[14], head[15]]);
    let comp = u32::from_le_bytes([head[16], head[17], head[18], head[19]]);
    if bi_size != 40 || planes != 1 || bits != 32 || comp != 0 || w <= 0 || h2 <= 0 || h2 % 2 != 0 {
        return None;
    }
    let (w, h) = (w as u32, h2 as u32 / 2);
    // 帧里必须真装得下这些像素，截断的数据不能靠越界读混过去。
    if frame.len() < 40 + (w as usize) * (h as usize) * 4 {
        return None;
    }
    Some((w, h))
}

/// 自底向上 BGRA 的 DIB 像素 → 自顶向下 RGBA8（托盘只吃这个）。
///
/// 32bpp 的 DIB 每行就是紧凑的 4 字节像素（`biSizeImage` 的 4 字节对齐
/// 只影响 1/4/8bpp 的调色板图），所以这里不需要额外补行。
fn dib_to_rgba(frame: &[u8], w: u32, h: u32) -> Vec<u8> {
    let (w, h) = (w as usize, h as usize);
    let mut out = vec![0u8; w * h * 4];
    for y in 0..h {
        let src = 40 + (h - 1 - y) * w * 4;
        for x in 0..w {
            let s = src + x * 4;
            let d = (y * w + x) * 4;
            out[d] = frame[s + 2];
            out[d + 1] = frame[s + 1];
            out[d + 2] = frame[s];
            out[d + 3] = frame[s + 3];
        }
    }
    out
}

// ---------------------------------------------------------------------------
// 开关：是否「关闭窗口 = 收进托盘」
// ---------------------------------------------------------------------------

use std::sync::atomic::{AtomicBool, Ordering};

static CLOSE_TO_TRAY: AtomicBool = AtomicBool::new(false);

/// 设置「关闭窗口时最小化到托盘」。
///
/// 存在的理由是**关窗守卫拿不到 `Settings`**：`on_window_should_close` 的回调
/// 跑在窗口过程里，那里既没有 `Context` 也没有设置快照，只有一个 `&mut App`。
/// 与其在回调里重读一遍配置文件（I/O + 拿到的还是磁盘上的旧值），
/// 不如让设置面板每次改动时把这一位**推**到这里 —— 守卫读的是最新事实。
pub fn set_close_to_tray(on: bool) {
    CLOSE_TO_TRAY.store(on, Ordering::Release);
}

/// 当前「关闭窗口时最小化到托盘」的取值。
pub fn close_to_tray() -> bool {
    CLOSE_TO_TRAY.load(Ordering::Acquire)
}

// ---------------------------------------------------------------------------
// 平台实现
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod imp {
    use super::{action_of, TrayAction, TrayAvailability, TrayLabels, TrayUnavailable};
    use std::cell::RefCell;
    use std::sync::OnceLock;

    /// 托盘图标的身份 id。
    const TRAY_ID: &str = "rustshell-main";
    /// 固定的身份 GUID（Windows 专用）：**一旦发布就不能改**。
    ///
    /// 通知区按 GUID 认图标，同 GUID 的重建会被系统当作「同一个图标」——
    /// 这既能让用户调整过的「隐藏/显示」偏好跨启动保留，也能避免
    /// 反复安装时留下「幽灵图标」（旧图标还在、点它没反应）。
    const TRAY_GUID: u128 = 0x8f42_1c6d_5a93_4e07_b1d8_35a6_9c2e_7f40;

    thread_local! {
        /// 图标必须**活到进程结束**：`TrayIcon` 是引用计数的，最后一个实例被
        /// drop 掉时系统就把它从通知区摘了。同时它只能在创建它的线程上操作，
        /// 所以钉在主线程的 thread_local 里而不是某个静态 `Mutex`。
        static TRAY: RefCell<Option<tray_icon::TrayIcon>> = const { RefCell::new(None) };
    }

    /// 最近一次安装的结果。用 `OnceLock` 而不是原子标志：装失败之后
    /// 那个失败**原因**要一直能被设置面板读到（规格 §1.8「标注」）。
    static STATUS: OnceLock<TrayUnavailable> = OnceLock::new();

    /// 悬停提示的自建通路（绕开 `tray-icon` 0.25.1 的缺陷，见模块文档）。
    ///
    /// 为什么不直接用 crate 的方法：它的 `set_tooltip` 发 `NIM_MODIFY` 时不带
    /// `NIF_GUID`，而我们的图标是 GUID 身份的，于是必然 `E_FAIL`。
    ///
    /// 为什么可以自己发：`NIF_GUID` 的语义就是「用 `guidItem` 认图标，忽略
    /// `hWnd`/`uID`」。实测（2026-10-10，`platform/tests/tray_tooltip_probe.rs`，
    /// 真通知区）：自建通告带 `NIF_GUID|NIF_TIP` 时，`hWnd` 传 `NULL`/真实句柄、
    /// `uID` 传 `0`/`1` **四组全部返回 TRUE**；而不带 `NIF_GUID` 的那条一律 FALSE。
    mod notify_icon {
        use std::ffi::c_void;

        /// `NIM_MODIFY`（shellapi.h）：修改已存在的图标。
        const NIM_MODIFY: u32 = 1;
        /// `NIF_TIP`：`sz_tip` 有效。
        const NIF_TIP: u32 = 0x4;
        /// `NIF_GUID`：`guid_item` 有效；**此时 `hWnd` 与 `uID` 被忽略**。
        const NIF_GUID: u32 = 0x20;

        /// `GUID`（guiddef.h）。字段顺序/宽度与 Windows 一致，不能改成 `u128`
        /// —— `GUID` 的字节序不是 u128 的内存序（高 32 位在前、随后两个 16 位、
        /// 最后 8 字节按**大端**排）。
        #[repr(C)]
        #[derive(Clone, Copy, Debug)]
        struct Guid {
            data1: u32,
            data2: u16,
            data3: u16,
            data4: [u8; 8],
        }

        /// `u128` → `GUID`（与 `windows-sys` 的 `GUID::from_u128` 同口径）。
        const fn guid_from_u128(v: u128) -> Guid {
            Guid {
                data1: (v >> 96) as u32,
                data2: ((v >> 80) & 0xffff) as u16,
                data3: ((v >> 64) & 0xffff) as u16,
                data4: (v as u64).to_be_bytes(),
            }
        }

        /// `NOTIFYICONDATAW`（shellapi.h，V4）。
        ///
        /// 手写而不是引 `windows-sys`：本 crate 的其它原生调用（`window_ctl`
        /// 的 `user32`、`secret` 的 `crypt32`）都是就地声明，保持一致；
        /// 布局由下面那条编译期断言钉死。
        #[repr(C)]
        struct NotifyIconDataW {
            cb_size: u32,
            hwnd: *mut c_void,
            uid: u32,
            u_flags: u32,
            u_callback_message: u32,
            h_icon: *mut c_void,
            sz_tip: [u16; 128],
            dw_state: u32,
            dw_state_mask: u32,
            sz_info: [u16; 256],
            u_timeout_or_version: u32,
            sz_info_title: [u16; 64],
            dw_info_flags: u32,
            guid_item: Guid,
            h_balloon_icon: *mut c_void,
        }

        /// 编译期布局自检（铁律 #96：比测试更硬）。
        ///
        /// 结构体写错的后果是「通告被系统拒绝」，而它在界面上只表现为
        /// 「悬停提示不更新」—— 极难查。这里钉死 x64 下的 V4 大小与三个
        /// 关键偏移（Windows 按 `cbSize` 校验版本，对不上就直接返 FALSE）。
        #[cfg(target_pointer_width = "64")]
        const _LAYOUT_OK: () = {
            use std::mem::{offset_of, size_of};
            assert!(size_of::<NotifyIconDataW>() == 976);
            assert!(offset_of!(NotifyIconDataW, hwnd) == 8);
            assert!(offset_of!(NotifyIconDataW, sz_tip) == 40);
            assert!(offset_of!(NotifyIconDataW, guid_item) == 952);
        };

        impl NotifyIconDataW {
            /// 一条「只改提示」的通告：GUID 身份 + `sz_tip`。
            ///
            /// `hWnd` / `uID` 填 `NULL` / `0`：`NIF_GUID` 下它们不参与匹配
            /// （实测四组组合都成功，见模块文档），填任何值都不影响正确性。
            fn for_tooltip(text: &str) -> Self {
                // szTip 上限 128 个 UTF-16 码元，**含**结尾的 NUL —— 超长截断
                // 而不是溢出（通知区本来也只显示前几十个字符）。
                let mut sz_tip = [0u16; 128];
                for (i, c) in text.encode_utf16().take(127).enumerate() {
                    sz_tip[i] = c;
                }
                Self {
                    cb_size: std::mem::size_of::<Self>() as u32,
                    hwnd: std::ptr::null_mut(),
                    uid: 0,
                    u_flags: NIF_TIP | NIF_GUID,
                    u_callback_message: 0,
                    h_icon: std::ptr::null_mut(),
                    sz_tip,
                    dw_state: 0,
                    dw_state_mask: 0,
                    sz_info: [0; 256],
                    u_timeout_or_version: 0,
                    sz_info_title: [0; 64],
                    dw_info_flags: 0,
                    guid_item: guid_from_u128(super::TRAY_GUID),
                    h_balloon_icon: std::ptr::null_mut(),
                }
            }
        }

        #[link(name = "shell32")]
        extern "system" {
            fn Shell_NotifyIconW(message: u32, data: *mut NotifyIconDataW) -> i32;
        }

        /// 把悬停提示推给通知区，返回系统是否接受（`FALSE` = 没找到这个图标）。
        pub fn set_tooltip(text: &str) -> bool {
            let mut nid = NotifyIconDataW::for_tooltip(text);
            // 调用方（`imp::set_tooltip`）保证此时托盘图标已经登记过，
            // 且本函数不在任何 `RefCell` 借用里跑（`Shell_NotifyIconW` 是同步的，
            // 返回前会给托盘窗口过程派消息）。
            unsafe { Shell_NotifyIconW(NIM_MODIFY, &mut nid) != 0 }
        }
    }

    pub fn install(labels: &TrayLabels) -> Result<(), TrayUnavailable> {
        if is_ready() {
            return Ok(());
        }
        let result = install_inner(labels);
        if let Err(reason) = result {
            let _ = STATUS.set(reason);
        }
        result
    }

    fn install_inner(labels: &TrayLabels) -> Result<(), TrayUnavailable> {
        use tray_icon::menu::{Menu, MenuItem, PredefinedMenuItem};
        use tray_icon::{Icon, TrayIconBuilder};

        let menu = Menu::new();
        let show = MenuItem::with_id(super::ID_SHOW, &labels.show, true, None);
        let quit = MenuItem::with_id(super::ID_QUIT, &labels.quit, true, None);
        let separator = PredefinedMenuItem::separator();
        menu.append_items(&[&show, &separator, &quit])
            .map_err(|e| {
                log::warn!("托盘菜单构建失败：{e}");
                TrayUnavailable::System
            })?;

        // 图标取的是**应用图标**（与桌面快捷方式/任务栏同源），交给
        // Shell_NotifyIcon 自己缩放到通知区大小（与改造前的 32×32 口径一致）。
        let (w, h, rgba) = super::app_icon(32).ok_or_else(|| {
            log::warn!("托盘图标读取失败：app/assets/rustshell.ico 里没有可用的 32bpp 帧");
            TrayUnavailable::System
        })?;
        let icon = Icon::from_rgba(rgba, w, h).map_err(|e| {
            log::warn!("托盘图标像素非法：{e}");
            TrayUnavailable::System
        })?;

        let tray = TrayIconBuilder::new()
            .with_id(TRAY_ID)
            .with_guid(TRAY_GUID)
            .with_menu(Box::new(menu))
            .with_icon(icon)
            .with_tooltip(&labels.tooltip)
            .build()
            .map_err(|e| {
                log::warn!("托盘创建失败：{e}");
                TrayUnavailable::System
            })?;

        TRAY.with(|slot| *slot.borrow_mut() = Some(tray));
        log::info!("系统托盘已就绪（菜单：显示主窗口 / 退出）");
        Ok(())
    }

    /// 取走积压的托盘菜单动作。
    ///
    /// 唯一消费点在 `MainView::poll`；`muda` 的事件通道是进程级的，这里 `try_recv`
    /// 到空为止 —— 一次点击最多积一个，不会出现「点了没反应要再点一次」。
    pub fn drain_actions() -> Vec<TrayAction> {
        let mut actions = Vec::new();
        let receiver = tray_icon::menu::MenuEvent::receiver();
        while let Ok(event) = receiver.try_recv() {
            if let Some(action) = action_of(&event.id.0) {
                actions.push(action);
            }
        }
        actions
    }

    /// 更新托盘提示（悬停时显示）。返回是否更新成功。
    ///
    /// 走 [`notify_icon`] 的自建通告，**不**用 `TrayIcon::set_tooltip`
    /// （它在 GUID 身份的图标上必然失败，见模块文档）。
    ///
    /// ⚠️ 这里**不能**在持有 `TRAY` 借用的同时发原生通告：`Shell_NotifyIconW`
    /// 是同步的，返回前会给托盘窗口过程派消息，那条路径一旦回头再 `borrow()`
    /// 一次 `TRAY` 就是双借 panic（与 `window_ctl::set_visible` 那次同类，
    /// 铁律：原生同步调用别待在 `RefCell` 借用里）。所以先判「图标在不在」
    /// （借用在语句内结束），再在**无借用**状态下发通告。
    pub fn set_tooltip(text: &str) -> bool {
        if !is_ready() {
            return false;
        }
        if notify_icon::set_tooltip(text) {
            return true;
        }
        log::warn!("托盘提示更新失败：Shell_NotifyIcon(NIM_MODIFY) 被通知区拒绝");
        false
    }

    pub fn is_ready() -> bool {
        TRAY.with(|slot| slot.borrow().is_some())
    }

    pub fn availability() -> TrayAvailability {
        if is_ready() {
            TrayAvailability::Ready
        } else {
            TrayAvailability::Unavailable(*STATUS.get().unwrap_or(&TrayUnavailable::System))
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::{TrayAction, TrayAvailability, TrayLabels, TrayUnavailable};

    /// 非 Windows 不做任何事，并把原因如实报给 UI。
    /// 这里**不**返回 `Ok`：返回 `Ok` 会让设置面板显示「托盘已启用」而实际什么都没有。
    pub fn install(_labels: &TrayLabels) -> Result<(), TrayUnavailable> {
        Err(TrayUnavailable::Platform)
    }

    pub fn drain_actions() -> Vec<TrayAction> {
        Vec::new()
    }

    pub fn set_tooltip(_text: &str) -> bool {
        false
    }

    pub fn is_ready() -> bool {
        false
    }

    pub fn availability() -> TrayAvailability {
        TrayAvailability::Unavailable(TrayUnavailable::Platform)
    }
}

pub use imp::{availability, drain_actions, install, is_ready, set_tooltip};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_menu_id_maps_to_its_own_action() {
        assert_eq!(action_of(ID_SHOW), Some(TrayAction::ShowWindow));
        assert_eq!(action_of(ID_QUIT), Some(TrayAction::Quit));
    }

    #[test]
    fn the_menu_is_exactly_show_quit() {
        // 2026-10-10 用户要求撤掉「新建终端」：它的落点
        // （`TrayAction::NewTerminal` → `open_local_shell`）在窗口收进托盘时会把
        // 标签开在**看不见的地方**——点了没有反馈的入口是坑。
        //
        // 这条用例钉住菜单的**全部**认领范围：除「显示主窗口 / 退出」之外，
        // 任何 id 都必须落到 `None`。谁想把第三项加回来，先得在这里说明白
        // 它跟托盘是什么关系（`action_of` 是唯一的认领表，改它就会被这条挡住）。
        for id in [
            "rustshell.tray.show",
            "rustshell.tray.quit",
            "rustshell.tray.new-terminal",
            "rustshell.tray.new_terminal",
            "rustshell.tray.restart",
        ] {
            let expected = match id {
                "rustshell.tray.show" => Some(TrayAction::ShowWindow),
                "rustshell.tray.quit" => Some(TrayAction::Quit),
                _ => None,
            };
            assert_eq!(action_of(id), expected, "托盘菜单认领范围变了：{id}");
        }
    }

    #[test]
    fn ids_are_distinct_and_stable_ascii() {
        // id 会落进 muda 的 `MenuId` 并跨帧比较；重复或含非 ASCII 会让
        // 「点了菜单没反应」变得极难查（不同语言下 id 还会变）。
        let all = [ID_SHOW, ID_QUIT];
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a, b, "托盘菜单 id 重复：{a}");
            }
            assert!(a.is_ascii(), "托盘菜单 id 必须是纯 ASCII：{a}");
            assert!(a.starts_with("rustshell.tray."), "id 归属前缀丢了：{a}");
        }
    }

    #[test]
    fn foreign_menu_ids_are_ignored() {
        // 同一进程里可能还有别人（或旧版本）的菜单项在广播事件，
        // 认不出来的必须丢掉，不能崩、也不能误当成某一项。
        for id in ["", "quit", "rustshell.tray", "RUSTSHELL.TRAY.QUIT"] {
            assert_eq!(action_of(id), None, "不该认领：{id:?}");
        }
    }

    #[test]
    fn close_to_tray_flag_round_trips() {
        set_close_to_tray(true);
        assert!(close_to_tray());
        set_close_to_tray(false);
        assert!(!close_to_tray());
    }

    #[test]
    fn the_app_icon_picks_the_smallest_frame_that_is_not_smaller_than_asked() {
        // 托盘问 32（通知区口径与改造前一致）。这张表同时钉住「尺寸选择」与
        // 「真能解开」：换 `want` 口径、或 `.ico` 里少了某个尺寸，都会先红在这里。
        // 300 这一行是「没有更大的 → 退到最大一帧」的边界。
        for (want, side) in [
            (16u32, 16u32),
            (24, 32),
            (32, 32),
            (48, 48),
            (100, 128),
            (300, 256),
        ] {
            let (w, h, px) = app_icon(want).unwrap_or_else(|| panic!("want={want} 取不到帧"));
            assert_eq!((w, h), (side, side), "want={want} 选错帧");
            assert_eq!(
                px.len(),
                (side * side * 4) as usize,
                "want={want} 像素数不对"
            );
        }
    }

    #[test]
    fn the_icon_corners_are_transparent_and_the_middle_is_opaque() {
        // 圆角方必须**真的**透明，否则通知区里会出现一块方方的贴片。
        let (w, h, px) = app_icon(32).expect("32px 帧");
        let alpha = |x: u32, y: u32| px[((y * w + x) * 4 + 3) as usize];
        for (x, y) in [(0, 0), (w - 1, 0), (0, h - 1), (w - 1, h - 1)] {
            assert_eq!(alpha(x, y), 0, "角点 ({x},{y}) 应当是透明的");
        }
        assert_eq!(alpha(w / 2, h / 2), 255, "中心应当不透明");
    }

    #[test]
    fn the_icon_keeps_its_blue_r_and_its_terminal_box() {
        // 桌面图标长这样：深色圆角底 + 蓝色「R」（左上为主）+ 浅色终端框（只在右下）。
        // 这些断言真正要挡的是「RGB 通道被当成 BGR 读」：读反了蓝色会变褐、
        // 浅色框会被判成蓝色 —— 只看「非透明像素有多少」是发现不了的。
        let (w, h, px) = app_icon(32).expect("32px 帧");
        let mut blue = 0u32;
        let mut blue_left = 0u32;
        let mut light = 0u32;
        let mut light_off_corner = 0u32;
        let mut dark = 0u32;
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                let (r, g, b, a) = (px[i], px[i + 1], px[i + 2], px[i + 3]);
                if a == 0 {
                    continue;
                }
                if b > r + 25 && b >= g + 25 && b > 90 {
                    blue += 1;
                    if x < w / 2 {
                        blue_left += 1;
                    }
                } else if r.min(g).min(b) > 140 {
                    light += 1;
                    if x < w / 2 || y < h / 2 {
                        light_off_corner += 1;
                    }
                } else if a == 255 && r.max(g).max(b) < 80 {
                    dark += 1;
                }
            }
        }
        let total = w * h;
        assert!(
            dark > total / 2,
            "深色底不见了（只有 {dark}/{total} 个深像素）"
        );
        assert!(blue > total / 16, "蓝色「R」不见了（只有 {blue} 个蓝像素）");
        assert!(blue < total / 4, "蓝色区域过大（{blue}/{total}）");
        assert!(
            blue_left * 2 > blue,
            "「R」应当在左半边（左边只有 {blue_left}/{blue}）"
        );
        assert!(light > 10, "终端框不见了（只有 {light} 个浅像素）");
        assert!(light < total / 8, "浅色区域过大（{light}/{total}）");
        assert_eq!(light_off_corner, 0, "终端框应当只在右下角");
    }

    #[test]
    fn a_broken_icon_yields_none_instead_of_a_blank_tray() {
        // 「读不出来就报 None，让调用方走『托盘不可用』」——不静默留白。
        assert!(decode_ico(&[], 32).is_none(), "空数据");
        assert!(decode_ico(b"not an ico", 32).is_none(), "不是图标");
        assert!(
            decode_ico(&[0, 0, 2, 0, 0, 0], 32).is_none(),
            "问号类型（光标）不该当图标"
        );

        // 声明了一帧、但帧数据被截断（头部还写着 32×32）。
        let mut truncated = vec![0u8, 0, 1, 0, 1, 0];
        truncated.extend_from_slice(&[32, 32, 0, 0, 1, 0, 32, 0]);
        truncated.extend_from_slice(&4000u32.to_le_bytes()); // 声称的字节数
        truncated.extend_from_slice(&22u32.to_le_bytes()); // 帧起点
        truncated.extend_from_slice(&[
            40, 0, 0, 0, 32, 0, 0, 0, 64, 0, 0, 0, 1, 0, 32, 0, 0, 0, 0, 0,
        ]);
        assert!(decode_ico(&truncated, 32).is_none(), "截断的帧");

        // 只有 PNG 帧的 .ico：PNG 不像 DIB 那样自带未压缩像素，本仓也不为它引 zlib。
        // 真换成了 PNG 帧，这里红掉（并伴随托盘日志）比通知区里显示一坨乱码强。
        let mut png_only = vec![0u8, 0, 1, 0, 1, 0];
        png_only.extend_from_slice(&[0, 0, 0, 0, 1, 0, 32, 0]);
        png_only.extend_from_slice(&8u32.to_le_bytes()); // 声称的字节数
        png_only.extend_from_slice(&22u32.to_le_bytes()); // 帧起点
        png_only.extend_from_slice(b"\x89PNG\r\n\x1a\n");
        assert!(
            decode_ico(&png_only, 32).is_none(),
            "PNG 帧不该被当成像素读"
        );
    }

    #[test]
    fn the_decoded_icon_is_deterministic() {
        // 每次启动取到的必须是同一份像素：否则托盘图标会闪、也谈不上「可复现」。
        assert_eq!(app_icon(32), app_icon(32));
    }

    #[test]
    fn the_tooltip_goes_through_a_guid_addressed_notice_not_the_crate_api() {
        // 第三方缺陷的护栏（2026-10-10）：`tray-icon` 0.25.1 的
        // `TrayIcon::set_tooltip` 发 `NIM_MODIFY` 时**不带** `NIF_GUID`，
        // 而我们的图标是 GUID 身份的 ⇒ 每次都 `E_FAIL`，悬停提示永远停在
        // 建图标那一刻的初始值（真通知区实测见模块文档）。
        //
        // 钉三件事：① 提示确实由本模块自己发通告；② 通告带 GUID 身份；
        // ③ 谁也别把它换回 crate 那个坏方法。
        let src = include_str!("tray.rs");
        let prod = &src[..src.find("#[cfg(test)]").expect("本文件尾部有测试模块")];
        assert!(
            prod.contains("Shell_NotifyIconW(NIM_MODIFY"),
            "提示必须由本模块自己发通告，而不是交给 TrayIcon::set_tooltip"
        );
        assert!(
            prod.contains("NIF_TIP | NIF_GUID"),
            "通告必须带 NIF_GUID —— 不带就是按 (hWnd,uID) 找，GUID 图标找不到"
        );
        assert!(
            !prod.contains("set_tooltip(Some("),
            "不许退回 `TrayIcon::set_tooltip`：GUID 身份的图标上它必然失败"
        );
    }
}
