//! `propose_task` → 真正的定时任务（v1.1 §3.6.1「任务草案」）。
//!
//! 为什么要有这一层：`propose_task` 是 agent 全部工具里唯一「产出**配置**而不是
//! 产出**信息**」的工具。让 UI 自己解析工具参数，等于把「参数语义」抄第二遍 ——
//! 抄错的表现是「AI 说它建了一条每 15 分钟的巡检，实际存进去的是每天 9 点」。
//! 所以解析与落库形状固化在 core，UI 只负责「用户点了确认 → 调这个函数 → 存盘」。
//!
//! 本模块零 UI 依赖、零 IO：给定参数与连接库就能算出 `Task`，可脱离 GPUI 单测（AC-47）。

use crate::agent::resolve_targets;
use crate::agent::tools::{ProposalKind, ToolArgs};
use crate::connections::ConnectionStore;
use crate::tasks::types::{CheckKind, TaskAction, TaskTarget, Trigger};
use crate::tasks::Task;

/// 把 `propose_task` 的参数算成一条 `Task`。
///
/// **不落盘、不入库** —— 由调用方（UI 的确认卡片）在用户确认后决定怎么存。
/// 返回 `Err` 时带上人类可读原因，直接回注给模型让它自纠。
pub fn to_task(args: &ToolArgs, store: &ConnectionStore, now: u64) -> Result<Task, String> {
    let ToolArgs::ProposeTask {
        name,
        schedule,
        action,
        selector,
        prompt,
        command,
    } = args
    else {
        return Err("参数不是 propose_task".to_string());
    };

    let name = name.trim();
    if name.is_empty() {
        return Err("任务名称不能为空".to_string());
    }
    let trigger = parse_schedule(schedule)?;
    // 目标在**草案阶段就解析成具体 id**：AI 巡检看到的是这一批机器，任务就该盯这
    // 一批。存 `groups` 会让「之后有人往分组里加了机器」悄悄改变任务范围 ——
    // 那是一个用户没批准过的改动。
    let ids = resolve_targets(store, selector).map_err(|e| e.to_string())?;
    if ids.is_empty() {
        return Err("目标解析为空，无法建任务".to_string());
    }

    let action = match action {
        ProposalKind::InspectAi => TaskAction::AiInspect {
            target: TaskTarget {
                profiles: ids,
                ..Default::default()
            },
            prompt: prompt.clone().unwrap_or_default(),
            // 草案默认只查负载：日志路径必须由用户明确给出，
            // 让 AI 猜一个路径再写进定时任务，等于每天去 grep 一个可能不存在的文件。
            checks: vec![CheckKind::Load],
            thresholds: None,
            log_path: None,
            log_level: Default::default(),
        },
        ProposalKind::ExecMulti => {
            let cmd = command
                .as_deref()
                .map(str::trim)
                .filter(|c| !c.is_empty())
                .ok_or_else(|| "exec_multi 草案缺少 command".to_string())?;
            TaskAction::ExecMulti {
                target: TaskTarget {
                    profiles: ids,
                    ..Default::default()
                },
                command: cmd.to_string(),
                timeout_secs: crate::agent::DEFAULT_TOOL_TIMEOUT_SECS,
                // 状态变更在无人值守下一律不执行（§7.4）：草案固定为只读，
                // 用户要放开得自己去任务对话框里打开那个开关。
                allow_mutating: false,
            }
        }
        ProposalKind::Download => {
            let remote = prompt
                .as_deref()
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .ok_or_else(|| "download 草案缺少远端路径（用 prompt 字段给出）".to_string())?;
            crate::tasks::types::validate_remote_path(remote).map_err(|e| e.to_string())?;
            TaskAction::Download {
                target: TaskTarget {
                    profiles: ids,
                    ..Default::default()
                },
                remote_path: remote.to_string(),
                local_dir: None,
            }
        }
    };

    let mut task = Task::new(name, trigger, action, now);
    // 校验一遍：草案必须是**能保存的**，否则确认卡片点了「加入」却存不进去。
    if let Err(e) = task.validate() {
        return Err(e.to_string());
    }
    // 建出来的任务默认**停用**：AI 提议的东西不该在用户还没看过的时候就到点开跑。
    task.enabled = false;
    task.next_run_at = 0;
    Ok(task)
}

