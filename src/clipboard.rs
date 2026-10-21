use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use futures::future::join_all;
use lan_mouse_proto::{
    ClipboardFrame, ClipboardReassembler, ClipboardReassemblyResult, ClipboardUpdateId,
    ProtocolError, chunk_files, chunk_image, chunk_text,
};
use tokio::{
    sync::{Mutex, Notify, oneshot},
    time::timeout,
};
use tokio_util::sync::CancellationToken;
use webrtc_util::Conn;

type ArcConn = Arc<dyn Conn + Send + Sync>;
pub(crate) type SharedClipboardReassembler = Arc<Mutex<ClipboardReassembler>>;

#[derive(Clone)]
pub(crate) enum ClipboardPayload {
    Text(String),
    ImagePng(Vec<u8>),
    Files(Vec<u8>),
}

impl std::fmt::Debug for ClipboardPayload {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Text(text) => formatter.debug_tuple("Text").field(&text.len()).finish(),
            Self::ImagePng(png) => formatter.debug_tuple("ImagePng").field(&png.len()).finish(),
            Self::Files(bundle) => formatter.debug_tuple("Files").field(&bundle.len()).finish(),
        }
    }
}

const ACK_TIMEOUT: Duration = Duration::from_millis(250);
const UPDATE_ACK_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_CHUNKS_IN_FLIGHT: usize = 8;
const MAX_RETRIES: usize = 3;
const CAPABILITY_TIMEOUT: Duration = Duration::from_secs(2);

struct PeerState {
    supports_clipboard: bool,
    supports_image: bool,
    supports_files: bool,
    chunk_acks: HashMap<(ClipboardUpdateId, u16), oneshot::Sender<()>>,
    update_acks: HashMap<ClipboardUpdateId, oneshot::Sender<bool>>,
    transfer_cancel: CancellationToken,
}

impl Default for PeerState {
    fn default() -> Self {
        Self {
            supports_clipboard: false,
            supports_image: false,
            supports_files: false,
            chunk_acks: HashMap::new(),
            update_acks: HashMap::new(),
            transfer_cancel: CancellationToken::new(),
        }
    }
}

/// State and reliable transfer helpers for one authenticated DTLS session.
pub(crate) struct ClipboardPeer {
    conn: ArcConn,
    reassembler: SharedClipboardReassembler,
    state: Mutex<PeerState>,
    ready: Notify,
    send_generation: AtomicU64,
}

impl ClipboardPeer {
    pub(crate) fn new(conn: ArcConn, reassembler: SharedClipboardReassembler) -> Self {
        Self {
            conn,
            reassembler,
            state: Mutex::new(PeerState::default()),
            ready: Notify::new(),
            send_generation: AtomicU64::new(0),
        }
    }

    pub(crate) async fn advertise(&self) {
        if let Err(error) = self.send_frame(ClipboardFrame::Capabilities).await {
            log::debug!("could not advertise clipboard support: {error}");
        }
        if let Err(error) = self.send_frame(ClipboardFrame::ImageCapabilities).await {
            log::debug!("could not advertise image clipboard support: {error}");
        }
        if let Err(error) = self.send_frame(ClipboardFrame::FileCapabilities).await {
            log::debug!("could not advertise file clipboard support: {error}");
        }
    }

    pub(crate) async fn send_text(&self, update_id: ClipboardUpdateId, text: &str) {
        let frames = match chunk_text(update_id, text) {
            Ok(frames) => frames,
            Err(error) => {
                log::debug!("clipboard text could not be chunked: {error}");
                return;
            }
        };
        self.send_frames(update_id, frames, false, false).await;
    }

    pub(crate) async fn send_image(&self, update_id: ClipboardUpdateId, png: &[u8]) {
        let frames = match chunk_image(update_id, png) {
            Ok(frames) => frames,
            Err(error) => {
                log::debug!("clipboard image could not be chunked: {error}");
                return;
            }
        };
        self.send_frames(update_id, frames, true, false).await;
    }

    pub(crate) async fn send_files(&self, update_id: ClipboardUpdateId, bundle: &[u8]) {
        let frames = match chunk_files(update_id, bundle) {
            Ok(frames) => frames,
            Err(error) => {
                log::debug!("clipboard files could not be chunked: {error}");
                return;
            }
        };
        self.send_frames(update_id, frames, false, true).await;
    }

