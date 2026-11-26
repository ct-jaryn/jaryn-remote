//! Windows 音频：WASAPI 回环采集 + Media Foundation AAC 编解码。
//!
//! 对外只有六个函数，节点里的 `av.rs` 直接调用它们：
//!
//! - 采集：默认播放设备上的 WASAPI loopback（系统正在放什么就采什么），
//!   不碰麦克风，也不做回送。
//! - 编码：Media Foundation 的 AAC 编码器，输入 48 kHz / 16-bit / 双声道
//!   PCM，输出裸 AAC-LC 帧（没有 ADTS 头），每帧 1024 个采样。
//! - 播放：Media Foundation 的 AAC 解码器解回同样的 PCM，再交给 WASAPI
//!   shared render。播放缓冲目标 40 ms，队列上限 80 ms，超了就丢旧帧。
//!
//! **状态：类型检查通过，没有在任何一台 Windows 机器上跑过。**
//! 真正的风险集中在 COM/MF 的调用顺序、设备热插拔和不同声卡的混音格式，
//! 这些只有真机能验。协议文档见 `docs/03-protocol.md` §9.4 和
//! `docs/04-media-pipeline.md` §9。
//!
//! 几个边界：
//!
//! - 隐私模式静音由上层做，本模块不判断隐私状态。
//! - 音频不重传，坏帧或不完整帧直接丢；播放端不为了音频拉长视频缓冲。
//! - 采集线程内部自建 MTA，播放线程也一样；`capture_ready` /
//!   `playback_ready` 每次调用都自己 `CoInitializeEx`，不假设调用线程
//!   已经初始化过 COM。
//! - 所有 `unsafe` 都只是为了调用 Win32 / COM / Media Foundation 接口，
//!   不在这里手写协议解析之外的指针游戏。

use std::collections::VecDeque;
use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use windows::core::HRESULT;
use windows::core::{Interface, GUID};
use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
use windows::Win32::Media::Audio::{
    eMultimedia, eRender, IAudioCaptureClient, IAudioClient, IAudioRenderClient, IMMDevice,
    IMMDeviceEnumerator, MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_LOOPBACK,
    AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
};
use windows::Win32::Media::MediaFoundation::{
    IMFMediaType, IMFSample, IMFTransform, MFAudioFormat_AAC, MFAudioFormat_PCM, MFCreateMediaType,
    MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Audio, MFShutdown, MFStartup, MFTEnumEx,
    MFSTARTUP_FULL, MFT_CATEGORY_AUDIO_DECODER, MFT_CATEGORY_AUDIO_ENCODER, MFT_ENUM_FLAG_LOCALMFT,
    MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_FLAG_SYNCMFT, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES,
    MFT_REGISTER_TYPE_INFO, MF_E_NOTACCEPTING, MF_E_TRANSFORM_NEED_MORE_INPUT,
    MF_E_TRANSFORM_STREAM_CHANGE, MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, MF_MT_AAC_PAYLOAD_TYPE,
    MF_MT_AUDIO_AVG_BYTES_PER_SECOND, MF_MT_AUDIO_BITS_PER_SAMPLE, MF_MT_AUDIO_BLOCK_ALIGNMENT,
    MF_MT_AUDIO_NUM_CHANNELS, MF_MT_AUDIO_SAMPLES_PER_SECOND, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE,
    MF_MT_USER_DATA, MF_VERSION,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED,
};

/// 协议固定的输出格式。采集端无论设备混音格式是什么，最终都变成这个。
const TARGET_RATE: u32 = 48_000;
const TARGET_CHANNELS: u16 = 2;
const TARGET_BITS: u16 = 16;
const FRAME_SAMPLES: usize = 1024;
/// 1024 个采样在 48 kHz 下的时长，单位是 100 纳秒。
const FRAME_HNS: i64 = 213_333;
/// 同一帧的微秒表示，播放端补静音用。
const FRAME_US: u64 = 21_333;

/// 采集时给音频引擎留的缓冲。loopback 不需要很小，200 ms 足够抗调度抖动。
const CAPTURE_BUFFER_HNS: i64 = 2_000_000;
/// 主动停止采集时等待线程收尾的上限。正常路径下采集循环每 5 ms 看一次
/// 停止标记，250 ms 足够覆盖 `IAudioClient::Stop` 和关 MF 的开销；如果
/// 线程卡在 `Initialize` / `MFTEnumEx` 这类驱动调用里，就不能无限等，
/// 超时后丢弃 JoinHandle（detach），让它在调用返回后自己看到 stop 退出。
const CAPTURE_STOP_TIMEOUT: Duration = Duration::from_millis(250);
/// 播放缓冲目标 40 ms。真正的“上限 80 ms”由下面的队列长度控制。
const PLAY_BUFFER_HNS: i64 = 400_000;
/// 队列硬上限。12 帧是任务里给的数量；时间上的 80 ms 上限由时间戳再收一层。
const MAX_QUEUED_FRAMES: usize = 12;
const MAX_QUEUED_US: u64 = 80_000;
/// 默认码率，节点目前传 128 kbps 进来。
const DEFAULT_BITRATE: i32 = 128_000;

const WAVE_FORMAT_PCM: u16 = 1;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

// 这两个 GUID 在 windows crate 的 KernelStreaming / Multimedia 特性里，
// 而本 crate 没有启用那两个特性。值来自 ksmedia.h，直接写成常量即可。
const KSDATAFORMAT_SUBTYPE_PCM: GUID = GUID::from_u128(0x0000_0001_0000_0010_8000_00aa_0038_9b71);
const KSDATAFORMAT_SUBTYPE_IEEE_FLOAT: GUID =
    GUID::from_u128(0x0000_0003_0000_0010_8000_00aa_0038_9b71);

/// 裸 AAC 帧的队列。只放已经编码好的帧，播放线程自己解码。
static PLAY: Mutex<PlayQueue> = Mutex::new(PlayQueue {
    frames: VecDeque::new(),
    reset_epoch: 0,
    ack_epoch: 0,
    started: false,
    retry_at: None,
});
static PLAY_CV: Condvar = Condvar::new();
/// 播放线程写设备时用它察觉 reset。原子量避免为了一个标志反复拿锁。
static PLAY_EPOCH: AtomicU64 = AtomicU64::new(0);

static CAPTURE: Mutex<Option<CaptureSession>> = Mutex::new(None);
/// 串行化 start_capture，避免两个调用同时看到“没有会话”。
static START: Mutex<()> = Mutex::new(());

/// 采集会话代次。新会话、`stop_capture`、`abandon` 和 `reset` 都会推进它。
/// 采集线程启动时拿走一个值，之后在把帧交给编码器/全局 sink 之前核对；
/// 只要不再相等，就说明这场会话已经被停止或被新会话顶掉，手里的帧必须
/// 全部丢弃并退出，不能写进 `av.rs` 的全局队列。
static CAPTURE_GENERATION: AtomicU64 = AtomicU64::new(0);

/// 分配并发布下一个采集代次。无锁，只做一次原子自增。
fn next_capture_generation() -> u64 {
    CAPTURE_GENERATION
        .fetch_add(1, Ordering::AcqRel)
        .wrapping_add(1)
}

fn capture_generation_is(generation: u64) -> bool {
    CAPTURE_GENERATION.load(Ordering::Acquire) == generation
}

/// 只让指定的一代失效。如果代次已经被并发的 stop/reset 推得更新，就什么
/// 都不做，避免把新会话一起顶掉。CAS 循环不拿任何锁，采集线程也不会因为
/// 这里去碰 CAPTURE / START，不会产生新的锁顺序。
fn invalidate_capture_generation(generation: u64) {
    let mut current = CAPTURE_GENERATION.load(Ordering::Acquire);
    loop {
        if current != generation {
            return;
        }
        match CAPTURE_GENERATION.compare_exchange(
            current,
            current.wrapping_add(1),
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return,
            Err(now) => current = now,
        }
    }
}

// ---------------------------------------------------------------------------
// 对外接口
// ---------------------------------------------------------------------------

/// 本机现在能不能从默认播放设备做 loopback 采集。
///
/// 没有设备、设备被禁用、音频服务没起来都返回 false。这里只做端点检查，
/// 不真的开流，避免每次询问都在系统里留下一个采集会话。
pub fn capture_ready() -> bool {
    with_com(|| match default_render_device() {
        Ok(device) => match activate_audio_client(&device) {
            Ok(client) => {
                // SAFETY: GetMixFormat 返回 CoTaskMem 分配的格式块，读一下
                // 能证明设备音频引擎可用，随后按 COM 规则释放。
                match unsafe { client.GetMixFormat() } {
                    Ok(format) if !format.is_null() => {
                        unsafe { CoTaskMemFree(Some(format as *const c_void)) };
                        true
                    }
                    Ok(_) => false,
                    Err(e) => {
                        eprintln!("node: 音频采集检查失败，拿不到设备混音格式: {e}");
                        false
                    }
                }
            }
            Err(e) => {
                eprintln!("node: 音频采集检查失败: {e}");
                false
            }
        },
        Err(e) => {
            eprintln!("node: 音频采集检查失败: {e}");
            false
        }
    })
    .unwrap_or(false)
}

/// 本机能不能播放远端 AAC：默认播放设备在，而且系统 AAC 解码器能建起来。
pub fn playback_ready() -> bool {
    with_com(|| {
        if let Err(e) = default_render_device() {
            eprintln!("node: 音频播放检查失败: {e}");
            return false;
        }
        // SAFETY: 开关 MF 只是进程级引用计数，配对的 MFShutdown 在同一个
        // 闭包里调用；建解码器也同样只调用系统接口。
        unsafe {
            if let Err(e) = MFStartup(MF_VERSION, MFSTARTUP_FULL) {
                eprintln!("node: 音频播放检查失败，媒体框架起不来: {e}");
                return false;
            }
            let ok = AacDecoder::new().is_ok();
            let _ = MFShutdown();
            ok
        }
    })
    .unwrap_or(false)
}

