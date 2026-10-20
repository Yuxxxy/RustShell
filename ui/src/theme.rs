//! UI 主题装配：将 core 层 Token写入 gpui-component`Theme`全局。

use gpui::{px, rgb, App};
use gpui_component::theme::{
    Theme, ThemeMode as ComponentThemeMode, ThemeTokens as ComponentThemeTokens,
};
use rustshell_core::theme::{ThemeMode, ThemeTokens};

pub fn to_hsla(hex: u32) -> gpui::Hsla {
    gpui::Hsla::from(rgb(hex))
}

/// 当前生效的 Token 全局（apply_theme 时写入，业务组件经`current(cx)`消费）
pub struct CurrentTheme(pub ThemeTokens, pub bool /* dark */);
impl gpui::Global for CurrentTheme {}

/// 读取当前 Token（组件内统一入口，替代硬编码 ThemeTokens::dark()）
pub fn current(cx: &App) -> ThemeTokens {
    cx.try_global::<CurrentTheme>()
        .map(|g| g.0)
        .unwrap_or_else(ThemeTokens::dark)
}

/// 当前是否为深色主题（：遮罩等「派生色」按明暗取值）
pub fn is_dark(cx: &App) -> bool {
    cx.try_global::<CurrentTheme>().map(|g| g.1).unwrap_or(true)
}

/// 面板底色：`alpha >= 1` 时就是主题原色，小于 1 时叠加透明度让窗口背景图透出来。
/// 不做无意义合成：未启用背景图时调用方传 1.0，取色结果与改造前逐位一致。
pub fn surface(alpha: f32, hex_color: u32) -> gpui::Hsla {
    let c = to_hsla(hex_color);
    if alpha >= 1.0 {
        c
    } else {
        c.opacity(alpha.clamp(0.0, 1.0))
    }
}

/// 浮层遮罩 scrim：随主题派生，Dark 35% 纯黑 / Light 20% 纯黑。
pub fn scrim(cx: &App) -> gpui::Hsla {
    gpui::black().opacity(if is_dark(cx) { 0.35 } else { 0.20 })
}

