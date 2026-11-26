//! Windows 采集。
//!
//! 默认路线是 DXGI 桌面复制拿到 D3D11 纹理，在 GPU 上用视频处理器
//! 转成 NV12，再通过 Media Foundation 的 D3D 设备管理器直接交给 H.264
//! 编码器。默认路径不 `Map` 回 CPU。
//!
//! **状态：代码写完了，没有在任何一台 Windows 机器上跑过。**
//! 纯数据的两段（AVCC 转 Annex-B、BGRA 转 NV12）在别的平台有测试，
//! 剩下的是 COM 调用顺序和 HRESULT 处理，只有真机能验。
//!
//! 几条保底规则：
//!
//! - 零拷贝是默认路径，但每一步都可能失败：设备没有视频接口、驱动不收
//!   BGRA/NV12、编码器不认 D3D 设备管理器、编码器拒收 D3D 表面。
//!   这些情况都打日志并回退到「拷到 staging → `Map` → CPU 转 NV12」。
//!   回退不能把整条采集拖死。
//! - 桌面复制给的是 BGRA，编码器吃 NV12，所以 GPU 路径至少有一次格式
//!   转换。多数机器还需要一次 GPU 内 `CopyResource` 到带绑定标志的纹理；
//!   这只发生在显卡上，不回读 CPU。
//! - 码率通过输出类型的 `MF_MT_AVG_BITRATE` 给，没有用 ICodecAPI。
//! - IDR 是从输出里扫 NAL 类型认出来的，不是读 `MFSampleExtension_CleanPoint`。
//!
//! 恢复路径：`AcquireNextFrame` 返回 `DXGI_ERROR_ACCESS_LOST`（桌面切换、
//! 分辨率变化、锁屏）时整条链重建一次，不退出。

#[path = "d3d.rs"]
mod d3d;

use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::core::{Interface, GUID};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0;
use windows::Win32::Graphics::Direct3D10::ID3D10Multithread;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_MAPPED_SUBRESOURCE,
    D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_MODE_ROTATION_IDENTITY, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGIAdapter, IDXGIDevice, IDXGIOutput, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
    DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
    DXGI_OUTDUPL_POINTER_SHAPE_INFO,
};
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFDXGIDeviceManager, IMFMediaBuffer, IMFMediaType, IMFSample, IMFTransform,
    MFCreateDXGIDeviceManager, MFCreateDXGISurfaceBuffer, MFCreateMediaType, MFCreateMemoryBuffer,
    MFCreateSample, MFMediaType_Video, MFShutdown, MFStartup, MFVideoFormat_H264,
    MFVideoFormat_NV12, MFVideoInterlace_Progressive, MFT_CATEGORY_VIDEO_ENCODER,
    MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_LOCALMFT, MFT_ENUM_FLAG_SORTANDFILTER,
    MFT_ENUM_FLAG_SYNCMFT, MFT_MESSAGE_COMMAND_FLUSH, MFT_MESSAGE_SET_D3D_MANAGER,
    MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFT_REGISTER_TYPE_INFO,
    MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE, MF_MT_AVG_BITRATE,
    MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE,
    MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MF_VERSION,
};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};

use crate::h264::{avcc_to_annex_b, bgra_to_nv12, even_size};
use crate::AccessUnit;

/// 一帧最多等这么久。返回超时是正常情况：桌面没变就没有新帧。
const FRAME_TIMEOUT_MS: u32 = 2;
/// 时间戳单位是 100 纳秒。
const HNS_PER_SEC: i64 = 10_000_000;
/// 目标帧率。
const DEFAULT_FPS: u32 = 60;
/// 未指定码率时的兜底。
const DEFAULT_BITRATE: i32 = 6_000_000;
/// 运行中掉线最多重建这么多次。每轮间隔 500ms，超过就交回节点重启。
const MAX_REBUILDS: u32 = 10;
/// 一段重建好的链路跑过这么久，就认为上一个错误是偶发的，重新计数。
const HEALTHY_RUN: Duration = Duration::from_secs(5);

static RUNNING: AtomicBool = AtomicBool::new(false);
/// 采集已经停了且不会自己回来。节点看到后按有限次数重启，
/// 而不是对着一个永远不出的画面干等。
static FAILED: AtomicBool = AtomicBool::new(false);
/// 当前编码器是不是硬编。没有硬编才退回软编，节点要提示一次。
static HARDWARE: AtomicBool = AtomicBool::new(false);
static FORCE_IDR: AtomicBool = AtomicBool::new(false);
static BITRATE: AtomicI32 = AtomicI32::new(DEFAULT_BITRATE);
static FPS: AtomicI32 = AtomicI32::new(DEFAULT_FPS as i32);
/// 最近一次成功转换的指针形状。DXGI 只在形状变化时报告新数据，
/// 缓冲大小为 0 的一轮表示没变，保留这里的值。
static LAST_SHAPE: Mutex<Option<jr_proto::CursorShape>> = Mutex::new(None);

