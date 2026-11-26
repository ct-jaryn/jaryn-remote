use std::path::Path;

use jr_crypto::sha256;
use rcgen::{CertificateParams, KeyPair};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

pub fn load_or_create_cert(
    dir: &Path,
) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>, [u8; 32]), String> {
    let cert_path = dir.join("cert.der");
    let key_path = dir.join("key.der");
    if cert_path.exists() && key_path.exists() {
        let cert = std::fs::read(&cert_path).map_err(|e| e.to_string())?;
        let key = std::fs::read(&key_path).map_err(|e| e.to_string())?;
        let fingerprint = sha256(&cert);
        return Ok((
            vec![CertificateDer::from(cert)],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key)),
            fingerprint,
        ));
    }
    if cert_path.exists() || key_path.exists() {
        return Err("证书或私钥缺失，拒绝自动重建以免指纹悄悄变化".into());
    }
    let key_pair = KeyPair::generate().map_err(|e| e.to_string())?;
    let params = CertificateParams::new(vec!["localhost".into()]).map_err(|e| e.to_string())?;
    let cert = params.self_signed(&key_pair).map_err(|e| e.to_string())?;
    let cert_der = cert.der().to_vec();
    let key_der = key_pair.serialize_der();
    std::fs::write(&cert_path, &cert_der).map_err(|e| e.to_string())?;
    std::fs::write(&key_path, &key_der).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600));
    }
    let fingerprint = sha256(&cert_der);
    Ok((
        vec![CertificateDer::from(cert_der)],
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_der)),
        fingerprint,
    ))
}
