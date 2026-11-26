use std::net::SocketAddr;

use jr_crypto::{ephemeral_message, verify_grant, EphKey, SessionKeys};
use jr_proto::{
    encode_frame, encode_outer, hex_encode, Candidates, Decision, Ephemeral, Inner, Invite, MsgType,
    OuterFlags, OuterHeader, SockAddr, SignalType, FEAT_AUDIO_OUT, PLATFORM_IOS, Reason,
};
use tokio::sync::mpsc;

use crate::net::{ip_bytes, unix_now};
use crate::{
    invite_error_code, send_heartbeat_locked, send_state_diagnostic, Sess, State, HELLO_ACK_BODY,
    HELLO_BODY,
};

use super::path::{routing_token, PATH_DIRECT, PATH_RELAY};

pub(crate) fn begin_session(st: &mut State, sid: [u8; 16], peer: [u8; 32], controller: bool) -> Result<(), String> {
    if controller {
        st.remote_video = false;
        st.remote_capability_seen = false;
        st.view_open = false;
        st.remote_displays.clear();
    }
    let eph = EphKey::generate().map_err(|e| e.to_string())?;
    let role = if controller { 1 } else { 2 };
    let sig = st.identity.sign(&ephemeral_message(
        &sid,
        role,
        &eph.public,
        &peer,
    ));
    let sess = Sess {
        role_controller: controller,
        peer,
        my_pub: eph.public,
        my_sig: sig,
        peer_pub: None,
        peer_sig: None,
        keys: None,
        seq: 0,
        replay: jr_proto::ReplayWindow::default(),
        peer_addr: None,
        candidates: Vec::new(),
        path: 0,
        direct_addr: None,
        got_ack: false,
        ready: false,
        hello_armed: false,
        hello_tries: 0,
        waiter: None,
    };
    st.eph.insert(sid, eph);
    st.sessions.insert(sid, sess);
    st.session_active = true;
    Ok(())
}

/// 收发地址和路径状态必须一起改：只改 peer_addr 会让诊断和 `direct_path()`
/// 把“实际在发中继”的会话报成直连。中继地址一律记 PATH_RELAY，其余记直连。
/// `force_relay` 的会话即使收到直连来源也一律算中继。
pub(super) fn set_peer_route(
    sess: &mut Sess,
    addr: SocketAddr,
    relay: Option<SocketAddr>,
    force_relay: bool,
) {
    sess.peer_addr = Some(addr);
    sess.path = if force_relay || Some(addr) == relay {
        PATH_RELAY
    } else {
        PATH_DIRECT
    };
}

pub(super) fn send_ephemeral(st: &State, tx: &mpsc::UnboundedSender<Vec<u8>>, sid: &[u8; 16]) {
    let Some(sess) = st.sessions.get(sid) else {
        return;
    };
    let eph = Ephemeral {
        session_id: *sid,
        sender_id: st.identity.device_id(),
        eph_pub: sess.my_pub,
        sig: sess.my_sig,
        role: if sess.role_controller { 1 } else { 2 },
    };
    if let Ok(frame) = encode_frame(SignalType::Ephemeral as u8, &eph.encode()) {
        let _ = tx.send(frame);
        println!(
            "node: ephemeral sent session={} role={}",
            hex_encode(&sid[..4]),
            eph.role
        );
    }
}

pub(super) fn candidates_frame(st: &State, sid: &[u8; 16]) -> Option<Vec<u8>> {
    let mut addrs: Vec<SockAddr> = st
        .advertise
        .iter()
        .map(|ip| SockAddr {
            path_id: 0,
            ip: ip.clone(),
            port: st.media_port,
        })
        .collect();
    // 反射地址排在前面：直连候选打不通的时候，这是唯一能打通的路。
    if let Some(addr) = st.reflexive {
        addrs.insert(
            0,
            SockAddr {
                path_id: 1,
                ip: ip_bytes(addr.ip()),
                port: addr.port(),
            },
        );
    }
    if let Some(addr) = st.extra_candidate {
        addrs.push(SockAddr {
            path_id: 2,
            ip: ip_bytes(addr.ip()),
            port: addr.port(),
        });
    }
    let body = Candidates {
        session_id: *sid,
        addrs,
    };
    let raw = body.encode().ok()?;
    encode_frame(SignalType::Candidates as u8, &raw).ok()
}

