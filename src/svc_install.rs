//! Service installation, removal and control.
//!
//! Everything the SCM needs:
//!
//! * `install`   - create the service (`SERVICE_AUTO_START`), set its
//!   description, configure crash recovery, register the Windows event log
//!   source and declare a dependency on `RasMan`;
//! * `uninstall` - stop and delete the service, remove the event log source;
//! * `start` / `stop` / `query` - thin wrappers around the `sc` equivalents.
//!
//! `install` writes the **absolute** executable path: a service is started with
//! `C:\Windows\System32` as its working directory, so a relative path would
//! never resolve.

use std::ffi::c_void;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, KEY_WRITE, REG_DWORD, REG_EXPAND_SZ, REG_OPTION_NON_VOLATILE,
    RegCloseKey, RegCreateKeyExW, RegDeleteKeyW, RegSetValueExW,
};
use windows::Win32::System::Services::{
    ChangeServiceConfig2W, CloseServiceHandle, ControlService, CreateServiceW, DeleteService,
    OpenSCManagerW, OpenServiceW, QueryServiceStatusEx, SC_ACTION, SC_ACTION_RESTART,
    SC_ACTION_TYPE, SC_HANDLE, SC_MANAGER_ALL_ACCESS, SC_STATUS_PROCESS_INFO, SERVICE_ALL_ACCESS,
    SERVICE_AUTO_START, SERVICE_CONFIG_DESCRIPTION, SERVICE_CONFIG_FAILURE_ACTIONS,
    SERVICE_CONTROL_STOP, SERVICE_DESCRIPTIONW, SERVICE_ERROR_NORMAL, SERVICE_FAILURE_ACTIONSW,
    SERVICE_STATUS, SERVICE_STATUS_PROCESS, SERVICE_STOPPED, SERVICE_WIN32_OWN_PROCESS,
    StartServiceW,
};
use windows::core::{Error as WinError, PCWSTR, PWSTR};

use crate::config::{Config, DEFAULT_FILE_NAME, exe_dir};
use crate::error::{Result, err, os_err};
use crate::ras::to_wide;
use crate::service::SERVICE_NAME;

/// `ERROR_SERVICE_EXISTS`
const ERROR_SERVICE_EXISTS: u32 = 1073;
/// `ERROR_SERVICE_DOES_NOT_EXIST`
const ERROR_SERVICE_DOES_NOT_EXIST: u32 = 1060;
/// `ERROR_SERVICE_ALREADY_RUNNING`
const ERROR_SERVICE_ALREADY_RUNNING: u32 = 1056;
/// `ERROR_SERVICE_NOT_ACTIVE`
const ERROR_SERVICE_NOT_ACTIVE: u32 = 1062;

/// Values written under `...\EventLog\Application\<name>`.
const EVENT_MESSAGE_FILE: &str = "%SystemRoot%\\System32\\EventCreate.exe";
/// error | warning | information
const EVENT_TYPES_SUPPORTED: u32 = 0x07;

/// Delay before each automatic restart, in milliseconds.
const RECOVERY_DELAYS: [u32; 3] = [5_000, 10_000, 30_000];

/// Name the service is registered under, after checking it against the
/// compiled-in constant the `ServiceMain` entry point is published as.
pub fn service_name(config: &Config) -> Result<String> {
    let name = config.service.name.trim();
    if name != SERVICE_NAME {
        return Err(err(format!(
            "[service] name = \"{name}\" does not match the built-in service name \
             \"{SERVICE_NAME}\"; fix the configuration file (or rebuild the binary)"
        )));
    }
    Ok(name.to_string())
}

