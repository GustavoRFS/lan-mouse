use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::ptr::addr_of_mut;

use std::default::Default;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use tokio::sync::mpsc::Sender;
use tokio::sync::mpsc::error::TrySendError;
use windows::Win32::Foundation::{FALSE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    DEVMODEW, DISPLAY_DEVICE_ATTACHED_TO_DESKTOP, DISPLAY_DEVICEW, ENUM_CURRENT_SETTINGS,
    EnumDisplayDevicesW, EnumDisplaySettingsW,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::core::{PCWSTR, w};

use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DispatchMessageW, EDD_GET_DEVICE_INTERFACE_NAME, GetMessageW,
    HOOKPROC, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, MSG, MSLLHOOKSTRUCT, PM_NOREMOVE, PeekMessageW,
    PostThreadMessageW, RegisterClassW, SetWindowsHookExW, TranslateMessage, WH_KEYBOARD_LL,
    WH_MOUSE_LL, WINDOW_STYLE, WM_DISPLAYCHANGE, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL,
    WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_USER, WM_XBUTTONDOWN,
    WM_XBUTTONUP, WNDCLASSW, WNDPROC,
};

use input_event::{
    BTN_BACK, BTN_FORWARD, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, Event, KeyboardEvent, MonitorId,
    PointerEvent,
    scancode::{self, Linux},
};

use crate::output_geometry::OutputGeometry;

#[cfg(test)]
use crate::Position;

use super::{CaptureEvent, CapturePoint, display_util};

pub(crate) struct EventThread {
    request_buffer: Arc<Mutex<Vec<ClientUpdate>>>,
    thread: Option<thread::JoinHandle<()>>,
    thread_id: u32,
}

impl EventThread {
    pub(crate) fn new(event_tx: Sender<(CapturePoint, CaptureEvent)>) -> Self {
        let request_buffer = Default::default();
        let (thread, thread_id) = start(event_tx, Arc::clone(&request_buffer));
        Self {
            request_buffer,
            thread: Some(thread),
            thread_id,
        }
    }

    pub(crate) fn release_capture(&self) -> windows::core::Result<()> {
        self.signal(RequestType::Release)
    }

    pub(crate) fn create(&self, point: CapturePoint) -> windows::core::Result<()> {
        self.client_update(ClientUpdate::Create(point))
    }

    pub(crate) fn destroy(&self, point: CapturePoint) -> windows::core::Result<()> {
        self.client_update(ClientUpdate::Destroy(point))
    }

    #[cfg(test)]
    fn exit(&self) -> windows::core::Result<()> {
        self.signal(RequestType::Exit)
    }

    fn client_update(&self, request: ClientUpdate) -> windows::core::Result<()> {
        {
            let mut requests = self.request_buffer.lock().unwrap();
            requests.push(request);
        }
        self.signal(RequestType::ClientUpdate)
    }

    fn signal(&self, event_type: RequestType) -> windows::core::Result<()> {
        let id = self.thread_id;
        unsafe { PostThreadMessageW(id, WM_USER, WPARAM(event_type as usize), LPARAM(0)) }
    }
}

impl Drop for EventThread {
    fn drop(&mut self) {
        self.shutdown_with(|id| unsafe {
            PostThreadMessageW(id, WM_USER, WPARAM(RequestType::Exit as usize), LPARAM(0))
        });
    }
}

impl EventThread {
    fn shutdown_with(&mut self, mut post_exit: impl FnMut(u32) -> windows::core::Result<()>) {
        let Some(worker) = self.thread.as_ref() else {
            return;
        };
        let mut logged_error = false;
        loop {
            match post_exit(self.thread_id) {
                Ok(()) => break,
                Err(err) if worker.is_finished() => {
                    log::warn!("capture thread {} already exited: {err}", self.thread_id);
                    break;
                }
                Err(err) => {
                    if !logged_error {
                        log::warn!("waiting to stop capture thread {}: {err}", self.thread_id);
                        logged_error = true;
                    }
                    // The worker drains its queue. Retry until it can accept
                    // Exit rather than leave its global input hooks installed.
                    thread::sleep(std::time::Duration::from_millis(1));
                }
            }
        }
        if let Some(worker) = self.thread.take() {
            let _ = worker.join();
        }
    }
}

enum RequestType {
    ClientUpdate = 0,
    Release = 1,
    Exit = 2,
}

enum ClientUpdate {
    Create(CapturePoint),
    Destroy(CapturePoint),
}

