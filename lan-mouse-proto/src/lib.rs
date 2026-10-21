use input_event::{Event as InputEvent, KeyboardEvent, PointerEvent};
use num_enum::{IntoPrimitive, TryFromPrimitive, TryFromPrimitiveError};
use paste::paste;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fmt::{Debug, Display, Formatter},
    mem::size_of,
    time::{Duration, Instant},
};
use thiserror::Error;

/// defines the maximum size an encoded event can take up
/// this is currently the pointer motion event
/// type: u8, time: u32, dx: f64, dy: f64
pub const MAX_EVENT_SIZE: usize = size_of::<u8>() + size_of::<u32>() + 2 * size_of::<f64>();

/// Maximum size of a clipboard frame carried as one DTLS application datagram.
pub const MAX_CLIPBOARD_FRAME_SIZE: usize = 1200;
/// Maximum UTF-8 text size accepted in one clipboard update.
pub const MAX_CLIPBOARD_TEXT_BYTES: usize = 256 * 1024;
/// Maximum encoded PNG size accepted in one clipboard update.
pub const MAX_CLIPBOARD_IMAGE_BYTES: usize = 8 * 1024 * 1024;
/// Maximum encoded [`FileOfferAnnouncement`]. File contents are fetched
/// separately when pasted, so only the offer itself crosses this channel.
pub const MAX_CLIPBOARD_FILE_OFFER_BYTES: usize = 64 * 1024;
/// Maximum text bytes buffered across incomplete clipboard updates.
pub const MAX_CLIPBOARD_REASSEMBLY_BYTES: usize = 64 * 1024 * 1024;
/// Maximum number of incomplete clipboard updates buffered at once.
pub const MAX_CLIPBOARD_REASSEMBLIES: usize = 64;

const CLIPBOARD_FRAME_MAGIC: u8 = 0xff;
const CLIPBOARD_FRAME_VERSION: u8 = 1;
const CLIPBOARD_FRAME_PREFIX_SIZE: usize = 3;
const TEXT_CHUNK_HEADER_SIZE: usize = CLIPBOARD_FRAME_PREFIX_SIZE + 16 + 2 + 2 + 4 + 32;
const TEXT_CHUNK_CAPACITY: usize = MAX_CLIPBOARD_FRAME_SIZE - TEXT_CHUNK_HEADER_SIZE;
const REASSEMBLY_TIMEOUT: Duration = Duration::from_secs(30);
const COMPLETED_ID_TIMEOUT: Duration = Duration::from_secs(600);
const MAX_COMPLETED_IDS: usize = 128;
const MAX_CLIPBOARD_ORIGINS: usize = 128;

/// A sender-generated id: 8 random origin bytes followed by a monotonically
/// increasing 64-bit sequence number, both in network byte order.
pub type ClipboardUpdateId = [u8; 16];

/// error type for protocol violations
#[derive(Debug, Error)]
pub enum ProtocolError {
    /// event type does not exist
    #[error("invalid event id: `{0}`")]
    InvalidEventId(#[from] TryFromPrimitiveError<EventType>),
    /// position type does not exist
    #[error("invalid event id: `{0}`")]
    InvalidPosition(#[from] TryFromPrimitiveError<Position>),
    /// Clipboard frame is malformed or has an unsupported version.
    #[error("invalid clipboard frame")]
    InvalidClipboardFrame,
    /// Clipboard text exceeds the protocol limit.
    #[error("clipboard text exceeds the {MAX_CLIPBOARD_TEXT_BYTES}-byte limit")]
    ClipboardTextTooLarge,
    /// Clipboard PNG exceeds the protocol limit.
    #[error("clipboard image exceeds the {MAX_CLIPBOARD_IMAGE_BYTES}-byte limit")]
    ClipboardImageTooLarge,
    /// Clipboard file offer is malformed.
    #[error("invalid clipboard file offer")]
    InvalidClipboardFileOffer,
    /// Clipboard receiver exceeded its bounded reassembly budget.
    #[error("clipboard reassembly budget exceeded")]
    ClipboardReassemblyLimit,
    /// Reassembled clipboard data is not UTF-8 text.
    #[error("clipboard text is not valid UTF-8")]
    ClipboardInvalidUtf8,
    /// Reassembled text did not match the announced SHA-256 digest.
    #[error("clipboard text checksum mismatch")]
    ClipboardChecksumMismatch,
}

/// Application frames for clipboard synchronization. These are deliberately
/// distinct from [`ProtoEvent`], whose encoded size is fixed for input events.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClipboardFrame {
    Capabilities,
    CapabilitiesAck,
    ImageCapabilities,
    ImageCapabilitiesAck,
    FileOfferCapabilities,
    FileOfferCapabilitiesAck,
    TextChunk {
        update_id: ClipboardUpdateId,
        index: u16,
        count: u16,
        total_bytes: u32,
        checksum: [u8; 32],
        payload: Vec<u8>,
    },
    ImageChunk {
        update_id: ClipboardUpdateId,
        index: u16,
        count: u16,
        total_bytes: u32,
        checksum: [u8; 32],
        payload: Vec<u8>,
    },
    FileOfferChunk {
        update_id: ClipboardUpdateId,
        index: u16,
        count: u16,
        total_bytes: u32,
        checksum: [u8; 32],
        payload: Vec<u8>,
    },
    TextChunkAck {
        update_id: ClipboardUpdateId,
        index: u16,
    },
    TextUpdateAck(ClipboardUpdateId),
    TextUpdateNack(ClipboardUpdateId),
}

impl ClipboardFrame {
    pub fn is_clipboard_frame(data: &[u8]) -> bool {
        data.first() == Some(&CLIPBOARD_FRAME_MAGIC)
    }

