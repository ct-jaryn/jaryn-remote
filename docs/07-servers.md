# 07 信令、中继和数据

服务器是所有者自己的两个进程。它们不是远控产品的托管版，也不兼容别人的 ID 服务器。

## 1. 进程

```text
signal   TCP 47210   TLS    有状态，SQLite
signal   UDP 47213   无解密  默认公网源地址反射端口（可通过 --reflex-port 调整）
relay    UDP 47211   无解密  内存状态，可重启
```

反射发现是打洞前的第一步。节点在自己那颗媒体 socket 上往信令的 UDP 端口发一颗 12 字节探针（`JRRF` + 随机数），探针先经 TLS（`ReflexiveAsk`）报备过，所以信令知道该回给谁；信令把看到的源地址通过 `ReflexiveOk` 回过去，节点再把这个地址放进候选列表排在直连候选前面。信令不参与打洞本身，也看不到任何密钥材料：它只知道「谁在哪个地址上」。独立部署时信令和节点默认都用 `47213/UDP`；若服务端通过 `--reflex-port` 改用其他端口，节点侧传入相同的 `--reflex-port` 对齐即可。

默认同一台机器。配置文件：

```text
signal_addr = 0.0.0.0:47210
relay_addr  = 0.0.0.0:47211
data_path   = ./jaryn-remote.sqlite
public_host = 所有者的域名或 IP    # 写进 RelayTicket
bandwidth_cap_per_session_bps = 80000000
```

`public_host` 是节点能访问到的地址。写错时打洞仍可能成功，中继路径会失败。界面在三次中继探测失败时提示检查服务器地址，而不是只说“连不上”。

## 2. 信令职责

只做这些：

- 校验舰队令牌，登记设备公钥。
- 维持心跳，向同账户设备推 Presence。
- 转发邀请、决定、临时公钥、候选地址。
- 签发 60 秒中继票。
- 保存被控端已经签好的授权断言，供展示。验签仍由被控端做。
- 写审计。
- 把唤醒请求转给同一私网里仍在线的节点。

不做这些：

- 不转发视频和文件。
- 不生成会话媒体密钥。
- 不解析 Ephemeral 签名的含义，只限制长度。
- 不做账号密码体系。第一版身份是设备密钥加舰队令牌。
- 不做网页管理后台。管理动作从已注册节点的 UI 发出：撤销、看审计、轮换令牌。

## 3. Presence

内存里每个设备一条：

```text
device_id
last_heartbeat_ms
lan_seen_ms          # 由各节点本地维护，不上报给所有人
blocker              # 节点心跳里带上
session_active_bool
```

心跳 body 带 blocker 和 session_active，这样列表状态不用另开查询。

15 秒无心跳标离线。离线仍保留设备记录。Presence 推送是增量：加入、离开、状态变化、撤销。节点连上时先收一份全量。

新设备 Enroll 成功，向其他在线设备推一条“新设备”Presence，UI 必须提示。这是安全功能。

## 4. 邀请时序

```text
Controller            Signal                 Host
    | Invite            |                      |
    |------------------>| 若 host 离线：失败    |
    |                   | 若 session_active：busy
    |                   |--------------------->| Invite
    |                   |                      | 验票据或弹窗
    |                   | InviteDecision       |
    |<------------------| <--------------------|
    | Ephemeral         |                      |
    |------------------>|--------------------->|
    |                   |<---------------------| Ephemeral
    | Candidates        |      双方转发         |
    | RelayTicket       |                      |
    |<------------------| -------------------->|
```

Invite 到达被控端后，被控端 60 秒内必须决定。超时信令代回 `unauthorized`，避免主控干等。用户还在看弹窗时，60 秒足够。不做到 180 秒。

一个被控端同时只处理一个 Invite。第二个直接 busy。

## 5. 中继

中继是一张表：`routing_token -> { endpoint_a, endpoint_b, last_ms, bytes }`。

规则：

- 第一包确定 endpoint_a，第二包若地址不同则确定 endpoint_b。
- 已经有两端后，来自第三地址的包丢弃。防止别人拿听到的 token 挤进来。token 仍是 128 位随机，听不到就猜不到。
- 只把包转给另一端。原样，包括外层头。
- 30 秒无包删除。
- 单会话超过 `bandwidth_cap_per_session_bps` 时丢包，不断开对端进程。发送端的 JCC 会因为丢包下降。
- 不记录载荷。可以记录 token、字节数、存活时间，供审计对账。token 和 session_id 的对应关系只在信令库里。
- 重启即清空。客户端把中继死亡当成路径失败，不要求中继做会话恢复。

中继不需要 TLS。它转发的已经是密文。它自己不分配媒体密钥。

## 6. 唤醒

信令不能对公网发出魔术包就叫醒家里的电脑。NAT 后面的广播到不了。

Wake 请求：

```text
target_device_id
mac[6]
```

信令把它转给与目标“最近一次在同一局域网信标里出现”的在线节点。这个“同一局域网”由节点上报：心跳里可以带一个 `lan_hash`，是本机默认网关 MAC 的哈希，或信标里听到的同伴集合哈希。第一版用更简单的规则：节点在心跳里报告自己过去 30 秒听到的舰队 device_id 列表。信令选一个正在听到目标、且自己在线的节点代发。