fn blocking_send_event(point: CapturePoint, event: CaptureEvent) -> bool {
    EVENT_TX.with_borrow_mut(|tx| match tx.as_mut() {
        Some(tx) => tx.blocking_send((point, event)).is_ok(),
        None => false,
    })
}

fn try_send_event(
    point: CapturePoint,
    event: CaptureEvent,
) -> Result<(), TrySendError<(CapturePoint, CaptureEvent)>> {
    EVENT_TX.with_borrow_mut(|tx| tx.as_mut().unwrap().try_send((point, event)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::ManuallyDrop;
    use std::time::{Duration, Instant};
    use windows::Win32::UI::WindowsAndMessaging::WM_NULL;

    #[test]
    fn newly_started_thread_accepts_messages_immediately() {
        for _ in 0..128 {
            let (tx, _rx) = tokio::sync::mpsc::channel(10);
            let mut worker = ManuallyDrop::new(EventThread::new(tx));
            let initial_post =
                unsafe { PostThreadMessageW(worker.thread_id, WM_NULL, WPARAM(0), LPARAM(0)) };

            // Clean up even on the old implementation, whose constructor can
            // return before the worker has a queue. Never panic in its Drop.
            let deadline = Instant::now() + Duration::from_secs(5);
            while unsafe {
                PostThreadMessageW(
                    worker.thread_id,
                    WM_USER,
                    WPARAM(RequestType::Exit as usize),
                    LPARAM(0),
                )
            }
            .is_err()
            {
                assert!(Instant::now() < deadline, "worker did not initialize");
                thread::yield_now();
            }
            worker.thread.take().unwrap().join().unwrap();
            assert!(
                initial_post.is_ok(),
                "constructor returned before queue existed: {initial_post:?}"
            );
        }
    }

    #[test]
    fn signaling_an_exited_thread_does_not_panic() {
        let id = thread::spawn(|| unsafe { GetCurrentThreadId() })
            .join()
            .unwrap();
        let worker = ManuallyDrop::new(EventThread {
            request_buffer: Default::default(),
            thread: None,
            thread_id: id,
        });
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            worker.signal(RequestType::Release)
        }));
        assert!(
            outcome.is_ok(),
            "posting failure must be returned, not panic"
        );
        assert!(
            outcome.unwrap().is_err(),
            "an exited thread cannot accept messages"
        );
    }

    #[test]
    fn blocking_send_to_closed_receiver_does_not_panic() {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        drop(rx);
        EVENT_TX.replace(Some(tx));
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            blocking_send_event(
                CapturePoint::directional(Position::Right),
                CaptureEvent::Begin,
            )
        }));
        EVENT_TX.replace(None);
        assert!(
            matches!(outcome, Ok(false)),
            "a closed receiver must be reported without panicking"
        );
    }

    #[test]
    fn drop_retries_exit_after_a_transient_queue_full_error() {
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (finished_tx, finished_rx) = std::sync::mpsc::channel();
        let thread = thread::spawn(move || {
            unsafe {
                let mut msg = MSG::default();
                let _ = PeekMessageW(&mut msg, None, WM_USER, WM_USER, PM_NOREMOVE);
                ready_tx.send(GetCurrentThreadId()).unwrap();
            }
            while let Some(msg) = get_msg() {
                if msg.message == WM_USER && msg.wParam.0 == RequestType::Exit as usize {
                    break;
                }
            }
            finished_tx.send(()).unwrap();
        });
        let id = ready_rx.recv().unwrap();
        let mut worker = EventThread {
            request_buffer: Default::default(),
            thread: Some(thread),
            thread_id: id,
        };
        let mut attempts = 0;
        worker.shutdown_with(|thread_id| {
            attempts += 1;
            if attempts == 1 {
                Err(windows::core::Error::from_hresult(windows::core::HRESULT(
                    0x80070718_u32 as i32,
                )))
            } else {
                unsafe {
                    PostThreadMessageW(
                        thread_id,
                        WM_USER,
                        WPARAM(RequestType::Exit as usize),
                        LPARAM(0),
                    )
                }
            }
        });
        assert_eq!(attempts, 2, "shutdown retries transient posting failures");
        finished_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        drop(worker);
    }

    #[test]
    fn drop_joins_an_already_exited_worker() {
        let (tx, _rx) = tokio::sync::mpsc::channel(10);
        let worker = EventThread::new(tx);
        worker.exit().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !worker.thread.as_ref().unwrap().is_finished() {
            assert!(Instant::now() < deadline, "worker did not exit");
            thread::yield_now();
        }
        drop(worker);
    }
}

