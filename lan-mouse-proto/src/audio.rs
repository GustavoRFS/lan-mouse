use super::ProtocolError;

/// Maximum encoded audio datagram size, including the framing header.
pub const MAX_AUDIO_DATAGRAM_SIZE: usize = 1200;

const AUDIO_MAGIC: &[u8; 4] = b"LMAU";
const AUDIO_VERSION: u8 = 1;
const CONTROL_KIND: u8 = 1;
const DATA_KIND: u8 = 2;
const CONTROL_SIZE: usize = 8;
const DATA_HEADER_SIZE: usize = 21;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioTrack {
    System,
    Microphone,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioRole {
    Off,
    Send,
    Receive,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioControl {
    pub role: AudioRole,
    pub send_system_audio: bool,
    pub send_microphone: bool,
    pub receive_microphone: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioData {
    pub track: AudioTrack,
    pub sequence: u32,
    pub timestamp_48khz: u64,
    pub opus: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AudioMessage {
    Control(AudioControl),
    Data(AudioData),
}

/// Returns whether a datagram belongs to the audio framing, including frames
/// with an unsupported version so callers can discard them instead of parsing
/// them as input events.
pub fn is_audio_message(bytes: &[u8]) -> bool {
    bytes.starts_with(AUDIO_MAGIC)
}

pub fn encode_audio_message(message: &AudioMessage) -> Result<Vec<u8>, ProtocolError> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(AUDIO_MAGIC);
    bytes.push(AUDIO_VERSION);

    match message {
        AudioMessage::Control(control) => {
            bytes.push(CONTROL_KIND);
            bytes.push(match control.role {
                AudioRole::Off => 0,
                AudioRole::Send => 1,
                AudioRole::Receive => 2,
            });
            let flags = u8::from(control.send_system_audio)
                | (u8::from(control.send_microphone) << 1)
                | (u8::from(control.receive_microphone) << 2);
            bytes.push(flags);
        }
        AudioMessage::Data(data) => {
            if data.opus.len() > MAX_AUDIO_DATAGRAM_SIZE - DATA_HEADER_SIZE {
                return Err(ProtocolError::InvalidAudioFrame);
            }
            let payload_len =
                u16::try_from(data.opus.len()).map_err(|_| ProtocolError::InvalidAudioFrame)?;

            bytes.push(DATA_KIND);
            bytes.push(match data.track {
                AudioTrack::System => 0,
                AudioTrack::Microphone => 1,
            });
            bytes.extend_from_slice(&data.sequence.to_be_bytes());
            bytes.extend_from_slice(&data.timestamp_48khz.to_be_bytes());
            bytes.extend_from_slice(&payload_len.to_be_bytes());
            bytes.extend_from_slice(&data.opus);
        }
    }

    if bytes.len() > MAX_AUDIO_DATAGRAM_SIZE {
        return Err(ProtocolError::InvalidAudioFrame);
    }
    Ok(bytes)
}

pub fn decode_audio_message(bytes: &[u8]) -> Result<AudioMessage, ProtocolError> {
    if bytes.len() > MAX_AUDIO_DATAGRAM_SIZE
        || bytes.len() < 6
        || !is_audio_message(bytes)
        || bytes[4] != AUDIO_VERSION
    {
        return Err(ProtocolError::InvalidAudioFrame);
    }

    match bytes[5] {
        CONTROL_KIND if bytes.len() == CONTROL_SIZE => {
            let role = match bytes[6] {
                0 => AudioRole::Off,
                1 => AudioRole::Send,
                2 => AudioRole::Receive,
                _ => return Err(ProtocolError::InvalidAudioFrame),
            };
            let flags = bytes[7];
            if flags & !0b111 != 0 {
                return Err(ProtocolError::InvalidAudioFrame);
            }
            Ok(AudioMessage::Control(AudioControl {
                role,
                send_system_audio: flags & 0b001 != 0,
                send_microphone: flags & 0b010 != 0,
                receive_microphone: flags & 0b100 != 0,
            }))
        }
        DATA_KIND if bytes.len() >= DATA_HEADER_SIZE => {
            let track = match bytes[6] {
                0 => AudioTrack::System,
                1 => AudioTrack::Microphone,
                _ => return Err(ProtocolError::InvalidAudioFrame),
            };
            let sequence = u32::from_be_bytes(
                bytes[7..11]
                    .try_into()
                    .map_err(|_| ProtocolError::InvalidAudioFrame)?,
            );
            let timestamp_48khz = u64::from_be_bytes(
                bytes[11..19]
                    .try_into()
                    .map_err(|_| ProtocolError::InvalidAudioFrame)?,
            );
            let payload_len = u16::from_be_bytes(
                bytes[19..21]
                    .try_into()
                    .map_err(|_| ProtocolError::InvalidAudioFrame)?,
            ) as usize;
            if bytes.len() != DATA_HEADER_SIZE + payload_len {
                return Err(ProtocolError::InvalidAudioFrame);
            }
            Ok(AudioMessage::Data(AudioData {
                track,
                sequence,
                timestamp_48khz,
                opus: bytes[DATA_HEADER_SIZE..].to_vec(),
            }))
        }
        _ => Err(ProtocolError::InvalidAudioFrame),
    }
}
