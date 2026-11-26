mod dispatch;
mod enroll;
mod handshake;
mod heartbeat;
mod path;

pub(crate) use dispatch::{
    apply_frame, on_signal, status_word, st_take_prompt, PromptStep, LAN_FRESH_US,
};
pub(crate) use enroll::{
    make_enroll_frame, pairing_message, read_challenge, read_frame, read_frame_split,
    reconnect_signal,
};
pub(crate) use heartbeat::{
    bind_beacon, note_beacon, reflex_probe, send_beacon, send_heartbeat, send_heartbeat_locked,
    send_wol, REFLEX_EVERY_TICKS,
};
pub(crate) use handshake::{
    accept_invite, begin_session, fail_session, finish_sessions, maybe_hello, on_datagram,
    send_decision,
};
pub(crate) use path::{finish_hangup, path_silence_step, PATH_DIRECT, PATH_RELAY};

#[cfg(test)]
pub(crate) use dispatch::parse_relay;
#[cfg(test)]
pub(crate) use path::sock_of;
