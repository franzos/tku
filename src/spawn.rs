//! `tku account exec`: launch an isolated one-shot Claude Code instance
//! authenticated as a stashed account, without disturbing the globally-active
//! `~/.claude`.
//!
//! Claude Code honours `CLAUDE_CONFIG_DIR`: every `~/.claude*` path relocates
//! under it, with no fallback to `~/.claude`. We seed a private dir from the
//! account's stashed credentials (symlinking the shared skills/plugins/etc.
//! by default), launch claude there, mirror any refreshed credentials back to
//! the stash, and refuse to run if the account is already live anywhere.
//! Claude's OAuth refresh tokens are single-use, so two live sessions sharing
//! one login invalidate each other.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use directories::BaseDirs;
use notify::{EventKind, RecursiveMode, Watcher};

use crate::accounts::{self, redact};
use crate::atomic_write::atomic_write;

const TOOL: &str = "claude";
const CREDS_FILE: &str = ".credentials.json";
/// Shared, non-stateful config that every isolated instance can borrow from
/// `~/.claude/` by symlink (or copy under `--copy`).
const SHARED_ENTRIES: &[&str] = &[
    "skills",
    "plugins",
    "agents",
    "commands",
    "CLAUDE.md",
    "output-styles",
];

pub fn run(
    name: &str,
    ephemeral: bool,
    clean: bool,
    copy: bool,
    command: Vec<String>,
) -> Result<i32> {
    accounts::validate_name(name)?;
    // Like sudo/env: exec runs an explicit command, it never launches claude
    // implicitly. `command[0]` is the program, the rest its arguments.
    if command.is_empty() {
        bail!("no command given. usage: tku account exec <name> [--ephemeral] [--clean] [--copy] -- <command> [args...]");
    }

    let registry = accounts::load_registry(TOOL);
    let account = registry.find_by_name(name).cloned().ok_or_else(|| {
        anyhow!("Account '{name}' not found. Run `tku account list` to see available accounts.")
    })?;

    let stash_creds = accounts::stashed_creds_path(TOOL, name)
        .context("cannot determine stashed credentials path")?;
    if !stash_creds.exists() {
        bail!(
            "Stashed credentials for '{name}' are missing ({}). The registry is out of sync; \
             re-add the account with `tku account add {name}`.",
            redact(&stash_creds)
        );
    }

    // Guard (a): refuse if this account is the live ~/.claude login. Resolve the
    // live org UUID from the strongest available witness: the creds file, then
    // ~/.claude.json's oauthAccount (survives token refresh, catches an
    // out-of-band /login), then the switch log.
    let live_org = resolve_live_org(
        accounts::current_claude_org_uuid(),
        accounts::current_claude_oauth_org(),
        registry.latest_switch().map(|s| s.org_uuid.clone()),
    );
    if is_already_live(&account.org_uuid, live_org.as_deref()) {
        bail!(
            "Account '{name}' is already live as the active ~/.claude login.\n\
             Running it here too would share one single-use refresh token between two sessions \
             and brick both.\n\
             To run a second session of this account, add a separate login with fresh \
             credentials via `tku account add`."
        );
    }

    let parent = crate::paths::spawn_dir(TOOL)
        .context("cannot determine a private runtime dir (set $XDG_RUNTIME_DIR or $HOME)")?;
    create_dir_secure(&parent).with_context(|| format!("create {}", redact(&parent)))?;
    ensure_private_dir(&parent)?;

    // Guard (b): per-account advisory lock. Held for the lifetime of this
    // process; auto-clears if a previous holder died without releasing it.
    let lock_path = parent.join(format!("{name}.lock"));
    let _lock = match SpawnLock::acquire(&lock_path)? {
        Some(l) => l,
        None => bail!(
            "Account '{name}' is already live in another `tku account exec` session.\n\
             Its single-use refresh token can't be shared without bricking both.\n\
             To run a second session of this account, add a separate login with fresh \
             credentials via `tku account add`."
        ),
    };

    let config = crate::config::load_config();
    let ephemeral = ephemeral || config.spawn.as_ref().and_then(|s| s.ephemeral) == Some(true);

    let (dir, _tmp) = if ephemeral {
        let d = make_ephemeral_dir(&parent, name)?;
        let guard = EphemeralGuard {
            path: d.clone(),
            active: true,
        };
        (d, Some(guard))
    } else {
        let d = parent.join(name);
        create_dir_secure(&d).with_context(|| format!("create {}", redact(&d)))?;
        (d, None)
    };

    let home = BaseDirs::new().ok_or_else(|| anyhow!("cannot determine home directory"))?;
    let seed = Seed {
        dir: &dir,
        stash_creds: &stash_creds,
        claude_home: &home.home_dir().join(".claude"),
        claude_json: &home.home_dir().join(".claude.json"),
        oauth_account: account.oauth_account.as_ref(),
        clean,
        copy,
    };
    if account.oauth_account.is_none() {
        eprintln!(
            "tku: warning: no cached identity for '{name}'; Claude's /status may show a blank \
             account. Run `tku account use {name}` once (or re-add it) to backfill it."
        );
    }
    seed.apply()?;

    let email = account
        .oauth_account
        .as_ref()
        .and_then(|v| v.get("emailAddress"))
        .and_then(|v| v.as_str());
    match email {
        Some(addr) => eprintln!(
            "tku: running as account '{name}' ({addr}), isolated config at {}",
            redact(&dir)
        ),
        None => eprintln!(
            "tku: running as account '{name}', isolated config at {}",
            redact(&dir)
        ),
    }

    let code = launch_and_sync(&dir, &stash_creds, name, &account.org_uuid, &command)?;
    Ok(code)
}

