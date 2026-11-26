use std::time::{SystemTime, UNIX_EPOCH};

use jr_proto::Reason;

use crate::State;

mod capabilities;
mod capture;
mod capture_state;
mod events;
mod media;
mod terminal;
mod view;

pub use capabilities::{local_features_for, local_host_features, open_engine};
pub use events::release;
pub use media::on_media;
pub use terminal::note_recovery;

use capture::sync_capture;
use events::apply_events;
use media::seal_pending;
use terminal::drain_shell;
use view::{send_feedback_due, sync_view};

pub fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

pub(crate) fn bitrate_of(profile: u8) -> i32 {
    match profile {
        1 => 8_000_000,
        3 => 6_000_000,
        4 => 1_000_000,
        _ => 4_000_000,
    }
}

pub fn poll_media(st: &mut State) -> Vec<(std::net::SocketAddr, Vec<u8>)> {
    if let Some(reason) = crate::path_silence_step(st) {
        crate::finish_hangup(st, reason);
        return Vec::new();
    }
    drain_shell(st);
    let now = now_us();
    if st.sessions.values().any(|s| s.ready) && now.saturating_sub(st.last_ping_us) > 200_000 {
        if let Some(engine) = st.engine.as_mut() {
            engine.send_ping(now);
        }
        st.last_ping_us = now;
    }
    sync_capture(st);
    sync_view(st);
    // 先无条件把标志取走再判断。原生窗口总会在解码失败、等 IDR 超时时置位，
    // 窗口关着或不归我们发请求时标志留着不清，下一段会话一开窗就会立刻
    // 补发一条过期的关键帧请求，把对端刚编好的 GOP 打断。
    let view_wants_keyframe = crate::av::take_view_keyframe_request();
    if view_wants_keyframe && now.saturating_sub(st.last_kf_us) >= 200_000 {
        if st.view_open {
            st.last_kf_us = now;
            if let Some(engine) = st.engine.as_mut() {
                if engine.is_controller() {
                    let _ = engine.request_keyframe(st.next_frame);
                }
            }
        }
    }
    send_feedback_due(st);
    if crate::av::take_hangup() {
        if let Some(engine) = st.engine.as_mut() {
            let _ = engine.hangup(Reason::UserHangup);
        }
        st.pending_hangup = Some(Reason::UserHangup);
    }
    let Some(engine) = st.engine.as_mut() else {
        return Vec::new();
    };
    let events = engine.tick(now);
    apply_events(st, events);
    seal_pending(st)
}
