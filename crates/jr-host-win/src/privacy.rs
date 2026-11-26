//! Windows 上的遮挡层。
//!
//! 真正的隐私屏要在驱动层插一块虚拟显示器，那是另一件事。这里做的是
//! 软件遮挡：一块置顶、无边、覆盖整块虚拟桌面的黑窗，上面写清楚现状。
//! 站在机器前的人看不到工作内容，但截图和录屏照样拿得到——所以节点必须
//! 把这个状态报成「降级」，不能说是完整隐私屏。

use std::sync::atomic::{AtomicIsize, Ordering};

use windows::core::w;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, DrawTextW, EndPaint, GetStockObject, SelectObject, SetBkMode, SetTextColor,
    BLACK_BRUSH, DEFAULT_GUI_FONT, DT_CENTER, DT_SINGLELINE, DT_VCENTER, DT_WORDBREAK, HBRUSH,
    PAINTSTRUCT, TRANSPARENT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect, GetMessageW,
    RegisterClassW, SetWindowPos, ShowWindow, TranslateMessage, CS_HREDRAW, CS_VREDRAW,
    CW_USEDEFAULT, HWND_TOPMOST, MSG, SWP_NOACTIVATE, SW_HIDE, SW_SHOW, WNDCLASSW, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE, WM_PAINT,
};

static COVER: AtomicIsize = AtomicIsize::new(0);

const CLASS_NAME: windows::core::PCWSTR = w!("JarynRemoteCover");

fn register_class() {
    unsafe {
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wnd_proc),
            hbrBackground: HBRUSH(GetStockObject(BLACK_BRUSH).0),
            lpszClassName: CLASS_NAME,
            ..Default::default()
        };
        RegisterClassW(&wc);
    }
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match msg {
        // WS_POPUP 没有标题栏，窗口标题不会自己显示；必须自己在客户区画字，
        // 否则站在机器前的人只看到一块黑屏，不知道发生了什么。
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);
            let width = rc.right - rc.left;
            let height = rc.bottom - rc.top;
            SetBkMode(hdc, TRANSPARENT);
            SetTextColor(hdc, COLORREF(0x00F2F2F2));
            let old = SelectObject(hdc, GetStockObject(DEFAULT_GUI_FONT));
            let mut title: Vec<u16> = "正在被远控".encode_utf16().collect();
            let mut title_rc = RECT {
                left: 40,
                top: height / 2 - 70,
                right: width - 40,
                bottom: height / 2 - 20,
            };
            DrawTextW(hdc, &mut title, &mut title_rc, DT_CENTER | DT_SINGLELINE | DT_VCENTER);
            let mut body: Vec<u16> = "屏幕已遮挡。这是软件遮挡，不是硬件隐私屏：截图和录屏不受影响。"
                .encode_utf16()
                .collect();
            let mut body_rc = RECT {
                left: 40,
                top: height / 2 - 10,
                right: width - 40,
                bottom: height / 2 + 80,
            };
            DrawTextW(hdc, &mut body, &mut body_rc, DT_CENTER | DT_WORDBREAK);
            SelectObject(hdc, old);
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        // 只处理关闭：别的一律交回系统，不去抢键鼠。
        windows::Win32::UI::WindowsAndMessaging::WM_SYSCOMMAND => LRESULT(0),
        _ => DefWindowProcW(hwnd, msg, w, l),
    }
}

/// 开或关遮挡层。开着的时候返回 true。
pub fn cover(on: bool) -> bool {
    if on {
        if COVER.load(Ordering::Relaxed) != 0 {
            return true;
        }
        register_class();
        unsafe {
            let win = CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
                CLASS_NAME,
                w!("正在被远控。画面已遮挡。（软件遮挡，不是硬件隐私屏：截图不受影响）"),
                WS_POPUP | WS_VISIBLE,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                400,
                200,
                None,
                None,
                None,
                None,
            );
            let Ok(win) = win else {
                return false;
            };
            // 盖住整块虚拟桌面，包括副屏。
            let (x, y, w, h) = virtual_desktop();
            let _ = SetWindowPos(win, Some(HWND_TOPMOST), x, y, w, h, SWP_NOACTIVATE);
            let _ = ShowWindow(win, SW_SHOW);
            COVER.store(win.0 as isize, Ordering::Relaxed);
        }
        true
    } else {
        let raw = COVER.swap(0, Ordering::Relaxed);
        if raw != 0 {
            unsafe {
                let _ = ShowWindow(HWND(raw as *mut _), SW_HIDE);
                let _ = DestroyWindow(HWND(raw as *mut _));
            }
        }
        true
    }
}

pub fn cover_on() -> bool {
    COVER.load(Ordering::Relaxed) != 0
}

fn virtual_desktop() -> (i32, i32, i32, i32) {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    };
    unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    }
}

/// 遮挡层是窗口，必须在有消息循环的线程上跑。节点在 Windows 上要起一个。
pub fn run_message_loop() {
    unsafe {
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
