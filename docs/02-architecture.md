# 02 系统架构

## 1. 设计决定

这些决定后面的文档都遵守。改其中一条，要同时改协议和验收。

| 编号 | 决定 | 原因 |
|---|---|---|
| D1 | 完全自研会话和传输，不链 WebRTC、不嵌现成远控 | 产品要求独立开发。WebRTC 的抖动缓冲和拥塞是为通话调的，改不动到目标延迟 |
| D2 | 只调用操作系统采集、硬编、注入和系统 AAC | 重写显卡编码器不属于远控产品，而且会更慢 |
| D3 | 密码学用库，协议自己定 | 自造加密算法不能做。库停在算法层，密钥交换流程和包格式是 JRP |
| D4 | 一个节点两种角色 | 设备舰队不是“主机版 / 客户端版”两个产品 |
| D5 | 信令可信作介绍和在线，不可信作画面保密 | 中继和网络路径都只看见密文。授权票据由被控端签名，信令不能伪造无人值守 |
| D6 | 视频不可靠，控制可靠，输入不可靠但最新优先 | 丢一帧画面可以，丢文件块不行，过期的鼠标移动不补 |
| D7 | 光标独立通道 | 跟手的主要来源。编进画面至少慢一帧 |
| D8 | 能力协商，而不是按平台写死 | iOS、无桌面 Linux、权限被收回，都是正常状态 |
| D9 | 第一版单会话独占 | 输入冲突比“多人一起看”更先要避免 |
| D10 | 隐私屏失败必须可恢复 | 黑屏卡住比看得到屏幕更严重 |

明确不采用的实现路径：把 RustDesk、Sunshine、Moonlight、scrcpy、Guacamole 打进安装包；调用 UU 的 URL Scheme；用浏览器 `<video>` 或 WebView 播画面；用 TCP 传视频；用定时截图冒充远控。

可以阅读那些项目的公开架构来对照延迟来源。代码、包格式、握手和服务器接口不复制。

## 2. 系统上下文

```text
                    +------------------+
                    | 信令 signal      |
                    | 在线、邀请、票据 |
                    +--------+---------+
                             | TLS 长连接
         +-------------------+-------------------+
         |                   |                   |
   +-----v-----+       +-----v-----+       +-----v-----+
   | Mac 节点  |       | Win 节点  |       | 手机节点  |
   | 主控/被控 |       | 主控/被控 |       | 主控或被控|
   +-----+-----+       +-----+-----+       +-----+-----+
         |                   |                   |
         |   UDP 直连 / 打洞  |                   |
         +-------------------+                   |
                             |                   |
                      +------v------+            |
                      | 中继 relay  |<-----------+
                      | 只转发密文  |
                      +-------------+

   局域网内另有 UDP 信标，用来发现“此刻在同一个网上”，不替代信令。
```

所有者的服务器上跑 `signal` 和 `relay` 两个进程。它们可以在同一台机器，也可以分开。节点配置服务器地址。没有服务器时，局域网信标仍可发现设备并发起仅局域网会话。这是降级，不是另一种产品。

## 3. 进程

桌面节点是一个进程。设备列表、邀请弹窗、观看窗口和软件遮挡都由进程内的原生窗口创建：macOS 用 Cocoa / ObjC，Windows 用 Win32。不是网页壳，也没有本机 IPC。

```text
+---------------- 节点进程 ----------------+
|  原生窗口（macOS Cocoa / Windows Win32） |
|  设备列表、授权弹窗、观看窗口、遮挡层    |
|  与信令、会话、采集共享进程              |
|  信令长连接、采集、编码、UDP、注入、文件 |
+------------------------------------------+
```

窗口只负责界面。信令、会话、采集、编码和注入在同一进程里，由固定线程承载。关掉或隐藏设备窗口不结束进程，无人值守会话继续；只有明确的退出命令才结束节点。

手机还没有实现。目标上的拆分是 Android 用一个前台服务承载被控逻辑，界面绑定到服务；iOS 只有主控。

