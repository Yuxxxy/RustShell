//! 端口转发面板的动作（F-3）：规则增删改、单条隧道启停、表单落库。
//!
//! 与 `task_actions` 同一分工：面板只读快照、只发动作，所有状态改写都在这里。
//!
//! # 两处状态，各归各的
//!
//! * **规则**存在 `ConnectionProfile::forwards`（跟着连接走）—— 「这条隧道属于
//!   哪台机器」是连接属性，换一台机器同一条规则毫无意义。
//! * **运行状态**存在会话的 [`ForwardBoard`](rustshell_core::forward::ForwardBoard)
//!   上 —— 会话一关，板和监听一起没了。所以「面板里显示什么」= 库里的规则
//!   ⋈ 活动会话上的运行态；没有活动会话的规则照样列出来，只是状态是「未启动」。
//!
//! # 为什么 Start 要找「拥有这个 profile 的活动会话」
//!
//! 隧道是 SSH 信道的副产物，没有连接就没有隧道可建。同一个 profile 开了两个
//! 标签时取**第一个**活着并且有转发板（`SshChannels::Full`）的会话：两者共享
//! 同一台机器、同一条网络路径，挑哪条对结果没有影响，而「最新那个」会随用户
//! 开关标签而变，面板上的启停就会指向一个搬过家的会话。

use gpui::{App, Context, SharedString, Window};
use rustshell_core::connections::{default_listen_host, ForwardKind, ForwardRule};
use rustshell_core::forward::{validate_error_text, ForwardStatus};
use rustshell_core::i18n::t;
use rustshell_core::session::{SessionKind, SessionState};

use super::domain::DialogMode;
use super::MainView;
use crate::notice::NoticeKind;

/// 「所属连接」下拉里的一项：显示「名称（主机）」，落库的值是 profile id。
///
/// 为什么必须自定义 item，而不是像组件库示例那样用 `&'static str`：
/// ① id 是运行期字符串；② 下拉里读到的文本（名称 + 主机）与要落库的值（id）
/// 不是同一份东西 —— 拿名字当值，改个名归属就断了。
#[derive(Clone)]
pub struct ProfileItem {
    pub id: String,
    pub label: String,
}

impl gpui_kit::component::searchable_list::SearchableListItem for ProfileItem {
    type Value = String;

    fn title(&self) -> SharedString {
        SharedString::from(self.label.clone())
    }

    fn value(&self) -> &Self::Value {
        &self.id
    }
}

/// 面板一行的只读快照（渲染期先复制成纯值，免得与 `cx.listener` 的可变借用打架）。
#[derive(Clone)]
pub struct ForwardRow {
    pub profile_id: String,
    /// 连接名（规则属于哪个连接 —— 同一台机器可能有两条不同认证的连接）
    pub profile_name: String,
    /// SSH 主机地址（列表第二行的起点）
    pub host: String,
    pub rule: ForwardRule,
    /// 运行状态；`None` = 这条规则没在跑（含「该连接没有活动会话」）
    pub status: Option<ForwardStatus>,
    /// 该连接当前有可启停的转发板（= 有一个已连上的 Full 通道 SSH 会话）
    pub can_start: bool,
}

impl ForwardRow {
    /// 是否正在监听（状态点取绿的那一档）。
    pub fn is_active(&self) -> bool {
        matches!(self.status, Some(ForwardStatus::Active))
    }

    /// 这一行现在该显示「停止」还是「启动」。
    ///
    /// `Starting` 也走「停止」：监听马上就会起来，此时给「启动」等于让用户
    /// 对着一条正在建的隧道再点一次。
    pub fn is_running(&self) -> bool {
        matches!(
            self.status,
            Some(ForwardStatus::Active) | Some(ForwardStatus::Starting)
        )
    }

    /// 状态文案（i18n）。
    pub fn status_label(&self) -> String {
        match &self.status {
            Some(ForwardStatus::Active) => t("已启动").to_string(),
            Some(ForwardStatus::Starting) => t("启动中").to_string(),
            Some(ForwardStatus::Failed(msg)) => format!("{}: {msg}", t("失败")),
            Some(ForwardStatus::Stopped) | None => t("未启动").to_string(),
        }
    }
}

