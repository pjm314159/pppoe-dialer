//! Physical link (network cable) detection and interface change notifications.
//!
//! Two very different operations live here:
//!
//! * [`snapshot`] / [`check`] - a **one shot** query of the current interface
//!   state (`GetIfTable2`). It is called once at start-up and once per system
//!   notification, never on a timer.
//! * [`notify`] - registration of a change callback
//!   (`NotifyIpInterfaceChange`) that just signals a Win32 event. The worker
//!   blocks on that event instead of polling.
//!
//! Why `GetIfTable2` / `MIB_IF_ROW2` instead of `GetAdaptersAddresses`:
//! `IP_ADAPTER_ADDRESSES` has no media connect state, which is exactly the field
//! needed to tell "the cable is unplugged" from "authentication failed".
//!
//! Why the interface flags matter: a typical Windows machine reports dozens of
//! `IF_TYPE_ETHERNET_CSMACD` interfaces that are not network cards at all -
//! NDIS lightweight filter shims (`...-WFP Native MAC Layer LightWeight Filter-0000`,
//! `-Npcap Packet Driver-0000`, `-QoS Packet Scheduler-0000`) and virtual
//! miniports. They happily report `MediaConnectStateConnected`, so taking "any
//! ethernet interface is up" as "the cable is plugged in" would be wrong. The
//! `FilterInterface` / `HardwareInterface` / `ConnectorPresent` bits are used to
//! discard the shims and to prefer real hardware.

use std::ffi::c_void;

use windows::Win32::Foundation::{ERROR_SUCCESS, HANDLE};
use windows::Win32::NetworkManagement::IpHelper::{
    CancelMibChangeNotify2, FreeMibTable, GetIfTable2, IF_TYPE_ETHERNET_CSMACD, MIB_IF_TABLE2,
    MIB_IPINTERFACE_ROW, MIB_NOTIFICATION_TYPE, NotifyIpInterfaceChange,
};
use windows::Win32::NetworkManagement::Ndis::{
    IfOperStatusUp, MediaConnectStateConnected, MediaConnectStateUnknown,
};
use windows::Win32::Networking::WinSock::AF_UNSPEC;
use windows::Win32::System::Threading::SetEvent;

use crate::error::{Result, os_err};

/// `IF_TYPE_ETHERNET_CSMACD` - wired ethernet.
pub const IF_TYPE_ETHERNET: u32 = IF_TYPE_ETHERNET_CSMACD;

/// `MIB_IF_ROW2::InterfaceAndOperStatusFlags` bits.
const FLAG_HARDWARE_INTERFACE: u8 = 0x01;
const FLAG_FILTER_INTERFACE: u8 = 0x02;
const FLAG_CONNECTOR_PRESENT: u8 = 0x04;

/// Media (cable) connect state of one interface.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum MediaState {
    Connected,
    Disconnected,
    /// The driver does not report a media state (common for wireless and some
    /// virtual adapters). Such interfaces must not decide the verdict.
    Unknown,
}

/// Everything the service needs to know about one interface.
#[derive(Clone)]
pub struct AdapterInfo {
    /// Friendly name, for example `Ethernet` / `以太网`.
    pub alias: String,
    pub description: String,
    pub if_type: u32,
    pub oper_up: bool,
    pub media: MediaState,
    /// A real network card rather than a virtual miniport.
    pub hardware: bool,
    /// An NDIS filter / lightweight-filter shim, never a cable.
    pub is_filter: bool,
    /// A physical connector exists (a cable can be plugged in here).
    pub connector_present: bool,
}

impl AdapterInfo {
    /// Interface types that can plausibly carry the PPPoE session.
    pub fn is_candidate(&self) -> bool {
        !self.is_filter
    }
}

/// Outcome of a cable check.
pub struct LinkStatus {
    /// `true` when dialling may proceed.
    pub up: bool,
    /// `true` when the verdict came from `OperStatus` because no adapter
    /// reported a usable media state. Logged so it can be diagnosed.
    pub fallback: bool,
    /// Short ASCII explanation for the log.
    pub reason: String,
    /// Adapter that decided the verdict, when there was one.
    pub decided_by: Option<AdapterInfo>,
    pub adapters: Vec<AdapterInfo>,
}

