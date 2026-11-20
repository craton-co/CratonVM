// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 4, lane MEMOSLOT: per-op compile switches that were
//! `OnceLock<bool>` latches are now [`crate::memo_flag::memo_bool`] slots, so
//! `with_thread_overrides` reaches them while the read stays one load. Each
//! test reads the default, the switch's own off (or on) word, and the default
//! again: a latch answers all three with whatever the process saw first.

use cratonvm_types::flags::with_thread_overrides;

/// `f` read with `key` overridden to `value` (`None` = as if unset).
fn read(key: &str, value: Option<&str>, f: fn() -> bool) -> bool {
    with_thread_overrides(&[(key, value)], f)
}

/// A default-ON switch: on unset, off at every word in `kills`, on unset
/// again; `keeps` lists words that must NOT kill it (the truth table kept).
fn assert_default_on(key: &str, f: fn() -> bool, kills: &[&str], keeps: &[&str]) {
    assert!(read(key, None, f), "{key} unset must be ON");
    for &word in kills {
        assert!(!read(key, Some(word), f), "{key}={word} must be OFF");
    }
    for &word in keeps {
        assert!(read(key, Some(word), f), "{key}={word} must stay ON");
    }
    assert!(read(key, None, f), "{key} unset again must be ON");
}

