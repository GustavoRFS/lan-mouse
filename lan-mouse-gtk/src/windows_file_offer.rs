//! Pasting a peer's files on Windows. The clipboard gets an OLE data object
//! with virtual files (`FileGroupDescriptorW` + `FileContents`): Explorer
//! reads every file through a stream while it shows its own progress dialog,
//! and nothing crosses the network unless the files are pasted.

use std::{
    cell::Cell,
    ffi::c_void,
    mem::{ManuallyDrop, size_of},
    os::windows::ffi::OsStrExt,
    ptr, slice,
    sync::{Arc, Mutex, OnceLock, mpsc},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use lan_mouse_ipc::RemoteFileOffer;
use lan_mouse_transfer::{
    Endpoint, Entry, EntryKind, FileStream, Manifest, Pool, RemoteErrorKind, TransferError,
};
use windows::{
    Win32::{
        Foundation::{
            DATA_S_SAMEFORMATETC, DV_E_FORMATETC, DV_E_LINDEX, DV_E_TYMED, E_FAIL, E_NOTIMPL,
            E_OUTOFMEMORY, E_POINTER, FILETIME, OLE_E_ADVISENOTSUPPORTED, S_OK, STG_E_ACCESSDENIED,
            STG_E_INVALIDFUNCTION,
        },
        System::{
            Com::{
                COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemAlloc, FORMATETC, IAdviseSink,
                IDataObject, IDataObject_Impl, IEnumFORMATETC, IEnumSTATDATA,
                ISequentialStream_Impl, IStream, IStream_Impl, LOCKTYPE,
                Marshal::CoMarshalInterThreadInterfaceInStream, STATFLAG, STATSTG, STGC, STGMEDIUM,
                STGMEDIUM_0, STREAM_SEEK, STREAM_SEEK_CUR, STREAM_SEEK_END, STREAM_SEEK_SET,
                StructuredStorage::CoGetInterfaceAndReleaseStream,
            },
            DataExchange::RegisterClipboardFormatW,
            Ole::{DROPEFFECT_COPY, OleInitialize, OleSetClipboard, ReleaseStgMedium},
        },
        UI::Shell::{
            CFSTR_FILECONTENTS, CFSTR_FILEDESCRIPTORW, CFSTR_PREFERREDDROPEFFECT,
            CFSTR_ZONEIDENTIFIER, FD_ATTRIBUTES, FD_FILESIZE, FD_PROGRESSUI, FD_UNICODE,
            FD_WRITESTIME, FILEDESCRIPTORW, SHCreateStdEnumFmtEtc,
        },
    },
    core::{BOOL, Error, HRESULT, Interface, PWSTR, Ref, Result, implement},
};

use crate::{
    file_offer, notify,
    windows_clipboard::{global_from_parts, struct_bytes},
};

const TYMED_HGLOBAL: u32 = 1;
const TYMED_ISTREAM: u32 = 4;
const DVASPECT_CONTENT: u32 = 1;
const DATADIR_GET: u32 = 1;
const STGTY_STREAM: u32 = 2;
const STATFLAG_NONAME: i32 = 1;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
/// Files from the network get the internet zone, so the shell applies the
/// same checks as for downloads (Mark-of-the-Web) to what Explorer writes.
const URLZONE_INTERNET: u32 = 3;
/// `FILEDESCRIPTORW::cFileName` holds at most this many characters.
const MAX_DESCRIPTOR_PATH: usize = 259;
/// A paste asks for the file list more than once; refetching it every time
/// would walk the copied folders on the peer again.
const MANIFEST_REUSE: Duration = Duration::from_secs(10);

/// Puts the peer's offer on the clipboard. Must run on the GTK thread, which
/// is a single-threaded COM apartment with a message loop.
pub(crate) fn publish(offer: &RemoteFileOffer) -> bool {
    let result = (|| -> Result<()> {
        ensure_ole()?;
        let object: IDataObject = FileOfferDataObject {
            context: Arc::new(OfferContext {
                host: offer.host.clone(),
                endpoint: file_offer::endpoint(offer),
                pool: Pool::default(),
            }),
            formats: Formats::register()?,
            manifest: Mutex::new(None),
        }
        .into();
        // GTK reads the clipboard on its own thread and may hold it open briefly
        let mut attempts = 0;
        loop {
            match unsafe { OleSetClipboard(&object) } {
                Ok(()) => return Ok(()),
                Err(_) if attempts < 5 => {
                    attempts += 1;
                    thread::sleep(Duration::from_millis(30));
                }
                Err(error) => return Err(error),
            }
        }
    })();
    match result {
        Ok(()) => {
            log::debug!(
                "published {} items offered by {}",
                offer.item_count,
                offer.host
            );
            true
        }
        Err(error) => {
            notify::transfer_failed(&format!(
                "Não foi possível colocar os arquivos de {} na área de transferência do Windows \
                 ({error}).",
                offer.host
            ));
            false
        }
    }
}

fn ensure_ole() -> Result<()> {
    thread_local! {
        static INITIALIZED: Cell<bool> = const { Cell::new(false) };
    }
    if !INITIALIZED.get() {
        // S_FALSE (already initialized, e.g. by GTK for drag and drop) is Ok
        unsafe { OleInitialize(None)? };
        INITIALIZED.set(true);
    }
    Ok(())
}

struct OfferContext {
    host: String,
    endpoint: Endpoint,
    pool: Pool,
}

impl OfferContext {
    fn report(&self, error: &TransferError) {
        notify::transfer_failed(&file_offer::describe_error(error, &self.host));
    }
}

#[derive(Clone, Copy)]
struct Formats {
    descriptor: u16,
    contents: u16,
    drop_effect: u16,
    zone: u16,
}

impl Formats {
    fn register() -> Result<Self> {
        let register = |name| match unsafe { RegisterClipboardFormatW(name) } {
            0 => Err(Error::from_win32()),
            format => Ok(format as u16),
        };
        Ok(Self {
            descriptor: register(CFSTR_FILEDESCRIPTORW)?,
            contents: register(CFSTR_FILECONTENTS)?,
            drop_effect: register(CFSTR_PREFERREDDROPEFFECT)?,
            zone: register(CFSTR_ZONEIDENTIFIER)?,
        })
    }

    fn tymed(&self, format: u16) -> Option<u32> {
        if format == self.contents {
            Some(TYMED_ISTREAM)
        } else if [self.descriptor, self.drop_effect, self.zone].contains(&format) {
            Some(TYMED_HGLOBAL)
        } else {
            None
        }
    }

    fn all(&self) -> [FORMATETC; 4] {
        let format = |format, tymed| FORMATETC {
            cfFormat: format,
            ptd: ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT,
            lindex: -1,
            tymed,
        };
        [
            format(self.descriptor, TYMED_HGLOBAL),
            format(self.contents, TYMED_ISTREAM),
            format(self.drop_effect, TYMED_HGLOBAL),
            format(self.zone, TYMED_HGLOBAL),
        ]
    }
}

/// The manifest plus the name Explorer gets for each entry.
struct LocalManifest {
    manifest: Manifest,
    /// Relative Windows path per entry, UTF-16 without terminator.
    names: Vec<Vec<u16>>,
}

impl LocalManifest {
    fn new(manifest: Manifest) -> std::result::Result<Self, String> {
        let names = file_offer::local_paths(&manifest)
            .iter()
            .map(|path| path.as_os_str().encode_wide().collect::<Vec<_>>())
            .collect::<Vec<_>>();
        if let Some(long) = names.iter().find(|name| name.len() > MAX_DESCRIPTOR_PATH) {
            return Err(format!(
                "O caminho \"{}\" passa do limite de {MAX_DESCRIPTOR_PATH} caracteres que o \
                 Windows aceita ao colar arquivos de outro computador. Copie uma pasta com \
                 caminhos mais curtos.",
                String::from_utf16_lossy(long)
            ));
        }
        Ok(Self { manifest, names })
    }

    /// `FILEGROUPDESCRIPTORW`: an item count followed by one descriptor per entry.
    fn descriptor(&self) -> Vec<u8> {
        let mut bytes =
            Vec::with_capacity(size_of::<u32>() + self.names.len() * size_of::<FILEDESCRIPTORW>());
        bytes.extend_from_slice(&(self.names.len() as u32).to_ne_bytes());
        for (entry, name) in self.manifest.entries.iter().zip(&self.names) {
            let mut descriptor = FILEDESCRIPTORW {
                dwFlags: (FD_ATTRIBUTES.0 | FD_FILESIZE.0 | FD_PROGRESSUI.0 | FD_UNICODE.0) as u32,
                dwFileAttributes: match entry.kind {
                    EntryKind::Directory => FILE_ATTRIBUTE_DIRECTORY,
                    EntryKind::File => FILE_ATTRIBUTE_NORMAL,
                },
                nFileSizeHigh: (entry.size >> 32) as u32,
                nFileSizeLow: entry.size as u32,
                ..Default::default()
            };
            if let Some(modified) = entry.modified.and_then(filetime) {
                descriptor.dwFlags |= FD_WRITESTIME.0 as u32;
                descriptor.ftLastWriteTime = modified;
            }
            // the struct is packed, so the name is assigned as a whole
            let mut file_name = [0u16; MAX_DESCRIPTOR_PATH + 1];
            file_name[..name.len()].copy_from_slice(name);
            descriptor.cFileName = file_name;
            bytes.extend_from_slice(struct_bytes(&descriptor));
        }
        bytes
    }
}

#[implement(IDataObject)]
struct FileOfferDataObject {
    context: Arc<OfferContext>,
    formats: Formats,
    manifest: Mutex<Option<(Arc<LocalManifest>, Instant)>>,
}

impl FileOfferDataObject {
    /// The file list, fetched from the peer unless a recent one may be reused.
    /// Stream requests always reuse the list Explorer got, since their index
    /// refers to it.
    fn manifest(&self, refresh: bool) -> Result<Arc<LocalManifest>> {
        let mut cached = self.manifest.lock().expect("lock");
        if let Some((manifest, fetched)) = cached.as_ref() {
            if !refresh || fetched.elapsed() < MANIFEST_REUSE {
                return Ok(manifest.clone());
            }
        }
        let manifest = self
            .context
            .pool
            .manifest(&self.context.endpoint)
            .inspect_err(|error| self.context.report(error))
            .map_err(|_| Error::from(E_FAIL))?;
        log::debug!(
            "{} offers {} entries, {} bytes",
            self.context.host,
            manifest.entries.len(),
            manifest.total_bytes()
        );
        let manifest = Arc::new(
            LocalManifest::new(manifest)
                .inspect_err(|message| notify::transfer_failed(message))
                .map_err(|_| Error::from(E_FAIL))?,
        );
        *cached = Some((manifest.clone(), Instant::now()));
        Ok(manifest)
    }
}

impl IDataObject_Impl for FileOfferDataObject_Impl {
    fn GetData(&self, format: *const FORMATETC) -> Result<STGMEDIUM> {
        let format = unsafe { format.as_ref() }.ok_or(Error::from(E_POINTER))?;
        log::trace!(
            "GetData format {} index {} tymed {}",
            format.cfFormat,
            format.lindex,
            format.tymed
        );
        let tymed = self
            .formats
            .tymed(format.cfFormat)
            .ok_or(Error::from(DV_E_FORMATETC))?;
        if format.tymed & tymed == 0 {
            return Err(DV_E_TYMED.into());
        }
        if format.cfFormat == self.formats.descriptor {
            hglobal_medium(&self.manifest(true)?.descriptor())
        } else if format.cfFormat == self.formats.drop_effect {
            hglobal_medium(&DROPEFFECT_COPY.0.to_ne_bytes())
        } else if format.cfFormat == self.formats.zone {
            hglobal_medium(&URLZONE_INTERNET.to_ne_bytes())
        } else {
            let manifest = self.manifest(false)?;
            let index = usize::try_from(format.lindex).map_err(|_| Error::from(DV_E_LINDEX))?;
            let entry = manifest
                .manifest
                .entries
                .get(index)
                .filter(|entry| entry.kind == EntryKind::File)
                .ok_or(Error::from(DV_E_LINDEX))?;
            let stream = RemoteFileStream {
                context: self.context.clone(),
                entry: entry.clone(),
                display: manifest.manifest.display_path(entry),
                name: manifest.names[index].clone(),
                state: Mutex::new(StreamState::default()),
            };
            let stream = stream_in_mta(move || stream.into())?;
            Ok(STGMEDIUM {
                tymed: TYMED_ISTREAM,
                u: STGMEDIUM_0 {
                    pstm: ManuallyDrop::new(Some(stream)),
                },
                pUnkForRelease: ManuallyDrop::new(None),
            })
        }
    }

    fn GetDataHere(&self, _format: *const FORMATETC, _medium: *mut STGMEDIUM) -> Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn QueryGetData(&self, format: *const FORMATETC) -> HRESULT {
        let Some(format) = (unsafe { format.as_ref() }) else {
            return E_POINTER;
        };
        match self.formats.tymed(format.cfFormat) {
            None => DV_E_FORMATETC,
            Some(tymed) if format.tymed & tymed == 0 => DV_E_TYMED,
            Some(_) => S_OK,
        }
    }

    fn GetCanonicalFormatEtc(&self, input: *const FORMATETC, output: *mut FORMATETC) -> HRESULT {
        unsafe {
            if let (Some(input), Some(output)) = (input.as_ref(), output.as_mut()) {
                *output = *input;
                output.ptd = ptr::null_mut();
            }
        }
        DATA_S_SAMEFORMATETC
    }

    /// Explorer reports the outcome of a paste ("Performed DropEffect" and
    /// others) here; a copy needs nothing from it.
    fn SetData(
        &self,
        _format: *const FORMATETC,
        medium: *const STGMEDIUM,
        release: BOOL,
    ) -> Result<()> {
        if release.as_bool() && !medium.is_null() {
            unsafe { ReleaseStgMedium(medium.cast_mut()) };
        }
        Ok(())
    }

    fn EnumFormatEtc(&self, direction: u32) -> Result<IEnumFORMATETC> {
        if direction != DATADIR_GET {
            return Err(E_NOTIMPL.into());
        }
        unsafe { SHCreateStdEnumFmtEtc(&self.formats.all()) }
    }

    fn DAdvise(&self, _: *const FORMATETC, _: u32, _: Ref<'_, IAdviseSink>) -> Result<u32> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn DUnadvise(&self, _connection: u32) -> Result<()> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn EnumDAdvise(&self) -> Result<IEnumSTATDATA> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }
}

/// Content of one remote file, fetched from the peer as Explorer reads it.
#[implement(IStream)]
struct RemoteFileStream {
    context: Arc<OfferContext>,
    entry: Entry,
    /// Path on the peer, for error messages.
    display: String,
    /// Relative local path, for `Stat`.
    name: Vec<u16>,
    state: Mutex<StreamState>,
}

#[derive(Default)]
struct StreamState {
    position: u64,
    reader: Option<FileStream>,
}

impl RemoteFileStream {
    fn changed(&self) -> TransferError {
        TransferError::Remote {
            kind: RemoteErrorKind::Changed,
            path: self.display.clone(),
            detail: "size changed".into(),
        }
    }

    fn fill(
        &self,
        state: &mut StreamState,
        buf: &mut [u8],
        total: &mut usize,
    ) -> std::result::Result<(), TransferError> {
        while *total < buf.len() && state.position < self.entry.size {
            let reader = match state.reader.as_mut() {
                Some(reader) => reader,
                None => {
                    let reader = self.context.pool.file(
                        &self.context.endpoint,
                        self.entry.root,
                        &self.entry.components,
                        state.position,
                    )?;
                    if reader.size() != self.entry.size {
                        return Err(self.changed());
                    }
                    state.reader.insert(reader)
                }
            };
            let read = reader.read_chunk(&mut buf[*total..])?;
            if read == 0 {
                return Err(self.changed());
            }
            *total += read;
            state.position += read as u64;
        }
        if state.position >= self.entry.size {
            // read the end marker now so the connection can serve the next file
            if let Some(mut reader) = state.reader.take() {
                reader.finish()?;
                if let Some(connection) = reader.into_connection() {
                    self.context.pool.put(connection);
                }
            }
        }
        Ok(())
    }
}

impl ISequentialStream_Impl for RemoteFileStream_Impl {
    fn Read(&self, pv: *mut c_void, cb: u32, pcbread: *mut u32) -> HRESULT {
        if pv.is_null() {
            return E_POINTER;
        }
        let buf = unsafe { slice::from_raw_parts_mut(pv.cast::<u8>(), cb as usize) };
        let mut state = self.state.lock().expect("lock");
        let mut total = 0;
        let result = self.fill(&mut state, buf, &mut total);
        if !pcbread.is_null() {
            unsafe { *pcbread = total as u32 };
        }
        match result {
            Ok(()) => S_OK,
            Err(error) => {
                state.reader = None;
                self.context.report(&error);
                E_FAIL
            }
        }
    }

    fn Write(&self, _pv: *const c_void, _cb: u32, _pcbwritten: *mut u32) -> HRESULT {
        STG_E_ACCESSDENIED
    }
}

impl IStream_Impl for RemoteFileStream_Impl {
    fn Seek(&self, offset: i64, origin: STREAM_SEEK, new_position: *mut u64) -> Result<()> {
        let mut state = self.state.lock().expect("lock");
        let base = if origin == STREAM_SEEK_SET {
            0
        } else if origin == STREAM_SEEK_CUR {
            state.position as i128
        } else if origin == STREAM_SEEK_END {
            self.entry.size as i128
        } else {
            return Err(STG_E_INVALIDFUNCTION.into());
        };
        let target =
            u64::try_from(base + offset as i128).map_err(|_| Error::from(STG_E_INVALIDFUNCTION))?;
        if target != state.position {
            state.reader = None;
            state.position = target;
        }
        if !new_position.is_null() {
            unsafe { *new_position = target };
        }
        Ok(())
    }

    fn SetSize(&self, _size: u64) -> Result<()> {
        Err(STG_E_ACCESSDENIED.into())
    }

    fn CopyTo(
        &self,
        target: Ref<'_, IStream>,
        cb: u64,
        pcbread: *mut u64,
        pcbwritten: *mut u64,
    ) -> Result<()> {
        let target = target.ok()?;
        let mut buf = vec![0u8; 256 * 1024];
        let (mut read_total, mut written_total) = (0u64, 0u64);
        while read_total < cb {
            let want = (cb - read_total).min(buf.len() as u64) as u32;
            let mut read = 0;
            self.Read(buf.as_mut_ptr().cast(), want, &mut read).ok()?;
            if read == 0 {
                break;
            }
            read_total += read as u64;
            let mut written = 0;
            unsafe { target.Write(buf.as_ptr().cast(), read, Some(&mut written)) }.ok()?;
            written_total += written as u64;
        }
        unsafe {
            if !pcbread.is_null() {
                *pcbread = read_total;
            }
            if !pcbwritten.is_null() {
                *pcbwritten = written_total;
            }
        }
        Ok(())
    }

    fn Commit(&self, _flags: &STGC) -> Result<()> {
        Ok(())
    }

    fn Revert(&self) -> Result<()> {
        Ok(())
    }

    fn LockRegion(&self, _offset: u64, _cb: u64, _lock_type: &LOCKTYPE) -> Result<()> {
        Err(STG_E_INVALIDFUNCTION.into())
    }

    fn UnlockRegion(&self, _offset: u64, _cb: u64, _lock_type: u32) -> Result<()> {
        Err(STG_E_INVALIDFUNCTION.into())
    }

    fn Stat(&self, stat: *mut STATSTG, flags: &STATFLAG) -> Result<()> {
        let stat = unsafe { stat.as_mut() }.ok_or(Error::from(E_POINTER))?;
        *stat = STATSTG {
            r#type: STGTY_STREAM,
            cbSize: self.entry.size,
            mtime: self.entry.modified.and_then(filetime).unwrap_or_default(),
            ..Default::default()
        };
        if flags.0 & STATFLAG_NONAME == 0 {
            stat.pwcsName = co_task_string(&self.name)?;
        }
        Ok(())
    }

    fn Clone(&self) -> Result<IStream> {
        Err(E_NOTIMPL.into())
    }
}

fn hglobal_medium(bytes: &[u8]) -> Result<STGMEDIUM> {
    let memory = global_from_parts(&[bytes]).ok_or(Error::from(E_OUTOFMEMORY))?;
    Ok(STGMEDIUM {
        tymed: TYMED_HGLOBAL,
        u: STGMEDIUM_0 { hGlobal: memory },
        pUnkForRelease: ManuallyDrop::new(None),
    })
}

fn co_task_string(text: &[u16]) -> Result<PWSTR> {
    unsafe {
        let memory = CoTaskMemAlloc((text.len() + 1) * size_of::<u16>()).cast::<u16>();
        if memory.is_null() {
            return Err(E_OUTOFMEMORY.into());
        }
        ptr::copy_nonoverlapping(text.as_ptr(), memory, text.len());
        *memory.add(text.len()) = 0;
        Ok(PWSTR(memory))
    }
}

fn filetime(time: SystemTime) -> Option<FILETIME> {
    const UNIX_EPOCH_AS_FILETIME: u128 = 116_444_736_000_000_000;
    let ticks = time.duration_since(UNIX_EPOCH).ok()?.as_nanos() / 100 + UNIX_EPOCH_AS_FILETIME;
    let ticks = u64::try_from(ticks).ok()?;
    Some(FILETIME {
        dwLowDateTime: ticks as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    })
}

/// A marshaled interface pointer in a stream, safe to hand to another thread.
struct MarshaledStream(IStream);

unsafe impl Send for MarshaledStream {}

type Job = Box<dyn FnOnce() + Send>;

/// Creates a stream in the process' multithreaded apartment and returns a
/// proxy for the calling thread. Explorer's reads then arrive on COM worker
/// threads; a stream created on the GTK thread would be read on the GTK
/// thread and freeze the frontend for the whole copy.
fn stream_in_mta(create: impl FnOnce() -> IStream + Send + 'static) -> Result<IStream> {
    static JOBS: OnceLock<Mutex<mpsc::Sender<Job>>> = OnceLock::new();
    let jobs = JOBS.get_or_init(|| {
        let (sender, receiver) = mpsc::channel::<Job>();
        thread::Builder::new()
            .name("clipboard-mta".into())
            .spawn(move || {
                // the thread stays in the MTA, which keeps the apartment alive
                let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
                for job in receiver {
                    job();
                }
            })
            .expect("failed to spawn the clipboard COM thread");
        Mutex::new(sender)
    });
    let (sender, receiver) = mpsc::sync_channel(1);
    jobs.lock()
        .expect("lock")
        .send(Box::new(move || {
            let stream = create();
            let marshaled =
                unsafe { CoMarshalInterThreadInterfaceInStream(&IStream::IID, &stream) }
                    .map(MarshaledStream)
                    .map_err(|error| error.code());
            let _ = sender.send(marshaled);
        }))
        .map_err(|_| Error::from(E_FAIL))?;
    let MarshaledStream(marshaled) = receiver.recv().map_err(|_| Error::from(E_FAIL))??;
    // CoGetInterfaceAndReleaseStream releases the marshaling stream itself
    let marshaled = ManuallyDrop::new(marshaled);
    unsafe { CoGetInterfaceAndReleaseStream(&*marshaled) }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        process::Command,
    };

    use windows::Win32::{
        System::{Memory::GlobalLock, Ole::OleGetClipboard},
        UI::WindowsAndMessaging::{
            DispatchMessageW, MSG, PM_REMOVE, PeekMessageW, TranslateMessage,
        },
    };

    use super::*;

    /// Publishes files served by a local transfer server like a peer's offer
    /// and pastes them from another process, then compares them byte by byte.
    /// The paste is done by `read_clipboard_offer`, or by Explorer itself if
    /// `LAN_MOUSE_PASTE_WITH_EXPLORER` is set (this opens a folder window).
    /// It replaces the clipboard content, so it only runs on request:
    /// `cargo test -p lan-mouse-gtk -- --ignored --exact windows_file_offer::tests::pastes_virtual_files`
    #[test]
    #[ignore = "uses the desktop clipboard"]
    fn pastes_virtual_files() {
        let source = tempfile::tempdir().unwrap();
        let project = source.path().join("Projeto ç");
        fs::create_dir_all(project.join("sub").join("fundo")).unwrap();
        fs::create_dir_all(project.join("vazio")).unwrap();
        fs::write(project.join("relatório.txt"), "olá").unwrap();
        fs::write(project.join("sub").join("vazio.bin"), b"").unwrap();
        let big = (0..40 * 1024 * 1024 + 123u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8)
            .collect::<Vec<_>>();
        fs::write(project.join("sub").join("fundo").join("grande.bin"), &big).unwrap();
        let loose = source.path().join("solto.txt");
        fs::write(&loose, "solto").unwrap();

        let certified = rcgen::generate_simple_self_signed(vec!["lan-mouse".into()]).unwrap();
        let certificate = certified.cert.der().clone();
        let fingerprint = lan_mouse_transfer::fingerprint(&certificate);
        let config = lan_mouse_transfer::server_config(
            vec![certificate],
            certified.key_pair.serialize_der(),
        )
        .unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let token = lan_mouse_transfer::new_token();
        let store = lan_mouse_transfer::OfferStore::default();
        store.replace(Some(lan_mouse_transfer::Offer::new(
            token,
            vec![project, loose],
        )));
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (config, store) = (config.clone(), store.clone());
                thread::spawn(move || {
                    let _ = lan_mouse_transfer::serve_connection(stream, config, &store);
                });
            }
        });

        assert!(publish(&RemoteFileOffer {
            host: "127.0.0.1".into(),
            addr,
            fingerprint,
            token,
            item_count: 2,
            names: Vec::new(),
        }));

        let destination = tempfile::tempdir().unwrap();
        let done = destination.path().with_extension("done");
        let mut paste = if std::env::var_os("LAN_MOUSE_PASTE_WITH_EXPLORER").is_some() {
            // the verb runs inside explorer.exe, at medium integrity like a
            // user's Ctrl+V; the window closes once the test saw the files
            let script = format!(
                "$dest = '{}'; $done = '{}'; $shell = New-Object -ComObject Shell.Application; \
                 Start-Process explorer.exe $dest; $window = $null; \
                 for ($i = 0; $i -lt 100 -and -not $window; $i++) {{ \
                     Start-Sleep -Milliseconds 100; \
                     $window = $shell.Windows() | Where-Object {{ \
                         try {{ $_.Document.Folder.Self.Path -eq $dest }} catch {{ $false }} }} | \
                         Select-Object -First 1 }}; \
                 if (-not $window) {{ exit 1 }}; \
                 $window.Document.Folder.Self.InvokeVerb('paste'); \
                 for ($i = 0; $i -lt 1200 -and -not (Test-Path $done); $i++) {{ \
                     Start-Sleep -Milliseconds 100 }}; \
                 $window.Quit()",
                destination.path().display(),
                done.display()
            );
            Command::new("powershell.exe")
                .args(["-NoProfile", "-Command", &script])
                // inherited PowerShell 7 module paths break Windows PowerShell
                .env_remove("PSModulePath")
                .spawn()
                .unwrap()
        } else {
            Command::new(std::env::current_exe().unwrap())
                .args(["--ignored", "--exact", "--nocapture"])
                .arg("windows_file_offer::tests::read_clipboard_offer")
                .env("LAN_MOUSE_PASTE_INTO", destination.path())
                .spawn()
                .unwrap()
        };
        let started = Instant::now();
        let pasted = loop {
            // incoming COM calls are dispatched by this thread's message loop
            pump_messages();
            if same_tree(source.path(), destination.path()) {
                break true;
            }
            if started.elapsed() > Duration::from_secs(120) {
                break false;
            }
            thread::sleep(Duration::from_millis(50));
        };
        fs::write(&done, b"").unwrap();
        let waited = Instant::now();
        while paste.try_wait().unwrap().is_none() && waited.elapsed() < Duration::from_secs(10) {
            pump_messages();
            thread::sleep(Duration::from_millis(50));
        }
        let _ = paste.kill();
        let _ = fs::remove_file(&done);
        assert!(
            pasted,
            "pasted files differ from the source: {:?}",
            listing(destination.path())
        );
        eprintln!("pasted in {:?}", started.elapsed());
    }

    /// Pastes the clipboard offer into `LAN_MOUSE_PASTE_INTO` the way the
    /// shell reads virtual files. Runs as a separate process.
    #[test]
    #[ignore = "helper process of pastes_virtual_files"]
    fn read_clipboard_offer() {
        let Some(destination) = std::env::var_os("LAN_MOUSE_PASTE_INTO") else {
            return;
        };
        let destination = PathBuf::from(destination);
        unsafe { OleInitialize(None).unwrap() };
        let object = unsafe { OleGetClipboard() }.unwrap();
        let formats = Formats::register().unwrap();
        let mut format = FORMATETC {
            cfFormat: formats.descriptor,
            ptd: ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT,
            lindex: -1,
            tymed: TYMED_HGLOBAL,
        };
        let medium = unsafe { object.GetData(&format) }.unwrap();
        let descriptors = unsafe {
            let data = GlobalLock(medium.u.hGlobal) as *const u8;
            let count = (data as *const u32).read_unaligned() as usize;
            let first = data.add(size_of::<u32>()) as *const FILEDESCRIPTORW;
            (0..count)
                .map(|i| first.add(i).read_unaligned())
                .collect::<Vec<_>>()
        };
        for (index, descriptor) in descriptors.iter().enumerate() {
            let name = descriptor.cFileName;
            let len = name.iter().position(|c| *c == 0).unwrap();
            let path = destination.join(String::from_utf16_lossy(&name[..len]));
            if descriptor.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
                fs::create_dir_all(&path).unwrap();
                continue;
            }
            format.cfFormat = formats.contents;
            format.lindex = index as i32;
            format.tymed = TYMED_ISTREAM;
            let medium = unsafe { object.GetData(&format) }.unwrap();
            let stream = unsafe { medium.u.pstm.as_ref() }.unwrap().clone();
            let mut content = Vec::new();
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                let mut read = 0;
                unsafe { stream.Read(buf.as_mut_ptr().cast(), buf.len() as u32, Some(&mut read)) }
                    .ok()
                    .unwrap();
                if read == 0 {
                    break;
                }
                content.extend_from_slice(&buf[..read as usize]);
            }
            fs::write(&path, content).unwrap();
        }
    }

    fn pump_messages() {
        unsafe {
            let mut message = MSG::default();
            while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
    }

    fn same_tree(a: &Path, b: &Path) -> bool {
        let (Some(left), Some(right)) = (listing(a), listing(b)) else {
            return false;
        };
        left == right
            && left
                .iter()
                .filter(|(_, size)| size.is_some())
                .all(|(path, _)| fs::read(a.join(path)).ok() == fs::read(b.join(path)).ok())
    }

    /// Relative paths with their sizes (`None` for directories), sorted.
    fn listing(root: &Path) -> Option<Vec<(PathBuf, Option<u64>)>> {
        let mut entries = Vec::new();
        let mut pending = vec![root.to_owned()];
        while let Some(dir) = pending.pop() {
            for child in fs::read_dir(&dir).ok()? {
                let child = child.ok()?;
                let metadata = child.metadata().ok()?;
                let relative = child.path().strip_prefix(root).ok()?.to_owned();
                if metadata.is_dir() {
                    pending.push(child.path());
                    entries.push((relative, None));
                } else {
                    entries.push((relative, Some(metadata.len())));
                }
            }
        }
        entries.sort();
        Some(entries)
    }
}
