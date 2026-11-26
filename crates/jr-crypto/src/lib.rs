//! 设备身份、会话密钥和授权票据。不自造算法。
#![forbid(unsafe_code)]

use ring::aead::{self, Aad, LessSafeKey, Nonce, UnboundKey};
use ring::agreement::{self, UnparsedPublicKey};
use ring::digest;
use ring::hkdf;
use ring::rand::{SecureRandom, SystemRandom};
use ring::signature::{self, Ed25519KeyPair, KeyPair};

const GRANT_PREFIX: &[u8] = b"JRP-GRANT1";
const SIG_PREFIX: &[u8] = b"JRP1";

#[derive(Debug)]
pub enum CryptoError {
    Ring,
    Length,
    BadSignature,
    Expired,
    NotYetValid,
    Features,
}

impl std::fmt::Display for CryptoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CryptoError::Ring => write!(f, "加密库拒绝"),
            CryptoError::Length => write!(f, "长度不对"),
            CryptoError::BadSignature => write!(f, "签名无效"),
            CryptoError::Expired => write!(f, "授权已过期"),
            CryptoError::NotYetValid => write!(f, "授权尚未生效"),
            CryptoError::Features => write!(f, "授权未覆盖请求的能力"),
        }
    }
}

impl std::error::Error for CryptoError {}

fn ring<T>(r: Result<T, ring::error::Unspecified>) -> Result<T, CryptoError> {
    r.map_err(|_| CryptoError::Ring)
}

pub struct Identity {
    pkcs8: Vec<u8>,
    pair: Ed25519KeyPair,
}

impl Identity {
    pub fn generate() -> Result<Self, CryptoError> {
        let rng = SystemRandom::new();
        let doc = ring(Ed25519KeyPair::generate_pkcs8(&rng))?;
        Self::from_pkcs8(doc.as_ref())
    }

    pub fn from_pkcs8(bytes: &[u8]) -> Result<Self, CryptoError> {
        let pair = Ed25519KeyPair::from_pkcs8(bytes).map_err(|_| CryptoError::Ring)?;
        Ok(Self {
            pkcs8: bytes.to_vec(),
            pair,
        })
    }

    pub fn pkcs8(&self) -> &[u8] {
        &self.pkcs8
    }

    pub fn device_id(&self) -> [u8; 32] {
        let mut id = [0u8; 32];
        id.copy_from_slice(self.pair.public_key().as_ref());
        id
    }

    pub fn sign(&self, msg: &[u8]) -> [u8; 64] {
        let sig = self.pair.sign(msg);
        let mut out = [0u8; 64];
        out.copy_from_slice(sig.as_ref());
        out
    }
}

pub fn verify(device_id: &[u8; 32], msg: &[u8], sig: &[u8]) -> Result<(), CryptoError> {
    let peer = signature::UnparsedPublicKey::new(&signature::ED25519, device_id.as_ref());
    peer.verify(msg, sig).map_err(|_| CryptoError::BadSignature)
}

pub fn enroll_message(token: &[u8; 32], device_id: &[u8; 32]) -> Vec<u8> {
    let mut m = Vec::with_capacity(64);
    m.extend_from_slice(token);
    m.extend_from_slice(device_id);
    m
}

pub fn ephemeral_message(
    session_id: &[u8; 16],
    role: u8,
    eph_pub: &[u8; 32],
    peer_device_id: &[u8; 32],
) -> Vec<u8> {
    let mut m = Vec::with_capacity(4 + 16 + 1 + 32 + 32);
    m.extend_from_slice(SIG_PREFIX);
    m.extend_from_slice(session_id);
    m.push(role);
    m.extend_from_slice(eph_pub);
    m.extend_from_slice(peer_device_id);
    m
}

pub struct EphKey {
    private: agreement::EphemeralPrivateKey,
    pub public: [u8; 32],
}

impl EphKey {
    pub fn generate() -> Result<Self, CryptoError> {
        let rng = SystemRandom::new();
        let private = ring(agreement::EphemeralPrivateKey::generate(&agreement::X25519, &rng))?;
        let public_key = ring(private.compute_public_key())?;
        let bytes = public_key.as_ref();
        if bytes.len() != 32 {
            return Err(CryptoError::Length);
        }
        let mut public = [0u8; 32];
        public.copy_from_slice(bytes);
        Ok(Self { private, public })
    }