没有图形会话的 Linux 只跑终端命令，不创建窗口。

## 4. 模块

依赖只能从上往下。平台适配器不反向调用界面。

```text
apps/node                节点入口与组装（node/、signal/、live/、config.rs、state.rs、native/macos）
apps/signal              TLS 信令、在线状态、封闭配对与审计
apps/relay               无状态 UDP 密文中继

crates/jr-proto          编解码，无 IO，无系统调用
crates/jr-crypto         密钥协商和 AEAD 包装，内部用加密库
crates/jr-transport      UDP、打洞、中继客户端、可靠 ARQ、FEC、拥塞
crates/jr-session        状态机，把协议和传输接起来
crates/jr-input          跨平台键码与坐标变换
crates/jr-host-win       Windows 采集、零拷贝编码、音频、注入、剪贴板、ConPTY、原生窗口与托盘
```

macOS 的采集、编码和窗口仍在 `apps/node/native/macos` 的 ObjC 桥里，并直接从节点链接。文档早先设想的 `jr-media`、`jr-av`、`platform-*` 还没有拆出来；在真正拆之前，不要把目录图当成已存在。

### 4.1 trait 合同

采集端每次交出的是一帧引用，不是内存拷贝。

```text
trait FrameSource {
  fn start(display: DisplayId) -> Result<()>
  fn stop()
  fn poll(timeout_ms: u32) -> Poll<Frame>
  fn displays() -> Vec<DisplayInfo>
}

struct Frame {
  texture: GpuTextureHandle,  // 平台私有，编解码器认识
  frame_id: u32,           // 与 JRP 内层头一致，本地计数溢出就回绕
  captured_at_us: u64,        // 本机单调钟
  dirty: DirtyHint,           // 全帧或未知。第一版允许永远是全帧
}

trait Encoder {
  fn negotiate(prefs: Profile) -> EncoderCaps
  fn encode(frame: Frame, force_idr: bool) -> Result<Bitstream>
  fn set_bitrate(bps: u32)
  fn request_idr()
}

trait Decoder {
  fn decode(bitstream: Bitstream) -> Result<DecodedFrame>
}

trait Presenter {
  fn present(frame: DecodedFrame, cursor: Option<CursorOverlay>)
}

trait InputInjector {
  fn inject(event: InputEvent) -> Result<()>
  fn set_mode(mode: PointerMode)
}

trait CursorCapture {
  fn poll() -> Option<CursorUpdate>  // 形状变化或位置变化
}
```

`GpuTextureHandle` 在 Windows 上是 DXGI 纹理指针，在 macOS 上是 IOSurface，在 Android 上是 MediaCodec 的输入 surface 索引。它不出平台适配层。目前 macOS 的原生桥和 Windows 的 `jr-host-win` 各自处理，尚未抽出统一媒体 crate。

这是有意的。跨平台零拷贝抽象做不好就会在中间拷一次 CPU，延迟预算直接没了。

## 5. 被控端线程

节点进程里远控会话使用固定线程，不用任务调度器承载采集循环。

| 线程 | 优先级 | 职责 | 队列 |
|---|---|---|---|
| capture | 时间关键 | 等下一帧，只保留最新 | 到 encode，容量 1，后来的覆盖未编码帧 |
| encode | 时间关键 | 硬编，产出码流 | 到 send，最多 3 帧；满了丢非参考帧 |
| send | 高 | 分包、FEC、按发送时钟发出 | 套接字 |
| recv | 高 | 收输入、反馈、控制 | 输入队列 64；移动事件合并成最新 |
| inject | 高 | 立刻注入，不等待上屏 | 从 recv 来 |
| control | 普通 | 文件、剪贴板、终端 | 可靠通道 |
| signal | 普通 | 心跳、邀请 | TCP/TLS |

采集线程禁止做编码、磁盘和锁外的分配。编码线程禁止等网络。发送线程禁止等编码器。

