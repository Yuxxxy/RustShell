//! 快捷命令栏（v3.1，2026-09-14 用户调整）：底部条 + 展开抽屉 + 分组 Tab 页。

use gpui::{
    div, px, AnyElement, AppContext as _, Context, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _,
};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    h_flex,
    input::Input,
    menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem},
    scroll::ScrollableElement as _,
    v_flex, Icon, IconName, Sizable as _,
};

use super::main_view::MainView;

/// 折叠态栏高
pub const QUICK_BAR_H: f32 = 40.;
/// 展开态栏高
pub const QUICK_BAR_H_EXPANDED: f32 = 280.;
/// 折叠态最多渲染的胶囊数（，超出只在展开态可见）
const PILL_CAP: usize = 60;

/// N8：样式取色统一走主题工具函数（删除就地 hex 定义）
use crate::theme::to_hsla as hex;

/// 通过 UiHub 把操作转回 MainView。
fn with_main(
    w: &mut gpui::Window,
    cx: &mut gpui::App,
    f: impl FnOnce(&mut MainView, &mut gpui::Window, &mut gpui::Context<MainView>),
) {
    let Some(hub) = cx.try_global::<super::main_view::UiHub>() else {
        return;
    };
    let main = hub.main.clone();
    if main.update(cx, |m, cx| f(m, w, cx)).is_err() {
        log::warn!("quick-commands 动作未能路由到 MainView（实体已释放）");
    }
}

// 分组拖拽重排：拖动分组行到另一分组上完成换位

/// 拖拽载荷：被拖动的分组 id 与名称
#[derive(Clone)]
struct QcFolderDrag {
    folder_id: String,
    name: String,
}

/// 拖拽跟随幻影
struct DragGhost {
    label: SharedString,
}

impl Render for DragGhost {
    fn render(&mut self, _window: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = crate::theme::current(cx);
        div()
            .px_2()
            .py_1()
            .rounded(px(4.))
            .bg(hex(t.accent))
            .text_color(hex(t.on_accent))
            .text_size(px(12.))
            .child(self.label.clone())
    }
}

/// 瞬时提示的**浮层**形态：绝对定位，不参与布局。
///
/// 旧实现把提示当成一行（折叠态替换胶囊、展开态独占一行），代价是把命令区
/// 顶下去/挤走 —— 用户 2026-09-16 要求「占用栏空间的提示改为弹出框」。
/// 这里改为覆盖在内容之上的浮动卡片，2s 后由 `clear_quick_toast` 收走。
fn toast_overlay(kind: crate::notice::NoticeKind, msg: SharedString, top: f32) -> AnyElement {
    div()
        .absolute()
        .top(px(top))
        .left(px(8.))
        .right(px(8.))
        .child(kind.card("qc-toast", msg).xsmall().text_size(px(11.)))
        .into_any_element()
}

// 渲染快照

/// 菜单构建所需的只读快照。
#[derive(Clone, Default)]
struct QcSnapshot {
    /// 命令所在分组的位置与长度（右键「上移/下移」置灰依据）
    cmd_pos: Option<(usize, usize)>,
    /// 某个分组的位置与总分组数（分组右键置灰依据）
    group_pos: (usize, usize),
    /// 除当前分组外的其它分组（「移动到 」子菜单）
    other_folders: Vec<(String, String)>,
    /// 当前可投递 Tab 数（右键「发送到全部终端」的显示条件：>=2 才有广播意义）
    sendable: usize,
}

// 栏主体

pub fn render(m: &mut MainView, cx: &mut Context<MainView>) -> impl IntoElement {
    let expanded = m.settings.quick_bar_expanded;

    if expanded {
        render_expanded(m, cx).into_any_element()
    } else {
        render_collapsed(m, cx).into_any_element()
    }
}

fn snapshot_for_folder(m: &MainView, folder_id: &str) -> QcSnapshot {
    let group_pos = (
        m.quick_store
            .folders
            .iter()
            .position(|f| f.id == folder_id)
            .unwrap_or(0),
        m.quick_store.folders.len(),
    );
    let other_folders = m
        .quick_store
        .folders
        .iter()
        .filter(|f| f.id != folder_id)
        .map(|f| (f.id.clone(), f.name.clone()))
        .collect();
    QcSnapshot {
        cmd_pos: None,
        group_pos,
        other_folders,
        sendable: quick_sendable_count(m),
    }
}

