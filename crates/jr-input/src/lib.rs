#![forbid(unsafe_code)]

use jr_proto::InputEvent;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Display {
    pub origin_x: i32,
    pub origin_y: i32,
    pub width: u32,
    pub height: u32,
}

/// 协议里的坐标是这块屏上的物理像素。换成全局坐标时加上原点。
/// 第二屏原点例如 x=1920 时，屏内 (10, 20) 必须落到 (1930, 20)。
pub fn map_abs(display: Display, x: i32, y: i32) -> Option<(i32, i32)> {
    if x < 0 || y < 0 || x >= display.width as i32 || y >= display.height as i32 {
        return None;
    }
    Some((display.origin_x.saturating_add(x), display.origin_y.saturating_add(y)))
}

/// macOS 的指针位置和窗口坐标都是 point，采集和协议用的是物理像素。
/// 先把全局 point 换成全局物理像素，再减掉这块屏的物理原点，得到屏内像素。
/// 例如 2x Retina 主屏上 point (100, 50) -> 物理 (200, 100)；副屏原点
/// (1920, 0) 时，屏内 point (10, 20) 必须得到 (20, 40)。
pub fn local_from_points(point: (i32, i32), display: Display, scale_milli: u32) -> (i32, i32) {
    let (gx, gy) = points_to_physical(point, scale_milli);
    (
        (gx as i64 - display.origin_x as i64).clamp(i32::MIN as i64, i32::MAX as i64) as i32,
        (gy as i64 - display.origin_y as i64).clamp(i32::MIN as i64, i32::MAX as i64) as i32,
    )
}

/// macOS 全局 point 换成全局物理像素。其他平台本来就是物理像素，
/// 不要调用这个函数。
pub fn points_to_physical(point: (i32, i32), scale_milli: u32) -> (i32, i32) {
    let scale = scale_milli.max(1) as i64;
    (
        (point.0 as i64 * scale / 1000).clamp(i32::MIN as i64, i32::MAX as i64) as i32,
        (point.1 as i64 * scale / 1000).clamp(i32::MIN as i64, i32::MAX as i64) as i32,
    )
}

/// 相对移动落在当前显示器内。基准点是上一次的全局物理坐标；
/// 越界就夹到这块屏的边缘，不能悄悄跑回 (0,0)。
pub fn map_rel(display: Display, base: (i32, i32), dx: i32, dy: i32) -> (i32, i32) {
    let max_x = display.origin_x.saturating_add(display.width.max(1) as i32 - 1);
    let max_y = display.origin_y.saturating_add(display.height.max(1) as i32 - 1);
    (
        base.0.saturating_add(dx).clamp(display.origin_x, max_x),
        base.1.saturating_add(dy).clamp(display.origin_y, max_y),
    )
}

pub fn map_event(display: Display, event: &InputEvent) -> Option<(i32, i32)> {
    if event.kind != InputEvent::MOVE_ABS && event.kind != InputEvent::BUTTON && event.kind != InputEvent::TOUCH {
        return Some((0, 0));
    }
    if event.kind == InputEvent::BUTTON || event.kind == InputEvent::TOUCH {
        return Some((display.origin_x, display.origin_y));
    }
    if event.payload.len() < 8 {
        return None;
    }
    let x = i32::from_le_bytes(event.payload[0..4].try_into().ok()?);
    let y = i32::from_le_bytes(event.payload[4..8].try_into().ok()?);
    map_abs(display, x, y)
}

/// 主控窗口的滚动量换成被控端要投递的像素滚动量。
///
/// 约定是直通自然滚动：窗口里往下滚，远端内容也往下走。取反是因为
/// NSEvent 的读数方向和历史滚动条方向相反。窗口有时会给很大的惯性值，
/// 单次滚动限制在 ±200 像素内，避免一次投递把远端甩出去。
pub fn wheel_delta(delta_y: f64, delta_x: f64) -> (i32, i32) {
    let clamp = |v: f64| -> i32 {
        let out = -v.round();
        out.clamp(-200.0, 200.0) as i32
    };
    (clamp(delta_x), clamp(delta_y))
}

