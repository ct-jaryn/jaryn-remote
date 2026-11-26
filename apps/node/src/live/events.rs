use crate::live::now_us;
use jr_proto::Reason;
use jr_session::{Engine, SessionEvent};

use super::capabilities::displays_of;
use super::terminal::spawn_shell;
use crate::live::bitrate_of;
use crate::State;

pub fn release(st: &mut State, reason: Reason) {
    // 先把引擎摘出来再收尾。`close()` 会吐出 `Closed`，如果此时引擎还在
    // `st.engine` 里，`apply_events` 会再走一次 `finish_hangup`。
    if let Some(mut engine) = st.engine.take() {
        let events = engine.close(reason);
        apply_events(st, events);
    }
    // 兜底：引擎可能已经被摘掉，或者异常路径没能吐出 PrivacyOff/ReleaseAll。
    // 软件遮挡层留在屏幕上会让机器前的人看不到桌面，下一场采集也会被误静音，
    // 所以这里必须再确认一次。正常收尾时上面的 apply_events 已经把标志清零，
    // 不会重复撤遮挡。
    if st.privacy_cover {
        crate::av::privacy_cover(false);
        st.privacy_cover = false;
    }
    Engine::write_recovery(&st.data_dir.join("recovery"));
    st.capture_on = false;
    Engine::clear_recovery(&st.data_dir.join("recovery"));
    crate::inject::release_os(st.os_inject);
    crate::av::stop_capture();
    crate::av::stop_audio_capture();
    crate::av::reset_audio();
    crate::av::reset_view();
    crate::av::indicator(false);
    st.indicating = false;
    reset_capture_state(st);
    // 下一个主控是新的会话，不能拿上一个会话的“没变化”去重。
    st.last_cursor = None;
    st.last_cursor_shape = None;
    if let Some(mut pty) = st.term_pty.take() {
        pty.kill();
    }
    st.term_rx = None;
}

/// 采集这一侧的状态归零。换显示器、收到 StreamStop 和会话收尾都要用它，
/// 三处各写一遍很容易漏掉某一项，漏掉就会让下一次启动被旧失败标记挡住。
pub(super) fn reset_capture_state(st: &mut State) {
    st.capture_started = false;
    st.capture_announced = false;
    st.capture_failed = false;
    st.audio_started = false;
    st.audio_retries = 0;
    st.audio_retry_at_us = 0;
    st.capture_retries = 0;
    st.capture_retry_at_us = 0;
}

