use crate::{
    AUDIO_CHANNEL_CAPACITY, AudioBackend, AudioCapture, AudioDevice, AudioDeviceDirection,
    AudioError, AudioPlayback, AudioSource, AudioTarget, PcmFrame, SAMPLE_RATE_HZ,
    SAMPLES_PER_FRAME,
};
use lan_mouse_proto::AudioTrack;
use pulse::{sample, stream::Direction};
use pulse_simple::Simple;
use std::{
    env,
    ffi::CString,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::unix::{
        ffi::OsStrExt,
        fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, oneshot};

const PULSE_DEVICE_PREFIX: &str = "pulse:";
const FIFO_MODE: u32 = 0o600;
const RECORD_CHUNK_BYTES_STEREO: usize = SAMPLES_PER_FRAME * 2 * 2;
static FIFO_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Default)]
pub struct PulseAudioBackend;

impl AudioBackend for PulseAudioBackend {
    fn enumerate_devices(&self) -> Result<Vec<AudioDevice>, AudioError> {
        let sinks = list_pulse_names("sinks")?;
        let sources = list_pulse_names("sources")?;
        let mut devices = Vec::with_capacity(sinks.len() + sources.len());

        devices.extend(sinks.into_iter().map(|name| AudioDevice {
            id: pulse_device_id("sink", &name),
            name,
            direction: AudioDeviceDirection::Playback,
        }));
        devices.extend(
            sources
                .into_iter()
                .filter(|name| !name.ends_with(".monitor"))
                .map(|name| AudioDevice {
                    id: pulse_device_id("source", &name),
                    name,
                    direction: AudioDeviceDirection::Capture,
                }),
        );
        Ok(devices)
    }

    fn start_capture(&self, source: AudioSource) -> Result<AudioCapture, AudioError> {
        let (device, track, channels) = match source {
            AudioSource::SystemOutput(id) => {
                let sinks = list_pulse_names("sinks")?;
                let sources = list_pulse_names("sources")?;
                let sink = resolve_device_id(&id, "sink", &sinks)?;
                let monitor = resolve_monitor_source(&sink, &sources)?;
                (monitor, AudioTrack::System, 2)
            }
            AudioSource::Microphone(id) => {
                let sources = list_pulse_names("sources")?;
                let source = resolve_device_id(&id, "source", &sources)?;
                if source.ends_with(".monitor") {
                    return Err(AudioError::DeviceUnavailable(
                        "a sink monitor cannot be selected as a microphone".into(),
                    ));
                }
                (source, AudioTrack::Microphone, 1)
            }
        };

        let chunk_bytes = frame_byte_len(channels);
        let spec = sample_spec(channels)?;
        let attr = record_buffer_attr(chunk_bytes);
        let stream = Simple::new(
            None,
            "Lan Mouse",
            Direction::Record,
            Some(&device),
            match track {
                AudioTrack::System => "System audio capture",
                AudioTrack::Microphone => "Microphone capture",
            },
            &spec,
            None,
            Some(&attr),
        )
        .map_err(|error| {
            AudioError::Backend(format!("cannot open PulseAudio capture: {error:?}"))
        })?;

        let (frames_tx, frames_rx) = mpsc::channel(AUDIO_CHANNEL_CAPACITY);
        let (stop_tx, stop_rx) = oneshot::channel();
        thread::Builder::new()
            .name("lan-mouse-audio-capture".into())
            .spawn(move || capture_worker(stream, frames_tx, stop_rx, track, channels, chunk_bytes))
            .map_err(|error| AudioError::Backend(format!("cannot start audio capture: {error}")))?;

        Ok(AudioCapture {
            frames: frames_rx,
            stop: stop_tx,
        })
    }

