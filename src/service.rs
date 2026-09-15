//! Windows service lifecycle (SCM integration).
//!
//! Written directly against the Win32 service APIs instead of using a helper
//! crate: the whole integration is roughly one hundred lines, and staying on
//! the `windows` crate keeps the dependency tree - and the memory footprint -
//! as small as possible.
//!
//! State transitions reported to the SCM:
//!
//! ```text
//! START_PENDING --(config + logger up)--> RUNNING
//! RUNNING --(SERVICE_CONTROL_STOP/SHUTDOWN)--> STOP_PENDING --(worker exit)--> STOPPED
//! ```
//!
//! **Stopping never hangs the broadband connection up.** The connection is
//! owned by the `RasMan` service, so it survives the service and is picked up
//! again ("already online") on the next start.

use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};
use std::time::Duration;

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Console::FreeConsole;
use windows::Win32::System::Services::{
    RegisterServiceCtrlHandlerExW, SERVICE_ACCEPT_SHUTDOWN, SERVICE_ACCEPT_STOP,
    SERVICE_CONTROL_INTERROGATE, SERVICE_CONTROL_SHUTDOWN, SERVICE_CONTROL_STOP, SERVICE_RUNNING,
    SERVICE_START_PENDING, SERVICE_STATUS, SERVICE_STATUS_CURRENT_STATE, SERVICE_STATUS_HANDLE,
    SERVICE_STOP_PENDING, SERVICE_STOPPED, SERVICE_TABLE_ENTRYW, SERVICE_WIN32_OWN_PROCESS,
    SetServiceStatus, StartServiceCtrlDispatcherW,
};
use windows::core::{PCWSTR, PWSTR};

use crate::config::{Config, config_arg};
// `config_arg` is re-exported through `Config` usage below; keep the import
// explicit so the SCM-supplied `--config` argument keeps working.
use crate::error::{Result, err};
use crate::logger::{self, Level, Settings, log_error, log_info};
use crate::worker;

/// SCM service name.
///
/// The SCM needs the name before it can read any configuration file, so it is a
/// compile time constant. `install` verifies that `[service] name` matches it
/// instead of silently creating a second, differently named service.
pub const SERVICE_NAME: &str = "PppoeDialer";

/// Log file base name.
const LOG_FILE_NAME: &str = "pppoe.log";

/// `ERROR_CALL_NOT_IMPLEMENTED`, returned for unsupported control codes.
const ERROR_CALL_NOT_IMPLEMENTED: u32 = 120;

const STATE_START_PENDING: u32 = SERVICE_START_PENDING.0;
const STATE_RUNNING: u32 = SERVICE_RUNNING.0;
const STATE_STOP_PENDING: u32 = SERVICE_STOP_PENDING.0;
const STATE_STOPPED: u32 = SERVICE_STOPPED.0;

/// Raw handles shared with the control handler callback.
static STOP_EVENT: AtomicIsize = AtomicIsize::new(0);
static STATUS_HANDLE: AtomicIsize = AtomicIsize::new(0);
static CURRENT_STATE: AtomicU32 = AtomicU32::new(STATE_START_PENDING);
static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Hand the process over to the SCM and block until the service has stopped.
pub fn run() -> Result<()> {
    // A console subsystem binary started as a service gets a hidden console in
    // session 0; release it so the service holds no unnecessary resources.
    unsafe {
        let _ = FreeConsole();
    }

    let mut name = crate::ras::to_wide(SERVICE_NAME);
    let table = [
        SERVICE_TABLE_ENTRYW {
            lpServiceName: PWSTR(name.as_mut_ptr()),
            lpServiceProc: Some(service_main),
        },
        SERVICE_TABLE_ENTRYW { lpServiceName: PWSTR(std::ptr::null_mut()), lpServiceProc: None },
    ];

    unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) }.map_err(|e| {
        err(format!(
            "StartServiceCtrlDispatcherW failed: {e} - this process was not started by the \
             service control manager (use 'install' + 'sc start', or 'console' for testing)"
        ))
    })
}

extern "system" fn service_main(argc: u32, argv: *mut PWSTR) {
    let args = collect_args(argc, argv);
    if let Err(e) = service_main_impl(&args) {
        log_error!("service stopped with an error: {e}");
        report(STATE_STOPPED, 0, 0);
    }
}

