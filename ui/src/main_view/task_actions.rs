//! 定时任务域：调度器启停、任务增删改、立即执行、运行记录读取。
//!
//! 与快捷命令域同一分工：**本模块写状态，`tasks_panel` 只读状态渲染**。
//! 面板里的每一个可点元素都只调这里的一个方法，不做自己的状态推导 ——
//! 两处各推导一遍的代价是「界面显示的与落盘的不一致」，而这类不一致在
//! 「任务到底跑没跑」这种问题上极难自查。

use gpui::{Context, Window};
use gpui_component::WindowExt as _;

use rustshell_core::alerts::{LogChannel, Notifier, SmtpChannel};
use rustshell_core::connections::unix_now_secs;
use rustshell_core::runner::{RealBackend, Scheduler, TaskBackend};
use rustshell_core::tasks::{
    RunRecord, RunStatus, Task, TaskAction, TaskStore, TaskTarget, Trigger,
};

use rustshell_platform::paths;

use super::{DialogMode, MainView};

/// 往 `InputState` 里写入初值（空串也要写：清掉上一次打开对话框时的残留）。
///
/// 单独抽出来是因为「写空串」与「不写」在 `InputState` 上是两种行为：
/// 同一个输入框被多个任务复用，不写空串就会把上一个任务的值带过来。
fn set_input(
    input: &gpui::Entity<gpui_component::input::InputState>,
    value: String,
    window: &mut Window,
    cx: &mut Context<MainView>,
) {
    // InputState::set_value(value, window, cx)：三参数形态（见 main_view.rs 既有用法）
    input.update(cx, |s, cx| s.set_value(value.clone(), window, cx));
    let _ = &value;
}

/// 同 [`set_input`]，但用于多行 `TextareaState`。
///
/// 与 `InputState` 分开写而不是泛型化：两者的 `set_value` 是**各自**的方法，
/// 泛型化要靠 trait 或宏，而这里只有两处调用 —— 抄一遍更省事也更好读。
fn set_textarea(
    input: &gpui::Entity<gpui_component::input::TextareaState>,
    value: String,
    window: &mut Window,
    cx: &mut Context<MainView>,
) {
    input.update(cx, |s, cx| s.set_value(value, window, cx));
}

/// **名称留空时**的兜底：从「AI 要求」首行取一个名字。
///
/// 取第一段非空行并按名称上限截断（截断处留 `…`）：名字只是列表里的一行
/// 字，而 `Task::validate` 对空名/超长名是**报错**，在这里兜住就不会出现
/// 「用户只填了要求，点保存却被驳回」。
fn task_name_from_prompt(prompt: &str) -> String {
    let Some(line) = prompt.lines().map(str::trim).find(|l| !l.is_empty()) else {
        return rustshell_core::i18n::t("AI 巡检").to_string();
    };
    let max = rustshell_core::tasks::MAX_TASK_NAME_CHARS;
    if line.chars().count() <= max {
        return line.to_string();
    }
    let mut s: String = line.chars().take(max.saturating_sub(1)).collect();
    s.push('…');
    s
}

impl MainView {
    /// 取（必要时创建）调度器。
    ///
    /// 懒建而不是在 `new` 里无条件起线程：只想用终端的用户不该白白多一个
    /// 每秒醒一次的调度线程。首次打开任务面板或首次手动执行时才建。
    pub(super) fn scheduler(&mut self) -> std::sync::Arc<Scheduler> {
        if let Some(s) = &self.io.scheduler {
            return s.clone();
        }
        let store = std::sync::Arc::new(parking_lot::Mutex::new(self.io.task_store.clone()));
        let alerts = std::sync::Arc::new(parking_lot::Mutex::new(self.io.alert_store.clone()));
        // 通知通道：日志永远可用；邮件通道按配置就绪（发送留待后续实现，
        // 未实现时派发侧记为「跳过」而不是失败）。
        let notifier = std::sync::Arc::new(
            Notifier::new(vec![
                std::sync::Arc::new(LogChannel::new(paths::alerts_log_path())),
                std::sync::Arc::new(SmtpChannel::new(self.io.smtp_cfg.clone())),
            ])
            // 通知扇出也要留痕（AC-29）
            .with_audit(paths::agent_audit_path()),
        );
        let sched = std::sync::Arc::new(Scheduler::new(
            store,
            alerts,
            notifier,
            paths::tasks_path(),
            paths::task_runs_dir(),
            paths::alert_rules_path(),
        ));
        self.io.scheduler = Some(sched.clone());
        // 会话基线：只给**这次启动之后**跑完的运行自动建 AI 会话。
        // 历史记录（含重启前跑的那批）在用户点「会话」时按需建 —— 否则一启动
        // 就往聊天记录里灌几十条陈年会话，把用户真正在聊的那些挤没了。
        self.ai_ui.ai_runs_baseline_at = unix_now_secs();
        // 是否执行只看每个任务自己的开关（没有全局总开关）；新建任务默认停用。
        log::info!("定时任务调度器已启动");
        sched
    }

