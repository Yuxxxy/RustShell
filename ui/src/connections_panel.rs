//! 连接树侧栏（3.2 节 连接面板 / 5.1 节 连接管理）：官方 gpui-component Tree/TreeState（展开/选中/键盘导航由组件持有），分组 + 服务器行、状态点、右键菜单（6.3 节/6.4 节）、双击连接；新增连接入口在**栏右侧的图标按钮**（2026-09-17 由底部整条按钮改来）。

use std::collections::HashMap;

use gpui::{
    div, prelude::FluentBuilder as _, px, App, AppContext as _, Context, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, SharedString, StatefulInteractiveElement as _,
    Styled as _, Window,
};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    h_flex,
    input::Input,
    list::ListItem,
    menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem},
    tree::{Tree as TreeView, TreeEntry, TreeItem, TreeState},
    v_flex, Icon, IconName, Sizable as _,
};

use super::main_view::MainView;

/// N8：样式取色统一走主题工具函数（删除就地 hex 定义）
use crate::theme::to_hsla as hex;

/// 连接树行的字号（px）。2026-09-18 用户要求「除终端外全局较大字体减小一号，
/// 比如…连接分组名称和连接名称」：分组名与连接名 13 到 12。
const ROW_TEXT: f32 = 12.;

/// 连接树选中框的圆角半径（px）。2026-09-18 用户要求「选中框…顺便改为圆角」。
///
/// 官方 `ListItem` 在选中态画一层底 + 一圈「外线」（`list.active_highlight` 时的
/// `selected_style`），两者的圆角都取自本元素的 `corner_radii`
/// （`gpui-component/src/list/list_item.rs`）——所以一处 `.rounded(..)` 同时管到。
/// 6px 与卡片、输入框同档（组件库 `theme.radius`）。
const SELECTION_RADIUS: f32 = 6.;

/// 分组行「展开箭头槽」的宽度（px）。
///
/// 这个槽**两种情形都要占**：有子分组的行画 chevron，空分组留白 —— 否则两行的
/// 文件夹图标不在同一列。
///
/// 取值必须等于 `Icon::small()` 的渲染盒：gpui-component 的 `Size::Small` 在
/// `icon.rs::into_svg` 里走 `size_3p5()`（0.875rem = 14px，rem 基准 16px）。
///
/// 2026-09-20 用户反馈「有连接文件夹和无连接文件夹位置不齐」：空分组原先写的是
/// `div().w(px(16.))`（凭「图标大约 16px」的印象），比 chevron 的 14px 宽 2px
/// ⇒ 该行的文件夹图标与其后所有内容整体右移。截图逐列投影实测两行文件夹墨迹
/// 左缘 41 vs 45（150% 缩放下 4 物理 px ≈ 2.7 逻辑 px）。
const EXPAND_SLOT_WIDTH: f32 = 14.;

/// 连接树内部拖拽的载荷：把一个服务器行拖到分组行上。
#[derive(Clone)]
pub struct DraggedProfile {
    pub id: String,
    pub name: SharedString,
}

/// 连接树内部拖拽的载荷（嵌套分组）：把一个分组行拖到另一个分组行上换父。
#[derive(Clone)]
pub struct DraggedGroup {
    pub id: String,
    pub name: SharedString,
}

/// 拖拽时跟随指针的幽灵视图
struct DragGhost {
    label: SharedString,
}

impl Render for DragGhost {
    fn render(&mut self, _w: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = crate::theme::current(cx);
        h_flex()
            .px_2()
            .py_1()
            .rounded_md()
            .bg(hex(t.bg_elevated))
            .border_1()
            .border_color(hex(t.border_subtle))
            .text_size(px(12.))
            .text_color(hex(t.fg_primary))
            .child(self.label.clone())
    }
}

/// 树渲染闭包（App 上下文）所需的会话状态快照：避免 render 期间升级/借用 MainView 实体（paint 阶段借用冲突风险），由 MainView::render 每帧写入。
#[derive(Default)]
pub struct ConnTreeSnapshot {
    pub selected: Option<String>,
    /// pid 到 "user@host" 显示标签
    pub host_labels: HashMap<String, String>,
    /// pid 到备注（2026-09-18 新增）：只放**非空**备注，行的 tooltip 直接按 pid 查。
    /// 空备注不入表，于是「绝大多数连接没有备注」这一常见情形下多出来的开销是 0。
    pub notes: HashMap<String, String>,
    /// 节点 id（`grp:<分组 id>` / `prof:<连接 id>`）到「同级位置」（序号, 同级总数）。
    ///
    /// 只服务右键菜单的「上移 / 下移」置灰：菜单构建跑在弹层上下文，读不到
    /// `MainView`（读库会 E0500），所以位置必须随快照一起全局缓存。
    /// 口径直接取自 core 的 `group_position` / `profile_position`，
    /// 与树上的显示次序同源（分组按同级兄弟、连接按同组成员）。
    /// 搜索过滤态下本表为空（见 `snapshot_of`），菜单两项随之置灰。
    pub positions: HashMap<String, (usize, usize)>,
}
impl gpui::Global for ConnTreeSnapshot {}

/// 渲染快照的**唯一**构建点：行显示所需的一切（选中态 / 主机标签 / 备注 / 同级位置）。
///
/// 抽成函数是因为快照有两条写入路径（指纹变了就全量重建；只有选中态变了就补写）。
/// 两条路径共用同一份构建逻辑，才不会出现「树刷新了、位置表还是旧的」这种
/// 只体现在「上移 / 下移」置灰上的错位。
///
/// `searching` = 当前有搜索词。搜索态**不填位置表**：过滤后的可见兄弟只是全量的
/// 子集，按全量挪一格时界面上可能原地不动（命中的两行之间夹着被过滤掉的成员）——
/// 那种「点了没反应」比菜单置灰更让人困惑。位置表空着，右键菜单两项自然置灰。
fn snapshot_of(m: &MainView, searching: bool) -> ConnTreeSnapshot {
    let mut snap = ConnTreeSnapshot {
        selected: m.conn_ui.selected_profile.clone(),
        ..Default::default()
    };
    for p in &m.conn_store.profiles {
        snap.host_labels
            .insert(p.id.clone(), format!("{}@{}", p.username, p.host));
        // 空备注不入表：行的 tooltip 只看「有没有这条记录」，不必再判空
        if !p.notes.trim().is_empty() {
            snap.notes.insert(p.id.clone(), p.notes.clone());
        }
    }
    if !searching {
        for p in &m.conn_store.profiles {
            if let Some(pos) = m.conn_store.profile_position(&p.id) {
                snap.positions.insert(format!("prof:{}", p.id), pos);
            }
        }
        for g in &m.conn_store.groups {
            if let Some(pos) = m.conn_store.group_position(&g.id) {
                snap.positions.insert(format!("grp:{}", g.id), pos);
            }
        }
    }
    snap
}

/// 连接树「本帧需要重建吗」的输入指纹（纯函数，可单测）。
///
/// # 为什么需要它（2026-09 性能修复）
///
/// 旧实现**每帧**都重算 `visible_tree`、重建整棵 `Vec<TreeItem>`（每个节点一次
/// `format!` + `SharedString` + 名字克隆）并调用 `TreeState::set_items`。
/// `set_items` 在库内会做 `selected_ix = None; right_clicked_ix = None; cx.notify()`
/// —— 也就是说**树的选中态每帧被清空一次**，键盘上下导航永远从「无选中」重新开始。
///
/// 指纹覆盖「能改变可见树或行的显示」的全部输入：搜索串、展开集合、
/// 以及 store 里每个连接/分组的可显示字段与父子关系。三者都没变就复用上一帧的树。
pub fn conn_tree_key(
    query: &str,
    expanded: &std::collections::HashSet<String>,
    store: &rustshell_core::connections::ConnectionStore,
) -> String {
    let mut key = String::with_capacity(64 + store.profiles.len() * 32 + store.groups.len() * 24);
    // 搜索串：分隔符用不可能出现在字段里的 `\u{1}`/`\u{2}`，避免拼接歧义
    key.push_str(query);
    key.push('\u{1}');
    // 展开集合是 HashSet：必须排序后才稳定（否则同一状态会算出两个指纹）
    let mut ids: Vec<&String> = expanded.iter().collect();
    ids.sort_unstable();
    for id in ids {
        key.push_str(id);
        key.push(',');
    }
    key.push('\u{2}');
    for g in &store.groups {
        key.push_str(&g.id);
        key.push(',');
        key.push_str(&g.name);
        key.push(',');
        key.push_str(g.parent.as_deref().unwrap_or("-"));
        key.push(';');
    }
    key.push('\u{2}');
    for p in &store.profiles {
        key.push_str(&p.id);
        key.push(',');
        key.push_str(&p.name);
        key.push(',');
        key.push_str(&p.host);
        key.push(',');
        key.push_str(&p.username);
        key.push(',');
        // 备注也进指纹：它是**行的显示内容**（hover 提示），漏了它就会出现
        // 「改完备注，树不重建，tooltip 还是旧的」。分隔符沿用上面几项的朴素拼法
        // （自由文本理论上能伪造分隔符，但那需要刻意构造，而失效代价只是少刷一次提示）。
        key.push_str(&p.notes);
        key.push(',');
        key.push_str(p.group_id.as_deref().unwrap_or("-"));
        key.push(';');
    }
    key
}