/// True when `target_org` is the currently-live `~/.claude` account.
fn is_already_live(target_org: &str, live_org: Option<&str>) -> bool {
    live_org == Some(target_org)
}

/// Pick the strongest available witness of the live account's org UUID:
/// the creds file, then `~/.claude.json`'s oauthAccount, then the switch log.
fn resolve_live_org(
    creds: Option<String>,
    oauth: Option<String>,
    switch: Option<String>,
) -> Option<String> {
    creds.or(oauth).or(switch)
}

// --- Seeding ---

struct Seed<'a> {
    dir: &'a Path,
    stash_creds: &'a Path,
    claude_home: &'a Path,
    claude_json: &'a Path,
    oauth_account: Option<&'a serde_json::Value>,
    clean: bool,
    copy: bool,
}

impl Seed<'_> {
    fn apply(&self) -> Result<()> {
        // Credentials: always re-seed from the stash (the single source of
        // truth, kept current by the sync-back below).
        let creds = fs::read(self.stash_creds)
            .with_context(|| format!("read {}", redact(self.stash_creds)))?;
        let creds_dst = self.dir.join(CREDS_FILE);
        atomic_write(&creds_dst, &creds, Some(0o600))
            .with_context(|| format!("write {}", redact(&creds_dst)))?;

        // .claude.json: start from the user's global copy (projects, onboarding
        // flags) then patch oauthAccount to this account's identity. Clear any
        // pre-existing symlink first so the copy can't be redirected elsewhere.
        let claude_json_dst = self.dir.join(".claude.json");
        replace_path(&claude_json_dst)?;
        if self.claude_json.exists() {
            fs::copy(self.claude_json, &claude_json_dst).with_context(|| {
                format!(
                    "copy {} -> {}",
                    redact(self.claude_json),
                    redact(&claude_json_dst)
                )
            })?;
        }
        if let Some(blob) = self.oauth_account {
            accounts::apply_oauth_account_to_config(&claude_json_dst, blob)?;
        }

        for f in ["settings.json", "settings.local.json"] {
            let src = self.claude_home.join(f);
            if src.exists() {
                let dst = self.dir.join(f);
                replace_path(&dst)?;
                fs::copy(&src, &dst)
                    .with_context(|| format!("copy {} -> {}", redact(&src), redact(&dst)))?;
            }
        }

        if self.clean {
            return Ok(());
        }

        for entry in SHARED_ENTRIES {
            let src = self.claude_home.join(entry);
            if src.symlink_metadata().is_err() {
                continue;
            }
            let dst = self.dir.join(entry);
            replace_path(&dst)?;
            if self.copy {
                copy_path(&src, &dst)?;
            } else {
                symlink(&src, &dst)
                    .with_context(|| format!("symlink {} -> {}", redact(&dst), redact(&src)))?;
            }
        }

        Ok(())
    }
}

