//! Durable close-reason logging for the long-standing "cider vanishes instantly" bug.
//!
//! The launch wrapper tees stderr through a process-substitution, which truncates the LAST line
//! on a fast exit — so the one message that matters (why the window closed) is exactly the one
//! that gets lost. This writes straight to `~/.cache/cider/close-reason.log` with an immediate
//! flush on every call, so a clean exit-0 vanish always leaves a durable record of its cause.
//! Temporary instrumentation; remove once the disappearance is pinned.

use std::io::Write as _;

/// Append `msg` (flushed) to the close-reason log and echo to stderr.
pub fn note(msg: &str) {
    let Some(dir) = std::env::var_os("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".cache")))
        .map(|d| d.join("cider"))
    else {
        return;
    };
    let _ = std::fs::create_dir_all(&dir);
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("close-reason.log"))
    {
        let _ = writeln!(f, "[{secs}] pid {} — {msg}", std::process::id());
        let _ = f.flush();
    }
    eprintln!("[cider] CLOSE-REASON: {msg}");
}
