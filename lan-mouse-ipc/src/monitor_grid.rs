use std::collections::HashSet;

use input_event::MonitorId;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::Position;

pub const MAX_MONITOR_GRID_ENTRIES: usize = 256;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct GridPosition {
    pub x: i32,
    pub y: i32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MonitorGridEntry {
    pub owner_fingerprint: String,
    pub monitor_id: MonitorId,
    pub position: GridPosition,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "MonitorGridUnchecked")]
pub struct MonitorGrid {
    pub revision: u64,
    pub origin_fingerprint: String,
    pub entries: Vec<MonitorGridEntry>,
}

#[derive(Deserialize)]
struct MonitorGridUnchecked {
    revision: u64,
    origin_fingerprint: String,
    entries: Vec<MonitorGridEntry>,
}

impl TryFrom<MonitorGridUnchecked> for MonitorGrid {
    type Error = MonitorGridError;

    fn try_from(value: MonitorGridUnchecked) -> Result<Self, Self::Error> {
        Self::new(value.revision, value.origin_fingerprint, value.entries)
    }
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum MonitorGridError {
    #[error("monitor grid has {0} entries; maximum is {MAX_MONITOR_GRID_ENTRIES}")]
    TooManyEntries(usize),
    #[error("monitor grid contains more than one display at {0:?}")]
    DuplicatePosition(GridPosition),
    #[error(
        "monitor grid contains monitor {monitor_id:?} more than once for owner {owner_fingerprint}"
    )]
    DuplicateMonitor {
        owner_fingerprint: String,
        monitor_id: MonitorId,
    },
}

impl MonitorGrid {
    pub fn new(
        revision: u64,
        origin_fingerprint: String,
        entries: Vec<MonitorGridEntry>,
    ) -> Result<Self, MonitorGridError> {
        if entries.len() > MAX_MONITOR_GRID_ENTRIES {
            return Err(MonitorGridError::TooManyEntries(entries.len()));
        }

        let mut positions = HashSet::with_capacity(entries.len());
        let mut monitors = HashSet::with_capacity(entries.len());
        for entry in &entries {
            if !positions.insert(entry.position) {
                return Err(MonitorGridError::DuplicatePosition(entry.position));
            }
            if !monitors.insert((entry.owner_fingerprint.as_str(), &entry.monitor_id)) {
                return Err(MonitorGridError::DuplicateMonitor {
                    owner_fingerprint: entry.owner_fingerprint.clone(),
                    monitor_id: entry.monitor_id.clone(),
                });
            }
        }

        Ok(Self {
            revision,
            origin_fingerprint,
            entries,
        })
    }

    pub fn validate(&self) -> Result<(), MonitorGridError> {
        Self::new(
            self.revision,
            self.origin_fingerprint.clone(),
            self.entries.clone(),
        )
        .map(|_| ())
    }

    pub fn neighbor(
        &self,
        owner_fingerprint: &str,
        monitor_id: &MonitorId,
        edge: Position,
    ) -> Option<&MonitorGridEntry> {
        let source = self.entries.iter().find(|entry| {
            entry.owner_fingerprint == owner_fingerprint && &entry.monitor_id == monitor_id
        })?;
        let target = match edge {
            Position::Left => GridPosition {
                x: source.position.x.checked_sub(1)?,
                y: source.position.y,
            },
            Position::Right => GridPosition {
                x: source.position.x.checked_add(1)?,
                y: source.position.y,
            },
            Position::Top => GridPosition {
                x: source.position.x,
                y: source.position.y.checked_sub(1)?,
            },
            Position::Bottom => GridPosition {
                x: source.position.x,
                y: source.position.y.checked_add(1)?,
            },
        };
        self.entries.iter().find(|entry| entry.position == target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Position;
    use input_event::MonitorId;

    fn monitor_id(value: &str) -> MonitorId {
        MonitorId::new(value.to_owned()).unwrap()
    }

    fn entry(owner: &str, id: &str, x: i32, y: i32) -> MonitorGridEntry {
        MonitorGridEntry {
            owner_fingerprint: owner.to_owned(),
            monitor_id: monitor_id(id),
            position: GridPosition { x, y },
        }
    }

    #[test]
    fn neighbor_returns_the_occupied_cell_in_each_direction() {
        let source = entry("local", "center", 0, 0);
        let cases = [
            (Position::Left, entry("remote", "left", -1, 0)),
            (Position::Right, entry("remote", "right", 1, 0)),
            (Position::Top, entry("remote", "top", 0, -1)),
            (Position::Bottom, entry("remote", "bottom", 0, 1)),
        ];

        for (edge, neighbor) in cases {
            let grid = MonitorGrid::new(1, "local".into(), vec![source.clone(), neighbor.clone()])
                .unwrap();
            assert_eq!(
                grid.neighbor("local", &source.monitor_id, edge),
                Some(&neighbor)
            );
        }
    }

    #[test]
    fn neighbor_preserves_owner_for_local_and_remote_cells() {
        let source = entry("local", "source", 0, 0);
        let local = entry("local", "second", 1, 0);
        let grid =
            MonitorGrid::new(1, "local".into(), vec![source.clone(), local.clone()]).unwrap();
        assert_eq!(
            grid.neighbor("local", &source.monitor_id, Position::Right),
            Some(&local)
        );

        let remote = entry("remote", "display", 1, 0);
        let grid =
            MonitorGrid::new(2, "local".into(), vec![source.clone(), remote.clone()]).unwrap();
        assert_eq!(
            grid.neighbor("local", &source.monitor_id, Position::Right),
            Some(&remote)
        );
    }

    #[test]
    fn rejects_duplicate_cells_and_duplicate_owner_monitor_pairs() {
        let first = entry("local", "one", 0, 0);
        let second = entry("remote", "two", 0, 0);
        assert!(matches!(
            MonitorGrid::new(1, "local".into(), vec![first.clone(), second]),
            Err(MonitorGridError::DuplicatePosition(_))
        ));

        let duplicate = entry("local", "one", 1, 0);
        assert!(matches!(
            MonitorGrid::new(1, "local".into(), vec![first, duplicate]),
            Err(MonitorGridError::DuplicateMonitor { .. })
        ));
    }

    #[test]
    fn empty_or_unknown_neighbors_are_none() {
        let source = entry("local", "source", 0, 0);
        let grid = MonitorGrid::new(1, "local".into(), vec![source.clone()]).unwrap();
        assert_eq!(
            grid.neighbor("local", &source.monitor_id, Position::Right),
            None
        );
        assert_eq!(
            grid.neighbor("local", &monitor_id("missing"), Position::Right),
            None
        );
    }

    #[test]
    fn enforces_grid_size_and_monitor_id_byte_limits() {
        assert!(MonitorId::new(String::new()).is_err());
        assert!(MonitorId::new("x".repeat(97)).is_err());
        MonitorId::new("é".repeat(48)).unwrap();
        assert!(MonitorId::new("é".repeat(49)).is_err());

        let entries = (0..257)
            .map(|index| entry("local", &format!("display-{index}"), index, 0))
            .collect();
        assert!(matches!(
            MonitorGrid::new(1, "local".into(), entries),
            Err(MonitorGridError::TooManyEntries(257))
        ));
    }
}