/// Create the service and make it start automatically.
pub fn install(config: &Config) -> Result<()> {
    let name = service_name(config)?;
    let exe = std::env::current_exe()
        .map_err(|e| err(format!("cannot determine the executable path: {e}")))?;

    let scm = open_scm()?;
    let name_w = to_wide(&name);
    let display_w = to_wide(&config.service.display_name);
    let bin_w = to_wide(&exe.to_string_lossy());
    // RAS must be ready before we can dial, so declare the dependency.
    // `lpDependencies` is a double NUL terminated multi-string.
    let mut deps_w = to_wide("RasMan");
    deps_w.push(0);

    let created = unsafe {
        CreateServiceW(
            scm,
            PCWSTR::from_raw(name_w.as_ptr()),
            PCWSTR::from_raw(display_w.as_ptr()),
            SERVICE_ALL_ACCESS,
            SERVICE_WIN32_OWN_PROCESS,
            SERVICE_AUTO_START,
            SERVICE_ERROR_NORMAL,
            PCWSTR::from_raw(bin_w.as_ptr()),
            PCWSTR::null(),
            None,
            PCWSTR::from_raw(deps_w.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
        )
    };

    let service = match created {
        Ok(handle) => handle,
        Err(e) => {
            unsafe {
                let _ = CloseServiceHandle(scm);
            }
            if win32_code(&e) == ERROR_SERVICE_EXISTS {
                return Err(err(format!(
                    "the service \"{name}\" already exists - run 'uninstall' first"
                )));
            }
            return Err(err(format!(
                "CreateServiceW failed: {e} (this command needs administrator rights)"
            )));
        }
    };

    // A failing cosmetic step must not abort the installation.
    let mut problems: Vec<String> = Vec::new();
    if let Err(e) = set_description(service, &config.service.description) {
        problems.push(format!("description: {e}"));
    }
    if let Err(e) = set_recovery(service) {
        problems.push(format!("recovery actions: {e}"));
    }
    if let Err(e) = register_event_source(&name) {
        problems.push(format!("event log source: {e}"));
    }

    unsafe {
        let _ = CloseServiceHandle(service);
        let _ = CloseServiceHandle(scm);
    }

    let exe_dir = exe_dir()?;
    println!("service \"{name}\" installed with automatic start");
    println!("  binary : {}", exe.display());
    println!("  config : {}", exe_dir.join(DEFAULT_FILE_NAME).display());
    println!("  logs   : {}", config.log_dir()?.display());
    if !problems.is_empty() {
        println!("warnings (the service runs, but these should be fixed):");
        for problem in &problems {
            println!("  - {problem}");
        }
    }
    println!();
    println!("next steps:");
    println!(
        "  1. copy pppoe.toml.example to {} and fill in the connection details",
        exe_dir.join(DEFAULT_FILE_NAME).display()
    );
    println!("  2. restrict its permissions:");
    println!(
        "       icacls pppoe.toml /inheritance:r /grant:r \"BUILTIN\\Administrators:F\" \"NT AUTHORITY\\SYSTEM:F\""
    );
    println!("  3. start it: sc.exe start {name}");
    Ok(())
}

/// Stop (best effort) and delete the service.
pub fn uninstall(config: &Config) -> Result<()> {
    let name = service_name(config)?;
    let (scm, service) = open_service(&name)?;

    let mut status = SERVICE_STATUS::default();
    if unsafe { ControlService(service, SERVICE_CONTROL_STOP, &mut status) }.is_ok() {
        println!("stop requested, waiting for the service to exit ...");
        // Stopping may take up to `dial.dial_timeout_secs`: the service never
        // aborts an in-flight dial attempt on purpose.
        let deadline = Instant::now() + Duration::from_secs(config.dial.dial_timeout_secs + 20);
        loop {
            let state = query_state(service)?;
            if state == SERVICE_STOPPED.0 {
                break;
            }
            if Instant::now() >= deadline {
                println!("warning: the service did not stop in time, deleting it anyway");
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    let deleted = unsafe { DeleteService(service) };
    unsafe {
        let _ = CloseServiceHandle(service);
        let _ = CloseServiceHandle(scm);
    }
    match deleted {
        Ok(()) => {}
        Err(e) if win32_code(&e) == ERROR_SERVICE_DOES_NOT_EXIST => {
            return Err(err(format!("the service \"{name}\" is not installed")));
        }
        Err(e) => return Err(err(format!("DeleteServiceW failed: {e}"))),
    }

    if let Err(e) = remove_event_source(&name) {
        println!("warning: could not remove the event log source: {e}");
    }

    println!("service \"{name}\" removed");
    println!("note: the broadband connection was left online; hang it up manually if needed");
    Ok(())
}

/// Start the installed service.
pub fn start(config: &Config) -> Result<()> {
    let name = service_name(config)?;
    let (scm, service) = open_service(&name)?;
    let outcome = unsafe { StartServiceW(service, None) };
    unsafe {
        let _ = CloseServiceHandle(service);
        let _ = CloseServiceHandle(scm);
    }
    match outcome {
        Ok(()) => {
            println!("service \"{name}\": start requested");
            Ok(())
        }
        Err(e) if win32_code(&e) == ERROR_SERVICE_ALREADY_RUNNING => {
            println!("service \"{name}\" is already running");
            Ok(())
        }
        Err(e) => Err(err(format!("StartServiceW failed: {e}"))),
    }
}

/// Stop the running service. The broadband connection stays online.
pub fn stop(config: &Config) -> Result<()> {
    let name = service_name(config)?;
    let (scm, service) = open_service(&name)?;
    let mut status = SERVICE_STATUS::default();
    let outcome = unsafe { ControlService(service, SERVICE_CONTROL_STOP, &mut status) };
    unsafe {
        let _ = CloseServiceHandle(service);
        let _ = CloseServiceHandle(scm);
    }
    match outcome {
        Ok(()) => {
            println!("service \"{name}\": stop requested (the connection is kept online)");
            Ok(())
        }
        Err(e) if win32_code(&e) == ERROR_SERVICE_NOT_ACTIVE => {
            println!("service \"{name}\" is not running");
            Ok(())
        }
        Err(e) => Err(err(format!("ControlService(STOP) failed: {e}"))),
    }
}

/// Print the current SCM state of the service.
pub fn query(config: &Config) -> Result<()> {
    let name = service_name(config)?;
    let (scm, service) = open_service(&name)?;
    let mut status = SERVICE_STATUS_PROCESS::default();
    let mut needed: u32 = 0;
    let buffer = status_bytes(&mut status);
    let outcome =
        unsafe { QueryServiceStatusEx(service, SC_STATUS_PROCESS_INFO, Some(buffer), &mut needed) };
    unsafe {
        let _ = CloseServiceHandle(service);
        let _ = CloseServiceHandle(scm);
    }
    outcome.map_err(|e| err(format!("QueryServiceStatusEx failed: {e}")))?;

    println!(
        "service \"{name}\": state={} process_id={} controls_accepted=0x{:x}",
        state_name(status.dwCurrentState.0),
        status.dwProcessId,
        status.dwControlsAccepted
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Extract the Win32 error code from a `windows` error.
///
/// The `windows` crate wraps Win32 failures in `HRESULT_FROM_WIN32`, so the
/// human readable code lives in the low word of facility 7.
fn win32_code(error: &WinError) -> u32 {
    let hresult = error.code().0 as u32;
    if hresult & 0xFFFF_0000 == 0x8007_0000 { hresult & 0xFFFF } else { hresult }
}

fn open_scm() -> Result<SC_HANDLE> {
    unsafe { OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_ALL_ACCESS) }.map_err(|e| {
        err(format!("OpenSCManagerW failed: {e} - this command needs administrator rights"))
    })
}

fn open_service(name: &str) -> Result<(SC_HANDLE, SC_HANDLE)> {
    let scm = open_scm()?;
    let name_w = to_wide(name);
    match unsafe { OpenServiceW(scm, PCWSTR::from_raw(name_w.as_ptr()), SERVICE_ALL_ACCESS) } {
        Ok(service) => Ok((scm, service)),
        Err(e) => {
            unsafe {
                let _ = CloseServiceHandle(scm);
            }
            if win32_code(&e) == ERROR_SERVICE_DOES_NOT_EXIST {
                Err(err(format!("the service \"{name}\" is not installed - run 'install' first")))
            } else {
                Err(err(format!("OpenServiceW failed: {e}")))
            }
        }
    }
}

fn query_state(service: SC_HANDLE) -> Result<u32> {
    let mut status = SERVICE_STATUS_PROCESS::default();
    let mut needed: u32 = 0;
    let buffer = status_bytes(&mut status);
    unsafe { QueryServiceStatusEx(service, SC_STATUS_PROCESS_INFO, Some(buffer), &mut needed) }
        .map_err(|e| err(format!("QueryServiceStatusEx failed: {e}")))?;
    Ok(status.dwCurrentState.0)
}

/// Reinterpret a `SERVICE_STATUS_PROCESS` as the raw byte buffer the API wants.
fn status_bytes(status: &mut SERVICE_STATUS_PROCESS) -> &mut [u8] {
    // Safe: plain data, and the length is expressed in bytes.
    unsafe {
        std::slice::from_raw_parts_mut(
            status as *mut SERVICE_STATUS_PROCESS as *mut u8,
            std::mem::size_of::<SERVICE_STATUS_PROCESS>(),
        )
    }
}

fn set_description(service: SC_HANDLE, description: &str) -> Result<()> {
    let mut text = to_wide(description);
    let info = SERVICE_DESCRIPTIONW { lpDescription: PWSTR(text.as_mut_ptr()) };
    unsafe {
        ChangeServiceConfig2W(
            service,
            SERVICE_CONFIG_DESCRIPTION,
            Some(&info as *const SERVICE_DESCRIPTIONW as *const c_void),
        )
    }
    .map_err(|e| err(format!("ChangeServiceConfig2W(description) failed: {e}")))
}

/// Let the SCM restart the process after an unexpected exit.
///
/// This is the safety net for the `panic = "abort"` release profile, and it also
/// means a crash does not take the broadband connection down: the connection is
/// owned by `RasMan`, not by this process.
fn set_recovery(service: SC_HANDLE) -> Result<()> {
    let mut actions: Vec<SC_ACTION> = RECOVERY_DELAYS
        .iter()
        .map(|delay| SC_ACTION { Type: SC_ACTION_TYPE(SC_ACTION_RESTART.0), Delay: *delay })
        .collect();
    let info = SERVICE_FAILURE_ACTIONSW {
        dwResetPeriod: 86_400,
        lpRebootMsg: PWSTR(std::ptr::null_mut()),
        lpCommand: PWSTR(std::ptr::null_mut()),
        cActions: actions.len() as u32,
        lpsaActions: actions.as_mut_ptr(),
    };
    unsafe {
        ChangeServiceConfig2W(
            service,
            SERVICE_CONFIG_FAILURE_ACTIONS,
            Some(&info as *const SERVICE_FAILURE_ACTIONSW as *const c_void),
        )
    }
    .map_err(|e| err(format!("ChangeServiceConfig2W(recovery) failed: {e}")))
}

/// Create the event log source so Event Viewer shows readable entries instead of
/// "the description for event id ... cannot be found".
fn register_event_source(name: &str) -> Result<()> {
    let mut key = HKEY::default();
    let path_w = to_wide(&event_source_key(name));
    let rc = unsafe {
        RegCreateKeyExW(
            HKEY_LOCAL_MACHINE,
            PCWSTR::from_raw(path_w.as_ptr()),
            0,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut key,
            None,
        )
    };
    if rc.0 != ERROR_SUCCESS.0 {
        return Err(os_err("RegCreateKeyExW", rc.0));
    }

    let mut result = Ok(());
    let value_name = to_wide("EventMessageFile");
    let message_file = to_wide(EVENT_MESSAGE_FILE);
    let rc = unsafe {
        RegSetValueExW(
            key,
            PCWSTR::from_raw(value_name.as_ptr()),
            0,
            REG_EXPAND_SZ,
            Some(wide_as_bytes(&message_file)),
        )
    };
    if rc.0 != ERROR_SUCCESS.0 {
        result = Err(os_err("RegSetValueExW(EventMessageFile)", rc.0));
    }

    if result.is_ok() {
        let types_name = to_wide("TypesSupported");
        let supported = EVENT_TYPES_SUPPORTED.to_le_bytes();
        let rc = unsafe {
            RegSetValueExW(
                key,
                PCWSTR::from_raw(types_name.as_ptr()),
                0,
                REG_DWORD,
                Some(&supported),
            )
        };
        if rc.0 != ERROR_SUCCESS.0 {
            result = Err(os_err("RegSetValueExW(TypesSupported)", rc.0));
        }
    }

    unsafe {
        let _ = RegCloseKey(key);
    }
    result
}

fn remove_event_source(name: &str) -> Result<()> {
    let path_w = to_wide(&event_source_key(name));
    let rc = unsafe { RegDeleteKeyW(HKEY_LOCAL_MACHINE, PCWSTR::from_raw(path_w.as_ptr())) };
    if rc.0 != ERROR_SUCCESS.0 {
        return Err(os_err("RegDeleteKeyW", rc.0));
    }
    Ok(())
}

fn event_source_key(name: &str) -> String {
    format!("SYSTEM\\CurrentControlSet\\Services\\EventLog\\Application\\{name}")
}

fn wide_as_bytes(wide: &[u16]) -> &[u8] {
    // Safe: `u16` is plain data and the length is expressed in bytes.
    unsafe { std::slice::from_raw_parts(wide.as_ptr() as *const u8, wide.len() * 2) }
}

fn state_name(state: u32) -> &'static str {
    match state {
        1 => "STOPPED",
        2 => "START_PENDING",
        3 => "STOP_PENDING",
        4 => "RUNNING",
        5 => "CONTINUE_PENDING",
        6 => "PAUSE_PENDING",
        7 => "PAUSED",
        _ => "UNKNOWN",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_source_key_uses_the_service_name() {
        assert_eq!(
            event_source_key("PppoeDialer"),
            "SYSTEM\\CurrentControlSet\\Services\\EventLog\\Application\\PppoeDialer"
        );
    }

    #[test]
    fn state_names_are_stable() {
        assert_eq!(state_name(SERVICE_STOPPED.0), "STOPPED");
        assert_eq!(state_name(4), "RUNNING");
        assert_eq!(state_name(0xFFFF), "UNKNOWN");
    }

    #[test]
    fn service_name_must_match_the_binary() {
        let mut config = Config::default();
        assert!(service_name(&config).is_ok());
        config.service.name = String::from("SomethingElse");
        assert!(service_name(&config).is_err());
    }

    #[test]
    fn wide_bytes_are_two_bytes_per_unit() {
        let wide = to_wide("ab");
        assert_eq!(wide_as_bytes(&wide).len(), 6);
    }
}
