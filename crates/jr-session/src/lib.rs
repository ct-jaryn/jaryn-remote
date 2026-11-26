#![forbid(unsafe_code)]

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};

use jr_crypto::sha256;
use jr_proto::{
    split_clipboard, AudioFrame, Capability, ClipboardPart, CursorShape, FileAction, FileChunk, Feedback, InputEvent, Inner, MsgType, Reason,
    SessionControl, CLIPBOARD_MAX, CURSOR_SHAPE_MAX, CURSOR_SHAPE_HEADER, FEAT_AUDIO_OUT, FEAT_CLIPBOARD, FEAT_FILE,
    FEAT_INPUT, FEAT_PRIVACY, FEAT_TERMINAL, FEAT_VIDEO, FEAT_WATCH_ONLY, FLAG_FEC, FLAG_IDR,
    FLAG_REF, FLAG_SLICE_END, MAX_BODY, REL_HEADER_LEN,
    PLATFORM_ANDROID, PLATFORM_IOS, PLATFORM_LINUX,
};
use jr_transport::{CursorLatest, InputGate, Jcc, Reassembled, Reassembler, Reliable, RxMeter};

pub const FEEDBACK_WINDOW_MS: u64 = 200;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    SendReliable { stream: u8, payload: Vec<u8> },
    Inject(InputEvent),
    Cursor(CursorLatest),
    /// 远端光标形状变了。主控照着画，不要用本机的箭头冒充。
    CursorShape(CursorShape),
    Clipboard(String),
    FileDone { id: u32, path: PathBuf },
    FileFailed { id: u32, why: String },
    PrivacyRejected,
    /// 隐私屏开了，但只是软件遮挡。主控要照着这个说清楚。
    PrivacyDegraded,
    /// 隐私屏要撤掉。被控端照着把遮挡层收掉。
    PrivacyOff,
    TerminalOpen { cols: u16, rows: u16, term: String },
    Quality(u8),
    TerminalData(Vec<u8>),
    TerminalResize { cols: u16, rows: u16 },
    ReleaseAll,
    Closed(Reason),
    NeedKeyframe(u32),
    Video { idr: bool, bytes: Vec<u8> },
    /// 一帧 AAC-LC。timestamp 是采集端时钟，播放端只用它排抖动缓冲。
    Audio { timestamp_us: u64, bytes: Vec<u8> },
    RemoteCapability { video: bool, width: u32, height: u32, displays: Vec<(u32, u32)> },
    Bitrate { bps: u32, fps: u16 },
    StreamStart { display: u32 },
    StreamStop,
    /// 某条可靠流重传耗尽：终端、文件或剪贴板已经不可用，会话该结束了。
    ReliableFailed,
}

pub struct Engine {
    controller: bool,
    features: u32,
    privacy_supported: bool,
    privacy_degraded: bool,
    input: InputGate,
    reliable: Reliable,
    clips: HashMap<u32, ClipBatch>,
    files: HashMap<u32, RecvFile>,
    inbox: PathBuf,
    privacy_on: bool,
    input_paused: bool,
    capture_on: bool,
    released: bool,
    cursor: CursorLatest,
    next_file: u32,
    out_meta: HashMap<(u8, u32), OutMeta>,
    in_meta: HashMap<u32, OutMeta>,
    next_cookie: u32,
    /// 光标形状分片的批次号，用通用分片字段 frame_id 表达。
    cursor_frame: u32,
    pending_send: HashMap<u32, SendFile>,
    /// 可靠窗口满的时候排不进去的控制帧。等窗口腾出位置再发。
    /// 用户连着发两个文件、或者刚传完大文件马上发剪贴板，都会走到这里。
    deferred: VecDeque<(u8, u8, u32, u16, u16, Vec<u8>)>,
    hot: VecDeque<WireMsg>,
    video: Reassembler,
    jcc: Jcc,
    rx: RxMeter,
    stream_w: u32,
    stream_h: u32,
    last_fb_us: u64,
    /// 可靠流失败只报一次，别每拍都刷同一个事件。
    reliable_failed_reported: bool,
    /// 光标形状分片正在拼。形状最大 128×128，必须分片才塞得进单包。
    /// 按 frame_id 区分批次，收到乱序分片也能拼回去。
    cursor_shape_parts: Option<CursorParts>,
}

/// 要发出去的文件。分片不能一口气全塞进可靠流：窗口只有 64 帧，
/// 塞不进去的部分以前是被静默丢掉的，到最后只会报校验失败。
#[derive(Clone)]
struct SendFile {
    bytes: Vec<u8>,
    offset: u64,
    accepted: bool,
    done: bool,
}

#[derive(Clone, Copy)]
struct OutMeta {
    msg_type: u8,
    frame_id: u32,
    slice_index: u16,
    slice_count: u16,
}

pub struct WireMsg {
    pub msg_type: u8,
    pub stream: u8,
    pub frame_id: u32,
    pub slice_index: u16,
    pub slice_count: u16,
    pub body_flags: u16,
    pub timestamp_us: u64,
    pub body: Vec<u8>,
}

struct ClipBatch {
    total: u32,
    got: Vec<Option<Vec<u8>>>,
}

struct CursorParts {
    frame: u32,
    count: u16,
    pieces: Vec<Option<Vec<u8>>>,
    /// 已收分片的字节数。先算上限再存，别让一堆合法小分片拼出超大缓冲。
    total: usize,
}

/// 一批剪贴板最多允许的分片数。1 MiB 按单包净荷切也到不了 1024。
const CLIP_PARTS_MAX: usize = 1024;
/// 同时只保留几批没拼完的剪贴板。正常复制一次只有一批，
/// 未完成批次不能无限留在 HashMap 里。
const CLIP_BATCH_MAX: usize = 8;

struct RecvFile {
    tmp: PathBuf,
    final_name: String,
    size: u64,
    expect: [u8; 32],
    written: u64,
}

impl Engine {
    pub fn new(controller: bool, inbox: PathBuf, features: u32, privacy_supported: bool) -> Self {
        let _ = fs::create_dir_all(&inbox);
        Self {
            controller,
            features,
            privacy_supported,
            privacy_degraded: false,
            input: InputGate::default(),
            reliable: Reliable::new(),
            clips: HashMap::new(),
            files: HashMap::new(),
            inbox,
            privacy_on: false,
            input_paused: false,
            capture_on: false,
            released: false,
            cursor: CursorLatest::default(),
            next_file: 1,
            out_meta: HashMap::new(),
            in_meta: HashMap::new(),
            next_cookie: 1,
            cursor_frame: 0,
            pending_send: HashMap::new(),
            deferred: VecDeque::new(),
            hot: VecDeque::new(),
            video: Reassembler::new(),
            jcc: Jcc::new(2),
            rx: RxMeter::new(),
            stream_w: 0,
            stream_h: 0,
            last_fb_us: 0,
            reliable_failed_reported: false,
            cursor_shape_parts: None,
        }
    }

    /// 被控端告诉引擎真实采集尺寸，拥塞控制器要用它算每帧预算。
    pub fn set_stream_size(&mut self, width: u32, height: u32) {
        if width > 0 && height > 0 {
            self.stream_w = width;
            self.stream_h = height;
        }
    }

    pub fn set_profile(&mut self, profile: u8) -> Result<(), String> {
        if !(1..=4).contains(&profile) {
            return Err("画质档只有 1 到 4".into());
        }
        self.remember_profile(profile);
        self.control(SessionControl {
            action: SessionControl::SWITCH_PROFILE,
            display_id: 0,
            profile,
            flags: 0,
        })
    }

    /// 只改本端拥塞控制里的档，不发控制包。开会话前选的档用这个，避免握手时多塞一帧。
    pub fn remember_profile(&mut self, profile: u8) {
        if (1..=4).contains(&profile) {
            self.jcc = Jcc::new(profile);
        }
    }

    /// 主控每满一个反馈窗口调一次。返回 Some 时调用方把反馈发出去。
    pub fn feedback_due(&mut self, now_us: u64) -> Option<Feedback> {
        if !self.controller {
            return None;
        }
        let rtt = self.reliable.rtt_ms().min(u16::MAX as u32) as u16;
        let fb = self.rx.report(now_us / 1000, rtt, FEEDBACK_WINDOW_MS)?;
        self.rx.note_queue(0);
        Some(fb)
    }

