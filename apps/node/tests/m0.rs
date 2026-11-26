use std::path::PathBuf;
use std::time::Duration;

use jr_crypto::{sign_grant, GrantClaims, Identity};
use jr_node::{Node, NodeConfig};
use jr_proto::{PLATFORM_MACOS, Reason};
use jr_signal::{serve, Config};

fn scratch(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "jrp-m0-{}-{}-{}",
        label,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn node_cfg(
    signal: std::net::SocketAddr,
    pin: [u8; 32],
    token: [u8; 32],
    dir: PathBuf,
    name: &str,
    auto_accept: bool,
    beacon_port: u16,
) -> NodeConfig {
    NodeConfig {
        signal,
        pin,
        data_dir: dir,
        name: name.into(),
        platform: PLATFORM_MACOS,
        fleet_token: token,
        auto_accept,
        heartbeat: Duration::from_millis(80),
        beacon_port,
        beacon_every: Duration::from_millis(200),
        force_relay: false,
        reflex_port: 0,
        advertise_override: None,
        extra_candidate: None,
        os_inject: false,
        os_capture: false,
        os_clipboard: false,
    }
}

async fn wait_until(timeout: Duration, mut pred: impl FnMut() -> bool) -> bool {
    let start = tokio::time::Instant::now();
    while start.elapsed() < timeout {
        if pred() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    pred()
}

#[tokio::test]
async fn remote_hangup_cleans_immediately_and_allows_reinvite() {
    let root = scratch("rehangup");
    let running = serve(Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        data_dir: root.join("signal"),
        offline_after: Duration::from_millis(400),
        invite_timeout: Duration::from_secs(2),
        public_host: "127.0.0.1".into(),
        relay_port: jr_proto::RELAY_PORT,
        reflex_port: 0,
    })
    .await
    .unwrap();
    let token = running.fleet_token.expect("first boot token");
    let pin = running.fingerprint;
    let beacon_port = std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let a = Node::start(node_cfg(running.addr, pin, token, root.join("a"), "alpha", false, beacon_port))
        .await
        .unwrap();
    let b = Node::start(node_cfg(running.addr, pin, token, root.join("b"), "beta", true, beacon_port))
        .await
        .unwrap();
    let online = wait_until(Duration::from_secs(3), || {
        a.devices().iter().any(|d| d.short_id == b.short_id && d.online)
            && b.devices().iter().any(|d| d.short_id == a.short_id && d.online)
    })
    .await;
    assert!(online, "双方先要互相看见");

    let fp = a.invite(&b.short_id, false).await.expect("first invite");
    assert_eq!(a.session_fp().as_deref(), Some(fp.as_str()));
    let host_ready = wait_until(Duration::from_secs(2), || b.session_fp().as_deref() == Some(fp.as_str())).await;
    assert!(host_ready, "被控端也要完成握手 host={:?}", b.session_fp());

    b.hangup();
    let cleaned = wait_until(Duration::from_secs(2), || {
        a.session_fp().is_none() && b.session_fp().is_none()
    })
    .await;
    assert!(cleaned, "对端挂断后两边都应立刻清理 a={:?} b={:?}", a.session_fp(), b.session_fp());

    let fp2 = a.invite(&b.short_id, false).await.expect("挂断后应能马上重新邀请");
    assert_eq!(a.session_fp().as_deref(), Some(fp2.as_str()));
    a.end_session();
    b.end_session();
}

#[tokio::test]
async fn two_nodes_see_presence_handshake_and_reject_forged_grant() {
    let root = scratch("fleet");
    let running = serve(Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        data_dir: root.join("signal"),
        offline_after: Duration::from_millis(400),
        invite_timeout: Duration::from_secs(2),
        public_host: "127.0.0.1".into(),
        relay_port: jr_proto::RELAY_PORT,
        reflex_port: 0,
    })
    .await
    .unwrap();
    let token = running.fleet_token.expect("first boot token");
    let pin = running.fingerprint;
    let beacon_port = std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let a = Node::start(node_cfg(
        running.addr,
        pin,
        token,
        root.join("a"),
        "alpha",
        false,
        beacon_port,
    ))
    .await
    .unwrap();
    let b = Node::start(node_cfg(
        running.addr,
        pin,
        token,
        root.join("b"),
        "beta",
        true,
        beacon_port,
    ))
    .await
    .unwrap();

    let saw_online = wait_until(Duration::from_secs(3), || {
        a.devices().iter().any(|d| d.short_id == b.short_id && d.online && !d.revoked)
            && b.devices().iter().any(|d| d.short_id == a.short_id && d.online)
    })
    .await;
    assert!(saw_online, "双方应看到对方在线 a={:?} b={:?}", a.devices(), b.devices());

    let saw_lan = wait_until(Duration::from_secs(4), || {
        a.devices().iter().any(|d| d.short_id == b.short_id && d.lan)
            && b.devices().iter().any(|d| d.short_id == a.short_id && d.lan)
    })
    .await;
    assert!(saw_lan, "信标应把对方标成局域网 a={:?} b={:?}", a.devices(), b.devices());

    let fp = a.invite(&b.short_id, false).await.expect("attended invite");
    let host_fp = wait_until(Duration::from_secs(2), || b.session_fp().as_deref() == Some(fp.as_str())).await;
    assert!(host_fp, "两边会话指纹应相同 host={:?}", b.session_fp());
    assert_eq!(a.session_fp().as_deref(), Some(fp.as_str()));
    assert_eq!(fp.len(), 16);

    a.end_session();
    b.end_session();
    assert!(a.session_fp().is_none());

    b.set_heartbeat_enabled(false);
    let saw_off = wait_until(Duration::from_secs(2), || {
        a.devices().iter().any(|d| d.short_id == b.short_id && !d.online && !d.revoked)
    })
    .await;
    assert!(saw_off, "心跳停止后应离线 {:?}", a.devices());
    b.set_heartbeat_enabled(true);
    let saw_back = wait_until(Duration::from_secs(2), || {
        a.devices().iter().any(|d| d.short_id == b.short_id && d.online)
    })
    .await;
    assert!(saw_back, "心跳恢复后应在线 {:?}", a.devices());

    let attacker = Identity::generate().unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let forged = sign_grant(
        &attacker,
        &GrantClaims {
            host: b.device_id(),
            controller: a.device_id(),
            feature_bits: 0,
            not_before: now.saturating_sub(1),
            not_after: now + 60,
        },
    );
    b.plant_grant(a.device_id(), &forged).unwrap();
    let rejected = a.invite(&b.short_id, true).await;
    assert!(rejected.is_err(), "伪造票据不能进入会话");
    assert!(a.session_fp().is_none());
    let host_err = b.last_error().unwrap_or_default();
    assert!(
        host_err.contains("伪造"),
        "被控端应拒绝伪造票据: {host_err}"
    );

    b.grant_controller(a.device_id(), 3600).unwrap();
    let fp2 = a.invite(&b.short_id, true).await.expect("valid grant");
    let host_fp2 = wait_until(Duration::from_secs(2), || b.session_fp().as_deref() == Some(fp2.as_str())).await;
    assert!(host_fp2, "有效票据握手后两边指纹应相同 host={:?}", b.session_fp());
    a.end_session();
    b.end_session();

    // 撤掉授权之后，下一秒就不能再直接进。
    b.ungrant(&a.short_id).unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let after = a.invite(&b.short_id, true).await;
    let why = after.expect_err("撤掉授权后不能再无人值守进去");
    assert!(a.session_fp().is_none(), "失败后不该留下会话");
    assert!(why.contains("没有无人值守授权"), "应说明缺授权: {why}");

    a.revoke(&b.short_id).unwrap();
    let saw_revoked = wait_until(Duration::from_secs(2), || {
        a.devices().iter().any(|d| d.short_id == b.short_id && d.revoked)
    })
    .await;
    assert!(saw_revoked, "撤销后列表应显示已撤销 {:?}", a.devices());

    drop(b);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let again = Node::start(node_cfg(
        running.addr,
        pin,
        token,
        root.join("b"),
        "beta",
        true,
        beacon_port,
    ))
    .await;
    assert!(again.is_err(), "撤销后同一公钥不能再注册");
    let _ = Reason::Unauthorized;
}

