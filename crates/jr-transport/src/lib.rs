#![forbid(unsafe_code)]

use std::collections::{HashMap, VecDeque};

use jr_proto::{FecParity, Feedback, Inner, MsgType, ReliableHeader, FLAG_FEC, MAX_BODY, REL_HEADER_LEN};

pub const CONTROL_WINDOW: u32 = 64;
pub const MAX_RETRANSMIT: u8 = 8;
pub const JCC_BITS_PER_PIXEL_THRESHOLD: f32 = 0.07;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Profile {
    pub id: u8,
    pub start_bps: u32,
    pub max_bps: u32,
    pub max_fps: u16,
}

pub fn profile(id: u8) -> Profile {
    match id {
        1 => Profile { id: 1, start_bps: 8_000_000, max_bps: 30_000_000, max_fps: 60 },
        3 => Profile { id: 3, start_bps: 16_000_000, max_bps: 50_000_000, max_fps: 120 },
        4 => Profile { id: 4, start_bps: 1_500_000, max_bps: 4_000_000, max_fps: 30 },
        _ => Profile { id: 2, start_bps: 8_000_000, max_bps: 20_000_000, max_fps: 60 },
    }
}

pub fn path_score(loss_permille: u32, jitter_ms: u32, rtt_ms: u32) -> u32 {
    loss_permille
        .saturating_mul(10)
        .saturating_add(jitter_ms.saturating_mul(20))
        .saturating_add(rtt_ms)
}

pub fn choose_path(paths: &[(u8, u32)]) -> Option<u8> {
    paths.iter().min_by_key(|(_, score)| *score).map(|(id, _)| *id)
}

#[derive(Clone, Debug)]
pub struct Jcc {
    pub target_bps: u32,
    pub fps: u16,
    last_rtt: Option<u16>,
    rise_ms: u32,
    profile: Profile,
    applied_bps: u32,
}

impl Jcc {
    pub fn new(profile_id: u8) -> Self {
        let profile = profile(profile_id);
        Self {
            target_bps: profile.start_bps,
            fps: profile.max_fps.min(60),
            last_rtt: None,
            rise_ms: 0,
            profile,
            applied_bps: profile.start_bps,
        }
    }