/// HID 键盘用法到 macOS 虚拟键码。表里没有的键返回 None，调用方忽略，不断会话。
pub fn mac_keycode(usage: u16) -> Option<u16> {
    Some(match usage {
        0x04 => 0x00, // a
        0x05 => 0x0B, // b
        0x06 => 0x08, // c
        0x07 => 0x02, // d
        0x08 => 0x0E, // e
        0x09 => 0x03, // f
        0x0A => 0x05, // g
        0x0B => 0x04, // h
        0x0C => 0x22, // i
        0x0D => 0x26, // j
        0x0E => 0x28, // k
        0x0F => 0x25, // l
        0x10 => 0x2E, // m
        0x11 => 0x2D, // n
        0x12 => 0x1F, // o
        0x13 => 0x23, // p
        0x14 => 0x0C, // q
        0x15 => 0x0F, // r
        0x16 => 0x01, // s
        0x17 => 0x11, // t
        0x18 => 0x20, // u
        0x19 => 0x09, // v
        0x1A => 0x0D, // w
        0x1B => 0x07, // x
        0x1C => 0x10, // y
        0x1D => 0x06, // z
        0x1E => 0x12, // 1
        0x1F => 0x13,
        0x20 => 0x14,
        0x21 => 0x15,
        0x22 => 0x17, // 5
        0x23 => 0x16, // 6
        0x24 => 0x1A, // 7
        0x25 => 0x1C, // 8
        0x26 => 0x19, // 9
        0x27 => 0x1D, // 0
        0x28 => 0x24, // enter
        0x29 => 0x35, // escape
        0x2A => 0x33, // backspace
        0x2B => 0x30, // tab
        0x2C => 0x31, // space
        0x2D => 0x1B, // -
        0x2E => 0x18, // =
        0x2F => 0x21, // [
        0x30 => 0x1E, // ]
        0x31 => 0x2A, // backslash
        0x33 => 0x29, // ;
        0x34 => 0x27, // '
        0x35 => 0x32, // `
        0x36 => 0x2B, // ,
        0x37 => 0x2F, // .
        0x38 => 0x2C, // /
        0x39 => 0x39, // caps lock
        0x3A => 0x7A, // F1
        0x3B => 0x78,
        0x3C => 0x63,
        0x3D => 0x76,
        0x3E => 0x60,
        0x3F => 0x61,
        0x40 => 0x62,
        0x41 => 0x64,
        0x42 => 0x65,
        0x43 => 0x6D,
        0x44 => 0x67,
        0x45 => 0x6F, // F12
        0x49 => 0x72, // insert（Mac 上是 Help 键）
        0x4A => 0x73, // home
        0x4B => 0x74, // page up
        0x4C => 0x75, // forward delete
        0x4D => 0x77, // end
        0x4E => 0x79, // page down
        0x4F => 0x7C, // right
        0x50 => 0x7B, // left
        0x51 => 0x7D, // down
        0x52 => 0x7E, // up
        0xE0 => 0x3B, // left control
        0xE1 => 0x38, // left shift
        0xE2 => 0x3A, // left option
        0xE3 => 0x37, // left command
        0xE4 => 0x3E, // right control
        0xE5 => 0x3C, // right shift
        0xE6 => 0x3D, // right option
        0xE7 => 0x36, // right command
        _ => return None,
    })
}

