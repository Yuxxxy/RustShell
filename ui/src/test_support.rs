//! 测试专用的「读自己源码」工具（仅测试构建编译）。
//!
//! RustShell 有一大批护栏测试靠「扫源码、断言某个字符串在 / 不在」来钉住
//! 界面结构（面板摆了哪些条目、哪个 id 还在、文案表有没有跟上）。这类断言
//! 只有走**同一个取源入口**才有牙，否则会反复踩两个坑：
//!
//! 1. **自我命中**：断言的「针」就写在被扫文件的测试模块里。若取源时没有
//!    切掉 `#[cfg(test)]` 之后的测试代码，`contains` 会命中针自己 ——
//!    负向断言（`!contains`）恒失败，正向断言恒通过，护栏形同虚设。
//!    `prod()` 一律切掉测试模块，从根上消掉这一类失效。
//! 2. **字符串里的 `//` 被当注释**：一度有实现按行 `line.split("//").next()`
//!    剥注释，于是 `"https://api.deepseek.com/v1"` 这种生产代码被整段切掉
//!    （URL 里的 `//` 被当成行注释起点），断言随之变松或假红。
//!    `strip_comments()` 按 `char` 迭代并跟踪字符串 / 字符字面量状态。
//!
//! 新增扫源码的护栏时，只有一个正确取源姿势：
//!
//! ```ignore
//! let code = crate::test_support::prod(include_str!("relative/path.rs"));
//! ```
//!
//! 断言的针可以直接写成字面量，不必再拼 `concat!` 绕开自我命中。
//!
//! 注意：机器护栏 `every_rs_source_scan_goes_through_prod` 只能静态认出
//! `include_str!`。走 `std::fs::read_dir` 逐文件读源码的扫描（`notice.rs` 的
//! `ui_sources()`）同样要在 `contains` 之前套 `prod()` —— 那条路只能靠人守。

