//! Tab 栏（3.1 节 Tab 区 / 6.2 节）：官方 TabBar/Tab（状态点 + 标题 + 关闭），右侧侧栏开关。
//!
//! 2026-09-17 第二轮运行反馈「tab 改为官网 demo 那个样式，移除 tab 滑动轴」：
//! 整条标签栏改用官方 `TabVariant::Segmented` —— 灰色圆角轨道 + 选中项由官方指示器
//! 绘制（圆角胶囊 + 投影，在 `tabs-inner` 里弹簧滑动），标签自身不再叠任何自绘
//! 覆盖层。随之撤销三样自造件：顶部 accent 选中条、底部「连接状态」呼吸横条
//! （换成标签前缀里的一枚 6px 状态圆点，见 [`TabStatus`]），以及第一轮补的
//! 横向滚动条覆盖层（滑动轴本体）。
//!
//! 2026-09-17 第三轮运行反馈「tab 标题去掉图标」：前缀里连类型图标一并撤掉，
//! 只剩状态圆点 + 标题（`TAB_ICON_TITLE_TIGHTEN` 随之删除）。
//!
//! 2026-09-17 第四轮运行反馈「状态灯改为可呼吸式」：实心状态点（已连接 / 连接中）
//! 以脉动动画 + `with_max_fps` 限频来做（见 [`build_tab_item`]）。
//!
//! ★ 2026-09-20 性能修复：呼吸从「无限重播」改为**有限次**（[`TAB_DOT_BREATH_CYCLES`]
//! 次，播完即停，见 [`status_dot_animation`]），并移除 `repeat_synced`。原因见
//! [`TabStatusDot`] 的「为什么必须是有限次」：无限动画会让**整个窗口以 20fps 永久重绘**，
//! 实测空闲占用 `gap_avg_us ≈ 100000`、`slow_frames 60/60`、UI 线程吃满 0.73 个核。
//!
//! 2026-09-17 第五轮运行反馈「呼吸灯改为微微放大，不要变暗淡」：脉动从**压透明度**
//! 改为**改直径**（6 → 7.5px，[`dot_pulse`]）—— 透明度脉动在深色底上被读成
//! 「灯忽明忽暗」，而直径胀缩才是「灯在呼吸」。圆点套在一个固定 6px 的槽位里放大，
//! 所以标题不会跟着左右抖。
//!
//! 2026-09-17 第六轮运行反馈「呼吸灯用光晕 + 缩放 + 透明度 + 向外扩散的圆环」：
//! 三层都落在 [`dot_pulse`] 里 —— 圆点直径脉动（缩放）＋ 圆点柔光（`box-shadow` 光晕）
//! ＋ 一圈向外扩、扩到最大时淡尽的光环。**圆点自身不设透明度**（第五轮的要求不变），
//! 「会淡的那个」只有外扩光环。同时按「tab 标题和左侧贴合」把胶囊左右内缩
//! 4px → 6px（见 [`TAB_EDGE_INSET`]）。
//!
//! 2026-09-17 第七轮运行反馈「未修复 tab 标题和 tab 框左侧贴合」（截图确认指的是
//! **第一个标签的白框离灰色轨道左缘只有 4px**）：官方 `Segmented` 的轨道内边距是写死的
//! `padding_x`，应用侧用 `TabBar::prefix` 在标签条之前补了一档占位，第一个标签离轨道
//! 左缘变成 12px（见 [`TAB_STRIP_LEFT_INSET`]）。
//!
//! 2026-09-18 运行反馈「tab 栏可以按住拖动左右移动和其他 tab 切换位置，轻点可以
//! 类似滑动位置、类似滑动轴移动」：补上两种拖拽，都用 gpui 的 `on_drag` /
//! `on_drag_move`（不是自己拼 `on_mouse_down` + `on_mouse_move`）——
//!
//! 1. **按住标签拖动 = 与相邻标签换位**（[`TabDrag`] + [`MainView::reorder_tab_dragged_to`]）。
//!    换位是**实时**的：每跨过一个邻居就把 `tabs` 里的顺序真正改一次，松手时已经就位。
//! 2. **按住标签栏空白处拖动 = 整条标签栏横向平移**（[`BarPan`] + [`pan_offset`]）——
//!    这就是「类似滑动轴移动」：1:1 跟手地把溢出内容拖出来，不必去找滚动条。
//!    平移量按**逐次位移累加**（`MainView::tab_pan_x` 存上一次指针 x，拖拽开始时清零），
//!    不记「按下点 + 总位移」：拖拽期间的偏移有一部分是自己刚写进去的，
//!    用累计量再乘一遍会越拖越快。
//!
//! 为什么必须走 `on_drag`：`on_drag` 只在「按下点在本元素上 **且** 位移超过
//! `DRAG_THRESHOLD`(2px)」时才建立 `active_drag`，于是**轻点仍然是普通点击**（标签照常
//! 切换、`TabBar::on_click` 照常收到）；而 `on_drag_move` 是 window 级监听（capture 阶段，
//! 只按「当前拖拽载荷的类型」过滤，**不要求指针还在本元素上**），指针拖出标签栏甚至拖
//! 到终端上也照样收到 —— 换位恰恰需要这个。
//! 两者的优先级天然正确：gpui 的鼠标事件先跑最内层（bubble 阶段按注册反序），按下点在
//! 标签上时标签先建立 `active_drag` 并 `stop_propagation`，标签栏那一层就不会跟着起拖。

use gpui::{
    div, point, px, Animation, AnimationExt as _, AnyElement, AppContext as _, BoxShadow,
    ClickEvent, Context, DragMoveEvent, Hsla, InteractiveElement as _, IntoElement, IsZero as _,
    MouseButton, ParentElement as _, Pixels, Render, ScrollWheelEvent, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window,
};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    h_flex,
    menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem},
    tab::{Tab, TabBar, TabVariant},
    IconName, Sizable as _,
};
use std::time::Duration;

use rustshell_core::split::SplitAxis;

use super::main_view::MainView;

/// N8：样式取色统一走主题工具函数（删除就地 hex 定义）
use crate::theme::to_hsla as hex;

/// 单个 Tab 的最大宽度（`render_tab_bar`的`max_width`）。
const TAB_MAX_WIDTH: f32 = 220.;

/// 右击菜单覆盖层相对 suffix 包装层的外扩量（px）。
const TAB_MENU_OVERHANG: f32 = TAB_MAX_WIDTH + 100.;

/// 连接状态圆点直径（px）。与终端面板、分屏标题栏的状态点同规格。
const TAB_STATUS_DOT: f32 = 6.;

/// 状态点呼吸周期（ms）：一次「静 → 涨 → 静」的完整脉动。
const TAB_DOT_BREATH_MS: u64 = 1400;

/// 呼吸幅度（px）：脉动峰值时圆点直径比常态大这么多（6 → 7.5，约 +25%）。
///
/// 2026-09-17 第五轮运行反馈「呼吸灯改为微微放大，不要变暗淡」：上一版动的是透明度
/// （谷值压到 0.45），观感是「灯忽明忽暗」，在深色底上被读成「灯接触不良」。幅度取
/// 1.5px 是「微微」的量化：再小看不见，超过 2px 在 6px 的圆点上就成了闪烁。
const TAB_DOT_BREATH_GROW: f32 = 1.5;

/// 外扩光环每边的扩散量（px）：光环从「贴着圆点」一路扩到 `圆点直径 + 2 × 本值`。
///
/// 2026-09-17 第六轮运行反馈给了这套呼吸灯的做法：光晕 + 缩放 + 透明度 + **向外扩散的
/// 光环**。前两样由圆点自己承担（直径脉动 + 柔光），这一样是「光环」那一层：它只在
/// 向外扩张时存在，扩到最大时刚好淡尽 —— 观感是灯在「吐」出一圈光。
const TAB_DOT_HALO_SPREAD: f32 = 3.5;

/// 外扩光环的起始透明度（扩散到底时为 0）。
///
/// 注意与「不要变暗淡」的区别：被淡出的只有这圈**外环**，圆点自身恒为不透明
/// （见 [`dot_pulse`]：它连透明度都不返回）。
const TAB_DOT_HALO_ALPHA: f32 = 0.55;

/// 圆点自带柔光（`box-shadow`）的模糊半径与蔓延（px）。
///
/// 6px 的圆点配 3px 模糊：再大就糊成一团白斑，再小看不出「灯在发光」。
const TAB_DOT_GLOW_BLUR: f32 = 3.;
const TAB_DOT_GLOW_SPREAD: f32 = 0.5;

/// 圆点柔光的透明度区间：呼吸谷值 → 峰值。**两端都 > 0**：灯芯只是变亮，不是熄灭。
const TAB_DOT_GLOW_MIN: f32 = 0.25;
const TAB_DOT_GLOW_MAX: f32 = 0.5;

/// 呼吸动画的限频（FPS）。
///
/// 这个数字是**播放期间每一次重绘整窗**的成本尺：动画元素每 tick 一次就
/// `cx.notify` 一次当前视图，播放中的全窗口重绘频率就等于它。1.4s 的慢脉动不需要
/// 满帧：20FPS 下每周期 28 帧，位移与直径的步进都远小于一个像素。
///
/// ★ 注意它**只在播放时**生效：动画是一次性的（见 [`status_dot_animation`]），
/// 播完 `done = true` 后 gpui 不再排帧，空闲时窗口是 0 帧/秒。
/// 2026-09-17 实测该视图的单帧成本不低（见 `docs` 的性能记录），这里不用 30 换那点顺滑。
const TAB_DOT_BREATH_FPS: f32 = 20.;

/// 呼吸动画播放的**周期数**：播完这么多下就停，不无限重播。
///
/// 「连接成功」是一次性事件，值得给一次可感知的反馈，但**不该让窗口永远重绘**。
/// 2026-09-20 实测：`repeat_synced()` 的无限动画会让空闲窗口稳定停在 20fps、
/// UI 线程吃掉 0.73 个核（`term_paint_n = 60/60` —— 终端每帧都在重画）。
/// 3 次约 4.2s：足够看清「它连上了」，又不至于变成常驻开销。
///
/// **改这个数只影响「播几下」**；每下的快慢由 [`TAB_DOT_BREATH_MS`] 决定，
/// 因为 [`breath_easing_bounded`] 是按周期数折算相位的。
const TAB_DOT_BREATH_CYCLES: u32 = 3;

/// 呼吸动画的总时长 = 周期数 × 单周期（一次性播放的 `Animation::duration`）。
const TAB_DOT_BREATH_TOTAL_MS: u64 = TAB_DOT_BREATH_MS * TAB_DOT_BREATH_CYCLES as u64;

/// 余弦呼吸缓动：归一化时间 0..1 映射为 0→1→0 的平滑往返（谷点在周期正中）。
fn breath_easing(d: f32) -> f32 {
    (1.0 - (d * std::f32::consts::TAU).cos()) / 2.0
}

/// 有限次呼吸的缓动：把 [`TAB_DOT_BREATH_CYCLES`] 个整周期压进**一次**播放。
///
/// 之所以能一行搞定：`breath_easing(d)` 在 `d` 取整数处恰好回到谷值 0，于是
/// `breath_easing(d × N)` 就是「N 次 0→1→0」的连播；而且 `d = 1` 时正好落在
/// 谷值 —— 动画结束那一刻圆点回到常态直径，不会「卡」在放大态。
///
/// 由此得到一个反直觉但正确的事实：单次脉动的**实际时长 = 总时长 / N**，
/// 也就是仍然等于 [`TAB_DOT_BREATH_MS`]，改周期数不会让呼吸变快变慢。
fn breath_easing_bounded(d: f32) -> f32 {
    breath_easing(d * TAB_DOT_BREATH_CYCLES as f32)
}

/// 状态点的呼吸动画（**唯一**构造入口，护栏直接检查它的返回值）。
///
/// # 三条约束（任何一条破了，空闲窗口就会持续重绘）
///
/// 1. **一次性**（`Animation::new` 的默认 `oneshot = true`）—— 播完
///    [`TAB_DOT_BREATH_TOTAL_MS`] 后 `delta > 1.0` 且 oneshot ⇒ `done = true`，
///    gpui 不再调度下一帧。**绝不要加 `.repeat()` / `.repeat_synced()`**：
///    它们把 `oneshot` 置为 false，`done` 就永远为假（见 [`TabStatusDot`]）。
/// 2. **不能用 `synced`** —— `synced` 的相位是「全局 epoch 对 duration 取模」
///    （`elements/animation.rs:419-422`），`delta` 恒落在 `[0, 1)`，
///    于是 `delta > 1.0` 永不成立：**连「播够时长就结束」都不会发生**。
///    顺带一提，这也意味着任何 `repeat_synced()` 都是事实上的永动机。
/// 3. **限频**（`with_max_fps`）—— 播放期间反正要重绘整窗，把频率压在
///    [`TAB_DOT_BREATH_FPS`]，别按屏幕刷新率烧。
fn status_dot_animation() -> Animation {
    Animation::new(Duration::from_millis(TAB_DOT_BREATH_TOTAL_MS))
        .with_easing(breath_easing_bounded)
        .with_max_fps(TAB_DOT_BREATH_FPS)
}

/// 一个呼吸相位下的圆点外观（纯函数，可单测）。
///
/// 2026-09-17 第六轮运行反馈「用光晕 + 缩放 + 透明度 + 外扩光环做呼吸灯」：这三层
/// 都收在这里，**并且刻意不给圆点自身留透明度字段** —— 用户上一轮明确要求
/// 「不要变暗淡」，所以「会淡的那个」只能是外扩光环（[`Self::halo_alpha`]）。
#[derive(Debug, Clone, Copy, PartialEq)]
struct DotPulse {
    /// 圆点直径（px）：`TAB_STATUS_DOT .. TAB_STATUS_DOT + TAB_DOT_BREATH_GROW`
    dot: f32,
    /// 圆点柔光的不透明度：`TAB_DOT_GLOW_MIN .. TAB_DOT_GLOW_MAX`
    glow: f32,
    /// 外扩光环直径（px）：从圆点边缘扩到 `圆点 + 2 × TAB_DOT_HALO_SPREAD`
    halo: f32,
    /// 外扩光环的不透明度：`TAB_DOT_HALO_ALPHA .. 0`
    halo_alpha: f32,
}

/// 呼吸相位（`0..1`，谷点在 0 与 1）→ 圆点外观。
///
/// 抽成纯函数是为了让「微微放大、不要变暗淡」这条要求能被直接量：直径谷值恰好是常态
/// 直径（**不会变小**）、全程单调不减；柔光两端都 > 0（**不会熄灭**）；只有外扩光环
/// 的透明度一路降到 0（那正是「扩散出去」）。
fn dot_pulse(phase: f32) -> DotPulse {
    let p = phase.clamp(0.0, 1.0);
    DotPulse {
        dot: TAB_STATUS_DOT + TAB_DOT_BREATH_GROW * p,
        glow: TAB_DOT_GLOW_MIN + (TAB_DOT_GLOW_MAX - TAB_DOT_GLOW_MIN) * p,
        halo: TAB_STATUS_DOT + TAB_DOT_BREATH_GROW * p + 2. * TAB_DOT_HALO_SPREAD * p,
        halo_alpha: TAB_DOT_HALO_ALPHA * (1. - p),
    }
}

/// 标签行高（px），等于官方`TabVariant::Segmented`在`Size::Small`下的`height()`。
/// 单测`tab_height_matches_the_row_height_constant`守住它，官方若调整该尺寸会立刻报红。
///
/// 2026-09-17 第一轮反馈「tab 栏太高、字体太大」：整栏从官方默认档（32px /
/// `text_sm`）降到`Size::XSmall`（20px / `text_xs`）。
/// 同日第二轮反馈「图标与标题重合、关闭按钮压住上下两根横条、标题再高一点」：
/// 回到`Size::Small`（24px / `text_sm`）。20px 时关闭按钮恰好也是 20px 高
/// （图标按钮`Size::XSmall`即`size_5`），与标签等高，悬停底色会盖满整条标签。
///
/// 第二轮改用官方 Segmented 后，标签栏里那两块「零宽同高占位」被撤掉，
/// 本常量在渲染路径上不再有引用点，只剩上面那条护栏断言在用 —— 这是刻意的：
/// 它是`TAB_SIZE`的**成对契约**，不是死配置，故显式放行 `dead_code`。
///
/// 2026-09-17 第六轮：`TAB_SIZE` 回到官方默认档`Medium`（32px / `text_base`），
/// 本常量随之同步到 32 —— 它守的就是「这两个数不许各走各的」。
#[allow(dead_code)]
const TAB_ROW_HEIGHT: f32 = 32.;

/// 标签尺寸。与 `TAB_ROW_HEIGHT` 必须成对修改：高度由它决定，字号/内边距也随之走一档。
const TAB_SIZE: gpui_component::Size = gpui_component::Size::Medium;

/// 标签栏外壳总高（px）：轨道 32（`TAB_ROW_HEIGHT`）+ 上下各 4px（`py_1`）。
///
/// 2026-09-18 运行反馈「全部 tab 关闭后 tab 栏变矮、打开 tab 变高」：官方 `TabBar`
/// 的高度由内容撑出 —— 有标签时是 32px 的标签，空标签时只剩 ~20px 的「更多」按钮，
/// 外壳高度随之跳变。定高后内容 `items_center` 居中，两种状态总高一致。
const TAB_SHELL_HEIGHT: f32 = TAB_ROW_HEIGHT + 8.;