    /// 构造任务执行后端。
    ///
    /// 连接库用**当前内存态**（口令已恢复）：定时任务无人值守，没有口令的连接
    /// 会被后端跳过并说明原因 —— 这是刻意的，后台不允许弹口令框。
    /// 对话框里的「发一次 AI 测试」也要用它（`pub(super)` + 同属 `main_view`）。
    pub(super) fn task_backend(&self, sched: &Scheduler) -> RealBackend {
        let conn = std::sync::Arc::new(self.conn_store.clone());
        // 告警 store 与调度器共享同一份（否则告警判定读到的规则与面板显示的不是一套）
        let alerts = std::sync::Arc::new(parking_lot::Mutex::new(self.io.alert_store.clone()));
        let ai = if self.ai_ui.ai_cfg.is_configured() {
            let key = self
                .ai_ui
                .ai_cfg
                .api_key_cipher
                .as_deref()
                .and_then(rustshell_platform::secret::unprotect)
                .unwrap_or_default();
            Some((self.ai_ui.ai_cfg.clone(), key))
        } else {
            None
        };
        let _ = sched;
        RealBackend::new(
            conn,
            alerts,
            ai,
            paths::default_download_dir(),
            // 审计落点：exec / 下载 / 通知扇出都写这一份（AC-29）
            paths::agent_audit_path(),
            // 全局代理（F-10）：无人值守的定时任务同样要能借代理出去，
            // 否则「会话能连、任务连不上」会变成一类说不清的怪故障。
            self.runtime_global_proxy(),
        )
    }

    /// 调度 tick：由既有 `poll` 每 300ms 调一次，内部自己按秒节流。
    ///
    /// 返回是否产生了需要重绘的变化。
    ///
    /// **重绘归属必须点名 `ChildView::Sidebar`**（2026-09-28 用户反馈「任务面板
    /// 搜索框打字失去焦点」的根因）：
    ///
    /// 这里此前是裸 `cx.notify()`。而 `ViewEpoch::wants_redraw` 的判定是
    /// 「归属序号没前进 ⇒ 这次 notify 没有归属 ⇒ 兜底重绘」—— 于是任务面板
    /// （长在 `SidebarPanelView` 里）**每秒都会被整块重建一次**。
    ///
    /// 重建本身不致命，致命的是它跟 `gpui-component` 的
    /// `sync_focused_input_registry` 撞在一起：`Input::render` 每次渲染都会
    /// `Root::try_update(..) { .. cx.notify() }`（把当前聚焦输入框登记进 Root）。
    /// 于是「每秒重建侧栏」→「输入框重渲染」→「Root 再 notify」形成滚雪球，
    /// 焦点在重排中被重新登记，表现为**打字打一半焦点就没了**。
    ///
    /// 点名归属之后，侧栏的版本号只在**任务数据真的变了**时才前进
    ///（`merge_scoped` 才会推进 `counters[Sidebar]`），搜索框不再被每秒重建。
    /// 注意返回的 `changed` 仍要让 `poll` 转发给 `notify_view`，所以这里
    /// **不自己发 notify**，只把「该重绘侧栏」这个事实交回去 ——
    /// 与 `notify_view` 的约定一致（见那里的文档：不要在同一个 update 里
    /// 既点归属又补裸 notify）。
    pub fn tasks_tick(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(sched) = self.io.scheduler.clone() else {
            return false;
        };
        let now = unix_now_secs();
        // 每秒最多 tick 一次（`poll` 是 300ms 一轮）
        if now == self.io.tasks_last_tick {
            return false;
        }
        self.io.tasks_last_tick = now;
        let backend = self.task_backend(&sched);
        let started = sched.tick(&backend);
        // 任务定义可能被后端/调度器改过（next_run_at / 连停用），同步回内存态，
        // 否则面板显示的下次触发时间会一直停在打开面板那一刻。
        let store = sched_store_snapshot(&sched);
        let changed = store != self.io.task_store;
        if changed {
            self.io.task_store = store;
            // 点名侧栏：任务列表长在侧栏里，只有它读 `task_store`。
            // 用 `notify_view` 而不是裸 `cx.notify()`，正是为了不让终端 /
            // AI 对话 / 搜索浮层跟着每秒重排。
            self.notify_view(super::ChildView::Sidebar, cx);
        }
        // 刚跑完的 AI 巡检要在聊天记录里各留一条会话（用户要求「每次执行都会
        // 创建一条 AI 会话记录」）。放在这里而不是完成回调里：运行的收口在
        // 后端线程，UI 只有这个每秒一次的点能观察到它。
        let sessions = self.task_materialize_sessions();
        started > 0 || changed || sessions
    }

