use crate::clipboard::ClipboardPayload;
use crate::config::local_commit;
use crate::listen::{LanMouseListener, ListenEvent, ListenerCreationError};
use futures::StreamExt;
use input_emulation::{EmulationHandle, InputEmulation, InputEmulationError};
use input_event::{Event, MonitorId};
use lan_mouse_ipc::Position as IpcPosition;
use lan_mouse_proto::{ClipboardUpdateId, KeyboardLayout, Position, ProtoEvent};
use local_channel::mpsc::{Receiver, Sender, channel};
use std::{
    cell::Cell,
    collections::HashMap,
    net::SocketAddr,
    rc::Rc,
    time::{Duration, Instant},
};
use tokio::{
    select,
    sync::oneshot,
    task::{JoinHandle, spawn_local},
};

/// emulation handling events received from a listener
pub(crate) struct Emulation {
    task: JoinHandle<()>,
    request_tx: Sender<EmulationRequest>,
    event_rx: Receiver<EmulationEvent>,
}

pub(crate) enum EmulationEvent {
    Connected {
        addr: SocketAddr,
        fingerprint: String,
    },
    ConnectionAttempt {
        fingerprint: String,
    },
    /// new connection
    Entered {
        /// address of the connection
        addr: SocketAddr,
        /// position of the connection
        pos: lan_mouse_ipc::Position,
        /// certificate fingerprint of the connection
        fingerprint: String,
    },
    EnteredMonitor {
        addr: SocketAddr,
        monitor_id: MonitorId,
        edge: IpcPosition,
        fingerprint: String,
    },
    /// connection closed
    Disconnected {
        addr: SocketAddr,
    },
    /// the port of the listener has changed
    PortChanged(Result<u16, ListenerCreationError>),
    /// emulation was disabled
    EmulationDisabled,
    /// emulation was enabled
    EmulationEnabled,
    /// capture should be released
    ReleaseNotify,
    /// peer sent us a Hello with its build commit hash. Used to
    /// populate `client_manager.peer_commit` from the listen side
    /// too — without this, peer-version visibility silently fails
    /// whenever the outgoing connection in the *other* direction is
    /// broken (one-way setups, asymmetric NAT, peer's TCP listener
    /// down). The connect-side path stays as the primary source;
    /// this is the defensive fallback.
    PeerHello {
        addr: SocketAddr,
        commit: [u8; 8],
    },
    Clipboard {
        addr: SocketAddr,
        update_id: ClipboardUpdateId,
        payload: ClipboardPayload,
    },
    PeerProtocol {
        addr: SocketAddr,
        fingerprint: String,
        event: ProtoEvent,
    },
}

enum EmulationRequest {
    Reenable,
    Release(SocketAddr),
    Clipboard {
        update_id: ClipboardUpdateId,
        payload: ClipboardPayload,
    },
    CancelClipboardTransfers,
    SendProto(SocketAddr, ProtoEvent),
    ChangePort(u16),
    Terminate,
}

impl Emulation {
    pub(crate) fn new(
        backend: Option<input_emulation::Backend>,
        options: input_emulation::EmulationOptions,
        listener: LanMouseListener,
    ) -> Self {
        let emulation_proxy = EmulationProxy::new(backend, options);
        let (request_tx, request_rx) = channel();
        let (event_tx, event_rx) = channel();
        let emulation_task = ListenTask {
            listener,
            emulation_proxy,
            request_rx,
            event_tx,
        };
        let task = spawn_local(emulation_task.run());
        Self {
            task,
            request_tx,
            event_rx,
        }
    }

    pub(crate) fn send_leave_event(&self, addr: SocketAddr) {
        self.request_tx
            .send(EmulationRequest::Release(addr))
            .expect("channel closed");
    }

    pub(crate) fn broadcast_clipboard(
        &self,
        update_id: ClipboardUpdateId,
        payload: ClipboardPayload,
    ) {
        self.request_tx
            .send(EmulationRequest::Clipboard { update_id, payload })
            .expect("channel closed");
    }

    pub(crate) fn cancel_clipboard_transfers(&self) {
        self.request_tx
            .send(EmulationRequest::CancelClipboardTransfers)
            .expect("channel closed");
    }

