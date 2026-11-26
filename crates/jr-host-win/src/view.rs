//! Windows 观看窗口。
//!
//! 和 macOS 一样是系统窗口，不是网页壳。顶栏放画质、显示器和断开，
//! 画面区按远端分辨率等比居中，留边上的点击不发给对面。
//!
//! **窗口和解码器写完了，没有在任何一台 Windows 机器上打开过。**
//! 坐标换算不碰系统，在别的平台上有测试。Media Foundation 解码、
//! 窗口消息和首帧时序只有真机能验。

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::sync::Mutex;

/// 观看窗口回给节点的一条输入。编号和 macOS `jrp_input` 一致：
/// 1 移动，2 按键，3 键盘（HID 用法），4 滚轮。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ViewInput {
    pub kind: u8,
    pub a: i32,
    pub b: i32,
}

impl ViewInput {
    /// 抬起事件在窗口失焦后也必须送出去，否则远端会一直按着。
    pub fn is_release(&self) -> bool {
        (self.kind == KIND_BUTTON || self.kind == KIND_KEY) && self.b == 0
    }
}

pub const KIND_MOVE: u8 = 1;
pub const KIND_BUTTON: u8 = 2;
pub const KIND_KEY: u8 = 3;
pub const KIND_WHEEL: u8 = 4;
/// 菜单：1 画质 1-4，2 显示器序号。和 macOS 观看窗口同一套。
pub const MENU_QUALITY: u8 = 1;
pub const MENU_DISPLAY: u8 = 2;

/// 画面在客户区里的矩形。坐标是客户区像素，原点在左上。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoRect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// 把远端画面等比塞进客户区，多出来的留边。宽或高是 0 时铺满。
pub fn video_rect(client_w: i32, client_h: i32, remote_w: u32, remote_h: u32) -> VideoRect {
    let cw = client_w.max(0);
    let ch = client_h.max(0);
    if remote_w == 0 || remote_h == 0 || cw == 0 || ch == 0 {
        return VideoRect { x: 0, y: 0, w: cw, h: ch };
    }
    let scale_w = cw as i64 * 1000 / remote_w as i64;
    let scale_h = ch as i64 * 1000 / remote_h as i64;
    let scale = scale_w.min(scale_h).max(1);
    let w = ((remote_w as i64 * scale) / 1000).clamp(1, cw as i64) as i32;
    let h = ((remote_h as i64 * scale) / 1000).clamp(1, ch as i64) as i32;
    VideoRect {
        x: (cw - w) / 2,
        y: (ch - h) / 2,
        w,
        h,
    }
}

/// 客户区坐标换成远端像素。落在留边上返回 None，调用方不要发移动。
/// y 从窗口左上往下数，和 Win32 客户区一致。
pub fn map_pointer(rect: VideoRect, px: i32, py: i32, remote_w: u32, remote_h: u32) -> Option<(i32, i32)> {
    if remote_w == 0 || remote_h == 0 || rect.w <= 0 || rect.h <= 0 {
        return None;
    }
    if px < rect.x || py < rect.y || px >= rect.x + rect.w || py >= rect.y + rect.h {
        return None;
    }
    let x = ((px - rect.x) as i64 * remote_w as i64 / rect.w as i64).clamp(0, remote_w as i64 - 1) as i32;
    let y = ((py - rect.y) as i64 * remote_h as i64 / rect.h as i64).clamp(0, remote_h as i64 - 1) as i32;
    Some((x, y))
}

/// 收进来、还没送去解码的一帧。非 Windows 没有解码器，字段不会被读。
#[cfg_attr(not(windows), allow(dead_code))]
struct PendingFrame {
    bytes: Vec<u8>,
}

/// 主控端画在画面上的远端光标。rgba 在存进来时就换成 BGRA，
/// StretchDIBits 的 BI_RGB 要这个顺序。
struct CursorState {
    x: i32,
    y: i32,
    visible: bool,
    width: u32,
    height: u32,
    hot_x: i32,
    hot_y: i32,
    bgra: Vec<u8>,
}

impl Default for CursorState {
    fn default() -> Self {
        Self {
            x: 0,
            y: 0,
            visible: false,
            width: 0,
            height: 0,
            hot_x: 0,
            hot_y: 0,
            bgra: Vec::new(),
        }
    }
}

static CURSOR: Mutex<CursorState> = Mutex::new(CursorState {
    x: 0,
    y: 0,
    visible: false,
    width: 0,
    height: 0,
    hot_x: 0,
    hot_y: 0,
    bgra: Vec::new(),
});

static CURSOR_DIRTY: AtomicBool = AtomicBool::new(false);

pub fn set_cursor_pos(x: i32, y: i32, visible: bool) {
    if let Ok(mut cur) = CURSOR.lock() {
        if cur.x != x || cur.y != y || cur.visible != visible {
            cur.x = x;
            cur.y = y;
            cur.visible = visible;
            CURSOR_DIRTY.store(true, Ordering::Release);
        }
    }
}

pub fn set_cursor_shape(shape: &jr_proto::CursorShape) {
    if let Ok(mut cur) = CURSOR.lock() {
        cur.width = u32::from(shape.width);
        cur.height = u32::from(shape.height);
        cur.hot_x = i32::from(shape.hotspot_x);
        cur.hot_y = i32::from(shape.hotspot_y);
        cur.bgra.clear();
        cur.bgra.extend_from_slice(&shape.bgra);
        CURSOR_DIRTY.store(true, Ordering::Release);
    }
}

struct ViewState {
    open: bool,
    width: u32,
    height: u32,
    quality: u8,
    display: i32,
    displays: Vec<String>,
    frames: Vec<PendingFrame>,
    wait_idr: bool,
    inputs: Vec<ViewInput>,
    menus: Vec<(u8, i32)>,
    focused: bool,
    hangup: bool,
    need_keyframe: bool,
}

impl Default for ViewState {
    fn default() -> Self {
        Self {
            open: false,
            width: 1280,
            height: 720,
            quality: 2,
            display: 0,
            displays: Vec::new(),
            frames: Vec::new(),
            wait_idr: true,
            inputs: Vec::new(),
            menus: Vec::new(),
            focused: false,
            hangup: false,
            need_keyframe: false,
        }
    }
}

static VIEW: Mutex<ViewState> = Mutex::new(ViewState {
    open: false,
    width: 1280,
    height: 720,
    quality: 2,
    display: 0,
    displays: Vec::new(),
    frames: Vec::new(),
    wait_idr: true,
    inputs: Vec::new(),
    menus: Vec::new(),
    focused: false,
    hangup: false,
    need_keyframe: false,
});

static OPEN_REQ: AtomicBool = AtomicBool::new(false);
static QUALITY_REQ: AtomicU32 = AtomicU32::new(0);
static DISPLAY_REQ: AtomicI32 = AtomicI32::new(-1);