/// 每帧把连接数据同步进 TreeState，并写入渲染快照
pub fn render(m: &mut MainView, cx: &mut Context<MainView>) -> impl IntoElement {
    // 搜索过滤：可见性决策全在 core 的纯函数 `visible_tree` 里，这里只做映射。
    // 输入串只读一次（指纹与过滤共用），免得同一帧里两次 `read(cx)`。
    let raw_query = m.conn_ui.conn_search.read(cx).value();
    let query = rustshell_core::connections::SearchQuery::parse(&raw_query);
    let searching = !query.is_empty();

    // 渲染快照（供 render_item 在 App 上下文读取选中态 / 主机标签）。
    // 标签按**全量**连接建立、不受搜索影响：搜索只决定「显示哪些行」，
    // 不决定「这行长什么样」。
    //
    // ⚠️ 只在「输入指纹变了」时重建并 `set_global`：本函数每帧都会跑
    // （侧栏每帧重排），而快照的内容只在连接数据变化时才变。无脑每帧重建
    // 会带来「每连接 2 次字符串分配 + 一次全局写入」，纯属浪费。
    let tree_key = conn_tree_key(&raw_query, &m.conn_ui.expanded_groups, &m.conn_store);
    let rebuild = m.conn_ui.conn_tree_key.as_deref() != Some(tree_key.as_str());

    if rebuild {
        cx.set_global(snapshot_of(m, searching));
    } else if !cx
        .try_global::<ConnTreeSnapshot>()
        .is_some_and(|g| g.selected == m.conn_ui.selected_profile)
    {
        // 选中态不进指纹（它不改变树结构），但 render_item 要读它。
        // 命中的分支是「什么都没变」的常态：一次全局读 + 一次 Option 比较，走到这里
        // 说明选中项换了人（每次点击至多一次），重建快照的代价可以忽略。
        cx.set_global(snapshot_of(m, searching));
    }

    let plan = m.conn_store.visible_tree(&query);
    // 连接数据 到 官方 TreeItem 树（嵌套：`plan.groups` 的 `children` 递归展开）。
    // 节点 id 用分组 **id**（不是名）：改名不丢展开态，也不会与另一个同名分组撞键
    // （`expanded_groups` 同源存 id；子分组也是 grp: 节点，展开/右键菜单自动同权）。
    let name_of = |pid: &str| {
        m.conn_store
            .profile(pid)
            .map(|p| p.name.clone())
            .unwrap_or_default()
    };
    fn group_node(
        g: &rustshell_core::connections::VisibleGroup,
        searching: bool,
        expanded_groups: &std::collections::HashSet<String>,
        name_of: &dyn Fn(&str) -> String,
    ) -> TreeItem {
        // 搜索态强制展开：命中的连接若还藏在收起的分组里，等于「搜了没结果」
        let expanded = if searching {
            true
        } else {
            expanded_groups.contains(&g.id)
        };
        let mut gi = TreeItem::new(SharedString::from(format!("grp:{}", g.id)), g.name.clone())
            .expanded(expanded);
        for pid in &g.profile_ids {
            gi = gi.child(TreeItem::new(
                SharedString::from(format!("prof:{pid}")),
                name_of(pid),
            ));
        }
        for c in &g.children {
            gi = gi.child(group_node(c, searching, expanded_groups, name_of));
        }
        gi
    }
    // 只有指纹变化时才重建整棵树并推进 `TreeState`。
    //
    // ★ 这不只是省分配：库里的 `TreeState::set_items` 会
    // `selected_ix = None; right_clicked_ix = None; cx.notify()`，
    // 旧实现每帧都调一次 ⇒ **树的选中态每帧被清空**，键盘上下导航永远从
    // 「无选中」重新开始（用户感知为「方向键选不中」）。按指纹重建后，
    // 选中态在数据不变时可以跨帧存活。
    if rebuild {
        let mut items: Vec<TreeItem> = Vec::new();
        for g in &plan.groups {
            items.push(group_node(
                g,
                searching,
                &m.conn_ui.expanded_groups,
                &name_of,
            ));
        }
        for pid in &plan.ungrouped {
            items.push(TreeItem::new(
                SharedString::from(format!("prof:{pid}")),
                name_of(pid),
            ));
        }
        // 重建后按**连接 id** 把库内部的选中态对齐回来。
        //
        // 库的选中态是**按下标**存的（`TreeState::selected_ix`），而 `set_items`
        // 把它清成 None、展开/折叠又会整体位移条目下标 —— 不对齐就会出现
        // 「选中框突然没了」或「选中框跳到别的连接上」。我们的选中态是
        // `m.conn_ui.selected_profile`（连接 id），换算成下标只该发生在这两处。
        //
        // 高亮本身也由库这个下标状态绘制（官方 `Tree` 会用
        // `entry_state.is_selected()` 覆盖我们传给 `ListItem::selected(..)` 的值）。
        let anchor = m
            .conn_ui
            .selected_profile
            .as_ref()
            .map(|pid| SharedString::from(format!("prof:{pid}")));
        m.conn_ui.tree_state.update(cx, |ts, cx| {
            ts.set_items(items, cx);
            let ix = anchor.as_ref().and_then(|id| ts.index_of(id));
            if ts.selected_index() != ix {
                ts.set_selected_index(ix, cx);
            }
        });
        m.conn_ui.conn_tree_key = Some(tree_key);
    }

    // /S5：宽度与背景由`MainView`的侧栏容器统一提供（可拖拽调宽 + bg_base
    // + 右侧分割线），面板只负责内容，避免各面板底色不一致导致与终端界限不清。
    //
    // 2026-09-15 用户要求：取消树顶部的「服务器」标识，让分组/连接直接作为最上层。
    let mut col = v_flex().w_full().h_full();

    // 顶部一行：搜索框 + 右侧「新增连接」图标。
    //
    // 2026-09-17 运行反馈「连接栏，新增按钮改为栏右侧一个图标」：原先贴在面板底部的
    // 整条描边按钮（「+ 新建连接」）撤掉，改成一枚 `xsmall` ghost 图标按钮挂在栏右侧 ——
    // 面板底部不再被一条常驻按钮占位，树的可见高度多出一行。
    //
    // 2026-10-21 运行反馈「连接栏搜索框消失」：搜索框原先只在 `profiles` 非空时渲染
    // （「没有可搜的东西，输入框只是噪音」），于是新建/清空连接库之后整条搜索框凭空
    // 消失，用户以为功能被砍了。现在**常显** —— 位置固定才认得出这是搜索框；
    // 空库时它搜不到东西是自然的，但它的存在本身是「这里能搜」的唯一提示。
    let mut head = h_flex().items_center().gap_2().px_3().pt_2().pb_1();
    // 点框外失焦：与快捷命令栏 / 文件地址栏同一口径（见
    // `main_view::blur_input_on_outside_click` 的说明）。
    // 失焦只放掉焦点，**筛选词留在框里**（它决定显示哪些行，与焦点无关）。
    //
    // 宽度收在包装层（原先写在输入框自己的 `.flex_1()` 上），所以内层要 `w_full()`
    // ——输入框的默认宽度是内容宽，不铺满就会在包装层里留出一段「点了没反应」的空白。
    head = head.child(
        div()
            .flex_1()
            .min_w_0()
            // 量取钩子：回归用例靠它在**空库**下也要拿到搜索框的真实矩形。
            .debug_selector(|| "conn-search-box".to_string())
            .on_mouse_down_out(cx.listener(|m, _e, w, cx| {
                super::main_view::blur_input_on_outside_click(&m.conn_ui.conn_search, w, cx)
            }))
            .child(
                Input::new(&m.conn_ui.conn_search)
                    .small()
                    .cleanable(true)
                    // 左侧放大镜（2026-10-21 用户要求）：搜索框常显之后，
                    // 「这是一只搜索框」应该由图标来提示 —— 占位符一旦被输入内容
                    // 覆盖就消失了，图标则一直在。走官方 `prefix`，图标落在框内左侧、
                    // 随输入框高度自动居中，不用自己算内边距。
                    .prefix(
                        // 外层 div 只为挂量取钩子（图标本身不带 `debug_selector`）：
                        // 用例要证明这枚放大镜真的画出来了，而不是只写在链上。
                        div()
                            .debug_selector(|| "conn-search-icon".to_string())
                            .child(
                                Icon::new(IconName::Search)
                                    .xsmall()
                                    .text_color(hex(crate::theme::current(cx).fg_muted)),
                            ),
                    )
                    .w_full(),
            ),
    );
    col = col.child(
        head.child(
            // 外层 `div` 只为挂 `debug_selector`（宽高由内容决定，不改变布局）——
            // 用例借它拿到这枚图标的真实矩形并模拟点击，从而覆盖 `on_click` 这条
            // 此前**从未被测过**的路径（其余用例都直接调 `open_connect_dialog`，
            // 绕过了按钮本身）。这正是「点了图标就闪退」能长期潜伏的原因。
            div()
                .debug_selector(|| "new-connection-btn".to_string())
                .child(
                    Button::new("new-connection")
                        .ghost()
                        .xsmall()
                        .icon(IconName::Plus)
                        .tooltip(rustshell_core::i18n::t("新建连接"))
                        .on_click(cx.listener(|m, _e, w, cx| {
                            m.open_connect_dialog(None, w, cx);
                        })),
                ),
        ),
    );

    // 搜索无结果必须给可见原因：空树会让用户以为连接被删了。
    let body = if searching && plan.is_empty() {
        div()
            .flex_1()
            .min_h_0()
            .px_3()
            .py_2()
            .text_size(px(12.))
            .text_color(hex(crate::theme::current(cx).fg_muted))
            .child(rustshell_core::i18n::t("未找到匹配的连接"))
            .into_any_element()
    } else {
        // 官方 Tree：虚拟化列表 + 内建滚动条 + 键盘导航（上下选择、左右折叠）
        //
        // 右键菜单分两处挂：**行**用官方 `Tree::context_menu`（0.8.0 起官方 Tree
        // 会无条件把每一行包进 `ContextMenu`，容器级菜单收不到行的右键），
        // **空白处**仍挂在容器上（新增连接 / 新建分组）。
        //
        // 两处不会同时弹：行上的 `ContextMenu` 在右键按下时 `stop_propagation()`，
        // 容器那一层收不到同一次事件。
        div()
            .flex_1()
            .min_h_0()
            .context_menu(|menu: PopupMenu, _w, _cx| blank_menu(menu))
            .child(
                TreeView::new(&m.conn_ui.tree_state.clone(), render_tree_item)
                    .context_menu(tree_row_ctx_menu),
            )
            .into_any_element()
    };

    // 树占满剩余高度：面板底部不再有常驻按钮（新增入口已挪到顶部栏右侧）。
    //
    // 2026-09-30 用户要求「删除连接树下方的隧道信息」：此前这里还接着一份隧道列表
    // （F-3 本来允许「竖栏新增面板 **或** 并入连接面板底部」，取的是后者），现整块撤下。
    // 撤的理由不是「列表没用」，而是它和连接树**不同源**：这一栏回答的是「我要连到哪台
    // 机器」，隧道回答的是「哪条转发此刻在监听」；两者混在一栏里，既都不容易一眼看清，
    // 列表又长期占着树的可见高度（位置还在最下方，正是最容易看不见的地方）。
    // 端口转发功能本身一字未动：规则仍随连接保存、连接建立后照旧自动启动，
    // 见 `rustshell_core::forward` 与 `ConnectionProfile::forwards`。
    col.child(body)
}