/// 当前可投递 Tab 数（供右键菜单判定「发送到全部终端」是否显示）
fn quick_sendable_count(m: &MainView) -> usize {
    m.quick_target_labels()
        .iter()
        .filter(|(_, _, ok)| *ok)
        .count()
}

fn snapshot_for_cmd(m: &MainView, folder_id: &str, cmd_id: &str) -> QcSnapshot {
    let mut snap = snapshot_for_folder(m, folder_id);
    snap.cmd_pos = m.quick_cmd_position(folder_id, cmd_id);
    snap
}

/// 折叠态：40px 胶囊条（无图标、无目标选择器）
fn render_collapsed(m: &mut MainView, cx: &mut Context<MainView>) -> AnyElement {
    let t = crate::theme::current(cx);
    // 瞬时提示：有则整条命令区换成提示
    let kind = m.ai_ui.quick_toast_kind;
    let toast: Option<SharedString> = m.ai_ui.quick_toast.clone().map(SharedString::from);

    // 扁平化命令快照（分组 id, 分组名, 命令），供胶囊与菜单共用
    let flat: Vec<(String, String, rustshell_core::quickcmd::QuickCmd)> = m
        .quick_store
        .folders
        .iter()
        .flat_map(|f| {
            f.commands
                .iter()
                .map(move |c| (f.id.clone(), f.name.clone(), c.clone()))
        })
        .collect();

    let mut pills = h_flex().flex_1().min_w_0().items_center().gap_1().px_1();
    if flat.is_empty() {
        pills = pills.child(
            div()
                .px_2()
                .text_size(px(11.))
                .text_color(hex(t.fg_muted))
                .child(rustshell_core::i18n::t("暂无快捷命令")),
        );
    }
    for (fid, fname, cmd) in flat.iter().take(PILL_CAP) {
        let snap = snapshot_for_cmd(m, fid, &cmd.id);
        pills = pills.child(command_pill(fid.clone(), fname, cmd, snap, cx));
    }
    let hidden = flat.len().saturating_sub(PILL_CAP);
    if hidden > 0 {
        pills = pills.child(
            div()
                .px_2()
                .flex_shrink_0()
                .text_size(px(10.))
                .text_color(hex(t.fg_muted))
                .child(SharedString::from(rustshell_core::i18n::ts(
                    format!("+{hidden}"),
                    format!("+{hidden}"),
                ))),
        );
    }

    let mut root = v_flex()
        .h(px(QUICK_BAR_H))
        .flex_shrink_0()
        .justify_center()
        .border_t_1()
        .border_color(hex(t.border_subtle))
        .bg(crate::theme::surface(1.0, t.bg_base))
        .child(
            h_flex()
                .flex_1()
                .min_h_0()
                .items_center()
                .gap_2()
                .px_2()
                // 胶囊横向滚动（：命令多时不换行、不挤压）
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(pills.overflow_x_scrollbar())
                        .into_any_element(),
                )
                .child(
                    Button::new("qc-expand")
                        .small()
                        .ghost()
                        .icon(IconName::ChevronUp)
                        .tooltip(rustshell_core::i18n::t("展开"))
                        .on_click(cx.listener(|m, _e, w, cx| {
                            m.set_quick_bar_expanded(true, w, cx);
                        })),
                ),
        );
    // 未执行原因：浮层，不占布局高度（不再把胶囊挤走）
    if let Some(msg) = toast {
        root = root.child(toast_overlay(kind, msg, 6.));
    }
    root.into_any_element()
}

