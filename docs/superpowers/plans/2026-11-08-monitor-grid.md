# Monitor Grid Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Route mouse and keyboard control among individual Windows and Hyprland monitors arranged in a logical grid, using screen-edge crossings without pixel offset matching.

**Architecture:** Add shared monitor/grid types and backward-compatible bounded protocol messages. Make Windows and Hyprland capture report an output ID with the edge; let the service route that edge through the persisted grid; let the receiving emulator place the pointer at the selected output's entry-edge midpoint. Add a GTK grid editor, authenticated topology discovery and versioned grid sync, and document the TOML and compatibility behavior.

**Tech Stack:** Rust workspace, serde/TOML, Tokio, GTK4/Libadwaita, existing DTLS peer connections, Windows display APIs, Wayland layer-shell and virtual pointer.

**Spec:** `docs/superpowers/specs/2026-11-08-monitor-grid-design.md`

## Global Constraints

- Windows and Linux with Hyprland are the supported mixed-OS pair for this change.
- Each display is one equal logical grid cell; cursor entry uses the target display edge midpoint and does not map pixel offsets.
- Local output topology stays as reported by the operating system; remote outputs can be positioned individually.
- Grid sync uses authenticated peers, acknowledgment, retry, reconnect replay, and deterministic resolution of concurrent offline edits.
- Peers without monitor-grid capability keep using the existing four-direction behavior.
- Keep new protocol datagrams bounded and preserve existing event IDs and payloads.
- Update README.md and DOC.md for the new configuration and supported platform behavior.
- Build and test the Windows path on this Windows machine and the Linux/Hyprland path on `gauss`. Do not use `ligma` unless the user explicitly resumes it.

## Review Focus

1. Crossing an edge on one local display selects only the remote display in that grid cell; native transitions between local displays remain local.
2. Different display resolutions still place the remote pointer on the selected output's requested edge midpoint.
3. A disconnected or hot-unplugged output remains visible in saved layout but never routes to a stale destination.
4. An older peer ignores new messages and continues to use the legacy directional path.
5. Lost, reordered, malformed, or oversized catalog/grid chunks cannot corrupt a layout; retries converge on the newest valid revision.

Owning checks: `route_uses_exact_source_monitor_and_edge`, `entry_midpoint_uses_destination_geometry`, `unavailable_outputs_are_not_routed`, `legacy_peer_capability_falls_back_to_position`, and `grid_snapshot_reassembles_and_retries_lost_chunks`.

---

### Preflight

- [ ] Before source changes, run `cargo test -p input-event -p lan-mouse-ipc -p lan-mouse-proto -p input-capture -p input-emulation -p lan-mouse-gtk` on this Windows machine and on `gauss`; record each baseline. If either is already failing, preserve the failure details and distinguish them from this feature's results.
- [ ] Confirm the feature worktree is clean apart from the approved design and this plan.

### Task 1: Shared monitor-grid model and protocol messages

**Files:**
- Create: `input-event/src/display.rs`
- Modify: `input-event/src/lib.rs`
- Create: `lan-mouse-ipc/src/monitor_grid.rs`
- Modify: `lan-mouse-ipc/src/lib.rs`
- Modify: `lan-mouse-ipc/Cargo.toml` with `input-event` default features disabled
- Modify: `lan-mouse-proto/src/lib.rs`

**Interfaces:**
- `input_event::MonitorId(String)` is the opaque output identity, shared by capture, IPC, and network code and scoped by a machine certificate fingerprint.
- `MonitorId::new(value: String) -> Result<MonitorId, MonitorIdError>` rejects empty IDs and IDs over 96 UTF-8 bytes; `MonitorId::as_str(&self) -> &str` exposes the validated value.
- `input_event::MonitorDescriptor { id: MonitorId, label: String, width: u32, height: u32 }` carries an output's display label and current pixel dimensions for the editor and target-edge placement.
- `GridPosition { x: i32, y: i32 }` stores logical cell coordinates.
- `MonitorGridEntry { owner_fingerprint: String, monitor_id: MonitorId, position: GridPosition }` describes one cell.
- `MonitorGrid { revision: u64, origin_fingerprint: String, entries: Vec<MonitorGridEntry> }` validates unique coordinates and unique owner/output pairs.
- `MonitorGrid::neighbor(owner_fingerprint: &str, monitor_id: &MonitorId, edge: Position) -> Option<&MonitorGridEntry>` returns the adjacent cell, if occupied.
- Add backward-compatible protocol events for capability exchange, catalog request/chunks, grid snapshot chunks/acknowledgment, and `EnterMonitor { monitor_id, edge }`. Keep legacy `Enter(Position)` unchanged. Keep each encoded datagram at or below 1200 bytes and reject invalid lengths/counts safely.