/// Remove any existing file, symlink, or directory at `path` so the seed step
/// is idempotent for the reused per-account dir.
fn replace_path(path: &Path) -> Result<()> {
    match path.symlink_metadata() {
        Ok(meta) => {
            if meta.is_dir() && !meta.file_type().is_symlink() {
                fs::remove_dir_all(path).with_context(|| format!("remove {}", redact(path)))?;
            } else {
                fs::remove_file(path).with_context(|| format!("remove {}", redact(path)))?;
            }
            Ok(())
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(anyhow!("stat {}: {}", redact(path), e)),
    }
}

fn copy_path(src: &Path, dst: &Path) -> Result<()> {
    if src.is_dir() {
        copy_dir_recursive(src, dst)
    } else {
        fs::copy(src, dst)
            .map(|_| ())
            .with_context(|| format!("copy {} -> {}", redact(src), redact(dst)))
    }
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst).with_context(|| format!("create {}", redact(dst)))?;
    for entry in fs::read_dir(src).with_context(|| format!("read {}", redact(src)))? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else {
            fs::copy(&from, &to)
                .with_context(|| format!("copy {} -> {}", redact(&from), redact(&to)))?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn symlink(src: &Path, dst: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(src, dst)
}

#[cfg(not(unix))]
fn symlink(src: &Path, dst: &Path) -> io::Result<()> {
    copy_path(src, dst).map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))
}

// --- Launch + credential sync-back ---

fn launch_and_sync(
    dir: &Path,
    stash_creds: &Path,
    name: &str,
    expected_org: &str,
    command: &[String],
) -> Result<i32> {
    let (tx, rx) = mpsc::channel::<()>();
    // Watch the dir, not the file: Claude replaces .credentials.json via an
    // atomic rename, so a direct file watch would go stale after the first
    // rotation.
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(ev) = res {
            let touches_creds = ev
                .paths
                .iter()
                .any(|p| p.file_name().map(|n| n == CREDS_FILE).unwrap_or(false));
            if touches_creds
                && matches!(
                    ev.kind,
                    EventKind::Create(_) | EventKind::Modify(_) | EventKind::Any
                )
            {
                let _ = tx.send(());
            }
        }
    })
    .context("create credentials watcher")?;
    watcher
        .watch(dir, RecursiveMode::NonRecursive)
        .with_context(|| format!("watch {}", redact(dir)))?;

    let mut cmd = Command::new(&command[0]);
    cmd.args(&command[1..])
        .env("CLAUDE_CONFIG_DIR", dir)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    reset_child_signals(&mut cmd);

    // Ignore terminal signals in the parent so a Ctrl-C reaches only the
    // interactive child; we stay alive to reap it and run the mandatory
    // credentials sync-back before exiting. A SIGKILL can't be caught, so it
    // skips the final sync and a just-rotated token then lives only in `dir`.
    ignore_parent_signals();

    let spawn_creds = dir.join(CREDS_FILE);
    let claude_json = dir.join(".claude.json");
    let mut child = cmd
        .spawn()
        .with_context(|| format!("run '{}'", command[0]))?;

    let status = loop {
        if let Some(st) = child.try_wait().context("wait for command")? {
            break st;
        }
        match rx.recv_timeout(Duration::from_millis(400)) {
            Ok(()) => sync_back(&spawn_creds, &claude_json, stash_creds, name, expected_org),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    };

    drop(watcher);
    // Final sync: covers a token rotation that landed between the last watcher
    // event and the child exiting. Not optional: skipping it can leave the
    // stash holding an invalidated refresh token and brick the next spawn.
    sync_back(&spawn_creds, &claude_json, stash_creds, name, expected_org);

    Ok(status.code().unwrap_or(1))
}

