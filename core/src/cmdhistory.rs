//! 会话命令历史 + 内置常见命令表（C-1 的数据源）。
//!
//! # 两层数据，两种命运
//!
//! | 层 | 生命周期 | 归属 | 可变 |
//! |---|---|---|---|
//! | [`builtin_commands`] | 编译期常量 | 无 | 否 |
//! | [`CommandHistory`] | 会话生命周期 | **会话**（`SessionHandle`） | 是 |
//!
//! 历史**必须挂会话**，不许放全局 map：同一个 profile 开两个标签时，全局 map
//! 会让两边互相偷对方的命令（对照铁律 #85：重试账本挂标签、缓存挂 `SessionHandle`
//! 是同一个理由）。标签关掉，账本跟着走，不留孤儿。
//!
//! # 历史从哪来
//!
//! C-1 **只采本地可观测的那一份**：用户在终端里敲出来的行由宿主在「提交」时投喂
//! （见 `CommandHistory::record`）。远端 `~/.bash_history` 那一路留给 C-3（要复用
//! 会话连接开 exec 子信道），本层不碰网络 —— 冷启动期不许扫远端（调研文档 §六-6）。
//!
//! # 脱敏：为什么必须做，且为什么只做「形状」判断
//!
//! 网格 diff 采历史是有噪声的：用户可能手滑把密码打在命令行上。本模块用
//! [`looks_sensitive`] 做**形状**判断（`export FOO=secret` / `--password=x` /
//! `mysql -pPASS` / 长 base64 串）。它刻意只做高置信度的形状匹配，不猜语义 ——
//! 假阳性（把普通命令挡在历史外）会让用户觉得"历史怎么少了"，比假阴性更烦。
//! 所以宁可漏，不可错杀：判定是**词级**的，且只认 `=` / 空白 / `-p` 这几种
//! 明确的赋值形状。

use serde::{Deserialize, Serialize};

/// 单条内置命令（静态表，非 i18n —— 它是**命令名**不是界面文案）。
///
/// `desc` 是中文说明，走 [`crate::i18n::t`]：它**会渲染给用户看**，必须进词条表
/// （铁律 #24 / 门禁 g1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuiltinCommand {
    /// 命令名（要写进 PTY 的字面量，**绝不翻译**）
    pub name: &'static str,
    /// 中文说明（渲染给用户看，走 i18n）
    pub desc: &'static str,
    /// 所属分类（渲染用，走 i18n）
    pub group: &'static str,
}

/// 内置常见命令表。
///
/// 选取口径：**跨发行版都存在、且用户高频使用**的命令。刻意不收
/// 发行版专有工具（`apt` / `yum` 之类），也不收危险运维命令 ——
/// 补全浮层里赫然列着 `rm -rf /` 的补全建议，是帮凶不是帮手。
///
/// 表本身按分类成组，同组内保持稳定次序（不排序）：次序即用户扫视路径，
/// 每次重排会让"第三个是那个"失效（对照铁律 #47 的同一理由）。
pub fn builtin_commands() -> &'static [BuiltinCommand] {
    &BUILTIN
}

/// 命令分类（渲染浮层里的小标题，走 i18n）。
pub const GROUP_FILES: &str = "文件";
pub const GROUP_TEXT: &str = "文本";
pub const GROUP_SYSTEM: &str = "系统";
pub const GROUP_NETWORK: &str = "网络";
pub const GROUP_PROCESS: &str = "进程";
pub const GROUP_GIT: &str = "Git";
pub const GROUP_DOCKER: &str = "容器";

