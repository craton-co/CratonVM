// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The resolved-handle form of a native->Java callback.
//!
//! # The question this answers
//!
//! `NativeContext::invoke_virtual(receiver, "someMethod", "()V", &[])` is how a
//! registered native calls back into Java, and ~2 500 stubs use it. Every one
//! of those calls resolves the callee BY NAME, on every call:
//!
//!  * `NativeContextImpl::invoke_virtual` takes the class-manager read lock and
//!    `to_string()`s the receiver's class name — a lock and a heap allocation;
//!  * `invoke_or_native` hashes the `(class, method, descriptor)` triple
//!    against the native slot table (`find_with_kind`), and on a miss runs the
//!    cold descriptor-quirk rewrite;
//!  * it then takes the class-manager read lock twice more (the JVMTI-redefine
//!    probe, which itself runs `find_method_recursive`, and the receiver /
//!    `class_store` lookup at the tail);
//!  * `invoke_on_class_shared_inner` resolves the method by name AGAIN,
//!    `to_string()`s the class name AGAIN, and walks a long `check_override`
//!    chain before reaching `interpreter::execute`.
//!
//! `completablefuture-composition-is-20x-and-5-percent-compiled-CLOSED-20260902.md`
//! §3 measured exactly ONE instance of this. `CompletableFuture.complete`'s
//! `postComplete()` callback produced 100 008 missed registry lookups in 40 000
//! chains — 76 % of every missed lookup on the workload — and routing that one
//! call through `invoke_virtual_bytecode_only` was **1.154x of the whole
//! benchmark**. That fix works because a human knows `postComplete` is
//! bytecode. This module is the general form: the VM DISCOVERS it, once, per
//! `(receiver class, method, descriptor)`.
//!
//! # Why a witness, and not a classifier
//!
//! The obvious implementation is a predicate — "is this triple plain
//! bytecode?" — evaluated ahead of the dispatch. It is also unimplementable:
//! `invoke_or_native` and `invoke_on_class_shared_inner` between them carry
//! more than thirty special arms (signature-polymorphic names, dynamic and
//! annotation proxies, the `ClassLoader` bridges, the Spring Boot loader
//! classes, the `SyntheticStub`/real-bytecode yield, the JVMTI-redefine shadow
//! drop, the FFM interface overrides, …), and a predicate that has to agree
//! with all of them is a second copy of the resolver that will drift from the
//! first. `AGENTS.md` names that shape directly: "do not add another
//! hard-coded class-name allow-list. Several already exist, in disagreeing
//! copies."
//!
//! So nothing here predicts. [`arm`] marks the dispatch as a candidate,
//! [`note_plain_bytecode_tail`] fires at the ONE site that is the ordinary
//! tail — the `interpreter::execute` call at the bottom of
//! `invoke_on_class_shared_inner` — and a memo is filled only if the dispatch
//! actually arrived there. Every special arm returns before it, so every
//! special arm is refused by construction, including ones added later that
//! this file has never heard of.
//!
//! # What invalidates a memo
//!
//! The fast path replays `interpreter::execute(declaring_class_id, name, desc)`,
//! so a memo is wrong exactly when the ordinary tail would no longer be
//! reached, or would be reached with a different declaring class. Every input
//! to that decision is covered:
//!
//! | input | guard |
//! |---|---|
//! | a native registered for the triple after the fill | `NativeMethodRegistry::generation()` |
//! | a second loader defining the receiver's class name | `class_definition_epoch()` |
//! | a synthetic stub promoted to real bytecode in place | `class_origin_epoch()` |
//! | an agent redefining a class in place | refused outright while [`any_class_redefined`](crate::classloading::any_class_redefined) |
//! | the receiver being a different class | the `ClassId` is part of the key |
//! | a second VM in the same process | the `SharedVm` address is part of the key |
//!
//! `ClassId`s are dense and never reused, and lambda-proxy ids come from a
//! reserved high range (`LAMBDA_PROXY_ID_BASE`), so an id that was an ordinary
//! class when the memo was filled cannot become a proxy afterwards.
//!
//! # Why the table is thread-local
//!
//! Natives run on the mutator thread that entered them, so a per-thread table
//! needs no synchronisation at all — which matters, because the thing being
//! removed IS lock traffic. A shared table would reintroduce, on the same hot
//! path, the contention the memo exists to delete. The cost is that each
//! thread warms its own entries; on any workload where this matters that is a
//! handful of fills against millions of hits.
//!
//! # Why the key is verified by CONTENT
//!
//! The slot index is computed from the receiver id and a few bytes of the two
//! names — cheap, and deliberately NOT a hash of all three strings, which is
//! the cost being removed. The entry then compares the full `method_name` and
//! `descriptor`. Pointer identity would be cheaper still and is not sound: a
//! caller may pass a `&String` whose buffer was freed and reallocated at the
//! same address with the same length, and a wrong hit here runs a method on
//! the wrong declaring class. `native_id.rs`'s module header records the
//! `class_manager` defect of exactly that shape ("Round 4 audit fix (CRIT)");
//! this module does not repeat it.
//!
//! # Switch
//!
//! `CRATONVM_NATIVE_CALLBACK_MEMO=0` disables it, so the mechanism can be
//! priced in ONE binary. `CRATONVM_DBG_CALLBACK_MEMO=1` prints the engagement
//! census at exit — probes, hits, fills and the distinct triples memoised.

