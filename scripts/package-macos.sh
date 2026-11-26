#!/bin/bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

if [[ "$(uname -s)" != "Darwin" ]]; then
    echo "此打包脚本只能在 macOS 上运行" >&2
    exit 1
fi

arch="$(uname -m)"
bundle_signal="${JARYN_BUNDLE_SIGNAL:-}"
bundle_pin="${JARYN_BUNDLE_PIN:-}"
bundle_reflex_port="${JARYN_BUNDLE_REFLEX_PORT:-}"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --signal)
            bundle_signal="${2:?--signal 需要 HOST:PORT}"
            shift 2
            ;;
        --pin)
            bundle_pin="${2:?--pin 需要 64 位十六进制证书指纹}"
            shift 2
            ;;
        --reflex-port)
            bundle_reflex_port="${2:?--reflex-port 需要端口号}"
            shift 2
            ;;
        -h|--help)
            cat <<'USAGE'
用法: ./scripts/package-macos.sh [--signal HOST:PORT --pin SHA256_HEX] [--reflex-port PORT]

不带参数时构建通用客户端（首次启动弹窗填写信令地址、令牌与证书指纹）。
传入 --signal 与 --pin 时构建预置封闭配对服务器配置的自用客户端。
USAGE
            exit 0
            ;;
        *)
            echo "未知参数: $1" >&2
            exit 2
            ;;
    esac
done

if [[ -n "$bundle_signal" || -n "$bundle_pin" ]]; then
    if [[ -z "$bundle_signal" || -z "$bundle_pin" ]]; then
        echo "--signal 与 --pin 必须同时提供，或同时省略" >&2
        exit 2
    fi
    export JARYN_BUNDLE_SIGNAL="$bundle_signal"
    export JARYN_BUNDLE_PIN="$bundle_pin"
    if [[ -n "$bundle_reflex_port" ]]; then
        export JARYN_BUNDLE_REFLEX_PORT="$bundle_reflex_port"
    fi
fi

cargo build --release -p jr-node

dist="$root/target/dist"
stage="$dist/JarynRemote-macOS-$arch"
app="$stage/Jaryn Remote.app"
rm -rf "$stage"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$root/target/release/jr-node" "$app/Contents/MacOS/jr-node"
cp "$root/assets/JarynRemote.icns" "$app/Contents/Resources/AppIcon.icns"

cat > "$app/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key><string>zh_CN</string>
  <key>CFBundleDisplayName</key><string>Jaryn Remote</string>
  <key>CFBundleExecutable</key><string>jr-node</string>
  <key>CFBundleIdentifier</key><string>com.jaryn.remote.client</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
  <key>CFBundleName</key><string>Jaryn Remote</string>
  <key>CFBundleIconFile</key><string>AppIcon.icns</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>0.1.0</string>
  <key>CFBundleVersion</key><string>1</string>
  <key>LSMinimumSystemVersion</key><string>12.3</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST

cat > "$stage/START-HERE.md" <<'README'
# Jaryn Remote macOS 客户端

适用于 macOS 12.3 或更新版本。双击 `Jaryn Remote.app` 启动客户端：

- **通用构建**：首次打开会提示输入自建信令地址（`IP:47210`）、设备名称与服务器 TLS 证书指纹（若服务器未开启 `--private`，还需填写 `fleet-token`）。
- **预配置封闭配对构建**：若构建时传入了 `--signal` 与 `--pin`，客户端直接连接该服务器并校验内置证书指纹；首次加入需管理员在信令服务器执行 `--pair-open 600` 并通过 `--pair-approve <8位核对码>` 批准，详见仓库 `docs/09-private-pairing.md`。

设备私钥与本机滚动日志保存在 `~/Library/Application Support/Jaryn Remote/` 下，权限仅当前系统用户可读写，请勿对外分享该目录。

界面快捷键：`⌘F` 搜索、`⌘R` 刷新、`⌘L` 查看日志、`⌘.` 断开会话、`⌘Q` 退出。「屏幕权限」可引导申请录屏授权；辅助功能权限需在「系统设置 → 隐私与安全性 → 辅助功能」中授予后重开客户端。

本地构建默认使用临时代码签名（ad-hoc signing），未经过 Apple Developer 公证；跨机器分发时若遇 Gatekeeper 拦截，请在系统隐私与安全性设置中手动放行或使用自有开发者证书签名。
README

plutil -lint "$app/Contents/Info.plist"
codesign --force --deep --sign - --timestamp=none "$app"
codesign --verify --deep --strict --verbose=2 "$app"
archive="$dist/JarynRemote-macOS-$arch.zip"
rm -f "$archive"
ditto -c -k --norsrc --noextattr --noqtn --noacl --keepParent "$stage" "$archive"
echo "$archive"
