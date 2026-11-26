//! `NodeHandle` 上的命令入口：界面和 CLI 都从这里下发动作。

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use jr_crypto::{sign_grant, GrantClaims};
use jr_proto::{encode_frame, hex_encode, Invite, Reason, SignalType, PLATFORM_IOS};
use tokio::sync::oneshot;

use crate::net::{unix_now, write_grant_file};
use crate::node::NodeHandle;
use crate::signal::{
    accept_invite, begin_session, fail_session, finish_sessions, on_datagram, send_decision,
    st_take_prompt, status_word, PromptStep, PATH_DIRECT,
};
use crate::{
    autostart, av, desk, find_short, invite_error_code, live,
    send_diagnostic, send_heartbeat, send_replies, send_state_diagnostic, ui, DeviceView,
    CONTROLLER_FEATURES,
};
#[cfg(target_os = "macos")]
use crate::request_permission;

impl NodeHandle {
    pub fn privacy_cover_on(&self) -> bool {
        av::privacy_cover_on()
    }

    /// 信令看到的、本机媒体 socket 的公网地址。没做过反射发现就是 None。
    pub fn reflexive_addr(&self) -> Option<SocketAddr> {
        self.shared.state.lock().unwrap().reflexive
    }

    /// 本机媒体 socket 的端口。测试用它在本机另一个回环地址上摆一个诱饵，
    /// 看对面是不是真的往每一个候选都发了探测包。
    pub fn media_port(&self) -> u16 {
        self.shared.state.lock().unwrap().media_port
    }

    /// 测试用：把发出的媒体包拦下来。设了之后节点不再直接发 UDP，
    /// 由调用方决定这颗包能不能到对端。
    pub fn set_media_hook(&self, hook: Option<Arc<dyn Fn(&[u8], SocketAddr) + Send + Sync>>) {
        *self.shared.media_hook.lock().unwrap() = hook;
    }

    /// 测试用：假装从某个地址收到一颗媒体包。
    /// 状态锁在这里不持有：调用方可能正处在发送钩子里，而发送钩子本身
    /// 不持锁，但回复路径会再进来。
    pub fn inject_datagram(&self, src: SocketAddr, bytes: &[u8]) {
        let replies = {
            let mut st = self.shared.state.lock().unwrap();
            on_datagram(&mut st, src, bytes, &self.shared.outbound)
        };
        if !replies.is_empty() {
            let shared = self.shared.clone();
            self.handle.spawn(async move {
                send_replies(&shared, replies).await;
            });
        }
    }

    /// 当前这条会话是不是落在直连上。中继是 PATH_RELAY。
    pub fn direct_path(&self) -> bool {
        let st = self.shared.state.lock().unwrap();
        st.sessions
            .values()
            .find(|s| s.ready)
            .map(|s| s.path == PATH_DIRECT)
            .unwrap_or(false)
    }

    pub fn device_id(&self) -> [u8; 32] {
        self.shared.state.lock().unwrap().identity.device_id()
    }

    pub fn devices(&self) -> Vec<DeviceView> {
        self.shared
            .state
            .lock()
            .unwrap()
            .peers
            .values()
            .map(|p| DeviceView {
                name: p.snap.name.clone(),
                short_id: p.snap.short_id.clone(),
                device_id: p.snap.device_id,
                online: p.snap.online,
                revoked: p.snap.revoked,
                lan: p.lan,
                status: status_word(&p.snap).into(),
            })
            .collect()
    }

    pub fn session_fp(&self) -> Option<String> {
        self.shared.state.lock().unwrap().last_fp.clone()
    }

    pub fn indicating(&self) -> bool {
        self.shared.state.lock().unwrap().indicating
    }

    pub fn last_point(&self) -> Option<(i32, i32)> {
        self.shared.state.lock().unwrap().last_point
    }

    pub fn set_display(&self, origin_x: i32, origin_y: i32, width: u32, height: u32) {
        let mut st = self.shared.state.lock().unwrap();
        st.display = jr_input::Display { origin_x, origin_y, width, height };
        st.display_override = true;
        st.scale_milli = 1000;
    }