    fn start_target(&self, target: AudioTarget) -> Result<AudioPlayback, AudioError> {
        let worker = match target {
            AudioTarget::Playback(id) => {
                let sinks = list_pulse_names("sinks")?;
                let sink = resolve_device_id(&id, "sink", &sinks)?;
                let spec = sample_spec(2)?;
                let attr = playback_buffer_attr(RECORD_CHUNK_BYTES_STEREO);
                let stream = Simple::new(
                    None,
                    "Lan Mouse",
                    Direction::Playback,
                    Some(&sink),
                    "Received system audio",
                    &spec,
                    None,
                    Some(&attr),
                )
                .map_err(|error| {
                    AudioError::Backend(format!("cannot open PulseAudio playback: {error:?}"))
                })?;
                PlaybackWorker::Pulse(stream)
            }
            AudioTarget::VirtualMicrophone(peer_id) => {
                PlaybackWorker::VirtualMicrophone(create_virtual_microphone(&peer_id)?)
            }
        };

        let (frames_tx, frames_rx) = mpsc::channel(AUDIO_CHANNEL_CAPACITY);
        let (stop_tx, stop_rx) = oneshot::channel();
        thread::Builder::new()
            .name("lan-mouse-audio-playback".into())
            .spawn(move || playback_worker(worker, frames_rx, stop_rx))
            .map_err(|error| {
                AudioError::Backend(format!("cannot start audio playback: {error}"))
            })?;

        Ok(AudioPlayback {
            frames: frames_tx,
            stop: stop_tx,
        })
    }
}

enum PlaybackWorker {
    Pulse(Simple),
    VirtualMicrophone(VirtualMicrophoneOutput),
}

fn list_pulse_names(kind: &str) -> Result<Vec<String>, AudioError> {
    let output = run_pactl(&["list", "short", kind])?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(parse_short_device_names(&stdout))
}

fn parse_short_device_names(output: &str) -> Vec<String> {
    output
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            fields.next()?;
            let name = fields.next()?;
            (!name.is_empty()).then(|| name.to_string())
        })
        .collect()
}

fn pulse_device_id(kind: &str, name: &str) -> String {
    format!("{PULSE_DEVICE_PREFIX}{kind}:{name}")
}

fn resolve_device_id(id: &str, kind: &str, known_names: &[String]) -> Result<String, AudioError> {
    let prefix = format!("{PULSE_DEVICE_PREFIX}{kind}:");
    let Some(name) = id.strip_prefix(&prefix) else {
        return Err(AudioError::DeviceUnavailable(id.to_owned()));
    };
    known_names
        .iter()
        .find(|known| known.as_str() == name)
        .cloned()
        .ok_or_else(|| AudioError::DeviceUnavailable(name.to_owned()))
}

fn resolve_monitor_source(sink: &str, sources: &[String]) -> Result<String, AudioError> {
    let expected_name = format!("{sink}.monitor");
    sources
        .iter()
        .find(|source| **source == expected_name)
        .cloned()
        .ok_or(AudioError::DeviceUnavailable(expected_name))
}

fn sample_spec(channels: u8) -> Result<sample::Spec, AudioError> {
    let spec = sample::Spec {
        format: sample::Format::S16NE,
        rate: SAMPLE_RATE_HZ,
        channels,
    };
    if spec.is_valid() {
        Ok(spec)
    } else {
        Err(AudioError::Backend(
            "invalid PulseAudio sample format".into(),
        ))
    }
}

fn frame_byte_len(channels: u8) -> usize {
    SAMPLES_PER_FRAME * usize::from(channels) * 2
}

fn track_channel_count(track: AudioTrack) -> u8 {
    match track {
        AudioTrack::System => 2,
        AudioTrack::Microphone => 1,
    }
}

fn record_buffer_attr(frame_bytes: usize) -> pulse::def::BufferAttr {
    pulse::def::BufferAttr {
        maxlength: (frame_bytes * AUDIO_CHANNEL_CAPACITY) as u32,
        tlength: u32::MAX,
        prebuf: u32::MAX,
        minreq: u32::MAX,
        fragsize: frame_bytes as u32,
    }
}

fn playback_buffer_attr(frame_bytes: usize) -> pulse::def::BufferAttr {
    pulse::def::BufferAttr {
        maxlength: (frame_bytes * AUDIO_CHANNEL_CAPACITY) as u32,
        tlength: (frame_bytes * 2) as u32,
        prebuf: frame_bytes as u32,
        minreq: frame_bytes as u32,
        fragsize: u32::MAX,
    }
}