    pub(crate) fn send_proto(&self, addr: SocketAddr, event: ProtoEvent) {
        self.request_tx
            .send(EmulationRequest::SendProto(addr, event))
            .expect("channel closed");
    }

    pub(crate) fn reenable(&self) {
        self.request_tx
            .send(EmulationRequest::Reenable)
            .expect("channel closed");
    }

    pub(crate) fn request_port_change(&self, port: u16) {
        self.request_tx
            .send(EmulationRequest::ChangePort(port))
            .expect("channel closed")
    }

    pub(crate) async fn event(&mut self) -> EmulationEvent {
        self.event_rx.recv().await.expect("channel closed")
    }

    /// wait for termination
    pub(crate) async fn terminate(&mut self) {
        log::debug!("terminating emulation");
        self.request_tx
            .send(EmulationRequest::Terminate)
            .expect("channel closed");
        if let Err(e) = (&mut self.task).await {
            log::warn!("{e}");
        }
    }
}

struct ListenTask {
    listener: LanMouseListener,
    emulation_proxy: EmulationProxy,
    request_rx: Receiver<EmulationRequest>,
    event_tx: Sender<EmulationEvent>,
}

impl ListenTask {
    async fn run(mut self) {
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        let mut last_response = HashMap::new();
        let mut rejected_connections = HashMap::new();
        loop {
            select! {
                e = self.listener.next() => {match e {
                    Some(ListenEvent::Msg { event, addr }) => {
                        log::trace!("{event} <-<-<-<-<- {addr}");
                        last_response.insert(addr, Instant::now());
                        match event {
                            ProtoEvent::KeyboardLayout(layout) => {
                                if let Err(error) = self.emulation_proxy.set_keyboard_layout(addr, layout).await {
                                    log::warn!("failed to apply keyboard layout from {addr}: {error}");
                                }
                            }
                            ProtoEvent::Enter(pos) => {
                                if let Some(fingerprint) = self.listener.get_certificate_fingerprint(addr).await {
                                    log::info!("releasing capture: {addr} entered this device");
                                    self.event_tx.send(EmulationEvent::ReleaseNotify).expect("channel closed");
                                    self.listener.reply(addr, ProtoEvent::Ack(0)).await;
                                    self.event_tx.send(EmulationEvent::Entered{addr, pos: to_ipc_pos(pos), fingerprint}).expect("channel closed");
                                }
                            }
                            ProtoEvent::EnterMonitor { monitor_id, edge } => {
                                if let Some(fingerprint) = self.listener.get_certificate_fingerprint(addr).await {
                                    let edge = to_ipc_pos(edge);
                                    match self.emulation_proxy.enter_monitor(addr, monitor_id.clone(), edge).await {
                                        Ok(()) => {
                                            self.event_tx.send(EmulationEvent::ReleaseNotify).expect("channel closed");
                                            self.listener.reply(addr, ProtoEvent::Ack(0)).await;
                                            self.event_tx.send(EmulationEvent::EnteredMonitor {
                                                addr,
                                                monitor_id,
                                                edge,
                                                fingerprint,
                                            }).expect("channel closed");
                                        }
                                        Err(error) => log::warn!("unable to enter monitor on {addr}: {error}"),
                                    }
                                }
                            }
                            ProtoEvent::Leave(_) => {
                                self.emulation_proxy.remove(addr);
                                self.listener.reply(addr, ProtoEvent::Ack(0)).await;
                            }
                            ProtoEvent::Input(event) => self.emulation_proxy.consume(event, addr),
                            ProtoEvent::Ping => self.listener.reply(addr, ProtoEvent::Pong(self.emulation_proxy.emulation_active.get())).await,
                            // Peer's version handshake. Echo our own
                            // commit back so the peer's connect-side
                            // receive_loop populates its `peer_commit`,
                            // AND publish a PeerHello upward so our
                            // service can populate ours from the listen
                            // side too — the connect side is the primary
                            // path, but if the outbound direction is
                            // broken (one-way setup, NAT, peer's TCP
                            // listener down) the version display would
                            // otherwise silently say "unknown" while
                            // the peer is in fact happily talking to us.
                            ProtoEvent::Hello { commit } => {
                                self.listener.reply(addr, ProtoEvent::Hello { commit: local_commit() }).await;
                                self.event_tx.send(EmulationEvent::PeerHello { addr, commit }).expect("channel closed");
                            }
                            ProtoEvent::MonitorGridCapabilities { version } => {
                                self.listener.reply(addr, ProtoEvent::MonitorGridCapabilities { version: 1 }).await;
                                if let Some(fingerprint) = self.listener.get_certificate_fingerprint(addr).await {
                                    self.event_tx.send(EmulationEvent::PeerProtocol {
                                        addr,
                                        fingerprint,
                                        event: ProtoEvent::MonitorGridCapabilities { version },
                                    }).expect("channel closed");
                                }
                            }
                            event @ ProtoEvent::MonitorCatalogRequest
                            | event @ ProtoEvent::MonitorCatalogChunk { .. }
                            | event @ ProtoEvent::GridSnapshotChunk { .. }
                            | event @ ProtoEvent::GridSnapshotAck { .. } => {
                                if let Some(fingerprint) = self.listener.get_certificate_fingerprint(addr).await {
                                    self.event_tx.send(EmulationEvent::PeerProtocol { addr, fingerprint, event }).expect("channel closed");
                                }
                            }
                            _ => {}
                        }
                    }
                    Some(ListenEvent::Clipboard { update_id, payload, addr }) => {
                        self.event_tx.send(EmulationEvent::Clipboard { addr, update_id, payload }).expect("channel closed");
                    }
                    Some(ListenEvent::Accept { addr, fingerprint }) => {
                        self.event_tx.send(EmulationEvent::Connected { addr, fingerprint }).expect("channel closed");
                    }
                    Some(ListenEvent::Rejected { fingerprint }) => {
                        if rejected_connections.insert(fingerprint.clone(), Instant::now())
                            .is_none_or(|i| i.elapsed() >= Duration::from_secs(2)) {
                                self.event_tx.send(EmulationEvent::ConnectionAttempt { fingerprint }).expect("channel closed");
                            }
                    }
                    None => break
                }}
                event = self.emulation_proxy.event() => {
                    self.event_tx.send(event).expect("channel closed");
                }
                request = self.request_rx.recv() => match request.expect("channel closed") {
                    // reenable emulation
                    EmulationRequest::Reenable => self.emulation_proxy.reenable(),
                    // notify the other end that we hit a barrier (should release capture)
                    EmulationRequest::Release(addr) => self.listener.reply(addr, ProtoEvent::Leave(0)).await,
                    EmulationRequest::Clipboard { update_id, payload } => {
                        self.listener.broadcast_clipboard(update_id, payload).await;
                    }
                    EmulationRequest::CancelClipboardTransfers => {
                        self.listener.cancel_clipboard_transfers().await;
                    }
                    EmulationRequest::SendProto(addr, event) => self.listener.reply(addr, event).await,
                    EmulationRequest::ChangePort(port) => {
                        self.listener.request_port_change(port);
                        let result = self.listener.port_changed().await;
                        self.event_tx.send(EmulationEvent::PortChanged(result)).expect("channel closed");
                    }
                    EmulationRequest::Terminate => break,
                },
                _ = interval.tick() => {
                    last_response.retain(|&addr,instant| {
                        if instant.elapsed() > Duration::from_secs(1) {
                            log::warn!("releasing keys: {addr} not responding!");
                            self.emulation_proxy.remove(addr);
                            self.event_tx.send(EmulationEvent::Disconnected { addr }).expect("channel closed");
                            false
                        } else {
                            true
                        }
                    });
                }
            }
        }
        self.listener.terminate().await;
        self.emulation_proxy.terminate().await;
    }
}

