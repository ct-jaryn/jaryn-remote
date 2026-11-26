use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use jr_crypto::{EphKey, Identity, SessionKeys};
use jr_proto::{DeviceSnap, Invite, Reason};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot};

use crate::pty;

pub(crate) struct Peer {
    pub(crate) snap: DeviceSnap,
    pub(crate) lan: bool,
}

pub(crate) struct RelayEndpoint {
    pub(crate) token: [u8; 16],
    pub(crate) addr: SocketAddr,
}

pub(crate) struct Sess {
    pub(crate) role_controller: bool,
    pub(crate) peer: [u8; 32],
    pub(crate) my_pub: [u8; 32],
    pub(crate) my_sig: [u8; 64],
    pub(crate) peer_pub: Option<[u8; 32]>,
    pub(crate) peer_sig: Option<[u8; 64]>,
    pub(crate) keys: Option<SessionKeys>,
    pub(crate) seq: u32,
    pub(crate) replay: jr_proto::ReplayWindow,
    pub(crate) peer_addr: Option<SocketAddr>,
    /// 对端通过信令列出来的全部候选。打洞要同时往每一个都发一颗，
    /// 只试第一个会在「公网地址打不通、局域网地址能通」这种常见情形下白等。
    pub(crate) candidates: Vec<SocketAddr>,
    /// 当前在用的路：0 未知，1 直连，2 中继。换路时用得上。
    pub(crate) path: u8,
    /// 直连候选地址。走中继的时候留着，直连恢复了可以切回去。
    pub(crate) direct_addr: Option<SocketAddr>,
    pub(crate) got_ack: bool,
    pub(crate) ready: bool,
    pub(crate) hello_armed: bool,
    pub(crate) hello_tries: u8,
    pub(crate) waiter: Option<oneshot::Sender<Result<String, String>>>,
}