/// 起采集线程。最多等 5 秒初始化，失败返回能直接给用户看的话。
///
/// `sink(bytes, timestamp_us)` 里的 `bytes` 一定是裸 AAC-LC 帧，不带 ADTS
/// 头；`timestamp_us` 从采集启动开始按 1024/48000 递增。
pub fn start_capture(
    bitrate: i32,
    sink: Box<dyn FnMut(Vec<u8>, u64) + Send>,
) -> Result<(), String> {
    let _start_guard = START.lock().unwrap_or_else(|e| e.into_inner());

    // 上一个线程如果已经自己出错退出，先把 handle 收掉再开新的。
    let stale = {
        let mut slot = CAPTURE.lock().unwrap_or_else(|e| e.into_inner());
        match slot.as_ref() {
            Some(session) if session.is_running() => {
                return Err("音频采集已经在跑".into());
            }
            Some(_) => slot.take(),
            None => None,
        }
    };
    if let Some(session) = stale {
        // 这条路径只做非阻塞回收：线程已退出就 join 收尸，否则直接
        // detach。绝不能在 START 锁里等一个可能卡在设备初始化里的线程。
        session.reclaim_stale();
    }

    let stop = Arc::new(AtomicBool::new(false));
    let alive = Arc::new(AtomicBool::new(true));
    // 新会话在拉起线程之前就占一代。从这一刻起，之前 detach 的旧线程即使
    // 刚从驱动调用里返回，也会在把帧交给 sink 前发现自己已经过期。
    let generation = next_capture_generation();
    let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();

    let thread_stop = stop.clone();
    let thread_alive = alive.clone();
    let join = std::thread::Builder::new()
        .name("jrp-audio-capture".into())
        .spawn(move || {
            let result = capture_thread(bitrate, sink, thread_stop, generation, ready_tx);
            if let Err(e) = result {
                eprintln!("node: 音频采集退出: {e}");
            }
            thread_alive.store(false, Ordering::SeqCst);
        })
        .map_err(|e| format!("音频采集线程起不来: {e}"))?;

    let session = CaptureSession {
        stop,
        alive,
        generation,
        join: Some(join),
    };

    match ready_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(())) => {
            let mut slot = CAPTURE.lock().unwrap_or_else(|e| e.into_inner());
            // 初始化这段时间里可能有并发的 stop/reset 推进过代次。stop 也走
            // CAPTURE 槽，持锁核对能让“stop 在入槽前发生”和“stop 在入槽后
            // 发生”变成两条确定的路：前者这里直接失败，后者 stop 会拿到刚
            // 入槽的会话。reset 不碰槽，但它在初始化期间推进的代次这里同样
            // 能看到。
            if !capture_generation_is(generation) {
                drop(slot);
                session.stop();
                return Err("音频采集已被停止".into());
            }
            *slot = Some(session);
            Ok(())
        }
        Ok(Err(e)) => {
            session.stop();
            Err(e)
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            // 线程可能还卡在设备初始化里。立起停止标记、推进代次后直接
            // detach：初始化返回后 capture_loop 第一轮就会看到 stop 或
            // 过期代次并退出，就算已经拿到一包也不会送进 sink。
            //
            // 不能把它留在全局槽里等下一次 join —— 下一次 start_capture
            // 会在持锁路径上 join 一个可能永久阻塞的线程；而且这个会话
            // 本来就不算 running，留在槽里只会挡住新的启动。
            session.abandon();
            Err("音频采集初始化超时".into())
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            session.stop();
            Err("音频采集线程意外退出".into())
        }
    }
}

/// 只停采集，不动播放。回调随线程一起释放。
///
/// 最多等 `CAPTURE_STOP_TIMEOUT`：正常线程会被 join 回收；卡在驱动调用里
/// 的线程超时后 detach，等它自己从调用里返回再退出。这样调用方（可能正
/// 持着上层会话锁）不会被无限拖住。
pub fn stop_capture() {
    // 先推进代次再动槽/等线程。旧线程可能正卡在
    // GetBuffer→ReleaseBuffer→feed 之间，250 ms 后 detach；只要代次已经
    // 变了，它返回后会立刻丢弃手里这一包并退出，不会把旧会话音频写进
    // 全局队列。这里只是一次原子自增，不碰 CAPTURE，也不和 START 抢锁。
    let _ = next_capture_generation();
    let taken = {
        let mut slot = CAPTURE.lock().unwrap_or_else(|e| e.into_inner());
        slot.take()
    };
    if let Some(session) = taken {
        // 代次已经在上面推进过；session.stop() 里的失效操作会发现自己
        // 已经不是当前代次，不会重复推进。
        session.stop();
    }
}

/// 采集是否还在正常跑。设备热插拔会让线程自己退出；已经请求停止的会话
/// （初始化超时后 detach 的线程、正在被 stop 的会话）不算在跑。节点靠这个
/// 发现 `audio_started` 已经过期并重新启动采集。
pub fn running() -> bool {
    let slot = CAPTURE.lock().unwrap_or_else(|e| e.into_inner());
    slot.as_ref()
        .map(|session| session.is_running())
        .unwrap_or(false)
}

/// 把一帧裸 AAC 交给播放队列。非阻塞；队列满了丢旧帧。
pub fn play(bytes: &[u8], ts: u64) {
    if bytes.is_empty() {
        return;
    }

    let mut spawn_worker = false;
    let mut spawn_epoch = 0u64;
    {
        let mut queue = PLAY.lock().unwrap_or_else(|e| e.into_inner());

        // 数量上限和时间上限同时生效：12 帧是硬顶，超过 80 ms 的旧帧也丢。
        while queue.frames.len() >= MAX_QUEUED_FRAMES {
            let _ = queue.frames.pop_front();
        }
        while let Some((_, oldest_ts)) = queue.frames.front() {
            if ts > *oldest_ts && ts - *oldest_ts > MAX_QUEUED_US {
                let _ = queue.frames.pop_front();
            } else {
                break;
            }
        }
        queue.frames.push_back((bytes.to_vec(), ts));

        if !queue.started {
            let now = Instant::now();
            if queue.retry_at.is_some_and(|at| now < at) {
                // 上一轮播放设备刚失败，先别为每一帧都新建线程。
                return;
            }
            queue.retry_at = None;
            queue.started = true;
            // 记下这一轮线程属于哪个 reset epoch。线程起不来时交给
            // mark_playback_stopped 判断期间有没有被 reset 顶掉。
            spawn_epoch = queue.reset_epoch;
            spawn_worker = true;
        }
    }

    PLAY_CV.notify_one();

    if spawn_worker {
        let result = std::thread::Builder::new()
            .name("jrp-audio-play".into())
            .spawn(move || playback_thread(spawn_epoch));
        if let Err(e) = result {
            eprintln!("node: 音频播放线程起不来: {e}");
            mark_playback_stopped(spawn_epoch);
        }
    }
}

/// 清空播放队列并停掉播放设备。播放线程保留，下一帧来了再重新开设备。
pub fn reset() {
    // 会话重置时也让采集侧推进一代：即使上层漏了 stop_capture，旧线程
    // 之后吐出的帧也不会混进重置后的新会话。只做一次原子自增，不持有
    // 采集侧的锁，和播放队列的锁没有嵌套关系。
    let _ = next_capture_generation();

    let (epoch, started) = {
        let mut queue = PLAY.lock().unwrap_or_else(|e| e.into_inner());
        queue.frames.clear();
        queue.retry_at = None;
        queue.reset_epoch = queue.reset_epoch.wrapping_add(1);
        let epoch = queue.reset_epoch;
        if !queue.started {
            queue.ack_epoch = queue.ack_epoch.max(epoch);
        }
        // 在持锁时写播放线程读的原子量。放到锁外会让并发 reset 的两次
        // store 以相反顺序落地，PLAY_EPOCH 可能退回旧 epoch，播放线程
        // 之后每次写设备都会因为 epoch 不等而静默丢掉整段音频。
        PLAY_EPOCH.store(epoch, Ordering::SeqCst);
        (epoch, queue.started)
    };
    PLAY_CV.notify_all();

    if !started {
        return;
    }

    // 等播放线程把设备关掉，最多等 500 ms。等不到也不报错：设备最多在
    // 下一次 100 ms 轮询里被释放，不影响下一场会话。
    let deadline = Instant::now() + Duration::from_millis(500);
    let mut queue = PLAY.lock().unwrap_or_else(|e| e.into_inner());
    while queue.ack_epoch < epoch {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        let (next, _) = PLAY_CV
            .wait_timeout(queue, deadline.saturating_duration_since(now))
            .unwrap_or_else(|e| e.into_inner());
        queue = next;
    }
}

// ---------------------------------------------------------------------------
// COM / WASAPI 基础
// ---------------------------------------------------------------------------

/// 在调用线程上临时初始化 MTA。已由别的组件初始化过时直接用现有套间。
fn with_com<T>(f: impl FnOnce() -> T) -> Option<T> {
    // SAFETY: 只是在本线程上增减 COM 引用计数，没有跨线程传递接口。
    unsafe {
        let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
        let owned = hr.is_ok();
        if hr.is_err() && hr != RPC_E_CHANGED_MODE {
            return None;
        }
        let value = f();
        if owned {
            CoUninitialize();
        }
        Some(value)
    }
}

fn default_render_device() -> Result<IMMDevice, String> {
    // SAFETY: 创建系统音频端点枚举器，接口在本函数返回后由 COM 引用计数管理。
    let enumerator: IMMDeviceEnumerator = unsafe {
        CoCreateInstance(
            &MMDeviceEnumerator,
            None::<&windows::core::IUnknown>,
            CLSCTX_ALL,
        )
    }
    .map_err(|e| format!("没有音频端点枚举器: {e}"))?;
    // SAFETY: 只查询默认播放端点；没有设备时会返回错误。
    unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eMultimedia) }
        .map_err(|e| format!("没有默认播放设备: {e}"))
}

type ActivateFn = unsafe extern "system" fn(
    *mut c_void,
    *const GUID,
    u32,
    *const c_void,
    *mut *mut c_void,
) -> HRESULT;

/// 从 `IMMDevice` 激活 `IAudioClient`。
///
/// windows crate 把 `IMMDevice::Activate` 包在未启用的 `Win32_System_Variant`
/// 特性后面。COM vtable 布局是固定的，这里按槽位调用，避免为一个
/// `PROPVARIANT` 空指针去改 Cargo.toml。
fn activate_audio_client(device: &IMMDevice) -> Result<IAudioClient, String> {
    let raw = device.as_raw();
    if raw.is_null() {
        return Err("音频设备指针为空".into());
    }

    // SAFETY: IMMDevice 的前三个槽是 IUnknown，第四个槽是 Activate。
    // 参数按 COM ABI 传入，返回的接口指针由 IAudioClient 接管所有权。
    unsafe {
        let vtable = *(raw as *const *const usize);
        if vtable.is_null() {
            return Err("音频设备 vtable 为空".into());
        }
        let activate: ActivateFn = std::mem::transmute(*vtable.add(3));
        let mut out: *mut c_void = ptr::null_mut();
        let hr = activate(raw, &IAudioClient::IID, CLSCTX_ALL.0, ptr::null(), &mut out);
        if hr.is_err() {
            return Err(format!("激活音频客户端失败: {hr}"));
        }
        if out.is_null() {
            return Err("音频客户端为空".into());
        }
        Ok(IAudioClient::from_raw(out))
    }
}

