## [0.1.13] - 2026-05-12

### Fixed
- Vault no longer holds a stale token after Claude Code refreshes or after `claude /login`. Every `tku` invocation now mirrors the live `~/.claude/.credentials.json` back into the active account's stash when it has changed — `account use` would otherwise restore a dead token on swap-back. Identity-gated, so live creds for a different account are never written to the wrong vault.
- `tku account list` and `tku account current` show the live `subscriptionType` / `rateLimitTier` — refreshed at snapshot time instead of staying frozen from `account add`.
- One-step rollback: previous vault contents are kept at `<name>.previous.credentials.json` on every snapshot.
- `~/.claude.json` writes drop from `0o644` to `0o600`. The file carries email + org identity even when no token is present.

## [0.1.12] - 2026-04-29

### Added
- `tku sub --all` — one-row overview across every registered Claude account, with a totals row
- `tku account use --force` / `remove --force` — safeguards against overwriting an unsaved live login or removing the active account by accident

### Changed
- Plain-language help for `tku account`, with a walkthrough for the two-account workflow

### Fixed
- Per-account attribution stays correct after cache rebuilds and credential swaps made outside tku
- `tku sub --account <name>` now actually scopes to that account (was silently ignored)
- `--all` and `--account` together are rejected instead of silently picking one
- Dedup no longer collapses identical message IDs seen under different accounts
- Atomic registry writes — concurrent `tku` runs can't truncate each other's switch log

## [0.1.11] - 2026-04-24

### Changed
- Smaller, faster release binary (`lto = "thin"`, `strip = true`, single codegen unit)
- Provider is now a typed enum; unknown `--tool` names drop to zero matches instead of matching everything
- Progress output throttled — no more TTY flush storms on warm runs
- Centralised path resolution with `TKU_HOME` override for isolated runs

### Fixed
- Atomic cache and credential writes (tmp + fsync + rename)
- Bounded JSONL reads (500 MB file / 16 MB line cap)
- Bitcode cache size guard falls back to re-parsing on corruption
- Credential stash dir created `0o700` on Unix
- Sqlite open failure falls back to bitcode cache instead of panicking
- Shared HTTPS-only `ureq` agent with bounded redirects
- Home-dir prefix redacted in user-visible paths

## [0.1.10] - 2026-04-21

### Added
- `tku account` — stash multiple Claude credentials and swap between them (add/use/list/current/rename/remove)
- `--account <name>` filter on all reports to scope records to a specific account
- Per-account subscription snapshot history (separate cycle data per `organizationUuid`)

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
