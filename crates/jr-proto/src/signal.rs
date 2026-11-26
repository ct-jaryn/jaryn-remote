use crate::{read_lp_str, read_u16, read_u32, take, write_lp_str, ProtoError, Reason};

pub const MAX_FRAME_BODY: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SignalType {
    Enroll = 1,
    EnrollOk = 2,
    Heartbeat = 3,
    Presence = 4,
    Invite = 5,
    InviteDecision = 6,
    Ephemeral = 7,
    Candidates = 8,
    RelayTicket = 9,
    Revoke = 10,
    Wake = 11,
    ListAudit = 32,
    RevokeDevice = 33,
    RotateFleetToken = 34,
    RenameDevice = 35,
    StoreGrant = 36,
    SetWake = 37,
    /// 节点问信令「我这颗媒体 socket 在公网上是什么地址」，用来做打洞前的反射发现。
    /// 请求走 TLS，所以不用另做认证；回答里是信令看到的那颗 UDP 源地址。
    ReflexiveAsk = 38,
    ReflexiveOk = 39,
    /// 封闭配对模式下，每条 TLS 连接独有的 32 字节挑战。
    AuthChallenge = 40,
    /// 尚未获本机管理员批准，body 为核对码或 closed。
    PairPending = 41,
    /// 已认证节点上传的定长诊断事件；不包含画面、按键、剪贴板或任意日志文本。
    Diagnostic = 42,
}

impl SignalType {
    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            1 => Self::Enroll,
            2 => Self::EnrollOk,
            3 => Self::Heartbeat,
            4 => Self::Presence,
            5 => Self::Invite,
            6 => Self::InviteDecision,
            7 => Self::Ephemeral,
            8 => Self::Candidates,
            9 => Self::RelayTicket,
            10 => Self::Revoke,
            11 => Self::Wake,
            32 => Self::ListAudit,
            33 => Self::RevokeDevice,
            34 => Self::RotateFleetToken,
            35 => Self::RenameDevice,
            36 => Self::StoreGrant,
            37 => Self::SetWake,
            38 => Self::ReflexiveAsk,
            39 => Self::ReflexiveOk,
            40 => Self::AuthChallenge,
            41 => Self::PairPending,
            42 => Self::Diagnostic,
            _ => return None,
        })
    }
}

/// 私有部署的低流量诊断事件。服务器以已认证的 TLS 连接确定发送者。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Diagnostic {
    pub event: u8,
    pub value: u16,
    pub session: [u8; 4],
    pub peer: [u8; 8],
}

impl Diagnostic {
    pub const LEN: usize = 16;

    pub fn encode(self) -> [u8; Self::LEN] {
        let mut out = [0u8; Self::LEN];
        out[0] = 1;
        out[1] = self.event;
        out[2..4].copy_from_slice(&self.value.to_le_bytes());
        out[4..8].copy_from_slice(&self.session);
        out[8..16].copy_from_slice(&self.peer);
        out
    }

    pub fn decode(body: &[u8]) -> Result<Self, ProtoError> {
        if body.len() != Self::LEN || body[0] != 1 || !(1..=20).contains(&body[1]) {
            return Err(ProtoError::Malformed("diagnostic"));
        }
        let mut session = [0u8; 4];
        session.copy_from_slice(&body[4..8]);
        let mut peer = [0u8; 8];
        peer.copy_from_slice(&body[8..16]);
        if !peer.iter().all(|b| *b == 0 || b.is_ascii_uppercase() || b.is_ascii_digit()) {
            return Err(ProtoError::Malformed("diagnostic peer"));
        }
        Ok(Self { event: body[1], value: u16::from_le_bytes([body[2], body[3]]), session, peer })
    }

