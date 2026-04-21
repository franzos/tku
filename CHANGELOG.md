## [0.1.9] - 2026-04-21

### Added
- `tku sub --plan` — recommends whether to upgrade, downgrade, or stay on your Claude plan based on recent weekly cycles

## [0.1.8] - 2026-03-08

### Added
- Pace projection bar for subscription usage (estimated % at reset)

## [0.1.7] - 2026-03-06

### Added
- `tku subscription` (`tku sub`) — subscription usage overview with weekly breakdown

## [0.1.6] - 2026-03-01

### Fixed
- Safer error handling, bounded HTTP reads, and dedup collision resistance
- Watch mode loads exchange rate once instead of every refresh

## [0.1.5] - 2026-02-23

### Changed
- Cache now retains usage records after source files are deleted (e.g. agent cleanup)
- New `--prune` flag to manually remove stale cache entries when needed

## [0.1.4] - 2026-02-22

### Added
- `tku plot` — inline bar chart of token usage over time (1d/1w/1m periods)
- `tku watch` — live-updating cost monitor with file watcher (inotify/FSEvents/kqueue)
- Gemini CLI provider (`~/.gemini/tmp/*/chats/`)
- Droid (Factory) provider (`~/.factory/sessions/`)
- OpenClaw provider (`~/.openclaw/agents/`, + legacy paths)
- Kimi CLI provider (`~/.kimi/sessions/`)
- OpenCode SQLite support (`opencode.db`, behind `--features sqlite`)

## [0.1.3] - 2026-02-21

### Added
- CI Workflow: Output .deb (Debian,Ubuntu) & .rpm (Fedora,Centos,...) packages

## [0.1.2] - 2026-02-20

### Added
- Config file support (`~/.config/tku/config.toml`)
- OpenRouter and LLM Prices as alternative pricing sources (`--pricing-source`)
- Currency conversion via Frankfurter API (`--currency EUR`, etc.)
- Currency field in JSON and bar output

## [0.1.1] - 2025-07-08

- Initial release
