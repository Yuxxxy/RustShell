//! 标签与会话窗格的**领域模型**：一个标签（会话）在应用层的完整状态，以及
//! 分屏态下「两个窗格各自显示哪个标签」的表达。
//!
//! 2026-10-12（开源评审 P0-4 刀二）从 `main_view.rs` 本体搬出。
//!
//! 为什么它们该有自己的家：这一组类型是**纯数据 + 纯判定**，没有一个字段是
//! GPUI 的东西 —— `TabEntry` 描述「这条会话现在什么状态」，`SplitPane` /
//! `PaneSide` 描述「分屏时谁在左边」，`SessionEnd` + `classify_session_end`
//! 是「会话结束时该关标签还是留现场」的唯一判定点。
//!
//! 把它们和 `MainView` 的 new / poll / render 放在一起，会让人误以为这些状态
//! 依赖视图；实际上 `TabEntry` 可以脱离 GPUI 单测（`is_connecting` /
//! `is_disconnected` / `is_live` / `is_dead` / `is_reconnecting` /
//! `retry_progress` 全是纯函数方法）。

use gpui::Entity;
use rustshell_core::connections::ConnectionStore;
use rustshell_core::session::{SessionHandle, SessionId, SessionKind, SessionState};
use rustshell_core::split::SplitAxis;

/// 分屏态：两个窗格各自显示池中的一个标签。
/// 不变量：两槽位都存在、互不相同；`MainView::active` 始终等于焦点窗格的标签。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitPane {
    pub axis: SplitAxis,
    /// 左（Vertical）/ 上（Horizontal）窗格的当前标签
    pub primary: SessionId,
    /// 右（Vertical）/ 下（Horizontal）窗格的当前标签
    pub secondary: SessionId,
}

/// 窗格标识（决定「当前标签」与新建标签的落点）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneSide {
    Primary,
    Secondary,
}

pub struct TabEntry {
    pub session: SessionHandle,
    /// 会话尚未建立（连接中）时为`None`；连接成功后填充。
    pub terminal: Option<Entity<rustshell_terminal::TerminalView>>,
    /// 终端滚动条句柄（T-2）：与`terminal`同生命周期；`None`= 尚无终端。
    pub scroll: Option<crate::terminal_scrollbar::TermScrollHandle>,
    /// 连接失败原因（超时 / 拒绝 / 认证失败...）。
    pub connect_error: Option<String>,
    /// 连接发起时刻：用于「已等待 N 秒」的实时提示（None = 非连接中）
    pub connect_started: Option<std::time::Instant>,
    /// **意外断连的现场**：`Some(原因)` = 断连态。
    ///
    /// 与 [`TabEntry::is_disconnected`]（用户主动「断开」、终端视图已被卸下）是
    /// 两回事：断连态下终端视图**仍然挂着**，画面、滚动、划选复制全部保留，
    /// 只把输入封死并在窗格底部叠一条断连条。
    ///
    /// 这是「掉线不许跳页面」的落点：远端注销 / 网络中断时标签不关、不弹命令窗口、
    /// 不显示「连接失败」占位卡 —— 用户看到的是**他自己那一屏输出**加一句
    /// 「连接已断开」，而不是一张陌生的空页面。
    pub dead: Option<String>,
    /// **断线重连的账本**（F-4）：`Some` = 这条掉线的标签正在走一套重试计划。
    ///
    /// 为什么挂在标签上，而不是像旧实现那样按 `profile_id` 记一张 `HashMap`：
    /// ① 同一个 profile 开两个标签时，两边会互相偷对方的次数（一边连上了就把
    ///    另一边的账也清了）；② 关标签时那个计数没人清 —— 同一个 profile 重开
    ///    一次，次数接着上次用掉的算。挂标签上这两件事都不成立：账本的生命周期
    ///    就是标签的生命周期。
    pub retry: Option<RetryLedger>,
}

/// 一次掉线的重连账本（F-4）。
///
/// 三个字段构成一台小状态机：`due_at = Some(t)` =「等到 t 再试」，
/// `due_at = None` =「这一轮已经发出去了，等结果」。
///
/// `max` 是**掉线那一刻**从设置里取的快照：一套重试计划开始就按同一套规矩走完，
/// 中途改设置不该让断连条上的「第 3/3 次」忽然变成「第 3/7 次」。反过来，
/// 「开 / 关」这一档每次判定都读**实时**值 —— 那个是停止键，必须立刻生效。
#[derive(Debug, Clone, Copy)]
pub struct RetryLedger {
    /// 已发出的重试次数
    pub attempts: u32,
    /// 本次掉线的重试上限（掉线那一刻的快照）
    pub max: u32,
    /// 下一次重试的到期时刻；`None` = 这一轮已在飞（或还没排过）
    pub due_at: Option<std::time::Instant>,
}

