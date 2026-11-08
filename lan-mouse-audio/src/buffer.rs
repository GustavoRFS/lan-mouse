use crate::PcmFrame;
use lan_mouse_proto::AudioTrack;
use std::collections::VecDeque;

pub const PLAYOUT_FRAMES_PER_TRACK: usize = 4;

/// A fixed-capacity playout queue that tracks ordering independently per track.
/// When full, it discards the oldest frame to keep latency bounded.
#[derive(Default)]
pub struct PlayoutBuffer {
    system: TrackBuffer,
    microphone: TrackBuffer,
}

#[derive(Default)]
struct TrackBuffer {
    frames: VecDeque<PcmFrame>,
    latest_sequence: Option<u32>,
}

impl PlayoutBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue a fresh frame. Duplicate and reordered frames are discarded.
    pub fn push(&mut self, frame: PcmFrame) -> bool {
        let track = self.track_mut(frame.track);
        if let Some(previous) = track.latest_sequence {
            if !sequence_is_newer(frame.sequence, previous) {
                return false;
            }
        }

        track.latest_sequence = Some(frame.sequence);
        if track.frames.len() == PLAYOUT_FRAMES_PER_TRACK {
            track.frames.pop_front();
        }
        track.frames.push_back(frame);
        true
    }

    pub fn pop(&mut self, track: AudioTrack) -> Option<PcmFrame> {
        self.track_mut(track).frames.pop_front()
    }

    pub fn len(&self, track: AudioTrack) -> usize {
        self.track(track).frames.len()
    }

    pub fn is_empty(&self, track: AudioTrack) -> bool {
        self.track(track).frames.is_empty()
    }

    pub fn clear(&mut self) {
        self.system = TrackBuffer::default();
        self.microphone = TrackBuffer::default();
    }

    fn track(&self, track: AudioTrack) -> &TrackBuffer {
        match track {
            AudioTrack::System => &self.system,
            AudioTrack::Microphone => &self.microphone,
        }
    }

    fn track_mut(&mut self, track: AudioTrack) -> &mut TrackBuffer {
        match track {
            AudioTrack::System => &mut self.system,
            AudioTrack::Microphone => &mut self.microphone,
        }
    }
}

fn sequence_is_newer(candidate: u32, previous: u32) -> bool {
    let distance = candidate.wrapping_sub(previous);
    distance != 0 && distance < (1 << 31)
}