/// One shot snapshot of every interface whose type is in `if_types`.
pub fn snapshot(if_types: &[u32]) -> Result<Vec<AdapterInfo>> {
    snapshot_filtered(Some(if_types))
}

/// Snapshot of **every** interface, regardless of type. Used by the
/// `list-adapters` diagnostic command so the adapter name filter can be chosen.
pub fn snapshot_all() -> Result<Vec<AdapterInfo>> {
    snapshot_filtered(None)
}

fn snapshot_filtered(if_types: Option<&[u32]>) -> Result<Vec<AdapterInfo>> {
    let mut table: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
    let rc = unsafe { GetIfTable2(&mut table) };
    if rc.0 != ERROR_SUCCESS.0 || table.is_null() {
        return Err(os_err("GetIfTable2", rc.0));
    }

    let mut out = Vec::new();
    // The table is owned by the caller and must be released with FreeMibTable.
    unsafe {
        let count = (*table).NumEntries as usize;
        // `Table` is a flexible array member, so the real length comes from
        // `NumEntries` and the rows are viewed as a slice.
        let rows = std::slice::from_raw_parts((*table).Table.as_ptr(), count);
        for row in rows {
            let wanted = match if_types {
                Some(types) => types.contains(&row.Type),
                None => true,
            };
            if !wanted {
                continue;
            }
            let flags = row.InterfaceAndOperStatusFlags._bitfield;
            out.push(AdapterInfo {
                alias: wide_to_string(&row.Alias),
                description: wide_to_string(&row.Description),
                if_type: row.Type,
                oper_up: row.OperStatus.0 == IfOperStatusUp.0,
                media: media_state(row.MediaConnectState.0),
                hardware: flags & FLAG_HARDWARE_INTERFACE != 0,
                is_filter: flags & FLAG_FILTER_INTERFACE != 0,
                connector_present: flags & FLAG_CONNECTOR_PRESENT != 0,
            });
        }
        FreeMibTable(table as *const c_void);
    }
    Ok(out)
}

/// Decide whether the cable is plugged in.
///
/// * `filter` - when non-empty, only adapters whose alias or description
///   contains it (case insensitive) are considered.
///
/// Selection order:
/// 1. drop NDIS filter shims;
/// 2. apply the configured name filter, if any;
/// 3. prefer real hardware interfaces; fall back to whatever is left;
/// 4. the cable is "plugged in" when one of them is operationally up **and**
///    reports `MediaConnectStateConnected`.
///
/// Interfaces reporting an unknown media state are never counted as connected;
/// if *no* remaining interface reports a media state at all, the verdict falls
/// back to `OperStatus` so that an unusual driver cannot keep the service from
/// ever dialing.
pub fn check(if_types: &[u32], filter: &str) -> Result<LinkStatus> {
    let adapters = snapshot(if_types)?;
    let filter = filter.trim().to_lowercase();

    let without_shims: Vec<AdapterInfo> =
        adapters.iter().filter(|a| a.is_candidate()).cloned().collect();
    let named: Vec<AdapterInfo> = if filter.is_empty() {
        without_shims
    } else {
        without_shims.iter().filter(|a| matches(a, &filter)).cloned().collect()
    };
    let hardware: Vec<AdapterInfo> = named.iter().filter(|a| a.hardware).cloned().collect();
    let matched = if hardware.is_empty() { named } else { hardware };

    if matched.is_empty() {
        let reason = if filter.is_empty() {
            String::from("no usable ethernet adapter found (filter shims are ignored)")
        } else {
            format!("no ethernet adapter matches the configured filter '{filter}'")
        };
        return Ok(LinkStatus { up: false, fallback: false, reason, decided_by: None, adapters });
    }

    // Preferred: a definitive media state.
    if let Some(adapter) = matched.iter().find(|a| a.media == MediaState::Connected && a.oper_up) {
        let reason = format!("cable connected on '{}'", adapter.alias);
        return Ok(LinkStatus {
            up: true,
            fallback: false,
            reason,
            decided_by: Some(adapter.clone()),
            adapters,
        });
    }

    let media_is_reported = matched.iter().any(|a| a.media != MediaState::Unknown);
    if media_is_reported {
        let adapter = matched
            .iter()
            .find(|a| a.media == MediaState::Disconnected)
            .or_else(|| matched.first());
        let reason = match adapter {
            Some(a) => format!("cable not connected on '{}'", a.alias),
            None => String::from("cable not connected"),
        };
        return Ok(LinkStatus {
            up: false,
            fallback: false,
            reason,
            decided_by: adapter.cloned(),
            adapters,
        });
    }

    // No driver reported a media state: fall back to the operational status so
    // dialing can still be attempted.
    let adapter = matched.iter().find(|a| a.oper_up).or_else(|| matched.first());
    let up = matched.iter().any(|a| a.oper_up);
    let reason = if up {
        String::from("no media state reported; falling back to operational status (up)")
    } else {
        String::from("no media state reported and the interface is operationally down")
    };
    Ok(LinkStatus { up, fallback: up, reason, decided_by: adapter.cloned(), adapters })
}