/// 标签栏外壳的左右内缩（px）：与侧栏分割线脱开（2026-09-16 反馈「tab 和侧边栏贴合」）。
const TAB_BAR_SHELL_INSET: f32 = 8.;

/// 官方 `Segmented` 轨道**自身**的左内边距（px）。
///
/// 取自 `gpui-component` 的 `TabBar::render`：`Size::Medium` → `padding_x = px(4.)`，
/// 且 `tabs` 容器用 `mx(-padding_x)` 又把外层 padding 抵消掉，所以净效果就是「第一个
/// 标签距轨道左缘 4px」。这个数写在这里是为了让
/// `tab_strip_starts_clear_of_the_track_edge` 能断言「应用补的内缩 + 框架的 4px」——
/// 官方改档位或改这个常量时用例会报红，而不是悄悄退回 4px。
///
/// 它只被那条护栏用例引用（渲染路径用的是框架自己的值），故显式放行 `dead_code`：
/// 它是 `TAB_STRIP_LEFT_INSET` 的**成对契约**，不是死配置。
#[allow(dead_code)]
const TRACK_BUILTIN_PADDING_X: f32 = 4.;

/// 应用侧再补的**标签条左内缩**（px）。
///
/// 2026-09-17 运行反馈「tab 标题和 tab 框左侧贴合」（截图确认：指的是第一个标签的白框
/// 距灰色轨道左缘只有 4px）：官方没给「轨道内边距」的入口，但 `TabBar::prefix` 会把元素
/// 插在标签条**之前**、且仍在轨道内 —— 于是用一个 8px 宽的占位把整条标签右推，
/// 第一个标签离轨道左缘变成 4 + 8 = 12px。
const TAB_STRIP_LEFT_INSET: f32 = 6.;

/// 「标签条」左内缩占位（生产与探针共用同一份写法）。
fn tab_strip_prefix() -> gpui::Div {
    div().w(px(TAB_STRIP_LEFT_INSET)).flex_shrink_0()
}

/// 标签前缀/后缀相对胶囊边缘的横向内缩（px）。
/// Segmented 的选中胶囊由官方指示器按**整个标签**的边界绘制，而标签根自身没有横向
/// 内边距 —— 不加这一档，前缀首项（状态圆点）与后缀末项（关闭按钮）就会贴着胶囊边缘。
///
/// 2026-09-17 第六轮运行反馈「tab 标题和左侧贴合」：4px 太紧 —— 圆点上下离胶囊边
/// `(胶囊高 - 6) / 2`（`Size::Medium` 下即 9px），左右却只有 4px，观感是圆点被摁在
/// 左边缘上（关闭按钮同理贴在右边缘）。改成 6px：与纵向留白同向，也给呼吸灯的
/// 外扩光环让出位置（光环峰值每边外扩 `TAB_DOT_HALO_SPREAD`，4px 时会被胶囊裁掉）。
const TAB_EDGE_INSET: f32 = 6.;

/// 标签的连接状态（决定标签前缀那枚 6px 圆点的外观）。
///
/// 画法与终端面板、分屏标题栏的状态点一致：已连接 = 实心绿点，连接中 = 实心黄点，
/// 未连接 = 空心灰点。判定收口在 [`TabStatus::of`]（纯函数，可单测）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TabStatus {
    Connected,
    Connecting,
    /// 意外断连（画面还在，但会话已死）：空心红点。
    ///
    /// 与 `Idle` 分开是为了让「掉线了」在标签栏就能被看见 —— 用户可能正在
    /// 别的标签里干活，红点是他回来找现场的唯一线索。
    Dead,
    Idle,
}

impl TabStatus {
    /// 由「会话是否可用 / 是否在连接 / 是否断连」推导。
    ///
    /// 注意第一个参数是 [`crate::main_view::TabEntry::is_live`]（有终端**且未断连**），
    /// 不是 `terminal.is_some()` —— 断连态刻意保留终端视图，用后者会把掉线的标签
    /// 显示成「已连接」的绿点。
    fn of(live: bool, connecting: bool, dead: bool) -> Self {
        if live {
            Self::Connected
        } else if dead {
            Self::Dead
        } else if connecting {
            Self::Connecting
        } else {
            Self::Idle
        }
    }

    /// 圆点取色 + 是否实心（空心 = 未连接，只描一圈）。
    fn dot(&self, t: &rustshell_core::theme::ThemeTokens) -> (Hsla, bool) {
        match self {
            Self::Connected => (hex(t.success), true),
            Self::Connecting => (hex(t.warning), true),
            Self::Dead => (hex(t.danger), false),
            Self::Idle => (hex(t.fg_muted), false),
        }
    }
}

/// 构造单个 Tab：连接状态圆点（前缀）、标题、关闭按钮与右击菜单覆盖层（后缀）。
///
/// 2026-09-17 运行反馈「tab 标题去掉图标」：前缀里只剩状态圆点，不再画本地/SSH 的
/// 类型图标 —— 标题本身就足以区分会话，多一个图标只是白占宽度，还在 150% 缩放下
/// 与标题首笔糊在一起（即第一轮反馈的「图标和名称重合」）。
///
/// 选中样式完全交给官方`TabVariant::Segmented`：白色圆角胶囊由`TabBar`的指示器按
/// **整个标签**的边界绘制（含前缀的圆点与后缀的关闭按钮），本函数不做任何选中态
/// 绘制，也不再有自绘覆盖层 —— 所以这里既不需要零宽同高占位，也不需要负偏移外扩。
/// 呼吸状态点：**独立成视图**的标签连接状态圆点。
///
/// # 为什么必须独立成视图（2026-09 性能修复）
///
/// gpui 的动画调度器在 `with_max_fps` 分支里做的最后一件事是
/// （`elements/animation.rs:452-466`）：
///
/// ```text
/// let view = window.current_view();      // ← 动画元素所属的视图
/// timer(1/max_fps).await;
/// cx.notify(view);                       // ← 只把这个视图标脏
/// ```
///
/// `current_view()` 取的是「**元素所属的视图**」。此前圆点的动画元素直接建在
/// `MainView::render_tab_bar` 里，于是 `current_view()` 就是 `MainView` ——
/// 只要存在一个已连接/连接中的标签，**整个窗口树（标题栏 / 侧栏 / 标签栏 /
/// 终端 / 快捷命令栏 / 状态栏，含约 20 个 `.on_action` 监听器）就每 50ms 重建一次**。
///
/// 摘成子视图后，`current_view()` 变成这个小视图，`cx.notify` 只重排这 6px 的圆点，
/// `MainView` 子树走 gpui 的视图缓存（`reuse_prepaint`/`reuse_paint`）。
///
/// 与 [`SidebarPanelView`](super::main_view::SidebarPanelView) 是同一手法：视图越窄，
/// 被高频 `notify` 的代价越小。
///
/// ## ★ 为什么必须是有限次（2026-09-20 性能修复，比「摘子视图」更关键）
///
/// 摘子视图只省掉**元素树重建**（`current_view()` 变窄），**省不掉整窗
/// layout/prepaint/paint**：实测 `term_paint_n` 在每个 60 帧窗口里恒为 60
/// —— 终端每帧都在重画，而它单帧要 9~16ms。所以只要动画还在无限重播，
/// 窗口就永远以 `TAB_DOT_BREATH_FPS` 出帧、UI 线程永远占着 0.73 个核。
///
/// 唯一的出口是让动画**结束**：`Animation` 只有 `oneshot: bool`，
/// `repeat()`/`repeat_synced()` 都会把它置为 false ⇒ `done` 永不为真。
/// 于是改用「一次性播放 + 把 N 个周期压进一次的 easing」，
/// 见 [`status_dot_animation`] 与 [`breath_easing_bounded`]。
///
/// 另一条硬约束：**不能用 `synced`**。`synced` 的相位是全局 epoch 对 duration
/// 取模，`delta` 恒 `< 1` ⇒ 连「播够时长就自然结束」都做不到，等于变相永动。
/// 代价是各点的相位不再全局对齐 —— 但这反而更合语义：每个点从**自己连上的那一刻**
/// 起播，反馈的是「这个标签刚连上」，而不是一个与连接无关的全局节拍。
///
/// 元素身份的支撑：`AnyView` 的 `ElementId` 是 `ElementId::View(实体 id)`
/// （`view.rs:302`），`with_element_state` 又按整条 id 链取状态
/// （`window.rs:3986`）—— 所以每个圆点持有**自己那份** `AnimationState`
/// （含各自的起播时刻），标签换位也不会重启动画。
pub struct TabStatusDot {
    /// 圆点颜色（主题色；主题变化时由父视图每帧写入，`Hsla` 是 `Copy`）
    color: Hsla,
    /// 是否绘制「实心 + 呼吸」。false = 未连接的空心静态点。
    filled: bool,
}

impl TabStatusDot {
    /// `filled = true` 时渲染呼吸实心点，否则渲染空心静态点。
    fn new(filled: bool, color: Hsla, cx: &mut Context<Self>) -> Self {
        // 圆点自身不需要订阅任何东西：颜色/填充态由父视图在渲染时同步进来。
        let _ = cx;
        Self { color, filled }
    }

    /// 父视图每帧同步外观（`copy` 语义，无分配）。
    fn sync(&mut self, filled: bool, color: Hsla) {
        self.filled = filled;
        self.color = color;
    }
}

impl Render for TabStatusDot {
    fn render(&mut self, _w: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        render_status_dot(self.filled, self.color)
    }
}

/// 固定尺寸槽位（圆点与其外扩光环都在槽位里居中，放大时向四周溢出）。
fn dot_slot() -> gpui::Div {
    h_flex()
        .debug_selector(|| "tab-status-dot-slot".to_string())
        .justify_center()
        .flex_shrink_0()
        .w(px(TAB_STATUS_DOT))
        .h(px(TAB_STATUS_DOT))
}

/// 静态空心点（未连接）：没有会呼吸的东西，也就没有动画开销。
fn static_hollow_dot(color: Hsla) -> AnyElement {
    dot_slot()
        .child(
            div()
                .debug_selector(|| "tab-status-dot".to_string())
                .w(px(TAB_STATUS_DOT))
                .h(px(TAB_STATUS_DOT))
                .flex_shrink_0()
                .rounded_full()
                .border_1()
                .border_color(color),
        )
        .into_any_element()
}

/// 呼吸实心点的**画法**：把某个相位下的圆点外观画进给定槽位
/// （**纯布局**：相位是入参，不读任何时钟）。
///
/// 动画元素挂的是**槽位**（不是圆点）：一帧里要同时摆两样东西 —— 底下的外扩光环与
/// 上面的圆点，两者都从同一个相位算出来，所以只能在同一处重建。
///
/// 尺寸动画**必须**留在槽位里，不能直接挂在标签的 flex 子项上：那样圆点每胀 1.5px
/// 就把标题右推 1.5px，一个呼吸周期标题左右抖一下，比不做动画更难受。槽位是固定
/// 尺寸，所以放大不参与父级布局；槽位本身 `flex_shrink_0`，窄标签也压不扁它。
/// 槽位边长 = 常态直径，圆点在谷值恰好铺满槽位 ——「静止状态」与改前同位置同大小。
///
/// 拆出来是为了让两处共用**同一份**画法，而不是各写一套：
///   * 动画闭包 —— 每帧喂当前相位；
///   * 几何测试 —— **直接喂 0.0 / 1.0**。
///
/// 「测试能直接喂相位」是这次改动的关键基础设施：动画状态里的 `start` 是
/// `web_time::Instant`（`gpui-pre-scheduler` 的 `clock.rs` 无条件
/// `pub use web_time::Instant`），也就是**真实墙钟** —— 只有 `synced` 分支才走
/// 执行器时钟。呼吸改成一次性之后相位不再来自执行器时钟，`advance_clock` 便推不动它，
/// 于是测试必须能自己给相位，否则「直径真的变大」这一半就永远量不到了。
fn paint_status_dot(slot: gpui::Div, phase: f32, dot_color: Hsla) -> gpui::Div {
    let p = dot_pulse(phase);
    // 光环：只有 1px 描边的圆环，直径一路扩、透明度一路淡到 0，
    // 画在圆点之前（先画的在下层）。偏移取 `(槽位 - 直径) / 2`，
    // 于是它始终以槽位中心为中心向外扩 —— 也就是「向外扩散」。
    let off = (TAB_STATUS_DOT - p.halo) / 2.;
    slot.child(
        div()
            .debug_selector(|| "tab-status-dot-halo".to_string())
            .absolute()
            .left(px(off))
            .top(px(off))
            .w(px(p.halo))
            .h(px(p.halo))
            .rounded_full()
            .border_1()
            .border_color(dot_color.opacity(p.halo_alpha)),
    )
    // 圆点：直径脉动 + 柔光（`box-shadow`）。**不设透明度** ——
    // 用户要求「不要变暗淡」，会淡的只有上面那圈外环。
    .child(
        div()
            .debug_selector(|| "tab-status-dot".to_string())
            .w(px(p.dot))
            .h(px(p.dot))
            .flex_shrink_0()
            .rounded_full()
            .bg(dot_color)
            .shadow(vec![BoxShadow::new(
                px(0.),
                px(0.),
                dot_color.opacity(p.glow),
            )
            .blur_radius(px(TAB_DOT_GLOW_BLUR))
            .spread_radius(px(TAB_DOT_GLOW_SPREAD))]),
    )
}

/// 把「某个相位」包成一次性呼吸动画（相位与外观都取自 [`paint_status_dot`]）。
fn render_status_dot(filled: bool, dot_color: Hsla) -> AnyElement {
    if !filled {
        return static_hollow_dot(dot_color);
    }
    dot_slot()
        .with_animation(
            SharedString::from("tab-dot-breathe"),
            // 动画参数全部收在 `status_dot_animation()` 里（一次性 + 有限周期 +
            // 限频）。**别在这里内联构造**：护栏要靠那个函数的返回值把
            // 「不许无限重播」钉住。
            status_dot_animation(),
            move |slot, phase| paint_status_dot(slot, phase, dot_color),
        )
        .into_any_element()
}

/// 「按住标签拖动换位」的拖拽载荷。
///
/// 只带会话 id，见 [`build_tab_item`] 的说明：换位是实时的，下标会在拖拽过程中
/// 反复变化，只有 id 稳定。
#[derive(Clone)]
struct TabDrag {
    /// 被拖动的会话 id
    sid: String,
}

/// 「按住标签栏空白处拖动平移」的拖拽载荷：只作类型标记，状态在 `MainView::tab_pan_x`
/// （上一次指针 x）与滚动句柄上，见 [`MainView::drag_tab_bar_pan`]。
struct BarPan;

/// 拖拽预览视图：**空**元素。
///
/// `on_drag` 必须给一个「跟随光标的视图」，但标签换位的反馈由「标签真的挪了位置、
/// 选中胶囊跟着滑过去」承担，再飘一块半透明的标签副本只会挡住相邻标签。
/// 所以这里渲染一个零尺寸的空 div：拖拽语义齐全，视觉上什么都不加。
struct DragGhost;