/// proxy handling the actual input emulation,
/// discarding events when it is disabled
pub(crate) struct EmulationProxy {
    emulation_active: Rc<Cell<bool>>,
    exit_requested: Rc<Cell<bool>>,
    request_tx: Sender<ProxyRequest>,
    event_rx: Receiver<EmulationEvent>,
    task: JoinHandle<()>,
}

enum ProxyRequest {
    Input(Event, SocketAddr),
    Remove(SocketAddr),
    EnterMonitor {
        addr: SocketAddr,
        monitor_id: MonitorId,
        edge: IpcPosition,
        response: oneshot::Sender<Result<(), String>>,
    },
    SetKeyboardLayout {
        addr: SocketAddr,
        layout: Option<KeyboardLayout>,
        response: oneshot::Sender<Result<(), String>>,
    },
    Terminate,
    Reenable,
}

impl EmulationProxy {
    fn new(
        backend: Option<input_emulation::Backend>,
        options: input_emulation::EmulationOptions,
    ) -> Self {
        let (request_tx, request_rx) = channel();
        let (event_tx, event_rx) = channel();
        let emulation_active = Rc::new(Cell::new(false));
        let exit_requested = Rc::new(Cell::new(false));
        let emulation_task = EmulationTask {
            backend,
            options,
            exit_requested: exit_requested.clone(),
            request_rx,
            event_tx,
            handles: Default::default(),
            next_id: 0,
            keyboard_layout_source: None,
            pending_monitor_entries: Vec::new(),
        };
        let task = spawn_local(emulation_task.run());
        Self {
            emulation_active,
            exit_requested,
            request_tx,
            task,
            event_rx,
        }
    }