enum Sync {
    Written,
    Unchanged,
    LoggedOut,
    AccountMismatch,
}

fn sync_back(spawn_creds: &Path, claude_json: &Path, stash: &Path, name: &str, expected_org: &str) {
    match try_sync_back(spawn_creds, claude_json, stash, expected_org) {
        Ok(Sync::Written) => {
            eprintln!("tku: synced refreshed credentials for '{name}' back to the stash")
        }
        Ok(Sync::Unchanged) => {}
        Ok(Sync::LoggedOut) => {
            eprintln!(
                "tku: warning: in-session credentials look logged-out; not syncing to the stash"
            )
        }
        Ok(Sync::AccountMismatch) => {
            eprintln!("tku: warning: in-session login changed account; not syncing to the stash")
        }
        Err(e) => eprintln!("tku: warning: could not sync credentials back to the stash: {e}"),
    }
}

fn try_sync_back(
    spawn_creds: &Path,
    claude_json: &Path,
    stash: &Path,
    expected_org: &str,
) -> Result<Sync> {
    let bytes = match fs::read(spawn_creds) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Sync::Unchanged),
        Err(e) => return Err(anyhow!("read {}: {}", redact(spawn_creds), e)),
    };
    // Reject a logged-out or half-written (torn read) blob before it can
    // clobber a good stash entry: a valid login carries a non-empty access +
    // refresh token and a positive expiry.
    let Some(login) = parse_login(&bytes) else {
        return Ok(Sync::LoggedOut);
    };
    let current = fs::read(stash).ok().and_then(|b| parse_login(&b));
    if current.map(|l| l.access_token) == Some(login.access_token.clone()) {
        return Ok(Sync::Unchanged);
    }
    // If the in-session identity witness disagrees with the account we seeded,
    // a /login as someone else happened inside the exec: don't overwrite this
    // account's stash. Absent field: best-effort, proceed.
    if let Some(org) = read_oauth_org(claude_json) {
        if org != expected_org {
            return Ok(Sync::AccountMismatch);
        }
    }
    atomic_write(stash, &bytes, Some(0o600)).with_context(|| format!("write {}", redact(stash)))?;
    Ok(Sync::Written)
}

struct Login {
    access_token: String,
}

/// Parse a credentials blob only if it is a complete, usable login. Guards
/// against syncing a logged-out or torn (half-written) file back to the stash.
fn parse_login(bytes: &[u8]) -> Option<Login> {
    let v: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let oauth = v.get("claudeAiOauth")?;
    let access = oauth
        .get("accessToken")
        .and_then(|t| t.as_str())
        .filter(|s| !s.is_empty())?;
    oauth
        .get("refreshToken")
        .and_then(|t| t.as_str())
        .filter(|s| !s.is_empty())?;
    oauth
        .get("expiresAt")
        .and_then(serde_json::Value::as_f64)
        .filter(|n| *n > 0.0)?;
    Some(Login {
        access_token: access.to_string(),
    })
}

/// `oauthAccount.organizationUuid` from a `.claude.json` at `path`, if present.
fn read_oauth_org(path: &Path) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    v.pointer("/oauthAccount/organizationUuid")
        .and_then(|o| o.as_str())
        .map(String::from)
}

// --- Signals ---

#[cfg(unix)]
fn ignore_parent_signals() {
    // SAFETY: SIG_IGN only changes signal disposition; no memory is touched.
    // The child restores SIG_DFL in pre_exec, so claude still handles Ctrl-C.
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_IGN);
        libc::signal(libc::SIGTERM, libc::SIG_IGN);
    }
}

