# 09 自托管部署：封闭配对与设备鉴权

除默认的共享 `fleet-token` 注册方式外，`jr-signal` 提供面向长期公网自托管的 **封闭配对模式（`--private`）**。

在该模式下，信令服务器不生成、也不接受共享注册令牌，默认拒绝一切未批准的新设备；管理权限完全绑定在服务器本机（SSH 访问权限），不开放任何网页后台或公开注册接口。以下示例均使用 RFC 5737 文档示例公网地址 `203.0.113.10`，实际部署时请替换为你自己的服务器公网 IPv4。

## 1. 服务端部署与端口

信令服务与 UDP 密文中继可部署在同一台 Linux 主机上（systemd 单元示例见仓库 `deploy/jaryn-signal.service` 与 `deploy/jaryn-relay.service`）：

```bash
# 启动封闭配对模式信令服务（首次启动会生成 TLS 证书并打印 64 位十六进制 cert-fingerprint）
/opt/jaryn-remote/bin/jr-signal --private \
  --data /var/lib/jaryn-remote/signal \
  --bind 0.0.0.0:47210 \
  --public-host 203.0.113.10 \
  --reflex-port 47213

# 启动无状态密文 UDP 中继
/opt/jaryn-remote/bin/jr-relay --bind 0.0.0.0:47211
```

防火墙需放行：

- `47210/TCP`：信令 TLS 连接
- `47211/UDP`：媒体密文中继
- `47213/UDP`：NAT 反射地址探测

服务器证书私钥与 SQLite 数据库保存在 `/var/lib/jaryn-remote/signal`（`UMask=0077`），切勿打包进客户端。客户端按首次钉扎的证书指纹严格校验服务器身份，不会在证书变更时自动信任新证书。

## 2. 客户端准备

可以任选以下两种方式连接封闭配对服务器：

1. **通用客户端配置**：直接运行 `jr-node`（或通用构建的 `Jaryn Remote.app`），在首次连接弹窗中填写 `203.0.113.10:47210`、设备名称与服务器 `cert-fingerprint`，将 `fleet-token` 留空（命令行模式下可传 64 个 `0`）。
2. **自用预配置客户端包（macOS）**：在 macOS 上通过编译期参数生成内置自建服务器地址与证书指纹的 `.app` 包（包内仅含公开地址与公钥证书指纹，不含任何私钥或管理员凭据）：

   ```bash
   ./scripts/package-macos.sh --signal 203.0.113.10:47210 --pin <cert-fingerprint>
   ```

## 3. 新设备配对审批流程

1. 所有者 SSH 登录服务器，开启一个 10 分钟有效、最多只允许批准一台设备的配对窗口：

   ```bash
   runuser -u jaryn-remote -- /opt/jaryn-remote/bin/jr-signal \
     --data /var/lib/jaryn-remote/signal --pair-open 600
   ```

2. 启动新设备客户端。客户端在本机生成 Ed25519 身份密钥，验证服务器 TLS 证书指纹后对当前连接的 32 字节随机挑战（`AuthChallenge`）签名，界面显示 **8 位核对码**（未批准前无法查看设备列表或发起会话）。
3. 在服务器查看待审设备并核对设备名、8 位核对码与设备公钥 ID，确认无误后批准：

   ```bash
   runuser -u jaryn-remote -- /opt/jaryn-remote/bin/jr-signal \
     --data /var/lib/jaryn-remote/signal --pair-list

   runuser -u jaryn-remote -- /opt/jaryn-remote/bin/jr-signal \
     --data /var/lib/jaryn-remote/signal --pair-approve <8位核对码>
   ```

   批准后本次配对窗口立即自动关闭。8 位核对码仅用于在服务端定位待审请求，单独截获核对码无法冒充设备私钥。
4. 重新连接客户端。已批准设备后续每次连接均使用本机私钥对新的随机挑战签名，无需再次审批。远控会话本身仍遵守本机确认或被控端签名无人值守票据规则。

## 4. 撤销设备

在服务器列出已注册设备并按完整设备 ID 撤销：

```bash
runuser -u jaryn-remote -- /opt/jaryn-remote/bin/jr-signal \
  --data /var/lib/jaryn-remote/signal --device-list

runuser -u jaryn-remote -- /opt/jaryn-remote/bin/jr-signal \
  --data /var/lib/jaryn-remote/signal --device-revoke <设备ID>
```

撤销后已在线连接会在 1 秒内被强制断开，该公钥后续的任何挑战签名都会被拒绝。封闭配对模式下客户端不开放服务器级「撤销设备」或「轮换令牌」操作；被控端授予的无人值守票据仍可在被控端随时单独撤回。

## 5. 诊断事件与本机日志

- **定长状态诊断（`Diagnostic`）**：已认证节点通过信令 TLS 连接上报 16 字节定长状态事件（仅含事件编号、数值状态位、会话前 4 字节与对端短号，绝不含画面、键鼠、剪贴板、文件或自由文本），单连接限速 120 条/分钟。服务端排障可执行 `journalctl -u jaryn-signal --since '10 minutes ago' --no-pager | grep client-diag`。
- **本机滚动日志**：客户端在数据目录 `logs/` 下保留当前 `client.log` 与最近 4 份历史日志（单文件上限约 5 MiB，权限 `0600`），仅本机当前系统用户可读，不会上传至服务器。
