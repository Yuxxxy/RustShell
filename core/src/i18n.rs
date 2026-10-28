//! 轻量国际化（v1：中文/英文）。零依赖、运行时可切换。
//!
//! 边界：本表只收「会渲染给用户看」的文案。开发文本（`assert!` / `panic!` /
//! `log::` / `watchdog` 阶段名 / 单测断言消息）不进表，也不要顺手 `t()` 化——
//! 它们面向开发者，且源码扫描断言会把它们当缺口报出来（见 tests 的 G1..G4）。

use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Lang {
    #[default]
    Zh,
    En,
}

static LANG: AtomicU8 = AtomicU8::new(0);

pub fn lang() -> Lang {
    match LANG.load(Ordering::Relaxed) {
        1 => Lang::En,
        _ => Lang::Zh,
    }
}

/// 由字符串（settings.language）设置语言；非法值回退中文。
pub fn set_lang_str(s: &str) {
    match s.to_ascii_lowercase().as_str() {
        "en" | "english" => LANG.store(1, Ordering::Relaxed),
        _ => LANG.store(0, Ordering::Relaxed),
    }
}

pub fn set_lang(l: Lang) {
    LANG.store(
        match l {
            Lang::En => 1,
            Lang::Zh => 0,
        },
        Ordering::Relaxed,
    );
}

/// 翻译：中文 key 到 当前语言文案。En 缺映射时回退中文。
pub fn t(zh: &'static str) -> &'static str {
    if lang() == Lang::Zh {
        return zh;
    }
    translate(zh)
}

/// 成对文案选择（用于带插值的组合字符串）：中文场景取 zh，英文场景取 en。
pub fn tp(zh: &'static str, en: &'static str) -> &'static str {
    if lang() == Lang::Zh {
        zh
    } else {
        en
    }
}

/// 当前是否英文：纯动态插值文案用`if is_en()`分支，避免 tp 的 &'static str 限制
pub fn is_en() -> bool {
    lang() == Lang::En
}

/// 中文/英文二选一（返回 String，支持带插值的动态文案）
pub fn ts(zh: String, en: String) -> String {
    if lang() == Lang::Zh {
        zh
    } else {
        en
    }
}

/// 可选文案：`Some((中文, 英文))` 时按当前语言取一个，`None` 保持 `None`。
///
/// 存在的意义是让「有值才渲染」的分支不必每处都写 `if is_en() { .. } else { .. }`：
/// 那种写法在代码里散开之后，漏一个分支的表现是英文界面里冒出中文。
pub fn ts_opt(pair: Option<(String, String)>) -> Option<String> {
    pair.map(|(zh, en)| if is_en() { en } else { zh })
}

