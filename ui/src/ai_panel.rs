//! AI 对话面板：会话式消息流 + 输入条 + 思考模式开关。
//!
//! 2026-09-18 运行反馈「点击 AI 助手，聊天界面显示在终端右侧」：本文件从左侧栏
//! 搬到**终端右侧那一列**（宿主是 `MainView::ai_chat_panel`，见 `AiChatPanelView`），
//! 左侧栏同一位置改放**聊天记录列表**（`ai_history_panel`）。
//!
//! 版式对齐 gpui-kit 的 `MessageScroller` 演示（对话帧 + 尾部跟随的虚拟列表 +
//! 底部输入条）：助手消息是通栏的无底「幽灵」气泡，用户消息是右对齐的浅底气泡，
//! 消息区最后一行与输入条之间的间距由输入条自身承担。
//!
//! 圆角由外层卡片壳（`MainView::render` 的 `card_shell`）统一提供，面板本身
//! 不自带底色与描边。

use gpui::{
    div, prelude::FluentBuilder as _, px, AnyElement, App, Context, ElementId, Entity,
    InteractiveElement as _, IntoElement, ParentElement as _, Role, SharedString,
    StatefulInteractiveElement as _, StyleRefinement, Styled as _,
};
use gpui_component::{
    bubble::{Bubble, BubbleContent, BubbleVariant},
    button::{Button, ButtonCustomVariant, ButtonVariant, ButtonVariants as _},
    h_flex,
    input::Textarea,
    marker::{Marker, MarkerContent},
    menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem},
    message::{Message, MessageAlignment, MessageContent, MessageFooter},
    message_scroller::MessageScroller,
    scroll::ScrollableElement as _,
    spinner::Spinner,
    text::TextView,
    v_flex, ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _,
};

/// 全量 Lucide 图标目录。
///
/// `gpui_component::IconName` 只是**组件默认子集**（约 90 个，清单见
/// gpui-kit-assets 的 `default-icons.txt`）—— `Trash` / `Brain` / `MessageSquare`
/// 根本不在那个枚举里，写 `IconName::Trash` 是**编译错误**而不是「图标空白」。
/// 全量枚举与 `IconNamed` 实现由 gpui-kit-assets 提供（见根 Cargo.toml 该依赖的
/// 注释）；用到的 `brain.svg` / `trash.svg` 已随 `shell/assets/icons` 打进
/// `AppAssets`，路径 `icons/brain.svg` 能解析到。
use gpui_kit_assets::IconName as LucideIcon;

/// 纯文本、可拖选的文本段（窗口级选区的一员）。
///
/// 2026-10-09 用户要求「run command 之类的 agent 工具展开内容支持鼠标进行选中」：
/// 工具展开出来的正文此前是 `div().child(SharedString)` —— 那是一个**画上去的**
/// 字符串，没有任何选区参与；鼠标拖过去只会选中旁边的正文，选中范围里缺这一段。
///
/// 为什么是它而不是 AI 消息正文那条路（`TextView::markdown(..).selectable(true)`）：
/// 消息正文是 **Markdown 原文**，解析它是对的；而工具展开出来的是**原样输出**
/// （`run_command` 直接放远端 stdout），过一遍 Markdown 会把 `-rw-r--r--` 这种
/// 行首当列表符号、`__x__` 当强调 —— 那是**渲染出错**，不是「格式更漂亮」。
///
/// 它注册进的 `TextSelection` 是**窗口级**的：挂载（`TextSelectionLayer`）与
/// `Ctrl+C` / 右键复制都由 `gpui_component::Root` 全窗提供（本应用的外壳就是
/// `Root::new(AppShell)`，见 `shell/src/app.rs`），应用侧不需要额外装配 ——
/// 也因此它与消息正文的选区**是同一个**，拖选可以跨「正文 → 展开内容」。
/// 代价是版本线必须与 gpui-component 一致，见本文件末尾的编译期护栏。
use gpui_base::SelectableText;

use rustshell_core::ai::{ChatMessage, MsgState, Risk};

use super::main_view::MainView;

/// N8：样式取色统一走主题工具函数（删除就地 hex 定义）
use crate::theme::to_hsla as hex;

/// 会话正文基准字号（侧栏窄，比组件默认的 `text_sm` 低一档）
const BODY_TEXT: f32 = 12.5;

/// 官方输入框的**一行**高度（px）。
///
/// 行高由组件库写死：`gpui-component/src/input/input.rs` 的
/// `const LINE_HEIGHT: Rems = Rems(1.25)`，而本仓把 `rem_size` 收到 **15**
/// （`crate::theme`：`Root` 每帧 `window.set_rem_size(cx.theme().font_size)`，
/// 用户要求「比组件库默认的 16 小一号」）⇒ 一行 = 1.25 × 15 = 18.75px。
const AI_LINE_H: f32 = 1.25 * 15.;

/// 工具行（思考开关 / 发送按钮）的高度（px）：发送按钮是官方 icon-only 的
/// **Medium** 档 = `size_8()` = 2rem = 30px；思考开关的 `xsmall` 只有 1.25rem，
/// 故取大者。
const AI_TOOL_ROW_H: f32 = 2. * 15.;

/// 输入条上下「吃掉」的高度（px）：根 `pt_2 + pb_3`（0.5 + 0.75rem）、
/// 内层容器 `p_2` 上下（0.5 × 2rem）、以及输入框与工具行之间的 `gap_2`（0.5rem），
/// 合计 2.75rem = 41.25px。
const AI_COMPOSER_CHROME_Y: f32 = 2.75 * 15.;

/// 输入条**下方那行提示**（`ai_hint`：未配置 / 已保存 / 已加入白名单…）占掉的高度（px）：
/// 它挂在输入条的**兄弟位置**上（root 的 `gap_1` = 0.25rem = 3.75px），本身是一行
/// 10.5px 的小字，而 gpui 的默认行高是 `phi()`（字号的 1.618 倍）⇒ 10.5 × 1.618 ≈ 17px。
///
/// 为什么要算进「下限」：`ai_hint` 全仓**只写不清**（见 `main_view/ai_actions.rs`），
/// 也就是说一次「已保存」之后它会一直挂在那儿 —— 它跟输入框抢的是同一块面板高度。
/// 不算进来的话，「拖到最小也还能看见两行」在真实使用里立刻不成立。
const AI_HINT_ROW_H: f32 = 10.5 * 1.618 + 0.25 * 15.;

/// 输入条里**不给输入框**的那部分固定高度（px）—— 唯一口径。
///
/// 生产侧拿它推下限（[`AI_COMPOSER_MIN_H`]）；测试侧拿它把「面板高度」换算成
/// 「输入框实际高度」（`mod tests` 里的 `composer_input_height`）。
/// 这两处若各写一遍，差一个 padding 就是半行字被切掉。
const AI_COMPOSER_FIXED_H: f32 = AI_COMPOSER_CHROME_Y + AI_TOOL_ROW_H + AI_HINT_ROW_H;

/// 输入条默认高度 / 拖拽区间（2026-09-20「输入栏可以上下调整大小」；
/// 2026-09-30「输入框需要高一点，保持有两行文字可以正常显示」）。
///
/// 输入框拿到的高度 = 本值 − 固定开销（[`AI_COMPOSER_FIXED_H`]，含提示行
/// [`AI_HINT_ROW_H`]）。旧值 104 只给到 32.75px —— 那是扣掉「不含提示行的那部分」
/// （[`AI_COMPOSER_CHROME_Y`] + [`AI_TOOL_ROW_H`] = 71.25px）之后的数：**不到两行**
/// （1.75 行），第二行被切掉一截，正是用户看到的样子。所以一起重定：
///
/// * 默认：两行 + 全部固定开销（含提示行）再抬高 12px ⇒ 提示行挂着时输入框
///   ≈ 2.6 行，没挂时 ≈ 3.8 行，写东西时不必一直滚；
/// * 下限：**恰好**两行 + 全部固定开销（含提示行）—— 再往里拖就会破坏
///   「两行可读」这条用户明确提的要求；
/// * 上限：避免拖到把消息区挤没（面板本身还矮时，官方面板组会自己往回收）。
pub(crate) const AI_COMPOSER_H: f32 = AI_COMPOSER_MIN_H + 12.;
const AI_COMPOSER_MIN_H: f32 = 2. * AI_LINE_H + AI_COMPOSER_FIXED_H;
const AI_COMPOSER_MAX_H: f32 = 400.;

pub fn render(m: &mut MainView, cx: &mut Context<MainView>) -> impl IntoElement {
    let cfg_ready = m.ai_ui.ai_cfg.is_configured();

    // /S5：宽度与背景由`MainView`的侧栏容器统一提供（可拖拽调宽 + bg_elevated
    // + 右侧分割线），面板只负责内容。
    //
    // 2026-09-20 运行反馈「AI助手 输入栏可以上下调整大小」：消息区与输入区之间插一根
    // 可拖拽分界线（官方 `v_resizable`，与终端上下分屏用的是同一件东西）。
    // 上块吃满剩余空间；下块定高 + **`flex_none()`** —— 面板组内部无条件
    // `flex_grow_1`，不给它 `flex_none` 两块会各占一半，输入区会凭空占掉半个面板。
    // 高度落回 `MainView::ai_composer_h`（拖拽结束才写回，见 `on_resize`），
    // 与主窗格侧栏宽度同一套手法。
    v_flex()
        .id("ai-panel")
        .w_full()
        .h_full()
        .child(header(m, cx))
        .child(
            gpui_component::resizable::v_resizable("ai-composer-split")
                .child(
                    gpui_component::resizable::resizable_panel().child(
                        // 上块：消息区（空列表不渲染 row，四态由应用侧组合）
                        // + 生成中 / 失败提示（UI-04：放在消息区之外，
                        // 既不随滚动漂走，也不占用虚拟行）
                        v_flex()
                            .flex_1()
                            .min_h_0()
                            .child(message_area(m, cx))
                            .when_some(status_row(m, cx), |this, row| this.child(row)),
                    ),
                )
                .child(
                    gpui_component::resizable::resizable_panel()
                        .size(px(m.ai_ui.ai_composer_h))
                        .size_range(px(AI_COMPOSER_MIN_H)..px(AI_COMPOSER_MAX_H))
                        .flex_none()
                        .child(composer(m, cx, cfg_ready)),
                )
                .on_resize({
                    let me = cx.entity();
                    move |state, _w, cx| {
                        // 拖某一根分界线只改它相邻的两块，这里取**最后一块**
                        // （输入区）的高度即可。`on_resize` 只在 mouse up 触发一次，
                        // 拖拽过程中面板自己跟手，松手才把结果落到字段上。
                        let h = state.read(cx).sizes().last().copied();
                        if let Some(h) = h {
                            me.update(cx, |m, cx| {
                                m.ai_ui.ai_composer_h =
                                    f32::from(h).clamp(AI_COMPOSER_MIN_H, AI_COMPOSER_MAX_H);
                                cx.notify();
                            });
                        }
                    }
                }),
        )
}

/// 标题行：会话标题 + 「新建对话 / 删除对话 / 关闭对话」三枚图标动作。
///
/// 2026-09-18 运行反馈「清空按钮改为 new chat 图标，去除设置图标，增删关闭图标，
/// 聊天界面去除模型名称展示」，四条改动各自的理由：
///
/// - **模型名撤下**：它回答的是「后端是谁」，不是「我正看着哪段对话」。换成本条
///   会话的标题 —— 与左侧聊天记录里高亮的那一行对得上，这才是头部该答的问题。
/// - **设置入口撤下**：设置面板在竖栏栏底、命令面板、以及「未配置」空态里都有，
///   头部这一枚是重复入口。
/// - **「清空」→「新建对话」**（`RotateCw` → `Plus`）：旧动作清掉当前会话，用户
///   还得自己重开一条；现在直接开新的一条，旧的那条留在聊天记录里可回看。
/// - **补上「删除对话」与「关闭对话」**：删的是当前会话（左侧列表行上也有同一
///   动作），关的是终端右侧这一列。此前两者都无处可点 —— 想收起这一列只能去
///   竖栏再点一次 AI 图标。
fn header(m: &MainView, cx: &mut Context<MainView>) -> impl IntoElement {
    let t = crate::theme::current(cx);
    // 不变量：`ai_chats` 恒非空（见 `MainView` 字段文档），故这里无需兜底分支
    let title = m.ai_cur().title.clone();
    let busy = m.ai_ui.ai_busy;

    h_flex()
        .w_full()
        .items_start()
        .justify_between()
        .gap_2()
        .px_3()
        .py_2()
        .border_b_1()
        .border_color(hex(t.border_subtle))
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_0p5()
                .child(
                    div()
                        .text_size(px(12.5))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .child(rustshell_core::i18n::t("AI 助手")),
                )
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_size(px(10.5))
                        .text_color(hex(t.fg_muted))
                        .child(SharedString::from(title)),
                ),
        )
        .child(
            h_flex()
                .flex_none()
                .items_center()
                .gap_0p5()
                .child(
                    Button::new("ai-new-chat")
                        .ghost()
                        .xsmall()
                        .icon(LucideIcon::Plus)
                        .tooltip(rustshell_core::i18n::t("新建对话"))
                        .on_click(cx.listener(|m, _e, _w, cx| {
                            m.ai_new_chat(cx);
                        })),
                )
                .child(
                    Button::new("ai-delete-chat")
                        .ghost()
                        .xsmall()
                        .icon(LucideIcon::Trash)
                        .tooltip(rustshell_core::i18n::t("删除对话"))
                        // 生成中不许删：在途答复按「当前会话的最后一条消息」收口，
                        // 会话被拿掉之后增量无处可写（`ai_delete_chat` 自己也会拦，
                        // 这里置灰只是让「现在不能删」这个原因看得见）。
                        .disabled(busy)
                        .on_click(cx.listener(|m, _e, _w, cx| {
                            let id = m.ai_ui.ai_chat_id;
                            m.ai_delete_chat(id, cx);
                        })),
                )
                .child(
                    Button::new("ai-close-chat")
                        .ghost()
                        .xsmall()
                        .icon(LucideIcon::PanelRightClose)
                        .tooltip(rustshell_core::i18n::t("关闭对话"))
                        .on_click(cx.listener(|m, _e, _w, cx| {
                            m.ai_close_chat(cx);
                        })),
                ),
        )
}