pub struct Session {
    stop: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

/// 起采集。返回 Ok 表示设备、复制和编码器都建起来了，画面要等第一帧。
/// 任何一步失败都返回人话错误，节点照原样显示，不再包装。
pub fn start(
    bitrate: i32,
    display_index: i32,
    mut sink: Box<dyn FnMut(AccessUnit) + Send>,
) -> Result<Session, String> {
    if RUNNING.swap(true, Ordering::SeqCst) {
        return Err("采集已经在跑了".into());
    }
    FAILED.store(false, Ordering::SeqCst);
    HARDWARE.store(false, Ordering::SeqCst);
    BITRATE.store(
        if bitrate > 0 {
            bitrate
        } else {
            DEFAULT_BITRATE
        },
        Ordering::Relaxed,
    );
    let stop = Arc::new(AtomicBool::new(false));
    let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
    let stop_thread = stop.clone();
    let join = std::thread::Builder::new()
        .name("jrp-capture".into())
        .spawn(move || {
            // MF 和 D3D 都要求这个线程先声明是 MTA。
            let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
            if let Err(e) = unsafe { MFStartup(MF_VERSION, 0) } {
                let _ = ready_tx.send(Err(format!("媒体框架起不来: {e}")));
                FAILED.store(true, Ordering::SeqCst);
                RUNNING.store(false, Ordering::SeqCst);
                return;
            }
            let mut first = true;
            let mut rebuilds = 0u32;
            loop {
                let began = Instant::now();
                let r = capture_loop(
                    &stop_thread,
                    &mut sink,
                    &ready_tx,
                    &mut first,
                    display_index,
                );
                if stop_thread.load(Ordering::Relaxed) {
                    break;
                }
                match r {
                    Ok(()) => break,
                    Err(e) => {
                        if first {
                            let _ = ready_tx.send(Err(e));
                            FAILED.store(true, Ordering::SeqCst);
                            break;
                        }
                        // 设备在跑之后掉线：先等一会儿重建整条链。
                        // 连续重建失败说明不是一次热插拔，交回节点做有限次重启。
                        if began.elapsed() >= HEALTHY_RUN {
                            rebuilds = 0;
                        }
                        rebuilds = rebuilds.saturating_add(1);
                        if rebuilds >= MAX_REBUILDS {
                            eprintln!(
                                "node: 采集连续重建 {rebuilds} 次失败，停止并等待节点重启: {e}"
                            );
                            FAILED.store(true, Ordering::SeqCst);
                            break;
                        }
                        eprintln!("node: 采集重建中（第 {rebuilds} 次）{e}");
                        std::thread::sleep(Duration::from_millis(500));
                    }
                }
            }
            unsafe {
                let _ = MFShutdown();
                if com.is_ok() {
                    CoUninitialize();
                }
            }
            RUNNING.store(false, Ordering::SeqCst);
        })
        .map_err(|e| format!("采集线程起不来: {e}"))?;

    match ready_rx.recv_timeout(std::time::Duration::from_secs(5)) {
        Ok(Ok(())) => Ok(Session {
            stop,
            join: Some(join),
        }),
        Ok(Err(e)) => Err(e),
        Err(_) => {
            // 初始化超时后不能再把线程留在后台自己跑：调用方拿不到
            // Session，就没法停它，下一次采集还会撞上“已经在跑”。
            stop.store(true, Ordering::SeqCst);
            Err("采集初始化超时".into())
        }
    }
}

pub fn stop(mut session: Session) {
    session.stop.store(true, Ordering::Relaxed);
    if let Some(join) = session.join.take() {
        let _ = join.join();
    }
    // 用户主动停不叫失败，下一次启动要从干净状态开始。
    FAILED.store(false, Ordering::SeqCst);
}

/// 没有 Session 可停时也要能清掉失败标记。启动失败或初始化超时后
/// 采集线程可能已经退出，但调用方手里没有 handle。
pub fn clear_failed() {
    FAILED.store(false, Ordering::SeqCst);
}

pub fn running() -> bool {
    RUNNING.load(Ordering::Relaxed)
}

pub fn failed() -> bool {
    FAILED.load(Ordering::Relaxed)
}

pub fn hardware() -> Option<bool> {
    if RUNNING.load(Ordering::Relaxed) {
        Some(HARDWARE.load(Ordering::Relaxed))
    } else {
        None
    }
}

/// 让下一帧一定是 IDR。窗口刚打开或者丢包之后要走一次。
pub fn force_idr() {
    FORCE_IDR.store(true, Ordering::Relaxed);
}

/// 改码率。下一条采集循环会重建编码器，采集本身不停。
pub fn set_bitrate(bitrate: i32) {
    if bitrate > 0 {
        BITRATE.store(bitrate, Ordering::Relaxed);
    }
}

/// 改帧率。和码率一样，靠重建编码器生效。
pub fn set_fps(fps: u16) {
    if fps > 0 {
        FPS.store(i32::from(fps.clamp(1, 120)), Ordering::Relaxed);
    }
}

/// 最近一次抓到的光标形状。主控按 hash 去重，这里不做节流。
pub fn cursor_shape() -> Option<jr_proto::CursorShape> {
    LAST_SHAPE.lock().ok().and_then(|shape| shape.clone())
}

/// 采集重建时扔掉旧形状，别让主控继续画上一块屏的光标。
fn clear_cursor_shape() {
    if let Ok(mut shape) = LAST_SHAPE.lock() {
        *shape = None;
    }
}

/// 光标在桌面全局坐标系里的位置和可见性。
/// 和 macOS 的 `CGEventGetLocation` 一样用全局坐标，节点原样发给主控。
/// 不用 `DXGI_OUTDUPL_POINTER_POSITION`：那是这块输出的局部坐标。
pub fn cursor_state() -> Option<(i32, i32, bool)> {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetCursorInfo, GetCursorPos, CURSORINFO, CURSOR_SHOWING,
    };

