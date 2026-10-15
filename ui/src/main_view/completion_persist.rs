//! 命令历史落盘 + 按会话分桶（遗留项 1 与 2）。
//!
//! # 两个遗留项其实是同一件事
//!
//! | 遗留项 | 症状 | 本模块的落点 |
//! |---|---|---|
//! | ① 历史未落盘 | 关一次软件，昨天敲过的命令全没了，浮层只剩内置表 | [`persist`] 一族 |
//! | ② 多会话未分桶 | 两个标签互相"偷"对方的命令，A 机器上看得见 B 机器的历史 | [`buckets`] 一族 |
//!
//! 它们必须**一起**做。只做 ①（把单个 `CommandHistory` 存文件）等于把"偷"这件事
//! 从内存搬到了磁盘：下次启动一条会话桶接管全部历史，用户看到的还是混的。
//! 只做 ② 而不做 ①，则每次重启又回到"全糊在一起"。
//!
//! # 分桶键为什么是「连接指纹」而不是标签 id
//!
//! 标签 id 是**进程内**的临时编号：重启后重新分配，昨天的 `2` 可能是今天的 `5`。
//! 拿它当磁盘上的键，第二天每个桶都换了主人（历史张冠李戴，比不存更糟）。
//! 所以键取**连接指纹**：`协议 + 主机 + 端口 + 用户名`，只描述"这是哪台机器"。
//!
//! 也不取 `profile_id`：同一台机器完全可能存了多条 profile（改过端口、换过用户名），
//! 而那几份历史在用户眼里**是同一台机器的**。指纹让它们自然合流。
//! 反过来说，两条 profile 指向不同机器时必须分开 —— 指纹天然做到。
//!
//! 这与 `ai_persist` 的 key 策略相反（那边用会话 id）也**不是**不一致：
//! AI 会话是"一次对话"，它天然按创建时间排序、有标题、可以被用户删；命令历史是
//! "这台机器上敲过什么"，属于机器。两者挂的对象本来就不同。
//!
//! # 本机 shell 的桶键是 `LOCAL_SESSION_KEY`
//!
//! 本地 shell 没有主机名可言，用一个固定哨兵。它不是"没有归属"——
//! 它就是"本机"这一台。
//!
//! # 为什么不复用 `ai_persist` 的 `production_path()` 手法
//!
//! 这里**照样**用 `cfg!(test)` 隔离（见 [`production_path`]）：`MainView` 是所有 UI
//! 用例的脚手架，一推进时钟 `poll` 就会真写开发机上的 `cmd_history.json`。
//! 两条数据线在这一点上的风险完全相同，所以手法照抄、文件各存各的
//! （历史是"数据"，坏了不该连 AI 会话一起丢）。

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use rustshell_core::cmdhistory::{CommandHistory, HistoryEntry};

/// 文件格式版本。改结构时 +1（与 `ai_persist` 同一约定）。
const VERSION: u32 = 1;

/// 落盘节流间隔（毫秒）。
///
/// 比 AI 会话的 3 秒短得多：历史每次只改一条记录、整份文件也就几十 KB，
/// 而用户敲命令的节奏是"一条接一条"。等 3 秒会让「刚敲完的命令」
/// 在关窗前有极大概率还没写进去 —— 而这正是用户最可能马上重用的那一条。
const SAVE_MIN_GAP_MS: u64 = 1_000;

/// 本机 shell 的历史桶键。
///
/// 本地 shell 没有"连接到哪台机器"这回事，所以给它一个固定哨兵而不是空串：
/// 空串在 `BTreeMap` 里合法但含义模糊（"没归属"还是"本机"？），
/// 将来加"未识别远端"这类桶时容易撞在一起。
pub const LOCAL_SESSION_KEY: &str = "__local__";

/// 连接指纹：复用 core 的 [`profile_key`](rustshell_core::connections::profile_key)。
///
/// # 为什么不自己拼一个
///
/// 铁律 #50：同一个指标有两条链路时，口径必须只有一处。core 里已经有
/// `profile_key(host, port, username)`，它的归一化口径是"主机与用户名大小写不敏感、
/// 端口参与比较"——这**正是**历史分桶要的口径（`WEB` 与 `web` 是同一台机器，
/// 而 `22` 与 `2222` 不是）。
///
/// 自己再拼一遍的第一个后果不是"重复"，而是**两处会漂移**：哪天连接去重改了
/// 归一化规则（比如开始忽略结尾的点），历史桶还按老口径分，于是"同一台机器"
/// 在连接列表里是一台、在历史里是两台。所以这里只加一层前缀把命名空间与
/// 本地桶隔开。
///
/// **协议不进指纹**：本地桶由 [`LOCAL_SESSION_KEY`] 单独占着，
/// SSH 一律带 `ssh:` 前缀，两者不会相撞；再加一层协议名只是给"以后支持 telnet"
/// 留位置 —— 到时候加。
pub fn session_key_of(p: &rustshell_core::connections::ConnectionProfile) -> String {
    format!(
        "ssh:{}",
        rustshell_core::connections::profile_key(&p.host, p.port, &p.username)
    )
}