impl MainView {
    /// 找到拥有该 profile、且带转发板的活动 SSH 会话。
    fn live_forward_board(
        &self,
        profile_id: &str,
    ) -> Option<std::sync::Arc<rustshell_core::forward::ForwardBoard>> {
        self.tabs_state
            .tabs
            .iter()
            .filter(|tab| tab.session.profile_id.as_deref() == Some(profile_id))
            .find(|tab| {
                tab.session.kind == SessionKind::Ssh
                    && tab.session.state == SessionState::Connected
                    && tab.dead.is_none()
                    && tab.session.forwards.is_some()
            })
            .and_then(|tab| tab.session.forwards.clone())
    }

    /// 面板要显示的全部行：**所有连接的所有规则**（没有活动会话的也在列表里）。
    pub fn forward_rows(&self) -> Vec<ForwardRow> {
        let mut out = Vec::new();
        for profile in &self.conn_store.profiles {
            if profile.forwards.is_empty() {
                continue;
            }
            let board = self.live_forward_board(&profile.id);
            for rule in &profile.forwards {
                out.push(ForwardRow {
                    profile_id: profile.id.clone(),
                    profile_name: profile.name.clone(),
                    host: profile.host.clone(),
                    rule: rule.clone(),
                    status: board.as_ref().and_then(|b| {
                        b.find(&rule.id).map(|slot| slot.status()).filter(|s| {
                            // `Stopped` 的槽位已经不该在板上（stop 会摘掉）；
                            // 万一还在（老版本留下的），按「没在跑」显示。
                            !matches!(s, ForwardStatus::Stopped)
                        })
                    }),
                    can_start: board.is_some(),
                });
            }
        }
        out
    }

