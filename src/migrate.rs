//! Upgrades of tku's on-disk data, run before any command. Every upgrade goes
//! through here so the user always sees what is running, what it waits on and
//! how it ended.

use std::time::Duration;

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
pub(crate) const MIGRATIONS: &[Migration] = &[
    crate::storage::relocate::RELOCATE,
    crate::storage::upgrade::STORE_FORMAT,
    crate::accounts::ACCOUNT_IDS,
];

const LOCK_WAIT: Duration = Duration::from_secs(30);

/// Without `wait_for_lock`, a held upgrade lock skips the upgrades for this run.
pub fn run_pending(progress: bool, wait_for_lock: bool) {
    let wait = if wait_for_lock {
        LOCK_WAIT
    } else {
        Duration::ZERO
    };
    run_all(MIGRATIONS, wait, &Progress::new(progress));
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
    SpawnLock::acquire_wait(&path, wait, |waited| {
        progress.show(format_args!(
            "Waiting for another tku process to finish the upgrade... {}s",
            waited.as_secs()
        ));
    })?
    .ok_or_else(|| {
        if wait.is_zero() {
            anyhow!("another tku process holds the upgrade lock")
        } else {
            anyhow!(
                "another tku process held the upgrade lock for {}s",
                wait.as_secs()
            )
        }
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use super::*;

    static SKIPPED_RUNS: AtomicUsize = AtomicUsize::new(0);
    static WAITED_RUNS: AtomicUsize = AtomicUsize::new(0);

    const SKIPPED: Migration = Migration {
        label: "test",
        pending: || true,
        run: |_| {
            SKIPPED_RUNS.fetch_add(1, Ordering::SeqCst);
            Ok(String::new())
        },
    };

    const WAITED: Migration = Migration {
        label: "test",
        pending: || true,
        run: |_| {
            WAITED_RUNS.fetch_add(1, Ordering::SeqCst);
            Ok(String::new())
        },
    };

    fn hold_lock() -> SpawnLock {
        SpawnLock::acquire(&private_spawn_dir().unwrap().join(".migrate.lock"))
            .unwrap()
            .unwrap()
    }

    #[test]
    fn held_lock_without_waiting_skips_at_once() {
        crate::storage::with_scratch_home("migrate-nowait", || {
            let _lock = hold_lock();
            let started = Instant::now();
            run_all(&[SKIPPED, SKIPPED], Duration::ZERO, &Progress::new(false));
            assert!(started.elapsed() < Duration::from_secs(1));
            assert_eq!(SKIPPED_RUNS.load(Ordering::SeqCst), 0);
        });
    }

    #[test]
    fn held_lock_with_waiting_runs_once_released() {
        crate::storage::with_scratch_home("migrate-wait", || {
            let lock = hold_lock();
            let holder = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(300));
                drop(lock);
            });
            run_all(&[WAITED], LOCK_WAIT, &Progress::new(false));
            holder.join().unwrap();
            assert_eq!(WAITED_RUNS.load(Ordering::SeqCst), 1);
        });
    }
}
