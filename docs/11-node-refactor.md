# 11 节点拆分与调整方案

本文描述重构方案；第 1、2 节的行数和目录是拆分前的基线，落地情况以第 9 节为准。

## 1. 结论

底层 crate 的分层是合理的，应该保留：

```text
jr-proto  <-  jr-crypto  <-  jr-transport  <-  jr-session
     ^            ^
     |            |
  jr-input     jr-host-win
```

- 协议、加密、传输、会话状态机都不碰系统调用，可以独立测试。
- `jr-host-win` 只依赖 `jr-proto` / `jr-input`，没有反向依赖节点或界面。
- 平台差异集中在 `jr-host-win` 和 macOS 的 ObjC 桥，符合“适配器不反向调用界面”。

真正需要调整的是节点这一层：

1. `apps/node/src/lib.rs` 约 2969 行，同时承担启动、信令、会话、文件、终端、授权和 UI 命令。
2. `live.rs` 约 896 行，能力协商、采集生命周期、观看端、事件分发和封包都在一起。
3. 媒体由 tokio 的 5ms 定时器驱动，每一拍都要拿全局 `Mutex<State>`；信令、UDP、UI 和媒体共享同一把锁。
4. macOS 原生桥和 Windows `jr-host-win` 各自实现媒体能力，暂时没有统一的媒体边界；这是已知债务，但不建议现在为了“好看”强行抽 trait。

## 2. 目标结构

第一阶段只拆 `apps/node`，不新增 crate。目标目录：

```text
apps/node/src/
  lib.rs                 # 模块声明、公共导出、平台入口，目标 < 300 行
  config.rs              # NodeConfig、启动参数、注册信息持久化
  state/
    mod.rs               # State、Shared、只读快照
    session.rs           # Sess、Peer、RelayEndpoint
  node/
    mod.rs               # Node::start、重连监督、NodeHandle 基础访问器
    commands.rs          # NodeHandle 对 CLI / UI 暴露的命令入口
  signal/
    mod.rs               # TLS 连接、读写循环、重连、出站队列
    enroll.rs            # enrollment、challenge、配对提示
    dispatch.rs          # on_signal、Presence、Invite、Decision、Ephemeral、Candidates
    heartbeat.rs         # heartbeat、status、beacon、reflex
  session/
    mod.rs               # begin / finish / fail / release
    handshake.rs         # Hello、HelloAck、seal_handshake、on_datagram
    path.rs              # 反射、候选选择、直连/中继切换、路径静默
    media_io.rs          # on_media、seal_pending、seal_media
  media/
    mod.rs               # poll_media 编排、能力协商、engine 生命周期
    capture.rs           # 视频/音频启动、失败检测、有限重试
    view.rs              # 观看端、输入、反馈、光标
    events.rs            # apply_events、release
  terminal.rs            # pty 会话、drain_shell、spawn_shell
  # 以下平台文件暂时留在原位
  av.rs
  inject.rs
  ui.rs
  desk.rs
  pty.rs
  autostart.rs
  tls.rs
```

约束：

- 只有 `node/` 和 `signal/` 可以碰 TLS 和信令帧。
- 只有 `session/` 可以碰 `jr_session::Engine` 的握手与路由。
- 只有 `media/` 可以碰采集、编码、音频和观看端。
- `terminal.rs` 不依赖媒体；文件、剪贴板和终端这类可靠控制不要塞进媒体循环。
- `lib.rs` 只做组装，不再实现业务。

## 3. 状态调整

### 3.1 先拆状态，不换锁

把 `State` 按所有权拆成两组字段：

```rust
struct State {
    control: ControlState, // 信令、peers、sessions、grants、UI、终端
    media: MediaState,     // engine、采集标志、计数器、光标、画质
}
```

这一步只是让字段归属清楚，暂时仍用一把锁。收益是后续可以把 `MediaState` 搬进独立线程，而不需要一次性重写所有信令代码。

### 3.2 再把媒体循环搬出 5ms 定时器

