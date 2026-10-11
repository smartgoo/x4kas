# Changelog

Notable changes to x4kas. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- Multiple named watchlists, each with its own alert rules, in the GUI and the CLI.
- Reset the app or resync the index from Settings.
- Configurable index slab size and retention period.
- The index records the stretches it skipped, and queries over them say so.
- Logging with configurable levels, to rolling files for the GUI and stderr for the CLI.
- A Health page with live logs, log file search and resource charts, opened from the top bar.

### Fixed

- Watchlist activity and query alerts, with their read marks, are kept across restarts and connection switches.
- The Kaspa client's warnings and errors no longer print into the CLI's stdout output.
- Quitting on macOS with Cmd+Q, the app menu or the Dock now shuts down cleanly.

## [0.1.0]

First release.

[Unreleased]: https://github.com/smartgoo/x4kas/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/smartgoo/x4kas/releases/tag/v0.1.0
