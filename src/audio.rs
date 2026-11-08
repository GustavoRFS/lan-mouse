use crate::capture::AudioSender;
use lan_mouse_audio::{
    AudioBackend, AudioCapture, AudioDecoder, AudioEncoder, AudioError, AudioPlayback,
    AudioSessionState, AudioSource, AudioTarget, PlayoutBuffer, session::audio_control,
};
use lan_mouse_ipc::{
    AudioConfig, AudioDevice, AudioDeviceDirection, AudioMode, AudioPeerStatus, ClientHandle,
};
use lan_mouse_proto::{AudioData, AudioMessage, AudioTrack};
use std::sync::Arc;
use tokio::task::{JoinHandle, spawn_local};
use tokio_util::sync::CancellationToken;

pub(crate) struct AudioRuntime {
    backend: Arc<dyn AudioBackend>,
    peers: std::collections::HashMap<ClientHandle, PeerRuntime>,
}

struct PeerRuntime {
    config: AudioConfig,
    session: AudioSessionState,
    status: AudioPeerStatus,
    captures: Vec<ActiveCapture>,
    targets: Vec<(AudioTrack, AudioPlayback)>,
    decoder: Option<AudioDecoder>,
    playout: PlayoutBuffer,
}

struct ActiveCapture {
    cancellation: CancellationToken,
    task: JoinHandle<()>,
}

impl Drop for ActiveCapture {
    fn drop(&mut self) {
        self.cancellation.cancel();
        self.task.abort();
    }
}

impl PeerRuntime {
    fn new(config: AudioConfig) -> Self {
        let status = if config.mode == AudioMode::Off {
            AudioPeerStatus::Off
        } else {
            AudioPeerStatus::WaitingForPeer
        };
        Self {
            session: AudioSessionState::new(config.clone()),
            config,
            status,
            captures: Vec::new(),
            targets: Vec::new(),
            decoder: None,
            playout: PlayoutBuffer::new(),
        }
    }

    fn stop_streams(&mut self) {
        self.captures.clear();
        self.targets.clear();
        self.decoder = None;
        self.playout.clear();
    }
}

impl AudioRuntime {
    pub(crate) fn new() -> Self {
        Self {
            backend: platform_backend(),
            peers: Default::default(),
        }
    }

    pub(crate) fn enumerate_devices(&self) -> Result<Vec<AudioDevice>, AudioError> {
        self.backend.enumerate_devices()
    }

    pub(crate) fn register_peer(&mut self, handle: ClientHandle, config: AudioConfig) {
        self.peers
            .entry(handle)
            .or_insert_with(|| PeerRuntime::new(config));
    }

    pub(crate) fn configure_peer(
        &mut self,
        handle: ClientHandle,
        config: AudioConfig,
        transport: &AudioSender,
    ) -> AudioPeerStatus {
        let peer = self
            .peers
            .entry(handle)
            .or_insert_with(|| PeerRuntime::new(config.clone()));
        let should_announce = config.mode != AudioMode::Off || peer.config.mode != AudioMode::Off;
        if peer.config != config {
            peer.stop_streams();
            peer.config = config.clone();
            peer.session.set_local_config(config.clone());
        }
        if should_announce {
            transport.send(handle, AudioMessage::Control(audio_control(&config)));
        }
        self.reconcile(handle, transport);
        self.status(handle).unwrap_or(AudioPeerStatus::Off)
    }

    pub(crate) fn announce_peer(
        &mut self,
        handle: ClientHandle,
        config: AudioConfig,
        transport: &AudioSender,
    ) {
        self.peers
            .entry(handle)
            .or_insert_with(|| PeerRuntime::new(config.clone()));
        transport.send(handle, AudioMessage::Control(audio_control(&config)));
    }

    pub(crate) fn observe_message(
        &mut self,
        handle: ClientHandle,
        config: AudioConfig,
        message: AudioMessage,
        transport: &AudioSender,
    ) -> AudioPeerStatus {
        self.peers
            .entry(handle)
            .or_insert_with(|| PeerRuntime::new(config.clone()));
        match message {
            AudioMessage::Control(control) => {
                let peer = self.peers.get_mut(&handle).expect("peer was inserted");
                peer.stop_streams();
                peer.session.set_local_config(config.clone());
                peer.config = config;
                peer.session.observe_remote(control);
                self.reconcile(handle, transport);
            }
            AudioMessage::Data(data) => self.receive_data(handle, data),
        }
        self.status(handle).unwrap_or(AudioPeerStatus::Off)
    }

