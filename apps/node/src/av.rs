use std::sync::Mutex;
#[cfg(windows)]
use std::sync::atomic::{AtomicU8, Ordering};

use jr_proto::{DisplayInfo, InputEvent};

pub struct EncodedAu {
    pub idr: bool,
    pub bytes: Vec<u8>,
    pub ts: u64,
}

/// 一段 AAC-LC。时间戳沿用采集端时钟，播放端只用它排缓冲。
pub struct EncodedAudio {
    pub bytes: Vec<u8>,
    pub ts: u64,
}

#[derive(Clone, Copy)]
pub struct ViewInput {
    pub kind: u8,
    pub a: i32,
    pub b: i32,
}

impl ViewInput {
    /// 按下/抬起事件里的 b 是 1/0。失焦后抬起仍要转发。
    pub fn is_release(&self) -> bool {
        (self.kind == 2 || self.kind == 3) && self.b == 0
    }
}

struct Queues {
    aus: Vec<EncodedAu>,
    audios: Vec<EncodedAudio>,
    inputs: Vec<ViewInput>,
    menus: Vec<(u8, i32)>,
    /// 软编提示只报一次。macOS 和 Windows 都走这条判断。
    #[cfg(any(target_os = "macos", windows))]
    warned_software: bool,
}

static QUEUES: Mutex<Queues> = Mutex::new(Queues {
    aus: Vec::new(),
    audios: Vec::new(),
    inputs: Vec::new(),
    menus: Vec::new(),
    #[cfg(any(target_os = "macos", windows))]
    warned_software: false,
});

#[cfg(windows)]
static COVER_REQUEST: AtomicU8 = AtomicU8::new(0);

#[cfg(target_os = "macos")]
mod ffi {
    use super::{EncodedAu, QUEUES};

    unsafe extern "C" {
        pub fn jrp_screen_granted() -> i32;
        pub fn jrp_display_count() -> i32;
        pub fn jrp_display_at(index: i32, ox: *mut i32, oy: *mut i32, w: *mut u32, h: *mut u32, scale_milli: *mut u32);
        pub fn jrp_capture_start(bitrate: i32, display_index: i32, cb: extern "C" fn(*const u8, i32, u8, u64, *mut std::ffi::c_void), ctx: *mut std::ffi::c_void) -> i32;
        pub fn jrp_capture_stop();
        pub fn jrp_capture_force_idr();
        pub fn jrp_capture_set_bitrate(bitrate: i32);
        pub fn jrp_capture_set_fps(fps: i32);
        pub fn jrp_capture_running() -> i32;
        pub fn jrp_capture_failed() -> i32;
        pub fn jrp_capture_hardware() -> i32;
        pub fn jrp_audio_ready() -> i32;
        pub fn jrp_audio_running() -> i32;
        pub fn jrp_audio_start(cb: extern "C" fn(*const u8, i32, u64, *mut std::ffi::c_void), ctx: *mut std::ffi::c_void) -> i32;
        pub fn jrp_audio_stop();
        pub fn jrp_audio_play(bytes: *const u8, len: i32, ts: u64);
        pub fn jrp_audio_reset();
        pub fn jrp_view_request_open(width: u32, height: u32);
        pub fn jrp_view_reset();
        pub fn jrp_view_present(bytes: *const u8, len: i32, idr: u8);
        pub fn jrp_view_take_need_keyframe() -> i32;
        pub fn jrp_view_set_input(cb: extern "C" fn(u8, i32, i32, i32, *mut std::ffi::c_void), ctx: *mut std::ffi::c_void);
        pub fn jrp_view_set_menu(cb: extern "C" fn(u8, i32, *mut std::ffi::c_void), ctx: *mut std::ffi::c_void);
        pub fn jrp_view_set_displays(packed: *const std::ffi::c_char);
        pub fn jrp_view_set_quality(profile: i32);
        pub fn jrp_view_set_display(index: i32);
        pub fn jrp_view_focused() -> i32;
        pub fn jrp_indicator(on: i32);
        pub fn jrp_privacy_cover(on: i32);
        pub fn jrp_privacy_cover_on() -> i32;
        pub fn jrp_clipboard_set(text: *const std::ffi::c_char, len: i32);
        pub fn jrp_clipboard_get(buf: *mut std::ffi::c_char, cap: i32) -> i32;
        pub fn jrp_take_hangup() -> i32;
        pub fn jrp_cursor(x: *mut i32, y: *mut i32);
        pub fn jrp_cursor_shape_request();
        pub fn jrp_cursor_shape(out: *mut u8, cap: i32) -> i32;
        pub fn jrp_view_set_cursor(x: i32, y: i32, visible: i32);
        pub fn jrp_view_set_cursor_shape(bgra: *const u8, w: i32, h: i32, hx: i32, hy: i32);
        pub fn jrp_view_pump();
    }

