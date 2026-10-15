//! 连接管理：树形分组 + 连接配置 + JSON 持久化。

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionProfile {
    pub id: String,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    /// 认证方式：密码（不持久化）或私钥路径
    #[serde(default, skip_serializing)]
    pub password: Option<String>,
    /// 记住的密码（用户显式勾选后持久化），以「引用」形式存储，不落明文。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stored_password: Option<String>,
    #[serde(default)]
    pub key_path: Option<String>,
    /// 备注：自由文本，可多行（用途、跳板机、到期日……）。
    ///
    /// 纯展示信息，**不参与**任何认证或连接参数：改它不会影响已打开的会话。
    /// `skip_serializing_if` 让绝大多数没有备注的连接不往 `connections.json`
    /// 里塞一个空串；`default` 让加字段之前落盘的老库照常读得进来。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub notes: String,
    /// 所属分组 **id**（`None` = 未分组）。落盘键为 `group_id`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_id: Option<String>,
    /// 旧格式（schema v1）的 `group` 字段存的是分组**名**。
    /// 只在加载期由 `ConnectionStore::normalize()` 迁移进 `group_id`，之后不再写盘。
    #[serde(default, rename = "group", skip_serializing)]
    pub legacy_group: Option<String>,
    /// 最近一次发起连接的时刻（Unix 秒；`0` = 从未连接过）。
    ///
    /// 唯一的消费者是命令窗口的「快速连接」列表 —— 按这个字段倒序，
    /// 最近用过的排最前（与 FinalShell 的快速连接同一个手感）。
    /// 它**不参与导出**：导出走独立的 `ExportedProfile` DTO，
    /// 这台机器上的使用习惯不该跟着连接配置传到别人那里。
    #[serde(default)]
    pub last_used_at: u64,
    /// 连接标签（如 `prod` / `db` / `edge`）：agent 与定时任务的目标筛选依据。
    ///
    /// 与分组的区别是**语义**：分组是树形归属（一台机器在一个分组下），
    /// 标签是横切筛选（一台机器可以同时是 prod 和 db）。两者都要有，
    /// 因为运维提问时两种说法都会出现（「生产组里的机器」/「所有数据库机器」）。
    ///
    /// 与会话、认证无关：改标签不会影响任何已打开的会话。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// 端口转发规则（F-3）：连接建立后按表**自动启动**，会话断开随之清理。
    ///
    /// 与「隧道」的区别是**时刻**：这里存的是**意图**（用户想开哪些隧道），
    /// 而活动隧道的实时状态在运行期的 [`crate::forward::ForwardSlot`] 里。
    /// 所以本字段可以在会话未连接时照常编辑 —— 规则表是配置，不是状态。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub forwards: Vec<ForwardRule>,
    /// 会话日志（F-7）的连接级覆盖：`None` = 跟随全局设置
    /// （`Settings::session_log_enabled`），`Some(v)` = 这台机器强制开 / 强制关。
    ///
    /// 用 `Option<bool>` 而不是照抄一份全局值：抄下来的那一刻就把全局设置
    /// **冻进了配置** —— 之后改全局，老连接会一直保持旧行为，而用户以为改的是
    /// 全局开关。三态（跟全局 / 开 / 关）才能表达「这条连接例外」，
    /// 这正是 F-15（连接级编码）被复核为缺口时的那条教训。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_log: Option<bool>,
    /// 代理（F-10）的连接级口径：跟随全局 / 明确直连 / 本条连接自带一套。
    ///
    /// 默认（也是老配置读进来后的取值）是 `FollowGlobal`，故落盘时**不写** ——
    /// 绝大多数连接都跟随全局，写出来只是噪声（判定见
    /// [`crate::proxy::ProxyChoice::is_follow_global`]）。
    ///
    /// 三态合成的唯一落点是 `ssh::connect_only`，配置里**不保存合成结果**：
    /// 把全局值抄进来的那一刻就把它冻住了，之后改全局，老连接会一直保持旧行为。
    #[serde(
        default,
        skip_serializing_if = "crate::proxy::ProxyChoice::is_follow_global"
    )]
    pub proxy: crate::proxy::ProxyChoice,
    /// 断线自动重连（F-4）的连接级覆盖：`None` = 跟随全局设置
    /// （`Settings::auto_reconnect`），`Some(v)` = 这台机器强制重连 / 绝不重连。
    ///
    /// 与 `session_log` 同一条纪律（三态随连接走）：生产机器上「别自动连」
    /// 是**连接属性**，而「重试几次、等多久」是本机网络条件的产物 —— 后者
    /// 留在全局设置里（`Settings::reconnect_max_retries` / `reconnect_interval_secs`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_reconnect: Option<bool>,
}

/// 转发类型。落盘用稳定的 snake_case 串，**不用** `Debug`/序号：
/// 换实现（比如以后加 `UnixSocket`）不能让老配置读不出来。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForwardKind {
    /// 本地转发（`ssh -L`）：在**本机**监听，连进来的流量经服务器转到目标。
    Local,
    /// 远端转发（`ssh -R`）：在**服务器**上监听，连进来的流量经本机转到目标。
    Remote,
    /// 动态转发（`ssh -D`）：在本机起一个 SOCKS5，目标由客户端在握手时才说。
    Dynamic,
}

impl ForwardKind {
    /// 稳定 ASCII 键：落盘 / 几何护栏 / 设置项都用它，**不要**用 i18n 标签。
    pub fn key(self) -> &'static str {
        match self {
            ForwardKind::Local => "local",
            ForwardKind::Remote => "remote",
            ForwardKind::Dynamic => "dynamic",
        }
    }

    /// 该类型是否需要「目标」两段（Dynamic 由客户端在握手时提供，故不需要）。
    pub fn needs_target(self) -> bool {
        !matches!(self, ForwardKind::Dynamic)
    }

    pub const ALL: [ForwardKind; 3] = [
        ForwardKind::Local,
        ForwardKind::Remote,
        ForwardKind::Dynamic,
    ];
}

/// 单条端口转发规则。
///
/// 字段用**具名**而非规格初稿里的元组（`listen: (String,u16)`）：落盘 JSON 里
/// `["127.0.0.1",8080]` 这种数组位置语义靠约定，加第三个维度（比如绑定设备名）
/// 就是破坏性变更；具名对象能靠 `#[serde(default)]` 平滑加字段。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForwardRule {
    /// 规则 id：UI 行内定位（增删改）与「哪条失败」的报账依据。
    #[serde(default)]
    pub id: String,
    pub kind: ForwardKind,
    /// 监听地址。本地/动态是**本机**地址；远端是**服务器**上的地址。
    #[serde(default = "default_listen_host")]
    pub listen_host: String,
    pub listen_port: u16,
    /// 目标地址。`Local` 由服务器去连；`Remote` 由本机去连；`Dynamic` 恒为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_port: Option<u16>,
}

/// 监听地址缺省值：**回环**而非 `0.0.0.0`。
///
/// 与 `ssh -L` 的取舍不同：命令行用户敲下 `-L 8080:db:5432` 时就知道自己在把
/// 端口暴露到哪；桌面应用的规则表是「点几下就建好」的，默认全网卡监听会让一台
/// 连着咖啡店 Wi-Fi 的笔记本凭空开出一个无认证的入口。要暴露得显式改地址。
pub fn default_listen_host() -> String {
    "127.0.0.1".to_string()
}

impl ForwardRule {
    pub fn new(kind: ForwardKind) -> Self {
        Self {
            id: uuid_like(),
            kind,
            listen_host: default_listen_host(),
            listen_port: 0,
            target_host: None,
            target_port: None,
        }
    }

    /// 校验：返回 `None` 表示可启动，`Some(原因)` 为**面向用户**的不可用原因。
    ///
    /// 校验放 core 而不是对话框：手改 `connections.json`、导入配置两条路径都
    /// 绕不过对话框，规则合法性必须在数据层收口（与 `normalize_tags` 同理）。
    pub fn validate(&self) -> Option<ForwardValidateError> {
        if self.listen_host.trim().is_empty() {
            return Some(ForwardValidateError::NoListenHost);
        }
        if self.listen_port == 0 {
            return Some(ForwardValidateError::NoListenPort);
        }
        if self.kind.needs_target() {
            match (self.target_host.as_deref(), self.target_port) {
                (Some(h), Some(p)) if !h.trim().is_empty() && p != 0 => {}
                _ => return Some(ForwardValidateError::NoTarget),
            }
        }
        None
    }

    /// 归一化：去空白、补 `Dynamic` 的 `None` 目标、补空 id。
    pub fn normalize(&mut self) -> bool {
        let before = self.clone();
        self.listen_host = self.listen_host.trim().to_string();
        if self.listen_host.is_empty() {
            self.listen_host = default_listen_host();
        }
        self.target_host = self
            .target_host
            .as_deref()
            .map(str::trim)
            .filter(|h| !h.is_empty())
            .map(str::to_string);
        if let Some(p) = self.target_port {
            if p == 0 {
                self.target_port = None;
            }
        }
        if !self.kind.needs_target() {
            // SOCKS5 的目标由客户端每连接指定，留着一个空目标只会让校验/序列化多两个分支。
            self.target_host = None;
            self.target_port = None;
        }
        if self.id.trim().is_empty() {
            self.id = uuid_like();
        }
        *self != before
    }

    /// 「127.0.0.1:8080 → db.internal:5432」这种一行摘要（日志与 tooltip 用）。
    pub fn summary(&self) -> String {
        match (&self.target_host, self.target_port) {
            (Some(h), Some(p)) if self.kind.needs_target() => {
                format!("{}:{} -> {}:{}", self.listen_host, self.listen_port, h, p)
            }
            _ => format!("{}:{} (SOCKS5)", self.listen_host, self.listen_port),
        }
    }
}

/// 规则不可用的原因（结构化：UI 自己决定文案，core 不产出中文）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForwardValidateError {
    NoListenHost,
    NoListenPort,
    NoTarget,
}

/// 单个连接的标签数量上限。
pub const MAX_TAGS_PER_PROFILE: usize = 8;
/// 单个标签的长度上限（字符）。
pub const MAX_TAG_CHARS: usize = 16;

impl ConnectionProfile {
    /// 归一化标签：去空白、去空串、去重，并截断到上限。返回是否发生了变化。
    ///
    /// 做在 core 而不是对话框里：导入配置、手改 `connections.json` 这两条
    /// 路径都绕不过对话框，校验必须落在数据层才有意义。
    pub fn normalize_tags(&mut self) -> bool {
        let before = self.tags.clone();
        let mut seen = std::collections::HashSet::new();
        let mut out: Vec<String> = Vec::new();
        for raw in &self.tags {
            let tag = raw.trim();
            if tag.is_empty() {
                continue;
            }
            let tag: String = tag.chars().take(MAX_TAG_CHARS).collect();
            if seen.insert(tag.clone()) {
                out.push(tag);
            }
            if out.len() >= MAX_TAGS_PER_PROFILE {
                break;
            }
        }
        self.tags = out;
        self.tags != before
    }

    /// 是否带某个标签（不区分大小写）。
    pub fn has_tag(&self, tag: &str) -> bool {
        let want = tag.trim();
        self.tags.iter().any(|t| t.eq_ignore_ascii_case(want))
    }
}

impl ConnectionProfile {
    pub fn new(
        name: impl Into<String>,
        host: impl Into<String>,
        username: impl Into<String>,
    ) -> Self {
        Self {
            id: uuid_like(),
            name: name.into(),
            host: host.into(),
            port: 22,
            username: username.into(),
            password: None,
            stored_password: None,
            key_path: None,
            notes: String::new(),
            group_id: None,
            legacy_group: None,
            last_used_at: 0,
            tags: Vec::new(),
            forwards: Vec::new(),
            session_log: None,
            proxy: crate::proxy::ProxyChoice::default(),
            auto_reconnect: None,
        }
    }

    /// 复制服务器信息一行
    pub fn summary_line(&self) -> String {
        format!("{}@{}:{}", self.username, self.host, self.port)
    }
}

/// 复制连接时的名称后缀（名称 +`copy`，其余参数不变）
pub const COPY_SUFFIX: &str = "copy";

/// 连接分组。
///
/// `id` 与 `name` 分离：`name` 是给人看的、可重名也可能被改；一切引用
/// （连接归属、树节点 id、展开状态）都按 `id` 走 —— 改名不会丢归属、不会把分组收起来。
/// 嵌套（2026-09-17「连接组里还有组」）：`parent` 指向父分组 id，`None` 为根级；
/// 环与悬空引用由 `normalize` 收口，渲染侧另有只读兜底（见 `effective_parent`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionGroup {
    pub id: String,
    #[serde(default)]
    pub name: String,
    /// 父分组 id（`None` = 根级分组）
    #[serde(default)]
    pub parent: Option<String>,
}

impl ConnectionGroup {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            id: uuid_like(),
            name: name.into(),
            parent: None,
        }
    }
}

/// `groups` 的两种落盘形态：新格式 `[{"id":..,"name":..}]`、旧格式 `["名"]`。
///
/// 旧格式在反序列化时即时补一个 id；同一次加载里连接侧的旧 `group`（名）
/// 会按名字解析到同一批 id，所以内部引用始终自洽。
#[derive(Deserialize)]
#[serde(untagged)]
enum GroupRepr {
    Full(ConnectionGroup),
    Name(String),
}

fn de_groups<'de, D>(deserializer: D) -> Result<Vec<ConnectionGroup>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw: Vec<GroupRepr> = Vec::deserialize(deserializer)?;
    Ok(raw
        .into_iter()
        .map(|g| match g {
            GroupRepr::Full(g) => g,
            GroupRepr::Name(name) => ConnectionGroup {
                id: uuid_like(),
                name,
                parent: None,
            },
        })
        .collect())
}

