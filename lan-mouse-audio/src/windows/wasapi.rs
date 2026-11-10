use super::driver::{DriverQueue, WindowsVirtualMic};
use crate::{
    AUDIO_CHANNEL_CAPACITY, AudioBackend, AudioCapture, AudioDevice, AudioDeviceDirection,
    AudioError, AudioPlayback, AudioSource, AudioTarget, PcmFrame, SAMPLE_RATE_HZ,
    SAMPLES_PER_FRAME,
};
use lan_mouse_proto::AudioTrack;
use std::{
    collections::VecDeque,
    thread,
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, oneshot};
use wasapi::{
    AudioCaptureClient, AudioClient, AudioRenderClient, DeviceEnumerator, Direction, SampleType,
    StreamMode, WaveFormat,
};

const WASAPI_RENDER_PREFIX: &str = "wasapi:render:";
const WASAPI_CAPTURE_PREFIX: &str = "wasapi:capture:";
const SHARED_BUFFER_DURATION_HNS: i64 = 2_000_000;
const WORKER_POLL_INTERVAL: Duration = Duration::from_millis(4);

#[derive(Clone, Copy, Debug, Default)]
pub struct WasapiAudioBackend;

impl AudioBackend for WasapiAudioBackend {
    fn enumerate_devices(&self) -> Result<Vec<AudioDevice>, AudioError> {
        with_mta(|| {
            let enumerator = DeviceEnumerator::new().map_err(audio_backend_error)?;
            let mut devices = Vec::new();
            append_devices(&enumerator, Direction::Render, &mut devices)?;
            append_devices(&enumerator, Direction::Capture, &mut devices)?;
            Ok(devices)
        })
    }

    fn start_capture(&self, source: AudioSource) -> Result<AudioCapture, AudioError> {
        let (device_id, endpoint_direction, track, channels) = match source {
            AudioSource::SystemOutput(id) => (
                resolve_device_id(&id, Direction::Render)?,
                Direction::Render,
                AudioTrack::System,
                2,
            ),
            AudioSource::Microphone(id) => (
                resolve_device_id(&id, Direction::Capture)?,
                Direction::Capture,
                AudioTrack::Microphone,
                1,
            ),
        };
        let (frames_tx, frames_rx) = mpsc::channel(AUDIO_CHANNEL_CAPACITY);
        let (stop_tx, stop_rx) = oneshot::channel();
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);

        thread::Builder::new()
            .name("lan-mouse-wasapi-capture".into())
            .spawn(move || {
                capture_thread(
                    device_id,
                    endpoint_direction,
                    track,
                    channels,
                    frames_tx,
                    stop_rx,
                    ready_tx,
                )
            })
            .map_err(|error| {
                AudioError::Backend(format!("cannot start WASAPI capture: {error}"))
            })?;

        let mut stop = Some(stop_tx);
        wait_for_worker(ready_rx, &mut stop, "WASAPI capture")?;
        Ok(AudioCapture {
            frames: frames_rx,
            stop: stop
                .take()
                .expect("successful worker retains its stop sender"),
        })
    }

    fn start_target(&self, target: AudioTarget) -> Result<AudioPlayback, AudioError> {
        let target = match target {
            AudioTarget::Playback(id) => {
                PlaybackTarget::Wasapi(resolve_device_id(&id, Direction::Render)?)
            }
            AudioTarget::VirtualMicrophone(_peer_id) => PlaybackTarget::VirtualMicrophone,
        };
        let (frames_tx, frames_rx) = mpsc::channel(AUDIO_CHANNEL_CAPACITY);
        let (stop_tx, stop_rx) = oneshot::channel();
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);

        thread::Builder::new()
            .name("lan-mouse-wasapi-playback".into())
            .spawn(move || playback_thread(target, frames_rx, stop_rx, ready_tx))
            .map_err(|error| AudioError::Backend(format!("cannot start audio output: {error}")))?;

        let mut stop = Some(stop_tx);
        wait_for_worker(ready_rx, &mut stop, "audio output")?;
        Ok(AudioPlayback {
            frames: frames_tx,
            stop: stop
                .take()
                .expect("successful worker retains its stop sender"),
        })
    }
}

