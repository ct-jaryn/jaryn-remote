# 任务清单

当前状态：无进行中任务；已完成 23 项（T01–T22 + 远端光标形状），未完成 8 项（U01–U08）。

范围：只包含当前开发机上能实现并用 `cargo check` / `cargo test` 验证的项；
需要设备、整机或双机验收的项单列。节点拆分的详细实现记录见
[docs/11-node-refactor.md](docs/11-node-refactor.md) 第 9 节，本文件只记任务状态与验证结论。

规则：一次只做一项；每项完成后运行最小相关测试并在此勾选、写明验证方式。
实现不了或验证不了必须如实标注，不能勾成完成。

## 一、未完成

### 需要设备 / 整机 / 双机

- [ ] U01 Android 主控与被控
  - 未完成：本机没有 Android SDK 和设备，不能实现也不能验收。
- [ ] U02 iOS 主控
  - 未完成：本机没有 iOS 工程和真机，不能实现也不能验收。
- [ ] U03 Linux 桌面采集 / 输入 / 音频 / 隐私屏
  - 未完成：本机是 macOS，没有 Linux 桌面会话，不能实现也不能验收。
- [ ] U04 触控输入实机验证
  - 现状：Windows 被控端已接 `InitializeTouchInjection` / `InjectTouchInput`，
    能力位按系统是否支持报 0 或 10 点；抬起/取消会清按住状态，会话结束补发取消。
  - 未完成：没有带触摸屏的 Windows；macOS 没有触摸屏，主控端触控手势也未接。
- [ ] U05 IPv6 与双栈媒体端口双机验证
  - 现状：本机测试通过（`cargo test -p jr-node --lib net_tests` 4 项）；
    双栈优先、候选地址接受 4/16 字节、中继 IPv6 与方括号、收发 v4 映射都已实现。
  - 未完成：没有在两台机器之间真的用 IPv6 跑过会话。
- [ ] U06 Wake-on-LAN 双机验证
  - 现状：`wake-mac AA:BB:CC:DD:EE:FF` 登记 MAC，信令代发魔包并回
    `wake-unavailable`；`cargo test -p jr-node --bin jr-node` 3 项通过。
  - 未完成：没有第二台机器，没有真的唤醒过一次。
- [ ] U07 真实双机 NAT、端到端延迟、200 MB 文件验收
  - 未完成：本机只有进程内模拟，不能代替两台物理机。

### 本机可做

- [ ] U08 阶段 2 收尾：`sync_capture` 的真实开流 / 系统授权 / 重试路径
  - 现状：「不需要采集」分支已有真实代码路径测试
    （`no_capture_wanted_clears_every_capture_flag`、`no_capture_wanted_keeps_display_choice`），
    重试状态机（`live/capture_state.rs`）和能力矩阵（`compose_features`）有纯函数测试；
    Windows 侧 `jr-host-win` 全部 30 项单测（含音频代际隔离与 `frame_delivery_allowed`）及
    `ConPTY` 终端集成测试已在 `x86_64-pc-windows-msvc` 真机跑通。
  - 未完成：真实开启桌面画面/音频流、读取系统录屏授权弹窗与实机中断重试仍待桌面交互验收。

## 二、已完成归档

`（未实机验证）` 表示只有编译检查或单元测试，没有在目标机器上跑过。

### 2026-10-19 首轮审查：P0 / P1 / 服务端

- T01 会话反馈真正发出，拥塞控制生效 —— `cargo test -p jr-session feedback_is_actually_queued_for_sending`
- T02 会话结束立即清理引擎，允许马上重邀 —— `cargo test -p jr-node --test m0`（3 项）；新增 `Node::hangup` 先发 Bye
- T03 软件遮挡控制消息回环 —— `cargo test -p jr-session degraded_privacy_reply_only_comes_from_host`
- T04 Windows ConPTY 不再把终端类型当命令执行 —— `cargo check -p jr-node`（Windows 分支未实机运行）
- T05 非 Windows 平台剪贴板分派降级 —— `cargo check -p jr-node`（缺 `x86_64-linux-gnu-gcc`，Linux 完整检查未完成）
- T06 可靠流失败与文件 Accept 丢失可感知 —— `cargo test -p jr-session reliable_failure_is_reported_once`
- T07 无人值守授权按能力位校验 —— `cargo test -p jr-crypto -p jr-session grant_feature_bits_are_enforced`；
  注意：旧票据 `feature_bits = 0` 升级后会被拒绝，需重新授权一次