/// 按 Token 应用主题（theme_mode 由 Settings 驱动，可即时切换）
pub fn apply_theme_mode(cx: &mut App, mode: ThemeMode) {
    let t = ThemeTokens::resolved(mode);
    let dark = t.bg_base == ThemeTokens::dark().bg_base;

    cx.set_global(CurrentTheme(t, dark));

    // 第 1 步：先经官方`Theme::change`切到目标模式。
    // 它会按 Light/Dark 预设重建 colors + tokens，并把 Base 层主题
    // （滚动条/分隔条）与文本默认色写回全局。
    // 运行反馈修复：此前只赋值`theme.mode`字段，`theme.tokens.*`
    // 始终停留在启动时`Theme::init`写入的 Light 预设。官方 TabBar 取
    //`tokens.tab_bar`(#F5F5F5)、StatusBar 取`tokens.status_bar`(#F8F8F8)，
    // 于是在深色主题下渲染出整条浅色底，即反馈中的两处「颜色不对」。
    Theme::change(
        if dark {
            ComponentThemeMode::Dark
        } else {
            ComponentThemeMode::Light
        },
        None,
        cx,
    );

    // 第 2 步：用本应用 Token 覆盖语义色
    let theme = Theme::global_mut(cx);
    let colors = &mut theme.colors;
    colors.background = to_hsla(t.bg_base);
    colors.foreground = to_hsla(t.fg_primary);
    colors.border = to_hsla(t.border_subtle);
    colors.accent = to_hsla(t.accent);
    // N5：accent/danger 底色上的前景色统一走 token（不再硬编码 0x0D1117 / white()）
    colors.accent_foreground = to_hsla(t.on_accent);
    colors.muted = to_hsla(t.bg_elevated);
    colors.muted_foreground = to_hsla(t.fg_muted);
    colors.secondary = to_hsla(t.bg_elevated);
    colors.secondary_foreground = to_hsla(t.fg_primary);
    // 窗口控制按钮 hover/active 背景：gpui-component TitleBar 用 secondary_hover，
    // 未配置时是默认浅色，深色主题下会出现白色块（bug 反馈 2026-09-08）。
    //
    // 2026-09-18 运行反馈「软件右上角缩小和放大在浅色模式上鼠标放上去看不出来效果」：
    // 浅色主题里 `bg_overlay == bg_base == #FFFFFF`，而标题栏底色取的正是 `bg_base`
    // （见 `MainView::render` 里 TitleBar 的 `.bg(self.panel_bg(t.bg_base))`）⇒
    // 白底上盖白高亮，鼠标移上去逐像素不变。深色下 `bg_overlay(#1C2128)` 与
    // `bg_base(#0D1117)` 差一档灰阶，所以只有浅色露馅。
    //
    // 浅色改用「主文字色的低透明度」当 hover 面：它按定义与任何底色可区分，
    // 不引入新的硬编码色值，且 Dark 分支保持原值——深色主题的观感逐像素不变。
    colors.secondary_hover = if dark {
        to_hsla(t.bg_overlay)
    } else {
        to_hsla(t.fg_primary).opacity(0.09)
    };
    colors.secondary_active = if dark {
        to_hsla(t.bg_elevated)
    } else {
        to_hsla(t.fg_primary).opacity(0.16)
    };
    colors.title_bar = to_hsla(t.bg_overlay);
    colors.title_bar_border = to_hsla(t.border_subtle);
    colors.tab_bar = to_hsla(t.bg_base);
    // Segmented 轨道底色：官方选中胶囊画的是 `tokens.background`（= bg_base），
    // 深色主题里 bg_elevated(#161B22) 与 bg_base(#0D1117) 只差一档灰阶，胶囊几乎隐身，
    // 即 2026-09-17 反馈「黑色主题下分不清当前 tab」。轨道改用更亮的 bg_overlay(#1C2128)
    // 拉开对比；Light 下 bg_overlay == bg_base == #FFFFFF，会把轨道整个抹掉，故维持 bg_elevated。
    colors.tab_bar_segmented = to_hsla(if dark { t.bg_overlay } else { t.bg_elevated });
    // 官方 Tab variant 配色依赖这三个令牌；此前缺失导致选中态取默认浅色值（白底白字）
    colors.tab_foreground = to_hsla(t.fg_secondary);
    // 选中标签底色必须取「比 Tab 栏底色更抬升」的那一档。
    // 2026-09-17 运行反馈「白色背景下看不出当前终端属于哪个标签」根因就在这里：
    // Light 主题里 `bg_overlay == bg_base == #FFFFFF`，而 Tab 栏底色也是 `bg_base`，
    // 于是选中态与未选中态**逐像素同色**。`bg_elevated`（Light #F6F8FA / Dark #161B22）
    // 才是两种主题下都与栏底色可区分的那一档。
    colors.tab_active = to_hsla(t.bg_elevated);
    colors.tab_active_foreground = to_hsla(t.fg_primary);
    colors.sidebar = to_hsla(t.bg_elevated);
    colors.sidebar_border = to_hsla(t.border_subtle);
    colors.sidebar_foreground = to_hsla(t.fg_primary);
    colors.sidebar_accent = to_hsla(t.bg_overlay);
    colors.sidebar_accent_foreground = to_hsla(t.fg_primary);
    colors.popover = to_hsla(t.bg_overlay);
    colors.popover_foreground = to_hsla(t.fg_primary);
    // N22（2026-09-10 反馈「新建连接浅色主题看不到输入框的线」）：
    // 官方`Input::render`的边框取色是`cx.theme().input`
    // （gpui-component/src/input/input.rs：`.when(self.bordered, |this|
    // this.border_1().border_color(cx.theme().input))`），
    // 而`input`在主题里的语义是 「Border color for inputs」（theme_color.rs 注释），
    // 不是输入框底色。此前这里错赋成`bg_base`，于是：
    // - Light：边框 = 白底 到 输入框「没有线」；Checkbox 未选中态边框同样消失；
    // - Dark ：边框 = 主背景色 到 输入框与卡片背景糊成一片。
    // 输入框底色另有来源（`Theme::input_background()`：Light=background、
    // Dark=input 混 30% 透明），与本字段无关，故改回「边框语义」即可两全。
    colors.input = to_hsla(t.border_subtle);
    colors.primary = to_hsla(t.accent);
    // N5：accent/danger 底色上的前景统一走 token（Light 下 accent 转深蓝需白字）
    colors.primary_foreground = to_hsla(t.on_accent);
    colors.ring = to_hsla(t.accent);
    colors.link = to_hsla(t.accent);
    colors.selection = to_hsla(t.accent).opacity(0.35);
    colors.danger = to_hsla(t.danger);
    colors.danger_foreground = to_hsla(t.on_danger);
    colors.success = to_hsla(t.success);
    colors.warning = to_hsla(t.warning);
    // 官方`Alert`的 Info 变体读 `theme().info`。此前这一项从未赋值，
    // 于是「信息级」提示会渲染成组件库预设的蓝色，与本应用主题脱钩。
    // 本应用没有独立的 info token，语义上「信息 / 激活 / 链接」同为 accent。
    colors.info = to_hsla(t.accent);
    colors.info_foreground = to_hsla(t.on_accent);
    // 状态栏：官方`StatusBar`取`tokens.status_bar`，此前完全未赋值
    colors.status_bar = to_hsla(t.bg_elevated);
    colors.status_bar_border = to_hsla(t.border_subtle);
    // 列表行悬停 / 选中：**共用同一份淡底色**。
    //
    // 2026-09-18 用户要求「连接选中效果和连接 hover 效果改为一致」：此前 hover 取
    // `bg_overlay`，浅色主题下它是纯白块（`bg_overlay == bg_base == #FFFFFF`），
    // 与选中的 accent 淡底是两套配色 —— 同一行「划过」与「选中」看着像两个控件。
    // 现在 hover 与选中同底（accent 18%），选中再多那圈 accent 外线
    // （`list_active_border`，由官方 `ListItem` 的 `active_highlight` 分支绘制，
    // 圆角取行自身的 `corner_radii`）作为「哪个是选中」的标记。
    //
    // 口径只有这一处：`tokens.list_hover` / `tokens.list_active` 在第 3 步由
    // `colors` 重建，二者不会再各自漂移。
    let row_tint = to_hsla(t.accent).opacity(0.18);
    colors.list_hover = row_tint;
    colors.list_active = row_tint;
    colors.list_active_border = to_hsla(t.accent);

    // primary / danger 变体按钮取色收口（2026-09-10 反馈：保存并连接按钮与主题不一致）。
    //`Button::primary()`读的是`tokens.button_primary`来自`colors.button_primary`
    // （不是`colors.primary`）。组件库 Dark 预设里 button_primary 是「#FAFAFA 白底深字」，
    // 只覆盖 colors.primary 会漏掉它，于是深色主题下渲染出一枚刺眼的白按钮。
    // 这里把 button_* 系列一并对齐 accent/danger 语义，全局所有 primary 按钮随之统一。
    let accent = to_hsla(t.accent);
    let on_accent = to_hsla(t.on_accent);
    // 交互态：直接调亮度（不引入组件库 Colorize trait，保持此处只依赖 gpui）
    // Dark 亮色按钮 到 更亮；Light 深色按钮 到 更暗。
    let variant = |c: gpui::Hsla| -> (gpui::Hsla, gpui::Hsla) {
        let shift = |dl: f32| gpui::Hsla {
            l: (c.l + dl).clamp(0.0, 1.0),
            ..c
        };
        if dark {
            (shift(0.06), shift(0.12))
        } else {
            (shift(-0.05), shift(-0.10))
        }
    };
    let (acc_hover, acc_active) = variant(accent);
    let (dan_hover, dan_active) = variant(to_hsla(t.danger));
    colors.button_primary = accent;
    colors.button_primary_hover = acc_hover;
    colors.button_primary_active = acc_active;
    colors.button_primary_foreground = on_accent;
    colors.button_danger = to_hsla(t.danger);
    colors.button_danger_hover = dan_hover;
    colors.button_danger_active = dan_active;
    colors.button_danger_foreground = to_hsla(t.on_danger);

    // 第 3 步（关键收口）：`tokens`与`colors`同源派生，
    //`ThemeColor::apply_config`内部即`tokens.<field> = colors.<field>`。
    // 覆盖 colors 之后必须重建 tokens，否则官方组件读到的仍是预设残留值。
    let fitted = theme.colors;
    theme.tokens = ComponentThemeTokens::from(&fitted);

    // 全局字号收一档（2026-09-18 用户要求「除终端外，全局较大字体减小一号，
    // 比如 tab 栏标题、AI 输入栏、连接分组名称和连接名称；标题行不用动」）。
    //
    // `theme.font_size` 是**唯一的全局字号杠杆**：`Root::render` 每帧做
    // `window.set_rem_size(cx.theme().font_size)`（gpui-component/src/root.rs），
    // 而组件库里的 `text_sm()` / `text_xs()` / `text_base()` 全是 `rems(..)`
    // （gpui-pre `styled.rs`：`text_sm` = `rems(0.875)`）⇒ 16 到 15 让
    // tab 标题 14 到 13.1、AI 输入框 16 到 15、按钮/菜单/对话框文字同比例收一档。
    //
    // 本文件与业务代码里**显式写死的 px 字号**不受影响（终端字号更是只认
    // `settings.terminal_font_size`），所以终端与手写的小号标签保持原样；
    // 窗口标题行按用户要求用显式 px 钉住不动（见 `MainView` 里 `TitleBar` 那处）。
    theme.font_size = px(15.);

    // 用户要求：**全局去掉输入框聚焦后的外环阴影**。
    // 官方`Input::render`聚焦时会调`ThemeStyled::focus_ring_style(window, cx)`，
    // 而该函数第一件事就是看这个开关（`gpui-component/src/styled.rs`）：
    // if !cx.theme().focus_ring { return self.border_color(cx.theme().ring); }
    // 置`false`后聚焦只剩「描边换成 ring 色」，不再向外画一圈 ring。
    // 一处开关即覆盖全部输入框（命令编辑器、新增连接、设置面板、终端搜索），
    // 无需逐个改调用点；顺带解决「外环被裁剪容器切掉」的问题。
    theme.focus_ring = false;
}

