//! AI 助手域：问答、命令卡片、配置同步与结果轮询。

use gpui::{Context, Window};

use rustshell_platform::paths;

use super::{chat_title_from, display_tab_title, AiChat, MainView, PanelKind};

impl MainView {
    /// 发送用户提问，注入当前终端最近输出上下文并发起后台请求
    pub fn ai_send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.ai_ui.ai_busy || !self.ai_ui.ai_cfg.is_configured() {
            return;
        }
        let q = self.ai_ui.ai_input.read(cx).value().trim().to_string();
        if q.is_empty() {
            return;
        }
        self.ai_ui
            .ai_input
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.ai_send_inner(&q, cx);
    }

    pub(super) fn ai_send_inner(&mut self, q: &str, cx: &mut Context<Self>) {
        use rustshell_core::ai as ai_mod;
        let tail = self
            .active_tab()
            .and_then(|t| t.terminal.as_ref().map(|term| term.read(cx).visible_text()));
        // 注入行数取 core 常量而不是配置字段：2026-09-18「取消 AI 上下文限制配置」
        // 之后它不是配置项了（见 `rustshell_core::ai::CONTEXT_LINES`）。
        let ctx = tail.map(|s| ai_mod::truncate_tail(&s, ai_mod::CONTEXT_LINES));
        let snap = self.ai_terminal_snapshot();
        let wrapped = ai_mod::with_context(q, snap.as_ref(), ctx.as_deref());
        // 记录实际注入的上下文行数（可追溯「AI 看到了什么」）
        let injected = ctx.as_deref().map(|c| c.lines().count()).unwrap_or(0);
        self.ai_send_wrapped(wrapped, injected, cx);
    }

    /// 「当前终端」画像：用户此刻在看的标签的会话事实 + 实时指标。
    ///
    /// 指标只在**已连接**时带上：断开后 `MetricsStore` 里还留着上一次样本，
    /// 照抄会让模型拿旧数字回答「现在」的问题。本地标签取本机采样 —— 本地
    /// 终端就在本机上，「这个终端的内存」问的正是本机。
    fn ai_terminal_snapshot(&self) -> Option<rustshell_core::ai::TerminalSnapshot> {
        let tab = self.active_tab()?;
        let connected = tab.session.state == rustshell_core::session::SessionState::Connected;
        let store = self.metrics_for_active();
        let overview = connected
            .then(|| store.overview_arc())
            .filter(|o| !o.is_empty());
        Some(rustshell_core::ai::TerminalSnapshot {
            title: display_tab_title(tab, &self.conn_store),
            kind: tab.session.kind,
            profile_id: tab.session.profile_id.clone(),
            target: tab
                .session
                .profile_id
                .as_deref()
                .and_then(|id| self.conn_store.profile(id))
                .map(|p| p.summary_line()),
            state: tab.session.state.clone(),
            metrics: connected.then(|| store.latest()).flatten(),
            overview,
        })
    }

    /// 以（已包装的）用户消息发起请求；ai_send_inner 与「重新生成」共用
    pub(super) fn ai_send_wrapped(
        &mut self,
        wrapped: String,
        injected_lines: usize,
        cx: &mut Context<Self>,
    ) {
        use rustshell_core::ai as ai_mod;
        // 会话标题取第一条提问（`chat_title_from` 会剥掉 `[当前终端上下文]` 包装）。
        // 必须在 `wrapped` 被 move 进消息之前算，否则要先 clone 一份长字符串。
        let title = chat_title_from(&wrapped);
        let is_first_question = self.ai_msgs().iter().all(|m| !m.role.is_user());
        // 消息先行（含 id 分配），见 AI 面板
        let user_msg = ai_mod::ChatMessage::user(self.next_ai_msg_id(), wrapped, injected_lines);
        let mut msgs = vec![ai_mod::ChatMessage::system(0, ai_mod::system_prompt())];
        msgs.extend(self.ai_msgs().iter().cloned());
        msgs.push(user_msg.clone());

        let key = self
            .ai_ui
            .ai_cfg
            .api_key_cipher
            .as_deref()
            .and_then(rustshell_platform::secret::unprotect)
            .unwrap_or_default();
        self.ai_msgs_mut().push(user_msg);
        self.ai_ui.ai_scroller.update(cx, |s, cx| s.append(1, cx)); // 与 push 成对，漏了就会少一行
                                                                    // 聊天记录列表里只有标题：全是「新对话」的列表等于没有列表。
        let now = rustshell_core::connections::unix_now_secs();
        {
            let chat = self.ai_cur_mut();
            if is_first_question && !title.is_empty() {
                chat.title = title;
            }
            chat.updated_at = now;
        }
        // 发问即把终端右侧那一列露出来：消息发出去了而面板收着，用户只能靠左侧
        // 列表猜「到底有没有回复」。不落盘 —— 这是「本次使用」的临时展开，
        // 用户显式关掉的选择仍由 `ai_close_chat` 写进设置。
        self.ai_ui.ai_chat_open = true;
        self.ai_ui.ai_error = None;
        self.ai_ui.ai_busy = true;
        self.ai_ui.ai_stream_open = false;
        let mut cfg = self.ai_ui.ai_cfg.clone();
        // 面板开关是本次请求的唯一口径（设置面板保存不会覆盖它）
        cfg.thinking = self.ai_ui.ai_thinking;
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.ai_ui.ai_cancel = Some(cancel.clone());
        // agent 模式（v1.1 §3.9）：开了工具调用且模型能力**不是**「明确不支持」时，
        // 走多步工具循环；否则退化成原来的单轮问答，行为与改造前一致。
        if cfg.agent_enabled && !ai_mod::current_capabilities(&cfg).tool_calling_unsupported() {
            self.ai_start_agent(&cfg, key, msgs, cancel, cx);
        } else {
            self.ai_ui.ai_pending = Some(ai_mod::spawn_chat(cfg, key, msgs, cancel));
        }
        cx.notify();
    }

    /// 起一轮交互式 agent（工具循环跑在后台线程上，进度经通道回来）。
    ///
    /// `msgs` 里的最后一条是**本轮提问**，其余是历史 —— 传给 agent 时把最后一条
    /// 摘出来当 `user`，避免它被当成「上一轮」而被重复注入。
    fn ai_start_agent(
        &mut self,
        cfg: &rustshell_core::ai::AiConfig,
        api_key: String,
        mut msgs: Vec<rustshell_core::ai::ChatMessage>,
        cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
        cx: &mut Context<Self>,
    ) {
        use rustshell_core::runner as r;
        let user = match msgs.pop() {
            Some(m) => m.content,
            None => return,
        };
        // system 由 `spec.system` 单独给，历史里不要再带一份（否则 transcript 里
        // 会出现两条 system，模型会把它当普通上下文）。
        let history: Vec<_> = msgs
            .into_iter()
            .filter(|m| m.role != rustshell_core::ai::Role::System)
            .collect();
        let profiles = self.conn_store.profiles.clone();
        let store = std::sync::Arc::new(self.conn_store.clone());
        let (tx, rx) = flume::unbounded::<r::AgentUiEvent>();
        let (dtx, drx) = flume::unbounded::<(String, r::AgentDecision)>();
        let alive = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let spec = r::InteractiveSpec {
            cfg: cfg.clone(),
            api_key,
            system: rustshell_core::ai::system_prompt(),
            user,
            history,
            profiles,
            thresholds: rustshell_core::alerts::LoadThresholds::default(),
            audit_path: rustshell_platform::paths::agent_audit_path(),
            download_dir: rustshell_platform::paths::default_download_dir(),
            whitelist: cfg.auto_approve.clone(),
            allow_mutating: cfg.agent_allow_mutating,
            cancel,
        };
        r::spawn_interactive_agent(
            spec,
            store,
            self.task_exec_backend(),
            tx,
            drx,
            alive.clone(),
        );
        self.ai_ui.ai_agent = Some(super::AgentRun {
            rx,
            decisions: dtx,
            alive,
            pending: None,
            msg_id: 0,
            opened: false,
        });
        // 轨迹块要立刻有落点：`Trace` 事件可能先于第一个正文增量到达，
        // 若等到 Delta 才建占位消息，前几步的工具轨迹会**丢在无人认领的地方**。
        self.ai_ensure_stream_msg(cx);
        // 先取出消息 id 再写回：`as_mut()` 的可变借与 `ai_msgs()` 的不可变借
        // 不能同时活着。
        let msg_id = self.ai_msgs().last().map(|m| m.id).unwrap_or(0);
        if let Some(run) = self.ai_ui.ai_agent.as_mut() {
            run.msg_id = msg_id;
            run.opened = true;
        }
    }

    /// agent 需要的 exec 后端（与定时任务同一份实现：SSH exec + 审计）。
    fn task_exec_backend(&self) -> std::sync::Arc<dyn rustshell_core::exec::ExecBackend> {
        std::sync::Arc::new(rustshell_core::exec::SshExec {
            // 与定时任务同一口径：AI 的 exec / 下载也走全局代理（F-10）。
            global_proxy: self.runtime_global_proxy(),
        })
    }

    /// 用户对确认卡片的答复（「执行」/「忽略」）。
    pub fn ai_confirm(&mut self, approve: bool, cx: &mut Context<Self>) {
        use rustshell_core::runner as r;
        let Some(run) = self.ai_ui.ai_agent.as_mut() else {
            return;
        };
        let Some(req) = run.pending.take() else {
            return;
        };
        let decision = if approve {
            r::AgentDecision::Approve
        } else {
            r::AgentDecision::Deny
        };
        // 发送失败（线程已退出）不是错误：卡片本来就该消失。
        let _ = run.decisions.send((req.call_id.clone(), decision));
        cx.notify();
    }

    /// 把后台线程来的 agent 事件消化进面板状态。返回是否有变更（决定要不要重绘）。
    fn ai_drain_agent(&mut self, cx: &mut Context<Self>) -> bool {
        use rustshell_core::runner as r;
        // 先把通道抽干再处理：处理过程中要可变借 `self`，不能同时持有 `&self.ai_ui.ai_agent`。
        let events: Vec<r::AgentUiEvent> = {
            let Some(run) = self.ai_ui.ai_agent.as_ref() else {
                return false;
            };
            let mut v = Vec::new();
            while let Ok(ev) = run.rx.try_recv() {
                v.push(ev);
                if v.len() >= 512 {
                    break; // 一轮的极端上限；剩下的下一轮 poll 接着收
                }
            }
            v
        };
        if events.is_empty() {
            return false;
        }
        let mut done: Option<(String, rustshell_core::agent::FinishReason)> = None;
        for ev in events {
            match ev {
                r::AgentUiEvent::Delta(d) => {
                    self.ai_ensure_stream_msg(cx);
                    let msgs = self.ai_msgs_mut();
                    if let Some(last) = msgs.last_mut() {
                        last.content.push_str(&d);
                        last.state = rustshell_core::ai::MsgState::Streaming;
                    }
                    // 轨迹块与正文同属一条消息，高度会一起变 —— 成对重测（§3.9）
                    self.ai_scroller_remeasure_last(cx);
                }
                r::AgentUiEvent::Trace(row) => {
                    let row = *row;
                    let target = self.ai_ui.ai_agent.as_ref().map(|r| r.msg_id);
                    if let Some(id) = target {
                        if let Some(msg) = self.ai_msgs_mut().iter_mut().find(|m| m.id == id) {
                            rustshell_core::agent::trace::upsert(&mut msg.trace, row);
                        }
                    }
                    self.ai_scroller_remeasure_last(cx);
                }
                r::AgentUiEvent::Confirm(req) => {
                    if let Some(run) = self.ai_ui.ai_agent.as_mut() {
                        run.pending = Some(*req);
                    }
                    self.ai_scroller_remeasure_last(cx);
                }
                r::AgentUiEvent::TaskProposed(task) => {
                    // 用户已经在确认卡片上点过「执行」⇒ 这条草案可以入库。
                    // 默认停用（`proposal::to_task` 已置位），等用户去任务面板
                    // 看一眼再决定启用 —— AI 提议的东西不该到点自己开跑。
                    // 走 `task_store_update`（落库 + 落盘 + 同步调度器快照）：
                    // 直接改 `self.io.task_store` 会被下一次 tick 覆盖掉。
                    let name = task.name.clone();
                    self.task_store_update(|s| {
                        s.add(*task);
                    });
                    self.show_task_toast(
                        rustshell_core::i18n::ts(
                            format!("AI 已添加任务草案「{name}」（默认停用）"),
                            format!("AI added task draft \"{name}\" (disabled)"),
                        ),
                        cx,
                    );
                }
                r::AgentUiEvent::Done { conclusion, reason } => {
                    done = Some((conclusion, reason));
                }
            }
        }
        if let Some((conclusion, reason)) = done {
            self.ai_finish_agent(&conclusion, &reason, cx);
        }
        true
    }

    /// agent 收尾：把结论写进当前消息、清掉在途状态、把「停止」按钮复位。
    fn ai_finish_agent(
        &mut self,
        conclusion: &str,
        reason: &rustshell_core::agent::FinishReason,
        cx: &mut Context<Self>,
    ) {
        use rustshell_core::ai::MsgState;
        self.ai_ensure_stream_msg(cx);
        let last_id = self.ai_msgs().last().map(|m| m.id);
        {
            let msgs = self.ai_msgs_mut();
            if let Some(last) = msgs.last_mut() {
                // 正文以最终结论为准收口：网关漏发末尾增量时也能补齐。
                // 正常结束时结论是**权威**的（它就是完整回复）；非正常结束
                // （超步数 / 被停止 / 出错）时 `conclusion` 往往只是一句说明，
                // 只在正文为空时才拿它兜底 —— 否则会把已经流出来的答案抹掉。
                if !conclusion.is_empty() && (reason.is_normal() || last.content.is_empty()) {
                    last.content = conclusion.to_string();
                }
                if matches!(reason, rustshell_core::agent::FinishReason::Cancelled) {
                    last.state = MsgState::Cancelled;
                } else if last.content.is_empty() {
                    last.state = MsgState::Failed(
                        reason
                            .note()
                            .unwrap_or_else(|| rustshell_core::i18n::t("未产生结论").to_string()),
                    );
                } else {
                    last.state = MsgState::Done;
                }
                // 收尾时把没收到结果的轨迹行标成「已停止」，不留永远转圈的行
                if !reason.is_normal() {
                    rustshell_core::agent::mark_stopped(&mut last.trace);
                }
            }
        }
        if let Some(id) = last_id {
            self.ai_ui.ai_think_expanded.remove(&id);
        }
        if let Some(mut run) = self.ai_ui.ai_agent.take() {
            run.shutdown();
        }
        self.ai_ui.ai_stream_open = false;
        self.ai_ui.ai_pending = None;
        self.ai_ui.ai_busy = false;
        self.ai_ui.ai_cancel = None;
        // 收尾原因追加一句说明（超出上限、被停止……），否则用户只看到「停了」。
        if let Some(note) = reason.note() {
            if !reason.is_normal() {
                self.ai_ui.ai_hint = Some(note);
            }
        }
        self.ai_scroller_remeasure_last(cx);
        // agent 收尾同理（见 `ai_poll_results` 的 Reply 分支）：结论与轨迹都已定稿
        self.ai_chats_touch();
        cx.notify();
    }

    /// 中止在途生成（用户点「停止」）。
    ///
    /// UI 立即收尾而不等线程回消息：线程可能正阻塞在下一次流读取上（读超时 120s），
    /// 等它就会让整个面板卡在「生成中」。收尾后 `ai_pending` 置空，线程后续发出的
    /// 增量按 id 判定为过期并丢弃，不会串到下一轮对话。
    pub fn ai_stop(&mut self, cx: &mut Context<Self>) {
        use rustshell_core::ai::MsgState;
        if !self.ai_ui.ai_busy {
            return;
        }
        if let Some(cancel) = &self.ai_ui.ai_cancel {
            cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        // agent 运行：置 `alive = false` 让后台线程的确认等待立刻返回，
        // 并把未决卡片撤掉（否则它会一直挂在面板上，点「执行」也没人接）。
        if let Some(mut run) = self.ai_ui.ai_agent.take() {
            run.shutdown();
        }
        if self.ai_ui.ai_stream_open {
            let last_id = self.ai_msgs().last().map(|m| m.id);
            if let Some(last) = self.ai_msgs_mut().last_mut() {
                last.state = MsgState::Cancelled;
            }
            if let Some(id) = last_id {
                self.ai_ui.ai_think_expanded.remove(&id);
            }
            // 未收到结果的轨迹行标成「已停止」（AC-7：部分轨迹保留并标记）
            if let Some(last) = self.ai_msgs_mut().last_mut() {
                rustshell_core::agent::mark_stopped(&mut last.trace);
            }
        }
        self.ai_ui.ai_stream_open = false;
        self.ai_ui.ai_pending = None;
        self.ai_ui.ai_busy = false;
        self.ai_ui.ai_cancel = None;
        self.ai_scroller_remeasure_last(cx);
        // 「停止」把在途消息定成 `Cancelled`（半截答复），也是定稿
        self.ai_chats_touch();
        cx.notify();
    }

    /// 思考模式开关：面板 Toggle 的唯一入口（同时落盘，重启后保持）。
    pub fn ai_toggle_thinking(&mut self, on: bool, cx: &mut Context<Self>) {
        if self.ai_ui.ai_thinking == on {
            return;
        }
        self.ai_ui.ai_thinking = on;
        self.ai_ui.ai_cfg.thinking = on;
        let _ = self.ai_ui.ai_cfg.save(&paths::ai_config_path());
        cx.notify();
    }

    /// 展开 / 折叠某条消息的「思考过程」折叠块。
    pub fn ai_toggle_think_block(&mut self, id: u64, cx: &mut Context<Self>) {
        if !self.ai_ui.ai_think_expanded.remove(&id) {
            self.ai_ui.ai_think_expanded.insert(id);
        }
        // 折叠块改了行高 到 必须重测该行，否则下方内容会被压在旧高度里
        if let Some(row) = self.ai_msgs().iter().position(|m| m.id == id) {
            self.ai_ui
                .ai_scroller
                .update(cx, |s, cx| s.remeasure_items(row..row + 1, cx));
        }
        cx.notify();
    }

    /// 展开 / 折叠某行轨迹的「明细」（键 = 消息 id + call_id）。
    ///
    /// 官方 `Collapsible` 是**受控只读区域**（没有点击回调，`open=false` 时
    /// content 根本不渲染），点了没反应 —— 「明细无法打开」的根因。展开态只能
    /// 由这里持有，与「思考过程」同一套做法。
    pub fn ai_toggle_trace_detail(&mut self, msg_id: u64, call_id: String, cx: &mut Context<Self>) {
        let key = (msg_id, call_id);
        if !self.ai_ui.ai_trace_expanded.remove(&key) {
            self.ai_ui.ai_trace_expanded.insert(key);
        }
        // 与思考过程同理：明细改了行高，不重测会让下方内容压在旧高度里
        if let Some(row) = self.ai_msgs().iter().position(|m| m.id == msg_id) {
            self.ai_ui
                .ai_scroller
                .update(cx, |s, cx| s.remeasure_items(row..row + 1, cx));
        }
        cx.notify();
    }

    /// 流式占位行：首个增量（正文或思考）到达时插入 assistant 行并同步 scroller。
    fn ai_ensure_stream_msg(&mut self, cx: &mut Context<Self>) {
        use rustshell_core::ai as ai_mod;
        if self.ai_ui.ai_stream_open {
            return;
        }
        let id = self.next_ai_msg_id();
        self.ai_msgs_mut()
            .push(ai_mod::ChatMessage::assistant_streaming(id));
        self.ai_ui.ai_stream_open = true;
        self.ai_ui.ai_scroller.update(cx, |s, cx| s.append(1, cx));
    }

    /// 重新生成：去掉末尾的 assistant 回复后，重发最后一条用户消息
    pub fn ai_regen(&mut self, cx: &mut Context<Self>) {
        if self.ai_ui.ai_busy || self.ai_ui.ai_pending.is_some() {
            return;
        }
        let Some(pos) = self.ai_msgs().iter().rposition(|m| m.role.is_user()) else {
            return;
        };
        let wrapped = self.ai_msgs()[pos].content.clone();
        let injected = self.ai_msgs()[pos].injected_lines;
        // 存活 id 先取成局部量：`ai_think_expanded.retain(..)` 已经可变借了
        // `self.ai_ui.ai_think_expanded`，而 `self.ai_msgs()` 是对 `self` 的方法调用
        // （借整个 self）—— 字段级精确捕获救不了方法调用，闭包里直接调会 E0502。
        let alive: Vec<u64> = self.ai_msgs()[..pos].iter().map(|m| m.id).collect();
        self.ai_msgs_mut().truncate(pos);
        self.ai_ui.ai_think_expanded.retain(|id| alive.contains(id));
        self.ai_ui
            .ai_trace_expanded
            .retain(|(mid, _)| alive.contains(mid));
        let len = self.ai_msgs().len();
        self.ai_ui.ai_scroller.update(cx, |s, cx| s.reset(len, cx));
        self.ai_send_wrapped(wrapped, injected, cx);
    }

    /// 删除指定消息（6.10 节 气泡菜单）。按稳定 id 定位，不按行索引，行索引会随虚拟列表增删漂移（需求文档 5.7 节）。
    pub fn ai_delete_msg(&mut self, id: u64, cx: &mut Context<Self>) {
        if self.ai_ui.ai_busy {
            return;
        }
        let Some(ix) = self.ai_msgs().iter().position(|m| m.id == id) else {
            return;
        };
        self.ai_msgs_mut().remove(ix);
        // 折叠态随消息一起消失，避免 id 集无限增长
        self.ai_ui.ai_think_expanded.remove(&id);
        self.ai_ui.ai_trace_expanded.retain(|(mid, _)| *mid != id);
        self.ai_ui.ai_scroller.update(cx, |s, cx| {
            s.splice(ix..ix + 1, 0, cx);
        });
        cx.notify();
    }

    // ---- 会话（聊天记录） ----------------------------------------------------

    /// 当前会话。不变量：`ai_chats` 恒非空、`ai_chat_id` 恒在其内
    /// （由 `new` 的初始化与 `ai_delete_chat` 的补齐共同维持）。
    pub fn ai_cur(&self) -> &AiChat {
        self.ai_ui
            .ai_chats
            .iter()
            .find(|c| c.id == self.ai_ui.ai_chat_id)
            .unwrap_or(&self.ai_ui.ai_chats[0])
    }

    /// 当前会话（可变）。索引缺失时回落到第 0 条，与 `ai_cur` 同一兜底口径 ——
    /// 两条路径必须同源，否则「读到的」与「写进去的」会不是同一条会话。
    ///
    /// **取到可变引用 = 有改动**，顺手打上落盘标记（节流的那种，见
    /// [`MainView::ai_chats_touch_streaming`]）：消息的增删改全都从
    /// `ai_msgs_mut()` 这一条路进来，在这里收口比在十几个调用点各补一行更难漏
    /// —— 漏一处的表现是「聊了半天，重启后少了最后一段」，而那是最难被发现的
    /// 失败模式（用户只会觉得「保存功能时灵时不灵」）。
    pub fn ai_cur_mut(&mut self) -> &mut AiChat {
        self.ai_chats_touch_streaming();
        let id = self.ai_ui.ai_chat_id;
        let ix = self
            .ai_ui
            .ai_chats
            .iter()
            .position(|c| c.id == id)
            .unwrap_or(0);
        &mut self.ai_ui.ai_chats[ix]
    }

    /// 当前会话的消息（渲染与请求组包都读它）
    pub fn ai_msgs(&self) -> &[rustshell_core::ai::ChatMessage] {
        &self.ai_cur().msgs
    }

    /// 当前会话的消息（可变）
    pub fn ai_msgs_mut(&mut self) -> &mut Vec<rustshell_core::ai::ChatMessage> {
        &mut self.ai_cur_mut().msgs
    }

    /// 分配下一个消息 id（gpui 元素 id 由它派生，替代易漂移的行索引）。
    /// 编号随会话走：每条会话各自从 1 起，同一时刻只有当前会话在渲染。
    pub(super) fn next_ai_msg_id(&mut self) -> u64 {
        let chat = self.ai_cur_mut();
        let id = chat.next_msg_id;
        chat.next_msg_id += 1;
        id
    }

    /// 新建一条会话并切过去（对话面板头部的 `+`、聊天记录列表顶部的「新建对话」）。
    ///
    /// 当前会话还是空白时直接复用：否则连点两次就会攒出两条一模一样的空会话，
    /// 而用户在列表里分辨不出它们（标题都是「新对话」）。
    pub fn ai_new_chat(&mut self, cx: &mut Context<Self>) {
        self.ai_ui.ai_chat_open = true;
        if self.ai_cur().is_empty() {
            cx.notify();
            return;
        }
        self.ai_add_chat(cx);
    }

    /// 追加一条空白会话并切过去（删除最后一条之后的补齐也走这里）
    fn ai_add_chat(&mut self, cx: &mut Context<Self>) {
        let id = self.ai_ui.ai_next_chat_id;
        self.ai_ui.ai_next_chat_id += 1;
        self.ai_ui.ai_chats.push(AiChat::new(
            id,
            rustshell_core::connections::unix_now_secs(),
        ));
        // 会话表本身变了（不是消息）：走「下个 tick 就写」那档，不等流式节流
        self.ai_chats_touch();
        self.ai_switch_chat(id, cx);
    }

    /// 切到某条会话（聊天记录列表行的点击）。
    ///
    /// 生成中不允许切换：在途答复是按「当前会话的最后一条消息」收口的
    /// （见 `ai_poll_results`），切走之后增量就会写进另一条会话里。
    /// 列表行据此置灰，用户看得到「现在不能切」这个原因。
    pub fn ai_switch_chat(&mut self, id: u64, cx: &mut Context<Self>) {
        self.ai_ui.ai_chat_open = true;
        if self.ai_ui.ai_busy || self.ai_ui.ai_chat_id == id {
            cx.notify();
            return;
        }
        if !self.ai_ui.ai_chats.iter().any(|c| c.id == id) {
            return;
        }
        self.ai_ui.ai_chat_id = id;
        // 「当前会话」也是落盘内容的一部分（重启后回到哪一条）：用户刚切完就关窗
        // 也该记住，所以是立即写那档。
        self.ai_chats_touch();
        // 折叠集与 scroller 都跟着换：前者按消息 id 记账，后者必须与行数成对更新
        self.ai_ui.ai_think_expanded.clear();
        let len = self.ai_msgs().len();
        self.ai_ui.ai_scroller.update(cx, |s, cx| s.reset(len, cx));
        cx.notify();
    }

    /// 把一次定时任务的运行痕迹挂进聊天记录，返回会话 id。
    ///
    /// 幂等：同一次运行只会有一条会话 —— `ai_run_chats` 记着 `run_id → chat_id`，
    /// 无论被调用多少次（每秒的自动接线、用户反复点「会话」）都回到同一条。
    /// 返回 `None` 只有一种情况：这条运行**没有**留下会话痕迹（老记录、
    /// 非 AI 动作，或巡检一句话没说、一个工具没调）—— 调用方据此如实提示。
    pub(super) fn ai_attach_run_session(
        &mut self,
        rec: &rustshell_core::tasks::RunRecord,
    ) -> Option<u64> {
        if let Some(id) = self.ai_ui.ai_run_chats.get(&rec.run_id).copied() {
            // 会话还在（用户没把它删掉）就复用；被删了则按下面的流程重建一条。
            if self.ai_ui.ai_chats.iter().any(|c| c.id == id) {
                return Some(id);
            }
        }
        let session = rec.session.as_ref()?;
        let id = self.ai_ui.ai_next_chat_id;
        self.ai_ui.ai_next_chat_id += 1;
        let now = rustshell_core::connections::unix_now_secs();
        let mut chat = AiChat::new(id, now);
        // 标题 =「任务名 · 时间」：列表里同一条任务会跑很多次，只写任务名
        // 就是几行一模一样的「夜间巡检」，用户分不出哪次是哪次。
        chat.title = super::chat_title_from(&format!(
            "{} · {}",
            rec.task_name,
            rustshell_core::metrics::datetime_label(rec.started_at)
        ));
        let mut next = 1;
        let user = rustshell_core::ai::ChatMessage::user(next, run_session_prompt(rec, session), 0);
        next += 1;
        let mut reply =
            rustshell_core::ai::ChatMessage::assistant(next, session.conclusion.clone());
        // 轨迹挂在回复上（与交互式巡检同一手法）：面板里那个可折叠的轨迹块
        // 就是「这次到底查了什么」，它是会话里信息量最大的部分。
        reply.trace = session.trace.clone();
        next += 1;
        chat.msgs = vec![user, reply];
        chat.next_msg_id = next;
        self.ai_ui.ai_chats.push(chat);
        self.ai_ui.ai_run_chats.insert(rec.run_id.clone(), id);
        // 会话与映射一起变：映射也要落盘，否则重启后点同一次运行的「会话」
        // 会再建一条（见 `ai_persist::normalize` 的悬空剔除）。
        self.ai_chats_touch();
        Some(id)
    }

    /// 删除一条会话（聊天记录列表行上的删除按钮）。
    ///
    /// 删完必须仍有当前会话：删掉最后一条时补一条空白 —— 「一条会话都没有」
    /// 会让 `ai_cur()` 失去落点，而所有 AI 动作都从它出发。
    pub fn ai_delete_chat(&mut self, id: u64, cx: &mut Context<Self>) {
        if self.ai_ui.ai_busy {
            return;
        }
        let Some(ix) = self.ai_ui.ai_chats.iter().position(|c| c.id == id) else {
            return;
        };
        self.ai_ui.ai_chats.remove(ix);
        // 删掉一条是用户动作：立即写那档（下面若走 `ai_add_chat` 补齐，那里也会标一次，
        // 标两次无副作用）
        self.ai_chats_touch();
        if self.ai_ui.ai_chats.is_empty() {
            self.ai_add_chat(cx);
            return;
        }
        if self.ai_ui.ai_chat_id == id {
            // 落到最近改动的那条（列表顺序就是「最近优先」）
            let next = self
                .ai_ui
                .ai_chats
                .iter()
                .max_by_key(|c| c.updated_at)
                .map(|c| c.id)
                .unwrap_or(self.ai_ui.ai_chats[0].id);
            self.ai_ui.ai_chat_id = next;
            self.ai_ui.ai_think_expanded.clear();
            let len = self.ai_msgs().len();
            self.ai_ui.ai_scroller.update(cx, |s, cx| s.reset(len, cx));
        }
        cx.notify();
    }

    /// 展开终端右侧的 AI 对话面板（幂等；开合状态落盘）。
    pub fn ai_open_chat(&mut self, cx: &mut Context<Self>) {
        if !self.ai_ui.ai_chat_open {
            self.ai_ui.ai_chat_open = true;
            self.settings.ai_chat_open = true;
            let _ = self.settings.save(&paths::settings_path());
        }
        cx.notify();
    }

    /// 收起终端右侧的 AI 对话面板（头部 ×）。左侧栏的聊天记录不受影响。
    pub fn ai_close_chat(&mut self, cx: &mut Context<Self>) {
        self.ai_ui.ai_chat_open = false;
        self.settings.ai_chat_open = false;
        let _ = self.settings.save(&paths::settings_path());
        cx.notify();
    }

    /// 忽略某条回复上的某张命令卡片（消息 id + 卡片下标定位）。 卡片消失会改变该行高度 到 必须同步重测，否则行高残留、下方内容重叠。
    pub fn ai_card_dismiss(&mut self, msg_id: u64, card_ix: usize, cx: &mut Context<Self>) {
        let Some(row) = self.ai_msgs().iter().position(|m| m.id == msg_id) else {
            return;
        };
        if let Some(c) = self.ai_msgs_mut()[row].commands.get_mut(card_ix) {
            c.done = true;
        }
        self.ai_ui
            .ai_scroller
            .update(cx, |s, cx| s.remeasure_items(row..row + 1, cx));
        cx.notify();
    }

    /// 正文/行内容变化后重测末行。 只改`content`不重测 到 行高不更新，表现为「末行被截断 / 文字重叠」。
    fn ai_scroller_remeasure_last(&mut self, cx: &mut Context<Self>) {
        let len = self.ai_msgs().len();
        if len == 0 {
            return;
        }
        self.ai_ui
            .ai_scroller
            .update(cx, |s, cx| s.remeasure_items(len - 1..len, cx));
    }

    /// 加入 auto-approve 白名单（命令卡片的「加入白名单」动作）。
    ///
    /// 与白名单配置框里的「添加」写的是同一个字段、同一条规则：已存在就顺手打开它，
    /// 不存在才追加（2026-09-22 白名单升级为带开关的规则表，见 `ai::WhitelistRule`）。
    /// 这条路径**立即落盘** —— 它不在设置面板里，没有「保存 / 取消」可依托。
    pub fn ai_add_whitelist(&mut self, cmd: &str, cx: &mut Context<Self>) {
        let cmd = cmd.trim();
        if cmd.is_empty() {
            return;
        }
        match self
            .ai_ui
            .ai_cfg
            .auto_approve
            .iter_mut()
            .find(|r| r.command == cmd)
        {
            Some(r) if r.enabled => return,
            Some(r) => r.enabled = true,
            None => self
                .ai_ui
                .ai_cfg
                .auto_approve
                .push(rustshell_core::ai::WhitelistRule::new(cmd)),
        }
        let _ = self.ai_ui.ai_cfg.save(&paths::ai_config_path());
        self.ai_ui.ai_hint = Some(rustshell_core::i18n::t("已加入免确认白名单").into());
        cx.notify();
    }

    /// 保存 AI 配置（API Key 经 DPAPI 加密；留空沿用旧 Key）
    pub fn ai_save_config(&mut self, _w: &mut Window, cx: &mut Context<Self>) {
        match self.sync_ai_config(cx) {
            Ok(()) => self.ai_ui.ai_hint = Some(rustshell_core::i18n::t("已保存").into()),
            Err(e) => self.ai_ui.ai_hint = Some(e),
        }
        cx.notify();
    }

    /// 从 AI 输入框收集端点/模型/Key 并落盘（AI 面板「保存」 与设置面板「保存」共用同一入口，避免两处漂移）。 返回`Err(人类可读原因)`表示校验或加密失败，调用方就地提示并保持面板打开。
    pub(super) fn sync_ai_config(&mut self, cx: &mut Context<Self>) -> Result<(), String> {
        let base = self
            .ai_ui
            .ai_cfg_base
            .read(cx)
            .value()
            .trim()
            .trim_end_matches('/')
            .to_string();
        let model = self.ai_ui.ai_cfg_model.read(cx).value().trim().to_string();
        let key_plain = self.ai_ui.ai_cfg_key.read(cx).value().trim().to_string();

        if base.is_empty() || model.is_empty() {
            return Err(rustshell_core::i18n::t("端点与模型名不能为空").to_string());
        }
        // 采样温度不进这里：设置面板已改用官方 `Slider` 直接写 `ai_cfg.temperature`
        // （滑块状态与订阅在 `MainView::new`），故此处不再解析文本、也不再需要区间校验。
        // 兜底：即使经其它路径写脏，也在请求体侧 clamp（见 core::ai 组包）。
        // 上下文行数同理，它已经不是配置项（`rustshell_core::ai::CONTEXT_LINES`）。

        self.ai_ui.ai_cfg.base_url = base;
        self.ai_ui.ai_cfg.model = model;
        if !key_plain.is_empty() {
            match rustshell_platform::secret::protect("ai-api-key", &key_plain) {
                Ok(cipher) => self.ai_ui.ai_cfg.api_key_cipher = Some(cipher),
                Err(_) => {
                    return Err(
                        rustshell_core::i18n::t("API Key 加密失败（系统凭据存储不可用）")
                            .to_string(),
                    )
                }
            }
        }
        self.ai_ui
            .ai_cfg
            .save(&paths::ai_config_path())
            .map_err(|e| {
                rustshell_core::i18n::ts(format!("保存失败: {e}"), format!("Save failed: {e}"))
            })
    }

    /// 轮询 AI 结果（MainView::poll 调用）：按在途 id 匹配，过期忽略。 流式：Delta 增量拼接到在途 assistant 消息；Reply 收尾（提取命令卡片）。
    ///
    /// 返回**本轮是否消费到了结果**（= 是否改变了需要重绘的状态），
    /// 供 `MainView::poll` 决定要不要 `cx.notify()`：没有在途请求时本函数
    /// 每 300ms 只是对空通道做一次 `try_recv`，不该顺带重绘整棵窗口树。
    pub fn ai_poll_results(&mut self, cx: &mut Context<Self>) -> bool {
        use rustshell_core::ai as ai_mod;
        // agent 通道优先：它与旧通道**互斥**（一轮只会走其中一条），
        // 但仍然要先抽干 agent 事件，否则 `Done` 会被后面的早退分支漏掉。
        let mut changed = self.ai_drain_agent(cx);
        while let Ok(res) = ai_mod::ai_results().1.try_recv() {
            if Some(res.id) != self.ai_ui.ai_pending {
                continue; // 过期结果
            }
            changed = true;
            match res.outcome {
                ai_mod::AiOutcome::Delta(delta) => {
                    //5.2 节：首个增量插入 assistant 占位行（+ scroller.append）；
                    //5.3 节：后续增量只改正文 到 必须 remeasure 该行，否则末行高度不更新。
                    self.ai_ensure_stream_msg(cx);
                    {
                        // 借期收在块里：`ai_msgs_mut()` 借的是整个 `self`，
                        // 紧接着的 `ai_scroller_remeasure_last` 也是 `&mut self`。
                        let msgs = self.ai_msgs_mut();
                        if let Some(last) = msgs.last_mut() {
                            last.content.push_str(&delta);
                            last.state = ai_mod::MsgState::Streaming;
                        }
                    }
                    self.ai_scroller_remeasure_last(cx);
                }
                ai_mod::AiOutcome::Reasoning(reason) => {
                    // 思考过程与正文共用同一个占位行：先思考后回答是同一轮回复，
                    // 拆成两行会让虚拟列表凭空多一行（且两行都必须 remeasure）。
                    self.ai_ensure_stream_msg(cx);
                    // 思考期间默认展开（行 id 出块后再盖戳：`ai_think_expanded`
                    // 与 `ai_msgs_mut()` 不能同时可变借 `self`）
                    let stream_id = {
                        let msgs = self.ai_msgs_mut();
                        msgs.last_mut().map(|last| {
                            last.reasoning.push_str(&reason);
                            last.state = ai_mod::MsgState::Streaming;
                            last.id
                        })
                    };
                    if let Some(id) = stream_id {
                        self.ai_ui.ai_think_expanded.insert(id);
                    }
                    self.ai_scroller_remeasure_last(cx);
                }
                ai_mod::AiOutcome::ToolCalls(calls) => {
                    // 这条通道**不带工具清单**（`spawn_chat` 走的是
                    // `tools = []`），正常路径下永远不会出现 tool_calls：
                    // agent 模式整轮都跑在 `spawn_interactive_agent` 的私有通道上
                    // （见 `ai_start_agent`）。所以走到这里说明有别处用工具清单
                    // 发起了请求却没接住 —— 既不静默吞掉，也不假装执行：
                    // 明确告诉用户「这一轮不会执行工具」，并留下日志便于定位。
                    log::warn!(
                        "AI 请求了 {} 次工具调用，但发起方未接入 agent 循环，本次不执行：{}",
                        calls.len(),
                        calls
                            .iter()
                            .map(|c| c.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                    self.ai_ui.ai_hint = Some(
                        rustshell_core::i18n::t(
                            "本轮模型请求了工具调用，但请求未经过 agent 通道，已忽略。请重新发送",
                        )
                        .to_string(),
                    );
                }
                ai_mod::AiOutcome::ToolsUnsupported => {
                    // 能力结论落盘（v1.1 §3.8.1）：下次请求不再带 `tools`，
                    // 省掉每轮一次注定失败的往返。面板给一次性提示而不是沉默。
                    let mut caps = ai_mod::ModelCapabilities::for_provider(&self.ai_ui.ai_cfg);
                    caps.note_tool_calling(false);
                    self.ai_ui.ai_cfg.capabilities = Some(caps);
                    let _ = self.ai_ui.ai_cfg.save(&paths::ai_config_path());
                    self.ai_ui.ai_hint = Some(
                        rustshell_core::i18n::t(
                            "当前模型不支持工具调用，已降级为问答模式（可在设置里关闭 Agent）",
                        )
                        .to_string(),
                    );
                    log::info!("模型能力缓存已更新：tool_calling = unsupported");
                }
                ai_mod::AiOutcome::Reply(reply) => {
                    if !self.ai_ui.ai_stream_open {
                        let id = self.next_ai_msg_id();
                        self.ai_msgs_mut()
                            .push(ai_mod::ChatMessage::assistant(id, reply.clone()));
                        self.ai_ui.ai_scroller.update(cx, |s, cx| s.append(1, cx));
                    } else {
                        let last_id = self.ai_msgs().last().map(|m| m.id);
                        {
                            let msgs = self.ai_msgs_mut();
                            if let Some(last) = msgs.last_mut() {
                                // 正文以完整回复为准收口：网关漏发末尾增量时也能补齐
                                if last.content.is_empty() && !reply.is_empty() {
                                    last.content = reply.clone();
                                }
                                last.state = ai_mod::MsgState::Done;
                            }
                        }
                        // 收尾即折叠思考过程（用户想回看再点开）
                        if let Some(id) = last_id {
                            self.ai_ui.ai_think_expanded.remove(&id);
                        }
                    }
                    self.ai_ui.ai_stream_open = false;
                    // L2：提取围栏命令到白名单直跑或确认卡片
                    if let Some(cmd) = ai_mod::extract_command(&reply) {
                        let risk = ai_mod::assess_command(&cmd);
                        // 与 agent 侧同一口径：关闭的规则不生效、支持 `*` 通配符。
                        // 两处各写一份匹配的话，用户在弹窗里关掉一条却仍被直跑，
                        // 就成了「安全开关是假的」。
                        let whitelisted = rustshell_core::ai::whitelist_hit(
                            &self.ai_ui.ai_cfg.auto_approve,
                            &cmd,
                        ) && risk != ai_mod::Risk::Blocked;
                        if whitelisted {
                            self.ai_execute_command(&cmd, cx);
                        } else if let Some(msg) = self
                            .ai_msgs_mut()
                            .iter_mut()
                            .rev()
                            .find(|m| m.role == ai_mod::Role::Assistant)
                        {
                            // 卡片挂在产生它的回复上（行高变化由下方 remeasure 兜住）
                            msg.commands.push(ai_mod::AiCommandCard {
                                cmd,
                                risk,
                                done: false,
                            });
                        }
                    }
                    // 卡片出现会改变该行高度 到 必须重测
                    self.ai_scroller_remeasure_last(cx);
                    self.ai_ui.ai_pending = None;
                    self.ai_ui.ai_busy = false;
                    self.ai_ui.ai_cancel = None;
                    // 一轮收尾：内容已定稿（正文 / 卡片 / 状态都不会再变），立刻落盘 ——
                    // 用户常见的动作就是「看完答复顺手关窗」，不该让最后一段等节流。
                    // 下面两个终态分支同理。
                    self.ai_chats_touch();
                }
                ai_mod::AiOutcome::Cancelled => {
                    // 正常路径下 `ai_stop` 已经就地收尾过（此处只是兜底：
                    // 旗标在线程侧先被观察到，UI 还没点过停止）。
                    if self.ai_ui.ai_stream_open {
                        let last_id = self.ai_msgs().last().map(|m| m.id);
                        {
                            let msgs = self.ai_msgs_mut();
                            if let Some(last) = msgs.last_mut() {
                                last.state = ai_mod::MsgState::Cancelled;
                            }
                        }
                        if let Some(id) = last_id {
                            self.ai_ui.ai_think_expanded.remove(&id);
                        }
                    }
                    self.ai_ui.ai_stream_open = false;
                    self.ai_ui.ai_pending = None;
                    self.ai_ui.ai_busy = false;
                    self.ai_ui.ai_cancel = None;
                    self.ai_scroller_remeasure_last(cx);
                    self.ai_chats_touch();
                }
                ai_mod::AiOutcome::Error(e) => {
                    // 在途行标记失败（气泡据此提示「重试」）；若首个增量前就失败，则没有占位行
                    if self.ai_ui.ai_stream_open {
                        let msgs = self.ai_msgs_mut();
                        if let Some(last) = msgs.last_mut() {
                            last.state = ai_mod::MsgState::Failed(e.clone());
                        }
                    }
                    self.ai_ui.ai_stream_open = false;
                    self.ai_ui.ai_pending = None;
                    self.ai_ui.ai_busy = false;
                    self.ai_ui.ai_cancel = None;
                    self.ai_ui.ai_error = Some(e);
                    self.ai_scroller_remeasure_last(cx);
                    self.ai_chats_touch();
                }
            }
        }
        changed
    }

    /// 执行命令卡片（写入当前会话 PTY，回车触发）
    pub fn ai_execute_command(&mut self, cmd: &str, cx: &mut Context<Self>) {
        use rustshell_core::ai as ai_mod;
        let risk = ai_mod::assess_command(cmd);
        if risk == ai_mod::Risk::Blocked {
            // 审计：拦截也留痕
            ai_mod::audit_log(&paths::agent_audit_path(), "ai_cmd", cmd, risk, false);
            self.ai_ui.ai_error =
                Some(rustshell_core::i18n::t("该命令命中拦截黑名单，已拒绝执行").into());
            cx.notify();
            return;
        }
        self.send_to_active_pty(cmd, cx);
        // 审计：agent-audit.log 全量工具调用
        ai_mod::audit_log(&paths::agent_audit_path(), "ai_cmd", cmd, risk, true);
        // 所有未完成的卡片一并作废（卡片挂在各条回复上）
        for msg in self.ai_msgs_mut().iter_mut() {
            for c in msg.commands.iter_mut() {
                if !c.done {
                    c.done = true;
                }
            }
        }
        let id = self.next_ai_msg_id();
        self.ai_msgs_mut().push(ai_mod::ChatMessage::assistant(
            id,
            rustshell_core::i18n::ts(
                format!("已发送执行：`{cmd}`（输出见终端）"),
                format!("Sent to run: `{cmd}` (see terminal output)"),
            ),
        ));
        // append 与 push 成对，漏写会少一行。
        self.ai_ui.ai_scroller.update(cx, |s, cx| s.append(1, cx));
        cx.notify();
    }
    /// AI 直通（命令面板`ai <描述>`到 AI 面板自动发送；空态示例问题也走这里）
    pub(crate) fn ai_passthrough(&mut self, text: &str, w: &mut Window, cx: &mut Context<Self>) {
        self.panel_ui.panel = PanelKind::Ai;
        self.panel_ui.sidebar_collapsed = false;
        // 2026-09-18 运行反馈「点击 AI 助手，聊天界面显示在终端右侧」：进入 AI 面板
        // 必须**同时**展开终端右侧那一列 —— 左侧栏现在只放聊天记录列表，只切侧栏
        // 就是「一列列表 + 一面空白」。命令面板的「AI助手」也走这里
        // （见 `main_view.rs` 的 `on_action(&AiAssistant)`）。
        //
        // 不落盘（与 `ai_send_wrapped` 同一口径）：显式的开合状态由竖栏 AI 图标与
        // 面板头部的关闭按钮负责写进设置；这里是「带用户过去看」，不该改他的布局偏好。
        // 顺带的好处：本函数是单测入口（见 `ai_panel::tests`），不落盘就不会在跑测试时
        // 改写用户真实的 settings.json。
        self.ai_ui.ai_chat_open = true;
        if text.is_empty() {
            cx.notify();
            return;
        }
        if !self.ai_ui.ai_cfg.is_configured() {
            self.ai_ui.ai_cfg_open = true;
            self.ai_ui.ai_hint = Some(rustshell_core::i18n::t("请先配置 AI 端点与 API Key").into());
            cx.notify();
            return;
        }
        self.ai_ui
            .ai_input
            .update(cx, |s, cx| s.set_value(text.to_string(), w, cx));
        self.ai_send(w, cx);
    }
}

/// 定时任务运行会话里的**用户消息**：任务里那条「AI 要求」原文，
/// 外加一行「哪条任务、什么时候跑的」。
///
/// 为什么要加后面那行：会话一旦进了聊天记录就与运行记录脱了钩，
/// 用户翻聊天记录时只看到一段要求，分不清它属于哪次运行。
fn run_session_prompt(
    rec: &rustshell_core::tasks::RunRecord,
    session: &rustshell_core::tasks::RunSession,
) -> String {
    let body = session.prompt.trim();
    let body = if body.is_empty() {
        rustshell_core::i18n::ts(
            "（这次巡检没有填写 AI 要求）".into(),
            "(no AI prompt was set for this run)".into(),
        )
    } else {
        body.to_string()
    };
    let when = rustshell_core::metrics::datetime_label(rec.started_at);
    format!(
        "{body}\n\n{}",
        rustshell_core::i18n::ts(
            format!("（定时任务「{}」于 {when} 自动运行）", rec.task_name),
            format!("(scheduled task \"{}\" ran at {when})", rec.task_name),
        )
    )
}

#[cfg(test)]
mod tests {
    use crate::test_support::prod;

    fn code() -> String {
        prod(include_str!("ai_actions.rs"))
    }

    /// agent 模式必须真的接上工具循环，而不是留一个占位日志。
    ///
    /// 这条护栏的由来：接线的第一版是 `AiOutcome::ToolCalls` 分支里一句
    /// `log::warn!("agent 轨迹接线尚未完成")` —— 编译通过、界面正常、
    /// 单测全绿，但**模型永远拿不到工具结果**，回复会变成答非所问。
    /// 因此这里正向要求接线存在，反向要求占位消失。
    #[test]
    fn agent_mode_is_wired_to_the_interactive_loop() {
        let c = code();
        for needle in [
            "spawn_interactive_agent",
            "InteractiveSpec",
            "ai_drain_agent",
            "ai_confirm",
        ] {
            assert!(
                c.contains(needle),
                "agent 接线缺了 `{needle}` —— 模型请求的工具不会被执行。"
            );
        }
        assert!(
            !c.contains("轨迹接线尚未完成"),
            "`ToolCalls` 分支里还留着「接线尚未完成」的占位：工具调用会被静默丢弃。"
        );
        // 工具清单必须按能力探测结果收放：模型明确不支持时不该再带 `tools`
        assert!(
            c.contains("tool_calling_unsupported"),
            "起 agent 之前必须查一次「模型是否支持工具调用」（v1.1 §3.8.1）。"
        );
        // 停止按钮要能把后台线程叫停
        assert!(
            c.contains("run.shutdown()"),
            "`ai_stop` 必须关掉 agent 运行（否则确认等待会把线程挂满超时）。"
        );
    }

    /// 确认卡片与轨迹块都必须在 UI 侧有落点（core 发得出来，接不住也没用）。
    #[test]
    fn every_agent_event_has_a_ui_handler() {
        let c = code();
        for ev in [
            "AgentUiEvent::Delta",
            "AgentUiEvent::Trace",
            "AgentUiEvent::Confirm",
            "AgentUiEvent::Done",
            "AgentUiEvent::TaskProposed",
        ] {
            assert!(c.contains(ev), "面板没有处理 `{ev}` —— 该事件会被丢掉。");
        }
        // 草案入库必须走统一变更入口（`Scheduler::update_store` 内部落盘：
        // 临时改 UI 副本会在下一次 tick 被调度器快照覆盖）
        assert!(
            c.contains("self.task_store_update(|s| {") && c.contains("s.add(*task)"),
            "AI 建的草案必须经 `task_store_update` 入库（落盘 + 同步调度器）。"
        );
    }

    /// 一次跑完的定时任务要在聊天记录里留下**一条**会话，且再点也只回到这一条。
    ///
    /// 「每次执行都会创建一条 AI 会话记录」是用户明说的要求，而这条链路横跨
    /// 后端线程（写运行记录）/ 每秒 tick（发现新记录）/ 聊天记录（建会话）三段，
    /// 任何一段断了都只是「聊天记录里少几条」—— 界面上看不出坏了。
    #[gpui::test]
    fn a_finished_run_becomes_exactly_one_chat(cx: &mut gpui::TestAppContext) {
        use gpui::{AppContext as _, Entity};
        cx.update(|cx| {
            gpui_component::init(cx);
            crate::theme::apply_theme_mode(cx, rustshell_core::theme::ThemeMode::Dark);
        });
        let slot: std::rc::Rc<std::cell::RefCell<Option<Entity<super::MainView>>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let (_, vcx) = {
            let slot = slot.clone();
            cx.add_window_view(move |window, cx| {
                let main = cx.new(|cx| super::MainView::new(window, cx));
                *slot.borrow_mut() = Some(main.clone());
                let shell = cx.new(|_| crate::main_view::AppShell::new(main));
                gpui_component::Root::new(shell, window, cx)
            })
        };
        vcx.run_until_parked();
        let main = slot.borrow().clone().expect("MainView 未创建");

        let rec = rustshell_core::tasks::RunRecord {
            run_id: "r-1".into(),
            task_id: "t-a".into(),
            task_name: "夜间巡检".into(),
            started_at: 1_700_000_000,
            finished_at: 1_700_000_010,
            status: rustshell_core::tasks::RunStatus::Ok,
            targets: Vec::new(),
            machine: Vec::new(),
            conclusion: Some("一切正常".into()),
            findings: Vec::new(),
            note: None,
            session: Some(rustshell_core::tasks::RunSession {
                prompt: "看看磁盘还剩多少".into(),
                conclusion: "一切正常".into(),
                trace: vec![rustshell_core::agent::TraceRow::new(
                    1,
                    "c1",
                    "inspect_load",
                )],
            }),
        };

        let before = main.read_with(vcx, |m, _| m.ai_ui.ai_chats.len());
        let first = main.update(vcx, |m, _cx| m.ai_attach_run_session(&rec));
        let again = main.update(vcx, |m, _cx| m.ai_attach_run_session(&rec));
        let Some(chat_id) = first else {
            panic!("有会话痕迹的运行没有建出会话：聊天记录里会缺这次执行");
        };
        assert_eq!(
            again,
            Some(chat_id),
            "同一次运行被建成了两条会话：列表里会出现两行一模一样的记录"
        );
        let (len, title, msgs) = main.read_with(vcx, |m, _| {
            let chat = m
                .ai_ui
                .ai_chats
                .iter()
                .find(|c| c.id == chat_id)
                .expect("会话建了却没进聊天记录列表");
            (
                m.ai_ui.ai_chats.len(),
                chat.title.clone(),
                chat.msgs.clone(),
            )
        });
        assert_eq!(len, before + 1, "同一次运行建了不止一条会话");
        assert!(
            title.contains("夜间巡检"),
            "会话标题里要有任务名（同一任务会跑很多次）：{title}"
        );
        assert_eq!(msgs.len(), 2, "一条会话 = 一条用户消息 + 一条结论");
        assert!(
            msgs[0].content.contains("看看磁盘还剩多少"),
            "用户消息应当是任务里的 AI 要求原文：{}",
            msgs[0].content
        );
        assert!(
            msgs[0].content.contains("夜间巡检"),
            "用户消息要标明是哪条任务、什么时候跑的：{}",
            msgs[0].content
        );
        assert_eq!(msgs[1].content, "一切正常");
        assert_eq!(
            msgs[1].trace.len(),
            1,
            "轨迹必须挂进会话 —— 「这次到底查了什么」是会话里信息量最大的部分"
        );

        // 没有会话痕迹的运行（升级前的老记录、非 AI 动作）：不给会话，
        // 也不用空壳占位（面板会显示 `—`）。
        let plain = rustshell_core::tasks::RunRecord {
            run_id: "r-0".into(),
            session: None,
            ..rec
        };
        assert!(
            main.update(vcx, |m, _cx| m.ai_attach_run_session(&plain))
                .is_none(),
            "没有会话痕迹的运行不该凭空造一条空会话"
        );
    }

    /// 提问必须把「当前终端」画像（会话事实 + 最近一次采样）一起注入。
    ///
    /// 盯的是 **UI 侧接线**：core 的 `with_context` 有自己的单测，但界面这条路
    /// 若还按旧的两参调用、或忘了把指标塞进画像，模型收到的就只有一个孤零零的
    /// 问题 —— 编译、界面、其余单测全绿，只有真问一句「这个终端的内存是多少」才
    /// 看得出来。
    #[gpui::test]
    fn the_current_terminal_rides_along_with_the_question(cx: &mut gpui::TestAppContext) {
        use gpui::{AppContext as _, Entity};
        use rustshell_core::metrics::{MetricsStore, Sample, SystemOverview};
        use rustshell_core::session::{SessionHandle, SessionKind, SessionState};
        use std::cell::RefCell;
        use std::rc::Rc;
        use std::sync::Arc;

        use super::super::{MainView, TabEntry};

        cx.update(|cx| {
            gpui_component::init(cx);
            crate::theme::apply_theme_mode(cx, rustshell_core::theme::ThemeMode::Dark);
        });
        let slot: Rc<RefCell<Option<Entity<MainView>>>> = Rc::new(RefCell::new(None));
        let (_, vcx) = {
            let slot = slot.clone();
            cx.add_window_view(move |window, cx| {
                let main = cx.new(|cx| MainView::new(window, cx));
                *slot.borrow_mut() = Some(main.clone());
                let shell = cx.new(|_| crate::main_view::AppShell::new(main));
                gpui_component::Root::new(shell, window, cx)
            })
        };
        vcx.run_until_parked();
        let main = slot.borrow().clone().expect("MainView 未创建");

        // 真采样（远端轮询写进会话自己的存储），概况也是真的 —— 只把数值钉死。
        let store = Arc::new(MetricsStore::new());
        store.push(Sample {
            cpu_percent: 12.3,
            mem_percent: 37.5,
            mem_used: 3 * 1024 * 1024 * 1024,
            mem_total: 8 * 1024 * 1024 * 1024,
            timestamp: 1_700_000_000,
            ..Default::default()
        });
        store.set_overview(SystemOverview {
            hostname: "web-1".into(),
            ip: "10.0.0.5".into(),
            uptime_secs: 3 * 86400 + 2 * 3600 + 5 * 60,
            disks: Vec::new(),
            timestamp: 1_700_000_000,
        });

        main.update(vcx, |m, cx| {
            m.conn_store
                .profiles
                .push(rustshell_core::connections::ConnectionProfile {
                    id: "p-1".into(),
                    name: "生产 1".into(),
                    host: "10.0.0.5".into(),
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
                    forwards: Vec::new(),
                    session_log: None,
                    proxy: Default::default(),
                    auto_reconnect: None,
                });
            let mut session = SessionHandle::pending(
                "s-1".to_string(),
                SessionKind::Ssh,
                "生产 1".to_string(),
                Some("p-1".to_string()),
            );
            session.state = SessionState::Connected;
            session.metrics = Some(store);
            m.tabs_state.tabs.push(TabEntry {
                session,
                terminal: None,
                scroll: None,
                connect_error: None,
                connect_started: None,
                dead: None,
                retry: None,
            });
            m.tabs_state.active = Some("s-1".to_string());
            cx.notify();
        });
        vcx.run_until_parked();

        let wrap = |m: &MainView| {
            let snap = m.ai_terminal_snapshot();
            rustshell_core::ai::with_context("当前终端的内存是多少", snap.as_ref(), None)
        };

        let out = main.read_with(vcx, |m, _| wrap(m));
        for needle in [
            "内存 3G/8G (37.50%)",
            "CPU 12.3%",
            "主机 web-1",
            "连接 id：p-1",
            "地址：root@10.0.0.5:22",
            "状态：已连接",
        ] {
            assert!(out.contains(needle), "注入块缺了 `{needle}`：{out}");
        }
        assert!(
            out.contains("当前终端的内存是多少"),
            "注入不能把用户原话弄丢：{out}"
        );

        // 断开后不许再吹旧数字：存储里还留着上一次采样，照抄就是拿「刚才」答「现在」。
        main.update(vcx, |m, _| {
            m.tabs_state.tabs[0].session.state = SessionState::Closed
        });
        let out = main.read_with(vcx, |m, _| wrap(m));
        assert!(!out.contains("内存 3G/8G"), "已断开还在报内存：{out}");
        assert!(out.contains("状态：已断开"), "断开这个事实本身要说：{out}");

        // 没有活动标签（最后一个标签被关）：不给画像，也不许凭空编一个。
        main.update(vcx, |m, _| m.tabs_state.active = None);
        main.read_with(vcx, |m, _| {
            assert!(
                m.ai_terminal_snapshot().is_none(),
                "没有活动标签时不该有画像"
            );
        });
    }
}
