//! DXGI 光标形状转协议里的 BGRA。
//!
//! 这里只认字节和普通整数，不碰 Windows API，测试在 macOS 上也能跑。
//! `capture.rs` 负责把 `GetFramePointerShape` 的结果填进 [`ShapeInfo`]，
//! 再调用 [`convert`]。
//!
//! DXGI 的三种形状：
//!
//! - `COLOR`：32 位 BGRA，alpha 原样保留。
//! - `MONOCHROME`：1bpp。`Height` 是实际高度的两倍，前半是 AND 掩码，
//!   后半是 XOR 掩码，`Pitch` 是位图的行宽（含对齐）。
//! - `MASKED_COLOR`：32 位 BGRA，alpha 当开关用。alpha 为 0 的像素要和
//!   桌面做 XOR，alpha 非 0 的像素直接盖在桌面上。
//!
//! 反色（单色的 AND=1/XOR=1、masked color 的 alpha=0）需要桌面像素才能算，
//! 这里拿不到桌面，所以这些像素留成透明，不伪造颜色。

// 非 Windows 平台只有单元测试会用到这些函数。
#![cfg_attr(not(windows), allow(dead_code))]

use jr_proto::{CursorShape, CURSOR_SHAPE_MAX};

/// 和 `DXGI_OUTDUPL_POINTER_SHAPE_TYPE` 的数值一致。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShapeType {
    Monochrome,
    Color,
    MaskedColor,
}

impl ShapeType {
    /// 把 DXGI 的 `Type` 字段转成枚举。不认识的值返回 None，调用方跳过。
    pub fn from_dxgi(raw: u32) -> Option<Self> {
        match raw {
            1 => Some(Self::Monochrome),
            2 => Some(Self::Color),
            4 => Some(Self::MaskedColor),
            _ => None,
        }
    }
}

/// 从 `DXGI_OUTDUPL_POINTER_SHAPE_INFO` 抄过来的普通结构。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShapeInfo {
    pub shape_type: ShapeType,
    pub width: u32,
    /// DXGI 原值。单色形状这里是实际高度的两倍。
    pub height: u32,
    /// 位图行宽，单位字节。
    pub pitch: u32,
    pub hotspot_x: i32,
    pub hotspot_y: i32,
}

