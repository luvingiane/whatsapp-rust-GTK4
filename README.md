# WhatsApp Rust GTK4 🦀💬

> A **native, lightweight WhatsApp client for Linux** — built in pure **Rust** with
> **GTK4 + libadwaita**, so it looks and feels like a real GNOME app.
>
> 🍳 **Status: early development — we're cooking.** Things will change fast and break often.

---

## 🤔 What is this & why?

Most desktop WhatsApp clients on Linux are **Electron/Chromium wrappers**: heavy, slow,
hundreds of MB of RAM to show some chat bubbles. This project is an experiment in doing it
the *native* way:

- **One single Rust process** — no Chromium, no Node sidecar.
- **GTK4 + libadwaita** — 100% GNOME-native look, like GNOME Chats or Fractal.
- **Light on RAM**, fast to start.

It talks to WhatsApp through the [`whatsapp-rust`](https://github.com/jlucaso1/whatsapp-rust)
crate (an independent Rust implementation of the WhatsApp Web protocol).

## ✨ What it should do (eventually)

Roughly, and not all there yet — this is a roadmap, not a feature list:

- 📱 Log in by scanning a QR code, and stay logged in across restarts ✅ *(done)*
- 💬 Chat list + conversations with proper sent/received bubbles
- ⌨️ Send & receive text messages in real time
- 🖼️ **Media**: photos, videos, GIFs, stickers, voice notes, documents — send & receive
- 👀 Typing / delivery / read indicators
- 🪶 All of the above while staying light and native

## 🤖 Heads-up: this is "vibecoded"

Honesty first: this project is being built largely through **AI-assisted "vibecoding"**
(with Claude). That means it gets written fast and iteratively — so expect rough edges,
occasional weird code, and bugs. I'm learning in the open and showing the cooking process,
not shipping a polished product (yet). **Apologies in advance for the jank** 🙏 — patience,
issues, and fixes are all very welcome.

## ⚠️ Important disclaimer

- This is an **unofficial** client. Using it **may violate WhatsApp's Terms of Service and
  could get your account suspended or banned.** Consider using a secondary number.
- WhatsApp/Meta change the protocol periodically; things will break and need maintenance.
- The underlying library is young — expect bugs and breaking changes.

## 🧪 Try it (early adopters only)

You'll need a Rust toolchain plus GTK4 + libadwaita dev packages. On **Arch/Manjaro**:

```bash
sudo pacman -S --needed rust gtk4 libadwaita pkgconf sqlite
cargo run
```

(See the build files for other distros — more docs coming as the project matures.)

## 🙏 Acknowledgements & thanks

This stands entirely on the shoulders of others. Huge thanks to:

- **[whatsapp-rust](https://github.com/jlucaso1/whatsapp-rust)** (oxidezap / jlucaso1) — the
  protocol backend that makes this possible.
- **[whatsmeow](https://github.com/tulir/whatsmeow)** (Go) and
  **[Baileys](https://github.com/WhiskeySockets/Baileys)** (TypeScript) — the reference
  implementations that paved the way.
- The **[gtk4-rs](https://github.com/gtk-rs/gtk4-rs)** and **libadwaita-rs** maintainers,
  and the whole **GNOME** project for the platform and design language.
- The **Rust** community.
- **Claude / Anthropic** — for doing most of the typing 🤖.
- And **you**, for being curious enough to read this far. ❤️

## 📄 License

[GPLv3](LICENSE) — in keeping with the GNOME ecosystem.
