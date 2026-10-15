//! Agent 轨迹：UI 渲染「工具调用轨迹块」所需的**纯数据**（v1.1 §3.9）。
//!
//! 为什么单独一个模块而不是直接让 UI 读 `AgentStep`：
//! 1. `AgentStep` 是循环的**内部状态**（含 `pending_calls` 这类协议细节），
//!    而 UI 只需要「一行显示什么」。把投影固化在这里，UI 就不再依赖循环的内部形状；
//! 2. 本模块零 UI 依赖、零网络 —— 轨迹的状态推演与文案可以脱离 GPUI 单测（AC-47）；
//! 3. 轨迹的「增删会改变消息行高」是 UI 的硬约束（§3.9：必须与消息一起
//!    `remeasure_items`），所以行的**数量**与**稳定性**必须可断言 ——
//!    `TraceRow` 一旦生成就不再改身份（`call_id` 恒定），只更新状态与明细。

use serde::{Deserialize, Serialize};

use crate::agent::ToolStatus;

/// 轨迹行的状态（UI 着色与文案的唯一依据）。
///
/// 与 [`ToolStatus`] 刻意不是同一个类型：`ToolStatus` 里的 `Failed(String)`
/// 等变体带着**给模型看**的原始说明，而 UI 要的是「警告色 / 危险色 / 弱化色」
/// 这类呈现分类。把两者混成一个类型，会让「模型上下文里的文案」一变，
/// 界面的颜色也跟着变。
///
/// `Serialize`/`Deserialize` 只为**定时任务的运行记录**（`RunRecord::session`）
/// 服务：无人值守跑完的轨迹要能跟着 jsonl 落盘，重启后点开会话还能看到。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TraceState {
    /// 正在执行（尚未收到结果）
    Running,
    /// 成功完成
    Ok,
    /// 完成但发现异常（命中日志、负载超阈值）—— 轨迹上走 warning 色
    Anomaly,
    /// 部分成功（多台里有些失败）
    Partial,
    /// 执行失败
    Failed,
    /// 参数错误（模型自纠依据）
    BadArgs,
    /// 风险拦截（Blocked，永不执行）
    Blocked,
    /// 策略禁用（未开启状态变更等）
    Disabled,
    /// 等待用户确认（轨迹上走 accent 色）
    Waiting,
    /// 用户拒绝
    Rejected,
    /// 被停止（用户点了停止按钮时仍未收口的那些）
    Stopped,
}

impl TraceState {
    /// 稳定的状态标签（写进扫描型护栏与 debug 选择器用，不随语言变化）。
    pub fn tag(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Ok => "ok",
            Self::Anomaly => "anomaly",
            Self::Partial => "partial",
            Self::Failed => "failed",
            Self::BadArgs => "bad-args",
            Self::Blocked => "blocked",
            Self::Disabled => "disabled",
            Self::Waiting => "waiting",
            Self::Rejected => "rejected",
            Self::Stopped => "stopped",
        }
    }

    /// 用户看得懂的一句话。
    pub fn label(self) -> String {
        use crate::i18n::ts;
        match self {
            Self::Running => ts("执行中…".into(), "running…".into()),
            Self::Ok => ts("完成".into(), "done".into()),
            Self::Anomaly => ts("发现异常".into(), "anomaly found".into()),
            Self::Partial => ts("部分失败".into(), "partially failed".into()),
            Self::Failed => ts("失败".into(), "failed".into()),
            Self::BadArgs => ts("参数错误".into(), "bad arguments".into()),
            Self::Blocked => ts("已拦截".into(), "blocked".into()),
            Self::Disabled => ts("已禁用".into(), "disabled".into()),
            Self::Waiting => ts("等待确认".into(), "awaiting confirmation".into()),
            Self::Rejected => ts("已忽略".into(), "dismissed".into()),
            Self::Stopped => ts("已停止".into(), "stopped".into()),
        }
    }

    /// 是否还需要用户/执行侧推进（`Running` 与 `Waiting` 是仅有的两个「未定」态）。
    pub fn is_settled(self) -> bool {
        !matches!(self, Self::Running | Self::Waiting)
    }

    /// 这一行是否代表「出事了」（UI 用 warning/danger 色）。
    pub fn is_bad(self) -> bool {
        matches!(
            self,
            Self::Anomaly
                | Self::Partial
                | Self::Failed
                | Self::BadArgs
                | Self::Blocked
                | Self::Disabled
        )
    }

    /// 从循环的执行状态投影出来。
    ///
    /// `anomaly` 由调用方判定（只有它知道这次巡检发现了什么）——
    /// `ToolStatus` 本身不携带 finding，硬塞进去会让循环状态机沾上业务语义。
    pub fn of(status: &ToolStatus, anomaly: bool) -> Self {
        match status {
            ToolStatus::Pending => Self::Running,
            ToolStatus::Ok => {
                if anomaly {
                    Self::Anomaly
                } else {
                    Self::Ok
                }
            }
            // 「成功但有异常」只在 Ok 分支成立；失败/拦截一律按各自的严重态呈现，
            // 否则「某台机器连不上」会被画成 warning，用户会以为只是有点吵。
            ToolStatus::Failed(_) => Self::Failed,
            ToolStatus::BadArgs(_) => Self::BadArgs,
            ToolStatus::Blocked(_) => Self::Blocked,
            ToolStatus::AwaitingConfirm(_) => Self::Waiting,
            ToolStatus::Rejected => Self::Rejected,
            ToolStatus::Disabled(_) => Self::Disabled,
        }
    }
}