/// 连接库 schema 版本：v2 起分组带稳定 id、连接按 `group_id` 归属。
pub const CONNECTIONS_SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ConnectionStore {
    pub profiles: Vec<ConnectionProfile>,
    /// 分组列表（嵌套树形：`parent` 指向父分组，`None` 为根级）。兼容读旧格式的名字数组。
    #[serde(default, deserialize_with = "de_groups")]
    pub groups: Vec<ConnectionGroup>,
    /// 已落盘的 schema 版本（加载时抬到 `CONNECTIONS_SCHEMA_VERSION`）
    #[serde(default)]
    pub schema_version: u32,
}

impl ConnectionStore {
    pub fn load(path: &PathBuf) -> Self {
        match std::fs::read_to_string(path) {
            Ok(s) => match serde_json::from_str::<Self>(&s) {
                Ok(mut v) => {
                    // 旧格式的分组名/连接归属统一收敛成 id。只改内存，
                    // 等下次 save 才落盘（用户没保存前不就地改写磁盘文件）。
                    v.normalize();
                    v.schema_version = CONNECTIONS_SCHEMA_VERSION;
                    v
                }
                Err(_) => {
                    // 解析失败先留底坏文件，再回退默认
                    crate::configio::backup_broken_config(path, &s);
                    Self::with_seed()
                }
            },
            Err(_) => Self::with_seed(),
        }
    }

    pub fn save(&self, path: &PathBuf) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let json = serde_json::to_string_pretty(self)?;
        // 原子写入：先写临时文件再替换
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// 首次启动种子数据
    fn with_seed() -> Self {
        Self {
            profiles: vec![],
            groups: vec![
                ConnectionGroup::new("生产组"),
                ConnectionGroup::new("开发组"),
            ],
            schema_version: CONNECTIONS_SCHEMA_VERSION,
        }
    }

    /// 追加一条连接。`group_id` 指向不存在的分组时降级为「未分组」，
    /// 绝不留下悬空引用（否则这条连接在树上既不在任何分组、也不在未分区分组里）。
    pub fn add_profile(&mut self, mut p: ConnectionProfile) {
        if let Some(gid) = p.group_id.clone() {
            if !self.groups.iter().any(|g| g.id == gid) {
                p.group_id = None;
            }
        }
        self.profiles.push(p);
    }

    pub fn remove_profile(&mut self, id: &str) {
        self.profiles.retain(|p| p.id != id);
    }

    /// 重命名连接（只改展示名，`id` 与分组归属不变）。空名视为无效并返回 `false`。
    ///
    /// 只动 `name` 是关键：Tab、指标来源、SFTP 会话都按 `id` 关联，改名不会断开任何引用。
    pub fn rename_profile(&mut self, id: &str, name: &str) -> bool {
        let name = name.trim();
        if name.is_empty() {
            return false;
        }
        let Some(p) = self.profiles.iter_mut().find(|p| p.id == id) else {
            return false;
        };
        p.name = name.to_string();
        true
    }

    /// 复制连接：名称追加`COPY_SUFFIX`，其余字段全部原样保留
    pub fn duplicate_profile(&mut self, id: &str) -> Option<String> {
        let idx = self.profiles.iter().position(|p| p.id == id)?;
        let mut copy = self.profiles[idx].clone();
        // 新 id 必须重新生成：直接复用会让引用（Tab 的 profile_id）指向同一条记录
        copy.id = uuid_like();
        copy.name = format!("{}{}", copy.name, COPY_SUFFIX);
        // 副本还没被访问过：沿用原条目的 `last_used_at` 会让它在快速连接里
        // 排在「真的刚用过」的那条前面，属于凭空捏造的使用记录。
        copy.last_used_at = 0;
        let new_id = copy.id.clone();
        self.profiles.insert(idx + 1, copy);
        Some(new_id)
    }

    /// 记下「刚刚发起过一次连接」：命令窗口的快速连接按它倒序。
    ///
    /// 只改内存，由调用方决定何时落盘（与其余编辑动作一致）。
    /// 未知 id 返回 false —— 调用方据此判断这条连接是不是已被删掉。
    pub fn touch_profile(&mut self, id: &str, now_unix_secs: u64) -> bool {
        match self.profiles.iter_mut().find(|p| p.id == id) {
            Some(p) => {
                p.last_used_at = now_unix_secs;
                true
            }
            None => false,
        }
    }

    /// 按「最近访问」排序的连接：`last_used_at` 倒序，从未连接过的排最后；
    /// 时刻相同（或都从未连接）时按名称升序。
    ///
    /// 排序必须**完全确定**：命令窗口每次打开都重排一遍，若同刻条目顺序随机，
    /// 列表会在两次打开之间跳动，用户就再也记不住「第三个是那台机器」。
    pub fn by_recent_use(&self) -> Vec<&ConnectionProfile> {
        let mut v: Vec<&ConnectionProfile> = self.profiles.iter().collect();
        v.sort_by(|a, b| {
            b.last_used_at
                .cmp(&a.last_used_at)
                .then_with(|| a.name.cmp(&b.name))
        });
        v
    }

    /// 把连接移动到指定分组 id（`None`= 移出到「未分组」）；目标须已登记，
    /// 返回是否真发生变更。
    pub fn move_profile_to_group(&mut self, id: &str, group_id: Option<&str>) -> bool {
        if let Some(g) = group_id {
            if !self.groups.iter().any(|x| x.id == g) {
                return false;
            }
        }
        let Some(p) = self.profiles.iter_mut().find(|p| p.id == id) else {
            return false;
        };
        if p.group_id.as_deref() == group_id {
            return false;
        }
        p.group_id = group_id.map(str::to_string);
        true
    }

    /// 连接在**所属分组内**的位置与同组成员数（右键「上移 / 下移」置灰依据）。
    ///
    /// 位置取的是「同组成员里的序号」，不是 `profiles` 的平铺下标：拖拽换组与导入
    /// 都会让不同分组的成员在平铺序列里交叉，用平铺下标会让「下移」在界面上一次
    /// 跳过好几个同组成员。同组口径与树上完全一致 —— 同一个 `group_id`
    /// （`None` = 未分组）。连接不存在时返回 `None`。
    pub fn profile_position(&self, id: &str) -> Option<(usize, usize)> {
        let group = self.profile(id)?.group_id.clone();
        let siblings: Vec<&str> = self
            .profiles
            .iter()
            .filter(|p| p.group_id == group)
            .map(|p| p.id.as_str())
            .collect();
        let idx = siblings.iter().position(|x| *x == id)?;
        Some((idx, siblings.len()))
    }

    /// 连接在所属分组内**上移 / 下移一格**（`delta` = -1 上移、+1 下移）。
    ///
    /// 只改平铺次序，`group_id` 一个字都不动 —— 这是排序，不是换组（换组走
    /// `move_profile_to_group`）。越界（已在首/末位）、id 不存在一律返回 `false`
    /// 且不改动任何字段；菜单项在首/末位本就置灰，这里是第二道闸门。
    pub fn move_profile_step(&mut self, id: &str, delta: i32) -> bool {
        let group = match self.profile(id) {
            Some(p) => p.group_id.clone(),
            None => return false,
        };
        let siblings: Vec<String> = self
            .profiles
            .iter()
            .filter(|p| p.group_id == group)
            .map(|p| p.id.clone())
            .collect();
        let Some(pos) = siblings.iter().position(|x| x == id) else {
            return false;
        };
        let target = pos as i64 + delta as i64;
        if target < 0 || target >= siblings.len() as i64 {
            return false;
        }
        let neighbor = siblings[target as usize].clone();
        let Some(from) = self.profiles.iter().position(|p| p.id == id) else {
            return false;
        };
        // 取出再插到邻居的**紧前 / 紧后**：同组成员交叉平铺时，直接 swap 会让
        // 被移动项跨过中间那些同组成员，一次跳好几格（见 `profile_position` 注释）。
        let moved = self.profiles.remove(from);
        match self.profiles.iter().position(|p| p.id == neighbor) {
            Some(at) => {
                self.profiles
                    .insert(if delta < 0 { at } else { at + 1 }, moved);
                true
            }
            // 邻居在此期间消失（并发改动）：放回原位，保持数据一致，不静默丢连接
            None => {
                self.profiles.insert(from, moved);
                false
            }
        }
    }

    /// 分组在**同级兄弟**中的位置与兄弟总数（右键「上移 / 下移」置灰依据）。
    ///
    /// 同级 = 有效父分组相同（口径与 `visible_tree` 同源，环/悬空引用按根级算），
    /// 次序 = `groups` 平铺次序，也就是树上的显示次序。分组不存在时返回 `None`。
    pub fn group_position(&self, id: &str) -> Option<(usize, usize)> {
        self.group(id)?;
        let parent = self.effective_parent(id);
        let siblings: Vec<&str> = self
            .groups
            .iter()
            .filter(|g| self.effective_parent(&g.id) == parent)
            .map(|g| g.id.as_str())
            .collect();
        let idx = siblings.iter().position(|x| *x == id)?;
        Some((idx, siblings.len()))
    }

    /// 分组在同一层兄弟之间**上移 / 下移一格**（`delta` = -1 上移、+1 下移）。
    ///
    /// 只与其他兄弟交换前后位置，**不换父**（换父走 `move_group_under`）：用户要的
    /// 是「把这个组往上挪一位」，越级跳到别的分组里不是排序。整棵子树一起搬，
    /// 子分组绝不会与父分组拆散，也不会插到别人的子树中间。越界 / id 不存在返回 `false`。
    pub fn move_group_step(&mut self, id: &str, delta: i32) -> bool {
        let Some((pos, len)) = self.group_position(id) else {
            return false;
        };
        let target = pos as i64 + delta as i64;
        if target < 0 || target >= len as i64 {
            return false;
        }
        let parent = self.effective_parent(id);
        let siblings: Vec<String> = self
            .groups
            .iter()
            .filter(|g| self.effective_parent(&g.id) == parent)
            .map(|g| g.id.clone())
            .collect();
        let neighbor = siblings[target as usize].clone();

        let Some(start) = self.groups.iter().position(|g| g.id == id) else {
            return false;
        };
        let end = self.subtree_end(id);
        let subtree: Vec<ConnectionGroup> = self.groups.drain(start..end).collect();
        if subtree.is_empty() {
            return false;
        }
        match self.groups.iter().position(|g| g.id == neighbor) {
            Some(at) => {
                // 上移插到邻居子树**之前**；下移插到邻居子树**之后**（整棵子树一起让位）
                let at = if delta < 0 {
                    at
                } else {
                    self.subtree_end(&neighbor)
                };
                let at = at.min(self.groups.len());
                self.groups.splice(at..at, subtree);
                true
            }
            None => {
                // 邻居消失：原样放回，绝不静默丢整棵子树
                self.groups.splice(start..start, subtree);
                false
            }
        }
    }

    /// 按名登记分组：已存在则返回既有 id，否则新建。空名返回 `None`
    /// （调用方据此把连接放到「未分组」）。
    pub fn ensure_group_by_name(&mut self, name: &str) -> Option<String> {
        let name = name.trim();
        if name.is_empty() {
            return None;
        }
        if let Some(id) = self.group_id_by_name(name) {
            return Some(id);
        }
        let g = ConnectionGroup::new(name);
        let id = g.id.clone();
        self.groups.push(g);
        Some(id)
    }

    /// 新建分组，返回新 id（追加到末尾；要指定位置用 `add_group_after`）
    pub fn add_group(&mut self, name: impl Into<String>) -> String {
        self.add_group_after(name, None)
    }

    /// 在 `after` 指向的分组**之后**（同级）插入新分组（`after` 为 `None` 或锚点已失效时
    /// 追加到末尾），返回新 id。新分组继承锚点的 `parent`（同级 = 同父）。
    ///
    /// 位置是语义的一部分：用户是在某个分组上右键「新建分组」的，新分组出现在那个分组
    /// **下面**才对得上「就在这儿建的」的直觉；一律追加到末尾会让人以为点错了地方。
    /// 插入点取锚点**子树末尾**（锚点可能带着一串子分组，插进子树中间会变成别人的孩子）。
    pub fn add_group_after(&mut self, name: impl Into<String>, after: Option<&str>) -> String {
        let mut g = ConnectionGroup::new(name);
        if let Some(a) = after {
            if let Some(anchor) = self.group(a) {
                g.parent = anchor.parent.clone();
                let id = g.id.clone();
                let ix = self.subtree_end(a);
                self.groups.insert(ix, g);
                return id;
            }
        }
        let id = g.id.clone();
        self.groups.push(g);
        id
    }

    /// 在 `parent` 分组**里面**新建子分组（2026-09-17 需求「连接组里还有组」），
    /// 返回新 id；父分组不存在时返回 `None`（调用方据此给可见提示，不静默）。
    pub fn add_subgroup(&mut self, name: impl Into<String>, parent: &str) -> Option<String> {
        self.group(parent)?;
        let mut g = ConnectionGroup::new(name);
        g.parent = Some(parent.to_string());
        let id = g.id.clone();
        let ix = self.subtree_end(parent);
        self.groups.insert(ix, g);
        Some(id)
    }

    /// 把分组 `id` 整棵子树挪到 `parent`（`None` = 提到根级）之下。
    /// 拒绝并返回 `false` 的情形：分组不存在、目标不存在、目标是自身、目标是自己的后代
    /// （后两种会造成环，树上会丢整棵子树）。挪动 = 改 `parent` + 平铺序列里整棵子树
    /// 搬到目标子树末尾（兄弟次序稳定：新来的排在末位）。
    pub fn move_group_under(&mut self, id: &str, parent: Option<&str>) -> bool {
        if self.group(id).is_none() {
            return false;
        }
        if let Some(p) = parent {
            if p == id || self.group(p).is_none() || self.is_descendant_of(p, id) {
                return false;
            }
        }
        let start = match self.groups.iter().position(|g| g.id == id) {
            Some(i) => i,
            None => return false,
        };
        let end = self.subtree_end(id);
        let mut subtree: Vec<ConnectionGroup> = self.groups.drain(start..end).collect();
        subtree[0].parent = parent.map(str::to_string);
        let at = parent.map_or(self.groups.len(), |p| self.subtree_end(p));
        let at = at.min(self.groups.len());
        self.groups.splice(at..at, subtree);
        true
    }