    pub fn encode(&self) -> Result<Vec<u8>, ProtocolError> {
        let mut buf = vec![CLIPBOARD_FRAME_MAGIC, CLIPBOARD_FRAME_VERSION];
        match self {
            Self::Capabilities => buf.push(1),
            Self::CapabilitiesAck => buf.push(2),
            Self::ImageCapabilities => buf.push(8),
            Self::ImageCapabilitiesAck => buf.push(9),
            // 10-12 were the in-memory file bundle frames of an earlier build
            Self::FileOfferCapabilities => buf.push(13),
            Self::FileOfferCapabilitiesAck => buf.push(14),
            Self::TextChunk {
                update_id,
                index,
                count,
                total_bytes,
                checksum,
                payload,
            }
            | Self::ImageChunk {
                update_id,
                index,
                count,
                total_bytes,
                checksum,
                payload,
            }
            | Self::FileOfferChunk {
                update_id,
                index,
                count,
                total_bytes,
                checksum,
                payload,
            } => {
                let is_image = matches!(self, Self::ImageChunk { .. });
                let is_file = matches!(self, Self::FileOfferChunk { .. });
                let max_bytes = if is_file {
                    MAX_CLIPBOARD_FILE_OFFER_BYTES
                } else if is_image {
                    MAX_CLIPBOARD_IMAGE_BYTES
                } else {
                    MAX_CLIPBOARD_TEXT_BYTES
                };
                if *count == 0
                    || *index >= *count
                    || *total_bytes as usize > max_bytes
                    || payload.len() > TEXT_CHUNK_CAPACITY
                {
                    return Err(ProtocolError::InvalidClipboardFrame);
                }
                let expected_count = chunk_count(*total_bytes as usize);
                let expected_payload_len = if *index + 1 < *count {
                    TEXT_CHUNK_CAPACITY
                } else {
                    (*total_bytes as usize)
                        .saturating_sub(TEXT_CHUNK_CAPACITY * (*count as usize - 1))
                };
                if *count as usize != expected_count || payload.len() != expected_payload_len {
                    return Err(ProtocolError::InvalidClipboardFrame);
                }
                buf.push(if is_file {
                    15
                } else if is_image {
                    7
                } else {
                    3
                });
                buf.extend_from_slice(update_id);
                buf.extend_from_slice(&index.to_be_bytes());
                buf.extend_from_slice(&count.to_be_bytes());
                buf.extend_from_slice(&total_bytes.to_be_bytes());
                buf.extend_from_slice(checksum);
                buf.extend_from_slice(payload);
            }
            Self::TextChunkAck { update_id, index } => {
                buf.push(4);
                buf.extend_from_slice(update_id);
                buf.extend_from_slice(&index.to_be_bytes());
            }
            Self::TextUpdateAck(update_id) => {
                buf.push(5);
                buf.extend_from_slice(update_id);
            }
            Self::TextUpdateNack(update_id) => {
                buf.push(6);
                buf.extend_from_slice(update_id);
            }
        }
        if buf.len() > MAX_CLIPBOARD_FRAME_SIZE {
            return Err(ProtocolError::InvalidClipboardFrame);
        }
        Ok(buf)
    }

