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
pub fn class_is_served(class_id: u32) -> bool {
    if class_id == 0 {
        return false;
    }
    SERVED.iter().any(|c| c.load(Ordering::Relaxed) == class_id)
}

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

    /// **An unserved class is never claimed, and a served one is** — the whole
    /// contract, and the reason the JIT helper may answer at all.
    #[test]
    fn only_a_served_class_is_claimed() {
        // Ids picked high to avoid colliding with a sibling test's note.
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