fn append_devices(
    enumerator: &DeviceEnumerator,
    direction: Direction,
    devices: &mut Vec<AudioDevice>,
) -> Result<(), AudioError> {
    let collection = enumerator
        .get_device_collection(&direction)
        .map_err(audio_backend_error)?;
    for index in 0..collection.get_nbr_devices().map_err(audio_backend_error)? {
        let device = collection
            .get_device_at_index(index)
            .map_err(audio_backend_error)?;
        let raw_id = device.get_id().map_err(audio_backend_error)?;
        let (prefix, device_direction) = match direction {
            Direction::Render => (WASAPI_RENDER_PREFIX, AudioDeviceDirection::Playback),
            Direction::Capture => (WASAPI_CAPTURE_PREFIX, AudioDeviceDirection::Capture),
        };
        devices.push(AudioDevice {
            id: format!("{prefix}{raw_id}"),
            name: device.get_friendlyname().map_err(audio_backend_error)?,
            direction: device_direction,
        });
    }
    Ok(())
}

fn resolve_device_id(id: &str, direction: Direction) -> Result<String, AudioError> {
    let prefix = match direction {
        Direction::Render => WASAPI_RENDER_PREFIX,
        Direction::Capture => WASAPI_CAPTURE_PREFIX,
    };
    id.strip_prefix(prefix)
        .filter(|raw_id| !raw_id.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| AudioError::DeviceUnavailable(id.to_owned()))
}

fn wait_for_worker(
    ready: std::sync::mpsc::Receiver<Result<(), String>>,
    stop: &mut Option<oneshot::Sender<()>>,
    name: &str,
) -> Result<(), AudioError> {
    match ready.recv_timeout(Duration::from_secs(10)) {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(AudioError::Backend(error)),
        Err(error) => {
            if let Some(stop) = stop.take() {
                let _ = stop.send(());
            }
            Err(AudioError::Backend(format!(
                "{name} did not initialize: {error}"
            )))
        }
    }
}

fn capture_thread(
    device_id: String,
    endpoint_direction: Direction,
    track: AudioTrack,
    channels: u8,
    frames_tx: mpsc::Sender<PcmFrame>,
    mut stop_rx: oneshot::Receiver<()>,
    ready_tx: std::sync::mpsc::SyncSender<Result<(), String>>,
) {
    let _com = match ComApartment::initialize() {
        Ok(com) => com,
        Err(error) => {
            let _ = ready_tx.send(Err(error));
            return;
        }
    };
    let setup = create_capture_stream(&device_id, endpoint_direction, channels);
    let (mut client, capture) = match setup {
        Ok(stream) => stream,
        Err(error) => {
            let _ = ready_tx.send(Err(error));
            return;
        }
    };
    if let Err(error) = client.start_stream() {
        let _ = ready_tx.send(Err(backend_error(error)));
        return;
    }
    if ready_tx.send(Ok(())).is_err() {
        let _ = client.stop_stream();
        return;
    }

    capture_loop(
        &mut client,
        capture,
        track,
        channels,
        frames_tx,
        &mut stop_rx,
    );
    let _ = client.stop_stream();
}

fn create_capture_stream(
    device_id: &str,
    endpoint_direction: Direction,
    channels: u8,
) -> Result<(AudioClient, AudioCaptureClient), String> {
    let enumerator = DeviceEnumerator::new().map_err(backend_error)?;
    let device = enumerator
        .get_device(device_id)
        .map_err(|error| format!("selected WASAPI device is unavailable: {error:?}"))?;
    if device.get_direction() != endpoint_direction {
        return Err("selected WASAPI device direction changed".into());
    }
    let mut client = device.get_iaudioclient().map_err(backend_error)?;
    let format = pcm_format(channels);
    let mode = StreamMode::PollingShared {
        autoconvert: true,
        buffer_duration_hns: SHARED_BUFFER_DURATION_HNS,
    };
    // WASAPI permits capture initialization on a render endpoint only in shared loopback mode.
    client
        .initialize_client(&format, &Direction::Capture, &mode)
        .map_err(backend_error)?;
    let capture = client.get_audiocaptureclient().map_err(backend_error)?;
    Ok((client, capture))
}

