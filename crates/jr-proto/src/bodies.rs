use crate::{read_u16, read_u32, read_u64, take, ProtoError, CLIPBOARD_MAX, FILE_CHUNK, MAX_BODY};

pub const REL_HEADER_LEN: usize = 16;
pub const INPUT_MAX_AGE_US: u64 = 200_000;
pub const KEY_HOLD_TIMEOUT_US: u64 = 500_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReliableHeader {
    pub rel_seq: u32,
    pub ack: u32,
    pub ack_bits: u64,
    pub payload: Vec<u8>,
}

impl ReliableHeader {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        if REL_HEADER_LEN + self.payload.len() > MAX_BODY {
            return Err(ProtoError::TooLarge);
        }
        let mut out = Vec::with_capacity(REL_HEADER_LEN + self.payload.len());
        push_u32(&mut out, self.rel_seq);
        push_u32(&mut out, self.ack);
        push_u64(&mut out, self.ack_bits);
        out.extend_from_slice(&self.payload);
        Ok(out)
    }

    pub fn decode(buf: &[u8]) -> Result<Self, ProtoError> {
        if buf.len() < REL_HEADER_LEN || buf.len() > MAX_BODY {
            return Err(if buf.len() > MAX_BODY {
                ProtoError::TooLarge
            } else {
                ProtoError::Truncated
            });
        }
        Ok(Self {
            rel_seq: read_u32(buf, 0)?,
            ack: read_u32(buf, 4)?,
            ack_bits: read_u64(buf, 8)?,
            payload: buf[REL_HEADER_LEN..].to_vec(),
        })
    }
}

/// 一帧 AAC-LC。音频不走可靠流，缺帧由播放端补静音。
/// 头长度固定为 9，后面就是编码器输出的一帧，不再包 Annex-B 之类的外壳。
pub const AUDIO_HEADER_LEN: usize = 9;
pub const AUDIO_CODEC_AAC_LC: u8 = 1;
pub const AUDIO_SAMPLE_RATE: u32 = 48_000;
pub const AUDIO_FRAME_SAMPLES: u32 = 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioFrame {
    pub codec: u8,
    pub sample_rate: u32,
    pub frame_samples: u32,
    pub payload: Vec<u8>,
}

