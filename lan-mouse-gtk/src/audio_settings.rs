use lan_mouse_ipc::AudioMode;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct AudioSettingsVisibility {
    pub send_system_audio: bool,
    pub system_capture_device: bool,
    pub send_microphone: bool,
    pub microphone_capture_device: bool,
    pub playback_device: bool,
    pub receive_microphone: bool,
}

pub(crate) fn audio_settings_visibility(mode: AudioMode) -> AudioSettingsVisibility {
    match mode {
        AudioMode::Off => AudioSettingsVisibility::default(),
        AudioMode::Send => AudioSettingsVisibility {
            send_system_audio: true,
            system_capture_device: true,
            send_microphone: true,
            microphone_capture_device: true,
            playback_device: false,
            receive_microphone: false,
        },
        AudioMode::Receive => AudioSettingsVisibility {
            send_system_audio: false,
            system_capture_device: false,
            send_microphone: false,
            microphone_capture_device: false,
            playback_device: true,
            receive_microphone: true,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_hides_audio_controls() {
        assert_eq!(
            audio_settings_visibility(AudioMode::Off),
            AudioSettingsVisibility::default()
        );
    }

    #[test]
    fn send_shows_capture_choices() {
        let visibility = audio_settings_visibility(AudioMode::Send);
        assert!(visibility.send_system_audio);
        assert!(visibility.system_capture_device);
        assert!(visibility.send_microphone);
        assert!(visibility.microphone_capture_device);
        assert!(!visibility.playback_device);
        assert!(!visibility.receive_microphone);
    }

    #[test]
    fn receive_shows_playback_and_virtual_mic() {
        let visibility = audio_settings_visibility(AudioMode::Receive);
        assert!(!visibility.send_system_audio);
        assert!(visibility.playback_device);
        assert!(visibility.receive_microphone);
        assert!(!visibility.system_capture_device);
        assert!(!visibility.microphone_capture_device);
    }
}
