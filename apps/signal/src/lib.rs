//! 自建信令。只做注册、在线和邀请转发，不派生会话密钥。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use jr_crypto::{enroll_message, random_bytes, short_id, token_hash, verify};
use jr_proto::{
    decode_frame, encode_frame, hex_encode, Candidates, Decision, DeviceSnap, Enroll, EnrollOk,
    Diagnostic, Ephemeral, Heartbeat, Invite, Presence, PresenceKind, RelayTicket, SignalType, Wake, PLATFORM_IOS,
    Reason, JRP_VERSION,
};
use rusqlite::{params, Connection};
use rustls::ServerConfig;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{mpsc, watch};
use tokio_rustls::TlsAcceptor;

mod cert;
use cert::load_or_create_cert;

#[derive(Clone)]
pub struct Config {
    pub bind: SocketAddr,
    pub data_dir: PathBuf,
    pub offline_after: Duration,
    pub invite_timeout: Duration,
    pub public_host: String,
    pub relay_port: u16,
    /// 反射发现的 UDP 端口。填 0 就随机选，实际端口在 Running 里给。
    pub reflex_port: u16,
}

pub struct Running {
    pub addr: SocketAddr,
    pub fingerprint: [u8; 32],
    /// 只有首次创建数据目录时有值。进程内测试用它，不落盘。
    pub fleet_token: Option<[u8; 32]>,
    /// 反射发现用到的那颗 UDP 端口。
    pub reflex_port: u16,
    shutdown: watch::Sender<bool>,
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
    }
}

struct Live {
    gen: u64,
    last_beat: Instant,
    blocker: u8,
    session_active: bool,
    marked_offline: bool,
    heard: Vec<[u8; 32]>,
    diag_window: Instant,
    diag_count: u16,
    tx: mpsc::UnboundedSender<Vec<u8>>,
}

struct Pending {
    controller: [u8; 32],
    host: [u8; 32],
}

struct SessionPeers {
    controller: [u8; 32],
    host: [u8; 32],
}

struct App {
    db: Mutex<Connection>,
    token_hash: Mutex<[u8; 32]>,
    live: Mutex<HashMap<[u8; 32], Live>>,
    pending: Mutex<HashMap<[u8; 16], Pending>>,
    sessions: Mutex<HashMap<[u8; 16], SessionPeers>>,
    next_gen: AtomicU64,
    offline_after: Duration,
    invite_timeout: Duration,
    public_host: String,
    relay_port: u16,
    data_dir: PathBuf,
    private_mode: bool,
    /// 等 UDP 探针的 nonce → 谁在问。只在几秒内有效。
    reflexive: Mutex<HashMap<u64, [u8; 32]>>,
}

pub async fn serve(cfg: Config) -> Result<Running, String> {
    serve_mode(cfg, false).await
}

/// 自用部署：新设备只能在服务器本机开启的短暂配对窗口内提交申请。
pub async fn serve_private(cfg: Config) -> Result<Running, String> {
    serve_mode(cfg, true).await
}

async fn serve_mode(cfg: Config, private_mode: bool) -> Result<Running, String> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    std::fs::create_dir_all(&cfg.data_dir).map_err(|e| e.to_string())?;
    let (token, hash, fresh) = if private_mode {
        ([0; 32], [0; 32], false)
    } else {
        load_or_create_token(&cfg.data_dir)?
    };
    let (certs, key, fingerprint) = load_or_create_cert(&cfg.data_dir)?;
    let db = open_db(&cfg.data_dir.join("fleet.sqlite"))?;
    let app = Arc::new(App {
        db: Mutex::new(db),
        token_hash: Mutex::new(hash),
        live: Mutex::new(HashMap::new()),
        pending: Mutex::new(HashMap::new()),
        sessions: Mutex::new(HashMap::new()),
        next_gen: AtomicU64::new(1),
        offline_after: cfg.offline_after,
        invite_timeout: cfg.invite_timeout,
        public_host: cfg.public_host,
        relay_port: cfg.relay_port,
        data_dir: cfg.data_dir.clone(),
        private_mode,
        reflexive: Mutex::new(HashMap::new()),
    });
    let server_cfg = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| e.to_string())?;
    let acceptor = TlsAcceptor::from(Arc::new(server_cfg));
    let listener = TcpListener::bind(cfg.bind).await.map_err(|e| e.to_string())?;
    let addr = listener.local_addr().map_err(|e| e.to_string())?;
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let app_acc = app.clone();
    let mut shutdown_acc = shutdown_rx.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown_acc.changed() => {
                    if *shutdown_acc.borrow() {
                        break;
                    }
                }
                accepted = listener.accept() => {
                    let Ok((sock, _)) = accepted else { break };
                    let _ = sock.set_nodelay(true);
                    let acc = acceptor.clone();
                    let app = app_acc.clone();
                    tokio::spawn(async move {
                        match tokio::time::timeout(Duration::from_secs(10), acc.accept(sock)).await {
                            Ok(Ok(tls)) => handle_conn(app, tls).await,
                            Ok(Err(e)) => eprintln!("signal: tls-fail {e}"),
                            Err(_) => eprintln!("signal: tls-timeout"),
                        }
                    });
                }
            }
        }
    });
    // 反射发现：节点从自己的媒体 socket 往这颗 UDP 端口发一个探针，
    // 信令把它看到的源地址回给对方。地址是公网上唯一能看到的东西，
    // 探针本身不带任何密钥材料。
    let reflex_sock = UdpSocket::bind(("0.0.0.0", cfg.reflex_port)).await.map_err(|e| e.to_string())?;
    let reflex_port = reflex_sock.local_addr().map_err(|e| e.to_string())?.port();
    let app_reflex = app.clone();
    let mut shutdown_reflex = shutdown_rx.clone();
    tokio::spawn(async move {
        let mut buf = [0u8; 64];
        loop {
            tokio::select! {
                _ = shutdown_reflex.changed() => {
                    if *shutdown_reflex.borrow() {
                        break;
                    }
                }
                got = reflex_sock.recv_from(&mut buf) => {
                    let Ok((n, src)) = got else { break };
                    if n < 12 || buf[0..4] != *b"JRRF" {
                        continue;
                    }
                    let mut nonce = [0u8; 8];
                    nonce.copy_from_slice(&buf[4..12]);
                    let nonce = u64::from_le_bytes(nonce);
                    let who = app_reflex.reflexive.lock().unwrap().remove(&nonce);
                    let Some(who) = who else { continue };
                    let ip = match src.ip() {
                        std::net::IpAddr::V4(v4) => v4.octets().to_vec(),
                        std::net::IpAddr::V6(_) => continue,
                    };
                    // 体：ip 长度 1 字节 | ip | 端口 2 字节小端。
                    let mut body = Vec::with_capacity(3 + ip.len());
                    body.push(ip.len() as u8);
                    body.extend_from_slice(&ip);
                    body.extend_from_slice(&src.port().to_le_bytes());
                    send_raw(&app_reflex, &who, SignalType::ReflexiveOk as u8, &body);
                }
            }
        }
    });
    println!("signal: reflex listening port={reflex_port}");
    let app_sweep = app.clone();
    let mut shutdown_sweep = shutdown_rx;
    tokio::spawn(async move {
        let tick = (app_sweep.offline_after / 3).max(Duration::from_millis(50));
        let mut interval = tokio::time::interval(tick);
        loop {
            tokio::select! {
                _ = shutdown_sweep.changed() => {
                    if *shutdown_sweep.borrow() {
                        break;
                    }
                }
                _ = interval.tick() => sweep_offline(&app_sweep),
            }
        }
    });
    println!("signal: ready addr={addr} fp={}", hex_encode(&fingerprint));
    Ok(Running {
        addr,
        fingerprint,
        fleet_token: fresh.then_some(token),
        reflex_port,
        shutdown: shutdown_tx,
    })
}

