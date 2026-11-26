# 参与贡献指南（Contributing Guide）

非常欢迎你参与 **Jaryn Remote** 的讨论、测试与代码贡献！无论是随手点亮一颗 **Star ⭐**、反馈一条真机运行日志、修正一处文档措辞，还是提交核心模块 PR，对项目都非常有价值。

---

## 当前最欢迎社区参与的方向

如果你正在寻找切入点，以下方向非常欢迎反馈与共建（详见 [TASKS.md](TASKS.md) 与 [docs/08-roadmap.md](docs/08-roadmap.md)）：

| 方向 | 具体内容 | 涉及目录 |
| --- | --- | --- |
| **双机真机实测反馈** | 在真实两台 Windows / macOS 设备间测试画面解码、系统音频回环、Retina/高 DPI 光标落点与 NAT 打洞表现，提交体验与日志反馈 | `apps/node`、`crates/jr-host-win` |
| **阶段 3 独立媒体线程** | 将媒体轮询从 `tokio` 5ms 定时器迁移至独立 OS 线程，配合只读 UI 快照彻底消除全局锁争用（方案已写在 [docs/11-node-refactor.md](docs/11-node-refactor.md)） | `apps/node/src/live`、`apps/node/src/node` |
| **Windows 链路台界面升级** | 对齐 macOS v2「链路台」信息架构，升级 Windows 原生设备窗口的布局与无障碍体验 | `crates/jr-host-win/src/desk.rs` |
| **Linux 桌面门户适配** | 基于 XDG Desktop Portal + PipeWire 为有桌面的 Linux 补齐受控录屏与可选输入能力 | 新增平台适配模块 |
| **移动端客户端** | Android（主控 + 前台通知被控）与 iOS（仅主控）原生编解码与触控映射适配 | 新增移动端工程 |

不确定某个改动是否符合项目方向时，随时先开一个 [Issue](https://github.com/ct-jaryn/jaryn-remote/issues) 聊聊即可！

---

## 动手前必读的四条工程准则

为保持代码库清晰、低延迟且可审计，请在编码前了解以下约定（建议先快速浏览 [01-product.md](docs/01-product.md) 与 [02-architecture.md](docs/02-architecture.md)）：

1. **核心协议层零系统调用**：
   - `jr-proto`、`jr-crypto`、`jr-transport`、`jr-session`、`jr-input` 严禁直接读写网络套接字、读系统时钟或依赖平台窗口句柄，所有时间与字节均由调用方注入，保证状态机 100% 可确定性单测。
   - `jr-proto` 保持 `#![forbid(unsafe_code)]`；加密原语统一走 `ring`，禁止自造密码学或混淆算法。
2. **坚守合规与用户知情边界**：
   - 绝不接受绕过操作系统权限弹窗、隐藏会话指示图标、免杀对抗、后台静默监听剪贴板或提权注入系统安全桌面的改动。
3. **诚实标注硬件验证状态**：
   - 仅完成编译检查或纯函数单测、尚未在对应操作系统物理机实跑验证的原生采集/编解码/注入代码，必须在文档和 PR 描述中如实写明「未实机验证」，切勿标为已真机验收。
4. **严禁引入任何私有环境信息**：
   - 源码、脚本、测试、服务文件与 SVG/PNG 素材中禁止出现真实公网 IP、域名、证书指纹、令牌、个人设备名或私有邮箱；文档与测试统一使用 RFC 5737 示例地址（如 `203.0.113.10`、`192.0.2.1`）。

---

## 本地校验清单

提交 Pull Request 前，请在本地运行以下命令确认全绿：

```bash
# 1. 全工作区所有目标（含 tests / bins）类型与编译检查
cargo check --workspace --all-targets

# 2. 全工作区单元与集成测试
cargo test --workspace

# 3. 检查行尾空白与冲突标记
git diff --check
```

若你的改动调整了线协议字段、扩展信令帧或能力位，请同步更新唯一权威协议文档 [docs/03-protocol.md](docs/03-protocol.md)。

---

## 贡献许可协议

除非你另有明确说明，你向本仓库提交的任何代码、文档或素材贡献，均默认按照仓库根目录的 **MIT OR Apache-2.0** 双许可证条款授权发布。