impl Render for DragGhost {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

/// 平移拖拽的偏移换算（纯函数，可单测）：当前偏移 + 本次位移 = 新偏移。
///
/// 方向取「抓住纸拖」的手感：指针右移（`dx > 0`）意味着露出左边的内容，
/// 偏移向 0 走 —— 与 gpui 的 `offset.x` 约定（越向右滚越负）一致。
/// 夹取区间是 `[-max, 0]`（`max` 是 [`gpui::ScrollHandle::max_offset`] 的**正值幅度**，
/// 见 [`wheel_scroll_target`] 的说明，别改回老 Zed 的负值约定）。
fn pan_offset(current: f32, dx: f32, max: f32) -> f32 {
    (current + dx).clamp(-max.max(0.), 0.)
}

/// 从「每个标签的屏上横坐标区间」里挑出指针压住的那一格（纯函数，可单测）。
///
/// 落在区间之间的空隙、或整条栏之外时取**最近**的区间：这样「把标签拖到最左边」
/// 能一路换到 0 号位，不会卡在某个缝隙里不动。列表为空（边界还没就绪）返回 `None`。
fn nearest_slot(ranges: &[(f32, f32)], x: f32) -> Option<usize> {
    let mut best: Option<(f32, usize)> = None;
    for (i, &(left, right)) in ranges.iter().enumerate() {
        let dist = if x < left {
            left - x
        } else if x > right {
            x - right
        } else {
            0.
        };
        if best.is_none_or(|(best_dist, _)| dist < best_dist) {
            best = Some((dist, i));
        }
    }
    best.map(|(_, i)| i)
}

fn build_tab_item(
    i: usize,
    ctx: TabMenuCtx,
    dot: AnyElement,
    on_close: impl Fn(&ClickEvent, &mut gpui::Window, &mut gpui::App) + 'static,
) -> Tab {
    let title = ctx.title.clone();
    let sid = ctx.id.clone();
    let menu_ctx = ctx;
    let drag_sid = sid.clone();
    Tab::new()
        .label(title)
        // 「按住拖动换位」的载荷只带**会话 id**，不带下标：换位是实时进行的，
        // 被拖标签在 `tabs` 里的下标一帧一变，而 id 全程稳定 —— 用 id 反查当前
        // 下标，回调逻辑就是幂等的，重复收到同一个位置也不会抖。
        .on_drag(
            TabDrag { sid: drag_sid },
            |_d: &TabDrag, _cursor_offset, _w, cx: &mut gpui::App| cx.new(|_| DragGhost),
        )
        // 图标必须放 prefix，不能放`.icon()`：
        // 官方`Tab::render`里`match self.icon { Some(icon) => 只画 icon, None => 画 label }`，
        // 一旦设置`.icon()`，label 会被完全丢弃（官方把「有图标」当作 icon-only 定宽标签）。
        // 此前用`.icon()`导致所有 Tab 只有图标没有标题，用户看不出是哪个会话（2026-09-10 运行反馈）。
        .prefix(
            h_flex()
                .gap_1()
                .items_center()
                .pl(px(TAB_EDGE_INSET))
                .child(dot),
        )
        .suffix(
            h_flex()
                .items_center()
                .pr(px(TAB_EDGE_INSET))
                // 0. 透明覆盖层：只承载右键菜单，自身不注册左键处理。
                // 靠`TAB_MENU_OVERHANG`外扩 + Tab 根节点 overflow_hidden
                // 裁剪，命中范围正好等于整个 Tab。
                .child(
                    div()
                        .id(SharedString::from(format!("tab-ctx-{i}-{sid}")))
                        .absolute()
                        .left(px(-TAB_MENU_OVERHANG))
                        .top(px(-TAB_MENU_OVERHANG))
                        .right(px(-TAB_MENU_OVERHANG))
                        .bottom(px(-TAB_MENU_OVERHANG))
                        .context_menu(move |menu: PopupMenu, _w, _cx| {
                            tab_context_menu(menu, &menu_ctx)
                        }),
                )
                // 1. 关闭按钮：排在覆盖层之后，绘制在上层
                .child(
                    // 阻断冒泡：点关闭不触发 Tab 激活
                    div()
                        .debug_selector(|| "tab-close".to_string())
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .child(
                            Button::new(SharedString::from(format!("close-tab-{i}-{sid}")))
                                .ghost()
                                .xsmall()
                                .compact()
                                .icon(IconName::Close)
                                .on_click(on_close),
                        ),
                ),
        )
}

/// Tab 右键菜单的渲染期快照（菜单闭包要求`'static`，不能借用`MainView`）。
#[derive(Clone)]
struct TabMenuCtx {
    /// 目标 Tab 的 session id
    id: String,
    /// 标签显示名（菜单首行 label）
    title: SharedString,
    /// 是否为本地 Shell（决定「复制」的语义）
    local: bool,
    /// SSH 会话是否已保存为连接配置（决定「复制 / 连接」是否可用）
    has_profile: bool,
    /// 终端已挂载（会话已建立）
    connected: bool,
    /// 正在连接（占位态）
    connecting: bool,
    /// 标签总数（决定「关闭其他」是否可用）
    total: usize,
    /// 当前是否处于分屏态（决定「取消分屏」是否可用）
    in_split: bool,
}

/// 菜单项可用性矩阵（纯函数产物，便于单测）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TabMenuFlags {
    /// 复制标签：本地 Shell 恒可；SSH 需有连接配置
    duplicate: bool,
    /// 连接：SSH 需有连接配置；本地 Shell 恒可（那一栏其实是「重开一只 Shell」）。
    /// 两者都要求当前既没连上也不在连接中。
    connect: bool,
    /// 断开：仅当终端已挂载（会话已建立）
    disconnect: bool,
    /// 关闭其他：标签数 > 1
    close_others: bool,
}

fn tab_menu_flags(ctx: &TabMenuCtx) -> TabMenuFlags {
    TabMenuFlags {
        duplicate: ctx.local || ctx.has_profile,
        // 本地 Shell 也要给这一项：会话恢复里启动失败的那一格会停在「已断开」，
        // 占位卡上写着「右键标签可重新连接」（`render_connect_placeholder`），
        // 不给菜单项就是那句提示在骗人。落点 `reconnect_tab` 已按 kind 分流。
        connect: !ctx.connected && !ctx.connecting && (ctx.has_profile || ctx.local),
        disconnect: ctx.connected,
        close_others: ctx.total > 1,
    }
}

/// 构造单个 Tab 的右键菜单。
fn tab_context_menu(menu: PopupMenu, ctx: &TabMenuCtx) -> PopupMenu {
    let flags = tab_menu_flags(ctx);

    let id_dup = ctx.id.clone();
    let id_conn = ctx.id.clone();
    let id_disc = ctx.id.clone();
    let id_close = ctx.id.clone();
    let id_others = ctx.id.clone();
    let id_split_r = ctx.id.clone();
    let id_split_d = ctx.id.clone();

    let mut menu = menu;

    // 复制 / 连接 / 断开
    menu = menu.item(
        PopupMenuItem::new(rustshell_core::i18n::t("复制标签"))
            .icon(IconName::Copy)
            .disabled(!flags.duplicate)
            .on_click(move |_, w, cx| {
                let id = id_dup.clone();
                super::main_view::ui_call(
                    "",
                    move |m, _id, w, cx| m.duplicate_tab(&id, w, cx),
                    w,
                    cx,
                );
            }),
    );
    menu = menu.item(
        PopupMenuItem::new(rustshell_core::i18n::t("连接"))
            .icon(IconName::Play)
            .disabled(!flags.connect)
            .on_click(move |_, w, cx| {
                let id = id_conn.clone();
                super::main_view::ui_call(
                    "",
                    move |m, _id, _w, cx| m.reconnect_tab(&id, cx),
                    w,
                    cx,
                );
            }),
    );
    menu = menu.item(
        PopupMenuItem::new(rustshell_core::i18n::t("断开"))
            .icon(IconName::Minus)
            .disabled(!flags.disconnect)
            .on_click(move |_, w, cx| {
                let id = id_disc.clone();
                super::main_view::ui_call(
                    "",
                    move |m, _id, _w, cx| m.disconnect_tab(&id, cx),
                    w,
                    cx,
                );
            }),
    );

    // 分屏：把「本标签」当作主窗格，按方向开启/切换分屏。
    // 未分屏 → 开启分屏（副窗格自动取池中下一个标签，池中只有它一个新开同类型会话）；
    // 已分屏 → 只切换分屏方向，不动任何标签。
    menu = menu.separator().item(
        PopupMenuItem::new(rustshell_core::i18n::ts(
            format!("向右拆分 ({})", crate::keybinds::display_spec("mod-alt-r")),
            format!(
                "Split Right ({})",
                crate::keybinds::display_spec("mod-alt-r")
            ),
        ))
        .icon(IconName::PanelRight)
        .on_click(move |_, w, cx| {
            let id = id_split_r.clone();
            super::main_view::ui_call(
                "",
                move |m, _id, w, cx| {
                    m.activate_session(&id, cx);
                    m.split_pane(SplitAxis::Vertical, w, cx);
                },
                w,
                cx,
            );
        }),
    );
    menu = menu.item(
        PopupMenuItem::new(rustshell_core::i18n::ts(
            format!("向下拆分 ({})", crate::keybinds::display_spec("mod-alt-d")),
            format!(
                "Split Down ({})",
                crate::keybinds::display_spec("mod-alt-d")
            ),
        ))
        .icon(IconName::PanelBottom)
        .on_click(move |_, w, cx| {
            let id = id_split_d.clone();
            super::main_view::ui_call(
                "",
                move |m, _id, w, cx| {
                    m.activate_session(&id, cx);
                    m.split_pane(SplitAxis::Horizontal, w, cx);
                },
                w,
                cx,
            );
        }),
    );
    // 取消分屏：只清掉分屏态，两个会话都留在标签栏（不关闭任何会话）
    menu = menu.item(
        PopupMenuItem::new(rustshell_core::i18n::t("取消分屏"))
            .icon(IconName::PanelRightClose)
            .disabled(!ctx.in_split)
            .on_click(move |_, w, cx| {
                super::main_view::ui_call("", move |m, _id, _w, cx| m.unsplit(cx), w, cx);
            }),
    );

    // 关闭 / 关闭其他 / 关闭全部
    menu = menu.separator().item(
        PopupMenuItem::new(rustshell_core::i18n::t("关闭"))
            .icon(IconName::Close)
            .on_click(move |_, w, cx| {
                let id = id_close.clone();
                super::main_view::ui_call(
                    "",
                    move |m, _id, _w, cx| m.close_tab_by_session(&id, cx),
                    w,
                    cx,
                );
            }),
    );
    menu = menu.item(
        PopupMenuItem::new(rustshell_core::i18n::t("关闭其他"))
            .icon(IconName::CircleX)
            .disabled(!flags.close_others)
            .on_click(move |_, w, cx| {
                let id = id_others.clone();
                super::main_view::ui_call(
                    "",
                    move |m, _id, _w, cx| m.close_other_tabs(&id, cx),
                    w,
                    cx,
                );
            }),
    );
    menu.item(
        PopupMenuItem::new(rustshell_core::i18n::t("全部关闭"))
            .icon(IconName::Delete)
            .on_click(move |_, w, cx| {
                super::main_view::ui_call("", move |m, _id, _w, cx| m.close_all_tabs(cx), w, cx);
            }),
    )
}

impl MainView {
    pub fn render_tab_bar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let active_ix = self.tabs_state.active.as_deref().and_then(|a| {
            self.tabs_state
                .tabs
                .iter()
                .position(|tb| tb.session.id == a)
        });
        let total = self.tabs_state.tabs.len();

        // 标签多到放不下时，激活的那个必须留在视口内（否则「切过去了却看不见」）。
        // 只在激活标签变化时请求一次滚动，用户手动横向滚动不会被拽回，见 `TabScroll`。
        // 头部子元素数（指示器）由 `TabScroll` 按 children_count 现算，这里只给标签数。
        if let Some(ix) = active_ix {
            self.tabs_state
                .tab_scroll
                .follow(ix, self.tabs_state.tabs.len());
        }
        // 校验上一帧写入的偏移是否幸存（丢失则在预算内补写），并处理待滚动目标
        self.tabs_state.tab_scroll.sync();
        let t = crate::theme::current(cx);

        // 状态圆点子视图：按会话 id 取用（缺失才创建）。
        //
        // 每个圆点是一个**独立视图**，呼吸动画的 `cx.notify` 因此只重排那一小块
        // （见 [`TabStatusDot`]）。此前动画元素直接建在 `MainView::render_tab_bar` 里，
        // gpui 每 1/TAB_DOT_BREATH_FPS 秒 `notify(window.current_view())` =
        // notify(MainView) ⇒ 只要有一个已连接标签，整棵窗口树就永远以 20fps 重建。
        //
        // 先把「创建视图所需的数据」收集成不借用 self 的 Vec：随后的循环要同时
        // 可变借用 `self.tabs_state.tab_dots`（插入实体），不能在借用 `self.tabs_state.tabs` 的迭代器里做。
        let plan: Vec<(String, TabStatus, Hsla, bool)> = self
            .tabs_state
            .tabs
            .iter()
            .map(|tab| {
                let status = TabStatus::of(tab.is_live(), tab.is_connecting(), tab.is_dead());
                let (color, filled) = status.dot(&t);
                (tab.session.id.clone(), status, color, filled)
            })
            .collect();
        let mut dots: Vec<gpui::Entity<TabStatusDot>> = Vec::with_capacity(plan.len());
        for (sid, _status, color, filled) in &plan {
            let entity = match self.tabs_state.tab_dots.get(sid) {
                Some(e) => e.clone(),
                None => {
                    let (filled, color) = (*filled, *color);
                    let e = cx.new(move |cx| TabStatusDot::new(filled, color, cx));
                    self.tabs_state.tab_dots.insert(sid.clone(), e.clone());
                    e
                }
            };
            // 外观（主题色 / 连接状态）每帧同步进子视图；实体内部不订阅父视图，
            // 因此父视图的重绘不会反向把圆点标脏。
            entity.update(cx, |d, _| d.sync(*filled, *color));
            dots.push(entity);
        }
        // 已关闭标签的圆点实体必须回收：否则反复关开标签会持续累积视图与动画定时器
        self.tabs_state
            .tab_dots
            .retain(|id, _| plan.iter().any(|(sid, ..)| sid == id));

        // Tab 项：官方 Tab（状态圆点 + 类型图标 + 标题 + 关闭按钮 + 右击菜单覆盖层）
        let tabs: Vec<Tab> = self
            .tabs_state
            .tabs
            .iter()
            .enumerate()
            .map(|(i, tab)| {
                let display_title = crate::main_view::display_tab_title(tab, &self.conn_store);
                let title: SharedString = if display_title.trim().is_empty() {
                    rustshell_core::i18n::t("终端").to_string()
                } else {
                    display_title
                }
                .into();
                let sid = tab.session.id.clone();

                // 右键菜单快照（菜单闭包要求 'static，故此处取好所有权）
                let ctx = TabMenuCtx {
                    id: sid.clone(),
                    title,
                    local: tab.session.kind == rustshell_core::session::SessionKind::Local,
                    has_profile: tab.session.profile_id.is_some(),
                    // 断连态在标签栏必须**当未连接**：否则右键菜单给的动作会是
                    // 「断开」（点了没反应），而不是用户真正需要的「重新连接」。
                    connected: tab.is_live(),
                    connecting: tab.is_connecting(),
                    total,
                    in_split: self.is_split(),
                };
                let close_sid = sid;
                build_tab_item(
                    i,
                    ctx,
                    dots[i].clone().into_any_element(),
                    cx.listener(move |m, _e, _w, cx| {
                        m.close_tab_by_session(&close_sid, cx);
                    }),
                )
            })
            .collect();

        let mut bar = TabBar::new("main-tab-bar")
            // 2026-09-17 第二轮运行反馈「tab 改为官网 demo 那个样式」：整条切到官方
            // `Segmented` —— 选中项由官方指示器绘制（白色圆角胶囊 + 投影、弹簧滑动），
            // 标签自身不画任何选中态，因此也不再有顶部 accent 条与底部呼吸条。
            .with_variant(TabVariant::Segmented)
            // 2026-09-17 运行反馈「tab 字体过大 / tab 栏太高」：整栏降到`Size::Small`
            // （轨道 24px、胶囊 18px、字号`text_sm`）。再降一档到`XSmall`（20px）会让
            // 关闭按钮与轨道等高，悬停底色盖满整条标签。
            .with_size(TAB_SIZE)
            // 第一个标签离轨道左缘的内缩：官方轨道自带 4px，再补 `TAB_STRIP_LEFT_INSET`
            // （`prefix` 是轨道内、标签条之前的占位）—— 见 `TAB_STRIP_LEFT_INSET` 的说明。
            .prefix(tab_strip_prefix())
            .children(tabs)
            // tab 过多时官方溢出菜单（列出全部标签）。
            // 2026-09-17 保留理由：官方内层只做`overflow_x_scroll` + `lock_scroll_axis`，
            // 普通滚轮的纵向 delta 不会被换算成横向，没有这枚按钮，被挤出视口的标签
            // 在鼠标下就没有任何入口。
            .menu(true)
            .max_width(px(TAB_MAX_WIDTH))
            // 溢出横向滚动：官方把滚动放在内层`tabs-inner`（`overflow_x_scroll`），
            // 这里把句柄接上，才能在「标签多到放不下」时用触摸板 / 横向滚轮移动，
            // 也才能在被激活的标签滚出视口时把它带回可见区（见下方`scroll_to_item`）。
            // 注意：gpui-pre 的 overflow 容器只预留布局、不绘制滚动条，界面上看不到滑动轴 ——
            // 这正是 2026-09-17 第二轮反馈要的结果（第一轮补的覆盖层已整条撤掉）。
            .track_scroll(&self.tabs_state.tab_scroll.handle)
            // Segmented 轨道底色官方取主题 token（不透明）：2026-09-18 反馈
            // 「背景图只替换了连接栏」，改经 `panel_bg` 透出背景图。`TabBar`
            // 实现了 `Styled`，refinement 在官方 `.bg(..)` 之后应用、优先生效。
            .bg(crate::theme::surface(
                self.settings.panel_alpha(),
                t.bg_elevated,
            ))
            // 点击标签 = 放进「焦点窗格」。
            // 未分屏时与普通切换等价；分屏时若该标签不在任何窗格，
            // 则替换掉焦点窗格的槽位（被换下来的标签仍留在标签栏，不会被关闭）。
            .on_click(|ix, w, cx| {
                super::main_view::ui_call(
                    "",
                    |m, _id, _w, cx| {
                        if let Some(id) = m.tabs_state.tabs.get(*ix).map(|tb| tb.session.id.clone())
                        {
                            m.place_in_focused_pane(&id, cx);
                        }
                    },
                    w,
                    cx,
                );
            });
        if let Some(ix) = active_ix {
            bar = bar.selected_index(ix);
        }

