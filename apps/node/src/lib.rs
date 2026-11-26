//! 命令行节点。核心 crate 禁止 unsafe；系统注入只在 inject.rs 里调用公开的辅助功能和事件接口。

use std::net::SocketAddr;

use jr_proto::{
    encode_frame, hex_encode, DeviceSnap, Diagnostic, SignalType,
    FEAT_AUDIO_OUT, FEAT_CLIPBOARD, FEAT_FILE, FEAT_INPUT, FEAT_PRIVACY, FEAT_TERMINAL, FEAT_VIDEO,
};
use tokio::sync::mpsc;

mod autostart;
mod av;
mod config;
mod desk;
mod inject;
mod pty;
mod ui;

#[cfg(target_os = "macos")]
pub use ui::Setup as MacSetup;

#[cfg(target_os = "macos")]
pub fn mac_setup(default_signal: &str, default_name: &str) -> Option<MacSetup> {
    ui::setup(default_signal, default_name)
}

#[cfg(target_os = "macos")]
pub fn mac_error(title: &str, message: &str) {
    ui::error(title, message);
}

#[cfg(target_os = "macos")]
pub fn mac_quit_requested() -> bool {
    ui::quit_requested()
}

#[cfg(windows)]
pub fn windows_quit_requested() -> bool {
    ui::quit_requested()
}

pub fn request_permission() {
    inject::request_permission();
}

pub fn pump_ui() {
    av::pump_ui();
}
mod live;
mod net;
mod node;
mod signal;
mod state;
mod tls;

pub use net::{load_enrollment, save_enrollment};
use net::{advertise_ips, bind_media_socket, load_identity, media_dest, media_src};
use signal::{apply_frame, finish_hangup, maybe_hello, on_datagram, on_signal, path_silence_step, status_word, PATH_DIRECT, PATH_RELAY};
#[allow(unused_imports)]
pub(crate) use signal::{
    bind_beacon, make_enroll_frame, note_beacon, pairing_message, read_challenge, read_frame,
    read_frame_split, reconnect_signal, reflex_probe, send_beacon,
    send_heartbeat, send_heartbeat_locked, send_wol, LAN_FRESH_US, REFLEX_EVERY_TICKS,
};

pub use config::{DeviceView, NodeConfig};
pub use node::{Node, NodeHandle};
pub(crate) use state::{Peer, RelayEndpoint, Sess, Shared, State};

const HELLO_BODY: usize = 145;
const HELLO_ACK_BODY: usize = 21;

/// 主控端会用的能力。邀请时把这份清单发给被控端，无人值守票据按它校验。
const CONTROLLER_FEATURES: u32 =
    FEAT_VIDEO | FEAT_INPUT | FEAT_CLIPBOARD | FEAT_FILE | FEAT_TERMINAL | FEAT_PRIVACY | FEAT_AUDIO_OUT;

/// 舰队令牌和信令指纹落盘，权限 0600。
///
/// 之前每次启动都要把令牌敲在命令行上，任何本机进程 `ps` 一下就看见了。
/// 存进数据目录并复用，比每次从命令行传更安全。这里只落盘，不打印。
fn find_short(st: &State, short_id: &str) -> Result<DeviceSnap, String> {
    let found: Vec<_> = st
        .peers
        .values()
        .filter(|p| p.snap.short_id.eq_ignore_ascii_case(short_id))
        .map(|p| p.snap.clone())
        .collect();
    match found.len() {
        1 => Ok(found.into_iter().next().unwrap()),
        0 => Err("没有这台设备".into()),
        _ => Err("短号冲突，用完整公钥区分。这一版列表里不该出现".into()),
    }
}

fn send_diagnostic(tx: &mpsc::UnboundedSender<Vec<u8>>, event: u8, value: u16,
    session_id: Option<&[u8; 16]>, peer_short: &str) {
    let mut session = [0u8; 4];
    if let Some(sid) = session_id { session.copy_from_slice(&sid[..4]); }
    let mut peer = [0u8; 8];
    for (dst, src) in peer.iter_mut().zip(peer_short.bytes()) { *dst = src; }
    let diag = Diagnostic { event, value, session, peer };
    let body = diag.encode();
    let queued = encode_frame(SignalType::Diagnostic as u8, &body)
        .ok().is_some_and(|frame| tx.send(frame).is_ok());
    println!("node: diag event={} value={} session={} peer={} queued={}",
        diag.label(), value, hex_encode(&session), peer_short, queued);
}