/// 启动时读回来的全部历史现场。
#[derive(Debug, Clone, Default)]
pub struct LoadedHistory {
    /// 按桶键索引（`BTreeMap`：文件逐字节可比，改了什么一眼看得出）
    pub buckets: BTreeMap<String, CommandHistory>,
}

impl LoadedHistory {
    /// 取某个桶（没有就现造一个空的）。
    ///
    /// 返回**拥有权**而不是引用：调用方（`MainView::completion_history_for`）要把它
    /// 塞进 `MainView` 的字段里，用 `&` 会借住整个 map。
    pub fn take_or_new(&mut self, key: &str) -> CommandHistory {
        self.buckets.remove(key).unwrap_or_default()
    }
}

/// 生产用的历史文件路径；`None` = 本进程不该碰真实文件。
///
/// **测试构建下一律 `None`**：理由与 `ai_persist::production_path` 一字不差 ——
/// `MainView` 是几乎所有 UI 用例的脚手架，用例一推进时钟（`advance_clock`）
/// `poll` 里的落盘就会真执行，砸的是开发机上**真实**的命令历史。
/// 用例要测落盘就走 [`MainView::cmd_history_flush`]，它收路径参数。
fn production_path() -> Option<std::path::PathBuf> {
    if cfg!(test) {
        None
    } else {
        Some(rustshell_platform::paths::cmd_history_path())
    }
}

/// 启动装载。读不到 / 读坏了 / 空文件都返回空表（一条历史都没有，正常开局）。
pub fn load_for_startup() -> LoadedHistory {
    match production_path() {
        Some(path) => load(&path),
        None => LoadedHistory::default(),
    }
}

/// 读历史。与其它配置同一手法：解析失败先留底成 `.broken-<ts>.bak` 再从头开始 ——
/// 静默清空用户数据是最糟的失败方式（用户会以为"这软件又丢东西了"）。
pub fn load(path: &Path) -> LoadedHistory {
    let Some(raw) = std::fs::read_to_string(path).ok() else {
        return LoadedHistory::default();
    };
    match serde_json::from_str::<HistoryFile>(&raw) {
        Ok(file) => {
            if file.version > VERSION {
                log::warn!(
                    "命令历史文件版本 {} 比本程序认识的 {VERSION} 新，按已知字段尽力读取: {}",
                    file.version,
                    path.display()
                );
            }
            normalize(file)
        }
        Err(e) => {
            log::warn!(
                "命令历史文件解析失败（{e}），已留底并从头开始: {}",
                path.display()
            );
            rustshell_core::configio::backup_broken_config(path, &raw);
            LoadedHistory::default()
        }
    }
}