当前 `live::poll_media` 由 `tokio::time::interval(5ms)` 驱动，并且整段在 `State` 锁内执行。目标：

- 新增 `media` 线程（普通 OS 线程），独占 `MediaState` 和 `Engine` 的媒体侧。
- 信令/UDP/UI 通过有界 `mpsc` 发送 `MediaCommand`：开始采集、停止、切屏、画质、隐私、输入、音频重置。
- 媒体线程通过另一个有界队列把已加密的 `(SocketAddr, Vec<u8>)` 交给 UDP 发送任务；不直接 await 网络。
- UI 只读 `watch<Arc<UiSnapshot>>` 或短锁快照，不再和媒体抢同一把大锁。
- 信令侧保留自己的小锁或 actor；不要让媒体线程等 TLS。

### 3.3 是否上 actor

建议先做 3.2，并用指标验证收益；不要一开始就把所有东西改成 actor。

若 3.2 后仍有明显锁竞争，再把 `ControlState` 收进单线程 `NodeActor`：

```text
UI / CLI --Command--> NodeActor --Event/Snapshot--> UI
network tasks --Event--> NodeActor --outbound--> send task
media thread <--MediaCommand-- NodeActor
```

actor 的验收标准是“没有跨 await 持锁”，不是“用了 actor”本身。

## 4. 平台与媒体边界

现在不要抽 `jr-media` 通用帧 trait。原因：

- Windows 的零拷贝句柄是 D3D11 纹理，macOS 是 IOSurface/CVPixelBuffer。
- 硬抽 `GpuTextureHandle` 很容易在中间多一次 CPU 拷贝，直接破坏延迟目标。
- 两个平台都还没有真机验收，此时抽象会把未验证的行为固化。

建议的中间形态是一个很薄的平台门面：

```text
media facade（app 内）
  screen_capabilities()
  start_screen(config, sink)
  stop_screen()
  start_audio(config, sink)
  stop_audio()
  play_audio(bytes, ts)
  set_privacy(on)
```

门面只传控制参数和已编码字节，不传纹理。等 Windows/macOS 都跑过真机、确认各自的零拷贝路径后，再决定是否抽 crate，以及 trait 的粒度。

## 5. 分阶段迁移

### 阶段 0：基线

- 保留当前工作区快照。
- 记录 `cargo test --workspace`、Windows 交叉 `cargo check`、节点构建结果。
- 记录一次空闲和 60fps 会话的 CPU、唤醒次数、媒体 tick 抖动；没有基线就不改线程模型。

### 阶段 1：纯机械拆分

- 新建 `config.rs`、`state/`、`node/`、`signal/`、`session/`、`media/`、`terminal.rs`。
- 只移动代码和改 `mod` / `pub(crate)`，不改逻辑、不改线程、不改协议。
- `lib.rs` 保留 `pub use`，确保 CLI、macOS UI、Windows desk 和 `tests/m0.rs` 的公共 API 不变。
- 每一步都跑全量测试；单个文件超过 1200 行就继续拆。

验收：行为、日志、测试结果和阶段 0 一致；`git diff` 中只有移动和可见性变化。

### 阶段 2：收紧边界

- `signal/dispatch.rs` 的处理函数只接收 `&mut State` 和已解码的消息，不直接读 socket。
- `session/media_io.rs` 只负责会话内媒体封包，不负责采集。
- `media/capture.rs` 把“启动失败、线程死亡、切屏、重试次数”收成一个显式状态机，避免布尔标志散落。
- 为能力矩阵、重试状态机和隐私/音频组合补单元测试。

验收：新增测试覆盖状态迁移；`live.rs` 不再是所有逻辑的入口。

### 阶段 3：媒体线程与快照

- 把 `poll_media` 从 5ms tokio tick 移到媒体线程。
- 全局 `State` 锁不再覆盖编码、加密、FFI 和发送。
- UI/CLI 通过快照或命令访问节点。
- 保留旧路径作为短期开关，出现回归可以切回。

验收：30 分钟会话无死锁；空闲 CPU 和唤醒次数下降；60fps 下 tick 抖动不超过基线；断线、切屏、隐私、音频重启行为不变。

