//! RAS (Remote Access Service) wrapper: dialling, status queries, change
//! notifications and error code translation.
//!
//! Design notes
//! ------------
//! * **No polling.** Connection changes are delivered through
//!   `RasConnectionNotificationW` registered with `INVALID_HANDLE_VALUE`
//!   (system wide mode), which signals a Win32 event when *any* RAS connection
//!   is created or terminated. `RasEnumConnectionsW` is only called once per
//!   notification to learn *which* connection changed.
//! * **No connection handle is kept.** Because the notification is registered
//!   globally the service never has to store an `HRASCONN`. That removes the
//!   `HRASCONN: !Send` cross-thread problem entirely and also means pulling the
//!   connection down is never needed: stopping the service leaves the broadband
//!   connection online.
//! * **Credentials never reach the log.** `dial` reports error codes and the
//!   phone book entry name only.

use std::time::Duration;

use windows::Win32::Foundation::{BOOL, CloseHandle, HANDLE, WAIT_TIMEOUT};
use windows::Win32::NetworkManagement::Rras::{
    ERROR_BUFFER_TOO_SMALL, ERROR_CANNOT_FIND_PHONEBOOK_ENTRY, HRASCONN, RASCN_Connection,
    RASCN_Disconnection, RASCONNSTATUSW, RASCONNW, RASCS_Connected, RASDIALPARAMSW, RASENTRYW,
    RASEO_RemoteDefaultGateway, RASET_Broadband, RASFP_Ppp, RASNP_Ip, RasConnectionNotificationW,
    RasDialW, RasEnumConnectionsW, RasGetConnectStatusW, RasGetEntryPropertiesW, RasHangUpW,
    RasSetEntryPropertiesW,
};
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};
use windows::core::PCWSTR;

use crate::config::DialConfig;
use crate::error::{Result, err, os_err};

/// `ERROR_MORE_DATA` - returned by `RasEnumConnectionsW` when the buffer filled up.
const ERROR_MORE_DATA: u32 = 234;

/// Stack size of the short lived dial watchdog thread (memory friendly).
const WATCHDOG_STACK_SIZE: usize = 128 * 1024;

/// Upper bound of buffer growth attempts for enumeration.
const MAX_ENUM_ATTEMPTS: usize = 5;

/// One entry of the system connection table.
#[derive(Clone)]
pub struct ConnectionInfo {
    pub entry: String,
    /// Raw `RASCONNSTATE` value.
    pub state: u32,
    pub connected: bool,
    /// Device name, useful when several connections share an entry name.
    pub device: String,
}

/// Translated RAS error code.
#[derive(Copy, Clone)]
pub struct ErrorInfo {
    pub code: u32,
    pub message: &'static str,
    /// `false` for problems the service cannot fix by trying again
    /// (wrong account, missing phone book entry, ...).
    pub retryable: bool,
}

/// Error carrying a RAS error code so the caller can react to it.
///
/// The message deliberately contains the entry-independent code only: it is
/// formatted into log records.
#[derive(Debug)]
pub struct RasFailure {
    pub code: u32,
    pub context: &'static str,
}

impl std::fmt::Display for RasFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} returned error code {}", self.context, self.code)
    }
}

impl std::error::Error for RasFailure {}

/// Extract the RAS error code from a boxed error, when there is one.
pub fn error_code(err: &crate::error::Error) -> Option<u32> {
    err.downcast_ref::<RasFailure>().map(|failure| failure.code)
}

/// A live RAS connection returned by [`dial`].
///
/// `HRASCONN` wraps a raw pointer and is therefore **not** `Send`: this value
/// must stay on the thread that created it. The service drops it immediately
/// after a successful dial - only the `dial-once` command keeps it, in order to
/// hang the test connection up again.
pub struct Dialed {
    conn: HRASCONN,
}