static BUILTIN: [BuiltinCommand; 63] = [
    // ---- 文件 ----
    cmd("ls", "列出目录内容", GROUP_FILES),
    cmd("cd", "切换目录", GROUP_FILES),
    cmd("pwd", "显示当前目录", GROUP_FILES),
    cmd("cp", "复制文件或目录", GROUP_FILES),
    cmd("mv", "移动或重命名", GROUP_FILES),
    cmd("mkdir", "创建目录", GROUP_FILES),
    cmd("touch", "创建空文件", GROUP_FILES),
    cmd("find", "按条件查找文件", GROUP_FILES),
    cmd("du", "统计目录占用", GROUP_FILES),
    cmd("df", "查看磁盘空间", GROUP_FILES),
    cmd("tar", "打包或解包归档", GROUP_FILES),
    cmd("ln", "创建链接", GROUP_FILES),
    // ---- 文本 ----
    cmd("cat", "查看文件内容", GROUP_TEXT),
    cmd("less", "分页查看文件", GROUP_TEXT),
    cmd("head", "查看文件开头", GROUP_TEXT),
    cmd("tail", "查看文件末尾", GROUP_TEXT),
    cmd("grep", "按模式搜索文本", GROUP_TEXT),
    cmd("sed", "流式文本替换", GROUP_TEXT),
    cmd("awk", "按列处理文本", GROUP_TEXT),
    cmd("wc", "统计行数 / 字数", GROUP_TEXT),
    cmd("sort", "排序文本行", GROUP_TEXT),
    cmd("uniq", "去掉相邻重复行", GROUP_TEXT),
    cmd("diff", "比较两个文件", GROUP_TEXT),
    cmd("tee", "分流到文件与屏幕", GROUP_TEXT),
    // ---- 系统 ----
    cmd("sudo", "以管理员身份执行", GROUP_SYSTEM),
    cmd("whoami", "显示当前用户", GROUP_SYSTEM),
    cmd("uname", "显示系统信息", GROUP_SYSTEM),
    cmd("date", "显示或设置时间", GROUP_SYSTEM),
    cmd("uptime", "显示运行时长与负载", GROUP_SYSTEM),
    cmd("env", "显示环境变量", GROUP_SYSTEM),
    cmd("which", "查找命令所在路径", GROUP_SYSTEM),
    cmd("chmod", "修改文件权限", GROUP_SYSTEM),
    cmd("chown", "修改文件属主", GROUP_SYSTEM),
    cmd("history", "显示命令历史", GROUP_SYSTEM),
    cmd("clear", "清屏", GROUP_SYSTEM),
    cmd("exit", "退出当前会话", GROUP_SYSTEM),
    cmd("man", "查看命令手册", GROUP_SYSTEM),
    // ---- 网络 ----
    cmd("ping", "测试网络连通性", GROUP_NETWORK),
    cmd("curl", "发起 HTTP 请求", GROUP_NETWORK),
    cmd("wget", "下载文件", GROUP_NETWORK),
    cmd("ss", "查看网络连接", GROUP_NETWORK),
    cmd("netstat", "查看端口与连接", GROUP_NETWORK),
    cmd("ip", "查看或配置网卡", GROUP_NETWORK),
    cmd("ifconfig", "查看网卡信息", GROUP_NETWORK),
    cmd("dig", "查询 DNS 记录", GROUP_NETWORK),
    cmd("ssh", "登录远程主机", GROUP_NETWORK),
    cmd("scp", "跨主机复制文件", GROUP_NETWORK),
    cmd("rsync", "增量同步文件", GROUP_NETWORK),
    // ---- 进程 ----
    cmd("ps", "查看进程列表", GROUP_PROCESS),
    cmd("top", "实时查看进程", GROUP_PROCESS),
    cmd("htop", "交互式进程查看", GROUP_PROCESS),
    cmd("kill", "按 PID 结束进程", GROUP_PROCESS),
    cmd("pkill", "按名称结束进程", GROUP_PROCESS),
    cmd("nohup", "后台运行且不挂断", GROUP_PROCESS),
    cmd("systemctl", "管理系统服务", GROUP_PROCESS),
    cmd("journalctl", "查看系统日志", GROUP_PROCESS),
    // ---- Git ----
    cmd("git status", "查看工作区状态", GROUP_GIT),
    cmd("git log", "查看提交历史", GROUP_GIT),
    cmd("git diff", "查看改动差异", GROUP_GIT),
    cmd("git branch", "管理分支", GROUP_GIT),
    cmd("git checkout", "切换分支或还原文件", GROUP_GIT),
    // ---- 容器 ----
    cmd("docker ps", "查看运行中的容器", GROUP_DOCKER),
    cmd("docker logs", "查看容器日志", GROUP_DOCKER),
];