/// 消息区（需求文档 7 节）：空列表不渲染行，四态由应用侧组合。
fn message_area(m: &mut MainView, cx: &mut Context<MainView>) -> AnyElement {
    let t = crate::theme::current(cx);

    // 态 1：未配置 AI 服务
    if !m.ai_ui.ai_cfg.is_configured() {
        return centered_card(
            cx,
            IconName::Bot,
            rustshell_core::i18n::t("未配置 AI 服务"),
            rustshell_core::i18n::t("填写端点与 API Key 后即可提问"),
            Some(
                Button::new("ai-empty-settings")
                    .small()
                    .primary()
                    .label(rustshell_core::i18n::t("打开设置"))
                    .on_click(cx.listener(|m, _e, w, cx| {
                        m.open_settings(w, cx);
                    })),
            ),
        );
    }

    // 态 2：已配置但会话为空
    if m.ai_msgs().is_empty() {
        let examples: [&'static str; 3] = [
            "当前目录下最近修改的文件有哪些？",
            "上一条命令报错，帮我分析原因",
            "写一条按端口查占用进程的命令",
        ];
        let mut col = v_flex().w_full().gap_2().items_center().child(
            div()
                .text_size(px(BODY_TEXT))
                .text_color(hex(t.fg_muted))
                .child(rustshell_core::i18n::t(
                    "向 AI 描述任务或提问，会自动附上当前终端最近输出。",
                )),
        );
        for (ex_ix, ex) in examples.into_iter().enumerate() {
            let text = ex.to_string();
            col = col.child(
                div()
                    // 可点击的 div 必须先`.id()`（gpui 的 Stateful 元素要求）
                    .id(SharedString::from(format!("ai-example-{ex_ix}")))
                    .w_full()
                    .rounded(cx.theme().radius_lg)
                    .border_1()
                    .border_color(hex(t.border_subtle))
                    .bg(hex(t.bg_base))
                    .px_2()
                    .py_1()
                    .text_size(px(11.5))
                    .text_color(hex(t.fg_secondary))
                    .cursor_pointer()
                    .hover(|s| s.border_color(hex(t.accent)))
                    .child(SharedString::from(text.clone()))
                    .on_click(cx.listener(move |m, _e, w, cx| {
                        m.ai_passthrough(&text, w, cx);
                    })),
            );
        }
        return v_flex()
            .flex_1()
            .min_h_0()
            .justify_center()
            .px_4()
            .child(col)
            .into_any_element();
    }

    // 态 3：正常消息流（虚拟列表 + 尾部跟随 + 「跳到最新」）
    let me = cx.entity();
    div()
        .flex_1()
        .min_h_0()
        .child(
            MessageScroller::new(
                "ai-messages",
                m.ai_ui.ai_scroller.clone(),
                move |ix, _w, cx| message_row(&me, ix, cx),
            )
            // UI-05：本地化文案 + 保留可读 label（a11y）
            .with_jump_button_label(rustshell_core::i18n::t("跳到最新"))
            // UI-08：底部渐隐用面板底色，被裁的半行融入背景而非硬切
            .with_bottom_fade(hex(t.bg_elevated))
            // 演示稿的行距是「2rem 行间 + 列表底 padding」，侧栏比演示框窄得多，
            // 统一收到 12px，最后一行到输入条的间距交给输入条自己的 pt。
            .with_row_style(StyleRefinement::default().pb_3()),
        )
        .into_any_element()
}

/// 单行消息（作为`MessageScroller`的 row renderer）。
fn message_row(me: &Entity<MainView>, ix: usize, cx: &mut App) -> AnyElement {
    let (msg, think_open) = {
        let m = me.read(cx);
        let Some(msg) = m.ai_msgs().get(ix).cloned() else {
            return div().into_any_element();
        };
        let open = m.ai_ui.ai_think_expanded.contains(&msg.id);
        (msg, open)
    };
    let msg_id = msg.id;
    // 一条消息的**正文**：显示与复制共用同一个口径（`message_body`），
    // 两处不会再分叉。
    //
    // 2026-09-30 用户反馈「复制按钮只需要正常复制对应的消息」：此前显示走的是
    // 「剥掉上下文包装的提问」，而复制按钮与右键菜单复制的却是 `msg.content`
    // **原文** —— 对用户消息来说，那里面还裹着 `with_context` 注入的终端事实与
    // 终端最近输出，粘到别处是一大段「我没问过的东西」。
    let body = message_body(&msg);
    let streaming = matches!(msg.state, MsgState::Streaming);
    // 右键菜单与常驻按钮复制的是同一份文本（`body`）。两个闭包都是 `move`，
    // 各持一份拷贝，免得为了「谁先 move」再分叉出第二个口径。
    let copy_menu = body.clone();
    let copy_click = body.clone();
    let can_regen = msg.role.is_user()
        && me
            .read(cx)
            .ai_msgs()
            .iter()
            .rposition(|m| m.role.is_user())
            .is_some_and(|p| me.read(cx).ai_msgs()[p].id == msg_id);
    let failed_reason = match &msg.state {
        MsgState::Failed(r) => Some(r.clone()),
        _ => None,
    };

    // 演示稿口径：用户消息右对齐浅底气泡，助手消息左对齐通栏文本（无气泡底色）。
    let alignment = if msg.role.is_user() {
        MessageAlignment::End
    } else {
        MessageAlignment::Start
    };
    let variant = if msg.role.is_user() {
        BubbleVariant::Muted
    } else {
        BubbleVariant::Ghost
    };
    // 2026-09-17 运行反馈「请求和想要内容都需要有外框线」：官方 `Ghost` 变体把边框、
    // 底色、内边距全部归零（见 gpui-component/src/bubble.rs），助手回复因此是一段悬空文字，
    // 与提问之间没有边界。这里在气泡表面补一圈描边，两种形态都成为可辨认的卡片。
    // 描边加在 `BubbleContent` 上（而不是 `Bubble`），是因为 `BubbleContent::render`
    // 结尾会 `refine_style`，调用方样式最后生效；加在外层会被内层背景盖住。
    let bubble_border = hex(crate::theme::current(cx).border_subtle);
    let ghost_pad = if msg.role.is_user() {
        // 浅底气泡（Muted）自带内边距，只补描边
        None
    } else {
        // Ghost 的 `p_0()` 会把文字贴到描边上，补回一档内边距
        Some(px(10.))
    };

    // 流式在途：末尾打字机光标
    let mut bubble_text = body;
    if streaming && !bubble_text.is_empty() {
        bubble_text.push_str(" ▍");
    }

    let mut content = MessageContent::new();
    // 思考过程挂在同一行正文之上（先思考后回答是同一轮回复）
    if !msg.reasoning.is_empty() {
        content = content.child(think_block(&msg, think_open, me, cx));
    }
    let mut bubble_content = BubbleContent::new()
        .text_size(px(BODY_TEXT))
        .border_1()
        .border_color(bubble_border)
        .child(
            // 正文按 **Markdown** 渲染（2026-09-20 反馈「消息支持 md 格式」），并允许
            // 鼠标**选中**（反馈「也可以选中」）。两件事都由官方 `TextView` 一并提供：
            // 它自己解析 md、自己管选区，不需要在应用侧维护「渲染成什么元素」。
            //
            // 选中内容的复制口径取默认的 `SelectionFormat::Plain`（渲染后的纯文本，
            // 不带 `**` 这类标记），与下面「复制消息」按钮**故意不同**：
            // 按钮给的是这条消息的正文原文（保留 Markdown 标记，适合「整段拿走」），
            // 拖选给的是所见即所得的纯文本（适合「挑一句粘给别人」）。
            //
            // id 必须逐消息唯一且稳定（虚拟列表会复用行、索引会漂移），所以用消息 id
            // 派生；`TextView` 的解析结果按 id + 文本缓存，流式期间文本每帧都在变，
            // 也就是每帧重解析一次 —— 单条消息长度有限，实测可接受。
            TextView::markdown(
                ElementId::Name(SharedString::from(format!("ai-msg-md-{msg_id}"))),
                SharedString::from(bubble_text),
            )
            .selectable(true)
            .text_size(px(BODY_TEXT))
            .w_full(),
        );
    // agent 工具轨迹块（v1.1 §3.9）：**在气泡内**、正文之下 —— 它属于这条回复，
    // 而不是一条独立的消息。行数变化会一起改这一行的高度，上面/下面都已
    // 成对 `remeasure`（调用点在 `ai_actions::ai_drain_agent`）。
    if !msg.trace.is_empty() {
        bubble_content = bubble_content.child(trace_block(&msg, me, cx));
    }
    if let Some(pad) = ghost_pad {
        bubble_content = bubble_content
            .px(pad)
            .py(px(6.))
            .rounded(cx.theme().radius_lg);
    }
    content = content.bubble(Bubble::new().with_variant(variant).content(bubble_content));

    let mut message = Message::new()
        .alignment(alignment)
        .text_size(px(BODY_TEXT))
        .content(content);
    if let Some(footer) = message_footer(&msg, cx) {
        message = message.footer(footer);
    }

    // 元素 id 由稳定消息 id 派生（不再用行索引，虚拟列表会让索引漂移）
    let row_selector: &'static str = if msg.role.is_user() {
        "ai-message-row-user"
    } else {
        "ai-message-row-assistant"
    };
    let mut row = div()
        .id(SharedString::from(format!("ai-msg-{msg_id}")))
        // 测试钩子（release 下被 `#[cfg]` 摘掉）：让用例能确认「这一行确实被渲染」
        .debug_selector(move || row_selector.to_string())
        .w_full()
        .flex()
        .flex_col()
        .gap_2()
        .context_menu(move |menu: PopupMenu, _w, _cx| {
            let copy = copy_menu.clone();
            menu.item(
                PopupMenuItem::new(rustshell_core::i18n::t("复制")).on_click(move |_, _w, cx| {
                    let item = gpui::ClipboardItem::new_string(copy.clone());
                    cx.write_to_clipboard(item);
                }),
            )
            .when(can_regen, |m_| {
                m_.item(
                    PopupMenuItem::new(rustshell_core::i18n::t("重新生成")).on_click(
                        |_, _w, cx| {
                            super::main_view::ui_call_ai_regen(cx);
                        },
                    ),
                )
            })
            .item(
                PopupMenuItem::new(rustshell_core::i18n::t("删除消息")).on_click(
                    move |_, _w, cx| {
                        super::main_view::ui_call_ai_delete_msg(msg_id, cx);
                    },
                ),
            )
        })
        .child(message);

    // 常驻的「复制这条消息」图标（2026-09-20 反馈「AI助手消息增加复制图标，可以
    // 复制信息」）。**常驻**而不是悬停才出现：悬停态只在鼠标真的移到那一行之后才被
    // 发现，而「消息太长、拖选很麻烦」恰恰是用户正想找按钮的时刻 —— 那个瞬间找不到
    // 入口就等于没有。视觉上用 `ghost`（无底无框）+ 右对齐，不去和正文的 Markdown
    // 排版抢注意力。
    //
    // 复制的是**这条消息的正文**（`copy_click` = 上面那个 `body`，保留 `**` 这类
    // Markdown 标记），与右键菜单里那条「复制」同源 —— 两处都是 `message_body`
    // 的输出，不会再分叉。用户消息因此只给出自己问的那句，不带注入的终端上下文。
    row = row.child(
        h_flex().w_full().justify_end().child(
            Button::new(SharedString::from(format!("ai-copy-{msg_id}")))
                .ghost()
                .xsmall()
                .icon(IconName::Copy)
                .tooltip(rustshell_core::i18n::t("复制消息"))
                .on_click(move |_e, _w, cx| {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(copy_click.clone()));
                }),
        ),
    );

    // 失败态：气泡下方一条错误提示（官方`Alert`）+「重试」。
    // `Alert` 没有任意子节点槽，所以重试按钮落在提示条下面，不再与原因挤同一行。
    if let Some(reason) = failed_reason {
        row = row.child(
            v_flex()
                .gap_1()
                .child(
                    crate::notice::NoticeKind::Error
                        .card(
                            SharedString::from(format!("ai-msg-error-{msg_id}")),
                            SharedString::from(reason),
                        )
                        .xsmall()
                        .text_size(px(10.5)),
                )
                .child(
                    h_flex().justify_end().child(
                        Button::new(SharedString::from(format!("ai-retry-{msg_id}")))
                            .ghost()
                            .xsmall()
                            .label(rustshell_core::i18n::t("重试"))
                            .on_click(|_e, _w, cx| {
                                super::main_view::ui_call_ai_regen(cx);
                            }),
                    ),
                ),
        );
    }

    // L2 命令卡片：挂在产生它的回复上，随行一起测高（需求文档 5.4 节）
    for (card_ix, card) in msg.commands.iter().enumerate().filter(|(_, c)| !c.done) {
        row = row.child(render_command_card(
            msg_id, card_ix, &card.cmd, card.risk, cx,
        ));
    }

    // agent 确认卡片（v1.1 §3.6）：只挂在**拥有这次运行**的那条消息上。
    // 必须按消息 id 对号入座 —— 同一会话里先后跑过两轮 agent 时，
    // 卡片挂在别的消息下面会让人以为「这个操作属于上一轮的问题」。
    let pending = me
        .read(cx)
        .ai_ui
        .ai_agent
        .as_ref()
        .filter(|run| run.msg_id == msg_id)
        .and_then(|run| run.pending.clone());
    if let Some(req) = pending {
        row = row.child(render_confirm_card(msg_id, &req, cx));
    }

    row.into_any_element()
}

