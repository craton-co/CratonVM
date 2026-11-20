// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 4, lane LATCHES: compile-time switch readers that were
//! `OnceLock<bool>` latches now read per call, so a per-thread (and per-VM)
//! override reaches them. One test per converted family; each shows the
//! default AND the registry row's off (or on) word arriving through
//! `with_thread_overrides`, which a latch answered with whatever the first
//! reader in the process saw.

use cratonvm_types::flags::with_thread_overrides;

/// `f` read with `key` overridden to `value` (`None` = as if unset).
fn read(key: &str, value: Option<&str>, f: fn() -> bool) -> bool {
    with_thread_overrides(&[(key, value)], f)
}

/// A default-ON switch: on when unset, off at `=0`, on again when unset.
fn assert_default_on(key: &str, f: fn() -> bool) {
    assert!(read(key, None, f), "{key} unset must be ON");
    assert!(!read(key, Some("0"), f), "{key}=0 must be OFF");
    assert!(read(key, None, f), "{key} unset again must be ON");
}

/// An opt-in switch: off when unset, on at `=1`, off again when unset.
fn assert_opt_in(key: &str, f: fn() -> bool) {
    assert!(!read(key, None, f), "{key} unset must be OFF");
    assert!(read(key, Some("1"), f), "{key}=1 must be ON");
    assert!(!read(key, None, f), "{key} unset again must be OFF");
}

/// An opt-out `NO_` key read as `!runtime_flag_on(key)`: on when unset, off
/// at `=1`.
fn assert_no_key(key: &str, f: fn() -> bool) {
    assert!(read(key, None, f), "{key} unset must leave the feature ON");
    assert!(!read(key, Some("1"), f), "{key}=1 must turn the feature OFF");
}


/// `ir_verify`: the master gate (build-profile default) and the explicit `=0`.
#[test]
fn a_thread_override_reaches_the_verify_gates() {
    use crate::ir_verify::{pre_lower_verify_disabled, verify_enabled};
    let key = "CRATONVM_JIT_VERIFY_IR";
    assert_eq!(read(key, None, verify_enabled), cfg!(debug_assertions));
    assert!(read(key, Some("1"), verify_enabled));
    assert!(!read(key, Some("0"), verify_enabled));
    assert!(!read(key, None, pre_lower_verify_disabled));
    assert!(read(key, Some("0"), pre_lower_verify_disabled));
}

