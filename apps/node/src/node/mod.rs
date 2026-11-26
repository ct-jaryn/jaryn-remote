//! 节点本身：启动时完成注册，再把信令、心跳、诊断、发现和媒体各条循环挂到 tokio 上。

mod commands;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use jr_crypto::fleet_tag;
use jr_proto::{hex_encode, EnrollOk, Reason, SignalType, JRP_VERSION, PLATFORM_IOS};
use rustls::pki_types::ServerName;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::{mpsc, watch};
use tokio_rustls::TlsConnector;

use crate::live;
use crate::tls;
use crate::{
    advertise_ips, apply_frame, autostart, bind_beacon, bind_media_socket, finish_hangup,
    load_identity, make_enroll_frame, maybe_hello, media_src, note_beacon, on_datagram, on_signal,
    pairing_message, read_challenge, read_frame, read_frame_split, reconnect_signal,
    reflex_probe, send_beacon, send_diagnostic, send_heartbeat, send_replies,
    send_status_diagnostic, NodeConfig, Shared, State, REFLEX_EVERY_TICKS,
};

pub struct Node {
    inner: NodeHandle,
    shutdown: watch::Sender<bool>,
}

impl std::ops::Deref for Node {
    type Target = NodeHandle;

    fn deref(&self) -> &NodeHandle {
        &self.inner
    }
}

/// 可以搬到别的线程用的节点句柄。界面在自己的线程上操作它时，异步动作回到 tokio 运行时。
#[derive(Clone)]
pub struct NodeHandle {
    pub short_id: String,
    shared: Arc<Shared>,
    handle: tokio::runtime::Handle,
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
    }
}