pub(super) fn apply_events(st: &mut State, events: Vec<SessionEvent>) {
    for ev in events {
        match ev {
            SessionEvent::Clipboard(text) => {
                println!("node: clipboard bytes={}", text.len());
                // 对面明确发过来的才写进本机剪贴板。写失败只报一句，不断会话。
                if st.os_clipboard {
                    if let Err(e) = crate::av::clipboard_set(&text) {
                        println!("node: 写入本机剪贴板失败 {e}");
                    }
                }
                st.last_clip = Some(text);
            }
            SessionEvent::PrivacyRejected => {
                let msg = "此设备不能隐藏本机画面".to_string();
                println!("node: {msg}");
                st.last_error = Some(msg);
            }
            SessionEvent::PrivacyDegraded => {
                let host = st.engine.as_ref().map(|e| !e.is_controller()).unwrap_or(false);
                if host {
                    crate::av::privacy_cover(true);
                    st.privacy_cover = true;
                }
                let msg = if host {
                    "已放上软件遮挡层：站在机器前只看到提示。这不是硬件隐私屏，截图和录屏不受影响，遮挡层也能被人点掉"
                } else {
                    "对方开了隐私屏，但只是软件遮挡：那台机器的物理屏幕会被盖住，不挡截图"
                };
                println!("node: {msg}");
                st.last_error = Some(msg.to_string());
            }
            SessionEvent::PrivacyOff => {
                if st.privacy_cover {
                    crate::av::privacy_cover(false);
                    st.privacy_cover = false;
                }
                println!("node: 隐私屏已撤");
            }
            SessionEvent::ReleaseAll => {
                if st.privacy_cover {
                    crate::av::privacy_cover(false);
                    st.privacy_cover = false;
                }
                Engine::write_recovery(&st.data_dir.join("recovery"));
                println!("node: release_all");
                st.capture_on = false;
            }
            SessionEvent::Inject(event) => match crate::inject::apply(st.os_inject, st.display, st.scale_milli, &event) {
                Ok(point) => {
                    st.last_point = Some(point);
                    st.inject_count = st.inject_count.saturating_add(1);
                }
                Err(e) => {
                    if st.last_error.as_deref() != Some(e.as_str()) {
                        println!("node: {e}");
                        st.last_error = Some(e);
                    }
                }
            },
            SessionEvent::Quality(profile) => {
                st.quality = profile;
                crate::av::set_view_quality(profile);
                if st.video_ready {
                    crate::av::set_bitrate(bitrate_of(profile));
                    println!("node: quality {profile}");
                } else {
                    println!("node: quality {profile} 已记下。当前没有编码器，画面不会变");
                }
            }
            SessionEvent::Bitrate { bps, fps } => {
                st.fps = fps;
                if st.capture_started {
                    crate::av::set_bitrate(bps.min(i32::MAX as u32) as i32);
                    crate::av::set_fps(fps);
                    if st.applied_bps != bps {
                        st.applied_bps = bps;
                        println!("node: 拥塞控制把码率调到 {bps} bps，帧率 {fps}");
                    }
                }
            }
            SessionEvent::Audio { timestamp_us, bytes } => {
                st.audio_frames = st.audio_frames.saturating_add(1);
                crate::av::play_audio(&bytes, timestamp_us);
            }
            SessionEvent::Video { idr, bytes } => {
                st.video_frames = st.video_frames.saturating_add(1);
                if st.view_open {
                    crate::av::present(&bytes, idr);
                }
                st.last_video = Some(bytes);
            }
            SessionEvent::RemoteCapability { video, width, height, displays } => {
                st.remote_video = video;
                st.remote_capability_seen = true;
                crate::send_state_diagnostic(st, &st.diag_tx, 10, u16::from(video), None);
                if width > 0 && height > 0 {
                    // 会话中途对面换了显示器或改了分辨率：观看窗口开着的话，
                    // 画面比例和鼠标换算要跟着换，不然画面会被拉伸、点会偏。
                    let resized = st.remote_w != width || st.remote_h != height;
                    st.remote_w = width;
                    st.remote_h = height;
                    if resized && st.view_open {
                        crate::av::request_view(width, height);
                    }
                }
                if !displays.is_empty() {
                    st.remote_displays = displays.clone();
                    let names: Vec<String> = displays
                        .iter()
                        .enumerate()
                        .map(|(i, (w, h))| format!("显示器 {} {}x{}", i + 1, w, h))
                        .collect();
                    crate::av::set_displays(&names);
                    crate::av::set_view_display(st.want_display);
                }
            }
            SessionEvent::StreamStart { display } => {
                st.want_display = display as usize;
                crate::av::set_view_display(st.want_display);
                if let Some(selected) = displays_of(st).get(st.want_display) {
                    st.display = jr_input::Display {
                        origin_x: selected.origin_x,
                        origin_y: selected.origin_y,
                        width: selected.width,
                        height: selected.height,
                    };
                    st.scale_milli = selected.scale_milli.max(1);
                }
                // 换显示器一律重来一次，不管上一次采集有没有起来。之前只在
                // capture_started 时清失败标记，采集中断后换到能用的显示器也
                // 卡在 capture_failed，整个会话不再出画面。
                if st.capture_display != st.want_display {
                    crate::av::stop_capture();
                    // 原生停采集会同时关掉这条流的音频输出，不把音频状态
                    // 一起清掉，下一拍就不会再 jrp_audio_start，整个会话静音。
                    crate::av::stop_audio_capture();
                    reset_capture_state(st);
                }
                st.remote_video = true;
                if st.remote_w == 0 || st.remote_h == 0 {
                    st.remote_w = 1280;
                    st.remote_h = 720;
                }
            }
            SessionEvent::StreamStop => {
                crate::av::stop_capture();
                crate::av::stop_audio_capture();
                reset_capture_state(st);
            }
            SessionEvent::FileDone { id, path } => {
                println!("node: file-done id={id} name={}", path.file_name().and_then(|s| s.to_str()).unwrap_or("file"));
                st.last_file = Some(path);
            }
            SessionEvent::FileFailed { id, why } => {
                println!("node: file-failed id={id} {why}");
                st.last_error = Some(why);
            }
            SessionEvent::Closed(reason) => {
                println!("node: session-closed {}", reason.user_text());
                // 引擎已经自己关闭了。先把引擎摘掉再走统一收尾，
                // 否则 release() 会再调一次 close()，而 close() 又会吐出 Closed，
                // 变成递归；同时也能立刻允许下一次邀请，不必等路静默超时。
                // release() 自己已经把引擎取走时，这里不要再收尾一次。
                if st.engine.take().is_some() {
                    crate::finish_hangup(st, reason);
                }
            }
            SessionEvent::TerminalData(bytes) => {
                if st.sessions.values().any(|s| s.ready && s.role_controller) {
                    st.term_buf.extend_from_slice(&bytes);
                } else if let Some(pty) = st.term_pty.as_mut() {
                    let _ = pty.write(&bytes);
                }
            }
            SessionEvent::TerminalResize { cols, rows } => {
                st.term_cols = cols.max(1);
                st.term_rows = rows.max(1);
                if let Some(pty) = st.term_pty.as_ref() {
                    pty.resize(st.term_cols, st.term_rows);
                    println!("node: terminal-resize {cols}x{rows}");
                }
            }
            SessionEvent::TerminalOpen { term, .. } => {
                println!("node: terminal-open");
                if st.sessions.values().any(|s| s.ready && !s.role_controller) {
                    spawn_shell(st, &term);
                }
            }
            SessionEvent::NeedKeyframe(id) => {
                let controller = st.sessions.values().any(|s| s.ready && s.role_controller);
                let now = now_us();
                if now.saturating_sub(st.last_kf_us) < 200_000 {
                    continue;
                }
                st.last_kf_us = now;
                if controller {
                    if let Some(engine) = st.engine.as_mut() {
                        let _ = engine.request_keyframe(id);
                    }
                } else {
                    crate::av::force_idr();
                }
            }
            SessionEvent::ReliableFailed => {
                println!("node: 可靠通道重传耗尽，结束会话");
                // 不要先把引擎摘掉：`finish_hangup` 最终走 `release`，只有让
                // `release` 拿到引擎并调用 `Engine::close()`，才能正常产生
                // `PrivacyOff`/`ReleaseAll`/`FileFailed`（撤遮挡、清半截文件）。
                // 这里和 `Closed` 分支保持一致，由 `release` 负责摘引擎；
                // `Closed` 事件回头看到引擎已经不在，也不会重复收尾。
                crate::finish_hangup(st, Reason::NoPath);
            }
            SessionEvent::Cursor(c) => {
                crate::av::set_cursor_pos(c.x, c.y, c.visible);
            }
            SessionEvent::CursorShape(shape) => {
                crate::av::set_cursor_shape(&shape);
            }
            SessionEvent::SendReliable { .. } => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jr_proto::{FileAction, MsgType};
    use std::collections::HashMap;
    use std::net::SocketAddr;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "jrp-node-events-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 只构造收尾需要的状态。没有真实设备、网络或 UI。
    fn test_state(dir: PathBuf, engine: Option<Engine>) -> State {
        let (diag_tx, _diag_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        State {
            diag_tx,
            signal_addr: "127.0.0.1:9".parse::<SocketAddr>().unwrap(),
            signal_connected: false,
            name: "test".into(),
            identity: jr_crypto::Identity::generate().unwrap(),
            short_id: "test".into(),
            peers: HashMap::new(),
            lan_from: HashMap::new(),
            sessions: HashMap::new(),
            eph: HashMap::new(),
            pending_in: None,
            pending_dg: Vec::new(),
            auto_accept: false,
            platform: jr_proto::PLATFORM_MACOS,
            grants_dir: dir.join("grants"),
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
            fleet_tag: [0u8; 8],
            private_mode: false,
            revoked: false,
            data_dir: dir,
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
            blocker: 0,
            engine,
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

    /// 可靠通道耗尽时不能先把引擎摘掉，否则 `Engine::close()` 永远不执行：
    /// 遮挡层不会撤、收到一半的文件也不会清。用真实的 `Engine`（只碰本地
    /// 临时目录，不走网络）验证收尾事件确实被处理了。
    #[test]
    fn reliable_failed_closes_engine_and_releases_file() {
        let dir = scratch();
        let inbox = dir.join("inbox");
        let mut engine = Engine::new(false, inbox.clone(), jr_proto::FEAT_FILE, false);
        let offer = FileAction::Offer {
            transfer_id: 7,
            direction: 0,
            name: "half.bin".into(),
            size: 16,
            sha256: [0u8; 32],
        }
        .encode()
        .unwrap();
        let ev = engine.on_payload(MsgType::FileControl as u8, 0, 0, 1, 0, 0, 0, &offer);
        assert!(ev.is_empty());
        let partial = inbox.join(".7.partial");
        assert!(partial.exists(), "Offer 之后应该留下半截临时文件");

        let mut st = test_state(dir, Some(engine));
        st.privacy_cover = true;

        apply_events(&mut st, vec![SessionEvent::ReliableFailed]);

        assert!(st.engine.is_none(), "会话结束后引擎必须被摘掉");
        assert!(!st.privacy_cover, "会话结束不能把软件遮挡层留在屏幕上");
        assert!(!partial.exists(), "Engine::close 必须走 drop_file 清掉半截文件");
        assert_eq!(st.last_error.as_deref(), Some("会话结束"));
    }

    /// 采集中断后对方换到另一块显示器：旧的失败标记必须清掉，否则
    /// `sync_capture` 会被 `capture_failed` 挡住，整个会话不再出画面。
    #[test]
    fn switching_display_clears_stale_capture_failure() {
        let dir = scratch();
        let mut st = test_state(dir, None);
        st.platform = jr_proto::PLATFORM_LINUX;
        st.capture_started = false;
        st.capture_failed = true;
        st.capture_retries = 2;
        st.capture_display = 0;

        apply_events(&mut st, vec![SessionEvent::StreamStart { display: 1 }]);

        assert_eq!(st.want_display, 1);
        assert!(!st.capture_failed, "换显示器必须清掉旧的采集失败标记");
        assert_eq!(st.capture_retries, 0);
        assert!(!st.capture_started);
    }

    /// 引擎已经被摘掉的异常路径也要撤遮挡，避免遮挡层留在屏幕上、
    /// 下一场会话的音频继续被 capture 丢掉。
    #[test]
    fn release_unsets_privacy_cover_without_engine() {
        let dir = scratch();
        let mut st = test_state(dir, None);
        st.privacy_cover = true;

        release(&mut st, Reason::NoPath);

        assert!(!st.privacy_cover);
    }
}