        // 右侧：侧栏开关 + 终端全屏。**挂在轨道之外**（不进`TabBar`的 suffix）：
        // Segmented 的轨道底色画在`TabBar`根节点上、覆盖包括 suffix 在内的整条盒子，
        // 留在 suffix 里会被压在灰色轨道右端，像轨道自带的一枚按钮。
        // 2026-09-17 运行反馈：移除「向右拆分 / 向下拆分 / 新建本地终端」三个图标 ——
        // 拆分入口仍在标签右键菜单与 Ctrl+Alt+R / Ctrl+Alt+D，新建终端在 Ctrl+T
        // 与终端右键菜单，标签栏不必再摆一遍。
        let fullscreen = self.terminal_fullscreen;
        let mut trailing = h_flex().gap_1().items_center();
        // 全屏时侧栏开关**不画**：侧栏此刻已被全屏藏起来，这枚图标点下去画面
        // 不会有任何变化，却会悄悄改掉 `sidebar_collapsed` —— 用户退出全屏时才会
        // 发现侧栏没了，且找不到是哪一步弄丢的。
        if !fullscreen {
            trailing = trailing.child(
                Button::new("toggle-sidebar")
                    .ghost()
                    .xsmall()
                    .icon(IconName::PanelLeft)
                    .tooltip(rustshell_core::i18n::ts(
                        format!(
                            "{}  {}",
                            rustshell_core::i18n::t("切换侧栏"),
                            crate::keybinds::display_spec("mod-b")
                        ),
                        format!(
                            "Toggle Sidebar ({})",
                            crate::keybinds::display_spec("mod-b")
                        ),
                    ))
                    .on_click(cx.listener(|m, _e, _w, cx| {
                        m.panel_ui.sidebar_collapsed = !m.panel_ui.sidebar_collapsed;
                        cx.notify();
                    })),
            );
        }
        // 终端全屏：图标随状态换成「最大化 / 最小化」，点它 = 进出全屏
        // （窗口进系统全屏 + 竖栏/侧栏/AI 列让位，见 `MainView::toggle_terminal_fullscreen`）。
        // 全屏下它是**唯一**的鼠标出口，所以任何状态都必须画出来。
        let (fs_icon, fs_tip) = if fullscreen {
            (IconName::Minimize, rustshell_core::i18n::t("退出终端全屏"))
        } else {
            (IconName::Maximize, rustshell_core::i18n::t("终端全屏"))
        };
        trailing = trailing.child(
            Button::new("toggle-terminal-fullscreen")
                .ghost()
                .xsmall()
                // 供测试量「全屏后这枚出口还在不在」；
                // release 下是 no-op。
                .debug_selector(|| "toggle-terminal-fullscreen".to_string())
                .icon(fs_icon)
                .tooltip(fs_tip)
                .on_click(cx.listener(|m, _e, w, cx| m.toggle_terminal_fullscreen(w, cx))),
        );

        // 标签轨道外面再套一层：把**滚轮**接到横向滚动上（见 `scroll_tabs_by_wheel`）。
        // 官方内层是 `overflow_x_scroll + lock_scroll_axis`，纵向滚轮不会被换算成横向，
        // 鼠标在标签栏上滚等于没反应 —— 这正是「tab 过多无法移动到里面」。
        //
        // 这一层同时承担两种拖拽（见模块头）：
        // · `on_drag(BarPan)`：按下点**不在标签上**时起拖 = 整条横向平移（1:1 跟手）。
        //   按下点在标签上时，标签自己先建立 `TabDrag` 并 `stop_propagation`，
        //   这一层收不到事件，于是「拖标签换位 / 拖空白平移」互不打架。
        // · `on_drag_move::<TabDrag>`：换位。它是 window 级监听，指针拖出标签栏
        //   （横向出视口 / 纵向挪到终端上）也照收，靠会话 id 反查当前下标，幂等。
        let strip = div()
            // `.id()` 必须先给：`on_drag` 属于 `StatefulInteractiveElement`，
            // 只有 `Stateful<E>`（= `.id()` 的产物）才实现它，裸 `Div` 不实现。
            .id(SharedString::from("main-tab-strip"))
            .flex_1()
            .min_w_0()
            .on_scroll_wheel(cx.listener(|m, e: &ScrollWheelEvent, w, cx| {
                m.scroll_tabs_by_wheel(e, w, cx);
            }))
            .on_drag(BarPan, {
                // `weak_entity()` 而不是 `entity().downgrade()`：拖拽构造函数只在
                //「真的开始拖」那一刻被调用，用弱引用就不必赌 `MainView` 一定还活着。
                let me = cx.weak_entity();
                move |_d: &BarPan, _cursor_offset, _window, cx: &mut gpui::App| {
                    // 拖拽开始：清掉上一次平移留下的基准指针 x，
                    // 本次从「首帧只记基准、不产生位移」重新开始。
                    let _ = me.update(cx, |m, _| m.tabs_state.tab_pan_x = None);
                    cx.new(|_| DragGhost)
                }
            })
            .on_drag_move(cx.listener(|m, e: &DragMoveEvent<BarPan>, _w, cx| {
                m.drag_tab_bar_pan(f32::from(e.event.position.x), cx);
            }))
            .on_drag_move(cx.listener(|m, e: &DragMoveEvent<TabDrag>, _w, cx| {
                let sid = e.drag(cx).sid.clone();
                m.reorder_tab_dragged_to(&sid, f32::from(e.event.position.x), cx);
            }))
            .child(bar);

        tab_bar_shell(strip, trailing)
    }

    /// 标签栏「按住空白处拖动」的横向平移：把**本次**位移叠加到当前偏移上。
    ///
    /// 语义是「抓住纸拖」：指针右移就把内容一起右移（偏移向 0 走），到两端由
    /// [`pan_offset`] 夹住。用逐次位移而非「按下点 + 总位移」，是因为拖拽过程中
    /// 偏移本身就在被我们改写，再用累计总位移换算会越拖越快。
    ///
    /// 拖拽首帧只记基准（此时指针相对按下点刚超过 2px 阈值，丢掉这 2px 不可感知），
    /// 之后每一帧才真正产生位移。
    pub(super) fn drag_tab_bar_pan(&mut self, x: f32, cx: &mut Context<Self>) {
        let Some(prev) = self.tabs_state.tab_pan_x.replace(x) else {
            return;
        };
        let dx = x - prev;
        if dx == 0. {
            return;
        }
        let handle = &self.tabs_state.tab_scroll.handle;
        let cur = handle.offset();
        let target = pan_offset(f32::from(cur.x), dx, f32::from(handle.max_offset().x));
        if target == f32::from(cur.x) {
            return;
        }
        // 用户开始自己平移：放弃「待滚动目标 + 偏移补写」，别跟这只手抢位置。
        // 必须在 `set_offset` 之前调用（那两处状态是私有的，见 `TabScroll::user_took_control`）。
        self.tabs_state.tab_scroll.user_took_control();
        self.tabs_state
            .tab_scroll
            .handle
            .set_offset(point(px(target), cur.y));
        cx.notify();
    }

    /// 标签拖拽换位：把被拖的标签挪到指针当前压住的那一格。
    ///
    /// 载荷里只有**会话 id**（下标在拖拽过程中一帧一变），所以每帧现算 `from`；
    /// `to` 由上一帧的布局（`bounds_for_item`）算出来。两者都取自「当前状态」，
    /// 于是同一个位置被重复送达也只会得到同一个结果 —— 幂等，不会左右抖。
    pub(super) fn reorder_tab_dragged_to(&mut self, sid: &str, x: f32, cx: &mut Context<Self>) {
        let Some(from) = self
            .tabs_state
            .tabs
            .iter()
            .position(|tb| tb.session.id == sid)
        else {
            return; // 标签已被关掉（拖拽中按了关闭）：什么都不做
        };
        let Some(to) = self.tab_index_at_x(x) else {
            return;
        };
        if to == from {
            return;
        }
        let tab = self.tabs_state.tabs.remove(from);
        self.tabs_state.tabs.insert(to, tab);
        // 拖拽中不改 `active`：会话 id 没变，`active_ix` 由 id 现算（见 `render_tab_bar`），
        // 顺序变了也不会「切到别的会话」。
        cx.notify();
    }

    /// 指针横坐标落在第几个标签上（用上一帧布局现算）。
    ///
    /// 位置换算：`child_bounds` 是**未滚动**的内容坐标（见 `TabScroll::scroll_into_view`），
    /// 屏上区间 = 边界 + 当前偏移。挑格子的规则见 [`nearest_slot`]。
    fn tab_index_at_x(&self, x: f32) -> Option<usize> {
        let n = self.tabs_state.tabs.len();
        if n == 0 {
            return None;
        }
        let handle = &self.tabs_state.tab_scroll.handle;
        let off = f32::from(handle.offset().x);
        let leading = leading_children(handle, n);
        let mut ranges = Vec::with_capacity(n);
        for i in 0..n {
            // 首帧尚未布局：取不到边界的先跳过（本帧不动，下一帧再来）
            let Some(b) = handle.bounds_for_item(leading + i) else {
                continue;
            };
            ranges.push((f32::from(b.left()) + off, f32::from(b.right()) + off));
        }
        nearest_slot(&ranges, x)
    }

    /// 标签栏滚轮 → 横向滚动。
    ///
    /// 2026-09-17 运行反馈「tab 过多会被隐藏掉，无法移动选择到里面」：官方 `tabs-inner`
    /// 只吃**横向** delta（`overflow_x_scroll` + `lock_scroll_axis`），普通滚轮的纵向
    /// delta 落到它手里等于没发生。这里在外层接住同一个滚轮事件（官方那个监听器不停冒泡，
    /// 见 `div.rs` 的 `paint_scroll_listener`），把纵向 delta 折算成横向偏移；带 Shift 的
    /// 滚轮由 Windows 直接给成横向 delta，内层已经处理，这里会因为 delta 为 0 而不插手。
    ///
    /// 手动滚过之后**不重置** `last_scrolled`：那是「激活标签自动回到可见区」的去重键，
    /// 重置会让下一帧把用户刚滚出来的位置又拽回激活标签，变成「滚不动」。
    pub(super) fn scroll_tabs_by_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let delta_y = event.delta.pixel_delta(window.line_height()).y;
        let handle = &self.tabs_state.tab_scroll.handle;
        let current = handle.offset();
        if let Some(target_x) = wheel_scroll_target(current.x, delta_y, handle.max_offset().x) {
            handle.set_offset(point(target_x, current.y));
            cx.notify();
        }
    }
}

/// Tab 栏外壳：`[标签轨道（占满剩余宽度）][侧栏开关]` 横排。
///
/// 抽成独立函数是为了让护栏能渲染**这份真实写法**（见`tests`里的探针用例）——
/// 就地写在`render_tab_bar`里就只能靠源码串断言，而源码串恰恰拦不住
/// 「开关在、画面上什么都没有」这一形态的缺陷。
///
/// 上下留 4px（`py_1`）：`Segmented`的轨道上下内边距为 0（轨道要贴身包住胶囊），
/// 与上下两行的间距只能由外壳给；左右 8px 与侧栏分割线脱开（2026-09-16 反馈
/// 「tab 和侧边栏贴合」）。
///
/// `bar` 由调用方包好（`flex_1 + min_w_0` + 滚轮处理）：外壳不再自己套一层，
/// 免得滚轮监听落在「包住轨道的那一层」之外。
fn tab_bar_shell(bar: impl IntoElement, trailing: impl IntoElement) -> gpui::Div {
    h_flex()
        .items_center()
        .flex_shrink_0()
        // 定高（见 `TAB_SHELL_HEIGHT`）：空标签态与有标签态总高一致，不再跳变
        .h(px(TAB_SHELL_HEIGHT))
        .gap_1()
        .px(px(TAB_BAR_SHELL_INSET))
        .py_1()
        .child(bar)
        .child(trailing)
}

/// 滚轮 → 横向滚动目标偏移（纯函数，方向与夹取都能单测）。
///
/// 约定与 gpui 的 `Div` 一致：`offset.x` 越向右滚越负，可滚区间是 `[-max, 0]`
/// （`0` = 停在最左）。**注意 `max_offset().x` 的符号**：gpui-pre 0.3.4 里它是
/// 「可滚距离」的**正值幅度**（`content_size - viewport` 再 `max(0)`，见
/// `div.rs` 的 `clamp_scroll_position`；老 Zed 版本曾是负值，别改回去）。
/// 2026-09-18 实机数据 `max=216` 配 `offset=-210` 证实正值约定。
///
/// 滚轮向下（`delta_y < 0`，Windows 的 `WM_MOUSEWHEEL` 向上为正）应该看到
/// **更靠后**的标签，也就是偏移继续变小，所以是 `current + delta_y` —— 与官方
/// 处理纵向滚动时的 `scroll_offset.y += delta_y` 同源。
///
/// 返回 `None` = 没有位移（delta 为 0 / 已经到头），调用方据此免掉一次通知与重绘。
fn wheel_scroll_target(current_x: Pixels, delta_y: Pixels, max_x: Pixels) -> Option<Pixels> {
    if delta_y.is_zero() {
        return None;
    }
    let min_x = -max_x.max(px(0.));
    let target = (current_x + delta_y).clamp(min_x, px(0.));
    (target != current_x).then_some(target)
}

/// 标签栏的自动滚动记录（见 [`MainView::render_tab_bar`]）。
///
/// 「上次已滚过的激活标签下标」必须跨帧保留：若每帧都请求一次滚动，用户手动横向
/// 滚动把激活标签推出视口后，下一帧就会被拽回来，变成「滚不动」。只在激活标签
/// **变化**时请求一次，把滚动权交还用户。
///
/// # 为什么自己写偏移，而不用 `ScrollHandle::scroll_to_item`
///
/// 2026-09-18 反馈「溢出菜单选被隐藏的 tab 无法跳转过去」：实机日志显示
/// `scroll_to_item` 的写入**向右生效、向左丢失**（offset 停在 -210 不动），而无头
/// 测试里同一条 API 双向都通过 —— 它是「渲染期打标记、prepaint 消费」的两段式协议，
/// 在真实应用的动画/多视图帧时序下标记会被吞掉。无头环境复现不了，就不再赌协议：
/// 这里用 `bounds_for_item` 自己算出目标偏移、`set_offset` 直接写入（该值立刻对
/// 本帧 prepaint 的 clamp 与绘制可见），并在下一帧校验是否幸存，丢失就补写。
pub struct TabScroll {
    pub handle: gpui::ScrollHandle,
    pub last_scrolled: Option<usize>,
    /// 待滚动目标：`Some((激活标签下标, 请求时的标签总数, 剩余尝试帧数))`。
    /// 子元素下标每帧按「头部子元素数 = children_count − 标签数 − 1(菜单占位)」
    /// 现算（见 [`Self::sync`]），不缓存 —— 指示器胶囊是否已随弹簧就位、标签是否
    /// 增删，都会改变头部子元素数。
    pending_target: Option<(usize, usize, u8)>,
    /// 请求帧跳过标记：`follow` 发生在渲染期，此刻 `children_count` 还是**上一帧**
    /// 的布局结果（标签增删帧会对不上数）。置位后本帧跳过、下一帧再算 —— 那时
    /// `children_count` 已包含当前标签集，反推的头部子元素数才可靠。
    pending_deferred: bool,
    /// 期望偏移校验：`Some((期望的 offset.x, 剩余补写次数))`。写入后下一帧若发现
    /// 实际偏移对不上（被吞/被改），在预算内补写；归零后放弃（别跟用户抢滚轮）。
    verify: Option<(gpui::Pixels, u8)>,
}

/// 滚动容器里「标签之前的子元素个数」——头部（指示器相关）+ 标签 + 末尾菜单占位，
/// 后者恒在，于是 `标签起点 = children_count − 标签数 − 1`。
///
/// 头部子元素数**不是常数**（Segmented 稳态 2：视口等宽占位 + 停在选中位的胶囊；
/// 无头探针 1；胶囊还要等弹簧出宽度才出现），所以只能这样反推，不能写死。
/// 三处用同一份算式：`TabScroll::sync`（滚进可见区）与 `MainView::tab_index_at_x`
/// （拖拽换位），任何一处写死了 `+1`，另一处就会「差一两个身位」。
fn leading_children(handle: &gpui::ScrollHandle, n_tabs: usize) -> usize {
    handle.children_count().saturating_sub(n_tabs + 1)
}

impl TabScroll {
    pub fn new(handle: gpui::ScrollHandle) -> Self {
        Self {
            handle,
            last_scrolled: None,
            pending_target: None,
            pending_deferred: false,
            verify: None,
        }
    }

    /// 激活标签变了就请求把它滚进可见区；未变则保留用户当前的横向位置。
    ///
    /// `n_tabs` = 请求时的标签总数（`self.tabs_state.tabs.len()`），与 `children_count` 一起
    /// 反推「标签排在滚动容器子元素里的起点」：官方滚动容器 `tabs-inner` 的直接
    /// 子元素 = 头部（指示器相关）+ 各标签 + 末尾菜单占位。头部子元素数**不是常数**
    /// —— 2026-09-18 实机 `child_bounds` 实测 Segmented 稳态是 2（视口等宽占位 +
    /// 停在选中位的胶囊），无头探针里是 1；胶囊还要等弹簧出宽度后才出现。硬编码
    /// 任何一档都会像 `+1` 那版一样「向右看似正常、向左差一两个身位」—— 那正是
    /// 「溢出菜单选隐藏 tab 无法跳转过去」的根因。
    fn follow(&mut self, active_ix: usize, n_tabs: usize) {
        if self.last_scrolled != Some(active_ix) {
            self.last_scrolled = Some(active_ix);
            self.pending_target = Some((active_ix, n_tabs, Self::TARGET_TRIES));
            self.pending_deferred = true;
        }
    }

