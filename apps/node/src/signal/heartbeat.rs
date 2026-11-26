use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;

use jr_proto::{encode_beacon, encode_frame, Beacon, Heartbeat, SignalType};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

use crate::net::media_dest;
use crate::{Shared, State};

use super::dispatch::LAN_FRESH_US;
use super::handshake::reflex_probe_bytes;

/// 从媒体 socket 发一颗探针到信令的反射端口，换回自己的公网地址。
/// 必须用同一颗 socket，否则看到的端口不是媒体用的那个。
pub(crate) fn reflex_probe(shared: &Arc<Shared>) -> u64 {
    let (nonce, port, ref_port) = {
        let mut st = shared.state.lock().unwrap();
        st.reflex_nonce = st.reflex_nonce.wrapping_add(1).max(1);
        (st.reflex_nonce, st.media_port, st.reflex_port)
    };
    if ref_port == 0 {
        return nonce;
    }
    let frame = match encode_frame(SignalType::ReflexiveAsk as u8, &nonce.to_le_bytes()) {
        Ok(f) => f,
        Err(_) => return nonce,
    };
    if shared.outbound.send(frame).is_err() {
        return nonce;
    }
    let target = SocketAddr::new(shared.signal_ip, ref_port);
    let probe = reflex_probe_bytes(nonce);
    let sock = shared.udp.clone();
    let dual = shared.media_v6;
    tokio::spawn(async move {
        let _ = sock.send_to(&probe, media_dest(target, dual)).await;
        let _ = port;
    });
    nonce
}

/// 每过一会儿重新问一次自己的公网地址。地址会因为 NAT 重绑定而变。
pub(crate) const REFLEX_EVERY_TICKS: u32 = 1000;

pub(crate) fn send_heartbeat(shared: &Shared) {
    let st = shared.state.lock().unwrap();
    if !st.heartbeat_enabled {
        return;
    }
    send_heartbeat_locked(&st, &shared.outbound);
}

pub(crate) fn send_heartbeat_locked(st: &State, tx: &mpsc::UnboundedSender<Vec<u8>>) {
    if !st.heartbeat_enabled {
        return;
    }
    let heard: Vec<[u8; 32]> = {
        let now = crate::live::now_us();
        st.lan_from
            .iter()
            .filter(|(_, (_, seen))| now.saturating_sub(*seen) <= LAN_FRESH_US)
            .map(|(id, _)| *id)
            .take(64)
            .collect()
    };
    let hb = Heartbeat {
        blocker: st.blocker,
        session_active: st.session_active,
        heard,
    };
    if let Ok(body) = hb.encode() {
        if let Ok(frame) = encode_frame(SignalType::Heartbeat as u8, &body) {
            let _ = tx.send(frame);
        }
    }
}

pub(crate) fn send_wol(mac: [u8; 6]) {
    let pkt = jr_proto::magic_packet(mac);
    if let Ok(sock) = std::net::UdpSocket::bind("0.0.0.0:0") {
        let _ = sock.set_broadcast(true);
        let _ = sock.send_to(&pkt, "255.255.255.255:9");
        println!("node: wol sent");
    }
}

pub(crate) fn note_beacon(shared: &Shared, buf: &[u8], src: SocketAddr) {
    let Ok(beacon) = jr_proto::decode_beacon(buf) else {
        return;
    };
    let mut st = shared.state.lock().unwrap();
    if beacon.device_id == st.identity.device_id() {
        return;
    }
    if beacon.fleet_tag != st.fleet_tag {
        return;
    }
    let _ = beacon.tcp_port;
    st.lan_from.insert(beacon.device_id, (src, crate::live::now_us()));
    if let Some(peer) = st.peers.get_mut(&beacon.device_id) {
        if !peer.lan {
            peer.lan = true;
            println!("node: lan short={}", peer.snap.short_id);
        }
    }
}

pub(crate) async fn send_beacon(shared: &Shared, sock: &UdpSocket, port: u16) {
    let (id, tag) = {
        let st = shared.state.lock().unwrap();
        (st.identity.device_id(), st.fleet_tag)
    };
    let pkt = encode_beacon(&Beacon {
        device_id: id,
        fleet_tag: tag,
        tcp_port: 0,
    });
    let group = SocketAddr::from(SocketAddrV4::new(Ipv4Addr::new(239, 255, 42, 210), port));
    let bcast = SocketAddr::from(SocketAddrV4::new(Ipv4Addr::BROADCAST, port));
    let local = SocketAddr::from(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port));
    let _ = sock.send_to(&pkt, group).await;
    let _ = sock.send_to(&pkt, bcast).await;
    let _ = sock.send_to(&pkt, local).await;
}

pub(crate) fn bind_beacon(port: u16) -> Result<UdpSocket, String> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).map_err(|e| e.to_string())?;
    socket.set_reuse_address(true).map_err(|e| e.to_string())?;
    // Windows 上没有「同一端口被两个进程绑定」这件事，自然也没有这个开关。
    #[cfg(unix)]
    socket.set_reuse_port(true).map_err(|e| e.to_string())?;
    socket.set_nonblocking(true).map_err(|e| e.to_string())?;
    socket
        .bind(&SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port).into())
        .map_err(|e| e.to_string())?;
    socket.set_broadcast(true).ok();
    let group = Ipv4Addr::new(239, 255, 42, 210);
    socket.join_multicast_v4(&group, &Ipv4Addr::UNSPECIFIED).ok();
    socket.set_multicast_loop_v4(true).ok();
    let std_sock: std::net::UdpSocket = socket.into();
    UdpSocket::from_std(std_sock).map_err(|e| e.to_string())
}
