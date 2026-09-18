//! Configuration model, TOML loading, validation and redaction.
//!
//! The configuration file lives **next to the executable** (`pppoe.toml`).
//! A service is started with `C:\Windows\System32` as its working directory, so
//! relative paths are always resolved against `current_exe()`, never against
//! the current directory.
//!
//! `Config` deliberately does **not** implement `Debug`/`Display`: the broadband
//! user name and password must never be formatted by accident. Use
//! [`Config::summary`] instead, which masks both of them.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::{Result, err};
use crate::logger::{Level, Rotation};

/// Default configuration file name (next to the executable).
pub const DEFAULT_FILE_NAME: &str = "pppoe.toml";

// ---------------------------------------------------------------------------
// [service]
// ---------------------------------------------------------------------------

#[derive(Clone, Deserialize)]
#[serde(default)]
pub struct ServiceConfig {
    /// SCM service name. Must match the compiled-in `SERVICE_NAME` constant,
    /// which is also used as the Windows event log source name.
    pub name: String,
    pub display_name: String,
    pub description: String,
}

impl Default for ServiceConfig {
    fn default() -> Self {
        Self {
            name: String::from("PppoeDialer"),
            display_name: String::from("PPPoE Auto Dialer"),
            description: String::from("Keeps a PPPoE broadband connection online"),
        }
    }
}

// ---------------------------------------------------------------------------
// [dial]
// ---------------------------------------------------------------------------

#[derive(Clone, Deserialize)]
#[serde(default)]
pub struct DialConfig {
    /// Phone book entry name, for example `Dr.com`. Not a secret: it is logged
    /// to make troubleshooting possible.
    pub entry_name: String,
    /// Broadband account. **Never logged.**
    pub username: String,
    /// Broadband password. **Never logged.**
    pub password: String,
    /// Phone book path. Empty means the default phone book of the account the
    /// process runs as.
    pub pbk_path: String,
    /// Create the phone book entry from the PPPoE template when it is missing.
    ///
    /// Enabled by default: a service runs as `LocalSystem`, whose phone book
    /// does not contain the connection created on the desktop, so the very
    /// first dial would otherwise fail with error 623.
    pub create_entry_if_missing: bool,
    /// Upper bound for a single dial attempt.
    pub dial_timeout_secs: u64,
    /// Optional dial domain, usually empty for PPPoE.
    pub domain: String,
}

impl Default for DialConfig {
    fn default() -> Self {
        Self {
            entry_name: String::new(),
            username: String::new(),
            password: String::new(),
            pbk_path: String::new(),
            // Self contained by default: the service creates the phone book
            // entry it needs instead of relying on one created on the desktop.
            create_entry_if_missing: true,
            dial_timeout_secs: 90,
            domain: String::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// [link_check]
// ---------------------------------------------------------------------------

#[derive(Clone, Deserialize)]
#[serde(default)]
pub struct LinkCheckConfig {
    pub enabled: bool,
    /// Empty means "any wired ethernet adapter". Otherwise a case insensitive
    /// substring match against the adapter alias or description.
    pub adapter_name: String,
    /// Interface types taking part in the verdict. Only `ethernet` is supported.
    pub interface_types: Vec<String>,
}

impl Default for LinkCheckConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            adapter_name: String::new(),
            interface_types: vec![String::from("ethernet")],
        }
    }
}

// ---------------------------------------------------------------------------
// [monitor]
// ---------------------------------------------------------------------------

#[derive(Clone, Deserialize)]
#[serde(default)]
pub struct MonitorConfig {
    pub reconnect_delay_secs: u64,
    pub max_backoff_secs: u64,
    pub backoff_jitter_secs: u64,
    /// Coalescing window for system notifications.
    pub event_debounce_ms: u64,
    /// Optional safety re-check. `0` (default) means "pure event driven".
    pub safety_recheck_secs: u64,
}

impl Default for MonitorConfig {
    fn default() -> Self {
        Self {
            reconnect_delay_secs: 5,
            max_backoff_secs: 300,
            backoff_jitter_secs: 3,
            event_debounce_ms: 500,
            safety_recheck_secs: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// [log]
// ---------------------------------------------------------------------------

#[derive(Clone, Deserialize)]
#[serde(default)]
pub struct LogConfig {
    /// Log directory, relative to the executable unless absolute.
    pub dir: String,
    pub level: String,
    pub rotation: String,
    pub max_files: usize,
    pub event_log: bool,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            dir: String::from("logs"),
            level: String::from("info"),
            rotation: String::from("daily"),
            max_files: 14,
            event_log: true,
        }
    }
}

// ---------------------------------------------------------------------------
// Root
// ---------------------------------------------------------------------------

#[derive(Clone, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    pub service: ServiceConfig,
    pub dial: DialConfig,
    pub link_check: LinkCheckConfig,
    pub monitor: MonitorConfig,
    pub log: LogConfig,
}

