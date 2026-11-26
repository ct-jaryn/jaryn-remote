use crate::{read_u16, read_u32, read_u64, ProtoError, INNER_HEADER_LEN, MAX_BODY, MAX_INNER_PLAIN};

pub const FLAG_IDR: u16 = 1 << 0;
pub const FLAG_REF: u16 = 1 << 1;
pub const FLAG_SLICE_END: u16 = 1 << 2;
pub const FLAG_FEC: u16 = 1 << 3;
pub const FLAG_ACK_REQUEST: u16 = 1 << 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MsgType {
    Hello = 1,
    HelloAck = 2,
    Ping = 3,
    Pong = 4,
    Video = 5,
    Audio = 6,
    Input = 7,
    CursorPos = 8,
    CursorShape = 9,
    Feedback = 10,
    KeyframeRequest = 11,
    Capability = 12,
    SessionControl = 13,
    Clipboard = 14,
    FileControl = 15,
    FileChunk = 16,
    TerminalOpen = 17,
    TerminalData = 18,
    TerminalResize = 19,
    Ack = 20,
    Bye = 21,
}

impl MsgType {
    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            1 => Self::Hello,
            2 => Self::HelloAck,
            3 => Self::Ping,
            4 => Self::Pong,
            5 => Self::Video,
            6 => Self::Audio,
            7 => Self::Input,
            8 => Self::CursorPos,
            9 => Self::CursorShape,
            10 => Self::Feedback,
            11 => Self::KeyframeRequest,
            12 => Self::Capability,
            13 => Self::SessionControl,
            14 => Self::Clipboard,
            15 => Self::FileControl,
            16 => Self::FileChunk,
            17 => Self::TerminalOpen,
            18 => Self::TerminalData,
            19 => Self::TerminalResize,
            20 => Self::Ack,
            21 => Self::Bye,
            _ => return None,
        })
    }
}

/// 已知类型必须走的流。未知类型返回 None，调用方跳过该条。
pub fn expected_stream(ty: MsgType) -> u8 {
    match ty {
        MsgType::Hello
        | MsgType::HelloAck
        | MsgType::Ping
        | MsgType::Pong
        | MsgType::KeyframeRequest
        | MsgType::Capability
        | MsgType::SessionControl
        | MsgType::Clipboard
        | MsgType::Bye => 0,
        MsgType::Video => 1,
        MsgType::Audio => 2,
        MsgType::Input => 3,
        MsgType::CursorPos | MsgType::CursorShape => 4,
        MsgType::Feedback => 5,
        MsgType::FileControl | MsgType::FileChunk => 6,
        MsgType::TerminalOpen | MsgType::TerminalData | MsgType::TerminalResize => 7,
        MsgType::Ack => 0,
    }
}

pub fn stream_matches(ty: MsgType, stream_id: u8) -> bool {
    if ty == MsgType::Ack {
        return true;
    }
    expected_stream(ty) == stream_id
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inner {
    pub msg_type: u8,
    pub stream_id: u8,
    pub body_flags: u16,
    pub frame_id: u32,
    pub slice_index: u16,
    pub slice_count: u16,
    pub timestamp_us: u64,
    pub body: Vec<u8>,
}

impl Inner {
    pub fn known_type(&self) -> Option<MsgType> {
        MsgType::from_u8(self.msg_type)
    }

    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        if self.body.len() > MAX_BODY {
            return Err(ProtoError::TooLarge);
        }
        if self.slice_count == 0 {
            return Err(ProtoError::Malformed("slice_count"));
        }
        let total = INNER_HEADER_LEN + self.body.len();
        if total > MAX_INNER_PLAIN {
            return Err(ProtoError::TooLarge);
        }
        let mut out = Vec::with_capacity(total);
        out.push(self.msg_type);
        out.push(self.stream_id);
        out.extend_from_slice(&self.body_flags.to_le_bytes());
        out.extend_from_slice(&self.frame_id.to_le_bytes());
        out.extend_from_slice(&self.slice_index.to_le_bytes());
        out.extend_from_slice(&self.slice_count.to_le_bytes());
        out.extend_from_slice(&self.timestamp_us.to_le_bytes());
        out.extend_from_slice(&(self.body.len() as u16).to_le_bytes());
        out.extend_from_slice(&self.body);
        Ok(out)
    }

    pub fn decode(buf: &[u8]) -> Result<Self, ProtoError> {
        if buf.len() < INNER_HEADER_LEN {
            return Err(ProtoError::Truncated);
        }
        if buf.len() > MAX_INNER_PLAIN {
            return Err(ProtoError::TooLarge);
        }
        let body_len = read_u16(buf, 20)? as usize;
        if body_len > MAX_BODY {
            return Err(ProtoError::TooLarge);
        }
        if buf.len() < INNER_HEADER_LEN + body_len {
            return Err(ProtoError::Truncated);
        }
        if buf.len() != INNER_HEADER_LEN + body_len {
            return Err(ProtoError::Malformed("内层长度"));
        }
        let slice_count = read_u16(buf, 10)?;
        if slice_count == 0 {
            return Err(ProtoError::Malformed("slice_count"));
        }
        Ok(Self {
            msg_type: buf[0],
            stream_id: buf[1],
            body_flags: read_u16(buf, 2)?,
            frame_id: read_u32(buf, 4)?,
            slice_index: read_u16(buf, 8)?,
            slice_count,
            timestamp_us: read_u64(buf, 12)?,
            body: buf[INNER_HEADER_LEN..].to_vec(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_unknown() {
        let msg = Inner {
            msg_type: 99,
            stream_id: 0,
            body_flags: 0,
            frame_id: 0,
            slice_index: 0,
            slice_count: 1,
            timestamp_us: 42,
            body: b"skip-me".to_vec(),
        };
        let bytes = msg.encode().unwrap();
        let back = Inner::decode(&bytes).unwrap();
        assert_eq!(back, msg);
        assert!(back.known_type().is_none());
    }

    #[test]
    fn truncated_inner() {
        let msg = Inner {
            msg_type: 1,
            stream_id: 0,
            body_flags: 0,
            frame_id: 1,
            slice_index: 0,
            slice_count: 1,
            timestamp_us: 0,
            body: vec![9, 9],
        };
        let bytes = msg.encode().unwrap();
        assert_eq!(Inner::decode(&bytes[..8]), Err(ProtoError::Truncated));
        assert_eq!(
            Inner::decode(&bytes[..bytes.len() - 1]),
            Err(ProtoError::Truncated)
        );
    }

    #[test]
    fn stream_check() {
        assert!(stream_matches(MsgType::Video, 1));
        assert!(!stream_matches(MsgType::Video, 0));
        assert!(stream_matches(MsgType::Ack, 6));
    }
}
