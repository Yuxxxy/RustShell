//! 窗口几何的跨进程记忆（2026-11-03 用户要求「关闭和打开时记住原窗口大小」）。
//!
//! 只做两件事：把窗口的位置 / 尺寸 / 最大化状态落到 `settings.json` 的
//! `window_bounds` 一节，以及在**放回屏幕之前先把它收进当前可见区域**。
//!
//! 后者是这个功能唯一真正危险的地方。用户上次在外接 2K 屏上用了窗口，回家拔了屏
//! 再启动：记录的坐标还在 `(3000, 200)`，而身上只剩一块 1920 宽的屏 —— 窗口落在
//! 看不见的地方，用户看到的是「程序启动了，可什么都没出现」，比不记忆更糟。
//! 所以恢复路径必须经过 [`WindowBoundsRecord::fitted_to`]，没有旁路。
//!
//! 本模块**不依赖 gpui**：这里只有纯函数与单测。采集在
//! `ui/src/main_view/window_persist.rs`（`Window::window_bounds()`），
//! 组装成 `gpui::WindowBounds` 在 `shell/src/app.rs`。
//!
//! 坐标是 gpui 的**逻辑像素**（`Pixels`），高分屏缩放差异因此不落在本层 ——
//! 但跨机器搬 `settings.json` 会：Windows 的 100% 与 150% 缩放给出不同的逻辑尺寸，
//! 这正是「必须按当前屏 clamp 而不是信任记录」的又一个理由。

use serde::{Deserialize, Serialize};

/// 窗口能被缩到多小（宽度逻辑像素）。
///
/// 与 `shell/src/app.rs` 的 `WindowOptions::window_min_size` 是同一个数：
/// 两处写死会漂移 —— 恢复逻辑把窗口钳到 720，而系统允许缩到 600，
/// 用户的窗口会「自己变大」，反之则「自己变小」。
pub const MIN_WINDOW_WIDTH: f32 = 720.0;
/// 窗口能被缩到多小（高度逻辑像素）。
pub const MIN_WINDOW_HEIGHT: f32 = 480.0;

/// 一块屏幕的可见区域（已排除任务栏 / Dock / 菜单栏）。
///
/// 用「可见区域」而不是整块屏：`y` 从任务栏上方开始算，窗口才不会被摆到
/// 任务栏底下（那部分在 Windows 上不可拖动，用户只能靠快捷键救回来）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScreenRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl ScreenRect {
    pub fn new(x: f32, y: f32, width: f32, height: f32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// 与一个窗口矩形的重叠面积（不相交 = `0.0`）。
    ///
    /// 多屏时挑重叠最大的那块，比「有没有相交」准得多：用户上次把窗口横跨两块屏，
    /// 按「相交即选中」挑到的是遍历里第一块（往往是主屏），窗口会被拽到另一块去。
    pub fn overlap_area(&self, win: &WindowBoundsRecord) -> f32 {
        let x0 = self.x.max(win.x);
        let y0 = self.y.max(win.y);
        let x1 = (self.x + self.width).min(win.x + win.width);
        let y1 = (self.y + self.height).min(win.y + win.height);
        if x1 <= x0 || y1 <= y0 {
            0.0
        } else {
            (x1 - x0) * (y1 - y0)
        }
    }

    /// 这块屏是不是与窗口有交集（哪怕只露出一角）。
    pub fn intersects(&self, win: &WindowBoundsRecord) -> bool {
        self.overlap_area(win) > 0.0
    }
}

/// 一次退出时的窗口几何（`settings.json` 的 `window_bounds`）。
///
/// `f32` 而不是 `i32`：gpui 的 `Bounds<Pixels>` 就是 f32，存整数会在 125% / 150%
/// 缩放下每启动一次丢半个像素，十几次之后窗口肉眼可见地变小。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WindowBoundsRecord {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    /// 上次退出时是否最大化。
    ///
    /// `#[serde(default)]` 是给手写 `settings.json` 的：这一节是后加的，
    /// 老文件里没有这个键，缺了必须当 `false` 而不是整个 `settings.json` 解析失败
    /// （后者会让用户的**全部**设置回到默认值）。
    #[serde(default)]
    pub maximized: bool,
}