    pub fn decode(data: &[u8]) -> Result<Self, ProtocolError> {
        if data.len() < CLIPBOARD_FRAME_PREFIX_SIZE
            || data.len() > MAX_CLIPBOARD_FRAME_SIZE
            || data[0] != CLIPBOARD_FRAME_MAGIC
            || data[1] != CLIPBOARD_FRAME_VERSION
        {
            return Err(ProtocolError::InvalidClipboardFrame);
        }
        let kind = data[2];
        let payload = &data[CLIPBOARD_FRAME_PREFIX_SIZE..];
        match kind {
            1 if payload.is_empty() => Ok(Self::Capabilities),
            2 if payload.is_empty() => Ok(Self::CapabilitiesAck),
            3 | 7 | 15 if payload.len() >= 56 => {
                let update_id = payload[..16]
                    .try_into()
                    .map_err(|_| ProtocolError::InvalidClipboardFrame)?;
                let index = u16::from_be_bytes(
                    payload[16..18]
                        .try_into()
                        .map_err(|_| ProtocolError::InvalidClipboardFrame)?,
                );
                let count = u16::from_be_bytes(
                    payload[18..20]
                        .try_into()
                        .map_err(|_| ProtocolError::InvalidClipboardFrame)?,
                );
                let total_bytes = u32::from_be_bytes(
                    payload[20..24]
                        .try_into()
                        .map_err(|_| ProtocolError::InvalidClipboardFrame)?,
                );
                let checksum = payload[24..56]
                    .try_into()
                    .map_err(|_| ProtocolError::InvalidClipboardFrame)?;
                let payload = payload[56..].to_vec();
                let frame = if kind == 15 {
                    Self::FileOfferChunk {
                        update_id,
                        index,
                        count,
                        total_bytes,
                        checksum,
                        payload,
                    }
                } else if kind == 7 {
                    Self::ImageChunk {
                        update_id,
                        index,
                        count,
                        total_bytes,
                        checksum,
                        payload,
                    }
                } else {
                    Self::TextChunk {
                        update_id,
                        index,
                        count,
                        total_bytes,
                        checksum,
                        payload,
                    }
                };
                // Reuse the encoder's shape validation after decoding fields.
                frame.encode()?;
                Ok(frame)
            }
            4 if payload.len() == 18 => Ok(Self::TextChunkAck {
                update_id: payload[..16]
                    .try_into()
                    .map_err(|_| ProtocolError::InvalidClipboardFrame)?,
                index: u16::from_be_bytes(
                    payload[16..18]
                        .try_into()
                        .map_err(|_| ProtocolError::InvalidClipboardFrame)?,
                ),
            }),
            5 if payload.len() == 16 => Ok(Self::TextUpdateAck(
                payload
                    .try_into()
                    .map_err(|_| ProtocolError::InvalidClipboardFrame)?,
            )),
            6 if payload.len() == 16 => Ok(Self::TextUpdateNack(
                payload
                    .try_into()
                    .map_err(|_| ProtocolError::InvalidClipboardFrame)?,
            )),
            8 if payload.is_empty() => Ok(Self::ImageCapabilities),
            9 if payload.is_empty() => Ok(Self::ImageCapabilitiesAck),
            13 if payload.is_empty() => Ok(Self::FileOfferCapabilities),
            14 if payload.is_empty() => Ok(Self::FileOfferCapabilitiesAck),
            _ => Err(ProtocolError::InvalidClipboardFrame),
        }
    }
}

pub fn chunk_text(
    update_id: ClipboardUpdateId,
    text: &str,
) -> Result<Vec<ClipboardFrame>, ProtocolError> {
    let bytes = text.as_bytes();
    if bytes.len() > MAX_CLIPBOARD_TEXT_BYTES {
        return Err(ProtocolError::ClipboardTextTooLarge);
    }
    let count = chunk_count(bytes.len());
    let count = u16::try_from(count).map_err(|_| ProtocolError::ClipboardTextTooLarge)?;
    let checksum: [u8; 32] = Sha256::digest(bytes).into();
    if bytes.is_empty() {
        return Ok(vec![ClipboardFrame::TextChunk {
            update_id,
            index: 0,
            count: 1,
            total_bytes: 0,
            checksum,
            payload: Vec::new(),
        }]);
    }

    let mut frames = Vec::with_capacity(count as usize);
    for (index, payload) in bytes.chunks(TEXT_CHUNK_CAPACITY).enumerate() {
        frames.push(ClipboardFrame::TextChunk {
            update_id,
            index: index as u16,
            count,
            total_bytes: bytes.len() as u32,
            checksum,
            payload: payload.to_vec(),
        });
    }
    Ok(frames)
}

pub fn chunk_image(
    update_id: ClipboardUpdateId,
    png: &[u8],
) -> Result<Vec<ClipboardFrame>, ProtocolError> {
    if png.len() > MAX_CLIPBOARD_IMAGE_BYTES {
        return Err(ProtocolError::ClipboardImageTooLarge);
    }
    let count =
        u16::try_from(chunk_count(png.len())).map_err(|_| ProtocolError::ClipboardImageTooLarge)?;
    let checksum: [u8; 32] = Sha256::digest(png).into();
    let chunks = if png.is_empty() {
        vec![&[][..]]
    } else {
        png.chunks(TEXT_CHUNK_CAPACITY).collect::<Vec<_>>()
    };
    Ok(chunks
        .into_iter()
        .enumerate()
        .map(|(index, payload)| ClipboardFrame::ImageChunk {
            update_id,
            index: index as u16,
            count,
            total_bytes: png.len() as u32,
            checksum,
            payload: payload.to_vec(),
        })
        .collect())
}

pub fn chunk_file_offer(
    update_id: ClipboardUpdateId,
    announcement: &[u8],
) -> Result<Vec<ClipboardFrame>, ProtocolError> {
    if announcement.is_empty() || announcement.len() > MAX_CLIPBOARD_FILE_OFFER_BYTES {
        return Err(ProtocolError::InvalidClipboardFileOffer);
    }
    let count = u16::try_from(chunk_count(announcement.len()))
        .map_err(|_| ProtocolError::InvalidClipboardFileOffer)?;
    let checksum: [u8; 32] = Sha256::digest(announcement).into();
    Ok(announcement
        .chunks(TEXT_CHUNK_CAPACITY)
        .enumerate()
        .map(|(index, payload)| ClipboardFrame::FileOfferChunk {
            update_id,
            index: index as u16,
            count,
            total_bytes: announcement.len() as u32,
            checksum,
            payload: payload.to_vec(),
        })
        .collect())
}

const FILE_OFFER_MAGIC: &[u8; 4] = b"LMO1";

/// Announces files or folders copied on the sending peer. The receiver
/// fetches them from `port` over TLS on TCP, presenting `token`, only once
/// the user pastes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileOfferAnnouncement {
    pub token: [u8; 32],
    pub port: u16,
    /// Number of copied items.
    pub item_count: u32,
    /// Names of the copied items for display; may be cut short to fit.
    pub names: Vec<String>,
}