/// A parsed configuration together with the file it came from.
pub struct Loaded {
    pub config: Config,
    pub path: PathBuf,
}

impl Config {
    /// Resolve the configuration file path.
    ///
    /// Returns the explicit path when given, otherwise `<exe dir>/pppoe.toml`.
    pub fn resolve_path(explicit: Option<&Path>) -> Result<PathBuf> {
        match explicit {
            Some(path) => Ok(path.to_path_buf()),
            None => Ok(exe_dir()?.join(DEFAULT_FILE_NAME)),
        }
    }

    /// Read, parse and validate the configuration file.
    pub fn load(explicit: Option<&Path>) -> Result<Loaded> {
        let path = Self::resolve_path(explicit)?;
        let bytes = fs::read(&path)
            .map_err(|e| err(format!("cannot read configuration file {}: {e}", path.display())))?;
        let text =
            decode(&bytes).map_err(|e| err(format!("cannot decode {}: {e}", path.display())))?;
        let config = Self::from_toml_str(&text)
            .map_err(|e| err(format!("invalid configuration in {}: {e}", path.display())))?;
        config.validate()?;
        Ok(Loaded { config, path })
    }

    /// Parse a configuration from a TOML string (also used by unit tests).
    pub fn from_toml_str(text: &str) -> Result<Self> {
        let config: Config = toml::from_str(text)?;
        Ok(config)
    }

    /// Reject obviously broken configurations before the service starts.
    ///
    /// Error messages only mention *field names*; values are never echoed so a
    /// typo cannot leak the account or password into the log.
    pub fn validate(&self) -> Result<()> {
        if self.dial.entry_name.trim().is_empty() {
            return Err(err("dial.entry_name must not be empty"));
        }
        if self.dial.username.trim().is_empty() {
            return Err(err("dial.username must not be empty"));
        }
        if self.dial.password.is_empty() {
            return Err(err("dial.password must not be empty"));
        }
        if self.service.name.trim().is_empty() || self.service.name.contains(['\\', '/']) {
            return Err(err("service.name must be a plain name without path separators"));
        }
        if Level::parse(&self.log.level).is_none() {
            return Err(err("log.level must be one of: error, warn, info, debug, trace"));
        }
        if Rotation::parse(&self.log.rotation).is_none() {
            return Err(err("log.rotation must be either 'daily' or 'never'"));
        }
        if self.monitor.reconnect_delay_secs < 1 {
            return Err(err("monitor.reconnect_delay_secs must be at least 1"));
        }
        if self.monitor.max_backoff_secs < self.monitor.reconnect_delay_secs {
            return Err(err("monitor.max_backoff_secs must be >= monitor.reconnect_delay_secs"));
        }
        if self.monitor.event_debounce_ms > 60_000 {
            return Err(err("monitor.event_debounce_ms must not exceed 60000"));
        }
        if self.monitor.safety_recheck_secs != 0 && self.monitor.safety_recheck_secs < 30 {
            return Err(err("monitor.safety_recheck_secs must be 0 (disabled) or at least 30"));
        }
        if self.dial.dial_timeout_secs < 10 {
            return Err(err("dial.dial_timeout_secs must be at least 10"));
        }
        let _ = self.interface_types()?;
        Ok(())
    }

    pub fn level(&self) -> Level {
        Level::parse(&self.log.level).unwrap_or(Level::Info)
    }

    pub fn rotation(&self) -> Rotation {
        Rotation::parse(&self.log.rotation).unwrap_or(Rotation::Daily)
    }

    /// Interface type numbers (`IF_TYPE_*`) taking part in the cable verdict.
    pub fn interface_types(&self) -> Result<Vec<u32>> {
        if self.link_check.interface_types.is_empty() {
            return Err(err("link_check.interface_types must not be empty"));
        }
        let mut out = Vec::with_capacity(self.link_check.interface_types.len());
        for name in &self.link_check.interface_types {
            match name.trim().to_ascii_lowercase().as_str() {
                "ethernet" => out.push(crate::link::IF_TYPE_ETHERNET),
                other => {
                    return Err(err(format!(
                        "link_check.interface_types contains unsupported value '{other}' \
                         (supported: ethernet)"
                    )));
                }
            }
        }
        Ok(out)
    }