pub(super) fn send_candidates(st: &State, tx: &mpsc::UnboundedSender<Vec<u8>>, sid: &[u8; 16]) {
    if let Some(frame) = candidates_frame(st, sid) {
        let _ = tx.send(frame);
    }
}

pub(crate) fn reflex_probe_bytes(nonce: u64) -> [u8; 12] {
    let mut out = [0u8; 12];
    out[0..4].copy_from_slice(b"JRRF");
    out[4..12].copy_from_slice(&nonce.to_le_bytes());
    out
}

pub(crate) fn send_decision(
    st: &State,
    tx: &mpsc::UnboundedSender<Vec<u8>>,
    sid: &[u8; 16],
    accept: bool,
    reason: Reason,
) {
    let _ = st;
    let d = Decision {
        session_id: *sid,
        accept,
        reason: reason as u16,
    };
    if let Ok(frame) = encode_frame(SignalType::InviteDecision as u8, &d.encode()) {
        let _ = tx.send(frame);
    }
}

pub(crate) fn accept_invite(st: &mut State, invite: Invite, tx: &mpsc::UnboundedSender<Vec<u8>>) -> Vec<(SocketAddr, Vec<u8>)> {
    if st.platform == PLATFORM_IOS {
        send_decision(st, tx, &invite.session_id, false, Reason::PlatformNotHostable);
        return Vec::new();
    }
    if st.session_active {
        send_decision(st, tx, &invite.session_id, false, Reason::Busy);
        return Vec::new();
    }
    if begin_session(st, invite.session_id, invite.controller_id, false).is_err() {
        send_decision(st, tx, &invite.session_id, false, Reason::SessionDead);
        return Vec::new();
    }
    st.session_features = invite.feature_bits;
    send_decision(st, tx, &invite.session_id, true, Reason::Ok);
    send_state_diagnostic(st, tx, 6, 0, Some(&invite.session_id));
    send_ephemeral(st, tx, &invite.session_id);
    send_candidates(st, tx, &invite.session_id);
    arm_hello(st, invite.session_id);
    Vec::new()
}

pub(super) fn eval_unattended(st: &State, invite: &mut Invite) -> Result<(), String> {
    let path = st.grants_dir.join(hex_encode(&invite.controller_id));
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(_) => return Err(Reason::Unauthorized.user_text().into()),
    };
    match verify_grant(&bytes, unix_now()) {
        Ok(claims) => {
            if claims.host != st.identity.device_id() {
                return Err("伪造的无人值守票据被拒绝".into());
            }
            // 只校验本机确实能提供的能力：主控不知道被控端缺哪项权限，
            // 不能因为它申请了本机没有的能力就整张票据作废。
            let requested = invite.feature_bits & crate::live::local_host_features(st);
            if jr_crypto::grant_covers(&claims, &invite.controller_id, requested).is_ok() {
                return Ok(());
            }
            // 音频位是后加的。旧票据没有这一位时不能整张作废，否则升级后
            // 所有已授权的无人值守连接都会被判越权；降级为无声音会话。
            if requested & FEAT_AUDIO_OUT != 0 {
                let without_audio = requested & !FEAT_AUDIO_OUT;
                if jr_crypto::grant_covers(&claims, &invite.controller_id, without_audio).is_ok() {
                    invite.feature_bits &= !FEAT_AUDIO_OUT;
                    return Ok(());
                }
            }
            Err(Reason::Unauthorized.user_text().into())
        }
        Err(jr_crypto::CryptoError::BadSignature) | Err(jr_crypto::CryptoError::Length) => {
            Err("伪造的无人值守票据被拒绝".into())
        }
        Err(_) => Err(Reason::Unauthorized.user_text().into()),
    }
}

pub(crate) fn finish_sessions(st: &mut State, result: Result<String, String>) {
    let sessions = std::mem::take(&mut st.sessions);
    st.eph.clear();
    st.pending_dg.clear();
    for (_, mut sess) in sessions {
        if let Some(tx) = sess.waiter.take() {
            let _ = tx.send(result.clone());
        }
    }
}

