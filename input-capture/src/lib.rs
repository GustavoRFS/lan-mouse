use std::{
    collections::{HashMap, HashSet, VecDeque},
    fmt::Display,
    mem::swap,
    task::{Poll, ready},
};

use async_trait::async_trait;
use futures::StreamExt;
use futures_core::Stream;

use input_event::{Event, KeyboardEvent, MonitorDescriptor, MonitorId, scancode};

pub use error::{CaptureCreationError, CaptureError, InputCaptureError};

pub mod error;

#[cfg(libei)]
mod libei;

#[cfg(target_os = "macos")]
mod macos;

#[cfg(layer_shell)]
mod layer_shell;

#[cfg(windows)]
mod windows;

#[cfg(x11)]
mod x11;

mod output_geometry;

/// fallback input capture (does not produce events)
mod dummy;

pub type CaptureHandle = u64;

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum CaptureEvent {
    /// capture on this capture handle is now active
    Begin,
    /// input event coming from capture handle
    Input(Event),
}

impl Display for CaptureEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CaptureEvent::Begin => write!(f, "begin capture"),
            CaptureEvent::Input(e) => write!(f, "{e}"),
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, Hash, PartialEq)]
pub enum Position {
    Left,
    Right,
    Top,
    Bottom,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct CapturePoint {
    pub monitor_id: Option<MonitorId>,
    pub edge: Position,
}

impl CapturePoint {
    pub fn directional(edge: Position) -> Self {
        Self {
            monitor_id: None,
            edge,
        }
    }
}

impl Position {
    pub fn opposite(&self) -> Self {
        match self {
            Position::Left => Self::Right,
            Position::Right => Self::Left,
            Position::Top => Self::Bottom,
            Position::Bottom => Self::Top,
        }
    }
}

impl Display for Position {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let pos = match self {
            Position::Left => "left",
            Position::Right => "right",
            Position::Top => "top",
            Position::Bottom => "bottom",
        };
        write!(f, "{pos}")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Backend {
    #[cfg(libei)]
    InputCapturePortal,
    #[cfg(layer_shell)]
    LayerShell,
    #[cfg(x11)]
    X11,
    #[cfg(windows)]
    Windows,
    #[cfg(target_os = "macos")]
    MacOs,
    Dummy,
}

impl Display for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            #[cfg(libei)]
            Backend::InputCapturePortal => write!(f, "input-capture-portal"),
            #[cfg(layer_shell)]
            Backend::LayerShell => write!(f, "layer-shell"),
            #[cfg(x11)]
            Backend::X11 => write!(f, "X11"),
            #[cfg(windows)]
            Backend::Windows => write!(f, "windows"),
            #[cfg(target_os = "macos")]
            Backend::MacOs => write!(f, "MacOS"),
            Backend::Dummy => write!(f, "dummy"),
        }
    }
}

pub struct InputCapture {
    /// capture backend
    capture: Box<dyn Capture>,
    /// keys pressed by active capture
    pressed_keys: HashSet<scancode::Linux>,
    /// map from a monitor edge to the clients registered for it
    point_map: HashMap<CapturePoint, Vec<CaptureHandle>>,
    /// map from a client to all of its monitor edges
    id_map: HashMap<CaptureHandle, Vec<CapturePoint>>,
    /// pending events
    pending: VecDeque<(CaptureHandle, CapturePoint, CaptureEvent)>,
}

impl InputCapture {
    /// create a new client with the given id
    pub async fn create(
        &mut self,
        id: CaptureHandle,
        points: Vec<CapturePoint>,
    ) -> Result<(), CaptureError> {
        assert!(!self.id_map.contains_key(&id));

        let mut unique_points = HashSet::new();
        let mut registered = Vec::with_capacity(points.len());
        for point in points {
            if !unique_points.insert(point.clone()) {
                continue;
            }

            if let Some(handles) = self.point_map.get_mut(&point) {
                handles.push(id);
            } else if let Err(error) = self.capture.create(point.clone()).await {
                let _ = self.capture.destroy(point.clone()).await;
                for registered_point in registered {
                    self.unregister_point(id, &registered_point).await;
                }
                return Err(error);
            } else {
                self.point_map.insert(point.clone(), vec![id]);
            }
            registered.push(point);
        }

        self.id_map.insert(id, registered);
        Ok(())
    }