thread_local! {
    /// all configured clients
    static CLIENTS: RefCell<HashSet<CapturePoint>> = RefCell::new(HashSet::new());
    /// currently active client
    static ACTIVE_CLIENT: RefCell<Option<CapturePoint>> = const { RefCell::new(None) };
    /// input event channel
    static EVENT_TX: RefCell<Option<Sender<(CapturePoint, CaptureEvent)>>> = const { RefCell::new(None) };
    /// position of barrier entry
    static ENTRY_POINT: Cell<(i32, i32)> = const { Cell::new((0, 0)) };
    /// previous mouse position
    static PREV_POS: Cell<Option<(i32, i32)>> = const { Cell::new(None) };
    /// displays and generation counter
    static DISPLAYS: RefCell<(Vec<OutputGeometry>, i32)> = const { RefCell::new((Vec::new(), 0)) };
}

fn active_client() -> Option<CapturePoint> {
    ACTIVE_CLIENT.with_borrow(|active| active.clone())
}

fn clear_active_client() {
    ACTIVE_CLIENT.with_borrow_mut(Option::take);
}

fn get_msg() -> Option<MSG> {
    unsafe {
        let mut msg = std::mem::zeroed();
        let ret = GetMessageW(addr_of_mut!(msg), None, 0, 0);
        match ret.0 {
            0 => None,
            x if x > 0 => Some(msg),
            _ => panic!("error in GetMessageW"),
        }
    }
}

fn start(
    event_tx: Sender<(CapturePoint, CaptureEvent)>,
    request_buffer: Arc<Mutex<Vec<ClientUpdate>>>,
) -> (thread::JoinHandle<()>, u32) {
    /* condition variable to wait for thead id */
    let thread_id = Arc::new((Condvar::new(), Mutex::new(None)));
    let thread_id_ = Arc::clone(&thread_id);

    let msg_thread = thread::spawn(|| start_routine(thread_id_, event_tx, request_buffer));

    /* wait for thread to set its id */
    let (cond, thread_id) = &*thread_id;
    let mut thread_id = thread_id.lock().unwrap();
    while (*thread_id).is_none() {
        thread_id = cond.wait(thread_id).expect("channel closed");
    }
    (msg_thread, thread_id.expect("thread id"))
}