    /// 给**刚跑完**的运行补上 AI 会话（每个 tick 调一次，开销极小）。
    ///
    /// 只看每个任务的**最近一次**运行：任务不会与自己并发（调度器的运行标记
    /// 挡着），所以「新出现的记录」必然是最近这一条。整条记录（含轨迹明细）
    /// 只在真的要建会话时才拉取，逐帧克隆一份 8 KiB×N 的明细是不能接受的。
    fn task_materialize_sessions(&mut self) -> bool {
        let Some(sched) = self.io.scheduler.clone() else {
            return false;
        };
        let baseline = self.ai_ui.ai_runs_baseline_at;
        let ids: Vec<String> = self
            .io
            .task_store
            .tasks
            .iter()
            .map(|t| t.id.clone())
            .collect();
        let mut created = false;
        for id in ids {
            let Some((run_id, finished_at, has_session)) = sched.latest_run_meta(&id) else {
                continue;
            };
            if !has_session || finished_at < baseline {
                continue;
            }
            if self.ai_ui.ai_run_chats.contains_key(&run_id) {
                continue;
            }
            if let Some(rec) = self.task_runs(&id).into_iter().find(|r| r.run_id == run_id) {
                if self.ai_attach_run_session(&rec).is_some() {
                    created = true;
                }
            }
        }
        created
    }

    /// 打开某次运行留下的 AI 会话（运行记录行上的「会话」）。
    ///
    /// 老记录（没有会话痕迹）与生成中的面板都走各自的明确分支：
    /// 只把能打开的情况打开，其余给一句人能看懂的话 —— 静默失败会让用户
    /// 以为是「点了没反应」。
    pub fn task_open_run_session(&mut self, task_id: &str, run_id: &str, cx: &mut Context<Self>) {
        // 生成中不切会话：在途答复是按「当前会话的最后一条消息」收口的
        // （见 `ai_switch_chat` 的注释）—— 切走之后增量会写进另一条会话里。
        if self.ai_ui.ai_busy {
            self.show_task_toast(
                rustshell_core::i18n::ts(
                    "AI 正在生成回复，请稍后再打开会话".into(),
                    "AI is replying; open the session after it finishes".into(),
                ),
                cx,
            );
            cx.notify();
            return;
        }
        let Some(rec) = self
            .task_runs(task_id)
            .into_iter()
            .find(|r| r.run_id == run_id)
        else {
            self.show_task_toast(
                rustshell_core::i18n::ts(
                    "这条运行记录已经不在（可能被裁剪）".into(),
                    "this run record is gone".into(),
                ),
                cx,
            );
            cx.notify();
            return;
        };
        let Some(chat_id) = self.ai_attach_run_session(&rec) else {
            self.show_task_toast(
                rustshell_core::i18n::ts(
                    "这条运行没有留下会话记录".into(),
                    "this run has no session".into(),
                ),
                cx,
            );
            cx.notify();
            return;
        };
        // 「带用户过去看」：进 AI 面板、展开终端右侧那一列、切到这条会话。
        // 开合状态由 `ai_switch_chat` 里的 `ai_set_chat_open` 一并落盘
        // （2026-10-30 用户要求「默认记录窗口开闭」），这里不再单独赋值。
        self.panel_ui.panel = super::PanelKind::Ai;
        self.panel_ui.sidebar_collapsed = false;
        self.ai_switch_chat(chat_id, cx);
        cx.notify();
    }

    /// 新增任务（模板入口）：落库 + 立即打开编辑对话框。
    ///
    /// 落库后立刻可编辑，用户在对话框里点取消时**删除**这条空壳
    /// （见 `task_draft_cancel`）—— 比「表单里的值丢了」更符合直觉：
    /// 用户看到的是「我取消了新建」，而不是「我建了一个空任务」。
    pub fn task_add_template(
        &mut self,
        action: TaskAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let now = unix_now_secs();
        let name = match &action {
            TaskAction::MultiConnect { .. } => rustshell_core::i18n::t("新建批量连接"),
            TaskAction::ExecMulti { .. } => rustshell_core::i18n::t("新建批量命令"),
            TaskAction::Download { .. } => rustshell_core::i18n::t("新建批量下载"),
            TaskAction::AiInspect { .. } => rustshell_core::i18n::t("新建 AI 巡检"),
        };
        // 模板的初始触发：**cron**（触发方式已收敛成表达式一种）。
        // 给一个可读的默认值（每天 09:00）而不是空串 —— 空串会让第一次点保存
        // 立刻撞校验错误，而用户还没搞清「表达式」该怎么写。
        let task = Task::new(
            name,
            Trigger::cron(rustshell_core::tasks::DEFAULT_CRON),
            action,
            now,
        );
        // 新建即**停用**：模板只是给个起点，用户还没选目标、没填命令。
        // 与 AI 草案同一口径 —— 未经用户确认的东西不该到点开跑。
        let mut task = task;
        task.set_enabled(false, now);
        let id = task.id.clone();
        self.task_store_update(|s| {
            s.add(task);
        });
        self.open_task_editor_inner(&id, true, window, cx);
    }