impl FileOfferAnnouncement {
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(FILE_OFFER_MAGIC);
        buf.extend_from_slice(&self.token);
        buf.extend_from_slice(&self.port.to_be_bytes());
        buf.extend_from_slice(&self.item_count.to_be_bytes());
        let count_offset = buf.len();
        buf.extend_from_slice(&0u16.to_be_bytes());
        let mut count = 0u16;
        for name in &self.names {
            let name = name.as_bytes();
            if name.len() > u16::MAX as usize
                || buf.len() + 2 + name.len() > MAX_CLIPBOARD_FILE_OFFER_BYTES
                || count == u16::MAX
            {
                break;
            }
            buf.extend_from_slice(&(name.len() as u16).to_be_bytes());
            buf.extend_from_slice(name);
            count += 1;
        }
        buf[count_offset..count_offset + 2].copy_from_slice(&count.to_be_bytes());
        buf
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtocolError> {
        let invalid = || ProtocolError::InvalidClipboardFileOffer;
        if bytes.len() > MAX_CLIPBOARD_FILE_OFFER_BYTES || !bytes.starts_with(FILE_OFFER_MAGIC) {
            return Err(invalid());
        }
        let mut rest = &bytes[FILE_OFFER_MAGIC.len()..];
        let mut take = |len: usize| -> Result<&[u8], ProtocolError> {
            if rest.len() < len {
                return Err(invalid());
            }
            let (head, tail) = rest.split_at(len);
            rest = tail;
            Ok(head)
        };
        let token = take(32)?.try_into().map_err(|_| invalid())?;
        let port = u16::from_be_bytes(take(2)?.try_into().map_err(|_| invalid())?);
        let item_count = u32::from_be_bytes(take(4)?.try_into().map_err(|_| invalid())?);
        let count = u16::from_be_bytes(take(2)?.try_into().map_err(|_| invalid())?);
        let mut names = Vec::with_capacity(count as usize);
        for _ in 0..count {
            let len = u16::from_be_bytes(take(2)?.try_into().map_err(|_| invalid())?) as usize;
            let name = std::str::from_utf8(take(len)?).map_err(|_| invalid())?;
            names.push(name.to_owned());
        }
        if !rest.is_empty() || port == 0 || item_count == 0 {
            return Err(invalid());
        }
        Ok(Self {
            token,
            port,
            item_count,
            names,
        })
    }
}

/// Bounded receiver-side assembly for text chunks from all peer sessions.
#[derive(Default)]
pub struct ClipboardReassembler {
    partials: HashMap<ClipboardUpdateId, PartialClipboardData>,
    completed: HashMap<ClipboardUpdateId, Instant>,
    latest_sequence: HashMap<[u8; 8], (u64, Instant)>,
    buffered_bytes: usize,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ClipboardDataKind {
    Text,
    ImagePng,
    FileOffer,
}

struct PartialClipboardData {
    kind: ClipboardDataKind,
    count: u16,
    total_bytes: usize,
    checksum: [u8; 32],
    chunks: Vec<Option<Vec<u8>>>,
    received_chunks: usize,
    received_bytes: usize,
    updated: Instant,
}

#[derive(Debug, Eq, PartialEq)]
pub enum ClipboardReassemblyResult {
    Pending,
    DuplicateChunk,
    DuplicateUpdate,
    StaleUpdate,
    Complete(String),
    CompleteImage(Vec<u8>),
    CompleteFileOffer(Vec<u8>),
}

impl ClipboardReassembler {
    pub fn push(
        &mut self,
        update_id: ClipboardUpdateId,
        index: u16,
        count: u16,
        total_bytes: u32,
        checksum: [u8; 32],
        payload: Vec<u8>,
    ) -> Result<ClipboardReassemblyResult, ProtocolError> {
        self.push_kind(
            ClipboardDataKind::Text,
            update_id,
            index,
            count,
            total_bytes,
            checksum,
            payload,
        )
    }

    pub fn push_image(
        &mut self,
        update_id: ClipboardUpdateId,
        index: u16,
        count: u16,
        total_bytes: u32,
        checksum: [u8; 32],
        payload: Vec<u8>,
    ) -> Result<ClipboardReassemblyResult, ProtocolError> {
        self.push_kind(
            ClipboardDataKind::ImagePng,
            update_id,
            index,
            count,
            total_bytes,
            checksum,
            payload,
        )
    }