    let mut pos = POINT::default();
    unsafe { GetCursorPos(&mut pos) }.ok()?;
    let mut info = CURSORINFO {
        cbSize: std::mem::size_of::<CURSORINFO>() as u32,
        ..Default::default()
    };
    // 查询失败时按不可见处理，位置本身仍然有效。
    let visible =
        unsafe { GetCursorInfo(&mut info) }.is_ok() && info.flags.0 & CURSOR_SHOWING.0 != 0;
    Some((pos.x, pos.y, visible))
}

/// 取这一帧的指针形状。必须在 `ReleaseFrame` 之前调用，
/// 形状缓冲在释放帧之后就失效了。
fn update_pointer_shape(dupl: &IDXGIOutputDuplication, info: &DXGI_OUTDUPL_FRAME_INFO) {
    if info.PointerShapeBufferSize == 0 {
        // 这次没有新形状，保留上一次。
        return;
    }
    let mut buf = vec![0u8; info.PointerShapeBufferSize as usize];
    let mut required = 0u32;
    let mut raw = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
    let got = unsafe {
        dupl.GetFramePointerShape(
            buf.len() as u32,
            buf.as_mut_ptr().cast(),
            &mut required,
            &mut raw,
        )
    };
    if got.is_err() {
        // 指针隐藏或形状还在变时可能失败，不影响画面。
        return;
    }
    let Some(shape_type) = crate::pointer::ShapeType::from_dxgi(raw.Type) else {
        return;
    };
    let shape_info = crate::pointer::ShapeInfo {
        shape_type,
        width: raw.Width,
        height: raw.Height,
        pitch: raw.Pitch,
        hotspot_x: raw.HotSpot.x,
        hotspot_y: raw.HotSpot.y,
    };
    if let Some(shape) = crate::pointer::convert(&shape_info, &buf) {
        if let Ok(mut last) = LAST_SHAPE.lock() {
            *last = Some(shape);
        }
    }
    // 转换不了（太大、格式不认识、需要桌面反色）就跳过这一块，
    // 保留上一次，不拿假数据顶替。
}