    /// 面板状态指纹：规则集合（id）与每条运行状态的稳定键（`ForwardStatus::key`，
    /// 失败原因也算进去 —— 两条不同的失败原因要各刷新一次）。
    ///
    /// 用 FNV-1a 而不是 `std::hash::DefaultHasher`：后者不保证跨版本/跨平台
    /// 稳定，而这里的用途是「与上一拍比是否相同」，同一进程内稳定就够了 ——
    /// 但既然只要几十行，用固定常数更不用担心实现换掉。
    pub fn forwards_signature(&self) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        let mut mix = |bytes: &[u8]| {
            for b in bytes {
                h ^= *b as u64;
                h = h.wrapping_mul(0x0000_0100_0000_01b3);
            }
        };
        for row in self.forward_rows() {
            mix(row.rule.id.as_bytes());
            mix(&[row.can_start as u8, row.is_running() as u8]);
            match &row.status {
                Some(st) => {
                    mix(st.key().as_bytes());
                    if let ForwardStatus::Failed(msg) = st {
                        mix(msg.as_bytes());
                    }
                }
                None => mix(b"none"),
            }
        }
        h
    }

    /// 当前库里这条规则（找不到 = 刚被删掉）。
    fn forward_rule(&self, profile_id: &str, rule_id: &str) -> Option<ForwardRule> {
        self.conn_store
            .profiles
            .iter()
            .find(|p| p.id == profile_id)?
            .forwards
            .iter()
            .find(|r| r.id == rule_id)
            .cloned()
    }

    /// 打开规则表单：`rule_id` 为 `None` 时是新建。
    pub fn open_forward_form(
        &mut self,
        profile_id: &str,
        rule_id: Option<&str>,
        w: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let profile_id = if profile_id.is_empty() {
            // 面板的 `+` 不指定连接：优先「当前活动标签所属的连接」，
            // 其次是库里第一条 —— 没有连接时给提示，别把表单开在一个假 id 上。
            match self
                .tabs_state
                .active
                .clone()
                .and_then(|id| self.tabs_state.tabs.iter().find(|t| t.session.id == id))
                .and_then(|tab| tab.session.profile_id.clone())
                .or_else(|| self.conn_store.profiles.first().map(|p| p.id.clone()))
            {
                Some(id) => id,
                None => {
                    self.queue_notice(NoticeKind::Warning, t("还没有连接，先建一条连接再配转发"));
                    cx.notify();
                    return;
                }
            }
        } else {
            profile_id.to_string()
        };

        let existing = rule_id.and_then(|id| self.forward_rule(&profile_id, id));
        let kind = existing
            .as_ref()
            .map(|r| r.kind)
            .unwrap_or(ForwardKind::Local);
        self.io.fwd_form.profile_id = profile_id.clone();
        self.io.fwd_form.editing_id = existing.as_ref().map(|r| r.id.clone());
        self.io.fwd_form.kind = kind;
        self.io.fwd_form.error = None;

        // 回填：只在编辑时；新建一律空（含端口 —— 留着上一条的数字最容易误存）。
        // 监听地址是例外：新建也要给初值（127.0.0.1），否则框里空着，
        // 用户以为「不填就不监听」。
        let (name, listen_host, listen, thost, tport) = match &existing {
            Some(r) => (
                r.name.clone(),
                r.listen_host.clone(),
                if r.listen_port == 0 {
                    String::new()
                } else {
                    r.listen_port.to_string()
                },
                r.target_host.clone().unwrap_or_default(),
                r.target_port.map(|p| p.to_string()).unwrap_or_default(),
            ),
            None => (
                String::new(),
                default_listen_host(),
                String::new(),
                String::new(),
                String::new(),
            ),
        };
        self.io
            .fwd_form
            .name
            .update(cx, |s, cx| s.set_value(name, w, cx));
        self.io
            .fwd_form
            .listen_host
            .update(cx, |s, cx| s.set_value(listen_host, w, cx));
        self.io
            .fwd_form
            .listen_port
            .update(cx, |s, cx| s.set_value(listen, w, cx));
        self.io
            .fwd_form
            .target_host
            .update(cx, |s, cx| s.set_value(thost, w, cx));
        self.io
            .fwd_form
            .target_port
            .update(cx, |s, cx| s.set_value(tport, w, cx));

        // 「所属连接」下拉：每次打开都按**当前连接库**重建（新增/删除连接后不必重启）。
        // 选中项由 profile id 反查下标 —— 下拉里显示的是「名称（主机）」，
        // 而落库的值是 id，两者不是一回事。
        let items: Vec<ProfileItem> = self
            .conn_store
            .profiles
            .iter()
            .map(|p| ProfileItem {
                id: p.id.clone(),
                label: format!("{}（{}）", p.name, p.host),
            })
            .collect();
        let selected = items.iter().position(|it| it.id == profile_id);
        self.io.fwd_form.conn_select.update(cx, |state, cx| {
            state.set_items(
                gpui_kit::component::searchable_list::SearchableVec::new(items),
                w,
                cx,
            );
            state.set_selected_index(selected.map(gpui_kit::component::IndexPath::new), w, cx);
        });

        self.present_dialog(
            DialogMode::ForwardForm {
                profile_id,
                rule_id: existing.map(|r| r.id),
            },
            w,
            cx,
        );
    }

    /// 表单保存：写回 `ConnectionProfile::forwards` 并落盘。
    ///
    /// 校验失败**不关窗**（原因写进 `io.fwd_form.error`）—— 关掉之后用户
    /// 要重新开一次表单才能改那个端口，而错在哪就得靠记忆。
    pub fn forward_save_form(&mut self, w: &mut Window, cx: &mut Context<Self>) {
        let Some(DialogMode::ForwardForm {
            profile_id,
            rule_id,
        }) = self.dialog.clone()
        else {
            return;
        };
        let draft = &self.io.fwd_form;
        let kind = draft.kind;
        let name = draft.name.read(cx).value().trim().to_string();
        let listen_host_raw = draft.listen_host.read(cx).value().trim().to_string();
        let listen_raw = draft.listen_port.read(cx).value().trim().to_string();
        let thost = draft.target_host.read(cx).value().trim().to_string();
        let tport_raw = draft.target_port.read(cx).value().trim().to_string();
        // 「所属连接」以下拉的当前选中为准（`profile_id` 只是打开表单时的初值）——
        // 不读它的话，用户在表单里换了连接，规则会落回原来那台机器。
        let target_pid = draft
            .conn_select
            .read(cx)
            .selected_value()
            .cloned()
            .unwrap_or_else(|| profile_id.clone());

        let mut rule = ForwardRule::new(kind);
        rule.id = rule_id.clone().unwrap_or_default();
        rule.name = name;
        // 监听地址取自输入框（默认 127.0.0.1）；**留空回退回环**而不是拒绝保存 ——
        // 空地址在 `validate` 里是 `NoListenHost`，但「没填」与「填了非法地址」
        // 对用户是两件事：前者按默认值补上更省事。
        rule.listen_host = if listen_host_raw.is_empty() {
            default_listen_host()
        } else {
            listen_host_raw
        };
        rule.listen_port = listen_raw.parse::<u16>().unwrap_or(0);
        if kind.needs_target() {
            rule.target_host = Some(thost);
            rule.target_port = tport_raw.parse::<u16>().ok();
        }
        rule.normalize();

        if let Some(err) = rule.validate() {
            self.io.fwd_form.error = Some(validate_error_text(err).to_string());
            cx.notify();
            return;
        }

        let Some(profile) = self
            .conn_store
            .profiles
            .iter_mut()
            .find(|p| p.id == target_pid)
        else {
            self.io.fwd_form.error = Some(t("连接已不存在").to_string());
            cx.notify();
            return;
        };
        let replaced = match rule_id.as_deref() {
            Some(id) => {
                if let Some(slot) = profile.forwards.iter_mut().find(|r| r.id == id) {
                    *slot = rule.clone();
                    true
                } else {
                    false
                }
            }
            None => false,
        };
        if !replaced {
            profile.forwards.push(rule.clone());
        }

        if let Err(e) = self
            .conn_store
            .save(&rustshell_platform::paths::connections_path())
        {
            self.io.fwd_form.error = Some(format!("{}: {e}", t("保存失败")));
            cx.notify();
            return;
        }

        // 规则改过而隧道正在跑：停掉旧监听。用户改完端口/目标后期待的是
        // 「新的生效」，而不是「旧的继续按老配置搬字节，新的点一次才起」。
        if replaced {
            if let Some(board) = self.live_forward_board(&target_pid) {
                board.stop(&rule.id);
            }
        }

        log::info!("端口转发规则已保存: {:?}", rule.summary());
        self.close_dialog_ui(w, cx);
        cx.notify();
    }

    /// 启动一条规则（没有活动会话时给提示，不做任何事）。
    pub fn forward_start(&mut self, profile_id: &str, rule_id: &str, cx: &mut Context<Self>) {
        let Some(rule) = self.forward_rule(profile_id, rule_id) else {
            return;
        };
        let Some(board) = self.live_forward_board(profile_id) else {
            self.queue_notice(NoticeKind::Warning, t("该连接还没有连上，先连上再启动隧道"));
            cx.notify();
            return;
        };
        let slot = board.start(&rule);
        log::info!("端口转发已请求启动: {}", slot.rule.summary());
        // 状态要等任务真正 bind 完才变；面板每帧读一次快照，立刻重绘一次即可。
        cx.notify();
    }

    /// 停掉一条规则（幂等：没在跑时什么都不做）。
    pub fn forward_stop(&mut self, profile_id: &str, rule_id: &str, cx: &mut Context<Self>) {
        if let Some(board) = self.live_forward_board(profile_id) {
            board.stop(rule_id);
        }
        cx.notify();
    }

    /// 停掉这台机器上的全部隧道。
    pub fn forward_stop_all(&mut self, profile_id: &str, cx: &mut Context<Self>) {
        if let Some(board) = self.live_forward_board(profile_id) {
            board.stop_all();
        }
        cx.notify();
    }

    /// 删除一条规则：先从隧道板上摘掉（否则监听会带着一条已经不存在的规则继续跑），
    /// 再从库里删并落盘。
    pub fn forward_remove(&mut self, profile_id: &str, rule_id: &str, cx: &mut Context<Self>) {
        if let Some(board) = self.live_forward_board(profile_id) {
            board.stop(rule_id);
        }
        let Some(profile) = self
            .conn_store
            .profiles
            .iter_mut()
            .find(|p| p.id == profile_id)
        else {
            return;
        };
        let before = profile.forwards.len();
        profile.forwards.retain(|r| r.id != rule_id);
        if profile.forwards.len() == before {
            return;
        }
        if let Err(e) = self
            .conn_store
            .save(&rustshell_platform::paths::connections_path())
        {
            self.queue_notice(NoticeKind::Error, format!("{}: {e}", t("保存失败")));
        }
        cx.notify();
    }

    /// 规则表单的对话框正文（`DialogMode::ForwardForm`）。
    ///
    /// 「所属连接」与「类型」都用**循环按钮**而不是下拉：官方 `Dropdown` 是受控
    /// 组件（要额外维护展开态与选中下标），而这两项各自只有四五种取值 ——
    /// 一枚按钮点到底比「点开、找、再点」少两步，且不会挡住表单。
    pub(super) fn render_forward_dialog(
        &mut self,
        dialog: gpui_kit::component::dialog::Dialog,
        _profile_id: &str,
        rule_id: Option<&str>,
        _w: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui_kit::component::dialog::Dialog {
        use gpui::{div, px, FontWeight, InteractiveElement as _, ParentElement as _, Styled as _};
        use gpui_kit::component::{
            button::{Button, ButtonVariants as _},
            dialog::DialogButtonProps,
            h_flex,
            input::{Input, InputState},
            radio::Radio,
            select::Select,
            v_flex, Sizable as _,
        };

        let th = crate::theme::current(cx);
        let editing = rule_id.is_some();
        let kind = self.io.fwd_form.kind;
        let err = self.io.fwd_form.error.clone();
        let muted = crate::theme::to_hsla(th.fg_muted);
        // 输入框走 Small 档（`h_6` = 1.5rem），与任务 / 快捷命令 / 连接对话框
        // 同一行高；默认的 Medium 是 `h_8` = 2rem，在 420px 宽的模态里
        // 每只框高出 7.5px，一屏六只框就显得「框比字大」。
        let field = |label: &'static str, input: &gpui::Entity<InputState>, required: bool| {
            v_flex()
                .gap_1()
                .flex_1()
                .min_w_0()
                // 标签与星号走 dialogs 里那一份（必填星号必须是同一个红色、同一个字号）
                .child(super::dialogs::field_label(t(label), required, th))
                .child(Input::new(input).small().w_full())
        };

        // 固定宽度的字段列（端口这类短输入）：`PORT_W` 与新增连接窗口的「端口」同宽。
        let field_w =
            |label: &'static str, input: &gpui::Entity<InputState>, required: bool, w: f32| {
                v_flex()
                    .gap_1()
                    .w(px(w))
                    .flex_none()
                    .child(super::dialogs::field_label(t(label), required, th))
                    .child(Input::new(input).small().w_full())
            };

        let header = div()
            .text_size(px(14.))
            .font_weight(FontWeight::BOLD)
            .child(t(if editing {
                "编辑端口转发"
            } else {
                "新建端口转发"
            }));

        // 量取钩子：单选框 / 下拉都是**首次**出现在本仓的表单里，用例靠它证明
        // 「点开表单不 panic，且真的长出了内容」。
        let mut body = v_flex()
            .gap_3()
            .w_full()
            .debug_selector(|| "fwd-form-body".to_string());
        // ① 所属连接：**可筛选**下拉（点开后顶部有输入框，打字即过滤）。
        //    连接一多，原先那种「点一下换一台」的循环按钮要按十几次才轮到目标。
        body = body.child(
            v_flex()
                .gap_1()
                .w_full()
                .child(super::dialogs::field_label(t("所属连接"), true, th))
                .child(
                    Select::new(&self.io.fwd_form.conn_select)
                        .small()
                        .placeholder(t("选择连接"))
                        .w_full(),
                ),
        );
        // ② 转发类型：**单独一行单选框**（2026-11-20 用户要求，此前是一枚循环按钮）。
        body = body.child(
            v_flex()
                .gap_1()
                .w_full()
                .child(super::dialogs::field_label(t("类型"), false, th))
                .child(
                    h_flex()
                        .gap_4()
                        .flex_wrap()
                        .children(ForwardKind::ALL.map(|k| {
                            Radio::new(kind_radio_id(k))
                                .xsmall()
                                .label(kind_label(k))
                                .checked(kind == k)
                                .on_click(cx.listener(move |m, _checked, _w, cx| {
                                    if m.io.fwd_form.kind != k {
                                        m.io.fwd_form.kind = k;
                                        // 换类型后目标两段可能出现/消失：清掉上一次的错误，
                                        // 否则「缺少目标地址」会挂在一个不需要目标的类型上。
                                        m.io.fwd_form.error = None;
                                        cx.notify();
                                    }
                                }))
                        })),
                ),
        );
        // ③ 监听地址 + 端口。远端转发的监听点在**服务器**上，标签跟着换口径 ——
        //    同一个框在两种语义下都叫「监听地址」会让人以为绑的是本机网卡。
        let listen_label = if kind == ForwardKind::Remote {
            t("服务器监听地址")
        } else {
            t("本地监听 IP")
        };
        body = body.child(
            h_flex()
                .gap_2()
                .w_full()
                .child(field(listen_label, &self.io.fwd_form.listen_host, true))
                .child(field_w(
                    "监听端口",
                    &self.io.fwd_form.listen_port,
                    true,
                    PORT_W,
                )),
        );
        // ④ 目标地址 + 目标端口：端口宽度与新增连接窗口里的「端口」一致（96px），
        //    地址占掉剩下的宽度 —— 两个框各占一半会让端口框显得过宽、地址框不够敲。
        if kind.needs_target() {
            body = body.child(
                h_flex()
                    .gap_2()
                    .w_full()
                    .child(field("目标地址", &self.io.fwd_form.target_host, true))
                    .child(field_w(
                        "目标端口",
                        &self.io.fwd_form.target_port,
                        true,
                        PORT_W,
                    )),
            );
        } else {
            body = body.child(
                div()
                    .text_size(px(10.5))
                    .text_color(muted)
                    .child(t("目标由每个连接自己给出，不需要在这里填")),
            );
        }
        // ⑤ 规则名（可空）：可选项放最后，前面的必填项先读完。
        body = body.child(field("规则名称（可空）", &self.io.fwd_form.name, false));
        if let Some(e) = err {
            body = body.child(crate::notice::NoticeKind::Error.card("fwd-form-error", e));
        }

        let footer = h_flex()
            .items_center()
            .justify_end()
            .gap_2()
            .pt_1()
            .child(
                Button::new("fwd-form-cancel")
                    .small()
                    .label(t("取消"))
                    .on_click(cx.listener(|m, _e, w, cx| {
                        m.close_dialog_ui(w, cx);
                    })),
            )
            .child(
                Button::new("fwd-form-save")
                    .small()
                    .primary()
                    .label(t("保存"))
                    .on_click(cx.listener(|m, _e, w, cx| {
                        m.forward_save_form(w, cx);
                    })),
            );

        let view = cx.entity();
        let view_ok = view.clone();
        dialog
            .w(px(420.))
            .bg(crate::theme::to_hsla(th.bg_overlay))
            .title(header)
            .child(body)
            .footer(footer)
            .button_props(
                DialogButtonProps::default()
                    .on_ok(move |_, w, cx| {
                        // Enter = 保存。校验失败时业务侧保持对话框打开，故返回 false。
                        view_ok.update(cx, |m, cx| m.forward_save_form(w, cx));
                        false
                    })
                    .on_cancel(move |_, w, cx| {
                        view.update(cx, |m, cx| m.close_dialog_ui(w, cx));
                        true
                    }),
            )
    }
}

