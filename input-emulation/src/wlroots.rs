use crate::error::EmulationError;

use super::{Emulation, EmulationOptions, error::WlrootsEmulationCreationError};
use async_trait::async_trait;
use bitflags::bitflags;
use rustix::fs::{MemfdFlags, memfd_create};
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;
use wayland_client::WEnum;
use wayland_client::backend::WaylandError;

use wayland_client::protocol::wl_keyboard::{self, WlKeyboard};
use wayland_client::protocol::wl_output::{self, WlOutput};
use wayland_client::protocol::wl_pointer::{Axis, AxisSource, ButtonState};
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_protocols::xdg::xdg_output::zv1::client::{
    zxdg_output_manager_v1::ZxdgOutputManagerV1,
    zxdg_output_v1::{self, ZxdgOutputV1},
};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1 as VpManager,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1 as Vp,
};

use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1 as VkManager,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1 as Vk,
};

use xkbcommon::xkb;

use wayland_client::{
    Connection, Dispatch, EventQueue, QueueHandle, delegate_noop,
    globals::{Global, GlobalList, GlobalListContents, registry_queue_init},
    protocol::{wl_registry, wl_seat},
};

use super::monitor_position::{MonitorGeometry, resolve_entry_midpoint};
use input_event::{Event, KeyboardEvent, MonitorId, PointerEvent, scancode};
use lan_mouse_ipc::Position;

use super::EmulationHandle;
use super::error::WaylandBindError;

/// What `zwp_virtual_keyboard_v1::keymap` takes: the format, a file holding the keymap and its size.
type Keymap = (u32, OwnedFd, u32);

struct State {
    /// the compositor's keymap, as of the last time it sent one
    keymap: Option<Keymap>,
    /// keymap configured on the receiving machine, which is the fallback for remote layouts
    configured_keymap: Option<Keymap>,
    /// keymap currently selected by the sending machine
    fixed_keymap: Option<Keymap>,
    /// shared by all clients, so the keymap fd is sent once instead of once per client
    keyboard: Option<Vk>,
    input_for_client: HashMap<EmulationHandle, VirtualInput>,
    seat: wl_seat::WlSeat,
    qh: QueueHandle<Self>,
    vpm: VpManager,
    vkm: VkManager,
    global_list: GlobalList,
    xdg_output_manager: ZxdgOutputManagerV1,
    outputs: Vec<Output>,
    last_pointer_handle: Option<EmulationHandle>,
}

#[derive(Clone, Debug, Default)]
struct OutputInfo {
    name: String,
    position: (i32, i32),
    size: (i32, i32),
}

struct Output {
    global: Global,
    wl_output: WlOutput,
    info: Option<OutputInfo>,
    pending_info: OutputInfo,
    has_xdg_info: bool,
}

// App State, implements Dispatch event handlers
pub(crate) struct WlrootsEmulation {
    last_flush_failed: bool,
    state: State,
    queue: EventQueue<State>,
}

