use crate::live::bitrate_of;
use crate::live::now_us;
use jr_proto::Reason;

use super::capture_state::{
    after_audio_failure, after_capture_failure, may_start_audio, should_start_capture, Next,
};
use crate::State;

pub(super) fn sync_capture(st: &mut State) {
    let want = st.os_capture && st.engine.as_ref().map(|e| e.capture_on()).unwrap_or(false);
    if !want {
        // 采集可能已经自己停了，失败标记还在；也要走一次 stop 把它清掉，
        // 否则下一次会话一上来就会被当成“采集已失败”。
        if crate::av::capture_running() || crate::av::capture_failed() {
            crate::av::stop_capture();
        }
        // 原生停流会一起把这条流的音频输出摘掉，但 st.audio_started 还留着
        // true。下一次开会话时就会先被判成“音频采集中断”，白烧一次重试次数，
        // 所以要连着音频状态一起清干净。
        if st.audio_started || crate::av::audio_capture_running() {
            crate::av::stop_audio_capture();
        }
        crate::live::events::reset_capture_state(st);
        return;
    }
    if !crate::av::screen_granted() {
        crate::av::stop_capture();
        if !st.capture_failed {
            st.capture_failed = true;
            crate::send_state_diagnostic(st, &st.diag_tx, 14, 1, None);
            st.last_error = Some("需授权".into());
            println!("node: 需授权");
            if let Some(engine) = st.engine.as_mut() {
                let _ = engine.hangup(Reason::PermissionMissing);
            }
            st.pending_hangup = Some(Reason::PermissionMissing);
        }
        return;
    }
    // 采集中途自己停了：显示器热插拔、休眠、ScreenCaptureKit 报错都会走到这里。
    // 先停干净，再有限次重试；重试也失败才把错误固定下来。
    if crate::av::capture_failed() {
        crate::av::stop_capture();
        crate::av::stop_audio_capture();
        st.audio_started = false;
        st.audio_retries = 0;
        st.audio_retry_at_us = 0;
        st.capture_started = false;
        st.capture_announced = false;
        match after_capture_failure(st.capture_retries) {
            Next::Retry { retries, delay_us } => {
                st.capture_retries = retries;
                st.capture_retry_at_us = crate::live::now_us().saturating_add(delay_us);
                crate::send_state_diagnostic(st, &st.diag_tx, 14, 3, None);
                st.last_error = Some("采集中断，正在重试".into());
                println!("node: 采集中断，稍后重试（第 {retries} 次）");
            }
            Next::GiveUp => {
                st.capture_failed = true;
                crate::send_state_diagnostic(st, &st.diag_tx, 14, 3, None);
                st.last_error = Some("采集中断，重试已达上限".into());
                println!("node: 采集中断，重试 {} 次仍失败", st.capture_retries);
            }
        }
    }
    if should_start_capture(st.capture_started, st.capture_failed, st.capture_retry_at_us, now_us()) {
        let pick = st.want_display.min(i32::MAX as usize) as i32;
        match crate::av::start_capture(bitrate_of(st.quality), pick) {
            Ok(()) => {
                st.capture_started = true;
                st.capture_display = st.want_display;
            }
            Err(e) => {
                st.capture_failed = true;
                crate::send_state_diagnostic(st, &st.diag_tx, 14, 2, None);
                st.last_error = Some(e.clone());
                println!("node: {e}");
            }
        }
        crate::av::set_fps(st.fps);
        if let Some(engine) = st.engine.as_mut() {
            let (w, h) = (st.display.width, st.display.height);
            engine.set_stream_size(w, h);
        }
    }
    // 音频启动和视频启动分开：视频已经起来后，音频失败仍然可以重试。
    if st.audio_started && !crate::av::audio_capture_running() {
        // 设备热插拔、默认输出设备切换都会让采集线程自己退出。只信
        // audio_started 布尔会一直以为声音还在，剩余会话全静音。
        crate::av::stop_audio_capture();
        st.audio_started = false;
        match after_audio_failure(st.audio_retries) {
            Next::Retry { retries, delay_us } => {
                st.audio_retries = retries;
                st.audio_retry_at_us = now_us().saturating_add(delay_us);
                println!("node: 音频采集中断，稍后重试（第 {retries} 次）");
            }
            Next::GiveUp => {
                println!("node: 音频采集中断，重试已达上限，只传画面");
            }
        }
    }
    if may_start_audio(st.audio_started, st.capture_failed, st.audio_retries) && now_us() >= st.audio_retry_at_us {
        match crate::av::start_audio_capture(128_000) {
            Ok(()) => {
                st.audio_started = true;
                st.audio_retries = 0;
                st.audio_retry_at_us = 0;
                println!("node: 开始采集系统声音");
            }
            Err(e) => match after_audio_failure(st.audio_retries) {
                Next::Retry { retries, delay_us } => {
                    st.audio_retries = retries;
                    st.audio_retry_at_us = now_us().saturating_add(delay_us);
                    println!("node: 音频没起来，继续只传画面（第 {retries} 次）：{e}");
                }
                Next::GiveUp => {
                    st.audio_retries = super::capture_state::MAX_AUDIO_RETRIES;
                    st.audio_retry_at_us = 0;
                    println!("node: 音频始终没起来，本次会话只传画面：{e}");
                }
            },
        }
    }
    if crate::av::capture_running() && !st.capture_announced {
        st.capture_announced = true;
        // 真的跑起来了才清零；只是“启动函数返回成功”不算，异步启动可能马上失败。
        st.capture_retries = 0;
        st.capture_retry_at_us = 0;
        crate::send_state_diagnostic(st, &st.diag_tx, 13, 0, None);
        println!("node: 开始采集");
    }
    if let Some(msg) = crate::av::note_encoder() {
        println!("node: {msg}");
    }
    let aus = crate::av::drain_aus();
    let audios = crate::av::drain_audios();
    let cursor = crate::av::cursor_state();
    if let Some(engine) = st.engine.as_mut() {
        // 隐私遮挡期间不把系统声音送出去：屏幕已经藏起来了，声音还在传
        // 等于把本机正在放的内容漏给主控。帧照常从队列取走丢掉，避免积压。
        if !st.privacy_cover {
            for audio in audios {
                engine.push_audio(&audio.bytes, audio.ts);
            }
        }
        for au in aus {
            st.next_frame = st.next_frame.wrapping_add(1).max(1);
            if engine.push_video(st.next_frame, au.idr, &au.bytes, au.ts) {
                crate::av::force_idr();
            }
        }
        if let Some((x, y, visible)) = cursor {
            // 协议里的 CursorPos 是当前串流显示器的局部物理像素。
            // macOS 的 CGEventGetLocation 给的是全局 point，Retina 屏上
            // 1 point 是 2 像素，必须先换成物理像素再减原点；Windows 的
            // GetCursorPos 本来就是物理像素。
            #[cfg(target_os = "macos")]
            let (lx, ly) = jr_input::local_from_points((x, y), st.display, st.scale_milli);
            #[cfg(not(target_os = "macos"))]
            let (lx, ly) = (
                x.saturating_sub(st.display.origin_x),
                y.saturating_sub(st.display.origin_y),
            );
            let inside = visible
                && lx >= 0
                && ly >= 0
                && lx < st.display.width as i32
                && ly < st.display.height as i32;
            let at = (lx, ly, inside);
            if st.last_cursor != Some(at) {
                st.last_cursor = Some(at);
                engine.send_cursor(lx, ly, inside);
            }
        }
        // 光标形状只在真正变了的时候发，别让每一拍都占一条控制消息。
        crate::av::request_cursor_shape();
        if let Some(shape) = crate::av::cursor_shape() {
            if st.last_cursor_shape != Some(shape.hash) {
                // 只有真的进了发送队列才记住 hash。队列满时分片会发一半，
                // 下一拍必须重发完整形状，否则主控永远收不齐。
                if engine.send_cursor_shape(&shape).is_ok() {
                    st.last_cursor_shape = Some(shape.hash);
                }
            }
        }
    }
}



