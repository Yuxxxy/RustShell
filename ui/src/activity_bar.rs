//! 图标竖栏 ActivityBar（48px 通底）：**上**：五个面板图标；**下**：语言 / 主题 /
//! 设置三枚（2026-09-18 起下沉到栏底，两者之间靠留白分界，不再画横线）。

use gpui::{
    div, prelude::FluentBuilder as _, px, Context, InteractiveElement as _, IntoElement,
    ParentElement as _, SharedString, Styled as _,
};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem},
    IconName, Sizable as _, Size,
};

use super::main_view::{MainView, PanelKind};

/// 竖栏**图标按钮**的尺寸档（2026-09-18 用户要求「最左侧图标改大一号」）。
///
/// 官方 `Button` 的图标字号只有一个旋钮 —— **按钮自己的 size**：
/// `button.rs` 里 `icon_size` 直接取自 `self.size`，再 `.with_size(icon_size)`
/// 覆盖我们传给 `ButtonIcon` 的字号（所以给图标单独写字号是无效的）。
/// 而 `Size` 到图标的映射是 XSmall 12px / Small 14px / Medium 16px / Large 24px，
/// 故 Medium 正好是 Small 的「大一号」（按钮盒 24 到 32px，栏宽 48px 容得下）。
const RAIL_ICON_SIZE: Size = Size::Medium;

/// 竖栏宽度（px）；与 `render` 里 `.w(px(RAIL_W))` 同源。
///
/// 2026-09-21 用户要求「宽度减小五分之一」：原 48px × 4/5 = **38.4px**。
/// 图标按钮盒是 32px（见 [`RAIL_ICON_SIZE`]），两侧各余 3.2px —— 栏宽还能再收，
/// 但按钮盒不能再小（上一轮用户刚要求「图标改大一号」）。指示条位置由
/// [`rail_bar_left`] 按栏宽推导，跟着一起变，不用手改。
const RAIL_W: f32 = 38.4;
/// 选中指示条：左缘距**栏左缘**的位置、条宽、上下内缩（px）。
const RAIL_BAR_X: f32 = 2.;
const RAIL_BAR_W: f32 = 2.;
const RAIL_BAR_INSET: f32 = 8.;

pub fn panel_icon(p: PanelKind) -> IconName {
    match p {
        PanelKind::Dashboard => IconName::LayoutDashboard,
        PanelKind::Connections => IconName::Globe,
        PanelKind::Terminals => IconName::SquareTerminal,
        PanelKind::Ai => IconName::Bot,
        // `Calendar`（不是 `CalendarClock`）：`shell/assets/icons/` 只随包了前者，
        // 写一个没有对应 svg 的名字**不会报错**，只在界面上留一片空白
        // （见 `ui/src/test_support.rs::every_icon_the_ui_asks_for_is_shipped`）。
        PanelKind::Tasks => IconName::Calendar,
    }
}

pub fn panel_hotkey(p: PanelKind) -> usize {
    match p {
        PanelKind::Dashboard => 1,
        PanelKind::Connections => 2,
        PanelKind::Terminals => 3,
        PanelKind::Ai => 4,
        PanelKind::Tasks => 5,
    }
}