    async fn event(&mut self) -> EmulationEvent {
        let event = self.event_rx.recv().await.expect("channel closed");
        if let EmulationEvent::EmulationEnabled = event {
            self.emulation_active.replace(true);
        }
        if let EmulationEvent::EmulationDisabled = event {
            self.emulation_active.replace(false);
        }
        event
    }

    fn consume(&self, event: Event, addr: SocketAddr) {
        // ignore events if emulation is currently disabled
        if self.emulation_active.get() {
            self.request_tx
                .send(ProxyRequest::Input(event, addr))
                .expect("channel closed");
        }
    }

    fn remove(&self, addr: SocketAddr) {
        self.request_tx
            .send(ProxyRequest::Remove(addr))
            .expect("channel closed");
    }

    async fn enter_monitor(
        &self,
        addr: SocketAddr,
        monitor_id: MonitorId,
        edge: IpcPosition,
    ) -> Result<(), String> {
        let (response, response_rx) = oneshot::channel();
        self.request_tx
            .send(ProxyRequest::EnterMonitor {
                addr,
                monitor_id,
                edge,
                response,
            })
            .expect("channel closed");
        response_rx
            .await
            .unwrap_or_else(|_| Err("input emulation stopped before monitor entry".into()))
    }

    async fn set_keyboard_layout(
        &self,
        addr: SocketAddr,
        layout: Option<KeyboardLayout>,
    ) -> Result<(), String> {
        let (response, response_rx) = oneshot::channel();
        self.request_tx
            .send(ProxyRequest::SetKeyboardLayout {
                addr,
                layout,
                response,
            })
            .expect("channel closed");
        response_rx
            .await
            .unwrap_or_else(|_| Err("input emulation stopped before applying the layout".into()))
    }

    fn reenable(&self) {
        self.request_tx
            .send(ProxyRequest::Reenable)
            .expect("channel closed");
    }

    async fn terminate(&mut self) {
        self.exit_requested.replace(true);
        self.request_tx
            .send(ProxyRequest::Terminate)
            .expect("channel closed");
        let _ = (&mut self.task).await;
    }
}

struct EmulationTask {
    backend: Option<input_emulation::Backend>,
    options: input_emulation::EmulationOptions,
    exit_requested: Rc<Cell<bool>>,
    request_rx: Receiver<ProxyRequest>,
    event_tx: Sender<EmulationEvent>,
    handles: HashMap<SocketAddr, EmulationHandle>,
    next_id: EmulationHandle,
    keyboard_layout_source: Option<(SocketAddr, Option<KeyboardLayout>)>,
    pending_monitor_entries: Vec<PendingMonitorEntry>,
}

struct PendingMonitorEntry {
    addr: SocketAddr,
    monitor_id: MonitorId,
    edge: IpcPosition,
    response: oneshot::Sender<Result<(), String>>,
}

