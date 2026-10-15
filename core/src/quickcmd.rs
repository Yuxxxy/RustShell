//! 快捷命令：文件夹 + 命令，JSON 持久化，支持多窗口投递与折叠分组。

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// 当前存储 schema 版本。
///
/// - v1：无 `schema_version` 字段（历史文件）
/// - v2：引入 `CommandFolder.collapsed`
/// - v3：删除 `QuickCmd.confirm`，引入 `QuickCmd.targets`
/// - v4：引入 `QuickCmd.append_newline`（旧文件缺该键默认 `true`，保持「写完即执行」语义）
pub const SCHEMA_VERSION: u32 = 4;

fn default_schema_version() -> u32 {
    SCHEMA_VERSION
}

/// `bool`的 serde 默认值`true`：保持旧文件「立即执行」语义，避免静默变成「只键入」。
fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuickCmd {
    pub id: String,
    pub label: String,
    pub script: String,
    /// 多窗口投递目标（会话 id 列表）；空 = 用当前激活 Tab。
    #[serde(default)]
    pub targets: Vec<String>,
    /// 发送时是否在脚本末尾追加回车符（`\n`）。
    #[serde(default = "default_true")]
    pub append_newline: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommandFolder {
    pub id: String,
    pub name: String,
    /// 展开态下该分组是否收起
    #[serde(default)]
    pub collapsed: bool,
    #[serde(default)]
    pub commands: Vec<QuickCmd>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuickCommandStore {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    pub folders: Vec<CommandFolder>,
}

impl Default for QuickCommandStore {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            folders: Vec::new(),
        }
    }
}

impl QuickCommandStore {
    pub fn load(path: &PathBuf) -> Self {
        match std::fs::read_to_string(path) {
            Ok(s) => match serde_json::from_str::<Self>(&s) {
                // 对齐到当前版本（旧文件读入后即升版；不写盘，等到下次 save）
                Ok(mut v) => {
                    v.schema_version = SCHEMA_VERSION;
                    v
                }
                Err(_) => {
                    // 解析失败先留底坏文件，再回退默认
                    crate::configio::backup_broken_config(path, &s);
                    Self::seed()
                }
            },
            Err(_) => Self::seed(),
        }
    }