impl WlrootsEmulation {
    pub(crate) fn new(options: &EmulationOptions) -> Result<Self, WlrootsEmulationCreationError> {
        let conn = Connection::connect_to_env()?;
        let (globals, queue) = registry_queue_init::<State>(&conn)?;
        let qh = queue.handle();

        let seat: wl_seat::WlSeat = globals
            .bind(&qh, 7..=8, ())
            .map_err(|e| WaylandBindError::new(e, "wl_seat 7..=8"))?;

        let vpm: VpManager = globals
            .bind(&qh, 1..=1, ())
            .map_err(|e| WaylandBindError::new(e, "wlr-virtual-pointer-unstable-v1"))?;
        let vkm: VkManager = globals
            .bind(&qh, 1..=1, ())
            .map_err(|e| WaylandBindError::new(e, "virtual-keyboard-unstable-v1"))?;
        let xdg_output_manager: ZxdgOutputManagerV1 = globals
            .bind(&qh, 1..=3, ())
            .map_err(|e| WaylandBindError::new(e, "xdg_output_manager 1..=3"))?;

        let input_for_client: HashMap<EmulationHandle, VirtualInput> = HashMap::new();
        let configured_keymap = options.keyboard_layout.as_deref().and_then(|layout| {
            let variant = options.keyboard_variant.as_deref().unwrap_or_default();
            match fixed_keymap(layout, variant) {
                Ok(keymap) => {
                    log::info!("virtual keyboard uses the fixed layout `{layout}` `{variant}`");
                    Some(keymap)
                }
                Err(e) => {
                    log::error!("{e}, using the compositor's keymap instead");
                    None
                }
            }
        });

        let mut emulate = WlrootsEmulation {
            last_flush_failed: false,
            state: State {
                keymap: None,
                configured_keymap,
                fixed_keymap: None,
                keyboard: None,
                input_for_client,
                seat,
                vpm,
                vkm,
                global_list: globals,
                xdg_output_manager,
                outputs: Vec::new(),
                last_pointer_handle: None,
                qh,
            },
            queue,
        };
        for global in emulate.state.global_list.contents().clone_list() {
            emulate.state.register_global(global);
        }
        emulate.queue.flush()?;
        while emulate.state.keymap.is_none() && emulate.state.configured_keymap.is_none() {
            emulate.queue.blocking_dispatch(&mut emulate.state)?;
        }
        // let fd = unsafe { &File::from_raw_fd(emulate.state.keymap.unwrap().1.as_raw_fd()) };
        // let mmap = unsafe { MmapOptions::new().map_copy(fd).unwrap() };
        // log::debug!("{:?}", &mmap[..100]);
        Ok(emulate)
    }
}

impl State {
    fn register_global(&mut self, global: Global) {
        if global.interface.as_str() != "wl_output" {
            return;
        }
        let version = global.version.min(4);
        let output = self.global_list.registry().bind::<WlOutput, _, _>(
            global.name,
            version,
            &self.qh,
            global.name,
        );
        self.xdg_output_manager
            .get_xdg_output(&output, &self.qh, global.name);
        self.outputs.push(Output {
            global,
            wl_output: output,
            info: None,
            pending_info: OutputInfo::default(),
            has_xdg_info: false,
        });
    }

    fn deregister_global(&mut self, name: u32) {
        self.outputs.retain(|output| {
            if output.global.name == name {
                output.wl_output.release();
                false
            } else {
                true
            }
        });
    }

    fn update_output_info(&mut self, name: u32) {
        if let Some(output) = self.outputs.iter_mut().find(|o| o.global.name == name) {
            if output.has_xdg_info {
                output.info = Some(output.pending_info.clone());
            }
        }
    }

    fn monitor_geometries(&self) -> Vec<MonitorGeometry> {
        self.outputs
            .iter()
            .filter_map(|output| {
                let info = output.info.as_ref()?;
                Some(MonitorGeometry {
                    id: MonitorId::new(info.name.clone()).ok()?,
                    x: info.position.0,
                    y: info.position.1,
                    width: u32::try_from(info.size.0).ok()?,
                    height: u32::try_from(info.size.1).ok()?,
                })
            })
            .collect()
    }

    fn add_client(&mut self, client: EmulationHandle) {
        let pointer: Vp = self.vpm.create_virtual_pointer(None, &self.qh, ());

        let keyboard = match self.keyboard.as_ref() {
            Some(keyboard) => keyboard.clone(),
            None => {
                let keyboard: Vk = self.vkm.create_virtual_keyboard(&self.seat, &self.qh, ());
                // TODO: use server side keymap
                let Some((format, fd, size)) = self
                    .fixed_keymap
                    .as_ref()
                    .or(self.configured_keymap.as_ref())
                    .or(self.keymap.as_ref())
                else {
                    panic!("no keymap");
                };
                keyboard.keymap(*format, fd.as_fd(), *size);
                self.keyboard = Some(keyboard.clone());
                keyboard
            }
        };

        let vinput = VirtualInput {
            pointer,
            keyboard,
            modifiers: Arc::new(Mutex::new(XMods::empty())),
        };

        self.input_for_client.insert(client, vinput);
        self.last_pointer_handle = Some(client);
    }