/// Windows 虚拟键码（`VK_*`）到 USB HID 键盘用法。
///
/// 通用修饰键 `VK_SHIFT` / `VK_CONTROL` / `VK_MENU` 按左侧处理；窗口过程
/// 会用扫描码和扩展位把它们细化成 `VK_L*` / `VK_R*`。表里没有的键返回
/// None，调用方忽略，不断会话。
pub fn hid_from_windows_vk(vk: u16) -> Option<u16> {
    Some(match vk {
        // A-Z 和 0-9 在 HID 里是连续段，只差一个固定偏移。
        0x41..=0x5A => vk - 0x41 + 0x04,
        0x30 => 0x27,                    // 0
        0x31..=0x39 => vk - 0x31 + 0x1E, // 1-9
        0x08 => 0x2A,                    // backspace
        0x09 => 0x2B,                    // tab
        0x0D => 0x28,                    // enter
        0x10 => 0xE1,                    // shift（通用码，按左）
        0x11 => 0xE0,                    // control（通用码，按左）
        0x12 => 0xE2,                    // alt（通用码，按左）
        0x14 => 0x39,                    // caps lock
        0x1B => 0x29,                    // escape
        0x20 => 0x2C,                    // space
        0x21 => 0x4B,                    // page up
        0x22 => 0x4E,                    // page down
        0x23 => 0x4D,                    // end
        0x24 => 0x4A,                    // home
        0x25 => 0x50,                    // left
        0x26 => 0x52,                    // up
        0x27 => 0x4F,                    // right
        0x28 => 0x51,                    // down
        0x2D => 0x49,                    // insert
        0x2E => 0x4C,                    // delete
        0x5B => 0xE3,                    // left win
        0x5C => 0xE7,                    // right win
        0x70..=0x7B => vk - 0x70 + 0x3A, // F1-F12
        0xA0 => 0xE1,                    // left shift
        0xA1 => 0xE5,                    // right shift
        0xA2 => 0xE0,                    // left control
        0xA3 => 0xE4,                    // right control
        0xA4 => 0xE2,                    // left alt
        0xA5 => 0xE6,                    // right alt
        0xBA => 0x33,                    // ;:
        0xBB => 0x2E,                    // =+
        0xBC => 0x36,                    // ,<
        0xBD => 0x2D,                    // -_
        0xBE => 0x37,                    // .>
        0xBF => 0x38,                    // /?
        0xC0 => 0x35,                    // `~
        0xDB => 0x2F,                    // [{
        0xDC => 0x31,                    // \|
        0xDD => 0x30,                    // ]}
        0xDE => 0x34,                    // '"
        0xE2 => 0x64,                    // 非美式键盘的反斜杠
        _ => return None,
    })
}

/// HID 键盘用法到 PS/2 扫描码（set 1）。返回的第二项表示要不要带 E0 扩展前缀。
///
/// Windows 的 SendInput 用扫描码投键盘，因为同一个虚拟键码在不同布局下
/// 打出来的字符不一样；扫描码才跟着物理按键走。
pub fn scancode_from_hid(usage: u16) -> Option<(u8, bool)> {
    Some(match usage {
        0x04 => (0x1E, false),
        0x05 => (0x30, false),
        0x06 => (0x2E, false),
        0x07 => (0x20, false),
        0x08 => (0x12, false),
        0x09 => (0x21, false),
        0x0A => (0x22, false),
        0x0B => (0x23, false),
        0x0C => (0x17, false),
        0x0D => (0x24, false),
        0x0E => (0x25, false),
        0x0F => (0x26, false),
        0x10 => (0x32, false),
        0x11 => (0x31, false),
        0x12 => (0x18, false),
        0x13 => (0x19, false),
        0x14 => (0x10, false),
        0x15 => (0x13, false),
        0x16 => (0x1F, false),
        0x17 => (0x14, false),
        0x18 => (0x16, false),
        0x19 => (0x2F, false),
        0x1A => (0x11, false),
        0x1B => (0x2D, false),
        0x1C => (0x15, false),
        0x1D => (0x2C, false),
        0x1E => (0x02, false),
        0x1F => (0x03, false),
        0x20 => (0x04, false),
        0x21 => (0x05, false),
        0x22 => (0x06, false),
        0x23 => (0x07, false),
        0x24 => (0x08, false),
        0x25 => (0x09, false),
        0x26 => (0x0A, false),
        0x27 => (0x0B, false),
        0x28 => (0x1C, false),
        0x29 => (0x01, false),
        0x2A => (0x0E, false),
        0x2B => (0x0F, false),
        0x2C => (0x39, false),
        0x2D => (0x0C, false),
        0x2E => (0x0D, false),
        0x2F => (0x1A, false),
        0x30 => (0x1B, false),
        0x31 => (0x2B, false),
        0x33 => (0x27, false),
        0x34 => (0x28, false),
        0x35 => (0x29, false),
        0x36 => (0x33, false),
        0x37 => (0x34, false),
        0x38 => (0x35, false),
        0x39 => (0x3A, false),
        0x3A => (0x3B, false),
        0x3B => (0x3C, false),
        0x3C => (0x3D, false),
        0x3D => (0x3E, false),
        0x3E => (0x3F, false),
        0x3F => (0x40, false),
        0x40 => (0x41, false),
        0x41 => (0x42, false),
        0x42 => (0x43, false),
        0x43 => (0x44, false),
        0x44 => (0x57, false),
        0x45 => (0x58, false),
        // 方向键和上面那几个编辑键要带扩展前缀。
        0x49 => (0x52, true),
        0x4A => (0x47, true),
        0x4B => (0x49, true),
        0x4C => (0x53, true),
        0x4D => (0x4F, true),
        0x4E => (0x51, true),
        0x4F => (0x4D, true),
        0x50 => (0x4B, true),
        0x51 => (0x50, true),
        0x52 => (0x48, true),
        0x53 => (0x45, false),
        0x54 => (0x35, true),
        0x55 => (0x37, true),
        0x56 => (0x4A, false),
        0x58 => (0x1C, true),
        0x62 => (0x35, true),
        0x63 => (0x46, false),
        0x64 => (0x1D, true),
        0x65 => (0x5D, true),
        0x66 => (0x5B, true),
        0x67 => (0x5C, true),
        0xE0 => (0x1D, false),
        0xE1 => (0x2A, false),
        0xE2 => (0x38, false),
        0xE3 => (0x37, false),
        0xE4 => (0x1D, true),
        0xE5 => (0x36, false),
        0xE6 => (0x38, true),
        0xE7 => (0x5C, true),
        0xE8 => (0x6D, true),
        0xE9 => (0x6B, true),
        0xEA => (0x6C, true),
        0xEB => (0x6D, true),
        0xEC => (0x6E, true),
        0xED => (0x76, true),
        _ => return None,
    })
}

