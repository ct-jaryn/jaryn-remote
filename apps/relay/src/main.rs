use std::net::SocketAddr;
use std::time::Duration;

use jr_relay::{serve, Config};

fn main() {
    let mut bind: SocketAddr = format!("0.0.0.0:{}", jr_proto::RELAY_PORT).parse().unwrap();
    let mut cap = 20_000_000u64;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--bind" => bind = args.next().expect("--bind").parse().expect("bind"),
            "--cap-bps" => cap = args.next().expect("--cap-bps").parse().expect("cap"),
            "--help" | "-h" => {
                eprintln!("jr-relay --bind 0.0.0.0:47211 --cap-bps 20000000");
                return;
            }
            other => {
                eprintln!("未知参数 {other}");
                std::process::exit(2);
            }
        }
    }
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _running = rt.block_on(serve(Config {
        bind,
        idle: Duration::from_secs(30),
        bandwidth_cap_bps: cap,
    })).unwrap_or_else(|e| {
        eprintln!("relay: {e}");
        std::process::exit(1);
    });
    let _ = rt.block_on(std::future::pending::<()>());
}
