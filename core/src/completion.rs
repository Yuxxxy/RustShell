//! 补全候选引擎（C-1）。
//!
//! # 定位（对照调研文档 §2.5(c) 的 `carapace` 分层）
//!
//! **纯数据进、纯候选出**：不依赖终端、不依赖 UI、不做 I/O、不读系统时钟。
//! 这样 C-2 的行内上下文、C-3 的远端 `compgen`、C-5 的 AI 候选才能全部
//! 汇到同一个引擎里，靠 [`CandidateSource`] 标签区分来源 —— 浮层只认这一个模型，
//! 不需要为每个来源各写一套渲染。
//!
//! # 「只补后缀，不重写整行」（调研文档 §5.1 的硬规矩）
//!
//! 候选计算的是**补齐部分**（[`Candidate::suffix`]），不是整行替换。
//! [`suffix_for`] 是这条规矩的唯一落点：它拿"已输入前缀"和"候选全文"求差。
//! 为什么不在这里直接算"要把整行改成什么"：那需要回退删除（`\x7f` × n），
//! 会和远端 readline 的编辑缓冲抢屏幕，宽字符 / 组合记号下必错位。
//!
//! # 打分：为什么不用 `fuzzy` 打分器
//!
//! Warp 用 exact / prefix / fuzzy 三档。本层同样三档，但用的是**位置加权**的
//! 朴素打分（[`score`]），因为：① 补全场景里"前缀"是压倒性的主流（用户从头打
//! 命令名），模糊命中的命令几乎总是更差的选择；② 朴素打分可以完全确定性地单测，
//! 而通用的模糊打分器（如 fzf 那套）要复现它的边界行为代价很高。
//! 打分**只决定排序**，不决定"是否出现在候选里" —— 只要匹配就列，排序差一点
//! 比"该有的候选被藏起来"危害小得多。

use crate::cmdhistory::{builtin_commands, BuiltinCommand, CommandHistory, HistoryEntry};

/// 候选来源（浮层用它打标签，也是用户判断"这条可不可信"的依据）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CandidateSource {
    /// 用户自己的命令库（快捷命令）
    Library,
    /// 本会话用过的历史
    History,
    /// 内置常见命令表
    Builtin,
}

impl CandidateSource {
    /// 标签文案（i18n key；浮层渲染时走 `t()`）。
    pub fn label(self) -> &'static str {
        match self {
            CandidateSource::Library => "库",
            CandidateSource::History => "历史",
            CandidateSource::Builtin => "内置",
        }
    }

    /// 同分时的排序权重（小的排前面）。
    ///
    /// 次序 = 可信度：用户自己配的库 > 自己用过的历史 > 通用内置表。
    /// 这与 FinalShell 的"频率学习"同向，但更简单 —— 库是显式意图，
    /// 永远压过统计推断。
    fn tiebreak(self) -> u8 {
        match self {
            CandidateSource::Library => 0,
            CandidateSource::History => 1,
            CandidateSource::Builtin => 2,
        }
    }
}

/// 一条补全候选。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// 候选全文（会被完整展示）
    pub text: String,
    /// **要写进 PTY 的补齐部分**（见模块文档；空串 = 已完全匹配，采纳无意义）
    pub suffix: String,
    /// 说明文案（内置命令的中文说明 / 历史的使用次数）
    pub detail: Option<String>,
    /// 来源
    pub source: CandidateSource,
    /// 排序分（越大越靠前）
    pub score: i32,
}

impl Candidate {
    /// 采纳这条候选时是否还有字节要写。
    ///
    /// `false` 的候选（前缀已等于全文）**不该出现在浮层里** —— 采纳它等于
    /// 什么都不做，用户会以为"点了个没反应的按钮"。过滤发生在 [`candidates`]。
    pub fn is_actionable(&self) -> bool {
        !self.suffix.is_empty()
    }
}

/// 浮层候选上限。
///
/// 10 条：一屏（浮层高 ~10 行）之内看完，不需要用户滚动 —— 补全浮层一旦要滚动，
/// 键盘导航就变得难用（用户得先想"我要的那条在第几屏"）。
pub const MAX_CANDIDATES: usize = 10;

