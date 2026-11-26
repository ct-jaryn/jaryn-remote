use super::events::apply_events;
use crate::live::now_us;
use jr_proto::{encode_outer, Inner, MsgType, OuterFlags, OuterHeader};

use crate::State;

pub fn on_media(st: &mut State, src: std::net::SocketAddr, outer: &jr_proto::OuterHeader) -> Vec<(std::net::SocketAddr, Vec<u8>)> {
    let relay = st.relay.as_ref().map(|r| r.addr);
    let Some((sid, _)) = st.sessions.iter().find(|(_, s)| s.ready).map(|(id, s)| (*id, s.role_controller)) else {
        return Vec::new();
    };
    // 外层头是明文，packet_seq 和源地址在认证之前都不可信。先解密，
    // 认证失败就原样返回，不推进重放窗、不改地址、不碰 last_rx_us。
    let plain = {
        let Some(sess) = st.sessions.get(&sid) else {
            return Vec::new();
        };
        if outer.flags.from_controller == sess.role_controller {
            return Vec::new();
        }
        let Some(keys) = sess.keys.as_ref() else {
            return Vec::new();
        };
        let nonce = jr_proto::nonce(outer.packet_seq, outer.flags.from_controller);
        let key = keys.direction_key(outer.flags.from_controller, false);
        match jr_crypto::open(key, &nonce, &outer.ciphertext) {
            Ok(p) => p,
            Err(_) => return Vec::new(),
        }
    };
    let Some(sess) = st.sessions.get_mut(&sid) else {
        return Vec::new();
    };
    // packet_seq 已被 AEAD 认证，这时推进重放窗才安全。
    if sess.replay.check(outer.packet_seq).is_err() {
        return Vec::new();
    }
    // 谁发的就从谁那儿回。进来的是中继地址就说明这条会话现在走中继。
    if sess.peer_addr != Some(src) {
        sess.peer_addr = Some(src);
        sess.path = if Some(src) == relay { crate::PATH_RELAY } else { crate::PATH_DIRECT };
    }
    if Some(src) != relay {
        sess.direct_addr = Some(src);
    }
    st.last_rx_us = now_us();
    let Ok(inner) = Inner::decode(&plain) else {
        return Vec::new();
    };
    if let Some(ty) = inner.known_type() {
        if !jr_proto::stream_matches(ty, inner.stream_id) {
            return Vec::new();
        }
    }
    let events = st.engine.as_mut().map(|engine| {
        engine.ingest(
            inner.msg_type,
            inner.stream_id,
            inner.frame_id,
            inner.slice_index,
            inner.slice_count,
            inner.timestamp_us,
            now_us(),
            inner.body_flags,
            &inner.body,
        )
    });
    if let Some(events) = events {
        apply_events(st, events);
    }
    seal_pending(st)
}

pub(super) fn seal_pending(st: &mut State) -> Vec<(std::net::SocketAddr, Vec<u8>)> {
    let wires = {
        let Some(engine) = st.engine.as_mut() else {
            return Vec::new();
        };
        engine.poll_wire(now_us() / 1000)
    };
    let Some(sess) = st.sessions.values_mut().find(|s| s.ready) else {
        return Vec::new();
    };
    let Some(addr) = sess.peer_addr else {
        return Vec::new();
    };
    let from_controller = sess.role_controller;
    let Some(key) = sess.keys.as_ref().map(|k| *k.direction_key(from_controller, false)) else {
        return Vec::new();
    };
    let token = st.relay.as_ref().map(|r| r.token).unwrap_or([0u8; 16]);
    let mut out = Vec::new();
    for wire in wires {
        sess.seq = sess.seq.saturating_add(1).max(1);
        let seq = sess.seq;
        let inner = Inner {
            msg_type: wire.msg_type,
            stream_id: wire.stream,
            body_flags: wire.body_flags,
            frame_id: wire.frame_id,
            slice_index: wire.slice_index,
            slice_count: wire.slice_count.max(1),
            timestamp_us: if wire.timestamp_us != 0 { wire.timestamp_us } else { now_us() },
            body: wire.body,
        };
        if let Some(pkt) = seal_media(&key, from_controller, seq, token, &inner) {
            out.push((addr, pkt));
        }
    }
    let _ = MsgType::Ack;
    out
}

