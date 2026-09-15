//! Event driven dial state machine.
//!
//! The worker owns the whole "keep the connection up" logic and is the only
//! thread that touches the blocking RAS APIs.
//!
//! ```text
//!   +---------------------------------------------------------------+
//!   |  wait for a system notification (or a back-off deadline)      |
//!   +---------------------------------------------------------------+
//!                 |                          ^
//!                 v                          |
//!   evaluate: cable?  -> down  -> wait ------+
//!             already online? -> wait -------+
//!             otherwise dial
//!                 |  ok  -> wait (connection is up, owned by RasMan)
//!                 |  err -> back-off, then wait
//! ```
//!
//! Nothing here polls: the thread blocks in `WaitForMultipleObjects` on the stop
//! event, the IP interface change event and the RAS connection change event.
//! `GetIfTable2` / `RasEnumConnectionsW` are evaluated **once per wake-up**.
//!
//! The connection is never hung up. It is owned by the `RasMan` service, so it
//! survives the service (and even a crash) - by design, see `docs/DESIGN.md`.

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows::Win32::Foundation::{BOOL, CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForMultipleObjects};
use windows::core::PCWSTR;

use crate::config::{Config, MonitorConfig};
use crate::error::{Result, err};
use crate::link;
use crate::logger::{log_debug, log_error, log_info, log_warn};
use crate::ras;

/// Stack size of the worker thread. The state machine is shallow, so 256 KiB is
/// plenty and keeps the committed memory of this idle service tiny.
const WORKER_STACK_SIZE: usize = 256 * 1024;

/// Create a Win32 event object.
///
/// `manual_reset = true` is used for the stop event (it must stay signalled);
/// auto reset events are used for the notification events.
pub fn create_event(manual_reset: bool) -> Result<HANDLE> {
    unsafe { CreateEventW(None, BOOL::from(manual_reset), BOOL::from(false), PCWSTR::null()) }
        .map_err(|e| err(format!("CreateEventW failed: {e}")))
}

/// Why the worker woke up.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum Wake {
    /// The service is stopping.
    Stop,
    /// A system notification arrived.
    Signal,
    /// The optional safety re-check (or a back-off deadline) elapsed.
    Timeout,
    /// `WaitForMultipleObjects` itself failed.
    Failed,
}

struct Waiter {
    stop: HANDLE,
    link: HANDLE,
    ras: HANDLE,
}

impl Waiter {
    fn wait(&self, timeout_ms: u32) -> Wake {
        // `stop` must be the first handle: WaitForMultipleObjects reports the
        // lowest signalled index, so a stop request always wins.
        let handles = [self.stop, self.link, self.ras];
        let rc = unsafe { WaitForMultipleObjects(&handles, BOOL::from(false), timeout_ms) };
        if rc.0 == WAIT_OBJECT_0.0 {
            Wake::Stop
        } else if rc.0 == WAIT_OBJECT_0.0 + 1 || rc.0 == WAIT_OBJECT_0.0 + 2 {
            Wake::Signal
        } else if rc.0 == WAIT_TIMEOUT.0 {
            Wake::Timeout
        } else {
            Wake::Failed
        }
    }
}

/// Exponential back-off with a cap and a little jitter.
pub struct Backoff {
    base: Duration,
    max: Duration,
    jitter: Duration,
    attempts: u32,
    seed: u64,
}

impl Backoff {
    pub fn new(cfg: &MonitorConfig) -> Self {
        Self {
            base: Duration::from_secs(cfg.reconnect_delay_secs.max(1)),
            max: Duration::from_secs(cfg.max_backoff_secs.max(cfg.reconnect_delay_secs.max(1))),
            jitter: Duration::from_secs(cfg.backoff_jitter_secs),
            attempts: 0,
            seed: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0x9E37_79B9_7F4A_7C15)
                | 1,
        }
    }

    /// Consecutive failed attempts since the last success.
    pub fn attempts(&self) -> u32 {
        self.attempts
    }

    pub fn reset(&mut self) {
        self.attempts = 0;
    }

    /// Next delay: `base * 2^(attempts-1)`, capped, plus `+/- jitter`.
    pub fn next_delay(&mut self) -> Duration {
        self.attempts = self.attempts.saturating_add(1);
        let shift = self.attempts.saturating_sub(1).min(16);
        let seconds = self.base.as_secs().saturating_mul(1u64 << shift).min(self.max.as_secs());

        let jitter_secs = self.jitter.as_secs();
        let seconds = if jitter_secs == 0 {
            seconds
        } else {
            let span = jitter_secs.saturating_mul(2).saturating_add(1);
            let offset = self.next_random() % span;
            seconds.saturating_sub(jitter_secs).saturating_add(offset).max(1)
        };
        Duration::from_secs(seconds.max(1))
    }

    fn next_random(&mut self) -> u64 {
        // xorshift64: avoids pulling in the `rand` crate just for jitter.
        let mut x = self.seed;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.seed = x;
        x
    }
}

