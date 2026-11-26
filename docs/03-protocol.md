# 03 JRP/1 协议

协议名 JRP，主版本 1。实现以本文常量为主。其他文档如果和这里冲突，以这里为准。

字节序全部小端。未知主版本直接拒绝。未知消息类型跳过该条，不断会话，以便次版本加字段。次版本只允许在已有消息尾部加可选字段，或增加新的消息类型。不允许改变已有字段的含义。

## 1. 常量

```text
JRP_VERSION              = 1
JRP_MAGIC                = 0x4A52
SIGNAL_PORT              = 47210          # TCP + TLS
RELAY_PORT               = 47211          # UDP 密文中继
BEACON_PORT              = 47212          # UDP 局域网广播 / 组播
REFLEX_PORT              = 47213          # UDP 公网源地址反射探测
OUTER_HEADER_LEN         = 28
AEAD_KEY_LEN             = 32
AEAD_TAG_LEN             = 16
AEAD_NONCE_LEN           = 12
MAX_DATAGRAM             = 1280
MAX_CIPHERTEXT           = 1200           # 含 16 字节 tag。28 字节外层头之后，整个 UDP 包仍 <= 1280
MAX_INNER_PLAIN          = 1184           # MAX_CIPHERTEXT - tag
INNER_HEADER_LEN         = 22
MAX_BODY                 = 1162           # 一条消息必须放进一个数据报
PROBE_MS                 = 300
PROBE_PACKETS            = 20
SIGNAL_HEARTBEAT_SEC     = 5
SESSION_SETUP_TIMEOUT_S  = 8
MEDIA_HEARTBEAT_MS       = 1000
SESSION_DEAD_MS          = 5000
RECONNECT_MS             = 5000
PRIVACY_RELEASE_GRACE_MS = 3000
INPUT_MAX_AGE_MS         = 200
FEEDBACK_MS              = 50
IDR_MAX_INTERVAL_MS      = 2000
CONTROL_RTO_MIN_MS       = 20
CONTROL_RTO_MAX_MS       = 500
CONTROL_WINDOW           = 64
FILE_CHUNK               = 1024           # 加消息头后仍必须小于 MAX_BODY
CLIPBOARD_MAX            = 1048576
GRANT_MAX_AGE_SEC        = 31536000       # 1 年，被控端仍可更短
AUDIO_HEADER_LEN         = 9              # codec_u8 + sample_rate_u32 + frame_samples_u32
AUDIO_CODEC_AAC_LC       = 1
AUDIO_SAMPLE_RATE        = 48000
AUDIO_FRAME_SAMPLES      = 1024           # 每帧固定 1024 个采样
KEY_HOLD_TIMEOUT_US      = 500000         # 键盘按下 500ms 未续期自动抬起
CURSOR_SHAPE_HEADER      = 16             # hotspot_x/y + width/height + hash_u64
CURSOR_SHAPE_MAX         = 262144         # 光标 BGRA 像素重组上限
```

加密算法固定为 AES-256-GCM。签名固定为 Ed25519。密钥交换固定为 X25519。派生用 HKDF-SHA256。这些名字指算法，不指某个远控产品。

## 2. 身份和标识

设备身份是 32 字节 Ed25519 公钥，叫 `device_id`。认证永远用完整公钥。

界面短号是 `SHA-256(device_id)` 的前 5 字节，Crockford Base32，8 个字符。短号可以碰撞，只用于展示。信令如果发现同一账户短号碰撞，后注册的设备短号加 1 字节盐重算，盐存在设备记录里。

会话 `session_id` 是 16 字节随机数，由主控生成。

中继 `routing_token` 是 16 字节随机数，由信令生成。它不是 session_id，避免中继日志和会话日志被外人直接对上。信令自己的数据库可以把两者关联，这是所有者服务器上的元数据，接受。

## 3. 传输分面