/// 轨迹块里的一行。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceRow {
    /// 从 1 开始的步号（与 `AgentStep::index` 对齐）
    pub step: u8,
    /// 工具调用 id（行的**稳定身份**：更新状态时按它定位，不按序号）
    pub call_id: String,
    /// 稳定的工具名（`inspect_load` 等；写进 debug 选择器）
    pub tool: String,
    /// 人类可读的调用摘要（参数，renderer 不做解析）
    pub summary: String,
    pub state: TraceState,
    /// 结果的一句话说明（来自 `ToolStatus` 的可读版本）
    pub note: String,
    pub elapsed_ms: u64,
    /// 明细：每台机器的原始输出（已截断）。折叠时只显示前若干行。
    pub detail: String,
}

/// 轨迹明细在 UI 上的截断口径（字节）。
///
/// 与 §3.9「超过 64 KiB 的按折叠口径处理」一致：明细是**给人看**的，
/// 不必整段进内存渲染。取 64 KiB 而不是更小，是因为「一次 30 台机器的
/// `/proc` 摘要」本来就在这个量级 —— 截太小会让用户看不到最后几台。
pub const MAX_TRACE_DETAIL_BYTES: usize = 64 * 1024;

/// 明细**送进渲染**的上限（字节）。
///
/// 为什么要比 [`MAX_TRACE_DETAIL_BYTES`] 更小：官方 `Collapsible` 的「收起」是
/// `opacity(0)` + 零高度 —— 内容元素**仍然会被构建和布局**。消息列表是虚拟列表，
/// 每帧都会重排这一行，于是「每条轨迹各挂 64 KiB 文本」会变成每帧几十万字符的
/// 文本布局。8 KiB 与回注给模型的上下文同量级，正好够看清异常（异常段本来就
/// 排在明细最前面，见 `inspect::trace_detail`）。
pub const MAX_TRACE_DETAIL_RENDER_BYTES: usize = 8 * 1024;

/// 渲染口径必须严格小于存储口径。写成编译期断言而不是用例里的 `assert!`：
/// 两个都是常量，用例里的断言会被 clippy 判成「断言常量」而拦下，
/// 而这条约束本该在编译期就成立。
const _: () = assert!(MAX_TRACE_DETAIL_RENDER_BYTES < MAX_TRACE_DETAIL_BYTES);

impl TraceRow {
    pub fn new(step: u8, call_id: impl Into<String>, tool: impl Into<String>) -> Self {
        Self {
            step,
            call_id: call_id.into(),
            tool: tool.into(),
            summary: String::new(),
            state: TraceState::Running,
            note: String::new(),
            elapsed_ms: 0,
            detail: String::new(),
        }
    }

    /// 一行的显示文本（`▸ 🔧 inspect_load  prod-01 ✅ 3/3 完成 · 1.8s`）。
    ///
    /// 图标由 UI 侧按 `tool`/`state` 决定（core 不引入图标资产），这里只给文字。
    pub fn one_line(&self) -> String {
        let mut s = format!("{}  {}", self.tool, self.summary);
        let note = self.note.trim();
        if !note.is_empty() {
            s.push_str("  ");
            s.push_str(note);
        } else {
            s.push_str("  ");
            s.push_str(&self.state.label());
        }
        if self.elapsed_ms > 0 {
            s.push_str(&format!(" · {:.1}s", self.elapsed_ms as f64 / 1000.0));
        }
        s.trim_end().to_string()
    }