/// Run the dial state machine until the stop event is signalled.
pub fn run(cfg: Arc<Config>, stop: HANDLE) -> Result<()> {
    let if_types = cfg.interface_types()?;
    let entry = cfg.dial.entry_name.clone();
    let timeout = Duration::from_secs(cfg.dial.dial_timeout_secs);

    // --- notifications -----------------------------------------------
    // One event object and one registration for the whole process lifetime:
    // the RAS notification API has no counterpart to unregister with, so
    // re-registering per dial would leak a registration each time.
    let link_event = create_event(false)?;
    let ras_event = create_event(false)?;
    let ip_guard = link::notify(link_event, true)?;
    ras::register_connection_notification(ras_event)?;
    log_info!("notifications registered: IP interface changes + all RAS connection events");

    prepare_entry(&cfg);

    let waiter = Waiter { stop, link: link_event, ras: ras_event };
    let safety_ms = safety_timeout_ms(&cfg);
    let debounce_ms = cfg.monitor.event_debounce_ms.min(u32::MAX as u64) as u32;

    let mut backoff = Backoff::new(&cfg.monitor);
    let mut cable_down = false;
    let mut online = false;

    let result = loop {
        // ---------------- 1. cable -------------------------------------
        if cfg.link_check.enabled {
            match link::check(&if_types, &cfg.link_check.adapter_name) {
                Ok(status) => {
                    log_debug!(
                        "cable check: usable={} fallback={} reason=\"{}\" candidates={}",
                        status.up,
                        status.fallback,
                        status.reason,
                        link::describe_candidates(&status.adapters)
                    );
                    if !status.up {
                        if !cable_down {
                            cable_down = true;
                            log_warn!("network cable is not connected: {}", status.reason);
                        }
                        if wait_for_change(&waiter, safety_ms, debounce_ms) == Wake::Stop {
                            break Ok(());
                        }
                        continue;
                    }
                    if cable_down {
                        cable_down = false;
                        log_info!("network cable is connected: {}", status.reason);
                    }
                    if let Some(adapter) = &status.decided_by {
                        log_debug!(
                            "cable verdict decided by \"{}\" (hardware={}, connector_present={})",
                            adapter.alias,
                            adapter.hardware,
                            adapter.connector_present
                        );
                    }
                    if status.fallback {
                        log_warn!(
                            "no adapter reports a media state; continuing on operational status"
                        );
                    }
                }
                Err(e) => {
                    // Never let a failed query stop the service permanently.
                    log_warn!("cannot read the interface table: {e}");
                }
            }
        }

        // ---------------- 2. already online? ---------------------------
        match ras::is_connected(&entry) {
            Ok(true) => {
                if !online {
                    online = true;
                    log_info!(
                        "connection \"{entry}\" is already online - no dial needed, waiting for events"
                    );
                }
                if wait_for_change(&waiter, safety_ms, debounce_ms) == Wake::Stop {
                    break Ok(());
                }
                continue;
            }
            Ok(false) => {
                if online {
                    online = false;
                    log_warn!("connection \"{entry}\" went offline");
                }
            }
            Err(e) => log_warn!("cannot enumerate RAS connections: {e}"),
        }

        // ---------------- 3. dial --------------------------------------
        if backoff.attempts() == 0 {
            log_info!("dialling \"{entry}\"");
        }
        match ras::dial(&cfg.dial, timeout) {
            // The returned handle is deliberately discarded right here:
            // `RasMan` owns the connection, so it stays up even when this
            // service stops or crashes. That is required behaviour, which is why
            // `RasHangUpW` is never called on this path.
            Ok(_) => {
                online = true;
                backoff.reset();
                log_info!("dial succeeded, connection \"{entry}\" is up");
            }
            Err(e) => {
                let code = ras::error_code(&e);
                match code {
                    Some(code) => {
                        let info = ras::describe_error(code);
                        if info.retryable {
                            log_error!("dial failed: code={} ({})", info.code, info.message);
                        } else {
                            log_error!(
                                "dial failed: code={} ({}) - this looks like a configuration problem",
                                info.code,
                                info.message
                            );
                        }
                    }
                    None => log_error!("dial failed: {e}"),
                }

                let mut delay = backoff.next_delay();
                if code.map(|c| !ras::describe_error(c).retryable).unwrap_or(false) {
                    // Configuration problems are not worth hammering the server
                    // for, so keep a slower floor for them.
                    delay = delay.max(Duration::from_secs(60));
                }
                log_warn!("retrying in {}s (attempt {})", delay.as_secs(), backoff.attempts());
                if wait_for_change(&waiter, millis32(delay), debounce_ms) == Wake::Stop {
                    break Ok(());
                }
            }
        }
    };

    // --- shutdown ------------------------------------------------------
    // Cancel the notification before releasing the event objects, otherwise an
    // in-flight callback could signal a handle that no longer exists.
    drop(ip_guard);
    unsafe {
        let _ = CloseHandle(link_event);
        let _ = CloseHandle(ras_event);
    }
    log_info!(
        "worker stopped; the broadband connection is left online (RasHangUp is never called)"
    );
    result
}