    /// 渲染期每帧调用（渲染 = 本帧 prepaint **之前**，此刻写入的偏移对
    /// 本帧的 clamp 与绘制立即可见）：先校验补写上一帧的偏移，再处理待滚动目标。
    fn sync(&mut self) {
        // 1. 校验：上一帧写入的期望偏移还在吗？不在且预算未尽就补写。
        //    （`Option<(Pixels, u8)>` 是 Copy：先拷出来，避免借用中赋值。）
        if let Some((expected, mut tries_left)) = self.verify {
            let current = self.handle.offset().x;
            if (f32::from(current) - f32::from(expected)).abs() <= 0.5 {
                self.verify = None;
            } else if tries_left > 0 {
                tries_left -= 1;
                self.verify = Some((expected, tries_left));
                self.handle
                    .set_offset(point(expected, self.handle.offset().y));
            } else {
                self.verify = None;
            }
        }
        // 2. 待滚动目标：请求帧只登记不处理（等 children_count 刷新），此后每帧
        //    按当前布局现算目标下标；边界未就绪（首帧布局未完成）则下一帧再试。
        if let Some((ix, n_tabs, mut tries_left)) = self.pending_target {
            if self.pending_deferred {
                self.pending_deferred = false;
                return;
            }
            let leading = leading_children(&self.handle, n_tabs);
            match self.scroll_into_view(leading + ix) {
                ScrollStep::Ready(None) => {
                    self.pending_target = None;
                }
                ScrollStep::Ready(expected @ Some(_)) => {
                    self.pending_target = None;
                    self.verify = Some((expected.unwrap(), Self::VERIFY_TRIES));
                }
                ScrollStep::NotReady if tries_left > 0 => {
                    tries_left -= 1;
                    self.pending_target = Some((ix, n_tabs, tries_left));
                }
                ScrollStep::NotReady => {
                    self.pending_target = None;
                }
            }
        }
    }

    /// 用户自己开始横向平移（按住标签栏空白处拖动，见 [`BarPan`]）：放弃
    /// 「待滚动目标」与「偏移补写」，别跟这只手抢位置。
    ///
    /// 不清 `last_scrolled`：它只是「激活标签变化时才自动跟随」的去重键，
    /// 清掉会让下一帧把用户刚拖出来的位置又拽回激活标签上（同 `scroll_tabs_by_wheel`
    /// 的说明）—— 拖完松手不该被回弹。
    fn user_took_control(&mut self) {
        self.verify = None;
        self.pending_target = None;
    }

    /// 每阶段的最大等待帧数：真实布局 1–2 帧内完成，5 帧是宽裕的余量。
    const TARGET_TRIES: u8 = 5;
    /// 偏移补写的最大次数：只用于对抗帧时序丢失，不跟用户的滚轮抢位置。
    const VERIFY_TRIES: u8 = 3;

    /// 计算并把第 `target` 个直接子元素滚进视口。
    ///
    /// `child_bounds` 是**未滚动**的内容坐标（见 gpui-pre `Div::prepaint`：边界在
    /// 本容器的滚动偏移入栈前采集），所以「屏上位置 = 子边界 ± 当前偏移」；
    /// 目标偏移 = 让目标完整落入视口所需的最小位移，天然落在合法区间
    /// `[-max_offset.x, 0]` 内，不会被下一帧的 clamp 弹回。
    fn scroll_into_view(&mut self, target: usize) -> ScrollStep {
        let handle = &self.handle;
        let Some(item) = handle.bounds_for_item(target) else {
            return ScrollStep::NotReady;
        };
        let view = handle.bounds();
        // 还没完成过一次布局（视口为零）：下一帧再算
        if f32::from(view.size.width) <= 0. {
            return ScrollStep::NotReady;
        }
        let off = handle.offset();
        let mut x = off.x;
        if item.left() + off.x < view.left() {
            x = view.left() - item.left();
        } else if item.right() + off.x > view.right() {
            x = view.right() - item.right();
        }
        if x == off.x {
            return ScrollStep::Ready(None); // 已在视口内，无需滚动
        }
        handle.set_offset(point(x, off.y));
        ScrollStep::Ready(Some(x))
    }
}

/// [`TabScroll::scroll_into_view`] 的结果。
enum ScrollStep {
    /// 边界尚未就绪（`bounds_for_item` 取不到 / 视口还没量出尺寸）。
    NotReady,
    /// 边界就绪。`None` = 目标已在视口内无需滚动；`Some(x)` = 已写入偏移 `x`。
    Ready(Option<gpui::Pixels>),
}

#[cfg(test)]
mod tests {
    use super::{tab_menu_flags, TabMenuCtx, TabMenuFlags, TabStatus};
    use crate::test_support::prod;
    use gpui::{
        div, point, px, Bounds, Context, InteractiveElement as _, IntoElement, MouseButton,
        MouseDownEvent, ParentElement as _, Pixels, Quad, Render, SharedString, Styled as _,
        Window,
    };
    use gpui_component::{
        tab::{Tab, TabBar, TabVariant},
        Sizable as _,
    };

    /// 生产代码正文（剥注释 + 切测试模块，见 `crate::test_support`）。
    fn production_source() -> String {
        prod(include_str!("tab_bar.rs"))
    }

    /// 回归（2026-09-17 运行反馈）：标签栏右侧不再有「向右拆分 / 向下拆分 /
    /// 新建本地终端」三个图标按钮。
    ///
    /// 这三个入口仍然存在（标签右键菜单、终端右键菜单、`Ctrl+Alt+R/D`、`Ctrl+T`），
    /// 只是不该在标签栏上再占位置。
    #[test]
    fn tab_bar_no_longer_carries_split_and_new_tab_buttons() {
        let code = production_source();
        for needle in ["split-right", "split-down", "new-tab"] {
            assert!(
                !code.contains(needle),
                "标签栏仍残留 `{needle}` 按钮；拆分与新建入口只保留在右键菜单与快捷键里。"
            );
        }
        // 反向：侧栏开关必须还在（那是标签栏上唯一该留的图标）
        assert!(
            code.contains("toggle-sidebar"),
            "标签栏必须保留侧栏开关，否则连最后这个入口也没了。"
        );
    }

