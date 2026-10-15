//! 会话布局落盘与恢复（F-9，规格 §1.9）：`<config>/rustshell/session_layout.json`。
//!
//! 两半住在一个模块里，因为它们共用同一份跨帧状态：**恢复期间一律不写快照**
//! （`LayoutState::pending` 非空即停笔）。拆成两个模块时，写快照那半看不到
//! 「此刻正在恢复」—— 而踩中它的后果是本功能最坏的一种丢数据：启动时那张
//! 「恢复上次会话？」还挂在屏上，poll 拿**当前的空布局**把待恢复的快照覆盖掉。
//!
//! 快照只记布局（池顺序 / 分屏 / 激活位），不记终端内容与连接参数：
//! 模型、`sanitize` 与读写都在 `core::session_layout`（纯函数 + 单测），
//! 这里只负责「从 UI 状态取快照」与「把快照落回 UI 状态」。

use std::path::Path;
use std::time::Instant;

use rustshell_core::session::{SessionHandle, SessionId, SessionKind, SessionState};
use rustshell_core::session_layout::{self, LayoutTab, SessionLayout};

use super::{MainView, SplitPane, TabEntry};

/// 布局两次落盘之间的最小间隔（毫秒）。
///
/// 布局变化是**用户动作驱动**的（开标签、切标签、分屏），不像 AI 正文那样
/// 每毫秒都在变，2s 足够把「连点五次标签」收成一次写；而崩溃时最多丢 2s
/// 前的那次布局 —— 代价只是少恢复一个标签。
const SAVE_MIN_GAP_MS: u64 = 2_000;

/// 生产用的快照路径；`None` = 本进程不该碰真实文件。
///
/// **测试构建下一律 `None`**：`MainView` 是几乎所有 UI 用例的脚手架，
/// 测试调度器一推时钟，`poll` 里的落盘就会真执行 —— 砸的是开发机上真实的
/// `session_layout.json`（跑一次用例就把用户的会话布局换成用例造的那份）。
/// 而且更阴：启动分支还会**读**它，于是「开局几个标签」这类断言会随开发机
/// 上次退出时的布局漂移。用例要测落盘/恢复就走收路径参数的那几个方法。
fn production_path() -> Option<std::path::PathBuf> {
    if cfg!(test) {
        None
    } else {
        Some(rustshell_platform::paths::session_layout_path())
    }
}

/// 启动时读快照：没有文件 / 读坏了 / 测试构建都当「没有快照」。
///
/// 失败路径不打断启动：布局记忆是锦上添花，为它弹一个错误框、或让窗口起不来，
/// 都远糟于「这次没恢复」。坏文件的留底在 `core::session_layout::load` 里做。
fn load_for_startup() -> Option<SessionLayout> {
    let path = production_path()?;
    let layout = session_layout::load(&path)?;
    let (layout, warnings) = session_layout::sanitize(layout);
    for w in warnings {
        log::warn!("会话布局快照已整理：{w}");
    }
    Some(layout)
}

/// 布局落盘与恢复的全部跨帧状态。
///
/// `pub(crate)` 而不是 `pub(super)`：`MainView::layout_state` 字段写的是
/// `pub(super)`，而 `MainView` 住在 `main_view` 里 —— 那一档对字段来说等于
/// `pub(crate)`，类型比字段更私有会被 `private_interfaces` 判红（-D warnings 下即编译失败）。
/// 内部各项仍按 `pub(super)` 收口：出了 `main_view` 就只能看见一个空壳。
#[derive(Default)]
pub(crate) struct LayoutState {
    /// 上次写进文件的那份快照（指纹）。
    ///
    /// 脏检测做成「当前布局 vs 这份」的比较，而不是在每个改动点打标记：
    /// 标签增删 / 切换 / 分屏有近十个入口（快捷键、右键菜单、托盘、命令窗口…），
    /// 逐个打标记必然漏一处，而漏掉的表现是「快照停在旧布局」这种只在重启后
    /// 才看得见的静默错误。放在 `poll` 里比对只有一个调用点，将来新增入口自动覆盖。
    last: Option<SessionLayout>,
    /// 有改动还没写。
    dirty: bool,
    /// 最早允许落盘的时刻（unix 毫秒；`0` = 立刻可写）。
    save_at_ms: u64,
    /// 待恢复流程；`Some` = 恢复进行中（**期间禁止写快照**）。
    pub(super) pending: Option<RestoreFlow>,
    /// 「恢复上次会话？」对话框是否在屏上（启动命令窗口据此让路）。
    pub(super) prompt_open: bool,
    /// **首帧兑现标记**：手动触发恢复时置上，真正的 `present_dialog` 落在
    /// 第一帧 `render` 里。
    ///
    /// 为什么不能就地开框：手动入口（`layout_restore_now`）可能从「非窗口上下文」
    /// 被调起，且 `present_dialog` 开头要读 `window.root::<Root>()`；一旦在
    /// `Root` 尚未装上时读它就会 panic（历史现场：`session_restore = ask` +
    /// 有布局快照 ⇒ 启动即闪退，见本模块顶部注释）。把开框推到第一帧 `render`
    /// 最稳 —— 能跑到 `render` 就说明 `Root` 已就位。
    pub(super) prompt_arm: bool,
}

/// 一次恢复的进行态。
pub(super) struct RestoreFlow {
    /// 正在恢复的快照（分屏 / 激活位的下标都相对它）。
    layout: SessionLayout,
    /// 每个槽位落位后的 session id（下标与 `layout.tabs` 对齐）。
    ///
    /// 本地终端的 id 要等后台 ConPTY 起来才知道，故是 `Option`；
    /// `None` = 该槽位已被用户关掉，收口时不再引用它。
    slot_ids: Vec<Option<SessionId>>,
    /// 需要真正重起的本地槽位（快照下标，按池顺序）。
    local_slots: Vec<usize>,
    /// 已排到 `local_slots` 的第几个。
    next_local: usize,
    /// 有一只本地 Shell 正在后台启动：串行闸门。
    ///
    /// 为什么必须串行：`local_starting` 会把并发的第二次请求直接丢掉，
    /// 并发发起 = 后一个槽位永远等不到属于它的那次 spawn。
    awaiting_local: bool,
}

impl MainView {
    // ---- 启动：读快照 + 决定要不要恢复 ----------------------------------------

