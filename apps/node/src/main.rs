use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use jr_node::{Node, NodeConfig, NodeHandle};
use jr_proto::{hex_decode, PLATFORM_ANDROID, PLATFORM_IOS, PLATFORM_LINUX, PLATFORM_MACOS, PLATFORM_WINDOWS};
use tokio::io::AsyncBufReadExt;

#[cfg(target_os = "macos")]
mod local_log;

#[cfg(target_os = "macos")]
const BUNDLE_SIGNAL: Option<&str> = option_env!("JARYN_BUNDLE_SIGNAL");
#[cfg(target_os = "macos")]
const BUNDLE_REFLEX_PORT: Option<&str> = option_env!("JARYN_BUNDLE_REFLEX_PORT");
#[cfg(target_os = "macos")]
const BUNDLE_SERVER_PIN: Option<&str> = option_env!("JARYN_BUNDLE_PIN");

fn main() {
    #[cfg(windows)]
    jr_host_win::prepare_process();
    let mut signal: SocketAddr = format!("127.0.0.1:{}", jr_proto::SIGNAL_PORT).parse().unwrap();
    let mut data = PathBuf::from("./node-data");
    let mut name = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "node".into());
    let mut token: Option<[u8; 32]> = None;
    let mut pin: Option<[u8; 32]> = None;
    let mut auto_accept = false;
    let mut platform = default_platform();
    let incoming: Vec<String> = std::env::args().skip(1).collect();
    #[cfg(target_os = "macos")]
    let bundled_app = launched_from_app_bundle();
    #[cfg(not(target_os = "macos"))]
    let bundled_app = false;
    let app_launch = incoming.is_empty() && bundled_app;
    #[cfg(target_os = "macos")]
    if app_launch {
        if let Some(sig) = BUNDLE_SIGNAL {
            signal = sig.parse().expect("JARYN_BUNDLE_SIGNAL");
        }
    }
    let mut open_ui = app_launch;
    let mut reflex_port = jr_proto::REFLEX_PORT;
    let mut os_clipboard = true;
    #[cfg(target_os = "macos")]
    let provided = if app_launch {
        vec!["--data".to_string(), app_data_dir().to_string_lossy().into_owned()]
    } else {
        incoming
    };
    #[cfg(not(target_os = "macos"))]
    let provided = incoming;
    let restore = provided.len() == 2 && matches!(provided[0].as_str(), "--data" | "--auto-start-from");
    let auto_start = provided.first().map(String::as_str) == Some("--auto-start-from");
    let parsed = if restore {
        let path = PathBuf::from(&provided[1]).join("launch.args");
        match std::fs::read_to_string(path) {
            Ok(text) if !text.is_empty() => {
                let mut saved: Vec<String> = text.lines().map(str::to_string).collect();
                if saved.first().is_some_and(|a| !a.starts_with("--")) {
                    saved.remove(0);
                }
                if let Some(i) = saved.iter().position(|a| a == "--auto-start-from") {
                    saved.drain(i..(i + 2).min(saved.len()));
                }
                if auto_start && !saved.iter().any(|a| a == "--ui") {
                    saved.push("--ui".into());
                }
                saved
            }
            _ => provided.clone(),
        }
    } else {
        provided.clone()
    };
    let mut args = parsed.into_iter();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--signal" => signal = args.next().expect("--signal").parse().expect("signal"),
            "--data" => data = PathBuf::from(args.next().expect("--data")),
            "--name" => name = args.next().expect("--name"),
            "--token" => token = Some(parse_32(&args.next().expect("--token"), "token")),
            "--pin" => pin = Some(parse_32(&args.next().expect("--pin"), "pin")),
            "--accept" => auto_accept = true,
            "--platform" => platform = parse_platform(&args.next().expect("--platform")),
            "--ui" => open_ui = true,
            "--reflex-port" => reflex_port = args.next().expect("--reflex-port").parse().expect("port"),
            // 收到对面剪贴板时不写进本机剪贴板。默认写。
            "--no-clipboard" => os_clipboard = false,
            // 自启项用这个参数回来。令牌和指纹从数据目录读，不再写在 plist 里。
            "--auto-start-from" => {
                data = PathBuf::from(args.next().expect("--auto-start-from"));
                open_ui = true;
            }
            "--help" | "-h" => {
                eprintln!(
                    "jr-node --data DIR --name NAME [--signal 127.0.0.1:47210] [--token HEX --pin HEX] [--accept] [--ui]"
                );
                eprintln!("第一次要带 --token 和 --pin，之后会存进数据目录，可以省略。");
                eprintln!("用法: jr-node --signal HOST:PORT --data DIR --name NAME [--ui] [--accept] [--no-clipboard]");
                eprintln!("命令: list | invite SHORT | watch | view | displays | display N | quality 1-4 | stats | ui | invite-unattended SHORT | grant SHORT | revoke SHORT | accept | reject | clip TEXT | copy | file PATH | progress | cancel ID | privacy | term | type TEXT | resize COLS ROWS | wake SHORT | wake-mac MAC | audit | autostart on|off | end | quit");
                return;
            }
            other => {
                eprintln!("未知参数 {other}");
                std::process::exit(2);
            }
        }
    }
    // 若打包时通过编译期环境变量内置了自托管封闭配对服务器配置，则固定使用该配置。
    #[cfg(target_os = "macos")]
    if bundled_app {
        if let (Some(sig), Some(server_pin)) = (BUNDLE_SIGNAL, BUNDLE_SERVER_PIN) {
            signal = sig.parse().expect("JARYN_BUNDLE_SIGNAL");
            if let Some(port_text) = BUNDLE_REFLEX_PORT {
                reflex_port = port_text.parse().expect("JARYN_BUNDLE_REFLEX_PORT");
            }
            token = Some([0; 32]);
            pin = Some(parse_32(server_pin, "JARYN_BUNDLE_PIN"));
        }
    }
    if !data.is_absolute() {
        data = std::env::current_dir().expect("当前目录不可用").join(data);
    }
    #[cfg(target_os = "macos")]
    if bundled_app {
        if let Err(e) = local_log::install(&data) {
            eprintln!("node: 本机日志初始化失败 {e}");
        }
    }
    let stored = jr_node::load_enrollment(&data);
    let (token, pin) = match (token, pin) {
        (Some(t), Some(p)) => (t, p),
        (None, None) => match stored {
            Some(pair) => pair,
            None => {
                #[cfg(target_os = "macos")]
                if app_launch {
                    let Some(setup) = jr_node::mac_setup(&signal.to_string(), &name) else {
                        return;
                    };
                    match setup.signal.parse::<SocketAddr>() {
                        Ok(addr) => signal = addr,
                        Err(_) => {
                            jr_node::mac_error(
                                "信令地址格式错误",
                                "请填写 IPv4:端口 或 [IPv6]:端口，例如 203.0.113.10:47210。",
                            );
                            std::process::exit(2);
                        }
                    }
                    name = setup.name;
                    (parse_32(&setup.token, "token"), parse_32(&setup.pin, "pin"))
                } else {
                    eprintln!("缺少 --token 和 --pin，数据目录里也没有存过。");
                    std::process::exit(2);
                }
                #[cfg(not(target_os = "macos"))]
                {
                    eprintln!("缺少 --token 和 --pin，数据目录里也没有存过。");
                    std::process::exit(2);
                }
            }
        },
        _ => {
            eprintln!("--token 和 --pin 要同时给，或者都不给（用数据目录里存过的）。");
            std::process::exit(2);
        }
    };
    if let Err(e) = jr_node::save_enrollment(&data, &token, &pin) {
        eprintln!("node: 存授权信息失败 {e}");
        std::process::exit(1);
    }
    let mut launch_args = vec![
        "--data".to_string(), data.to_string_lossy().into_owned(),
        "--signal".to_string(), signal.to_string(),
        "--name".to_string(), name.clone(),
        "--reflex-port".to_string(), reflex_port.to_string(),
    ];
    if auto_accept { launch_args.push("--accept".into()); }
    if !os_clipboard { launch_args.push("--no-clipboard".into()); }
    if platform != default_platform() {
        launch_args.extend(["--platform".into(), platform_name(platform).into()]);
    }
    if open_ui { launch_args.push("--ui".into()); }
    let cfg = NodeConfig {
        signal,
        pin,
        data_dir: data,
        name,
        platform,
        fleet_token: token,
        auto_accept,
        heartbeat: Duration::from_secs(5),
        beacon_port: jr_proto::BEACON_PORT,
        beacon_every: Duration::from_secs(2),
        force_relay: false,
        reflex_port,
        advertise_override: None,
        extra_candidate: None,
        os_inject: true,
        os_capture: true,
        os_clipboard,
    };
    let rt = tokio::runtime::Runtime::new().unwrap();
    let node = match rt.block_on(Node::start(cfg)) {
        Ok(node) => node,
        Err(e) => {
            #[cfg(target_os = "macos")]
            if app_launch {
                let title = if e.contains("配对") || e.contains("批准") { "等待设备批准" } else { "Jaryn Remote 启动失败" };
                jr_node::mac_error(title, &e);
            }
            eprintln!("node: {e}");
            std::process::exit(1);
        }
    };
    println!("node: ready short={}", node.short_id);
    if let Err(e) = node.remember_launch_args(&launch_args) {
        eprintln!("node: {e}");
    }
    if open_ui {
        node.open_ui();
    } else {
        println!("node: 命令行模式。输入 ui 可以打开设备窗口。");
    }
    let stop = Arc::new(AtomicBool::new(false));
    let worker = node.handle();
    let worker_stop = stop.clone();
    rt.spawn(async move {
        if cli_loop(worker).await {
            worker_stop.store(true, Ordering::Relaxed);
        }
    });
    // 主线程留着跑 AppKit。网络在 tokio 运行时的那几个线程上。
    while !stop.load(Ordering::Relaxed) {
        node.pump_ui();
        #[cfg(target_os = "macos")]
        if jr_node::mac_quit_requested() {
            node.end_session();
            break;
        }
        #[cfg(windows)]
        if jr_node::windows_quit_requested() {
            node.end_session();
            break;
        }
        std::thread::sleep(Duration::from_millis(8));
    }
    // 托盘退出和 stdin 的 quit/exit 都走这里；不统一收尾会在通知区域
    // 留下一个没有进程的图标，直到 Explorer 重启才消失。
    #[cfg(windows)]
    jr_host_win::desk::shutdown();
}