fn translate(zh: &'static str) -> &'static str {
    match zh {
        // 通用
        "取消" => "Cancel",
        "创建" => "Create",
        "保存" => "Save",
        // 「保存并连接」的词条随按钮一起撤下（2026-09-22 用户要求）：
        // 保存之后要不要连，由连接入口留下的意图决定，不再有两个几乎同义的按钮。
        "删除" => "Delete",
        "添加" => "Add",
        "编辑…" => "Edit…",
        "连接" => "Connect",
        "导入导出" => "Import / Export",
        "复制" => "Copy",
        "重命名…" => "Rename…",
        "刷新" => "Refresh",
        "关闭" => "Close",
        "快速连接" => "Quick Connect",
        "执行" => "Run",
        "忽略" => "Dismiss",
        "白名单" => "Whitelist",
        "修改" => "Change",
        "重置" => "Reset",
        "新建连接" => "New Connection",
        "编辑连接" => "Edit Connection",
        "端口需为 1–65535 的数字" => "Port must be a number 1–65535",
        "名称、主机、用户名为必填项" => "Name, host and username are required",
        // ---- 连接对话框：连接测试与私钥（2026-09-22）----
        "连接测试" => "Test connection",
        "测试中…" => "Testing…",
        "主机与用户名为必填项" => "Host and username are required",
        "请先填写密码或私钥" => "Enter a password or a private key first",
        "测试线程异常结束" => "The connection test ended unexpectedly",
        // 认证方式（2026-10-21：改为单选框，密码与私钥不再同时占位）
        "认证方式" => "Authentication",
        "密码认证" => "Password",
        "私钥认证" => "Private key",
        "私钥文件" => "Private key file",
        "请选择私钥文件" => "Select a private key file first",
        "选择私钥文件" => "Select a private key file",
        "选择…" => "Choose…",
        "清除" => "Clear",
        "字体名称不合法（过长或含控制字符）" => {
            "Invalid font name (too long or contains control chars)"
        }
        "全部连接" => "Connect All",
        "打开 SFTP" => "Open SFTP",
        "复制连接" => "Duplicate Connection",
        "编辑配置文件" => "Edit Config File",
        "打开设置" => "Open Settings",

        // ActivityBar / 面板
        "概览" => "Dashboard",
        "终端" => "Terminals",
        "文件" => "Files",
        "AI" => "AI",
        "设置" => "Settings",
        "深色主题" => "Dark Theme",
        "浅色主题" => "Light Theme",
        "深色" => "Dark",
        "浅色" => "Light",
        "服务器" => "Servers",
        "传输队列" => "Transfers",

        // 设置对话框
        "字号" => "Font Size",
        "快捷键" => "Keyboard Shortcuts",

        // 设置面板（Settings 组件重构）
        // 页面名刻意与既有「终端」(Terminals 面板) / 「连接」(Connect 按钮) 区分开
        // （「外观」页已撤下，`外观` / `主题模式` 两条词条随之删除）
        // 终端交互与设置增强相关文案
        "字体" => "Font",
        "编码" => "Encoding",
        "配色方案" => "Scheme",
        "背景" => "Background",
        "前景" => "Foreground",
        "光标" => "Cursor",
        "标准色 0–7" => "Normal 0–7",
        "高亮色 8–15" => "Bright 8–15",
        "恢复默认" => "Restore Defaults",
        "选择「自定义」配色方案后可编辑 16 色。" => {
            "Pick the “Custom” scheme to edit the 16 colors."
        }
        "等宽字体候选；选择后即应用到已打开的终端。" => {
            "Monospace candidates; applies to open terminals immediately."
        }
        "中文乱码时改选 GBK / GB18030；需新建终端生效。" => {
            "Switch to GBK / GB18030 if Chinese text is garbled; needs a new terminal."
        }
        "跟随主题随深浅色变化；预设与自定义为固定配色。" => {
            "“Follow theme” tracks dark/light; presets and custom are fixed."
        }
        "连接超时（秒）" => "Connect Timeout (s)",
        "断线自动重连" => "Auto Reconnect",
        "最大重试次数" => "Max Retries",
        // 重连间隔（F-4）：数字是「首次重试前等多久」，之后按 2 的幂退避到 60 秒封顶
        // ——描述里必须写明「之后每次翻倍」，否则用户看到「重连间隔 5 秒」会以为
        // 每次都是 5 秒，然后在第 40 秒时以为程序卡住了。
        "重连间隔" => "Retry Interval",
        "首次重试前等待的秒数；之后每次翻倍，最多 60 秒。" => {
            "Seconds to wait before the first retry; doubles each time, up to 60 seconds."
        }
        // 2026-09-29：连接对话框的「高级」页隐藏（用户要求「保持简单」），连接级
        // 三态的两个标签随之删掉 ——「总是重连」「从不重连」。连接级三态本身
        // 照旧存盘（`p.auto_reconnect` 回填 / 写回），只是不再有界面去改它。
        // ---- 文件传输（F-13）----
        "下载后校验（SHA-256）" => "Verify Downloads (SHA-256)",
        "比对大小与摘要；大文件要多花一遍读取时间。" => {
            "Compare size and digest; large files cost an extra read pass."
        }
        "SFTP 并发传输数" => "Concurrent SFTP Transfers",
        "1–4；对新建立的连接生效，已在途的传输不受影响。" => {
            "1–4; applies to new connections, in-flight transfers are unaffected."
        }
        "没读到拖入的文件" => "The drop contained no files",
        // 外部链接（`sftp://`）：只有文件面板，没有终端
        "SFTP 已连接" => "SFTP Connected",
        "文件面板已打开，可在此浏览与传输文件" => {
            "The file panel is open — browse and transfer files there"
        }
        // ZMODEM 窗格浮层与拖放上传（协议在引擎字节泵里，文案归这里）
        "下载" => "Download",
        "上传" => "Upload",
        // 独立 SFTP 标签页（WinSCP 式双窗格，2026-10-26）
        "上传到远端" => "Upload to remote",
        "下载到本地" => "Download to local",
        "大小" => "Size",
        "修改时间" => "Modified",
        "权限" => "Permissions",
        "所有者" => "Owner",
        "新建文件夹" => "New folder",
        "新建文件" => "New file",
        "文件名称" => "File name",
        "正在读取…" => "Loading…",
        "没有可上传的文件" => "No files to upload",
        "传输命令没发出去" => "The transfer command could not be sent",
        "暂不支持上传整个文件夹，请展开后逐个上传" => {
            "Whole-folder upload is not supported yet — open it and upload files individually"
        }
        "传输" => "Transfer",
        "已有传输在进行" => "A transfer is already running",
        "传输已取消" => "Transfer cancelled",
        // 取消在途传输（2026-10-27 用户要求「支持取消任务」）
        "取消任务" => "Cancel transfer",
        "取消中…" => "Cancelling…",
        "取消请求没发出去，传输可能已经结束" => {
            "The cancel request could not be sent; the transfer may have finished already"
        }
        "会话已断开，无法发送" => "Session closed, cannot send",
        "API 端点" => "API Endpoint",
        "模型名称" => "Model",
        "API Key" => "API Key",
        "采样温度" => "Temperature",
        "8–32，支持 0.5 步进。" => "8–32, 0.5 step.",
        "握手、认证、建链各阶段上限" => {
            "Upper bound for handshake, auth and channel setup"
        }
        "会话意外断开（非用户主动关闭）后自动重连。" => {
            "Reconnect automatically after an unexpected disconnect (not a user close)."
        }
        "达到该次数后不再重试，等待手动连接。" => {
            "Stop retrying after this many attempts; wait for a manual connect."
        }
        "OpenAI 兼容端点；请求失败直接报错，不自动降级。" => {
            "OpenAI-compatible endpoint; failures surface directly, no fallback."
        }
        "如 https://api.deepseek.com/v1（不含 /chat/completions）" => {
            "e.g. https://api.deepseek.com/v1 (without /chat/completions)"
        }
        "如 deepseek-flash" => "e.g. deepseek-flash",
        "加密保存、不回显；留空沿用已保存的 Key。" => {
            "Stored encrypted, never echoed; leave empty to keep the saved key."
        }
        "0–2；越低越稳定，建议 0.3" => "0–2; lower is more stable, ~0.3 recommended",
        "终端字体不能为空" => "Terminal font must not be empty",
        "点「修改」后按下新组合键；Esc 取消录制。" => {
            "Click Change then press a new combo; Esc cancels recording."
        }

        // 连接对话框
        "名称" => "Name",
        "主机" => "Host",
        "端口" => "Port",
        "用户名" => "Username",
        "密码" => "Password",
        "分组" => "Group",
        "备注" => "Notes",

        // 会话类型（原状态栏段；状态栏已于 2026-09-17 移除，「本地」仍被终端面板使用）
        "本地" => "Local",

        "连接中" => "Connecting",
        "断开" => "Disconnect",

        // 会话启动
        "正在启动本地 Shell…" => "Starting local shell…",
        "本地 Shell 启动失败" => "Failed to start local shell",

        // 命令面板
        "命令" => "Commands",
        "搜索命令…（Ctrl + Shift + P）" => "Search commands… (ctrl + shift + P)",
        "问 AI" => "Ask AI",

        // 提示
        "新建本地终端" => "New Local Terminal",
        "关闭当前标签" => "Close Current Tab",
        "下一个标签" => "Next Tab",
        "上一个标签" => "Previous Tab",
        "终端全屏" => "Terminal Fullscreen",
        "退出终端全屏" => "Exit Terminal Fullscreen",
        "命令面板" => "Command Palette",
        "终端内搜索" => "Search in Terminal",

        // 指标 / 走势卡（状态栏 / 概览共用）
        "CPU 使用率" => "CPU Usage",
        "交换分区" => "Swap",
        "暂无采样数据" => "No samples yet",
        "网络" => "Network",
        "内存" => "Memory",
        "上行" => "Up",
        "下行" => "Down",
        // 2026-09-21「概览增加 IO 波动图」：磁盘读写速率（远端 /proc/diskstats，
        // 本机 sysinfo）。2026-09-29 起两条线的标签是英文单字母 `R` / `W`
        // （磁盘 IO 的通用记号，中英界面同一个样子），直接写在
        // `ui/src/metrics_card.rs` 的 `disk_lines` 里，不再进词条表。
        "磁盘" => "Disk",

        // 概览 Dashboard
        "本地 Shell" => "Local Shell",
        // 概览：概况卡（服务器名 + 主机名 / IP / 运行时长）与存储卡（df 信息）
        //
        // 2026-09-18 用户要求「概览第一行应该是服务器名称？反正不是状态」：
        // 首行改用**当前会话名**（数据直接取，不进词条表），
        // 「运行状态」这个词条随之无人引用，按 G2「不得留死词条」删除。
        "主机名" => "Hostname",
        "系统 IP" => "System IP",
        "已运行" => "Uptime",
        "存储" => "Storage",
        "资源走势" => "Trends",
        "暂无磁盘信息" => "No disk info",
        "可用" => "Avail",

        // 文件面板 SFTP
        "需要 SSH 会话" => "An SSH session is required",
        "上级目录" => "Parent Directory",
        "新建目录" => "New Folder",
        "上传文件到当前目录" => "Upload File to Current Directory",
        "加载中…" => "Loading…",
        "空目录" => "Empty folder",
        "完成" => "Done",
        "失败" => "Failed",
        "下载…" => "Download…",
        "下载文件夹…" => "Download Folder…",
        "复制文件名" => "Copy Name",
        "复制文件路径" => "Copy Path",
        "打开" => "Open",
        "选择要上传的文件" => "Choose files to upload",

        // 会话面板
        "暂无活动会话" => "No active sessions",

        // 快捷命令栏
        "快捷命令" => "Quick Commands",
        "新建命令" => "New Command",
        "展开" => "Expand",
        "收起" => "Collapse",
        "搜索命令…" => "Search commands…",
        "暂无快捷命令" => "No quick commands yet",
        "暂无命令" => "No commands yet",
        "重命名" => "Rename",
        "复制命令" => "Duplicate",
        "复制脚本" => "Copy Script",
        "复制分组" => "Duplicate Group",
        "上移" => "Move Up",
        "下移" => "Move Down",
        "移动到" => "Move To",
        "删除分组" => "Delete Group",
        "尚未新建分组" => "No groups yet",
        // 投递目标
        "无 PTY" => "no PTY",
        // 执行反馈
        "当前没有已连接的会话" => "No connected session right now",
        "命令没有发出去：先新建本地终端，或连接一台服务器。" => {
            "The command was not sent: open a local terminal or connect to a server first."
        }
        // 「知道了」的词条随「没有可用会话」提示的两枚按钮一并撤下
        // （2026-09-18 运行反馈：该提示改成不需要确认的弹出消息，没有按钮可点）。
        // g2 护栏会指名报出这类孤儿词条 —— 删 UI 记得回来删词条。
        "命令已不存在，列表已刷新" => {
            "Command no longer exists; the list was refreshed"
        }
        "未执行" => "not run",
        "目标没有可投递的窗口" => "None of the targets can receive commands",
        // 快捷命令栏（命令输入框 / 分组 Tab 页）
        "输入命令，回车发送…" => "Type a command, press Enter to send…",
        "发送到全部终端" => "Send to all terminals",
        // 快捷命令：末尾回车配置
        "末尾添加回车（关闭则只键入不执行）" => {
            "Append Enter at the end (off = type without running)"
        }

        // AI 面板
        "AI 助手" => "AI Assistant",
        // 聊天记录侧栏 + 对话面板头部（2026-09-18 运行反馈「点击 AI 助手，聊天界面
        // 显示在终端右侧，原 AI 助手侧边栏改为聊天记录，支持删除」）：
        // 对话正文搬到终端右侧，左侧栏那一列改为会话列表。
        // 「清空」随旧按钮一并撤下 —— 它当时唯一的作用是清掉当前会话，
        // 现在的等价动作是「新建对话」（另开一条）或列表里的删除。
        "聊天记录" => "Chat History",
        "新对话" => "New Chat",
        "新建对话" => "New Chat",
        "删除对话" => "Delete Chat",
        "关闭对话" => "Close Chat",
        "跳到最新" => "Jump to latest",
        "重试" => "Retry",
        "未配置 AI 服务" => "AI service not configured",
        "填写端点与 API Key 后即可提问" => {
            "Fill in the endpoint and API key to start asking"
        }
        "向 AI 描述任务或提问，会自动附上当前终端最近输出。" => {
            "Describe a task or ask a question; recent terminal output is attached automatically."
        }
        "重新生成" => "Regenerate",
        "删除消息" => "Delete Message",
        "复制消息" => "Copy Message",
        "思考中…" => "Thinking…",
        "思考模式" => "Thinking Mode",
        "思考过程" => "Reasoning",
        "正在生成…" => "Generating…",
        "已停止" => "Stopped",
        "停止生成" => "Stop generating",
        "低风险" => "Low risk",
        "需确认" => "Needs confirmation",
        "已拦截" => "Blocked",
        "禁止执行，请手动处理" => "Blocked — handle manually",
        "发送" => "Send",

        // 终端右键菜单
        "粘贴" => "Paste",

        // 提示条 / 状态反馈
        "已加入免确认白名单" => "Added to the no-confirm list",
        "端点与模型名不能为空" => "Endpoint and model are required",
        "API Key 加密失败（系统凭据存储不可用）" => {
            "Failed to encrypt the API key (system credential store unavailable)"
        }
        "请先配置 AI 端点与 API Key" => "Configure the AI endpoint and API key first",
        "该命令命中拦截黑名单，已拒绝执行" => {
            "Command is blacklisted; execution refused"
        }
        "已保存" => "Saved",

        // 关于页已于 2026-09-18 整体撤下（用户要求移除「配置-关于」与诊断导出/复制），
        // 该页的 9 条词条（关于 / 版本 / 运行环境 / 配置目录 / 日志文件 /
        // 导出诊断信息 / 复制诊断信息 / 已复制诊断信息到剪贴板 / 那句安全性说明）
        // 一并删除 —— g2 护栏会盯着「表里有、代码里没人引用」，留着就是红。

        // 导入 / 导出连接（设置页「连接管理」）
        "搜索…" => "Search…",
        "未找到匹配的连接" => "No matching connections",
        "导出文件写入失败" => "Failed to write the export file",
        "读取导入文件失败" => "Failed to read the import file",
        "该文件里没有新的连接" => "No new connection in that file",

        // 背景图的**设置页控件**已随「外观」页撤下（2026-10-20），
        // 既有配置值仍由渲染路径消费；相关文案条目一并删除。

        // 标签分屏
        "向右拆分" => "Split Right",
        "向下拆分" => "Split Down",
        "取消分屏" => "Unsplit",
        "没有可拆分的会话" => "No session to split",
        "该会话未保存为连接配置，无法拆分" => {
            "This session is not saved as a connection; cannot split"
        }
        "拆分已取消（没有可用的新会话）" => {
            "Split cancelled (no new session available)"
        }
        "本地 Shell 正在启动，请稍后再拆分" => {
            "A local shell is already starting; try splitting again shortly"
        }
        // 「主窗格为空」的词条随那行标题一起撤下（2026-09-22 用户要求）：
        // 空窗格里只剩快捷键表，表头那句话说的是用户自己就能看见的事。

        // SFTP 删除确认
        "删除目录" => "Delete Folder",
        "删除文件" => "Delete File",
        "不可恢复" => "Irreversible",
        "确认删除以下远端路径？该操作不可恢复：" => {
            "Delete the following remote path? This cannot be undone:"
        }
        // 2026-09-22 用户要求「支持递归删除文件目录」：确认框原来写的是
        // 「仅能删除空目录」，而目录删除现在是递归的 —— 这句必须跟着改，
        // 否则确认框会对用户**隐瞒**即将发生的批量删除。
        "这是目录：其中的所有文件与子目录都会被一并删除。" => {
            "This is a folder: all of its files and subfolders will be deleted with it."
        }
        "确认删除" => "Confirm Delete",

        // SFTP「新建目录 / 重命名」对话框（2026-09-22 用户要求：两个入口都要
        // 弹框收名字，此前分别是「自动时间戳命名」与「加 -renamed 后缀」）
        "目录名称" => "Folder Name",
        "新名称" => "New Name",
        "名称不能为空" => "Name cannot be empty",
        "名称不能包含斜杠" => "Name cannot contain a slash",
        "名称不能是 . 或 .." => "Name cannot be `.` or `..`",

        // 首次信任（known_hosts）
        "首次连接：验证主机指纹" => "First Connection: Verify Host Fingerprint",
        "信任并连接" => "Trust & Connect",
        "信任后该密钥将加入白名单；若指纹与服务器管理员公布的不一致，请勿信任。" => {
            "Once trusted, this key is whitelisted. If the fingerprint differs from the one \
             published by your server administrator, do not trust it."
        }
        // 主机密钥**变更**（容器重建 / 重装系统 / 疑似中间人）
        "主机密钥已变更：请先核对指纹" => "Host Key Changed: Verify the Fingerprint",
        "取消连接" => "Disconnect",
        "更新密钥并连接" => "Update Key & Connect",
        "确认后将以新密钥替换旧记录并继续连接。若无法确认这次变化来自你自己（重建容器 / 重装系统 / 换机器），请取消 —— 这可能是中间人攻击。" => {
            "Confirming replaces the old record with the new key and continues. If you cannot \
             explain the change (rebuilt container, reinstalled system, new machine), cancel — \
             this may be a man-in-the-middle attack."
        }

        // 表单占位符 / 标签 / 诊断
        "API Key（加密保存，不回显）" => "API Key (encrypted, never shown)",
        "命令名称，如 build" => "Command name, e.g. build",
        "API 端点，如 https://api.deepseek.com/v1" => {
            "API endpoint, e.g. https://api.deepseek.com/v1"
        }
        "按下新组合键…" => "Press the new shortcut…",
        "搜索终端…" => "Search terminal…",
        "新分组" => "New Group",
        "模型名，如 deepseek-flash" => "Model name, e.g. deepseek-flash",
        "问 AI…（自动附终端上下文）" => "Ask AI… (terminal context attached)",

        // 补齐：原先调 t() 但表里无键（英文模式下漏译）
        "复制标签" => "Duplicate Tab",
        "关闭其他" => "Close Others",
        "全部关闭" => "Close All",
        "连接失败" => "Connection Failed",
        "正在建立 SSH 连接…" => "Connecting over SSH…",
        "已断开" => "Disconnected",
        // 掉线现场（「不跳页面」那一版）：终端窗格内叠的一条断连条 + 两个动作。
        // 与上面的「已断开」是**两种不同状态** —— 那个是用户主动断开（终端已卸下、
        // 只剩占位卡），这里是意外掉线（终端画面仍在，只把输入封死）。
        "连接已断开" => "Connection Lost",
        "连接已被远端关闭" => "The remote side closed the connection",
        "重新连接" => "Reconnect",
        "关闭标签" => "Close Tab",
        "正在重连…" => "Reconnecting…",
        // 重连成功后写进终端画面的分隔行（见 `TerminalView::reattach`）。
        // 它是「旧会话的输出」与「新会话的输出」之间唯一的界线，所以措辞要能独立读懂。
        "已重新连接" => "Reconnected",
        "重连失败：" => "Reconnect failed: ",
        "会话已终止，右键标签可重新连接" => {
            "Session ended; right-click the tab to reconnect"
        }
        "该会话未保存为连接配置，无法复制" => {
            "This session is not saved as a connection; cannot duplicate"
        }
        "该会话的连接配置已被删除，无法重连" => {
            "The saved connection of this session was deleted; cannot reconnect"
        }
        "这是一个目录，请选择 json 文件" => "That is a folder; pick a JSON file",
        "导出连接" => "Export Connections",
        "导入连接" => "Import Connections",
        "导出或导入连接配置；文件不含任何口令。" => {
            "Export or import connections; files never contain credentials."
        }
        // 行高（2026-09-28）：面板上仅存的一项终端外观细调。
        // 2026-10-19 起光标样式 / 光标闪烁 / 回滚行数 / 多行粘贴行为 / 采样间隔
        // 的控件已撤下（值仍在 `settings.json` 里继续生效），对应词条一并删除。
        "行高" => "Line height",
        "1.0–2.0；觉得行距挤就调大。" => "1.0-2.0; increase it if lines feel cramped.",
        // 粘贴保护（F-5）：确认框 + 设置面板两行
        "确认粘贴" => "Confirm Paste",
        "含换行的内容会被逐行发送，命令会立即执行。" => {
            "Multi-line content is sent line by line; commands run immediately."
        }
        "字体、配色改动立即生效；编码改动对新建终端生效。" => {
            "Font and color changes apply instantly; encoding applies to new terminals."
        }
        "启动时打开" => "Open on startup",
        "打开软件后直接进入什么。默认开一个本地终端；不需要终端时可选命令面板或不打开。" => {
            "What opens when the app starts. The default starts a local shell; \
             choose the command palette or nothing if you do not need a terminal."
        }
        "未分组" => "Ungrouped",
        // 连接分组：新建与改名共用同一个「分组名」对话框，入口都在连接树的右键菜单里
        //（连接对话框里的自由文本分组名已取消）。
        "新建分组" => "New Group",
        "新增分组" => "New Subgroup",
        "重命名分组" => "Rename Group",
        "分组名称" => "Group name",
        "分组名不能为空" => "Group name cannot be empty",
        "分组名已存在" => "That group name already exists",
        "该分组已不存在" => "That group no longer exists",
        // 嵌套分组：拖拽换父时被 core 拒绝（拖进自己或自己的后代）给可见提示
        "不能把分组移到它自己或它的子分组里" => {
            "A group cannot be moved into itself or one of its subgroups"
        }
        // 连接重命名：右键菜单「重命名…」与 F2 共用同一个对话框（同一个正文视图），
        // 字段名与校验文案与分组侧成对。
        "重命名连接" => "Rename Connection",
        "连接名称" => "Connection name",
        "连接名称不能为空" => "Connection name cannot be empty",
        "同名连接已存在" => "A connection with that name already exists",
        "该连接已不存在" => "That connection no longer exists",
        "请先在连接树中选中一个连接" => "Select a connection in the tree first",
        "选中的连接已不存在" => "The selected connection no longer exists",
        "命令内容，如 make -j4（支持多行）" => {
            "Command, e.g. make -j4 (multi-line supported)"
        }
        "加入免确认白名单" => "Add to the no-confirm list",
        "粘贴并执行" => "Paste and Run",
        "前往连接" => "Go to Connections",
        // ---- 定时任务（tasks 模块）----
        // 只列已被代码引用的词条：`g2_no_dead_entries` 会拦下「表里有、代码里没有」
        // 的死条目，因此这里不能提前铺一整份计划中的文案。
        "按计划" => "On schedule",
        "表达式无效" => "Invalid expression",
        "批量连接" => "Connect All",
        "批量执行命令" => "Run Command on All",
        "批量下载" => "Download from All",
        "AI 巡检" => "AI Inspection",
        "负载" => "Load",
        "日志" => "Log",
        "连通性" => "Reachability",
        "从未运行" => "Never run",
        "正常" => "OK",
        "部分失败" => "Partial failure",
        "已跳过" => "Skipped",
        "已取消" => "Cancelled",
        "提示" => "Info",
        "警告" => "Warning",
        "严重" => "Critical",
        "未选择目标" => "No target selected",
        "请选择目标" => "Select a target",
        "请填写任务名称" => "Enter a task name",
        // 2026-09-22 用户要求「定时任务面板，AI 要求必输」：
        // 拦截在 core 的 `TaskAction::validate()`，所以这句和上面几条同处一节。
        "请填写 AI 要求" => "AI requirement required",
        "命令不能为空" => "Command cannot be empty",
        "路径不能为空" => "Path cannot be empty",
        "路径不能包含 .. （禁止目录穿越）" => {
            "Path must not contain .. (traversal is blocked)"
        }
        "该命令属于不可逆破坏操作，已被安全策略永久拦截，无法用于定时任务" => {
            "This command is irreversibly destructive and permanently blocked; it cannot be scheduled"
        }
        // ---- 定时任务面板（UI 侧引用）----
        "定时" => "Schedule",
        "定时任务" => "Scheduled Tasks",
        // 「AI 要求」旁的必填标记（2026-09-22）：拦截在 core，这枚标记负责提前说明
        "必输" => "Required",
        "运行记录" => "Run History",
        "还没有运行记录" => "No runs yet",
        "还没有定时任务" => "No scheduled tasks yet",
        "搜索任务…" => "Search tasks…",
        "没有匹配的任务" => "No matching tasks",
        "新建 AI 巡检" => "New AI Inspection",
        "新建批量连接" => "New Bulk Connect",
        "新建批量命令" => "New Bulk Command",
        "新建批量下载" => "New Bulk Download",
        "立即执行" => "Run Now",
        "停止" => "Stop",
        "启用" => "Enable",
        "停用" => "Disable",
        "正在运行" => "Running",
        "会话" => "Session",
        // 模型能力三态（v1.1 §3.8.1）。**只**给 core 侧 `CapabilityState::label`
        // 这类非设置面板的展示用：2026-09-21 用户要求撤下设置-AI 助手的「模型能力」条目。
        "未知" => "Unknown",
        "支持" => "Supported",
        "不支持" => "Unsupported",
        "当前模型不支持工具调用，已降级为问答模式（可在设置里关闭 Agent）" => {
            "The current model does not support tool calling; degraded to chat mode (you can turn Agent off in settings)"
        }
        // ---- 定时任务编辑对话框（§6.3）----
        "任务" => "Task",
        "这条任务已在别处打开，请重新打开编辑" => {
            "This task is already open elsewhere; reopen it to edit"
        }
        "时间" => "Time",
        "目标" => "Target",
        "通知" => "Notify",
        "异常时" => "On anomaly",
        "失败时" => "On failure",
        "每次运行后" => "After every run",
        "发一次 AI 测试" => "Send one AI test",
        "这条任务不是 AI 巡检；保存只更新时间、目标与通知，原动作保持不变。" => {
            "This task is not an AI inspection; saving only updates time, targets and notifications, the original action is kept."
        }
        // ---- 设置：Agent 页（2026-09-22 起从「AI 助手」页独立出来）----
        "工具调用的开关、命令权限与免确认白名单。" => {
            "Switches, command permission and the no-confirm list for tool calls."
        }
        "启用 Agent 工具调用" => "Enable agent tool calling",
        "允许状态变更命令" => "Allow state-changing commands",
        "AI 调试日志" => "AI debug log",
        "免确认白名单" => "No-confirm list",
        "配置" => "Configure",
        "命中的状态变更命令免确认直接执行；关闭某条即该命令失效；支持 * 通配符；批量执行不适用。" => {
            "Matching state-changing commands run without confirmation; turning an entry off disables it; * wildcards supported; never applied to batch runs."
        }
        "自定义白名单" => "Custom list",
        "内置白名单" => "Built-in list",
        "还没有自定义免确认命令：内置只读命令本来就无需确认，见「内置白名单」页。" => {
            "No custom no-confirm commands yet: the read-only built-ins never need confirmation, see the Built-in list tab."
        }
        "只读命令由安全策略直接执行，本来就无需确认；这里仅列出，不占用白名单。" => {
            "Read-only commands run directly under the safety policy and never need confirmation; they are listed here for reference only and are not part of the list."
        }
        "输入完整命令，如 systemctl restart nginx" => {
            "Full command, e.g. systemctl restart nginx"
        }
        "搜索命令" => "Search commands",
        "没有匹配的命令" => "No matching commands",
        "关闭后退化为纯问答：请求体不再附带 tools，AI 只能用已有上下文回答。" => {
            "When off, the agent degrades to plain chat: no tools are sent to the model."
        }
        "开启后 AI 仍会先弹确认卡片；无人值守的定时巡检一律不执行。" => {
            "Even when on, the AI asks for confirmation first; unattended scheduled inspections never execute."
        }
        "把 AI 的请求、流式原文和工具调用明细写进日志（logs/日期/rustshell.log），排查问题时开启。" => {
            "Write AI requests, streamed output and tool-call details to the log (logs/<date>/rustshell.log); turn on when troubleshooting."
        }
        // ---- 设置：通知（SMTP）----
        "异常预警的邮件投递（SMTP）。发送功能后续实现，配置先备好。" => {
            "Email delivery for anomaly alerts (SMTP). Sending comes later; the settings are ready now."
        }
        "启用邮件预警" => "Enable email alerts",
        "关闭时预警只写本地日志，不发邮件。" => {
            "When off, alerts only go to the local log; no mail is sent."
        }
        "SMTP 服务器" => "SMTP host",
        "如 smtp.example.com" => "e.g. smtp.example.com",
        "SSL 465 / STARTTLS 587 / 明文 25" => "SSL 465 / STARTTLS 587 / plaintext 25",
        "加密方式" => "Encryption",
        "选「明文」只在可信内网使用。" => "Use plaintext only on a trusted intranet.",
        "明文（不加密）" => "Plaintext (no encryption)",
        "STARTTLS" => "STARTTLS",
        "SSL / TLS" => "SSL / TLS",
        "多数服务要求用户名是完整邮箱地址。" => {
            "Most providers require the full email address as the username."
        }
        "口令" => "Password",
        "加密保存、不回显；留空沿用已保存的口令。" => {
            "Stored encrypted and never displayed; leave blank to keep the saved password."
        }
        "SMTP 口令（加密保存，不回显）" => {
            "SMTP password (stored encrypted, never displayed)"
        }
        "SMTP 口令加密失败（系统凭据存储不可用）" => {
            "SMTP password encryption failed (credential store unavailable)"
        }
        "发件人地址" => "Sender address",
        "SMTP 信封上的 From，通常与用户名一致。" => {
            "The SMTP envelope From, usually the same as the username."
        }
        "发件人显示名" => "Sender display name",
        "如「RustShell 预警」，可为空。" => "e.g. \"RustShell Alerts\"; may be blank.",
        "收件人" => "Recipients",
        "逗号或分号分隔，最多 50 个。" => "Comma or semicolon separated, up to 50.",
        "抄送" => "CC",
        "可为空。" => "May be blank.",
        "主题前缀" => "Subject prefix",
        "留空则用 [RustShell]。" => "Defaults to [RustShell] when blank.",
        "发送测试邮件" => "Send test email",
        // 2026-10-12：投递实现后，旧文案「配置已完整；发送功能尚未实现」成为孤儿，
        // 按 g2 门禁要求随 UI 改动一并删除（留着的后果是英文界面仍显示「即将推出」）。
        "正在发送…" => "Sending…",
        "将按上面的配置发一封测试邮件到收件人。" => {
            "Sends a test email to the recipients using the settings above."
        }
        "测试邮件" => "Test email",
        "这是一封来自 RustShell 的测试邮件。看到它说明 SMTP 配置可用。" => {
            "This is a test email from RustShell. Receiving it means the SMTP settings work."
        }
        "正在发送测试邮件…" => "Sending a test email…",
        "（已加密）" => " (encrypted)",
        "（明文）" => " (unencrypted)",
        "配置尚未完整。" => "Configuration is incomplete.",
        // ---- AI 面板：agent 轨迹块与确认卡片（§3.9）----
        // 「明细」条目已于 2026-09-22 删除：它与行首展开箭头重复，用户要求撤下
        // （删 UI 必须回来删词条，否则 g2 报 orphan）。
        "需要确认" => "Confirmation required",
        "未产生结论" => "The model produced no conclusion",
        "本轮模型请求了工具调用，但请求未经过 agent 通道，已忽略。请重新发送" => {
            "The model requested tool calls, but this request did not go through the agent channel; ignored. Please send again"
        }
        // ---- 定时任务编辑器（2026-09-20 运行反馈改版）----
        "五段式：分 时 日 月 周，如 0 9 * * *（每天 09:00，跟随本机时区）" => {
            "Five fields: minute hour day month weekday, e.g. 0 9 * * * (daily at 09:00, local time)"
        }
        "AI 要求" => "AI instructions",
        "任务名称（留空则取 AI 要求首行）" => {
            "Task name (blank = first line of the AI instructions)"
        }
        "用一句话说明要巡检什么" => "Describe what to inspect in one line",
        "还没有可选项，请先在连接面板里建好分组或连接" => {
            "Nothing to pick yet; create groups or connections in the connections panel first"
        }
        "没有选到任何连接，请在上面的列表里勾选分组或连接" => {
            "No connection selected; tick a group or connection in the list above"
        }
        "这条任务原来是「固定间隔」，已换算成等价的 cron 表达式" => {
            "This task used a fixed interval; converted to the equivalent cron expression"
        }
        // ---- OpenSSH 配置导入（F-1）----
        "从 OpenSSH 配置导入" => "Import from OpenSSH config",
        "全选" => "Select all",
        "全不选" => "Select none",
        "没有解析到任何主机" => "No hosts were found",
        "新增" => "New",
        "重复，导入时跳过" => "Duplicate, skipped",
        "缺少主机" => "No host",
        "缺少用户名" => "No username",
        "解析提示" => "Parse notes",
        "未找到 OpenSSH 配置" => "OpenSSH config not found",
        "选择文件…" => "Choose file…",
        // ---- 端口转发（F-3，2026-09-23）----
        //
        // 2026-09-29：连接对话框的「高级」页隐藏（用户要求「保持简单」），
        // 只在那个页里用过的词条随之删掉（「端口转发」「本地转发」「远端转发」
        // 「动态转发（SOCKS5）」「连接后生效」「暂无规则」「请填写目标地址」
        // 「监听端口需为 1–65535 的数字」「目标端口需为 1–65535 的数字」
        // 「监听地址:端口 → …」「目标主机，如 db.internal」）。转发功能本身
        // 照旧（规则随连接保存、连接建立后自动启动），只是对话框里不再能新增 / 删除。
        //
        // 2026-09-30：连接面板底部的隧道列表按用户要求整块撤下，随之删掉**只在那块
        // 列表里用过**的三条（「隧道」标题、「暂无隧道」空态、「全部停止」按钮）。
        // 下面这三条校验文案不属此列 —— 它们的引用点在 core 的 `forward.rs`
        // （`ForwardValidateError` 的文案），只要转发引擎还在就不能删。
        "监听地址不能为空" => "Listen address cannot be empty",
        "监听端口不能为 0" => "Listen port cannot be 0",
        "缺少目标地址或端口" => "Target address or port is missing",
        // ---- 会话日志（F-7，2026-09-23）----
        //
        // 2026-09-29 同上：「会话日志」标题与三态标签（「跟随全局」「记录」
        // 「不记录」）随「高级」页删除；全局开关这几条仍在用。
        "记录会话日志" => "Record session log",
        "打开日志目录" => "Open log folder",
        "日志目录创建失败" => "Failed to create the log folder",
        "把终端输出原样落盘；对新建会话生效，每条连接可单独覆盖。" => {
            "Writes raw terminal output to disk; applies to new sessions, and each connection can override it."
        }
        // ---- 窗口 ----
        // 2026-10-19：系统托盘整体移除（用户要求），关闭窗口一律退出。
        // 托盘相关词条（显示主窗口 / 退出 / 最小化到托盘 / 托盘状态……）随之删除。
        "窗口" => "Window",
        "启动程序时的行为。" => "What happens when the app starts.",
        // ---- 会话恢复（F-9，2026-09-24）----
        // 「恢复」与「不恢复」是提示框的两个按钮。
        //
        // 2026-09-28：设置面板那行下拉已按用户要求撤下（启动行为统一由
        // 「启动时打开」一处口径决定），随之删掉了三个**只在那个下拉里用**的词条
        // ——「启动时会话恢复」「询问」「自动恢复」。留下它们是死条目，
        // `g2_no_dead_entries` 会红；而且死词条比死代码更坏：下次有人想加回
        // 一个「询问」档，会看到一个现成的译文，顺手用了却没人知道它早已没人维护。
        // 2026-09-29：提示框上的「下次不再询问」勾选框随「取消恢复上次会话功能」一并
        // 撤掉（启动不再自动弹框，也就没有一个「下次」可免）。词条同步删除 —— 留着
        // 它就是死条目，`g2_no_dead_entries` 必红；而且死词条比死代码更坏：下次有人
        // 想加回一个同义勾选框，会看到一个现成的译文，顺手用了却没人知道它早已没人维护。
        "恢复上次会话" => "Restore Last Session",
        "恢复" => "Restore",
        "不恢复" => "Don't Restore",
        "连接会恢复成「已断开」，不会自动发起；本地终端会照常重开。" => {
            "Connections come back as \"Disconnected\" and are never opened automatically; local terminals restart as usual."
        }
        // 拼在标签标题末尾的后缀：英文要自带前导空格（中文的括号本身就留得出空隙）。
        "（连接配置已删除）" => " (connection deleted)",
        // ---- 代理（F-10，2026-09-28）----
        // 握手失败的原因文案由 core 产出（`proxy::validate_error_text` 与
        // `socks5_rep_text`）：它们要能进「连接失败」提示框，走 `t()` 才能随语言切换。
        // 进度类句子（带地址/状态码的那种）在 core 里走 `ts`，两边都写在原地，不进本表。
        "请填写代理地址" => "Enter the proxy address",
        "代理端口需为 1–65535 的数字" => "Proxy port must be a number 1–65535",
        "代理地址过长（超过 255 字节）" => "Proxy address is too long (over 255 bytes)",
        // SOCKS5 的 REP 码（RFC1928 §6）：短句直译，用户要能一眼对上代理日志。
        "一般性失败" => "General failure",
        "代理不允许该连接" => "The proxy does not allow this connection",
        "网络不可达" => "Network unreachable",
        "主机不可达" => "Host unreachable",
        "连接被拒" => "Connection refused",
        "TTL 超时" => "TTL expired",
        "代理不支持 CONNECT 命令" => "The proxy does not support the CONNECT command",
        "代理不支持该地址类型" => "The proxy does not support this address type",
        "未知错误" => "Unknown error",
        // 连接对话框（与「会话日志」同一版式：一行标签 + 三态循环按钮）
        //
        // 2026-09-29：那两行随「高级」页隐藏而撤（用户要求「保持简单」），
        // 只在它们里用过的词条随之删掉 ——「直连」与三条「生效：…」投影、
        // 以及口令输入框的说明「仅本条连接；口令加密保存、不回显」。
        // 代理功能本身照旧（设置面板的全局代理 + 存盘里的连接级覆盖都在）。
        "代理" => "Proxy",
        "自定义" => "Custom",
        // 设置面板「连接」页的代理分区
        "使用代理" => "Use a proxy",
        "建立 SSH 连接前先与代理握手（HTTP CONNECT / SOCKS5）。" => {
            "Handshakes with the proxy before opening an SSH connection (HTTP CONNECT / SOCKS5)."
        }
        "类型" => "Type",
        "HTTP CONNECT 最通用；SOCKS5 支持域名直通。" => {
            "HTTP CONNECT is the most compatible; SOCKS5 passes domain names through."
        }
        "代理地址" => "Proxy address",
        "如 127.0.0.1 或 proxy.corp.lan" => "e.g. 127.0.0.1 or proxy.corp.lan",
        "代理端口" => "Proxy port",
        "常见：8080 明文 / 3128 Squid / 1080 SOCKS5" => {
            "Common: 8080 plain HTTP / 3128 Squid / 1080 SOCKS5"
        }
        "代理用户名" => "Proxy username",
        "留空 = 匿名代理（不带认证）" => {
            "Leave empty for an anonymous proxy (no authentication)"
        }
        "代理口令" => "Proxy password",
        // 「加密保存、不回显；留空沿用已保存的口令。」复用 SMTP 那份（同一句话）：
        // 两条口令的落库口径本来就是一个（掩码 + 留空不改），措辞无需分叉。
        // 输入框占位与保存期报错
        "代理用户名（可选）" => "Proxy username (optional)",
        "代理口令（可选）" => "Proxy password (optional)",
        "代理口令（加密保存，不回显）" => {
            "Proxy password (stored encrypted, never shown)"
        }
        "代理口令加密失败（系统凭据存储不可用）" => {
            "Failed to encrypt the proxy password (system credential storage is unavailable)"
        }
        // ---- 设置：关于（2026-10-22 恢复）----
        "关于" => "About",
        "版本" => "Version",
        "GitHub 仓库" => "GitHub Repository",
        "检查更新" => "Check for Updates",
        "正在检查…" => "Checking…",
        "打开发布页" => "Open release page",
        // 在线更新（2026-10-28）：检查 → 下载 → 安装 → 重启
        "软件更新" => "Software Update",
        "立即更新" => "Update Now",
        "正在下载…" => "Downloading…",
        "重启以完成更新" => "Restart to Finish",
        "稍后" => "Later",
        "详情" => "Details",
        "选择 OpenSSH 配置文件" => "Select an OpenSSH config file",
        "依据 GitHub 上的版本标签判断；有新版时标题栏右侧会显示提示。" => {
            "Compares against the version tags on GitHub; when a newer one exists a badge appears on the right of the title bar."
        }
        "RustShell 是一款基于 Rust 与 GPUI 构建的跨平台 SSH / SFTP 终端，支持多标签与分屏、文件传输、AI 助手和批量命令执行。" => {
            "RustShell is a cross-platform SSH / SFTP terminal built with Rust and GPUI, with tabs and split panes, file transfer, an AI assistant, and batch command execution."
        }
        // ---- 命令补全（C-1）----
        // 浮层标题与来源标签
        "命令补全" => "Command completion",
        "库" => "Library",
        "历史" => "History",
        "内置" => "Built-in",
        "最近用过" => "Recently used",
        "按 Esc 关闭" => "Press Esc to close",
        // 内置命令表分类（`文件` / `网络` 已在上面通用段，此处只补缺的）
        "文本" => "Text",
        "系统" => "System",
        "进程" => "Processes",
        "容器" => "Containers",
        // 内置命令表说明（`cmdhistory::BUILTIN` 的 `desc`，会渲染给用户看）
        "列出目录内容" => "List directory contents",
        "切换目录" => "Change directory",
        "显示当前目录" => "Print working directory",
        "复制文件或目录" => "Copy files or directories",
        "移动或重命名" => "Move or rename",
        "创建目录" => "Create a directory",
        "创建空文件" => "Create an empty file",
        "按条件查找文件" => "Find files by condition",
        "统计目录占用" => "Show directory disk usage",
        "查看磁盘空间" => "Show disk space",
        "打包或解包归档" => "Pack or unpack archives",
        "创建链接" => "Create a link",
        "查看文件内容" => "Show file contents",
        "分页查看文件" => "View a file page by page",
        "查看文件开头" => "Show the start of a file",
        "查看文件末尾" => "Show the end of a file",
        "按模式搜索文本" => "Search text by pattern",
        "流式文本替换" => "Stream-edit text",
        "按列处理文本" => "Process text by column",
        "统计行数 / 字数" => "Count lines / words",
        "排序文本行" => "Sort lines",
        "去掉相邻重复行" => "Drop adjacent duplicate lines",
        "比较两个文件" => "Compare two files",
        "分流到文件与屏幕" => "Tee output to file and screen",
        "以管理员身份执行" => "Run as administrator",
        "显示当前用户" => "Show the current user",
        "显示系统信息" => "Show system information",
        "显示或设置时间" => "Show or set the date",
        "显示运行时长与负载" => "Show uptime and load",
        "显示环境变量" => "Show environment variables",
        "查找命令所在路径" => "Locate a command",
        "修改文件权限" => "Change file permissions",
        "修改文件属主" => "Change file ownership",
        "显示命令历史" => "Show command history",
        "清屏" => "Clear the screen",
        "退出当前会话" => "Exit the session",
        "查看命令手册" => "Show the manual page",
        "测试网络连通性" => "Test connectivity",
        "发起 HTTP 请求" => "Make an HTTP request",
        "下载文件" => "Download a file",
        "查看网络连接" => "Show network connections",
        "查看端口与连接" => "Show ports and connections",
        "查看或配置网卡" => "Show or configure interfaces",
        "查看网卡信息" => "Show interface information",
        "查询 DNS 记录" => "Query DNS records",
        "登录远程主机" => "Log in to a remote host",
        "跨主机复制文件" => "Copy files across hosts",
        "增量同步文件" => "Incrementally sync files",
        "查看进程列表" => "List processes",
        "实时查看进程" => "Watch processes live",
        "交互式进程查看" => "Interactive process viewer",
        "按 PID 结束进程" => "Kill a process by PID",
        "按名称结束进程" => "Kill processes by name",
        "后台运行且不挂断" => "Run in the background, detached",
        "管理系统服务" => "Manage system services",
        "查看系统日志" => "Show system logs",
        "查看工作区状态" => "Show working tree status",
        "查看提交历史" => "Show commit history",
        "查看改动差异" => "Show changes",
        "管理分支" => "Manage branches",
        "切换分支或还原文件" => "Switch branches or restore files",
        "查看运行中的容器" => "List running containers",
        "查看容器日志" => "Show container logs",
        _ => zh,
    }
}

/// 测试期的语言互斥。语言是进程内全局状态，而 cargo 默认并行跑测试：
/// 多个用例都会改 `LANG`，不加锁就会互相踩（表现为随机失败）。
/// 放在 `pub(crate)` 模块里，供其它模块（如 `metrics::format_uptime`）的用例共用。
#[cfg(test)]
pub(crate) mod test_support {
    static LANG_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// 取语言互斥锁；持锁期间独占 `LANG`。
    pub(crate) fn lang_guard() -> std::sync::MutexGuard<'static, ()> {
        LANG_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::lang_guard;
    use super::*;

    #[test]
    fn zh_identity_and_en_mapping() {
        let _g = lang_guard();
        set_lang(Lang::Zh);
        assert_eq!(t("设置"), "设置");
        set_lang(Lang::En);
        assert_eq!(t("设置"), "Settings");
        assert_eq!(t("不存在的文案XYZ"), "不存在的文案XYZ"); // 缺映射回退
        set_lang(Lang::Zh);
    }

    #[test]
    fn set_lang_str_parses() {
        let _g = lang_guard();
        set_lang_str("en");
        assert_eq!(lang(), Lang::En);
        set_lang_str("zh-CN");
        assert_eq!(lang(), Lang::Zh);
        set_lang_str("");
        assert_eq!(lang(), Lang::Zh);
    }

    #[test]
    fn tp_picks_by_lang() {
        let _g = lang_guard();
        set_lang(Lang::En);
        assert_eq!(tp("连接: {}", "Connect: {}"), "Connect: {}");
        set_lang(Lang::Zh);
        assert_eq!(tp("连接: {}", "Connect: {}"), "连接: {}");
    }

    // ------------------------------------------------------------------
    // 文案一致性回归：词条表是「契约」，调用点必须与之一致。
    // 这几条断言把 2026-09-15 复查出来的四类问题钉住，防止回潮：
    //   G1 缺口   —— 调了 t() 但表里没键（英文模式下原样显示中文）
    //   G2 死条目 —— 表里有、代码里无人引用（改名后忘了删）
    //   G3 符号   —— 界面文案里出现 emoji / 装饰符（色彩不可控、字号不搭）
    //   G4 双翻   —— `t(t(".."))`（碰巧能跑，但语义是错的）
    // 扫描范围是工作区源码，故必须先有 `core` 的上级目录；不存在时安全跳过。
    // ------------------------------------------------------------------

    /// 工作区根（`core/` 的上一级）
    fn workspace_root() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("core 目录必须有上级")
            .to_path_buf()
    }

    fn collect_rs(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                collect_rs(&p, out);
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push(p);
            }
        }
    }

    /// 界面源码（4 个 crate 的 `src`，排除词条表自身）
    fn ui_sources() -> Vec<(String, String)> {
        let root = workspace_root();
        let mut files = Vec::new();
        for c in ["core", "ui", "shell", "platform"] {
            collect_rs(&root.join(c).join("src"), &mut files);
        }
        files
            .into_iter()
            .filter(|p| p.file_name().is_some_and(|n| n != "i18n.rs"))
            .filter_map(|p| {
                let rel = p
                    .strip_prefix(&root)
                    .ok()?
                    .to_string_lossy()
                    .replace('\\', "/");
                let text = std::fs::read_to_string(&p).ok()?;
                Some((rel, text))
            })
            .collect()
    }

    /// 词条表的 key（就地解析 `i18n.rs` 的 match 臂：行首 `"key" => ...`）。
    /// 只看 `fn translate` 的函数体到 `#[cfg(test)]` 之间——文件别处（如
    /// `set_lang_str` 的 `"en" | "english" =>`）也有同形状的行，不属于词条表。
    fn table_keys() -> Vec<&'static str> {
        let src: &'static str = include_str!("i18n.rs");
        let start = src.find("fn translate(").expect("词条表函数必须存在");
        let end = src.find("\n#[cfg(test)]").unwrap_or(src.len());
        src[start..end]
            .lines()
            .filter_map(|l| {
                let rest = l.trim_start().strip_prefix('"')?;
                let ix = rest.find("\" =>")?;
                Some(&rest[..ix])
            })
            .collect()
    }

    /// 取 `name(` 之后的字符串字面量；跳过空白（含换行），容忍
    /// `.description(t(\n "…",\n))` 这种多行调用形态。
    /// 返回 (行号, 内容)。
    fn call_literals(text: &str, name: &str) -> Vec<(usize, String)> {
        let pat = format!("{name}(");
        let bytes = text.as_bytes();
        let mut out = Vec::new();
        let mut from = 0;
        while let Some(rel) = text[from..].find(&pat) {
            let at = from + rel;
            from = at + pat.len();
            // 前一字符若是标识符字符，说明这是 insert( / expect( / format!( 之类
            if at > 0 {
                let prev = bytes[at - 1] as char;
                if prev.is_alphanumeric() || prev == '_' {
                    continue;
                }
            }
            // 注释里的调用不算
            let line_start = text[..at].rfind('\n').map_or(0, |i| i + 1);
            if text[line_start..at].contains("//") {
                continue;
            }
            let mut j = at + pat.len();
            while j < bytes.len() && (bytes[j] as char).is_whitespace() {
                j += 1;
            }
            if j >= bytes.len() || bytes[j] != b'"' {
                continue;
            }
            j += 1;
            let mut raw: Vec<u8> = Vec::new();
            let mut esc = false;
            while j < bytes.len() {
                let b = bytes[j];
                if esc {
                    raw.push(match b {
                        b'n' => b'\n',
                        b't' => b'\t',
                        b'r' => b'\r',
                        // Rust 的行续接：反斜杠 + 换行 = 二者都消失，然后吃掉
                        // 后续的**行首空白**（`cargo fmt` 会把长文案折行，
                        // 续接后的空白是源码缩进，不属于文案本身）。
                        // 漏掉这一条会让续接串的 key 永远对不上词条表：
                        // g1 报「表里没有这个 key」，g2 同时报「表里这条没人用」，
                        // 同一个 bug 自相矛盾地报两次。
                        b'\n' => {
                            while j + 1 < bytes.len()
                                && matches!(bytes[j + 1], b' ' | b'\t' | b'\r' | b'\n')
                            {
                                j += 1;
                            }
                            esc = false;
                            continue;
                        }
                        other => other,
                    });
                    esc = false;
                } else if b == b'\\' {
                    esc = true;
                } else if b == b'"' {
                    break;
                } else {
                    raw.push(b);
                }
                j += 1;
            }
            // 注意：必须按字节收集后再整体解码。逐字节 `as char` 会把 UTF-8
            // 中文拆成乱码，导致与词条 key 永不相等（曾因此误报 300+ 处缺口）。
            out.push((
                text[..at].matches('\n').count() + 1,
                String::from_utf8_lossy(&raw).into_owned(),
            ));
        }
        out
    }

    /// 界面禁用的字符：emoji 与装饰性符号。
    /// 箭头 `↑`/`↓` 表达上下行语义，属正当用法，**不在**禁用集内。
    fn is_decorative(c: char) -> bool {
        matches!(c,
            '\u{1F300}'..='\u{1FAFF}'   // Emoji 区
            | '\u{2600}'..='\u{27BF}'   // 杂项符号与装饰符（含 ⚠ ⚡ ✅）
            | '\u{2B00}'..='\u{2BFF}'
            | '\u{25CF}' | '\u{25CB}'   // ● ○ 字符圆点
            | '\u{21C5}'                // ⇅
        )
    }

    fn decorative_hits(text: &str) -> Vec<char> {
        let mut v: Vec<char> = text.chars().filter(|c| is_decorative(*c)).collect();
        v.sort_unstable();
        v.dedup();
        v
    }

    #[test]
    fn g3_table_and_ui_text_have_no_decorative_chars() {
        // 词条表正文（key 与英文值都在这里；正文之外是测试代码，不算界面文案）
        let hits = decorative_hits(table_text());
        assert!(hits.is_empty(), "词条表不得出现装饰符号，命中：{hits:?}");

        // 进界面的字面量（ts / tp 自带两语言，一并查）
        for (path, text) in ui_sources() {
            for name in ["t", "tr", "ts", "tp"] {
                for (line, s) in call_literals(&text, name) {
                    let hits = decorative_hits(&s);
                    assert!(
                        hits.is_empty(),
                        "{path}:{line} 文案含装饰符号 {hits:?}：{s}"
                    );
                }
            }
        }
    }

    /// 词条表正文（`fn translate` 体，不含测试模块）
    fn table_text() -> &'static str {
        let src: &'static str = include_str!("i18n.rs");
        let start = src.find("fn translate(").expect("词条表函数必须存在");
        let end = src.find("\n#[cfg(test)]").unwrap_or(src.len());
        &src[start..end]
    }

    #[test]
    fn g1_every_t_call_has_an_entry() {
        // ts / tp 的实参自带中英两份，不查表，故只断言 t / tr
        let keys = table_keys();
        let mut missing = Vec::new();
        for (path, text) in ui_sources() {
            for name in ["t", "tr"] {
                for (line, s) in call_literals(&text, name) {
                    if !keys.contains(&s.as_str()) {
                        missing.push(format!("{path}:{line}  {s}"));
                    }
                }
            }
        }
        assert!(
            missing.is_empty(),
            "以下位置调了 t() 但词条表里没有对应 key（英文模式会原样显示中文）：\n{}",
            missing.join("\n")
        );
    }

    #[test]
    fn g2_no_dead_entries() {
        let srcs: Vec<String> = ui_sources().into_iter().map(|(_, t)| t).collect();
        let keys = table_keys();
        let dead: Vec<&str> = keys
            .into_iter()
            .filter(|k| {
                let quoted = format!("\"{k}\"");
                !srcs.iter().any(|s| s.contains(&quoted))
            })
            .collect();
        assert!(
            dead.is_empty(),
            "以下词条表里有、但代码里无人引用（改名后忘了删？）：{dead:?}"
        );
    }

    #[test]
    fn g4_no_double_translation() {
        // t(t( / t(i18n::t( / t(rustshell_core::i18n::t(
        fn nested(after: &str) -> bool {
            let s = after.trim_start();
            for head in ["t(", "i18n::t(", "rustshell_core::i18n::t("] {
                if s.starts_with(head) {
                    return true;
                }
            }
            false
        }
        let mut bad = Vec::new();
        for (path, text) in ui_sources() {
            let mut from = 0;
            while let Some(rel) = text[from..].find("t(") {
                let at = from + rel;
                from = at + 2;
                if at > 0 {
                    let prev = text.as_bytes()[at - 1] as char;
                    if prev.is_alphanumeric() || prev == '_' {
                        continue;
                    }
                }
                if nested(&text[from..]) {
                    bad.push(format!("{path}:{}", text[..at].matches('\n').count() + 1));
                }
            }
        }
        assert!(bad.is_empty(), "存在双重翻译 t(t(..))：{bad:?}");
    }
}
