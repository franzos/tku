## [0.1.5] - 2026-02-22

### Changed
- Bump

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