/// macOS 虚拟键码回到 HID 用法。左右修饰键共用一个键码时取左侧。没有的键返回 None。
pub fn hid_from_mac(code: u16) -> Option<u16> {
    Some(match code {
        0x00 => 0x04,
        0x0B => 0x05,
        0x08 => 0x06,
        0x02 => 0x07,
        0x0E => 0x08,
        0x03 => 0x09,
        0x05 => 0x0A,
        0x04 => 0x0B,
        0x22 => 0x0C,
        0x26 => 0x0D,
        0x28 => 0x0E,
        0x25 => 0x0F,
        0x2E => 0x10,
        0x2D => 0x11,
        0x1F => 0x12,
        0x23 => 0x13,
        0x0C => 0x14,
        0x0F => 0x15,
        0x01 => 0x16,
        0x11 => 0x17,
        0x20 => 0x18,
        0x09 => 0x19,
        0x0D => 0x1A,
        0x07 => 0x1B,
        0x10 => 0x1C,
        0x06 => 0x1D,
        0x12 => 0x1E,
        0x13 => 0x1F,
        0x14 => 0x20,
        0x15 => 0x21,
        0x17 => 0x22,
        0x16 => 0x23,
        0x1A => 0x24,
        0x1C => 0x25,
        0x19 => 0x26,
        0x1D => 0x27,
        0x24 => 0x28,
        0x35 => 0x29,
        0x33 => 0x2A,
        0x30 => 0x2B,
        0x31 => 0x2C,
        0x1B => 0x2D,
        0x18 => 0x2E,
        0x21 => 0x2F,
        0x1E => 0x30,
        0x2A => 0x31,
        0x29 => 0x33,
        0x27 => 0x34,
        0x32 => 0x35,
        0x2B => 0x36,
        0x2F => 0x37,
        0x2C => 0x38,
        0x7A => 0x3A,
        0x78 => 0x3B,
        0x63 => 0x3C,
        0x76 => 0x3D,
        0x60 => 0x3E,
        0x61 => 0x3F,
        0x62 => 0x40,
        0x64 => 0x41,
        0x65 => 0x42,
        0x6D => 0x43,
        0x67 => 0x44,
        0x6F => 0x45,
        0x7C => 0x4F,
        0x7B => 0x50,
        0x7D => 0x51,
        0x7E => 0x52,
        0x3B => 0xE0,
        0x38 => 0xE1,
        0x3A => 0xE2,
        0x37 => 0xE3,
        0x3E => 0xE4,
        0x3C => 0xE5,
        0x3D => 0xE6,
        0x36 => 0xE7,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_display_origin_is_added() {
        let display = Display { origin_x: 1920, origin_y: 0, width: 1920, height: 1080 };
        assert_eq!(map_abs(display, 10, 20), Some((1930, 20)));
        assert_eq!(map_abs(display, 1920, 0), None);
    }

    #[test]
    fn retina_points_convert_to_physical_pixels() {
        let display = Display { origin_x: 0, origin_y: 0, width: 2880, height: 1800 };
        assert_eq!(local_from_points((100, 50), display, 2000), (200, 100));
        // 非 Retina 副屏：原点已经是物理像素，point 直接减。
        let second = Display { origin_x: 1920, origin_y: 0, width: 1920, height: 1080 };
        assert_eq!(local_from_points((1930, 20), second, 1000), (10, 20));
        // 缩放进位：1.5x 屏上的 1 point 是 1.5 像素，向下取整。
        assert_eq!(local_from_points((1, 1), display, 1500), (1, 1));
    }

    #[test]
    fn relative_move_stays_inside_display() {
        let display = Display { origin_x: 1920, origin_y: 0, width: 1920, height: 1080 };
        assert_eq!(map_rel(display, (1930, 20), 5, -10), (1935, 10));
        // 越界夹到边缘，而不是回到左上角。
        assert_eq!(map_rel(display, (1930, 20), -5000, 5000), (1920, 1079));
    }

    #[test]
    fn editing_keys_are_mapped_on_mac() {
        assert_eq!(mac_keycode(0x39), Some(0x39)); // caps lock
        assert_eq!(mac_keycode(0x49), Some(0x72)); // insert
        assert_eq!(mac_keycode(0x4A), Some(0x73)); // home
        assert_eq!(mac_keycode(0x4B), Some(0x74)); // page up
        assert_eq!(mac_keycode(0x4C), Some(0x75)); // delete
        assert_eq!(mac_keycode(0x4D), Some(0x77)); // end
        assert_eq!(mac_keycode(0x4E), Some(0x79)); // page down
    }

    #[test]
    fn unknown_hid_usage_is_ignored() {
        assert_eq!(mac_keycode(0x04), Some(0x00));
        assert_eq!(mac_keycode(0xFFFF), None);
        assert_eq!(hid_from_mac(0x00), Some(0x04));
        assert_eq!(hid_from_mac(0x3B), Some(0xE0));
        assert_eq!(hid_from_mac(0x2C), Some(0x38)); // slash is needed for paths and code
        assert_eq!(hid_from_mac(0x3C), Some(0xE5)); // right shift is distinct
        assert_eq!(mac_keycode(0xE5), Some(0x3C));
        assert_eq!(hid_from_mac(0xFFFF), None);
    }

    #[test]
    fn windows_virtual_keys_map_to_hid() {
        // 字母和数字。
        assert_eq!(hid_from_windows_vk(0x41), Some(0x04)); // A
        assert_eq!(hid_from_windows_vk(0x5A), Some(0x1D)); // Z
        assert_eq!(hid_from_windows_vk(0x30), Some(0x27)); // 0
        assert_eq!(hid_from_windows_vk(0x31), Some(0x1E)); // 1
        assert_eq!(hid_from_windows_vk(0x39), Some(0x26)); // 9
        // 功能键。
        assert_eq!(hid_from_windows_vk(0x70), Some(0x3A)); // F1
        assert_eq!(hid_from_windows_vk(0x7B), Some(0x45)); // F12
        // 方向键和编辑键。
        assert_eq!(hid_from_windows_vk(0x25), Some(0x50));
        assert_eq!(hid_from_windows_vk(0x26), Some(0x52));
        assert_eq!(hid_from_windows_vk(0x27), Some(0x4F));
        assert_eq!(hid_from_windows_vk(0x28), Some(0x51));
        assert_eq!(hid_from_windows_vk(0x24), Some(0x4A)); // home
        assert_eq!(hid_from_windows_vk(0x23), Some(0x4D)); // end
        assert_eq!(hid_from_windows_vk(0x21), Some(0x4B)); // page up
        assert_eq!(hid_from_windows_vk(0x22), Some(0x4E)); // page down
        assert_eq!(hid_from_windows_vk(0x2E), Some(0x4C)); // delete
        assert_eq!(hid_from_windows_vk(0x2D), Some(0x49)); // insert
        assert_eq!(hid_from_windows_vk(0x14), Some(0x39)); // caps lock
        // 左右修饰键和 Win 键。
        assert_eq!(hid_from_windows_vk(0xA0), Some(0xE1));
        assert_eq!(hid_from_windows_vk(0xA1), Some(0xE5));
        assert_eq!(hid_from_windows_vk(0xA2), Some(0xE0));
        assert_eq!(hid_from_windows_vk(0xA3), Some(0xE4));
        assert_eq!(hid_from_windows_vk(0xA4), Some(0xE2));
        assert_eq!(hid_from_windows_vk(0xA5), Some(0xE6));
        assert_eq!(hid_from_windows_vk(0x5B), Some(0xE3));
        assert_eq!(hid_from_windows_vk(0x5C), Some(0xE7));
        // 空白、回车、取消、制表、退格。
        assert_eq!(hid_from_windows_vk(0x20), Some(0x2C));
        assert_eq!(hid_from_windows_vk(0x0D), Some(0x28));
        assert_eq!(hid_from_windows_vk(0x1B), Some(0x29));
        assert_eq!(hid_from_windows_vk(0x09), Some(0x2B));
        assert_eq!(hid_from_windows_vk(0x08), Some(0x2A));
        // 常用标点。
        assert_eq!(hid_from_windows_vk(0xBA), Some(0x33)); // ;
        assert_eq!(hid_from_windows_vk(0xBB), Some(0x2E)); // =
        assert_eq!(hid_from_windows_vk(0xBC), Some(0x36)); // ,
        assert_eq!(hid_from_windows_vk(0xBD), Some(0x2D)); // -
        assert_eq!(hid_from_windows_vk(0xBE), Some(0x37)); // .
        assert_eq!(hid_from_windows_vk(0xBF), Some(0x38)); // /
        assert_eq!(hid_from_windows_vk(0xC0), Some(0x35)); // `
        assert_eq!(hid_from_windows_vk(0xDB), Some(0x2F)); // [
        assert_eq!(hid_from_windows_vk(0xDC), Some(0x31)); // backslash
        assert_eq!(hid_from_windows_vk(0xDD), Some(0x30)); // ]
        assert_eq!(hid_from_windows_vk(0xDE), Some(0x34)); // '
        assert_eq!(hid_from_windows_vk(0xFFFF), None);
    }

    #[test]
    fn scancodes_cover_letters_and_mark_extended_keys() {
        assert_eq!(scancode_from_hid(0x04), Some((0x1E, false)));
        assert_eq!(scancode_from_hid(0x28), Some((0x1C, false)));
        assert_eq!(scancode_from_hid(0x2C), Some((0x39, false)));
        // 方向键和右边的 Ctrl/Alt 是扩展键，少了前缀会被当成小键盘数字。
        assert_eq!(scancode_from_hid(0x52), Some((0x48, true)));
        assert_eq!(scancode_from_hid(0x64), Some((0x1D, true)));
        assert_eq!(scancode_from_hid(0xE4), Some((0x1D, true))); // right control
        assert_eq!(scancode_from_hid(0xFFFF), None);
    }

    #[test]
    fn wheel_keeps_natural_direction_and_clamps() {
        assert_eq!(wheel_delta(-12.0, 0.0), (0, 12));
        assert_eq!(wheel_delta(0.0, -3.0), (3, 0));
        assert_eq!(wheel_delta(-12.3, 4.6), (-5, 12));
        assert_eq!(wheel_delta(-9000.0, 9000.0), (-200, 200));
        assert_eq!(wheel_delta(0.0, 0.0), (0, 0));
    }
}