/// 展开态：命令输入框 + 分组 Tab 页 + 当前分组命令列表
fn render_expanded(m: &mut MainView, cx: &mut Context<MainView>) -> AnyElement {
    let t = crate::theme::current(cx);
    let kind = m.ai_ui.quick_toast_kind;
    let toast: Option<SharedString> = m.ai_ui.quick_toast.clone().map(SharedString::from);
    let folders = m.quick_store.folders.clone();
    let active_folder = m.quick_active_folder_resolved();

    // 删除分组的确认已改走官方 `AlertDialog` 浮动弹层（见
    // `MainView::confirm_remove_quick_folder`），此处不再渲染栏内确认条 ——
    // 那种做法会把命令列表整体顶下去。

    // 顶部工具栏：搜索过滤（最左）+ 命令输入框 + 发送到全部终端 + 收起。
    //
    // 2026-09-15 用户调整：取消「假输入外观 / 编辑态」两态切换，四件东西**常驻**。
    // 两态切换有三个后果，都是用户报的问题：① 输入框不是真输入框时回车无处可发；
    // ② 失焦后外观退回那个「点击即编辑」的假框，看起来像"切回了搜索命令"，
    // 且假框只显示占位符、草稿看不见，主观上就像输入被吞了；③ 发送按钮只在编辑态出现。
    // 现在命令输入框始终是真实 `Input`，回车即发送（订阅见 `main_view.rs`），
    // 发送按钮始终可见。
    let toolbar = h_flex()
        .flex_shrink_0()
        .items_center()
        .gap_2()
        .px_2()
        .py_1()
        .border_b_1()
        .border_color(hex(t.border_subtle))
        // 搜索命令：固定宽，置于最左侧
        .child(
            div()
                .w(px(150.))
                .flex_shrink_0()
                // 点框外失焦（2026-09-18 用户要求「搜索命令和输入命令栏…聚焦后，
                // 点击别的地方需要进行失焦」）：官方 `Input` 只在点到**另一个可聚焦
                // 元素**时才换焦点，点终端/侧栏空白处不会松手 —— 光标一直闪、键盘
                // 输入也继续被这个框吃掉。实现在 `main_view::blur_input_on_outside_click`
                // （捕获阶段触发、且只在焦点真在本框上时才收）。
                .on_mouse_down_out(cx.listener(|m, _e, w, cx| {
                    super::main_view::blur_input_on_outside_click(&m.ai_ui.quick_search, w, cx)
                }))
                .child(Input::new(&m.ai_ui.quick_search).small()),
        )
        // 命令输入框：吃掉剩余宽度
        .child(
            div()
                .flex_1()
                .min_w_0()
                // 同上：失焦只放掉焦点，草稿留在框里（失焦不做任何处理，
                // 见 `main_view.rs` 里对 `quick_compose_input` 的订阅说明）。
                .on_mouse_down_out(cx.listener(|m, _e, w, cx| {
                    super::main_view::blur_input_on_outside_click(
                        &m.ai_ui.quick_compose_input,
                        w,
                        cx,
                    )
                }))
                .child(Input::new(&m.ai_ui.quick_compose_input).small()),
        )
        .child(
            Button::new("qc-compose-send")
                .small()
                .label(rustshell_core::i18n::t("发送到全部终端"))
                .on_click(cx.listener(|m, _e, w, cx| {
                    m.send_compose_to_all_tabs(w, cx);
                })),
        )
        .child(
            Button::new("qc-collapse")
                .small()
                .ghost()
                .icon(IconName::ChevronDown)
                .tooltip(rustshell_core::i18n::t("收起"))
                .on_click(cx.listener(|m, _e, w, cx| {
                    m.set_quick_bar_expanded(false, w, cx);
                })),
        );

    // 分组 Tab 行（横向滚动；点击切换；右键菜单；拖拽换位）
    let mut tabs = h_flex()
        .flex_shrink_0()
        .items_center()
        .gap_1()
        .px_2()
        .py_1()
        .w_full()
        .min_w_0();

    let query = m.ai_ui.quick_search.read(cx).value().trim().to_lowercase();
    if folders.is_empty() {
        tabs = tabs.child(
            div()
                .flex_1()
                .min_w_0()
                .text_size(px(11.))
                .text_color(hex(t.fg_muted))
                .child(rustshell_core::i18n::t("暂无快捷命令")),
        );
    } else {
        for folder in &folders {
            let fid = folder.id.clone();
            let is_active = active_folder.as_deref() == Some(fid.as_str());
            let name: SharedString = folder.name.clone().into();
            let fid_click = fid.clone();
            let fid_slot = fid.clone();
            let fid_drop = fid.clone();
            // 该分组是否处于行内重命名态（决定渲染输入框还是名称）
            let is_renaming = m.ai_ui.quick_renaming.as_deref() == Some(fid.as_str());

            let tab = div()
                .id(SharedString::from(format!("qc-tab-{fid}")))
                .flex_shrink_0()
                .px_2()
                .py_0p5()
                .rounded(px(4.))
                .text_size(px(12.))
                // 栏底色已是 `bg_base`：选中的 Tab 抬到 `bg_elevated` 才分得出来，
                // 未选中的 Tab 与栏同色（2026-10-22 内外分层调整）。
                .bg(if is_active {
                    hex(t.bg_elevated)
                } else {
                    hex(t.bg_base)
                })
                .border_b_2()
                .border_color(if is_active {
                    hex(t.accent)
                } else {
                    hex(t.border_subtle)
                })
                .text_color(if is_active {
                    hex(t.fg_primary)
                } else {
                    hex(t.fg_muted)
                })
                .hover(|s| s.bg(hex(t.bg_elevated)))
                .on_click(cx.listener(move |m, _e, _w, cx| {
                    m.set_quick_active_folder(&fid_click, cx);
                }))
                .context_menu({
                    let folder = folder.clone();
                    let snap = snapshot_for_folder(m, &fid);
                    move |menu: PopupMenu, w, cx| folder_menu(menu, &folder, w, cx, &snap)
                })
                .child(if is_renaming {
                    // 修复「右键重命名无效」：v3.1 初版把分组列表改成 Tab 页时，
                    // 漏掉了 quick_renaming 的渲染路径：begin_folder_rename 只是
                    // 设状态 + 抢焦点 + notify，界面上什么都不出现。
                    // 现在在对应 Tab 位置就地渲染重命名输入框（Enter / 失焦提交，见
                    // main_view.rs 里对该 InputState 的 subscribe_in）。
                    div()
                        .w(px(120.))
                        .flex_shrink_0()
                        .child(Input::new(&m.ai_ui.quick_rename_input).xsmall())
                        .into_any_element()
                } else {
                    // 2026-09-30 用户要求「命令 tab 上不需要显示命令数量」：原先名称
                    // 后面还跟一枚 10px 的灰数字（`folder.commands.len()`）。撤掉的理由
                    // 与连接分组去掉成员数徽标同源 —— 这排 Tab 是「切到哪一组」的开关，
                    // 数字回答的是「那一组里有多少条」，不是当前这一屏需要决定的事；
                    // 而它每次都占着 Tab 的实际宽度（分组一多就得横向滚）。
                    // 真要数，切过去列表就在眼前。
                    h_flex()
                        .items_center()
                        .gap_1()
                        .child(name)
                        .into_any_element()
                });

            let slot = div()
                .id(SharedString::from(format!("qc-slot-{fid_slot}")))
                .flex_shrink_0()
                .on_drag(
                    QcFolderDrag {
                        folder_id: fid.clone(),
                        name: folder.name.clone(),
                    },
                    |value: &QcFolderDrag, _pos, _w, cx| {
                        let name = value.name.clone();
                        cx.new(|_| DragGhost { label: name.into() })
                    },
                )
                .drag_over::<QcFolderDrag>(|style, _v, _w, cx| {
                    style
                        .border_color(hex(crate::theme::current(cx).accent))
                        .rounded(px(4.))
                        .border_1()
                })
                .on_drop::<QcFolderDrag>(move |value, w, cx| {
                    let from = value.folder_id.clone();
                    let to = fid_drop.clone();
                    with_main(w, cx, move |m, _w, cx| {
                        m.reorder_quick_folder(&from, &to, cx);
                    });
                })
                .child(tab);

            tabs = tabs.child(slot);
        }
    }

    // 分组栏尾部的「+」：新建分组（创建后立刻进入行内重命名态，见
    // `add_quick_folder`）。放在 Tab 行末尾而不是工具栏里，语义上它属于「分组」
    // 这一层；无分组时也渲染，否则第一次建组没有入口（空态另有向导按钮）。
    tabs = tabs.child(
        Button::new("qc-folder-add")
            .xsmall()
            .ghost()
            .icon(IconName::Plus)
            .tooltip(rustshell_core::i18n::t("新建分组"))
            .on_click(cx.listener(|m, _e, w, cx| {
                m.add_quick_folder(w, cx);
            })),
    );

    // 当前分组的命令列表
    let mut list = v_flex().w_full().gap_0p5().px_1().py_1();
    let current = active_folder
        .as_ref()
        .and_then(|id| folders.iter().find(|f| &f.id == id));

    match current {
        None if folders.is_empty() => {
            list = list.child(
                v_flex()
                    .w_full()
                    .items_center()
                    .py_6()
                    .gap_2()
                    .child(
                        Icon::new(IconName::Folder)
                            .large()
                            .text_color(hex(t.fg_muted)),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(hex(t.fg_muted))
                            .child(rustshell_core::i18n::t("暂无快捷命令")),
                    )
                    .child(
                        Button::new("qc-empty-new")
                            .small()
                            .label(rustshell_core::i18n::t("新建命令"))
                            .on_click(cx.listener(|m, _e, w, cx| {
                                m.quick_new_command_dialog(w, cx);
                            })),
                    ),
            );
        }
        None => {
            list = list.child(
                div()
                    .px(px(2.))
                    .py_1()
                    .text_size(px(11.))
                    .text_color(hex(t.fg_muted))
                    .child(rustshell_core::i18n::t("尚未新建分组")),
            );
        }
        Some(folder) => {
            let fid = folder.id.clone();
            // 搜索过滤（保留原 QB-FIND-01 能力）：命中分组名不过滤，否则按命令过滤
            let group_hit = !query.is_empty() && folder.name.to_lowercase().contains(&query);
            let matched: Vec<rustshell_core::quickcmd::QuickCmd> = if query.is_empty() || group_hit
            {
                folder.commands.clone()
            } else {
                folder
                    .commands
                    .iter()
                    .filter(|c| {
                        c.label.to_lowercase().contains(&query)
                            || c.script.to_lowercase().contains(&query)
                    })
                    .cloned()
                    .collect()
            };
            if matched.is_empty() {
                list = list.child(
                    div()
                        .px(px(2.))
                        .py_1()
                        .text_size(px(11.))
                        .text_color(hex(t.fg_muted))
                        .child(rustshell_core::i18n::t("暂无命令")),
                );
            }
            for cmd in &matched {
                let snap = snapshot_for_cmd(m, &fid, &cmd.id);
                list = list.child(command_row(fid.clone(), cmd, snap, cx));
            }
        }
    }

    let mut root = v_flex()
        .h(px(QUICK_BAR_H_EXPANDED))
        .flex_shrink_0()
        .border_t_1()
        .border_color(hex(t.border_subtle))
        .bg(crate::theme::surface(1.0, t.bg_base))
        .child(toolbar)
        .child(div().w_full().child(tabs.overflow_x_scrollbar()))
        .child(
            // 当前分组的命令列表：纵向滚动
            div()
                .flex_1()
                .min_h_0()
                .w_full()
                .child(list.overflow_y_scrollbar()),
        );
    // 未执行原因：浮层（不占布局行，故不会把命令列表顶下去；也不经手输入框，
    // 故不会顶掉草稿）。`top(44.)` 落在工具栏下沿。
    if let Some(msg) = toast {
        root = root.child(toast_overlay(kind, msg, 44.));
    }
    root.into_any_element()
}