fn load_or_create_token(dir: &Path) -> Result<([u8; 32], [u8; 32], bool), String> {
    let path = dir.join("token.hash");
    if path.exists() {
        let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
        if bytes.len() != 32 {
            return Err("token.hash 损坏".into());
        }
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&bytes);
        return Ok(([0u8; 32], hash, false));
    }
    let token: [u8; 32] = random_bytes().map_err(|e| e.to_string())?;
    let hash = token_hash(&token);
    std::fs::write(&path, hash).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok((token, hash, true))
}

fn open_db(path: &Path) -> Result<Connection, String> {
    let conn = Connection::open(path).map_err(|e| e.to_string())?;
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS device (
            device_id BLOB PRIMARY KEY,
            account_id TEXT NOT NULL,
            name TEXT NOT NULL,
            platform INTEGER NOT NULL,
            short_id TEXT NOT NULL UNIQUE,
            short_salt INTEGER NOT NULL,
            feature_bits INTEGER NOT NULL,
            enrolled_at INTEGER NOT NULL,
            revoked_at INTEGER
        );
        CREATE TABLE IF NOT EXISTS audit (
            audit_id INTEGER PRIMARY KEY,
            controller_id BLOB NOT NULL,
            host_id BLOB NOT NULL,
            started_ms INTEGER NOT NULL,
            ended_ms INTEGER,
            end_reason INTEGER,
            path_id INTEGER,
            bytes_to_host INTEGER NOT NULL DEFAULT 0,
            bytes_to_controller INTEGER NOT NULL DEFAULT 0,
            feature_bits INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS grant_cache (
            host_id BLOB NOT NULL,
            controller_id BLOB NOT NULL,
            assertion BLOB NOT NULL,
            stored_at INTEGER NOT NULL,
            PRIMARY KEY (host_id, controller_id)
        );
        CREATE TABLE IF NOT EXISTS pair_window (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            until_ms INTEGER NOT NULL,
            remaining INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS pair_request (
            device_id BLOB PRIMARY KEY,
            code TEXT NOT NULL UNIQUE,
            name TEXT NOT NULL,
            platform INTEGER NOT NULL,
            feature_bits INTEGER NOT NULL,
            expires_ms INTEGER NOT NULL
        );
        ",
    )
    .map_err(|e| e.to_string())?;
    let _ = conn.execute("ALTER TABLE device ADD COLUMN wol_mac BLOB", []);
    Ok(conn)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

async fn handle_conn(app: Arc<App>, mut tls: tokio_rustls::server::TlsStream<TcpStream>) {
    let challenge = if app.private_mode {
        let Ok(nonce) = random_bytes::<32>() else { return };
        if write_frame(&mut tls, SignalType::AuthChallenge as u8, &nonce).await.is_err() {
            return;
        }
        Some(nonce)
    } else {
        None
    };
    let mut buf = Vec::new();
    let first = match tokio::time::timeout(Duration::from_secs(10), read_frame(&mut tls, &mut buf)).await {
        Ok(Ok(f)) => f,
        _ => return,
    };
    if first.msg_type != SignalType::Enroll as u8 {
        return;
    }
    let enroll = match Enroll::decode(&first.body) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("signal: bad-enroll {e}");
            return;
        }
    };
    let accepted = match challenge {
        Some(nonce) => accept_private_enroll(&app, &enroll, &nonce),
        None => accept_enroll(&app, &enroll).map(PrivateEnroll::Accepted),
    };
    let device_id = match accepted {
        Ok(PrivateEnroll::Accepted(id)) => id,
        Ok(PrivateEnroll::Pending(code)) => {
            let _ = write_frame(&mut tls, SignalType::PairPending as u8, code.as_bytes()).await;
            return;
        }
        Err(e) => {
            eprintln!("signal: enroll-reject {e}");
            return;
        }
    };
    let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let gen = app.next_gen.fetch_add(1, Ordering::Relaxed);
    {
        let mut live = app.live.lock().unwrap();
        live.insert(
            device_id,
            Live {
                gen,
                last_beat: Instant::now(),
                blocker: 0,
                session_active: false,
                marked_offline: false,
                heard: Vec::new(),
                diag_window: Instant::now(),
                diag_count: 0,
                tx,
            },
        );
    }
    let short = device_short(&app, &device_id);
    println!("signal: enroll name={} short={short}", enroll.name);
    let ok = EnrollOk {
        version: JRP_VERSION,
        short_id: short,
    };
    let _ = write_frame(&mut tls, SignalType::EnrollOk as u8, &ok.encode().unwrap()).await;
    if let Ok(body) = full_presence(&app).encode() {
        let _ = write_frame(&mut tls, SignalType::Presence as u8, &body).await;
    }
    broadcast(
        &app,
        SignalType::Presence as u8,
        &Presence {
            kind: PresenceKind::NewDevice,
            devices: vec![snap_one(&app, &device_id)],
        }
        .encode()
        .unwrap_or_default(),
        Some(device_id),
    );
    let app2 = app.clone();
    let (mut read_half, mut write_half) = tokio::io::split(tls);
    let writer = tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            if write_half.write_all(&frame).await.is_err() {
                break;
            }
        }
    });
    let mut read_buf = Vec::new();
    let mut revoke_check = tokio::time::interval(Duration::from_secs(1));
    loop {
        let frame = tokio::select! {
            _ = revoke_check.tick(), if app.private_mode => {
                if is_revoked(&app, &device_id) { break; }
                continue;
            }
            result = read_frame_split(&mut read_half, &mut read_buf) => match result {
                Ok(f) => f,
                Err(_) => break,
            },
        };
        if on_frame(&app, device_id, gen, &frame).is_err() {
            break;
        }
    }
    writer.abort();
    disconnect(&app2, device_id, gen);
}

