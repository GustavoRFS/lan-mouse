# Audio Sharing Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Send system audio and an optional microphone between authenticated Windows and Linux peers, with one direction active per peer and a local virtual recording device for received microphones.

**Architecture:** Keep per-peer settings in the existing client configuration and GUI IPC. Add a bounded, low-latency Opus media path over each existing DTLS peer connection, a common audio crate, PulseAudio-compatible Linux backend, WASAPI capture/playback, and a WDM Windows virtual microphone driver. Older peers continue normal input and clipboard traffic and never receive audio until they advertise compatible audio support.

**Tech Stack:** Rust 2021, Tokio/LocalSet, GTK4/Libadwaita, `lan-mouse-proto`, `lan-mouse-ipc`, `opus` 0.4 with bundled libopus, `libpulse-binding` 2.30, `wasapi` 0.26, Windows Driver Kit (WDM/WaveRT), CMake for bundled libopus.

**Spec:** `docs/superpowers/specs/2026-11-05-audio-sharing-design.md`

## Global Constraints

- Support Windows and Linux, including mixed Windows↔Linux peers.
- Permit one direction of transmission per pair at a time.
- Keep audio sessions independent per peer; a connected computer's mode and selected devices must not disable sessions with other computers.
- Establish authenticated DTLS connectivity for configured audio peers even when pointer capture has never entered that peer.
- Send system audio and microphone in separate tracks; both may be sent together in the active direction.
- Present a received microphone as a recording endpoint other applications can select.
- Save role and device choices per connected computer.
- Keep audio packets at or below 1200 bytes and do not route them through `ProtoEvent` or clipboard framing.
- Use a 2048-byte network receive buffer, then explicitly discard audio datagrams over the 1200-byte media limit so UDP truncation cannot make an oversized frame appear valid.
- Stop capture, playback, driver forwarding, and pending buffers on disconnect, role change, pause, or client removal.
- Develop and test the Windows WDM driver with test signing; public production signing and installer integration are out of scope.
- Build Windows Rust code in the user's Windows checkout; use gauss for Linux builds while ligma is unavailable; perform real audio verification on Windows and Linux audio sessions.

## Review Focus

1. A peer with no audio support or a non-complementary role must not receive audio — test negotiation with `Off`, `Send`, `Receive`, and an old peer.
2. Disconnect, pause, and client removal must stop all audio work and flush queued samples — test session cleanup in the audio coordinator.
3. Missing or hot-unplugged devices must report an error and stop/recover the stream without panicking — test the fake backend and manually unplug selected devices.
4. Malformed, oversized, or unknown-version media datagrams must be dropped safely — test protocol decoding at the 1200-byte boundary.
5. Loss, duplicate, and reordering must not create unbounded latency or memory use — test sequence handling and the fixed-capacity playout buffer.

## File Structure

- `lan-mouse-audio/`: audio types, Opus codec, bounded playout, PulseAudio-compatible backend, WASAPI backend, and user-mode driver bridge.
- `lan-mouse-proto/src/audio.rs`: bounded audio control/data datagram framing; `lan-mouse-proto/src/lib.rs` exports it.
- `lan-mouse-ipc/src/lib.rs`: serialized audio settings, device list/status events, and frontend requests.
- `src/config.rs`, `src/client.rs`, `src/service.rs`: persist per-client audio settings and coordinate sessions with peer connections.
- `src/connect.rs`, `src/listen.rs`, `src/capture.rs`, `src/emulation.rs`: classify and route audio control/data frames on authenticated DTLS connections.
- `lan-mouse-gtk/src/client_row.rs`, `lan-mouse-gtk/src/client_row/imp.rs`, `lan-mouse-gtk/src/audio_settings.rs`, `lan-mouse-gtk/resources/client_row.ui`, `lan-mouse-gtk/src/window.rs`, `lan-mouse-gtk/src/lib.rs`: per-peer controls, device choices, and status display.
- `audio-driver/windows/`: WDK solution, WDM/WaveRT virtual capture endpoint, INF package, and developer build notes.
- `Cargo.toml`, `README.md`, `DOC.md`, `.github/workflows/rust.yml`: workspace/dependency, platform build requirements, and public behavior documentation.

## Interfaces

### Shared configuration and IPC

