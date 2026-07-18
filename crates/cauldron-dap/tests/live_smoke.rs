//! Live smokes against real adapters.
//!
//! debugpy is installed on this box, so its smoke runs unconditionally: a real
//! `python3 -m debugpy.adapter`, a real breakpoint, real locals. The lldb-dap smoke is
//! `#[ignore]`d (the binary may be absent) — run it with `cargo test -- --ignored` once
//! lldb-dap is on PATH.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use cauldron_dap::{AdapterKind, DebugEvent, DebugManager, Frame, Scope, Var};

const T: Duration = Duration::from_secs(10);

/// Pump into `log` until `pred` matches an UNSEEN event or the timeout elapses; returns a clone
/// of the match. Everything drained stays in `log`, so a later wait can't lose events that
/// arrived in the same pump batch as an earlier match.
fn wait_for(
    dap: &mut DebugManager,
    log: &mut Vec<DebugEvent>,
    seen: &mut usize,
    timeout: Duration,
    mut pred: impl FnMut(&DebugEvent) -> bool,
) -> Option<DebugEvent> {
    let deadline = Instant::now() + timeout;
    loop {
        log.extend(dap.pump());
        while *seen < log.len() {
            let ev = &log[*seen];
            *seen += 1;
            if pred(ev) {
                return Some(ev.clone());
            }
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Fresh per-test temp dir (under the system temp root; cleaned by the caller).
fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cauldron-dap-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The shared script: stop at `bp_line`, verify stack/locals, continue to completion.
fn run_breakpoint_session(
    kind: AdapterKind,
    program: &Path,
    args: &[String],
    cwd: &Path,
    src: &Path,
    bp_line: u32,
    expect_local: &str,
) {
    let mut dap_owned = DebugManager::new(Arc::new(|| {}));
    let dap = &mut dap_owned;
    // Every drained event lands here; `seen` is the wait_for scan cursor.
    let mut log_store: Vec<DebugEvent> = Vec::new();
    let mut seen_cursor = 0usize;
    let (log, seen) = (&mut log_store, &mut seen_cursor);

    dap.set_breakpoints(src, vec![(bp_line, None)]);
    dap.launch(kind, program, args, cwd).expect("launch");

    wait_for(dap, log, seen, T, |ev| matches!(ev, DebugEvent::Started)).expect("handshake");

    // --- Stopped at the breakpoint --------------------------------------------------------
    let ev = wait_for(dap, log, seen, T, |ev| {
        matches!(ev, DebugEvent::Stopped { .. })
    })
    .expect("stop");
    let DebugEvent::Stopped { reason, .. } = &ev else {
        unreachable!()
    };
    assert_eq!(reason, "breakpoint", "stop reason");
    assert!(dap.is_stopped());

    // --- automatic stack: our script's frame with the right file + line ---------------------
    let ev = wait_for(dap, log, seen, T, |ev| {
        matches!(ev, DebugEvent::Stack { .. })
    })
    .expect("stack");
    let DebugEvent::Stack { frames } = ev else {
        unreachable!()
    };
    let frame: &Frame = frames
        .iter()
        .find(|f| f.path.as_deref() == Some(src))
        .expect("a frame in our source file");
    assert_eq!(frame.line, bp_line, "stopped on the breakpoint line");

    // --- scopes → variables: the known local must be visible --------------------------------
    dap.request_scopes(frame.id);
    let ev = wait_for(dap, log, seen, T, |ev| {
        matches!(ev, DebugEvent::Scopes { .. })
    })
    .expect("scopes");
    let DebugEvent::Scopes { scopes, .. } = ev else {
        unreachable!()
    };
    let locals: &Scope = scopes
        .iter()
        .find(|s| s.name.to_lowercase().contains("local"))
        .unwrap_or(&scopes[0]);
    dap.request_variables(locals.variables_reference);
    let want_ref = locals.variables_reference;
    let ev = wait_for(
        dap,
        log,
        seen,
        T,
        |ev| matches!(ev, DebugEvent::Variables { reference, .. } if *reference == want_ref),
    )
    .expect("variables");
    let DebugEvent::Variables { vars, .. } = ev else {
        unreachable!()
    };
    let v: &Var = vars
        .iter()
        .find(|v| v.name == expect_local)
        .unwrap_or_else(|| panic!("local {expect_local:?} not in {vars:?}"));
    assert!(!v.value.is_empty());

    // --- clear the breakpoint (it's in a loop) and run to completion -------------------------
    dap.set_breakpoints(src, vec![]);
    wait_for(dap, log, seen, T, |ev| {
        matches!(ev, DebugEvent::BreakpointsResolved { verified_lines, .. }
                 if verified_lines.is_empty())
    })
    .expect("breakpoint clear ack");
    dap.continue_run();
    wait_for(dap, log, seen, T, |ev| {
        matches!(ev, DebugEvent::Exited { .. } | DebugEvent::Terminated)
    })
    .expect("debuggee never finished");

    dap.stop();
    let t0 = Instant::now();
    while dap.is_running() && t0.elapsed() < Duration::from_secs(5) {
        dap.pump();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!dap.is_running(), "adapter must be reaped");
}

#[test]
fn debugpy_breakpoint_smoke() {
    let dir = temp_dir("debugpy");
    let script = dir.join("loopy.py");
    // Breakpoint on the `total += i` line — inside the loop, with obvious locals.
    std::fs::write(&script, "def work(n):\n    total = 0\n    for i in range(n):\n        total += i\n    return total\n\nprint(\"result\", work(3))\n").unwrap();

    run_breakpoint_session(
        AdapterKind::Debugpy,
        &script,
        &[],
        &dir,
        &script,
        4,
        "total",
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[ignore = "requires lldb-dap on PATH"]
fn lldb_dap_breakpoint_smoke() {
    let dir = temp_dir("lldb");
    let src = dir.join("main.c");
    std::fs::write(&src, "#include <stdio.h>\nint work(int n) {\n    int total = 0;\n    for (int i = 0; i < n; i++)\n        total += i;\n    return total;\n}\nint main(void) {\n    printf(\"result %d\\n\", work(3));\n    return 0;\n}\n").unwrap();
    let bin = dir.join("a.out");
    let out = std::process::Command::new("cc")
        .args(["-g", "-O0", "-o"])
        .arg(&bin)
        .arg(&src)
        .output()
        .expect("cc");
    assert!(
        out.status.success(),
        "cc failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    run_breakpoint_session(AdapterKind::LldbDap, &bin, &[], &dir, &src, 5, "total");
    let _ = std::fs::remove_dir_all(&dir);
}
