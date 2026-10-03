//! Claude Code's OAuth credentials: `.credentials.json` in the config dir, or
//! a generic-password item in the login Keychain on macOS.

use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use directories::BaseDirs;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::accounts::redact;
use crate::atomic_write::atomic_write;
use crate::scrub::fingerprint::to_hex;

pub const CREDS_FILE: &str = ".credentials.json";

const DEFAULT_SERVICE: &str = "Claude Code-credentials";
const SECURITY: &str = "/usr/bin/security";
const FALLBACK_ACCOUNT: &str = "claude-code-user";
const OAUTH_KEY: &str = "claudeAiOauth";
const MAX_INTERACTIVE_LINE: usize = 4032;
const TIMEOUT: Duration = Duration::from_secs(30);

pub enum Store {
    File(PathBuf),
    Keychain(Keychain),
}

pub struct Keychain {
    pub(crate) program: PathBuf,
    pub(crate) account: String,
    pub(crate) service: String,
    pub(crate) file: PathBuf,
    pub(crate) timeout: Duration,
}

#[derive(Debug)]
struct Locked;

impl fmt::Display for Locked {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("macOS Keychain is locked; run `security unlock-keychain` and retry")
    }
}

impl std::error::Error for Locked {}

pub fn is_locked(err: &anyhow::Error) -> bool {
    err.downcast_ref::<Locked>().is_some()
}

/// The store Claude Code uses for `~/.claude`.
pub fn live() -> Result<Store> {
    let base = BaseDirs::new().ok_or_else(|| anyhow!("cannot determine home directory"))?;
    Ok(at(&base.home_dir().join(".claude"), service_for(None)))
}

/// The store Claude Code uses when `CLAUDE_CONFIG_DIR` is `dir`.
pub fn isolated(dir: &Path) -> Store {
    at(dir, service_for(Some(dir.as_os_str())))
}

/// Keychain service name Claude Code derives from `CLAUDE_CONFIG_DIR`.
pub fn service_for(config_dir: Option<&OsStr>) -> String {
    match config_dir {
        Some(dir) if !dir.is_empty() => {
            let hash = to_hex(&Sha256::digest(dir.as_encoded_bytes()));
            format!("{DEFAULT_SERVICE}-{}", &hash[..8])
        }
        _ => DEFAULT_SERVICE.to_string(),
    }
}

fn at(dir: &Path, service: String) -> Store {
    if cfg!(target_os = "macos") {
        Store::Keychain(Keychain::for_dir(dir, service))
    } else {
        Store::File(dir.join(CREDS_FILE))
    }
}

impl Store {
    /// Current credentials blob; `None` means no login.
    pub fn read(&self) -> Result<Option<Vec<u8>>> {
        match self {
            Store::File(path) => read_file(path),
            Store::Keychain(k) => k.read(),
        }
    }

    /// Replace `claudeAiOauth` in the current blob with the one in `stash`.
    pub fn write(&self, stash: &[u8]) -> Result<()> {
        match self {
            Store::File(path) => {
                let merged = merge_oauth(self.read()?.as_deref(), stash)?;
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)
                        .with_context(|| format!("cannot create {}", redact(parent)))?;
                }
                atomic_write(path, &merged, Some(0o600))
                    .with_context(|| format!("cannot write {}", redact(path)))
            }
            Store::Keychain(k) => k.write(stash),
        }
    }

    pub fn delete(&self) -> Result<()> {
        match self {
            Store::File(path) => remove_file(path),
            Store::Keychain(k) => k.delete(),
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Store::File(path) => redact(path),
            Store::Keychain(k) => format!("Keychain item \"{}\"", k.service),
        }
    }

    pub fn file_path(&self) -> Option<&Path> {
        match self {
            Store::File(path) => Some(path),
            Store::Keychain(_) => None,
        }
    }
}

impl Keychain {
    fn for_dir(dir: &Path, service: String) -> Self {
        Keychain {
            program: PathBuf::from(SECURITY),
            account: current_account(),
            service,
            file: dir.join(CREDS_FILE),
            timeout: TIMEOUT,
        }
    }

    fn read(&self) -> Result<Option<Vec<u8>>> {
        match self.item()? {
            Some(blob) => Ok(Some(blob)),
            None => read_file(&self.file),
        }
    }