Add these serialized types in `lan-mouse-ipc/src/lib.rs`; derive `Serialize`/`Deserialize`, `Clone`, `Debug`, and equality traits, and serialize enum names in snake case. `lan-mouse-audio` may depend on `lan-mouse-ipc` for the shared device descriptors and on `lan-mouse-proto` for wire track IDs; these crates must not depend back on the audio crate.

```rust
pub enum AudioMode { Off, Send, Receive }

pub struct AudioConfig {
    pub mode: AudioMode,
    pub send_system_audio: bool,
    pub system_capture_device: Option<String>,
    pub send_microphone: bool,
    pub microphone_capture_device: Option<String>,
    pub playback_device: Option<String>,
    pub receive_microphone: bool,
}

pub struct AudioDevice {
    pub id: String,
    pub name: String,
    pub direction: AudioDeviceDirection,
}

pub enum AudioDeviceDirection { Capture, Playback }
pub enum AudioPeerStatus { Off, WaitingForPeer, Starting, Active, DeviceUnavailable(String), Error(String) }
```

`AudioConfig::default()` is `Off`, with system audio enabled for a future `Send` selection, microphone sending disabled, and received-microphone exposure disabled. Add `audio: AudioConfig` to `ClientConfig`, `ConfigClient`, and optional `TomlClient.audio`; a TOML client with no audio section must deserialize as `Off`.

Add `FrontendRequest::UpdateAudioConfig(ClientHandle, AudioConfig)` and `FrontendRequest::EnumerateAudioDevices`, plus `FrontendEvent::AudioDevices(Vec<AudioDevice>)` and `FrontendEvent::AudioPeerStatus { handle, status }`.

### Media protocol and audio core

Expose `lan_mouse_proto::AudioMessage` with:

```rust
pub enum AudioTrack { System, Microphone }
pub enum AudioRole { Off, Send, Receive }
pub struct AudioControl { pub role: AudioRole, pub send_system_audio: bool, pub send_microphone: bool, pub receive_microphone: bool }
pub struct AudioData { pub track: AudioTrack, pub sequence: u32, pub timestamp_48khz: u64, pub opus: Vec<u8> }
pub enum AudioMessage { Control(AudioControl), Data(AudioData) }
pub fn is_audio_message(bytes: &[u8]) -> bool;
pub fn encode_audio_message(message: &AudioMessage) -> Result<Vec<u8>, ProtocolError>;
pub fn decode_audio_message(bytes: &[u8]) -> Result<AudioMessage, ProtocolError>;
```

Use a manual, endian-stable binary encoding: four-byte magic `LMAU`, version byte `1`, message-kind byte, and fixed-width fields. Control packets carry role plus three flag bits; data packets carry track, `u32` sequence, `u64` 48 kHz timestamp, `u16` Opus length, and exact payload bytes. Reject unknown versions, reserved flag bits, invalid enum values, mismatched lengths, and packets larger than 1200 bytes before allocating. Keep native device IDs local. `AudioRole` is the wire equivalent of IPC `AudioMode`; map explicitly in the service. Audio capability/role announcements contain no device names or IDs.

In `lan-mouse-audio`, add `AudioSource::{SystemOutput(String), Microphone(String)}`, `AudioTarget::{Playback(String), VirtualMicrophone(String)}`, `PcmFrame { track, sequence, timestamp_48khz, channels, samples: Vec<i16> }` where samples are interleaved and contain 960 samples per channel, `AudioCapture { frames: tokio::sync::mpsc::Receiver<PcmFrame>, stop: tokio::sync::oneshot::Sender<()> }`, and `AudioPlayback { frames: tokio::sync::mpsc::Sender<PcmFrame>, stop: tokio::sync::oneshot::Sender<()> }`. Define `AudioBackend: Send + Sync` with `enumerate_devices(&self) -> Result<Vec<AudioDevice>, AudioError>`, `start_capture(&self, source: AudioSource) -> Result<AudioCapture, AudioError>`, and `start_target(&self, target: AudioTarget) -> Result<AudioPlayback, AudioError>`. Capture/playback channels have capacity four and dropping either stop sender terminates its worker. Encode 48 kHz audio in 20 ms Opus frames; use stereo 128 kbit/s for system audio and mono 48 kbit/s for microphone audio. The playout queue holds at most four frames per track and discards stale frames.

## Tasks

### Task 1: Per-client audio settings and IPC

**Files:** Modify `lan-mouse-ipc/src/lib.rs`, `src/config.rs`, `src/client.rs`, `src/service.rs`.

