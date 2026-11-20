// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! j27 wave 4, lane MEMOSLOT: a compile-time switch read per emitted op or per
//! node, memoised in a [`MemoSlot`] instead of a `OnceLock<bool>`.
//!
//! The read stays one relaxed load. Unlike the `OnceLock` it replaces, the
//! slot is reset whenever a test installs or drops a flag override
//! (`with_thread_overrides`), and it does not publish while one is live, so
//! the override reaches the switch and nothing latches it. The encoding is
//! `vm/src/runtime/env_cache.rs`'s: 1 off, 2 on.
//!
//! Only for class (b) switches of
//! `docs/feature-designs/jit-ir-optimizations.md`:
//! every caller is compile-time. A class (c) switch (layout or maps a runtime
//! walker or the GC must agree with) keeps its latch.

use cratonvm_types::flags::{MemoSlot, MEMO_UNSET};

const MEMO_FALSE: u8 = 1;
const MEMO_TRUE: u8 = 2;

/// The memoised answer of `compute`, derived on the first read (and on every
/// read while a flag override is installed).
///
/// `compute` is a generic `FnOnce`, not a `fn` pointer: the closures at the
/// call sites capture nothing, so it costs the hot path no argument.
///
/// Public since j27 wave 7 (lane T1IC): `vm/src/jit/**` reads its helper
/// switches through it too, in place of `OnceLock<bool>` latches that a test's
/// `with_thread_overrides` could never reach. In a process with no override
/// installed it answers exactly what the latch did: the environment is
/// read once and never again.
#[inline]
pub fn memo_bool<F: FnOnce() -> bool>(slot: &'static MemoSlot, compute: F) -> bool {
    let state = slot.load();
    if state == MEMO_UNSET {
        return derive(slot, compute);
    }
    state == MEMO_TRUE
}

#[cold]
#[inline(never)]
fn derive<F: FnOnce() -> bool>(slot: &'static MemoSlot, compute: F) -> bool {
    let value = compute();
    slot.publish(if value { MEMO_TRUE } else { MEMO_FALSE });
    value
}


