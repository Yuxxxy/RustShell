//! 统一领域事件（v1.1 §2.2.2）。
//!
//! 这一层的作用是**把 UI 与内部状态机解耦**：UI 只消费事件、维护自己的
//! `AgentViewState` / `TaskViewState` / `AlertViewState`，不直接读
//! `AgentStep`、`DedupEntry` 这类内部结构。
//!
//! 为什么值得单独抽一层：
//! * 内部状态机为了「能单测」会不断调整字段，而事件是**对外契约**，
//!   一旦冻结就不该跟着内部重构来回改（否则每次重构都要动 UI）；
//! * 同一个生命周期可能被多个 UI 消费者观察（任务面板、概览卡片、
//!   通知条），各自订阅一份事件比各自复制一份内部状态更不容易漂移。
//!
//! 事件是**纯数据**：不携带 `Arc`、不携带回调、不携带 UI 类型，
//! 因此可以在没有 GPUI 的纯单测里构造与断言。

use serde::{Deserialize, Serialize};

/// Agent 一轮运行的事件。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentEvent {
    /// 一次运行开始
    Started { run_id: String },
    /// 正在等模型（首 token 之前）
    Thinking { run_id: String },
    /// 某个工具开始执行
    ToolStarted {
        run_id: String,
        call_id: String,
        tool: String,
    },
    /// 工具执行进度（批量任务用它报「N/总 M」）
    ToolProgress {
        run_id: String,
        call_id: String,
        message: String,
    },
    /// 工具执行结束
    ToolCompleted {
        run_id: String,
        call_id: String,
        ok: bool,
    },
    /// 需要用户确认（Confirm 级工具，循环在此挂起）
    ConfirmationRequired { run_id: String, call_id: String },
    /// 整轮完成
    Completed { run_id: String },
    /// 整轮失败
    Failed { run_id: String, message: String },
    /// 用户中止
    Cancelled { run_id: String },
}

impl AgentEvent {
    pub fn run_id(&self) -> &str {
        match self {
            Self::Started { run_id }
            | Self::Thinking { run_id }
            | Self::ToolStarted { run_id, .. }
            | Self::ToolProgress { run_id, .. }
            | Self::ToolCompleted { run_id, .. }
            | Self::ConfirmationRequired { run_id, .. }
            | Self::Completed { run_id }
            | Self::Failed { run_id, .. }
            | Self::Cancelled { run_id } => run_id,
        }
    }

    /// 是否为终止事件（UI 据此收尾「正在运行」态）。
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed { .. } | Self::Failed { .. } | Self::Cancelled { .. }
        )
    }
}

/// 任务运行的事件。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TaskEvent {
    /// 到点开始
    Started { task_id: String, run_id: String },
    /// 正在解析目标（目标为空等失败在这一步暴露）
    ResolvingTargets { task_id: String },
    /// 正在跑（`done` / `total` 供「正在运行 N/M」）
    Progress {
        task_id: String,
        done: usize,
        total: usize,
    },
    /// 正在跑告警判定
    EvaluatingFindings { task_id: String },
    /// 正在派发通知
    Notifying { task_id: String },
    /// 跑完（`status` 为 `RunStatus` 的蛇形名，避免本模块反向依赖 tasks）
    Finished {
        task_id: String,
        run_id: String,
        status: String,
    },
    /// 跳过（重入 / 错过窗口 / 目标为空）
    Skipped { task_id: String, reason: String },
}

impl TaskEvent {
    pub fn task_id(&self) -> &str {
        match self {
            Self::Started { task_id, .. }
            | Self::ResolvingTargets { task_id }
            | Self::Progress { task_id, .. }
            | Self::EvaluatingFindings { task_id }
            | Self::Notifying { task_id }
            | Self::Finished { task_id, .. }
            | Self::Skipped { task_id, .. } => task_id,
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Finished { .. } | Self::Skipped { .. })
    }
}

/// 告警生命周期的事件。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AlertEvent {
    /// 进入 firing（`suppressed` = 处于抑制窗口内、本次没发通知）
    Firing {
        rule_id: String,
        profile_id: String,
        severity: String,
        /// 是否真的发出了通知（被抑制时为 false）
        notified: bool,
    },
    /// 已恢复
    Recovered { rule_id: String, profile_id: String },
    /// 通知已派发到某通道（失败时 `ok = false`）
    Dispatched {
        dedup_key: String,
        channel: String,
        ok: bool,
    },
}