    pub fn agree(self, peer_pub: &[u8; 32]) -> Result<[u8; 32], CryptoError> {
        let peer = UnparsedPublicKey::new(&agreement::X25519, peer_pub.as_ref());
        agreement::agree_ephemeral(self.private, &peer, |shared| {
            if shared.len() != 32 {
                return Err(CryptoError::Length);
            }
            let mut out = [0u8; 32];
            out.copy_from_slice(shared);
            Ok(out)
        })
        .map_err(|_| CryptoError::Ring)?
    }
}

struct Okm64;
impl hkdf::KeyType for Okm64 {
    fn len(&self) -> usize {
        64
    }
}

fn hkdf_64(salt: &[u8], ikm: &[u8], info: &[u8]) -> Result<[u8; 64], CryptoError> {
    let salt = hkdf::Salt::new(hkdf::HKDF_SHA256, salt);
    let prk = salt.extract(ikm);
    let info_ref = [info];
    let okm = ring(prk.expand(&info_ref, Okm64))?;
    let mut out = [0u8; 64];
    ring(okm.fill(&mut out))?;
    Ok(out)
}

/// 会话密钥。Debug 只打印指纹，避免进日志。
pub struct SessionKeys {
    pub handshake_c2h: [u8; 32],
    pub handshake_h2c: [u8; 32],
    pub media_c2h: [u8; 32],
    pub media_h2c: [u8; 32],
}

impl std::fmt::Debug for SessionKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SessionKeys(fp={})", self.fingerprint_hex())
    }
}

impl SessionKeys {
    pub fn derive(
        shared: &[u8; 32],
        session_id: &[u8; 16],
        controller_id: &[u8; 32],
        host_id: &[u8; 32],
        controller_eph: &[u8; 32],
        host_eph: &[u8; 32],
        controller_sig: &[u8; 64],
        host_sig: &[u8; 64],
    ) -> Result<Self, CryptoError> {
        let hs = hkdf_64(session_id, shared, b"jrp-v1-handshake")?;
        let mut transcript = Vec::with_capacity(16 + 32 + 32 + 32 + 32 + 64 + 64);
        transcript.extend_from_slice(session_id);
        transcript.extend_from_slice(controller_id);
        transcript.extend_from_slice(host_id);
        transcript.extend_from_slice(controller_eph);
        transcript.extend_from_slice(host_eph);
        transcript.extend_from_slice(controller_sig);
        transcript.extend_from_slice(host_sig);
        let salt = digest::digest(&digest::SHA256, &transcript);
        let media = hkdf_64(salt.as_ref(), shared, b"jrp-v1-media")?;
        let mut handshake_c2h = [0u8; 32];
        let mut handshake_h2c = [0u8; 32];
        let mut media_c2h = [0u8; 32];
        let mut media_h2c = [0u8; 32];
        handshake_c2h.copy_from_slice(&hs[..32]);
        handshake_h2c.copy_from_slice(&hs[32..]);
        media_c2h.copy_from_slice(&media[..32]);
        media_h2c.copy_from_slice(&media[32..]);
        Ok(Self {
            handshake_c2h,
            handshake_h2c,
            media_c2h,
            media_h2c,
        })
    }

    pub fn fingerprint(&self) -> [u8; 8] {
        let mut buf = [0u8; 64];
        buf[..32].copy_from_slice(&self.media_c2h);
        buf[32..].copy_from_slice(&self.media_h2c);
        let d = digest::digest(&digest::SHA256, &buf);
        let mut fp = [0u8; 8];
        fp.copy_from_slice(&d.as_ref()[..8]);
        fp
    }

    pub fn fingerprint_hex(&self) -> String {
        jr_proto::hex_encode(&self.fingerprint())
    }

    pub fn direction_key(&self, from_controller: bool, handshake: bool) -> &[u8; 32] {
        match (from_controller, handshake) {
            (true, true) => &self.handshake_c2h,
            (false, true) => &self.handshake_h2c,
            (true, false) => &self.media_c2h,
            (false, false) => &self.media_h2c,
        }
    }
}

pub fn seal(key: &[u8; 32], nonce: &[u8; 12], plain: &[u8]) -> Result<Vec<u8>, CryptoError> {
    let unbound = ring(UnboundKey::new(&aead::AES_256_GCM, key))?;
    let key = LessSafeKey::new(unbound);
    let nonce = Nonce::assume_unique_for_key(*nonce);
    let mut out = plain.to_vec();
    ring(key.seal_in_place_append_tag(nonce, Aad::empty(), &mut out))?;
    Ok(out)
}

