<p align="center">
  <img src="assets/jaryn-remote-logo.svg" width="88" height="88" alt="Jaryn Remote Logo">
</p>

<h1 align="center">Jaryn Remote</h1>

<p align="center">
  Self-hosted remote desktop, terminal, and file transfer for a single owner's personal device fleet.<br>
  Clean-room protocol, session engine, transport, capture scheduling, and input injection in Rust.
</p>

<p align="center">
  <a href="https://github.com/ct-jaryn/jaryn-remote/actions/workflows/ci.yml"><img src="https://github.com/ct-jaryn/jaryn-remote/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="#license"><img src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg" alt="License: MIT OR Apache-2.0"></a>
  <a href="README.md">中文文档</a>
</p>

---

## Overview

**Jaryn Remote** lets one owner control their own paired devices across LAN and WAN without depending on third-party remote-desktop vendors, accounts, or WebRTC stacks.

- **Clean-room `JRP/1` protocol & transport**: Custom UDP wire format with per-stream ARQ reliability, XOR forward error correction (FEC), delay/loss-aware JCC congestion control, independent cursor position/shape channels, LAN multicast beacons, simultaneous UDP hole punching, and fallback to a stateless ciphertext-only UDP relay.
- **Native OS capture & hardware codecs**: Directly drives OS capture and GPU codecs — DXGI Desktop Duplication + D3D11 zero-copy BGRA→NV12 + Media Foundation H.264/AAC on Windows, and ScreenCaptureKit + VideoToolbox + AVAudioConverter on macOS.
- **End-to-end encrypted & minimal server trust**: Uses `ring` for Ed25519 device identities, ephemeral X25519 key agreement, HKDF-SHA256, and AES-256-GCM. Neither `jr-signal` nor `jr-relay` can decrypt media, input, files, clipboard, or terminal streams, nor forge host-signed unattended grants (`JRP-GRANT1`).
- **Single-binary native desktop node**: One `jr-node` binary acts as both controller and host with native Cocoa/AppKit (macOS) and Win32 (Windows) windows — zero Electron or WebView overhead.

---

## Workspace Layout

- `crates/jr-proto` — `JRP/1` wire codecs, constants, and replay window (`#![forbid(unsafe_code)]`, zero syscalls)
- `crates/jr-crypto` — Ed25519 identity, ephemeral X25519 handshake, AES-256-GCM, signed unattended grants
- `crates/jr-transport` — UDP packetizer, multi-stream ARQ, XOR FEC, JCC congestion control
- `crates/jr-session` — Session state machine (video/audio, input, SHA-256 verified file transfer, clipboard, PTY terminal)
- `crates/jr-input` — Cross-platform HID keycode tables and physical-pixel / Retina coordinate transforms
- `crates/jr-host-win` — Windows native adapter (DXGI capture, D3D11 zero-copy, MF H.264/AAC, SendInput & touch, ConPTY, Win32 windows & tray)
- `apps/node` — Desktop & CLI node (includes macOS Objective-C ScreenCaptureKit/VideoToolbox/AppKit bridge)
- `apps/signal` — Self-hosted TLS signaling server (presence, invites, closed pairing, audit, WoL forwarding)
- `apps/relay` — Stateless ciphertext-only UDP relay

---

## Platform Status

We strictly separate automated compile/unit/integration verification from physical dual-machine hardware sign-off:

| Platform | Role | Status |
| --- | --- | --- |
| **macOS** | Controller + Host | Native Link Deck UI, viewer window, ScreenCaptureKit video/audio, VideoToolbox H.264, input injection, software privacy cover, and WoL implemented & tested in workspace CI; physical two-Mac hardware verification pending |
| **Windows** | Controller + Host | `jr-host-win` (DXGI + D3D11 GPU zero-copy, MF H.264/AAC, SendInput + touch, ConPTY, Win32 device/viewer windows, tray, software privacy cover) implemented & unit-tested; physical Windows desktop verification pending |
| **Linux** | Host (headless PTY + files) | Headless PTY terminal & file transfer supported; Wayland/X11 desktop capture planned (`TASKS.md` U03) |
| **Android / iOS** | Planned | Protocol capability bits reserved; mobile clients not yet implemented (`TASKS.md` U01–U02; iOS is controller-only by design) |

---

## Quick Start

### Local Two-Node Demo

```bash
# Terminal 1: Start signaling server (prints one-time fleet-token and cert-fingerprint)
cargo run -p jr-signal -- --data ./signal-data

# Terminal 2: Start controller node with native UI
cargo run -p jr-node -- --signal 127.0.0.1:47210 --data ./node-a --name alpha \
  --token <fleet-token> --pin <cert-fingerprint> --ui

# Terminal 3: Start host node
cargo run -p jr-node -- --signal 127.0.0.1:47210 --data ./node-b --name beta --accept \
  --token <fleet-token> --pin <cert-fingerprint>
```

### Self-Hosted WAN Deployment

Open `47210/TCP` (TLS signaling), `47211/UDP` (ciphertext relay), and `47213/UDP` (NAT address reflection) on your server (example IP `203.0.113.10`):

```bash
cargo run --release -p jr-signal -- --private --data /var/lib/jaryn-remote/signal \
  --bind 0.0.0.0:47210 --public-host 203.0.113.10 --reflex-port 47213

cargo run --release -p jr-relay -- --bind 0.0.0.0:47211
```

See [deploy/](deploy/) for `systemd` units and [docs/09-private-pairing.md](docs/09-private-pairing.md) for SSH closed-pairing approval (`--pair-open`, `--pair-list`, `--pair-approve`).

---

## Explicit Non-Goals

- Never bypasses OS permission prompts, hides active-session indicators, or runs stealth/anti-detection tricks.
- Never silently monitors background clipboard changes or records screen/keystroke content in audit logs.
- No public stranger-support share codes, multi-tenant billing, or third-party protocol compatibility.

---

## Contributing & Security

- Please read [CONTRIBUTING.md](CONTRIBUTING.md) before opening a pull request.
- Report security vulnerabilities privately via [SECURITY.md](SECURITY.md).

## License

Dual-licensed under either of:

- **MIT License** ([LICENSE-MIT](LICENSE-MIT))
- **Apache License, Version 2.0** ([LICENSE-APACHE](LICENSE-APACHE))

at your option.