/// 端口类输入框的宽度（px）：与新增连接窗口里「端口」那一格同宽
/// （那里写的是 `field_w(.., 96.)`）—— 两个表单的短输入对齐看起来是一套。
const PORT_W: f32 = 96.;

/// 转发类型单选框的稳定 id（`Radio::new` 只收 `&'static str`）。
///
/// 用固定字面量而不是 `format!`：`Radio` 的 id 进了 gpui 的缓动/焦点表，
/// 每帧换一个字符串等于每帧换一个新元素。
fn kind_radio_id(kind: ForwardKind) -> &'static str {
    match kind {
        ForwardKind::Local => "fwd-kind-local",
        ForwardKind::Remote => "fwd-kind-remote",
        ForwardKind::Dynamic => "fwd-kind-socks5",
        ForwardKind::HttpConnect => "fwd-kind-http",
    }
}

/// 规则类型的显示名（面板 / 表单共用）。
pub fn kind_label(kind: ForwardKind) -> &'static str {
    match kind {
        ForwardKind::Local => t("本地"),
        ForwardKind::Remote => t("远端"),
        ForwardKind::Dynamic => t("SOCKS5"),
        ForwardKind::HttpConnect => t("HTTP CONNECT"),
    }
}

// ── 右键菜单 / 行内按钮用的入口（没有 `&mut MainView`，只能走 UiHub）────────
/// 启动一条隧道。
pub fn ui_call_forward_start(profile_id: String, rule_id: String, cx: &mut App) {
    let Some(hub) = cx.try_global::<super::UiHub>() else {
        return;
    };
    let main = hub.main.clone();
    let _ = main.update(cx, |m, cx| m.forward_start(&profile_id, &rule_id, cx));
}