- [ ] Add model tests for all four neighbor directions, same-owner versus remote-owner neighbors, duplicate-cell rejection, and empty-cell behavior.
- [ ] Run the model tests on Windows and `gauss`; confirm they fail because the grid model is missing.
- [ ] Add protocol tests for each new event's round trip, chunk ordering metadata, malformed lengths, and legacy event round trips.
- [ ] Run the protocol tests on Windows and `gauss`; confirm they fail because the events are missing.
- [ ] Implement the shared types and protocol event codecs with new event discriminants appended after existing ones.
- [ ] Bound monitor IDs and labels to 96 UTF-8 bytes, and a grid to 256 displays. Keep encoded chunks within 1200 bytes and split larger snapshots into numbered chunks.
- [ ] Run `cargo test -p lan-mouse-ipc -p lan-mouse-proto` on Windows and `gauss`; confirm all existing and new tests pass.
- [ ] Commit as `feat: add monitor grid protocol model`.

### Task 2: Monitor-aware edge capture on Windows and Hyprland

**Files:**
- Modify: `input-capture/src/lib.rs`
- Modify: `input-capture/src/libei.rs`
- Modify: `input-capture/src/macos.rs`
- Modify: `input-capture/src/x11.rs`
- Modify: `input-capture/src/windows.rs`
- Modify: `input-capture/src/windows/event_thread.rs`
- Modify: `input-capture/src/windows/display_util.rs`
- Modify: `input-capture/src/layer_shell.rs`
- Modify: `input-capture/src/dummy.rs`
- Modify: other `input-capture/src/*.rs` implementations as required by the shared `Capture` trait

**Interfaces:**
- `CapturePoint { monitor_id: Option<MonitorId>, edge: Position }` identifies an output edge; `None` preserves direction-only backends.
- `Capture::create/destroy` accept one `CapturePoint`; `InputCapture::create(handle, points: Vec<CapturePoint>)` registers all of a client's edges and `destroy(handle)` removes them. The capture stream returns `(handle, point, event)` so routing knows which edge fired.
- `InputCapture::monitor_catalog() -> Vec<MonitorDescriptor>` returns the backend's current named output catalog; unsupported backends return an empty catalog.
- Windows output enumeration returns stable device IDs together with current rectangles. `display_util::entered_barrier` returns the source display ID and edge.
- Hyprland uses `OutputInfo.name` as the output ID and creates/selects layer-shell edge surfaces per output and edge, preserving the existing suppression of edges shared by local outputs.

- [ ] Add portable geometry tests showing the Windows barrier identifies the source monitor and does not classify movement between local displays as a remote edge.
- [ ] Run the geometry tests on Windows and `gauss`; confirm they fail because the result has no monitor identity.
- [ ] Add output-selection tests showing a Hyprland output edge does not activate the same-direction edge on another output.
- [ ] Run the output-selection tests on Windows and `gauss`; confirm they fail because output identity is discarded.
- [ ] Update the common capture map and platform backends to use `CapturePoint`; keep legacy backends functional with `monitor_id: None`.
- [ ] Preserve multiple points for one client handle without duplicating handles or losing which point emitted an event.
- [ ] Run affected `input-capture` tests and `cargo check -p input-capture` on Windows and `gauss`; each host checks its native backend.
- [ ] Commit as `feat: capture per-monitor screen edges`.

### Task 3: Persist, discover, and route synchronized layouts

**Files:**
- Create: `src/monitor_grid.rs`
- Modify: `src/config.rs`
- Modify: `src/lib.rs`
- Modify: `src/client.rs`
- Modify: `src/connect.rs`
- Modify: `src/listen.rs`
- Modify: `src/capture.rs`
- Modify: `src/emulation.rs`
- Modify: `src/service.rs`
- Modify: `lan-mouse-ipc/src/lib.rs`
- Modify: `input-emulation/src/lib.rs`
- Create: `input-emulation/src/monitor_position.rs`
- Modify: `input-emulation/src/windows.rs`
- Modify: `input-emulation/src/wlroots.rs`