/// 轨迹行的状态色（§3.9：异常走 warning、待确认走 accent、拦截走 danger）。
fn trace_color(
    state: rustshell_core::agent::TraceState,
    t: &rustshell_core::theme::ThemeTokens,
) -> u32 {
    use rustshell_core::agent::TraceState as S;
    match state {
        S::Ok => t.success,
        S::Anomaly => t.warning,
        S::Running | S::Waiting => t.accent,
        S::Partial | S::Failed | S::BadArgs | S::Blocked | S::Disabled => t.danger,
        S::Rejected | S::Stopped => t.fg_muted,
    }
}

/// 工具轨迹块：每行「展开箭头 + 工具名 + 参数摘要 + 状态」，明细由**点击行**
/// 展开/收起。
///
/// 展开态存在 `MainView::ai_trace_expanded`（键 = 消息 id + call_id），点击经
/// `ai_toggle_trace_detail` 翻转。**不能用官方 `Collapsible`**：它是受控只读区域，
/// 没有点击回调，`open=false` 时 content 根本不渲染 —— 2026-09-21 运行反馈
/// 「明细无法打开」就是它（行里放了个永远打不开的折叠块）。
///
/// 2026-09-22 运行反馈「移除工具执行后的明细按钮，和展开图标功能重复了」：
/// 行内原本除了行首箭头，末尾还多挂了一行「明细」小字 —— 两个入口做的是同一件事
/// （都是翻转 `ai_trace_expanded`）。现在**行首箭头是唯一入口**，与「思考过程」块
/// （`think_block`：箭头 + 标题、整行可点）形态一致。
///
/// 顺带把箭头的**可见性**接到「这行有没有明细」上：
/// - 没有明细的行画一个点不动的箭头 = 一个骗人的入口 ⇒ 用透明色；但**槽位照占**，
///   否则「有明细 / 没明细」两态的工具名会左右错开（§38：图标占位必须与图标盒
///   同宽，宽度只能有一个来源 —— 这里两态共用同一个 `Icon::xsmall()` 盒）。
/// - 同理，没有明细的行也不挂 `hover` / `on_click`（挂了就是「点了没反应」）。
fn trace_block(msg: &ChatMessage, me: &Entity<MainView>, cx: &mut App) -> AnyElement {
    let t = crate::theme::current(cx);
    let expanded = me.read(cx).ai_ui.ai_trace_expanded.clone();
    let mut list = v_flex().w_full().min_w_0().gap_1().pt_1();
    for row in &msg.trace {
        let open = expanded.contains(&(msg.id, row.call_id.clone()));
        let color = hex(trace_color(row.state, &t));
        // `render_detail()` 而不是 `truncated_detail()`：那是 8 KiB 的**渲染**口径
        // （64 KiB 的存储口径只适合落盘）。收起时内容不进元素树，但展开一次就是
        // 全量文本布局，口径仍然要收紧。
        //
        // 在 head 之前算出来，是因为它同时决定「这行能不能展开」（箭头可见性与
        // 这行是否可点都看它）。
        let detail = row.render_detail();
        let has_detail = !detail.trim().is_empty();
        // `mut` + 恒给 `id`：可点 / 悬停挂在 **head** 上，而那几行要等算出
        // `has_detail` 之后才补（见下面 `if has_detail`）。2026-10-09 之前它们挂在
        // 整个 `line` 上，`head` 因此既不必 `mut` 也不必自己带 id。
        //
        // id 恒在（元素身份跨帧稳定），交互能力按 `has_detail` 补 —— 若把 `.id()`
        // 也塞进那个分支里，`head` 的类型会从 `Div` 变成 `Stateful<Div>`，
        // 两个分支就得各写一遍 `line.child(head)`。
        let mut head = h_flex()
            .w_full()
            .min_w_0()
            .items_center()
            .gap_2()
            .id(SharedString::from(format!(
                "ai-trace-head-{}-{}",
                msg.id, row.call_id
            )))
            .child(
                Icon::new(if open {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                })
                .xsmall()
                .text_color(if has_detail {
                    hex(t.fg_muted)
                } else {
                    gpui::transparent_black()
                }),
            )
            .child(Icon::new(LucideIcon::Settings2).xsmall().text_color(color))
            .child(
                div()
                    .flex_shrink_0()
                    .text_size(px(11.))
                    .text_color(hex(t.fg_primary))
                    .child(SharedString::from(row.tool.clone())),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(11.))
                    .text_color(hex(t.fg_muted))
                    .child(SharedString::from(row.summary.clone())),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .text_size(px(11.))
                    .text_color(color)
                    .child(SharedString::from(if row.note.trim().is_empty() {
                        row.state.label()
                    } else {
                        row.note.clone()
                    })),
            );
        let mut line = v_flex()
            .w_full()
            .min_w_0()
            .id(SharedString::from(format!(
                "ai-trace-{}-{}",
                msg.id, row.call_id
            )))
            .debug_selector({
                // `debug_bounds` 只收 `&'static str`：按状态给两个固定选择器，
                // 用例据此确认「轨迹行确实渲染出来了、状态也是那个状态」。
                let state = row.state;
                move || format!("ai-trace-{}", state.tag())
            });
        if has_detail {
            let call_id = row.call_id.clone();
            let me_click = me.clone();
            let msg_id = msg.id;
            // 热区落在 **head** 这一行上（箭头 + 工具名 + 摘要 + 状态），展开入口
            // 仍然只剩行首箭头一个（2026-09-22 运行反馈撤掉了与它重复的「明细」小字）。
            //
            // 为什么不继续盖住整行（2026-10-09 改）：展开出来的正文现在**可以用鼠标
            // 拖选**（见下面那个 `SelectableText`），而 gpui 的点击判定**没有拖拽
            // 阈值** —— 按下与松开落在同一个 hitbox 上就发 `ClickEvent`。整行可点的
            // 话，「按住 → 拖过正文 → 松开」会在选区刚成型的同一刻把这块折回去，
            // 用户看到的是「一拖就没了」。
            head = head
                .cursor_pointer()
                .hover(|s| s.bg(hex(t.bg_overlay)))
                .on_click(move |_e, _w, cx: &mut App| {
                    let call_id = call_id.clone();
                    me_click.update(cx, |m, cx| {
                        m.ai_toggle_trace_detail(msg_id, call_id, cx);
                    });
                });
            if open {
                let cid = row.call_id.clone();
                let detail_id = row.call_id.clone();
                line = line.child(
                    div()
                        .debug_selector(move || format!("ai-trace-detail-{cid}"))
                        .w_full()
                        .max_h(px(180.))
                        .overflow_y_scrollbar()
                        .text_size(px(10.))
                        .text_color(hex(t.fg_secondary))
                        // 原样输出 ⇒ 纯文本可拖选（`SelectableText` 的文档说明了两条
                        // 为什么：Markdown 会把 stdout 解析坏；选区与消息正文同源）。
                        .child(SelectableText::new(
                            ElementId::Name(SharedString::from(format!(
                                "ai-trace-detail-{detail_id}"
                            ))),
                            detail,
                        )),
                );
            }
        }
        list = list.child(line.child(head));
    }
    // 汇总行：折叠时用户也能看出「跑了几次、有没有出事的」
    list = list.child(div().text_size(px(10.)).text_color(hex(t.fg_muted)).child(
        SharedString::from(rustshell_core::agent::summary_line(&msg.trace)),
    ));
    list.into_any_element()
}

/// 确认卡片：说明「要做什么、影响几台、为什么需要确认」，两个动作。
///
/// 不用「加入白名单」第三个动作：批量（>1 台）本来就不走白名单（§7.4），
/// 而单目标的 `run_command` 走的是既有的命令卡片那条路。这里保持两个动作，
/// 用户不会误以为「点了这个以后就不用再确认了」。
fn render_confirm_card(
    msg_id: u64,
    req: &rustshell_core::runner::ConfirmRequest,
    cx: &mut App,
) -> AnyElement {
    let t = crate::theme::current(cx);
    v_flex()
        .w_full()
        .min_w_0()
        .gap_2()
        .p_2()
        .rounded(cx.theme().radius_lg)
        .border_1()
        .border_color(hex(t.warning))
        .bg(hex(t.bg_overlay))
        .id(SharedString::from(format!("ai-confirm-{msg_id}")))
        .debug_selector(move || "ai-agent-confirm".to_string())
        .child(
            h_flex()
                .gap_2()
                .items_center()
                .child(
                    Icon::new(LucideIcon::TriangleAlert)
                        .xsmall()
                        .text_color(hex(t.warning)),
                )
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(hex(t.fg_primary))
                        .child(SharedString::from(rustshell_core::i18n::t("需要确认"))),
                ),
        )
        .child(
            div()
                .text_size(px(11.))
                .text_color(hex(t.fg_secondary))
                .child(SharedString::from(req.reason.clone())),
        )
        .child(
            div()
                .w_full()
                .min_w_0()
                .text_size(px(11.))
                .text_color(hex(t.fg_primary))
                .child(SharedString::from(format!(
                    "{} · {}",
                    req.tool, req.summary
                ))),
        )
        .child(
            div()
                .text_size(px(10.))
                .text_color(hex(t.fg_muted))
                .child(SharedString::from(rustshell_core::i18n::ts(
                    format!("影响 {} 台", req.target_count),
                    format!("affects {} host(s)", req.target_count),
                ))),
        )
        .child(
            h_flex()
                .gap_2()
                .justify_end()
                .child(
                    Button::new(SharedString::from(format!("ai-confirm-deny-{msg_id}")))
                        .ghost()
                        .xsmall()
                        .label(rustshell_core::i18n::t("忽略"))
                        .on_click(|_e, _w, cx| {
                            super::main_view::ui_call_ai_confirm(false, cx);
                        }),
                )
                .child(
                    Button::new(SharedString::from(format!("ai-confirm-ok-{msg_id}")))
                        .primary()
                        .xsmall()
                        .label(rustshell_core::i18n::t("执行"))
                        .on_click(|_e, _w, cx| {
                            super::main_view::ui_call_ai_confirm(true, cx);
                        }),
                ),
        )
        .into_any_element()
}

/// 消息页脚：用户消息不再标注「是否附带终端上下文」，助手消息标出生成状态。
///
/// 2026-09-17 运行反馈「移除文字：附带终端最近输出」：上下文是否附上属于内部实现，
/// 摆在每条提问下面只是噪音（`injected_lines` 仍如实记录在消息里，需要时可查）。
/// 助手侧的「正在生成… / 已停止」是**进行中状态**，不是说明文字，保留。
fn message_footer(msg: &ChatMessage, cx: &App) -> Option<MessageFooter> {
    let t = crate::theme::current(cx);
    if msg.role.is_user() {
        return None;
    }
    match &msg.state {
        MsgState::Streaming => Some(
            MessageFooter::new()
                .text_size(px(10.))
                .gap_1()
                .child(Spinner::new().xsmall().color(hex(t.fg_muted)))
                .child(SharedString::from(rustshell_core::i18n::t("正在生成…"))),
        ),
        MsgState::Cancelled => Some(
            MessageFooter::new()
                .text_size(px(10.))
                .child(SharedString::from(rustshell_core::i18n::t("已停止"))),
        ),
        _ => None,
    }
}

