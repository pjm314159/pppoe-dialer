//! PPPoE auto dialer for Windows.
//!
//! One executable, several roles:
//!
//! ```text
//!   pppoe.exe                       run as a service (started by the SCM)
//!   pppoe.exe install | uninstall   register / remove the service
//!   pppoe.exe start | stop | query  control the installed service
//!   pppoe.exe console               run in the foreground for debugging
//!   pppoe.exe dial-once [--keep]    test a single dial attempt
//!   pppoe.exe list-connections      show the RAS connection table
//!   pppoe.exe list-adapters         show every network interface
//! ```
//!
//! All console output and every log record is ASCII English on purpose, so the
//! program behaves identically no matter which code page the host uses.
//!
//! The binary uses the default console subsystem so that the diagnostic
//! commands can print; when it is started by the SCM it detaches that console
//! immediately (see `service::run`).
//!
//! See `docs/DESIGN.md` for the full design.

mod config;
mod error;
mod link;
mod logger;
mod ras;
mod service;
mod svc_install;
mod worker;

use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicIsize, Ordering};

use windows::Win32::Foundation::{BOOL, HANDLE, TRUE};
use windows::Win32::System::Console::{SetConsoleCtrlHandler, SetConsoleOutputCP};
use windows::Win32::System::Threading::SetEvent;

use crate::config::Config;
use crate::error::{Result, err};
use crate::logger::{Settings, log_error, log_info};

/// Package version, taken from `Cargo.toml`.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Log file base name (the suffix `.YYYY-MM-DD` is added when rotating daily).
const LOG_FILE_NAME: &str = "pppoe.log";

/// UTF-8 code page for the console.
const CP_UTF8: u32 = 65001;

/// Stop event used by `console` mode (set by the Ctrl+C handler).
static CONSOLE_STOP: AtomicIsize = AtomicIsize::new(0);

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match dispatch(&args) {
        Ok(()) => 0,
        Err(e) => {
            log_error!("command failed: {e}");
            if logger::is_initialized() {
                eprintln!("error: {e}");
            }
            1
        }
    };
    logger::shutdown();
    std::process::exit(code);
}

fn dispatch(args: &[String]) -> Result<()> {
    let command = parse_command(args);
    match command.as_deref() {
        // No command: the SCM started us.
        None => service::run(),
        Some("install") => svc_install::install(&load_config(args)?),
        // Everything below must work even when the configuration file is
        // broken, otherwise a bad file could make the service unremovable.
        Some("uninstall") => svc_install::uninstall(&load_config(args).unwrap_or_default()),
        Some("start") => svc_install::start(&load_config(args).unwrap_or_default()),
        Some("stop") => svc_install::stop(&load_config(args).unwrap_or_default()),
        Some("query") => svc_install::query(&load_config(args).unwrap_or_default()),
        Some("console") => run_console(args),
        Some("dial-once") => run_dial_once(args),
        Some("list-connections") => run_list_connections(args),
        Some("list-adapters") => run_list_adapters(),
        Some("help") | Some("--help") | Some("-h") => {
            print_help();
            Ok(())
        }
        Some("version") | Some("--version") | Some("-V") => {
            println!("pppoe {VERSION}");
            Ok(())
        }
        Some(other) => {
            print_help();
            Err(err(format!("unknown command \"{other}\"")))
        }
    }
}

/// First non option token, skipping `--config <path>` and its value.
fn parse_command(args: &[String]) -> Option<String> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--config" {
            iter.next();
            continue;
        }
        if arg.starts_with("--config=") {
            continue;
        }
        return Some(arg.clone());
    }
    None
}

fn load_config(args: &[String]) -> Result<Config> {
    let explicit = config::config_arg(args);
    Ok(Config::load(explicit.as_deref())?.config)
}

/// Foreground mode: identical state machine, logs also go to the console.
fn run_console(args: &[String]) -> Result<()> {
    enable_utf8_console();
    let config = load_config(args)?;
    init_console_logger(&config)?;
    log_info!("pppoe {VERSION} console mode - press Ctrl+C to stop");
    log_info!("effective configuration: {}", config.summary());

    let stop = worker::create_event(true)?;
    CONSOLE_STOP.store(stop.0 as isize, Ordering::SeqCst);
    unsafe { SetConsoleCtrlHandler(Some(console_ctrl_handler), BOOL::from(true)) }
        .map_err(|e| err(format!("SetConsoleCtrlHandler failed: {e}")))?;

    let handle = worker::spawn(Arc::new(config), stop.0 as isize)?;
    match handle.join() {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(e),
        Err(_) => return Err(err("the worker thread panicked")),
    }
    log_info!("console mode finished; the broadband connection was left online");
    Ok(())
}

