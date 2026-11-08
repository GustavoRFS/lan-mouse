#[cfg(test)]
mod tests {
    use super::*;
    use input_event::MonitorId;
    use lan_mouse_ipc::{GridPosition, MonitorGrid, MonitorGridEntry, Position};
    use std::collections::{HashMap, HashSet};

    fn id(value: &str) -> MonitorId {
        MonitorId::new(value.to_owned()).unwrap()
    }

    fn entry(owner: &str, output: &str, x: i32, y: i32) -> MonitorGridEntry {
        MonitorGridEntry {
            owner_fingerprint: owner.to_owned(),
            monitor_id: id(output),
            position: GridPosition { x, y },
        }
    }

    #[test]
    fn route_uses_exact_source_monitor_and_edge() {
        let local = entry("local", "left-monitor", 0, 0);
        let grid = MonitorGrid::new(
            2,
            "local".into(),
            vec![
                local.clone(),
                entry("local", "right-monitor", 1, 0),
                entry("remote", "upper-monitor", 0, -1),
                entry("remote", "lower-monitor", 0, 1),
            ],
        )
        .unwrap();

        assert_eq!(
            route(&grid, "local", &local.monitor_id, Position::Top),
            Some(entry("remote", "upper-monitor", 0, -1))
        );
        assert_eq!(
            route(&grid, "local", &local.monitor_id, Position::Right),
            None,
            "a neighboring local output must remain under the OS layout"
        );
        assert_eq!(route(&grid, "local", &id("missing"), Position::Top), None);
    }

    #[test]
    fn unavailable_outputs_are_not_routed() {
        let source = entry("local", "source", 0, 0);
        let target = entry("remote", "target", 1, 0);
        let grid =
            MonitorGrid::new(1, "local".into(), vec![source.clone(), target.clone()]).unwrap();
        let catalogs = HashMap::from([("remote".to_owned(), HashSet::from([id("other")]))]);

        assert_eq!(
            route_available(
                &grid,
                &catalogs,
                "local",
                &source.monitor_id,
                Position::Right
            ),
            None
        );
        let catalogs = HashMap::from([(
            "remote".to_owned(),
            HashSet::from([target.monitor_id.clone()]),
        )]);
        assert_eq!(
            route_available(
                &grid,
                &catalogs,
                "local",
                &source.monitor_id,
                Position::Right
            ),
            Some(target)
        );
    }

    #[test]
    fn legacy_peer_capability_falls_back_to_position() {
        let target = entry("remote", "display-2", 1, 0);
        assert_eq!(
            enter_event(false, &target, Position::Right),
            lan_mouse_proto::ProtoEvent::Enter(lan_mouse_proto::Position::Left)
        );
        assert_eq!(
            enter_event(true, &target, Position::Right),
            lan_mouse_proto::ProtoEvent::EnterMonitor {
                monitor_id: target.monitor_id,
                edge: lan_mouse_proto::Position::Left,
            }
        );
    }

    #[test]
    fn grid_snapshot_reassembles_and_retries_lost_chunks() {
        let entries = (0..32)
            .map(|index| {
                entry(
                    "local",
                    &format!("display-{index:02}-{}", "x".repeat(70)),
                    index,
                    0,
                )
            })
            .collect();
        let grid = MonitorGrid::new(14, "origin".into(), entries).unwrap();
        let mut sender = GridSnapshotSender::new(&grid).unwrap();
        let chunks = sender.pending_chunks();
        assert!(chunks.len() > 1);

        let mut receiver = GridSnapshotReassembler::default();
        for chunk in chunks.iter().skip(1).rev() {
            assert_eq!(receiver.push(chunk.clone()).unwrap(), None);
        }
        assert_eq!(sender.retry_unacknowledged(), chunks);
        let mut applied = None;
        for chunk in chunks.into_iter().take(1) {
            applied = receiver.push(chunk).unwrap().or(applied);
        }
        assert_eq!(applied, Some(GridSnapshotUpdate::Applied(grid.clone())));
        assert!(sender.acknowledge(grid.revision, &grid.origin_fingerprint));
        assert!(sender.pending_chunks().is_empty());
    }
}
use input_event::{MonitorDescriptor, MonitorId};
use lan_mouse_ipc::{MonitorGrid, MonitorGridEntry, Position};
use lan_mouse_proto::ProtoEvent;
use std::collections::{HashMap, HashSet};
use thiserror::Error;

