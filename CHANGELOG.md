# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.2] - 2026-09-17

### Fixed

- A failed dial no longer leaves the PPPoE port busy. `RasDial` hands back a
  connection handle even when it fails, and the documentation requires that
  handle to be hung up ("even if `RasDial` returns a nonzero value"). The service
  dropped it, so a dial that was interrupted - typically by the cable being
  unplugged while the line was still being established - left the port half open.
  Every later attempt for the same entry was then rejected with
  `756 ERROR_DIAL_ALREADY_IN_PROGRESS` ("the specified port is already open"),
  and because that state lives inside the RAS manager, neither stopping nor
  uninstalling the service could clear it: only a RAS restart or a reboot did.

### Added

- The service now clears a stale dial attempt instead of retrying into it: on
  `756` it hangs up the connections of its entry that never reached the connected
  state, drops the back-off and retries immediately. Only attempts that are
  *not* online are touched, so a working broadband link is never dropped.
- After three consecutive blocked attempts with nothing left to abort, a warning
  names the way out (`net stop RasMan`, or a reboot), because that means the port
  is stuck inside the RAS manager where no connection is visible.
- A failed dial logs the current RAS connection table at debug level.
- Two unit tests for the stale-attempt rule.

### Changed

- Hanging up now waits until RAS reports the handle as invalid (polling
  `RasGetConnectStatus`, at most five seconds) instead of returning immediately.
  The documentation warns that dialling again - or exiting the process - right
  after `RasHangUp` can leave the port inconsistent, which also affected the
  `dial-once` diagnostic command.
- The dial watchdog cancels a timed out attempt through the connection table
  instead of calling `RasHangUp` with a NULL handle, whose meaning is not part of
  the public documentation.

## [0.1.1] - 2026-09-16

### Added

- `install.ps1` and `uninstall.ps1`. They ask for administrator rights through
  UAC, check that the broadband account name and the password are present, and
  then register, start or remove the service. Both accept `-DryRun`, which
  performs the checks and prints the commands without changing anything. The
  sample value `12345678` is not treated as a placeholder, because it is a
  plausible account name and refusing it would block a legitimate setup.
- `install.ps1 -LockConfig` restricts `pppoe.toml` to Administrators and SYSTEM
  with `icacls`, so the plain text password is not readable by every account.
- A unit test for the log retention itself, including that files in the log
  directory which are not ours are left alone.

### Changed

- The log retention limit is now enforced on every rotation, not only at
  start-up. A machine that stays up for months used to accumulate one file per
  day without bound; the oldest files are now trimmed as soon as a new day
  starts.
- The release archive ships the configuration template as `pppoe.toml` instead
  of `pppoe.toml.example`, so there is no copy step before the first start. The
  README warns that extracting a fresh archive over an existing installation
  would overwrite a configured `pppoe.toml`.
- The CLI hints and the header of the configuration template no longer tell the
  user to copy `pppoe.toml.example`.

### Removed

- The release archive no longer contains `LICENSE`, `CHANGELOG.md` and
  `DESIGN.md`. They stay in the repository, which the README and the release
  page link to.

## [0.1.0] - 2026-09-15

First public release: a Windows service that dials a PPPoE broadband connection
at boot and keeps it online.

### Added

- Service lifecycle through the service control manager: `install`, `uninstall`,
  `start`, `stop` and `query`, with automatic start at boot and a restart after
  a crash (5 s / 10 s / 30 s).
- Cable check before dialling, and never on a timer: the worker blocks in
  `WaitForMultipleObjects` and is woken by `NotifyIpInterfaceChange` (cable
  plug/unplug) and `RasConnectionNotification` (dial-up state), then evaluates
  the state once.
- Dial-state check: an already online connection is detected and left alone, so
  no second session is created.
- Phone book entry created automatically from a PPPoE template, so the
  connection does not have to exist in the Network and Sharing Center first
  (`dial.create_entry_if_missing`, on by default).
- Reconnection with exponential back-off plus jitter after a drop.
- TOML configuration next to the executable, accepted as UTF-8, UTF-8 with BOM
  or UTF-16.
- Daily rotating log file, plus Windows event log records, with a configurable
  level and retention.
- Command line: `console`, `dial-once [--keep]`, `list-connections`,
  `list-adapters`, `help`, `version`, and `--config <path>`.
- `list-adapters` prints every interface with its media state and its hardware /
  filter flags, which is what makes the adapter selection diagnosable.

### Security

- The account and the password are never written to the log file or to the
  Windows event log. Configuration summaries show them as `***`, and every log
  record additionally passes through a redaction filter.

### Notes

- Windows 10 / 11 / Server 2016 or later, x86_64.
- Stopping the service does not hang the connection up: the session stays online
  and the next start reports "already online" instead of dialling twice.
- Licensed under GPL-3.0-or-later.

[Unreleased]: https://github.com/pjm314159/pppoe-dialer/compare/v0.1.2...HEAD
[0.1.2]: https://github.com/pjm314159/pppoe-dialer/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/pjm314159/pppoe-dialer/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/pjm314159/pppoe-dialer/releases/tag/v0.1.0