/// 48 kHz / 16-bit / 双声道的目标格式。
fn target_waveformat() -> WAVEFORMATEX {
    WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_PCM,
        nChannels: TARGET_CHANNELS,
        nSamplesPerSec: TARGET_RATE,
        nAvgBytesPerSec: TARGET_RATE * u32::from(TARGET_CHANNELS) * 2,
        nBlockAlign: TARGET_CHANNELS * 2,
        wBitsPerSample: TARGET_BITS,
        cbSize: 0,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SampleKind {
    Pcm,
    Float,
}

#[derive(Clone, Copy)]
struct SourceFormat {
    kind: SampleKind,
    channels: u16,
    rate: u32,
    bits: u16,
    block_align: u16,
}

impl SourceFormat {
    fn bytes_per_sample(&self) -> usize {
        match (self.kind, self.bits) {
            (SampleKind::Pcm, 8) => 1,
            (SampleKind::Pcm, 16) => 2,
            (SampleKind::Pcm, 24) => 3,
            (SampleKind::Pcm, 32) => 4,
            (SampleKind::Float, 32) => 4,
            (SampleKind::Float, 64) => 8,
            _ => 0,
        }
    }

    fn is_target(&self) -> bool {
        self.kind == SampleKind::Pcm
            && self.channels == TARGET_CHANNELS
            && self.rate == TARGET_RATE
            && self.bits == TARGET_BITS
            && self.bytes_per_sample() == 2
            && self.block_align == TARGET_CHANNELS * 2
    }

    fn validate(&self) -> Result<(), String> {
        if self.channels == 0 || self.rate == 0 {
            return Err("音频格式的声道数或采样率为零".into());
        }
        let bytes = self.bytes_per_sample();
        if bytes == 0 {
            return Err(format!(
                "不支持的音频采样格式: {:?} {} bit",
                if self.kind == SampleKind::Pcm {
                    "PCM"
                } else {
                    "float"
                },
                self.bits
            ));
        }
        if usize::from(self.block_align) < usize::from(self.channels) * bytes {
            return Err("音频格式的 block align 太小".into());
        }
        Ok(())
    }
}

/// 读 WAVEFORMATEX / WAVEFORMATEXTENSIBLE。
///
/// 调用方保证 `format` 指向 WASAPI 或 MF 返回的完整格式块。
unsafe fn source_format(format: *const WAVEFORMATEX) -> Result<SourceFormat, String> {
    if format.is_null() {
        return Err("设备没有给出音频格式".into());
    }

    // SAFETY: 上面检查过非空；WAVEFORMATEX 是 packed 结构，用
    // read_unaligned 避免因为对齐问题产生未定义行为。
    let base = unsafe { format.read_unaligned() };
    let kind = match base.wFormatTag {
        WAVE_FORMAT_PCM => SampleKind::Pcm,
        WAVE_FORMAT_IEEE_FLOAT => SampleKind::Float,
        WAVE_FORMAT_EXTENSIBLE => {
            if base.cbSize < 22 {
                return Err("扩展音频格式缺少 SubFormat".into());
            }
            // SAFETY: cbSize 已经确认能容纳完整的 WAVEFORMATEXTENSIBLE。
            let ext = unsafe { (format as *const WAVEFORMATEXTENSIBLE).read_unaligned() };
            // SAFETY: packed 结构里的 GUID 字段不能被直接借用，按值读出来。
            let subformat: GUID = unsafe { ptr::read_unaligned(ptr::addr_of!(ext.SubFormat)) };
            if subformat == KSDATAFORMAT_SUBTYPE_PCM {
                SampleKind::Pcm
            } else if subformat == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT {
                SampleKind::Float
            } else {
                return Err("不支持的音频子格式".into());
            }
        }
        other => return Err(format!("不支持的音频格式标签: {other}")),
    };

    // 有的驱动在 extensible 里把有效位数写得比容器位数小。解码按容器走，
    // 24-in-32 的值仍然是左对齐的，直接按 32 位缩放即可。
    let fmt = SourceFormat {
        kind,
        channels: base.nChannels,
        rate: base.nSamplesPerSec,
        bits: base.wBitsPerSample,
        block_align: base.nBlockAlign,
    };
    fmt.validate()?;
    Ok(fmt)
}

// ---------------------------------------------------------------------------
// 格式转换：任意设备格式 -> 48 kHz / 16-bit / 双声道
// ---------------------------------------------------------------------------

fn to_i16(sample: f32) -> i16 {
    (sample.clamp(-1.0, 1.0) * 32_767.0).round() as i16
}

/// 线性重采样。输入输出都是双声道 f32，跨缓冲区保持相位。
struct LinearResampler {
    step: f64,
    next_out: f64,
    next_in: u64,
    prev: [f32; 2],
    have_prev: bool,
}

impl LinearResampler {
    fn new(source_rate: u32) -> Self {
        Self {
            step: f64::from(source_rate) / f64::from(TARGET_RATE),
            next_out: 0.0,
            next_in: 0,
            prev: [0.0, 0.0],
            have_prev: false,
        }
    }

    fn push(&mut self, sample: [f32; 2], out: &mut Vec<i16>) {
        let index = self.next_in;
        self.next_in = self.next_in.saturating_add(1);

        if !self.have_prev {
            self.prev = sample;
            self.have_prev = true;
            self.next_out = index as f64;
            return;
        }

        let previous_index = index.saturating_sub(1) as f64;
        while self.next_out < index as f64 {
            let t = (self.next_out - previous_index).clamp(0.0, 1.0) as f32;
            let left = self.prev[0] + (sample[0] - self.prev[0]) * t;
            let right = self.prev[1] + (sample[1] - self.prev[1]) * t;
            out.push(to_i16(left));
            out.push(to_i16(right));
            self.next_out += self.step;
        }
        self.prev = sample;
    }
}

struct CaptureConverter {
    source: SourceFormat,
    resampler: LinearResampler,
}

impl CaptureConverter {
    fn new(source: SourceFormat) -> Self {
        Self {
            source,
            resampler: LinearResampler::new(source.rate),
        }
    }

    /// `data` 为 None 表示 WASAPI 说是静音包（或者指针为空）。
    fn push(&mut self, data: Option<&[u8]>, frames: u32, out: &mut Vec<i16>) -> Result<(), String> {
        let frames = frames as usize;
        if frames == 0 {
            return Ok(());
        }

        if self.source.is_target() {
            let wanted = frames.saturating_mul(4);
            match data {
                Some(bytes) => {
                    if bytes.len() < wanted {
                        return Err("采集到的 PCM 数据长度不足".into());
                    }
                    out.reserve(frames * 2);
                    for pair in bytes[..wanted].chunks_exact(2) {
                        out.push(i16::from_le_bytes([pair[0], pair[1]]));
                    }
                }
                None => out.resize(out.len() + frames * 2, 0),
            }
            return Ok(());
        }

        let Some(bytes) = data else {
            // 静音也要喂进重采样器，否则相位会跳。
            for _ in 0..frames {
                self.resampler.push([0.0, 0.0], out);
            }
            return Ok(());
        };

        let block_align = usize::from(self.source.block_align);
        let needed = frames.saturating_mul(block_align);
        if bytes.len() < needed {
            return Err("采集到的 PCM 数据长度不足".into());
        }

        let bytes_per_sample = self.source.bytes_per_sample();
        let channels = usize::from(self.source.channels);
        out.reserve(frames * 2);

        for frame in 0..frames {
            let base = frame * block_align;
            let mut left = 0.0f32;
            let mut right = 0.0f32;
            for channel in 0..channels {
                let offset = base + channel * bytes_per_sample;
                let value = sample_at(bytes, offset, &self.source);
                match channel {
                    0 => left = value,
                    1 => right = value,
                    // 多声道只取前两个（通常是前左 / 前右），不做复杂下混。
                    _ => {}
                }
            }
            if channels == 1 {
                right = left;
            }
            self.resampler.push([left, right], out);
        }
        Ok(())
    }
}

fn sample_at(bytes: &[u8], offset: usize, format: &SourceFormat) -> f32 {
    match (format.kind, format.bits) {
        (SampleKind::Pcm, 8) => (f32::from(bytes[offset]) - 128.0) / 128.0,
        (SampleKind::Pcm, 16) => {
            f32::from(i16::from_le_bytes([bytes[offset], bytes[offset + 1]])) / 32_768.0
        }
        (SampleKind::Pcm, 24) => {
            let raw = i32::from(bytes[offset])
                | (i32::from(bytes[offset + 1]) << 8)
                | (i32::from(bytes[offset + 2]) << 16);
            // 左移再算术右移做 24 位符号扩展；直接截成 i16 会丢掉最高位。
            let signed = (raw << 8) >> 8;
            signed as f32 / 8_388_608.0
        }
        (SampleKind::Pcm, 32) => {
            let raw = i32::from_le_bytes([
                bytes[offset],
                bytes[offset + 1],
                bytes[offset + 2],
                bytes[offset + 3],
            ]);
            raw as f32 / 2_147_483_648.0
        }
        (SampleKind::Float, 32) => f32::from_le_bytes([
            bytes[offset],
            bytes[offset + 1],
            bytes[offset + 2],
            bytes[offset + 3],
        ]),
        (SampleKind::Float, 64) => {
            let raw = f64::from_le_bytes([
                bytes[offset],
                bytes[offset + 1],
                bytes[offset + 2],
                bytes[offset + 3],
                bytes[offset + 4],
                bytes[offset + 5],
                bytes[offset + 6],
                bytes[offset + 7],
            ]);
            raw as f32
        }
        _ => 0.0,
    }
}

// ---------------------------------------------------------------------------
// 采集
// ---------------------------------------------------------------------------

/// 一帧音频能不能交给 sink 的纯判断。参数都是值，不读全局状态、不拿锁，
/// 因此可以在任何平台上直接测试。
///
/// 当前会话状态由 `stop_requested` 和 `published_generation` 共同表达：
/// stop / abandon / reset 会立停止标记或推进代次，两者只要有一个变化，
/// 旧线程手里的帧就不再属于当前会话。这里刻意不去读 `CAPTURE` 槽 ——
/// 那会引入一把新锁，而 sink 可能回调到上层队列，容易形成锁顺序问题；
/// 代次已经能唯一标识会话，不需要再读槽。
fn frame_delivery_allowed(
    stop_requested: bool,
    thread_generation: u64,
    published_generation: u64,
) -> bool {
    !stop_requested && thread_generation == published_generation
}

/// 采集线程是否仍属于当前会话。`generation` 是启动时拿到的代次；
/// stop_capture / abandon / reset 推进代次后，这里立刻变成 false。
fn capture_live(stop: &AtomicBool, generation: u64) -> bool {
    frame_delivery_allowed(
        stop.load(Ordering::SeqCst),
        generation,
        CAPTURE_GENERATION.load(Ordering::Acquire),
    )
}

struct CaptureSession {
    stop: Arc<AtomicBool>,
    alive: Arc<AtomicBool>,
    generation: u64,
    join: Option<std::thread::JoinHandle<()>>,
}

impl CaptureSession {
    /// 已经请求停止的会话不再算“正在跑”；否则 `start_capture` 会一直回
    /// “已经在跑”，节点重试也永远起不来。
    fn is_running(&self) -> bool {
        self.alive.load(Ordering::SeqCst) && !self.stop.load(Ordering::SeqCst)
    }

    /// 让这一代会话失效。只推进自己那一代：如果别的 stop/reset 已经推得
    /// 更新，就不要再动，否则会把新会话一起顶掉。
    fn invalidate(&self) {
        invalidate_capture_generation(self.generation);
    }

    /// 正常停止：请求停止后最多等 `CAPTURE_STOP_TIMEOUT` 回收线程。
    /// 超时说明线程卡在不检查 stop 的驱动调用里，丢弃 handle 让它自己
    /// 返回后退出；调用方不会因此被无限阻塞。
    fn stop(mut self) {
        self.invalidate();
        self.stop.store(true, Ordering::SeqCst);
        self.join_within(CAPTURE_STOP_TIMEOUT);
    }

    /// 回收 stale 会话。线程已经退出就 join（立即返回），否则 detach。
    /// 用在 `start_capture` 的持锁路径上，所以这里不允许等待。
    fn reclaim_stale(mut self) {
        self.invalidate();
        self.stop.store(true, Ordering::SeqCst);
        if self.alive.load(Ordering::SeqCst) {
            // 可能正卡在 AudioClient::Initialize / MFTEnumEx 里。丢 handle
            // 即 detach；线程初始化返回后会读到 stop 并自行退出，而且它的
            // 代次已经失效，不会再把帧交给 sink。
            self.join = None;
        } else {
            self.join_now();
        }
    }

    /// 初始化超时的会话不留在全局槽里：立停止标记后直接 detach。
    /// 线程之后若初始化成功，`capture_loop` 第一轮就会退出。
    fn abandon(mut self) {
        self.invalidate();
        self.stop.store(true, Ordering::SeqCst);
        self.join = None;
    }

    fn join_within(&mut self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while self.alive.load(Ordering::SeqCst) {
            let now = Instant::now();
            if now >= deadline {
                // 超时：丢弃 JoinHandle（detach）。线程还持有 stop/alive
                // 的 Arc，退出时会自己收尾，不需要这里再管。
                self.join = None;
                return;
            }
            std::thread::sleep(
                deadline
                    .saturating_duration_since(now)
                    .min(Duration::from_millis(2)),
            );
        }
        self.join_now();
    }

    fn join_now(&mut self) {
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
        self.alive.store(false, Ordering::SeqCst);
    }
}

fn capture_thread(
    bitrate: i32,
    mut sink: Box<dyn FnMut(Vec<u8>, u64) + Send>,
    stop: Arc<AtomicBool>,
    generation: u64,
    ready_tx: mpsc::Sender<Result<(), String>>,
) -> Result<(), String> {
    // SAFETY: 采集线程自己初始化 MTA，结尾配对 CoUninitialize。
    let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    let owns_com = com.is_ok();
    if com.is_err() && com != RPC_E_CHANGED_MODE {
        let message = format!("音频采集线程初始化 COM 失败: {com}");
        let _ = ready_tx.send(Err(message.clone()));
        return Err(message);
    }

    // SAFETY: Media Foundation 的进程级初始化，结尾配对 MFShutdown。
    if let Err(e) = unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) } {
        let message = format!("媒体框架起不来: {e}");
        let _ = ready_tx.send(Err(message.clone()));
        if owns_com {
            unsafe { CoUninitialize() };
        }
        return Err(message);
    }

    let initialized = (|| -> Result<(IAudioClient, IAudioCaptureClient, CaptureConverter, AacEncoder), String> {
        let device = default_render_device()?;
        let (client, format) = open_capture_client(&device)?;
        // SAFETY: 从已初始化的 IAudioClient 取采集服务，接口跟随 client 生命周期。
        let capture: IAudioCaptureClient = match unsafe { client.GetService() } {
            Ok(capture) => capture,
            Err(e) => return Err(format!("拿不到音频采集接口: {e}")),
        };
        let encoder = AacEncoder::new(bitrate)?;
        if let Err(e) = unsafe { client.Start() } {
            return Err(format!("音频采集流启动失败: {e}"));
        }
        Ok((client, capture, CaptureConverter::new(format), encoder))
    })();

    let (client, capture, mut converter, mut encoder) = match initialized {
        Ok(value) => {
            let _ = ready_tx.send(Ok(()));
            value
        }
        Err(e) => {
            let _ = ready_tx.send(Err(e.clone()));
            unsafe {
                let _ = MFShutdown();
            };
            if owns_com {
                unsafe { CoUninitialize() };
            }
            return Err(e);
        }
    };

    // 最靠近全局 sink 的一道闸：编码器一次 feed 可能吐出多帧，即使线程
    // 在 feed 中途才发现会话被顶掉，这些帧也到不了上层队列。检查只是两次
    // 原子读，不拿锁，不会和 sink 自己的队列锁形成嵌套。
    let mut guarded_sink = |bytes: Vec<u8>, ts: u64| {
        if frame_delivery_allowed(
            stop.load(Ordering::SeqCst),
            generation,
            CAPTURE_GENERATION.load(Ordering::Acquire),
        ) {
            sink(bytes, ts);
        }
    };

    let result = capture_loop(
        &stop,
        generation,
        &capture,
        &mut converter,
        &mut encoder,
        &mut guarded_sink,
    );
    let _ = unsafe { client.Stop() };

    // 先让编码器和设备接口离开作用域，再关 MF / COM。
    drop(encoder);
    drop(capture);
    drop(client);
    unsafe {
        let _ = MFShutdown();
    };
    if owns_com {
        unsafe { CoUninitialize() };
    }

    result
}

