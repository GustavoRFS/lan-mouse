# Clipboard image sharing implementation plan

> **For agentic workers:** Implement this plan task by task in the isolated `feat/clipboard-image` worktree. The current session instruction permits builds and manual diagnosis but does not request adding or running tests.

**Goal:** Copy an image between connected Windows and Linux peers as a native clipboard image.

**Architecture:** GTK reads or publishes PNG images. The daemon sends bounded image chunks over existing authenticated DTLS sessions after image capability negotiation. The existing text path remains intact.

**Tech Stack:** Rust, GTK4/GDK4, Tokio, DTLS, Serde JSON IPC, SHA-256, Wayland `wl-paste`.

**Spec:** `docs/superpowers/specs/2026-10-21-clipboard-image-design.md`

## Global constraints

- PNG payloads are limited to 8 MiB; reassembly is limited to 32 MiB globally.
- Send only to peers that advertise image support and are currently connected and authenticated.
- Keep image data out of logs; preserve text sharing and the existing Hyprland configuration.
- Do not add or run tests in this session; build and manually inspect behavior.

## Review focus

- A copied file with an image extension stays a file item and is not sent as an image.
- A copied image that also advertises text is sent as an image once.
- Older peers ignore the image capability frame and continue sharing text.
- Oversized or malformed PNG bytes never replace the receiver's clipboard.
- A received image does not echo back to the sender.

---

### Task 1: Protocol image frames

**Files:** `lan-mouse-proto/src/lib.rs`, `src/clipboard.rs`.

**Interfaces:** Add image capability frames, `ImageChunk` frames, PNG chunking and typed reassembly results. Preserve existing text frame IDs and behavior. `ClipboardPeer` exposes `send_image(update_id, png)` and handles image completion.

- [ ] Add image frame kinds and encoded size/shape checks, with an 8 MiB per-image limit.
- [ ] Extend the reassembler to accept image chunks with separate type metadata, a 32 MiB global budget, and SHA-256 validation.
- [ ] Add image capability exchange and reliable sending in `ClipboardPeer`.
- [ ] Build the protocol and daemon crates; inspect the diff for text compatibility.
- [ ] Commit the protocol task.

### Task 2: Daemon and local IPC

**Files:** `lan-mouse-ipc/src/lib.rs`, `src/service.rs`, `src/connect.rs`, `src/listen.rs`, `src/capture.rs`, `src/emulation.rs`.

**Interfaces:** Add local PNG payload request/event variants with update ID, and fan images to all live image-capable peers. Incoming images reach the frontend without peer relay.

- [ ] Add size-checked base64 PNG IPC variants.
- [ ] Route local image updates through outgoing and incoming connected peer sets.
- [ ] Route received images back to the GTK frontend and stop/cancel them when sharing is disabled.
- [ ] Build the daemon and IPC crates; inspect all transport paths.
- [ ] Commit the daemon and IPC task.

### Task 3: GTK image clipboard

**Files:** `lan-mouse-gtk/src/clipboard.rs`, `lan-mouse-gtk/src/lib.rs`, `lan-mouse-gtk/Cargo.toml`.

**Interfaces:** Read image bytes from GTK or Wayland, send bounded PNG through IPC, and publish received PNG with `GdkClipboard.set_texture()`.

- [ ] Classify file, image, and text MIME offers in that order; reserve file offers for the next stage.
- [ ] Read images via GDK on Windows and the existing Wayland watcher on wlroots desktops.
- [ ] Encode to PNG, enforce the 8 MiB limit, and send through IPC.
- [ ] Decode and publish received PNG, suppressing echo across GTK and Wayland notifications.
- [ ] Build the full Linux release binary and inspect the Windows-specific code paths.
- [ ] Commit the GTK task.

### Task 4: Documentation and deployment

**Files:** `README.md`, `docs/clipboard-sharing.md`.

- [ ] Document native image copy/paste, limits, compatibility, and Wayland dependency.
- [ ] Build and install the Linux release binary without editing `~/.config/lan-mouse/config.toml`.
- [ ] Verify local image handling and, once a Windows build is available, confirm remote image delivery.
- [ ] Push the reviewed branch to the private fork.
