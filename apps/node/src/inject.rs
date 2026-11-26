use std::sync::Mutex;

use jr_input::{map_event, map_rel, Display};
#[cfg(target_os = "macos")]
use jr_input::mac_keycode;
use jr_proto::InputEvent;
#[cfg(not(windows))]
use jr_proto::Reason;

#[cfg(target_os = "macos")]
static HELD: Mutex<Vec<u16>> = Mutex::new(Vec::new());
/// 按下的鼠标键也要记着，会话结束时补发抬起，不然远端会一直以为键还按着。
#[cfg(target_os = "macos")]
static HELD_BUTTONS: Mutex<Vec<u8>> = Mutex::new(Vec::new());
/// 远端光标最后一次落点。滚轮是相对事件，得投在当前位置上。
static LAST: Mutex<(i32, i32)> = Mutex::new((0, 0));
/// 最近一次移动的全局物理像素。按钮报文只有按键状态，不含坐标。
static LAST_ABS: Mutex<Option<(i32, i32)>> = Mutex::new(None);

pub fn trusted() -> bool {
    #[cfg(windows)]
    {
        // Windows 只走 SendInput，没有单独的授权开关。
        return true;
    }
    #[cfg(target_os = "macos")]
    {
        return unsafe { AXIsProcessTrusted() != 0 };
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        false
    }
}

/// 只在用户从命令行明确要求时弹出系统授权页。测试和普通心跳不调用。
pub fn request_permission() {
    #[cfg(target_os = "macos")]
    unsafe {
        let _ = CGRequestScreenCaptureAccess();
        // 录屏和辅助功能是两份独立授权，少问一个就会被用户当成“输入坏了”。
        jrp_request_accessibility();
    }
    println!("node: 请在系统设置里授予录屏和辅助功能。授予后需要重新开始会话");
}

pub fn apply(os_inject: bool, display: Display, scale_milli: u32, event: &InputEvent) -> Result<(i32, i32), String> {
    // 相对移动是相对上一次落点的增量。先换算成全局绝对坐标，
    // 后面按普通绝对移动处理；否则 map_event 会把它当成未知事件丢到左上角。
    let normalized;
    let event = if event.kind == InputEvent::MOVE_REL {
        if event.payload.len() < 8 {
            return Ok((0, 0));
        }
        let dx = i32::from_le_bytes(event.payload[0..4].try_into().unwrap_or([0; 4]));
        let dy = i32::from_le_bytes(event.payload[4..8].try_into().unwrap_or([0; 4]));
        let last = *LAST_ABS.lock().unwrap();
        let base = last.unwrap_or_else(|| {
            // 新会话第一条就是相对移动时，还没有绝对落点。从系统光标
            // 当前位置起步，不能从显示器左上角起步。
            super::av::cursor_state()
                .map(|(x, y, _)| {
                    // macOS 的光标位置是 point，map_rel 和显示信息用的是
                    // 物理像素；Retina 屏上不换算会把基准点缩小一半。
                    #[cfg(target_os = "macos")]
                    {
                        jr_input::points_to_physical((x, y), scale_milli)
                    }
                    #[cfg(not(target_os = "macos"))]
                    {
                        (x, y)
                    }
                })
                .unwrap_or((display.origin_x, display.origin_y))
        });
        let at = map_rel(display, base, dx, dy);
        *LAST_ABS.lock().unwrap() = Some(at);
        normalized = InputEvent::move_abs(
            event.event_id,
            at.0.saturating_sub(display.origin_x),
            at.1.saturating_sub(display.origin_y),
        );
        &normalized
    } else {
        event
    };
    // 触控载荷里的 x/y 和绝对移动一样是显示器内坐标。Windows 的
    // InjectTouchInput 要屏幕像素，所以先补上这块显示器的原点。
    if event.kind == InputEvent::TOUCH {
        let Some((id, phase, x, y)) = event.touch_info() else {
            return Ok((0, 0));
        };
        let gx = x.saturating_add(display.origin_x);
        let gy = y.saturating_add(display.origin_y);
        if os_inject {
            #[cfg(windows)]
            {
                let global = InputEvent::touch(event.event_id, id, phase, gx, gy);
                jr_host_win::inject::apply(&global, (gx, gy))?;
            }
            #[cfg(not(windows))]
            {
                let _ = (id, phase);
            }
        }
        return Ok((gx, gy));
    }
    // 滚轮没有绝对坐标，投在光标最后落点上。
    if event.kind == InputEvent::WHEEL {
        if event.payload.len() < 8 {
            return Ok((0, 0));
        }
        let at = *LAST.lock().unwrap();
        if os_inject {
            #[cfg(windows)]
            jr_host_win::inject::apply(event, at)?;
            #[cfg(target_os = "macos")]
            {
                let dx = i32::from_le_bytes(event.payload[0..4].try_into().unwrap_or([0; 4]));
                let dy = i32::from_le_bytes(event.payload[4..8].try_into().unwrap_or([0; 4]));
                if trusted() {
                    post_scroll(at, dx, dy)?;
                }
            }
        }
        return Ok(at);
    }
    let mapped = if event.kind == InputEvent::BUTTON {
        LAST_ABS.lock().unwrap().ok_or("还没有鼠标位置")?
    } else {
        map_event(display, event).ok_or_else(|| "点击落在屏幕外".to_string())?
    };
    if event.kind == InputEvent::MOVE_ABS {
        *LAST_ABS.lock().unwrap() = Some(mapped);
    }
    if !os_inject {
        return Ok(mapped);
    }
    #[cfg(windows)]
    {
        let _ = scale_milli;
        // Windows 没有辅助功能授权这一层，装上来就能投。
        let point = mapped;
        if event.kind == InputEvent::MOVE_ABS || event.kind == InputEvent::BUTTON {
            *LAST.lock().unwrap() = point;
        }
        let res = jr_host_win::inject::apply(event, point);
        if let Err(e) = res {
            println!("node: 注入失败 {e}");
        }
        return Ok(mapped);
    }
    #[cfg(target_os = "macos")]
    {
        if !trusted() {
            return Err(Reason::PermissionMissing.user_text().into());
        }
        let point = to_points(mapped, scale_milli);
        if event.kind == InputEvent::MOVE_ABS || event.kind == InputEvent::BUTTON {
            *LAST.lock().unwrap() = point;
        }
        post(event, point)?;
        Ok(mapped)
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = (mapped, scale_milli);
        Err("这个系统没有键鼠注入适配器".into())
    }
}

