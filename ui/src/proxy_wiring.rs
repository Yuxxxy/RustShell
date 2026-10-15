//! F-10 代理设置的护栏（契约 / 行为 / 接线三层），仅测试构建编译。
//!
//! # 被钉住的四条规矩
//!
//! 1. **三态合成只有一处落点**（`core::proxy::resolve`，由 `ssh::connect_only` 调）：
//!    界面把**全局值**交给 core，自己绝不解析。这条链上有四个调用点（会话 / 重连 /
//!    连接测试 / 任务与 agent 后端），谁自己解析一次、漏一个，就是「会话走代理、
//!    定时任务直连」这种最难发现的半生效 —— 传全局值、由 core 合成，新增调用点
//!    自动继承正确行为（规格 §1.10 ③）。
//! 2. **代理口令只以密文引用落盘**：面板里不得出现 `password_ref`（密文不可逆，
//!    摆进面板只会诱使人以为能读回明文）；连接级与全局**各查各的键**
//!    （`secret_key_for` / `GLOBAL_SECRET_KEY`），互不覆盖（规格 §1.10 ⑧⑨）。
//! 3. **新建对话框要清掉整份代理草稿**（四个输入框 + 档位 + 类型 + 来源快照），
//!    编辑则原样回填 —— 漏了清态 = 「新建带出上一条连接代理的地址与用户名」，
//!    与「新建带出上一条口令」是同一类事故（规格 §1.10 ⑧）。
//! 4. **「留空 = 不改口令」只在代理目标没变时成立**：类型 / 地址 / 端口任一不同，
//!    旧口令就必须跟着走（连带运行期明文一起清）。判定只许走 core 的
//!    `may_inherit_password` —— 面板与对话框各写一遍，两边迟早在边界上分叉
//!    （一边继承、一边不继承，用户看到的是「同一个操作两种结果」）。
//!
//! 扫源码一律走 [`crate::test_support::prod`]（剥注释 + 切测试区），否则针会
//! 命中本文件自己（见 `test_support` 模块文档）。行为层（草稿的清态 / 回填 /
//! 端口唯一读法）在 `main_view::dialogs` 的用例里 —— 那边有生产同构的窗口
//! （`build_main`）与真正的输入框实体，这里只钉「接线有没有接上、接在哪」。
//!
//! 编译层的合成测试（`resolve` 三态 × 全局有无、`may_inherit_password` 的
//! 目标指纹、假代理三段握手）在 `core/src/proxy.rs`，不在这里重复。

use crate::test_support::{prod, region};

// ---------------------------------------------------------------- 接线层

/// 三个连接入口 + 两条无人值守链路都必须把**全局值**交给 core。
///
/// 少一个的后果各不相同，但都不会在界面上露出来：
/// 会话入口漏了 ⇒ 用户配了代理却还是连不上（以为代理没生效）；
/// 连接测试漏了 ⇒ 测试走直连、真连走代理，「测试通过」变成假绿；
/// 任务 / agent 后端漏了 ⇒ 手点能跑、到点跑不通，且报错只说「连不上」。
#[test]
fn every_connection_path_hands_core_the_global_proxy() {
    let session = prod(include_str!("main_view/connections.rs"));
    let dialogs = prod(include_str!("main_view/dialogs.rs"));
    let tasks = prod(include_str!("main_view/task_actions.rs"));
    let ai = prod(include_str!("main_view/ai_actions.rs"));
    let main = prod(include_str!("main_view.rs"));
    let actions = prod(include_str!("main_view/settings_actions.rs"));

    // 会话与重连：同一个 `connect_in_background` 的两个入口，各传一次。
    // 这里用等号：又加一个建连入口是**好事**，但那条路必须同样带上全局代理
    // —— 撞红时请把新入口的调用点补上再改这个数字。
    assert_eq!(
        session.matches("self.runtime_global_proxy()").count(),
        2,
        "会话 / 重连两个入口都要把全局代理交给 core（少一个 = 那条路静默直连）"
    );
    assert!(
        dialogs.contains("self.runtime_global_proxy()"),
        "连接测试（`test_connection`）没有带全局代理：\n\
         测试的结果会与真实连接不一致 —— 用户按测试结论去连，照样连不上。"
    );
    assert!(
        tasks.contains("self.runtime_global_proxy()"),
        "定时任务后端（`RealBackend`）没有带全局代理：同一个任务手点能连、到点连不上。"
    );
    assert!(
        ai.contains("global_proxy: self.runtime_global_proxy(),"),
        "AI agent 的 `SshExec` 没有带全局代理：agent 的每一条命令都会直连出去。"
    );

    // 界面**不许**自己合成三态：合成只此一处（`connect_only` → `resolve`）。
    // 一旦界面开始传「已经解析好的代理」，四个调用点就会各解析一遍，
    // 而漏掉的那个不会报错、只会静默直连。
    for (name, src) in [
        ("main_view/connections.rs", &session),
        ("main_view/dialogs.rs", &dialogs),
        ("main_view/task_actions.rs", &tasks),
        ("main_view/ai_actions.rs", &ai),
        ("main_view.rs", &main),
    ] {
        assert!(
            !src.contains("proxy::resolve("),
            "{name} 自己解析了代理三态 —— 合成必须留在 `core::ssh::connect_only` 一处。"
        );
    }

    // 运行期形态必须**现算**（每次调用现解密），不是启动时缓存一份：
    // 缓存的那份会让「改了口令、新连接还是旧口令」有藏身处。
    let rt = region(
        &actions,
        "pub(super) fn runtime_global_proxy",
        "pub(super) fn sync_smtp_config",
    );
    assert!(
        rt.contains("runtime_proxy(") && rt.contains("secret::unprotect"),
        "运行期代理必须现从设置里解出明文（`settings.runtime_proxy(unprotect)`）。"
    );
}

