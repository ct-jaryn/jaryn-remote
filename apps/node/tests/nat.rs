use std::path::PathBuf;
use std::time::Duration;

use jr_node::{Node, NodeConfig};

mod natbox;
use jr_proto::PLATFORM_MACOS;
use jr_signal::{serve, Config};

fn scratch(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "jrp-nat-{}-{}-{}",
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

/// 两台机器各自的局域网地址都换成一个不可达的地址，模拟「直连候选打不通」。
/// 然后只能靠反射发现拿到的地址把路建起来。
#[tokio::test]
async fn direct_path_comes_up_from_the_reflexive_address_only() {
    let root = scratch("reflex");
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
    let cfg = |dir: PathBuf, name: &str| NodeConfig {
        signal: running.addr,
        pin: running.fingerprint,
        data_dir: dir,
        name: name.into(),
        platform: PLATFORM_MACOS,
        fleet_token: token,
        auto_accept: true,
        heartbeat: Duration::from_millis(80),
        // 关掉局域网信标，别让同机发现把这条路绕过去。
        beacon_port: 0,
        beacon_every: Duration::from_secs(60),
        force_relay: false,
        reflex_port: running.reflex_port,
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

    let learned = wait_until(Duration::from_secs(5), || a.reflexive_addr().is_some() && b.reflexive_addr().is_some()).await;
    assert!(
        learned,
        "两台机器都该从信令拿到自己的公网地址 a={:?} b={:?}",
        a.reflexive_addr(),
        b.reflexive_addr()
    );

    // 只带反射地址也应当能把会话建起来。不可达的那个候选必须被跳过。
    let fp = a.invite(&b.short_id, false).await.expect("应当靠反射地址建路");
    let both = wait_until(Duration::from_secs(3), || b.session_fp().as_deref() == Some(fp.as_str())).await;
    assert!(both, "两边指纹应一致 host={:?}", b.session_fp());
    assert!(a.direct_path(), "这条路应该落在直连上，而不是中继");
    a.end_session();
    b.end_session();
}

/// 打洞不是「试第一个候选」，是同时往每一个候选都发。
///
/// 这里在对端的候选里放一个诱饵：它在 127.0.0.2 上占着对端媒体端口，
/// 只有真的往每个候选都发过，诱饵才会收到包；同时会话仍然要建起来，
/// 说明没有卡在诱饵上。
#[tokio::test]
async fn probes_go_to_every_candidate_not_just_the_first() {
    let root = scratch("fanout");
    let running = serve(Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        data_dir: root.join("signal"),
        offline_after: Duration::from_secs(3),
        invite_timeout: Duration::from_secs(3),
        public_host: "127.0.0.1".into(),
        relay_port: jr_proto::RELAY_PORT,
        reflex_port: 0,
    })
    .await
    .unwrap();
    let token = running.fleet_token.expect("token");
    let decoy = tokio::net::UdpSocket::bind(("127.0.0.1", 0)).await.unwrap();
    let decoy_addr = decoy.local_addr().unwrap();
    let cfg = |dir: PathBuf, name: &str, extra: Option<std::net::SocketAddr>| NodeConfig {
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
        reflex_port: running.reflex_port,
        advertise_override: Some(vec![[127, 0, 0, 1]]),
        extra_candidate: extra,
        os_inject: false,
        os_capture: false,
        os_clipboard: false,
    };
    let a = Node::start(cfg(root.join("a"), "alpha", None)).await.unwrap();
    // 被控端多报一个候选：测试占着的诱饵端口。
    // 对面只有真的往每个候选都发，诱饵才会收到包。
    let b = Node::start(cfg(root.join("b"), "beta", Some(decoy_addr))).await.unwrap();
    assert!(wait_until(Duration::from_secs(3), || a.devices().iter().any(|d| d.short_id == b.short_id && d.online)).await);

    let fp = a.invite(&b.short_id, false).await.expect("会话应当建起来");
    let mut buf = [0u8; 1500];
    let got = tokio::time::timeout(Duration::from_secs(3), decoy.recv_from(&mut buf)).await;
    assert!(got.is_ok(), "对面应当也往第二个候选发了探测包");
    assert!(got.unwrap().unwrap().0 > 0);

    let ready = wait_until(Duration::from_secs(3), || b.session_fp().as_deref() == Some(fp.as_str())).await;
    assert!(ready, "会话不能被诱饵卡住 host={:?}", b.session_fp());
    assert!(a.direct_path(), "仍然要落在直连上");
    a.end_session();
    b.end_session();
}

/// 两边都在 NAT 后面时，只有同时往外发才能建路。
///
/// 这里把两台机器的媒体包都交给一个用户态 NAT：没往外发过的来源一律丢掉。
/// 如果实现只往一边发，会话会建不起来。
#[tokio::test]
async fn symmetric_nat_opens_only_when_both_sides_send() {
    let root = scratch("natbox");
    let running = serve(Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        data_dir: root.join("signal"),
        offline_after: Duration::from_secs(3),
        invite_timeout: Duration::from_secs(3),
        public_host: "127.0.0.1".into(),
        relay_port: jr_proto::RELAY_PORT,
        reflex_port: 0,
    })
    .await
    .unwrap();
    let token = running.fleet_token.expect("token");
    // 两边各自报一个「公网」地址。NAT 会按这个地址转发，本机并没有这两个地址。
    let pub_a: std::net::SocketAddr = "203.0.113.10:40000".parse().unwrap();
    let pub_b: std::net::SocketAddr = "203.0.113.20:40000".parse().unwrap();
    let cfg = |dir: PathBuf, name: &str, public: std::net::SocketAddr| NodeConfig {
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
        reflex_port: running.reflex_port,
        advertise_override: Some(vec![[0, 0, 0, 0]]),
        extra_candidate: Some(public),
        os_inject: false,
        os_capture: false,
        os_clipboard: false,
    };
    let a = Node::start(cfg(root.join("a"), "alpha", pub_a)).await.unwrap();
    let b = Node::start(cfg(root.join("b"), "beta", pub_b)).await.unwrap();
    let _pair = natbox::Pair::new(a.handle(), pub_a, b.handle(), pub_b);
    assert!(wait_until(Duration::from_secs(3), || a.devices().iter().any(|d| d.short_id == b.short_id && d.online)).await);

    let fp = match a.invite(&b.short_id, false).await {
        Ok(fp) => fp,
        Err(e) => {
            let log = _pair.log.lock().unwrap().clone();
            let mut counts = std::collections::HashMap::<String, usize>::new();
            for (from_a, verdict, dst) in &log {
                *counts
                    .entry(format!("{} -> {dst} {verdict:?}", if *from_a { "a" } else { "b" }))
                    .or_default() += 1;
            }
            let mut lines: Vec<_> = counts.into_iter().collect();
            lines.sort_by(|x, y| y.1.cmp(&x.1));
            panic!("握手失败 {e}，发包统计 {lines:?}");
        }
    };
    let ready = wait_until(Duration::from_secs(3), || b.session_fp().as_deref() == Some(fp.as_str())).await;
    assert!(ready, "被控端也应当握上手 host={:?}", b.session_fp());
    a.end_session();
    b.end_session();
}