**Interfaces:**
- Persist a top-level `monitor_grid` snapshot in TOML while retaining each client's legacy `position` for older peers.
- Add frontend requests `DiscoverMonitors(ClientHandle)` and `UpdateMonitorGrid(MonitorGrid)` and corresponding catalog/grid update events.
- `monitor_grid::route(grid: &MonitorGrid, source_owner: &str, source_monitor: &MonitorId, edge: Position) -> Option<MonitorGridEntry>` selects only a remote neighbor.
- On authenticated connection, exchange capabilities and the local output catalog. The discovery request opens or reuses a peer connection without capturing input.
- Receive grid chunks into a bounded reassembler; accept only the highest `(revision, origin_fingerprint)` tuple, acknowledge applied or stale revisions, retry unacknowledged sends, and replay the current snapshot on reconnect.
- Incoming `EnterMonitor` creates an enter-only capture point at the named local output; the source edge's opposite side is the target entry edge, and the emulation backend moves the pointer to that edge's midpoint before input begins.
- `InputEmulation::enter_monitor(monitor_id: &MonitorId, edge: Position) -> Result<(), EmulationError>` resolves the output and places the virtual pointer at its edge midpoint. Windows uses virtual-desktop coordinates; wlroots uses output geometry and absolute virtual-pointer motion.

- [ ] Add service-model tests named in Review Focus for exact monitor-edge routing, missing outputs, legacy fallback, and grid chunk retry/reassembly.
- [ ] Run those tests on Windows and `gauss`; confirm they fail for the missing service behavior.
- [ ] Add monitor-position tests proving midpoint entry is independent of source and destination resolutions and unknown output IDs return an error.
- [ ] Run the monitor-position tests on Windows and `gauss`; confirm they fail because pointer placement is missing.
- [ ] Implement TOML persistence, catalog discovery, snapshot sync, route selection, enter-only barrier setup, and reconnect replay.
- [ ] Implement monitor-edge pointer placement for Windows and the wlroots virtual pointer; return a clear error for an unknown or disconnected output.
- [ ] Run affected crate tests and `cargo check --workspace` on Windows and `gauss`.
- [ ] Commit as `feat: route input through synchronized monitor grids`.

### Task 4: GTK monitor-grid editor

**Files:**
- Create: `lan-mouse-gtk/src/monitor_grid_window.rs`
- Create: `lan-mouse-gtk/resources/monitor_grid_window.ui`
- Modify: `lan-mouse-gtk/src/lib.rs`
- Modify: `lan-mouse-gtk/resources/window.ui`
- Modify: `lan-mouse-gtk/resources/resources.gresource.xml`
- Modify: `lan-mouse-gtk/src/window.rs`
- Modify: `lan-mouse-gtk/src/window/imp.rs`

**Interfaces:**
- Add an **Arrange monitors** action that opens a window showing local outputs in their detected arrangement and remote outputs as movable cells.
- Add **Discover displays** for a selected client; show connection/discovery progress and keep saved remote outputs visible but unavailable while disconnected.
- Dragging a remote tile updates its `GridPosition` and sends `UpdateMonitorGrid`; the window reflects the acknowledged revision and preserves the legacy position selector for peers without grid support.

- [ ] Add a GTK model test for moving a remote tile, preventing occupied-cell collisions, and retaining unavailable outputs.
- [ ] Run the GTK model test on Windows and `gauss`; confirm it fails because the editor model is missing.
- [ ] Implement the grid window, drag/drop, discovery action, IPC event updates, and save/ack state.
- [ ] Run `cargo check -p lan-mouse-gtk` and the app's GTK tests on Windows and `gauss`; inspect the window on both hosts when their graphical sessions are available.
- [ ] Commit as `feat: add monitor grid editor`.

### Task 5: Public documentation and final verification

**Files:**
- Modify: `README.md`
- Modify: `DOC.md`

- [ ] Document the editor, per-monitor output discovery, equal-cell edge handoff, legacy-peer fallback, and the new TOML layout.
- [ ] Run `cargo fmt --check`, `cargo clippy --workspace --all-targets --all-features`, and affected workspace tests on Windows and `gauss`.
- [ ] Build the Windows workspace here and the Linux workspace on `gauss`; record any platform prerequisites that are unavailable.
- [ ] Review the full diff for protocol compatibility, stale-output handling, documentation accuracy, and any changes outside this feature.
- [ ] Commit as `docs: explain monitor grid configuration`.
