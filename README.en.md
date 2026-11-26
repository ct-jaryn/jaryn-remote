<p align="center">
  <img src="assets/jaryn-remote-logo.svg" width="96" height="96" alt="Jaryn Remote Logo">
</p>

<h1 align="center">Jaryn Remote</h1>

<p align="center">
  <strong>Self-hosted, end-to-end encrypted remote desktop, terminal &amp; file transfer for a personal device fleet</strong><br>
  <sub>Clean-Room JRP/1 UDP Protocol · Native GPU Zero-Copy Pipeline · Zero-Trust Ciphertext Relay · Single-Binary Rust</sub>
</p>

<p align="center">
  <a href="https://github.com/ct-jaryn/jaryn-remote/actions/workflows/ci.yml"><img src="https://github.com/ct-jaryn/jaryn-remote/actions/workflows/ci.yml/badge.svg" alt="CI Status"></a>
  <a href="https://www.rust-lang.org/"><img src="https://img.shields.io/badge/Rust-2021_Edition-DEA584?logo=rust&logoColor=white" alt="Rust 2021"></a>
  <a href="docs/03-protocol.md"><img src="https://img.shields.io/badge/Protocol-JRP%2F1_(UDP%2FARQ%2FFEC)-0A8FBA" alt="Protocol JRP/1"></a>
  <a href="#license"><img src="https://img.shields.io/badge/License-MIT_OR_Apache--2.0-2ea44f" alt="License: MIT OR Apache-2.0"></a>
  <a href="https://github.com/ct-jaryn/jaryn-remote/stargazers"><img src="https://img.shields.io/github/stars/ct-jaryn/jaryn-remote?style=social" alt="GitHub Stars"></a>
</p>

<p align="center">
  <a href="#why-jaryn-remote">Why Jaryn Remote</a> ·
  <a href="#engineering-highlights">Highlights</a> ·
  <a href="#architecture">Architecture</a> ·
  <a href="#quick-start">Quick Start</a> ·
  <a href="#self-hosting">Self-Hosting</a> ·
  <a href="#community--feedback">Feedback</a> ·
  <a href="README.md"><strong>简体中文</strong></a>
</p>

---