    /// 终端全屏按钮（2026-09-22 用户要求「切换侧栏图标右侧增加全屏图标」）。
    ///
    /// 结构层三条：按钮在、点击调的是 [`MainView::toggle_terminal_fullscreen`]
    /// （而不是只翻一下本地标记 —— 那样窗口不会真的进系统全屏）、图标与提示词
    /// 随状态换；外加一条「全屏时侧栏开关不画」：它此刻点下去画面不会变，
    /// 却会悄悄改掉 `sidebar_collapsed`，用户退出全屏才发现侧栏没了。
    ///
    /// 行为面（全屏时这枚出口真的还在、三列真的让位、退出后宽度还原）由
    /// `main_view` 的 `terminal_fullscreen_hides_the_chrome_and_puts_it_back` 量实宽。
    #[test]
    fn fullscreen_toggle_sits_right_of_the_sidebar_toggle() {
        let code = production_source();

        assert!(
            code.contains(r#"Button::new("toggle-terminal-fullscreen")"#),
            "标签栏右侧必须有终端全屏按钮。"
        );
        assert!(
            code.contains("m.toggle_terminal_fullscreen(w, cx)"),
            "全屏按钮必须走 `MainView::toggle_terminal_fullscreen` —— 它同时管系统全屏\n\
             与 `terminal_fullscreen`；只改本地标记等于图标变了窗口没变。"
        );
        for needle in [
            "IconName::Maximize",
            "IconName::Minimize",
            r#"t("终端全屏")"#,
            r#"t("退出终端全屏")"#,
        ] {
            assert!(
                code.contains(needle),
                "全屏按钮少了 `{needle}`：图标与提示词都要随全屏状态换。"
            );
        }

        // 用户要的是「**右侧**增加全屏图标」：顺序 = 侧栏开关先挂、全屏后挂。
        let sidebar = code
            .find(r#"Button::new("toggle-sidebar")"#)
            .expect("找不到侧栏开关");
        let fullscreen = code
            .find(r#"Button::new("toggle-terminal-fullscreen")"#)
            .expect("找不到终端全屏按钮");
        assert!(
            sidebar < fullscreen,
            "全屏图标必须排在侧栏开关右侧（挂进 `trailing` 的先后顺序）。"
        );

        // 全屏时侧栏开关不画（侧栏已被全屏藏起来，点了没反应还会污染用户意图）。
        let guard = code
            .find("if !fullscreen {")
            .expect("找不到「全屏时不画侧栏开关」的守卫");
        assert!(
            guard < sidebar,
            "侧栏开关必须被 `if !fullscreen {{ .. }}` 守卫住 —— 全屏下它点了没用，\n\
             却会把 `sidebar_collapsed` 改掉，用户退出全屏才会发现侧栏没了。"
        );
    }

    /// 溢出横向滚动：滚动句柄必须真的接到官方 `TabBar` 上，并且
    /// 「激活标签变化时请求把它滚进可见区」这条逻辑要能被读到。
    ///
    /// 只做源码级断言：横向滚动是「滚轮 + 布局盒」的运行时行为，
    /// 无头环境里既没有滚轮也没有真实视口宽度，硬测只会得到假绿。
    ///
    /// 2026-09-17 第二轮反馈「移除 tab 滑动轴」之后，**没有**滚动条本体可量：
    /// 界面上不该再出现任何横向滑块，故这一条只剩「句柄接上了」与「滑动轴不存在」。
    #[test]
    fn tab_bar_tracks_scroll_for_overflow() {
        let code = production_source();
        assert!(
            code.contains("track_scroll"),
            "官方 `TabBar` 的内层滚动容器必须接上句柄，否则标签溢出时无法横向移动。"
        );
        assert!(
            // ★ 锚点必须取**源码里真实出现的连续片段**，不能取「逻辑上连续」的串。
            //   rustfmt 把这条链折成三行：`self.tabs_state` / `.tab_scroll` / `.follow(ix, ..)`，
            //   所以 `tab_scroll.follow(` 这类整串恒不存在 ⇒ 断言恒假。
            //   这里只钉两段各自存在 —— 折行位置是 rustfmt 的决定，会变；
            //   哪一段该出现在哪条链上是语义，由下面第三条断言（`follow(ix,` 的实参）兜。
            code.contains("self.tabs_state"),
            "标签滚动句柄必须挂在 `tabs_state` 上（刀三归组后 `MainView` 不再有平级 `tab_scroll`）。"
        );
        assert!(
            code.contains(".follow(ix,"),
            "激活标签变化时必须请求把它滚进可见区（`TabScroll::follow` 是唯一落点）。"
        );
        // 每帧都请求就会把用户的横向滚动拽回来 到 必须带「上次已滚过的下标」守卫
        assert!(
            code.contains("last_scrolled"),
            "自动滚动必须有跨帧去重，否则用户手动滚动会被每帧拽回。"
        );
        // 2026-09-17 第二轮运行反馈「移除 tab 滑动轴」：第一轮补的那条覆盖层滚动条
        // （`Scrollbar::horizontal` + `tab-scrollbar-layer`）与官方 Segmented 轨道相冲，
        // 已整条撤掉。横向移动仍由官方内层`overflow_x_scroll` + `track_scroll` 承担
        // （鼠标的溢出入口见`menu(true)` 的标签菜单）。
        for needle in ["tab-scrollbar", "Scrollbar::horizontal", "ScrollbarMode"] {
            assert!(
                !code.contains(needle),
                "tab 栏又长出了滑动轴（`{needle}`）；用户要求移除，溢出改走官方标签菜单。"
            );
        }
    }

    /// 回归（2026-09-17 运行反馈「tab 改为官网 demo 样式」）：标签的**自绘装饰**必须
    /// 全部落在标签自己的盒子里，也即落在官方 Segmented 胶囊内。
    ///
    /// 关于胶囊本身：`Segmented` 的白色胶囊由官方指示器按 `bounds.tabs[ix].size`
    /// 直接铺出来 —— 它的宽度**由框架给定，应用代码改不动**。应用侧唯一能搞坏它的事，
    /// 就是让状态圆点 / 关闭按钮跑到标签盒子之外（那样胶囊画得再宽，装饰也露在外面）。
    /// 所以这里量的是「装饰是否越界」，而不是去数那一块颜色：
    ///
    /// 曾试过直接断言「画布上存在一块覆盖整标签宽的胶囊色 quad」，在无头测试里不可靠 ——
    /// 指示器的宽度来自 `spring_move` 弹簧（`render_indicator` 里 `spring(..)`），
    /// 只 `advance_clock` 不重绘、或只重绘不推进时钟，量到的都是弹簧起步那一帧。
    #[gpui::test]
    fn tab_decorations_stay_inside_the_tab_box(cx: &mut gpui::TestAppContext) {
        let vcx = bar_probe(cx, true);
        let tab = vcx.debug_bounds("probe-tab").expect("标签未渲染");
        let dot = vcx.debug_bounds("tab-status-dot").expect("状态圆点未渲染");
        let close = vcx.debug_bounds("tab-close").expect("关闭按钮未渲染");

        for (what, b) in [("状态圆点", dot), ("关闭按钮", close)] {
            assert!(
                b.top() >= tab.top() && b.bottom() <= tab.bottom(),
                "{what}纵向越界（{:?}..{:?} / 标签 {:?}..{:?}）",
                b.top(),
                b.bottom(),
                tab.top(),
                tab.bottom()
            );
            assert!(
                b.left() >= tab.left() && b.right() <= tab.right(),
                "{what}横向越界（{:?}..{:?} / 标签 {:?}..{:?}）",
                b.left(),
                b.right(),
                tab.left(),
                tab.right()
            );
        }
        // 圆点在前、关闭在后，且都在标签内 —— 三者共同撑出「胶囊覆盖整标签」的观感。
        assert!(
            dot.right() <= close.left(),
            "状态圆点排到了关闭按钮后面（圆点 {:?} / 关闭 {:?}）",
            dot.right(),
            close.left()
        );
    }

    /// 回归（-bug）：右击菜单覆盖层必须覆盖整个 Tab，且不得吃掉左键选中。
    #[gpui::test]
    fn context_overlay_covers_whole_tab(cx: &mut gpui::TestAppContext) {
        use std::cell::RefCell;
        use std::rc::Rc;

        /// 探针视图：一条 TabBar + 一个由生产代码构建的 Tab。
        struct Probe {
            clicked: Rc<RefCell<Vec<usize>>>,
            closed: Rc<RefCell<usize>>,
        }

        impl Render for Probe {
            fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                let clicked = self.clicked.clone();
                let closed = self.closed.clone();
                let ctx = TabMenuCtx {
                    id: "s1".into(),
                    title: "Terminal".into(),
                    local: true,
                    has_profile: false,
                    connected: true,
                    connecting: false,
                    total: 2,
                    in_split: false,
                };
                TabBar::new("probe")
                    // 与生产代码同一变体/尺寸：探针若用别的档位，量到的标签就不是
                    // 屏幕上那一条（Segmented 的轨道还带 3px 左右内边距），
                    // 覆盖层的命中范围与标签边界是否吻合也就验不出来了。
                    .with_variant(TabVariant::Segmented)
                    .with_size(super::TAB_SIZE)
                    .max_width(px(super::TAB_MAX_WIDTH))
                    .on_click(move |ix, _w, _cx| clicked.borrow_mut().push(*ix))
                    .child(
                        super::build_tab_item(
                            0,
                            ctx,
                            super::static_hollow_dot(crate::theme::to_hsla(
                                crate::theme::current(cx).fg_muted,
                            )),
                            move |_e, _w, _cx| *closed.borrow_mut() += 1,
                        )
                        .debug_selector(|| "probe-tab".to_string()),
                    )
            }
        }

        cx.update(|cx| {
            gpui_component::init(cx);
            crate::theme::apply_theme_mode(cx, rustshell_core::theme::ThemeMode::Dark);
        });
        // 每点用一个全新窗口：避免上一轮菜单未关闭造成的状态污染，
        // 也让每次断言都从「当前无焦点」这个干净前置条件出发。
        // 覆盖层最容易漏掉的是 Tab 的最左端（缺陷现场 x<115 全无响应），
        // 因此三个采样点取 1% / 50% / 97% 宽度。
        for frac in [0.01f32, 0.5, 0.97] {
            let clicked = Rc::new(RefCell::new(Vec::new()));
            let closed = Rc::new(RefCell::new(0usize));
            let (_, vcx) = cx.add_window_view({
                let clicked = clicked.clone();
                let closed = closed.clone();
                move |_, _| Probe { clicked, closed }
            });
            vcx.run_until_parked();

            let tab = vcx.debug_bounds("probe-tab").expect("Tab 未渲染");
            let tab_w = f32::from(tab.size.width);
            assert!(tab_w > 100., "Tab 宽度 {tab_w} 太窄，测不出覆盖范围问题");
            let y = tab.origin.y + tab.size.height / 2.0;
            // 避开关闭按钮本体（它在最右侧 ~32px 内，由下一段单独验证）。
            // 采样点必须叠上标签左缘：`Segmented` 的轨道自带左右内边距，
            // 标签并不从窗口 x=0 开始，用纯相对量会打到轨道空白处。
            let x = tab.origin.x + px((tab_w * frac).clamp(1.0, tab_w - 40.));
            let pos = point(x, y);

            vcx.update(|w, cx| assert!(w.focused(cx).is_none(), "前置条件：初始不应有焦点"));

            // 1. 左键：点击必须送达`TabBar::on_click`（覆盖层不得拦截）。
            // 放在右击之前，菜单打开时左键会先被用于关闭菜单。
            vcx.simulate_click(pos, Default::default());
            vcx.run_until_parked();
            assert_eq!(
                clicked.borrow().as_slice(),
                &[0],
                "x={x}（Tab 宽 {tab_w}）处左键未能选中 Tab —— 覆盖层吞掉了点击"
            );

            // 2. 关闭按钮：排在覆盖层之后绘制，仍须可点。
            vcx.simulate_click(point(tab.origin.x + px(tab_w - 14.), y), Default::default());
            vcx.run_until_parked();
            assert_eq!(*closed.borrow(), 1, "关闭按钮被覆盖层挡住");

            // 3. 右击：菜单必须真的弹出（以焦点被菜单接管为证）。
            vcx.simulate_event(MouseDownEvent {
                button: MouseButton::Right,
                position: pos,
                modifiers: Default::default(),
                click_count: 1,
                first_mouse: false,
            });
            vcx.run_until_parked();
            vcx.update(|w, cx| {
                _ = w.draw(cx);
            });
            vcx.update(|w, cx| {
                assert!(
                    w.focused(cx).is_some(),
                    "x={x}（Tab 宽 {tab_w}）处右击未弹出菜单 —— 覆盖层没覆盖到这里"
                );
            });
        }
    }

    /// 探针视图：一条 TabBar + 一个由生产代码构建的标签（可切换选中态）。
    struct BarProbe {
        selected: bool,
        /// 直接喂给 [`super::paint_status_dot`] 的相位（0.0 = 谷值 / 1.0 = 峰值）。
        ///
        /// 圆点外观是相位的**纯函数**，测试想量峰值就传 1.0 —— 不必再去推虚拟时钟：
        /// 相位来自真实墙钟（见 `paint_status_dot` 的说明），推不动。
        phase: f32,
    }

    impl Render for BarProbe {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let t = crate::theme::current(cx);
            let probe_ctx = TabMenuCtx {
                id: "s1".into(),
                title: "Terminal".into(),
                local: true,
                has_profile: false,
                connected: true,
                connecting: false,
                total: 1,
                in_split: false,
            };
            let mut bar = TabBar::new("probe-bar")
                // 与生产代码同一变体：探针若还用`TabVariant::Tab`，量到的标签外观
                // 就不是屏幕上那一条，胶囊/高度之类的断言也就验不出来了。
                .with_variant(TabVariant::Segmented)
                .with_size(super::TAB_SIZE)
                // 生产代码给轨道补的左内缩，探针必须一起带上 —— 否则
                // `tab_strip_starts_clear_of_the_track_edge` 量的是另一条轨道。
                .prefix(super::tab_strip_prefix())
                .max_width(px(super::TAB_MAX_WIDTH));
            if self.selected {
                bar = bar.selected_index(0);
            }
            bar.child(
                super::build_tab_item(
                    0,
                    probe_ctx,
                    // 探针量的是圆点占位尺寸，与生产路径同一个渲染函数
                    // （`paint_status_dot` 是动画闭包与静止态共用的那一个）
                    super::paint_status_dot(
                        super::dot_slot(),
                        self.phase,
                        crate::theme::to_hsla(t.accent),
                    )
                    .into_any_element(),
                    |_, _, _| {},
                )
                .debug_selector(|| "probe-tab".to_string()),
            )
        }
    }

    /// 单窗口探针：返回可视测试上下文（圆点停在**谷值**相位）。
    fn bar_probe(cx: &mut gpui::TestAppContext, selected: bool) -> &mut gpui::VisualTestContext {
        bar_probe_at(cx, selected, 0.0)
    }

    /// 同上，但指定圆点相位（0.0 = 谷值 / 1.0 = 峰值）。
    fn bar_probe_at(
        cx: &mut gpui::TestAppContext,
        selected: bool,
        phase: f32,
    ) -> &mut gpui::VisualTestContext {
        cx.update(|cx| {
            gpui_component::init(cx);
            crate::theme::apply_theme_mode(cx, rustshell_core::theme::ThemeMode::Dark);
        });
        let (_, vcx) = cx.add_window_view(move |_, _| BarProbe { selected, phase });
        vcx.run_until_parked();
        vcx
    }

    /// 标签高度必须等于 `TAB_ROW_HEIGHT`（也即当前 `TAB_SIZE` 档位的官方高度）。
    ///
    /// 这条断言把「把 `TAB_SIZE` 改成别的档位却忘了同步 `TAB_ROW_HEIGHT`」变成红灯：
    /// 两者一旦分叉，`TAB_ROW_HEIGHT` 描述的就不是屏幕上那一条的高度了。
    /// 2026-09-17 第六轮 `TAB_SIZE` 回到`Medium`（32px）后，原来那条「必须比官方默认档
    /// 更矮」的反向断言随之作废 —— 档位是用户选的，护栏只管住「两个数成对」。
    #[gpui::test]
    fn tab_height_matches_the_row_height_constant(cx: &mut gpui::TestAppContext) {
        let vcx = bar_probe(cx, true);
        let tab = vcx.debug_bounds("probe-tab").expect("标签未渲染");
        assert_eq!(
            tab.size.height,
            px(super::TAB_ROW_HEIGHT),
            "标签实际高度与 TAB_ROW_HEIGHT 不一致"
        );
        // 档位必须落在官方四档之内（XSmall 20 / Small 24 / Medium 32 / Large 40）：
        // 数值对不上说明改的不是官方档位，而是别的地方把高度撑开了。
        assert!(
            [20., 24., 32., 40.].contains(&f32::from(tab.size.height)),
            "标签高度 {} 不是官方四档之一 —— 高度被别的东西撑开了",
            f32::from(tab.size.height)
        );
    }

    /// 回归（2026-09-17 运行反馈「x 图标挡住了上下的光条」）：关闭按钮必须完整落在标签内。
    ///
    /// 现场缺陷：标签 20px（`Size::XSmall`）而图标按钮同样是 20px（官方图标按钮
    /// `Size::XSmall`即`size_5`）—— 两者等高，按钮的悬停底色正好盖满整条标签。
    /// `Segmented` 的胶囊只有 18px 高（24px 标签内上下的 3px 是轨道），按钮比它还高，
    /// 更不能再让按钮越出标签边缘。
    #[gpui::test]
    fn close_button_stays_inside_the_tab(cx: &mut gpui::TestAppContext) {
        let vcx = bar_probe(cx, true);
        let tab = vcx.debug_bounds("probe-tab").expect("标签未渲染");
        let close = vcx.debug_bounds("tab-close").expect("关闭按钮未渲染");
        assert!(
            close.top() >= tab.top() && close.bottom() <= tab.bottom(),
            "关闭按钮溢出了标签（按钮 {:?}..{:?} / 标签 {:?}..{:?}）",
            close.top(),
            close.bottom(),
            tab.top(),
            tab.bottom()
        );
    }

    /// 连接状态圆点必须挂在**每个**标签上（选中与否都要能看出该会话连没连上），
    /// 且三样自绘覆盖层（底部呼吸横条 / 顶部选中条 / 切换高亮）一根都不能再渲染 ——
    /// 选中态已完全交给官方 Segmented 指示器。
    #[gpui::test]
    fn tabs_carry_a_status_dot_and_no_self_drawn_overlay(cx: &mut gpui::TestAppContext) {
        let vcx = bar_probe(cx, false);
        assert!(
            vcx.debug_bounds("tab-status-dot").is_some(),
            "非选中标签上缺少连接状态圆点"
        );
        for gone in ["tab-status-bar", "tab-active-bar", "tab-switch-flash"] {
            assert!(
                vcx.debug_bounds(gone).is_none(),
                "自绘覆盖层 `{gone}` 仍在渲染；选中态应完全由官方 Segmented 指示器承担"
            );
        }
    }

    /// 回归（2026-09-17 运行反馈「tab 标题去掉图标」）：标签前缀里不再画类型图标。
    ///
    /// 只看生产源码里还有没有 `Icon::new(`：关闭按钮走 `Button::icon(IconName::Close)`、
    /// 右键菜单项走 `PopupMenuItem::icon(..)`，都不经过它。所以本文件一旦出现
    /// `Icon::new(`，就说明又有人在标签里塞了自绘图标。
    #[test]
    fn tab_titles_carry_no_kind_icon() {
        let code = production_source();
        assert!(
            !code.contains("Icon::new("),
            "标签栏又画了自绘图标；标题区只保留状态圆点 + 标题。"
        );
        assert!(
            !code.contains("IconName::SquareTerminal") && !code.contains("IconName::Globe"),
            "本地/SSH 的类型图标不该出现在标签栏源码里。"
        );
    }

    /// 状态圆点由「会话是否可用 / 是否在连接 / 是否断连」三个信号推导。
    ///
    /// 优先级是硬约束：**可用 > 断连 > 连接中**。
    /// - 可用优先：重连成功那一帧 `Connecting` 还没落回去，但终端已经能用了；
    /// - 断连优先于连接中：断连态保留终端视图，`is_connecting()` 在「断连后原地
    ///   重连」时同时为真，此时红点比黄点更有信息量（用户已经知道它在重连，
    ///   他是从终端窗格底部那条断连条知道的）。
    #[test]
    fn tab_status_derives_from_session_state() {
        assert_eq!(TabStatus::of(true, false, false), TabStatus::Connected);
        assert_eq!(TabStatus::of(true, true, false), TabStatus::Connected);
        // 断连：live=false 且 dead=true —— 空心红点，标签栏里唯一的「出事了」信号
        assert_eq!(TabStatus::of(false, true, true), TabStatus::Dead);
        assert_eq!(TabStatus::of(false, false, true), TabStatus::Dead);
        assert_eq!(TabStatus::of(false, false, false), TabStatus::Idle);
        // 断连态不可能 live（保留终端视图 != 能用），两者互斥
        assert_ne!(TabStatus::of(false, true, true), TabStatus::Connecting);
    }

    /// 呼吸缓动必须是平滑往返：两端为谷、周期正中为峰，且全程落在 0..1。
    #[test]
    fn breath_easing_is_a_smooth_round_trip() {
        assert_eq!(super::breath_easing(0.0), 0.0);
        assert_eq!(super::breath_easing(0.5), 1.0);
        assert!((super::breath_easing(1.0)).abs() < 1e-6);
        for i in 0..=20 {
            let v = super::breath_easing(i as f32 / 20.0);
            assert!((0.0..=1.0).contains(&v), "缓动越界：{v}");
        }
    }

    /// 2026-09-17 第五／第六轮运行反馈：呼吸灯「微微放大，不要变暗淡」，且做法是
    /// 光晕 + 缩放 + 外扩光环。
    ///
    /// 这条量的是**呼吸本身**（纯函数层）：直径谷值恰好是常态直径、峰值是常态 + 幅度、
    /// 全程单调不减；柔光两端都 > 0（**灯芯不会熄灭**）；只有外扩光环的透明度一路降到
    /// 0（其余部分都是「扩散出去」的那一圈）。
    #[test]
    fn breath_grows_the_dot_and_never_dims_it() {
        let rest = super::dot_pulse(0.0);
        assert_eq!(
            rest.dot,
            super::TAB_STATUS_DOT,
            "呼吸谷值必须正好是常态直径"
        );
        assert_eq!(
            super::dot_pulse(1.0).dot,
            super::TAB_STATUS_DOT + super::TAB_DOT_BREATH_GROW,
            "呼吸峰值必须是常态直径 + 幅度"
        );
        // 「微微」的量化上限：涨幅超过 2px，6px 的圆点会被读成闪烁。
        assert!(
            super::dot_pulse(1.0).dot - rest.dot <= 2.0,
            "呼吸涨幅超过 2px 就不是「微微放大」了"
        );
        // 外扩光环必须真的扩出去：峰值直径比圆点本身大出两倍的扩散量。
        assert_eq!(
            super::dot_pulse(1.0).halo,
            super::dot_pulse(1.0).dot + 2. * super::TAB_DOT_HALO_SPREAD,
            "光环峰值必须是「圆点 + 每边一个扩散量」"
        );

        let peak = super::dot_pulse(1.0);
        let mut prev_dot = f32::MIN;
        let mut prev_halo = f32::MIN;
        let mut prev_alpha = f32::MAX;
        for i in 0..=20 {
            let p = super::dot_pulse(i as f32 / 20.0);
            assert!(
                p.dot >= prev_dot && p.dot >= rest.dot,
                "圆点直径必须单调不减且不小于常态：相位 {i}/20 处 {} < {prev_dot}",
                p.dot
            );
            assert!(
                p.glow >= super::TAB_DOT_GLOW_MIN && p.glow <= super::TAB_DOT_GLOW_MAX,
                "柔光透明度越界：相位 {i}/20 处 {}",
                p.glow
            );
            assert!(
                p.glow > 0.0,
                "柔光不能降到 0：用户要求「不要变暗淡」，灯芯任何相位都必须在发光"
            );
            assert!(
                p.halo >= prev_halo,
                "光环直径必须单调向外扩：相位 {i}/20 处 {} < {prev_halo}",
                p.halo
            );
            assert!(
                p.halo_alpha <= prev_alpha,
                "光环透明度必须单调淡出：相位 {i}/20 处 {} > {prev_alpha}",
                p.halo_alpha
            );
            prev_dot = p.dot;
            prev_halo = p.halo;
            prev_alpha = p.halo_alpha;
        }
        assert!(
            peak.halo_alpha.abs() < 1e-6,
            "光环扩到最大时必须淡尽（否则会出现一圈不消失的描边）：{}",
            peak.halo_alpha
        );
    }

    /// ★ 回归护栏（2026-09-20 性能修复）：呼吸必须是**有限次**，播完必须停。
    ///
    /// 这一条直接检查 `status_dot_animation()` 的**返回值**，而不是源码字符串 ——
    /// 「有没有加 `.repeat()`」用字符串断言只在「写出来」时才抓得到，而这里要钉的是
    /// 语义：只有 `oneshot == true`，gpui 才会在 `delta > 1.0` 时置 `done = true`
    /// 并**停止排帧**（`elements/animation.rs:428-470`）。
    ///
    /// 三条一起守：
    ///   * `oneshot`：无限重播 ⇒ `done` 永为假 ⇒ 空闲窗口永久以 `max_fps` 出帧；
    ///   * `!synced`：`synced` 的相位是全局 epoch 对 duration 取模，`delta` 恒 `< 1`
    ///     ⇒ `delta > 1.0` 永不成立 ⇒ **连 oneshot 都结束不了**（变相永动）；
    ///   * `max_fps`：播放期间反正是整窗重绘，必须限频。
    #[test]
    fn status_dot_breathing_is_bounded_and_stops() {
        let anim = super::status_dot_animation();
        assert!(
            anim.oneshot,
            "呼吸动画必须是一次性（oneshot）。`repeat()` / `repeat_synced()` 会把 oneshot \
             置为 false，gpui 的 `done` 就永远为假 ⇒ 空闲窗口以 {}fps 永久重绘整窗。",
            super::TAB_DOT_BREATH_FPS
        );
        assert!(
            !anim.synced,
            "呼吸动画不能用 synced：synced 的相位 = 全局 epoch 对 duration 取模，\
             delta 恒落在 [0,1) ⇒ `delta > 1.0` 永不成立 ⇒ 一次性动画也永远结束不了。"
        );
        assert_eq!(
            anim.duration,
            std::time::Duration::from_millis(super::TAB_DOT_BREATH_TOTAL_MS),
            "总时长必须 = 周期数 × 单周期，否则「播几下」会跟着周期数漂。"
        );
        assert_eq!(
            anim.max_fps,
            Some(super::TAB_DOT_BREATH_FPS),
            "播放期间必须限频（with_max_fps），否则按屏幕刷新率烧整窗重绘。"
        );
        // 两端都必须落在谷值：否则最后一帧会把圆点「卡」在放大态，
        // 而此后不再重绘，那个放大态就是永久的。
        assert!(
            super::breath_easing_bounded(0.0).abs() < 1e-6,
            "起播必须从谷值（常态直径）开始"
        );
        assert!(
            super::breath_easing_bounded(1.0).abs() < 1e-6,
            "结束必须回到谷值（常态直径），否则圆点会永久停在放大态"
        );
    }

    /// 有限次缓动必须**恰好**脉动配置的次数：多了＝还在无限播，少了＝没播完就停。
    ///
    /// 判据取「向上穿过 0.5」的次数 —— 与 `dot_pulse` 的直径涨幅单调对应，
    /// 且一个周期只穿一次，不受采样步长影响。
    #[test]
    fn bounded_breathing_pulses_exactly_the_configured_number_of_times() {
        let expected = super::TAB_DOT_BREATH_CYCLES as usize;
        let steps = 2000usize;
        let mut crossings = 0usize;
        let mut prev = super::breath_easing_bounded(0.0);
        for i in 1..=steps {
            let v = super::breath_easing_bounded(i as f32 / steps as f32);
            assert!(
                (0.0..=1.0).contains(&v),
                "缓动越界：相位 {i}/{steps} 得 {v}"
            );
            if prev < 0.5 && v >= 0.5 {
                crossings += 1;
            }
            prev = v;
        }
        assert_eq!(
            crossings, expected,
            "一次播放必须恰好脉动 {expected} 次（实测 {crossings} 次）—— 次数由 \
             TAB_DOT_BREATH_CYCLES 决定；改它只改「播几下」，因为单次脉动的实际时长 \
             仍 = 总时长 / N = TAB_DOT_BREATH_MS"
        );
    }

    /// 2026-09-17 第四轮反馈「状态灯改为可呼吸式」：实心状态点必须挂呼吸动画。
    ///
    /// 第六轮反馈补的三条：动画闭包必须**同时**产出外扩光环与带柔光的圆点，且圆点
    /// 自己不许设透明度 —— 「看起来是不是变暗淡」在无头测试里量不到，只能钉住这三件事
    /// （前两件是「有光晕」，第三件是「灯芯不淡」）。
    ///
    /// 2026-09 性能修复后新增一条：动画元素**只能**挂在 [`TabStatusDot`] 子视图里。
    /// 若它回到 `MainView::render_tab_bar`，gpui 的动画调度器就会
    /// `cx.notify(window.current_view())` = `notify(MainView)`，整棵窗口树被反复重建
    /// （见 [`TabStatusDot`] 与 `tab_status_dot_animation_is_isolated_to_its_own_view`）。
    ///
    /// ★ 2026-09-20：「同步相位」这条**已作废** —— `repeat_synced()` 的相位是全局
    /// epoch 对 duration 取模，`delta` 恒 `< 1`，一次性动画永远结束不了。
    /// 取代它的负向断言是「生产源码里不许出现任何无限重播」。
    #[test]
    fn filled_status_dot_breathes_with_capped_fps() {
        let code = production_source();
        assert!(
            code.contains("with_animation"),
            "实心状态点必须挂呼吸动画（with_animation），不得退回静态圆点。"
        );
        assert!(
            code.contains("status_dot_animation()"),
            "呼吸动画必须经 `status_dot_animation()` 构造（护栏要能直接检查它的返回值），\
             不要在 render_status_dot 里内联 Animation::new。"
        );
        assert!(
            code.contains("with_max_fps"),
            "呼吸动画必须限频（with_max_fps），否则按帧率全速重绘与 120FPS 目标冲突。"
        );
        // 负向：无限重播会让 `done` 永为假 ⇒ 窗口永久重绘。
        // 注释已被 `prod()` 剥掉，所以文档里提到的 `repeat_synced()` 不会误伤。
        for banned in ["repeat_synced", ".repeat()"] {
            assert!(
                !code.contains(banned),
                "生产源码里出现了 `{banned}`：无限动画的 `done` 永远为假，gpui 会以 \
                 20fps 永久重绘整窗（2026-09-20 实测 gap_avg_us≈100000、UI 线程占 0.73 核）。\
                 呼吸必须是有限次，见 status_dot_animation()。"
            );
        }
        // 未连接的空心点必须保持静态：动画分支只允许出现在实心一侧
        assert!(
            code.contains("if !filled {"),
            "呼吸动画必须只作用于实心点（`!filled` 提前返回静态空心点），未连接点保持静态。"
        );
        assert!(
            code.contains("dot_pulse("),
            "呼吸外观必须来自纯函数 `dot_pulse`（直径/柔光/光环三者同源）。"
        );
        assert!(
            code.contains("BoxShadow::new(") && code.contains(".blur_radius("),
            "圆点必须带柔光（`BoxShadow` + `blur_radius`）—— 用户要的「光晕」这一层。"
        );
        assert!(
            code.contains("tab-status-dot-halo"),
            "必须画出外扩光环（`tab-status-dot-halo`）—— 用户要的「向外扩散的圆环」这一层。"
        );
    }

    /// ★ 回归护栏（2026-09 性能修复）：呼吸动画的**唯一宿主**必须是
    /// [`TabStatusDot`] 这个独立视图。
    ///
    /// gpui 的动画调度器在 `with_max_fps` 分支里执行
    /// `cx.notify(window.current_view())`，而 `current_view()` 取的是
    /// **动画元素所属的视图**。动画元素此前直接建在 `MainView::render_tab_bar` 的
    /// tab 项里 ⇒ `current_view()` = `MainView` ⇒ 只要有一个已连接标签，
    /// **整棵窗口树（含终端、侧栏、快捷命令栏）就永远以 20fps 重建**。
    ///
    /// 本护栏用源码结构把这件事钉死：`render_tab_bar` 里不许出现 `with_animation`，
    /// 且 `MainView` 里不许再直接构造 `Animation::new`。
    #[test]
    fn tab_status_dot_animation_is_isolated_to_its_own_view() {
        let code = production_source();

        // 切出 `render_tab_bar` 的函数体（从签名到下一个顶层 `fn` 之前）
        let start = code
            .find("pub fn render_tab_bar(")
            .expect("render_tab_bar 必须存在");
        let rest = &code[start..];
        let end = rest.find("\n    /// ").unwrap_or(rest.len());
        let body = &rest[..end];

        assert!(
            !body.contains("with_animation") && !body.contains("Animation::new("),
            "呼吸动画不得建在 render_tab_bar（MainView）里：动画调度器会把 \
             `window.current_view()`（= MainView）每 1/max_fps 秒标脏，导致整棵窗口树 \
             永远以 20fps 重建。请把动画留在 TabStatusDot 子视图内。"
        );
        assert!(
            body.contains("entity.into_any_element()") || body.contains("into_any_element()"),
            "render_tab_bar 必须把 TabStatusDot **实体**挂进 tab 项（子视图），\
             而不是内联构造动画元素。"
        );

        // 动画必须落在 TabStatusDot::render 的调用链里
        assert!(
            code.contains("impl Render for TabStatusDot"),
            "TabStatusDot 必须实现 Render（动画宿主）"
        );
        assert!(
            code.contains("render_status_dot(self.filled, self.color)"),
            "TabStatusDot::render 必须委托给 render_status_dot（唯一绘制入口）"
        );
    }

    /// 已关闭标签的圆点实体必须被回收：否则反复关开标签会持续累积
    /// 视图与它们的动画定时器（每个都是一个常驻 20fps 唤醒源）。
    #[test]
    fn closed_tab_dots_are_pruned() {
        let code = production_source();
        assert!(
            code.contains("tab_dots") && code.contains(".retain("),
            "render_tab_bar 必须清理已关闭标签的圆点实体（tab_dots + retain）"
        );
    }

    /// ★ 2026-09-18 反馈「溢出菜单选被隐藏的 tab 无法跳转过去」的根因定位用例：
    /// `ScrollHandle::scroll_to_item` 在 Segmented TabBar 上必须**双向**可达 ——
    /// 实机日志（10:36:58 / 10:44:26）显示向右滚（offset -64 → -210）生效，
    /// 从最右 `scroll_to_item(1)` 回最左时 offset 纹丝不动。
    ///
    /// 探针尽量贴近生产路径：`selected_index`（Segmented 指示器 + 弹簧）、`menu(true)`、
    /// `prefix`、标签页逐个出现（与实机「连开 8 个标签再从菜单选第 1 个」同时序）。
    #[gpui::test]
    fn scroll_to_item_reaches_hidden_tabs_in_both_directions(cx: &mut gpui::TestAppContext) {
        struct Probe {
            handle: gpui::ScrollHandle,
        }

        impl Render for Probe {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                let mut bar = TabBar::new("scroll-probe")
                    .with_variant(TabVariant::Segmented)
                    .with_size(super::TAB_SIZE)
                    .max_width(px(super::TAB_MAX_WIDTH))
                    .prefix(super::tab_strip_prefix())
                    .menu(true)
                    .track_scroll(&self.handle)
                    .selected_index(0);
                for i in 0..10 {
                    bar = bar.child(Tab::new().label(SharedString::from(format!("Terminal {i}"))));
                }
                // 窄视口：400px 装不下 10 个标签，必然横向溢出
                div().w(px(400.)).h(px(48.)).child(bar)
            }
        }

        cx.update(|cx| {
            gpui_component::init(cx);
            crate::theme::apply_theme_mode(cx, rustshell_core::theme::ThemeMode::Dark);
        });
        let handle = gpui::ScrollHandle::new();
        let (_, vcx) = cx.add_window_view({
            let handle = handle.clone();
            move |_, _| Probe { handle }
        });
        vcx.run_until_parked();

        // 前置：内容确实溢出（max_offset > 0），初始停在最左
        assert!(
            f32::from(handle.max_offset().x) > 100.,
            "探针里 10 个标签没有溢出 400px 视口（max_offset.x = {}），测不了隐藏标签",
            f32::from(handle.max_offset().x)
        );
        assert_eq!(f32::from(handle.offset().x), 0.0, "初始应停在最左");

        // ★ 结构护栏：官方 Segmented 指示器占的布局子元素数 + 末尾菜单占位。
        // 打印全量头部边界，用于核对「tab_i 的 child 下标 = i + leading」这一档。
        let fmt = |b: Option<gpui::Bounds<gpui::Pixels>>| match b {
            Some(b) => format!("[{:.0},{:.0}]", f32::from(b.left()), f32::from(b.right())),
            None => "none".to_string(),
        };
        let head: Vec<String> = (0..5)
            .map(|i| format!("b{i}={}", fmt(handle.bounds_for_item(i))))
            .collect();
        eprintln!(
            "probe children={} max={} {}",
            handle.children_count(),
            f32::from(handle.max_offset().x),
            head.join(" ")
        );
        let n = handle.children_count();
        // 10 标签 + 1 菜单占位 = 11 个尾部子元素 ⇒ 指示器占 n - 11 个头部子元素，
        // tab0 的下标 = 指示器子元素数。
        let tab0_ix = n - 11;
        let tab0 = handle.bounds_for_item(tab0_ix).expect("tab0 应有边界");
        let tab9 = handle.bounds_for_item(tab0_ix + 9).expect("tab9 应有边界");
        assert!(
            f32::from(tab9.left()) - f32::from(tab0.left()) > 200.,
            "child {tab0_ix} 与 child {} 必须分属条子两端的标签（tab0={tab0:?} tab9={tab9:?}）\
             —— 指示器占的子元素数变了，`follow(ix, ..)` 的偏移档要同步改。",
            tab0_ix + 9
        );

        // 向右：滚到最后一个标签（实机已验证生效的方向）
        handle.scroll_to_item(tab0_ix + 9);
        vcx.update(|w, cx| {
            _ = w.draw(cx);
        });
        let right = f32::from(handle.offset().x);
        assert!(
            right < -50.,
            "向右 scroll_to_item(11) 后 offset = {right}，没有滚动"
        );

        // 向左：回到第一个标签（实机失效的方向，child 2 = tab0）。
        // 生效时 offset 应明显回到 0 附近。
        handle.scroll_to_item(2);
        vcx.update(|w, cx| {
            _ = w.draw(cx);
        });
        let back = f32::from(handle.offset().x);
        assert!(
            back > right + 50.,
            "向左 scroll_to_item(2) 后 offset = {back}（向右时 {right}）—— \
             左向滚动没有生效，这正是「溢出菜单选隐藏 tab 跳不过去」的缺陷现场"
        );
    }