/// Dial once and optionally hang the connection up again (`dial-once` command).
pub fn dial_once(cfg: &Config, hang_up: bool) -> Result<()> {
    let if_types = cfg.interface_types()?;

    // Same order as the service: make sure the phone book entry exists first,
    // so a setup problem is reported before anything touches the line.
    prepare_entry(cfg);

    if cfg.link_check.enabled {
        let status = link::check(&if_types, &cfg.link_check.adapter_name)?;
        log_info!(
            "cable check: usable={} fallback={} reason=\"{}\"",
            status.up,
            status.fallback,
            status.reason
        );
        log_debug!("cable candidates: {}", link::describe_candidates(&status.adapters));
        if !status.up {
            return Err(err(format!("network cable is not connected: {}", status.reason)));
        }
    }

    match ras::list_connections() {
        Ok(connections) => {
            for c in &connections {
                log_info!(
                    "existing RAS connection: entry=\"{}\" device=\"{}\" state={} connected={}",
                    c.entry,
                    c.device,
                    c.state,
                    c.connected
                );
            }
            if connections.iter().any(|c| c.connected) {
                log_warn!("another RAS connection is already online; RAS may refuse a second dial");
            }
        }
        Err(e) => log_warn!("cannot enumerate RAS connections: {e}"),
    }

    if ras::is_connected(&cfg.dial.entry_name)? {
        log_info!("connection \"{}\" is already online; nothing to do", cfg.dial.entry_name);
        return Ok(());
    }

    let started = Instant::now();
    let dialed = ras::dial(&cfg.dial, Duration::from_secs(cfg.dial.dial_timeout_secs))?;
    log_info!(
        "dial succeeded in {:.1}s (entry \"{}\")",
        started.elapsed().as_secs_f32(),
        cfg.dial.entry_name
    );

    if hang_up {
        dialed.hang_up()?;
        log_info!("test connection hung up again (--keep would have left it online)");
    } else {
        log_info!("test connection left online on request (--keep)");
    }
    Ok(())
}