/// 打开默认播放设备的 loopback 采集。
///
/// 先按 48 kHz / 16-bit / 双声道加 AUTOCONVERT 去谈；谈不下来就退到设备
/// mix format，转换交给 `CaptureConverter`。两条路返回的格式都保证能被
/// 转成协议要求的 PCM。
fn open_capture_client(device: &IMMDevice) -> Result<(IAudioClient, SourceFormat), String> {
    let client = activate_audio_client(device)?;
    let target = target_waveformat();
    let target_flags = AUDCLNT_STREAMFLAGS_LOOPBACK
        | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
        | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;

    // SAFETY: target 是栈上的完整 WAVEFORMATEX，Initialize 只读取它的内容。
    let first = unsafe {
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            target_flags,
            CAPTURE_BUFFER_HNS,
            0,
            &target,
            None,
        )
    };
    match first {
        Ok(()) => Ok((
            client,
            SourceFormat {
                kind: SampleKind::Pcm,
                channels: TARGET_CHANNELS,
                rate: TARGET_RATE,
                bits: TARGET_BITS,
                block_align: TARGET_CHANNELS * 2,
            },
        )),
        Err(first_error) => {
            eprintln!("node: 音频采集不接受 48k/16bit/双声道，退回设备混音格式: {first_error}");

            // 失败的那只 client 不再复用，重新激活一只，避免驱动把状态留着。
            let client = activate_audio_client(device)?;
            // SAFETY: GetMixFormat 返回 CoTaskMem 分配的格式块，复制出需要的
            // 字节后立即释放；后面的 Initialize 只读这份拷贝。
            let format_ptr =
                unsafe { client.GetMixFormat() }.map_err(|e| format!("拿不到设备混音格式: {e}"))?;
            if format_ptr.is_null() {
                return Err("设备混音格式为空".into());
            }
            let format = unsafe { source_format(format_ptr) };
            let base = unsafe { format_ptr.read_unaligned() };
            let size = 18usize.saturating_add(usize::from(base.cbSize));
            let format_bytes =
                unsafe { std::slice::from_raw_parts(format_ptr as *const u8, size).to_vec() };
            unsafe { CoTaskMemFree(Some(format_ptr as *const c_void)) };

            let format = format?;
            // SAFETY: format_bytes 里是完整的 WAVEFORMATEX(EXTENSIBLE)，
            // 紧跟着的 Initialize 只读取它。
            unsafe {
                client.Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    AUDCLNT_STREAMFLAGS_LOOPBACK,
                    CAPTURE_BUFFER_HNS,
                    0,
                    format_bytes.as_ptr() as *const WAVEFORMATEX,
                    None,
                )
            }
            .map_err(|e| format!("音频采集流初始化失败: {e}"))?;
            Ok((client, format))
        }
    }
}

fn capture_loop(
    stop: &AtomicBool,
    generation: u64,
    capture: &IAudioCaptureClient,
    converter: &mut CaptureConverter,
    encoder: &mut AacEncoder,
    sink: &mut (dyn FnMut(Vec<u8>, u64) + Send),
) -> Result<(), String> {
    let mut pcm = Vec::<i16>::new();

    loop {
        if !capture_live(stop, generation) {
            return Ok(());
        }

        // SAFETY: 每次只问下一包有多少帧，不持有缓冲区。
        let mut packet = match unsafe { capture.GetNextPacketSize() } {
            Ok(size) => size,
            Err(e) => return Err(format!("读取音频包长度失败: {e}")),
        };

        if packet == 0 {
            std::thread::sleep(Duration::from_millis(5));
            continue;
        }

        while packet > 0 {
            if !capture_live(stop, generation) {
                return Ok(());
            }

            let mut data: *mut u8 = ptr::null_mut();
            let mut frames = 0u32;
            let mut flags = 0u32;
            // SAFETY: GetBuffer 借出下一包数据，ReleaseBuffer 前不跨调用保存。
            if let Err(e) =
                unsafe { capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None) }
            {
                return Err(format!("读取音频包失败: {e}"));
            }

            let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
            let slice = if silent || data.is_null() || frames == 0 {
                None
            } else {
                // SAFETY: frames 来自 GetBuffer，block_align 描述每帧字节数；
                // 这块内存只在 ReleaseBuffer 前有效。
                Some(unsafe {
                    std::slice::from_raw_parts(
                        data,
                        frames as usize * usize::from(converter.source.block_align),
                    )
                })
            };

            pcm.clear();
            let converted = converter.push(slice, frames, &mut pcm);
            // SAFETY: 无论转换是否成功都要归还这一包，否则音频引擎会卡住。
            let _ = unsafe { capture.ReleaseBuffer(frames) };
            converted?;

            // 第二道保险：GetBuffer→ReleaseBuffer 这段里可能刚好发生 stop
            // 或新会话启动。这里再核一次，绝不让这一包进编码器。
            if !capture_live(stop, generation) {
                return Ok(());
            }

            encoder.feed(&pcm, sink)?;

            // SAFETY: 继续问下一包；循环条件和上面一致。
            packet = match unsafe { capture.GetNextPacketSize() } {
                Ok(size) => size,
                Err(e) => return Err(format!("读取音频包长度失败: {e}")),
            };
        }
    }
}

// ---------------------------------------------------------------------------
// AAC 编码
// ---------------------------------------------------------------------------

struct AacEncoder {
    transform: IMFTransform,
    provides_samples: bool,
    output_size: u32,
    pending: Vec<i16>,
    frame_index: u64,
    accepted_times: VecDeque<u64>,
    last_output_ts: Option<u64>,
}