pub fn open(key: &[u8; 32], nonce: &[u8; 12], ciphertext: &[u8]) -> Result<Vec<u8>, CryptoError> {
    if ciphertext.len() < 16 {
        return Err(CryptoError::Length);
    }
    let unbound = ring(UnboundKey::new(&aead::AES_256_GCM, key))?;
    let key = LessSafeKey::new(unbound);
    let nonce = Nonce::assume_unique_for_key(*nonce);
    let mut out = ciphertext.to_vec();
    let plain = ring(key.open_in_place(nonce, Aad::empty(), &mut out))?;
    Ok(plain.to_vec())
}

pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

pub fn sha256(data: &[u8]) -> [u8; 32] {
    let d = digest::digest(&digest::SHA256, data);
    let mut out = [0u8; 32];
    out.copy_from_slice(d.as_ref());
    out
}

pub fn token_hash(token: &[u8]) -> [u8; 32] {
    sha256(token)
}

pub fn fleet_tag(token: &[u8; 32]) -> [u8; 8] {
    let d = sha256(token);
    let mut tag = [0u8; 8];
    tag.copy_from_slice(&d[..8]);
    tag
}

pub fn short_id(device_id: &[u8; 32], salt: u8) -> String {
    let digest = if salt == 0 {
        sha256(device_id)
    } else {
        let mut buf = Vec::with_capacity(33);
        buf.extend_from_slice(device_id);
        buf.push(salt);
        sha256(&buf)
    };
    crockford(&digest[..5])
}