fn disconnect(app: &App, id: [u8; 32], gen: u64) {
    let removed = {
        let mut live = app.live.lock().unwrap();
        match live.get(&id) {
            Some(l) if l.gen == gen => {
                live.remove(&id);
                true
            }
            _ => false,
        }
    };
    if removed {
        println!("signal: offline short={}", device_short(app, &id));
        let _ = broadcast(
            app,
            SignalType::Presence as u8,
            &Presence {
                kind: PresenceKind::Offline,
                devices: vec![snap_one(app, &id)],
            }
            .encode()
            .unwrap_or_default(),
            None,
        );
    }
}

fn accept_enroll(app: &App, enroll: &Enroll) -> Result<[u8; 32], String> {
    let hash = token_hash(&enroll.fleet_token);
    let stored = *app.token_hash.lock().unwrap();
    let current_token = jr_crypto::ct_eq(&hash, &stored);
    verify(
        &enroll.device_id,
        &enroll_message(&enroll.fleet_token, &enroll.device_id),
        &enroll.sig,
    )
    .map_err(|_| "注册签名无效".to_string())?;
    let db = app.db.lock().unwrap();
    let revoked: Option<Option<i64>> = db
        .query_row(
            "SELECT revoked_at FROM device WHERE device_id = ?1",
            params![enroll.device_id.as_slice()],
            |r| r.get(0),
        )
        .ok();
    if let Some(Some(_)) = revoked {
        return Err("设备已撤销".into());
    }
    // 令牌只负责新设备入舰队。已注册设备靠自己的私钥重连；轮换令牌不踢掉它们。
    if !current_token && revoked.is_none() {
        return Err("令牌错误".into());
    }
    if revoked.is_some() {
        db.execute(
            "UPDATE device SET name = ?1, platform = ?2, feature_bits = ?3 WHERE device_id = ?4",
            params![
                enroll.name,
                enroll.platform,
                enroll.feature_bits,
                enroll.device_id.as_slice()
            ],
        )
        .map_err(|e| e.to_string())?;
        return Ok(enroll.device_id);
    }
    let mut salt = 0u8;
    let mut sid = short_id(&enroll.device_id, salt);
    loop {
        let clash: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM device WHERE short_id = ?1",
                params![sid],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if clash == 0 {
            break;
        }
        salt = salt.wrapping_add(1);
        if salt == 0 {
            return Err("短号耗尽".into());
        }
        sid = short_id(&enroll.device_id, salt);
    }
    db.execute(
        "INSERT INTO device (device_id, account_id, name, platform, short_id, short_salt, feature_bits, enrolled_at, revoked_at)
         VALUES (?1, 'owner', ?2, ?3, ?4, ?5, ?6, ?7, NULL)",
        params![
            enroll.device_id.as_slice(),
            enroll.name,
            enroll.platform,
            sid,
            salt,
            enroll.feature_bits,
            now_ms()
        ],
    )
    .map_err(|e| e.to_string())?;
    Ok(enroll.device_id)
}

enum PrivateEnroll {
    Accepted([u8; 32]),
    Pending(String),
}

fn accept_private_enroll(app: &App, enroll: &Enroll, nonce: &[u8; 32]) -> Result<PrivateEnroll, String> {
    if !jr_crypto::ct_eq(&enroll.fleet_token, nonce) {
        return Err("连接挑战不匹配".into());
    }
    verify(&enroll.device_id, &enroll_message(nonce, &enroll.device_id), &enroll.sig)
        .map_err(|_| "设备签名无效".to_string())?;
    let db = app.db.lock().unwrap();
    let revoked: Option<Option<i64>> = db.query_row(
        "SELECT revoked_at FROM device WHERE device_id = ?1",
        params![enroll.device_id.as_slice()],
        |r| r.get(0),
    ).ok();
    if let Some(Some(_)) = revoked {
        return Err("设备已撤销".into());
    }
    if revoked.is_some() {
        db.execute(
            "UPDATE device SET name = ?1, platform = ?2, feature_bits = ?3 WHERE device_id = ?4",
            params![enroll.name, enroll.platform, enroll.feature_bits, enroll.device_id.as_slice()],
        ).map_err(|e| e.to_string())?;
        return Ok(PrivateEnroll::Accepted(enroll.device_id));
    }
    let until: Option<(i64, i64)> = db.query_row(
        "SELECT until_ms, remaining FROM pair_window WHERE id = 1", [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    ).ok();
    if !until.is_some_and(|(time, remaining)| time > now_ms() && remaining > 0) {
        return Ok(PrivateEnroll::Pending("closed".into()));
    }
    let existing: Option<String> = db.query_row(
        "SELECT code FROM pair_request WHERE device_id = ?1 AND expires_ms > ?2",
        params![enroll.device_id.as_slice(), now_ms()], |r| r.get(0),
    ).ok();
    if let Some(code) = existing {
        return Ok(PrivateEnroll::Pending(code));
    }
    let pending_count: i64 = db.query_row(
        "SELECT COUNT(*) FROM pair_request WHERE expires_ms > ?1",
        params![now_ms()],
        |r| r.get(0),
    ).map_err(|e| e.to_string())?;
    if pending_count >= 100 {
        return Err("待审核设备数量已达上限".into());
    }
    db.execute(
        "DELETE FROM pair_request WHERE device_id = ?1 AND expires_ms <= ?2",
        params![enroll.device_id.as_slice(), now_ms()],
    ).map_err(|e| e.to_string())?;
    for _ in 0..4 {
        let bytes: [u8; 8] = random_bytes().map_err(|e| e.to_string())?;
        let code = format!("{:08}", u64::from_le_bytes(bytes) % 100_000_000);
        if db.execute(
            "INSERT INTO pair_request (device_id, code, name, platform, feature_bits, expires_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![enroll.device_id.as_slice(), code, enroll.name, enroll.platform, enroll.feature_bits, now_ms() + 600_000],
        ).is_ok() {
            return Ok(PrivateEnroll::Pending(code));
        }
    }
    Err("配对申请暂不可用".into())
}

/// 仅供服务器本机管理员命令调用。公网信令协议没有开启配对的入口。
pub fn pair_open(data_dir: &Path, seconds: u64) -> Result<(), String> {
    if !(60..=3600).contains(&seconds) {
        return Err("配对窗口时长须为 60 到 3600 秒".into());
    }
    let db = open_db(&data_dir.join("fleet.sqlite"))?;
    db.execute("DELETE FROM pair_request", []).map_err(|e| e.to_string())?;
    db.execute(
        "INSERT OR REPLACE INTO pair_window (id, until_ms, remaining) VALUES (1, ?1, 1)",
        params![now_ms() + seconds as i64 * 1000],
    ).map_err(|e| e.to_string())?;
    Ok(())
}

pub fn pair_list(data_dir: &Path) -> Result<Vec<String>, String> {
    let db = open_db(&data_dir.join("fleet.sqlite"))?;
    let mut stmt = db.prepare(
        "SELECT code, hex(device_id), name, expires_ms FROM pair_request WHERE expires_ms > ?1 ORDER BY expires_ms",
    ).map_err(|e| e.to_string())?;
    let rows = stmt.query_map(params![now_ms()], |r| {
        Ok(format!("{}  {}  {}  expires_ms={}", r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?))
    }).map_err(|e| e.to_string())?;
    rows.collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())
}