    pub fn on_feedback(&mut self, fb: &Feedback, width: u32, height: u32, elapsed_ms: u32) -> bool {
        let loss = fb.loss_permille as f32 / 1000.0;
        let gradient = self
            .last_rtt
            .map(|prev| fb.rtt_ms as i32 - prev as i32)
            .unwrap_or(0);
        self.last_rtt = Some(fb.rtt_ms);
        let floor = (self.profile.start_bps / 2).max(500_000).min(self.profile.max_bps);
        if loss > 0.05 || gradient > 15 {
            self.target_bps = ((self.target_bps as f32) * 0.8) as u32;
            self.rise_ms = 0;
        } else if loss < 0.005 && gradient < 5 && fb.decode_queue == 0 {
            self.target_bps = ((self.target_bps as f32) * 1.05) as u32;
            self.rise_ms = self.rise_ms.saturating_add(elapsed_ms);
        }
        self.target_bps = self.target_bps.clamp(floor, self.profile.max_bps);
        let pixels = width.saturating_mul(height).max(1) as f32;
        let need = pixels * (self.fps as f32) * JCC_BITS_PER_PIXEL_THRESHOLD;
        if (self.target_bps as f32) < need {
            self.fps = match self.fps {
                120 => 60,
                60 => 30,
                _ => 15,
            };
            self.rise_ms = 0;
        } else if self.rise_ms >= 2000 && self.fps < self.profile.max_fps {
            self.fps = match self.fps {
                15 => 30,
                30 => 60,
                _ => self.profile.max_fps.min(120),
            };
            self.rise_ms = 0;
        }
        let delta = self.target_bps.abs_diff(self.applied_bps) as f32 / self.applied_bps.max(1) as f32;
        if delta >= 0.10 {
            self.applied_bps = self.target_bps;
            true
        } else {
            false
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RxStats {
    pub frames: u64,
    pub lost: u64,
    pub loss_permille: u16,
    pub jitter_ms: u16,
    pub bitrate_bps: u32,
    pub present_interval_ms: u16,
    pub highest_frame_id: u32,
}

/// 主控侧的收流统计。只数到达和丢失，不碰画面内容。
pub struct RxMeter {
    window_start_ms: u64,
    bytes: u64,
    frames: u64,
    lost: u64,
    highest: u32,
    last_frame_ms: Option<u64>,
    jitter_ms: u32,
    queue_len: u8,
}

impl Default for RxMeter {
    fn default() -> Self {
        Self::new()
    }
}

impl RxMeter {
    pub fn new() -> Self {
        Self {
            window_start_ms: 0,
            bytes: 0,
            frames: 0,
            lost: 0,
            highest: 0,
            last_frame_ms: None,
            jitter_ms: 0,
            queue_len: 0,
        }
    }

    pub fn note_frame(&mut self, now_ms: u64, bytes: usize, frame_id: u32) {
        self.bytes = self.bytes.saturating_add(bytes as u64);
        self.frames = self.frames.saturating_add(1);
        if frame_id > self.highest {
            self.highest = frame_id;
        }
        if let Some(prev) = self.last_frame_ms {
            let delta = now_ms.saturating_sub(prev) as i64;
            let expected = self.jitter_ms as i64;
            if expected == 0 {
                self.jitter_ms = delta.max(0) as u32;
            } else {
                let diff = (delta - expected).unsigned_abs() as u32;
                self.jitter_ms = (self.jitter_ms * 7 + diff) / 8;
            }
        }
        self.last_frame_ms = Some(now_ms);
    }

    pub fn note_lost(&mut self, count: u64) {
        self.lost = self.lost.saturating_add(count);
    }

    pub fn note_queue(&mut self, depth: u8) {
        self.queue_len = depth;
    }

    pub fn stats(&self) -> RxStats {
        let total = self.frames + self.lost;
        let loss_permille = if total == 0 { 0 } else { (self.lost.saturating_mul(1000) / total).min(1000) as u16 };
        RxStats {
            frames: self.frames,
            lost: self.lost,
            loss_permille,
            jitter_ms: self.jitter_ms.min(u16::MAX as u32) as u16,
            bitrate_bps: 0,
            present_interval_ms: 0,
            highest_frame_id: self.highest,
        }
    }

    /// 满一个窗口才出一份反馈。窗口内没有收到任何东西时只报丢失，不报码率。
    pub fn report(&mut self, now_ms: u64, rtt_ms: u16, window_ms: u64) -> Option<Feedback> {
        if self.window_start_ms == 0 {
            self.window_start_ms = now_ms;
            return None;
        }
        let elapsed = now_ms.saturating_sub(self.window_start_ms);
        if elapsed < window_ms {
            return None;
        }
        let stats = self.stats();
        let bitrate = if elapsed == 0 { 0 } else { ((self.bytes.saturating_mul(8) * 1000) / elapsed).min(u32::MAX as u64) as u32 };
        let interval = if self.frames > 1 { (elapsed / self.frames).min(u16::MAX as u64) as u16 } else { 0 };
        let fb = Feedback {
            recv_bitrate_bps: bitrate,
            loss_permille: stats.loss_permille,
            jitter_ms: stats.jitter_ms,
            rtt_ms,
            decode_queue: self.queue_len,
            present_interval_ms: interval,
            highest_frame_id: self.highest,
        };
        self.window_start_ms = now_ms;
        self.bytes = 0;
        self.frames = 0;
        self.lost = 0;
        Some(fb)
    }
}

#[derive(Clone, Debug)]
struct Pending {
    payload: Vec<u8>,
    tag: u32,
    sent_ms: u64,
    tries: u8,
}

#[derive(Clone, Debug)]
struct Stream {
    next_seq: u32,
    unacked: HashMap<u32, Pending>,
    recv_ack: u32,
    seen: u64,
    hold: HashMap<u32, (u32, Vec<u8>)>,
    failed: bool,
    ack_dirty: bool,
}

impl Stream {
    fn new() -> Self {
        Self {
            next_seq: 1,
            unacked: HashMap::new(),
            recv_ack: 0,
            seen: 0,
            hold: HashMap::new(),
            failed: false,
            ack_dirty: false,
        }
    }
}

/// 纯确认包要借一个可靠类型的壳，否则接收端不会把它送进可靠层。
/// 每个流借自己的类型，且 rel_seq 为 0，不会触发任何上层副作用。
fn ack_type(stream: u8) -> u32 {
    match stream {
        4 => MsgType::CursorShape as u32,
        6 => MsgType::FileControl as u32,
        7 => MsgType::TerminalData as u32,
        _ => MsgType::Capability as u32,
    }
}

pub struct Reliable {
    streams: [Stream; 8],
    rtt_ms: u32,
    rtt_measured: bool,
}

#[derive(Clone, Debug)]
pub struct ReliableOut {
    pub stream_id: u8,
    pub tag: u32,
    pub rel_seq: u32,
    pub body: Vec<u8>,
    pub payload: Vec<u8>,
}

impl Reliable {
    pub fn new() -> Self {
        Self {
            streams: std::array::from_fn(|_| Stream::new()),
            rtt_ms: 40,
            rtt_measured: false,
        }
    }

    pub fn note_rtt(&mut self, rtt_ms: u32) {
        self.rtt_ms = rtt_ms.clamp(1, 5_000);
        self.rtt_measured = true;
    }

    pub fn rtt_ms(&self) -> u32 {
        self.rtt_ms
    }

    /// 真正量到的往返时间。没收到过回包时是 None，不要拿默认值当实测值往外报。
    pub fn measured_rtt_ms(&self) -> Option<u32> {
        if self.rtt_measured {
            Some(self.rtt_ms)
        } else {
            None
        }
    }

    pub fn failed(&self) -> bool {
        self.streams.iter().any(|s| s.failed)
    }

    /// 这个流还能再塞几帧。批量传文件要看着这个数发，塞满了就得等确认回来。
    pub fn free_window(&self, stream: u8) -> usize {
        if stream > 7 {
            return 0;
        }
        let st = &self.streams[stream as usize];
        if st.failed {
            return 0;
        }
        (CONTROL_WINDOW as usize).saturating_sub(st.unacked.len())
    }

    pub fn enqueue(&mut self, stream: u8, tag: u32, payload: Vec<u8>) -> Result<u32, &'static str> {
        if stream > 7 || payload.len() + REL_HEADER_LEN > MAX_BODY {
            return Err("可靠消息放不进一个数据报");
        }
        let st = &mut self.streams[stream as usize];
        if st.failed {
            return Err("可靠流已失败");
        }
        if st.unacked.len() as u32 >= CONTROL_WINDOW {
            return Err("窗口已满");
        }
        let seq = st.next_seq;
        st.next_seq = st.next_seq.wrapping_add(1);
        if st.next_seq == 0 {
            st.next_seq = 1;
        }
        st.unacked.insert(seq, Pending { payload, tag, sent_ms: 0, tries: 0 });
        Ok(seq)
    }

    pub fn poll(&mut self, now_ms: u64) -> Vec<ReliableOut> {
        let rto = ((self.rtt_ms as u64) * 3 / 2).clamp(20, 500);
        let order = [7u8, 0, 4, 6, 1, 2, 3, 5];
        let mut out = Vec::new();
        for stream in order {
            let st = &mut self.streams[stream as usize];
            if st.failed {
                continue;
            }
            let mut seqs: Vec<u32> = st.unacked.keys().copied().collect();
            seqs.sort_unstable();
            for seq in seqs {
                let due = st.unacked.get(&seq).map(|p| p.sent_ms == 0 || now_ms.saturating_sub(p.sent_ms) >= rto).unwrap_or(false);
                if !due {
                    continue;
                }
                let pending = st.unacked.get_mut(&seq).unwrap();
                if pending.tries >= MAX_RETRANSMIT {
                    st.failed = true;
                    break;
                }
                pending.tries += 1;
                pending.sent_ms = now_ms;
                let payload = pending.payload.clone();
                let tag = pending.tag;
                let header = ReliableHeader {
                    rel_seq: seq,
                    ack: st.recv_ack,
                    ack_bits: st.seen,
                    payload: payload.clone(),
                };
                if let Ok(body) = header.encode() {
                    out.push(ReliableOut { stream_id: stream, tag, rel_seq: seq, body, payload });
                }
            }
            // 自己没有待发数据时也要把确认送出去。否则对端只能盲等重传。
            if !st.failed && st.ack_dirty && st.unacked.is_empty() {
                let header = ReliableHeader {
                    rel_seq: 0,
                    ack: st.recv_ack,
                    ack_bits: st.seen,
                    payload: Vec::new(),
                };
                if let Ok(body) = header.encode() {
                    st.ack_dirty = false;
                    out.push(ReliableOut { stream_id: stream, tag: ack_type(stream), rel_seq: 0, body, payload: Vec::new() });
                }
            }
        }
        let _ = order;
        out
    }

    pub fn on_body(&mut self, stream: u8, tag: u32, body: &[u8]) -> Result<Vec<(u32, Vec<u8>)>, &'static str> {
        if stream > 7 {
            return Err("流号不对");
        }
        let header = ReliableHeader::decode(body).map_err(|_| "可靠头截断")?;
        let st = &mut self.streams[stream as usize];
        st.unacked.retain(|seq, _| !ack_has(header.ack, header.ack_bits, *seq));
        if header.rel_seq == 0 {
            return Ok(Vec::new());
        }
        if header.rel_seq <= st.recv_ack {
            return Ok(Vec::new());
        }
        let ahead = header.rel_seq - st.recv_ack - 1;
        if ahead >= 64 {
            return Err("超出确认窗口");
        }
        if ahead > 0 {
            st.seen |= 1u64 << (ahead - 1);
        }
        st.hold.insert(header.rel_seq, (tag, header.payload));
        let mut delivered = Vec::new();
        loop {
            let next = st.recv_ack + 1;
            let Some((tag, payload)) = st.hold.remove(&next) else {
                break;
            };
            st.recv_ack = next;
            st.seen >>= 1;
            delivered.push((tag, payload));
        }
        if !delivered.is_empty() {
            st.ack_dirty = true;
        }
        Ok(delivered)
    }
}

fn ack_has(ack: u32, bits: u64, seq: u32) -> bool {
    if seq <= ack {
        return true;
    }
    let ahead = seq - ack - 1;
    ahead < 64 && bits & (1u64 << ahead) != 0
}

pub fn fec_params(loss_permille: u16) -> (usize, usize) {
    if loss_permille < 10 {
        (8, 1)
    } else {
        (4, 2)
    }
}

pub fn xor_slices(parts: &[Vec<u8>]) -> (Vec<u16>, Vec<u8>) {
    let lengths: Vec<u16> = parts.iter().map(|p| p.len() as u16).collect();
    let max = lengths.iter().copied().max().unwrap_or(0) as usize;
    let mut acc = vec![0u8; max];
    for part in parts {
        for (i, b) in part.iter().enumerate() {
            acc[i] ^= b;
        }
    }
    (lengths, acc)
}

pub fn recover_one(parts: &[Option<Vec<u8>>], lengths: &[u16], xor: &[u8]) -> Option<Vec<Vec<u8>>> {
    let missing: Vec<usize> = parts.iter().enumerate().filter_map(|(i, p)| p.is_none().then_some(i)).collect();
    if missing.len() != 1 || lengths.len() != parts.len() {
        return None;
    }
    let miss = missing[0];
    let mut acc = xor.to_vec();
    for (i, part) in parts.iter().enumerate() {
        if i == miss {
            continue;
        }
        let part = part.as_ref()?;
        for (b, x) in part.iter().zip(acc.iter_mut()) {
            *x ^= *b;
        }
    }
    let len = *lengths.get(miss)? as usize;
    if len > acc.len() {
        return None;
    }
    acc.truncate(len);
    let mut out = Vec::new();
    for (i, part) in parts.iter().enumerate() {
        if i == miss {
            out.push(acc.clone());
        } else {
            out.push(part.clone()?);
        }
    }
    Some(out)
}

pub fn video_inner(frame_id: u32, slice_index: u16, slice_count: u16, idr: bool, reference: bool, codec: u8, bytes: &[u8], ts: u64) -> Result<Inner, jr_proto::ProtoError> {
    let mut body = Vec::with_capacity(1 + bytes.len());
    body.push(codec);
    body.extend_from_slice(bytes);
    let mut flags = 0u16;
    if idr {
        flags |= jr_proto::FLAG_IDR;
    }
    if reference {
        flags |= jr_proto::FLAG_REF;
    }
    if slice_index + 1 == slice_count {
        flags |= jr_proto::FLAG_SLICE_END;
    }
    Ok(Inner {
        msg_type: MsgType::Video as u8,
        stream_id: 1,
        body_flags: flags,
        frame_id,
        slice_index,
        slice_count,
        timestamp_us: ts,
        body,
    })
}

pub fn fec_inner(group_id: u16, index: u8, parts: &[Vec<u8>], ts: u64) -> Result<Inner, jr_proto::ProtoError> {
    let (lengths, xor) = xor_slices(parts);
    let parity = FecParity { group_id, index, lengths, xor };
    Ok(Inner {
        msg_type: MsgType::Video as u8,
        stream_id: 1,
        body_flags: FLAG_FEC,
        frame_id: 0,
        slice_index: 0,
        slice_count: 1,
        timestamp_us: ts,
        body: parity.encode()?,
    })
}

pub struct Reassembler {
    frames: HashMap<u32, PartialFrame>,
}

struct PartialFrame {
    slices: Vec<Option<Vec<u8>>>,
    flags: u16,
    born_us: u64,
}

impl Reassembler {
    pub fn new() -> Self {
        Self { frames: HashMap::new() }
    }