/// 滚动条滑块三态取色（唯一入口；终端窗格与 Tab 栏共用）。
///
/// 为什么必须自己定色：官方滑块读的是预设里的 `tokens.scrollbar_thumb`，浅色预设下
/// 铺在终端白底上只有约 `(234,234,234)` —— **滑块一直在画，只是看不见**。这正是
/// 2026-09-17「终端和 tab 栏滑动轴还是没有」两次反馈背后的一半原因（另一半是
/// 句柄把 `offset` 的符号写反，滑块被推出轨道之外，见 `terminal_scrollbar`）。
/// 常态用 `fg_muted` 的 60% 保证在明暗两种底色上都清晰可辨，悬停加深。
///
/// 第三态（按下）**与悬停同色**：2026-09-17 第五轮运行反馈「点按的时候取消变色」。
/// 上一版按下会把滑块染成 accent，于是「按住拖动」的整段时间里滚动条忽然变蓝，用户
/// 读到的是「我点到别的东西了」。按下的反馈由滑块自身变宽承担（官方 active 档
/// 6px → 8px，见 `gpui_base::scrollbar`），不靠换颜色 —— 官方主题投影同样是
/// 「active 取 hover 色」（`gpui_component::theme` 的 `thumb_active`），这里只是跟回去。
pub fn scrollbar_thumb(cx: &App) -> (gpui::Hsla, gpui::Hsla, gpui::Hsla) {
    let base = to_hsla(current(cx).fg_muted);
    let hover = base.opacity(0.85);
    (base.opacity(0.60), hover, hover)
}