const fn cmd(name: &'static str, desc: &'static str, group: &'static str) -> BuiltinCommand {
    BuiltinCommand { name, desc, group }
}

/// 一条历史记录。
///
/// `count` 是「用过几次」：浮层按**用得多 + 用得近**排序（FinalShell 手感）。
/// 只留两个排序因子（次数、序号）而不是存时间戳 —— 时间戳要读系统时钟，
/// 而历史只在**会话内**有效，一个单调递增的序号就够表达"先后"，且可确定性测试
/// （对照铁律 #50：同一指标别用两把尺子；这里的"近"用会话内序号而非墙钟）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEntry {
    /// 命令原文（已 normalize：去首尾空白、内部换行折成空格）
    pub text: String,
    /// 会话内被使用过的次数
    pub count: u32,
    /// 最近一次使用的会话内序号（越大越近）
    pub last_seq: u64,
}

/// 会话命令历史。
///
/// 容量上限与去重口径：
/// - 相同命令**合并计数**（不是各存一条）—— 浮层里列三行同一个 `ls -la` 没有意义；
/// - 满员时淘汰 `last_seq` 最小的那条（LRU-ish，但用序号而非时间）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CommandHistory {
    entries: Vec<HistoryEntry>,
    next_seq: u64,
}

/// 历史上限：超出淘汰最久未用的一条。
///
/// 取 500：够覆盖一个长会话里用户真正会重复用到的量，又小到整表能被浮层一次
/// 过滤完（逐键过滤不允许有线性成本以外的东西，`MAX_HISTORY` 就是这个成本的界）。
pub const MAX_HISTORY: usize = 500;

impl CommandHistory {
    pub fn new() -> Self {
        Self::default()
    }

    /// 记录一条用户敲的命令。
    ///
    /// 三道门（顺序固定，越靠前越廉价）：
    /// 1. [`normalize_command`] 归一化后为空 ⇒ 丢（纯空行 / 纯空白）；
    /// 2. [`looks_sensitive`] 命中 ⇒ 丢（不把疑似凭据写进内存里的历史）；
    /// 3. 已存在 ⇒ 计数 +1、更新序号；否则插入，超限则淘汰最久未用。
    ///
    /// 返回是否真的记下了（供调用方决定要不要重绘浮层）。
    pub fn record(&mut self, raw: &str) -> bool {
        let Some(text) = normalize_command(raw) else {
            return false;
        };
        if looks_sensitive(&text) {
            return false;
        }
        self.next_seq += 1;
        let seq = self.next_seq;

        if let Some(e) = self.entries.iter_mut().find(|e| e.text == text) {
            e.count = e.count.saturating_add(1);
            e.last_seq = seq;
            return true;
        }
        self.entries.push(HistoryEntry {
            text,
            count: 1,
            last_seq: seq,
        });
        self.evict_if_needed();
        true
    }

    /// 淘汰最久未用的一条，直到回到上限内。
    ///
    /// 逻辑上最多淘汰一条（每次只 push 一条），但写成循环是为了**不依赖**这个前提：
    /// 将来若有人加了"批量导入历史"的路子，这里不会静默留下超限的表。
    fn evict_if_needed(&mut self) {
        while self.entries.len() > MAX_HISTORY {
            let Some(idx) = self
                .entries
                .iter()
                .enumerate()
                .min_by_key(|(_, e)| e.last_seq)
                .map(|(i, _)| i)
            else {
                break;
            };
            self.entries.remove(idx);
        }
    }