    /// 圆点自身**不得**被压透明度：会淡的只能是外扩光环。
    /// 取色用 `Hsla::opacity`（`dot_color.opacity(..)`）是允许的 —— 那是给光环描边调
    /// alpha；这里挡的是 `Styled::opacity` 那种「整个元素转半透明」的写法（上一版就是
    /// 用它做「亮—暗」循环，被用户判为「变暗淡」）。判据：圆点那一段构建里不出现
    /// `.opacity(` 且不出现 `opacity(` 作为元素方法调用 —— 用「动画闭包内是否给圆点
    /// 设过 `opacity`」这一条源码断言代替像素测量。
    #[test]
    fn breathing_dot_itself_is_never_faded() {
        let code = production_source();
        // 生产代码里 `opacity(` 的合法用法只有取色：`Hsla` 的 `.opacity(alpha)`。
        // 元素级的 `Styled::opacity` 会被写成链式调用，形如 `.bg(..).opacity(`。
        assert!(
            !code.contains(".opacity(1.") && !code.contains(".opacity(0."),
            "圆点元素又被压透明度了；用户反馈「不要变暗淡」，圆点只改直径与柔光。"
        );
    }

    /// 2026-09-17 第五轮反馈「呼吸灯改为微微放大，不要变暗淡」：放大必须发生在固定
    /// 6px 的槽位里，**不得推动标签布局，也不得只往一侧长**。
    ///
    /// 两条判据：
    /// * **直径真的变大** —— 相位由测试**直接指定**（探针把 0.0 / 1.0 喂进
    ///   [`super::paint_status_dot`]）：谷值必须正好是常态直径、峰值必须正好是
    ///   常态 + 幅度。不指定相位的话，量到的永远是最初一帧 = 谷值 `6px`，
    ///   「变大」这一半就完全量不到了 —— 一个「只会变暗、尺寸恒定」的实现也能让
    ///   本用例假绿。
    ///
    ///   ★ 2026-09-20 为什么改成直接喂相位：呼吸从 `repeat_synced`（相位 = 执行器
    ///   时钟）换成**一次性**动画（相位 = `web_time::Instant` 墙钟）之后，
    ///   `advance_clock` 再也推不动相位。直接喂相位反而更强：不再依赖时钟与定时器
    ///   的落地时序，端点值可以**精确**断言，而不是只能给一个区间。
    /// * **放大是「向四周」而不是「向一侧」** —— 判据取左右（上下）两侧的溢出量之差。
    ///   不居中的实现（宽高动画直接挂在标签的 flex 子项上）会把 1.5px 全长在一侧，
    ///   差值 1.5px；居中的实现两侧各溢出一半，差值只有布局取整带来的 ≤0.5px。
    #[gpui::test]
    fn breathing_dot_grows_centered_in_its_slot(cx: &mut gpui::TestAppContext) {
        let rest_dot = {
            let vcx = bar_probe_at(cx, true, 0.0);
            let slot = vcx
                .debug_bounds("tab-status-dot-slot")
                .expect("状态圆点槽位未渲染");
            let dot = vcx.debug_bounds("tab-status-dot").expect("状态圆点未渲染");
            let rest_w = f32::from(dot.size.width);
            assert!(
                (rest_w - super::TAB_STATUS_DOT).abs() <= 0.5,
                "谷值相位必须正好是常态直径 {}px，实测 {rest_w}px",
                super::TAB_STATUS_DOT
            );
            assert_growth_is_symmetric(dot, slot, "谷值");
            dot.size
        };

        // 峰值相位：直径涨幅最大，越界信号最强。
        let vcx = bar_probe_at(cx, true, 1.0);
        let slot = vcx
            .debug_bounds("tab-status-dot-slot")
            .expect("状态圆点槽位未渲染");
        let grown = vcx.debug_bounds("tab-status-dot").expect("状态圆点未渲染");
        let peak_w = f32::from(grown.size.width);
        assert!(
            (peak_w - super::dot_pulse(1.0).dot).abs() <= 0.5,
            "峰值相位必须正好是常态直径 + 幅度（{}px），实测 {peak_w}px —— \
             呼吸必须由直径驱动（`dot_pulse` 的 `dot`）；只压透明度是量不到尺寸变化的。",
            super::dot_pulse(1.0).dot
        );
        assert!(
            grown.size.width > rest_dot.width && grown.size.height > rest_dot.height,
            "峰值比谷值没有变大（{rest_dot:?} → {:?}）—— 呼吸必须由直径驱动",
            grown.size
        );
        assert_growth_is_symmetric(grown, slot, "峰值");
    }

    /// 圆点的放大必须向四周等量溢出：两侧溢出量之差超过 0.5px 就说明它只往一侧长。
    ///
    /// 容差 0.5px 来自布局引擎对元素原点的取整（实测 0.5 逻辑像素栅格），不是笔误：
    /// 居中的实现两侧溢出之差 ≤0.5px，只往一侧长的实现是「一侧 0、另一侧整个涨幅」
    /// （峰值 1.5px），两者在这条判据上分得很开。
    fn assert_growth_is_symmetric(dot: Bounds<Pixels>, slot: Bounds<Pixels>, phase: &str) {
        let sides = [
            ("x", slot.left() - dot.left(), dot.right() - slot.right()),
            ("y", slot.top() - dot.top(), dot.bottom() - slot.bottom()),
        ];
        for (axis, before, after) in sides {
            let (before, after) = (f32::from(before), f32::from(after));
            assert!(
                before >= -0.5 && after >= -0.5,
                "{phase}：圆点在 {axis} 轴上比槽位还小（前 {before:.2}px / 后 {after:.2}px）—— \
                 呼吸只许放大，不许缩小。"
            );
            assert!(
                (before - after).abs() <= 0.5,
                "{phase}：圆点在 {axis} 轴上只往一侧放大（前 {before:.2}px / 后 {after:.2}px）\
                 —— 放大必须向四周溢出，否则标签标题会跟着抖。"
            );
        }
    }