/// Token 快捷取色（业务组件内使用，避免直接写十六进制）
pub trait TokenColor {
    fn token(&self) -> ThemeTokens;
    fn accent(&self) -> gpui::Hsla {
        to_hsla(self.token().accent)
    }
    fn success(&self) -> gpui::Hsla {
        to_hsla(self.token().success)
    }
    fn danger(&self) -> gpui::Hsla {
        to_hsla(self.token().danger)
    }
    fn warning(&self) -> gpui::Hsla {
        to_hsla(self.token().warning)
    }
    fn purple(&self) -> gpui::Hsla {
        to_hsla(self.token().purple)
    }
    fn fg_muted(&self) -> gpui::Hsla {
        to_hsla(self.token().fg_muted)
    }
    fn bg_elevated(&self) -> gpui::Hsla {
        to_hsla(self.token().bg_elevated)
    }
    fn bg_overlay(&self) -> gpui::Hsla {
        to_hsla(self.token().bg_overlay)
    }
    fn border_subtle(&self) -> gpui::Hsla {
        to_hsla(self.token().border_subtle)
    }
    /// accent 底色上的前景（on-accent 语义，随主题变化）
    fn on_accent(&self) -> gpui::Hsla {
        to_hsla(self.token().on_accent)
    }
    /// danger 底色上的前景
    fn on_danger(&self) -> gpui::Hsla {
        to_hsla(self.token().on_danger)
    }
}

