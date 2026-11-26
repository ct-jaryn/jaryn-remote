use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, ToSocketAddrs};

use jr_crypto::{ephemeral_message, verify, SessionKeys};
use jr_proto::{
    hex_encode, Candidates, Decision, DeviceSnap, Ephemeral, Invite, Presence, PresenceKind,
    RelayTicket, SignalType, Reason,
};
use tokio::sync::mpsc;

use crate::{send_heartbeat_locked, send_state_diagnostic, send_wol, Peer, RelayEndpoint, State};

use super::handshake::{
    accept_invite, arm_hello, candidates_frame, eval_unattended, fail_session, finish_sessions,
    maybe_hello, on_datagram, send_candidates, send_decision, send_ephemeral, set_peer_route,
};
use super::path::{finish_hangup, pick_direct, sock_of, PATH_DIRECT, PATH_RELAY};

pub(crate) fn apply_frame(st: &mut State, frame: &jr_proto::SignalFrame, tx: &mpsc::UnboundedSender<Vec<u8>>) -> Vec<(SocketAddr, Vec<u8>)> {
    on_signal(st, frame, tx)
}

pub(crate) fn on_signal(st: &mut State, frame: &jr_proto::SignalFrame, tx: &mpsc::UnboundedSender<Vec<u8>>) -> Vec<(SocketAddr, Vec<u8>)> {
    let mut replies = Vec::new();
    let mut refresh = false;
    match SignalType::from_u8(frame.msg_type) {
        Some(SignalType::Presence) => {
            if let Ok(p) = Presence::decode(&frame.body) {
                apply_presence(st, p);
            }
        }
        Some(SignalType::Invite) => {
            if let Ok(invite) = Invite::decode(&frame.body) {
                replies.extend(on_invite(st, invite, tx));
            }
        }
        Some(SignalType::InviteDecision) => {
            if let Ok(d) = Decision::decode(&frame.body) {
                on_decision(st, d, tx);
            }
        }
        Some(SignalType::Ephemeral) => {
            if let Ok(eph) = Ephemeral::decode(&frame.body) {
                replies.extend(on_ephemeral(st, eph, tx));
            }
        }
        Some(SignalType::ReflexiveOk) => {
            // 体：ip 长度 | ip | 端口。
            if frame.body.len() >= 4 {
                let len = frame.body[0] as usize;
                if frame.body.len() >= 1 + len + 2 && len == 4 {
                    let ip = Ipv4Addr::new(frame.body[1], frame.body[2], frame.body[3], frame.body[4]);
                    let port = u16::from_le_bytes([frame.body[1 + len], frame.body[2 + len]]);
                    let addr = SocketAddr::from(SocketAddrV4::new(ip, port));
                    if st.reflexive != Some(addr) {
                        st.reflexive = Some(addr);
                        println!("node: 公网地址 {addr}");
                        refresh = true;
                    }
                }
            }
        }
        Some(SignalType::Candidates) => {
            if let Ok(c) = Candidates::decode(&frame.body) {
                replies.extend(on_candidates(st, c));
            }
        }
        Some(SignalType::RelayTicket) => {
            if let Ok(t) = RelayTicket::decode(&frame.body) {
                replies.extend(store_ticket(st, t));
            }
        }
        Some(SignalType::Revoke) => {
            st.revoked = true;
            println!("node: revoked-self short={}", st.short_id);
            finish_sessions(st, Err(Reason::Revoked.user_text().into()));
        }
        Some(SignalType::Wake) => {
            if let Ok(wake) = jr_proto::Wake::decode(&frame.body) {
                if wake.mac == [0; 6] {
                    // 信令在这两种情况下都回全零 MAC，本机分不出是哪一种：
                    // 对方没登记过网卡，或者当前没有能代发魔包的同网节点。
                    // 只说后者会让用户去查网络，而真正原因是对方从没跑过
                    // `wake-mac`。两种可能都要说出来。
                    let msg = "叫不醒：对方没登记网卡 MAC，或暂时没有同网常开节点".to_string();
                    println!("node: {msg}");
                    if st.ui_open {
                        crate::ui::notice(&msg);
                    }
                    st.last_error = Some(msg);
                } else {
                    send_wol(wake.mac);
                }
            }
        }
        Some(SignalType::ListAudit) => {
            if frame.body.len() >= 2 {
                let n = u16::from_le_bytes([frame.body[0], frame.body[1]]) as usize;
                println!("node: audit rows={n}");
                let mut off = 2;
                for _ in 0..n {
                    // 一行 8 + 8 + 2 + 1 字节
                    if frame.body.len() < off + 19 {
                        break;
                    }
                    let started = i64::from_le_bytes(frame.body[off..off + 8].try_into().unwrap_or([0; 8]));
                    let ended = i64::from_le_bytes(frame.body[off + 8..off + 16].try_into().unwrap_or([0; 8]));
                    let reason = u16::from_le_bytes([frame.body[off + 16], frame.body[off + 17]]);
                    let path = frame.body[off + 18];
                    let outcome = if ended == 0 {
                        "还开着".to_string()
                    } else {
                        jr_proto::Reason::from_u16(reason)
                            .map(|r| r.user_text().to_string())
                            .unwrap_or_else(|| format!("原因 {reason}"))
                    };
                    let path_word = match path {
                        0 => "未知",
                        1 => "直连",
                        _ => "中继",
                    };
                    println!("node:   开始 {started} 结束 {ended} {outcome} 路径 {path_word}");
                    off += 19;
                }
            }
        }
        Some(_) => {}
        None => {
            println!("node: skip unknown signal type={}", frame.msg_type);
        }
    }
    if refresh {
        // 地址变了，把所有在谈会话的候选都重发一遍。
        for sid in st.sessions.keys().copied().collect::<Vec<_>>() {
            if let Some(frame) = candidates_frame(st, &sid) {
                let _ = tx.send(frame);
            }
        }
    }
    replies
}