    pub(crate) fn disconnect(&mut self, handle: ClientHandle) -> AudioPeerStatus {
        if let Some(peer) = self.peers.get_mut(&handle) {
            peer.stop_streams();
            peer.session.disconnect();
            peer.status = if peer.config.mode == AudioMode::Off {
                AudioPeerStatus::Off
            } else {
                AudioPeerStatus::WaitingForPeer
            };
        }
        self.status(handle).unwrap_or(AudioPeerStatus::Off)
    }

    pub(crate) fn remove_peer(&mut self, handle: ClientHandle) {
        self.peers.remove(&handle);
    }

    pub(crate) fn status(&self, handle: ClientHandle) -> Option<AudioPeerStatus> {
        self.peers.get(&handle).map(|peer| peer.status.clone())
    }

    fn reconcile(&mut self, handle: ClientHandle, transport: &AudioSender) {
        let Some(peer) = self.peers.get_mut(&handle) else {
            return;
        };
        peer.stop_streams();
        let plan = peer.session.negotiation();
        if peer.config.mode == AudioMode::Off {
            peer.status = AudioPeerStatus::Off;
            return;
        }
        if plan.status != Some(AudioPeerStatus::Active) {
            peer.status = plan.status.unwrap_or(AudioPeerStatus::WaitingForPeer);
            return;
        }

        let devices = match self.backend.enumerate_devices() {
            Ok(devices) => devices,
            Err(error) => {
                peer.status = audio_status_for_error(error);
                return;
            }
        };
        let mut errors = Vec::new();
        for track in plan.send_tracks.iter().copied() {
            let source = match capture_source(track, &peer.config, &devices) {
                Ok(source) => source,
                Err(error) => {
                    errors.push(error);
                    continue;
                }
            };
            match self.backend.start_capture(source) {
                Ok(capture) => peer
                    .captures
                    .push(spawn_capture(handle, capture, transport)),
                Err(error) => errors.push(error),
            }
        }

        for track in plan.receive_tracks.iter().copied() {
            let target = match playback_target(track, handle, &peer.config, &devices) {
                Ok(target) => target,
                Err(error) => {
                    errors.push(error);
                    continue;
                }
            };
            match self.backend.start_target(target) {
                Ok(playback) => peer.targets.push((track, playback)),
                Err(error) => errors.push(error),
            }
        }
        peer.status = errors
            .into_iter()
            .next()
            .map(audio_status_for_error)
            .unwrap_or(AudioPeerStatus::Active);
    }

    fn receive_data(&mut self, handle: ClientHandle, data: AudioData) {
        let Some(peer) = self.peers.get_mut(&handle) else {
            return;
        };
        let plan = peer.session.negotiation();
        if !plan.receive_tracks.contains(&data.track) {
            return;
        }
        let decoder = match peer.decoder.as_mut() {
            Some(decoder) => decoder,
            None => match AudioDecoder::new() {
                Ok(decoder) => peer.decoder.insert(decoder),
                Err(error) => {
                    peer.status = audio_status_for_error(error);
                    return;
                }
            },
        };
        let frame = match decoder.decode(&data) {
            Ok(frame) => frame,
            Err(error) => {
                log::debug!("discarding invalid audio data from peer {handle}: {error}");
                return;
            }
        };
        if !peer.playout.push(frame) {
            return;
        }
        while let Some(frame) = peer.playout.pop(data.track) {
            if let Some((_, target)) = peer.targets.iter().find(|(track, _)| *track == data.track) {
                if let Err(error) = target.frames.try_send(frame) {
                    log::debug!("dropping received audio for peer {handle}: {error}");
                }
            }
        }
    }
}