impl TokenColor for gpui::App {
    fn token(&self) -> ThemeTokens {
        current(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::Hsla;

    /// 回归：深色主题下`Theme.tokens.*`必须与`Theme.colors.*`同源，不得残留`Theme::init`启动时写入的 Light 预设。
    #[gpui::test]
    fn dark_theme_does_not_leak_light_preset_tokens(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            apply_theme_mode(cx, ThemeMode::Dark);

            let theme = Theme::global(cx);
            let d = ThemeTokens::dark();

            assert_eq!(
                Hsla::from(theme.tokens.tab_bar),
                to_hsla(d.bg_base),
                "tokens.tab_bar 残留 Light 预设 → Tab 栏整条浅色底"
            );
            assert_eq!(
                Hsla::from(theme.tokens.status_bar),
                to_hsla(d.bg_elevated),
                "tokens.status_bar 残留 Light 预设 → 状态栏整条浅色底"
            );
            assert_eq!(
                Hsla::from(theme.tokens.background),
                to_hsla(d.bg_base),
                "tokens.background 残留 Light 预设"
            );
            // colors 与 tokens 同源：两侧必须一致
            assert_eq!(theme.colors.tab_bar, Hsla::from(theme.tokens.tab_bar));
            assert_eq!(theme.colors.status_bar, Hsla::from(theme.tokens.status_bar));
            // mode 与 Base 层（滚动条/分隔条）同步为深色
            assert!(theme.mode.is_dark());
        });
    }

