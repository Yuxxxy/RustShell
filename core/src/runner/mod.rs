//! 定时任务调度与执行。
//!
//! 分三层，边界刻意划清：
//! * [`Scheduler`]（本模块）：**纯决策** —— 哪些任务到点、能否执行、跑完怎么记账；
//! * [`TaskBackend`]：执行一个任务（连服务器、跑命令、下载、AI 巡检），
//!   生产实现由 core 提供、UI 层注入；测试注入假后端；
//! * [`inspect`]：把探针输出变成结论与 finding（纯逻辑）；
//! * [`ssh_backend`]：四类动作的生产实现（真实 SSH / SFTP / AI 巡检）。
//!
//! 这样 `Scheduler` 的重入、漏触发、连败停用这些**最容易写错**的语义可以
//! 完全靠单测覆盖，不需要任何一台真服务器。

pub mod inspect;
pub mod ssh_backend;

pub use ssh_backend::{
    spawn_interactive_agent, AgentDecision, AgentLlm, AgentSink, AgentUiEvent, ChannelSink,
    ConfirmRequest, InteractiveSpec, RealBackend, StreamingLlm, CONFIRM_TIMEOUT_SECS,
};

use crate::alerts::{self, AlertStore, EvalInput, Notification, Notifier, DEFAULT_SUPPRESS_SECS};
use crate::connections::unix_now_secs;
use crate::tasks::{RunLogStore, RunRecord, RunStatus, Task, TaskAction, TaskStore};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

pub use inspect::{
    bind_findings, join_summary, judge_exit, judge_load, judge_log, log_probe_command, parse_load,
    parse_log_probe, CommandOutput, LoadCheck, LoadMetrics, LogProbe, TargetReport,
};

/// 调度 tick 的间隔。
pub const TICK_INTERVAL: Duration = Duration::from_secs(1);

/// 「宽限窗口」：计划时刻过去了多久之内还允许执行。
///
/// 超过它就记 `Skipped` 而不补跑 —— 补跑一个 15 分钟前的巡检没有价值，
/// 还可能在任务密集时瞬间把目标机压垮（见需求文档 §5.5）。
pub const GRACE_SECS: u64 = 60;

/// 正在运行的任务句柄（可取消 + 可查进度）。
pub trait TaskJob: Send + Sync {
    fn cancel(&self);
    /// 已完成的目标数 / 总数
    fn progress(&self) -> (usize, usize);
}

/// 执行结果（一次性收口，由 Scheduler 记进运行记录）。
#[derive(Debug)]
pub struct BackendOutcome {
    pub record: RunRecord,
}

/// 一条待派发的通知：通知本体 + 它该走哪些通道。
///
/// 把通道**和通知绑在一起**而不是事后反查规则：通知渲染后只剩一个标题字符串，
/// 再靠标题去匹配规则是脆的（规则改名就失联）；而派发时拿不到规则列表时
/// 只能退化成「发到默认通道」，那会让用户配的通道静默失效。
#[derive(Debug, Clone)]
pub struct Dispatch {
    pub notification: Notification,
    pub channels: Vec<String>,
}

/// 任务执行后端。
///
/// 返回 `(job, 结果接收端)`：立即返回一个句柄（供 UI 显示进度与停止），
/// 真正的结果稍后经通道到达 —— 与仓库既有的「后台线程 + flume 回传」模式一致。
pub trait TaskBackend: Send + Sync + 'static {
    fn start(&self, task: Task) -> (Arc<dyn TaskJob>, flume::Receiver<BackendOutcome>);
}

/// 任务运行的实时快照（列表行与「正在运行 N/M」用它）。
#[derive(Debug, Clone)]
pub struct RunningTask {
    pub task_id: String,
    pub progress: (usize, usize),
}

/// 调度器。
pub struct Scheduler {
    store: Arc<parking_lot::Mutex<TaskStore>>,
    run_log: Arc<parking_lot::Mutex<RunLogStore>>,
    alerts: Arc<parking_lot::Mutex<AlertStore>>,
    notifier: Arc<Notifier>,
    /// 正在运行的任务（id -> 句柄）
    running: Arc<parking_lot::Mutex<std::collections::HashMap<String, Arc<dyn TaskJob>>>>,
    /// 结果已到、但**尚未被收口**的任务。
    ///
    /// 这个集合是「同一任务不重入」的关键：收集线程把结果投进 `finished` 之后、
    /// `collect` 真正更新任务状态之前，存在一个窗口。若此时只以 `running` 判断
    /// 是否在跑，任务会被判为「可以再次启动」而被跑第二遍 ——
    /// 表现为同一秒里留下两条运行记录（自动化测试抓到的真实缺陷）。
    awaiting_collect: Arc<parking_lot::Mutex<std::collections::HashSet<String>>>,
    /// 攒住的通知摘要（v1.1 §8.6 digest）：按任务收口，到点由 `tick` 发出。
    ///
    /// 只放内存**刻意**：摘要的寿命就是一个窗口（默认 300s），
    /// 落盘反而会带来「应用重启后发出一批过时告警」这种更糟的行为。
    /// 关闭时由 `flush_all` 取空发完（见 `shutdown`）。
    digest_buf: Arc<parking_lot::Mutex<Vec<alerts::digest::PendingDigest>>>,
    /// 攒住通知的通行道（按 `dedup_key` 记住，flush 时取用）。
    digest_channels: Arc<parking_lot::Mutex<std::collections::HashMap<String, Vec<String>>>>,
    /// 已完成、等待 Scheduler 轮询收口的结果通道
    finished: flume::Receiver<BackendOutcome>,
    finished_tx: flume::Sender<BackendOutcome>,
    cancel: Arc<AtomicBool>,
    tasks_path: PathBuf,
    runs_dir: PathBuf,
    alerts_path: PathBuf,
}

