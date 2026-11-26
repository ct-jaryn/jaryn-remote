use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use jr_crypto::Identity;
use jr_proto::hex_encode;
use socket2::{Domain, Protocol, Socket, Type};

pub fn save_enrollment(data_dir: &Path, token: &[u8; 32], pin: &[u8; 32]) -> Result<(), String> {
    std::fs::create_dir_all(data_dir).map_err(|e| e.to_string())?;
    let path = data_dir.join("enrollment");
    let mut body = Vec::with_capacity(64);
    body.extend_from_slice(token);
    body.extend_from_slice(pin);
    write_secret(&path, &body)
}

pub fn load_enrollment(data_dir: &Path) -> Option<([u8; 32], [u8; 32])> {
    let body = std::fs::read(data_dir.join("enrollment")).ok()?;
    if body.len() != 64 {
        return None;
    }
    let mut token = [0u8; 32];
    let mut pin = [0u8; 32];
    token.copy_from_slice(&body[..32]);
    pin.copy_from_slice(&body[32..]);
    Some((token, pin))
}

pub(super) fn write_secret(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
    }
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

pub(super) fn load_identity(dir: &Path) -> Result<Identity, String> {
    let path = dir.join("device.pkcs8");
    if path.exists() {
        let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
        return Identity::from_pkcs8(&bytes).map_err(|e| e.to_string());
    }
    let id = Identity::generate().map_err(|e| e.to_string())?;
    std::fs::write(&path, id.pkcs8()).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(id)
}

pub(super) fn advertise_ips(media_v6: bool) -> Vec<Vec<u8>> {
    let mut ips = vec![vec![127, 0, 0, 1]];
    if media_v6 {
        ips.push(Ipv6Addr::LOCALHOST.octets().to_vec());
    }
    if let Some(ip) = primary_ipv4() {
        let oct = ip.octets();
        if oct != [127, 0, 0, 1] {
            ips.push(oct.to_vec());
        }
    }
    if media_v6 {
        if let Some(ip) = primary_ipv6() {
            let oct = ip.octets();
            if oct != Ipv6Addr::LOCALHOST.octets() {
                ips.push(oct.to_vec());
            }
        }
    }
    ips
}

pub(super) fn primary_ipv4() -> Option<Ipv4Addr> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("192.0.2.1:9").ok()?;
    match sock.local_addr().ok()?.ip() {
        IpAddr::V4(ip) if !ip.is_unspecified() => Some(ip),
        _ => None,
    }
}

pub(super) fn primary_ipv6() -> Option<Ipv6Addr> {
    let sock = std::net::UdpSocket::bind("[::]:0").ok()?;
    sock.connect("[2001:4860:4860::8888]:53").ok()?;
    match sock.local_addr().ok()?.ip() {
        IpAddr::V6(ip)
            if !ip.is_unspecified()
                && !ip.is_loopback()
                && !ip.is_multicast()
                && (ip.segments()[0] & 0xffc0) != 0xfe80 =>
        {
            Some(ip)
        }
        _ => None,
    }
}

const MEDIA_SOCKET_BUF: usize = 2 * 1024 * 1024;

/// 媒体 socket 优先做成双栈；系统禁用 IPv6 时退回 IPv4，不能因此起不来。
/// 同时显式拉大收发缓冲：Windows 默认 UDP 接收缓冲只有 64 KiB，64 片并发窗口
/// 一次打满就会在内核侧整批丢包，导致大文件或高码率视频重传耗尽。
pub(super) fn bind_media_socket() -> std::io::Result<(std::net::UdpSocket, bool)> {
    match dual_stack_socket() {
        Ok(sock) => Ok((sock, true)),
        Err(_) => {
            let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
            let _ = sock.set_recv_buffer_size(MEDIA_SOCKET_BUF);
            let _ = sock.set_send_buffer_size(MEDIA_SOCKET_BUF);
            let addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0));
            sock.bind(&addr.into())?;
            // 后面要交给 tokio，阻塞 socket 会在 debug 构建直接触发断言。
            sock.set_nonblocking(true)?;
            Ok((sock.into(), false))
        }
    }
}

pub(super) fn dual_stack_socket() -> std::io::Result<std::net::UdpSocket> {
    let sock = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
    sock.set_only_v6(false)?;
    let _ = sock.set_recv_buffer_size(MEDIA_SOCKET_BUF);
    let _ = sock.set_send_buffer_size(MEDIA_SOCKET_BUF);
    let addr = SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, 0, 0, 0));
    sock.bind(&addr.into())?;
    sock.set_nonblocking(true)?;
    Ok(sock.into())
}

/// 双栈 socket 发 IPv4 目标必须用映射地址。状态里仍保留原来的 v4，比较和测试都不受影响。
/// 退回纯 IPv4 socket 时保持原地址，否则映射地址根本发不出去。
pub(super) fn media_dest(addr: SocketAddr, dual: bool) -> SocketAddr {
    match addr {
        SocketAddr::V4(v4) if dual => {
            SocketAddr::V6(SocketAddrV6::new(v4.ip().to_ipv6_mapped(), v4.port(), 0, 0))
        }
        v4 => v4,
    }
}

/// 收包时把映射地址还原成 v4，后面按普通 IPv4 处理。
pub(super) fn media_src(addr: SocketAddr) -> SocketAddr {
    match addr {
        SocketAddr::V6(v6) => match v6.ip().to_ipv4_mapped() {
            Some(v4) => SocketAddr::V4(SocketAddrV4::new(v4, v6.port())),
            None => SocketAddr::V6(v6),
        },
        v4 => v4,
    }
}

pub(super) fn ip_bytes(ip: IpAddr) -> Vec<u8> {
    match ip {
        IpAddr::V4(v4) => v4.octets().to_vec(),
        IpAddr::V6(v6) => v6.octets().to_vec(),
    }
}

pub(super) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub(super) fn write_grant_file(dir: &Path, controller: &[u8; 32], assertion: &[u8]) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let path = dir.join(hex_encode(controller));
    std::fs::write(&path, assertion).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}