    /// `id` 是否是 `ancestor` 的后代（跨级也算）。链上带 hop 上限 —— 数据被手改成环时
    /// 按「不是后代」处理，绝不无限循环。
    pub fn is_descendant_of(&self, id: &str, ancestor: &str) -> bool {
        let mut cur = self.group(id).and_then(|g| g.parent.clone());
        let mut hops = 0;
        while let Some(p) = cur {
            if p == ancestor {
                return true;
            }
            if hops > self.groups.len() {
                return false;
            }
            cur = self.group(&p).and_then(|g| g.parent.clone());
            hops += 1;
        }
        false
    }

    /// `id` 子树（含自身与全部后代）在 `groups` 平铺序列中的结束下标（不含）。
    /// 树的兄弟次序 = 平铺次序，所以「插到子树末尾」就是「排在它所有后代后面」。
    fn subtree_end(&self, id: &str) -> usize {
        let base = self
            .groups
            .iter()
            .position(|g| g.id == id)
            .map_or(0, |i| i + 1);
        self.groups
            .iter()
            .rposition(|g| self.is_descendant_of(&g.id, id))
            .map_or(base, |i| (i + 1).max(base))
    }

    /// 分组改名。空名、与**其它**分组重名一律拒绝（返回 `false`）；分组未登记同样返回 `false`。
    /// 改成自己原来的名字算成功（幂等：重复提交不该报「已存在」）。
    pub fn rename_group(&mut self, id: &str, name: &str) -> bool {
        let name = name.trim();
        if name.is_empty() || self.groups.iter().any(|g| g.id != id && g.name == name) {
            return false;
        }
        match self.groups.iter_mut().find(|g| g.id == id) {
            Some(g) => {
                g.name = name.to_string();
                true
            }
            None => false,
        }
    }

    /// 删除分组。组内连接**不删除**，一律回收到「未分组」（`group_id = None`）；
    /// 直接子分组上移一级（挂到被删分组的父分组上，被删的是根级则提到根级）。
    ///
    /// 返回被回收的连接数；分组未登记时返回 `None`（调用方据此给提示，而不是静默）。
    /// 回收而不是连坐删除：删分组是整理动作，用户不会预期里面的服务器一起消失。
    pub fn remove_group(&mut self, id: &str) -> Option<usize> {
        let ix = self.groups.iter().position(|g| g.id == id)?;
        let parent = self.groups[ix].parent.clone();
        self.groups.remove(ix);
        for g in self.groups.iter_mut() {
            if g.parent.as_deref() == Some(id) {
                g.parent = parent.clone();
            }
        }
        let mut moved = 0;
        for p in self.profiles.iter_mut() {
            if p.group_id.as_deref() == Some(id) {
                p.group_id = None;
                moved += 1;
            }
        }
        Some(moved)
    }

    /// 按 id 取分组
    pub fn group(&self, id: &str) -> Option<&ConnectionGroup> {
        self.groups.iter().find(|g| g.id == id)
    }

    /// 分组展示名（id 已失效时返回 `None`，调用方自行兜底）
    pub fn group_name(&self, id: &str) -> Option<&str> {
        self.group(id).map(|g| g.name.as_str())
    }

    /// 按名取分组 id
    pub fn group_id_by_name(&self, name: &str) -> Option<String> {
        self.groups
            .iter()
            .find(|g| g.name == name)
            .map(|g| g.id.clone())
    }

    /// 迁移与纠偏的唯一收口：给缺 id 的分组补 id、把旧的按名归属
    /// （`legacy_group`）解析成 `group_id`、同名分组去重、清理指向已消失
    /// 分组的悬空引用。返回是否发生过变更（测试据此断言幂等）。
    pub fn normalize(&mut self) -> bool {
        let mut changed = false;

        for g in self.groups.iter_mut() {
            if g.id.trim().is_empty() {
                g.id = uuid_like();
                changed = true;
            }
        }
        {
            let mut seen = std::collections::HashSet::new();
            let before = self.groups.len();
            self.groups.retain(|g| seen.insert(g.name.clone()));
            if self.groups.len() != before {
                changed = true;
            }
        }

        // 旧 `group`（名）到 `group_id`：未知的分组名就地补建，不丢归属
        let mut extra: Vec<ConnectionGroup> = Vec::new();
        for p in self.profiles.iter_mut() {
            let Some(name) = p.legacy_group.take() else {
                continue;
            };
            changed = true;
            let name = name.trim().to_string();
            if name.is_empty() {
                p.group_id = None;
                continue;
            }
            match self
                .groups
                .iter()
                .chain(extra.iter())
                .find(|g| g.name == name)
            {
                Some(g) => p.group_id = Some(g.id.clone()),
                None => {
                    let g = ConnectionGroup::new(name);
                    p.group_id = Some(g.id.clone());
                    extra.push(g);
                }
            }
        }
        self.groups.extend(extra);

        // 悬空引用（分组被删）回收为「未分组」
        let ids: std::collections::HashSet<String> =
            self.groups.iter().map(|g| g.id.clone()).collect();
        for p in self.profiles.iter_mut() {
            if let Some(gid) = p.group_id.clone() {
                if !ids.contains(&gid) {
                    p.group_id = None;
                    changed = true;
                }
            }
        }

        // parent 引用纠偏（嵌套分组）：自指 / 悬空 / 沿链绕回自身（环）一律断开成根级。
        // 环只可能来自手工编辑 JSON —— 正常 API 全部带环校验；这里兜底保证渲染不丢分组。
        let mut fixed: Vec<Option<String>> = Vec::with_capacity(self.groups.len());
        for g in &self.groups {
            fixed.push(match g.parent.as_deref() {
                Some(p) if p != g.id && ids.contains(p) => {
                    // 沿父链向上走，看是否绕回自己（hop 上限防环里死循环）
                    let mut cur = self.group(p).and_then(|x| x.parent.clone());
                    let mut hops = 0;
                    let mut cycle = false;
                    while let Some(c) = cur {
                        if c == g.id || hops > self.groups.len() {
                            cycle = true;
                            break;
                        }
                        cur = self.group(&c).and_then(|x| x.parent.clone());
                        hops += 1;
                    }
                    if cycle {
                        None
                    } else {
                        g.parent.clone()
                    }
                }
                _ => None,
            });
        }
        for (g, p) in self.groups.iter_mut().zip(fixed) {
            if g.parent != p {
                g.parent = p;
                changed = true;
            }
        }
        changed
    }

    pub fn profile(&self, id: &str) -> Option<&ConnectionProfile> {
        self.profiles.iter().find(|p| p.id == id)
    }

    /// 指定分组 id 下的连接
    pub fn profiles_in_group(&self, group_id: &str) -> Vec<&ConnectionProfile> {
        self.profiles
            .iter()
            .filter(|p| p.group_id.as_deref() == Some(group_id))
            .collect()
    }

    /// 无分组（未分组）成员
    pub fn ungrouped(&self) -> Vec<&ConnectionProfile> {
        self.profiles
            .iter()
            .filter(|p| p.group_id.is_none())
            .collect()
    }

    /// 启动时恢复记住的密码到运行时字段（unprotect 由平台层提供）。
    ///
    /// 代理口令（F-10）走同一条通道：连接级「自定义代理」的口令也是引用落盘，
    /// 不解出来这条连接就会带着空口令去认证（代理回 407，用户看不出为什么）。
    pub fn restore_passwords(&mut self, unprotect: impl Fn(&str) -> Option<String>) {
        for p in self.profiles.iter_mut() {
            if p.password.is_none() {
                if let Some(stored) = &p.stored_password {
                    p.password = unprotect(stored);
                }
            }
            if let crate::proxy::ProxyChoice::Custom(c) = &mut p.proxy {
                *c = c.with_plaintext(&unprotect);
            }
        }
    }
}

// 导入 / 导出

/// 导出文件标识：导入时据此区分本程序的导出体与裸连接文件。
pub const EXPORT_FORMAT: &str = "rustshell-connections";
/// 导出格式版本；导入时高于此值的文件一律拒绝，不做猜测性解析。
pub const EXPORT_VERSION: u32 = 1;

/// 导出体中的单条连接。
/// 刻意不复用 `ConnectionProfile`：它的 `stored_password`（系统凭据密文，换机不可解）
/// 是机器绑定的敏感串，必须从导出体里结构性排除，而不是靠调用方自觉。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ExportedProfile {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_path: Option<String>,
    /// 备注：与 `ConnectionProfile::notes` 同义，随连接一起搬家。
    /// 它是**用户自己写的说明**（不是本机使用习惯，故不像 `last_used_at` 那样被排除）；
    /// `default` + `skip_serializing_if` 保证旧导出体（无此键）与空备注都不产生噪声。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub notes: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// 连接标签：与备注同属「连接自身的属性」，随导出/导入一起搬家。
    /// 定时任务与 agent 的目标筛选靠它，跨机搬家后目标集合仍然可解析。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// 端口转发规则（F-3）：规则里只有地址与端口，**不含任何凭据**，
    /// 与 `tags` 同属「连接自身的属性」，跟着连接一起搬家。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub forwards: Vec<ForwardRule>,
    /// 会话日志的连接级覆盖（F-7）。它也是**连接自身的属性**（「这台机器要留档」），
    /// 不是本机使用习惯，所以随导出搬家 —— 三态照旧：`None` 落盘时不写键。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_log: Option<bool>,
    /// 代理（F-10）的连接级口径。**只带路线、不带凭据**：`Custom` 的载荷是
    /// [`crate::proxy::ExportedProxy`]（一个没有口令字段的类型），
    /// 代理口令导入后需重填。
    #[serde(
        default,
        skip_serializing_if = "crate::proxy::ExportedProxyChoice::is_follow_global"
    )]
    pub proxy: crate::proxy::ExportedProxyChoice,
    /// 断线自动重连（F-4）的连接级覆盖。与 `session_log` 同理：这是
    /// **连接自身的属性**（「这台生产机别自动连」），随导出搬家，三态照旧
    /// （`None` 落盘不写键）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_reconnect: Option<bool>,
}

/// 导出文件体。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionExport {
    pub format: String,
    pub version: u32,
    /// 导出时刻（unix 秒）；仅作信息展示。
    #[serde(default)]
    pub exported_at: u64,
    /// 分组名数组。宽容读对象数组：用户可能直接把本程序新版的
    /// `connections.json` 当裸文件丢进来导入。
    #[serde(default, deserialize_with = "de_group_names")]
    pub groups: Vec<String>,
    #[serde(default)]
    pub profiles: Vec<ExportedProfile>,
}

/// 一次导入的结果（UI 据此出提示；三项计数之和 = 文件内条目数）。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ImportOutcome {
    /// 新增的连接数
    pub added: usize,
    /// 因重复（同 host+port+username）而跳过的连接数
    pub skipped: usize,
    /// 新增的分组数
    pub groups_added: usize,
}

impl ImportOutcome {
    /// 是否什么都没导入（用于「文件里没有可导入的新连接」这种明确提示）
    pub fn is_empty(&self) -> bool {
        self.added == 0 && self.groups_added == 0
    }
}

/// 导出体 `groups` 用的读取器：名字数组 / 对象数组都接受，统一取名字。
fn de_group_names<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw: Vec<GroupRepr> = Vec::deserialize(deserializer)?;
    Ok(raw
        .into_iter()
        .map(|g| match g {
            GroupRepr::Full(g) => g.name,
            GroupRepr::Name(n) => n,
        })
        .collect())
}

/// 去重键：主机与用户名大小写不敏感，端口参与比较。
pub fn profile_key(host: &str, port: u16, username: &str) -> String {
    format!(
        "{}:{}:{}",
        host.trim().to_ascii_lowercase(),
        port,
        username.trim().to_ascii_lowercase()
    )
}

/// 连接树搜索关键词（已归一化：trim + 小写）。
///
/// 空关键词表示不筛，一切匹配。归一化只做一次，避免在过滤循环里反复分配。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchQuery(String);

impl SearchQuery {
    pub fn parse(raw: &str) -> Self {
        Self(raw.trim().to_lowercase())
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// 按 名称 / 主机 / 用户名 匹配，三处都用子串而非前缀。
    /// 用户更常记得中间一段（`10.0.3`、`prod`），前缀匹配会把它们全漏掉。
    pub fn matches(&self, p: &ConnectionProfile) -> bool {
        if self.is_empty() {
            return true;
        }
        let q = self.0.as_str();
        if p.name.to_lowercase().contains(q)
            || p.host.to_lowercase().contains(q)
            || p.username.to_lowercase().contains(q)
        {
            return true;
        }
        // `root@10.0.0.1` 连写形态：用户常整段粘贴，而两个字段各自都不含它。
        format!("{}@{}", p.username, p.host)
            .to_lowercase()
            .contains(q)
    }

    /// 分组名是否命中。命中时该分组下所有连接一并展示，便于按分组名一次捞全。
    pub fn matches_group(&self, name: &str) -> bool {
        self.is_empty() || name.to_lowercase().contains(self.0.as_str())
    }
}

/// 可见分组：搜索过滤后仍要展示的分组（嵌套树形）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisibleGroup {
    pub id: String,
    pub name: String,
    /// 该分组下可见的连接 id（直接成员，保持原有顺序）
    pub profile_ids: Vec<String>,
    /// 可见的子分组（2026-09-17「连接组里还有组」）
    pub children: Vec<VisibleGroup>,
}

/// 连接树的可见内容（搜索过滤后的结果）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct VisibleTree {
    /// 根级分组（子分组挂在各根的 `children` 里）
    pub groups: Vec<VisibleGroup>,
    /// 未分组的可见连接 id
    pub ungrouped: Vec<String>,
}

