//! Windows 采集的 GPU 内转换。
//!
//! 桌面复制交给我们的是 BGRA 纹理，而 H.264 编码器要 NV12。这里用
//! `ID3D11VideoProcessor` 在显卡上完成 BGRA → NV12，再把 NV12 纹理
//! 包成 Media Foundation 样本交给编码器。默认路径不再 `Map` 回 CPU。
//!
//! 桌面复制纹理不一定带视频处理器输入视图要求的绑定标志，所以第一帧
//! 会先试着直接在源纹理上建输入视图；建不出来就退到一次 `CopyResource`
//! 到自己的 BGRA 纹理，再交给视频处理器。两条路都只走 GPU。
//!
//! NV12 纹理做了 3 个轮转槽位：编码器收下样本后可能还占着纹理，
//! 下一帧不能立刻覆盖同一张。初始化任何一步失败都返回人话错误，
//! 由 `capture.rs` 决定回退到 CPU 回读，而不是让整个采集起不来。

use std::mem::ManuallyDrop;

use windows::core::Interface;
use windows::Win32::Foundation::{RECT, TRUE};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Resource, ID3D11Texture2D, ID3D11VideoContext,
    ID3D11VideoDevice, ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator,
    ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView, D3D11_BIND_RENDER_TARGET,
    D3D11_BIND_SHADER_RESOURCE, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_COLOR_SPACE,
    D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT,
    D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_STREAM,
    D3D11_VIDEO_USAGE_PLAYBACK_NORMAL, D3D11_VPIV_DIMENSION_TEXTURE2D,
    D3D11_VPOV_DIMENSION_TEXTURE2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};

use crate::h264::even_size;

/// 编码器可能还攥着上一帧甚至两帧，纹理多留几个槽位避免覆盖。
/// 这里按硬件 MFT 最多同时持有 2 个已接受的输入来留 3 槽；没有 keyed
/// mutex 或 fence，编码器若破坏这个约定仍可能读到被覆盖的纹理。
/// 编码器拒收时 `capture.rs` 不会前进槽位，这一点是真机验证的重点。
const SLOT_COUNT: usize = 3;

/// 视频处理器要的 BT.709 有限范围色度。bit0 Usage(playback)、
/// bit2 YCbCr_Matrix(BT.709)、bit4-5 Nominal_Range(16-235)。
/// 和 `h264::bgra_to_nv12` 的换算保持一致。
const NV12_OUTPUT_COLOR_SPACE: u32 = (1 << 2) | (1 << 4);

struct Slot {
    texture: ID3D11Texture2D,
    output_view: ID3D11VideoProcessorOutputView,
}

/// 把桌面复制的 BGRA 纹理在 GPU 上转成 NV12，并轮转输出纹理。
pub struct SurfaceConverter {
    context: ID3D11DeviceContext,
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    /// 源纹理不能直接建输入视图时，先拷到这张 BGRA 纹理。
    scratch: ID3D11Texture2D,
    scratch_view: ID3D11VideoProcessorInputView,
    input_desc: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
    slots: Vec<Slot>,
    next: usize,
    /// `None` 表示还没试过直接在源纹理上建视图。
    direct_input: Option<bool>,
}

