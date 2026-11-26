use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use jr_proto::{Reason, SockAddr};

use crate::{send_heartbeat_locked, send_state_diagnostic, State};

use super::handshake::finish_sessions;

pub(crate) const PATH_DIRECT: u8 = 1;
pub(crate) const PATH_RELAY: u8 = 2;

pub(super) fn pick_direct(addrs: &[SocketAddr]) -> Option<SocketAddr> {
    // 每个节点都会把自己看到的 127.0.0.1 放进候选；对远端来说那是它自己。
    // 优先挑真实地址，只有全是回环时才退回第一项（单机测试用）。
    addrs
        .iter()
        .copied()
        .find(|a| !a.ip().is_loopback() && !a.ip().is_unspecified())
        .or_else(|| addrs.first().copied())
}

pub(super) fn routing_token(st: &State) -> [u8; 16] {
    st.relay.as_ref().map(|r| r.token).unwrap_or([0u8; 16])
}

/// 一条路静默了先换路，换不了就等一小会儿看能不能恢复，最后才报「直连和中继都失败」。
/// 返回 Some 表示该结束了，原因由调用方带给用户。
pub(crate) fn path_silence_step(st: &mut State) -> Option<Reason> {
    const SILENCE_US: u64 = 5_000_000;
    const RECOVER_US: u64 = 2_000_000;
    let ready = st.sessions.values().any(|s| s.ready);
    if !ready || st.last_rx_us == 0 {
        return None;
    }
    let silent = crate::live::now_us().saturating_sub(st.last_rx_us);
    if silent < SILENCE_US {
        st.recovering = false;
        return None;
    }
    if !st.recovering {
        if switch_path(st) {
            st.last_rx_us = crate::live::now_us();
            return None;
        }
        st.recovering = true;
        st.recover_since_us = crate::live::now_us();
        println!("node: 路静默，等一小会儿看能不能恢复");
        return None;
    }
    if crate::live::now_us().saturating_sub(st.recover_since_us) > RECOVER_US {
        return Some(Reason::NoPath);
    }
    None
}

/// 静默时换个方向试。直连不通走中继，中继不通回直连。
pub(super) fn switch_path(st: &mut State) -> bool {
    let relay = st.relay.as_ref().map(|r| r.addr);
    let Some(sess) = st.sessions.values_mut().find(|s| s.ready) else {
        return false;
    };
    match sess.path {
        PATH_RELAY => {
            // 中继静默：有直连候选就切回直连。
            let Some(direct) = sess.direct_addr else {
                return false;
            };
            sess.peer_addr = Some(direct);
            sess.path = PATH_DIRECT;
            println!("node: 中继静默，改走直连");
        }
        _ => {
            let Some(relay) = relay else {
                return false;
            };
            if sess.peer_addr == Some(relay) {
                return false;
            }
            sess.peer_addr = Some(relay);
            sess.path = PATH_RELAY;
            println!("node: 直连静默，改走中继");
        }
    }
    true
}

pub(crate) fn finish_hangup(st: &mut State, reason: Reason) {
    send_state_diagnostic(st, &st.diag_tx, 15, reason as u16, None);
    crate::live::release(st, reason);
    st.engine = None;
    finish_sessions(st, Err(reason.user_text().into()));
    st.session_active = false;
    st.session_features = 0;
    st.remote_video = false;
    st.remote_capability_seen = false;
    st.view_open = false;
    st.indicating = false;
    crate::av::indicator(false);
    // 信令靠心跳里的 session_active 判断忙闲。挂断后立刻补一拍，
    // 否则用户紧接着重新邀请时会撞上“对方正在被另一台设备控制”。
    let tx = st.diag_tx.clone();
    send_heartbeat_locked(st, &tx);
    st.last_fp = None;
}

pub(crate) fn sock_of(addr: &SockAddr) -> Option<SocketAddr> {
    let ip = match addr.ip.len() {
        4 => IpAddr::V4(Ipv4Addr::new(addr.ip[0], addr.ip[1], addr.ip[2], addr.ip[3])),
        16 => {
            let mut oct = [0u8; 16];
            oct.copy_from_slice(&addr.ip);
            IpAddr::V6(Ipv6Addr::from(oct))
        }
        _ => return None,
    };
    Some(SocketAddr::new(ip, addr.port))
}