/// 断连条上「重连进展」那一段（F-4）：第几次 / 上限 / 是否在等退避。
///
/// 单独成结构体而不是元组：断连条那句整话要把三件事编成两种句式
/// （「正在重连…（第 2/3 次）」/「等待重试（第 2/3 次）」），
/// 元组里的两个 `u32` 与一个 `bool` 读起来分不清谁是谁。
pub struct DeadBarRetry {
    /// 第几次（1 起）：等待时指的是**将要**发的那一次
    pub attempt: u32,
    /// 本次掉线的上限
    pub max: u32,
    /// 两次尝试之间等退避（`false` = 这一轮正在连）
    pub waiting: bool,
}

/// 会话走到尽头时，UI 该怎么收场。
///
/// 这是「掉线不许跳页面」这条规则的**唯一判定点**，刻意抽成纯函数：
/// 它藏在 `poll` 的事件循环里时没法直接单测，而它一旦被判错，症状是
/// 「远端注销 = 标签凭空消失 + 冒出命令窗口」这种只在实机才看得见的跳变。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionEnd {
    /// 关掉标签（用户主动关、或本地 shell 跑完）。
    CloseTab,
    /// **原地转断连态**：终端画面留着、输入封死、底部叠一条断连条。
    KeepScene,
}

/// `SessionEvent::Closed` 到达时的归类。
///
/// `state` 不再是 `Connected` ⟹ 是我们自己先置的 `Closed`（关标签 / 主动断开
/// 时终端被析构，输出泵在收尾时会照样补发一个 `Closed`）→ 照旧关。
///
/// `state` 仍是 `Connected` ⟹ 没人动过它，是对端/进程把会话终止了：
/// - **SSH**：远端注销、空闲超时（服务端 `TMOUT`）、网络中断。**留现场** ——
///   用户那一屏输出是判断「为什么断」的唯一证据，换成一页「连接失败」占位卡
///   等于把证据删了，观感就是「跳到一张新页面」。
/// - **本地 Shell**：用户敲了 `exit` / 进程自己结束。关标签是各家的通行预期
///   （Windows Terminal 同款），这里不留现场。
pub fn classify_session_end(state: SessionState, kind: SessionKind) -> SessionEnd {
    if state != SessionState::Connected {
        return SessionEnd::CloseTab;
    }
    match kind {
        SessionKind::Ssh => SessionEnd::KeepScene,
        SessionKind::Local => SessionEnd::CloseTab,
    }
}

impl TabEntry {
    /// 是否处于「占位连接中」状态（尚无真实会话，且未失败）
    pub fn is_connecting(&self) -> bool {
        self.session.state == SessionState::Connecting && self.connect_error.is_none()
    }

    /// 是否处于「已断开但保留占位」状态（N25 右键菜单「断开」）。
    pub fn is_disconnected(&self) -> bool {
        // 断连态不算：那一种有终端画面，判定与渲染都走另一条路
        self.dead.is_none()
            && self.terminal.is_none()
            && self.connect_error.is_none()
            && self.connect_started.is_none()
            && self.session.state == SessionState::Closed
    }

    /// 会话此刻是否**可用**（有终端且未断连）。
    ///
    /// 状态点、快捷命令目标筛选、右键菜单的「断开 / 连接」二选一都问这个 ——
    /// `terminal.is_some()` 单独用会把断连态误判成「连着」，于是菜单给的
    /// 动作会是「断开」（点了没反应）而不是「重新连接」。
    pub fn is_live(&self) -> bool {
        self.terminal.is_some() && self.dead.is_none()
    }

    /// 是否处于「意外断连、保留画面」态。
    pub fn is_dead(&self) -> bool {
        self.dead.is_some()
    }

    /// 单通道 SFTP 会话（外部 `sftp://`）：**已连接但没有终端**。
    ///
    /// 它与「连接中的占位」「已断开的占位」共用 `terminal = None`，靠
    /// `state == Connected && sftp_tx.is_some()` 区分 —— 状态点、占位卡文案
    /// 与右键菜单都要认这一种，否则它会被画成「正在连」。
    pub fn is_sftp_live(&self) -> bool {
        self.terminal.is_none()
            && self.session.state == SessionState::Connected
            && self.session.sftp_tx.is_some()
    }