/// RAII wrapper around a `NotifyIpInterfaceChange` registration.
///
/// Dropping it cancels the notification. Always drop it *before* closing the
/// event handle, otherwise an in-flight callback could signal a released handle.
pub struct IpChangeGuard {
    handle: HANDLE,
    active: bool,
}

impl Drop for IpChangeGuard {
    fn drop(&mut self) {
        if self.active {
            unsafe {
                let _ = CancelMibChangeNotify2(self.handle);
            }
            self.active = false;
        }
    }
}

/// Register a callback that signals `event` whenever an IP interface is added,
/// removed or updated (this includes cable plug/unplug).
///
/// With `initial = true` the callback fires once right away, which gives the
/// worker a free first evaluation without any polling.
pub fn notify(event: HANDLE, initial: bool) -> Result<IpChangeGuard> {
    let mut handle = HANDLE::default();
    let rc = unsafe {
        NotifyIpInterfaceChange(
            AF_UNSPEC,
            Some(interface_changed),
            Some(event.0 as *const c_void),
            initial,
            &mut handle,
        )
    };
    if rc.0 != ERROR_SUCCESS.0 {
        return Err(os_err("NotifyIpInterfaceChange", rc.0));
    }
    Ok(IpChangeGuard { handle, active: true })
}

/// System callback. It runs on an OS thread, therefore it must be as short as
/// possible: signal the event and return. No allocation, no locking, no I/O.
extern "system" fn interface_changed(
    caller_context: *const c_void,
    _row: *const MIB_IPINTERFACE_ROW,
    _notification_type: MIB_NOTIFICATION_TYPE,
) {
    if caller_context.is_null() {
        return;
    }
    unsafe {
        let _ = SetEvent(HANDLE(caller_context as *mut c_void));
    }
}

fn matches(adapter: &AdapterInfo, lowercase_filter: &str) -> bool {
    adapter.alias.to_lowercase().contains(lowercase_filter)
        || adapter.description.to_lowercase().contains(lowercase_filter)
}

fn media_state(raw: i32) -> MediaState {
    if raw == MediaConnectStateConnected.0 {
        MediaState::Connected
    } else if raw == MediaConnectStateUnknown.0 {
        MediaState::Unknown
    } else {
        MediaState::Disconnected
    }
}

/// Convert a NUL terminated UTF-16 buffer into a `String` (lossy).
pub fn wide_to_string(buffer: &[u16]) -> String {
    let len = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..len])
}

/// Describe only the usable (non filter-shim) adapters.
///
/// A typical machine reports 30+ filter shims; dumping all of them into every
/// log record would be pure noise, so only the rows that can decide the cable
/// verdict are logged. `pppoe.exe list-adapters` prints the full table.
pub fn describe_candidates(adapters: &[AdapterInfo]) -> String {
    let candidates: Vec<AdapterInfo> =
        adapters.iter().filter(|adapter| adapter.is_candidate()).cloned().collect();
    describe(&candidates)
}