    pub fn push(&mut self, inner: &Inner, now_us: u64) -> Vec<Reassembled> {
        self.expire(now_us);
        if inner.body_flags & FLAG_FEC != 0 || inner.slice_count == 0 {
            return Vec::new();
        }
        let slot = self.frames.entry(inner.frame_id).or_insert_with(|| PartialFrame {
            slices: vec![None; inner.slice_count as usize],
            flags: inner.body_flags,
            born_us: now_us,
        });
        slot.flags |= inner.body_flags;
        if (inner.slice_index as usize) < slot.slices.len() {
            let bytes = if inner.body.first() == Some(&1) || inner.body.first() == Some(&2) {
                inner.body.get(1..).unwrap_or_default().to_vec()
            } else {
                inner.body.clone()
            };
            slot.slices[inner.slice_index as usize] = Some(bytes);
        }
        if slot.slices.iter().all(|s| s.is_some()) {
            let frame = self.frames.remove(&inner.frame_id).unwrap();
            let mut bytes = Vec::new();
            for part in frame.slices.into_iter().flatten() {
                bytes.extend(part);
            }
            vec![Reassembled::Frame { frame_id: inner.frame_id, bytes, idr: frame.flags & jr_proto::FLAG_IDR != 0 }]
        } else {
            Vec::new()
        }
    }