    extern "C" fn on_au(bytes: *const u8, len: i32, idr: u8, ts: u64, _ctx: *mut std::ffi::c_void) {
        if bytes.is_null() || len <= 0 {
            return;
        }
        let slice = unsafe { std::slice::from_raw_parts(bytes, len as usize) };
        if let Ok(mut q) = QUEUES.lock() {
            if q.aus.len() >= 3 {
                q.aus.remove(0);
            }
            q.aus.push(EncodedAu { idr: idr != 0, bytes: slice.to_vec(), ts });
        }
    }

    extern "C" fn on_input(kind: u8, a: i32, b: i32, _c: i32, _ctx: *mut std::ffi::c_void) {
        if let Ok(mut q) = QUEUES.lock() {
            if q.inputs.len() < 64 {
                q.inputs.push(super::ViewInput { kind, a, b });
            }
        }
    }

    extern "C" fn on_menu(kind: u8, value: i32, _ctx: *mut std::ffi::c_void) {
        if let Ok(mut q) = QUEUES.lock() {
            q.menus.push((kind, value));
        }
    }

    pub fn bind_input() {
        unsafe { jrp_view_set_input(on_input, std::ptr::null_mut()) }
    }

    pub fn bind_menu() {
        unsafe { jrp_view_set_menu(on_menu, std::ptr::null_mut()) }
    }

    pub fn push_displays(packed: &str) {
        if let Ok(text) = std::ffi::CString::new(packed) {
            unsafe { jrp_view_set_displays(text.as_ptr()) }
        }
    }

    extern "C" fn on_audio(bytes: *const u8, len: i32, ts: u64, _ctx: *mut std::ffi::c_void) {
        if bytes.is_null() || len <= 0 {
            return;
        }
        let slice = unsafe { std::slice::from_raw_parts(bytes, len as usize) };
        if let Ok(mut q) = QUEUES.lock() {
            // 编码器或采集队列卡住时不要无限堆内存；声音宁可丢一帧。
            if q.audios.len() >= 64 {
                q.audios.remove(0);
            }
            q.audios.push(super::EncodedAudio { bytes: slice.to_vec(), ts });
        }
    }

    pub fn start_audio(bitrate: i32) -> i32 {
        let _ = bitrate;
        unsafe { jrp_audio_start(on_audio, std::ptr::null_mut()) }
    }

    pub fn start(bitrate: i32, display_index: i32) -> i32 {
        unsafe { jrp_capture_start(bitrate, display_index, on_au, std::ptr::null_mut()) }
    }
}

pub fn screen_granted() -> bool {
    #[cfg(target_os = "macos")]
    {
        unsafe { ffi::jrp_screen_granted() == 1 }
    }
    #[cfg(not(target_os = "macos"))]
    {
        // 别的平台交给各自的适配器。适配器没有这条能力时就报 false。
        jr_host_win::screen_granted()
    }
}

pub fn local_displays() -> Vec<DisplayInfo> {
    #[cfg(target_os = "macos")]
    {
        let n = unsafe { ffi::jrp_display_count() }.clamp(0, 8);
        let mut out = Vec::new();
        for i in 0..n {
            let mut ox = 0i32;
            let mut oy = 0i32;
            let mut w = 0u32;
            let mut h = 0u32;
            let mut scale = 1000u32;
            unsafe { ffi::jrp_display_at(i, &mut ox, &mut oy, &mut w, &mut h, &mut scale) };
            if w == 0 || h == 0 {
                continue;
            }
            out.push(DisplayInfo {
                display_id: i as u32,
                origin_x: ox,
                origin_y: oy,
                width: w,
                height: h,
                scale_milli: scale,
                refresh_hz: 60,
                is_virtual: false,
            });
        }
        out
    }
    #[cfg(not(target_os = "macos"))]
    {
        jr_host_win::local_displays()
            .into_iter()
            .enumerate()
            .map(|(i, d)| DisplayInfo {
                display_id: i as u32,
                origin_x: d.origin_x,
                origin_y: d.origin_y,
                width: d.width,
                height: d.height,
                scale_milli: d.scale_milli,
                refresh_hz: 60,
                is_virtual: false,
            })
            .collect()
    }
}