impl AacEncoder {
    fn new(bitrate: i32) -> Result<Self, String> {
        let bitrate = if bitrate > 0 {
            bitrate.clamp(32_000, 512_000) as u32
        } else {
            DEFAULT_BITRATE as u32
        };
        let avg_bytes = aac_avg_bytes(bitrate);

        let output = aac_output_type(avg_bytes)?;
        let input = pcm_input_type()?;
        let transform = find_transform(
            MFT_CATEGORY_AUDIO_ENCODER,
            MFAudioFormat_PCM,
            MFAudioFormat_AAC,
        )
        .map_err(|e| format!("系统里没有 AAC 编码器: {e}"))?;

        // SAFETY: 先谈输出再谈输入，MF 编码器按输出格式决定输入约束。
        unsafe {
            transform
                .SetOutputType(0, &output, 0)
                .map_err(|e| format!("AAC 编码器不收这个输出格式: {e}"))?;
            transform
                .SetInputType(0, &input, 0)
                .map_err(|e| format!("AAC 编码器不收 48k/16bit/双声道 PCM: {e}"))?;
        }

        let info = unsafe { transform.GetOutputStreamInfo(0) }
            .map_err(|e| format!("问不到 AAC 编码器输出信息: {e}"))?;
        let provides_samples = info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;

        // 有些编码器会接受 raw AAC 属性却仍输出 ADTS。先回读它实际选中的
        // 输出类型；拿到属性就严格要求裸 AAC，拿不到就靠首帧同步字兜底。
        if let Ok(actual) = unsafe { transform.GetOutputCurrentType(0) } {
            if let Ok(payload) = unsafe { actual.GetUINT32(&MF_MT_AAC_PAYLOAD_TYPE) } {
                if payload != 0 {
                    return Err(format!("系统 AAC 编码器输出的是 ADTS（payload_type={payload}），不是裸 AAC"));
                }
            }
        }

        let _ = unsafe { transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0) };
        let _ = unsafe { transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0) };

        Ok(Self {
            transform,
            provides_samples,
            output_size: info.cbSize.max(64 * 1024),
            pending: Vec::with_capacity(FRAME_SAMPLES * 4),
            frame_index: 0,
            accepted_times: VecDeque::new(),
            last_output_ts: None,
        })
    }

    /// 攒够 1024 个采样再送一帧。`sink` 拿到的是裸 AAC 帧。
    fn feed(
        &mut self,
        samples: &[i16],
        sink: &mut (dyn FnMut(Vec<u8>, u64) + Send),
    ) -> Result<(), String> {
        self.pending.extend_from_slice(samples);

        while self.pending.len() >= FRAME_SAMPLES * 2 {
            let frame: Vec<i16> = self.pending.drain(..FRAME_SAMPLES * 2).collect();
            let ts_us = frame_timestamp_us(self.frame_index);
            let time = frame_timestamp_hns(self.frame_index);

            let sample = pcm_sample(&frame, time, FRAME_HNS)?;
            let accepted = match unsafe { self.transform.ProcessInput(0, &sample, 0) } {
                Ok(()) => true,
                Err(e) if e.code() == MF_E_NOTACCEPTING => {
                    // 先把编码器里积压的结果取干净，再喂一次。
                    self.drain(sink, ts_us)?;
                    match unsafe { self.transform.ProcessInput(0, &sample, 0) } {
                        Ok(()) => true,
                        Err(again) if again.code() == MF_E_NOTACCEPTING => {
                            // 丢帧后不把它的时间戳记入输出队列；下一帧保留
                            // 原始采集时间，播放端会按空洞补静音。
                            eprintln!("node: AAC 编码器来不及，丢掉一帧");
                            false
                        }
                        Err(again) => return Err(format!("喂 AAC 编码器失败: {again}")),
                    }
                }
                Err(e) => return Err(format!("喂 AAC 编码器失败: {e}")),
            };
            if accepted {
                self.accepted_times.push_back(ts_us);
            }

            self.frame_index = self.frame_index.saturating_add(1);
            self.drain(sink, ts_us)?;
        }
        Ok(())
    }

    fn drain(
        &mut self,
        sink: &mut (dyn FnMut(Vec<u8>, u64) + Send),
        fallback_ts_us: u64,
    ) -> Result<(), String> {
        for _ in 0..8 {
            let mut output = MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: std::mem::ManuallyDrop::new(if self.provides_samples {
                    None
                } else {
                    Some(output_sample(self.output_size)?)
                }),
                dwStatus: 0,
                pEvents: std::mem::ManuallyDrop::new(None),
            };
            let mut status = 0u32;
            // SAFETY: 输出缓冲由编码器填写；失败时也要把样本取回来释放。
            let result = unsafe {
                self.transform
                    .ProcessOutput(0, std::slice::from_mut(&mut output), &mut status)
            };
            let sample = unsafe { std::mem::ManuallyDrop::take(&mut output.pSample) };
            let _events = unsafe { std::mem::ManuallyDrop::take(&mut output.pEvents) };

            match result {
                Ok(()) => {
                    if let Some(sample) = sample {
                        let bytes = read_sample_bytes(&sample)?;
                        if bytes.len() >= 2 && bytes[0] == 0xFF && bytes[1] & 0xF0 == 0xF0 {
                            // 协议下发的是裸 AAC；夹带 ADTS 头会让对端解码成噪声。
                            return Err("AAC 编码器输出了 ADTS 头".into());
                        }
                        if !bytes.is_empty() {
                            // MF 可能把几帧积压后一次吐出。每个输出分别取
                            // 对应已接受输入的时间戳，不能给同一批全部打上
                            // 当前输入帧的时间，否则播放端会把后几帧当迟到包。
                            let ts = next_output_timestamp(
                                &mut self.accepted_times,
                                &mut self.last_output_ts,
                                fallback_ts_us,
                            );
                            sink(bytes, ts);
                        }
                    }
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(()),
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    self.renegotiate_output()?;
                }
                Err(e) => return Err(format!("取 AAC 编码结果失败: {e}")),
            }
        }
        Ok(())
    }

    fn renegotiate_output(&mut self) -> Result<(), String> {
        // SAFETY: 编码器换了输出类型，把它当前可用的类型拿出来，重新强
        // 调裸 AAC 和 AAC-LC 后设回去。设不回去就交给上层退线程。
        unsafe {
            let again = self
                .transform
                .GetOutputAvailableType(0, 0)
                .map_err(|e| format!("AAC 编码器重谈输出格式失败: {e}"))?;
            again
                .SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0)
                .map_err(|e| format!("设置裸 AAC 输出失败: {e}"))?;
            again
                .SetUINT32(&MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, 0x29)
                .map_err(|e| format!("设置 AAC-LC 档次失败: {e}"))?;
            self.transform
                .SetOutputType(0, &again, 0)
                .map_err(|e| format!("重设 AAC 输出格式失败: {e}"))?;
            let info = self
                .transform
                .GetOutputStreamInfo(0)
                .map_err(|e| format!("问不到 AAC 编码器输出信息: {e}"))?;
            self.provides_samples = info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
            self.output_size = info.cbSize.max(64 * 1024);
        }
        Ok(())
    }
}

