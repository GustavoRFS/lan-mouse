use std::cell::RefCell;

use adw::prelude::*;
use gtk::{gio, glib};
use ksni::{blocking::TrayMethods, menu::StandardItem, Tray};

use crate::window::Window;

enum TrayAction {
    Open(Option<String>),
    Quit,
}

struct LinuxTray {
    actions: async_channel::Sender<TrayAction>,
}

impl LinuxTray {
    fn send_action(&self, action: TrayAction) {
        let _ = self.actions.send_blocking(action);
    }

    fn open_window(&self) {
        let token = ksni::current_xdg_activation_token().or_else(|| {
            std::thread::sleep(std::time::Duration::from_millis(1));
            ksni::current_xdg_activation_token()
        });
        self.send_action(TrayAction::Open(token));
    }
}

impl Tray for LinuxTray {
    fn id(&self) -> String {
        "lan-mouse".into()
    }

    fn title(&self) -> String {
        "Lan Mouse".into()
    }

    fn icon_name(&self) -> String {
        "de.feschber.LanMouse".into()
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        self.open_window();
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::MenuItem;

        vec![
            StandardItem {
                label: "Open Lan Mouse".into(),
                activate: Box::new(|tray: &mut LinuxTray| tray.open_window()),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Quit Lan Mouse".into(),
                activate: Box::new(|tray: &mut LinuxTray| {
                    tray.send_action(TrayAction::Quit)
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
}

struct TrayState {
    app: glib::WeakRef<adw::Application>,
    window: glib::WeakRef<Window>,
    _hold: gio::ApplicationHoldGuard,
    _tray: ksni::blocking::Handle<LinuxTray>,
}

thread_local! {
    static TRAY_STATE: RefCell<Option<TrayState>> = const { RefCell::new(None) };
}

pub fn setup(app: &adw::Application, window: &Window) -> bool {
    let (actions, receiver) = async_channel::bounded(8);
    let tray = match (LinuxTray { actions }).spawn() {
        Ok(tray) => tray,
        Err(error) => {
            log::warn!("could not register Linux system tray item: {error}");
            return false;
        }
    };

    let hold = app.hold();
    TRAY_STATE.with(|state| {
        *state.borrow_mut() = Some(TrayState {
            app: app.downgrade(),
            window: window.downgrade(),
            _hold: hold,
            _tray: tray,
        });
    });

    glib::spawn_future_local(async move {
        while let Ok(action) = receiver.recv().await {
            match action {
                TrayAction::Open(token) => present_window(token),
                TrayAction::Quit => quit_app(),
            }
        }
    });

    true
}

fn present_window(token: Option<String>) {
    TRAY_STATE.with(|state| {
        if let Some(state) = state.borrow().as_ref() {
            if let Some(window) = state.window.upgrade() {
                if let Some(token) = token {
                    window.set_startup_id(&token);
                }
                window.set_visible(true);
                window.present();
            }
        }
    });
}

fn quit_app() {
    TRAY_STATE.with(|state| {
        if let Some(state) = state.borrow().as_ref() {
            if let Some(app) = state.app.upgrade() {
                app.quit();
            }
        }
    });
}
