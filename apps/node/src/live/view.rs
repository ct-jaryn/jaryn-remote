use crate::live::now_us;
use crate::State;

pub(super) fn send_feedback_due(st: &mut State) {
    if !st.view_open {
        return;
    }
    let Some(engine) = st.engine.as_mut() else {
        return;
    };
    st.rx_stats = engine.rx_stats();
    if let Some(fb) = engine.feedback_due(now_us()) {
        engine.send_feedback(&fb);
        st.feedback_count = st.feedback_count.saturating_add(1);
    }
}
pub(super) fn sync_view(st: &mut State) {
    let menu = crate::av::drain_menu();
    if !menu.is_empty() {
        let Some(engine) = st.engine.as_mut() else {
            return;
        };
        let mut applied = None;
        for (kind, value) in menu {
            let result = match kind {
                1 if value >= 1 && value <= 4 => engine.set_profile(value as u8),
                2 => engine.switch_display(value.max(0) as u32),
                _ => Ok(()),
            };
            match result {
                Ok(()) => applied = Some((kind, value)),
                Err(e) => println!("node: {e}"),
            }
        }
        // 下拉框本身也要跟着会话状态走，不能被下一次刷新改回旧值。
        if let Some((kind, value)) = applied {
            if kind == 1 {
                st.quality = value as u8;
                crate::av::set_view_quality(value as u8);
            } else if kind == 2 {
                st.want_display = value.max(0) as usize;
                crate::av::set_view_display(st.want_display);
            }
        }
    }
    if !st.view_open {
        return;
    }
    let focused = crate::av::view_focused();
    let inputs = crate::av::drain_input();
    let Some(engine) = st.engine.as_mut() else {
        return;
    };
    for input in inputs {
        // 失焦后不再发移动和按下，但用户可能刚在画面里按下、切走后才松开。
        // 抬起必须放行，否则被控端会一直等超时兜底。
        if !focused && !input.is_release() {
            continue;
        }
        if let Some(event) = crate::av::view_event(input) {
            let _ = engine.send_input(event);
        }
    }
}