// 命令胶囊 / 命令行

/// 折叠态命令胶囊：标签（已去图标），左键按目标执行、右键命令菜单
fn command_pill(
    folder_id: String,
    _folder_name: &str,
    cmd: &rustshell_core::quickcmd::QuickCmd,
    snap: QcSnapshot,
    cx: &mut Context<MainView>,
) -> AnyElement {
    let t = crate::theme::current(cx);
    let cid = cmd.id.clone();
    let cmd_menu = cmd.clone();
    let label: SharedString = cmd.label.clone().into();
    // tooltip 只显示脚本首行，避免超长脚本把提示撑爆（9 节「超长脚本」）
    let tip: SharedString = cmd
        .script
        .lines()
        .next()
        .unwrap_or_default()
        .chars()
        .take(80)
        .collect::<String>()
        .into();

    div()
        .id(SharedString::from(format!("qc-pill-{cid}")))
        .flex_shrink_0()
        .max_w(px(140.))
        .px_2()
        .py_1()
        .rounded(px(4.))
        .bg(hex(t.bg_base))
        .border_1()
        .border_color(hex(t.border_subtle))
        .hover(|s| s.bg(hex(t.bg_elevated)).border_color(hex(t.accent)))
        .flex()
        .items_center()
        .gap_1()
        .tooltip(move |_w, cx| gpui_component::tooltip::Tooltip::new(tip.clone()).build(_w, cx))
        // 命令按钮不再带图标（用户要求「命令按钮取消图标」）
        .child(
            div()
                .min_w_0()
                .truncate()
                .text_size(px(12.))
                .text_color(hex(t.fg_primary))
                .child(label),
        )
        // 左键 = 按当前目标执行
        .on_click(cx.listener(move |m, _e, w, cx| {
            m.execute_quick_cmd(&cid, w, cx);
        }))
        .context_menu(move |menu: PopupMenu, w, cx| {
            command_menu(menu, &folder_id, &cmd_menu, w, cx, &snap)
        })
        .into_any_element()
}