impl Scheduler {
    pub fn new(
        store: Arc<parking_lot::Mutex<TaskStore>>,
        alerts: Arc<parking_lot::Mutex<AlertStore>>,
        notifier: Arc<Notifier>,
        tasks_path: PathBuf,
        runs_dir: PathBuf,
        alerts_path: PathBuf,
    ) -> Self {
        let (finished_tx, finished) = flume::unbounded();
        let mut run_log = RunLogStore::new();
        // 启动时回填最近记录：列表行要立刻显示「上次结果」，
        // 否则重启后所有任务都显示「从未运行」直到下一次跑完。
        {
            let s = store.lock();
            for t in &s.tasks {
                run_log.warm(&runs_dir, &t.id);
            }
        }
        Self {
            store,
            run_log: Arc::new(parking_lot::Mutex::new(run_log)),
            alerts,
            notifier,
            running: Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new())),
            awaiting_collect: Arc::new(parking_lot::Mutex::new(std::collections::HashSet::new())),
            digest_buf: Arc::new(parking_lot::Mutex::new(Vec::new())),
            digest_channels: Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new())),
            finished,
            finished_tx,
            cancel: Arc::new(AtomicBool::new(false)),
            tasks_path,
            runs_dir,
            alerts_path,
        }
    }

    /// 请求停止一切（应用退出时调用）。
    ///
    /// 顺手把攒住的摘要**发完**：不 flush 就等于用户永远看不到那批告警 ——
    /// 攒住的窗口恰好在退出时没到点，是最容易漏掉的一类丢失。
    pub fn shutdown(&self) {
        self.cancel.store(true, Ordering::SeqCst);
        for job in self.running.lock().values() {
            job.cancel();
        }
        let n = self.flush_all_digests(unix_now_secs());
        if n > 0 {
            log::info!("退出前已发出 {n} 条攒住的摘要通知");
        }
    }

    /// 当前正在运行的任务快照。
    pub fn running(&self) -> Vec<RunningTask> {
        self.running
            .lock()
            .iter()
            .map(|(id, job)| RunningTask {
                task_id: id.clone(),
                progress: job.progress(),
            })
            .collect()
    }

    pub fn is_running(&self, task_id: &str) -> bool {
        self.running.lock().contains_key(task_id) || self.awaiting_collect.lock().contains(task_id)
    }

    /// 手动触发一个任务（右键「立即执行」）。
    ///
    /// 手动触发**不受** `enabled`、宽限窗口与**总开关**约束（用户就在眼前点了），
    /// 但仍受「同一任务不重入」约束：跑一半再点一次不会起第二份。
    ///
    /// 总开关放行手动触发是刻意的：它管的是「到点自动跑」，而手动触发是用户
    /// 此刻的明确指令。把它一起挡掉会让人以为「点了没反应」，而真正的原因是
    /// 一个藏在面板顶部的开关。
    pub fn run_now(&self, task_id: &str, backend: &dyn TaskBackend) -> Result<(), String> {
        let task = {
            let s = self.store.lock();
            s.get(task_id).cloned()
        };
        let Some(task) = task else {
            return Err(crate::i18n::ts(
                "任务已不存在".into(),
                "task no longer exists".into(),
            ));
        };
        self.start_task(task, backend, false)
    }

    /// 停止一个正在运行的任务。
    pub fn stop(&self, task_id: &str) -> bool {
        let job = self.running.lock().get(task_id).cloned();
        match job {
            Some(j) => {
                j.cancel();
                true
            }
            None => false,
        }
    }

    /// 一次调度 tick。
    ///
    /// 三段顺序**不可调换**：
    /// 1. **先收口**上一轮已完成的结果。放在最前是为了让「谁到点」基于最新状态
    ///    判断；若先判到点，一个刚跑完、结果还没记账的任务会被当成「仍在运行」
    ///    而白白记一条 Skipped。
    /// 2. 再决定谁到点并启动。
    /// 3. 最后处理启动过程中新到的结果（同一 tick 内跑完的极快任务）。
    ///
    /// 返回本次启动的任务数（UI 据此决定要不要重绘）。
    ///
    /// 是否自动执行**只看每个任务自己的 `enabled`** —— 没有全局总开关；
    /// 「新建任务默认停用」是安全底线（见 UI 侧任务模板与 AI 草案）。
    pub fn tick(&self, backend: &dyn TaskBackend) -> usize {
        let mut started = 0;
        let mut dirty = false;

        // 1) 收口已完成的结果
        while let Ok(outcome) = self.finished.try_recv() {
            self.collect(outcome);
            dirty = true;
        }

        // 2) 到点且可执行的任务
        let now = unix_now_secs();
        let due: Vec<Task> = {
            let s = self.store.lock();
            s.due(now)
                .into_iter()
                .filter_map(|id| s.get(&id).cloned())
                .collect()
        };
        for task in due {
            // 重入：上一次还在跑 -> 记 Skipped 并后推，不并发跑两份
            if self.is_running(&task.id) {
                self.record_skipped(
                    &task,
                    now,
                    crate::i18n::ts("上一次仍在运行".into(), "previous run still active".into()),
                );
                dirty = true;
                continue;
            }
            match self.start_task(task.clone(), backend, true) {
                Ok(()) => started += 1,
                Err(reason) => {
                    self.record_skipped(&task, now, reason);
                    dirty = true;
                }
            }
        }

        // 3) 同一 tick 内就跑完的任务，顺手收掉，避免多等一轮
        while let Ok(outcome) = self.finished.try_recv() {
            self.collect(outcome);
            dirty = true;
        }

        // 4) 到点的摘要窗口：把攒住的通知合并成一条发出
        if self.flush_due_digests(now) {
            dirty = true;
        }

        if dirty {
            self.persist();
        }
        started
    }

    /// 发出所有到点的摘要（返回是否发了东西）。
    ///
    /// 合并成**一条**而不是逐条重放：这正是 digest 存在的意义 ——
    /// 三十台机器同时告警，用户要的是一条「哪几台、什么问题」，不是三十条。
    pub fn flush_due_digests(&self, now: u64) -> bool {
        let due = alerts::digest::take_due(&mut self.digest_buf.lock(), now);
        if due.is_empty() {
            return false;
        }
        for d in due {
            let merged = d.merge(now);
            // 通道取自该任务这批通知**共用的**通道（同一任务同一批规则，
            // 通道一致；不一致时按第一条的通道走，并保留 dedup_key 便于追溯）
            let channels = self
                .digest_channels
                .lock()
                .iter()
                .find(|(k, _)| k.starts_with(&format!("{}|", d.task_id)))
                .map(|(_, c)| c.clone())
                .unwrap_or_else(|| vec!["log".into()]);
            let carrier = alerts::AlertRule {
                channels: channels.clone(),
                ..digest_carrier_rule()
            };
            let results = self.notifier.dispatch(&carrier, &merged);
            for r in &results {
                // 只为「失败」记警告：「跳过」是有意为之（通道未配置），
                // 每次都 warn 会在没配邮件的用户机器上刷出一片噪声。
                match &r.outcome {
                    alerts::notify::ChannelOutcome::Failed(e) => {
                        log::warn!(
                            "摘要通知派发失败 task={} channel={}: {e}",
                            d.task_id,
                            r.channel
                        );
                    }
                    alerts::notify::ChannelOutcome::Skipped(why) => {
                        log::info!(
                            "摘要通知跳过 channel={} task={}（{why}）",
                            r.channel,
                            d.task_id
                        );
                    }
                    alerts::notify::ChannelOutcome::Ok => {}
                }
            }
            log::info!("已发出摘要通知：task={} 合并 {} 条", d.task_id, d.len());
        }
        // 清掉已发出的通道记录，避免 map 越积越大
        self.digest_channels.lock().clear();
        true
    }

    /// 把所有攒住的摘要立刻发出去（应用退出前调用）。
    ///
    /// **必须做**：不 flush 就等于用户永远看不到那批告警 ——
    /// 攒住的窗口恰好在退出时没到点，是最容易漏掉的一类丢失。
    pub fn flush_all_digests(&self, now: u64) -> usize {
        let all = alerts::digest::take_all(&mut self.digest_buf.lock());
        if all.is_empty() {
            return 0;
        }
        let n = all.len();
        // 把 flush_at 拉到现在之前，复用同一条路径
        for d in &all {
            let merged = d.merge(now);
            let channels = self
                .digest_channels
                .lock()
                .iter()
                .find(|(k, _)| k.starts_with(&format!("{}|", d.task_id)))
                .map(|(_, c)| c.clone())
                .unwrap_or_else(|| vec!["log".into()]);
            let carrier = alerts::AlertRule {
                channels,
                ..digest_carrier_rule()
            };
            let _ = self.notifier.dispatch(&carrier, &merged);
        }
        self.digest_channels.lock().clear();
        n
    }

    /// 内部启动：`scheduled` 区分「调度到点」与「用户手动」，
    /// 前者要受宽限窗口约束、后者不受。
    fn start_task(
        &self,
        task: Task,
        backend: &dyn TaskBackend,
        scheduled: bool,
    ) -> Result<(), String> {
        let now = unix_now_secs();
        // 宽限窗口：只在调度路径上判
        if scheduled && now.saturating_sub(task.next_run_at) > GRACE_SECS {
            let missed = now.saturating_sub(task.next_run_at) / 60;
            return Err(crate::i18n::ts(
                format!("错过计划时间 {missed} 分钟，已跳过（不补跑）"),
                format!("missed schedule by {missed} min; skipped"),
            ));
        }
        if self.cancel.load(Ordering::SeqCst) {
            return Err(crate::i18n::ts(
                "应用正在退出".into(),
                "application is shutting down".into(),
            ));
        }

        let (job, rx) = backend.start(task.clone());
        // 结果统一收口到 `finished`：调度线程只做 tick，不阻塞等任何任务
        let finished_tx = self.finished_tx.clone();
        let running = self.running.clone();
        let awaiting = self.awaiting_collect.clone();
        let id = task.id.clone();
        running.lock().insert(id.clone(), job);
        std::thread::Builder::new()
            .name(format!("rs-task-collect-{id}"))
            .spawn(move || {
                // 只等**一条**结果：后端约定「一个任务一次运行只回一个 outcome」。
                //
                // **不在收到结果后就从 `running` 摘掉自己**：那会留下一个
                // 「已结束但还没被 collect 记账」的窗口，任务会在该窗口里被判为
                // 可再次启动而跑第二遍。改为投进 `awaiting_collect`，
                // 由 `collect` 在更新完任务状态后统一清理。
                if let Ok(outcome) = rx.recv() {
                    awaiting.lock().insert(id.clone());
                    let _ = finished_tx.send(outcome);
                } else {
                    // 后端放弃了（没发结果就 drop）：必须收尾，否则任务会永远
                    // 卡在「正在运行」而再也跑不起来。
                    awaiting.lock().remove(&id);
                    running.lock().remove(&id);
                }
            })
            .expect("spawn task collector");
        Ok(())
    }

    /// 收口一次运行结果：写记录、更新任务状态、跑告警、派发通知。
    fn collect(&self, outcome: BackendOutcome) {
        let now = unix_now_secs();
        let rec = &outcome.record;
        let task_id = rec.task_id.clone();

        // 1) 运行记录
        self.run_log.lock().push(&self.runs_dir, rec.clone());

        // 2) 任务上的运行态（末次结果、连败计数、下次触发）
        let mut auto_disabled: Option<String> = None;
        {
            let mut s = self.store.lock();
            if let Some(t) = s.get_mut(&task_id) {
                t.last_run_at = rec.started_at;
                t.last_status = rec.status;
                t.last_summary = Some(rec.one_line());
                match rec.status {
                    RunStatus::Ok | RunStatus::Partial => t.consecutive_failures = 0,
                    RunStatus::Failed => {
                        t.consecutive_failures = t.consecutive_failures.saturating_add(1)
                    }
                    // Skipped / Cancelled 不改变连败计数（它们不代表任务本身坏了）
                    _ => {}
                }
                if t.consecutive_failures >= crate::tasks::AUTO_DISABLE_AFTER_FAILS && t.enabled {
                    t.enabled = false;
                    t.last_summary = Some(crate::i18n::ts(
                        format!("连续失败 {} 次，已自动停用", t.consecutive_failures),
                        format!(
                            "disabled after {} consecutive failures",
                            t.consecutive_failures
                        ),
                    ));
                    auto_disabled = Some(t.name.clone());
                }
                // 后推下次触发（下一轮 tick 不会重复触发同一计划时刻）。
                //
                // 基准取 `now` 而不是「旧的 next_run_at」：`Trigger::Interval` 会把
                // 时刻对齐到 interval 的整数倍，若旧的计划时刻落在过去，对齐结果
                // 可能**仍在过去**，于是下一轮 tick 立刻又判一次「错过窗口」，
                // 每 tick 积一条 Skipped。这正是自动化测试里发现的真实缺陷。
                t.recompute_next(now);
            }
        }

        // 2.5) 任务状态已落地，现在才允许再次调度。
        //
        // 这一步的位置是**关键**：早一步摘标记，任务会在「已结束但 next_run_at
        // 还没更新」的窗口里被判为可再次启动而跑第二遍（实测会在同一秒留下
        // 两条运行记录）。晚一步则任务可能长时间显示为「正在运行」。
        self.awaiting_collect.lock().remove(&task_id);
        self.running.lock().remove(&task_id);

        // 3) 告警判定（含去重/抑制/恢复）+ 4) 派发
        //
        // 通道由告警引擎随通知成对带出（`Evaluation::channels`），这里不再
        // 反查规则 —— 没有字符串解析，也就没有「规则改名后通道失联」这类隐患。
        //
        // digest：任务自己的 `NotifyPolicy` 决定窗口。非 Critical 的通知
        // **不在本轮发出**，而是攒进 `digest_buf`，由 `tick` 按窗口收口成一条
        // —— 三十台机器同时内存告警不该变成三十封邮件。
        let digest_policy = self.store.lock().get(&task_id).map(|t| t.notify_policy());
        let mut dispatches: Vec<Dispatch> = {
            let mut alerts = self.alerts.lock();
            let rules = alerts.rules.clone();
            let ev = alerts::evaluate(
                &rules,
                &mut alerts.dedup,
                EvalInput {
                    task_id: &task_id,
                    task_name: &rec.task_name,
                    findings: &rec.findings,
                    now,
                },
                digest_policy.as_ref(),
            );
            let digested: Vec<alerts::Notification> =
                ev.digest.iter().map(|(n, _)| n.clone()).collect();
            alerts::apply_notification(&mut alerts.dedup, &ev.notifications, &digested, now);
            // 攒住的那批进缓冲（通道一起留着，flush 时才需要它们）
            if let Some(policy) = &digest_policy {
                let held: Vec<alerts::Notification> =
                    ev.digest.iter().map(|(n, _)| n.clone()).collect();
                alerts::digest::push(
                    &mut self.digest_buf.lock(),
                    &task_id,
                    &rec.task_name,
                    policy,
                    held,
                    now,
                );
                // 通道按 dedup_key 记住：flush 时按它派发
                for (n, ch) in &ev.digest {
                    self.digest_channels
                        .lock()
                        .insert(n.dedup_key.clone(), ch.clone());
                }
            }
            ev.paired()
                .into_iter()
                .map(|(n, channels)| Dispatch {
                    notification: n.clone(),
                    channels,
                })
                .collect()
        };
        if let Some(name) = auto_disabled {
            let rule = auto_disable_rule();
            dispatches.push(Dispatch {
                notification: alerts::notify::render_notification(
                    alerts::notify::NotificationDraft {
                        rule: &rule,
                        severity: alerts::Severity::Warning,
                        title: &crate::i18n::ts(
                            format!("任务已自动停用：{name}"),
                            format!("Task auto-disabled: {name}"),
                        ),
                        body: &crate::i18n::ts(
                            format!(
                                "{name} 连续失败达到阈值，已停止调度；请检查目标与凭证后手动启用"
                            ),
                            format!(
                                "{name} kept failing and has been disabled; check the target and credentials, then re-enable it"
                            ),
                        ),
                        now,
                        task_name: Some(&rec.task_name),
                        dedup_key: &format!("autodisable:{task_id}:{}", rec.started_at),
                        target: None,
                    },
                ),
                channels: rule.channels.clone(),
            });
        }
        for d in &dispatches {
            // 借用一条临时规则承载「该发到哪些通道」；`dispatch` 只读 channels
            let carrier = alerts::AlertRule {
                channels: d.channels.clone(),
                ..auto_disable_rule()
            };
            let results = self.notifier.dispatch(&carrier, &d.notification);
            for r in &results {
                // 同上：只为「失败」warn，「跳过」是有意为之（通道未配置）。
                match &r.outcome {
                    alerts::notify::ChannelOutcome::Failed(e) => {
                        log::warn!("通知派发失败 task={task_id} channel={}: {e}", r.channel);
                    }
                    alerts::notify::ChannelOutcome::Skipped(why) => {
                        log::info!("通知跳过 channel={} task={task_id}（{why}）", r.channel);
                    }
                    alerts::notify::ChannelOutcome::Ok => {}
                }
            }
        }
    }

    /// 记录一次「跳过」（目标为空、上一次仍在跑、错过窗口…）。
    ///
    /// **已停用的任务不再改写末次摘要**：自动停用的原因（「连续失败 N 次，已自动停用」）
    /// 比后续那些「错过计划时间」有用得多，被后者盖掉会让用户完全看不出
    /// 任务为什么停了 —— 而这条信息正是他恢复任务前最需要的。
    fn record_skipped(&self, task: &Task, now: u64, reason: String) {
        let rec = RunRecord {
            run_id: crate::connections::uuid_like(),
            task_id: task.id.clone(),
            task_name: task.name.clone(),
            started_at: now,
            finished_at: now,
            status: RunStatus::Skipped,
            targets: Vec::new(),
            machine: Vec::new(),
            conclusion: None,
            findings: Vec::new(),
            note: Some(reason.clone()),
            session: None,
        };
        self.run_log.lock().push(&self.runs_dir, rec);
        let mut s = self.store.lock();
        if let Some(t) = s.get_mut(&task.id) {
            t.last_run_at = now;
            if t.enabled {
                t.last_status = RunStatus::Skipped;
                t.last_summary = Some(reason);
            }
            // 同样以 `now` 为基准：跳过之后必须保证下次触发在未来，
            // 否则每个 tick 都会再记一次同样的跳过。
            t.recompute_next(now);
        }
    }

    fn persist(&self) {
        let store = self.store.lock();
        if let Err(e) = store.save(&self.tasks_path) {
            log::warn!("任务落盘失败: {e}");
        }
        drop(store);
        let alerts = self.alerts.lock();
        if let Err(e) = alerts.save(&self.alerts_path) {
            log::warn!("告警规则落盘失败: {e}");
        }
    }

    /// 主动落盘（UI 改完任务定义后调用）。
    pub fn save(&self) {
        self.persist();
    }

    /// UI 全部任务变更的**唯一入口**：改 store → 落盘 → 返回新快照。
    ///
    /// 为什么不让 UI 自己改自己那份 `TaskStore`：`tick` 的收口会用调度器的
    /// store 覆盖 UI 的副本 —— UI 只改本地副本的话，下一次 tick（≤1 秒）就会
    /// 把它抹掉（表现为「删掉的任务又回来了」）。改走这里，两边永远同步。
    pub fn update_store(&self, f: impl FnOnce(&mut TaskStore)) -> TaskStore {
        let snapshot = {
            let mut s = self.store.lock();
            f(&mut s);
            s.clone()
        };
        self.persist();
        snapshot
    }

    /// 任务仓库的只读快照。
    ///
    /// UI 渲染需要看到调度器改过的东西（`next_run_at` / 连败计数 / 自动停用），
    /// 但不能把锁交出去：一个卡住的渲染会让所有任务推迟触发。
    pub fn store_snapshot(&self) -> TaskStore {
        self.store.lock().clone()
    }

    /// 告警仓库的只读快照（设置面板展示规则用）。
    pub fn alerts_snapshot(&self) -> AlertStore {
        self.alerts.lock().clone()
    }

    /// 运行记录（UI 展开某个任务时读）。
    pub fn recent_runs(&self, task_id: &str) -> Vec<RunRecord> {
        self.run_log.lock().recent(task_id).to_vec()
    }

    /// 最近一次运行的「身份元组」：`(run_id, finished_at, 有没有会话痕迹)`。
    ///
    /// 给 UI 的每秒轮询用（发现「刚跑完的那次」要不要在聊天记录里建会话）。
    /// 为什么不直接给整条 [`RunRecord`]：会话里带着最多 8 KiB×N 的轨迹明细，
    /// 每秒给每个任务克隆一份是不可接受的 —— 这里只取三个标量，
    /// 真要建会话时再按 `run_id` 取整条。
    pub fn latest_run_meta(&self, task_id: &str) -> Option<(String, u64, bool)> {
        self.run_log
            .lock()
            .latest(task_id)
            .map(|r| (r.run_id.clone(), r.finished_at, r.session.is_some()))
    }

    /// 最近一次运行（列表行的「上次结果」）。
    pub fn latest_run(&self, task_id: &str) -> Option<RunRecord> {
        self.run_log.lock().latest(task_id).cloned()
    }
}