use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::classloading::ClassId;

/// Direct-mapped slots per thread. 256 short entries is ~16 KiB of `Box<str>`
/// in the worst case and covers every distinct callback triple any workload
/// measured here produces; a collision evicts, which degrades to the
/// pre-memo behaviour rather than to a wrong answer.
const SLOTS: usize = 256;

/// The revalidation stamp. See the table in the module header: every entry is
/// one thing that can make a filled memo describe a dispatch that would no
/// longer happen.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Stamp {
    /// `NativeMethodRegistry::generation()` — a new native slot was appended.
    native_generation: u32,
    /// `class_definition_epoch()` — some name now resolves to another id.
    class_definition: u64,
    /// `class_origin_epoch()` — some class's provenance changed in place.
    class_origin: u64,
    /// The `SharedVm` this memo was taken against. A test binary that builds
    /// two VMs on one thread must not redeem the first one's `ClassId`s
    /// against the second's class store.
    vm: usize,
}

struct Entry {
    receiver_class: u32,
    /// Owned copies, compared in full on every probe — see the module header.
    method_name: Box<str>,
    descriptor: Box<str>,
    stamp: Stamp,
    /// The class whose bytecode the ordinary tail actually ran.
    declaring_class: u32,
}

/// What [`arm`] recorded about the dispatch in flight, and what the tail wrote
/// back into it.
#[derive(Clone, Copy)]
enum Witness {
    /// No dispatch in flight is a fill candidate.
    Idle,
    /// A candidate is in flight. The three fields identify it so that a
    /// NESTED arrival at the tail — a `<clinit>` driven by
    /// `ensure_class_initialized_shared`, say — cannot claim this witness.
    Armed {
        receiver_class: u32,
        name_ptr: usize,
        name_len: usize,
        desc_ptr: usize,
        desc_len: usize,
    },
    /// The candidate reached the ordinary tail, on this declaring class.
    Reached { declaring_class: u32 },
}

thread_local! {
    static TABLE: RefCell<Vec<Option<Entry>>> = RefCell::new(Vec::new());
    static WITNESS: Cell<Witness> = const { Cell::new(Witness::Idle) };
}

/// Engagement counters. Indices are [`PROBE`], [`HIT`], [`FILL`], [`EVICT`].
static COUNTS: [AtomicU64; 4] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];
const PROBE: usize = 0;
const HIT: usize = 1;
const FILL: usize = 2;
const EVICT: usize = 3;

#[inline]
fn bump(kind: usize) {
    if crate::runtime::interp_census::callback_memo_enabled() {
        COUNTS[kind].fetch_add(1, Ordering::Relaxed);
    }
}