pub(super) fn apply_presence(st: &mut State, presence: Presence) {
    match presence.kind {
        PresenceKind::Full => {
            st.peers.clear();
            for snap in presence.devices {
                println!(
                    "node: peer short={} name={} {}",
                    snap.short_id,
                    snap.name,
                    status_word(&snap)
                );
                insert_peer(st, snap);
            }
        }
        PresenceKind::NewDevice => {
            for snap in presence.devices {
                println!("node: new-device short={} name={}", snap.short_id, snap.name);
                insert_peer(st, snap);
            }
        }
        PresenceKind::Upsert | PresenceKind::Offline | PresenceKind::Revoked => {
            for snap in presence.devices {
                let prev = st.peers.get(&snap.device_id).map(|p| p.snap.clone());
                if let Some(prev) = prev {
                    if prev.online && !snap.online {
                        println!("node: offline short={}", snap.short_id);
                    } else if !prev.online && snap.online {
                        println!("node: online short={}", snap.short_id);
                    }
                    if !prev.revoked && snap.revoked {
                        println!("node: revoked short={}", snap.short_id);
                    }
                }
                if snap.revoked && st.sessions.values().any(|s| s.peer == snap.device_id) {
                    finish_hangup(st, Reason::Revoked);
                }
                insert_peer(st, snap);
            }
        }
    }
}

pub(crate) fn status_word(snap: &DeviceSnap) -> &'static str {
    if snap.revoked {
        "已撤销"
    } else if snap.blocker == 1 || snap.blocker == 2 {
        "需授权"
    } else if snap.blocker == 3 {
        "需重新允许录屏"
    } else if snap.blocker == 4 {
        "仅终端"
    } else if snap.blocker == 5 {
        "对方正在系统安全提示，无法查看"
    } else if snap.session_active {
        "忙"
    } else if snap.online {
        "在线"
    } else {
        "离线"
    }
}

/// 局域网信标只在最近 30 秒内算数。笔记本换网、休眠、对端退出后，
/// 不能让“曾经在同一网段”永久留在界面上。
pub(crate) const LAN_FRESH_US: u64 = 30_000_000;

pub(super) fn insert_peer(st: &mut State, snap: DeviceSnap) {
    let lan = st
        .lan_from
        .get(&snap.device_id)
        .map(|(_, seen)| crate::live::now_us().saturating_sub(*seen) <= LAN_FRESH_US)
        .unwrap_or(false);
    st.peers.insert(snap.device_id, Peer { snap, lan });
}