### 阶段 4：可选 crate 化

只有阶段 3 稳定且两台真机都验收后，才考虑：

```text
crates/jr-media-api   # 控制面类型，不含 GPU 句柄
crates/jr-media-win   # 包装 jr-host-win 的采集/编码/音频
crates/jr-media-mac   # 包装 macOS FFI
```

如果抽出来会让热路径多一次拷贝，就不抽；保留 app 内门面也是合理结果。

## 6. 明确不做

- 不为了目录好看把 `jr-proto`、`jr-crypto`、`jr-transport` 再拆细。
- 不把 `jr-session` 改成通用框架；它只服务 JRP。
- 不在没有真机数据前换编码器、抖动缓冲或零拷贝策略。
- 不把 UI 框架引入节点进程；现有原生窗口和单进程模型继续保留。
- 不做“大爆炸”重写。每个阶段都能单独回滚。

## 7. 风险

| 风险 | 处理 |
|---|---|
| 移动代码导致可见性/导入错误 | 只改 `mod` 和 `pub(crate)`，每步编译；公共 API 用 `pub use` 兜住 |
| 媒体线程与 UI 互相等待 | 所有跨线程队列有界；禁止在持锁时 await 或调用 FFI |
| 音频/采集状态机回归 | 阶段 2 先补状态迁移测试，再动线程 |
| 快照过期导致按钮状态错 | 快照带版本号；命令执行结果回写事件，不依赖旧快照 |
| 重构掩盖未实机验证的问题 | 每个阶段保留“未实机验证”标注；真机验收前不宣称完成 |

## 8. 完成标准

- `apps/node/src/lib.rs` 只做组装，目标 300 行以内。
- 信令、会话握手、媒体、终端、命令处理各有唯一入口。
- 媒体热路径不持有全局 `State` 锁，不在锁内做加密、FFI 或网络发送。
- `cargo test --workspace`、Windows 交叉 `cargo check`、`git diff --check` 通过。
- 所有平台行为仍如实标注实机验证状态。

## 9. 进度

> 第 1、2 节的行数和目录是拆分前的基线，实际结构以下面的进度记录为准。

2026-10-20 阶段 1 纯机械拆分（已基本完成；只移动代码和改可见性，不改线程和协议）。
以下行数是本轮用 `wc -l` 读回核对的实际值：

已落地结构：

- `live/`：`mod.rs`（77 行，`poll_media` 编排、`now_us`、`bitrate_of`）、
  `capabilities.rs`（173 行，能力位、`open_engine`、显示器列表）、
  `capture.rs`（166 行，`sync_capture` 采集/音频启停和有限重试）、
  `view.rs`（64 行，观看端同步与反馈）、`media.rs`（375 行，`on_media`、`seal_pending`
  及测试）、`events.rs`（453 行，`apply_events`、`release` 及测试）、
  `terminal.rs`（68 行，shell 会话与 recovery 提示）；
  `crate::live::*` 的公共路径（`poll_media`、`local_features_for`、`release`、
  `on_media`、`note_recovery`）保持不变。
- `config.rs`（41 行）：`NodeConfig`、`DeviceView`。
- `state.rs`（169 行）：`State`、`Shared`、`Peer`、`Sess`、`RelayEndpoint`，
  字段可见性统一放到 `pub(crate)`。
- `net.rs`（173 行）：注册与身份落盘、地址和双栈 socket 辅助。
- `signal/`：`mod.rs`（27 行，只做 re-export）、`dispatch.rs`（425 行，
  Presence / Invite / Decision / Ephemeral / Candidates）、`path.rs`（118 行，
  候选选择、路径静默、直连/中继切换、`finish_hangup`）、`handshake.rs`（564 行，
  Hello / HelloAck、会话开始与结束、票据与邀请校验、媒体包封装）、
  `heartbeat.rs`（144 行，heartbeat / status / beacon / reflex / WoL）、
  `enroll.rs`（106 行，enrollment / challenge / 配对提示、TLS 重连与读帧）。
