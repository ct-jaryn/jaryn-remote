use crate::{read_u16, read_u32, take, ProtoError, JRP_MAGIC, JRP_VERSION, MAX_CIPHERTEXT, MAX_DATAGRAM};

pub const OUTER_HEADER_LEN: usize = 28;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OuterFlags {
    pub from_controller: bool,
    pub relay_only: bool,
    pub handshake: bool,
}

impl OuterFlags {
    pub fn pack(self) -> u8 {
        let mut f = 0u8;
        if self.from_controller {
            f |= 1 << 0;
        }
        if self.relay_only {
            f |= 1 << 1;
        }
        if self.handshake {
            f |= 1 << 2;
        }
        f
    }

    pub fn unpack(v: u8) -> Self {
        Self {
            from_controller: v & (1 << 0) != 0,
            relay_only: v & (1 << 1) != 0,
            handshake: v & (1 << 2) != 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OuterHeader {
    pub flags: OuterFlags,
    pub routing_token: [u8; 16],
    pub packet_seq: u32,
    /// 含 tag。0 表示路径探测，没有内层。
    pub ciphertext: Vec<u8>,
}

pub fn encode_outer(h: &OuterHeader) -> Result<Vec<u8>, ProtoError> {
    if h.ciphertext.len() > MAX_CIPHERTEXT {
        return Err(ProtoError::TooLarge);
    }
    let total = OUTER_HEADER_LEN + h.ciphertext.len();
    if total > MAX_DATAGRAM {
        return Err(ProtoError::TooLarge);
    }
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&JRP_MAGIC.to_le_bytes());
    out.push(JRP_VERSION);
    out.push(h.flags.pack());
    out.extend_from_slice(&h.routing_token);
    out.extend_from_slice(&h.packet_seq.to_le_bytes());
    out.extend_from_slice(&(h.ciphertext.len() as u16).to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&h.ciphertext);
    debug_assert_eq!(out.len(), total);
    Ok(out)
}

pub fn decode_outer(buf: &[u8]) -> Result<OuterHeader, ProtoError> {
    if buf.len() > MAX_DATAGRAM {
        return Err(ProtoError::TooLarge);
    }
    if buf.len() < OUTER_HEADER_LEN {
        return Err(ProtoError::Truncated);
    }
    let magic = read_u16(buf, 0)?;
    if magic != JRP_MAGIC {
        return Err(ProtoError::BadMagic);
    }
    let version = buf[2];
    if version != JRP_VERSION {
        return Err(ProtoError::BadVersion);
    }
    let flags = OuterFlags::unpack(buf[3]);
    let mut routing_token = [0u8; 16];
    routing_token.copy_from_slice(take(buf, 4, 16)?);
    let packet_seq = read_u32(buf, 20)?;
    let clen = read_u16(buf, 24)? as usize;
    let reserved = read_u16(buf, 26)?;
    if reserved != 0 {
        return Err(ProtoError::Reserved);
    }
    if clen > MAX_CIPHERTEXT {
        return Err(ProtoError::TooLarge);
    }
    if buf.len() < OUTER_HEADER_LEN + clen {
        return Err(ProtoError::Truncated);
    }
    if buf.len() != OUTER_HEADER_LEN + clen {
        return Err(ProtoError::Malformed("外层长度"));
    }
    Ok(OuterHeader {
        flags,
        routing_token,
        packet_seq,
        ciphertext: buf[OUTER_HEADER_LEN..].to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> OuterHeader {
        OuterHeader {
            flags: OuterFlags {
                from_controller: true,
                relay_only: false,
                handshake: true,
            },
            routing_token: [7u8; 16],
            packet_seq: 3,
            ciphertext: vec![1, 2, 3, 4],
        }
    }

    #[test]
    fn roundtrip() {
        let h = sample();
        let bytes = encode_outer(&h).unwrap();
        assert_eq!(decode_outer(&bytes).unwrap(), h);
    }

    #[test]
    fn truncated() {
        let bytes = encode_outer(&sample()).unwrap();
        assert_eq!(decode_outer(&bytes[..10]), Err(ProtoError::Truncated));
        assert_eq!(
            decode_outer(&bytes[..bytes.len() - 1]),
            Err(ProtoError::Truncated)
        );
    }

    #[test]
    fn bad_version() {
        let mut bytes = encode_outer(&sample()).unwrap();
        bytes[2] = 9;
        assert_eq!(decode_outer(&bytes), Err(ProtoError::BadVersion));
    }

    #[test]
    fn bad_magic() {
        let mut bytes = encode_outer(&sample()).unwrap();
        bytes[0] = 0;
        assert_eq!(decode_outer(&bytes), Err(ProtoError::BadMagic));
    }
}