    fn expire(&mut self, now_us: u64) -> Vec<Reassembled> {
        let mut lost = Vec::new();
        self.frames.retain(|id, frame| {
            if now_us.saturating_sub(frame.born_us) > 200_000 {
                lost.push(Reassembled::Lost { frame_id: *id, reference: frame.flags & jr_proto::FLAG_REF != 0 });
                false
            } else {
                true
            }
        });
        lost
    }

    pub fn expire_lost(&mut self, now_us: u64) -> Vec<Reassembled> {
        self.expire(now_us)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Reassembled {
    Frame { frame_id: u32, bytes: Vec<u8>, idr: bool },
    Lost { frame_id: u32, reference: bool },
}

#[derive(Default)]
pub struct InputGate {
    keys: HashMap<u16, u64>,
    buttons: HashMap<u8, u64>,
    /// 还按着的手指：id -> (最后时间, x, y)。触控 UP 丢了要能补取消。
    touches: HashMap<u8, (u64, i32, i32)>,
}

/// 鼠标键和触点没有协议里的“自动抬起”语义。按住拖动、长按都可能超过
/// 500ms，所以这两类只做兜底超时；会话结束时仍由 `release_all` 立即清干净。
const POINTER_HOLD_TIMEOUT_US: u64 = 30_000_000;

impl InputGate {
    pub fn accept(&mut self, timestamp_us: u64, now_us: u64, event: &jr_proto::InputEvent) -> bool {
        let age = now_us.saturating_sub(timestamp_us);
        let stale = age > jr_proto::INPUT_MAX_AGE_US;
        match event.kind {
            jr_proto::InputEvent::MOVE_ABS | jr_proto::InputEvent::MOVE_REL => {
                if stale {
                    return false;
                }
                // 拖动时鼠标键一直是按下的，移动要续上它的保活时间。
                for at in self.buttons.values_mut() {
                    *at = now_us;
                }
                true
            }
            jr_proto::InputEvent::WHEEL => !stale,
            jr_proto::InputEvent::KEY => {
                let down = event.payload.get(2).copied().unwrap_or(0) != 0;
                if stale && down {
                    return false;
                }
                if event.payload.len() >= 2 {
                    let usage = u16::from_le_bytes([event.payload[0], event.payload[1]]);
                    if down {
                        self.keys.insert(usage, now_us);
                    } else {
                        self.keys.remove(&usage);
                    }
                }
                true
            }
            jr_proto::InputEvent::BUTTON => {
                let down = event.payload.get(1).copied().unwrap_or(0) != 0;
                if stale && down {
                    return false;
                }
                let button = event.payload.first().copied().unwrap_or(0);
                if down {
                    self.buttons.insert(button, now_us);
                } else {
                    self.buttons.remove(&button);
                }
                true
            }
            jr_proto::InputEvent::TOUCH => {
                let Some((id, phase, x, y)) = event.touch_info() else {
                    return false;
                };
                // 抬起和取消永远收，不然一次过期就把手指留在按下状态。
                if stale
                    && phase != jr_proto::InputEvent::TOUCH_UP
                    && phase != jr_proto::InputEvent::TOUCH_CANCEL
                {
                    return false;
                }
                match phase {
                    jr_proto::InputEvent::TOUCH_DOWN | jr_proto::InputEvent::TOUCH_MOVE => {
                        self.touches.insert(id, (now_us, x, y));
                    }
                    jr_proto::InputEvent::TOUCH_UP | jr_proto::InputEvent::TOUCH_CANCEL => {
                        self.touches.remove(&id);
                    }
                    _ => return false,
                }
                true
            }
            _ => !stale,
        }
    }

    pub fn release_stale(&mut self, now_us: u64) -> Vec<jr_proto::InputEvent> {
        let mut out = Vec::new();
        let old_keys: Vec<u16> = self
            .keys
            .iter()
            .filter(|(_, t)| now_us.saturating_sub(**t) > jr_proto::KEY_HOLD_TIMEOUT_US)
            .map(|(k, _)| *k)
            .collect();
        for usage in old_keys {
            self.keys.remove(&usage);
            out.push(jr_proto::InputEvent::key(0, usage, false));
        }
        let old_buttons: Vec<u8> = self
            .buttons
            .iter()
            .filter(|(_, t)| now_us.saturating_sub(**t) > POINTER_HOLD_TIMEOUT_US)
            .map(|(b, _)| *b)
            .collect();
        for button in old_buttons {
            self.buttons.remove(&button);
            out.push(jr_proto::InputEvent {
                event_id: 0,
                kind: jr_proto::InputEvent::BUTTON,
                payload: vec![button, 0],
            });
        }
        let old_touches: Vec<(u8, i32, i32)> = self
            .touches
            .iter()
            .filter(|(_, (t, _, _))| now_us.saturating_sub(*t) > POINTER_HOLD_TIMEOUT_US)
            .map(|(id, (_, x, y))| (*id, *x, *y))
            .collect();
        for (id, x, y) in old_touches {
            self.touches.remove(&id);
            out.push(jr_proto::InputEvent::touch(
                0,
                id,
                jr_proto::InputEvent::TOUCH_CANCEL,
                x,
                y,
            ));
        }
        out
    }

    pub fn release_all(&mut self) -> Vec<jr_proto::InputEvent> {
        let mut out = Vec::new();
        for usage in self.keys.keys().copied().collect::<Vec<_>>() {
            out.push(jr_proto::InputEvent::key(0, usage, false));
        }
        for button in self.buttons.keys().copied().collect::<Vec<_>>() {
            out.push(jr_proto::InputEvent {
                event_id: 0,
                kind: jr_proto::InputEvent::BUTTON,
                payload: vec![button, 0],
            });
        }
        for (id, (_, x, y)) in self.touches.drain() {
            out.push(jr_proto::InputEvent::touch(
                0,
                id,
                jr_proto::InputEvent::TOUCH_CANCEL,
                x,
                y,
            ));
        }
        self.keys.clear();
        self.buttons.clear();
        out
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CursorLatest {
    pub x: i32,
    pub y: i32,
    pub visible: bool,
    pub shape_hash: u64,
}

impl Default for CursorLatest {
    fn default() -> Self {
        Self { x: 0, y: 0, visible: true, shape_hash: 0 }
    }
}

#[allow(dead_code)]
fn _queue_hint(q: VecDeque<u8>) -> usize {
    q.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lossy_low_rtt_loses_to_clean_relay() {
        let direct = path_score(80, 1, 5);
        let relay = path_score(0, 2, 15);
        assert!(relay < direct);
        assert_eq!(choose_path(&[(0, direct), (2, relay)]), Some(2));
    }

    #[test]
    fn arq_retransmit_then_fail() {
        let mut arq = Reliable::new();
        arq.note_rtt(20);
        arq.enqueue(0, 12, b"cap".to_vec()).unwrap();
        let first = arq.poll(0);
        assert_eq!(first.len(), 1);
        let again = arq.poll(30);
        assert_eq!(again.len(), 1);
        for i in 2..9 {
            let _ = arq.poll(i * 30);
        }
        assert!(arq.failed());
    }

    #[test]
    fn arq_delivers_in_order_and_acks() {
        let mut send = Reliable::new();
        let mut recv = Reliable::new();
        send.enqueue(6, 16, b"one".to_vec()).unwrap();
        send.enqueue(6, 16, b"two".to_vec()).unwrap();
        let pkts = send.poll(0);
        let second = recv.on_body(6, 16, &pkts[1].body).unwrap();
        assert!(second.is_empty());
        let first = recv.on_body(6, 16, &pkts[0].body).unwrap();
        assert_eq!(first, vec![(16, b"one".to_vec()), (16, b"two".to_vec())]);
    }

    #[test]
    fn fec_recovers_one_loss() {
        let parts = vec![b"aa".to_vec(), b"bbbb".to_vec(), b"c".to_vec()];
        let (lengths, xor) = xor_slices(&parts);
        let mut got = vec![Some(parts[0].clone()), None, Some(parts[2].clone())];
        let back = recover_one(&got, &lengths, &xor).unwrap();
        assert_eq!(back[1], parts[1]);
        got[1] = Some(parts[1].clone());
        assert!(recover_one(&got, &lengths, &xor).is_none());
    }

    #[test]
    fn stale_move_dropped_and_key_released() {
        let mut gate = InputGate::default();
        let mv = jr_proto::InputEvent::move_abs(1, 1, 1);
        assert!(!gate.accept(0, 250_000, &mv));
        let down = jr_proto::InputEvent::key(2, 0x04, true);
        assert!(gate.accept(0, 0, &down));
        let ups = gate.release_stale(600_000);
        assert_eq!(ups.len(), 1);
        assert_eq!(ups[0].payload[2], 0);
    }

    #[test]
    fn stale_touch_up_is_kept_and_release_cancels() {
        let mut gate = InputGate::default();
        let down = jr_proto::InputEvent::touch(1, 2, jr_proto::InputEvent::TOUCH_DOWN, 10, 20);
        assert!(gate.accept(0, 0, &down));
        // UP 再旧也要收，否则触点会一直留在按下状态。
        let up = jr_proto::InputEvent::touch(2, 2, jr_proto::InputEvent::TOUCH_UP, 10, 20);
        assert!(gate.accept(0, 900_000, &up));
        assert!(gate.release_all().is_empty());

        let down = jr_proto::InputEvent::touch(3, 4, jr_proto::InputEvent::TOUCH_DOWN, 30, 40);
        assert!(gate.accept(0, 0, &down));
        // 长按不是丢 UP，500ms 不能把手指弹起来。
        assert!(gate.release_stale(600_000).is_empty());
        let out = gate.release_stale(POINTER_HOLD_TIMEOUT_US + 1);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].touch_info(), Some((4, jr_proto::InputEvent::TOUCH_CANCEL, 30, 40)));
    }

    #[test]
    fn held_mouse_button_survives_drag_and_is_refreshed_by_move() {
        let mut gate = InputGate::default();
        let down = jr_proto::InputEvent {
            event_id: 0,
            kind: jr_proto::InputEvent::BUTTON,
            payload: vec![0, 1],
        };
        assert!(gate.accept(0, 0, &down));
        assert!(gate.release_stale(600_000).is_empty(), "按住 500ms 不能自动松开");
        for t in (0..20).step_by(4) {
            let mv = jr_proto::InputEvent::move_abs(0, 10, 10);
            assert!(gate.accept(t as u64, t as u64, &mv));
        }
        assert!(
            gate.release_stale(19_000_000).is_empty(),
            "持续拖动时鼠标键要保活"
        );
        let out = gate.release_stale(POINTER_HOLD_TIMEOUT_US + 19_000_000);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].payload, vec![0, 0]);
    }

    #[test]
    fn jcc_drops_on_loss_and_ignores_small_change() {
        let mut jcc = Jcc::new(2);
        let start = jcc.target_bps;
        let changed = jcc.on_feedback(
            &Feedback {
                recv_bitrate_bps: 1,
                loss_permille: 80,
                jitter_ms: 1,
                rtt_ms: 10,
                decode_queue: 0,
                present_interval_ms: 16,
                highest_frame_id: 1,
            },
            1920,
            1080,
            50,
        );
        assert!(jcc.target_bps < start);
        assert!(changed);
    }
}