    /// 明细截断：按字符边界切，附省略标记（存储口径，64 KiB）。
    pub fn truncated_detail(&self) -> String {
        truncate_detail(&self.detail)
    }

    /// 送进渲染的明细（[`MAX_TRACE_DETAIL_RENDER_BYTES`]，比存储口径更紧）。
    ///
    /// 被截断时明确写出「还有多少字节」——用户在界面上看不到的内容，
    /// 至少要让他知道它存在，而不是以为「明细就这么点」。
    pub fn render_detail(&self) -> String {
        truncate_to(&self.detail, MAX_TRACE_DETAIL_RENDER_BYTES)
    }

    /// 更新状态（按 `call_id` 定位由调用方负责；本方法只改自己）。
    pub fn settle(&mut self, state: TraceState, note: impl Into<String>, elapsed_ms: u64) {
        self.state = state;
        self.note = note.into();
        self.elapsed_ms = elapsed_ms;
    }
}

/// 明细文本截断到 [`MAX_TRACE_DETAIL_BYTES`]（按字符边界，附省略字节数）。
pub fn truncate_detail(text: &str) -> String {
    truncate_to(text, MAX_TRACE_DETAIL_BYTES)
}

/// 按字节上限截断（落在字符边界上，附省略字节数）。
pub fn truncate_to(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut cut = max_bytes;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    let omitted = text.len() - cut;
    format!("{}\n… (省略 {omitted} 字节) …", &text[..cut])
}

/// 把一串行按 `call_id` 合并（新状态覆盖旧的），保持**首次出现顺序**。
///
/// 顺序稳定性是硬要求：轨迹行会改变消息高度，顺序抖动等于每帧都在重排。
/// 用 `Vec` + 线性查找而不是 `HashMap` 是无意的也是刻意的 —— 单步内行数
/// 上限只有 [`crate::agent::MAX_CALLS_PER_STEP`]，线性查找更快也更好读。
pub fn upsert(rows: &mut Vec<TraceRow>, row: TraceRow) -> usize {
    match rows.iter().position(|r| r.call_id == row.call_id) {
        Some(ix) => {
            // 保留原有的步号与摘要（模型重发同一 id 时不该把摘要换掉），
            // 只更新会变的部分。
            let prev = &mut rows[ix];
            let step = prev.step;
            let tool = prev.tool.clone();
            let summary = prev.summary.clone();
            *prev = row;
            prev.step = step;
            if prev.tool.is_empty() {
                prev.tool = tool;
            }
            if prev.summary.is_empty() {
                prev.summary = summary;
            }
            ix
        }
        None => {
            rows.push(row);
            rows.len() - 1
        }
    }
}

/// 未收口的行标记为已停止（用户点「停止」时调用）。
///
/// 返回被标记的条数 —— 用例据此断言「部分轨迹保留且标记为已停止」（AC-7）。
pub fn mark_stopped(rows: &mut [TraceRow]) -> usize {
    let mut n = 0;
    for r in rows.iter_mut() {
        if !r.state.is_settled() {
            r.state = TraceState::Stopped;
            if r.note.trim().is_empty() {
                r.note = TraceState::Stopped.label();
            }
            n += 1;
        }
    }
    n
}