#[cfg(target_os = "macos")]
fn to_points(point: (i32, i32), scale_milli: u32) -> (i32, i32) {
    let scale = scale_milli.max(1) as i64;
    ((point.0 as i64 * 1000 / scale) as i32, (point.1 as i64 * 1000 / scale) as i32)
}

pub fn release_os(os_inject: bool) {
    if !os_inject {
        return;
    }
    #[cfg(windows)]
    {
        jr_host_win::inject::release_all();
        *LAST.lock().unwrap() = (0, 0);
        *LAST_ABS.lock().unwrap() = None;
        return;
    }
    #[cfg(target_os = "macos")]
    {
        if !trusted() {
            return;
        }
        let held = std::mem::take(&mut *HELD.lock().unwrap());
        for code in held {
            let _ = post_key(code, false);
        }
        let at = *LAST.lock().unwrap();
        for button in std::mem::take(&mut *HELD_BUTTONS.lock().unwrap()) {
            let typ = match button {
                1 => 4,  // right up
                2 => 26, // other (middle) up
                _ => 2,  // left up
            };
            let _ = post_mouse(typ, at, u32::from(button));
        }
        *LAST.lock().unwrap() = (0, 0);
        *LAST_ABS.lock().unwrap() = None;
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        *LAST.lock().unwrap() = (0, 0);
        *LAST_ABS.lock().unwrap() = None;
    }
}

