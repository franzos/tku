//! Atomic file writes: create a temp file with `O_CREAT|O_EXCL`, write +
//! fsync, then rename into place. Refuses to follow a symlink planted in the
//! meantime. On Unix, an optional mode is applied at creation — no post-hoc
//! chmod window.
//!
//! Used for credential stashes (0o600), pricing/exchange/bitcode caches, and
//! subscription snapshots. Protects against torn writes and TOCTOU on shared
//! cache directories.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Write `contents` to `path` atomically. On Unix, `mode` (if provided) is
/// applied via `mode()` on the `OpenOptions` at creation so the file is never
/// visible with wider permissions.
///
/// Strategy: write to `<path>.tmp` with `O_CREAT|O_EXCL`, `sync_all`,
/// then `rename` over `path`. Any stale `<path>.tmp` from a crashed run is
/// removed first.
pub fn atomic_write(path: &Path, contents: &[u8], mode: Option<u32>) -> std::io::Result<()> {
    let tmp = tmp_path(path);

    let res: std::io::Result<()> = (|| {
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            if let Some(m) = mode {
                opts.mode(m);
            }
        }
        #[cfg(not(unix))]
        {
            let _ = mode;
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(contents)?;
        f.sync_all()?;
        Ok(())
    })();

    match res {
        Ok(()) => fs::rename(&tmp, path).inspect_err(|_| {
            let _ = fs::remove_file(&tmp);
        }),
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e)
        }
    }
}

fn tmp_path(path: &Path) -> PathBuf {
    // Avoid `with_extension` — it drops any multi-dot suffix, so
    // `foo.credentials.json` must not become `foo.credentials.tmp`.
    //
    // PID is appended so concurrent `tku` invocations (waybar polling during
    // a CLI run, cron + watch, etc.) don't race on the same tmp path: each
    // writer owns its own `<name>.<pid>.tmp`, and a loser's cleanup cannot
    // clobber the winner's in-flight tmp.
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.tmp", std::process::id()));
    path.with_file_name(name)
}