    /// 启动时按设置决定怎么处理上次的布局快照。`MainView::new` 里调一次。
    ///
    /// **不收 `Window`**：本函数在 `MainView::new` 里跑，那一刻窗口的 `Root` 还没
    /// 装上，任何 `Root::read/update` 都会 panic（见 `prompt_arm` 的文档）。
    /// `Ask` 档位因此只置旗，开框延到第一帧 `render`。
    ///
    /// 三态（规格 §1.9 交互模型）：
    /// - `Always`：直接恢复，不打扰用户；
    /// - `Never`：什么都不做 —— 快照不设 `pending`，于是第一次 `poll` 就会
    ///   拿当前（空）布局把它覆盖掉，即「用户明确不要了」；
    /// - `Ask`：置待恢复 + 弹提示框。**提示期间不写快照**（`pending` 挡着），
    ///   否则用户还没选，文件里那份就已经被换成空布局了。
    pub(super) fn layout_startup(&mut self, cx: &mut gpui::Context<Self>) {
        let Some(layout) = load_for_startup() else {
            return;
        };
        self.layout_startup_with(layout, cx);
    }

    /// 三态分发的本体（**不碰文件系统**，从 `layout_startup` 单拆出来）。
    ///
    /// 为什么拆：读盘那一步在测试构建下恒为 `None`（见 `production_path`），
    /// 三个档位于是永远走不到用例里。把「拿到快照之后怎么办」做成吃参数的纯
    /// 逻辑，询问 / 自动恢复 / 不恢复才都能真跑一遍（规格 §1.9 验收点 4）。
    ///
    /// **不收 `Window`**：`Ask` 档位只置 `prompt_arm` 旗（真正的开框在第一帧
    /// `render`），`Always` 档位走 `layout_begin_restore`（也不碰窗口）。
    /// 收一个用不上的 `&mut Window` 只会诱使后来者「就地开个框试试」——
    /// 而那条路在 `MainView::new` 阶段是必炸的（见 `prompt_arm` 的文档）。
    ///
    /// **2026-09-29 用户要求「取消恢复上次会话功能」**：启动阶段**一律不再**
    /// 自动弹「恢复上次会话？」框、也不再按 `Always` 自动重建标签 —— 无论
    /// `settings.session_restore` 是三态里的哪一档，启动都当成「不打扰」处理：
    /// 快照**不设 `pending`**，于是第一次 `poll` 就会拿当前（空）布局把它覆盖掉，
    /// 与原先 `Never` 档位完全一致。
    ///
    /// 为什么连 `Always` 也一并停掉：用户的口径是「启动别自作主张恢复」，而
    /// `Always` 恰恰**最自作主张**（一声不响重建一堆标签）。三态设置本身已从
    /// 设置面板撤下（见 `settings_panel.rs` 的说明），保留 `Always` 的自动行为
    /// 只会让老用户的旧配置继续触发、且没有面板入口可关。
    ///
    /// 恢复能力**没有删**：`layout_begin_restore` / `layout_accept` /
    /// `DialogMode::RestoreSession` 全部保留，改由 [`Self::layout_restore_now`]
    /// 在用户**主动触发**时走同一条路。
    pub(super) fn layout_startup_with(
        &mut self,
        layout: SessionLayout,
        cx: &mut gpui::Context<Self>,
    ) {
        let _ = cx;
        if layout.is_empty() {
            return; // 上次退出时就没开着标签：没什么可恢复的，不必问
        }
        // 只记一笔日志说明「有快照但按用户口径不自动恢复」，不再分三态行动。
        log::info!(
            "发现会话布局快照（{} 个标签）：按「启动不自动恢复」口径跳过 —— \
             需要时由用户手动触发恢复。",
            layout.tabs.len()
        );
    }

    /// 用户**手动**触发「恢复上次会话」：读盘 → 弹确认框（与原先启动那条路同框）。
    ///
    /// 这是 2026-09-29「取消启动自动弹框」之后恢复功能的**唯一入口**。
    /// 语义与原先 `Ask` 档位一字不差：有快照才弹、弹时置 `pending`（期间禁止写
    /// 快照，免得用户还没选文件就被空布局覆盖）、真正的开框落在第一帧 `render`
    /// 由 `prompt_arm` 兑现。
    ///
    /// 与 `layout_startup` 的区别只有一处：这里是**用户按下去**的，所以不管
    /// `settings.session_restore` 是什么，一律弹框（不再看设置分档）。
    ///
    /// **不收 `cx`**：本函数只置旗（开框延到 `render`），不需要上下文也能跑。
    /// 收一个用不上的 `&mut Context` 只会诱使后来者「就地开个框试试」——
    /// 而那条路在窗口 `Root` 未就位时是必炸的（见 `prompt_arm` 文档）。
    pub(super) fn layout_restore_now(&mut self) {
        let Some(layout) = load_for_startup() else {
            log::info!("手动恢复上次会话：没有可用快照");
            return;
        };
        if layout.is_empty() {
            log::info!("手动恢复上次会话：快照里没有任何标签");
            return;
        }
        log::info!(
            "手动恢复上次会话：发现快照（{} 个标签），等待用户确认",
            layout.tabs.len()
        );
        self.tabs_state.layout_state.pending = Some(RestoreFlow::new(layout));
        self.tabs_state.layout_state.prompt_open = true;
        self.tabs_state.layout_state.prompt_arm = true;
    }

    /// 提示框正文用的两个数：`(标签数, 连接数)`；没有待恢复的快照时返 `None`。
    pub(super) fn restore_prompt_stats(&self) -> Option<(usize, usize)> {
        let flow = self.tabs_state.layout_state.pending.as_ref()?;
        let total = flow.layout.tabs.len();
        let ssh = flow
            .layout
            .tabs
            .iter()
            .filter(|t| t.profile_id.is_some())
            .count();
        Some((total, ssh))
    }

    /// 用户点了「恢复」（或按了回车）。
    ///
    /// 2026-09-29：本框改为**只由用户手动触发**，故不再有「下次不再询问」勾选框，
    /// 这里也就不再回写 `session_restore` 设置（那个字段仅作数据兼容保留）。
    pub(super) fn layout_accept(&mut self, w: &mut gpui::Window, cx: &mut gpui::Context<Self>) {
        let Some(flow) = self.tabs_state.layout_state.pending.take() else {
            return;
        };
        self.tabs_state.layout_state.prompt_open = false;
        self.close_dialog_ui(w, cx);
        self.layout_begin_restore(flow.layout, cx);
    }

