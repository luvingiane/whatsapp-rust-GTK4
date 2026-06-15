# WhatsApp Rust GTK4

A **native WhatsApp desktop client for Linux**, written entirely in **Rust** with
**GTK4 + libadwaita**. It speaks the WhatsApp Web protocol through the
[`whatsapp-rust`](https://github.com/jlucaso1/whatsapp-rust) (oxidezap) crate — no
Electron, no Chromium, no Node sidecar. The goal is a lightweight, 100% GNOME-native
alternative to Electron/CEF wrappers, with low RAM usage and a single Rust process.

> **Status: early development.** This is **Step 1** of an incremental build: project
> setup and **QR-code authentication with persistent sessions**. There is no chat UI
> yet — that comes in the next modules.

---

## ⚠️ Important disclaimers — read before using

- **Unofficial client.** `whatsapp-rust` is an independent, unofficial
  reimplementation of the WhatsApp Web protocol. Using a custom client **may violate
  WhatsApp's Terms of Service and can get your account suspended or banned.** Consider
  testing with a secondary number, not your primary account.
- **Meta breaks the protocol periodically.** When that happens the app stops working
  until `whatsapp-rust` is updated. This project requires ongoing maintenance.
- **`whatsapp-rust` is young (v0.6).** Expect bugs and breaking API changes. This repo
  **pins an exact version** (`=0.6.0`) on purpose; upgrades are deliberate.
- **License: GPLv3** (see [`LICENSE`](LICENSE)), consistent with the GNOME ecosystem.

---

## What works in Step 1

- A libadwaita window that starts the WhatsApp backend on launch.
- **QR-code pairing**: scan it from your phone to link the device. The code auto-refreshes
  on expiry.
- **Persistent session** stored in SQLite (`whatsapp-rust`'s `SqliteStore`, WAL mode).
  After pairing once, **restarting the app reconnects without re-scanning**.
- Status feedback in the UI: waiting → scan QR → connecting → *Connected as `<number>`*.
- Robust logging of disconnects and decryption errors (`Bad MAC` / `No Session`) — these
  are logged, never crash the app.

---

## System dependencies

You need a Rust toolchain plus the GTK4 and libadwaita development packages.

**Arch / Manjaro:**
```bash
sudo pacman -S --needed rust gtk4 libadwaita pkgconf sqlite
```

**Fedora:**
```bash
sudo dnf install rust cargo gtk4-devel libadwaita-devel pkgconf-pkg-config sqlite-devel
```

**Debian / Ubuntu:**
```bash
sudo apt install cargo libgtk-4-dev libadwaita-1-dev pkg-config libsqlite3-dev
```

> A later **media module** will additionally require `ffmpeg` (voice-note transcoding to
> ogg/opus and video thumbnails). It is **not** needed for Step 1.

---

## Build & run

```bash
cargo run                 # debug build
cargo run --release       # optimized build (lower RAM, faster)
```

Increase log verbosity with the standard `RUST_LOG` variable:

```bash
RUST_LOG=debug cargo run
```

### How to pair

1. Launch the app — a window opens and, after a moment, shows a **QR code**.
2. On your phone: **WhatsApp → Settings → Linked Devices → Link a device**, then scan.
3. The window switches to **“Connected as `<your number>`”**.
4. Quit and relaunch: it should connect **without** showing a QR code again.

The session database lives at:
```
~/.local/share/whatsapp-rust-gtk4/whatsapp.db
```
Delete that file to fully reset (you will need to scan again).

---

## Architecture

```
src/
├── main.rs              Entry point: logging + launch the libadwaita app
├── app.rs              Builds the AdwApplication, spawns the backend, bridges events → UI
├── config.rs          App id and the XDG path of the session database
├── backend/           The "Tokio side" — everything that talks to whatsapp-rust
│   ├── bridge.rs        WaEvent / WaCommand enums + the async channels
│   ├── runtime.rs       Dedicated Tokio runtime thread
│   └── client.rs        Builds the whatsapp-rust Bot; maps its Events → WaEvents
├── ui/                The GTK4 / libadwaita interface (hand-written for now)
│   ├── window.rs        Main AdwApplicationWindow
│   └── login.rs         QR / connection status view
└── util/
    └── qr.rs           Renders a QR string to a gdk::Texture
```

### Tokio ↔ GTK bridge (the critical piece)

`whatsapp-rust` is async and delivers events inside callbacks on a **Tokio** runtime,
while GTK requires all UI work on the **GLib main loop**. We never touch widgets from the
backend. Instead:

- A **dedicated OS thread** hosts a multi-threaded Tokio runtime running the WhatsApp
  client (`backend/runtime.rs`).
- Backend → UI events are translated into a small `WaEvent` enum and sent over an
  `async-channel`. The GTK side drains it with `glib::spawn_future_local`, so every UI
  update runs on the main thread (`app.rs`).
- A second `async-channel` carries UI → backend `WaCommand`s (only `Shutdown` so far;
  message-sending lands in a later module).

### State

`whatsapp-rust`'s `SqliteStore` owns only the **session and Signal keys**. The
application-level state (chat list, message history, unread counts) will be a **separate
SQLite database** owned by this app and used as the UI's source of truth — introduced in
upcoming modules.

### Why Cargo (not Meson) for now

Step 1 targets the fastest path to a working, debuggable app. Meson + Blueprint +
Flatpak packaging will be added in a dedicated packaging module once the app is mature.

---

## Roadmap (incremental, one module at a time)

1. ✅ **QR auth + persistent session** (this step)
2. Chat list (`AdwNavigationSplitView`): last message, timestamp, unread badge
3. Open a thread: message history with sent/received bubbles
4. Send text messages
5. Real-time receive (push, no polling)
6. Typing / delivery / read receipts
7. **Media** (non-negotiable): photos, video, GIF, stickers, voice notes (PTT),
   documents — send & receive, with inline previews
8. Packaging: Meson + Blueprint + Flatpak

**Out of scope** (unsupported by the protocol/library): voice & video calls, broadcast
lists, multi-media albums, interactive/button messages, multi-account.