    async fn send_frames(
        &self,
        update_id: ClipboardUpdateId,
        frames: Vec<ClipboardFrame>,
        image: bool,
        files: bool,
    ) {
        let (generation, cancellation) = {
            let mut state = self.state.lock().await;
            state.transfer_cancel.cancel();
            let cancellation = CancellationToken::new();
            state.transfer_cancel = cancellation.clone();
            let generation = self.send_generation.fetch_add(1, Ordering::SeqCst) + 1;
            (generation, cancellation)
        };
        if !self
            .wait_until_ready(generation, &cancellation, image, files)
            .await
            || !self.is_current(generation)
        {
            return;
        }

        for _ in 0..MAX_RETRIES {
            if !self.is_current(generation) {
                return;
            }
            let (ack_tx, ack_rx) = oneshot::channel();
            self.state
                .lock()
                .await
                .update_acks
                .insert(update_id, ack_tx);

            let mut transfer_ok = true;
            for group in frames.chunks(MAX_CHUNKS_IN_FLIGHT) {
                if !self.is_current(generation) {
                    transfer_ok = false;
                    break;
                }
                let results = join_all(group.iter().cloned().map(|frame| {
                    self.send_chunk_with_retry(generation, cancellation.clone(), frame)
                }))
                .await;
                if results.iter().any(|ok| !ok) {
                    transfer_ok = false;
                    break;
                }
            }

            if !transfer_ok {
                self.remove_update_ack(update_id).await;
                continue;
            }

            let ack_result = tokio::select! {
                _ = cancellation.cancelled() => {
                    self.remove_update_ack(update_id).await;
                    return;
                },
                result = timeout(UPDATE_ACK_TIMEOUT, ack_rx) => result,
            };
            match ack_result {
                Ok(Ok(true)) => return,
                Ok(Ok(false)) => log::debug!("peer rejected clipboard update"),
                Ok(Err(_)) | Err(_) => {
                    log::debug!("clipboard update acknowledgement timed out")
                }
            }
            self.remove_update_ack(update_id).await;
        }
    }

    pub(crate) async fn cancel_pending(&self) {
        self.send_generation.fetch_add(1, Ordering::SeqCst);
        self.state.lock().await.transfer_cancel.cancel();
    }