    pub fn rx_stats(&self) -> jr_transport::RxStats {
        self.rx.stats()
    }

    /// 真正量到的往返时间。没收到过回包时是 None。
    pub fn measured_rtt_ms(&self) -> Option<u32> {
        self.reliable.measured_rtt_ms()
    }

    pub fn send_input(&mut self, event: InputEvent) -> Result<(), String> {
        let body = event.encode().map_err(|e| e.to_string())?;
        self.hot.push_back(WireMsg {
            msg_type: MsgType::Input as u8,
            stream: 3,
            frame_id: 0,
            slice_index: 0,
            slice_count: 1,
            body_flags: 0,
            timestamp_us: 0,
            body,
        });
        Ok(())
    }

    pub fn send_ping(&mut self, timestamp_us: u64) {
        self.hot.push_back(WireMsg {
            msg_type: MsgType::Ping as u8,
            stream: 0,
            frame_id: 0,
            slice_index: 0,
            slice_count: 1,
            body_flags: 0,
            timestamp_us,
            body: timestamp_us.to_le_bytes().to_vec(),
        });
    }

    /// 把拥塞反馈发给被控端。反馈是统计信息，丢了下一窗口还有，所以走热队列不重传。
    pub fn send_feedback(&mut self, fb: &Feedback) {
        self.hot.push_back(WireMsg {
            msg_type: MsgType::Feedback as u8,
            stream: 0,
            frame_id: 0,
            slice_index: 0,
            slice_count: 1,
            body_flags: 0,
            timestamp_us: 0,
            body: fb.encode(),
        });
    }

    /// 返回是否丢过未发出的参考帧；调用方应请求编码器尽快再出 IDR。
    pub fn push_video(&mut self, frame_id: u32, idr: bool, annex_b: &[u8], timestamp_us: u64) -> bool {
        if annex_b.is_empty() || self.released {
            return false;
        }
        let mut dropped_reference = false;
        if idr {
            self.hot.retain(|m| m.msg_type != MsgType::Video as u8);
        } else {
            let mut ids = Vec::new();
            for msg in &self.hot {
                if msg.msg_type == MsgType::Video as u8 && !ids.contains(&msg.frame_id) {
                    ids.push(msg.frame_id);
                }
            }
            while ids.len() >= 3 {
                let old = ids.remove(0);
                self.hot.retain(|m| m.msg_type != MsgType::Video as u8 || m.frame_id != old);
                dropped_reference = true;
            }
        }
        let slices = split_access_unit(annex_b, 1160);
        let count = slices.len() as u16;
        for (i, slice) in slices.into_iter().enumerate() {
            let mut body = Vec::with_capacity(1 + slice.len());
            body.push(1);
            body.extend(slice);
            let mut flags = FLAG_REF;
            if idr {
                flags |= FLAG_IDR;
            }
            if i as u16 + 1 == count {
                flags |= FLAG_SLICE_END;
            }
            self.hot.push_back(WireMsg {
                msg_type: MsgType::Video as u8,
                stream: 1,
                frame_id,
                slice_index: i as u16,
                slice_count: count,
                body_flags: flags,
                timestamp_us,
                body,
            });
        }
        dropped_reference
    }

    /// 音频帧不走可靠流，也不做分片：一帧 AAC 通常只有几百字节，
    /// 超过单包就丢掉这一帧，不能让声音把视频的发送窗口占满。
    pub fn push_audio(&mut self, bytes: &[u8], timestamp_us: u64) -> bool {
        if self.controller
            || self.released
            || !self.capture_on
            || bytes.is_empty()
            || self.features & FEAT_AUDIO_OUT == 0
        {
            return false;
        }
        let frame = AudioFrame::aac_lc(bytes.to_vec());
        let Ok(body) = frame.encode() else {
            return false;
        };
        self.hot.push_back(WireMsg {
            msg_type: MsgType::Audio as u8,
            stream: 2,
            frame_id: 0,
            slice_index: 0,
            slice_count: 1,
            body_flags: 0,
            timestamp_us,
            body,
        });
        true
    }

    pub fn send_cursor(&mut self, x: i32, y: i32, visible: bool) {
        let mut body = Vec::with_capacity(9);
        body.extend_from_slice(&x.to_le_bytes());
        body.extend_from_slice(&y.to_le_bytes());
        body.push(u8::from(visible));
        self.hot.push_back(WireMsg {
            msg_type: MsgType::CursorPos as u8,
            stream: 4,
            frame_id: 0,
            slice_index: 0,
            slice_count: 1,
            body_flags: 0,
            timestamp_us: 0,
            body,
        });
    }

    pub fn send_cursor_shape(&mut self, shape: &CursorShape) -> Result<(), String> {
        let body = shape.encode().map_err(|e| format!("光标形状编码失败: {e:?}"))?;
        let budget = MAX_BODY.saturating_sub(REL_HEADER_LEN).max(1);
        let chunks: Vec<&[u8]> = body.chunks(budget).collect();
        if chunks.len() > u16::MAX as usize {
            return Err("光标形状分片太多".into());
        }
        self.cursor_frame = self.cursor_frame.wrapping_add(1).max(1);
        let frame = self.cursor_frame;
        let count = chunks.len() as u16;
        // 走可靠流 4，分片信息用外层通用的 frame_id/slice_index/slice_count，
        // 和 docs/03-protocol.md 9.6 一致；body 就是形状本身的一段。
        for (i, chunk) in chunks.into_iter().enumerate() {
            self.queue_or_defer(
                4,
                MsgType::CursorShape as u8,
                frame,
                i as u16,
                count,
                chunk.to_vec(),
            )?;
        }
        Ok(())
    }

    pub fn request_keyframe(&mut self, frame_id: u32) -> Result<(), String> {
        self.queue(0, MsgType::KeyframeRequest as u8, 0, 0, 1, frame_id.to_le_bytes().to_vec())
    }

    pub fn hangup(&mut self, reason: Reason) -> Result<(), String> {
        self.queue(0, MsgType::Bye as u8, 0, 0, 1, (reason as u16).to_le_bytes().to_vec())
    }

    pub fn poll_wire(&mut self, now_ms: u64) -> Vec<WireMsg> {
        self.flush_deferred();
        self.pump_files();
        let mut out: Vec<WireMsg> = self.hot.drain(..).collect();
        out.extend(self.reliable
            .poll(now_ms)
            .into_iter()
            .map(|o| {
                let meta = self.out_meta.get(&(o.stream_id, o.rel_seq)).copied().unwrap_or(OutMeta {
                    msg_type: o.tag as u8,
                    frame_id: 0,
                    slice_index: 0,
                    slice_count: 1,
                });
                WireMsg {
                    msg_type: meta.msg_type,
                    stream: o.stream_id,
                    frame_id: meta.frame_id,
                    slice_index: meta.slice_index,
                    slice_count: meta.slice_count,
                    body_flags: 0,
                    timestamp_us: 0,
                    body: o.body,
                }
            }));
        out
    }

    pub fn ingest(&mut self, msg_type: u8, stream: u8, frame_id: u32, slice_index: u16, slice_count: u16, timestamp_us: u64, now_us: u64, body_flags: u16, body: &[u8]) -> Vec<SessionEvent> {
        if is_reliable(msg_type) {
            let cookie = self.next_cookie;
            self.next_cookie = self.next_cookie.wrapping_add(1).max(1);
            self.in_meta.insert(cookie, OutMeta { msg_type, frame_id, slice_index, slice_count });
            let delivered = self.reliable.on_body(stream, cookie, body).unwrap_or_default();
            let mut ev = Vec::new();
            for (cookie, payload) in delivered {
                let meta = self.in_meta.remove(&cookie).unwrap_or(OutMeta { msg_type, frame_id, slice_index, slice_count });
                ev.extend(self.on_payload(meta.msg_type, meta.frame_id, meta.slice_index, meta.slice_count, timestamp_us, now_us, 0, &payload));
            }
            ev
        } else {
            self.on_payload(msg_type, frame_id, slice_index, slice_count, timestamp_us, now_us, body_flags, body)
        }
    }

