# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

[Unreleased]: https://github.com/pjm314159/pppoe-dialer/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/pjm314159/pppoe-dialer/releases/tag/v0.1.0
