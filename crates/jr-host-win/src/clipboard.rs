//! Windows 剪贴板：只做纯文本，用系统的 CF_UNICODETEXT。
//!
//! 打开剪贴板可能被别人占着，所以要有重试和上限，不能一失败就把整个会话卡住。

use std::ffi::c_void;

use windows::Win32::Foundation::{HANDLE, HGLOBAL, HWND};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::CF_UNICODETEXT;

/// 剪贴板最多读这么多字符，防止别人放了个几百兆的东西进来。
const MAX_CHARS: usize = 1 << 20;

fn open_retry() -> bool {
    for _ in 0..20 {
        unsafe {
            if OpenClipboard(Some(HWND::default())).is_ok() {
                return true;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    false
}

/// 读当前剪贴板的文本。不是文本就返回 None。
pub fn get_text() -> Option<String> {
    if !open_retry() {
        return None;
    }
    let text = unsafe {
        let handle = GetClipboardData(CF_UNICODETEXT.0 as u32).ok()?;
        let ptr = GlobalLock(HGLOBAL(handle.0)) as *const u16;
        if ptr.is_null() {
            None
        } else {
            let mut len = 0usize;
            while len < MAX_CHARS && *ptr.add(len) != 0 {
                len += 1;
            }
            let slice = std::slice::from_raw_parts(ptr, len);
            let out = String::from_utf16_lossy(slice);
            let _ = GlobalUnlock(HGLOBAL(handle.0));
            Some(out)
        }
    };
    unsafe {
        let _ = CloseClipboard();
    }
    text
}

/// 把文本写进剪贴板。写失败返回 Err，交给上层去报告，不重试。
pub fn set_text(text: &str) -> Result<(), String> {
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let bytes = wide.len() * 2;
    if !open_retry() {
        return Err("剪贴板被别的程序占着".into());
    }
    let ok = unsafe {
        let res = (|| -> Result<(), String> {
            EmptyClipboard().map_err(|e| e.to_string())?;
            let mem = GlobalAlloc(GMEM_MOVEABLE, bytes).map_err(|e| e.to_string())?;
            let dst = GlobalLock(HGLOBAL(mem.0)) as *mut u16;
            if dst.is_null() {
                return Err("剪贴板内存拿不到".into());
            }
            std::ptr::copy_nonoverlapping(wide.as_ptr(), dst, wide.len());
            let _ = GlobalUnlock(HGLOBAL(mem.0));
            SetClipboardData(CF_UNICODETEXT.0 as u32, Some(HANDLE(mem.0)))
                .map(|_| ())
                .map_err(|e| e.to_string())
        })();
        let _ = CloseClipboard();
        res
    };
    ok
}

/// 只给日志用：不打印内容，只打印长度。
pub fn describe(text: &str) -> String {
    format!("{} 字", text.chars().count())
}

#[allow(dead_code)]
fn _unused(_p: *mut c_void) {}