#[cfg(target_os = "macos")]
fn launched_from_app_bundle() -> bool {
    std::env::current_exe().ok().is_some_and(|exe| {
        exe.ancestors().any(|path| path.extension().is_some_and(|ext| ext == "app"))
    })
}

#[cfg(target_os = "macos")]
fn app_data_dir() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp"));
    let profile = BUNDLE_SIGNAL
        .and_then(|s| s.rsplit_once(':').map(|(h, _)| h))
        .map(|h| h.trim_matches(['[', ']']))
        .filter(|h| !h.is_empty())
        .unwrap_or("default");
    home.join("Library/Application Support/Jaryn Remote").join(profile)
}

async fn cli_loop(node: NodeHandle) -> bool {
    let stdin = tokio::io::stdin();
    let mut lines = tokio::io::BufReader::new(stdin).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split_whitespace();
        let cmd = parts.next().unwrap().to_string();
        let arg = parts.next().map(|s| s.to_string());
        if matches!(cmd.as_str(), "quit" | "exit") {
            return true;
        }
        if let Err(e) = handle_line(&node, &cmd, arg.as_deref(), &line).await {
            eprintln!("node: {e}");
        }
    }
    false
}

async fn handle_line(node: &NodeHandle, cmd: &str, arg: Option<&str>, line: &str) -> Result<(), String> {
    match cmd {
        "list" => {
            for d in node.devices() {
                if d.short_id == node.short_id {
                    continue;
                }
                let net = if d.lan { "局域网" } else { "广域网" };
                println!("{}  {}  {}  {}", d.short_id, d.name, d.status, net);
            }
            Ok(())
        }
        "invite" => {
            let short = arg.ok_or("用法: invite SHORT")?;
            let fp = node.invite(short, false).await?;
            println!("node: session-fp {fp}");
            Ok(())
        }
        "invite-unattended" => {
            let short = arg.ok_or("用法: invite-unattended SHORT")?;
            let fp = node.invite(short, true).await?;
            println!("node: session-fp {fp}");
            Ok(())
        }
        "grant" => {
            let short = arg.ok_or("用法: grant SHORT")?;
            let id = node
                .devices()
                .into_iter()
                .find(|d| d.short_id.eq_ignore_ascii_case(short))
                .ok_or("没有这台设备")?
                .device_id;
            node.grant_controller(id, 30 * 24 * 3600)
        }
        "revoke" => {
            let short = arg.ok_or("用法: revoke SHORT")?;
            node.revoke(short)
        }
        "ungrant" => {
            let short = arg.ok_or("用法: ungrant SHORT")?;
            node.ungrant(short)?;
            println!("node: 已撤掉 {short} 的无人值守授权");
            Ok(())
        }
        "accept" => node.accept_pending(),
        "reject" => node.reject_pending(),
        "permit" => {
            jr_node::request_permission();
            Ok(())
        }
        "quality" => {
            let n = arg.ok_or("用法: quality 1-4")?.parse::<u8>().map_err(|_| "画质是 1 到 4".to_string())?;
            node.set_quality(n)
        }
        "display" => {
            let n = arg.ok_or("用法: display N")?.parse::<u32>().map_err(|_| "显示器序号从 0 开始".to_string())?;
            node.switch_display(n)?;
            println!("node: 已请求切到第 {} 块屏", n + 1);
            Ok(())
        }
        "displays" => {
            let list = node.remote_displays();
            if list.is_empty() {
                println!("node: 对方还没报显示器");
            }
            for (i, (w, h)) in list.iter().enumerate() {
                println!("{}   {}x{}", i, w, h);
            }
            Ok(())
        }
        "stats" => {
            let rtt = match node.measured_rtt_ms() {
                Some(ms) => format!("{ms}ms"),
                None => "还没量到".into(),
            };
            println!(
                "node: 往返 {rtt} 帧 {} 音频 {} 反馈 {} 采集 {}",
                node.video_frames(),
                node.audio_frames(),
                node.feedback_count(),
                if node.capture_running() { "在跑" } else { "停着" }
            );
            Ok(())
        }
        "watch" => {
            node.start_stream()?;
            println!("node: watch requested");
            Ok(())
        }
        "view" => {
            node.request_view()?;
            println!("node: view opened");
            Ok(())
        }
        "ui" => {
            node.open_ui();
            println!("node: 设备窗口已打开");
            Ok(())
        }
        "autostart" => {
            let on = match arg {
                Some("on") => true,
                Some("off") => false,
                _ => return Err("用法: autostart on|off".into()),
            };
            node.set_autostart(on)?;
            println!("node: 开机自启 {}", if on { "已开" } else { "已关" });
            Ok(())
        }
        "end" => {
            // 和托盘一致：先给对面发 Bye，再收尾。end_session 只清本机，
            // 对端要等路静默才发现，期间还以为自己在被控制。进程仍然活着，
            // 主循环会把排队里的 Bye 发出去。
            node.hangup();
            Ok(())
        }
        "copy" => {
            let n = node.copy_local_clipboard()?;
            println!("node: 已把本机剪贴板 {n} 字节发给对面");
            Ok(())
        }
        "clip" => {
            let text = line.split_once(' ').map(|(_, t)| t).unwrap_or("").to_string();
            node.send_clipboard(&text)?;
            println!("node: clip queued bytes={}", text.len());
            Ok(())
        }
        "file" => {
            let path = arg.ok_or("用法: file PATH")?;
            let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
            let name = std::path::Path::new(path)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("file");
            let id = node.send_file(name, &bytes)?;
            println!("node: file queued id={id} name={name} bytes={}", bytes.len());
            Ok(())
        }
        "progress" => {
            match node.file_progress() {
                Some((id, sent, total)) => println!("node: 传输 {id} 已发 {sent}/{total}"),
                None => println!("node: 没有正在发的文件"),
            }
            Ok(())
        }
        "cancel" => {
            let id = arg.ok_or("用法: cancel ID")?.parse::<u32>().map_err(|_| "传输编号是整数".to_string())?;
            node.cancel_file(id)?;
            println!("node: 已取消 {id}");
            Ok(())
        }
        "privacy" => {
            node.request_privacy(true)?;
            println!("node: privacy requested");
            Ok(())
        }
        "term" => {
            node.open_terminal()?;
            println!("node: terminal requested");
            Ok(())
        }
        "type" => {
            let text = line.split_once(' ').map(|(_, t)| t).unwrap_or("");
            node.write_terminal(text.as_bytes())?;
            node.write_terminal(b"\n")?;
            Ok(())
        }
        "resize" => {
            let mut parts = line.split_whitespace().skip(1);
            let cols = parts.next().ok_or("用法: resize COLS ROWS")?.parse::<u16>().map_err(|_| "列数是整数".to_string())?;
            let rows = parts.next().ok_or("用法: resize COLS ROWS")?.parse::<u16>().map_err(|_| "行数是整数".to_string())?;
            node.resize_terminal(cols, rows)?;
            println!("node: 已让对面把终端改成 {cols}x{rows}");
            Ok(())
        }
        "wake" => {
            let short = arg.ok_or("用法: wake SHORT")?;
            node.request_wake(short)?;
            println!("node: wake requested");
            Ok(())
        }
        // 本机登记能被唤醒的 MAC。信令拿它代发魔包；不登记就永远回 wake-unavailable。
        "wake-mac" => {
            let text = arg.ok_or("用法: wake-mac AA:BB:CC:DD:EE:FF")?;
            let mac = parse_mac(text)?;
            node.allow_wake(mac)?;
            println!("node: wake mac registered");
            Ok(())
        }
        "audit" => node.request_audit(),
        other => Err(format!("未知命令 {other}")),
    }
}