    pub(crate) async fn handle_frame(
        &self,
        data: &[u8],
    ) -> Result<Option<(ClipboardUpdateId, ClipboardPayload)>, ProtocolError> {
        self.reassembler.lock().await.expire();
        let frame = ClipboardFrame::decode(data)?;
        let image = matches!(frame, ClipboardFrame::ImageChunk { .. });
        let files = matches!(frame, ClipboardFrame::FileChunk { .. });
        match frame {
            ClipboardFrame::Capabilities => {
                self.mark_supported().await;
                self.send_frame(ClipboardFrame::CapabilitiesAck)
                    .await
                    .map_err(|_| ProtocolError::InvalidClipboardFrame)?;
            }
            ClipboardFrame::CapabilitiesAck => self.mark_supported().await,
            ClipboardFrame::ImageCapabilities => {
                self.mark_image_supported().await;
                self.send_frame(ClipboardFrame::ImageCapabilitiesAck)
                    .await
                    .map_err(|_| ProtocolError::InvalidClipboardFrame)?;
            }
            ClipboardFrame::ImageCapabilitiesAck => self.mark_image_supported().await,
            ClipboardFrame::FileCapabilities => {
                self.mark_files_supported().await;
                self.send_frame(ClipboardFrame::FileCapabilitiesAck)
                    .await
                    .map_err(|_| ProtocolError::InvalidClipboardFrame)?;
            }
            ClipboardFrame::FileCapabilitiesAck => self.mark_files_supported().await,
            ClipboardFrame::TextChunk {
                update_id,
                index,
                count,
                total_bytes,
                checksum,
                payload,
            }
            | ClipboardFrame::ImageChunk {
                update_id,
                index,
                count,
                total_bytes,
                checksum,
                payload,
            }
            | ClipboardFrame::FileChunk {
                update_id,
                index,
                count,
                total_bytes,
                checksum,
                payload,
            } => {
                if !self.supports_kind(image, files).await {
                    return Err(ProtocolError::InvalidClipboardFrame);
                }
                let result = if files {
                    self.reassembler.lock().await.push_files(
                        update_id,
                        index,
                        count,
                        total_bytes,
                        checksum,
                        payload,
                    )
                } else if image {
                    self.reassembler.lock().await.push_image(
                        update_id,
                        index,
                        count,
                        total_bytes,
                        checksum,
                        payload,
                    )
                } else {
                    self.reassembler.lock().await.push(
                        update_id,
                        index,
                        count,
                        total_bytes,
                        checksum,
                        payload,
                    )
                };
                match result {
                    Ok(ClipboardReassemblyResult::Pending)
                    | Ok(ClipboardReassemblyResult::DuplicateChunk) => {
                        self.send_frame(ClipboardFrame::TextChunkAck { update_id, index })
                            .await
                            .map_err(|_| ProtocolError::InvalidClipboardFrame)?;
                    }
                    Ok(ClipboardReassemblyResult::DuplicateUpdate)
                    | Ok(ClipboardReassemblyResult::StaleUpdate) => {
                        self.send_frame(ClipboardFrame::TextChunkAck { update_id, index })
                            .await
                            .map_err(|_| ProtocolError::InvalidClipboardFrame)?;
                        self.send_frame(ClipboardFrame::TextUpdateAck(update_id))
                            .await
                            .map_err(|_| ProtocolError::InvalidClipboardFrame)?;
                    }
                    Ok(ClipboardReassemblyResult::Complete(text)) => {
                        self.send_frame(ClipboardFrame::TextChunkAck { update_id, index })
                            .await
                            .map_err(|_| ProtocolError::InvalidClipboardFrame)?;
                        self.send_frame(ClipboardFrame::TextUpdateAck(update_id))
                            .await
                            .map_err(|_| ProtocolError::InvalidClipboardFrame)?;
                        return Ok(Some((update_id, ClipboardPayload::Text(text))));
                    }
                    Ok(ClipboardReassemblyResult::CompleteImage(png)) => {
                        self.send_frame(ClipboardFrame::TextChunkAck { update_id, index })
                            .await
                            .map_err(|_| ProtocolError::InvalidClipboardFrame)?;
                        self.send_frame(ClipboardFrame::TextUpdateAck(update_id))
                            .await
                            .map_err(|_| ProtocolError::InvalidClipboardFrame)?;
                        return Ok(Some((update_id, ClipboardPayload::ImagePng(png))));
                    }
                    Ok(ClipboardReassemblyResult::CompleteFiles(bundle)) => {
                        self.send_frame(ClipboardFrame::TextChunkAck { update_id, index })
                            .await
                            .map_err(|_| ProtocolError::InvalidClipboardFrame)?;
                        self.send_frame(ClipboardFrame::TextUpdateAck(update_id))
                            .await
                            .map_err(|_| ProtocolError::InvalidClipboardFrame)?;
                        return Ok(Some((update_id, ClipboardPayload::Files(bundle))));
                    }
                    Err(error) => {
                        let _ = self
                            .send_frame(ClipboardFrame::TextUpdateNack(update_id))
                            .await;
                        return Err(error);
                    }
                }
            }
            ClipboardFrame::TextChunkAck { update_id, index } => {
                if let Some(ack) = self
                    .state
                    .lock()
                    .await
                    .chunk_acks
                    .remove(&(update_id, index))
                {
                    let _ = ack.send(());
                }
            }
            ClipboardFrame::TextUpdateAck(update_id) => {
                if let Some(ack) = self.state.lock().await.update_acks.remove(&update_id) {
                    let _ = ack.send(true);
                }
            }
            ClipboardFrame::TextUpdateNack(update_id) => {
                if let Some(ack) = self.state.lock().await.update_acks.remove(&update_id) {
                    let _ = ack.send(false);
                }
            }
        }
        Ok(None)
    }

    fn is_current(&self, generation: u64) -> bool {
        self.send_generation.load(Ordering::SeqCst) == generation
    }