    /// 用户不想恢复：快照作废，本次开空窗格。
    ///
    /// 这是个**三合一入口**：按钮「不恢复」、ESC / 点遮罩 / 右上角 X
    /// （由 `on_close` 统一兜底）都走这里 —— 对话框的每一种关闭路径都必须
    /// 解除「不许写快照」的封锁，否则那个封锁会跟到进程结束。
    ///
    /// 幂等：已经把 `pending` 交给 `layout_accept` 时是空操作（`None` 分支）。
    pub(super) fn layout_decline(&mut self) {
        let had = self.tabs_state.layout_state.pending.take().is_some();
        self.tabs_state.layout_state.prompt_open = false;
        if had {
            log::info!("用户选择不恢复上次会话：快照作废，下次落盘覆盖");
            // 立刻放开节流：不等 2s，下一次 poll 就把「作废」写成事实。
            self.tabs_state.layout_state.save_at_ms = 0;
        }
    }

    // ---- 快照：从 UI 状态取 ------------------------------------------------

    /// 当前布局的快照（池顺序 + 分屏 + 激活位）。
    ///
    /// 两件事顺手做掉：
    /// - **临时 SSH 会话不进快照**（没有 `profile_id`，重建它连「连哪台」都不知道）；
    /// - 分屏 / 激活位引用的下标跟着上面那次过滤**重排** —— 不重排的话，
    ///   池里少一个临时会话就会让分屏指向隔壁标签（比不恢复更糟的一种错）。
    pub(super) fn layout_snapshot(&self) -> SessionLayout {
        let mut tabs: Vec<LayoutTab> = Vec::with_capacity(self.tabs_state.tabs.len());
        // 池下标 → 快照下标（`None` = 这个标签不进快照）
        let mut map: Vec<Option<usize>> = vec![None; self.tabs_state.tabs.len()];
        for (ix, tab) in self.tabs_state.tabs.iter().enumerate() {
            if tab.session.kind == SessionKind::Ssh && tab.session.profile_id.is_none() {
                continue;
            }
            map[ix] = Some(tabs.len());
            tabs.push(LayoutTab {
                profile_id: tab.session.profile_id.clone(),
                title: tab.session.title.clone(),
            });
        }
        let slot_of = |id: &str| -> Option<usize> {
            let ix = self
                .tabs_state
                .tabs
                .iter()
                .position(|t| t.session.id == id)?;
            map[ix]
        };
        let mut layout = SessionLayout::new(tabs);
        layout.split = self.tabs_state.split.as_ref().and_then(|sp| {
            Some(session_layout::LayoutSplit {
                axis: sp.axis,
                primary: slot_of(&sp.primary)?,
                secondary: slot_of(&sp.secondary)?,
            })
        });
        layout.active = self.tabs_state.active.as_deref().and_then(slot_of);
        layout
    }

    // ---- 落盘 --------------------------------------------------------------

    /// 指纹脏标记 + 到点落盘（`poll` 每轮调用）。
    ///
    /// 恢复期间**只泵本地 Shell、不写盘**（见模块头：写下去就是拿当前布局
    /// 覆盖待恢复的那份）。
    pub(super) fn layout_poll(&mut self, cx: &mut gpui::Context<Self>) {
        if self.tabs_state.layout_state.pending.is_some() {
            self.layout_pump_local(cx);
            return;
        }
        let snap = self.layout_snapshot();
        if self.tabs_state.layout_state.last.as_ref() != Some(&snap) {
            self.tabs_state.layout_state.last = Some(snap);
            // 节流窗口只在「干净 → 脏」那一次定下时刻：之后布局再变也不顺延，
            // 否则连续拖动/连点标签会把落盘无限推后，崩溃时反而丢得更多。
            if !self.tabs_state.layout_state.dirty {
                self.tabs_state.layout_state.dirty = true;
                self.tabs_state.layout_state.save_at_ms = now_ms().saturating_add(SAVE_MIN_GAP_MS);
            }
        }
        self.layout_flush_if_due();
    }

    /// 到点就落盘。返回是否真的写了（调用方**不该**据此 `cx.notify()`：
    /// 写文件不改任何被渲染的东西）。
    pub(super) fn layout_flush_if_due(&mut self) -> bool {
        match production_path() {
            Some(path) => self.layout_flush(&path, false),
            None => false,
        }
    }

    /// 立即落盘（退出钩子调它）。
    ///
    /// `App::on_app_quit` 给处理器 100ms：一份布局 JSON 只有几百字节，
    /// 远在这个量级之下。没改动时是空操作 —— 退出不该为「什么都没发生」
    /// 重写一遍文件。
    pub fn layout_flush_now(&mut self) -> bool {
        match production_path() {
            Some(path) => self.layout_flush(&path, true),
            None => false,
        }
    }

    /// 落盘本体。`path` 是参数而不是写死：用例要把它导到临时目录
    /// （写开发机真实的布局文件是**破坏性**的）。
    pub(super) fn layout_flush(&mut self, path: &Path, force: bool) -> bool {
        // 恢复未收口前**绝不落盘**：此刻池里是恢复出来的占位，写下去就把
        // 「待恢复的那份快照」永久换成了「恢复了一半的这份」。
        if self.tabs_state.layout_state.pending.is_some() {
            return false;
        }
        if !self.tabs_state.layout_state.dirty {
            return false;
        }
        let now = now_ms();
        if !force && now < self.tabs_state.layout_state.save_at_ms {
            return false;
        }
        let snap = self.layout_snapshot();
        match session_layout::save(path, &snap) {
            Ok(()) => {
                self.tabs_state.layout_state.last = Some(snap);
                self.tabs_state.layout_state.dirty = false;
                true
            }
            Err(e) => {
                // 写失败（磁盘满 / 目录只读）就保持脏、下个窗口再试：
                // 静默丢掉用户的布局，比多试几次糟得多。
                self.tabs_state.layout_state.save_at_ms = now.saturating_add(SAVE_MIN_GAP_MS);
                log::warn!("会话布局落盘失败（下轮重试）: {e}");
                false
            }
        }
    }

    // ---- 恢复：把快照落回 UI 状态 -------------------------------------------