fn capture_loop(
    stop: &Arc<AtomicBool>,
    sink: &mut Box<dyn FnMut(AccessUnit) + Send>,
    ready: &mpsc::Sender<Result<(), String>>,
    first: &mut bool,
    display_index: i32,
) -> Result<(), String> {
    // 重建时先扔掉上一块屏留下的光标形状。
    clear_cursor_shape();
    let (device, context, output, gpu_safe) = open_output(display_index)?;
    let output1: IDXGIOutput1 = output
        .cast()
        .map_err(|e| format!("输出不支持桌面复制: {e}"))?;
    let dupl =
        unsafe { output1.DuplicateOutput(&device) }.map_err(|e| format!("桌面复制起不来: {e}"))?;
    let desc = unsafe { dupl.GetDesc() };
    let width = desc.ModeDesc.Width;
    let height = desc.ModeDesc.Height;
    if width == 0 || height == 0 {
        return Err("这块屏没有有效尺寸".into());
    }
    if desc.Rotation != DXGI_MODE_ROTATION_IDENTITY {
        // 旋转过的屏给的是未旋转的纹理，直接转码画面会是歪的。
        // 第一版不做旋转，报清楚，不糊过去。
        return Err("这块屏设了旋转，第一版还不支持".into());
    }

    let staging = create_staging(&device, width, height)?;
    let mut applied_bitrate = BITRATE.load(Ordering::Relaxed).max(500_000);
    let mut applied_fps = FPS.load(Ordering::Relaxed).clamp(1, 120) as u32;
    let mut frame_hns = HNS_PER_SEC / applied_fps as i64;

    // 零拷贝不是启动前提。转换链任何一步失败都退到 staging 回读；
    // staging 先建好，运行中掉回 CPU 时不用再分配。
    let mut converter = if gpu_safe {
        match d3d::SurfaceConverter::new(&device, &context, width, height, applied_fps) {
            Ok(converter) => {
                println!("node: Windows 采集启用 GPU 零拷贝（BGRA→NV12 在显卡上完成）");
                Some(converter)
            }
            Err(e) => {
                println!("node: GPU 零拷贝不可用，回退 CPU 回读: {e}");
                None
            }
        }
    } else {
        println!("node: D3D11 设备不能安全共享给编码器，回退 CPU 回读");
        None
    };
    let mut encoder = Encoder::new(width, height, &device, converter.is_some())?;
    if !encoder.zero_copy {
        // 编码器不认 D3D 设备管理器时，转换链留着也没用。
        converter = None;
    }
    if *first {
        *first = false;
        let _ = ready.send(Ok(()));
        println!(
            "node: Windows 采集就绪 {width}x{height}，编码器 {}，{}",
            encoder.name,
            if encoder.zero_copy {
                "GPU 零拷贝"
            } else {
                "CPU 回读"
            }
        );
    }

    let mut nv12: Vec<u8> = Vec::new();
    let mut ts: i64 = 0;

    while !stop.load(Ordering::Relaxed) {
        // 运行中改档：重建编码器只影响编码这一段，桌面复制不用重来。
        let want_bitrate = BITRATE.load(Ordering::Relaxed).max(500_000);
        let want_fps = FPS.load(Ordering::Relaxed).clamp(1, 120) as u32;
        if want_bitrate != applied_bitrate || want_fps != applied_fps {
            match Encoder::new(width, height, &device, converter.is_some()) {
                Ok(next) => {
                    println!("node: Windows 编码器切换到 {want_bitrate} bps / {want_fps} fps");
                    if converter.is_some() && !next.zero_copy {
                        // 新编码器不认 D3D 表面，后续帧走 CPU 回读。
                        converter = None;
                    }
                    encoder = next;
                    // 新编码器要从 IDR 重新开始，否则对端解不出参考帧。
                    FORCE_IDR.store(true, Ordering::Relaxed);
                }
                Err(e) => println!("node: 切换编码参数失败，继续用旧参数 {e}"),
            }
            applied_bitrate = want_bitrate;
            applied_fps = want_fps;
            frame_hns = HNS_PER_SEC / applied_fps as i64;
        }
        // 桌面帧信息里还带着指针变化；形状必须在 ReleaseFrame 之前取。
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        match unsafe { dupl.AcquireNextFrame(FRAME_TIMEOUT_MS, &mut info, &mut resource) } {
            Ok(()) => {}
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => continue,
            Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                return Err("桌面被切换或分辨率变了，重建".into());
            }
            Err(e) => return Err(format!("取帧失败: {e}")),
        }
        update_pointer_shape(&dupl, &info);
        let Some(resource) = resource else {
            // 只有指针变化、没有新桌面图时 resource 是 None。AcquireNextFrame
            // 成功过就必须 ReleaseFrame，否则下一轮会一直失败。
            let _ = unsafe { dupl.ReleaseFrame() };
            continue;
        };
        let texture: ID3D11Texture2D = match resource.cast() {
            Ok(texture) => texture,
            Err(e) => {
                let _ = unsafe { dupl.ReleaseFrame() };
                return Err(format!("取到的不是纹理: {e}"));
            }
        };

        // 转换必须赶在 ReleaseFrame 之前：源纹理只在释放前有效。
        let conversion = match converter.as_mut() {
            Some(pipeline) => Some(pipeline.convert(&texture)),
            None => None,
        };
        let mut surface_slot = None;
        match conversion {
            Some(Ok(slot)) => surface_slot = Some(slot),
            Some(Err(e)) => {
                println!("node: GPU 零拷贝转换失败，回退 CPU 回读: {e}");
                if let Err(rebuild) =
                    fallback_to_cpu(width, height, &device, &mut converter, &mut encoder)
                {
                    let _ = unsafe { dupl.ReleaseFrame() };
                    return Err(rebuild);
                }
            }
            None => {}
        }

        // 只有零拷贝不可用时才回读 CPU；运行中掉回这里也能拿到当前帧。
        if surface_slot.is_none() {
            unsafe { context.CopyResource(&staging, &texture) };
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            if let Err(e) =
                unsafe { context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) }
            {
                let _ = unsafe { dupl.ReleaseFrame() };
                return Err(format!("纹理读不回来: {e}"));
            }
            let pitch = mapped.RowPitch as usize;
            {
                let bytes = unsafe {
                    std::slice::from_raw_parts(mapped.pData as *const u8, pitch * height as usize)
                };
                bgra_to_nv12(bytes, pitch, width as usize, height as usize, &mut nv12);
            }
            unsafe { context.Unmap(&staging, 0) };
        }

        // 必须在下一帧等待之前释放，忘了就会静默停在最后一帧。
        let _ = unsafe { dupl.ReleaseFrame() };

        if FORCE_IDR.swap(false, Ordering::Relaxed) {
            // FLUSH 之后编码器下一帧会重新出 IDR。
            let _ = unsafe {
                encoder
                    .transform
                    .ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0)
            };
        }

        if let Some(slot) = surface_slot {
            let first = match converter.as_ref() {
                Some(pipeline) => encoder.feed_surface(pipeline.surface(slot), ts, frame_hns),
                None => Ok(false),
            };
            match first {
                Ok(true) => {
                    if let Some(pipeline) = converter.as_mut() {
                        pipeline.advance();
                    }
                }
                Ok(false) => {
                    // 编码器还占着，先把算完的输出取走再喂一次。
                    encoder.drain(sink, ts as u64 / 10)?;
                    let retry = match converter.as_ref() {
                        Some(pipeline) => {
                            encoder.feed_surface(pipeline.surface(slot), ts, frame_hns)
                        }
                        None => Ok(false),
                    };
                    match retry {
                        Ok(true) => {
                            if let Some(pipeline) = converter.as_mut() {
                                pipeline.advance();
                            }
                        }
                        // 还占着就丢这一帧；槽位没前进，下一帧覆盖它是安全的。
                        Ok(false) => {}
                        Err(e) => {
                            println!("node: 编码器拒收 D3D 表面，回退 CPU 回读: {e}");
                            if let Err(rebuild) = fallback_to_cpu(
                                width,
                                height,
                                &device,
                                &mut converter,
                                &mut encoder,
                            ) {
                                return Err(rebuild);
                            }
                        }
                    }
                }
                Err(e) => {
                    println!("node: 编码器拒收 D3D 表面，回退 CPU 回读: {e}");
                    if let Err(rebuild) =
                        fallback_to_cpu(width, height, &device, &mut converter, &mut encoder)
                    {
                        return Err(rebuild);
                    }
                }
            }
        } else if !encoder.feed(&nv12, ts, frame_hns)? {
            // 编码器还占着，先把它算完的输出取走，再喂这一帧。
            encoder.drain(sink, ts as u64 / 10)?;
            if !encoder.feed(&nv12, ts, frame_hns)? {
                // 还是占着，说明编码器跟不上，这一帧丢掉，下一帧再来。
            }
        }
        ts += frame_hns;
        encoder.drain(sink, ts as u64 / 10)?;
    }
    Ok(())
}

