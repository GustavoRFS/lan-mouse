use super::error::{EmulationError, WindowsEmulationCreationError};
use super::monitor_position::{MonitorGeometry, resolve_entry_midpoint};
use input_event::{
    BTN_BACK, BTN_FORWARD, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, Event, KeyboardEvent, MonitorId,
    PointerEvent, scancode,
};
use lan_mouse_ipc::Position;

use async_trait::async_trait;
use std::ops::BitOrAssign;
use std::time::Duration;
use tokio::task::AbortHandle;
use windows::Win32::Foundation::FALSE;
use windows::Win32::Graphics::Gdi::{
    DEVMODEW, DISPLAY_DEVICE_ATTACHED_TO_DESKTOP, DISPLAY_DEVICEW, ENUM_CURRENT_SETTINGS,
    EnumDisplayDevicesW, EnumDisplaySettingsW,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE,
    MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
    MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEINPUT,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT_0, KEYEVENTF_EXTENDEDKEY, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, SendInput,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EDD_GET_DEVICE_INTERFACE_NAME, GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
    SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, XBUTTON1, XBUTTON2,
};
use windows::core::PCWSTR;

use super::{Emulation, EmulationHandle};

const DEFAULT_REPEAT_DELAY: Duration = Duration::from_millis(500);
const DEFAULT_REPEAT_INTERVAL: Duration = Duration::from_millis(32);

pub(crate) struct WindowsEmulation {
    repeat_task: Option<AbortHandle>,
}

impl WindowsEmulation {
    pub(crate) fn new() -> Result<Self, WindowsEmulationCreationError> {
        Ok(Self { repeat_task: None })
    }
}

#[async_trait]
impl Emulation for WindowsEmulation {
    async fn consume(&mut self, event: Event, _: EmulationHandle) -> Result<(), EmulationError> {
        match event {
            Event::Pointer(pointer_event) => match pointer_event {
                PointerEvent::Motion { time: _, dx, dy } => {
                    rel_mouse(dx as i32, dy as i32);
                }
                PointerEvent::Button {
                    time: _,
                    button,
                    state,
                } => mouse_button(button, state),
                PointerEvent::Axis {
                    time: _,
                    axis,
                    value,
                } => scroll(axis, value as i32),
                PointerEvent::AxisDiscrete120 { axis, value } => scroll(axis, value),
            },
            Event::Keyboard(keyboard_event) => match keyboard_event {
                KeyboardEvent::Key {
                    time: _,
                    key,
                    state,
                } => {
                    match state {
                        // pressed
                        0 => self.kill_repeat_task(),
                        1 => self.spawn_repeat_task(key).await,
                        _ => {}
                    }
                    key_event(key, state)
                }
                KeyboardEvent::Modifiers { .. } => {}
            },
        }
        // FIXME
        Ok(())
    }

    async fn create(&mut self, _handle: EmulationHandle) {}

    async fn destroy(&mut self, _handle: EmulationHandle) {}

    async fn terminate(&mut self) {}

    async fn enter_monitor(
        &mut self,
        _handle: super::EmulationHandle,
        monitor_id: &MonitorId,
        edge: Position,
    ) -> Result<(), EmulationError> {
        let displays = enumerate_outputs();
        let target = resolve_entry_midpoint(&displays, monitor_id, edge)
            .map_err(|error| EmulationError::MonitorEntry(error.to_string()))?;
        let virtual_x = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
        let virtual_y = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
        let virtual_width = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) };
        let virtual_height = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) };
        if virtual_width <= 0 || virtual_height <= 0 {
            return Err(EmulationError::MonitorEntry(
                "Windows reported an invalid virtual desktop size".to_owned(),
            ));
        }
        let dx = normalize_virtual_coordinate(target.x, virtual_x, virtual_width);
        let dy = normalize_virtual_coordinate(target.y, virtual_y, virtual_height);
        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx,
                    dy,
                    mouseData: 0,
                    dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        let sent = unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) };
        if sent == 0 {
            return Err(EmulationError::MonitorEntry(
                "Windows SendInput rejected absolute monitor entry".to_owned(),
            ));
        }
        Ok(())
    }
}

fn normalize_virtual_coordinate(value: i32, origin: i32, extent: i32) -> i32 {
    if extent <= 1 {
        return 0;
    }
    let relative = i64::from(value) - i64::from(origin);
    ((relative.clamp(0, i64::from(extent - 1)) * 65_535) / i64::from(extent - 1)) as i32
}

fn enumerate_outputs() -> Vec<MonitorGeometry> {
    let mut displays = Vec::new();
    unsafe {
        for index in 0.. {
            let mut display: DISPLAY_DEVICEW = std::mem::zeroed();
            display.cb = std::mem::size_of::<DISPLAY_DEVICEW>() as u32;
            if EnumDisplayDevicesW(None, index, &mut display, EDD_GET_DEVICE_INTERFACE_NAME)
                == FALSE
            {
                break;
            }
            if !display
                .StateFlags
                .contains(DISPLAY_DEVICE_ATTACHED_TO_DESKTOP)
            {
                continue;
            }
            let mut monitor: DISPLAY_DEVICEW = std::mem::zeroed();
            monitor.cb = std::mem::size_of::<DISPLAY_DEVICEW>() as u32;
            let monitor_found = EnumDisplayDevicesW(
                PCWSTR::from_raw(display.DeviceName.as_ptr()),
                0,
                &mut monitor,
                EDD_GET_DEVICE_INTERFACE_NAME,
            ) != FALSE;
            let device_name = wide_string(&display.DeviceName);
            let id = if monitor_found {
                let device_id = wide_string(&monitor.DeviceID);
                MonitorId::new(device_id).or_else(|_| MonitorId::new(device_name.clone()))
            } else {
                MonitorId::new(device_name.clone())
            };
            let Ok(id) = id else { continue };

            let mut mode: DEVMODEW = std::mem::zeroed();
            mode.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
            if EnumDisplaySettingsW(
                PCWSTR::from_raw(display.DeviceName.as_ptr()),
                ENUM_CURRENT_SETTINGS,
                &mut mode,
            ) == FALSE
            {
                continue;
            }
            let position = mode.Anonymous1.Anonymous2.dmPosition;
            displays.push(MonitorGeometry {
                id,
                x: position.x,
                y: position.y,
                width: mode.dmPelsWidth,
                height: mode.dmPelsHeight,
            });
        }
    }
    displays
}