impl AudioFrame {
    pub fn aac_lc(payload: Vec<u8>) -> Self {
        Self {
            codec: AUDIO_CODEC_AAC_LC,
            sample_rate: AUDIO_SAMPLE_RATE,
            frame_samples: AUDIO_FRAME_SAMPLES,
            payload,
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        if AUDIO_HEADER_LEN + self.payload.len() > MAX_BODY {
            return Err(ProtoError::TooLarge);
        }
        let mut out = Vec::with_capacity(AUDIO_HEADER_LEN + self.payload.len());
        out.push(self.codec);
        push_u32(&mut out, self.sample_rate);
        push_u32(&mut out, self.frame_samples);
        out.extend_from_slice(&self.payload);
        Ok(out)
    }

    pub fn decode(buf: &[u8]) -> Result<Self, ProtoError> {
        if buf.len() < AUDIO_HEADER_LEN {
            return Err(ProtoError::Truncated);
        }
        let frame = Self {
            codec: buf[0],
            sample_rate: read_u32(buf, 1)?,
            frame_samples: read_u32(buf, 5)?,
            payload: buf[AUDIO_HEADER_LEN..].to_vec(),
        };
        if frame.codec != AUDIO_CODEC_AAC_LC {
            return Err(ProtoError::Malformed("audio codec"));
        }
        if frame.sample_rate == 0 || frame.frame_samples == 0 {
            return Err(ProtoError::Malformed("audio frame"));
        }
        Ok(frame)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayInfo {
    pub display_id: u32,
    pub origin_x: i32,
    pub origin_y: i32,
    pub width: u32,
    pub height: u32,
    pub scale_milli: u32,
    pub refresh_hz: u16,
    pub is_virtual: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capability {
    pub protocol_version: u8,
    pub platform: u8,
    pub feature_bits: u32,
    pub displays: Vec<DisplayInfo>,
    pub encoder_caps: u32,
    pub audio_codec: u8,
    pub max_touch_points: u8,
}

impl Capability {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        if self.displays.len() > 16 {
            return Err(ProtoError::TooLarge);
        }
        let mut out = Vec::new();
        out.push(self.protocol_version);
        out.push(self.platform);
        push_u32(&mut out, self.feature_bits);
        out.push(self.displays.len() as u8);
        for d in &self.displays {
            push_u32(&mut out, d.display_id);
            push_i32(&mut out, d.origin_x);
            push_i32(&mut out, d.origin_y);
            push_u32(&mut out, d.width);
            push_u32(&mut out, d.height);
            push_u32(&mut out, d.scale_milli);
            push_u16(&mut out, d.refresh_hz);
            out.push(u8::from(d.is_virtual));
        }
        push_u32(&mut out, self.encoder_caps);
        out.push(self.audio_codec);
        out.push(self.max_touch_points);
        if out.len() > MAX_BODY {
            return Err(ProtoError::TooLarge);
        }
        Ok(out)
    }

    pub fn decode(buf: &[u8]) -> Result<Self, ProtoError> {
        if buf.len() < 7 {
            return Err(ProtoError::Truncated);
        }
        let n = buf[6] as usize;
        let mut off = 7;
        let mut displays = Vec::with_capacity(n);
        for _ in 0..n {
            if buf.len() < off + 27 {
                return Err(ProtoError::Truncated);
            }
            displays.push(DisplayInfo {
                display_id: read_u32(buf, off)?,
                origin_x: read_i32(buf, off + 4)?,
                origin_y: read_i32(buf, off + 8)?,
                width: read_u32(buf, off + 12)?,
                height: read_u32(buf, off + 16)?,
                scale_milli: read_u32(buf, off + 20)?,
                refresh_hz: read_u16(buf, off + 24)?,
                is_virtual: buf[off + 26] != 0,
            });
            off += 27;
        }
        if buf.len() != off + 6 {
            return Err(ProtoError::Malformed("capability"));
        }
        Ok(Self {
            protocol_version: buf[0],
            platform: buf[1],
            feature_bits: read_u32(buf, 2)?,
            displays,
            encoder_caps: read_u32(buf, off)?,
            audio_codec: buf[off + 4],
            max_touch_points: buf[off + 5],
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionControl {
    pub action: u8,
    pub display_id: u32,
    pub profile: u8,
    pub flags: u32,
}

impl SessionControl {
    pub const START: u8 = 1;
    pub const SWITCH_DISPLAY: u8 = 2;
    pub const SWITCH_PROFILE: u8 = 3;
    pub const PRIVACY_ON: u8 = 4;
    pub const PRIVACY_OFF: u8 = 5;
    pub const PAUSE_INPUT: u8 = 6;
    pub const RESUME_INPUT: u8 = 7;
    pub const END: u8 = 8;
    pub const FLAG_REJECTED: u32 = 1;
    /// 隐私屏开了，但这台机器只能做软件遮挡，不是硬件级。
    /// 被控端在回执里带上这个位，主控必须把话说全，不能当成完整隐私屏。
    pub const FLAG_DEGRADED: u32 = 2;

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(10);
        out.push(self.action);
        push_u32(&mut out, self.display_id);
        out.push(self.profile);
        push_u32(&mut out, self.flags);
        out
    }

    pub fn decode(buf: &[u8]) -> Result<Self, ProtoError> {
        if buf.len() != 10 {
            return Err(ProtoError::Truncated);
        }
        Ok(Self {
            action: buf[0],
            display_id: read_u32(buf, 1)?,
            profile: buf[5],
            flags: read_u32(buf, 6)?,
        })
    }
}

/// 远端光标形状。像素是未压缩的 BGRA8，按行从上到下排列。
/// 只传形状变化，不跟着每一帧视频走。
pub const CURSOR_SHAPE_MAX: u16 = 128;
pub const CURSOR_SHAPE_HEADER: usize = 16;

/// 内容散列，用来让主控跳过没变的形状。算法只要求在两端一致。
pub fn cursor_shape_hash(hotspot_x: u16, hotspot_y: u16, width: u16, height: u16, bgra: &[u8]) -> u64 {
    let mut h = 1469598103934665603u64;
    for byte in hotspot_x
        .to_le_bytes()
        .iter()
        .chain(hotspot_y.to_le_bytes().iter())
        .chain(width.to_le_bytes().iter())
        .chain(height.to_le_bytes().iter())
        .chain(bgra.iter())
    {
        h ^= u64::from(*byte);
        h = h.wrapping_mul(1099511628211);
    }
    h
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorShape {
    pub hotspot_x: u16,
    pub hotspot_y: u16,
    pub width: u16,
    pub height: u16,
    pub hash: u64,
    pub bgra: Vec<u8>,
}

impl CursorShape {
    pub fn new(hotspot_x: u16, hotspot_y: u16, width: u16, height: u16, bgra: Vec<u8>) -> Self {
        let hash = cursor_shape_hash(hotspot_x, hotspot_y, width, height, &bgra);
        Self { hotspot_x, hotspot_y, width, height, hash, bgra }
    }

    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        if self.width == 0
            || self.height == 0
            || self.width > CURSOR_SHAPE_MAX
            || self.height > CURSOR_SHAPE_MAX
        {
            return Err(ProtoError::TooLarge);
        }
        let need = self.width as usize * self.height as usize * 4;
        if self.bgra.len() != need {
            return Err(ProtoError::Malformed("cursor pixels"));
        }
        let hash = cursor_shape_hash(self.hotspot_x, self.hotspot_y, self.width, self.height, &self.bgra);
        let mut out = Vec::with_capacity(CURSOR_SHAPE_HEADER + need);
        out.extend_from_slice(&self.hotspot_x.to_le_bytes());
        out.extend_from_slice(&self.hotspot_y.to_le_bytes());
        out.extend_from_slice(&self.width.to_le_bytes());
        out.extend_from_slice(&self.height.to_le_bytes());
        out.extend_from_slice(&hash.to_le_bytes());
        out.extend_from_slice(&self.bgra);
        Ok(out)
    }

    pub fn decode(buf: &[u8]) -> Result<Self, ProtoError> {
        if buf.len() < CURSOR_SHAPE_HEADER {
            return Err(ProtoError::Truncated);
        }
        let hotspot_x = read_u16(buf, 0)?;
        let hotspot_y = read_u16(buf, 2)?;
        let width = read_u16(buf, 4)?;
        let height = read_u16(buf, 6)?;
        if width == 0 || height == 0 || width > CURSOR_SHAPE_MAX || height > CURSOR_SHAPE_MAX {
            return Err(ProtoError::Malformed("cursor size"));
        }
        let need = width as usize * height as usize * 4;
        if buf.len() != CURSOR_SHAPE_HEADER + need {
            return Err(ProtoError::Malformed("cursor pixels"));
        }
        let bgra = buf[CURSOR_SHAPE_HEADER..].to_vec();
        let hash = cursor_shape_hash(hotspot_x, hotspot_y, width, height, &bgra);
        Ok(Self { hotspot_x, hotspot_y, width, height, hash, bgra })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputEvent {
    pub event_id: u32,
    pub kind: u8,
    pub payload: Vec<u8>,
}

impl InputEvent {
    pub const MOVE_ABS: u8 = 1;
    pub const MOVE_REL: u8 = 2;
    pub const KEY: u8 = 3;
    pub const BUTTON: u8 = 4;
    pub const WHEEL: u8 = 5;
    pub const TOUCH: u8 = 6;
    /// 触控阶段。载荷是 id(u32) + 阶段(u8) + x(i32) + y(i32)。
    pub const TOUCH_DOWN: u8 = 0;
    pub const TOUCH_MOVE: u8 = 1;
    pub const TOUCH_UP: u8 = 2;
    pub const TOUCH_CANCEL: u8 = 3;

    pub fn touch(event_id: u32, id: u8, phase: u8, x: i32, y: i32) -> Self {
        let mut payload = Vec::with_capacity(10);
        payload.push(id);
        payload.push(phase);
        push_i32(&mut payload, x);
        push_i32(&mut payload, y);
        Self { event_id, kind: Self::TOUCH, payload }
    }

    /// 解析触控载荷。长度不对或阶段未知就返回 None，不要猜。
    pub fn touch_info(&self) -> Option<(u8, u8, i32, i32)> {
        if self.kind != Self::TOUCH || self.payload.len() != 10 {
            return None;
        }
        let id = self.payload[0];
        let phase = self.payload[1];
        if phase > Self::TOUCH_CANCEL {
            return None;
        }
        let x = i32::from_le_bytes(self.payload[2..6].try_into().ok()?);
        let y = i32::from_le_bytes(self.payload[6..10].try_into().ok()?);
        Some((id, phase, x, y))
    }

    pub fn move_abs(event_id: u32, x: i32, y: i32) -> Self {
        let mut payload = Vec::with_capacity(8);
        push_i32(&mut payload, x);
        push_i32(&mut payload, y);
        Self { event_id, kind: Self::MOVE_ABS, payload }
    }

    pub fn wheel(event_id: u32, dx: i32, dy: i32) -> Self {
        let mut payload = Vec::with_capacity(8);
        push_i32(&mut payload, dx);
        push_i32(&mut payload, dy);
        Self { event_id, kind: Self::WHEEL, payload }
    }

    pub fn key(event_id: u32, usage: u16, down: bool) -> Self {
        Self {
            event_id,
            kind: Self::KEY,
            payload: {
                let mut p = Vec::with_capacity(3);
                push_u16(&mut p, usage);
                p.push(u8::from(down));
                p
            },
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        if 5 + self.payload.len() > MAX_BODY {
            return Err(ProtoError::TooLarge);
        }
        let mut out = Vec::with_capacity(5 + self.payload.len());
        push_u32(&mut out, self.event_id);
        out.push(self.kind);
        out.extend_from_slice(&self.payload);
        Ok(out)
    }

    pub fn decode(buf: &[u8]) -> Result<Self, ProtoError> {
        if buf.len() < 5 {
            return Err(ProtoError::Truncated);
        }
        Ok(Self {
            event_id: read_u32(buf, 0)?,
            kind: buf[4],
            payload: buf[5..].to_vec(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Feedback {
    pub recv_bitrate_bps: u32,
    pub loss_permille: u16,
    pub jitter_ms: u16,
    pub rtt_ms: u16,
    pub decode_queue: u8,
    pub present_interval_ms: u16,
    pub highest_frame_id: u32,
}

impl Feedback {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(17);
        push_u32(&mut out, self.recv_bitrate_bps);
        push_u16(&mut out, self.loss_permille);
        push_u16(&mut out, self.jitter_ms);
        push_u16(&mut out, self.rtt_ms);
        out.push(self.decode_queue);
        push_u16(&mut out, self.present_interval_ms);
        push_u32(&mut out, self.highest_frame_id);
        out
    }

    pub fn decode(buf: &[u8]) -> Result<Self, ProtoError> {
        if buf.len() != 17 {
            return Err(ProtoError::Truncated);
        }
        Ok(Self {
            recv_bitrate_bps: read_u32(buf, 0)?,
            loss_permille: read_u16(buf, 4)?,
            jitter_ms: read_u16(buf, 6)?,
            rtt_ms: read_u16(buf, 8)?,
            decode_queue: buf[10],
            present_interval_ms: read_u16(buf, 11)?,
            highest_frame_id: read_u32(buf, 13)?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardPart {
    pub direction: u8,
    pub mime: u8,
    pub total_len: u32,
    pub bytes: Vec<u8>,
}

impl ClipboardPart {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        if self.total_len as usize > CLIPBOARD_MAX || self.bytes.len() > CLIPBOARD_MAX {
            return Err(ProtoError::TooLarge);
        }
        let mut out = Vec::with_capacity(6 + self.bytes.len());
        out.push(self.direction);
        out.push(self.mime);
        push_u32(&mut out, self.total_len);
        out.extend_from_slice(&self.bytes);
        if out.len() > MAX_BODY - REL_HEADER_LEN {
            return Err(ProtoError::TooLarge);
        }
        Ok(out)
    }

    pub fn decode(buf: &[u8]) -> Result<Self, ProtoError> {
        if buf.len() < 6 {
            return Err(ProtoError::Truncated);
        }
        let total_len = read_u32(buf, 2)?;
        if total_len as usize > CLIPBOARD_MAX {
            return Err(ProtoError::TooLarge);
        }
        Ok(Self {
            direction: buf[0],
            mime: buf[1],
            total_len,
            bytes: buf[6..].to_vec(),
        })
    }
}

pub fn split_clipboard(direction: u8, text: &str) -> Result<Vec<ClipboardPart>, ProtoError> {
    if text.len() > CLIPBOARD_MAX {
        return Err(ProtoError::TooLarge);
    }
    let budget = MAX_BODY - REL_HEADER_LEN - 6;
    if text.is_empty() {
        return Ok(vec![ClipboardPart {
            direction,
            mime: 1,
            total_len: 0,
            bytes: Vec::new(),
        }]);
    }
    let raw = text.as_bytes();
    let mut out = Vec::new();
    for chunk in raw.chunks(budget) {
        out.push(ClipboardPart {
            direction,
            mime: 1,
            total_len: raw.len() as u32,
            bytes: chunk.to_vec(),
        });
    }
    Ok(out)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileAction {
    Offer {
        transfer_id: u32,
        direction: u8,
        name: String,
        size: u64,
        sha256: [u8; 32],
    },
    Accept { transfer_id: u32, final_name: String },
    Reject { transfer_id: u32 },
    Cancel { transfer_id: u32 },
    Complete { transfer_id: u32 },
    Error { transfer_id: u32 },
}

impl FileAction {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        let mut out = Vec::new();
        match self {
            Self::Offer { transfer_id, direction, name, size, sha256 } => {
                if name.len() > 255 {
                    return Err(ProtoError::TooLarge);
                }
                out.push(1);
                push_u32(&mut out, *transfer_id);
                out.push(*direction);
                push_u16(&mut out, name.len() as u16);
                out.extend_from_slice(name.as_bytes());
                push_u64(&mut out, *size);
                out.extend_from_slice(sha256);
            }
            Self::Accept { transfer_id, final_name } => {
                if final_name.len() > 255 {
                    return Err(ProtoError::TooLarge);
                }
                out.push(2);
                push_u32(&mut out, *transfer_id);
                push_u16(&mut out, final_name.len() as u16);
                out.extend_from_slice(final_name.as_bytes());
            }
            Self::Reject { transfer_id } => {
                out.push(3);
                push_u32(&mut out, *transfer_id);
            }
            Self::Cancel { transfer_id } => {
                out.push(4);
                push_u32(&mut out, *transfer_id);
            }
            Self::Complete { transfer_id } => {
                out.push(5);
                push_u32(&mut out, *transfer_id);
            }
            Self::Error { transfer_id } => {
                out.push(6);
                push_u32(&mut out, *transfer_id);
            }
        }
        Ok(out)
    }

    pub fn decode(buf: &[u8]) -> Result<Self, ProtoError> {
        if buf.is_empty() {
            return Err(ProtoError::Truncated);
        }
        match buf[0] {
            1 => {
                if buf.len() < 8 {
                    return Err(ProtoError::Truncated);
                }
                let transfer_id = read_u32(buf, 1)?;
                let direction = buf[5];
                let n = read_u16(buf, 6)? as usize;
                if n > 255 || buf.len() != 8 + n + 8 + 32 {
                    return Err(ProtoError::Malformed("file offer"));
                }
                let name = std::str::from_utf8(&buf[8..8 + n])
                    .map_err(|_| ProtoError::Malformed("file name"))?
                    .to_string();
                let size = read_u64(buf, 8 + n)?;
                let mut sha256 = [0u8; 32];
                sha256.copy_from_slice(take(buf, 16 + n, 32)?);
                Ok(Self::Offer { transfer_id, direction, name, size, sha256 })
            }
            2 => {
                if buf.len() < 7 {
                    return Err(ProtoError::Truncated);
                }
                let transfer_id = read_u32(buf, 1)?;
                let n = read_u16(buf, 5)? as usize;
                if buf.len() != 7 + n || n > 255 {
                    return Err(ProtoError::Malformed("file accept"));
                }
                let final_name = std::str::from_utf8(&buf[7..7 + n])
                    .map_err(|_| ProtoError::Malformed("file name"))?
                    .to_string();
                Ok(Self::Accept { transfer_id, final_name })
            }
            3..=6 => {
                if buf.len() != 5 {
                    return Err(ProtoError::Truncated);
                }
                let transfer_id = read_u32(buf, 1)?;
                Ok(match buf[0] {
                    3 => Self::Reject { transfer_id },
                    4 => Self::Cancel { transfer_id },
                    5 => Self::Complete { transfer_id },
                    _ => Self::Error { transfer_id },
                })
            }
            _ => Err(ProtoError::Malformed("file action")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChunk {
    pub transfer_id: u32,
    pub offset: u64,
    pub bytes: Vec<u8>,
}

impl FileChunk {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        if self.bytes.len() > FILE_CHUNK {
            return Err(ProtoError::TooLarge);
        }
        let mut out = Vec::with_capacity(12 + self.bytes.len());
        push_u32(&mut out, self.transfer_id);
        push_u64(&mut out, self.offset);
        out.extend_from_slice(&self.bytes);
        if REL_HEADER_LEN + out.len() > MAX_BODY {
            return Err(ProtoError::TooLarge);
        }
        Ok(out)
    }

    pub fn decode(buf: &[u8]) -> Result<Self, ProtoError> {
        if buf.len() < 12 {
            return Err(ProtoError::Truncated);
        }
        Ok(Self {
            transfer_id: read_u32(buf, 0)?,
            offset: read_u64(buf, 4)?,
            bytes: buf[12..].to_vec(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FecParity {
    pub group_id: u16,
    pub index: u8,
    pub lengths: Vec<u16>,
    pub xor: Vec<u8>,
}

impl FecParity {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        if self.lengths.len() > 8 {
            return Err(ProtoError::TooLarge);
        }
        let mut out = Vec::new();
        push_u16(&mut out, self.group_id);
        out.push(self.index);
        out.push(self.lengths.len() as u8);
        for n in &self.lengths {
            push_u16(&mut out, *n);
        }
        out.extend_from_slice(&self.xor);
        if out.len() > MAX_BODY {
            return Err(ProtoError::TooLarge);
        }
        Ok(out)
    }

    pub fn decode(buf: &[u8]) -> Result<Self, ProtoError> {
        if buf.len() < 4 {
            return Err(ProtoError::Truncated);
        }
        let n = buf[3] as usize;
        if buf.len() < 4 + n * 2 {
            return Err(ProtoError::Truncated);
        }
        let mut lengths = Vec::with_capacity(n);
        for i in 0..n {
            lengths.push(read_u16(buf, 4 + i * 2)?);
        }
        Ok(Self {
            group_id: read_u16(buf, 0)?,
            index: buf[2],
            lengths,
            xor: buf[4 + n * 2..].to_vec(),
        })
    }
}

pub fn magic_packet(mac: [u8; 6]) -> [u8; 102] {
    let mut pkt = [0u8; 102];
    pkt[..6].fill(0xff);
    for i in 0..16 {
        pkt[6 + i * 6..12 + i * 6].copy_from_slice(&mac);
    }
    pkt
}

fn push_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn push_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn push_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn push_i32(out: &mut Vec<u8>, v: i32) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn read_i32(buf: &[u8], off: usize) -> Result<i32, ProtoError> {
    Ok(read_u32(buf, off)? as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn touch_payload_roundtrips_and_rejects_bad_phases() {
        let ev = InputEvent::touch(7, 3, InputEvent::TOUCH_MOVE, -4, 900);
        assert_eq!(ev.touch_info(), Some((3, InputEvent::TOUCH_MOVE, -4, 900)));
        let mut bad = ev.clone();
        bad.payload[1] = 9;
        assert!(bad.touch_info().is_none());
        let mut short = ev;
        short.payload.pop();
        assert!(short.touch_info().is_none());
    }

    #[test]
    fn cursor_shape_roundtrips_and_rejects_bad_sizes() {
        let shape = CursorShape::new(
            1,
            1,
            2,
            2,
            vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16],
        );
        assert_eq!(CursorShape::decode(&shape.encode().unwrap()).unwrap(), shape);
        let short = CursorShape::new(0, 0, 2, 2, vec![0; 8]);
        assert!(short.encode().is_err());
        let too_big = CursorShape::new(0, 0, CURSOR_SHAPE_MAX + 1, 1, vec![0; 8]);
        assert!(too_big.encode().is_err());
        assert!(CursorShape::decode(&[0; CURSOR_SHAPE_HEADER]).is_err());
    }

    #[test]
    fn capability_and_clipboard_limits() {
        let cap = Capability {
            protocol_version: 1,
            platform: 2,
            feature_bits: 0b111,
            displays: vec![DisplayInfo {
                display_id: 1,
                origin_x: -10,
                origin_y: 20,
                width: 1920,
                height: 1080,
                scale_milli: 2000,
                refresh_hz: 60,
                is_virtual: false,
            }],
            encoder_caps: 1,
            audio_codec: 1,
            max_touch_points: 0,
        };
        assert_eq!(Capability::decode(&cap.encode().unwrap()).unwrap(), cap);
        let big = "x".repeat(CLIPBOARD_MAX + 1);
        assert!(split_clipboard(1, &big).is_err());
        let ok = split_clipboard(1, &"y".repeat(100_000)).unwrap();
        assert!(ok.len() > 1);
        assert_eq!(ok[0].total_len, 100_000);
    }

    #[test]
    fn audio_frame_roundtrips_and_rejects_unknown_codecs() {
        let frame = AudioFrame::aac_lc(vec![0x21, 0x10, 0x05]);
        let wire = frame.encode().unwrap();
        assert_eq!(AudioFrame::decode(&wire).unwrap(), frame);
        assert_eq!(wire.len(), AUDIO_HEADER_LEN + 3);

        let mut bad = wire.clone();
        bad[0] = 9;
        assert!(AudioFrame::decode(&bad).is_err());
        assert!(AudioFrame::decode(&wire[..AUDIO_HEADER_LEN - 1]).is_err());

        let too_big = AudioFrame::aac_lc(vec![0; MAX_BODY]);
        assert!(too_big.encode().is_err());
    }

    #[test]
    fn file_offer_roundtrip_and_chunk_cap() {
        let offer = FileAction::Offer {
            transfer_id: 7,
            direction: 1,
            name: "a.txt".into(),
            size: 3,
            sha256: [9; 32],
        };
        assert_eq!(FileAction::decode(&offer.encode().unwrap()).unwrap(), offer);
        let chunk = FileChunk { transfer_id: 7, offset: 0, bytes: vec![1, 2, 3] };
        assert_eq!(FileChunk::decode(&chunk.encode().unwrap()).unwrap(), chunk);
        assert!(FileChunk { transfer_id: 1, offset: 0, bytes: vec![0; FILE_CHUNK + 1] }
            .encode()
            .is_err());
    }
}