| 面 | 承载 | 可靠性 | 内容 |
|---|---|---|---|
| 信令 | TCP + TLS | TCP | 注册、心跳、邀请、候选地址、票据转发 |
| 媒体 | UDP 直连或中继 | 不可靠 | 视频、音频、输入、光标位置、反馈、探测 |
| 控制 | 同一条 UDP，ARQ | 可靠有序 | 能力、剪贴板、文件控制、终端、关键帧请求、光标形状 |
| 信标 | UDP 47212 | 不可靠 | 局域网发现 |

不存在第二条“视频 TCP”。文件块也走控制 ARQ，不走视频流。文件慢不能堵住鼠标，所以 ARQ 按流分开，见第 7 节。

## 4. 外层 UDP 头

中继和路径探测都只认外层。外层明文。

```text
偏移  长度  字段
0     2     magic = 0x4A52
2     1     version = 1
3     1     flags
4     16    routing_token
20    4     packet_seq
24    2     ciphertext_len     # 含 tag，不含本头
26    2     reserved = 0
28    N     ciphertext || tag
```

`flags`：

| 位 | 含义 |
|---|---|
| 0 | 1 = 主控发出，0 = 被控发出 |
| 1 | 1 = 只允许中继转发，探测包用于测中继质量 |
| 2 | 1 = 握手包，中继同样转发，端点用来完成密钥确认前的交换 |
| 3–7 | 必须为 0，接收方忽略 |

`packet_seq` 每个方向独立递增，从 1 开始。允许跳号，不允许复用。接收方维护 64 包的重放窗口。窗口内重复包丢弃。比窗口下界还旧的包丢弃。

`ciphertext_len` 为 0 的包只用于路径探测，没有内层。探测包仍占一个 seq。

整个数据报超过 `MAX_DATAGRAM` 直接丢弃。

中继行为：按 `routing_token` 找到另一端地址并原样转发。不解析 28 字节之后的内容。令牌不存在、会话超过 30 秒没有包、或单会话超过所有者配置的带宽上限，丢弃。

## 5.  nonce 和密钥

同一会话两个方向使用不同的密钥，避免 seq 空间撞车。

```text
nonce = packet_seq_le_u32 || direction_u8 || 0x00 * 7
direction = 1 主控 -> 被控，0 被控 -> 主控
```

nonce 不写入数据包。双方都能从外层头算出来。

密钥派生见安全文档。在 `keys_ready` 之前，只有握手内层使用临时的握手密钥。握手密钥只保护握手确认，不保护媒体。媒体包在 `keys_ready` 之前收到必须丢弃。

握手内层的密钥：

```text
hk_c2h, hk_h2c = HKDF-SHA256(
  salt = session_id,
  ikm  = X25519(local_eph, peer_eph),
  info = "jrp-v1-handshake",
  len  = 64
)
```

确认双方签名都验证通过后，再派生媒体密钥，握手密钥丢弃：

```text
salt = SHA-256(transcript)
sk_c2h, sk_h2c = HKDF-SHA256(
  salt = salt,
  ikm  = 同一个 X25519 共享秘密,
  info = "jrp-v1-media",
  len  = 64
)
transcript =
  session_id ||
  controller_device_id || host_device_id ||
  controller_eph_pub || host_eph_pub ||
  controller_sig || host_sig
```

签名覆盖的内容：

```text
sig_input = ASCII "JRP1" || session_id || role_u8 || eph_pub || peer_device_id
role = 1 主控，2 被控
```

缺签名、验签失败、device_id 不在舰队里、或被控端没有对应授权却声称无人值守，握手失败，不派生媒体密钥。

## 6. 内层明文

解密后：

```text
偏移  长度  字段
0     1     msg_type
1     1     stream_id
2     2     body_flags
4     4     frame_id          # 不使用的消息填 0
8     2     slice_index
10    2     slice_count       # 不分片时为 1
12    8     timestamp_us      # 发送方会话时钟，握手完成时为 0
20    2     body_len
22    N     body
```

`msg_type`：

