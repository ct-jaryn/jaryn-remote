use jr_proto::PLATFORM_IOS;

use super::{status_word, State};
use crate::ui::{Row, ROW_CAN_GRANT, ROW_CAN_INVITE, ROW_CAN_REVOKE, ROW_HAS_GRANT, ROW_LAN, ROW_ONLINE, ROW_REVOKED};

/// 列表只放真实状态。不能邀请的设备不给按钮，避免先放一个点了就失败的入口。
pub fn rows(st: &State) -> Vec<Row> {
    let mut rows: Vec<Row> = st
        .peers
        .values()
        .filter(|p| p.snap.short_id != st.short_id)
        .map(|p| {
            let snap = &p.snap;
            let can_invite = snap.platform != PLATFORM_IOS && !snap.revoked && snap.online
                && !snap.session_active && snap.blocker == 0;
            let mut flags = 0;
            if snap.online {
                flags |= ROW_ONLINE;
            }
            if snap.revoked {
                flags |= ROW_REVOKED;
            }
            if p.lan {
                flags |= ROW_LAN;
            }
            if can_invite { flags |= ROW_CAN_INVITE; }
            if !snap.revoked { flags |= ROW_CAN_GRANT; }
            if !snap.revoked && !st.private_mode {
                flags |= ROW_CAN_REVOKE;
            }
            let grant_path = st.grants_dir.join(jr_proto::hex_encode(&snap.device_id));
            if grant_path.exists() {
                flags |= ROW_HAS_GRANT;
            }
            Row {
                short_id: snap.short_id.clone(),
                name: snap.name.clone(),
                status: status_word(snap).into(),
                net: if p.lan { "局域网".into() } else { "广域网".into() },
                flags,
            }
        })
        .collect();
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    rows
}

pub fn header(st: &State) -> String {
    let session = if st.session_active { "会话中" } else { "空闲" };
    let server = if st.signal_connected { "已连接" } else { "重连中" };
    format!("{}  {}  {}    服务器 {} {}", st.name, st.short_id, session, st.signal_addr, server)
}
