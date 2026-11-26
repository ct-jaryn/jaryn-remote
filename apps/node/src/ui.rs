#[cfg(target_os = "macos")]
use std::ffi::{CStr, CString};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

/// Linux / Android / iOS 还没有原生窗口，这些字段暂时只被 macOS 和 Windows 读。
#[cfg_attr(not(any(target_os = "macos", windows)), allow(dead_code))]
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Row {
    pub short_id: String,
    pub name: String,
    pub status: String,
    pub net: String,
    pub flags: u32,
}

pub const ROW_ONLINE: u32 = 1 << 0;
pub const ROW_LAN: u32 = 1 << 1;
pub const ROW_REVOKED: u32 = 1 << 2;
pub const ROW_CAN_INVITE: u32 = 1 << 3;
pub const ROW_CAN_GRANT: u32 = 1 << 4;
pub const ROW_CAN_REVOKE: u32 = 1 << 5;
pub const ROW_HAS_GRANT: u32 = 1 << 6;

pub const ACT_INVITE: u8 = 1;
pub const ACT_GRANT: u8 = 2;
pub const ACT_REVOKE: u8 = 3;
pub const ACT_AUTOSTART: u8 = 4;
pub const ACT_END: u8 = 5;
pub const ACT_RESCAN: u8 = 6;
/// 画质档。参数是 "1" 到 "4"。编号必须和 `jr_host_win::desk::ACT_QUALITY` 相同。
pub const ACT_QUALITY: u8 = 7;
pub const ACT_VIEW: u8 = 8;
pub const ACT_QUIT: u8 = 9;
#[cfg(target_os = "macos")]
pub const ACT_PERMIT: u8 = 10;
pub const ACT_LOGS: u8 = 11;
pub const ACT_UNGRANT: u8 = 12;
pub const ACT_WAKE: u8 = 13;
/// 托盘里的“恢复屏幕”：结束软件遮挡，不依赖会话窗口是否还能看见。
pub const ACT_PRIVACY_OFF: u8 = 14;

#[cfg(target_os = "macos")]
#[derive(Clone, Default, PartialEq, Eq)]
struct Overview {
    name: String,
    short_id: String,
    signal: String,
    connected: bool,
    session_active: bool,
    session_peer: String,
    controller: bool,
}

static QUIT_REQUESTED: AtomicBool = AtomicBool::new(false);

pub fn quit_requested() -> bool {
    QUIT_REQUESTED.load(Ordering::Relaxed)
}

pub fn request_quit() {
    QUIT_REQUESTED.store(true, Ordering::Relaxed);
}

#[cfg(target_os = "macos")]
pub struct Setup {
    pub signal: String,
    pub name: String,
    pub token: String,
    pub pin: String,
}

#[cfg(target_os = "macos")]
pub fn setup(default_signal: &str, default_name: &str) -> Option<Setup> {
    let signal = CString::new(default_signal).ok()?;
    let name = CString::new(default_name).ok()?;
    let mut signal_out = [0i8; 128];
    let mut name_out = [0i8; 128];
    let mut token_out = [0i8; 65];
    let mut pin_out = [0i8; 65];
    let ok = unsafe {
        ffi::jrp_ui_setup(
            signal.as_ptr(), name.as_ptr(),
            signal_out.as_mut_ptr(), signal_out.len(),
            name_out.as_mut_ptr(), name_out.len(),
            token_out.as_mut_ptr(), token_out.len(),
            pin_out.as_mut_ptr(), pin_out.len(),
        )
    };
    if ok != 1 {
        return None;
    }
    let read = |buf: &[i8]| unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().into_owned();
    Some(Setup {
        signal: read(&signal_out),
        name: read(&name_out),
        token: read(&token_out),
        pin: read(&pin_out),
    })
}

#[cfg(target_os = "macos")]
pub fn error(title: &str, message: &str) {
    if let (Ok(title), Ok(message)) = (CString::new(title), CString::new(message)) {
        unsafe { ffi::jrp_ui_error(title.as_ptr(), message.as_ptr()) };
    }
}