impl Dialed {
    /// Hang the connection up.
    ///
    /// Used by the `dial-once` diagnostic command only; the service itself
    /// never hangs a connection up, so that stopping the service keeps the
    /// broadband link online.
    pub fn hang_up(self) -> Result<()> {
        let rc = unsafe { RasHangUpW(self.conn) };
        if rc != 0 {
            return Err(os_err("RasHangUpW", rc));
        }
        Ok(())
    }
}

/// `HRASCONN` holding `INVALID_HANDLE_VALUE`, i.e. "all RAS connections".
fn all_connections() -> HRASCONN {
    HRASCONN(usize::MAX as *mut core::ffi::c_void)
}

/// Register the system wide RAS connection notification.
///
/// `event` is signalled whenever any RAS connection on this machine is created
/// or terminated. There is no public API to unregister it, so it is registered
/// exactly once per process lifetime with a single event object: registering
/// again per dial attempt would leak one registration every time.
pub fn register_connection_notification(event: HANDLE) -> Result<()> {
    let rc = unsafe {
        RasConnectionNotificationW(all_connections(), event, RASCN_Connection | RASCN_Disconnection)
    };
    if rc != 0 {
        return Err(os_err("RasConnectionNotificationW", rc));
    }
    Ok(())
}

/// Dial the configured connection synchronously.
///
/// A short lived watchdog thread aborts the attempt after `timeout` (a blocking
/// `RasDialW` has no timeout parameter of its own). The watchdog deliberately
/// does **not** watch the service stop event: stopping the service must not tear
/// down a connection that is just being established.
pub fn dial(cfg: &DialConfig, timeout: Duration) -> Result<Dialed> {
    let entry = to_wide(&cfg.entry_name);
    let user = to_wide(&cfg.username);
    let password = to_wide(&cfg.password);
    let domain = to_wide(&cfg.domain);

    let mut params = RASDIALPARAMSW {
        dwSize: std::mem::size_of::<RASDIALPARAMSW>() as u32,
        ..Default::default()
    };
    copy_to_buf(&mut params.szEntryName, &entry);
    copy_to_buf(&mut params.szUserName, &user);
    copy_to_buf(&mut params.szPassword, &password);
    copy_to_buf(&mut params.szDomain, &domain);

    let pbk = to_wide(&cfg.pbk_path);
    let pbk_ptr = if cfg.pbk_path.trim().is_empty() {
        PCWSTR::null()
    } else {
        PCWSTR::from_raw(pbk.as_ptr())
    };

    let done = unsafe { CreateEventW(None, BOOL::from(false), BOOL::from(false), PCWSTR::null()) }
        .map_err(|e| err(format!("CreateEventW failed: {e}")))?;
    // The watchdog thread only needs the raw handle value, which keeps it
    // `Send` without wrapping anything in `Arc`.
    let done_raw = done.0 as isize;
    let timeout_ms = duration_to_millis(timeout);

    let watchdog = std::thread::Builder::new()
        .name(String::from("dial-timeout"))
        .stack_size(WATCHDOG_STACK_SIZE)
        .spawn(move || {
            let done = HANDLE(done_raw as *mut core::ffi::c_void);
            let rc = unsafe { WaitForSingleObject(done, timeout_ms) };
            if rc == WAIT_TIMEOUT {
                // Still dialling after the deadline: interrupt the attempt.
                // A NULL handle drops every connection owned by this process,
                // which can only be the in-flight attempt because the service
                // holds no other handle.
                unsafe {
                    let _ = RasHangUpW(HRASCONN::default());
                }
            }
        })
        .ok();

    let mut conn = HRASCONN::default();
    // `RasDialW` runs on this thread, so the connection handle never crosses a
    // thread boundary.
    let rc = unsafe { RasDialW(None, pbk_ptr, &params, 0, None, &mut conn) };

    unsafe {
        let _ = SetEvent(done);
    }
    if let Some(handle) = watchdog {
        let _ = handle.join();
    }
    unsafe {
        let _ = CloseHandle(done);
    }

    if rc != 0 {
        // The code is preserved so the worker can decide whether retrying makes
        // sense; the message never carries credentials.
        return Err(Box::new(RasFailure { code: rc, context: "RasDialW" }));
    }
    Ok(Dialed { conn })
}