pub(super) fn on_invite(st: &mut State, mut invite: Invite, tx: &mpsc::UnboundedSender<Vec<u8>>) -> Vec<(SocketAddr, Vec<u8>)> {
    if invite.host_id != st.identity.device_id() {
        return Vec::new();
    }
    send_state_diagnostic(st, tx, 5, u16::from(invite.unattended), Some(&invite.session_id));
    if invite.unattended && !jr_session::host_profile(st.platform).unattended {
        println!("node: {}", jr_session::host_profile(st.platform).label);
        st.last_error = Some(Reason::Unauthorized.user_text().into());
        send_decision(st, tx, &invite.session_id, false, Reason::Unauthorized);
        return Vec::new();
    }
    if invite.unattended {
        match eval_unattended(st, &mut invite) {
            Ok(()) => return accept_invite(st, invite, tx),
            Err(e) => {
                println!("node: {e}");
                st.last_error = Some(e);
                send_decision(st, tx, &invite.session_id, false, Reason::Unauthorized);
                return Vec::new();
            }
        }
    }
    if st.auto_accept {
        return accept_invite(st, invite, tx);
    }
    if st.ui_open {
        // 界面开着就走弹窗。这里只排队，等人点，不代答。
        st.pending_prompt_name = st
            .peers
            .get(&invite.controller_id)
            .map(|p| p.snap.name.clone())
            .unwrap_or_else(|| hex_encode(&invite.controller_id[..2]));
        st.pending_prompt = Some(invite);
        return Vec::new();
    }
    println!(
        "node: incoming-invite session={} 输入 accept 或 reject",
        hex_encode(&invite.session_id[..4])
    );
    st.pending_in = Some(invite);
    Vec::new()
}

/// 界面线程调。返回要么是一个等人点的邀请，要么是已经被点击结果转换出来的待发帧。
///
/// 只接收调用方已经拿到的 State 和出站队列，锁由调用方持有：信令这一层
/// 不去反查 Shared，也就不会在别处再拿一次同一把锁。
pub(crate) fn st_take_prompt(st: &mut State, tx: &mpsc::UnboundedSender<Vec<u8>>) -> PromptStep {
    if let Some(answer) = st.prompt_answer.take() {
        let Some(invite) = st.pending_in.take() else {
            return PromptStep::Idle;
        };
        let replies = if answer {
            accept_invite(st, invite, tx)
        } else {
            send_decision(st, tx, &invite.session_id, false, Reason::Rejected);
            Vec::new()
        };
        return PromptStep::Replies(replies);
    }
    match st.pending_prompt.take() {
        Some(invite) => {
            let name = std::mem::take(&mut st.pending_prompt_name);
            PromptStep::Ask(invite, name)
        }
        None => PromptStep::Idle,
    }
}

pub(crate) enum PromptStep {
    Idle,
    Ask(Invite, String),
    Replies(Vec<(SocketAddr, Vec<u8>)>),
}

pub(super) fn on_decision(st: &mut State, decision: Decision, tx: &mpsc::UnboundedSender<Vec<u8>>) {
    if !decision.accept {
        send_state_diagnostic(st, tx, 7, decision.reason, Some(&decision.session_id));
        let text = decision
            .reason_enum()
            .map(Reason::user_text)
            .unwrap_or("对方拒绝了")
            .to_string();
        println!("node: invite-rejected {text}");
        if let Some(mut sess) = st.sessions.remove(&decision.session_id) {
            st.eph.remove(&decision.session_id);
            if let Some(wait) = sess.waiter.take() {
                let _ = wait.send(Err(text));
            }
        }
        st.session_active = st.sessions.values().any(|_| true);
        return;
    }
    send_ephemeral(st, tx, &decision.session_id);
    send_candidates(st, tx, &decision.session_id);
    arm_hello(st, decision.session_id);
    send_heartbeat_locked(st, tx);
}