| 值 | 名字 | 流 | 可靠性 |
|---|---|---|---|
| 1 | Hello | 0 | 握手 |
| 2 | HelloAck | 0 | 握手 |
| 3 | Ping | 0 | 不可靠 |
| 4 | Pong | 0 | 不可靠 |
| 5 | Video | 1 | 不可靠，参考帧可请求重发 |
| 6 | Audio | 2 | 不可靠 |
| 7 | Input | 3 | 不可靠，最新优先 |
| 8 | CursorPos | 4 | 不可靠，最新优先 |
| 9 | CursorShape | 4 | 可靠 |
| 10 | Feedback | 5 | 不可靠 |
| 11 | KeyframeRequest | 0 | 可靠 |
| 12 | Capability | 0 | 可靠 |
| 13 | SessionControl | 0 | 可靠 |
| 14 | Clipboard | 0 | 可靠 |
| 15 | FileControl | 6 | 可靠 |
| 16 | FileChunk | 6 | 可靠 |
| 17 | TerminalOpen | 7 | 可靠 |
| 18 | TerminalData | 7 | 可靠 |
| 19 | TerminalResize | 7 | 可靠 |
| 20 | Ack | 见 ARQ | 不可靠，但是否到达由重传兜住 |
| 21 | Bye | 0 | 可靠，发一次也接受不可靠副本 |

`stream_id` 必须和上表一致。不一致的包丢弃。这样文件重传不会插进视频重组器。

`body_flags` 通用位：

| 位 | 含义 |
|---|---|
| 0 | IDR / 关键帧 |
| 1 | 参考帧（丢了需要 KeyframeRequest 或重传） |
| 2 | 分片结束 |
| 3 | FEC 校验包，不是媒体 |
| 4 | 发送方请求确认（可靠流） |

## 7. 可靠 ARQ

只用于 stream 0、4 的形状、6、7，以及带“请求确认”的 KeyframeRequest。

每个可靠流有自己的 `rel_seq`，放在 body 前 4 字节。外层 `packet_seq` 仍全局递增。可靠体：

```text
body = rel_seq_u32 || ack_u32 || ack_bits_u64 || payload
```

`ack` 是对方已连续收到的最大 rel_seq。`ack_bits` 的第 i 位表示 `ack+1+i` 是否已收到，用来选择重传，而不是停等。

规则：

- 一条可靠消息必须放进一个数据报。更大的内容先切成多条消息，不在 UDP 层再套一层分片。`ack_bits` 只有 64 位，所以窗口不能大于 64，除非改协议。
- 窗口 `CONTROL_WINDOW = 64`。窗口满则暂停该流的新消息，不影响 stream 1/3/5。
- 重传超时 `RTO = clamp(rtt_ms * 3 / 2, 20, 500)`。
- 同一可靠消息最多重传 8 次，然后该流失败，会话进入重连。不静默丢掉文件块。
- 视频流不进入这个窗口。
- FileChunk 和 TerminalData 是两个流，终端输出打满时文件让路：实现上终端流优先占用窗口的一半。第一版终端流量很小，可以简单做成终端优先发送。

Ack 消息本身不可靠。每个反向数据包的可靠体里都带最新 ack。没有反向数据时，每 50ms 发一条 `Ack`。

## 8. 握手

信令先把双方临时公钥和签名送到对端。UDP 上的 Hello 用来确认这条路径真能通，并绑定地址。

Hello body：

```text
session_id[16]
sender_device_id[32]
eph_pub[32]
sig[64]
path_id_u8          # 0=lan 1=p2p 2=relay
```

HelloAck body：

```text
session_id[16]
echo_packet_seq_u32
path_id_u8
```

双方都收到对端 HelloAck，并且验签已在收到信令转发的签名时完成，进入 `keys_ready`。只收到 Hello 不算完成，否则单向伪造地址会把状态机推过去。

8 秒内完不成，状态到 Failed，原因 `handshake_timeout`。

## 9. 消息体

### 9.1 Capability

被控端在 `keys_ready` 后第一条可靠消息必须是 Capability。主控收到前不发送输入。