/// 自动停用通知用的伪规则（不参与去重表，仅用于渲染与选通道）。
fn auto_disable_rule() -> alerts::AlertRule {
    alerts::AlertRule {
        id: "builtin.task_disabled".into(),
        name: crate::i18n::ts("任务自动停用".into(), "Task auto-disabled".into()),
        enabled: true,
        task_ids: Vec::new(),
        target_profiles: Vec::new(),
        condition: alerts::AlertCondition::InspectionFailed { min_failed: 1 },
        suppress_secs: DEFAULT_SUPPRESS_SECS,
        notify_recovery: false,
        severity: alerts::Severity::Warning,
        channels: vec!["log".into()],
    }
}

/// 试运行结果（v1.1 §6.5）。
///
/// 「试运行」把任务会做的一切**可计算**的部分都算出来给用户看，但**绝不**
/// 建立 SSH 执行连接、不执行远程命令、不下载文件、不发送通知。
///
/// 为什么值得单独做：批量任务与 AI 生成的草案最容易出的错是「目标选错了」
/// 或「命令其实是 Confirm/Blocked 级」，而这两种错在真正执行前都能用纯计算查出来。
/// 让用户在点「跑」之前就看见范围与风险，比事后从运行记录里复盘便宜得多。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DryRunResult {
    /// 解析后的目标（稳定顺序）
    pub targets: Vec<String>,
    /// 目标展示名（与 `targets` 一一对应）
    pub target_names: Vec<String>,
    /// 动作类别（`TaskAction::kind_label`）
    pub action: String,
    /// 命令风险级（非命令类动作为 `None`）
    #[serde(default)]
    pub risk: Option<String>,
    pub concurrency: usize,
    pub timeout_secs: u64,
    /// 需要提醒的问题（目标为空、Conn 无口令、命令会被拦…）
    pub warnings: Vec<String>,
    /// 是否会被直接拦下（Blocked / 无目标）
    pub blocked: bool,
}