fn parse_mac(text: &str) -> Result<[u8; 6], String> {
    let hex: String = text.chars().filter(|c| *c != ':' && *c != '-').collect();
    if hex.len() != 12 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("MAC 要写成 AA:BB:CC:DD:EE:FF".into());
    }
    let mut out = [0u8; 6];
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).map_err(|_| "MAC 里有非法字符".to_string())?;
    }
    if out == [0u8; 6] {
        return Err("全零 MAC 不能登记".into());
    }
    // 第一字节最低位是组播位：广播地址和组播地址不可能是单台机器的网卡。
    if out[0] & 1 != 0 {
        return Err("广播或组播 MAC 不能登记".into());
    }
    Ok(out)
}

fn parse_32(hex: &str, what: &str) -> [u8; 32] {
    let bytes = hex_decode(hex).unwrap_or_else(|_| {
        eprintln!("{what} 不是十六进制");
        std::process::exit(2);
    });
    if bytes.len() != 32 {
        eprintln!("{what} 必须是 32 字节");
        std::process::exit(2);
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    out
}

fn parse_platform(name: &str) -> u8 {
    match name {
        "macos" | "mac" => PLATFORM_MACOS,
        "windows" | "win" => PLATFORM_WINDOWS,
        "linux" => PLATFORM_LINUX,
        "android" => PLATFORM_ANDROID,
        "ios" => PLATFORM_IOS,
        other => {
            eprintln!("未知平台 {other}");
            std::process::exit(2);
        }
    }
}

fn platform_name(platform: u8) -> &'static str {
    match platform {
        PLATFORM_MACOS => "macos",
        PLATFORM_WINDOWS => "windows",
        PLATFORM_ANDROID => "android",
        PLATFORM_IOS => "ios",
        _ => "linux",
    }
}

