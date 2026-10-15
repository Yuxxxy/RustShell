//! Shell integration（OSC 133 / 633 / 7）的**感知**层。
//!
//! 这一模块只做一件事：**听懂远端 shell 已经说出来的话**。
//! 它不发任何字节给远端、不改用户环境、不需要事先配置 ——
//! 远端（或用户自己的 `starship` / `oh-my-zsh` / VS Code 集成）如果发了这些序列，
//! 我们就把里面的语义提取出来；没发就什么也不发生。
//!
//! # 为什么需要它
//!
//! 应用侧原本**看不见用户正在输入的那一行**：终端是纯 PTY 字节流，
//! 网格里只有渲染结果，没有"prompt 到哪结束 / 命令是什么 / 退出码多少 / cwd 在哪"
//! 这四件事。命令补全、命令块、跳转上一提示符，全部建在这四件事之上。
//!
//! 拿到它们有两条路：**启发式猜**（行首空白宽度之类）和**让 shell 直接说**。
//! 后者就是本模块。业界（VS Code / WezTerm / iTerm2 / kitty）走的是同一条路。
//!
//! # 协议
//!
//! | 序列 | 含义 |
//! |---|---|
//! | `OSC 133 ; A` | prompt 开始 |
//! | `OSC 133 ; B` | prompt 结束 / 命令输入开始 |
//! | `OSC 133 ; C` | 命令开始执行 |
//! | `OSC 133 ; D ; <exit>` | 命令结束，带退出码 |
//! | `OSC 633 ; …` | VS Code 的扩展版，语义兼容 133 |
//! | `OSC 7 ; file://<host>/<path>` | 上报当前工作目录 |
//!
//! 注：`OSC 7` 在本仓是**新识别**的（此前完全未处理）；
//! `OSC 133/633` 同样此前完全未处理。vte 对未认领的 OSC 只打一条 `debug!` 日志、
//! **不回调**，所以"什么都不做、等 alacritty 透传"这条路是不通的 ——
//! 必须自己包一层 `Perform`（见 [`crate::terminal`]）。
//!
//! # 与 `D` 段退出码的关系
//!
//! 退出码在 OSC 里是**文本**，`OSC 133;D;1` 与 `OSC 133;D` 必须区分：
//! 前者是"退出码 1"，后者是"命令结束但没给退出码"。两者都解析成 `None`
//! 会让"上一条命令失败了吗"永远答不上来。

/// 一条命令的生命周期标记。
///
/// 取自 `OSC 133` / `OSC 633`。**顺序是协议保证的**（A → B → C → D），
/// 但实现上不依赖顺序（远端可能只发其中几个）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptMark {
    /// `A`：prompt 即将开始。
    PromptStart,
    /// `B`：prompt 结束，用户开始输入命令。
    CommandStart,
    /// `C`：命令开始执行（用户按了回车）。
    CommandExecuted,
    /// `D`：命令结束。
    ///
    /// `exit_code` 为 `None` 表示远端**没给**退出码（`OSC 133;D`），
    /// 与"退出码是 0"是两回事。
    CommandFinished { exit_code: Option<i32> },
}

/// 从 `OSC 133` / `OSC 633` 解析出的语义事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellIntegration {
    /// 命令生命周期标记。
    Mark(PromptMark),
    /// 当前工作目录变更（`OSC 7`）。路径是**远端**的路径。
    Cwd(String),
    /// 当前命令的只读提示文本（`OSC 633;E`，VS Code 扩展）。
    ///
    /// 133 里没有这一段；有它就能拿到**未经网格推断**的准确命令行。
    CommandLine(String),
}

/// 解析一组 OSC 参数（vte 已按 `;` 切好）。
///
/// 返回 `None` 表示这条 OSC 与本模块无关（调用方应原样交给下层处理）。
///
/// # 参数切分约定
///
/// vte 的切分有个容易踩的边界：空参数会被**跳过**。
/// `OSC 133;D;;host` 切出来是 `["133", "D", "host"]` 而不是
/// `["133","D","","host"]`（见 vte `action_osc_put_param` 只对非空段推进）。
/// 所以 `params[2]` **不能**无条件当成退出码 —— 必须靠 `parse_number` 失败来区分
/// "空段"与"主机名"。同理 `OSC 7;file://host/path` 的 `params[1]` 才是 URI。
///
/// # 与 OSC 8 的冲突
///
/// `OSC 8`（超链接）也是 `8` 打头，且 `OSC 8;;` 开头的段落切法与本模块无关，
/// 但**绝不能**把它当 cwd —— 本函数只认 `params[0] == "7"` 这一种，不按后缀匹配。
pub fn parse_osc(params: &[&[u8]]) -> Option<ShellIntegration> {
    let head = params.first()?;

    // OSC 7：`file://<host>/<path>`。host 通常为空或机器名。
    if head == b"7" {
        let uri = std::str::from_utf8(params.get(1)?).ok()?;
        return parse_osc7_uri(uri).map(ShellIntegration::Cwd);
    }

    // OSC 133 / 633：命令生命周期。
    if head == b"133" || head == b"633" {
        let action = params.get(1)?;
        return match *action {
            b"A" => Some(ShellIntegration::Mark(PromptMark::PromptStart)),
            b"B" => Some(ShellIntegration::Mark(PromptMark::CommandStart)),
            b"C" => Some(ShellIntegration::Mark(PromptMark::CommandExecuted)),
            b"D" => {
                // 退出码可缺；见本函数文档里"空参数会被跳过"那段。
                // 只认「纯数字」—— `parse_number` 的 `+` 前缀是 vte 的通用语法，
                // 对退出码来说纯数字才是合法的，`+1` 这种不认（宁可报 None）。
                let exit_code = params.get(2).and_then(|p| parse_exit_code(p));
                Some(ShellIntegration::Mark(PromptMark::CommandFinished {
                    exit_code,
                }))
            }
            b"E" => {
                let line = std::str::from_utf8(params.get(2)?).ok()?;
                Some(ShellIntegration::CommandLine(line.to_string()))
            }
            _ => None,
        };
    }

    None
}

