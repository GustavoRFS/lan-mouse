//! Platform independent parts of pasting files offered by a peer.

use std::{
    collections::{HashMap, HashSet},
    io,
    path::{Path, PathBuf},
};

use lan_mouse_ipc::RemoteFileOffer;
use lan_mouse_transfer::{Endpoint, Manifest, RemoteErrorKind, TransferError};

/// Marks clipboard content that lan-mouse published for a peer's offer, so
/// the clipboard watchers do not mistake it for files copied on this device.
pub(crate) const OFFER_MARKER_MIME: &str = "application/x-lan-mouse-file-offer";

pub(crate) fn endpoint(offer: &RemoteFileOffer) -> Endpoint {
    Endpoint {
        addr: offer.addr,
        fingerprint: offer.fingerprint.clone(),
        token: offer.token,
    }
}

/// "3 itens", "\"relatório.pdf\"" and similar, for notifications.
#[cfg_attr(windows, allow(dead_code))]
pub(crate) fn describe_items(offer: &RemoteFileOffer) -> String {
    match (offer.item_count, offer.names.first()) {
        (1, Some(name)) => format!("\"{name}\""),
        (count, _) => format!("{count} itens"),
    }
}

/// Relative local path for every manifest entry, in manifest order. Names
/// are made valid for this system and unique within their directory.
pub(crate) fn local_paths(manifest: &Manifest) -> Vec<PathBuf> {
    let mut directories = HashMap::<(u32, &[String]), PathBuf>::new();
    let mut used = HashMap::<PathBuf, HashSet<String>>::new();
    let mut paths = Vec::with_capacity(manifest.entries.len());
    for (index, entry) in manifest.entries.iter().enumerate() {
        let (parent, name) = match entry.components.split_last() {
            None => (
                PathBuf::new(),
                manifest
                    .roots
                    .get(entry.root as usize)
                    .map(String::as_str)
                    .unwrap_or_default(),
            ),
            Some((name, parents)) => {
                let parent = directories
                    .get(&(entry.root, parents))
                    .cloned()
                    .unwrap_or_else(|| {
                        parents
                            .iter()
                            .enumerate()
                            .map(|(i, name)| sanitize_component(name, i))
                            .collect()
                    });
                (parent, name.as_str())
            }
        };
        let name = unique_name(
            &sanitize_component(name, index),
            used.entry(parent.clone()).or_default(),
        );
        let path = parent.join(name);
        if entry.kind == lan_mouse_transfer::EntryKind::Directory {
            directories.insert((entry.root, entry.components.as_slice()), path.clone());
        }
        paths.push(path);
    }
    paths
}

/// Makes a name from another system valid here: on Windows reserved
/// characters and device names are replaced, everywhere path separators.
fn sanitize_component(name: &str, index: usize) -> String {
    let mut clean = name
        .chars()
        .map(|character| {
            let reserved = if cfg!(windows) {
                character.is_control() || "<>:\"/\\|?*".contains(character)
            } else {
                character == '/' || character == '\0'
            };
            if reserved { '_' } else { character }
        })
        .collect::<String>();
    if cfg!(windows) {
        clean = clean.trim_end_matches([' ', '.']).to_owned();
    }
    while clean.len() > 200 {
        clean.pop();
    }
    let stem = clean
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    let reserved_device = cfg!(windows)
        && matches!(
            stem.as_str(),
            "CON"
                | "PRN"
                | "AUX"
                | "NUL"
                | "COM1"
                | "COM2"
                | "COM3"
                | "COM4"
                | "COM5"
                | "COM6"
                | "COM7"
                | "COM8"
                | "COM9"
                | "LPT1"
                | "LPT2"
                | "LPT3"
                | "LPT4"
                | "LPT5"
                | "LPT6"
                | "LPT7"
                | "LPT8"
                | "LPT9"
        );
    if clean.is_empty() || clean == "." || clean == ".." || reserved_device {
        clean = format!("arquivo-{}", index + 1);
    }
    clean
}

fn unique_name(name: &str, used: &mut HashSet<String>) -> String {
    // Windows file names are case-insensitive
    let key = |name: &str| {
        if cfg!(windows) {
            name.to_lowercase()
        } else {
            name.to_owned()
        }
    };
    let (stem, extension) = match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => (stem, format!(".{extension}")),
        _ => (name, String::new()),
    };
    let mut candidate = name.to_owned();
    let mut suffix = 2;
    while !used.insert(key(&candidate)) {
        candidate = format!("{stem} ({suffix}){extension}");
        suffix += 1;
    }
    candidate
}