#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::net::SocketAddr;
    use std::path::PathBuf;

    /// 只构造 `sync_capture` 需要的状态。`os_capture = false`、`engine = None`
    /// 时整条路径都不会碰原生采集，测试可以安全跑真实代码。
    fn idle_state() -> State {
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
            platform: jr_proto::PLATFORM_LINUX,
            grants_dir: PathBuf::from("."),
            media_port: 0,
            advertise: Vec::new(),
            extra_candidate: None,
            reflexive: None,
            reflex_nonce: 0,
            reflex_port: 0,
            session_active: false,
            session_features: 0,
            heartbeat_enabled: false,
            last_fp: None,
            last_error: None,
            fleet_tag: [0u8; 8],
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
            capture_started: true,
            capture_announced: true,
            capture_failed: true,
            audio_started: true,
            audio_retries: 3,
            audio_retry_at_us: 12345,
            capture_retries: 2,
            capture_retry_at_us: 54321,
            capture_display: 1,
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

    /// 不需要采集时所有采集/音频状态都要归零。留一个 `capture_failed`
    /// 或 `audio_started`，下一次会话一上来就会误判成「正在重试」或
    /// 「音频已开」，整段会话没有声音。
    #[test]
    fn no_capture_wanted_clears_every_capture_flag() {
        let mut st = idle_state();
        sync_capture(&mut st);
        assert!(!st.capture_started);
        assert!(!st.capture_announced);
        assert!(!st.capture_failed);
        assert!(!st.audio_started);
        assert_eq!(st.capture_retries, 0);
        assert_eq!(st.capture_retry_at_us, 0);
        assert_eq!(st.audio_retries, 0);
        assert_eq!(st.audio_retry_at_us, 0);
    }

    /// `want_display` 只在真的开流时写进 `capture_display`；没采集请求时
    /// 不能因为它被清掉，否则切屏判断会以为「显示器变了」。
    #[test]
    fn no_capture_wanted_keeps_display_choice() {
        let mut st = idle_state();
        st.capture_display = 1;
        st.want_display = 1;
        sync_capture(&mut st);
        assert_eq!(st.capture_display, 1);
    }
}