pub(crate) fn fail_session(st: &mut State, sid: [u8; 16], text: &str) {
    send_state_diagnostic(st, &st.diag_tx, 9, invite_error_code(text), Some(&sid));
    st.last_error = Some(text.into());
    if let Some(mut sess) = st.sessions.remove(&sid) {
        if let Some(wait) = sess.waiter.take() {
            let _ = wait.send(Err(text.into()));
        }
    }
    st.eph.remove(&sid);
    st.session_active = !st.sessions.is_empty();
}

pub(super) fn arm_hello(st: &mut State, sid: [u8; 16]) {
    let Some(sess) = st.sessions.get_mut(&sid) else {
        return;
    };
    sess.hello_armed = true;
}

pub(crate) fn maybe_hello(st: &mut State, sid: [u8; 16]) -> Vec<(SocketAddr, Vec<u8>)> {
    if st.force_relay && st.relay.is_none() {
        return Vec::new();
    }
    let relay = st.relay.as_ref().map(|r| r.addr);
    let token = routing_token(st);
    let me = st.identity.device_id();
    let Some(sess) = st.sessions.get_mut(&sid) else {
        return Vec::new();
    };
    if sess.got_ack || sess.keys.is_none() {
        return Vec::new();
    }
    if st.force_relay {
        if let Some(addr) = relay {
            set_peer_route(sess, addr, relay, true);
        }
    } else if sess.hello_tries >= 5 && !sess.got_ack {
        if let Some(addr) = relay {
            if sess.path != PATH_RELAY {
                println!("node: path relay");
            }
            // 直连试了几轮还没 ACK：地址和路一起落到中继上，不能只改地址。
            set_peer_route(sess, addr, relay, false);
        }
    }
    let Some(addr) = sess.peer_addr else {
        return Vec::new();
    };
    if st.force_relay && sess.hello_tries == 0 {
        println!("node: path relay");
    }
    sess.hello_tries = sess.hello_tries.saturating_add(1);
    // 还没握上手就同时投向每一个候选，每次重发都这样。
    // 只在第一次这么做不够：打洞的第一颗包几乎一定被对端 NAT 丢掉，
    // 要等两边都往外发过，后面的包才能进来。
    let mut targets = Vec::new();
    if !st.force_relay && !sess.got_ack {
        for cand in &sess.candidates {
            if !targets.contains(cand) {
                targets.push(*cand);
            }
        }
    }
    if !targets.contains(&addr) {
        targets.push(addr);
    }
    match build_hello(sess, sid, me, &token) {
        // 同一颗包投给多个地址：序号一样，对面按序号去重，不会当成两次握手。
        Some(pkt) => targets.into_iter().map(|t| (t, pkt.clone())).collect(),
        None => Vec::new(),
    }
}

pub(super) fn build_hello(sess: &mut Sess, sid: [u8; 16], me: [u8; 32], token: &[u8; 16]) -> Option<Vec<u8>> {
    let keys = sess.keys.as_ref()?;
    sess.seq = sess.seq.saturating_add(1);
    if sess.seq == 0 {
        sess.seq = 1;
    }
    let seq = sess.seq;
    let mut body = Vec::with_capacity(HELLO_BODY);
    body.extend_from_slice(&sid);
    body.extend_from_slice(&me);
    body.extend_from_slice(&sess.my_pub);
    body.extend_from_slice(&sess.my_sig);
    body.push(0);
    let inner = Inner {
        msg_type: MsgType::Hello as u8,
        stream_id: 0,
        body_flags: 0,
        frame_id: 0,
        slice_index: 0,
        slice_count: 1,
        timestamp_us: 0,
        body,
    };
    seal_handshake(keys, sess.role_controller, seq, &inner, token)
}

pub(super) fn seal_handshake(
    keys: &SessionKeys,
    from_controller: bool,
    seq: u32,
    inner: &Inner,
    token: &[u8; 16],
) -> Option<Vec<u8>> {
    let plain = inner.encode().ok()?;
    let nonce = jr_proto::nonce(seq, from_controller);
    let key = keys.direction_key(from_controller, true);
    let ct = jr_crypto::seal(key, &nonce, &plain).ok()?;
    let outer = OuterHeader {
        flags: OuterFlags {
            from_controller,
            relay_only: false,
            handshake: true,
        },
        routing_token: *token,
        packet_seq: seq,
        ciphertext: ct,
    };
    encode_outer(&outer).ok()
}