    pub fn start_stream(&self) -> Result<(), String> {
        let mut st = self.shared.state.lock().unwrap();
        let display = st.want_display as u32;
        let profile = if (1..=4).contains(&st.quality) { st.quality } else { 2 };
        let result = st.engine.as_mut().ok_or_else(|| "还没有会话".to_string())?.start_stream(display, profile);
        if result.is_ok() {
            send_state_diagnostic(&st, &self.shared.outbound, 11, profile as u16, None);
        }
        result
    }

    pub fn switch_display(&self, display: u32) -> Result<(), String> {
        self.shared
            .state
            .lock()
            .unwrap()
            .engine
            .as_mut()
            .ok_or_else(|| "还没有会话".to_string())?
            .switch_display(display)?;
        av::set_view_display(display as usize);
        Ok(())
    }

    /// 记住画质档。有会话时主控发给对面，被控只改自己的编码器，不反向发切换。
    /// 还没开会话也可以先选，下次 `watch` 带上这一档。
    pub fn set_quality(&self, profile: u8) -> Result<(), String> {
        if !(1..=4).contains(&profile) {
            return Err("画质档只有 1 到 4".into());
        }
        let apply_local = {
            let mut st = self.shared.state.lock().unwrap();
            let controller = st.engine.as_ref().is_some_and(|e| e.is_controller());
            if controller {
                let prev = st.quality;
                st.quality = profile;
                if let Some(engine) = st.engine.as_mut() {
                    if let Err(e) = engine.set_profile(profile) {
                        st.quality = prev;
                        return Err(e);
                    }
                }
                false
            } else {
                st.quality = profile;
                st.capture_started
            }
        };
        if apply_local {
            av::set_bitrate(live::bitrate_of(profile));
        }
        av::set_view_quality(profile);
        Ok(())
    }

    pub fn remote_displays(&self) -> Vec<(u32, u32)> {
        self.shared.state.lock().unwrap().remote_displays.clone()
    }

    pub fn video_frames(&self) -> u32 {
        self.shared.state.lock().unwrap().video_frames
    }

    pub fn audio_frames(&self) -> u32 {
        self.shared.state.lock().unwrap().audio_frames
    }

    pub fn feedback_count(&self) -> u32 {
        self.shared.state.lock().unwrap().feedback_count
    }

    pub fn submit_access_unit(&self, idr: bool, bytes: &[u8]) -> Result<(), String> {
        let mut st = self.shared.state.lock().unwrap();
        let frame = st.next_frame.wrapping_add(1).max(1);
        st.next_frame = frame;
        st.engine
            .as_mut()
            .ok_or_else(|| "还没有会话".to_string())?
            .push_video(frame, idr, bytes, live::now_us());
        Ok(())
    }

    pub fn last_video(&self) -> Option<Vec<u8>> {
        self.shared.state.lock().unwrap().last_video.clone()
    }

    pub fn capture_running(&self) -> bool {
        av::capture_running()
    }

    pub fn request_view(&self) -> Result<(), String> {
        if !cfg!(any(target_os = "macos", windows)) {
            return Err("此平台还没有观看窗口".into());
        }
        let mut st = self.shared.state.lock().unwrap();
        if st.engine.is_none() {
            return Err("还没有会话".into());
        }
        if !st.remote_video {
            return Err("对方没有画面".into());
        }
        let w = st.remote_w;
        let h = st.remote_h;
        st.view_open = true;
        send_state_diagnostic(&st, &self.shared.outbound, 12, 0, None);
        let quality = st.quality;
        let display = st.want_display;
        drop(st);
        // 窗口可能开在改过画质/显示器之后，先把当前选择同步过去。
        av::set_view_quality(quality);
        av::set_view_display(display);
        av::request_view(w, h);
        Ok(())
    }