- `node/`：`node/mod.rs`（426 行，`Node`、`NodeHandle` 定义、`Drop`、`Node::start`
  和各条 tokio 循环）、`node/commands.rs`（780 行，`NodeHandle` 对 CLI / UI 的命令入口）；
  `pub use node::{Node, NodeHandle}` 保持在 `lib.rs`。
- `lib.rs`（195 行，拆分前约 2969 行）：模块声明、公共导出、macOS/Windows 平台导出
  （`mac_setup`、`mac_error`、`mac_quit_requested`、`windows_quit_requested`、
  `request_permission`、`pump_ui`）、诊断（`send_diagnostic`、
  `send_state_diagnostic`、`send_status_diagnostic`、`invite_error_code`）、
  `find_short`、`send_replies` 和 `net_tests` 等装配代码。TLS 重连、心跳/Beacon、
  注册/配对已经不在 `lib.rs`。

阶段 1 已基本完成，剩余只有 `lib.rs` 的平台导出、诊断、`find_short`、`send_replies`、
`net_tests` 等装配代码；继续收口也只是把这些辅助移进 `node/` 或 `signal/`，
不再涉及业务拆分。

本轮修掉的缺陷（均有代码和测试证据；未做真机验证）：

- 可靠流失败：`SessionEvent::ReliableFailed` 不再先摘引擎，统一走
  `finish_hangup` → `release`，让 `Engine::close()` 照常产出
  `PrivacyOff` / `ReleaseAll` / `FileFailed`。测试
  `reliable_failed_closes_engine_and_releases_file` 验证遮挡层和半截文件都会被清掉。
- `release` 兜底：即使引擎已经被摘掉、没有收到 `PrivacyOff` / `ReleaseAll`，
  `release` 也会撤掉软件遮挡并清 `privacy_cover`。测试
  `release_unsets_privacy_cover_without_engine`。
- `on_media` 顺序：先用 AEAD 认证外层密文，认证通过后才推进重放窗口、更新
  `peer_addr` / `path` / `direct_addr` 和 `last_rx_us`。测试
  `forged_outer_packet_has_no_side_effects_and_does_not_move_replay_window`
  验证伪造包不会改地址，也不会把重放窗推到很远的序号。
- 中继回退：直连候选多次 Hello 没有 ACK 时，`set_peer_route` 同时把地址和路径
  写成 `PATH_RELAY`；`on_candidates` 收到可用直连候选且当前不在直连时切回直连，
  `switch_path` 也支持中继静默后回直连。集成测试
  `unreachable_direct_candidates_fall_back_to_relay_path` 验证两端都不再误报直连。
- Windows 音频采集会话与代际隔离：初始化超时和停止不再无界 `join`；`stop` /
  `join_within` 有等待上限，超时后 detach，`reclaim_stale` 不在启动路径上等待可能
  卡住的线程，`abandon` 不把超时会话留在全局槽里。采集侧用 `CAPTURE_GENERATION`
  记录当前代次，由 `next_capture_generation` 分配、`capture_generation_is` 核对、
  `invalidate_capture_generation` 只让指定一代失效；新会话启动前先占一代，
  `stop_capture`、`abandon` 和 `reset` 都会推进代次。采集线程在循环入口、每次
  `GetBuffer` 前、`ReleaseBuffer` 之后以及 `guarded_sink` 写入全局 sink 前重核代次，
  旧线程手里的帧不会送进编码器或下一会话队列。测试
  `session_teardown_invalidates_only_its_generation` 覆盖 stop、abandon、
  stale 会话收尾、`stop_capture` 和 `reset` 的代次推进；相关测试位于
  Windows-only 模块内，本机 macOS 只做了目标检查，未运行、未实机验证。
- macOS 播放释放：`jrp_audio_reset` 在播放队列上停掉 `AVAudioEngine` /
  `AVAudioPlayerNode`，并清空 `g_play_dec`、`g_play_aac`、`g_play_pcm`，
  避免下一次会话复用上一次会话的 AAC 解码 converter。`g_play_dec` 是播放
  decoder，和采集编码 converter 不是同一个实例。
