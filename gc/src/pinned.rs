// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Process-global GC object **keep-alive pin set** for JNI array access.
//!
//! # Why this exists
//!
//! `GetPrimitiveArrayCritical` / `Get<Type>ArrayElements` check out the body of
//! a Java array for native C code until the matching `Release`. The VM ships
//! *moving* collectors (the semi-space [`crate::heap::Heap`], the generational
//! [`crate::gen_heap::GenerationalHeap`] young-gen copy, the
//! [`crate::g1::G1Collector`] evacuator), so an array handed out as a raw heap
//! pointer could be relocated out from under the native code mid-call.
//!
//! The JNI layer closes the *data-movement* half of that hazard by handing
//! native code a detached **copy** of the array body (`is_copy = JNI_TRUE`),
//! never a direct heap pointer — so a relocation of the source array is
//! harmless. What remains is the *liveness* half: the source array must not be
//! **reclaimed** before the copy-back at `Release` (it may be reachable only
//! through native code that the GC's ordinary root scan does not see). This
//! module is the cross-collector primitive for that: a process-global,
//! **reference-counted** set of pinned heap addresses spliced into the root set.
//!
//! # Contract
//!
//! * [`pin`] is called on every array-critical / array-element handout, keyed by
//!   the source array object's base address.
//! * [`unpin`] is called by the matching `Release`. Pin/unpin are refcounted so
//!   nested or overlapping checkouts of the same array are safe — the address
//!   only leaves the set when its count returns to zero.
//! * A collector splices [`pinned_addrs`] into its root set so a pinned array is
//!   kept **alive** (and remapped if relocated). It does NOT need to keep the
//!   object **in place**: because native code holds a copy, not a heap pointer,
//!   relocating a pinned array is safe. [`is_pinned`] is exposed for any
//!   collector that wants to *additionally* avoid relocating pinned objects (a
//!   pure optimisation), but correctness does not depend on it.
//!
//! # Why global (not per-`Heap`)
//!
//! The JNI layer only sees `shared.heap` and a raw address; the address space
//! is process-global, and the existing per-`Heap` `gpu_pinned_refs` set is
//! feature-gated behind `gpu-offload`. A single global set keeps the JNI
//! call sites collector-agnostic and always compiled in.
//!
//! # Wiring status
//!
//! The pin set, the refcounting, the JNI `pin`/`unpin` call sites, and the
//! keep-alive root splice in [`crate::heap::Heap::collect_garbage`] (and its
//! finalizer variant) are fully implemented. The generational and G1 collectors
//! keep checked-out arrays alive through their existing root machinery
//! (`native_pin_roots` for a method-argument array, the implicit JNI local frame
//! for one the native created), so the splice is belt-and-suspenders there. No
//! per-object no-relocation enforcement is required anywhere — data-movement
//! safety is provided by the JNI copy, not by pinning the object in place.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use parking_lot::Mutex;

/// Fast gate: total number of *distinct* addresses currently pinned. Checked
/// with a single relaxed load on the hot GC per-object path so the common
/// "nothing pinned" case never touches the mutex.
static PINNED_COUNT: AtomicUsize = AtomicUsize::new(0);

/// The pin table: heap address -> pin reference count. An address is present
/// iff its count is > 0. Behind a `parking_lot::Mutex` to match the rest of the
/// crate. A `HashMap` (not a `HashSet`) so overlapping critical sections on the
/// same array refcount correctly instead of a second `unpin` prematurely
/// dropping a still-live pin.
fn table() -> &'static Mutex<cratonvm_types::PointerMap> {
    static TABLE: std::sync::OnceLock<Mutex<cratonvm_types::PointerMap>> =
        std::sync::OnceLock::new();
    TABLE.get_or_init(|| Mutex::new(cratonvm_types::PointerMap::default()))
}

/// Pin the heap object at `addr` so a moving collector will neither relocate
/// nor reclaim it. Refcounted: each `pin` must be balanced by exactly one
/// [`unpin`]. A zero address is ignored (null handle / non-heap pointer).
pub fn pin(addr: usize) {
    if addr == 0 {
        return;
    }
    let mut t = table().lock();
    let entry = t.entry(addr).or_insert(0);
    if *entry == 0 {
        // First pin for this address — it becomes a distinct member of the set.
        PINNED_COUNT.fetch_add(1, Ordering::Relaxed);
    }
    *entry += 1;
}

