#[cfg(test)]
mod tests {
    use super::*;
    use input_event::MonitorId;
    use lan_mouse_ipc::Position;

    #[test]
    fn entry_midpoint_uses_destination_geometry() {
        let output = MonitorGeometry {
            id: MonitorId::new("remote-output".into()).unwrap(),
            x: 1920,
            y: 100,
            width: 1280,
            height: 1024,
        };

        assert_eq!(
            resolve_entry_midpoint(
                &[output],
                &MonitorId::new("remote-output".into()).unwrap(),
                Position::Left
            ),
            Ok(PointerPosition { x: 1920, y: 612 })
        );
    }

    #[test]
    fn unknown_output_id_returns_an_error() {
        let known = MonitorGeometry {
            id: MonitorId::new("connected".into()).unwrap(),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        };
        assert!(matches!(
            resolve_entry_midpoint(
                &[known],
                &MonitorId::new("unplugged".into()).unwrap(),
                Position::Right
            ),
            Err(MonitorPositionError::UnknownOutput(_))
        ));
    }
}
use input_event::MonitorId;
use lan_mouse_ipc::Position;
use std::fmt;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MonitorGeometry {
    pub id: MonitorId,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PointerPosition {
    pub x: i32,
    pub y: i32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MonitorPositionError {
    UnknownOutput(MonitorId),
    InvalidGeometry(MonitorId),
}

impl fmt::Display for MonitorPositionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownOutput(id) => {
                write!(f, "monitor output `{}` is not connected", id.as_str())
            }
            Self::InvalidGeometry(id) => {
                write!(f, "monitor output `{}` has invalid geometry", id.as_str())
            }
        }
    }
}

impl std::error::Error for MonitorPositionError {}

pub(crate) fn resolve_entry_midpoint(
    outputs: &[MonitorGeometry],
    monitor_id: &MonitorId,
    edge: Position,
) -> Result<PointerPosition, MonitorPositionError> {
    let output = outputs
        .iter()
        .find(|output| &output.id == monitor_id)
        .ok_or_else(|| MonitorPositionError::UnknownOutput(monitor_id.clone()))?;
    if output.width == 0 || output.height == 0 {
        return Err(MonitorPositionError::InvalidGeometry(monitor_id.clone()));
    }
    let width = i32::try_from(output.width)
        .map_err(|_| MonitorPositionError::InvalidGeometry(monitor_id.clone()))?;
    let height = i32::try_from(output.height)
        .map_err(|_| MonitorPositionError::InvalidGeometry(monitor_id.clone()))?;
    let midpoint_x = output.x.saturating_add(width / 2);
    let midpoint_y = output.y.saturating_add(height / 2);
    let right = output.x.saturating_add(width - 1);
    let bottom = output.y.saturating_add(height - 1);
    Ok(match edge {
        Position::Left => PointerPosition {
            x: output.x,
            y: midpoint_y,
        },
        Position::Right => PointerPosition {
            x: right,
            y: midpoint_y,
        },
        Position::Top => PointerPosition {
            x: midpoint_x,
            y: output.y,
        },
        Position::Bottom => PointerPosition {
            x: midpoint_x,
            y: bottom,
        },
    })
}