impl VisibleTree {
    /// 搜索结果为空。UI 必须据此给出可见提示，不能白屏让用户以为连接丢了。
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty() && self.ungrouped.is_empty()
    }
}

impl ConnectionStore {
    /// 只读口径的有效父分组：`parent` 自指 / 悬空 / 沿链绕回自身（环）时按根级处理。
    /// 与 `normalize` 的纠偏同口径，但不改库 —— 渲染在两次 normalize 之间也要绝不丢分组。
    fn effective_parent(&self, id: &str) -> Option<String> {
        let first = self.group(id)?.parent.clone()?;
        self.group(&first)?;
        let mut cur = self.group(&first).and_then(|g| g.parent.clone());
        let mut hops = 0;
        while let Some(p) = cur {
            if p == id || hops > self.groups.len() {
                return None;
            }
            cur = self.group(&p).and_then(|g| g.parent.clone());
            hops += 1;
        }
        Some(first)
    }

    /// 按搜索关键词规划树的可见内容（嵌套版）。
    ///
    /// 规则：分组名命中 ⇒ 该分组**整棵子树**保留（全部直接成员 + 全部子孙，与旧扁平
    /// 语义一致）；否则只保留命中的成员与命中的子孙分组，但**祖先链必须保留**
    /// （命中藏三级深处的连接而把它的两个祖先分组隐藏，等于搜了没结果）。
    /// 无搜索时空分组照常显示（它是用户搭的收纳结构，不是搜索结果）。
    pub fn visible_tree(&self, q: &SearchQuery) -> VisibleTree {
        // 有效父快照：环成员会被判成根级（成对的环各自成根，不会互相挂、不会无限递归）
        let eff: Vec<Option<String>> = self
            .groups
            .iter()
            .map(|g| self.effective_parent(&g.id))
            .collect();
        let roots: Vec<VisibleGroup> = self
            .groups
            .iter()
            .enumerate()
            .filter(|(i, _)| eff[*i].is_none())
            .filter_map(|(_, g)| self.visible_subtree(g, &eff, q))
            .collect();
        VisibleTree {
            groups: roots,
            ungrouped: self
                .ungrouped()
                .into_iter()
                .filter(|p| q.matches(p))
                .map(|p| p.id.clone())
                .collect(),
        }
    }

    /// 单个分组的可见子树（`eff` 是全库的有效父快照；子分组 = 有效父指向本组的分组）。
    fn visible_subtree(
        &self,
        g: &ConnectionGroup,
        eff: &[Option<String>],
        q: &SearchQuery,
    ) -> Option<VisibleGroup> {
        let children: Vec<&ConnectionGroup> = self
            .groups
            .iter()
            .zip(eff)
            .filter(|(_, p)| p.as_deref() == Some(g.id.as_str()))
            .map(|(c, _)| c)
            .collect();

        if q.is_empty() {
            return Some(VisibleGroup {
                id: g.id.clone(),
                name: g.name.clone(),
                profile_ids: self
                    .profiles_in_group(&g.id)
                    .into_iter()
                    .map(|p| p.id.clone())
                    .collect(),
                children: children
                    .iter()
                    .filter_map(|c| self.visible_subtree(c, eff, q))
                    .collect(),
            });
        }
        if q.matches_group(&g.name) {
            // 分组名命中 ⇒ 整棵子树保留（递归全量，子孙不再按关键词裁剪）
            return Some(self.full_subtree(g, eff));
        }
        let ids: Vec<String> = self
            .profiles_in_group(&g.id)
            .into_iter()
            .filter(|p| q.matches(p))
            .map(|p| p.id.clone())
            .collect();
        let vis_children: Vec<VisibleGroup> = children
            .iter()
            .filter_map(|c| self.visible_subtree(c, eff, q))
            .collect();
        // 无搜索不会走到这；有搜索时一个成员/子组都不中就不占版面
        if ids.is_empty() && vis_children.is_empty() {
            None
        } else {
            Some(VisibleGroup {
                id: g.id.clone(),
                name: g.name.clone(),
                profile_ids: ids,
                children: vis_children,
            })
        }
    }

    /// 整棵子树全量可见（分组名命中的分支专用）
    fn full_subtree(&self, g: &ConnectionGroup, eff: &[Option<String>]) -> VisibleGroup {
        VisibleGroup {
            id: g.id.clone(),
            name: g.name.clone(),
            profile_ids: self
                .profiles_in_group(&g.id)
                .into_iter()
                .map(|p| p.id.clone())
                .collect(),
            children: self
                .groups
                .iter()
                .zip(eff)
                .filter(|(_, p)| p.as_deref() == Some(g.id.as_str()))
                .map(|(c, _)| self.full_subtree(c, eff))
                .collect(),
        }
    }

    /// 分组子树（自身 + 全部后代分组）内的连接 id。「全部连接」与树上分组徽标都用它：
    /// 嵌套后用户预期「这个组里的连接」= 整棵子树。`seen` 防手工编辑出的环。
    pub fn profile_ids_in_subtree(&self, group_id: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut seen = std::collections::HashSet::from([group_id.to_string()]);
        self.collect_subtree_profiles(group_id, &mut seen, &mut out);
        out
    }

    fn collect_subtree_profiles(
        &self,
        group_id: &str,
        seen: &mut std::collections::HashSet<String>,
        out: &mut Vec<String>,
    ) {
        out.extend(
            self.profiles_in_group(group_id)
                .into_iter()
                .map(|p| p.id.clone()),
        );
        for g in &self.groups {
            if g.parent.as_deref() == Some(group_id) && seen.insert(g.id.clone()) {
                self.collect_subtree_profiles(&g.id, seen, out);
            }
        }
    }

    /// 后代分组数（不含自身）。删除确认文案用：有子分组时要说清「上移一级」。
    pub fn descendant_group_count(&self, group_id: &str) -> usize {
        self.groups
            .iter()
            .filter(|g| self.is_descendant_of(&g.id, group_id))
            .count()
    }
}

// 目标解析：agent 与定时任务共用的「选出一批连接」入口

impl ConnectionStore {
    /// 全部连接的 id（按分组树先序 + 组内原序）。
    ///
    /// 顺序必须**稳定**：同一个指令两次运行选出同一批、同一个顺序，
    /// 巡检结果的对比才有意义（否则「哪台变了」会被顺序抖动淹没）。
    pub fn all_profile_ids(&self) -> Vec<String> {
        let mut out = Vec::new();
        // 先按分组树的先序取，再补未分组的
        for g in &self.groups {
            if g.parent.is_none() {
                let mut seen = std::collections::HashSet::new();
                self.collect_subtree_profiles(&g.id, &mut seen, &mut out);
            }
        }
        out.extend(self.ungrouped().into_iter().map(|p| p.id.clone()));
        // 去重（手工编辑出的异常归属可能让同一 id 出现两次）
        let mut seen = std::collections::HashSet::new();
        out.retain(|id| seen.insert(id.clone()));
        out
    }

    /// 带指定标签的连接 id（不区分大小写；顺序与 [`Self::all_profile_ids`] 一致）。
    pub fn profile_ids_by_tag(&self, tag: &str) -> Vec<String> {
        let want = tag.trim();
        if want.is_empty() {
            return Vec::new();
        }
        self.all_profile_ids()
            .into_iter()
            .filter(|id| self.profile(id).map(|p| p.has_tag(want)).unwrap_or(false))
            .collect()
    }

    /// 已使用的全部标签（去重、按首次出现排序）。连接编辑对话框做候选提示用。
    pub fn all_tags(&self) -> Vec<String> {
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for id in self.all_profile_ids() {
            if let Some(p) = self.profile(&id) {
                for t in &p.tags {
                    if seen.insert(t.to_lowercase()) {
                        out.push(t.clone());
                    }
                }
            }
        }
        out
    }

    /// 把若干来源（分组 / 连接 / 标签）合起来解析为去重后的连接 id 列表，
    /// 再扣掉 `exclude`。**这是 agent 与定时任务唯一的目标解析入口。**
    ///
    /// 返回空列表不是错误（「这个组里没有连接」是合法状态），
    /// 由调用方决定怎么提示；但把这三种来源混在一处的去重与保序
    /// 必须只有一份实现 —— 否则「分组 + 标签」会被算成两台机器。
    pub fn resolve_target(
        &self,
        groups: &[String],
        profiles: &[String],
        tags: &[String],
        exclude: &[String],
    ) -> Vec<String> {
        let mut ordered: Vec<String> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let push =
            |id: String, seen: &mut std::collections::HashSet<String>, out: &mut Vec<String>| {
                if seen.insert(id.clone()) {
                    out.push(id);
                }
            };

        for g in groups {
            for id in self.profile_ids_in_subtree(g) {
                push(id, &mut seen, &mut ordered);
            }
        }
        for p in profiles {
            // 显式 id 也要过滤掉不存在的（配置被手工改坏时不留悬空引用）
            if self.profile(p).is_some() {
                push(p.clone(), &mut seen, &mut ordered);
            }
        }
        for t in tags {
            for id in self.profile_ids_by_tag(t) {
                push(id, &mut seen, &mut ordered);
            }
        }

        if !exclude.is_empty() {
            let drop: std::collections::HashSet<&str> =
                exclude.iter().map(|s| s.as_str()).collect();
            ordered.retain(|id| !drop.contains(id.as_str()));
        }
        ordered
    }
}

impl ConnectionStore {
    /// 生成导出体（不含任何口令字段）。
    pub fn export(&self, exported_at: u64) -> ConnectionExport {
        ConnectionExport {
            format: EXPORT_FORMAT.to_string(),
            version: EXPORT_VERSION,
            exported_at,
            // 导出体是给人看、跨机搬的交换格式，保持「按名」的 v1 形态：
            // 分组 id 是本机库的内部标识，搬过去没有意义（对端会重新分配）。
            groups: self.groups.iter().map(|g| g.name.clone()).collect(),
            profiles: self
                .profiles
                .iter()
                .map(|p| ExportedProfile {
                    name: p.name.clone(),
                    host: p.host.clone(),
                    port: p.port,
                    username: p.username.clone(),
                    key_path: p.key_path.clone(),
                    notes: p.notes.clone(),
                    group: p
                        .group_id
                        .as_deref()
                        .and_then(|id| self.group_name(id))
                        .map(str::to_string),
                    tags: p.tags.clone(),
                    forwards: p.forwards.clone(),
                    session_log: p.session_log,
                    // 只搬路线，不搬口令（`to_export` 换成没有口令字段的镜像类型）
                    proxy: p.proxy.to_export(),
                    // 重连三态也随连接走：`None` 到新机器后跟随那台的全局设置
                    auto_reconnect: p.auto_reconnect,
                })
                .collect(),
        }
    }

    /// 导出体 到 可落盘的 JSON 文本（缩进便于人工 diff / 代码评审）。
    /// 放在 core 而不是 UI 层：`serde_json` 是 core 的既有依赖，
    /// UI 层不必为了写一个文件而多引一个 crate。
    pub fn export_to_json(exp: &ConnectionExport) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(exp)
    }

    /// 合并一份导出体：重复项跳过、新增项分配新 id、分组顺带登记。
    /// 不覆盖任何既有连接，也不因单条异常中断整批（逐条独立判定）。
    pub fn merge_import(&mut self, exp: &ConnectionExport) -> ImportOutcome {
        let mut out = ImportOutcome::default();
        let existing: Vec<String> = self
            .profiles
            .iter()
            .map(|p| profile_key(&p.host, p.port, &p.username))
            .collect();

        for ep in &exp.profiles {
            // 空主机/空用户名没有可用性，跳过而不是导入一条连不上的记录
            if ep.host.trim().is_empty() || ep.username.trim().is_empty() {
                out.skipped += 1;
                continue;
            }
            let key = profile_key(&ep.host, ep.port, &ep.username);
            if existing.contains(&key) || self.has_key(&key) {
                out.skipped += 1;
                continue;
            }
            let group_id = match ep.group.as_deref().map(str::trim) {
                Some(name) if !name.is_empty() => {
                    let existed = self.group_id_by_name(name).is_some();
                    let id = self.ensure_group_by_name(name);
                    if id.is_some() && !existed {
                        out.groups_added += 1;
                    }
                    id
                }
                _ => None,
            };
            self.profiles.push(ConnectionProfile {
                id: uuid_like(),
                name: ep.name.clone(),
                host: ep.host.clone(),
                port: ep.port,
                username: ep.username.clone(),
                password: None,
                // 导出体本就不含任何口令，导入后必须重新输入
                stored_password: None,
                key_path: ep.key_path.clone(),
                notes: ep.notes.clone(),
                group_id,
                legacy_group: None,
                // 导入是「别的机器上的使用习惯」的终点：这条连接在本机还没访问过
                last_used_at: 0,
                // 标签是连接自身的属性（不是使用习惯），随导出/导入一起走
                tags: ep.tags.clone(),
                // 端口转发规则同属连接配置，也随迁移走（导出体里是脱敏后的规则）
                forwards: ep.forwards.clone(),
                // 会话日志覆盖同上（`None` = 到新机器后跟随那台机器的全局设置）
                session_log: ep.session_log,
                // 代理同属连接配置，但**口令不跟着走**（导出体里本就没有）：
                // 自定义代理导入后是「地址齐全、口令为空」，需用户重填
                proxy: crate::proxy::ProxyChoice::from_export(ep.proxy.clone()),
                // 重连三态同 `session_log`：`None` = 到新机器后跟随那台的全局设置
                auto_reconnect: ep.auto_reconnect,
            });
            out.added += 1;
        }
        // 文件里登记但尚无连接的分组也保留（用户可能在整理分组）
        for name in &exp.groups {
            if self.group_id_by_name(name).is_none() && self.ensure_group_by_name(name).is_some() {
                out.groups_added += 1;
            }
        }
        out
    }

    fn has_key(&self, key: &str) -> bool {
        self.profiles
            .iter()
            .any(|p| profile_key(&p.host, p.port, &p.username) == key)
    }

    /// 需要导入的新条目数（预检：让 UI 在真正写入前就能告诉用户「这份文件里有没有新东西」）。
    pub fn import_preview(&self, exp: &ConnectionExport) -> ImportOutcome {
        let mut probe = self.clone();
        probe.merge_import(exp)
    }
}

