mod buffer;
mod codec;

pub use buffer::{PLAYOUT_FRAMES_PER_TRACK, PlayoutBuffer};
pub use codec::{AudioDecoder, AudioEncoder};
pub use lan_mouse_ipc::{AudioDevice, AudioDeviceDirection};
pub use lan_mouse_proto::{AudioTrack, MAX_AUDIO_DATAGRAM_SIZE};
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};

/// Audio is normalized to 48 kHz, with one Opus frame per 20 ms.
pub const SAMPLE_RATE_HZ: u32 = 48_000;
pub const SAMPLES_PER_FRAME: usize = 960;
pub const FRAME_DURATION_MS: u32 = 20;
pub const AUDIO_CHANNEL_CAPACITY: usize = 4;

/// Audio data framing size, including the 21-byte protocol header.
pub const AUDIO_DATA_HEADER_SIZE: usize = 21;

/// Largest Opus payload that can fit into one audio datagram.
pub const MAX_OPUS_PACKET_SIZE: usize =
    lan_mouse_proto::MAX_AUDIO_DATAGRAM_SIZE - AUDIO_DATA_HEADER_SIZE;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AudioSource {
    SystemOutput(String),
    Microphone(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AudioTarget {
    Playback(String),
    VirtualMicrophone(String),
}

/// Interleaved signed 16-bit PCM with exactly 960 samples per channel.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PcmFrame {
    pub track: AudioTrack,
    pub sequence: u32,
    pub timestamp_48khz: u64,
    pub channels: u8,
    pub samples: Vec<i16>,
}

#[derive(Debug, Error)]
pub enum AudioError {
    #[error("Opus codec error: {0}")]
    Opus(#[from] opus::Error),
    #[error(
        "invalid PCM frame for {track:?}: expected {expected_channels} channels and {expected_samples} samples, got {channels} channels and {samples} samples"
    )]
    InvalidPcmFrame {
        track: AudioTrack,
        channels: u8,
        samples: usize,
        expected_channels: u8,
        expected_samples: usize,
    },
    #[error("invalid Opus packet size: {0} bytes")]
    InvalidOpusPacket(usize),
    #[error("Opus packet decoded to {actual} samples per channel; expected {expected}")]
    UnexpectedDecodedFrameSize { actual: usize, expected: usize },
    #[error("audio device is unavailable: {0}")]
    DeviceUnavailable(String),
    #[error("audio backend is unavailable: {0}")]
    BackendUnavailable(String),
    #[error("audio backend error: {0}")]
    Backend(String),
}

/// A capture stream. Dropping `stop` signals its backend worker to stop.
pub struct AudioCapture {
    pub frames: mpsc::Receiver<PcmFrame>,
    pub stop: oneshot::Sender<()>,
}

/// A playback stream. Dropping `stop` signals its backend worker to stop.
pub struct AudioPlayback {
    pub frames: mpsc::Sender<PcmFrame>,
    pub stop: oneshot::Sender<()>,
}

pub trait AudioBackend: Send + Sync {
    fn enumerate_devices(&self) -> Result<Vec<AudioDevice>, AudioError>;
    fn start_capture(&self, source: AudioSource) -> Result<AudioCapture, AudioError>;
    fn start_target(&self, target: AudioTarget) -> Result<AudioPlayback, AudioError>;
}

#[cfg(test)]
mod tests;
