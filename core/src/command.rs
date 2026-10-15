//! 命令源：内置命令 + 命令窗口的「快速连接」。
//!
//! 快捷命令**不在此列**：它有自己的入口（底部快捷命令栏），命令窗口只列
//! 内置命令与连接。两者混在一张列表里时，用户搜「make」既可能是想跑快捷
//! 命令、也可能是想找那台叫 make 的机器，而快捷键语义完全不同。

use crate::connections::ConnectionStore;

/// 平台修饰键提示（macOS Cmd / Win-Linux Ctrl）。
pub fn mod_hint() -> &'static str {
    if cfg!(target_os = "macos") {
        "⌘"
    } else {
        "Ctrl"
    }
}

/// 命令来源
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandOrigin {
    /// 内置命令（绑定 Action，不可删除）
    Builtin,
    /// AI 直通（命令面板输入`ai <描述>`生成，text 为自然语言描述）
    AiPassthrough { text: String },
    /// 快速连接（命令面板列出的连接配置，回车直接连接）
    ConnectProfile { profile_id: String },
}

/// 统一命令条目
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandEntry {
    /// 展示名（中文原文；内置命令经`display_label()`走 i18n）
    pub label: String,
    /// 内置命令的 i18n key（与 label 同值；AI/连接条目为 None）
    pub i18n_key: Option<&'static str>,
    /// 默认快捷键的平台无关描述（"mod+T"，展示时经`shortcut_label()`平台化）
    pub default_shortcut: Option<&'static str>,
    /// 内置命令对应的 Action id（AI/连接条目为 None）
    pub action_id: Option<&'static str>,
    /// 标签之外的额外搜索词。
    ///
    /// 命令窗口的过滤是「标签或任一搜索词包含查询串」：连接条目只靠
    /// `label`（连接名）匹配，会让「按 IP 找那台机器」这种最自然的用法失灵。
    pub keywords: Vec<String>,
    /// 行内次要信息：连接条目为 `用户名@主机:端口`（2026-09-17 用户要求
    /// 「快速连接中的连接要显示 ip 端口名称组名」）。`None` 表示该条目只有标签。
    pub detail: Option<String>,
    /// 所属分组名（`None` = 未分组）。同样只供连接条目展示。
    pub group: Option<String>,
    pub origin: CommandOrigin,
}

impl CommandEntry {
    /// 展示名（内置命令在 En 模式经 i18n 翻译）
    pub fn display_label(&self) -> String {
        match self.i18n_key {
            Some(k) => crate::i18n::t(k).to_string(),
            None => self.label.clone(),
        }
    }

    /// 平台化快捷键提示（`mod+T`到`CmdT`/`Ctrl+T`）
    pub fn shortcut_label(&self) -> Option<String> {
        self.default_shortcut
            .map(|spec| spec.replace("mod", mod_hint()))
    }
}

/// 命令源提供者（后续插件体系可按同签名扩展）
pub trait CommandProvider {
    fn commands(&self) -> Vec<CommandEntry>;
}

/// 内置命令清单（**应用级**命令）。
///
/// 2026-09-17 运行反馈「简化命令面板，tab 右键功能不再放入命令面板中」：标签与分屏的
/// 操作各有专属入口（标签右键菜单 / 终端右键菜单 / 快捷键），不再在命令面板里列第二遍 ——
/// 面板只留**没有别处可去**的应用级命令。
/// 判据落在测试 `builtin_commands_carry_no_tab_or_split_operations` 上：新加条目时，
/// 只要它同时也出现在 `tab_bar::tab_context_menu` 里，那个用例就会红。
///
/// 同日第二轮（「命令窗口新增【新增连接】【AI助手】，移除命令面板命令」）：
/// * 新增连接 / AI 助手 排在最前 —— 启动不再自动开本地 shell，命令窗口就是入口，
///   头两条要是用户最可能想做的事；
/// * 删掉「命令面板」自身那一条（在面板里列一条「打开面板」没有意义）。
///
/// 2026-09-20 运行反馈「命令面板，新增连接下面增加新增本地 Shell 命令，移除终端内搜索命令」：
/// * 「新增本地 Shell」紧随「新建连接」—— 这两条是启动后最可能想做的事（连远端 / 开本地），
///   顺序本身就是优先级（动作 id `NewLocalShell` 复用 `NewTab` 处理器，见 `keybind_actions`）；
/// * 「终端内搜索」从面板撤下：它是**终端内**的动作，Ctrl+F 与终端右键都能到，面板里
///   再列一遍属于重复入口（与 2026-09-17 撤标签 / 分屏动作同一条理由）。
///   注意它仍在 [`action_label`] 全量表与 `platform::keymap::DEFAULT_BINDINGS` 里 ——
///   设置 → 键位仍要列出并允许重绑，撤掉的只是面板里那一条。
pub fn builtin_commands() -> Vec<CommandEntry> {
    vec![
        entry("新增连接", None, "NewConnection"),
        entry("新增本地 Shell", None, "NewLocalShell"),
        entry("AI助手", None, "AiAssistant"),
        entry("切换侧栏", Some("mod+B"), "ToggleSidebar"),
        entry("打开设置", Some("mod+,"), "OpenSettings"),
        // 2026-09-29「取消恢复上次会话功能」：启动不再自动弹「恢复上次会话？」框，
        // 恢复改由这一条**手动触发** —— 所以它必须进面板，否则功能就没入口了。
        entry("恢复上次会话", None, "RestoreLastSession"),
    ]
}