pub fn start_capture(bitrate: i32, display_index: i32) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        if !screen_granted() {
            return Err("需授权".into());
        }
        ffi::bind_input();
        let code = ffi::start(bitrate, display_index);
        if code == 1 {
            return Err("需授权".into());
        }
        if code != 0 {
            return Err("采集没有起来".into());
        }
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        if !screen_granted() {
            return Err("需授权".into());
        }
        // 适配器在别的线程回帧，这里把它塞进和 macOS 一样的队列，
        // 上层只认 drain_aus()，不用知道自己跑在哪个系统上。
        let sink: Box<dyn FnMut(jr_host_win::AccessUnit) + Send> = Box::new(|au| {
            if let Ok(mut q) = QUEUES.lock() {
                q.aus.push(EncodedAu {
                    idr: au.idr,
                    bytes: au.bytes,
                    ts: au.ts_us,
                });
            }
        });
        jr_host_win::start_capture(bitrate, display_index, sink)
    }
}

pub fn stop_capture() {
    #[cfg(target_os = "macos")]
    unsafe {
        ffi::jrp_capture_stop();
    }
    #[cfg(not(target_os = "macos"))]
    jr_host_win::stop_capture();
    if let Ok(mut q) = QUEUES.lock() {
        q.aus.clear();
    }
}

pub fn capture_failed() -> bool {
    #[cfg(target_os = "macos")]
    {
        unsafe { ffi::jrp_capture_failed() == 1 }
    }
    #[cfg(windows)]
    {
        jr_host_win::capture_failed()
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        // 这些平台没有采集适配器，start 会直接返回错误。
        false
    }
}

pub fn capture_running() -> bool {
    #[cfg(target_os = "macos")]
    {
        unsafe { ffi::jrp_capture_running() == 1 }
    }
    #[cfg(not(target_os = "macos"))]
    {
        jr_host_win::capture_running()
    }
}

pub fn note_encoder() -> Option<&'static str> {
    #[cfg(target_os = "macos")]
    {
        let hw = unsafe { ffi::jrp_capture_hardware() };
        if hw == 0 {
            if let Ok(mut q) = QUEUES.lock() {
                if !q.warned_software {
                    q.warned_software = true;
                    return Some("此设备正在软件编码，延迟会高");
                }
            }
        }
        None
    }
    #[cfg(windows)]
    {
        if jr_host_win::capture_running() && jr_host_win::hardware_encoder() == Some(false) {
            if let Ok(mut q) = QUEUES.lock() {
                if !q.warned_software {
                    q.warned_software = true;
                    return Some("此设备正在软件编码，延迟会高");
                }
            }
        }
        None
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        None
    }
}

pub fn force_idr() {
    #[cfg(target_os = "macos")]
    unsafe {
        ffi::jrp_capture_force_idr();
    }
    #[cfg(not(target_os = "macos"))]
    jr_host_win::force_idr();
}

pub fn set_bitrate(bitrate: i32) {
    #[cfg(target_os = "macos")]
    unsafe {
        ffi::jrp_capture_set_bitrate(bitrate);
    }
    #[cfg(not(target_os = "macos"))]
    jr_host_win::set_bitrate(bitrate);
}

pub fn set_fps(fps: u16) {
    #[cfg(target_os = "macos")]
    unsafe {
        ffi::jrp_capture_set_fps(fps as i32);
    }
    #[cfg(not(target_os = "macos"))]
    jr_host_win::set_fps(fps);
}

pub fn drain_aus() -> Vec<EncodedAu> {
    QUEUES.lock().map(|mut q| std::mem::take(&mut q.aus)).unwrap_or_default()
}

/// 本机能不能把系统播放的声音采成 AAC。macOS 依赖录屏权限；
/// Windows 的 WASAPI loopback 不需要单独授权。
pub fn audio_capture_ready() -> bool {
    #[cfg(target_os = "macos")]
    {
        unsafe { ffi::jrp_audio_ready() == 1 }
    }
    #[cfg(windows)]
    {
        jr_host_win::audio::capture_ready()
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        false
    }
}

/// 本机能不能播放远端 AAC。两边都不是靠第三方播放器。
pub fn audio_playback_ready() -> bool {
    #[cfg(target_os = "macos")]
    {
        true
    }
    #[cfg(windows)]
    {
        jr_host_win::audio::playback_ready()
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        false
    }
}