/// Enumerate the connections currently known to RAS.
///
/// The buffer has to be sized from the value RAS reports for a `NULL` array,
/// and it may still be too small on the next call (connections come and go), so
/// `ERROR_BUFFER_TOO_SMALL` is handled instead of assumed away.
pub fn list_connections() -> Result<Vec<ConnectionInfo>> {
    let mut needed: u32 = 0;
    let mut count: u32 = 0;
    let rc = unsafe { RasEnumConnectionsW(None, &mut needed, &mut count) };
    if rc == 0 {
        return Ok(Vec::new());
    }
    if rc != ERROR_BUFFER_TOO_SMALL {
        return Err(os_err("RasEnumConnectionsW", rc));
    }

    let entry_size = std::mem::size_of::<RASCONNW>();
    let mut needed = needed.max(entry_size as u32);

    for _ in 0..MAX_ENUM_ATTEMPTS {
        let capacity = (needed as usize / entry_size) + 1;
        let mut buffer = vec![RASCONNW::default(); capacity];
        // `capacity` is at least 1, and `first_mut` keeps this index-free.
        if let Some(first) = buffer.first_mut() {
            first.dwSize = entry_size as u32;
        }
        let mut buffer_bytes = (capacity * entry_size) as u32;
        let mut found: u32 = 0;

        let rc = unsafe {
            RasEnumConnectionsW(Some(buffer.as_mut_ptr()), &mut buffer_bytes, &mut found)
        };
        if rc == 0 {
            return Ok(collect(&buffer, found));
        }
        if rc == ERROR_BUFFER_TOO_SMALL || rc == ERROR_MORE_DATA {
            needed = buffer_bytes.max(needed.saturating_mul(2));
            continue;
        }
        return Err(os_err("RasEnumConnectionsW", rc));
    }
    Err(err("RasEnumConnectionsW: buffer still too small after retries"))
}

/// `true` when an entry with that name is currently in the `Connected` state.
pub fn is_connected(entry_name: &str) -> Result<bool> {
    Ok(list_connections()?.iter().any(|c| c.connected && c.entry.eq_ignore_ascii_case(entry_name)))
}

