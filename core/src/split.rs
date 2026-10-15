//! 标签分屏的决策逻辑（纯函数，不碰 UI 与 IO）。
//!
//! 分屏模型：标签栏是「池子」，两个窗格各自显示池中的一个标签。
//! 本模块只回答「点一下拆分该做什么」，具体落地（移动槽位 / 新建会话）留在 UI 层。

use serde::{Deserialize, Serialize};

/// 分屏轴向：决定第二个窗格出现在哪一侧。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum SplitAxis {
    /// 左右分屏（「向右拆分」）：副窗格在右侧
    #[default]
    Vertical,
    /// 上下分屏（「向下拆分」）：副窗格在下方
    Horizontal,
}

impl SplitAxis {
    /// 另一侧的轴向（再次拆分即切换方向）。
    pub fn flipped(self) -> Self {
        match self {
            SplitAxis::Vertical => SplitAxis::Horizontal,
            SplitAxis::Horizontal => SplitAxis::Vertical,
        }
    }
}

/// 拆分动作的决策结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SplitPlan {
    /// 没有可拆分的标签（无激活会话）
    Noop,
    /// 已在分屏中：只切换方向，不动任何标签、不新建会话
    SwitchAxis(SplitAxis),
    /// 池中除焦点标签外还有别的标签：把那个标签放进副窗格（零成本、立即生效）
    Show { secondary: String },
    /// 池中只有焦点标签：需要新建一个同类型会话放进副窗格
    NeedSession,
}

/// 决定「拆分」要做什么。
///
/// - `focused`：焦点窗格当前标签（None = 没有任何激活会话）
/// - `pool`：全部标签的 session id（含焦点标签）
/// - `split`：当前是否已在分屏（已在则只切方向）
/// - `axis`：本次请求的方向
///
/// 池中挑选副窗格标签时按 `pool` 顺序取第一个不是焦点标签的条目，
/// 保证同一状态下结果稳定（不受 HashMap 迭代顺序之类影响）。
pub fn plan_split(
    focused: Option<&str>,
    pool: &[String],
    in_split: bool,
    axis: SplitAxis,
) -> SplitPlan {
    let Some(focused) = focused else {
        return SplitPlan::Noop;
    };
    if in_split {
        return SplitPlan::SwitchAxis(axis);
    }
    match pool.iter().find(|id| id.as_str() != focused) {
        Some(other) => SplitPlan::Show {
            secondary: other.clone(),
        },
        None => SplitPlan::NeedSession,
    }
}

/// 分屏态修正的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SplitFix {
    /// 两个槽位都合法，保持不变
    Keep,
    /// 副窗格换标签（关掉副窗格标签后，用池里另一个标签补位）
    ReplaceSecondary { with: String },
    /// 主窗格标签没了，但副窗格还活着：副窗格升格为主窗格，另找一个标签进副窗格
    PromoteSecondary { primary: String, secondary: String },
    /// 无法维持分屏，应取消（取消分屏不等于关闭会话，标签都留在池子里）
    Unsplit,
}