fn with_view<T>(f: impl FnOnce(&mut ViewState) -> T) -> Option<T> {
    VIEW.lock().ok().map(|mut v| f(&mut v))
}

/// 请求打开或调整观看窗口。真正建窗口发生在主线程 `pump`。
pub fn request_open(width: u32, height: u32) {
    if let Some(()) = with_view(|v| {
        if width > 0 {
            v.width = width;
        }
        if height > 0 {
            v.height = height;
        }
        v.open = true;
    }) {
        OPEN_REQ.store(true, Ordering::Release);
    }
}

pub fn set_quality(profile: u8) {
    if (1..=4).contains(&profile) {
        QUALITY_REQ.store(u32::from(profile), Ordering::Release);
    }
}

pub fn set_display(index: i32) {
    DISPLAY_REQ.store(index.max(0), Ordering::Release);
}

pub fn set_displays(names: &[String]) {
    let _ = with_view(|v| {
        v.displays = names.to_vec();
    });
}

/// 收一帧 Annex-B。非关键帧在等到 IDR 之前丢掉，积压时丢掉旧帧并要关键帧。
pub fn present(bytes: &[u8], idr: bool) {
    if bytes.is_empty() {
        return;
    }
    let _ = with_view(|v| {
        if idr || v.frames.len() >= 8 {
            v.frames.clear();
            if !idr {
                v.wait_idr = true;
                v.need_keyframe = true;
                return;
            }
        }
        if v.wait_idr && !idr {
            // 首帧 IDR 丢了时，不能只是把后续帧全丢掉：对端不会知道
            // 主控没画面，必须主动要一次关键帧，否则窗口会一直黑着。
            v.need_keyframe = true;
            return;
        }
        if idr {
            v.wait_idr = false;
        }
        v.frames.push(PendingFrame { bytes: bytes.to_vec() });
    });
}

/// Windows 和 NSEvent 的正方向一致（向前/向上），这里只把 Windows 的
/// 120 单位折成节点使用的像素读数；方向取反由节点统一做。
#[cfg_attr(not(windows), allow(dead_code))]
fn windows_wheel_delta_for_node(delta: i32) -> i32 {
    let step = (f64::from(delta) / 60.0).round() as i32;
    if step == 0 { delta.signum() } else { step }
}

/// `AlphaBlend` 的 `AC_SRC_ALPHA` 要求源像素预乘 alpha。
/// 协议里的 COLOR 光标是普通 BGRA，半透明边缘直接画会偏亮。
#[cfg_attr(not(windows), allow(dead_code))]
fn premultiply_bgra(bgra: &[u8]) -> Vec<u8> {
    let mut out = bgra.to_vec();
    for px in out.chunks_exact_mut(4) {
        let a = u16::from(px[3]);
        for c in &mut px[..3] {
            *c = ((u16::from(*c) * a + 127) / 255) as u8;
        }
    }
    out
}

pub fn take_need_keyframe() -> bool {
    with_view(|v| {
        let need = v.need_keyframe;
        v.need_keyframe = false;
        need
    })
    .unwrap_or(false)
}

pub fn focused() -> bool {
    VIEW.lock().ok().map(|v| v.focused).unwrap_or(false)
}

pub fn take_hangup() -> bool {
    with_view(|v| {
        let on = v.hangup;
        v.hangup = false;
        on
    })
    .unwrap_or(false)
}

pub fn drain_input() -> Vec<ViewInput> {
    with_view(|v| std::mem::take(&mut v.inputs)).unwrap_or_default()
}

pub fn drain_menu() -> Vec<(u8, i32)> {
    with_view(|v| std::mem::take(&mut v.menus)).unwrap_or_default()
}

/// 主线程每拍调。非 Windows 上只消化请求，不建窗口。
pub fn pump() {
    let quality = QUALITY_REQ.swap(0, Ordering::AcqRel);
    let display = DISPLAY_REQ.swap(-1, Ordering::AcqRel);
    let open = OPEN_REQ.swap(false, Ordering::AcqRel);
    let _ = with_view(|v| {
        if quality >= 1 && quality <= 4 {
            v.quality = quality as u8;
        }
        if display >= 0 {
            v.display = display;
        }
    });
    #[cfg(windows)]
    {
        window::pump(open, open);
    }
    #[cfg(not(windows))]
    {
        let _ = open;
    }
}

/// 测试和窗口过程共用的输入入口。失焦时不收移动和按下，但抬起要放行。
pub fn push_input(input: ViewInput) {
    let _ = with_view(|v| {
        if (v.focused || input.is_release()) && v.inputs.len() < 64 {
            v.inputs.push(input);
        }
    });
}

pub fn push_menu(kind: u8, value: i32) {
    let _ = with_view(|v| v.menus.push((kind, value)));
}

pub fn set_focused(on: bool) {
    let _ = with_view(|v| v.focused = on);
}

pub fn request_hangup() {
    let _ = with_view(|v| {
        // 会话结束后不再接受输入，已经排队的按键也不带进下一段会话。
        v.focused = false;
        v.inputs.retain(|input| input.kind != KIND_KEY);
        v.hangup = true;
    });
}

/// 会话结束：丢掉上一段的帧和远端光标，下一段重新等 IDR。
/// 窗口可能留着等下一次邀请，不能把旧画面带过去。
pub fn reset_session() {
    let _ = with_view(|v| {
        v.frames.clear();
        v.wait_idr = true;
        v.need_keyframe = false;
        // 上一段会话排队但没发出去的输入不能带进下一段：那时按键可能已经
        // 在物理上松开了，补发一次抬起会让远端凭空收到没人按下的松开。
        v.inputs.clear();
    });
    #[cfg(windows)]
    window::clear_view_buttons();
    if let Ok(mut cur) = CURSOR.lock() {
        cur.visible = false;
        cur.width = 0;
        cur.height = 0;
        cur.bgra.clear();
    }
    CURSOR_DIRTY.store(true, Ordering::Release);
    #[cfg(windows)]
    window::reset_frame();
}

/// 测试用：窗口是否已被请求打开。
#[cfg(test)]
pub fn is_open() -> bool {
    VIEW.lock().ok().map(|v| v.open).unwrap_or(false)
}

#[cfg(windows)]
mod window {
    use std::cell::RefCell;
    use std::ptr;
    use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
    use std::sync::Mutex;