    /// 唯一的提示出口，只用于**失败与异常**（用户要求：已保存 / 已开始执行 /
    /// 已启用这类成功确认不再弹）。
    ///
    /// 全部调用点都走这里，而不是各自 `self.io.task_toast = Some(..)`：后者不会推进
    /// `task_toast_seq`，提示就永远留在面板上；而「新的提示顶掉旧的」也要求每次
    /// 自增代次，否则上一批定时器会把新提示提前清掉。
    pub(super) fn show_task_toast(&mut self, msg: String, cx: &mut Context<Self>) {
        self.io.task_toast = Some(msg);
        self.io.task_toast_seq = self.io.task_toast_seq.wrapping_add(1);
        self.schedule_task_toast_clear(self.io.task_toast_seq, cx);
    }

    /// 所有任务变更的**唯一落点**。
    ///
    /// 改的是调度器持有的那份 store（`Scheduler::update_store` 顺手落盘），
    /// 改完把最新快照取回内存态。为什么不直接改 `self.io.task_store`：
    /// `tasks_tick` 每秒把调度器的快照同步回来，只改 UI 副本的变更（删除、
    /// 停用…）会在 1 秒内被抹掉 ——「右键删除后任务又回来了」正是这个原因。
    pub(super) fn task_store_update(&mut self, f: impl FnOnce(&mut TaskStore)) -> TaskStore {
        let sched = self.scheduler();
        let snapshot = sched.update_store(f);
        self.io.task_store = snapshot.clone();
        snapshot
    }

    /// 打开任务编辑对话框（面板/右键菜单入口）。
    pub(super) fn open_task_editor(
        &mut self,
        task_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_task_editor_inner(task_id, false, window, cx);
    }

    /// 打开编辑器。`is_new` 为真时记下「这是新建」，取消即删除空壳。
    fn open_task_editor_inner(
        &mut self,
        task_id: &str,
        is_new: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.io.dlg_task_is_new = is_new;
        let Some(task) = self.io.task_store.get(task_id).cloned() else {
            self.show_task_toast(
                rustshell_core::i18n::ts("任务已不存在".into(), "task no longer exists".into()),
                cx,
            );
            cx.notify();
            return;
        };
        // 触发：只有 cron 一种。**老任务**可能是 `Interval`（存下来的），
        // 原样搬进 cron 输入框是做不到的（间隔不是 cron），故换算成等价的
        // cron，保存后这条任务就变成「按计划」的了 —— 这正是收敛方向。
        let (cron, fmt_note) = match &task.trigger {
            Trigger::Cron { expr } => (expr.clone(), None),
            Trigger::Interval { every_secs } => (
                rustshell_core::tasks::cron::interval_to_cron(*every_secs),
                Some(rustshell_core::i18n::ts(
                    "这条任务原来是「固定间隔」，已换算成等价的 cron 表达式".into(),
                    "this task used a fixed interval; converted to the equivalent cron expression"
                        .into(),
                )),
            ),
        };
        let target = task.action.target();
        self.io.task_target_kind = if !target.profiles.is_empty() { 1 } else { 0 };
        // 目标从**选择状态**读回：分组存 id、连接存 id（名字只在界面显示）
        self.io.dlg_task_group_sel = target.groups.first().cloned();
        self.io.dlg_task_profiles_sel = target.profiles.clone();
        let prompt = match &task.action {
            TaskAction::AiInspect { prompt, .. } => prompt.clone(),
            // 非 AI 巡检的既有任务：表单里没有它那种动作的字段，要求框留空
            // （保存时按下面的分支原样保留其动作字段）。
            _ => String::new(),
        };
        let np = task.notify.unwrap_or_default();
        self.io.dlg_task_notify = (np.on_anomaly, np.on_error, np.on_ok);

        set_input(&self.io.dlg_task_name, task.name.clone(), window, cx);
        set_input(&self.io.dlg_task_cron, cron, window, cx);
        // 高度由文本域自己按内容生长（`auto_grow(2, 10)`），这里不再算高度
        set_textarea(&self.io.dlg_task_prompt, prompt.clone(), window, cx);

        self.io.task_draft = Some(task);
        // 老任务被换算过触发方式时，把原因写在对话框里 —— 否则用户会以为
        // 「我只改了目标，怎么触发方式也变了」。
        self.io.task_dlg_error = fmt_note;
        let mode = DialogMode::TaskEdit(task_id.to_string());
        self.dialog = Some(mode.clone());
        self.present_dialog(mode, window, cx);
        cx.notify();
    }