impl EmulationTask {
    async fn run(mut self) {
        loop {
            if let Err(e) = self.do_emulation().await {
                log::warn!("input emulation exited: {e}");
            }
            if self.exit_requested.get() {
                break;
            }
            // wait for reenable request
            loop {
                match self.request_rx.recv().await.expect("channel closed") {
                    ProxyRequest::Reenable => break,
                    ProxyRequest::Terminate => return,
                    ProxyRequest::Input(..) => { /* emulation inactive => ignore */ }
                    ProxyRequest::EnterMonitor { response, .. } => {
                        let _ = response.send(Err("input emulation is disabled".into()));
                    }
                    ProxyRequest::Remove(addr) => {
                        self.handles.remove(&addr);
                        if self
                            .keyboard_layout_source
                            .is_some_and(|(source, _)| source == addr)
                        {
                            self.keyboard_layout_source = None;
                        }
                    }
                    ProxyRequest::SetKeyboardLayout {
                        addr,
                        layout,
                        response,
                    } => {
                        self.keyboard_layout_source = Some((addr, layout));
                        let _ = response.send(Ok(()));
                    }
                }
            }
        }
    }

    async fn do_emulation(&mut self) -> Result<(), InputEmulationError> {
        log::info!("creating input emulation ...");
        let creation = InputEmulation::new(self.backend, self.options.clone());
        tokio::pin!(creation);
        let mut pending_layout_responses = Vec::new();
        let mut emulation = loop {
            tokio::select! {
                result = &mut creation => break result?,
                request = self.request_rx.recv() => match request.expect("channel closed") {
                    ProxyRequest::Terminate => return Ok(()),
                    ProxyRequest::SetKeyboardLayout { addr, layout, response } => {
                        self.keyboard_layout_source = Some((addr, layout));
                        pending_layout_responses.push(response);
                    }
                    ProxyRequest::Remove(addr) => {
                        self.handles.remove(&addr);
                        if self.keyboard_layout_source.is_some_and(|(source, _)| source == addr) {
                            self.keyboard_layout_source = None;
                        }
                    }
                    ProxyRequest::EnterMonitor {
                        addr,
                        monitor_id,
                        edge,
                        response,
                    } => self.pending_monitor_entries.push(PendingMonitorEntry {
                        addr,
                        monitor_id,
                        edge,
                        response,
                    }),
                    ProxyRequest::Input(..) | ProxyRequest::Reenable => {}
                }
            }
        };

        if let Some((_, layout)) = self.keyboard_layout_source {
            if let Err(error) = self.apply_keyboard_layout(&mut emulation, layout).await {
                for response in pending_layout_responses {
                    let _ = response.send(Err(error.to_string()));
                }
                return Err(error);
            }
        }
        for response in pending_layout_responses {
            let _ = response.send(Ok(()));
        }

        // used to send enabled and disabled events
        let _emulation_guard = DropGuard::new(
            self.event_tx.clone(),
            EmulationEvent::EmulationEnabled,
            EmulationEvent::EmulationDisabled,
        );

        // create active handles
        match self.create_clients(&mut emulation).await {
            Ok(true) => {
                emulation.terminate().await;
                return Ok(());
            }
            Ok(false) => {}
            Err(error) => {
                emulation.terminate().await;
                return Err(error);
            }
        }

        for pending in std::mem::take(&mut self.pending_monitor_entries) {
            let result = self
                .enter_monitor_for(
                    &mut emulation,
                    pending.addr,
                    &pending.monitor_id,
                    pending.edge,
                )
                .await;
            let _ = pending.response.send(result);
        }

        let res = self.do_emulation_session(&mut emulation).await;
        // FIXME replace with async drop when stabilized
        emulation.terminate().await;
        res
    }

    async fn create_clients(
        &mut self,
        emulation: &mut InputEmulation,
    ) -> Result<bool, InputEmulationError> {
        let handles = self.handles.values().copied().collect::<Vec<_>>();
        let mut pending_layout_responses = Vec::new();
        for handle in handles {
            tokio::select! {
                _ = emulation.create(handle) => {},
                request = self.request_rx.recv() => match request.expect("channel closed") {
                    ProxyRequest::Terminate => return Ok(true),
                    ProxyRequest::SetKeyboardLayout { addr, layout, response } => {
                        self.keyboard_layout_source = Some((addr, layout));
                        pending_layout_responses.push(response);
                    }
                    ProxyRequest::EnterMonitor {
                        addr,
                        monitor_id,
                        edge,
                        response,
                    } => self.pending_monitor_entries.push(PendingMonitorEntry {
                        addr,
                        monitor_id,
                        edge,
                        response,
                    }),
                    ProxyRequest::Input(..) | ProxyRequest::Remove(..) | ProxyRequest::Reenable => {}
                }
            }
        }
        if !pending_layout_responses.is_empty() {
            if let Some((_, layout)) = self.keyboard_layout_source {
                self.apply_keyboard_layout(emulation, layout).await?;
            }
            for response in pending_layout_responses {
                let _ = response.send(Ok(()));
            }
        }
        Ok(false)
    }