    /// 2026-09-17 第六轮反馈「光晕 + 缩放 + 透明度 + 向外扩散的圆环」：三层必须**真的
    /// 画出来**。
    ///
    /// 只断言源码里有 `BoxShadow` / `tab-status-dot-halo` 拦不住「有代码没画面」——
    /// 终端滚动条那一轮就吃过这个亏（源码齐全、右缘什么都没有）。这里量渲染出的 quad：
    /// * **圆点**：半相位下边长 = `dot_pulse(0.5).dot`，实质不透明度 **= 1**（用户要求
    ///   「不要变暗淡」，圆点自身一帧都不许半透明）；
    /// * **外扩光环**：同一处另有一圈「只有描边、没有底色」的 quad，边长 =
    ///   `dot_pulse(0.5).halo`（比圆点大），且完整包住圆点 —— 这一圈才是会淡的那个。
    ///
    /// 相位由测试**直接指定**（探针喂 0.5 = 半相位）：谷值时光环恰好被圆点盖住；
    /// 而**峰值相位根本量不到光环** —— `halo_alpha = TAB_DOT_HALO_ALPHA × (1 − p)`
    /// 在 `p = 1` 时正好是 0（「扩到最大时淡尽」正是设计意图），gpui 的
    /// `is_border_visible()`（`style.rs`）会因此**整个跳过**那一圈边框的绘制。
    /// 老版本这条用例靠 `advance_clock` 推相位，实际落在 `p ≈ 0.99`（测试调度器的
    /// 时钟抖动），是擦着边过的；改成直接喂相位后必须显式取半相位。
    ///
    /// ★ 2026-09-20 起不再用 `advance_clock` 推相位：呼吸改成一次性动画，相位取自
    /// `web_time::Instant`（真实墙钟），推虚拟时钟没有用。直接喂相位才是确定性的做法。
    #[gpui::test]
    fn pulse_paints_an_opaque_dot_inside_a_wider_halo_ring(cx: &mut gpui::TestAppContext) {
        let vcx = bar_probe_at(cx, true, 0.5);

        let half = super::dot_pulse(0.5);
        let scale = vcx.update(|window, _| window.scale_factor());
        let dot_side = half.dot * scale;
        let halo_side = half.halo * scale;
        // 只收「小方块」：圆点与它的光环是这一片里唯一两种这么小的 quad。
        let small: Vec<Quad> = vcx.update(|window, _| {
            window
                .painted_quads()
                .into_iter()
                .filter(|q| {
                    let (w, h) = (q.bounds.size.width.as_f32(), q.bounds.size.height.as_f32());
                    w <= halo_side + 2. && h <= halo_side + 2.
                })
                .collect()
        });

        let dot = small
            .iter()
            .find(|q| {
                let w = q.bounds.size.width.as_f32();
                (w - dot_side).abs() <= 1. && q.background.as_solid().is_some_and(|c| c.a >= 0.99)
            })
            .unwrap_or_else(|| {
                panic!(
                    "半相位下找不到「边长 ≈ {dot_side:.1}px（{:.1} 逻辑像素）且不透明」的圆点 \
                     quad；实际小 quad 有 {} 个。",
                    half.dot,
                    small.len()
                )
            });
        let halo = small
            .iter()
            .find(|q| {
                let w = q.bounds.size.width.as_f32();
                let border = q.border_widths.left.as_f32();
                (w - halo_side).abs() <= 1.
                    && border > 0.
                    && q.background.as_solid().is_none_or(|c| c.a <= 0.01)
            })
            .unwrap_or_else(|| {
                panic!(
                    "半相位下找不到「边长 ≈ {halo_side:.1}px、只有描边」的外扩光环 quad；\
                     实际小 quad 有 {} 个。",
                    small.len()
                )
            });

        // 光环必须比圆点大，且把圆点整个包在里面（「向外扩散」而不是叠在圆点上）
        assert!(
            halo.bounds.size.width > dot.bounds.size.width,
            "光环没有比圆点大：光环 {:?} / 圆点 {:?}",
            halo.bounds.size,
            dot.bounds.size
        );
        assert!(
            halo.bounds.origin.x.as_f32() <= dot.bounds.origin.x.as_f32()
                && halo.bounds.origin.y.as_f32() <= dot.bounds.origin.y.as_f32()
                && halo.bounds.origin.x.as_f32() + halo.bounds.size.width.as_f32()
                    >= dot.bounds.origin.x.as_f32() + dot.bounds.size.width.as_f32()
                && halo.bounds.origin.y.as_f32() + halo.bounds.size.height.as_f32()
                    >= dot.bounds.origin.y.as_f32() + dot.bounds.size.height.as_f32(),
            "光环没有包住圆点：光环 {:?} / 圆点 {:?}",
            halo.bounds,
            dot.bounds
        );
    }

    /// 2026-09-17 运行反馈「tab 过多会被隐藏掉，无法移动选择到里面」。
    ///
    /// 官方 `tabs-inner` 只吃横向 delta（`overflow_x_scroll + lock_scroll_axis`），
    /// 普通滚轮推不动它，所以标签一多就只能靠溢出菜单 —— 而菜单是**半窗高上限**
    /// 的滚动列表（`PopupMenu` 默认 `min(半窗, 450px)`），一样要用户自己发现能滚。
    /// 这里把滚轮接上，并把手感写成可单测的纯函数：方向、端点夹取、无位移不重绘。
    #[test]
    fn wheel_scrolls_the_tab_strip_with_clamping() {
        use gpui::px;
        // 内容比视口宽 300px ⇒ max_offset.x = +300（正值幅度），可滚区间 [-300, 0]
        let max_x = px(300.);
        // 滚轮向下（delta < 0）看到更靠后的标签：偏移继续变小
        assert_eq!(
            super::wheel_scroll_target(px(0.), px(-50.), max_x),
            Some(px(-50.)),
            "滚轮向下必须能滚到后面的标签"
        );
        assert_eq!(
            super::wheel_scroll_target(px(-200.), px(-50.), max_x),
            Some(px(-250.))
        );
        // 滚轮向上回到更靠前的标签
        assert_eq!(
            super::wheel_scroll_target(px(-250.), px(50.), max_x),
            Some(px(-200.))
        );
        // 两端夹取：到最右不再变小、到最左不再变大，且都算「无位移」
        assert_eq!(
            super::wheel_scroll_target(px(-280.), px(-50.), max_x),
            Some(px(-300.)),
            "越过右端必须夹到 -max_offset"
        );
        assert_eq!(super::wheel_scroll_target(px(-300.), px(-50.), max_x), None);
        assert_eq!(super::wheel_scroll_target(px(0.), px(50.), max_x), None);
        // 无位移（含横向滚轮：delta.y = 0）不产生重绘
        assert_eq!(super::wheel_scroll_target(px(-100.), px(0.), max_x), None);
        // 内容没超出视口（max_offset 为 0）时任何滚轮都不动
        assert_eq!(super::wheel_scroll_target(px(0.), px(-50.), px(0.)), None);
    }

    /// 结构侧：标签轨道必须接上滚轮 → 横向滚动，且 `min_w_0` 不能丢。
    ///
    /// `min_w_0` 决定「轨道是否真的溢出滚动」：丢了它，标签会把外壳顶宽，
    /// 右侧开关被挤出窗口，滚轮也就无从谈起。
    #[test]
    fn tab_strip_wires_the_wheel_to_horizontal_scroll() {
        let code = production_source();
        assert!(
            code.contains("on_scroll_wheel"),
            "标签轨道必须接上滚轮监听（`on_scroll_wheel`）—— 否则纵向滚轮推不动它，\
             被挤出视口的标签在鼠标下没有入口。"
        );
        assert!(
            code.contains("wheel_scroll_target"),
            "滚轮位移必须走 `wheel_scroll_target`（方向与夹取有单测）。"
        );
        assert!(
            code.contains(".flex_1()") && code.contains(".min_w_0()"),
            "标签轨道必须仍是 `flex_1 + min_w_0`：否则标签把外壳顶宽、右侧开关被挤出窗口。"
        );
        assert!(
            code.contains("scroll_to_item") || code.contains("follow("),
            "激活标签变化时必须仍请求把它滚进可见区（`TabScroll::follow`）。"
        );
    }

    /// 回归（2026-09-18 运行反馈「按住标签栏空白处拖动 = 类似滑动轴移动」）：
    /// 平移手感写成可单测的纯函数 —— 方向（抓住纸拖）、端点夹取、无位移不重绘。
    #[test]
    fn bar_pan_follows_the_hand_and_clamps_to_the_strip() {
        // 内容比视口宽 300px ⇒ max_offset.x = +300（正值幅度），可滚区间 [-300, 0]
        let max = 300.;
        // 指针右移（dx > 0）露出左边的内容：偏移向 0 走
        assert_eq!(
            super::pan_offset(-200., 50., max),
            -150.,
            "向右拖必须把手底下的标签一起向右带（偏移向 0 走）"
        );
        // 指针左移（dx < 0）露出右边的内容：偏移继续变小
        assert_eq!(super::pan_offset(-100., -50., max), -150.);
        // 两端夹取
        assert_eq!(
            super::pan_offset(-280., -50., max),
            -300.,
            "越过右端必须夹到 -max_offset，不能拖出内容外"
        );
        assert_eq!(
            super::pan_offset(-20., 50., max),
            0.,
            "越过左端必须夹到 0，不能拖出内容外"
        );
        // 内容没溢出（max_offset = 0）：任何位移都不动，也就不会产生重绘
        assert_eq!(super::pan_offset(0., -50., 0.), 0.);
        // 句柄还没量出可滚范围时（异常值）：夹到 0，别把内容推出去
        assert_eq!(super::pan_offset(-10., -5., -5.), 0.);
    }

    /// 回归（2026-09-18 运行反馈「按住标签拖动，和其他 tab 切换位置」）：
    /// 「指针压住第几个标签」的判定写成可单测的纯函数。
    ///
    /// 三个标签，屏上区间分别是 `[0,100] [100,200] [200,300]`。
    #[test]
    fn dragged_tab_lands_on_the_slot_under_the_pointer() {
        let slots = [(0., 100.), (100., 200.), (200., 300.)];
        // 区间内：各归各的
        assert_eq!(super::nearest_slot(&slots, 50.), Some(0));
        assert_eq!(super::nearest_slot(&slots, 150.), Some(1));
        assert_eq!(super::nearest_slot(&slots, 250.), Some(2));
        // 边界（半开半闭看具体实现，这里只要落在这两个之一）
        assert_eq!(super::nearest_slot(&slots, 100.), Some(0));
        assert_eq!(super::nearest_slot(&slots, 200.), Some(1));
        // 落在空隙里：取最近的那一格（标签之间有 `gap_1`，指针必然会在空隙上停过）
        assert_eq!(
            super::nearest_slot(&[(0., 100.), (110., 210.)], 104.),
            Some(0)
        );
        assert_eq!(
            super::nearest_slot(&[(0., 100.), (110., 210.)], 106.),
            Some(1)
        );
        // 拖出整条栏：夹到首 / 末，于是「拖到最左边」能一路换到 0 号位
        assert_eq!(super::nearest_slot(&slots, -500.), Some(0));
        assert_eq!(super::nearest_slot(&slots, 500.), Some(2));
        // 边界还没就绪（首帧未布局）：不动
        assert_eq!(super::nearest_slot(&[], 50.), None);
    }

    /// 结构侧：标签栏必须同时接上两种拖拽，且换位走的是「会话 id 反查下标」这条路。
    ///
    /// 只做源码级断言：`on_drag` 的建立要位移超过 2px、`on_drag_move` 是 window 级
    /// 监听，无头环境里没有真实的指针轨迹，硬测只会得到假绿（纯逻辑部分另有单测：
    /// [`bar_pan_follows_the_hand_and_clamps_to_the_strip`] 与
    /// [`dragged_tab_lands_on_the_slot_under_the_pointer`]）。
    #[test]
    fn tab_bar_wires_tab_reorder_and_strip_pan() {
        let code = production_source();
        // 1. 标签自己起拖（换位）：载荷必须是 `TabDrag`
        assert!(
            code.contains("TabDrag { sid:"),
            "标签必须挂 `on_drag(TabDrag …)`，否则按住标签拖不动、无法与邻居换位。"
        );
        // 2. 标签栏空白处起拖（平移）：载荷必须是 `BarPan`
        assert!(
            code.contains("on_drag(BarPan"),
            "标签栏必须挂 `on_drag(BarPan …)`，否则按住空白处拖不动整条标签栏。"
        );
        // 3. 两个 `on_drag_move` 的类型必须分别对上费
        for needle in ["DragMoveEvent<TabDrag>", "DragMoveEvent<BarPan>"] {
            assert!(
                code.contains(needle),
                "缺少 `on_drag_move::<{needle}>`：拖拽过程中收不到指针位置。"
            );
        }
        // 4. 换位必须用会话 id 反查（下标在拖拽过程中一帧一变，存下来就会錯位）
        assert!(
            code.contains("fn reorder_tab_dragged_to"),
            "换位必须收口在 `MainView::reorder_tab_dragged_to`（幂等：id 反查下标）。"
        );
        // 5. 平移基准点必须跨帧存活（逐次位移累加），不能只存在闭包里
        assert!(
            code.contains("tab_pan_x"),
            "平移基准指针位置必须存在 `MainView::tab_pan_x`：逐帧重建的闭包传不过去。"
        );
        // 6. 用户开始自己平移时不得再被「自动滚到激活标签」拽回去
        assert!(
            code.contains("user_took_control"),
            "拖拽平移时必须放弃待滚动目标与偏移补写，否则会跟用户的手抢位置。"
        );
    }

    /// 的白框距**灰色轨道左缘**只有官方自带的 4px，看着像贴在轨道边上。
    ///
    /// 官方没给「轨道内边距」的入口，应用侧用 `TabBar::prefix`（轨道内、标签条之前的
    /// 占位）补了 `TAB_STRIP_LEFT_INSET`。这条用例量真实布局：第一个标签的左缘 =
    /// 轨道自身内边距（[`super::TRACK_BUILTIN_PADDING_X`]）+ 应用补的那一档。
    ///
    /// 为什么把框架那个 4px 也写进断言：它是**成对契约** —— 官方换档位/换常量时，这里
    /// 会报红并告诉后来人「4px 变了，补给标签条的内缩要重算」，而不是悄悄退回贴边。
    #[gpui::test]
    fn tab_strip_starts_clear_of_the_track_edge(cx: &mut gpui::TestAppContext) {
        // 生产代码那一侧必须挂的是**同一份**占位：只量探针会漏掉「生产忘了加」这一形态
        // （探针自带占位、量出来照样是 12px，屏幕上却还是 4px）。
        let code = production_source();
        assert!(
            code.contains(".prefix(tab_strip_prefix())"),
            "生产代码的 `TabBar` 没挂标签条左内缩占位（`.prefix(tab_strip_prefix())`）—— \
             第一个标签会贴回轨道左缘。"
        );

        let vcx = bar_probe(cx, true);
        let tab = vcx.debug_bounds("probe-tab").expect("标签未渲染");
        // 探针里 TabBar 就是窗口根的第一个元素：窗口 x=0 即轨道左缘。
        let expected = px(super::TRACK_BUILTIN_PADDING_X + super::TAB_STRIP_LEFT_INSET);
        assert_eq!(
            tab.origin.x,
            expected,
            "第一个标签离轨道左缘 {}px，应为 {}px（框架 {} + 应用补 {}）—— \
             标签条又贴回轨道边上了。",
            f32::from(tab.origin.x),
            f32::from(expected),
            super::TRACK_BUILTIN_PADDING_X,
            super::TAB_STRIP_LEFT_INSET
        );
    }

    fn ctx(
        local: bool,
        has_profile: bool,
        connected: bool,
        connecting: bool,
        total: usize,
    ) -> TabMenuCtx {
        TabMenuCtx {
            id: "s1".into(),
            title: "t".into(),
            local,
            has_profile,
            connected,
            connecting,
            total,
            // 分屏开关只影响「取消分屏」菜单项的可用性，与本组判据无关
            in_split: false,
        }
    }

    /// 本地 Shell 已连接：可复制、可断开；无 profile 到 不可连接
    #[test]
    fn local_connected_tab() {
        assert_eq!(
            tab_menu_flags(&ctx(true, false, true, false, 2)),
            TabMenuFlags {
                duplicate: true,
                connect: false,
                disconnect: true,
                close_others: true,
            }
        );
    }

    /// SSH 已成功连接：三项管理动作全部可用
    #[test]
    fn ssh_connected_tab() {
        assert_eq!(
            tab_menu_flags(&ctx(false, true, true, false, 1)),
            TabMenuFlags {
                duplicate: true,
                connect: false, // 已连接，无需再连
                disconnect: true,
                close_others: false, // 只有一个标签
            }
        );
    }

    /// SSH 连接中（占位 Tab）：连接项必须置灰，避免重复发起
    #[test]
    fn ssh_connecting_tab_disables_connect() {
        let f = tab_menu_flags(&ctx(false, true, false, true, 3));
        assert!(f.duplicate, "有 profile 即可复制");
        assert!(!f.connect, "连接中不得再点「连接」");
        assert!(!f.disconnect, "尚未建立会话，无可断开");
        assert!(f.close_others);
    }

    /// SSH 已断开但保留 Tab（「断开」后的状态）：连接项可用，用于原地重连
    #[test]
    fn ssh_disconnected_tab_can_reconnect() {
        let f = tab_menu_flags(&ctx(false, true, false, false, 1));
        assert!(f.duplicate);
        assert!(f.connect, "断开态必须能重新连接");
        assert!(!f.disconnect);
        assert!(!f.close_others);
    }

    /// 未保存为连接配置的临时 SSH 会话：复制与连接都不可用
    #[test]
    fn adhoc_ssh_tab_is_not_duplicable() {
        let f = tab_menu_flags(&ctx(false, false, true, false, 2));
        assert!(!f.duplicate, "没有 profile 无法复制");
        assert!(!f.connect, "没有 profile 无法重连");
        assert!(f.disconnect);
        assert!(f.close_others);
    }

    /// 本地 Shell 停在「已断开」（会话恢复时启动失败的那一格）：连接项必须可用，
    /// 否则占位卡上那句「右键标签可重新连接」就是假提示。
    #[test]
    fn local_disconnected_tab_can_restart() {
        let f = tab_menu_flags(&ctx(true, false, false, false, 1));
        assert!(f.connect, "本地 Shell 断开的标签要能重开");
        assert!(!f.disconnect, "没有终端，无可断开");
        // 正在启动的那一格（恢复排队中）不该重复发起
        assert!(
            !tab_menu_flags(&ctx(true, false, false, true, 1)).connect,
            "启动中不得再点「连接」"
        );
    }
}
