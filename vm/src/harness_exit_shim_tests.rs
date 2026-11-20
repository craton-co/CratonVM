//! T17.E.2 regression guard — asserts the Windows shim actually
//! installed its panic hook + atexit during process startup.
//! Does NOT exercise the failure path (that would kill the
//! harness); instead verifies the CTOR_RAN sentinel so a future
//! refactor that breaks `.CRT$XCU` placement or conditional
//! compilation is caught immediately.

#[test]
fn shim_installed_at_startup() {
    assert!(
        super::harness_exit_shim::ctor_ran(),
        "CRT init hook did not run — \
         atexit shim is not in place; Windows teardown crash will resurface",
    );
}

// Deliberately-failing test that only runs when the extra
// `--cfg test_harness_exit_code` flag is passed:
//
//   cargo test -p cratonvm-vm --lib --config \
//     'build.rustflags=["--cfg","test_harness_exit_code"]'
//
// The invariant it proves is the one the whole shim rests on: a
// genuine test failure must reach cargo as a failure, NOT be
// rewritten by our hooks. **Expect exit code 101** — libtest's own
// `ERROR_EXIT_CODE`, reached through `process::exit` before either
// hook can fire.
//
// It used to assert exit 1, on the theory that a failure arrives
// via `atexit` with the panic counter over budget. It does not:
// `atexit` never runs on the failing path, which is exactly why
// `on_exit` can return 0 unconditionally. Asserting 1 here would
// have made this guard pass only while the bug it was meant to
// catch was present.
//
// The outer `#[allow(unexpected_cfgs)]` on the module silences
// the lint for this opt-in cfg that cargo doesn't learn about
// from `Cargo.toml`.
#[cfg(test_harness_exit_code)]
#[test]
fn harness_exit_code_regression() {
    panic!("T17.E.2 deliberate failure — the run must exit 101, not 0");
}
