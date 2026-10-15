//! 断线自动重连的判定与退避（F-4）。
//!
//! 这里只有**纯函数与常量**：什么时候该重试、下一次等多久。谁在什么线程上等、
//! 账本存在哪，都是宿主（`ui`）的事 —— core 不认识标签，也不认识时钟。
//!
//! 为什么退避要收在 core：它是「风暴防护」这条安全属性的唯一定义处。
//! 分散到调用点的话，`interval × 2^(n-1)` 与 60s 上限会在某处漏掉一项，
//! 而症状是「某个入口疯狂重连」——线上最难查的一类问题。

/// 重连间隔默认值（秒）。首次重试等一个间隔，之后每次翻倍。
pub const DEFAULT_INTERVAL_SECS: u32 = 5;

/// 退避上限（秒）。到顶后不再翻倍，按这个间隔一直试到次数用尽。
///
/// 必须有上限：没有它，第 5 次就得等 80s、第 7 次 320s —— 用户面前的
/// 「正在重连」会变成一块停止跳动的墓碑；有上限才保证「还在动」。
pub const BACKOFF_CAP_SECS: u64 = 60;

/// 间隔下界（秒）。0 会让退避序列退化成「立刻、立刻、立刻」。
pub const MIN_INTERVAL_SECS: u32 = 1;

/// 间隔上界（秒）。10 分钟已经很接近「算了别试了」，再大不如关开关。
pub const MAX_INTERVAL_SECS: u32 = 600;

/// 把设置里的间隔钳到合法区间（越界值一律夹边，不报错：这是设置项，
/// 不是用户当场的输入）。
pub fn clamp_interval(secs: u32) -> u32 {
    secs.clamp(MIN_INTERVAL_SECS, MAX_INTERVAL_SECS)
}

/// 第 `attempt` 次重试该等多久（秒）。
///
/// `attempt` **从 1 起**：第 1 次重试等 `interval`，第 2 次 2×，第 3 次 4×……
/// 封顶 [`BACKOFF_CAP_SECS`]。
///
/// `attempt = 0` 按第 1 次算（调用方不该传 0，但传了也不该得到「等 0 秒」）——
/// 一次「立刻重试」正是退避要防的东西，所以这里**夹到 1 而不是返回 0**。
/// 用饱和移位而非 `pow`：`attempt` 很大时不 panic、不溢出。
pub fn backoff_secs(interval_secs: u32, attempt: u32) -> u64 {
    let base = u64::from(clamp_interval(interval_secs));
    let shift = attempt.max(1).saturating_sub(1).min(32);
    base.saturating_mul(1u64 << shift).min(BACKOFF_CAP_SECS)
}

/// 这次掉线要不要排重试。
///
/// 三态口径与「记录会话日志」（`session_log::enabled_for`）完全同型：连接级
/// `Some(v)` 压全局，`None` 跟随全局。生产机器上「关掉自动重连」与
/// 「这台机器必须重连」都表达得出来。
///
/// 次数判据用**已用次数**：`attempts_used < max_retries` 才排下一次。
/// `max_retries = 0` ⇒ 恒 `false`（0 次就是「不许重试」，与全局开关关掉等效）。
pub fn should_retry(
    choice: Option<bool>,
    global: bool,
    attempts_used: u32,
    max_retries: u32,
) -> bool {
    choice.unwrap_or(global) && attempts_used < max_retries
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_from_one_interval_and_then_caps() {
        // 5 → 10 → 20 → 40 → 60 → 60：第 5 次就顶到上限（40×2=80 > 60）
        let seq: Vec<u64> = (1..=7).map(|n| backoff_secs(5, n)).collect();
        assert_eq!(seq, vec![5, 10, 20, 40, 60, 60, 60]);
    }

    #[test]
    fn backoff_honours_the_configured_interval() {
        assert_eq!(backoff_secs(1, 1), 1);
        assert_eq!(backoff_secs(1, 3), 4);
        // 间隔很大时第一次就顶上（钳制到 600，运算里仍封 60）
        assert_eq!(backoff_secs(600, 1), 60);
        // 越界的间隔按钳制后的值算：0 不许退化成「立刻重试」
        assert_eq!(backoff_secs(0, 1), 1);
    }

    #[test]
    fn backoff_is_saturating_at_absurd_attempts() {
        // 第 64 次、u32::MAX 次都不许 panic / 回到 0（移位溢出会得到 0，
        // 那就是「立刻重试」——正是防护要挡的方向）
        assert_eq!(backoff_secs(5, 64), BACKOFF_CAP_SECS);
        assert_eq!(backoff_secs(5, u32::MAX), BACKOFF_CAP_SECS);
        assert_eq!(backoff_secs(5, 0), 5, "0 按第 1 次算，不给「等 0 秒」");
    }

    #[test]
    fn clamp_interval_covers_the_four_editions() {
        assert_eq!(clamp_interval(0), MIN_INTERVAL_SECS);
        assert_eq!(clamp_interval(5), 5);
        assert_eq!(clamp_interval(MAX_INTERVAL_SECS), MAX_INTERVAL_SECS);
        assert_eq!(clamp_interval(u32::MAX), MAX_INTERVAL_SECS);
    }

    #[test]
    fn should_retry_follows_global_and_honours_the_connection_override() {
        // 无覆盖：跟随全局
        assert!(should_retry(None, true, 0, 3));
        assert!(!should_retry(None, false, 0, 3));
        // 连接级开压过全局关（「这台机器必须重连」）
        assert!(should_retry(Some(true), false, 0, 3));
        // 连接级关压过全局开（生产机器别自动连）
        assert!(!should_retry(Some(false), true, 0, 3));
    }

    #[test]
    fn should_retry_stops_exactly_at_the_retry_budget() {
        // 上限 3：已用 0/1/2 排得动，已用 3 停手
        assert!(should_retry(None, true, 0, 3));
        assert!(should_retry(None, true, 2, 3));
        assert!(!should_retry(None, true, 3, 3), "用满 3 次必须停手");
        // 0 次 = 不许重试（与把开关关掉等效），即使全局开着
        assert!(!should_retry(None, true, 0, 0));
    }
}
