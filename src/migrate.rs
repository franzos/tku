//! Upgrades of tku's on-disk data, run before any command. Every upgrade goes
//! through here so the user always sees what is running, what it waits on and
//! how it ended.

use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};

use crate::progress::Progress;
use crate::spawn::{private_spawn_dir, SpawnLock};

pub struct Migration {
    /// Names the upgrade in `tku: upgrading {label}`.
    pub label: &'static str,
    /// Cheap check, run on every invocation and again once the lock is held.
    pub pending: fn() -> bool,
    /// Returns the result line. An error leaves the upgrade pending for the next run.
    pub run: fn(&Progress) -> Result<String>,
}

/// In order; a failed upgrade stops the ones after it.
const MIGRATIONS: &[Migration] = &[crate::accounts::ACCOUNT_IDS];

const LOCK_WAIT: Duration = Duration::from_secs(30);

pub fn run_pending(progress: bool) {
    run_all(MIGRATIONS, LOCK_WAIT, &Progress::new(progress));
}

pub(crate) fn run_all(migrations: &[Migration], wait: Duration, progress: &Progress) {
    for m in migrations {
        if !(m.pending)() {
            continue;
        }
        eprintln!("tku: upgrading {}", m.label);
        let result = acquire(wait, progress).and_then(|_lock| {
            if (m.pending)() {
                (m.run)(progress)
            } else {
                Ok("finished by another tku process".to_string())
            }
        });
        progress.clear();
        match result {
            Ok(summary) => eprintln!("tku: {} upgraded: {summary}", m.label),
            Err(e) => {
                eprintln!(
                    "tku: warning: {} not upgraded: {e:#}; retrying on the next run",
                    m.label
                );
                return;
            }
        }
    }
}

fn acquire(wait: Duration, progress: &Progress) -> Result<SpawnLock> {
    // Dot prefix: account names cannot start with '.', so no `account exec` lock collides.
    let path = private_spawn_dir()?.join(".migrate.lock");
    let started = Instant::now();
    loop {
        if let Some(lock) = SpawnLock::acquire(&path)? {
            return Ok(lock);
        }
        let waited = started.elapsed();
        if waited >= wait {
            return Err(anyhow!(
                "another tku process held the upgrade lock for {}s",
                wait.as_secs()
            ));
        }
        progress.show(format_args!(
            "Waiting for another tku process to finish the upgrade... {}s",
            waited.as_secs()
        ));
        std::thread::sleep(Duration::from_millis(100));
    }
}