    fn destroy_client(&mut self, handle: EmulationHandle) {
        // the shared keyboard outlives every client; keys are released by InputEmulation::destroy
        if let Some(input) = self.input_for_client.remove(&handle) {
            input.pointer.destroy();
        }
    }
}

#[async_trait]
impl Emulation for WlrootsEmulation {
    async fn consume(
        &mut self,
        event: Event,
        handle: EmulationHandle,
    ) -> Result<(), EmulationError> {
        if let Some(virtual_input) = self.state.input_for_client.get(&handle) {
            if self.last_flush_failed {
                match self.queue.flush() {
                    Err(WaylandError::Io(e)) if e.kind() == io::ErrorKind::WouldBlock => {
                        /*
                         * outgoing buffer is full - sending more events
                         * will overwhelm the output buffer and leave the
                         * wayland connection in a broken state
                         */
                        log::warn!("can't keep up, discarding event: ({handle}) - {event:?}");
                        return Ok(());
                    }
                    _ => {}
                }
            }
            virtual_input
                .consume_event(event)
                .unwrap_or_else(|_| panic!("failed to convert event: {event:?}"));
            match self.queue.flush() {
                Err(WaylandError::Io(e)) if e.kind() == io::ErrorKind::WouldBlock => {
                    self.last_flush_failed = true;
                    log::warn!("can't keep up, discarding event: ({handle}) - {event:?}");
                }
                Err(WaylandError::Protocol(e)) => panic!("wayland protocol violation: {e}"),
                Ok(()) => self.last_flush_failed = false,
                Err(e) => Err(e)?,
            }
        }
        Ok(())
    }

    async fn create(&mut self, handle: EmulationHandle) {
        self.state.add_client(handle);
        if let Err(e) = self.queue.flush() {
            log::error!("{e}");
        }
    }
    async fn destroy(&mut self, handle: EmulationHandle) {
        self.state.destroy_client(handle);
        if self.state.last_pointer_handle == Some(handle) {
            self.state.last_pointer_handle = self.state.input_for_client.keys().next().copied();
        }
        if let Err(e) = self.queue.flush() {
            log::error!("{e}");
        }
    }
    fn select_handle(&mut self, handle: EmulationHandle) {
        if self.state.input_for_client.contains_key(&handle) {
            self.state.last_pointer_handle = Some(handle);
        }
    }
    async fn terminate(&mut self) {
        /* nothing to do */
    }

