use jr_crypto::{enroll_message, Identity};
use jr_proto::{
    decode_frame, encode_frame, Enroll, EnrollOk, Reason, SignalType, JRP_VERSION,
};
use rustls::pki_types::ServerName;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

use crate::{live, NodeConfig, Shared};

pub(crate) fn make_enroll_frame(identity: &Identity, cfg: &NodeConfig, token_or_nonce: [u8; 32]) -> Result<Vec<u8>, String> {
    let device_id = identity.device_id();
    let enroll = Enroll {
        fleet_token: token_or_nonce,
        device_id,
        sig: identity.sign(&enroll_message(&token_or_nonce, &device_id)),
        name: cfg.name.clone(),
        platform: cfg.platform,
        feature_bits: live::local_features_for(cfg.platform, cfg.os_inject, cfg.os_capture),
    };
    encode_frame(SignalType::Enroll as u8, &enroll.encode().map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())
}

pub(crate) fn read_challenge(frame: &jr_proto::SignalFrame) -> Result<[u8; 32], String> {
    if frame.msg_type != SignalType::AuthChallenge as u8 || frame.body.len() != 32 {
        return Err("服务器未开启封闭配对鉴权".into());
    }
    let mut nonce = [0; 32];
    nonce.copy_from_slice(&frame.body);
    Ok(nonce)
}

pub(crate) fn pairing_message(body: &[u8]) -> String {
    if body == b"closed" {
        "服务器未开放新设备配对。请先经 SSH 临时开启配对窗口，再重新打开客户端。".into()
    } else {
        let code = String::from_utf8_lossy(body);
        format!("设备待批准，核对码 {code}。请在服务器通过 SSH 核对并批准，然后重新打开客户端。")
    }
}

pub(crate) async fn reconnect_signal(
    cfg: &NodeConfig,
    connector: &TlsConnector,
    enroll_frame: &[u8],
    shared: &Shared,
) -> Result<(tokio_rustls::client::TlsStream<TcpStream>, Vec<u8>, jr_proto::SignalFrame), String> {
    let tcp = TcpStream::connect(cfg.signal).await.map_err(|e| e.to_string())?;
    tcp.set_nodelay(true).map_err(|e| e.to_string())?;
    let name = ServerName::try_from("localhost").map_err(|e| e.to_string())?;
    let mut tls = connector.connect(name, tcp).await.map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    let frame = if cfg.fleet_token == [0; 32] {
        let challenge = read_frame(&mut tls, &mut buf).await.map_err(|_| "没有收到连接挑战".to_string())?;
        let nonce = read_challenge(&challenge)?;
        let state = shared.state.lock().unwrap();
        make_enroll_frame(&state.identity, cfg, nonce)?
    } else {
        enroll_frame.to_vec()
    };
    tls.write_all(&frame).await.map_err(|e| e.to_string())?;
    let first = read_frame(&mut tls, &mut buf).await.map_err(|_| "注册被拒绝".to_string())?;
    if first.msg_type == SignalType::PairPending as u8 {
        return Err(pairing_message(&first.body));
    }
    if first.msg_type != SignalType::EnrollOk as u8 {
        return Err("注册被拒绝".into());
    }
    let ok = EnrollOk::decode(&first.body).map_err(|e| e.to_string())?;
    if ok.version != JRP_VERSION {
        return Err(Reason::VersionMismatch.user_text().into());
    }
    let snapshot = read_frame(&mut tls, &mut buf).await.map_err(|_| "没有收到设备列表".to_string())?;
    Ok((tls, buf, snapshot))
}

pub(crate) async fn read_frame(
    io: &mut tokio_rustls::client::TlsStream<TcpStream>,
    buf: &mut Vec<u8>,
) -> Result<jr_proto::SignalFrame, ()> {
    read_frame_from(io, buf).await
}

pub(crate) async fn read_frame_split<R: AsyncReadExt + Unpin>(io: &mut R, buf: &mut Vec<u8>) -> Result<jr_proto::SignalFrame, ()> {
    read_frame_from(io, buf).await
}

async fn read_frame_from<R: AsyncReadExt + Unpin>(io: &mut R, buf: &mut Vec<u8>) -> Result<jr_proto::SignalFrame, ()> {
    loop {
        if let Ok((frame, n)) = decode_frame(buf) {
            buf.drain(..n);
            return Ok(frame);
        }
        let mut tmp = [0u8; 4096];
        let n = io.read(&mut tmp).await.map_err(|_| ())?;
        if n == 0 {
            return Err(());
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.len() > 70_000 {
            return Err(());
        }
    }
}