/// 剥掉 Rust 源码里的行注释与块注释，只留下可执行代码（含字符串字面量）。
///
/// 行号会被保留（块注释里的换行补一个 `\n`），方便断言报告里对得上位置。
pub(crate) fn strip_comments(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    // ⚠️ 必须按 **char** 迭代，不能按 byte：早期实现用
    // `bytes[i] as char` 逐字节推进，中文（3 字节 UTF-8）会被按 Latin-1
    // 解释成乱码 —— 于是所有针对中文界面文案的断言全部假红/假绿。
    let chars: Vec<char> = src.chars().collect();
    let mut i = 0usize;
    let mut in_line = false;
    let mut in_block = 0usize; // 0 = 不在块注释；否则为嵌套深度
    let mut in_str = false;
    let mut in_char = false;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        if in_line {
            if c == '\n' {
                in_line = false;
                out.push(c);
            }
            i += 1;
            continue;
        }
        if in_block > 0 {
            if c == '/' && next == Some('*') {
                in_block += 1;
                i += 2;
            } else if c == '*' && next == Some('/') {
                in_block -= 1;
                i += 2;
            } else {
                if c == '\n' {
                    out.push('\n'); // 保留行号
                }
                i += 1;
            }
            continue;
        }
        if in_str {
            out.push(c);
            if c == '\\' {
                if let Some(n) = next {
                    out.push(n);
                }
                i += 2;
                continue;
            }
            if c == '"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        if in_char {
            out.push(c);
            if c == '\\' {
                if let Some(n) = next {
                    out.push(n);
                }
                i += 2;
                continue;
            }
            if c == '\'' {
                in_char = false;
            }
            i += 1;
            continue;
        }
        // 原始字符串（`r"…"` / `r#"…"#`）必须**整段照抄**：里面的反斜杠不是转义。
        // 按普通字符串处理时，`r"\\?\UNC\"` 这种**以反斜杠结尾**的原始串会被读成
        // 「字符串还没结束」，一路吞到下一个引号 —— 中间夹着的注释于是被当成代码
        // （2026-10-28 实测：`clean_local_path` 的三个 `r"…"` 让 sftp_tab 的结构护栏
        // 读到注释里的「新建」，误报「行菜单里还有新建」）。
        if c == 'r' {
            let mut hashes = 0usize;
            let mut k = i + 1;
            while chars.get(k) == Some(&'#') {
                hashes += 1;
                k += 1;
            }
            if chars.get(k) == Some(&'"') {
                let mut j = k + 1;
                let mut end = None;
                while j < chars.len() {
                    if chars[j] == '"' && (0..hashes).all(|h| chars.get(j + 1 + h) == Some(&'#')) {
                        end = Some(j + 1 + hashes);
                        break;
                    }
                    j += 1;
                }
                if let Some(end) = end {
                    for &ch in &chars[i..end] {
                        out.push(ch);
                    }
                    i = end;
                    continue;
                }
            }
        }
        if c == '/' && next == Some('/') {
            in_line = true;
            i += 2;
            continue;
        }        if c == '/' && next == Some('*') {
            in_block = 1;
            i += 2;
            continue;
        }
        if c == '"' {
            in_str = true;
            out.push(c);
            i += 1;
            continue;
        }
        if c == '\'' {
            // 区分字符字面量与生命周期（`'a`），只把「'x'」当字符字面量
            let is_char_lit = chars.get(i + 2) == Some(&'\'')
                || (chars.get(i + 1) == Some(&'\\')
                    && chars.iter().skip(i + 1).take(6).any(|c| *c == '\''));
            if is_char_lit {
                in_char = true;
            }
            out.push(c);
            i += 1;
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

/// 源码到「只含生产代码」：**行尾归一 (CRLF→LF)** + 剥注释 +
/// 切掉从 `#[cfg(test)]` 起的测试模块。
///
/// 扫源码的护栏一律用这个入口，不要再自己 `include_str!` + `strip_comments`。
///
/// ## 为什么必须在这里归一 `\r\n`
///
/// `include_str!` 读进来的是**磁盘上的原始字节**：本仓在 Windows 上检出时，
/// `main_view.rs` 等文件的换行是 `\r\n`，而 `\n` 与 `\r\n` 是两种**不同的**
/// 字节序列 —— 任何拿 `"\n}\n"` / `"\n}"` / `"\n\n"` 当锚点的护栏，在 CRLF
/// 检出上都会**找不到**（LALR 一点：`\r\n}\r\n` 里根本没有 `\n}\n`）。
///
/// 2026-09-28 实测：`ai_chat_history_is_wired_to_disk` 里
/// `.find("\n}\n")` 在 CRLF 检出上恒返回 `None` ⇒ `.expect(...)` 直接 panic，
/// 而且**只在 Windows 检出上红**、CI（LF）上是绿的 —— 最坏的一类护栏失效：
/// 本机挂、远端过，改了本机的锚点去「修」它就又变成只在 CI 上挂。
///
/// 归一只做一次、只在这个唯一入口做：所有下游（`strip_comments`、切片、
/// `contains`）拿到的都是 LF 形态，护栏从此与检出配置无关。
pub(crate) fn prod(src: &str) -> String {
    let normalized = src.replace("\r\n", "\n");
    let mut code = strip_comments(&normalized);
    if let Some(i) = code.find("#[cfg(test)]") {
        code.truncate(i);
    }
    code
}

/// 取源码里 [`from`, `to`) 之间的片段（`to` 在 `from` 之后找）。
///
/// 用来把断言**钉在某个函数体里**：整文件级 `contains` 会让「这个调用出现在
/// 另一个函数里」也算通过，护栏就白挂了。
///
/// 传进来的 `src` 应当是 [`prod`] 的输出 —— 剥过注释之后，注释里举例的写法
/// 不会被算成调用。
/// 取源码里 [`from`, `to`) 之间的片段（`to` 在 `from` 之后找）。
///
/// 用来把断言**钉在某个函数体里**：整文件级 `contains` 会让「这个调用出现在
/// 另一个函数里」也算通过，护栏就白挂了。
///
/// 传进来的 `src` 应当是 [`prod`] 的输出 —— 剥过注释之后，注释里举例的写法
/// 不会被算成调用。
pub(crate) fn region<'a>(src: &'a str, from: &str, to: &str) -> &'a str {
    let a = src
        .find(from)
        .unwrap_or_else(|| panic!("源码里找不到起点 `{from}`（是不是被改名/删了？）"));
    let b = src[a..]
        .find(to)
        .map(|i| a + i)
        .unwrap_or_else(|| panic!("源码里找不到终点 `{to}`（在 `{from}` 之后）"));
    &src[a..b]
}