pub fn show_logs(dir: &std::path::Path) {
    #[cfg(target_os = "macos")]
    if let Ok(path) = CString::new(dir.join("logs").to_string_lossy().as_bytes()) {
        unsafe { ffi::jrp_ui_show_logs(path.as_ptr()) };
    }
    #[cfg(windows)]
    {
        // Windows 上没有 Finder，直接用资源管理器打开日志目录。
        let logs = dir.join("logs");
        let _ = std::fs::create_dir_all(&logs);
        let _ = std::process::Command::new("explorer").arg(&logs).spawn();
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    let _ = dir;
}

struct UiState {
    header: String,
    rows: Vec<Row>,
    autostart: bool,
    #[cfg(target_os = "macos")]
    can_view: bool,
    #[cfg(target_os = "macos")]
    overview: Option<Overview>,
    #[cfg(target_os = "macos")]
    overview_dirty: bool,
    dirty: bool,
    show: Option<bool>,
    action: Option<(u8, String)>,
    notice: Option<String>,
    #[cfg(windows)]
    quality: u8,
}

static UI: Mutex<UiState> = Mutex::new(UiState {
    header: String::new(),
    rows: Vec::new(),
    autostart: false,
    #[cfg(target_os = "macos")]
    can_view: false,
    #[cfg(target_os = "macos")]
    overview: None,
    #[cfg(target_os = "macos")]
    overview_dirty: false,
    dirty: false,
    show: None,
    action: None,
    notice: None,
    #[cfg(windows)]
    quality: 2,
});

/// 设备行按 短号\t名字\t状态\t网络\t标志 打包，行之间换行。字段里不含这些分隔符。
#[cfg_attr(not(any(target_os = "macos", windows)), allow(dead_code))]
fn pack(header: &str, rows: &[Row]) -> String {
    let mut out = String::with_capacity(rows.len() * 64);
    out.push_str(header);
    out.push('\n');
    for row in rows {
        let clean = |s: &str| s.replace(['\t', '\n'], " ");
        out.push_str(&clean(&row.short_id));
        out.push('\t');
        out.push_str(&clean(&row.name));
        out.push('\t');
        out.push_str(&clean(&row.status));
        out.push('\t');
        out.push_str(&clean(&row.net));
        out.push('\t');
        out.push_str(&row.flags.to_string());
        out.push('\n');
    }
    out
}

pub fn present(header: &str, rows: &[Row], autostart: bool, can_view: bool) {
    if let Ok(mut ui) = UI.lock() {
        // macOS 重建行会销毁按钮；仅在列表实际变化时更新，避免在鼠标按下和
        // 松开之间替换按钮，导致“邀请”等点击没有回调。
        if ui.header != header || ui.rows != rows || ui.autostart != autostart {
            ui.header = header.to_string();
            ui.rows = rows.to_vec();
            ui.autostart = autostart;
            ui.dirty = true;
        }
        #[cfg(target_os = "macos")]
        { ui.can_view = can_view; }
        #[cfg(not(target_os = "macos"))]
        let _ = can_view;
    }
}

#[cfg(target_os = "macos")]
pub fn set_overview(name: &str, short_id: &str, signal: &str, connected: bool,
    session_active: bool, session_peer: &str, controller: bool) {
    let next = Overview {
        name: name.into(), short_id: short_id.into(), signal: signal.into(),
        connected, session_active, session_peer: session_peer.into(), controller,
    };
    if let Ok(mut ui) = UI.lock() {
        if ui.overview.as_ref() != Some(&next) {
            ui.overview = Some(next);
            ui.overview_dirty = true;
        }
    }
}

pub fn set_show(on: bool) {
    if let Ok(mut ui) = UI.lock() {
        ui.show = Some(on);
    }
}

/// 界面底部的一行提示。带换行的内容会被压成一行。
pub fn notice(text: &str) {
    if let Ok(mut ui) = UI.lock() {
        ui.notice = Some(text.replace(['\n', '\t'], " "));
    }
}

pub fn take_action() -> Option<(u8, String)> {
    #[cfg(windows)]
    if let Some(action) = jr_host_win::desk::take_action() {
        return Some(action);
    }
    UI.lock().ok().and_then(|mut ui| ui.action.take())
}

/// 把当前画质档推到窗口。只在 Windows 设备窗口上用，macOS 的档在观看窗口里。
pub fn set_shown_quality(profile: u8) {
    #[cfg(windows)]
    if let Ok(mut ui) = UI.lock() {
        ui.quality = profile.clamp(1, 4);
    }
    #[cfg(not(windows))]
    let _ = profile;
}

/// 阻塞式确认框，只在主线程调。返回 true 表示同意。
pub fn ask(title: &str, body: &str) -> bool {
    #[cfg(target_os = "macos")]
    {
        let Ok(title) = CString::new(title) else {
            return false;
        };
        let Ok(body) = CString::new(body) else {
            return false;
        };
        unsafe { ffi::jrp_ui_ask(title.as_ptr(), body.as_ptr()) == 1 }
    }
    #[cfg(windows)]
    {
        jr_host_win::desk::ask(title, body)
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = (title, body);
        false
    }
}

pub fn pump() {
    #[cfg(target_os = "macos")]
    {
        let pending = {
            let Ok(mut ui) = UI.lock() else {
                return;
            };
            let wanted = if ui.dirty { Some(pack(&ui.header, &ui.rows)) } else { None };
            let auto = ui.autostart;
            let can_view = ui.can_view;
            let overview = if ui.overview_dirty { ui.overview.clone() } else { None };
            let show = ui.show.take();
            let note = ui.notice.take();
            if ui.dirty {
                ui.dirty = false;
            }
            ui.overview_dirty = false;
            (wanted, auto, can_view, overview, show, note)
        };
        let (packed, autostart, can_view, overview, show, note) = pending;
        unsafe { ffi::jrp_ui_set_can_view(i32::from(can_view)) };
        if let Some(info) = overview {
            if let (Ok(name), Ok(short), Ok(signal), Ok(peer)) = (
                CString::new(info.name), CString::new(info.short_id),
                CString::new(info.signal), CString::new(info.session_peer)) {
                unsafe { ffi::jrp_ui_set_overview(name.as_ptr(), short.as_ptr(), signal.as_ptr(),
                    i32::from(info.connected), i32::from(info.session_active), peer.as_ptr(),
                    i32::from(info.controller)) };
            }
        }
        if let Some(note) = note {
            if let Ok(text) = CString::new(note) {
                unsafe { ffi::jrp_ui_set_notice(text.as_ptr()) };
            }
        }
        if let Some(packed) = packed {
            if let Ok(text) = CString::new(packed) {
                unsafe {
                    ffi::jrp_ui_set_rows(text.as_ptr());
                    ffi::jrp_ui_set_autostart(i32::from(autostart));
                }
            }
        }
        if let Some(on) = show {
            unsafe { ffi::jrp_ui_show(i32::from(on)) };
        }
        unsafe { ffi::jrp_ui_pump() };
    }
    #[cfg(windows)]
    {
        let pending = UI.lock().ok().map(|mut ui| {
            let wanted = if ui.dirty { Some(pack(&ui.header, &ui.rows)) } else { None };
            let auto = ui.autostart;
            let show = ui.show.take();
            let note = ui.notice.take();
            let quality = ui.quality;
            ui.dirty = false;
            (wanted, auto, show, note, quality)
        });
        if let Some((packed, autostart, show, note, quality)) = pending {
            if let Some(note) = note {
                jr_host_win::desk::set_notice(&note);
            }
            if let Some(packed) = packed {
                jr_host_win::desk::set_rows(&packed);
            }
            jr_host_win::desk::set_autostart(autostart);
            jr_host_win::desk::set_quality(quality);
            if let Some(on) = show {
                jr_host_win::desk::show(on);
            }
        }
        // 没开设备窗口也要抽消息，遮挡层才收得到绘制。
        jr_host_win::desk::pump();
    }
}

#[cfg(target_os = "macos")]
mod ffi {
    use std::ffi::{c_char, c_void};

    unsafe extern "C" {
        pub fn jrp_ui_action_cb(cb: extern "C" fn(u8, *const c_char, *mut c_void), ctx: *mut c_void);
        pub fn jrp_ui_set_rows(packed: *const c_char);
        pub fn jrp_ui_set_notice(text: *const c_char);
        pub fn jrp_ui_set_autostart(on: i32);
        pub fn jrp_ui_set_can_view(on: i32);
        pub fn jrp_ui_show(on: i32);
        pub fn jrp_ui_pump();
        pub fn jrp_ui_ask(title: *const c_char, body: *const c_char) -> i32;
        pub fn jrp_ui_setup(default_signal: *const c_char, default_name: *const c_char,
            signal: *mut c_char, signal_cap: usize,
            name: *mut c_char, name_cap: usize,
            token: *mut c_char, token_cap: usize,
            pin: *mut c_char, pin_cap: usize) -> i32;
        pub fn jrp_ui_error(title: *const c_char, message: *const c_char);
        pub fn jrp_ui_show_logs(dir: *const c_char);
        pub fn jrp_ui_set_overview(name: *const c_char, short_id: *const c_char,
            signal: *const c_char, connected: i32, session_active: i32,
            session_peer: *const c_char, controller: i32);
    }

    extern "C" fn on_action(kind: u8, arg: *const c_char, _ctx: *mut c_void) {
        if kind == super::ACT_QUIT {
            super::QUIT_REQUESTED.store(true, super::Ordering::Relaxed);
            return;
        }
        let text = if arg.is_null() {
            String::new()
        } else {
            unsafe { std::ffi::CStr::from_ptr(arg) }.to_string_lossy().into_owned()
        };
        if let Ok(mut ui) = super::UI.lock() {
            ui.action = Some((kind, text));
        }
    }

    pub fn bind() {
        unsafe { jrp_ui_action_cb(on_action, std::ptr::null_mut()) }
    }
}

pub fn bind() {
    #[cfg(target_os = "macos")]
    ffi::bind();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packed_rows_match_the_windows_desk_parser() {
        let rows = [Row {
            short_id: "abcd".into(),
            name: "beta".into(),
            status: "在线".into(),
            net: "局域网".into(),
            flags: ROW_ONLINE | ROW_LAN | ROW_CAN_INVITE | ROW_CAN_GRANT | ROW_HAS_GRANT,
        }];
        let packed = pack("alpha  abcd  空闲", &rows);
        let (header, parsed) = jr_host_win::desk::parse_packed(&packed);
        assert_eq!(header, "alpha  abcd  空闲");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].short_id, "abcd");
        assert!(parsed[0].can_invite());
        assert!(parsed[0].can_grant());
        assert!(!parsed[0].can_revoke());
        assert!(parsed[0].has_grant());
        assert!(parsed[0].label().contains("已授权无人值守"));
        assert_eq!(jr_host_win::desk::ACT_INVITE, ACT_INVITE);
        assert_eq!(jr_host_win::desk::ACT_GRANT, ACT_GRANT);
        assert_eq!(jr_host_win::desk::ACT_REVOKE, ACT_REVOKE);
        assert_eq!(jr_host_win::desk::ACT_AUTOSTART, ACT_AUTOSTART);
        assert_eq!(jr_host_win::desk::ACT_END, ACT_END);
        assert_eq!(jr_host_win::desk::ACT_RESCAN, ACT_RESCAN);
        assert_eq!(jr_host_win::desk::ACT_QUALITY, ACT_QUALITY);
        assert_eq!(jr_host_win::desk::ACT_LOGS, ACT_LOGS);
        assert_eq!(jr_host_win::desk::ACT_UNGRANT, ACT_UNGRANT);
        assert_eq!(jr_host_win::desk::ACT_WAKE, ACT_WAKE);
        assert_eq!(jr_host_win::desk::ACT_PRIVACY_OFF, ACT_PRIVACY_OFF);
    }

    #[test]
    fn unchanged_rows_do_not_request_button_rebuild() {
        let rows = [Row {
            short_id: "ABCD1234".into(),
            name: "peer".into(),
            status: "在线".into(),
            net: "广域网".into(),
            flags: ROW_ONLINE | ROW_CAN_INVITE,
        }];
        present("local  空闲", &rows, false, false);
        UI.lock().unwrap().dirty = false;

        present("local  空闲", &rows, false, false);
        assert!(!UI.lock().unwrap().dirty);

        present("local  会话中", &rows, false, false);
        assert!(UI.lock().unwrap().dirty);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn unchanged_overview_does_not_request_redraw() {
        set_overview("alpha", "ABCD1234", "127.0.0.1:47210", false, false, "", false);
        UI.lock().unwrap().overview_dirty = false;
        set_overview("alpha", "ABCD1234", "127.0.0.1:47210", false, false, "", false);
        assert!(!UI.lock().unwrap().overview_dirty);
        set_overview("alpha", "ABCD1234", "127.0.0.1:47210", true, false, "", false);
        assert!(UI.lock().unwrap().overview_dirty);
    }
}
