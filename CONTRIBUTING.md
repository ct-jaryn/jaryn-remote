# 参与贡献与折腾指南（Contributing）

这是我的个人自研开源项目，非常欢迎你提反馈、贴真机运行日志，或者直接发 Pull Request 一起折腾！随手点颗 **Star ⭐** 或提一条 Issue 指出问题，对我来说都很有帮助。

---

## 目前可以一起折腾的方向

如果你想找个切入点练手或帮忙完善，下面几块是接下来重点要做的（详见 [TASKS.md](TASKS.md) 与 [docs/08-roadmap.md](docs/08-roadmap.md)）：

| 方向 | 内容说明 | 相关目录 |
| --- | --- | --- |
| **双机真机实测与排障** | 在两台真实 Windows / Mac 之间跑画面解码、音频回环、Retina/高分屏光标落点与公网打洞，反馈实测表现或修真机小毛病 | `apps/node`、`crates/jr-host-win` |
| **阶段 3 独立媒体线程** | 把媒体轮询从 `tokio` 5ms 定时器搬到独立 OS 线程，配合只读 UI 快照彻底解开全局锁（方案已写在 [docs/11-node-refactor.md](docs/11-node-refactor.md)） | `apps/node/src/live`、`apps/node/src/node` |
| **Windows 链路台界面升级** | 对齐 macOS v2「链路台」布局，改进 Windows 原生设备窗口的交互细节 | `crates/jr-host-win/src/desk.rs` |
| **Linux 桌面门户采集** | 基于 XDG Desktop Portal + PipeWire 给有桌面的 Linux 补上画面采集与可选输入 | 新增平台适配层 |
| **手机端适配** | Android（主控 + 带前台通知的有限被控）与 iOS（仅主控）原生编解码与触摸映射 | 新增移动端工程 |

不确定某个改动合不合项目胃口时，先开个 [Issue](https://github.com/ct-jaryn/jaryn-remote/issues) 聊两句再动手，省得白费力气。

---

## 代码与设计边界

为了不把代码库越改越重，提交代码前请留意这四条硬规矩（建议先翻一下 [01-product.md](docs/01-product.md) 和 [02-architecture.md](docs/02-architecture.md)）：

1. **核心层不碰系统调用**：
   - `jr-proto`、`jr-crypto`、`jr-transport`、`jr-session`、`jr-input` 不直接调网络 socket、不读系统时钟、不碰原生窗口句柄，保持纯状态机和确定性单测。
   - `jr-proto` 保持 `#![forbid(unsafe_code)]`；加密算法只调 `ring`，不自造密码学算法。
2. **绝不碰灰产/隐蔽控制特性**：
   - 不做绕过系统权限弹窗、隐藏被控指示、免杀对抗、后台静默监听剪贴板或登录前安全桌面提权注入。
3. **跑过就是跑过，没跑过就直说**：
   - 只过了编译检查或单元测试、没在目标系统真机上实际点过的原生采集/编解码/注入改动，请在描述和文档里如实标明「未实机验证」。
4. **别把个人环境配进仓库**：
   - 源码、脚本、测试、服务配置和图片里不要出现真实公网 IP、域名、证书指纹、令牌或个人主机名/邮箱；示例统一用 RFC 5737 测试地址（如 `203.0.113.10`、`192.0.2.1`）。

---

## 提交前本地检查

发 PR 前跑一下这三条命令确认全绿即可：

```bash
cargo check --workspace --all-targets
cargo test --workspace
git diff --check
```

如果动到了线协议常量、消息字段或能力位，记得同步改一下协议唯一来源文档 [docs/03-protocol.md](docs/03-protocol.md)。

---

## 许可证约定

除非你另有说明，提交到本仓库的 PR 默认与项目保持一致，按 **MIT OR Apache-2.0** 双许可证授权发布。