const MAX_SNAPSHOT_CHUNKS: usize = 256;
const MAX_SNAPSHOT_BYTES: usize =
    MAX_SNAPSHOT_CHUNKS * lan_mouse_proto::MAX_MONITOR_GRID_CHUNK_PAYLOAD;
const MAX_PARTIAL_SNAPSHOTS: usize = 4;
const MAX_CATALOG_CHUNKS: usize = 52;

pub fn route(
    grid: &MonitorGrid,
    source_owner: &str,
    source_monitor: &MonitorId,
    edge: Position,
) -> Option<MonitorGridEntry> {
    grid.neighbor(source_owner, source_monitor, edge)
        .filter(|entry| entry.owner_fingerprint != source_owner)
        .cloned()
}

pub fn route_available(
    grid: &MonitorGrid,
    catalogs: &HashMap<String, HashSet<MonitorId>>,
    source_owner: &str,
    source_monitor: &MonitorId,
    edge: Position,
) -> Option<MonitorGridEntry> {
    let target = route(grid, source_owner, source_monitor, edge)?;
    catalogs
        .get(&target.owner_fingerprint)
        .is_some_and(|catalog| catalog.contains(&target.monitor_id))
        .then_some(target)
}

pub fn enter_event(
    supports_monitor_grid: bool,
    target: &MonitorGridEntry,
    source_edge: Position,
) -> ProtoEvent {
    let target_edge = proto_position(source_edge.opposite());
    if supports_monitor_grid {
        ProtoEvent::EnterMonitor {
            monitor_id: target.monitor_id.clone(),
            edge: target_edge,
        }
    } else {
        ProtoEvent::Enter(target_edge)
    }
}

pub fn catalog_chunks(transfer_id: u32, monitors: &[MonitorDescriptor]) -> Vec<ProtoEvent> {
    if monitors.is_empty() {
        return Vec::new();
    }
    let count = monitors
        .len()
        .div_ceil(lan_mouse_proto::MAX_MONITOR_CATALOG_CHUNK_ITEMS);
    if count > MAX_CATALOG_CHUNKS {
        return Vec::new();
    }
    monitors
        .chunks(lan_mouse_proto::MAX_MONITOR_CATALOG_CHUNK_ITEMS)
        .enumerate()
        .map(|(index, descriptors)| ProtoEvent::MonitorCatalogChunk {
            transfer_id,
            index: index as u16,
            count: count as u16,
            descriptors: descriptors.to_vec(),
        })
        .collect()
}

fn proto_position(position: Position) -> lan_mouse_proto::Position {
    match position {
        Position::Left => lan_mouse_proto::Position::Left,
        Position::Right => lan_mouse_proto::Position::Right,
        Position::Top => lan_mouse_proto::Position::Top,
        Position::Bottom => lan_mouse_proto::Position::Bottom,
    }
}

#[derive(Debug, Error)]
pub enum GridSnapshotError {
    #[error("failed to serialize monitor grid: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("monitor grid snapshot exceeds the bounded transfer size")]
    TooLarge,
    #[error("invalid monitor grid snapshot chunk")]
    InvalidChunk,
    #[error("monitor grid snapshot chunks disagree")]
    ConflictingChunks,
}

pub struct GridSnapshotSender {
    revision: u64,
    origin_fingerprint: String,
    chunks: Vec<ProtoEvent>,
    acknowledged: bool,
}