- T08 macOS 从界面申请辅助功能权限 —— `cargo build -p jr-node`（未在真实桌面点按钮）
- T09 macOS 采集中途失败上报并有限重试 —— `cargo build -p jr-node`（未实机触发中断）
- T10 观看窗口画质/显示器下拉与会话同步 —— `cargo build -p jr-node`（未实机点下拉框）
- T11 Windows 软件遮挡层绘制提示文字 —— `cargo check -p jr-host-win`（Windows 目标，未实机看字体）
- T12 Windows 运行中改码率/帧率生效 —— `cargo check -p jr-host-win`（Windows 目标）
- T13 输入补齐：相对移动、鼠标释放、编辑键 —— `cargo test -p jr-input`（6 项）
- T14 Windows 设备窗口撤回授权与日志入口 —— `cargo test -p jr-host-win`（6 项）（未实机看布局）
- T15 正常结束的会话从信令 sessions 表移除 —— `cargo test -p jr-signal ended_sessions_are_dropped_from_the_routing_table`
- T16 回归测试与文档同步 —— `cargo test --workspace` 退出码 0

### 2026-10-19 / 10-20 平台补齐

- T17 Windows 观看窗口骨架（画质、显示器、断开、等比留边）—— `cargo test -p jr-host-win view::`；
  解码与输入由 T20 补齐（未实机验证）
- T18 CI 工作流 `.github/workflows/ci.yml`（macOS 全量测试、Windows 只检查 `jr-host-win`）—— 未在 GitHub Actions 上跑过
- T19 `wake-mac`、IPv6 双栈、光标形状、Windows 触控注入 —— `cargo test --workspace` 退出码 0（未实机验证）
- T20 Windows 观看窗口 H.264 解码上屏、键盘/滚轮注入、光标 AlphaBlend；Windows 被控端光标形状抓取；
  同时修掉触控屏幕坐标、跨会话光标去重、Retina 光标尺寸、光标形状分片、WoL MAC 校验、
  局域网记录过期、候选地址选择、鼠标键 500ms 误弹起、DPI 感知、关窗后无法再开、
  显示器下拉每帧重建、触控释放自死锁、IPv6 回退阻塞 socket 等问题 —— `cargo test --workspace`（未实机验证）
- T21 采集中断恢复、剪贴板/光标形状重组内存上限、Windows 采集帧释放与软编提示 ——
  `cargo test --workspace` 退出码 0（未实机验证）
- T22 音频链路（macOS ScreenCaptureKit / Windows WASAPI + MF）、Windows GPU 零拷贝、
  唤醒按钮、Windows 托盘 —— `cargo test -p jr-proto -p jr-session`、`cargo test -p jr-host-win`（21 项）
  （未实机验证）
- 远端光标形状协议与两端绘制（16 字节头 + BGRA，按可靠流分片）—— `cargo test -p jr-host-win`（18 项）（未实机验证）

### 2026-11-06 阶段 2 边界收紧与本机缺陷回归

边界收紧：信令只接 `&mut State`；`media_io` 不负责采集；采集状态机
（`live/capture_state.rs`）与能力矩阵（`compose_features` / `can_capture`）纯函数化并补测试；
采集/音频状态归零统一到 `reset_capture_state`。

本机挖出并修掉的缺陷：macOS Retina 光标只走一半；观看窗口失焦不补抬起
（macOS `clearPointerButtons`、Windows `window::clear_view_buttons`）；`end` 命令改走 `hangup()`
先发 Bye；关键帧请求标志积压；macOS 音频 `g_audio_gen` 代次隔离；「不需要采集」分支漏清
`audio_started`；`Wake` 全零 MAC 的提示补上「对方没登记网卡」。

验证：`cargo build -p jr-node`、`cargo test -p jr-node --lib`（25 项）、
`cargo test -p jr-host-win`（22 项）、
`cargo check -p jr-host-win --target x86_64-pc-windows-msvc --tests`、
`cargo check --workspace --all-targets`、`cargo test --workspace` 全绿，`git diff --check` 通过。
未实机验证：macOS 观看窗口失焦补抬起、音频代次隔离、Retina 光标换算、
Windows `clear_view_buttons` 都只做了编译或纯函数测试。