    /// 回归（2026-09-17）：选中标签的底色必须与 Tab 栏底色**可区分**。
    ///
    /// 现场缺陷：Light 主题里 `bg_overlay`、`bg_base` 都是 `#FFFFFF`，而 Tab 栏底色也取
    /// `bg_base` 到 选中态与未选中态同色，「看不出当前终端属于哪个标签」。
    /// 这里守住的就是这条不变量，两套主题都要成立。
    #[gpui::test]
    fn active_tab_is_distinguishable_from_the_tab_bar(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            for (mode, token) in [
                (ThemeMode::Dark, ThemeTokens::dark()),
                (ThemeMode::Light, ThemeTokens::light()),
            ] {
                apply_theme_mode(cx, mode);
                let theme = Theme::global(cx);
                assert_eq!(
                    theme.colors.tab_bar,
                    to_hsla(token.bg_base),
                    "Tab 栏底色必须仍是页面底色"
                );
                assert_ne!(
                    theme.colors.tab_active, theme.colors.tab_bar,
                    "{mode:?} 主题下选中标签底色与 Tab 栏底色相同 → 分不出当前标签"
                );
                // 选中态同时必须有独立的文字色（白底白字是同一个缺陷的另一面）
                assert_ne!(theme.colors.tab_active_foreground, theme.colors.tab_bar);
            }
        });
    }

    /// 回归（2026-09-17 反馈「黑色主题下分不清当前 tab」）：Segmented 轨道与官方
    /// 选中胶囊（画的是 `tokens.background`）在两种主题下都必须可区分 ——
    /// 轨道与胶囊同色 = 整条栏看不出谁被选中。
    #[gpui::test]
    fn segmented_track_differs_from_the_indicator_pill(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            for (mode, _token) in [
                (ThemeMode::Dark, ThemeTokens::dark()),
                (ThemeMode::Light, ThemeTokens::light()),
            ] {
                apply_theme_mode(cx, mode);
                let theme = Theme::global(cx);
                assert_ne!(
                    Hsla::from(theme.tokens.tab_bar_segmented),
                    Hsla::from(theme.tokens.background),
                    "{mode:?} 主题下 Segmented 轨道与选中胶囊同色 → 分不出当前标签"
                );
            }
        });
    }

    /// Light 模式同样不得残留深色，且 accent/danger 前景走 on-accent token
    #[gpui::test]
    fn light_theme_uses_light_tokens_and_on_accent_foreground(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            apply_theme_mode(cx, ThemeMode::Light);

            let theme = Theme::global(cx);
            let l = ThemeTokens::light();

            assert_eq!(Hsla::from(theme.tokens.tab_bar), to_hsla(l.bg_base));
            assert_eq!(theme.colors.background, to_hsla(l.bg_base));
            assert_eq!(theme.colors.primary_foreground, to_hsla(l.on_accent));
            assert_eq!(theme.colors.danger_foreground, to_hsla(l.on_danger));
            assert!(!Theme::global(cx).mode.is_dark());
        });
    }

    /// 回归（2026-09-16）：官方`Alert`的四个变体分别读
    /// `theme().info / success / warning / danger`，本应用必须把这四个语义色
    /// 全部写进`Theme.colors` 到 漏一个，对应级别的提示就会渲染成组件库预设色
    /// （Dark 预设的 info 是另一个蓝，与本应用 accent 不一致）。
    ///
    /// 四个级别 到 变体的映射在 `crate::notice::NoticeKind`，本测试守住的是
    /// 它下游的**取色链路**：级别对了但颜色没接上，提示依然是错的。
    #[gpui::test]
    fn alert_variant_colors_follow_app_tokens(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            for (mode, token) in [
                (ThemeMode::Dark, ThemeTokens::dark()),
                (ThemeMode::Light, ThemeTokens::light()),
            ] {
                apply_theme_mode(cx, mode);
                let theme = Theme::global(cx);
                assert_eq!(
                    theme.colors.info,
                    to_hsla(token.accent),
                    "Alert::info 未跟随 accent 到 信息级提示会写成组件库预设色"
                );
                assert_eq!(theme.colors.success, to_hsla(token.success));
                assert_eq!(theme.colors.warning, to_hsla(token.warning));
                assert_eq!(theme.colors.danger, to_hsla(token.danger));
            }
        });
    }

    /// 回归（2026-09-17 第五轮反馈「点按的时候取消变色」）：滑块三态里「按下」必须
    /// 与「悬停」同色。
    ///
    /// 上一版按下取 accent：按住滑块拖动的整段时间里滚动条都变蓝，而拖动本来就伴随
    /// 滑块变宽（官方 active 档 8px），两重反馈叠在一起反而像切了控件。这里守住的是
    /// 「按下不换色」这条取色口径 —— 它唯一实现在 [`scrollbar_thumb`]，改错一处
    /// 终端滚动条与 Tab 栏两条都会跟着变。
    #[gpui::test]
    fn scrollbar_thumb_press_keeps_the_hover_color(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            for mode in [ThemeMode::Dark, ThemeMode::Light] {
                apply_theme_mode(cx, mode);
                let (rest, hover, pressed) = scrollbar_thumb(cx);
                assert_eq!(
                    pressed, hover,
                    "{mode:?} 主题下按住滑块的取色与悬停不同 → 点按滑动轴会变色"
                );
                assert_ne!(
                    rest, hover,
                    "{mode:?} 主题下悬停与常态同色 → 鼠标移到滑动轴上没有任何反馈"
                );
                assert!(
                    rest.a >= 0.4,
                    "{mode:?} 主题下常态滑块太淡（α={:.2}）→ 等于「有滚动条但看不见」",
                    rest.a
                );
            }
        });
    }

    /// 回归：`colors.input`是输入框边框语义，必须与背景可区分。
    #[gpui::test]
    fn input_token_is_a_visible_border_not_the_background(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            for (mode, token) in [
                (ThemeMode::Dark, ThemeTokens::dark()),
                (ThemeMode::Light, ThemeTokens::light()),
            ] {
                apply_theme_mode(cx, mode);
                let theme = Theme::global(cx);
                assert_eq!(
                    theme.colors.input,
                    to_hsla(token.border_subtle),
                    "colors.input 必须是边框色（Input/Checkbox 直接读它）"
                );
                assert_ne!(
                    theme.colors.input, theme.colors.background,
                    "输入框边框不得等于背景色 → 否则输入框「没有线」"
                );
                // 与 tokens 同源收口
                assert_eq!(Hsla::from(theme.tokens.input), theme.colors.input);
            }
        });
    }

    /// 回归：`Button::primary()`读`tokens.button_primary`，必须跟随本应用 accent，不得残留组件库预设的「#FAFAFA 白底深字」（2026-09-10 反馈：保存并连接按钮白底）。
    #[gpui::test]
    fn primary_button_token_follows_accent(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);

            for (mode, token) in [
                (ThemeMode::Dark, ThemeTokens::dark()),
                (ThemeMode::Light, ThemeTokens::light()),
            ] {
                apply_theme_mode(cx, mode);
                let theme = Theme::global(cx);
                assert_eq!(
                    Hsla::from(theme.tokens.button_primary),
                    to_hsla(token.accent),
                    "button_primary 未跟随 accent → primary 按钮会渲染成预设白底"
                );
                assert_eq!(
                    theme.colors.button_primary_foreground,
                    to_hsla(token.on_accent),
                    "button_primary_foreground 未走 on-accent 语义"
                );
                // hover/active 必须与常态不同（否则交互无反馈）
                assert_ne!(
                    Hsla::from(theme.tokens.button_primary_hover),
                    to_hsla(token.accent)
                );
                assert_ne!(
                    Hsla::from(theme.tokens.button_primary_active),
                    to_hsla(token.accent)
                );
            }
        });
    }

    /// 回归（2026-09-18 用户要求「终端默认字体大小为 12 号；除终端外全局较大字体
    /// 减小一号，比如 tab 栏标题、AI 输入栏、连接分组名称和连接名称；标题行不用动」）：
    /// **全局字号只有一个杠杆**（`theme.font_size`，官方 `Root` 每帧拿它做
    /// `window.set_rem_size`），这里钉住「比组件库默认的 16 小一号」。
    ///
    /// 组件库那侧的字号全是 rem（`text_sm()` = `rems(0.875)`），所以这一处一改，
    /// tab 标题、AI 输入框、按钮、菜单、对话框正文**同时**收一档；
    /// 而本仓手写的 px 字号与终端字号（`settings.terminal_font_size`）不受影响。
    #[gpui::test]
    fn global_font_scale_is_one_step_smaller(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            for mode in [ThemeMode::Dark, ThemeMode::Light] {
                apply_theme_mode(cx, mode);
                let theme = Theme::global(cx);
                assert_eq!(
                    theme.font_size,
                    px(15.),
                    "{mode:?} 主题下全局字号基准应为 15（组件库默认 16，收一档）"
                );
                // 终端字号**不受**它影响：终端只认 `settings.terminal_font_size`
                // （见 `TerminalPaneConfig::font_size`）。
                assert!(
                    theme.mono_font_size != theme.font_size,
                    "等宽字号与全局字号是两条独立的线，不得被一起改动"
                );
            }
        });
    }

    /// 回归（2026-09-18 用户要求「连接选中效果和连接 hover 效果改为一致」）：
    /// 列表行的 hover 与选中必须**同一份淡底色**，选中再叠那圈 accent 外线。
    ///
    /// 官方 `ListItem` 的两条渲染分支：hover 取 `tokens.list_hover` 当底色；
    /// 选中取 `list_active` 当底色、`list_active_border` 当外线
    /// （`list.active_highlight` 为真时）。三条都是本应用的主题字段，
    /// 所以「一致」只能在这一处收口 —— 断言「悬停底 == 选中底」正是它的定义。
    #[gpui::test]
    fn list_hover_matches_selection_tint(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            for (mode, token) in [
                (ThemeMode::Dark, ThemeTokens::dark()),
                (ThemeMode::Light, ThemeTokens::light()),
            ] {
                apply_theme_mode(cx, mode);
                let theme = Theme::global(cx);
                assert_eq!(
                    theme.colors.list_hover, theme.colors.list_active,
                    "{mode:?} 主题下 hover 底色与选中底色不一致（用户要求两者统一）"
                );
                assert_eq!(
                    theme.colors.list_active_border,
                    to_hsla(token.accent),
                    "{mode:?} 主题下选中外线必须取 accent（它是「哪个被选中」的唯一标记）"
                );
                // 底色必须是**半透明**的 accent：不透明的 `bg_overlay` 正是旧口径
                // （浅色主题下纯白块），断言它会挡住「又退回两套配色」的改动。
                assert!(
                    theme.colors.list_hover.a < 1.0,
                    "{mode:?} 主题下 hover 底色是不透明色 —— 旧口径（bg_overlay）回来了"
                );
            }
        });
    }

    /// 回归（2026-09-18 用户要求「标题行不用动」）：标题行的品牌名与右上角三枚
    /// 控制图标都按**继承**字号绘制（`Icon::render` 取 `window.text_style().font_size`），
    /// 全局字号收一档后必须在 `TitleBar` 这一层用**显式 px** 钉回原值，
    /// 否则整条标题行会跟着一起变小 —— 这正是用户点名不要动的那一处。
    ///
    /// 取值 15px 是**当时的定案**（同批次把标题栏高度也收了一档，见
    /// `main_view.rs` 的 `TITLE_BAR_HEIGHT` 与那一段注释）。这条用例钉的是
    /// 「这一层必须有显式字号」以及「它就钉在这个值上」—— 后来若真要调这个数，
    /// 必须连同本条一起改，从而在 diff 里留下痕迹。
    #[test]
    fn title_bar_text_scale_is_pinned() {
        let code = crate::test_support::prod(include_str!("main_view.rs"));
        let bar = code
            .find("gpui_component::TitleBar::new()")
            .expect("找不到标题栏（结构锚点）");
        let brand = code
            .find(r#".child("RustShell")"#)
            .expect("找不到标题栏品牌名（结构锚点）");
        assert!(
            code[bar..brand].contains(".text_size(px(15.))"),
            "标题栏构造链上没有显式字号：全局字号一改，品牌名与控制图标都会跟着动"
        );
    }

    /// 回归（2026-09-18 运行反馈「软件右上角缩小和放大在浅色模式上鼠标放上去看不
    /// 出来效果」）：窗口控制按钮（最小化 / 最大化 / 关闭）的 hover 底色必须与
    /// **标题栏底色**可区分。
    ///
    /// 取色链路：`gpui-component/src/title_bar.rs` 的 `ControlIcon::hover_bg()` 读
    /// `theme.secondary_hover`；而本应用的标题栏底色是 `bg_base`
    /// （`MainView::render` 里 `TitleBar` 的 `.bg(self.panel_bg(t.bg_base))`）。
    /// 浅色主题里 `bg_overlay == bg_base == #FFFFFF`，于是旧实现是「白底白高亮」，
    /// 深色主题恰好差一档灰阶才没暴露 —— 这正是「只有浅色模式看不出来」的原因。
    ///
    /// 为什么断言「不等于 bg_base」而不是钉死具体色值：色值可以随主题调整，
    /// 要守住的是「悬停看得出变化」这条不变量，两种主题都得成立。
    #[gpui::test]
    fn window_control_hover_is_visible_on_the_title_bar(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            for (mode, token) in [
                (ThemeMode::Dark, ThemeTokens::dark()),
                (ThemeMode::Light, ThemeTokens::light()),
            ] {
                apply_theme_mode(cx, mode);
                let theme = Theme::global(cx);
                assert_ne!(
                    theme.colors.secondary_hover,
                    to_hsla(token.bg_base),
                    "{mode:?} 主题下窗口控制按钮 hover 底色 == 标题栏底色 → \
                     鼠标放上去看不出来效果"
                );
                assert_ne!(
                    theme.colors.secondary_active, theme.colors.secondary_hover,
                    "{mode:?} 主题下按下与悬停同色 → 缺少按下反馈"
                );
                // 与 tokens 同源收口：官方 ControlIcon 读的是 tokens.secondary_hover
                assert_eq!(
                    Hsla::from(theme.tokens.secondary_hover),
                    theme.colors.secondary_hover
                );
            }
        });
    }
}
