use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;


#[derive(Clone)]
pub struct NodeConfig {
    pub signal: SocketAddr,
    pub pin: [u8; 32],
    pub data_dir: PathBuf,
    pub name: String,
    pub platform: u8,
    pub fleet_token: [u8; 32],
    pub auto_accept: bool,
    pub heartbeat: Duration,
    pub beacon_port: u16,
    pub beacon_every: Duration,
    pub force_relay: bool,
    /// 信令上的反射端口。填 0 就不做反射发现。
    pub reflex_port: u16,
    /// 测试和网络模拟用：把这台机器对外报的直连地址换掉。
    pub advertise_override: Option<Vec<[u8; 4]>>,
    /// 测试用：额外报一个候选，端口可以和本机媒体端口不一样。
    /// 用来证明打洞是同时往每个候选都发，而不是只试第一个。
    pub extra_candidate: Option<SocketAddr>,
    pub os_inject: bool,
    pub os_capture: bool,
    /// 收到对面的剪贴板时要不要写进本机剪贴板。测试里关掉，不然会动到跑测试的人的剪贴板。
    pub os_clipboard: bool,
}

#[derive(Clone, Debug)]
pub struct DeviceView {
    pub name: String,
    pub short_id: String,
    pub device_id: [u8; 32],
    pub online: bool,
    pub revoked: bool,
    pub lan: bool,
    pub status: String,
}