    pub fn capture_on(&self) -> bool {
        self.capture_on
    }

    pub fn released(&self) -> bool {
        self.released
    }

    pub fn cursor(&self) -> &CursorLatest {
        &self.cursor
    }

    pub fn host_capability(&mut self, cap: Capability) -> Result<(), String> {
        if self.controller {
            return Err("主控不发能力".into());
        }
        let payload = cap.encode().map_err(|e| e.to_string())?;
        self.queue(0, MsgType::Capability as u8, 0, 0, 1, payload)
    }

    pub fn start_stream(&mut self, display: u32, profile: u8) -> Result<(), String> {
        self.switch_display(display)?;
        self.control(SessionControl { action: SessionControl::START, display_id: display, profile, flags: 0 })
    }

    /// 主控切被控的显示器。display 是从 0 开始的序号，来自对方能力里的顺序。
    pub fn switch_display(&mut self, display: u32) -> Result<(), String> {
        self.control(SessionControl { action: SessionControl::SWITCH_DISPLAY, display_id: display, profile: 0, flags: 0 })
    }

    pub fn stop_stream(&mut self) -> Result<(), String> {
        self.control(SessionControl { action: SessionControl::END, display_id: 0, profile: 0, flags: 0 })
    }

    pub fn open_terminal(&mut self, cols: u16, rows: u16, term: &str) -> Result<(), String> {
        let mut body = Vec::new();
        body.extend_from_slice(&cols.to_le_bytes());
        body.extend_from_slice(&rows.to_le_bytes());
        body.extend_from_slice(term.as_bytes());
        self.queue(7, MsgType::TerminalOpen as u8, 0, 0, 1, body)
    }

    pub fn write_terminal(&mut self, data: &[u8]) -> Result<(), String> {
        self.queue(7, MsgType::TerminalData as u8, 0, 0, 1, data.to_vec())
    }

    /// 主控这边的终端窗口变了，告诉被控端去改 pty 尺寸。
    pub fn resize_terminal(&mut self, cols: u16, rows: u16) -> Result<(), String> {
        if cols == 0 || rows == 0 {
            return Err("终端尺寸不能是 0".into());
        }
        let mut body = Vec::with_capacity(4);
        body.extend_from_slice(&cols.to_le_bytes());
        body.extend_from_slice(&rows.to_le_bytes());
        self.queue(7, MsgType::TerminalResize as u8, 0, 0, 1, body)
    }

    /// 这台机器只能做软件遮挡时调。开了隐私屏之后会同时告诉对面这是降级模式。
    pub fn mark_privacy_degraded(&mut self) {
        self.privacy_degraded = true;
    }

    pub fn privacy_is_degraded(&self) -> bool {
        self.privacy_degraded
    }

    pub fn is_controller(&self) -> bool {
        self.controller
    }

    pub fn set_privacy(&mut self, on: bool) -> Result<(), String> {
        self.control(SessionControl {
            action: if on { SessionControl::PRIVACY_ON } else { SessionControl::PRIVACY_OFF },
            display_id: 0,
            profile: 0,
            flags: 0,
        })
    }

    pub fn send_clipboard(&mut self, text: &str) -> Result<(), String> {
        if text.len() > CLIPBOARD_MAX {
            return Err("剪贴板太大，最多 1 MiB".into());
        }
        if self.features & FEAT_CLIPBOARD == 0 && !self.controller {
            return Err("对方没有剪贴板能力".into());
        }
        let parts = split_clipboard(if self.controller { 1 } else { 2 }, text).map_err(|e| e.to_string())?;
        let count = parts.len() as u16;
        for (i, part) in parts.iter().enumerate() {
            // 传大文件的时候窗口很可能是满的，剪贴板也得能排进去等一会儿。
            self.queue_or_defer(0, MsgType::Clipboard as u8, 1, i as u16, count, part.encode().map_err(|e| e.to_string())?)?;
        }
        Ok(())
    }

    pub fn offer_file(&mut self, name: &str, bytes: &[u8]) -> Result<u32, String> {
        if name.len() > 255 || name.contains('/') || name.contains('\\') {
            return Err("文件名不合法".into());
        }
        let id = self.next_file;
        self.next_file = self.next_file.wrapping_add(1).max(1);
        let offer = FileAction::Offer {
            transfer_id: id,
            direction: if self.controller { 1 } else { 2 },
            name: name.into(),
            size: bytes.len() as u64,
            sha256: sha256(bytes),
        };
        let body = offer.encode().map_err(|e| e.to_string())?;
        // 窗口满不算失败：先记下这个文件，控制帧等窗口腾出来再发。
        // 刚传完一个文件马上再传一个，窗口可能还没排空。排不进去就先记着。
        self.queue_or_defer(6, MsgType::FileControl as u8, 0, 0, 1, body)?;
        self.pending_send.insert(id, SendFile { bytes: bytes.to_vec(), offset: 0, accepted: false, done: false });
        Ok(id)
    }

    /// 按可靠窗口把文件分片推出去。窗口满了就停，等确认回来再继续。
    /// 放在 poll_wire 里每拍调一次，长文件不会因为一次塞太多而失败。
    /// 排不进去就先存着，等窗口腾出来再发。
    /// 「窗口满」不该让用户的一次操作失败：取消正在传的大文件时窗口一定是满的。
    fn queue_or_defer(&mut self, stream: u8, msg_type: u8, frame_id: u32, slice_index: u16, slice_count: u16, payload: Vec<u8>) -> Result<(), String> {
        if self.queue(stream, msg_type, frame_id, slice_index, slice_count, payload.clone()).is_ok() {
            return Ok(());
        }
        // 存也要有上限，不然一直失败的调用方会把内存吃光。
        if self.deferred.len() >= 32 {
            return Err("发送队列已满".into());
        }
        self.deferred.push_back((stream, msg_type, frame_id, slice_index, slice_count, payload));
        Ok(())
    }

    fn flush_deferred(&mut self) {
        while let Some((stream, msg_type, frame_id, slice_index, slice_count, payload)) = self.deferred.front() {
            if self.queue(*stream, *msg_type, *frame_id, *slice_index, *slice_count, payload.clone()).is_err() {
                return;
            }
            self.deferred.pop_front();
        }
    }

    /// 还有多少控制帧在等窗口。界面可以拿它显示「排队中」。
    pub fn deferred_len(&self) -> usize {
        self.deferred.len()
    }

    fn pump_files(&mut self) {
        const BUDGET: usize = 32;
        let ids: Vec<u32> = self.pending_send.iter().filter(|(_, f)| f.accepted && !f.done).map(|(id, _)| *id).collect();
        for id in ids {
            let mut sent = 0usize;
            while sent < BUDGET {
                if self.reliable.free_window(6) == 0 {
                    return;
                }
                let planned = self.pending_send.get(&id).and_then(|f| {
                    let start = f.offset as usize;
                    if start >= f.bytes.len() {
                        return None;
                    }
                    let end = (start + jr_proto::FILE_CHUNK).min(f.bytes.len());
                    Some((f.offset, end - start, f.bytes[start..end].to_vec()))
                });
                match planned {
                    None => {
                        let payload = FileAction::Complete { transfer_id: id }.encode().unwrap();
                        if self.queue(6, MsgType::FileControl as u8, 0, 0, 1, payload).is_err() {
                            return;
                        }
                        if let Some(file) = self.pending_send.get_mut(&id) {
                            file.done = true;
                        }
                        break;
                    }
                    Some((offset, advance, bytes)) => {
                        let payload = FileChunk { transfer_id: id, offset, bytes }.encode().unwrap();
                        if self.queue(6, MsgType::FileChunk as u8, 0, 0, 1, payload).is_err() {
                            return;
                        }
                        if let Some(file) = self.pending_send.get_mut(&id) {
                            file.offset += advance as u64;
                        }
                        sent += 1;
                    }
                }
            }
        }
        self.pending_send.retain(|_, f| !f.done);
    }

    /// 发到哪儿了。界面和命令行用它显示进度。
    pub fn file_progress(&self, id: u32) -> Option<(u64, u64)> {
        self.pending_send.get(&id).map(|f| (f.offset, f.bytes.len() as u64))
    }