impl WindowBoundsRecord {
    pub fn new(x: f32, y: f32, width: f32, height: f32, maximized: bool) -> Self {
        Self {
            x,
            y,
            width,
            height,
            maximized,
        }
    }

    /// 收进 `screen` 的可见区域内。
    ///
    /// 三步，顺序不能换：
    /// 1. **尺寸**先钳进 `[最小值, 屏幕]` —— 先钳位置再钳尺寸的话，一块比最小尺寸
    ///    还窄的屏会把窗口推出屏幕外；
    /// 2. **位置**钳进 `[屏幕起点, 屏幕终点 - 尺寸]`；
    /// 3. 原来的窗口与这块屏**完全不相交**（上次那块屏已经拔了）时不钳，直接居中 ——
    ///    钳出来的位置永远贴着边角，看着像 bug，居中最少还像个正常窗口。
    pub fn fitted_to(self, screen: ScreenRect) -> Self {
        // 下限先与屏幕取小：一块比最小尺寸还窄的屏（分屏后的半块屏、平板模式的
        // 窄窗口区）里，`clamp(720, 720)` 会把窗口撑到屏幕外 —— 那是本函数的第一个
        // 自测用例抓出来的，别再用 `screen.width.max(MIN)` 这种写法。
        let min_width = MIN_WINDOW_WIDTH.min(screen.width);
        let min_height = MIN_WINDOW_HEIGHT.min(screen.height);
        let width = self.width.clamp(min_width, screen.width.max(min_width));
        let height = self.height.clamp(min_height, screen.height.max(min_height));

        let (x, y) = if screen.intersects(&self) {
            let x_max = (screen.x + screen.width - width).max(screen.x);
            let y_max = (screen.y + screen.height - height).max(screen.y);
            (self.x.clamp(screen.x, x_max), self.y.clamp(screen.y, y_max))
        } else {
            (
                screen.x + (screen.width - width) / 2.0,
                screen.y + (screen.height - height) / 2.0,
            )
        };

        Self {
            x,
            y,
            width,
            height,
            maximized: self.maximized,
        }
    }
}