pub fn render(m: &mut MainView, cx: &mut Context<MainView>) -> impl IntoElement {
    use rustshell_core::i18n::{self, Lang};
    use rustshell_core::theme::ThemeMode;
    let tk = crate::theme::current(cx);
    let active = m.panel_ui.panel;

    // 主题图标随模式变化
    let (theme_icon, theme_label) = match m.settings.theme_mode {
        ThemeMode::Dark => (IconName::Moon, i18n::t("深色主题")),
        ThemeMode::Light => (IconName::Sun, i18n::t("浅色主题")),
    };
    let theme_tooltip = if i18n::lang() == Lang::Zh {
        rustshell_core::i18n::ts(
            format!("主题：{theme_label}"),
            format!("Theme: {theme_label}"),
        )
    } else {
        format!("Theme: {theme_label}")
    };
    // 语言切换按钮文案（M4 国际化：中/英）；已按语言分支，无需再走 t()
    let (lang_label, lang_tooltip) = if i18n::lang() == Lang::Zh {
        ("中", "语言：中文")
    } else {
        ("EN", "Language: English")
    };

    gpui_component::v_flex()
        .id("activity-rail")
        // 供测试量「栏到底多宽」用（release 下 no-op）。宽度是布局算出来的，
        // 源码扫描只能证明 `.w(px(RAIL_W))` 还在，证明不了它真变成 38.4px。
        .debug_selector(|| "activity-rail".to_string())
        .w(px(RAIL_W))
        .h_full()
        .flex_shrink_0()
        .bg(crate::theme::surface(1.0, tk.bg_elevated))
        // 右侧那条 1px 分割线随圆角卡片一并撤下：2026-09-18 起侧栏是**独立卡片**，
        // 与竖栏之间隔着 `CARD_GAP` 的留白，分界由留白自己承担；再画一条线，
        // 它会孤零零地立在空隙中间（离卡片还有 8px），比不画更像渲染缺陷。
        .items_center()
        // 2026-09-21 用户要求「图标栏和上下文区的空白区域紧凑一点」：
        // 上下内边距 8 → 6、图标间距 4 → 2。栏本身同时收窄（见 `RAIL_W`），
        // 三处一起收才是「紧凑」而不是「只窄了但空着」。
        .py(px(6.))
        .gap(px(2.))
        // 图标次序 = `PanelKind::ALL` 的**固定**次序。
        //
        // 2026-09-21 先上过一版「按最近访问倒序」（点哪块面板它跳到第一格），
        // 同日用户要求「这个排序回退」：图标会跳格，用户再也记不住第几格是哪块面板。
        // 回退后不论最近打开过什么，位置一律不动 —— 「最近用过」这个信息走
        // 命令面板与各自的列表，不要动这排图标的位置。
        .children(PanelKind::ALL.map(|p| rail_item(m, p, p == active, cx)))
        // 弹簧：把下面三枚（语言 / 主题 / 设置）压到栏底（2026-09-18 运行反馈
        // 「中英、浅色模式切换和设置按钮都下沉到底部」）。此前它们是紧跟在五个
        // 面板图标后面的普通流式子节点，视觉上属「上半截」，与「底部工具区」
        // 的心智不符：面板图标选的是**内容**，这三枚改的是**应用**。
        //
        // 用 `flex_1` 占位而不是 `.justify_between()`：栏里还有六个面板图标，
        // 分组语义只在「面板 | 应用」这一处，加一个撑高元素比重新分组更少改动、
        // 也不影响上方 `children(..)` 的映射写法。
        .child(div().flex_1())
        // 分隔线撤下（用户「移除——」）：这条 28px 的横线原本是「面板图标区」与
        // 「底部三枚」之间的分界，但按钮下沉到栏底后，中间隔着一整片留白，
        // 分界由留白自己承担，再画一条线纯属多一笔。
        .child(
            // 语言切换（M4 国际化）：单击 中/EN 互换，持久化到 settings.json
            //
            // 这一枚**保持 `small`**：它是竖栏里唯一**带文字**的按钮，而官方按钮的
            // 文字档是 `Size::Small 到 text_sm、其余到 text_base`（sizing.rs
            // `button_text_size`）—— 跟着图标一起升到 Medium 会把标签文字推到 15px，
            // 与上一轮「全局较大字体收一档」相抵。
            Button::new("rail-lang")
                .ghost()
                .small()
                .label(lang_label)
                .tooltip(lang_tooltip)
                .on_click(cx.listener(|m, _e, _w, cx| {
                    use rustshell_core::i18n::{set_lang, Lang};
                    let next = if rustshell_core::i18n::lang() == Lang::Zh {
                        Lang::En
                    } else {
                        Lang::Zh
                    };
                    set_lang(next);
                    m.settings.language = if next == Lang::En { "en" } else { "zh" }.to_string();
                    let path = rustshell_platform::paths::settings_path();
                    let _ = m.settings.save(&path);
                    log::info!("语言切换为 {}", m.settings.language);
                    cx.notify();
                })),
        )
        .child(
            // 主题按钮：单击在 Dark / Light 间切换；右键菜单两模式单选
            div()
                .id("rail-theme-wrap")
                .context_menu({
                    let cur = m.settings.theme_mode;
                    move |menu: PopupMenu, _w, _cx| {
                        menu.item(
                            PopupMenuItem::new(rustshell_core::i18n::t("深色"))
                                .icon(IconName::Moon)
                                .checked(cur == rustshell_core::theme::ThemeMode::Dark)
                                .on_click(|_, _w, cx| {
                                    super::main_view::ui_call_set_theme(
                                        rustshell_core::theme::ThemeMode::Dark,
                                        cx,
                                    )
                                }),
                        )
                        .item(
                            PopupMenuItem::new(rustshell_core::i18n::t("浅色"))
                                .icon(IconName::Sun)
                                .checked(cur == rustshell_core::theme::ThemeMode::Light)
                                .on_click(|_, _w, cx| {
                                    super::main_view::ui_call_set_theme(
                                        rustshell_core::theme::ThemeMode::Light,
                                        cx,
                                    )
                                }),
                        )
                    }
                })
                .child(
                    Button::new("rail-theme")
                        .ghost()
                        .with_size(RAIL_ICON_SIZE)
                        .icon(theme_icon)
                        .tooltip(theme_tooltip)
                        .on_click(cx.listener(|m, _e, _w, cx| m.toggle_theme(cx))),
                ),
        )
        .child(
            // 设置按钮：单击开设置面板；右键菜单
            div()
                .id("rail-settings-wrap")
                .context_menu(|menu: PopupMenu, _w, _cx| {
                    menu.item(
                        PopupMenuItem::new(rustshell_core::i18n::t("打开设置")).on_click(
                            |_, w, cx| {
                                super::main_view::ui_call_open_settings(w, cx);
                            },
                        ),
                    )
                    .item(
                        PopupMenuItem::new(rustshell_core::i18n::t("编辑配置文件")).on_click(
                            |_, _w, cx| {
                                super::main_view::ui_call_open_config(cx);
                            },
                        ),
                    )
                })
                .child(
                    Button::new("rail-settings")
                        .ghost()
                        .with_size(RAIL_ICON_SIZE)
                        .icon(IconName::Settings)
                        .tooltip(rustshell_core::i18n::t("设置"))
                        .on_click(cx.listener(|m, _e, w, cx| m.open_settings(w, cx))),
                ),
        )
}

