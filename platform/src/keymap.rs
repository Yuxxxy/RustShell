//! 跨平台键位映射：`mod`抽象 到 平台具体修饰键。

/// `mod`修饰键的平台键名（gpui Keystroke 解析用）
pub fn mod_key() -> &'static str {
    if cfg!(target_os = "macos") {
        "cmd"
    } else {
        "ctrl"
    }
}

/// `opt`修饰键的平台键名
pub fn opt_key() -> &'static str {
    "alt"
}

/// UI 提示文本（：macOS 符号 / Win-Linux 文字）。
pub fn mod_hint() -> &'static str {
    rustshell_core::command::mod_hint()
}

/// 组合键绑定串，如 bind("T") 则 "ctrl-t"
pub fn bind(key: &str) -> String {
    format!("{}-{}", mod_key(), key.to_lowercase())
}

/// Shift 组合键绑定串，如 bind_shift("1") 则 "ctrl-shift-1"
pub fn bind_shift(key: &str) -> String {
    format!("{}-shift-{}", mod_key(), key.to_lowercase())
}

// 键位持久化：默认表 + 用户覆盖（keymap.json），支持导入/导出

use std::collections::BTreeMap;
use std::path::Path;

/// 默认键位表（action id 到 键位描述，"mod" 为平台修饰键占位）
pub const DEFAULT_BINDINGS: &[(&str, &str)] = &[
    ("NewTab", "mod-t"),
    ("CloseTab", "mod-w"),
    ("TabNext", "mod-tab"),
    ("TabPrev", "mod-shift-tab"),
    ("OpenSettings", "mod-,"),
    ("CommandPalette", "mod-shift-p"),
    ("SearchInTerminal", "mod-f"),
    // 分屏：R=右（左右分屏）、D=下（上下分屏）；字母键避免方向键在不同后端上的键名差异
    ("SplitRight", "mod-alt-r"),
    ("SplitDown", "mod-alt-d"),
];

/// 把 "mod-t" 这类描述解析为平台具体键位串（"ctrl-t" / "cmd-t"）
pub fn resolve(spec: &str) -> String {
    spec.replace("mod-", &format!("{}-", mod_key()))
}

/// 读取用户覆盖（keymap.json：{ "NewTab": "mod-shift-t", ... }）
pub fn load_overrides(path: &Path) -> BTreeMap<String, String> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// 保存用户覆盖（原子写入）
pub fn save_overrides(path: &Path, overrides: &BTreeMap<String, String>) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let json = serde_json::to_string_pretty(overrides)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// 生效键位表：默认表 + 用户覆盖（仅允许覆盖已有 action，防拼写错误产生死键位）
pub fn effective(path: &Path) -> Vec<(String, String)> {
    let overrides = load_overrides(path);
    DEFAULT_BINDINGS
        .iter()
        .map(|(id, spec)| {
            let spec = overrides
                .get(*id)
                .cloned()
                .unwrap_or_else(|| spec.to_string());
            ((*id).to_string(), resolve(&spec))
        })
        .collect()
}

/// 导出当前生效键位到目标文件（keymap.json 兼容格式，可再导入）
pub fn export_bindings(path: &Path, out: &Path) -> anyhow::Result<()> {
    let map: BTreeMap<String, String> = effective(path).into_iter().collect();
    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(out, serde_json::to_string_pretty(&map)?)?;
    Ok(())
}

/// 从外部文件导入键位覆盖（仅接受 DEFAULT_BINDINGS 中存在的 action id）
pub fn import_bindings(path: &Path, src: &Path) -> anyhow::Result<usize> {
    let external: BTreeMap<String, String> = serde_json::from_str(&std::fs::read_to_string(src)?)?;
    let mut merged = load_overrides(path);
    let mut count = 0;
    for (id, spec) in external {
        if DEFAULT_BINDINGS.iter().any(|(valid, _)| *valid == id) {
            merged.insert(id, spec);
            count += 1;
        }
    }
    save_overrides(path, &merged)?;
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_platform_prefix() {
        if cfg!(target_os = "macos") {
            assert_eq!(resolve("mod-t"), "cmd-t");
        } else {
            assert_eq!(resolve("mod-t"), "ctrl-t");
        }
    }

    #[test]
    fn effective_applies_overrides() {
        let dir = std::env::temp_dir().join(format!("rs-keymap-test-{}", std::process::id()));
        let path = dir.join("keymap.json");
        let mut o = BTreeMap::new();
        o.insert("NewTab".to_string(), "mod-shift-n".to_string());
        save_overrides(&path, &o).unwrap();
        let eff = effective(&path);
        let new_tab = eff.iter().find(|(id, _)| id == "NewTab").unwrap();
        assert_eq!(new_tab.1, resolve("mod-shift-n"));
        // 未覆盖的保持默认
        let close_tab = eff.iter().find(|(id, _)| id == "CloseTab").unwrap();
        assert_eq!(close_tab.1, resolve("mod-w"));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn import_rejects_unknown_actions() {
        let dir = std::env::temp_dir().join(format!("rs-keymap-imp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let keymap = dir.join("keymap.json");
        let src = dir.join("import.json");
        std::fs::write(&src, r#"{"NewTab":"mod-y","Bogus":"mod-q"}"#).unwrap();
        let n = import_bindings(&keymap, &src).unwrap();
        assert_eq!(n, 1);
        let o = load_overrides(&keymap);
        assert!(o.contains_key("NewTab"));
        assert!(!o.contains_key("Bogus"));
        std::fs::remove_dir_all(dir).ok();
    }
}