pub fn start_audio_capture(bitrate: i32) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        if !audio_capture_ready() {
            return Err("需授权".into());
        }
        if ffi::start_audio(bitrate) != 0 {
            return Err("音频采集没有起来".into());
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        let sink: Box<dyn FnMut(Vec<u8>, u64) + Send> = Box::new(|bytes, ts| {
            if let Ok(mut q) = QUEUES.lock() {
                if q.audios.len() >= 64 {
                    q.audios.remove(0);
                }
                q.audios.push(EncodedAudio { bytes, ts });
            }
        });
        jr_host_win::audio::start_capture(bitrate, sink)
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = bitrate;
        Err("这个系统还没有音频采集适配器".into())
    }
}

/// 采集线程/音频输出是否仍在工作。启动函数返回成功不代表它会一直活着。
pub fn audio_capture_running() -> bool {
    #[cfg(target_os = "macos")]
    {
        unsafe { ffi::jrp_audio_running() == 1 }
    }
    #[cfg(windows)]
    {
        jr_host_win::audio::running()
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        false
    }
}

pub fn stop_audio_capture() {
    #[cfg(target_os = "macos")]
    unsafe {
        ffi::jrp_audio_stop();
    }
    #[cfg(windows)]
    jr_host_win::audio::stop_capture();
    if let Ok(mut q) = QUEUES.lock() {
        q.audios.clear();
    }
}

pub fn drain_audios() -> Vec<EncodedAudio> {
    QUEUES.lock().map(|mut q| std::mem::take(&mut q.audios)).unwrap_or_default()
}

pub fn play_audio(bytes: &[u8], ts: u64) {
    if bytes.is_empty() {
        return;
    }
    #[cfg(target_os = "macos")]
    unsafe {
        ffi::jrp_audio_play(bytes.as_ptr(), bytes.len() as i32, ts);
    }
    #[cfg(windows)]
    {
        jr_host_win::audio::play(bytes, ts);
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = (bytes, ts);
    }
}

pub fn reset_audio() {
    #[cfg(target_os = "macos")]
    unsafe {
        ffi::jrp_audio_reset();
    }
    #[cfg(windows)]
    jr_host_win::audio::reset();
}

pub fn drain_input() -> Vec<ViewInput> {
    QUEUES.lock().map(|mut q| std::mem::take(&mut q.inputs)).unwrap_or_default()
}

pub fn drain_menu() -> Vec<(u8, i32)> {
    QUEUES.lock().map(|mut q| std::mem::take(&mut q.menus)).unwrap_or_default()
}

/// 会话窗口的画质和显示器下拉框：kind 1 是画质 1-4，kind 2 是显示器序号。
pub fn set_displays(names: &[String]) {
    #[cfg(target_os = "macos")]
    ffi::push_displays(&names.join("\n"));
    #[cfg(windows)]
    {
        jr_host_win::view::set_displays(names);
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = names;
    }
}

/// 让观看窗口的画质下拉跟随当前档位。窗口没开时会先记下，开窗后补上。
pub fn set_view_quality(profile: u8) {
    #[cfg(target_os = "macos")]
    unsafe {
        ffi::jrp_view_set_quality(i32::from(profile));
    }
    #[cfg(windows)]
    {
        jr_host_win::view::set_quality(profile);
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = profile;
    }
}

/// 让显示器下拉跟随当前选中的显示器序号。
pub fn set_view_display(index: usize) {
    #[cfg(target_os = "macos")]
    unsafe {
        ffi::jrp_view_set_display(index.min(i32::MAX as usize) as i32);
    }
    #[cfg(windows)]
    {
        jr_host_win::view::set_display(index.min(i32::MAX as usize) as i32);
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = index;
    }
}

pub fn bind_view() {
    #[cfg(target_os = "macos")]
    {
        ffi::bind_input();
        ffi::bind_menu();
    }
}

pub fn request_view(width: u32, height: u32) {
    #[cfg(target_os = "macos")]
    unsafe {
        ffi::bind_input();
        ffi::bind_menu();
        ffi::jrp_view_request_open(width, height);
    }
    #[cfg(windows)]
    {
        jr_host_win::view::request_open(width, height);
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = (width, height);
    }
}

pub fn reset_view() {
    #[cfg(target_os = "macos")]
    unsafe {
        ffi::jrp_view_reset();
    }
    #[cfg(windows)]
    {
        jr_host_win::view::reset_session();
    }
}