- macOS 采集编码 converter 释放：`g_audio_enc`（连同 `g_audio_enc_in` /
  `g_audio_enc_out`）是采集端把 LPCM 编成 AAC 的 converter，不是播放 decoder。
  `jrp_audio_stop`、`jrp_capture_stop` 和 `jrp_audio_reset` 都在 `g_audio_enc_mu`
  下调用 `audio_enc_release_locked` 释放它；`AVAudioConverter` 内部缓存的不足一包
  尾部 PCM 不会留给下一段会话。未实机验证。
- macOS 采集代次：原生层用 `g_epoch` / `cap.epoch` 丢弃 stop 之后旧 `SCStream`
  的画面和音频回调，防止旧流混进新会话。未实机验证。
- macOS 采集编码锁内代次复核：`av.m` 的 `encode_audio_sample`（146 行）在入口
  已按 `self.epoch` 过滤旧样本，拿到 `g_audio_enc_mu` 后（160 行）又用传入的
  `epoch` 与 `g_epoch`、`g_audio_on`、`g_audio` 复核（169 行）；等锁期间被
  stop/reset 换代的样本不会重建 converter，也不会把残留 PCM 带进下一会话。
  调用点在 `JrpCap` 的 `didOutputSampleBuffer`（526 行）。未实机验证。
- macOS 会话收尾清采集回调：`jrp_audio_reset`（`av.m` 1140 行）在同一把
  `g_audio_enc_mu` 锁下（1151–1156 行）把 `g_audio_on` 清零、`g_audio` /
  `g_audio_ctx` 置空并调用 `audio_enc_release_locked`，再在播放队列上清
  `AVAudioEngine` / `AVAudioPlayerNode` 与播放 decoder；单独调用 reset 也不会
  留下仍认为会话存在的采集回调。未实机验证。
- Windows 帧交付纯函数：`crates/jr-host-win/src/audio.rs` 新增
  `frame_delivery_allowed(stop_requested, thread_generation, published_generation)`
  （812–818 行），只比较参数、不读全局也不拿锁；`capture_live`（822 行）和
  靠近 sink 的 `guarded_sink`（970–978 行）共用它。回归测试
  `frame_delivery_requires_current_generation_and_no_stop`（2290–2310 行）覆盖
  当前代次放行、旧代次丢弃、stop 后丢弃和异常代次；该测试在 Windows-only 的
  `audio` 模块内，本机 macOS 未运行，未实机验证。

验证：本轮复跑 `cargo test -p jr-node --lib` 14 项通过；`cargo test -p jr-node
--test relay_path` 4 项通过；`cargo test -p jr-node --test nat` 3 项通过
（首次运行有一项偶发失败，重跑通过）；`cargo test -p jr-host-win` 21 项通过；
`cargo check -p jr-host-win --target x86_64-pc-windows-msvc` 退出码 0。
`cargo test --workspace` 在并行子任务高负载时曾出现
`apps/node/tests/features.rs` 的 8 MiB 传输用例超时（`err=None`）；该用例单独复跑通过，
随后在没有并行代理时全量复跑两次均全绿。本轮没有跑 `cargo fmt`。所有平台行为
维持“未实机验证”；Windows 音频/采集、macOS 音频/采集和真实双机路径都没有
实机验收，交叉检查不等于真机验证。Windows-only 的
`frame_delivery_requires_current_generation_and_no_stop` 和代际测试在本机
macOS 不运行。

仍未完成、需要设备或新平台（与 `TASKS.md` 第一节 U01–U07 一致，均不得视为已完成）：

- Android 主控与被控：本机没有 Android SDK / 设备，未实现、未验收。
- iOS 主控：本机没有 iOS 工程和真机，未实现、未验收。
- Linux 桌面采集 / 桌面输入 / 音频 / 隐私屏：本机没有 Linux 桌面会话，未实现、
  未验收。
