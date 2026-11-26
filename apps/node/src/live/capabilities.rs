use jr_proto::{Capability, DisplayInfo, AUDIO_CODEC_AAC_LC, FEAT_AUDIO_OUT, FEAT_CLIPBOARD, FEAT_INPUT, FEAT_PRIVACY, FEAT_VIDEO, PLATFORM_ANDROID, PLATFORM_IOS, PLATFORM_LINUX, PLATFORM_MACOS, PLATFORM_WINDOWS};
use jr_session::{host_profile, Engine};

use crate::State;

/// 本机真正能提供的远程能力。授权票据、注册声明和能力协商都用这一份，
/// 不要再在别处手写 0 或者照抄平台模板。
pub fn local_features_for(platform: u8, os_inject: bool, os_capture: bool) -> u32 {
    let capture_ready = if can_capture(platform) {
        match platform {
            PLATFORM_MACOS => crate::av::screen_granted(),
            _ => jr_host_win::ready(),
        }
    } else {
        false
    };
    compose_features(
        platform,
        os_inject,
        os_capture,
        crate::av::clipboard_ready(),
        crate::av::audio_playback_ready(),
        capture_ready,
        crate::inject::trusted(),
    )
}

/// 有没有桌面采集适配器。只有 macOS 和 Windows 接了原生采集。
pub fn can_capture(platform: u8) -> bool {
    matches!(platform, PLATFORM_MACOS | PLATFORM_WINDOWS)
}

/// 能力矩阵本身。输入全是调用方探测好的结果，不读全局、不碰系统。
/// 抽出来是为了让「某个平台缺某项适配器时必须把对应位清掉」这条能被测试
/// 钉住：这里少清一位，授权票据就会把做不了的能力写给对方。
pub fn compose_features(
    platform: u8,
    os_inject: bool,
    os_capture: bool,
    clipboard_ready: bool,
    audio_playback_ready: bool,
    screen_ready: bool,
    inject_trusted: bool,
) -> u32 {
    let mut features = host_profile(platform).features;
    if !clipboard_ready {
        features &= !FEAT_CLIPBOARD;
    }
    let desktop = platform != PLATFORM_LINUX && platform != PLATFORM_ANDROID && platform != PLATFORM_IOS;
    // 主控只要有播放器就能收声音；被控端能不能采由 open_engine 再按权限收紧。
    if desktop && audio_playback_ready {
        features |= FEAT_AUDIO_OUT;
    }
    // 关掉系统注入时照样报位（对方仍可只收画面）；开着注入就必须真拿到授权。
    if desktop && (!os_inject || inject_trusted) {
        features |= FEAT_INPUT;
    }
    // 只有 macOS / Windows 有采集适配器。其他平台即使调用方传了
    // `screen_ready = true` 也不能报画面：那些平台的采集还没实现。
    if os_capture && screen_ready && can_capture(platform) {
        features |= FEAT_VIDEO;
    }
    if desktop {
        features |= FEAT_PRIVACY;
    }
    features
}

pub fn local_features(st: &State) -> u32 {
    local_features_for(st.platform, st.os_inject, st.os_capture)
}

/// 被控端真正能提供的音频能力。主控能播放不等于被控端能采集，
/// 授权票据和接受邀请都按这一份算，免得把 audio 位写给采不了声音的机器。
pub fn local_host_features(st: &State) -> u32 {
    let mut features = local_features(st);
    if !crate::av::audio_capture_ready() {
        features &= !FEAT_AUDIO_OUT;
    }
    features
}

pub fn open_engine(st: &mut State, controller: bool) {
    if st.engine.is_some() {
        return;
    }
    let inbox = st.data_dir.join("inbox");
    let profile = host_profile(st.platform);
    // 只报本机真的有适配器的能力；再和这次邀请申请的能力取交集。
    let mut features = local_features(st);
    if st.session_features != 0 {
        features &= st.session_features;
    }
    // 被控端只有真的能起系统回环时才报声音；否则能力位必须诚实为 0。
    // 不要在这里把 audio 位强行加回来，否则会绕过上面和邀请申请位的交集。
    let audio_ready = !controller && crate::av::audio_capture_ready();
    if !controller && !audio_ready {
        features &= !FEAT_AUDIO_OUT;
    }
    let mut encoder_caps = 0u32;
    st.video_ready = false;
    let capture_ready = match st.platform {
        PLATFORM_MACOS => crate::av::screen_granted(),
        PLATFORM_WINDOWS => jr_host_win::ready(),
        _ => false,
    };
    if st.os_capture && capture_ready {
        encoder_caps = 1;
        st.video_ready = features & FEAT_VIDEO != 0;
        st.blocker = if st.os_inject && !crate::inject::trusted() { 2 } else { 0 };
    } else {
        st.blocker = profile.blocker;
    }
    // macOS 没有公开的虚拟显示器接口，隐私屏只能做软件遮挡。能力位照样报，
    // 但引擎要记住这是降级，开的时候会同步告诉主控。
    let privacy_supported = st.platform != PLATFORM_LINUX && st.platform != PLATFORM_ANDROID && st.platform != PLATFORM_IOS;
    let privacy_degraded = st.platform == PLATFORM_MACOS || st.platform == PLATFORM_WINDOWS;
    if privacy_supported {
        features |= FEAT_PRIVACY;
    }
    let displays = displays_of(st);
    if let Some(first) = displays.first() {
        if !st.display_override {
            st.display = jr_input::Display {
                origin_x: first.origin_x,
                origin_y: first.origin_y,
                width: first.width,
                height: first.height,
            };
            st.scale_milli = first.scale_milli.max(1);
        }
    }
    let mut engine = Engine::new(controller, inbox, features, privacy_supported);
    if privacy_degraded {
        engine.mark_privacy_degraded();
    }
    st.engine = Some(engine);
    if controller {
        // 开会话前选的档只写进拥塞控制。发给被控会在握手刚完成时占一个可靠帧，
        // 剪贴板会排在它后面。真正开流时 START 会带上这一档。
        if let Some(engine) = st.engine.as_mut() {
            engine.remember_profile(st.quality);
        }
    }
    if !controller {
        crate::send_state_diagnostic(st, &st.diag_tx, 10, u16::from(st.video_ready), None);
        let cap = Capability {
            protocol_version: jr_proto::JRP_VERSION,
            platform: st.platform,
            feature_bits: features,
            displays,
            encoder_caps,
            audio_codec: if audio_ready { AUDIO_CODEC_AAC_LC } else { 0 },
            max_touch_points: touch_points(),
        };
        if let Some(engine) = st.engine.as_mut() {
            let _ = engine.host_capability(cap);
        }
        if st.video_ready {
            println!("node: 已有录屏权限。主控开始后才采集，空闲不采集");
        } else {
            println!("node: {}。当前不能出画面", profile.label);
        }
    }
}