pub fn present(bytes: &[u8], idr: bool) {
    #[cfg(target_os = "macos")]
    unsafe {
        ffi::jrp_view_present(bytes.as_ptr(), bytes.len() as i32, u8::from(idr));
    }
    #[cfg(windows)]
    {
        jr_host_win::view::present(bytes, idr);
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = (bytes, idr);
    }
}

pub fn view_focused() -> bool {
    #[cfg(target_os = "macos")]
    {
        unsafe { ffi::jrp_view_focused() == 1 }
    }
    #[cfg(windows)]
    {
        jr_host_win::view::focused()
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        false
    }
}

pub fn take_view_keyframe_request() -> bool {
    #[cfg(target_os = "macos")]
    {
        unsafe { ffi::jrp_view_take_need_keyframe() != 0 }
    }
    #[cfg(windows)]
    {
        jr_host_win::view::take_need_keyframe()
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        false
    }
}

pub fn indicator(on: bool) {
    #[cfg(target_os = "macos")]
    unsafe {
        ffi::jrp_indicator(i32::from(on));
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = on;
    }
}

/// 系统剪贴板最多读这么多字节。
#[cfg(target_os = "macos")]
const CLIP_MAX: usize = 1 << 20;

/// 把文本放进本机剪贴板。只在对面明确发过来的时候调，不做后台同步。
pub fn clipboard_set(text: &str) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let bytes = text.as_bytes();
        if bytes.len() > CLIP_MAX {
            return Err("剪贴板内容太长".into());
        }
        unsafe { ffi::jrp_clipboard_set(bytes.as_ptr() as *const std::ffi::c_char, bytes.len() as i32) };
        Ok(())
    }
    #[cfg(target_os = "windows")]
    {
        jr_host_win::clipboard::set_text(text)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = text;
        Err("这个系统还没有剪贴板适配器".into())
    }
}

/// 读本机剪贴板里的文本。不是文本或太长就返回 None。
pub fn clipboard_get() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let mut buf = vec![0u8; CLIP_MAX];
        let n = unsafe { ffi::jrp_clipboard_get(buf.as_mut_ptr() as *mut std::ffi::c_char, CLIP_MAX as i32) };
        if n <= 0 {
            return None;
        }
        buf.truncate(n as usize);
        String::from_utf8(buf).ok()
    }
    #[cfg(target_os = "windows")]
    {
        jr_host_win::clipboard::get_text()
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        None
    }
}

/// 当前平台有没有系统剪贴板适配器。能力协商要用它，别报一个用不了的能力。
pub fn clipboard_ready() -> bool {
    cfg!(any(target_os = "macos", target_os = "windows"))
}

/// 软件遮挡层。开了以后站在机器前只能看到提示，但截图和录屏不受影响，
/// 所以节点必须把这件事当成降级，而不是完整的隐私屏。
pub fn privacy_cover(on: bool) {
    #[cfg(target_os = "macos")]
    unsafe {
        ffi::jrp_privacy_cover(i32::from(on));
    }
    #[cfg(not(target_os = "macos"))]
    {
        #[cfg(windows)]
        COVER_REQUEST.store(if on { 1 } else { 2 }, Ordering::Release);
        #[cfg(not(windows))]
        jr_host_win::privacy_cover(on);
    }
}

pub fn privacy_cover_on() -> bool {
    #[cfg(target_os = "macos")]
    {
        unsafe { ffi::jrp_privacy_cover_on() == 1 }
    }
    #[cfg(not(target_os = "macos"))]
    {
        jr_host_win::privacy_cover_on()
    }
}

pub fn take_hangup() -> bool {
    #[cfg(target_os = "macos")]
    {
        unsafe { ffi::jrp_take_hangup() == 1 }
    }
    #[cfg(windows)]
    {
        jr_host_win::view::take_hangup()
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        false
    }
}

/// 光标在桌面全局坐标系里的位置和可见性。
pub fn cursor_state() -> Option<(i32, i32, bool)> {
    #[cfg(target_os = "macos")]
    {
        let mut x = 0i32;
        let mut y = 0i32;
        unsafe { ffi::jrp_cursor(&mut x, &mut y) };
        Some((x, y, true))
    }
    #[cfg(windows)]
    {
        jr_host_win::cursor_state()
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        None
    }
}

/// 请主线程下一步抓一次当前系统光标。抓取和读缓存分开，AppKit 调用不跨线程。
pub fn request_cursor_shape() {
    #[cfg(target_os = "macos")]
    unsafe {
        ffi::jrp_cursor_shape_request();
    }
}