- 主控触控手势：Windows 被控端触控注入已接但未实机验证；macOS 主控端手势仍未接。
- 双机公网 NAT、IPv6、端到端延迟与 200 MB 文件：只有本机 / 进程内模拟，未做两台
  物理机验收。
- Wake-on-LAN 完整链路：未双机验证，`TASKS.md` 中仍为 U06。
- Windows 真机验收：观看窗口、音频、零拷贝、托盘、触控等只做了类型检查或单元
  测试，未在 Windows 机器上运行。
- macOS 音频采集 / 播放与真实桌面：只做了编译检查，未在已授权设备上实跑。

下一步进入阶段 2 的边界收紧：信令处理只接收 `&mut State` 和解码后的消息，
`media_io` 不负责采集，采集状态机补显式迁移测试；线程模型和协议暂不动。

2026-11-06 阶段 2 边界收紧 + 本机缺陷回归（线程模型和协议未动）。

已落地的边界收紧：

- 信令只接 `&mut State`：`signal/dispatch.rs` 的 `st_take_prompt` 改成
  `(st: &mut State, tx: &mpsc::UnboundedSender<Vec<u8>>)`，内部不再自己拿
  `Shared` 的锁；`node/commands.rs::pump_ui` 先在块内持锁调用它，放锁后才
  弹窗，避免「拿锁时发系统模态框」。该文件已无 `Arc` / `Shared` 依赖。
- `media_io` 不负责采集：`live/media.rs` 只有 `on_media`（AEAD 认证、重放窗、
  路径更新）和 `seal_pending`（封包），没有启动/停止采集的调用，本条已满足，
  本轮未再改代码。
- 能力矩阵纯函数化：`live/capabilities.rs` 新增 `compose_features(platform,
  os_inject, os_capture, clipboard_ready, audio_playback_ready, screen_ready,
  inject_trusted)` 和 `can_capture(platform)`，`local_features_for` 只负责探测
  系统状态再调用它。这样能力矩阵不再是「没法测的一串 if」，补了 3 项测试：
  `missing_adapters_clear_their_feature_bits`、
  `non_desktop_platforms_never_claim_desktop_features`、
  `capture_off_drops_video`。测试顺带确认了：Linux / Android / iOS 即使调用方
  误传 `screen_ready = true` 也不会报画面位（这些平台没有采集适配器），
  而 `os_inject = false` 时仍如实上报输入位。
- 采集状态机纯函数化：新增 `live/capture_state.rs`（101 行）集中
  `MAX_CAPTURE_RETRIES=2`、`CAPTURE_RETRY_DELAY_US=1_000_000`、
  `MAX_AUDIO_RETRIES=3`、`AUDIO_RETRY_DELAY_US=2_000_000`、`enum Next`
  和 `after_capture_failure` / `after_audio_failure` / `may_start_audio` /
  `should_start_capture`；`live/capture.rs` 改为直通这些函数，原来散在
  `sync_capture` 里的 `2` 次、`3` 次和秒数常量不再硬编码。
- 采集/音频状态归零只有一个入口：`live/events.rs::reset_capture_state`
  （`pub(super)`）由 `release`、`StreamStop`、`StreamStart` 换显示器和
  `sync_capture` 的「不需要采集」分支共用。换显示器的条件也从
  「`capture_started && 显示号不同`」放宽到「显示号不同」，采集中断后
  切到可用显示器不再被 `capture_failed` 永久挡住。

本轮修掉的缺陷（都有代码和测试证据；平台原生行为未实机验证）：

- Retina 光标换算：新增 `jr_input::local_from_points(point, display,
  scale_milli)` 和 `jr_input::points_to_physical(point, scale_milli)`
  （`crates/jr-input/src/lib.rs`）。macOS 的 `CGEventGetLocation` 给的是
  全局 point，Retina 上 1 point 是 2 像素，原来的
  `x.saturating_sub(display.origin_x)` 会让远端光标只走一半。
  `capture.rs` 的 macOS 分支改用 `local_from_points`，`inject.rs` 的
  `MOVE_REL` 基准点改用 `points_to_physical`。测试
  `retina_points_convert_to_physical_pixels`。