fn crockford(data: &[u8]) -> String {
    const ALPH: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let mut bits: u64 = 0;
    for b in data {
        bits = (bits << 8) | u64::from(*b);
    }
    let mut out = [0u8; 8];
    for i in 0..8 {
        let shift = 35 - i * 5;
        out[i] = ALPH[((bits >> shift) & 31) as usize];
    }
    String::from_utf8(out.to_vec()).unwrap()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantClaims {
    pub host: [u8; 32],
    pub controller: [u8; 32],
    pub feature_bits: u32,
    pub not_before: u64,
    pub not_after: u64,
}

impl GrantClaims {
    fn payload(&self) -> Vec<u8> {
        let mut p = Vec::with_capacity(94);
        p.extend_from_slice(GRANT_PREFIX);
        p.extend_from_slice(&self.host);
        p.extend_from_slice(&self.controller);
        p.extend_from_slice(&self.feature_bits.to_le_bytes());
        p.extend_from_slice(&self.not_before.to_le_bytes());
        p.extend_from_slice(&self.not_after.to_le_bytes());
        p
    }
}

pub fn sign_grant(host: &Identity, claims: &GrantClaims) -> Vec<u8> {
    let payload = claims.payload();
    let sig = host.sign(&payload);
    let mut out = payload;
    out.extend_from_slice(&sig);
    out
}

pub fn verify_grant(assertion: &[u8], now: u64) -> Result<GrantClaims, CryptoError> {
    if assertion.len() != 94 + 64 {
        return Err(CryptoError::Length);
    }
    let (payload, sig) = assertion.split_at(94);
    if &payload[..10] != GRANT_PREFIX {
        return Err(CryptoError::BadSignature);
    }
    let mut host = [0u8; 32];
    host.copy_from_slice(&payload[10..42]);
    let mut controller = [0u8; 32];
    controller.copy_from_slice(&payload[42..74]);
    let feature_bits = u32::from_le_bytes(payload[74..78].try_into().unwrap());
    let not_before = u64::from_le_bytes(payload[78..86].try_into().unwrap());
    let not_after = u64::from_le_bytes(payload[86..94].try_into().unwrap());
    verify(&host, payload, sig)?;
    if now < not_before {
        return Err(CryptoError::NotYetValid);
    }
    if now > not_after {
        return Err(CryptoError::Expired);
    }
    if not_after.saturating_sub(not_before) > jr_proto::GRANT_MAX_AGE_SEC {
        return Err(CryptoError::Expired);
    }
    Ok(GrantClaims {
        host,
        controller,
        feature_bits,
        not_before,
        not_after,
    })
}

pub fn grant_covers(claims: &GrantClaims, controller: &[u8; 32], requested: u32) -> Result<(), CryptoError> {
    if &claims.controller != controller || requested & !claims.feature_bits != 0 {
        return Err(CryptoError::Features);
    }
    Ok(())
}

pub fn random_bytes<const N: usize>() -> Result<[u8; N], CryptoError> {
    let mut out = [0u8; N];
    ring(SystemRandom::new().fill(&mut out))?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grant_feature_bits_are_enforced() {
        let controller = [7u8; 32];
        // 位 3 是 input，位 6 是 terminal，位 0 是 video。
        let claims = GrantClaims {
            host: [9u8; 32],
            controller,
            feature_bits: (1 << 3) | (1 << 6),
            not_before: 0,
            not_after: u64::MAX,
        };
        assert!(grant_covers(&claims, &controller, (1 << 3) | (1 << 6)).is_ok());
        assert!(grant_covers(&claims, &controller, 1 << 0).is_err());
        assert!(grant_covers(&claims, &[8u8; 32], 1 << 3).is_err());
    }

    #[test]
    fn both_sides_match_and_forged_grant_fails() {
        let controller = Identity::generate().unwrap();
        let host = Identity::generate().unwrap();
        let c_eph = EphKey::generate().unwrap();
        let h_eph = EphKey::generate().unwrap();
        let session_id = [9u8; 16];
        let c_pub = c_eph.public;
        let h_pub = h_eph.public;
        let c_sig = controller.sign(&ephemeral_message(
            &session_id,
            1,
            &c_pub,
            &host.device_id(),
        ));
        let h_sig = host.sign(&ephemeral_message(
            &session_id,
            2,
            &h_pub,
            &controller.device_id(),
        ));
        verify(
            &controller.device_id(),
            &ephemeral_message(&session_id, 1, &c_pub, &host.device_id()),
            &c_sig,
        )
        .unwrap();
        let shared_c = c_eph.agree(&h_pub).unwrap();
        let shared_h = h_eph.agree(&c_pub).unwrap();
        assert_eq!(shared_c, shared_h);
        let keys_c = SessionKeys::derive(
            &shared_c,
            &session_id,
            &controller.device_id(),
            &host.device_id(),
            &c_pub,
            &h_pub,
            &c_sig,
            &h_sig,
        )
        .unwrap();
        let keys_h = SessionKeys::derive(
            &shared_h,
            &session_id,
            &controller.device_id(),
            &host.device_id(),
            &c_pub,
            &h_pub,
            &c_sig,
            &h_sig,
        )
        .unwrap();
        assert_eq!(keys_c.fingerprint(), keys_h.fingerprint());
        assert_eq!(keys_c.media_c2h, keys_h.media_c2h);
        let plain = b"hello-jrp";
        let nonce = jr_proto::nonce(1, true);
        let ct = seal(&keys_c.handshake_c2h, &nonce, plain).unwrap();
        let opened = open(&keys_h.handshake_c2h, &nonce, &ct).unwrap();
        assert_eq!(opened, plain);
        assert!(open(&keys_h.media_c2h, &nonce, &ct).is_err());

        let claims = GrantClaims {
            host: host.device_id(),
            controller: controller.device_id(),
            feature_bits: 0b1001,
            not_before: 1,
            not_after: 100,
        };
        let assertion = sign_grant(&host, &claims);
        assert_eq!(verify_grant(&assertion, 50).unwrap(), claims);
        let mut forged = assertion.clone();
        forged[20] ^= 0xff;
        assert!(verify_grant(&forged, 50).is_err());
        let attacker = Identity::generate().unwrap();
        let mut bad = claims.clone();
        bad.host = attacker.device_id();
        let forged_by_other = sign_grant(&attacker, &bad);
        let parsed = verify_grant(&forged_by_other, 50).unwrap();
        assert_ne!(parsed.host, host.device_id());
        assert!(verify_grant(&assertion, 1000).is_err());
        let mut signed_by_attacker = assertion.clone();
        let fake_sig = attacker.sign(&signed_by_attacker[..94]);
        signed_by_attacker[94..].copy_from_slice(&fake_sig);
        assert!(verify_grant(&signed_by_attacker, 50).is_err());
    }

    #[test]
    fn hkdf_known_answer_fingerprint() {
        let shared = [0x11u8; 32];
        let session_id = [0x22u8; 16];
        let controller_id = [0x33u8; 32];
        let host_id = [0x44u8; 32];
        let controller_eph = [0x55u8; 32];
        let host_eph = [0x66u8; 32];
        let controller_sig = [0x77u8; 64];
        let host_sig = [0x88u8; 64];
        let keys = SessionKeys::derive(
            &shared,
            &session_id,
            &controller_id,
            &host_id,
            &controller_eph,
            &host_eph,
            &controller_sig,
            &host_sig,
        )
        .unwrap();
        assert_eq!(keys.fingerprint_hex(), "b5efd1285e985a32");
    }

    #[test]
    fn short_id_len() {
        let id = Identity::generate().unwrap();
        let s = short_id(&id.device_id(), 0);
        assert_eq!(s.len(), 8);
        assert_ne!(s, short_id(&id.device_id(), 1));
    }
}