fn aac_output_type(avg_bytes: u32) -> Result<IMFMediaType, String> {
    // SAFETY: 创建一个空的媒体类型，随后逐项填属性。
    let media_type =
        unsafe { MFCreateMediaType() }.map_err(|e| format!("建 AAC 输出类型失败: {e}"))?;
    set_guid(&media_type, &MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
    set_guid(&media_type, &MF_MT_SUBTYPE, &MFAudioFormat_AAC)?;
    set_u32(&media_type, &MF_MT_AUDIO_SAMPLES_PER_SECOND, TARGET_RATE)?;
    set_u32(
        &media_type,
        &MF_MT_AUDIO_NUM_CHANNELS,
        u32::from(TARGET_CHANNELS),
    )?;
    set_u32(
        &media_type,
        &MF_MT_AUDIO_BITS_PER_SAMPLE,
        u32::from(TARGET_BITS),
    )?;
    // 平均值只是给编码器的码率提示，不要求每帧严格相等。
    set_u32(&media_type, &MF_MT_AUDIO_AVG_BYTES_PER_SECOND, avg_bytes)?;
    // 0 = raw_data_block，不要 ADTS 头；0x29 = AAC-LC。
    set_u32(&media_type, &MF_MT_AAC_PAYLOAD_TYPE, 0)?;
    set_u32(&media_type, &MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, 0x29)?;
    Ok(media_type)
}

/// AAC 编码器只接受几档立体声码率。把请求值映射到最接近的一档，
/// 否则 `SetOutputType` 会直接拒绝。
fn aac_avg_bytes(bitrate_bps: u32) -> u32 {
    const SUPPORTED: [u32; 4] = [12_000, 16_000, 20_000, 24_000];
    let requested = bitrate_bps / 8;
    let mut best = SUPPORTED[0];
    let mut best_diff = requested.abs_diff(best);
    for value in SUPPORTED.iter().skip(1) {
        let diff = requested.abs_diff(*value);
        if diff < best_diff {
            best = *value;
            best_diff = diff;
        }
    }
    best
}

fn pcm_input_type() -> Result<IMFMediaType, String> {
    // SAFETY: 创建一个空的媒体类型，随后逐项填属性。
    let media_type =
        unsafe { MFCreateMediaType() }.map_err(|e| format!("建 PCM 输入类型失败: {e}"))?;
    set_guid(&media_type, &MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
    set_guid(&media_type, &MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
    set_u32(&media_type, &MF_MT_AUDIO_SAMPLES_PER_SECOND, TARGET_RATE)?;
    set_u32(
        &media_type,
        &MF_MT_AUDIO_NUM_CHANNELS,
        u32::from(TARGET_CHANNELS),
    )?;
    set_u32(
        &media_type,
        &MF_MT_AUDIO_BITS_PER_SAMPLE,
        u32::from(TARGET_BITS),
    )?;
    set_u32(
        &media_type,
        &MF_MT_AUDIO_BLOCK_ALIGNMENT,
        u32::from(TARGET_CHANNELS * 2),
    )?;
    set_u32(
        &media_type,
        &MF_MT_AUDIO_AVG_BYTES_PER_SECOND,
        TARGET_RATE * u32::from(TARGET_CHANNELS) * 2,
    )?;
    Ok(media_type)
}

fn frame_timestamp_us(frame: u64) -> u64 {
    frame
        .saturating_mul(FRAME_SAMPLES as u64)
        .saturating_mul(1_000_000)
        / u64::from(TARGET_RATE)
}

/// 编码器输出可能比输入晚几帧，而且一次 drain 可以吐出多帧。
/// 优先按被编码器接受的输入顺序配时间戳；异常多出的输出也要单调前进。
fn next_output_timestamp(
    accepted_times: &mut VecDeque<u64>,
    last_output_ts: &mut Option<u64>,
    fallback_ts_us: u64,
) -> u64 {
    let proposed = accepted_times.pop_front().unwrap_or(fallback_ts_us);
    let ts = last_output_ts
        .map(|last| proposed.max(last.saturating_add(FRAME_US)))
        .unwrap_or(proposed);
    *last_output_ts = Some(ts);
    ts
}

fn frame_timestamp_hns(frame: u64) -> i64 {
    let value = frame
        .saturating_mul(FRAME_SAMPLES as u64)
        .saturating_mul(10_000_000)
        / u64::from(TARGET_RATE);
    value.min(i64::MAX as u64) as i64
}

// ---------------------------------------------------------------------------
// Media Foundation 公共小工具
// ---------------------------------------------------------------------------

fn find_transform(
    category: GUID,
    input_subtype: GUID,
    output_subtype: GUID,
) -> Result<IMFTransform, String> {
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Audio,
        guidSubtype: input_subtype,
    };
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Audio,
        guidSubtype: output_subtype,
    };
    let flags = MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_LOCALMFT | MFT_ENUM_FLAG_SORTANDFILTER;
    let mut activates: *mut Option<windows::Win32::Media::MediaFoundation::IMFActivate> =
        ptr::null_mut();
    let mut count = 0u32;

    // SAFETY: MFTEnumEx 分配一个 activate 数组；下面复制出元素后按 COM
    // 规则用 CoTaskMemFree 释放数组本身。
    let enumerated = unsafe {
        MFTEnumEx(
            category,
            flags,
            Some(&input),
            Some(&output),
            &mut activates,
            &mut count,
        )
    };
    if let Err(e) = enumerated {
        return Err(format!("枚举音频 MFT 失败: {e}"));
    }
    if count == 0 || activates.is_null() {
        if !activates.is_null() {
            unsafe { CoTaskMemFree(Some(activates as *const c_void)) };
        }
        return Err("系统里没有匹配的音频 MFT".into());
    }

    let list = unsafe { std::slice::from_raw_parts_mut(activates, count as usize) };
    let mut picked = None;
    // 每个元素都是枚举器给出的一个 COM 引用。CoTaskMemFree 只释放保存这些
    // 指针的数组，不会 Release 元素本身，所以即使已经选中了 MFT，也必须
    // 把后面的 activate 取出来释放，否则每次建编解码器都会泄漏若干个引用。
    for slot in list.iter_mut() {
        let Some(activate) = slot.take() else {
            continue;
        };
        if picked.is_none() {
            // SAFETY: 激活 enumerate 返回的 MFT；失败的条目直接跳过。
            if let Ok(transform) = unsafe { activate.ActivateObject::<IMFTransform>() } {
                picked = Some(transform);
            }
        }
        // activate 在这里离开作用域，未选中的枚举结果随之 Release。
    }
    unsafe { CoTaskMemFree(Some(activates as *const c_void)) };
    picked.ok_or_else(|| "音频 MFT 激活失败".to_string())
}

fn set_guid(media_type: &IMFMediaType, key: &GUID, value: &GUID) -> Result<(), String> {
    // SAFETY: 只是给媒体类型写一个 GUID 属性。
    unsafe { media_type.SetGUID(key, value) }.map_err(|e| format!("设置媒体类型失败: {e}"))
}

fn set_u32(media_type: &IMFMediaType, key: &GUID, value: u32) -> Result<(), String> {
    // SAFETY: 只是给媒体类型写一个 UINT32 属性。
    unsafe { media_type.SetUINT32(key, value) }.map_err(|e| format!("设置媒体类型失败: {e}"))
}

fn output_sample(size: u32) -> Result<IMFSample, String> {
    // SAFETY: 建一块 MF 输出缓冲和样本，交给 MFT 填写。
    unsafe {
        let buffer =
            MFCreateMemoryBuffer(size.max(1)).map_err(|e| format!("建音频输出缓冲失败: {e}"))?;
        let sample = MFCreateSample().map_err(|e| format!("建音频输出样本失败: {e}"))?;
        sample
            .AddBuffer(&buffer)
            .map_err(|e| format!("音频输出样本挂缓冲失败: {e}"))?;
        Ok(sample)
    }
}

fn make_sample(bytes: &[u8], time: i64, duration: i64) -> Result<IMFSample, String> {
    // SAFETY: 把一段字节拷进 MF 缓冲；长度做过 u32 转换检查。
    let length = u32::try_from(bytes.len()).map_err(|_| "音频帧太大".to_string())?;

    unsafe {
        let buffer =
            MFCreateMemoryBuffer(length.max(1)).map_err(|e| format!("建音频输入缓冲失败: {e}"))?;
        let mut data: *mut u8 = ptr::null_mut();
        let mut max = 0u32;
        buffer
            .Lock(&mut data, Some(&mut max), None)
            .map_err(|e| format!("音频输入缓冲拿不到: {e}"))?;
        if data.is_null() {
            let _ = buffer.Unlock();
            return Err("音频输入缓冲为空".into());
        }
        ptr::copy_nonoverlapping(bytes.as_ptr(), data, bytes.len());
        let _ = buffer.Unlock();
        buffer
            .SetCurrentLength(length)
            .map_err(|e| format!("设置音频输入长度失败: {e}"))?;

        let sample = MFCreateSample().map_err(|e| format!("建音频输入样本失败: {e}"))?;
        sample
            .AddBuffer(&buffer)
            .map_err(|e| format!("音频输入样本挂缓冲失败: {e}"))?;
        let _ = sample.SetSampleTime(time);
        let _ = sample.SetSampleDuration(duration);
        Ok(sample)
    }
}

fn pcm_sample(samples: &[i16], time: i64, duration: i64) -> Result<IMFSample, String> {
    // SAFETY: Windows 是小端平台，i16 的裸内存就是 PCM16LE；只读这段切片。
    let bytes =
        unsafe { std::slice::from_raw_parts(samples.as_ptr() as *const u8, samples.len() * 2) };
    make_sample(bytes, time, duration)
}

fn read_sample_bytes(sample: &IMFSample) -> Result<Vec<u8>, String> {
    // SAFETY: ConvertToContiguousBuffer 给出一个连续缓冲，Lock 在 Unlock 前
    // 有效；复制出来后再归还。
    unsafe {
        let buffer = sample
            .ConvertToContiguousBuffer()
            .map_err(|e| format!("音频样本没有连续缓冲: {e}"))?;
        let mut data: *mut u8 = ptr::null_mut();
        let mut max = 0u32;
        let mut length = 0u32;
        buffer
            .Lock(&mut data, Some(&mut max), Some(&mut length))
            .map_err(|e| format!("音频样本拿不到数据: {e}"))?;
        let bytes = if data.is_null() || length == 0 {
            Vec::new()
        } else {
            std::slice::from_raw_parts(data, length as usize).to_vec()
        };
        let _ = buffer.Unlock();
        Ok(bytes)
    }
}

// ---------------------------------------------------------------------------
// AAC 解码 + 播放
// ---------------------------------------------------------------------------

struct PlayQueue {
    frames: VecDeque<(Vec<u8>, u64)>,
    reset_epoch: u64,
    ack_epoch: u64,
    started: bool,
    /// 播放管线连续失败后的退避时间。设备被禁用时不能每收一帧就重建线程。
    retry_at: Option<Instant>,
}

enum PlaybackError {
    /// 这一帧坏了，丢掉继续。
    Frame(String),
    /// 设备或解码器已经不可用，播放线程要退出去等下一次重建。
    Fatal(String),
}

struct AacDecoder {
    transform: IMFTransform,
    provides_samples: bool,
    output_size: u32,
    next_time: i64,
}

impl AacDecoder {
    fn new() -> Result<Self, String> {
        let transform = find_transform(
            MFT_CATEGORY_AUDIO_DECODER,
            MFAudioFormat_AAC,
            MFAudioFormat_PCM,
        )
        .map_err(|e| format!("系统里没有 AAC 解码器: {e}"))?;

        let input = aac_decoder_input_type()?;
        // SAFETY: 给解码器设输入类型；裸 AAC 的 AudioSpecificConfig 放在
        // MF_MT_USER_DATA 里，类型不符合时解码器会直接拒绝。
        unsafe {
            transform
                .SetInputType(0, &input, 0)
                .map_err(|e| format!("AAC 解码器不收裸 AAC-LC 输入: {e}"))?;
        }

        let mut decoder = Self {
            transform,
            provides_samples: false,
            output_size: 64 * 1024,
            next_time: 0,
        };
        decoder.set_output_type()?;

        let _ = unsafe {
            decoder
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
        };
        let _ = unsafe {
            decoder
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
        };
        Ok(decoder)
    }

    fn set_output_type(&mut self) -> Result<(), String> {
        let desired = pcm_output_type()?;
        // SAFETY: 先按协议要求的 PCM16 48k/2ch 去设；解码器拒绝时再从它
        // 自己列出的类型里挑一个同样规格的。
        unsafe {
            let set = self.transform.SetOutputType(0, &desired, 0);
            if set.is_err() {
                let mut index = 0u32;
                let mut chosen = None;
                loop {
                    let Ok(kind) = self.transform.GetOutputAvailableType(0, index) else {
                        break;
                    };
                    let subtype = kind.GetGUID(&MF_MT_SUBTYPE).ok();
                    let rate = kind.GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND).ok();
                    let channels = kind.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS).ok();
                    let bits = kind.GetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE).ok();
                    if subtype == Some(MFAudioFormat_PCM)
                        && rate == Some(TARGET_RATE)
                        && channels == Some(u32::from(TARGET_CHANNELS))
                        && bits == Some(u32::from(TARGET_BITS))
                    {
                        chosen = Some(kind);
                        break;
                    }
                    index = index.saturating_add(1);
                }
                let chosen =
                    chosen.ok_or_else(|| "AAC 解码器没有 48k/16bit/双声道 PCM 输出".to_string())?;
                self.transform
                    .SetOutputType(0, &chosen, 0)
                    .map_err(|e| format!("设置 AAC 解码输出格式失败: {e}"))?;
            }

            let info = self
                .transform
                .GetOutputStreamInfo(0)
                .map_err(|e| format!("问不到 AAC 解码器输出信息: {e}"))?;
            self.provides_samples = info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
            self.output_size = info.cbSize.max(64 * 1024);
        }
        Ok(())
    }

    fn decode(&mut self, bytes: &[u8], _timestamp_us: u64) -> Result<Vec<i16>, PlaybackError> {
        let time = self.next_time;
        let duration = FRAME_HNS;
        self.next_time = self.next_time.saturating_add(duration);

        let sample = make_sample(bytes, time, duration).map_err(PlaybackError::Frame)?;
        let mut pcm = Vec::<i16>::with_capacity(FRAME_SAMPLES * 2);

        // SAFETY: 喂一帧裸 AAC；NOTACCEPTING 时先取干净输出再试一次。
        match unsafe { self.transform.ProcessInput(0, &sample, 0) } {
            Ok(()) => {}
            Err(e) if e.code() == MF_E_NOTACCEPTING => {
                self.drain(&mut pcm)?;
                if let Err(again) = unsafe { self.transform.ProcessInput(0, &sample, 0) } {
                    if again.code() == MF_E_NOTACCEPTING {
                        return Ok(pcm);
                    }
                    return Err(PlaybackError::Frame(format!("喂 AAC 解码器失败: {again}")));
                }
            }
            Err(e) => return Err(PlaybackError::Frame(format!("喂 AAC 解码器失败: {e}"))),
        }

        self.drain(&mut pcm)?;
        Ok(pcm)
    }

    fn drain(&mut self, pcm: &mut Vec<i16>) -> Result<(), PlaybackError> {
        for _ in 0..16 {
            let mut output = MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: std::mem::ManuallyDrop::new(if self.provides_samples {
                    None
                } else {
                    Some(output_sample(self.output_size).map_err(PlaybackError::Frame)?)
                }),
                dwStatus: 0,
                pEvents: std::mem::ManuallyDrop::new(None),
            };
            let mut status = 0u32;
            // SAFETY: 解码器填写输出；无论成败都把样本取回来释放。
            let result = unsafe {
                self.transform
                    .ProcessOutput(0, std::slice::from_mut(&mut output), &mut status)
            };
            let sample = unsafe { std::mem::ManuallyDrop::take(&mut output.pSample) };
            let _events = unsafe { std::mem::ManuallyDrop::take(&mut output.pEvents) };

            match result {
                Ok(()) => {
                    if let Some(sample) = sample {
                        let bytes = read_sample_bytes(&sample).map_err(PlaybackError::Frame)?;
                        pcm.extend(
                            bytes
                                .chunks_exact(2)
                                .map(|pair| i16::from_le_bytes([pair[0], pair[1]])),
                        );
                    }
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(()),
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    // 解码器换了输出布局。能按原规格重设就继续，不能就退线程。
                    self.set_output_type().map_err(|e| {
                        PlaybackError::Fatal(format!("AAC 解码器重设输出格式失败: {e}"))
                    })?;
                }
                Err(e) => return Err(PlaybackError::Frame(format!("取 AAC 解码结果失败: {e}"))),
            }
        }
        Ok(())
    }
}