pub(super) fn displays_of(st: &State) -> Vec<DisplayInfo> {
    if st.display_override || (st.platform != PLATFORM_MACOS && st.platform != PLATFORM_WINDOWS) {
        return vec![DisplayInfo {
            display_id: 1,
            origin_x: st.display.origin_x,
            origin_y: st.display.origin_y,
            width: st.display.width,
            height: st.display.height,
            scale_milli: st.scale_milli.max(1),
            refresh_hz: 60,
            is_virtual: false,
        }];
    }
    let real = crate::av::local_displays();
    if real.is_empty() {
        vec![DisplayInfo {
            display_id: 1,
            origin_x: 0,
            origin_y: 0,
            width: st.display.width,
            height: st.display.height,
            scale_milli: 1000,
            refresh_hz: 60,
            is_virtual: false,
        }]
    } else {
        real
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 桌面平台缺哪个适配器就必须把哪个能力位清掉。写错一位，授权票据
    /// 就会把做不了的能力写给对方，主控点下去才发现没反应。
    #[test]
    fn missing_adapters_clear_their_feature_bits() {
        let full = compose_features(PLATFORM_WINDOWS, true, true, true, true, true, true);
        assert_ne!(full & FEAT_CLIPBOARD, 0);
        assert_ne!(full & FEAT_AUDIO_OUT, 0);
        assert_ne!(full & FEAT_INPUT, 0);
        assert_ne!(full & FEAT_VIDEO, 0);
        assert_ne!(full & FEAT_PRIVACY, 0);

        let no_clip = compose_features(PLATFORM_WINDOWS, true, true, false, true, true, true);
        assert_eq!(no_clip & FEAT_CLIPBOARD, 0);
        assert_ne!(no_clip & FEAT_VIDEO, 0, "缺剪贴板不影响画面");

        let no_audio = compose_features(PLATFORM_WINDOWS, true, true, true, false, true, true);
        assert_eq!(no_audio & FEAT_AUDIO_OUT, 0);

        let no_screen = compose_features(PLATFORM_WINDOWS, true, true, true, true, false, true);
        assert_eq!(no_screen & FEAT_VIDEO, 0);
        // 输入和画面是两位：没有画面时输入位仍按注入授权如实上报，
        // 主控会自己按视频能力决定能不能操作。
        assert_ne!(no_screen & FEAT_INPUT, 0);

        // 注入开着但没拿到授权：不能报输入。
        let no_trust = compose_features(PLATFORM_MACOS, true, true, true, true, true, false);
        assert_eq!(no_trust & FEAT_INPUT, 0);
        // 注入关掉时照样报位，对方还能只收画面。
        let inject_off = compose_features(PLATFORM_MACOS, false, true, true, true, true, false);
        assert_ne!(inject_off & FEAT_INPUT, 0);
    }

    /// 手机和 Linux 桌面不做采集：不能因为传了 `os_capture=true` 就报出
    /// 画面/输入/隐私这些在那些平台上根本不存在的位。
    #[test]
    fn non_desktop_platforms_never_claim_desktop_features() {
        for platform in [PLATFORM_LINUX, PLATFORM_ANDROID, PLATFORM_IOS] {
            let f = compose_features(platform, true, true, true, true, true, true);
            assert_eq!(f & FEAT_VIDEO, 0, "platform={platform}");
            assert_eq!(f & FEAT_INPUT, 0, "platform={platform}");
            assert_eq!(f & FEAT_PRIVACY, 0, "platform={platform}");
            assert_eq!(f & FEAT_AUDIO_OUT, 0, "platform={platform}");
        }
    }

    /// 关了采集就不能报画面能力，哪怕系统权限都拿到了。
    #[test]
    fn capture_off_drops_video() {
        let f = compose_features(PLATFORM_MACOS, true, false, true, true, true, true);
        assert_eq!(f & FEAT_VIDEO, 0);
    }
}

/// 被控端能同时接几根手指。macOS 没有触摸屏，老实报 0。
fn touch_points() -> u8 {
    #[cfg(windows)]
    {
        jr_host_win::inject::touch_points()
    }
    #[cfg(not(windows))]
    {
        0
    }
}