    fn item(&self) -> Result<Option<Vec<u8>>> {
        let out = self.run_security(
            &[
                "find-generic-password",
                "-a",
                &self.account,
                "-w",
                "-s",
                &self.service,
            ],
            None,
        )?;
        if !self.found(&out)? {
            return Ok(None);
        }
        let mut blob = out.stdout;
        if blob.last() == Some(&b'\n') {
            blob.pop();
        }
        // `security -w` prints hex when any byte fails isprint(), which covers non-ASCII UTF-8.
        Ok(Some(decode_hex(&blob).unwrap_or(blob)))
    }

    fn write(&self, stash: &[u8]) -> Result<()> {
        let merged = merge_oauth(self.read()?.as_deref(), stash)?;
        let hex = to_hex(&merged);
        let line = format!(
            "add-generic-password -U -a \"{}\" -s \"{}\" -X \"{hex}\"",
            self.account, self.service
        );
        // Over the `-i` line limit the hex goes on argv, visible to `ps` for the call's duration.
        let out = if line.len() <= MAX_INTERACTIVE_LINE {
            let mut input = line.into_bytes();
            input.push(b'\n');
            self.run_security(&["-i"], Some(&input))?
        } else {
            self.run_security(
                &[
                    "add-generic-password",
                    "-U",
                    "-a",
                    &self.account,
                    "-s",
                    &self.service,
                    "-X",
                    &hex,
                ],
                None,
            )?
        };
        if !self.found(&out)? {
            bail!(
                "{} add-generic-password reported item not found",
                self.program.display()
            );
        }
        match self.item()? {
            Some(stored) if stored == merged => {}
            Some(_) => bail!(
                "Keychain item \"{}\" does not match what was written",
                self.service
            ),
            None => bail!("Keychain item \"{}\" is missing after write", self.service),
        }
        remove_file(&self.file)
    }

    fn delete(&self) -> Result<()> {
        let out = self.run_security(
            &[
                "delete-generic-password",
                "-a",
                &self.account,
                "-s",
                &self.service,
            ],
            None,
        )?;
        self.found(&out)?;
        remove_file(&self.file)
    }