/// Slot index. Cheap on purpose: the receiver id, the two lengths and four
/// sampled bytes. Hashing all three strings is the cost this module removes,
/// and `completablefuture-composition-is-20x-and-5-percent-compiled-CLOSED-20260902.md`
/// §3 already priced "shave the hash instead of removing it" at 0.995x.
#[inline]
fn slot(receiver_class: u32, method_name: &str, descriptor: &str) -> usize {
    let n = method_name.as_bytes();
    let d = descriptor.as_bytes();
    let mut h = (receiver_class as usize).wrapping_mul(0x9E37_79B9);
    h ^= n.len().wrapping_shl(3) ^ d.len().wrapping_shl(11);
    if let Some(b) = n.first() {
        h ^= (*b as usize) << 5;
    }
    if let Some(b) = n.last() {
        h ^= (*b as usize) << 13;
    }
    if let Some(b) = d.last() {
        h ^= (*b as usize) << 19;
    }
    (h ^ (h >> 9)) & (SLOTS - 1)
}

#[inline]
fn stamp(shared: &crate::vm::SharedVm) -> Stamp {
    Stamp {
        native_generation: shared.natives.native_methods.generation(),
        class_definition: crate::classloading::class_definition_epoch(),
        class_origin: cratonvm_classloading::class_origin_epoch(),
        vm: shared as *const crate::vm::SharedVm as usize,
    }
}

/// The memoised declaring class for this callback, if one is valid.
///
/// A `Some` answer means: the last time this exact `(VM, receiver class,
/// method, descriptor)` was dispatched through `NativeContextImpl::invoke_virtual`
/// it reached the ordinary bytecode tail on this declaring class, and nothing
/// that could change that has happened since.
#[inline]
pub fn lookup(
    shared: &crate::vm::SharedVm,
    receiver_class: ClassId,
    method_name: &str,
    descriptor: &str,
) -> Option<ClassId> {
    bump(PROBE);
    let receiver_class = receiver_class.as_u32();
    let want = stamp(shared);
    let idx = slot(receiver_class, method_name, descriptor);
    let hit = TABLE.with(|t| {
        let table = t.borrow();
        let entry = table.get(idx)?.as_ref()?;
        (entry.receiver_class == receiver_class
            && entry.stamp == want
            && &*entry.method_name == method_name
            && &*entry.descriptor == descriptor)
            .then_some(entry.declaring_class)
    })?;
    bump(HIT);
    Some(ClassId::new(hit))
}

/// Mark the dispatch about to run as a fill candidate, returning the previous
/// witness for the caller to restore.
///
/// Saved and restored rather than simply overwritten because the dispatch can
/// re-enter: a class initialiser, a lambda body, or a nested native callback
/// runs between this call and the tail. The restore is what keeps the outer
/// candidate alive across them.
#[inline]
#[must_use = "the previous witness must be restored by `disarm`"]
pub fn arm(receiver_class: ClassId, method_name: &str, descriptor: &str) -> WitnessSave {
    let previous = WITNESS.with(|w| {
        w.replace(Witness::Armed {
            receiver_class: receiver_class.as_u32(),
            name_ptr: method_name.as_ptr() as usize,
            name_len: method_name.len(),
            desc_ptr: descriptor.as_ptr() as usize,
            desc_len: descriptor.len(),
        })
    });
    WitnessSave(previous)
}

/// The witness [`arm`] displaced. Opaque so it cannot be forged.
pub struct WitnessSave(Witness);

/// Restore the displaced witness and report whether the armed dispatch reached
/// the ordinary tail.
#[inline]
pub fn disarm(save: WitnessSave) -> Option<ClassId> {
    let outcome = WITNESS.with(|w| w.replace(save.0));
    match outcome {
        Witness::Reached { declaring_class } => Some(ClassId::new(declaring_class)),
        _ => None,
    }
}