fn wide_string(value: &[u16]) -> String {
    let end = value
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(value.len());
    String::from_utf16_lossy(&value[..end])
}

impl WindowsEmulation {
    async fn spawn_repeat_task(&mut self, key: u32) {
        // there can only be one repeating key and it's
        // always the last to be pressed
        self.kill_repeat_task();
        let repeat_task = tokio::task::spawn_local(async move {
            tokio::time::sleep(DEFAULT_REPEAT_DELAY).await;
            loop {
                key_event(key, 1);
                tokio::time::sleep(DEFAULT_REPEAT_INTERVAL).await;
            }
        });
        self.repeat_task = Some(repeat_task.abort_handle());
    }
    fn kill_repeat_task(&mut self) {
        if let Some(task) = self.repeat_task.take() {
            task.abort();
        }
    }
}

fn send_input_safe(input: INPUT) {
    unsafe {
        loop {
            /* retval = number of successfully submitted events */
            if SendInput(&[input], std::mem::size_of::<INPUT>() as i32) > 0 {
                break;
            }
        }
    }
}

fn send_mouse_input(mi: MOUSEINPUT) {
    send_input_safe(INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 { mi },
    });
}

fn send_keyboard_input(ki: KEYBDINPUT) {
    send_input_safe(INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki },
    });
}
fn rel_mouse(dx: i32, dy: i32) {
    let mi = MOUSEINPUT {
        dx,
        dy,
        mouseData: 0,
        dwFlags: MOUSEEVENTF_MOVE,
        time: 0,
        dwExtraInfo: 0,
    };
    send_mouse_input(mi);
}

fn mouse_button(button: u32, state: u32) {
    let dw_flags = match state {
        0 => match button {
            BTN_LEFT => MOUSEEVENTF_LEFTUP,
            BTN_RIGHT => MOUSEEVENTF_RIGHTUP,
            BTN_MIDDLE => MOUSEEVENTF_MIDDLEUP,
            BTN_BACK => MOUSEEVENTF_XUP,
            BTN_FORWARD => MOUSEEVENTF_XUP,
            _ => return,
        },
        1 => match button {
            BTN_LEFT => MOUSEEVENTF_LEFTDOWN,
            BTN_RIGHT => MOUSEEVENTF_RIGHTDOWN,
            BTN_MIDDLE => MOUSEEVENTF_MIDDLEDOWN,
            BTN_BACK => MOUSEEVENTF_XDOWN,
            BTN_FORWARD => MOUSEEVENTF_XDOWN,
            _ => return,
        },
        _ => return,
    };
    let mouse_data = match button {
        BTN_BACK => XBUTTON1 as u32,
        BTN_FORWARD => XBUTTON2 as u32,
        _ => 0,
    };
    let mi = MOUSEINPUT {
        dx: 0,
        dy: 0, // no movement
        mouseData: mouse_data,
        dwFlags: dw_flags,
        time: 0,
        dwExtraInfo: 0,
    };
    send_mouse_input(mi);
}

fn scroll(axis: u8, value: i32) {
    // WHEEL is positive up but HWHEEL positive right, like lan-mouse's horizontal axis
    let (event_type, value) = match axis {
        0 => (MOUSEEVENTF_WHEEL, value.saturating_neg()),
        1 => (MOUSEEVENTF_HWHEEL, value),
        _ => return,
    };
    let mi = MOUSEINPUT {
        dx: 0,
        dy: 0,
        mouseData: value as u32,
        dwFlags: event_type,
        time: 0,
        dwExtraInfo: 0,
    };
    send_mouse_input(mi);
}

fn key_event(key: u32, state: u8) {
    let scancode = match linux_keycode_to_windows_scancode(key) {
        Some(code) => code,
        None => return,
    };
    let extended = scancode > 0xff;
    let scancode = scancode & 0xff;
    let mut flags = KEYEVENTF_SCANCODE;
    if extended {
        flags.bitor_assign(KEYEVENTF_EXTENDEDKEY);
    }
    if state == 0 {
        flags.bitor_assign(KEYEVENTF_KEYUP);
    }
    let ki = KEYBDINPUT {
        wVk: Default::default(),
        wScan: scancode,
        dwFlags: flags,
        time: 0,
        dwExtraInfo: 0,
    };
    send_keyboard_input(ki);
}

fn linux_keycode_to_windows_scancode(linux_keycode: u32) -> Option<u16> {
    let linux_scancode = match scancode::Linux::try_from(linux_keycode) {
        Ok(s) => s,
        Err(_) => {
            log::warn!("unknown keycode: {linux_keycode}");
            return None;
        }
    };
    log::trace!("linux code: {linux_scancode:?}");
    let windows_scancode = match scancode::Windows::try_from(linux_scancode) {
        Ok(s) => s,
        Err(_) => {
            log::warn!("failed to translate linux code into windows scancode: {linux_scancode:?}");
            return None;
        }
    };
    log::trace!("windows code: {windows_scancode:?}");
    Some(windows_scancode as u16)
}