    pub fn push_file_offer(
        &mut self,
        update_id: ClipboardUpdateId,
        index: u16,
        count: u16,
        total_bytes: u32,
        checksum: [u8; 32],
        payload: Vec<u8>,
    ) -> Result<ClipboardReassemblyResult, ProtocolError> {
        self.push_kind(
            ClipboardDataKind::FileOffer,
            update_id,
            index,
            count,
            total_bytes,
            checksum,
            payload,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn push_kind(
        &mut self,
        kind: ClipboardDataKind,
        update_id: ClipboardUpdateId,
        index: u16,
        count: u16,
        total_bytes: u32,
        checksum: [u8; 32],
        payload: Vec<u8>,
    ) -> Result<ClipboardReassemblyResult, ProtocolError> {
        self.expire();
        if self.completed.contains_key(&update_id) {
            return Ok(ClipboardReassemblyResult::DuplicateUpdate);
        }
        let total_bytes = total_bytes as usize;
        let max_bytes = match kind {
            ClipboardDataKind::Text => MAX_CLIPBOARD_TEXT_BYTES,
            ClipboardDataKind::ImagePng => MAX_CLIPBOARD_IMAGE_BYTES,
            ClipboardDataKind::FileOffer => MAX_CLIPBOARD_FILE_OFFER_BYTES,
        };
        if count == 0
            || index >= count
            || total_bytes > max_bytes
            || count as usize != chunk_count(total_bytes)
            || payload.len() > TEXT_CHUNK_CAPACITY
        {
            return Err(ProtocolError::InvalidClipboardFrame);
        }

        let expected_payload_len = if index + 1 < count {
            TEXT_CHUNK_CAPACITY
        } else {
            total_bytes.saturating_sub(TEXT_CHUNK_CAPACITY * (count as usize - 1))
        };
        if payload.len() != expected_payload_len {
            self.remove_partial(&update_id);
            return Err(ProtocolError::InvalidClipboardFrame);
        }

        let origin: [u8; 8] = update_id[..8]
            .try_into()
            .map_err(|_| ProtocolError::InvalidClipboardFrame)?;
        let sequence = u64::from_be_bytes(
            update_id[8..]
                .try_into()
                .map_err(|_| ProtocolError::InvalidClipboardFrame)?,
        );
        if let Some((latest, _)) = self.latest_sequence.get(&origin) {
            if sequence < *latest {
                return Ok(ClipboardReassemblyResult::StaleUpdate);
            }
        }
        if self
            .latest_sequence
            .get(&origin)
            .is_none_or(|(latest, _)| sequence > *latest)
        {
            let superseded = self
                .partials
                .keys()
                .filter(|id| {
                    id[..8] == origin
                        && u64::from_be_bytes(id[8..].try_into().expect("fixed update id"))
                            < sequence
                })
                .copied()
                .collect::<Vec<_>>();
            for old_id in superseded {
                self.remove_partial(&old_id);
            }
            if !self.latest_sequence.contains_key(&origin)
                && self.latest_sequence.len() >= MAX_CLIPBOARD_ORIGINS
            {
                if let Some(oldest) = self
                    .latest_sequence
                    .iter()
                    .min_by_key(|(_, (_, updated))| *updated)
                    .map(|(origin, _)| *origin)
                {
                    self.latest_sequence.remove(&oldest);
                }
            }
            self.latest_sequence
                .insert(origin, (sequence, Instant::now()));
        } else if let Some((_, updated)) = self.latest_sequence.get_mut(&origin) {
            *updated = Instant::now();
        }

        if !self.partials.contains_key(&update_id) {
            if self.partials.len() >= MAX_CLIPBOARD_REASSEMBLIES {
                return Err(ProtocolError::ClipboardReassemblyLimit);
            }
            self.partials.insert(
                update_id,
                PartialClipboardData {
                    kind,
                    count,
                    total_bytes,
                    checksum,
                    chunks: vec![None; count as usize],
                    received_chunks: 0,
                    received_bytes: 0,
                    updated: Instant::now(),
                },
            );
        }

        let metadata_matches = self.partials.get(&update_id).is_some_and(|partial| {
            partial.kind == kind
                && partial.count == count
                && partial.total_bytes == total_bytes
                && partial.checksum == checksum
        });
        if !metadata_matches {
            self.remove_partial(&update_id);
            return Err(ProtocolError::InvalidClipboardFrame);
        }
        if let Some(existing) = self
            .partials
            .get(&update_id)
            .and_then(|partial| partial.chunks[index as usize].as_ref())
        {
            if existing == &payload {
                self.partials
                    .get_mut(&update_id)
                    .expect("partial exists")
                    .updated = Instant::now();
                return Ok(ClipboardReassemblyResult::DuplicateChunk);
            }
            self.remove_partial(&update_id);
            return Err(ProtocolError::InvalidClipboardFrame);
        }
        if self.buffered_bytes.saturating_add(payload.len()) > MAX_CLIPBOARD_REASSEMBLY_BYTES {
            self.remove_partial(&update_id);
            return Err(ProtocolError::ClipboardReassemblyLimit);
        }

        let payload_len = payload.len();
        let complete = {
            let partial = self.partials.get_mut(&update_id).expect("partial exists");
            partial.received_bytes += payload_len;
            partial.received_chunks += 1;
            partial.updated = Instant::now();
            partial.chunks[index as usize] = Some(payload);
            partial.received_chunks == partial.count as usize
        };
        self.buffered_bytes = self.buffered_bytes.saturating_add(payload_len);

        if !complete {
            return Ok(ClipboardReassemblyResult::Pending);
        }

        let partial = self
            .partials
            .remove(&update_id)
            .expect("complete partial exists");
        self.buffered_bytes = self.buffered_bytes.saturating_sub(partial.received_bytes);
        let mut bytes = Vec::with_capacity(partial.total_bytes);
        for chunk in &partial.chunks {
            bytes.extend_from_slice(
                chunk
                    .as_deref()
                    .ok_or(ProtocolError::InvalidClipboardFrame)?,
            );
        }
        let digest: [u8; 32] = Sha256::digest(&bytes).into();
        if bytes.len() != partial.total_bytes || digest != checksum {
            return Err(ProtocolError::ClipboardChecksumMismatch);
        }
        let result = match kind {
            ClipboardDataKind::Text => ClipboardReassemblyResult::Complete(
                String::from_utf8(bytes).map_err(|_| ProtocolError::ClipboardInvalidUtf8)?,
            ),
            ClipboardDataKind::ImagePng => {
                if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
                    return Err(ProtocolError::InvalidClipboardFrame);
                }
                ClipboardReassemblyResult::CompleteImage(bytes)
            }
            ClipboardDataKind::FileOffer => {
                FileOfferAnnouncement::decode(&bytes)?;
                ClipboardReassemblyResult::CompleteFileOffer(bytes)
            }
        };
        self.completed.insert(update_id, Instant::now());
        self.trim_completed();
        Ok(result)
    }

    pub fn expire(&mut self) {
        let expired = self
            .partials
            .iter()
            .filter(|(_, partial)| partial.updated.elapsed() >= REASSEMBLY_TIMEOUT)
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        for update_id in expired {
            self.remove_partial(&update_id);
        }
        self.completed
            .retain(|_, inserted| inserted.elapsed() < COMPLETED_ID_TIMEOUT);
        self.latest_sequence
            .retain(|_, (_, updated)| updated.elapsed() < COMPLETED_ID_TIMEOUT);
    }

    fn remove_partial(&mut self, update_id: &ClipboardUpdateId) {
        if let Some(partial) = self.partials.remove(update_id) {
            self.buffered_bytes = self.buffered_bytes.saturating_sub(partial.received_bytes);
        }
    }

    fn trim_completed(&mut self) {
        while self.completed.len() > MAX_COMPLETED_IDS {
            if let Some(oldest) = self
                .completed
                .iter()
                .min_by_key(|(_, inserted)| **inserted)
                .map(|(id, _)| *id)
            {
                self.completed.remove(&oldest);
            }
        }
    }
}

fn chunk_count(bytes: usize) -> usize {
    if bytes == 0 {
        1
    } else {
        bytes.div_ceil(TEXT_CHUNK_CAPACITY)
    }
}

/// Position of a client
#[derive(Clone, Copy, Debug, TryFromPrimitive, IntoPrimitive)]
#[repr(u8)]
pub enum Position {
    Left,
    Right,
    Top,
    Bottom,
}

impl Display for Position {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let pos = match self {
            Position::Left => "left",
            Position::Right => "right",
            Position::Top => "top",
            Position::Bottom => "bottom",
        };
        write!(f, "{pos}")
    }
}

/// main lan-mouse protocol event type
#[derive(Clone, Copy, Debug)]
pub enum ProtoEvent {
    /// notify a client that the cursor entered its region at the given position
    /// [`ProtoEvent::Ack`] with the same serial is used for synchronization between devices
    Enter(Position),
    /// notify a client that the cursor left its region
    /// [`ProtoEvent::Ack`] with the same serial is used for synchronization between devices
    Leave(u32),
    /// acknowledge of an [`ProtoEvent::Enter`] or [`ProtoEvent::Leave`] event
    Ack(u32),
    /// Input event
    Input(InputEvent),
    /// Ping event for tracking unresponsive clients.
    /// A client has to respond with [`ProtoEvent::Pong`].
    Ping,
    /// Response to [`ProtoEvent::Ping`], true if emulation is enabled / available
    Pong(bool),
    /// Build identification for the sending peer. Sent by the
    /// connect side once after the connection authenticates, and
    /// echoed back by the listen side in reply, so each end can
    /// display the peer's build hash and warn (soft) on mismatch.
    /// `commit` is the 8-byte ASCII short commit hash from
    /// `shadow_rs`'s `SHORT_COMMIT`. Old peers that don't
    /// recognize the event type silently skip it per the
    /// forward-compat handling in the receive loop.
    Hello { commit: [u8; 8] },
}

impl Display for ProtoEvent {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            ProtoEvent::Enter(s) => write!(f, "Enter({s})"),
            ProtoEvent::Leave(s) => write!(f, "Leave({s})"),
            ProtoEvent::Ack(s) => write!(f, "Ack({s})"),
            ProtoEvent::Input(e) => write!(f, "{e}"),
            ProtoEvent::Ping => write!(f, "ping"),
            ProtoEvent::Pong(alive) => {
                write!(
                    f,
                    "pong: {}",
                    if *alive { "alive" } else { "not available" }
                )
            }
            ProtoEvent::Hello { commit } => {
                let s = std::str::from_utf8(commit).unwrap_or("????????");
                write!(f, "Hello({s})")
            }
        }
    }
}

