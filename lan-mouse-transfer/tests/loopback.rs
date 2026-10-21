use std::{
    fs,
    io::Read,
    net::{SocketAddr, TcpListener},
    path::{Path, PathBuf},
    sync::Arc,
    thread,
};

use lan_mouse_transfer::{
    Connection, Endpoint, EntryKind, Offer, OfferStore, Pool, RemoteErrorKind, TransferError,
    fingerprint, new_token, serve_connection, server_config,
};

struct Peer {
    addr: SocketAddr,
    fingerprint: String,
    store: OfferStore,
}

fn start_peer() -> Peer {
    let certified = rcgen::generate_simple_self_signed(vec!["lan-mouse".into()]).unwrap();
    let certificate = certified.cert.der().clone();
    let fingerprint = fingerprint(&certificate);
    let config = server_config(vec![certificate], certified.key_pair.serialize_der()).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let store = OfferStore::default();
    let served = store.clone();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let stream = stream.unwrap();
            let config = config.clone();
            let store = served.clone();
            thread::spawn(move || {
                let _ = serve_connection(stream, config, &store);
            });
        }
    });
    Peer {
        addr,
        fingerprint,
        store,
    }
}

impl Peer {
    fn offer(&self, roots: Vec<PathBuf>) -> Endpoint {
        let token = new_token();
        self.store.replace(Some(Offer::new(token, roots)));
        Endpoint {
            addr: self.addr,
            fingerprint: self.fingerprint.clone(),
            token,
        }
    }
}

/// `Projeto/{a.txt, sub/b.bin, vazio/}` plus a loose `solto.txt`.
fn sample_tree(dir: &Path) -> (PathBuf, PathBuf, Vec<u8>) {
    let project = dir.join("Projeto");
    fs::create_dir_all(project.join("sub")).unwrap();
    fs::create_dir_all(project.join("vazio")).unwrap();
    fs::write(project.join("a.txt"), b"conteudo a").unwrap();
    let big = (0..3 * 1024 * 1024 + 17)
        .map(|i: u32| (i.wrapping_mul(2_654_435_761) >> 24) as u8)
        .collect::<Vec<_>>();
    fs::write(project.join("sub").join("b.bin"), &big).unwrap();
    let loose = dir.join("solto.txt");
    fs::write(&loose, b"solto").unwrap();
    (project, loose, big)
}

fn read_all(
    connection: Connection,
    root: u32,
    components: &[&str],
    offset: u64,
) -> (Vec<u8>, Option<Connection>) {
    let components = components.iter().map(|c| c.to_string()).collect::<Vec<_>>();
    let mut stream = connection.file(root, &components, offset).unwrap();
    let mut content = Vec::new();
    stream.read_to_end(&mut content).unwrap();
    (content, stream.into_connection())
}

#[test]
fn manifest_lists_parents_first_and_contents_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let (project, loose, big) = sample_tree(dir.path());
    let peer = start_peer();
    let endpoint = peer.offer(vec![project, loose]);

    let mut connection = Connection::open(&endpoint).unwrap();
    let manifest = connection.manifest().unwrap();
    assert_eq!(manifest.roots, vec!["Projeto", "solto.txt"]);
    let listed = manifest
        .entries
        .iter()
        .map(|entry| (manifest.display_path(entry), entry.kind, entry.size))
        .collect::<Vec<_>>();
    assert_eq!(
        listed,
        vec![
            ("Projeto".to_string(), EntryKind::Directory, 0),
            ("Projeto/a.txt".to_string(), EntryKind::File, 10),
            ("Projeto/sub".to_string(), EntryKind::Directory, 0),
            (
                "Projeto/sub/b.bin".to_string(),
                EntryKind::File,
                big.len() as u64
            ),
            ("Projeto/vazio".to_string(), EntryKind::Directory, 0),
            ("solto.txt".to_string(), EntryKind::File, 5),
        ]
    );
    assert!(
        manifest
            .entries
            .iter()
            .all(|entry| entry.modified.is_some())
    );

    // one connection serves several files in a row
    let (content, connection) = read_all(connection, 0, &["sub", "b.bin"], 0);
    assert_eq!(content, big);
    let (content, connection) = read_all(connection.unwrap(), 0, &["a.txt"], 0);
    assert_eq!(content, b"conteudo a");
    let (content, connection) = read_all(connection.unwrap(), 1, &[], 0);
    assert_eq!(content, b"solto");
    let (content, _) = read_all(connection.unwrap(), 0, &["sub", "b.bin"], 1_000_000);
    assert_eq!(content, big[1_000_000..]);
}