fn capture_loop(
    client: &mut AudioClient,
    capture: AudioCaptureClient,
    track: AudioTrack,
    channels: u8,
    frames_tx: mpsc::Sender<PcmFrame>,
    stop_rx: &mut oneshot::Receiver<()>,
) {
    let bytes_per_block = SAMPLES_PER_FRAME * usize::from(channels) * 2;
    let mut bytes = VecDeque::with_capacity(bytes_per_block * 2);
    let mut sequence = 0u32;
    let started = Instant::now();

    while !stop_requested(stop_rx) {
        let packet_count = match capture.get_next_packet_size() {
            Ok(Some(count)) => count,
            Ok(None) => match client.get_buffer_size() {
                Ok(count) => count,
                Err(error) => {
                    log::warn!("WASAPI capture stopped: {error:?}");
                    break;
                }
            },
            Err(error) => {
                log::warn!("WASAPI capture stopped: {error:?}");
                break;
            }
        };

        if packet_count > 0 {
            if let Err(error) = capture.read_from_device_to_deque(&mut bytes) {
                log::warn!("WASAPI capture read failed: {error:?}");
                break;
            }
            while bytes.len() >= bytes_per_block {
                let mut samples = Vec::with_capacity(SAMPLES_PER_FRAME * usize::from(channels));
                for _ in 0..(SAMPLES_PER_FRAME * usize::from(channels)) {
                    let low = bytes.pop_front().expect("full PCM frame was checked");
                    let high = bytes.pop_front().expect("PCM samples contain two bytes");
                    samples.push(i16::from_le_bytes([low, high]));
                }
                let timestamp_48khz = (started.elapsed().as_nanos() * u128::from(SAMPLE_RATE_HZ)
                    / 1_000_000_000) as u64;
                let frame = PcmFrame {
                    track,
                    sequence,
                    timestamp_48khz,
                    channels,
                    samples,
                };
                sequence = sequence.wrapping_add(1);
                match frames_tx.try_send(frame) {
                    Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => {}
                    Err(mpsc::error::TrySendError::Closed(_)) => return,
                }
            }
        } else {
            thread::sleep(WORKER_POLL_INTERVAL);
        }
    }
}

enum PlaybackTarget {
    Wasapi(String),
    VirtualMicrophone,
}

enum PlaybackDevice {
    Wasapi {
        client: AudioClient,
        render: AudioRenderClient,
        channels: u8,
    },
    VirtualMicrophone(WindowsVirtualMic),
}

fn playback_thread(
    target: PlaybackTarget,
    frames_rx: mpsc::Receiver<PcmFrame>,
    mut stop_rx: oneshot::Receiver<()>,
    ready_tx: std::sync::mpsc::SyncSender<Result<(), String>>,
) {
    let (device, com) = match target {
        PlaybackTarget::Wasapi(device_id) => {
            let com = match ComApartment::initialize() {
                Ok(com) => com,
                Err(error) => {
                    let _ = ready_tx.send(Err(error));
                    return;
                }
            };
            match create_playback_stream(&device_id) {
                Ok(device) => (device, Some(com)),
                Err(error) => {
                    let _ = ready_tx.send(Err(error));
                    return;
                }
            }
        }
        PlaybackTarget::VirtualMicrophone => match WindowsVirtualMic::open() {
            Ok(mic) => (PlaybackDevice::VirtualMicrophone(mic), None),
            Err(error) => {
                let _ = ready_tx.send(Err(error.to_string()));
                return;
            }
        },
    };

    if ready_tx.send(Ok(())).is_err() {
        return;
    }
    playback_loop(device, frames_rx, &mut stop_rx);
    drop(com);
}

fn create_playback_stream(device_id: &str) -> Result<PlaybackDevice, String> {
    let enumerator = DeviceEnumerator::new().map_err(backend_error)?;
    let device = enumerator
        .get_device(device_id)
        .map_err(|error| format!("selected WASAPI output is unavailable: {error:?}"))?;
    if device.get_direction() != Direction::Render {
        return Err("selected WASAPI output is not a render endpoint".into());
    }
    let mut client = device.get_iaudioclient().map_err(backend_error)?;
    let format = pcm_format(2);
    let mode = StreamMode::PollingShared {
        autoconvert: true,
        buffer_duration_hns: SHARED_BUFFER_DURATION_HNS,
    };
    client
        .initialize_client(&format, &Direction::Render, &mode)
        .map_err(backend_error)?;
    let render = client.get_audiorenderclient().map_err(backend_error)?;
    client.start_stream().map_err(backend_error)?;
    Ok(PlaybackDevice::Wasapi {
        client,
        render,
        channels: 2,
    })
}

