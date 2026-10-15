//! UI 线程现场自诊断（运行反馈轮 V，2026-09-10）

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// UI 线程上可能长时间阻塞的阶段（用于定位）
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Phase {
    Idle = 0,
    /// 会话轮询（`MainView::poll`，每 300ms 一轮）
    Poll = 1,
    /// 视图渲染（`MainView::render`）
    Render = 2,
    /// 终端尺寸变更 到 ConPTY`ResizePseudoConsole`（跨进程同步调用）
    PtyResize = 3,
    /// 会话销毁（关标签 / 关窗口；含 ConPTY`ClosePseudoConsole`的异步释放）
    SessionTeardown = 4,
    /// 启动本地 Shell（挂载终端视图、并入标签）
    Spawn = 5,
}

impl Phase {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Poll,
            2 => Self::Render,
            3 => Self::PtyResize,
            4 => Self::SessionTeardown,
            5 => Self::Spawn,
            _ => Self::Idle,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Phase::Poll => "会话轮询 poll",
            Phase::Render => "视图渲染 render",
            Phase::PtyResize => "终端尺寸变更 ConPTY ResizePseudoConsole",
            Phase::SessionTeardown => "会话销毁 关标签/关窗口",
            Phase::Spawn => "启动本地 Shell",
            Phase::Idle => "空闲",
        }
    }
}

/// 心跳超时阈值：正常轮询间隔 300ms，超过 4s（~=13 个周期）即认定 UI 线程被卡住。
const STALL_MS: u64 = 4_000;
/// 长时间挂起时的重复告警间隔，避免刷屏。
const REPEAT_MS: u64 = 30_000;
/// 墙钟与单调钟推进量之差超过该秒数 到 判定为「系统挂起/休眠」而非「UI 卡死」。
const SUSPEND_DETECT_SECS: u64 = 30;

static EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
static LAST_BEAT_MS: AtomicU64 = AtomicU64::new(0);
static PHASE_SINCE_MS: AtomicU64 = AtomicU64::new(0);
static PHASE: AtomicU8 = AtomicU8::new(0);
static STARTED: AtomicBool = AtomicBool::new(false);

/// 单调毫秒时钟。从 1 开始计时：0 用作「该字段未设置」的哨兵
fn now_ms() -> u64 {
    EPOCH
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis() as u64
        + 1
}

/// 墙钟秒（用于识别系统挂起；单调钟在睡眠期间不前进）
fn wall_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 纯逻辑：墙钟推进`wall_delta`秒、单调钟仅推进`mono_delta`秒时，是否应判定为「刚经历系统挂起/休眠」。
fn is_suspend_gap(wall_delta: u64, mono_delta: u64) -> bool {
    wall_delta.saturating_sub(mono_delta) >= SUSPEND_DETECT_SECS
}

/// UI 线程心跳：每轮`poll`调用一次。
pub fn beat() {
    LAST_BEAT_MS.store(now_ms(), Ordering::Relaxed);
}

/// 进入一个可能在 UI 线程上长时间阻塞的阶段。
pub fn enter(phase: Phase) {
    PHASE.store(phase as u8, Ordering::Relaxed);
    PHASE_SINCE_MS.store(now_ms(), Ordering::Relaxed);
}

/// 阶段结束。
pub fn leave() {
    PHASE_SINCE_MS.store(0, Ordering::Relaxed);
}

/// 纯逻辑：已停滞`elapsed_ms`，上次告警发生在停滞到`last_report_ms`时，现在是否需要（再次）告警。
fn should_report(elapsed_ms: u64, last_report_ms: Option<u64>) -> bool {
    if elapsed_ms < STALL_MS {
        return false;
    }
    match last_report_ms {
        None => true,
        Some(prev) => elapsed_ms.saturating_sub(prev) >= REPEAT_MS,
    }
}

/// 启动看门狗（幂等；在应用启动时调用一次）。
pub fn init() {
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    let _ = EPOCH.set(std::time::Instant::now());
    beat();
    if std::thread::Builder::new()
        .name("rs-ui-watchdog".into())
        .spawn(watchdog_loop)
        .is_err()
    {
        log::warn!("UI 看门狗线程启动失败（不影响功能，仅缺少卡顿定位信息）");
    }
}