pub(super) fn on_ephemeral(st: &mut State, eph: Ephemeral, tx: &mpsc::UnboundedSender<Vec<u8>>) -> Vec<(SocketAddr, Vec<u8>)> {
    let Some(sess) = st.sessions.get(&eph.session_id) else {
        return Vec::new();
    };
    if eph.sender_id != sess.peer {
        return Vec::new();
    }
    let expect_role = if sess.role_controller { 2 } else { 1 };
    if eph.role != expect_role {
        return Vec::new();
    }
    let msg = ephemeral_message(&eph.session_id, eph.role, &eph.eph_pub, &st.identity.device_id());
    if verify(&eph.sender_id, &msg, &eph.sig).is_err() {
        println!("node: ephemeral-rejected");
        fail_session(st, eph.session_id, "签名无效");
        return Vec::new();
    }
    let Some(private) = st.eph.remove(&eph.session_id) else {
        return Vec::new();
    };
    let shared = match private.agree(&eph.eph_pub) {
        Ok(s) => s,
        Err(_) => {
            fail_session(st, eph.session_id, "密钥协商失败");
            return Vec::new();
        }
    };
    let me = st.identity.device_id();
    let Some(sess) = st.sessions.get_mut(&eph.session_id) else {
        return Vec::new();
    };
    sess.peer_pub = Some(eph.eph_pub);
    sess.peer_sig = Some(eph.sig);
    let (c_id, h_id, c_pub, h_pub, c_sig, h_sig) = if sess.role_controller {
        (me, sess.peer, sess.my_pub, eph.eph_pub, sess.my_sig, eph.sig)
    } else {
        (sess.peer, me, eph.eph_pub, sess.my_pub, eph.sig, sess.my_sig)
    };
    match SessionKeys::derive(&shared, &eph.session_id, &c_id, &h_id, &c_pub, &h_pub, &c_sig, &h_sig) {
        Ok(keys) => {
            // 日志只在 HelloAck 之后打指纹。这里不打印密钥。
            sess.keys = Some(keys);
        }
        Err(_) => {
            fail_session(st, eph.session_id, "密钥派生失败");
            return Vec::new();
        }
    }
    let buffered = std::mem::take(&mut st.pending_dg);
    let mut replies = maybe_hello(st, eph.session_id);
    for (addr, bytes) in buffered {
        replies.extend(on_datagram(st, addr, &bytes, tx));
    }
    replies
}

pub(super) fn on_candidates(st: &mut State, c: Candidates) -> Vec<(SocketAddr, Vec<u8>)> {
    if st.force_relay {
        return maybe_hello(st, c.session_id);
    }
    let relay = st.relay.as_ref().map(|r| r.addr);
    let Some(sess) = st.sessions.get_mut(&c.session_id) else {
        return Vec::new();
    };
    sess.candidates = c.addrs.iter().filter_map(sock_of).collect();
    let chosen = pick_direct(&sess.candidates);
    if let Some(addr) = chosen {
        // 已经验证过的直连地址不要被后来的一份候选表覆盖。
        match sess.direct_addr {
            None => sess.direct_addr = Some(addr),
            Some(old) if old.ip().is_loopback() && !addr.ip().is_loopback() => {
                sess.direct_addr = Some(addr)
            }
            _ => {}
        }
        // 未知或中继状态下，新到的直连候选可以立刻把路切回直连；
        // 已经在直连上时保留当前路径，不要被后来的一份候选表随意覆盖。
        if sess.path != PATH_DIRECT {
            set_peer_route(sess, addr, relay, false);
        }
    }
    maybe_hello(st, c.session_id)
}

pub(super) fn store_ticket(st: &mut State, t: RelayTicket) -> Vec<(SocketAddr, Vec<u8>)> {
    let Some(addr) = parse_relay(&t.relay_host, t.relay_port) else {
        println!("node: ticket host 不是 IPv4，当前不能走中继");
        return Vec::new();
    };
    st.relay = Some(RelayEndpoint { token: t.routing_token, addr });
    println!(
        "node: ticket prefix={} port={}",
        hex_encode(&t.routing_token[..4]),
        t.relay_port
    );
    if st.force_relay {
        for sess in st.sessions.values_mut() {
            if !sess.ready {
                sess.peer_addr = Some(addr);
                sess.path = PATH_RELAY;
            }
        }
    }
    let sids: Vec<[u8; 16]> = st.sessions.keys().copied().collect();
    sids.into_iter().flat_map(|sid| maybe_hello(st, sid)).collect()
}

pub(crate) fn parse_relay(host: &str, port: u16) -> Option<SocketAddr> {
    let host = host.trim().trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Some(SocketAddr::new(ip, port));
    }
    (host, port).to_socket_addrs().ok()?.next()
}
