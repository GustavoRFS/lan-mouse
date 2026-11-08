use std::cell::{Cell, RefCell};

use adw::subclass::prelude::*;
use adw::{ActionRow, ComboRow, prelude::*};
use glib::{Binding, subclass::InitializingObject};
use gtk::glib::subclass::Signal;
use gtk::glib::{SignalHandlerId, clone};
use gtk::{Button, CompositeTemplate, Entry, Switch, glib};
use lan_mouse_ipc::{
    AudioConfig, AudioDevice, AudioDeviceDirection, AudioMode, AudioPeerStatus, Position,
};
use std::sync::OnceLock;

use crate::audio_settings::audio_settings_visibility;
use crate::client_object::ClientObject;

#[derive(CompositeTemplate, Default)]
#[template(resource = "/de/feschber/LanMouse/client_row.ui")]
pub struct ClientRow {
    #[template_child]
    pub enable_switch: TemplateChild<gtk::Switch>,
    #[template_child]
    pub dns_button: TemplateChild<gtk::Button>,
    #[template_child]
    pub hostname: TemplateChild<gtk::Entry>,
    #[template_child]
    pub port: TemplateChild<gtk::Entry>,
    #[template_child]
    pub position: TemplateChild<ComboRow>,
    #[template_child]
    pub delete_row: TemplateChild<ActionRow>,
    #[template_child]
    pub delete_button: TemplateChild<gtk::Button>,
    #[template_child]
    pub dns_loading_indicator: TemplateChild<gtk::Spinner>,
    #[template_child]
    pub audio_mode: TemplateChild<ComboRow>,
    #[template_child]
    pub send_system_audio_row: TemplateChild<ActionRow>,
    #[template_child]
    pub send_system_audio_switch: TemplateChild<Switch>,
    #[template_child]
    pub system_capture_device: TemplateChild<ComboRow>,
    #[template_child]
    pub send_microphone_row: TemplateChild<ActionRow>,
    #[template_child]
    pub send_microphone_switch: TemplateChild<Switch>,
    #[template_child]
    pub microphone_capture_device: TemplateChild<ComboRow>,
    #[template_child]
    pub playback_device: TemplateChild<ComboRow>,
    #[template_child]
    pub receive_microphone_row: TemplateChild<ActionRow>,
    #[template_child]
    pub receive_microphone_switch: TemplateChild<Switch>,
    #[template_child]
    pub audio_status_row: TemplateChild<ActionRow>,
    pub bindings: RefCell<Vec<Binding>>,
    hostname_change_handler: RefCell<Option<SignalHandlerId>>,
    port_change_handler: RefCell<Option<SignalHandlerId>>,
    position_change_handler: RefCell<Option<SignalHandlerId>>,
    set_state_handler: RefCell<Option<SignalHandlerId>>,
    pub client_object: RefCell<Option<ClientObject>>,
    pub(super) audio_config: RefCell<AudioConfig>,
    audio_devices: RefCell<Vec<AudioDevice>>,
    system_device_ids: RefCell<Vec<Option<String>>>,
    microphone_device_ids: RefCell<Vec<Option<String>>>,
    playback_device_ids: RefCell<Vec<Option<String>>>,
    applying_audio_state: Cell<bool>,
}

#[glib::object_subclass]
impl ObjectSubclass for ClientRow {
    // `NAME` needs to match `class` attribute of template
    const NAME: &'static str = "ClientRow";
    const ABSTRACT: bool = false;

    type Type = super::ClientRow;
    type ParentType = adw::ExpanderRow;

    fn class_init(klass: &mut Self::Class) {
        klass.bind_template();
        klass.bind_template_callbacks();
    }

    fn instance_init(obj: &InitializingObject<Self>) {
        obj.init_template();
    }
}

