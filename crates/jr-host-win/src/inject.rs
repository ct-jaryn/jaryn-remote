//! Windows 上的键鼠注入。
//!
//! 只用 SendInput，也就是所有人都能用的公开接口。不改注册表、不装驱动、
//! 不碰任何游戏反作弊会拦的东西。反作弊拦住的时候是「这台机器不支持」，
//! 不是「被控故障」，不去想办法绕过。

use std::sync::{Mutex, OnceLock};

use jr_input::scancode_from_hid;
use jr_proto::InputEvent;

use windows::Win32::Foundation::{POINT, RECT};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSEINPUT, MOUSE_EVENT_FLAGS,
    MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
    MOUSEEVENTF_MOVE, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL,
};
use windows::Win32::UI::Input::Pointer::{
    InitializeTouchInjection, InjectTouchInput, POINTER_FLAG_CANCELED, POINTER_FLAG_DOWN,
    POINTER_FLAG_INCONTACT, POINTER_FLAG_INRANGE, POINTER_FLAG_UPDATE, POINTER_FLAG_UP,
    POINTER_INFO, POINTER_TOUCH_INFO, TOUCH_FEEDBACK_DEFAULT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetSystemMetrics, POINTER_INPUT_TYPE, SetCursorPos, SM_CXVIRTUALSCREEN,
    SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

/// 按下的键要记着，会话结束时统一弹起来，不然会卡在按下状态。
static HELD: Mutex<Vec<u16>> = Mutex::new(Vec::new());
/// 鼠标键同理。只发 down 不补 up，远端会一直认为键还按着。
static HELD_BUTTONS: Mutex<Vec<u8>> = Mutex::new(Vec::new());
/// 还按着的手指。触控走不可靠流，UP 丢了要由这一层补取消。
static HELD_TOUCH: Mutex<Vec<u8>> = Mutex::new(Vec::new());
/// 触控初始化一次就够，失败过也不要每次事件都重试。
static TOUCH_READY: OnceLock<bool> = OnceLock::new();

const TOUCH_MAX: u8 = 10;
const PT_TOUCH: u32 = 2;
const TOUCH_MASK_CONTACTAREA: u32 = 1;
const TOUCH_MASK_PRESSURE: u32 = 4;

/// 这台机器能不能注入触控。能力协商用这个值，别报一个用不了的能力。
pub fn touch_points() -> u8 {
    if *TOUCH_READY.get_or_init(|| unsafe {
        InitializeTouchInjection(u32::from(TOUCH_MAX), TOUCH_FEEDBACK_DEFAULT).is_ok()
    }) {
        TOUCH_MAX
    } else {
        0
    }
}

fn touch(event: &InputEvent) -> Result<(), String> {
    let Some((id, phase, x, y)) = event.touch_info() else {
        return Ok(());
    };
    if u32::from(id) >= u32::from(TOUCH_MAX) {
        return Ok(());
    }
    if touch_points() == 0 {
        return Err("这台机器不支持触控注入".into());
    }
    // InjectTouchInput 要的是屏幕像素，不是 SendInput 的 0..65535 比例。
    // 节点已经把这个显示器的原点加回来了，这里直接用全局坐标。
    let (sx, sy) = (x, y);
    let flags = match phase {
        InputEvent::TOUCH_DOWN => POINTER_FLAG_DOWN | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT,
        InputEvent::TOUCH_MOVE => POINTER_FLAG_UPDATE | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT,
        InputEvent::TOUCH_UP => POINTER_FLAG_UP,
        InputEvent::TOUCH_CANCEL => POINTER_FLAG_CANCELED,
        _ => return Ok(()),
    };
    let contact = RECT {
        left: sx - 2,
        top: sy - 2,
        right: sx + 2,
        bottom: sy + 2,
    };
    let info = POINTER_TOUCH_INFO {
        pointerInfo: POINTER_INFO {
            pointerType: POINTER_INPUT_TYPE(PT_TOUCH as i32),
            pointerId: u32::from(id),
            pointerFlags: flags,
            ptPixelLocation: POINT { x: sx, y: sy },
            ptPixelLocationRaw: POINT { x: sx, y: sy },
            ..Default::default()
        },
        touchFlags: 0,
        touchMask: TOUCH_MASK_CONTACTAREA | TOUCH_MASK_PRESSURE,
        rcContact: contact,
        rcContactRaw: contact,
        orientation: 0,
        pressure: if phase == InputEvent::TOUCH_UP { 0 } else { 32000 },
    };
    unsafe {
        InjectTouchInput(&[info]).map_err(|_| "触控注入被系统拒绝".to_string())?;
    }
    let mut held = HELD_TOUCH.lock().unwrap();
    match phase {
        InputEvent::TOUCH_DOWN => {
            if !held.contains(&id) {
                held.push(id);
            }
        }
        InputEvent::TOUCH_UP | InputEvent::TOUCH_CANCEL => held.retain(|v| *v != id),
        _ => {}
    }
    Ok(())
}

fn abs_coords(x: i32, y: i32) -> (i32, i32) {
    // SendInput 的绝对坐标是 0..65535 的虚拟桌面比例，不是像素。
    unsafe {
        let vx = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let vw = GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1);
        let vh = GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1);
        let sx = ((x - vx) as i64 * 65535 / (vw - 1).max(1) as i64).clamp(0, 65535) as i32;
        let sy = ((y - vy) as i64 * 65535 / (vh - 1).max(1) as i64).clamp(0, 65535) as i32;
        (sx, sy)
    }
}

fn send(inputs: &[INPUT]) -> bool {
    if inputs.is_empty() {
        return true;
    }
    unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) == inputs.len() as u32 }
}