/// 轨迹块的汇总（折叠时显示：「3 次调用 · 1 异常 · 2.4s」）。
pub fn summary_line(rows: &[TraceRow]) -> String {
    use crate::i18n::ts;
    if rows.is_empty() {
        return String::new();
    }
    let total = rows.len();
    let bad = rows.iter().filter(|r| r.state.is_bad()).count();
    let ms: u64 = rows.iter().map(|r| r.elapsed_ms).sum();
    let secs = ms as f64 / 1000.0;
    if bad > 0 {
        ts(
            format!("{total} 次调用 · {bad} 项异常 · {secs:.1}s"),
            format!("{total} calls · {bad} flagged · {secs:.1}s"),
        )
    } else {
        ts(
            format!("{total} 次调用 · {secs:.1}s"),
            format!("{total} calls · {secs:.1}s"),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, state: TraceState) -> TraceRow {
        let mut r = TraceRow::new(1, id, "inspect_load");
        r.summary = "prod-01".into();
        r.state = state;
        r.elapsed_ms = 900;
        r
    }

    /// 十一个状态各有标签与稳定 tag；漏一个的表现是轨迹行上出现空白。
    #[test]
    fn every_state_has_a_label_and_a_stable_tag() {
        let states = [
            TraceState::Running,
            TraceState::Ok,
            TraceState::Anomaly,
            TraceState::Partial,
            TraceState::Failed,
            TraceState::BadArgs,
            TraceState::Blocked,
            TraceState::Disabled,
            TraceState::Waiting,
            TraceState::Rejected,
            TraceState::Stopped,
        ];
        let mut tags = std::collections::HashSet::new();
        for s in states {
            assert!(!s.label().is_empty(), "{s:?} 没有可读文案");
            assert!(!s.tag().is_empty(), "{s:?} 没有稳定 tag");
            assert!(tags.insert(s.tag()), "{s:?} 的 tag 与别的状态撞车");
        }
        // 「未定」只有两个：其余都要能被 `mark_stopped` 之外的路径收口
        assert!(!TraceState::Running.is_settled());
        assert!(!TraceState::Waiting.is_settled());
        assert!(TraceState::Ok.is_settled());
        assert!(TraceState::Stopped.is_settled());
    }

    /// `ToolStatus` → `TraceState` 的投影：异常只在「成功」分支上升级。
    #[test]
    fn state_projection_keeps_failures_severe() {
        assert_eq!(
            TraceState::of(&ToolStatus::Ok, false),
            TraceState::Ok,
            "成功且无异常"
        );
        assert_eq!(
            TraceState::of(&ToolStatus::Ok, true),
            TraceState::Anomaly,
            "成功但有异常要走 warning"
        );
        // 有异常标记也不能把失败/拦截「洗」成警告色
        assert_eq!(
            TraceState::of(&ToolStatus::Failed("连不上".into()), true),
            TraceState::Failed
        );
        assert_eq!(
            TraceState::of(&ToolStatus::Blocked("rm -rf /".into()), true),
            TraceState::Blocked
        );
        assert_eq!(
            TraceState::of(&ToolStatus::AwaitingConfirm("确认".into()), false),
            TraceState::Waiting
        );
        assert_eq!(
            TraceState::of(&ToolStatus::Pending, false),
            TraceState::Running
        );
        assert_eq!(
            TraceState::of(&ToolStatus::BadArgs("缺 path".into()), false),
            TraceState::BadArgs
        );
        assert_eq!(
            TraceState::of(&ToolStatus::Rejected, false),
            TraceState::Rejected
        );
        assert_eq!(
            TraceState::of(&ToolStatus::Disabled("未开启".into()), false),
            TraceState::Disabled
        );
    }

    /// 行的身份是 `call_id`：更新只改状态，**不追加新行**（否则轨迹会越用越长，
    /// 而每多一行都要重测一次消息高度）。
    #[test]
    fn upsert_updates_in_place_and_keeps_the_first_seen_order() {
        let mut rows = Vec::new();
        upsert(&mut rows, row("a", TraceState::Running));
        upsert(&mut rows, row("b", TraceState::Running));
        assert_eq!(rows.len(), 2);

        let mut done = row("a", TraceState::Ok);
        done.note = "3/3 完成".into();
        let ix = upsert(&mut rows, done);
        assert_eq!(ix, 0, "已存在的行必须原地更新");
        assert_eq!(rows.len(), 2, "不得追加新行");
        assert_eq!(rows[0].state, TraceState::Ok);
        assert_eq!(rows[0].note, "3/3 完成");
        // 顺序仍是 a、b
        assert_eq!(rows[0].call_id, "a");
        assert_eq!(rows[1].call_id, "b");

        // 新 id 追加在末尾
        upsert(&mut rows, row("c", TraceState::Running));
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[2].call_id, "c");
    }

    /// 重发同一 id 时，摘要与步号不该被空值覆盖（模型会把 assistant 消息
    /// 原样重放，摘要为空会让轨迹行只剩工具名）。
    #[test]
    fn upsert_does_not_blank_out_the_summary() {
        let mut rows = Vec::new();
        let mut first = row("a", TraceState::Running);
        first.summary = "prod-01, prod-02".into();
        upsert(&mut rows, first);
        // 同一 id 的裸更新
        upsert(&mut rows, TraceRow::new(1, "a", "inspect_load"));
        assert_eq!(rows[0].summary, "prod-01, prod-02", "摘要被清空了");
        assert_eq!(rows[0].tool, "inspect_load");
    }

    /// AC-7：停止后未收口的行标记为已停止，已收口的**一个字都不动**。
    #[test]
    fn mark_stopped_only_touches_unsettled_rows() {
        let mut rows = vec![
            row("a", TraceState::Ok),
            row("b", TraceState::Running),
            row("c", TraceState::Waiting),
            row("d", TraceState::Blocked),
        ];
        let n = mark_stopped(&mut rows);
        assert_eq!(n, 2, "只有 Running 与 Waiting 需要标记");
        assert_eq!(rows[0].state, TraceState::Ok);
        assert_eq!(rows[1].state, TraceState::Stopped);
        assert_eq!(rows[2].state, TraceState::Stopped);
        assert_eq!(rows[3].state, TraceState::Blocked);
        // 已收口的备注不被改写
        assert_eq!(mark_stopped(&mut rows), 0, "再标记一次不该有变化");
    }

    /// 一行文本要同时带上工具名、目标与状态，且耗时为 0 时不显示「0.0s」。
    #[test]
    fn one_line_carries_tool_target_and_status() {
        let mut r = row("a", TraceState::Ok);
        r.note = "3/3 完成".into();
        let s = r.one_line();
        assert!(s.contains("inspect_load"), "缺工具名：{s}");
        assert!(s.contains("prod-01"), "缺目标：{s}");
        assert!(s.contains("3/3 完成"), "缺状态：{s}");
        assert!(s.contains("0.9s"), "缺耗时：{s}");

        let mut quick = row("b", TraceState::Ok);
        quick.elapsed_ms = 0;
        assert!(
            !quick.one_line().contains("0.0s"),
            "没有耗时时不该显示 0.0s：{}",
            quick.one_line()
        );
        // 没有 note 时回退到状态文案
        assert!(quick.one_line().contains(&TraceState::Ok.label()));
    }

    /// 明细截断必须落在字符边界（CJK 被切开会渲染成替换字符）。
    #[test]
    fn detail_truncation_is_char_safe() {
        let small = "ok".to_string();
        assert_eq!(truncate_detail(&small), "ok");

        let big = "汉".repeat(MAX_TRACE_DETAIL_BYTES); // 3 字节/字，远超上限
        let out = truncate_detail(&big);
        assert!(out.len() < big.len());
        assert!(out.contains("省略"), "截断要给出省略标记");
        assert!(!out.contains('\u{FFFD}'), "截断落在了字符中间");
        assert!(
            out.len() <= MAX_TRACE_DETAIL_BYTES + 32,
            "截断后不该还接近原文长度"
        );
    }

    /// **送渲染**的明细比存储口径更紧，且自带省略标记。
    ///
    /// 这条钉住的是性能而不是功能：官方 `Collapsible` 收起时内容照样被布局，
    /// 若渲染口径与存储口径一样是 64 KiB，一条轨迹就能让每帧多布局几万字符。
    #[test]
    fn render_detail_is_tighter_than_the_stored_one() {
        // 「渲染口径 < 存储口径」由模块级的 `const _: () = assert!(..)` 在编译期保证，
        // 这里只验行为。
        let mut r = TraceRow::new(1, "c1", "inspect_load");
        // 造一段比渲染上限长、但仍短于存储上限的明细
        r.detail = "a".repeat(MAX_TRACE_DETAIL_RENDER_BYTES + 100);
        assert_eq!(
            r.truncated_detail().len(),
            r.detail.len(),
            "存储口径下这段不该被截（否则测的不是渲染口径）"
        );
        let rendered = r.render_detail();
        assert!(rendered.len() < r.detail.len(), "渲染口径必须截断");
        assert!(rendered.contains("省略"), "截断了就要告诉用户：{rendered}");
        // 短明细原样返回
        let mut short = TraceRow::new(1, "c2", "inspect_load");
        short.detail = "异常明细（1 项）：\n· [警告] prod-01 — 内存 94%".into();
        assert_eq!(short.render_detail(), short.detail);
    }

    /// 汇总行要能一眼看出「几次调用、有没有异常」。
    #[test]
    fn summary_line_counts_calls_and_flags() {
        assert_eq!(summary_line(&[]), "", "没有轨迹时不该有汇总行");
        let rows = vec![
            row("a", TraceState::Ok),
            row("b", TraceState::Anomaly),
            row("c", TraceState::Blocked),
        ];
        let s = summary_line(&rows);
        assert!(s.contains('3'), "缺调用总数：{s}");
        assert!(s.contains('2'), "缺异常条数：{s}");
        let clean = vec![row("a", TraceState::Ok)];
        let s2 = summary_line(&clean);
        assert!(!s2.contains("异常"), "没有异常时不该提异常：{s2}");
    }
}