    pub fn save(&self, path: &PathBuf) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let json = serde_json::to_string_pretty(self)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// 首次启动种子数据（示例：部署 / 诊断）
    fn seed() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            folders: vec![
                CommandFolder {
                    id: crate::connections::uuid_like(),
                    name: "部署".into(),
                    collapsed: false,
                    commands: vec![
                        cmd("build", "echo [build] make -j4"),
                        cmd("restart", "echo [restart] systemctl restart app"),
                        cmd("logs", "echo [logs] tail -f /var/log/app.log"),
                    ],
                },
                CommandFolder {
                    id: crate::connections::uuid_like(),
                    name: "诊断".into(),
                    collapsed: false,
                    commands: vec![
                        cmd("top", "top -b -n 1 | head -20"),
                        cmd("df", "df -h"),
                        cmd("tail", "tail -100 /var/log/syslog"),
                    ],
                },
            ],
        }
    }

    // 分组动作

    /// 新建分组，返回新分组 id（便于创建后立即进入行内重命名态）
    pub fn add_folder(&mut self, name: impl Into<String>) -> String {
        let id = crate::connections::uuid_like();
        self.folders.push(CommandFolder {
            id: id.clone(),
            name: name.into(),
            collapsed: false,
            commands: vec![],
        });
        id
    }

    /// 分组重命名。空名（trim 后）返回`false`且不改动，调用方保留原名。
    pub fn rename_folder(&mut self, folder_id: &str, name: &str) -> bool {
        let name = name.trim();
        if name.is_empty() {
            return false;
        }
        let Some(f) = self.folders.iter_mut().find(|f| f.id == folder_id) else {
            return false;
        };
        if f.name == name {
            return false;
        }
        f.name = name.to_string();
        true
    }

    /// 删除分组（连带其命令）。二次确认在 UI 层做，这里只负责执行。
    pub fn remove_folder(&mut self, folder_id: &str) -> bool {
        let before = self.folders.len();
        self.folders.retain(|f| f.id != folder_id);
        self.folders.len() != before
    }

    /// 复制分组（含全部命令，id 全部新生成，名加「副本」）
    pub fn duplicate_folder(&mut self, folder_id: &str, suffix: &str) -> Option<String> {
        let src = self.folders.iter().find(|f| f.id == folder_id)?.clone();
        let new_id = crate::connections::uuid_like();
        let mut commands: Vec<QuickCmd> = src
            .commands
            .iter()
            .map(|c| QuickCmd {
                id: crate::connections::uuid_like(),
                label: c.label.clone(),
                script: c.script.clone(),
                targets: c.targets.clone(),
                append_newline: c.append_newline,
            })
            .collect();
        for c in commands.iter_mut() {
            // 逐条复制时 id 全部重生成，避免与源组命令同 id
            c.id = crate::connections::uuid_like();
        }
        self.folders.push(CommandFolder {
            id: new_id.clone(),
            name: format!("{}{}", src.name, suffix),
            collapsed: false,
            commands,
        });
        Some(new_id)
    }

    /// 设置分组展开/收起，返回是否发生变化
    pub fn set_folder_collapsed(&mut self, folder_id: &str, collapsed: bool) -> bool {
        let Some(f) = self.folders.iter_mut().find(|f| f.id == folder_id) else {
            return false;
        };
        if f.collapsed == collapsed {
            return false;
        }
        f.collapsed = collapsed;
        true
    }

    /// 批量展开 / 收起全部分组（菜单「展开全部 / 收起全部」），返回是否有变化
    pub fn set_all_collapsed(&mut self, collapsed: bool) -> bool {
        let mut changed = false;
        for f in self.folders.iter_mut() {
            if f.collapsed != collapsed {
                f.collapsed = collapsed;
                changed = true;
            }
        }
        changed
    }

    /// 分组重排：把 from_id 分组移动到 to_id 分组当前所在的位置（其余依次后移）。
    pub fn move_folder(&mut self, from_id: &str, to_id: &str) -> bool {
        let Some(from) = self.folders.iter().position(|f| f.id == from_id) else {
            return false;
        };
        let Some(to) = self.folders.iter().position(|f| f.id == to_id) else {
            return false;
        };
        if from == to {
            return false;
        }
        let folder = self.folders.remove(from);
        self.folders.insert(to, folder);
        true
    }

    // 命令动作

    /// 在指定分组末尾追加一条命令，返回新命令 id
    pub fn add_cmd(&mut self, folder_id: &str, label: &str, script: &str) -> Option<String> {
        let f = self.folders.iter_mut().find(|f| f.id == folder_id)?;
        let id = crate::connections::uuid_like();
        f.commands.push(QuickCmd {
            id: id.clone(),
            label: label.to_string(),
            script: script.to_string(),
            targets: Vec::new(),
            append_newline: true,
        });
        Some(id)
    }

    /// 定位命令所在分组 id
    pub fn folder_of_cmd(&self, cmd_id: &str) -> Option<String> {
        self.folders
            .iter()
            .find(|f| f.commands.iter().any(|c| c.id == cmd_id))
            .map(|f| f.id.clone())
    }

    /// 按 id 取命令（只读）
    pub fn find_cmd(&self, cmd_id: &str) -> Option<&QuickCmd> {
        self.folders
            .iter()
            .flat_map(|f| f.commands.iter())
            .find(|c| c.id == cmd_id)
    }

    /// 更新命令的标签、脚本与「末尾回车」开关（`targets`不受影响）。
    pub fn update_cmd(
        &mut self,
        cmd_id: &str,
        label: &str,
        script: &str,
        append_newline: bool,
    ) -> bool {
        for f in self.folders.iter_mut() {
            if let Some(c) = f.commands.iter_mut().find(|c| c.id == cmd_id) {
                c.label = label.to_string();
                c.script = script.to_string();
                c.append_newline = append_newline;
                return true;
            }
        }
        false
    }

    /// 删除命令。
    pub fn remove_cmd(&mut self, cmd_id: &str) -> bool {
        for f in self.folders.iter_mut() {
            let before = f.commands.len();
            f.commands.retain(|c| c.id != cmd_id);
            if f.commands.len() != before {
                return true;
            }
        }
        false
    }

    /// 复制命令到同组末尾，返回新命令 id
    pub fn duplicate_cmd(&mut self, cmd_id: &str, suffix: &str) -> Option<String> {
        let folder = self
            .folders
            .iter_mut()
            .find(|f| f.commands.iter().any(|c| c.id == cmd_id))?;
        let src = folder.commands.iter().find(|c| c.id == cmd_id)?.clone();
        let id = crate::connections::uuid_like();
        folder.commands.push(QuickCmd {
            id: id.clone(),
            label: format!("{}{}", src.label, suffix),
            script: src.script.clone(),
            targets: src.targets.clone(),
            append_newline: src.append_newline,
        });
        Some(id)
    }

    /// 命令跨组移动，落到目标组末尾
    pub fn move_cmd_to(&mut self, cmd_id: &str, to_folder_id: &str) -> bool {
        // 先在目标组存在性上早失败，避免把命令从源组摘下来却无处可放
        if !self.folders.iter().any(|f| f.id == to_folder_id) {
            return false;
        }
        let Some(from_idx) = self
            .folders
            .iter()
            .position(|f| f.commands.iter().any(|c| c.id == cmd_id))
        else {
            return false;
        };
        // 同组 = 无需移动
        if self.folders[from_idx].id == to_folder_id {
            return false;
        }
        let Some(cmd) = self.folders[from_idx]
            .commands
            .iter()
            .position(|c| c.id == cmd_id)
        else {
            return false;
        };
        let cmd = self.folders[from_idx].commands.remove(cmd);
        if let Some(to) = self.folders.iter_mut().find(|f| f.id == to_folder_id) {
            to.commands.push(cmd);
            return true;
        }
        // 目标组在此期间消失：把命令放回原位，保持数据一致（不静默丢命令）
        self.folders[from_idx].commands.push(cmd);
        false
    }

    /// 分组内命令上移/下移：delta = -1 上移，+1 下移。
    pub fn move_command(&mut self, folder_id: &str, cmd_id: &str, delta: i32) -> bool {
        let Some(folder) = self.folders.iter_mut().find(|f| f.id == folder_id) else {
            return false;
        };
        let Some(pos) = folder.commands.iter().position(|c| c.id == cmd_id) else {
            return false;
        };
        let new_pos = pos as i64 + delta as i64;
        if new_pos < 0 || new_pos >= folder.commands.len() as i64 {
            return false;
        }
        let cmd = folder.commands.remove(pos);
        folder.commands.insert(new_pos as usize, cmd);
        true
    }

    /// 设置命令的多窗口投递目标，返回是否发生变化
    pub fn set_cmd_targets(&mut self, cmd_id: &str, targets: Vec<String>) -> bool {
        for f in self.folders.iter_mut() {
            if let Some(c) = f.commands.iter_mut().find(|c| c.id == cmd_id) {
                if c.targets == targets {
                    return false;
                }
                c.targets = targets;
                return true;
            }
        }
        false
    }
}