fn service_main_impl(args: &[String]) -> Result<()> {
    // The stop event is created first: the control handler needs it.
    let stop_event = worker::create_event(true)?;
    STOP_EVENT.store(stop_event.0 as isize, Ordering::SeqCst);

    let name = crate::ras::to_wide(SERVICE_NAME);
    let status_handle = unsafe {
        RegisterServiceCtrlHandlerExW(PCWSTR::from_raw(name.as_ptr()), Some(control_handler), None)
    }
    .map_err(|e| err(format!("RegisterServiceCtrlHandlerExW failed: {e}")))?;
    STATUS_HANDLE.store(status_handle.0 as isize, Ordering::SeqCst);

    // Tell the SCM immediately that more time is needed.
    report(STATE_START_PENDING, 1, 10_000);

    let explicit = config_arg(args);
    let config = match Config::load(explicit.as_deref()) {
        Ok(loaded) => {
            log_info!("configuration loaded from {}", loaded.path.display());
            loaded.config
        }
        Err(e) => {
            // The logger is not up yet, so start it with defaults to make sure
            // the reason lands in a file and in the event log.
            let fallback = Config::default();
            init_logger(&fallback, false);
            log_error!("configuration error: {e}");
            logger::event(
                Level::Error,
                1000,
                "pppoe",
                format_args!("the service cannot start: {e}"),
            );
            logger::shutdown();
            report(STATE_STOPPED, 0, 0);
            return Err(e);
        }
    };

    if !init_logger(&config, false) {
        // Keep going: a broken log directory must not take the service down.
    }
    log_info!("service starting, executable dir = {}", crate::config::exe_dir()?.display());
    log_info!("effective configuration: {}", config.summary());
    log_info!("credentials are never written to the log (user name and password are masked)");

    report(STATE_RUNNING, 0, 0);
    logger::event(Level::Info, 1000, "pppoe", format_args!("PPPoE dialer service started"));

    let worker_handle = worker::spawn(Arc::new(config), stop_event.0 as isize)?;

    // Wait for the worker. While a stop is in flight the SCM must be kept
    // informed, because stopping can legitimately take up to
    // `dial.dial_timeout_secs` (an in-flight dial is not aborted on purpose).
    let mut checkpoint: u32 = 2;
    while !worker_handle.is_finished() {
        std::thread::sleep(Duration::from_secs(1));
        if STOP_REQUESTED.load(Ordering::SeqCst) {
            checkpoint = checkpoint.saturating_add(1);
            report(STATE_STOP_PENDING, checkpoint, 15_000);
        }
    }

    match worker_handle.join() {
        Ok(Ok(())) => {}
        Ok(Err(e)) => log_error!("worker terminated with an error: {e}"),
        Err(_) => log_error!("the worker thread panicked"),
    }

    log_info!("service stopped; the broadband connection was left online by design");
    logger::event(Level::Info, 1001, "pppoe", format_args!("PPPoE dialer service stopped"));
    report(STATE_STOPPED, 0, 0);

    unsafe {
        let _ = CloseHandle(stop_event);
    }
    logger::shutdown();
    Ok(())
}

/// Initialise the logger from `config`. Returns `false` when it could not be set up.
fn init_logger(config: &Config, console: bool) -> bool {
    let secrets = config.secrets();
    let log_dir = match config.log_dir() {
        Ok(dir) => dir,
        Err(e) => {
            eprintln!("logger: {e}");
            return false;
        }
    };
    logger::init(Settings {
        level: config.level(),
        dir: &log_dir,
        file_name: LOG_FILE_NAME,
        rotation: config.rotation(),
        max_files: config.log.max_files,
        console,
        event_log: config.log.event_log,
        source_name: &config.service.name,
        secrets: &secrets,
    })
    .is_ok()
}

/// Report the current status to the SCM. Never blocks, so it is safe to call
/// from the control handler callback.
fn report(state: u32, checkpoint: u32, wait_hint: u32) {
    let raw = STATUS_HANDLE.load(Ordering::SeqCst);
    if raw == 0 {
        return;
    }
    CURRENT_STATE.store(state, Ordering::SeqCst);
    let status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: SERVICE_STATUS_CURRENT_STATE(state),
        dwControlsAccepted: if state == STATE_RUNNING {
            SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN
        } else {
            0
        },
        dwWin32ExitCode: 0,
        dwServiceSpecificExitCode: 0,
        dwCheckPoint: checkpoint,
        dwWaitHint: wait_hint,
    };
    unsafe {
        let _ = SetServiceStatus(SERVICE_STATUS_HANDLE(raw as *mut c_void), &status);
    }
}

/// Called by the SCM on its own thread. Must return quickly.
extern "system" fn control_handler(
    control: u32,
    _event_type: u32,
    _event_data: *mut c_void,
    _context: *mut c_void,
) -> u32 {
    match control {
        SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN => {
            STOP_REQUESTED.store(true, Ordering::SeqCst);
            // Push STOP_PENDING right away; the worker may still be dialling.
            report(STATE_STOP_PENDING, 1, 15_000);
            let raw = STOP_EVENT.load(Ordering::SeqCst);
            if raw != 0 {
                unsafe {
                    // Signalling an event is an atomic kernel operation, so the
                    // handler stays well inside the SCM time budget.
                    let _ = windows::Win32::System::Threading::SetEvent(HANDLE(raw as *mut c_void));
                }
            }
            0
        }
        SERVICE_CONTROL_INTERROGATE => {
            report(CURRENT_STATE.load(Ordering::SeqCst), 0, 0);
            0
        }
        _ => ERROR_CALL_NOT_IMPLEMENTED,
    }
}

/// Read the `argv` array `ServiceMain` receives (`argv[0]` is the service name).
///
/// The SCM passes the arguments this service was registered with, which lets an
/// administrator point the service at a non default configuration file.
fn collect_args(count: u32, argv: *mut PWSTR) -> Vec<String> {
    let mut out = Vec::new();
    if argv.is_null() {
        return out;
    }
    unsafe {
        for index in 0..count as usize {
            let wide = *argv.add(index);
            if wide.0.is_null() {
                continue;
            }
            let mut len = 0usize;
            while *wide.0.add(len) != 0 {
                len += 1;
            }
            out.push(String::from_utf16_lossy(std::slice::from_raw_parts(wide.0, len)));
        }
    }
    out
}