#[derive(TryFromPrimitive, IntoPrimitive)]
#[repr(u8)]
pub enum EventType {
    PointerMotion,
    PointerButton,
    PointerAxis,
    PointerAxisValue120,
    KeyboardKey,
    KeyboardModifiers,
    Ping,
    Pong,
    Enter,
    Leave,
    Ack,
    Hello,
}

impl ProtoEvent {
    fn event_type(&self) -> EventType {
        match self {
            ProtoEvent::Input(e) => match e {
                InputEvent::Pointer(p) => match p {
                    PointerEvent::Motion { .. } => EventType::PointerMotion,
                    PointerEvent::Button { .. } => EventType::PointerButton,
                    PointerEvent::Axis { .. } => EventType::PointerAxis,
                    PointerEvent::AxisDiscrete120 { .. } => EventType::PointerAxisValue120,
                },
                InputEvent::Keyboard(k) => match k {
                    KeyboardEvent::Key { .. } => EventType::KeyboardKey,
                    KeyboardEvent::Modifiers { .. } => EventType::KeyboardModifiers,
                },
            },
            ProtoEvent::Ping => EventType::Ping,
            ProtoEvent::Pong(_) => EventType::Pong,
            ProtoEvent::Enter(_) => EventType::Enter,
            ProtoEvent::Leave(_) => EventType::Leave,
            ProtoEvent::Ack(_) => EventType::Ack,
            ProtoEvent::Hello { .. } => EventType::Hello,
        }
    }
}