/// 解析草案里的 `schedule` 字段。
///
/// 接受两种写法：
/// * 5 段 cron 表达式（`0 9 * * *`）—— 与任务对话框同一套解析器；
/// * 间隔写法：`15m` / `15 m` / `every 15m` / `每 15 分钟` / `2h` / `30s`。
///
/// 为什么不让模型直接给 cron：模型经常给出 `*/15 * * * *` 这类正确但用户看不懂的
/// 表达式；容忍两种写法，用户在任务对话框里看到的是同一份可读文案（`每 15 分钟`）。
pub fn parse_schedule(raw: &str) -> Result<Trigger, String> {
    let s = raw.trim();
    if s.is_empty() {
        return Err("schedule 不能为空".to_string());
    }
    if let Ok(_cron) = crate::tasks::cron::parse_cron(s) {
        return Ok(Trigger::cron(s));
    }
    if let Some(secs) = parse_interval(s) {
        return Ok(Trigger::Interval { every_secs: secs });
    }
    Err(format!(
        "无法理解的 schedule：{s}（可用 cron 五段式，或 15m / 2h / 每 15 分钟）"
    ))
}

/// 间隔写法 → 秒。识别不出来返回 `None`（由调用方给出统一错误）。
fn parse_interval(raw: &str) -> Option<u64> {
    let mut s = raw.trim().to_ascii_lowercase();
    for prefix in ["every ", "每", "每 "] {
        if let Some(rest) = s.strip_prefix(prefix) {
            s = rest.trim().to_string();
        }
    }
    // 单位后缀（中英文都要认）
    let (num, unit_secs) = if let Some(n) = s.strip_suffix("分钟") {
        (n, 60)
    } else if let Some(n) = s.strip_suffix("小时") {
        (n, 3600)
    } else if let Some(n) = s.strip_suffix("秒") {
        (n, 1)
    } else if let Some(n) = s.strip_suffix('h') {
        (n, 3600)
    } else if let Some(n) = s.strip_suffix('m') {
        (n, 60)
    } else if let Some(n) = s.strip_suffix('s') {
        (n, 1)
    } else {
        // 裸数字按分钟解释（「每 15」在中文语境里就是 15 分钟）
        (s.as_str(), 60)
    };
    let n: u64 = num.trim().parse().ok()?;
    if n == 0 {
        return None;
    }
    let secs = n.saturating_mul(unit_secs);
    // 与 `MIN_INTERVAL_SECS` 对齐：小于下限的间隔会让任务变成持续运行，
    // 直接钳到下限而不是报错 —— 模型给 `30s` 是常见的，用户接受就行。
    Some(secs.max(crate::tasks::types::MIN_INTERVAL_SECS))
}