    pub fn file_chunk(&mut self, id: u32, offset: u64, bytes: &[u8]) -> Result<(), String> {
        let chunk = FileChunk { transfer_id: id, offset, bytes: bytes.to_vec() };
        self.queue(6, MsgType::FileChunk as u8, 0, 0, 1, chunk.encode().map_err(|e| e.to_string())?)
    }

    pub fn cancel_file(&mut self, id: u32) -> Result<(), String> {
        self.pending_send.remove(&id);
        // 正在传的时候窗口一定是满的，所以这个取消必须能排进去等着。
        self.queue_or_defer(6, MsgType::FileControl as u8, 0, 0, 1, FileAction::Cancel { transfer_id: id }.encode().unwrap())
    }

    pub fn on_payload(&mut self, msg_type: u8, frame_id: u32, slice_index: u16, slice_count: u16, timestamp_us: u64, now_us: u64, body_flags: u16, payload: &[u8]) -> Vec<SessionEvent> {
        let mut ev = Vec::new();
        match MsgType::from_u8(msg_type) {
            Some(MsgType::Ping) => {
                if payload.len() >= 8 {
                    self.hot.push_back(WireMsg {
                        msg_type: MsgType::Pong as u8,
                        stream: 0,
                        frame_id: 0,
                        slice_index: 0,
                        slice_count: 1,
                        body_flags: 0,
                        timestamp_us: 0,
                        body: payload[..8].to_vec(),
                    });
                }
            }
            Some(MsgType::Pong) => {
                if payload.len() >= 8 {
                    if let Ok(sent) = payload[..8].try_into().map(u64::from_le_bytes) {
                        if now_us > sent {
                            self.reliable.note_rtt(((now_us - sent) / 1000).min(u32::MAX as u64) as u32);
                        }
                    }
                }
            }
            Some(MsgType::Capability) => {
                if let Ok(cap) = Capability::decode(payload) {
                    ev.push(SessionEvent::RemoteCapability {
                        video: cap.feature_bits & FEAT_VIDEO != 0,
                        width: cap.displays.first().map(|d| d.width).unwrap_or(0),
                        height: cap.displays.first().map(|d| d.height).unwrap_or(0),
                        displays: cap.displays.iter().map(|d| (d.width, d.height)).collect(),
                    });
                }
            }
            Some(MsgType::Video) => {
                if body_flags & FLAG_FEC != 0 || payload.is_empty() {
                    return ev;
                }
                let inner = Inner {
                    msg_type,
                    stream_id: 1,
                    body_flags,
                    frame_id,
                    slice_index,
                    slice_count: slice_count.max(1),
                    timestamp_us,
                    body: payload.to_vec(),
                };
                for item in self.video.push(&inner, now_us) {
                    match item {
                        Reassembled::Frame { bytes, idr, .. } => {
                            self.rx.note_frame(now_us / 1000, bytes.len(), frame_id);
                            ev.push(SessionEvent::Video { idr, bytes });
                        }
                        Reassembled::Lost { frame_id, reference: true } => {
                            self.rx.note_lost(1);
                            ev.push(SessionEvent::NeedKeyframe(frame_id));
                        }
                        Reassembled::Lost { .. } => self.rx.note_lost(1),
                    }
                }
            }
            Some(MsgType::Audio) => {
                if self.controller && self.features & FEAT_AUDIO_OUT != 0 {
                    if let Ok(frame) = AudioFrame::decode(payload) {
                        ev.push(SessionEvent::Audio { timestamp_us, bytes: frame.payload });
                    }
                }
            }
            Some(MsgType::Feedback) => {
                if self.controller {
                    return ev;
                }
                let Ok(fb) = Feedback::decode(payload) else {
                    return ev;
                };
                let elapsed = if self.last_fb_us == 0 {
                    FEEDBACK_WINDOW_MS as u32
                } else {
                    ((now_us.saturating_sub(self.last_fb_us)) / 1000).min(u32::MAX as u64) as u32
                };
                self.last_fb_us = now_us;
                if self.jcc.on_feedback(&fb, self.stream_w, self.stream_h, elapsed) {
                    ev.push(SessionEvent::Bitrate { bps: self.jcc.target_bps, fps: self.jcc.fps });
                }
            }
            Some(MsgType::SessionControl) => ev.extend(self.on_control(payload)),
            Some(MsgType::Input) => ev.extend(self.on_input(timestamp_us, now_us, payload)),
            Some(MsgType::CursorPos) => {
                if payload.len() == 9 {
                    self.cursor.x = i32::from_le_bytes(payload[0..4].try_into().unwrap());
                    self.cursor.y = i32::from_le_bytes(payload[4..8].try_into().unwrap());
                    self.cursor.visible = payload[8] != 0;
                    ev.push(SessionEvent::Cursor(self.cursor.clone()));
                }
            }
            Some(MsgType::CursorShape) => {
                if self.controller {
                    let max = CURSOR_SHAPE_HEADER + (CURSOR_SHAPE_MAX as usize) * (CURSOR_SHAPE_MAX as usize) * 4;
                    // 分片数也要设上限，别让对端用一个巨大的 slice_count 撑内存。
                    if slice_count == 0
                        || slice_count as usize > 1024
                        || slice_index >= slice_count
                        || payload.len() > max
                    {
                        self.cursor_shape_parts = None;
                    } else {
                        let start = !matches!(
                            &self.cursor_shape_parts,
                            Some(parts) if parts.frame == frame_id && parts.count == slice_count
                        );
                        if start {
                            self.cursor_shape_parts = Some(CursorParts {
                                frame: frame_id,
                                count: slice_count,
                                pieces: vec![None; slice_count as usize],
                                total: 0,
                            });
                        }
                        let mut overflow = false;
                        if let Some(parts) = self.cursor_shape_parts.as_mut() {
                            let idx = slice_index as usize;
                            if parts.pieces[idx].is_none() {
                                if parts.total.saturating_add(payload.len()) > max {
                                    overflow = true;
                                } else {
                                    parts.total += payload.len();
                                    parts.pieces[idx] = Some(payload.to_vec());
                                }
                            }
                        }
                        if overflow {
                            self.cursor_shape_parts = None;
                        }
                        let complete = self
                            .cursor_shape_parts
                            .as_ref()
                            .map(|parts| parts.pieces.iter().all(|p| p.is_some()))
                            .unwrap_or(false);
                        if complete {
                            let parts = self.cursor_shape_parts.take().unwrap();
                            let mut body = Vec::with_capacity(parts.total);
                            for piece in parts.pieces.into_iter().flatten() {
                                body.extend_from_slice(&piece);
                            }
                            if let Ok(shape) = CursorShape::decode(&body) {
                                ev.push(SessionEvent::CursorShape(shape));
                            }
                        }
                    }
                }
            }
            Some(MsgType::Clipboard) => ev.extend(self.on_clip(frame_id, slice_index, slice_count, payload)),
            Some(MsgType::FileControl) => ev.extend(self.on_file_control(payload)),
            Some(MsgType::FileChunk) => ev.extend(self.on_chunk(payload)),
            Some(MsgType::TerminalOpen) => {
                if !self.controller && self.features & FEAT_TERMINAL != 0 && payload.len() >= 4 {
                    let cols = u16::from_le_bytes([payload[0], payload[1]]);
                    let rows = u16::from_le_bytes([payload[2], payload[3]]);
                    let term = String::from_utf8_lossy(&payload[4..]).to_string();
                    ev.push(SessionEvent::TerminalOpen { cols, rows, term });
                }
            }
            Some(MsgType::TerminalData) => {
                if self.controller || self.features & FEAT_TERMINAL != 0 {
                    ev.push(SessionEvent::TerminalData(payload.to_vec()));
                }
            }
            Some(MsgType::TerminalResize) => {
                if !self.controller && self.features & FEAT_TERMINAL != 0 && payload.len() == 4 {
                    ev.push(SessionEvent::TerminalResize {
                        cols: u16::from_le_bytes([payload[0], payload[1]]),
                        rows: u16::from_le_bytes([payload[2], payload[3]]),
                    });
                }
            }
            Some(MsgType::Bye) => {
                let code = if payload.len() >= 2 {
                    u16::from_le_bytes([payload[0], payload[1]])
                } else {
                    0
                };
                ev.extend(self.close(Reason::from_u16(code).unwrap_or(Reason::Ok)));
            }
            Some(MsgType::KeyframeRequest) => {
                if payload.len() >= 4 {
                    let id = u32::from_le_bytes(payload[..4].try_into().unwrap());
                    ev.push(SessionEvent::NeedKeyframe(id));
                }
            }
            _ => {}
        }
        ev
    }