    pub fn label(self) -> &'static str {
        match self.event {
            1 => "ready", 2 => "invite-click", 3 => "invite-local-error",
            4 => "invite-sent", 5 => "invite-received", 6 => "invite-accepted",
            7 => "invite-rejected", 8 => "handshake-ready", 9 => "handshake-failed",
            10 => "host-capability", 11 => "stream-requested", 12 => "view-opened",
            13 => "capture-started", 14 => "capture-failed", 15 => "session-ended",
            16 => "path-selected", 17 => "signal-reconnected", 18 => "signal-disconnected",
            19 => "status", 20 => "view-failed", _ => "unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignalFrame {
    pub msg_type: u8,
    pub body: Vec<u8>,
}

pub fn encode_frame(msg_type: u8, body: &[u8]) -> Result<Vec<u8>, ProtoError> {
    if body.len() > MAX_FRAME_BODY {
        return Err(ProtoError::TooLarge);
    }
    let mut out = Vec::with_capacity(5 + body.len());
    out.extend_from_slice(&(body.len() as u32 + 1).to_le_bytes());
    out.push(msg_type);
    out.extend_from_slice(body);
    Ok(out)
}

/// 返回一帧和消费的字节数。数据不够时 Truncated。
pub fn decode_frame(buf: &[u8]) -> Result<(SignalFrame, usize), ProtoError> {
    if buf.len() < 4 {
        return Err(ProtoError::Truncated);
    }
    let len = read_u32(buf, 0)? as usize;
    if len == 0 || len > MAX_FRAME_BODY + 1 {
        return Err(ProtoError::TooLarge);
    }
    if buf.len() < 4 + len {
        return Err(ProtoError::Truncated);
    }
    Ok((
        SignalFrame {
            msg_type: buf[4],
            body: buf[5..4 + len].to_vec(),
        },
        4 + len,
    ))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Enroll {
    pub fleet_token: [u8; 32],
    pub device_id: [u8; 32],
    pub sig: [u8; 64],
    pub name: String,
    pub platform: u8,
    pub feature_bits: u32,
}

impl Enroll {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        let mut out = Vec::new();
        out.extend_from_slice(&self.fleet_token);
        out.extend_from_slice(&self.device_id);
        out.extend_from_slice(&self.sig);
        write_lp_str(&mut out, &self.name, 64)?;
        out.push(self.platform);
        out.extend_from_slice(&self.feature_bits.to_le_bytes());
        Ok(out)
    }

    pub fn decode(buf: &[u8]) -> Result<Self, ProtoError> {
        if buf.len() < 32 + 32 + 64 + 2 + 1 + 4 {
            return Err(ProtoError::Truncated);
        }
        let mut fleet_token = [0u8; 32];
        fleet_token.copy_from_slice(take(buf, 0, 32)?);
        let mut device_id = [0u8; 32];
        device_id.copy_from_slice(take(buf, 32, 32)?);
        let mut sig = [0u8; 64];
        sig.copy_from_slice(take(buf, 64, 64)?);
        let (name, off) = read_lp_str(buf, 128, 64)?;
        let platform = *buf.get(off).ok_or(ProtoError::Truncated)?;
        let feature_bits = read_u32(buf, off + 1)?;
        if buf.len() != off + 1 + 4 {
            return Err(ProtoError::Malformed("enroll 尾部"));
        }
        Ok(Self {
            fleet_token,
            device_id,
            sig,
            name,
            platform,
            feature_bits,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnrollOk {
    pub version: u8,
    pub short_id: String,
}

impl EnrollOk {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        if self.short_id.len() > 16 {
            return Err(ProtoError::TooLarge);
        }
        let mut out = vec![self.version, self.short_id.len() as u8];
        out.extend_from_slice(self.short_id.as_bytes());
        Ok(out)
    }

    pub fn decode(buf: &[u8]) -> Result<Self, ProtoError> {
        if buf.len() < 2 {
            return Err(ProtoError::Truncated);
        }
        let n = buf[1] as usize;
        let bytes = take(buf, 2, n)?;
        if buf.len() != 2 + n {
            return Err(ProtoError::Malformed("enrollok"));
        }
        let short_id = std::str::from_utf8(bytes)
            .map_err(|_| ProtoError::Malformed("short_id"))?
            .to_string();
        Ok(Self {
            version: buf[0],
            short_id,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Heartbeat {
    pub blocker: u8,
    pub session_active: bool,
    pub heard: Vec<[u8; 32]>,
}

impl Heartbeat {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        if self.heard.len() > 64 {
            return Err(ProtoError::TooLarge);
        }
        let mut out = vec![self.blocker, u8::from(self.session_active), self.heard.len() as u8];
        for id in &self.heard {
            out.extend_from_slice(id);
        }
        Ok(out)
    }

    pub fn decode(buf: &[u8]) -> Result<Self, ProtoError> {
        if buf.len() < 3 {
            return Err(ProtoError::Truncated);
        }
        let n = buf[2] as usize;
        if buf.len() != 3 + n * 32 {
            return Err(ProtoError::Truncated);
        }
        let mut heard = Vec::with_capacity(n);
        for i in 0..n {
            let mut id = [0u8; 32];
            id.copy_from_slice(take(buf, 3 + i * 32, 32)?);
            heard.push(id);
        }
        Ok(Self {
            blocker: buf[0],
            session_active: buf[1] != 0,
            heard,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PresenceKind {
    Full = 1,
    Upsert = 2,
    Offline = 3,
    Revoked = 4,
    NewDevice = 5,
}

impl PresenceKind {
    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            1 => Self::Full,
            2 => Self::Upsert,
            3 => Self::Offline,
            4 => Self::Revoked,
            5 => Self::NewDevice,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceSnap {
    pub device_id: [u8; 32],
    pub name: String,
    pub platform: u8,
    pub short_id: String,
    pub online: bool,
    pub revoked: bool,
    pub blocker: u8,
    pub session_active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Presence {
    pub kind: PresenceKind,
    pub devices: Vec<DeviceSnap>,
}

impl Presence {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        if self.devices.len() > u16::MAX as usize {
            return Err(ProtoError::TooLarge);
        }
        let mut out = vec![self.kind as u8];
        out.extend_from_slice(&(self.devices.len() as u16).to_le_bytes());
        for d in &self.devices {
            out.extend_from_slice(&d.device_id);
            write_lp_str(&mut out, &d.name, 64)?;
            out.push(d.platform);
            write_lp_str(&mut out, &d.short_id, 16)?;
            out.push(u8::from(d.online));
            out.push(u8::from(d.revoked));
            out.push(d.blocker);
            out.push(u8::from(d.session_active));
        }
        Ok(out)
    }

    pub fn decode(buf: &[u8]) -> Result<Self, ProtoError> {
        if buf.len() < 3 {
            return Err(ProtoError::Truncated);
        }
        let kind = PresenceKind::from_u8(buf[0]).ok_or(ProtoError::Malformed("presence kind"))?;
        let n = read_u16(buf, 1)? as usize;
        let mut off = 3;
        let mut devices = Vec::with_capacity(n);
        for _ in 0..n {
            let mut device_id = [0u8; 32];
            device_id.copy_from_slice(take(buf, off, 32)?);
            off += 32;
            let (name, next) = read_lp_str(buf, off, 64)?;
            off = next;
            let platform = *buf.get(off).ok_or(ProtoError::Truncated)?;
            off += 1;
            let (short_id, next) = read_lp_str(buf, off, 16)?;
            off = next;
            let online = *buf.get(off).ok_or(ProtoError::Truncated)? != 0;
            let revoked = *buf.get(off + 1).ok_or(ProtoError::Truncated)? != 0;
            let blocker = *buf.get(off + 2).ok_or(ProtoError::Truncated)?;
            let session_active = *buf.get(off + 3).ok_or(ProtoError::Truncated)? != 0;
            off += 4;
            devices.push(DeviceSnap {
                device_id,
                name,
                platform,
                short_id,
                online,
                revoked,
                blocker,
                session_active,
            });
        }
        if off != buf.len() {
            return Err(ProtoError::Malformed("presence 尾部"));
        }
        Ok(Self { kind, devices })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invite {
    pub session_id: [u8; 16],
    pub controller_id: [u8; 32],
    pub host_id: [u8; 32],
    pub feature_bits: u32,
    pub unattended: bool,
}

impl Invite {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        let mut out = Vec::with_capacity(16 + 32 + 32 + 4 + 1);
        out.extend_from_slice(&self.session_id);
        out.extend_from_slice(&self.controller_id);
        out.extend_from_slice(&self.host_id);
        out.extend_from_slice(&self.feature_bits.to_le_bytes());
        out.push(u8::from(self.unattended));
        Ok(out)
    }

    pub fn decode(buf: &[u8]) -> Result<Self, ProtoError> {
        if buf.len() != 16 + 32 + 32 + 4 + 1 {
            return Err(ProtoError::Truncated);
        }
        let mut session_id = [0u8; 16];
        session_id.copy_from_slice(take(buf, 0, 16)?);
        let mut controller_id = [0u8; 32];
        controller_id.copy_from_slice(take(buf, 16, 32)?);
        let mut host_id = [0u8; 32];
        host_id.copy_from_slice(take(buf, 48, 32)?);
        Ok(Self {
            session_id,
            controller_id,
            host_id,
            feature_bits: read_u32(buf, 80)?,
            unattended: buf[84] != 0,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub session_id: [u8; 16],
    pub accept: bool,
    pub reason: u16,
}

impl Decision {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(19);
        out.extend_from_slice(&self.session_id);
        out.push(u8::from(self.accept));
        out.extend_from_slice(&self.reason.to_le_bytes());
        out
    }

    pub fn decode(buf: &[u8]) -> Result<Self, ProtoError> {
        if buf.len() != 19 {
            return Err(ProtoError::Truncated);
        }
        let mut session_id = [0u8; 16];
        session_id.copy_from_slice(take(buf, 0, 16)?);
        Ok(Self {
            session_id,
            accept: buf[16] != 0,
            reason: read_u16(buf, 17)?,
        })
    }

    pub fn reason_enum(&self) -> Option<Reason> {
        Reason::from_u16(self.reason)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ephemeral {
    pub session_id: [u8; 16],
    pub sender_id: [u8; 32],
    pub eph_pub: [u8; 32],
    pub sig: [u8; 64],
    pub role: u8,
}

impl Ephemeral {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(145);
        out.extend_from_slice(&self.session_id);
        out.extend_from_slice(&self.sender_id);
        out.extend_from_slice(&self.eph_pub);
        out.extend_from_slice(&self.sig);
        out.push(self.role);
        debug_assert!(out.len() <= 256);
        out
    }

    pub fn decode(buf: &[u8]) -> Result<Self, ProtoError> {
        if buf.len() != 145 {
            return Err(ProtoError::Truncated);
        }
        let mut session_id = [0u8; 16];
        session_id.copy_from_slice(take(buf, 0, 16)?);
        let mut sender_id = [0u8; 32];
        sender_id.copy_from_slice(take(buf, 16, 32)?);
        let mut eph_pub = [0u8; 32];
        eph_pub.copy_from_slice(take(buf, 48, 32)?);
        let mut sig = [0u8; 64];
        sig.copy_from_slice(take(buf, 80, 64)?);
        Ok(Self {
            session_id,
            sender_id,
            eph_pub,
            sig,
            role: buf[144],
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SockAddr {
    pub path_id: u8,
    pub ip: Vec<u8>,
    pub port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidates {
    pub session_id: [u8; 16],
    pub addrs: Vec<SockAddr>,
}

impl Candidates {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        if self.addrs.len() > 8 {
            return Err(ProtoError::TooLarge);
        }
        let mut out = Vec::new();
        out.extend_from_slice(&self.session_id);
        out.push(self.addrs.len() as u8);
        for a in &self.addrs {
            if a.ip.len() != 4 && a.ip.len() != 16 {
                return Err(ProtoError::Malformed("ip"));
            }
            out.push(a.path_id);
            out.push(a.ip.len() as u8);
            out.extend_from_slice(&a.ip);
            out.extend_from_slice(&a.port.to_le_bytes());
        }
        Ok(out)
    }

    pub fn decode(buf: &[u8]) -> Result<Self, ProtoError> {
        if buf.len() < 17 {
            return Err(ProtoError::Truncated);
        }
        let mut session_id = [0u8; 16];
        session_id.copy_from_slice(take(buf, 0, 16)?);
        let n = buf[16] as usize;
        let mut off = 17;
        let mut addrs = Vec::with_capacity(n);
        for _ in 0..n {
            let path_id = *buf.get(off).ok_or(ProtoError::Truncated)?;
            let len = *buf.get(off + 1).ok_or(ProtoError::Truncated)? as usize;
            if len != 4 && len != 16 {
                return Err(ProtoError::Malformed("ip"));
            }
            let ip = take(buf, off + 2, len)?.to_vec();
            let port = read_u16(buf, off + 2 + len)?;
            off += 2 + len + 2;
            addrs.push(SockAddr { path_id, ip, port });
        }
        if off != buf.len() {
            return Err(ProtoError::Malformed("candidates"));
        }
        Ok(Self { session_id, addrs })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayTicket {
    pub session_id: [u8; 16],
    pub routing_token: [u8; 16],
    pub relay_host: String,
    pub relay_port: u16,
    pub expire_unix: u32,
}

impl RelayTicket {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        let mut out = Vec::new();
        out.extend_from_slice(&self.session_id);
        out.extend_from_slice(&self.routing_token);
        write_lp_str(&mut out, &self.relay_host, 255)?;
        out.extend_from_slice(&self.relay_port.to_le_bytes());
        out.extend_from_slice(&self.expire_unix.to_le_bytes());
        Ok(out)
    }

    pub fn decode(buf: &[u8]) -> Result<Self, ProtoError> {
        if buf.len() < 16 + 16 + 2 {
            return Err(ProtoError::Truncated);
        }
        let mut session_id = [0u8; 16];
        session_id.copy_from_slice(take(buf, 0, 16)?);
        let mut routing_token = [0u8; 16];
        routing_token.copy_from_slice(take(buf, 16, 16)?);
        let (relay_host, off) = read_lp_str(buf, 32, 255)?;
        let relay_port = read_u16(buf, off)?;
        let expire_unix = read_u32(buf, off + 2)?;
        if buf.len() != off + 6 {
            return Err(ProtoError::Malformed("ticket"));
        }
        Ok(Self {
            session_id,
            routing_token,
            relay_host,
            relay_port,
            expire_unix,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wake {
    pub target: [u8; 32],
    pub mac: [u8; 6],
}

impl Wake {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(38);
        out.extend_from_slice(&self.target);
        out.extend_from_slice(&self.mac);
        out
    }

    pub fn decode(buf: &[u8]) -> Result<Self, ProtoError> {
        if buf.len() != 38 {
            return Err(ProtoError::Truncated);
        }
        let mut target = [0u8; 32];
        target.copy_from_slice(take(buf, 0, 32)?);
        let mut mac = [0u8; 6];
        mac.copy_from_slice(take(buf, 32, 6)?);
        Ok(Self { target, mac })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_truncated() {
        assert_eq!(decode_frame(&[1, 0, 0]), Err(ProtoError::Truncated));
        let frame = encode_frame(1, b"abc").unwrap();
        assert_eq!(decode_frame(&frame[..frame.len() - 1]), Err(ProtoError::Truncated));
    }

    #[test]
    fn unknown_type_is_a_frame_not_a_hard_error() {
        let frame = encode_frame(99, b"future").unwrap();
        let (decoded, n) = decode_frame(&frame).unwrap();
        assert_eq!(n, frame.len());
        assert!(SignalType::from_u8(decoded.msg_type).is_none());
        assert_eq!(decoded.body, b"future");
    }

    #[test]
    fn diagnostic_is_fixed_size_and_rejects_arbitrary_text() {
        let event = Diagnostic {
            event: 2, value: 7, session: [1, 2, 3, 4], peer: *b"Z9B55QR4",
        };
        assert_eq!(Diagnostic::decode(&event.encode()).unwrap(), event);
        let mut bad = event.encode();
        bad[8] = b'\n';
        assert!(Diagnostic::decode(&bad).is_err());
        assert!(Diagnostic::decode(&bad[..15]).is_err());
    }

    #[test]
    fn enroll_roundtrip() {
        let e = Enroll {
            fleet_token: [1u8; 32],
            device_id: [2u8; 32],
            sig: [3u8; 64],
            name: "mac".into(),
            platform: 2,
            feature_bits: 9,
        };
        assert_eq!(Enroll::decode(&e.encode().unwrap()).unwrap(), e);
    }
}