fn capture_worker(
    stream: Simple,
    frames_tx: mpsc::Sender<PcmFrame>,
    mut stop_rx: oneshot::Receiver<()>,
    track: AudioTrack,
    channels: u8,
    chunk_bytes: usize,
) {
    let started_at = Instant::now();
    let mut sequence = 0u32;
    let mut bytes = vec![0; chunk_bytes];

    loop {
        if stop_requested(&mut stop_rx) {
            break;
        }
        if let Err(error) = stream.read(&mut bytes) {
            log::warn!("PulseAudio capture stopped: {error:?}");
            break;
        }

        let (sample_bytes, remainder) = bytes.as_chunks::<2>();
        debug_assert!(remainder.is_empty());
        let samples = sample_bytes
            .iter()
            .map(|sample| i16::from_ne_bytes(*sample))
            .collect();
        let frame = PcmFrame {
            track,
            sequence,
            timestamp_48khz: elapsed_samples_48khz(started_at),
            channels,
            samples,
        };
        sequence = sequence.wrapping_add(1);
        match frames_tx.try_send(frame) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                log::debug!("dropping captured audio frame because its queue is full");
            }
            Err(mpsc::error::TrySendError::Closed(_)) => break,
        }
    }
}

fn playback_worker(
    mut worker: PlaybackWorker,
    mut frames_rx: mpsc::Receiver<PcmFrame>,
    mut stop_rx: oneshot::Receiver<()>,
) {
    loop {
        if stop_requested(&mut stop_rx) {
            break;
        }
        let frame = match frames_rx.try_recv() {
            Ok(frame) => frame,
            Err(mpsc::error::TryRecvError::Empty) => {
                thread::sleep(Duration::from_millis(2));
                continue;
            }
            Err(mpsc::error::TryRecvError::Disconnected) => break,
        };

        let expected_track = match &worker {
            PlaybackWorker::Pulse(_) => AudioTrack::System,
            PlaybackWorker::VirtualMicrophone(_) => AudioTrack::Microphone,
        };
        if frame.track != expected_track
            || frame.channels != track_channel_count(expected_track)
            || frame.samples.len() != SAMPLES_PER_FRAME * usize::from(frame.channels)
        {
            log::warn!("discarding invalid audio frame for PulseAudio target");
            continue;
        }

        let bytes = samples_as_native_bytes(&frame.samples);
        let result = match &mut worker {
            PlaybackWorker::Pulse(stream) => stream.write(&bytes).map_err(|error| {
                AudioError::Backend(format!("PulseAudio playback failed: {error:?}"))
            }),
            PlaybackWorker::VirtualMicrophone(output) => {
                write_virtual_microphone_frame(output, &bytes, &mut stop_rx)
            }
        };
        if let Err(error) = result {
            log::warn!("audio playback stopped: {error}");
            break;
        }
    }
}

fn samples_as_native_bytes(samples: &[i16]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(samples.len() * 2);
    for sample in samples {
        bytes.extend_from_slice(&sample.to_ne_bytes());
    }
    bytes
}

fn elapsed_samples_48khz(started_at: Instant) -> u64 {
    started_at
        .elapsed()
        .as_nanos()
        .saturating_mul(u128::from(SAMPLE_RATE_HZ))
        .checked_div(1_000_000_000)
        .unwrap_or_default() as u64
}

fn stop_requested(stop_rx: &mut oneshot::Receiver<()>) -> bool {
    matches!(
        stop_rx.try_recv(),
        Ok(()) | Err(oneshot::error::TryRecvError::Closed)
    )
}

fn create_virtual_microphone(peer_id: &str) -> Result<VirtualMicrophoneOutput, AudioError> {
    let runtime_dir = current_user_runtime_dir()?;
    let sequence = FIFO_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let fifo_path = runtime_dir.join(format!(
        "lan-mouse-remote-mic-{}-{sequence}.fifo",
        std::process::id()
    ));
    create_fifo_at(&fifo_path)?;

    let result = (|| {
        let fifo = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&fifo_path)
            .map_err(|error| {
                AudioError::Backend(format!("cannot open microphone FIFO: {error}"))
            })?;
        fifo.set_permissions(fs::Permissions::from_mode(FIFO_MODE))
            .map_err(|error| {
                AudioError::Backend(format!("cannot secure microphone FIFO: {error}"))
            })?;

        let source_name = virtual_source_name(peer_id, sequence);
        let arguments = pipe_source_arguments(&fifo_path, &source_name, 1);
        let output = run_pactl(&["load-module", "module-pipe-source", &arguments])?;
        let module_id = parse_module_id(&output)?;
        let module = ModuleLease::new(module_id, Box::new(unload_pulse_module));
        Ok(VirtualMicrophoneOutput {
            module: Some(module),
            fifo: Some(fifo),
            fifo_path: fifo_path.clone(),
        })
    })();

    if result.is_err() {
        remove_fifo_if_present(&fifo_path);
    }
    result
}

