use super::*;

fn pcm_frame(track: AudioTrack, sequence: u32) -> PcmFrame {
    let channels = match track {
        AudioTrack::System => 2,
        AudioTrack::Microphone => 1,
    };
    let samples_per_channel = 960;
    let samples = (0..samples_per_channel * channels)
        .map(|index| {
            let phase = (index / channels) as f32 * 440.0 * std::f32::consts::TAU / 48_000.0;
            (phase.sin() * 12_000.0) as i16
        })
        .collect();

    PcmFrame {
        track,
        sequence,
        timestamp_48khz: u64::from(sequence) * 960,
        channels: channels as u8,
        samples,
    }
}

#[test]
fn system_opus_frame_round_trips_stereo_48khz() {
    let frame = pcm_frame(AudioTrack::System, 7);
    let mut encoder = AudioEncoder::new().unwrap();
    let mut decoder = AudioDecoder::new().unwrap();

    let packet = encoder.encode(&frame).unwrap();
    assert!(packet.opus.len() + AUDIO_DATA_HEADER_SIZE <= MAX_AUDIO_DATAGRAM_SIZE);
    let decoded = decoder.decode(&packet).unwrap();

    assert_eq!(decoded.track, AudioTrack::System);
    assert_eq!(decoded.sequence, 7);
    assert_eq!(decoded.timestamp_48khz, 7 * 960);
    assert_eq!(decoded.channels, 2);
    assert_eq!(decoded.samples.len(), 960 * 2);
    assert!(decoded.samples.iter().any(|sample| sample.abs() > 100));
}

#[test]
fn microphone_opus_frame_round_trips_mono_48khz() {
    let frame = pcm_frame(AudioTrack::Microphone, 11);
    let mut encoder = AudioEncoder::new().unwrap();
    let mut decoder = AudioDecoder::new().unwrap();

    let packet = encoder.encode(&frame).unwrap();
    assert!(packet.opus.len() + AUDIO_DATA_HEADER_SIZE <= MAX_AUDIO_DATAGRAM_SIZE);
    let decoded = decoder.decode(&packet).unwrap();

    assert_eq!(decoded.track, AudioTrack::Microphone);
    assert_eq!(decoded.sequence, 11);
    assert_eq!(decoded.timestamp_48khz, 11 * 960);
    assert_eq!(decoded.channels, 1);
    assert_eq!(decoded.samples.len(), 960);
    assert!(decoded.samples.iter().any(|sample| sample.abs() > 100));
}

#[test]
fn playout_queue_is_bounded() {
    let mut queue = PlayoutBuffer::new();
    for sequence in 0..7 {
        assert!(queue.push(pcm_frame(AudioTrack::System, sequence)));
    }

    assert_eq!(queue.len(AudioTrack::System), PLAYOUT_FRAMES_PER_TRACK);
    assert_eq!(queue.pop(AudioTrack::System).unwrap().sequence, 3);
    assert_eq!(queue.pop(AudioTrack::System).unwrap().sequence, 4);
    assert_eq!(queue.pop(AudioTrack::System).unwrap().sequence, 5);
    assert_eq!(queue.pop(AudioTrack::System).unwrap().sequence, 6);
    assert!(queue.pop(AudioTrack::System).is_none());
}

#[test]
fn playout_queue_discards_stale_sequence() {
    let mut queue = PlayoutBuffer::new();
    assert!(queue.push(pcm_frame(AudioTrack::Microphone, 8)));
    assert!(queue.push(pcm_frame(AudioTrack::Microphone, 10)));
    assert!(!queue.push(pcm_frame(AudioTrack::Microphone, 9)));

    assert_eq!(queue.len(AudioTrack::Microphone), 2);
}

#[test]
fn playout_queue_drops_duplicate_or_reordered_sequences() {
    let mut queue = PlayoutBuffer::new();
    assert!(queue.push(pcm_frame(AudioTrack::System, 42)));
    assert!(!queue.push(pcm_frame(AudioTrack::System, 42)));
    assert!(!queue.push(pcm_frame(AudioTrack::System, 41)));

    assert_eq!(queue.len(AudioTrack::System), 1);
}