impl ObjectImpl for ClientRow {
    fn constructed(&self) {
        self.parent_constructed();
        self.delete_button.connect_clicked(clone!(
            #[weak(rename_to = row)]
            self,
            move |button| {
                row.handle_client_delete(button);
            }
        ));
        let handler = self.hostname.connect_changed(clone!(
            #[weak(rename_to = row)]
            self,
            move |entry| {
                row.handle_hostname_changed(entry);
            }
        ));
        self.hostname_change_handler.replace(Some(handler));
        let handler = self.port.connect_changed(clone!(
            #[weak(rename_to = row)]
            self,
            move |entry| {
                row.handle_port_changed(entry);
            }
        ));
        self.port_change_handler.replace(Some(handler));
        let handler = self.position.connect_selected_notify(clone!(
            #[weak(rename_to = row)]
            self,
            move |position| {
                row.handle_position_changed(position);
            }
        ));
        self.position_change_handler.replace(Some(handler));
        let handler = self.enable_switch.connect_state_set(clone!(
            #[weak(rename_to = row)]
            self,
            #[upgrade_or]
            glib::Propagation::Proceed,
            move |switch, state| {
                row.handle_activate_switch(state, switch);
                glib::Propagation::Proceed
            }
        ));
        self.set_state_handler.replace(Some(handler));

        self.audio_mode.connect_selected_notify(clone!(
            #[weak(rename_to = row)]
            self,
            move |_| row.handle_audio_mode_changed()
        ));
        self.send_system_audio_switch.connect_active_notify(clone!(
            #[weak(rename_to = row)]
            self,
            move |switch| row.update_audio_bool(
                |config| config.send_system_audio,
                |config, value| config.send_system_audio = value,
                switch.is_active()
            )
        ));
        self.send_microphone_switch.connect_active_notify(clone!(
            #[weak(rename_to = row)]
            self,
            move |switch| row.update_audio_bool(
                |config| config.send_microphone,
                |config, value| config.send_microphone = value,
                switch.is_active()
            )
        ));
        self.receive_microphone_switch.connect_active_notify(clone!(
            #[weak(rename_to = row)]
            self,
            move |switch| row.update_audio_bool(
                |config| config.receive_microphone,
                |config, value| config.receive_microphone = value,
                switch.is_active()
            )
        ));
        self.system_capture_device.connect_selected_notify(clone!(
            #[weak(rename_to = row)]
            self,
            move |combo| row.update_audio_device(0, combo.selected())
        ));
        self.microphone_capture_device
            .connect_selected_notify(clone!(
                #[weak(rename_to = row)]
                self,
                move |combo| row.update_audio_device(1, combo.selected())
            ));
        self.playback_device.connect_selected_notify(clone!(
            #[weak(rename_to = row)]
            self,
            move |combo| row.update_audio_device(2, combo.selected())
        ));
    }

    fn signals() -> &'static [glib::subclass::Signal] {
        static SIGNALS: OnceLock<Vec<Signal>> = OnceLock::new();
        SIGNALS.get_or_init(|| {
            vec![
                Signal::builder("request-activate")
                    .param_types([bool::static_type()])
                    .build(),
                Signal::builder("request-delete").build(),
                Signal::builder("request-dns").build(),
                Signal::builder("request-hostname-change")
                    .param_types([String::static_type()])
                    .build(),
                Signal::builder("request-port-change")
                    .param_types([u32::static_type()])
                    .build(),
                Signal::builder("request-position-change")
                    .param_types([u32::static_type()])
                    .build(),
                Signal::builder("request-audio-config").build(),
            ]
        })
    }
}

#[gtk::template_callbacks]
impl ClientRow {
    #[template_callback]
    fn handle_activate_switch(&self, state: bool, _switch: &Switch) -> bool {
        self.obj().emit_by_name::<()>("request-activate", &[&state]);
        true // dont run default handler
    }

    #[template_callback]
    fn handle_request_dns(&self, _: &Button) {
        self.obj().emit_by_name::<()>("request-dns", &[]);
    }

    #[template_callback]
    fn handle_client_delete(&self, _button: &Button) {
        self.obj().emit_by_name::<()>("request-delete", &[]);
    }

    fn handle_port_changed(&self, port_entry: &Entry) {
        if let Ok(port) = port_entry.text().parse::<u16>() {
            self.obj()
                .emit_by_name::<()>("request-port-change", &[&(port as u32)]);
        }
    }

    fn handle_hostname_changed(&self, hostname_entry: &Entry) {
        self.obj()
            .emit_by_name::<()>("request-hostname-change", &[&hostname_entry.text()]);
    }

    fn handle_position_changed(&self, position: &ComboRow) {
        self.obj()
            .emit_by_name("request-position-change", &[&position.selected()])
    }

