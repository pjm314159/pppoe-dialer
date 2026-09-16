//! Small, dependency free logger.
//!
//! Because this service is meant to run for months on a personal computer, the
//! logger is built for a low memory footprint instead of features:
//!
//! * one global [`Logger`] holding a single `Mutex<File>` - no background
//!   thread, no channel, no subscriber registry;
//! * disabled records cost one integer comparison, no allocation;
//! * the level filter is resolved once at start-up.
//!
//! Three sinks are supported:
//!
//! 1. rotating file - `<log dir>/pppoe.log` or `pppoe.log.YYYY-MM-DD`;
//! 2. console - only used by the `console` / `dial-once` commands;
//! 3. Windows event log - optional (`[log] event_log = true`).
//!
//! Every record passes through a credential guard: the configured broadband
//! user name and password are replaced by `***` before reaching any sink, so a
//! careless `log_info!("{:?}", params)` can never leak them.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

use windows::Win32::Foundation::HANDLE;
use windows::Win32::Security::PSID;
use windows::Win32::System::EventLog::{
    DeregisterEventSource, EVENTLOG_ERROR_TYPE, EVENTLOG_INFORMATION_TYPE, EVENTLOG_WARNING_TYPE,
    REPORT_EVENT_TYPE, RegisterEventSourceW, ReportEventW,
};
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::core::PCWSTR;

use crate::error::Result;

/// Placeholder written instead of any configured credential.
const MASK: &str = "***";

/// Credentials shorter than this are not searched for in log records. Our code
/// never formats credentials, so a very short secret only risks mangling
/// unrelated text (for example the digit `1` inside a normal message).
const MIN_SECRET_LEN: usize = 3;

/// Severity of a record. The ordering is significant: a record is emitted when
/// `record <= configured`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Error = 0,
    Warn = 1,
    Info = 2,
    Debug = 3,
    Trace = 4,
}

impl Level {
    /// Parse a `[log] level` value from the configuration file.
    pub fn parse(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "error" => Some(Self::Error),
            "warn" | "warning" => Some(Self::Warn),
            "info" => Some(Self::Info),
            "debug" => Some(Self::Debug),
            "trace" => Some(Self::Trace),
            _ => None,
        }
    }

    /// Fixed width label used in the file / console sinks.
    fn label(self) -> &'static str {
        match self {
            Self::Error => "ERROR",
            Self::Warn => "WARN ",
            Self::Info => "INFO ",
            Self::Debug => "DEBUG",
            Self::Trace => "TRACE",
        }
    }

    fn event_type(self) -> REPORT_EVENT_TYPE {
        match self {
            Self::Error => EVENTLOG_ERROR_TYPE,
            Self::Warn => EVENTLOG_WARNING_TYPE,
            _ => EVENTLOG_INFORMATION_TYPE,
        }
    }

    /// Default event id when the caller does not provide one.
    fn event_id(self) -> u32 {
        1000 + self as u32
    }
}

/// File rotation policy.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Rotation {
    /// One file per day: `pppoe.log.2026-09-15`.
    Daily,
    /// A single, ever growing file: `pppoe.log`.
    Never,
}

impl Rotation {
    pub fn parse(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "daily" => Some(Self::Daily),
            "never" => Some(Self::Never),
            _ => None,
        }
    }
}

/// Everything [`init`] needs. Borrowed to avoid cloning configuration strings
/// that already live in `Config`.
pub struct Settings<'a> {
    pub level: Level,
    /// Resolved, absolute log directory.
    pub dir: &'a Path,
    /// Base file name, e.g. `pppoe.log`.
    pub file_name: &'a str,
    pub rotation: Rotation,
    /// Oldest files above this count are removed at start-up.
    pub max_files: usize,
    /// Mirror records to stdout (`console` / `dial-once` commands).
    pub console: bool,
    /// Mirror records of level `Info` and above to the Windows event log.
    pub event_log: bool,
    /// Event log source name (matches the SCM service name).
    pub source_name: &'a str,
    /// Credentials that must never appear in any sink.
    pub secrets: &'a [String],
}