pub fn pair_approve(data_dir: &Path, code: &str) -> Result<String, String> {
    if code.len() != 8 || !code.bytes().all(|b| b.is_ascii_digit()) {
        return Err("核对码须为 8 位数字".into());
    }
    let mut db = open_db(&data_dir.join("fleet.sqlite"))?;
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).map_err(|e| e.to_string())?;
    let (id, name, platform, feature_bits): (Vec<u8>, String, u8, u32) = tx.query_row(
        "SELECT device_id, name, platform, feature_bits FROM pair_request WHERE code = ?1 AND expires_ms > ?2",
        params![code, now_ms()],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    ).map_err(|_| "没有有效的配对申请".to_string())?;
    let changed = tx.execute(
        "UPDATE pair_window SET remaining = remaining - 1 WHERE id = 1 AND until_ms > ?1 AND remaining > 0",
        params![now_ms()],
    ).map_err(|e| e.to_string())?;
    if changed != 1 {
        return Err("配对窗口已关闭".into());
    }
    if id.len() != 32 {
        return Err("配对申请设备标识无效".into());
    }
    let mut device_id = [0u8; 32];
    device_id.copy_from_slice(&id);
    let mut salt = 0u8;
    let short = loop {
        let short = short_id(&device_id, salt);
        let exists: i64 = tx.query_row(
            "SELECT COUNT(*) FROM device WHERE short_id = ?1", params![short], |r| r.get(0),
        ).map_err(|e| e.to_string())?;
        if exists == 0 { break short; }
        salt = salt.checked_add(1).ok_or("短号耗尽")?;
    };
    tx.execute(
        "INSERT INTO device (device_id, account_id, name, platform, short_id, short_salt, feature_bits, enrolled_at, revoked_at)
         VALUES (?1, 'owner', ?2, ?3, ?4, ?5, ?6, ?7, NULL)",
        params![id, name, platform, short, salt, feature_bits, now_ms()],
    ).map_err(|e| e.to_string())?;
    tx.execute("DELETE FROM pair_request", []).map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| e.to_string())?;
    Ok(format!("已批准 {} ({})", name, short))
}

pub fn device_list(data_dir: &Path) -> Result<Vec<String>, String> {
    let db = open_db(&data_dir.join("fleet.sqlite"))?;
    let mut stmt = db.prepare(
        "SELECT hex(device_id), name, short_id, revoked_at FROM device ORDER BY enrolled_at",
    ).map_err(|e| e.to_string())?;
    let rows = stmt.query_map([], |r| {
        let revoked: Option<i64> = r.get(3)?;
        Ok(format!("{}  {}  {}  {}", r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, if revoked.is_some() { "已撤销" } else { "已批准" }))
    }).map_err(|e| e.to_string())?;
    rows.collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())
}

pub fn device_revoke(data_dir: &Path, hex_id: &str) -> Result<String, String> {
    let id = jr_proto::hex_decode(hex_id).map_err(|_| "设备 ID 须为 64 位十六进制".to_string())?;
    if id.len() != 32 {
        return Err("设备 ID 须为 64 位十六进制".into());
    }
    let db = open_db(&data_dir.join("fleet.sqlite"))?;
    let changed = db.execute(
        "UPDATE device SET revoked_at = ?1 WHERE device_id = ?2 AND revoked_at IS NULL",
        params![now_ms(), id],
    ).map_err(|e| e.to_string())?;
    if changed != 1 {
        return Err("设备不存在或已撤销".into());
    }
    Ok("已撤销设备；在线连接将在一秒内断开".into())
}