fn rail_item(
    // 会话数角标撤下之后，这里不再读主视图数据；参数保留是为了不给每个调用点
    // 改签名（也留个位置：将来若要按会话状态改图标，落点就在这）。
    _m: &mut MainView,
    panel: PanelKind,
    is_active: bool,
    cx: &mut Context<MainView>,
) -> gpui::AnyElement {
    let t = crate::theme::current(cx);
    // N8：统一走主题工具函数（原就地 hex 闭包）
    use crate::theme::to_hsla as hex;
    let id: SharedString = format!("rail-{:?}-{}", panel, panel_hotkey(panel)).into();
    // 2026-10-21 用户要求「最左栏终端图标不显示数字」：原先终端那格右上角挂着一枚
    // 会话数（`tabs.len()`，最多显示 99）。撤掉之后图标只看形状与选中指示条 ——
    // 标签栏本来就写着有哪几个会话，这里那枚小字既挤在 38px 宽的栏里，
    // 又要在每次开关标签时重排。

    let btn = Button::new(id)
        .ghost()
        .with_size(RAIL_ICON_SIZE)
        .icon(panel_icon(panel))
        // 2026-09-18 运行反馈「最左侧栏移除 hover 显示的快捷键信息」：tooltip 只留
        // 面板名。快捷键提示原本拼的是「概览 (Ctrl+Shift+1)」，悬停即弹出，
        // 对已记住键位的用户是干扰，且面板名本身才是这枚图标要回答的问题。
        // 键位入口仍在设置-快捷键页与命令面板里，没有丢失。
        .tooltip(SharedString::from(panel.label().to_string()))
        .on_click(cx.listener(move |m, _e, _w, cx| {
            if m.panel_ui.panel == panel && !m.panel_ui.sidebar_collapsed {
                m.panel_ui.sidebar_collapsed = true; // 再点当前面板 = 隐藏
            } else {
                m.panel_ui.panel = panel;
                m.panel_ui.sidebar_collapsed = false;
                if panel == PanelKind::Ai {
                    // 2026-09-18 运行反馈「点击 AI 助手，聊天界面显示在终端右侧」：
                    // 左侧栏那一列现在放的是**聊天记录**，对话正文在终端右侧。
                    // 只切侧栏而不展开右侧，用户看到的就是「一列列表 + 一面空白」。
                    m.ai_open_chat(cx);
                }
                if panel == PanelKind::Tasks {
                    // 起调度器 + 回填运行记录：用户打开任务面板就是要它按计划跑
                    m.tasks_open(cx);
                }
            }
            cx.notify();
        }));

    div()
        .relative()
        // 每枚图标各自一个**稳定** selector（`rail-connections` 等）：测试靠它量出
        // 「谁排在第一格」。面板一多，用序号定位（`rail-item-0`）就会随排序漂移，
        // 量到的永远是"第一格"而不是"连接那枚"。
        //
        // selector 取 `PanelKind::key()` 而不是显示名：后者走 i18n，
        // 英文环境下会变成 `rail-Connections`，测试就找不到它了。
        .debug_selector(move || format!("rail-{}", panel.key()))
        .child(
            div()
                .absolute()
                // 指示条按**栏左缘**定位（推导见 `rail_bar_left`）：写成固定的
                // `-10px` 会在图标档变宽后（按钮盒 24 到 32px）被推出栏外。
                .left(px(rail_bar_left()))
                .top(px(RAIL_BAR_INSET))
                .bottom(px(RAIL_BAR_INSET))
                .w(px(RAIL_BAR_W))
                .rounded_full()
                .when(is_active, |d| d.bg(hex(t.accent))),
        )
        .child(btn)
        .into_any_element()
}