/// **crate 级**串行锁：给「要改进程级环境变量」的测试用。
///
/// `RUSTSHELL_CONFIG_DIR` 这类变量是**整个测试进程共享**的，而 `cargo test`
/// 会并行跑**同一个二进制里所有 crate 的用例** —— 所以锁必须是 crate 级
/// 唯一一把，放在**每个文件各自的 `static Mutex`** 上是不够的：
/// 那样两个文件各锁各的，并行跑时照样互相偷对方的配置目录。
///
/// 2026-10-12 实测踩过：`keybind_actions`（锁 A）与 `settings_actions`（锁 B）
/// 各有各的锁，全量 `cargo test` 时前者刚 `set_var` 完、后者立刻覆盖成自己的
/// 目录，前者接着去 `save_overrides` 就写进了别人的目录 ⇒ 间歇性失败
/// （单跑该文件永远绿，全量才红 —— 最坏的一类不稳定）。
///
/// 返回值必须**持有到用例结束**（`let _guard = ...`），它既是锁也是「清理者」。
#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 把 `RUSTSHELL_CONFIG_DIR` 指向一个**独占的临时目录**，并返回守卫。
///
/// 用例结束（守卫 drop）时把变量**还原**——不还原的话，后续不拿守卫的用例
/// 会继承上一个用例的目录，出现同样难查的串味。
#[cfg(test)]
pub(crate) fn config_env_guard(name: &str) -> (EnvRestore, std::path::PathBuf) {
    let guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let prev = std::env::var("RUSTSHELL_CONFIG_DIR").ok();
    let dir = std::env::temp_dir().join(format!("rustshell-cfg-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("临时配置目录建不出来");
    std::env::set_var("RUSTSHELL_CONFIG_DIR", &dir);
    (EnvRestore { guard, prev }, dir)
}

/// 随守卫一起 drop，负责还原环境变量。
#[cfg(test)]
pub(crate) struct EnvRestore {
    #[allow(dead_code)]
    guard: std::sync::MutexGuard<'static, ()>,
    prev: Option<String>,
}

#[cfg(test)]
impl Drop for EnvRestore {
    fn drop(&mut self) {
        match self.prev.take() {
            Some(p) => std::env::set_var("RUSTSHELL_CONFIG_DIR", p),
            None => std::env::remove_var("RUSTSHELL_CONFIG_DIR"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{prod, strip_comments};

    /// 原始字符串必须整段照抄 —— 尤其**以反斜杠结尾**的那种。
    ///
    /// `r"\\?\UNC\"` 是 Windows verbatim 路径的字面量写法；按普通字符串解析时，
    /// 结尾的 `\"` 会被当成「转义的引号」，字符串永远不闭合，后面几十行里夹着的
    /// **注释**就被当成代码读进去了（2026-10-28：sftp_tab 的结构护栏因此误报
    /// 「行菜单里还有新建」）。`r#"…"#` 同理。
    #[test]
    fn strip_comments_keeps_raw_strings_verbatim() {
        let src = "let a = r\"\\\\?\\UNC\\\";\n// 这行注释必须被剥掉\nlet b = r#\"含 \" 与 // 的原始串\"#;\n";
        let stripped = strip_comments(src);
        assert!(
            stripped.contains(r#"r"\\?\UNC\""#),
            "原始字符串被改写了：{stripped:?}"
        );
        assert!(!stripped.contains("必须被剥掉"), "原始串之后的注释没被剥掉");
        assert!(
            stripped.contains(r##"r#"含 " 与 // 的原始串"#"##),
            "带井号的原始串被截断了：{stripped:?}"
        );
    }

    /// 非 ASCII（中文）内容必须原样保留。
    ///
    /// 这是真实踩过的坑：早先 `strip_comments` 按**字节**迭代，
    /// `bytes[i] as char` 把 3 字节 UTF-8 中文按 Latin-1 解释 ⇒ 输出变乱码，
    /// 于是所有「设置页必须有【光标与粘贴】分组」这类中文断言全部失效（假红）。
    #[test]
    fn strip_comments_keeps_non_ascii_intact() {
        let src = "let s = t(\"光标与粘贴\"); // 这句中文注释应被剥掉\n";
        let stripped = strip_comments(src);
        assert!(
            stripped.contains("光标与粘贴"),
            "字符串里的中文必须原样保留，实际得到：{stripped:?}"
        );
        assert!(!stripped.contains("应被剥掉"));
    }

    /// `strip_comments` 自身的护栏：确保它真的剥掉了注释、且没破坏字符串。
    #[test]
    fn strip_comments_drops_comments_but_keeps_code() {
        let demo = r#"
            // track_focus(&self.panel_ui.focus) 只出现在注释里
            /* 块注释里的 track_focus(&self.panel_ui.focus) */
            let a = "track_focus(&self.panel_ui.focus)"; // 字符串里的必须保留
            let b = 'x';
        "#;
        let stripped = strip_comments(demo);
        // 行注释与块注释都没了
        assert!(!stripped.contains("只出现在注释里"));
        assert!(!stripped.contains("块注释里的"));
        // 字符串字面量保留（这是真实的「代码」）
        assert!(stripped.contains("track_focus(&self.panel_ui.focus)"));
        // 行号未被压扁
        assert_eq!(stripped.lines().count(), demo.lines().count());
    }

    /// 字符串字面量里的 `//` **不是**注释起点。
    ///
    /// 这正是「逐行 `line.split("//")`」那版实现的缺陷：`https://…` 这类
    /// 生产代码会被整段切掉，针对它的断言随之变松或假红。
    #[test]
    fn strip_comments_keeps_slashes_inside_string_literals() {
        let src = concat!(
            "let endpoint = \"https://api.deepseek.com/v1\"; // 注释\n",
            "let path = \"C:\\\\tmp\"; // 反斜杠不吞噬引号\n"
        );
        let stripped = strip_comments(src);
        assert!(
            stripped.contains("https://api.deepseek.com/v1"),
            "字符串里的 // 必须保留（URL 被当成注释会让护栏变松）：{stripped:?}"
        );
        assert!(stripped.contains("C:\\\\tmp"), "反斜杠转义不能被吞掉");
        assert!(!stripped.contains("注释"), "行注释仍须剥掉");
    }

    /// `prod()` 必须切掉测试模块：否则「针」会自己命中自己。
    #[test]
    fn prod_cuts_the_test_module_so_needles_cannot_self_match() {
        let src = concat!(
            "fn production() {}\n",
            "#[cfg(test)]\n",
            "mod tests {\n",
            "    #[test]\n",
            "    fn t() {\n",
            "        assert!(!code.contains(\".mem_summary()\"));\n",
            "    }\n",
            "}\n"
        );
        let code = prod(src);
        assert!(code.contains("fn production() {}"), "生产代码必须留下");
        assert!(
            !code.contains(".mem_summary()"),
            "测试模块里的针必须被切掉，否则 contains 命中针自己：{code:?}"
        );
        // 反证：不切测试模块就会自我命中
        assert!(
            strip_comments(src).contains(".mem_summary()"),
            "本用例的前提是「不切测试模块就会自命中」，先证明它成立。"
        );
    }

    /// 没有测试模块的文件原样保留（例如被扫的 `Cargo.toml` / 外部 crate 源码）。
    #[test]
    fn prod_without_a_test_module_keeps_all_code() {
        let src = "pub mod clipboard;\nfn helper() {}\n";
        assert_eq!(prod(src), src);
    }

    /// `prod()` 必须把 CRLF 归一成 LF —— 否则所有「以换行为锚点」的护栏
    /// 在 Windows 检出上恒红（2026-09-28 的真实事故，见 `prod` 的文档）。
    ///
    /// 这条护栏本身也必须在**两种行尾下都成立**，所以源码里的字面量一律用
    /// `\r\n` 拼出来，不依赖本文件是怎么检出的。
    #[test]
    fn prod_normalizes_crlf_to_lf() {
        let crlf = "fn a() {\r\n    b();\r\n}\r\n";
        let code = prod(crlf);
        assert!(
            !code.contains('\r'),
            "输出里不该再有 CR（CRLF 检出下以换行为锚点的断言会找不到）：{code:?}"
        );
        // 归一之后，「行首闭合括号 + 换行」这个锚点必须真的能命中
        assert!(
            code.contains("\n}\n"),
            "归一后应能匹配「换行 + 行首 }} + 换行」：{code:?}"
        );
        // LF 检出不受影响（幂等）
        assert_eq!(prod("fn a() {\n    b();\n}\n"), "fn a() {\n    b();\n}\n");
        // CRLF 的**测试模块切分**也不能因为归一而失灵
        let with_tests = concat!(
            "fn production() {}\r\n",
            "#[cfg(test)]\r\n",
            "mod tests { fn t() { needle() } }\r\n"
        );
        let cut = prod(with_tests);
        assert!(cut.contains("fn production()"));
        assert!(
            !cut.contains("needle()"),
            "CRLF 检出下也必须切掉测试模块：{cut:?}"
        );
    }

    /// 机器护栏：本 crate 测试区里**凡是扫 `.rs` 源码的 `include_str!` 都必须走 `prod()`**。
    ///
    /// 挡的是两类真实踩过的回归：
    ///   * 裸 `include_str!` 直接进 `contains` ⇒ 针命中自己（负断言恒失败、
    ///     正断言恒通过），`main_view.rs` 与 `status_bar.rs` 各中过一次；
    ///   * 自己写一版剥注释（曾用逐行 `line.split("//")`）⇒ 字符串里的 `//`
    ///     被当注释起点，`"https://…"` 所在的生产代码被整段切掉，护栏悄悄变松
    ///     （`dashboard_panel` / `ai_panel` / `settings_panel` / `terminal_scrollbar` 四处）。
    ///
    /// 非 `.rs` 的取值（如 `terminal/Cargo.toml`）不在约束内：TOML 里没有
    /// Rust 注释语法，套 `prod()` 反而会误切 `//`。
    #[test]
    fn every_rs_source_scan_goes_through_prod() {
        // 针要拼出来 —— 直接写这个字面量就会扫到自己（正是本文件在防的坑）。
        let needle = ["include", "_str!"].concat();
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        collect_rs(&root, &mut files);
        let mut bad = Vec::new();
        for f in files {
            let Ok(text) = std::fs::read_to_string(&f) else {
                continue;
            };
            let Some(scan) = text.find("#[cfg(test)]") else {
                continue; // 没有测试区，不适用
            };
            let region = &text[scan..];
            for (at, _) in region.match_indices(needle.as_str()) {
                // 只取**调用实参那一个字面量**：若拿「从该点起的整个余文」去判，
                // 文件后面任意一个 `.rs"` 都会让非源码取值被误判成源码（踩过一次）。
                let arg = region[at + needle.len()..]
                    .strip_prefix('(')
                    .unwrap_or("")
                    .trim_start();
                let Some(rest) = arg.strip_prefix('"') else {
                    continue;
                };
                let Some(close) = rest.find('"') else {
                    continue;
                };
                // 只约束「扫 Rust 源码」的调用
                if !rest[..close].ends_with(".rs") {
                    continue;
                }
                if !region[..at].ends_with("prod(") {
                    bad.push(format!("{} 测试区内第 {} 行", f.display(), at));
                }
            }
        }
        assert!(
            bad.is_empty(),
            "这些位置扫的是 .rs 源码却没走 prod()（会自我命中，或自己剥注释时把字符串里的 // 当注释）：{bad:?}"
        );
    }

    /// 机器护栏：UI 里点名的图标，`AppAssets` 必须真的发得出来。
    ///
    /// `AppAssets`（`shell/src/app.rs`）是 `rust_embed` 扫 `shell/assets/` 的
    /// **选择性**目录，没有「回退到全量目录」这回事：名字写对、`path()` 也解析
    /// 得出，但只要 `shell/assets/icons/<name>.svg` 不在目录里，`load()` 就返回
    /// `Ok(None)` ⇒ 那个位置**什么都不画**（不 panic、不打日志、单测全绿）。
    ///
    /// 2026-09-18 实机取像素抓到过一次：聊天记录行首（`ai_history_panel`）的图标槽
    /// 是空白，同一行右侧的垃圾桶却正常 —— 同一个面板、同一次渲染，差别只在
    /// 第 5 个图标（`MessageSquare`）的 svg 没随包。根因很结构性：
    /// `gpui_component::IconName` 只是**组件默认子集**（约 90 个），用到子集外的
    /// 名字得别名到全量目录 `gpui_kit_assets::IconName`，而别名只解决了「编译」，
    /// **没解决「文件在不在」**。
    ///
    /// 判据不是手工维护一张「名 → 路径」表（那会腐烂），而是反过来问全量目录：
    /// 逐个取 `path()`，凡本仓库没有该 svg 的，其变体名都不得出现在 UI 生产源码里
    /// （`IconName::X` / `LucideIcon::X`）。用取源入口 `prod()` 剥注释切测试区，
    /// 否则针会命中护栏自己的字符串（见本模块文档）。
    #[test]
    fn every_icon_the_ui_asks_for_is_shipped() {
        let icons_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("ui 目录必须有父目录（= 仓库根）")
            .join("shell")
            .join("assets")
            .join("icons");

        let mut src_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        src_dir.push("src");
        let mut files = Vec::new();
        collect_rs(&src_dir, &mut files);
        let mut sources = String::new();
        for f in &files {
            if let Ok(text) = std::fs::read_to_string(f) {
                sources.push_str(&prod(&text));
                sources.push('\n');
            }
        }

        let mut missing = Vec::new();
        for icon in gpui_kit_assets::IconName::ALL {
            let icon = *icon;
            let path = icon.path().to_string();
            let Some(file) = path.strip_prefix("icons/") else {
                continue;
            };
            if icons_dir.join(file).exists() {
                continue;
            }
            let name = format!("{icon:?}");
            for alias in ["IconName::", "LucideIcon::"] {
                if mentions(&sources, alias, &name) {
                    missing.push(format!("  {alias}{name}  →  {path}"));
                }
            }
        }

        // 第二段：`Icon::default().path("icons/file-code.svg")` 这类**直接点名资源**
        // 的写法。它压根不经过 `IconName`，上面按变体名扫的那段完全看不见它 ⇒
        // 文件缺失同样是**静默留白**。所以这里反过来扫字符串字面量：UI 源码里出现
        // 的每个 `icons/*.svg`，磁盘上都必须有同名文件。
        //
        // 2026-09-20 补：文件面板要按扩展名分派十余种图标，而 `IconName` 只是组件
        // 默认子集（文件类型图标不在其中）⇒ 只能点名路径。护栏必须跟着扩到这条通道，
        // 否则「新加了 kind → 忘了 copy svg」会一路绿到实机才发现整列图标空着。
        for path in icon_path_literals(&sources) {
            let Some(file) = path.strip_prefix("icons/") else {
                continue;
            };
            if !icons_dir.join(file).exists() {
                missing.push(format!(
                    "  \"{path}\"  →  `Icon::path(..)` 直接点名，但 shell/assets/icons/ 下没有该文件"
                ));
            }
        }

        assert!(
            missing.is_empty(),
            "这些图标名解析得出路径，但 `shell/assets/icons/` 下没有对应 svg —— \
             渲染出来是**空白**（不报错、不打日志、单测也全绿）：\n{}\n\
             修法：从 gpui-kit-assets 的 `assets/icons/` 复制同名 svg 到 `shell/assets/icons/`。",
            missing.join("\n")
        );
    }

    /// 源码里是否**以标识符的方式**点名了这个图标变体。
    ///
    /// 不能用裸 `contains`：`IconName::Circle` 会命中 `IconName::CircleCheck`，
    /// `Square` 会命中 `SquareTerminal`（竖栏真在用），`Triangle` 会命中
    /// `TriangleAlert` —— 一次就误报三条。名字后面必须不是标识符字符。
    fn mentions(sources: &str, alias: &str, name: &str) -> bool {
        let needle = format!("{alias}{name}");
        let mut from = 0usize;
        while let Some(at) = sources[from..].find(&needle) {
            let end = from + at + needle.len();
            let next_is_word = sources[end..]
                .chars()
                .next()
                .is_some_and(|c| c.is_alphanumeric() || c == '_');
            if !next_is_word {
                return true;
            }
            from = end;
        }
        false
    }

    /// 抽出源码里所有形如 `icons/<name>.svg` 的**字符串字面量**（去重、保序）。
    ///
    /// 必须按**引号**切，不能用裸 `find("icons/")` 再取剩余全部：那样会把后面无关的
    /// 代码一路吞进来（本仓在别处的扫描里踩过同样的坑）。这里只认「开引号 → 到下一个
    /// 引号为止」，且要求以 `.svg` 结尾 —— 于是 `strip_prefix("icons/")` 这种把前缀
    /// 当字面量切片的写法（本文件自己就在用）天然不会误报。
    ///
    /// `sources` 已经过 `prod()`（剥注释 + 切掉测试区），所以文档注释里举例的路径
    /// 和护栏自己的断言文案都不会被算进来。
    fn icon_path_literals(sources: &str) -> Vec<String> {
        let mut found: Vec<String> = Vec::new();
        let mut from = 0usize;
        while let Some(at) = sources[from..].find("\"icons/") {
            let start = from + at + 1; // 跳过开引号
            let Some(end_rel) = sources[start..].find('"') else {
                break;
            };
            let end = start + end_rel;
            let path = &sources[start..end];
            if path.ends_with(".svg") && !found.iter().any(|p| p == path) {
                found.push(path.to_string());
            }
            from = end;
        }
        found
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
}