/// Called from the ONE ordinary-tail site — the `interpreter::execute` at the
/// bottom of `invoke_on_class_shared_inner`, reached only when no override,
/// no proxy and no special arm applied.
///
/// `is_plain` is the tail's own two facts about the callee: it is neither
/// `ACC_NATIVE` nor `ACC_SYNCHRONIZED`. A synchronized callee's monitor is
/// taken by that function's `_sync_guard`, which the fast path does not have,
/// so those are never memoised.
///
/// The identity check is why a nested arrival cannot steal the witness: a
/// `<clinit>` driven from inside this dispatch reaches the same tail with a
/// different name, and a nested native callback armed its own witness before
/// getting here.
#[inline]
pub fn note_plain_bytecode_tail(
    declaring_class: ClassId,
    method_name: &str,
    descriptor: &str,
    is_plain: bool,
) {
    if !is_plain {
        return;
    }
    WITNESS.with(|w| {
        if let Witness::Armed {
            name_ptr,
            name_len,
            desc_ptr,
            desc_len,
            ..
        } = w.get()
        {
            if name_ptr == method_name.as_ptr() as usize
                && name_len == method_name.len()
                && desc_ptr == descriptor.as_ptr() as usize
                && desc_len == descriptor.len()
            {
                w.set(Witness::Reached {
                    declaring_class: declaring_class.as_u32(),
                });
            }
        }
    });
}

/// Record a verified outcome. Only ever called with a `declaring_class` that
/// [`disarm`] reported, i.e. one the resolver actually reached.
pub fn fill(
    shared: &crate::vm::SharedVm,
    receiver_class: ClassId,
    method_name: &str,
    descriptor: &str,
    declaring_class: ClassId,
) {
    let receiver_class = receiver_class.as_u32();
    let idx = slot(receiver_class, method_name, descriptor);
    let entry = Entry {
        receiver_class,
        method_name: method_name.into(),
        descriptor: descriptor.into(),
        stamp: stamp(shared),
        declaring_class: declaring_class.as_u32(),
    };
    TABLE.with(|t| {
        let mut table = t.borrow_mut();
        if table.is_empty() {
            table.resize_with(SLOTS, || None);
        }
        if table[idx].is_some() {
            bump(EVICT);
        }
        table[idx] = Some(entry);
    });
    bump(FILL);
}

/// `[probes, hits, fills, evictions]`, for the exit census.
pub fn census() -> [u64; 4] {
    [
        COUNTS[PROBE].load(Ordering::Relaxed),
        COUNTS[HIT].load(Ordering::Relaxed),
        COUNTS[FILL].load(Ordering::Relaxed),
        COUNTS[EVICT].load(Ordering::Relaxed),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_slot_index_is_stable_and_spread() {
        // The same triple always lands in the same slot...
        assert_eq!(
            slot(7, "postComplete", "()V"),
            slot(7, "postComplete", "()V")
        );
        // ...and the receiver class is part of it, which is what keeps two
        // classes' answers apart.
        let a = slot(7, "postComplete", "()V");
        let b = slot(8, "postComplete", "()V");
        assert_ne!(a, b, "the receiver id must move the slot");
        // Empty strings must not panic — `slot` samples first/last bytes.
        let _ = slot(0, "", "");
    }

    #[test]
    fn the_witness_is_only_claimed_by_its_own_triple() {
        let name = "postComplete";
        let desc = "()V";
        let save = arm(ClassId::new(42), name, desc);
        // A nested arrival with a DIFFERENT triple (a `<clinit>`, say) must
        // not claim it.
        note_plain_bytecode_tail(ClassId::new(99), "<clinit>", "()V", true);
        // ...nor may an arrival with the right strings but a non-plain callee.
        note_plain_bytecode_tail(ClassId::new(99), name, desc, false);
        note_plain_bytecode_tail(ClassId::new(77), name, desc, true);
        assert_eq!(disarm(save), Some(ClassId::new(77)));
        // And the witness is back to idle, so a later tail claims nothing.
        let save = arm(ClassId::new(1), name, desc);
        assert_eq!(disarm(save), None);
    }

    #[test]
    fn a_nested_arm_restores_the_outer_candidate() {
        let name = "postComplete";
        let desc = "()V";
        let outer = arm(ClassId::new(42), name, desc);
        {
            let inner = arm(ClassId::new(43), "inner", "()I");
            note_plain_bytecode_tail(ClassId::new(43), "inner", "()I", true);
            assert_eq!(disarm(inner), Some(ClassId::new(43)));
        }
        // The outer candidate survived the nested dispatch.
        note_plain_bytecode_tail(ClassId::new(42), name, desc, true);
        assert_eq!(disarm(outer), Some(ClassId::new(42)));
    }
}
