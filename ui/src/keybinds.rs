//! 键位管理：设置面板键位表格数据、录制换算、运行时重绑定。

use std::any::TypeId;

use gpui::{App, KeyBinding, Keymap, Keystroke};
use rustshell_platform::keymap as km;
use rustshell_platform::paths;

use crate::main_view::{
    CloseTab, CommandPalette, NewTab, OpenSettings, SplitDown, SplitRight, TabNext, TabPrev,
};

/// 键位表格行快照（设置面板渲染用）
#[derive(Debug, Clone)]
pub struct KeymapRow {
    pub action_id: String,
    pub label: String,
    /// 存储用 spec（"mod-t"；"mod-" 为平台修饰键占位，用户覆盖时为具体组合）
    pub spec: String,
    /// 是否存在用户覆盖（keymap.json 中有记录）
    pub overridden: bool,
}

/// 键位表那一行的中文名。
///
/// 取 [`rustshell_core::command::action_label`]（**全量**动作表），不是命令面板的
/// `builtin_commands()`：2026-09-17 面板简化后两者不再是同一份清单，键位表仍要显示
/// 标签/分屏这些「不进面板」的动作名。
pub fn builtin_label(action_id: &str) -> String {
    rustshell_core::command::action_label(action_id)
        .map(|key| rustshell_core::i18n::t(key).to_string())
        .unwrap_or_else(|| action_id.to_string())
}

/// 从 keymap.json 生成键位表格快照（按内置命令默认表顺序）
pub fn snapshot_rows() -> Vec<KeymapRow> {
    let overrides = km::load_overrides(&paths::keymap_path());
    km::DEFAULT_BINDINGS
        .iter()
        .map(|(id, default_spec)| {
            let overridden = overrides.contains_key(*id);
            let spec = overrides
                .get(*id)
                .cloned()
                .unwrap_or_else(|| default_spec.to_string());
            KeymapRow {
                action_id: (*id).to_string(),
                label: builtin_label(id),
                spec,
                overridden,
            }
        })
        .collect()
}

/// 空窗格「快捷键提示」要列的动作（顺序 = 展示顺序，第一行是空窗格里最该做的事）。
///
/// 2026-09-20：空窗格从「图标 + 一句说明 + 一个按钮」改成 VS Code 那种
/// 「动作名 …… 键帽」提示表，这是那张表的行。
///
/// 选材标准：此刻窗格里**没有任何会话**，所以只列「怎么把会话弄出来」和
/// 「其余能力的入口在哪」。终端相关动作（终端内搜索 / 拆分 / 切标签）刻意不列 ——
/// 现在按下它们一点效果都没有，列出来只会让人以为坏了。
pub const EMPTY_STATE_ACTIONS: &[&str] =
    &["NewTab", "CommandPalette", "OpenSettings"];

/// 生成空窗格提示行：`(显示名, 键位展示串)`，与 [`EMPTY_STATE_ACTIONS`] 一一对应。
///
/// 名称与键位取自**同一份快照**（[`KeymapRow`]）⇒ 用户在设置 → 键位里改过绑定、
/// 或切换了界面语言，这张表会跟着变，不会出现「照着提示按却没反应」。
///
/// 快照还没建立时（`Vec::new()`，即 [`MainView`](crate::main_view::MainView) 构造期）
/// 回退到 [`rustshell_platform::keymap::DEFAULT_BINDINGS`] + 全量动作表的名字，
/// 保证空态任何时候都有内容 —— 空态是「应用刚起来、什么都还没有」时唯一能看的东西，
/// 它不能因为一个尚未填充的缓存而空着。
pub fn empty_state_hints(rows: &[KeymapRow]) -> Vec<(String, String)> {
    EMPTY_STATE_ACTIONS
        .iter()
        .map(|id| match rows.iter().find(|r| r.action_id == *id) {
            Some(r) => (r.label.clone(), display_spec(&r.spec)),
            None => {
                let spec = rustshell_platform::keymap::DEFAULT_BINDINGS
                    .iter()
                    .find(|(action, _)| action == id)
                    .map(|(_, spec)| *spec)
                    .unwrap_or("");
                (builtin_label(id), display_spec(spec))
            }
        })
        .collect()
}