    fn handle_audio_mode_changed(&self) {
        if self.applying_audio_state.get() {
            return;
        }
        let mode = match self.audio_mode.selected() {
            1 => AudioMode::Send,
            2 => AudioMode::Receive,
            _ => AudioMode::Off,
        };
        if self.audio_config.borrow().mode == mode {
            return;
        }
        self.audio_config.borrow_mut().mode = mode;
        self.update_audio_visibility();
        self.emit_audio_config_request();
    }

    fn update_audio_bool(
        &self,
        get_value: impl Fn(&AudioConfig) -> bool,
        set_value: impl Fn(&mut AudioConfig, bool),
        value: bool,
    ) {
        if self.applying_audio_state.get() {
            return;
        }
        let mut config = self.audio_config.borrow_mut();
        if get_value(&config) == value {
            return;
        }
        set_value(&mut config, value);
        drop(config);
        self.update_audio_visibility();
        self.emit_audio_config_request();
    }

    fn update_audio_device(&self, device: usize, index: u32) {
        if self.applying_audio_state.get() {
            return;
        }
        let selected = match device {
            0 => self.system_device_ids.borrow().get(index as usize).cloned(),
            1 => self
                .microphone_device_ids
                .borrow()
                .get(index as usize)
                .cloned(),
            _ => self
                .playback_device_ids
                .borrow()
                .get(index as usize)
                .cloned(),
        };
        let Some(selected) = selected else {
            return;
        };
        let mut config = self.audio_config.borrow_mut();
        let current = match device {
            0 => &mut config.system_capture_device,
            1 => &mut config.microphone_capture_device,
            _ => &mut config.playback_device,
        };
        if *current == selected {
            return;
        }
        *current = selected;
        drop(config);
        self.emit_audio_config_request();
    }

    fn emit_audio_config_request(&self) {
        self.obj().emit_by_name::<()>("request-audio-config", &[]);
    }

    fn update_audio_visibility(&self) {
        let config = self.audio_config.borrow();
        let visibility = audio_settings_visibility(config.mode);
        self.send_system_audio_row
            .set_visible(visibility.send_system_audio);
        self.system_capture_device
            .set_visible(visibility.system_capture_device && config.send_system_audio);
        self.send_microphone_row
            .set_visible(visibility.send_microphone);
        self.microphone_capture_device
            .set_visible(visibility.microphone_capture_device && config.send_microphone);
        self.playback_device.set_visible(visibility.playback_device);
        self.receive_microphone_row
            .set_visible(visibility.receive_microphone);
    }

    fn rebuild_device_combo(
        &self,
        combo: &ComboRow,
        direction: AudioDeviceDirection,
        selected: Option<&str>,
        ids: &RefCell<Vec<Option<String>>>,
    ) {
        let mut labels = vec!["Primeiro disponível".to_owned()];
        let mut values = vec![None];
        for device in self
            .audio_devices
            .borrow()
            .iter()
            .filter(|device| device.direction == direction)
        {
            labels.push(device.name.clone());
            values.push(Some(device.id.clone()));
        }
        if let Some(selected) = selected {
            if !values.iter().flatten().any(|id| id == selected) {
                labels.push(format!("{selected} (indisponível)"));
                values.push(Some(selected.to_owned()));
            }
        }
        let selected_index = selected
            .and_then(|id| values.iter().position(|value| value.as_deref() == Some(id)))
            .unwrap_or(0) as u32;
        let label_refs = labels.iter().map(String::as_str).collect::<Vec<_>>();
        let model = gtk::StringList::new(&label_refs);
        ids.replace(values);
        combo.set_model(Some(&model));
        combo.set_selected(selected_index);
    }