/// Dial once to validate the account, the entry name and the phone book.
fn run_dial_once(args: &[String]) -> Result<()> {
    enable_utf8_console();
    let config = load_config(args)?;
    init_console_logger(&config)?;
    // Hanging up keeps a diagnostic run from disturbing the machine state.
    let keep = args.iter().any(|arg| arg == "--keep");
    worker::dial_once(&config, !keep)
}

/// Print the RAS connection table (the main troubleshooting command).
fn run_list_connections(args: &[String]) -> Result<()> {
    enable_utf8_console();
    let config = load_config(args).unwrap_or_default();
    let _ = init_console_logger(&config);

    let connections = ras::list_connections()?;
    if connections.is_empty() {
        println!("no RAS connections are currently registered");
        return Ok(());
    }
    println!("{:<24} {:>10}  {:<22} connected", "entry", "state", "device");
    for connection in &connections {
        println!(
            "{:<24} {:>10}  {:<22} {}",
            connection.entry, connection.state, connection.device, connection.connected
        );
    }
    Ok(())
}

/// Print every network interface, to pick `link_check.adapter_name`.
fn run_list_adapters() -> Result<()> {
    enable_utf8_console();
    let adapters = link::snapshot_all()?;
    if adapters.is_empty() {
        println!("no network interfaces reported by GetIfTable2");
        return Ok(());
    }
    println!(
        "{:>5}  {:<8}  {:<8}  {:<7}  {:<7}  {:<10}  alias | description",
        "type", "ethernet", "media", "oper_up", "hw", "filter"
    );
    for adapter in &adapters {
        println!(
            "{:>5}  {:<8}  {:<8}  {:<7}  {:<7}  {:<10}  {} | {}",
            adapter.if_type,
            if adapter.if_type == link::IF_TYPE_ETHERNET { "yes" } else { "no" },
            format!("{:?}", adapter.media),
            adapter.oper_up,
            adapter.hardware,
            adapter.is_filter,
            adapter.alias,
            adapter.description
        );
    }
    println!();
    println!("columns:");
    println!("  ethernet  part of link_check.interface_types (\"ethernet\" = type 6)");
    println!("  media     Connected / Disconnected / Unknown; 'Unknown' means the driver");
    println!("            reports no media state, so it can never prove the cable is in");
    println!("  hw        real network card (HardwareInterface bit)");
    println!("  filter    NDIS filter shim (FilterInterface bit): never a cable");
    println!();
    println!("the cable verdict ignores 'filter' rows and prefers 'hw' rows; if the");
    println!("machine has more than one real card, set link_check.adapter_name to a");
    println!("substring of the alias or description above.");
    Ok(())
}

fn init_console_logger(config: &Config) -> Result<()> {
    let log_dir = config.log_dir()?;
    let secrets = config.secrets();
    logger::init(Settings {
        level: config.level(),
        dir: &log_dir,
        file_name: LOG_FILE_NAME,
        rotation: config.rotation(),
        max_files: config.log.max_files,
        console: true,
        // Debugging must not spam the Windows event log.
        event_log: false,
        source_name: &config.service.name,
        secrets: &secrets,
    })
}

/// Non-ASCII text can show up in adapter names and in the configuration, so the
/// console is switched to UTF-8 while the program talks to a terminal.
fn enable_utf8_console() {
    unsafe {
        let _ = SetConsoleOutputCP(CP_UTF8);
    }
}

extern "system" fn console_ctrl_handler(_control_type: u32) -> BOOL {
    let raw = CONSOLE_STOP.load(Ordering::SeqCst);
    if raw != 0 {
        unsafe {
            let _ = SetEvent(HANDLE(raw as *mut c_void));
        }
    }
    // Returning TRUE keeps the process alive so the worker can shut down
    // cleanly (and so the connection is left online on purpose).
    TRUE
}

fn print_help() {
    println!("pppoe {VERSION} - PPPoE auto dialer service");
    println!();
    println!("usage: pppoe.exe [command] [--config <path>]");
    println!();
    println!("commands:");
    println!("  (none)            run as a service (started by the service control manager)");
    println!("  install           register the service with automatic start (needs admin)");
    println!("  uninstall         stop and delete the service (needs admin)");
    println!("  start             start the installed service");
    println!("  stop              stop the service; the broadband connection stays online");
    println!("  query             show the current service state");
    println!("  console           run in the foreground with console logging");
    println!("  dial-once         perform a single dial attempt and hang it up again");
    println!("  dial-once --keep  perform a single dial attempt and leave it online");
    println!("  list-connections  show the RAS connection table");
    println!("  list-adapters     show every network interface and its link state");
    println!("  help              show this text");
    println!("  version           show the version");
    println!();
    println!("options:");
    println!("  --config <path>   use a configuration file other than <exe dir>/pppoe.toml");
    println!();
    println!("The configuration file is TOML; all fields are documented in pppoe.toml.");
    println!("Logs are written to the configured directory (default: <exe dir>/logs).");
}