代发节点向登记的广播地址发送标准唤醒魔术包。没有代发节点时，信令回 `wake_unavailable`。主控若自己刚才听到过目标的信标，可以自己发，不必问信令。

MAC 地址由被控端在用户打开“允许远程开机”时上报。默认不上报。

## 7. 数据模型

SQLite，同步写审计，其余可以批量。表：

```text
meta
  key text primary key
  value blob
  -- fleet_token_hash, signal_pubkey, created_at

device
  device_id blob primary key          -- 32
  account_id text                      -- 第一版恒为 "owner"
  name text
  platform integer
  short_id text unique
  enrolled_at integer
  revoked_at integer                   -- 空表示有效
  wol_mac blob                         -- 可空
  wol_broadcast text                   -- 可空

grant_cache
  host_id blob
  controller_id blob
  assertion blob
  stored_at integer
  primary key (host_id, controller_id)

audit
  audit_id integer primary key
  controller_id blob
  host_id blob
  started_ms integer
  ended_ms integer
  end_reason integer
  path_id integer
  bytes_to_host integer
  bytes_to_controller integer
  feature_bits integer

revoked
  device_id blob primary key
  revoked_at integer
```

在线状态不入库。进程重启后所有设备都要重新心跳，这是对的。

`account_id` 现在恒为 `owner`。表里留着，是为了以后不加列就能放第二个人。第一版代码可以拒绝任何非 owner 的写入。

令牌轮换只改 `meta` 里的哈希。不迁移设备表。

## 8. 管理操作

从已注册节点发到信令的管理帧，走同一条 TLS，消息类型从 32 起，和会话转发分开：

| type | 作用 |
|---|---|
| 32 | ListAudit，分页，每页 50（仅返回请求设备自己参与过的会话） |
| 33 | RevokeDevice（`--private` 封闭配对模式下禁用客户端发起，仅允许服务端本机命令撤销） |
| 34 | RotateFleetToken，返回新令牌一次（`--private` 模式下禁用） |
| 35 | RenameDevice，只能改自己的名字，或 owner 节点改任意未撤销设备 |
| 36 | StoreGrant，被控端上传已签名的无人值守授权断言副本 |
| 37 | SetWake，登记本机允许远程唤醒的单播网卡 MAC |
| 38 / 39 | ReflexiveAsk / ReflexiveOk，报备并返回媒体端口公网反射地址 |
| 40 / 41 | AuthChallenge / PairPending，`--private` 封闭配对挑战与待审响应（详见 `docs/09-private-pairing.md`） |
| 42 | Diagnostic，定长 16 字节连接与会话状态诊断码（单连接限速 120 条/分钟） |

第一版所有已注册设备都能撤销其他设备。舰队只有所有者一个人，这是可以接受的。不在第一版做管理员角色。手机丢了就从还在手里的电脑撤销它。

RotateFleetToken 之后，界面要求所有者把新令牌抄到要新加入的设备上。已在舰队中的设备不受影响。

## 9. 局域网降级

节点配置了信令地址但信令连不上时：

- 仍听信标。
- 对听到的、并且本机曾经成功验证过其设备密钥的同伴，允许发起仅局域网会话。
- 密钥验证过指：过去曾经共同完成过一次带签名的会话，本机把对端公钥标成“见过”。
- 没见过的信标设备不能仅靠局域网控制。否则换一个公钥就能在咖啡厅广播自己是“你的电脑”。

首次配对必须有信令或由用户在两端输入一次性 6 位核对码。核对码流程：

```text
两端都离线服务器时，用户在两台电脑上打开“局域网配对”
被控端显示 6 位数字，60 秒有效
主控输入
双方在 UDP 上交换临时公钥，签名覆盖核对码和临时公钥
用户口头或目视确认短号
```

这是信令不可用时的唯一注册路径。核对码不写入协议的长期授权。配对成功后的信任是本机记住的公钥，和有信令时一样。

M3 之前可以只做信令注册。核对码写在这里，避免以后局域网配对变成另一套身份。实现放在 M3，和打洞一起做。若 M3 时间不够，核对码可以后移，但不能换成“看到信标就信任”。

## 10. 失败和容量

个人舰队的容量目标：64 台注册设备，4 路同时进行的会话。不是互联网公共中继的容量目标。超过 4 路会话时，信令仍允许邀请，中继按带宽上限自然丢包。不做排队系统。

信令单线程外加异步 IO 即可。不要为了想象中的规模拆微服务。

SQLite 文件损坏时，信令拒绝启动并打印路径。不自动删库重建，避免把舰队和审计清掉。恢复用所有者的文件备份。文档要求所有者自己备份这一个文件，服务器不上传到别处。

## 11. 日志

信令日志：连接、注册、邀请结果、撤销、票据签发。不记录 Ephemeral 正文。

中继日志：令牌前 4 字节、字节数、存活时间。不记录完整令牌，降低日志泄漏后被转发劫持的风险。完整令牌只活在内存。

两端日志保留 14 天，本机滚动。