/// 展开态命令行：标签 + 脚本摘要（2026-09-14：去掉图标）
fn command_row(
    folder_id: String,
    cmd: &rustshell_core::quickcmd::QuickCmd,
    snap: QcSnapshot,
    cx: &mut Context<MainView>,
) -> AnyElement {
    let t = crate::theme::current(cx);
    let cid = cmd.id.clone();
    let cmd_menu = cmd.clone();
    let label: SharedString = cmd.label.clone().into();
    let script_line: SharedString = cmd
        .script
        .lines()
        .next()
        .unwrap_or_default()
        .chars()
        .take(120)
        .collect::<String>()
        .into();

    h_flex()
        .id(SharedString::from(format!("qc-row-{cid}")))
        .w_full()
        .items_center()
        .gap_2()
        .px_2()
        .py_0p5()
        .rounded(px(4.))
        .hover(|s| s.bg(hex(t.bg_elevated)))
        .on_click(cx.listener(move |m, _e, w, cx| {
            m.execute_quick_cmd(&cid, w, cx);
        }))
        .context_menu(move |menu: PopupMenu, w, cx| {
            command_menu(menu, &folder_id, &cmd_menu, w, cx, &snap)
        })
        .child(
            div()
                .flex_shrink_0()
                .max_w(px(160.))
                .truncate()
                .text_size(px(12.))
                .text_color(hex(t.fg_primary))
                .child(label),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(px(11.))
                .font_family(rustshell_core::theme::default_terminal_font())
                .text_color(hex(t.fg_muted))
                .child(script_line),
        )
        .into_any_element()
}