/// 夹一次光标形状抓取，AppKit 那边再快也只有 4 次/秒。
#[cfg(target_os = "macos")]
static LAST_SHAPE_POLL: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);

/// 取上一次抓到并编码好的光标形状。没有变化时调用方自己按内容去重。
pub fn cursor_shape() -> Option<jr_proto::CursorShape> {
    #[cfg(target_os = "macos")]
    {
        {
            let now = std::time::Instant::now();
            let mut last = LAST_SHAPE_POLL.lock().ok()?;
            if let Some(at) = *last {
                if now.duration_since(at) < std::time::Duration::from_millis(250) {
                    return None;
                }
            }
            *last = Some(now);
        }
        let mut buf = vec![0u8; jr_proto::CURSOR_SHAPE_HEADER + (jr_proto::CURSOR_SHAPE_MAX as usize) * (jr_proto::CURSOR_SHAPE_MAX as usize) * 4];
        let n = unsafe { ffi::jrp_cursor_shape(buf.as_mut_ptr(), buf.len() as i32) };
        if n <= 0 {
            return None;
        }
        buf.truncate(n as usize);
        jr_proto::CursorShape::decode(&buf).ok()
    }
    #[cfg(windows)]
    {
        jr_host_win::cursor_shape()
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        None
    }
}

/// 主控端把远端光标位置交给观看窗口。窗口自己决定怎么画。
pub fn set_cursor_pos(x: i32, y: i32, visible: bool) {
    #[cfg(target_os = "macos")]
    unsafe {
        ffi::jrp_view_set_cursor(x, y, i32::from(visible));
    }
    #[cfg(windows)]
    {
        jr_host_win::view::set_cursor_pos(x, y, visible);
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = (x, y, visible);
    }
}

pub fn set_cursor_shape(shape: &jr_proto::CursorShape) {
    #[cfg(target_os = "macos")]
    unsafe {
        ffi::jrp_view_set_cursor_shape(
            shape.bgra.as_ptr(),
            i32::from(shape.width),
            i32::from(shape.height),
            i32::from(shape.hotspot_x),
            i32::from(shape.hotspot_y),
        );
    }
    #[cfg(windows)]
    {
        jr_host_win::view::set_cursor_shape(shape);
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = shape;
    }
}

pub fn pump_ui() {
    #[cfg(target_os = "macos")]
    unsafe {
        ffi::jrp_view_pump();
    }
    #[cfg(windows)]
    {
        match COVER_REQUEST.swap(0, Ordering::AcqRel) {
            1 => jr_host_win::privacy_cover(true),
            2 => jr_host_win::privacy_cover(false),
            _ => {},
        }
        jr_host_win::view::pump();
        for input in jr_host_win::view::drain_input() {
            if let Ok(mut q) = QUEUES.lock() {
                if q.inputs.len() < 64 {
                    q.inputs.push(ViewInput { kind: input.kind, a: input.a, b: input.b });
                }
            }
        }
        for menu in jr_host_win::view::drain_menu() {
            if let Ok(mut q) = QUEUES.lock() {
                q.menus.push(menu);
            }
        }
    }
}

pub fn view_event(input: ViewInput) -> Option<InputEvent> {
    match input.kind {
        1 => Some(InputEvent::move_abs(0, input.a, input.b)),
        2 => Some(InputEvent {
            event_id: 0,
            kind: InputEvent::BUTTON,
            payload: vec![input.a as u8, input.b as u8],
        }),
        3 => {
            // Windows 观看窗口送来的 a 已经是 HID 用法；macOS 窗口送的是
            // 虚拟键码，仍然要过一遍转换表。
            #[cfg(windows)]
            let event = Some(InputEvent::key(0, input.a as u16, input.b != 0));
            #[cfg(target_os = "macos")]
            let event = {
                let usage = jr_input::hid_from_mac(input.a as u16)?;
                Some(InputEvent::key(0, usage, input.b != 0))
            };
            #[cfg(not(any(windows, target_os = "macos")))]
            let event: Option<InputEvent> = None;
            event
        }
        // 4 是滚轮：a 是纵向、b 是横向的原始读数。
        4 => {
            let (dx, dy) = jr_input::wheel_delta(input.a as f64, input.b as f64);
            Some(InputEvent::wheel(0, dx, dy))
        }
        _ => None,
    }
}