fn default_platform() -> u8 {
    if cfg!(target_os = "macos") {
        PLATFORM_MACOS
    } else if cfg!(target_os = "windows") {
        PLATFORM_WINDOWS
    } else if cfg!(target_os = "android") {
        PLATFORM_ANDROID
    } else if cfg!(target_os = "ios") {
        PLATFORM_IOS
    } else {
        PLATFORM_LINUX
    }
}

#[cfg(test)]
mod tests {
    use super::parse_mac;

    #[test]
    fn mac_takes_colon_dash_and_bare_hex() {
        assert_eq!(parse_mac("AA:BB:CC:DD:EE:FF").unwrap(), [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]);
        assert_eq!(parse_mac("aa-bb-cc-dd-ee-ff").unwrap(), [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]);
        assert_eq!(parse_mac("aabbccddeeff").unwrap(), [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]);
    }

    #[test]
    fn bad_mac_is_rejected() {
        assert!(parse_mac("").is_err());
        assert!(parse_mac("AA:BB:CC:DD:EE").is_err());
        assert!(parse_mac("GG:BB:CC:DD:EE:FF").is_err());
        assert!(parse_mac("00:00:00:00:00:00").is_err());
        assert!(parse_mac("FF:FF:FF:FF:FF:FF").is_err());
        assert!(parse_mac("01:00:5E:00:00:01").is_err());
    }
}
