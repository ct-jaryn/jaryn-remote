use std::path::PathBuf;
use std::time::Duration;

use jr_node::{Node, NodeConfig};
use jr_proto::PLATFORM_MACOS;
use jr_signal::{serve, Config};

fn scratch(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "jrp-feat-{}-{}-{}",
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

/// 等一个具体的东西出现，中途顺便让闭包看一眼进度。
async fn wait_until_path(timeout: Duration, mut pred: impl FnMut() -> Option<PathBuf>) -> Option<PathBuf> {
    let start = tokio::time::Instant::now();
    while start.elapsed() < timeout {
        if let Some(v) = pred() {
            return Some(v);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    pred()
}

/// 收件箱目录。文件名里带进程号，不同测试不会互相看见。
fn inbox_of(node: &jr_node::NodeHandle) -> PathBuf {
    node.data_dir().join("inbox")
}

#[tokio::test]
async fn clipboard_file_privacy_and_terminal_after_invite() {
    let root = scratch("io");
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
    let pin = running.fingerprint;
    let beacon_port = std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let cfg = |dir: PathBuf, name: &str, auto: bool| NodeConfig {
        signal: running.addr,
        pin,
        data_dir: dir,
        name: name.into(),
        platform: PLATFORM_MACOS,
        fleet_token: token,
        auto_accept: auto,
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
    };
    let a = Node::start(cfg(root.join("a"), "alpha", false)).await.unwrap();
    let b = Node::start(cfg(root.join("b"), "beta", true)).await.unwrap();
    let online = wait_until(Duration::from_secs(3), || {
        a.devices().iter().any(|d| d.short_id == b.short_id && d.online)
    })
    .await;
    assert!(online, "对方应在线");
    a.invite(&b.short_id, false).await.expect("invite");
    let view_error = a.watch_and_open().await.expect_err("host without capture cannot be viewed");
    assert!(view_error.contains("被控端没有画面能力"), "{view_error}");

    a.send_clipboard("hello-jrp").unwrap();
    let got = wait_until(Duration::from_secs(3), || b.last_clipboard().as_deref() == Some("hello-jrp")).await;
    assert!(got, "剪贴板应到达 host={:?}", b.last_clipboard());

    a.send_file("note.txt", b"abc").unwrap();
    let filed = wait_until(Duration::from_secs(3), || b.last_file().is_some()).await;
    assert!(filed, "小文件应落到收件箱 err={:?}", b.last_error_text());
    let path = b.last_file().unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"abc");
    assert!(path.ends_with("note.txt"));

    // 比一个确认窗口大得多的文件：约 260 片，窗口只有 64 片。
    let big: Vec<u8> = (0..260_000u32).map(|i| (i % 253) as u8).collect();
    let big_name = format!("big-{}.bin", std::process::id());
    a.send_file(&big_name, &big).unwrap();
    let arrived = wait_until(Duration::from_secs(20), || {
        b.last_file()
            .map(|p| p.ends_with(&big_name))
            .unwrap_or(false)
    })
    .await;
    assert!(arrived, "大文件应传完 err={:?}", b.last_error_text());
    let big_path = b.last_file().unwrap();
    assert_eq!(std::fs::read(&big_path).unwrap(), big, "内容要一模一样");

    // 真的大的一个：8 MiB，约 8000 片，窗口只有 64 片。
    // 这条路要把流控、分片、重组都跑一遍。
    let huge: Vec<u8> = (0..8 * 1024 * 1024u32).map(|i| (i.wrapping_mul(31) % 251) as u8).collect();
    let huge_name = format!("huge-{}.bin", std::process::id());
    let id = a.send_file(&huge_name, &huge).unwrap();
    // 进度要能报出来，而且是单调往前走的。
    let mut saw_progress = false;
    let mut last_sent = 0u64;
    let huge_path = wait_until_path(Duration::from_secs(60), || {
        if let Some((got_id, sent, total)) = a.file_progress() {
            if got_id == id && total as usize == huge.len() {
                assert!(sent >= last_sent, "进度不能往回走 {sent} < {last_sent}");
                last_sent = sent;
                saw_progress = true;
            }
        }
        b.last_file().filter(|p| p.ends_with(&huge_name))
    })
    .await;
    let huge_path = huge_path.unwrap_or_else(|| panic!("8 MiB 文件应传完 err={:?}", b.last_error_text()));
    assert!(saw_progress, "传输过程中应当能看到进度");
    let got = std::fs::read(&huge_path).unwrap();
    assert_eq!(got.len(), huge.len(), "长度要一致");
    assert_eq!(got, huge, "8 MiB 内容要一模一样");

    // 传到一半取消：收件箱里不能留下半截文件。
    // 要等到真的发出去一部分再取消，不然取消的就是还没开始的东西，等于没测。
    let cut_name = format!("cut-{}.bin", std::process::id());
    let cut_id = a.send_file(&cut_name, &huge).unwrap();
    let mid = wait_until(Duration::from_secs(10), || {
        a.file_progress()
            .map(|(id, sent, total)| id == cut_id && sent > total / 5)
            .unwrap_or(false)
    })
    .await;
    assert!(mid, "取消前应当真的已经发出了一部分");
    a.cancel_file(cut_id).unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let partial = std::fs::read_dir(inbox_of(&b))
        .map(|d| {
            d.flatten()
                .any(|e| e.file_name().to_string_lossy().starts_with(&cut_name))
        })
        .unwrap_or(false);
    assert!(!partial, "取消之后不能有半截文件落在收件箱里");
    assert!(!b.last_file().map(|p| p.ends_with(&cut_name)).unwrap_or(false), "取消的文件不能算完成");

    a.request_privacy(true).unwrap();
    let degraded = wait_until(Duration::from_secs(3), || {
        a.last_error_text().unwrap_or_default().contains("遮挡")
    })
    .await;
    assert!(
        degraded,
        "macOS 只能软件遮挡，必须如实说明而不是声称完整隐私屏 err={:?}",
        a.last_error_text()
    );

    a.open_terminal().unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;
    #[cfg(unix)]
    {
        a.write_terminal(b"echo jrp-term\n").unwrap();
        let term = wait_until(Duration::from_secs(3), || a.terminal_text().contains("jrp-term")).await;
        assert!(term, "终端输出应回到控制端 text={}", a.terminal_text());

        // 窗口尺寸要真的落到对面的 shell 上，不是只发个消息。
        a.resize_terminal(100, 40).unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        a.write_terminal(b"stty size\n").unwrap();
        let sized = wait_until(Duration::from_secs(3), || a.terminal_text().contains("40 100")).await;
        assert!(sized, "对面 shell 应看到 40 行 100 列 text={}", a.terminal_text());
    }
    #[cfg(windows)]
    {
        a.write_terminal(b"echo jrp-term\r\n").unwrap();
        let term = wait_until(Duration::from_secs(5), || a.terminal_text().contains("jrp-term")).await;
        assert!(term, "ConPTY 输出应回到控制端 text={}", a.terminal_text());

        // 验证 ResizePseudoConsole 真正落到对面 Windows 终端尺寸。
        a.resize_terminal(100, 40).unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        a.write_terminal(
            b"powershell -NoProfile -Command \"$w=$Host.UI.RawUI.WindowSize; Write-Output ('sz='+$w.Height+'x'+$w.Width)\"\r\n",
        )
        .unwrap();
        let sized = wait_until(Duration::from_secs(6), || a.terminal_text().contains("sz=40x100")).await;
        assert!(sized, "对面 ConPTY 应看到 40 行 100 列 text={}", a.terminal_text());
    }

    let mut annex = vec![0, 0, 0, 1, 0x65, 0x88];
    annex.extend(std::iter::repeat(0x22).take(2000));
    b.submit_access_unit(true, &annex).unwrap();
    let video = wait_until(Duration::from_secs(2), || a.last_video().as_deref() == Some(annex.as_slice())).await;
    assert!(video, "访问单元应原样回到主控，这不是截图");
    assert!(!a.capture_running() && !b.capture_running(), "测试不得启动采集");
    a.start_stream().unwrap();
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(!b.capture_running(), "没有画面能力时不得采集");

    a.end_session();
    b.end_session();
}