```text
protocol_version_u8
platform_u8          # 1 win 2 mac 3 linux 4 android 5 ios
feature_bits_u32
display_count_u8
displays[]:
  display_id_u32
  origin_x_i32
  origin_y_i32
  width_u32
  height_u32
  scale_milli_u32    # 1000 = 100%，1500 = 150%
  refresh_hz_u16
  is_virtual_u8
encoder_caps_u32     # bit0 h264 bit1 h265 bit2 yuv444 bit3 hdr
audio_codec_u8       # 1 = AAC-LC，0 = 没有音频能力
max_touch_points_u8
```

`feature_bits`：

| 位 | 能力 |
|---|---|
| 0 | video |
| 1 | audio_out |
| 2 | audio_in，第一版恒 0 |
| 3 | input |
| 4 | clipboard |
| 5 | file |
| 6 | terminal |
| 7 | privacy |
| 8 | wake |
| 9 | virtual_display |
| 10 | watch_only，强制无输入，和 bit3 互斥 |

iOS 节点若被邀请当被控，信令阶段就拒绝，不建立 UDP。拒绝原因 `platform_not_hostable`。

### 9.2 SessionControl

```text
action_u8
display_id_u32
profile_u8           # 1 清晰 2 均衡 3 高帧率（曾叫游戏） 4 省流量（曾叫省流）
flags_u32
```

`action`：1 开始流，2 切换显示器，3 切换画质，4 开隐私，5 关隐私，6 暂停输入，7 恢复输入，8 结束。

被控端可以拒绝隐私：回一条 SessionControl action=5，`flags` bit0 = 拒绝。主控必须显示“此设备不能隐藏本机画面”，不能显示成已打开。

### 9.3 Video

body 在可靠头之外，视频没有可靠头。

```text
codec_u8             # 1 h264 2 h265
timestamp_capture_us 已在内层头
slice bytes          # 一个访问单元可以切多片，slice_count > 1
```

分片重组键是 `(frame_id, slice_index)`。200ms 内没收齐就丢整帧。若该帧 `body_flags` bit1 = 1，立刻发 KeyframeRequest。

KeyframeRequest body：`lost_frame_id_u32`。被控端收到后下一个编码帧必须是 IDR，并且最多每 200ms 响应一次，避免主控抖动时把码率打爆。

即使没有请求，被控端也至少每 `IDR_MAX_INTERVAL_MS` 出一个 IDR，方便中途加入解码和错误恢复。

### 9.4 Audio

```text
codec_u8 = 1
sample_rate_u32 = 48000
frame_samples_u32 = 1024
payload               # 一帧 AAC-LC
```

音频不重传。缺帧由播放器插入静音，不拉长视频缓冲去等音频。

### 9.5 Input

```text
event_id_u32
kind_u8
age 不放在包里。接收方用 timestamp_us 和会话时钟比较
payload
```

`kind`：

| 值 | 载荷 |
|---|---|
| 1 绝对移动 | x_i32, y_i32，被控虚拟桌面坐标，物理像素 |
| 2 相对移动 | dx_i32, dy_i32 |
| 3 按键 | hid_usage_u16, down_u8 |
| 4 鼠标键 | button_u8, down_u8 |
| 5 滚轮 | dx_i16, dy_i16 |
| 6 触摸 | pointer_id_u8, action_u8, x_i32, y_i32 |
| 7 手柄 | 预留。第一版收到则忽略并在 Capability 里不声明 |

键盘传 HID usage，不传字符。大小写和输入法由被控端当前焦点决定。主控端输入法组字不在第一版同步。这是已知限制，界面在会话开始时不需要打断用户，但帮助里要写。

接收方：`now - timestamp_us > 200ms` 的移动和滚轮丢弃。按键按下若过期也丢弃，避免补一次点击。已经注入的按下必须有超时：按键 500ms 内没有对应抬起，被控端自动抬起。这防止主控崩溃留下按住的修饰键。鼠标键和触点不套 500ms（拖拽、长按本来就会超过），只在 30s 没有任何移动/更新时兜底抬起，会话结束时立即全部释放。

### 9.6 CursorPos / CursorShape

CursorPos body：`x_i32 y_i32 visible_u8`。只保留最新。不重传。

CursorShape 走可靠流：