fn watchdog_loop() {
    // 一次告警会话：记录首次告警时间，恢复后输出总时长
    let mut beat_report: Option<u64> = None;
    let mut phase_report: Option<u64> = None;
    let mut stall_start_ms: Option<u64> = None;
    // 挂起识别的基线（墙钟秒 + 单调毫秒）
    let mut prev_wall = wall_secs();
    let mut prev_now = now_ms();

    loop {
        std::thread::sleep(Duration::from_millis(1000));
        let now = now_ms();
        let wall = wall_secs();
        let mono_delta = now.saturating_sub(prev_now);
        let wall_delta = wall.saturating_sub(prev_wall);
        prev_now = now;
        prev_wall = wall;
        let phase = Phase::from_u8(PHASE.load(Ordering::Relaxed));

        // 0) 系统挂起/休眠识别（方案 D）：睡眠期间单调钟停摆而墙钟照走，
        // 若不识别，恢复后会报「停滞 10800s / 40599s」这类假告警并淹没真实问题。
        if is_suspend_gap(wall_delta, mono_delta) {
            log::info!(
                "检测到系统挂起/休眠：墙钟推进 {wall_delta}s，单调钟仅推进 {mono_delta}s —— \
                 已重置 UI 看门狗基线（本次不计为 UI 卡顿）"
            );
            beat();
            // 阶段仍在持有时重启其计时，否则阶段检查会把睡眠时长算进去
            if PHASE_SINCE_MS.load(Ordering::Relaxed) != 0 {
                enter(phase);
            }
            beat_report = None;
            phase_report = None;
            stall_start_ms = None;
            continue;
        }

        // 1) 心跳检查：poll 停跳说明 UI 线程无法处理前台任务
        let last_beat = LAST_BEAT_MS.load(Ordering::Relaxed);
        let idle_ms = if last_beat == 0 {
            0
        } else {
            now.saturating_sub(last_beat)
        };
        if last_beat != 0 && idle_ms >= STALL_MS {
            if should_report(idle_ms, beat_report) {
                beat_report = Some(idle_ms);
                let started = *stall_start_ms.get_or_insert(now.saturating_sub(idle_ms));
                log::warn!(
                    "UI 线程已 {:.1}s 无心跳（最后阶段：{}；起点约 {:.1}s）——此刻窗口对系统即为「未响应」，\
                     若持续下去会被判定 AppHang 并以 STATUS_APPLICATION_HANG(0xCFFFFFFF) 结束。\
                     请核查该阶段内的跨进程/阻塞式系统调用。",
                    idle_ms as f64 / 1000.0,
                    phase.label(),
                    started as f64 / 1000.0
                );
            }
        } else if let Some(started) = stall_start_ms.take() {
            log::warn!(
                "UI 线程恢复响应：本次停滞约 {:.1}s（最后阶段：{}）",
                now.saturating_sub(started) as f64 / 1000.0,
                phase.label()
            );
            beat_report = None;
        }

        // 2) 阶段检查：进了某阶段迟迟不出来
        let since = PHASE_SINCE_MS.load(Ordering::Relaxed);
        if since != 0 {
            let held_ms = now.saturating_sub(since);
            if should_report(held_ms, phase_report) {
                phase_report = Some(held_ms);
                log::warn!(
                    "阶段「{}」已持续 {:.1}s 未结束——疑似阻塞在其中，请优先怀疑该调用。",
                    phase.label(),
                    held_ms as f64 / 1000.0
                );
            }
        } else {
            phase_report = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_threshold_and_repeat() {
        // 未达阈值不报
        assert!(!should_report(0, None));
        assert!(!should_report(STALL_MS - 1, None));
        // 首次达阈值报
        assert!(should_report(STALL_MS, None));
        assert!(should_report(STALL_MS + 5_000, None));
        // 已报过则要再等一个重复间隔
        assert!(!should_report(STALL_MS + 1_000, Some(STALL_MS)));
        assert!(!should_report(STALL_MS + REPEAT_MS - 1, Some(STALL_MS)));
        assert!(should_report(STALL_MS + REPEAT_MS, Some(STALL_MS)));
    }

    #[test]
    fn phase_label_is_stable() {
        for p in [
            Phase::Idle,
            Phase::Poll,
            Phase::Render,
            Phase::PtyResize,
            Phase::SessionTeardown,
            Phase::Spawn,
        ] {
            assert!(!p.label().is_empty());
            assert_eq!(Phase::from_u8(p as u8), p);
        }
    }

    #[test]
    fn enter_leave_toggles_phase_holding() {
        enter(Phase::PtyResize);
        assert_ne!(PHASE_SINCE_MS.load(Ordering::Relaxed), 0);
        assert_eq!(PHASE.load(Ordering::Relaxed), Phase::PtyResize as u8);
        leave();
        assert_eq!(PHASE_SINCE_MS.load(Ordering::Relaxed), 0);
    }

    /// 方案 D 回归护栏：正常 tick 不算挂起，睡眠造成的时钟落差才算。
    #[test]
    fn suspend_gap_detection() {
        // 正常：墙钟与单调钟同步推进（含少量抖动）
        assert!(!is_suspend_gap(1, 1));
        assert!(!is_suspend_gap(2, 1));
        assert!(!is_suspend_gap(31, 2)); // 差值 29 到 差一秒未达阈值
        assert!(is_suspend_gap(31, 1)); // 差值 30 到 达阈值（边界，按 >= 判定）
                                        // 睡眠 3 小时：墙钟 +10800s，单调钟几乎没走
        assert!(is_suspend_gap(10_800, 1));
        // 睡眠 11 小时（历史误报值）
        assert!(is_suspend_gap(40_599, 0));
        // 单调钟正常但墙钟被回拨（NTP 校时） 到 差值下溢，不应误判
        assert!(!is_suspend_gap(0, 1));
    }
}
