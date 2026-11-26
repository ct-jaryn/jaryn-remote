//! Windows 设备窗口：列表、授权弹窗、画质档。
//!
//! 和 macOS 一样是系统窗口，不是网页壳。按钮编号必须和 `apps/node/src/ui.rs`
//! 的 `ACT_*` 一致，节点按这个数执行邀请、授权、撤销和切档。
//!
//! **窗口代码写完了，没有在任何一台 Windows 机器上打开过。**
//! 列表怎么拆、哪一行能邀请，不碰系统，在别的平台上有测试。

/// 和节点界面的动作编号是同一组数。改一边必须改另一边。
pub const ACT_INVITE: u8 = 1;
pub const ACT_GRANT: u8 = 2;
pub const ACT_REVOKE: u8 = 3;
pub const ACT_AUTOSTART: u8 = 4;
pub const ACT_END: u8 = 5;
pub const ACT_RESCAN: u8 = 6;
pub const ACT_QUALITY: u8 = 7;
pub const ACT_QUIT: u8 = 9;
pub const ACT_LOGS: u8 = 11;
pub const ACT_UNGRANT: u8 = 12;
pub const ACT_WAKE: u8 = 13;
pub const ACT_PRIVACY_OFF: u8 = 14;

/// 节点打包过来的一行。标志位和 `ui.rs` 的 `ROW_*` 对齐：
/// 邀请 1<<3，授权 1<<4，撤销 1<<5，已有授权 1<<6。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listed {
    pub short_id: String,
    pub name: String,
    pub status: String,
    pub net: String,
    pub flags: u32,
}

impl Listed {
    pub fn can_invite(&self) -> bool {
        self.flags & (1 << 3) != 0
    }

    pub fn can_grant(&self) -> bool {
        self.flags & (1 << 4) != 0
    }

    pub fn can_revoke(&self) -> bool {
        self.flags & (1 << 5) != 0
    }

    pub fn has_grant(&self) -> bool {
        self.flags & (1 << 6) != 0
    }

    pub fn label(&self) -> String {
        let mut out = format!("{}  {}  {}  {}", self.name, self.short_id, self.status, self.net);
        if self.has_grant() {
            out.push_str("  [已授权无人值守]");
        }
        out
    }
}

/// 第一行是标题，后面每行是 `短号\t名字\t状态\t网络\t标志`。
/// 字段不够或标志不是数字的行丢掉，不拿半行去点按钮。
pub fn parse_packed(packed: &str) -> (String, Vec<Listed>) {
    let mut lines = packed.split('\n');
    let header = lines.next().unwrap_or("").to_string();
    let mut rows = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 5 {
            continue;
        }
        let Ok(flags) = parts[4].parse::<u32>() else {
            continue;
        };
        rows.push(Listed {
            short_id: parts[0].to_string(),
            name: parts[1].to_string(),
            status: parts[2].to_string(),
            net: parts[3].to_string(),
            flags,
        });
    }
    (header, rows)
}

/// 四档的名字。和 macOS 观看窗口的下拉框同一套，不是文档里的「游戏 / 省流」。
/// 命令行 `quality 1-4` 也是这四档，两边不能各叫各的。
pub fn quality_label(profile: u8) -> &'static str {
    match profile {
        1 => "清晰",
        3 => "高帧率",
        4 => "省流量",
        _ => "均衡",
    }
}

pub const QUALITY_LABELS: [&str; 4] = ["清晰", "均衡", "高帧率", "省流量"];