#[cfg(not(unix))]
fn ignore_parent_signals() {}

#[cfg(unix)]
fn reset_child_signals(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: pre_exec runs in the forked child before exec. `signal` is
    // async-signal-safe and only restores default disposition so the launched
    // process handles terminal signals normally despite the parent ignoring
    // them.
    unsafe {
        cmd.pre_exec(|| {
            libc::signal(libc::SIGINT, libc::SIG_DFL);
            libc::signal(libc::SIGTERM, libc::SIG_DFL);
            Ok(())
        });
    }
}

#[cfg(not(unix))]
fn reset_child_signals(_cmd: &mut Command) {}

// --- Per-account advisory lock (pidfile + liveness check) ---

// A recycled PID could in theory keep a stale lock looking live; clear it by
// removing the lockfile at its printed path.
struct SpawnLock {
    path: PathBuf,
}

impl SpawnLock {
    /// O_EXCL pidfile lock. Returns `Ok(None)` when a live process already
    /// holds it; clears a stale lock left by a dead holder and retries.
    fn acquire(path: &Path) -> Result<Option<Self>> {
        for _ in 0..8 {
            match write_pidfile(path) {
                Ok(()) => {
                    return Ok(Some(Self {
                        path: path.to_path_buf(),
                    }))
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    match read_pid(path) {
                        Some(pid) if pid_alive(pid) => return Ok(None),
                        _ => {
                            // Stale or unreadable: clear it and retry.
                            let _ = fs::remove_file(path);
                        }
                    }
                }
                Err(e) => return Err(anyhow!("acquire lock {}: {}", redact(path), e)),
            }
        }
        bail!(
            "could not acquire the lock at {} after repeated stale-lock cleanup",
            redact(path)
        )
    }
}

impl Drop for SpawnLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn write_pidfile(path: &Path) -> io::Result<()> {
    use io::Write;
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    write!(f, "{}", std::process::id())?;
    Ok(())
}

fn read_pid(path: &Path) -> Option<u32> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    // SAFETY: `kill` with signal 0 performs permission/existence checks only;
    // it sends no signal and touches no memory.
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if rc == 0 {
        return true;
    }
    // EPERM: the process exists but we may not signal it (still alive).
    io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn pid_alive(_pid: u32) -> bool {
    true
}

// --- Ephemeral dir ---

struct EphemeralGuard {
    path: PathBuf,
    active: bool,
}