    /// Absolute log directory.
    pub fn log_dir(&self) -> Result<PathBuf> {
        let dir = Path::new(self.log.dir.trim());
        if dir.as_os_str().is_empty() {
            return Err(err("log.dir must not be empty"));
        }
        if dir.is_absolute() { Ok(dir.to_path_buf()) } else { Ok(exe_dir()?.join(dir)) }
    }

    /// Human readable, credential-free configuration summary.
    pub fn summary(&self) -> String {
        format!(
            "service={{ name:\"{}\", display:\"{}\" }} \
             dial={{ entry:\"{}\", user:\"***\", password:\"***\", pbk:\"{}\", \
             create_entry:{}, timeout:{}s, domain:{} }} \
             link_check={{ enabled:{}, adapter:\"{}\", types:{:?} }} \
             monitor={{ reconnect:{}s, max_backoff:{}s, jitter:{}s, debounce:{}ms, \
             safety_recheck:{} }} \
             log={{ dir:\"{}\", level:\"{}\", rotation:\"{}\", max_files:{}, event_log:{} }}",
            self.service.name,
            self.service.display_name,
            self.dial.entry_name,
            self.dial.pbk_path,
            self.dial.create_entry_if_missing,
            self.dial.dial_timeout_secs,
            if self.dial.domain.is_empty() { "\"\"" } else { "\"***\"" },
            self.link_check.enabled,
            self.link_check.adapter_name,
            self.link_check.interface_types,
            self.monitor.reconnect_delay_secs,
            self.monitor.max_backoff_secs,
            self.monitor.backoff_jitter_secs,
            self.monitor.event_debounce_ms,
            self.monitor.safety_recheck_secs,
            self.log.dir,
            self.log.level,
            self.log.rotation,
            self.log.max_files,
            self.log.event_log,
        )
    }

    /// Credentials that the logger must keep out of every sink.
    pub fn secrets(&self) -> Vec<String> {
        vec![self.dial.username.clone(), self.dial.password.clone()]
    }
}

/// Extract `--config <path>` / `--config=<path>` from a command line.
///
/// Used both by the interactive commands and by `ServiceMain`, so that a
/// service started by the SCM can be pointed at a non default configuration.
pub fn config_arg(args: &[String]) -> Option<PathBuf> {
    for (index, arg) in args.iter().enumerate() {
        // Form 1: `--config=<path>`.
        if let Some(value) = arg.strip_prefix("--config=") {
            if value.is_empty() {
                continue;
            }
            return Some(PathBuf::from(value));
        }
        // Form 2: `--config <path>`. An empty or missing value is ignored so the
        // caller falls back to the default location.
        let next = args.get(index + 1).map(String::as_str).unwrap_or("");
        if arg == "--config" && !next.is_empty() {
            return Some(PathBuf::from(next));
        }
    }
    None
}

/// Directory that contains the running executable.
pub fn exe_dir() -> Result<PathBuf> {
    let exe = std::env::current_exe()
        .map_err(|e| err(format!("cannot determine the executable path: {e}")))?;
    exe.parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| err("the executable path has no parent directory"))
}

/// Decode a configuration file to UTF-8 text.
///
/// Editors on Windows happily save `.toml` files as UTF-8 with BOM or even as
/// UTF-16. Supporting all three avoids the classic "the service dies with a
/// cryptic parse error after the file was edited in Notepad" problem.
fn decode(bytes: &[u8]) -> Result<String> {
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8(rest.to_vec()).map_err(|e| err(format!("not valid UTF-8: {e}")));
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return Ok(utf16_to_string(rest, true));
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        return Ok(utf16_to_string(rest, false));
    }
    String::from_utf8(bytes.to_vec()).map_err(|e| err(format!("not valid UTF-8: {e}")))
}