    /// 邀请建立的是加密会话；收到被控端的画面能力后再打开观看窗口。
    pub async fn watch_and_open(&self) -> Result<(), String> {
        // invite() 的等待者可能略早于媒体引擎就绪；给握手回调一个短窗口。
        let mut engine_ready = false;
        for _ in 0..20 {
            if self.shared.state.lock().unwrap().engine.is_some() {
                engine_ready = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        if !engine_ready { return Err("会话引擎未就绪".into()); }
        self.start_stream()?;
        for _ in 0..50 {
            let status = {
                let st = self.shared.state.lock().unwrap();
                if st.engine.is_none() {
                    return Err("会话已结束".into());
                }
                (st.remote_capability_seen, st.remote_video)
            };
            if status.0 {
                return if status.1 {
                    self.request_view()
                } else {
                    Err("被控端没有画面能力。请在被控 Mac 的系统设置开启“录屏与系统录音”，重启被控客户端后重新邀请。".into())
                };
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Err("未收到被控端的画面能力，请重新邀请并检查两端连接".into())
    }

    pub fn send_input(&self, event: jr_proto::InputEvent) -> Result<(), String> {
        self.shared
            .state
            .lock()
            .unwrap()
            .engine
            .as_mut()
            .ok_or_else(|| "还没有会话".to_string())?
            .send_input(event)
    }

    pub fn last_error(&self) -> Option<String> {
        self.shared.state.lock().unwrap().last_error.clone()
    }

    pub fn set_heartbeat_enabled(&self, on: bool) {
        self.shared.state.lock().unwrap().heartbeat_enabled = on;
    }

    pub fn end_session(&self) {
        let mut st = self.shared.state.lock().unwrap();
        live::release(&mut st, Reason::UserHangup);
        st.engine = None;
        finish_sessions(&mut st, Err("已断开".into()));
        st.session_active = false;
        st.session_features = 0;
        st.last_fp = None;
        st.indicating = false;
        drop(st);
        send_heartbeat(&self.shared);
    }

    /// 通知对端后结束会话：Bye 先排进发送队列，主循环把它发出去再收尾。
    /// `end_session` 只清本机，对端要等路静默才知道；正常断开应该用这个。
    pub fn hangup(&self) {
        let mut st = self.shared.state.lock().unwrap();
        if let Some(engine) = st.engine.as_mut() {
            let _ = engine.hangup(Reason::UserHangup);
            st.pending_hangup = Some(Reason::UserHangup);
        } else {
            drop(st);
            self.end_session();
        }
    }

    pub fn send_clipboard(&self, text: &str) -> Result<(), String> {
        let mut st = self.shared.state.lock().unwrap();
        let engine = st.engine.as_mut().ok_or("还没有会话")?;
        engine.send_clipboard(text)?;
        Ok(())
    }

    /// 读本机剪贴板，然后整段发给对面。是 `clip` 的反方向，省得手动贴一遍。
    pub fn copy_local_clipboard(&self) -> Result<usize, String> {
        let text = av::clipboard_get().ok_or("本机剪贴板里没有文本")?;
        self.send_clipboard(&text)?;
        Ok(text.len())
    }

    pub fn send_file(&self, name: &str, bytes: &[u8]) -> Result<u32, String> {
        let mut st = self.shared.state.lock().unwrap();
        let engine = st.engine.as_mut().ok_or("还没有会话")?;
        let id = engine.offer_file(name, bytes)?;
        st.last_transfer = Some(id);
        Ok(id)
    }

    /// 正在发出的文件进度：编号、已发、总长。发完就从队列里拿掉了。
    pub fn file_progress(&self) -> Option<(u32, u64, u64)> {
        let st = self.shared.state.lock().unwrap();
        let engine = st.engine.as_ref()?;
        let id = st.last_transfer?;
        engine.file_progress(id).map(|(sent, total)| (id, sent, total))
    }

    pub fn cancel_file(&self, id: u32) -> Result<(), String> {
        let mut st = self.shared.state.lock().unwrap();
        st.engine.as_mut().ok_or("还没有会话")?.cancel_file(id)
    }

    /// 最近一次 ping/pong 量到的往返时间。没量到就是 None，不编数字。
    pub fn measured_rtt_ms(&self) -> Option<u32> {
        let st = self.shared.state.lock().unwrap();
        st.engine.as_ref().and_then(|e| e.measured_rtt_ms())
    }

    pub fn request_privacy(&self, on: bool) -> Result<(), String> {
        let mut st = self.shared.state.lock().unwrap();
        let engine = st.engine.as_mut().ok_or("还没有会话")?;
        engine.set_privacy(on)?;
        Ok(())
    }

    pub fn last_clipboard(&self) -> Option<String> {
        self.shared.state.lock().unwrap().last_clip.clone()
    }

    pub fn last_file(&self) -> Option<PathBuf> {
        self.shared.state.lock().unwrap().last_file.clone()
    }

    pub fn last_error_text(&self) -> Option<String> {
        self.shared.state.lock().unwrap().last_error.clone()
    }

    /// 数据目录。测试要看收件箱里到底落了什么。
    pub fn data_dir(&self) -> PathBuf {
        self.shared.state.lock().unwrap().data_dir.clone()
    }

    pub fn allow_wake(&self, mac: [u8; 6]) -> Result<(), String> {
        self.shared
            .outbound
            .send(encode_frame(SignalType::SetWake as u8, &mac).map_err(|e| e.to_string())?)
            .map_err(|_| "信令已断开".to_string())
    }

    pub fn request_wake(&self, short_id: &str) -> Result<(), String> {
        let st = self.shared.state.lock().unwrap();
        if !st.signal_connected {
            return Err("信令已断开，暂时不能发唤醒".into());
        }
        let peer = find_short(&st, short_id)?;
        let wake = jr_proto::Wake { target: peer.device_id, mac: [0; 6] };
        self.shared
            .outbound
            .send(encode_frame(SignalType::Wake as u8, &wake.encode()).map_err(|e| e.to_string())?)
            .map_err(|_| "信令已断开".to_string())
    }

    pub fn open_terminal(&self) -> Result<(), String> {
        let mut st = self.shared.state.lock().unwrap();
        let engine = st.engine.as_mut().ok_or("还没有会话")?;
        engine.open_terminal(80, 24, "xterm-256color")
    }

    pub fn write_terminal(&self, data: &[u8]) -> Result<(), String> {
        let mut st = self.shared.state.lock().unwrap();
        let engine = st.engine.as_mut().ok_or("还没有会话")?;
        engine.write_terminal(data)
    }

    pub fn resize_terminal(&self, cols: u16, rows: u16) -> Result<(), String> {
        let mut st = self.shared.state.lock().unwrap();
        let engine = st.engine.as_mut().ok_or("还没有会话")?;
        engine.resize_terminal(cols, rows)
    }

    pub fn terminal_text(&self) -> String {
        String::from_utf8_lossy(&self.shared.state.lock().unwrap().term_buf).into_owned()
    }

    pub fn request_audit(&self) -> Result<(), String> {
        self.shared
            .outbound
            .send(encode_frame(SignalType::ListAudit as u8, &[]).map_err(|e| e.to_string())?)
            .map_err(|_| "信令已断开".to_string())
    }

    pub async fn invite(&self, short_id: &str, unattended: bool) -> Result<String, String> {
        let (frame, rx, sid) = {
            let mut st = self.shared.state.lock().unwrap();
            let peer = find_short(&st, short_id)?;
            if peer.revoked {
                return Err(Reason::Revoked.user_text().into());
            }
            if !peer.online {
                return Err(Reason::SessionDead.user_text().into());
            }
            if peer.platform == PLATFORM_IOS {
                return Err(Reason::PlatformNotHostable.user_text().into());
            }
            if st.session_active {
                return Err("当前已有会话，请先点“断开”再邀请".into());
            }
            let sid = jr_crypto::random_bytes::<16>().map_err(|e| e.to_string())?;
            begin_session(&mut st, sid, peer.device_id, true)?;
            let (tx, rx) = oneshot::channel();
            st.sessions.get_mut(&sid).unwrap().waiter = Some(tx);
            let invite = Invite {
                session_id: sid,
                controller_id: st.identity.device_id(),
                host_id: peer.device_id,
                feature_bits: CONTROLLER_FEATURES,
                unattended,
            };
            st.session_features = CONTROLLER_FEATURES;
            let frame = encode_frame(SignalType::Invite as u8, &invite.encode().map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?;
            (frame, rx, sid)
        };
        let sent = self.shared
            .outbound
            .send(frame)
            .map_err(|_| "信令已断开".to_string());
        if sent.is_err() {
            let mut st = self.shared.state.lock().unwrap();
            fail_session(&mut st, sid, "信令已断开");
            return Err("信令已断开".into());
        }
        {
            let st = self.shared.state.lock().unwrap();
            send_state_diagnostic(&st, &self.shared.outbound, 4, u16::from(unattended), None);
        }
        let result = match tokio::time::timeout(Duration::from_secs(8), rx).await {
            Ok(Ok(Ok(fp))) => Ok(fp),
            Ok(Ok(Err(e))) => Err(e),
            _ => Err(Reason::HandshakeTimeout.user_text().into()),
        };
        if result.is_err() {
            let mut st = self.shared.state.lock().unwrap();
            if st.sessions.contains_key(&sid) {
                fail_session(&mut st, sid, result.as_ref().err().unwrap());
            }
        }
        result
    }

    pub fn accept_pending(&self) -> Result<(), String> {
        let replies = {
            let mut st = self.shared.state.lock().unwrap();
            let Some(invite) = st.pending_in.take() else {
                return Err("没有待处理的邀请".into());
            };
            accept_invite(&mut st, invite, &self.shared.outbound)
        };
        let shared = self.shared.clone();
        let handle = self.handle.clone();
        handle.spawn(async move {
            send_replies(&shared, replies).await;
        });
        Ok(())
    }

    pub fn reject_pending(&self) -> Result<(), String> {
        let mut st = self.shared.state.lock().unwrap();
        let Some(invite) = st.pending_in.take() else {
            return Err("没有待处理的邀请".into());
        };
        send_decision(&st, &self.shared.outbound, &invite.session_id, false, Reason::Rejected);
        Ok(())
    }

    pub fn revoke(&self, short_id: &str) -> Result<(), String> {
        let st = self.shared.state.lock().unwrap();
        if st.private_mode {
            return Err("自用服务器只能经 SSH 管理命令撤销设备".into());
        }
        let peer = find_short(&st, short_id)?;
        let frame = encode_frame(SignalType::RevokeDevice as u8, &peer.device_id)
            .map_err(|e| e.to_string())?;
        self.shared
            .outbound
            .send(frame)
            .map_err(|_| "信令已断开".to_string())?;
        Ok(())
    }

    pub fn grant_controller(&self, controller: [u8; 32], seconds: u64) -> Result<(), String> {
        let st = self.shared.state.lock().unwrap();
        let now = unix_now();
        let life = seconds.min(jr_proto::GRANT_MAX_AGE_SEC);
        let claims = GrantClaims {
            host: st.identity.device_id(),
            controller,
            feature_bits: live::local_host_features(&st),
            not_before: now,
            not_after: now.saturating_add(life),
        };
        let assertion = sign_grant(&st.identity, &claims);
        write_grant_file(&st.grants_dir, &controller, &assertion)?;
        let mut body = Vec::new();
        body.extend_from_slice(&controller);
        body.extend_from_slice(&(assertion.len() as u16).to_le_bytes());
        body.extend_from_slice(&assertion);
        let frame = encode_frame(SignalType::StoreGrant as u8, &body).map_err(|e| e.to_string())?;
        st.outbound_send(&self.shared.outbound, frame);
        println!("node: grant stored controller={}", hex_encode(&controller[..4]));
        Ok(())
    }

    /// 撤掉无人值守授权。文件不在就是没有授权，之后对方只能走有人在场的邀请。
    pub fn ungrant(&self, short_id: &str) -> Result<(), String> {
        let (dir, device) = {
            let st = self.shared.state.lock().unwrap();
            let peer = find_short(&st, short_id)?;
            (st.grants_dir.clone(), peer.device_id)
        };
        let path = dir.join(hex_encode(&device));
        match std::fs::remove_file(&path) {
            Ok(()) => {
                println!("node: grant removed controller={}", hex_encode(&device[..4]));
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err("这台设备本来就没有授权".into()),
            Err(e) => Err(e.to_string()),
        }
    }

    /// 测试用：把一段断言放到本机授权目录，不代表信任它。
    pub fn plant_grant(&self, controller: [u8; 32], assertion: &[u8]) -> Result<(), String> {
        let st = self.shared.state.lock().unwrap();
        write_grant_file(&st.grants_dir, &controller, assertion)
    }

    pub fn open_ui(&self) {
        self.shared.state.lock().unwrap().ui_open = true;
        ui::set_show(true);
    }

    pub fn close_ui(&self) {
        self.shared.state.lock().unwrap().ui_open = false;
        ui::set_show(false);
    }

    pub fn ui_open(&self) -> bool {
        self.shared.state.lock().unwrap().ui_open
    }

    pub fn set_autostart(&self, on: bool) -> Result<(), String> {
        let dir = self.shared.state.lock().unwrap().data_dir.clone();
        autostart::set(&dir, on)?;
        self.shared.state.lock().unwrap().autostart = on;
        Ok(())
    }

    pub fn autostart(&self) -> bool {
        self.shared.state.lock().unwrap().autostart
    }

    pub fn remember_launch_args(&self, args: &[String]) -> Result<(), String> {
        autostart::remember_args(&self.shared.state.lock().unwrap().data_dir, args)
    }

    pub fn handle(&self) -> NodeHandle {
        self.clone()
    }

    /// 界面线程每拍调一次：刷新列表、处理按钮、在主线程弹邀请确认框。
    /// 没开设备窗口时不画列表。Windows 上仍要抽消息，遮挡层才画得出来。
    pub fn pump_ui(&self) {
        ui::bind();
        av::bind_view();
        let open = self.ui_open();
        if open {
            let (header, rows, auto, quality, can_view) = {
                let st = self.shared.state.lock().unwrap();
                (desk::header(&st), desk::rows(&st), st.autostart, st.quality,
                    st.engine.as_ref().is_some_and(|engine| engine.is_controller()) && st.remote_video)
            };
            ui::present(&header, &rows, auto, can_view);
            #[cfg(target_os = "macos")]
            {
                let st = self.shared.state.lock().unwrap();
                let session = st.sessions.values().next();
                let peer = session.and_then(|s| st.peers.get(&s.peer))
                    .map(|p| p.snap.short_id.as_str()).unwrap_or("");
                ui::set_overview(&st.name, &st.short_id, &st.signal_addr.to_string(),
                    st.signal_connected, st.session_active, peer,
                    session.is_some_and(|s| s.role_controller));
            }
            ui::set_shown_quality(quality);
        }
        av::pump_ui();
        ui::pump();
        let prompt = {
            let mut st = self.shared.state.lock().unwrap();
            st_take_prompt(&mut st, &self.shared.outbound)
        };
        match prompt {
            PromptStep::Idle => {}
            PromptStep::Ask(invite, name) => {
                // 弹窗要等人点，不能拿着 State 锁。锁在上面的块里已经放掉了。
                let body = format!("设备「{name}」请求控制这台电脑。同意后对方可使用本机开放的屏幕、键鼠、剪贴板、文件和终端能力。");
                let allow = ui::ask("远控邀请", &body);
                let mut st = self.shared.state.lock().unwrap();
                st.pending_in = Some(invite);
                st.prompt_answer = Some(allow);
            }
            PromptStep::Replies(replies) => {
                if !replies.is_empty() {
                    let shared = self.shared.clone();
                    self.handle.spawn(async move {
                        send_replies(&shared, replies).await;
                    });
                }
            }
        }
        while let Some((kind, arg)) = ui::take_action() {
            self.apply_ui_action(kind, &arg);
        }
    }

    fn apply_ui_action(&self, kind: u8, arg: &str) {
        if kind == ui::ACT_INVITE {
            // 邀请要走网络来回，不能卡住界面线程。结果回头写进状态，由列表刷新看到。
            let handle = self.clone();
            let short = arg.to_string();
            send_diagnostic(&self.shared.outbound, 2, 0, None, &short);
            self.push_notice(format!("正在邀请 {short}…"));
            self.handle.spawn(async move {
                match handle.invite(&short, false).await {
                    Ok(fp) => {
                        println!("node: 已连接，会话指纹 {fp}");
                        match handle.watch_and_open().await {
                            Ok(()) => handle.push_notice(format!("已连接并打开 {short} 的画面")),
                            Err(e) => {
                                send_diagnostic(&handle.shared.outbound, 20, view_error_code(&e), None, &short);
                                handle.push_notice(format!("已连接 {short}，但无法观看：{e}"));
                            }
                        }
                    }
                    Err(e) => {
                        println!("node: {e}");
                        let mut st = handle.shared.state.lock().unwrap();
                        st.last_error = Some(e.clone());
                        drop(st);
                        send_diagnostic(&handle.shared.outbound, 3, invite_error_code(&e), None, &short);
                        handle.push_notice(format!("邀请失败：{e}"));
                    }
                }
            });
            return;
        }
        if kind == ui::ACT_LOGS {
            println!("node: logs-open");
            let dir = self.shared.state.lock().unwrap().data_dir.clone();
            ui::show_logs(&dir);
            return;
        }
        let result = match kind {
            ui::ACT_GRANT => self.grant_by_short(arg, jr_proto::GRANT_MAX_AGE_SEC),
            ui::ACT_UNGRANT => self.ungrant(arg).map(|()| format!("已撤回 {arg} 的无人值守授权")),
            ui::ACT_WAKE => self.request_wake(arg).map(|()| format!("已请求唤醒 {arg}，等待同网节点代发")),
            ui::ACT_REVOKE => self.revoke(arg).map(|()| format!("已撤销 {arg}")),
            ui::ACT_AUTOSTART => {
                let on = arg == "1";
                self.set_autostart(on).map(|()| {
                    if on {
                        "已开启开机自启".to_string()
                    } else {
                        "已关闭开机自启".to_string()
                    }
                })
            }
            ui::ACT_END => {
                // 走 hangup 而不是 end_session：先把 Bye 排进发送队列，对端
                // 立刻知道断开了。end_session 只清本机，对端要等 30 秒路静默
                // 才发现，期间还会以为自己仍在被控制。
                self.hangup();
                Ok("已结束会话".to_string())
            }
            ui::ACT_QUIT => {
                ui::request_quit();
                Ok(String::new())
            }
            ui::ACT_PRIVACY_OFF => {
                // 托盘菜单可能就是本机被遮挡时唯一还能点的入口。先撤本地
                // 遮挡和静音，再通知对端，不能只发控制包。
                av::privacy_cover(false);
                {
                    let mut st = self.shared.state.lock().unwrap();
                    st.privacy_cover = false;
                }
                self.request_privacy(false).map(|()| "已恢复屏幕".to_string())
            }
            ui::ACT_RESCAN => Ok("已刷新".to_string()),
            ui::ACT_VIEW => {
                let handle = self.clone();
                self.handle.spawn(async move {
                    match handle.watch_and_open().await {
                        Ok(()) => handle.push_notice("已打开观看窗口".to_string()),
                        Err(e) => {
                            send_diagnostic(&handle.shared.outbound, 20, view_error_code(&e), None, "");
                            handle.push_notice(e);
                        }
                    }
                });
                Ok(String::new())
            },
            #[cfg(target_os = "macos")]
            ui::ACT_PERMIT => {
                request_permission();
                Ok("请在系统设置授予录屏和辅助功能，之后重启客户端".to_string())
            }
            ui::ACT_QUALITY => match arg.parse::<u8>() {
                Ok(n) => self.set_quality(n).map(|()| format!("画质已切到 {}", jr_host_win::desk::quality_label(n))),
                Err(_) => Err("画质是 1 到 4".to_string()),
            },
            _ => Ok(String::new()),
        };
        match result {
            Ok(notice) if !notice.is_empty() => self.push_notice(notice),
            Ok(_) => {}
            Err(e) => {
                println!("node: {e}");
                self.shared.state.lock().unwrap().last_error = Some(e.clone());
                self.push_notice(e);
            }
        }
    }

    fn grant_by_short(&self, short_id: &str, seconds: u64) -> Result<String, String> {
        let device = {
            let st = self.shared.state.lock().unwrap();
            find_short(&st, short_id)?.device_id
        };
        self.grant_controller(device, seconds)?;
        Ok(format!("已给 {short_id} 无人值守授权"))
    }

    /// 界面上的一条短提示。没有界面时只进日志。
    fn push_notice(&self, text: String) {
        if self.ui_open() {
            ui::notice(&text);
        }
    }
}

/// 观看失败时上报的事件取值。文案映射和邀请那边保持一样粗，只给统计用。
fn view_error_code(text: &str) -> u16 {
    if text.contains("没有画面能力") || text.contains("没有画面") { 1 }
    else if text.contains("未收到") { 2 }
    else if text.contains("会话已结束") { 3 }
    else if text.contains("未就绪") { 4 }
    else { 0 }
}