/// 图标按钮盒边长（px）：与 [`RAIL_ICON_SIZE`] 同源。
///
/// 官方 `button.rs` 图标按钮的盒子：XSmall 到 `size_5`(20)、Small 到 `size_6`(24)、
/// Medium / Large 到 `size_8`(32)、自定义 px 就是该值本身。
fn rail_box() -> f32 {
    match RAIL_ICON_SIZE {
        Size::XSmall => 20.,
        Size::Small => 24.,
        Size::Large | Size::Medium => 32.,
        Size::Size(v) => v.into(),
    }
}

/// 选中指示条相对**按钮盒**左缘的偏移（px，负值 = 向左）。
///
/// 按钮盒在 48px 宽的栏里居中，所以「贴栏左缘 `RAIL_BAR_X`」= 盒左缘再往左
/// `(RAIL_W - 盒宽)/2 - RAIL_BAR_X`。用推导而不是写死 `-10`：图标档一改
/// （见 `RAIL_ICON_SIZE`），写死的偏移会把整条指示条推到栏外（窗口左边缘之外），
/// 表现就是「当前面板没有标记」。
fn rail_bar_left() -> f32 {
    RAIL_BAR_X - (RAIL_W - rail_box()) / 2.
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::prod;

    /// 回归（2026-09-21 用户要求）：
    /// 1. 「最左侧图标栏宽度减小五分之一」—— 48 × 4/5 = 38.4px；
    /// 2. 「这个排序回退」—— 图标次序必须**固定**为 `PanelKind::ALL`，不许再按
    ///    最近访问重排（点了哪块面板它跳到第一格，用户记不住位置）。
    ///
    /// 反向断言的适用边界：`!contains(..)` 单看容易误伤，此处安全 —— `prod()`
    /// 已剥掉注释，且「遍历固定表渲染图标」在本文件只有 `render` 一处。
    #[test]
    fn rail_is_one_fifth_narrower_and_keeps_a_fixed_order() {
        let code = prod(include_str!("activity_bar.rs"));

        assert!(
            (RAIL_W - 48. * 4. / 5.).abs() < 0.01,
            "竖栏宽度必须是原 48px 的 4/5（= 38.4px），实际 {RAIL_W}"
        );
        assert!(
            code.contains("PanelKind::ALL.map("),
            "图标栏必须直接遍历固定表 `PanelKind::ALL` 渲染 —— 次序固定是用户明确要求的口径"
        );
        assert!(
            !code.contains("ordered_panels("),
            "图标栏又接回了 `ordered_panels`（按最近访问重排）：用户已要求回退，\
             图标位置不许再随访问历史变化。"
        );
    }

    /// 2026-10-21 用户要求「最左栏终端图标不显示数字」：终端那格不再挂会话数角标。
    ///
    /// 反向断言在这里是安全的：`prod()` 已剥注释，且「往图标上挂数字」在本文件
    /// 只有 `rail_item` 一处（`count.min(99)` 是那枚角标的独有写法）。
    #[test]
    fn the_rail_does_not_pin_a_tab_count_badge() {
        let code = prod(include_str!("activity_bar.rs"));
        assert!(
            !code.contains("count.min(99)"),
            "终端图标右上角又出现会话数角标了 —— 用户要求不显示数字。"
        );
        assert!(
            !code.contains(".children(badge)"),
            "`rail_item` 又把 badge 挂回图标盒上了。"
        );
    }

    /// 回归（2026-09-18 运行反馈「左侧栏下面的中英、浅色模式切换和设置按钮都
    /// 下沉到底部，移除——」）：
    ///
    /// 1. 三枚**应用级**按钮（语言 / 主题 / 设置）必须是栏内的最后三个子节点，
    ///    且它们之前有一个撑高元素（`flex_1`）—— 只看源码顺序就能判出「在栏底」，
    ///    少了弹簧它们会退回到紧跟五个面板图标的「上半截」。
    /// 2. 那条 28px 的分隔线（`border_b_1`）不得再出现在本文件：按钮已在栏底，
    ///    中间是一整片留白，分界不再需要画线。
    ///
    /// 为什么不写「顺序 = 语言/主题/设置」就够：顺序断言单独成立时，
    /// 三枚仍可能在栏顶 —— 本用例真正的断言是「弹簧在它们之前」。
    #[test]
    fn rail_pins_app_buttons_to_the_bottom_and_drops_the_divider() {
        let code = prod(include_str!("activity_bar.rs"));

        let spring = code
            .find("div().flex_1()")
            .expect("栏里必须有撑高元素（`div().flex_1()`）把三枚应用按钮压到底部");
        let lang = code.find("\"rail-lang\"").expect("找不到语言按钮");
        let theme = code.find("\"rail-theme\"").expect("找不到主题按钮");
        let settings = code.find("\"rail-settings\"").expect("找不到设置按钮");
        assert!(
            spring < lang && lang < theme && theme < settings,
            "顺序必须是「弹簧 → 语言 → 主题 → 设置」：弹簧在语言按钮之后，\
             三枚就还挂在栏上半截（实际 spring={spring}, lang={lang}, theme={theme}, settings={settings}）"
        );

        assert!(
            !code.contains("border_b_1"),
            "栏里又出现了分隔线（`border_b_1`）—— 按钮下沉后分界由留白承担，\
             用户明确要求移除那条横线。"
        );
    }

    /// 回归（2026-09-18 用户要求「最左侧图标改大一号」）：
    /// 竖栏的**图标按钮**统一升到 [`RAIL_ICON_SIZE`]（Medium = 图标 16px、按钮盒 32px；
    /// 此前 Small = 14px / 24px），而**带文字**的语言按钮必须留在 `small`。
    ///
    /// 为什么语言按钮是例外：官方按钮的文字档只认按钮尺寸
    /// （`sizing.rs::button_text_size`：Small 到 `text_sm`，其余到 `text_base`），
    /// 跟着升档会把它的「中 / EN」推到 15px，把上一轮「全局较大字体收一档」抵消掉。
    #[test]
    fn rail_icon_buttons_are_one_size_bigger() {
        let code = prod(include_str!("activity_bar.rs"));

        // 三处图标按钮：`rail_item`（5 个面板图标共用它）+ 主题 + 设置
        assert_eq!(
            code.matches(".with_size(RAIL_ICON_SIZE)").count(),
            3,
            "竖栏的图标按钮都要走 `RAIL_ICON_SIZE`（面板 5 枚共用 `rail_item`，\
             外加主题与设置各一枚）"
        );
        assert!(
            code.contains("const RAIL_ICON_SIZE: Size = Size::Medium;"),
            "图标档必须集中在 `RAIL_ICON_SIZE` 一处：官方按钮的图标字号只跟按钮尺寸走"
        );

        // 带文字的语言按钮保持 small（升档会连带放大它的文字）
        let lang = code.find("\"rail-lang\"").expect("找不到语言按钮");
        let seg = &code[lang..(lang + 600).min(code.len())];
        assert!(
            seg.contains(".small()"),
            "语言按钮必须保持 `small`：升档会把它的标签文字推到 text_base（15px）"
        );
        assert!(
            !seg.contains("with_size(RAIL_ICON_SIZE)"),
            "语言按钮是带文字的按钮，不该跟着图标一起升档"
        );
    }

    /// 选中指示条必须按**栏左缘**定位，且在任何图标档下都落在栏内。
    ///
    /// 这是「图标大一号」的连带风险：指示条是相对**按钮盒**绝对定位的
    /// （`left(px(rail_bar_left()))`），而按钮盒在栏内居中 —— 盒一宽，原来那个
    /// 写死的 `-10` 就把它推到栏外（窗口左边缘之外），表现是「当前面板没有标记」。
    #[test]
    fn rail_active_bar_stays_inside_the_rail() {
        let left = rail_bar_left();
        assert!(
            left < 0.,
            "指示条相对按钮盒必须是负偏移（按钮盒在栏里居中）"
        );
        let absolute = (RAIL_W - rail_box()) / 2. + left;
        assert_eq!(
            absolute, RAIL_BAR_X,
            "指示条左缘必须正好落在栏内 {RAIL_BAR_X}px 处（实际 {absolute}）"
        );
        assert!(
            absolute >= 0. && absolute + RAIL_BAR_W <= RAIL_W,
            "指示条越出竖栏（栏宽 {RAIL_W}，条占 {absolute}..{}）",
            absolute + RAIL_BAR_W
        );
        assert!(
            rail_box() < RAIL_W,
            "按钮盒比栏还宽：{RAIL_ICON_SIZE:?} 档放不进 {RAIL_W}px 的栏"
        );
    }
}