/// 动作 id → 展示名（**全量**表，含不再进命令面板的标签 / 分屏动作）。
///
/// 2026-09-17 命令面板简化后，「面板里列什么」与「键位表里能绑什么」是两张表：
/// 面板 = [`builtin_commands`]（只留应用级命令），键位表（设置 → 键位）仍要列出**全部**
/// 可绑定动作并显示中文名 —— 所以标签查这里，而不是查面板清单。
/// 曾经两件事共用 `builtin_commands()`：从面板删掉一个条目，键位表那一行的中文名
/// 会悄悄退化成动作 id（`NewTab`），护栏用例 `snapshot_rows_match_defaults` 当场报红。
pub fn action_label(action_id: &str) -> Option<&'static str> {
    Some(match action_id {
        "NewTab" => "新建本地终端",
        "CloseTab" => "关闭当前标签",
        "TabNext" => "下一个标签",
        "TabPrev" => "上一个标签",
        "ToggleSidebar" => "切换侧栏",
        "OpenSettings" => "打开设置",
        "CommandPalette" => "命令面板",
        "SearchInTerminal" => "终端内搜索",
        "SplitRight" => "向右拆分",
        "SplitDown" => "向下拆分",
        // 命令窗口专属的三个动作（2026-09-17 起）：**不进键位表**（`DEFAULT_BINDINGS`
        // 里没有它们 —— 面板条目没有独立快捷键，`NewLocalShell` 复用 `NewTab` 的 Ctrl+T），
        // 但要有全量中文名，免得面板与全量表两张表的护栏（见 tests）对不上。
        "NewConnection" => "新增连接",
        // 2026-09-20 面板新增：面板里叫「新增本地 Shell」，与键位表里 NewTab 的
        // 「新建本地终端」是**同一个动作的两处入口**，各自用各自语境下最准的词。
        "NewLocalShell" => "新增本地 Shell",
        "AiAssistant" => "AI助手",
        // 2026-09-29：手动恢复上次会话（启动不再自动弹框）。
        "RestoreLastSession" => "恢复上次会话",
        _ => return None,
    })
}

fn entry(
    label: &'static str,
    shortcut: Option<&'static str>,
    action_id: &'static str,
) -> CommandEntry {
    CommandEntry {
        label: label.to_string(),
        i18n_key: Some(label),
        default_shortcut: shortcut,
        action_id: Some(action_id),
        keywords: Vec::new(),
        detail: None,
        group: None,
        origin: CommandOrigin::Builtin,
    }
}