fn on_frame(app: &Arc<App>, self_id: [u8; 32], gen: u64, frame: &jr_proto::SignalFrame) -> Result<(), ()> {
    if app.private_mode && matches!(SignalType::from_u8(frame.msg_type),
        Some(SignalType::RevokeDevice | SignalType::RotateFleetToken)) {
        return Err(());
    }
    // 撤销或被新的连接替换后，旧 TLS 连接不能继续发管理命令。
    if !app.live.lock().unwrap().get(&self_id).is_some_and(|slot| slot.gen == gen) {
        return Err(());
    }
    match SignalType::from_u8(frame.msg_type) {
        Some(SignalType::Diagnostic) => {
            let diag = Diagnostic::decode(&frame.body).map_err(|_| ())?;
            // 每个已认证连接每分钟最多 120 条，避免客户端故障淹没 journal。
            let allowed = {
                let mut live = app.live.lock().unwrap();
                let slot = live.get_mut(&self_id).ok_or(())?;
                if slot.diag_window.elapsed() >= Duration::from_secs(60) {
                    slot.diag_window = Instant::now();
                    slot.diag_count = 0;
                }
                if slot.diag_count >= 120 { false } else {
                    slot.diag_count += 1;
                    true
                }
            };
            if allowed {
                let peer_len = diag.peer.iter().position(|b| *b == 0).unwrap_or(8);
                let peer = std::str::from_utf8(&diag.peer[..peer_len]).unwrap_or("");
                println!("signal: client-diag device={} event={} value={} session={} peer={}",
                    device_short(app, &self_id), diag.label(), diag.value,
                    hex_encode(&diag.session), peer);
            }
            Ok(())
        }
        Some(SignalType::Heartbeat) => {
            let hb = Heartbeat::decode(&frame.body).map_err(|_| ())?;
            let (came_back, session_ended) = {
                let mut live = app.live.lock().unwrap();
                if let Some(slot) = live.get_mut(&self_id) {
                    let was = slot.marked_offline;
                    let ended = slot.session_active && !hb.session_active;
                    slot.last_beat = Instant::now();
                    slot.blocker = hb.blocker;
                    slot.heard = hb.heard;
                    slot.session_active = hb.session_active;
                    slot.marked_offline = false;
                    (was, ended)
                } else {
                    (false, false)
                }
            };
            if session_ended {
                // 会话已经结束，路由留着只会随会话数增长。
                drop_sessions_for(app, &self_id);
                let _ = app.db.lock().unwrap().execute(
                    "UPDATE audit SET ended_ms = ?1, end_reason = ?2 WHERE host_id = ?3 AND ended_ms IS NULL",
                    params![now_ms(), Reason::UserHangup as u16, self_id.as_slice()],
                );
                println!("signal: audit-end host={}", device_short(app, &self_id));
            }
            if came_back {
                println!("signal: online short={}", device_short(app, &self_id));
            }
            let _ = broadcast(
                app,
                SignalType::Presence as u8,
                &Presence {
                    kind: PresenceKind::Upsert,
                    devices: vec![snap_one(app, &self_id)],
                }
                .encode()
                .unwrap_or_default(),
                Some(self_id),
            );
            Ok(())
        }
        Some(SignalType::Invite) => {
            let invite = Invite::decode(&frame.body).map_err(|_| ())?;
            if invite.controller_id != self_id {
                return Err(());
            }
            handle_invite(app, invite);
            Ok(())
        }
        Some(SignalType::InviteDecision) => {
            let decision = Decision::decode(&frame.body).map_err(|_| ())?;
            handle_decision(app, self_id, decision);
            Ok(())
        }
        Some(SignalType::Ephemeral) => {
            if frame.body.len() > 256 {
                return Err(());
            }
            let eph = Ephemeral::decode(&frame.body).map_err(|_| ())?;
            if eph.sender_id != self_id {
                return Err(());
            }
            forward_session(app, &eph.session_id, self_id, frame);
            Ok(())
        }
        Some(SignalType::Candidates) => {
            let c = Candidates::decode(&frame.body).map_err(|_| ())?;
            forward_session(app, &c.session_id, self_id, frame);
            Ok(())
        }
        Some(SignalType::RevokeDevice) => {
            if frame.body.len() != 32 {
                return Err(());
            }
            let mut id = [0u8; 32];
            id.copy_from_slice(&frame.body);
            revoke(app, id);
            Ok(())
        }
        Some(SignalType::StoreGrant) => {
            store_grant(app, self_id, &frame.body);
            Ok(())
        }
        Some(SignalType::RotateFleetToken) => {
            rotate_token(app, self_id);
            Ok(())
        }
        Some(SignalType::ReflexiveAsk) => {
            // 只记下谁在问。真正回话要等那颗 UDP 探针到，因为地址只有那儿看得到。
            if frame.body.len() >= 8 {
                let mut nonce = [0u8; 8];
                nonce.copy_from_slice(&frame.body[..8]);
                let mut map = app.reflexive.lock().unwrap();
                if map.len() > 512 {
                    map.clear();
                }
                map.insert(u64::from_le_bytes(nonce), self_id);
            }
            Ok(())
        }
        Some(SignalType::ReflexiveOk) => Ok(()),
        Some(SignalType::ListAudit) => {
            list_audit(app, self_id);
            Ok(())
        }
        Some(SignalType::Wake) => {
            let Ok(wake) = Wake::decode(&frame.body) else {
                return Err(());
            };
            forward_wake(app, self_id, wake);
            Ok(())
        }
        Some(SignalType::SetWake) => {
            if frame.body.len() != 6 {
                return Err(());
            }
            let _ = app.db.lock().unwrap().execute(
                "UPDATE device SET wol_mac = ?1 WHERE device_id = ?2",
                params![frame.body.as_slice(), self_id.as_slice()],
            );
            println!("signal: wol-mac stored short={}", device_short(app, &self_id));
            Ok(())
        }
        Some(_) => Ok(()),
        None => Ok(()),
    }
}

fn handle_invite(app: &Arc<App>, invite: Invite) {
    if invite.controller_id == invite.host_id {
        send_decision(app, &invite.controller_id, &invite.session_id, false, Reason::Rejected);
        return;
    }
    let host_platform = platform_of(app, &invite.host_id);
    let reason = if host_platform.is_none() {
        Some(Reason::Rejected)
    } else if host_platform == Some(PLATFORM_IOS) {
        Some(Reason::PlatformNotHostable)
    } else if is_revoked(app, &invite.host_id) {
        Some(Reason::Revoked)
    } else if !is_online(app, &invite.host_id) {
        Some(Reason::SessionDead)
    } else if is_busy(app, &invite.host_id) {
        Some(Reason::Busy)
    } else {
        None
    };
    if let Some(reason) = reason {
        send_decision(app, &invite.controller_id, &invite.session_id, false, reason);
        println!(
            "signal: invite-reject reason={} host={}",
            reason.user_text(),
            device_short(app, &invite.host_id)
        );
        return;
    }
    app.pending.lock().unwrap().insert(
        invite.session_id,
        Pending {
            controller: invite.controller_id,
            host: invite.host_id,
        },
    );
    let body = invite.encode().unwrap();
    send_raw(app, &invite.host_id, SignalType::Invite as u8, &body);
    let timeout = app.invite_timeout;
    let session_id = invite.session_id;
    let controller = invite.controller_id;
    let app_timeout = app.clone();
    tokio::spawn(async move {
        tokio::time::sleep(timeout).await;
        let removed = app_timeout.pending.lock().unwrap().remove(&session_id);
        if let Some(pending) = removed {
            if pending.controller == controller {
                send_decision(
                    &app_timeout,
                    &controller,
                    &session_id,
                    false,
                    Reason::Unauthorized,
                );
                println!("signal: invite-timeout");
            }
        }
    });
    println!(
        "signal: invite controller={} host={}",
        device_short(app, &invite.controller_id),
        device_short(app, &invite.host_id)
    );
}