impl SurfaceConverter {
    /// 建齐视频处理器、中转纹理和 NV12 轮转槽位。
    /// 任何一步失败都返回错误，调用方回退 CPU 回读。
    pub fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        width: u32,
        height: u32,
        fps: u32,
    ) -> Result<Self, String> {
        if width == 0 || height == 0 {
            return Err("这块屏没有有效尺寸".into());
        }
        let (out_width, out_height) = even_size(width, height);
        let video_device: ID3D11VideoDevice = device
            .cast()
            .map_err(|e| format!("设备没有 D3D11 视频接口: {e}"))?;
        let video_context: ID3D11VideoContext = context
            .cast()
            .map_err(|e| format!("上下文没有 D3D11 视频接口: {e}"))?;

        let rate = DXGI_RATIONAL {
            Numerator: fps.max(1),
            Denominator: 1,
        };
        let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: width,
            InputHeight: height,
            OutputFrameRate: rate,
            OutputWidth: out_width,
            OutputHeight: out_height,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };
        let enumerator = unsafe { video_device.CreateVideoProcessorEnumerator(&content) }
            .map_err(|e| format!("视频处理器枚举器起不来: {e}"))?;

        // 先确认驱动真的能收 BGRA、能出 NV12，避免后面每帧才失败。
        let in_flags = unsafe { enumerator.CheckVideoProcessorFormat(DXGI_FORMAT_B8G8R8A8_UNORM) }
            .map_err(|e| format!("查不了 BGRA 输入支持: {e}"))?;
        if in_flags & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT.0 as u32 == 0 {
            return Err("视频处理器不收 BGRA 输入".into());
        }
        let out_flags = unsafe { enumerator.CheckVideoProcessorFormat(DXGI_FORMAT_NV12) }
            .map_err(|e| format!("查不了 NV12 输出支持: {e}"))?;
        if out_flags & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT.0 as u32 == 0 {
            return Err("视频处理器不出 NV12 输出".into());
        }

        let processor = unsafe { video_device.CreateVideoProcessor(&enumerator, 0) }
            .map_err(|e| format!("建不了视频处理器: {e}"))?;
        let dest = RECT {
            left: 0,
            top: 0,
            right: out_width as i32,
            bottom: out_height as i32,
        };
        // RGB 输入按全范围；输出按 BT.709 有限范围，和 CPU 回读路径同一套颜色。
        let input_color = D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 };
        let output_color = D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
            _bitfield: NV12_OUTPUT_COLOR_SPACE,
        };
        unsafe {
            video_context.VideoProcessorSetStreamFrameFormat(
                &processor,
                0,
                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            );
            video_context.VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&dest));
            video_context.VideoProcessorSetOutputTargetRect(&processor, true, Some(&dest));
            video_context.VideoProcessorSetStreamColorSpace(&processor, 0, &input_color);
            video_context.VideoProcessorSetOutputColorSpace(&processor, &output_color);
        }

        let input_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV {
                    MipSlice: 0,
                    ArraySlice: 0,
                },
            },
        };
        let (scratch, scratch_view) = create_scratch(
            device,
            &video_device,
            &enumerator,
            &input_desc,
            width,
            height,
        )?;

        let output_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
            },
        };
        let mut slots = Vec::with_capacity(SLOT_COUNT);
        for _ in 0..SLOT_COUNT {
            slots.push(create_slot(
                device,
                &video_device,
                &enumerator,
                &output_desc,
                out_width,
                out_height,
            )?);
        }

        Ok(SurfaceConverter {
            context: context.clone(),
            video_device,
            video_context,
            enumerator,
            processor,
            scratch,
            scratch_view,
            input_desc,
            slots,
            next: 0,
            direct_input: None,
        })
    }

    /// 把这一帧的 BGRA 纹理转进当前 NV12 槽位，返回槽位编号。
    /// 必须在 `ReleaseFrame` 之前调用。
    pub fn convert(&mut self, source: &ID3D11Texture2D) -> Result<usize, String> {
        let slot = self.next.min(self.slots.len().saturating_sub(1));

        // 先试着直接给源纹理建输入视图。桌面复制纹理通常没有所需
        // 绑定标志，第一次失败后就固定走中转纹理，不每帧重试。
        let mut direct_view: Option<ID3D11VideoProcessorInputView> = None;
        if self.direct_input != Some(false) {
            let resource: ID3D11Resource = source
                .cast()
                .map_err(|e| format!("采集纹理不是资源: {e}"))?;
            let mut view = None;
            let built = unsafe {
                self.video_device.CreateVideoProcessorInputView(
                    &resource,
                    &self.enumerator,
                    &self.input_desc,
                    Some(&mut view),
                )
            };
            if built.is_ok() && view.is_some() {
                self.direct_input = Some(true);
                direct_view = view;
            } else {
                self.direct_input = Some(false);
            }
        }

        let input_view = match direct_view {
            Some(view) => view,
            None => {
                // 一次 GPU 内拷贝，不读回 CPU。
                unsafe { self.context.CopyResource(&self.scratch, source) };
                self.scratch_view.clone()
            }
        };

        let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: TRUE,
            OutputIndex: 0,
            InputFrameOrField: 0,
            PastFrames: 0,
            FutureFrames: 0,
            ppPastSurfaces: std::ptr::null_mut(),
            pInputSurface: ManuallyDrop::new(Some(input_view)),
            ppFutureSurfaces: std::ptr::null_mut(),
            ppPastSurfacesRight: std::ptr::null_mut(),
            pInputSurfaceRight: ManuallyDrop::new(None),
            ppFutureSurfacesRight: std::ptr::null_mut(),
        };
        let result = unsafe {
            self.video_context.VideoProcessorBlt(
                &self.processor,
                &self.slots[slot].output_view,
                0,
                std::slice::from_ref(&stream),
            )
        };
        // 结构里的接口指针是借给 API 的（windows crate 用 ManuallyDrop 表示），
        // 调用完由我们自己释放，不然每帧都会漏一个 COM 引用。
        unsafe {
            drop(ManuallyDrop::take(&mut stream.pInputSurface));
            drop(ManuallyDrop::take(&mut stream.pInputSurfaceRight));
        }
        result.map_err(|e| format!("GPU 内 BGRA→NV12 失败: {e}"))?;
        Ok(slot)
    }

    /// 编码器收下这一帧后再调用；没收下就不要前进，下一帧覆盖同一槽位是安全的。
    pub fn advance(&mut self) {
        if !self.slots.is_empty() {
            self.next = (self.next + 1) % self.slots.len();
        }
    }

    pub fn surface(&self, slot: usize) -> &ID3D11Texture2D {
        &self.slots[slot.min(self.slots.len().saturating_sub(1))].texture
    }
}