/// Create the phone book entry if it does not exist yet.
///
/// This is the **default deployment path** (`create_entry_if_missing = true`),
/// because a service runs as `LocalSystem` and its phone book does not contain
/// the broadband connection created on the desktop.
///
/// Returns `true` when an entry was created, `false` when it already existed.
pub fn ensure_entry(cfg: &DialConfig) -> Result<bool> {
    let entry = to_wide(&cfg.entry_name);
    let pbk = to_wide(&cfg.pbk_path);
    let pbk_ptr = if cfg.pbk_path.trim().is_empty() {
        PCWSTR::null()
    } else {
        PCWSTR::from_raw(pbk.as_ptr())
    };
    let entry_ptr = PCWSTR::from_raw(entry.as_ptr());

    // Probe first: a NULL entry buffer asks RAS only for the required size.
    // `ERROR_BUFFER_TOO_SMALL` also proves the entry exists (RAS got far enough
    // to know how much space it needs), so it must not be treated as an error:
    // only `ERROR_CANNOT_FIND_PHONEBOOK_ENTRY` means "missing".
    let mut size: u32 = 0;
    let rc = unsafe { RasGetEntryPropertiesW(pbk_ptr, entry_ptr, None, &mut size, None, None) };
    if rc == 0 || rc == ERROR_BUFFER_TOO_SMALL {
        return Ok(false);
    }
    if rc != ERROR_CANNOT_FIND_PHONEBOOK_ENTRY {
        return Err(os_err("RasGetEntryPropertiesW", rc));
    }

    // PPPoE template: a broadband entry whose device is the PPPoE WAN miniport,
    // negotiating TCP/IP over PPP. The field set mirrors what the Windows
    // "Broadband (PPPoE)" wizard writes, so the generated entry looks the same
    // as a hand made one in
    // `%ProgramData%\Microsoft\Network\Connections\Pbk\rasphone.pbk`:
    // `Type=5`, `DEVICE=PPPoE`, `Device=WAN Miniport (PPPOE)`.
    let mut template =
        RASENTRYW { dwSize: std::mem::size_of::<RASENTRYW>() as u32, ..Default::default() };

    // `RASET_Broadband` (5) is what a PPPoE entry must be; `RASET_Phone` (1)
    // would produce a dial-up style entry. The phone book stores this as
    // `Type=5`.
    template.dwType = RASET_Broadband;
    template.dwfNetProtocols = RASNP_Ip;
    template.dwFramingProtocol = RASFP_Ppp;

    // Dialling options.
    //
    // `LcpExtensions=1` - what a wizard created entry shows, and what most ISPs
    // expect - means "LCP extensions ENABLED", while the API flag is inverted
    // (`RASEO_DisableLcpExtensions`). Leaving that bit clear keeps them enabled,
    // so it is deliberately absent here; it is called out because it is easy to
    // get backwards.
    //
    // `RASEO_RemoteDefaultGateway` (`IpPrioritizeRemote=1` in the phone book)
    // routes traffic through the new connection, which is the point of a
    // broadband entry. Drop this flag if the machine must keep its existing
    // default route.
    //
    // `RASEO_IpHeaderCompression` and `RASEO_SwCompression` stay off: a PPPoE
    // link carries full sized frames, so compressing them is useless at best and
    // has been known to break the session on some access concentrators.
    // `RASEO_SpecificIpAddr` stays off so the address is server assigned
    // (`IpAssign=1`), and `RASEO_UseLogonCredentials` stays off because
    // `RasDialW` is given the credentials explicitly.
    template.dwfOptions = RASEO_RemoteDefaultGateway;

    template.dwCountryID = 1;
    template.dwCountryCode = 1;
    copy_to_buf(&mut template.szDeviceType, &to_wide("PPPoE"));
    copy_to_buf(&mut template.szDeviceName, &to_wide("WAN Miniport (PPPOE)"));

    let rc = unsafe {
        RasSetEntryPropertiesW(
            pbk_ptr,
            entry_ptr,
            &template,
            std::mem::size_of::<RASENTRYW>() as u32,
            None,
            0,
        )
    };
    if rc != 0 {
        return Err(os_err("RasSetEntryPropertiesW", rc));
    }
    Ok(true)
}

/// Translate a RAS error code into a message and a retry recommendation.
pub fn describe_error(code: u32) -> ErrorInfo {
    let (message, retryable) = match code {
        600 => ("an operation is still in progress", true),
        603 => ("the supplied buffer was too small", true),
        621 => ("the phone book could not be opened (check dial.pbk_path)", false),
        622 => ("the phone book could not be loaded (check dial.pbk_path)", false),
        623 => {
            ("the phone book entry was not found (check dial.entry_name / dial.pbk_path)", false)
        }
        624 => ("the phone book could not be updated (check file permissions)", false),
        628 => ("the connection was disconnected", true),
        629 => ("the connection was closed by the remote computer", true),
        638 => ("the request timed out", true),
        651 => ("the modem or network adapter reported an error", true),
        676 => ("the line is busy", true),
        678 => ("there was no answer (is the cable connected?)", true),
        691 => ("authentication failed - check the account or password", false),
        720 => ("PPP negotiation failed", true),
        739 => ("saved logon credentials cannot be used", false),
        756 => ("a dial operation is already in progress", true),
        775 => ("the call was blocked by the remote computer", true),
        _ => ("unknown RAS error", true),
    };
    ErrorInfo { code, message, retryable }
}