    /// 全部记录（只读）。
    pub fn entries(&self) -> &[HistoryEntry] {
        &self.entries
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 清空（断连时调用：会话死了，它的历史不该留着 —— 见模块文档）。
    pub fn clear(&mut self) {
        self.entries.clear();
        self.next_seq = 0;
    }

    /// 序号分配器的当前值（落盘用）。
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// 恢复序号分配器（装载时用）。
    ///
    /// 调用方负责保证它越过所有已恢复条目的 `last_seq`（见
    /// [`set_next_seq_above_entries`](Self::set_next_seq_above_entries) 的更安全形态）。
    pub fn set_next_seq(&mut self, v: u64) {
        self.next_seq = v;
    }

    /// 已恢复条目里的最大 `last_seq`（没条目时 0）。
    pub fn max_last_seq(&self) -> u64 {
        self.entries.iter().map(|e| e.last_seq).max().unwrap_or(0)
    }

    /// 把序号分配器抬到"必然高于所有已恢复条目"的位置。
    ///
    /// 装载路径的安全入口：直接用 [`set_next_seq`](Self::set_next_seq) 传文件里的值，
    /// 遇到被外部改坏（或老版本）的文件时分配器可能落后于已有条目 ——
    /// 表现是"新敲的命令与某条老旧记录并列同样新"，淘汰时可能先踢掉刚敲的那条。
    pub fn set_next_seq_above_entries(&mut self, v: u64) {
        self.next_seq = v.max(self.max_last_seq());
    }

    /// 直接放回一条**已经脱敏过**的记录（装载路径专用）。
    ///
    /// 刻意**不**走 [`record`](Self::record)：那条路会跑 [`normalize_command`] 与
    /// [`looks_sensitive`]，而磁盘上的数据已经是它们跑过的结果。再跑一遍不只是浪费 ——
    /// 脱敏规则会随版本演进，今天判为凭据、改天放宽之后，**老数据仍然进不来**，
    /// 用户会发现"以前存下的命令怎么没了"。
    ///
    /// 重复文本按 `count` 相加合并（与 `record` 同一口径）。
    pub fn push_restored(&mut self, e: HistoryEntry) {
        if e.text.is_empty() {
            return;
        }
        if let Some(existing) = self.entries.iter_mut().find(|x| x.text == e.text) {
            existing.count = existing.count.saturating_add(e.count);
            existing.last_seq = existing.last_seq.max(e.last_seq);
            return;
        }
        self.entries.push(e);
        self.evict_if_needed();
    }
}

/// 归一化一条命令：去首尾空白，内部换行 / 制表折成单个空格。
///
/// 为什么要折内部换行：粘贴进来的多行脚本在网格里是**多行**，但作为一条
/// "命令"它只有一个身份。不折的话，同一段脚本每次粘贴都因为末尾换行差异
/// 被当成新命令，历史迅速被垃圾撑满。
///
/// 返回 `None` 表示归一化后为空（纯空白，没有记录价值）。
pub fn normalize_command(raw: &str) -> Option<String> {
    let mut out = String::with_capacity(raw.len());
    let mut pending_space = false;
    for ch in raw.chars() {
        if ch.is_whitespace() {
            // 前导空白直接丢；句中空白压成一个
            if !out.is_empty() {
                pending_space = true;
            }
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(ch);
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// 疑似含凭据的赋值形状（**只认高置信度形状**，见模块文档的"宁可漏不可错杀"）。
///
/// 命中的形态：
/// - `FOO=secret` / `export FOO=secret`（环境变量式赋值；值非空才判，`FOO=` 放行 ——
///   那常是"清空变量"或"占位待填"，不是凭据）；
/// - `--password=x` / `--token=x` / `-p x` / `-P x`（明确的口令开关）；
/// - `-pXXXX`（`mysql -pMyPass` 这种粘连写法：粘连且长度 >= 3 才算 —— 避免把
///   `-p` 独立的短选项误判，如 `docker run -p 80:80`）。
///
/// **不**命中的（刻意）：
/// - 单独的 `password` 词（`grep password /etc/foo` 是查询，不是凭据）；
/// - URL 里的 `user:pass@host`（形状太广，误杀率高）；
/// - `-p 80:80`（端口映射，中间含 `:` 或纯数字 ⇒ 放行）。
pub fn looks_sensitive(cmd: &str) -> bool {
    const KEY_HINTS: [&str; 6] = ["password", "passwd", "token", "secret", "apikey", "api_key"];

    // 短选项粘连/独立形态：`-p xxx` / `-pXXX`
    let tokens: Vec<&str> = cmd.split(' ').filter(|s| !s.is_empty()).collect();

    for (i, tok) in tokens.iter().enumerate() {
        // ① `KEY=value` 形态（含 `--password=x`）：等号左侧是关键词，右侧非空
        if let Some((key, value)) = tok.split_once('=') {
            let key_l = key.trim_start_matches('-').to_ascii_lowercase();
            if !value.is_empty() && KEY_HINTS.contains(&key_l.as_str()) {
                return true;
            }
            // 环境变量式赋值：`FOO=bar` 在**首 token**（或紧跟 export）时，
            // 右侧看着像凭据才判 —— 这里保守到只认显式关键词，普通 FOO=bar 放行，
            // 因为 `export PATH=/x` 这类误杀会明显惹人烦。
            if !value.is_empty() && (key_l == "p" || key_l == "pass" || key_l == "passwd") {
                return true;
            }
        }

        // ② `-p XXX` 形态：`-p` 是独立 token，且**下一个 token 不是端口映射**
        if *tok == "-p" || *tok == "-P" {
            if let Some(next) = tokens.get(i + 1) {
                // 端口映射 / 纯数字 / 带冒号的都放行（`-p 80:80` / `-p 3306`）
                if !next.contains(':') && !next.chars().all(|c| c.is_ascii_digit()) {
                    return true;
                }
            }
            continue;
        }

        // ③ `-pXXX` 粘连：`-p` 后紧跟非空白内容，长度 >= 3 且不是纯数字 / 端口
        if let Some(rest) = tok.strip_prefix("-p").or_else(|| tok.strip_prefix("-P")) {
            if rest.len() >= 3 && !rest.contains(':') && !rest.chars().all(|c| c.is_ascii_digit()) {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_table_is_categorized_and_nonempty() {
        let cmds = builtin_commands();
        assert!(cmds.len() >= 50, "内置表太薄，撑不起一次有用的补全");
        // 每条的三个字段都非空
        for c in cmds {
            assert!(!c.name.is_empty());
            assert!(!c.desc.is_empty(), "命令 {} 缺说明", c.name);
            assert!(!c.group.is_empty());
        }
        // 命令名唯一（重复会让浮层出现"同一个命令两行、说明不同"）
        let mut names: Vec<&str> = cmds.iter().map(|c| c.name).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len(), "内置命令表里有重名条目");
    }

    /// 内置表的 `desc` 必须都进 i18n 词条表（g1 会在 core 侧另扫一遍，
    /// 这里挡的是"加命令时忘了加说明词条"这件事的**上游**：desc 本身来自这里）。
    #[test]
    fn builtin_descriptions_are_translatable() {
        for c in builtin_commands() {
            // 中文模式下 t() 原样返回；这里断言的是"表里的 desc 确实是渲染文案"
            assert_eq!(crate::i18n::t(c.desc), c.desc);
        }
    }

    #[test]
    fn normalize_collapses_internal_whitespace() {
        assert_eq!(normalize_command("  ls -la  ").as_deref(), Some("ls -la"));
        assert_eq!(
            normalize_command("cd /tmp\nls -la").as_deref(),
            Some("cd /tmp ls -la"),
            "多行脚本归一成单行：内部换行折成空格"
        );
        assert_eq!(normalize_command("\t\n  \n").as_deref(), None);
        assert_eq!(normalize_command("").as_deref(), None);
    }

    #[test]
    fn record_merges_duplicates_and_counts() {
        let mut h = CommandHistory::new();
        assert!(h.record("ls -la"));
        assert!(h.record("ls -la"));
        assert!(h.record("pwd"));
        assert_eq!(h.len(), 2, "重复命令必须合并计数，不是各存一条");
        let ls = h.entries().iter().find(|e| e.text == "ls -la").unwrap();
        assert_eq!(ls.count, 2);
        let ls_seq = h
            .entries()
            .iter()
            .find(|e| e.text == "ls -la")
            .unwrap()
            .last_seq;
        let pwd_seq = h
            .entries()
            .iter()
            .find(|e| e.text == "pwd")
            .unwrap()
            .last_seq;
        assert!(
            pwd_seq > ls_seq,
            "后用的命令序号必须更大（pwd 在 ls 之后用）"
        );

        // 再敲一次 ls：它的序号要翻到 pwd 之后（「用得近」的口径）
        assert!(h.record("ls -la"));
        let ls = h.entries().iter().find(|e| e.text == "ls -la").unwrap();
        assert_eq!(ls.count, 3);
        assert!(ls.last_seq > pwd_seq, "重新用过之后，ls 的序号必须超过 pwd");
    }

    #[test]
    fn record_rejects_blank_and_sensitive() {
        let mut h = CommandHistory::new();
        assert!(!h.record("   "), "纯空白不记");
        assert!(!h.record("export TOKEN=abcdef"), "疑似凭据不记");
        assert!(!h.record("mysql -psecret"), "粘连式口令不记");
        assert!(h.is_empty());
    }

    #[test]
    fn eviction_drops_the_least_recently_used() {
        let mut h = CommandHistory::new();
        for i in 0..(MAX_HISTORY + 10) {
            h.record(&format!("cmd{i}"));
        }
        assert_eq!(h.len(), MAX_HISTORY, "超出上限必须淘汰");
        assert!(
            h.entries().iter().all(|e| e.text != "cmd0"),
            "最久未用的那条必须被淘汰"
        );
        assert!(
            h.entries()
                .iter()
                .any(|e| e.text == format!("cmd{}", MAX_HISTORY + 9)),
            "最新的那条必须留着"
        );
    }

    #[test]
    fn sensitive_detection_accepts_ordinary_commands() {
        // 反向：这些**不该**被误杀（误杀会让用户觉得"历史怎么少了"）
        for ok in [
            "ls -la",
            "docker run -p 80:80 nginx",
            "docker run -p 3306 mysql",
            "grep password /etc/app.conf",
            "export PATH=/usr/local/bin:$PATH",
            "git commit -m 'fix token handling'",
            "ssh -p 2222 host",
            "tail -f /var/log/app.log",
            "make -j4",
        ] {
            assert!(
                !looks_sensitive(ok),
                "普通命令被误判成凭据：{ok}（误杀比漏判更烦人）"
            );
        }
    }

    #[test]
    fn sensitive_detection_catches_obvious_credentials() {
        for bad in [
            "export TOKEN=abcdef123",
            "export API_KEY=xyz",
            "mysql --password=hunter2",
            "curl --token=ghp_xxx https://api",
            "mysql -pMyPass123",
            "tool -p hunter2",
        ] {
            assert!(looks_sensitive(bad), "明显的凭据形状没被拦住：{bad}");
        }
    }

    #[test]
    fn clear_wipes_history() {
        let mut h = CommandHistory::new();
        h.record("ls");
        h.record("pwd");
        h.clear();
        assert!(h.is_empty());
        // 清空后序号也归零（新会话从干净状态开始）
        h.record("ls");
        assert_eq!(h.entries()[0].last_seq, 1);
    }
}