/// Compact description of an adapter list for log records.
pub fn describe(adapters: &[AdapterInfo]) -> String {
    if adapters.is_empty() {
        return String::from("[]");
    }
    let mut out = String::from("[");
    for (index, adapter) in adapters.iter().enumerate() {
        if index > 0 {
            out.push_str(", ");
        }
        out.push_str(&format!(
            "{{type:{}, up:{}, media:{:?}, hw:{}, filter:{}, alias:\"{}\"}}",
            adapter.if_type,
            adapter.oper_up,
            adapter.media,
            adapter.hardware,
            adapter.is_filter,
            adapter.alias
        ));
    }
    out.push(']');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adapter(alias: &str, media: MediaState, oper_up: bool) -> AdapterInfo {
        AdapterInfo {
            alias: String::from(alias),
            description: format!("{alias} controller"),
            if_type: IF_TYPE_ETHERNET,
            oper_up,
            media,
            hardware: true,
            is_filter: false,
            connector_present: true,
        }
    }

    fn filter_shim(alias: &str) -> AdapterInfo {
        AdapterInfo {
            alias: String::from(alias),
            description: format!("{alias} -WFP Native MAC Layer LightWeight Filter-0000"),
            if_type: IF_TYPE_ETHERNET,
            oper_up: true,
            media: MediaState::Connected,
            hardware: false,
            is_filter: true,
            connector_present: false,
        }
    }

    fn virtual_miniport(alias: &str, media: MediaState) -> AdapterInfo {
        AdapterInfo {
            alias: String::from(alias),
            description: format!("{alias} miniport"),
            if_type: IF_TYPE_ETHERNET,
            oper_up: true,
            media,
            hardware: false,
            is_filter: false,
            connector_present: false,
        }
    }

    #[test]
    fn wide_string_conversion() {
        let buffer = [0x0041u16, 0x0042, 0, 0x0043];
        assert_eq!(wide_to_string(&buffer), "AB");
        assert_eq!(wide_to_string(&[]), "");
    }

    #[test]
    fn filter_matches_alias_and_description() {
        let a = adapter("Ethernet 2", MediaState::Connected, true);
        assert!(matches(&a, "ethernet"));
        assert!(matches(&a, "controller"));
        assert!(!matches(&a, "wifi"));
    }

    #[test]
    fn media_state_mapping() {
        assert_eq!(media_state(MediaConnectStateConnected.0), MediaState::Connected);
        assert_eq!(media_state(MediaConnectStateUnknown.0), MediaState::Unknown);
        assert_eq!(media_state(2), MediaState::Disconnected);
    }

    #[test]
    fn describe_is_stable() {
        assert_eq!(describe(&[]), "[]");
        let text = describe(&[adapter("Eth", MediaState::Connected, true)]);
        assert!(text.starts_with('['));
        assert!(text.contains("media:Connected"));
        assert!(text.contains("hw:true"));
    }

    #[test]
    fn filter_shims_are_never_candidates() {
        assert!(!filter_shim("本地连接* 8-WFP").is_candidate());
        assert!(adapter("Ethernet", MediaState::Connected, true).is_candidate());
    }

    #[test]
    fn hardware_wins_over_virtual_miniports() {
        // A virtual miniport that claims to be connected must not mask a real
        // card whose cable is unplugged.
        let mut hardware = adapter("Ethernet", MediaState::Disconnected, false);
        hardware.connector_present = true;
        let miniport = virtual_miniport("WAN Miniport (IP)", MediaState::Connected);

        // Emulate the selection performed inside `check`.
        let named = [hardware, miniport];
        let only_hardware: Vec<AdapterInfo> =
            named.iter().filter(|a| a.hardware).cloned().collect();
        assert_eq!(only_hardware.len(), 1);
        assert!(only_hardware[0].alias == "Ethernet");
        assert!(!only_hardware[0].oper_up);
    }
}