struct Sink {
    file: Option<File>,
    dir: PathBuf,
    file_name: String,
    rotation: Rotation,
    /// Day (`YYYY-MM-DD`) the currently open file belongs to.
    day: String,
    /// Oldest files above this count are removed - at start-up and again on
    /// every rotation, because a service that runs for months never reaches
    /// `init` a second time.
    max_files: usize,
    /// Event source handle kept as `usize` so the logger stays `Send + Sync`.
    event_source: Option<usize>,
    event_log: bool,
    /// Set after the first file open failure to avoid spamming the console.
    file_error_reported: bool,
}

struct Logger {
    level: Level,
    console: bool,
    secrets: Vec<String>,
    sink: Mutex<Sink>,
}

static LOGGER: OnceLock<Logger> = OnceLock::new();

/// Initialise the global logger. Calling it twice is a no-op.
pub fn init(settings: Settings<'_>) -> Result<()> {
    if LOGGER.get().is_some() {
        return Ok(());
    }

    if let Err(e) = fs::create_dir_all(settings.dir) {
        eprintln!("logger: cannot create log directory {}: {e}", settings.dir.display());
    } else if settings.max_files > 0 {
        prune(settings.dir, settings.file_name, settings.max_files);
    }

    let event_source =
        if settings.event_log { register_event_source(settings.source_name) } else { None };

    let logger = Logger {
        level: settings.level,
        console: settings.console,
        secrets: settings
            .secrets
            .iter()
            .filter(|s| s.chars().count() >= MIN_SECRET_LEN)
            .cloned()
            .collect(),
        sink: Mutex::new(Sink {
            file: None,
            dir: settings.dir.to_path_buf(),
            file_name: settings.file_name.to_string(),
            rotation: settings.rotation,
            day: String::new(),
            max_files: settings.max_files,
            event_source,
            event_log: settings.event_log,
            file_error_reported: false,
        }),
    };

    // Only fails when another thread initialised the logger concurrently.
    let _ = LOGGER.set(logger);
    Ok(())
}

/// Release the Windows event log source. Call once, right before exiting.
pub fn shutdown() {
    if let Some(logger) = LOGGER.get() {
        let mut sink = lock(&logger.sink);
        if let Some(raw) = sink.event_source.take() {
            unsafe {
                let _ = DeregisterEventSource(HANDLE(raw as *mut core::ffi::c_void));
            }
        }
        sink.file = None;
    }
}

/// `true` once [`init`] has completed successfully.
pub fn is_initialized() -> bool {
    LOGGER.get().is_some()
}

/// `true` when a record of that level would be emitted.
///
/// The log macros call this first so that `format_args!` is never evaluated.
pub fn enabled(level: Level) -> bool {
    match LOGGER.get() {
        Some(logger) => level <= logger.level,
        // Before `init` (configuration errors, early start-up) keep it quiet
        // but still surface problems.
        None => level <= Level::Info,
    }
}

/// Emit one record with an implicit event id.
pub fn write(level: Level, target: &str, args: std::fmt::Arguments<'_>) {
    emit(level, None, target, args);
}

/// Emit one record with an explicit event id (used for lifecycle events).
pub fn event(level: Level, event_id: u32, target: &str, args: std::fmt::Arguments<'_>) {
    emit(level, Some(event_id), target, args);
}

fn emit(level: Level, event_id: Option<u32>, target: &str, args: std::fmt::Arguments<'_>) {
    let Some(logger) = LOGGER.get() else {
        // Not initialised yet: fall back to stderr so early failures are seen.
        eprintln!("{level:?} {target}: {args}");
        return;
    };
    if level > logger.level {
        return;
    }

    let (text, leaked) = redact(logger, args);
    let (stamp, day) = now();
    let line = format!("{stamp} {} {target}: {text}", level.label());

    if logger.console {
        println!("{line}");
    }

    let mut sink = lock(&logger.sink);
    sink.write_file(&day, &line);
    if sink.event_log && level <= Level::Info {
        sink.write_event(level, event_id.unwrap_or_else(|| level.event_id()), &text);
    }
    if leaked {
        // Never print the offending text again - only report that it happened.
        let warning = format!(
            "{stamp} {} pppoe::logger: a credential was found in a log record and masked",
            Level::Warn.label()
        );
        sink.write_file(&day, &warning);
    }
}