/// 零拷贝链在中途失败时切回 CPU 回读。换一颗没有 D3D 设备管理器的
/// 编码器，避免硬件编码器继续期待 D3D 表面。这一帧由调用方按 CPU 路径处理，
/// 或者直接丢掉；下一帧一定走回读。
fn fallback_to_cpu(
    width: u32,
    height: u32,
    device: &ID3D11Device,
    converter: &mut Option<d3d::SurfaceConverter>,
    encoder: &mut Encoder,
) -> Result<(), String> {
    converter.take();
    *encoder = Encoder::new(width, height, device, false)
        .map_err(|e| format!("回退 CPU 回读时重建编码器失败: {e}"))?;
    FORCE_IDR.store(true, Ordering::Relaxed);
    Ok(())
}

fn open_output(
    display_index: i32,
) -> Result<(ID3D11Device, ID3D11DeviceContext, IDXGIOutput, bool), String> {
    // 视频处理器要 VIDEO_SUPPORT。老驱动可能不认这个标志，那就退回
    // 只带 BGRA 的设备；采集还能走 CPU 回读，不是启动失败。
    let (device, context, gpu_safe) =
        match create_device(D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT) {
            Ok(pair) => pair,
            Err(first) => {
                println!("node: D3D11 视频支持起不来，零拷贝不可用，按回读路径继续: {first}");
                let (device, context, _) = create_device(D3D11_CREATE_DEVICE_BGRA_SUPPORT)?;
                (device, context, false)
            }
        };
    let dxgi: IDXGIDevice = device
        .cast()
        .map_err(|e| format!("设备不是 DXGI 设备: {e}"))?;
    let adapter: IDXGIAdapter =
        unsafe { dxgi.GetAdapter() }.map_err(|e| format!("拿不到显卡: {e}"))?;
    let output: IDXGIOutput = unsafe { adapter.EnumOutputs(display_index.max(0) as u32) }
        .map_err(|e| format!("没有第 {display_index} 块屏: {e}"))?;
    Ok((device, context, output, gpu_safe))
}

fn create_device(
    flags: windows::Win32::Graphics::Direct3D11::D3D11_CREATE_DEVICE_FLAG,
) -> Result<(ID3D11Device, ID3D11DeviceContext, bool), String> {
    let mut device: Option<ID3D11Device> = None;
    let mut context = None;
    let levels = [D3D_FEATURE_LEVEL_11_0];
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            flags,
            Some(&levels),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
        .map_err(|e| format!("建不了 D3D11 设备: {e}"))?;
    }
    let device = device.ok_or("D3D11 没有给出设备")?;
    let context = context.ok_or("D3D11 没有给出上下文")?;
    // Media Foundation 的硬件编码器会在自己的工作线程读同一台设备上的
    // 纹理。D3D11 默认不替跨线程访问加锁，必须在把设备交给 MFT 之前
    // 打开 multithread protected，否则 GPU 零拷贝会出现撕裂帧甚至驱动崩溃。
    let gpu_safe = if let Ok(mt) = device.cast::<ID3D10Multithread>() {
        unsafe {
            let _ = mt.SetMultithreadProtected(true);
        }
        true
    } else {
        // 没有跨线程保护时不能共享 GPU 表面，但桌面复制和 CPU 回读仍可用。
        false
    };
    Ok((device, context, gpu_safe))
}