#[cfg(target_os = "macos")]
fn post_scroll(at: (i32, i32), dx: i32, dy: i32) -> Result<(), String> {
    if dx == 0 && dy == 0 {
        return Ok(());
    }
    unsafe {
        // 滚轮不会自己带位置，先把光标挪过去，再投滚动。
        post_mouse(5, at, 0)?;
        let source = CGEventSourceCreate(1);
        if source.is_null() {
            return Err(Reason::PermissionMissing.user_text().into());
        }
        let event = CGEventCreateScrollWheelEvent2(source, 1, 2, dy, dx, 0);
        CFRelease(source);
        if event.is_null() {
            return Err(Reason::PermissionMissing.user_text().into());
        }
        CGEventPost(0, event);
        CFRelease(event);
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn post(event: &InputEvent, mapped: (i32, i32)) -> Result<(), String> {
    match event.kind {
        InputEvent::MOVE_ABS => post_mouse(5, mapped, 0),
        InputEvent::BUTTON => {
            let button = event.payload.first().copied().unwrap_or(0);
            let down = event.payload.get(1).copied().unwrap_or(0) != 0;
            let typ = match button {
                1 => if down { 3 } else { 4 },   // 右键
                2 => if down { 25 } else { 26 }, // 中键
                _ => if down { 1 } else { 2 },   // 左键
            };
            post_mouse(typ, mapped, u32::from(button))?;
            let mut held = HELD_BUTTONS.lock().unwrap();
            if down {
                if !held.contains(&button) {
                    held.push(button);
                }
            } else {
                held.retain(|b| *b != button);
            }
            Ok(())
        }
        InputEvent::KEY => {
            if event.payload.len() < 3 {
                return Ok(());
            }
            let usage = u16::from_le_bytes([event.payload[0], event.payload[1]]);
            let Some(code) = mac_keycode(usage) else {
                return Ok(());
            };
            let down = event.payload[2] != 0;
            post_key(code, down)?;
            let mut held = HELD.lock().unwrap();
            if down {
                if !held.contains(&code) {
                    held.push(code);
                }
            } else {
                held.retain(|c| *c != code);
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

#[cfg(target_os = "macos")]
fn post_mouse(typ: u32, mapped: (i32, i32), button: u32) -> Result<(), String> {
    unsafe {
        let source = CGEventSourceCreate(1);
        if source.is_null() {
            return Err(Reason::PermissionMissing.user_text().into());
        }
        let event = CGEventCreateMouseEvent(
            source,
            typ,
            CGPoint { x: mapped.0 as f64, y: mapped.1 as f64 },
            button,
        );
        CFRelease(source);
        if event.is_null() {
            return Err(Reason::PermissionMissing.user_text().into());
        }
        CGEventPost(0, event);
        CFRelease(event);
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn post_key(code: u16, down: bool) -> Result<(), String> {
    unsafe {
        let source = CGEventSourceCreate(1);
        if source.is_null() {
            return Err(Reason::PermissionMissing.user_text().into());
        }
        let event = CGEventCreateKeyboardEvent(source, code, u8::from(down));
        CFRelease(source);
        if event.is_null() {
            return Err(Reason::PermissionMissing.user_text().into());
        }
        CGEventPost(0, event);
        CFRelease(event);
    }
    Ok(())
}

#[cfg(target_os = "macos")]
#[repr(C)]
struct CGPoint {
    x: f64,
    y: f64,
}

#[cfg(target_os = "macos")]
#[link(name = "CoreGraphics", kind = "framework")]
#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> u8;
    fn CGRequestScreenCaptureAccess() -> u8;
    fn CGEventSourceCreate(state: u32) -> *mut std::ffi::c_void;
    fn CGEventCreateMouseEvent(source: *mut std::ffi::c_void, typ: u32, point: CGPoint, button: u32) -> *mut std::ffi::c_void;
    fn CGEventCreateKeyboardEvent(source: *mut std::ffi::c_void, key: u16, down: u8) -> *mut std::ffi::c_void;
    fn CGEventCreateScrollWheelEvent2(source: *mut std::ffi::c_void, units: u32, wheel_count: u32, wheel1: i32, wheel2: i32, wheel3: i32) -> *mut std::ffi::c_void;
    fn CGEventPost(tap: u32, event: *mut std::ffi::c_void);
    fn CFRelease(item: *mut std::ffi::c_void);
}

#[cfg(target_os = "macos")]
extern "C" {
    /// av.m 里用 WithOptions 版本触发的辅助功能授权提示。
    fn jrp_request_accessibility();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn button_uses_last_move_on_secondary_display() {
        let display = Display { origin_x: 1920, origin_y: 0, width: 1920, height: 1080 };
        let button = InputEvent { event_id: 2, kind: InputEvent::BUTTON, payload: vec![0, 1] };
        *LAST_ABS.lock().unwrap() = None;
        assert!(apply(false, display, 1000, &button).is_err());
        let move_event = InputEvent::move_abs(1, 10, 20);
        assert_eq!(apply(false, display, 1000, &move_event).unwrap(), (1930, 20));
        assert_eq!(apply(false, display, 1000, &button).unwrap(), (1930, 20));
        *LAST_ABS.lock().unwrap() = None;
    }
}
