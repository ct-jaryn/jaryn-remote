use std::path::PathBuf;
use std::time::Duration;

use jr_node::{Node, NodeConfig};
use jr_proto::{InputEvent, PLATFORM_ANDROID, PLATFORM_LINUX, PLATFORM_MACOS};
use jr_relay::serve as serve_relay;
use jr_signal::{serve, Config};

fn scratch(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "jrp-relay-{}-{}-{}",
        label,
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
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

fn node_cfg(signal: std::net::SocketAddr, pin: [u8; 32], token: [u8; 32], dir: PathBuf, name: &str, platform: u8, force_relay: bool) -> NodeConfig {
    NodeConfig {
        signal,
        pin,
        data_dir: dir,
        name: name.into(),
        platform,
        fleet_token: token,
        auto_accept: true,
        heartbeat: Duration::from_millis(80),
        beacon_port: 0,
        beacon_every: Duration::from_secs(60),
        force_relay,
        reflex_port: 0,
        advertise_override: None,
        extra_candidate: None,
        os_inject: false,
        os_capture: false,
        os_clipboard: false,
    }
}

#[tokio::test]
async fn clipboard_goes_through_relay_and_dies_when_relay_stops() {
    let root = scratch("wan");
    let relay = serve_relay(jr_relay::Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        idle: Duration::from_secs(30),
        bandwidth_cap_bps: 10_000_000,
    })
    .await
    .unwrap();
    let running = serve(Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        data_dir: root.join("signal"),
        offline_after: Duration::from_secs(3),
        invite_timeout: Duration::from_secs(2),
        public_host: "127.0.0.1".into(),
        relay_port: relay.addr.port(),
        reflex_port: 0,
    })
    .await
    .unwrap();
    let token = running.fleet_token.expect("token");
    let a = Node::start(node_cfg(running.addr, running.fingerprint, token, root.join("a"), "alpha", PLATFORM_MACOS, true)).await.unwrap();
    let b = Node::start(node_cfg(running.addr, running.fingerprint, token, root.join("b"), "beta", PLATFORM_MACOS, true)).await.unwrap();
    assert!(wait_until(Duration::from_secs(3), || a.devices().iter().any(|d| d.short_id == b.short_id && d.online)).await);
    a.invite(&b.short_id, false).await.expect("中继握手");
    assert!(b.indicating(), "被控端应显示正在被控");
    assert!(!a.direct_path(), "强制中继时控制器不应报成直连");
    assert!(!b.direct_path(), "强制中继时被控端不应报成直连");
    a.send_clipboard("via-relay").unwrap();
    assert!(wait_until(Duration::from_secs(3), || b.last_clipboard().as_deref() == Some("via-relay")).await, "剪贴板应经中继到达");
    drop(relay);
    assert!(
        wait_until(Duration::from_secs(8), || a.session_fp().is_none() && b.session_fp().is_none() && !b.indicating()).await,
        "中继断开后应在数秒内结束会话"
    );
}

#[tokio::test]
async fn unreachable_direct_candidates_fall_back_to_relay_path() {
    let root = scratch("fallback");
    let relay = serve_relay(jr_relay::Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        idle: Duration::from_secs(30),
        bandwidth_cap_bps: 10_000_000,
    })
    .await
    .unwrap();
    let running = serve(Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        data_dir: root.join("signal"),
        offline_after: Duration::from_secs(3),
        invite_timeout: Duration::from_secs(3),
        public_host: "127.0.0.1".into(),
        relay_port: relay.addr.port(),
        reflex_port: 0,
    })
    .await
    .unwrap();
    let token = running.fleet_token.expect("token");
    // 两台机器都只报一个路由不到的内网地址，直连候选必死；中继是唯一活路。
    let cfg = |dir: PathBuf, name: &str| NodeConfig {
        signal: running.addr,
        pin: running.fingerprint,
        data_dir: dir,
        name: name.into(),
        platform: PLATFORM_MACOS,
        fleet_token: token,
        auto_accept: true,
        heartbeat: Duration::from_millis(80),
        beacon_port: 0,
        beacon_every: Duration::from_secs(60),
        force_relay: false,
        reflex_port: 0,
        // 192.0.2.0/24 是给文档和测试留的，路由不到。
        advertise_override: Some(vec![[192, 0, 2, 1]]),
        extra_candidate: None,
        os_inject: false,
        os_capture: false,
        os_clipboard: false,
    };
    let a = Node::start(cfg(root.join("a"), "alpha")).await.unwrap();
    let b = Node::start(cfg(root.join("b"), "beta")).await.unwrap();
    assert!(wait_until(Duration::from_secs(3), || a.devices().iter().any(|d| d.short_id == b.short_id && d.online)).await);

    let fp = a.invite(&b.short_id, false).await.expect("直连不通时应回退中继");
    let both = wait_until(Duration::from_secs(3), || b.session_fp().as_deref() == Some(fp.as_str())).await;
    assert!(both, "被控端也应当握上手 host={:?}", b.session_fp());
    // 回退中继后 peer_addr 就是中继地址；路径必须同步标成中继，
    // 否则 direct_path() 和诊断事件 16 会误报直连。
    assert!(!a.direct_path(), "直连候选不可达，控制器应当落在中继上");
    assert!(!b.direct_path(), "直连候选不可达，被控端应当落在中继上");
    a.end_session();
    b.end_session();
}

#[tokio::test]
async fn second_display_click_is_mapped_without_posting() {
    let root = scratch("input");
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
    let token = running.fleet_token.expect("token");
    let a = Node::start(node_cfg(running.addr, running.fingerprint, token, root.join("a"), "alpha", PLATFORM_MACOS, false)).await.unwrap();
    let b = Node::start(node_cfg(running.addr, running.fingerprint, token, root.join("b"), "beta", PLATFORM_MACOS, false)).await.unwrap();
    b.set_display(1920, 0, 1920, 1080);
    assert!(wait_until(Duration::from_secs(3), || a.devices().iter().any(|d| d.short_id == b.short_id && d.online)).await);
    a.invite(&b.short_id, false).await.unwrap();
    a.send_input(InputEvent::move_abs(1, 10, 20)).unwrap();
    assert!(wait_until(Duration::from_secs(3), || b.last_point() == Some((1930, 20))).await, "第二屏原点应加上");
}

#[tokio::test]
async fn linux_is_terminal_only_and_android_rejects_unattended() {
    let root = scratch("plat");
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
    let token = running.fleet_token.expect("token");
    let desk = Node::start(node_cfg(running.addr, running.fingerprint, token, root.join("desk"), "desk", PLATFORM_MACOS, false)).await.unwrap();
    let linux = Node::start(node_cfg(running.addr, running.fingerprint, token, root.join("lin"), "lin", PLATFORM_LINUX, false)).await.unwrap();
    assert!(wait_until(Duration::from_secs(3), || {
        desk.devices().iter().any(|d| d.short_id == linux.short_id && d.status == "仅终端")
    }).await, "Linux 应显示仅终端");
    let android = Node::start(node_cfg(running.addr, running.fingerprint, token, root.join("and"), "and", PLATFORM_ANDROID, false)).await.unwrap();
    assert!(wait_until(Duration::from_secs(3), || desk.devices().iter().any(|d| d.short_id == android.short_id && d.online)).await);
    let err = desk.invite(&android.short_id, true).await.expect_err("安卓不能无人值守");
    assert!(err.contains("没有无人值守授权"), "{err}");
}