/// Start the state machine on a dedicated, modestly sized thread.
///
/// The stop event is passed as a raw handle value: `HANDLE` wraps a raw pointer
/// and is therefore not `Send`, so it cannot be moved into the thread directly.
pub fn spawn(cfg: Arc<Config>, stop_raw: isize) -> Result<std::thread::JoinHandle<Result<()>>> {
    std::thread::Builder::new()
        .name(String::from("pppoe-worker"))
        .stack_size(WORKER_STACK_SIZE)
        .spawn(move || {
            let stop = HANDLE(stop_raw as *mut core::ffi::c_void);
            run(cfg, stop)
        })
        .map_err(|e| err(format!("cannot start the worker thread: {e}")))
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Verify the phone book entry, creating it when `create_entry_if_missing` is on.
///
/// A failure is never fatal: the dial attempt that follows reports the real RAS
/// error (typically 623) and the retry logic takes over from there. Without this
/// step the very first dial of a freshly installed service would always fail,
/// because `LocalSystem`'s phone book does not contain the connection created on
/// the desktop.
fn prepare_entry(cfg: &Config) {
    if !cfg.dial.create_entry_if_missing {
        return;
    }
    match ras::ensure_entry(&cfg.dial) {
        Ok(true) => log_info!("created the phone book entry \"{}\"", cfg.dial.entry_name),
        Ok(false) => log_debug!("phone book entry \"{}\" already exists", cfg.dial.entry_name),
        Err(e) => log_warn!(
            "cannot verify the phone book entry \"{}\": {e} \
             (check dial.pbk_path and its permissions; dialling may fail with code 623)",
            cfg.dial.entry_name
        ),
    }
}

/// Wait for the next notification, coalescing bursts of them into one
/// evaluation. A `timeout_ms` of `INFINITE` means "event driven only".
fn wait_for_change(waiter: &Waiter, timeout_ms: u32, debounce_ms: u32) -> Wake {
    let wake = waiter.wait(timeout_ms);
    match wake {
        Wake::Signal => {
            if debounce_ms > 0 {
                // A single cable event produces several interface updates.
                // Swallow the whole burst before touching the API again.
                let deadline = Instant::now() + Duration::from_millis(debounce_ms as u64);
                loop {
                    let now = Instant::now();
                    if now >= deadline {
                        break;
                    }
                    let remaining = deadline.duration_since(now).as_millis() as u32;
                    match waiter.wait(remaining.max(1)) {
                        Wake::Signal => continue,
                        Wake::Stop => return Wake::Stop,
                        _ => break,
                    }
                }
            }
            Wake::Signal
        }
        Wake::Failed => {
            // Do not spin on a broken wait.
            log_warn!("WaitForMultipleObjects failed; retrying in 1s");
            std::thread::sleep(Duration::from_secs(1));
            Wake::Failed
        }
        other => other,
    }
}

fn safety_timeout_ms(cfg: &Config) -> u32 {
    if cfg.monitor.safety_recheck_secs == 0 {
        INFINITE
    } else {
        millis32(Duration::from_secs(cfg.monitor.safety_recheck_secs))
    }
}

fn millis32(duration: Duration) -> u32 {
    let millis = duration.as_millis();
    if millis >= u32::MAX as u128 { u32::MAX } else { millis as u32 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor(base: u64, max: u64, jitter: u64) -> MonitorConfig {
        MonitorConfig {
            reconnect_delay_secs: base,
            max_backoff_secs: max,
            backoff_jitter_secs: jitter,
            event_debounce_ms: 0,
            safety_recheck_secs: 0,
        }
    }

    #[test]
    fn backoff_doubles_and_caps() {
        let mut backoff = Backoff::new(&monitor(5, 60, 0));
        let expected = [5u64, 10, 20, 40, 60, 60, 60];
        for want in expected {
            assert_eq!(backoff.next_delay().as_secs(), want);
        }
        assert_eq!(backoff.attempts(), 7);
    }

    #[test]
    fn backoff_resets_after_success() {
        let mut backoff = Backoff::new(&monitor(5, 300, 0));
        let _ = backoff.next_delay();
        let _ = backoff.next_delay();
        assert_eq!(backoff.attempts(), 2);
        backoff.reset();
        assert_eq!(backoff.attempts(), 0);
        assert_eq!(backoff.next_delay().as_secs(), 5);
    }

    #[test]
    fn jitter_stays_within_bounds() {
        let jitter = 3;
        let mut backoff = Backoff::new(&monitor(10, 10, jitter));
        for _ in 0..200 {
            let secs = backoff.next_delay().as_secs();
            assert!((10 - jitter..=10 + jitter).contains(&secs), "jitter out of range: {secs}");
        }
    }

    #[test]
    fn jitter_never_goes_below_one_second() {
        let mut backoff = Backoff::new(&monitor(1, 1, 5));
        for _ in 0..200 {
            assert!(backoff.next_delay().as_secs() >= 1);
        }
    }

    #[test]
    fn millis_saturate() {
        assert_eq!(millis32(Duration::from_millis(10)), 10);
        assert_eq!(millis32(Duration::from_secs(u64::MAX)), u32::MAX);
    }

    #[test]
    fn safety_timeout_follows_the_configuration() {
        let mut config = Config::default();
        config.dial.entry_name = String::from("x");
        config.dial.username = String::from("y");
        config.dial.password = String::from("z");
        assert_eq!(safety_timeout_ms(&config), INFINITE);
        config.monitor.safety_recheck_secs = 300;
        assert_eq!(safety_timeout_ms(&config), 300_000);
    }
}