/// Replace every configured credential with [`MASK`].
///
/// Returns the sanitised text and whether a replacement happened.
fn redact(logger: &Logger, args: std::fmt::Arguments<'_>) -> (String, bool) {
    let mut text = args.to_string();
    let mut leaked = false;
    for secret in &logger.secrets {
        if text.contains(secret.as_str()) {
            text = text.replace(secret.as_str(), MASK);
            leaked = true;
        }
    }
    (text, leaked)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // A poisoned logger must not take the whole service down.
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

impl Sink {
    fn write_file(&mut self, day: &str, line: &str) {
        if self.rotation == Rotation::Daily && self.day != day {
            // Rotating onto a new day. `self.day` is empty until the first record
            // has been written, so this only counts as a rotation once a file has
            // actually been open.
            let rotated = !self.day.is_empty();
            self.file = None;
            // A service that runs for months never reaches `init` again, so the
            // retention limit is enforced here too. Without this the file count
            // would only ever come down on a restart.
            if rotated && self.max_files > 0 {
                prune(&self.dir, &self.file_name, self.max_files);
            }
        }
        if self.file.is_none() {
            let name = match self.rotation {
                Rotation::Daily => format!("{}.{}", self.file_name, day),
                Rotation::Never => self.file_name.clone(),
            };
            let path = self.dir.join(name);
            match OpenOptions::new().create(true).append(true).open(&path) {
                Ok(file) => {
                    self.file = Some(file);
                    self.day = day.to_string();
                }
                Err(e) => {
                    if !self.file_error_reported {
                        self.file_error_reported = true;
                        eprintln!("logger: cannot open {}: {e}", path.display());
                    }
                    return;
                }
            }
        }
        if let Some(file) = self.file.as_mut() {
            // No BufWriter on purpose: a single small write per record keeps the
            // memory footprint flat and guarantees the bytes reach the OS even
            // if the process is terminated abruptly.
            let _ = file.write_all(line.as_bytes());
            let _ = file.write_all(b"\r\n");
            let _ = file.flush();
        }
    }

    fn write_event(&mut self, level: Level, event_id: u32, text: &str) {
        let Some(raw) = self.event_source else {
            return;
        };
        let handle = HANDLE(raw as *mut core::ffi::c_void);
        let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        let strings = [PCWSTR::from_raw(wide.as_ptr())];
        unsafe {
            let _ = ReportEventW(
                handle,
                level.event_type(),
                0,
                event_id,
                PSID::default(),
                0,
                Some(&strings),
                None,
            );
        }
    }
}

fn register_event_source(name: &str) -> Option<usize> {
    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    match unsafe { RegisterEventSourceW(PCWSTR::null(), PCWSTR::from_raw(wide.as_ptr())) } {
        Ok(handle) => Some(handle.0 as usize),
        Err(_) => None,
    }
}

/// Current local time as `("YYYY-MM-DD HH:MM:SS.mmm", "YYYY-MM-DD")`.
fn now() -> (String, String) {
    // GetLocalTime never fails and is cheap; it also decides the rotation
    // boundary, so local time (not UTC) is the right choice here.
    let t = unsafe { GetLocalTime() };
    let stamp = format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond, t.wMilliseconds
    );
    let day = format!("{:04}-{:02}-{:02}", t.wYear, t.wMonth, t.wDay);
    (stamp, day)
}

/// Keep at most `keep` files whose name starts with `prefix`, newest first.
fn prune(dir: &Path, prefix: &str, keep: usize) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with(prefix))
        .filter_map(|e| {
            let modified = e.metadata().ok()?.modified().ok()?;
            Some((modified, e.path()))
        })
        .collect();
    if files.len() <= keep {
        return;
    }
    files.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    for (_, path) in files.into_iter().skip(keep) {
        let _ = fs::remove_file(path);
    }
}

// ---------------------------------------------------------------------------
// Macros
// ---------------------------------------------------------------------------

macro_rules! log_error {
    ($($arg:tt)*) => {
        if $crate::logger::enabled($crate::logger::Level::Error) {
            $crate::logger::write(
                $crate::logger::Level::Error,
                module_path!(),
                format_args!($($arg)*),
            );
        }
    };
}

macro_rules! log_warn {
    ($($arg:tt)*) => {
        if $crate::logger::enabled($crate::logger::Level::Warn) {
            $crate::logger::write(
                $crate::logger::Level::Warn,
                module_path!(),
                format_args!($($arg)*),
            );
        }
    };
}