fn send_state_diagnostic(st: &State, tx: &mpsc::UnboundedSender<Vec<u8>>,
    event: u8, value: u16, sid: Option<&[u8; 16]>) {
    let sid = sid.or_else(|| st.sessions.keys().next());
    let peer = sid.and_then(|id| st.sessions.get(id))
        .and_then(|s| st.peers.get(&s.peer))
        .map(|p| p.snap.short_id.as_str()).unwrap_or("");
    send_diagnostic(tx, event, value, sid, peer);
}

fn send_status_diagnostic(shared: &Shared) {
    let st = shared.state.lock().unwrap();
    let mut flags = 0u16;
    if st.session_active { flags |= 1; }
    if st.engine.is_some() { flags |= 2; }
    if st.video_ready { flags |= 4; }
    if st.os_capture && av::screen_granted() { flags |= 8; }
    if st.view_open { flags |= 16; }
    if st.capture_started { flags |= 32; }
    if st.remote_video { flags |= 64; }
    if st.os_inject && inject::trusted() { flags |= 128; }
    send_state_diagnostic(&st, &shared.outbound, 19, flags, None);
}

fn invite_error_code(text: &str) -> u16 {
    if text.contains("没有这台设备") { 1 }
    else if text.contains("离线") || text.contains("已断开") { 2 }
    else if text.contains("正在被另一台设备控制") { 3 }
    else if text.contains("超时") { 4 }
    else if text.contains("信令") { 5 }
    else { 0 }
}

async fn send_replies(shared: &Shared, replies: Vec<(SocketAddr, Vec<u8>)>) {
    let hook = shared.media_hook.lock().unwrap().clone();
    for (addr, pkt) in replies {
        if let Some(hook) = &hook {
            hook(&pkt, addr);
        } else {
            let _ = shared.udp.send_to(&pkt, media_dest(addr, shared.media_v6)).await;
        }
    }
}

#[cfg(test)]
mod net_tests {
    use super::*;
    use crate::signal::{parse_relay, sock_of};
    use jr_proto::SockAddr;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddrV4, SocketAddrV6};

    #[test]
    fn v4_roundtrips_through_mapped_form() {
        let v4 = SocketAddr::from(SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 9), 7000));
        assert_eq!(media_src(media_dest(v4, true)), v4);
        assert_eq!(media_dest(v4, false), v4);
    }

    #[test]
    fn v6_is_left_alone() {
        let v6 = SocketAddr::from(SocketAddrV6::new(Ipv6Addr::LOCALHOST, 7000, 0, 0));
        assert_eq!(media_src(media_dest(v6, true)), v6);
    }

    #[test]
    fn candidate_accepts_four_and_sixteen_byte_addresses() {
        let v4 = sock_of(&SockAddr { path_id: 0, ip: vec![10, 0, 0, 2], port: 9 }).unwrap();
        assert_eq!(v4, SocketAddr::from(SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 2), 9)));
        let oct = Ipv6Addr::LOCALHOST.octets().to_vec();
        let v6 = sock_of(&SockAddr { path_id: 0, ip: oct, port: 9 }).unwrap();
        assert_eq!(v6, SocketAddr::from(SocketAddrV6::new(Ipv6Addr::LOCALHOST, 9, 0, 0)));
        assert!(sock_of(&SockAddr { path_id: 0, ip: vec![1, 2, 3], port: 9 }).is_none());
    }

    #[test]
    fn relay_accepts_ipv6_and_brackets() {
        let want = SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 47211);
        assert_eq!(parse_relay("::1", 47211), Some(want));
        assert_eq!(parse_relay("[::1]", 47211), Some(want));
    }

    #[test]
    fn controller_advertises_audio_so_received_frames_are_not_dropped() {
        // 主控 Engine 的 features 会用这份清单和邀请申请位取交集；
        // 少了 audio 位会让 jr-session 把每一帧声音都静默丢掉。
        assert_ne!(CONTROLLER_FEATURES & FEAT_AUDIO_OUT, 0);
    }
}
