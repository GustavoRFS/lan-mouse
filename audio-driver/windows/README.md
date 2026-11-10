# Windows virtual microphone driver

The Rust audio service expects a recording endpoint named **Lan Mouse Virtual
Microphone** and opens `\\.\LanMouseVirtualMic`. It sends mono 48 kHz signed
16-bit PCM in 20 ms blocks (960 samples / 1,920 bytes) using
`IOCTL_LAN_MOUSE_MIC_WRITE`. `IOCTL_LAN_MOUSE_MIC_FLUSH` clears pending samples
when a peer stops or disconnects. The shared IOCTL and PCM contract is in
[`lan_mouse_mic_ioctl.h`](lan_mouse_mic_ioctl.h).

The WDM/WaveRT endpoint package is not implemented yet. The current Windows
checkout has the Windows SDK but no WDK, so it cannot build, install, or test a
kernel audio endpoint. The service-side WASAPI backend and IOCTL bridge compile
and their Rust unit tests pass; that does not make the virtual microphone
available to recording applications until a WDK driver implements this
contract.

When implementing the endpoint, start from Microsoft's [SYSVAD virtual audio
driver sample](https://learn.microsoft.com/en-us/samples/microsoft/windows-driver-samples/sysvad-virtual-audio-device-driver-sample/).
Use a test-signed package for development only. Production signing and an
installer are separate distribution work.