#[tokio::test]
async fn revoking_controller_closes_existing_host_session() {
    let root = scratch("revoke-active");
    let running = serve(Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        data_dir: root.join("signal"),
        offline_after: Duration::from_secs(3),
        invite_timeout: Duration::from_secs(2),
        public_host: "127.0.0.1".into(),
        relay_port: jr_proto::RELAY_PORT,
        reflex_port: 0,
    })
    .await
    .unwrap();
    let token = running.fleet_token.unwrap();
    let pin = running.fingerprint;
    let beacon_port = std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let controller = Node::start(node_cfg(running.addr, pin, token, root.join("controller"), "controller", false, beacon_port)).await.unwrap();
    let host = Node::start(node_cfg(running.addr, pin, token, root.join("host"), "host", true, beacon_port)).await.unwrap();
    assert!(wait_until(Duration::from_secs(3), || controller.devices().iter().any(|d| d.short_id == host.short_id && d.online)).await);
    controller.invite(&host.short_id, false).await.unwrap();
    assert!(wait_until(Duration::from_secs(3), || host.session_fp().is_some()).await);
    host.revoke(&controller.short_id).unwrap();
    assert!(wait_until(Duration::from_secs(3), || host.session_fp().is_none()).await, "撤销后被控端仍保留现有会话");
}