```text
hotspot_x_u16 hotspot_y_u16
width_u16 height_u16
hash_u64
bgra pixels, 每像素 4 字节
```

形状没变不发送。主控按 hash 去重。形状包超过单包就分片，走 ARQ 的通用分片字段。

### 9.7 Feedback

主控每 50ms 发一条，不可靠。

```text
recv_bitrate_bps_u32
loss_permille_u16          # 过去 1 秒，千分比
jitter_ms_u16
rtt_ms_u16
decode_queue_u8
present_interval_ms_u16
highest_frame_id_u32
```

被控端用它做拥塞控制，不另做探测流。Ping/Pong 只为了时钟和 RTT。

### 9.8 Clipboard

```text
direction_u8               # 1 主控到被控 2 反向
mime_u8                    # 1 utf-8 文本
byte_len_u32
bytes
```

大于 `CLIPBOARD_MAX` 的请求在本地拒绝，不发送。第一版没有图片剪贴板。剪贴板不是后台同步：主控只在会话窗口聚焦、且用户按下粘贴或点击“发送剪贴板”时发送。被控端收到完整文本后写入剪贴板，并注入一次粘贴。不监听任何一端的日常复制。文本超过一个数据报时切成多条 Clipboard，用 `frame_id` 当批次号，`slice_index/slice_count` 只表示批次进度；缺一条则整批失败，不写入半截文本。

### 9.9 文件

FileControl action：1 提议，2 接受，3 拒绝，4 取消，5 完成，6 错误。

提议：

```text
transfer_id_u32
direction_u8
name_utf8，长度 u16，最长 255 字节
size_u64
sha256[32]
```

FileChunk：

```text
transfer_id_u32
offset_u64
bytes                      # 最多 FILE_CHUNK，整条消息必须小于 MAX_BODY
```

1024 字节是为了让一条文件消息放进一个数据报。64 条窗口、20ms 往返时大约 3 MB/s，局域网更快。第一版够用。不够时加大 `FILE_CHUNK` 以外的做法是改 `ack_bits` 宽度，那是协议版本 2 的事，不在实现里偷偷加长。

接收方写入临时文件，校验 SHA-256 后再改成最终名字。校验失败删除临时文件并回 action=6。取消同样删除临时文件。不覆盖已有文件；同名则在最终名后加 ` (2)`，这个决定在接受时就返回最终名。

最终名回在 action=2 的载荷里：`final_name`。

### 9.10 终端

TerminalOpen：`cols_u16 rows_u16 term_utf8`，默认 `xterm-256color`。

TerminalData：原始字节，不做行纪律。最大单条 16 KiB。

TerminalResize：`cols_u16 rows_u16`。

被控端启动本机用户的默认 shell，工作目录是该用户主目录。不提供提权。不注入到已有 root 会话。

### 9.11 Bye

```text
reason_u16
```

原因码见第 13 节。收到 Bye 后停止注入、停止编码、恢复隐私，再回一条 Bye。只发不收回复也可以，对端超时会走同一条恢复路径。

## 10. FEC

视频流分组。每组 `D` 个数据包加 `P` 个异或校验包。

| 过去 1 秒丢包 | D | P |
|---|---|---|
| < 1% | 8 | 1 |
| 1%–5% | 4 | 2 |
| > 5% | 4 | 2，并且码率控制器必须降档 |

校验包 `body_flags` bit3 = 1。body 前 2 字节是组号，其后是本组数据片的异或。长度按组内最长片，短片用 0 填充，填充长度记在校验头里。

FEC 恢复不触发重传。恢复失败且该帧是参考帧，才发 KeyframeRequest。非参考帧恢复失败就丢掉。

第一版不做 Reed-Solomon。异或不够时靠降码率和关键帧，不靠更复杂的纠删码把实现拖住。

## 11. 拥塞 JCC

被控端持有发送码率 `target_bps`，初始值等于画质档起点。

每收到一条 Feedback，或最迟每 200ms，更新一次：