fn create_staging(
    device: &ID3D11Device,
    width: u32,
    height: u32,
) -> Result<ID3D11Texture2D, String> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    let mut tex: Option<ID3D11Texture2D> = None;
    unsafe {
        device
            .CreateTexture2D(&desc, None, Some(&mut tex))
            .map_err(|e| format!("建不了回读纹理: {e}"))?;
    }
    tex.ok_or_else(|| "回读纹理为空".to_string())
}

/// 编码器外面包一层，管住输入样本和输出轮询。
struct Encoder {
    transform: IMFTransform,
    name: String,
    provides_samples: bool,
    /// 编码器是否已经接上 D3D 设备管理器，可以直接吃 GPU 上的 NV12 纹理。
    zero_copy: bool,
    /// 编码器可能只引用设备管理器而不 AddRef，必须由我们持有到编码器结束。
    _d3d_manager: Option<IMFDXGIDeviceManager>,
}

impl Encoder {
    fn new(
        width: u32,
        height: u32,
        device: &ID3D11Device,
        want_zero_copy: bool,
    ) -> Result<Self, String> {
        let bitrate = BITRATE.load(Ordering::Relaxed).max(500_000) as u32;
        let fps = FPS.load(Ordering::Relaxed).clamp(1, 120) as u32;
        // 编码器要求宽高是偶数，NV12 纹理也一样。
        let (width, height) = even_size(width, height);
        let out_type = unsafe { MFCreateMediaType() }.map_err(|e| e.to_string())?;
        set_guid(&out_type, &MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        set_guid(&out_type, &MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
        set_u32(&out_type, &MF_MT_AVG_BITRATE, bitrate)?;
        set_u64(&out_type, &MF_MT_FRAME_SIZE, pack2(width, height))?;
        set_u64(&out_type, &MF_MT_FRAME_RATE, pack2(fps, 1))?;
        set_u64(&out_type, &MF_MT_PIXEL_ASPECT_RATIO, pack2(1, 1))?;
        set_u32(
            &out_type,
            &MF_MT_INTERLACE_MODE,
            MFVideoInterlace_Progressive.0 as u32,
        )?;

        let in_type = unsafe { MFCreateMediaType() }.map_err(|e| e.to_string())?;
        set_guid(&in_type, &MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        set_guid(&in_type, &MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
        set_u64(&in_type, &MF_MT_FRAME_SIZE, pack2(width, height))?;
        set_u64(&in_type, &MF_MT_FRAME_RATE, pack2(fps, 1))?;
        set_u64(&in_type, &MF_MT_PIXEL_ASPECT_RATIO, pack2(1, 1))?;
        set_u32(
            &in_type,
            &MF_MT_INTERLACE_MODE,
            MFVideoInterlace_Progressive.0 as u32,
        )?;

        // 先挑硬编，没有再退回全部 MFT。
        // 先要硬编，没有再看全部。这条顺序是有意为之，不靠显卡品牌字符串判断。
        let (transform, name, hardware) = match pick_encoder(true) {
            Some((transform, name)) => (transform, name, true),
            None => {
                let (transform, name) = pick_encoder(false).ok_or("系统里没有 H.264 编码器")?;
                (transform, name, false)
            }
        };
        HARDWARE.store(hardware, Ordering::Relaxed);
        // 输出类型要先设，编码器会照着它谈输入格式。
        unsafe {
            transform
                .SetOutputType(0, &out_type, 0)
                .map_err(|e| format!("编码器不收这个输出格式: {e}"))?;
            transform
                .SetInputType(0, &in_type, 0)
                .map_err(|e| format!("编码器不收 NV12 输入: {e}"))?;
        }

        // 输入类型谈好之后再挂 D3D 设备管理器。任何一步失败都只关掉
        // 零拷贝，不改编码器本身，调用方继续用 CPU 样本喂它。
        let mut d3d_manager = None;
        let mut zero_copy = false;
        if want_zero_copy {
            match attach_d3d_manager(&transform, device) {
                Ok(manager) => {
                    d3d_manager = Some(manager);
                    zero_copy = true;
                }
                Err(e) => {
                    println!("node: 编码器不接受 D3D 设备管理器，回退 CPU 回读: {e}");
                }
            }
        }

        let info = unsafe { transform.GetOutputStreamInfo(0) }
            .map_err(|e| format!("问不到输出信息: {e}"))?;
        let provides_samples = info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
        Ok(Encoder {
            transform,
            name,
            provides_samples,
            zero_copy,
            _d3d_manager: d3d_manager,
        })
    }

    /// Ok(true) 表示编码器收下了这一帧；Ok(false) 表示它还在算上一帧，
    /// 调用方要先 drain 再喂回来，而不是把这一帧丢掉。
    fn feed(&mut self, nv12: &[u8], ts: i64, dur: i64) -> Result<bool, String> {
        let sample = make_sample(nv12, ts, dur)?;
        match unsafe { self.transform.ProcessInput(0, &sample, 0) } {
            Ok(()) => Ok(true),
            Err(e) if e.code() == windows::Win32::Media::MediaFoundation::MF_E_NOTACCEPTING => {
                Ok(false)
            }
            Err(e) => Err(format!("喂帧失败: {e}")),
        }
    }

    /// 把 GPU 上的 NV12 纹理包成 MF 样本喂给编码器。样本持有纹理引用，
    /// 编码器没收下之前不能覆盖同一张纹理（`d3d::SurfaceConverter` 用槽位轮转）。
    fn feed_surface(
        &mut self,
        texture: &ID3D11Texture2D,
        ts: i64,
        dur: i64,
    ) -> Result<bool, String> {
        let sample = make_surface_sample(texture, ts, dur)?;
        match unsafe { self.transform.ProcessInput(0, &sample, 0) } {
            Ok(()) => Ok(true),
            Err(e) if e.code() == windows::Win32::Media::MediaFoundation::MF_E_NOTACCEPTING => {
                Ok(false)
            }
            Err(e) => Err(format!("喂 GPU 纹理失败: {e}")),
        }
    }

    /// 把编码器里所有现成的输出取干净。GF(?) 取不到就返回。
    fn drain(
        &mut self,
        sink: &mut Box<dyn FnMut(AccessUnit) + Send>,
        ts_us: u64,
    ) -> Result<(), String> {
        loop {
            let mut out = MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: std::mem::ManuallyDrop::new(if self.provides_samples {
                    None
                } else {
                    Some(make_sample(&[], 0, 0)?)
                }),
                dwStatus: 0,
                pEvents: std::mem::ManuallyDrop::new(None),
            };
            let mut status = 0u32;
            let r = unsafe {
                self.transform
                    .ProcessOutput(0, std::slice::from_mut(&mut out), &mut status)
            };
            // 不管成功还是失败，都要把样本取回来：provides_samples 为假时
            // 那颗样本是我们自己建的，留在 ManuallyDrop 里就永远不会释放。
            let sample = std::mem::ManuallyDrop::into_inner(out.pSample);
            match r {
                Ok(()) => {
                    if let Some(sample) = sample {
                        if let Some(au) = read_sample(&sample, ts_us) {
                            sink(au);
                        }
                    }
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(()),
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    // 编码器换了输出格式。重新谈一次，这一轮不出帧。
                    let again = unsafe { self.transform.GetOutputAvailableType(0, 0) }
                        .map_err(|e| format!("重新谈输出格式失败: {e}"))?;
                    unsafe { self.transform.SetOutputType(0, &again, 0) }
                        .map_err(|e| format!("重新设置输出格式失败: {e}"))?;
                    continue;
                }
                Err(e) => return Err(format!("取编码结果失败: {e}")),
            }
        }
    }
}

fn pick_encoder(hardware_ok: bool) -> Option<(IMFTransform, String)> {
    let info_in = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_NV12,
    };
    let info_out = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };
    // SORTANDFILTER 会把更合适的排在前面，硬编通常就在最前面。
    let mut flags = MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_LOCALMFT | MFT_ENUM_FLAG_SORTANDFILTER;
    if hardware_ok {
        flags |= MFT_ENUM_FLAG_HARDWARE;
    }
    let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    unsafe {
        windows::Win32::Media::MediaFoundation::MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            flags,
            Some(&info_in),
            Some(&info_out),
            &mut activates,
            &mut count,
        )
        .ok()?;
    }
    if count == 0 || activates.is_null() {
        return None;
    }
    let list = unsafe { std::slice::from_raw_parts_mut(activates, count as usize) };
    let mut picked = None;
    for slot in list.iter_mut() {
        if let Some(activate) = slot.take() {
            if let Ok(transform) = unsafe { activate.ActivateObject::<IMFTransform>() } {
                let name = friendly_name(&activate);
                picked = Some((transform, name));
                break;
            }
        }
    }
    unsafe { windows::Win32::System::Com::CoTaskMemFree(Some(activates as *const _)) };
    picked
}

