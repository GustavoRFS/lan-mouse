# Clipboard Text Sync Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Synchronize UTF-8 clipboard text on Windows and Linux between all directly connected authenticated Lan Mouse peers.

**Architecture:** The GTK frontend watches and updates the desktop clipboard and sends bounded text through local IPC. The daemon fans updates across its live outgoing and incoming DTLS sessions using version-negotiated, chunked clipboard frames separate from the fixed mouse protocol, and returns received updates to the frontend without relaying them.

**Tech Stack:** Rust workspace, GTK4/GDK4, serde JSON local IPC, existing Tokio/DTLS networking.

**Spec:** `docs/clipboard-sharing.md`

## Global Constraints

- Support Windows and Linux only in this stage.
- Share plain UTF-8 text up to 256 KiB per update.
- Enable sharing by default and persist the visible frontend toggle.
- Send only to directly connected authenticated peers, including incoming and outgoing sessions, regardless of pointer-capture selection.
- Do not relay received updates or send a clipboard snapshot on startup/reconnect.
- Use the existing DTLS sessions and negotiate clipboard support before sending payloads.
- Keep fixed-size `ProtoEvent` mouse/input framing unchanged.

## Review Focus

- An older peer sees a capability probe but never receives unsupported payload frames.
- A duplicate incoming/outgoing session does not create repeated clipboard writes after update-ID deduplication.
- A disabled frontend neither sends local changes nor applies remote changes, including updates already in flight.
- A remote clipboard write does not echo back or fan out from this host.
- Oversized, malformed, incomplete, or timed-out text transfers do not allocate unbounded memory or alter the clipboard.

---

### Task 1: Clipboard wire format and compatibility

**Files:**
- Modify: `lan-mouse-proto/src/lib.rs`
- Modify: `lan-mouse-proto/Cargo.toml`
- Modify: root `Cargo.toml` for the UUID generator
- Modify: root and dependent crate manifests to bump internal protocol crates to `0.4.0`

**Interfaces:**
- Produces `ClipboardUpdateId = [u8; 16]`, `ClipboardFrame`, `encode(&self) -> Result<Vec<u8>, ProtocolError>`, `decode(&[u8]) -> Result<ClipboardFrame, ProtocolError>`, and `chunk_text(update_id, text) -> Result<Vec<ClipboardFrame>, ProtocolError>`; chunk frames carry chunk index/count, total byte count, SHA-256 checksum, and payload.
- Existing `ProtoEvent` encode/decode behavior remains byte-for-byte compatible.

- [x] Bump `lan-mouse-proto` to `0.4.0` and update its workspace consumers; add `sha2` for SHA-256 checksums and root dependency `uuid` with `v4` for update IDs.
- [x] Add capability and text-frame types with `MAX_CLIPBOARD_FRAME_SIZE = 1200` bytes and `MAX_CLIPBOARD_TEXT_BYTES = 256 * 1024`.
- [x] Implement chunking/reassembly metadata and SHA-256 validation; cap reassembly at 64 incomplete updates and 4 MiB globally, and expire incomplete updates after 30 seconds.
- [x] Implement per-chunk acknowledgements with a window of eight in flight, 250 ms timeout, and three retries; use random origin bytes plus a per-process sequence for `ClipboardUpdateId` and cancel an older pending local update when a newer local update starts.
- [x] Keep legacy fixed-size `ProtoEvent` parsing available alongside clipboard-frame decoding.
- [ ] Review all parser branches for truncated payloads, invalid UTF-8, zero/oversized chunk counts, out-of-range indices, and checksum failures.

### Task 2: Negotiate and route clipboard across peer sessions

**Files:**
- Modify: `src/connect.rs`
- Modify: `src/listen.rs`
- Modify: `src/capture.rs`
- Modify: `src/emulation.rs`
- Modify: `src/service.rs`

**Interfaces:**
- Outgoing and incoming receive loops surface fully reassembled clipboard updates with the peer address/session identity.
- Outgoing and incoming network owners expose best-effort clipboard broadcast to every live session that advertised support; clipboard receive events do not enter the mouse capture/emulation path.

- [x] Exchange the capability frame after each DTLS connection is established and retain support state per session.
- [x] Route clipboard updates from the service to every currently connected outgoing session and accepted incoming authorized session, without opening new connections.
- [x] Route incoming clipboard updates to the service/frontend and do not rebroadcast them.
- [x] Deduplicate update IDs across duplicate sessions and isolate send/retry errors per peer.
- [x] Ensure pointer `active_client` filtering does not drop clipboard receive events.

### Task 3: Persist preference and connect GTK clipboard to IPC

**Files:**
- Modify: `src/config.rs`
- Modify: `src/service.rs`
- Modify: `lan-mouse-ipc/src/lib.rs`
- Modify: `lan-mouse-ipc/Cargo.toml`
- Modify: `lan-mouse-gtk/Cargo.toml`
- Modify: `lan-mouse-gtk/src/lib.rs`
- Modify: `lan-mouse-gtk/src/window.rs`
- Modify: `lan-mouse-gtk/src/window/imp.rs`
- Modify: `lan-mouse-gtk/resources/window.ui`
- Create: `lan-mouse-gtk/src/clipboard.rs`

**Interfaces:**
- IPC carries `SetClipboardSharing(bool)`, `ClipboardSharing(bool)`, `ClipboardText { text }` request, and `ClipboardUpdate { update_id: [u8; 16], text }` event.
- `clipboard.rs` watches the default GDK clipboard while enabled and applies incoming updates without sending them back.

- [x] Bump `lan-mouse-ipc` to `0.4.0` and update workspace consumers; add an optional persisted configuration value whose default is enabled, expose it through service IPC, and save toggle changes.
- [x] Add a visible “Share clipboard” switch to the GTK frontend and initialize it from the service state.
- [x] Monitor GDK clipboard changes on Windows and Linux; read plain text asynchronously, exclude `text/uri-list`, suppress remote-applied values, and enforce the 256 KiB UTF-8 limit.
- [x] Apply incoming text only while the preference is enabled; suppress the resulting GDK `changed` event so it is not retransmitted.
- [x] Make sharing changes take effect immediately and ignore stale clipboard reads after disable.

### Task 4: Documentation and implementation review

**Files:**
- Modify: `README.md`
- Modify: `docs/clipboard-sharing.md`

- [x] Mark text sharing as implemented and describe the enabled-by-default toggle, peer scope, 256 KiB limit, and current image/file follow-up stages.
- [x] Review the resulting diff for Windows/Linux cfg coverage, peer direction coverage, protocol compatibility, bounded allocations, and no accidental clipboard relay.
- [ ] Inspect the final diff and record build/runtime verification limits clearly if target platform dependencies prevent a local run.