/// 「思考过程」折叠块：头部一行（展开箭头 + 标题 + 进行中的转圈），展开时给出正文。
fn think_block(msg: &ChatMessage, open: bool, me: &Entity<MainView>, cx: &mut App) -> AnyElement {
    let t = crate::theme::current(cx);
    let streaming = matches!(msg.state, MsgState::Streaming);
    let id = msg.id;
    let title = if streaming {
        rustshell_core::i18n::t("思考中…")
    } else {
        rustshell_core::i18n::t("思考过程")
    };
    let me = me.clone();

    v_flex()
        .w_full()
        .min_w_0()
        .rounded(cx.theme().radius_lg)
        .border_1()
        .border_color(hex(t.border_subtle))
        .bg(hex(t.bg_base))
        .overflow_hidden()
        .child(
            div()
                .id(SharedString::from(format!("ai-think-{id}")))
                .w_full()
                .flex()
                .items_center()
                .gap_1()
                .px_2()
                .py_1()
                .cursor_pointer()
                .hover(|s| s.bg(hex(t.bg_overlay)))
                .child(
                    Icon::new(if open {
                        IconName::ChevronDown
                    } else {
                        IconName::ChevronRight
                    })
                    .xsmall()
                    .text_color(hex(t.fg_muted)),
                )
                .child(
                    div()
                        .text_size(px(10.5))
                        .text_color(hex(t.fg_muted))
                        .child(SharedString::from(title)),
                )
                .when(streaming, |this| {
                    this.child(Spinner::new().xsmall().color(hex(t.fg_muted)))
                })
                .on_click(move |_e, _w, cx: &mut App| {
                    me.update(cx, |m, cx| m.ai_toggle_think_block(id, cx));
                }),
        )
        .when(open, |this| {
            this.child(
                div()
                    .w_full()
                    .min_w_0()
                    .px_2()
                    .pb_2()
                    .text_size(px(11.))
                    .line_height(gpui::relative(1.6))
                    .text_color(hex(t.fg_secondary))
                    // 思考正文与工具展开内容同属「展开出来的正文」⇒ 同样可拖选。
                    // 它虽然是模型写的 Markdown 味的散文，但这里**不解析**：
                    // 折叠块里紧挨着工具明细（原样输出），两条路各写一套 Markdown
                    // 口径只会让「同一种白底小字块」有两种渲染结果。
                    .debug_selector(move || format!("ai-think-detail-{id}"))
                    .child(SelectableText::new(
                        ElementId::Name(SharedString::from(format!("ai-think-detail-{id}"))),
                        msg.reasoning.clone(),
                    )),
            )
        })
        .into_any_element()
}

/// 生成中 / 失败提示（消息区之外，UI-04）
fn status_row(m: &MainView, cx: &mut Context<MainView>) -> Option<AnyElement> {
    if m.ai_ui.ai_busy && !m.ai_ui.ai_stream_open {
        return Some(
            div()
                .px_3()
                .py_1()
                .child(
                    Marker::new()
                        .id("ai-status-pending")
                        .role(Role::Status)
                        .loading(true)
                        .text_size(px(11.))
                        .content(MarkerContent::new().text(rustshell_core::i18n::t("思考中…"))),
                )
                .into_any_element(),
        );
    }
    // 失败态：若已有`MsgState::Failed`的消息行承载了原因，就不再重复显示；
    // 首个增量到达前就失败时没有占位行，必须在这里给出「原因 + 重试」。
    let carried_by_message = m
        .ai_msgs()
        .iter()
        .any(|x| matches!(x.state, MsgState::Failed(_)));
    let err = m.ai_ui.ai_error.clone().filter(|_| !carried_by_message)?;
    Some(
        v_flex()
            .px_3()
            .py_1()
            .gap_1()
            .child(
                crate::notice::NoticeKind::Error
                    .card("ai-error-last", SharedString::from(err))
                    .xsmall()
                    .text_size(px(11.)),
            )
            .child(
                h_flex().justify_end().child(
                    Button::new("ai-retry-last")
                        .ghost()
                        .xsmall()
                        .label(rustshell_core::i18n::t("重试"))
                        .on_click(cx.listener(|m, _e, _w, cx| {
                            m.ai_regen(cx);
                        })),
                ),
            )
            .into_any_element(),
    )
}

/// 输入条：圆角容器里放「无边框输入 + 思考模式开关 + 发送/停止」，与演示稿同构。
///
/// 2026-09-20「输入栏可以上下调整大小」之后，本函数的高度不再由内容决定：外层
/// 是可拖拽的定高面板（见 `render` 的 `v_resizable`），这里只要一路 `flex_1`
/// 把高度传下去，输入框就会跟着变高（`Textarea` 从单行换成多行也是为此）。
/// 那行工具按钮（思考模式 / 发送）`.flex_none()` —— 它们的高度是内容给的，
/// 不该跟着一起拉伸。
fn composer(m: &MainView, cx: &mut Context<MainView>, cfg_ready: bool) -> impl IntoElement {
    let t = crate::theme::current(cx);
    v_flex()
        .w_full()
        .h_full()
        .gap_1()
        .px_3()
        .pt_2()
        .pb_3()
        .child(
            v_flex()
                .w_full()
                .flex_1()
                .min_h_0()
                .gap_2()
                .p_2()
                .rounded(cx.theme().radius_lg)
                .border_1()
                .border_color(hex(t.border_subtle))
                .bg(hex(t.bg_base))
                .text_size(px(BODY_TEXT))
                // 几何用例（`ai_composer_shows_at_least_two_lines`）的度量点。
                // 输入框本身挂不了：`Textarea` 只实现 `Styled`（`debug_selector` 在
                // `InteractiveElement` 上）。所以量它的**容器**与工具行，输入框高度
                // = 容器 − 内边距 − 间距 − 工具行（三项都是本函数里写死的 rem）。
                .debug_selector(|| "ai-composer-box".to_string())
                .child(
                    // 多行输入（Enter 发送 / Shift+Enter 换行，见 `TextareaState::submit_on_enter`）。
                    // `appearance(false)` 与旧写法一致：边框与底色由外面这层圆角容器给。
                    Textarea::new(&m.ai_ui.ai_input)
                        .appearance(false)
                        .flex_1()
                        .min_h_0(),
                )
                .child(
                    h_flex()
                        .w_full()
                        .flex_none()
                        .items_center()
                        .justify_between()
                        .debug_selector(|| "ai-composer-tools".to_string())
                        .gap_2()
                        .child(
                            // 思考模式：开关即请求口径（关 = 不附带 thinking 参数、
                            // 不展示思考过程）。
                            //
                            // 2026-09-20 运行反馈「暗色模式下 AI 助手思考图标是否选中
                            // 看不出来区别」：官方 `.ghost().selected(..)` 的选中底色取
                            // `tokens.secondary_active`，而深色主题里它是 `bg_elevated
                            // (#161B22)`，与输入条底色 `bg_base(#0D1117)` **只差一档灰阶**
                            // —— 选中态与未选中态几乎同色（连 hover 的 `bg_overlay
                            // (#1C2128)` 都比它亮，于是「亮起来」的层次是反的）。
                            // 这里改成「选中即点亮」：accent 淡底 + accent 前景，
                            // 明暗两种主题下都与底色可区分；未选中仍是幽灵按钮，
                            // 只补一档 hover / active 底色（与面板里其它图标按钮一致）。
                            // 状态语义走 `.toggled(..)`（a11y 的 toggle 语义），
                            // 不再用 `.selected(..)` —— 它会把底色改成 active 那一档，
                            // 与我们自己按状态挑好的 variant 打架。
                            Button::new("ai-thinking")
                                .xsmall()
                                .with_variant(if m.ai_ui.ai_thinking {
                                    ButtonVariant::Custom(
                                        ButtonCustomVariant::new(cx)
                                            .color(hex(t.accent).opacity(0.18))
                                            .hover(hex(t.accent).opacity(0.28))
                                            .active(hex(t.accent).opacity(0.34))
                                            .foreground(hex(t.accent)),
                                    )
                                } else {
                                    ButtonVariant::Custom(
                                        ButtonCustomVariant::new(cx)
                                            .hover(hex(t.bg_overlay))
                                            .active(hex(t.bg_elevated))
                                            .foreground(hex(t.fg_secondary)),
                                    )
                                })
                                .icon(LucideIcon::Brain)
                                .toggled(m.ai_ui.ai_thinking)
                                .tooltip(rustshell_core::i18n::t("思考模式"))
                                .disabled(m.ai_ui.ai_busy)
                                .on_click(cx.listener(|m, _e, _w, cx| {
                                    let next = !m.ai_ui.ai_thinking;
                                    m.ai_toggle_thinking(next, cx);
                                })),
                        )
                        .child(if m.ai_ui.ai_busy {
                            Button::new("ai-stop")
                                .outline()
                                .icon(IconName::Pause)
                                .rounded(cx.theme().radius_full())
                                .tooltip(rustshell_core::i18n::t("停止生成"))
                                .on_click(cx.listener(|m, _e, _w, cx| {
                                    m.ai_stop(cx);
                                }))
                        } else {
                            Button::new("ai-send")
                                .primary()
                                .icon(IconName::ArrowUp)
                                .rounded(cx.theme().radius_full())
                                .tooltip(rustshell_core::i18n::t("发送"))
                                .disabled(!cfg_ready)
                                .on_click(cx.listener(|m, _e, w, cx| {
                                    m.ai_send(w, cx);
                                }))
                        }),
                ),
        )
        // 面板级提示（未配置、白名单生效等）落在输入条下方，不抢消息区
        .when_some(m.ai_ui.ai_hint.clone(), |this, hint| {
            this.child(
                div()
                    .min_w_0()
                    .text_size(px(10.5))
                    .text_color(hex(t.fg_muted))
                    .child(SharedString::from(hint)),
            )
        })
}

/// 居中卡片（未配置态等）
fn centered_card(
    cx: &App,
    icon: IconName,
    title: &'static str,
    desc: &'static str,
    action: Option<Button>,
) -> AnyElement {
    let t = crate::theme::current(cx);
    v_flex()
        .flex_1()
        .min_h_0()
        .justify_center()
        .items_center()
        .gap_2()
        .px_4()
        .child(Icon::new(icon).text_color(hex(t.fg_muted)))
        .child(
            div()
                .text_size(px(13.))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .child(SharedString::from(title)),
        )
        .child(
            div()
                .text_size(px(11.5))
                .text_color(hex(t.fg_muted))
                .text_center()
                .child(SharedString::from(desc)),
        )
        .children(action)
        .into_any_element()
}