```text
loss = loss_permille / 1000
gradient = 本次 rtt_ms - 上次 rtt_ms

若 loss > 0.05 或 gradient > 15:
  target = target * 0.8
否则若 loss < 0.005 且 gradient < 5 且 decode_queue == 0:
  target = target * 1.05
否则:
  不变

target = clamp(target, 档位下限, 档位上限)
```

档位下限是起点的一半，但不低于 500 kbps。上限见产品文档。

码率变化小于 10% 时不调用编码器 `set_bitrate`，避免硬件编码器来回重置。

帧率台阶：120、60、30、15。当 `target` 低于“当前分辨率宽度×高度×帧率×0.07”时降一档帧率。这个系数是经验门槛，实现时做成命名常量 `JCC_BITS_PER_PIXEL_THRESHOLD = 0.07`，方便以后用自己的测量改，不散落在代码里。

升帧率要连续 2 秒条件满足，降帧率立即。升容易抖，降要快。

## 12. 路径选择

同时探测三条路径，每条 20 个探测包，最长等 300ms。

| path_id | 做法 |
|---|---|
| 0 lan | 信标看到的私网地址，直接发 |
| 1 p2p | 信令交换的映射地址，双方同时向对端映射地址发包 |
| 2 relay | 双方都向中继发，token 相同 |

空探测包没有身份，只说明这个地址有回复。路径必须在该地址上完成 Hello / HelloAck 之后才能参加选路。只回答空探测的地址得分为无穷。

得分越低越好，只统计已经握手成功的路径：

```text
score = loss_permille * 10 + jitter_ms * 20 + rtt_ms
```

丢包权重最高。RTT 低但丢包高的直连必须输给稍慢的干净中继。三条都完不成握手则会话失败，原因 `no_path`。不无限重试。用户再点一次可以重新邀请。

选定后，其他已握手路径每 2 秒发一个探测包保活。当前路径连续 1 秒没有媒体也没有 Pong，切到得分次优、且最近 2 秒仍有已认证往返的路径。未完成握手的地址不能作为备用。切换不换密钥。主控解码器若发现时间戳回跳，丢弃旧路径迟到包。

## 13. 原因码

| 码 | 名字 | 用户可见的话 |
|---|---|---|
| 0 | ok | 已断开 |
| 1 | user_hangup | 已断开 |
| 2 | rejected | 对方拒绝了 |
| 3 | busy | 对方正在被另一台设备控制 |
| 4 | unauthorized | 没有无人值守授权 |
| 5 | handshake_timeout | 连不上 |
| 6 | no_path | 直连和中继都失败 |
| 7 | version_mismatch | 需要升级 |
| 8 | platform_not_hostable | 这台设备不能被控 |
| 9 | permission_missing | 对方没有授予系统权限 |
| 10 | session_dead | 连接中断 |
| 11 | revoked | 设备已从舰队移除 |
| 12 | privacy_unsupported | 不能隐藏本机画面 |

日志用名字，界面用右边的句子。不把枚举数字给用户看。

## 14. 状态机

```text
Idle
  --本地发起--> Outgoing
  --收到邀请--> Incoming
Outgoing
  --对端签名公钥到达--> Handshaking
  --拒绝 / 8s--> Failed
Incoming
  --本机点击允许，或票据验签通过--> Handshaking
  --拒绝--> Idle
Handshaking
  --双方 HelloAck--> Connecting
  --8s--> Failed
Connecting
  --选路完成且 Capability 已到--> Streaming
  --no_path--> Failed
Streaming
  --当前路径死亡且还有备用--> Reconnecting
  --Bye / session_dead--> Closing
Reconnecting
  --备用路径通--> Streaming
  --5s--> Failed
Closing
  --恢复隐私和按键--> Idle
Failed
  --通知 UI--> Idle
```

任何进入 Idle 的路径都必须执行 `release_all`：抬起所有按键和鼠标键、关闭隐私屏、停止采集、停止编码器。`release_all` 要能在进程重启后重入，见架构文档的恢复文件。

## 15. 信令帧

信令不是 UDP 内层。它是 TLS 上的长度前缀帧。

