//! 会话布局快照（F-9 会话恢复）：把「上次开着哪些标签、怎么分屏、哪个是激活」记住。
//!
//! # 为什么只记布局，不记会话
//!
//! 快照存的是**重建布局需要什么**（连接 id / 本地终端 / 池顺序 / 两个分屏下标），
//! 不存终端内容、不存凭据、不存连接参数 —— 那些是**会话**的事：内容随进程结束就没了，
//! 连接参数在 `connections.json` 里各有一份真值，凭据在 Keychain 里。
//! 抄一份进来只会多一份会腐烂的副本（改了连接却按旧参数重连是最难查的那类 bug）。
//!
//! # 为什么不记临时 SSH 会话
//!
//! 快速连接来的会话没有 `profile_id`，没有任何可重放的目标：写进快照只能得一个
//! 点了「连接」却没反应的标签。调用方在组装快照时就把它排除（见 `ui` 侧的
//! `layout_snapshot`），这里只负责「装进来的是什么就存什么」。
//!
//! # 下标而不是 id
//!
//! 分屏与激活位用**池下标**引用标签。快照是一次性写完的（写完就整份读回），
//! 下标在文件内部自洽；而 session id 每次启动都会重造（连接时生成），存 id
//! 反而要维护一张映射表。代价是读取方必须过一遍 [`sanitize`] —— 手改坏的文件、
//! 或将来版本改了池结构，都可能出现越界下标。

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::split::SplitAxis;

/// 文件格式版本。改结构时 +1：`load` 见到比自己新的版本会出声（见 `load`），
/// 但不拒绝解析 —— 未知字段被 serde 忽略，已知字段照样能用上。
pub const SCHEMA_VERSION: u32 = 1;

/// 上次退出时的布局。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionLayout {
    pub schema_version: u32,
    /// 标签池顺序（= 标签栏顺序）。下标即下面两个引用用的下标。
    pub tabs: Vec<LayoutTab>,
    /// 分屏态；`None` = 单窗格。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub split: Option<LayoutSplit>,
    /// 激活标签下标。`None` = 由恢复方兜底（取第一个标签）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<usize>,
}

/// 一个标签槽位。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayoutTab {
    /// 连接配置 id；`None` = 本地终端。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    /// 标题快照。唯一的用途是：连接配置被删掉之后，占位标签还能说清「原来是哪一台」。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
}

/// 分屏态。两个槽位都取 `tabs` 的下标。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayoutSplit {
    pub axis: SplitAxis,
    pub primary: usize,
    pub secondary: usize,
}

impl SessionLayout {
    pub fn new(tabs: Vec<LayoutTab>) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            tabs,
            split: None,
            active: None,
        }
    }

    /// 空快照（没有标签）。启动时「没什么可恢复」的判据。
    pub fn is_empty(&self) -> bool {
        self.tabs.is_empty()
    }
}

impl LayoutTab {
    pub fn local(title: impl Into<String>) -> Self {
        Self {
            profile_id: None,
            title: title.into(),
        }
    }

    pub fn ssh(profile_id: impl Into<String>, title: impl Into<String>) -> Self {
        Self {
            profile_id: Some(profile_id.into()),
            title: title.into(),
        }
    }
}

/// 整理一份快照：丢掉不成立的引用，返回 (整理后, 警告)。
///
/// 纯函数、无 IO —— 它是「文件里写的什么」与「UI 拿到的什么」之间唯一的一道闸。
/// 判据只有一条：**引用必须落在 `tabs` 里**。分屏不合法就地取消（**不丢标签**：
/// 标签是用户的会话，分屏只是排布，前者比分者值钱）；激活位不合法交给恢复方兜底。
pub fn sanitize(mut layout: SessionLayout) -> (SessionLayout, Vec<String>) {
    let mut warn: Vec<String> = Vec::new();
    let n = layout.tabs.len();

    if layout.schema_version > SCHEMA_VERSION {
        // 不拒绝解析：新版本多加的字段这里天然看不见，能用的部分照样用。
        warn.push(format!(
            "快照版本 {} 比本版 {SCHEMA_VERSION} 新，可能有本版认不得的字段",
            layout.schema_version
        ));
    }

    if let Some(sp) = layout.split {
        let bad = if sp.primary >= n || sp.secondary >= n {
            Some(format!("分屏下标越界（{} 个标签）", n))
        } else if sp.primary == sp.secondary {
            Some("分屏两侧指向同一个标签".to_string())
        } else {
            None
        };
        if let Some(why) = bad {
            warn.push(format!("{why}，已取消分屏（标签都保留）"));
            layout.split = None;
        }
    }

    if layout.active.is_some_and(|a| a >= n) {
        warn.push(format!("激活下标越界（{} 个标签），已交给恢复方兜底", n));
        layout.active = None;
    }

    (layout, warn)
}