impl TryFrom<[u8; MAX_EVENT_SIZE]> for ProtoEvent {
    type Error = ProtocolError;

    fn try_from(buf: [u8; MAX_EVENT_SIZE]) -> Result<Self, Self::Error> {
        let mut buf = &buf[..];
        let event_type = decode_u8(&mut buf)?;
        match EventType::try_from(event_type)? {
            EventType::PointerMotion => {
                Ok(Self::Input(InputEvent::Pointer(PointerEvent::Motion {
                    time: decode_u32(&mut buf)?,
                    dx: decode_f64(&mut buf)?,
                    dy: decode_f64(&mut buf)?,
                })))
            }
            EventType::PointerButton => {
                Ok(Self::Input(InputEvent::Pointer(PointerEvent::Button {
                    time: decode_u32(&mut buf)?,
                    button: decode_u32(&mut buf)?,
                    state: decode_u32(&mut buf)?,
                })))
            }
            EventType::PointerAxis => Ok(Self::Input(InputEvent::Pointer(PointerEvent::Axis {
                time: decode_u32(&mut buf)?,
                axis: decode_u8(&mut buf)?,
                value: decode_f64(&mut buf)?,
            }))),
            EventType::PointerAxisValue120 => Ok(Self::Input(InputEvent::Pointer(
                PointerEvent::AxisDiscrete120 {
                    axis: decode_u8(&mut buf)?,
                    value: decode_i32(&mut buf)?,
                },
            ))),
            EventType::KeyboardKey => Ok(Self::Input(InputEvent::Keyboard(KeyboardEvent::Key {
                time: decode_u32(&mut buf)?,
                key: decode_u32(&mut buf)?,
                state: decode_u8(&mut buf)?,
            }))),
            EventType::KeyboardModifiers => Ok(Self::Input(InputEvent::Keyboard(
                KeyboardEvent::Modifiers {
                    depressed: decode_u32(&mut buf)?,
                    latched: decode_u32(&mut buf)?,
                    locked: decode_u32(&mut buf)?,
                    group: decode_u32(&mut buf)?,
                },
            ))),
            EventType::Ping => Ok(Self::Ping),
            EventType::Pong => Ok(Self::Pong(decode_u8(&mut buf)? != 0)),
            EventType::Enter => Ok(Self::Enter(decode_u8(&mut buf)?.try_into()?)),
            EventType::Leave => Ok(Self::Leave(decode_u32(&mut buf)?)),
            EventType::Ack => Ok(Self::Ack(decode_u32(&mut buf)?)),
            EventType::Hello => {
                let mut commit = [0u8; 8];
                for b in commit.iter_mut() {
                    *b = decode_u8(&mut buf)?;
                }
                Ok(Self::Hello { commit })
            }
        }
    }
}