/// 生成候选（C-1 的全部逻辑）。
///
/// `prefix` 是**当前已输入的整行文本**（已由引擎取到；C-1 阶段可能为空串 ——
/// 那就列"库 + 历史 + 内置"里最常用的一批）。`history` 与 `library` 由调用方给
/// （历史挂会话，库是全局用户数据）。
///
/// 输出已排序、已截断到 [`MAX_CANDIDATES`]、已滤掉"无后缀可补"的条目。
pub fn candidates(
    prefix: &str,
    history: &CommandHistory,
    library: &[LibraryCommand],
) -> Vec<Candidate> {
    // 「精确命令行取不到就退回网格」这条策略的**唯一落点**（见 [`exact_prefix`]，
    // 铁律 #81）。⚠️ 别把 `shell_command_line` 的判断挪到调用方 ——
    // 那等于每多一个入口就多漏一处的机会。
    candidates_with_exact(prefix, None, history, library)
}

/// 与 [`candidates`] 同源，但允许调用方给出**更准的一把尺子**。
///
/// `exact` = shell integration（`OSC 633;E`）报上来的精确命令行。
///
/// # 为什么必须在这里合流，而不是在浮层里做两套
///
/// "取哪一份文本当前缀"决定了**候选长什么样**（[`suffix_for`] 直接拿它求差），
/// 所以它是引擎语义的一部分，不是渲染细节。若让浮层先挑好再传进来：
/// ① 以后每加一个候选来源（C-2 行内上下文 / C-5 AI）都要重新挑一遍；
/// ② 更糟的是**两者会不一致** —— 浮层按精确行算候选、历史采集按网格行存原文，
/// 同一时刻两条路看到的"当前命令"不是同一条。
///
/// # 何时退回网格（H1 启发式）
///
/// `exact` 只在远端支持 `OSC 633;E` 时才有值，而绝大多数环境本来就不发
/// （见 `shell_integration_active`）。所以退回是常态、不是异常：
///
/// | 条件 | 取舍 |
/// |---|---|
/// | `exact` 为 `None` | 用 `prefix`（网格 + H1） |
/// | `exact` 非空但**不是** `prefix` 的前缀 | 用 `prefix` —— 网格行是**用户眼前那一行**，可信度更高 |
/// | `exact` 为空串 | 用 `prefix` —— 空命令行多半是"新 prompt 刚下来，还没敲"，网格上那一行同理 |
///
/// 第三个条件还有一层：`633;E` 是**执行时**上报的，一条命令跑完后它会一直留着旧值
/// 直到下一条被敲出来。拿它当"当前输入行"会让补全一直按上一条命令算 —— 所以只有
/// 当它确实延续了用户正在打的那一行（第 2 条）才敢用。
pub fn candidates_with_exact(
    prefix: &str,
    exact: Option<&str>,
    history: &CommandHistory,
    library: &[LibraryCommand],
) -> Vec<Candidate> {
    let prefix = exact_prefix(prefix, exact);
    let needle = prefix.trim();
    let mut out: Vec<Candidate> = Vec::new();

    // ① 库
    for c in library {
        if let Some(sc) = score(&c.script, needle) {
            out.push(mk(
                &c.script,
                Some(c.label.clone()),
                CandidateSource::Library,
                sc,
                needle,
            ));
        }
    }
    // ② 历史（可能来自多个桶 —— 见 `history_rows`）
    for e in history_rows(std::slice::from_ref(&history)) {
        if let Some(sc) = score(&e.text, needle) {
            out.push(mk(
                &e.text,
                Some(history_detail(&e)),
                CandidateSource::History,
                sc,
                needle,
            ));
        }
    }
    // ③ 内置表（命令名与说明都可作为匹配面：搜"磁盘"也该能找到 `df`）
    for b in builtin_commands() {
        if let Some(sc) = score_builtin(b, needle) {
            out.push(mk(
                b.name,
                Some(crate::i18n::t(b.desc).to_string()),
                CandidateSource::Builtin,
                sc,
                needle,
            ));
        }
    }

    // 排序：分高者前；同分按来源权重；再同分按文本（确定性 —— 不给"顺序随机跳"留缝）
    out.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| a.source.tiebreak().cmp(&b.source.tiebreak()))
            .then_with(|| a.text.cmp(&b.text))
    });
    out.retain(Candidate::is_actionable);
    out.truncate(MAX_CANDIDATES);
    out
}

/// 前缀选择：精确行可信就用精确行，否则退回网格推断的那一行。
///
/// 判据与理由见 [`candidates_with_exact`] 的表格。抽成独立函数是为了让它能被
/// **单独测**（这是"两条数据源合成一个"的地方，正是铁律 #81 警告的漏点位置）。
pub fn exact_prefix<'a>(grid: &'a str, exact: Option<&'a str>) -> &'a str {
    match exact {
        // 精确行必须是用户当前那一行的**延续**才用：`633;E` 是执行时快照，
        // 留着旧值直到下一条命令被敲出来，此时它与网格行没有前缀关系。
        Some(ex) if !ex.is_empty() && grid.starts_with(ex) => ex,
        _ => grid,
    }
}