/// 多屏里挑一块来安置窗口：**重叠面积最大**的那块，都不相交则 `None`。
///
/// `None` 交给调用方去拿主屏（core 不知道「主屏」是什么，也不该知道）。
pub fn pick_screen(screens: &[ScreenRect], win: &WindowBoundsRecord) -> Option<ScreenRect> {
    screens
        .iter()
        .copied()
        .filter(|s| s.intersects(win))
        .max_by(|a, b| {
            a.overlap_area(win)
                .partial_cmp(&b.overlap_area(win))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 常见笔记本屏（1920×1080，可见区扣掉任务栏 40）。
    fn laptop() -> ScreenRect {
        ScreenRect::new(0.0, 0.0, 1920.0, 1040.0)
    }

    /// 拔掉外接屏之后，记在 (3000, 200) 的窗口必须回到主屏**正中**，
    /// 而不是留在屏幕外或贴着右边缘。
    ///
    /// 这条是整个功能的存在理由：不 clamp 的话用户看到的是「程序启动了但没窗口」，
    /// 而且他连「窗口跑到屏幕外了」都想不到。
    #[test]
    fn fitted_brings_back_a_window_from_an_unplugged_display() {
        let rec = WindowBoundsRecord::new(3000.0, 200.0, 1200.0, 800.0, false);
        let fitted = rec.fitted_to(laptop());
        assert_eq!(fitted.width, 1200.0, "尺寸本身没问题，不该被动");
        assert_eq!(fitted.height, 800.0);
        assert_eq!(fitted.x, (1920.0 - 1200.0) / 2.0, "完全不相交时必须居中");
        assert_eq!(fitted.y, (1040.0 - 800.0) / 2.0);
    }

    /// 只是稍微出界（右下角被任务栏/屏幕边缘切掉一点）⇒ 拉回来，不居中。
    #[test]
    fn fitted_pulls_a_partly_offscreen_window_back_to_the_edge() {
        let rec = WindowBoundsRecord::new(1500.0, 900.0, 1000.0, 600.0, false);
        let fitted = rec.fitted_to(laptop());
        assert_eq!(fitted.x, 920.0, "右边界应当贴到屏幕右边缘：1920 - 1000");
        assert_eq!(fitted.y, 440.0, "下边界应当贴到可见区底部：1040 - 600");
        assert_eq!(fitted.width, 1000.0);
        assert_eq!(fitted.height, 600.0);
    }

    /// 尺寸的两个边界：小于最小尺寸要抬起来，大于屏幕要压回去；
    /// 屏幕本身比最小尺寸还窄时**以屏幕为准**（否则窗口会被强行撑出屏）。
    #[test]
    fn fitted_clamps_size_between_the_minimum_and_the_screen() {
        let tiny = WindowBoundsRecord::new(10.0, 10.0, 320.0, 200.0, false);
        let fitted = tiny.fitted_to(laptop());
        assert_eq!(fitted.width, MIN_WINDOW_WIDTH, "不能比最小宽度还窄");
        assert_eq!(fitted.height, MIN_WINDOW_HEIGHT, "不能比最小高度还矮");

        let huge = WindowBoundsRecord::new(0.0, 0.0, 5000.0, 4000.0, false);
        let fitted = huge.fitted_to(laptop());
        assert_eq!(fitted.width, 1920.0, "不能比屏幕还宽");
        assert_eq!(fitted.height, 1040.0, "不能比可见区还高");

        let narrow = ScreenRect::new(0.0, 0.0, 600.0, 400.0);
        let fitted = tiny.fitted_to(narrow);
        assert_eq!(fitted.width, 600.0, "屏幕比最小尺寸还小时以屏幕为准");
        assert_eq!(fitted.height, 400.0);
        assert_eq!(fitted.x, 0.0);
    }

    /// 最大化标记要原样带过去 —— 丢掉它，用户会发现「上次最大化开的，
    /// 这次是个小窗」，而那正是这个功能要解决的第一件事。
    #[test]
    fn fitted_keeps_the_maximized_flag() {
        let rec = WindowBoundsRecord::new(100.0, 100.0, 1200.0, 800.0, true);
        assert!(rec.fitted_to(laptop()).maximized);
    }

    /// 横跨两块屏时挑**重叠更大**的那块，不是遍历里第一块。
    ///
    /// 挑错的症状很具体：用户把窗口摆在左屏偏右，重启后它整块跑到右屏去了。
    #[test]
    fn pick_screen_prefers_the_largest_overlap() {
        let left = ScreenRect::new(-1920.0, 0.0, 1920.0, 1040.0);
        let right = ScreenRect::new(0.0, 0.0, 1920.0, 1040.0);
        let mostly_left = WindowBoundsRecord::new(-1500.0, 100.0, 1200.0, 800.0, false);
        assert_eq!(
            pick_screen(&[left, right], &mostly_left),
            Some(left),
            "三分之二在左屏，就该挑左屏"
        );

        let mostly_right = WindowBoundsRecord::new(-200.0, 100.0, 1200.0, 800.0, false);
        assert_eq!(pick_screen(&[left, right], &mostly_right), Some(right));
    }

    /// 一块都不相交时返回 `None`（调用方拿主屏兜底），而不是硬塞第一块。
    #[test]
    fn pick_screen_returns_none_when_nothing_intersects() {
        let rec = WindowBoundsRecord::new(9000.0, 9000.0, 1200.0, 800.0, false);
        assert_eq!(pick_screen(&[laptop()], &rec), None);
    }

    /// `settings.json` 是用户可以手改的：`maximized` 缺席必须当 `false`，
    /// 不能让整份设置解析失败（那会把用户全部设置清成默认值）。
    #[test]
    fn record_tolerates_a_hand_written_entry_without_maximized() {
        let raw = r#"{"x":100.0,"y":50.0,"width":1200.0,"height":800.0}"#;
        let rec: WindowBoundsRecord = serde_json::from_str(raw).expect("缺 maximized 也要能读");
        assert!(!rec.maximized);
        assert_eq!(rec.width, 1200.0);

        let round: WindowBoundsRecord =
            serde_json::from_str(&serde_json::to_string(&rec).expect("序列化")).expect("反序列化");
        assert_eq!(round, rec);
    }
}