    async fn wait_until_ready(
        &self,
        generation: u64,
        cancellation: &CancellationToken,
        image: bool,
        files: bool,
    ) -> bool {
        let deadline = tokio::time::Instant::now() + CAPABILITY_TIMEOUT;
        loop {
            if !self.is_current(generation) || cancellation.is_cancelled() {
                return false;
            }
            if self.supports_kind(image, files).await {
                return true;
            }
            let notified = self.ready.notified();
            if !self.is_current(generation) || cancellation.is_cancelled() {
                return false;
            }
            if self.supports_kind(image, files).await {
                return true;
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return false;
            }
            tokio::select! {
                _ = cancellation.cancelled() => return false,
                result = timeout(remaining.min(Duration::from_millis(500)), notified) => {
                    if result.is_err() {
                        let capability = if files {
                            ClipboardFrame::FileCapabilities
                        } else if image {
                            ClipboardFrame::ImageCapabilities
                        } else {
                            ClipboardFrame::Capabilities
                        };
                        if let Err(error) = self.send_frame(capability).await {
                            log::debug!("clipboard capability retry failed: {error}");
                        }
                    }
                }
            }
        }
    }

    async fn supports_clipboard(&self) -> bool {
        self.state.lock().await.supports_clipboard
    }

    async fn supports_image(&self) -> bool {
        self.state.lock().await.supports_image
    }

    async fn supports_files(&self) -> bool {
        self.state.lock().await.supports_files
    }

    async fn supports_kind(&self, image: bool, files: bool) -> bool {
        if files {
            self.supports_files().await
        } else if image {
            self.supports_image().await
        } else {
            self.supports_clipboard().await
        }
    }

    async fn mark_supported(&self) {
        self.state.lock().await.supports_clipboard = true;
        self.ready.notify_waiters();
    }

    async fn mark_image_supported(&self) {
        self.state.lock().await.supports_image = true;
        self.ready.notify_waiters();
    }

    async fn mark_files_supported(&self) {
        self.state.lock().await.supports_files = true;
        self.ready.notify_waiters();
    }

    async fn send_chunk_with_retry(
        &self,
        generation: u64,
        cancellation: CancellationToken,
        frame: ClipboardFrame,
    ) -> bool {
        let (update_id, index) = match &frame {
            ClipboardFrame::TextChunk {
                update_id, index, ..
            }
            | ClipboardFrame::ImageChunk {
                update_id, index, ..
            }
            | ClipboardFrame::FileChunk {
                update_id, index, ..
            } => (update_id, index),
            _ => return false,
        };
        for _ in 0..MAX_RETRIES {
            if !self.is_current(generation) || cancellation.is_cancelled() {
                return false;
            }
            let (ack_tx, ack_rx) = oneshot::channel();
            self.state
                .lock()
                .await
                .chunk_acks
                .insert((*update_id, *index), ack_tx);
            let send_result = tokio::select! {
                _ = cancellation.cancelled() => {
                    self.remove_chunk_ack(*update_id, *index).await;
                    return false;
                },
                result = self.send_frame(frame.clone()) => result,
            };
            if let Err(error) = send_result {
                log::debug!("clipboard chunk send failed: {error}");
                self.remove_chunk_ack(*update_id, *index).await;
                continue;
            }
            let ack_result = tokio::select! {
                _ = cancellation.cancelled() => {
                    self.remove_chunk_ack(*update_id, *index).await;
                    return false;
                },
                result = timeout(ACK_TIMEOUT, ack_rx) => result,
            };
            if matches!(ack_result, Ok(Ok(()))) {
                return true;
            }
            self.remove_chunk_ack(*update_id, *index).await;
        }
        false
    }

    async fn remove_chunk_ack(&self, update_id: ClipboardUpdateId, index: u16) {
        self.state
            .lock()
            .await
            .chunk_acks
            .remove(&(update_id, index));
    }

    async fn remove_update_ack(&self, update_id: ClipboardUpdateId) {
        self.state.lock().await.update_acks.remove(&update_id);
    }

    async fn send_frame(&self, frame: ClipboardFrame) -> Result<(), webrtc_util::Error> {
        let bytes = frame
            .encode()
            .map_err(|error| webrtc_util::Error::Other(error.to_string()))?;
        let written = self.conn.send(&bytes).await?;
        if written != bytes.len() {
            return Err(webrtc_util::Error::Other(format!(
                "partial clipboard frame write: {written}/{} bytes",
                bytes.len()
            )));
        }
        Ok(())
    }
}
