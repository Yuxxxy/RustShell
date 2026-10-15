//! 提示条统一入口：把本应用的「语义级别」映射到官方`Alert`的变体。
//!
//! 全应用只有这一处决定「哪个级别的提示长什么样」。业务侧只表达语义
//! （信息 / 成功 / 警告 / 错误），不自己挑 `Alert::info/warning/error`，
//! 也不自己拼「三角图标 + 彩色文字」。此前的分叉正是这么来的：
//! 快捷命令的瞬时提示一律画黄色三角，连「投递失败」也是黄的；设置面板的
//! 成功提示与失败提示都是红的（`set_hint_ok` 写了但没有任何渲染路径读它）。
//!
//! 取色链路：`Alert` 读的是 gpui-component 的`Theme`。本应用在
//! `crate::theme::apply_theme_mode` 里已把 accent/success/warning/danger
//! 四个语义色写进`Theme.colors`并重建 tokens，故四种提示自动跟随本应用主题，
//! 不需要（也不应该）在调用点写 `hex(t.danger)` 之类的硬取色。
//!
//! 图标与变体的配对**照抄**官方构造器（`Alert::info/success/warning/error`），
//! 这里改写成一张表是为了让两者可被单测覆盖 到 官方构造器把配对写进了函数体，
//! 外部无从断言；拆成两张表后「级别 到 变体 / 级别 到 图标」各自只有一处。

use gpui::{px, ElementId, SharedString, Styled as _};
use gpui_component::{
    alert::{Alert, AlertVariant},
    IconName, Sizable as _,
};

/// 提示的语义级别（唯一真源：调用点只选级别，不选颜色与图标）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NoticeKind {
    /// 中性信息（说明性提示、已应用的设置项）
    #[default]
    Info,
    /// 成功（动作已完成）
    Success,
    /// 警告（需要注意，但动作本身没有失败）
    Warning,
    /// 错误（动作失败 / 危险 / 不可逆）
    Error,
}

impl NoticeKind {
    /// 该级别对应的官方变体。
    pub fn variant(self) -> AlertVariant {
        match self {
            Self::Info => AlertVariant::Info,
            Self::Success => AlertVariant::Success,
            Self::Warning => AlertVariant::Warning,
            Self::Error => AlertVariant::Error,
        }
    }

    /// 该级别对应的官方图标（配对与官方构造器一致）。
    pub fn icon(self) -> IconName {
        match self {
            Self::Info => IconName::Info,
            Self::Success => IconName::CircleCheck,
            Self::Warning => IconName::TriangleAlert,
            Self::Error => IconName::CircleX,
        }
    }

    /// 圆角卡片形态：浮层、对话框内、空态说明。
    pub fn card(self, id: impl Into<ElementId>, msg: impl Into<SharedString>) -> Alert {
        // 先收敛成 `SharedString` 再交给官方构造器：`Alert::new` 要的是
        // `impl Into<Text>`，而 `Text` 只有 `From<SharedString/String/&str>`
        // 三个具体实现，泛型实参直接透传满足不了 `Into<Text>`。
        let msg: SharedString = msg.into();
        Alert::new(id, msg)
            .with_variant(self.variant())
            .icon(self.icon())
    }

    /// 通栏条形形态：嵌在面板里占满一行（官方 banner 模式 到 去圆角、不显示标题）。
    pub fn bar(self, id: impl Into<ElementId>, msg: impl Into<SharedString>) -> Alert {
        self.card(id, msg).banner()
    }