/// 录制到的 Keystroke 到 存储用 spec（如 "mod-shift-t"）。
pub fn keystroke_to_spec(k: &Keystroke) -> Option<String> {
    let key = k.key.as_ref();
    if matches!(key, "control" | "alt" | "shift" | "platform" | "function") {
        return None;
    }
    let m = &k.modifiers;
    let mut parts: Vec<String> = Vec::new();
    if m.platform {
        parts.push(km::mod_key().to_string());
    } else if m.control {
        parts.push("ctrl".to_string());
    }
    if m.alt {
        parts.push(km::opt_key().to_string());
    }
    if m.shift {
        parts.push("shift".to_string());
    }
    parts.push(key.to_lowercase());
    Some(parts.join("-"))
}

/// spec 是否带可派发的修饰键（防止录制到裸字母/数字抢占终端输入）
pub fn has_action_modifier(spec: &str) -> bool {
    let first = spec.split('-').next().unwrap_or("");
    matches!(first, "mod" | "ctrl" | "cmd" | "alt")
}

/// spec 到 展示串（"mod-shift-t" 到 "Ctrl+Shift+T"）
pub fn display_spec(spec: &str) -> String {
    km::resolve(spec)
        .split('-')
        .map(|part| match part {
            "ctrl" => "Ctrl".to_string(),
            "alt" => "Alt".to_string(),
            "shift" => "Shift".to_string(),
            "cmd" => "⌘".to_string(),
            "tab" => "Tab".to_string(),
            "escape" => "Esc".to_string(),
            other => {
                let mut c = other.chars();
                match c.next() {
                    Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                    None => String::new(),
                }
            }
        })
        .collect::<Vec<_>>()
        .join("+")
}

/// action id + 平台键位串 到 KeyBinding（与 shell 启动注册逻辑一致）
pub fn make_binding(action_id: &str, keys: &str) -> Option<KeyBinding> {
    use crate::main_view::{SearchInTerminal, SwitchTab, TermShiftTab, TermTab};
    Some(match action_id {
        "NewTab" => KeyBinding::new(keys, NewTab, None),
        "CloseTab" => KeyBinding::new(keys, CloseTab, None),
        "TabNext" => KeyBinding::new(keys, TabNext, None),
        "TabPrev" => KeyBinding::new(keys, TabPrev, None),
        "OpenSettings" => KeyBinding::new(keys, OpenSettings, None),
        "CommandPalette" => KeyBinding::new(keys, CommandPalette, None),
        "SearchInTerminal" => KeyBinding::new(keys, SearchInTerminal, None),
        "SplitRight" => KeyBinding::new(keys, SplitRight, None),
        "SplitDown" => KeyBinding::new(keys, SplitDown, None),
        // 不可重绑定的固定绑定（终端补全 / 数字切 Tab），仅用于启动注册
        "TermTab" => KeyBinding::new(keys, TermTab, None),
        "TermShiftTab" => KeyBinding::new(keys, TermShiftTab, None),
        "SwitchTab" => KeyBinding::new(keys, SwitchTab { index: 0 }, None),
        _ => return None,
    })
}

fn action_type_id(action_id: &str) -> Option<TypeId> {
    Some(match action_id {
        "NewTab" => TypeId::of::<NewTab>(),
        "CloseTab" => TypeId::of::<CloseTab>(),
        "TabNext" => TypeId::of::<TabNext>(),
        "TabPrev" => TypeId::of::<TabPrev>(),
        "OpenSettings" => TypeId::of::<OpenSettings>(),
        "CommandPalette" => TypeId::of::<CommandPalette>(),
        _ => return None,
    })
}