> **Enjoying the project? Please consider giving it a Star ⭐ on GitHub!**  
> Questions, bug reports, hardware test results, and architectural discussions are always welcome — feel free to open an [Issue](https://github.com/ct-jaryn/jaryn-remote/issues) or [Pull Request](https://github.com/ct-jaryn/jaryn-remote/pulls) anytime!

---

## Why Jaryn Remote?

Most personal remote-control workflows boil down to one goal: **controlling your own paired machines from whichever device is in front of you — with crisp text, responsive input, quick file transfer, and an interactive terminal — without handing trust to a third-party vendor.**

**Jaryn Remote** is built from scratch in Rust around strict, auditable boundaries:

- **Zero third-party remote stacks**: Does not embed, wrap, or interoperate with RustDesk, Sunshine/Moonlight, ToDesk, scrcpy, or WebRTC. Every protocol frame, reliability window, FEC group, congestion loop, and hole-punching state machine is clean-room Rust.
- **Tuned for desktop latency (`15–30ms` LAN budget)**: Sub-1280-byte UDP datagrams avoid IP fragmentation; real-time video/audio bypass head-of-line blocking while control, clipboard, cursor shapes, files, and PTY streams run over isolated 64-packet selective-ACK ARQ channels.
- **Minimal server trust**: You run two tiny binaries (`jr-signal` and `jr-relay`). Media and control payloads are end-to-end encrypted with ephemeral X25519 + Ed25519 + AES-256-GCM, and unattended grants (`JRP-GRANT1`) are signed locally by the host's Ed25519 key — **even a compromised signaling server cannot view frames or forge unattended access.**

<p align="center">
  <img src="assets/client-ui-v2.png" width="940" alt="Jaryn Remote Native Link Deck UI">
  <br>
  <sub>macOS native AppKit "Link Deck" workspace, viewer window, and honest software privacy overlay</sub>
</p>

---

## Engineering Highlights

- **Native OS Capture & Hardware Codecs**: Drives `DXGI Desktop Duplication` + `D3D11 VideoProcessor` GPU zero-copy (`BGRA → NV12`) + `Media Foundation` H.264/AAC on Windows, and `ScreenCaptureKit` + `VideoToolbox` + `AVAudioConverter` on macOS.
- **Decoupled 250Hz Cursor Channel**: Cursor coordinates and BGRA cursor shapes travel on dedicated channels and composite locally at the viewer — pointer movement stays smooth even under heavy video bitrate throttling.
- **Single-Binary Native Desktop Node**: One `jr-node` binary serves as both controller and host using native Cocoa/AppKit and Win32 windows — zero Electron or WebView overhead.
- **Closed Pairing (`--private`) Mode**: Production self-hosted mode eliminates shared registration tokens in favor of per-connection 32-byte TLS challenge signatures and time-boxed, single-use 8-character SSH approval windows.
- **Transparent & Fail-Safe by Design**: Mandatory active-session indicators, manual-only clipboard send (never background-snooped), automatic key-release timeouts, and crash-safe recovery of software privacy overlays.

---

## Architecture

```mermaid
flowchart LR
    subgraph Fleet["Owner's Personal Device Fleet (End-to-End Encrypted)"]
        C["Controller Node (jr-node)"]
        H["Host Node (jr-node)"]
    end

    subgraph SelfHosted["Self-Hosted Infrastructure (Zero Plaintext Access)"]
        S["Signaling Server (jr-signal · TCP/TLS 47210 + Reflex UDP 47213)"]
        R["Ciphertext Relay (jr-relay · Stateless UDP 47211)"]
    end

    C <-->|"Presence / Invites / Signed Ephemeral Keys"| S
    H <-->|"Heartbeat / Closed Pairing / Audit Metadata"| S
    C <==>|"1. LAN Beacon Direct (UDP 47212)  2. Simultaneous UDP Hole Punching"| H
    C -.->|"3. Automatic Fallback (AES-256-GCM Ciphertext Only)"| R
    R -.->|"Stateless Forwarding"| H
```

### Workspace Crates & Binaries

| Path | Kind | Responsibility |
| --- | --- | --- |
| `crates/jr-proto` | Library | `JRP/1` wire codecs, constants, and 64-packet replay window (`#![forbid(unsafe_code)]`, zero syscalls) |
| `crates/jr-crypto` | Library | `ring`-backed Ed25519 identity, ephemeral X25519 handshake, HKDF-SHA256, AES-256-GCM, `JRP-GRANT1` grants |
| `crates/jr-transport` | Library | UDP packetizer, per-stream selective ARQ, adaptive XOR FEC, JCC congestion control |
| `crates/jr-session` | Library | End-to-end session engine (video/audio slicing, input aging, SHA-256 atomic file transfer, clipboard, PTY) |
| `crates/jr-input` | Library | Cross-platform HID keycode mapping, multi-monitor origins, Retina physical-pixel transforms |
| `crates/jr-host-win` | Platform | Windows native adapter (DXGI + D3D11 zero-copy, MF H.264/AAC, SendInput & touch, ConPTY, Win32 UI & tray) |
| `apps/node` | Binary | Unified desktop & CLI node (includes macOS Objective-C ScreenCaptureKit/VideoToolbox/AppKit bridge) |
| `apps/signal` | Binary | Self-hosted TLS signaling server (presence, closed pairing, audit, LAN WoL forwarding) |
| `apps/relay` | Binary | Stateless ciphertext-only UDP relay with per-session bandwidth cap |

---

## Platform Status

We maintain strict engineering honesty between **automated CI verification** and **physical dual-machine hardware sign-off**:

| Platform | Role | Status |
| --- | --- | --- |
| **Windows** | Controller + Host | `jr-host-win` (DXGI + D3D11 zero-copy, MF H.264/AAC, SendInput + touch, ConPTY, Win32 device/viewer windows, tray, software cover) implemented; **full workspace unit & integration suite (including ConPTY resize & 8 MiB transfer) passing on Windows MSVC CI**; physical two-PC screen/audio interactive sign-off pending |
| **macOS** | Controller + Host | Native Link Deck UI, viewer window, ScreenCaptureKit video/audio, VideoToolbox H.264, input injection, software cover, and `.app` bundler implemented & passing macOS CI; physical two-Mac authorized screen/audio sign-off pending |
| **Linux** | Host (PTY + files) | Headless PTY terminal & SHA-256 file transfer passing Ubuntu CI; Wayland/X11 portal desktop capture planned (`TASKS.md` U03) |
| **Android / iOS** | Planned | Protocol capability bits reserved; mobile clients planned (`TASKS.md` U01–U02; iOS is controller-only by design) |

---

## Quick Start

### Local Three-Terminal Demo

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

---

## Self-Hosting

Open `47210/TCP` (TLS signaling), `47211/UDP` (ciphertext relay), and `47213/UDP` (NAT address reflection) on your server (RFC 5737 example IP `203.0.113.10`):

```bash
cargo run --release -p jr-signal -- --private \
  --data /var/lib/jaryn-remote/signal \
  --bind 0.0.0.0:47210 \
  --public-host 203.0.113.10 \
  --reflex-port 47213

cargo run --release -p jr-relay -- --bind 0.0.0.0:47211
```

Package a macOS `.app` bundle on macOS 12.3+:

```bash
# Generic interactive app (prompts for server & fingerprint on first launch)
./scripts/package-macos.sh

# Preconfigured app bound to your closed-pairing server
./scripts/package-macos.sh --signal 203.0.113.10:47210 --pin <64-char-sha256-pin>
```

See [deploy/](deploy/) for `systemd` service units and [docs/01-product.md](docs/01-product.md)–[docs/11-node-refactor.md](docs/11-node-refactor.md) for the complete architecture and protocol specifications (in Chinese).

---

## Community & Feedback

- ⭐ **Star the Repository**: If you appreciate clean-room Rust networking, transparent security boundaries, or self-hosted tooling, giving the repo a **Star** helps a ton!
- 💬 **Questions, Bugs & Real-Hardware Feedback**: Hit a build issue, tested on real Windows/macOS hardware, or want to suggest an improvement? Open an **[Issue](https://github.com/ct-jaryn/jaryn-remote/issues)** anytime — all feedback is welcome!
- 🛠️ **Contribute**: Check out [CONTRIBUTING.md](CONTRIBUTING.md) and [TASKS.md](TASKS.md) for open engineering milestones and PR guidelines.
- 🔒 **Security**: Please report security vulnerabilities privately via [SECURITY.md](SECURITY.md).

---

## License

Dual-licensed under either of:

- **MIT License** ([LICENSE-MIT](LICENSE-MIT))
- **Apache License, Version 2.0** ([LICENSE-APACHE](LICENSE-APACHE))

at your option.