/// 读快照。文件不存在 / 解析失败 / 版本为 0 都返回 `None`（=「没有快照」），
/// 解析失败额外把原文留底成 `<path>.broken-<ts>.bak`（与其它配置同一手法）。
///
/// 返回 `None` 的语义是**静默降级**：启动流程照常，只是不弹恢复提示。
pub fn load(path: &Path) -> Option<SessionLayout> {
    let raw = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str::<SessionLayout>(&raw) {
        Ok(layout) => {
            let (layout, warn) = sanitize(layout);
            for w in warn {
                log::warn!("会话布局快照：{w}");
            }
            Some(layout)
        }
        Err(e) => {
            log::warn!("会话布局快照解析失败（按「没有快照」处理）: {e}");
            crate::configio::backup_broken_config(path, &raw);
            None
        }
    }
}

/// 原子写：先写同目录临时文件再 `rename`，避免「写到一半被杀」留下半截 JSON。
pub fn save(path: &Path, layout: &SessionLayout) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let json = serde_json::to_string_pretty(layout)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tabs(n: usize) -> Vec<LayoutTab> {
        (0..n)
            .map(|i| {
                if i % 2 == 0 {
                    LayoutTab::local(format!("本地 {i}"))
                } else {
                    LayoutTab::ssh(format!("p{i}"), format!("机器 {i}"))
                }
            })
            .collect()
    }

    /// 合法的快照必须原样通过 —— 否则「整理」会变成「悄悄改用户布局」。
    #[test]
    fn sanitize_keeps_a_valid_layout_untouched() {
        let mut l = SessionLayout::new(tabs(3));
        l.split = Some(LayoutSplit {
            axis: SplitAxis::Vertical,
            primary: 2,
            secondary: 1,
        });
        l.active = Some(1);
        let (out, warn) = sanitize(l.clone());
        assert_eq!(out, l, "合法快照不该被改动");
        assert!(warn.is_empty(), "合法快照不该产生警告：{warn:?}");
    }

    /// 分屏下标越界 ⇒ 取消分屏，但标签一个不少。
    #[test]
    fn sanitize_drops_a_split_that_points_out_of_range() {
        let mut l = SessionLayout::new(tabs(2));
        l.split = Some(LayoutSplit {
            axis: SplitAxis::Horizontal,
            primary: 0,
            secondary: 9,
        });
        let (out, warn) = sanitize(l);
        assert!(out.split.is_none(), "越界的分屏必须被取消");
        assert_eq!(out.tabs.len(), 2, "取消分屏绝不牵连标签");
        assert!(warn.iter().any(|w| w.contains("越界")), "{warn:?}");
    }

    /// 分屏两侧指向同一个标签 ⇒ 取消分屏（那是「自己跟自己分屏」）。
    #[test]
    fn sanitize_drops_a_split_that_points_at_itself() {
        let mut l = SessionLayout::new(tabs(2));
        l.split = Some(LayoutSplit {
            axis: SplitAxis::Vertical,
            primary: 1,
            secondary: 1,
        });
        let (out, warn) = sanitize(l);
        assert!(out.split.is_none());
        assert!(warn.iter().any(|w| w.contains("同一个标签")), "{warn:?}");
    }

    /// 激活下标越界 ⇒ 置空（**不**顺手改成 0：那是恢复方的决定）。
    #[test]
    fn sanitize_clears_an_out_of_range_active_index() {
        let mut l = SessionLayout::new(tabs(1));
        l.active = Some(4);
        let (out, warn) = sanitize(l);
        assert_eq!(out.active, None);
        assert!(warn.iter().any(|w| w.contains("激活下标越界")), "{warn:?}");
    }

    /// 空快照：没有任何引用可判，原样返回且无警告（调用方据 `is_empty` 决定不弹提示）。
    #[test]
    fn sanitize_accepts_an_empty_layout_quietly() {
        let (out, warn) = sanitize(SessionLayout::new(Vec::new()));
        assert!(out.is_empty());
        assert!(warn.is_empty(), "{warn:?}");
    }

    /// 版本比本版新：出声但**照常解析**（旧版读新版文件时能用的部分要用上）。
    #[test]
    fn sanitize_warns_but_keeps_a_newer_schema_version() {
        let mut l = SessionLayout::new(tabs(1));
        l.schema_version = SCHEMA_VERSION + 3;
        let (out, warn) = sanitize(l);
        assert_eq!(out.tabs.len(), 1, "新版本的快照仍要能用");
        assert!(warn.iter().any(|w| w.contains("比本版")), "{warn:?}");
    }

    /// 序列化往返：写出去、读回来必须逐字段相等（分屏与激活位都要带上）。
    #[test]
    fn layout_round_trips_through_json() {
        let mut l = SessionLayout::new(tabs(3));
        l.split = Some(LayoutSplit {
            axis: SplitAxis::Horizontal,
            primary: 1,
            secondary: 2,
        });
        l.active = Some(2);
        let json = serde_json::to_string(&l).expect("序列化");
        let back: SessionLayout = serde_json::from_str(&json).expect("反序列化");
        assert_eq!(back, l);
    }

    /// 缺字段的老/手改文件要读得进来：`split` / `active` / `title` 都可省。
    #[test]
    fn minimal_json_is_accepted() {
        let json = r#"{"schema_version":1,"tabs":[{"profile_id":"p1"}]}"#;
        let l: SessionLayout = serde_json::from_str(json).expect("最小快照要能读");
        assert_eq!(l.tabs.len(), 1);
        assert_eq!(l.tabs[0].title, "");
        assert_eq!(l.split, None);
        assert_eq!(l.active, None);
    }

    /// 落盘 → 读回：真实文件往返（含原子写留下的临时文件不残留）。
    #[test]
    fn save_then_load_round_trips_on_disk() {
        let dir = std::env::temp_dir().join(format!("rustshell-layout-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        let path = dir.join("session_layout.json");
        let mut l = SessionLayout::new(tabs(2));
        l.split = Some(LayoutSplit {
            axis: SplitAxis::Vertical,
            primary: 0,
            secondary: 1,
        });
        save(&path, &l).expect("落盘");
        assert_eq!(load(&path), Some(l));
        assert!(
            !path.with_extension("json.tmp").exists(),
            "原子写的临时文件必须被 rename 吃掉"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 坏文件：`load` 返回 None（当「没有快照」），并把原文留底 —— 用户改坏的布局不该凭空消失。
    #[test]
    fn a_broken_file_degrades_to_no_snapshot_and_keeps_a_backup() {
        let dir = std::env::temp_dir().join(format!("rustshell-layout-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        let path = dir.join("session_layout.json");
        std::fs::write(&path, "{ 这不是 JSON").expect("写坏文件");
        assert_eq!(load(&path), None, "坏文件必须降级成「没有快照」");
        let backups: Vec<_> = std::fs::read_dir(&dir)
            .expect("列目录")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.contains("broken") && n.ends_with(".bak"))
            .collect();
        assert!(
            !backups.is_empty(),
            "坏文件必须留底，实际目录内容：{backups:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 文件不存在 ⇒ None（首次运行的常态，不是错误）。
    #[test]
    fn a_missing_file_is_just_no_snapshot() {
        let path = std::env::temp_dir().join("rustshell-layout-does-not-exist-9f3.json");
        assert_eq!(load(&path), None);
    }
}