/// 视频处理器输入/输出视图只要渲染目标就够；部分编码器还会用 SRV 读 NV12。
/// 组合标志建不出来时退回只有渲染目标，避免因此整条零拷贝不可用。
const PREFERRED_BINDS: u32 =
    D3D11_BIND_RENDER_TARGET.0 as u32 | D3D11_BIND_SHADER_RESOURCE.0 as u32;
const MINIMAL_BINDS: u32 = D3D11_BIND_RENDER_TARGET.0 as u32;

fn create_texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
    format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
    bind_flags: u32,
) -> Result<ID3D11Texture2D, String> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: bind_flags,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut texture: Option<ID3D11Texture2D> = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }
        .map_err(|e| format!("建不了 GPU 纹理: {e}"))?;
    texture.ok_or_else(|| "GPU 纹理为空".into())
}

fn create_scratch(
    device: &ID3D11Device,
    video_device: &ID3D11VideoDevice,
    enumerator: &ID3D11VideoProcessorEnumerator,
    input_desc: &D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
    width: u32,
    height: u32,
) -> Result<(ID3D11Texture2D, ID3D11VideoProcessorInputView), String> {
    let mut first_error = None;
    for binds in [PREFERRED_BINDS, MINIMAL_BINDS] {
        let texture = match create_texture(device, width, height, DXGI_FORMAT_B8G8R8A8_UNORM, binds)
        {
            Ok(texture) => texture,
            Err(e) => {
                first_error.get_or_insert(e);
                continue;
            }
        };
        match create_input_view(video_device, enumerator, &texture, input_desc) {
            Ok(view) => return Ok((texture, view)),
            Err(e) => {
                first_error.get_or_insert(e);
            }
        }
    }
    Err(first_error.unwrap_or_else(|| "建不了 BGRA 中转纹理".into()))
}

fn create_slot(
    device: &ID3D11Device,
    video_device: &ID3D11VideoDevice,
    enumerator: &ID3D11VideoProcessorEnumerator,
    output_desc: &D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
    width: u32,
    height: u32,
) -> Result<Slot, String> {
    let mut first_error = None;
    for binds in [PREFERRED_BINDS, MINIMAL_BINDS] {
        let texture = match create_texture(device, width, height, DXGI_FORMAT_NV12, binds) {
            Ok(texture) => texture,
            Err(e) => {
                first_error.get_or_insert(e);
                continue;
            }
        };
        let resource: ID3D11Resource = match texture.cast() {
            Ok(resource) => resource,
            Err(e) => {
                first_error.get_or_insert(format!("NV12 纹理不是资源: {e}"));
                continue;
            }
        };
        let mut view: Option<ID3D11VideoProcessorOutputView> = None;
        match unsafe {
            video_device.CreateVideoProcessorOutputView(
                &resource,
                enumerator,
                output_desc,
                Some(&mut view),
            )
        } {
            Ok(()) => {
                if let Some(output_view) = view {
                    return Ok(Slot {
                        texture,
                        output_view,
                    });
                }
                first_error.get_or_insert("NV12 输出视图为空".to_string());
            }
            Err(e) => {
                first_error.get_or_insert(format!("建不了 NV12 输出视图: {e}"));
            }
        }
    }
    Err(first_error.unwrap_or_else(|| "建不了 NV12 纹理".into()))
}

fn create_input_view(
    video_device: &ID3D11VideoDevice,
    enumerator: &ID3D11VideoProcessorEnumerator,
    texture: &ID3D11Texture2D,
    desc: &D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
) -> Result<ID3D11VideoProcessorInputView, String> {
    let resource: ID3D11Resource = texture.cast().map_err(|e| format!("纹理不是资源: {e}"))?;
    let mut view: Option<ID3D11VideoProcessorInputView> = None;
    unsafe {
        video_device.CreateVideoProcessorInputView(&resource, enumerator, desc, Some(&mut view))
    }
    .map_err(|e| format!("建不了视频处理器输入视图: {e}"))?;
    view.ok_or_else(|| "视频处理器输入视图为空".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_color_space_is_bt709_limited() {
        // Usage=0、RGB_Range=0、YCbCr_Matrix=1、Nominal_Range=1。
        assert_eq!(NV12_OUTPUT_COLOR_SPACE & 1, 0);
        assert_eq!((NV12_OUTPUT_COLOR_SPACE >> 2) & 1, 1);
        assert_eq!((NV12_OUTPUT_COLOR_SPACE >> 4) & 3, 1);
    }
}