- [x] Add tests `audio_config_ipc_json_round_trips`, `legacy_toml_client_defaults_audio_to_off`, `audio_config_toml_preserves_device_preferences`, and `audio_config_update_changes_only_the_selected_client`; verify existing client TOML still loads with `AudioMode::Off` and new device IDs survive config parsing.
- [x] Run `cargo test -p lan-mouse-ipc audio_config_ipc_json_round_trips` and `cargo test -p lan-mouse --lib --no-default-features audio_` in the Windows checkout; confirm both fail for the missing audio fields.
  - Expected: the IPC test fails to compile because `AudioConfig` is not defined; the config test fails because legacy clients do not yet expose `audio`.
- [x] Implement the serialized types and `ClientConfig.audio` from Interfaces; add `#[serde(default)]` to the optional TOML audio setting and map it through `ConfigClient` conversions.
- [x] Handle `UpdateAudioConfig` in the service, update the corresponding `ClientConfig`, broadcast the updated state, and persist it through `save_config()`.
- [x] Re-run both tests in the Windows checkout; verify an old config without `[clients.audio]` defaults to `Off` and a new one preserves every device ID and option.
  - Expected: both focused tests pass, with existing TOML configs loading as `AudioMode::Off`.
- [x] Commit as `feat: add per-client audio configuration`.

### Task 2: Audio control/data framing and DTLS routing

**Files:** Create `lan-mouse-proto/src/audio.rs`; modify `lan-mouse-proto/src/lib.rs`, `src/connect.rs`, `src/listen.rs`, `src/capture.rs`, `src/emulation.rs`, `src/service.rs`.

- [x] Add tests `audio_control_round_trips`, `audio_data_round_trips`, `audio_decoder_rejects_oversize`, and `audio_decoder_rejects_unknown_version_or_track`; assert 1200 bytes is the maximum accepted datagram.
- [x] Run `cargo test -p lan-mouse-proto audio_` in the Windows checkout; confirm the new tests fail before implementation.
  - Expected: the new protocol tests fail to compile because the audio framing API is not implemented.
- [x] Implement `AudioMessage` encode/decode and `is_audio_message` with a distinct magic/version prefix and a hard 1200-byte limit; export it without changing the fixed-size `ProtoEvent` representation. Use `AudioRole` on the wire and map it to/from IPC `AudioMode` in the service.
- [x] Update both DTLS read loops to check clipboard framing, then audio framing, then existing input events; route audio messages through bounded channels to `EmulationEvent`/`NetworkEvent`.
- [x] Use a 2048-byte DTLS receive scratch buffer and drop audio frames over 1200 bytes before decoding.
- [x] Emit connect-side and listen-side disconnect events to the service; count authenticated audio frames as peer activity so they do not trigger the existing one-second no-input disconnect detector.
- [x] Add tests proving an unknown audio version is ignored while ordinary `ProtoEvent::Input` and clipboard frames keep their existing decode paths.
- [x] Re-run `cargo test -p lan-mouse-proto -p lan-mouse-ipc -p lan-mouse --lib --no-default-features` in the Windows checkout; commit as `feat: add bounded audio datagrams`.
  - Expected: both crate suites pass; input and clipboard serialization tests remain unchanged.

### Task 3: Audio core, Opus codec, and bounded playout

**Files:** Create `lan-mouse-audio/Cargo.toml`, `lan-mouse-audio/src/lib.rs`, `lan-mouse-audio/src/codec.rs`, `lan-mouse-audio/src/buffer.rs`; modify root `Cargo.toml`.

- [x] Add tests `system_opus_frame_round_trips_stereo_48khz`, `microphone_opus_frame_round_trips_mono_48khz`, `playout_queue_is_bounded`, `playout_queue_discards_stale_sequence`, and `playout_queue_drops_duplicate_or_reordered_sequences`.
- [x] Run `cargo test -p lan-mouse-audio` in the Windows checkout; confirm tests fail before the crate exists.
  - Expected: the new codec and buffer tests fail because their types and functions are not implemented.