    async fn apply_keyboard_layout(
        &self,
        emulation: &mut InputEmulation,
        layout: Option<KeyboardLayout>,
    ) -> Result<(), InputEmulationError> {
        let (layout, variant) = match layout {
            Some(layout) => (Some(layout.xkb_name()), None),
            None => (
                self.options.keyboard_layout.as_deref(),
                self.options.keyboard_variant.as_deref(),
            ),
        };
        emulation.set_keyboard_layout(layout, variant).await?;
        Ok(())
    }

    async fn enter_monitor_for(
        &mut self,
        emulation: &mut InputEmulation,
        addr: SocketAddr,
        monitor_id: &MonitorId,
        edge: IpcPosition,
    ) -> Result<(), String> {
        if !self.handles.contains_key(&addr) {
            let handle = self.next_id;
            self.next_id = self.next_id.wrapping_add(1);
            emulation.create(handle).await;
            self.handles.insert(addr, handle);
        }
        let handle = self.handles[&addr];
        emulation
            .enter_monitor(handle, monitor_id, edge)
            .await
            .map_err(|error| error.to_string())
    }

    async fn do_emulation_session(
        &mut self,
        emulation: &mut InputEmulation,
    ) -> Result<(), InputEmulationError> {
        loop {
            tokio::select! {
                e = self.request_rx.recv() => match e.expect("channel closed") {
                    ProxyRequest::Input(event, addr) => {
                        let handle = match self.handles.get(&addr) {
                            Some(&handle) => handle,
                            None => {
                                let handle = self.next_id;
                                self.next_id += 1;
                                emulation.create(handle).await;
                                self.handles.insert(addr, handle);
                                handle
                            }
                        };
                        emulation.consume(event, handle).await?;
                    },
                    ProxyRequest::EnterMonitor { addr, monitor_id, edge, response } => {
                        let result = self.enter_monitor_for(emulation, addr, &monitor_id, edge).await;
                        let _ = response.send(result);
                    }
                    ProxyRequest::Remove(addr) => {
                        if let Some(handle) = self.handles.remove(&addr) {
                            emulation.destroy(handle).await;
                        }
                        if self.keyboard_layout_source.is_some_and(|(source, _)| source == addr) {
                            self.keyboard_layout_source = None;
                            self.apply_keyboard_layout(emulation, None).await?;
                        }
                    }
                    ProxyRequest::SetKeyboardLayout { addr, layout, response } => {
                        let changed = self.keyboard_layout_source != Some((addr, layout));
                        let result = if changed {
                            self.apply_keyboard_layout(emulation, layout).await
                        } else {
                            Ok(())
                        };
                        if result.is_ok() {
                            self.keyboard_layout_source = Some((addr, layout));
                        }
                        let result = result.map_err(|error| error.to_string());
                        let _ = response.send(result);
                    }
                    ProxyRequest::Terminate => break Ok(()),
                    ProxyRequest::Reenable => continue,
                },
            }
        }
    }
}

fn to_ipc_pos(pos: Position) -> lan_mouse_ipc::Position {
    match pos {
        Position::Left => lan_mouse_ipc::Position::Left,
        Position::Right => lan_mouse_ipc::Position::Right,
        Position::Top => lan_mouse_ipc::Position::Top,
        Position::Bottom => lan_mouse_ipc::Position::Bottom,
    }
}

struct DropGuard<T> {
    tx: Sender<T>,
    on_drop: Option<T>,
}

impl<T> DropGuard<T> {
    fn new(tx: Sender<T>, on_new: T, on_drop: T) -> Self {
        tx.send(on_new).expect("channel closed");
        let on_drop = Some(on_drop);
        Self { tx, on_drop }
    }
}

impl<T> Drop for DropGuard<T> {
    fn drop(&mut self) {
        self.tx
            .send(self.on_drop.take().expect("item"))
            .expect("channel closed");
    }
}