/// 把多个桶的历史**并成一个可查询视图**（遗留项 2：多会话共用一份历史）。
///
/// # 为什么不把桶合成一个 `CommandHistory` 再查
///
/// 合并要挑 `count` / `last_seq` 谁大，而两个桶的 `last_seq` 是**各自从 1 数起**的
/// 会话内序号：同一个数字在两台机器上代表不同的时刻，"比大小"没有意义
/// （对照铁律 #50：同一指标别用两把尺子）。所以这里只按文本去重（`count` 相加、
/// `last_seq` 取大）作为**同一台机器内部的**归并口径，跨桶的大小关系不做承诺 ——
/// 排序最终由 [`candidates`] 的 score 决定，而 score 只看文本匹配度。
///
/// 返回的 `Vec` 排序稳定（先按 `last_seq` 降序，再按文本），便于确定性断言。
pub fn history_rows(buckets: &[&CommandHistory]) -> Vec<HistoryEntry> {
    use std::collections::HashMap;
    let mut merged: HashMap<&str, HistoryEntry> = HashMap::new();
    for b in buckets {
        for e in b.entries() {
            merged
                .entry(e.text.as_str())
                .and_modify(|m| {
                    m.count = m.count.saturating_add(e.count);
                    // 两个桶的序号不可比（见文档），这里只保证"同一机器上更近的赢"
                    m.last_seq = m.last_seq.max(e.last_seq);
                })
                .or_insert_with(|| e.clone());
        }
    }
    let mut out: Vec<HistoryEntry> = merged.into_values().collect();
    out.sort_by(|a, b| {
        b.last_seq
            .cmp(&a.last_seq)
            .then_with(|| a.text.cmp(&b.text))
    });
    out
}

/// 用户命令库的一条（把 `QuickCommandStore` 摊平后的最小投影）。
///
/// 刻意**不**依赖 `quickcmd::QuickCmd`：引擎只认"标签 + 要插入的脚本"两个字段，
/// 这样库的存储结构怎么演进都不会牵动引擎（与 `CommandEntry` 同样的解耦思路）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryCommand {
    /// 展示名（用户起的标签）
    pub label: String,
    /// 要插入终端的脚本文本
    pub script: String,
}

/// 从快捷命令库构造引擎输入（纯函数，便于单测）。
pub fn library_from_folders(store: &crate::quickcmd::QuickCommandStore) -> Vec<LibraryCommand> {
    store
        .folders
        .iter()
        .flat_map(|f| f.commands.iter())
        .map(|c| LibraryCommand {
            label: c.label.clone(),
            script: c.script.clone(),
        })
        .collect()
}

fn mk(
    text: &str,
    detail: Option<String>,
    source: CandidateSource,
    score: i32,
    needle: &str,
) -> Candidate {
    Candidate {
        text: text.to_string(),
        suffix: suffix_for(needle, text),
        detail,
        source,
        score,
    }
}

fn history_detail(e: &HistoryEntry) -> String {
    if e.count > 1 {
        crate::i18n::ts(
            format!("用过 {} 次", e.count),
            format!("Used {} times", e.count),
        )
    } else {
        crate::i18n::t("最近用过").to_string()
    }
}

/// **只补后缀**的唯一落点（见模块文档）。
///
/// `prefix` 是已输入文本，`full` 是候选全文：
/// - `prefix` 为空 ⇒ 后缀就是全文（从头插入）；
/// - `full` 以 `prefix` 开头 ⇒ 差集就是后缀；
/// - 否则（模糊命中、前缀并不真的是前缀）⇒ 返回**全文**：此时"续写"无从谈起，
///   只能整体插入。这个分支是模糊匹配能安全落地的关键 —— 不做整行替换，
///   退化成"在光标处插入一条完整命令"，仍然不碰已有字符。
pub fn suffix_for(prefix: &str, full: &str) -> String {
    if prefix.is_empty() {
        return full.to_string();
    }
    match full.strip_prefix(prefix) {
        Some(rest) => rest.to_string(),
        None => full.to_string(),
    }
}

