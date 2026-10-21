use std::{
    collections::HashSet,
    fs,
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::{RemoteErrorKind, wire};

const MAX_ENTRIES: usize = 10_000_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EntryKind {
    File,
    Directory,
}

/// One file or directory of a clipboard offer, listed parents first.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Entry {
    /// Index into [`Manifest::roots`].
    pub root: u32,
    /// Path below the root; empty for the root itself.
    pub components: Vec<String>,
    pub kind: EntryKind,
    /// Size in bytes; 0 for directories.
    pub size: u64,
    pub modified: Option<SystemTime>,
}

/// Everything below the copied items, as it was when the manifest was built.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Manifest {
    /// Display name of every copied item, unique among each other.
    pub roots: Vec<String>,
    pub entries: Vec<Entry>,
}

impl Manifest {
    pub fn total_bytes(&self) -> u64 {
        self.entries.iter().map(|entry| entry.size).sum()
    }

    /// Display path of an entry, e.g. `Photos/2024/a.jpg`.
    pub fn display_path(&self, entry: &Entry) -> String {
        display_path(
            self.roots
                .get(entry.root as usize)
                .map(String::as_str)
                .unwrap_or_default(),
            &entry.components,
        )
    }

    /// Walks the copied items. Directories are listed before their content
    /// and siblings are sorted by name. Symbolic links to files are followed;
    /// links to directories below a copied folder are skipped so a link cycle
    /// cannot make the walk endless.
    pub(crate) fn build(roots: &[PathBuf], names: &[String]) -> Result<Self, RemoteError> {
        let mut entries = Vec::new();
        for (index, (root, name)) in roots.iter().zip(names).enumerate() {
            let index = index as u32;
            let metadata =
                fs::metadata(root).map_err(|error| RemoteError::io(&error, name.clone()))?;
            if metadata.is_dir() {
                entries.push(directory_entry(index, Vec::new(), &metadata));
                let mut components = Vec::new();
                walk(root, index, name, &mut components, &mut entries)?;
            } else if metadata.is_file() {
                entries.push(file_entry(index, Vec::new(), &metadata));
            } else {
                log::debug!("skipping copied item that is neither file nor directory: {root:?}");
            }
        }
        Ok(Self {
            roots: names.to_vec(),
            entries,
        })
    }

    pub(crate) fn write(&self, w: &mut impl Write) -> io::Result<()> {
        wire::write_u32(w, self.roots.len() as u32)?;
        for root in &self.roots {
            wire::write_str(w, root)?;
        }
        wire::write_u32(w, self.entries.len() as u32)?;
        for entry in &self.entries {
            wire::write_u32(w, entry.root)?;
            wire::write_components(w, &entry.components)?;
            wire::write_u8(
                w,
                match entry.kind {
                    EntryKind::File => 0,
                    EntryKind::Directory => 1,
                },
            )?;
            wire::write_u64(w, entry.size)?;
            match entry.modified.and_then(unix_time) {
                Some((secs, nanos)) => {
                    wire::write_u8(w, 1)?;
                    wire::write_i64(w, secs)?;
                    wire::write_u32(w, nanos)?;
                }
                None => wire::write_u8(w, 0)?,
            }
        }
        Ok(())
    }