// 右键菜单（需求文档 6 节）

/// 命令项菜单：8 项，删除置于末位并以 separator 隔离
fn command_menu(
    menu: PopupMenu,
    fid: &str,
    cmd: &rustshell_core::quickcmd::QuickCmd,
    w: &mut gpui::Window,
    cx: &mut gpui::Context<PopupMenu>,
    snap: &QcSnapshot,
) -> PopupMenu {
    let folder_id = fid;
    let cid = cmd.id.clone();
    let menu = menu;

    // 1. 执行
    let cid_run = cid.clone();
    let mut menu = menu.item(
        PopupMenuItem::new(rustshell_core::i18n::t("执行"))
            .icon(IconName::Play)
            .on_click(move |_, w, cx| {
                let cid = cid_run.clone();
                with_main(w, cx, move |m, w, cx| m.execute_quick_cmd(&cid, w, cx));
            }),
    );

    // 2. 发送到全部终端（仅当存在 >=2 个可投递 Tab：只有一个时与「执行」等价）
    let multi = snap.sendable;
    if multi >= 2 {
        let cid_send = cid.clone();
        menu = menu.item(
            PopupMenuItem::new(rustshell_core::i18n::t("发送到全部终端"))
                .icon(IconName::Globe)
                .on_click(move |_, w, cx| {
                    let cid = cid_send.clone();
                    with_main(w, cx, move |m, w, cx| m.execute_quick_cmd_all(&cid, w, cx));
                }),
        );
    }

    // 3. 编辑...
    let (fid_edit, cid_edit) = (folder_id.to_string(), cid.clone());
    menu = menu.item(
        PopupMenuItem::new(rustshell_core::i18n::t("编辑…"))
            .icon(IconName::Copy)
            .on_click(move |_, w, cx| {
                let fid = fid_edit.clone();
                let cid = cid_edit.clone();
                with_main(w, cx, move |m, w, cx| m.open_qc_edit(fid, cid, w, cx));
            }),
    );

    // 4. 复制一份
    let cid_dup = cid.clone();
    menu = menu.item(
        PopupMenuItem::new(rustshell_core::i18n::t("复制命令"))
            .icon(IconName::Copy)
            .on_click(move |_, w, cx| {
                let cid = cid_dup.clone();
                with_main(w, cx, move |m, _w, cx| m.duplicate_quick_cmd(&cid, cx));
            }),
    );

    // 5. 复制脚本
    let script = cmd.script.clone();
    menu = menu.item(
        PopupMenuItem::new(rustshell_core::i18n::t("复制脚本"))
            .icon(IconName::Copy)
            .on_click(move |_, _w, cx| {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(script.clone()))
            }),
    );

    menu = menu.separator();

    // 6. 上移 / 下移（首/末项对应置灰）
    let (idx, len) = snap.cmd_pos.unwrap_or((0, 1));
    let fid_up = folder_id.to_string();
    let cid_up = cid.clone();
    menu = menu.item(
        PopupMenuItem::new(rustshell_core::i18n::t("上移"))
            .icon(IconName::ArrowUp)
            .disabled(idx == 0)
            .on_click(move |_, w, cx| {
                let fid = fid_up.clone();
                let cid = cid_up.clone();
                with_main(w, cx, move |m, _w, cx| m.move_quick_cmd(&fid, &cid, -1, cx));
            }),
    );
    let fid_dn = folder_id.to_string();
    let cid_dn = cid.clone();
    menu = menu.item(
        PopupMenuItem::new(rustshell_core::i18n::t("下移"))
            .icon(IconName::ArrowDown)
            .disabled(len == 0 || idx + 1 >= len)
            .on_click(move |_, w, cx| {
                let fid = fid_dn.clone();
                let cid = cid_dn.clone();
                with_main(w, cx, move |m, _w, cx| m.move_quick_cmd(&fid, &cid, 1, cx));
            }),
    );

    // 7. 移动到 <其它分组>（分组数 >= 2 时显示）
    let others: Vec<(String, String)> = snap.other_folders.clone();
    if !others.is_empty() {
        let cid_mv = cid.clone();
        menu = menu.submenu(
            rustshell_core::i18n::t("移动到"),
            w,
            cx,
            move |mut sub, _w, _cx| {
                for (tfid, tname) in &others {
                    let tfid = tfid.clone();
                    let cid = cid_mv.clone();
                    let label: SharedString = tname.clone().into();
                    sub = sub.item(PopupMenuItem::new(label).on_click(move |_, w, cx| {
                        let tfid = tfid.clone();
                        let cid = cid.clone();
                        with_main(w, cx, move |m, _w, cx| m.move_quick_cmd_to(&cid, &tfid, cx));
                    }));
                }
                sub
            },
        );
    }

    menu = menu.separator();

    // 8. 删除（末位、隔离；不做二次确认）
    let cid_del = cid.clone();
    menu.item(
        PopupMenuItem::new(rustshell_core::i18n::t("删除"))
            .icon(IconName::Delete)
            .on_click(move |_, w, cx| {
                let cid = cid_del.clone();
                with_main(w, cx, move |m, _w, cx| m.delete_quick_cmd(&cid, cx));
            }),
    )
}

