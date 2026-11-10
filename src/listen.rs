use futures::{Stream, StreamExt};
use lan_mouse_proto::{
    AudioMessage, ClipboardFrame, ClipboardUpdateId, MAX_AUDIO_DATAGRAM_SIZE, MAX_EVENT_SIZE,
    ProtoEvent, decode_audio_message, is_audio_message,
};
use local_channel::mpsc::{Receiver, Sender, channel};
use rustls::pki_types::CertificateDer;
use std::{
    collections::{HashMap, VecDeque},
    net::SocketAddr,
    rc::Rc,
    sync::{Arc, Mutex, RwLock},
    time::Duration,
};
use thiserror::Error;
use tokio::{
    sync::Mutex as AsyncMutex,
    task::{JoinHandle, spawn_local},
};
use webrtc_dtls::{
    config::{ClientAuthType::RequireAnyClientCert, Config, ExtendedMasterSecretType},
    conn::DTLSConn,
    crypto::Certificate,
    listener::listen,
};
use webrtc_util::{Conn, Error, conn::Listener};

use crate::{
    clipboard::{ClipboardPayload, ClipboardPeer, SharedClipboardReassembler},
    crypto,
};

#[derive(Error, Debug)]
pub enum ListenerCreationError {
    #[error(transparent)]
    WebrtcUtil(#[from] webrtc_util::Error),
    #[error(transparent)]
    WebrtcDtls(#[from] webrtc_dtls::Error),
}

type ArcConn = Arc<dyn Conn + Send + Sync>;

pub(crate) enum ListenEvent {
    Msg {
        event: ProtoEvent,
        addr: SocketAddr,
    },
    Clipboard {
        update_id: ClipboardUpdateId,
        payload: ClipboardPayload,
        addr: SocketAddr,
    },
    Audio {
        addr: SocketAddr,
        fingerprint: String,
        message: AudioMessage,
    },
    Disconnected {
        addr: SocketAddr,
    },
    Accept {
        addr: SocketAddr,
        fingerprint: String,
    },
    Rejected {
        fingerprint: String,
    },
}

pub(crate) struct LanMouseListener {
    listen_rx: Receiver<ListenEvent>,
    listen_tx: Sender<ListenEvent>,
    audio_rx: tokio::sync::mpsc::Receiver<(SocketAddr, String, AudioMessage)>,
    prefer_audio: bool,
    listen_task: JoinHandle<()>,
    conns: Rc<AsyncMutex<Vec<IncomingConnection>>>,
    request_port_change: Sender<u16>,
    port_changed: Receiver<Result<u16, ListenerCreationError>>,
}

#[derive(Clone)]
struct IncomingConnection {
    addr: SocketAddr,
    conn: ArcConn,
    clipboard: Arc<ClipboardPeer>,
}

type VerifyPeerCertificateFn = Arc<
    dyn (Fn(&[Vec<u8>], &[CertificateDer<'static>]) -> Result<(), webrtc_dtls::Error>)
        + Send
        + Sync,
>;

impl LanMouseListener {
    pub(crate) async fn new(
        port: u16,
        cert: Certificate,
        authorized_keys: Arc<RwLock<HashMap<String, String>>>,
        reassembler: SharedClipboardReassembler,
    ) -> Result<Self, ListenerCreationError> {
        let (listen_tx, listen_rx) = channel();
        let (audio_tx, audio_rx) = tokio::sync::mpsc::channel(4);
        let listen_audio_tx = audio_tx.clone();
        let (request_port_change, mut request_port_change_rx) = channel();
        let (port_changed_tx, port_changed) = channel();
        let connection_attempts: Arc<Mutex<VecDeque<String>>> = Default::default();

        let authorized = authorized_keys.clone();
        let verify_peer_certificate: Option<VerifyPeerCertificateFn> = {
            let connection_attempts = connection_attempts.clone();
            Some(Arc::new(
                move |certs: &[Vec<u8>], _chains: &[CertificateDer<'static>]| {
                    assert!(certs.len() == 1);
                    let fingerprints = certs
                        .iter()
                        .map(|c| crypto::generate_fingerprint(c))
                        .collect::<Vec<_>>();
                    if authorized
                        .read()
                        .expect("lock")
                        .contains_key(&fingerprints[0])
                    {
                        Ok(())
                    } else {
                        let fingerprint = fingerprints.into_iter().next().expect("fingerprint");
                        connection_attempts
                            .lock()
                            .expect("lock")
                            .push_back(fingerprint);
                        Err(webrtc_dtls::Error::ErrVerifyDataMismatch)
                    }
                },
            ))
        };
        let cfg = Config {
            certificates: vec![cert.clone()],
            extended_master_secret: ExtendedMasterSecretType::Require,
            client_auth: RequireAnyClientCert,
            verify_peer_certificate,
            ..Default::default()
        };

        let listen_addr = SocketAddr::new("0.0.0.0".parse().expect("invalid ip"), port);
        let mut listener = listen(listen_addr, cfg.clone()).await?;

        let conns: Rc<AsyncMutex<Vec<IncomingConnection>>> = Rc::new(AsyncMutex::new(Vec::new()));

        let conns_clone = conns.clone();
        let listen_task: JoinHandle<()> = {
            let listen_tx = listen_tx.clone();
            let audio_tx = listen_audio_tx.clone();
            let connection_attempts = connection_attempts.clone();
            let reassembler = reassembler.clone();
            spawn_local(async move {
                loop {
                    let sleep = tokio::time::sleep(Duration::from_secs(2));
                    tokio::select! {
                        /* workaround for https://github.com/webrtc-rs/webrtc/issues/614 */
                        _ = sleep => continue,
                        c = listener.accept() => match c {
                            Ok((conn, addr)) => {
                                log::info!("dtls client connected, ip: {addr}");
                                let clipboard = Arc::new(ClipboardPeer::new(conn.clone(), addr, reassembler.clone()));
                                clipboard.advertise().await;
                                let dtls_conn: &DTLSConn = conn.as_any().downcast_ref().expect("dtls conn");
                                let certs = dtls_conn.connection_state().await.peer_certificates;
                                let cert = certs.first().expect("cert");
                                let fingerprint = crypto::generate_fingerprint(cert);
                                conns_clone.lock().await.push(IncomingConnection {
                                    addr,
                                    conn: conn.clone(),
                                    clipboard: clipboard.clone(),
                                });
                                listen_tx.send(ListenEvent::Accept { addr, fingerprint: fingerprint.clone() }).expect("channel closed");
                                spawn_local(read_loop(conns_clone.clone(), addr, fingerprint, conn, clipboard, listen_tx.clone(), audio_tx.clone()));
                            },
                            Err(e) => {
                                if let Error::Std(ref e) = e {
                                    if let Some(e) = e.0.downcast_ref::<webrtc_dtls::Error>() {
                                        match e {
                                            webrtc_dtls::Error::ErrVerifyDataMismatch => {
                                                if let Some(fingerprint) = connection_attempts.lock().expect("lock").pop_front() {
                                                    listen_tx.send(ListenEvent::Rejected { fingerprint }).expect("channel closed");
                                                }
                                            }
                                            _ => log::warn!("accept: {e}"),
                                        }
                                    } else {
                                        log::warn!("accept: {e:?}");
                                    }
                                } else {
                                    log::warn!("accept: {e:?}");
                                }
                            }
                        },
                        port = request_port_change_rx.recv() => {
                            let port = port.expect("channel closed");
                            let listen_addr = SocketAddr::new("0.0.0.0".parse().expect("invalid ip"), port);
                            match listen(listen_addr, cfg.clone()).await {
                                Ok(new_listener) => {
                                    let _ = listener.close().await;
                                    listener = new_listener;
                                    port_changed_tx.send(Ok(port)).expect("channel closed");
                                }
                                Err(e) => {
                                    log::warn!("unable to change port: {e}");
                                    port_changed_tx.send(Err(e.into())).expect("channel closed");
                                }
                            };
                        },
                    };
                }
            })
        };

        Ok(Self {
            conns,
            listen_rx,
            listen_tx,
            audio_rx,
            prefer_audio: true,
            listen_task,
            port_changed,
            request_port_change,
        })
    }

    pub(crate) fn request_port_change(&mut self, port: u16) {
        self.request_port_change.send(port).expect("channel closed");
    }

    pub(crate) async fn port_changed(&mut self) -> Result<u16, ListenerCreationError> {
        self.port_changed.recv().await.expect("channel closed")
    }

    pub(crate) async fn terminate(&mut self) {
        self.listen_task.abort();
        let conns = self.conns.lock().await;
        for connection in conns.iter() {
            let _ = connection.conn.close().await;
        }
        self.listen_tx.close();
    }

    pub(crate) async fn reply(&self, addr: SocketAddr, event: ProtoEvent) {
        log::trace!("reply {event} >=>=>=>=>=> {addr}");
        let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = event.into();
        let conns = self.conns.lock().await;
        for connection in conns.iter() {
            if connection.addr == addr {
                let _ = connection.conn.send(&buf[..len]).await;
            }
        }
    }

    pub(crate) async fn broadcast_clipboard(
        &self,
        update_id: ClipboardUpdateId,
        payload: ClipboardPayload,
    ) {
        let peers = self
            .conns
            .lock()
            .await
            .iter()
            .map(|connection| connection.clipboard.clone())
            .collect::<Vec<_>>();
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
            .iter()
            .map(|connection| connection.clipboard.clone())
            .collect::<Vec<_>>();
        for peer in peers {
            peer.cancel_pending().await;
        }
    }

    pub(crate) async fn get_certificate_fingerprint(&self, addr: SocketAddr) -> Option<String> {
        if let Some(conn) = self
            .conns
            .lock()
            .await
            .iter()
            .find(|connection| connection.addr == addr)
            .map(|connection| connection.conn.clone())
        {
            let conn: &DTLSConn = conn.as_any().downcast_ref().expect("dtls conn");
            let certs = conn.connection_state().await.peer_certificates;
            let cert = certs.first()?;
            let fingerprint = crypto::generate_fingerprint(cert);
            Some(fingerprint)
        } else {
            None
        }
    }
}

impl Stream for LanMouseListener {
    type Item = ListenEvent;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let this = self.as_mut().get_mut();
        let audio_first = this.prefer_audio;
        this.prefer_audio = !audio_first;
        let first = if audio_first {
            poll_audio(&mut this.audio_rx, cx)
        } else {
            this.listen_rx.poll_next_unpin(cx)
        };
        let first_closed = matches!(&first, std::task::Poll::Ready(None));
        if let std::task::Poll::Ready(Some(event)) = first {
            return std::task::Poll::Ready(Some(event));
        }

        let second = if audio_first {
            this.listen_rx.poll_next_unpin(cx)
        } else {
            poll_audio(&mut this.audio_rx, cx)
        };
        match second {
            std::task::Poll::Ready(Some(event)) => std::task::Poll::Ready(Some(event)),
            std::task::Poll::Ready(None) if first_closed => std::task::Poll::Ready(None),
            std::task::Poll::Ready(None) | std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

fn poll_audio(
    audio_rx: &mut tokio::sync::mpsc::Receiver<(SocketAddr, String, AudioMessage)>,
    cx: &mut std::task::Context<'_>,
) -> std::task::Poll<Option<ListenEvent>> {
    match audio_rx.poll_recv(cx) {
        std::task::Poll::Ready(Some((addr, fingerprint, message))) => {
            std::task::Poll::Ready(Some(ListenEvent::Audio {
                addr,
                fingerprint,
                message,
            }))
        }
        std::task::Poll::Ready(None) => std::task::Poll::Ready(None),
        std::task::Poll::Pending => std::task::Poll::Pending,
    }
}

async fn read_loop(
    conns: Rc<AsyncMutex<Vec<IncomingConnection>>>,
    addr: SocketAddr,
    fingerprint: String,
    conn: ArcConn,
    clipboard: Arc<ClipboardPeer>,
    dtls_tx: Sender<ListenEvent>,
    audio_tx: tokio::sync::mpsc::Sender<(SocketAddr, String, AudioMessage)>,
) -> Result<(), Error> {
    let mut b = [0u8; 2048];

    loop {
        let len = match conn.recv(&mut b).await {
            Ok(len) => len,
            Err(_) => break,
        };
        if ClipboardFrame::is_clipboard_frame(&b[..len]) {
            match clipboard.handle_frame(&b[..len]).await {
                Ok(Some((update_id, payload))) => dtls_tx
                    .send(ListenEvent::Clipboard {
                        update_id,
                        payload,
                        addr,
                    })
                    .expect("channel closed"),
                Ok(None) => {}
                Err(error) => log::debug!("ignoring invalid clipboard frame from {addr}: {error}"),
            }
            continue;
        }
        if is_audio_message(&b[..len]) {
            if len > MAX_AUDIO_DATAGRAM_SIZE {
                log::debug!("ignoring oversized audio frame from {addr}: {len} bytes");
                continue;
            }
            match decode_audio_message(&b[..len]) {
                Ok(message) => {
                    if let Err(error) = audio_tx.try_send((addr, fingerprint.clone(), message)) {
                        log::debug!("dropping audio frame from {addr}: {error}");
                    }
                }
                Err(error) => log::debug!("ignoring invalid audio frame from {addr}: {error}"),
            }
            continue;
        }
        let mut event_buf = [0u8; MAX_EVENT_SIZE];
        let event_len = len.min(MAX_EVENT_SIZE);
        event_buf[..event_len].copy_from_slice(&b[..event_len]);
        match event_buf.try_into() {
            Ok(event) => dtls_tx
                .send(ListenEvent::Msg { event, addr })
                .expect("channel closed"),
            Err(e) => {
                // Skip the malformed/unknown datagram and keep
                // listening. Each DTLS recv returns one full
                // datagram, so a parse error here can't desync a
                // stream; the next call gets a fresh, framed
                // message. This makes the protocol forward-
                // compatible: a peer running a newer Lan Mouse
                // version can introduce additional event types
                // and old peers will simply ignore them rather
                // than dropping the connection.
                log::debug!("ignoring undecodable event from {addr}: {e}");
            }
        }
    }
    log::info!("dtls client disconnected {addr:?}");
    let mut conns = conns.lock().await;
    let index = conns.iter().position(|connection| connection.addr == addr);
    let Some(index) = index else {
        return Ok(());
    };
    conns.remove(index);
    drop(conns);
    dtls_tx
        .send(ListenEvent::Disconnected { addr })
        .expect("channel closed");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::task::noop_waker;
    use lan_mouse_proto::{AudioControl, AudioRole};
    use std::task::{Context, Poll};

    #[tokio::test]
    async fn listener_delivers_audio_and_input_when_both_are_ready() {
        let (listen_tx, listen_rx) = channel();
        let (audio_tx, audio_rx) = tokio::sync::mpsc::channel(4);
        let (request_port_change, _request_rx) = channel();
        let (_port_changed_tx, port_changed) = channel();
        let addr = "127.0.0.1:4242".parse().unwrap();
        let message = AudioMessage::Control(AudioControl {
            role: AudioRole::Off,
            send_system_audio: false,
            send_microphone: false,
            receive_microphone: false,
        });

        assert!(
            listen_tx
                .send(ListenEvent::Msg {
                    event: ProtoEvent::Ping,
                    addr,
                })
                .is_ok()
        );
        assert!(
            audio_tx
                .try_send((addr, "peer-fingerprint".into(), message))
                .is_ok()
        );

        let listener = LanMouseListener {
            listen_rx,
            listen_tx,
            audio_rx,
            prefer_audio: true,
            listen_task: tokio::spawn(async {}),
            conns: Rc::new(AsyncMutex::new(Vec::new())),
            request_port_change,
            port_changed,
        };
        let mut listener = Box::pin(listener);
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        let first = listener.as_mut().poll_next(&mut cx);
        let second = listener.as_mut().poll_next(&mut cx);

        let mut received_audio = false;
        let mut received_input = false;
        for event in [first, second] {
            match event {
                Poll::Ready(Some(ListenEvent::Audio { .. })) => received_audio = true,
                Poll::Ready(Some(ListenEvent::Msg {
                    event: ProtoEvent::Ping,
                    ..
                })) => received_input = true,
                _ => {}
            }
        }

        assert!(received_audio, "the queued audio event must be delivered");
        assert!(
            received_input,
            "the queued input event must not be discarded"
        );
    }
}