    /// 从表单读回草稿（渲染前与保存前各调一次；校验只在保存时做）。
    pub(super) fn task_collect_draft(&mut self, cx: &Context<Self>) {
        let Some(mut draft) = self.io.task_draft.clone() else {
            return;
        };
        let prompt = self.io.dlg_task_prompt.read(cx).value().trim().to_string();
        // 名称以表单为准；**留空**时才回退到「AI 要求」首行 —— 名称字段是
        // 用户要求的，但它可以空着：空名的任务在列表里就是一行空白，
        // 而那正是 `Task::validate` 会驳回的。回退只在名字为空时发生，
        // 用户自己起的名字（含老任务的名字）一律不动。
        let typed_name = self.io.dlg_task_name.read(cx).value().trim().to_string();
        draft.name = if typed_name.is_empty() {
            task_name_from_prompt(&prompt)
        } else {
            typed_name
        };

        let cron_expr = self.io.dlg_task_cron.read(cx).value().trim().to_string();
        // 触发**只有 cron 一种**（按运行反馈收敛）。表达式为空时留空串交给
        // `Trigger::validate` 报错 —— 在这里悄悄塞一个默认值会让用户以为
        // 自己填的表达式生效了。时区不存：跟随本机系统时区（桌面软件自动获取）。
        draft.trigger = Trigger::Cron { expr: cron_expr };

        // 目标来自**选择状态**（不再解析文本输入）：分组是单选的分组 id，
        // 连接是多选的 id 列表，排除是「已选连接」的子集。
        // 存 id 不存名字 —— 改名不丢目标。
        let target = TaskTarget {
            groups: if self.io.task_target_kind == 0 {
                self.io.dlg_task_group_sel.clone().into_iter().collect()
            } else {
                Vec::new()
            },
            profiles: if self.io.task_target_kind == 1 {
                self.io.dlg_task_profiles_sel.clone()
            } else {
                Vec::new()
            },
            // 标签分类已撤下；既有任务里的 tags 会被**清空**（用户已在界面上
            // 看不到它，留着一条看不见的目标条件比清掉更危险）。
            tags: Vec::new(),
            // 排除功能整体撤下：一律清空。留着老任务里的排除项就是「用户
            // 看不见、任务却在少跑几台」——比清掉危险得多。
            exclude: Vec::new(),
        };

        // 保留动作**类型**，只替换字段（换类型会让用户填的其他字段凭空消失）
        draft.action = match draft.action.clone() {
            TaskAction::MultiConnect {
                open_tabs,
                keep_alive_secs,
                ..
            } => TaskAction::MultiConnect {
                target,
                open_tabs,
                keep_alive_secs,
            },
            // 「命令」已从表单撤下（表单只编辑 AI 巡检）：既有批量命令任务
            // **原样保留**它存着的命令，改目标 / 时间 / 通知不该顺手把命令抹了。
            TaskAction::ExecMulti {
                command,
                timeout_secs,
                allow_mutating,
                ..
            } => TaskAction::ExecMulti {
                target,
                command,
                timeout_secs,
                allow_mutating,
            },
            // 下载类型已从界面撤下：既有任务**原样保留**（改个名字不该把动作
            // 也改掉），但用户不能新建。`remote_path` 因此不在表单里，直接沿用；
            // 目标仍按上面的选择更新（改目标对下载同样有意义）。
            TaskAction::Download {
                remote_path,
                local_dir,
                ..
            } => TaskAction::Download {
                target,
                remote_path,
                local_dir,
            },
            // 「巡检项」已撤下：检查什么由模型按「AI 要求」自己决定。
            // 既有任务里存着的 checks/log_path **清空**，否则用户改完 AI 要求
            // 保存后，老的那套巡检项还在背地里限制模型能查什么。
            TaskAction::AiInspect {
                thresholds,
                log_level,
                ..
            } => TaskAction::AiInspect {
                target,
                prompt,
                checks: Vec::new(),
                thresholds,
                log_path: None,
                log_level,
            },
        };
        let (on_anomaly, on_error, on_ok) = self.io.dlg_task_notify;
        draft.notify = Some(rustshell_core::tasks::NotifyPolicy {
            on_ok,
            on_anomaly,
            on_error,
            ..Default::default()
        });
        self.io.task_draft = Some(draft);
    }