    pub(super) fn apply_audio_config(&self, config: AudioConfig) {
        self.applying_audio_state.set(true);
        self.audio_config.replace(config.clone());
        self.audio_mode.set_selected(match config.mode {
            AudioMode::Off => 0,
            AudioMode::Send => 1,
            AudioMode::Receive => 2,
        });
        self.send_system_audio_switch
            .set_active(config.send_system_audio);
        self.send_microphone_switch
            .set_active(config.send_microphone);
        self.receive_microphone_switch
            .set_active(config.receive_microphone);
        self.rebuild_device_combo(
            &self.system_capture_device,
            AudioDeviceDirection::Playback,
            config.system_capture_device.as_deref(),
            &self.system_device_ids,
        );
        self.rebuild_device_combo(
            &self.microphone_capture_device,
            AudioDeviceDirection::Capture,
            config.microphone_capture_device.as_deref(),
            &self.microphone_device_ids,
        );
        self.rebuild_device_combo(
            &self.playback_device,
            AudioDeviceDirection::Playback,
            config.playback_device.as_deref(),
            &self.playback_device_ids,
        );
        self.update_audio_visibility();
        self.applying_audio_state.set(false);
    }

    pub(super) fn set_audio_devices(&self, devices: Vec<AudioDevice>) {
        self.applying_audio_state.set(true);
        self.audio_devices.replace(devices);
        let config = self.audio_config.borrow().clone();
        self.rebuild_device_combo(
            &self.system_capture_device,
            AudioDeviceDirection::Playback,
            config.system_capture_device.as_deref(),
            &self.system_device_ids,
        );
        self.rebuild_device_combo(
            &self.microphone_capture_device,
            AudioDeviceDirection::Capture,
            config.microphone_capture_device.as_deref(),
            &self.microphone_device_ids,
        );
        self.rebuild_device_combo(
            &self.playback_device,
            AudioDeviceDirection::Playback,
            config.playback_device.as_deref(),
            &self.playback_device_ids,
        );
        self.applying_audio_state.set(false);
    }

    pub(super) fn set_audio_status(&self, status: AudioPeerStatus) {
        let text = match status {
            AudioPeerStatus::Off => "Desativado".to_owned(),
            AudioPeerStatus::WaitingForPeer => "Aguardando o outro dispositivo".to_owned(),
            AudioPeerStatus::Starting => "Iniciando".to_owned(),
            AudioPeerStatus::Active => "Ativo".to_owned(),
            AudioPeerStatus::DeviceUnavailable(error) => {
                format!("Dispositivo indisponível: {error}")
            }
            AudioPeerStatus::Error(error) => format!("Erro: {error}"),
        };
        self.audio_status_row.set_subtitle(&text);
    }

    pub(super) fn set_hostname(&self, hostname: Option<String>) {
        let position = self.hostname.position();
        let handler = self.hostname_change_handler.borrow();
        let handler = handler.as_ref().expect("signal handler");
        self.hostname.block_signal(handler);
        self.client_object
            .borrow_mut()
            .as_mut()
            .expect("client object")
            .set_property("hostname", hostname);
        self.hostname.unblock_signal(handler);
        self.hostname.set_position(position);
    }

    pub(super) fn set_port(&self, port: u16) {
        let position = self.port.position();
        let handler = self.port_change_handler.borrow();
        let handler = handler.as_ref().expect("signal handler");
        self.port.block_signal(handler);
        self.client_object
            .borrow_mut()
            .as_mut()
            .expect("client object")
            .set_port(port as u32);
        self.port.unblock_signal(handler);
        self.port.set_position(position);
    }

    pub(super) fn set_pos(&self, pos: Position) {
        let handler = self.position_change_handler.borrow();
        let handler = handler.as_ref().expect("signal handler");
        self.position.block_signal(handler);
        self.client_object
            .borrow_mut()
            .as_mut()
            .expect("client object")
            .set_position(pos.to_string());
        self.position.unblock_signal(handler);
    }

    pub(super) fn set_active(&self, active: bool) {
        let handler = self.set_state_handler.borrow();
        let handler = handler.as_ref().expect("signal handler");
        self.enable_switch.block_signal(handler);
        self.client_object
            .borrow_mut()
            .as_mut()
            .expect("client object")
            .set_active(active);
        self.enable_switch.unblock_signal(handler);
    }

    pub(super) fn set_dns_state(&self, resolved: bool) {
        if resolved {
            self.dns_button.set_css_classes(&["success"])
        } else {
            self.dns_button.set_css_classes(&["warning"])
        }
    }
}

impl WidgetImpl for ClientRow {}
impl BoxImpl for ClientRow {}
impl ListBoxRowImpl for ClientRow {}
impl PreferencesRowImpl for ClientRow {}
impl ExpanderRowImpl for ClientRow {}