impl DryRunResult {
    /// 一行摘要（UI 折叠区与通知里用）。
    pub fn one_line(&self) -> String {
        use crate::i18n::ts;
        if self.blocked {
            return self
                .warnings
                .first()
                .cloned()
                .unwrap_or_else(|| ts("会被拦下".into(), "would be blocked".into()));
        }
        ts(
            format!(
                "{} · {} 台 · 并发 {}",
                self.action,
                self.targets.len(),
                self.concurrency
            ),
            format!(
                "{} · {} hosts · concurrency {}",
                self.action,
                self.targets.len(),
                self.concurrency
            ),
        )
    }
}

/// 对任务做一次试运行（**纯计算，无任何 IO**）。
///
/// 与真实执行共用同一份目标解析与风险判定，所以「试运行说什么、真跑就做什么」——
/// 两处各写一遍的话，试运行会退化成一份看起来很美但会骗人的预览。
pub fn dry_run(task: &Task, conn: &crate::connections::ConnectionStore) -> DryRunResult {
    use crate::agent::TargetSelector;
    let action = task.action.clone();
    let target = action.target();
    let mut warnings: Vec<String> = Vec::new();
    let mut blocked = false;

    // 1) 解析目标（与执行侧同一入口）
    let selector = if !target.profiles.is_empty() {
        TargetSelector::Profiles(target.profiles.clone())
    } else if !target.groups.is_empty() {
        let mut ids = Vec::new();
        for g in &target.groups {
            match crate::agent::resolve_targets(conn, &TargetSelector::Group(g.clone())) {
                Ok(mut v) => ids.append(&mut v),
                Err(e) => {
                    warnings.push(e.to_string());
                    blocked = true;
                }
            }
        }
        TargetSelector::Profiles(ids)
    } else if !target.tags.is_empty() {
        let mut ids = Vec::new();
        for t in &target.tags {
            match crate::agent::resolve_targets(conn, &TargetSelector::Tag(t.clone())) {
                Ok(mut v) => ids.append(&mut v),
                Err(e) => {
                    warnings.push(e.to_string());
                    blocked = true;
                }
            }
        }
        TargetSelector::Profiles(ids)
    } else {
        TargetSelector::All
    };

    let mut ids = Vec::new();
    if !blocked {
        match crate::agent::resolve_targets(conn, &selector) {
            Ok(mut v) => ids.append(&mut v),
            Err(e) => {
                warnings.push(e.to_string());
                blocked = true;
            }
        }
    }
    // 2) exclude 最后生效（与执行侧同一顺序）
    if !target.exclude.is_empty() {
        let drop: std::collections::HashSet<&str> =
            target.exclude.iter().map(|s| s.as_str()).collect();
        ids.retain(|id| !drop.contains(id.as_str()));
    }

    // 3) 无口令的连接在无人值守下会被跳过 —— 试运行时就要说出来，
    //    免得用户以为「跑了 10 台」而实际只跑了 8 台
    let mut skip_nocred: Vec<String> = Vec::new();
    for id in &ids {
        if let Some(p) = conn.profile(id) {
            if p.password.is_none() && p.key_path.is_none() {
                skip_nocred.push(p.name.clone());
            }
        }
    }
    if !skip_nocred.is_empty() {
        warnings.push(crate::i18n::ts(
            format!(
                "{} 台未保存口令，无人值守执行时会被跳过：{}",
                skip_nocred.len(),
                skip_nocred.join("、")
            ),
            format!(
                "{} host(s) have no stored credential and will be skipped: {}",
                skip_nocred.len(),
                skip_nocred.join(", ")
            ),
        ));
    }

    // 4) 参数校验与风险级
    if let Err(e) = action.validate() {
        warnings.push(e);
        blocked = true;
    }
    let risk = match &action {
        TaskAction::ExecMulti {
            command,
            allow_mutating,
            ..
        } => {
            let r = crate::ai::assess_command(command);
            let tag = match r {
                crate::ai::Risk::Safe => "safe",
                crate::ai::Risk::Confirm => "confirm",
                crate::ai::Risk::Blocked => "blocked",
            };
            if r == crate::ai::Risk::Blocked {
                warnings.push(crate::i18n::ts(
                    "该命令属于不可逆破坏操作，执行时会被永久拦截".into(),
                    "the command is irreversibly destructive and will be blocked".into(),
                ));
                blocked = true;
            } else if r == crate::ai::Risk::Confirm && !*allow_mutating {
                warnings.push(crate::i18n::ts(
                    "该命令会改变服务器状态，未开启「允许状态变更命令」时会被跳过".into(),
                    "the command changes state and will be skipped unless allowed".into(),
                ));
            }
            Some(tag.to_string())
        }
        _ => None,
    };

    // 5) 并发与超时（与执行侧同一常量）
    let concurrency = crate::exec::DEFAULT_CONCURRENCY;
    let timeout_secs = match &action {
        TaskAction::ExecMulti { timeout_secs, .. } => {
            (*timeout_secs).clamp(1, crate::tasks::MAX_EXEC_TIMEOUT_SECS)
        }
        _ => 0,
    };

    let names: Vec<String> = ids
        .iter()
        .map(|id| {
            conn.profile(id)
                .map(|p| p.name.clone())
                .unwrap_or_else(|| id.clone())
        })
        .collect();

    if ids.is_empty() && !blocked {
        warnings.push(crate::i18n::ts(
            "解析出的目标为空，执行时会被跳过".into(),
            "resolved target set is empty; the run would be skipped".into(),
        ));
        blocked = true;
    }

    DryRunResult {
        targets: ids,
        target_names: names,
        action: action.kind_label().to_string(),
        risk,
        concurrency,
        timeout_secs,
        warnings,
        blocked,
    }
}