/// 打分：`None` = 不匹配（不进候选）。
///
/// 三档（对齐 Warp 的 exact / prefix / fuzzy）：
/// - **前缀命中**：`needle` 为空 ⇒ 基础分；`prefix` 命中 ⇒ 基础分 + 长度奖励。
///   长度奖励偏向**更短**的候选（`ls` 优于 `lsblk`），因为补全场景里短的通常是
///   用户想要的。
/// - **模糊命中**：`needle` 的字符按序出现在 `full` 里（不要求连续）⇒ 低分。
///   分数随"命中跨度"收缩：越紧凑的模糊命中越可信（`gco` → `git checkout` 就比
///   跨大半行的命中好）。
///
/// 大小写不敏感（命令名几乎全小写，但用户可能大写开头 —— 别让这个差异
/// 把候选藏起来）。
pub fn score(full: &str, needle: &str) -> Option<i32> {
    if needle.is_empty() {
        return Some(0);
    }
    let full_l = full.to_lowercase();
    let needle_l = needle.to_lowercase();

    if let Some(rest) = full_l.strip_prefix(&needle_l) {
        // 前缀命中：精确等于的整体长度不惩罚；剩余越短越靠前
        let base = 1000;
        let bonus = 40_i32.saturating_sub(rest.chars().count() as i32);
        return Some(base + bonus.max(0));
    }
    fuzzy_score(&full_l, &needle_l).map(|s| s / 10)
}

/// 内置表的打分：命令名命中优先，说明命中次之（允许用中文搜命令）。
///
/// `+500` 的"命令名命中"权重**只在真的有查询串时**给。空查询串下若也给，
/// 内置表会凭这个权重把所有库 / 历史条目挤出前 [`MAX_CANDIDATES`] 名 ——
/// 用户打开浮层（还没打字）看到的全是通用命令，而他自己配的、用过的
/// 一条都看不见，正好和"库 > 历史 > 内置"的排序意图相反。
fn score_builtin(b: &BuiltinCommand, needle: &str) -> Option<i32> {
    if needle.is_empty() {
        return Some(0);
    }
    if let Some(s) = score(b.name, needle) {
        return Some(s + 500);
    }
    // 说明里的子串命中：`磁盘` → `df`（说明是"查看磁盘空间"）
    let desc = crate::i18n::t(b.desc);
    if desc.contains(needle) {
        return Some(200);
    }
    None
}

