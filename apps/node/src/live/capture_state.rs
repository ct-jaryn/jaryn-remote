//! 采集状态机里纯做判断的那部分。
//!
//! `sync_capture` 会真的开流、关流、读系统状态，测试跑不到。这里把
//! 「失败之后是重试还是放弃」抽成不碰全局、不碰时间的纯函数，让代际和
//! 次数上限能被回归测试固定住，避免以后改常量时把重试次数或顺序改错。

/// 视频采集中断后最多重试几次。第三次失败就把错误固定下来。
pub(super) const MAX_CAPTURE_RETRIES: u8 = 2;
/// 视频重试前的等待。显示器热插拔时系统要一点时间才恢复。
pub(super) const CAPTURE_RETRY_DELAY_US: u64 = 1_000_000;
/// 音频单独重试，语音失败不影响画面继续传。
pub(super) const MAX_AUDIO_RETRIES: u8 = 3;
pub(super) const AUDIO_RETRY_DELAY_US: u64 = 2_000_000;

/// 一次失败之后该怎么办。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Next {
    /// 已经累计的次数（含这一次）和要等的时长。
    Retry { retries: u8, delay_us: u64 },
    /// 次数用完了，本次会话不再尝试。
    GiveUp,
}

/// 视频采集中断：次数没用完就再等一秒，用完就放弃。
pub(super) fn after_capture_failure(retries: u8) -> Next {
    if retries < MAX_CAPTURE_RETRIES {
        Next::Retry {
            retries: retries.saturating_add(1),
            delay_us: CAPTURE_RETRY_DELAY_US,
        }
    } else {
        Next::GiveUp
    }
}

/// 音频采集中断或启动失败走同一套次数预算。
pub(super) fn after_audio_failure(retries: u8) -> Next {
    if retries < MAX_AUDIO_RETRIES {
        Next::Retry {
            retries: retries.saturating_add(1),
            delay_us: AUDIO_RETRY_DELAY_US,
        }
    } else {
        Next::GiveUp
    }
}

/// 现在该不该再试一次音频。次数用完、或者画面已经失败时都不再试：
/// 画面都不出来时单独开音频只会在远端留下没人看的声音。
pub(super) fn may_start_audio(audio_started: bool, capture_failed: bool, retries: u8) -> bool {
    !audio_started && !capture_failed && retries < MAX_AUDIO_RETRIES
}

/// 视频这边该不该真的去开流。`sync_capture` 每 50ms 跑一次，这三个条件
/// 缺一不可：已经在跑不重开、已经定案失败不再试、重试等待期没过也不试。
/// 抽出来是为了让「放弃之后不会又被重试」这条能被测试钉住——写成散落的
/// 布尔判断时，漏掉任何一项都会变成无限重开日志或永久黑屏。
pub(super) fn should_start_capture(started: bool, failed: bool, retry_at_us: u64, now_us: u64) -> bool {
    !started && !failed && now_us >= retry_at_us
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_failure_retries_twice_then_gives_up() {
        assert_eq!(
            after_capture_failure(0),
            Next::Retry { retries: 1, delay_us: CAPTURE_RETRY_DELAY_US }
        );
        assert_eq!(
            after_capture_failure(1),
            Next::Retry { retries: 2, delay_us: CAPTURE_RETRY_DELAY_US }
        );
        // 累计两次之后不能再无限重试：显示器真的没了时会一直打日志。
        assert_eq!(after_capture_failure(2), Next::GiveUp);
        assert_eq!(after_capture_failure(u8::MAX), Next::GiveUp);
    }

    #[test]
    fn audio_failure_retries_three_times_then_gives_up() {
        assert_eq!(
            after_audio_failure(0),
            Next::Retry { retries: 1, delay_us: AUDIO_RETRY_DELAY_US }
        );
        assert_eq!(
            after_audio_failure(2),
            Next::Retry { retries: 3, delay_us: AUDIO_RETRY_DELAY_US }
        );
        assert_eq!(after_audio_failure(3), Next::GiveUp);
        // 次数不能回绕成 0，否则会永远重试。
        assert_eq!(after_audio_failure(u8::MAX - 1), Next::GiveUp);
    }

    #[test]
    fn audio_never_starts_after_budget_or_capture_failure() {
        assert!(may_start_audio(false, false, 0));
        assert!(!may_start_audio(true, false, 0));
        assert!(!may_start_audio(false, true, 0));
        assert!(!may_start_audio(false, false, MAX_AUDIO_RETRIES));
    }

    /// 放弃采集之后不能再开：条件里漏掉 `failed` 就会每 50ms 重开一次流，
    /// 日志刷屏且显示器真的没了时永远恢复不了。等待期没过也不能开。
    #[test]
    fn capture_start_waits_for_retry_and_stops_after_giveup() {
        assert!(should_start_capture(false, false, 0, 100));
        assert!(!should_start_capture(true, false, 0, 100), "已经在跑不重开");
        assert!(!should_start_capture(false, true, 0, 100), "放弃之后不能再开");
        assert!(!should_start_capture(false, false, 500, 100), "等待期没过不能开");
        assert!(should_start_capture(false, false, 500, 500), "到点可以开");
    }

    #[test]
    fn retry_delay_is_longer_for_audio() {
        // 音频重试比视频慢：设备切换时立刻重开会拿到同一个坏设备。
        assert!(AUDIO_RETRY_DELAY_US > CAPTURE_RETRY_DELAY_US);
    }
}