/// 命令卡片（L2）：渲染在产生它的回复行内，因此拿不到`Context<MainView>`，「忽略」经捕获的`Entity<MainView>`回到实体上下文并同步重测该行（卡片消失会改行高）。
fn render_command_card(
    msg_id: u64,
    card_ix: usize,
    cmd: &str,
    risk: Risk,
    cx: &mut App,
) -> AnyElement {
    let t = crate::theme::current(cx);
    let (risk_color, risk_label) = match risk {
        Risk::Safe => (t.success, rustshell_core::i18n::t("低风险")),
        Risk::Confirm => (t.warning, rustshell_core::i18n::t("需确认")),
        Risk::Blocked => (t.danger, rustshell_core::i18n::t("已拦截")),
    };

    let cmd_view: SharedString = cmd.to_string().into();
    let cmd_copy = cmd.to_string();
    let cmd_exec = cmd.to_string();
    let cmd_wl = cmd.to_string();
    // 元素 id 由「消息 id + 卡片下标」派生：跨帧稳定且全局唯一
    let id_prefix: SharedString = format!("ai-card-{msg_id}-{card_ix}").into();

    div()
        .w_full()
        .rounded(cx.theme().radius_lg)
        .border_1()
        .border_color(hex(risk_color))
        .p_2()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div().flex().items_center().gap_1().child(
                div().text_size(px(10.)).text_color(hex(risk_color)).child(
                    rustshell_core::i18n::ts(
                        format!("命令 · {risk_label}"),
                        format!("Command · {risk_label}"),
                    ),
                ),
            ),
        )
        .child(
            div()
                .text_size(px(11.))
                .bg(hex(t.bg_base))
                .rounded(px(4.))
                .px_1()
                .py_1()
                .debug_selector(move || format!("ai-card-cmd-{msg_id}-{card_ix}"))
                // 命令本身也要能拖选：卡片上那枚「复制」给的是**整条**命令，而用户
                // 常常只想要其中一段（一个路径、一个参数）—— 那正是拖选存在的理由。
                .child(SelectableText::new(
                    ElementId::Name(SharedString::from(format!(
                        "ai-card-cmd-{msg_id}-{card_ix}"
                    ))),
                    cmd_view,
                )),
        )
        // 「禁止执行」是错误级提示，提成卡片里独立的一行（官方`Alert` 通栏形态）。
        // 此前它是一小段 danger 文字夹在按钮行中间，既与「复制/忽略」按钮抢同一行，
        // 也容易被当成按钮的说明文字而不是拦截原因。
        .when(risk == Risk::Blocked, |d| {
            d.child(
                crate::notice::NoticeKind::Error
                    .bar(
                        SharedString::from(format!("{id_prefix}-blocked")),
                        rustshell_core::i18n::t("禁止执行，请手动处理"),
                    )
                    .xsmall()
                    .text_size(px(10.)),
            )
        })
        .child(
            div()
                .flex()
                .gap_1()
                .justify_end()
                .child(
                    Button::new(format!("{id_prefix}-copy"))
                        .ghost()
                        .xsmall()
                        .label(rustshell_core::i18n::t("复制"))
                        .on_click(move |_e, _w, cx| {
                            let item = gpui::ClipboardItem::new_string(cmd_copy.clone());
                            cx.write_to_clipboard(item);
                        }),
                )
                .when(risk == Risk::Safe || risk == Risk::Confirm, |d| {
                    d.child(
                        Button::new(format!("{id_prefix}-run"))
                            .primary()
                            .xsmall()
                            .label(rustshell_core::i18n::t("执行"))
                            .on_click(move |_e, _w, cx| {
                                let c = cmd_exec.clone();
                                exec_via_hub(&c, cx);
                            }),
                    )
                    .child(
                        Button::new(format!("{id_prefix}-wl"))
                            .ghost()
                            .xsmall()
                            .label(rustshell_core::i18n::t("白名单"))
                            .tooltip(rustshell_core::i18n::t("加入免确认白名单"))
                            .on_click(move |_e, _w, cx| {
                                let c = cmd_wl.clone();
                                super::main_view::ui_call_ai_whitelist(c, cx);
                            }),
                    )
                })
                .child(
                    Button::new(format!("{id_prefix}-dismiss"))
                        .ghost()
                        .xsmall()
                        .label(rustshell_core::i18n::t("忽略"))
                        .on_click(move |_e, _w, cx| {
                            super::main_view::ui_call_ai_dismiss_card(msg_id, card_ix, cx);
                        }),
                ),
        )
        .into_any_element()
}

/// 供卡片按钮（非 cx.listener 场景）执行命令：经 UiHub 路由
pub fn exec_via_hub(cmd: &str, cx: &mut gpui::App) {
    let c = cmd.to_string();
    let Some(hub) = cx.try_global::<super::main_view::UiHub>() else {
        return;
    };
    let main = hub.main.clone();
    let _ = main.update(cx, |m, cx| m.ai_execute_command(&c, cx));
}

/// 一条消息「显示出来是什么、复制走的就该是什么」的**唯一**口径。
///
/// 用户消息存的不是用户原话，而是 `rustshell_core::ai::with_context` 包装过的请求体
/// （终端事实 + 终端最近输出 + `[用户]` 分隔行 + 原话）。包装是**协议**：要发给模型的
/// 是完整那一段，而屏幕上出现、以及「复制」按钮交到剪贴板里的，只该是用户自己问的话。
///
/// 2026-09-30 修：此前显示侧自己切了一次（`split_once("\n[用户]\n")`），复制侧却直接
/// 用 `msg.content` —— 于是同一行消息，「看到的」与「复制的」是两样东西（用户反馈
/// 「复制按钮只需要正常复制对应的消息」）。现在两处都走这里，改口径不可能只改一半。
///
/// 助手消息原样返回：它的 `content` 就是模型输出（保留 `**` 这类 Markdown 标记），
/// 前面没有包装可剥。
fn message_body(msg: &ChatMessage) -> String {
    if msg.role.is_user() {
        strip_context_wrapper(&msg.content)
    } else {
        msg.content.clone()
    }
}

/// 剥掉 `with_context` 的包装，只留用户原话。
///
/// 用 **`rsplit_once`（最后一个）而不是 `split_once`（第一个）**：注入的终端最近输出
/// 排在分隔行**之前**，而终端里出现的任何字面量都可能包含这一行（比如用户刚 `cat`
/// 过一个含 `[用户]` 的日志）。取最后一个才能保证剥出来的永远是分隔行**之后**的原话。
///
/// 剥不出分隔行时原样返回：没有终端上下文时包装只多一行 `[用户]`（`with_context` 的
/// 头部块会整段省略），老会话数据也可能根本没有包装 —— 两种情形都不该把内容吃掉。
fn strip_context_wrapper(content: &str) -> String {
    match content.rsplit_once("[用户]\n") {
        Some((_, q)) => q.to_string(),
        None => content.to_string(),
    }
}