    /// 窄行用的紧凑条形：字号压到 11px，与面板内既有正文同级。
    /// （不压字号时走官方 `text_sm` 的 14px，在 40px 高的快捷命令栏里会撑破行高。）
    pub fn compact(self, id: impl Into<ElementId>, msg: impl Into<SharedString>) -> Alert {
        self.bar(id, msg).xsmall().text_size(px(11.))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::prod;

    const ALL: [NoticeKind; 4] = [
        NoticeKind::Info,
        NoticeKind::Success,
        NoticeKind::Warning,
        NoticeKind::Error,
    ];

    /// `ui/src` 下全部 `.rs`（相对路径, 内容），不含本文件。
    fn ui_sources() -> Vec<(String, String)> {
        fn collect(dir: &std::path::Path, root: &std::path::Path, out: &mut Vec<(String, String)>) {
            let Ok(rd) = std::fs::read_dir(dir) else {
                return;
            };
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    collect(&p, root, out);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    let rel = p
                        .strip_prefix(root)
                        .map(|r| r.to_string_lossy().replace('\\', "/"))
                        .unwrap_or_default();
                    if rel == "notice.rs" {
                        continue;
                    }
                    if let Ok(text) = std::fs::read_to_string(&p) {
                        out.push((rel, text));
                    }
                }
            }
        }
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut out = Vec::new();
        collect(&root, &root, &mut out);
        out
    }

    /// 四个语义级别必须落在四个**互不相同**的官方变体上。
    /// 真实风险是复制粘贴 match 臂：`Success => AlertVariant::Warning` 这类
    /// 写错不会编译失败，只会让「成功」长成「警告」的样子。
    #[test]
    fn four_kinds_map_to_four_distinct_variants() {
        let variants: Vec<AlertVariant> = ALL.iter().map(|k| k.variant()).collect();
        for (i, a) in variants.iter().enumerate() {
            for b in variants.iter().skip(i + 1) {
                assert_ne!(
                    a, b,
                    "两个语义级别映射到了同一个 Alert 变体：级别顺序 = Info/Success/Warning/Error"
                );
            }
        }
        assert_eq!(NoticeKind::default(), NoticeKind::Info);
    }

    /// 图标同样必须四个互不相同（否则「错误」会画成信息图标）。
    ///
    /// `IconName` 既没有 `PartialEq` 也没有 `Debug`，只能经 `IconNamed::path()`
    /// 取到实际的 SVG 资源路径来比较。
    #[test]
    fn four_kinds_map_to_four_distinct_icons() {
        use gpui_component::IconNamed as _;

        let paths: Vec<SharedString> = ALL.iter().map(|k| k.icon().path()).collect();
        for p in &paths {
            assert!(!p.is_empty(), "语义级别映射到了空图标路径");
        }
        for (i, a) in paths.iter().enumerate() {
            for b in paths.iter().skip(i + 1) {
                assert_ne!(
                    a, b,
                    "两个语义级别用了同一个图标：级别顺序 = Info/Success/Warning/Error"
                );
            }
        }
    }

    /// 护栏：警告/错误的图标归官方`Alert`所有，面板不得再自绘。
    ///
    /// 这正是本次改造要消除的形态 —— 各面板各自 `Icon::new(IconName::TriangleAlert)`
    /// 加 danger 取色手拼提示条，于是同一个「出错了」在四处长得都不一样，
    /// 且「投递失败」与「没有会话」被画成同一枚黄色三角。
    /// 新增面板若需要提示，走 [`NoticeKind`]，不要回到自绘。
    #[test]
    fn no_panel_hand_rolls_a_warning_icon() {
        let needles = [
            "Icon::new(IconName::TriangleAlert)",
            "Icon::new(IconName::CircleX)",
        ];
        let mut offenders = Vec::new();
        for (rel, src) in ui_sources() {
            // 只看生产代码：注释里出现这些字面量（例如说明「不得这样写」）不算违规，
            // 别的文件的**测试模块**里出现也不算 —— 统一走 `prod()`
            //（剥注释 + 切测试模块），比「筛掉整行注释」可靠。
            // 本文件的 doc 注释里就有这个字面量，故本文件仍被 `ui_sources` 排除。
            let code = prod(&src);
            for n in needles {
                if code.contains(n) {
                    offenders.push(format!("{rel}: {n}"));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "警告/错误提示必须经 `crate::notice` 走官方 Alert，不得自绘图标：{offenders:?}"
        );
    }
}
