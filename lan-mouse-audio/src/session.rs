use lan_mouse_ipc::{AudioConfig, AudioMode, AudioPeerStatus};
use lan_mouse_proto::{AudioControl, AudioRole, AudioTrack};

#[derive(Clone, Debug)]
pub struct AudioSessionState {
    local: AudioConfig,
    remote: Option<AudioControl>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AudioNegotiation {
    pub status: Option<AudioPeerStatus>,
    pub send_tracks: Vec<AudioTrack>,
    pub receive_tracks: Vec<AudioTrack>,
}

impl AudioSessionState {
    pub fn new(local: AudioConfig) -> Self {
        Self {
            local,
            remote: None,
        }
    }

    pub fn set_local_config(&mut self, local: AudioConfig) {
        self.local = local;
    }

    pub fn observe_remote(&mut self, remote: AudioControl) {
        self.remote = Some(remote);
    }

    pub fn disconnect(&mut self) {
        self.remote = None;
    }

    pub fn negotiation(&self) -> AudioNegotiation {
        if self.local.mode == AudioMode::Off {
            return AudioNegotiation {
                status: Some(AudioPeerStatus::Off),
                ..Default::default()
            };
        }

        let Some(remote) = self.remote.as_ref() else {
            return AudioNegotiation {
                status: Some(AudioPeerStatus::WaitingForPeer),
                ..Default::default()
            };
        };

        let (send_tracks, receive_tracks) = match (self.local.mode, remote.role) {
            (AudioMode::Send, AudioRole::Receive) => {
                let mut tracks = Vec::with_capacity(2);
                if self.local.send_system_audio {
                    tracks.push(AudioTrack::System);
                }
                if self.local.send_microphone && remote.receive_microphone {
                    tracks.push(AudioTrack::Microphone);
                }
                (tracks, Vec::new())
            }
            (AudioMode::Receive, AudioRole::Send) => {
                let mut tracks = Vec::with_capacity(2);
                if remote.send_system_audio {
                    tracks.push(AudioTrack::System);
                }
                if self.local.receive_microphone && remote.send_microphone {
                    tracks.push(AudioTrack::Microphone);
                }
                (Vec::new(), tracks)
            }
            _ => (Vec::new(), Vec::new()),
        };

        let active = matches!(
            (self.local.mode, remote.role),
            (AudioMode::Send, AudioRole::Receive) | (AudioMode::Receive, AudioRole::Send)
        );
        AudioNegotiation {
            status: Some(if active {
                AudioPeerStatus::Active
            } else {
                AudioPeerStatus::WaitingForPeer
            }),
            send_tracks,
            receive_tracks,
        }
    }
}

pub fn audio_control(config: &AudioConfig) -> AudioControl {
    let role = match config.mode {
        AudioMode::Off => AudioRole::Off,
        AudioMode::Send => AudioRole::Send,
        AudioMode::Receive => AudioRole::Receive,
    };
    AudioControl {
        role,
        send_system_audio: config.mode == AudioMode::Send && config.send_system_audio,
        send_microphone: config.mode == AudioMode::Send && config.send_microphone,
        receive_microphone: config.mode == AudioMode::Receive && config.receive_microphone,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn config(mode: AudioMode) -> AudioConfig {
        AudioConfig {
            mode,
            ..AudioConfig::default()
        }
    }

    fn control(role: AudioRole) -> AudioControl {
        AudioControl {
            role,
            send_system_audio: true,
            send_microphone: false,
            receive_microphone: false,
        }
    }

    #[test]
    fn audio_starts_only_for_complementary_peer_modes() {
        let mut sender = AudioSessionState::new(config(AudioMode::Send));
        sender.observe_remote(control(AudioRole::Receive));
        assert_eq!(sender.negotiation().send_tracks, [AudioTrack::System]);
        assert_eq!(sender.negotiation().status, Some(AudioPeerStatus::Active));

        let mut receiver = AudioSessionState::new(config(AudioMode::Receive));
        receiver.observe_remote(control(AudioRole::Send));
        assert_eq!(receiver.negotiation().receive_tracks, [AudioTrack::System]);
        assert_eq!(receiver.negotiation().status, Some(AudioPeerStatus::Active));

        sender.observe_remote(control(AudioRole::Send));
        assert!(sender.negotiation().send_tracks.is_empty());
        assert_eq!(
            sender.negotiation().status,
            Some(AudioPeerStatus::WaitingForPeer)
        );
    }

    #[test]
    fn audio_never_starts_for_old_peer() {
        let session = AudioSessionState::new(config(AudioMode::Send));
        let state = session.negotiation();
        assert!(state.send_tracks.is_empty());
        assert!(state.receive_tracks.is_empty());
        assert_eq!(state.status, Some(AudioPeerStatus::WaitingForPeer));
    }

    #[test]
    fn multiple_audio_peers_have_independent_sessions() {
        let mut peers = HashMap::new();
        peers.insert(1, AudioSessionState::new(config(AudioMode::Send)));
        peers.insert(2, AudioSessionState::new(config(AudioMode::Receive)));
        peers
            .get_mut(&1)
            .unwrap()
            .observe_remote(control(AudioRole::Receive));
        peers
            .get_mut(&2)
            .unwrap()
            .observe_remote(control(AudioRole::Send));

        assert_eq!(peers[&1].negotiation().send_tracks, [AudioTrack::System]);
        assert_eq!(peers[&2].negotiation().receive_tracks, [AudioTrack::System]);
    }

    #[test]
    fn audio_stops_on_disconnect() {
        let mut session = AudioSessionState::new(config(AudioMode::Receive));
        session.observe_remote(control(AudioRole::Send));
        assert_eq!(session.negotiation().status, Some(AudioPeerStatus::Active));
        session.disconnect();
        assert_eq!(
            session.negotiation().status,
            Some(AudioPeerStatus::WaitingForPeer)
        );
        assert!(session.negotiation().receive_tracks.is_empty());
    }

    #[test]
    fn audio_stops_on_role_change() {
        let mut session = AudioSessionState::new(config(AudioMode::Receive));
        session.observe_remote(control(AudioRole::Send));
        assert_eq!(session.negotiation().status, Some(AudioPeerStatus::Active));
        session.set_local_config(config(AudioMode::Send));
        assert_eq!(
            session.negotiation().status,
            Some(AudioPeerStatus::WaitingForPeer)
        );
        assert!(session.negotiation().receive_tracks.is_empty());
    }

    #[test]
    fn system_and_microphone_tracks_remain_separate() {
        let mut local = config(AudioMode::Send);
        local.send_microphone = true;
        let mut session = AudioSessionState::new(local);
        session.observe_remote(AudioControl {
            role: AudioRole::Receive,
            send_system_audio: false,
            send_microphone: false,
            receive_microphone: true,
        });
        assert_eq!(
            session.negotiation().send_tracks,
            [AudioTrack::System, AudioTrack::Microphone]
        );

        let mut local = config(AudioMode::Receive);
        local.receive_microphone = true;
        let mut session = AudioSessionState::new(local);
        session.observe_remote(AudioControl {
            role: AudioRole::Send,
            send_system_audio: true,
            send_microphone: true,
            receive_microphone: false,
        });
        assert_eq!(
            session.negotiation().receive_tracks,
            [AudioTrack::System, AudioTrack::Microphone]
        );
    }
}