/// 分组行的展开槽：固定宽度、内容居中；传 `None` 得到一枚等宽空槽。
///
/// 之所以把 chevron 也一起塞进定宽槽，而不是「有子分组就画图标、没有就 `div().w(..)`」：
/// 后者是两条彼此独立的路径，一旦库改了 `Size::Small` 的像素值、或有人换用别的尺寸档，
/// 两条路就再次错开 —— 2026-09-20 那次错位正是这么来的（占位写 16px，而图标盒是 14px）。
/// 收口成一个槽之后，「两行文件夹图标是否同列」只取决于槽宽这一个数。
fn expand_slot(icon: Option<gpui::AnyElement>) -> gpui::AnyElement {
    let mut slot = h_flex()
        .w(px(EXPAND_SLOT_WIDTH))
        .h(px(EXPAND_SLOT_WIDTH))
        .flex_shrink_0()
        .items_center()
        .justify_center();
    if let Some(icon) = icon {
        slot = slot.child(icon);
    }
    slot.into_any_element()
}

/// 官方 Tree 行渲染（App 上下文；分组行折叠/展开由 TreeState 内建处理）
fn render_tree_item(
    _ix: usize,
    entry: &TreeEntry,
    _selected: bool,
    _w: &mut Window,
    cx: &mut App,
) -> ListItem {
    let t = crate::theme::current(cx);
    let item = entry.item();
    let id = item.id.to_string();
    // 缩进：对齐 面板内边距 12px，再按层级步进 16px（原实现根节点为 0，贴边）
    let indent = px(12.) + px(16.0 * entry.depth() as f32);

    if let Some(group_id) = id.strip_prefix("grp:").map(str::to_string) {
        // 分组行：chevron（仅可展开时）+ 文件夹 + 名称 + 成员数
        let has_children = !item.children.is_empty();
        let mut row = h_flex().w_full().min_w_0().items_center().gap_1();
        // 展开箭头：有子分组画 chevron，空分组留白 —— 两者共用**同一个定宽槽**
        // （`expand_slot`），文件夹图标才会落在同一列（见 `EXPAND_SLOT_WIDTH`）。
        row = row.child(expand_slot(has_children.then(|| {
            Icon::new(if entry.is_expanded() {
                IconName::ChevronDown
            } else {
                IconName::ChevronRight
            })
            .small()
            .into_any_element()
        })));
        // 供几何护栏用例（`grp_folder_is_aligned_across_rows`）度量「文件夹图标落在哪一列」。
        // `debug_selector` 在正式构建里不产生任何东西。
        let folder_sel = format!("grp-folder:{group_id}");
        let drop_group = group_id;
        let drag_label = item.label.clone();
        // 三处闭包各持一份目标分组 id（move 闭包不能共享借用）
        let drop_for_prof = drop_group.clone();
        let drop_for_group = drop_group.clone();
        return ListItem::new(item.id.clone())
            .pl(indent)
            // 选中框圆角（2026-09-18 用户要求「选中框…改为圆角」）：
            // 官方 `ListItem` 把本元素的 `corner_radii` 同时用在选中底色与那圈
            // 「选中框外线」上（`list_item.rs`：`selected_style.corner_radii =
            // corner_radii`），所以一处 `.rounded(..)` 就够。
            .rounded(px(SELECTION_RADIUS))
            // 落点高亮：拖着服务器行/分组行经过时给出可放置反馈
            .drag_over::<DraggedProfile>(|s, _, _w, cx| {
                let t = crate::theme::current(cx);
                s.bg(hex(t.accent).opacity(0.18))
            })
            .drag_over::<DraggedGroup>(|s, _, _w, cx| {
                let t = crate::theme::current(cx);
                s.bg(hex(t.accent).opacity(0.18))
            })
            // 只接受连接树内部的拖拽载荷；其他类型（如外部文件）不放行
            .can_drop(|data: &dyn std::any::Any, _w, _cx| {
                data.is::<DraggedProfile>() || data.is::<DraggedGroup>()
            })
            .on_drop(move |v: &DraggedProfile, _w, cx| {
                super::main_view::ui_call_move_connection(&v.id, Some(drop_for_prof.as_str()), cx);
            })
            // 嵌套分组：把分组拖到另一个分组行上 = 整棵子树换父
            //（拖进自己或自己的后代会被 core 拒绝并给提示，见 `ui_call_move_group`）
            .on_drop(move |v: &DraggedGroup, w, cx| {
                super::main_view::ui_call_move_group(&v.id, Some(drop_for_group.as_str()), w, cx);
            })
            .on_drag(
                DraggedGroup {
                    id: drop_group.clone(),
                    name: drag_label,
                },
                |v: &DraggedGroup, _offset, _w, cx| {
                    cx.new(|_| DragGhost {
                        label: v.name.clone(),
                    })
                },
            )
            .child(
                // 2026-09-18 用户要求「分组右侧去掉数字显示」：行尾不再摆成员数徽标。
                // 分组里有多少东西，展开一眼就看得到；那枚数字既占宽度又常被省略号
                // 挤成「1…」，反倒是噪声。
                row.child(
                    // 外面这层 `div` 只为挂 `debug_selector`（宽高由内容决定，不改变布局）：
                    // 「有子分组的行」与「空分组的行」的文件夹图标必须落在同一个 x 上，
                    // 由 `grp_folder_is_aligned_across_rows` 把关。
                    div().debug_selector(move || folder_sel.clone()).child(
                        // 2026-10-21 用户要求「连接树文件夹增加展开关闭图标变化效果」：
                        // 图标随展开状态换（打开 = `FolderOpen`，收起 = `FolderClosed`）。
                        // 此前恒为 `FolderClosed`，只有左边那枚 chevron 在动 —— 一眼看不出
                        // 「这个文件夹此刻是开着的」，尤其是分组里的连接很少时。
                        Icon::new(if entry.is_expanded() {
                            IconName::FolderOpen
                        } else {
                            IconName::FolderClosed
                        })
                        .small()
                        .text_color(hex(t.warning)),
                    ),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(px(ROW_TEXT))
                        .text_color(hex(t.fg_primary))
                        // 名称必须**单行**：官方 Tree 是 uniform 列表（每行等高），
                        // 长名字一旦在空格处折行就把行撑高，压到上下相邻行上。
                        // `text_ellipsis` 只管截断不管换行，所以 nowrap 要显式给。
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .child(item.label.clone()),
                ),
            );
    }

    // 服务器行：终端图标 + 名称 + user@host（无连接状态点；选中态由快照决定）
    let pid = id.strip_prefix("prof:").unwrap_or("").to_string();
    let (selected, host_label, note) = cx
        .try_global::<ConnTreeSnapshot>()
        .map(|s| {
            (
                s.selected.as_deref() == Some(pid.as_str()),
                s.host_labels.get(&pid).cloned().unwrap_or_default(),
                s.notes.get(&pid).cloned(),
            )
        })
        .unwrap_or((false, String::new(), None));

    ListItem::new(item.id.clone())
        .selected(selected)
        .pl(indent)
        // 选中框圆角：与分组行同规格（见分组行处的说明）
        .rounded(px(SELECTION_RADIUS))
        .child(
            h_flex()
                .w_full()
                .min_w_0()
                .items_center()
                .gap_1()
                .child(
                    Icon::new(IconName::SquareTerminal)
                        .small()
                        .text_color(hex(t.fg_muted)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(px(ROW_TEXT))
                        .text_color(hex(t.fg_primary))
                        // 同上：单行 + 省略号，撑高行会压到相邻行（uniform 列表）
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .child(item.label.clone()),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .whitespace_nowrap()
                        .text_size(px(10.))
                        .text_color(hex(t.fg_muted))
                        .child(SharedString::from(host_label)),
                ),
        )
        // 拖拽源：把本服务器行拖到任意分组行即可移动归属
        .on_drag(
            DraggedProfile {
                id: pid.clone(),
                name: item.label.clone(),
            },
            |v: &DraggedProfile, _offset, _w, cx| {
                cx.new(|_| DragGhost {
                    label: v.name.clone(),
                })
            },
        )
        // 单击选中 / 双击连接（450ms 时间窗兜底，语义与旧实现一致）
        .on_click(move |e, w, cx| {
            let click_count = e.click_count();
            super::main_view::ui_call(
                &pid,
                |m, pid, w, cx| {
                    let now = std::time::Instant::now();
                    let double_click = click_count >= 2
                        || m.conn_ui
                            .last_click_profile
                            .as_ref()
                            .is_some_and(|(id, t)| {
                                *id == pid && now.duration_since(*t).as_millis() < 450
                            });
                    if double_click {
                        m.conn_ui.last_click_profile = None;
                        m.connect_profile(pid, w, cx);
                    } else {
                        m.conn_ui.last_click_profile = Some((pid.to_string(), now));
                        m.conn_ui.selected_profile = Some(pid.to_string());
                        cx.notify();
                    }
                },
                w,
                cx,
            );
        })
        // 备注（2026-09-18 新增）：**非空才挂** hover 提示 —— 绝大多数连接没有备注，
        // 给每一行都挂一个空 tooltip 只会让「鼠标划过每行都弹个空气泡」。
        // 提示内容就是用户自己写的原文（保留换行），上面标一行「备注」小标题免得
        // 让人误以为是主机名之类的系统信息。快照按 pid 查，渲染路径零 IO、零分配。
        //
        // 走 `Tooltip::element(..).build(..)` 而不是裸闭包：`.tooltip(closure)` 要求的
        // 返回类型是 `AnyView`（一个**视图实体**，见 `Tooltip::build`），不是元素；
        // 想要多行富内容（小标题 + 正文两行）只能借 `Tooltip::element` 把元素包进去。
        .when_some(note, |this, note| {
            this.tooltip(move |w, cx| {
                let note = note.clone();
                gpui_component::tooltip::Tooltip::element(move |_w, cx| {
                    let t = crate::theme::current(cx);
                    v_flex()
                        .max_w(px(320.))
                        .gap_1()
                        .child(
                            div()
                                .text_size(px(10.5))
                                .text_color(hex(t.fg_muted))
                                .child(SharedString::from(rustshell_core::i18n::t("备注"))),
                        )
                        .child(
                            div()
                                .text_size(px(12.))
                                .text_color(hex(t.fg_primary))
                                .child(SharedString::from(note.clone())),
                        )
                })
                .build(w, cx)
            })
        })
}

/// 行右键菜单：官方 `Tree::context_menu` 逐行挂载，按条目 id 分发到分组 / 连接菜单。
///
/// 菜单构建跑在弹层上下文（拿不到 `MainView`），所以这里只读渲染快照并做纯分发；
/// 真正的动作全部落在各菜单项的 `on_click` 里，经 `super::main_view` 的入口回到 MainView。
fn tree_row_ctx_menu(
    _ix: usize,
    entry: &TreeEntry,
    menu: PopupMenu,
    _w: &mut Window,
    cx: &mut Context<TreeState>,
) -> PopupMenu {
    let id = entry.item().id.to_string();
    // 「上移 / 下移」的置灰依据：本条节点在**同级**里的（序号, 总数）。
    //
    // 位置随渲染快照全局缓存（见 `ConnTreeSnapshot::positions`）：菜单构建在弹层
    // 上下文里跑，读不到 `MainView`。快照里查不到就按 `(0, 1)` 兜底到两个方向都
    // 置灰 —— 搜索过滤态正是这种情况（见 `snapshot_of`），宁可这一次点不动，
    // 也不让菜单项点了没反应。
    let pos = cx
        .try_global::<ConnTreeSnapshot>()
        .and_then(|s| s.positions.get(&id).copied())
        .unwrap_or((0, 1));
    if let Some(group_id) = id.strip_prefix("grp:") {
        group_menu(menu, group_id, pos)
    } else if let Some(profile_id) = id.strip_prefix("prof:") {
        profile_menu(menu, profile_id, pos)
    } else {
        menu
    }
}

/// 树空白处的菜单：新增连接 / 新建分组。
///
/// 语义上「空白」不属于任何分组，所以新增连接不带归属（`None` = 未分组）——
/// 表单里已经没有分组一栏，这里的 `None` 就是最终归属；新建分组的锚点也是
/// `None`（追加到末尾）。
fn blank_menu(menu: PopupMenu) -> PopupMenu {
    menu.item(
        PopupMenuItem::new(rustshell_core::i18n::t("新建连接"))
            .on_click(|_, w, cx| super::main_view::open_connect_dialog_for(None, w, cx)),
    )
    .item(
        PopupMenuItem::new(rustshell_core::i18n::t("新建分组"))
            .on_click(|_, w, cx| super::main_view::ui_call_new_group_dialog(None, None, w, cx)),
    )
    // 「从 OpenSSH 配置导入…」曾挂在这里，2026-09-28 按用户要求撤下：
    // 它是一次性的**批量导入**动作，挂在「空白处右键」这种高频、轻量的位置上
    // 不合适（右键空白多半只是想新建点什么，弹出一整层导入预览是误触成本）。
    // 功能本身没删 —— 设置 → 导入导出 里那枚按钮仍是它的入口
    // （`MainView::import_openssh_config`，两条入口共用同一实现）。
}

/// 6.3 节 服务器节点菜单（M2 子集：连接/编辑/重命名/删除/复制信息 + 同组内上下排序）
///
/// `pos` 是本连接在**同组成员里**的（序号, 总数），只用来给「上移 / 下移」置灰。
/// 排序是「同组内挪一位」，不换组 —— 换组仍走拖拽（`move_profile_to_group`）。
fn profile_menu(menu: PopupMenu, profile_id: &str, pos: (usize, usize)) -> PopupMenu {
    let (idx, len) = pos;
    menu.item(
        PopupMenuItem::new(rustshell_core::i18n::t("连接")).on_click({
            let id = profile_id.to_string();
            move |_, w, cx| super::main_view::ui_call(&id, MainView::connect_profile, w, cx)
        }),
    )
    .item(
        // 标签里带上快捷键：这是连接树上唯一有固定键位的动作，不写出来没人会去按 F2
        PopupMenuItem::new(rustshell_core::i18n::ts(
            format!("重命名…  ({})", crate::keybinds::display_spec("f2")),
            format!("Rename…  ({})", crate::keybinds::display_spec("f2")),
        ))
        .on_click({
            let id = profile_id.to_string();
            move |_, w, cx| {
                super::main_view::ui_call(&id, MainView::open_rename_connection_dialog, w, cx)
            }
        }),
    )
    .item(
        PopupMenuItem::new(rustshell_core::i18n::t("编辑…")).on_click({
            let id = profile_id.to_string();
            move |_, w, cx| super::main_view::ui_call(&id, MainView::edit_connection, w, cx)
        }),
    )
    .item(
        PopupMenuItem::new(rustshell_core::i18n::t("复制连接")).on_click({
            let id = profile_id.to_string();
            move |_, w, cx| super::main_view::ui_call(&id, MainView::duplicate_connection, w, cx)
        }),
    )
    .item(
        PopupMenuItem::new(rustshell_core::i18n::t("复制服务器信息")).on_click({
            let id = profile_id.to_string();
            move |_, _w, cx| super::main_view::copy_profile_summary(&id, cx)
        }),
    )
    // 同组内上下排序（首 / 末位对应置灰）
    .separator()
    .item(move_up_item(idx == 0, {
        let id = profile_id.to_string();
        move |cx| super::main_view::ui_call_move_connection_step(&id, -1, cx)
    }))
    .item(move_down_item(len == 0 || idx + 1 >= len, {
        let id = profile_id.to_string();
        move |cx| super::main_view::ui_call_move_connection_step(&id, 1, cx)
    }))
    .separator()
    .item(
        PopupMenuItem::new(rustshell_core::i18n::t("删除")).on_click({
            let id = profile_id.to_string();
            move |_, w, cx| super::main_view::ui_call(&id, MainView::remove_profile, w, cx)
        }),
    )
}

/// 「上移 / 下移」两个菜单项的唯一拼装点（连接行与分组行共用）。
///
/// 抽出来是为了让「置灰条件」与「动作方向」在全树里各只有一个来源：
/// 两处菜单各写一遍，早晚会出现「连接行是首项置灰、分组行忘了置灰」这类分叉。
fn move_up_item(disabled: bool, on_pick: impl Fn(&mut App) + 'static) -> PopupMenuItem {
    PopupMenuItem::new(rustshell_core::i18n::t("上移"))
        .disabled(disabled)
        .on_click(move |_, _w, cx| on_pick(cx))
}

/// 见 [`move_up_item`]。
fn move_down_item(disabled: bool, on_pick: impl Fn(&mut App) + 'static) -> PopupMenuItem {
    PopupMenuItem::new(rustshell_core::i18n::t("下移"))
        .disabled(disabled)
        .on_click(move |_, _w, cx| on_pick(cx))
}

/// 6.4 节 分组菜单（新建连接 / 新增分组 / 全部连接 / 上下排序 / 重命名 / 删除）。
/// `group_id` 为分组 id。
///
/// 2026-09-20 运行反馈「连接分组上右键，新增分组删除，新建子分组改名为新增分组」：
/// 这里原来并列着**两条**新建入口 ——「新建子分组」（建到本分组里）与「新建分组」
/// （落在本分组下面、同级）。两条并排时用户得先分辨「子 / 不子」才敢点，而分组
/// 本就支持嵌套、拖拽也能换父，同级插入多数时候是多余的。现在只留一条
/// 「新增分组」= 建到**本分组里面**；顶级分组仍走连接树空白处右键
/// （`blank_menu` 的「新建分组」）。
///
/// `pos` 是本分组在**同级兄弟里**的（序号, 总数），只用来给「上移 / 下移」置灰；
/// 排序只在同一层里换位（整棵子树一起搬），不会把分组挪进别的分组 —— 换父仍走拖拽。
fn group_menu(menu: PopupMenu, group_id: &str, pos: (usize, usize)) -> PopupMenu {
    let (idx, len) = pos;
    menu.item(
        // 本分组的 id 就是这次新建连接的**归属**：连接表单里已经没有分组一栏，
        // 从这里点进去是唯一还能「落在指定分组」的入口（其余入口一律未分组）。
        PopupMenuItem::new(rustshell_core::i18n::t("新建连接")).on_click({
            let g = group_id.to_string();
            move |_, w, cx| super::main_view::open_connect_dialog_for(Some(g.as_str()), w, cx)
        }),
    )
    .item(
        // 本分组 id 当**父分组**传下去：新分组建到本分组里面
        PopupMenuItem::new(rustshell_core::i18n::t("新增分组")).on_click({
            let g = group_id.to_string();
            move |_, w, cx| {
                super::main_view::ui_call_new_group_dialog(None, Some(g.as_str()), w, cx)
            }
        }),
    )
    .separator()
    .item(
        PopupMenuItem::new(rustshell_core::i18n::t("全部连接")).on_click({
            let g = group_id.to_string();
            move |_, w, cx| super::main_view::connect_group(&g, w, cx)
        }),
    )
    // 同级上下排序（首 / 末位对应置灰）
    .separator()
    .item(move_up_item(idx == 0, {
        let g = group_id.to_string();
        move |cx| super::main_view::ui_call_move_group_step(&g, -1, cx)
    }))
    .item(move_down_item(len == 0 || idx + 1 >= len, {
        let g = group_id.to_string();
        move |cx| super::main_view::ui_call_move_group_step(&g, 1, cx)
    }))
    .separator()
    .item(
        PopupMenuItem::new(rustshell_core::i18n::t("重命名…")).on_click({
            let g = group_id.to_string();
            move |_, w, cx| super::main_view::ui_call_rename_group_dialog(&g, w, cx)
        }),
    )
    .item(
        PopupMenuItem::new(rustshell_core::i18n::t("删除")).on_click({
            let g = group_id.to_string();
            move |_, w, cx| super::main_view::ui_call_confirm_remove_group(&g, w, cx)
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::prod;
    use rustshell_core::connections::{ConnectionGroup, ConnectionProfile, ConnectionStore};

    /// 2026-10-21 用户要求「连接树文件夹增加展开关闭图标变化效果」：分组行的文件夹
    /// 图标必须随展开状态在 `FolderOpen` / `FolderClosed` 之间切换。
    ///
    /// 来源断言而不是像素断言：图标是矢量绘制，无头环境里量不出「哪张图」；而这条
    /// 契约要挡的正是「有人把它改回恒定的 `FolderClosed`」（此前就是这样，只有左边
    /// 那枚 chevron 在动）。
    #[test]
    fn group_rows_swap_the_folder_icon_when_expanded() {
        let code = prod(include_str!("connections_panel.rs"));
        let at = code
            .find("fn render_tree_item(")
            .expect("找不到连接树行渲染函数");
        let body = &code[at..];
        let end = body.find("\n}").expect("找不到行渲染函数的结尾");
        let body = &body[..end];

        assert!(
            body.contains("IconName::FolderOpen"),
            "分组行没有「打开的文件夹」图标 —— 展开与否在界面上看不出来。"
        );
        assert!(
            body.contains("IconName::FolderClosed"),
            "分组行丢了「关闭的文件夹」图标（收起态）。"
        );
        assert!(
            body.contains("entry.is_expanded()"),
            "文件夹图标必须按 `entry.is_expanded()` 分支，否则不随展开状态变。"
        );
        assert!(
            !body.contains("Icon::new(IconName::FolderClosed)"),
            "文件夹图标又写成恒定的 `FolderClosed` 了 —— 展开时应当换成 `FolderOpen`。"
        );
    }

    /// 回归（2026-09-17 运行反馈「连接栏，新增按钮改为栏右侧一个图标」）：
    /// 新增连接入口只能是**顶部栏右侧的一枚图标按钮**。
    ///
    /// 四条判据各挡一种倒退：
    /// * 入口只能有一个 —— 别再冒出第二个「新建连接」（底部那条被撤掉的按钮）；
    /// * 必须是图标按钮（`.icon(IconName::Plus)` + tooltip），不再是带 `.label(...)`
    ///   的整条描边按钮（`.outline()` 在本文件里只属于那条被撤掉的按钮）；
    /// * 它在源码里必须出现在 `col.child(body)` **之前** —— 顺序即布局：之后就是
    ///   底部常驻按钮，正是这次要撤掉的形态；
    /// * 树的容器必须还是 `col` 的**最后一个**子节点（`col.child(body)` 结尾不再接
    ///   任何 `.child(..)`）。
    #[test]
    fn new_connection_entry_is_a_plain_icon_on_the_panel_bar() {
        let code = prod(include_str!("connections_panel.rs"));
        assert_eq!(
            code.matches("Button::new(\"new-connection\")").count(),
            1,
            "「新建连接」入口必须只有一个（图标按钮那一枚）"
        );
        assert!(
            code.contains(".icon(IconName::Plus)") && code.contains(".tooltip("),
            "栏右侧的新增入口必须是图标按钮（`.icon(IconName::Plus)` + tooltip），\
             否则一枚 20px 的图标说不清自己是什么。"
        );
        assert!(
            !code.contains(".outline()"),
            "面板里又出现了整条描边按钮 —— 用户要求把新增入口收成右上角一枚图标。"
        );

        let button = code
            .find("Button::new(\"new-connection\")")
            .expect("找不到新增连接按钮");
        let body = code.find("col.child(body)").expect("找不到树容器挂载点");
        assert!(
            button < body,
            "新增入口必须在树容器**之前**渲染（顺序即布局）—— 落在之后就是又一条底部按钮。"
        );
        let tail = &code[body + "col.child(body)".len()..];
        let fn_end = tail.find("\n}").expect("找不到 render 的结尾");
        // 2026-09-23 一度放宽成白名单（当时树容器之后恰好挂着隧道列表）；2026-09-30
        // 用户要求「删除连接树下方的隧道信息」之后，白名单里那一项也撤了，口径回到
        // **一律不许挂**：树容器是 `render` 的最后一个子节点。
        //
        // 这条断言挡的从来不只是「底部按钮」：连接树的可见高度全靠「树吃到面板底」，
        // 任何常驻在它下面的东西（按钮、列表、提示条）都是在按像素向它收租 —— 而
        // 位置在面板最下方的东西，用户往往是「看不见它、也想不到抬头找」。
        let after = tail[..fn_end].trim().trim_start_matches(';').trim();
        assert!(
            after.is_empty(),
            "树容器之后挂了子节点：{after:?}\n\
             连接面板底部不再挂任何常驻内容（隧道列表已于 2026-09-30 按用户要求撤下），\
             否则会挤掉树的可视高度。"
        );
        assert!(
            !code.contains("tunnel_section"),
            "隧道列表被重新挂回了连接面板 —— 用户已明确要求删除连接树下方的隧道信息。\
             转发功能本身仍在（规则随连接保存、core 的 forward 模块照旧启动它们），\
             撤掉的只是这块面板。"
        );
    }

    /// 回归（2026-09-28 用户要求）：连接树**空白处右键**不再挂「从 OpenSSH 配置导入」。
    ///
    /// 功能没删（设置 → 导入导出 里那枚按钮仍是入口，`MainView::import_openssh_config`），
    /// 撤的只是右键空白菜单里那一条。判据分两半，缺一不可：
    /// * 空白菜单里不许再出现那条文案；
    /// * 设置面板那枚按钮**必须还在** —— 否则是「顺手把功能也删了」，
    ///   而用户明确说的是「去除右键那一项」。
    #[test]
    fn blank_context_menu_no_longer_imports_openssh() {
        let code = prod(include_str!("connections_panel.rs"));
        let at = code
            .find("fn blank_menu(menu: PopupMenu) -> PopupMenu {")
            .expect("找不到 `blank_menu`");
        let end = code[at..].find("\n}").expect("找不到 `blank_menu` 的结尾");
        let body = &code[at..at + end];
        assert!(
            !body.contains("从 OpenSSH 配置导入"),
            "空白右键菜单里又出现了「从 OpenSSH 配置导入」：用户已要求撤下这一条。"
        );
        assert!(
            body.contains("新建连接") && body.contains("新建分组"),
            "撤掉 OpenSSH 导入时把另外两条也一起削掉了？空白菜单只剩新建连接/新建分组。"
        );

        // 另一半：设置面板那枚按钮必须还在（功能本身不许被顺手删掉）。
        let settings = prod(include_str!("main_view/settings_panel.rs"));
        assert!(
            settings.contains("从 OpenSSH 配置导入"),
            "设置面板里的「从 OpenSSH 配置导入」不见了：用户只要求撤右键那一项，\n             功能本身要保留（否则就是偷偷删了功能）。"
        );
    }

    /// 回归（2026-10-21 运行反馈「连接栏搜索框消失」）：搜索框必须**常显**，
    /// 与连接库空不空无关。
    ///
    /// 原先它被 `if profiles.is_empty() { 占位 } else { 搜索框 }` 挡着，于是
    /// ① 首次启动（库是空的）、② 把连接全删掉 —— 两种情况下整条搜索框凭空消失，
    /// 用户看到的是「这个功能被砍了」。判据是**几何**：空库下量得到、且高度 > 0。
    /// 只看源码里的 `Input::new(&m.conn_ui.conn_search)` 无法证明它真进了渲染树。
    #[gpui::test]
    fn the_search_box_stays_visible_when_the_store_is_empty(cx: &mut gpui::TestAppContext) {
        use crate::main_view::{AppShell, MainView, PanelKind};
        use gpui::AppContext as _;
        use std::cell::RefCell;
        use std::rc::Rc;

        cx.update(|cx| {
            gpui_component::init(cx);
            crate::theme::apply_theme_mode(cx, rustshell_core::theme::ThemeMode::Dark);
        });
        let slot: Rc<RefCell<Option<gpui::Entity<MainView>>>> = Rc::new(RefCell::new(None));
        let (_, vcx) = {
            let slot = slot.clone();
            cx.add_window_view(move |window, cx| {
                let main = cx.new(|cx| MainView::new(window, cx));
                *slot.borrow_mut() = Some(main.clone());
                let shell = cx.new(|_| AppShell::new(main));
                gpui_component::Root::new(shell, window, cx)
            })
        };
        vcx.run_until_parked();
        let main = slot.borrow().clone().expect("MainView 未创建");

        // 前提：库里一条连接都没有（不给前提，这条用例会退化成「本来就非空」的假绿）
        vcx.update(|_w, cx| {
            main.update(cx, |m, cx| {
                m.conn_store.profiles.clear();
                m.panel_ui.sidebar_collapsed = false;
                m.panel_ui.panel = PanelKind::Connections;
                cx.notify();
            });
        });
        vcx.run_until_parked();
        assert!(
            main.read_with(vcx, |m, _| m.conn_store.profiles.is_empty()),
            "前提丢了：库里还有连接，测不到「空库下搜索框在不在」"
        );

        let bounds = vcx
            .debug_bounds("conn-search-box")
            .expect("空库下搜索框整个消失了 —— 这正是用户反馈的那个问题");
        assert!(
            f32::from(bounds.size.height) > 0.,
            "搜索框在渲染树里但高度为 0（被压扁了，与消失无异）"
        );

        // 放大镜（2026-10-21 用户要求「搜索栏左侧增加放大镜图标」）：
        // 必须在场、有尺寸、且落在外层搜索框的**左半边**（左右顺序即视觉顺序）。
        let icon = vcx
            .debug_bounds("conn-search-icon")
            .expect("搜索框左侧的放大镜没渲染出来");
        assert!(
            f32::from(icon.size.width) > 0. && f32::from(icon.size.height) > 0.,
            "放大镜图标尺寸为 0（不可见）"
        );
        let icon_mid = f32::from(icon.origin.x) + f32::from(icon.size.width) / 2.;
        let box_mid = f32::from(bounds.origin.x) + f32::from(bounds.size.width) / 2.;
        assert!(
            icon_mid < box_mid,
            "放大镜落在了搜索框的右半边（x={icon_mid}，框中线={box_mid}）—— \
             用户要的是**左侧**图标。"
        );
    }

    /// 真·端到端：**点击栏顶那枚「+ 新建连接」图标**必须打开新建连接对话框，且不得 panic。
    ///
    /// 这条补的是一个长期空档：此前所有对话框用例都直接调
    /// `MainView::open_connect_dialog(..)`，**绕过了按钮本身的 `on_click` 闭包**。
    /// 于是「点了图标就闪退」这类只在真实点击路径上暴露的问题，静态断言与直接调用
    /// 都看不见。做法与 `grp_folder_is_aligned_across_rows` 同一手法：真渲染、量到
    /// 按钮外层 `new-connection-btn` 盒子的矩形、在其中心 `simulate_click`。
    ///
    /// 注意：`simulate_click` 会跑完这条点击路径的所有同步逻辑（清空表单 →
    /// `w.open_dialog` → 渲染新层）。若其中存在「MainView 被借用时又去 update
    /// 同一实体」之类的重入，就会在此处以 `RefCell already borrowed` 暴露 ——
    /// 这正是这条用例要守的东西。
    #[gpui::test]
    fn clicking_the_new_connection_icon_opens_the_dialog(cx: &mut gpui::TestAppContext) {
        use crate::main_view::{AppShell, MainView, PanelKind};
        use gpui::AppContext as _;
        use gpui_component::WindowExt as _;
        use std::cell::RefCell;
        use std::rc::Rc;

        cx.update(|cx| {
            gpui_component::init(cx);
            crate::theme::apply_theme_mode(cx, rustshell_core::theme::ThemeMode::Dark);
        });

        let slot: Rc<RefCell<Option<gpui::Entity<MainView>>>> = Rc::new(RefCell::new(None));
        let (_, vcx) = {
            let slot = slot.clone();
            cx.add_window_view(move |window, cx| {
                let main = cx.new(|cx| MainView::new(window, cx));
                *slot.borrow_mut() = Some(main.clone());
                let shell = cx.new(|_| AppShell::new(main));
                gpui_component::Root::new(shell, window, cx)
            })
        };
        vcx.run_until_parked();
        let main = slot.borrow().clone().expect("MainView 未创建");

        // 面板切到「连接」、侧栏展开 —— 图标才在画面上（否则拿不到矩形）。
        vcx.update(|_w, cx| {
            main.update(cx, |m, _cx| {
                m.panel_ui.sidebar_collapsed = false;
                m.panel_ui.panel = PanelKind::Connections;
            });
        });
        vcx.run_until_parked();

        let bounds = vcx
            .debug_bounds("new-connection-btn")
            .expect("拿不到「+ 新建连接」图标的矩形 —— 图标没渲染出来？");
        assert!(
            f32::from(bounds.size.width) > 0. && f32::from(bounds.size.height) > 0.,
            "「+ 新建连接」图标尺寸为 0，点不到"
        );

        // 点它。若这条路径 panic，用例即红（gpui 测试里 panic 会直接冒出来）。
        vcx.simulate_click(bounds.center(), gpui::Modifiers::default());
        vcx.run_until_parked();

        vcx.update(|window, cx| {
            assert!(
                window.has_active_dialog(cx),
                "点了「+ 新建连接」图标却没打开对话框 —— 点击路径断了（闪退/无响应）。"
            );
        });
        // 从图标进来没有「上下文归属」：库里恰好只有一个分组（初始化时的 `default`）
        // 就落到它，多于一个则未分组（2026-10-21「默认 default 文件夹」）。
        // 这里按**当时库里的实际分组数**断言，而不是写死某个 id ——
        // 用例不该依赖本机 `connections.json` 长什么样。
        let (got, expected) = main.read_with(vcx, |m, _| {
            let got = m.io.dlg_group_id.clone();
            let expected = if m.conn_store.groups.len() == 1 {
                Some(m.conn_store.groups[0].id.clone())
            } else {
                None
            };
            (got, expected)
        });
        assert_eq!(
            got, expected,
            "从「+」图标进来的新建连接：单分组时必须落到那个分组，多分组/无分组时未分组。"
        );

        // 关掉再点一次：这条「重复开合」路径最易暴露两类老问题 ——
        //   ① `present_dialog` 的 `has_active_dialog` 守卫把第二次静默挡掉；
        //   ② 关闭时 `on_close` 回收业务状态与再次打开抢占同一借用，触发
        //      `RefCell already borrowed` 的重入 panic。二者都只在真实点击
        //      路径上才出现，直接调 `open_connect_dialog` 的用例看不见。
        main.update_in(vcx, |m, window, cx| {
            m.close_dialog_ui(window, cx);
        });
        vcx.run_until_parked();

        let bounds2 = vcx
            .debug_bounds("new-connection-btn")
            .expect("关掉对话框后图标不见了？");
        vcx.simulate_click(bounds2.center(), gpui::Modifiers::default());
        vcx.run_until_parked();
        vcx.update(|window, cx| {
            assert!(
                window.has_active_dialog(cx),
                "关掉后再点「+」图标没再打开对话框（守卫卡住或重入失败）。"
            );
        });
    }

    // ---------- 连接树输入指纹（2026-09 性能修复） ----------
    fn store_with(profiles: &[(&str, &str, &str, &str, Option<&str>)]) -> ConnectionStore {
        ConnectionStore {
            groups: vec![ConnectionGroup {
                id: "g1".into(),
                name: "生产".into(),
                parent: None,
            }],
            profiles: profiles
                .iter()
                .map(|(id, name, host, user, group)| ConnectionProfile {
                    id: (*id).into(),
                    name: (*name).into(),
                    host: (*host).into(),
                    port: 22,
                    username: (*user).into(),
                    password: None,
                    stored_password: None,
                    key_path: None,
                    notes: String::new(),
                    group_id: group.map(|g| g.to_string()),
                    legacy_group: None,
                    last_used_at: 0,
                    tags: Vec::new(),
                    forwards: Vec::new(),
                    session_log: None,
                    proxy: Default::default(),
                    auto_reconnect: None,
                })
                .collect(),
            ..Default::default()
        }
    }

    /// ★ 指纹必须**稳定**：同一状态两次计算必须相同 —— 否则「没变就不重建」的
    /// 前提失效，树会退化成每帧重建（并且每帧清空选中态）。
    ///
    /// 展开集合是 `HashSet`，迭代顺序不保证：指纹里必须排序后才拼，
    /// 否则两次插入顺序不同就会算出两个指纹。
    #[test]
    fn conn_tree_key_is_stable_across_hashset_iteration_order() {
        use std::collections::HashSet;
        let store = store_with(&[("p1", "web", "10.0.0.1", "root", Some("g1"))]);

        let mut a: HashSet<String> = HashSet::new();
        for id in ["g1", "g2", "g3", "g4", "g5", "g6", "g7", "g8"] {
            a.insert(id.to_string());
        }
        let mut b: HashSet<String> = HashSet::new();
        for id in ["g8", "g7", "g6", "g5", "g4", "g3", "g2", "g1"] {
            b.insert(id.to_string());
        }
        // 前提：两个集合确实可能迭代顺序不同（不同插入顺序 → 不同桶序）
        assert_eq!(
            conn_tree_key("", &a, &store),
            conn_tree_key("", &b, &store),
            "展开集合的插入顺序不得影响指纹"
        );
        // 同一集合重复计算也必须相同
        assert_eq!(
            conn_tree_key("q", &a, &store),
            conn_tree_key("q", &a, &store)
        );
    }

    /// 指纹必须对**每一种会改变树的输入**敏感：搜索串、展开态、以及影响行显示或
    /// 归属的连接字段。少一种就会出现「改了东西但树不刷新」的假死。
    #[test]
    fn conn_tree_key_reacts_to_every_tree_input() {
        use std::collections::HashSet;
        let base_store = store_with(&[("p1", "web", "10.0.0.1", "root", Some("g1"))]);
        let empty: HashSet<String> = HashSet::new();
        let mut expanded: HashSet<String> = HashSet::new();
        expanded.insert("g1".into());

        let base = conn_tree_key("", &empty, &base_store);

        // 1) 搜索串
        assert_ne!(base, conn_tree_key("web", &empty, &base_store), "搜索串");
        // 2) 展开态（折叠时树里根本看不到子节点）
        assert_ne!(base, conn_tree_key("", &expanded, &base_store), "展开态");

        // 3) 连接显示字段 / 归属
        let cases = [
            store_with(&[("p1", "web2", "10.0.0.1", "root", Some("g1"))]),
            store_with(&[("p1", "web", "10.0.0.2", "root", Some("g1"))]),
            store_with(&[("p1", "web", "10.0.0.1", "admin", Some("g1"))]),
            store_with(&[("p1", "web", "10.0.0.1", "root", None)]),
            store_with(&[]),
        ];
        for (i, s) in cases.iter().enumerate() {
            assert_ne!(base, conn_tree_key("", &empty, s), "第 {i} 个字段变化");
        }

        // 4) 分组改名 / 换父（分组 id 不变时也要被察觉）
        let mut renamed = base_store.clone();
        renamed.groups[0].name = "生产环境".into();
        assert_ne!(base, conn_tree_key("", &empty, &renamed), "分组改名");

        // 5) 备注（2026-09-18 新增字段）：它是**行的显示内容**（hover 提示），
        //    漏进指纹就会出现「改完备注，树不重建，鼠标划过去还是旧内容」。
        let mut noted = base_store.clone();
        noted.profiles[0].notes = "跳板机，到期 2026-10-01".into();
        assert_ne!(base, conn_tree_key("", &empty, &noted), "备注");

        // 6) 次序（上下移动排序）：平铺次序就是树上次序，指纹必须跟着变，
        //    否则右键「上移 / 下移」落了库、树却按旧次序渲染 到 用户以为没生效。
        let two = store_with(&[
            ("p1", "web", "10.0.0.1", "root", Some("g1")),
            ("p2", "db", "10.0.0.2", "root", Some("g1")),
        ]);
        let mut swapped = two.clone();
        swapped.profiles.swap(0, 1);
        let ordered = conn_tree_key("", &empty, &two);
        assert_ne!(ordered, conn_tree_key("", &empty, &swapped), "连接次序");

        let mut groups_swapped = two.clone();
        groups_swapped.groups.push(ConnectionGroup {
            id: "g2".into(),
            name: "开发".into(),
            parent: None,
        });
        let mut groups_other = groups_swapped.clone();
        groups_other.groups.swap(0, 1);
        assert_ne!(
            conn_tree_key("", &empty, &groups_swapped),
            conn_tree_key("", &empty, &groups_other),
            "分组次序"
        );
    }

    /// 回归（2026-09-18 用户要求「分组右侧去掉数字显示」）：
    /// 分组行不再渲染成员数徽标。
    ///
    /// 徽标数的是**直接子节点**（子分组 + 成员），嵌套之后这个数字既不代表
    /// 「这个组里有多少连接」，又常被省略号挤成「1…」；展开一眼就能看到有什么。
    #[test]
    fn group_row_has_no_member_count_badge() {
        let code = prod(include_str!("connections_panel.rs"));
        assert!(
            !code.contains("item.children.len()"),
            "分组行仍在渲染成员数徽标：用户要求去掉右侧数字显示。"
        );
    }

    /// 回归（2026-09-18 用户要求「连接选中框外线有时候一直停留在那，切换了也没用，
    /// 顺便改为圆角」）：两条一起守。
    ///
    /// 1. **圆角**：官方 `ListItem` 用本元素的 `corner_radii` 同时画选中底色与那圈
    ///    外线，所以两行各挂一处 `.rounded(px(SELECTION_RADIUS))` 就够；
    /// 2. **不跳行**：展开/折叠会让条目下标整体位移，库的选中态是按下标存的，
    ///    事件后必须重建并按连接 id 对齐（`MainView::invalidate_conn_tree`）。
    ///
    /// 「右击外线粘住」在 0.8.0 已不会复现：官方 `Tree` 逐行包的 `ContextMenu`
    /// 在右键冒泡时先 `stop_propagation()`，库自己的 `right_clicked_ix` 监听根本
    /// 收不到这次按下 —— 外线（`secondary_selected`）从源头上就不会被点亮。
    #[test]
    fn selection_box_is_rounded_and_never_sticks() {
        let code = prod(include_str!("connections_panel.rs"));
        assert_eq!(
            code.matches("rounded(px(SELECTION_RADIUS))").count(),
            2,
            "分组行与连接行都要圆角（各一处）"
        );
        assert!(
            code.contains(".context_menu(tree_row_ctx_menu)"),
            "行右键必须走官方逐行菜单：容器级菜单收不到 0.8.0 的行右键"
        );

        let actions = prod(include_str!("main_view/connections.rs"));
        assert!(
            actions.contains("pub fn invalidate_conn_tree")
                && actions.contains("conn_tree_key = None"),
            "`invalidate_conn_tree` 必须把树指纹置空（下一帧重建）"
        );

        let main = prod(include_str!("main_view.rs"));
        assert!(
            main.contains("m.invalidate_conn_tree(cx)"),
            "展开/折叠事件后必须重建并按连接 id 对齐选中行（否则选中框会跳到别的连接上）"
        );
    }

    /// ★ 回归护栏：**行标签必须单行**。
    ///
    /// 官方 `Tree` 用的是 uniform 列表（每行等高），而 `text_ellipsis()` 在 gpui 里
    /// 只设截断、**不管换行**：长连接名（「docker ssh3copycopycopycopy」）在深缩进下
    /// 会在空格处折成两行，把该行撑到两倍高 —— 直接压在上下相邻行上（2026-10-20 反馈
    /// 「分组下的连接文字互相重叠」）。所以每个 `.text_ellipsis()` 都要配一枚
    /// `.whitespace_nowrap()`，分组行与连接行各一处。
    #[test]
    fn row_labels_are_single_line() {
        let code = prod(include_str!("connections_panel.rs"));
        let ellipsis = code.matches(".text_ellipsis()").count();
        let nowrap = code.matches(".whitespace_nowrap()").count();
        assert_eq!(ellipsis, 2, "分组行与连接行的名称各一处省略号截断");
        assert!(
            nowrap >= ellipsis,
            "每个省略号截断都要配一枚 nowrap，否则长名字折行会撑高 uniform 行\
            （名称 {ellipsis} 处 / nowrap {nowrap} 处）"
        );
    }

    /// ★ 回归护栏：**禁止每帧**重建树并推 `set_items`。
    ///
    /// `TreeState::set_items` 在库内会 `selected_ix = None; right_clicked_ix = None`，
    /// 每帧调用等于「树的选中态每帧被清空」，键盘上下导航永远从无选中重来。
    /// 这里用源码结构把「先看指纹再重建」这件事钉死。
    #[test]
    fn tree_items_are_rebuilt_only_when_the_input_key_changes() {
        let code = prod(include_str!("connections_panel.rs"));
        assert!(
            code.contains("let rebuild ="),
            "连接面板必须先算出「是否需要重建」（let rebuild = ...）"
        );
        let guard = code.find("if rebuild {").expect("找不到 rebuild 闸门");
        let set_items = code
            .find("ts.set_items(items, cx);")
            .expect("找不到 set_items 调用");
        assert!(
            guard < set_items,
            "set_items 必须落在 `if rebuild {{}}` 闸门**之内**：每帧推送会清空树的选中态。"
        );
        // 闸门到 set_items 之间不应出现右花括号收尾（说明 set_items 在闸门内）
        let between = &code[guard..set_items];
        let depth_ok = between.matches('{').count() > between.matches('}').count();
        assert!(
            depth_ok,
            "set_items 不在 `if rebuild` 块内（闸门已闭合），等于每帧重建。"
        );
        assert!(
            code.contains("conn_tree_key"),
            "重建决策必须基于 `conn_tree_key` 指纹"
        );

        // 重建会清掉库内部按下标记的选中态，必须紧接着按**连接 id** 对齐回来：
        // 少了这一步，每次改名/排序/展开之后选中框都会凭空消失。
        let realign = code
            .find("ts.set_selected_index(ix, cx);")
            .expect("set_items 之后必须按连接 id 重新对齐选中下标");
        assert!(
            set_items < realign,
            "对齐必须发生在 `set_items` **之后**（后者才会清空 selected_ix）。"
        );
        assert!(
            code.contains(r#"SharedString::from(format!("prof:{pid}"))"#),
            "对齐的锚点必须是连接节点 id，不能靠下标硬记。"
        );
    }

    /// 回归护栏（用户要求「增加连接组和连接上下移动排序的功能」）：
    /// 分组行与连接行的右键菜单都要有「上移 / 下移」，首 / 末位对应置灰，
    /// 且动作必须真的落到 `MainView` 并持久化。
    ///
    /// 一条断言串起四处 —— 缺任何一处，这个功能都会是「看着有、按了没反应」：
    ///   1) 两个菜单都挂了这两项（首末置灰的判据只写在一个地方）；
    ///   2) 位置来自渲染快照的 `positions`（菜单跑在弹层上下文，就地查库会 E0500）；
    ///   3) 位置按**同级**算（core 的 `group_position` / `profile_position`）；
    ///   4) 落点是 `MainView::move_group_step` / `move_connection_step`，且都落库。
    #[test]
    fn tree_menu_offers_move_up_down_for_groups_and_profiles() {
        let code = prod(include_str!("connections_panel.rs"));

        // 1) 两个菜单都挂了上移 / 下移（`fn X(` 到函数闭合的 `\n}` 之间）
        let body_of = |name: &str| -> &str {
            let start = code
                .find(name)
                .unwrap_or_else(|| panic!("找不到 {name}（结构锚点）"));
            let rest = &code[start..];
            let end = rest.find("\n}").unwrap_or(rest.len());
            &rest[..end]
        };
        for menu in ["fn profile_menu(", "fn group_menu("] {
            let body = body_of(menu);
            assert!(
                body.contains("move_up_item(") && body.contains("move_down_item("),
                "{menu} 缺少「上移 / 下移」：分组与连接都必须能上下排序。"
            );
        }

        // 判据只能有一处（首项置灰上移、末项置灰下移），两个菜单各调用一次
        assert_eq!(
            code.matches("move_up_item(idx == 0,").count(),
            2,
            "「上移」必须按同级序号置灰（首位不得再上移），且分组 / 连接两处都要判"
        );
        assert_eq!(
            code.matches("move_down_item(len == 0 || idx + 1 >= len,")
                .count(),
            2,
            "「下移」必须按同级序号置灰（末位不得再下移），且分组 / 连接两处都要判"
        );

        // 2) 位置随渲染快照全局缓存（弹层上下文读不到 MainView），且搜索态不填
        assert!(
            code.contains(r#"snap.positions.insert(format!("prof:{}", p.id), pos)"#)
                && code.contains(r#"snap.positions.insert(format!("grp:{}", g.id), pos)"#),
            "同级位置必须随快照一起缓存（分组与连接两种节点都要收），\
             否则菜单只能就地查库 到 E0500。"
        );
        assert!(
            code.contains("if !searching {"),
            "位置表必须只在无搜索时收集：过滤后的可见兄弟只是子集，\
             按全量挪一格时界面上可能原地不动（点了没反应）。"
        );
        assert!(
            code.contains("s.positions.get(&id).copied()"),
            "菜单必须从快照里取位置（`positions.get(id)`）作为置灰依据。"
        );

        // 3) 位置口径来自 core（同级兄弟 / 同组成员），不在 UI 里另算一套
        for needle in ["profile_position(&p.id)", "group_position(&g.id)"] {
            assert!(
                code.contains(needle),
                "同级位置必须取自 core 的 `{needle}`（与树上显示次序同源）。"
            );
        }

        // 4) 落点：MainView 动作 + core 排序 + 落库
        let actions = prod(include_str!("main_view/connections.rs"));
        for needle in [
            "pub fn move_connection_step",
            "pub fn move_group_step",
            "move_profile_step(profile_id, delta)",
            "move_group_step(group_id, delta)",
        ] {
            assert!(
                actions.contains(needle),
                "排序动作缺少落点 `{needle}` —— 菜单点了不会改变任何东西。"
            );
        }
        assert!(
            code.contains("ui_call_move_connection_step")
                && code.contains("ui_call_move_group_step"),
            "菜单必须经 App 上下文入口（`ui_call_move_*_step`）回到 MainView。"
        );
    }

    /// 回归（2026-09-20 用户反馈「有连接文件夹和无连接文件夹位置不齐」）：
    /// 展开箭头列只能有**一个**宽度来源。
    ///
    /// 成因：空分组原先自己写了个 `div().w(px(16.))` 占位（凭「图标大约 16px」的印象），
    /// 而 `Icon::small()` 的渲染盒是 `size_3p5()` = 0.875rem = 14px
    /// （`gpui-component/src/icon.rs::into_svg`），于是空分组那一行的文件夹图标
    /// 及其后所有内容整体右移 2 逻辑 px（截图逐列投影实测 4 物理 px @150%）。
    ///
    /// 两条判据各挡一种倒退：占位宽度不得再就地写死（必须走 `expand_slot`）、
    /// 槽宽常量必须还是 14。
    #[test]
    fn chevron_column_has_a_single_width_source() {
        let code = prod(include_str!("connections_panel.rs"));
        assert!(
            code.contains("const EXPAND_SLOT_WIDTH: f32 = 14.;"),
            "展开槽宽度常量不见了或被改值了：它必须等于 `Icon::small()` 的渲染盒 \
             （gpui-component `Size::Small` ⇒ `size_3p5()` = 0.875rem = 14px）。"
        );
        assert_eq!(
            code.matches("expand_slot(").count(),
            2,
            "`expand_slot` 应该只有「一处定义 + 一处调用」：分组行无论有没有子节点都必须走它，\
             各写各的占位就会重新错位。"
        );
        for bad in ["div().w(px(16.))", "div().w(px(15.))", "div().w(px(17.))"] {
            assert!(
                !code.contains(bad),
                "又出现了写死的展开占位宽度 `{bad}` —— 占位必须与 chevron 同宽才能对齐。"
            );
        }
    }

    /// 端到端几何：空分组行与有子分组行的**文件夹图标必须落在同一个 x 上**。
    ///
    /// 量的是生产代码里特意挂在文件夹图标外层的 `grp-folder:<id>` 盒子
    /// （`debug_selector`，正式构建里不产生任何东西）。两行同为根级分组、
    /// `pl(indent)` 相同，唯一变量就是展开槽 —— 之所以要真渲染一次而不是只看常量：
    /// 这个 bug 的成因正是「空分组走了**另一条**路径」，常量护栏守不住路径分叉。
    ///
    /// `debug_bounds` 只吃 `&'static str`，而 selector 是带分组 id 的动态串；
    /// 测试侧把这几个串 `Box::leak` 成静态的即可查同一张表（map 的 key 是 String，
    /// 按 `&str` 查得中）。
    #[gpui::test]
    fn grp_folder_is_aligned_across_rows(cx: &mut gpui::TestAppContext) {
        use crate::main_view::{AppShell, MainView, PanelKind};
        use gpui::AppContext as _;
        use std::cell::RefCell;
        use std::rc::Rc;

        cx.update(|cx| {
            gpui_component::init(cx);
            crate::theme::apply_theme_mode(cx, rustshell_core::theme::ThemeMode::Dark);
        });

        let slot: Rc<RefCell<Option<gpui::Entity<MainView>>>> = Rc::new(RefCell::new(None));
        let (_, vcx) = {
            let slot = slot.clone();
            cx.add_window_view(move |window, cx| {
                let main = cx.new(|cx| MainView::new(window, cx));
                *slot.borrow_mut() = Some(main.clone());
                let shell = cx.new(|_| AppShell::new(main));
                gpui_component::Root::new(shell, window, cx)
            })
        };
        vcx.run_until_parked();
        let main = slot.borrow().clone().expect("MainView 未创建");

        // g1 挂 2 条连接（有子节点 ⇒ 画 chevron）；g2 是空分组（无子节点 ⇒ 走占位槽）。
        // 两个都是根级分组、都没有搜索词（无搜索时空分组也会显示，见 core 的
        // `visible_tree_filters_members_and_hides_empty_groups`）。
        vcx.update(|_w, cx| {
            main.update(cx, |m, cx| {
                m.panel_ui.sidebar_collapsed = false;
                m.panel_ui.panel = PanelKind::Connections;
                let mut store = store_with(&[
                    ("p1", "web", "10.0.0.1", "root", Some("g1")),
                    ("p2", "db", "10.0.0.2", "root", Some("g1")),
                ]);
                store.groups.push(ConnectionGroup {
                    id: "g2".into(),
                    name: "空文件夹".into(),
                    parent: None,
                });
                m.conn_store = store;
                m.conn_ui.conn_tree_key = None; // 强制下一帧重建树
                cx.notify();
            })
        });
        vcx.run_until_parked();

        let folder_left = |vcx: &mut gpui::VisualTestContext, id: &str| -> f32 {
            let sel: &'static str = Box::leak(format!("grp-folder:{id}").into_boxed_str());
            let bounds = vcx
                .debug_bounds(sel)
                .unwrap_or_else(|| panic!("找不到 `{sel}` 的边界 —— 连接树没渲染出来？"));
            f32::from(bounds.origin.x)
        };
        let with_children = folder_left(vcx, "g1");
        let empty = folder_left(vcx, "g2");
        assert_eq!(
            with_children,
            empty,
            "有子分组的行与空分组的行，文件夹图标没落在同一列：{with_children} vs {empty}\
             （差 {}px）。空分组必须用与 chevron 同宽的槽占位（`EXPAND_SLOT_WIDTH`）。",
            (with_children - empty).abs()
        );
    }
}