/// 解析 `OSC 7` 的 URI，取出路径。
///
/// 形式是 `file://<host>/<path>`（host 可为空，此时是 `file:///path`）。
/// 不做 URL 解码：远端上的路径极少带百分号转义，而**多解一层**的风险更大
/// （`%20` 到底该不该变空格，各家 shell 的实现并不一致）；
/// 万一遇到，交给下游按字面用即可 —— 宁可不还原，也不要还原错。
pub fn parse_osc7_uri(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("file://")?;
    // 第一个 `/` 之后才是路径；之前的整段是 host。
    // 只有 host、没有路径（`file://hostonly`）⇒ 不是一条有效的 cwd 上报。
    let path = &rest[rest.find('/')?..];
    if path.is_empty() {
        return None;
    }
    Some(path.to_string())
}

/// 解析退出码：只接受**纯 ASCII 十进制**（可带负号）。
///
/// 不用 `str::parse` 直接吞下全部输入，是因为 vte 的 `parse_number` 认 `+` 号，
/// 而过一下 `+` 号会让 `OSC 133;D;+1` 被当成合法退出码 —— 协议里没有这种写法。
fn parse_exit_code(raw: &[u8]) -> Option<i32> {
    let s = std::str::from_utf8(raw).ok()?;
    let s = s.strip_prefix('-').map_or(("", s), |rest| ("-", rest));
    let (sign, digits) = s;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: i32 = digits.parse().ok()?;
    Some(if sign == "-" { -n } else { n })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 参数切分辅助：与 vte 的行为保持一致（**空段被跳过**）。
    ///
    /// 这里刻意不写成 `split(';')`：vte 的 `action_osc_put_param` 在遇到分隔符时
    /// 才推进参数下标，而空段不会产生新的下标区间，效果就是空段被丢弃。
    /// 测试要照这个口径构造输入，否则测的是"我以为的 vte"而不是"真的 vte"。
    fn osc(raw: &str) -> Vec<&[u8]> {
        raw.split(';')
            .filter(|s| !s.is_empty())
            .map(|s| s.as_bytes())
            .collect()
    }

    #[test]
    fn parses_prompt_lifecycle_marks() {
        for (raw, want) in [
            ("133;A", PromptMark::PromptStart),
            ("133;B", PromptMark::CommandStart),
            ("133;C", PromptMark::CommandExecuted),
        ] {
            let got = parse_osc(&osc(raw));
            assert_eq!(got, Some(ShellIntegration::Mark(want)), "raw={raw}");
        }
    }

    /// `133;D` 与 `133;D;0` 必须区分：前者是"没给退出码"，后者是"成功"。
    #[test]
    fn distinguishes_absent_exit_code_from_zero() {
        assert_eq!(
            parse_osc(&osc("133;D")),
            Some(ShellIntegration::Mark(PromptMark::CommandFinished {
                exit_code: None
            })),
            "没有退出码段 ⇒ None，不能默认成 0"
        );
        assert_eq!(
            parse_osc(&osc("133;D;0")),
            Some(ShellIntegration::Mark(PromptMark::CommandFinished {
                exit_code: Some(0)
            }))
        );
        assert_eq!(
            parse_osc(&osc("133;D;1")),
            Some(ShellIntegration::Mark(PromptMark::CommandFinished {
                exit_code: Some(1)
            }))
        );
        assert_eq!(
            parse_osc(&osc("133;D;130")),
            Some(ShellIntegration::Mark(PromptMark::CommandFinished {
                exit_code: Some(130)
            }))
        );
    }

    /// `133;D;;host` 里的 host 段**不是**退出码。
    ///
    /// 这是最容易写错的一条：空段被 vte 跳过后，`params[2]` 就是 host，
    /// 若直接 `parse` 会拿到 `None` 而看起来"正常"，但若实现里写了
    /// `params[2].parse().unwrap_or(0)`，退出码就会被静默填成 0。
    #[test]
    fn empty_exit_code_slot_is_not_a_number() {
        assert_eq!(
            parse_osc(&osc("133;D;;127.0.0.1")),
            Some(ShellIntegration::Mark(PromptMark::CommandFinished {
                exit_code: None
            })),
            "空段后的 host 不能被当成退出码"
        );
        // 带 `+` 号不是合法退出码写法（vte 的 parse_number 会认，我们不能认）
        assert_eq!(
            parse_osc(&osc("133;D;+3")),
            Some(ShellIntegration::Mark(PromptMark::CommandFinished {
                exit_code: None
            })),
            "+3 不是协议里的退出码写法"
        );
    }

    /// OSC 633 与 133 语义兼容（VS Code 扩展版）。
    #[test]
    fn osc_633_is_treated_like_133() {
        assert_eq!(
            parse_osc(&osc("633;A")),
            Some(ShellIntegration::Mark(PromptMark::PromptStart))
        );
        assert_eq!(
            parse_osc(&osc("633;D;2")),
            Some(ShellIntegration::Mark(PromptMark::CommandFinished {
                exit_code: Some(2)
            }))
        );
    }

    /// `633;E;<cmdline>` 给出准确的命令行。
    #[test]
    fn osc_633_e_carries_the_command_line() {
        assert_eq!(
            parse_osc(&osc("633;E;git commit -m 'x'")),
            Some(ShellIntegration::CommandLine("git commit -m 'x'".into()))
        );
    }

    /// OSC 7 的三种形态。
    #[test]
    fn parses_cwd_from_osc7() {
        // 常见形态：host 为空
        assert_eq!(
            parse_osc(&osc("7;file:///home/deploy")),
            Some(ShellIntegration::Cwd("/home/deploy".into()))
        );
        // 带 host（iTerm2 / kitty 会带机器名）
        assert_eq!(
            parse_osc(&osc("7;file://myhost/var/log")),
            Some(ShellIntegration::Cwd("/var/log".into()))
        );
        // 带百分号转义：按字面保留，不做 URL 解码（见 parse_osc7_uri 文档）
        assert_eq!(
            parse_osc(&osc("7;file:///tmp/a%20b")),
            Some(ShellIntegration::Cwd("/tmp/a%20b".into()))
        );
    }

    /// 无关的 OSC 必须返回 `None`，交给下层（超链接、颜色、剪贴板…）。
    #[test]
    fn unrelated_osc_returns_none() {
        for raw in [
            "0;window title",
            "2;title",
            "8;;https://example.com",
            "52;c;aGVsbG8=",
            "4;1;rgb:00/00/00",
            "133",   // 只有号、没有动作
            "133;Z", // 未知动作
            "7",     // 只有号、没有 URI
            "7;notauri",
            "7;file://hostonly", // 有 host 无路径
        ] {
            assert_eq!(parse_osc(&osc(raw)), None, "raw={raw} 不该被认领");
        }
    }

    /// 空输入与只有分隔符的输入不能 panic。
    #[test]
    fn empty_input_is_safe() {
        assert_eq!(parse_osc(&[]), None);
        assert_eq!(parse_osc(&[b""]), None);
    }

    /// 退出码解析的边界。
    #[test]
    fn exit_code_parser_rejects_non_numbers() {
        assert_eq!(parse_exit_code(b"0"), Some(0));
        assert_eq!(parse_exit_code(b"255"), Some(255));
        assert_eq!(parse_exit_code(b"-1"), Some(-1));
        assert_eq!(parse_exit_code(b""), None);
        assert_eq!(parse_exit_code(b"abc"), None);
        assert_eq!(parse_exit_code(b"1a"), None);
        assert_eq!(parse_exit_code(b"+1"), None);
        assert_eq!(parse_exit_code(b"1.5"), None);
        // 溢出不是"大退出码"，是脏数据
        assert_eq!(parse_exit_code(b"99999999999999999999"), None);
    }

    /// cwd URI 解析的边界。
    #[test]
    fn cwd_uri_requires_a_path() {
        // 根目录 `/` 是**合法**的 cwd：`cd /` 之后 shell 就上报它。
        // （`file:///` 去掉 `file://` 剩 `/`，第一个 `/` 就是路径本身。）
        assert_eq!(
            parse_osc7_uri("file:///"),
            Some("/".into()),
            "根目录是合法 cwd"
        );
        assert_eq!(parse_osc7_uri("file://"), None, "只有 scheme、连路径都没有");
        assert_eq!(parse_osc7_uri("http://x/y"), None, "不是 file:// 不认");
        assert_eq!(parse_osc7_uri("file:///a/b/"), Some("/a/b/".into()));
        assert_eq!(parse_osc7_uri("file://hostonly"), None, "有 host 无路径");
    }
}
