use std::cell::RefCell;
use std::mem;

use adw::prelude::*;
use gtk::{gio, glib};

use crate::window::Window;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, GetCursorPos,
    IDI_APPLICATION, LoadIconW, MF_SEPARATOR, MF_STRING, PostMessageW, RegisterClassW,
    SetForegroundWindow, TPM_BOTTOMALIGN, TPM_LEFTALIGN, TrackPopupMenu, WINDOW_STYLE, WM_APP,
    WM_COMMAND, WM_LBUTTONDBLCLK, WM_NULL, WM_RBUTTONUP, WNDCLASSW,
};
use windows::core::w;

const WM_TRAY_CALLBACK: u32 = WM_APP + 1;
const IDM_OPEN: usize = 1000;
const IDM_QUIT: usize = 1001;

struct TrayState {
    app: glib::WeakRef<adw::Application>,
    window: glib::WeakRef<Window>,
    _hold: gio::ApplicationHoldGuard,
    hwnd: HWND,
}

thread_local! {
    static TRAY_STATE: RefCell<Option<TrayState>> = const { RefCell::new(None) };
}

pub(crate) fn clipboard_owner_hwnd() -> Option<HWND> {
    TRAY_STATE.with(|state| state.borrow().as_ref().map(|state| state.hwnd))
}

pub fn setup(app: &adw::Application, window: &Window) {
    unsafe {
        let hold = app.hold();

        let instance = GetModuleHandleW(None).unwrap();

        let wc = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            hInstance: instance.into(),
            lpszClassName: w!("LanMouseTrayClass"),
            ..Default::default()
        };
        RegisterClassW(&wc);

        let hwnd = CreateWindowExW(
            Default::default(),
            w!("LanMouseTrayClass"),
            w!("LanMouseTray"),
            WINDOW_STYLE::default(),
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance.into()),
            None,
        )
        .unwrap();

        let icon = LoadIconW(Some(instance.into()), w!("lan-mouse"))
            .or_else(|_| LoadIconW(None, IDI_APPLICATION))
            .unwrap();

        let mut nid: NOTIFYICONDATAW = mem::zeroed();
        nid.cbSize = mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = 1;
        nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        nid.uCallbackMessage = WM_TRAY_CALLBACK;
        nid.hIcon = icon;
        let tip = "Lan Mouse\0".encode_utf16().collect::<Vec<u16>>();
        nid.szTip[..tip.len()].copy_from_slice(&tip);

        let _ = Shell_NotifyIconW(NIM_ADD, &nid);

        TRAY_STATE.with(|state| {
            *state.borrow_mut() = Some(TrayState {
                app: app.downgrade(),
                window: window.downgrade(),
                _hold: hold,
                hwnd,
            });
        });
    }
}

fn present_window() {
    TRAY_STATE.with(|state| {
        let state = state.borrow();
        if let Some(state) = state.as_ref() {
            if let Some(window) = state.window.upgrade() {
                window.set_visible(true);
                window.present();
            }
        }
    });
}

fn quit_app() {
    TRAY_STATE.with(|state| {
        let state = state.borrow();
        if let Some(state) = state.as_ref() {
            unsafe {
                let mut nid: NOTIFYICONDATAW = mem::zeroed();
                nid.cbSize = mem::size_of::<NOTIFYICONDATAW>() as u32;
                nid.hWnd = state.hwnd;
                nid.uID = 1;
                let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
            }
            if let Some(app) = state.app.upgrade() {
                app.quit();
            }
        }
    });
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_TRAY_CALLBACK => {
            let event = (lparam.0 & 0xFFFF) as u32;
            match event {
                WM_LBUTTONDBLCLK => {
                    glib::idle_add_local_once(present_window);
                }
                WM_RBUTTONUP => {
                    show_context_menu(hwnd);
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            let id = wparam.0 & 0xFFFF;
            match id {
                IDM_OPEN => {
                    glib::idle_add_local_once(present_window);
                }
                IDM_QUIT => {
                    glib::idle_add_local_once(quit_app);
                }
                _ => {}
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe fn show_context_menu(hwnd: HWND) {
    let menu = CreatePopupMenu().unwrap();
    let _ = AppendMenuW(menu, MF_STRING, IDM_OPEN, w!("Open Lan Mouse"));
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
    let _ = AppendMenuW(menu, MF_STRING, IDM_QUIT, w!("Quit"));

    let mut pt = mem::zeroed();
    let _ = GetCursorPos(&mut pt);
    let _ = SetForegroundWindow(hwnd);
    let _ = TrackPopupMenu(
        menu,
        TPM_BOTTOMALIGN | TPM_LEFTALIGN,
        pt.x,
        pt.y,
        Some(0),
        hwnd,
        None,
    );
    // Without a message posted to the owner after TrackPopupMenu, a tray menu
    // stays on screen when the user clicks away instead of dismissing.
    let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
    let _ = DestroyMenu(menu);
}