/// Explains a failed transfer from `host` in Portuguese.
pub(crate) fn describe_error(error: &TransferError, host: &str) -> String {
    match error {
        TransferError::Connect(error) => match error.kind() {
            io::ErrorKind::ConnectionRefused => format!(
                "{host} recusou a conexão. Verifique se o Lan Mouse está aberto lá e na mesma \
                 versão deste computador."
            ),
            io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => format!(
                "{host} não respondeu. Verifique se o firewall de lá libera a porta TCP do Lan \
                 Mouse (4242 por padrão)."
            ),
            io::ErrorKind::HostUnreachable | io::ErrorKind::NetworkUnreachable => {
                format!("{host} não está acessível pela rede.")
            }
            _ => format!("Não foi possível conectar a {host}: {error}."),
        },
        TransferError::IdentityMismatch => format!(
            "A identidade de {host} não confere com a conexão autorizada. A cópia foi cancelada \
             por segurança."
        ),
        TransferError::Tls(error) => format!("Falha na conexão segura com {host} ({error})."),
        TransferError::Timeout => format!("{host} parou de responder durante a cópia."),
        TransferError::ConnectionLost(_) => {
            format!("A conexão com {host} caiu durante a cópia.")
        }
        TransferError::Protocol(_) => format!(
            "{host} respondeu de forma inesperada. Verifique se os dois computadores estão com a \
             mesma versão do Lan Mouse."
        ),
        TransferError::Remote { kind, path, detail } => match kind {
            RemoteErrorKind::InvalidToken => format!(
                "Os arquivos copiados em {host} não estão mais disponíveis, porque outra coisa \
                 foi copiada ou o Lan Mouse reiniciou lá. Copie-os de novo."
            ),
            RemoteErrorKind::UnsupportedVersion => format!(
                "O Lan Mouse em {host} está em uma versão incompatível. Atualize os dois \
                 computadores."
            ),
            RemoteErrorKind::NotFound => format!("\"{path}\" não existe mais em {host}."),
            RemoteErrorKind::PermissionDenied => {
                format!("Sem permissão para ler \"{path}\" em {host}.")
            }
            RemoteErrorKind::Changed => {
                format!("\"{path}\" foi alterado em {host} durante a cópia. Tente colar de novo.")
            }
            RemoteErrorKind::UnsupportedName => format!(
                "O nome de \"{path}\" em {host} tem caracteres que não podem ser transferidos."
            ),
            RemoteErrorKind::InvalidRequest | RemoteErrorKind::Io => {
                format!("Erro ao ler \"{path}\" em {host}: {detail}.")
            }
        },
    }
}

/// Explains a failure to write received files in Portuguese.
#[cfg_attr(windows, allow(dead_code))]
pub(crate) fn describe_local_error(error: &io::Error, path: &Path) -> String {
    match error.kind() {
        io::ErrorKind::StorageFull | io::ErrorKind::QuotaExceeded => {
            "Não há espaço em disco suficiente para receber os arquivos.".to_owned()
        }
        io::ErrorKind::PermissionDenied => {
            format!("Sem permissão para gravar \"{}\".", path.display())
        }
        _ => format!("Erro ao gravar \"{}\": {error}.", path.display()),
    }
}

#[cfg(test)]
mod tests {
    use lan_mouse_transfer::{Entry, EntryKind};

    use super::*;

    fn entry(root: u32, components: &[&str], kind: EntryKind) -> Entry {
        Entry {
            root,
            components: components.iter().map(|c| c.to_string()).collect(),
            kind,
            size: 0,
            modified: None,
        }
    }

    #[test]
    fn local_paths_follow_renamed_parents() {
        let manifest = Manifest {
            roots: vec!["dir".into(), "a.txt".into()],
            entries: vec![
                entry(0, &[], EntryKind::Directory),
                entry(0, &["sub/x"], EntryKind::Directory),
                entry(0, &["sub/x", "f.txt"], EntryKind::File),
                entry(1, &[], EntryKind::File),
            ],
        };
        let paths = local_paths(&manifest);
        assert_eq!(paths[0], Path::new("dir"));
        assert_eq!(paths[1], Path::new("dir").join("sub_x"));
        assert_eq!(paths[2], Path::new("dir").join("sub_x").join("f.txt"));
        assert_eq!(paths[3], Path::new("a.txt"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_names_are_made_valid_and_unique() {
        let manifest = Manifest {
            roots: vec!["d".into()],
            entries: vec![
                entry(0, &[], EntryKind::Directory),
                entry(0, &["Nota.txt"], EntryKind::File),
                entry(0, &["nota.txt"], EntryKind::File),
                entry(0, &["12:30?.txt"], EntryKind::File),
                entry(0, &["con.txt"], EntryKind::File),
            ],
        };
        let names = local_paths(&manifest)
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                "d",
                "d\\Nota.txt",
                "d\\nota (2).txt",
                "d\\12_30_.txt",
                "d\\arquivo-5"
            ]
        );
    }
}
