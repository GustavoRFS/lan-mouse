use std::{cell::RefCell, collections::HashMap, rc::Rc};

use gtk::{glib, prelude::*};
use input_event::{MonitorDescriptor, MonitorId};
use lan_mouse_ipc::{
    ClientHandle, FrontendRequest, GridPosition, MAX_MONITOR_GRID_ENTRIES, MonitorGrid,
    MonitorGridEntry,
};
use thiserror::Error;

#[derive(Debug, Error, Eq, PartialEq)]
enum GridEditorMoveError {
    #[error("monitor is not in the saved grid")]
    UnknownMonitor,
    #[error("local monitor placement follows the operating system layout")]
    LocalMonitor,
    #[error("another monitor already occupies {0:?}")]
    Occupied(GridPosition),
    #[error("the saved layout has reached its display limit")]
    TooManyDisplays,
    #[error("no free grid cell is available")]
    NoFreeCell,
}

struct GridEditorModel {
    grid: MonitorGrid,
    local_owner: String,
}

impl GridEditorModel {
    fn new(grid: MonitorGrid, local_owner: String) -> Self {
        Self { grid, local_owner }
    }

    fn grid(&self) -> &MonitorGrid {
        &self.grid
    }

    fn move_remote_monitor(
        &mut self,
        owner: &str,
        monitor_id: &MonitorId,
        position: GridPosition,
    ) -> Result<(), GridEditorMoveError> {
        if owner == self.local_owner {
            return Err(GridEditorMoveError::LocalMonitor);
        }
        let Some(index) =
            self.grid.entries.iter().position(|entry| {
                entry.owner_fingerprint == owner && &entry.monitor_id == monitor_id
            })
        else {
            return Err(GridEditorMoveError::UnknownMonitor);
        };
        if self
            .grid
            .entries
            .iter()
            .enumerate()
            .any(|(i, entry)| i != index && entry.position == position)
        {
            return Err(GridEditorMoveError::Occupied(position));
        }

        let mut entries = self.grid.entries.clone();
        entries[index].position = position;
        self.grid = MonitorGrid::new(
            self.grid.revision.saturating_add(1),
            self.local_owner.clone(),
            entries,
        )
        .expect("a validated single-tile move preserves grid uniqueness");
        Ok(())
    }

    fn add_remote_catalog(
        &mut self,
        owner: &str,
        monitors: &[MonitorDescriptor],
    ) -> Result<bool, GridEditorMoveError> {
        if owner == self.local_owner {
            return Ok(false);
        }
        let mut entries = self.grid.entries.clone();
        let mut changed = false;
        for monitor in monitors {
            if entries
                .iter()
                .any(|entry| entry.owner_fingerprint == owner && entry.monitor_id == monitor.id)
            {
                continue;
            }
            if entries.len() == MAX_MONITOR_GRID_ENTRIES {
                return Err(GridEditorMoveError::TooManyDisplays);
            }
            let x = entries
                .iter()
                .map(|entry| entry.position.x)
                .max()
                .unwrap_or(-1)
                .checked_add(1);
            let position = x
                .and_then(|x| find_free_cell(&entries, GridPosition { x, y: 0 }))
                .or_else(|| find_free_cell(&entries, GridPosition { x: 0, y: 0 }))
                .ok_or(GridEditorMoveError::NoFreeCell)?;
            entries.push(MonitorGridEntry {
                owner_fingerprint: owner.to_owned(),
                monitor_id: monitor.id.clone(),
                position,
            });
            changed = true;
        }
        if changed {
            self.grid = MonitorGrid::new(
                self.grid.revision.saturating_add(1),
                self.local_owner.clone(),
                entries,
            )
            .map_err(|_| GridEditorMoveError::TooManyDisplays)?;
        }
        Ok(changed)
    }
}

