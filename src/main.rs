// Windows: build as a GUI binary so launching from Explorer or a shortcut does
// not pop a console window. `--debug` opens one on demand (see `init_console`).
#![cfg_attr(windows, windows_subsystem = "windows")]

use env_logger::Env;
use input_capture::InputCaptureError;
use input_emulation::InputEmulationError;
use lan_mouse::{
    capture_test,
    config::{self, Command, Config, ConfigError},
    emulation_test,
    service::{Service, ServiceError},
};
use lan_mouse_cli::CliError;
#[cfg(feature = "gtk")]
use lan_mouse_gtk::GtkError;
use lan_mouse_ipc::{IpcError, IpcListenerCreationError};
use std::{
    future::Future,
    io,
    path::Path,
    process::{self, Child},
};
use thiserror::Error;
use tokio::task::LocalSet;

#[derive(Debug, Error)]
enum LanMouseError {
    #[error(transparent)]
    Service(#[from] ServiceError),
    #[error(transparent)]
    IpcError(#[from] IpcError),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Capture(#[from] InputCaptureError),
    #[error(transparent)]
    Emulation(#[from] InputEmulationError),
    #[cfg(feature = "gtk")]
    #[error(transparent)]
    Gtk(#[from] GtkError),
    #[error(transparent)]
    Cli(#[from] CliError),
}

fn debug_requested() -> bool {
    std::env::args().any(|a| a == "--debug")
}

/// Give a windows-subsystem process somewhere to write its stdio: the terminal
/// it was launched from if there is one, a fresh console window when `--debug`
/// asks for it, and `NUL` otherwise so log writes are discarded rather than
/// failing against the null handles such a process starts with.
#[cfg(windows)]
fn init_console() {
    use windows::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows::Win32::System::Console::{
        ATTACH_PARENT_PROCESS, AllocConsole, AttachConsole, GetConsoleWindow, STD_ERROR_HANDLE,
        STD_OUTPUT_HANDLE, SetStdHandle,
    };
    use windows::core::w;

    unsafe {
        // The spawned daemon inherits the parent's console and handles already.
        if !GetConsoleWindow().0.is_null() {
            return;
        }

        let have_console = AttachConsole(ATTACH_PARENT_PROCESS).is_ok()
            || (debug_requested() && AllocConsole().is_ok());

        let target = if have_console {
            w!("CONOUT$")
        } else {
            w!("NUL")
        };
        if let Ok(handle) = CreateFileW(
            target,
            GENERIC_READ.0 | GENERIC_WRITE.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        ) {
            let _ = SetStdHandle(STD_OUTPUT_HANDLE, handle);
            let _ = SetStdHandle(STD_ERROR_HANDLE, handle);
        }
    }
}

fn main() {
    #[cfg(windows)]
    init_console();

    init_logging();

    if let Err(e) = run() {
        log::error!("{e}");
        process::exit(1);
    }
}

fn init_logging() {
    // init logging
    let default_level = if debug_requested() { "debug" } else { "info" };
    let env = Env::default().filter_or("LAN_MOUSE_LOG_LEVEL", default_level);
    let mut logger = env_logger::Builder::from_env(env);
    // LAN_MOUSE_LOG_FILE appends the log to a file, e.g. on Windows where the
    // GUI binary has no console unless `--debug` opens one. The daemon inherits
    // the variable and appends to the same file.
    if let Some(path) = std::env::var_os("LAN_MOUSE_LOG_FILE") {
        rotate_log_file(Path::new(&path));
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            Ok(file) => {
                logger.target(env_logger::Target::Pipe(Box::new(file)));
            }
            Err(e) => eprintln!("could not open log file {}: {e}", path.display()),
        }
    }
    logger.init();

    // The Windows GUI redirects stderr to NUL. The default panic hook alone
    // would discard the cause of an abort even when file logging is enabled.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log::error!(
            "{info}\nbacktrace:\n{}",
            std::backtrace::Backtrace::force_capture()
        );
        log::logger().flush();
        default_hook(info);
    }));
}

#[cfg(test)]
mod tests {
    #[test]
    fn panic_logging_child() {
        if std::env::var_os("LAN_MOUSE_PANIC_LOG_TEST").is_none() {
            return;
        }
        super::init_logging();
        panic!("lan-mouse regression panic");
    }