/// 停止一条隧道。
pub fn ui_call_forward_stop(profile_id: String, rule_id: String, cx: &mut App) {
    let Some(hub) = cx.try_global::<super::UiHub>() else {
        return;
    };
    let main = hub.main.clone();
    let _ = main.update(cx, |m, cx| m.forward_stop(&profile_id, &rule_id, cx));
}

/// 删除一条规则。
pub fn ui_call_forward_remove(profile_id: String, rule_id: String, cx: &mut App) {
    let Some(hub) = cx.try_global::<super::UiHub>() else {
        return;
    };
    let main = hub.main.clone();
    let _ = main.update(cx, |m, cx| m.forward_remove(&profile_id, &rule_id, cx));
}

/// 打开规则表单（`rule_id` 为空 = 新建）。
pub fn ui_call_forward_form(
    profile_id: String,
    rule_id: Option<String>,
    w: &mut Window,
    cx: &mut App,
) {
    let Some(hub) = cx.try_global::<super::UiHub>() else {
        return;
    };
    let main = hub.main.clone();
    let _ = main.update(cx, |m, cx| {
        m.open_forward_form(&profile_id, rule_id.as_deref(), w, cx)
    });
}

#[cfg(test)]
mod tests {
    use crate::test_support::prod;

    fn code() -> String {
        prod(include_str!("forward_actions.rs"))
    }

