// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Which receiver classes the `java.nio.Buffer.session()` shim has served.
//!
//! # Why a table rather than a name list
//!
//! `session()` is registered as a null-returning shim for eleven named buffer
//! classes (see the `Wave 2 D` block in `lib.rs`), and native dispatch is keyed
//! on the RECEIVER's class. That list is deliberately not exhaustive — the
//! read-only twins `HeapByteBufferR` and `DirectByteBufferR` are absent — so
//! "is this class name in the list?" is the wrong question and answering it
//! from the list would be wrong for exactly the classes the list forgot.
//!
//! The right question is "did the shim actually run for a receiver of this
//! class?", which is a fact rather than a guess, and the shim itself is the
//! only thing that can answer it. It records each class id it serves here, and
//! the JIT's thin `session()` helper serves *only* those — so the helper
//! returns, for any receiver it answers, byte-for-byte what the shim it
//! replaces returned for that same receiver. Every other receiver declines to
//! the generic dispatcher and keeps today's behaviour, whatever that is.
//!
//! This mirrors `native-io`'s `direct_buffer::elem_fastpath`, for the same
//! reason and with the same discipline: the funnel teaches the fast path, and
//! the fast path never widens the population the funnel proved.

use std::sync::atomic::{AtomicU32, Ordering};

/// Receiver class ids the shim has served.
///
/// Sized for the eleven registered names plus headroom. A class beyond the
/// table's capacity is simply never served by the fast path — the failure mode
/// is a missed optimisation, never a wrong answer.
const CAPACITY: usize = 16;
static SERVED: [AtomicU32; CAPACITY] = [const { AtomicU32::new(0) }; CAPACITY];

/// Has the `session()` shim actually run for a receiver of this class?
///
/// `false` for a class id of 0 and for anything the shim has not served. Every
/// caller uses this to decide whether to answer without the funnel, so the
/// conservative answer costs a crossing and never a wrong value.
///
/// # Why `extern "C"`
///
/// This function's ADDRESS is published to the JIT, which cannot depend on this
/// crate and so receives it as a bare `usize` in
/// `DirectHelperTable::buffer_session_served_class`. The compiler transmutes
/// that word back to a fn pointer before calling it
/// (`jit/src/direct_helpers.rs`, `buffer_session_class_is_served`). The `"Rust"`
/// ABI is explicitly UNSPECIFIED: it carries no stability guarantee between
/// separately compiled crates or across codegen flags, and erasing the address
/// to a `usize` is precisely the arrangement in which the two ends compile
/// separately. `extern "C"` is the only convention both ends can name and agree
/// on, so the consumer's `DirectHelperFnBufferSessionServedClass` alias
/// (`unsafe extern "C" fn(u32) -> bool`) and this definition are one change in
/// two crates; neither half is meaningful alone.
///
/// This does not affect the direct Rust callers below and in `native-io`/`vm`:
/// an `extern "C" fn` item is still an ordinary function at a Rust call site.
pub extern "C" fn class_is_served(class_id: u32) -> bool {
    if class_id == 0 {
        return false;
    }
    SERVED.iter().any(|c| c.load(Ordering::Relaxed) == class_id)
}

// The `extern "C"` above is load-bearing, not decoration, and nothing else in
// this crate would notice if it were dropped: every Rust caller here calls
// `class_is_served` by name, and a name call is spelled identically whatever
// the function's ABI. The one consumer that DOES care never sees this
// signature — it sees a `usize` and transmutes it back
// (`jit/src/direct_helpers.rs::buffer_session_class_is_served`).
//
// So pin it where the definition is. A fn item coerces to a fn-pointer type
// only when its `extern`-ness, arity, argument types and return type all match,
// which makes deleting the `extern "C"` a compile error on this line instead of
// an unspecified-ABI call at a JIT compile site. The pin is spelled out rather
// than written against `cratonvm_jit::direct_helpers::
// DirectHelperFnBufferSessionServedClass` because this crate does not depend on
// the JIT crate and must not start: the JIT sits below the native layer. The
// two spellings are checked against each other on the VM side, which is the one
// place that can name both (`docs/jit/helper-abi.md` §10).
const _: extern "C" fn(u32) -> bool = class_is_served;

/// Record that the shim served a receiver of this class.
///
/// Idempotent, lock-free, and safe to lose a race: a racing writer that wins
/// stores a class id that was also served, so the table only ever holds ids
/// the shim really answered for.
pub fn note_served(class_id: u32) {
    if class_id == 0 {
        return;
    }
    for cell in SERVED.iter() {
        match cell.load(Ordering::Relaxed) {
            v if v == class_id => return,
            0 => {
                let _ = cell.compare_exchange(0, class_id, Ordering::Relaxed, Ordering::Relaxed);
                return;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `SERVED` is process-global, and `overflow_degrades_to_unserved` fills
    /// every one of its slots on purpose. Cargo runs the two tests below on
    /// separate threads of one process, so before this guard existed the
    /// outcome depended on which ran first: with the overflow test ahead,
    /// `note_served(0xDEAD_0001)` found no free slot and the claim test failed
    /// its positive assertion. Picking ids that do not collide -- which the
    /// comment below used to rely on -- cannot help, because the problem is
    /// capacity, not collision.
    ///
    /// Each test takes this lock and starts from an empty table. The lock is
    /// test-only; the production table is never cleared.
    static TABLE: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Empty `SERVED`. Test-only: the shim's table is append-only in
    /// production, and a class is only ever removed from it by process exit.
    fn reset_table() {
        for cell in SERVED.iter() {
            cell.store(0, Ordering::Relaxed);
        }
    }

    /// **An unserved class is never claimed, and a served one is** — the whole
    /// contract, and the reason the JIT helper may answer at all.
    #[test]
    fn only_a_served_class_is_claimed() {
        let _guard = TABLE.lock().unwrap_or_else(|e| e.into_inner());
        reset_table();
        assert!(!class_is_served(0), "class id 0 is never served");
        assert!(!class_is_served(0xDEAD_0001));
        note_served(0xDEAD_0001);
        assert!(class_is_served(0xDEAD_0001));
        // A different class stays unserved: the table is per-class, not a flag.
        assert!(!class_is_served(0xDEAD_0002));
        // Re-noting is idempotent and must not consume a second slot.
        note_served(0xDEAD_0001);
        assert!(class_is_served(0xDEAD_0001));
    }

    /// **A full table refuses rather than wraps.** The capacity is an
    /// optimisation bound, not a correctness one, so overflow must degrade to
    /// "not served" instead of evicting an entry and claiming a class the
    /// funnel never proved.
    #[test]
    fn overflow_degrades_to_unserved() {
        let _guard = TABLE.lock().unwrap_or_else(|e| e.into_inner());
        reset_table();
        for i in 0..(CAPACITY as u32 + 4) {
            note_served(0xBEEF_0000 + i + 1);
        }
        // Whatever fits is served; nothing outside the table is claimed.
        let claimed = (0..(CAPACITY as u32 + 4))
            .filter(|i| class_is_served(0xBEEF_0000 + i + 1))
            .count();
        assert!(
            claimed <= CAPACITY,
            "claimed {claimed} classes from a {CAPACITY}-slot table"
        );
    }
}