/// 设置面板的代理分区：绑定 `settings.proxy` 本体，口令走不回显输入框。
///
/// 与 `notification_page_binds_to_smtp_config` 同形（规格 §1.10 ⑧ 点名了这条护栏）：
/// `SettingField` 的 getter/setter 顺手写成一份 UI 私有的临时状态时，面板上改得动、
/// 落盘的却不是这一份 —— 而界面看起来一切正常，用户以为代理改好了。
#[test]
fn the_proxy_panel_binds_to_settings_and_never_shows_the_cipher() {
    let panel = prod(include_str!("main_view/settings_panel.rs"));
    let actions = prod(include_str!("main_view/settings_actions.rs"));

    assert!(
        panel.contains("settings.proxy.is_some()") && panel.contains("settings.proxy = None"),
        "面板必须直接读写 `settings.proxy`（开关的关 = `None`，规格 §1.10 ④）。"
    );
    assert!(
        panel.contains("rustshell_core::proxy::ProxyConfig::default()"),
        "打开开关要落一份默认配置（地址留空由用户填）：\n\
         替用户猜一个代理地址，等于让他点开开关就再也连不上任何机器。"
    );
    assert!(
        panel.contains("proxy_cfg_pass"),
        "代理口令必须走不回显的 `MainView::proxy_cfg_pass`（与 `smtp_cfg_pass` 同一条纪律）。"
    );
    assert!(
        !panel.contains("password_ref"),
        "设置面板不得出现 `password_ref`：密文不可逆，摆进面板只会诱使人以为能读回明文。"
    );
    assert!(
        !panel.contains("proxy.password"),
        "面板不得直接读写 `proxy.password`（运行期明文只由 `sync_proxy_config` 在保存时写入）。"
    );

    // 类型必须是真的循环按钮（两类型都要能切到），不是只显示当前值的标签
    assert!(
        panel.contains(r#"Button::new("set-proxy-kind")"#) && panel.contains("ProxyKind::ALL"),
        "「类型」必须是循环按钮且遍历 `ProxyKind::ALL`：只显示当前值 = SOCKS5 永远选不到。"
    );

    // 落盘收口：一个函数管加密 / 校验 / 沿用，返回 `Err` 时面板保持打开
    let sync = region(
        &actions,
        "pub(super) fn sync_proxy_config",
        "pub fn toggle_theme",
    );
    assert!(
        sync.contains("normalize()") && sync.contains("validate()"),
        "`sync_proxy_config` 要先 `normalize` 再去空白、再 `validate`：\n\
         手改 `settings.json` 写出空地址 / 端口 0 / 超长地址要在保存时就地报错（规格 §1.10 ⑨）。"
    );
    assert!(
        sync.contains("GLOBAL_SECRET_KEY"),
        "全局代理的密文键必须是 `GLOBAL_SECRET_KEY`（`proxy-global`）。"
    );
    assert!(
        !sync.contains("secret_key_for"),
        "全局代理不该用连接级的 `secret_key_for`：键串了，全局与连接级会互相覆盖口令。"
    );
    assert!(
        sync.contains("may_inherit_password"),
        "沿用判定必须走 core 的 `may_inherit_password`（面板与对话框同一份纯函数）。"
    );
    assert!(
        sync.contains("cfg.password_ref = None;") && sync.contains("cfg.password = None;"),
        "目标变了要连**运行期明文**一起清：只清 `password_ref`，本次会话里新建的连接\n\
         会带着旧命令去敲新代理（用户以为已经清掉了）。"
    );

    // 打开与回滚都要回填：少一个，第二次打开面板就会拿着上一次的明文与旧快照做判定
    assert_eq!(
        actions.matches("self.prefill_proxy_inputs(w, cx);").count(),
        2,
        "`prefill_proxy_inputs` 必须同时接在打开面板与取消回滚两条路上。"
    );
    let prefill = region(
        &actions,
        "pub(super) fn prefill_proxy_inputs",
        "pub(super) fn sync_proxy_config",
    );
    assert!(
        prefill.contains("set_value(\"\"") && prefill.contains("self.settings.proxy.clone()"),
        "回填必须清空口令框并**留一份来源快照**（`proxy_cfg_prev`）：\n\
         面板里的地址/端口是双向绑定的，没有快照就无从知道「用户改没改代理目标」。"
    );
}

/// 连接对话框：新建清态、编辑回填、保存定稿，三处缺一不可。
#[test]
fn the_connect_dialog_clears_and_restores_the_whole_proxy_draft() {
    let session = prod(include_str!("main_view/connections.rs"));
    let dialogs = prod(include_str!("main_view/dialogs.rs"));

    // ① 新建：四个输入框进清态表
    let clear = region(
        &session,
        "pub fn open_connect_dialog(",
        "dlg_forwards.clear();",
    );
    for field in [
        "&self.io.dlg_proxy_host",
        "&self.io.dlg_proxy_port",
        "&self.io.dlg_proxy_user",
        "&self.io.dlg_proxy_pass",
    ] {
        assert!(
            clear.contains(field),
            "新建对话框的清态表里漏了 `{field}`：新建会带着上一条连接的代理配置，\n\
             用户稍不留神就把新连接挂到别人的代理上。"
        );
    }
    // ② 新建：草稿本体（不在清态表里，是另外几个字段）
    let body = region(&session, "dlg_forwards.clear();", "dlg_notes");
    assert!(
        body.contains("dlg_proxy_mode = rustshell_core::proxy::ProxyMode::FollowGlobal;")
            && body.contains("dlg_proxy_kind = rustshell_core::proxy::ProxyKind::Http;")
            && body.contains("dlg_proxy_prev = None;"),
        "新建必须把档位 / 类型 / 来源快照一并回零：\n\
         档位留着「自定义」而输入框是空的，会存出一条指向空地址的代理配置。"
    );

    // ③ 编辑：回填 + 来源快照
    let edit = region(
        &session,
        "pub(super) fn open_connect_dialog_for_profile",
        "dlg_group_id = p.group_id.clone();",
    );
    for field in [
        "dlg_proxy_mode = p.proxy.mode();",
        "dlg_proxy_host",
        "dlg_proxy_port",
        "dlg_proxy_user",
        "dlg_proxy_pass",
    ] {
        assert!(
            edit.contains(field),
            "编辑回填漏了 `{field}`：打开对话框看到的不是这条连接真正的代理。"
        );
    }
    assert!(
        edit.contains("dlg_proxy_prev = prev;"),
        "编辑没有留下来源快照：`may_inherit_password` 会把「没改」判成「新建」，\n\
         于是「留空 = 不改口令」直接失效（用户一保存就丢口令）。"
    );

    // ④ 对话框侧：唯一读法、保存定稿、高级控件已退场
    assert_eq!(
        dialogs.matches("self.dlg_proxy_draft(cx)").count(),
        2,
        "连接测试 / 保存定稿两处必须共用同一份读法：\n\
         各判一遍端口就会各有一种口径（`abc` 在一处报错、在另一处变成默认值）。\n\
         （原先还有第三个消费点 —— 渲染的「生效」投影，随「高级」界面隐藏，2026-09-29。）"
    );
    assert!(
        dialogs.contains("ProxyChoice::from_parts(self.io.dlg_proxy_mode"),
        "保存必须走 `from_parts` 合成三态：非自定义档位要**丢弃**草稿本体（规格 §1.10 ③）。"
    );
    assert_eq!(
        dialogs.matches("p.proxy = proxy").count(),
        2,
        "编辑与新建两条分支都要写回代理（少一条，那一半的连接存下去就丢了代理）。"
    );
    assert!(
        dialogs.contains("secret_key_for(&secret_key)"),
        "连接级代理口令要按 profile id 查键（与全局的 `proxy-global` 互不覆盖）。"
    );
    assert!(
        dialogs.contains("may_inherit_password"),
        "对话框的「留空 = 不改口令」必须走 core 的同一份判定。"
    );
    // ⑤ 高级控件已退场（2026-09-29 用户要求「保持简单」），但判定逻辑全部留着
    for gone in [
        r#"Button::new("dlg-proxy-mode")"#,
        r#"Button::new("dlg-proxy-kind")"#,
        "生效：",
        "dlg-connect-advanced",
    ] {
        assert!(
            !dialogs.contains(gone),
            "「高级」内容已隐藏，连接对话框里不该再有 `{gone}`；\n\
             删界面别顺手删数据链路 —— 上面的回填 / 清态 / 保存断言才是这条测试的主干。"
        );
    }
}