    async fn enter_monitor(
        &mut self,
        _handle: EmulationHandle,
        monitor_id: &MonitorId,
        edge: Position,
    ) -> Result<(), EmulationError> {
        let geometries = self.state.monitor_geometries();
        let target = resolve_entry_midpoint(&geometries, monitor_id, edge)
            .map_err(|error| EmulationError::MonitorEntry(error.to_string()))?;
        let min_x = geometries.iter().map(|output| output.x).min().unwrap_or(0);
        let min_y = geometries.iter().map(|output| output.y).min().unwrap_or(0);
        let max_x = geometries
            .iter()
            .map(|output| output.x.saturating_add(output.width as i32))
            .max()
            .unwrap_or(0);
        let max_y = geometries
            .iter()
            .map(|output| output.y.saturating_add(output.height as i32))
            .max()
            .unwrap_or(0);
        let width = max_x.saturating_sub(min_x).max(1) as u32;
        let height = max_y.saturating_sub(min_y).max(1) as u32;
        let Some(handle) = self.state.last_pointer_handle else {
            return Err(EmulationError::MonitorEntry(
                "no virtual pointer is available for the peer".to_owned(),
            ));
        };
        let Some(input) = self.state.input_for_client.get(&handle) else {
            return Err(EmulationError::MonitorEntry(
                "no virtual pointer is available for the peer".to_owned(),
            ));
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u32;
        input.pointer.motion_absolute(
            now,
            target.x.saturating_sub(min_x).max(0) as u32,
            target.y.saturating_sub(min_y).max(0) as u32,
            width,
            height,
        );
        input.pointer.frame();
        self.queue.flush()?;
        Ok(())
    }

    async fn set_keyboard_layout(
        &mut self,
        layout: Option<&str>,
        variant: Option<&str>,
    ) -> Result<(), EmulationError> {
        let fixed_keymap = match layout {
            Some(layout) => match fixed_keymap(layout, variant.unwrap_or_default()) {
                Ok(keymap) => Some(keymap),
                Err(KeymapError::Unknown { layout, variant }) => {
                    log::error!("xkb has no keymap for layout `{layout}` with variant `{variant}`");
                    return Ok(());
                }
                Err(KeymapError::Io(error)) => return Err(error.into()),
            },
            None => None,
        };

        let Some((format, fd, size)) = fixed_keymap
            .as_ref()
            .or(self.state.configured_keymap.as_ref())
            .or(self.state.keymap.as_ref())
        else {
            log::error!("no keymap is available for the virtual keyboard");
            return Ok(());
        };
        if let Some(keyboard) = self.state.keyboard.as_ref() {
            keyboard.keymap(*format, fd.as_fd(), *size);
            self.queue.flush()?;
        }
        self.state.fixed_keymap = fixed_keymap;
        Ok(())
    }
}

#[derive(Debug, Error)]
enum KeymapError {
    #[error("xkb has no keymap for layout `{layout}` with variant `{variant}`")]
    Unknown { layout: String, variant: String },
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// A keymap holding just this layout, which makes it group 0 for good.
fn fixed_keymap(layout: &str, variant: &str) -> Result<Keymap, KeymapError> {
    let text = keymap_text(layout, variant).ok_or_else(|| KeymapError::Unknown {
        layout: layout.to_owned(),
        variant: variant.to_owned(),
    })?;
    Ok(keymap_file(&text)?)
}

fn keymap_text(layout: &str, variant: &str) -> Option<String> {
    let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    let keymap = xkb::Keymap::new_from_names(
        &context,
        "",
        "",
        layout,
        variant,
        None,
        xkb::KEYMAP_COMPILE_NO_FLAGS,
    )?;
    Some(keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1))
}

/// The compositor reads the keymap as a C string, and the protocol counts the terminating NUL
/// in the size.
fn keymap_file(text: &str) -> io::Result<Keymap> {
    let mut file = File::from(memfd_create("lan-mouse-keymap", MemfdFlags::CLOEXEC)?);
    file.write_all(text.as_bytes())?;
    file.write_all(&[0])?;
    let size = u32::try_from(text.len() + 1).map_err(io::Error::other)?;
    Ok((
        u32::from(wl_keyboard::KeymapFormat::XkbV1),
        file.into(),
        size,
    ))
}

struct VirtualInput {
    pointer: Vp,
    keyboard: Vk,
    modifiers: Arc<Mutex<XMods>>,
}

impl VirtualInput {
    fn consume_event(&self, event: Event) -> Result<(), ()> {
        // the peer's timestamps use its own clock and unit (libei sends microseconds,
        // windows and macos send 0); clients compare them to detect double clicks
        let now: u32 = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u32;

        match event {
            Event::Pointer(e) => {
                match e {
                    PointerEvent::Motion { dx, dy, .. } => self.pointer.motion(now, dx, dy),
                    PointerEvent::Button { button, state, .. } => {
                        let state: ButtonState = state.try_into()?;
                        self.pointer.button(now, button, state);
                    }
                    PointerEvent::Axis { axis, value, .. } => {
                        let axis: Axis = (axis as u32).try_into()?;
                        self.pointer.axis(now, axis, value);
                        self.pointer.axis_source(AxisSource::Continuous);
                        self.pointer.frame();
                    }
                    PointerEvent::AxisDiscrete120 { axis, value } => {
                        let axis: Axis = (axis as u32).try_into()?;
                        self.pointer
                            .axis_discrete(now, axis, value as f64 / 8., value / 120);
                        self.pointer.axis_source(AxisSource::Wheel);
                        self.pointer.frame();
                    }
                }
                self.pointer.frame();
            }
            Event::Keyboard(e) => match e {
                KeyboardEvent::Key { key, state, .. } => {
                    self.keyboard.key(now, key, state as u32);
                    if let Ok(mut mods) = self.modifiers.lock() {
                        if mods.update_by_key_event(key, state) {
                            log::trace!("Key triggers modifier change: {mods:?}");
                            self.keyboard.modifiers(
                                mods.mask_pressed().bits(),
                                0,
                                mods.mask_locks().bits(),
                                0,
                            );
                        }
                    }
                }
                KeyboardEvent::Modifiers {
                    depressed: mods_depressed,
                    latched: mods_latched,
                    locked: mods_locked,
                    group,
                } => {
                    // Synchronize internal modifier state, assuming server is authoritative
                    if let Ok(mut mods) = self.modifiers.lock() {
                        mods.update_by_mods_event(e);
                    }
                    self.keyboard
                        .modifiers(mods_depressed, mods_latched, mods_locked, group);
                }
            },
        }
        Ok(())
    }
}

delegate_noop!(State: Vp);
delegate_noop!(State: Vk);
delegate_noop!(State: VpManager);
delegate_noop!(State: VkManager);
delegate_noop!(State: ZxdgOutputManagerV1);

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        state: &mut State,
        _: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => {
                state.register_global(Global {
                    name,
                    interface,
                    version,
                });
            }
            wl_registry::Event::GlobalRemove { name } => state.deregister_global(name),
            _ => {}
        }
    }
}