    /// 按快照把标签池摆出来。
    ///
    /// **一次把所有占位都建好**（而不是边起边插）：本地 Shell 的 ConPTY 冷启动
    /// 实测可达数十秒，期间窗口里必须已经是正确的标签布局 —— 用户此刻正看着
    /// 标签栏。顺序也因此天然正确：先占位、后「就地替换」，不需要任何插入计算。
    ///
    /// 两类占位（规格 §1.9「不自动连」）：
    /// - **SSH** → `state = Closed` 的断开占位：内容区「已断开」卡，右键「连接」
    ///   走既有 `reconnect_tab` 原地连回来。生产连接**一条都不自动发起**；
    /// - **本地** → `Connecting` 占位（显示「正在启动本地 Shell…」+ 已等待秒数）
    ///   并进 `local_slots` 排队：本地 Shell 没有「连接」这一步，按「重建 Tab」
    ///   的语义由 `layout_pump_local` 逐个真起回来。
    pub(super) fn layout_begin_restore(
        &mut self,
        layout: SessionLayout,
        cx: &mut gpui::Context<Self>,
    ) {
        let mut slot_ids: Vec<Option<SessionId>> = Vec::with_capacity(layout.tabs.len());
        let mut local_slots: Vec<usize> = Vec::new();
        for (ix, lt) in layout.tabs.iter().enumerate() {
            let local = lt.profile_id.is_none();
            let id = rustshell_core::connections::uuid_like();
            let mut session = SessionHandle::pending(
                id.clone(),
                if local {
                    SessionKind::Local
                } else {
                    SessionKind::Ssh
                },
                lt.title.clone(),
                lt.profile_id.clone(),
            );
            if !local {
                // 断开占位：`is_disconnected()` 的口径（无终端、无失败原因、
                // 无等待计时、state = Closed）。缺 `state` 这一下就落到
                // 「正在建立 SSH 连接…」，那是在骗人 —— 我们并没在连。
                session.state = SessionState::Closed;
            } else {
                local_slots.push(ix);
            }
            self.tabs_state.tabs.push(TabEntry {
                session,
                terminal: None,
                scroll: None,
                connect_error: None,
                // 本地槽位从排队那一刻就开始计时（卡片上的「已等待 N 秒」），
                // SSH 槽位不是「在等」，是「断了」——给它计时就成了假提示。
                connect_started: local.then(Instant::now),
                dead: None,
                // 恢复出来的标签**不排重连**（F-9 口径：生产连接一条都不自动发起）：
                // 断连占位要用户自己点「连接」，见 `reconnect_tab`。
                retry: None,
            });
            slot_ids.push(Some(id));
        }
        self.tabs_after_restore_focus();
        self.tabs_state.layout_state.pending = Some(RestoreFlow {
            layout,
            slot_ids,
            local_slots,
            next_local: 0,
            awaiting_local: false,
        });
        self.tabs_state.pending_focus = true;
        log::info!("开始恢复会话布局：{} 个标签", self.tabs_state.tabs.len());
        self.layout_pump_local(cx);
        cx.notify();
    }

    /// 恢复期间激活位的兜底：先把焦点落在第一个标签上（收口时再按快照纠正）。
    ///
    /// 不留 `None` 的话，恢复中途（本地 Shell 还在起）窗口是「一个标签都没激活」
    /// 的形态 —— 标签栏没有高亮、`pending_focus` 无处可去。
    fn tabs_after_restore_focus(&mut self) {
        if self.tabs_state.active.is_none() {
            self.tabs_state.active = self.tabs_state.tabs.first().map(|t| t.session.id.clone());
        }
    }

    /// 排队起下一只本地 Shell（串行）。
    ///
    /// 三道闸门，缺一个都会出问题：
    /// 1. `awaiting_local`：上一只还在后台 spawn，等它落位再排下一只；
    /// 2. `local_starting`：用户自己按了 Ctrl+T（终端入口之一），
    ///    那只优先级更高 —— 让它先跑完；
    /// 3. 槽位目标还在不在池里：用户可能已经把那个占位关掉了，此时不能凭空
    ///    造一个新标签出来（那就成了「关掉的标签自己回来了」）。
    fn layout_pump_local(&mut self, cx: &mut gpui::Context<Self>) {
        loop {
            let Some(flow) = self.tabs_state.layout_state.pending.as_ref() else {
                return;
            };
            if flow.awaiting_local || self.conn_ui.local_starting {
                return;
            }
            let Some(&slot) = flow.local_slots.get(flow.next_local) else {
                self.layout_finish(cx);
                return;
            };
            let target = flow.slot_ids.get(slot).cloned().flatten();
            // 消费一个槽位（无论起没起成），避免同一槽位被反复排队。
            if let Some(flow) = self.tabs_state.layout_state.pending.as_mut() {
                flow.next_local += 1;
            }
            let Some(target) = target else {
                continue; // 该槽位已被关掉：直接看下一个
            };
            if !self.tabs_state.tabs.iter().any(|t| t.session.id == target) {
                continue; // 同上（`slot_ids` 与池之间的第二道校验）
            }
            if let Some(flow) = self.tabs_state.layout_state.pending.as_mut() {
                flow.awaiting_local = true;
            }
            self.spawn_local_shell_into(Some(target), cx);
            return; // 一次只起一只
        }
    }

    /// 一只本地 Shell 的排队结束（**成功或失败都要调**）：放行下一只。
    ///
    /// 失败也得放行：`spawn_local_shell_into` 失败时那个槽位留在「已断开」占位上
    /// （用户可右键重试），后面排队的槽位不该被一起拖死。
    pub(super) fn layout_local_settled(&mut self, cx: &mut gpui::Context<Self>) {
        let Some(flow) = self.tabs_state.layout_state.pending.as_mut() else {
            return;
        };
        flow.awaiting_local = false;
        self.layout_pump_local(cx);
    }