impl Drop for EphemeralGuard {
    fn drop(&mut self) {
        if self.active {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

fn make_ephemeral_dir(parent: &Path, name: &str) -> Result<PathBuf> {
    for attempt in 0..64 {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let cand = parent.join(format!(".{name}.{}.{attempt}.{nanos}", std::process::id()));
        match create_dir_excl(&cand) {
            Ok(()) => return Ok(cand),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(anyhow!("create {}: {}", redact(&cand), e)),
        }
    }
    bail!(
        "could not create a unique ephemeral config dir under {}",
        redact(parent)
    )
}

// --- Secure dir creation ---

/// Reject a spawn root that another user owns or that group/other can write to,
/// so a hostile pre-existing dir can't intercept seeded credentials.
#[cfg(unix)]
fn ensure_private_dir(path: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let meta = fs::symlink_metadata(path).with_context(|| format!("stat {}", redact(path)))?;
    // SAFETY: geteuid only reads the effective uid; it touches no memory.
    let uid = unsafe { libc::geteuid() };
    if meta.uid() != uid {
        bail!(
            "{} is not owned by the current user; refusing to seed credentials there",
            redact(path)
        );
    }
    if meta.mode() & 0o022 != 0 {
        bail!(
            "{} is group- or world-writable; refusing to seed credentials there",
            redact(path)
        );
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_private_dir(_path: &Path) -> Result<()> {
    Ok(())
}

fn create_dir_secure(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .mode(0o700)
            .recursive(true)
            .create(path)
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(path)
    }
}

fn create_dir_excl(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .mode(0o700)
            .recursive(false)
            .create(path)
    }
    #[cfg(not(unix))]
    {
        fs::create_dir(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "tku-spawn-test-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn guard_detects_live_account() {
        assert!(is_already_live("org-abc", Some("org-abc")));
        assert!(!is_already_live("org-abc", Some("org-xyz")));
        assert!(!is_already_live("org-abc", None));
    }

    #[test]
    fn empty_command_is_rejected() {
        // No command after `--`: error before touching the registry or spawning.
        let err = run("some-account", false, false, false, vec![]).unwrap_err();
        assert!(err.to_string().contains("no command given"));
    }

    #[test]
    fn seed_produces_creds_symlinks_and_patched_config() {
        let root = scratch("seed");
        let claude_home = root.join(".claude");
        fs::create_dir_all(claude_home.join("skills")).unwrap();
        fs::write(claude_home.join("skills").join("s.md"), b"skill").unwrap();
        fs::write(claude_home.join("CLAUDE.md"), b"global instructions").unwrap();
        fs::write(claude_home.join("settings.json"), b"{\"theme\":\"dark\"}").unwrap();

        let claude_json = root.join(".claude.json");
        fs::write(
            &claude_json,
            b"{\"projects\":{\"a\":1},\"oauthAccount\":{\"emailAddress\":\"old@x.io\"}}",
        )
        .unwrap();

        let stash = root.join("stash.credentials.json");
        fs::write(&stash, b"{\"claudeAiOauth\":{\"accessToken\":\"tok-1\"}}").unwrap();

        let blob = serde_json::json!({
            "emailAddress": "new@iota.org",
            "organizationUuid": "org-new",
        });

        let dir = root.join("D");
        fs::create_dir_all(&dir).unwrap();
        let seed = Seed {
            dir: &dir,
            stash_creds: &stash,
            claude_home: &claude_home,
            claude_json: &claude_json,
            oauth_account: Some(&blob),
            clean: false,
            copy: false,
        };
        seed.apply().unwrap();

        // Credentials copied verbatim from the stash.
        assert_eq!(
            fs::read(dir.join(CREDS_FILE)).unwrap(),
            fs::read(&stash).unwrap()
        );

        // .claude.json keeps other keys, oauthAccount replaced.
        let cfg: serde_json::Value =
            serde_json::from_slice(&fs::read(dir.join(".claude.json")).unwrap()).unwrap();
        assert_eq!(cfg.pointer("/projects/a").unwrap(), &serde_json::json!(1));
        assert_eq!(
            cfg.pointer("/oauthAccount/emailAddress").unwrap(),
            "new@iota.org"
        );

        // Shared entries symlinked; settings copied.
        let skills = dir.join("skills");
        assert!(skills.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(fs::read_link(&skills).unwrap(), claude_home.join("skills"));
        assert!(dir
            .join("CLAUDE.md")
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(dir.join("settings.json").exists());
        assert!(!dir
            .join("settings.json")
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink());

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn clean_skips_shared_symlinks() {
        let root = scratch("clean");
        let claude_home = root.join(".claude");
        fs::create_dir_all(claude_home.join("skills")).unwrap();
        fs::write(claude_home.join("CLAUDE.md"), b"x").unwrap();
        let claude_json = root.join(".claude.json");
        let stash = root.join("stash.credentials.json");
        fs::write(&stash, b"{\"claudeAiOauth\":{\"accessToken\":\"tok\"}}").unwrap();
        let blob = serde_json::json!({"emailAddress": "a@b.io"});

        let dir = root.join("D");
        fs::create_dir_all(&dir).unwrap();
        Seed {
            dir: &dir,
            stash_creds: &stash,
            claude_home: &claude_home,
            claude_json: &claude_json,
            oauth_account: Some(&blob),
            clean: true,
            copy: false,
        }
        .apply()
        .unwrap();

        assert!(dir.join(CREDS_FILE).exists());
        assert!(dir.join(".claude.json").exists());
        assert!(dir.join("skills").symlink_metadata().is_err());
        assert!(dir.join("CLAUDE.md").symlink_metadata().is_err());

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn seed_is_idempotent_and_copy_mode_works() {
        let root = scratch("idem");
        let claude_home = root.join(".claude");
        fs::create_dir_all(claude_home.join("agents")).unwrap();
        fs::write(claude_home.join("agents").join("a.md"), b"agent").unwrap();
        let claude_json = root.join(".claude.json");
        let stash = root.join("stash.credentials.json");
        fs::write(&stash, b"{\"claudeAiOauth\":{\"accessToken\":\"tok\"}}").unwrap();
        let blob = serde_json::json!({"emailAddress": "a@b.io"});

        let dir = root.join("D");
        fs::create_dir_all(&dir).unwrap();
        let mk = |copy: bool| Seed {
            dir: &dir,
            stash_creds: &stash,
            claude_home: &claude_home,
            claude_json: &claude_json,
            oauth_account: Some(&blob),
            clean: false,
            copy,
        };
        // First seed as symlink, second seed as copy over the top must replace
        // cleanly without error.
        mk(false).apply().unwrap();
        mk(true).apply().unwrap();

        let agents = dir.join("agents");
        assert!(!agents.symlink_metadata().unwrap().file_type().is_symlink());
        assert!(agents.join("a.md").exists());

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn lock_blocks_second_holder_and_clears_stale() {
        let root = scratch("lock");
        let path = root.join("acct.lock");

        let held = SpawnLock::acquire(&path).unwrap().unwrap();
        // Second acquire while the first is live (our own PID) is refused.
        assert!(SpawnLock::acquire(&path).unwrap().is_none());
        drop(held);
        assert!(!path.exists());

        // A stale pidfile with a dead PID is cleared and re-acquired.
        fs::write(&path, "2147483646").unwrap();
        let re = SpawnLock::acquire(&path).unwrap();
        assert!(re.is_some());

        fs::remove_dir_all(&root).unwrap();
    }

    fn login_blob(token: &str) -> String {
        format!(
            "{{\"claudeAiOauth\":{{\"accessToken\":\"{token}\",\
             \"refreshToken\":\"r-{token}\",\"expiresAt\":9999999999}}}}"
        )
    }

    fn claude_json_with_org(org: &str) -> String {
        format!("{{\"oauthAccount\":{{\"organizationUuid\":\"{org}\"}}}}")
    }

    #[test]
    fn resolve_live_org_prefers_strongest_witness() {
        // Creds file wins over both fallbacks.
        assert_eq!(
            resolve_live_org(Some("a".into()), Some("b".into()), Some("c".into())),
            Some("a".into())
        );
        // No creds field: ~/.claude.json oauthAccount org is preferred over the
        // switch log.
        assert_eq!(
            resolve_live_org(None, Some("b".into()), Some("c".into())),
            Some("b".into())
        );
        // Only the switch log remains.
        assert_eq!(
            resolve_live_org(None, None, Some("c".into())),
            Some("c".into())
        );
        assert_eq!(resolve_live_org(None, None, None), None);
    }

    #[test]
    fn read_oauth_org_parses_claude_json() {
        let root = scratch("org");
        let cj = root.join(".claude.json");
        fs::write(&cj, claude_json_with_org("org-live")).unwrap();
        assert_eq!(read_oauth_org(&cj).as_deref(), Some("org-live"));

        // Missing field / file: None (best-effort, sync proceeds).
        fs::write(&cj, b"{\"projects\":{}}").unwrap();
        assert!(read_oauth_org(&cj).is_none());
        assert!(read_oauth_org(&root.join("nope.json")).is_none());

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn sync_back_syncs_only_complete_rotated_login() {
        let root = scratch("sync");
        let spawn_creds = root.join(CREDS_FILE);
        let cj = root.join(".claude.json");
        let stash = root.join("stash.json");
        fs::write(&stash, login_blob("old")).unwrap();
        // Matching account so the org check passes.
        fs::write(&cj, claude_json_with_org("org-1")).unwrap();

        // Unchanged token: no write.
        fs::write(&spawn_creds, login_blob("old")).unwrap();
        assert!(matches!(
            try_sync_back(&spawn_creds, &cj, &stash, "org-1").unwrap(),
            Sync::Unchanged
        ));

        // Complete rotated login: mirrored back.
        fs::write(&spawn_creds, login_blob("new")).unwrap();
        assert!(matches!(
            try_sync_back(&spawn_creds, &cj, &stash, "org-1").unwrap(),
            Sync::Written
        ));
        assert_eq!(
            parse_login(&fs::read(&stash).unwrap())
                .unwrap()
                .access_token,
            "new"
        );

        // Missing spawn creds: no-op.
        fs::remove_file(&spawn_creds).unwrap();
        assert!(matches!(
            try_sync_back(&spawn_creds, &cj, &stash, "org-1").unwrap(),
            Sync::Unchanged
        ));

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn sync_back_rejects_logged_out_or_partial_blob() {
        let root = scratch("logout");
        let spawn_creds = root.join(CREDS_FILE);
        let cj = root.join(".claude.json");
        let stash = root.join("stash.json");
        fs::write(&stash, login_blob("good")).unwrap();
        fs::write(&cj, claude_json_with_org("org-1")).unwrap();

        let bad = [
            // empty access token
            "{\"claudeAiOauth\":{\"accessToken\":\"\",\"refreshToken\":\"r\",\"expiresAt\":1}}",
            // missing refresh token
            "{\"claudeAiOauth\":{\"accessToken\":\"a\",\"expiresAt\":1}}",
            // empty refresh token
            "{\"claudeAiOauth\":{\"accessToken\":\"a\",\"refreshToken\":\"\",\"expiresAt\":1}}",
            // zero expiry
            "{\"claudeAiOauth\":{\"accessToken\":\"a\",\"refreshToken\":\"r\",\"expiresAt\":0}}",
            // torn / non-JSON
            "{\"claudeAiOauth\":{\"accessToke",
        ];
        for blob in bad {
            fs::write(&spawn_creds, blob).unwrap();
            assert!(matches!(
                try_sync_back(&spawn_creds, &cj, &stash, "org-1").unwrap(),
                Sync::LoggedOut
            ));
        }
        // Stash untouched.
        assert_eq!(
            parse_login(&fs::read(&stash).unwrap())
                .unwrap()
                .access_token,
            "good"
        );

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn sync_back_skips_on_account_mismatch() {
        let root = scratch("mismatch");
        let spawn_creds = root.join(CREDS_FILE);
        let cj = root.join(".claude.json");
        let stash = root.join("stash.json");
        fs::write(&stash, login_blob("old")).unwrap();
        fs::write(&spawn_creds, login_blob("new")).unwrap();

        // Sibling .claude.json says a different account is logged in now.
        fs::write(&cj, claude_json_with_org("org-other")).unwrap();
        assert!(matches!(
            try_sync_back(&spawn_creds, &cj, &stash, "org-1").unwrap(),
            Sync::AccountMismatch
        ));
        // Stash preserved.
        assert_eq!(
            parse_login(&fs::read(&stash).unwrap())
                .unwrap()
                .access_token,
            "old"
        );

        // Matching org: proceeds.
        fs::write(&cj, claude_json_with_org("org-1")).unwrap();
        assert!(matches!(
            try_sync_back(&spawn_creds, &cj, &stash, "org-1").unwrap(),
            Sync::Written
        ));

        // Absent org field: best-effort proceed.
        fs::write(&stash, login_blob("old")).unwrap();
        fs::write(&cj, b"{\"projects\":{}}").unwrap();
        assert!(matches!(
            try_sync_back(&spawn_creds, &cj, &stash, "org-1").unwrap(),
            Sync::Written
        ));

        fs::remove_dir_all(&root).unwrap();
    }
}