fn seal_media(key: &[u8; 32], from_controller: bool, seq: u32, token: [u8; 16], inner: &Inner) -> Option<Vec<u8>> {
    let plain = inner.encode().ok()?;
    let nonce = jr_proto::nonce(seq, from_controller);
    let ct = jr_crypto::seal(key, &nonce, &plain).ok()?;
    encode_outer(&OuterHeader {
        flags: OuterFlags { from_controller, relay_only: false, handshake: false },
        routing_token: token,
        packet_seq: seq,
        ciphertext: ct,
    })
    .ok()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::path::PathBuf;

    use jr_crypto::{Identity, SessionKeys};
    use jr_proto::ReplayWindow;

    use crate::{Sess, State, PATH_DIRECT, PATH_RELAY};

    use super::*;

    fn addr(last: u8) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 0, 2, last)), 4000)
    }

    fn test_keys() -> SessionKeys {
        SessionKeys {
            handshake_c2h: [1; 32],
            handshake_h2c: [2; 32],
            media_c2h: [3; 32],
            media_h2c: [4; 32],
        }
    }

    /// 被控端（role_controller=false）的会话状态。只构造 on_media 用得到的那部分，
    /// 其余字段取中性值，不碰网络、不碰设备。
    fn test_state(peer: SocketAddr) -> State {
        let (diag_tx, _diag_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut sessions = HashMap::new();
        sessions.insert(
            [7u8; 16],
            Sess {
                role_controller: false,
                peer: [9; 32],
                my_pub: [0; 32],
                my_sig: [0; 64],
                peer_pub: None,
                peer_sig: None,
                keys: Some(test_keys()),
                seq: 0,
                replay: ReplayWindow::default(),
                peer_addr: Some(peer),
                candidates: vec![peer],
                path: PATH_DIRECT,
                direct_addr: Some(peer),
                got_ack: true,
                ready: true,
                hello_armed: false,
                hello_tries: 0,
                waiter: None,
            },
        );
        State {
            diag_tx,
            signal_addr: SocketAddr::from(([127, 0, 0, 1], 1)),
            signal_connected: false,
            name: "test".into(),
            identity: Identity::generate().unwrap(),
            short_id: "test".into(),
            peers: HashMap::new(),
            lan_from: HashMap::new(),
            sessions,
            eph: HashMap::new(),
            pending_in: None,
            pending_dg: Vec::new(),
            auto_accept: false,
            platform: 0,
            grants_dir: PathBuf::from("."),
            media_port: 0,
            advertise: Vec::new(),
            extra_candidate: None,
            reflexive: None,
            reflex_nonce: 0,
            reflex_port: 0,
            session_active: true,
            session_features: 0,
            heartbeat_enabled: false,
            last_fp: None,
            last_error: None,
            fleet_tag: [0; 8],
            private_mode: false,
            revoked: false,
            data_dir: PathBuf::from("."),
            relay: None,
            force_relay: false,
            os_inject: false,
            os_clipboard: false,
            os_capture: false,
            display: jr_input::Display { origin_x: 0, origin_y: 0, width: 1920, height: 1080 },
            display_override: false,
            scale_milli: 1000,
            video_ready: false,
            capture_started: false,
            capture_announced: false,
            capture_failed: false,
            audio_started: false,
            audio_retries: 0,
            audio_retry_at_us: 0,
            capture_retries: 0,
            capture_retry_at_us: 0,
            capture_display: 0,
            quality: 2,
            fps: 60,
            applied_bps: 0,
            feedback_count: 0,
            rx_stats: jr_transport::RxStats::default(),
            next_frame: 0,
            last_ping_us: 0,
            last_video: None,
            video_frames: 0,
            audio_frames: 0,
            remote_video: false,
            remote_capability_seen: false,
            remote_w: 0,
            remote_h: 0,
            remote_displays: Vec::new(),
            want_display: 0,
            view_open: false,
            privacy_cover: false,
            pending_hangup: None,
            last_kf_us: 0,
            last_cursor: None,
            last_cursor_shape: None,
            last_rx_us: 0,
            recovering: false,
            recover_since_us: 0,
            indicating: false,
            last_point: None,
            blocker: 0,
            engine: None,
            last_clip: None,
            last_file: None,
            last_transfer: None,
            inject_count: 0,
            capture_on: false,
            term_buf: Vec::new(),
            term_pty: None,
            term_rx: None,
            term_cols: 80,
            term_rows: 24,
            ui_open: false,
            autostart: false,
            pending_prompt: None,
            pending_prompt_name: String::new(),
            prompt_answer: None,
        }
    }

    fn ping_inner() -> Inner {
        Inner {
            msg_type: MsgType::Ping as u8,
            stream_id: 0,
            body_flags: 0,
            frame_id: 0,
            slice_index: 0,
            slice_count: 1,
            timestamp_us: 0,
            body: Vec::new(),
        }
    }

    /// 主控方向（from_controller=true）的合法媒体包。
    fn inbound_outer(st: &State, seq: u32, inner: &Inner) -> OuterHeader {
        let sess = st.sessions.values().next().unwrap();
        let key = sess.keys.as_ref().unwrap().direction_key(true, false);
        let nonce = jr_proto::nonce(seq, true);
        let ciphertext = jr_crypto::seal(key, &nonce, &inner.encode().unwrap()).unwrap();
        OuterHeader {
            flags: OuterFlags { from_controller: true, relay_only: false, handshake: false },
            routing_token: [0; 16],
            packet_seq: seq,
            ciphertext,
        }
    }

    /// 攻击者能造出来的东西：外层头全对，密文是假的。
    fn forged(seq: u32) -> OuterHeader {
        OuterHeader {
            flags: OuterFlags { from_controller: true, relay_only: false, handshake: false },
            routing_token: [0; 16],
            packet_seq: seq,
            ciphertext: vec![0xAB; 48],
        }
    }

    #[test]
    fn forged_outer_packet_has_no_side_effects_and_does_not_move_replay_window() {
        let legit = addr(10);
        let attacker = addr(66);
        let mut st = test_state(legit);
        st.last_rx_us = 123;

        // 伪造一颗 seq 极大的包。认证必然失败，不能改地址，也不能把重放窗推到 1_000_000。
        assert!(on_media(&mut st, attacker, &forged(1_000_000)).is_empty());
        {
            let sess = st.sessions.values().next().unwrap();
            assert_eq!(sess.peer_addr, Some(legit));
            assert_eq!(sess.path, PATH_DIRECT);
            assert_eq!(sess.direct_addr, Some(legit));
        }
        assert_eq!(st.last_rx_us, 123);

        // 合法包 seq=1 仍然收得下，证明重放窗没被伪造包推进。
        let inner = ping_inner();
        let pkt = inbound_outer(&st, 1, &inner);
        on_media(&mut st, legit, &pkt);
        assert!(st.last_rx_us > 123, "合法包应该更新 last_rx_us");
        assert_eq!(st.sessions.values().next().unwrap().peer_addr, Some(legit));

        // 合法包的重放：认证通过但重放窗拒绝，不能改地址。
        let seen = st.last_rx_us;
        on_media(&mut st, attacker, &pkt);
        assert_eq!(st.last_rx_us, seen, "重放包不能更新 last_rx_us");
        {
            let sess = st.sessions.values().next().unwrap();
            assert_eq!(sess.peer_addr, Some(legit));
            assert_eq!(sess.direct_addr, Some(legit));
        }
    }

    #[test]
    fn valid_packet_still_updates_source_path_and_direct_addr() {
        let old = addr(10);
        let new_peer = addr(11);
        let mut st = test_state(old);
        let pkt = inbound_outer(&st, 1, &ping_inner());
        on_media(&mut st, new_peer, &pkt);
        let sess = st.sessions.values().next().unwrap();
        assert_eq!(sess.peer_addr, Some(new_peer));
        assert_eq!(sess.path, PATH_DIRECT);
        assert_eq!(sess.direct_addr, Some(new_peer));
    }

    #[test]
    fn valid_packet_from_relay_switches_path_but_keeps_direct_addr() {
        let direct = addr(10);
        let relay = addr(70);
        let mut st = test_state(direct);
        st.relay = Some(crate::RelayEndpoint { token: [0; 16], addr: relay });
        let pkt = inbound_outer(&st, 1, &ping_inner());
        on_media(&mut st, relay, &pkt);
        let sess = st.sessions.values().next().unwrap();
        assert_eq!(sess.peer_addr, Some(relay));
        assert_eq!(sess.path, PATH_RELAY);
        assert_eq!(sess.direct_addr, Some(direct));
    }
}