fn cmd(label: &str, script: &str) -> QuickCmd {
    QuickCmd {
        id: crate::connections::uuid_like(),
        label: label.into(),
        script: script.into(),
        targets: Vec::new(),
        append_newline: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> QuickCommandStore {
        let mut s = QuickCommandStore::default();
        s.add_folder("A");
        s.add_folder("B");
        s.add_folder("C");
        for (i, name) in ["one", "two", "three"].iter().enumerate() {
            s.folders[0].commands.push(QuickCmd {
                id: format!("c{}", i + 1),
                label: (*name).into(),
                script: (*name).into(),
                targets: Vec::new(),
                append_newline: true,
            });
        }
        s
    }

    #[test]
    fn move_folder_reorders() {
        let mut s = store();
        let (a, c) = (s.folders[0].id.clone(), s.folders[2].id.clone());
        // 初始 [A, B, C]；A 到 C 的位置(2)：remove A 到 [B, C]，insert at 2 到 [B, C, A]
        assert!(s.move_folder(&a, &c));
        assert_eq!(
            s.folders
                .iter()
                .map(|f| f.name.as_str())
                .collect::<Vec<_>>(),
            ["B", "C", "A"]
        );
        // 无效 id
        assert!(!s.move_folder("nope", &c));
        // 自身
        let b = s.folders[0].id.clone();
        assert!(!s.move_folder(&b, &b));
    }

    #[test]
    fn move_command_shifts() {
        let mut s = store();
        let fid = s.folders[0].id.clone();
        // c3 上移一格
        assert!(s.move_command(&fid, "c3", -1));
        let labels: Vec<_> = s.folders[0]
            .commands
            .iter()
            .map(|c| c.id.as_str())
            .collect();
        assert_eq!(labels, ["c1", "c3", "c2"]);
        // c1 上移越界
        assert!(!s.move_command(&fid, "c1", -1));
        // c3 下移一格
        assert!(s.move_command(&fid, "c3", 1));
        let labels: Vec<_> = s.folders[0]
            .commands
            .iter()
            .map(|c| c.id.as_str())
            .collect();
        assert_eq!(labels, ["c1", "c2", "c3"]);
        // c3 下移越界
        assert!(!s.move_command(&fid, "c3", 1));
    }

    /// v1 到 v3 迁移：旧文件无`schema_version`且命令里有`confirm`字段，必须能读入且不报错。
    #[test]
    fn legacy_v1_with_confirm_migrates() {
        let legacy = r#"{
            "folders": [
                {
                    "id": "f1",
                    "name": "部署",
                    "commands": [
                        { "id": "k1", "label": "build", "script": "make -j4", "confirm": true },
                        { "id": "k2", "label": "rm", "script": "rm -rf /tmp/x", "confirm": false }
                    ]
                }
            ]
        }"#;
        let s: QuickCommandStore = serde_json::from_str(legacy).expect("旧文件必须能反序列化");
        assert_eq!(s.folders.len(), 1);
        assert_eq!(s.folders[0].commands.len(), 2);
        // confirm 键被忽略（结构体已无该字段），命令本身完整
        assert_eq!(s.folders[0].commands[0].label, "build");
        assert_eq!(s.folders[0].commands[0].script, "make -j4");
        // targets 默认空（= 用当前激活 Tab）
        assert!(s.folders[0].commands[0].targets.is_empty());
        // collapsed 默认 false
        assert!(!s.folders[0].collapsed);
        // 无 schema_version 到 走 default（= 3）
        assert_eq!(s.schema_version, SCHEMA_VERSION);
    }

    /// v2 到 v3 迁移：有`schema_version = 2`与`collapsed`，应保留 collapsed、targets 默认空。
    #[test]
    fn legacy_v2_keeps_collapsed() {
        let v2 = r#"{
            "schema_version": 2,
            "folders": [
                {
                    "id": "f1",
                    "name": "诊断",
                    "collapsed": true,
                    "commands": [
                        { "id": "k1", "label": "top", "script": "top -b -n 1" }
                    ]
                }
            ]
        }"#;
        let s: QuickCommandStore = serde_json::from_str(v2).expect("v2 文件必须能反序列化");
        assert!(s.folders[0].collapsed, "collapsed 必须保留");
        assert!(s.folders[0].commands[0].targets.is_empty());
        assert_eq!(s.schema_version, 2, "反序列化保留文件里的版本号");
    }

    /// targets 序列化往返：非空 targets 必须原样保留（目标记忆持久化的核心契约）。
    #[test]
    fn targets_roundtrip() {
        let mut s = store();
        s.set_cmd_targets("c1", vec!["sess-a".into(), "sess-b".into()]);
        let json = serde_json::to_string(&s).unwrap();
        let back: QuickCommandStore = serde_json::from_str(&json).unwrap();
        assert_eq!(
            back.find_cmd("c1").unwrap().targets,
            vec!["sess-a".to_string(), "sess-b".to_string()]
        );
        // 没有 targets 的命令序列化后仍可读回（空 vec 走 default）
        assert!(back.find_cmd("c2").unwrap().targets.is_empty());
    }

    /// 序列化后命令对象只含业务键（无`confirm`），确认`confirm`不会以任何形式回到 JSON 里。
    /// 键集是**白名单**：新增字段必须在此登记，这样任何意外多出的键（比如`confirm`被重新引入）
    /// 都会让这条测试立刻变红。
    #[test]
    fn cmd_serializes_without_confirm() {
        let mut s = QuickCommandStore::default();
        let fid = s.add_folder("A");
        s.add_cmd(&fid, "build", "make");
        let json = serde_json::to_string(&s).unwrap();
        assert!(
            !json.contains("\"confirm\""),
            "confirm 不得出现在 v3 JSON 中"
        );
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let cmd = &v["folders"][0]["commands"][0];
        let obj = cmd.as_object().unwrap();
        let mut keys: Vec<&str> = obj.keys().map(|s| s.as_str()).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["append_newline", "id", "label", "script", "targets"]);
    }

    /// schema_version 必须落盘（否则下次读入仍按 v1 处理）。
    #[test]
    fn schema_version_serialized() {
        let s = QuickCommandStore::default();
        let json = serde_json::to_string(&s).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["schema_version"].as_u64(), Some(SCHEMA_VERSION as u64));
    }

    #[test]
    fn rename_folder_rejects_empty() {
        let mut s = store();
        let fid = s.folders[0].id.clone();
        assert!(s.rename_folder(&fid, "  部署  "));
        assert_eq!(s.folders[0].name, "部署", "首尾空白应被 trim");
        assert!(!s.rename_folder(&fid, "   "), "空名必须拒绝");
        assert_eq!(s.folders[0].name, "部署", "拒绝后原名保留");
        assert!(!s.rename_folder(&fid, "部署"), "无变化返回 false");
        assert!(!s.rename_folder("nope", "x"));
    }

    #[test]
    fn remove_folder_takes_commands_with_it() {
        let mut s = store();
        let fid = s.folders[0].id.clone();
        assert_eq!(s.folders[0].commands.len(), 3);
        assert!(s.remove_folder(&fid));
        assert_eq!(s.folders.len(), 2);
        assert!(s.find_cmd("c1").is_none(), "命令应随分组一并删除");
        assert!(!s.remove_folder(&fid), "重复删除返回 false");
    }

    #[test]
    fn duplicate_cmd_copies_script_with_new_id() {
        let mut s = store();
        s.set_cmd_targets("c1", vec!["sess-a".into()]);
        let new_id = s.duplicate_cmd("c1", " 副本").expect("复制成功");
        assert_ne!(new_id, "c1");
        let c = s.find_cmd(&new_id).unwrap();
        assert_eq!(c.label, "one 副本");
        assert_eq!(c.script, "one");
        assert_eq!(c.targets, vec!["sess-a".to_string()], "目标一并复制");
        // 落在同组末尾
        assert_eq!(s.folders[0].commands.last().unwrap().id, new_id);
        assert!(s.duplicate_cmd("nope", " 副本").is_none());
    }

    #[test]
    fn move_cmd_to_other_folder() {
        let mut s = store();
        let to = s.folders[1].id.clone();
        assert!(s.move_cmd_to("c1", &to));
        assert_eq!(s.folders[0].commands.len(), 2);
        assert_eq!(s.folders[1].commands.len(), 1);
        assert_eq!(s.folders[1].commands[0].id, "c1");
        assert_eq!(s.folder_of_cmd("c1").as_deref(), Some(to.as_str()));
        // 同组移动 = 无操作
        assert!(!s.move_cmd_to("c1", &to));
        // 目标组不存在 = 早失败（命令仍在原处）
        assert!(!s.move_cmd_to("c2", "nope"));
        assert_eq!(s.folders[0].commands.len(), 2);
    }

    #[test]
    fn set_cmd_targets_reports_change() {
        let mut s = store();
        assert!(s.set_cmd_targets("c1", vec!["a".into()]));
        assert!(!s.set_cmd_targets("c1", vec!["a".into()]), "同值无变化");
        assert!(s.set_cmd_targets("c1", vec![]), "清空算变化");
        assert!(!s.set_cmd_targets("nope", vec!["a".into()]));
    }

    #[test]
    fn collapse_toggles() {
        let mut s = store();
        let fid = s.folders[0].id.clone();
        assert!(s.set_folder_collapsed(&fid, true));
        assert!(s.folders[0].collapsed);
        assert!(!s.set_folder_collapsed(&fid, true), "同值无变化");
        assert!(s.set_all_collapsed(false));
        assert!(!s.folders.iter().any(|f| f.collapsed));
        assert!(!s.set_all_collapsed(false), "已是全展开无变化");
        assert!(!s.set_folder_collapsed("nope", true));
    }

    #[test]
    fn add_cmd_appends() {
        let mut s = QuickCommandStore::default();
        let fid = s.add_folder("A");
        let id = s.add_cmd(&fid, "build", "make -j4").unwrap();
        assert_eq!(s.folders[0].commands.len(), 1);
        assert_eq!(s.find_cmd(&id).unwrap().script, "make -j4");
        assert!(s.add_cmd("nope", "x", "y").is_none());
    }

    #[test]
    fn update_cmd_keeps_targets() {
        let mut s = store();
        s.set_cmd_targets("c1", vec!["sess-a".into()]);
        assert!(s.update_cmd("c1", "renamed", "echo hi", true));
        let c = s.find_cmd("c1").unwrap();
        assert_eq!(c.label, "renamed");
        assert_eq!(c.script, "echo hi");
        assert_eq!(c.targets, vec!["sess-a".to_string()], "编辑不得清掉目标");
        assert!(!s.update_cmd("nope", "a", "b", true));
    }

    /// `update_cmd`必须把末尾回车开关一并写入
    #[test]
    fn update_cmd_writes_append_newline() {
        let mut s = store();
        assert!(
            s.find_cmd("c1").unwrap().append_newline,
            "新建命令默认末尾回车"
        );
        assert!(s.update_cmd("c1", "one", "one", false));
        assert!(!s.find_cmd("c1").unwrap().append_newline);
    }

    /// 旧文件无`append_newline`键必须默认`true`（保持「写完即执行」），漏掉默认值会让老命令静默变「只键入不执行」。
    #[test]
    fn legacy_cmd_gains_append_newline_true() {
        let json = r#"{
            "schema_version": 3,
            "folders": [{
                "id": "f1", "name": "A", "collapsed": false,
                "commands": [{ "id": "c1", "label": "one", "script": "echo 1", "targets": [] }]
            }]
        }"#;
        let s: QuickCommandStore = serde_json::from_str(json).unwrap();
        assert!(
            s.folders[0].commands[0].append_newline,
            "旧文件缺 append_newline 时必须默认 true"
        );
    }

    /// 新字段必须真的落盘（否则下次读入仍按默认值处理）
    #[test]
    fn append_newline_serialized() {
        let mut s = store();
        s.update_cmd("c1", "one", "one", false);
        let v: serde_json::Value = serde_json::to_value(&s).unwrap();
        assert_eq!(
            v["folders"][0]["commands"][0]["append_newline"].as_bool(),
            Some(false)
        );
    }
}