/// 运行时重绑定：移除该 action 的全部旧绑定、注册新绑定（保留其余所有绑定）。
pub fn apply_override_live(cx: &mut App, action_id: &str, new_keys: &str) {
    let Some(tid) = action_type_id(action_id) else {
        return;
    };
    let Some(new_binding) = make_binding(action_id, new_keys) else {
        return;
    };
    let keymap = cx.key_bindings();
    let kept: Vec<KeyBinding> = {
        let km_ref = keymap.borrow();
        km_ref
            .bindings()
            .filter(|b| b.action().as_any().type_id() != tid)
            .cloned()
            .collect()
    };
    let mut all = kept;
    all.push(new_binding);
    *keymap.borrow_mut() = Keymap::new(all);
    cx.refresh_windows();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keystroke_to_spec_combinations() {
        let k = Keystroke {
            key: "t".into(),
            key_char: None,
            modifiers: gpui::Modifiers {
                control: true,
                ..Default::default()
            },
        };
        assert_eq!(keystroke_to_spec(&k).unwrap(), km::bind("t"));

        let k = Keystroke {
            key: "p".into(),
            key_char: None,
            modifiers: gpui::Modifiers {
                control: true,
                shift: true,
                ..Default::default()
            },
        };
        assert_eq!(keystroke_to_spec(&k).unwrap(), km::bind_shift("p"));

        // 裸修饰键：继续等待
        let k = Keystroke {
            key: "control".into(),
            key_char: None,
            modifiers: Default::default(),
        };
        assert!(keystroke_to_spec(&k).is_none());
    }

    #[test]
    fn modifier_requirement() {
        assert!(has_action_modifier("mod-t"));
        assert!(has_action_modifier("ctrl-alt-x"));
        assert!(!has_action_modifier("shift-tab"));
        assert!(!has_action_modifier("a"));
    }

    #[test]
    fn display_spec_formats() {
        assert_eq!(display_spec("mod-t"), "Ctrl+T");
        assert_eq!(display_spec("mod-shift-p"), "Ctrl+Shift+P");
        assert_eq!(display_spec("mod-,"), "Ctrl+,");
        assert_eq!(display_spec("mod-tab"), "Ctrl+Tab");
    }

    #[test]
    fn snapshot_rows_match_defaults() {
        let rows = snapshot_rows();
        assert_eq!(rows.len(), km::DEFAULT_BINDINGS.len());
        let new_tab = rows.iter().find(|r| r.action_id == "NewTab").unwrap();
        assert_eq!(new_tab.label, "新建本地终端");
        assert_eq!(new_tab.spec, "mod-t");
        assert!(!new_tab.overridden);
    }

    /// 空窗格提示表选的动作必须都在默认键位表里 —— 否则 [`empty_state_hints`]
    /// 的回退分支会给出空键位串，画面上就是一行「没有键帽的提示」。
    #[test]
    fn empty_state_actions_are_all_bindable() {
        for id in EMPTY_STATE_ACTIONS {
            assert!(
                km::DEFAULT_BINDINGS.iter().any(|(action, _)| action == id),
                "{id} 不在 DEFAULT_BINDINGS 里，空窗格提示会找不到键位"
            );
            assert!(
                rustshell_core::command::action_label(id).is_some(),
                "{id} 不在全量动作表里，空窗格提示会退化成显示动作 id"
            );
        }
    }

    /// 快照还没建立时也要有内容（回退到默认表），且顺序与 [`EMPTY_STATE_ACTIONS`] 一致。
    #[test]
    fn empty_state_hints_fall_back_to_defaults() {
        let hints = empty_state_hints(&[]);
        assert_eq!(hints.len(), EMPTY_STATE_ACTIONS.len());
        let (label, keys) = &hints[0];
        assert_eq!(label, "新建本地终端");
        assert_eq!(keys, &display_spec("mod-t"));
        assert!(hints.iter().all(|(_, keys)| !keys.is_empty()));
    }

    /// 用户改过键位 / 切换语言后，提示表跟着快照走（不是永远显示默认键位）。
    #[test]
    fn empty_state_hints_follow_the_snapshot() {
        let rows = vec![KeymapRow {
            action_id: "CommandPalette".to_string(),
            label: "命令面板".to_string(),
            spec: "mod-shift-k".to_string(),
            overridden: true,
        }];
        let hints = empty_state_hints(&rows);
        // 被覆盖的那一行用快照的值……
        let (label, keys) = &hints[1];
        assert_eq!(label, "命令面板");
        assert_eq!(keys, &display_spec("mod-shift-k"));
        // ……没被覆盖的那些行仍然回退到默认表，一行都不缺。
        assert_eq!(hints[0].1, display_spec("mod-t"));
        assert_eq!(hints.len(), EMPTY_STATE_ACTIONS.len());
    }
}