/// 分组项菜单：6 组动作，删除分组置末位并二次确认
fn folder_menu(
    menu: PopupMenu,
    folder: &rustshell_core::quickcmd::CommandFolder,
    _w: &mut gpui::Window,
    _cx: &mut gpui::Context<PopupMenu>,
    snap: &QcSnapshot,
) -> PopupMenu {
    let menu = menu;
    let fid = folder.id.clone();

    // 1. 新建命令
    let fid_new = fid.clone();
    let mut menu = menu.item(
        PopupMenuItem::new(rustshell_core::i18n::t("新建命令"))
            .icon(IconName::Plus)
            .on_click(move |_, w, cx| {
                let fid = fid_new.clone();
                with_main(w, cx, move |m, w, cx| m.open_qc_new(fid, w, cx));
            }),
    );

    // 2. 重命名（进入行内重命名态，QB-GRP-02）
    let fid_rn = fid.clone();
    let cur = folder.name.clone();
    menu = menu.item(
        PopupMenuItem::new(rustshell_core::i18n::t("重命名"))
            .icon(IconName::Copy)
            .on_click(move |_, w, cx| {
                let fid = fid_rn.clone();
                let cur = cur.clone();
                with_main(w, cx, move |m, w, cx| {
                    m.begin_folder_rename(&fid, &cur, w, cx)
                });
            }),
    );

    // 3. 复制分组
    let fid_dup = fid.clone();
    menu = menu.item(
        PopupMenuItem::new(rustshell_core::i18n::t("复制分组"))
            .icon(IconName::Copy)
            .on_click(move |_, w, cx| {
                let fid = fid_dup.clone();
                with_main(w, cx, move |m, _w, cx| m.duplicate_quick_folder(&fid, cx));
            }),
    );

    // （原「展开全部 / 收起全部」两项已按用户要求移除：
    // Tab 页形态下每个分组天然只呈现当前一个，批量展开/收起已无意义。）

    menu = menu.separator();

    // 5. 上移 / 下移（分组级）
    let (gidx, glen) = snap.group_pos;
    let fid_u = fid.clone();
    menu = menu.item(
        PopupMenuItem::new(rustshell_core::i18n::t("上移"))
            .icon(IconName::ArrowUp)
            .disabled(gidx == 0)
            .on_click(move |_, w, cx| {
                let fid = fid_u.clone();
                with_main(w, cx, move |m, _w, cx| {
                    m.move_quick_folder_step(&fid, -1, cx)
                });
            }),
    );
    let fid_d = fid.clone();
    menu = menu.item(
        PopupMenuItem::new(rustshell_core::i18n::t("下移"))
            .icon(IconName::ArrowDown)
            .disabled(glen == 0 || gidx + 1 >= glen)
            .on_click(move |_, w, cx| {
                let fid = fid_d.clone();
                with_main(w, cx, move |m, _w, cx| {
                    m.move_quick_folder_step(&fid, 1, cx)
                });
            }),
    );

    menu = menu.separator();

    // 6. 删除分组（二次确认走官方 AlertDialog 浮动弹层，见 QB-GRP-03）
    // 确认框不再占命令栏空间：旧实现把确认条渲染在栏内，会把命令列表顶下去。
    let name = folder.name.clone();
    let count = folder.commands.len();
    menu.item(
        PopupMenuItem::new(rustshell_core::i18n::t("删除分组"))
            .icon(IconName::Delete)
            .on_click(move |_, w, cx| {
                let fid = fid.clone();
                let name = name.clone();
                with_main(w, cx, move |m, w, cx| {
                    m.confirm_remove_quick_folder(&fid, &name, count, w, cx)
                });
            }),
    )
}