fn current_user_runtime_dir() -> Result<PathBuf, AudioError> {
    let path = env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .ok_or_else(|| AudioError::BackendUnavailable("XDG_RUNTIME_DIR is not set".into()))?;
    validate_runtime_dir(&path, unsafe { libc::geteuid() })?;
    Ok(path)
}

fn validate_runtime_dir(path: &Path, uid: libc::uid_t) -> Result<(), AudioError> {
    let metadata = fs::metadata(path)
        .map_err(|error| AudioError::Backend(format!("cannot inspect XDG_RUNTIME_DIR: {error}")))?;
    if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
        return Err(AudioError::BackendUnavailable(
            "XDG_RUNTIME_DIR must be a private directory owned by the current user".into(),
        ));
    }
    Ok(())
}

fn create_fifo_at(path: &Path) -> Result<(), AudioError> {
    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|error| AudioError::Backend(format!("invalid FIFO path: {error}")))?;
    let result = unsafe { libc::mkfifo(c_path.as_ptr(), FIFO_MODE as libc::mode_t) };
    if result != 0 {
        return Err(AudioError::Backend(format!(
            "cannot create microphone FIFO: {}",
            io::Error::last_os_error()
        )));
    }
    fs::set_permissions(path, fs::Permissions::from_mode(FIFO_MODE)).map_err(|error| {
        remove_fifo_if_present(path);
        AudioError::Backend(format!("cannot secure microphone FIFO: {error}"))
    })?;
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| AudioError::Backend(format!("cannot inspect microphone FIFO: {error}")))?;
    if !metadata.file_type().is_fifo() || metadata.mode() & 0o777 != FIFO_MODE {
        remove_fifo_if_present(path);
        return Err(AudioError::Backend(
            "microphone FIFO permissions are not owner-only".into(),
        ));
    }
    Ok(())
}

fn remove_fifo_if_present(path: &Path) {
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if metadata.file_type().is_fifo() {
            if let Err(error) = fs::remove_file(path) {
                log::warn!("cannot remove microphone FIFO {}: {error}", path.display());
            }
        }
    }
}

fn virtual_source_name(peer_id: &str, sequence: u64) -> String {
    let suffix: String = peer_id
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || *character == '_')
        .take(24)
        .collect();
    let suffix = if suffix.is_empty() { "peer" } else { &suffix };
    format!(
        "lan_mouse_remote_mic_{}_{}_{}",
        std::process::id(),
        sequence,
        suffix
    )
}

fn pulse_quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn pipe_source_arguments(path: &Path, source_name: &str, channels: u8) -> String {
    let path = path.to_string_lossy();
    let format = if cfg!(target_endian = "little") {
        "s16le"
    } else {
        "s16be"
    };
    format!(
        "file={} source_name={source_name} format={format} rate={SAMPLE_RATE_HZ} channels={channels}",
        pulse_quote(&path)
    )
}

type ModuleUnload = Box<dyn FnOnce(u32) -> Result<(), AudioError> + Send>;

struct ModuleLease {
    id: u32,
    unload: Option<ModuleUnload>,
}

impl ModuleLease {
    fn new(id: u32, unload: ModuleUnload) -> Self {
        Self {
            id,
            unload: Some(unload),
        }
    }
}

impl Drop for ModuleLease {
    fn drop(&mut self) {
        if let Some(unload) = self.unload.take() {
            if let Err(error) = unload(self.id) {
                log::warn!("cannot unload PulseAudio module {}: {error}", self.id);
            }
        }
    }
}

struct VirtualMicrophoneOutput {
    // Drop order is explicit so the server releases the FIFO before it is removed.
    module: Option<ModuleLease>,
    fifo: Option<File>,
    fifo_path: PathBuf,
}

impl Drop for VirtualMicrophoneOutput {
    fn drop(&mut self) {
        drop(self.module.take());
        drop(self.fifo.take());
        remove_fifo_if_present(&self.fifo_path);
    }
}

