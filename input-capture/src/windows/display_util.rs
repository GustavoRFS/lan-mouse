use crate::{CapturePoint, output_geometry::OutputGeometry};

pub(crate) fn entered_barrier(
    previous: (i32, i32),
    current: (i32, i32),
    displays: &[OutputGeometry],
) -> Option<CapturePoint> {
    crate::output_geometry::entered_barrier(previous, current, displays)
}

pub(crate) fn clamp_to_display_bounds(
    display_regions: &[OutputGeometry],
    previous: (i32, i32),
    point: (i32, i32),
) -> (i32, i32) {
    crate::output_geometry::clamp_to_display_bounds(display_regions, previous, point)
}