#[cfg(test)]
mod tests {
    use crate::test_support::prod;

    /// 回归（2026-09-30 用户要求「命令 tab 上不需要显示命令数量」）：
    /// 分组 Tab 里不得再渲染命令条数。
    ///
    /// 判据落在**渲染出来的那个数字**上：Tab 里那枚徽标是唯一一处把
    /// `commands.len()` 转成字符串显示的地方（`folder_menu` 里也取了长度，但那是
    /// 交给删除确认框的**计数参数**，不是界面上的数字）。所以「tab 里不许出现
    /// `count.to_string()`」这一条恰好只挡徽标，不误伤确认框。
    #[test]
    fn folder_tab_shows_no_command_count() {
        let code = prod(include_str!("quick_commands_bar.rs"));
        assert!(
            !code.contains("count.to_string()"),
            "分组 Tab 上又出现了命令数量徽标 —— 用户已要求去掉。"
        );
        // 反向：折叠态胶囊区的「+N（还有多少条没显示）」不是 Tab 徽标，必须还在 ——
        // 它是「这里的胶囊没显示全」的唯一提示，撤掉会让用户以为命令丢了。
        assert!(
            code.contains("+{hidden}"),
            "折叠态「还有 N 条没显示」的提示被一起删掉了：那不是 Tab 徽标，\
             撤掉它用户会以为命令丢了。"
        );
    }

    /// 2026-10-20 用户要求「命令按钮右键-发送到xxx改为【发送到全部终端】」：
    /// 右键的发送项必须直接广播给所有可投递 Tab，不能再走按目标解析的旧入口。
    #[test]
    fn command_context_menu_broadcasts_to_every_terminal() {
        let code = prod(include_str!("quick_commands_bar.rs"));
        let start = code.find("fn command_menu(").expect("找不到命令右键菜单");
        let end = code[start..]
            .find("fn folder_menu(")
            .map(|i| start + i)
            .expect("找不到分组右键菜单（命令菜单的结束锚点）");
        let seg = &code[start..end];
        assert!(
            seg.contains("发送到全部终端") && seg.contains("execute_quick_cmd_all"),
            "命令右键的发送项必须是「发送到全部终端」，并调用广播入口 `execute_quick_cmd_all`。"
        );
        assert!(
            !seg.contains("open_quick_send_to") && !seg.contains("发送到…"),
            "按目标记忆/当前 Tab 解析的旧「发送到…」入口必须撤掉 —— \
             用户要的是一键发给全部终端。"
        );
    }
}