fn handle_decision(app: &App, host_id: [u8; 32], decision: Decision) {
    let pending = app.pending.lock().unwrap().remove(&decision.session_id);
    let Some(pending) = pending else {
        return;
    };
    if pending.host != host_id {
        return;
    }
    app.sessions.lock().unwrap().insert(
        decision.session_id,
        SessionPeers {
            controller: pending.controller,
            host: pending.host,
        },
    );
    send_raw(
        app,
        &pending.controller,
        SignalType::InviteDecision as u8,
        &decision.encode(),
    );
    if !decision.accept {
        app.sessions.lock().unwrap().remove(&decision.session_id);
        println!("signal: invite-declined");
        return;
    }
    let token: [u8; 16] = match random_bytes() {
        Ok(t) => t,
        Err(_) => {
            send_decision(app, &pending.controller, &decision.session_id, false, Reason::SessionDead);
            return;
        }
    };
    let ticket = RelayTicket {
        session_id: decision.session_id,
        routing_token: token,
        relay_host: app.public_host.clone(),
        relay_port: app.relay_port,
        expire_unix: (now_ms() / 1000) as u32 + 60,
    };
    if let Ok(body) = ticket.encode() {
        send_raw(app, &pending.controller, SignalType::RelayTicket as u8, &body);
        send_raw(app, &pending.host, SignalType::RelayTicket as u8, &body);
    }
    let _ = app.db.lock().unwrap().execute(
        "INSERT INTO audit (controller_id, host_id, started_ms, feature_bits) VALUES (?1, ?2, ?3, 0)",
        params![pending.controller.as_slice(), pending.host.as_slice(), now_ms()],
    );
    println!(
        "signal: invite-accept token-prefix={}",
        hex_encode(&token[..4])
    );
}

pub fn pick_wake_proxy(nodes: &[([u8; 32], Vec<[u8; 32]>)], target: [u8; 32]) -> Option<[u8; 32]> {
    nodes
        .iter()
        .find(|(id, heard)| *id != target && heard.iter().any(|h| *h == target))
        .map(|(id, _)| *id)
}

fn forward_wake(app: &App, from: [u8; 32], wake: Wake) {
    let mac = app
        .db
        .lock()
        .unwrap()
        .query_row(
            "SELECT wol_mac FROM device WHERE device_id = ?1",
            params![wake.target.as_slice()],
            |row| row.get::<_, Option<Vec<u8>>>(0),
        )
        .ok()
        .flatten();
    let Some(mac) = mac.filter(|m| m.len() == 6) else {
        send_raw(app, &from, SignalType::Wake as u8, &Wake { target: wake.target, mac: [0; 6] }.encode());
        println!("signal: wake-unavailable");
        return;
    };
    let nodes: Vec<([u8; 32], Vec<[u8; 32]>)> = app
        .live
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, slot)| !slot.marked_offline)
        .map(|(id, slot)| (*id, slot.heard.clone()))
        .collect();
    let Some(proxy) = pick_wake_proxy(&nodes, wake.target) else {
        send_raw(app, &from, SignalType::Wake as u8, &Wake { target: wake.target, mac: [0; 6] }.encode());
        println!("signal: wake-unavailable");
        return;
    };
    let mut mac6 = [0u8; 6];
    mac6.copy_from_slice(&mac);
    send_raw(app, &proxy, SignalType::Wake as u8, &Wake { target: wake.target, mac: mac6 }.encode());
    println!("signal: wake-forward proxy={}", device_short(app, &proxy));
}

/// 取某人参与过的会话记录：他当主控的，或者他被控的。别的一律不给。
fn audit_rows(db: &rusqlite::Connection, who: [u8; 32]) -> Vec<(i64, i64, u16, u8)> {
    let Ok(mut stmt) = db.prepare(
        "SELECT started_ms, ifnull(ended_ms, 0), ifnull(end_reason, 0), ifnull(path_id, 0)
         FROM audit WHERE controller_id = ?1 OR host_id = ?1
         ORDER BY audit_id DESC LIMIT 50",
    ) else {
        return Vec::new();
    };
    let rows = stmt
        .query_map([&who], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)? as u16,
                row.get::<_, i64>(3)? as u8,
            ))
        })
        .ok();
    rows.into_iter().flatten().flatten().collect()
}

fn list_audit(app: &App, to: [u8; 32]) {
    let rows = {
        let db = app.db.lock().unwrap();
        audit_rows(&db, to)
    };
    let mut body = Vec::new();
    body.extend_from_slice(&(rows.len() as u16).to_le_bytes());
    for (started, ended, reason, path) in rows {
        body.extend_from_slice(&started.to_le_bytes());
        body.extend_from_slice(&ended.to_le_bytes());
        body.extend_from_slice(&reason.to_le_bytes());
        body.push(path);
    }
    send_raw(app, &to, SignalType::ListAudit as u8, &body);
    println!("signal: audit-list rows={}", body.len());
}

fn forward_session(app: &App, session_id: &[u8; 16], from: [u8; 32], frame: &jr_proto::SignalFrame) {
    let Some(peer) = session_peer(app, session_id, &from) else {
        return;
    };
    // 不打印正文。临时公钥和签名都不是信令该看的会话密钥，但正文本身也不进日志。
    println!("signal: forward type={} bytes={}", frame.msg_type, frame.body.len());
    send_raw(app, &peer, frame.msg_type, &frame.body);
}

fn session_peer(app: &App, session_id: &[u8; 16], from: &[u8; 32]) -> Option<[u8; 32]> {
    let sessions = app.sessions.lock().unwrap();
    let peers = sessions.get(session_id)?;
    if &peers.controller == from {
        Some(peers.host)
    } else if &peers.host == from {
        Some(peers.controller)
    } else {
        None
    }
}

/// 会话结束或设备离线后，路由不能一直留在表里。
/// 两边都按对端 id 清；同一对设备在信令里只会有一条活会话。
fn forget_sessions_for(sessions: &mut HashMap<[u8; 16], SessionPeers>, id: &[u8; 32]) {
    sessions.retain(|_, peers| {
        let is_controller = peers.controller == *id;
        let is_host = peers.host == *id;
        !is_controller && !is_host
    });
}