    pub(crate) fn read(r: &mut impl Read) -> io::Result<Self> {
        let root_count = wire::read_u32(r)? as usize;
        if root_count > MAX_ENTRIES {
            return Err(wire::invalid("too many roots"));
        }
        let roots = (0..root_count)
            .map(|_| wire::read_str(r))
            .collect::<io::Result<Vec<_>>>()?;
        let entry_count = wire::read_u32(r)? as usize;
        if entry_count > MAX_ENTRIES {
            return Err(wire::invalid("too many entries"));
        }
        let mut entries = Vec::with_capacity(entry_count.min(65_536));
        for _ in 0..entry_count {
            let root = wire::read_u32(r)?;
            if root as usize >= roots.len() {
                return Err(wire::invalid("entry refers to an unknown root"));
            }
            let components = wire::read_components(r)?;
            if components
                .iter()
                .any(|component| !is_valid_component(component))
            {
                return Err(wire::invalid("invalid path component"));
            }
            let kind = match wire::read_u8(r)? {
                0 => EntryKind::File,
                1 => EntryKind::Directory,
                _ => return Err(wire::invalid("unknown entry kind")),
            };
            let size = wire::read_u64(r)?;
            let modified = match wire::read_u8(r)? {
                0 => None,
                _ => {
                    let secs = wire::read_i64(r)?;
                    let nanos = wire::read_u32(r)?;
                    system_time(secs, nanos)
                }
            };
            entries.push(Entry {
                root,
                components,
                kind,
                size,
                modified,
            });
        }
        Ok(Self { roots, entries })
    }
}

/// Error the server reports for one item.
#[derive(Debug)]
pub(crate) struct RemoteError {
    pub(crate) kind: RemoteErrorKind,
    pub(crate) path: String,
    pub(crate) detail: String,
}

impl RemoteError {
    pub(crate) fn new(kind: RemoteErrorKind, path: String, detail: &str) -> Self {
        Self {
            kind,
            path,
            detail: detail.to_owned(),
        }
    }

    pub(crate) fn io(error: &io::Error, path: String) -> Self {
        let kind = match error.kind() {
            io::ErrorKind::NotFound => RemoteErrorKind::NotFound,
            io::ErrorKind::PermissionDenied => RemoteErrorKind::PermissionDenied,
            _ => RemoteErrorKind::Io,
        };
        Self::new(kind, path, &error.to_string())
    }
}

/// Unique display names for the copied items. Items from different folders
/// may share a name, so later duplicates get a ` (2)` style suffix.
pub(crate) fn root_names(roots: &[PathBuf]) -> Vec<String> {
    let mut used = HashSet::new();
    roots
        .iter()
        .map(|root| {
            let base = root_name(root);
            let mut candidate = base.clone();
            let mut suffix = 2;
            while !used.insert(candidate.to_lowercase()) {
                candidate = format!("{base} ({suffix})");
                suffix += 1;
            }
            candidate
        })
        .collect()
}

