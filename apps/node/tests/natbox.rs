//! 用户态模拟 NAT。
//!
//! 真实的 NAT 在这台机器上没有，但打洞依赖的那条规则可以单独搬出来：
//! 只有往外发过一颗包，外部地址才能从那条映射上进来。没发过的来源一律丢掉。
//!
//! 这不是网络模拟器，只覆盖打洞要验证的那一条行为。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use jr_node::NodeHandle;

/// 一台机器在 NAT 后面的样子。
struct Side {
    node: NodeHandle,
    /// 对外看到的地址。NAT 自己分配，外部只认这个。
    public: SocketAddr,
    /// 往外发过的目的地址。只有这些地址能把包送回来。
    mapped: Mutex<HashMap<SocketAddr, ()>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Verdict {
    /// 从公网地址进来，而且这台的 NAT 上有对应映射：送到了。
    Delivered,
    /// 从公网地址进来，但这台还没往外发过：丢了。这就是打洞要解决的问题。
    NoMapping,
    /// 目标不是这台机器的公网地址（比如局域网地址、中继地址），不在这个模拟里。
    Elsewhere,
}

/// 两台机器各自在一台 NAT 后面。
pub struct Pair {
    a: Side,
    b: Side,
    /// 每次发往公网地址的结果，用来在失败时看清楚是哪一步没接上。
    pub log: Mutex<Vec<(bool, Verdict, SocketAddr)>>,
}

impl Pair {
    pub fn new(a: NodeHandle, a_public: SocketAddr, b: NodeHandle, b_public: SocketAddr) -> Arc<Self> {
        let pair = Arc::new(Self {
            a: Side { node: a, public: a_public, mapped: Mutex::new(HashMap::new()) },
            b: Side { node: b, public: b_public, mapped: Mutex::new(HashMap::new()) },
            log: Mutex::new(Vec::new()),
        });
        let left = pair.clone();
        pair.a.node.set_media_hook(Some(Arc::new(move |pkt, dst| left.forward(true, pkt, dst))));
        let right = pair.clone();
        pair.b.node.set_media_hook(Some(Arc::new(move |pkt, dst| right.forward(false, pkt, dst))));
        pair
    }

    /// 从一侧往外发。先记下这条映射，再看对端的 NAT 放不放进来。
    fn forward(&self, from_a: bool, pkt: &[u8], dst: SocketAddr) {
        let (src, dst_side) = if from_a { (&self.a, &self.b) } else { (&self.b, &self.a) };
        src.mapped.lock().unwrap().insert(dst, ());
        let verdict = if dst != dst_side.public {
            Verdict::Elsewhere
        } else if !dst_side.mapped.lock().unwrap().contains_key(&src.public) {
            // 对端只放行它自己往外发过的来源。这就是打洞要同时发包的原因：
            // 单边发，另一边的 NAT 还没有映射，包会被丢掉。
            Verdict::NoMapping
        } else {
            Verdict::Delivered
        };
        self.log.lock().unwrap().push((from_a, verdict, dst));
        if verdict != Verdict::Delivered {
            return;
        }
        let node = dst_side.node.clone();
        let from = src.public;
        let owned = pkt.to_vec();
        // 不能在发送钩子里同步注入：注入会立刻产生回复，回复又进这个钩子。
        // 丢到线程里，让它从外面进来。
        std::thread::spawn(move || node.inject_datagram(from, &owned));
    }
}