fn aac_decoder_input_type() -> Result<IMFMediaType, String> {
    // SAFETY: 创建空的媒体类型并逐项填裸 AAC-LC 的参数。
    let media_type =
        unsafe { MFCreateMediaType() }.map_err(|e| format!("建 AAC 输入类型失败: {e}"))?;
    set_guid(&media_type, &MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
    set_guid(&media_type, &MF_MT_SUBTYPE, &MFAudioFormat_AAC)?;
    set_u32(&media_type, &MF_MT_AUDIO_SAMPLES_PER_SECOND, TARGET_RATE)?;
    set_u32(
        &media_type,
        &MF_MT_AUDIO_NUM_CHANNELS,
        u32::from(TARGET_CHANNELS),
    )?;
    set_u32(
        &media_type,
        &MF_MT_AUDIO_BITS_PER_SAMPLE,
        u32::from(TARGET_BITS),
    )?;
    set_u32(&media_type, &MF_MT_AAC_PAYLOAD_TYPE, 0)?;
    set_u32(&media_type, &MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, 0x29)?;

    // HEAACWAVEINFO 去掉 WAVEFORMATEX 之后的 12 字节，再接 2 字节
    // AudioSpecificConfig：AAC-LC(2)、48 kHz(索引 3)、双声道。
    let user_data: [u8; 14] = [
        0x00, 0x00, // wPayloadType = 0，裸 AAC
        0x29, 0x00, // wAudioProfileLevelIndication = 0x29，AAC-LC
        0x00, 0x00, // wStructType
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // 保留字段，和 MS 文档示例一致
        0x11, 0x90, // AudioSpecificConfig
    ];
    // SAFETY: 只是把一段固定字节写进媒体类型属性。
    unsafe { media_type.SetBlob(&MF_MT_USER_DATA, &user_data) }
        .map_err(|e| format!("设置 AAC AudioSpecificConfig 失败: {e}"))?;

    Ok(media_type)
}

fn pcm_output_type() -> Result<IMFMediaType, String> {
    // SAFETY: 创建空的媒体类型并逐项填 PCM16 参数。
    let media_type =
        unsafe { MFCreateMediaType() }.map_err(|e| format!("建 PCM 输出类型失败: {e}"))?;
    set_guid(&media_type, &MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
    set_guid(&media_type, &MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
    set_u32(&media_type, &MF_MT_AUDIO_SAMPLES_PER_SECOND, TARGET_RATE)?;
    set_u32(
        &media_type,
        &MF_MT_AUDIO_NUM_CHANNELS,
        u32::from(TARGET_CHANNELS),
    )?;
    set_u32(
        &media_type,
        &MF_MT_AUDIO_BITS_PER_SAMPLE,
        u32::from(TARGET_BITS),
    )?;
    set_u32(
        &media_type,
        &MF_MT_AUDIO_BLOCK_ALIGNMENT,
        u32::from(TARGET_CHANNELS * 2),
    )?;
    set_u32(
        &media_type,
        &MF_MT_AUDIO_AVG_BYTES_PER_SECOND,
        TARGET_RATE * u32::from(TARGET_CHANNELS) * 2,
    )?;
    Ok(media_type)
}

struct Renderer {
    client: IAudioClient,
    render: IAudioRenderClient,
    buffer_size: u32,
}

impl Renderer {
    fn new() -> Result<Self, String> {
        let device = default_render_device()?;
        let client = activate_audio_client(&device)?;
        let format = target_waveformat();
        let flags = AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;

        // SAFETY: 用 shared render + AUTOCONVERT 谈 48k/16bit/双声道；
        // 缓冲目标 40 ms。format 是栈上的完整 WAVEFORMATEX。
        unsafe {
            client
                .Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    flags,
                    PLAY_BUFFER_HNS,
                    0,
                    &format,
                    None,
                )
                .map_err(|e| format!("播放设备不接受 48k/16bit/双声道: {e}"))?;
        }

        // SAFETY: 初始化成功后取缓冲大小和渲染服务，随后 Start。
        let (buffer_size, render) = unsafe {
            let size = client
                .GetBufferSize()
                .map_err(|e| format!("问不到播放缓冲大小: {e}"))?;
            let render: IAudioRenderClient = client
                .GetService()
                .map_err(|e| format!("拿不到音频渲染接口: {e}"))?;
            client
                .Start()
                .map_err(|e| format!("音频播放流启动失败: {e}"))?;
            (size, render)
        };

        Ok(Self {
            client,
            render,
            buffer_size,
        })
    }

    /// 把 PCM 写进设备。`epoch` 用于在 reset 时尽快退出写循环。
    fn write(&self, pcm: &[i16], epoch: u64) -> Result<(), PlaybackError> {
        let total_frames = pcm.len() / 2;
        let mut offset = 0usize;
        let mut stalls = 0u32;

        while offset < total_frames {
            if PLAY_EPOCH.load(Ordering::SeqCst) != epoch {
                return Ok(());
            }

            // SAFETY: 只查询当前设备缓冲里已经排队的帧数。
            let padding = unsafe { self.client.GetCurrentPadding() }
                .map_err(|e| PlaybackError::Fatal(format!("读取播放缓冲失败: {e}")))?;
            let available = self.buffer_size.saturating_sub(padding);
            if available == 0 {
                stalls = stalls.saturating_add(1);
                if stalls > 500 {
                    return Err(PlaybackError::Fatal("播放设备长时间不消费音频".into()));
                }
                std::thread::sleep(Duration::from_millis(2));
                continue;
            }
            stalls = 0;

            let frames = (total_frames - offset).min(available as usize) as u32;
            // SAFETY: GetBuffer 借出 frames 帧空间；下面只写 frames*4 字节，
            // 然后 ReleaseBuffer 交还。
            let target = unsafe { self.render.GetBuffer(frames) }
                .map_err(|e| PlaybackError::Fatal(format!("拿播放缓冲失败: {e}")))?;
            if target.is_null() {
                return Err(PlaybackError::Fatal("播放缓冲指针为空".into()));
            }
            // SAFETY: i16 切片是双声道交错的 PCM16LE，长度已经按帧限制；
            // 目标缓冲刚好有 frames*4 字节。两块内存不重叠。
            unsafe {
                ptr::copy_nonoverlapping(
                    pcm.as_ptr().add(offset * 2) as *const u8,
                    target,
                    frames as usize * 4,
                );
            }
            // SAFETY: 告诉 WASAPI 实际写了多少帧。
            unsafe { self.render.ReleaseBuffer(frames, 0) }
                .map_err(|e| PlaybackError::Fatal(format!("交还播放缓冲失败: {e}")))?;
            offset += frames as usize;
        }
        Ok(())
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        // SAFETY: 释放在前，停流只是让设备尽快回到空闲；失败也不影响释放。
        unsafe {
            let _ = self.client.Stop();
        }
    }
}

struct PlaybackPipeline {
    decoder: AacDecoder,
    renderer: Renderer,
    next_ts_us: u64,
    last_ts_us: u64,
    have_ts: bool,
}

impl PlaybackPipeline {
    fn new() -> Result<Self, String> {
        let decoder = AacDecoder::new()?;
        let renderer = Renderer::new()?;
        Ok(Self {
            decoder,
            renderer,
            next_ts_us: 0,
            last_ts_us: 0,
            have_ts: false,
        })
    }

    fn play(&mut self, bytes: &[u8], ts: u64, epoch: u64) -> Result<(), PlaybackError> {
        // 对端重开采集后时间戳会从 0 重新开始。只按“迟到”丢包会把新流
        // 一直丢到追上旧时钟，表现就是整场无声；这里检测大幅回退直接重对齐。
        if self.have_ts
            && (ts < self.last_ts_us || ts.saturating_add(1_000_000) < self.next_ts_us)
        {
            self.have_ts = false;
            self.next_ts_us = ts;
        }
        // 音频不重传：比播放时钟落后超过一帧的包直接丢，中间的空洞补静音。
        if self.have_ts {
            if ts.saturating_add(FRAME_US) < self.next_ts_us {
                return Ok(());
            }
            let mut filled = 0;
            while self.next_ts_us.saturating_add(FRAME_US) < ts && filled < 4 {
                let silence = [0i16; FRAME_SAMPLES * 2];
                self.renderer.write(&silence, epoch)?;
                self.next_ts_us = self.next_ts_us.saturating_add(FRAME_US);
                filled += 1;
            }
        } else {
            self.next_ts_us = ts;
            self.have_ts = true;
        }

        let pcm = self.decoder.decode(bytes, ts)?;
        if !pcm.is_empty() {
            self.renderer.write(&pcm, epoch)?;
        }
        self.last_ts_us = ts;
        self.next_ts_us = self.next_ts_us.max(ts).saturating_add(FRAME_US);
        Ok(())
    }
}

fn playback_thread(spawn_epoch: u64) {
    // SAFETY: 播放线程自己初始化 MTA，退出时配对。
    let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    let owns_com = com.is_ok();
    if com.is_err() && com != RPC_E_CHANGED_MODE {
        eprintln!("node: 音频播放线程初始化 COM 失败: {com}");
        mark_playback_stopped(spawn_epoch);
        return;
    }

    // SAFETY: MF 进程级初始化，退出时配对 MFShutdown。
    if let Err(e) = unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) } {
        eprintln!("node: 音频播放线程起不来，媒体框架初始化失败: {e}");
        if owns_com {
            unsafe { CoUninitialize() };
        }
        mark_playback_stopped(spawn_epoch);
        return;
    }

    let mut pipeline: Option<PlaybackPipeline> = None;
    // 从创建这条线程时的 epoch 开始。初始化 COM / MF 期间可能已经发生
    // reset；从 0 开始会在第一次循环里补一次多余的 ack，而用 spawn_epoch
    // 才能让 mark_playback_stopped 正确判断自己是不是被 reset 顶掉。
    let mut epoch = spawn_epoch;

    'outer: loop {
        let (frame, current_epoch) = {
            let mut queue = PLAY.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if queue.reset_epoch != epoch {
                    epoch = queue.reset_epoch;
                    break (None, epoch);
                }
                if let Some(frame) = queue.frames.pop_front() {
                    break (Some(frame), epoch);
                }

                let (next, _) = PLAY_CV
                    .wait_timeout(queue, Duration::from_millis(100))
                    .unwrap_or_else(|e| e.into_inner());
                queue = next;
            }
        };

        let Some((bytes, ts)) = frame else {
            // reset：先真正把解码器和设备放掉，再告诉 reset 可以返回。
            drop(pipeline.take());
            let mut queue = PLAY.lock().unwrap_or_else(|e| e.into_inner());
            queue.ack_epoch = queue.ack_epoch.max(current_epoch);
            drop(queue);
            PLAY_CV.notify_all();
            continue;
        };

        if pipeline.is_none() {
            match PlaybackPipeline::new() {
                Ok(value) => pipeline = Some(value),
                Err(e) => {
                    eprintln!("node: 音频播放管线建不起来: {e}");
                    break 'outer;
                }
            }
        }

        if let Some(active) = pipeline.as_mut() {
            match active.play(&bytes, ts, current_epoch) {
                Ok(()) => {}
                Err(PlaybackError::Frame(e)) => {
                    eprintln!("node: 丢掉一帧坏音频: {e}");
                }
                Err(PlaybackError::Fatal(e)) => {
                    eprintln!("node: 音频播放设备出错，停到下一帧重建: {e}");
                    break 'outer;
                }
            }
        }
    }

    drop(pipeline);
    unsafe {
        let _ = MFShutdown();
    };
    if owns_com {
        unsafe { CoUninitialize() };
    }
    mark_playback_stopped(epoch);
}