    pub fn tick(&mut self, now_us: u64) -> Vec<SessionEvent> {
        let mut ev: Vec<SessionEvent> = self
            .input
            .release_stale(now_us)
            .into_iter()
            .map(SessionEvent::Inject)
            .collect();
        if !self.reliable_failed_reported && self.reliable.failed() {
            self.reliable_failed_reported = true;
            ev.push(SessionEvent::ReliableFailed);
        }
        ev
    }

    pub fn close(&mut self, reason: Reason) -> Vec<SessionEvent> {
        if self.released {
            return vec![SessionEvent::Closed(reason)];
        }
        self.released = true;
        self.capture_on = false;
        let mut ev = vec![SessionEvent::ReleaseAll];
        if self.privacy_on {
            self.privacy_on = false;
            ev.push(SessionEvent::PrivacyOff);
        }
        for up in self.input.release_all() {
            ev.push(SessionEvent::Inject(up));
        }
        for id in self.files.keys().copied().collect::<Vec<_>>() {
            ev.extend(self.drop_file(id, "会话结束"));
        }
        ev.push(SessionEvent::Closed(reason));
        ev
    }

    pub fn write_recovery(path: &Path) {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::write(path, b"release_all\n");
    }

    pub fn clear_recovery(path: &Path) {
        let _ = fs::remove_file(path);
    }

    pub fn recovery_pending(path: &Path) -> bool {
        fs::read(path).ok().as_deref() == Some(b"release_all\n")
    }

    fn queue(&mut self, stream: u8, msg_type: u8, frame_id: u32, slice_index: u16, slice_count: u16, payload: Vec<u8>) -> Result<(), String> {
        let seq = self
            .reliable
            .enqueue(stream, msg_type as u32, payload)
            .map_err(|e| e.to_string())?;
        self.out_meta.insert((stream, seq), OutMeta { msg_type, frame_id, slice_index, slice_count });
        Ok(())
    }

    fn control(&mut self, msg: SessionControl) -> Result<(), String> {
        self.queue(0, MsgType::SessionControl as u8, 0, 0, 1, msg.encode())
    }

    fn on_control(&mut self, payload: &[u8]) -> Vec<SessionEvent> {
        let Ok(msg) = SessionControl::decode(payload) else {
            return Vec::new();
        };
        let mut ev = Vec::new();
        match msg.action {
            SessionControl::START => {
                if self.features & FEAT_VIDEO != 0 {
                    self.capture_on = true;
                    // 开始流时带上主控选的档。以前这个字段被丢掉，窗口上选的档到不了编码器。
                    if (1..=4).contains(&msg.profile) {
                        ev.push(SessionEvent::Quality(msg.profile));
                    }
                    ev.push(SessionEvent::StreamStart { display: msg.display_id });
                }
            }
            SessionControl::SWITCH_DISPLAY => {
                if self.features & FEAT_VIDEO != 0 {
                    ev.push(SessionEvent::StreamStart { display: msg.display_id });
                }
            }
            SessionControl::END => {
                // 对方要求结束：先停采集，再走完整的关闭流程（含 release_all）。
                self.capture_on = false;
                ev.push(SessionEvent::StreamStop);
                ev.extend(self.close(Reason::UserHangup));
            }
            SessionControl::PRIVACY_ON => {
                if !self.privacy_supported || self.features & FEAT_PRIVACY == 0 {
                    ev.push(SessionEvent::PrivacyRejected);
                    let _ = self.queue(
                        0,
                        MsgType::SessionControl as u8,
                        0,
                        0,
                        1,
                        SessionControl {
                            action: SessionControl::PRIVACY_OFF,
                            display_id: 0,
                            profile: 0,
                            flags: SessionControl::FLAG_REJECTED,
                        }
                        .encode(),
                    );
                } else {
                    self.privacy_on = true;
                    if self.privacy_degraded {
                        // 软件遮挡也算开了，但要如实告诉对面这只是遮挡。
                        ev.push(SessionEvent::PrivacyDegraded);
                        // 只有被控端回执。主控也会收到这条降级通知，如果再回一次，
                        // 两端就会互发 PRIVACY_ON+DEGRADED，把可靠窗口塞满。
                        if !self.controller {
                            let _ = self.queue(
                                0,
                                MsgType::SessionControl as u8,
                                0,
                                0,
                                1,
                                SessionControl {
                                    action: SessionControl::PRIVACY_ON,
                                    display_id: 0,
                                    profile: 0,
                                    flags: SessionControl::FLAG_DEGRADED,
                                }
                                .encode(),
                            );
                        }
                    }
                }
            }
            SessionControl::PRIVACY_OFF => {
                self.privacy_on = false;
                ev.push(SessionEvent::PrivacyOff);
                if msg.flags & SessionControl::FLAG_DEGRADED != 0 {
                    ev.push(SessionEvent::PrivacyDegraded);
                }
                if msg.flags & SessionControl::FLAG_REJECTED != 0 {
                    ev.push(SessionEvent::PrivacyRejected);
                }
            }
            SessionControl::PAUSE_INPUT => self.input_paused = true,
            SessionControl::RESUME_INPUT => self.input_paused = false,
            SessionControl::SWITCH_PROFILE => ev.push(SessionEvent::Quality(msg.profile)),
            _ => {}
        }
        ev
    }

    fn on_input(&mut self, timestamp_us: u64, now_us: u64, payload: &[u8]) -> Vec<SessionEvent> {
        if self.input_paused || self.features & FEAT_WATCH_ONLY != 0 || self.features & FEAT_INPUT == 0 {
            return Vec::new();
        }
        let Ok(event) = InputEvent::decode(payload) else {
            return Vec::new();
        };
        if event.kind == 7 {
            return Vec::new();
        }
        if self.input.accept(timestamp_us, now_us, &event) {
            vec![SessionEvent::Inject(event)]
        } else {
            Vec::new()
        }
    }

    fn on_clip(&mut self, frame_id: u32, index: u16, count: u16, payload: &[u8]) -> Vec<SessionEvent> {
        let Ok(part) = ClipboardPart::decode(payload) else {
            return Vec::new();
        };
        if part.total_len as usize > CLIPBOARD_MAX
            || count == 0
            || count as usize > CLIP_PARTS_MAX
        {
            return Vec::new();
        }
        // frame_id 由对端给，不能信它会自己收敛。同一批的分片数或总长
        // 对不上就整批丢掉，避免拿第一批的 count 分配出一个巨大的空表。
        if let Some(batch) = self.clips.get(&frame_id) {
            if batch.total != part.total_len || batch.got.len() != count as usize {
                self.clips.remove(&frame_id);
            }
        }
        if !self.clips.contains_key(&frame_id) && self.clips.len() >= CLIP_BATCH_MAX {
            // 正常只会有一批未完成。到上限说明对端在丢半截批次，
            // 直接清空比无限涨内存好；没拼完的剪贴板本来也用不了。
            self.clips.clear();
        }
        let batch = self.clips.entry(frame_id).or_insert_with(|| ClipBatch {
            total: part.total_len,
            got: vec![None; count as usize],
        });
        if (index as usize) < batch.got.len() {
            batch.got[index as usize] = Some(part.bytes);
        }
        if batch.got.iter().any(|p| p.is_none()) {
            return Vec::new();
        }
        let batch = self.clips.remove(&frame_id).unwrap();
        let mut raw = Vec::new();
        for piece in batch.got.into_iter().flatten() {
            raw.extend(piece);
        }
        if raw.len() != batch.total as usize {
            return Vec::new();
        }
        match String::from_utf8(raw) {
            Ok(text) => vec![SessionEvent::Clipboard(text)],
            Err(_) => Vec::new(),
        }
    }