pub(crate) fn on_datagram(
    st: &mut State,
    src: SocketAddr,
    buf: &[u8],
    tx: &mpsc::UnboundedSender<Vec<u8>>,
) -> Vec<(SocketAddr, Vec<u8>)> {
    let outer = match jr_proto::decode_outer(buf) {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };
    if outer.ciphertext.is_empty() {
        return Vec::new();
    }
    if !outer.flags.handshake {
        return crate::live::on_media(st, src, &outer);
    }
    // 握手包是加密的，看不出会话号，只能挨个会话试着解。会话一般只有一个。
    // 已经握上手的会话也要试：对面可能还没收到我们的 ACK，正在重发 Hello，
    // 把这种包丢掉会让对面一直等到超时。
    let sids: Vec<[u8; 16]> = st
        .sessions
        .iter()
        .filter(|(_, s)| s.keys.is_some())
        .map(|(id, _)| *id)
        .collect();
    if sids.is_empty() {
        // 密钥还没协商出来。同一个来源只留最新一颗：打洞会持续重发，
        // 不限来源会把队列挤满，真正能用的那颗反而被丢掉。
        st.pending_dg.retain(|(addr, _)| *addr != src);
        if st.pending_dg.len() < 8 {
            st.pending_dg.push((src, buf.to_vec()));
        }
        return Vec::new();
    }
    for sid in sids {
        if let Some(replies) = on_handshake(st, sid, src, &outer, tx) {
            return replies;
        }
    }
    Vec::new()
}

/// 试着用某个会话解这颗握手包。解不开就返回 None，交给下一个会话。
pub(super) fn on_handshake(
    st: &mut State,
    sid: [u8; 16],
    src: SocketAddr,
    outer: &jr_proto::OuterHeader,
    tx: &mpsc::UnboundedSender<Vec<u8>>,
) -> Option<Vec<(SocketAddr, Vec<u8>)>> {
    let sess = st.sessions.get(&sid)?;
    let peer_from_controller = !sess.role_controller;
    if outer.flags.from_controller != peer_from_controller {
        return None;
    }
    let opened = {
        let sess = st.sessions.get(&sid)?;
        let keys = sess.keys.as_ref()?;
        let nonce = jr_proto::nonce(outer.packet_seq, outer.flags.from_controller);
        let key = keys.direction_key(outer.flags.from_controller, true);
        jr_crypto::open(key, &nonce, &outer.ciphertext).ok()
    };
    let plain = opened?;
    let inner = Inner::decode(&plain).ok()?;
    if let Some(ty) = inner.known_type() {
        if !jr_proto::stream_matches(ty, inner.stream_id) {
            return None;
        }
    } else {
        return None;
    }
    {
        let sess = st.sessions.get_mut(&sid)?;
        if sess.replay.check(outer.packet_seq).is_err() {
            return None;
        }
    }
    Some(match MsgType::from_u8(inner.msg_type) {
        Some(MsgType::Hello) => on_hello(st, sid, src, outer.packet_seq, &inner.body),
        Some(MsgType::HelloAck) => on_hello_ack(st, sid, src, &inner.body, tx),
        Some(_) => Vec::new(),
        None => Vec::new(),
    })
}