    /// 保存链路三件套缺一不可：写回 profile、落盘、把校验失败留给表单显示。
    ///
    /// 只改内存不落盘的后果是「配了一堆规则，重启全没了」，而这类 bug 在开发
    /// 机上永远看不出来（进程一直是活的）。
    #[test]
    fn saving_a_rule_writes_the_profile_and_flushes_to_disk() {
        let c = code();
        for needle in [
            "profile.forwards.push(rule.clone())",
            // 只钉「落盘调用的两个标识符」而不钉整行：rustfmt 会把这条链折成多行，
            // 钉整行会在某次格式化后凭空变红（2026-11-19 实测）。
            "connections_path()",
            "fwd_form.error",
        ] {
            assert!(c.contains(needle), "规则保存链路缺了 `{needle}`");
        }
        assert!(
            c.contains(".conn_store") && c.contains(".save("),
            "保存规则必须落盘（`conn_store.save(..)`）—— 只改内存的话重启就丢"
        );
    }

    /// 转发表单的输入框必须与其它表单同档（Small）。
    ///
    /// 2026-11-19 用户点名这一扇与「新增连接」：它们原先都是默认 Medium，
    /// 比任务 / 快捷命令对话框里的框高 7.5px。
    #[test]
    fn the_form_inputs_share_the_same_size_as_other_dialogs() {
        let c = code();
        assert!(
            c.contains("Input::new(input).small().w_full()"),
            "转发表单的输入框必须走 Small 档"
        );
        assert!(
            !c.contains("Input::new(input).w_full()"),
            "转发表单的输入框又回到默认 Medium 档了（比其它表单高 7.5px）"
        );
    }

