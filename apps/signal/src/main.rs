use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use jr_proto::hex_encode;
use jr_signal::{device_list, device_revoke, pair_approve, pair_list, pair_open, serve, serve_private, Config};

fn main() {
    let mut data = PathBuf::from("./signal-data");
    let mut bind: SocketAddr = format!("0.0.0.0:{}", jr_proto::SIGNAL_PORT).parse().unwrap();
    let mut reflex_port = jr_proto::REFLEX_PORT;
    let mut public_host = "127.0.0.1".to_string();
    let mut private_mode = false;
    let mut admin: Option<(&str, String)> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--data" => data = PathBuf::from(args.next().expect("--data")),
            "--bind" => bind = args.next().expect("--bind").parse().expect("bind"),
            "--reflex-port" => reflex_port = args.next().expect("--reflex-port").parse().expect("port"),
            "--public-host" => public_host = args.next().expect("--public-host"),
            "--private" => private_mode = true,
            "--pair-open" => admin = Some(("open", args.next().expect("--pair-open 秒数"))),
            "--pair-list" => admin = Some(("list", String::new())),
            "--pair-approve" => admin = Some(("approve", args.next().expect("--pair-approve 核对码"))),
            "--device-list" => admin = Some(("devices", String::new())),
            "--device-revoke" => admin = Some(("revoke", args.next().expect("--device-revoke 设备ID"))),
            "--help" | "-h" => {
                eprintln!("jr-signal --private --data DIR --bind 0.0.0.0:47210 --public-host IPv4");
                eprintln!("管理员：--data DIR --pair-open 600 | --pair-list | --pair-approve 8位核对码 | --device-list | --device-revoke 设备ID");
                return;
            }
            other => {
                eprintln!("未知参数 {other}");
                std::process::exit(2);
            }
        }
    }
    if let Some((action, value)) = admin {
        let result = match action {
            "open" => value.parse::<u64>().map_err(|_| "秒数格式错误".to_string()).and_then(|seconds| pair_open(&data, seconds)).map(|()| "配对窗口已开放".to_string()),
            "list" => pair_list(&data).map(|rows| if rows.is_empty() { "没有待批准设备".into() } else { rows.join("\n") }),
            "approve" => pair_approve(&data, &value),
            "devices" => device_list(&data).map(|rows| if rows.is_empty() { "没有设备".into() } else { rows.join("\n") }),
            "revoke" => device_revoke(&data, &value),
            _ => unreachable!(),
        };
        match result {
            Ok(message) => println!("{message}"),
            Err(e) => { eprintln!("管理命令失败: {e}"); std::process::exit(1); }
        }
        return;
    }
    if public_host.parse::<std::net::Ipv4Addr>().is_err() {
        eprintln!("--public-host 需要可由节点访问的 IPv4 地址");
        std::process::exit(2);
    }
    let rt = tokio::runtime::Runtime::new().unwrap();
    let cfg = Config {
            bind,
            data_dir: data,
            offline_after: Duration::from_secs(15),
            invite_timeout: Duration::from_secs(60),
            public_host,
            relay_port: jr_proto::RELAY_PORT,
            reflex_port,
        };
    let running = rt
        .block_on(async { if private_mode { serve_private(cfg).await } else { serve(cfg).await } })
        .unwrap_or_else(|e| {
            eprintln!("信令启动失败: {e}");
            std::process::exit(1);
        });
    println!("cert-fingerprint: {}", hex_encode(&running.fingerprint));
    println!("listening: {}", running.addr);
    if let Some(token) = &running.fleet_token {
        println!("fleet-token: {}", hex_encode(token));
        println!("舰队令牌只打印这一次。丢了就轮换，已注册设备不会掉线。");
    } else if private_mode {
        println!("封闭配对模式：没有共享注册令牌。新设备需本机管理员批准。");
    } else {
        println!("舰队令牌已设置，不会再次打印。");
    }
    rt.block_on(async {
        tokio::signal::ctrl_c().await.ok();
    });
}
