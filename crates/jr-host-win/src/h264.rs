//! 编码器进出的两段纯数据换算。放在这里是因为它们不碰系统，
//! 任何平台都能跑测试——而 Windows 采集那条链上最容易写错的就是这两段。

/// 编码器和 NV12 纹理都只接受偶数尺寸。向下取整，并且至少给到 2，
/// 避免 0 或 1 这种建不出纹理的尺寸继续往下传。
pub fn even_size(width: u32, height: u32) -> (u32, u32) {
    ((width & !1).max(2), (height & !1).max(2))
}

/// AVCC（长度前缀）转 Annex-B（起始码）。返回转换后的字节，以及这段里有没有 IDR。
///
/// 编码器输出的是「长度 + NAL」的形式，网络和播放器要的是「00 00 00 01 + NAL」。
/// 长度是 4 字节大端。碰到不合规的长度就直接停，把已经转好的交出去，
/// 而不是把半个包发出去。
pub fn avcc_to_annex_b(src: &[u8]) -> (Vec<u8>, bool) {
    let mut out = Vec::with_capacity(src.len() + 16);
    let mut idr = false;
    let mut at = 0usize;
    while at + 4 <= src.len() {
        let len = u32::from_be_bytes([src[at], src[at + 1], src[at + 2], src[at + 3]]) as usize;
        at += 4;
        if len == 0 || at + len > src.len() {
            break;
        }
        let nal = &src[at..at + len];
        at += len;
        // NAL 头低 5 位是类型，5 就是 IDR 切片。
        if nal[0] & 0x1F == 5 {
            idr = true;
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(nal);
    }
    (out, idr)
}

/// BGRA（桌面复制的输出）转 NV12（编码器的输入），BT.709 有限范围。
///
/// `stride` 是每行真实字节数，桌面纹理的宽度会带对齐，不能当成 width*4。
/// 宽高按偶数处理，奇数会向下取整——编码器不接受奇数尺寸。
pub fn bgra_to_nv12(src: &[u8], stride: usize, width: usize, height: usize, out: &mut Vec<u8>) {
    let w = width & !1;
    let h = height & !1;
    if w == 0 || h == 0 {
        out.clear();
        return;
    }
    let y_size = w * h;
    let uv_size = w * h / 2;
    out.clear();
    out.resize(y_size + uv_size, 0);
    let (y_plane, uv_plane) = out.split_at_mut(y_size);

    for row in 0..h {
        let src_row = &src[row * stride..];
        for col in 0..w {
            let p = col * 4;
            let b = src_row[p] as f32;
            let g = src_row[p + 1] as f32;
            let r = src_row[p + 2] as f32;
            let y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
            y_plane[row * w + col] = clamp_u8(16.0 + 219.0 / 255.0 * y);
            // 色度是 2x2 平均，取每个 2x2 块的左上角样本即可，
            // 桌面内容细节高，逐点平均反而更慢。
            if row % 2 == 0 && col % 2 == 0 {
                let u = (b - y) / 1.8556;
                let v = (r - y) / 1.5748;
                let at = (row / 2) * w + (col & !1);
                uv_plane[at] = clamp_u8(128.0 + 224.0 / 255.0 * u);
                uv_plane[at + 1] = clamp_u8(128.0 + 224.0 / 255.0 * v);
            }
        }
    }
}

/// NV12（解码器输出）转 BGRA（StretchDIBits 的输入），BT.709 有限范围。
///
/// `stride` 是 Y 平面每行字节数，UV 平面沿用同一个行距。Windows 来的行距
/// 通常按 16 字节对齐，不能当成 `width`。宽高允许奇数：色度按
/// `(width + 1) / 2`、`(height + 1) / 2` 取，UV 每行按两字节一对交错。
/// 源数据不够时按有限范围的黑点补，调用方不必先裁长度。
pub fn nv12_to_bgra(src: &[u8], stride: usize, width: usize, height: usize, out: &mut Vec<u8>) {
    out.clear();
    if width == 0 || height == 0 || stride < width {
        return;
    }
    let Some(pixels) = width.checked_mul(height) else {
        return;
    };
    let Some(out_len) = pixels.checked_mul(4) else {
        return;
    };
    let Some(uv_start) = stride.checked_mul(height) else {
        return;
    };
    // 奇数宽度时 UV 一行至少要有 ceil(width / 2) 对，可能比 Y 行距多一字节。
    let uv_stride = stride.max((width + 1) / 2 * 2);
    out.resize(out_len, 0);

    for row in 0..height {
        let y_at = row * stride;
        let uv_at = uv_start + (row / 2) * uv_stride;
        for col in 0..width {
            let y = i32::from(src.get(y_at + col).copied().unwrap_or(16));
            let pair = uv_at + (col / 2) * 2;
            let u = i32::from(src.get(pair).copied().unwrap_or(128));
            let v = i32::from(src.get(pair + 1).copied().unwrap_or(128));
            // BT.709 有限范围：Y 16-235，Cb/Cr 16-240。
            // 系数放大 256 倍后定点算，比逐点浮点省时间。
            let yy = ((y - 16).max(0) * 298 + 128) >> 8;
            let cb = u - 128;
            let cr = v - 128;
            let r = yy + ((459 * cr + 128) >> 8);
            let g = yy - ((55 * cb + 128) >> 8) - ((136 * cr + 128) >> 8);
            let b = yy + ((541 * cb + 128) >> 8);
            let at = (row * width + col) * 4;
            out[at] = b.clamp(0, 255) as u8;
            out[at + 1] = g.clamp(0, 255) as u8;
            out[at + 2] = r.clamp(0, 255) as u8;
            out[at + 3] = 255;
        }
    }
}

fn clamp_u8(v: f32) -> u8 {
    v.round().clamp(0.0, 255.0) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn even_size_rounds_down_and_never_returns_zero() {
        assert_eq!(even_size(1920, 1080), (1920, 1080));
        assert_eq!(even_size(1919, 1079), (1918, 1078));
        // 0/1 这种尺寸建不出纹理，至少给到 2。
        assert_eq!(even_size(1, 1), (2, 2));
        assert_eq!(even_size(0, 0), (2, 2));
    }

    #[test]
    fn avcc_is_rewritten_to_start_codes_and_flags_idr() {
        // 一个 SPS(7) + 一个 IDR(5) + 一个非 IDR(1)，都是 4 字节长度前缀。
        let src = vec![
            0, 0, 0, 3, 0x67, 0x42, 0x00, //
            0, 0, 0, 2, 0x65, 0x88, //
            0, 0, 0, 2, 0x41, 0x9A,
        ];
        let (out, idr) = avcc_to_annex_b(&src);
        assert!(idr, "里面有类型 5，应当认出 IDR");
        assert_eq!(
            out,
            vec![
                0, 0, 0, 1, 0x67, 0x42, 0x00, //
                0, 0, 0, 1, 0x65, 0x88, //
                0, 0, 0, 1, 0x41, 0x9A,
            ]
        );
    }

    #[test]
    fn truncated_avcc_stops_instead_of_emitting_half_a_nal() {
        // 长度说 8 字节，实际只有 2 字节。这一段不能交出去。
        let src = vec![0, 0, 0, 2, 0x41, 0x9A, 0, 0, 0, 8, 0x65, 0x88];
        let (out, idr) = avcc_to_annex_b(&src);
        assert_eq!(out, vec![0, 0, 0, 1, 0x41, 0x9A]);
        assert!(!idr);
    }

    #[test]
    fn nv12_layout_and_colors_are_right() {
        let (w, h) = (4usize, 2usize);
        let stride = w * 4;
        // 上半行白、下半行黑，方便看 Y 和 UV 的排布。
        let mut bgra = vec![0u8; stride * h];
        for col in 0..w {
            let p = col * 4;
            bgra[p] = 255;
            bgra[p + 1] = 255;
            bgra[p + 2] = 255;
            bgra[p + 3] = 255;
        }
        let mut out = Vec::new();
        bgra_to_nv12(&bgra, stride, w, h, &mut out);
        assert_eq!(out.len(), w * h + w * h / 2, "Y 平面加半分辨率的 UV 平面");
        // 第一行是白的：Y 接近 235，U/V 在中点 128。
        assert!((out[0] as i32 - 235).abs() <= 2, "白点亮度 {}", out[0]);
        let uv_at = w * h;
        assert!(
            (out[uv_at] as i32 - 128).abs() <= 2,
            "白点 U {}",
            out[uv_at]
        );
        // 第二行是黑的：Y 到 16 附近。
        assert!((out[w] as i32 - 16).abs() <= 2, "黑点亮度 {}", out[w]);
    }

    #[test]
    fn stride_larger_than_width_is_respected() {
        let (w, h) = (2usize, 2usize);
        // 真实行距比 width*4 宽，多出来的是对齐填充。
        let stride = 32usize;
        let mut bgra = vec![0u8; stride * h];
        // 每行第一个像素点是纯红，其余是黑。
        for row in 0..h {
            let p = row * stride;
            bgra[p + 2] = 255;
        }
        let mut out = Vec::new();
        bgra_to_nv12(&bgra, stride, w, h, &mut out);
        // 红点的 V 明显高于中点，U 低于中点。
        let uv_at = w * h;
        assert!(out[uv_at] < 120, "红点 U 应当偏低 {}", out[uv_at]);
        assert!(out[uv_at + 1] > 150, "红点 V 应当偏高 {}", out[uv_at + 1]);
        // 第二个像素是黑的，说明没有把填充当成画面。
        assert!((out[1] as i32 - 16).abs() <= 2, "第二列 {}", out[1]);
    }

    #[test]
    fn nv12_black_and_white_are_bt709_limited_range() {
        let mut out = Vec::new();
        // 2x2 全白：Y=235、UV=128，解码后应当是纯白。
        let white = [235u8, 235, 235, 235, 128, 128];
        nv12_to_bgra(&white, 2, 2, 2, &mut out);
        assert_eq!(out, vec![255u8; 16]);
        // 全黑：Y=16、UV=128，解码后应当是纯黑。alpha 固定不透明。
        let black = [16u8, 16, 16, 16, 128, 128];
        nv12_to_bgra(&black, 2, 2, 2, &mut out);
        assert_eq!(
            out,
            vec![0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255]
        );
    }

    #[test]
    fn nv12_stride_and_uv_interleave_are_right() {
        // 4x2，行距 6（每行多 2 字节填充）；UV 每行也按 6 走。
        let (stride, w, h) = (6usize, 4usize, 2usize);
        let mut src = vec![126u8; stride * h + stride];
        // 第一对色度 U 低 V 高，第二对 U 高 V 低。
        src[stride * h] = 90;
        src[stride * h + 1] = 180;
        src[stride * h + 2] = 200;
        src[stride * h + 3] = 50;
        let mut out = Vec::new();
        nv12_to_bgra(&src, stride, w, h, &mut out);
        assert_eq!(out.len(), w * h * 4);
        // 第 0 个像素 V 高，红色分量大于蓝色；第 2 个像素反过来。
        assert!(out[2] > out[0], "V 高应当偏红: {:?}", &out[0..4]);
        assert!(out[8] > out[10], "U 高应当偏蓝: {:?}", &out[8..12]);
        assert!(out.chunks(4).all(|p| p[3] == 255));
    }

    #[test]
    fn nv12_odd_size_is_padded_without_panicking() {
        // 3x3 奇数尺寸，行距按 4 对齐。源数据只给到最后一对色度，
        // 缺的行按黑点补，不能越界。
        let (stride, w, h) = (4usize, 3usize, 3usize);
        let mut src = vec![126u8; stride * h + stride * 2];
        // UV 归到中点，Y 才决定亮度。
        for b in &mut src[stride * h..] {
            *b = 128;
        }
        let mut out = Vec::new();
        nv12_to_bgra(&src, stride, w, h, &mut out);
        assert_eq!(out.len(), w * h * 4);
        assert!(out.chunks(4).all(|p| p[3] == 255));
        // 中灰的 Y=126 解码后接近 128。
        assert!((i32::from(out[0]) - 128).abs() <= 2, "中灰 {}", out[0]);
    }
}
