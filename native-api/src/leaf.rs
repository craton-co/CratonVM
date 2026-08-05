// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! LEAF natives — the class of registered natives that may be dispatched
//! **without** the `safe_native_call` funnel.
//!
//! ## The predicate
//!
//! A native is a leaf when, for **every** input it can be given:
//!
//! 1. it allocates nothing on the Java heap, and initiates no collection;
//! 2. it reaches no safepoint and never blocks — in particular it takes no
//!    lock the collector can hold, and never enters the GC barrier;
//! 3. it raises no Java exception and calls no Java code (so it can neither
//!    re-enter the interpreter nor unwind through its caller); and
//! 4. it retains no `ObjectRef` past its own return.
//!
//! Together those are exactly the hazards the funnel's bookkeeping exists
//! for. **(1) and (2) are what make the pinning unnecessary**, and that is
//! the load-bearing step: a native that cannot allocate and cannot safepoint
//! cannot be running while the collector moves anything, because a peer STW
//! waits for this thread to reach its next poll and this native reaches none.
//! So the argument refs cannot go stale under it, and there is nothing for
//! `native_pin_roots` to protect.
//!
//! ## Why this is a registration property and not a `match`
//!
//! `native-call-funnel-per-call-floor-RETIRED-20260804.md`
//! asks for the bypass to generalise "as a *class* of leaf natives rather
//! than another hand-written case… That predicate wants to live on the
//! registration (a `NativeKind`-adjacent flag), not in a growing `match` in
//! `helpers.rs`." [`LEAF_NATIVES`] is that flag's source of truth: one
//! auditable table, consulted once per registration, never on a dispatch
//! path. Marking a triple here changes every dispatch route at once — the
//! interpreter's inline cache, the JIT's dispatch helper, `invoke_or_native`'s
//! twenty-nine call sites — because they all funnel through
//! `safe_native_call`, which is where the flag is read.
//!
//! ## How the flag is read without paying for it
//!
//! The funnel is handed a bare `NativeCallback` function pointer, not a
//! registry handle, so the check has to be keyed on the address. A set probe
//! per native call would tax the ~3,100 **non**-leaf natives to speed up the
//! handful of leaf ones, which is the wrong trade. Instead
//! [`is_leaf_callback`] is a one-word bloom test: a `u64` whose bits are set
//! at registration from the callback addresses. A clear bit is a *proof* of
//! non-leaf and costs one relaxed load, a shift and a test; only a set bit
//! consults the exact table, and only a genuine leaf (or a rare false
//! positive) ever gets that far.
//!
//! ## Identical code folding — the one sharp edge of keying on the address
//!
//! Two Rust `fn` items with **identical machine code** may share a single
//! address: the MSVC linker folds them (`/OPT:ICF`, on by default in release),
//! and other linkers have the same feature. Distinct `fn` items are therefore
//! *not* a guarantee of distinct addresses.
//!
//! For this module that means marking a leaf callback also marks every other
//! callback the linker folded with it. It is not hypothetical — it is how
//! `leaf_native_tests` first failed, with three same-bodied test natives
//! collapsing into one address.
//!
//! It is not a live hazard for [`LEAF_NATIVES`] as it stands (both entries
//! read a clock; nothing else in the registry compiles to the same bytes), and
//! it cannot be one for a *correctly* chosen entry: a native folded with a
//! leaf has byte-identical code, so it does byte-identically nothing
//! dangerous. It becomes a hazard the moment an entry is added whose body is a
//! trivial constant-return — the shape hundreds of registered stubs share. Do
//! not add one, and if you must, the audit below is what will catch it.
//!
//! ## The audit
//!
//! A mis-marked native is a silent heap-corruption bug, so the claim is
//! checkable at runtime rather than merely asserted: `CRATONVM_DBG=leafaudit`
//! makes the funnel verify, on every leaf dispatch, that the native really
//! did none of the four things above. See `vm_exec::safe_native_call_leaf`.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// Every native the VM is allowed to dispatch without the funnel, with the
/// reason each one satisfies the predicate.
///
/// Entries are deliberately few and deliberately boring. A native belongs
/// here only if its implementation has been read end-to-end **on every
/// branch** — "the hot path cannot allocate" is not the predicate; "no input
/// can make it allocate" is. Anything that consults a shard lock, a side
/// table keyed by identity hash, or a `ctx.` allocation helper on any branch
/// is not a leaf, however cheap its common case looks.
pub const LEAF_NATIVES: &[(&str, &str, &str)] = &[
    // Reads the platform monotonic clock into an `i64`. No heap contact at
    // all — not even a receiver.
    ("java/lang/System", "nanoTime", "()J"),
    ("java/lang/System", "currentTimeMillis", "()J"),
];