/// 草案在确认卡片上的一行预览（不建任务也能看懂会发生什么）。
pub fn describe(args: &ToolArgs, store: &ConnectionStore) -> String {
    let ToolArgs::ProposeTask {
        name,
        schedule,
        action,
        selector,
        prompt,
        command,
    } = args
    else {
        return String::new();
    };
    let targets = match resolve_targets(store, selector) {
        Ok(ids) => {
            let names = crate::agent::tools::display_names(store, &ids);
            if names.is_empty() {
                crate::i18n::ts("无目标".into(), "no target".into())
            } else if names.len() <= 4 {
                names.join(", ")
            } else {
                format!("{} …{}", names[..3].join(", "), names.len())
            }
        }
        Err(e) => e.to_string(),
    };
    let what = match action {
        ProposalKind::InspectAi => {
            let mut s = crate::i18n::ts("AI 巡检".into(), "AI inspection".into());
            if let Some(p) = prompt.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
                s.push_str(&format!("（{p}）"));
            }
            s
        }
        ProposalKind::ExecMulti => crate::i18n::ts(
            format!(
                "执行命令：{}",
                command
                    .as_deref()
                    .unwrap_or("")
                    .lines()
                    .next()
                    .unwrap_or("")
            ),
            format!(
                "run: {}",
                command
                    .as_deref()
                    .unwrap_or("")
                    .lines()
                    .next()
                    .unwrap_or("")
            ),
        ),
        ProposalKind::Download => crate::i18n::ts(
            format!("下载 {}", prompt.as_deref().unwrap_or("")),
            format!("download {}", prompt.as_deref().unwrap_or("")),
        ),
    };
    format!("{name} · {schedule} · {what} · {targets}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::TargetSelector;
    use crate::connections::{ConnectionGroup, ConnectionProfile};

    fn store() -> ConnectionStore {
        let mut st = ConnectionStore::default();
        let mut g = ConnectionGroup::new("生产");
        g.id = "g1".into();
        st.groups.push(g);
        let mut a = ConnectionProfile::new("prod-01", "10.0.0.1", "root");
        a.id = "p1".into();
        a.group_id = Some("g1".into());
        let mut b = ConnectionProfile::new("prod-02", "10.0.0.2", "root");
        b.id = "p2".into();
        b.group_id = Some("g1".into());
        st.profiles = vec![a, b];
        st
    }

    fn args(action: ProposalKind, schedule: &str) -> ToolArgs {
        ToolArgs::ProposeTask {
            name: "夜间巡检".into(),
            schedule: schedule.into(),
            action,
            selector: TargetSelector::Group("生产".into()),
            prompt: None,
            command: None,
        }
    }

    /// 间隔写法的各种形态都要认（模型给哪种都不该失败）。
    #[test]
    fn schedule_accepts_intervals_in_both_languages() {
        let cases = [
            ("15m", 15 * 60),
            ("15 m", 15 * 60),
            ("every 15m", 15 * 60),
            ("每 15 分钟", 15 * 60),
            ("每15分钟", 15 * 60),
            ("2h", 2 * 3600),
            ("30s", 30),
            // 裸数字按分钟
            ("15", 15 * 60),
        ];
        for (raw, want) in cases {
            match parse_schedule(raw).unwrap_or_else(|e| panic!("{raw} 解析失败：{e}")) {
                Trigger::Interval { every_secs } => {
                    assert_eq!(
                        every_secs,
                        want.max(crate::tasks::types::MIN_INTERVAL_SECS),
                        "{raw}"
                    )
                }
                other => panic!("{raw} 应当解析成间隔，实际 {other:?}"),
            }
        }
        // 间隔下限：比 `MIN_INTERVAL_SECS` 更密的一律钳上来
        match parse_schedule("1s").unwrap() {
            Trigger::Interval { every_secs } => {
                assert_eq!(every_secs, crate::tasks::types::MIN_INTERVAL_SECS)
            }
            other => panic!("应当钳到下限，实际 {other:?}"),
        }
    }

    /// cron 五段式原样保留（时区跟随本机，不猜也不存）。
    #[test]
    fn schedule_accepts_cron_verbatim() {
        match parse_schedule("0 9 * * *").unwrap() {
            Trigger::Cron { expr } => {
                assert_eq!(expr, "0 9 * * *");
            }
            other => panic!("应当解析成 cron，实际 {other:?}"),
        }
        // 认不出来的给可读错误（卡片上要显示）
        let e = parse_schedule("每天上午").unwrap_err();
        assert!(e.contains("无法理解"), "错误要说清是什么：{e}");
        assert!(parse_schedule("").is_err());
    }

    /// `inspect_ai` 草案：目标解析成具体 id、默认停用、日志巡检**不开**。
    #[test]
    fn inspect_proposal_becomes_a_disabled_task_with_resolved_targets() {
        let st = store();
        let mut a = args(ProposalKind::InspectAi, "15m");
        // 「AI 要求」必输（2026-09-22）：模型不给这句话，`to_task` 就该把
        // 「请填写 AI 要求」回注给它自纠 —— 缺了它任务到点也不知道要查什么。
        assert!(
            to_task(&a, &st, 0).unwrap_err().contains("AI 要求"),
            "缺 AI 要求要明确报错（回注给模型自纠）"
        );
        if let ToolArgs::ProposeTask { prompt, .. } = &mut a {
            *prompt = Some("看看负载与磁盘占用".into());
        }
        let t = to_task(&a, &st, 1_000).unwrap();
        assert_eq!(t.name, "夜间巡检");
        assert!(!t.enabled, "AI 提议的任务不该默认启用");
        assert_eq!(t.next_run_at, 0, "停用任务不该有下次运行时刻");
        assert_eq!(t.trigger, Trigger::Interval { every_secs: 900 });
        match &t.action {
            TaskAction::AiInspect {
                target,
                checks,
                log_path,
                ..
            } => {
                assert_eq!(target.profiles, vec!["p1".to_string(), "p2".to_string()]);
                assert!(target.groups.is_empty(), "必须落具体 id 而不是分组");
                assert_eq!(checks, &vec![CheckKind::Load]);
                assert!(log_path.is_none(), "不能替用户猜一个日志路径写进定时任务");
            }
            other => panic!("动作类型不对：{other:?}"),
        }
    }

    /// `exec_multi` 草案：必须带命令；且**固定只读**（无人值守不放开状态变更）。
    #[test]
    fn exec_proposal_requires_a_command_and_stays_read_only() {
        let st = store();
        let mut a = args(ProposalKind::ExecMulti, "0 9 * * *");
        if let ToolArgs::ProposeTask { command, .. } = &mut a {
            *command = None;
        }
        assert!(
            to_task(&a, &st, 0).unwrap_err().contains("command"),
            "缺命令要明确报错"
        );

        if let ToolArgs::ProposeTask { command, .. } = &mut a {
            *command = Some("systemctl restart nginx".into());
        }
        let t = to_task(&a, &st, 0).unwrap();
        match &t.action {
            TaskAction::ExecMulti {
                command,
                allow_mutating,
                ..
            } => {
                assert_eq!(command, "systemctl restart nginx");
                assert!(!allow_mutating, "草案固定只读，放开要用户自己动手");
            }
            other => panic!("动作类型不对：{other:?}"),
        }
    }

    /// `download` 草案：路径必填且要过目录穿越校验。
    #[test]
    fn download_proposal_validates_the_remote_path() {
        let st = store();
        let mut a = args(ProposalKind::Download, "1h");
        // prompt 里放远端路径
        if let ToolArgs::ProposeTask { prompt, .. } = &mut a {
            *prompt = Some("../../etc/passwd".into());
        }
        assert!(
            to_task(&a, &st, 0).is_err(),
            "目录穿越必须被 `validate_remote_path` 拦下"
        );
        if let ToolArgs::ProposeTask { prompt, .. } = &mut a {
            *prompt = Some("/var/log/app.log".into());
        }
        let t = to_task(&a, &st, 0).unwrap();
        match &t.action {
            TaskAction::Download { remote_path, .. } => {
                assert_eq!(remote_path, "/var/log/app.log")
            }
            other => panic!("动作类型不对：{other:?}"),
        }
    }

    /// 目标解析不出来（分组名写错）时要报错，而不是存一条范围为空的任务。
    #[test]
    fn unknown_target_is_rejected() {
        let st = store();
        let a = ToolArgs::ProposeTask {
            name: "x".into(),
            schedule: "15m".into(),
            action: ProposalKind::InspectAi,
            selector: TargetSelector::Group("不存在的组".into()),
            prompt: None,
            command: None,
        };
        let e = to_task(&a, &st, 0).unwrap_err();
        assert!(!e.is_empty(), "要给出可读原因");
        // 空名字也要拦
        let st2 = store();
        let mut bad = a.clone();
        if let ToolArgs::ProposeTask { name, selector, .. } = &mut bad {
            *name = "   ".into();
            *selector = TargetSelector::Group("生产".into());
        }
        assert!(to_task(&bad, &st2, 0).unwrap_err().contains("名称"));
    }

    /// 卡片预览要一行说清「名字 / 周期 / 干什么 / 哪些机器」。
    #[test]
    fn describe_summarises_the_draft() {
        let st = store();
        let mut a = args(ProposalKind::ExecMulti, "0 9 * * *");
        if let ToolArgs::ProposeTask { command, .. } = &mut a {
            *command = Some("df -h".into());
        }
        let s = describe(&a, &st);
        assert!(s.contains("夜间巡检"), "{s}");
        assert!(s.contains("0 9 * * *"), "{s}");
        assert!(s.contains("df -h"), "{s}");
        assert!(s.contains("prod-01"), "要列出目标展示名：{s}");
    }
}
