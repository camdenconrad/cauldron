//! Headless end-to-end against the scripted `fake-dap` binary (src/bin/fake_dap.rs).
//!
//! The manager spawns adapters by PATH name, so the test shims `lldb-dap` → fake-dap via a
//! symlink in a temp dir prepended to PATH. Everything lives in ONE #[test] because PATH is
//! process-global — no parallel-test races by construction.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use cauldron_dap::{AdapterKind, DebugEvent, DebugManager};

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
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn breakpoint_session_end_to_end_via_fake_adapter() {
    // --- shim the fake binary onto PATH as `lldb-dap` -----------------------------------------
    let fake = env!("CARGO_BIN_EXE_fake-dap");
    let dir = std::env::temp_dir().join(format!("cauldron-dap-fake-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::os::unix::fs::symlink(fake, dir.join("lldb-dap")).unwrap();
    std::env::set_var(
        "PATH",
        format!(
            "{}:{}",
            dir.display(),
            std::env::var("PATH").unwrap_or_default()
        ),
    );

    let main_c = PathBuf::from("/tmp/fake/main.c");
    let mut dap = DebugManager::new(Arc::new(|| {}));
    // Every drained event lands here; `seen` is the wait_for scan cursor.
    let mut log: Vec<DebugEvent> = Vec::new();
    let mut seen = 0usize;
    assert_eq!(dap.state(), "idle");
    assert!(!dap.is_running());

    // Breakpoints stored BEFORE launch must be replayed during the handshake (line 999 is the
    // deliberately-unverifiable one the fake refuses).
    dap.set_breakpoints(&main_c, vec![(4, None), (999, Some("x > 3".into()))]);
    dap.launch(
        AdapterKind::LldbDap,
        &PathBuf::from("/tmp/fake/a.out"),
        &["--x".into()],
        &dir,
    )
    .expect("spawn fake adapter");
    assert!(dap.is_running());
    assert_eq!(dap.state(), "launching");
    // A second launch while live must refuse.
    assert!(dap
        .launch(AdapterKind::LldbDap, &main_c, &[], &dir)
        .is_err());

    // --- handshake: breakpoints resolved, Started, debuggee output ----------------------------
    let ev = wait_for(
        &mut dap,
        &mut log,
        &mut seen,
        Duration::from_secs(10),
        |ev| matches!(ev, DebugEvent::BreakpointsResolved { .. }),
    )
    .expect("breakpoints never resolved");
    let DebugEvent::BreakpointsResolved {
        path,
        verified_lines,
    } = ev
    else {
        unreachable!()
    };
    assert_eq!(path, main_c);
    assert_eq!(
        verified_lines,
        vec![4],
        "the bogus line 999 must not verify"
    );

    wait_for(
        &mut dap,
        &mut log,
        &mut seen,
        Duration::from_secs(10),
        |ev| matches!(ev, DebugEvent::Started),
    )
    .expect("handshake never completed");
    let ev = wait_for(
        &mut dap,
        &mut log,
        &mut seen,
        Duration::from_secs(10),
        |ev| matches!(ev, DebugEvent::Output { .. }),
    )
    .expect("debuggee output never surfaced");
    let DebugEvent::Output { category, text } = ev else {
        unreachable!()
    };
    assert_eq!(category, "stdout");
    assert_eq!(text, "hello from fake debuggee\n");

    // --- stop at the breakpoint; the stack must arrive UNASKED --------------------------------
    let ev = wait_for(
        &mut dap,
        &mut log,
        &mut seen,
        Duration::from_secs(10),
        |ev| matches!(ev, DebugEvent::Stopped { .. }),
    )
    .expect("never stopped at the breakpoint");
    let DebugEvent::Stopped {
        reason,
        thread_id,
        description,
    } = ev
    else {
        unreachable!()
    };
    assert_eq!(reason, "breakpoint");
    assert_eq!(thread_id, 7);
    assert_eq!(description.as_deref(), Some("hit it"));
    assert!(dap.is_stopped());
    assert_eq!(dap.state(), "stopped");

    let ev = wait_for(
        &mut dap,
        &mut log,
        &mut seen,
        Duration::from_secs(10),
        |ev| matches!(ev, DebugEvent::Stack { .. }),
    )
    .expect("automatic stackTrace never arrived");
    let DebugEvent::Stack { frames } = ev else {
        unreachable!()
    };
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0].name, "work");
    assert_eq!(frames[0].path.as_deref(), Some(main_c.as_path()));
    assert_eq!(frames[0].line, 4);

    // --- scopes → variables → evaluate ---------------------------------------------------------
    dap.request_scopes(frames[0].id);
    let ev = wait_for(
        &mut dap,
        &mut log,
        &mut seen,
        Duration::from_secs(10),
        |ev| matches!(ev, DebugEvent::Scopes { .. }),
    )
    .expect("scopes response never arrived");
    let DebugEvent::Scopes { frame_id, scopes } = ev else {
        unreachable!()
    };
    assert_eq!(frame_id, 1000);
    assert_eq!(scopes.len(), 1);
    assert_eq!(scopes[0].name, "Locals");

    dap.request_variables(scopes[0].variables_reference);
    let ev = wait_for(
        &mut dap,
        &mut log,
        &mut seen,
        Duration::from_secs(10),
        |ev| matches!(ev, DebugEvent::Variables { .. }),
    )
    .expect("variables response never arrived");
    let DebugEvent::Variables { reference, vars } = ev else {
        unreachable!()
    };
    assert_eq!(reference, 500);
    assert_eq!(vars[0].name, "total");
    assert_eq!(vars[0].value, "3");
    assert_eq!(vars[0].type_name.as_deref(), Some("int"));
    assert_eq!(vars[0].variables_reference, 0, "scalar: not expandable");
    assert!(vars[1].variables_reference > 0, "composite: expandable");

    dap.evaluate("41 + 1", Some(frames[0].id));
    let ev = wait_for(
        &mut dap,
        &mut log,
        &mut seen,
        Duration::from_secs(10),
        |ev| matches!(ev, DebugEvent::Evaluated { .. }),
    )
    .expect("evaluate response never arrived");
    let DebugEvent::Evaluated { result, .. } = ev else {
        unreachable!()
    };
    assert_eq!(result, "42");

    // --- continue → run to completion ----------------------------------------------------------
    dap.continue_run();
    wait_for(
        &mut dap,
        &mut log,
        &mut seen,
        Duration::from_secs(10),
        |ev| matches!(ev, DebugEvent::Continued),
    )
    .expect("continue never acked");
    let ev = wait_for(
        &mut dap,
        &mut log,
        &mut seen,
        Duration::from_secs(10),
        |ev| matches!(ev, DebugEvent::Exited { .. }),
    )
    .expect("exited event never arrived");
    let DebugEvent::Exited { code } = ev else {
        unreachable!()
    };
    assert_eq!(code, 0);
    wait_for(
        &mut dap,
        &mut log,
        &mut seen,
        Duration::from_secs(10),
        |ev| matches!(ev, DebugEvent::Terminated),
    )
    .expect("terminated event never arrived");
    assert_eq!(dap.state(), "exited");

    // --- teardown stays snappy and idempotent --------------------------------------------------
    let t0 = Instant::now();
    dap.stop();
    while dap.is_running() && t0.elapsed() < Duration::from_secs(5) {
        dap.pump();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        !dap.is_running(),
        "session must be reaped after the adapter exits"
    );
    assert!(t0.elapsed() < Duration::from_secs(5));
    let _ = std::fs::remove_dir_all(&dir);
}
