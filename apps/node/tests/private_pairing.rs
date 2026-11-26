use std::path::PathBuf;
use std::time::Duration;

use jr_node::{Node, NodeConfig};
use jr_proto::PLATFORM_MACOS;
use jr_signal::{device_list, device_revoke, pair_approve, pair_list, pair_open, serve_private, Config};

fn scratch() -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "jrp-private-pair-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
    ));
    std::fs::create_dir_all(&path).unwrap();
    path
}

#[tokio::test]
async fn private_server_requires_local_approval_once() {
    let root = scratch();
    let server_data = root.join("signal");
    let server = serve_private(Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        data_dir: server_data.clone(),
        offline_after: Duration::from_secs(3),
        invite_timeout: Duration::from_secs(3),
        public_host: "127.0.0.1".into(),
        relay_port: jr_proto::RELAY_PORT,
        reflex_port: 0,
    }).await.unwrap();
    assert!(server.fleet_token.is_none());
    let config = NodeConfig {
        signal: server.addr,
        pin: server.fingerprint,
        data_dir: root.join("mac"),
        name: "my-mac".into(),
        platform: PLATFORM_MACOS,
        fleet_token: [0; 32],
        auto_accept: false,
        heartbeat: Duration::from_millis(100),
        beacon_port: 0,
        beacon_every: Duration::from_secs(1),
        force_relay: false,
        reflex_port: 0,
        advertise_override: None,
        extra_candidate: None,
        os_inject: false,
        os_capture: false,
        os_clipboard: false,
    };
    assert!(Node::start(config.clone()).await.err().unwrap().contains("未开放"));
    pair_open(&server_data, 600).unwrap();
    let pending = Node::start(config.clone()).await.err().unwrap();
    let rows = pair_list(&server_data).unwrap();
    assert_eq!(rows.len(), 1);
    let code = rows[0].split_whitespace().next().unwrap();
    assert!(pending.contains(code));
    assert!(pair_approve(&server_data, "0000000x").is_err());
    pair_approve(&server_data, code).unwrap();
    assert!(pair_approve(&server_data, code).is_err());
    let node = Node::start(config.clone()).await.unwrap();
    assert!(!node.short_id.is_empty());
    let other = NodeConfig { data_dir: root.join("other"), name: "other".into(), ..config };
    assert!(Node::start(other.clone()).await.err().unwrap().contains("未开放"));
    let devices = device_list(&server_data).unwrap();
    assert_eq!(devices.len(), 1);
    let id = devices[0].split_whitespace().next().unwrap();
    assert!(device_revoke(&server_data, id).unwrap().contains("已撤销"));
    assert!(Node::start(NodeConfig { data_dir: root.join("mac"), name: "my-mac".into(), ..other }).await.is_err());
    drop(node);
    drop(server);
    let _ = std::fs::remove_dir_all(root);
}