fn find_free_cell(entries: &[MonitorGridEntry], start: GridPosition) -> Option<GridPosition> {
    if !entries.iter().any(|entry| entry.position == start) {
        return Some(start);
    }
    for y_offset in 0..=entries.len() as i32 {
        for x_offset in 0..=entries.len() as i32 {
            let Some(x) = start.x.checked_add(x_offset) else {
                continue;
            };
            let Some(y) = start.y.checked_add(y_offset) else {
                continue;
            };
            let candidate = GridPosition { x, y };
            if !entries.iter().any(|entry| entry.position == candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

#[derive(Clone)]
pub(super) struct GridEditorPeer {
    pub handle: ClientHandle,
    pub name: String,
    pub fingerprint: Option<String>,
    pub supports_monitor_grid: bool,
}

#[derive(Default)]
struct EditorData {
    grid: Option<GridEditorModel>,
    local_owner: Option<String>,
    local_catalog: Vec<MonitorDescriptor>,
    remote_catalogs: HashMap<String, Vec<MonitorDescriptor>>,
    peers: Vec<GridEditorPeer>,
}

pub(super) struct MonitorGridWindow {
    window: adw::Window,
    data: Rc<RefCell<EditorData>>,
    peer_selector: gtk::DropDown,
    discover_button: gtk::Button,
    status: gtk::Label,
    grid: gtk::Grid,
    request: Rc<dyn Fn(FrontendRequest)>,
}

impl MonitorGridWindow {
    pub(super) fn new(
        parent: &impl IsA<gtk::Window>,
        request: impl Fn(FrontendRequest) + 'static,
    ) -> Self {
        let builder = gtk::Builder::from_resource("/de/feschber/LanMouse/monitor_grid_window.ui");
        let window: adw::Window = builder
            .object("monitor_grid_window")
            .expect("monitor grid window resource");
        let peer_selector: gtk::DropDown = builder
            .object("peer_selector")
            .expect("monitor grid peer selector");
        let discover_button: gtk::Button = builder
            .object("discover_button")
            .expect("monitor discovery button");
        let status: gtk::Label = builder
            .object("status_label")
            .expect("monitor grid status label");
        let grid: gtk::Grid = builder.object("monitor_grid").expect("monitor grid layout");
        window.set_transient_for(Some(parent));

        let data = Rc::new(RefCell::new(EditorData::default()));
        let request: Rc<dyn Fn(FrontendRequest)> = Rc::new(request);
        let editor = Self {
            window,
            data,
            peer_selector,
            discover_button,
            status,
            grid,
            request,
        };
        editor.connect_controls();
        editor.render();
        editor
    }

    pub(super) fn present(&self) {
        self.window.present();
    }

    pub(super) fn set_grid(&self, grid: MonitorGrid) {
        let mut data = self.data.borrow_mut();
        let local_owner = data.local_owner.clone().unwrap_or_default();
        let mut model = GridEditorModel::new(grid.clone(), local_owner);
        let changed = data
            .remote_catalogs
            .iter()
            .try_fold(false, |changed, (owner, monitors)| {
                model
                    .add_remote_catalog(owner, monitors)
                    .map(|added| changed || added)
            });
        data.grid = Some(model);
        self.status
            .set_text(&format!("Layout synchronized · revision {}", grid.revision));
        drop(data);
        match changed {
            Ok(true) => self.submit_grid_update(),
            Err(error) => self
                .status
                .set_text(&format!("Cannot add displays: {error}")),
            _ => {}
        }
        self.render();
    }

    pub(super) fn set_local_owner(&self, fingerprint: String) {
        let mut data = self.data.borrow_mut();
        data.local_owner = Some(fingerprint.clone());
        if let Some(model) = data.grid.take() {
            data.grid = Some(GridEditorModel::new(model.grid, fingerprint));
        }
        drop(data);
        self.render();
    }

    pub(super) fn set_local_catalog(&self, monitors: Vec<MonitorDescriptor>) {
        self.data.borrow_mut().local_catalog = monitors;
        self.render();
    }

    pub(super) fn set_remote_catalog(&self, owner: String, monitors: Vec<MonitorDescriptor>) {
        let result = {
            let mut data = self.data.borrow_mut();
            data.remote_catalogs.insert(owner.clone(), monitors.clone());
            data.grid
                .as_mut()
                .map(|model| model.add_remote_catalog(&owner, &monitors))
        };
        match result {
            Some(Ok(true)) => self.submit_grid_update(),
            Some(Err(error)) => self
                .status
                .set_text(&format!("Cannot add displays: {error}")),
            _ if monitors.is_empty() => self
                .status
                .set_text("Discovery completed; this peer reports no named displays"),
            _ => self
                .status
                .set_text("Displays discovered; drag remote tiles to arrange them"),
        }
        self.render();
    }

    pub(super) fn set_peers(&self, peers: Vec<GridEditorPeer>) {
        let current = self.peer_selector.selected() as usize;
        let current_handle = self
            .data
            .borrow()
            .peers
            .get(current)
            .map(|peer| peer.handle);
        let mut names = peers
            .iter()
            .map(|peer| {
                if peer.fingerprint.is_some() && !peer.supports_monitor_grid {
                    format!("{} (legacy)", peer.name)
                } else {
                    peer.name.clone()
                }
            })
            .collect::<Vec<_>>();
        if names.is_empty() {
            names.push("No configured peers".into());
        }
        let strings = gtk::StringList::new(&names.iter().map(String::as_str).collect::<Vec<_>>());
        let selected = current_handle
            .and_then(|handle| peers.iter().position(|peer| peer.handle == handle))
            .unwrap_or_else(|| current.min(peers.len().saturating_sub(1)));
        self.data.borrow_mut().peers = peers;
        self.peer_selector.set_model(Some(&strings));
        self.peer_selector.set_selected(selected as u32);
        self.discover_button
            .set_sensitive(!self.data.borrow().peers.is_empty());
        if self
            .data
            .borrow()
            .peers
            .get(selected)
            .is_some_and(|peer| peer.fingerprint.is_some() && !peer.supports_monitor_grid)
        {
            self.status
                .set_text("Selected peer is legacy; use its direction selector in the main window");
        }
        self.render();
    }

    fn connect_controls(&self) {
        let data = self.data.clone();
        let status = self.status.downgrade();
        let request = self.request.clone();
        let peer_selector = self.peer_selector.downgrade();
        self.discover_button.connect_clicked(move |_| {
            let (Some(status), Some(peer_selector)) = (status.upgrade(), peer_selector.upgrade())
            else {
                return;
            };
            let data = data.borrow();
            let Some(peer) = data.peers.get(peer_selector.selected() as usize) else {
                status.set_text("Add a peer before discovering displays");
                return;
            };
            if peer.fingerprint.is_some() && !peer.supports_monitor_grid {
                status.set_text(
                    "This peer uses legacy directional switching; keep its direction selector",
                );
                return;
            }
            request(FrontendRequest::DiscoverMonitors(peer.handle));
            status.set_text(&format!("Discovering displays on {}…", peer.name));
        });
    }

    fn submit_grid_update(&self) {
        if let Some(grid) = self.data.borrow().grid.as_ref() {
            (self.request)(FrontendRequest::UpdateMonitorGrid(grid.grid().clone()));
            self.status.set_text("Saving and synchronizing layout…");
        }
    }

    fn render(&self) {
        render_grid(&self.grid, &self.status, &self.data, &self.request);
    }
}

fn render_grid(
    grid_widget: &gtk::Grid,
    status: &gtk::Label,
    data: &Rc<RefCell<EditorData>>,
    request: &Rc<dyn Fn(FrontendRequest)>,
) {
    while let Some(child) = grid_widget.first_child() {
        grid_widget.remove(&child);
    }

    let data_ref = data.borrow();
    let Some(model) = data_ref.grid.as_ref() else {
        status.set_text("Waiting for the local display layout…");
        return;
    };
    let local_owner = data_ref.local_owner.as_deref().unwrap_or_default();
    let visible = model.grid().entries.to_vec();
    if visible.is_empty() {
        status.set_text("No displays are in the saved layout yet");
        return;
    }

    let min_x = visible
        .iter()
        .map(|entry| entry.position.x)
        .min()
        .unwrap_or(0);
    let min_y = visible
        .iter()
        .map(|entry| entry.position.y)
        .min()
        .unwrap_or(0);

    for entry in visible {
        let local = entry.owner_fingerprint == local_owner;
        let descriptor = if local {
            data_ref
                .local_catalog
                .iter()
                .find(|monitor| monitor.id == entry.monitor_id)
        } else {
            data_ref
                .remote_catalogs
                .get(&entry.owner_fingerprint)
                .and_then(|monitors| {
                    monitors
                        .iter()
                        .find(|monitor| monitor.id == entry.monitor_id)
                })
        };
        let label = descriptor
            .map(|monitor| monitor.label.clone())
            .unwrap_or_else(|| entry.monitor_id.as_str().to_owned());
        let subtitle = match descriptor {
            Some(monitor) => format!("{} × {}", monitor.width, monitor.height),
            None if local => "Unavailable on this device".into(),
            None => "Unavailable · saved position kept".into(),
        };
        let tile = display_tile(&label, &subtitle, local, descriptor.is_some());
        let column = entry.position.x.saturating_sub(min_x).min(1000);
        let row = entry.position.y.saturating_sub(min_y).min(1000);
        if !local {
            let drag = gtk::GestureDrag::new();
            let data = data.clone();
            let status = status.downgrade();
            let grid_widget = grid_widget.downgrade();
            let request = request.clone();
            let owner = entry.owner_fingerprint.clone();
            let monitor_id = entry.monitor_id.clone();
            let start = entry.position;
            drag.connect_drag_end(move |_, offset_x, offset_y| {
                let target = GridPosition {
                    x: start.x.saturating_add((offset_x / 182.0).round() as i32),
                    y: start.y.saturating_add((offset_y / 124.0).round() as i32),
                };
                if target == start {
                    return;
                }
                let result = data
                    .borrow_mut()
                    .grid
                    .as_mut()
                    .ok_or(GridEditorMoveError::UnknownMonitor)
                    .and_then(|model| {
                        model.move_remote_monitor(&owner, &monitor_id, target)?;
                        Ok(model.grid().clone())
                    });
                match result {
                    Ok(snapshot) => {
                        if let Some(status) = status.upgrade() {
                            status.set_text("Saving and synchronizing layout…");
                        }
                        request(FrontendRequest::UpdateMonitorGrid(snapshot));
                    }
                    Err(error) => {
                        if let Some(status) = status.upgrade() {
                            status.set_text(&format!("Cannot move display: {error}"));
                        }
                    }
                }
                let grid_widget = grid_widget.clone();
                let status = status.clone();
                let data = data.clone();
                let request = request.clone();
                glib::idle_add_local_once(move || {
                    if let (Some(grid_widget), Some(status)) =
                        (grid_widget.upgrade(), status.upgrade())
                    {
                        render_grid(&grid_widget, &status, &data, &request)
                    }
                });
            });
            tile.add_controller(drag);
        }
        grid_widget.attach(&tile, column, row, 1, 1);
    }
}

fn display_tile(label: &str, subtitle: &str, local: bool, available: bool) -> gtk::Button {
    let content = gtk::Box::new(gtk::Orientation::Vertical, 6);
    content.set_margin_top(12);
    content.set_margin_bottom(12);
    content.set_margin_start(12);
    content.set_margin_end(12);
    let title = gtk::Label::new(Some(label));
    title.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    title.set_max_width_chars(20);
    title.add_css_class("heading");
    let details = gtk::Label::new(Some(subtitle));
    details.add_css_class(if available { "dim-label" } else { "error" });
    content.append(&title);
    content.append(&details);
    let button = gtk::Button::new();
    button.set_child(Some(&content));
    button.set_size_request(170, 112);
    button.set_sensitive(!local);
    button.add_css_class("card");
    if local {
        button.add_css_class("monitor-local");
    } else {
        button.add_css_class("monitor-remote");
    }
    button
}

#[cfg(test)]
mod tests {
    use super::*;
    use input_event::MonitorId;

    #[cfg(windows)]
    #[test]
    fn monitor_grid_builder_loads_the_window_resource() {
        gtk::init().expect("GTK should initialize for the resource test");
        let builder = gtk::Builder::new();
        builder
            .add_from_string(include_str!("../resources/monitor_grid_window.ui"))
            .expect("monitor grid window resource should load");

        assert!(
            builder
                .object::<adw::Window>("monitor_grid_window")
                .is_some()
        );
    }

    fn entry(owner: &str, id: &str, x: i32, y: i32) -> MonitorGridEntry {
        MonitorGridEntry {
            owner_fingerprint: owner.to_owned(),
            monitor_id: MonitorId::new(id.to_owned()).unwrap(),
            position: GridPosition { x, y },
        }
    }

    #[test]
    fn moving_remote_tile_rejects_occupied_cells_and_keeps_unavailable_outputs() {
        let available = entry("peer", "DP-1", 1, 0);
        let unavailable = entry("peer", "HDMI-A-2", 2, 0);
        let local = entry("local", "DISPLAY1", 0, 0);
        let grid = MonitorGrid::new(
            4,
            "local".into(),
            vec![local.clone(), available.clone(), unavailable.clone()],
        )
        .unwrap();
        let mut model = GridEditorModel::new(grid, "local".into());

        assert_eq!(
            model.move_remote_monitor("peer", &available.monitor_id, GridPosition { x: 2, y: 0 }),
            Err(GridEditorMoveError::Occupied(GridPosition { x: 2, y: 0 }))
        );
        model
            .move_remote_monitor("peer", &available.monitor_id, GridPosition { x: 1, y: 1 })
            .unwrap();

        assert!(model.grid().entries.contains(&unavailable));
        assert!(model.grid().entries.contains(&local));
        assert_eq!(
            model.grid().entries[1].position,
            GridPosition { x: 1, y: 1 }
        );
    }
}