    /// `Ok(true)` on exit 0, `Ok(false)` on exit 44 (item not found).
    fn found(&self, out: &Output) -> Result<bool> {
        match out.status.code() {
            Some(0) => Ok(true),
            Some(44) => Ok(false),
            Some(36) => Err(Locked.into()),
            _ => bail!(
                "{} failed ({}): {}",
                self.program.display(),
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        }
    }

    // Polled with a deadline: exec ignores SIGINT in the parent and the child inherits it,
    // so a pending Keychain prompt could not otherwise be interrupted.
    fn run_security(&self, args: &[&str], stdin: Option<&[u8]>) -> Result<Output> {
        let mut child = Command::new(&self.program)
            .args(args)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("cannot run {}", self.program.display()))?;

        if let Some(input) = stdin {
            let written = match child.stdin.take() {
                Some(mut pipe) => pipe.write_all(input),
                None => Err(ErrorKind::BrokenPipe.into()),
            };
            if let Err(e) = written {
                let _ = child.kill();
                let _ = child.wait();
                return Err(e)
                    .with_context(|| format!("cannot write to {}", self.program.display()));
            }
        }

        let deadline = Instant::now() + self.timeout;
        loop {
            if child.try_wait()?.is_some() {
                return Ok(child.wait_with_output()?);
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                bail!(
                    "timed out waiting for {} (a Keychain prompt may be open)",
                    self.program.display()
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

fn current_account() -> String {
    resolve_account(std::env::var("USER").ok(), || {
        let out = Command::new("/usr/bin/id")
            .arg("-un")
            .stderr(Stdio::null())
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        String::from_utf8(out.stdout)
            .ok()
            .map(|s| s.trim_end().to_string())
    })
}

fn resolve_account(user: Option<String>, os_user: impl FnOnce() -> Option<String>) -> String {
    let name = user
        .filter(|u| !u.is_empty())
        .or_else(|| os_user().filter(|u| !u.is_empty()));
    match name {
        Some(n)
            if n.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-')) =>
        {
            n
        }
        _ => FALLBACK_ACCOUNT.to_string(),
    }
}

fn merge_oauth(current: Option<&[u8]>, stash: &[u8]) -> Result<Vec<u8>> {
    let parsed: Value =
        serde_json::from_slice(stash).context("stashed credentials are not valid JSON")?;
    let oauth = parsed
        .get(OAUTH_KEY)
        .ok_or_else(|| anyhow!("stashed credentials have no {OAUTH_KEY}"))?;
    match current.and_then(|c| serde_json::from_slice::<Value>(c).ok()) {
        Some(Value::Object(mut map)) => {
            map.insert(OAUTH_KEY.to_string(), oauth.clone());
            Ok(serde_json::to_vec(&Value::Object(map))?)
        }
        _ => Ok(stash.to_vec()),
    }
}

fn decode_hex(s: &[u8]) -> Option<Vec<u8>> {
    if s.is_empty() || !s.len().is_multiple_of(2) || !s.iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    let nibble = |c: u8| match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        _ => c - b'A' + 10,
    };
    let (pairs, _) = s.as_chunks::<2>();
    Some(
        pairs
            .iter()
            .map(|p| (nibble(p[0]) << 4) | nibble(p[1]))
            .collect(),
    )
}

fn read_file(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(b) => Ok(Some(b)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", redact(path))),
    }
}

fn remove_file(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("cannot remove {}", redact(path))),
    }
}

#[cfg(test)]
pub(crate) mod fake {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    /// Held by every test that executes a fake program.
    pub(crate) static LOCK: Mutex<()> = Mutex::new(());

    // Builtins only: the test shell has no coreutils.
    const SECURITY: &str = r#"#!/bin/sh
d=${0%/*}
n=0
[ -f "$d/calls" ] && read -r n < "$d/calls"
n=$((n + 1))
printf '%s\n' "$n" > "$d/calls"
: > "$d/argv.$n"
for a in "$@"; do printf '%s\n' "$a" >> "$d/argv.$n"; done
l=
if [ "$1" = "-i" ]; then
  { while IFS= read -r l; do printf '%s\n' "$l"; done; printf '%s' "$l"; } > "$d/stdin.$n"
else
  : > "$d/stdin.$n"
fi
if [ -f "$d/out.$n" ]; then
  l=
  while IFS= read -r l; do printf '%s\n' "$l"; done < "$d/out.$n"
  printf '%s' "$l"
fi
if [ -f "$d/err.$n" ]; then
  l=
  { while IFS= read -r l; do printf '%s\n' "$l"; done < "$d/err.$n"; printf '%s' "$l"; } >&2
fi
c=0
[ -f "$d/code.$n" ] && read -r c < "$d/code.$n"
exit "$c"
"#;

    /// Fake `security` in `dir`, driven by `out.N`, `err.N`, `code.N` and
    /// recording `argv.N` and `stdin.N` for the Nth call.
    pub(crate) fn security(dir: &Path) -> PathBuf {
        script(dir, "security", SECURITY)
    }

    #[cfg(unix)]
    pub(crate) fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        fs::write(&path, body).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "tku-creds-test-{}-{}-{}",
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

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        fake::LOCK.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn keychain(dir: &Path, account: &str) -> Keychain {
        Keychain {
            program: fake::security(dir),
            account: account.to_string(),
            service: DEFAULT_SERVICE.to_string(),
            file: dir.join(CREDS_FILE),
            timeout: TIMEOUT,
        }
    }

    fn put(dir: &Path, name: &str, contents: impl AsRef<[u8]>) {
        fs::write(dir.join(name), contents).unwrap();
    }

    fn argv(dir: &Path, n: u32) -> Vec<String> {
        fs::read_to_string(dir.join(format!("argv.{n}")))
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn stdin(dir: &Path, n: u32) -> Vec<u8> {
        fs::read(dir.join(format!("stdin.{n}"))).unwrap()
    }

    fn calls(dir: &Path) -> u32 {
        fs::read_to_string(dir.join("calls"))
            .map(|s| s.trim().parse().unwrap())
            .unwrap_or(0)
    }

    fn add_line(account: &str, hex: &str) -> String {
        format!("add-generic-password -U -a \"{account}\" -s \"{DEFAULT_SERVICE}\" -X \"{hex}\"")
    }

    /// Account and stash whose `-i` line is exactly `len` bytes. Hex is always
    /// even, so the account length sets the parity.
    fn sized(len: usize) -> (String, Vec<u8>) {
        let shell =
            |filler: usize| format!("{{\"claudeAiOauth\":{{\"t\":\"{}\"}}}}", "x".repeat(filler));
        for account in ["u", "uu"] {
            let overhead = add_line(account, "").len() + 2 * shell(0).len();
            if len >= overhead && (len - overhead) % 2 == 0 {
                let stash = shell((len - overhead) / 2).into_bytes();
                assert_eq!(add_line(account, &to_hex(&stash)).len(), len);
                return (account.to_string(), stash);
            }
        }
        unreachable!("one of the accounts has the right parity")
    }

    const STASH: &[u8] = br#"{"claudeAiOauth":{"a":1}}"#;

    #[test]
    fn for_dir_builds_production_keychain() {
        let dir = Path::new("/some/dir");
        let k = Keychain::for_dir(dir, "svc-x".to_string());
        assert_eq!(k.program, Path::new("/usr/bin/security"));
        assert_eq!(k.service, "svc-x");
        assert_eq!(k.file, dir.join(".credentials.json"));
        assert_eq!(k.account, current_account());
        assert_eq!(k.timeout, Duration::from_secs(30));
    }

    #[test]
    fn service_for_hashes_config_dir() {
        assert_eq!(service_for(None), "Claude Code-credentials");
        assert_eq!(service_for(Some(OsStr::new(""))), "Claude Code-credentials");
        assert_eq!(
            service_for(Some(OsStr::new("abc"))),
            "Claude Code-credentials-ba7816bf"
        );
        assert_eq!(
            service_for(Some(OsStr::new("/tmp/x"))),
            "Claude Code-credentials-2e56aa36"
        );
        assert_eq!(
            service_for(Some(OsStr::new("/Users/me/.claude/"))),
            "Claude Code-credentials-8586dac6"
        );
        assert_eq!(
            service_for(Some(OsStr::new("/Users/me/.claude"))),
            "Claude Code-credentials-0bd83d07"
        );
    }

    #[test]
    fn resolve_account_prefers_valid_user() {
        assert_eq!(
            resolve_account(Some("al.ice_1-x".into()), || Some("bob".into())),
            "al.ice_1-x"
        );
        assert_eq!(
            resolve_account(Some(String::new()), || Some("bob".into())),
            "bob"
        );
        assert_eq!(resolve_account(None, || Some("bob".into())), "bob");
        assert_eq!(
            resolve_account(Some("a b".into()), || panic!("must not be consulted")),
            "claude-code-user"
        );
        assert_eq!(
            resolve_account(None, || Some("b@d".into())),
            "claude-code-user"
        );
        assert_eq!(resolve_account(None, || None), "claude-code-user");
    }

    #[test]
    fn is_locked_matches_exit_36_only() {
        let _g = lock();
        let dir = scratch("locked");
        let k = keychain(&dir, "u");
        put(&dir, "code.1", "36");
        put(&dir, "code.2", "51");
        let locked = k.read().unwrap_err();
        let other = k.read().unwrap_err();
        assert!(is_locked(&locked));
        assert!(!is_locked(&other));
        assert!(!is_locked(&anyhow!("plain")));
    }

    #[test]
    fn keychain_read_strips_one_newline() {
        let _g = lock();
        let dir = scratch("read-nl");
        let k = keychain(&dir, "u");
        put(&dir, "out.1", b"{\"a\":1}\n");
        assert_eq!(k.read().unwrap().unwrap(), b"{\"a\":1}");
        assert_eq!(
            argv(&dir, 1),
            [
                "find-generic-password",
                "-a",
                "u",
                "-w",
                "-s",
                DEFAULT_SERVICE
            ]
        );
    }

    #[test]
    fn keychain_read_without_newline_is_unchanged() {
        let _g = lock();
        let dir = scratch("read-raw");
        let k = keychain(&dir, "u");
        put(&dir, "out.1", b"{\"a\":1}");
        assert_eq!(k.read().unwrap().unwrap(), b"{\"a\":1}");
    }

    #[test]
    fn keychain_read_decodes_hex() {
        let _g = lock();
        let dir = scratch("read-hex");
        let k = keychain(&dir, "u");
        let blob = "{\"name\":\"é\"}".as_bytes();
        put(&dir, "out.1", format!("{}\n", to_hex(blob)));
        assert_eq!(k.read().unwrap().unwrap(), blob);
    }

    #[test]
    fn keychain_read_missing_item_falls_back_to_file() {
        let _g = lock();
        let dir = scratch("read-44");
        let k = keychain(&dir, "u");
        put(&dir, "code.1", "44");
        put(&dir, "code.2", "44");
        assert_eq!(k.read().unwrap(), None);
        put(&dir, CREDS_FILE, STASH);
        assert_eq!(k.read().unwrap().unwrap(), STASH);
    }

    #[test]
    fn keychain_read_errors() {
        let _g = lock();
        let dir = scratch("read-err");
        let k = keychain(&dir, "u");
        put(&dir, "code.1", "36");
        put(&dir, "code.2", "51");
        put(&dir, "err.2", "boom\n");
        assert!(k.read().unwrap_err().to_string().contains("locked"));
        assert!(k.read().unwrap_err().to_string().contains("boom"));
    }

    #[test]
    fn keychain_write_at_limit_uses_stdin() {
        let _g = lock();
        let dir = scratch("write-4032");
        let (account, stash) = sized(4032);
        let k = keychain(&dir, &account);
        put(&dir, "code.1", "44");
        let mut back = stash.clone();
        back.push(b'\n');
        put(&dir, "out.3", &back);
        k.write(&stash).unwrap();

        let hex = to_hex(&stash);
        let line = add_line(&account, &hex);
        assert_eq!(line.len(), 4032);
        assert_eq!(argv(&dir, 2), ["-i"]);
        assert_eq!(stdin(&dir, 2), format!("{line}\n").into_bytes());
        assert_eq!(decode_hex(hex.as_bytes()).unwrap(), stash);
        assert_eq!(argv(&dir, 3)[0], "find-generic-password");
        assert_eq!(calls(&dir), 3);
    }

    #[test]
    fn keychain_write_over_limit_uses_argv() {
        let _g = lock();
        let dir = scratch("write-4033");
        let (account, stash) = sized(4033);
        let k = keychain(&dir, &account);
        put(&dir, "code.1", "44");
        put(&dir, "out.3", &stash);
        k.write(&stash).unwrap();

        let hex = to_hex(&stash);
        let args = argv(&dir, 2);
        assert_eq!(
            args,
            [
                "add-generic-password",
                "-U",
                "-a",
                &account,
                "-s",
                DEFAULT_SERVICE,
                "-X",
                &hex
            ]
        );
        assert!(args.iter().all(|a| !a.contains('"')));
        assert!(stdin(&dir, 2).is_empty());
        assert_eq!(argv(&dir, 3)[0], "find-generic-password");
    }

    #[test]
    fn keychain_write_mismatched_readback_fails() {
        let _g = lock();
        let dir = scratch("write-mismatch");
        let k = keychain(&dir, "u");
        put(&dir, "code.1", "44");
        put(&dir, "out.3", b"{\"claudeAiOauth\":{\"a\":2}}\n");
        let err = k.write(STASH).unwrap_err();
        assert!(err.to_string().contains("does not match"), "{err}");
    }

    #[test]
    fn keychain_write_missing_readback_ignores_file() {
        let _g = lock();
        let dir = scratch("write-readback-44");
        let k = keychain(&dir, "u");
        put(&dir, CREDS_FILE, STASH);
        put(&dir, "code.1", "44");
        put(&dir, "code.3", "44");
        assert!(k.write(STASH).is_err());
        assert_eq!(argv(&dir, 3)[0], "find-generic-password");
        assert_eq!(calls(&dir), 3);
        assert_eq!(fs::read(dir.join(CREDS_FILE)).unwrap(), STASH);
    }

    #[test]
    fn keychain_write_success_removes_file() {
        let _g = lock();
        let dir = scratch("write-ok");
        let k = keychain(&dir, "u");
        put(&dir, CREDS_FILE, STASH);
        put(&dir, "code.1", "44");
        put(&dir, "out.3", STASH);
        k.write(STASH).unwrap();
        assert!(!dir.join(CREDS_FILE).exists());
    }

    #[test]
    fn keychain_write_add_failure_keeps_file() {
        let _g = lock();
        let dir = scratch("write-add-err");
        let k = keychain(&dir, "u");
        put(&dir, CREDS_FILE, STASH);
        put(&dir, "code.1", "44");
        put(&dir, "code.2", "36");
        put(&dir, "code.3", "44");
        put(&dir, "code.4", "1");
        assert!(is_locked(&k.write(STASH).unwrap_err()));
        assert!(!is_locked(&k.write(STASH).unwrap_err()));
        assert_eq!(calls(&dir), 4);
        assert_eq!(fs::read(dir.join(CREDS_FILE)).unwrap(), STASH);
    }

    #[test]
    fn keychain_delete_missing_item_removes_file() {
        let _g = lock();
        let dir = scratch("delete");
        let k = keychain(&dir, "u");
        put(&dir, CREDS_FILE, STASH);
        put(&dir, "code.1", "44");
        Store::Keychain(k).delete().unwrap();
        assert_eq!(
            argv(&dir, 1),
            ["delete-generic-password", "-a", "u", "-s", DEFAULT_SERVICE]
        );
        assert!(!dir.join(CREDS_FILE).exists());
    }

    #[test]
    fn merge_replaces_only_oauth() {
        let current = br#"{"claudeAiOauth":{"old":1},"mcpOAuth":{"k":"v"}}"#;
        let stash = br#"{"claudeAiOauth":{"new":2},"other":3}"#;
        let merged: Value =
            serde_json::from_slice(&merge_oauth(Some(current), stash).unwrap()).unwrap();
        assert_eq!(
            merged,
            serde_json::json!({"claudeAiOauth": {"new": 2}, "mcpOAuth": {"k": "v"}})
        );
    }

    #[test]
    fn merge_without_usable_current_returns_stash() {
        let stash = b"{ \"claudeAiOauth\" : {\"a\":1} }";
        assert_eq!(merge_oauth(None, stash).unwrap(), stash);
        assert_eq!(merge_oauth(Some(b"not json"), stash).unwrap(), stash);
        assert_eq!(merge_oauth(Some(b"[1]"), stash).unwrap(), stash);
    }

    #[test]
    fn merge_requires_oauth_in_stash() {
        assert!(merge_oauth(None, br#"{"other":1}"#).is_err());
    }

    #[test]
    fn file_store_round_trip() {
        let dir = scratch("file");
        let path = dir.join("nested").join(CREDS_FILE);
        let store = Store::File(path.clone());
        assert_eq!(store.read().unwrap(), None);
        assert_eq!(store.file_path(), Some(path.as_path()));

        store
            .write(br#"{"claudeAiOauth":{"a":1},"keep":true}"#)
            .unwrap();
        store.write(br#"{"claudeAiOauth":{"a":2}}"#).unwrap();
        let got: Value = serde_json::from_slice(&store.read().unwrap().unwrap()).unwrap();
        assert_eq!(
            got,
            serde_json::json!({"claudeAiOauth": {"a": 2}, "keep": true})
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        store.delete().unwrap();
        store.delete().unwrap();
        assert_eq!(store.read().unwrap(), None);
    }

    #[test]
    fn keychain_describe_names_service() {
        let store = Store::Keychain(Keychain {
            program: PathBuf::from("/nonexistent"),
            account: "u".into(),
            service: "svc".into(),
            file: PathBuf::from("/d").join(CREDS_FILE),
            timeout: TIMEOUT,
        });
        assert_eq!(store.describe(), "Keychain item \"svc\"");
        assert_eq!(store.file_path(), None);
    }

    #[test]
    fn run_security_times_out() {
        let _g = lock();
        let dir = scratch("timeout");
        let mut k = keychain(&dir, "u");
        k.program = fake::script(&dir, "spin", "#!/bin/sh\nwhile :; do :; done\n");
        k.timeout = Duration::from_millis(50);
        let err = k.read().unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err}");
    }
}