/// SSH 会话层事件（连接/断开；供 UI 与任务面板共享）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SshEvent {
    Connected {
        profile_id: String,
        host: String,
    },
    /// 连接失败（`reason` 已脱敏）
    ConnectFailed {
        profile_id: String,
        reason: String,
    },
    Disconnected {
        profile_id: String,
    },
}

/// 统一领域事件。
///
/// UI 只订阅这一个枚举（按变体分派），而不是分别订阅四套内部通道 ——
/// 多订阅点的代价是「新增一类事件就漏改一处 UI」，而漏改的表现是静默不更新。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum AppEvent {
    Agent(AgentEvent),
    Task(TaskEvent),
    Alert(AlertEvent),
    Ssh(SshEvent),
}

impl AppEvent {
    /// 是否为终止事件。
    pub fn is_terminal(&self) -> bool {
        match self {
            Self::Agent(e) => e.is_terminal(),
            Self::Task(e) => e.is_terminal(),
            // 告警是状态迁移，没有「终止」概念
            Self::Alert(_) => false,
            Self::Ssh(e) => matches!(
                e,
                SshEvent::Disconnected { .. } | SshEvent::ConnectFailed { .. }
            ),
        }
    }

    /// 一行可读描述（通知条与日志用；不参与 UI 布局计算）。
    pub fn describe(&self) -> String {
        use crate::i18n::ts;
        match self {
            Self::Agent(AgentEvent::Started { .. }) => {
                ts("巡检开始".into(), "agent started".into())
            }
            Self::Agent(AgentEvent::ToolStarted { tool, .. }) => {
                ts(format!("执行工具 {tool}"), format!("tool {tool} started"))
            }
            Self::Agent(AgentEvent::ConfirmationRequired { .. }) => {
                ts("等待确认".into(), "awaiting confirmation".into())
            }
            Self::Agent(AgentEvent::Completed { .. }) => {
                ts("巡检完成".into(), "agent completed".into())
            }
            Self::Agent(AgentEvent::Failed { message, .. }) => ts(
                format!("巡检失败：{message}"),
                format!("agent failed: {message}"),
            ),
            Self::Agent(AgentEvent::Cancelled { .. }) => {
                ts("巡检已停止".into(), "agent cancelled".into())
            }
            Self::Agent(_) => ts("巡检进行中".into(), "agent running".into()),
            Self::Task(TaskEvent::Started { .. }) => ts("任务开始".into(), "task started".into()),
            Self::Task(TaskEvent::Finished { status, .. }) => ts(
                format!("任务结束：{status}"),
                format!("task finished: {status}"),
            ),
            Self::Task(TaskEvent::Skipped { reason, .. }) => ts(
                format!("任务已跳过：{reason}"),
                format!("task skipped: {reason}"),
            ),
            Self::Task(_) => ts("任务进行中".into(), "task running".into()),
            Self::Alert(AlertEvent::Firing { severity, .. }) => ts(
                format!("触发预警（{severity}）"),
                format!("alert firing ({severity})"),
            ),
            Self::Alert(AlertEvent::Recovered { .. }) => {
                ts("预警已恢复".into(), "alert recovered".into())
            }
            Self::Alert(AlertEvent::Dispatched { channel, ok, .. }) => {
                if *ok {
                    ts(
                        format!("已通知（{channel}）"),
                        format!("notified via {channel}"),
                    )
                } else {
                    ts(
                        format!("通知失败（{channel}）"),
                        format!("notify failed via {channel}"),
                    )
                }
            }
            Self::Ssh(SshEvent::Connected { host, .. }) => {
                ts(format!("已连接 {host}"), format!("connected to {host}"))
            }
            Self::Ssh(SshEvent::ConnectFailed { reason, .. }) => ts(
                format!("连接失败：{reason}"),
                format!("connect failed: {reason}"),
            ),
            Self::Ssh(SshEvent::Disconnected { .. }) => {
                ts("连接已断开".into(), "disconnected".into())
            }
        }
    }
}

/// 把 `RunStatus` 转成事件里的字符串（事件层不反向依赖 `tasks`）。
pub fn run_status_tag(s: crate::tasks::RunStatus) -> String {
    serde_json::to_value(s)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".into())
}