impl GridSnapshotSender {
    pub fn new(grid: &MonitorGrid) -> Result<Self, GridSnapshotError> {
        let payload = serde_json::to_vec(grid)?;
        if payload.is_empty()
            || payload.len() > MAX_SNAPSHOT_BYTES
            || grid.origin_fingerprint.is_empty()
            || grid.origin_fingerprint.len() > 96
        {
            return Err(GridSnapshotError::TooLarge);
        }
        let count = payload
            .len()
            .div_ceil(lan_mouse_proto::MAX_MONITOR_GRID_CHUNK_PAYLOAD);
        if count == 0 || count > MAX_SNAPSHOT_CHUNKS {
            return Err(GridSnapshotError::TooLarge);
        }
        let count = count as u16;
        let chunks = payload
            .chunks(lan_mouse_proto::MAX_MONITOR_GRID_CHUNK_PAYLOAD)
            .enumerate()
            .map(|(index, payload)| ProtoEvent::GridSnapshotChunk {
                revision: grid.revision,
                origin_fingerprint: grid.origin_fingerprint.clone(),
                index: index as u16,
                count,
                payload: payload.to_vec(),
            })
            .collect();
        Ok(Self {
            revision: grid.revision,
            origin_fingerprint: grid.origin_fingerprint.clone(),
            chunks,
            acknowledged: false,
        })
    }

    pub fn pending_chunks(&self) -> Vec<ProtoEvent> {
        if self.acknowledged {
            Vec::new()
        } else {
            self.chunks.clone()
        }
    }

    pub fn retry_unacknowledged(&self) -> Vec<ProtoEvent> {
        self.pending_chunks()
    }