impl Dispatch<ZxdgOutputV1, u32> for State {
    fn event(
        state: &mut State,
        _: &ZxdgOutputV1,
        event: zxdg_output_v1::Event,
        name: &u32,
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
        let Some(output) = state.outputs.iter_mut().find(|o| o.global.name == *name) else {
            return;
        };
        match event {
            zxdg_output_v1::Event::LogicalPosition { x, y } => {
                output.pending_info.position = (x, y);
                output.has_xdg_info = true;
            }
            zxdg_output_v1::Event::LogicalSize { width, height } => {
                output.pending_info.size = (width, height);
                output.has_xdg_info = true;
            }
            zxdg_output_v1::Event::Name { name } => {
                output.pending_info.name = name;
                output.has_xdg_info = true;
            }
            zxdg_output_v1::Event::Description { .. } => {}
            zxdg_output_v1::Event::Done => state.update_output_info(*name),
            _ => {}
        }
    }
}

impl Dispatch<WlOutput, u32> for State {
    fn event(
        state: &mut State,
        _: &WlOutput,
        event: wl_output::Event,
        name: &u32,
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
        if let wl_output::Event::Done = event {
            state.update_output_info(*name);
        }
    }
}

delegate_noop!(State: ZxdgOutputV1);

impl Dispatch<WlKeyboard, ()> for State {
    fn event(
        state: &mut Self,
        _: &WlKeyboard,
        event: <WlKeyboard as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_keyboard::Event::Keymap { format, fd, size } = event {
            state.keymap = Some((u32::from(format), fd, size));
        }
    }
}

impl Dispatch<WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        seat: &WlSeat,
        event: <WlSeat as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        qhandle: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(capabilities),
        } = event
        {
            if capabilities.contains(wl_seat::Capability::Keyboard) {
                seat.get_keyboard(qhandle, ());
            }
        }
    }
}

// From X11/X.h
bitflags! {
    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
    struct XMods: u32 {
        const ShiftMask = (1<<0);
        const LockMask = (1<<1);
        const ControlMask = (1<<2);
        const Mod1Mask = (1<<3);
        const Mod2Mask = (1<<4);
        const Mod3Mask = (1<<5);
        const Mod4Mask = (1<<6);
        const Mod5Mask = (1<<7);
    }
}

impl XMods {
    fn update_by_mods_event(&mut self, evt: KeyboardEvent) {
        if let KeyboardEvent::Modifiers {
            depressed, locked, ..
        } = evt
        {
            *self = XMods::from_bits_truncate(depressed) | XMods::from_bits_truncate(locked);
        }
    }