    fn on_file_control(&mut self, payload: &[u8]) -> Vec<SessionEvent> {
        let Ok(action) = FileAction::decode(payload) else {
            return Vec::new();
        };
        match action {
            FileAction::Offer { transfer_id, name, size, sha256, .. } => {
                if self.features & FEAT_FILE == 0 && !self.controller {
                    let _ = self.queue(6, MsgType::FileControl as u8, 0, 0, 1, FileAction::Reject { transfer_id }.encode().unwrap());
                return Vec::new();
                }
                let safe = sanitize_name(&name);
                let final_path = unique_path(&self.inbox, &safe);
                let final_name = final_path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("file")
                    .to_string();
                let tmp = self.inbox.join(format!(".{transfer_id}.partial"));
                let _ = fs::write(&tmp, b"");
                self.files.insert(transfer_id, RecvFile { tmp: tmp.clone(), final_name: final_name.clone(), size, expect: sha256, written: 0 });
                let payload = FileAction::Accept { transfer_id, final_name }.encode().unwrap();
                // 窗口满时排队等，不能把 Accept 丢掉；丢了发送方会一直等，文件卡死。
                if let Err(why) = self.queue_or_defer(6, MsgType::FileControl as u8, 0, 0, 1, payload) {
                    self.files.remove(&transfer_id);
                    let _ = fs::remove_file(&tmp);
                    return vec![SessionEvent::FileFailed { id: transfer_id, why }];
                }
                Vec::new()
            }
            FileAction::Cancel { transfer_id } | FileAction::Error { transfer_id } => {
                self.drop_file(transfer_id, "已取消")
            }
            FileAction::Complete { transfer_id } => self.finish_file(transfer_id),
            FileAction::Accept { transfer_id, .. } => {
                if let Some(file) = self.pending_send.get_mut(&transfer_id) {
                    file.accepted = true;
                }
                self.pump_files();
                Vec::new()
            }
            FileAction::Reject { transfer_id } => {
                self.pending_send.remove(&transfer_id);
                vec![SessionEvent::FileFailed { id: transfer_id, why: "对方拒绝".into() }]
            }
        }
    }

    fn on_chunk(&mut self, payload: &[u8]) -> Vec<SessionEvent> {
        let Ok(chunk) = FileChunk::decode(payload) else {
            return Vec::new();
        };
        let Some(file) = self.files.get_mut(&chunk.transfer_id) else {
            return Vec::new();
        };
        if chunk.offset != file.written || file.written + chunk.bytes.len() as u64 > file.size {
            let id = chunk.transfer_id;
            return self.drop_file(id, "偏移不对");
        }
        if fs::OpenOptions::new().append(true).open(&file.tmp).and_then(|mut f| {
            use std::io::Write;
            f.write_all(&chunk.bytes)
        }).is_err() {
            let id = chunk.transfer_id;
            return self.drop_file(id, "写不进去");
        }
        file.written += chunk.bytes.len() as u64;
        Vec::new()
    }

    fn finish_file(&mut self, id: u32) -> Vec<SessionEvent> {
        let Some(file) = self.files.remove(&id) else {
            return Vec::new();
        };
        let data = fs::read(&file.tmp).unwrap_or_default();
        if data.len() as u64 != file.size || sha256(&data) != file.expect {
            let _ = fs::remove_file(&file.tmp);
            return vec![SessionEvent::FileFailed { id, why: "校验失败".into() }];
        }
        let dest = self.inbox.join(&file.final_name);
        if fs::rename(&file.tmp, &dest).is_err() {
            let _ = fs::remove_file(&file.tmp);
            return vec![SessionEvent::FileFailed { id, why: "改名失败".into() }];
        }
        vec![SessionEvent::FileDone { id, path: dest }]
    }

    fn drop_file(&mut self, id: u32, why: &str) -> Vec<SessionEvent> {
        if let Some(file) = self.files.remove(&id) {
            let _ = fs::remove_file(file.tmp);
        }
        vec![SessionEvent::FileFailed { id, why: why.into() }]
    }
}

pub fn split_access_unit(bytes: &[u8], max: usize) -> Vec<Vec<u8>> {
    let max = max.max(1);
    if bytes.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let end = (i + max).min(bytes.len());
        out.push(bytes[i..end].to_vec());
        i = end;
    }
    out
}

fn is_reliable(msg_type: u8) -> bool {
    matches!(
        MsgType::from_u8(msg_type),
        Some(
            MsgType::Capability
                | MsgType::SessionControl
                | MsgType::Clipboard
                | MsgType::KeyframeRequest
                | MsgType::Bye
                | MsgType::CursorShape
                | MsgType::FileControl
                | MsgType::FileChunk
                | MsgType::TerminalOpen
                | MsgType::TerminalData
                | MsgType::TerminalResize
        )
    )
}

fn sanitize_name(name: &str) -> String {
    let base = Path::new(name)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("file");
    let clean: String = base.chars().filter(|c| *c != '/' && *c != '\\' && *c != '\0').collect();
    if clean.is_empty() { "file".into() } else { clean }
}

fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let plain = dir.join(name);
    if !plain.exists() {
        return plain;
    }
    let stem = Path::new(name).file_stem().and_then(|s| s.to_str()).unwrap_or("file");
    let ext = Path::new(name).extension().and_then(|s| s.to_str());
    for n in 2..1000 {
        let candidate = match ext {
            Some(ext) => dir.join(format!("{stem} ({n}).{ext}")),
            None => dir.join(format!("{stem} ({n})")),
        };
        if !candidate.exists() {
            return candidate;
        }
    }
    dir.join(format!("{stem}.dup"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostProfile {
    pub features: u32,
    pub blocker: u8,
    pub unattended: bool,
    pub label: &'static str,
}

pub fn host_profile(platform: u8) -> HostProfile {
    match platform {
        PLATFORM_IOS => HostProfile { features: 0, blocker: 0, unattended: false, label: "不能被控" },
        PLATFORM_LINUX => HostProfile {
            features: FEAT_CLIPBOARD | FEAT_FILE | FEAT_TERMINAL,
            blocker: 4,
            unattended: true,
            label: "仅终端",
        },
        PLATFORM_ANDROID => HostProfile {
            features: FEAT_CLIPBOARD | FEAT_FILE,
            blocker: 3,
            unattended: false,
            label: "需重新允许录屏",
        },
        _ => HostProfile {
            features: desktop_features(false, false, false),
            blocker: 1,
            unattended: true,
            label: "需授权",
        },
    }
}

pub fn desktop_features(video: bool, input: bool, privacy: bool) -> u32 {
    let mut bits = FEAT_CLIPBOARD | FEAT_FILE | FEAT_TERMINAL;
    if video {
        bits |= FEAT_VIDEO;
    }
    if input {
        bits |= FEAT_INPUT;
    }
    if privacy {
        bits |= FEAT_PRIVACY;
    }
    bits
}

pub fn ios_cannot_host() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use jr_proto::FEAT_INPUT;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("jrp-sess-{}-{}", std::process::id(), NEXT_DIR.fetch_add(1, Ordering::Relaxed)));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn feedback_is_actually_queued_for_sending() {
        let dir = scratch();
        let mut ctrl = Engine::new(true, dir, FEAT_INPUT, false);
        assert!(ctrl.feedback_due(1_000_000).is_none(), "第一个窗口只记起点");
        let fb = ctrl.feedback_due(1_000_000 + FEEDBACK_WINDOW_MS * 1000).expect("窗口满了要出反馈");
        ctrl.send_feedback(&fb);
        let wire = ctrl.poll_wire(0);
        assert!(
            wire.iter().any(|m| m.msg_type == MsgType::Feedback as u8 && Feedback::decode(&m.body).is_ok()),
            "反馈必须真的进入发送队列"
        );
    }

    #[test]
    fn terminal_open_requires_host_capability() {
        let dir = scratch();
        let mut host = Engine::new(false, dir.join("host"), 0, false);
        let payload = [80u8, 0, 24, 0];
        assert!(host.on_payload(MsgType::TerminalOpen as u8, 0, 0, 1, 0, 0, 0, &payload).is_empty());
        let mut allowed = Engine::new(false, dir.join("allowed"), FEAT_TERMINAL, false);
        assert_eq!(
            allowed.on_payload(MsgType::TerminalOpen as u8, 0, 0, 1, 0, 0, 0, &payload),
            vec![SessionEvent::TerminalOpen { cols: 80, rows: 24, term: String::new() }]
        );
    }

    #[test]
    fn cursor_shape_reaches_the_controller() {
        let dir = scratch();
        let mut host = Engine::new(false, dir.join("host"), 0, false);
        let mut ctrl = Engine::new(true, dir.join("ctrl"), 0, false);
        let shape = CursorShape::new(0, 0, 2, 1, vec![1, 2, 3, 255, 4, 5, 6, 255]);
        host.send_cursor_shape(&shape).unwrap();
        let wire = host.poll_wire(0);
        let msg = wire.iter().find(|m| m.msg_type == MsgType::CursorShape as u8).expect("形状要进发送队列");
        let ev = ctrl.ingest(msg.msg_type, msg.stream, msg.frame_id, msg.slice_index, msg.slice_count, 0, 0, 0, &msg.body);
        assert_eq!(ev, vec![SessionEvent::CursorShape(shape)]);
    }

    #[test]
    fn large_cursor_shape_is_split_and_reassembled() {
        let dir = scratch();
        let mut host = Engine::new(false, dir.join("host"), 0, false);
        let mut ctrl = Engine::new(true, dir.join("ctrl"), 0, false);
        let shape = CursorShape::new(4, 6, 32, 32, vec![9u8; 32 * 32 * 4]);
        host.send_cursor_shape(&shape).unwrap();
        let wire: Vec<_> = host
            .poll_wire(0)
            .into_iter()
            .filter(|m| m.msg_type == MsgType::CursorShape as u8)
            .collect();
        assert!(wire.len() > 1, "32×32 必须分片");
        assert!(wire.iter().all(|m| m.stream == 4), "光标形状走可靠流 4");
        assert!(wire.iter().all(|m| m.slice_count == wire.len() as u16));
        let mut events = Vec::new();
        // 可靠层是保序的，但重组不能依赖到达顺序；倒着喂一遍。
        for msg in wire.into_iter().rev() {
            events.extend(ctrl.ingest(msg.msg_type, msg.stream, msg.frame_id, msg.slice_index, msg.slice_count, 0, 0, 0, &msg.body));
        }
        assert_eq!(events, vec![SessionEvent::CursorShape(shape)]);
    }

    #[test]
    fn reliable_failure_is_reported_once() {
        let dir = scratch();
        let mut engine = Engine::new(true, dir, FEAT_TERMINAL, false);
        engine.request_keyframe(1).unwrap();
        // 没有任何确认回来，重传会耗尽。
        for i in 0..12 {
            let _ = engine.poll_wire(i * 1000);
        }
        let first = engine.tick(0);
        assert_eq!(
            first.iter().filter(|e| matches!(e, SessionEvent::ReliableFailed)).count(),
            1,
            "可靠流失败要产生一次事件"
        );
        let second = engine.tick(0);
        assert!(
            !second.iter().any(|e| matches!(e, SessionEvent::ReliableFailed)),
            "同一失败不能每拍都报"
        );
    }

    #[test]
    fn clipboard_over_limit_rejected_and_file_cancel_leaves_no_final() {
        let dir = scratch();
        let mut host = Engine::new(false, dir.join("in"), desktop_features(false, true, false), false);
        assert!(host.send_clipboard(&"a".repeat(CLIPBOARD_MAX + 1)).is_err());
        let text = "hello clipboard";
        let parts = split_clipboard(1, text).unwrap();
        let ev = host.on_payload(MsgType::Clipboard as u8, 3, 0, 1, 0, 0, 0, &parts[0].encode().unwrap());
        assert_eq!(ev, vec![SessionEvent::Clipboard(text.into())]);

        let mut ctrl = Engine::new(true, dir.join("c"), FEAT_FILE, false);
        let payload = b"abc";
        let id = ctrl.offer_file("note.txt", payload).unwrap();
        let offer = ctrl.reliable.poll(0).pop().unwrap();
        let accepted = host.on_payload(MsgType::FileControl as u8, 0, 0, 1, 0, 0, 0, &offer.payload);
        assert!(!accepted.iter().any(|e| matches!(e, SessionEvent::FileFailed { .. })));
        // Accept 必须真的排进发送队列，不能只产生一个没人处理的兜底事件。
        assert!(host.poll_wire(0).iter().any(|m| m.msg_type == MsgType::FileControl as u8));
        let chunk = FileChunk { transfer_id: id, offset: 0, bytes: payload.to_vec() };
        host.on_payload(MsgType::FileChunk as u8, 0, 0, 1, 0, 0, 0, &chunk.encode().unwrap());
        host.on_payload(MsgType::FileControl as u8, 0, 0, 1, 0, 0, 0, &FileAction::Cancel { transfer_id: id }.encode().unwrap());
        assert!(dir.join("in").read_dir().unwrap().filter_map(|e| e.ok()).all(|e| {
            let name = e.file_name().into_string().unwrap_or_default();
            !name.ends_with(".txt") && !name.contains("partial")
        }));
    }

    #[test]
    fn clipboard_reassembly_rejects_absurd_counts_and_caps_batches() {
        let dir = scratch();
        let mut host = Engine::new(false, dir, FEAT_CLIPBOARD, false);
        let part = ClipboardPart {
            direction: 1,
            mime: 1,
            total_len: 20,
            bytes: vec![b'x'; 10],
        }
        .encode()
        .unwrap();

        // 外层 slice_count 由对端填，不能被它拿来一次分配 65535 个槽。
        let ev = host.on_payload(MsgType::Clipboard as u8, 9, 0, u16::MAX, 0, 0, 0, &part);
        assert!(ev.is_empty());
        assert!(host.clips.is_empty());

        // 只收到一半的批次不能无限堆在 map 里。
        for frame in 1..=(CLIP_BATCH_MAX as u32 + 4) {
            host.on_payload(MsgType::Clipboard as u8, frame, 0, 2, 0, 0, 0, &part);
            assert!(host.clips.len() <= CLIP_BATCH_MAX);
        }
    }

    #[test]
    fn cursor_shape_reassembly_caps_fragments_and_payload() {
        let dir = scratch();
        let mut ctrl = Engine::new(true, dir, 0, false);

        // 分片数超过上限时直接丢，不按对端给的数字建表。
        let ev = ctrl.on_payload(MsgType::CursorShape as u8, 1, 0, u16::MAX, 0, 0, 0, &[0; 16]);
        assert!(ev.is_empty());
        assert!(ctrl.cursor_shape_parts.is_none());

        // 每片都合法，但加起来超过形状上限：继续收下去会白占内存，
        // 应该在越界时把批次丢掉，而不是等拼完再判断。
        let payload = vec![0u8; 1024];
        let mut emitted = false;
        for index in 0..100u16 {
            emitted |= !ctrl
                .on_payload(MsgType::CursorShape as u8, 2, index, 100, 0, 0, 0, &payload)
                .is_empty();
        }
        assert!(!emitted);
        let max = CURSOR_SHAPE_HEADER + (CURSOR_SHAPE_MAX as usize) * (CURSOR_SHAPE_MAX as usize) * 4;
        assert!(ctrl.cursor_shape_parts.as_ref().map(|p| p.total).unwrap_or(0) <= max);
    }

    #[test]
    fn degraded_privacy_reply_only_comes_from_host() {
        let dir = scratch();
        let payload = SessionControl { action: SessionControl::PRIVACY_ON, display_id: 0, profile: 0, flags: 0 }.encode();

        let mut host = Engine::new(false, dir.join("host"), FEAT_PRIVACY, true);
        host.mark_privacy_degraded();
        let ev = host.on_payload(MsgType::SessionControl as u8, 0, 0, 1, 0, 0, 0, &payload);
        assert!(ev.iter().any(|e| matches!(e, SessionEvent::PrivacyDegraded)));
        let replied = host
            .poll_wire(0)
            .into_iter()
            .any(|m| m.msg_type == MsgType::SessionControl as u8);
        assert!(replied, "被控端要回执降级");

        let mut ctrl = Engine::new(true, dir.join("ctrl"), FEAT_PRIVACY, true);
        ctrl.mark_privacy_degraded();
        let ev = ctrl.on_payload(MsgType::SessionControl as u8, 0, 0, 1, 0, 0, 0, &payload);
        assert!(ev.iter().any(|e| matches!(e, SessionEvent::PrivacyDegraded)));
        assert!(ctrl.poll_wire(0).is_empty(), "主控不能再回执，否则两端会互发");
    }

    #[test]
    fn privacy_rejected_on_mac_and_release_file_survives() {
        let dir = scratch();
        let mut host = Engine::new(false, dir.clone(), desktop_features(true, true, false), false);
        let ev = host.on_payload(
            MsgType::SessionControl as u8,
            0,
            0,
            1,
            0,
            0,
            0,
            &SessionControl { action: SessionControl::PRIVACY_ON, display_id: 0, profile: 0, flags: 0 }.encode(),
        );
        assert!(ev.iter().any(|e| matches!(e, SessionEvent::PrivacyRejected)));
        let rec = dir.join("recovery");
        Engine::write_recovery(&rec);
        assert!(Engine::recovery_pending(&rec));
        let closed = host.close(Reason::SessionDead);
        assert!(closed.iter().any(|e| matches!(e, SessionEvent::ReleaseAll)));
        Engine::clear_recovery(&rec);
        assert!(!Engine::recovery_pending(&rec));
        let _ = FEAT_INPUT;
    }

    #[test]
    fn stale_click_not_injected() {
        let dir = scratch();
        let mut host = Engine::new(false, dir, desktop_features(false, true, false), false);
        let down = InputEvent::key(1, 0x1e, true);
        let ev = host.on_payload(MsgType::Input as u8, 0, 0, 1, 0, 300_000, 0, &down.encode().unwrap());
        assert!(ev.is_empty());
    }

    /// 超过一个确认窗口的文件必须能传完。以前所有分片一次性入队，
    /// 第 65 片起就被静默丢掉，最后只会报校验失败。
    #[test]
    fn file_far_larger_than_the_window_arrives_intact() {
        let dir = scratch();
        let mut ctrl = Engine::new(true, dir.join("c"), FEAT_FILE, false);
        let mut host = Engine::new(false, dir.join("h"), desktop_features(false, false, false), false);
        let payload: Vec<u8> = (0..2_500_000u32).map(|i| (i % 251) as u8).collect();
        let id = ctrl.offer_file("big.bin", &payload).unwrap();
        let mut done = None;
        let mut t = 0u64;
        for _ in 0..4000 {
            t += 5;
            for wire in ctrl.poll_wire(t) {
                host.ingest(
                    wire.msg_type, wire.stream, wire.frame_id, wire.slice_index, wire.slice_count,
                    wire.timestamp_us, t * 1000, wire.body_flags, &wire.body,
                )
                .into_iter()
                .for_each(|ev| {
                    if let SessionEvent::FileDone { id: done_id, path } = ev {
                        done = Some((done_id, path));
                    }
                });
            }
            for wire in host.poll_wire(t) {
                ctrl.ingest(
                    wire.msg_type, wire.stream, wire.frame_id, wire.slice_index, wire.slice_count,
                    wire.timestamp_us, t * 1000, wire.body_flags, &wire.body,
                );
            }
            if done.is_some() {
                break;
            }
        }
        let (done_id, path) = done.expect("大文件应当传完");
        assert_eq!(done_id, id);
        assert_eq!(fs::read(&path).unwrap(), payload);
        assert!(ctrl.file_progress(id).is_none(), "传完就该从发送队列里拿掉");
    }

    /// 取消之后对端不能留下一个半截文件冒充完成。
    #[test]
    fn cancel_midway_leaves_no_final_file() {
        let dir = scratch();
        let mut ctrl = Engine::new(true, dir.join("c"), FEAT_FILE, false);
        let mut host = Engine::new(false, dir.join("h"), desktop_features(false, false, false), false);
        let payload: Vec<u8> = vec![7u8; 400_000];
        let id = ctrl.offer_file("half.bin", &payload).unwrap();
        let mut t = 0u64;
        for _ in 0..3 {
            t += 5;
            for wire in ctrl.poll_wire(t) {
                host.ingest(
                    wire.msg_type, wire.stream, wire.frame_id, wire.slice_index, wire.slice_count,
                    wire.timestamp_us, t * 1000, wire.body_flags, &wire.body,
                );
            }
            for wire in host.poll_wire(t) {
                ctrl.ingest(
                    wire.msg_type, wire.stream, wire.frame_id, wire.slice_index, wire.slice_count,
                    wire.timestamp_us, t * 1000, wire.body_flags, &wire.body,
                );
            }
        }
        assert!(ctrl.file_progress(id).is_some(), "这会儿应该才发了一半");
        ctrl.cancel_file(id).unwrap();
        for _ in 0..400 {
            t += 5;
            for wire in ctrl.poll_wire(t) {
                host.ingest(
                    wire.msg_type, wire.stream, wire.frame_id, wire.slice_index, wire.slice_count,
                    wire.timestamp_us, t * 1000, wire.body_flags, &wire.body,
                );
            }
            for wire in host.poll_wire(t) {
                ctrl.ingest(
                    wire.msg_type, wire.stream, wire.frame_id, wire.slice_index, wire.slice_count,
                    wire.timestamp_us, t * 1000, wire.body_flags, &wire.body,
                );
            }
        }
        let entries: Vec<String> = fs::read_dir(dir.join("h"))
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().into_string().unwrap_or_default())
            .collect();
        assert!(entries.iter().all(|n| !n.ends_with(".bin")), "不该留下 half.bin: {entries:?}");
        assert!(entries.iter().all(|n| !n.contains("partial")), "临时文件要清掉: {entries:?}");
        assert!(ctrl.file_progress(id).is_none());
    }

    #[test]
    fn audio_is_sent_unreliably_only_after_stream_start() {
        let dir = scratch();
        let mut host = Engine::new(false, dir.join("h"), FEAT_VIDEO | FEAT_AUDIO_OUT, false);
        let mut ctrl = Engine::new(true, dir.join("c"), FEAT_AUDIO_OUT, false);
        // 还没收到 START，不能凭空往对端送声音。
        assert!(!host.push_audio(&[0x21, 0x10, 0x05], 1));
        let start = SessionControl { action: SessionControl::START, display_id: 0, profile: 2, flags: 0 };
        host.on_control(&start.encode());
        assert!(host.push_audio(&[0x21, 0x10, 0x05], 42));
        let wire = host.poll_wire(0).into_iter().find(|m| m.msg_type == MsgType::Audio as u8).expect("音频要进发送队列");
        assert_eq!(wire.stream, 2);
        let events = ctrl.ingest(wire.msg_type, wire.stream, wire.frame_id, wire.slice_index, wire.slice_count, wire.timestamp_us, 0, wire.body_flags, &wire.body);
        assert!(events.contains(&SessionEvent::Audio { timestamp_us: 42, bytes: vec![0x21, 0x10, 0x05] }));
    }

    #[test]
    fn audio_is_not_queued_without_the_capability() {
        let dir = scratch();
        let mut host = Engine::new(false, dir.join("h"), FEAT_VIDEO, false);
        let start = SessionControl { action: SessionControl::START, display_id: 0, profile: 2, flags: 0 };
        host.on_control(&start.encode());
        assert!(!host.push_audio(&[1, 2, 3], 1));
    }

    #[test]
    fn video_slices_reassemble_without_a_screen() {
        let dir = scratch();
        let mut host = Engine::new(false, dir.join("h"), FEAT_VIDEO, false);
        let mut ctrl = Engine::new(true, dir.join("c"), 0, false);
        let mut annex = vec![0, 0, 0, 1, 0x65];
        annex.extend(std::iter::repeat(0x11).take(2400));
        host.push_video(7, true, &annex, 42);
        let wires = host.poll_wire(0);
        assert!(wires.len() > 1);
        let mut got = None;
        for wire in wires {
            for ev in ctrl.ingest(wire.msg_type, wire.stream, wire.frame_id, wire.slice_index, wire.slice_count, wire.timestamp_us, 1, wire.body_flags, &wire.body) {
                if let SessionEvent::Video { idr, bytes } = ev {
                    assert!(idr);
                    got = Some(bytes);
                }
            }
        }
        assert_eq!(got.as_deref(), Some(annex.as_slice()));
    }
}