fn playback_loop(
    device: PlaybackDevice,
    mut frames_rx: mpsc::Receiver<PcmFrame>,
    stop_rx: &mut oneshot::Receiver<()>,
) {
    match device {
        PlaybackDevice::Wasapi {
            client,
            render,
            channels,
        } => {
            let block_bytes = SAMPLES_PER_FRAME * usize::from(channels) * 2;
            let block_align = usize::from(channels) * 2;
            let mut bytes = VecDeque::with_capacity(block_bytes * AUDIO_CHANNEL_CAPACITY);
            'playback: while !stop_requested(stop_rx) {
                while bytes.len() + block_bytes <= block_bytes * AUDIO_CHANNEL_CAPACITY {
                    match frames_rx.try_recv() {
                        Ok(frame) if valid_playback_frame(&frame, AudioTrack::System, channels) => {
                            for sample in frame.samples {
                                bytes.extend(sample.to_le_bytes());
                            }
                        }
                        Ok(_) => log::warn!("dropping a malformed or unexpected audio frame"),
                        Err(mpsc::error::TryRecvError::Empty) => break,
                        Err(mpsc::error::TryRecvError::Disconnected) => break 'playback,
                    }
                }

                let available = match client.get_available_space_in_frames() {
                    Ok(available) => available as usize,
                    Err(error) => {
                        log::warn!("WASAPI playback stopped: {error:?}");
                        break;
                    }
                };
                let queued_frames = bytes.len() / block_align;
                let frames_to_write = if queued_frames == 0 {
                    available
                } else {
                    available.min(queued_frames)
                };
                if frames_to_write > 0 {
                    if queued_frames == 0 {
                        let silence = vec![0u8; frames_to_write * block_align];
                        if let Err(error) = render.write_to_device(frames_to_write, &silence, None)
                        {
                            log::warn!("WASAPI silence write failed: {error:?}");
                            break;
                        }
                    } else if let Err(error) =
                        render.write_to_device_from_deque(frames_to_write, &mut bytes, None)
                    {
                        log::warn!("WASAPI playback write failed: {error:?}");
                        break;
                    }
                }
                thread::sleep(WORKER_POLL_INTERVAL);
            }
            let _ = client.stop_stream();
            bytes.clear();
        }
        PlaybackDevice::VirtualMicrophone(device) => {
            let mut queue = DriverQueue::default();
            'worker: while !stop_requested(stop_rx) {
                match frames_rx.try_recv() {
                    Ok(frame) => {
                        if let Err(error) = queue.push(&frame) {
                            log::warn!("dropping invalid virtual microphone frame: {error}");
                        }
                    }
                    Err(mpsc::error::TryRecvError::Disconnected) => break,
                    Err(mpsc::error::TryRecvError::Empty) => {
                        thread::sleep(WORKER_POLL_INTERVAL);
                    }
                }
                while let Some(block) = queue.pop() {
                    if let Err(error) = device.write(&block) {
                        log::warn!("virtual microphone write failed: {error}");
                        queue.flush();
                        break 'worker;
                    }
                }
            }
            queue.flush();
            if let Err(error) = device.flush() {
                log::warn!("virtual microphone flush failed: {error}");
            }
        }
    }
}

fn valid_playback_frame(frame: &PcmFrame, track: AudioTrack, channels: u8) -> bool {
    frame.track == track
        && frame.channels == channels
        && frame.samples.len() == SAMPLES_PER_FRAME * usize::from(channels)
}

fn pcm_format(channels: u8) -> WaveFormat {
    WaveFormat::new(
        16,
        16,
        &SampleType::Int,
        SAMPLE_RATE_HZ as usize,
        usize::from(channels),
        None,
    )
}

fn stop_requested(stop_rx: &mut oneshot::Receiver<()>) -> bool {
    !matches!(stop_rx.try_recv(), Err(oneshot::error::TryRecvError::Empty))
}

fn backend_error(error: impl std::fmt::Debug) -> String {
    format!("{error:?}")
}

fn audio_backend_error(error: impl std::fmt::Debug) -> AudioError {
    AudioError::Backend(backend_error(error))
}

fn with_mta<T>(operation: impl FnOnce() -> Result<T, AudioError>) -> Result<T, AudioError> {
    let _com = ComApartment::initialize().map_err(AudioError::Backend)?;
    operation()
}

struct ComApartment;

impl ComApartment {
    fn initialize() -> Result<Self, String> {
        wasapi::initialize_mta()
            .ok()
            .map(|()| Self)
            .map_err(|error| format!("cannot initialize COM MTA: {error:?}"))
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        wasapi::deinitialize();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_ids_preserve_native_endpoint_ids() {
        let native_id = r"{0.0.0.00000000}.{endpoint-id}";
        let id = format!("{WASAPI_RENDER_PREFIX}{native_id}");
        assert_eq!(
            resolve_device_id(&id, Direction::Render).unwrap(),
            native_id
        );
        assert!(resolve_device_id(&id, Direction::Capture).is_err());
    }

    #[test]
    fn playback_frame_requires_the_system_stereo_track() {
        let frame = PcmFrame {
            track: AudioTrack::System,
            sequence: 0,
            timestamp_48khz: 0,
            channels: 2,
            samples: vec![0; SAMPLES_PER_FRAME * 2],
        };
        assert!(valid_playback_frame(&frame, AudioTrack::System, 2));
        assert!(!valid_playback_frame(&frame, AudioTrack::Microphone, 1));
    }
}