    /// 全部槽位落位：按快照摆回分屏与激活位，然后解除「禁止写快照」。
    fn layout_finish(&mut self, cx: &mut gpui::Context<Self>) {
        let Some(flow) = self.tabs_state.layout_state.pending.take() else {
            return;
        };
        // 分屏：两个下标都得还指着一个**仍在池里**的槽位。恢复期间用户完全
        // 可能关掉其中一屏（本地 Shell 冷启动以十秒计），此时宁可只剩单窗格
        // 也不能指向一个已经没了的会话。
        if let Some(sp) = flow.layout.split {
            let a = flow.slot_ids.get(sp.primary).cloned().flatten();
            let b = flow.slot_ids.get(sp.secondary).cloned().flatten();
            match (a, b) {
                (Some(a), Some(b)) if a != b => {
                    let pool: Vec<&str> = self
                        .tabs_state
                        .tabs
                        .iter()
                        .map(|t| t.session.id.as_str())
                        .collect();
                    if pool.contains(&a.as_str()) && pool.contains(&b.as_str()) {
                        self.tabs_state.split = Some(SplitPane {
                            axis: sp.axis,
                            primary: a,
                            secondary: b,
                        });
                    } else {
                        log::warn!("恢复分屏失败：槽位已不在标签池里");
                    }
                }
                _ => log::warn!("恢复分屏失败：槽位已被关闭或空缺"),
            }
        }
        let restored_active = flow
            .layout
            .active
            .and_then(|ix| flow.slot_ids.get(ix).cloned().flatten())
            .filter(|id| self.tabs_state.tabs.iter().any(|t| &t.session.id == id));
        self.tabs_state.active =
            restored_active.or_else(|| self.tabs_state.tabs.first().map(|t| t.session.id.clone()));
        self.normalize_split();
        self.tabs_state.pending_focus = true;
        log::info!("会话布局恢复完成：{} 个标签", self.tabs_state.tabs.len());
        cx.notify();
    }
}

impl RestoreFlow {
    fn new(layout: SessionLayout) -> Self {
        let slot_ids = vec![None; layout.tabs.len()];
        Self {
            layout,
            slot_ids,
            local_slots: Vec::new(),
            next_local: 0,
            awaiting_local: false,
        }
    }
}