/// 摘要通知用的伪规则（只承载通道，不参与去重表）。
fn digest_carrier_rule() -> alerts::AlertRule {
    alerts::AlertRule {
        id: "builtin.digest".into(),
        name: crate::i18n::ts("预警摘要".into(), "Alert digest".into()),
        enabled: true,
        task_ids: Vec::new(),
        target_profiles: Vec::new(),
        condition: alerts::AlertCondition::InspectionFailed { min_failed: 1 },
        suppress_secs: 0,
        notify_recovery: false,
        severity: alerts::Severity::Warning,
        channels: vec!["log".into()],
    }
}

/// 任务文件与运行记录目录的默认落点（由调用方传入 config 目录）。
pub fn tasks_path(config_dir: &Path) -> PathBuf {
    config_dir.join("tasks.json")
}

pub fn runs_dir(config_dir: &Path) -> PathBuf {
    config_dir.join("task-runs")
}

pub fn alerts_path(config_dir: &Path) -> PathBuf {
    config_dir.join("alert_rules.json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alerts::{AlertStore, LogChannel, Notifier};
    use crate::connections::ConnectionStore;
    use crate::tasks::{TaskAction, TaskTarget, Trigger};
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrd};

    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "rs-runner-{tag}-{}-{}",
            std::process::id(),
            unique_suffix()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// 单调递增的测试目录后缀。
    ///
    /// **不能**用秒级时间戳：同一秒内重跑（或两个用例并行）会拿到同名目录，
    /// 上一条留下的 `task-runs/*.jsonl` 会在 Scheduler 启动的「回填最近记录」
    /// 里被读回来 —— 于是「应当只有 1 条记录」的断言莫名其妙变成 2 条。
    fn unique_suffix() -> u64 {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        NEXT.fetch_add(1, AtomicOrd::SeqCst) as u64
            ^ (std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0))
    }

    fn task_at(id: &str, next_run_at: u64) -> Task {
        let mut t = Task::new(
            "测试任务",
            Trigger::Interval { every_secs: 3600 },
            TaskAction::ExecMulti {
                target: TaskTarget {
                    profiles: vec!["p1".into()],
                    ..Default::default()
                },
                command: "df -h".into(),
                timeout_secs: 15,
                allow_mutating: false,
            },
            0,
        );
        t.id = id.to_string();
        t.next_run_at = next_run_at;
        t
    }

    /// 假后端：可控制「立即完成」还是「挂着不完成」。
    struct FakeBackend {
        starts: Arc<AtomicUsize>,
        /// true = 不返回结果（模拟长时间运行）
        hang: bool,
        status: RunStatus,
        record_findings: Vec<alerts::Finding>,
    }

    struct FakeJob {
        cancelled: Arc<AtomicBool>,
        total: usize,
    }

    impl TaskJob for FakeJob {
        fn cancel(&self) {
            self.cancelled.store(true, Ordering::SeqCst);
        }
        fn progress(&self) -> (usize, usize) {
            (0, self.total)
        }
    }

    impl TaskBackend for FakeBackend {
        fn start(&self, task: Task) -> (Arc<dyn TaskJob>, flume::Receiver<BackendOutcome>) {
            self.starts.fetch_add(1, AtomicOrd::SeqCst);
            let (tx, rx) = flume::unbounded();
            let cancelled = Arc::new(AtomicBool::new(false));
            let job: Arc<dyn TaskJob> = Arc::new(FakeJob {
                cancelled: cancelled.clone(),
                total: 1,
            });
            if self.hang {
                return (job, rx);
            }
            let now = unix_now_secs();
            let rec = RunRecord {
                run_id: crate::connections::uuid_like(),
                task_id: task.id.clone(),
                task_name: task.name.clone(),
                started_at: now,
                finished_at: now,
                status: self.status,
                targets: Vec::new(),
                machine: Vec::new(),
                conclusion: None,
                findings: self.record_findings.clone(),
                note: None,
                session: None,
            };
            let _ = tx.send(BackendOutcome { record: rec });
            (job, rx)
        }
    }

    /// 等到调度器收口完一次运行（最多 2 秒）。
    ///
    /// 用轮询而不是固定 `sleep`：收口发生在后端的收集线程里，固定等待在慢机器上
    /// 会随机失败 —— 这类脆弱的时序断言是「测试偶发红」的最常见来源。
    fn wait_collected(s: &Scheduler, task_id: &str) {
        for _ in 0..200 {
            if !s.is_running(task_id) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn scheduler(dir: &Path, tasks: Vec<Task>) -> (Scheduler, Arc<parking_lot::Mutex<TaskStore>>) {
        let mut store = TaskStore::default();
        for t in tasks {
            store.add(t);
        }
        let store = Arc::new(parking_lot::Mutex::new(store));
        let alerts = Arc::new(parking_lot::Mutex::new(AlertStore::default()));
        let notifier = Arc::new(Notifier::new(vec![Arc::new(LogChannel::new(
            dir.join("alerts.log"),
        ))]));
        let s = Scheduler::new(
            store.clone(),
            alerts,
            notifier,
            tasks_path(dir),
            runs_dir(dir),
            alerts_path(dir),
        );
        (s, store)
    }

    #[test]
    fn due_task_runs_once_and_next_time_advances() {
        let dir = tmp_dir("due");
        let now = unix_now_secs();
        let (s, store) = scheduler(&dir, vec![task_at("t1", now - 1)]);
        let be = FakeBackend {
            starts: Arc::new(AtomicUsize::new(0)),
            hang: false,
            status: RunStatus::Ok,
            record_findings: Vec::new(),
        };
        assert_eq!(s.tick(&be), 1, "到点任务应被启动");
        // 收口结果
        std::thread::sleep(Duration::from_millis(50));
        s.tick(&be);
        let t = store.lock().get("t1").cloned().unwrap();
        assert!(t.next_run_at > now, "跑完必须把下次触发推到未来");
        assert_eq!(t.last_status, RunStatus::Ok);
        assert_eq!(t.consecutive_failures, 0);
        // 再 tick 不应重复启动（下次时刻在未来）
        assert_eq!(s.tick(&be), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missed_window_is_skipped_not_caught_up() {
        let dir = tmp_dir("missed");
        let now = unix_now_secs();
        // 计划时刻已过去 10 分钟，远超宽限窗口
        let (s, store) = scheduler(&dir, vec![task_at("t1", now - 600)]);
        let be = FakeBackend {
            starts: Arc::new(AtomicUsize::new(0)),
            hang: false,
            status: RunStatus::Ok,
            record_findings: Vec::new(),
        };
        let started = s.tick(&be);
        assert_eq!(started, 0, "错过窗口不得补跑");
        assert_eq!(be.starts.load(AtomicOrd::SeqCst), 0, "后端不该被调用");
        let t = store.lock().get("t1").cloned().unwrap();
        assert_eq!(t.last_status, RunStatus::Skipped);
        assert!(
            t.last_summary.as_deref().unwrap_or("").contains("错过"),
            "要说明为什么跳过：{:?}",
            t.last_summary
        );
        assert!(t.next_run_at > now, "跳过也要把下次推到未来");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn within_grace_window_still_runs() {
        let dir = tmp_dir("grace");
        let now = unix_now_secs();
        let (s, _) = scheduler(&dir, vec![task_at("t1", now - 5)]);
        let be = FakeBackend {
            starts: Arc::new(AtomicUsize::new(0)),
            hang: false,
            status: RunStatus::Ok,
            record_findings: Vec::new(),
        };
        assert_eq!(s.tick(&be), 1, "宽限窗口内应当执行");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reentry_is_skipped_not_duplicated() {
        let dir = tmp_dir("reentry");
        let now = unix_now_secs();
        let (s, _store) = scheduler(&dir, vec![task_at("t1", now - 1)]);
        // 后端挂着不返回 -> 任务一直处于运行中
        let be = FakeBackend {
            starts: Arc::new(AtomicUsize::new(0)),
            hang: true,
            status: RunStatus::Ok,
            record_findings: Vec::new(),
        };
        assert_eq!(s.tick(&be), 1);
        assert!(s.is_running("t1"));

        // 把 next_run_at 再拉回过去，模拟同一任务又到点
        {
            let mut st = _store.lock();
            st.get_mut("t1").unwrap().next_run_at = unix_now_secs() - 1;
        }
        let started = s.tick(&be);
        assert_eq!(started, 0, "重入不得起第二份");
        assert_eq!(be.starts.load(AtomicOrd::SeqCst), 1, "后端只应被调用一次");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn consecutive_failures_auto_disable_the_task() {
        let dir = tmp_dir("autodisable");
        let now = unix_now_secs();
        let (s, store) = scheduler(&dir, vec![task_at("t1", now - 1)]);
        let be = FakeBackend {
            starts: Arc::new(AtomicUsize::new(0)),
            hang: false,
            status: RunStatus::Failed,
            record_findings: Vec::new(),
        };
        for _ in 0..crate::tasks::AUTO_DISABLE_AFTER_FAILS {
            {
                let mut st = store.lock();
                if !st.get("t1").map(|t| t.enabled).unwrap_or(false) {
                    break; // 已停用：后续迭代不再有意义
                }
                st.get_mut("t1").unwrap().next_run_at = unix_now_secs() - 1;
            }
            s.tick(&be);
            wait_collected(&s, "t1");
            s.tick(&be);
            wait_collected(&s, "t1");
        }
        let t = store.lock().get("t1").cloned().unwrap();
        assert!(
            !t.enabled,
            "连续失败 {} 次后必须自动停用（实际计数 {}）",
            crate::tasks::AUTO_DISABLE_AFTER_FAILS,
            t.consecutive_failures
        );
        assert!(
            t.last_summary.as_deref().unwrap_or("").contains("停用"),
            "摘要要说明已停用：{:?}",
            t.last_summary
        );
        // 已停用的任务不再被调度
        {
            let mut st = store.lock();
            st.get_mut("t1").unwrap().next_run_at = unix_now_secs() - 1;
        }
        assert_eq!(s.tick(&be), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn skipped_runs_do_not_count_as_failures() {
        let dir = tmp_dir("skipcount");
        let now = unix_now_secs();
        let (s, store) = scheduler(&dir, vec![task_at("t1", now - 1)]);
        let be = FakeBackend {
            starts: Arc::new(AtomicUsize::new(0)),
            hang: true,
            status: RunStatus::Ok,
            record_findings: Vec::new(),
        };
        s.tick(&be); // 启动并挂着
        for _ in 0..3 {
            {
                let mut st = store.lock();
                st.get_mut("t1").unwrap().next_run_at = unix_now_secs() - 1;
            }
            s.tick(&be); // 每次都是「上一次仍在运行」-> Skipped
        }
        let t = store.lock().get("t1").cloned().unwrap();
        assert_eq!(
            t.consecutive_failures, 0,
            "跳过不代表任务坏了，不该累计失败"
        );
        assert!(t.enabled, "跳过不该把任务停用");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn manual_run_ignores_schedule_and_disabled_flag() {
        let dir = tmp_dir("manual");
        let now = unix_now_secs();
        let mut t = task_at("t1", now - 99999);
        t.enabled = false;
        let (s, _) = scheduler(&dir, vec![t]);
        let be = FakeBackend {
            starts: Arc::new(AtomicUsize::new(0)),
            hang: false,
            status: RunStatus::Ok,
            record_findings: Vec::new(),
        };
        // 调度路径不跑（已停用）
        assert_eq!(s.tick(&be), 0);
        // 手动路径照跑
        assert!(s.run_now("t1", &be).is_ok());
        assert_eq!(be.starts.load(AtomicOrd::SeqCst), 1);
        // 不存在的任务给出可读错误
        let err = s.run_now("nope", &be).unwrap_err();
        assert!(!err.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stop_cancels_the_running_job() {
        let dir = tmp_dir("stop");
        let now = unix_now_secs();
        let (s, _) = scheduler(&dir, vec![task_at("t1", now - 1)]);
        let be = FakeBackend {
            starts: Arc::new(AtomicUsize::new(0)),
            hang: true,
            status: RunStatus::Ok,
            record_findings: Vec::new(),
        };
        s.tick(&be);
        assert!(s.stop("t1"), "应当能找到并停止");
        assert!(!s.stop("nope"), "不存在的任务返回 false");
        let running = s.running();
        assert_eq!(running.len(), 1);
        assert_eq!(running[0].task_id, "t1");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_records_are_kept_and_readable() {
        let dir = tmp_dir("records");
        let now = unix_now_secs();
        let (s, _) = scheduler(&dir, vec![task_at("t1", now - 1)]);
        let be = FakeBackend {
            starts: Arc::new(AtomicUsize::new(0)),
            hang: false,
            status: RunStatus::Partial,
            record_findings: Vec::new(),
        };
        s.tick(&be);
        wait_collected(&s, "t1");
        s.tick(&be);
        let runs = s.recent_runs("t1");
        assert_eq!(
            runs.len(),
            1,
            "应当只留一条运行记录（后端被启动 {} 次，记录：{:?}）",
            be.starts.load(AtomicOrd::SeqCst),
            runs.iter()
                .map(|r| (r.status, r.started_at, r.note.clone()))
                .collect::<Vec<_>>()
        );
        assert_eq!(runs[0].status, RunStatus::Partial);
        assert!(s.latest_run("t1").is_some());
        // 记录落到磁盘（跨重启可见）
        let file = crate::tasks::run_log_path(&runs_dir(&dir), "t1");
        assert!(file.exists(), "运行记录必须落盘：{}", file.display());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 告警链路走通：finding 进 -> 通知出（落进 alerts.log）。
    ///
    /// 这条用例用的是 `Critical` finding，**故意**如此：它验证的是
    /// 「Critical 直发」这条路径（不会被 digest 窗口攒住）。
    #[test]
    fn findings_trigger_notifications_and_write_to_the_log_channel() {
        let dir = tmp_dir("alerts");
        let now = unix_now_secs();
        let (s, _) = scheduler(&dir, vec![task_at("t1", now - 1)]);
        let finding = alerts::Finding {
            kind: alerts::FindingKind::Anomaly,
            profile_id: "p1".into(),
            profile_name: "prod-01".into(),
            rule_id: "builtin.load".into(),
            fingerprint: alerts::metric_fingerprint("mem", 90.0),
            severity: alerts::Severity::Critical,
            message: "内存使用率 94%".into(),
            evidence: None,
        };
        let be = FakeBackend {
            starts: Arc::new(AtomicUsize::new(0)),
            hang: false,
            status: RunStatus::Partial,
            record_findings: vec![finding],
        };
        s.tick(&be);
        wait_collected(&s, "t1");
        s.tick(&be);

        let log = std::fs::read_to_string(dir.join("alerts.log")).unwrap_or_default();
        assert!(
            // 单条通知的正文是 finding 的 message；Critical 必须立即出现，
            // 而不是被 digest 窗口攒住
            log.contains("内存使用率"),
            "Critical 告警必须立即落进 log 通道：[{log}] 攒住数={}",
            s.digest_buf.lock().len()
        );
        // 去重状态已落盘（重启后抑制窗口仍有效）
        let save = std::fs::read_to_string(alerts_path(&dir)).unwrap();
        assert!(save.contains("dedup"), "去重状态要落盘：{save}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// v1.1 §8.6 / AC-24：同一任务一次运行的多条告警合并成**一条**通知。
    ///
    /// 关键行为有三处，缺一不可：
    /// 1. Warning 级被攒住（本轮不发）；
    /// 2. 到点后合并成一条（不是逐条重放）；
    /// 3. 退出前必须能取空发完（否则用户永远看不到）。
    #[test]
    fn digest_merges_many_alerts_into_one_and_flushes() {
        let dir = tmp_dir("digest");
        let now = unix_now_secs();
        let (s, store) = scheduler(&dir, vec![task_at("t1", now - 1)]);
        // 显式给一个短窗口：默认 300s 会让用例得等 5 分钟
        {
            let mut st = store.lock();
            let t = st.get_mut("t1").unwrap();
            t.notify = Some(crate::tasks::NotifyPolicy {
                digest_window_secs: 1,
                ..Default::default()
            });
        }
        // 三台机器各自内存告警（Warning）
        let findings: Vec<alerts::Finding> = (1..=3)
            .map(|i| alerts::Finding {
                kind: alerts::FindingKind::Anomaly,
                profile_id: format!("p{i}"),
                profile_name: format!("prod-0{i}"),
                rule_id: "builtin.load".into(),
                fingerprint: alerts::metric_fingerprint("mem", 90.0),
                severity: alerts::Severity::Warning,
                message: format!("内存使用率 9{i}%"),
                evidence: None,
            })
            .collect();
        let be = FakeBackend {
            starts: Arc::new(AtomicUsize::new(0)),
            hang: false,
            status: RunStatus::Partial,
            record_findings: findings,
        };
        s.tick(&be);
        wait_collected(&s, "t1");
        s.tick(&be);

        // 攒住：三条 Warning 都还没进 log 通道
        let log_before = std::fs::read_to_string(dir.join("alerts.log")).unwrap_or_default();
        assert!(
            !log_before.contains("内存使用率"),
            "窗口未到点不该发：{log_before}"
        );

        // 到点：合并成一条。
        //
        // ⚠️ 这里**不能**用 `sleep(...)` + `unix_now_secs()` 来"等窗口到点"。
        // 首版是 `sleep(1100ms)`，在 `cargo test --workspace`（并行、机器忙）下
        // 偶发必红，而单跑本用例几乎必绿 —— 曾据此误判为"睡 1.1s 不够长"。
        // 实测（`DIAG2` 探针）证明**不是**：失败时缓冲里明明有那条摘要
        // （`buf=1`），是 `flush_due_digests` **判定未到点**。根因在**秒的进位**：
        //   - 窗口落成 `flush_at = now_a + 1`（`now_a` = `collect()` 里的整秒）；
        //   - 用例手上的 `now` 是**更早**那次 `unix_now_secs()` 的整秒；
        //   - `collect()` 跑在独立的 worker/收口路径上，`unix_now_secs()` 是
        //     `as_secs()` 截断 ⇒ `now_a` 可以比 `now` 大 1（跨了个秒边界）。
        // 于是 `flush_at = now + 2 > now + 3` 不成立时判未到点？不 —— 是
        // `flush_at = now + 2`，`now + 3 >= flush_at` 本应成立。真正踩的是
        // **更大的进位**：`collect` 若落在 `now` 之后，`flush_at` 最大到
        // `now + 2`；而给 `now + 3` 只留了 1 秒余量，任何额外的调度延迟
        // （worker 线程被操作系统挂起、`wait_collected` 的 10ms 轮询步进）
        // 都会把 `flush_at` 推到 `now + 3` 之上。
        //
        // 修法：**别猜偏移量**。直接把缓冲里最大的 `flush_at` 读出来，
        // 喂一个它 + 1 —— 无论 `collect` 用了哪个整秒、机器多慢，都必然到点。
        // 用例从此与系统时钟和负载彻底无关（这才是"确定性"的正解，
        // 而不是把 sleep 从 1100ms 加到 3000ms，那只是把失败概率换个机器重现）。
        // 喂 `flush_at` 本身即可：`PendingDigest::is_due` 判的是 `now >= flush_at`，
        // 取等号就算到点。这样连"再多留一秒余量"都不需要。
        let flush_at = s
            .digest_buf
            .lock()
            .iter()
            .map(|d| d.flush_at)
            .max()
            .expect("窗口未到点这一步就该有攒住的摘要了（上面刚断言过它没进 log）");
        let flushed = s.flush_due_digests(flush_at);
        assert!(flushed, "窗口到点必须发出摘要");
        let log = std::fs::read_to_string(dir.join("alerts.log")).unwrap_or_default();
        let lines: Vec<&str> = log.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(
            lines.len(),
            1,
            "三台机器的告警必须合并成**一条**通知，而不是三条：{log}"
        );
        let first = lines[0];
        assert!(first.contains("合并 3 条"), "标题要写清合并了几条：{first}");
        // 三台都要出现在这一条里（用户扫一眼就知道是哪几台）
        for i in 1..=3 {
            assert!(
                first.contains(&format!("prod-0{i}")),
                "第 {i} 台必须出现在摘要里：{first}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 退出前必须把攒住的摘要发完 —— 否则那批告警用户永远看不到。
    #[test]
    fn shutdown_flushes_pending_digests() {
        let dir = tmp_dir("digest-exit");
        let now = unix_now_secs();
        let (s, store) = scheduler(&dir, vec![task_at("t1", now - 1)]);
        {
            let mut st = store.lock();
            // 窗口给得很长：只有「退出前 flush」这一条路径能把它发出去
            st.get_mut("t1").unwrap().notify = Some(crate::tasks::NotifyPolicy {
                digest_window_secs: 3600,
                ..Default::default()
            });
        }
        let be = FakeBackend {
            starts: Arc::new(AtomicUsize::new(0)),
            hang: false,
            status: RunStatus::Partial,
            record_findings: vec![alerts::Finding {
                kind: alerts::FindingKind::Anomaly,
                profile_id: "p1".into(),
                profile_name: "prod-01".into(),
                rule_id: "builtin.load".into(),
                fingerprint: alerts::metric_fingerprint("mem", 90.0),
                severity: alerts::Severity::Warning,
                message: "内存使用率 94%".into(),
                evidence: None,
            }],
        };
        s.tick(&be);
        wait_collected(&s, "t1");
        s.tick(&be);
        assert!(
            !std::fs::read_to_string(dir.join("alerts.log"))
                .unwrap_or_default()
                .contains("prod-01"),
            "窗口 1 小时，此刻不该发"
        );

        s.shutdown();
        let log = std::fs::read_to_string(dir.join("alerts.log")).unwrap_or_default();
        assert!(
            // 摘要正文用的是**目标名 + 规则名**（不含 finding 的原始 message），
            // 所以这里断言目标名
            log.contains("prod-01"),
            "退出前必须把攒住的告警发出去：{log}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tasks_and_alerts_are_persisted_on_change() {
        let dir = tmp_dir("persist");
        let now = unix_now_secs();
        let (s, _) = scheduler(&dir, vec![task_at("t1", now - 1)]);
        let be = FakeBackend {
            starts: Arc::new(AtomicUsize::new(0)),
            hang: false,
            status: RunStatus::Ok,
            record_findings: Vec::new(),
        };
        s.tick(&be);
        wait_collected(&s, "t1");
        s.tick(&be);
        let saved = TaskStore::load(&tasks_path(&dir));
        assert_eq!(saved.len(), 1);
        assert!(saved.get("t1").is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 回归：UI 的删除必须**经 `update_store` 落到调度器的 store**，之后的
    /// tick 不得让被删的任务复活。
    ///
    /// 旧实现里 UI 只改自己那份副本、调度器持另一份：右键删除后 ≤1 秒的 tick
    /// 就用调度器快照把 UI 副本整个盖回去，用户看到「删掉的任务又回来了」。
    /// 这里按旧路径（改副本、不改调度器）反证一次，再按新路径（`update_store`）
    /// 正断言：快照、盘上文件、tick 之后三者都没有那条任务。
    #[test]
    fn ui_deletion_via_update_store_survives_the_next_tick() {
        let dir = tmp_dir("ui-delete");
        let now = unix_now_secs();
        let (s, store) = scheduler(&dir, vec![task_at("t1", now - 1), task_at("t2", now - 1)]);
        let be = FakeBackend {
            starts: Arc::new(AtomicUsize::new(0)),
            hang: false,
            status: RunStatus::Ok,
            record_findings: Vec::new(),
        };

        // 反证：UI 侧即使删掉了自己的副本，调度器里仍是两条 —— 两个 store
        // 是分开的。旧实现就停在这一步（只删副本），随后 tick 用调度器快照
        // 覆盖 UI 副本，被删的任务便「复活」了。
        let mut ui_copy = s.store_snapshot();
        ui_copy.remove("t1");
        assert_eq!(ui_copy.len(), 1);
        assert_eq!(s.store_snapshot().len(), 2, "反证前提：调度器未被改动");

        // 正路径：UI 通过唯一入口删除，取回的快照就是新状态
        let snapshot = s.update_store(|st| {
            st.remove("t1");
        });
        assert_eq!(snapshot.len(), 1, "删除后的快照必须只剩 t2");
        assert!(snapshot.get("t1").is_none());
        assert!(snapshot.get("t2").is_some());

        // 落盘：文件里也没有了
        let saved = TaskStore::load(&tasks_path(&dir));
        assert_eq!(saved.len(), 1);
        assert!(saved.get("t1").is_none(), "删除必须落盘，重启后不得复活");

        // tick 不得复活：t2 正常到点执行，t1 不出现
        s.tick(&be);
        wait_collected(&s, "t2");
        s.tick(&be);
        let after = s.store_snapshot();
        assert_eq!(after.len(), 1);
        assert!(after.get("t1").is_none(), "tick 之后被删任务复活了");
        assert!(store.lock().get("t1").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // 试运行（v1.1 §6.5 / AC-42）

    fn conn_with(profiles: Vec<crate::connections::ConnectionProfile>) -> ConnectionStore {
        ConnectionStore {
            profiles,
            ..Default::default()
        }
    }

    fn exec_task(target: TaskTarget, command: &str, allow_mutating: bool) -> Task {
        Task::new(
            "试运行",
            Trigger::Interval { every_secs: 3600 },
            TaskAction::ExecMulti {
                target,
                command: command.into(),
                timeout_secs: 15,
                allow_mutating,
            },
            0,
        )
    }

    /// 试运行**绝不**建立连接、执行命令、下载、通知 —— 它是纯计算。
    #[test]
    fn dry_run_is_pure_computation() {
        let mut p = crate::connections::ConnectionProfile::new("prod-01", "10.0.0.1", "root");
        p.password = Some("pw".into());
        let conn = conn_with(vec![p.clone()]);
        let task = exec_task(
            TaskTarget {
                profiles: vec![p.id.clone()],
                ..Default::default()
            },
            "df -h",
            false,
        );
        let r = dry_run(&task, &conn);
        assert_eq!(r.targets, vec![p.id.clone()]);
        assert_eq!(r.target_names, vec!["prod-01".to_string()]);
        assert_eq!(r.risk.as_deref(), Some("safe"));
        assert!(!r.blocked);
        assert_eq!(r.concurrency, crate::exec::DEFAULT_CONCURRENCY);
        assert_eq!(r.timeout_secs, 15);
        assert!(!r.one_line().is_empty());
        // 试运行不产生任何运行记录、不改任务状态
        assert_eq!(task.last_status, RunStatus::Never);
    }

    #[test]
    fn dry_run_reports_blocked_and_confirm_commands() {
        let mut p = crate::connections::ConnectionProfile::new("prod-01", "10.0.0.1", "root");
        p.password = Some("pw".into());
        let conn = conn_with(vec![p.clone()]);
        let target = TaskTarget {
            profiles: vec![p.id.clone()],
            ..Default::default()
        };

        // Blocked：会被拦下，且原因说清
        let r = dry_run(&exec_task(target.clone(), "rm -rf /", true), &conn);
        assert!(r.blocked, "破坏类命令试运行就该标成会被拦下");
        assert_eq!(r.risk.as_deref(), Some("blocked"));
        assert!(r.warnings.iter().any(|w| w.contains("拦截")));

        // Confirm 且未开启：会跳过，但不是「被拦下」
        let r = dry_run(
            &exec_task(target.clone(), "systemctl restart nginx", false),
            &conn,
        );
        assert_eq!(r.risk.as_deref(), Some("confirm"));
        assert!(!r.blocked);
        assert!(
            r.warnings.iter().any(|w| w.contains("状态")),
            "要提醒它会被跳过：{:?}",
            r.warnings
        );

        // Confirm 且已开启：只提示风险等级，不报警告
        let r = dry_run(&exec_task(target, "systemctl restart nginx", true), &conn);
        assert_eq!(r.risk.as_deref(), Some("confirm"));
        assert!(r.warnings.iter().all(|w| !w.contains("状态")));
    }

    #[test]
    fn dry_run_flags_empty_targets_and_missing_credentials() {
        // 空目标
        let conn = conn_with(Vec::new());
        let task = exec_task(
            TaskTarget {
                profiles: Vec::new(),
                ..Default::default()
            },
            "df -h",
            false,
        );
        let r = dry_run(&task, &conn);
        assert!(r.blocked);
        assert!(r.targets.is_empty());
        assert!(!r.warnings.is_empty());

        // 有目标但没口令：会跑，但必须**提前**告诉用户会被跳过
        let p = crate::connections::ConnectionProfile::new("prod-01", "10.0.0.1", "root");
        let conn = conn_with(vec![p.clone()]);
        let task = exec_task(
            TaskTarget {
                profiles: vec![p.id.clone()],
                ..Default::default()
            },
            "df -h",
            false,
        );
        let r = dry_run(&task, &conn);
        assert!(!r.blocked, "有口令问题不等于整个任务被拦");
        assert_eq!(r.targets.len(), 1);
        assert!(
            r.warnings.iter().any(|w| w.contains("口令")),
            "必须提前说明会被跳过：{:?}",
            r.warnings
        );
    }

    /// `exclude` 是**最后**生效的：先并集再扣除（AC-37）。
    #[test]
    fn dry_run_applies_exclude_last() {
        let mut a = crate::connections::ConnectionProfile::new("a", "10.0.0.1", "root");
        a.password = Some("pw".into());
        a.tags = vec!["prod".into()];
        let mut b = crate::connections::ConnectionProfile::new("b", "10.0.0.2", "root");
        b.password = Some("pw".into());
        b.tags = vec!["prod".into()];
        let conn = conn_with(vec![a.clone(), b.clone()]);

        // 标签并集 = {a,b}，扣除 b -> 只剩 a
        let task = exec_task(
            TaskTarget {
                tags: vec!["prod".into()],
                exclude: vec![b.id.clone()],
                ..Default::default()
            },
            "df -h",
            false,
        );
        let r = dry_run(&task, &conn);
        assert_eq!(r.targets, vec![a.id.clone()], "exclude 必须最后生效");
        assert_eq!(r.target_names, vec!["a".to_string()]);
    }

    /// 试运行与真实执行必须**同一份**目标解析：否则预览会骗人。
    #[test]
    fn dry_run_and_execution_resolve_the_same_targets() {
        let mut p = crate::connections::ConnectionProfile::new("prod-01", "10.0.0.1", "root");
        p.password = Some("pw".into());
        let conn = std::sync::Arc::new(conn_with(vec![p.clone()]));
        let target = TaskTarget {
            profiles: vec![p.id.clone()],
            ..Default::default()
        };
        let task = exec_task(target.clone(), "df -h", false);
        let preview = dry_run(&task, &conn);

        let store = crate::connections::ConnectionStore {
            profiles: vec![p.clone()],
            ..Default::default()
        };
        let selector = crate::agent::TargetSelector::Profiles(target.profiles.clone());
        let executed = crate::agent::resolve_targets(&store, &selector).unwrap();
        assert_eq!(preview.targets, executed);
    }
}