/// 编译期护栏：本 crate 直接用到的 `gpui-base` 与 **gpui-component 用的是同一个
/// crate 版本**。
///
/// # 为什么必须钉住
///
/// 拖选注册进的是 `gpui-base` 的**进程级全局**（`GlobalState` /
/// `SelectionStateRegistry`），而读它的是 **gpui-component 的 `Root`**：
/// `TextSelectionLayer` 负责命中与高亮，`Root::on_action_copy` 负责 `Ctrl+C`。
/// 两条版本线走岔时就是**两套互不相干的全局** —— 拖选照常注册、照片什么也不显示，
/// `Ctrl+C` 拿到的仍是空串；全程不 panic、不打日志、**编译通过**，是本仓最不想要的
/// 那种「静默失效」。
///
/// 这个函数不会被调用，它的**存在**就是断言：入参与返回必须是同一个类型，
/// 版本线一岔它直接编译不过（比任何运行时用例都硬，也不必跑测试）。
///
/// 版本线的由来（「不能只写个 caret 就完事」）写在根 `Cargo.toml` 的 `gpui-base`
/// 条目下。
#[allow(dead_code)]
fn gpui_base_version_matches_the_one_gpui_component_uses(
    selection_format: gpui_base::text::SelectionFormat,
) -> gpui_component::text::SelectionFormat {
    selection_format
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::prod;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// 生产代码正文（剥注释 + 切测试模块，见 `crate::test_support`）。
    fn production_source() -> String {
        prod(include_str!("ai_panel.rs"))
    }

    /// 回归（2026-09-17 运行反馈「移除文字：附带终端最近输出」）：
    /// 用户消息下面不再挂「是否附带终端上下文」的页脚；
    /// 但助手侧的「正在生成… / 已停止」是进行中状态，必须保留。
    #[test]
    fn user_messages_carry_no_context_footer() {
        let code = production_source();
        assert!(
            !code.contains("附带终端最近输出") && !code.contains("未附带终端上下文"),
            "用户消息页脚不得再显示上下文附带文案。"
        );
        assert!(
            !code.contains("injected_lines"),
            "上下文行数不该再参与渲染（它仍记在消息数据里，只是不展示）。"
        );
        for needle in ["正在生成…", "已停止"] {
            assert!(
                code.contains(needle),
                "助手消息的进行中状态 `{needle}` 必须保留 —— 那是状态而非说明文字。"
            );
        }
    }

    /// 回归（2026-09-17 运行反馈「请求和想要内容都需要有外框线」）：
    /// 两类气泡都必须描边。官方 `Ghost`（助手）默认无边框、无内边距，
    /// 只补边框不补内边距会让文字贴线，故两者成对断言。
    #[test]
    fn both_bubble_kinds_are_outlined() {
        let code = production_source();
        assert!(
            code.contains(".border_1()") && code.contains(".border_color(bubble_border)"),
            "气泡表面必须描边（BubbleContent 上补 border_1 + border_color）。"
        );
        assert!(
            code.contains("ghost_pad"),
            "Ghost 气泡必须补回内边距，否则文字会贴在描边上。"
        );
    }

    /// 回归（2026-09-18 运行反馈「清空按钮改为 new chat 图标，去除设置图标，
    /// 增删关闭图标，聊天界面去除模型名称展示」）：
    ///
    /// 头部与输入条的四条改动必须同时成立 —— 只守住其中一条，另一条随时会
    /// 在下次改动里「反弹」回去（这一批反馈本身就是分几轮提的）。
    #[test]
    fn chat_header_drops_the_model_name_and_wires_three_icon_actions() {
        let code = production_source();

        assert!(
            !code.contains("ai_cfg.model"),
            "对话面板不再展示模型名 —— 它只该喂请求体（`sync_ai_config` / `spawn_chat`）。"
        );
        assert!(
            !code.contains("\"ai-goto-settings\""),
            "头部的设置入口已撤下：设置面板在竖栏栏底、命令面板与「未配置」空态里都有。"
        );
        for id in ["\"ai-new-chat\"", "\"ai-delete-chat\"", "\"ai-close-chat\""] {
            assert!(
                code.contains(id),
                "对话面板头部缺少动作按钮 {id}（增 / 删 / 关闭）。"
            );
        }
        assert!(
            !code.contains("IconName::RotateCw") && !code.contains("ai_clear"),
            "「清空」(`RotateCw` + `ai_clear`) 已由「新建对话」取代，不得复活。"
        );

        // 思考模式：图标开关（`Brain` 图标按钮），不再是带文字 label 的 `Toggle`
        // （它连 `icon()` 接口都没有）。选中态的上色方式见
        // `thinking_toggle_paints_its_checked_state_with_accent`。
        assert!(
            !code.contains("Toggle::new"),
            "「思考模式」不再是文字 Toggle —— 已改为图标开关。"
        );
        assert!(
            code.contains("LucideIcon::Brain"),
            "「思考模式」开关必须用脑图标（`Brain`）—— 图标即语义本身。"
        );
    }

    /// 回归（2026-09-20 运行反馈「暗色模式下，AI助手思考图标是否选中看不出来区别」）：
    ///
    /// 官方 `Button::selected()` 对 Ghost 变体取 `tokens.secondary_active`，而深色主题里
    /// 它是 `bg_elevated(#161B22)`，与输入条底色 `bg_base(#0D1117)` **只差一档灰阶**
    /// （连 hover 的 `bg_overlay(#1C2128)` 都比它亮，层次是反的）。这里钉住两条：
    /// * 选中态由 **accent** 上色 —— 按定义与任何底色可区分，明暗两种主题都成立；
    /// * 不再回头用 `.selected(..)` —— 它会用 active 那一档底色盖掉挑好的 variant，
    ///   正是这次要修的缺陷。
    #[test]
    fn thinking_toggle_paints_its_checked_state_with_accent() {
        let code = production_source();
        assert!(
            code.contains("ButtonCustomVariant") && code.contains("ButtonVariant::Custom"),
            "「思考模式」开关必须自己给选中 / 未选中两组颜色（ButtonCustomVariant），\n\
             否则又会退回官方 Ghost 的 selected 底色 —— 深色下它与底几乎同色。"
        );
        assert!(
            code.contains("hex(t.accent).opacity(0.18)") && code.contains("hex(t.accent)"),
            "选中态必须是「accent 淡底 + accent 前景」—— 只改底色或只改前景，\n\
             暗色主题下依旧分不出选中与未选中。"
        );
        assert!(
            !code.contains(".selected(m.ai_ui.ai_thinking)"),
            "思考开关不得再用官方 `.selected(..)`：它取 secondary_active（深色 = bg_elevated），\n\
             会把 accent 那套颜色盖回去。"
        );
        assert!(
            code.contains(".toggled(m.ai_ui.ai_thinking)"),
            "去掉 `.selected(..)` 之后，开关语义要靠 `.toggled(..)` 保住（a11y 的 toggle 状态）。"
        );
    }

    /// 回归（2026-09-20 运行反馈「AI助手 输入栏可以上下调整大小」）：
    ///
    /// 输入区要真的能被拖高、且拖高之后输入框跟着变高。三件事缺一不可：
    /// * `v_resizable` 提供那根分界线；`.flex_none()` 让输入区保持定高 —— 面板组内部
    ///   无条件 `flex_grow_1`，不给它两块会各占一半；
    /// * 高度写回 `MainView::ai_composer_h`（`on_resize` 只在 mouse up 触发一次）；
    /// * 输入框是**多行** `Textarea`，且一路把高度传下去 —— 单行 `Input` 的高度写死
    ///   一行，拖高输入栏只会多出一截空白。
    #[test]
    fn composer_height_is_draggable_and_the_input_stretches() {
        let code = production_source();
        for needle in [
            "v_resizable(\"ai-composer-split\")",
            ".size(px(m.ai_ui.ai_composer_h))",
            ".size_range(px(AI_COMPOSER_MIN_H)..px(AI_COMPOSER_MAX_H))",
            ".flex_none()",
            ".on_resize(",
            "m.ai_ui.ai_composer_h =",
            "Textarea::new(&m.ai_ui.ai_input)",
        ] {
            assert!(
                code.contains(needle),
                "输入栏可拖拽调整大小的通路缺了 `{needle}`：\n\
                 分界线 / 初始高度 / 区间 / 定高 / 回写 / 多行输入，缺一个都拖不成。"
            );
        }
        assert!(
            !code.contains("Input::new(&m.ai_ui.ai_input)"),
            "AI 输入框不该再是单行 `Input` —— 它高度写死一行，拖高输入栏等于多出一块空白。"
        );
    }

    /// 把 AI 面板跑起来（与 `shell::app` 同样的装配：Root → AppShell → MainView），
    /// 依次渲染「未配置 / 空会话 / 一轮带思考过程的流式回复」三种形态。
    ///
    /// 断言点：这一帧渲染不得 panic，且消息行真的建出来了（虚拟列表只建视口内的行，
    /// 行高塌成 0 或列表没渲染都会让这个用例红）。renderer 里的官方组件
    /// （`Message`/`Bubble`/`MessageFooter`/`Spinner`/`Toggle`）只要用法不成立，
    /// 都会在这里当场炸出来，不必等手工点开面板。
    #[gpui::test]
    fn ai_panel_renders_conversation_states_without_panic(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;
        use rustshell_core::ai::{AiCommandCard, ChatMessage, Risk};

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

        // 态 1：未配置（居中卡片）+ 切到 AI 面板
        main.update_in(&mut vcx.clone(), |m, window, cx| {
            m.ai_ui.ai_cfg.base_url = String::new();
            m.ai_ui.ai_cfg.model = String::new();
            m.ai_ui.ai_cfg.api_key_cipher = None;
            // 2026-09-18：对话正文搬到了**终端右侧那一列**（左侧栏 `PanelKind::Ai`
            // 现在放的是聊天记录列表），那一列的显隐由 `ai_chat_open` 决定。
            // `ai_passthrough` 自己会展开它 —— 本用例不手动置位，正好顺带钉住
            // 「命令面板的 AI助手 / 空态示例问题都能把对话列带出来」这条契约。
            m.ai_passthrough("", window, cx);
            cx.notify();
        });
        vcx.run_until_parked();

        // 态 2：已配置但会话为空（示例问题）
        main.update_in(&mut vcx.clone(), |m, _w, cx| {
            m.ai_ui.ai_cfg.base_url = "https://api.deepseek.com/v1".into();
            m.ai_ui.ai_cfg.model = "deepseek-reasoner".into();
            m.ai_ui.ai_cfg.api_key_cipher = Some("cipher".into());
            cx.notify();
        });
        vcx.run_until_parked();

        // 态 3：一轮在途回复——思考过程展开 + 流式正文 + 命令卡片 + 停止按钮
        main.update_in(&mut vcx.clone(), |m, _w, cx| {
            m.ai_msgs_mut().push(ChatMessage::user(
                1,
                "[当前终端上下文]\n/dev/sda1 100%\n\n[用户]\n磁盘满了吗",
                1,
            ));
            let mut reply = ChatMessage::assistant_streaming(2);
            reply.reasoning = "先看 df 输出：根分区 100%。".into();
            reply.content = "磁盘已写满，建议清理日志。".into();
            reply.commands.push(AiCommandCard {
                cmd: "df -h".into(),
                risk: Risk::Safe,
                done: false,
            });
            m.ai_msgs_mut().push(reply);
            // 思考期间默认展开（与 poll 侧同一口径）
            m.ai_ui.ai_think_expanded.insert(2);
            m.ai_ui.ai_stream_open = true;
            m.ai_ui.ai_busy = true;
            m.ai_ui.ai_scroller.update(cx, |s, cx| s.reset(2, cx));
            cx.notify();
        });
        vcx.run_until_parked();

        for (selector, label) in [
            ("ai-message-row-user", "用户提问"),
            ("ai-message-row-assistant", "助手回复"),
        ] {
            let row = vcx
                .debug_bounds(selector)
                .unwrap_or_else(|| panic!("{label}那一行没有渲染出来"));
            assert!(
                f32::from(row.size.height) > 0.,
                "{label}那一行高度为 0（消息列表塌陷）"
            );
        }
    }

    /// agent 轨迹块与确认卡片必须**真的渲染出来**（v1.1 §3.9 / §3.6）。
    ///
    /// 只断言「源码里有 `trace_block`」是不够的：把轨迹挂到错误的字段上、
    /// 或忘了在气泡里 `child` 它，界面照样打开、回复照样显示，只是**工具调用
    /// 全程不可见** —— 而「AI 到底跑了什么命令」正是 agent 最需要被看见的部分。
    /// 这里用行上挂的 `debug_selector`（按状态固定成 `ai-trace-{tag}`）量高度。
    #[gpui::test]
    fn agent_trace_and_confirm_card_are_rendered(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;
        use rustshell_core::agent::{TraceRow, TraceState};
        use rustshell_core::ai::ChatMessage;

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

        main.update_in(&mut vcx.clone(), |m, window, cx| {
            m.ai_ui.ai_cfg.base_url = "https://api.deepseek.com/v1".into();
            m.ai_ui.ai_cfg.model = "deepseek-flash".into();
            // **必须**把 `api_key_cipher` 也置上（与同文件 `ai_composer_shows_at_least_two_lines`、
            // `the_expanded_tool_detail_can_be_selected_with_the_mouse` 同一口径）。
            //
            // `ai_cfg` 是 `MainView::new` 从 `paths::ai_config_path()`（开发机上的
            // `%APPDATA%/rustshell/ai_config.json`）读进来的，而 `is_configured()`
            // 要求**端点 / 模型 / key 三者齐全**。用例只覆盖了前两项 ⇒ 面板落到
            // 「未配置 AI 服务」空态卡片并 `return`（`message_area` 态 1），消息列表
            // 压根不建，轨迹块与确认卡片自然都找不到。
            //
            // 这就是 2026-10-16 CI 失败的全部原因：开发机上那份 ai_config.json 恰好
            // 有一把真 key，本地因此是绿的；CI 是干净机器、没有这个文件 ⇒
            // `ai-trace-ok` 缺席 ⇒ `ui\src\ai_panel.rs:1694` panic。
            // 用例不能吃机器上的残留配置，自己把三件套配齐才算自洽。
            m.ai_ui.ai_cfg.api_key_cipher = Some("cipher".into());
            m.ai_passthrough("", window, cx);
            // 一条 assistant 回复，带三行轨迹（成功 / 异常 / 等待确认）
            let mut reply = ChatMessage::assistant(1, "三台都看过了。");
            let mut ok = TraceRow::new(1, "c1", "inspect_load");
            ok.summary = "prod-01, prod-02".into();
            ok.state = TraceState::Ok;
            ok.note = "3/3 完成".into();
            ok.elapsed_ms = 1800;
            ok.detail = "cpu 12%".into();
            let mut bad = TraceRow::new(2, "c2", "inspect_log");
            bad.summary = "prod-01:/var/log/app.log".into();
            bad.state = TraceState::Anomaly;
            bad.note = "命中 23 行".into();
            let mut wait = TraceRow::new(3, "c3", "run_command");
            wait.summary = "prod-01 · systemctl restart nginx".into();
            wait.state = TraceState::Waiting;
            reply.trace = vec![ok, bad, wait];
            m.ai_msgs_mut().push(reply);
            m.ai_ui.ai_scroller.update(cx, |s, cx| s.reset(1, cx));
            // 确认卡片挂在同一条消息上
            m.ai_ui.ai_agent = Some(crate::main_view::AgentRun {
                rx: flume::unbounded().1,
                decisions: flume::unbounded().0,
                alive: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
                pending: Some(rustshell_core::runner::ConfirmRequest {
                    step: 3,
                    call_id: "c3".into(),
                    tool: "run_command".into(),
                    summary: "prod-01 · systemctl restart nginx".into(),
                    reason: "该操作会改变服务器状态，请确认".into(),
                    target_count: 1,
                    command: "systemctl restart nginx".into(),
                    whitelistable: true,
                }),
                msg_id: 1,
                opened: true,
            });
            cx.notify();
        });
        vcx.run_until_parked();

        // 三种状态的轨迹行都要有非零高度（renderer 崩了/行没建出来都会红）
        for (sel, label) in [
            ("ai-trace-ok", "成功"),
            ("ai-trace-anomaly", "异常"),
            ("ai-trace-waiting", "等待确认"),
        ] {
            let b = vcx
                .debug_bounds(sel)
                .unwrap_or_else(|| panic!("{label}轨迹行没有渲染出来（轨迹块没挂进气泡？）"));
            assert!(
                f32::from(b.size.height) > 0.,
                "{label}轨迹行高度为 0（被 flex 挤没了？）"
            );
        }
        // 明细必须**点得开**（2026-09-21 运行反馈）：默认收起 → 展开出现非零高度
        // → 再收起消失。官方 `Collapsible` 时代这里点了什么都没发生。
        assert!(
            vcx.debug_bounds("ai-trace-detail-c1").is_none(),
            "明细默认应当是收起的（行首只剩一个向右的箭头，点行即展开）"
        );
        main.update_in(&mut vcx.clone(), |m, _w, cx| {
            m.ai_toggle_trace_detail(1, "c1".into(), cx);
        });
        vcx.run_until_parked();
        let detail = vcx
            .debug_bounds("ai-trace-detail-c1")
            .expect("展开后明细没有渲染出来 —— 轨迹行的折叠块点了没反应");
        assert!(f32::from(detail.size.height) > 0., "明细展开后高度为 0");
        main.update_in(&mut vcx.clone(), |m, _w, cx| {
            m.ai_toggle_trace_detail(1, "c1".into(), cx);
        });
        vcx.run_until_parked();
        assert!(
            vcx.debug_bounds("ai-trace-detail-c1").is_none(),
            "再点一次应当重新收起"
        );
        // 确认卡片同理：它带按钮，塌成 0 高就等于用户点不到「执行」
        let card = vcx
            .debug_bounds("ai-agent-confirm")
            .expect("确认卡片没有渲染出来（在途确认没有接到卡片上？）");
        assert!(f32::from(card.size.height) > 0., "确认卡片高度为 0");

        // 点「执行」→ 决定发出去、卡片消失（否则用户会以为点了没反应）
        main.update_in(&mut vcx.clone(), |m, _w, cx| {
            m.ai_confirm(true, cx);
        });
        vcx.run_until_parked();
        assert!(
            vcx.debug_bounds("ai-agent-confirm").is_none(),
            "点了「执行」之后确认卡片还挂着 —— 用户会重复点"
        );
    }

    /// agent 轨迹块必须能给出**异常明细**，且明细真的能点开。
    ///
    /// 契约（2026-09-22 用户要求「移除工具执行后的明细按钮，和展开图标功能重复了」
    /// 之后重写）：
    /// * 展开入口**只有一个**：行首箭头（与「思考过程」块同形）。此前行内另挂了一行
    ///   「明细」小字，做的却是同一个动作。负向断言里那个词用**转义**写 ——
    ///   源码里出现它的字面量会让 `core::i18n` 的 g2 认为该词条仍被引用，
    ///   而词条已删，于是「摘干净了」这件事反而没人看着。
    /// * 箭头可见性与「整行可点」都以 `has_detail` 为条件：没有明细的行画一个点不动
    ///   的入口 = 骗人。但箭头盒**恒在**（透明占位），否则「有 / 无明细」两态的工具名
    ///   会左右错开（§38：占位必须与图标盒同宽、宽度只能有一个来源）。
    /// * 明细用 `render_detail()`（8 KiB 渲染口径），而不是 `truncated_detail()`
    ///   （64 KiB 存储口径）—— 展开一次就是全量文本布局，口径要收紧；
    /// * 展开态必须接到 `ai_trace_expanded` / `ai_toggle_trace_detail` 上，
    ///   且**不得**再用官方 `Collapsible`：它是受控只读区域（没有点击回调，
    ///   `open=false` 时 content 不渲染），2026-09-21 运行反馈「明细无法打开」
    ///   就是行里放了个永远打不开的折叠块。
    #[test]
    fn trace_detail_block_uses_the_render_capped_text() {
        let code = production_source();
        assert!(
            !code.contains("Collapsible"),
            "明细不得再用官方 `Collapsible`：它没有点击回调，`open=false` 时\
             content 根本不渲染 —— 用户点了什么都没发生。"
        );
        // 只切出轨迹块本身：`.cursor_pointer()` 在「思考过程」「示例卡片」等处都有，
        // 拿全文件找第一个出现位置会把断言打到别的块上（假绿）。
        let start = code.find("fn trace_block(").expect("轨迹块必须存在");
        let tail = &code[start..];
        let end = tail
            .find("fn render_confirm_card(")
            .expect("轨迹块之后应当是确认卡片");
        let block = &tail[..end];

        assert!(
            block.contains("row.render_detail()"),
            "轨迹明细必须走 `render_detail()`（渲染口径截断）。"
        );
        assert!(
            !block.contains("row.truncated_detail()"),
            "`truncated_detail()` 是 64 KiB 的**存储**口径，不该直接进渲染 —— \
             折叠块收起时内容仍会被布局，每帧几十万字符会拖垮消息列表。"
        );
        assert!(
            block.contains("let has_detail = !detail.trim().is_empty();"),
            "轨迹行必须显式算出 `has_detail`：箭头可见性与「整行可点」都由它决定。"
        );
        let guard = block
            .find("if has_detail {")
            .expect("「整行可点 / 展开明细」必须挂在 `has_detail` 上");
        let clickable = block
            .find(".cursor_pointer()")
            .expect("有明细的轨迹行必须可点");
        assert!(
            guard < clickable,
            "`cursor_pointer` / `on_click` 必须落在 `if has_detail` 分支里 —— \
             没有明细的行挂上它们就是「点了没反应」。"
        );
        assert!(
            block.contains("gpui::transparent_black()"),
            "没有明细的行，箭头要用透明色**占住同一格**：既不出现骗人的入口，\
             工具名也不会随「有 / 无明细」两态左右错开（§38 占位同宽）。"
        );
        assert!(
            block.contains("ai_toggle_trace_detail"),
            "明细的展开必须接到交互状态上（`ai_toggle_trace_detail`），\
             否则点击没有任何东西翻转。"
        );
        // 全文件扫：入口只该有一个，别处也不许再长出第二处。
        let gone = "\u{660e}\u{7ec6}";
        assert!(
            !code.contains(gone),
            "轨迹行里又长出了「{gone}」文本入口 —— 展开 / 收起只留行首箭头一个入口。"
        );

        // 热区必须落在 **head**（箭头 + 工具名 + 摘要 + 状态）这一行上，而不是盖住整行
        // （2026-10-09）。展开出来的明细现在**可以用鼠标拖选**，而 gpui 的点击判定
        // **没有拖拽阈值**：按下与松开落在同一个 hitbox 上就会发 `ClickEvent`。
        // 热区盖整行的话，「按住 → 拖过明细 → 松开」会在选区刚成型的那一帧把这块折回去
        // —— 用户看到的是「一拖就没了」（`SelectableText` 的选区参与者随元素一起被裁掉）。
        let head_at = block
            .find("head = head")
            .expect("有明细的轨迹行必须把交互能力补在 `head` 上");
        // 注意形态：`head` 是被消费进 `list.child(line.child(head))` 的，
        // **没有** `line = line.child(head)` 这样的赋值（明细块才是 `line = line.child(…)`）。
        let mount_at = block
            .find("line.child(head)")
            .expect("轨迹行最终要把 `head` 挂上去");
        assert!(
            block[head_at..mount_at].contains(".on_click("),
            "点击热区必须挂在 `head` 上，不能盖住整行 —— 明细可拖选，gpui 又没有\
             拖拽阈值，整行可点会让「一拖」顺带把折叠块收起来。"
        );
    }

    /// 「展开内容支持鼠标选中」的源码级护栏（2026-10-09）。
    ///
    /// 需求：agent 工具（`run_command` 之类）的**展开内容**要能鼠标拖选，且**本地
    /// 终端**一路同样成立。实现是把三处「展开出来的正文」都注册进**窗口级选区**
    /// —— `gpui_base::SelectableText` 挂在 `gpui_component::Root` 已有的
    /// `TextSelectionLayer` 上（选区全局在 `gpui-base`，读它的 `Root` 在
    /// `gpui-component`），应用侧因此**零装配**：Ctrl+C 与右键复制自动可用。
    ///
    /// 为什么还要这条**源码扫**：端到端用例
    /// `the_expanded_tool_detail_can_be_selected_with_the_mouse` 只真拖了**工具明细**
    /// 一处；另外两处（思考正文 / 命令行本体）**没有**端到端覆盖，而它们是同一类需求、
    /// 随时可能各改各的。这里把三处一起钉住 —— 任何一处退回「画上去的字符串」
    /// （`SharedString` 直挂）都会红。
    ///
    /// 与**本地终端**的关系：AI 面板的渲染只读 `TraceRow` / 消息数据，**不看会话类型**；
    /// agent 工具链（`ai_execute_command` → `send_to_active_pty` → `send_lines_to_tabs`）
    /// 判的是连接 / 断连 / 终端在不在，也不判 `SessionKind`；本地与 SSH 又共用同一个
    /// 挂载关口 `mount_terminal`。所以「本地终端支持」与「SSH 支持」走的**是同一条
    /// 渲染路径** —— 钉住这条路径，也就同时钉住了两者。
    #[test]
    fn expanded_bodies_register_with_the_window_scoped_selection() {
        let code = production_source();

        // ① 三处展开正文都构造 `SelectableText`（参与窗口级选区）。
        assert_eq!(
            code.matches("SelectableText::new(").count(),
            3,
            "展开正文应**恰好三处**走窗口级选区：工具明细 / 思考正文 / 命令行本体。\
             少一处 = 那一处只能看不能选；多一处 = 有人往这里塞了新的正文却没登记，\
             `Root` 的选区层不会认它（编译、渲染、开面板全都正常，静默失效）。"
        );

        // ② 每一处都配 `debug_selector`（端到端用例要靠它量几何 —— 量不到就证不了「拖得动」）。
        for prefix in ["ai-trace-detail-", "ai-think-detail-", "ai-card-cmd-"] {
            let needle = format!("debug_selector(move || format!(\"{prefix}");
            assert!(
                code.contains(&needle),
                "展开正文 `{prefix}*` 少了 `debug_selector` —— \
                 端到端用例量不到它的几何，就没有证据说它可拖选。"
            );
        }
    }

    /// 「成对更新」契约的算术护栏（AI 面板需求文档 5 节）：`ai_msgs` 的每次增删改都必须映射到 scroller 的对应调用，否则会出现「少一行 / 残留行 / 末行被截断」。
    #[gpui::test]
    fn scroller_item_count_follows_message_mutations(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;
        use gpui_component::message_scroller::MessageScrollerState;

        cx.update(|cx| {
            let state = cx.new(|cx| MessageScrollerState::new(0, cx));
            assert_eq!(state.read(cx).item_count(), 0);

            // 发送用户消息：push + append(1)
            state.update(cx, |s, cx| assert!(s.append(1, cx)));
            assert_eq!(state.read(cx).item_count(), 1);

            // 首个 Delta：插入 assistant 占位行 到 append(1)
            state.update(cx, |s, cx| assert!(s.append(1, cx)));
            assert_eq!(state.read(cx).item_count(), 2);

            // 后续 Delta：行数不变，只重测末行
            state.update(cx, |s, cx| assert!(s.remeasure_items(1..2, cx)));
            assert_eq!(state.read(cx).item_count(), 2, "remeasure 不得改变行数");

            // 流结束后追加命令卡片所在行不变，但仍要能重测该行
            state.update(cx, |s, cx| assert!(s.remeasure_items(1..2, cx)));

            // 删除中间一条（气泡菜单） 到 splice 掉一行
            state.update(cx, |s, cx| assert!(s.splice(0..1, 0, cx)));
            assert_eq!(state.read(cx).item_count(), 1);

            // 清空会话 到 reset(0)
            state.update(cx, |s, cx| s.reset(0, cx));
            assert_eq!(state.read(cx).item_count(), 0);
        });
    }

    // ---------- 复制口径（2026-09-30） ----------

    /// 回归（2026-09-30 用户反馈「复制按钮只需要正常复制对应的消息」）：
    /// 用户消息的正文只留**用户自己问的那句**，注入的终端事实 / 终端输出 / `[用户]`
    /// 分隔行都不进正文 —— 而正文就是复制内容（两处同一个函数）。
    ///
    /// 三条判据各挡一种倒退：
    /// 1. 有终端上下文时剥干净（这正是用户看到「复制出来一大段我没问的东西」的成因）；
    /// 2. 没有终端上下文时包装只剩一行 `[用户]`，也必须剥掉
    ///    （旧的 `split_once("\n[用户]\n")` 在这里切不出来，会把 `[用户]` 原样显示）；
    /// 3. 助手消息的 Markdown 原文**不得**被改动（它是「整段拿走」时该有的样子）。
    #[test]
    fn user_message_body_drops_the_injected_context() {
        let with_ctx = "[终端] prod-01 · root@10.0.0.1\n\n\
                        [当前终端上下文（最近输出，可能截断）]\n/dev/sda1 100%\n\n\
                        [用户]\n磁盘满了吗";
        assert_eq!(
            message_body(&ChatMessage::user(1, with_ctx, 9)),
            "磁盘满了吗",
            "复制 / 显示都必须只给用户原话，不能带上注入的终端上下文。"
        );
        assert_eq!(
            message_body(&ChatMessage::user(2, "[用户]\nhi", 0)),
            "hi",
            "没有终端上下文时包装只剩 `[用户]` 一行，也必须剥掉。"
        );
        // 反例：中间那段终端输出自己就包含 `[用户]` 行 —— 取**最后**一个分隔行才切得对。
        let tricky = "[用户]\n这是终端里打印的字面量\n\n[用户]\n真正的问题";
        assert_eq!(
            message_body(&ChatMessage::user(3, tricky, 1)),
            "真正的问题",
            "终端输出里出现 `[用户]` 时，必须认最后一个分隔行（注入内容排在它之前）。"
        );

        let reply = ChatMessage::assistant(4, "**结论**：先清理日志。");
        assert_eq!(
            message_body(&reply),
            "**结论**：先清理日志。",
            "助手消息的 Markdown 原文必须原样保留（复制按钮给的是原文，不是渲染后的纯文本）。"
        );
    }

    /// 护栏：**显示**与**复制**必须是同一个函数的输出。
    ///
    /// 2026-09-30 修的就是这两个口径分叉（显示侧自己切了一次、复制侧直接拿
    /// `msg.content`）——「复制按钮只需要正常复制对应的消息」这句话的前提是，
    /// 界面上那条消息与剪贴板里那段文本是同一份东西。只断言「有 message_body」
    /// 不够：得把 `message_row` 体内**直接读 `msg.content` / 旧字段名**的路堵死。
    #[test]
    fn display_and_copy_share_one_body_source() {
        let code = production_source();
        let at = code.find("fn message_row(").expect("找不到 `message_row`");
        // 终点取下一个函数的**定义处**（切片含 message_row 整个函数体）。
        let end = code[at..]
            .find("fn trace_color(")
            .expect("找不到 `message_row` 之后的结构锚点");
        let row = &code[at..at + end];

        assert!(
            row.contains("let body = message_body(&msg);"),
            "message_row 里必须只用 `message_body(&msg)` 算正文（显示与复制同一个来源）。"
        );
        assert!(
            !row.contains("msg.content"),
            "message_row 里又出现了直接读 `msg.content` 的地方 —— 正文口径一旦就地再算一遍，\n\
             显示与复制就会重新分叉（这正是 2026-09-30 那个 bug）。"
        );
        assert!(
            !row.contains("split_once("),
            "正文的上下文剥离只允许在 `strip_context_wrapper` 里做一次，\
             别在渲染路径上再切一次。"
        );
        // 复制按钮与右键菜单都必须用 `body` 的拷贝（两个 move 闭包各一份）。
        assert!(
            row.contains("let copy_menu = body.clone();")
                && row.contains("let copy_click = body.clone();"),
            "复制按钮与右键菜单必须共用 `body` 的拷贝。"
        );
    }

    // ---------- 输入框高度（2026-09-30） ----------

    /// 给定输入条的高度，输入框**实际**能拿到的高度（px）—— 测试侧的换算入口。
    ///
    /// 口径是 [`AI_COMPOSER_FIXED_H`]（唯一源头：生产侧的 [`AI_COMPOSER_MIN_H`]
    /// 用的就是同一个常量）。生产代码用不到它 —— 面板高度由 `size_range` 管住，
    /// 这里只是把「面板高度」翻译成「输入框高度」好做断言。
    fn composer_input_height(composer_h: f32) -> f32 {
        composer_h - AI_COMPOSER_FIXED_H
    }

    /// 常量护栏：拖拽下限也必须装得下两行。
    ///
    /// 输入框拿到的高度 = 面板高度 − 固定开销（见 [`AI_COMPOSER_H`] 的推导）。
    /// 这条只算算术，不渲染 —— 它能挡住「有人把下限调回 72」这类改动的**前一半**
    /// （后一半由 `ai_composer_shows_at_least_two_lines` 真量像素）。
    #[test]
    fn composer_min_height_fits_two_lines() {
        let two_lines = 2. * AI_LINE_H;
        let floor = composer_input_height(AI_COMPOSER_MIN_H);
        let default_h = composer_input_height(AI_COMPOSER_H);
        assert!(
            floor >= two_lines,
            "拖拽下限 {AI_COMPOSER_MIN_H} 只给输入框 {floor}px（两行需要 {two_lines}px，\
             且这时 `ai_hint` 那行提示还挂着）—— 用户要求「保持有两行文字可以正常显示」，\
             下限不能低于两行。"
        );
        assert!(
            default_h >= floor,
            "默认高度（输入框 {default_h}px）比下限（{floor}px）还矮 —— 默认进来就是被压扁的。"
        );
        assert!(
            composer_input_height(AI_COMPOSER_MAX_H) > default_h,
            "上限比默认值还小：拖拽区间颠倒了。"
        );
    }

    /// 端到端几何：**真的渲染一次**，量出输入框拿到的高度，断言 ≥ 两行。
    ///
    /// 为什么必须真渲染：输入框不是「写死多高」，而是 `flex_1` 吃掉输入条的剩余空间
    /// —— 它的实际高度由「面板高度 − 根内边距 − 内层内边距 − 间距 − 工具行」五段算出，
    /// 其中工具行的高度来自官方按钮的尺寸档（Medium icon-only = 2rem）。任何一个环节
    /// 变了（改按钮尺寸、改 padding、改默认高度），屏幕上的输入框就会重新缩回一行半，
    /// 而常量断言完全看不见。2026-09-20 引入拖拽调高时正是这么掉回去的。
    ///
    /// 输入框自己挂不了 `debug_selector`（`Textarea` 只实现 `Styled`），所以量它的容器
    /// 与工具行，再按源码里那几个 rem 反推输入框高度。
    #[gpui::test]
    fn ai_composer_shows_at_least_two_lines(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;

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

        // 对话列展开（输入条在那一列里；`ai_passthrough` 会顺带把它带出来）。
        main.update_in(&mut vcx.clone(), |m, window, cx| {
            m.ai_ui.ai_cfg.base_url = "https://api.deepseek.com/v1".into();
            m.ai_ui.ai_cfg.model = "deepseek-flash".into();
            m.ai_ui.ai_cfg.api_key_cipher = Some("cipher".into());
            m.ai_passthrough("", window, cx);
            cx.notify();
        });
        vcx.run_until_parked();

        let rem = vcx.update(|_w, cx| f32::from(cx.theme().font_size));
        assert!(
            rem > 0.,
            "拿不到全局 rem 基准（`theme.font_size`）—— 下面按 rem 反推的换算都不成立。"
        );

        // 量一次输入框的可见高度：容器 = p_2 上下 + 输入框 + gap_2 + 工具行
        // （三处 rem 见 `composer`）。
        // `debug_bounds` 取的是 `&mut self`（内部要出帧），所以这里收 `&mut`。
        let measure_input = |vcx: &mut gpui::VisualTestContext, rem: f32| -> f32 {
            let box_bounds = vcx
                .debug_bounds("ai-composer-box")
                .expect("量不到输入条的容器 —— 对话列没渲染？");
            let tools_bounds = vcx
                .debug_bounds("ai-composer-tools")
                .expect("量不到输入条的工具行 —— 发送按钮没渲染？");
            f32::from(box_bounds.size.height)
                - 2. * 0.5 * rem
                - 0.5 * rem
                - f32::from(tools_bounds.size.height)
        };
        let two_lines = 2. * AI_LINE_H;

        // ① 无提示行的理想态。
        let input_h = measure_input(vcx, rem);
        assert!(
            input_h + 0.5 >= two_lines,
            "AI 输入框只有 {input_h}px 可见（一行 {AI_LINE_H}px，两行需要 {two_lines}px）：\n\
             输入条高度 {AI_COMPOSER_H} 减掉固定开销后不够两行 —— 用户要求「保持有两行文字\n\
             可以正常显示」。改 `AI_COMPOSER_H` / `AI_COMPOSER_MIN_H`，或别再放大工具行。"
        );

        // ② 挂上提示行再量一次：`ai_hint` 只写不清（一次「已保存」之后会一直挂在输入条
        //    下方），而它和输入框抢的是同一块高度 —— 这才是真实使用里的常态，
        //    也正是「改完常量、用户还是只看见一行半」最容易漏掉的一种。
        main.update_in(&mut vcx.clone(), |m, _w, cx| {
            m.ai_ui.ai_hint = Some(rustshell_core::i18n::t("已保存").into());
            cx.notify();
        });
        vcx.run_until_parked();
        let input_with_hint = measure_input(vcx, rem);

        // 校准：`composer_input_height` 是**最坏情况（含提示行）**的换算口径，所以只能拿
        // 「挂着提示行」的那次实测跟它比 —— 无提示行的那次天生多出一整行，比不得。
        // 这一步是为了保证下面「两行」的判断不是建在一个和真实排版脱节的基准上
        // （差一个 padding 就是半行字被切掉）。
        let computed = composer_input_height(AI_COMPOSER_H);
        assert!(
            (input_with_hint - computed).abs() <= 1.5,
            "挂提示行后量到的输入框高度 {input_with_hint}px 与常量换算出来的 {computed}px\n\
             对不上 —— `AI_COMPOSER_CHROME_Y` / `AI_TOOL_ROW_H` / `AI_HINT_ROW_H` 与\n\
             `composer` 里的实际排版已经脱节，先对齐这几组数字再看「两行」够不够。"
        );
        assert!(
            input_with_hint + 0.5 >= two_lines,
            "挂着 `ai_hint` 提示行（真实常态）时，AI 输入框只剩 {input_with_hint}px\n\
             （两行需要 {two_lines}px）—— 提示行把输入框压回了不到两行。\n\
             它挂在输入条的兄弟位置、从 `AI_COMPOSER_H` 里分走高度，必须一并算进下限\
             （见 `AI_HINT_ROW_H`）。"
        );

        // ③ 最后用实测反推 `AI_HINT_ROW_H` 有没有低估：常量护栏（`composer_min_height_fits_two_lines`）
        //    是拿这个常量算下限的，它若是小的，那条护栏算出来的「两行」就是假的 ——
        //    而它没法自己发现（纯算术看不见真实的提示行多高）。
        let real_hint = input_h - input_with_hint;
        assert!(
            AI_HINT_ROW_H + 1.0 >= real_hint,
            "`AI_HINT_ROW_H`（{AI_HINT_ROW_H}）低估了提示行的真实高度（{real_hint}）：\n\
             下限护栏会因此算出「两行够用」，实际拖到最小还是差一截。按实测把它调大。"
        );
    }

    // ---------- 展开内容的鼠标拖选（2026-10-09） ----------

    /// 用户要求：「run command 之类的 agent 工具展开内容支持鼠标进行选中」。
    ///
    /// # 为什么必须**真拖一遍**，而不是扫源码
    ///
    /// 「这段字能不能被选中」在界面上只表现为一件事：拖过去选得中，还是选不中。
    /// 用 `div().child(SharedString)` 画上去的字符串（改动前的写法）与注册进
    /// **窗口级选区**的 `SelectableText`，在编译、渲染、开面板、量高度上完全一样 ——
    /// 只有把真正的鼠标事件派发下去，这两者才分得开。
    ///
    /// 三条断言各挡一种倒退：
    ///
    /// 1. 拖完窗口级选区**非空** —— 明细没进选区（还是「画上去的字符串」）就红；
    /// 2. 选中内容里出现明细里的字 —— 选到的是**这一段**，不是旁边的正文；
    /// 3. 拖完那个折叠块**还开着** —— 这正是 2026-10-09 把热区从整行收到 head 的
    ///    理由：gpui 的点击判定**没有拖拽阈值**（按下与松开落在同一个 hitbox 上就发
    ///    `ClickEvent`），热区盖住整行的话，「按住 → 拖过正文 → 松开」会在选区刚
    ///    成型的同一刻把它折回去，用户看到的是「一拖就没了」。
    ///
    /// 装配与生产一致：`Root::new(AppShell)` —— 窗口级选区的挂载
    /// （`TextSelectionLayer`）与 `Ctrl+C` 都由这个 `Root` 提供，应用侧不额外装配。
    #[gpui::test]
    fn the_expanded_tool_detail_can_be_selected_with_the_mouse(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;
        use rustshell_core::agent::{TraceRow, TraceState};
        use rustshell_core::ai::ChatMessage;

        /// 明细正文取**纯 ASCII 单行**：拖选的起止点按像素给，混进宽字符的话
        /// 「拖到右端」究竟落在哪个字上会随字体度量漂移，断言就不稳了。
        const DETAIL: &str = "total 12 alpha bravo charlie";

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

        main.update_in(&mut vcx.clone(), |m, window, cx| {
            m.ai_ui.ai_cfg.base_url = "https://api.deepseek.com/v1".into();
            m.ai_ui.ai_cfg.model = "deepseek-flash".into();
            m.ai_ui.ai_cfg.api_key_cipher = Some("cipher".into());
            m.ai_passthrough("", window, cx);

            let mut reply = ChatMessage::assistant(1, "已执行。");
            let mut row = TraceRow::new(1, "c1", "run_command");
            row.summary = "prod-01 · ls -l".into();
            row.state = TraceState::Ok;
            row.note = "完成".into();
            row.detail = DETAIL.into();
            reply.trace = vec![row];
            m.ai_msgs_mut().push(reply);
            m.ai_ui.ai_scroller.update(cx, |s, cx| s.reset(1, cx));
            // 展开这一行：默认收起，展开才谈得上拖选。走真的是那条切换路径
            // （它顺带重测行高），不直接往 `ai_trace_expanded` 里塞。
            m.ai_toggle_trace_detail(1, "c1".into(), cx);
            cx.notify();
        });
        vcx.run_until_parked();

        // 出帧再量：`debug_bounds` 读的是**上一帧**画出来的坐标（铁律 #72）
        vcx.update(|window, cx| window.draw(cx).clear(cx));
        let detail_box = vcx
            .debug_bounds("ai-trace-detail-c1")
            .expect("明细展开后没有渲染出来 —— 前提不成立，后面的拖选无从谈起");
        assert!(
            f32::from(detail_box.size.width) > 40. && f32::from(detail_box.size.height) > 0.,
            "明细的盒子太小（{detail_box:?}）：拖选的起止点会落在同一个字上，\
             那样即便选中了也证明不了「拖得动」。"
        );

        // 一行之内从左拖到右。**垂直位置取盒子顶部下 6px，不取中线**：
        // `ai-trace-detail-*` 量出来的盒子是「明细 + 行首 head」整行的高度
        // （2026-10-19 实测：明细盒 34px 高、起点 128.5；head 的热区在 144.5..162.5，
        // 而盒子中线正好 145.5 —— 差 1px 就落在 head 里）。中线落在 head 上时，
        // 这一次拖选会先在 head 的 `on_click` 上把它折起来，选的却是「热区没盖住整行」
        // 这条用例本来要证伪的现象，报出来的原因还是错的。
        // 文本行本身在这一盒子的**顶上**，所以顶部下 6px 才是真的落在字上。
        let y = detail_box.origin.y + px(6.);
        let from = gpui::point(detail_box.origin.x + px(1.), y);
        let to = gpui::point(detail_box.origin.x + detail_box.size.width - px(1.), y);
        vcx.simulate_mouse_down(from, gpui::MouseButton::Left, gpui::Modifiers::default());
        vcx.simulate_mouse_move(to, gpui::MouseButton::Left, gpui::Modifiers::default());
        vcx.simulate_mouse_up(to, gpui::MouseButton::Left, gpui::Modifiers::default());

        // ① 先看折叠块还在不在。
        //
        // 顺序不能反：热区一旦盖回整行，拖完那一下就会把这块折起来，**选区也跟着
        // 一起没了**（参与者随元素一起被裁掉），于是「没选中」与「被折起来」两条
        // 断言会同时红在同一条根因上，报出来的原因还是错的。先判这一条，
        // 两个失效就各有各的落点。
        assert!(
            vcx.debug_bounds("ai-trace-detail-c1").is_some(),
            "拖选那一下把折叠块收起来了 —— 热区又盖回整行：\
             gpui 的点击判定没有拖拽阈值，按下与松开落在同一个 hitbox 上就会发 ClickEvent。"
        );

        // ② 再看选没选中、选中了什么
        let selected = vcx.update(gpui_base::TextSelection::selected_text);
        let picked = selected.trim();
        assert!(
            !picked.is_empty(),
            "拖过明细一个字符都没选中 —— 这段文字还只是「画上去的字符串」，\
             没有注册进窗口级选区（`SelectableText` 的注册在 prepaint 里，\
             盒子被裁掉 / 没画到都会静默地不注册）。"
        );
        assert!(
            DETAIL.contains(picked) && picked.contains("charlie"),
            "选中的是 `{picked}`，不是明细里的字 —— 拖选落到了旁边的正文上。"
        );
        // 起点贴着左端、终点贴着右端，所以应当正好是整行
        assert_eq!(
            picked, DETAIL,
            "从左端拖到右端应当选中**整行**明细，实际只选中 `{picked}`。"
        );
    }
}