- [x] Add the crate to the workspace and implement `AudioSource`, `AudioTarget`, `PcmFrame`, `AudioBackend`, 20 ms/48 kHz Opus encode/decode, and a four-frame-per-track playout queue.
- [x] Use `opus = "0.4"` with bundled libopus; keep the `opus` encoder/decoder state on its audio worker thread and never hold the service event loop while encoding.
- [x] Re-run `cargo test -p lan-mouse-audio` in the Windows checkout; commit as `feat: add shared audio engine core`.
  - Expected: all audio core tests pass and every encoded packet fits the protocol limit.

### Task 4: Linux PulseAudio-compatible backend

**Files:** Create `lan-mouse-audio/src/linux/mod.rs`, `lan-mouse-audio/src/linux/pulse.rs`; modify `lan-mouse-audio/Cargo.toml`, Linux build dependency documentation in `README.md`.

- [x] Add backend unit tests `pulse_device_ids_are_stable`, `virtual_microphone_fifo_is_owner_only`, `virtual_microphone_module_uses_requested_format`, `virtual_microphone_module_unloads_on_stop`, and `missing_selected_pulse_device_returns_unavailable`.
- [x] Run `cargo test -p lan-mouse-audio` on gauss; all 10 core and Linux backend tests pass.
- [x] Implement device enumeration, microphone capture, selected-output monitor capture, playback to a selected sink, and a virtual microphone using PulseAudio-compatible APIs and `module-pipe-source`.
- [x] Create FIFO files only in the current user's runtime directory with owner-only permissions; load/unload the source module with the audio session and remove the FIFO during cleanup.
- [x] Smoke-test PipeWire Pulse compatibility on gauss: device enumeration, virtual-source load/unload, and FIFO cleanup all succeeded.
- [ ] Verify actual capture and playback, including microphone visibility in a recording application, on PulseAudio and PipeWire sessions during Task 8 end-to-end validation.
- [x] Commit as `feat: add Linux audio backend`.

### Task 5: Windows WASAPI backend and virtual microphone driver

**Files:** Create `lan-mouse-audio/src/windows/mod.rs`, `lan-mouse-audio/src/windows/wasapi.rs`, `lan-mouse-audio/src/windows/driver.rs`, and `audio-driver/windows/` WDK solution/driver/INF; modify `lan-mouse-audio/Cargo.toml` and Windows developer build docs.

- [x] Add Rust tests `driver_pcm_block_rejects_wrong_format`, `driver_queue_is_bounded`, and `driver_queue_flushes_on_stop` for the service/driver bridge.
- [x] Run `cargo test -p lan-mouse-audio` in the Windows checkout; the bridge and WASAPI helper tests pass.
  - Expected: bridge tests fail because the Windows driver queue and PCM validation are not implemented.
- [x] Implement WASAPI device enumeration, selected-endpoint loopback capture, microphone capture, and rendering with `wasapi = "0.26"`; keep COM setup, interfaces, and teardown on a dedicated audio thread.
- [ ] Adapt the Microsoft SYSVAD/WaveRT sample into a mono 48 kHz capture endpoint named `Lan Mouse Virtual Microphone`; define `IOCTL_LAN_MOUSE_MIC_WRITE` for signed 16-bit PCM in 20 ms blocks and `IOCTL_LAN_MOUSE_MIC_FLUSH` for stop. Bound the driver queue to four blocks and flush it on stop.
- [ ] Implement and build the WDK solution on a Windows host with the documented test-signing configuration; manually install the test driver, confirm Windows lists it as a recording device, and verify a recording app receives remote microphone audio. The current Windows host has no WDK.
  - Expected: the WDK build produces a test-signed package and the installed endpoint appears to Windows recording applications.
- [x] Build the full Rust workspace on the Windows host; the audio backend and bridge unit tests pass. The WDK driver build/install verification remains outstanding.
- [ ] Commit as `feat: add Windows audio backend and virtual mic`.
  - Expected: the Rust backend and bridge tests pass. If no Windows WDK host is available, record driver build/install verification as outstanding.

### Task 6: Peer audio negotiation and service coordination

**Files:** Create `lan-mouse-audio/src/session.rs`; modify `lan-mouse-audio/src/lib.rs`, `src/service.rs`, `src/connect.rs`, `src/listen.rs`, `src/capture.rs`, `src/emulation.rs`.

