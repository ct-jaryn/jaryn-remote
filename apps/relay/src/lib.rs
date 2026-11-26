#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use jr_proto::{decode_outer, hex_encode};
use tokio::net::UdpSocket;
use tokio::sync::watch;

pub struct Config {
    pub bind: SocketAddr,
    pub idle: Duration,
    pub bandwidth_cap_bps: u64,
}

pub struct Running {
    pub addr: SocketAddr,
    shutdown: watch::Sender<bool>,
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
    }
}

struct Slot {
    a: SocketAddr,
    b: Option<SocketAddr>,
    last: Instant,
    window_start: Instant,
    window_bytes: u64,
}

pub async fn serve(cfg: Config) -> Result<Running, String> {
    let sock = Arc::new(UdpSocket::bind(cfg.bind).await.map_err(|e| e.to_string())?);
    let addr = sock.local_addr().map_err(|e| e.to_string())?;
    let table: Arc<Mutex<HashMap<[u8; 16], Slot>>> = Arc::new(Mutex::new(HashMap::new()));
    let (shutdown, mut rx) = watch::channel(false);
    let idle = cfg.idle;
    let cap = cfg.bandwidth_cap_bps;
    let sock_loop = sock.clone();
    tokio::spawn(async move {
        let mut buf = vec![0u8; 1500];
        loop {
            tokio::select! {
                changed = rx.changed() => {
                    if changed.is_err() || *rx.borrow() {
                        break;
                    }
                }
                got = sock_loop.recv_from(&mut buf) => {
                    let Ok((n, src)) = got else { break };
                    if n > jr_proto::MAX_DATAGRAM {
                        continue;
                    }
                    forward(&sock_loop, &table, &buf[..n], src, idle, cap).await;
                }
            }
        }
    });
    println!("relay: ready addr={addr}");
    Ok(Running { addr, shutdown })
}

async fn forward(
    sock: &UdpSocket,
    table: &Mutex<HashMap<[u8; 16], Slot>>,
    pkt: &[u8],
    src: SocketAddr,
    idle: Duration,
    cap_bps: u64,
) {
    let Ok(outer) = decode_outer(pkt) else {
        return;
    };
    let token = outer.routing_token;
    let now = Instant::now();
    let dest = {
        let mut map = table.lock().unwrap();
        map.retain(|_, slot| now.duration_since(slot.last) < idle);
        let slot = map.entry(token).or_insert_with(|| Slot {
            a: src,
            b: None,
            last: now,
            window_start: now,
            window_bytes: 0,
        });
        if now.duration_since(slot.window_start) >= Duration::from_secs(1) {
            slot.window_start = now;
            slot.window_bytes = 0;
        }
        if slot.window_bytes.saturating_add(pkt.len() as u64) > cap_bps / 8 {
            return;
        }
        if src == slot.a {
            slot.last = now;
            slot.window_bytes += pkt.len() as u64;
            slot.b
        } else if slot.b.is_none() {
            slot.b = Some(src);
            slot.last = now;
            slot.window_bytes += pkt.len() as u64;
            Some(slot.a)
        } else if slot.b == Some(src) {
            slot.last = now;
            slot.window_bytes += pkt.len() as u64;
            Some(slot.a)
        } else {
            println!("relay: drop third token-prefix={}", hex_encode(&token[..4]));
            return;
        }
    };
    if let Some(dest) = dest {
        let _ = sock.send_to(pkt, dest).await;
        println!(
            "relay: forward token-prefix={} bytes={}",
            hex_encode(&token[..4]),
            pkt.len()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jr_proto::{encode_outer, OuterFlags, OuterHeader};
    use std::time::Duration;

    fn packet(token: [u8; 16], seq: u32, body: &[u8]) -> Vec<u8> {
        encode_outer(&OuterHeader {
            flags: OuterFlags { from_controller: true, relay_only: true, handshake: false },
            routing_token: token,
            packet_seq: seq,
            ciphertext: body.to_vec(),
        })
        .unwrap()
    }

    #[tokio::test]
    async fn forwards_ciphertext_unchanged_and_drops_third() {
        let running = serve(Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            idle: Duration::from_secs(30),
            bandwidth_cap_bps: 8_000_000,
        })
        .await
        .unwrap();
        let a = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let b = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let c = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let token = [7u8; 16];
        let pkt = packet(token, 1, b"cipher-not-h264");
        a.send_to(&pkt, running.addr).await.unwrap();
        b.send_to(&pkt, running.addr).await.unwrap();
        let mut buf = [0u8; 1500];
        let (n, _) = tokio::time::timeout(Duration::from_secs(1), a.recv_from(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&buf[..n], pkt.as_slice());
        assert!(!pkt.windows(4).any(|w| w == b"\x00\x00\x00\x01"));
        c.send_to(&pkt, running.addr).await.unwrap();
        let late = tokio::time::timeout(Duration::from_millis(200), b.recv_from(&mut buf)).await;
        assert!(late.is_err(), "第三地址不能挤进会话");
    }
}