/// 挂「退出前把会话布局落盘」的钩子（`shell` 层启动时调一次）。
///
/// 为什么除了 `poll` 里的节流落盘还要它：节流最长等 2s，用户完全可能
/// 「刚拖完分屏就关窗」—— 那次布局变化还在内存里。`App::on_app_quit` 的
/// 处理器有 100ms 预算，一份布局 JSON 远小于这个量级。
///
/// 返回值就地 `detach()` 而不交给调用方：gpui 的 `Subscription` **一 drop 就退订**，
/// 调用处写成 `let _ = f(cx);` 就等于没挂上（而且不报错、不告警 —— 只有
/// 「关窗后还是丢布局」这一个现象）。把这件事封在函数里，调用方无从踩错。
pub fn ui_install_flush_session_layout_on_quit(cx: &mut gpui::App) {
    cx.on_app_quit(|cx| {
        if let Some(hub) = cx.try_global::<super::UiHub>() {
            let main = hub.main.clone();
            if main.update(cx, |m, _cx| m.layout_flush_now()).is_err() {
                log::warn!("退出前落盘会话布局未路由到 MainView（实体已释放）");
            }
        }
        async {}
    })
    .detach();
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
    use rustshell_core::session_layout::LayoutSplit;
    use rustshell_core::settings::SessionRestoreMode;
    use rustshell_core::split::SplitAxis;

    /// 用例专属的临时快照路径。
    ///
    /// 落盘本体的签名收 `&Path` 正是为此：`production_path()` 在 `cfg(test)` 下
    /// 恒为 `None` 只挡得住「poll 自己写」，挡不住用例想**测真读写**的诉求。
    /// 两者合起来才既测得到落盘，又不会砸掉开发机上那份真实的
    /// `session_layout.json`（砸了的后果是用户下次启动恢复出一堆用例造的标签）。
    fn tmp_path(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rustshell-session-layout-{}-{tag}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        dir.join("session_layout.json")
    }

    /// 只含 SSH 槽位的快照。
    ///
    /// 本地槽位不在造数据的选择之内：`layout_pump_local` 排到它就会真去起 ConPTY，
    /// 用例既等不到那次落位、也不该在 CI 上起一只真 shell。
    fn ssh_layout(n: usize) -> SessionLayout {
        SessionLayout::new(
            (0..n)
                .map(|i| LayoutTab::ssh(format!("p{i}"), format!("host{i}")))
                .collect(),
        )
    }

    fn split(axis: SplitAxis, primary: usize, secondary: usize) -> Option<LayoutSplit> {
        Some(LayoutSplit {
            axis,
            primary,
            secondary,
        })
    }

    /// 一条池里的标签（只造快照要看的三个字段，其余留空）。
    fn tab(kind: SessionKind, profile_id: Option<&str>) -> TabEntry {
        let session = SessionHandle::pending(
            rustshell_core::connections::uuid_like(),
            kind,
            profile_id.unwrap_or("临时").to_string(),
            profile_id.map(str::to_string),
        );
        TabEntry {
            session,
            terminal: None,
            scroll: None,
            connect_error: None,
            connect_started: None,
            dead: None,
            retry: None,
        }
    }

    /// 验收点 2：恢复落点。SSH 一律停在「已断开」占位，分屏与激活位按快照摆回。
    #[gpui::test]
    fn restored_ssh_tabs_land_as_disconnected_placeholders(cx: &mut gpui::TestAppContext) {
        let (main, vcx) = crate::main_view::dialogs::tests::build_main(cx);
        let mut layout = ssh_layout(3);
        layout.split = split(SplitAxis::Vertical, 0, 2);
        layout.active = Some(1);

        main.update_in(vcx, |m, _w, cx| {
            m.layout_begin_restore(layout, cx);

            assert_eq!(m.tabs_state.tabs.len(), 3, "三个槽位都要长出标签");
            for (i, t) in m.tabs_state.tabs.iter().enumerate() {
                assert!(t.terminal.is_none(), "恢复不许挂载终端");
                assert!(
                    t.is_disconnected(),
                    "标签 {i} 必须是「已断开」占位（右键「连接」的入口形态）"
                );
                assert_eq!(t.session.state, SessionState::Closed);
                assert!(
                    t.connect_started.is_none(),
                    "没在连却不计时：占位卡上的「已等待 N 秒」就在骗人"
                );
                assert_eq!(t.session.profile_id, Some(format!("p{i}")));
                assert_eq!(t.session.title, format!("host{i}"));
            }
            // 没有本地槽位 ⇒ 不必等任何异步落位，本帧就收口
            assert!(
                m.tabs_state.layout_state.pending.is_none(),
                "纯 SSH 快照应当场收口"
            );
            let sp = m.tabs_state.split.clone().expect("分屏要按快照摆回");
            assert_eq!(sp.axis, SplitAxis::Vertical);
            assert_eq!(sp.primary, m.tabs_state.tabs[0].session.id);
            assert_eq!(sp.secondary, m.tabs_state.tabs[2].session.id);
            assert_eq!(
                m.tabs_state.active.as_deref(),
                Some(m.tabs_state.tabs[1].session.id.as_str())
            );
        });
    }

    /// 验收点 5：`快照 → 恢复 → 再取快照` 必须与原快照相等，且落盘一次、去重一次。
    ///
    /// 这条等式是「快照只按下标引用分屏与激活位」的总账：下标先要能落回标签，
    /// 再要能从标签取回**同一对下标**（顺序与槽位对应关系都不能漂）。
    #[gpui::test]
    fn snapshot_survives_restore_and_flush_writes_it_once(cx: &mut gpui::TestAppContext) {
        let (main, vcx) = crate::main_view::dialogs::tests::build_main(cx);
        let mut layout = ssh_layout(2);
        layout.split = split(SplitAxis::Horizontal, 1, 0);
        layout.active = Some(0);
        let path = tmp_path("round-trip");
        let _ = std::fs::remove_file(&path);

        main.update_in(vcx, |m, _w, cx| {
            m.layout_begin_restore(layout.clone(), cx);
            assert_eq!(m.layout_snapshot(), layout, "恢复出来的布局与快照不一致");

            m.layout_poll(cx);
            assert!(m.layout_flush(&path, true), "有改动却没写");
            assert!(!m.layout_flush(&path, true), "没改动还重写了一次文件");
            let back = session_layout::load(&path).expect("刚写的文件读不回来");
            assert_eq!(back, layout, "落盘的不是这一份布局");
        });
    }

    /// 节流：脏了也不马上写，`force`（退出钩子）才越过窗口。
    #[gpui::test]
    fn dirty_layout_waits_for_the_throttle_window(cx: &mut gpui::TestAppContext) {
        let (main, vcx) = crate::main_view::dialogs::tests::build_main(cx);
        let path = tmp_path("throttle");
        let _ = std::fs::remove_file(&path);

        main.update_in(vcx, |m, _w, cx| {
            m.tabs_state.tabs.push(tab(SessionKind::Ssh, Some("p9")));
            m.layout_poll(cx);
            assert!(
                !m.layout_flush(&path, false),
                "2s 窗口内的轮次不该落盘：连点五次标签不该写五次"
            );
            assert!(!path.exists(), "节流窗口内不该有文件");
            assert!(m.layout_flush(&path, true), "退出钩子必须能越过窗口");
        });
    }

    /// 2026-09-29「取消恢复上次会话功能」：**启动阶段不自动恢复、不自动弹框**，
    /// 且与 `session_restore` 三档**无关**（一律当「不打扰」处理）。
    ///
    /// 原先这里是 `Ask` 三态各自的验收点；现在只有一个行为，故合并成一条：
    /// 启动拿到快照后既不长标签、也不置 `pending`/`prompt_arm`（于是第一轮 poll
    /// 就会拿当前布局覆盖旧快照，与旧 `Never` 档位一致）。
    #[gpui::test]
    fn startup_never_auto_restores_nor_prompts(cx: &mut gpui::TestAppContext) {
        let (main, vcx) = crate::main_view::dialogs::tests::build_main(cx);
        for mode in [
            SessionRestoreMode::Ask,
            SessionRestoreMode::Always,
            SessionRestoreMode::Never,
        ] {
            main.update_in(vcx, |m, _w, cx| {
                m.settings.session_restore = mode;
                m.layout_startup_with(ssh_layout(4), cx);
                assert!(
                    m.tabs_state.tabs.is_empty(),
                    "启动不自动恢复：不该凭空长出标签（mode={mode:?}）"
                );
                assert!(
                    m.tabs_state.layout_state.pending.is_none(),
                    "启动不该留下待恢复状态（mode={mode:?}）—— 否则会挡住落盘"
                );
                assert!(
                    !m.tabs_state.layout_state.prompt_open && !m.tabs_state.layout_state.prompt_arm,
                    "启动绝不自动弹恢复框（mode={mode:?}）"
                );
                assert!(m.dialog.is_none(), "启动阶段不许开任何框");
            });
        }
    }

    /// 手动触发（`layout_restore_now`）才置 `pending` + `prompt_arm`，并且
    /// **未答复期间一次都不许写** —— 这是本功能唯一会丢用户数据的一条线：
    /// 不挡的话，框还开着、`poll` 已经拿当前的空标签池把待恢复的快照覆盖掉了。
    ///
    /// 手动入口的落盘闸与原先 `Ask` 档位共用同一套 `pending` 判据，故这条仍然
    /// 有效（只是触发点从「启动」改成「用户手动」）。
    #[gpui::test]
    fn an_unanswered_prompt_blocks_every_write(cx: &mut gpui::TestAppContext) {
        let (main, vcx) = crate::main_view::dialogs::tests::build_main(cx);
        let path = tmp_path("blocked");
        let _ = std::fs::remove_file(&path);

        main.update_in(vcx, |m, _w, _cx| {
            // 手动入口的语义：把一份快照直接塞进「待恢复」态（等价于用户点了菜单）。
            m.tabs_state.layout_state.pending = Some(RestoreFlow::new(ssh_layout(4)));
            m.tabs_state.layout_state.prompt_open = true;
            m.tabs_state.layout_state.prompt_arm = true;

            assert!(
                m.tabs_state.layout_state.pending.is_some(),
                "要有待恢复的快照"
            );
            assert!(m.tabs_state.layout_state.prompt_open, "提示框要在屏上");
            assert!(
                m.tabs_state.layout_state.prompt_arm,
                "Ask 必须置 `prompt_arm`：开框延到第一帧 render（`MainView::new` 里\n\
                 `Root` 还没装上，就地开框 = 启动即 panic）"
            );
            assert!(
                m.dialog.is_none(),
                "`MainView::new` 阶段不许就地开框：那一刻 `Root` 还没装上，\n\
                 `present_dialog` 的 `has_active_dialog` 会 panic。开框走 `prompt_arm`。"
            );
            assert_eq!(m.restore_prompt_stats(), Some((4, 4)));
            assert!(
                m.tabs_state.tabs.is_empty(),
                "用户还没点头就不许凭空长出标签"
            );

            // 「已经脏了 + 提示还挂着」这一种才真会丢数据：`layout_poll` 开头的
            // 早退挡得住 poll 自己写，挡不住**退出钩子**那条路（`layout_flush_now`
            // 不经 poll）。这里直接打脏标记而不走 poll —— poll 在 pending 期间根本
            // 不打标记，绕开它才能量到落盘本体那道闸（变异验证：删掉 `pending`
            // 判据，本用例会红）。
            m.tabs_state.tabs.push(tab(SessionKind::Ssh, Some("px")));
            m.tabs_state.layout_state.dirty = true;
            assert!(
                !m.layout_flush(&path, true),
                "待恢复期间必须挡住落盘，连 `force`（退出钩子）也不行"
            );
            assert!(!path.exists(), "提示期间写出了文件");

            // 答「不恢复」：封锁解除，脏掉的那次改动落下去 —— 写的是**当前**布局，
            // 不是那份被作废的 4 标签快照（那才是「不恢复」四个字的实际后果）。
            m.layout_decline();
            assert!(m.tabs_state.layout_state.pending.is_none());
            assert!(!m.tabs_state.layout_state.prompt_open);
            assert!(m.layout_flush(&path, true), "答完之后该把新布局写下去");
            let back = session_layout::load(&path).expect("作废之后没有落盘");
            assert_eq!(back.tabs.len(), 1, "落盘的该是当前那一个标签");
            assert_eq!(back.tabs[0].profile_id.as_deref(), Some("px"));
            assert_eq!(back, m.layout_snapshot(), "落盘的不是当前布局");
        });
    }

    /// 验收点 4（`Always`）：**新口径下这一档已不再自动恢复**。
    ///
    /// `Always` 曾经表示「启动当场把标签摆回来」，现在启动路径统一为「只记账、
    /// 不动作」—— 三档（`Always` / `Never` / `Ask`）在启动时行为一致。本用例把
    /// 这条口径锁死：即便用户设置里存着 `Always`，启动也不许长出标签。
    #[gpui::test]
    fn always_mode_no_longer_restores_on_startup(cx: &mut gpui::TestAppContext) {
        let (main, vcx) = crate::main_view::dialogs::tests::build_main(cx);
        main.update_in(vcx, |m, _w, cx| {
            m.settings.session_restore = SessionRestoreMode::Always;
            m.layout_startup_with(ssh_layout(3), cx);
            assert!(
                m.tabs_state.tabs.is_empty(),
                "启动不再自动恢复：`Always` 也不许当场摆标签"
            );
            assert!(!m.tabs_state.layout_state.prompt_open, "启动不该开提示框");
            assert!(!m.tabs_state.layout_state.prompt_arm, "启动不该置开框标记");
            assert!(m.dialog.is_none(), "启动不该弹任何东西");

            // 恢复能力本身还在：手动入口可在任意时刻调用，且**不 panic**。
            // （这里落盘目录没有快照，所以 `pending` 保持为空 —— 入口要能优雅空转，
            // 不能像早期版本那样对着 `None` 直接开框。）
            m.layout_restore_now();
            assert!(
                m.tabs_state.layout_state.pending.is_none(),
                "没有快照时手动恢复要空转，不许留下半截状态"
            );
            assert!(m.dialog.is_none(), "没有快照时手动恢复不该弹空框");
        });
    }

    /// 验收点 4（`Never`）：启动什么都不做，但下一次落盘会把旧快照盖掉。
    ///
    /// 「不恢复」必须是**有后果的**选择：留着那份快照等下次再问，等于用户表过态
    /// 之后又被自己的设置推翻。
    #[gpui::test]
    fn never_mode_leaves_the_window_empty_and_overwrites_the_snapshot(
        cx: &mut gpui::TestAppContext,
    ) {
        let (main, vcx) = crate::main_view::dialogs::tests::build_main(cx);
        let path = tmp_path("never");
        let _ = std::fs::remove_file(&path);

        main.update_in(vcx, |m, _w, cx| {
            m.settings.session_restore = SessionRestoreMode::Never;
            m.layout_startup_with(ssh_layout(2), cx);
            assert!(m.tabs_state.tabs.is_empty(), "不恢复就不该有标签");
            assert!(
                m.tabs_state.layout_state.pending.is_none(),
                "不恢复不该留下待恢复状态"
            );
            assert!(!m.tabs_state.layout_state.prompt_open);

            m.layout_poll(cx);
            assert!(m.layout_flush(&path, true), "旧快照要被当前布局覆盖");
        });
    }

    /// 临时 SSH 会话（快速连接来的）不进快照，分屏与激活位的下标跟着重排。
    ///
    /// 重排是这条判据的全部难点：池里少一个标签，快照下标就整体前移，不换算的话
    /// 分屏会指向**隔壁**那个标签 —— 比不恢复更糟的一种错法。
    #[gpui::test]
    fn adhoc_ssh_tabs_are_dropped_and_indices_remap(cx: &mut gpui::TestAppContext) {
        let (main, vcx) = crate::main_view::dialogs::tests::build_main(cx);
        main.update_in(vcx, |m, _w, _cx| {
            m.tabs_state.tabs.push(tab(SessionKind::Ssh, Some("pa")));
            m.tabs_state.tabs.push(tab(SessionKind::Ssh, None)); // 临时会话：不进快照
            m.tabs_state.tabs.push(tab(SessionKind::Ssh, Some("pb")));
            m.tabs_state.active = Some(m.tabs_state.tabs[2].session.id.clone());

            let snap = m.layout_snapshot();
            assert_eq!(snap.tabs.len(), 2, "临时 SSH 不进快照");
            assert_eq!(snap.active, Some(1), "激活位要按前移后的下标算");

            // 分屏两侧都可进快照 ⇒ 只重排
            m.tabs_state.split = Some(SplitPane {
                axis: SplitAxis::Vertical,
                primary: m.tabs_state.tabs[0].session.id.clone(),
                secondary: m.tabs_state.tabs[2].session.id.clone(),
            });
            let sp = m.layout_snapshot().split.expect("两侧都留得下，分屏该留下");
            assert_eq!((sp.primary, sp.secondary), (0, 1));

            // 有一侧是临时会话 ⇒ 整条分屏作废，但**一个标签都不丢**
            m.tabs_state.split = Some(SplitPane {
                axis: SplitAxis::Vertical,
                primary: m.tabs_state.tabs[1].session.id.clone(),
                secondary: m.tabs_state.tabs[2].session.id.clone(),
            });
            let snap = m.layout_snapshot();
            assert!(snap.split.is_none(), "引用不成立就整条分屏取消");
            assert_eq!(snap.tabs.len(), 2, "分屏只是排布，标签才是用户的会话");

            // 本地终端没有 profile_id，却**要**进快照（它可重建）
            m.tabs_state.tabs.push(tab(SessionKind::Local, None));
            let snap = m.layout_snapshot();
            assert_eq!(snap.tabs.len(), 3);
            assert_eq!(snap.tabs[2].profile_id, None);
        });
    }

    /// 验收点 3（源码守卫）：恢复这条路径上一句「发起连接」都不许有。
    ///
    /// 规格 §1.9 的「生产连接默认不自动发起」是**安全口径**而不只是体验偏好
    /// （连上生产机就可能把上次留在 shell 里的东西跑一遍），所以它不能只靠人
    /// review：有人在恢复流程里接一句 `connect_profile`，这条用例就红。
    #[test]
    fn the_restore_path_never_starts_a_connection() {
        let code = crate::test_support::prod(include_str!("layout_persist.rs"));
        for needle in [
            "connect_in_background",
            "connect_profile",
            "reconnect_tab(",
            "open_local_shell(",
        ] {
            assert!(
                !code.contains(needle),
                "恢复路径里出现了 `{needle}`：生产连接只能由用户点「连接」发起"
            );
        }
        // 反向信号：本地 Shell 走的是「放回自己那个槽位」，不是菜单入口
        assert!(
            code.contains("spawn_local_shell_into("),
            "恢复必须经 `spawn_local_shell_into` 把 Shell 落回**它自己那个槽位**"
        );
    }

    /// 退出钩子的订阅不能被丢掉（gpui 的 `Subscription` 一 drop 就退订，且不报错）。
    #[test]
    fn the_quit_hook_keeps_its_subscription() {
        let code = crate::test_support::prod(include_str!("layout_persist.rs"));
        let start = code
            .find("pub fn ui_install_flush_session_layout_on_quit")
            .expect("退出钩子必须存在");
        let body = &code[start..];
        assert!(
            body.contains(".detach()"),
            "on_app_quit 的返回值必须就地 detach()，否则钩子根本没挂上"
        );
        assert!(
            body.contains("layout_flush_now()"),
            "退出前要走 force 落盘，不能等 2s 节流"
        );
    }

    /// 2026-09-29 用户要求「取消恢复上次会话功能」：
    /// **启动阶段一律不自动弹框、不自动恢复**。
    ///
    /// 历史背景（仍值得记住）：`MainView::new` 跑在 `cx.open_window(..)` 的
    /// 建窗闭包里、官方 `gpui_component::Root` 成为窗口根视图**之前**。那一刻
    /// `present_dialog` 开头的 `has_active_dialog` 会去读 `Root`，读到 `None` 就
    /// panic —— 表现正是**启动闪退**。所以「启动段不许开框」这条铁律没变；
    /// 只是现在启动段连「置旗延后开框」也不做了。
    ///
    /// 本用例把两件事钉死：
    /// 1. 启动分发 `layout_startup_with` **不得**置 `prompt_arm`/`prompt_open`、
    ///    也不得调 `layout_begin_restore`（即不自动弹框、不自动重建标签）；
    /// 2. 手动入口 `layout_restore_now` 必须**真的**置 `prompt_arm`（否则恢复
    ///    功能就没入口了），且开框仍延到 `render` 由 `prompt_arm` 兑现。
    ///
    /// 为什么必须源码级：读盘那一步在 `cfg(test)` 下恒为 `None`
    /// （见 `production_path`），任何在测试里跑 `layout_startup` 的用例都到不了
    /// 真读盘的分支，行为对不对根本测不出来。
    #[test]
    fn the_startup_prompt_is_deferred_out_of_the_window_build_closure() {
        let code = crate::test_support::prod(include_str!("layout_persist.rs"));
        let at = code
            .find("pub(super) fn layout_startup_with(")
            .expect("`layout_startup_with` 不见了");
        // 到下一个函数定义为止（`layout_restore_now`）
        let end = code[at..]
            .find("pub(super) fn layout_restore_now(")
            .expect("找不到 `layout_startup_with` 的结束锚点");
        let body = &code[at..at + end];

        assert!(
            !body.contains("present_dialog("),
            "`layout_startup_with` 里不许开框：它在 `MainView::new` 阶段被调，\n\
             那一刻窗口根视图还不是官方 `Root`，`present_dialog` 的\n\
             `has_active_dialog` 一读 `Root` 就 panic（现场：启动即闪退）。"
        );
        assert!(
            !body.contains("prompt_arm = true"),
            "启动分发里又置了 `prompt_arm`（等于启动自动弹恢复框）：\n\
             2026-09-29 用户要求启动不自动恢复。"
        );
        assert!(
            !body.contains("prompt_open = true"),
            "启动分发里又置了 `prompt_open`：启动不应出现恢复框的语义。"
        );
        assert!(
            !body.contains("layout_begin_restore("),
            "启动分发里又调了 `layout_begin_restore`：用户要求启动不自动重建标签。"
        );

        // 手动入口必须真的置旗（否则恢复功能失去唯一入口）。
        let manual_at = code
            .find("pub(super) fn layout_restore_now(")
            .expect("`layout_restore_now` 不见了（手动恢复入口）");
        let manual = &code[manual_at..];
        let manual_body = &manual[..manual.len().min(2000)];
        assert!(
            manual_body.contains("prompt_arm = true"),
            "手动恢复入口 `layout_restore_now` 必须置 `prompt_arm`，否则恢复框弹不出来。"
        );
        assert!(
            manual_body.contains("prompt_open = true"),
            "`prompt_open` 与 `prompt_arm` 要一起置：前者管「命令窗口让路」的语义，\n\
             后者管「什么时候真开框」。少一个都会出现半残状态。"
        );

        // 另一半：`render` 里真的把它兑现了。两处各自成立才算闭环。
        let render = crate::test_support::prod(include_str!("../main_view.rs"));
        assert!(
            render.contains("if self.tabs_state.layout_state.prompt_arm {"),
            "`render` 里没有 `prompt_arm` 的消费点：旗置了没人兑现，\n\
             恢复询问框就再也不弹了。"
        );
        assert!(
            render.contains("self.present_dialog(DialogMode::RestoreSession, window, cx);"),
            "`render` 里必须真的调 `present_dialog(DialogMode::RestoreSession, ..)`。"
        );
        // 启动段不许再收 `Window`：收了就等于给「就地开框」留了门路。
        assert!(
            !code.contains("fn layout_startup(&mut self, w: &mut gpui::Window"),
            "`layout_startup` 不该收 `&mut Window`：收了口子就会有人再在启动段开框。"
        );
    }
}