fn start_routine(
    ready: Arc<(Condvar, Mutex<Option<u32>>)>,
    event_tx: Sender<(CapturePoint, CaptureEvent)>,
    request_buffer: Arc<Mutex<Vec<ClientUpdate>>>,
) {
    EVENT_TX.replace(Some(event_tx));
    // PostThreadMessage fails until the target thread has a message queue.
    // Force its creation before publishing the ID to the constructor; merely
    // having a running thread is not sufficient (ERROR_INVALID_THREAD_ID).
    unsafe {
        let mut msg = MSG::default();
        let _ = PeekMessageW(&mut msg, None, WM_USER, WM_USER, PM_NOREMOVE);
    }
    /* communicate thread id */
    {
        let (cnd, mtx) = &*ready;
        let mut ready = mtx.lock().unwrap();
        *ready = Some(unsafe { GetCurrentThreadId() });
        cnd.notify_one();
    }

    let mouse_proc: HOOKPROC = Some(mouse_proc);
    let kybrd_proc: HOOKPROC = Some(kybrd_proc);
    let window_proc: WNDPROC = Some(window_proc);

    /* register hooks */
    unsafe {
        let _ = SetWindowsHookExW(WH_MOUSE_LL, mouse_proc, None, 0).unwrap();
        let _ = SetWindowsHookExW(WH_KEYBOARD_LL, kybrd_proc, None, 0).unwrap();
    }

    let instance = unsafe { GetModuleHandleW(None).unwrap() };
    let instance = instance.into();
    let window_class: WNDCLASSW = WNDCLASSW {
        lpfnWndProc: window_proc,
        hInstance: instance,
        lpszClassName: w!("lan-mouse-message-window-class"),
        ..Default::default()
    };

    static WINDOW_CLASS_REGISTERED: AtomicBool = AtomicBool::new(false);
    if WINDOW_CLASS_REGISTERED
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
    {
        /* register window class if not yet done so */
        unsafe {
            let ret = RegisterClassW(&window_class);
            if ret == 0 {
                panic!("RegisterClassW");
            }
        }
    }

    /* window is used ro receive WM_DISPLAYCHANGE messages */
    unsafe {
        CreateWindowExW(
            Default::default(),
            w!("lan-mouse-message-window-class"),
            w!("lan-mouse-msg-window"),
            WINDOW_STYLE::default(),
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance),
            None,
        )
        .expect("CreateWindowExW");
    }

    /* run message loop */
    while let Some(msg) = get_msg() {
        // mouse / keybrd proc do not actually return a message
        if msg.hwnd.0.is_null() {
            /* messages sent via PostThreadMessage */
            match msg.wParam.0 {
                x if x == RequestType::Exit as usize => break,
                x if x == RequestType::Release as usize => {
                    clear_active_client();
                }
                x if x == RequestType::ClientUpdate as usize => {
                    let requests = {
                        let mut res = vec![];
                        let mut requests = request_buffer.lock().unwrap();
                        for request in requests.drain(..) {
                            res.push(request);
                        }
                        res
                    };

                    for request in requests {
                        update_clients(request)
                    }
                }
                _ => {}
            }
        } else {
            /* other messages for window_procs */
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}

fn check_client_activation(wparam: WPARAM, lparam: LPARAM) -> bool {
    if wparam.0 != WM_MOUSEMOVE as usize {
        return active_client().is_some();
    }
    let mouse_low_level: MSLLHOOKSTRUCT = unsafe { *(lparam.0 as *const MSLLHOOKSTRUCT) };
    let curr_pos = (mouse_low_level.pt.x, mouse_low_level.pt.y);
    let prev_pos = PREV_POS.get().unwrap_or(curr_pos);
    PREV_POS.replace(Some(curr_pos));

    /* next event is the first actual event */
    let ret = active_client().is_some();

    /* client already active, no need to check */
    if ret {
        return ret;
    }

    /* check if a client was activated */
    let entered = DISPLAYS.with_borrow_mut(|(displays, generation)| {
        update_display_regions(displays, generation);
        display_util::entered_barrier(prev_pos, curr_pos, displays)
    });

    let Some(entered_point) = entered else {
        return ret;
    };

    /* exact monitor registrations take precedence over direction-only ones */
    let Some(point) = CLIENTS.with_borrow(|clients| {
        clients.get(&entered_point).cloned().or_else(|| {
            let directional = CapturePoint::directional(entered_point.edge);
            clients.contains(&directional).then_some(directional)
        })
    }) else {
        return ret;
    };

    /* update active client and entry point */
    ACTIVE_CLIENT.with_borrow_mut(|active| *active = Some(point.clone()));
    let entry_point = DISPLAYS.with_borrow(|(displays, _)| {
        display_util::clamp_to_display_bounds(displays, prev_pos, curr_pos)
    });
    ENTRY_POINT.replace(entry_point);

    /* notify main thread */
    log::debug!("ENTERED @ {prev_pos:?} -> {curr_pos:?}");
    let active = active_client().expect("active client");
    if !blocking_send_event(active, CaptureEvent::Begin) {
        // The receiver can close while the hook worker is handling its final
        // input event. Stop capturing and let this event reach local apps.
        clear_active_client();
        return false;
    }

    ret
}

unsafe extern "system" fn mouse_proc(ncode: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let active = check_client_activation(wparam, lparam);

    /* no client was active */
    if !active {
        return CallNextHookEx(None, ncode, wparam, lparam);
    }

    /* get active client if any */
    let Some(point) = active_client() else {
        return LRESULT(1);
    };

    /* convert to lan-mouse event */
    let Some(pointer_event) = to_mouse_event(wparam, lparam) else {
        return LRESULT(1);
    };

    /* notify mainthread (drop events if sending too fast) */
    if let Err(e) = try_send_event(point, CaptureEvent::Input(Event::Pointer(pointer_event))) {
        log::warn!("e: {e}");
    }

    /* don't pass event to applications */
    LRESULT(1)
}

unsafe extern "system" fn kybrd_proc(ncode: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    /* get active client if any */
    let Some(client) = active_client() else {
        return CallNextHookEx(None, ncode, wparam, lparam);
    };

    /* convert to key event */
    let Some(key_event) = to_key_event(wparam, lparam) else {
        return LRESULT(1);
    };

    if let Err(e) = try_send_event(client, CaptureEvent::Input(Event::Keyboard(key_event))) {
        log::warn!("e: {e}");
    }

    /* don't pass event to applications */
    LRESULT(1)
}

unsafe extern "system" fn window_proc(
    _hwnd: HWND,
    uint: u32,
    _wparam: WPARAM,
    _lparam: LPARAM,
) -> LRESULT {
    if uint == WM_DISPLAYCHANGE {
        log::debug!("display resolution changed");
        DISPLAY_RESOLUTION_GENERATION.fetch_add(1, Ordering::Release);
    }
    LRESULT(1)
}

static DISPLAY_RESOLUTION_GENERATION: AtomicI32 = AtomicI32::new(1);

fn update_display_regions(displays: &mut Vec<OutputGeometry>, generation: &mut i32) {
    let global_generation = DISPLAY_RESOLUTION_GENERATION.load(Ordering::Acquire);
    if *generation != global_generation {
        *displays = enumerate_displays();
        log::debug!("displays: {displays:?}");
        *generation = global_generation;
    }
}

pub(crate) fn enumerate_displays() -> Vec<OutputGeometry> {
    let mut display_regions = Vec::new();
    unsafe {
        let mut devices = vec![];
        for i in 0.. {
            let mut device: DISPLAY_DEVICEW = std::mem::zeroed();
            device.cb = std::mem::size_of::<DISPLAY_DEVICEW>() as u32;
            let ret = EnumDisplayDevicesW(None, i, &mut device, EDD_GET_DEVICE_INTERFACE_NAME);
            if ret == FALSE {
                break;
            }
            if device
                .StateFlags
                .contains(DISPLAY_DEVICE_ATTACHED_TO_DESKTOP)
            {
                let mut monitor: DISPLAY_DEVICEW = std::mem::zeroed();
                monitor.cb = std::mem::size_of::<DISPLAY_DEVICEW>() as u32;
                let monitor_found = EnumDisplayDevicesW(
                    PCWSTR::from_raw(device.DeviceName.as_ptr()),
                    0,
                    &mut monitor,
                    EDD_GET_DEVICE_INTERFACE_NAME,
                ) != FALSE;
                let display_name = wide_string(&device.DeviceName);
                let device_id = if monitor_found {
                    wide_string(&monitor.DeviceID)
                } else {
                    String::new()
                };
                let id =
                    MonitorId::new(device_id).or_else(|_| MonitorId::new(display_name.clone()));
                let label = if monitor_found {
                    wide_string(&monitor.DeviceString)
                } else {
                    wide_string(&device.DeviceString)
                };
                if let Ok(id) = id {
                    devices.push((id, display_name, device.DeviceName, label));
                }
            }
        }
        for (id, display_name, device_name_wide, label) in devices {
            let mut dev_mode: DEVMODEW = std::mem::zeroed();
            dev_mode.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
            let ret = EnumDisplaySettingsW(
                PCWSTR::from_raw(device_name_wide.as_ptr()),
                ENUM_CURRENT_SETTINGS,
                &mut dev_mode,
            );
            if ret == FALSE {
                log::warn!("no display mode");
                continue;
            }

            let pos = dev_mode.Anonymous1.Anonymous2.dmPosition;
            let (x, y) = (pos.x, pos.y);
            let (width, height) = (dev_mode.dmPelsWidth, dev_mode.dmPelsHeight);

            let label = if label.is_empty() {
                display_name
            } else {
                label
            };
            display_regions.push(OutputGeometry {
                id,
                label: truncate_utf8(&label, input_event::MAX_MONITOR_LABEL_BYTES),
                x,
                y,
                width: width as i32,
                height: height as i32,
            });
        }
    }
    display_regions
}

fn wide_string(value: &[u16]) -> String {
    let len = value
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(value.len());
    String::from_utf16_lossy(&value[..len])
}

fn truncate_utf8(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

fn update_clients(request: ClientUpdate) {
    match request {
        ClientUpdate::Create(point) => {
            CLIENTS.with_borrow_mut(|clients| clients.insert(point));
        }
        ClientUpdate::Destroy(point) => {
            if active_client().as_ref() == Some(&point) {
                clear_active_client();
            }
            CLIENTS.with_borrow_mut(|clients| clients.remove(&point));
        }
    }
}

fn to_key_event(wparam: WPARAM, lparam: LPARAM) -> Option<KeyboardEvent> {
    let kybrdllhookstruct: KBDLLHOOKSTRUCT = unsafe { *(lparam.0 as *const KBDLLHOOKSTRUCT) };
    let mut scan_code = kybrdllhookstruct.scanCode;
    log::trace!("scan_code: {scan_code}");
    if kybrdllhookstruct.flags.contains(LLKHF_EXTENDED) {
        scan_code |= 0xE000;
    }
    let Ok(win_scan_code) = scancode::Windows::try_from(scan_code) else {
        log::warn!("failed to translate to windows scancode: {scan_code}");
        return None;
    };
    log::trace!("windows_scan: {win_scan_code:?}");
    let Ok(linux_scan_code): Result<Linux, ()> = win_scan_code.try_into() else {
        log::warn!("failed to translate into linux scancode: {win_scan_code:?}");
        return None;
    };
    log::trace!("windows_scan: {linux_scan_code:?}");
    let scan_code = linux_scan_code as u32;
    match wparam {
        WPARAM(p) if p == WM_KEYDOWN as usize => Some(KeyboardEvent::Key {
            time: 0,
            key: scan_code,
            state: 1,
        }),
        WPARAM(p) if p == WM_KEYUP as usize => Some(KeyboardEvent::Key {
            time: 0,
            key: scan_code,
            state: 0,
        }),
        WPARAM(p) if p == WM_SYSKEYDOWN as usize => Some(KeyboardEvent::Key {
            time: 0,
            key: scan_code,
            state: 1,
        }),
        WPARAM(p) if p == WM_SYSKEYUP as usize => Some(KeyboardEvent::Key {
            time: 0,
            key: scan_code,
            state: 0,
        }),
        _ => None,
    }
}

fn to_mouse_event(wparam: WPARAM, lparam: LPARAM) -> Option<PointerEvent> {
    let mouse_low_level: MSLLHOOKSTRUCT = unsafe { *(lparam.0 as *const MSLLHOOKSTRUCT) };
    match wparam {
        WPARAM(p) if p == WM_LBUTTONDOWN as usize => Some(PointerEvent::Button {
            time: 0,
            button: BTN_LEFT,
            state: 1,
        }),
        WPARAM(p) if p == WM_MBUTTONDOWN as usize => Some(PointerEvent::Button {
            time: 0,
            button: BTN_MIDDLE,
            state: 1,
        }),
        WPARAM(p) if p == WM_RBUTTONDOWN as usize => Some(PointerEvent::Button {
            time: 0,
            button: BTN_RIGHT,
            state: 1,
        }),
        WPARAM(p) if p == WM_LBUTTONUP as usize => Some(PointerEvent::Button {
            time: 0,
            button: BTN_LEFT,
            state: 0,
        }),
        WPARAM(p) if p == WM_MBUTTONUP as usize => Some(PointerEvent::Button {
            time: 0,
            button: BTN_MIDDLE,
            state: 0,
        }),
        WPARAM(p) if p == WM_RBUTTONUP as usize => Some(PointerEvent::Button {
            time: 0,
            button: BTN_RIGHT,
            state: 0,
        }),
        WPARAM(p) if p == WM_MOUSEMOVE as usize => {
            let (x, y) = (mouse_low_level.pt.x, mouse_low_level.pt.y);
            let (ex, ey) = ENTRY_POINT.get();
            let (dx, dy) = (x - ex, y - ey);
            let (dx, dy) = (dx as f64, dy as f64);
            Some(PointerEvent::Motion { time: 0, dx, dy })
        }
        WPARAM(p) if p == WM_MOUSEWHEEL as usize => Some(PointerEvent::AxisDiscrete120 {
            axis: 0,
            value: -(mouse_low_level.mouseData as i32 >> 16),
        }),
        WPARAM(p) if p == WM_XBUTTONDOWN as usize || p == WM_XBUTTONUP as usize => {
            let hb = mouse_low_level.mouseData >> 16;
            let button = match hb {
                1 => BTN_BACK,
                2 => BTN_FORWARD,
                _ => {
                    log::warn!("unknown mouse button");
                    return None;
                }
            };
            Some(PointerEvent::Button {
                time: 0,
                button,
                state: if p == WM_XBUTTONDOWN as usize { 1 } else { 0 },
            })
        }
        WPARAM(p) if p == WM_MOUSEHWHEEL as usize => Some(PointerEvent::AxisDiscrete120 {
            axis: 1, // Horizontal
            value: mouse_low_level.mouseData as i32 >> 16,
        }),
        w => {
            log::warn!("unknown mouse event: {w:?}");
            None
        }
    }
}