macro_rules! log_info {
    ($($arg:tt)*) => {
        if $crate::logger::enabled($crate::logger::Level::Info) {
            $crate::logger::write(
                $crate::logger::Level::Info,
                module_path!(),
                format_args!($($arg)*),
            );
        }
    };
}

macro_rules! log_debug {
    ($($arg:tt)*) => {
        if $crate::logger::enabled($crate::logger::Level::Debug) {
            $crate::logger::write(
                $crate::logger::Level::Debug,
                module_path!(),
                format_args!($($arg)*),
            );
        }
    };
}

pub(crate) use {log_debug, log_error, log_info, log_warn};

#[cfg(test)]
mod tests {
    use super::*;

    fn test_logger(secrets: &[String]) -> Logger {
        Logger {
            level: Level::Trace,
            console: false,
            secrets: secrets
                .iter()
                .filter(|s| s.chars().count() >= MIN_SECRET_LEN)
                .cloned()
                .collect(),
            sink: Mutex::new(Sink {
                file: None,
                dir: PathBuf::new(),
                file_name: String::from("pppoe.log"),
                rotation: Rotation::Never,
                day: String::new(),
                max_files: 14,
                event_source: None,
                event_log: false,
                file_error_reported: true,
            }),
        }
    }

    #[test]
    fn level_order_is_significant() {
        assert!(Level::Error < Level::Warn);
        assert!(Level::Warn < Level::Info);
        assert!(Level::Info < Level::Debug);
        assert!(Level::Debug < Level::Trace);
        assert_eq!(Level::parse("WARN"), Some(Level::Warn));
        assert_eq!(Level::parse(" info "), Some(Level::Info));
        assert_eq!(Level::parse("verbose"), None);
    }

    #[test]
    fn rotation_parsing() {
        assert_eq!(Rotation::parse("daily"), Some(Rotation::Daily));
        assert_eq!(Rotation::parse("NEVER"), Some(Rotation::Never));
        assert_eq!(Rotation::parse("hourly"), None);
    }

    #[test]
    fn credentials_are_masked() {
        // Dummy values only - never a real account or password. The two secrets
        // must differ, otherwise the assertions below could not tell "both the
        // account and the password are masked" from "only one of them is".
        let secrets = vec![String::from("12345678"), String::from("87654321")];
        let logger = test_logger(&secrets);
        let (text, leaked) =
            redact(&logger, format_args!("dial user=12345678 pass=87654321 entry=Dr.com"));
        assert!(leaked);
        assert!(!text.contains("12345678"));
        assert!(!text.contains("87654321"));
        assert!(text.contains("***"));
        assert!(text.contains("Dr.com"));
    }

    #[test]
    fn short_secrets_are_ignored() {
        let secrets = vec![String::from("ab")];
        let logger = test_logger(&secrets);
        let (text, leaked) = redact(&logger, format_args!("no change expected"));
        assert!(!leaked);
        assert_eq!(text, "no change expected");
    }

    #[test]
    fn timestamp_shape() {
        let (stamp, day) = now();
        assert_eq!(stamp.len(), 23, "unexpected timestamp: {stamp}");
        assert_eq!(day.len(), 10);
        assert_eq!(&stamp[..10], day.as_str());
    }

    #[test]
    fn prune_keeps_the_newest_files_only() {
        let dir = std::env::temp_dir().join("pppoe-logger-prune-test");
        let _ = fs::remove_dir_all(&dir);
        if fs::create_dir_all(&dir).is_err() {
            // A missing or read-only temp directory must not fail the suite.
            return;
        }

        // `prune` ranks by modification time, so the files are written a little
        // apart to get a deterministic order.
        for index in 0..5u32 {
            let name = format!("pppoe.log.2026-09-{:02}", index + 1);
            let _ = fs::write(dir.join(name), "2026-09-15 00:00:00.000 INFO  test\r\n");
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
        // Anything that is not one of our log files has to survive.
        let _ = fs::write(dir.join("keep-me.txt"), "not a log file");

        prune(&dir, "pppoe.log", 2);

        let mut left: Vec<String> = fs::read_dir(&dir)
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        left.sort();
        assert_eq!(left, vec!["keep-me.txt", "pppoe.log.2026-09-04", "pppoe.log.2026-09-05"]);

        let _ = fs::remove_dir_all(&dir);
    }
}