fn spawn_capture(
    handle: ClientHandle,
    capture: AudioCapture,
    sender: &AudioSender,
) -> ActiveCapture {
    let cancellation = CancellationToken::new();
    let child = cancellation.clone();
    let sender = sender.clone();
    // Dropping this sender stops the backend worker, so retain it for the task's lifetime.
    let stop = capture.stop;
    let mut frames = capture.frames;
    let task = spawn_local(async move {
        let _stop = stop;
        let mut encoder = match AudioEncoder::new() {
            Ok(encoder) => encoder,
            Err(error) => {
                log::error!("cannot create audio encoder for peer {handle}: {error}");
                return;
            }
        };
        loop {
            tokio::select! {
                _ = child.cancelled() => break,
                frame = frames.recv() => match frame {
                    Some(frame) => match encoder.encode(&frame) {
                            Ok(data) => sender.send(handle, AudioMessage::Data(data)),
                            Err(error) => log::debug!("cannot encode audio for peer {handle}: {error}"),
                        },
                    None => break,
                }
            }
        }
    });
    ActiveCapture { cancellation, task }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{
        sync::{mpsc, oneshot},
        task::LocalSet,
    };

    #[test]
    fn capture_worker_stays_alive_while_audio_frames_are_consumed() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(LocalSet::new().run_until(async {
            let (frames_tx, frames_rx) = mpsc::channel(1);
            let (stop_tx, mut stop_rx) = oneshot::channel();
            let capture = AudioCapture {
                frames: frames_rx,
                stop: stop_tx,
            };
            let channel = crate::capture::test_utils::audio_channel();
            let mut active = spawn_capture(7, capture, channel.sender());

            tokio::task::yield_now().await;
            assert!(matches!(
                stop_rx.try_recv(),
                Err(oneshot::error::TryRecvError::Empty)
            ));

            frames_tx
                .send(lan_mouse_audio::PcmFrame {
                    track: AudioTrack::Microphone,
                    sequence: 0,
                    timestamp_48khz: 0,
                    channels: 1,
                    samples: vec![0; lan_mouse_audio::SAMPLES_PER_FRAME],
                })
                .await
                .unwrap();
            tokio::task::yield_now().await;

            active.cancellation.cancel();
            (&mut active.task).await.unwrap();
        }));
    }
}

#[cfg(any(target_os = "windows", target_os = "linux"))]
fn capture_source(
    track: AudioTrack,
    config: &AudioConfig,
    devices: &[AudioDevice],
) -> Result<AudioSource, AudioError> {
    match track {
        AudioTrack::System => Ok(AudioSource::SystemOutput(select_device(
            config.system_capture_device.as_deref(),
            AudioDeviceDirection::Playback,
            devices,
        )?)),
        AudioTrack::Microphone => Ok(AudioSource::Microphone(select_device(
            config.microphone_capture_device.as_deref(),
            AudioDeviceDirection::Capture,
            devices,
        )?)),
    }
}

#[cfg(any(target_os = "windows", target_os = "linux"))]
fn playback_target(
    track: AudioTrack,
    handle: ClientHandle,
    config: &AudioConfig,
    devices: &[AudioDevice],
) -> Result<AudioTarget, AudioError> {
    match track {
        AudioTrack::System => Ok(AudioTarget::Playback(select_device(
            config.playback_device.as_deref(),
            AudioDeviceDirection::Playback,
            devices,
        )?)),
        AudioTrack::Microphone => Ok(AudioTarget::VirtualMicrophone(handle.to_string())),
    }
}

#[cfg(any(target_os = "windows", target_os = "linux"))]
fn select_device(
    preferred: Option<&str>,
    direction: AudioDeviceDirection,
    devices: &[AudioDevice],
) -> Result<String, AudioError> {
    if let Some(id) = preferred {
        return Ok(id.to_owned());
    }
    devices
        .iter()
        .find(|device| device.direction == direction)
        .map(|device| device.id.clone())
        .ok_or_else(|| {
            AudioError::DeviceUnavailable(match direction {
                AudioDeviceDirection::Capture => "no capture device is available".into(),
                AudioDeviceDirection::Playback => "no playback device is available".into(),
            })
        })
}

fn audio_status_for_error(error: AudioError) -> AudioPeerStatus {
    match error {
        AudioError::DeviceUnavailable(message) => AudioPeerStatus::DeviceUnavailable(message),
        other => AudioPeerStatus::Error(other.to_string()),
    }
}

#[cfg(target_os = "windows")]
fn platform_backend() -> Arc<dyn AudioBackend> {
    Arc::new(lan_mouse_audio::windows::WasapiAudioBackend)
}

#[cfg(target_os = "linux")]
fn platform_backend() -> Arc<dyn AudioBackend> {
    Arc::new(lan_mouse_audio::linux::PulseAudioBackend)
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
fn platform_backend() -> Arc<dyn AudioBackend> {
    Arc::new(UnavailableAudioBackend)
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
struct UnavailableAudioBackend;

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
impl AudioBackend for UnavailableAudioBackend {
    fn enumerate_devices(&self) -> Result<Vec<AudioDevice>, AudioError> {
        Err(AudioError::BackendUnavailable(
            "audio sharing is supported on Windows and Linux".into(),
        ))
    }

    fn start_capture(&self, _source: AudioSource) -> Result<AudioCapture, AudioError> {
        Err(AudioError::BackendUnavailable(
            "audio sharing is supported on Windows and Linux".into(),
        ))
    }

    fn start_target(&self, _target: AudioTarget) -> Result<AudioPlayback, AudioError> {
        Err(AudioError::BackendUnavailable(
            "audio sharing is supported on Windows and Linux".into(),
        ))
    }
}