/// 落盘。先写同目录 `.tmp` 再 `rename` 原子替换（与 `ai_persist::save` 同一手法）。
pub fn save(path: &Path, buckets: &BTreeMap<String, CommandHistory>) -> anyhow::Result<()> {
    let file = HistoryFile {
        version: VERSION,
        buckets: buckets
            .iter()
            .map(|(k, v)| (k.clone(), bucket_to_rec(v)))
            .collect(),
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let json = serde_json::to_string(&file)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// 整理成能直接接管的不变量形态。
///
/// 唯一一件事：**丢掉空桶**。空桶没有任何信息，留着会让文件随时间无限长大
/// （用户连过一百台机器就有一百个空桶），而它对用户毫无意义。
fn normalize(file: HistoryFile) -> LoadedHistory {
    let mut buckets = BTreeMap::new();
    for (key, rec) in file.buckets {
        let history = rec_to_bucket(rec);
        if history.is_empty() {
            continue;
        }
        buckets.insert(key, history);
    }
    LoadedHistory { buckets }
}

// ---- 存储格式 ----------------------------------------------------------------

#[derive(Serialize, Deserialize)]
#[serde(default)]
struct HistoryFile {
    version: u32,
    /// 桶键 → 该桶的记录。用 `BTreeMap` 而非数组：桶是**按键访问**的
    /// （开标签就查一次），不是按顺序遍历的。
    buckets: BTreeMap<String, BucketRec>,
}

impl Default for HistoryFile {
    fn default() -> Self {
        Self {
            version: VERSION,
            buckets: BTreeMap::new(),
        }
    }
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
struct BucketRec {
    entries: Vec<EntryRec>,
    next_seq: u64,
}

/// 一条记录的磁盘形态。
///
/// 字段与 [`HistoryEntry`] 一一对应，为什么还要另立一套：`HistoryEntry` 是
/// **core 的领域类型**，将来加字段（比如"上次在哪个目录执行的"）时，
/// 磁盘格式必须显式跟着改，而不是被 serde 顺手带上（与 `ai_persist` 同一理由）。
#[derive(Serialize, Deserialize)]
#[serde(default)]
struct EntryRec {
    text: String,
    count: u32,
    last_seq: u64,
}

impl Default for EntryRec {
    fn default() -> Self {
        Self {
            text: String::new(),
            count: 1,
            last_seq: 0,
        }
    }
}

// ---- 转换 --------------------------------------------------------------------

fn bucket_to_rec(h: &CommandHistory) -> BucketRec {
    BucketRec {
        entries: h
            .entries()
            .iter()
            .map(|e| EntryRec {
                text: e.text.clone(),
                count: e.count,
                last_seq: e.last_seq,
            })
            .collect(),
        next_seq: h.next_seq(),
    }
}

fn rec_to_bucket(rec: BucketRec) -> CommandHistory {
    let mut h = CommandHistory::new();
    for e in rec.entries {
        // 走 `push_restored` 而不是 `record`：`record` 会跑脱敏与去重，
        // 而这份数据**已经**是脱敏后的结果 —— 二次过一遍既浪费，
        // 又可能因为规则演进把老数据误杀（今天是凭据、明天改规则就不是了）。
        h.push_restored(HistoryEntry {
            text: e.text,
            count: e.count.max(1),
            last_seq: e.last_seq,
        });
    }
    // 序号分配器必须越过所有已恢复的 `last_seq`：撞上会让新记录与老记录
    // 并列"同样新"，淘汰时可能先踢掉刚敲的那条。
    h.set_next_seq(rec.next_seq.max(h.max_last_seq()));
    h
}

// ---- 落盘时机（MainView 侧） --------------------------------------------------

impl super::MainView {
    /// 当前会话的历史桶键（见模块文档：连接指纹，不是标签 id）。
    ///
    /// 没有活动标签时落到本机桶 —— **不返回 `None`**：历史采集（`EnterCallback`）
    /// 与候选计算都在"当前会话"的语境里发生，拿不到键就没有落点。
    /// 落到本机桶比"丢弃这次采集"好：用户至少还能在下次开本机 shell 时看到它。
    pub(super) fn current_history_key(&self) -> String {
        let Some(tab) = self.active_tab() else {
            return LOCAL_SESSION_KEY.to_string();
        };
        if tab.session.kind != rustshell_core::session::SessionKind::Ssh {
            return LOCAL_SESSION_KEY.to_string();
        }
        // SSH：按 profile_id 反查指纹。查不到 profile（profile 已被删 /
        // 是「快速连接」这类临时会话）时退回 profile_id 本身而不是本机桶 ——
        // 退回本机桶会让一台远程机器的历史混进本机历史里，那是**错归属**，
        // 比"分得粗一点"糟得多（错的数据不如少的数据）。
        match tab.session.profile_id.as_deref() {
            Some(pid) => self
                .conn_store
                .profile(pid)
                .map(session_key_of)
                .unwrap_or_else(|| format!("profile:{pid}")),
            None => LOCAL_SESSION_KEY.to_string(),
        }
    }

    /// 切标签时把当前会话的历史换进 `completion_history`。
    ///
    /// # 为什么是"存回 + 取出"而不是"每处直接用 map"
    ///
    /// `completion_history` 这个字段被十几处读写（采集 / 候选计算 / 断连清空），
    /// 全都改成 `self.history_map[key]` 意味着**每一处都要自己算键** ——
    /// 铁律 #81 的那类漏点（每加一个入口漏一处）。所以保留"当前会话的那一份"
    /// 这个单一字段，切标签时做一次搬运：换出（写回 map）→ 换入（从 map 取）。
    ///
    /// # 调用契约：必须在 `self.tabs_state.active` 被改**之后**调，且把旧键传进来
    ///
    /// 换出用的键是**旧会话**的，而旧会话的身份只存在于 `self.tabs_state.active` 里 ——
    /// 调用方一改 `active` 就再也算不出旧键了。所以由调用方在改 `active` 之前
    /// 把旧键算好传进来（见 `activate_session`）。
    /// 这个参数不是"多余的手续"：让本函数自己去算会恒等于新键，于是
    /// 「换出」这一步永远把**新会话**的空桶写回去、把旧会话的历史丢掉。
    pub(super) fn switch_history_bucket_to(&mut self, from_key: &str) {
        let to_key = self.current_history_key();
        if from_key == to_key {
            return;
        }
        let leaving = std::mem::take(&mut self.panel_ui.completion_history);
        if !leaving.is_empty() {
            self.panel_ui
                .cmd_history_buckets
                .insert(from_key.to_string(), leaving);
        } else {
            // 空桶不往 map 里塞（与 `normalize` 的丢空桶口径一致）：
            // 留着它只会让"连过一百台机器"变成一百个空桶。
            self.panel_ui.cmd_history_buckets.remove(from_key);
        }
        self.panel_ui.completion_history = self
            .panel_ui
            .cmd_history_buckets
            .remove(&to_key)
            .unwrap_or_default();
        // 换了桶 ⇒ 候选全变了（历史是数据源之一）。浮层若开着，前缀要重算。
        self.panel_ui.completion.prefix.clear();
    }

    /// 标记历史有改动，**下个 tick 就写**（`poll` 每 300ms 一轮）。
    ///
    /// 与 AI 会话那边不同，这里**不区分**"用户动作"与"流式增量"：历史每次落盘
    /// 都来自一次回车（用户动作），没有高频路径可言。所以只有一个
    /// [`SAVE_MIN_GAP_MS`] 的防抖，防止"连敲十条命令"变成连写十次文件。
    pub(super) fn cmd_history_touch(&mut self) {
        if !self.panel_ui.cmd_history_dirty {
            self.panel_ui.cmd_history_dirty = true;
            self.panel_ui.cmd_history_save_at_ms = now_ms().saturating_add(SAVE_MIN_GAP_MS);
        }
    }

    /// 到点就落盘（`poll` 每轮调用）。返回是否真的写了。
    ///
    /// **不要**据此 `cx.notify()`：写文件不改任何被渲染的东西（与
    /// `ai_chats_flush_if_due` 同一约定）。
    pub(super) fn cmd_history_flush_if_due(&mut self) -> bool {
        match production_path() {
            Some(path) => self.cmd_history_flush(&path, false),
            None => false,
        }
    }

    /// 立即落盘（退出钩子调它）。没改动时是空操作。
    ///
    /// ⚠️ 用例**别调它**：它写的是开发机真实的命令历史。
    /// 测落盘请用 `cmd_history_flush(&临时路径, true)`。
    pub fn cmd_history_flush_now(&mut self) -> bool {
        match production_path() {
            Some(path) => self.cmd_history_flush(&path, true),
            None => false,
        }
    }

    /// 落盘本体。`path` 是参数而不是写死：用例要把它导到临时目录。
    ///
    /// # 写之前必须"存回 + 取出"吗？不必 —— 但必须把**当前桶**也并进去
    ///
    /// `cmd_history_buckets` 里没有当前会话那一桶（切标签时才写回），
    /// 所以落盘前要现拼一份视图：map 的克隆 + 当前桶覆盖进去。
    /// 忘了这一步的症状很隐蔽：**别的机器的历史都存好了，只有当前这台不存**。
    pub(super) fn cmd_history_flush(&mut self, path: &Path, force: bool) -> bool {
        if !self.panel_ui.cmd_history_dirty {
            return false;
        }
        let now = now_ms();
        if !force && now < self.panel_ui.cmd_history_save_at_ms {
            return false;
        }
        let view = self.history_view_for_save();
        match save(path, &view) {
            Ok(()) => {
                self.panel_ui.cmd_history_dirty = false;
                true
            }
            Err(e) => {
                // 写失败（磁盘满 / 目录只读）就保持脏，下一轮再来 ——
                // 静默把历史丢掉比多试几次糟得多。
                self.panel_ui.cmd_history_save_at_ms = now.saturating_add(SAVE_MIN_GAP_MS);
                log::warn!("命令历史落盘失败（下轮重试）: {e}");
                false
            }
        }
    }

    /// 落盘视图：`cmd_history_buckets` + **当前会话那一桶**（见 `cmd_history_flush`）。
    fn history_view_for_save(&self) -> BTreeMap<String, CommandHistory> {
        let mut view = self.panel_ui.cmd_history_buckets.clone();
        let cur = self.current_history_key();
        if self.panel_ui.completion_history.is_empty() && !view.contains_key(&cur) {
            // 空桶不落盘（与 `normalize` 的丢空桶口径一致）：留着只会让文件长大
            view.remove(&cur);
        } else {
            view.insert(cur, self.panel_ui.completion_history.clone());
        }
        view
    }

    /// 把磁盘上读回来的桶灌进 `MainView`（启动时调一次）。
    ///
    /// 顺带把当前会话那一桶取出来放进 `completion_history` —— 不这么做的话，
    /// 启动后的第一条命令会从一个空桶开始记，而磁盘上那份要等到切标签才被看见。
    pub(super) fn install_history_buckets(&mut self, mut loaded: LoadedHistory) {
        let cur = self.current_history_key();
        self.panel_ui.completion_history = loaded.take_or_new(&cur);
        self.panel_ui.cmd_history_buckets = loaded.buckets;
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustshell_core::cmdhistory::HistoryEntry;

    /// 每个用例一个独立目录（落盘用例真写文件，共用目录会互相踩）。
    fn tmp_path(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rustshell-cmd-history-{}-{tag}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        dir.join("cmd_history.json")
    }

    fn hist(items: &[&str]) -> CommandHistory {
        let mut h = CommandHistory::new();
        for s in items {
            h.record(s);
        }
        h
    }

    fn map(entries: &[(&str, CommandHistory)]) -> BTreeMap<String, CommandHistory> {
        entries
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect()
    }

    // ---- 往返：格式对不对 ----

    /// 两个桶、各自的计数与序号都要原样回来。
    ///
    /// 判据不是"文件写成功了"，而是**逐字段**回来 —— 少一个字段的表现是
    /// 「历史还在、但计数全变成 1」，用户看到排序莫名其妙地变了。
    #[test]
    fn round_trip_keeps_both_buckets_and_their_counters() {
        let path = tmp_path("round-trip");
        let mut local = hist(&["ls -la", "ls -la", "df -h"]);
        // 手工把序号推高，验证分配器也存下来了
        local.set_next_seq(50);
        let remote = hist(&["docker ps"]);
        save(
            &path,
            &map(&[("__local__", local), ("ssh:web:22:root", remote)]),
        )
        .expect("落盘");

        let loaded = load(&path);
        assert_eq!(loaded.buckets.len(), 2, "两个桶都要在");
        let l = &loaded.buckets["__local__"];
        assert_eq!(l.len(), 2, "重复的 ls -la 只占一条");
        let ls = l.entries().iter().find(|e| e.text == "ls -la").unwrap();
        assert_eq!(ls.count, 2, "计数必须活着回来");
        assert_eq!(l.next_seq(), 50, "序号分配器必须活着回来");

        let r = &loaded.buckets["ssh:web:22:root"];
        assert_eq!(r.len(), 1);
        assert_eq!(r.entries()[0].text, "docker ps");
    }

    /// 空桶不落盘、读回来也不留。
    ///
    /// 留着空桶的后果不是"占一点空间" —— 是用户连过一百台机器之后，
    /// 文件里有一百个空壳，而每个都要被 `load` 解析一遍。
    #[test]
    fn empty_buckets_are_dropped_on_save_and_load() {
        let path = tmp_path("empty-buckets");
        save(
            &path,
            &map(&[
                ("__local__", hist(&["a"])),
                ("ssh:gone:22:root", CommandHistory::new()),
            ]),
        )
        .expect("落盘");

        let loaded = load(&path);
        assert_eq!(
            loaded.buckets.len(),
            1,
            "空桶必须被丢掉：{:?}",
            loaded.buckets.keys()
        );
        assert!(loaded.buckets.contains_key("__local__"));
    }

    /// 一条记录都没有时：空表，不 panic，也不建出奇怪的东西。
    #[test]
    fn missing_file_yields_an_empty_map() {
        let path = tmp_path("missing");
        let _ = std::fs::remove_file(&path);
        let loaded = load(&path);
        assert!(loaded.buckets.is_empty());
    }

    /// 坏文件：留底 + 从头开始，绝不 panic。
    #[test]
    fn corrupt_file_is_backed_up_and_starts_fresh() {
        let path = tmp_path("corrupt");
        std::fs::write(&path, "{ 这不是 JSON").expect("写坏文件");
        let loaded = load(&path);
        assert!(loaded.buckets.is_empty());
        let names: Vec<String> = std::fs::read_dir(path.parent().unwrap())
            .expect("读目录")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert!(
            names.iter().any(|n| n.contains("broken")),
            "坏文件没留底，用户无从查「历史为什么全没了」: {names:?}"
        );
    }

    /// 分配器落后于已恢复条目时要被抬起来。
    ///
    /// 落后的表现很隐蔽：新敲的命令与某条老记录**并列同样新**，
    /// 淘汰（`MAX_HISTORY` 满员）时可能先踢掉刚敲的那条。
    #[test]
    fn the_seq_allocator_is_lifted_above_restored_entries() {
        let path = tmp_path("seq-lift");
        std::fs::write(
            &path,
            r#"{"version":1,"buckets":{"__local__":{"entries":[
                {"text":"a","count":1,"last_seq":99}],"next_seq":1}}}"#,
        )
        .expect("写文件");
        let loaded = load(&path);
        assert_eq!(
            loaded.buckets["__local__"].next_seq(),
            99,
            "分配器必须越过已有的最大序号"
        );
    }

    /// 装载**不重跑脱敏**（`push_restored` 的契约）。
    ///
    /// 磁盘上的数据已经过一遍 `record` 的脱敏。再跑一遍不只是浪费：
    /// 脱敏规则随版本演进，今天判为凭据、改天放宽之后，**老数据仍然进不来** ——
    /// 用户会发现"以前存下的命令怎么没了"。
    #[test]
    fn loading_does_not_re_run_the_sensitive_filter() {
        let path = tmp_path("no-refilter");
        // 直接构造一条"如果重跑就会被挡掉"的记录（形态像口令）
        let mut h = CommandHistory::new();
        h.push_restored(HistoryEntry {
            text: "mysql -pSecret123".to_string(),
            count: 1,
            last_seq: 1,
        });
        save(&path, &map(&[("__local__", h)])).expect("落盘");

        let loaded = load(&path);
        assert_eq!(
            loaded.buckets["__local__"].entries()[0].text,
            "mysql -pSecret123",
            "装载路径不许二次脱敏（那是采集路径的事）"
        );
    }

    /// 版本号比本程序新：未知字段被 serde 忽略，已知字段照读。
    #[test]
    fn a_newer_file_version_still_loads_the_known_fields() {
        let path = tmp_path("newer");
        std::fs::write(
            &path,
            r#"{"version":99,"future":1,"buckets":{"__local__":{"entries":[
                {"text":"git status","count":3,"last_seq":7,"extra":"x"}],"next_seq":8}}}"#,
        )
        .expect("写文件");
        let loaded = load(&path);
        let h = &loaded.buckets["__local__"];
        assert_eq!(h.entries()[0].text, "git status");
        assert_eq!(h.entries()[0].count, 3);
    }

    // ---- 分桶键（遗留项 2 的判据） ----

    /// 指纹口径必须与 core 的连接去重口径**同一把尺子**（铁律 #50）。
    ///
    /// 大小写不敏感、端口参与比较：`WEB:22` 与 `web:22` 是同一台机器，
    /// 而 `web:2222` 不是。
    #[test]
    fn session_key_matches_the_connection_dedup_ruler() {
        let p =
            |host: &str, port: u16, user: &str| rustshell_core::connections::ConnectionProfile {
                id: "x".into(),
                name: "x".into(),
                host: host.into(),
                port,
                username: user.into(),
                password: None,
                stored_password: None,
                key_path: None,
                notes: String::new(),
                group_id: None,
                legacy_group: None,
                last_used_at: 0,
                tags: Vec::new(),
                auto_reconnect: None,
                forwards: Vec::new(),
                proxy: rustshell_core::proxy::ProxyChoice::default(),
                session_log: None,
            };
        assert_eq!(
            session_key_of(&p("WEB", 22, "Root")),
            session_key_of(&p("web", 22, "root")),
            "大小写不同是同一台机器"
        );
        assert_ne!(
            session_key_of(&p("web", 22, "root")),
            session_key_of(&p("web", 2222, "root")),
            "端口不同不是同一台机器"
        );
        assert_ne!(
            session_key_of(&p("web", 22, "root")),
            session_key_of(&p("web", 22, "deploy")),
            "用户名不同要分开（两边的命令习惯常常不同）"
        );
        // 与 core 的 `profile_key` 同源 ⇒ 前缀 + 同一个串
        assert_eq!(
            session_key_of(&p("web", 22, "root")),
            format!(
                "ssh:{}",
                rustshell_core::connections::profile_key("web", 22, "root")
            )
        );
    }

    /// 本机桶键与任何一个 SSH 指纹都不会相撞。
    #[test]
    fn the_local_key_cannot_collide_with_an_ssh_fingerprint() {
        assert!(!LOCAL_SESSION_KEY.starts_with("ssh:"));
        // 反向：一个"看起来很怪"的主机名也不会撞上本机桶
        assert_ne!(
            format!(
                "ssh:{}",
                rustshell_core::connections::profile_key("__local__", 22, "x")
            ),
            LOCAL_SESSION_KEY
        );
    }

    // ---- 落盘时机（MainView 侧，真视图） ----

    /// 真相：记录一条命令之后，**它真的被写进文件、也读得回来**。
    /// 上一条覆盖"格式对不对"，这条覆盖"接线通不通"。
    #[gpui::test]
    fn the_live_view_persists_and_restores_its_history(cx: &mut gpui::TestAppContext) {
        let (main, vcx) = crate::main_view::dialogs::tests::build_main(cx);
        let path = tmp_path("live-view");
        let _ = std::fs::remove_file(&path);

        main.update_in(vcx, |m, _w, _cx| {
            m.panel_ui
                .completion_history
                .record("systemctl status nginx");
            // 与 `record_history_line` 同款：记下了才标脏
            assert!(
                !m.panel_ui.cmd_history_dirty,
                "还没标脏（模拟 record 之前）"
            );
            m.cmd_history_touch();
            assert!(m.panel_ui.cmd_history_dirty, "标脏之后必须为真");
            // ⚠️ 必须传临时路径：`cmd_history_flush_now()` 写的是开发机真实的历史文件。
            assert!(m.cmd_history_flush(&path, true), "有改动却没写");
            assert!(!m.cmd_history_flush(&path, true), "没改动还重写了一次文件");
        });

        let loaded = load(&path);
        let key = main.read_with(vcx, |m, _| m.current_history_key());
        let h = loaded
            .buckets
            .get(&key)
            .unwrap_or_else(|| panic!("按当前会话键 {key} 找不到桶：{:?}", loaded.buckets.keys()));
        assert_eq!(h.entries()[0].text, "systemctl status nginx");
    }

    /// 节流：连敲两条命令不该写两次文件。
    #[gpui::test]
    fn writing_is_throttled_within_the_window(cx: &mut gpui::TestAppContext) {
        let (main, vcx) = crate::main_view::dialogs::tests::build_main(cx);
        let path = tmp_path("throttle");
        let _ = std::fs::remove_file(&path);

        main.update_in(vcx, |m, _w, _cx| {
            m.panel_ui.completion_history.record("a");
            m.cmd_history_touch();
            assert!(!m.cmd_history_flush(&path, false), "节流窗口内不该写");
            assert!(!path.exists(), "节流窗口内不该有文件");
            // force = true 是退出钩子那条路，必须无视节流立刻写
            assert!(m.cmd_history_flush(&path, true), "退出钩子必须能立刻写");
        });
    }

    /// 落盘视图必须把**当前会话那一桶**并进去。
    ///
    /// 漏掉这一步的症状最隐蔽：别的机器的历史都存好了，**只有当前这台不存** ——
    /// 因为 `cmd_history_buckets` 里压根没有当前桶（切标签时才写回）。
    #[gpui::test]
    fn the_current_bucket_is_included_in_what_gets_written(cx: &mut gpui::TestAppContext) {
        let (main, vcx) = crate::main_view::dialogs::tests::build_main(cx);
        let path = tmp_path("current-bucket");
        let _ = std::fs::remove_file(&path);

        main.update_in(vcx, |m, _w, _cx| {
            // 先塞一个"别的会话"的桶进去（模拟从别的标签切过来）
            let mut other = CommandHistory::new();
            other.record("from-other-machine");
            m.panel_ui
                .cmd_history_buckets
                .insert("ssh:other:22:root".into(), other);
            // 当前会话记一条
            m.panel_ui.completion_history.record("from-this-machine");
            m.cmd_history_touch();
            assert!(m.cmd_history_flush(&path, true));
        });

        let loaded = load(&path);
        assert!(
            loaded.buckets.contains_key("ssh:other:22:root"),
            "别的会话的桶要存：{:?}",
            loaded.buckets.keys()
        );
        assert_eq!(
            loaded.buckets.len(),
            2,
            "当前会话的桶也必须存：{:?}",
            loaded.buckets.keys()
        );
        let cur = main.read_with(vcx, |m, _| m.current_history_key());
        assert_eq!(loaded.buckets[&cur].entries()[0].text, "from-this-machine");
    }

    /// 切桶：旧会话的历史换出去、新会话的换进来（遗留项 2 的核心行为）。
    ///
    /// 这里直接调 `switch_history_bucket_to`，并**手工把 `active` 指到一个假会话**，
    /// 而不是走 `activate_session` —— 后者需要真标签真会话（要建 PTY），
    /// 而本条要验的是**搬运本身**。`activate_session` 的接线段由
    /// 下面那条源码护栏盯着。
    ///
    /// ⚠️ 模拟 `active` 时用的是**真实的 `TabEntry`**（`SessionHandle::pending`），
    /// 不是往 `active` 里塞一个裸字符串：`current_history_key()` 会去 `tabs` 里
    /// 查这个 id，查不到就退回本机桶 —— 那样这条测试会在"两个键都是本机桶"
    /// 的假前提下变绿，什么也没验到。
    #[gpui::test]
    fn switching_buckets_swaps_both_sides(cx: &mut gpui::TestAppContext) {
        let (main, vcx) = crate::main_view::dialogs::tests::build_main(cx);
        main.update_in(vcx, |m, _w, _cx| {
            // ① 当前（无标签 ⇒ 本机桶）记一条
            let local_key = m.current_history_key();
            assert_eq!(local_key, LOCAL_SESSION_KEY, "没有标签时就是本机桶");
            m.panel_ui.completion_history.record("local-cmd");

            // ② 造一个真标签（SSH，profile 指向一个测试连接）并把 active 指过去
            let pid = ensure_test_profile(m);
            let sid = "s-remote".to_string();
            m.tabs_state.tabs.push(super::super::TabEntry {
                session: rustshell_core::session::SessionHandle::pending(
                    sid.clone(),
                    rustshell_core::session::SessionKind::Ssh,
                    "web".into(),
                    Some(pid.clone()),
                ),
                terminal: None,
                scroll: None,
                connect_error: None,
                connect_started: None,
                dead: None,
                retry: None,
            });
            m.tabs_state.active = Some(sid);

            // ③ 换桶（旧键是本机桶，要手工传 —— 见函数文档的调用契约）
            let remote_key = m.current_history_key();
            assert_ne!(remote_key, local_key, "远端标签的键必须与本机桶不同");
            m.switch_history_bucket_to(&local_key);
            assert!(
                m.panel_ui.completion_history.is_empty(),
                "新会话还没敲过东西，进来时应当是空的"
            );
            assert_eq!(
                m.panel_ui.cmd_history_buckets[&local_key].entries()[0].text,
                "local-cmd",
                "旧会话的历史必须被换出去，否则切回去就丢了"
            );

            // ④ 远端记一条
            m.panel_ui.completion_history.record("remote-cmd");

            // ⑤ 切回本机（模拟 `active` 被改回 None）
            m.tabs_state.active = None;
            m.switch_history_bucket_to(&remote_key);
            assert_eq!(
                m.panel_ui.completion_history.entries()[0].text,
                "local-cmd",
                "切回来必须看见本机自己的历史"
            );
            assert!(
                !m.panel_ui
                    .completion_history
                    .entries()
                    .iter()
                    .any(|e| e.text == "remote-cmd"),
                "远端的历史不许跟过来（这就是遗留项 2 要修的那个 bug）"
            );
            assert_eq!(
                m.panel_ui.cmd_history_buckets[&remote_key].entries()[0].text,
                "remote-cmd"
            );
        });
    }

    /// 往 `conn_store` 里确保有一条测试连接，返回它的 id。
    ///
    /// 目的是让 `current_history_key()` 能 resolve 出一个**非本机**的指纹 ——
    /// 见 `switching_buckets_swaps_both_sides` 的说明。
    fn ensure_test_profile(m: &mut super::super::MainView) -> String {
        let p = rustshell_core::connections::ConnectionProfile {
            id: "test-remote".into(),
            name: "web".into(),
            host: "web".into(),
            port: 22,
            username: "root".into(),
            password: None,
            stored_password: None,
            key_path: None,
            notes: String::new(),
            group_id: None,
            legacy_group: None,
            last_used_at: 0,
            tags: Vec::new(),
            auto_reconnect: None,
            forwards: Vec::new(),
            proxy: rustshell_core::proxy::ProxyChoice::default(),
            session_log: None,
        };
        if m.conn_store.profile("test-remote").is_none() {
            m.conn_store.profiles.push(p);
        }
        "test-remote".to_string()
    }

    /// 空桶不进 map（与 `normalize` 的丢空桶口径一致）。
    ///
    /// ⚠️ 建用例时踩过一次**假绿**：首版直接把 `m.current_history_key()` 当 `from_key`
    /// 传进去 —— 无标签时那个键恒等于本机桶，而 `switch_history_bucket_to` 开头就有
    /// `from_key == to_key ⇒ return`，函数**根本没走到插入那一步**，
    /// `contains_key` 自然为假。`leaving` 与 `to_key` 都取本机桶，去掉空桶分支
    /// 它照样绿（变异验证抓到的）。所以这里必须让**两个键不同**：`from_key` 是
    /// 一个没人用过的远端键、`to_key` 是本机键（`active` 为 `None`）。
    #[gpui::test]
    fn switching_away_from_an_empty_bucket_leaves_no_trace(cx: &mut gpui::TestAppContext) {
        let (main, vcx) = crate::main_view::dialogs::tests::build_main(cx);
        main.update_in(vcx, |m, _w, _cx| {
            let local = m.current_history_key();
            let remote = "ssh:test.invalid:22:root".to_string();
            assert_ne!(
                remote, local,
                "用例前提：两个键必须不同，否则函数会在 `from_key == to_key` 处提前返回"
            );
            assert!(m.panel_ui.completion_history.is_empty());
            m.switch_history_bucket_to(&remote);
            assert!(
                !m.panel_ui.cmd_history_buckets.contains_key(&remote),
                "空桶不该在 map 里留下壳：{:?}",
                m.panel_ui.cmd_history_buckets.keys()
            );
        });
    }
}