fn write_virtual_microphone_frame(
    output: &mut VirtualMicrophoneOutput,
    bytes: &[u8],
    stop_rx: &mut oneshot::Receiver<()>,
) -> Result<(), AudioError> {
    let fifo = output
        .fifo
        .as_mut()
        .expect("virtual microphone FIFO is open while its worker runs");
    let mut written = 0;
    while written < bytes.len() {
        if stop_requested(stop_rx) {
            return Ok(());
        }
        match fifo.write(&bytes[written..]) {
            Ok(0) => return Err(AudioError::Backend("microphone FIFO closed".into())),
            Ok(count) => written += count,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(2));
            }
            Err(error) => {
                return Err(AudioError::Backend(format!(
                    "microphone FIFO write failed: {error}"
                )));
            }
        }
    }
    Ok(())
}

fn run_pactl(arguments: &[&str]) -> Result<Output, AudioError> {
    let output = Command::new("pactl")
        .args(arguments)
        .output()
        .map_err(|error| AudioError::BackendUnavailable(format!("cannot run pactl: {error}")))?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(AudioError::Backend(format!(
            "pactl {} failed: {}",
            arguments.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}

fn parse_module_id(output: &Output) -> Result<u32, AudioError> {
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .map_err(|error| {
            AudioError::Backend(format!("pactl returned an invalid module id: {error}"))
        })
}

fn unload_pulse_module(module_id: u32) -> Result<(), AudioError> {
    run_pactl(&["unload-module", &module_id.to_string()]).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{Arc, Mutex};

    #[test]
    fn pulse_device_ids_are_stable() {
        let name = "alsa_output.pci-0000_00_1f.3.analog-stereo";
        assert_eq!(pulse_device_id("sink", name), pulse_device_id("sink", name));
        assert_eq!(pulse_device_id("sink", name), format!("pulse:sink:{name}"));
    }

    #[test]
    fn virtual_microphone_fifo_is_owner_only() {
        let runtime_dir = test_runtime_dir();
        let fifo_path = runtime_dir.join("remote-mic.fifo");
        create_fifo_at(&fifo_path).unwrap();
        let metadata = fs::symlink_metadata(&fifo_path).unwrap();
        assert!(metadata.file_type().is_fifo());
        assert_eq!(metadata.mode() & 0o777, FIFO_MODE);
        remove_fifo_if_present(&fifo_path);
        fs::remove_dir_all(runtime_dir).unwrap();
    }

    #[test]
    fn virtual_microphone_module_uses_requested_format() {
        let args = pipe_source_arguments(
            Path::new("/run/user/1000/remote mic.fifo"),
            "lan_mouse_remote_mic_test",
            1,
        );
        assert!(args.contains("format=s16le"));
        assert!(args.contains("rate=48000"));
        assert!(args.contains("channels=1"));
        assert!(args.contains("source_name=lan_mouse_remote_mic_test"));
        assert!(args.contains("file=\"/run/user/1000/remote mic.fifo\""));
    }

    #[test]
    fn virtual_microphone_module_unloads_on_stop() {
        let unloaded = Arc::new(Mutex::new(Vec::new()));
        let unloaded_by_worker = Arc::clone(&unloaded);
        let runtime_dir = test_runtime_dir();
        let fifo_path = runtime_dir.join("remote-mic.fifo");
        create_fifo_at(&fifo_path).unwrap();
        let fifo = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&fifo_path)
            .unwrap();
        let output = VirtualMicrophoneOutput {
            module: Some(ModuleLease::new(
                73,
                Box::new(move |id| {
                    unloaded_by_worker.lock().unwrap().push(id);
                    Ok(())
                }),
            )),
            fifo: Some(fifo),
            fifo_path: fifo_path.clone(),
        };

        drop(output);

        assert_eq!(*unloaded.lock().unwrap(), [73]);
        assert!(!fifo_path.exists());
        fs::remove_dir_all(runtime_dir).unwrap();
    }

    #[test]
    fn missing_selected_pulse_device_returns_unavailable() {
        let devices = vec!["alsa_output.test".to_string()];
        let result = resolve_device_id("pulse:sink:missing", "sink", &devices);
        assert!(matches!(result, Err(AudioError::DeviceUnavailable(_))));
    }

    fn test_runtime_dir() -> PathBuf {
        let sequence = FIFO_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!(
            "lan-mouse-pulse-test-{}-{sequence}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        validate_runtime_dir(&path, unsafe { libc::geteuid() }).unwrap();
        path
    }
}
