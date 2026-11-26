//! JRP/1 编解码。没有套接字，没有系统调用。
#![forbid(unsafe_code)]

mod beacon;
mod bodies;
mod error;
mod inner;
mod outer;
mod reason;
mod replay;
mod signal;

pub use beacon::{decode_beacon, encode_beacon, Beacon};
pub use bodies::{
    magic_packet, split_clipboard, AudioFrame, Capability, ClipboardPart, CursorShape, DisplayInfo, FecParity, Feedback,
    FileAction, FileChunk, InputEvent, ReliableHeader, SessionControl, INPUT_MAX_AGE_US,
    AUDIO_CODEC_AAC_LC, AUDIO_FRAME_SAMPLES, AUDIO_HEADER_LEN, AUDIO_SAMPLE_RATE,
    CURSOR_SHAPE_MAX, CURSOR_SHAPE_HEADER,
    KEY_HOLD_TIMEOUT_US, REL_HEADER_LEN,
};
pub use error::ProtoError;
pub use inner::{expected_stream, stream_matches, Inner, MsgType, FLAG_ACK_REQUEST, FLAG_FEC, FLAG_IDR, FLAG_REF, FLAG_SLICE_END};
pub use outer::{decode_outer, encode_outer, OuterFlags, OuterHeader, OUTER_HEADER_LEN};
pub use reason::Reason;
pub use replay::{ReplayError, ReplayWindow};
pub use signal::{
    decode_frame, encode_frame, Candidates, Decision, DeviceSnap, Diagnostic, Enroll, EnrollOk, Ephemeral,
    Heartbeat, Invite, Presence, PresenceKind, RelayTicket, SignalFrame, SignalType, SockAddr, Wake,
    MAX_FRAME_BODY,
};

pub const JRP_VERSION: u8 = 1;
pub const JRP_MAGIC: u16 = 0x4A52;
pub const SIGNAL_PORT: u16 = 47210;
pub const RELAY_PORT: u16 = 47211;
pub const BEACON_PORT: u16 = 47212;
/// 信令上的反射发现端口。节点往这儿发一颗 UDP 探针，换回自己在公网上的地址。
pub const REFLEX_PORT: u16 = 47213;
pub const AEAD_KEY_LEN: usize = 32;
pub const AEAD_TAG_LEN: usize = 16;
pub const AEAD_NONCE_LEN: usize = 12;
pub const MAX_DATAGRAM: usize = 1280;
pub const MAX_CIPHERTEXT: usize = 1200;
pub const MAX_INNER_PLAIN: usize = 1184;
pub const INNER_HEADER_LEN: usize = 22;
pub const MAX_BODY: usize = 1162;
pub const CONTROL_WINDOW: usize = 64;
pub const FILE_CHUNK: usize = 1024;
pub const CLIPBOARD_MAX: usize = 1_048_576;
pub const GRANT_MAX_AGE_SEC: u64 = 31_536_000;

pub const PLATFORM_WINDOWS: u8 = 1;
pub const PLATFORM_MACOS: u8 = 2;
pub const PLATFORM_LINUX: u8 = 3;
pub const PLATFORM_ANDROID: u8 = 4;
pub const PLATFORM_IOS: u8 = 5;

pub const FEAT_VIDEO: u32 = 1 << 0;
pub const FEAT_AUDIO_OUT: u32 = 1 << 1;
pub const FEAT_AUDIO_IN: u32 = 1 << 2;
pub const FEAT_INPUT: u32 = 1 << 3;
pub const FEAT_CLIPBOARD: u32 = 1 << 4;
pub const FEAT_FILE: u32 = 1 << 5;
pub const FEAT_TERMINAL: u32 = 1 << 6;
pub const FEAT_PRIVACY: u32 = 1 << 7;
pub const FEAT_WAKE: u32 = 1 << 8;
pub const FEAT_VIRTUAL_DISPLAY: u32 = 1 << 9;
pub const FEAT_WATCH_ONLY: u32 = 1 << 10;

pub fn nonce(packet_seq: u32, from_controller: bool) -> [u8; AEAD_NONCE_LEN] {
    let mut n = [0u8; AEAD_NONCE_LEN];
    n[..4].copy_from_slice(&packet_seq.to_le_bytes());
    n[4] = if from_controller { 1 } else { 0 };
    n
}

pub fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xf) as usize] as char);
    }
    out
}

pub fn hex_decode(s: &str) -> Result<Vec<u8>, ProtoError> {
    let s = s.trim();
    if s.len() % 2 != 0 {
        return Err(ProtoError::Malformed("hex 长度"));
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let bytes = s.as_bytes();
    for i in (0..bytes.len()).step_by(2) {
        let hi = hex_val(bytes[i])?;
        let lo = hex_val(bytes[i + 1])?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

fn hex_val(b: u8) -> Result<u8, ProtoError> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(ProtoError::Malformed("hex 字符")),
    }
}

pub(crate) fn read_u16(buf: &[u8], off: usize) -> Result<u16, ProtoError> {
    let s = buf.get(off..off + 2).ok_or(ProtoError::Truncated)?;
    Ok(u16::from_le_bytes([s[0], s[1]]))
}

pub(crate) fn read_u32(buf: &[u8], off: usize) -> Result<u32, ProtoError> {
    let s = buf.get(off..off + 4).ok_or(ProtoError::Truncated)?;
    Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

pub(crate) fn read_u64(buf: &[u8], off: usize) -> Result<u64, ProtoError> {
    let s = buf.get(off..off + 8).ok_or(ProtoError::Truncated)?;
    Ok(u64::from_le_bytes(s.try_into().unwrap()))
}

pub(crate) fn take(buf: &[u8], off: usize, n: usize) -> Result<&[u8], ProtoError> {
    buf.get(off..off + n).ok_or(ProtoError::Truncated)
}

pub(crate) fn read_lp_str(buf: &[u8], off: usize, max: usize) -> Result<(String, usize), ProtoError> {
    let len = read_u16(buf, off)? as usize;
    if len > max {
        return Err(ProtoError::TooLarge);
    }
    let bytes = take(buf, off + 2, len)?;
    let s = std::str::from_utf8(bytes).map_err(|_| ProtoError::Malformed("utf-8"))?;
    Ok((s.to_string(), off + 2 + len))
}

pub(crate) fn write_lp_str(out: &mut Vec<u8>, s: &str, max: usize) -> Result<(), ProtoError> {
    let b = s.as_bytes();
    if b.len() > max {
        return Err(ProtoError::TooLarge);
    }
    out.extend_from_slice(&(b.len() as u16).to_le_bytes());
    out.extend_from_slice(b);
    Ok(())
}
