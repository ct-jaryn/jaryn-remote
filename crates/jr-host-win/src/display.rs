//! Windows 上的屏幕枚举。坐标用的是系统虚拟桌面坐标，和 SendInput 的绝对坐标同一套。

use windows::core::BOOL;
use windows::Win32::Foundation::{LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO,
};

use crate::Display;

unsafe extern "system" fn on_monitor(mon: HMONITOR, _hdc: HDC, _clip: *mut RECT, data: LPARAM) -> BOOL {
    let out = &mut *(data.0 as *mut Vec<Display>);
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if GetMonitorInfoW(mon, &mut info).as_bool() {
        let r = info.rcMonitor;
        // 主屏排在最前面，和别的平台一致：序号 0 是主屏。
        let primary = info.dwFlags & 1 != 0;
        let entry = Display {
            id: mon.0 as u32,
            origin_x: r.left,
            origin_y: r.top,
            width: (r.right - r.left).max(0) as u32,
            height: (r.bottom - r.top).max(0) as u32,
            scale_milli: scale_milli(mon),
        };
        if primary {
            out.insert(0, entry);
        } else {
            out.push(entry);
        }
    }
    BOOL(1)
}

/// 每毫米多少个千分之一像素。Windows 上按 DPI 换算，96 DPI 记 1000。
fn scale_milli(mon: HMONITOR) -> u32 {
    use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
    let mut x = 96u32;
    let mut y = 96u32;
    unsafe {
        if GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut x, &mut y).is_err() {
            return 1000;
        }
    }
    ((x as u64 * 1000 / 96).max(1000)) as u32
}

pub fn displays() -> Vec<Display> {
    let mut out: Vec<Display> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(on_monitor), LPARAM(&mut out as *mut _ as isize));
    }
    out
}