pub(crate) struct State {
    pub(crate) diag_tx: mpsc::UnboundedSender<Vec<u8>>,
    pub(crate) signal_addr: SocketAddr,
    pub(crate) signal_connected: bool,
    pub(crate) name: String,
    pub(crate) identity: Identity,
    pub(crate) short_id: String,
    pub(crate) peers: HashMap<[u8; 32], Peer>,
    pub(crate) lan_from: HashMap<[u8; 32], (SocketAddr, u64)>,
    pub(crate) sessions: HashMap<[u8; 16], Sess>,
    pub(crate) eph: HashMap<[u8; 16], EphKey>,
    pub(crate) pending_in: Option<Invite>,
    pub(crate) pending_dg: Vec<(SocketAddr, Vec<u8>)>,
    pub(crate) auto_accept: bool,
    pub(crate) platform: u8,
    pub(crate) grants_dir: PathBuf,
    pub(crate) media_port: u16,
    pub(crate) advertise: Vec<Vec<u8>>,
    pub(crate) extra_candidate: Option<SocketAddr>,
    /// 信令看到的、本机媒体 socket 的公网地址。拿到之后会进候选列表。
    pub(crate) reflexive: Option<SocketAddr>,
    pub(crate) reflex_nonce: u64,
    pub(crate) reflex_port: u16,
    pub(crate) session_active: bool,
    /// 这次会话允许的能力位。0 表示旧对端没有声明，按不限制处理。
    pub(crate) session_features: u32,
    pub(crate) heartbeat_enabled: bool,
    pub(crate) last_fp: Option<String>,
    pub(crate) last_error: Option<String>,
    pub(crate) fleet_tag: [u8; 8],
    pub(crate) private_mode: bool,
    pub(crate) revoked: bool,
    pub(crate) data_dir: PathBuf,
    pub(crate) relay: Option<RelayEndpoint>,
    pub(crate) force_relay: bool,
    pub(crate) os_inject: bool,
    pub(crate) os_clipboard: bool,
    pub(crate) os_capture: bool,
    pub(crate) display: jr_input::Display,
    pub(crate) display_override: bool,
    pub(crate) scale_milli: u32,
    pub(crate) video_ready: bool,
    pub(crate) capture_started: bool,
    pub(crate) capture_announced: bool,
    pub(crate) capture_failed: bool,
    /// 系统回环采集是否已启动。音频和视频同起同停，但音频启动失败不能拖垮画面。
    pub(crate) audio_started: bool,
    /// 音频启动失败后的有限重试。和视频分开计数，失败一次不会整个会话都静音。
    pub(crate) audio_retries: u8,
    pub(crate) audio_retry_at_us: u64,
    /// 采集中途失败后的有限重启次数和下次尝试时间。会话结束会清零。
    pub(crate) capture_retries: u8,
    pub(crate) capture_retry_at_us: u64,
    pub(crate) capture_display: usize,
    pub(crate) quality: u8,
    pub(crate) fps: u16,
    pub(crate) applied_bps: u32,
    pub(crate) feedback_count: u32,
    pub(crate) rx_stats: jr_transport::RxStats,
    pub(crate) next_frame: u32,
    pub(crate) last_ping_us: u64,
    pub(crate) last_video: Option<Vec<u8>>,
    pub(crate) video_frames: u32,
    pub(crate) audio_frames: u32,
    pub(crate) remote_video: bool,
    pub(crate) remote_capability_seen: bool,
    pub(crate) remote_w: u32,
    pub(crate) remote_h: u32,
    pub(crate) remote_displays: Vec<(u32, u32)>,
    pub(crate) want_display: usize,
    pub(crate) view_open: bool,
    pub(crate) privacy_cover: bool,
    pub(crate) pending_hangup: Option<Reason>,
    pub(crate) last_kf_us: u64,
    pub(crate) last_cursor: Option<(i32, i32, bool)>,
    pub(crate) last_cursor_shape: Option<u64>,
    pub(crate) last_rx_us: u64,
    pub(crate) recovering: bool,
    pub(crate) recover_since_us: u64,
    pub(crate) indicating: bool,
    pub(crate) last_point: Option<(i32, i32)>,
    pub(crate) blocker: u8,
    pub(crate) engine: Option<jr_session::Engine>,
    pub(crate) last_clip: Option<String>,
    pub(crate) last_file: Option<PathBuf>,
    pub(crate) last_transfer: Option<u32>,
    pub(crate) inject_count: u32,
    pub(crate) capture_on: bool,
    pub(crate) term_buf: Vec<u8>,
    pub(crate) term_pty: Option<pty::Pty>,
    pub(crate) term_rx: Option<std::sync::mpsc::Receiver<Vec<u8>>>,
    pub(crate) term_cols: u16,
    pub(crate) term_rows: u16,
    pub(crate) ui_open: bool,
    pub(crate) autostart: bool,
    pub(crate) pending_prompt: Option<Invite>,
    pub(crate) pending_prompt_name: String,
    pub(crate) prompt_answer: Option<bool>,
}

pub(crate) struct Shared {
    pub(crate) state: Mutex<State>,
    pub(crate) udp: Arc<UdpSocket>,
    /// 测试用：媒体包先交给它，而不是直接发到对端。
    /// 用来在回环上插一个用户态 NAT，看打洞行为。
    pub(crate) media_hook: Mutex<Option<Arc<dyn Fn(&[u8], SocketAddr) + Send + Sync>>>,
    pub(crate) outbound: mpsc::UnboundedSender<Vec<u8>>,
    /// 信令的地址。反射探针要发到它的 UDP 端口上。
    pub(crate) signal_ip: IpAddr,
    /// 媒体 socket 是双栈时，发 IPv4 目标要先换成映射地址；退回 IPv4 时不能换。
    pub(crate) media_v6: bool,
}

/// 从媒体 socket 发一颗探针到信令的反射端口，换回自己的公网地址。
/// 必须用同一颗 socket，否则看到的端口不是媒体用的那个。

impl State {
    pub(crate) fn outbound_send(&self, tx: &mpsc::UnboundedSender<Vec<u8>>, frame: Vec<u8>) {
        let _ = tx.send(frame);
        let _ = self;
    }
}