fn utf16_to_string(bytes: &[u8], little_endian: bool) -> String {
    // `as_chunks` hands out `&[u8; 2]` pairs directly, so no indexing is needed.
    // A trailing odd byte is dropped, exactly as `chunks_exact(2)` did.
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(
            |pair| {
                if little_endian { u16::from_le_bytes(*pair) } else { u16::from_be_bytes(*pair) }
            },
        )
        .collect();
    String::from_utf16_lossy(&units)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
[dial]
entry_name = "Dr.com"
username = "account-9"
password = "secret-9"
"#;

    /// Tests return `Result` and use `?` instead of `unwrap()`/`expect()`, so the
    /// `clippy::unwrap_used` / `clippy::expect_used` denies hold for test code
    /// as well and no test can abort on an unwrap.
    type TestResult = Result<()>;

    #[test]
    fn defaults_are_applied() -> TestResult {
        let config = Config::from_toml_str(MINIMAL)?;
        assert_eq!(config.service.name, "PppoeDialer");
        assert_eq!(config.monitor.reconnect_delay_secs, 5);
        assert_eq!(config.monitor.safety_recheck_secs, 0);
        assert_eq!(config.link_check.interface_types, vec!["ethernet"]);
        assert!(config.link_check.enabled);
        assert_eq!(config.log.rotation, "daily");
        // The phone book entry is created automatically unless told otherwise.
        assert!(config.dial.create_entry_if_missing);
        assert!(config.validate().is_ok());
        Ok(())
    }

    #[test]
    fn unknown_fields_are_ignored() -> TestResult {
        let text = format!("{MINIMAL}\n[monitor]\nfuture_option = 1\n");
        assert!(Config::from_toml_str(&text).is_ok());
        Ok(())
    }

    #[test]
    fn summary_and_secrets() -> TestResult {
        let config = Config::from_toml_str(MINIMAL)?;
        let summary = config.summary();
        assert!(!summary.contains("account-9"));
        assert!(!summary.contains("secret-9"));
        assert!(summary.contains("***"));
        assert!(summary.contains("Dr.com"));
        assert_eq!(config.secrets().len(), 2);
        Ok(())
    }

    #[test]
    fn missing_credentials_are_rejected() -> TestResult {
        let config = Config::from_toml_str("[dial]\nentry_name = \"x\"\n")?;
        match config.validate() {
            Ok(()) => return Err("validate() must reject a missing username".into()),
            Err(e) => assert!(e.to_string().contains("dial.username")),
        }
        Ok(())
    }

    #[test]
    fn bad_enums_are_rejected() -> TestResult {
        let text = format!("{MINIMAL}\n[log]\nlevel = \"loud\"\n");
        assert!(Config::from_toml_str(&text)?.validate().is_err());

        let text = format!("{MINIMAL}\n[log]\nrotation = \"hourly\"\n");
        assert!(Config::from_toml_str(&text)?.validate().is_err());
        Ok(())
    }

    #[test]
    fn safety_recheck_guard() -> TestResult {
        let text = format!("{MINIMAL}\n[monitor]\nsafety_recheck_secs = 5\n");
        assert!(Config::from_toml_str(&text)?.validate().is_err());

        let text = format!("{MINIMAL}\n[monitor]\nsafety_recheck_secs = 300\n");
        assert!(Config::from_toml_str(&text)?.validate().is_ok());
        Ok(())
    }

    #[test]
    fn interface_type_mapping() -> TestResult {
        let config = Config::from_toml_str(MINIMAL)?;
        assert_eq!(config.interface_types()?, vec![6]);

        let text = format!("{MINIMAL}\n[link_check]\ninterface_types = [\"wi-fi\"]\n");
        assert!(Config::from_toml_str(&text)?.interface_types().is_err());
        Ok(())
    }

    #[test]
    fn config_argument_parsing() {
        let args = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(config_arg(&args(&["console"])), None);
        assert_eq!(
            config_arg(&args(&["console", "--config", "a.toml"])),
            Some(PathBuf::from("a.toml"))
        );
        assert_eq!(
            config_arg(&args(&["--config=b.toml", "console"])),
            Some(PathBuf::from("b.toml"))
        );
        assert_eq!(config_arg(&args(&["--config"])), None);
    }

    #[test]
    fn utf16_files_are_decoded() -> TestResult {
        let mut bytes = vec![0xFF, 0xFE];
        for unit in MINIMAL.encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        let config = Config::from_toml_str(&decode(&bytes)?)?;
        assert_eq!(config.dial.entry_name, "Dr.com");
        Ok(())
    }

    #[test]
    fn utf8_bom_is_stripped() -> TestResult {
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(MINIMAL.as_bytes());
        assert!(Config::from_toml_str(&decode(&bytes)?).is_ok());
        Ok(())
    }
}