/// A handle to one pin that survives relocation of the pinned object.
///
/// [`pin`] / [`unpin`] are keyed by the object's base address, which is only
/// usable by a caller that can re-derive that address at release time. JNI
/// cannot: `Get<Type>ArrayElements` hands native code a detached copy and has
/// nothing but the copy's pointer to key on at `Release`, so it captured the
/// base address at pin time — and a moving collection between the two calls
/// re-keyed the pin table underneath it. The `unpin` then named an address that
/// is no longer in the table, was swallowed by the tolerated-no-op path, and the
/// entry under the NEW address stayed pinned forever, holding the array and its
/// whole transitive closure live for the life of the process.
///
/// A token is an opaque, monotonically allocated id. [`update_after_gc`] re-keys
/// the token's address alongside the pin table, so [`unpin_token`] releases the
/// right entry no matter how many times the object moved in between.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PinToken(u64);

/// `token -> currently-pinned address`, re-keyed by [`update_after_gc`] in the
/// same critical section as the pin table so the two can never disagree.
fn tokens() -> &'static Mutex<HashMap<u64, usize>> {
    static TOKENS: std::sync::OnceLock<Mutex<HashMap<u64, usize>>> = std::sync::OnceLock::new();
    TOKENS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Monotonic token allocator. Never reused, so a stale token from a
/// double-`Release` cannot collide with a live pin.
static NEXT_TOKEN: AtomicUsize = AtomicUsize::new(1);

/// Pin `addr` and return a handle that stays valid across relocation.
///
/// Equivalent to [`pin`] for the pin table itself — the refcount is shared, so a
/// tokened and an untokened pin of the same address nest correctly. Returns
/// `None` for a zero address, matching [`pin`]'s null-handle behaviour.
pub fn pin_tokened(addr: usize) -> Option<PinToken> {
    if addr == 0 {
        return None;
    }
    // Take both locks in this order everywhere (`table` then `tokens`);
    // `update_after_gc` does the same, so there is no cycle.
    let mut t = table().lock();
    let entry = t.entry(addr).or_insert(0);
    if *entry == 0 {
        PINNED_COUNT.fetch_add(1, Ordering::Relaxed);
    }
    *entry += 1;
    let id = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed) as u64;
    tokens().lock().insert(id, addr);
    Some(PinToken(id))
}

/// Release the pin `token` names, at whatever address the object now lives.
///
/// A token that is absent (already released, or from a previous process-wide
/// reset) is a no-op, matching [`unpin`]'s tolerance of an unbalanced release.
pub fn unpin_token(token: PinToken) {
    // The lookup is its own statement on purpose: the guard drops at the end of
    // the `let`, so `unpin` (which takes the table lock) is called with the
    // token lock released. Folding this into `if let Some(addr) =
    // tokens().lock().remove(..)` would hold the token guard across the body and
    // invert the table-then-tokens order taken everywhere else.
    let addr = tokens().lock().remove(&token.0);
    if let Some(addr) = addr {
        unpin(addr);
    }
}