pub(super) fn on_hello(st: &mut State, sid: [u8; 16], src: SocketAddr, echo_seq: u32, body: &[u8]) -> Vec<(SocketAddr, Vec<u8>)> {
    let token = routing_token(st);
    let relay = st.relay.as_ref().map(|r| r.addr);
    if body.len() != HELLO_BODY {
        return Vec::new();
    }
    let mut got_sid = [0u8; 16];
    got_sid.copy_from_slice(&body[..16]);
    if got_sid != sid {
        return Vec::new();
    }
    let mut sender = [0u8; 32];
    sender.copy_from_slice(&body[16..48]);
    let Some(sess) = st.sessions.get_mut(&sid) else {
        return Vec::new();
    };
    if sender != sess.peer {
        return Vec::new();
    }
    let mut pub_bytes = [0u8; 32];
    pub_bytes.copy_from_slice(&body[48..80]);
    if sess.peer_pub != Some(pub_bytes) {
        return Vec::new();
    }
    if !st.force_relay {
        // Hello 从哪来就回哪去，路也跟着这个地址走：中继转来的包
        // 必须把会话标成中继，否则 ACK 回来时状态会一直是“直连”。
        set_peer_route(sess, src, relay, false);
    }
    let path_id = body[144];
    let from_controller = sess.role_controller;
    let already_acked = sess.got_ack;
    sess.seq = sess.seq.saturating_add(1);
    if sess.seq == 0 {
        sess.seq = 1;
    }
    let seq = sess.seq;
    let mut ack_body = Vec::with_capacity(HELLO_ACK_BODY);
    ack_body.extend_from_slice(&sid);
    ack_body.extend_from_slice(&echo_seq.to_le_bytes());
    ack_body.push(path_id);
    let inner = Inner {
        msg_type: MsgType::HelloAck as u8,
        stream_id: 0,
        body_flags: 0,
        frame_id: 0,
        slice_index: 0,
        slice_count: 1,
        timestamp_us: 0,
        body: ack_body,
    };
    let ack = sess.keys.as_ref().and_then(|keys| {
        seal_handshake(keys, from_controller, seq, &inner, &token)
    });
    let Some(pkt) = ack else {
        return Vec::new();
    };
    st.last_rx_us = crate::live::now_us();
    let me = st.identity.device_id();
    let hello = if already_acked {
        None
    } else if let Some(sess) = st.sessions.get_mut(&sid) {
        build_hello(sess, sid, me, &token)
    } else {
        None
    };
    let mut out = vec![(src, pkt)];
    if let Some(hello) = hello {
        out.push((src, hello));
    }
    out
}

pub(super) fn on_hello_ack(st: &mut State, sid: [u8; 16], src: SocketAddr, body: &[u8], tx: &mpsc::UnboundedSender<Vec<u8>>) -> Vec<(SocketAddr, Vec<u8>)> {
    if body.len() != HELLO_ACK_BODY {
        return Vec::new();
    }
    let mut got = [0u8; 16];
    got.copy_from_slice(&body[..16]);
    if got != sid {
        return Vec::new();
    }
    let relay = st.relay.as_ref().map(|r| r.addr);
    let (fp, host) = {
        let Some(sess) = st.sessions.get_mut(&sid) else {
            return Vec::new();
        };
        if sess.keys.is_none() || sess.got_ack {
            return Vec::new();
        }
        sess.got_ack = true;
        sess.ready = true;
        // 这个 ACK 真实到达的地址就是这条会话当前能用的路：打洞时同一颗
        // Hello 会投给所有候选，先到的 ACK 才能证明哪条路真的通。中继回来的
        // ACK 必须把路标成中继，否则路径状态会和实际收发地址不一致。
        if st.force_relay || Some(src) == relay {
            let addr = relay.unwrap_or(src);
            set_peer_route(sess, addr, relay, st.force_relay);
        } else {
            sess.direct_addr = Some(src);
            set_peer_route(sess, src, relay, false);
        }
        let host = !sess.role_controller;
        let fp = sess.keys.as_ref().unwrap().fingerprint_hex();
        if let Some(wait) = sess.waiter.take() {
            let _ = wait.send(Ok(fp.clone()));
        }
        (fp, host)
    };
    st.last_rx_us = crate::live::now_us();
    if host {
        st.indicating = true;
        crate::av::indicator(true);
        println!("node: 正在被控");
    }
    println!("node: keys-ready name={} fp={fp}", st.name);
    send_state_diagnostic(st, tx, 8, u16::from(host), Some(&sid));
    let path = st.sessions.get(&sid).map(|s| s.path).unwrap_or(0);
    send_state_diagnostic(st, tx, 16, path as u16, Some(&sid));
    st.last_fp = Some(fp);
    send_heartbeat_locked(st, tx);
    let controller = st.sessions.get(&sid).map(|s| s.role_controller).unwrap_or(false);
    crate::live::open_engine(st, controller);
    crate::live::poll_media(st)
}
