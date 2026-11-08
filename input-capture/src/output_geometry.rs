use input_event::{MonitorDescriptor, MonitorId};

use crate::{CapturePoint, Position};

#[derive(Clone, Debug)]
pub(crate) struct OutputGeometry {
    pub id: MonitorId,
    pub label: String,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl OutputGeometry {
    pub(crate) fn monitor_descriptor(&self) -> Option<MonitorDescriptor> {
        let label = truncate_utf8(&self.label, input_event::MAX_MONITOR_LABEL_BYTES);
        MonitorDescriptor::new(
            self.id.clone(),
            label,
            u32::try_from(self.width).ok()?,
            u32::try_from(self.height).ok()?,
        )
        .ok()
        .map(|descriptor| descriptor.with_native_position(self.x, self.y))
    }

    #[cfg(any(windows, test))]
    fn contains(&self, point: (i32, i32)) -> bool {
        self.width > 0
            && self.height > 0
            && self.x <= point.0
            && point.0 < self.x.saturating_add(self.width)
            && self.y <= point.1
            && point.1 < self.y.saturating_add(self.height)
    }
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

pub(crate) fn select_output_ids(
    outputs: &[OutputGeometry],
    point: &CapturePoint,
) -> Vec<MonitorId> {
    outputs
        .iter()
        .filter(|output| {
            point
                .monitor_id
                .as_ref()
                .is_none_or(|monitor_id| monitor_id == &output.id)
        })
        .filter(|output| !edge_is_shared(output, point.edge, outputs))
        .map(|output| output.id.clone())
        .collect()
}

fn edge_is_shared(output: &OutputGeometry, edge: Position, outputs: &[OutputGeometry]) -> bool {
    outputs.iter().any(|other| {
        if output.id == other.id {
            return false;
        }
        match edge {
            Position::Left => {
                output.x == other.x.saturating_add(other.width)
                    && overlaps(
                        output.y,
                        output.y.saturating_add(output.height),
                        other.y,
                        other.y.saturating_add(other.height),
                    )
            }
            Position::Right => {
                output.x.saturating_add(output.width) == other.x
                    && overlaps(
                        output.y,
                        output.y.saturating_add(output.height),
                        other.y,
                        other.y.saturating_add(other.height),
                    )
            }
            Position::Top => {
                output.y == other.y.saturating_add(other.height)
                    && overlaps(
                        output.x,
                        output.x.saturating_add(output.width),
                        other.x,
                        other.x.saturating_add(other.width),
                    )
            }
            Position::Bottom => {
                output.y.saturating_add(output.height) == other.y
                    && overlaps(
                        output.x,
                        output.x.saturating_add(output.width),
                        other.x,
                        other.x.saturating_add(other.width),
                    )
            }
        }
    })
}

fn overlaps(first_start: i32, first_end: i32, second_start: i32, second_end: i32) -> bool {
    first_start < second_end && second_start < first_end
}

#[cfg(any(windows, test))]
pub(crate) fn entered_barrier(
    previous: (i32, i32),
    current: (i32, i32),
    outputs: &[OutputGeometry],
) -> Option<CapturePoint> {
    let source = outputs.iter().find(|output| output.contains(previous))?;
    if outputs.iter().any(|output| output.contains(current)) {
        return None;
    }

    let right = source.x.saturating_add(source.width);
    let bottom = source.y.saturating_add(source.height);
    let crossed_edge = [
        (Position::Left, current.0 < source.x),
        (Position::Right, current.0 >= right),
        (Position::Top, current.1 < source.y),
        (Position::Bottom, current.1 >= bottom),
    ]
    .into_iter()
    .find_map(|(edge, crossed)| crossed.then_some(edge))?;

    let point = CapturePoint {
        monitor_id: Some(source.id.clone()),
        edge: crossed_edge,
    };
    select_output_ids(outputs, &point)
        .contains(&source.id)
        .then_some(point)
}

#[cfg(any(windows, test))]
pub(crate) fn clamp_to_display_bounds(
    outputs: &[OutputGeometry],
    previous: (i32, i32),
    point: (i32, i32),
) -> (i32, i32) {
    let output = outputs
        .iter()
        .find(|output| output.contains(previous))
        .expect("previous cursor position must be inside an output");
    let min_x = output.x;
    let max_x = output.x.saturating_add(output.width).saturating_sub(1);
    let min_y = output.y;
    let max_y = output.y.saturating_add(output.height).saturating_sub(1);
    (point.0.clamp(min_x, max_x), point.1.clamp(min_y, max_y))
}

#[cfg(test)]
mod tests {
    use input_event::MonitorId;

    use super::{OutputGeometry, entered_barrier, select_output_ids};
    use crate::{CapturePoint, Position};

    fn output(id: &str, x: i32, y: i32, width: i32, height: i32) -> OutputGeometry {
        OutputGeometry {
            id: MonitorId::new(id.to_owned()).unwrap(),
            label: id.to_owned(),
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn edge_entry_identifies_the_source_output_and_edge() {
        let outputs = [output("DP-1", 0, 0, 100, 100)];
        let cases = [
            ((0, 50), (-1, 50), Position::Left),
            ((99, 50), (100, 50), Position::Right),
            ((50, 0), (50, -1), Position::Top),
            ((50, 99), (50, 100), Position::Bottom),
        ];
        for (previous, current, edge) in cases {
            assert_eq!(
                entered_barrier(previous, current, &outputs),
                Some(CapturePoint {
                    monitor_id: Some(MonitorId::new("DP-1".into()).unwrap()),
                    edge,
                })
            );
        }
    }

    #[test]
    fn moving_into_an_adjacent_local_output_is_not_an_edge_entry() {
        let outputs = [
            output("DP-1", 0, 0, 100, 100),
            output("DP-2", 100, 0, 100, 100),
        ];
        assert_eq!(entered_barrier((99, 50), (100, 50), &outputs), None);
    }

    #[test]
    fn monitor_specific_edge_selects_only_the_requested_output() {
        let outputs = [
            output("DP-1", 0, 0, 100, 100),
            output("DP-2", 120, 0, 100, 100),
        ];
        let point = CapturePoint {
            monitor_id: Some(MonitorId::new("DP-2".into()).unwrap()),
            edge: Position::Right,
        };
        assert_eq!(
            select_output_ids(&outputs, &point),
            vec![MonitorId::new("DP-2".into()).unwrap()]
        );
    }

    #[test]
    fn shared_edge_between_local_outputs_is_suppressed() {
        let outputs = [
            output("DP-1", 0, 0, 100, 100),
            output("DP-2", 100, 0, 100, 100),
        ];
        let point = CapturePoint {
            monitor_id: Some(MonitorId::new("DP-1".into()).unwrap()),
            edge: Position::Right,
        };
        assert!(select_output_ids(&outputs, &point).is_empty());
    }

    #[test]
    fn cursor_is_clamped_to_its_source_output() {
        let outputs = [
            output("DP-1", 0, 0, 100, 100),
            output("DP-2", 100, 0, 200, 100),
        ];
        assert_eq!(
            super::clamp_to_display_bounds(&outputs, (50, 50), (500, -10)),
            (99, 0)
        );
    }
}