    /// 是否「好好连着」—— 标签栏的状态点与分屏窗格标题条**共用这一个判据**。
    ///
    /// 把「终端可用」与「SFTP 标签可用」合在一处，是因为两处显示同一件事：用户看到
    /// 的都是一条能干活、连着绿色点的会话。2026-10-30 用户反馈「sftp 拆分栏的小绿灯
    /// 和 tab 不一致」：窗格标题只问了 `is_live()`（要有终端），SFTP 标签没有终端，
    /// 于是同一张标签在栏里是绿点、在窗格标题上是灰点。
    pub fn is_healthy(&self) -> bool {
        self.is_live() || self.is_sftp_live()
    }

    /// 断连后正在原地重连（画面仍留在断连现场上）。
    pub fn is_reconnecting(&self) -> bool {
        self.dead.is_some() && self.session.state == SessionState::Connecting
    }

    /// 是否在等一次连接结果（占位连接中，或断连现场上正在重连）。
    ///
    /// 与 [`TabEntry::is_connecting`] 的差别：那个要求「还没有失败原因」—— 那是给
    /// **渲染**用的（有失败原因就该显示失败占位，而不是「连接中…」）。这里问的是
    /// 「连接结果该落回哪个标签」，因此必须把「断连现场上正在重连」也算进来：
    /// 它同样在等，而且**带着上一次的失败原因**（断连条还要显示那句话）。
    /// 漏掉它的后果是重连成功时新开一个标签，旧那个断连现场变成孤儿。
    pub fn awaits_connect_result(&self) -> bool {
        self.session.state == SessionState::Connecting
            && (self.connect_error.is_none() || self.dead.is_some())
    }

    /// 断连重连的展示态：`None` = 没有重连计划（没开 / 已用尽 / 用户接管）。
    pub fn retry_progress(&self) -> Option<DeadBarRetry> {
        let r = self.retry.as_ref()?;
        let waiting = r.due_at.is_some();
        Some(DeadBarRetry {
            // 等待中显示的是**将要**发的那一次，正在连时显示的就是当前这一次
            attempt: if waiting { r.attempts + 1 } else { r.attempts },
            max: r.max,
            waiting,
        })
    }

    /// 一次重连尝试失败后推进账本：预算还有就排下一次（返回等待秒数），
    /// 用尽就清账（返回 `None`）。
    ///
    /// 用尽时**清账**而不是留一个「已用尽」的终态：尾巴上那句「重连失败：…」
    /// 由 `connect_error` 负责说，两处各说各的反而容易打架 —— 而且清账之后
    /// 断连条就只剩「重新连接 / 关闭标签」，不再给用户「它还会自己再试」的错觉。
    ///
    /// `choice` / `global` 取**实时**值（见 [`RetryLedger`] 的说明），`max` 用快照。
    pub fn advance_retry_after_failure(
        &mut self,
        choice: Option<bool>,
        global: bool,
        interval_secs: u32,
    ) -> Option<u64> {
        let r = self.retry.as_ref()?;
        if !rustshell_core::reconnect::should_retry(choice, global, r.attempts, r.max) {
            self.retry = None;
            return None;
        }
        let wait = rustshell_core::reconnect::backoff_secs(interval_secs, r.attempts + 1);
        if let Some(r) = self.retry.as_mut() {
            r.due_at = Some(std::time::Instant::now() + std::time::Duration::from_secs(wait));
        }
        Some(wait)
    }

    /// 内容区可用的终端视图（连接成功前为 None）
    pub fn terminal(&self) -> Option<&Entity<rustshell_terminal::TerminalView>> {
        self.terminal.as_ref()
    }
}

/// 把到点的重连账本推进一格，返回该发起的那些标签 id（F-4）。
///
/// 抽成「传入 `now`」的自由函数而不是 `MainView` 的方法，只有一个理由：
/// 「什么时候该试下一次」这条判定必须能被单测直接钉住，而不是只能靠真时钟
/// 等上 5 秒（或者没人验）。推进账本的副作用在这里，发起连接留给调用方。
///
/// `attempts` 在**点火之前**推进、`due_at` 同一刻清空：此后这次尝试的成败都由
/// 连接结果那条路收口，这里不会重复点火（`due_at = None` 就是「已在飞」的标志）。
pub(crate) fn take_due_retries(tabs: &mut [TabEntry], now: std::time::Instant) -> Vec<String> {
    let mut due = Vec::new();
    for tab in tabs.iter_mut() {
        let Some(r) = tab.retry.as_mut() else {
            continue;
        };
        let Some(at) = r.due_at else {
            continue;
        };
        if at <= now {
            r.due_at = None;
            r.attempts += 1;
            due.push(tab.session.id.clone());
        }
    }
    due
}