    /// 监听地址：**默认回环，但可改**（2026-11-20 用户要求把框显示出来）。
    ///
    /// 2026-11-19 那版是反过来的 —— 表单不给地址、保存时钉回 127.0.0.1。现在用户
    /// 要能绑别的网卡（`0.0.0.0` 暴露到局域网、或某块内网卡）；留空仍回退回环，
    /// 而不是报「监听地址不能为空」—— 「没填」与「填了非法地址」是两件事。
    #[test]
    fn the_form_defaults_the_listen_host_to_loopback_but_lets_it_be_edited() {
        let c = code();
        assert!(
            c.contains("default_listen_host()"),
            "新建时给初值、留空时回退，都要走 `default_listen_host()`（127.0.0.1）"
        );
        // 钉**变量名**而不是调用链：rustfmt 会把 `draft.listen_host.read(cx)`
        // 折成三行，钉整行下次格式化就假红（2026-11-19 已踩过一次）。
        assert!(
            c.contains("listen_host_raw"),
            "保存时必须读监听地址框 —— 不读的话用户改的地址会被静默丢弃"
        );
        assert!(
            c.contains("\"本地监听 IP\""),
            "表单必须有「本地监听 IP」这一格（用户点名要显示出来）"
        );
        assert!(
            c.contains("\"服务器监听地址\""),
            "远端转发的监听点在服务器上，标签要换成「服务器监听地址」"
        );
    }