impl Node {
    pub async fn start(cfg: NodeConfig) -> Result<Self, String> {
        if cfg.name.is_empty() || cfg.name.len() > 64 {
            return Err("设备名要在 1 到 64 字节".into());
        }
        if cfg.platform == PLATFORM_IOS {
            println!("node: 本机是 iOS，只能当主控");
        }
        std::fs::create_dir_all(&cfg.data_dir).map_err(|e| e.to_string())?;
        let identity = load_identity(&cfg.data_dir)?;
        let (udp_std, media_v6) = bind_media_socket().map_err(|e| e.to_string())?;
        let udp = Arc::new(UdpSocket::from_std(udp_std).map_err(|e| e.to_string())?);
        let media_port = udp.local_addr().map_err(|e| e.to_string())?.port();
        let connector = TlsConnector::from(tls::client_config(cfg.pin)?);
        let tcp = TcpStream::connect(cfg.signal)
            .await
            .map_err(|e| format!("信令连不上: {e}"))?;
        tcp.set_nodelay(true).map_err(|e| e.to_string())?;
        let name = ServerName::try_from("localhost").map_err(|e| e.to_string())?;
        let mut tls = connector
            .connect(name, tcp)
            .await
            .map_err(|e| format!("服务器身份变了或 TLS 失败: {e}"))?;
        let device_id = identity.device_id();
        let mut buf = Vec::new();
        let private_mode = cfg.fleet_token == [0; 32];
        let enroll_frame = make_enroll_frame(&identity, &cfg, cfg.fleet_token)?;
        let first_enroll = if private_mode {
            let challenge = read_frame(&mut tls, &mut buf).await.map_err(|_| "没有收到连接挑战".to_string())?;
            let nonce = read_challenge(&challenge)?;
            make_enroll_frame(&identity, &cfg, nonce)?
        } else {
            enroll_frame.clone()
        };
        tls.write_all(&first_enroll).await.map_err(|e| e.to_string())?;
        let first = read_frame(&mut tls, &mut buf).await.map_err(|_| {
            "注册被拒绝。令牌错误或设备已撤销".to_string()
        })?;
        if first.msg_type == SignalType::PairPending as u8 {
            return Err(pairing_message(&first.body));
        }
        if first.msg_type != SignalType::EnrollOk as u8 {
            return Err("注册被拒绝".into());
        }
        let ok = EnrollOk::decode(&first.body).map_err(|e| e.to_string())?;
        if ok.version != JRP_VERSION {
            return Err(Reason::VersionMismatch.user_text().into());
        }
        let (tx, rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let mut state = State {
            diag_tx: tx.clone(),
            signal_addr: cfg.signal,
            signal_connected: true,
            name: cfg.name.clone(),
            identity,
            short_id: ok.short_id.clone(),
            peers: HashMap::new(),
            lan_from: HashMap::new(),
            sessions: HashMap::new(),
            eph: HashMap::new(),
            pending_in: None,
            pending_dg: Vec::new(),
            auto_accept: cfg.auto_accept,
            platform: cfg.platform,
            grants_dir: cfg.data_dir.join("grants"),
            media_port,
            advertise: cfg
                .advertise_override
                .clone()
                .map(|list| list.into_iter().map(|ip| ip.to_vec()).collect())
                .unwrap_or_else(|| advertise_ips(media_v6)),
            extra_candidate: cfg.extra_candidate,
            reflexive: None,
            reflex_nonce: 0,
            reflex_port: cfg.reflex_port,
            session_active: false,
            session_features: 0,
            heartbeat_enabled: true,
            last_fp: None,
            last_error: None,
            fleet_tag: fleet_tag(if private_mode { &cfg.pin } else { &cfg.fleet_token }),
            private_mode,
            revoked: false,
            data_dir: cfg.data_dir.clone(),
            relay: None,
            force_relay: cfg.force_relay,
            os_inject: cfg.os_inject,
            os_clipboard: cfg.os_clipboard,
            os_capture: cfg.os_capture,
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
            remote_w: 1280,
            remote_h: 720,
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
            blocker: jr_session::host_profile(cfg.platform).blocker,
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
            autostart: autostart::enabled(&cfg.data_dir),
            pending_prompt: None,
            pending_prompt_name: String::new(),
            prompt_answer: None,
        };
        live::note_recovery(&cfg.data_dir.join("recovery"));
        std::fs::create_dir_all(&state.grants_dir).map_err(|e| e.to_string())?;
        if let Ok(frame) = read_frame(&mut tls, &mut buf).await {
            apply_frame(&mut state, &frame, &tx);
        }
        println!(
            "node: enrolled name={} short={} id={}",
            cfg.name,
            ok.short_id,
            hex_encode(&device_id[..4])
        );
        let shared = Arc::new(Shared {
            state: Mutex::new(state),
            udp,
            media_hook: Mutex::new(None),
            outbound: tx,
            signal_ip: cfg.signal.ip(),
            media_v6,
        });
        send_diagnostic(&shared.outbound, 1, 0, None, "");
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let signal_shared = shared.clone();
        let mut signal_shutdown = shutdown_rx.clone();
        let reconnect_cfg = cfg.clone();
        tokio::spawn(async move {
            let mut rx = rx;
            let mut current = Some((tls, buf));
            loop {
                let (conn, mut read_buf) = if let Some(initial) = current.take() {
                    initial
                } else {
                    // 掉线期间的心跳和会话控制包已经过期，不能在重连后补发。
                    while rx.try_recv().is_ok() {}
                    let reconnect = tokio::time::timeout(
                        Duration::from_secs(8),
                        reconnect_signal(&reconnect_cfg, &connector, &enroll_frame, &signal_shared),
                    ).await;
                    match reconnect {
                        Ok(Ok((conn, read_buf, snapshot))) => {
                            while rx.try_recv().is_ok() {}
                            let replies = {
                                let mut st = signal_shared.state.lock().unwrap();
                                st.signal_connected = true;
                                on_signal(&mut st, &snapshot, &signal_shared.outbound)
                            };
                            send_replies(&signal_shared, replies).await;
                            println!("node: 信令已重连");
                            send_diagnostic(&signal_shared.outbound, 18, 0, None, "");
                            send_diagnostic(&signal_shared.outbound, 17, 0, None, "");
                            (conn, read_buf)
                        }
                        _ => {
                            tokio::select! {
                                changed = signal_shutdown.changed() => {
                                    if changed.is_err() || *signal_shutdown.borrow() { return; }
                                }
                                _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                            }
                            continue;
                        }
                    }
                };
                let (mut read_half, mut write_half) = tokio::io::split(conn);
                loop {
                    tokio::select! {
                        changed = signal_shutdown.changed() => {
                            if changed.is_err() || *signal_shutdown.borrow() { return; }
                        }
                        msg = rx.recv() => {
                            let Some(frame) = msg else { return; };
                            if write_half.write_all(&frame).await.is_err() { break; }
                        }
                        frame = read_frame_split(&mut read_half, &mut read_buf) => {
                            let Ok(frame) = frame else { break; };
                            let replies = {
                                let mut st = signal_shared.state.lock().unwrap();
                                on_signal(&mut st, &frame, &signal_shared.outbound)
                            };
                            send_replies(&signal_shared, replies).await;
                        }
                    }
                }
                {
                    let mut st = signal_shared.state.lock().unwrap();
                    if st.session_active { finish_hangup(&mut st, Reason::SessionDead); }
                    st.signal_connected = false;
                    for peer in st.peers.values_mut() { peer.snap.online = false; }
                    st.last_error = Some("信令断开，正在重连".into());
                }
                println!("node: 信令断开，正在重连");
            }
        });
        let beat_shared = shared.clone();
        let mut beat_shutdown = shutdown_rx.clone();
        let every = cfg.heartbeat.max(Duration::from_millis(50));
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(every);
            loop {
                tokio::select! {
                    changed = beat_shutdown.changed() => {
                        if changed.is_err() || *beat_shutdown.borrow() {
                            break;
                        }
                    }
                    _ = tick.tick() => send_heartbeat(&beat_shared),
                }
            }
        });
        let diag_shared = shared.clone();
        let mut diag_shutdown = shutdown_rx.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(30));
            loop {
                tokio::select! {
                    changed = diag_shutdown.changed() => {
                        if changed.is_err() || *diag_shutdown.borrow() { break; }
                    }
                    _ = tick.tick() => send_status_diagnostic(&diag_shared),
                }
            }
        });
        let hello_shared = shared.clone();
        let mut hello_shutdown = shutdown_rx.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(5));
            let mut hello_n = 0u32;
            loop {
                tokio::select! {
                    changed = hello_shutdown.changed() => {
                        if changed.is_err() || *hello_shutdown.borrow() {
                            break;
                        }
                    }
                    _ = tick.tick() => {
                        hello_n = hello_n.wrapping_add(1);
                        // 时不时问一次自己的公网地址，打洞用得上。
                        if hello_n % REFLEX_EVERY_TICKS == 1 {
                            reflex_probe(&hello_shared);
                        }
                        let replies = {
                            let mut st = hello_shared.state.lock().unwrap();
                            let mut out = Vec::new();
                            // 还没握上手就重发。打洞靠的就是这个节奏：
                            // 两边都得持续往对方的公网地址发，两边的 NAT 才会各自
                            // 开出一条能收的映射，早一步晚一步都不行。
                            if hello_n % 20 == 0 {
                                let ids: Vec<[u8; 16]> = st.sessions.keys().copied().collect();
                                for sid in ids {
                                    out.extend(maybe_hello(&mut st, sid));
                                }
                            }
                            out.extend(live::poll_media(&mut st));
                            if let Some(reason) = st.pending_hangup.take() {
                                finish_hangup(&mut st, reason);
                            }
                            out
                        };
                        send_replies(&hello_shared, replies).await;
                    }
                }
            }
        });
        let beacon_shared = shared.clone();
        let mut beacon_shutdown = shutdown_rx;
        let beacon_port = cfg.beacon_port;
        let beacon_every = cfg.beacon_every.max(Duration::from_millis(200));
        tokio::spawn(async move {
            let sock = match bind_beacon(beacon_port) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("node: beacon-bind {e}");
                    return;
                }
            };
            let mut tick = tokio::time::interval(beacon_every);
            let mut recv_buf = [0u8; 256];
            loop {
                tokio::select! {
                    changed = beacon_shutdown.changed() => {
                        if changed.is_err() || *beacon_shutdown.borrow() {
                            break;
                        }
                    }
                    _ = tick.tick() => send_beacon(&beacon_shared, &sock, beacon_port).await,
                    got = sock.recv_from(&mut recv_buf) => {
                        if let Ok((n, src)) = got {
                            note_beacon(&beacon_shared, &recv_buf[..n], src);
                        }
                    }
                }
            }
        });
        let udp_shared = shared.clone();
        let udp = shared.udp.clone();
        let mut udp_shutdown = shutdown_tx.subscribe();
        tokio::spawn(async move {
            let mut buf = [0u8; jr_proto::MAX_DATAGRAM];
            loop {
                tokio::select! {
                    changed = udp_shutdown.changed() => {
                        if changed.is_err() || *udp_shutdown.borrow() {
                            break;
                        }
                    }
                    got = udp.recv_from(&mut buf) => {
                        let (n, src) = match got {
                            Ok(pair) => pair,
                            // Windows 下向未监听端口发探测包会让下一次 recv_from 收到
                            // WSAECONNRESET (ConnectionReset)，不能因此退出整个收包循环。
                            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => continue,
                            Err(_) => break,
                        };
                        let src = media_src(src);
                        let replies = {
                            let mut st = udp_shared.state.lock().unwrap();
                            on_datagram(&mut st, src, &buf[..n], &udp_shared.outbound)
                        };
                        send_replies(&udp_shared, replies).await;
                    }
                }
            }
        });
        Ok(Self {
            inner: NodeHandle {
                short_id: ok.short_id,
                shared,
                handle: tokio::runtime::Handle::current(),
            },
            shutdown: shutdown_tx,
        })
    }
}