    pub fn acknowledge(&mut self, revision: u64, origin_fingerprint: &str) -> bool {
        if revision == self.revision && origin_fingerprint == self.origin_fingerprint {
            self.acknowledged = true;
            true
        } else {
            false
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GridSnapshotUpdate {
    Applied(MonitorGrid),
    Stale {
        revision: u64,
        origin_fingerprint: String,
    },
}

#[derive(Default)]
pub struct GridSnapshotReassembler {
    partial: HashMap<(u64, String), PartialSnapshot>,
    highest_applied: Option<(u64, String)>,
}

#[derive(Default)]
pub struct MonitorCatalogReassembler {
    partial: HashMap<u32, PartialCatalog>,
}

struct PartialCatalog {
    count: u16,
    chunks: Vec<Option<Vec<MonitorDescriptor>>>,
    count_items: usize,
}

impl MonitorCatalogReassembler {
    pub fn push(
        &mut self,
        event: ProtoEvent,
    ) -> Result<Option<Vec<MonitorDescriptor>>, GridSnapshotError> {
        let ProtoEvent::MonitorCatalogChunk {
            transfer_id,
            index,
            count,
            descriptors,
        } = event
        else {
            return Err(GridSnapshotError::InvalidChunk);
        };
        if count == 0
            || usize::from(count) > MAX_CATALOG_CHUNKS
            || index >= count
            || descriptors.len() > lan_mouse_proto::MAX_MONITOR_CATALOG_CHUNK_ITEMS
            || descriptors
                .iter()
                .any(|descriptor| descriptor.validate().is_err())
        {
            return Err(GridSnapshotError::InvalidChunk);
        }
        if !self.partial.contains_key(&transfer_id) && self.partial.len() >= MAX_PARTIAL_SNAPSHOTS {
            if let Some(oldest) = self.partial.keys().min().copied() {
                self.partial.remove(&oldest);
            }
        }
        let partial = self
            .partial
            .entry(transfer_id)
            .or_insert_with(|| PartialCatalog {
                count,
                chunks: vec![None; usize::from(count)],
                count_items: 0,
            });
        if partial.count != count {
            self.partial.remove(&transfer_id);
            return Err(GridSnapshotError::ConflictingChunks);
        }
        let slot = &mut partial.chunks[usize::from(index)];
        match slot {
            Some(existing) if existing != &descriptors => {
                self.partial.remove(&transfer_id);
                return Err(GridSnapshotError::ConflictingChunks);
            }
            Some(_) => return Ok(None),
            None => {
                partial.count_items = partial.count_items.saturating_add(descriptors.len());
                if partial.count_items > lan_mouse_ipc::MAX_MONITOR_GRID_ENTRIES {
                    self.partial.remove(&transfer_id);
                    return Err(GridSnapshotError::TooLarge);
                }
                *slot = Some(descriptors);
            }
        }
        if partial.chunks.iter().any(Option::is_none) {
            return Ok(None);
        }
        let partial = self
            .partial
            .remove(&transfer_id)
            .expect("completed catalog");
        let monitors = partial
            .chunks
            .into_iter()
            .flatten()
            .flatten()
            .collect::<Vec<_>>();
        let mut ids = HashSet::with_capacity(monitors.len());
        if monitors.len() > lan_mouse_ipc::MAX_MONITOR_GRID_ENTRIES
            || monitors
                .iter()
                .any(|monitor| !ids.insert(monitor.id.clone()))
        {
            return Err(GridSnapshotError::InvalidChunk);
        }
        Ok(Some(monitors))
    }
}

struct PartialSnapshot {
    count: u16,
    chunks: Vec<Option<Vec<u8>>>,
    received_bytes: usize,
}

impl GridSnapshotReassembler {
    pub fn push(
        &mut self,
        event: ProtoEvent,
    ) -> Result<Option<GridSnapshotUpdate>, GridSnapshotError> {
        let ProtoEvent::GridSnapshotChunk {
            revision,
            origin_fingerprint,
            index,
            count,
            payload,
        } = event
        else {
            return Err(GridSnapshotError::InvalidChunk);
        };
        if count == 0
            || usize::from(count) > MAX_SNAPSHOT_CHUNKS
            || index >= count
            || payload.is_empty()
            || payload.len() > lan_mouse_proto::MAX_MONITOR_GRID_CHUNK_PAYLOAD
            || origin_fingerprint.is_empty()
            || origin_fingerprint.len() > 96
        {
            return Err(GridSnapshotError::InvalidChunk);
        }
        let key = (revision, origin_fingerprint.clone());
        if self
            .highest_applied
            .as_ref()
            .is_some_and(|current| &key <= current)
        {
            return Ok(Some(GridSnapshotUpdate::Stale {
                revision,
                origin_fingerprint,
            }));
        }
        if !self.partial.contains_key(&key) && self.partial.len() == MAX_PARTIAL_SNAPSHOTS {
            if let Some(oldest) = self.partial.keys().min().cloned() {
                self.partial.remove(&oldest);
            }
        }
        let partial = self
            .partial
            .entry(key.clone())
            .or_insert_with(|| PartialSnapshot {
                count,
                chunks: vec![None; usize::from(count)],
                received_bytes: 0,
            });
        if partial.count != count {
            self.partial.remove(&key);
            return Err(GridSnapshotError::ConflictingChunks);
        }
        let slot = &mut partial.chunks[usize::from(index)];
        match slot {
            Some(existing) if existing != &payload => {
                self.partial.remove(&key);
                return Err(GridSnapshotError::ConflictingChunks);
            }
            Some(_) => return Ok(None),
            None => {
                partial.received_bytes = partial.received_bytes.saturating_add(payload.len());
                if partial.received_bytes > MAX_SNAPSHOT_BYTES {
                    self.partial.remove(&key);
                    return Err(GridSnapshotError::TooLarge);
                }
                *slot = Some(payload);
            }
        }
        if partial.chunks.iter().any(Option::is_none) {
            return Ok(None);
        }
        let partial = self.partial.remove(&key).expect("completed snapshot");
        let bytes = partial
            .chunks
            .into_iter()
            .flatten()
            .flatten()
            .collect::<Vec<_>>();
        if bytes.is_empty() || bytes.len() > MAX_SNAPSHOT_BYTES {
            return Err(GridSnapshotError::TooLarge);
        }
        let grid: MonitorGrid = serde_json::from_slice(&bytes)?;
        if grid.revision != revision || grid.origin_fingerprint != origin_fingerprint {
            return Err(GridSnapshotError::InvalidChunk);
        }
        grid.validate()
            .map_err(|_| GridSnapshotError::InvalidChunk)?;
        let received_key = (revision, origin_fingerprint.clone());
        if self
            .highest_applied
            .as_ref()
            .is_some_and(|current| &received_key <= current)
        {
            return Ok(Some(GridSnapshotUpdate::Stale {
                revision,
                origin_fingerprint,
            }));
        }
        self.highest_applied = Some(received_key);
        Ok(Some(GridSnapshotUpdate::Applied(grid)))
    }
}
