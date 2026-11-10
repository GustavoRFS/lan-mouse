use crate::{AudioError, PcmFrame, SAMPLES_PER_FRAME};
use lan_mouse_proto::AudioTrack;
use std::{
    collections::VecDeque,
    fs::{File, OpenOptions},
    os::windows::io::AsRawHandle,
};
use windows_sys::Win32::{Foundation::HANDLE, System::IO::DeviceIoControl};

const DEVICE_PATH: &str = r"\\.\LanMouseVirtualMic";
const DRIVER_QUEUE_CAPACITY: usize = 4;
const PCM_BYTES: usize = SAMPLES_PER_FRAME * 2;

// FILE_DEVICE_UNKNOWN, METHOD_BUFFERED, FILE_ANY_ACCESS, function 0x800/0x801.
const IOCTL_LAN_MOUSE_MIC_WRITE: u32 = (0x22 << 16) | (0x800 << 2);
const IOCTL_LAN_MOUSE_MIC_FLUSH: u32 = (0x22 << 16) | (0x801 << 2);

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct DriverPcmBlock {
    bytes: Vec<u8>,
}

impl DriverPcmBlock {
    pub(super) fn from_frame(frame: &PcmFrame) -> Result<Self, AudioError> {
        if frame.track != AudioTrack::Microphone
            || frame.channels != 1
            || frame.samples.len() != SAMPLES_PER_FRAME
        {
            return Err(AudioError::InvalidPcmFrame {
                track: frame.track,
                channels: frame.channels,
                samples: frame.samples.len(),
                expected_channels: 1,
                expected_samples: SAMPLES_PER_FRAME,
            });
        }

        let mut bytes = Vec::with_capacity(PCM_BYTES);
        for sample in &frame.samples {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        Ok(Self { bytes })
    }
}

#[derive(Default)]
pub(super) struct DriverQueue {
    blocks: VecDeque<DriverPcmBlock>,
}

impl DriverQueue {
    pub(super) fn push(&mut self, frame: &PcmFrame) -> Result<(), AudioError> {
        let block = DriverPcmBlock::from_frame(frame)?;
        if self.blocks.len() == DRIVER_QUEUE_CAPACITY {
            self.blocks.pop_front();
        }
        self.blocks.push_back(block);
        Ok(())
    }

    pub(super) fn pop(&mut self) -> Option<DriverPcmBlock> {
        self.blocks.pop_front()
    }

    pub(super) fn flush(&mut self) {
        self.blocks.clear();
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.blocks.len()
    }
}

pub(super) struct WindowsVirtualMic {
    device: File,
}

impl WindowsVirtualMic {
    pub(super) fn open() -> Result<Self, AudioError> {
        let device = OpenOptions::new()
            .read(true)
            .write(true)
            .open(DEVICE_PATH)
            .map_err(|error| AudioError::DeviceUnavailable(format!("{DEVICE_PATH}: {error}")))?;
        Ok(Self { device })
    }

    pub(super) fn write(&self, block: &DriverPcmBlock) -> Result<(), AudioError> {
        if block.bytes.len() != PCM_BYTES {
            return Err(AudioError::Backend(format!(
                "invalid virtual microphone block size: {} bytes",
                block.bytes.len()
            )));
        }
        self.ioctl(IOCTL_LAN_MOUSE_MIC_WRITE, Some(&block.bytes))
    }

    pub(super) fn flush(&self) -> Result<(), AudioError> {
        self.ioctl(IOCTL_LAN_MOUSE_MIC_FLUSH, None)
    }

    fn ioctl(&self, code: u32, input: Option<&[u8]>) -> Result<(), AudioError> {
        let (input_ptr, input_len) = input.map_or((std::ptr::null(), 0), |bytes| {
            (bytes.as_ptr().cast(), bytes.len() as u32)
        });
        let mut returned = 0;
        // SAFETY: the file handle is open for the duration of the call, and the optional input
        // slice remains valid for its complete lifetime. This IOCTL contract has no output buffer.
        let ok = unsafe {
            DeviceIoControl(
                self.device.as_raw_handle() as HANDLE,
                code,
                input_ptr,
                input_len,
                std::ptr::null_mut(),
                0,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(AudioError::Backend(format!(
                "virtual microphone IOCTL 0x{code:08x} failed: {}",
                std::io::Error::last_os_error()
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn microphone_frame(sequence: u32, sample: i16) -> PcmFrame {
        PcmFrame {
            track: AudioTrack::Microphone,
            sequence,
            timestamp_48khz: 0,
            channels: 1,
            samples: vec![sample; SAMPLES_PER_FRAME],
        }
    }

    #[test]
    fn driver_pcm_block_rejects_wrong_format() {
        let mut frame = microphone_frame(0, 0);
        frame.channels = 2;
        assert!(DriverPcmBlock::from_frame(&frame).is_err());

        let mut frame = microphone_frame(0, 0);
        frame.samples.pop();
        assert!(DriverPcmBlock::from_frame(&frame).is_err());

        let block = DriverPcmBlock::from_frame(&microphone_frame(0, -1234)).unwrap();
        assert_eq!(block.bytes.len(), PCM_BYTES);
        assert_eq!(&block.bytes[..2], &(-1234i16).to_le_bytes());
    }

    #[test]
    fn driver_queue_is_bounded() {
        let mut queue = DriverQueue::default();
        for sequence in 0..(DRIVER_QUEUE_CAPACITY as u32 + 2) {
            queue
                .push(&microphone_frame(sequence, sequence as i16))
                .unwrap();
        }

        assert_eq!(queue.len(), DRIVER_QUEUE_CAPACITY);
        let first = queue.pop().unwrap();
        assert_eq!(first.bytes[..2], 2i16.to_le_bytes());
    }

    #[test]
    fn driver_queue_flushes_on_stop() {
        let mut queue = DriverQueue::default();
        queue.push(&microphone_frame(0, 1)).unwrap();
        queue.push(&microphone_frame(1, 2)).unwrap();

        queue.flush();

        assert!(queue.pop().is_none());
    }
}