    /// 保存草稿：校验 → 写回 store → 落盘 → 关窗。校验失败**保持对话框打开**。
    pub(super) fn task_draft_save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.task_collect_draft(cx);
        let Some(draft) = self.io.task_draft.clone() else {
            return;
        };
        if let Err(e) = draft.validate() {
            self.io.task_dlg_error = Some(e);
            cx.notify();
            return;
        }
        // 目标解析必须非空：存一个到点必然跳过的任务没有意义
        let preview = rustshell_core::runner::dry_run(&draft, &self.conn_store);
        if preview.targets.is_empty() {
            self.io.task_dlg_error = Some(rustshell_core::i18n::ts(
                "没有选到任何连接，请在上面的列表里勾选分组或连接".into(),
                "no connection selected; pick a group or connection above".into(),
            ));
            cx.notify();
            return;
        }
        let id = draft.id.clone();
        let now = unix_now_secs();
        self.task_store_update(|s| {
            if s.get(&id).is_some() {
                s.replace(&id, draft, now);
            } else {
                s.add(draft);
            }
        });
        self.io.dlg_task_is_new = false;
        self.io.task_draft = None;
        self.io.task_dlg_error = None;
        self.dialog = None;
        window.close_dialog(cx);
        cx.notify();
    }

    /// 取消编辑：丢掉草稿。
    ///
    /// **新建流程点取消要顺手删掉空壳**：`task_add_template` 为了「落库后立刻可编辑」
    /// 先把任务写进了 store，取消时若不删，用户会看到列表里多出一条自己没建完的空任务。
    /// 只删**本轮新建**的那条（`dlg_task_is_new`）—— 编辑既有任务时点取消
    /// 不能碰任何已存下来的东西。
    pub(super) fn task_draft_cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(draft) = self.io.task_draft.take() {
            if self.io.dlg_task_is_new {
                let id = draft.id.clone();
                self.task_store_update(|s| {
                    s.remove(&id);
                });
            }
        }
        self.io.dlg_task_is_new = false;
        self.io.task_dlg_error = None;
        self.dialog = None;
        window.close_dialog(cx);
        cx.notify();
    }

    fn schedule_task_toast_clear(&self, seq: u64, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_secs(4))
                .await;
            let _ = this.update(cx, |m, cx| {
                // 代次已变说明有更新的提示，旧定时器不再清空
                if m.io.task_toast_seq == seq {
                    m.io.task_toast = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// 立即执行一个任务（右键菜单 / 行内按钮）。
    pub fn task_run_now(&mut self, task_id: &str, cx: &mut Context<Self>) {
        if self.io.task_store.get(task_id).is_none() {
            self.show_task_toast(
                rustshell_core::i18n::ts(
                    "任务已不存在，列表已刷新".into(),
                    "task no longer exists".into(),
                ),
                cx,
            );
            cx.notify();
            return;
        }
        let sched = self.scheduler();
        let backend = self.task_backend(&sched);
        // 成功不提示（用户要求）：跑没跑起来，行下方的「正在运行」和运行记录
        // 都会说话；只有失败才值得占一行字。
        if let Err(e) = sched.run_now(task_id, &backend as &dyn TaskBackend) {
            self.show_task_toast(e, cx);
        }
        cx.notify();
    }

    /// 停止一个正在运行的任务。
    pub fn task_stop(&mut self, task_id: &str, cx: &mut Context<Self>) {
        let Some(sched) = self.io.scheduler.clone() else {
            return;
        };
        let ok = sched.stop(task_id);
        if !ok {
            self.show_task_toast(
                rustshell_core::i18n::ts("该任务未在运行".into(), "task is not running".into()),
                cx,
            );
        }
        cx.notify();
    }

    /// 启用 / 停用任务。
    pub fn task_toggle_enabled(&mut self, task_id: &str, cx: &mut Context<Self>) {
        let now = unix_now_secs();
        self.task_store_update(|s| {
            if let Some(t) = s.get_mut(task_id) {
                let n = !t.enabled;
                t.set_enabled(n, now);
            }
        });
        cx.notify();
    }

    /// 把任务设成指定启停态（面板行内的开关走这里）。
    ///
    /// 与 [`Self::task_toggle_enabled`]（右键菜单的「启用」）分开：开关自带
    /// 目标态，若也用「取反」实现，在状态已经不同步时会把用户往反方向拨。
    pub fn task_enable(&mut self, task_id: &str, on: bool, cx: &mut Context<Self>) {
        let now = unix_now_secs();
        self.task_store_update(|s| {
            if let Some(t) = s.get_mut(task_id) {
                if t.enabled != on {
                    t.set_enabled(on, now);
                }
            }
        });
        cx.notify();
    }

    /// 删除任务（面板侧已二次确认）。
    pub fn task_delete(&mut self, task_id: &str, cx: &mut Context<Self>) {
        if self.io.task_store.get(task_id).is_none() {
            return;
        }
        self.task_store_update(|s| {
            s.remove(task_id);
        });
        if self.io.task_expanded.as_deref() == Some(task_id) {
            self.io.task_expanded = None;
        }
        cx.notify();
    }

    /// 复制一份任务。
    pub fn task_duplicate(&mut self, task_id: &str, cx: &mut Context<Self>) {
        let now = unix_now_secs();
        let mut ok = false;
        self.task_store_update(|s| {
            ok = s.duplicate(task_id, now).is_some();
        });
        if !ok {
            self.show_task_toast(
                rustshell_core::i18n::ts("任务已不存在".into(), "task no longer exists".into()),
                cx,
            );
        }
        cx.notify();
    }

    /// 展开 / 收起某个任务的运行记录（同一时刻只展开一条）。
    pub fn task_toggle_expand(&mut self, task_id: &str, cx: &mut Context<Self>) {
        self.io.task_expanded = if self.io.task_expanded.as_deref() == Some(task_id) {
            None
        } else {
            Some(task_id.to_string())
        };
        cx.notify();
    }

    /// 读取某个任务的运行记录（展开时才读，避免每帧扫盘）。
    pub fn task_runs(&self, task_id: &str) -> Vec<RunRecord> {
        match &self.io.scheduler {
            Some(s) => s.recent_runs(task_id),
            // 调度器还没起（用户只是打开面板看了眼）——直接读盘，
            // 否则「重启后打开面板看不到历史」会让人以为记录丢了。
            None => rustshell_core::tasks::read_records(&paths::task_runs_dir(), task_id, 20),
        }
    }

    /// 某个任务是否正在运行。
    pub fn task_running(&self, task_id: &str) -> bool {
        self.io
            .scheduler
            .as_ref()
            .map(|s| s.is_running(task_id))
            .unwrap_or(false)
    }

    /// 任务列表的展示顺序（按下次触发升序，停用/未计划的排最后）。
    pub fn task_ordered_ids(&self) -> Vec<String> {
        self.io.task_store.ordered_ids()
    }

    /// 按名称过滤后的任务 id（面板搜索框用）。关键字为空 = 不过滤。
    ///
    /// 判定在 core 的纯函数里（[`rustshell_core::tasks::TaskStore::ids_matching`]）：
    /// 大小写、空白、包含匹配这些语义要能被单测直接钉住，不靠界面。
    pub fn task_ids_matching(&self, query: &str) -> Vec<String> {
        self.io.task_store.ids_matching(query)
    }

    /// 打开任务面板时的入口（竖栏图标与命令面板共用）。
    pub fn tasks_open(&mut self, cx: &mut Context<Self>) {
        // 起调度器：用户既然打开了任务面板，就是要它按计划跑
        let _ = self.scheduler();
        // 顺带把磁盘上的运行记录回填一次（懒建的调度器已经在构造时做过，
        // 但面板打开前可能已经有记录产生）
        for t in self.io.task_store.tasks.clone() {
            if self.task_runs(&t.id).is_empty() {
                log::debug!("任务 {} 暂无运行记录", t.id);
            }
        }
        cx.notify();
    }

    /// 保存任务定义（编辑对话框用；对话框落地前供内部/测试调用）。
    pub fn task_save(&mut self, task: Task, cx: &mut Context<Self>) -> Result<(), String> {
        task.validate()?;
        let now = unix_now_secs();
        let id = task.id.clone();
        self.task_store_update(|s| {
            if s.get(&id).is_some() {
                s.replace(&id, task, now);
            } else {
                s.add(task);
            }
        });
        cx.notify();
        Ok(())
    }
}

/// 从调度器取一份任务仓库快照（面板渲染的数据源）。
fn sched_store_snapshot(sched: &Scheduler) -> TaskStore {
    // `Scheduler` 内部持锁，这里取只读快照：面板渲染不能长时间占着调度锁，
    // 否则一个卡住的渲染会让所有任务都推迟触发。
    sched.store_snapshot()
}

/// 状态对应的语义级别（决定颜色与图标）。
pub fn status_kind(status: RunStatus) -> crate::notice::NoticeKind {
    use crate::notice::NoticeKind;
    match status {
        RunStatus::Ok => NoticeKind::Info,
        RunStatus::Partial => NoticeKind::Warning,
        RunStatus::Failed => NoticeKind::Error,
        RunStatus::Never | RunStatus::Skipped | RunStatus::Cancelled => NoticeKind::Info,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::prod;

    /// 任务名从「AI 要求」首行取（表单里已经没有名称字段了）。
    #[test]
    fn task_name_comes_from_the_prompt_first_line() {
        assert_eq!(task_name_from_prompt("看看磁盘\n再看看内存"), "看看磁盘");
        assert_eq!(
            task_name_from_prompt("\n\n  看看负载  \n"),
            "看看负载",
            "首行是空行时要往下找第一行有字的"
        );
        assert_eq!(
            task_name_from_prompt("   "),
            rustshell_core::i18n::t("AI 巡检"),
            "要求为空时给一个通用名 —— 空名字过不了 `Task::validate`"
        );

        // 超长首行必须截断到**校验上限之内**：否则用户只在要求里写了句话，
        // 点保存却报「名称过长」，而名称字段根本不在界面上。
        let long = "巡检".repeat(80);
        let name = task_name_from_prompt(&long);
        assert_eq!(
            name.chars().count(),
            rustshell_core::tasks::MAX_TASK_NAME_CHARS,
            "截断后的长度要正好落在上限上：{name}"
        );
        assert!(name.ends_with('…'), "截断处要有 `…`：{name}");

        let t = Task::new(
            name,
            Trigger::Cron {
                expr: "0 9 * * *".into(),
            },
            TaskAction::AiInspect {
                target: TaskTarget {
                    profiles: vec!["p1".into()],
                    ..Default::default()
                },
                prompt: long,
                checks: Vec::new(),
                thresholds: None,
                log_path: None,
                log_level: Default::default(),
            },
            0,
        );
        assert!(
            t.validate().is_ok(),
            "自动取的名字必须能过校验：{:?}",
            t.validate().err()
        );
    }

    /// 「每次执行都留一条 AI 会话」的接线（UI 侧的三个落点）。
    ///
    /// 这条链路横跨后端线程（写记录）/ 每秒 tick（发现）/ 聊天记录（建会话），
    /// 断在哪一段都只是「聊天记录里少几条」—— 界面上看不出坏了。
    #[test]
    fn finished_runs_are_wired_into_the_ai_chat_history() {
        let c = prod(include_str!("task_actions.rs"));
        for needle in [
            "self.task_materialize_sessions()",           // tick 里的发现点
            "sched.latest_run_meta(",                     // 只读三个标量，不逐帧拉整条记录
            "self.ai_attach_run_session(&rec)",           // 建会话（幂等，见 ai_actions.rs）
            "self.ai_ui.ai_runs_baseline_at",             // 只给本次启动之后跑完的运行自动建
            "self.ai_ui.ai_run_chats.contains_key(",      // 同一次运行不重复建
            "self.ai_switch_chat(chat_id, cx)",           // 点「会话」→ 选中那条会话
            "self.panel_ui.panel = super::PanelKind::Ai", // 并把 AI 面板带出来
        ] {
            assert!(c.contains(needle), "运行 → 会话的接线缺了 `{needle}`");
        }
    }

    /// 表单撤下名称与命令后，草稿的读回必须换个来源：名字自动取、批量命令的
    /// 命令沿用它存着的那份（读一个已删掉的输入框会编译不过，真正危险的是
    /// 「悄悄读了个空串」—— 那会把既有任务的命令抹掉）。
    #[test]
    fn collect_draft_auto_names_and_keeps_stored_commands() {
        let c = prod(include_str!("task_actions.rs"));
        let at = c.find("fn task_collect_draft").expect("找不到草稿读回");
        let end = c.find("fn task_draft_save").expect("找不到保存");
        let collect = &c[at..end];
        // 名称以表单里的字段为准，**留空**时才回退到「AI 要求」首行
        assert!(
            collect.contains("self.io.dlg_task_name.read(cx)"),
            "草稿读回没有读「名称」输入框：用户填的名字会被丢掉"
        );
        assert!(
            collect.contains("task_name_from_prompt(&prompt)"),
            "草稿没有「名称留空 → 取 AI 要求首行」的兜底：空名会被校验驳回"
        );
        assert!(
            collect.contains("if typed_name.is_empty()"),
            "兜底必须**只在名字为空时**生效：否则每次改要求都会把用户起的名字换掉"
        );
        assert!(
            !collect.contains("dlg_task_command"),
            "草稿读回里还引用着已撤下的命令输入框"
        );
        assert!(
            collect.contains("TaskAction::ExecMulti {"),
            "草稿读回没有处理 ExecMulti：既有批量命令任务的命令会被抹掉"
        );
        assert!(
            collect.contains("command,") && collect.contains("timeout_secs,"),
            "ExecMulti 的臂必须沿用草稿里存着的 command / timeout_secs"
        );
    }
}
