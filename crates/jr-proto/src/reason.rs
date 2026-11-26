#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum Reason {
    Ok = 0,
    UserHangup = 1,
    Rejected = 2,
    Busy = 3,
    Unauthorized = 4,
    HandshakeTimeout = 5,
    NoPath = 6,
    VersionMismatch = 7,
    PlatformNotHostable = 8,
    PermissionMissing = 9,
    SessionDead = 10,
    Revoked = 11,
    PrivacyUnsupported = 12,
}

impl Reason {
    pub fn from_u16(v: u16) -> Option<Self> {
        Some(match v {
            0 => Self::Ok,
            1 => Self::UserHangup,
            2 => Self::Rejected,
            3 => Self::Busy,
            4 => Self::Unauthorized,
            5 => Self::HandshakeTimeout,
            6 => Self::NoPath,
            7 => Self::VersionMismatch,
            8 => Self::PlatformNotHostable,
            9 => Self::PermissionMissing,
            10 => Self::SessionDead,
            11 => Self::Revoked,
            12 => Self::PrivacyUnsupported,
            _ => return None,
        })
    }

    pub fn user_text(self) -> &'static str {
        match self {
            Self::Ok | Self::UserHangup => "已断开",
            Self::Rejected => "对方拒绝了",
            Self::Busy => "对方正在被另一台设备控制",
            Self::Unauthorized => "没有无人值守授权",
            Self::HandshakeTimeout => "连不上",
            Self::NoPath => "直连和中继都失败",
            Self::VersionMismatch => "需要升级",
            Self::PlatformNotHostable => "这台设备不能被控",
            Self::PermissionMissing => "对方没有授予系统权限",
            Self::SessionDead => "连接中断",
            Self::Revoked => "设备已从舰队移除",
            Self::PrivacyUnsupported => "不能隐藏本机画面",
        }
    }
}