impl From<ProtoEvent> for ([u8; MAX_EVENT_SIZE], usize) {
    fn from(event: ProtoEvent) -> Self {
        let mut buf = [0u8; MAX_EVENT_SIZE];
        let mut len = 0usize;
        {
            let mut buf = &mut buf[..];
            let buf = &mut buf;
            let len = &mut len;
            encode_u8(buf, len, event.event_type() as u8);
            match event {
                ProtoEvent::Input(event) => match event {
                    InputEvent::Pointer(p) => match p {
                        PointerEvent::Motion { time, dx, dy } => {
                            encode_u32(buf, len, time);
                            encode_f64(buf, len, dx);
                            encode_f64(buf, len, dy);
                        }
                        PointerEvent::Button {
                            time,
                            button,
                            state,
                        } => {
                            encode_u32(buf, len, time);
                            encode_u32(buf, len, button);
                            encode_u32(buf, len, state);
                        }
                        PointerEvent::Axis { time, axis, value } => {
                            encode_u32(buf, len, time);
                            encode_u8(buf, len, axis);
                            encode_f64(buf, len, value);
                        }
                        PointerEvent::AxisDiscrete120 { axis, value } => {
                            encode_u8(buf, len, axis);
                            encode_i32(buf, len, value);
                        }
                    },
                    InputEvent::Keyboard(k) => match k {
                        KeyboardEvent::Key { time, key, state } => {
                            encode_u32(buf, len, time);
                            encode_u32(buf, len, key);
                            encode_u8(buf, len, state);
                        }
                        KeyboardEvent::Modifiers {
                            depressed,
                            latched,
                            locked,
                            group,
                        } => {
                            encode_u32(buf, len, depressed);
                            encode_u32(buf, len, latched);
                            encode_u32(buf, len, locked);
                            encode_u32(buf, len, group);
                        }
                    },
                },
                ProtoEvent::Ping => {}
                ProtoEvent::Pong(alive) => encode_u8(buf, len, alive as u8),
                ProtoEvent::Enter(pos) => encode_u8(buf, len, pos as u8),
                ProtoEvent::Leave(serial) => encode_u32(buf, len, serial),
                ProtoEvent::Ack(serial) => encode_u32(buf, len, serial),
                ProtoEvent::Hello { commit } => {
                    for b in commit.iter() {
                        encode_u8(buf, len, *b);
                    }
                }
            }
        }
        (buf, len)
    }
}

macro_rules! decode_impl {
    ($t:ty) => {
        paste! {
            fn [<decode_ $t>](data: &mut &[u8]) -> Result<$t, ProtocolError> {
                let (int_bytes, rest) = data.split_at(size_of::<$t>());
                *data = rest;
                Ok($t::from_be_bytes(int_bytes.try_into().unwrap()))
            }
        }
    };
}

decode_impl!(u8);
decode_impl!(u32);
decode_impl!(i32);
decode_impl!(f64);

macro_rules! encode_impl {
    ($t:ty) => {
        paste! {
            fn [<encode_ $t>](buf: &mut &mut [u8], amt: &mut usize, n: $t) {
                let src = n.to_be_bytes();
                let data = std::mem::take(buf);
                let (int_bytes, rest) = data.split_at_mut(size_of::<$t>());
                int_bytes.copy_from_slice(&src);
                *amt += size_of::<$t>();
                *buf = rest
            }
        }
    };
}

encode_impl!(u8);
encode_impl!(u32);
encode_impl!(i32);
encode_impl!(f64);

#[cfg(test)]
mod tests {
    use super::*;

    fn offer(names: Vec<String>) -> FileOfferAnnouncement {
        FileOfferAnnouncement {
            token: [7; 32],
            port: 4242,
            item_count: names.len() as u32,
            names,
        }
    }

    #[test]
    fn file_offer_round_trips_through_frames() {
        let announcement = offer(vec!["Fotos".into(), "relatório.pdf".into()]);
        let encoded = announcement.encode();
        let frames = chunk_file_offer([1; 16], &encoded).unwrap();
        let mut reassembler = ClipboardReassembler::default();
        let mut result = None;
        for frame in frames {
            let ClipboardFrame::FileOfferChunk {
                update_id,
                index,
                count,
                total_bytes,
                checksum,
                payload,
            } = ClipboardFrame::decode(&frame.encode().unwrap()).unwrap()
            else {
                panic!("unexpected frame");
            };
            result = Some(
                reassembler
                    .push_file_offer(update_id, index, count, total_bytes, checksum, payload)
                    .unwrap(),
            );
        }
        let Some(ClipboardReassemblyResult::CompleteFileOffer(bytes)) = result else {
            panic!("offer was not reassembled");
        };
        assert_eq!(FileOfferAnnouncement::decode(&bytes).unwrap(), announcement);
    }

    #[test]
    fn file_offer_names_are_cut_to_fit() {
        let names = (0..10_000).map(|i| format!("arquivo-{i:05}.txt")).collect();
        let encoded = offer(names).encode();
        assert!(encoded.len() <= MAX_CLIPBOARD_FILE_OFFER_BYTES);
        let decoded = FileOfferAnnouncement::decode(&encoded).unwrap();
        assert_eq!(decoded.item_count, 10_000);
        assert!(decoded.names.len() < 10_000);
        assert_eq!(decoded.names[0], "arquivo-00000.txt");
    }

    #[test]
    fn malformed_file_offers_are_rejected() {
        let encoded = offer(vec!["a".into()]).encode();
        assert!(FileOfferAnnouncement::decode(&encoded[..encoded.len() - 1]).is_err());
        let mut trailing = encoded.clone();
        trailing.push(0);
        assert!(FileOfferAnnouncement::decode(&trailing).is_err());
        assert!(FileOfferAnnouncement::decode(b"LMF1").is_err());
    }
}