    fn update_by_key_event(&mut self, key: u32, state: u8) -> bool {
        if let Ok(key) = scancode::Linux::try_from(key) {
            log::trace!("Attempting to process modifier from: {key:#?}");
            let pressed_mask = match key {
                scancode::Linux::KeyLeftShift | scancode::Linux::KeyRightShift => XMods::ShiftMask,
                scancode::Linux::KeyLeftCtrl | scancode::Linux::KeyRightCtrl => XMods::ControlMask,
                scancode::Linux::KeyLeftAlt | scancode::Linux::KeyRightalt => XMods::Mod1Mask,
                scancode::Linux::KeyLeftMeta | scancode::Linux::KeyRightmeta => XMods::Mod4Mask,
                _ => XMods::empty(),
            };

            let locked_mask = match key {
                scancode::Linux::KeyCapsLock => XMods::LockMask,
                scancode::Linux::KeyNumlock => XMods::Mod2Mask,
                scancode::Linux::KeyScrollLock => XMods::Mod3Mask,
                _ => XMods::empty(),
            };

            // unchanged
            if pressed_mask.is_empty() && locked_mask.is_empty() {
                log::trace!("{key:#?} is not a modifier key");
                return false;
            }
            match state {
                1 => self.insert(pressed_mask),
                _ => {
                    self.remove(pressed_mask);
                    self.toggle(locked_mask);
                }
            }
            true
        } else {
            false
        }
    }

    fn mask_locks(&self) -> XMods {
        *self & (XMods::LockMask | XMods::Mod2Mask | XMods::Mod3Mask)
    }

    fn mask_pressed(&self) -> XMods {
        *self & (XMods::ShiftMask | XMods::ControlMask | XMods::Mod1Mask | XMods::Mod4Mask)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Seek, SeekFrom};

    /// evdev's KEY_SEMICOLON, the key right of L, plus the offset xkb adds to evdev codes
    const KEY_RIGHT_OF_L: u32 = 39 + 8;

    fn keymap_of(text: &str) -> xkb::Keymap {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        xkb::Keymap::new_from_string(
            &context,
            text.to_owned(),
            xkb::KEYMAP_FORMAT_TEXT_V1,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .expect("the keymap parses back")
    }

    fn keysym_right_of_l(layout: &str) -> String {
        let keymap = keymap_of(&keymap_text(layout, "").expect("layout exists"));
        let state = xkb::State::new(&keymap);
        xkb::keysym_get_name(state.key_get_one_sym(KEY_RIGHT_OF_L.into()))
    }

    #[test]
    fn spanish_types_enye_where_abnt2_types_c_cedilla() {
        assert_eq!(keysym_right_of_l("es"), "ntilde");
        assert_eq!(keysym_right_of_l("br"), "ccedilla");
    }

    #[test]
    fn fixed_keymap_holds_a_single_layout() {
        let keymap = keymap_of(&keymap_text("es", "").unwrap());
        assert_eq!(keymap.num_layouts(), 1);
        assert_eq!(keymap_of(&keymap_text("br", "").unwrap()).num_layouts(), 1);
    }

    #[test]
    fn variant_is_applied() {
        assert!(keymap_text("es", "dvorak").is_some());
        assert!(keymap_text("es", "no-such-variant").is_none());
    }

    #[test]
    fn unknown_layout_has_no_keymap() {
        assert!(keymap_text("zz", "").is_none());
        assert!(matches!(
            fixed_keymap("zz", ""),
            Err(KeymapError::Unknown { .. })
        ));
    }

    #[test]
    fn keymap_file_is_nul_terminated() {
        let (format, fd, size) = keymap_file("xkb_keymap {}").unwrap();
        assert_eq!(format, 1, "xkb_v1");
        assert_eq!(size, 14);

        let mut file = File::from(fd);
        file.seek(SeekFrom::Start(0)).unwrap();
        let mut contents = Vec::new();
        file.read_to_end(&mut contents).unwrap();
        assert_eq!(contents, b"xkb_keymap {}\0");
    }
}