/// 快速连接：把连接配置摊成命令条目，**按最近访问倒序**（FinalShell 手感）。
///
/// 从未连接过的条目排在最后并按名称升序 —— 排序必须完全确定，否则每次
/// 打开命令窗口列表顺序都在跳，用户记不住「第三个是那台机器」。
///
/// 每条同时带上三类可读信息（2026-09-17 用户要求）：
/// `label` = 连接名、`detail` = `用户名@主机:端口`、`group` = 分组名；
/// 搜索词再补上主机名 / 用户名 / 端口 / 分组名，方便按 IP 或账号找连接。
pub fn quick_connect(store: &ConnectionStore) -> Vec<CommandEntry> {
    store
        .by_recent_use()
        .into_iter()
        .map(|p| {
            // 分组 id 已失效时按「未分组」处理，不留一个空壳分组名。
            let group = p
                .group_id
                .as_deref()
                .and_then(|id| store.group_name(id))
                .filter(|name| !name.is_empty())
                .map(str::to_string);
            let port = p.port.to_string();

            let mut keywords = vec![p.host.clone(), p.username.clone(), port.clone()];
            if let Some(g) = &group {
                keywords.push(g.clone());
            }

            CommandEntry {
                label: p.name.clone(),
                i18n_key: None,
                default_shortcut: None,
                action_id: None,
                keywords,
                detail: Some(p.summary_line()),
                group,
                origin: CommandOrigin::ConnectProfile {
                    profile_id: p.id.clone(),
                },
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connections::ConnectionProfile;

    #[test]
    fn builtin_has_stable_labels() {
        let cmds = builtin_commands();
        // 2026-09-29：+1 条「恢复上次会话」（启动不再自动弹框后的手动入口）。
        assert_eq!(cmds.len(), 6);
        // 存储为平台无关描述，展示时平台化
        assert!(cmds
            .iter()
            .any(|c| c.label == "打开设置" && c.default_shortcut == Some("mod+,")));
        // 2026-09-20：面板里不再有「终端内搜索」——它只留在 Ctrl+F 与终端右键菜单。
        assert!(
            !cmds.iter().any(|c| c.label == "终端内搜索"),
            "「终端内搜索」已从命令面板撤下，它不该再回到面板清单里"
        );
        // 2026-09-17：启动不再自动开本地 shell，命令窗口就是入口 ——
        // 「新增连接」「新增本地 Shell」必须在，且排在最前（用户最先看到的入口）。
        assert_eq!(
            cmds[0].label, "新增连接",
            "命令窗口第一条必须是「新增连接」（启动入口）"
        );
        assert_eq!(
            cmds[1].label, "新增本地 Shell",
            "命令窗口第二条必须是「新增本地 Shell」（2026-09-20 要求紧跟在新增连接下面）"
        );
        assert_eq!(cmds[2].label, "AI助手");
        assert!(
            !cmds.iter().any(|c| c.label == "命令面板"),
            "面板里不该再列一条「命令面板」——在面板里打开面板没有意义"
        );
        assert!(cmds
            .iter()
            .all(|c| c.origin == CommandOrigin::Builtin && c.action_id.is_some()));
        // 展示串在非 macOS 下应为 Ctrl 前缀（CI 为 windows/linux）
        if !cfg!(target_os = "macos") {
            let sidebar = cmds.iter().find(|c| c.label == "切换侧栏").unwrap();
            assert_eq!(sidebar.shortcut_label().as_deref(), Some("Ctrl+B"));
        }
    }

    /// 回归（2026-09-17 运行反馈「简化命令面板，tab 右键功能不再放入命令面板中」）：
    /// 标签 / 分屏操作只在各自的专属入口（标签右键菜单、终端右键菜单、快捷键）里出现，
    /// 命令面板不再列第二遍。
    /// 判据是**同时出现在标签右键菜单里**的那批动作名：面板里一旦又冒出来，这里立刻红。
    /// 想加回某个动作，必须先想清楚「它是不是已经有专属入口」。
    #[test]
    fn builtin_commands_carry_no_tab_or_split_operations() {
        let banned = [
            "新建本地终端",
            "关闭当前标签",
            "下一个标签",
            "上一个标签",
            "向右拆分",
            "向下拆分",
        ];
        for c in builtin_commands() {
            assert!(
                !banned.contains(&c.label.as_str()),
                "命令面板里又出现了标签/分屏操作「{}」—— 用户要求这些只留在右键菜单与快捷键里。",
                c.label
            );
        }
        // 反向：面板该有的应用级命令一个都不能少（别把它们一起删掉）
        // 2026-09-17 第二轮起：新增连接 / AI助手 进面板，命令面板自己那条撤掉。
        let cmds = builtin_commands();
        let labels: Vec<&str> = cmds.iter().map(|c| c.label.as_str()).collect();
        for kept in [
            "新增连接",
            "新增本地 Shell",
            "AI助手",
            "切换侧栏",
            "打开设置",
        ] {
            assert!(labels.contains(&kept), "命令面板丢了应用级命令「{kept}」");
        }
        assert!(
            !labels.contains(&"终端内搜索"),
            "「终端内搜索」不该再出现在命令面板里（2026-09-20 用户要求移除）——\n\
             它是**终端内**的动作，Ctrl+F 与终端右键菜单才是它的入口；\n\
             键位表仍要列出它（`action_label` + `DEFAULT_BINDINGS`），别顺手把它删了。"
        );
        assert!(
            !labels.contains(&"命令面板"),
            "命令面板里不该再列「命令面板」自己（在面板里打开面板没有意义）"
        );
    }

    /// 面板动作与全量动作表必须一致：面板里每个动作，键位表（`action_label`）
    /// 都得叫得出同一个中文名。
    ///
    /// 这两张表 2026-09-17 起就是分开的（面板精简、键位表全量），分开之后最容易出的
    /// 错就是「面板加了新动作、键位表那一行退化成动作 id」——正是本次改动真实踩到的
    /// （`snapshot_rows_match_defaults` 报红）。这里把它挡在 core 侧。
    #[test]
    fn every_palette_action_has_the_same_label_in_the_full_table() {
        for c in builtin_commands() {
            let id = c.action_id.expect("内置条目必须带动作 id");
            assert_eq!(
                action_label(id),
                Some(c.label.as_str()),
                "动作 {id} 在面板里叫「{}」，全量动作表里却对不上",
                c.label
            );
        }
    }

    /// 命令窗口不再消费快捷命令：内置清单里不得出现任何快捷命令来源。
    #[test]
    fn builtin_commands_carry_no_quick_command_origin() {
        // 关键词只属于连接条目；内置命令的搜索面就是标签本身。
        assert!(builtin_commands().iter().all(|c| c.keywords.is_empty()));
    }

    fn store_with(entries: &[(&str, &str, &str, u64)]) -> ConnectionStore {
        let mut store = ConnectionStore::default();
        for (name, host, user, used) in entries {
            let mut p = ConnectionProfile::new(*name, *host, *user);
            p.last_used_at = *used;
            store.add_profile(p);
        }
        store
    }

    #[test]
    fn quick_connect_is_ordered_by_recent_use() {
        let store = store_with(&[
            ("old", "10.0.0.1", "root", 100),
            ("fresh", "10.0.0.2", "root", 900),
            ("never", "10.0.0.3", "root", 0),
        ]);
        let entries = quick_connect(&store);
        let labels: Vec<&str> = entries.iter().map(|e| e.label.as_str()).collect();
        assert_eq!(labels, ["fresh", "old", "never"]);

        // 从未连接过的排最后，且与「最近用过」的区分得出来
        assert!(matches!(
            entries[0].origin,
            CommandOrigin::ConnectProfile { .. }
        ));
        assert!(entries.iter().all(|e| e.action_id.is_none()));
    }

    /// 同一时刻（例如全部都是 0）必须退化成确定顺序：按名称升序。
    #[test]
    fn quick_connect_is_deterministic_for_equal_timestamps() {
        let store = store_with(&[
            ("zeta", "10.0.0.1", "root", 0),
            ("alpha", "10.0.0.2", "root", 0),
        ]);
        let first: Vec<String> = quick_connect(&store)
            .iter()
            .map(|e| e.label.clone())
            .collect();
        assert_eq!(first, ["alpha", "zeta"]);
        // 反复调用顺序不变（列表不会在两次打开之间跳动）
        assert_eq!(
            first,
            quick_connect(&store)
                .iter()
                .map(|e| e.label.clone())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn quick_connect_entries_are_searchable_by_host_and_user() {
        let store = store_with(&[("prod-01", "10.1.2.3", "deploy", 7)]);
        let entries = quick_connect(&store);
        // 主机 / 用户名 / 端口都在搜索词里（2026-09-17 起补上端口）
        assert_eq!(entries[0].keywords, vec!["10.1.2.3", "deploy", "22"]);
        assert_eq!(
            entries[0].origin,
            CommandOrigin::ConnectProfile {
                profile_id: store.profiles[0].id.clone()
            }
        );
    }

    /// 2026-09-17 用户要求：快速连接的条目要能一眼看到
    /// **名称 + ip:端口 + 用户名 + 组名**，而不是只有一个连接名。
    #[test]
    fn quick_connect_carries_name_host_port_and_group() {
        let mut store = ConnectionStore::default();
        let group_id = store.add_group_after("线上".to_string(), None);
        let mut p = ConnectionProfile::new("prod-01", "10.1.2.3", "deploy");
        p.port = 2222;
        p.group_id = Some(group_id.clone());
        store.add_profile(p);
        // 未分组的条目不该凭空长出一个组名
        store.add_profile(ConnectionProfile::new("box", "10.0.0.9", "root"));

        let entries = quick_connect(&store);
        let named = |name: &str| entries.iter().find(|e| e.label == name).unwrap();

        let grouped = named("prod-01");
        assert_eq!(grouped.detail.as_deref(), Some("deploy@10.1.2.3:2222"));
        assert_eq!(grouped.group.as_deref(), Some("线上"));
        assert!(
            grouped.keywords.contains(&"2222".to_string())
                && grouped.keywords.contains(&"线上".to_string()),
            "分组名与端口都要能被搜到：{:?}",
            grouped.keywords
        );

        let ungrouped = named("box");
        assert_eq!(ungrouped.detail.as_deref(), Some("root@10.0.0.9:22"));
        assert_eq!(ungrouped.group, None);
    }

    /// 内置命令没有连接信息，两个展示字段必须留空（否则 AI 直通 / 内置条目
    /// 会被渲染成连接行）。
    #[test]
    fn builtin_entries_have_no_connection_detail() {
        assert!(builtin_commands()
            .iter()
            .all(|c| c.detail.is_none() && c.group.is_none()));
    }

    #[test]
    fn quick_connect_on_empty_store_is_empty() {
        assert!(quick_connect(&ConnectionStore::default()).is_empty());
    }
}