/// 播放线程退出时收尾。`worker_epoch` 是这条线程最后处理的 reset epoch。
fn mark_playback_stopped(worker_epoch: u64) {
    let mut queue = PLAY.lock().unwrap_or_else(|e| e.into_inner());
    // 锁里的 reset_epoch 已经比线程的 epoch 新，说明这条线程失败/退出期间
    // 调用方执行过 reset：reset 才是更新的意图，不能再按旧设备的失败给它
    // 设 2 秒退避，否则新会话开头会整段没声音。反过来 reset 晚于本函数时，
    // reset 自己会清掉 retry_at，两种顺序都能收敛。
    let reset_superseded = queue.reset_epoch != worker_epoch;
    queue.started = false;
    if reset_superseded {
        // reset 在持锁时已经清空了旧会话的帧，所以此刻队列里剩下的只可能
        // 是 reset 之后新会话挤进来的帧；旧线程收尾时不能再清一遍，否则
        // 新会话开头会被吞掉一截声音。
        queue.retry_at = None;
    } else {
        // 队列里是这条失败线程没来得及播的帧，和退出前清空保持一致，
        // 避免下一场先播一段旧声音。
        queue.frames.clear();
        // 设备被禁用/拔掉时，播放管线会连续建不起来。退避 2 秒再试，
        // 避免每 21ms 一帧都 spawn 一个线程、反复初始化 Media Foundation。
        queue.retry_at = Some(Instant::now() + Duration::from_secs(2));
    }
    // 线程已经没了，正在等 reset 的调用方不用再等设备关闭。
    queue.ack_epoch = queue.ack_epoch.max(queue.reset_epoch);
    drop(queue);
    PLAY_CV.notify_all();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::thread;

    /// 不关心代次的测试用这个值。它永远不等于当前代次，因此这些会话
    /// 收尾时不会推进全局代次，避免并行测试互相干扰。
    const STALE_GENERATION: u64 = u64::MAX;

    #[test]
    fn delayed_aac_outputs_keep_their_input_times() {
        let mut accepted = VecDeque::from([
            frame_timestamp_us(0),
            frame_timestamp_us(1),
            frame_timestamp_us(3),
        ]);
        let mut last = None;
        // 一个 drain 在第 4 帧到达后吐出先前积压的三帧；丢失的第 2 帧
        // 仍留出时间戳空洞，播放端才能补静音。
        assert_eq!(
            next_output_timestamp(&mut accepted, &mut last, frame_timestamp_us(4)),
            0
        );
        assert_eq!(
            next_output_timestamp(&mut accepted, &mut last, frame_timestamp_us(4)),
            frame_timestamp_us(1)
        );
        assert_eq!(
            next_output_timestamp(&mut accepted, &mut last, frame_timestamp_us(4)),
            frame_timestamp_us(3)
        );
        // 编码器意外多吐一帧时也不能与上一帧共用时间戳。
        assert_eq!(
            next_output_timestamp(&mut accepted, &mut last, frame_timestamp_us(4)),
            frame_timestamp_us(4)
        );
    }

    fn make_session(
        stop: Arc<AtomicBool>,
        alive: Arc<AtomicBool>,
        generation: u64,
        join: Option<thread::JoinHandle<()>>,
    ) -> CaptureSession {
        CaptureSession {
            stop,
            alive,
            generation,
            join,
        }
    }

    #[test]
    fn timed_out_session_is_not_running() {
        // 初始化超时后的状态：stop=true、alive=true。它不能再阻止下一次启动。
        let session = make_session(
            Arc::new(AtomicBool::new(true)),
            Arc::new(AtomicBool::new(true)),
            STALE_GENERATION,
            None,
        );
        assert!(!session.is_running());

        // 正常会话仍然是 running；线程已经退出的也不是。
        let session = make_session(
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(true)),
            STALE_GENERATION,
            None,
        );
        assert!(session.is_running());
        let session = make_session(
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
            STALE_GENERATION,
            None,
        );
        assert!(!session.is_running());
    }

    #[test]
    fn reclaim_stale_does_not_block_on_initializing_thread() {
        // 复现旧缺陷里的状态：初始化已经超时（stop=true），但线程还卡在
        // 驱动调用里（alive=true）。下一次 start_capture 取到它时不能等。
        let stop = Arc::new(AtomicBool::new(true));
        let alive = Arc::new(AtomicBool::new(true));
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let (done_tx, done_rx) = mpsc::channel::<()>();
        let thread_alive = alive.clone();
        // 模拟卡在 AudioClient::Initialize / MFTEnumEx 里的采集线程：
        // 它不检查 stop，直到驱动调用返回。
        let join = thread::spawn(move || {
            let _ = release_rx.recv();
            thread_alive.store(false, Ordering::SeqCst);
            let _ = done_tx.send(());
        });

        let started = Instant::now();
        make_session(stop.clone(), alive, STALE_GENERATION, Some(join)).reclaim_stale();
        let elapsed = started.elapsed();

        assert!(stop.load(Ordering::SeqCst));
        assert!(
            elapsed < Duration::from_millis(100),
            "stale 回收不能等初始化线程: {elapsed:?}"
        );

        // 线程被 detach 之后仍应能自己退出。
        release_tx.send(()).unwrap();
        done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("detach 的线程应自行退出");
    }

    #[test]
    fn stop_waits_bounded_and_detaches_stuck_thread() {
        let stop = Arc::new(AtomicBool::new(false));
        let alive = Arc::new(AtomicBool::new(true));
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let (done_tx, done_rx) = mpsc::channel::<()>();
        let thread_alive = alive.clone();
        let join = thread::spawn(move || {
            let _ = release_rx.recv();
            thread_alive.store(false, Ordering::SeqCst);
            let _ = done_tx.send(());
        });

        let started = Instant::now();
        make_session(stop.clone(), alive, STALE_GENERATION, Some(join)).stop();
        let elapsed = started.elapsed();

        assert!(stop.load(Ordering::SeqCst));
        assert!(elapsed >= CAPTURE_STOP_TIMEOUT, "stop 应等满上限: {elapsed:?}");
        assert!(
            elapsed < CAPTURE_STOP_TIMEOUT + Duration::from_secs(2),
            "stop 不能无界等待: {elapsed:?}"
        );

        release_tx.send(()).unwrap();
        done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("detach 的线程应自行退出");
    }

    #[test]
    fn stop_reclaims_exited_thread_without_waiting_for_timeout() {
        let stop = Arc::new(AtomicBool::new(false));
        let alive = Arc::new(AtomicBool::new(true));
        let thread_alive = alive.clone();
        let join = thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            thread_alive.store(false, Ordering::SeqCst);
        });

        let started = Instant::now();
        make_session(stop, alive, STALE_GENERATION, Some(join)).stop();
        assert!(
            started.elapsed() < CAPTURE_STOP_TIMEOUT,
            "线程已退出的正常停止不应等满上限: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn session_teardown_invalidates_only_its_generation() {
        // stop：当前代次失效，旧线程手里的帧到不了 sink。
        let generation = next_capture_generation();
        let stop = AtomicBool::new(false);
        assert!(capture_generation_is(generation));
        assert!(capture_live(&stop, generation));
        make_session(
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
            generation,
            None,
        )
        .stop();
        assert!(!capture_generation_is(generation));
        assert!(!capture_live(&stop, generation));

        // abandon（初始化超时 detach）同样让旧线程失效。
        let generation = next_capture_generation();
        make_session(
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
            generation,
            None,
        )
        .abandon();
        assert!(!capture_generation_is(generation));

        // 已经过期的会话收尾时不能把新会话一起顶掉。
        let stale = next_capture_generation();
        let current = next_capture_generation();
        make_session(
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
            stale,
            None,
        )
        .stop();
        assert!(capture_generation_is(current));

        // stop_capture 即使槽里没有会话也推进代次。
        stop_capture();
        assert!(!capture_generation_is(current));

        // reset 也推进代次。
        let current = next_capture_generation();
        reset();
        assert!(!capture_generation_is(current));
    }

    #[test]
    fn frame_delivery_requires_current_generation_and_no_stop() {
        // 用固定值而不是全局代次，测试之间互不干扰，也不依赖 Windows。
        let stale = 41u64;
        let current = 42u64;

        // 当前代次、没有 stop：这一帧可以交给 sink。
        assert!(frame_delivery_allowed(false, current, current));

        // 旧线程手里的帧即使 stop 还没被它看到，也必须丢掉。把代次判断
        // 删掉（只剩 !stop）时，这一行会失败。
        assert!(!frame_delivery_allowed(false, stale, current));

        // stop 已经立起来时，同代次的帧同样不能再进 sink。
        assert!(!frame_delivery_allowed(true, current, current));
        assert!(!frame_delivery_allowed(true, stale, current));

        // 只有“等于已发布代次”才放行；异常地更新或更旧的代次都不行。
        assert!(!frame_delivery_allowed(false, current + 1, current));
        assert!(!frame_delivery_allowed(false, current - 1, current));
    }
}