    /// 类型 = 一整行单选框，所属连接 = 可筛选下拉（2026-11-20 用户要求）。
    ///
    /// 两条都是替换掉原先那两枚「点一下换一个」的循环按钮 —— 顺手钉住
    /// 「循环按钮不许回来」：它一旦回来，连接一多就要点十几次。
    #[test]
    fn the_form_uses_radio_kinds_and_a_searchable_connection_select() {
        let c = code();
        assert!(
            c.contains("Radio::new(kind_radio_id("),
            "转发类型必须渲染成单选框（逐个 `Radio`），而不是一枚循环按钮"
        );
        assert!(
            !c.contains("forward_cycle_kind") && !c.contains("forward_cycle_profile"),
            "循环按钮的动作又回来了 —— 类型改单选、连接改下拉之后它们应当已删除"
        );
        assert!(
            c.contains("Select::new(&self.io.fwd_form.conn_select)"),
            "所属连接必须走 `Select` 下拉"
        );
        assert!(
            c.contains("selected_value()"),
            "保存时必须以下拉的选中值为准，否则用户在表单里换了连接也不生效"
        );
        // 可筛选是 **state** 上的开关（`Select` 元素本身没有 `searchable`）
        let mv = prod(include_str!("../main_view.rs"));
        assert!(
            mv.contains(".searchable(true)"),
            "下拉必须在 `SelectState` 建的时候开 `searchable(true)`，否则点开没有筛选框"
        );
    }

    /// 连接建立时**不再**自动启动全部规则（2026-11-19 改为显式启动）。
    ///
    /// 旧行为让用户仅仅因为「连上了」就在后台多出若干本地端口，而界面上没有
    /// 任何一处提示它们在监听 —— 这与「隧道只在你点了 Start 之后才存在」的
    /// 心智模型直接冲突。
    #[test]
    fn connecting_does_not_autostart_every_rule() {
        let ssh = prod(include_str!("../../../core/src/ssh.rs"));
        assert!(
            ssh.contains("ForwardBoard::new"),
            "连接时必须为会话建一块转发板子（运行期启停的前提）"
        );
        assert!(
            !ssh.contains("spawn_forwards("),
            "连接时不许再把规则表一次性启动 —— 转发改为在面板里显式 Start"
        );
    }

    /// 面板行为必须能在活动栏里找到入口（新面板不许只存在于代码里）。
    #[test]
    fn the_panel_is_wired_into_the_sidebar_and_the_rail() {
        let shell = prod(include_str!("app_shell.rs"));
        assert!(
            shell.contains("PanelKind::Forwards"),
            "`render_sidebar_panel` 没有分派到端口转发面板"
        );
        let rail = prod(include_str!("../activity_bar.rs"));
        assert!(
            rail.contains("PanelKind::Forwards"),
            "活动栏没有端口转发图标（面板将没有任何入口）"
        );
    }
}