/// Decrement the pin count for `addr`, removing it from the set when the count
/// reaches zero. Unbalanced `unpin`s (address absent) are tolerated as no-ops
/// so a double-`Release` from native code cannot underflow the count.
pub fn unpin(addr: usize) {
    if addr == 0 {
        return;
    }
    let mut t = table().lock();
    if let Some(count) = t.get_mut(&addr) {
        *count -= 1;
        if *count == 0 {
            t.remove(&addr);
            PINNED_COUNT.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

/// True iff the heap object at `addr` is currently pinned by at least one JNI
/// critical section. Hot-path safe: a single relaxed load short-circuits the
/// overwhelmingly common "nothing pinned" case without locking.
#[inline]
pub fn is_pinned(addr: usize) -> bool {
    if PINNED_COUNT.load(Ordering::Relaxed) == 0 {
        return false;
    }
    table().lock().contains_key(&addr)
}

/// True iff *any* address is pinned. A cheap gate a collector can check once,
/// before bothering to snapshot the set, to keep the no-pins path allocation-free.
#[inline]
pub fn any_pinned() -> bool {
    PINNED_COUNT.load(Ordering::Relaxed) != 0
}

/// Snapshot of every currently-pinned address. A moving collector splices these
/// into its root set so a pinned array reachable ONLY through native code is not
/// reclaimed (and is remapped if the collector relocates it — which is safe,
/// because native code holds a copy of the body, not a heap pointer).
pub fn pinned_addrs() -> Vec<usize> {
    if PINNED_COUNT.load(Ordering::Relaxed) == 0 {
        return Vec::new();
    }
    table().lock().keys().copied().collect()
}

/// Re-key pins whose object a moving collection relocated (INT-10 pairing
/// for the [`pinned_addrs`] root splice). The pin table is keyed by object
/// base address; after a move, `is_pinned` and the next
/// [`pinned_addrs`] splice must find the entry under the NEW address.
///
/// # Why this REBUILDS instead of editing in place
///
/// The previous implementation snapshotted the moved entries and then applied
/// `t.remove(old); *t.entry(new) += count` against the LIVE table. That is only
/// correct when no entry's destination equals another entry's source — and
/// [`crate::old_gen::OldGen`]'s major collection is a **sliding** compactor,
/// which produces exactly that shape. `compact_with_drop_flags` walks live
/// objects low-to-high and assigns each a destination from a monotonic write
/// cursor with `dest <= src`, so a survivor routinely slides onto the address
/// its lower neighbour just vacated; `gen_heap` then merges that map into the
/// one handed here (`pointer_map.extend(compact_map)`).
///
/// With `{A: 1, B: 1}` and a map `{A -> A0, B -> A}`, the in-place loop in
/// `FxHashMap` iteration order `[B, A]` did:
///   1. `remove(B)`, `entry(A)` already exists -> merge, `A` becomes count 2;
///   2. `remove(A)` -> deletes that merged count-2 entry wholesale, then
///      inserts `A0` with count 1.
/// Final table `{A0: 1}`: `B`'s pin is silently gone, and `PINNED_COUNT` agrees
/// with the table length so nothing trips. The array `B` names is then no
/// longer spliced into the root set, gets reclaimed while native code still
/// holds its checked-out copy, and `Release` copies back over recycled storage.
/// Which entry is lost depends on hash-bucket order, so the loss is
/// nondeterministic.
///
/// Draining into a fresh map reads every source address exactly once, before
/// any destination is written, so a chained map cannot lose an entry. Counts
/// are still merged if two live pins genuinely forward to one address (they
/// should not, but merging is the safe direction: over-retain, never drop).
/// `PINNED_COUNT` is restored from the rebuilt length rather than adjusted by
/// deltas, which also heals any drift a previous cycle left behind.
pub fn update_after_gc(pointer_map: &cratonvm_types::PointerMap) {
    if PINNED_COUNT.load(Ordering::Relaxed) == 0 || pointer_map.is_empty() {
        return;
    }
    let mut t = table().lock();
    if !t.keys().any(|addr| pointer_map.contains_key(addr)) {
        // Nothing pinned moved — leave the table (and the counter) untouched
        // so the common case costs one pass and no allocation.
        return;
    }
    let old_table = std::mem::take(&mut *t);
    let mut rebuilt = cratonvm_types::PointerMap::default();
    for (old, count) in old_table {
        if count == 0 {
            continue;
        }
        // Objects that stayed in place are absent from the map and keep their
        // address; an identity entry (`new == old`) behaves the same way.
        let new = pointer_map.get(&old).copied().unwrap_or(old);
        *rebuilt.entry(new).or_insert(0) += count;
    }
    // Under this same lock, `pin`/`unpin` keep the counter equal to the number
    // of distinct keys; storing the rebuilt length preserves that invariant.
    PINNED_COUNT.store(rebuilt.len(), Ordering::Relaxed);
    *t = rebuilt;

    // Re-key the token map in the same critical section. A token whose address
    // is absent from the map did not move and keeps its address, exactly as the
    // pin table above. Held while `t` is still locked so no `pin_tokened` can
    // interleave and register a token against a pre-move address.
    let mut tk = tokens().lock();
    for addr in tk.values_mut() {
        if let Some(new) = pointer_map.get(addr) {
            *addr = *new;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Each test pins distinct addresses and fully unpins them so the global
    // table is left empty for the next test (tests in a module run on the same
    // process; keep them order-independent by balancing pin/unpin).
    //
    // Distinct addresses and balanced pin/unpin are NOT sufficient, because
    // these tests run in PARALLEL, not in sequence. `update_after_gc` rewrites
    // the whole global table, so a test calling it remaps or drops pins a
    // concurrently-running sibling is asserting on — and `pinned_addrs()` is a
    // snapshot of what EVERY test has pinned right now, not of this test's own
    // pins. `chained_pointer_map_keeps_every_pin` failed about 1 in 10
    // whole-crate runs on that race (as `pinned.rs:394 assertion failed`)
    // before this lock existed; it is pre-existing and was merely made more
    // visible when the `zgc` feature joined the crate's default set.
    //
    // So: **every test in this module holds `pin_test_lock()` for its whole
    // body**, not only the ones that call `update_after_gc`. The state is
    // process-global, so a reader races the writers exactly as much as the
    // writers race each other.
    fn pin_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn pin_then_unpin_roundtrips() {
        let _guard = pin_test_lock();
        let a = 0x1_0000_usize;
        assert!(!is_pinned(a));
        pin(a);
        assert!(is_pinned(a));
        unpin(a);
        assert!(!is_pinned(a));
    }

    #[test]
    fn refcount_is_balanced() {
        let _guard = pin_test_lock();
        let a = 0x2_0000_usize;
        pin(a);
        pin(a); // overlapping critical section on the same array
        assert!(is_pinned(a));
        unpin(a);
        // Still pinned: only one of two pins released.
        assert!(is_pinned(a));
        unpin(a);
        assert!(!is_pinned(a));
    }

    #[test]
    fn zero_addr_is_ignored() {
        let _guard = pin_test_lock();
        pin(0);
        assert!(!is_pinned(0));
        unpin(0); // no-op, must not panic
    }

    #[test]
    fn unbalanced_unpin_is_noop() {
        let _guard = pin_test_lock();
        let a = 0x3_0000_usize;
        unpin(a); // never pinned — tolerated
        assert!(!is_pinned(a));
    }

    /// A **sliding** compactor's pointer map CHAINS: one live object's
    /// destination is another live object's source (`OldGen::
    /// compact_with_drop_flags` assigns destinations from a monotonic write
    /// cursor with `dest <= src`, walking sources low-to-high). The old
    /// in-place `remove(old); entry(new) += count` loop lost a pin whenever
    /// hash-bucket order put the chained pair the wrong way round: the second
    /// iteration's `remove` deleted the entry the first iteration had just
    /// merged into.
    ///
    /// Only ONE of the `n!` iteration orders is safe for the old in-place loop
    /// (every source visited before whatever slides onto it), and which order
    /// the table yields is not under this test's control. Measured, so the
    /// scenario below is the one that actually discriminates: at seven or fewer
    /// entries `FxHashMap` iterates in INSERTION order, so the old code
    /// survived exactly when `pin()` happened to be called in ascending address
    /// order — 0/20000 chained maps wrong. Pin the higher-addressed array
    /// FIRST, which is just as likely (`pin` order is the order native code
    /// calls `Get<Type>ArrayElements`, unrelated to layout), and it is
    /// 20000/20000 wrong. Past seven entries insertion order no longer holds
    /// and it is ~96% wrong either way.
    ///
    /// Hence: pin DESCENDING, and replay over many address sets.
    #[test]
    fn chained_pointer_map_keeps_every_pin() {
        let _guard = pin_test_lock();
        for run in 0..64usize {
            // Three live old-gen objects each sliding down one 32-byte slot,
            // exactly the shape `OldGen::compact_with_drop_flags` emits:
            //   C @ base+0x60 -> base+0x40   (the address B occupies)
            //   B @ base+0x40 -> base+0x20   (the address A occupies)
            //   A @ base+0x20 -> base+0x00
            let base = 0x0051_0000_usize + run * 0x1000;
            let (a, b, c) = (base + 0x20, base + 0x40, base + 0x60);
            let a_new = base;

            // DESCENDING — see the note above. Ascending is the one order the
            // old in-place loop happened to survive.
            pin(c);
            pin(b);
            pin(a);

            let mut map = cratonvm_types::PointerMap::default();
            map.insert(c, b);
            map.insert(b, a);
            map.insert(a, a_new);
            update_after_gc(&map);

            // After the slide the pinned addresses are exactly {a_new, a, b}.
            for (addr, who) in [(a_new, 'A'), (a, 'B'), (b, 'C')] {
                assert!(
                    is_pinned(addr),
                    "run {run}: object {who} lost its pin at {addr:#x}. A pin \
                     dropped here un-roots a JNI array whose checked-out copy \
                     is still outstanding: the next collection reclaims it and \
                     `Release` copies back over recycled storage."
                );
            }
            let snap = pinned_addrs();
            for addr in [a_new, a, b] {
                assert!(
                    snap.contains(&addr),
                    "run {run}: {addr:#x} missing from the root splice"
                );
            }
            assert!(
                !is_pinned(c),
                "run {run}: C's vacated address must not stay pinned"
            );

            unpin(a_new);
            unpin(a);
            unpin(b);
            assert!(!is_pinned(a_new) && !is_pinned(a) && !is_pinned(b));
        }
    }

    /// Two pins forwarding to the SAME destination must merge their refcounts
    /// rather than lose one, and `PINNED_COUNT` must track distinct keys.
    #[test]
    fn colliding_destinations_merge_refcounts() {
        let _guard = pin_test_lock();
        let x = 0x52_0000_usize;
        let y = 0x52_0020_usize;
        let dest = 0x52_1000_usize;
        pin(x);
        pin(y);

        let mut map = cratonvm_types::PointerMap::default();
        map.insert(x, dest);
        map.insert(y, dest);
        update_after_gc(&map);

        assert!(is_pinned(dest));
        assert!(!is_pinned(x) && !is_pinned(y), "both sources are re-keyed");
        // Merged count is 2: one `unpin` must not release it.
        unpin(dest);
        assert!(is_pinned(dest));
        unpin(dest);
        assert!(!is_pinned(dest));
    }

    /// A map that moves nothing this table holds must leave it untouched.
    #[test]
    fn update_after_gc_ignores_an_unrelated_map() {
        let _guard = pin_test_lock();
        let a = 0x53_0000_usize;
        pin(a);
        let mut map = cratonvm_types::PointerMap::default();
        map.insert(0x99_0000, 0x99_1000);
        update_after_gc(&map);
        assert!(is_pinned(a));
        unpin(a);
        assert!(!is_pinned(a));
    }

    #[test]
    fn pinned_addrs_snapshots_set() {
        let _guard = pin_test_lock();
        let a = 0x4_0000_usize;
        let b = 0x4_1000_usize;
        pin(a);
        pin(b);
        let snap = pinned_addrs();
        assert!(snap.contains(&a));
        assert!(snap.contains(&b));
        unpin(a);
        unpin(b);
    }

    /// The bug a `PinToken` exists to close: a JNI `Release` runs after a moving
    /// collection and only knows the address the object had at `Get` time.
    ///
    /// Releasing by that captured address leaves the pin live forever, because
    /// `unpin` tolerates an absent address as a no-op and the real entry now
    /// sits under the new address. Asserted here in both spellings so the test
    /// fails if the token stops being re-keyed *or* if `unpin`'s tolerance is
    /// ever tightened into something that would mask the difference.
    #[test]
    fn a_token_releases_the_pin_after_the_object_moved() {
        let _guard = pin_test_lock();
        let old = 0x61_0000_usize;
        let new = 0x61_8000_usize;

        // What JNI used to do: capture the base, release by it after a move.
        pin(old);
        let mut map = cratonvm_types::PointerMap::default();
        map.insert(old, new);
        update_after_gc(&map);
        assert!(is_pinned(new), "the table must follow the object");
        unpin(old);
        assert!(
            is_pinned(new),
            "releasing by the pre-move address is the leak this test pins down"
        );
        unpin(new); // clean up the leaked entry so the table is empty again.
        assert!(!is_pinned(new));

        // What it does now: the token follows the object.
        let token = pin_tokened(old).expect("non-zero address yields a token");
        let mut map = cratonvm_types::PointerMap::default();
        map.insert(old, new);
        update_after_gc(&map);
        assert!(is_pinned(new));
        unpin_token(token);
        assert!(!is_pinned(new), "the token must release the moved entry");
    }

    /// A token survives more than one relocation, and a double release is a
    /// no-op rather than an underflow.
    #[test]
    fn a_token_survives_repeated_moves_and_a_double_release() {
        let _guard = pin_test_lock();
        let a = 0x62_0000_usize;
        let b = 0x62_4000_usize;
        let c = 0x62_8000_usize;

        let token = pin_tokened(a).expect("token");
        for (from, to) in [(a, b), (b, c)] {
            let mut map = cratonvm_types::PointerMap::default();
            map.insert(from, to);
            update_after_gc(&map);
        }
        assert!(is_pinned(c));
        unpin_token(token);
        assert!(!is_pinned(c));
        unpin_token(token); // second Release from native code
        assert!(!is_pinned(c));
    }

    /// A tokened and an untokened pin of the same address share one refcount,
    /// so releasing one must not drop the other's keep-alive.
    #[test]
    fn tokened_and_untokened_pins_of_one_address_nest() {
        let _guard = pin_test_lock();
        let a = 0x63_0000_usize;
        pin(a);
        let token = pin_tokened(a).expect("token");
        assert!(is_pinned(a));
        unpin_token(token);
        assert!(is_pinned(a), "the plain pin still holds it");
        unpin(a);
        assert!(!is_pinned(a));
    }

    /// A zero address is not pinnable, and must not consume a token either.
    #[test]
    fn a_null_address_yields_no_token() {
        let _guard = pin_test_lock();
        assert!(pin_tokened(0).is_none());
        assert!(!is_pinned(0));
    }
}