    #[test]
    fn panic_is_written_to_configured_log() {
        let path =
            std::env::temp_dir().join(format!("lan-mouse-panic-{}.log", uuid::Uuid::new_v4()));
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "tests::panic_logging_child", "--nocapture"])
            .env("LAN_MOUSE_PANIC_LOG_TEST", "1")
            .env("LAN_MOUSE_LOG_FILE", &path)
            .env("LAN_MOUSE_LOG_LEVEL", "error")
            .env("RUST_BACKTRACE", "0")
            .output()
            .unwrap();
        assert!(!output.status.success(), "child must actually panic");
        let log = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_file(path).unwrap();
        assert!(
            log.contains("lan-mouse regression panic"),
            "panic was lost: {log}"
        );
        assert!(
            log.contains("src/main.rs") || log.contains("src\\main.rs"),
            "panic location missing: {log}"
        );
        assert!(log.contains("backtrace:"), "backtrace missing: {log}");
    }
}

/// Keeps the log file bounded: once it exceeds this size it is moved to
/// `<name>.old` (replacing the previous one) when Lan Mouse starts. The daemon
/// started by the frontend finds the fresh file and leaves it alone.
const MAX_LOG_FILE_BYTES: u64 = 10 * 1024 * 1024;

fn rotate_log_file(path: &Path) {
    let Ok(metadata) = std::fs::metadata(path) else {
        return;
    };
    if metadata.len() > MAX_LOG_FILE_BYTES {
        let mut old = path.as_os_str().to_owned();
        old.push(".old");
        let _ = std::fs::rename(path, old);
    }
}

fn run() -> Result<(), LanMouseError> {
    let config = config::Config::new()?;
    match config.command() {
        Some(command) => match command {
            Command::TestEmulation(args) => run_async(emulation_test::run(config, args))?,
            Command::TestCapture(args) => run_async(capture_test::run(config, args))?,
            Command::Cli(cli_args) => run_async(lan_mouse_cli::run(cli_args))?,
            Command::Daemon => {
                // if daemon is specified we run the service
                match run_async(run_service(config)) {
                    Err(LanMouseError::Service(ServiceError::IpcListen(
                        IpcListenerCreationError::AlreadyRunning,
                    ))) => log::info!("service already running!"),
                    r => r?,
                }
            }
        },
        None => {
            //  otherwise start the service as a child process and
            //  run a frontend
            #[cfg(feature = "gtk")]
            {
                // Only spawn a new daemon if one isn't already running
                let mut service = if lan_mouse_ipc::is_service_running() {
                    log::info!("daemon already running, connecting to existing instance");
                    None
                } else {
                    Some(start_service()?)
                };
                let res = lan_mouse_gtk::run(config::local_commit());
                if let Some(ref mut service) = service {
                    #[cfg(unix)]
                    {
                        // on unix we give the service a chance to terminate gracefully
                        let pid = service.id() as libc::pid_t;
                        unsafe {
                            libc::kill(pid, libc::SIGINT);
                        }
                        service.wait()?;
                    }
                    service.kill()?;
                }
                res?;
            }
            #[cfg(not(feature = "gtk"))]
            {
                // run daemon if gtk is diabled
                match run_async(run_service(config)) {
                    Err(LanMouseError::Service(ServiceError::IpcListen(
                        IpcListenerCreationError::AlreadyRunning,
                    ))) => log::info!("service already running!"),
                    r => r?,
                }
            }
        }
    }

    Ok(())
}

fn run_async<F, E>(f: F) -> Result<(), LanMouseError>
where
    F: Future<Output = Result<(), E>>,
    LanMouseError: From<E>,
{
    // create single threaded tokio runtime
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?;

    // run async event loop
    Ok(runtime.block_on(LocalSet::new().run_until(f))?)
}

fn start_service() -> Result<Child, io::Error> {
    let child = process::Command::new(std::env::current_exe()?)
        .args(std::env::args().skip(1))
        .arg("daemon")
        .spawn()?;
    Ok(child)
}

async fn run_service(config: Config) -> Result<(), ServiceError> {
    let release_bind = config.release_bind();
    let config_path = config.config_path().to_owned();
    let mut service = Service::new(config).await?;
    log::info!("using config: {config_path:?}");
    log::info!("Press {release_bind:?} to release the mouse");
    service.run().await?;
    log::info!("service exited!");
    Ok(())
}
