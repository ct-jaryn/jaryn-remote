use crate::{read_u16, take, ProtoError, JRP_MAGIC, JRP_VERSION};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Beacon {
    pub device_id: [u8; 32],
    pub fleet_tag: [u8; 8],
    pub tcp_port: u16,
}

pub fn encode_beacon(b: &Beacon) -> Vec<u8> {
    let mut out = Vec::with_capacity(45);
    out.extend_from_slice(&JRP_MAGIC.to_le_bytes());
    out.push(JRP_VERSION);
    out.extend_from_slice(&b.device_id);
    out.extend_from_slice(&b.fleet_tag);
    out.extend_from_slice(&b.tcp_port.to_le_bytes());
    out
}

pub fn decode_beacon(buf: &[u8]) -> Result<Beacon, ProtoError> {
    if buf.len() < 45 {
        return Err(ProtoError::Truncated);
    }
    if read_u16(buf, 0)? != JRP_MAGIC {
        return Err(ProtoError::BadMagic);
    }
    if buf[2] != JRP_VERSION {
        return Err(ProtoError::BadVersion);
    }
    let mut device_id = [0u8; 32];
    device_id.copy_from_slice(take(buf, 3, 32)?);
    let mut fleet_tag = [0u8; 8];
    fleet_tag.copy_from_slice(take(buf, 35, 8)?);
    Ok(Beacon {
        device_id,
        fleet_tag,
        tcp_port: read_u16(buf, 43)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let b = Beacon {
            device_id: [3u8; 32],
            fleet_tag: [9u8; 8],
            tcp_port: 0,
        };
        assert_eq!(decode_beacon(&encode_beacon(&b)).unwrap(), b);
    }
}