/// Maximum number of distinct leaf callback addresses tracked exactly.
///
/// Sized well above [`LEAF_NATIVES`] because one triple can be registered
/// more than once (last registration wins for the slot, but both addresses
/// stay live), and because a re-registration must never silently push a leaf
/// address out of the table — overflow disables the fast path for the
/// overflowing address rather than mis-answering for it (see [`mark`]).
const MAX_LEAF_CALLBACKS: usize = 32;

/// Bloom filter over leaf callback addresses. A clear bit is exact: that
/// address was never marked.
static FILTER: AtomicU64 = AtomicU64::new(0);

/// Exact addresses behind the filter, and how many are live. Written only at
/// registration (boot), read only on a filter hit.
static ADDRS: [AtomicUsize; MAX_LEAF_CALLBACKS] =
    [const { AtomicUsize::new(0) }; MAX_LEAF_CALLBACKS];
static ADDR_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Which filter bit an address claims.
///
/// `>> 4` because function addresses are at least 16-byte aligned on both
/// supported targets, so the low bits carry no entropy and every leaf would
/// otherwise crowd into the same handful of bits.
#[inline]
const fn bit_of(addr: usize) -> u64 {
    1u64 << ((addr >> 4) as u32 & 63)
}

/// Record `addr` as a leaf callback. Called from the registry when a
/// registration's triple appears in [`LEAF_NATIVES`].
///
/// Idempotent. On table overflow the address is simply not recorded and the
/// filter bit is not set, so [`is_leaf_callback`] answers `false` for it —
/// the fast path is lost, never wrongly taken.
pub fn mark(addr: usize) {
    if addr == 0 {
        return;
    }
    let count = ADDR_COUNT.load(Ordering::Relaxed);
    for slot in ADDRS.iter().take(count) {
        if slot.load(Ordering::Relaxed) == addr {
            return;
        }
    }
    if count >= MAX_LEAF_CALLBACKS {
        return;
    }
    ADDRS[count].store(addr, Ordering::Relaxed);
    // Publish the count before the filter bit: a reader that sees the bit
    // must be able to see the address behind it.
    ADDR_COUNT.store(count + 1, Ordering::Release);
    FILTER.fetch_or(bit_of(addr), Ordering::Release);
}

/// Whether `addr` is a registered leaf native's callback.
///
/// One relaxed load, a shift and a test on the overwhelmingly common
/// (non-leaf) answer. The exact scan runs only behind a set bit.
#[inline]
pub fn is_leaf_callback(addr: usize) -> bool {
    if FILTER.load(Ordering::Relaxed) & bit_of(addr) == 0 {
        return false;
    }
    exact(addr)
}

#[cold]
#[inline(never)]
fn exact(addr: usize) -> bool {
    let count = ADDR_COUNT.load(Ordering::Acquire);
    ADDRS
        .iter()
        .take(count)
        .any(|slot| slot.load(Ordering::Relaxed) == addr)
}

/// Whether a `(class, method, descriptor)` triple is declared leaf.
///
/// Called once per registration, never on a dispatch path.
#[inline]
pub fn triple_is_leaf(class: &str, method: &str, descriptor: &str) -> bool {
    LEAF_NATIVES
        .iter()
        .any(|(c, m, d)| *c == class && *m == method && *d == descriptor)
}

/// Number of leaf callback addresses recorded so far. For tests and the
/// `--dump-native-registry` census.
pub fn marked_count() -> usize {
    ADDR_COUNT.load(Ordering::Acquire)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unmarked_address_is_never_leaf() {
        // Deliberately an address nothing registers. The filter may or may
        // not have this bit set (another test's address can collide), but
        // the exact table must still refuse it.
        assert!(!is_leaf_callback(0xdead_0000_0000_0010));
    }

    #[test]
    fn marking_makes_an_address_leaf_and_is_idempotent() {
        let addr = 0x1234_5670usize;
        mark(addr);
        let after_first = marked_count();
        assert!(is_leaf_callback(addr));
        mark(addr);
        assert_eq!(
            marked_count(),
            after_first,
            "marking the same address twice must not consume a second slot"
        );
    }

    #[test]
    fn the_table_names_only_triples_read_end_to_end() {
        // Not a behavioural assertion — a tripwire on the table's growth.
        // Every entry here is a native whose implementation was read on
        // every branch; adding one without doing that is the failure mode
        // this module's doc warns about, and the count is the cheapest
        // thing that makes an addition visible in review.
        assert_eq!(
            LEAF_NATIVES.len(),
            2,
            "LEAF_NATIVES grew — was the new entry's implementation read on \
             EVERY branch, including its null/synthetic/array arms? See the \
             predicate in this module's doc."
        );
        for (class, method, descriptor) in LEAF_NATIVES {
            assert!(triple_is_leaf(class, method, descriptor));
        }
        assert!(!triple_is_leaf("java/lang/System", "arraycopy", "()V"));
    }
}