```text
len_u32 || msg_type_u8 || body
```

单帧最大 64 KiB。超限断开连接。

| type | 名字 | 方向 |
|---|---|---|
| 1 | Enroll | 节点 -> 信令 |
| 2 | EnrollOk | 回 |
| 3 | Heartbeat | 节点 -> 信令 |
| 4 | Presence | 信令 -> 节点，全量或增量 |
| 5 | Invite | 主控 -> 信令 -> 被控 |
| 6 | InviteDecision | 被控 -> 信令 -> 主控 |
| 7 | Ephemeral | 双向转发，信令不解释 |
| 8 | Candidates | 双向转发 |
| 9 | RelayTicket | 信令 -> 两端 |
| 10 | Revoke | 信令 -> 节点 |
| 11 | Wake | 主控 -> 信令 -> 同网代发节点 |
| 32–35 | 管理帧 | 审计查询、设备撤销、轮换令牌、设备改名，见 `docs/07-servers.md` |
| 36 | StoreGrant | 被控 -> 信令，缓存已签名的无人值守授权断言供多端展示 |
| 37 | SetWake | 节点 -> 信令，登记本机允许远程唤醒的网卡 MAC |
| 38 / 39 | ReflexiveAsk / ReflexiveOk | 节点 <-> 信令，报备并取回媒体 socket 的公网反射地址 |
| 40 / 41 | AuthChallenge / PairPending | 信令 -> 节点，`--private` 封闭配对模式的连接挑战与待审批核对码 |
| 42 | Diagnostic | 节点 -> 信令，定长 16 字节状态诊断事件（不含任何自由文本或载荷内容） |

31 及以下留给会话转发，不要复用。

普通模式下 Enroll body：`fleet_token[32] || device_id[32] || sig[64] || name || platform_u8 || feature_bits_u32`，签名覆盖 token 和 device_id；令牌错误直接断开，不提示“差一位”。封闭配对模式（`--private`）下，信令在 TLS 建立后先发 32 字节随机 `AuthChallenge`，节点在同一 `Enroll` 结构中把 `fleet_token` 字段替换为该 challenge nonce 并签名；未批准设备收到 `PairPending`（8 位核对码或 `closed`），详见 `docs/09-private-pairing.md`。

Heartbeat 每 5 秒。15 秒没有心跳，信令把该设备标离线并广播 Presence。

Invite 含 session_id、主控 device_id、被请求的 feature。被控正在会话中回 busy，不把邀请排队。

Ephemeral 体是已经签好名的临时公钥。信令最大转发 256 字节，不解析签名。

RelayTicket：`routing_token[16] || relay_host_utf8 || relay_port_u16 || expire_unix_u32`。有效期 60 秒。过期要重新邀请，不刷新一张无限票。

## 16. 局域网信标

每 2 秒向广播地址和 `239.255.42.210:47212` 发一次。组播地址选在管理范围外的组织本地组播，实现时固定这个地址，不扫描。

```text
magic 0x4A52
version u8
device_id[32]
signal_short_id[8]     # 用来确认是同一个舰队，不是全局发现
tcp_port_u16 = 0       # 第一版不在局域网开入站服务
```

听到信标只把对应 device_id 标成局域网可达，并记下源地址作为 path 0 的候选。不听到信标不影响经信令的会话。信标没有舰队令牌，不携带密钥。它不是注册通道。

## 17. 版本协商

Capability 和 EnrollOk 都带主版本。主版本不同：信令拒绝注册到一个它不认识的行为上，会话原因码 `version_mismatch`。

次版本不在第一版出现。字段预留用“尾部可扩展”规则，不另设协商表。

## 18. 实现约束

- `jr-proto` 对所有消息做往返单测：编码再解码，字节相等。
- 模糊测试至少覆盖外层头和内层头的截断、超长、错误 magic。
- 重放窗口有单测。
- 密钥派生有已知答案向量，向量写在 `jr-crypto` 的测试里，不依赖在线服务器。
- 协议 crate 不读系统时钟。时间由调用方传入，否则单测不稳。