fn root_name(root: &Path) -> String {
    if let Some(name) = root.file_name() {
        return name.to_string_lossy().into_owned();
    }
    // A drive or filesystem root has no file name.
    let name = root
        .components()
        .filter_map(|component| match component {
            Component::Prefix(prefix) => Some(prefix.as_os_str().to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<String>()
        .replace(':', "");
    if name.is_empty() { "raiz".into() } else { name }
}

/// Maps a requested entry back to a path below one of the copied items.
/// Directories on the way must be real directories, not links, which keeps
/// requests inside what the manifest could have listed.
pub(crate) fn resolve(
    roots: &[PathBuf],
    names: &[String],
    root: u32,
    components: &[String],
) -> Result<PathBuf, RemoteError> {
    let display = display_path(
        names
            .get(root as usize)
            .map(String::as_str)
            .unwrap_or_default(),
        components,
    );
    let Some(base) = roots.get(root as usize) else {
        return Err(RemoteError::new(
            RemoteErrorKind::InvalidRequest,
            display,
            "unknown root",
        ));
    };
    if components.iter().any(|component| {
        !is_valid_component(component) || (cfg!(windows) && component.contains(':'))
    }) {
        return Err(RemoteError::new(
            RemoteErrorKind::InvalidRequest,
            display,
            "invalid path",
        ));
    }
    let mut path = base.clone();
    for (index, component) in components.iter().enumerate() {
        if index > 0 {
            let metadata = fs::symlink_metadata(&path)
                .map_err(|error| RemoteError::io(&error, display.clone()))?;
            if !metadata.is_dir() {
                return Err(RemoteError::new(
                    RemoteErrorKind::NotFound,
                    display,
                    "not a directory",
                ));
            }
        }
        path.push(component);
    }
    Ok(path)
}

pub(crate) fn display_path(root: &str, components: &[String]) -> String {
    std::iter::once(root)
        .chain(components.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join("/")
}

pub(crate) fn is_valid_component(component: &str) -> bool {
    !component.is_empty()
        && component != "."
        && component != ".."
        && !component.contains(['/', '\\', '\0'])
}

fn walk(
    dir: &Path,
    root: u32,
    root_name: &str,
    components: &mut Vec<String>,
    entries: &mut Vec<Entry>,
) -> Result<(), RemoteError> {
    let mut children = fs::read_dir(dir)
        .and_then(|children| children.collect::<io::Result<Vec<_>>>())
        .map_err(|error| RemoteError::io(&error, display_path(root_name, components)))?;
    children.sort_by_key(|child| child.file_name());
    for child in children {
        let name = child.file_name().into_string().map_err(|name| {
            let mut path = display_path(root_name, components);
            path.push('/');
            path.push_str(&name.to_string_lossy());
            RemoteError::new(
                RemoteErrorKind::UnsupportedName,
                path,
                "name is not valid Unicode",
            )
        })?;
        components.push(name);
        let path = child.path();
        let io_error = |error: io::Error, components: &[String]| {
            RemoteError::io(&error, display_path(root_name, components))
        };
        let file_type = child
            .file_type()
            .map_err(|error| io_error(error, components))?;
        if file_type.is_symlink() {
            match fs::metadata(&path) {
                Ok(metadata) if metadata.is_file() => {
                    entries.push(file_entry(root, components.clone(), &metadata));
                }
                _ => log::debug!("skipping link to a directory or a missing target: {path:?}"),
            }
        } else if file_type.is_dir() {
            let metadata = child
                .metadata()
                .map_err(|error| io_error(error, components))?;
            entries.push(directory_entry(root, components.clone(), &metadata));
            walk(&path, root, root_name, components, entries)?;
        } else if file_type.is_file() {
            let metadata = child
                .metadata()
                .map_err(|error| io_error(error, components))?;
            entries.push(file_entry(root, components.clone(), &metadata));
        } else {
            log::debug!("skipping special file {path:?}");
        }
        components.pop();
    }
    Ok(())
}

fn file_entry(root: u32, components: Vec<String>, metadata: &fs::Metadata) -> Entry {
    Entry {
        root,
        components,
        kind: EntryKind::File,
        size: metadata.len(),
        modified: metadata.modified().ok(),
    }
}

fn directory_entry(root: u32, components: Vec<String>, metadata: &fs::Metadata) -> Entry {
    Entry {
        root,
        components,
        kind: EntryKind::Directory,
        size: 0,
        modified: metadata.modified().ok(),
    }
}

fn unix_time(time: SystemTime) -> Option<(i64, u32)> {
    match time.duration_since(UNIX_EPOCH) {
        Ok(after) => Some((i64::try_from(after.as_secs()).ok()?, after.subsec_nanos())),
        Err(before) => {
            let before = before.duration();
            let secs = i64::try_from(before.as_secs()).ok()?;
            match before.subsec_nanos() {
                0 => Some((-secs, 0)),
                nanos => Some((-secs - 1, 1_000_000_000 - nanos)),
            }
        }
    }
}

fn system_time(secs: i64, nanos: u32) -> Option<SystemTime> {
    if nanos >= 1_000_000_000 {
        return None;
    }
    if secs >= 0 {
        UNIX_EPOCH.checked_add(Duration::new(secs as u64, nanos))
    } else {
        UNIX_EPOCH
            .checked_sub(Duration::from_secs(secs.unsigned_abs()))?
            .checked_add(Duration::from_nanos(nanos.into()))
    }
}