- [x] Add negotiation tests `audio_starts_only_for_complementary_peer_modes`, `audio_never_starts_for_old_peer`, `multiple_audio_peers_have_independent_sessions`, `audio_stops_on_disconnect`, `audio_stops_on_role_change`, and `system_and_microphone_tracks_remain_separate`.
- [ ] Re-run `cargo test -p lan-mouse-audio` in the Windows checkout after the final negotiation change.
- [x] Implement per-peer negotiation, complementary roles, `WaitingForPeer`, separate tracks, and cleanup scoped to each peer.
- [x] Keep audio-configured peers connected independently of input-capture activation; announce roles on connection and reconnect.
- [x] Route inbound audio only to an authenticated configured peer, including peers whose input capture is inactive.
- [x] Exchange role controls and stream system and microphone frames as distinct audio tracks over the authenticated DTLS connection.
- [x] Bound audio queues and stop workers and clear buffers on disconnect or configuration changes.
- [ ] Re-run the audio and protocol Rust suites after the final negotiation change.
- [ ] Commit as `feat: coordinate per-peer audio sessions`.
  - Expected: both suites pass, including independent sessions and cleanup after each terminal event.

### Task 7: GTK per-peer audio controls and status

**Files:** Create `lan-mouse-gtk/src/audio_settings.rs`; modify `lan-mouse-gtk/src/lib.rs`, `lan-mouse-gtk/src/window.rs`, `lan-mouse-gtk/src/client_object.rs`, `lan-mouse-gtk/src/client_object/imp.rs`, `lan-mouse-gtk/src/client_row.rs`, `lan-mouse-gtk/src/client_row/imp.rs`, `lan-mouse-gtk/resources/client_row.ui`.

- [x] Create `lan-mouse-gtk/src/audio_settings.rs` with pure `audio_settings_visibility(mode: AudioMode) -> AudioSettingsVisibility`; test `off_hides_audio_controls`, `send_shows_capture_choices`, and `receive_shows_playback_and_virtual_mic`.
- [x] Add a per-client `Compartilhamento de áudio` group with `Off`/`Send`/`Receive`, system-audio capture selection, optional microphone selection, playback selection, remote-mic switch, and peer status.
- [x] Send `UpdateAudioConfig` when a setting changes; handle device-list and status events in `lan-mouse-gtk/src/lib.rs`; update only the matching `ClientRow` and suppress callbacks while applying service state.
- [x] Run the GTK tests in the Windows checkout; manual inspection and restart persistence checks remain outstanding.
  - Expected: presentation tests pass; changing a control updates only its peer and survives a service restart.
- [ ] Commit as `feat: add per-peer audio settings UI`.

### Task 8: Documentation and end-to-end verification

**Files:** Modify `README.md`, `DOC.md`, `.github/workflows/rust.yml`; add developer instructions under `audio-driver/windows/README.md`.

- [x] Document supported Windows/Linux audio APIs, device selection, driver limitations, and per-peer modes.
- [x] Install `libpulse-dev` and CMake in the Linux Rust CI job; add compile checks for `lan-mouse-audio` to Linux and Windows CI jobs; keep production driver signing and installer publication out of CI.
- [ ] Run `cargo fmt --check` and resolve the existing formatting mismatch in `lan-mouse-gtk/src/linux_tray.rs`.
- [x] Run `cargo clippy --workspace --all-targets --all-features -- -D warnings` on Windows.
- [ ] Run the full Rust workspace test suite in the available hosts.
- [x] Build the complete Rust workspace on Windows.
- [ ] Build the Linux workspace on gauss and manually verify Windows→Linux and Linux→Windows, system-audio-only, microphone-only, both tracks together, mismatched modes, old peer, audio disconnect, selected-device removal, and restart persistence.
- [ ] Commit as `docs: document audio sharing and platform setup`.

## External Technical References

- [`opus` Rust bindings](https://docs.rs/opus/latest/opus/) and [bundled libopus build requirements](https://docs.rs/crate/opusic-sys/latest)
- [`wasapi` Rust crate](https://docs.rs/wasapi/latest/wasapi/index.html)
- [`libpulse-binding` Rust crate](https://docs.rs/crate/libpulse-binding/latest)
- [PulseAudio `module-pipe-source`](https://wiki.freedesktop.org/www/Software/PulseAudio/Documentation/User/Modules/)
- [PipeWire PulseAudio replacement](https://docs.pipewire.org/devel/page_man_pipewire-pulse_1.html)
- [Microsoft SYSVAD driver sample](https://learn.microsoft.com/en-us/samples/microsoft/windows-driver-samples/sysvad-virtual-audio-device-driver-sample/)