fn drop_sessions_for(app: &App, id: &[u8; 32]) {
    forget_sessions_for(&mut app.sessions.lock().unwrap(), id);
}

fn send_decision(app: &App, to: &[u8; 32], session: &[u8; 16], accept: bool, reason: Reason) {
    let d = Decision {
        session_id: *session,
        accept,
        reason: reason as u16,
    };
    send_raw(app, to, SignalType::InviteDecision as u8, &d.encode());
}

fn send_raw(app: &App, to: &[u8; 32], msg_type: u8, body: &[u8]) {
    if let Ok(frame) = encode_frame(msg_type, body) {
        if let Some(live) = app.live.lock().unwrap().get(to) {
            let _ = live.tx.send(frame);
        }
    }
}

fn broadcast(app: &App, msg_type: u8, body: &[u8], skip: Option<[u8; 32]>) {
    let Ok(frame) = encode_frame(msg_type, body) else {
        return;
    };
    let live = app.live.lock().unwrap();
    for (id, l) in live.iter() {
        if skip == Some(*id) {
            continue;
        }
        let _ = l.tx.send(frame.clone());
    }
}

fn is_online(app: &App, id: &[u8; 32]) -> bool {
    app.live
        .lock()
        .unwrap()
        .get(id)
        .map(|l| !l.marked_offline)
        .unwrap_or(false)
}

fn is_busy(app: &App, host: &[u8; 32]) -> bool {
    if app.pending.lock().unwrap().values().any(|p| &p.host == host) {
        return true;
    }
    app.live
        .lock()
        .unwrap()
        .get(host)
        .map(|l| l.session_active)
        .unwrap_or(false)
}

fn is_revoked(app: &App, id: &[u8; 32]) -> bool {
    app.db
        .lock()
        .unwrap()
        .query_row(
            "SELECT revoked_at FROM device WHERE device_id = ?1",
            params![id.as_slice()],
            |r| r.get::<_, Option<i64>>(0),
        )
        .ok()
        .flatten()
        .is_some()
}

fn platform_of(app: &App, id: &[u8; 32]) -> Option<u8> {
    app.db
        .lock()
        .unwrap()
        .query_row(
            "SELECT platform FROM device WHERE device_id = ?1",
            params![id.as_slice()],
            |r| r.get(0),
        )
        .ok()
}

fn device_short(app: &App, id: &[u8; 32]) -> String {
    app.db
        .lock()
        .unwrap()
        .query_row(
            "SELECT short_id FROM device WHERE device_id = ?1",
            params![id.as_slice()],
            |r| r.get(0),
        )
        .unwrap_or_else(|_| "????????".into())
}