/// 把一块指针形状缓冲转成协议形状。输出行从上到下，热点保持 DXGI 的左上原点。
///
/// 返回 None 表示这块形状不能可靠还原：尺寸非法、超过协议上限、缓冲长度
/// 对不上、或者类型不认识。调用方应当跳过它，不要拿旧数据凑。
pub fn convert(info: &ShapeInfo, data: &[u8]) -> Option<CursorShape> {
    let height = match info.shape_type {
        ShapeType::Monochrome => {
            // 单色缓冲的 Height 是实际高度乘二，奇数说明数据不完整。
            if info.height == 0 || info.height % 2 != 0 {
                return None;
            }
            info.height / 2
        }
        ShapeType::Color | ShapeType::MaskedColor => info.height,
    };
    if info.width == 0 || height == 0 {
        return None;
    }
    if info.width > u32::from(CURSOR_SHAPE_MAX) || height > u32::from(CURSOR_SHAPE_MAX) {
        return None;
    }
    let hotspot_x = u16::try_from(info.hotspot_x).ok()?;
    let hotspot_y = u16::try_from(info.hotspot_y).ok()?;

    let width = info.width as usize;
    let height = height as usize;
    let mut bgra = vec![0u8; width * height * 4];

    match info.shape_type {
        ShapeType::Color => {
            let pitch = info.pitch as usize;
            // 颜色形状每行至少 width 个 32 位像素。
            if pitch < width * 4 || data.len() < pitch * height {
                return None;
            }
            for y in 0..height {
                let src = &data[y * pitch..y * pitch + width * 4];
                bgra[y * width * 4..(y + 1) * width * 4].copy_from_slice(src);
            }
        }
        ShapeType::MaskedColor => {
            let pitch = info.pitch as usize;
            if pitch < width * 4 || data.len() < pitch * height {
                return None;
            }
            for y in 0..height {
                for x in 0..width {
                    let src = y * pitch + x * 4;
                    let dst = (y * width + x) * 4;
                    if data[src + 3] == 0 {
                        // alpha 0：该像素本来要和桌面 XOR。拿不到桌面，
                        // 留透明跳过，不伪造一个颜色。
                        continue;
                    }
                    // masked color 的 alpha 只区分「透明/不透明」，
                    // 不拿它做半透明，统一按不透明输出。
                    bgra[dst] = data[src];
                    bgra[dst + 1] = data[src + 1];
                    bgra[dst + 2] = data[src + 2];
                    bgra[dst + 3] = 255;
                }
            }
        }
        ShapeType::Monochrome => {
            // 单色位图：每个字节最高位是这一行最左边的像素。
            let pitch = info.pitch as usize;
            let raw_height = info.height as usize;
            if pitch == 0 || pitch * 8 < width || data.len() < pitch * raw_height {
                return None;
            }
            let xor_half = pitch * height;
            for y in 0..height {
                let and_row = y * pitch;
                let xor_row = xor_half + y * pitch;
                for x in 0..width {
                    let bit = 0x80u8 >> (x % 8);
                    let and = data[and_row + x / 8] & bit != 0;
                    let xor = data[xor_row + x / 8] & bit != 0;
                    let dst = (y * width + x) * 4;
                    let pixel = match (and, xor) {
                        (false, false) => [0, 0, 0, 255],      // 黑
                        (false, true) => [255, 255, 255, 255], // 白
                        (true, false) => [0, 0, 0, 0],         // 透出桌面
                        // AND=1/XOR=1 是把桌面反色，没有桌面像素就算不出来，
                        // 按透明处理，不伪造。
                        (true, true) => [0, 0, 0, 0],
                    };
                    bgra[dst..dst + 4].copy_from_slice(&pixel);
                }
            }
        }
    }

    Some(CursorShape::new(
        hotspot_x,
        hotspot_y,
        width as u16,
        height as u16,
        bgra,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(shape_type: ShapeType, width: u32, height: u32, pitch: u32) -> ShapeInfo {
        ShapeInfo {
            shape_type,
            width,
            height,
            pitch,
            hotspot_x: 1,
            hotspot_y: 2,
        }
    }

    fn pixel(shape: &CursorShape, x: usize, y: usize) -> [u8; 4] {
        let i = (y * shape.width as usize + x) * 4;
        [
            shape.bgra[i],
            shape.bgra[i + 1],
            shape.bgra[i + 2],
            shape.bgra[i + 3],
        ]
    }

    #[test]
    fn color_preserves_bgra_and_hotspot() {
        // 2x2，每行 8 字节，行内就是 BGRA。
        let data = [
            1, 2, 3, 4, 5, 6, 7, 8, // 第一行
            9, 10, 11, 12, 13, 14, 15, 16, // 第二行
        ];
        let shape = convert(&info(ShapeType::Color, 2, 2, 8), &data).unwrap();
        assert_eq!((shape.width, shape.height), (2, 2));
        assert_eq!((shape.hotspot_x, shape.hotspot_y), (1, 2));
        assert_eq!(shape.bgra, data);
    }

    #[test]
    fn color_honors_pitch_padding() {
        // 每行只取前 4 字节，跳过 4 字节行尾填充。
        let data = [
            1, 2, 3, 4, 0xFF, 0xFF, 0xFF, 0xFF, // 第一行
            9, 10, 11, 12, 0xFF, 0xFF, 0xFF, 0xFF, // 第二行
        ];
        let shape = convert(&info(ShapeType::Color, 1, 2, 8), &data).unwrap();
        assert_eq!(pixel(&shape, 0, 0), [1, 2, 3, 4]);
        assert_eq!(pixel(&shape, 0, 1), [9, 10, 11, 12]);
    }

    #[test]
    fn monochrome_combines_and_xor_masks() {
        // 实际 2x2，DXGI 的 Height 是 4。每行 4 字节，够放下 2 个像素。
        let mut data = vec![0u8; 4 * 4];
        // AND 前半：第一行左边置位，第二行右边置位。
        data[0] = 0b1000_0000;
        data[4] = 0b0100_0000;
        // XOR 后半：第一行右边置位，第二行都不置位。
        data[8] = 0b0100_0000;
        data[12] = 0;
        let shape = convert(&info(ShapeType::Monochrome, 2, 4, 4), &data).unwrap();
        assert_eq!((shape.width, shape.height), (2, 2));
        // (0,0) AND=1/XOR=0 -> 透明
        assert_eq!(pixel(&shape, 0, 0), [0, 0, 0, 0]);
        // (1,0) AND=0/XOR=1 -> 白
        assert_eq!(pixel(&shape, 1, 0), [255, 255, 255, 255]);
        // (0,1) AND=0/XOR=0 -> 黑
        assert_eq!(pixel(&shape, 0, 1), [0, 0, 0, 255]);
        // (1,1) AND=1/XOR=1 -> 需要桌面反色，留透明
        assert_eq!(pixel(&shape, 1, 1), [0, 0, 0, 0]);
    }

    #[test]
    fn monochrome_rejects_odd_height() {
        assert!(convert(&info(ShapeType::Monochrome, 1, 3, 4), &[0; 12]).is_none());
    }

    #[test]
    fn masked_color_treats_zero_alpha_as_xor() {
        // 两个像素：第一个 alpha=0（XOR，留透明），第二个 alpha 非 0（不透明）。
        let data = [
            10, 20, 30, 0, // XOR 像素
            40, 50, 60, 128, // 不透明像素
        ];
        let shape = convert(&info(ShapeType::MaskedColor, 2, 1, 8), &data).unwrap();
        assert_eq!(pixel(&shape, 0, 0), [0, 0, 0, 0]);
        assert_eq!(pixel(&shape, 1, 0), [40, 50, 60, 255]);
    }

    #[test]
    fn rejects_oversized_and_short_buffers() {
        assert!(convert(&info(ShapeType::Color, 129, 1, 129 * 4), &vec![0; 129 * 4]).is_none());
        assert!(convert(&info(ShapeType::Color, 2, 2, 8), &[0; 8]).is_none());
        assert!(convert(&info(ShapeType::Color, 2, 2, 4), &[0; 16]).is_none());
        assert!(convert(&info(ShapeType::Monochrome, 2, 4, 4), &[0; 8]).is_none());
    }

    #[test]
    fn unknown_shape_type_is_skipped() {
        assert!(ShapeType::from_dxgi(3).is_none());
        assert_eq!(ShapeType::from_dxgi(1), Some(ShapeType::Monochrome));
        assert_eq!(ShapeType::from_dxgi(2), Some(ShapeType::Color));
        assert_eq!(ShapeType::from_dxgi(4), Some(ShapeType::MaskedColor));
    }
}