fn friendly_name(activate: &IMFActivate) -> String {
    use windows::Win32::Media::MediaFoundation::MFT_FRIENDLY_NAME_Attribute;
    let mut pw = windows::core::PWSTR::null();
    let mut len = 0u32;
    if unsafe { activate.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut pw, &mut len) }
        .is_err()
    {
        return "未知编码器".into();
    }
    let name = unsafe { pw.to_string() }.unwrap_or_else(|_| "未知编码器".into());
    unsafe { windows::Win32::System::Com::CoTaskMemFree(Some(pw.0 as *const _)) };
    name
}

/// 把 D3D11 设备交给编码器。成功返回的管理器必须活到编码器销毁。
fn attach_d3d_manager(
    transform: &IMFTransform,
    device: &ID3D11Device,
) -> Result<IMFDXGIDeviceManager, String> {
    let mut token = 0u32;
    let mut manager: Option<IMFDXGIDeviceManager> = None;
    unsafe { MFCreateDXGIDeviceManager(&mut token, &mut manager) }
        .map_err(|e| format!("建不了 D3D 设备管理器: {e}"))?;
    let manager = manager.ok_or("D3D 设备管理器为空")?;
    unsafe { manager.ResetDevice(device, token) }
        .map_err(|e| format!("复位 D3D 设备管理器失败: {e}"))?;
    // ulParam 要的是 IMFDXGIDeviceManager 的 IUnknown 指针，不是管理器本身。
    unsafe { transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize) }
        .map_err(|e| format!("编码器不收 D3D 设备管理器: {e}"))?;
    Ok(manager)
}