/// 子序列模糊匹配，返回紧凑度分（越大越紧凑）；不匹配返回 `None`。
///
/// 实现是朴素的贪心：从左到右吃掉 `needle` 的每个字符，记录最后一个命中的位置。
/// 分数 = `100 - 跨度`（跨度 = 末位 - 首位）。贪心在"是否漏掉更紧凑的匹配"上
/// 不是最优，但**确定性可测**，且补全场景里候选都很短，贪心的偏差可忽略。
fn fuzzy_score(full: &str, needle: &str) -> Option<i32> {
    let mut chars = needle.chars();
    let mut want = chars.next()?;
    let mut first: Option<usize> = None;
    for (i, c) in full.chars().enumerate() {
        if c == want {
            let start = *first.get_or_insert(i);
            match chars.next() {
                Some(n) => want = n,
                None => {
                    let span = i - start;
                    return Some(100 - span.min(99) as i32);
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmdhistory::CommandHistory;

    fn lib(pairs: &[(&str, &str)]) -> Vec<LibraryCommand> {
        pairs
            .iter()
            .map(|(l, s)| LibraryCommand {
                label: (*l).to_string(),
                script: (*s).to_string(),
            })
            .collect()
    }

    // ---- suffix_for：只补后缀的契约 ----

    #[test]
    fn suffix_is_empty_prefix_returns_whole_text() {
        assert_eq!(suffix_for("", "git status"), "git status");
    }

    #[test]
    fn suffix_strips_the_common_prefix() {
        assert_eq!(suffix_for("git ch", "git checkout"), "eckout");
        assert_eq!(suffix_for("ls", "ls"), "");
    }

    /// 模糊命中时前缀并不是真前缀 ⇒ 退回"整体插入"，绝不产生删除序列。
    #[test]
    fn suffix_falls_back_to_whole_text_for_fuzzy_hits() {
        assert_eq!(suffix_for("gco", "git checkout"), "git checkout");
    }

    // ---- score ----

    #[test]
    fn prefix_match_beats_fuzzy() {
        let p = score("git checkout", "git ").unwrap();
        let f = score("git checkout", "gco").unwrap();
        assert!(p > f, "前缀命中必须压过模糊命中");
    }

    #[test]
    fn shorter_prefix_candidates_rank_higher() {
        let a = score("ls", "l").unwrap();
        let b = score("lsblk", "l").unwrap();
        assert!(a > b, "更短的候选在同等命中下应更靠前");
    }

    #[test]
    fn no_match_returns_none() {
        assert!(score("git status", "xyz").is_none());
    }

    #[test]
    fn match_is_case_insensitive() {
        assert!(score("git status", "GIT").is_some());
    }

    // ---- candidates：整体行为 ----

    #[test]
    fn empty_prefix_lists_from_all_three_sources() {
        let mut h = CommandHistory::new();
        h.record("docker compose up");
        let out = candidates("", &h, &lib(&[("部署", "systemctl restart app")]));
        let srcs: Vec<CandidateSource> = out.iter().map(|c| c.source).collect();
        assert!(srcs.contains(&CandidateSource::Library));
        assert!(srcs.contains(&CandidateSource::History));
        assert!(srcs.contains(&CandidateSource::Builtin));
        assert!(out.len() <= MAX_CANDIDATES);
    }

    /// 库永远排在历史与内置之前（同分时按来源权重）。
    #[test]
    fn library_outranks_history_and_builtin_on_ties() {
        let mut h = CommandHistory::new();
        // 造一个与库条目同前缀的历史与内置候选
        h.record("ls -la");
        let out = candidates("ls", &h, &lib(&[("看文件", "ls -la")]));
        assert_eq!(
            out[0].source,
            CandidateSource::Library,
            "同分时用户自己的库必须排最前，实际：{:?}",
            out.iter().map(|c| (c.source, &c.text)).collect::<Vec<_>>()
        );
    }

    /// 前缀已完全匹配的候选不该出现（采纳它等于什么都不做）。
    #[test]
    fn fully_matched_candidates_are_filtered_out() {
        let h = CommandHistory::new();
        let out = candidates("df", &h, &lib(&[]));
        assert!(
            !out.iter().any(|c| c.text == "df"),
            "`df` 已完整输入，不该再列出来当候选"
        );
        // 但以 df 开头的更长的命令仍在（如 df 没有，但内置表里有别的）
        assert!(out.iter().all(Candidate::is_actionable));
    }

    #[test]
    fn candidates_carry_actionable_suffixes() {
        let h = CommandHistory::new();
        let out = candidates("git ch", &h, &lib(&[]));
        let hit = out
            .iter()
            .find(|c| c.text == "git checkout")
            .expect("应命中");
        assert_eq!(hit.suffix, "eckout", "写进 PTY 的只能是补齐部分");
    }

    #[test]
    fn chinese_description_is_searchable() {
        let h = CommandHistory::new();
        let out = candidates("磁盘", &h, &lib(&[]));
        assert!(
            out.iter().any(|c| c.text == "df"),
            "按中文说明应能找到 df，实际：{:?}",
            out.iter().map(|c| &c.text).collect::<Vec<_>>()
        );
    }

    #[test]
    fn output_is_deterministic() {
        let mut h = CommandHistory::new();
        h.record("git status");
        h.record("git log");
        let a: Vec<String> = candidates("git", &h, &lib(&[]))
            .iter()
            .map(|c| c.text.clone())
            .collect();
        let b: Vec<String> = candidates("git", &h, &lib(&[]))
            .iter()
            .map(|c| c.text.clone())
            .collect();
        assert_eq!(a, b, "候选顺序必须确定，否则列表在两次刷新间跳动");
    }

    /// 历史为空、库为空时也要有东西可补（内置表是兜底）。
    #[test]
    fn builtin_table_alone_still_produces_candidates() {
        let out = candidates("", &CommandHistory::new(), &lib(&[]));
        assert!(!out.is_empty(), "什么都没有时内置表必须兜底");
    }

    #[test]
    fn suffix_is_ascii_safe_for_multibyte_prefix() {
        // 中文说明不会进 suffix（suffix 来自命令名），但前缀本身可能是中文搜索词
        let out = candidates("磁盘", &CommandHistory::new(), &lib(&[]));
        for c in out {
            // 模糊命中退化到整体插入：suffix = 全文
            assert!(!c.suffix.is_empty());
        }
    }

    // ---- exact_prefix：精确行取不到就退回网格（遗留项 3） ----

    /// 精确行是网格行的前缀 ⇒ 用精确行（`633;E` 不受 prompt / 换行干扰）。
    #[test]
    fn exact_prefix_wins_when_it_extends_the_grid_line() {
        // 网格行是「已知的」更长的那个（H1 剥完仍带着 prompt 尾巴），
        // 精确行是它的前缀 ⇒ 精确行更干净，采纳它
        assert_eq!(exact_prefix("$ git checkout", Some("$ git ch")), "$ git ch");
        // 最常见的形态：H1 剥多了 / 剥少了，精确行给的是干净的命令行
        assert_eq!(exact_prefix("git checkout", Some("git ch")), "git ch");
    }

    /// `633;E` 是**执行时**快照：一条命令跑完后它留着旧值，直到下一条被敲出来。
    /// 此时它与用户正在打的这一行没有前缀关系 ⇒ 必须退回网格，
    /// 否则补全会一直按**上一条**命令算候选（用户看着 `ls` 收到 `git` 的补全）。
    #[test]
    fn stale_exact_line_falls_back_to_the_grid() {
        assert_eq!(
            exact_prefix("ls", Some("git status")),
            "ls",
            "过期精确行不是当前行的前缀时不许用"
        );
    }

    /// 空精确行 = 「新 prompt 刚下来，还没敲」⇒ 退回网格。
    #[test]
    fn empty_exact_line_falls_back_to_the_grid() {
        assert_eq!(exact_prefix("git ch", Some("")), "git ch");
    }

    /// 没有 integration 的会话（绝大多数远端）：`None` ⇒ 网格那条路照常。
    #[test]
    fn missing_exact_line_uses_the_grid() {
        assert_eq!(exact_prefix("git ch", None), "git ch");
    }

    /// 端到端：精确行确实参与候选计算（不是只影响一个没用上的变量）。
    #[test]
    fn candidates_with_exact_uses_the_exact_line_for_suffixes() {
        let h = CommandHistory::new();
        let lib = lib(&[("部署", "git checkout main")]);
        // 网格行差了一截（H1 把 `$ ` 之后的内容都当过 prompt 剥掉了），
        // 精确行给全了 ⇒ suffix 应当按精确行求差
        let out = candidates_with_exact("x", Some("git"), &h, &lib);
        let hit = out.iter().find(|c| c.text == "git checkout main");
        if let Some(hit) = hit {
            assert_eq!(hit.suffix, " checkout main", "suffix 必须按精确行算");
        }
    }

    // ---- history_rows：多会话共用一份历史（遗留项 2） ----

    /// 两台机器的历史都要在候选里露脸 —— 这是「共用一份历史」的直接判据。
    #[test]
    fn history_rows_merges_every_bucket() {
        let mut a = CommandHistory::new();
        a.record("docker ps");
        let mut b = CommandHistory::new();
        b.record("kubectl get pods");
        let rows = history_rows(&[&a, &b]);
        let texts: Vec<&str> = rows.iter().map(|r| r.text.as_str()).collect();
        assert!(texts.contains(&"docker ps"));
        assert!(texts.contains(&"kubectl get pods"));
    }

    /// 同一条命令在两台机器上都用过 ⇒ 计数**相加**（不是各列一条）。
    #[test]
    fn history_rows_merges_counts_for_the_same_command() {
        let mut a = CommandHistory::new();
        a.record("ls -la");
        a.record("ls -la"); // 本机用过 2 次
        let mut b = CommandHistory::new();
        b.record("ls -la"); // 另一台 1 次
        let rows = history_rows(&[&a, &b]);
        assert_eq!(rows.len(), 1, "同一文本只留一条：{rows:?}");
        assert_eq!(rows[0].count, 3);
    }

    /// 合并顺序必须确定：候选列表在两次刷新间跳动是最烦人的观感问题。
    #[test]
    fn history_rows_is_deterministic() {
        let mut a = CommandHistory::new();
        a.record("a1");
        a.record("a2");
        let mut b = CommandHistory::new();
        b.record("b1");
        let x: Vec<String> = history_rows(&[&a, &b])
            .iter()
            .map(|r| r.text.clone())
            .collect();
        let y: Vec<String> = history_rows(&[&a, &b])
            .iter()
            .map(|r| r.text.clone())
            .collect();
        assert_eq!(x, y);
    }

    /// 空桶不影响结果（开着一堆没敲过命令的标签是常态）。
    #[test]
    fn history_rows_ignores_empty_buckets() {
        let a = CommandHistory::new();
        let mut b = CommandHistory::new();
        b.record("only-here");
        let rows = history_rows(&[&a, &b]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text, "only-here");
        assert!(history_rows(&[&a]).is_empty());
    }
}