主控端对称：recv 重组后立刻送解码；解码完成立刻 present；输入采集在界面线程或独立钩子里，直接进 send，不进视频时钟。

## 6. 一次会话的路径

```text
主控 UI                主控 host              信令                 被控 host
  |  请求连接            |                      |                      |
  |--------------------->| Invite               |                      |
  |                      |--------------------->| Invite               |
  |                      |                      |--------------------->|
  |                      |                      |     本机允许或验票    |
  |                      |                      |<---------------------|
  |                      | 双方临时公钥+签名    |                      |
  |                      |<-------------------->|                      |
  |                      | 派生会话密钥         |                      |
  |                      | UDP 探测 LAN/打洞/中继                        |
  |                      |<============================================>|
  |                      | 选路，发 Capability 和第一帧                  |
  |<---------------------| 画面窗口                                       |
```

信令不转发视频。它只转发邀请、签名过的临时公钥、候选地址和会话路由令牌。路由令牌是随机 128 位，不是设备公钥。中继用令牌认会话，不知道这是谁。

## 7. 部署

第一版就是两个静态二进制加各端 App。

```text
所有者的常开机器
  signal  监听 TCP 47210（TLS）与 UDP 47213（公网地址反射）
  relay   监听 UDP 47211
  数据    SQLite 一个文件

每个节点
  配置 signal 地址和证书指纹（共享令牌模式另带舰队注册令牌；--private 封闭配对模式走挑战签名审批）
  可选配置 relay 地址；不配则由 signal 下发 RelayTicket
```

防火墙需要放行 `47210/TCP`、`47211/UDP` 和反射端口 `47213/UDP`（若通过 `--reflex-port` 自定义反射端口，节点启动时需传入相同的 `--reflex-port`）。局域网信标使用本地 `47212/UDP`，公网防火墙无需放行。节点侧不要求入站端口；被控端主动连信令，所以家里没有公网 IP 也能在线。

局域网直连时，两端都在 NAT 后面仍可能打洞失败。失败是正常分支，不是错误日志里的异常。

## 8. 故障域

| 故障 | 行为 |
|---|---|
| 信令进程重启 | 节点 1 秒内重连，在线列表短暂空白。已建立的 UDP 会话不经过信令，继续 |
| 中继重启 | 走中继的会话进入重连，5 秒内换路或失败。局域网会话不受影响 |
| 节点进程崩溃 | 无人值守会话中断。启动时先清理隐私遮挡和残留输入，不能假设崩溃前的 finally 会执行 |
| 设备窗口关闭 | 只是隐藏窗口，节点和无人值守会话继续 |
| 主控消失 | 被控 5 秒收不到心跳就结束会话并恢复屏幕 |
| 编码器重置 | 发关键帧，主控重建解码器。不重做密钥 |
| 显示器拔插 | 能力更新，当前显示消失就切到剩下的第一块，并通知主控 |

隐私屏的恢复状态写在本机一个小文件里：进入隐私模式前的显示拓扑。host 每次启动先读这个文件，发现未清理就恢复，再删文件。只靠内存标记不够。

## 9. 观测

第一版不做远程遥测上报。日志在本机。

会话统计只留这些，给验收和以后的画质自适应用：捕获耗时、编码耗时、排队帧数、发送码率、丢包、RTT、抖动、解码耗时、present 间隔、端到端估计。统计走反馈包，不另开产品。

审计不记录这些统计里的画面内容，也不记录键鼠。审计字段见服务器文档。

## 10. 仓库映射

当前实际目录：

```text
apps/node          节点入口与 macOS 原生桥（native/macos）
apps/signal
apps/relay
crates/jr-proto
crates/jr-crypto
crates/jr-transport
crates/jr-session
crates/jr-input
crates/jr-host-win
docs/
```

Android / iOS / Linux 桌面适配层还没建立。新增平台时再按第 4 节的边界落目录，不先建空壳。