fn make_sample(bytes: &[u8], ts: i64, dur: i64) -> Result<IMFSample, String> {
    let buffer: IMFMediaBuffer =
        unsafe { MFCreateMemoryBuffer(bytes.len().max(1) as u32) }.map_err(|e| e.to_string())?;
    if !bytes.is_empty() {
        unsafe {
            let mut ptr: *mut u8 = std::ptr::null_mut();
            let mut max = 0u32;
            let mut cur = 0u32;
            buffer
                .Lock(&mut ptr, Some(&mut max), Some(&mut cur))
                .map_err(|e| format!("编码缓冲拿不到: {e}"))?;
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
            let _ = buffer.Unlock();
            buffer
                .SetCurrentLength(bytes.len() as u32)
                .map_err(|e| format!("设置缓冲长度失败: {e}"))?;
        }
    }
    let sample = unsafe { MFCreateSample() }.map_err(|e| e.to_string())?;
    unsafe {
        sample.AddBuffer(&buffer).map_err(|e| e.to_string())?;
        let _ = sample.SetSampleTime(ts);
        let _ = sample.SetSampleDuration(dur);
    }
    Ok(sample)
}

/// 用 GPU 上的 NV12 纹理做样本，不给 CPU 留副本。
/// `MFCreateDXGISurfaceBuffer` 只是把纹理包成 IMFDXGIBuffer；
/// 编码器通过 D3D 设备管理器直接用它。
fn make_surface_sample(texture: &ID3D11Texture2D, ts: i64, dur: i64) -> Result<IMFSample, String> {
    let buffer: IMFMediaBuffer =
        unsafe { MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, texture, 0, false) }
            .map_err(|e| format!("纹理包不成 DXGI 缓冲: {e}"))?;
    let sample = unsafe { MFCreateSample() }.map_err(|e| e.to_string())?;
    unsafe {
        sample.AddBuffer(&buffer).map_err(|e| e.to_string())?;
        let _ = sample.SetSampleTime(ts);
        let _ = sample.SetSampleDuration(dur);
    }
    Ok(sample)
}

fn read_sample(sample: &IMFSample, ts_us: u64) -> Option<AccessUnit> {
    let buffer = unsafe { sample.ConvertToContiguousBuffer() }.ok()?;
    let mut ptr: *mut u8 = std::ptr::null_mut();
    let mut max = 0u32;
    let mut cur = 0u32;
    unsafe {
        buffer.Lock(&mut ptr, Some(&mut max), Some(&mut cur)).ok()?;
        let raw = std::slice::from_raw_parts(ptr, cur as usize);
        let (bytes, idr) = avcc_to_annex_b(raw);
        let _ = buffer.Unlock();
        if bytes.is_empty() {
            return None;
        }
        Some(AccessUnit { idr, bytes, ts_us })
    }
}

fn set_guid(t: &IMFMediaType, key: &GUID, value: &GUID) -> Result<(), String> {
    unsafe { t.SetGUID(key, value) }.map_err(|e| format!("设置媒体类型失败: {e}"))
}

fn set_u32(t: &IMFMediaType, key: &GUID, value: u32) -> Result<(), String> {
    unsafe { t.SetUINT32(key, value) }.map_err(|e| format!("设置媒体类型失败: {e}"))
}

fn set_u64(t: &IMFMediaType, key: &GUID, value: u64) -> Result<(), String> {
    unsafe { t.SetUINT64(key, value) }.map_err(|e| format!("设置媒体类型失败: {e}"))
}

/// 两个 32 位拼成一个 64 位。MF 的尺寸、帧率、像素比都是这么装的。
fn pack2(hi: u32, lo: u32) -> u64 {
    ((hi as u64) << 32) | lo as u64
}