/// 依据当前池子修正分屏态的不变量：
/// 两个槽位都必须存在且互不相同；某个槽位不合法时优先用池中其它标签补位，
/// 补不上就取消分屏 —— 绝不留「窗格指向已关闭标签」的半残状态。
pub fn fix_split(primary: &str, secondary: &str, pool: &[String]) -> SplitFix {
    let primary_ok = pool.iter().any(|p| p == primary);
    // 两槽位指向同一标签时也视为副窗格失效（必须能自愈）
    let secondary_ok = secondary != primary && pool.iter().any(|p| p == secondary);

    if primary_ok && secondary_ok {
        return SplitFix::Keep;
    }

    // 主窗格失效：只有副窗格还活着时才可能维持分屏
    if !primary_ok {
        if !pool.iter().any(|p| p == secondary) {
            return SplitFix::Unsplit;
        }
        return match pool.iter().find(|id| id.as_str() != secondary) {
            Some(with) => SplitFix::PromoteSecondary {
                primary: secondary.to_string(),
                secondary: with.clone(),
            },
            None => SplitFix::Unsplit,
        };
    }

    // 主窗格还在，副窗格失效：换个标签顶上
    match pool.iter().find(|id| id.as_str() != primary) {
        Some(with) => SplitFix::ReplaceSecondary { with: with.clone() },
        None => SplitFix::Unsplit,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    /// 没有激活会话 到 什么都不做（调用方据此给出提示，而不是静默）
    #[test]
    fn no_focus_is_noop() {
        assert_eq!(
            plan_split(None, &pool(&["a"]), false, SplitAxis::Vertical),
            SplitPlan::Noop
        );
    }

    /// 已在分屏 到 只切方向（不新建会话、不移动标签）
    #[test]
    fn in_split_switches_axis() {
        assert_eq!(
            plan_split(Some("a"), &pool(&["a", "b"]), true, SplitAxis::Horizontal),
            SplitPlan::SwitchAxis(SplitAxis::Horizontal)
        );
        // 方向切换与池子内容无关
        assert_eq!(
            plan_split(Some("a"), &pool(&["a"]), true, SplitAxis::Vertical),
            SplitPlan::SwitchAxis(SplitAxis::Vertical)
        );
    }

    /// 池中还有别的标签 到 直接用它填副窗格，且挑选结果稳定（取第一个）
    #[test]
    fn other_tab_fills_secondary() {
        assert_eq!(
            plan_split(
                Some("a"),
                &pool(&["a", "b", "c"]),
                false,
                SplitAxis::Vertical
            ),
            SplitPlan::Show {
                secondary: "b".into()
            }
        );
        // 焦点标签在池子中间时，仍然取第一个「不是焦点」的标签
        assert_eq!(
            plan_split(
                Some("b"),
                &pool(&["a", "b", "c"]),
                false,
                SplitAxis::Vertical
            ),
            SplitPlan::Show {
                secondary: "a".into()
            }
        );
    }

    /// 池中只有焦点标签 到 需要新建会话
    #[test]
    fn only_one_tab_needs_session() {
        assert_eq!(
            plan_split(Some("a"), &pool(&["a"]), false, SplitAxis::Horizontal),
            SplitPlan::NeedSession
        );
        // 池子为空（理论上不该发生）也走同一分支，不 panic
        assert_eq!(
            plan_split(Some("a"), &pool(&[]), false, SplitAxis::Horizontal),
            SplitPlan::NeedSession
        );
    }

    /// 轴向翻转
    #[test]
    fn axis_flips() {
        assert_eq!(SplitAxis::Vertical.flipped(), SplitAxis::Horizontal);
        assert_eq!(SplitAxis::Horizontal.flipped(), SplitAxis::Vertical);
    }

    /// 槽位合法 到 保持不变
    #[test]
    fn fix_keeps_valid_slots() {
        assert_eq!(fix_split("a", "b", &pool(&["a", "b", "c"])), SplitFix::Keep);
    }

    /// 关掉副窗格标签 到 用池里另一个标签补位（不取消分屏，也不关会话）
    #[test]
    fn fix_replaces_dead_secondary() {
        assert_eq!(
            fix_split("a", "gone", &pool(&["a", "c"])),
            SplitFix::ReplaceSecondary { with: "c".into() }
        );
    }

    /// 两个槽位都失效 / 池子只剩一个 到 取消分屏
    #[test]
    fn fix_unsplits_when_unfillable() {
        assert_eq!(fix_split("gone", "gone2", &pool(&["a"])), SplitFix::Unsplit);
        assert_eq!(fix_split("a", "gone", &pool(&["a"])), SplitFix::Unsplit);
    }

    /// 两个槽位指向同一标签（不应出现，但必须能自愈） 到 用别的标签替换副窗格
    #[test]
    fn fix_handles_duplicate_slots() {
        assert_eq!(
            fix_split("a", "a", &pool(&["a", "b"])),
            SplitFix::ReplaceSecondary { with: "b".into() }
        );
        assert_eq!(fix_split("a", "a", &pool(&["a"])), SplitFix::Unsplit);
    }

    /// 主窗格失效而副窗格尚存 到 副窗格顶上，另找一个进副窗格
    #[test]
    fn fix_promotes_surviving_secondary() {
        assert_eq!(
            fix_split("gone", "b", &pool(&["b", "c"])),
            SplitFix::PromoteSecondary {
                primary: "b".into(),
                secondary: "c".into()
            }
        );
        assert_eq!(fix_split("gone", "b", &pool(&["b"])), SplitFix::Unsplit);
    }
}