/// 导入失败原因（区分「文件不对」与「版本不支持」，便于给出可操作提示）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportError {
    /// JSON 解析失败
    BadJson(String),
    /// 有 format 字段但不是本程序的格式
    UnsupportedFormat(String),
    /// 版本高于当前程序支持的版本
    VersionTooNew(u32),
    /// 既无 format 也无 profiles 字段
    UnknownShape,
}

impl std::fmt::Display for ImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ImportError::BadJson(e) => write!(f, "文件不是合法的 JSON：{e}"),
            ImportError::UnsupportedFormat(s) => {
                write!(f, "不是 RustShell 导出的连接文件（format={s}）")
            }
            ImportError::VersionTooNew(v) => write!(
                f,
                "文件版本过新（v{v} > v{EXPORT_VERSION}），请升级 RustShell 后再导入"
            ),
            ImportError::UnknownShape => write!(f, "文件里既没有导出标识，也没有 profiles 列表"),
        }
    }
}

impl std::error::Error for ImportError {}

/// 解析导入文件文本，接受两种形态：
/// 1. 本程序导出体（带 `format` 标识）；
/// 2. 裸 `ConnectionStore`（有 `profiles` 数组，`groups` 可缺省）。
pub fn parse_import(text: &str) -> Result<ConnectionExport, ImportError> {
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|e| ImportError::BadJson(e.to_string()))?;
    let obj = value.as_object().ok_or(ImportError::UnknownShape)?;

    if let Some(fmt) = obj.get("format").and_then(|v| v.as_str()) {
        if fmt != EXPORT_FORMAT {
            return Err(ImportError::UnsupportedFormat(fmt.to_string()));
        }
        let version = obj.get("version").and_then(|v| v.as_u64()).unwrap_or(1) as u32;
        if version > EXPORT_VERSION {
            return Err(ImportError::VersionTooNew(version));
        }
        return serde_json::from_value(value).map_err(|e| ImportError::BadJson(e.to_string()));
    }

    if obj.get("profiles").is_some() {
        // 裸连接文件：按同一导出体解析（缺 format/version 时补默认）
        let mut value = value;
        // 裸文件也可能是本程序**新版**的连接库：那里 `groups` 是对象数组、
        // 连接用 `group_id` 引用。导出体是按名的，故先把 id 就地翻成名字，
        // 否则分组信息会被静默丢掉（宁可多写几行，也不丢用户数据）。
        if let Some(m) = value.as_object_mut() {
            let id_to_name: std::collections::HashMap<String, String> = m
                .get("groups")
                .and_then(|g| g.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|g| {
                            let o = g.as_object()?;
                            Some((
                                o.get("id")?.as_str()?.to_string(),
                                o.get("name")?.as_str()?.to_string(),
                            ))
                        })
                        .collect()
                })
                .unwrap_or_default();
            if !id_to_name.is_empty() {
                if let Some(profiles) = m.get_mut("profiles").and_then(|p| p.as_array_mut()) {
                    for p in profiles.iter_mut() {
                        let Some(o) = p.as_object_mut() else {
                            continue;
                        };
                        let Some(gid) = o.remove("group_id") else {
                            continue;
                        };
                        if let Some(name) = gid.as_str().and_then(|g| id_to_name.get(g)) {
                            o.insert("group".to_string(), serde_json::Value::String(name.clone()));
                        }
                    }
                }
            }
        }
        if let Some(m) = value.as_object_mut() {
            m.insert(
                "format".to_string(),
                serde_json::Value::String(EXPORT_FORMAT.to_string()),
            );
            m.insert(
                "version".to_string(),
                serde_json::Value::from(EXPORT_VERSION),
            );
        }
        let mut exp: ConnectionExport =
            serde_json::from_value(value).map_err(|e| ImportError::BadJson(e.to_string()))?;
        // 裸文件里可能带口令字段，解析后被 serde 忽略（ExportedProfile 无对应字段），
        // 这里显式清空只为让「导入体永不含口令」成为结构性事实。
        exp.format = EXPORT_FORMAT.to_string();
        return Ok(exp);
    }

    Err(ImportError::UnknownShape)
}

/// 简易唯一 ID（避免引入 uuid 依赖）
pub fn uuid_like() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let rand: u16 = (nanos & 0xFFFF) as u16;
    format!("c-{:x}-{:x}", nanos as u64, rand)
}