/// 把 `Severity` 转成事件里的字符串。
pub fn severity_tag(s: crate::alerts::Severity) -> String {
    serde_json::to_value(s)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "warning".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_events_expose_run_id_and_terminality() {
        let e = AgentEvent::ToolStarted {
            run_id: "r1".into(),
            call_id: "c1".into(),
            tool: "inspect_load".into(),
        };
        assert_eq!(e.run_id(), "r1");
        assert!(!e.is_terminal());

        for e in [
            AgentEvent::Completed {
                run_id: "r1".into(),
            },
            AgentEvent::Failed {
                run_id: "r1".into(),
                message: "x".into(),
            },
            AgentEvent::Cancelled {
                run_id: "r1".into(),
            },
        ] {
            assert!(e.is_terminal(), "{e:?} 应当被当作终止事件");
        }
    }

    #[test]
    fn task_events_expose_task_id_and_terminality() {
        let e = TaskEvent::Progress {
            task_id: "t1".into(),
            done: 3,
            total: 10,
        };
        assert_eq!(e.task_id(), "t1");
        assert!(!e.is_terminal());
        assert!(TaskEvent::Skipped {
            task_id: "t1".into(),
            reason: "错过".into()
        }
        .is_terminal());
    }

    #[test]
    fn app_event_wraps_every_source() {
        let events = vec![
            AppEvent::Agent(AgentEvent::Started { run_id: "r".into() }),
            AppEvent::Task(TaskEvent::Started {
                task_id: "t".into(),
                run_id: "r".into(),
            }),
            AppEvent::Alert(AlertEvent::Firing {
                rule_id: "builtin.load".into(),
                profile_id: "p".into(),
                severity: severity_tag(crate::alerts::Severity::Critical),
                notified: true,
            }),
            AppEvent::Ssh(SshEvent::Connected {
                profile_id: "p".into(),
                host: "10.0.0.1".into(),
            }),
        ];
        for e in &events {
            // 每个变体都要有可读描述（通知条与日志直接用它）
            assert!(!e.describe().is_empty(), "{e:?} 缺少可读描述");
        }
        assert!(!events[0].is_terminal());
        assert!(
            !AppEvent::Alert(AlertEvent::Recovered {
                rule_id: "r".into(),
                profile_id: "p".into()
            })
            .is_terminal(),
            "告警是状态迁移，没有终止概念"
        );
        assert!(AppEvent::Ssh(SshEvent::Disconnected {
            profile_id: "p".into()
        })
        .is_terminal());
    }

    /// 事件是**协议**：序列化后的形状要稳（UI 侧按名字取字段）。
    #[test]
    fn events_serialize_with_stable_tags() {
        let e = AppEvent::Agent(AgentEvent::ToolCompleted {
            run_id: "r1".into(),
            call_id: "c1".into(),
            ok: true,
        });
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["source"], "agent");
        assert_eq!(v["kind"], "tool_completed");
        assert_eq!(v["run_id"], "r1");
        assert_eq!(v["ok"], true);

        // 往返一致
        let back: AppEvent = serde_json::from_value(v).unwrap();
        assert_eq!(back, e);

        // 状态标签是蛇形（事件层不反向依赖 tasks，用字符串传递）
        assert_eq!(run_status_tag(crate::tasks::RunStatus::Partial), "partial");
        assert_eq!(severity_tag(crate::alerts::Severity::Critical), "critical");
    }

    /// 关键约束（AC-46）：同一生命周期的事件能被**多个**消费者独立观察。
    ///
    /// 事件是纯数据且可克隆 —— 扇出给多个订阅者不需要共享内部状态。
    #[test]
    fn events_can_fan_out_to_multiple_consumers() {
        let e = AppEvent::Task(TaskEvent::Finished {
            task_id: "t1".into(),
            run_id: "r1".into(),
            status: run_status_tag(crate::tasks::RunStatus::Ok),
        });
        // 两个「消费者」各自持一份
        let task_panel = e.clone();
        let dashboard = e.clone();
        assert_eq!(task_panel, dashboard);
        // 终止事件两个消费者都能识别
        assert!(task_panel.is_terminal() && dashboard.is_terminal());
    }
}