    /// destroy the client with the given id, if it exists
    pub async fn destroy(&mut self, id: CaptureHandle) -> Result<(), CaptureError> {
        let points = self
            .id_map
            .remove(&id)
            .expect("no capture points for this handle");

        let mut first_error = None;
        for point in points {
            log::debug!("destroying capture {id} @ {point:?}");
            let remove_point = if let Some(handles) = self.point_map.get_mut(&point) {
                handles.retain(|&handle| handle != id);
                handles.is_empty()
            } else {
                false
            };

            if remove_point {
                self.point_map.remove(&point);
                if let Err(error) = self.capture.destroy(point).await {
                    first_error.get_or_insert(error);
                }
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    async fn unregister_point(&mut self, id: CaptureHandle, point: &CapturePoint) {
        let remove_point = if let Some(handles) = self.point_map.get_mut(point) {
            handles.retain(|&handle| handle != id);
            handles.is_empty()
        } else {
            false
        };
        if remove_point {
            self.point_map.remove(point);
            let _ = self.capture.destroy(point.clone()).await;
        }
    }

    /// release mouse
    pub async fn release(&mut self) -> Result<(), CaptureError> {
        self.pressed_keys.clear();
        self.capture.release().await
    }

    /// Drain and return every key the capture has forwarded as
    /// down-but-not-up. The caller is expected to synthesize key-up
    /// events to the remote peer for each — otherwise the peer
    /// retains phantom-held keys after capture is released. The
    /// canonical case is the release-bind chord
    /// (Ctrl+Shift+Alt+Meta): the down events were sent while
    /// capture was active, but the matching up events arrive after
    /// the local tap has flipped to passthrough and never reach
    /// the peer.
    pub fn take_pressed_keys(&mut self) -> HashSet<scancode::Linux> {
        std::mem::take(&mut self.pressed_keys)
    }

    /// destroy the input capture
    pub async fn terminate(&mut self) -> Result<(), CaptureError> {
        self.capture.terminate().await
    }

    /// creates a new [`InputCapture`]
    pub async fn new(backend: Option<Backend>) -> Result<Self, CaptureCreationError> {
        let capture = create(backend).await?;
        Ok(Self {
            capture,
            id_map: Default::default(),
            pending: Default::default(),
            point_map: Default::default(),
            pressed_keys: HashSet::new(),
        })
    }

    /// Return the active named outputs known by this capture backend.
    pub fn monitor_catalog(&self) -> Vec<MonitorDescriptor> {
        self.capture.monitor_catalog()
    }

    /// check whether the given keys are pressed
    pub fn keys_pressed(&self, keys: &[scancode::Linux]) -> bool {
        keys.iter().all(|k| self.pressed_keys.contains(k))
    }

    fn update_pressed_keys(&mut self, key: u32, state: u8) {
        if let Ok(scancode) = scancode::Linux::try_from(key) {
            log::debug!("key: {key}, state: {state}, scancode: {scancode:?}");
            match state {
                1 => self.pressed_keys.insert(scancode),
                _ => self.pressed_keys.remove(&scancode),
            };
        }
    }
}

impl Stream for InputCapture {
    type Item = Result<(CaptureHandle, CapturePoint, CaptureEvent), CaptureError>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        if let Some(e) = self.pending.pop_front() {
            return Poll::Ready(Some(Ok(e)));
        }

        // ready
        let event = ready!(self.capture.poll_next_unpin(cx));

        // stream closed
        let event = match event {
            Some(e) => e,
            None => return Poll::Ready(None),
        };

        // error occurred
        let (point, event) = match event {
            Ok(e) => e,
            Err(e) => return Poll::Ready(Some(Err(e))),
        };

        // handle key presses
        if let CaptureEvent::Input(Event::Keyboard(KeyboardEvent::Key { key, state, .. })) = event {
            self.update_pressed_keys(key, state);
        }

        let len = self.point_map.get(&point).map(|ids| ids.len()).unwrap_or(0);

        match len {
            0 => Poll::Pending,
            1 => Poll::Ready(Some(Ok((
                self.point_map.get(&point).expect("no id")[0],
                point,
                event,
            )))),
            _ => {
                let mut point_map = HashMap::new();
                swap(&mut self.point_map, &mut point_map);
                {
                    for &id in point_map.get(&point).expect("capture point") {
                        self.pending.push_back((id, point.clone(), event));
                    }
                }
                swap(&mut self.point_map, &mut point_map);

                Poll::Ready(Some(Ok(self.pending.pop_front().expect("event"))))
            }
        }
    }
}

#[async_trait]
trait Capture: Stream<Item = Result<(CapturePoint, CaptureEvent), CaptureError>> + Unpin {
    /// Start watching one output edge.
    async fn create(&mut self, point: CapturePoint) -> Result<(), CaptureError>;

    /// Stop watching one output edge.
    async fn destroy(&mut self, point: CapturePoint) -> Result<(), CaptureError>;

    fn monitor_catalog(&self) -> Vec<MonitorDescriptor> {
        Vec::new()
    }

    /// release mouse
    async fn release(&mut self) -> Result<(), CaptureError>;

    /// destroy the input capture
    async fn terminate(&mut self) -> Result<(), CaptureError>;
}

async fn create_backend(
    backend: Backend,
) -> Result<
    Box<dyn Capture<Item = Result<(CapturePoint, CaptureEvent), CaptureError>>>,
    CaptureCreationError,
> {
    match backend {
        #[cfg(libei)]
        Backend::InputCapturePortal => Ok(Box::new(libei::LibeiInputCapture::new().await?)),
        #[cfg(layer_shell)]
        Backend::LayerShell => Ok(Box::new(layer_shell::LayerShellInputCapture::new()?)),
        #[cfg(x11)]
        Backend::X11 => Ok(Box::new(x11::X11InputCapture::new()?)),
        #[cfg(windows)]
        Backend::Windows => Ok(Box::new(windows::WindowsInputCapture::new())),
        #[cfg(target_os = "macos")]
        Backend::MacOs => Ok(Box::new(macos::MacOSInputCapture::new().await?)),
        Backend::Dummy => Ok(Box::new(dummy::DummyInputCapture::new())),
    }
}

async fn create(
    backend: Option<Backend>,
) -> Result<
    Box<dyn Capture<Item = Result<(CapturePoint, CaptureEvent), CaptureError>>>,
    CaptureCreationError,
> {
    if let Some(backend) = backend {
        let b = create_backend(backend).await;
        if b.is_ok() {
            log::info!("using capture backend: {backend}");
        }
        return b;
    }

    for backend in [
        #[cfg(libei)]
        Backend::InputCapturePortal,
        #[cfg(layer_shell)]
        Backend::LayerShell,
        #[cfg(x11)]
        Backend::X11,
        #[cfg(windows)]
        Backend::Windows,
        #[cfg(target_os = "macos")]
        Backend::MacOs,
    ] {
        match create_backend(backend).await {
            Ok(b) => {
                log::info!("using capture backend: {backend}");
                return Ok(b);
            }
            Err(e) if e.cancelled_by_user() => return Err(e),
            Err(e) => log::warn!("{backend} input capture backend unavailable: {e}"),
        }
    }
    Err(CaptureCreationError::NoAvailableBackend)
}

#[cfg(test)]
mod tests {
    use std::{
        pin::Pin,
        sync::{Arc, Mutex},
        task::{Context, Poll},
    };

    use super::*;

    #[derive(Clone, Default)]
    struct MockCapture {
        created: Arc<Mutex<Vec<CapturePoint>>>,
        destroyed: Arc<Mutex<Vec<CapturePoint>>>,
    }

    #[async_trait]
    impl Capture for MockCapture {
        async fn create(&mut self, point: CapturePoint) -> Result<(), CaptureError> {
            self.created.lock().unwrap().push(point);
            Ok(())
        }

        async fn destroy(&mut self, point: CapturePoint) -> Result<(), CaptureError> {
            self.destroyed.lock().unwrap().push(point);
            Ok(())
        }

        async fn release(&mut self) -> Result<(), CaptureError> {
            Ok(())
        }

        async fn terminate(&mut self) -> Result<(), CaptureError> {
            Ok(())
        }
    }

    impl Stream for MockCapture {
        type Item = Result<(CapturePoint, CaptureEvent), CaptureError>;

        fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Poll::Pending
        }
    }

    fn point(output: &str, edge: Position) -> CapturePoint {
        CapturePoint {
            monitor_id: Some(MonitorId::new(output.into()).unwrap()),
            edge,
        }
    }

    fn input_capture(mock: &MockCapture) -> InputCapture {
        InputCapture {
            capture: Box::new(mock.clone()),
            pressed_keys: HashSet::new(),
            point_map: HashMap::new(),
            id_map: HashMap::new(),
            pending: VecDeque::new(),
        }
    }

    #[tokio::test]
    async fn one_handle_can_register_and_destroy_multiple_monitor_edges() {
        let mock = MockCapture::default();
        let mut capture = input_capture(&mock);
        let points = vec![
            point("DP-1", Position::Left),
            point("DP-2", Position::Right),
        ];

        capture.create(7, points.clone()).await.unwrap();
        assert_eq!(capture.id_map.get(&7), Some(&points));
        assert_eq!(mock.created.lock().unwrap().as_slice(), points.as_slice());

        capture.destroy(7).await.unwrap();
        assert!(capture.id_map.is_empty());
        assert!(capture.point_map.is_empty());
        assert_eq!(mock.destroyed.lock().unwrap().as_slice(), points.as_slice());
    }

    #[tokio::test]
    async fn shared_capture_point_lives_until_its_last_handle_is_removed() {
        let mock = MockCapture::default();
        let mut capture = input_capture(&mock);
        let shared = point("DP-1", Position::Right);

        capture.create(1, vec![shared.clone()]).await.unwrap();
        capture.create(2, vec![shared.clone()]).await.unwrap();
        assert_eq!(mock.created.lock().unwrap().as_slice(), &[shared.clone()]);

        capture.destroy(1).await.unwrap();
        assert!(mock.destroyed.lock().unwrap().is_empty());
        assert_eq!(capture.point_map.get(&shared), Some(&vec![2]));

        capture.destroy(2).await.unwrap();
        assert_eq!(mock.destroyed.lock().unwrap().as_slice(), &[shared]);
    }
}
