use crate::{AudioError, PcmFrame, SAMPLE_RATE_HZ, SAMPLES_PER_FRAME};
use lan_mouse_proto::{AudioData, AudioTrack};
use opus::{Application, Bitrate, Channels, Decoder, Encoder};

const SYSTEM_BITRATE: i32 = 128_000;
const MICROPHONE_BITRATE: i32 = 48_000;

pub struct AudioEncoder {
    system: Encoder,
    microphone: Encoder,
}

impl AudioEncoder {
    pub fn new() -> Result<Self, AudioError> {
        let mut system = Encoder::new(SAMPLE_RATE_HZ, Channels::Stereo, Application::Audio)?;
        system.set_bitrate(Bitrate::Bits(SYSTEM_BITRATE))?;

        let mut microphone = Encoder::new(SAMPLE_RATE_HZ, Channels::Mono, Application::Voip)?;
        microphone.set_bitrate(Bitrate::Bits(MICROPHONE_BITRATE))?;

        Ok(Self { system, microphone })
    }

    pub fn encode(&mut self, frame: &PcmFrame) -> Result<AudioData, AudioError> {
        let (channels, encoder) = match frame.track {
            AudioTrack::System => (2, &mut self.system),
            AudioTrack::Microphone => (1, &mut self.microphone),
        };
        let expected_samples = SAMPLES_PER_FRAME * usize::from(channels);
        if frame.channels != channels || frame.samples.len() != expected_samples {
            return Err(AudioError::InvalidPcmFrame {
                track: frame.track,
                channels: frame.channels,
                samples: frame.samples.len(),
                expected_channels: channels,
                expected_samples,
            });
        }

        let opus = encoder.encode_vec(&frame.samples, crate::MAX_OPUS_PACKET_SIZE)?;
        Ok(AudioData {
            track: frame.track,
            sequence: frame.sequence,
            timestamp_48khz: frame.timestamp_48khz,
            opus,
        })
    }
}

pub struct AudioDecoder {
    system: Decoder,
    microphone: Decoder,
}

impl AudioDecoder {
    pub fn new() -> Result<Self, AudioError> {
        Ok(Self {
            system: Decoder::new(SAMPLE_RATE_HZ, Channels::Stereo)?,
            microphone: Decoder::new(SAMPLE_RATE_HZ, Channels::Mono)?,
        })
    }

    pub fn decode(&mut self, packet: &AudioData) -> Result<PcmFrame, AudioError> {
        if packet.opus.is_empty() || packet.opus.len() > crate::MAX_OPUS_PACKET_SIZE {
            return Err(AudioError::InvalidOpusPacket(packet.opus.len()));
        }

        let (channels, decoder) = match packet.track {
            AudioTrack::System => (2, &mut self.system),
            AudioTrack::Microphone => (1, &mut self.microphone),
        };
        let mut samples = vec![0i16; SAMPLES_PER_FRAME * usize::from(channels)];
        let samples_per_channel = decoder.decode(&packet.opus, &mut samples, false)?;
        if samples_per_channel != SAMPLES_PER_FRAME {
            return Err(AudioError::UnexpectedDecodedFrameSize {
                actual: samples_per_channel,
                expected: SAMPLES_PER_FRAME,
            });
        }

        Ok(PcmFrame {
            track: packet.track,
            sequence: packet.sequence,
            timestamp_48khz: packet.timestamp_48khz,
            channels,
            samples,
        })
    }
}