#[cfg(windows)]
mod window {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, AtomicI32, AtomicIsize, AtomicU32, Ordering};
    use std::sync::Mutex;

    use windows::core::w;
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
    use windows::Win32::Graphics::Gdi::{GetStockObject, DEFAULT_GUI_FONT, HBRUSH, WHITE_BRUSH};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, SetFocus, VK_ESCAPE, VK_RETURN};
    use windows::Win32::UI::Shell::{
        Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        AppendMenuW, CreatePopupMenu, DestroyMenu, GetCursorPos, LoadIconW, PostMessageW,
        RegisterWindowMessageW, TrackPopupMenu, IDI_APPLICATION, MF_STRING, TPM_BOTTOMALIGN,
        TPM_LEFTALIGN, WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_NULL, WM_RBUTTONUP,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect, GetDlgItem,
        GetMessageW, GetWindowRect, MoveWindow, PeekMessageW, PostQuitMessage, RegisterClassW,
        SendMessageW, SetForegroundWindow, SetWindowPos, SetWindowTextW, ShowWindow, TranslateMessage,
        BS_AUTOCHECKBOX, BS_DEFPUSHBUTTON, BS_PUSHBUTTON, CBS_DROPDOWNLIST, CB_ADDSTRING, CB_GETCURSEL,
        CB_SETCURSEL, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, HMENU, HWND_TOPMOST, IsIconic, LBS_NOTIFY,
        LB_ADDSTRING, LB_GETCURSEL, LB_RESETCONTENT, LB_SETCURSEL, MINMAXINFO, MSG, PM_REMOVE, SWP_SHOWWINDOW,
        SW_HIDE, SW_RESTORE, SW_SHOW, WINDOW_EX_STYLE, WINDOW_STYLE, WM_CLOSE, WM_COMMAND, WM_DESTROY,
        WM_GETMINMAXINFO, WM_KEYDOWN,
        WM_SETFONT, WM_SIZE, WNDCLASSW, WS_BORDER, WS_CAPTION, WS_CHILD, WS_CLIPCHILDREN, WS_OVERLAPPEDWINDOW,
        WS_POPUP, WS_SYSMENU, WS_TABSTOP, WS_VISIBLE, WS_VSCROLL, WS_EX_DLGMODALFRAME, WS_EX_TOPMOST,
        BM_GETCHECK, BM_SETCHECK, BN_CLICKED, CBN_SELCHANGE, LBN_SELCHANGE,
    };

    use super::{Listed, ACT_AUTOSTART, ACT_END, ACT_GRANT, ACT_INVITE, ACT_LOGS, ACT_PRIVACY_OFF, ACT_QUALITY, ACT_QUIT, ACT_RESCAN, ACT_REVOKE, ACT_UNGRANT, ACT_WAKE, QUALITY_LABELS};

    const ID_REFRESH: i32 = 101;
    const ID_END: i32 = 102;
    const ID_AUTO: i32 = 103;
    const ID_INVITE: i32 = 104;
    const ID_GRANT: i32 = 105;
    const ID_REVOKE: i32 = 106;
    const ID_LIST: i32 = 107;
    const ID_QUALITY: i32 = 108;
    const ID_HEADER: i32 = 109;
    const ID_NOTICE: i32 = 110;
    const ID_QLABEL: i32 = 111;
    const ID_UNGRANT: i32 = 112;
    const ID_LOGS: i32 = 113;
    const ID_WAKE: i32 = 114;
    const ASK_ALLOW: u16 = 1;
    const ASK_DENY: u16 = 2;
    const TRAY_ID: u32 = 1;
    /// 托盘回调消息。避开 WM_APP 之后系统保留的范围，只在本窗口内用。
    const WM_TRAY: u32 = 0x8000 + 1;
    static TRAY_ADDED: AtomicBool = AtomicBool::new(false);
    /// Explorer 重启后系统广播的消息号；0 表示还没注册。
    static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);

    struct Pending {
        packed: Option<String>,
        notice: Option<String>,
        autostart: Option<bool>,
        quality: Option<u8>,
        show: Option<bool>,
    }

    impl Default for Pending {
        fn default() -> Self {
            Self {
                packed: None,
                notice: None,
                autostart: None,
                quality: None,
                show: None,
            }
        }
    }

    static PENDING: Mutex<Pending> = Mutex::new(Pending {
        packed: None,
        notice: None,
        autostart: None,
        quality: None,
        show: None,
    });
    static ROWS: Mutex<Vec<Listed>> = Mutex::new(Vec::new());
    static ACTIONS: Mutex<VecDeque<(u8, String)>> = Mutex::new(VecDeque::new());
    static LAST_PACKED: Mutex<String> = Mutex::new(String::new());
    static HWND_RAW: AtomicIsize = AtomicIsize::new(0);
    static CLASS_READY: AtomicBool = AtomicBool::new(false);
    static ASK_CLASS_READY: AtomicBool = AtomicBool::new(false);
    static CREATE_LOGGED: AtomicBool = AtomicBool::new(false);
    static IN_ASK: AtomicBool = AtomicBool::new(false);
    static ASK_ANSWER: AtomicI32 = AtomicI32::new(-1);

    pub fn set_rows(packed: &str) {
        if let Ok(mut p) = PENDING.lock() {
            p.packed = Some(packed.to_string());
        }
    }

    pub fn set_notice(text: &str) {
        if let Ok(mut p) = PENDING.lock() {
            p.notice = Some(text.replace(['\n', '\t', '\0'], " "));
        }
    }

    pub fn set_autostart(on: bool) {
        if let Ok(mut p) = PENDING.lock() {
            p.autostart = Some(on);
        }
    }

    pub fn set_quality(profile: u8) {
        if let Ok(mut p) = PENDING.lock() {
            p.quality = Some(profile);
        }
    }

    pub fn show(on: bool) {
        if let Ok(mut p) = PENDING.lock() {
            p.show = Some(on);
        }
    }

    pub fn take_action() -> Option<(u8, String)> {
        ACTIONS.lock().ok().and_then(|mut q| q.pop_front())
    }

    /// 主线程每拍调。没开窗口也要抽消息，遮挡层才能画出来。
    pub fn pump() {
        let snap = {
            let mut p = PENDING.lock().unwrap_or_else(|e| e.into_inner());
            std::mem::take(&mut *p)
        };
        if !apply(snap) {
            // 窗口没建起来，这次的显示请求留着下拍再试。
            if let Ok(mut p) = PENDING.lock() {
                if p.show.is_none() {
                    p.show = Some(true);
                }
            }
        }
        drain_messages();
    }

    /// 授权弹窗。只在界面线程调，会卡住这一拍直到人点完。
    /// 建不出来就当拒绝，绝不代答同意。回车、Esc、关掉窗口都是拒绝。
    pub fn ask(title: &str, body: &str) -> bool {
        if IN_ASK.swap(true, Ordering::AcqRel) {
            return false;
        }
        let _guard = AskGuard;
        let title = title.replace('\0', "");
        let body = format!(
            "{}\n\n点「允许」才会同意。回车、Esc 和关掉窗口都是拒绝。",
            body.replace('\0', "")
        );
        ASK_ANSWER.store(-1, Ordering::Relaxed);
        let parent = live_hwnd();
        let _lock = ParentLock::new(parent);
        register_ask_class();
        let (x, y) = dialog_origin(parent);
        let dialog = unsafe {
            CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_DLGMODALFRAME,
                w!("JarynRemoteAsk"),
                &hs(&title),
                WINDOW_STYLE(WS_POPUP.0 | WS_CAPTION.0 | WS_SYSMENU.0 | WS_VISIBLE.0),
                x,
                y,
                520,
                220,
                parent,
                None,
                instance(),
                None,
            )
        };
        let Ok(dialog) = dialog else {
            if !CREATE_LOGGED.swap(true, Ordering::Relaxed) {
                eprintln!("node: windows-ask 建不起来，这次邀请按拒绝处理");
            }
            return false;
        };
        let text = child(dialog, "STATIC", &body, style(0), 16, 16, 480, 110, 0);
        let allow = child(dialog, "BUTTON", "允许", style(WS_TABSTOP.0 | BS_PUSHBUTTON as u32), 300, 140, 88, 28, ASK_ALLOW as i32);
        let deny = child(
            dialog,
            "BUTTON",
            "拒绝",
            style(WS_TABSTOP.0 | BS_DEFPUSHBUTTON as u32),
            400,
            140,
            88,
            28,
            ASK_DENY as i32,
        );
        let _ = (text, allow);
        unsafe {
            let _ = SetWindowPos(dialog, Some(HWND_TOPMOST), x, y, 520, 220, SWP_SHOWWINDOW);
            let _ = SetForegroundWindow(dialog);
            if !deny.0.is_null() {
                let _ = SetFocus(Some(deny));
            }
            let mut msg = MSG::default();
            while ASK_ANSWER.load(Ordering::Relaxed) < 0 {
                let got = GetMessageW(&mut msg, None, 0, 0);
                if !got.as_bool() {
                    PostQuitMessage(0);
                    ASK_ANSWER.store(0, Ordering::Relaxed);
                    break;
                }
                if msg.message == WM_KEYDOWN {
                    let vk = msg.wParam.0 as u16;
                    // 回车不授权。允许必须是明确点那个按钮。
                    if vk == VK_RETURN.0 || vk == VK_ESCAPE.0 {
                        ASK_ANSWER.store(0, Ordering::Relaxed);
                        let _ = DestroyWindow(dialog);
                        break;
                    }
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        ASK_ANSWER.load(Ordering::Relaxed) == 1
    }

    struct AskGuard;
    impl Drop for AskGuard {
        fn drop(&mut self) {
            IN_ASK.store(false, Ordering::Release);
        }
    }

    struct ParentLock(Option<HWND>);
    impl Drop for ParentLock {
        fn drop(&mut self) {
            if let Some(hwnd) = self.0 {
                unsafe {
                    let _ = EnableWindow(hwnd, true);
                }
            }
        }
    }

    impl ParentLock {
        fn new(parent: Option<HWND>) -> Self {
            if let Some(hwnd) = parent {
                unsafe {
                    let _ = EnableWindow(hwnd, false);
                }
            }
            Self(parent)
        }
    }

    fn apply(mut snap: Pending) -> bool {
        if snap.show == Some(true) && live_hwnd().is_none() && !create_window() {
            restore(snap);
            return false;
        }
        let Some(hwnd) = live_hwnd() else {
            if snap.show != Some(false) {
                restore(snap);
            }
            return true;
        };
        if let Some(on) = snap.show.take() {
            unsafe {
                let _ = ShowWindow(hwnd, if on { SW_SHOW } else { SW_HIDE });
                if on {
                    let _ = SetForegroundWindow(hwnd);
                }
            }
        }
        if let Some(packed) = snap.packed.take() {
            apply_rows(hwnd, &packed);
        }
        if let Some(note) = snap.notice.take() {
            set_text(item(hwnd, ID_NOTICE), &note);
        }
        if let Some(on) = snap.autostart.take() {
            set_check(item(hwnd, ID_AUTO), on);
        }
        if let Some(profile) = snap.quality.take() {
            set_quality_sel(item(hwnd, ID_QUALITY), profile);
        }
        let _ = snap;
        true
    }

    fn restore(snap: Pending) {
        let Ok(mut p) = PENDING.lock() else {
            return;
        };
        if p.packed.is_none() {
            p.packed = snap.packed;
        }
        if p.notice.is_none() {
            p.notice = snap.notice;
        }
        if p.autostart.is_none() {
            p.autostart = snap.autostart;
        }
        if p.quality.is_none() {
            p.quality = snap.quality;
        }
        if p.show.is_none() {
            p.show = snap.show;
        }
    }

    fn tray_add(hwnd: HWND) {
        if TRAY_ADDED.swap(true, Ordering::AcqRel) {
            return;
        }
        let icon = unsafe { LoadIconW(None, IDI_APPLICATION) }.unwrap_or_default();
        let mut nid = NOTIFYICONDATAW::default();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = TRAY_ID;
        nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
        nid.uCallbackMessage = WM_TRAY;
        nid.hIcon = icon;
        let tip = "Jaryn Remote";
        for (i, unit) in tip.encode_utf16().take(127).enumerate() {
            nid.szTip[i] = unit;
        }
        if !unsafe { Shell_NotifyIconW(NIM_ADD, &nid) }.as_bool() {
            TRAY_ADDED.store(false, Ordering::Release);
        }
    }

    fn tray_remove(hwnd: HWND) {
        if !TRAY_ADDED.swap(false, Ordering::AcqRel) {
            return;
        }
        let mut nid = NOTIFYICONDATAW::default();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = TRAY_ID;
        let _ = unsafe { Shell_NotifyIconW(NIM_DELETE, &nid) };
    }

    fn show_main(hwnd: HWND) {
        unsafe {
            let cmd = if IsIconic(hwnd).as_bool() { SW_RESTORE } else { SW_SHOW };
            let _ = ShowWindow(hwnd, cmd);
            let _ = SetForegroundWindow(hwnd);
        }
    }

    /// 进程退出前显式收掉托盘和窗口，不能等 WM_DESTROY。
    pub fn shutdown() {
        if let Some(hwnd) = live_hwnd() {
            tray_remove(hwnd);
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            HWND_RAW.store(0, Ordering::Relaxed);
        }
    }

    fn tray_menu(hwnd: HWND) {
        let Ok(menu) = (unsafe { CreatePopupMenu() }) else {
            return;
        };
        unsafe {
            let _ = AppendMenuW(menu, MF_STRING, 1, w!("显示设备窗口"));
            let _ = AppendMenuW(menu, MF_STRING, 2, w!("刷新"));
            let _ = AppendMenuW(menu, MF_STRING, 3, w!("断开会话"));
            let _ = AppendMenuW(menu, MF_STRING, 5, w!("恢复屏幕"));
            let _ = AppendMenuW(menu, MF_STRING, 4, w!("退出"));
            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            let _ = SetForegroundWindow(hwnd);
            let _ = TrackPopupMenu(
                menu,
                TPM_LEFTALIGN | TPM_BOTTOMALIGN,
                pt.x,
                pt.y,
                Some(0),
                hwnd,
                None,
            );
            let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
            let _ = DestroyMenu(menu);
        }
    }

    fn on_tray(hwnd: HWND, l: LPARAM) {
        match l.0 as u32 & 0xffff {
            WM_LBUTTONUP | WM_LBUTTONDBLCLK => show_main(hwnd),
            WM_RBUTTONUP => tray_menu(hwnd),
            _ => {}
        }
    }

    fn create_window() -> bool {
        register_desk_class();
        // Explorer 重启会广播 TaskbarCreated。先记住消息号，窗口过程收到后
        // 重新 NIM_ADD，否则托盘图标会消失且没有入口恢复窗口。
        let tb = unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) };
        TASKBAR_CREATED.store(tb, Ordering::Relaxed);
        let created = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("JarynRemoteDesk"),
                w!("Jaryn Remote"),
                WINDOW_STYLE(WS_OVERLAPPEDWINDOW.0 | WS_CLIPCHILDREN.0),
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                720,
                520,
                None,
                None,
                instance(),
                None,
            )
        };
        let Ok(hwnd) = created else {
            if !CREATE_LOGGED.swap(true, Ordering::Relaxed) {
                eprintln!("node: windows-desk 窗口建不起来");
            }
            return false;
        };
        HWND_RAW.store(hwnd.0 as isize, Ordering::Relaxed);
        let _ = child(hwnd, "STATIC", "", style(0), 0, 0, 10, 10, ID_HEADER);
        let _ = child(hwnd, "BUTTON", "刷新", style(WS_TABSTOP.0 | BS_PUSHBUTTON as u32), 0, 0, 10, 10, ID_REFRESH);
        let _ = child(hwnd, "BUTTON", "断开", style(WS_TABSTOP.0 | BS_PUSHBUTTON as u32), 0, 0, 10, 10, ID_END);
        let _ = child(hwnd, "BUTTON", "开机自启", style(WS_TABSTOP.0 | BS_AUTOCHECKBOX as u32), 0, 0, 10, 10, ID_AUTO);
        let _ = child(hwnd, "STATIC", "画质", style(0), 0, 0, 10, 10, ID_QLABEL);
        let combo = child(
            hwnd,
            "COMBOBOX",
            "",
            style(WS_TABSTOP.0 | WS_VSCROLL.0 | CBS_DROPDOWNLIST as u32),
            0,
            0,
            10,
            10,
            ID_QUALITY,
        );
        let _ = child(
            hwnd,
            "LISTBOX",
            "",
            style(WS_TABSTOP.0 | WS_VSCROLL.0 | WS_BORDER.0 | LBS_NOTIFY as u32),
            0,
            0,
            10,
            10,
            ID_LIST,
        );
        let _ = child(hwnd, "BUTTON", "邀请", style(WS_TABSTOP.0 | BS_PUSHBUTTON as u32), 0, 0, 10, 10, ID_INVITE);
        let _ = child(hwnd, "BUTTON", "授权", style(WS_TABSTOP.0 | BS_PUSHBUTTON as u32), 0, 0, 10, 10, ID_GRANT);
        let _ = child(hwnd, "BUTTON", "撤回授权", style(WS_TABSTOP.0 | BS_PUSHBUTTON as u32), 0, 0, 10, 10, ID_UNGRANT);
        let _ = child(hwnd, "BUTTON", "撤销", style(WS_TABSTOP.0 | BS_PUSHBUTTON as u32), 0, 0, 10, 10, ID_REVOKE);
        let _ = child(hwnd, "BUTTON", "唤醒", style(WS_TABSTOP.0 | BS_PUSHBUTTON as u32), 0, 0, 10, 10, ID_WAKE);
        let _ = child(hwnd, "BUTTON", "日志", style(WS_TABSTOP.0 | BS_PUSHBUTTON as u32), 0, 0, 10, 10, ID_LOGS);
        let _ = child(hwnd, "STATIC", "", style(0), 0, 0, 10, 10, ID_NOTICE);
        for label in QUALITY_LABELS {
            let wide = wide_z(label);
            unsafe {
                let _ = SendMessageW(combo, CB_ADDSTRING, None, Some(LPARAM(wide.as_ptr() as isize)));
            }
        }
        unsafe {
            let _ = SendMessageW(combo, CB_SETCURSEL, Some(WPARAM(1)), None);
        }
        layout(hwnd);
        refresh_buttons(hwnd);
        tray_add(hwnd);
        true
    }

    fn apply_rows(hwnd: HWND, packed: &str) {
        if LAST_PACKED.lock().ok().as_deref().map(|s| s.as_str()) == Some(packed) {
            return;
        }
        let keep = selected_short(hwnd);
        let (header, rows) = super::parse_packed(packed);
        let labels: Vec<String> = rows.iter().map(Listed::label).collect();
        if let Ok(mut cur) = ROWS.lock() {
            *cur = rows;
        }
        if let Ok(mut last) = LAST_PACKED.lock() {
            *last = packed.to_string();
        }
        set_text(item(hwnd, ID_HEADER), &header);
        let list = item(hwnd, ID_LIST);
        unsafe {
            let _ = SendMessageW(list, LB_RESETCONTENT, None, None);
            for label in &labels {
                let wide = wide_z(label);
                let _ = SendMessageW(list, LB_ADDSTRING, None, Some(LPARAM(wide.as_ptr() as isize)));
            }
            if let Some(id) = keep {
                if let Ok(rows) = ROWS.lock() {
                    if let Some(i) = rows.iter().position(|r| r.short_id == id) {
                        let _ = SendMessageW(list, LB_SETCURSEL, Some(WPARAM(i)), None);
                    }
                }
            }
        }
        refresh_buttons(hwnd);
    }

    fn selected_short(hwnd: HWND) -> Option<String> {
        let index = unsafe { SendMessageW(item(hwnd, ID_LIST), LB_GETCURSEL, None, None).0 };
        if index < 0 {
            return None;
        }
        ROWS.lock().ok()?.get(index as usize).map(|r| r.short_id.clone())
    }

    fn selected_flags(hwnd: HWND) -> u32 {
        let index = unsafe { SendMessageW(item(hwnd, ID_LIST), LB_GETCURSEL, None, None).0 };
        if index < 0 {
            return 0;
        }
        ROWS.lock().ok().and_then(|rows| rows.get(index as usize).map(|r| r.flags)).unwrap_or(0)
    }

    fn refresh_buttons(hwnd: HWND) {
        let flags = selected_flags(hwnd);
        enable(hwnd, ID_INVITE, flags & (1 << 3) != 0);
        enable(hwnd, ID_GRANT, flags & (1 << 4) != 0);
        enable(hwnd, ID_REVOKE, flags & (1 << 5) != 0);
        // 没有已存授权时，撤回授权没有意义。
        enable(hwnd, ID_UNGRANT, flags & (1 << 6) != 0);
        // 只有离线设备才需要唤醒；在线设备直接邀请。
        enable(hwnd, ID_WAKE, flags != 0 && flags & 1 == 0 && flags & (1 << 2) == 0);
        enable(hwnd, ID_LOGS, true);
    }

    fn fire(kind: u8, arg: &str) {
        let Ok(mut q) = ACTIONS.lock() else {
            return;
        };
        if q.len() >= 64 {
            q.pop_front();
        }
        q.push_back((kind, arg.to_string()));
    }

    fn on_command(hwnd: HWND, w: WPARAM) {
        let id = (w.0 & 0xffff) as i32;
        let note = ((w.0 >> 16) & 0xffff) as u32;
        match (id, note) {
            (1, BN_CLICKED) => show_main(hwnd),
            (2, BN_CLICKED) => fire(ACT_RESCAN, ""),
            (3, BN_CLICKED) => fire(ACT_END, ""),
            (5, BN_CLICKED) => fire(ACT_PRIVACY_OFF, ""),
            (4, BN_CLICKED) => fire(ACT_QUIT, ""),
            (ID_REFRESH, BN_CLICKED) => fire(ACT_RESCAN, ""),
            (ID_END, BN_CLICKED) => fire(ACT_END, ""),
            (ID_AUTO, BN_CLICKED) => {
                let on = unsafe { SendMessageW(item(hwnd, ID_AUTO), BM_GETCHECK, None, None).0 == 1 };
                fire(ACT_AUTOSTART, if on { "1" } else { "0" });
            }
            (ID_INVITE, BN_CLICKED) => {
                if let Some(id) = selected_short(hwnd) {
                    fire(ACT_INVITE, &id);
                }
            }
            (ID_GRANT, BN_CLICKED) => {
                if let Some(id) = selected_short(hwnd) {
                    fire(ACT_GRANT, &id);
                }
            }
            (ID_UNGRANT, BN_CLICKED) => {
                if let Some(id) = selected_short(hwnd) {
                    fire(ACT_UNGRANT, &id);
                }
            }
            (ID_LOGS, BN_CLICKED) => fire(ACT_LOGS, ""),
            (ID_WAKE, BN_CLICKED) => {
                if let Some(id) = selected_short(hwnd) {
                    fire(ACT_WAKE, &id);
                }
            }
            (ID_REVOKE, BN_CLICKED) => {
                if let Some(id) = selected_short(hwnd) {
                    fire(ACT_REVOKE, &id);
                }
            }
            (ID_LIST, LBN_SELCHANGE) => refresh_buttons(hwnd),
            (ID_QUALITY, CBN_SELCHANGE) => {
                let index = unsafe { SendMessageW(item(hwnd, ID_QUALITY), CB_GETCURSEL, None, None).0 };
                if (0..4).contains(&index) {
                    fire(ACT_QUALITY, &(index + 1).to_string());
                }
            }
            _ => {}
        }
    }

    unsafe extern "system" fn desk_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
        let taskbar = TASKBAR_CREATED.load(Ordering::Relaxed);
        if taskbar != 0 && msg == taskbar {
            // Explorer 重启后旧图标已经不存在，重新添加一次。
            TRAY_ADDED.store(false, Ordering::Release);
            tray_add(hwnd);
            return LRESULT(0);
        }
        match msg {
            WM_COMMAND => {
                on_command(hwnd, w);
                LRESULT(0)
            }
            WM_TRAY => {
                on_tray(hwnd, l);
                LRESULT(0)
            }
            WM_SIZE => {
                layout(hwnd);
                LRESULT(0)
            }
            WM_GETMINMAXINFO => {
                let info = l.0 as *mut MINMAXINFO;
                if !info.is_null() {
                    // 动作行最右边到 508px，再窄就会裁掉唤醒/日志按钮。
                    (*info).ptMinTrackSize = POINT { x: 560, y: 420 };
                }
                LRESULT(0)
            }
            // 关掉窗口只是藏起来。邀请还要能弹，所以不在这里退出进程。
            WM_CLOSE => {
                let _ = ShowWindow(hwnd, SW_HIDE);
                LRESULT(0)
            }
            WM_DESTROY => {
                tray_remove(hwnd);
                if HWND_RAW.load(Ordering::Relaxed) == hwnd.0 as isize {
                    HWND_RAW.store(0, Ordering::Relaxed);
                }
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, w, l),
        }
    }

    unsafe extern "system" fn ask_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
        match msg {
            WM_COMMAND => {
                let id = (w.0 & 0xffff) as u16;
                if id == ASK_ALLOW || id == ASK_DENY {
                    ASK_ANSWER.store(if id == ASK_ALLOW { 1 } else { 0 }, Ordering::Relaxed);
                    let _ = DestroyWindow(hwnd);
                    return LRESULT(0);
                }
                DefWindowProcW(hwnd, msg, w, l)
            }
            WM_CLOSE => {
                ASK_ANSWER.store(0, Ordering::Relaxed);
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, w, l),
        }
    }

    fn layout(hwnd: HWND) {
        let mut rc = RECT::default();
        unsafe {
            if GetClientRect(hwnd, &mut rc).is_err() {
                return;
            }
        }
        let w = (rc.right - rc.left).max(1);
        let h = (rc.bottom - rc.top).max(1);
        let m = 12;
        let inner = (w - m * 2).max(40);
        move_to(item(hwnd, ID_HEADER), m, 8, inner, 22);
        move_to(item(hwnd, ID_REFRESH), m, 34, 72, 26);
        move_to(item(hwnd, ID_END), m + 80, 34, 72, 26);
        move_to(item(hwnd, ID_AUTO), m + 164, 34, 150, 26);
        move_to(item(hwnd, ID_QLABEL), m + 324, 38, 36, 20);
        // 组合框的高度是下拉列表的高度，不是收起后的高度。给矮了就点不开。
        move_to(item(hwnd, ID_QUALITY), m + 360, 34, 150, 180);
        let list_top = 70;
        let list_h = (h - list_top - 78).max(40);
        move_to(item(hwnd, ID_LIST), m, list_top, inner, list_h);
        let act_y = list_top + list_h + 8;
        move_to(item(hwnd, ID_INVITE), m, act_y, 80, 26);
        move_to(item(hwnd, ID_GRANT), m + 88, act_y, 80, 26);
        move_to(item(hwnd, ID_UNGRANT), m + 176, act_y, 88, 26);
        move_to(item(hwnd, ID_REVOKE), m + 272, act_y, 80, 26);
        move_to(item(hwnd, ID_WAKE), m + 360, act_y, 64, 26);
        move_to(item(hwnd, ID_LOGS), m + 432, act_y, 64, 26);
        move_to(item(hwnd, ID_NOTICE), m, h - 28, inner, 20);
    }

    fn drain_messages() {
        unsafe {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    fn register_desk_class() {
        if CLASS_READY.swap(true, Ordering::Relaxed) {
            return;
        }
        unsafe {
            let wc = WNDCLASSW {
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(desk_proc),
                hbrBackground: HBRUSH(GetStockObject(WHITE_BRUSH).0),
                lpszClassName: w!("JarynRemoteDesk"),
                hInstance: instance().unwrap_or_default(),
                ..Default::default()
            };
            RegisterClassW(&wc);
        }
    }

    fn register_ask_class() {
        if ASK_CLASS_READY.swap(true, Ordering::Relaxed) {
            return;
        }
        unsafe {
            let wc = WNDCLASSW {
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(ask_proc),
                hbrBackground: HBRUSH(GetStockObject(WHITE_BRUSH).0),
                lpszClassName: w!("JarynRemoteAsk"),
                hInstance: instance().unwrap_or_default(),
                ..Default::default()
            };
            RegisterClassW(&wc);
        }
    }

    fn child(parent: HWND, class_name: &str, title: &str, style: WINDOW_STYLE, x: i32, y: i32, w: i32, h: i32, id: i32) -> HWND {
        let class = wide_z(class_name);
        let text = wide_z(title);
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                windows::core::PCWSTR(class.as_ptr()),
                windows::core::PCWSTR(text.as_ptr()),
                style,
                x,
                y,
                w,
                h,
                Some(parent),
                Some(HMENU(id as isize as *mut _)),
                instance(),
                None,
            )
        };
        let Ok(hwnd) = hwnd else {
            return HWND::default();
        };
        set_font(hwnd);
        hwnd
    }

    fn item(parent: HWND, id: i32) -> HWND {
        unsafe { GetDlgItem(Some(parent), id).unwrap_or_default() }
    }

    fn set_text(hwnd: HWND, text: &str) {
        if hwnd.0.is_null() {
            return;
        }
        let text = hs(text);
        unsafe {
            let _ = SetWindowTextW(hwnd, &text);
        }
    }

    fn set_check(hwnd: HWND, on: bool) {
        if hwnd.0.is_null() {
            return;
        }
        let cur = unsafe { SendMessageW(hwnd, BM_GETCHECK, None, None).0 };
        let want = i32::from(on) as isize;
        if cur != want {
            unsafe {
                let _ = SendMessageW(hwnd, BM_SETCHECK, Some(WPARAM(want as usize)), None);
            }
        }
    }

    fn set_quality_sel(hwnd: HWND, profile: u8) {
        if hwnd.0.is_null() {
            return;
        }
        let want = (profile.clamp(1, 4) - 1) as isize;
        let cur = unsafe { SendMessageW(hwnd, CB_GETCURSEL, None, None).0 };
        if cur != want {
            unsafe {
                let _ = SendMessageW(hwnd, CB_SETCURSEL, Some(WPARAM(want as usize)), None);
            }
        }
    }

    fn enable(parent: HWND, id: i32, on: bool) {
        let hwnd = item(parent, id);
        if hwnd.0.is_null() {
            return;
        }
        unsafe {
            let _ = EnableWindow(hwnd, on);
        }
    }

    fn move_to(hwnd: HWND, x: i32, y: i32, w: i32, h: i32) {
        if hwnd.0.is_null() {
            return;
        }
        unsafe {
            let _ = MoveWindow(hwnd, x, y, w.max(1), h.max(1), true);
        }
    }

    fn set_font(hwnd: HWND) {
        unsafe {
            let font = GetStockObject(DEFAULT_GUI_FONT);
            let _ = SendMessageW(hwnd, WM_SETFONT, Some(WPARAM(font.0 as usize)), Some(LPARAM(1)));
        }
    }

    fn live_hwnd() -> Option<HWND> {
        let raw = HWND_RAW.load(Ordering::Relaxed);
        if raw == 0 {
            None
        } else {
            Some(HWND(raw as *mut _))
        }
    }

    fn instance() -> Option<windows::Win32::Foundation::HINSTANCE> {
        unsafe { GetModuleHandleW(None).ok().map(Into::into) }
    }

    fn dialog_origin(parent: Option<HWND>) -> (i32, i32) {
        let Some(parent) = parent else {
            return (160, 120);
        };
        let mut rc = RECT::default();
        unsafe {
            if GetWindowRect(parent, &mut rc).is_err() {
                return (160, 120);
            }
        }
        let x = rc.left + ((rc.right - rc.left) - 520) / 2;
        let y = rc.top + ((rc.bottom - rc.top) - 220) / 2;
        (x.max(0), y.max(0))
    }

    fn style(extra: u32) -> WINDOW_STYLE {
        WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | extra)
    }

    fn hs(text: &str) -> windows::core::HSTRING {
        text.replace('\0', "").into()
    }

    fn wide_z(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }
}

#[cfg(windows)]
pub use window::{ask, pump, set_autostart, set_notice, set_quality, set_rows, show, shutdown, take_action};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bad_lines_are_dropped_and_revoked_cannot_invite() {
        let packed = "本机  ab  空闲\nabc\tbeta\t已撤销\t广域网\t36\nshort\nnope\tx\ty\tz\tNaN\n";
        let (header, rows) = parse_packed(packed);
        assert_eq!(header, "本机  ab  空闲");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].short_id, "abc");
        assert!(!rows[0].can_invite());
        assert!(!rows[0].can_grant());
        assert!(rows[0].can_revoke());
    }

    #[test]
    fn quality_labels_are_the_same_four_the_cli_uses() {
        assert_eq!(quality_label(1), "清晰");
        assert_eq!(quality_label(2), "均衡");
        assert_eq!(quality_label(3), "高帧率");
        assert_eq!(quality_label(4), "省流量");
        assert_eq!(QUALITY_LABELS[1], "均衡");
    }
}