/// 当前 Unix 秒（取不到系统时钟时回退 0，仅影响「最近访问」排序）。
pub fn unix_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_not_serialized() {
        let mut p = ConnectionProfile::new("prod-01", "10.0.0.1", "root");
        p.password = Some("secret".into());
        let json = serde_json::to_string(&p).unwrap();
        assert!(!json.contains("secret"));
    }

    #[test]
    fn stored_password_serialized_when_set() {
        let mut p = ConnectionProfile::new("prod-01", "10.0.0.1", "root");
        p.stored_password = Some("dpapi-blob".into());
        let json = serde_json::to_string(&p).unwrap();
        assert!(json.contains("dpapi-blob"));
        // 明文密码仍不落盘
        p.password = Some("secret".into());
        let json = serde_json::to_string(&p).unwrap();
        assert!(!json.contains("secret"));
    }

    /// 加 `proxy` 字段之前落盘的库必须照常读得进来（跟随全局）。
    #[test]
    fn legacy_json_without_the_proxy_key_loads_as_follow_global() {
        let one: ConnectionProfile =
            serde_json::from_str(r#"{"id":"p1","name":"a","host":"h","port":22,"username":"u"}"#)
                .expect("单条旧格式必须能读");
        assert_eq!(one.proxy, crate::proxy::ProxyChoice::FollowGlobal);

        let store: ConnectionStore = serde_json::from_str(
            r#"{"profiles":[{"id":"p1","name":"a","host":"h","port":22,"username":"u"}],"groups":[]}"#,
        )
        .expect("旧格式的库必须能读");
        assert_eq!(
            store.profiles[0].proxy,
            crate::proxy::ProxyChoice::FollowGlobal
        );
        // 跟随全局是默认档位，落盘时不该冒出这个键（否则每条连接都多一行噪声）
        let back = serde_json::to_string(&store.profiles[0]).unwrap();
        assert!(!back.contains("proxy"), "{back}");
    }

    /// 连接级「自定义代理」的口令与连接口令走同一条恢复通道：
    /// 不解出来，这条连接就会带着空口令去敲代理（407 的现场看不出原因）。
    #[test]
    fn restore_passwords_also_fills_a_custom_proxy() {
        let mut store = ConnectionStore::default();
        let mut p = ConnectionProfile::new("a", "h", "u");
        p.proxy = crate::proxy::ProxyChoice::Custom(crate::proxy::ProxyConfig {
            host: "proxy.lan".into(),
            port: 3128,
            username: Some("pu".into()),
            password_ref: Some("cipher:PROXY".into()),
            ..Default::default()
        });
        store.add_profile(p);

        store.restore_passwords(|blob| (blob == "cipher:PROXY").then(|| "pw".to_string()));
        let crate::proxy::ProxyChoice::Custom(c) = &store.profiles[0].proxy else {
            panic!("自定义代理必须原样保留（不能被恢复流程降级成跟随全局）");
        };
        assert_eq!(c.password.as_deref(), Some("pw"));
        assert_eq!(
            c.password_ref.as_deref(),
            Some("cipher:PROXY"),
            "密文引用不能被明文顶掉 —— 它就是下次启动要用的那一份"
        );

        // 解不开（换了机器 / 凭据库不可用）时留空，而不是把引用也抹了
        let mut store2 = ConnectionStore::default();
        let mut fresh = ConnectionProfile::new("b", "h2", "u");
        fresh.proxy = crate::proxy::ProxyChoice::Custom(crate::proxy::ProxyConfig {
            host: "proxy.lan".into(),
            port: 3128,
            password_ref: Some("cipher:PROXY".into()),
            ..Default::default()
        });
        store2.add_profile(fresh);
        store2.restore_passwords(|_| None);
        let crate::proxy::ProxyChoice::Custom(c) = &store2.profiles[0].proxy else {
            unreachable!()
        };
        assert_eq!(c.password, None);
        assert_eq!(c.password_ref.as_deref(), Some("cipher:PROXY"));
    }

    /// 导出 / 导入的代理路线保真：三态口径与原样保留，凭据结构性丢失。
    #[test]
    fn export_import_round_trips_the_proxy_route_without_its_password() {
        let mut store = ConnectionStore::default();
        let mut p = ConnectionProfile::new("a", "h", "u");
        p.proxy = crate::proxy::ProxyChoice::Custom(crate::proxy::ProxyConfig {
            kind: crate::proxy::ProxyKind::Socks5,
            host: "proxy.lan".into(),
            port: 1080,
            username: Some("pu".into()),
            password_ref: Some("cipher:PROXY".into()),
            password: Some("plain-proxy-pw".into()),
        });
        store.add_profile(p);
        // 第二档：明确直连也要活着过去（它表达用户的刻意选择）
        let mut direct = ConnectionProfile::new("b", "h2", "u");
        direct.proxy = crate::proxy::ProxyChoice::Direct;
        store.add_profile(direct);

        let exp = store.export(0);
        let json = serde_json::to_string(&exp).unwrap();
        assert!(!json.contains("cipher:PROXY"), "导出体泄漏了代理密文引用");
        assert!(!json.contains("plain-proxy-pw"), "导出体泄漏了代理明文口令");

        let mut into = ConnectionStore::default();
        let out = into.merge_import(&exp);
        assert_eq!(out.added, 2);
        // 导入会换 id（避免与库内既有条目撞号），所以按主机找回来
        let imported = into
            .profiles
            .iter()
            .find(|q| q.host == "h")
            .expect("自定义代理那条应被导入");
        let crate::proxy::ProxyChoice::Custom(c) = &imported.proxy else {
            panic!("导入后必须还是「自定义」，不能悄悄退回跟随全局");
        };
        assert_eq!(c.kind, crate::proxy::ProxyKind::Socks5);
        assert_eq!(c.host, "proxy.lan");
        assert_eq!(c.port, 1080);
        assert_eq!(c.username.as_deref(), Some("pu"));
        assert_eq!(c.password_ref, None, "口令是机器绑定的，不随导出体搬家");
        assert_eq!(c.password, None);
        assert_eq!(
            into.profiles
                .iter()
                .find(|q| q.host == "h2")
                .map(|q| q.proxy.clone()),
            Some(crate::proxy::ProxyChoice::Direct),
            "「明确直连」这一档必须保真（导入后不该变成跟随全局）"
        );
    }

    /// 三态重连开关（F-4）与 `session_log` 同一纪律：三档全部保真搬家。
    #[test]
    fn export_import_round_trips_the_reconnect_override() {
        let mut store = ConnectionStore::default();
        let mut forced_on = ConnectionProfile::new("a", "h", "u");
        forced_on.auto_reconnect = Some(true);
        store.add_profile(forced_on);
        let mut forced_off = ConnectionProfile::new("b", "h2", "u");
        forced_off.auto_reconnect = Some(false);
        store.add_profile(forced_off);
        // 第三档：没表态 —— 到新机器后跟随那台的全局设置
        store.add_profile(ConnectionProfile::new("c", "h3", "u"));

        let exp = store.export(0);
        let mut into = ConnectionStore::default();
        assert_eq!(into.merge_import(&exp).added, 3);
        let find = |host: &str| {
            into.profiles
                .iter()
                .find(|q| q.host == host)
                .map(|q| q.auto_reconnect)
        };
        assert_eq!(find("h"), Some(Some(true)), "「强制重连」必须保真");
        assert_eq!(find("h2"), Some(Some(false)), "「绝不重连」必须保真");
        assert_eq!(
            find("h3"),
            Some(None),
            "没表态的那档不能变成 false（那会在新机器上悄悄关掉重连）"
        );
    }

    /// 未表态（`None`）不落盘：旧版本读到的 JSON 里不该多出一个 `auto_reconnect: null`。
    #[test]
    fn follow_global_reconnect_is_omitted_from_json() {
        let p = ConnectionProfile::new("a", "h", "u");
        let json = serde_json::to_string(&p).unwrap();
        assert!(!json.contains("auto_reconnect"), "{json}");

        let exp = ConnectionStore::default().export(0);
        let json = serde_json::to_string(&exp).unwrap();
        assert!(!json.contains("auto_reconnect"), "{json}");

        // 表了态就必须落盘（否则重开应用用户的选择就没了）
        let mut forced = ConnectionProfile::new("a", "h", "u");
        forced.auto_reconnect = Some(false);
        let json = serde_json::to_string(&forced).unwrap();
        assert!(json.contains(r#""auto_reconnect":false"#), "{json}");
    }

    /// 加 `auto_reconnect` 之前落盘的库必须照常读得进来（跟随全局）。
    #[test]
    fn legacy_json_without_the_reconnect_key_loads_as_follow_global() {
        let one: ConnectionProfile =
            serde_json::from_str(r#"{"id":"p1","name":"a","host":"h","port":22,"username":"u"}"#)
                .expect("单条旧格式必须能读");
        assert_eq!(one.auto_reconnect, None);

        let store: ConnectionStore = serde_json::from_str(
            r#"{"profiles":[{"id":"p1","name":"a","host":"h","port":22,"username":"u"}],"groups":[]}"#,
        )
        .expect("旧格式的库必须能读");
        assert_eq!(store.profiles[0].auto_reconnect, None);
        // 没表态的档位不落盘（表了态才写，见 `follow_global_reconnect_is_omitted`）
        let back = serde_json::to_string(&store.profiles[0]).unwrap();
        assert!(!back.contains("auto_reconnect"), "{back}");
    }

    /// 未表态（`None`）不落盘：旧版本读到的 JSON 里不该多出一个 `auto_reconnect: null`。
    #[test]
    fn summary_line() {
        let p = ConnectionProfile::new("a", "h", "u");
        assert_eq!(p.summary_line(), "u@h:22");
    }

    /// 复制连接：名称加 copy 后缀，其余字段一字不改，且紧跟原条目
    #[test]
    fn duplicate_profile_keeps_everything_but_name_and_id() {
        let mut store = ConnectionStore::default();
        let g_prod = store.ensure_group_by_name("生产组").expect("非空分组名");
        store.ensure_group_by_name("开发组");
        let mut src = ConnectionProfile::new("生产-01", "10.0.0.1", "root");
        src.port = 2222;
        src.key_path = Some("/home/u/.ssh/id_ed25519".into());
        src.stored_password = Some("dpapi:AAAA".into());
        src.group_id = Some(g_prod.clone());
        let src_id = src.id.clone();
        store.add_profile(src);
        store.add_profile(ConnectionProfile::new("生产-02", "10.0.0.2", "root"));

        let new_id = store.duplicate_profile(&src_id).expect("应返回新 id");
        assert_ne!(new_id, src_id, "副本必须有独立 id");

        let copy = store.profile(&new_id).expect("副本应存在");
        assert_eq!(copy.name, "生产-01copy");
        assert_eq!(copy.host, "10.0.0.1");
        assert_eq!(copy.port, 2222);
        assert_eq!(copy.username, "root");
        assert_eq!(copy.key_path.as_deref(), Some("/home/u/.ssh/id_ed25519"));
        assert_eq!(copy.stored_password.as_deref(), Some("dpapi:AAAA"));
        assert_eq!(copy.group_id, Some(g_prod), "分组保持不变");

        // 紧跟原条目插入（index 1），而不是追加到末尾
        assert_eq!(store.profiles[0].id, src_id);
        assert_eq!(store.profiles[1].id, new_id);
        assert_eq!(store.profiles.len(), 3);

        assert_eq!(store.duplicate_profile("不存在"), None);
    }

    /// 副本不得继承原条目的「最近访问」：否则它会凭空排在真正刚用过的那条前面。
    #[test]
    fn duplicate_profile_resets_recent_use() {
        let mut store = ConnectionStore::default();
        let mut src = ConnectionProfile::new("生产-01", "10.0.0.1", "root");
        src.last_used_at = 1_700_000_000;
        let src_id = src.id.clone();
        store.add_profile(src);

        let new_id = store.duplicate_profile(&src_id).expect("应返回新 id");
        assert_eq!(store.profile(&new_id).unwrap().last_used_at, 0);
        assert_eq!(store.profile(&src_id).unwrap().last_used_at, 1_700_000_000);
    }

    /// 最近访问排序：已用过的在前（按时刻倒序），从未用过的殿后（按名称升序）。
    #[test]
    fn by_recent_use_puts_used_first_and_is_deterministic() {
        let mut store = ConnectionStore::default();
        for (name, used) in [("b", 0u64), ("c", 500), ("a", 0), ("d", 900)] {
            let mut p = ConnectionProfile::new(name, "10.0.0.1", "root");
            p.last_used_at = used;
            store.add_profile(p);
        }

        let names: Vec<&str> = store
            .by_recent_use()
            .iter()
            .map(|p| p.name.as_str())
            .collect();
        assert_eq!(names, ["d", "c", "a", "b"]);
        // 重复调用结果稳定（命令窗口每次打开都重排，顺序不能跳）
        let again: Vec<&str> = store
            .by_recent_use()
            .iter()
            .map(|p| p.name.as_str())
            .collect();
        assert_eq!(names, again);
    }

    /// 打戳只改目标条目；未知 id 返回 false（调用方据此判断连接是否已被删掉）。
    #[test]
    fn touch_profile_updates_only_the_target() {
        let mut store = ConnectionStore::default();
        store.add_profile(ConnectionProfile::new("a", "10.0.0.1", "root"));
        store.add_profile(ConnectionProfile::new("b", "10.0.0.2", "root"));
        let b_id = store.profiles[1].id.clone();

        assert!(store.touch_profile(&b_id, 42));
        assert_eq!(store.profiles[0].last_used_at, 0);
        assert_eq!(store.profiles[1].last_used_at, 42);
        assert!(!store.touch_profile("不存在", 42));
    }

    /// 导出体不得带出任何口令：明文与系统凭据密文都不许出现在 JSON 里。
    #[test]
    fn export_never_carries_credentials() {
        let mut store = ConnectionStore::default();
        let g_prod = store.ensure_group_by_name("生产组").expect("非空分组名");
        let mut p = ConnectionProfile::new("prod-01", "10.0.0.1", "root");
        p.password = Some("plain-secret".into());
        p.stored_password = Some("dpapi-blob-AAAA".into());
        p.key_path = Some("/home/u/.ssh/id_ed25519".into());
        // 代理也带一份凭据：导出体里它必须同样**结构性**地装不下（F-10）
        p.proxy = crate::proxy::ProxyChoice::Custom(crate::proxy::ProxyConfig {
            host: "proxy.lan".into(),
            port: 3128,
            password_ref: Some("dpapi-blob-PROXY".into()),
            password: Some("plain-proxy-secret".into()),
            ..Default::default()
        });
        p.group_id = Some(g_prod);
        store.add_profile(p);

        let exp = store.export(1_757_900_000);
        let json = serde_json::to_string(&exp).unwrap();
        assert!(!json.contains("plain-secret"), "导出体泄漏了明文口令");
        assert!(!json.contains("dpapi-blob-AAAA"), "导出体泄漏了凭据密文");
        assert!(!json.contains("dpapi-blob-PROXY"), "导出体泄漏了代理密文");
        assert!(
            !json.contains("plain-proxy-secret"),
            "导出体泄漏了代理明文口令"
        );
        // 代理的「路线」必须完整保留（它就是这一条连接怎么出去的那半张图）
        let crate::proxy::ExportedProxyChoice::Custom(ep) = &exp.profiles[0].proxy else {
            panic!("自定义代理的路线应随导出体走");
        };
        assert_eq!(ep.host, "proxy.lan");
        assert_eq!(ep.port, 3128);
        // 非敏感字段必须完整保留
        assert_eq!(exp.format, EXPORT_FORMAT);
        assert_eq!(exp.version, EXPORT_VERSION);
        assert_eq!(exp.profiles.len(), 1);
        assert_eq!(
            exp.profiles[0].key_path.as_deref(),
            Some("/home/u/.ssh/id_ed25519")
        );
        assert_eq!(exp.profiles[0].group.as_deref(), Some("生产组"));
    }

    /// 合并导入：重复项跳过、新增项换 id、空主机/空用户名不导入。
    #[test]
    fn merge_import_dedups_and_reassigns_ids() {
        let mut store = ConnectionStore::default();
        let g_prod = store.ensure_group_by_name("生产组").expect("非空分组名");
        store.ensure_group_by_name("开发组");
        let mut same = ConnectionProfile::new("prod-01", "10.0.0.1", "root");
        same.group_id = Some(g_prod);
        let same_id = same.id.clone();
        store.add_profile(same);

        let exp = ConnectionExport {
            format: EXPORT_FORMAT.into(),
            version: EXPORT_VERSION,
            exported_at: 0,
            groups: vec!["生产组".into(), "预发组".into()],
            profiles: vec![
                // 与既有连接同 host+port+user（大小写不同也算同一台） 到 跳过
                ExportedProfile {
                    name: "prod-01-别人的名字".into(),
                    host: "10.0.0.1".into(),
                    port: 22,
                    username: "ROOT".into(),
                    key_path: None,
                    notes: String::new(),
                    group: Some("生产组".into()),
                    ..Default::default()
                },
                // 真正的新连接 到 导入
                ExportedProfile {
                    name: "dev-01".into(),
                    host: "10.0.0.9".into(),
                    port: 2222,
                    username: "dev".into(),
                    key_path: Some("/k".into()),
                    notes: "预发，改密码前先找我".into(),
                    group: Some("预发组".into()),
                    ..Default::default()
                },
                // 缺主机 到 跳过（不导入一条连不上的记录）
                ExportedProfile {
                    name: "broken".into(),
                    host: "  ".into(),
                    port: 22,
                    username: "u".into(),
                    key_path: None,
                    notes: String::new(),
                    group: None,
                    ..Default::default()
                },
            ],
        };

        let out = store.merge_import(&exp);
        assert_eq!(out.added, 1);
        assert_eq!(out.skipped, 2);
        assert_eq!(out.groups_added, 1, "只应新增「预发组」");
        assert!(!out.is_empty());

        assert_eq!(store.profiles.len(), 2);
        let imported = store.profiles.iter().find(|p| p.name == "dev-01").unwrap();
        assert_ne!(imported.id, same_id, "导入条目必须换新 id");
        assert_eq!(imported.port, 2222);
        assert!(
            imported.password.is_none(),
            "导出体不含口令，导入后不得带明文口令"
        );
        assert_eq!(
            imported.notes, "预发，改密码前先找我",
            "备注必须随连接一起搬家（导出 → 导入）"
        );
        assert!(
            imported.stored_password.is_none(),
            "导入体不含口令，必须为空"
        );
        assert!(store.groups.iter().any(|g| g.name == "预发组"));

        // 再导入同一份 到 全部重复，不再新增
        let again = store.merge_import(&exp);
        assert_eq!(again.added, 0);
        assert_eq!(again.groups_added, 0);
        assert!(again.is_empty());
    }

    /// 解析：本程序导出体 / 裸连接文件 / 格式不符 / 版本过新。
    #[test]
    fn parse_import_accepts_two_shapes_and_rejects_others() {
        let mut store = ConnectionStore::default();
        store.add_profile(ConnectionProfile::new("a", "h", "u"));
        let text = serde_json::to_string(&store.export(0)).unwrap();
        let exp = parse_import(&text).unwrap();
        assert_eq!(exp.profiles.len(), 1);

        // 裸文件（无 format/version）
        let raw = r#"{"profiles":[{"name":"x","host":"1.1.1.1","port":22,"username":"root",
            "stored_password":"should-be-dropped"}],"groups":["g"]}"#;
        let exp = parse_import(raw).unwrap();
        assert_eq!(exp.profiles.len(), 1);
        assert_eq!(exp.profiles[0].host, "1.1.1.1");

        assert!(matches!(
            parse_import(r#"{"format":"someone-else","profiles":[]}"#),
            Err(ImportError::UnsupportedFormat(_))
        ));
        assert!(matches!(
            parse_import(&format!(
                r#"{{"format":"{EXPORT_FORMAT}","version":{EXPORT_VERSION},"profiles":[]}}"#
            ))
            .map(|_| ()),
            Ok(())
        ));
        assert!(matches!(
            parse_import(&format!(
                r#"{{"format":"{EXPORT_FORMAT}","version":{},"profiles":[]}}"#,
                EXPORT_VERSION + 1
            )),
            Err(ImportError::VersionTooNew(_))
        ));
        assert!(matches!(parse_import("{"), Err(ImportError::BadJson(_))));
        assert!(matches!(
            parse_import(r#"{"foo":1}"#),
            Err(ImportError::UnknownShape)
        ));
    }

    /// 预检不改动 store 本体。
    #[test]
    fn import_preview_does_not_mutate() {
        let store = ConnectionStore::default();
        let exp = ConnectionExport {
            format: EXPORT_FORMAT.into(),
            version: EXPORT_VERSION,
            exported_at: 0,
            groups: vec![],
            profiles: vec![ExportedProfile {
                name: "n".into(),
                host: "h".into(),
                port: 22,
                username: "u".into(),
                key_path: None,
                notes: String::new(),
                group: None,
                ..Default::default()
            }],
        };
        let out = store.import_preview(&exp);
        assert_eq!(out.added, 1);
        assert!(store.profiles.is_empty(), "预检不得写入");
    }

    /// 拖拽移动：同组/未知分组不产生变更
    #[test]
    fn move_profile_to_group_rules() {
        let mut store = ConnectionStore::default();
        store.ensure_group_by_name("生产组");
        let g_dev = store.ensure_group_by_name("开发组").expect("非空分组名");
        let p = ConnectionProfile::new("a", "h", "u");
        let id = p.id.clone();
        store.add_profile(p);

        assert!(store.move_profile_to_group(&id, Some(&g_dev)));
        assert_eq!(
            store.profile(&id).unwrap().group_id.as_deref(),
            Some(g_dev.as_str())
        );

        // 目标分组 id 未登记 到 拒绝（不产生孤儿分组）
        assert!(!store.move_profile_to_group(&id, Some("不存在的分组 id")));
        assert_eq!(
            store.profile(&id).unwrap().group_id.as_deref(),
            Some(g_dev.as_str())
        );

        // 同组内拖放 到 无变更
        assert!(!store.move_profile_to_group(&id, Some(&g_dev)));

        // 拖到「未分组」
        assert!(store.move_profile_to_group(&id, None));
        assert_eq!(store.profile(&id).unwrap().group_id, None);
        assert!(!store.move_profile_to_group(&id, None));
    }

    /// 连接上下移动排序（连接树右键「上移 / 下移」）：
    /// 只在**同一分组内**挪一格，分组归属一个字都不动；越界与未知 id 一律拒绝。
    ///
    /// 关键用例是同组成员在平铺序列里**交叉**（拖拽换组、导入都会造成）：
    /// 那时按平铺下标 swap 会让被移动项一次跨过好几个同组成员，
    /// 位置查询也会与树上看到的序号对不上。
    #[test]
    fn move_profile_step_reorders_within_its_group_only() {
        let mut store = seeded_store();
        let g1 = store.add_group("A");
        let g2 = store.add_group("B");
        let a1 = push(&mut store, "a1", "10.0.0.1", "root");
        let b1 = push(&mut store, "b1", "10.0.1.1", "root");
        let a2 = push(&mut store, "a2", "10.0.0.2", "root");
        let a3 = push(&mut store, "a3", "10.0.0.3", "root");
        let free1 = push(&mut store, "free1", "8.8.8.8", "root");
        let free2 = push(&mut store, "free2", "8.8.4.4", "root");
        store.move_profile_to_group(&a1, Some(&g1));
        store.move_profile_to_group(&b1, Some(&g2));
        store.move_profile_to_group(&a2, Some(&g1));
        store.move_profile_to_group(&a3, Some(&g1));
        // 平铺次序：a1(g1) b1(g2) a2(g1) a3(g1) free1 free2 —— 同组成员是交叉的
        let names = |s: &ConnectionStore| {
            s.profiles_in_group(&g1)
                .into_iter()
                .map(|p| p.name.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(names(&store), ["a1", "a2", "a3"]);
        assert_eq!(
            store.profile_position(&a2),
            Some((1, 3)),
            "位置按同组成员算"
        );

        // a3 上移一格：只跨过 a2，不跨过平铺序列里夹在中间的 b1
        assert!(store.move_profile_step(&a3, -1));
        assert_eq!(names(&store), ["a1", "a3", "a2"]);
        assert_eq!(store.profile_position(&a3), Some((1, 3)));
        assert_eq!(
            store.profile(&a3).unwrap().group_id.as_deref(),
            Some(g1.as_str()),
            "排序不得改分组归属"
        );
        // 平铺序列里 b1 仍在原处，别的组一位不动
        assert_eq!(store.profiles_in_group(&g2).len(), 1);
        assert_eq!(store.profiles[1].id, b1);

        // 下移一格回到原样
        assert!(store.move_profile_step(&a3, 1));
        assert_eq!(names(&store), ["a1", "a2", "a3"]);

        // 已在首 / 末位 到 拒绝
        assert!(!store.move_profile_step(&a1, -1), "首位不得再上移");
        assert!(!store.move_profile_step(&a3, 1), "末位不得再下移");
        assert!(!store.move_profile_step("不存在的 id", 1));
        assert_eq!(names(&store), ["a1", "a2", "a3"], "被拒后次序不得改动");

        // 未分组连接（`group_id = None`）同样按「未分组」这一层排序
        assert!(store.move_profile_step(&free2, -1));
        let ungrouped: Vec<&str> = store
            .ungrouped()
            .into_iter()
            .map(|p| p.name.as_str())
            .collect();
        assert_eq!(ungrouped, ["free2", "free1"]);
        assert_eq!(store.profile_position(&free2), Some((0, 2)));
        assert_eq!(
            store.profile_position(&free1),
            Some((1, 2)),
            "未分组成员同样算「同级」，置灰依据才不会错位"
        );
        assert_eq!(store.profile_position(&a1), Some((0, 3)));
    }

    /// 旧格式（分组名字数组 + 连接按名归属）必须整体迁移成 id。
    #[test]
    fn legacy_name_based_groups_migrate_to_ids() {
        let legacy = r#"{
            "groups": ["生产组", "开发组"],
            "profiles": [
                {"id":"p1","name":"prod-01","host":"10.0.0.1","port":22,"username":"root","group":"生产组"},
                {"id":"p2","name":"dev-01","host":"10.0.0.9","port":22,"username":"dev","group":"摸鱼组"},
                {"id":"p3","name":"solo","host":"10.0.0.8","port":22,"username":"u"}
            ]
        }"#;
        let mut store: ConnectionStore = serde_json::from_str(legacy).expect("旧格式必须能读");
        assert!(store.normalize(), "旧格式应产生迁移变更");

        // 每个分组都有 id，且互不相同
        assert!(store.groups.iter().all(|g| !g.id.is_empty()));
        let ids: std::collections::HashSet<&str> =
            store.groups.iter().map(|g| g.id.as_str()).collect();
        assert_eq!(ids.len(), store.groups.len(), "分组 id 必须唯一");

        // 已知名 到 对应分组 id
        let g_prod = store.group_id_by_name("生产组").expect("生产组应存在");
        assert_eq!(
            store.profile("p1").unwrap().group_id.as_deref(),
            Some(g_prod.as_str())
        );
        // 未知分组名 到 补建分组，绝不丢归属
        let g_extra = store
            .group_id_by_name("摸鱼组")
            .expect("未知分组名应被补建");
        assert_eq!(
            store.profile("p2").unwrap().group_id.as_deref(),
            Some(g_extra.as_str())
        );
        // 无 group 字段 到 未分组
        assert_eq!(store.profile("p3").unwrap().group_id, None);

        // 迁移幂等：再跑一遍不应再产生变更
        assert!(!store.normalize(), "迁移必须幂等");
    }

    /// 备注字段（2026-09-18 新增）的两个兼容性要求：
    /// 1. **读**：老 `connections.json` 没有 `notes` 键，仍要能读进来（空备注）；
    /// 2. **写**：空备注不落盘（`skip_serializing_if`），不给绝大多数连接塞噪声键。
    #[test]
    fn notes_is_optional_and_empty_notes_are_not_written() {
        let legacy = r#"{"profiles":[{"id":"p1","name":"prod","host":"10.0.0.1",
            "port":22,"username":"root"}]}"#;
        let store: ConnectionStore = serde_json::from_str(legacy).expect("老库必须能读");
        assert_eq!(store.profile("p1").unwrap().notes, "");

        let mut with_notes = ConnectionProfile::new("prod", "10.0.0.1", "root");
        with_notes.notes = "跳板机：10.0.0.2".into();
        let json = serde_json::to_string(&with_notes).unwrap();
        assert!(json.contains("跳板机：10.0.0.2"), "有备注就必须落盘");

        with_notes.notes.clear();
        let json = serde_json::to_string(&with_notes).unwrap();
        assert!(
            !json.contains("\"notes\""),
            "空备注不该在 connections.json 里留下一个空串键"
        );
    }

    /// 悬空 `group_id`（分组已被删）回收为「未分组」，不留半残引用。
    #[test]
    fn normalize_reclaims_dangling_group_id() {
        let mut store = ConnectionStore::default();
        let mut p = ConnectionProfile::new("a", "h", "u");
        p.group_id = Some("不存在的分组 id".into());
        store.profiles.push(p);
        assert!(store.normalize());
        assert_eq!(store.profiles[0].group_id, None);
    }

    /// 导出体按「名」而不是 id：交换格式要能跨机读懂，也不能泄漏本机 id。
    #[test]
    fn export_serializes_group_names_not_ids() {
        let mut store = ConnectionStore::default();
        let g = store.ensure_group_by_name("生产组").expect("非空分组名");
        let mut p = ConnectionProfile::new("prod-01", "10.0.0.1", "root");
        p.group_id = Some(g.clone());
        store.add_profile(p);

        let exp = store.export(0);
        assert_eq!(exp.groups, vec!["生产组".to_string()]);
        assert_eq!(exp.profiles[0].group.as_deref(), Some("生产组"));
        let json = serde_json::to_string(&exp).unwrap();
        assert!(!json.contains(&g), "导出体不得带出本机分组 id");
    }

    /// 新版连接库（`groups` 为对象数组、连接用 `group_id`）当裸文件导入时，
    /// 分组名不能被静默丢掉。
    #[test]
    fn bare_new_format_store_keeps_group_names_on_import() {
        let raw = r#"{
            "schema_version": 2,
            "groups": [{"id":"g-1","name":"生产组"}],
            "profiles": [
                {"id":"p1","name":"prod-01","host":"10.0.0.1","port":22,"username":"root","group_id":"g-1"}
            ]
        }"#;
        let exp = parse_import(raw).expect("新版裸文件应可解析");
        assert_eq!(exp.groups, vec!["生产组".to_string()]);
        assert_eq!(exp.profiles[0].group.as_deref(), Some("生产组"));
    }

    // 连接树搜索（名称 / 主机 / 用户名）

    fn seeded_store() -> ConnectionStore {
        let mut s = ConnectionStore::with_seed();
        s.profiles.clear();
        s.groups.clear();
        s
    }

    fn push(store: &mut ConnectionStore, name: &str, host: &str, user: &str) -> String {
        let p = ConnectionProfile::new(name, host, user);
        let id = p.id.clone();
        store.add_profile(p);
        id
    }

    #[test]
    fn search_query_matches_name_host_and_username() {
        let p = ConnectionProfile::new("生产 Web", "10.0.3.77", "deploy");

        // 三个字段各自都要能命中
        assert!(SearchQuery::parse("生产").matches(&p), "名称应可命中");
        assert!(SearchQuery::parse("10.0.3").matches(&p), "主机应可命中");
        assert!(SearchQuery::parse("deploy").matches(&p), "用户名应可命中");
        // 大小写不敏感 + 两端空白自动去掉
        assert!(
            SearchQuery::parse("  DEPLOY ").matches(&p),
            "应大小写不敏感"
        );
        // `user@host` 连写：整段粘贴时要命中
        assert!(
            SearchQuery::parse("deploy@10.0.3.77").matches(&p),
            "user@host 连写形态必须命中"
        );
        assert!(
            !SearchQuery::parse("10.0.9").matches(&p),
            "不相关主机不得命中"
        );
        // 空查询放行一切；纯空白等同于空
        assert!(SearchQuery::parse("").matches(&p));
        assert!(SearchQuery::parse("   ").matches(&p));
        assert!(SearchQuery::parse("   ").is_empty());
    }

    #[test]
    fn visible_tree_filters_members_and_hides_empty_groups() {
        let mut store = seeded_store();
        let g_prod = store.add_group("生产");
        let g_dev = store.add_group("开发");
        let g_empty = store.add_group("空组");

        let prod = push(&mut store, "prod-web", "10.0.0.1", "root");
        let dev = push(&mut store, "dev-box", "192.168.1.5", "alice");
        let bare = push(&mut store, "独立机", "8.8.8.8", "bob");
        store.move_profile_to_group(&prod, Some(&g_prod));
        store.move_profile_to_group(&dev, Some(&g_dev));

        // 无搜索：三个分组都在（含空组），成员照旧
        let all = store.visible_tree(&SearchQuery::parse(""));
        assert_eq!(all.groups.len(), 3, "无搜索时空分组也要留着");
        assert_eq!(all.groups[2].id, g_empty);
        assert_eq!(all.ungrouped, vec![bare.clone()]);

        // 按主机搜：只剩命中成员，未命中的分组与未分组连接一并消失
        let hit = store.visible_tree(&SearchQuery::parse("192.168"));
        assert_eq!(hit.groups.len(), 1);
        assert_eq!(hit.groups[0].id, g_dev);
        assert_eq!(hit.groups[0].profile_ids, vec![dev.clone()]);
        assert!(hit.ungrouped.is_empty());
        assert!(!hit.is_empty());

        // 按分组名搜：整组保留（便于按分组一次捞全）
        let by_group = store.visible_tree(&SearchQuery::parse("生产"));
        assert_eq!(by_group.groups.len(), 1);
        assert_eq!(by_group.groups[0].profile_ids, vec![prod.clone()]);
        assert!(by_group.ungrouped.is_empty());

        // 什么都没搜到：必须能判定为空，UI 据此给提示而不是白屏
        let none = store.visible_tree(&SearchQuery::parse("zzz-不存在"));
        assert!(none.is_empty(), "无匹配时必须 is_empty");
        assert!(none.groups.is_empty() && none.ungrouped.is_empty());
    }

    #[test]
    fn visible_tree_ungrouped_matches_by_username() {
        let mut store = seeded_store();
        let a = push(&mut store, "机器甲", "10.1.1.1", "root");
        let _b = push(&mut store, "机器乙", "10.1.1.2", "ops");

        let hit = store.visible_tree(&SearchQuery::parse("ops"));
        assert!(hit.groups.is_empty());
        assert_eq!(hit.ungrouped.len(), 1);
        assert_ne!(hit.ungrouped[0], a, "命中的应是 ops 那台，而不是 root 那台");
    }

    /// 分组的新建位置：在锚点分组**之后**插入，锚点缺失时追加到末尾。
    /// 位置语义见 `add_group_after` 的注释（右键在哪个分组上，就建在哪个分组下面）。
    #[test]
    fn add_group_after_inserts_below_anchor() {
        let mut store = ConnectionStore::default();
        let a = store.add_group("A");
        let _c = store.add_group("C");
        let b = store.add_group_after("B", Some(&a));

        let names: Vec<&str> = store.groups.iter().map(|g| g.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["A", "B", "C"],
            "B 应插在锚点 A 之后，而不是末尾"
        );
        assert_eq!(store.groups[1].id, b);

        // 锚点已失效（id 不存在）到 追加到末尾，不 panic、也不丢分组
        let d = store.add_group_after("D", Some("不存在的分组 id"));
        assert_eq!(store.groups.last().map(|g| g.id.as_str()), Some(d.as_str()));
        assert_eq!(store.groups.last().map(|g| g.name.as_str()), Some("D"));
        assert_eq!(store.groups.len(), 4);
    }

    /// 分组改名：空名 / 与其它分组重名被拒（且名字不变），改成自己的名字算成功。
    #[test]
    fn rename_group_rules() {
        let mut store = ConnectionStore::default();
        let a = store.add_group("生产组");
        let b = store.add_group("开发组");

        assert!(
            store.rename_group(&a, "  线上组  "),
            "两侧空白应被裁剪后写入"
        );
        assert_eq!(store.group_name(&a), Some("线上组"));
        // 改名不换 id：连接归属与树上的展开态都按 id 走，故一律不受影响
        assert_eq!(store.group(&a).map(|g| g.id.as_str()), Some(a.as_str()));

        assert!(!store.rename_group(&a, "   "), "空名必须拒绝");
        assert!(!store.rename_group(&a, "开发组"), "与其它分组重名必须拒绝");
        assert_eq!(store.group_name(&a), Some("线上组"), "被拒后名字不得改动");

        assert!(store.rename_group(&b, "开发组"), "改成自己的名字应视为成功");
        assert!(!store.rename_group("不存在的 id", "x"));
        assert_eq!(store.groups.len(), 2);
    }

    /// 连接改名（2026-09-17 运行反馈「连接支持右键重命名和 F2 快捷键重命名」）：
    /// 只动 `name`，`id` 与分组归属一律不变（Tab / 指标 / SFTP 都按 id 关联）；
    /// 空名与不存在的 id 一律拒绝且不改动任何字段。
    #[test]
    fn rename_profile_keeps_id_and_group() {
        let mut store = ConnectionStore::default();
        let g = store.add_group("生产组");
        let mut p = ConnectionProfile::new("prod-01", "10.0.0.1", "root");
        p.group_id = Some(g.clone());
        let id = p.id.clone();
        store.add_profile(p);

        assert!(store.rename_profile(&id, "  线上-01  "), "两侧空白应被裁剪");
        let renamed = store.profile(&id).expect("改名不得换 id");
        assert_eq!(renamed.name, "线上-01");
        assert_eq!(
            renamed.group_id.as_deref(),
            Some(g.as_str()),
            "分组归属不变"
        );

        assert!(!store.rename_profile(&id, "   "), "空名必须拒绝");
        assert_eq!(
            store.profile(&id).map(|p| p.name.as_str()),
            Some("线上-01"),
            "被拒后名字不得改动"
        );
        assert!(!store.rename_profile("不存在的 id", "x"));
        assert_eq!(store.profiles.len(), 1);
    }

    /// 删除分组：连接一条都不删、全部回收到「未分组」；未登记的分组返回 `None`。
    #[test]
    fn remove_group_reclaims_members_to_ungrouped() {
        let mut store = ConnectionStore::default();
        let g = store.add_group("生产组");
        let keep = store.add_group("开发组");
        let mut p1 = ConnectionProfile::new("a", "h1", "u");
        p1.group_id = Some(g.clone());
        let mut p2 = ConnectionProfile::new("b", "h2", "u");
        p2.group_id = Some(g.clone());
        let mut p3 = ConnectionProfile::new("c", "h3", "u");
        p3.group_id = Some(keep.clone());
        store.profiles = vec![p1, p2, p3];

        assert_eq!(store.remove_group(&g), Some(2), "两条连接应被回收");
        assert!(store.group(&g).is_none(), "分组本身要删掉");
        assert_eq!(store.profiles.len(), 3, "连接一条都不能少");
        assert_eq!(store.ungrouped().len(), 2, "回收的连接必须落在未分组里");
        assert_eq!(store.profiles_in_group(&keep).len(), 1, "别的分组不受影响");

        // 第二个分组照常可删：它那一条成员同样被回收
        assert_eq!(store.remove_group(&keep), Some(1));
        assert!(store.profiles.iter().all(|p| p.group_id.is_none()));

        // 空分组也能删（没有成员可回收，计数为 0）
        let empty = store.add_group("空组");
        assert_eq!(store.remove_group(&empty), Some(0));

        assert_eq!(store.remove_group("不存在的 id"), None);
        assert!(!store.normalize(), "删除后必须自洽，normalize 不该再有变更");
    }

    /// 嵌套分组（2026-09-17 需求「连接组里还有组」）：
    /// 子分组挂进父分组、同级新建继承父、删除父分组时子分组上移一级。
    #[test]
    fn nested_subgroup_lifecycle() {
        let mut store = ConnectionStore::default();
        let prod = store.add_group("生产");
        let _dev = store.add_group("开发");
        let web = store
            .add_subgroup("网站", &prod)
            .expect("父分组存在时应建成功");
        let db = store.add_subgroup("数据库", &prod).unwrap();
        // 同级新建（在子分组上右键「新建分组」）继承锚点的父
        let cache = store.add_group_after("缓存", Some(&db));

        assert_eq!(
            store.group(&web).and_then(|g| g.parent.clone()),
            Some(prod.clone())
        );
        // 同级新建继承的是**锚点的父**：缓存建在数据库旁边，跟数据库一起挂在生产下
        assert_eq!(
            store.group(&cache).and_then(|g| g.parent.clone()),
            Some(prod.clone())
        );
        assert!(store.is_descendant_of(&web, &prod), "隔级也算后代");
        assert!(!store.is_descendant_of(&prod, &web), "方向不能反");

        // 子树连接数：父分组徽标 / 「全部连接」用的口径
        let p1 = push(&mut store, "web-1", "10.0.0.1", "root");
        let p2 = push(&mut store, "db-1", "10.0.0.2", "root");
        store.move_profile_to_group(&p1, Some(&web));
        store.move_profile_to_group(&p2, Some(&db));
        assert_eq!(
            store.profile_ids_in_subtree(&prod),
            vec![p1.clone(), p2.clone()]
        );
        assert_eq!(store.descendant_group_count(&prod), 3);

        // 删除父分组：子分组与孙分组都上移一级（挂到被删分组的父 = 根级），连接回收为未分组
        assert_eq!(store.remove_group(&prod), Some(0), "直接成员为 0");
        assert_eq!(
            store.group(&web).and_then(|g| g.parent.clone()),
            None,
            "子分组上移到根级"
        );
        assert_eq!(store.group(&db).and_then(|g| g.parent.clone()), None);
        assert_eq!(
            store.group(&cache).and_then(|g| g.parent.clone()),
            None,
            "孙分组跟它的父（数据库）一起上移到根级"
        );

        // 父分组不存在时 add_subgroup 必须给 None，而不是静默建到根级
        assert!(store.add_subgroup("孤儿", "不存在的 id").is_none());
    }

    /// 拖拽分组换父：正常换挂；挂到自身 / 自己的后代 / 不存在的目标一律拒绝（防环）。
    #[test]
    fn move_group_under_rules() {
        let mut store = ConnectionStore::default();
        let a = store.add_group("A");
        let b = store.add_subgroup("B", &a).unwrap();
        let c = store.add_subgroup("C", &b).unwrap();
        let d = store.add_group("D");

        // 正常：把 D 挂到 A 下
        assert!(store.move_group_under(&d, Some(&a)));
        assert_eq!(
            store.group(&d).and_then(|g| g.parent.clone()),
            Some(a.clone())
        );
        // 正常：把 D 从 A 下提到根级
        assert!(store.move_group_under(&d, None));
        assert_eq!(store.group(&d).and_then(|g| g.parent.clone()), None);

        // 拒绝：挂到自己
        assert!(!store.move_group_under(&a, Some(&a)));
        // 拒绝：挂到自己的后代（A 挂到 C 下面会成环，树上会丢整棵子树）
        assert!(!store.move_group_under(&a, Some(&c)));
        assert!(!store.move_group_under(&a, Some(&b)));
        // 拒绝：目标不存在 / 分组不存在
        assert!(!store.move_group_under(&a, Some("不存在的 id")));
        assert!(!store.move_group_under("不存在的 id", Some(&a)));

        // 拒绝后结构必须原样（B 仍在 A 下、C 仍在 B 下）
        assert_eq!(
            store.group(&b).and_then(|g| g.parent.clone()),
            Some(a.clone())
        );
        assert_eq!(
            store.group(&c).and_then(|g| g.parent.clone()),
            Some(b.clone())
        );
        assert!(!store.normalize(), "合法操作后必须自洽");
    }

    /// 分组上下移动排序（连接树右键「上移 / 下移」）：只在**同一层兄弟之间**换位，
    /// 不换父；整棵子树跟着一起搬；首/末位与未知 id 一律拒绝。
    #[test]
    fn move_group_step_reorders_siblings_and_carries_whole_subtree() {
        let mut store = seeded_store();
        let a = store.add_group("A");
        let a1 = store.add_subgroup("A1", &a).unwrap();
        let a2 = store.add_subgroup("A2", &a).unwrap();
        let _b = store.add_group("B");
        let c = store.add_group("C");

        let roots = |s: &ConnectionStore| {
            s.visible_tree(&SearchQuery::parse(""))
                .groups
                .iter()
                .map(|g| g.name.clone())
                .collect::<Vec<_>>()
        };
        let kids = |s: &ConnectionStore| {
            s.visible_tree(&SearchQuery::parse(""))
                .groups
                .iter()
                .find(|g| g.id == a)
                .map(|g| {
                    g.children
                        .iter()
                        .map(|c| c.name.clone())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        };

        assert_eq!(roots(&store), ["A", "B", "C"]);
        assert_eq!(store.group_position(&a), Some((0, 3)));
        assert_eq!(store.group_position(&c), Some((2, 3)));
        assert_eq!(
            store.group_position(&a2),
            Some((1, 2)),
            "位置按**同级**算，不是全库下标"
        );

        // 越界 / 未知 id 到 拒绝且不动结构
        assert!(!store.move_group_step(&a, -1), "同级首位不得再上移");
        assert!(!store.move_group_step(&c, 1), "同级末位不得再下移");
        assert!(!store.move_group_step("不存在的 id", 1));
        assert_eq!(roots(&store), ["A", "B", "C"]);

        // A 下移一格 到 [B, A, C]，两个子分组跟着一起走（不拆散、不插进别人的子树）
        assert!(store.move_group_step(&a, 1));
        assert_eq!(roots(&store), ["B", "A", "C"]);
        assert_eq!(
            store.group(&a1).and_then(|g| g.parent.clone()),
            Some(a.clone()),
            "子分组必须仍在 A 下"
        );
        assert_eq!(store.descendant_group_count(&a), 2);
        assert_eq!(kids(&store), ["A1", "A2"], "子分组次序不受根级换位影响");

        // 子分组自己上移：只动 A 内部次序，根级次序一位不动
        assert!(store.move_group_step(&a2, -1));
        assert_eq!(kids(&store), ["A2", "A1"]);
        assert_eq!(roots(&store), ["B", "A", "C"]);
        assert!(!store.move_group_step(&a2, -1), "已是同级首位");

        assert!(!store.normalize(), "排序后结构必须自洽");
    }

    /// 搜索 + 嵌套：命中的连接若藏在深层子分组里，祖先链必须保留；
    /// 分组名命中则整棵子树保留。
    #[test]
    fn visible_tree_keeps_ancestor_chain_and_group_subtree() {
        let mut store = seeded_store();
        let prod = store.add_group("生产");
        let web = store.add_subgroup("网站", &prod).unwrap();
        let p = push(&mut store, "web-1", "10.0.0.9", "root");
        store.move_profile_to_group(&p, Some(&web));

        // 深层命中：父分组「生产」自己没有直接成员、名字也不含关键词，但必须保留
        let hit = store.visible_tree(&SearchQuery::parse("10.0.0.9"));
        assert_eq!(
            hit.groups.len(),
            1,
            "祖先链必须保留 groups={:?}",
            hit.groups
        );
        assert_eq!(hit.groups[0].id, prod);
        assert!(
            hit.groups[0].profile_ids.is_empty(),
            "父分组直接成员没命中就不显示"
        );
        assert_eq!(hit.groups[0].children.len(), 1);
        assert_eq!(hit.groups[0].children[0].profile_ids, vec![p.clone()]);

        // 分组名命中（子分组名）：整棵子树保留，父分组链也保留
        let by_child_name = store.visible_tree(&SearchQuery::parse("网站"));
        assert_eq!(by_child_name.groups.len(), 1);
        assert_eq!(
            by_child_name.groups[0].children[0].profile_ids,
            vec![p.clone()]
        );

        // 分组名命中（父分组名）：全部子孙全量保留（与旧扁平语义一致）
        let by_parent_name = store.visible_tree(&SearchQuery::parse("生产"));
        assert_eq!(by_parent_name.groups.len(), 1);
        assert_eq!(by_parent_name.groups[0].children.len(), 1);
        assert_eq!(
            by_parent_name.groups[0].children[0].profile_ids,
            vec![p.clone()]
        );

        // 无搜索：树形结构完整呈现（根 + 子分组各就各位）
        let all = store.visible_tree(&SearchQuery::parse(""));
        assert_eq!(all.groups.len(), 1);
        assert_eq!(all.groups[0].id, prod);
        assert_eq!(all.groups[0].children.len(), 1);
        assert_eq!(all.groups[0].children[0].id, web);
    }

    /// 手工编辑 JSON 可能造出环 / 悬空 parent：normalize 必须断开成根级且幂等；
    /// 可见树在 normalize 之前也要能兜底渲染（不丢分组、不无限递归）。
    #[test]
    fn normalize_breaks_parent_cycles_and_visible_tree_survives_them() {
        let mut store = ConnectionStore::default();
        let a = store.add_group("A");
        let b = store.add_group("B");
        store.add_group("C");
        // 手工造环：A 到 B 到 A，C 指向不存在的父
        store.groups[0].parent = Some(b.clone());
        store.groups[1].parent = Some(a.clone());
        store.groups[2].parent = Some("不存在的父".into());

        // 先看渲染兜底（normalize 之前）：三个分组一个都不能丢
        let pre = store.visible_tree(&SearchQuery::parse(""));
        let total: usize = pre.groups.iter().map(count_nodes).sum();
        assert_eq!(total, 3, "环分组也得显示，绝不静默丢分组 pre={pre:?}");

        assert!(store.normalize(), "环与悬空引用必须被纠偏");
        assert!(
            store.groups.iter().all(|g| g.parent.is_none()),
            "断开成根级"
        );
        assert!(!store.normalize(), "纠偏必须幂等");

        let post = store.visible_tree(&SearchQuery::parse(""));
        assert_eq!(post.groups.len(), 3, "纠偏后三个分组都在根级");
    }

    /// 统计一棵可见子树的节点总数（含自身）
    fn count_nodes(g: &VisibleGroup) -> usize {
        1 + g.children.iter().map(count_nodes).sum::<usize>()
    }
}