/// 标签**展示**用的标题：快照里指向的连接配置已删除时，就地补一句说明。
///
/// 为什么只在渲染时加、绝不写回 `session.title`：`session.title` 是下次快照的
/// 数据源，写进去等于每重启一次就多套一层后缀（「（连接配置已删除）（连接配置
/// 已删除）…」），而且用户后来新建了同名连接也洗不回来。放在渲染侧则是一次
/// 纯派生 —— 配置删了才显示，补回来自动消失。
///
/// 已连上的标签不加后缀：那一刻配置是存在的，人已在这台机器上干活，标题突然
/// 多个「已删除」只会让人以为连接断了。
pub(crate) fn display_tab_title(tab: &TabEntry, conn_store: &ConnectionStore) -> String {
    let base = tab.session.title.clone();
    // 外部链接的临时连接本来就不在库里 —— 不能因此说它「配置已删除」。
    let external = tab.session.profile_id.as_deref().is_some_and(|pid| {
        pid.starts_with(rustshell_core::launch::EXTERNAL_PROFILE_PREFIX)
    });
    let gone = !external
        && tab.terminal.is_none()
        && tab
            .session
            .profile_id
            .as_deref()
            .is_some_and(|pid| conn_store.profile(pid).is_none());
    let mut title = if gone && !base.trim().is_empty() {
        format!("{base}{}", rustshell_core::i18n::t("（连接配置已删除）"))
    } else {
        base
    };
    // 自动重连（F-4）：标签栏上也要看得出「这条正自己往回连」—— 用户多半正看着
    // 别的标签，光靠窗格底部的断连条等于让他盯着一个不相关的页面。
    //
    // 只在**这一轮真的在连**时加后缀：退避等待期间（最长 60 秒）说「重连中」
    // 是假话，而那句等待有断连条负责说。
    if tab.is_reconnecting() {
        if let Some(p) = tab.retry_progress() {
            if !p.waiting {
                title.push_str(&rustshell_core::i18n::ts(
                    format!("（重连中 {}/{}）", p.attempt, p.max),
                    format!(" (reconnecting {}/{})", p.attempt, p.max),
                ));
            }
        }
    }
    // 类型标记（2026-10-30 用户要求「tab 页增加标记或名称区分 ssh 还是 sftp」）：
    // SFTP 标签连上后与终端标签**长得一样**（都只有名字 + 状态点），而两件事在该
    // 标签里的操作完全不同。用文字后缀而不是图标 —— 2026-09-17 用户明确要求
    // 「tab 标题去掉图标」（`tab_bar::tests::tab_titles_carry_no_kind_icon` 盯着）。
    //
    // 只给 SFTP 打标：SSH 是默认形态，人人都知道「没标记的那条是终端」；给两边
    // 都加后缀会把本来就不宽的标签再吃掉一截。
    if tab.is_sftp_live() {
        title.push_str(&rustshell_core::i18n::ts(
            "（SFTP）".to_string(),
            " (SFTP)".to_string(),
        ));
    }
    title
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一个「看起来像那么回事」的标签：SFTP 版 = 无终端 + 已连上 + 有 sftp 通道。
    fn tab(title: &str, sftp: bool) -> TabEntry {
        let mut session = rustshell_core::session::SessionHandle::pending(
            format!("id-{title}"),
            rustshell_core::session::SessionKind::Ssh,
            title.to_string(),
            Some("p1".to_string()),
        );
        session.state = rustshell_core::session::SessionState::Connected;
        if sftp {
            let (tx, _rx) = flume::bounded(1);
            session.sftp_tx = Some(tx);
        }
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

    /// 2026-10-30 用户要求「tab 页可以增加标记或名称区分 ssh 还是 sftp」。
    ///
    /// 用**文字后缀**而不是图标：2026-09-17 用户明确要求过「tab 标题去掉图标」，
    /// `tab_bar::tests::tab_titles_carry_no_kind_icon` 正盯着那条。
    ///
    /// 只给 SFTP 打标 —— SSH 是默认形态，给两边都加后缀会把本来就不宽的标签再吃掉一截。
    #[test]
    fn an_sftp_tab_says_so_in_its_title() {
        let store = rustshell_core::connections::ConnectionStore::default();

        let sftp = display_tab_title(&tab("web-01", true), &store);
        assert!(
            sftp.ends_with("（SFTP）") || sftp.ends_with(" (SFTP)"),
            "SFTP 标签的标题没有类型标记：{sftp}"
        );

        // 反例：同样「没有终端」但没有 sftp 通道（终端标签在建连时就是这个形状），
        // 一个字都不该加。
        let plain = display_tab_title(&tab("web-01", false), &store);
        assert!(
            !plain.contains("SFTP"),
            "终端标签不该带 SFTP 标记（SSH 是默认形态）：{plain}"
        );
    }
}
