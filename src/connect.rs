use crate::config::local_commit;
use crate::{
    client::ClientManager,
    clipboard::{ClipboardPayload, ClipboardPeer, SharedClipboardReassembler},
};
use lan_mouse_ipc::{ClientHandle, DEFAULT_PORT};
use lan_mouse_proto::{
    AudioControl, AudioMessage, ClipboardFrame, ClipboardUpdateId, MAX_AUDIO_DATAGRAM_SIZE,
    MAX_EVENT_SIZE, ProtoEvent, decode_audio_message, is_audio_message,
};
use local_channel::mpsc::{Receiver, Sender, channel};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    io,
    net::SocketAddr,
    rc::Rc,
    sync::Arc,
    time::Duration,
};
use thiserror::Error;
use tokio::{
    net::UdpSocket,
    sync::Mutex,
    task::{JoinSet, spawn_local},
};
use webrtc_dtls::{
    config::{Config, ExtendedMasterSecretType},
    conn::DTLSConn,
    crypto::Certificate,
};
use webrtc_util::Conn;

#[derive(Debug, Error)]
pub(crate) enum LanMouseConnectionError {
    #[error(transparent)]
    Bind(#[from] io::Error),
    #[error(transparent)]
    Dtls(#[from] webrtc_dtls::Error),
    #[error(transparent)]
    Webrtc(#[from] webrtc_util::Error),
    #[error(transparent)]
    AudioProtocol(#[from] lan_mouse_proto::ProtocolError),
    #[error("not connected")]
    NotConnected,
    #[error("emulation is disabled on the target device")]
    TargetEmulationDisabled,
    #[error("Connection timed out")]
    Timeout,
}

const DEFAULT_CONNECTION_TIMEOUT: Duration = Duration::from_secs(5);
type ArcConn = Arc<dyn Conn + Send + Sync>;

#[derive(Clone)]
struct ClientConnection {
    conn: ArcConn,
    clipboard: Arc<ClipboardPeer>,
}

pub(crate) enum NetworkEvent {
    Proto(ProtoEvent),
    Audio(AudioMessage),
    Disconnected,
    Clipboard {
        update_id: ClipboardUpdateId,
        payload: ClipboardPayload,
    },
}

async fn connect(
    addr: SocketAddr,
    cert: Certificate,
) -> Result<(Arc<dyn Conn + Sync + Send>, SocketAddr), (SocketAddr, LanMouseConnectionError)> {
    log::info!("connecting to {addr} ...");
    let conn = Arc::new(
        UdpSocket::bind("0.0.0.0:0")
            .await
            .map_err(|e| (addr, e.into()))?,
    );
    conn.connect(addr).await.map_err(|e| (addr, e.into()))?;
    let config = Config {
        certificates: vec![cert],
        server_name: "ignored".to_owned(),
        insecure_skip_verify: true,
        extended_master_secret: ExtendedMasterSecretType::Require,
        ..Default::default()
    };
    let timeout = tokio::time::sleep(DEFAULT_CONNECTION_TIMEOUT);
    tokio::select! {
        _ = timeout => Err((addr, LanMouseConnectionError::Timeout)),
        result = DTLSConn::new(conn, config, true, None) => match result {
            Ok(dtls_conn) => Ok((Arc::new(dtls_conn), addr)),
            Err(e) => Err((addr, e.into())),
        }
    }
}

async fn connect_any(
    addrs: &[SocketAddr],
    cert: Certificate,
) -> Result<(Arc<dyn Conn + Send + Sync>, SocketAddr), LanMouseConnectionError> {
    let mut joinset = JoinSet::new();
    for &addr in addrs {
        joinset.spawn_local(connect(addr, cert.clone()));
    }
    loop {
        match joinset.join_next().await {
            None => return Err(LanMouseConnectionError::NotConnected),
            Some(r) => match r.expect("join error") {
                Ok(conn) => return Ok(conn),
                Err((a, e)) => {
                    log::warn!("failed to connect to {a}: `{e}`")
                }
            },
        };
    }
}

pub(crate) struct LanMouseConnection {
    cert: Certificate,
    client_manager: ClientManager,
    conns: Rc<Mutex<HashMap<SocketAddr, ClientConnection>>>,
    connecting: Rc<Mutex<HashSet<ClientHandle>>>,
    pending_audio_controls: Rc<RefCell<HashMap<ClientHandle, AudioControl>>>,
    recv_rx: Receiver<(ClientHandle, NetworkEvent)>,
    recv_tx: Sender<(ClientHandle, NetworkEvent)>,
    audio_rx: tokio::sync::mpsc::Receiver<(ClientHandle, AudioMessage)>,
    audio_tx: tokio::sync::mpsc::Sender<(ClientHandle, AudioMessage)>,
    ping_response: Rc<RefCell<HashSet<SocketAddr>>>,
    reassembler: SharedClipboardReassembler,
}

impl LanMouseConnection {
    pub(crate) fn new(
        cert: Certificate,
        client_manager: ClientManager,
        reassembler: SharedClipboardReassembler,
    ) -> Self {
        let (recv_tx, recv_rx) = channel();
        let (audio_tx, audio_rx) = tokio::sync::mpsc::channel(4);
        Self {
            cert,
            client_manager,
            conns: Default::default(),
            connecting: Default::default(),
            pending_audio_controls: Default::default(),
            recv_rx,
            recv_tx,
            audio_rx,
            audio_tx,
            ping_response: Default::default(),
            reassembler,
        }
    }

    pub(crate) async fn recv(&mut self) -> (ClientHandle, NetworkEvent) {
        tokio::select! {
            event = self.recv_rx.recv() => event.expect("channel closed"),
            audio = self.audio_rx.recv() => {
                let (handle, message) = audio.expect("audio channel closed");
                (handle, NetworkEvent::Audio(message))
            }
        }
    }

    pub(crate) async fn broadcast_clipboard(
        &self,
        update_id: ClipboardUpdateId,
        payload: ClipboardPayload,
    ) {
        let mut peers = Vec::new();
        for handle in self.client_manager.active_clients() {
            if let Some(addr) = self.client_manager.active_addr(handle) {
                let conns = self.conns.lock().await;
                if let Some(connection) = conns.get(&addr) {
                    peers.push(connection.clipboard.clone());
                }
            }
        }

        for peer in peers {
            let payload = payload.clone();
            spawn_local(async move { peer.send(update_id, &payload).await });
        }
    }

    pub(crate) async fn cancel_clipboard_transfers(&self) {
        let peers = self
            .conns
            .lock()
            .await
            .values()
            .map(|connection| connection.clipboard.clone())
            .collect::<Vec<_>>();
        for peer in peers {
            peer.cancel_pending().await;
        }
    }

    pub(crate) async fn send(
        &self,
        event: ProtoEvent,
        handle: ClientHandle,
    ) -> Result<(), LanMouseConnectionError> {
        let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = event.into();
        let buf = &buf[..len];
        if let Some(addr) = self.client_manager.active_addr(handle) {
            let connection = {
                let conns = self.conns.lock().await;
                conns.get(&addr).cloned()
            };
            if let Some(connection) = connection {
                if !self.client_manager.alive(handle) {
                    return Err(LanMouseConnectionError::TargetEmulationDisabled);
                }
                match connection.conn.send(buf).await {
                    Ok(_) => {}
                    Err(e) => {
                        log::warn!("client {handle} failed to send: {e}");
                        disconnect(
                            &self.client_manager,
                            handle,
                            addr,
                            &self.conns,
                            &self.recv_tx,
                        )
                        .await;
                    }
                }
                log::trace!("{event} >->->->->- {addr}");
                return Ok(());
            }
        }

        // check if we are already trying to connect
        let mut connecting = self.connecting.lock().await;
        if !connecting.contains(&handle) {
            connecting.insert(handle);
            // connect in the background
            spawn_local(connect_to_handle(
                self.client_manager.clone(),
                self.cert.clone(),
                handle,
                self.conns.clone(),
                self.connecting.clone(),
                self.recv_tx.clone(),
                self.audio_tx.clone(),
                self.pending_audio_controls.clone(),
                self.ping_response.clone(),
                self.reassembler.clone(),
            ));
        }
        Err(LanMouseConnectionError::NotConnected)
    }

    pub(crate) async fn send_audio(
        &self,
        message: AudioMessage,
        handle: ClientHandle,
    ) -> Result<(), LanMouseConnectionError> {
        let pending_control = match &message {
            AudioMessage::Control(control) => Some(control.clone()),
            AudioMessage::Data(_) => None,
        };
        let is_control = pending_control.is_some();
        let bytes = lan_mouse_proto::encode_audio_message(&message)?;
        if let Some(addr) = self.client_manager.active_addr(handle) {
            let connection = {
                let conns = self.conns.lock().await;
                conns.get(&addr).cloned()
            };
            if let Some(connection) = connection {
                if let Err(error) = connection.conn.send(&bytes).await {
                    if let Some(control) = &pending_control {
                        self.pending_audio_controls
                            .borrow_mut()
                            .insert(handle, control.clone());
                    }
                    log::warn!("client {handle} failed to send audio: {error}");
                    disconnect(
                        &self.client_manager,
                        handle,
                        addr,
                        &self.conns,
                        &self.recv_tx,
                    )
                    .await;
                    return Err(error.into());
                }
                if let Some(control) = &pending_control {
                    let mut pending = self.pending_audio_controls.borrow_mut();
                    if pending.get(&handle) == Some(control) {
                        pending.remove(&handle);
                    }
                }
                log::trace!("audio frame sent to {addr}");
                return Ok(());
            }
        }

        if let Some(control) = &pending_control {
            self.pending_audio_controls
                .borrow_mut()
                .insert(handle, control.clone());
        }

        let mut connecting = self.connecting.lock().await;
        if !connecting.contains(&handle) {
            connecting.insert(handle);
            spawn_local(connect_to_handle(
                self.client_manager.clone(),
                self.cert.clone(),
                handle,
                self.conns.clone(),
                self.connecting.clone(),
                self.recv_tx.clone(),
                self.audio_tx.clone(),
                self.pending_audio_controls.clone(),
                self.ping_response.clone(),
                self.reassembler.clone(),
            ));
        }
        if is_control {
            log::debug!("queued audio control for client {handle} until connected");
            return Ok(());
        }
        Err(LanMouseConnectionError::NotConnected)
    }
}

#[allow(clippy::too_many_arguments)]
async fn connect_to_handle(
    client_manager: ClientManager,
    cert: Certificate,
    handle: ClientHandle,
    conns: Rc<Mutex<HashMap<SocketAddr, ClientConnection>>>,
    connecting: Rc<Mutex<HashSet<ClientHandle>>>,
    tx: Sender<(ClientHandle, NetworkEvent)>,
    audio_tx: tokio::sync::mpsc::Sender<(ClientHandle, AudioMessage)>,
    pending_audio_controls: Rc<RefCell<HashMap<ClientHandle, AudioControl>>>,
    ping_response: Rc<RefCell<HashSet<SocketAddr>>>,
    reassembler: SharedClipboardReassembler,
) -> Result<(), LanMouseConnectionError> {
    log::info!("client {handle} connecting ...");
    // sending did not work, figure out active conn.
    if let Some(addrs) = client_manager.get_ips(handle) {
        let port = client_manager.get_port(handle).unwrap_or(DEFAULT_PORT);
        let addrs = addrs
            .into_iter()
            .map(|a| SocketAddr::new(a, port))
            .collect::<Vec<_>>();
        log::info!("client ({handle}) connecting ... (ips: {addrs:?})");
        let res = connect_any(&addrs, cert).await;
        let (conn, addr) = match res {
            Ok(c) => c,
            Err(e) => {
                connecting.lock().await.remove(&handle);
                return Err(e);
            }
        };
        log::info!("client ({handle}) connected @ {addr}");
        client_manager.set_active_addr(handle, Some(addr));
        let clipboard = Arc::new(ClipboardPeer::new(conn.clone(), addr, reassembler));
        clipboard.advertise().await;
        conns.lock().await.insert(
            addr,
            ClientConnection {
                conn: conn.clone(),
                clipboard: clipboard.clone(),
            },
        );
        let pending_control = pending_audio_controls.borrow_mut().remove(&handle);
        if let Some(control) = pending_control {
            let bytes =
                lan_mouse_proto::encode_audio_message(&AudioMessage::Control(control.clone()))?;
            if let Err(error) = conn.send(&bytes).await {
                let mut pending = pending_audio_controls.borrow_mut();
                pending.entry(handle).or_insert(control);
                log::warn!("client {handle} failed to send queued audio control: {error}");
            } else {
                log::debug!("sent queued audio control to client {handle}");
            }
        }
        connecting.lock().await.remove(&handle);

        // Best-effort version handshake. Send our commit hash once
        // immediately after the DTLS handshake; the listen side
        // mirrors a Hello back so the receive loop can populate
        // `peer_commit`. Old peers will silently skip this event
        // per the forward-compat handler in [`receive_loop`].
        let (buf, len) = ProtoEvent::Hello {
            commit: local_commit(),
        }
        .into();
        if let Err(e) = conn.send(&buf[..len]).await {
            log::debug!("hello send to {addr} failed: {e}");
        }

        // poll connection for active
        spawn_local(ping_pong(addr, conn.clone(), ping_response.clone()));

        // receiver
        spawn_local(receive_loop(
            client_manager,
            handle,
            addr,
            conn,
            clipboard,
            conns,
            tx,
            audio_tx,
            ping_response.clone(),
        ));
        return Ok(());
    }
    connecting.lock().await.remove(&handle);
    Err(LanMouseConnectionError::NotConnected)
}

async fn ping_pong(
    addr: SocketAddr,
    conn: Arc<dyn Conn + Send + Sync>,
    ping_response: Rc<RefCell<HashSet<SocketAddr>>>,
) {
    loop {
        let (buf, len) = ProtoEvent::Ping.into();

        // send 4 pings, at least one must be answered
        for _ in 0..4 {
            if let Err(e) = conn.send(&buf[..len]).await {
                log::warn!("{addr}: send error `{e}`, closing connection");
                let _ = conn.close().await;
                break;
            }
            log::trace!("PING >->->->->- {addr}");

            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        if !ping_response.borrow_mut().remove(&addr) {
            log::warn!("{addr} did not respond, closing connection");
            let _ = conn.close().await;
            return;
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn receive_loop(
    client_manager: ClientManager,
    handle: ClientHandle,
    addr: SocketAddr,
    conn: ArcConn,
    clipboard: Arc<ClipboardPeer>,
    conns: Rc<Mutex<HashMap<SocketAddr, ClientConnection>>>,
    tx: Sender<(ClientHandle, NetworkEvent)>,
    audio_tx: tokio::sync::mpsc::Sender<(ClientHandle, AudioMessage)>,
    ping_response: Rc<RefCell<HashSet<SocketAddr>>>,
) {
    let mut buf = [0u8; 2048];
    loop {
        let len = match conn.recv(&mut buf).await {
            Ok(len) => len,
            Err(_) => break,
        };
        if ClipboardFrame::is_clipboard_frame(&buf[..len]) {
            match clipboard.handle_frame(&buf[..len]).await {
                Ok(Some((update_id, payload))) => {
                    tx.send((handle, NetworkEvent::Clipboard { update_id, payload }))
                        .expect("channel closed");
                }
                Ok(None) => {}
                Err(error) => log::debug!("ignoring invalid clipboard frame from {addr}: {error}"),
            }
            continue;
        }
        if is_audio_message(&buf[..len]) {
            if len > MAX_AUDIO_DATAGRAM_SIZE {
                log::debug!("ignoring oversized audio frame from {addr}: {len} bytes");
                continue;
            }
            match decode_audio_message(&buf[..len]) {
                Ok(message) => {
                    ping_response.borrow_mut().insert(addr);
                    if let Err(error) = audio_tx.try_send((handle, message)) {
                        log::debug!("dropping audio frame from {addr}: {error}");
                    }
                }
                Err(error) => log::debug!("ignoring invalid audio frame from {addr}: {error}"),
            }
            continue;
        }
        let mut event_buf = [0u8; MAX_EVENT_SIZE];
        let event_len = len.min(MAX_EVENT_SIZE);
        event_buf[..event_len].copy_from_slice(&buf[..event_len]);
        match event_buf.try_into() {
            Ok(event) => {
                log::trace!("{addr} <==<==<== {event}");
                match event {
                    ProtoEvent::Pong(b) => {
                        client_manager.set_active_addr(handle, Some(addr));
                        client_manager.set_alive(handle, b);
                        ping_response.borrow_mut().insert(addr);
                    }
                    ProtoEvent::Hello { commit } => {
                        client_manager.set_peer_commit(handle, Some(commit));
                    }
                    event => tx
                        .send((handle, NetworkEvent::Proto(event)))
                        .expect("channel closed"),
                }
            }
            // Skip undecodable datagrams without dropping the
            // connection. Each DTLS recv is one framed message, so
            // skipping is safe and keeps us forward-compatible with
            // peers that send event types we don't yet know about.
            Err(e) => log::debug!("ignoring undecodable event from {addr}: {e}"),
        }
    }
    log::warn!("recv error");
    disconnect(&client_manager, handle, addr, &conns, &tx).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use lan_mouse_proto::{AudioControl, AudioRole};
    use tokio::task::LocalSet;

    #[test]
    fn disconnected_audio_control_is_accepted_for_retry() {
        let client_manager = ClientManager::default();
        let handle = client_manager.add_client();
        let cert = Certificate::generate_self_signed(["audio-test".to_owned()]).unwrap();
        let reassembler = Arc::new(Mutex::new(lan_mouse_proto::ClipboardReassembler::default()));
        let connection = LanMouseConnection::new(cert, client_manager, reassembler);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let control = AudioControl {
            role: AudioRole::Receive,
            send_system_audio: false,
            send_microphone: false,
            receive_microphone: true,
        };
        let expected = control.clone();

        let result = runtime.block_on(LocalSet::new().run_until(async {
            connection
                .send_audio(AudioMessage::Control(control), handle)
                .await
        }));

        assert!(result.is_ok(), "disconnected control should be queued");
        assert_eq!(
            connection.pending_audio_controls.borrow().get(&handle),
            Some(&expected)
        );
    }
}

async fn disconnect(
    client_manager: &ClientManager,
    handle: ClientHandle,
    addr: SocketAddr,
    conns: &Mutex<HashMap<SocketAddr, ClientConnection>>,
    tx: &Sender<(ClientHandle, NetworkEvent)>,
) {
    log::warn!("client ({handle}) @ {addr} connection closed");
    let removed = conns.lock().await.remove(&addr).is_some();
    client_manager.set_active_addr(handle, None);
    client_manager.set_peer_commit(handle, None);
    if removed {
        tx.send((handle, NetworkEvent::Disconnected))
            .expect("channel closed");
    }
    let active: Vec<SocketAddr> = conns.lock().await.keys().copied().collect();
    log::info!("active connections: {active:?}");
}
