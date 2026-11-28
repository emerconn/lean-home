# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.0.1](https://github.com/emerconn/lean-home/compare/v0.0.0...v0.0.1) - 2026-11-28

### Added

- add sun trigger and sun condition matching astral 2.2 ([#55](https://github.com/emerconn/lean-home/pull/55))
- add blueprint loading and use_blueprint expansion ([#54](https://github.com/emerconn/lean-home/pull/54))
- add event bus, event trigger, and repeat/sequence/parallel/wait actions ([#53](https://github.com/emerconn/lean-home/pull/53))
- add numeric_state, time_pattern, homeassistant, template triggers and time condition ([#52](https://github.com/emerconn/lean-home/pull/52))
- add timedelta template helpers and state last_reported ([#51](https://github.com/emerconn/lean-home/pull/51))

## [0.0.0](https://github.com/emerconn/lean-home/releases/tag/v0.0.0) - 2026-11-26

### Added

- Docker image, a try-it Compose setup, and the roadmap as checklists ([#42](https://github.com/emerconn/lean-home/pull/42))
- keep automation on/off and last_triggered across restarts ([#41](https://github.com/emerconn/lean-home/pull/41))
- merge homeassistant packages like HA ([#40](https://github.com/emerconn/lean-home/pull/40))
- save and delete from the automation and script editors, with reload ([#39](https://github.com/emerconn/lean-home/pull/39))
- the automation and script editors open read-only ([#38](https://github.com/emerconn/lean-home/pull/38))
- triggers and automations ([#36](https://github.com/emerconn/lean-home/pull/36))
- scripts, conditions and run traces ([#28](https://github.com/emerconn/lean-home/pull/28))
- load configuration.yaml like HA, with the core config and time zone ([#23](https://github.com/emerconn/lean-home/pull/23))
- HA template helpers and live render_template ([#22](https://github.com/emerconn/lean-home/pull/22))
- serialize password checks and back off repeated failed logins ([#19](https://github.com/emerconn/lean-home/pull/19))
- MQTT over TLS and device-based discovery ([#16](https://github.com/emerconn/lean-home/pull/16))
- MQTT with HA discovery for lights, switches, sensors and binary sensors ([#15](https://github.com/emerconn/lean-home/pull/15))
- serve the HA frontend with HA-compatible auth and core WebSocket API ([#13](https://github.com/emerconn/lean-home/pull/13))

### Other

- check templates against HA's own test cases and recorded errors ([#27](https://github.com/emerconn/lean-home/pull/27))
- pin upstream versions in one file and document the upgrade path ([#25](https://github.com/emerconn/lean-home/pull/25))
- add CI, PR title check, Renovate, and cargo workspace skeleton ([#2](https://github.com/emerconn/lean-home/pull/2))