#[test]
fn pool_reuses_connections() {
    let dir = tempfile::tempdir().unwrap();
    let (project, _, _) = sample_tree(dir.path());
    let peer = start_peer();
    let endpoint = peer.offer(vec![project]);
    let pool = Pool::default();
    let manifest = pool.manifest(&endpoint).unwrap();
    assert_eq!(manifest.entries.len(), 5);
    for _ in 0..3 {
        let mut stream = pool.file(&endpoint, 0, &["a.txt".into()], 0).unwrap();
        let mut content = Vec::new();
        stream.read_to_end(&mut content).unwrap();
        assert_eq!(content, b"conteudo a");
        pool.put(stream.into_connection().expect("connection is reusable"));
    }
}

#[test]
fn wrong_token_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let (project, _, _) = sample_tree(dir.path());
    let peer = start_peer();
    let mut endpoint = peer.offer(vec![project]);
    endpoint.token[0] ^= 1;
    let error = Connection::open(&endpoint).err().unwrap();
    assert!(
        matches!(
            error,
            TransferError::Remote {
                kind: RemoteErrorKind::InvalidToken,
                ..
            }
        ),
        "{error:?}"
    );
}

#[test]
fn unexpected_certificate_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let (project, _, _) = sample_tree(dir.path());
    let peer = start_peer();
    let mut endpoint = peer.offer(vec![project]);
    endpoint.fingerprint = "00:".repeat(31) + "00";
    let error = Connection::open(&endpoint).err().unwrap();
    assert!(
        matches!(error, TransferError::IdentityMismatch),
        "{error:?}"
    );
}

#[test]
fn newer_copy_withdraws_the_offer() {
    let dir = tempfile::tempdir().unwrap();
    let (project, loose, _) = sample_tree(dir.path());
    let peer = start_peer();
    let endpoint = peer.offer(vec![project]);
    let mut connection = Connection::open(&endpoint).unwrap();
    peer.offer(vec![loose]);
    let error = connection.manifest().unwrap_err();
    assert!(
        matches!(
            error,
            TransferError::Remote {
                kind: RemoteErrorKind::InvalidToken,
                ..
            }
        ),
        "{error:?}"
    );
}

#[test]
fn missing_file_is_reported_by_path() {
    let dir = tempfile::tempdir().unwrap();
    let (project, _, _) = sample_tree(dir.path());
    let peer = start_peer();
    let endpoint = peer.offer(vec![project.clone()]);
    let mut connection = Connection::open(&endpoint).unwrap();
    connection.manifest().unwrap();
    fs::remove_file(project.join("a.txt")).unwrap();
    let error = connection.file(0, &["a.txt".into()], 0).err().unwrap();
    match error {
        TransferError::Remote { kind, path, .. } => {
            assert_eq!(kind, RemoteErrorKind::NotFound);
            assert_eq!(path, "Projeto/a.txt");
        }
        error => panic!("unexpected error {error:?}"),
    }
}

#[test]
fn requests_cannot_leave_the_copied_items() {
    let dir = tempfile::tempdir().unwrap();
    let (project, _, _) = sample_tree(dir.path());
    fs::write(dir.path().join("secret.txt"), b"no").unwrap();
    let peer = start_peer();
    let endpoint = peer.offer(vec![project]);
    for components in [
        vec![".."],
        vec!["..", "secret.txt"],
        vec!["sub/../../secret.txt"],
    ] {
        let components = components.iter().map(|c| c.to_string()).collect::<Vec<_>>();
        let connection = Connection::open(&endpoint).unwrap();
        let error = connection.file(0, &components, 0).err().unwrap();
        assert!(
            matches!(
                error,
                TransferError::Remote {
                    kind: RemoteErrorKind::InvalidRequest,
                    ..
                }
            ),
            "{components:?}: {error:?}"
        );
    }
}

#[test]
fn duplicate_root_names_are_made_unique() {
    let dir = tempfile::tempdir().unwrap();
    for folder in ["a", "b"] {
        fs::create_dir_all(dir.path().join(folder)).unwrap();
        fs::write(dir.path().join(folder).join("x.txt"), folder).unwrap();
    }
    let peer = start_peer();
    let endpoint = peer.offer(vec![
        dir.path().join("a").join("x.txt"),
        dir.path().join("b").join("x.txt"),
    ]);
    let manifest = Arc::new(Connection::open(&endpoint).unwrap().manifest().unwrap());
    assert_eq!(manifest.roots, vec!["x.txt", "x.txt (2)"]);
}
