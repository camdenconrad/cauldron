//! Test-only: safe `$HOME` redirection.
//!
//! Both `state` and `settings` resolve their on-disk paths through `$HOME`, so their tests point
//! it at a temp dir to avoid touching the user's real state. But `$HOME` is PROCESS-wide while
//! cargo runs tests on parallel threads — so two such tests overlapping means one of them silently
//! reads the other's temp dir and fails. (That is a real flake, not a hypothetical: it reproduced
//! about once in five runs.)
//!
//! [`HomeGuard`] serializes every `$HOME`-mutating test behind one lock and restores the old value
//! on drop, including on panic. The fix belongs here rather than in `--test-threads=1`, which would
//! slow the whole suite to protect four tests.

use std::ffi::OsString;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

static HOME_LOCK: Mutex<()> = Mutex::new(());

/// Owns `$HOME` for as long as it is alive. Hold it (`let _home = HomeGuard::set(&dir);`) for the
/// whole body of any test that reads or writes a `$HOME`-derived path.
pub struct HomeGuard {
    _lock: MutexGuard<'static, ()>,
    old: Option<OsString>,
}

impl HomeGuard {
    pub fn set(dir: &Path) -> Self {
        // A test that panicked while holding the lock poisons it. That says nothing about $HOME's
        // integrity — we overwrite it immediately — so take the guard anyway rather than cascading
        // one test's failure into every other test's.
        let lock = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let old = std::env::var_os("HOME");
        std::env::set_var("HOME", dir);
        Self { _lock: lock, old }
    }
}

impl Drop for HomeGuard {
    fn drop(&mut self) {
        match self.old.take() {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
    }
}