fn snap_one(app: &App, id: &[u8; 32]) -> DeviceSnap {
    let db = app.db.lock().unwrap();
    let (name, platform, short_id, revoked): (String, u8, String, Option<i64>) = db
        .query_row(
            "SELECT name, platform, short_id, revoked_at FROM device WHERE device_id = ?1",
            params![id.as_slice()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap_or_else(|_| ("?".into(), 0, "????????".into(), None));
    drop(db);
    let live = app.live.lock().unwrap();
    let online = live.get(id);
    DeviceSnap {
        device_id: *id,
        name,
        platform,
        short_id,
        online: online.map(|l| !l.marked_offline).unwrap_or(false),
        revoked: revoked.is_some(),
        blocker: online.map(|l| l.blocker).unwrap_or(0),
        session_active: online.map(|l| l.session_active).unwrap_or(false),
    }
}

fn full_presence(app: &App) -> Presence {
    let ids: Vec<Vec<u8>> = {
        let db = app.db.lock().unwrap();
        let mut stmt = db
            .prepare("SELECT device_id FROM device")
            .expect("prepare");
        stmt.query_map([], |r| r.get::<_, Vec<u8>>(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect()
    };
    let mut devices = Vec::new();
    for raw in ids {
        if raw.len() != 32 {
            continue;
        }
        let mut id = [0u8; 32];
        id.copy_from_slice(&raw);
        devices.push(snap_one(app, &id));
    }
    Presence {
        kind: PresenceKind::Full,
        devices,
    }
}

fn revoke(app: &App, id: [u8; 32]) {
    let _ = app.db.lock().unwrap().execute(
        "UPDATE device SET revoked_at = ?1 WHERE device_id = ?2",
        params![now_ms(), id.as_slice()],
    );
    send_raw(app, &id, SignalType::Revoke as u8, &id);
    let snap = snap_one(app, &id);
    let _ = broadcast(
        app,
        SignalType::Presence as u8,
        &Presence {
            kind: PresenceKind::Revoked,
            devices: vec![snap],
        }
        .encode()
        .unwrap_or_default(),
        None,
    );
    if let Some(live) = app.live.lock().unwrap().remove(&id) {
        drop(live.tx);
    }
    drop_sessions_for(app, &id);
    println!("signal: revoke short={}", device_short(app, &id));
}

fn rotate_token(app: &App, requester: [u8; 32]) {
    let token: [u8; 32] = match random_bytes() {
        Ok(t) => t,
        Err(_) => return,
    };
    let hash = token_hash(&token);
    let path = app.data_dir.join("token.hash");
    if std::fs::write(&path, hash).is_err() {
        println!("signal: token-rotate-persist-failed");
        return;
    }
    *app.token_hash.lock().unwrap() = hash;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    send_raw(app, &requester, SignalType::RotateFleetToken as u8, &token);
    println!("signal: token-rotated");
}

fn store_grant(app: &App, host: [u8; 32], body: &[u8]) {
    if body.len() < 32 + 2 {
        return;
    }
    let mut controller = [0u8; 32];
    controller.copy_from_slice(&body[..32]);
    let n = u16::from_le_bytes([body[32], body[33]]) as usize;
    if body.len() != 34 + n {
        return;
    }
    let assertion = &body[34..];
    let _ = app.db.lock().unwrap().execute(
        "INSERT OR REPLACE INTO grant_cache (host_id, controller_id, assertion, stored_at) VALUES (?1, ?2, ?3, ?4)",
        params![host.as_slice(), controller.as_slice(), assertion, now_ms()],
    );
}

fn sweep_offline(app: &App) {
    let stale: Vec<[u8; 32]> = {
        let mut live = app.live.lock().unwrap();
        let mut ids = Vec::new();
        for (id, slot) in live.iter_mut() {
            if !slot.marked_offline && slot.last_beat.elapsed() > app.offline_after {
                slot.marked_offline = true;
                slot.session_active = false;
                ids.push(*id);
            }
        }
        ids
    };
    for id in stale {
        println!("signal: offline short={}", device_short(app, &id));
        drop_sessions_for(app, &id);
        let _ = broadcast(
            app,
            SignalType::Presence as u8,
            &Presence {
                kind: PresenceKind::Offline,
                devices: vec![snap_one(app, &id)],
            }
            .encode()
            .unwrap_or_default(),
            None,
        );
    }
}

async fn read_frame(
    tls: &mut tokio_rustls::server::TlsStream<TcpStream>,
    buf: &mut Vec<u8>,
) -> Result<jr_proto::SignalFrame, ()> {
    read_frame_from(tls, buf).await
}

async fn read_frame_split<R: AsyncReadExt + Unpin>(
    io: &mut R,
    buf: &mut Vec<u8>,
) -> Result<jr_proto::SignalFrame, ()> {
    read_frame_from(io, buf).await
}

async fn read_frame_from<R: AsyncReadExt + Unpin>(
    io: &mut R,
    buf: &mut Vec<u8>,
) -> Result<jr_proto::SignalFrame, ()> {
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

async fn write_frame(
    tls: &mut tokio_rustls::server::TlsStream<TcpStream>,
    msg_type: u8,
    body: &[u8],
) -> Result<(), ()> {
    let frame = encode_frame(msg_type, body).map_err(|_| ())?;
    tls.write_all(&frame).await.map_err(|_| ())
}

#[cfg(test)]
mod tests {
    use super::{accept_enroll, audit_rows, forget_sessions_for, open_db, pick_wake_proxy, App, SessionPeers};
    use std::collections::HashMap;
    use std::sync::atomic::AtomicU64;
    use std::sync::Mutex;
    use std::time::Duration;

    #[test]
    fn rotated_token_keeps_enrolled_devices_but_blocks_new_ones() {
        let dir = std::env::temp_dir().join(format!("jrp-token-{}-{}", std::process::id(), super::now_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        let old = [1u8; 32];
        let new = [2u8; 32];
        let app = App {
            db: Mutex::new(open_db(&dir.join("fleet.sqlite")).unwrap()),
            token_hash: Mutex::new(jr_crypto::token_hash(&old)),
            live: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
            next_gen: AtomicU64::new(1),
            offline_after: Duration::from_secs(15),
            invite_timeout: Duration::from_secs(60),
            public_host: "127.0.0.1".into(),
            relay_port: 47211,
            data_dir: dir.clone(),
            private_mode: false,
            reflexive: Mutex::new(HashMap::new()),
        };
        let make_enroll = |identity: &jr_crypto::Identity| {
            let device_id = identity.device_id();
            jr_proto::Enroll {
                fleet_token: old,
                device_id,
                sig: identity.sign(&jr_crypto::enroll_message(&old, &device_id)),
                name: "device".into(),
                platform: jr_proto::PLATFORM_MACOS,
                feature_bits: 0,
            }
        };
        let existing = jr_crypto::Identity::generate().unwrap();
        let enrolled = make_enroll(&existing);
        assert!(accept_enroll(&app, &enrolled).is_ok());
        *app.token_hash.lock().unwrap() = jr_crypto::token_hash(&new);
        assert!(accept_enroll(&app, &enrolled).is_ok());
        let stranger = make_enroll(&jr_crypto::Identity::generate().unwrap());
        assert!(accept_enroll(&app, &stranger).is_err());
        app.db.lock().unwrap().execute(
            "UPDATE device SET revoked_at = 1 WHERE device_id = ?1",
            rusqlite::params![enrolled.device_id.as_slice()],
        ).unwrap();
        assert!(accept_enroll(&app, &enrolled).is_err());
        drop(app);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn ended_sessions_are_dropped_from_the_routing_table() {
        let mut sessions: HashMap<[u8; 16], SessionPeers> = HashMap::new();
        let controller = [1u8; 32];
        let host = [2u8; 32];
        let other = [3u8; 32];
        let third = [4u8; 32];
        sessions.insert([10u8; 16], SessionPeers { controller, host });
        sessions.insert([11u8; 16], SessionPeers { controller: other, host: controller });
        sessions.insert([12u8; 16], SessionPeers { controller: other, host: third });
        forget_sessions_for(&mut sessions, &controller);
        assert_eq!(sessions.len(), 1, "只清掉含这个设备的两条路由");
        assert!(sessions.contains_key(&[12u8; 16]));
    }

    #[test]
    fn wake_proxy_must_have_heard_target() {
        let target = [9u8; 32];
        let proxy = [2u8; 32];
        let other = [3u8; 32];
        let nodes = vec![(other, vec![[1u8; 32]]), (proxy, vec![target])];
        assert_eq!(pick_wake_proxy(&nodes, target), Some(proxy));
        assert_eq!(pick_wake_proxy(&[(other, vec![])], target), None);
    }

    /// 审计只回自己参与的会话。别人和别人的会话一眼都看不到。
    #[test]
    fn audit_only_returns_rows_you_took_part_in() {
        let db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE audit (
                audit_id INTEGER PRIMARY KEY,
                controller_id BLOB NOT NULL,
                host_id BLOB NOT NULL,
                started_ms INTEGER NOT NULL,
                ended_ms INTEGER,
                end_reason INTEGER,
                path_id INTEGER,
                bytes_to_host INTEGER NOT NULL DEFAULT 0,
                bytes_to_controller INTEGER NOT NULL DEFAULT 0,
                feature_bits INTEGER NOT NULL
            );",
        )
        .unwrap();
        let me = [1u8; 32];
        let other = [2u8; 32];
        let third = [3u8; 32];
        let insert = |controller: [u8; 32], host: [u8; 32], started: i64| {
            db.execute(
                "INSERT INTO audit (controller_id, host_id, started_ms, feature_bits) VALUES (?1, ?2, ?3, 0)",
                rusqlite::params![controller.to_vec(), host.to_vec(), started],
            )
            .unwrap();
        };
        insert(me, other, 100);
        insert(third, me, 200);
        insert(other, third, 300);

        let rows = audit_rows(&db, me);
        assert_eq!(rows.len(), 2, "只该看到自己当主控和被控的两条");
        assert_eq!(rows[0].0, 200, "新的在前");
        assert_eq!(rows[1].0, 100);
        assert!(audit_rows(&db, [7u8; 32]).is_empty(), "没参与过就是空的");
    }
}