    use windows::core::w;
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, RPC_E_CHANGED_MODE, WPARAM};
    use windows::Win32::Graphics::Gdi::{
        AlphaBlend, BeginPaint, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject,
        EndPaint, FillRect, GetStockObject, InvalidateRect, ScreenToClient, SelectObject,
        SetBkMode, SetTextColor, StretchDIBits, TextOutW, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
        BLACK_BRUSH, DEFAULT_GUI_FONT, DIB_RGB_COLORS, HBRUSH, HDC, PAINTSTRUCT, SRCCOPY,
        WHITE_BRUSH, AC_SRC_ALPHA, AC_SRC_OVER, BLENDFUNCTION,
    };
    use windows::Win32::Media::MediaFoundation::{
        CLSID_MSH264DecoderMFT, IMFSample, IMFTransform, MFCreateMediaType, MFCreateMemoryBuffer,
        MFCreateSample, MFStartup, MFVideoFormat_H264, MFVideoFormat_NV12, MFVideoInterlace_Progressive, MFMediaType_Video,
        MFVideoTransferMatrix_BT709, MF_E_BUFFERTOOSMALL, MF_E_NOTACCEPTING,
        MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE, MF_MT_DEFAULT_STRIDE,
        MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE,
        MF_MT_YUV_MATRIX, MFSTARTUP_FULL, MF_VERSION, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
        MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER,
        MFT_OUTPUT_STREAM_PROVIDES_SAMPLES,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
    };
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DispatchMessageW, GetClientRect, GetDlgItem,
        IsWindowVisible, PeekMessageW, RegisterClassW, SendMessageW, SetForegroundWindow, ShowWindow,
        TranslateMessage, BS_PUSHBUTTON, CBS_DROPDOWNLIST, CB_ADDSTRING, CB_GETCURSEL,
        CB_RESETCONTENT, CB_SETCURSEL, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, HMENU, MSG,
        PM_REMOVE, SW_HIDE, SW_SHOW, WINDOW_EX_STYLE, WINDOW_STYLE, WM_CLOSE, WM_COMMAND,
        WM_DESTROY, WM_KEYDOWN, WM_KEYUP, WM_KILLFOCUS, WM_LBUTTONDOWN, WM_LBUTTONUP,
        WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_PAINT,
        WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SETFOCUS, WM_SETFONT, WM_SIZE, WM_SYSKEYDOWN,
        WM_SYSKEYUP, WNDCLASSW, WS_CHILD, WS_OVERLAPPEDWINDOW, WS_TABSTOP, WS_VISIBLE, WS_VSCROLL,
        BN_CLICKED, CBN_SELCHANGE,
    };

    use super::{
        map_pointer, video_rect, VideoRect, ViewInput, KIND_BUTTON, KIND_MOVE, KIND_WHEEL, MENU_DISPLAY,
        MENU_QUALITY,
    };

    const ID_QUALITY: i32 = 201;
    const ID_DISPLAY: i32 = 202;
    const ID_HANGUP: i32 = 203;
    const ID_HINT: i32 = 204;
    const BAR_H: i32 = 36;

    static HWND_RAW: AtomicIsize = AtomicIsize::new(0);
    static CLASS_READY: AtomicBool = AtomicBool::new(false);
    static CREATE_LOGGED: AtomicBool = AtomicBool::new(false);
    /// 观看窗口里按下的鼠标键。拖出画面再松开时仍要补发抬起。
    static VIEW_BUTTONS: Mutex<[bool; 3]> = Mutex::new([false; 3]);
    /// 显示器下拉框上次填入的选项。pump 每 8ms 跑一次，内容没变就不要
    /// CB_RESETCONTENT，否则用户刚展开的列表会被清空。
    static DISPLAY_NAMES: Mutex<Vec<String>> = Mutex::new(Vec::new());

    /// 解码完成、可以直接给 StretchDIBits 的一帧。
    struct DecodedFrame {
        width: u32,
        height: u32,
        bgra: Vec<u8>,
    }

    static DECODED: Mutex<Option<DecodedFrame>> = Mutex::new(None);

    struct DecoderSlot {
        decoder: Option<Decoder>,
        tried: bool,
    }

    thread_local! {
        /// 解码器只在界面线程建一次。COM 对象不跨线程，存放也用线程本地。
        static DECODER: RefCell<DecoderSlot> = const {
            RefCell::new(DecoderSlot { decoder: None, tried: false })
        };
    }

    /// Media Foundation H.264 解码器。输入 Annex-B，输出 NV12。
    struct Decoder {
        transform: IMFTransform,
        provides_samples: bool,
        buffer_size: u32,
        frame_w: u32,
        frame_h: u32,
        next_time: i64,
    }

    impl Decoder {
        fn create(width: u32, height: u32) -> windows::core::Result<Self> {
            unsafe {
                // 界面线程可能已经被别的组件初始化过 COM。套间模式不同会
                // 返回 RPC_E_CHANGED_MODE，直接用现有套间即可，不算失败。
                let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
                if hr.is_err() && hr != RPC_E_CHANGED_MODE {
                    return Err(windows::core::Error::from_hresult(hr));
                }
                MFStartup(MF_VERSION, MFSTARTUP_FULL)?;

                let transform: IMFTransform = CoCreateInstance(
                    &CLSID_MSH264DecoderMFT,
                    None::<&windows::core::IUnknown>,
                    CLSCTX_INPROC_SERVER,
                )?;

                let input = MFCreateMediaType()?;
                input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
                input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
                transform.SetInputType(0, &input, 0)?;

                let mut decoder = Self {
                    transform,
                    provides_samples: false,
                    buffer_size: 0,
                    frame_w: width.max(2),
                    frame_h: height.max(2),
                    next_time: 0,
                };
                decoder.set_output_type(decoder.frame_w, decoder.frame_h)?;
                let _ = decoder
                    .transform
                    .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0);
                let _ = decoder
                    .transform
                    .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);
                Ok(decoder)
            }
        }

        /// 先把输出类型定成 NV12 并带上帧尺寸；有的解码器在首帧前不认
        /// 这个类型，就退回到它自己列出的 NV12 类型。
        fn set_output_type(&mut self, width: u32, height: u32) -> windows::core::Result<()> {
            unsafe {
                let output = MFCreateMediaType()?;
                output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
                output.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
                output.SetUINT64(
                    &MF_MT_FRAME_SIZE,
                    (u64::from(width) << 32) | u64::from(height),
                )?;
                output.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
                output.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
                if self.transform.SetOutputType(0, &output, 0).is_err() {
                    self.pick_available_output()?;
                }
                self.refresh_output_info()
            }
        }

        fn pick_available_output(&mut self) -> windows::core::Result<()> {
            unsafe {
                let mut index = 0u32;
                loop {
                    let Ok(kind) = self.transform.GetOutputAvailableType(0, index) else {
                        break;
                    };
                    if kind.GetGUID(&MF_MT_SUBTYPE).ok() == Some(MFVideoFormat_NV12) {
                        self.transform.SetOutputType(0, &kind, 0)?;
                        return Ok(());
                    }
                    index += 1;
                }
                Err(windows::core::Error::from_hresult(
                    windows::Win32::Foundation::E_FAIL,
                ))
            }
        }

        fn refresh_output_info(&mut self) -> windows::core::Result<()> {
            let info = unsafe { self.transform.GetOutputStreamInfo(0)? };
            self.provides_samples = info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
            self.buffer_size = info.cbSize;
            Ok(())
        }

        fn decode(&mut self, bytes: &[u8]) -> Option<DecodedFrame> {
            if bytes.is_empty() {
                return None;
            }
            let sample = self.input_sample(bytes)?;
            let mut latest = None;
            match unsafe { self.transform.ProcessInput(0, &sample, 0) } {
                Ok(()) => {}
                Err(e) if e.code() == MF_E_NOTACCEPTING => {
                    // 先把积压的输出取空，再喂这一帧。
                    self.drain(&mut latest);
                    if unsafe { self.transform.ProcessInput(0, &sample, 0) }.is_err() {
                        return latest;
                    }
                }
                Err(_) => return latest,
            }
            self.drain(&mut latest);
            latest
        }

        fn input_sample(&mut self, bytes: &[u8]) -> Option<IMFSample> {
            let len = u32::try_from(bytes.len()).ok()?;
            unsafe {
                let buffer = MFCreateMemoryBuffer(len).ok()?;
                let mut data = ptr::null_mut();
                buffer.Lock(&mut data, None, None).ok()?;
                if data.is_null() {
                    let _ = buffer.Unlock();
                    return None;
                }
                ptr::copy_nonoverlapping(bytes.as_ptr(), data, bytes.len());
                let _ = buffer.Unlock();
                buffer.SetCurrentLength(len).ok()?;

                let sample = MFCreateSample().ok()?;
                sample.AddBuffer(&buffer).ok()?;
                // 解码器不靠时间戳出帧，但按 30fps 递增更贴近真实流。
                let _ = sample.SetSampleTime(self.next_time);
                self.next_time = self.next_time.saturating_add(333_333);
                Some(sample)
            }
        }

        fn output_sample(&self) -> Option<IMFSample> {
            let needed = self
                .frame_w
                .saturating_mul(self.frame_h)
                .saturating_mul(3)
                / 2;
            let size = self.buffer_size.max(needed).max(1);
            unsafe {
                let buffer = MFCreateMemoryBuffer(size).ok()?;
                let sample = MFCreateSample().ok()?;
                sample.AddBuffer(&buffer).ok()?;
                Some(sample)
            }
        }

        /// 轮询输出。NEED_MORE_INPUT 表示这一帧喂完了；STREAM_CHANGE 表示
        /// 解码器换了输出格式，必须重新协商 NV12 类型再继续取。
        fn drain(&mut self, latest: &mut Option<DecodedFrame>) {
            for _ in 0..8 {
                let mut output = MFT_OUTPUT_DATA_BUFFER {
                    dwStreamID: 0,
                    pSample: std::mem::ManuallyDrop::new(None),
                    dwStatus: 0,
                    pEvents: std::mem::ManuallyDrop::new(None),
                };
                if !self.provides_samples {
                    match self.output_sample() {
                        Some(sample) => {
                            output.pSample = std::mem::ManuallyDrop::new(Some(sample));
                        }
                        None => return,
                    }
                }

                let mut status = 0u32;
                let mut buffers = [output];
                let result = unsafe { self.transform.ProcessOutput(0, &mut buffers, &mut status) };
                // 这两个字段是 ManuallyDrop，取出来才会释放。
                let sample = unsafe { std::mem::ManuallyDrop::take(&mut buffers[0].pSample) };
                let _events = unsafe { std::mem::ManuallyDrop::take(&mut buffers[0].pEvents) };
                match result {
                    Ok(()) => {
                        if let Some(sample) = sample {
                            if let Some(frame) = self.frame_from_sample(&sample) {
                                *latest = Some(frame);
                            }
                        }
                    }
                    Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => break,
                    Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                        if self.renegotiate_output().is_err() {
                            break;
                        }
                    }
                    Err(e) if e.code() == MF_E_BUFFERTOOSMALL => {
                        // 分辨率变了但输出类型还没刷新，按新尺寸再要一块。
                        match self.output_format() {
                            Some((width, height, _)) => {
                                self.frame_w = width as u32;
                                self.frame_h = height as u32;
                            }
                            None => break,
                        }
                    }
                    Err(_) => break,
                }
            }
        }

        fn renegotiate_output(&mut self) -> windows::core::Result<()> {
            self.pick_available_output()?;
            self.refresh_output_info()?;
            if let Some((width, height, _)) = self.output_format() {
                self.frame_w = width as u32;
                self.frame_h = height as u32;
            }
            Ok(())
        }

        fn frame_from_sample(&mut self, sample: &IMFSample) -> Option<DecodedFrame> {
            let (width, height, stride) = self.output_format()?;
            unsafe {
                let buffer = sample.ConvertToContiguousBuffer().ok()?;
                let mut data = ptr::null_mut();
                let mut len = 0u32;
                if buffer.Lock(&mut data, None, Some(&mut len)).is_err() {
                    return None;
                }
                if data.is_null() || len == 0 {
                    let _ = buffer.Unlock();
                    return None;
                }
                let src = std::slice::from_raw_parts(data, len as usize);
                let mut bgra = Vec::with_capacity(width.saturating_mul(height).saturating_mul(4));
                crate::h264::nv12_to_bgra(src, stride, width, height, &mut bgra);
                let _ = buffer.Unlock();
                if bgra.is_empty() {
                    return None;
                }
                self.frame_w = width as u32;
                self.frame_h = height as u32;
                Some(DecodedFrame {
                    width: width as u32,
                    height: height as u32,
                    bgra,
                })
            }
        }

        fn output_format(&self) -> Option<(usize, usize, usize)> {
            unsafe {
                let kind = self.transform.GetOutputCurrentType(0).ok()?;
                let frame_size = kind.GetUINT64(&MF_MT_FRAME_SIZE).ok();
                let (width, height) = match frame_size {
                    Some(size) => ((size >> 32) as usize, (size & 0xffff_ffff) as usize),
                    // 个别解码器不给这个属性，退回输出类型协商时的尺寸。
                    None => (self.frame_w as usize, self.frame_h as usize),
                };
                if width == 0 || height == 0 {
                    return None;
                }
                // NV12 的行距通常按 16 对齐；拿不到就按宽度走。
                let stride = kind
                    .GetUINT32(&MF_MT_DEFAULT_STRIDE)
                    .ok()
                    .map(|v| v as i32)
                    .filter(|v| *v > 0)
                    .map(|v| v as usize)
                    .unwrap_or(width);
                Some((width, height, stride.max(width)))
            }
        }
    }

    pub(super) fn reset_frame() {
        if let Ok(mut decoded) = DECODED.lock() {
            *decoded = None;
        }
    }

    /// 把积压的 Annex-B 帧送去解码，成功后更新上屏用的最新帧。
    /// 解码器建不起来只报一次，窗口继续显示等待提示。
    fn decode_pending() -> bool {
        let (width, height, frames) = match super::VIEW.lock() {
            Ok(mut v) => (v.width, v.height, std::mem::take(&mut v.frames)),
            Err(_) => return false,
        };
        if frames.is_empty() {
            return false;
        }

        let mut latest = None;
        DECODER.with(|slot| {
            let Ok(mut slot) = slot.try_borrow_mut() else {
                return;
            };
            if !slot.tried {
                slot.tried = true;
                match Decoder::create(width.max(2), height.max(2)) {
                    Ok(decoder) => slot.decoder = Some(decoder),
                    Err(e) => {
                        eprintln!("node: windows-view H.264 解码器初始化失败: {e:?}");
                        return;
                    }
                }
            }
            let Some(decoder) = slot.decoder.as_mut() else {
                return;
            };
            for frame in frames {
                if let Some(decoded) = decoder.decode(&frame.bytes) {
                    latest = Some(decoded);
                }
            }
        });

        let Some(frame) = latest else {
            return false;
        };
        if let Ok(mut decoded) = DECODED.lock() {
            *decoded = Some(frame);
        }
        if let Some(hwnd) = live_hwnd() {
            let hint = item(hwnd, ID_HINT);
            if !hint.0.is_null() {
                unsafe {
                    let _ = ShowWindow(hint, SW_HIDE);
                }
            }
            unsafe {
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
        }
        true
    }

    pub fn pump(open: bool, show_on_open: bool) {
        if open || super::OPEN_REQ.load(Ordering::Relaxed) {
            let _ = ensure_window(show_on_open);
        }
        apply_chrome();
        decode_pending();
        if super::CURSOR_DIRTY.swap(false, Ordering::AcqRel) {
            if let Some(hwnd) = live_hwnd() {
                unsafe {
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
            }
        }
        drain_messages();
    }

    fn ensure_window(show_hidden: bool) -> bool {
        if let Some(hwnd) = live_hwnd() {
            // 用户点右上角关闭时窗口只是隐藏，句柄还留着。新会话再开窗
            // 必须把它显示出来，不能只因为句柄还在就当作已经打开。
            if show_hidden {
                unsafe {
                    // 已显示（包括最小化）就不要每拍抢焦点；只把隐藏的窗口叫回来。
                    if !IsWindowVisible(hwnd).as_bool() {
                        let _ = ShowWindow(hwnd, SW_SHOW);
                    }
                    let _ = SetForegroundWindow(hwnd);
                }
            }
            return true;
        }
        register_class();
        let created = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("JarynRemoteView"),
                w!("Jaryn Remote"),
                WINDOW_STYLE(WS_OVERLAPPEDWINDOW.0),
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                960,
                600,
                None,
                None,
                instance(),
                None,
            )
        };
        let Ok(hwnd) = created else {
            if !CREATE_LOGGED.swap(true, Ordering::Relaxed) {
                eprintln!("node: windows-view 窗口建不起来");
            }
            return false;
        };
        HWND_RAW.store(hwnd.0 as isize, Ordering::Relaxed);
        let _ = child(hwnd, "COMBOBOX", "", WS_TABSTOP.0 | WS_VSCROLL.0 | CBS_DROPDOWNLIST as u32, ID_QUALITY);
        let _ = child(hwnd, "COMBOBOX", "", WS_TABSTOP.0 | WS_VSCROLL.0 | CBS_DROPDOWNLIST as u32, ID_DISPLAY);
        let _ = child(hwnd, "BUTTON", "断开", WS_TABSTOP.0 | BS_PUSHBUTTON as u32, ID_HANGUP);
        let _ = child(hwnd, "STATIC", "等待远端画面…", 0, ID_HINT);
        fill_quality(hwnd);
        if let Ok(mut cached) = DISPLAY_NAMES.lock() {
            cached.clear();
        }
        if DECODED.lock().map(|f| f.is_some()).unwrap_or(false) {
            // 窗口重建时已经有上一帧，别再让提示盖住画面。
            let hint = item(hwnd, ID_HINT);
            if !hint.0.is_null() {
                unsafe {
                    let _ = ShowWindow(hint, SW_HIDE);
                }
            }
        }
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOW);
            let _ = SetForegroundWindow(hwnd);
        }
        super::set_focused(true);
        layout(hwnd);
        true
    }

    fn apply_chrome() {
        let Some(hwnd) = live_hwnd() else {
            return;
        };
        let snap = super::VIEW.lock().ok().map(|v| (v.quality, v.display, v.displays.clone()));
        let Some((quality, display, names)) = snap else {
            return;
        };
        set_quality_sel(item(hwnd, ID_QUALITY), quality);
        fill_displays(hwnd, &names, display);
    }

    fn fill_quality(hwnd: HWND) {
        let combo = item(hwnd, ID_QUALITY);
        for label in crate::desk::QUALITY_LABELS {
            let wide = wide_z(label);
            unsafe {
                let _ = SendMessageW(combo, CB_ADDSTRING, None, Some(LPARAM(wide.as_ptr() as isize)));
            }
        }
        unsafe {
            let _ = SendMessageW(combo, CB_SETCURSEL, Some(windows::Win32::Foundation::WPARAM(1)), None);
        }
    }

    fn fill_displays(hwnd: HWND, names: &[String], selected: i32) {
        let combo = item(hwnd, ID_DISPLAY);
        if combo.0.is_null() {
            return;
        }
        let Ok(mut cached) = DISPLAY_NAMES.lock() else {
            return;
        };
        if cached.as_slice() != names {
            unsafe {
                let _ = SendMessageW(combo, CB_RESETCONTENT, None, None);
                for name in names {
                    let wide = wide_z(name);
                    let _ = SendMessageW(combo, CB_ADDSTRING, None, Some(LPARAM(wide.as_ptr() as isize)));
                }
            }
            *cached = names.to_vec();
        }
        if !names.is_empty() {
            let at = selected.clamp(0, names.len() as i32 - 1) as isize;
            let cur = unsafe { SendMessageW(combo, CB_GETCURSEL, None, None).0 };
            if cur != at {
                unsafe {
                    let _ = SendMessageW(combo, CB_SETCURSEL, Some(windows::Win32::Foundation::WPARAM(at as usize)), None);
                }
            }
        }
    }

    fn set_quality_sel(hwnd: HWND, profile: u8) {
        if hwnd.0.is_null() {
            return;
        }
        let want = (profile.clamp(1, 4) - 1) as isize;
        let cur = unsafe { SendMessageW(hwnd, CB_GETCURSEL, None, None).0 };
        if cur != want {
            unsafe {
                let _ = SendMessageW(hwnd, CB_SETCURSEL, Some(windows::Win32::Foundation::WPARAM(want as usize)), None);
            }
        }
    }

    unsafe extern "system" fn view_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
        match msg {
            WM_COMMAND => {
                on_command(hwnd, w);
                LRESULT(0)
            }
            WM_MOUSEMOVE
            | WM_LBUTTONDOWN
            | WM_LBUTTONUP
            | WM_RBUTTONDOWN
            | WM_RBUTTONUP
            | WM_MBUTTONDOWN
            | WM_MBUTTONUP => {
                on_mouse(hwnd, msg, l);
                LRESULT(0)
            }
            WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
                on_wheel(hwnd, msg, w, l);
                LRESULT(0)
            }
            WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP => {
                on_key(msg, w, l);
                LRESULT(0)
            }
            WM_SETFOCUS => {
                super::set_focused(true);
                LRESULT(0)
            }
            WM_KILLFOCUS => {
                super::set_focused(false);
                // 失焦后不再发移动，但按下中的鼠标键必须在远端补抬起。
                // 只靠 30 秒兜底会让对面一直以为键还按着。
                release_view_buttons();
                LRESULT(0)
            }
            WM_SIZE => {
                layout(hwnd);
                unsafe {
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
                LRESULT(0)
            }
            WM_PAINT => {
                paint(hwnd);
                LRESULT(0)
            }
            WM_CLOSE => {
                super::request_hangup();
                let _ = ShowWindow(hwnd, windows::Win32::UI::WindowsAndMessaging::SW_HIDE);
                LRESULT(0)
            }
            WM_DESTROY => {
                if HWND_RAW.load(Ordering::Relaxed) == hwnd.0 as isize {
                    HWND_RAW.store(0, Ordering::Relaxed);
                }
                release_view_buttons();
                super::set_focused(false);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, w, l),
        }
    }

    fn on_command(hwnd: HWND, w: WPARAM) {
        let id = (w.0 & 0xffff) as i32;
        let note = ((w.0 >> 16) & 0xffff) as u32;
        match (id, note) {
            (ID_HANGUP, BN_CLICKED) => super::request_hangup(),
            (ID_QUALITY, CBN_SELCHANGE) => {
                let index = unsafe { SendMessageW(item(hwnd, ID_QUALITY), CB_GETCURSEL, None, None).0 };
                if (0..4).contains(&index) {
                    super::push_menu(MENU_QUALITY, (index + 1) as i32);
                }
            }
            (ID_DISPLAY, CBN_SELCHANGE) => {
                let index = unsafe { SendMessageW(item(hwnd, ID_DISPLAY), CB_GETCURSEL, None, None).0 };
                if index >= 0 {
                    super::push_menu(MENU_DISPLAY, index as i32);
                }
            }
            _ => {}
        }
    }

    /// 把窗口里还按着的鼠标键补发抬起。焦点丢失和窗口销毁都要走这里。
    fn release_view_buttons() {
        let Ok(mut held) = VIEW_BUTTONS.lock() else {
            return;
        };
        for (which, down) in held.iter_mut().enumerate() {
            if std::mem::replace(down, false) {
                super::push_input(ViewInput { kind: KIND_BUTTON, a: which as i32, b: 0 });
            }
        }
    }

    /// 会话结束只清本地标记，不往对面补发：那时会话已经不在了。
    pub(super) fn clear_view_buttons() {
        if let Ok(mut held) = VIEW_BUTTONS.lock() {
            *held = [false; 3];
        }
    }

    fn on_mouse(hwnd: HWND, msg: u32, l: LPARAM) {
        let px = (l.0 & 0xffff) as i16 as i32;
        let py = ((l.0 >> 16) & 0xffff) as i16 as i32;
        let Some((remote_w, remote_h, rect)) = picture(hwnd) else {
            return;
        };
        let inside = map_pointer(rect, px, py, remote_w, remote_h);
        let button = match msg {
            WM_LBUTTONDOWN => Some((0, 1)),
            WM_LBUTTONUP => Some((0, 0)),
            WM_RBUTTONDOWN => Some((1, 1)),
            WM_RBUTTONUP => Some((1, 0)),
            WM_MBUTTONDOWN => Some((2, 1)),
            WM_MBUTTONUP => Some((2, 0)),
            _ => None,
        };
        if let Some((which, down)) = button {
            let Ok(mut held) = VIEW_BUTTONS.lock() else {
                return;
            };
            if down != 0 {
                // 只有按在画面里才算远端按下。
                let Some((x, y)) = inside else {
                    return;
                };
                held[which as usize] = true;
                super::push_input(ViewInput { kind: KIND_MOVE, a: x, b: y });
                super::push_input(ViewInput { kind: KIND_BUTTON, a: which, b: 1 });
            } else if std::mem::replace(&mut held[which as usize], false) {
                // 按下时在画面里，拖到留边或窗口外再松开也必须把抬起发出去，
                // 否则被控端会一直以为键还按着。
                super::push_input(ViewInput { kind: KIND_BUTTON, a: which, b: 0 });
            }
            return;
        }
        let Some((x, y)) = inside else {
            return;
        };
        // 聚焦状态只由 WM_SETFOCUS / WM_KILLFOCUS 改。鼠标移到非活动窗口
        // 上不算聚焦，否则失焦后还会继续把移动发出去。
        super::push_input(ViewInput { kind: KIND_MOVE, a: x, b: y });
    }

    /// 滚轮消息的 lParam 是屏幕坐标，先换成客户区再判断有没有落在画面上。
    fn on_wheel(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) {
        let delta = ((w.0 >> 16) & 0xffff) as i16 as i32;
        if delta == 0 {
            return;
        }
        let mut pt = POINT {
            x: (l.0 & 0xffff) as i16 as i32,
            y: ((l.0 >> 16) & 0xffff) as i16 as i32,
        };
        unsafe {
            if !ScreenToClient(hwnd, &mut pt).as_bool() {
                return;
            }
        }
        let Some((remote_w, remote_h, rect)) = picture(hwnd) else {
            return;
        };
        if map_pointer(rect, pt.x, pt.y, remote_w, remote_h).is_none() {
            return;
        }
        // 节点那条路把读数当成 macOS 的像素滚动量，再乘 60 还原成被控端
        // 的滚轮单位。一个 Windows 刻度是 120，这里先折成 2。
        let step = super::windows_wheel_delta_for_node(delta);
        let (a, b) = if msg == WM_MOUSEWHEEL { (step, 0) } else { (0, step) };
        super::push_input(ViewInput { kind: KIND_WHEEL, a, b });
    }

    fn on_key(msg: u32, w: WPARAM, l: LPARAM) {
        let vk = specific_vk(w.0 as u16, l.0);
        let Some(usage) = jr_input::hid_from_windows_vk(vk) else {
            return;
        };
        let down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
        super::push_input(ViewInput {
            kind: super::KIND_KEY,
            a: i32::from(usage),
            b: if down { 1 } else { 0 },
        });
    }

    /// WM_KEYDOWN 对 Shift/Ctrl/Alt 只给通用码，用扫描码和扩展位分左右。
    fn specific_vk(vk: u16, lp: isize) -> u16 {
        match vk {
            0x10 => {
                let scan = ((lp >> 16) & 0xff) as u8;
                if scan == 0x36 {
                    0xA1 // right shift
                } else {
                    0xA0 // left shift
                }
            }
            0x11 => {
                if lp & (1 << 24) != 0 {
                    0xA3 // right control
                } else {
                    0xA2 // left control
                }
            }
            0x12 => {
                if lp & (1 << 24) != 0 {
                    0xA5 // right alt
                } else {
                    0xA4 // left alt
                }
            }
            _ => vk,
        }
    }

    /// 客户区里的画面矩形。y 已经加上顶栏高度，和鼠标坐标同一套。
    ///
    /// 尺寸优先用解码帧自己的宽高。能力里的第一块屏可能不是当前串流的屏，
    /// 主控切到分辨率不同的副屏时，用能力尺寸会让画面、点击和光标错位。
    fn picture(hwnd: HWND) -> Option<(u32, u32, VideoRect)> {
        let decoded = DECODED.lock().ok().and_then(|frame| {
            frame.as_ref().and_then(|f| {
                (f.width > 0 && f.height > 0).then_some((f.width, f.height))
            })
        });
        let (width, height) = match decoded {
            Some(size) => size,
            None => {
                let v = super::VIEW.lock().ok()?;
                (v.width, v.height)
            }
        };
        let mut rc = RECT::default();
        unsafe {
            if GetClientRect(hwnd, &mut rc).is_err() {
                return None;
            }
        }
        let picture = video_rect(rc.right, (rc.bottom - BAR_H).max(0), width, height);
        Some((
            width,
            height,
            VideoRect {
                y: picture.y + BAR_H,
                ..picture
            },
        ))
    }

    fn paint(hwnd: HWND) {
        unsafe {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            if hdc.0.is_null() {
                return;
            }
            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);
            let picture = RECT {
                left: 0,
                top: BAR_H,
                right: rc.right,
                bottom: rc.bottom,
            };
            let _ = FillRect(hdc, &picture, HBRUSH(GetStockObject(BLACK_BRUSH).0));
            if let Ok(decoded) = DECODED.lock() {
                if let Some(frame) = decoded.as_ref() {
                    paint_frame(hdc, &rc, frame);
                    paint_cursor(hdc, &rc, frame.width, frame.height);
                    let _ = EndPaint(hwnd, &ps);
                    return;
                }
            }
            let font = GetStockObject(DEFAULT_GUI_FONT);
            let _ = SelectObject(hdc, font);
            let _ = SetBkMode(hdc, windows::Win32::Graphics::Gdi::TRANSPARENT);
            let _ = SetTextColor(hdc, windows::Win32::Foundation::COLORREF(0x00D0D0D0));
            let text = wide_z("等待远端画面…");
            let _ = TextOutW(hdc, 24, BAR_H + 24, &text[..text.len().saturating_sub(1)]);
            let _ = EndPaint(hwnd, &ps);
        }
    }

    /// 把最新一帧等比画进画面区。BITMAPINFO 的负高度表示从上往下存，
    /// 和 NV12 转换出来的 BGRA 行顺序一致。
    fn paint_frame(hdc: HDC, rc: &RECT, frame: &DecodedFrame) {
        if frame.width == 0 || frame.height == 0 || frame.bgra.is_empty() {
            return;
        }
        let area = video_rect(rc.right, (rc.bottom - BAR_H).max(0), frame.width, frame.height);
        if area.w <= 0 || area.h <= 0 {
            return;
        }
        let mut bi = BITMAPINFO::default();
        bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        bi.bmiHeader.biWidth = frame.width as i32;
        bi.bmiHeader.biHeight = -(frame.height as i32);
        bi.bmiHeader.biPlanes = 1;
        bi.bmiHeader.biBitCount = 32;
        bi.bmiHeader.biCompression = BI_RGB.0;
        unsafe {
            let _ = StretchDIBits(
                hdc,
                area.x,
                BAR_H + area.y,
                area.w,
                area.h,
                0,
                0,
                frame.width as i32,
                frame.height as i32,
                Some(frame.bgra.as_ptr() as *const _),
                &bi,
                DIB_RGB_COLORS,
                SRCCOPY,
            );
        }
    }

    /// 远端光标按画面缩放画在热点上。形状没到之前什么都不画。
    fn paint_cursor(hdc: windows::Win32::Graphics::Gdi::HDC, rc: &RECT, remote_w: u32, remote_h: u32) {
        if remote_w == 0 || remote_h == 0 {
            return;
        }
        let Ok(cur) = super::CURSOR.lock() else {
            return;
        };
        if !cur.visible || cur.width == 0 || cur.height == 0 || cur.bgra.is_empty() {
            return;
        }
        let area = video_rect(rc.right, (rc.bottom - BAR_H).max(0), remote_w, remote_h);
        if area.w <= 0 || area.h <= 0 {
            return;
        }
        let sx = area.w as f64 / f64::from(remote_w);
        let sy = area.h as f64 / f64::from(remote_h);
        let left = area.x + (((cur.x - cur.hot_x) as f64) * sx) as i32;
        let top = BAR_H + area.y + (((cur.y - cur.hot_y) as f64) * sy) as i32;
        let w = ((cur.width as f64) * sx).max(1.0) as i32;
        let h = ((cur.height as f64) * sy).max(1.0) as i32;
        let mut bi = BITMAPINFO::default();
        bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        bi.bmiHeader.biWidth = cur.width as i32;
        // 负高度表示从上往下存，和协议里的行顺序一致。
        bi.bmiHeader.biHeight = -(cur.height as i32);
        bi.bmiHeader.biPlanes = 1;
        bi.bmiHeader.biBitCount = 32;
        bi.bmiHeader.biCompression = BI_RGB.0;
        unsafe {
            // SRCCOPY 会忽略 alpha，把透明区域画成黑块。协议里的光标是
            // 普通 BGRA，先预乘 alpha 再用 AlphaBlend 合成。
            let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
            let Ok(bitmap) = CreateDIBSection(Some(hdc), &bi, DIB_RGB_COLORS, &mut bits, None, 0) else {
                return;
            };
            if bits.is_null() {
                let _ = DeleteObject(bitmap.into());
                return;
            }
            let premultiplied = super::premultiply_bgra(&cur.bgra);
            std::ptr::copy_nonoverlapping(
                premultiplied.as_ptr(),
                bits as *mut u8,
                premultiplied.len(),
            );
            let mem = CreateCompatibleDC(Some(hdc));
            if mem.0.is_null() {
                let _ = DeleteObject(bitmap.into());
                return;
            }
            let old = SelectObject(mem, bitmap.into());
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            let _ = AlphaBlend(
                hdc,
                left,
                top,
                w,
                h,
                mem,
                0,
                0,
                cur.width as i32,
                cur.height as i32,
                blend,
            );
            SelectObject(mem, old);
            let _ = DeleteObject(bitmap.into());
            let _ = DeleteDC(mem);
        }
    }

    fn layout(hwnd: HWND) {
        let mut rc = RECT::default();
        unsafe {
            if GetClientRect(hwnd, &mut rc).is_err() {
                return;
            }
        }
        move_to(item(hwnd, ID_QUALITY), 8, 6, 120, 180);
        move_to(item(hwnd, ID_DISPLAY), 136, 6, 180, 180);
        move_to(item(hwnd, ID_HANGUP), (rc.right - 88).max(340), 6, 72, 24);
        move_to(item(hwnd, ID_HINT), 8, BAR_H + 8, (rc.right - 16).max(40), 20);
    }

    fn drain_messages() {
        unsafe {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    fn register_class() {
        if CLASS_READY.swap(true, Ordering::Relaxed) {
            return;
        }
        unsafe {
            let wc = WNDCLASSW {
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(view_proc),
                hbrBackground: HBRUSH(GetStockObject(WHITE_BRUSH).0),
                lpszClassName: w!("JarynRemoteView"),
                hInstance: instance().unwrap_or_default(),
                ..Default::default()
            };
            RegisterClassW(&wc);
        }
    }

    fn child(parent: HWND, class_name: &str, title: &str, extra: u32, id: i32) -> HWND {
        let class = wide_z(class_name);
        let text = wide_z(title);
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                windows::core::PCWSTR(class.as_ptr()),
                windows::core::PCWSTR(text.as_ptr()),
                WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | extra),
                0,
                0,
                10,
                10,
                Some(parent),
                Some(HMENU(id as isize as *mut _)),
                instance(),
                None,
            )
        };
        let Ok(hwnd) = hwnd else {
            return HWND::default();
        };
        unsafe {
            let font = GetStockObject(DEFAULT_GUI_FONT);
            let _ = SendMessageW(hwnd, WM_SETFONT, Some(windows::Win32::Foundation::WPARAM(font.0 as usize)), Some(LPARAM(1)));
        }
        hwnd
    }

    fn item(parent: HWND, id: i32) -> HWND {
        unsafe { GetDlgItem(Some(parent), id).unwrap_or_default() }
    }

    fn move_to(hwnd: HWND, x: i32, y: i32, w: i32, h: i32) {
        if hwnd.0.is_null() {
            return;
        }
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::MoveWindow(hwnd, x, y, w.max(1), h.max(1), true);
        }
    }

    fn live_hwnd() -> Option<HWND> {
        let raw = HWND_RAW.load(Ordering::Relaxed);
        if raw == 0 { None } else { Some(HWND(raw as *mut _)) }
    }

    fn instance() -> Option<windows::Win32::Foundation::HINSTANCE> {
        unsafe { GetModuleHandleW(None).ok().map(Into::into) }
    }

    fn wide_z(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letterbox_keeps_aspect_and_ignores_margins() {
        let rect = video_rect(1000, 500, 1920, 1080);
        assert!(rect.w <= 1000 && rect.h <= 500);
        // 整数缩放最多偏一个像素，不能拉变形。
        let skew = (rect.w as i64 * 1080 - rect.h as i64 * 1920).abs();
        assert!(skew <= 1920, "skew={skew} rect={rect:?}");
        assert!(map_pointer(rect, 0, rect.y + rect.h / 2, 1920, 1080).is_none());
        let mid = map_pointer(rect, rect.x + rect.w / 2, rect.y + rect.h / 2, 1920, 1080).unwrap();
        assert!(mid.0 > 900 && mid.0 < 1020);
        assert!(mid.1 > 500 && mid.1 < 580);
    }

    #[test]
    fn backlog_without_idr_asks_for_keyframe() {
        let _ = with_view(|v| {
            v.frames.clear();
            v.wait_idr = true;
            v.need_keyframe = false;
        });
        present(&[0, 0, 0, 1, 1], false);
        assert!(take_need_keyframe(), "等首帧 IDR 时必须主动要关键帧");

        let _ = with_view(|v| {
            v.frames.clear();
            v.wait_idr = true;
            v.need_keyframe = false;
        });
        present(&[0, 0, 0, 1, 5], true);
        for _ in 0..8 {
            present(&[0, 0, 0, 1, 1], false);
        }
        present(&[9], false);
        assert!(take_need_keyframe());
        present(&[0, 0, 0, 1, 5], true);
        request_open(1280, 720);
        assert!(is_open());
    }

    #[test]
    fn wheel_and_cursor_helpers_match_painting() {
        // Windows 与 NSEvent 的正方向一致；方向取反由节点统一做。
        assert_eq!(windows_wheel_delta_for_node(120), 2);
        assert_eq!(windows_wheel_delta_for_node(-120), -2);
        assert_eq!(windows_wheel_delta_for_node(1), 1);
        assert_eq!(premultiply_bgra(&[100, 50, 25, 128]), vec![50, 25, 13, 128]);
        assert_eq!(premultiply_bgra(&[200, 1, 2, 0]), vec![0, 0, 0, 0]);
    }

    /// 上一段会话排队的输入不能带进下一段：那时按键可能已经在物理上松开，
    /// 补发一次抬起会让远端凭空收到没人按下的松开。
    #[test]
    fn reset_session_drops_queued_input() {
        set_focused(true);
        push_input(ViewInput { kind: KIND_MOVE, a: 10, b: 20 });
        push_input(ViewInput { kind: KIND_KEY, a: 4, b: 0 });
        assert!(!drain_input().is_empty());
        push_input(ViewInput { kind: KIND_MOVE, a: 30, b: 40 });
        reset_session();
        assert!(drain_input().is_empty());
        set_focused(false);
    }

    #[test]
    fn releases_survive_losing_focus() {
        assert!(ViewInput { kind: KIND_BUTTON, a: 0, b: 0 }.is_release());
        assert!(ViewInput { kind: KIND_KEY, a: 4, b: 0 }.is_release());
        assert!(!ViewInput { kind: KIND_BUTTON, a: 0, b: 1 }.is_release());
        assert!(!ViewInput { kind: KIND_MOVE, a: 10, b: 20 }.is_release());
        assert!(!ViewInput { kind: KIND_WHEEL, a: 2, b: 0 }.is_release());
    }
}
