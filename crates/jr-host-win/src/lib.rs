//! Windows 宿主适配器。
//!
//! 这一层做这些事：把桌面画面编成 H.264、把键鼠投出去、读写剪贴板、
//! 在需要时盖一层遮挡，以及设备窗口（列表、授权弹窗、画质档）。
//! 协议、会话、传输都在别的 crate 里，这里不认识它们。
//!
//! 没有做成第三个「平台 if」塞进节点：适配器单独一个 crate，Windows 目标
//! 上能单独 `cargo check`，别的平台编译时整块不参与。
//!
//! 当前状态：能在 macOS 上对 `x86_64-pc-windows-msvc` 通过类型检查，
//! 但没有在任何一台 Windows 机器上跑过。谁在真机上第一次跑，谁就该把
//! 这里的状态改掉，而不是照抄 README。

use std::net::SocketAddr;

pub mod h264;
pub mod desk;
pub mod pointer;
pub mod view;

/// 进程级初始化。必须在任何窗口和显示器查询之前调用。
///
/// DXGI 采集给的是物理像素，而 DPI 不感知的进程拿到的 `GetCursorPos` /
/// `GetMonitorInfo` 是缩放后的逻辑坐标。两边单位不一致时，主控的鼠标位置
/// 会被映射到屏幕的左上角那一块。这里统一成物理像素。
pub fn prepare_process() {
    #[cfg(windows)]
    unsafe {
        let _ = windows::Win32::UI::WindowsAndMessaging::SetProcessDPIAware();
    }
}

/// 光标形状和位置在采集模块里维护，节点按需取最近一次的值。
#[cfg(windows)]
pub use capture::{cursor_shape, cursor_state};

#[cfg(windows)]
pub mod inject;
#[cfg(windows)]
pub mod clipboard;
#[cfg(windows)]
pub mod privacy;
#[cfg(windows)]
pub mod display;
#[cfg(windows)]
pub mod autostart;
#[cfg(windows)]
pub mod conpty;
#[cfg(windows)]
pub mod capture;
#[cfg(windows)] pub mod audio;

/// 采集出来的一段 H.264 访问单元，Annex-B 形式。
pub struct AccessUnit {
    pub idr: bool,
    pub bytes: Vec<u8>,
    pub ts_us: u64,
}

/// 一块屏。坐标是这块屏在桌面坐标系里的原点。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Display {
    pub id: u32,
    pub origin_x: i32,
    pub origin_y: i32,
    pub width: u32,
    pub height: u32,
    pub scale_milli: u32,
}

/// 主控窗口可以用本机绝对坐标，也可以让被控端自己换算。这里两样都给。
pub fn map_point(display: Display, x: i32, y: i32) -> Option<(i32, i32)> {
    if x < 0 || y < 0 || x as u32 >= display.width || y as u32 >= display.height {
        return None;
    }
    Some((display.origin_x + x, display.origin_y + y))
}

/// 非 Windows 平台上的占位实现：接口在，能力没有。
/// 这样节点里那句 cfg 之外就不用再写一遍错误处理。
#[cfg(not(windows))]
mod unsupported {
    use super::Display;

    pub fn screen_granted() -> bool {
        false
    }

    pub fn android_style_consent() -> bool {
        false
    }

    pub fn local_displays() -> Vec<Display> {
        Vec::new()
    }

    pub fn start_capture(_bitrate: i32, _display_index: i32, _sink: Box<dyn FnMut(super::AccessUnit) + Send>) -> Result<(), String> {
        Err("这个系统没有 Windows 采集适配器".into())
    }

    pub fn stop_capture() {}

    pub fn capture_running() -> bool {
        false
    }

    pub fn capture_failed() -> bool {
        false
    }

    pub fn hardware_encoder() -> Option<bool> {
        None
    }

    pub fn force_idr() {}

    pub fn set_bitrate(_bitrate: i32) {}

    pub fn set_fps(_fps: u16) {}

    pub fn privacy_cover(_on: bool) {}

    pub fn privacy_cover_on() -> bool {
        false
    }

    pub fn lookup_local_addr() -> Option<std::net::SocketAddr> {
        None
    }
}

#[cfg(not(windows))]
pub use unsupported::*;

/// 适配器自己的状态。节点问「能不能出画面」时不用猜。
pub fn ready() -> bool {
    screen_granted() && !local_displays().is_empty() && start_capture_supported()
}

#[cfg(not(windows))]
fn start_capture_supported() -> bool {
    false
}

/// Windows 没有「录屏授权」这一说，装上来就能采。
#[cfg(windows)]
pub fn screen_granted() -> bool {
    true
}

#[cfg(windows)]
fn start_capture_supported() -> bool {
    true
}

#[cfg(windows)]
static CAPTURE: std::sync::Mutex<Option<capture::Session>> = std::sync::Mutex::new(None);

#[cfg(windows)]
pub fn local_displays() -> Vec<Display> {
    display::displays()
}

#[cfg(windows)]
pub fn start_capture(
    bitrate: i32,
    display_index: i32,
    sink: Box<dyn FnMut(AccessUnit) + Send>,
) -> Result<(), String> {
    let session = capture::start(bitrate, display_index, sink)?;
    *CAPTURE.lock().unwrap() = Some(session);
    Ok(())
}

#[cfg(windows)]
pub fn stop_capture() {
    let taken = CAPTURE.lock().unwrap().take();
    if let Some(session) = taken {
        capture::stop(session);
    }
    // 启动失败或初始化超时时 CAPTURE 里没有 Session，但失败标记还在。
    // 不在这里清掉，下一次会话会一上来就被当成采集已失败。
    capture::clear_failed();
}

#[cfg(windows)]
pub fn capture_running() -> bool {
    capture::running()
}

#[cfg(windows)]
pub fn capture_failed() -> bool {
    capture::failed()
}

#[cfg(windows)]
pub fn hardware_encoder() -> Option<bool> {
    capture::hardware()
}

#[cfg(windows)]
pub fn force_idr() {
    capture::force_idr()
}

#[cfg(windows)]
pub fn set_bitrate(bitrate: i32) {
    capture::set_bitrate(bitrate)
}

#[cfg(windows)]
pub fn set_fps(fps: u16) {
    capture::set_fps(fps)
}

#[cfg(windows)]
pub fn privacy_cover(on: bool) {
    privacy::cover(on);
}

#[cfg(windows)]
pub fn privacy_cover_on() -> bool {
    privacy::cover_on()
}

#[cfg(windows)]
pub fn lookup_local_addr() -> Option<std::net::SocketAddr> {
    None
}

/// 给日志用的一句话状态，避免节点里到处写 cfg 判断。
pub fn status_line() -> String {
    if ready() {
        format!("采集就绪，{} 块屏", local_displays().len())
    } else if screen_granted() {
        "采集器没有可用屏幕".into()
    } else {
        "需授权".into()
    }
}

/// 只给测试和调试：把某个地址说成是本机地址，不碰到系统。
pub fn describe(addr: SocketAddr) -> String {
    format!("{addr}")
}