- 观看窗口失焦补发抬起：macOS `av.m` 的 `JrpVideoView` 新增
  `_middle_in_picture`、`otherMouseDown/Up` 和 `releasePointerButtons`，
  `windowDidResignKey` / `windowWillClose` 调用它，把按下中的左/右/中键
  补一次抬起，否则拖出窗口或切走应用后远端会一直按着。会话结束时
  `jrp_view_reset` 只清本地按下标记（`clearPointerButtons`），不往已经
  结束的会话补发。Windows 侧 `crates/jr-host-win/src/view.rs` 的
  `reset_session` 也清掉排队输入和 `VIEW_BUTTONS`，并新增
  `window::clear_view_buttons()`；`WM_KILLFOCUS` / `WM_DESTROY` 仍走
  `release_view_buttons()`。新增测试 `reset_session_drops_queued_input`。
  macOS 部分只做了编译，未实机验证。
- CLI / 托盘挂断一致：`apps/node/src/main.rs` 的 `end` 命令从
  `end_session()` 改为 `hangup()`。`end_session` 只清本机，对端要等 30s
  路静默才发现，期间还以为自己在被控制；`hangup()` 先发 Bye。
- 关键帧请求标志不再积压：`live/mod.rs::poll_media` 原先只在 `view_open`
  为真时才 `take_view_keyframe_request()`，被控端没有观看窗口时该标志只增
  不减，下一段会话一开窗就会立刻补发一条过期请求，打断对端刚编好的 GOP。
  现在先无条件取走，再判断是否需要真的发。
- 音频代次隔离补一层：`av.m` 新增 `g_audio_gen`。`g_epoch` 只在整条采集流
  stop/start 时推进，同一代采集里音频 stop → start 不会变；旧 PCM 等锁醒来
  时 `g_audio_on` 又变回 1，会被编进新一段声音。`jrp_audio_stop`、
  `jrp_capture_stop`、`jrp_audio_reset` 在 `g_audio_enc_mu` 下
  `atomic_fetch_add(&g_audio_gen, 1)`；`encode_audio_sample` 多接一个
  `audio_gen` 参数，与 `epoch`、`g_audio_on`、`g_audio` 在同一临界区比对。
  未实机验证。
- `sync_capture` 的「不需要采集」分支补音频收尾：原来只停视频，
  `st.audio_started` 还留着 `true`，下一段会话开局会先被判成「音频采集中断」
  白烧一次重试预算。现在连音频状态一起清。

验证：`cargo build -p jr-node` 通过（含 `av.m` 重新编译，`av.o` 时间戳更新）；
`cargo test -p jr-node --lib` 25 项通过（新增
`switching_display_clears_stale_capture_failure`、
`capture_start_waits_for_retry_and_stops_after_giveup`，以及能力矩阵的
`missing_adapters_clear_their_feature_bits`、
`non_desktop_platforms_never_claim_desktop_features`、`capture_off_drops_video`，
以及 `sync_capture` 的 `no_capture_wanted_clears_every_capture_flag`、
`no_capture_wanted_keeps_display_choice`）；
`cargo test -p jr-host-win` 22 项通过（新增 `reset_session_drops_queued_input`）；
`cargo check -p jr-host-win --target x86_64-pc-windows-msvc --tests` 通过；
`cargo check --workspace --all-targets` 通过；`cargo test --workspace` 全绿。
未跑 `cargo fmt`（按前几轮约定）；`git diff --check` 通过（仓库可用，
无空白或冲突标记）。

阶段 2 仍未完成的部分：`sync_capture` 现在有「不需要采集」这一条真实分支的
测试（`no_capture_wanted_clears_every_capture_flag`、
`no_capture_wanted_keeps_display_choice`，走真实代码但不碰原生采集），
但真的开流、读系统授权、重试推进这些路径仍然只能靠纯函数测试覆盖；
Windows-only 的音频/采集中断路径本机不运行。这两条在 `TASKS.md` 里记为 U08。