fn collect(buffer: &[RASCONNW], count: u32) -> Vec<ConnectionInfo> {
    let limit = (count as usize).min(buffer.len());
    let mut out = Vec::with_capacity(limit);
    for item in buffer.iter().take(limit) {
        // `RASCONNW` is packed(4) on 64-bit targets: copy the arrays out by
        // value instead of borrowing fields, avoiding any alignment hazard.
        let entry: [u16; 257] = item.szEntryName;
        let device: [u16; 129] = item.szDeviceName;
        let state = connection_state(item.hrasconn).unwrap_or(0);
        out.push(ConnectionInfo {
            entry: crate::link::wide_to_string(&entry),
            device: crate::link::wide_to_string(&device),
            state,
            connected: state == RASCS_Connected.0 as u32,
        });
    }
    out
}

fn connection_state(conn: HRASCONN) -> Option<u32> {
    let mut status = RASCONNSTATUSW {
        dwSize: std::mem::size_of::<RASCONNSTATUSW>() as u32,
        ..Default::default()
    };
    let rc = unsafe { RasGetConnectStatusW(conn, &mut status) };
    if rc != 0 {
        return None;
    }
    Some(status.rasconnstate.0 as u32)
}

/// Convert a Rust string to a NUL terminated UTF-16 buffer.
pub fn to_wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Copy `source` into a fixed size UTF-16 field, truncating if necessary and
/// always keeping the terminating NUL.
///
/// Every field this is used with is a fixed size array, but the empty case is
/// still handled explicitly so the function can never panic.
fn copy_to_buf(dest: &mut [u16], source: &[u16]) {
    if dest.is_empty() {
        return;
    }
    let len = source.len().min(dest.len());
    dest[..len].copy_from_slice(&source[..len]);
    if len == dest.len() {
        // `len >= 1` here because `dest` is not empty.
        dest[len - 1] = 0;
    }
}

fn duration_to_millis(timeout: Duration) -> u32 {
    let millis = timeout.as_millis();
    if millis > u32::MAX as u128 { u32::MAX } else { millis as u32 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_conversion_is_nul_terminated() {
        let wide = to_wide("Dr.com");
        assert_eq!(wide.last(), Some(&0));
        assert_eq!(wide.len(), 7);
        assert_eq!(crate::link::wide_to_string(&wide), "Dr.com");
    }

    #[test]
    fn copy_truncates_and_keeps_nul() {
        let mut dest = [0xFFFFu16; 4];
        copy_to_buf(&mut dest, &to_wide("abcdef"));
        assert_eq!(dest, [0x0061, 0x0062, 0x0063, 0]);
    }

    #[test]
    fn copy_fits_without_touching_the_nul() {
        let mut dest = [0xFFFFu16; 8];
        copy_to_buf(&mut dest, &to_wide("ab"));
        assert_eq!(&dest[..3], &[0x0061, 0x0062, 0]);
        assert_eq!(dest[3], 0xFFFF);
    }

    #[test]
    fn copy_into_an_empty_field_is_a_no_op() {
        // Guards the `dest.len() - 1` underflow that would otherwise panic.
        let mut dest: [u16; 0] = [];
        copy_to_buf(&mut dest, &to_wide("abc"));
        copy_to_buf(&mut dest, &[]);
    }

    #[test]
    fn error_table_covers_the_important_codes() {
        assert!(!describe_error(691).retryable);
        assert!(!describe_error(623).retryable);
        assert!(describe_error(678).retryable);
        assert!(describe_error(651).retryable);
        assert_eq!(describe_error(691).code, 691);
        assert!(describe_error(9999).retryable);
        for code in [600, 603, 623, 678, 691, 720, 9999] {
            assert!(!describe_error(code).message.is_empty());
        }
    }

    #[test]
    fn timeout_conversion_saturates() {
        assert_eq!(duration_to_millis(Duration::from_millis(1500)), 1500);
        assert_eq!(duration_to_millis(Duration::from_secs(u64::MAX)), u32::MAX);
    }
}