fn mouse(dx: i32, dy: i32, data: u32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn key(scan: u16, up: bool, extended: bool) -> INPUT {
    let mut flags = KEYBD_EVENT_FLAGS(if up { KEYEVENTF_KEYUP.0 } else { 0 } | KEYEVENTF_SCANCODE.0);
    if extended {
        flags = KEYBD_EVENT_FLAGS(flags.0 | KEYEVENTF_EXTENDEDKEY.0);
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: Default::default(),
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// 把一条输入投到 Windows 上。返回 Ok 表示已投递，不代表对方程序一定收下。
pub fn apply(event: &InputEvent, cursor: (i32, i32)) -> Result<(), String> {
    match event.kind {
        InputEvent::MOVE_ABS => {
            if event.payload.len() < 8 {
                return Ok(());
            }
            let x = i32::from_le_bytes(event.payload[0..4].try_into().unwrap_or([0; 4]));
            let y = i32::from_le_bytes(event.payload[4..8].try_into().unwrap_or([0; 4]));
            let (sx, sy) = abs_coords(x, y);
            let ok = send(&[mouse(sx, sy, 0, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK)]);
            if !ok {
                return Err("注入被系统拒绝".into());
            }
            Ok(())
        }
        InputEvent::BUTTON => {
            let button = event.payload.first().copied().unwrap_or(0);
            let down = event.payload.get(1).copied().unwrap_or(0) != 0;
            let flag = match (button, down) {
                (1, true) => MOUSEEVENTF_RIGHTDOWN,
                (1, false) => MOUSEEVENTF_RIGHTUP,
                (2, true) => MOUSEEVENTF_MIDDLEDOWN,
                (2, false) => MOUSEEVENTF_MIDDLEUP,
                (_, true) => MOUSEEVENTF_LEFTDOWN,
                (_, false) => MOUSEEVENTF_LEFTUP,
            };
            // 点之前把光标挪到该在的位置，否则会点在上一次的地方。
            let (sx, sy) = abs_coords(cursor.0, cursor.1);
            let ok = send(&[
                mouse(sx, sy, 0, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK),
                mouse(0, 0, 0, flag),
            ]);
            if !ok {
                return Err("注入被系统拒绝".into());
            }
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
        InputEvent::WHEEL => {
            if event.payload.len() < 8 {
                return Ok(());
            }
            let dx = i32::from_le_bytes(event.payload[0..4].try_into().unwrap_or([0; 4]));
            let dy = i32::from_le_bytes(event.payload[4..8].try_into().unwrap_or([0; 4]));
            let mut events = Vec::new();
            // 一个滚轮事件只能有一个方向，横竖分开投。
            if dy != 0 {
                // 节点给的是内容滚动方向：正值向下。Windows 的 WHEEL 正值
                // 是滚轮向前（内容向上），所以这里要取反。
                let units = dy.saturating_mul(60).wrapping_neg();
                events.push(mouse(0, 0, units as u32, MOUSEEVENTF_WHEEL));
            }
            if dx != 0 {
                events.push(mouse(0, 0, dx.saturating_mul(60) as u32, MOUSEEVENTF_HWHEEL));
            }
            if !send(&events) {
                return Err("注入被系统拒绝".into());
            }
            Ok(())
        }
        InputEvent::KEY => {
            if event.payload.len() < 3 {
                return Ok(());
            }
            let usage = u16::from_le_bytes([event.payload[0], event.payload[1]]);
            let down = event.payload[2] != 0;
            let Some((scan, extended)) = scancode_from_hid(usage) else {
                // 表里没有的键就忽略，不断会话。
                return Ok(());
            };
            if !send(&[key(scan as u16, !down, extended)]) {
                return Err("注入被系统拒绝".into());
            }
            let mut held = HELD.lock().unwrap();
            if down {
                if !held.contains(&usage) {
                    held.push(usage);
                }
            } else {
                held.retain(|u| *u != usage);
            }
            Ok(())
        }
        InputEvent::TOUCH => touch(event),
        _ => Ok(()),
    }
}

/// 会话结束时把还按着的键弹起来。按键卡住算故障，这一步不能省。
pub fn release_all() {
    let held: Vec<u16> = std::mem::take(&mut *HELD.lock().unwrap());
    for usage in held {
        if let Some((scan, extended)) = scancode_from_hid(usage) {
            let _ = send(&[key(scan as u16, true, extended)]);
        }
    }
    for button in std::mem::take(&mut *HELD_BUTTONS.lock().unwrap()) {
        let flag = match button {
            1 => MOUSEEVENTF_RIGHTUP,
            2 => MOUSEEVENTF_MIDDLEUP,
            _ => MOUSEEVENTF_LEFTUP,
        };
        let _ = send(&[mouse(0, 0, 0, flag)]);
    }
    // 触控 UP 走不可靠流，可能已经丢了。会话结束时把还按着的手指取消掉。
    // 先把列表取出来再逐个注入：touch() 自己也会锁 HELD_TOUCH。
    let touches: Vec<u8> = {
        let mut held = HELD_TOUCH.lock().unwrap();
        std::mem::take(&mut *held)
    };
    for id in touches {
        let _ = touch(&InputEvent::touch(0, id, InputEvent::TOUCH_CANCEL, 0, 0));
    }
}

/// 光标现在在哪。滚轮这类相对事件要投在当前位置上。
pub fn cursor() -> Option<(i32, i32)> {
    let mut p = POINT::default();
    unsafe { GetCursorPos(&mut p).ok()? };
    Some((p.x, p.y))
}

/// 直接把光标挪过去。用于必须在某个坐标点上滚动的场合。
pub fn warp(x: i32, y: i32) -> bool {
    unsafe { SetCursorPos(x, y).is_ok() }
}
