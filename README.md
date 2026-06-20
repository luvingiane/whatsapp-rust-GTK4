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

## ✨ Roadmap & status

This is a roadmap, not a finished feature list — it moves fast.

**✅ Working today**

- 📱 QR login, persistent across restarts (reconnects without re-scanning).
- 💬 Chat list + conversations: message history (with scroll-up backfill) and live
  messages, proper sent/received bubbles, profile-picture avatars, chat search.
- ⌨️ Send & receive **text** in real time, with clickable links.
- 🎤 **Voice notes**: record (with a live waveform + timer), send, receive, and play
  back with a waveform, progress bar and elapsed time.
- ✔️ **Delivery / read ticks** (✓ sent, ✓✓ delivered, blue ✓✓ read) — in bubbles and as
  a prefix in the chat-list preview.
- 🟢 **Presence**: reports online only while the window is focused/active and "away"
  when unfocused or idle, so your phone resumes its own notifications when you step away.
- 🗂️ **Archived chats** view with an unread count; read chats clear their unread badge
  (incl. reads done on the phone); duplicate `@lid`/PN chats are merged.

**🚧 Not there yet**

- 🖼️ Other media — photos, videos, GIFs, stickers, documents (send & receive). Only
  voice notes are wired so far.
- ⌨️ "Typing…" indicators, reactions, replies/quotes, group management.

**⚠️ Known limitations**

- **Archived accuracy** depends on resolving WhatsApp's LID↔phone-number identities.
  That mapping fills in over time (and as you open chats); the bulk server lookup is
  rate-limited, so a few archived chats can linger in the main list until their pair is
  learned.
- 🪶 Goal throughout: stay light and native (one Rust process, no Chromium).

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

You'll need a Rust toolchain, GTK4 + libadwaita dev packages, and GStreamer (with the
base/good plugins, for recording & playing voice notes). On **Arch/Manjaro**:

```bash
sudo pacman -S --needed rust gtk4 libadwaita pkgconf sqlite \
    gstreamer gst-plugins-base gst-plugins-good
cargo run
```

(On Debian/Ubuntu the equivalents are `libgtk-4-dev libadwaita-1-dev libsqlite3-dev
libgstreamer1.0-dev gstreamer1.0-plugins-base gstreamer1.0-plugins-good`. More docs
coming as the project matures.)

## 🧹 Uninstall

There's nothing system-wide to remove — it's a plain Cargo build — but it does keep
local state. Two things to clean up:

1. **The app itself**: delete the cloned repo (and run `cargo clean` first if you want to
   drop the `target/` build cache). No files are installed outside the project folder.
2. **Your data & login (the databases)**: all session + chat state lives under
   `~/.local/share/whatsapp-rust-gtk4/` (the `whatsapp.db` session and `app.db` chat
   store) plus a cache in `~/.cache/whatsapp-rust-gtk4/` (avatars, downloaded voice
   notes). Remove them with:

   ```bash
   rm -rf ~/.local/share/whatsapp-rust-gtk4 ~/.cache/whatsapp-rust-gtk4
   ```

   ⚠️ Deleting `whatsapp.db` **unlinks this device** from your phone — you'll have to
   scan the QR code again next time, and WhatsApp keeps only a handful of linked devices.

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
