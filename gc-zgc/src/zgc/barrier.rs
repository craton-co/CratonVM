// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! ZGC load barrier — the real atomic, self-healing one.
//!
//! # Why this module exists
//!
//! [`crate::zgc::LoadBarrier`] (in `zgc.rs`) is an admitted simulation. Its
//! `slow_path` takes `&mut self` and recolors a plain [`crate::zgc::ColoredPointer`]
//! **value**: it never touches the in-memory oop, so nothing is healed, no
//! mutator can drive it concurrently, and it delivers exactly none of ZGC's
//! properties. The module docs on `zgc.rs` say so in as many words. This
//! module is the honest replacement.
//!
//! A production ZGC load barrier is three things at once, and dropping any
//! one of them collapses the design:
//!
//! 1. **A test, not a call.** Every reference load in the program runs the
//!    fast path. It must be a load, an `AND`, and a not-taken branch — no
//!    call, no lock, no atomic RMW. See [`load_barrier_fast`] and the note
//!    on why [`ZBarrierStats`] does *not* count fast hits by default.
//!
//!    **The masks must be hoisted, or the composed entry point breaks this**
//!    (2026-09-21). They are phase constants, flipped only at a safepoint, so
//!    re-reading them per slot through [`ZBarrierContext`] costs three to four
//!    trait calls for a value that cannot have changed — and through
//!    `&dyn ZBarrierContext` none of them inline. [`ZBarrierPhase`] is the
//!    hoist, [`z_barrier_phased`] is the per-slot path against it, and
//!    [`z_barrier_slice`] is the batched form every multi-slot caller should
//!    use. [`z_barrier_phased`] also lists, without softening them, the four
//!    things that still stand between this and a JIT-emitted barrier.
//!
//! 2. **Self-healing.** On a bad color the barrier computes the correct
//!    value and **compare-and-swaps it back into the slot it was loaded
//!    from**. The next load of that same slot then hits the fast path. This
//!    is the entire reason ZGC's barrier amortizes to near-zero: the slow
//!    path is paid once per *slot*, not once per *load*. See
//!    [`load_barrier_slow`].
//!
//! 3. **Single-shot CAS.** If the CAS loses, the barrier accepts the loss and
//!    moves on. It does **not** retry in a loop. A retry loop is not merely
//!    wasteful — it is *wrong*: the CAS can lose because a mutator stored a
//!    completely different reference into the slot, and re-CASing our value
//!    over theirs would resurrect a stale reference (a lost-update bug that
//!    presents later as a use-after-free of the object the mutator meant to
//!    drop). The barrier's contract is "return the correct address for the
//!    value *I* loaded"; what the slot holds afterwards is the winner's
//!    business. See [`load_barrier_slow`]'s CAS section.
//!
//! # Decoupling: [`ZBarrierContext`]
//!
//! The barrier depends on a **trait it owns**, not on the concrete
//! colored-pointer encoding ([`crate::zgc::vaddr`]) or forwarding table
//! (`zgc::forwarding`): [`ZBarrierContext`] declares exactly the questions the
//! slow path has to ask (what is bad, what color heals, are we relocating,
//! where did this object go, please mark this). Wiring the real heap into that
//! trait is a mechanical `impl`.
//!
//! That seam was written while `vaddr` was still in flight, so the encoding
//! constants were duplicated here. **Reconciled 2026-08-07**: the duplicates
//! are gone, this module re-exports `vaddr`'s constants, and the facts the
//! seam did *not* insulate the barrier from are now stated as contract clauses
//! rather than left implicit —
//!
//! 1. `heal_color` must carry [`Z_COLORED_TAG`], because the barrier writes
//!    `destination | heal_color` and nothing else. **The trait's default now
//!    supplies it** — see the `heal_color` decision below;
//! 2. the fast path tests the **bad** mask, so that null costs no branch and an
//!    object at heap offset 0 is not mistaken for null;
//! 3. every address the barrier takes or returns is a **42-bit heap offset**,
//!    never a machine pointer — see "Address domain" below.
//!
//! All three are on [`ZBarrierContext`].
//!
//! # Address domain: this module traffics in heap OFFSETS (decided 2026-08-07)
//!
//! Three modules were found to disagree about what a reference word holds, and
//! the disagreement is invisible on the Windows dev host: `vaddr` documents a
//! 42-bit offset from the heap base but also declares
//! [`Z_MAX_ADDRESS`](crate::zgc::vaddr::Z_MAX_ADDRESS) = 2^47-1; `page.rs` hands
//! out **real process addresses** out of a `Vec<u8>`, which Linux `mmap`s near
//! `0x7f…` (≈2^47) and Windows places low; and this module's doc comments said
//! "bare address" while its default mask was 42 bits.
//!
//! **The decision: a reference word, and therefore everything
//! [`ZBarrierContext::forward`] and [`ZFastPath::Good`] carry, is a heap offset
//! from [`ZVirtualAddressSpace::base`](crate::zgc::vaddr::ZVirtualAddressSpace::base).**
//! Three reasons, in order of weight:
//!
//! 1. **It is what is actually in the slot.** `vaddr::color` builds a word as
//!    `Z_COLORED_TAG | color.bit() | (offset & Z_OFFSET_MASK)` and
//!    `ZVirtualAddressSpace::address_for_offset` recovers the pointer as
//!    `base + (word & Z_OFFSET_MASK)`. The barrier reads and CASes that exact
//!    word. It does not get a vote: masking a colored word with anything other
//!    than the offset field yields metadata bits, not more address.
//! 2. **The 42-bit default mask is forced, not chosen.** The metadata field
//!    starts at bit 42 ([`Z_METADATA_SHIFT`]). A wider `address_mask` — anything
//!    reaching toward `Z_MAX_ADDRESS`'s 47 bits — would overlap
//!    [`Z_METADATA_MASK`], so `destination | heal_color` would corrupt the color
//!    and [`load_barrier_slow`]'s existing "heal_color must not overlap
//!    address_mask" assertion would fire. There is no room for a machine address
//!    in this encoding, and no amount of documentation could make room.
//! 3. **The offset domain cannot misbehave on Linux.** An offset is bounded by
//!    [`Z_MAX_HEAP_SIZE`](crate::zgc::vaddr::Z_MAX_HEAP_SIZE) = 2^42 by
//!    construction, wherever the OS placed the reservation. The machine-address
//!    reading is the one that breaks: `0x7f…` truncated to 42 bits is a silently
//!    wrong pointer on Linux and a silently *correct-looking* one on Windows,
//!    which is the worst possible pair of behaviours for a bug of this class.
//!
//! `Z_MAX_ADDRESS` is not evidence against any of this. It bounds where the
//! reservation may be *placed* (`vaddr.rs`'s `ZVirtualAddressSpace::new` refuses
//! `base + size > Z_MAX_ADDRESS`), because `cratonvm_types::plausible_heap_pointer`
//! and `CompactValue`'s NaN box assume a 47-bit window. It says nothing about
//! what a slot holds. So `Z_OFFSET_MASK >= Z_MAX_ADDRESS` is not a property that
//! should hold; the two constants are bounds on two different domains, and
//! asserting one against the other is the category error, not the finding.
//!
//! ## What the other modules must now do
//!
//! * **`page.rs`** keeps handing out machine addresses — nothing changes there,
//!   it has no notion of a colored word. Whoever wires `ZgcRealHeap` converts at
//!   that boundary: `offset = addr - base` going into a slot,
//!   `address_for_offset(word)` coming out. That conversion is the *only* place
//!   the two domains meet, and it is one function each way.
//! * **`forwarding.rs`** agrees on storage and is neutral on domain: a sibling
//!   landed [`ZForwardingPayload`](crate::zgc::forwarding::ZForwardingPayload),
//!   which stores destinations heap-base-relative behind a newtype for exactly
//!   this reason, and which deliberately refuses to know what its payload means.
//!   Nothing to do there.
//! * **`relocate.rs` has complied, and the adapter now exists.** *(Rewritten
//!   2026-08-07, second pass. The previous text here described a `relocate.rs`
//!   that no longer exists — four wrong claims and two dead line anchors, under
//!   the words "Checked in source, not assumed", which is worse than an ordinary
//!   stale comment because it tells the reader not to re-check. Line anchors are
//!   deliberately gone from this section; the symbol names are the stable
//!   reference.)*
//!
//!   That module is **absolute-domain internally** — it `copy_nonoverlapping`s
//!   object bytes and range-checks against real page bounds, all of which need
//!   machine addresses — and it says so. It therefore owns the conversion, and
//!   exposes **two** pairs: `ZRelocate::forward` / `ZRelocate::forward_lookup`
//!   in the absolute domain for the relocator and for external address-keyed
//!   tables, and
//!   [`ZRelocate::forward_offset`](crate::zgc::relocate::ZRelocate::forward_offset)
//!   / [`ZRelocate::forward_lookup_offset`](crate::zgc::relocate::ZRelocate::forward_lookup_offset)
//!   in the **offset domain, for the load barrier and only the load barrier**.
//!
//!   **[`ZBarrierContext::forward`] must be implemented over
//!   `ZRelocate::forward_offset`.** Not over `forward`, not over `decode_to`
//!   with a hand-rolled ± heap base: the `_offset` pair applies
//!   `ZRelocate::check_offset_domain` — the same [`is_bare_offset`] predicate
//!   this module uses, in release builds as well as debug — to both the argument
//!   and the answer, and hand-rolling the arithmetic bypasses exactly that
//!   guard. `relocate.rs`'s own header records that earlier drafts sent the
//!   barrier to `ZRelocate::forward` and that this **was wrong**.
//!
//!   Note the domains are two different bases and `relocate.rs` keeps them
//!   apart: `ZRelocate::heap_base` is the origin of *this* module's offset
//!   domain, while `ZRelocateConfig::to_encoding_base` is the forwarding-payload
//!   encoding base. Conflating them is correct in the default configuration and
//!   garbage in any other. (`to_encoding_base: Some(0)` is now *refused* rather
//!   than recommended: over a `Vec<u8>` heap that Linux `mmap`s near `0x7f…` it
//!   makes every insert fail.)
//!
//!   `forward_offset` returns `Result<Option<u64>, ZRelocateError>` and its
//!   `Err` must **not** fall back to the input — the from-space page may already
//!   be quarantined. [`ZBarrierContext::forward`] therefore returns
//!   `Option<u64>`, and [`ZBarrierContext::on_forward_failure`] defines what
//!   happens to `None`; see those two methods.
//! * **`mark.rs` is on the *machine-address* side, and this bullet is the one
//!   that was missing.** *(Added 2026-08-07, second pass — its absence from this
//!   list was rated the most dangerous finding of the cross-module audit.)*
//!   [`ZMarkContext`](crate::zgc::mark::ZMarkContext) declares every `u64`
//!   crossing it to be an unmasked machine address of an object base, and the
//!   production implementation **dereferences** it
//!   (`ZgcRealHeap::try_mark` does `header_ref(addr as usize as *mut u8)`).
//!
//!   [`ZBarrierContext::mark_live`] is handed a bare offset by
//!   [`load_barrier_slow`]. **An implementation of it must route through
//!   [`ZMarkHandle::mark_live_offset`](crate::zgc::mark::ZMarkHandle::mark_live_offset)
//!   (or `mark_live_buffered_offset`), never through plain
//!   `ZMarkHandle::mark_live`.** The `_offset` entry points convert with
//!   `ZMarkContext::heap_base` and refuse loudly if they cannot.
//!
//!   What makes this the dangerous one is that the wrong wiring does not crash.
//!   `ZMarkContext::is_in_heap` is a registry membership test for a real heap; a
//!   42-bit offset is not a registered object base, so the mark is refused,
//!   counted in `ZMarkStats::off_heap_children` — the *wild-pointer* counter —
//!   and discarded, while the object it named is swept. Silent, and
//!   indistinguishable from the collector correctly refusing a wild pointer. The
//!   separate `ZMarkStats::domain_refusals` counter exists so that the correct
//!   entry point, when it *is* handed the wrong domain, cannot hide there.
//! * **`remembered.rs`** is on the *machine-address* side, correctly and
//!   deliberately: `ZGenerationContext::is_old` / `is_young` range-compare
//!   against real heap bounds. A value that came out of this barrier must have
//!   the heap base added before it is handed to that trait, or every
//!   cross-generation edge is silently filtered. That mismatch is documented at
//!   length on `ZGenerationContext` itself.
//!
//! ## Why no newtype here, when `forwarding.rs` needed one
//!
//! `ZForwardingPayload` exists because its value's relation to an address is
//! *unknowable from the word*: the encoding (`to_absolute - heap_base + 8`)
//! belongs to the relocator, and a payload differs from the address it stands
//! for by an arbitrary runtime constant while looking every bit as plausible.
//! Nothing but the type can tell them apart. That is the fixed G1 `address + 3`
//! defect's shape.
//!
//! The barrier's case is different in one decisive way: **the confusion is
//! machine-checkable.** An offset satisfies `value & !address_mask == 0` by
//! construction; a ~47-bit Linux machine address misses it by five whole bits.
//! So the check a newtype would buy is available as an actual check —
//! [`is_bare_offset`], which now guards [`load_barrier_slow`]'s use of
//! `forward`'s return value in debug *and* release. A newtype would additionally
//! change [`ZFastPath::Good`]'s payload and `forward`'s signature, which are the
//! two types every call site in the VM will touch; buying a weaker guarantee
//! than the bound check at that price is the wrong trade. If the day comes that
//! the barrier carries a value it cannot bound-check, revisit this.
//!
//! # What the barrier is handed: reference slots in CratonVM
//!
//! *Recorded 2026-08-07 from `docs/feature-designs/zgc-reference-slot-representation.md`,
//! which measured this after an earlier draft of this module assumed the
//! opposite.*
//!
//! The premise this module was drafted under — "there is no `AtomicU64`
//! reference slot to CAS today, and making one is the largest piece of work in
//! front of this module" — **is false, in the good direction**. Every reference
//! word in this heap is already 8 bytes wide and naturally aligned, and two of
//! the four shapes are already accessed atomically:
//!
//! | shape | reference word | already atomic? |
//! |---|---|---|
//! | compact instance field | `obj + 16 + field_offsets[i]`, 8 bytes (`types/src/heap_types.rs:185`) | **yes** — `AtomicU64` load/store (`types/src/field_layout.rs:986`, `:1047`), alignment guaranteed at `classloading/src/class.rs:1571-1572` |
//! | legacy instance field | the payload word at `cell + 8` of a 16-byte `Value` cell; `0 == null` (`types/src/value.rs:1502-1509`) | **yes** — `read_value_atomic` / `write_value_atomic` treat the cell as two `AtomicU64` words (`types/src/value.rs:1570`, `:1582`) |
//! | reference array element | `obj + ARRAY_DATA_OFFSET(16) + i*8` | no — plain `read()`/`write()` |
//! | static reference field | `statics + i*16 + 8` | no |
//!
//! So [`slot_as_atomic`] has something real to consume today; what is missing
//! is one resolver that produces the `*mut u64` for a given `(object, index)`
//! across all four shapes, plus atomicity on the two unmigrated ones.
//!
//! ## Two hazards that bear directly on this module's correctness
//!
//! **1. A zero-filled legacy cell has tag `0`, not the `Object` tag `4`.**
//! Object bodies are handed out as `write_bytes(ptr, 0, size)`
//! (`gc/src/heap.rs:502`, `gc/src/zgc.rs:1576`) and nothing retypes them, so a
//! reference field that has never been written reads as `Value::Int(0)`.
//! **The barrier must not heal such a slot.** Writing `destination |
//! heal_color` into the payload of a `tag == 0` cell manufactures a
//! `Value::Int` whose 8-byte payload is a heap address — type confusion that
//! `read_value_checked_atomic` passes without complaint, because the
//! discriminant is in range. The rule: the slot *resolver* gates the legacy arm
//! on the tag dword at `cell + 0` reading `4`, and yields `None` otherwise; a
//! `None` slot degrades that one load to a non-healing barrier (correct, just
//! unamortised) and is counted. The barrier itself cannot check this — it sees
//! an `&AtomicU64` and nothing else — which is exactly why the obligation is
//! recorded here rather than assumed.
//!
//! **2. `read_prim_element` silently degrades a colored word to
//! `Object(None)`.** Its reference arm applies
//! `cratonvm_types::plausible_heap_pointer` and turns an implausible word into
//! a null reference (`gc/src/heap.rs:1692-1714`). A colored word is
//! *deliberately* implausible — that is the entire job of
//! [`Z_COLORED_TAG`] — so any array read that reaches `read_prim_element` with
//! a colored word in the slot **silently nulls a live reference**. The study
//! calls this the most dangerous unmigrated read path in the tree, and it is
//! shared with Generational and G1, so it needs a ZGC-aware branch rather than
//! an edit. **Until that branch exists, barriering reference array reads is
//! blocked**: `get_array_element` under ZGC must go through the barrier, not
//! around it.
//!
//! # Barrier variants
//!
//! ZGC does not have "a" load barrier, it has a family — see
//! [`ZBarrierKind`]. The semantic difference between `Weak` and `KeepAlive`
//! is the one that silently breaks `WeakReference` / `Cleaner` when it is
//! got backwards, and this codebase has been bitten by exactly that class of
//! bug before (see `reference.rs` and the `INT-8` referent-skip-set note in
//! `zgc.rs::collect_garbage`). It is documented at length on the enum.
//!
//! # Mark buffering
//!
//! [`ZBarrierContext::mark_live`] runs from every mutator thread that trips a
//! bad color during concurrent mark, so it must not take a process-global
//! lock per reference. [`ZMarkQueue`] + the per-thread buffers below are
//! modelled directly on [`cratonvm_gc_core::satb`]: per-thread bucket, auto-flush at
//! capacity into a sharded queue, registry of live buffers so a collector can
//! drain them at a safepoint, and orphan-parking so a dying thread's pending
//! entries are not lost. Those three properties were each a fixed
//! correctness bug in `satb.rs`; they are reproduced here rather than
//! rediscovered.
//!
//! Unlike SATB, this queue needs **no activation flag**. SATB's tri-state
//! `ACTIVE`/`DRAINING` gate exists because the write barrier has no other way
//! to know whether marking is live. The ZGC load barrier's gate *is the
//! color mask*: when marking is off, the marked color is a good color, the
//! fast path hits, and `mark_live` is never reached. One less TOCTOU.
//!
//! ## What this registry covers, and what it does NOT (scope, 2026-08-07)
//!
//! A protocol audit asked whether the machinery below already covers a mutator
//! thread dying with pending mark work. **For [`ZMarkQueue`], yes** — that is
//! precisely what `Z_ORPHANED_MARK_BUFFERS` and `ZMarkBufferGuard`'s `Drop` do,
//! and the `dying_thread_mark_entries_survive_until_the_next_drain` test pins
//! it.
//!
//! **For [`crate::zgc::mark::ZMarkMutatorBuffer`], no, and it never could.**
//! That is the *other* mutator hand-off in this subsystem — the one feeding
//! `zgc::mark`'s [`ZMarkIngress`](crate::zgc::mark::ZMarkIngress) rather than
//! the queue below — and the two mechanisms are disjoint at every level:
//!
//! * a `ZMarkMutatorBuffer` is caller-owned storage that is never registered in
//!   `Z_MARK_BUFFER_REGISTRY`, so [`flush_all_thread_mark_buffers`] cannot see
//!   one and orphan-parking never gets a chance to run for it;
//! * `zgc::mark` forbids `static` and `thread_local!` outright, so it could not
//!   adopt this registry's shape even if it wanted to;
//! * and the failure is *worse* there than here.
//!   [`ZMarkHandle::mark_live_buffered`](crate::zgc::mark::ZMarkHandle::mark_live_buffered)
//!   sets the mark bit **before** the address reaches the ingress, so a lost
//!   entry leaves an object marked-and-unscanned — undiscoverable by any future
//!   `try_mark` — whereas a lost entry from the queue below merely leaves an
//!   address unmarked, which any later load barrier can rediscover.
//!
//! `zgc::mark` closes its own case by giving the buffer a `Weak` link to its
//! pool and a `Drop` that flushes; see
//! [`ZMarkHandle::new_buffer`](crate::zgc::mark::ZMarkHandle::new_buffer).
//! **A [`ZBarrierContext`] implementation picks one hand-off or the other**, and
//! whichever it picks it inherits that mechanism's rules — do not read this
//! registry as covering both.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};

use parking_lot::Mutex;

// ---------------------------------------------------------------------------
// Colored-pointer layout constants
// ---------------------------------------------------------------------------
//
// RECONCILED 2026-08-07. This block previously *redefined* the encoding,
// copied verbatim from the simulation in `zgc.rs`, so that the module had no
// compile-time dependency on `zgc::vaddr` while that file was in flight. It
// carried a `TODO(orchestrator)` to collapse the duplicate once `vaddr` landed.
//
// `vaddr` has landed, and the two encodings were **not** the same: the
// simulation assigns `REMAPPED = 1<<42, MARKED0 = 1<<43`, whereas `vaddr`
// reproduces OpenJDK byte-for-byte with `MARKED0 = 1<<42, MARKED1 = 1<<43,
// REMAPPED = 1<<44, FINALIZABLE = 1<<45`. `vaddr` additionally sets
// `Z_COLORED_TAG` (bit 63) on every non-null colored word. The copies here were
// therefore an *active* divergence, not a latent one, and they are now deleted
// in favour of re-exports. There is exactly one definition of the encoding in
// the tree and it lives in `zgc::vaddr`.
//
// These re-exports remain the *defaults* only: every entry point takes its
// masks from [`ZBarrierContext`], so a heap whose encoding differs overrides
// `good_mask` / `bad_mask` / `heal_color` / `address_mask`.

pub use super::vaddr::{
    Z_COLORED_TAG, Z_FINALIZABLE, Z_MARKED0, Z_MARKED1, Z_METADATA_BITS, Z_METADATA_MASK,
    Z_METADATA_SHIFT, Z_NULL, Z_OFFSET_BITS, Z_OFFSET_MASK, Z_REMAPPED,
};

// ---------------------------------------------------------------------------
// ZBarrierStats
// ---------------------------------------------------------------------------

/// Diagnostic counters for the load barrier.
///
/// **All counters are relaxed atomics and are diagnostics, not correctness.**
/// Nothing in the barrier reads them back to make a decision, so a lost
/// increment costs a wrong number in a report and nothing else. Relaxed is
/// therefore the right ordering and any stronger one would be paying for a
/// guarantee no consumer needs.
///
/// ## THE SLOW-PATH ACCOUNTING IDENTITY (added 2026-08-07)
///
/// ```text
///   heal_cas_wins + heal_cas_losses + heal_skipped == slow_path_entries
/// ```
///
/// **Every entrant to [`load_barrier_slow`] lands in exactly one of those three
/// buckets, and adding a new exit from that function without picking one is a
/// bug in the new exit.** The identity is what makes "no entrant vanished
/// silently" a checkable claim rather than a hope, and this file's
/// `every_slow_path_exit_lands_in_exactly_one_heal_bucket` test pins it across
/// all four exits.
///
/// The identity used to be the two-term `wins + losses == entries`, and it was
/// *already false* by the time it was written down here: three later passes each
/// added an exit that legitimately performs no CAS — the whole-word `Z_NULL`
/// test (an object at heap offset 0 was classifying as null), and the two
/// [`ZBarrierContext::forward`] failure exits, which deliberately do not heal
/// because stamping a good colour onto a reference the collector just called
/// unusable would send every future load of that slot down the *fast* path
/// straight to it. Those exits are correct; the two-term identity was what had
/// not kept up. `heal_skipped` is the third bucket, and it exists so the
/// **next** such exit is a compile-time-visible choice instead of a silently
/// dropped entrant.
///
/// ### Why a roll-up bucket rather than summing the reasons
///
/// It used to be true that `heal_skipped == null_slow_paths + forward_failures`
/// exactly, so the bucket was arithmetically derivable — and that was precisely
/// the arrangement being rejected. A derived total goes stale the moment an exit
/// is added that forgets to bump one of the reason counters, and it goes stale
/// *silently*, which is the failure mode the identity exists to catch. The
/// reason counters answer "which early return fired"; `heal_skipped` answers
/// "did anyone vanish". Keep both.
///
/// **It is no longer derivable, and the exit that broke it is the reason the
/// roll-up was kept** (2026-09-20): a [`ZBarrierKind::Weak`] load during
/// concurrent marking now skips the heal — see exit 5 on [`load_barrier_slow`]
/// — and has no reason counter of its own beyond
/// [`Self::weak_slow_paths`], which also counts weak loads that *did* heal. The
/// identity survived the change without anyone having to notice it, which is
/// what a roll-up bucket is for.
///
/// A diverging exit (a `forward` failure — [`ZBarrierContext::on_forward_failure`]
/// returns `!`) still bumps its buckets **before** diverging, so the identity
/// holds when read from a crash dump or from a `catch_unwind`ing test.
///
/// ### What the identity does NOT say
///
/// It says nothing about how many *threads* called a barrier. A thread whose
/// load lands after another thread's heal hits the **fast** path and is not a
/// slow-path entrant at all — that is the entire point of self-healing, and it
/// makes `slow_path_entries` a value strictly bounded by, and routinely less
/// than, the number of racing threads. Assert against `slow_path_entries`;
/// asserting against a thread count asserts that healing did not work.
///
/// ## Why `fast_path_hits` is off by default
///
/// The fast path runs on *every reference load in the program*. An
/// unconditional `fetch_add` there is a `lock xadd` on a shared cache line
/// per load — it would cost far more than the barrier it is measuring and
/// would turn a read-mostly workload into a cache-line ping-pong benchmark.
/// So the fast-hit counter is behind a runtime flag ([`enable_hot_counters`])
/// rather than the more obvious `#[cfg(debug_assertions)]`: a `cfg` gate makes
/// the *behaviour* differ between debug and release builds, and this repo has
/// already been burned by exactly that (release test runs silently skipping a
/// debug-only check). A runtime flag behaves identically in both profiles.
///
/// [`enable_hot_counters`]: ZBarrierStats::enable_hot_counters
/// A statistic the load barrier's slow path bumps from every mutator thread:
/// one stripe per thread ([`StripedCounter`]), summed on the rare read. As
/// single `AtomicU64`s the four to six counters of a slow-path call were
/// lock-prefixed read-modify-writes on shared cache lines, which serialised
/// eight mutators that all heal slots after a color flip (the load barrier's
/// slow path was ~8 percent of `MtChurnProbe` under the default-on concurrent
/// relocation, most of it those atomics). Same `fetch_add` / `load` / `store`
/// spelling as the `AtomicU64` it replaces; the memory order is ignored (the
/// counts are diagnostics, read after the mutators quiesce).
pub struct ZStatCell(cratonvm_types::striped_counter::StripedCounter);

impl ZStatCell {
    /// A zeroed cell, usable in a `static`.
    pub const fn new() -> Self {
        Self(cratonvm_types::striped_counter::StripedCounter::new())
    }

    /// Add `n` on this thread's stripe. Returns 0: the running total is not
    /// available without summing every stripe.
    #[inline]
    pub fn fetch_add(&self, n: u64, _order: Ordering) -> u64 {
        self.0.add(n as usize);
        0
    }

    /// The total across all threads.
    pub fn load(&self, _order: Ordering) -> u64 {
        self.0.get() as u64
    }

    /// Zero the cell (`v` must be 0; a counter has no other legal store).
    pub fn store(&self, v: u64, _order: Ordering) {
        debug_assert_eq!(v, 0, "a statistic is only ever reset");
        self.0.reset();
    }
}

impl std::fmt::Debug for ZStatCell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0.get())
    }
}

#[derive(Debug)]
pub struct ZBarrierStats {
    /// Gate for the hot (per-load) counters. See the type docs.
    hot_counters: AtomicBool,
    /// Loads that found a good color and returned immediately.
    /// Only counted while [`ZBarrierStats::hot_counters_enabled`].
    pub fast_path_hits: ZStatCell,
    /// Loads that found a bad color and entered [`load_barrier_slow`].
    pub slow_path_entries: ZStatCell,
    /// Self-heal CASes that won: the slot now holds the healed value because
    /// of *this* barrier invocation.
    pub heal_cas_wins: ZStatCell,
    /// Self-heal CASes that lost: another thread changed the slot first. The
    /// barrier still returned the correct address; it simply did not have to
    /// write it. A high loss ratio means many threads racing on the same
    /// slot, which is normal and healthy, not an error.
    pub heal_cas_losses: ZStatCell,
    /// Slow paths that left [`load_barrier_slow`] **without attempting a CAS at
    /// all**, and therefore have no CAS outcome to record.
    ///
    /// The third term of the accounting identity on the type docs. Four exits
    /// bump it today, and each is correct to skip the heal:
    ///
    /// * the `observed == Z_NULL` guard — there is no correct colour for null,
    ///   so there is nothing to heal to (also counted in [`Self::null_slow_paths`]);
    /// * [`ZBarrierContext::forward`] answering `None`;
    /// * `forward` answering outside the barrier's address domain
    ///   ([`is_bare_offset`]);
    /// * a [`ZBarrierKind::Weak`] load **while marking is running** — healing
    ///   would stamp the cycle's mark colour into a slot whose referent this
    ///   barrier deliberately left unmarked, sending every later strong load
    ///   down the fast path past it. Exit 5 on [`load_barrier_slow`] carries the
    ///   argument.
    ///
    /// The second and third also bump [`Self::forward_failures`] and then diverge
    /// through [`ZBarrierContext::on_forward_failure`]; both counters are
    /// bumped *before* the divergence so the identity still balances in a crash
    /// dump.
    ///
    /// A nonzero value is not by itself a defect — a direct `load_barrier_slow`
    /// call with null is benign. Cross-check the reason counters: a nonzero
    /// [`Self::forward_failures`] is a collector defect.
    pub heal_skipped: ZStatCell,
    /// Slow paths that followed a forwarding pointer to a relocated copy.
    pub forwards_followed: ZStatCell,
    /// Slow paths where [`ZBarrierContext::forward`] could not produce a usable
    /// destination — it answered `None`, or it answered outside the barrier's
    /// address domain.
    ///
    /// Bumped **before** [`ZBarrierContext::on_forward_failure`] is called, so
    /// the count survives a diverging default hook and is readable from a crash
    /// dump. Any nonzero value is a collector defect; see
    /// [`load_barrier_slow`].
    pub forward_failures: ZStatCell,
    /// Addresses handed to [`ZBarrierContext::mark_live`].
    pub marks_enqueued: ZStatCell,
    /// Slow paths taken by [`ZBarrierKind::Weak`], which deliberately did
    /// **not** mark. Tracked separately so "referent not marked" is visible
    /// as a positive observation rather than as an absence.
    pub weak_slow_paths: ZStatCell,
    /// Slow paths entered with a null address. Should be zero: the fast path
    /// classifies null as good. A nonzero value means someone called
    /// [`load_barrier_slow`] directly with a null observation.
    pub null_slow_paths: ZStatCell,
}

impl ZBarrierStats {
    /// A zeroed counter block, usable in a `static`.
    pub const fn new() -> Self {
        Self {
            hot_counters: AtomicBool::new(false),
            fast_path_hits: ZStatCell::new(),
            slow_path_entries: ZStatCell::new(),
            heal_cas_wins: ZStatCell::new(),
            heal_cas_losses: ZStatCell::new(),
            heal_skipped: ZStatCell::new(),
            forwards_followed: ZStatCell::new(),
            forward_failures: ZStatCell::new(),
            marks_enqueued: ZStatCell::new(),
            weak_slow_paths: ZStatCell::new(),
            null_slow_paths: ZStatCell::new(),
        }
    }

    /// Turn on the per-load counters. See the type docs for the cost.
    pub fn enable_hot_counters(&self) {
        self.hot_counters.store(true, Ordering::Relaxed);
    }

    /// Turn the per-load counters back off.
    pub fn disable_hot_counters(&self) {
        self.hot_counters.store(false, Ordering::Relaxed);
    }

    /// Whether the per-load counters are currently being maintained.
    #[inline(always)]
    pub fn hot_counters_enabled(&self) -> bool {
        // Relaxed: this only decides whether to bump a diagnostic counter.
        // A thread reading a stale `false` for a few nanoseconds after
        // `enable_hot_counters` loses a handful of counts and nothing else.
        self.hot_counters.load(Ordering::Relaxed)
    }

    /// Record one fast-path hit, subject to the hot-counter gate.
    #[inline(always)]
    pub fn record_fast_hit(&self) {
        if self.hot_counters_enabled() {
            self.fast_path_hits.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Take a plain-integer snapshot of every counter.
    ///
    /// The reads are individually relaxed and NOT a consistent cut: a
    /// concurrently-running barrier can land between two of them, so e.g. the
    /// accounting identity `heal_cas_wins + heal_cas_losses + heal_skipped ==
    /// slow_path_entries` may briefly not hold. Quiesce the mutators before
    /// asserting on cross-counter identities.
    ///
    /// Note the skew is one-directional and bounded: `slow_path_entries` is
    /// bumped on entry and the outcome bucket on exit, so a torn read can only
    /// ever show the left side *short* of `slow_path_entries`, by at most the
    /// number of barriers in flight. It can never show a surplus.
    pub fn snapshot(&self) -> ZBarrierStatsSnapshot {
        ZBarrierStatsSnapshot {
            fast_path_hits: self.fast_path_hits.load(Ordering::Relaxed),
            slow_path_entries: self.slow_path_entries.load(Ordering::Relaxed),
            heal_cas_wins: self.heal_cas_wins.load(Ordering::Relaxed),
            heal_cas_losses: self.heal_cas_losses.load(Ordering::Relaxed),
            heal_skipped: self.heal_skipped.load(Ordering::Relaxed),
            forwards_followed: self.forwards_followed.load(Ordering::Relaxed),
            forward_failures: self.forward_failures.load(Ordering::Relaxed),
            marks_enqueued: self.marks_enqueued.load(Ordering::Relaxed),
            weak_slow_paths: self.weak_slow_paths.load(Ordering::Relaxed),
            null_slow_paths: self.null_slow_paths.load(Ordering::Relaxed),
        }
    }

    /// Zero every counter. Leaves the hot-counter gate alone.
    pub fn reset(&self) {
        self.fast_path_hits.store(0, Ordering::Relaxed);
        self.slow_path_entries.store(0, Ordering::Relaxed);
        self.heal_cas_wins.store(0, Ordering::Relaxed);
        self.heal_cas_losses.store(0, Ordering::Relaxed);
        self.heal_skipped.store(0, Ordering::Relaxed);
        self.forwards_followed.store(0, Ordering::Relaxed);
        self.forward_failures.store(0, Ordering::Relaxed);
        self.marks_enqueued.store(0, Ordering::Relaxed);
        self.weak_slow_paths.store(0, Ordering::Relaxed);
        self.null_slow_paths.store(0, Ordering::Relaxed);
    }
}

impl Default for ZBarrierStats {
    fn default() -> Self {
        Self::new()
    }
}

/// Plain-integer copy of [`ZBarrierStats`], for reporting.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ZBarrierStatsSnapshot {
    /// See [`ZBarrierStats::fast_path_hits`].
    pub fast_path_hits: u64,
    /// See [`ZBarrierStats::slow_path_entries`].
    pub slow_path_entries: u64,
    /// See [`ZBarrierStats::heal_cas_wins`].
    pub heal_cas_wins: u64,
    /// See [`ZBarrierStats::heal_cas_losses`].
    pub heal_cas_losses: u64,
    /// See [`ZBarrierStats::heal_skipped`]. Third term of the accounting
    /// identity: `heal_cas_wins + heal_cas_losses + heal_skipped ==
    /// slow_path_entries` for a quiesced barrier.
    pub heal_skipped: u64,
    /// See [`ZBarrierStats::forwards_followed`].
    pub forwards_followed: u64,
    /// See [`ZBarrierStats::forward_failures`]. Nonzero is a collector defect.
    pub forward_failures: u64,
    /// See [`ZBarrierStats::marks_enqueued`].
    pub marks_enqueued: u64,
    /// See [`ZBarrierStats::weak_slow_paths`].
    pub weak_slow_paths: u64,
    /// See [`ZBarrierStats::null_slow_paths`].
    pub null_slow_paths: u64,
}

/// Process-wide fallback counters, used by any [`ZBarrierContext`] that does
/// not override [`ZBarrierContext::stats`].
///
/// A single shared block is deliberate for a *fallback*: it means a heap that
/// forgets to wire up its own counters still produces numbers instead of
/// silently producing none. Production heaps should own a per-heap
/// [`ZBarrierStats`] so two heaps in one process do not pool their telemetry.
static GLOBAL_Z_BARRIER_STATS: ZBarrierStats = ZBarrierStats::new();

/// Handle on the process-wide fallback counters. See [`GLOBAL_Z_BARRIER_STATS`].
#[inline]
pub fn global_z_barrier_stats() -> &'static ZBarrierStats {
    &GLOBAL_Z_BARRIER_STATS
}

// ---------------------------------------------------------------------------
// ZBarrierContext
// ---------------------------------------------------------------------------

/// Everything the load-barrier slow path needs to know about the collector.
///
/// This trait is the seam that lets the barrier be written, reviewed and
/// tested independently of the colored-pointer encoding (`zgc::vaddr`) and
/// the forwarding table (`zgc::forwarding`). The barrier asks a fixed, small
/// set of questions — what is bad, what colour heals, are we relocating, where
/// did this object go, please mark this — and computes everything else from the
/// answers. One method is not a question: [`on_forward_failure`] is what the
/// slow path does when `forward` says it cannot answer, and it diverges (added
/// 2026-08-07, finding N4).
///
/// It is deliberately **object-safe** (every method takes `&self`), so a heap
/// may be reached either monomorphically or through `&dyn ZBarrierContext`. The
/// entry points below are generic over `C: ZBarrierContext + ?Sized` so both
/// work.
///
/// [`on_forward_failure`]: ZBarrierContext::on_forward_failure
///
/// ## Implementor contract
///
/// * **The address domain is the heap OFFSET, not the machine pointer.**
///   Everything `forward` takes and returns, and everything [`ZFastPath::Good`]
///   carries, is an offset from
///   [`ZVirtualAddressSpace::base`](crate::zgc::vaddr::ZVirtualAddressSpace::base)
///   — 42 bits, `Z_OFFSET_MASK`-wide, and bounded by
///   [`Z_MAX_HEAP_SIZE`](crate::zgc::vaddr::Z_MAX_HEAP_SIZE) wherever the OS put
///   the reservation. Decided 2026-08-07; the module header has the derivation
///   and the obligations it puts on `page.rs`, `forwarding.rs` and
///   `remembered.rs`. An implementation that returns a machine address from
///   `forward` trips [`is_bare_offset`] on the slow path — in release builds as
///   well as debug — rather than silently truncating.
/// * `forward` MUST return `Some(<bare offset>)` — no color bits, no tag. It is
///   the identity `Some(addr)` for an offset that is not in the relocation set,
///   and offset `0` is a legitimate heap location, not null; see the note on
///   `forward` itself. `None` means "this reference must not be used" and sends
///   the slow path to [`ZBarrierContext::on_forward_failure`], which diverges.
/// * **`mark_live` receives an offset, and `zgc::mark` wants a machine
///   address.** Route through
///   [`ZMarkHandle::mark_live_offset`](crate::zgc::mark::ZMarkHandle::mark_live_offset),
///   never plain `ZMarkHandle::mark_live`: the wrong one is silently discarded
///   as a wild pointer and the object is swept. Added 2026-08-07; the full
///   argument is on [`mark_live`](ZBarrierContext::mark_live) and in the module
///   header.
/// * `heal_color` MUST NOT overlap `address_mask`, or healing would corrupt
///   the address. Debug builds assert this on every slow path.
/// * **`heal_color` MUST carry every non-address bit a well-formed word needs,
///   not just the color bit.** Under `vaddr` that means
///   `ZColor::bit() | Z_COLORED_TAG`. Added 2026-08-07: the barrier writes
///   `destination | heal_color` and nothing else, so a `heal_color` that omits
///   [`Z_COLORED_TAG`] heals every slot it touches into a word that
///   [`is_well_formed`](crate::zgc::vaddr::is_well_formed) rejects **and** that
///   `cratonvm_types::plausible_heap_pointer` accepts — i.e. it disarms exactly
///   the tripwire bit 63 exists to arm (`vaddr.rs`, "Why bit 63"). **The default
///   now supplies the tag** (changed 2026-08-07, see [`Self::heal_color`]), so
///   this clause binds only an implementor that overrides it.
/// * `mark_live` MUST be cheap and non-blocking on the common path — it runs
///   from arbitrary mutator threads. Route it through [`ZMarkQueue`] /
///   [`z_mark_thread_local_log`] rather than a shared lock.
/// * Every method may be called concurrently from any number of threads.
pub trait ZBarrierContext {
    /// Colors that are "good" in the current phase. A slot whose value has
    /// any of these bits set needs no work.
    ///
    /// This is the barrier's *gate*: flipping this mask is how a phase
    /// transition arms and disarms the barrier, and it is why this design
    /// needs no separate activation flag (contrast [`cratonvm_gc_core::satb::SatbQueue`]'s
    /// tri-state gate).
    ///
    /// Corresponds to [`ZGoodMask::good`](crate::zgc::vaddr::ZGoodMask::good).
    fn good_mask(&self) -> u64;

    /// Colors that are "bad" in the current phase: a slot whose value has any
    /// of these bits set must take the slow path.
    ///
    /// **This, not [`good_mask`](ZBarrierContext::good_mask), is what the fast
    /// path tests** — see [`classify_bad_masked`]. Defaults to
    /// `good_mask() ^ Z_METADATA_MASK`, which is exactly how
    /// [`ZGoodMask::bad`](crate::zgc::vaddr::ZGoodMask::bad) derives it, and is
    /// correct for any good mask built from the metadata field. A heap whose
    /// good mask spans several colors gets the right answer from the same XOR.
    fn bad_mask(&self) -> u64 {
        self.good_mask() ^ Z_METADATA_MASK
    }

    /// The bad mask for a [`ZBarrierKind::Weak`] load.
    ///
    /// *Added 2026-09-21, closing
    /// `docs/feature-designs/zgc-architecture.md`.*
    ///
    /// OpenJDK's `weak_bad = (good | Remapped | Finalizable) ^ metadata`: a
    /// referent that is merely remapped, or finalizable-reachable, is already
    /// "good enough" for a load that is only deciding its fate, so it takes the
    /// fast path instead of a slow path that would resolve to the same address
    /// and mark nothing.
    /// [`ZGoodMask::weak_bad`](crate::zgc::vaddr::ZGoodMask::weak_bad)
    /// implements exactly that expression; a heap built on `vaddr` should
    /// override this with it.
    ///
    /// # Why the default is `bad_mask()` and not the OpenJDK expression
    ///
    /// Because the default must not *widen* what any existing implementor calls
    /// good. `bad_mask()` sends strictly more loads down the slow path than
    /// `weak_bad` does, and the slow path is correct for all of them — so the
    /// conservative default cannot be wrong, whereas synthesising
    /// `(good | Z_REMAPPED | Z_FINALIZABLE) ^ Z_METADATA_MASK` here would
    /// quietly re-classify `Remapped` as good-for-weak on a heap whose good mask
    /// is not `vaddr`-shaped at all. That is the same argument
    /// [`heap_base`](crate::zgc::mark::ZMarkContext::heap_base) makes for
    /// defaulting to `None` rather than to `Some(0)`: a default that cannot be
    /// wrong beats a default that is usually right.
    ///
    /// The point of the method is therefore not its default. It is that the
    /// *option* now exists and that [`z_barrier`] selects on it, so the weak
    /// path stops being "the strong path with `mark_live` suppressed".
    fn weak_bad_mask(&self) -> u64 {
        self.bad_mask()
    }

    /// The color bits a [`ZBarrierKind::Weak`] slow path stamps into a slot.
    ///
    /// *Added 2026-09-21 with [`weak_bad_mask`](Self::weak_bad_mask).*
    ///
    /// OpenJDK heals a weak load to `Remapped`, which is good under
    /// `weak_bad` and **still bad under the strong `bad_mask`** while marking is
    /// running — that asymmetry is what makes its weak heal safe, and it is the
    /// thing the single good colour of a `good_mask()`-only context cannot
    /// express. Exit 5 on [`load_barrier_slow`] tests the property directly
    /// rather than testing for `Weak`, so a heap that *can* express it starts
    /// healing weak loads again with no further change here.
    ///
    /// Defaults to [`heal_color`](Self::heal_color), which is what every
    /// implementor gets today — and which, under any `vaddr`-shaped mask set, is
    /// exactly the colour exit 5 refuses to write.
    fn weak_heal_color(&self) -> u64 {
        self.heal_color()
    }

    /// The color bits a healed value is stamped with.
    ///
    /// Defaults to `Z_COLORED_TAG | good_mask()`, which is correct whenever the
    /// good mask is a single bit. When the good mask covers several colors (e.g.
    /// "marked0 or marked1"), override this to return
    /// `Z_COLORED_TAG | <the single canonical bit for the current cycle>` so
    /// healed slots do not accumulate stale colors.
    ///
    /// # Why the default carries the tag (changed 2026-08-07)
    ///
    /// It used to be plain `good_mask()`. That was wrong under `vaddr` in a way
    /// with no loud failure: the barrier writes `destination | heal_color` and
    /// nothing else, so a tag-less heal color healed every slot it touched into
    /// a word that [`is_well_formed`](crate::zgc::vaddr::is_well_formed) and
    /// [`is_colored_word`](crate::zgc::vaddr::is_colored_word) both **reject**
    /// and that `cratonvm_types::plausible_heap_pointer` **accepts** — the
    /// barrier's own output disarming the exact tripwire bit 63 exists to arm
    /// (`vaddr.rs`, "Why bit 63"). Such a word is then dereferenceable as a wild
    /// pointer by every audit point in the tree, and no audit point objects.
    ///
    /// The alternative considered was to leave the default alone and make the
    /// override mandatory, enforced by a `debug_assert!` for a missing tag. It
    /// was rejected: a `debug_assert` fires only in debug builds, and this repo
    /// has already been bitten by a release test run silently skipping a
    /// debug-only check (`docs/known-issues/.../release-test-runs-had-lock-order-enforcement-off`).
    /// A mandatory-override rule whose enforcement evaporates in the build
    /// configuration the suites actually run under is not enforcement.
    ///
    /// Fixing the *default* instead is also the smaller claim. `vaddr::color`
    /// sets [`Z_COLORED_TAG`] unconditionally on every non-null word — it is not
    /// phase-dependent, not cycle-dependent, and not a choice — so the tag
    /// belongs in the default alongside the offset mask and the metadata-derived
    /// bad mask, which are already `vaddr`'s. The old arrangement had this
    /// module defaulting to `vaddr`'s encoding for `address_mask` and `bad_mask`
    /// but to *not* `vaddr`'s encoding for `heal_color`, which is the
    /// inconsistency that produced the bug.
    ///
    /// A heap whose encoding has no bit-63 tag overrides this, exactly as it
    /// already must override `good_mask` and `address_mask`.
    ///
    /// The tag cannot collide with [`Self::address_mask`]'s default (bit 63 vs
    /// bits 0..=41), so the "must not overlap address_mask" clause still holds
    /// for free.
    fn heal_color(&self) -> u64 {
        Z_COLORED_TAG | self.good_mask()
    }

    /// Mask selecting the address payload of a colored pointer.
    /// Defaults to [`Z_OFFSET_MASK`] — 42 bits, matching `vaddr`.
    ///
    /// # This 42-bit width is forced by the encoding, not a placeholder
    ///
    /// *Clarified 2026-08-07 after the cross-module sweep read it as a gap.*
    ///
    /// The value the barrier hands back is a bare heap **offset**: this mask
    /// excludes both [`Z_METADATA_MASK`] and [`Z_COLORED_TAG`], and
    /// [`ZVirtualAddressSpace::address_for_offset`](crate::zgc::vaddr::ZVirtualAddressSpace::address_for_offset)
    /// is the remaining step to a machine address. That is the whole domain
    /// decision, and the module header argues it.
    ///
    /// The mask **cannot** be widened toward
    /// [`Z_MAX_ADDRESS`](crate::zgc::vaddr::Z_MAX_ADDRESS)'s 47 bits to make room
    /// for a machine pointer: the metadata field starts at bit 42
    /// ([`Z_METADATA_SHIFT`]), so a 47-bit address mask would overlap
    /// `Z_MARKED0`..`Z_FINALIZABLE`, `destination | heal_color` would corrupt the
    /// color, and [`load_barrier_slow`]'s overlap assertion would fire. A heap
    /// that needs machine addresses in its slots needs a different *encoding*,
    /// not a different mask — and `vaddr`'s encoding is OpenJDK's.
    ///
    /// So an override here is for a heap with a genuinely different colored-word
    /// layout. It is **not** the escape hatch for "my addresses are bigger than
    /// 42 bits"; the answer to that is to store the offset.
    fn address_mask(&self) -> u64 {
        Z_OFFSET_MASK
    }

    /// Whether concurrent marking is running. When false the slow path does
    /// no marking even for barrier kinds that normally would.
    ///
    /// # This is the barrier's gate, and it is the barrier's obligation
    ///
    /// *Decided 2026-08-07; the question was previously undocumented in either
    /// direction.*
    ///
    /// [`load_barrier_slow`] tests this before calling
    /// [`mark_live`](Self::mark_live), and **that test is the whole gate**. The
    /// mark-side receiver does not re-check:
    /// [`ZMarkHandle::mark_live`](crate::zgc::mark::ZMarkHandle::mark_live)
    /// deliberately marks whatever it is handed, so a call that lands after
    /// `ZMarkCoordinator::end_cycle` sets a mark bit belonging to a finished
    /// cycle. Three reasons the obligation sits here rather than there, argued
    /// at length on that method:
    ///
    /// 1. this side has a *better* gate than a flag — the colour mask. When
    ///    marking is off the marked colour is a good colour, the fast path hits,
    ///    and the slow path is never entered at all. That is a data-dependent
    ///    gate on the word in the slot, and it is why this module needs no
    ///    activation flag (see the module header);
    /// 2. a check on the mark side could not be authoritative anyway —
    ///    `is_marking()` then `try_mark` is check-then-act, and the cycle can end
    ///    in between however it is written. A racy check that *reads* as a
    ///    guarantee is worse than none;
    /// 3. the failure is bounded to floating garbage by the phase protocol, not
    ///    by any gate: `begin_cycle` clears the ingress and the stripes, and the
    ///    stale mark bit belongs to a colour the next cycle flips away from —
    ///    which is what ZGC's two mark bits are for.
    ///
    /// A violation is nevertheless *counted*, by
    /// `ZMarkStats::late_marks`, so a barrier that gets this wrong is visible in
    /// telemetry rather than silent. An implementor whose `mark_live` routes to
    /// a `ZMarkHandle` should treat a nonzero `late_marks` as a bug on **this**
    /// side of the seam.
    fn is_marking(&self) -> bool;

    /// Whether concurrent relocation is running. When false the slow path
    /// skips [`forward`](ZBarrierContext::forward) entirely — an address is
    /// its own destination.
    fn is_relocating(&self) -> bool;

    /// Relocate-or-lookup: map a heap offset to its current location.
    ///
    /// Identity for an offset that is not in the relocation set. For one that IS
    /// in the relocation set this either finds the existing forwarding entry or
    /// performs the copy and installs one — that choice belongs to the
    /// forwarding table, not to the barrier.
    ///
    /// # Domain (restated 2026-08-07 — the docs used to say "bare address")
    ///
    /// `addr` is a **heap offset**, and so is the returned value: bare, no color
    /// bits, no tag, `Z_OFFSET_MASK`-wide. It is **not** a machine pointer.
    ///
    /// ## Implement this over `ZRelocate::forward_offset`
    ///
    /// *Rewritten 2026-08-07, second pass. The paragraph that stood here sent
    /// implementors to `ZRelocate::decode_to` with a hand-rolled ± heap base,
    /// and described a `relocate.rs` that no longer exists.*
    ///
    /// `relocate.rs` owns the conversion and exposes it as
    /// [`ZRelocate::forward_offset`](crate::zgc::relocate::ZRelocate::forward_offset)
    /// (and `forward_lookup_offset` for the non-relocating twin). Use it.
    /// Hand-rolling the arithmetic over `decode_to` — which the previous text
    /// here instructed — bypasses `ZRelocate::check_offset_domain`, the
    /// release-build guard that module added for precisely this mistake, and
    /// also risks reaching for `ZRelocateConfig::to_encoding_base`, which is a
    /// *different* base from `ZRelocate::heap_base` and coincides with it only
    /// in the default configuration.
    ///
    /// ## Returns
    ///
    /// * `Some(dest)` — the object's current heap offset. This is the identity
    ///   `Some(addr)` for an offset that is not in the relocation set.
    /// * `None` — **"this reference must not be used."** The collector could not
    ///   answer: the offset is outside the reservation, or relocation was needed
    ///   and failed. [`load_barrier_slow`] then marks nothing, heals nothing,
    ///   and calls [`on_forward_failure`](Self::on_forward_failure), which
    ///   diverges.
    ///
    /// ## Why there is an error channel at all (added 2026-08-07, finding N4)
    ///
    /// This method used to return a plain `u64`, and that was a genuine
    /// trait-composition defect rather than a doc gap.
    /// `ZRelocate::forward_offset` returns `Result<Option<u64>, _>` and its own
    /// docs state that **the barrier must not fall back to the input offset on
    /// `Err`**: the from-space page may already be quarantined, so handing back
    /// a from-space reference is the use-after-free the relocator's ZR-1
    /// invariant exists to prevent. With an infallible `forward`, that forbidden
    /// fallback was *the only body an implementor could write* — and
    /// `relocate.rs`'s own doc comment printed it as the sanctioned one, ten
    /// lines below the paragraph forbidding it.
    ///
    /// `Option<u64>` rather than `Result<u64, E>` because the barrier has
    /// nothing to do with an error *value*: there is exactly one thing it can
    /// do with a failure, the implementor is the only one who knows why it
    /// happened, and an error type here would have to be owned by this module
    /// and re-mapped by every implementor.
    ///
    /// ## `Some(0)`
    ///
    /// **Offset `0` is a real object location, not null** — `vaddr::color(0, c)`
    /// is a well-formed non-null word precisely because the tag bit distinguishes
    /// it from [`Z_NULL`] (`vaddr.rs`'s `color_offset_roundtrip_many_offsets`
    /// asserts this). The slow path therefore treats `Some(0)` as a
    /// forwarding-table defect **only when the input was non-zero**; see
    /// [`load_barrier_slow`]. That case keeps the stale offset and warns, and is
    /// deliberately *not* routed to `on_forward_failure`: the table answered, it
    /// just answered implausibly, and turning a suspicious answer into a VM
    /// abort is a different and much larger decision.
    ///
    /// ## A value outside `address_mask`
    ///
    /// Caught by [`is_bare_offset`] on the slow path, in release builds as well
    /// as debug, and treated as a `forward` failure — because on Linux a
    /// mistakenly-returned machine pointer (`0x7f…`) would otherwise be OR'd
    /// straight into the healed word and corrupt its metadata field, and be
    /// handed to [`mark_live`](Self::mark_live) where it marks nothing.
    fn forward(&self, addr: u64) -> Option<u64>;

    /// What to do when [`forward`](Self::forward) cannot produce a usable
    /// destination. **This diverges, and that is the contract.**
    ///
    /// *Added 2026-08-07 with finding N4. `stale_offset` is the offset the
    /// barrier loaded, supplied for the diagnostic only — it must not be
    /// returned to a mutator.*
    ///
    /// # Why the only sanctioned behaviour is "do not return"
    ///
    /// The barrier owes its caller a `u64`, and in this state there is no
    /// correct one. Every in-band answer is a known defect:
    ///
    /// * **the stale offset** — the fallback `relocate.rs` explicitly forbids.
    ///   The from-space page may already be quarantined, so this is the
    ///   use-after-free the whole relocation protocol is built to prevent, and
    ///   its crash site is arbitrarily far from here;
    /// * **null** — a `NullPointerException` in unrelated Java code, i.e. a
    ///   collector defect silently rewritten as a language-level event the
    ///   application is entitled to catch and continue from;
    /// * **the un-forwarded destination** — the same as the first, plus a
    ///   corrupted metadata field if it was out of domain.
    ///
    /// So the default panics, loudly and with the finding named. It is a
    /// deterministic, local, greppable failure in place of a non-deterministic,
    /// remote one. A VM that has a fatal-error path (a crash log, a core dump,
    /// `abort()`) **should override this** and call it; the `!` return type is
    /// what makes "and then return something anyway" unexpressible in the
    /// override.
    ///
    /// # When this can fire
    ///
    /// Only while [`is_relocating`](Self::is_relocating) is true, and
    /// `relocate.rs` states that `ZRelocateWorkers::relocate_set` must not be
    /// enabled until a thread-stack handshake exists. So on today's execution
    /// path this is unreachable, and it is written to be correct on the day it
    /// becomes reachable rather than discovered then.
    fn on_forward_failure(&self, stale_offset: u64) -> ! {
        panic!(
            "ZBarrierContext::forward could not resolve heap offset {stale_offset:#x} \
             during relocation, and there is no value the load barrier may return \
             instead: the stale offset is a from-space reference (the fallback \
             ZRelocate::forward_offset forbids), and null is a NullPointerException in \
             unrelated Java code. Override ZBarrierContext::on_forward_failure with this \
             VM's fatal-error path. See gc/src/zgc/barrier.rs, finding N4"
        )
    }

    /// Enqueue `addr` for concurrent marking.
    ///
    /// Called from mutator threads on the barrier slow path. Must not take a
    /// process-global lock per call — see the contract note on the trait.
    ///
    /// # Implementor contract: `addr` is a heap OFFSET, and `zgc::mark` is not
    ///
    /// *Added 2026-08-07 (finding N3), and this omission was rated the most
    /// dangerous item of the cross-module audit that found it.*
    ///
    /// `addr` is a bare 42-bit heap offset, like everything else this trait
    /// carries — [`load_barrier_slow`] passes the *destination* offset, already
    /// checked by [`is_bare_offset`]. The marking engine on the other side of
    /// the obvious wiring is the **opposite** domain:
    /// [`ZMarkContext`](crate::zgc::mark::ZMarkContext) declares unmasked
    /// machine addresses of object bases and the production implementation
    /// dereferences them.
    ///
    /// **So an implementation that routes to a
    /// [`ZMarkHandle`](crate::zgc::mark::ZMarkHandle) must call
    /// [`mark_live_offset`](crate::zgc::mark::ZMarkHandle::mark_live_offset)
    /// (or `mark_live_buffered_offset`), never plain `ZMarkHandle::mark_live`.**
    /// The `_offset` entry points convert with `ZMarkContext::heap_base` and
    /// refuse loudly when they cannot; the plain ones assume you already did.
    ///
    /// Getting it wrong is silent, which is why it is spelled out here rather
    /// than left to the reader: `ZMarkContext::is_in_heap` is a registry
    /// membership test, an offset is not a registered object base, so the mark
    /// is refused as a **wild pointer**, counted in
    /// `ZMarkStats::off_heap_children`, and dropped — and the object this
    /// barrier was keeping alive is swept. No crash, no assertion, and a
    /// diagnostic counter that reads like the collector working correctly.
    ///
    /// # Implementor contract: thread death
    ///
    /// *Added 2026-08-07 with the audit of `zgc::mark`'s Finding A.*
    ///
    /// Whatever buffering this routes through, **an entry pending in a
    /// per-thread buffer must survive the owning thread's exit**. The two
    /// in-tree hand-offs discharge that differently and neither covers the
    /// other (the module header's "What this registry covers" section spells the
    /// disjointness out):
    ///
    /// * [`z_mark_thread_local_log`] into a [`ZMarkQueue`] — covered by the
    ///   registry and orphan-parking below;
    /// * [`ZMarkHandle::mark_live_buffered`](crate::zgc::mark::ZMarkHandle::mark_live_buffered)
    ///   into a [`ZMarkMutatorBuffer`](crate::zgc::mark::ZMarkMutatorBuffer) —
    ///   covered **only** if the buffer came from
    ///   [`ZMarkHandle::new_buffer`](crate::zgc::mark::ZMarkHandle::new_buffer),
    ///   which attaches it to its pool so its `Drop` can flush.
    ///   `ZMarkMutatorBuffer::new` produces a detached buffer that is covered by
    ///   nothing, and losing one of *its* entries is worse than losing one of
    ///   this module's: the mark bit is set before the address is buffered, so
    ///   the object ends up marked-and-unscanned and no later load barrier can
    ///   rediscover it.
    ///
    /// The gate question — whether to check
    /// [`is_marking`](Self::is_marking) before calling through — is answered on
    /// that method. Short version: the slow path already does, and that is the
    /// gate.
    fn mark_live(&self, addr: u64);

    /// Counters to update. Defaults to the process-wide fallback block;
    /// production heaps should return their own.
    fn stats(&self) -> &ZBarrierStats {
        global_z_barrier_stats()
    }
}

// ---------------------------------------------------------------------------
// Barrier variants
// ---------------------------------------------------------------------------

/// Which load barrier is being applied.
///
/// The variants differ **only in their slow-path behaviour**; the fast path is
/// identical for all of them (that is the point — one test, many semantics).
///
/// # `Weak` versus `KeepAlive`: read this before touching either
///
/// These two look interchangeable and are not. Getting them backwards does
/// not fail loudly; it corrupts reference semantics in one of two directions:
///
/// * Using **`KeepAlive` where `Weak` was meant** (e.g. in the reference
///   processor's own reachability probe) marks the referent live from the act
///   of asking whether it is live. `WeakReference` then never clears, the
///   `ReferenceQueue` never fires, and `Cleaner` never runs — an unbounded
///   leak that looks like "the GC isn't collecting" rather than like a
///   barrier bug.
///
/// * Using **`Weak` where `KeepAlive` was meant** (e.g. for `Reference.get()`)
///   hands Java code a reference to an object the collector has not been told
///   is reachable. Marking finishes without it, the object is reclaimed, and
///   the mutator is left holding a dangling reference — a use-after-free whose
///   crash site is arbitrarily far from the barrier.
///
/// The rule, matching HotSpot ZGC:
///
/// | call site | kind |
/// |---|---|
/// | ordinary `getfield` / `aaload` of a reference | [`ZBarrierKind::Load`] |
/// | `getfield` of a `volatile` reference, `VarHandle` acquire read | [`ZBarrierKind::LoadVolatile`] |
/// | `Reference.get()` from Java | [`ZBarrierKind::KeepAlive`] |
/// | the reference processor reading a referent to *decide* its fate | [`ZBarrierKind::Weak`] |
/// | root scanning (stack slots, JNI handles, static fields) | [`ZBarrierKind::Root`] |
///
/// See `cratonvm_gc_core::reference` for the processor side and the `INT-8` referent
/// skip-set note in `zgc.rs::collect_garbage` for the corresponding rule in
/// the STW marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ZBarrierKind {
    /// Plain reference read. Marks the referent live during concurrent mark.
    Load,
    /// Volatile / acquire reference read. Same slow path as [`Load`], but the
    /// slot is loaded with `Acquire` and healed with `AcqRel` so the barrier
    /// cannot weaken the Java-level volatile edge the program is relying on.
    ///
    /// [`Load`]: ZBarrierKind::Load
    LoadVolatile,
    /// Weak read of a `java.lang.ref` referent that must **not** resurrect it.
    /// Follows forwarding and self-heals like the others; the *only*
    /// difference is that it never calls
    /// [`ZBarrierContext::mark_live`].
    ///
    /// # Divergence from OpenJDK (recorded 2026-08-07, closed 2026-09-21)
    ///
    /// OpenJDK gives a weak load its own **mask**, not just its own marking
    /// behaviour: `weak_bad = (good | Remapped | Finalizable) ^ metadata`, so a
    /// merely-`Remapped` or finalizable-reachable referent takes the *fast*
    /// path during marking instead of the slow one.
    /// [`ZGoodMask::weak_bad`](crate::zgc::vaddr::ZGoodMask::weak_bad) exists
    /// and implements exactly that, and until 2026-09-21 **nothing in this
    /// module consumed it**: this variant tested the ordinary bad mask and
    /// simply suppressed `mark_live`.
    ///
    /// It now has a mask of its own end to end.
    /// [`ZBarrierContext::weak_bad_mask`] is what [`z_barrier`] tests for this
    /// variant, and [`ZBarrierContext::weak_heal_color`] is what
    /// [`load_barrier_slow`] would stamp. Both **default to the strong
    /// answers**, so every implementor's behaviour is unchanged until it
    /// overrides them — the divergence that is closed is the structural one
    /// (the weak path was the strong path with one call removed), not a
    /// behavioural one that could only be closed with the load barrier armed.
    ///
    /// **That "performance only" reading was wrong, and it is now corrected in
    /// the code rather than only in this comment** (2026-09-20). What this
    /// paragraph used to say was: *"the difference is a performance and
    /// self-heal-churn difference, not a correctness one — a weak load that
    /// takes an unnecessary slow path still returns the right address and still
    /// does not mark."* The first half is true and the conclusion does not
    /// follow, because the missing mask is not the only thing OpenJDK's weak
    /// barrier gets out of it. The mask is also what makes OpenJDK's weak heal
    /// SAFE: `weak_bad` admits `Remapped`, so a weak slow path heals to a colour
    /// that is good-for-weak and still **bad for a strong load during marking**.
    /// With one good colour we cannot express that, so a weak heal would stamp
    /// the mark colour and send every later `getfield` of that slot down the
    /// fast path — past the `mark_live` the referent needed. See exit 5 on
    /// [`load_barrier_slow`]: a weak load now does not heal at all while marking
    /// is running.
    ///
    /// So the remaining divergence really is performance-only: without
    /// `weak_bad` a weak load on a merely-`Remapped` referent takes a slow path
    /// it need not take, and (since 2026-09-20) does not amortise it.
    ///
    /// **And the suppression no longer rests on that special case**
    /// (2026-09-21). Exit 5's gate is now the property — *would the word I am
    /// about to write be good under the strong `bad_mask`?* — rather than the
    /// variant. With the default masks the answer is always yes and the
    /// behaviour is identical; with a context that overrides
    /// [`ZBarrierContext::weak_bad_mask`] and
    /// [`ZBarrierContext::weak_heal_color`] the answer becomes no on its own and
    /// the weak path amortises again. Nothing about exit 5 has to be
    /// re-litigated when the load barrier is armed, which was the point of
    /// moving it onto an invariant.
    Weak,
    /// Read of a weak referent that MUST resurrect it — `Reference.get()`.
    /// Marks unconditionally-if-marking, exactly like [`Load`]; it is a
    /// distinct variant so the call sites are greppable and so the counters
    /// can tell resurrection apart from ordinary reads.
    ///
    /// [`Load`]: ZBarrierKind::Load
    KeepAlive,
    /// Root scan of a thread stack slot / JNI handle / static field. Marks and
    /// heals; the "slot" is a root location rather than a heap field, which
    /// the barrier does not need to distinguish — an `&AtomicU64` is an
    /// `&AtomicU64`.
    Root,
}

impl ZBarrierKind {
    /// Whether this barrier keeps the loaded referent alive.
    ///
    /// Everything except [`ZBarrierKind::Weak`] does. See the enum docs for
    /// why that one exception is load-bearing.
    #[inline(always)]
    pub fn marks(self) -> bool {
        !matches!(self, ZBarrierKind::Weak)
    }

    /// Whether this barrier must preserve acquire/release semantics on the
    /// slot access.
    #[inline(always)]
    pub fn is_volatile(self) -> bool {
        matches!(self, ZBarrierKind::LoadVolatile)
    }
}

// ---------------------------------------------------------------------------
// Fast path
// ---------------------------------------------------------------------------

/// Outcome of the fast-path color test.
///
/// A two-variant enum rather than `Option<u64>` because both arms carry a
/// value and they are different values: `Good` carries the finished,
/// color-stripped address, `Bad` carries the **raw** observed word, which the
/// slow path needs verbatim as the CAS comparand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZFastPath {
    /// Good color (or null): the payload is a bare **heap offset** — the word
    /// with every non-address bit masked off.
    ///
    /// **It is not a machine pointer and must not be dereferenced.** Add
    /// [`ZVirtualAddressSpace::base`](crate::zgc::vaddr::ZVirtualAddressSpace::base)
    /// first, via
    /// [`address_for_offset`](crate::zgc::vaddr::ZVirtualAddressSpace::address_for_offset).
    /// [`ZBarrierContext::address_mask`] excludes both the metadata field and
    /// [`Z_COLORED_TAG`], and the module header's "Address domain" section
    /// records why the offset — not the pointer — is what this type carries.
    ///
    /// `Good(0)` is ambiguous between null and an object at heap offset 0; the
    /// *classification* is not (both are `Good`, and the caller wanted the same
    /// answer either way), but a caller that needs to tell them apart must look
    /// at the raw word.
    Good(u64),
    /// Bad color: the payload is the raw word as observed in the slot.
    Bad(u64),
}

/// Pure color test against explicit masks, **bad-mask form**. No memory
/// access, no side effects. This is what [`z_barrier`] uses.
///
/// # Why the bad mask and not the good one (changed 2026-08-07)
///
/// `Z_NULL` is the all-zero word, so `0 & bad_mask == 0` and **null classifies
/// `Good` for free** — one `test`, one not-taken `jnz`, no separate null
/// branch. That is the form OpenJDK's barrier emits and the reason
/// [`crate::zgc::vaddr::is_bad`] exists alongside `is_good`.
///
/// The good-mask form ([`classify_masked`]) needs an explicit null test, and
/// the one this module originally shipped — `raw & address_mask == 0` — became
/// **wrong** when `vaddr` landed: a well-formed colored word for an object at
/// heap **offset 0** has a zero address payload but is not null (the tag bit
/// and the color bit are what make it non-null, and `vaddr`'s
/// `color_offset_roundtrip_many_offsets` asserts exactly that). The old test
/// would have handed the mutator `Good(0)` — a silent null for a live object.
/// The bad-mask form has no such test to get wrong.
///
/// The two forms agree for every well-formed colored word, because such a word
/// carries exactly one metadata bit and `bad == good ^ Z_METADATA_MASK`.
///
/// # Where they disagree, and why it matters here more than in OpenJDK
///
/// They disagree for an **uncolored** word — one with no metadata bit at all,
/// i.e. a plain machine pointer sitting in a reference slot. The good-mask form
/// calls it `Bad` and sends it to the slow path; the bad-mask form calls it
/// `Good` and hands its low 42 bits straight back.
///
/// In OpenJDK that case cannot arise: every reference slot in a ZGC heap is
/// colored by construction. **In CratonVM it can**, because ZGC's slot coverage
/// is being rolled out shape by shape (compact fields, then arrays, then legacy
/// cells, then statics — see the module header), and an un-migrated writer
/// stores a bare pointer. Such a word passes this test, and `raw & address_mask`
/// then truncates a machine pointer to 42 bits — garbage, silently.
///
/// This is not an argument for the good-mask form (which would send every null
/// down the slow path to get there). It is an argument for the *coverage* rule
/// the module header states: a slot is either fully barriered on both the read
/// and the write side, or it is not handed to this function at all.
/// [`crate::zgc::vaddr::is_colored_word`] is the audit primitive for asserting
/// that at a boundary; it is deliberately not on this path, which must stay a
/// `test` and a `jnz`.
#[inline(always)]
pub fn classify_bad_masked(raw: u64, bad_mask: u64, address_mask: u64) -> ZFastPath {
    if raw & bad_mask == 0 {
        ZFastPath::Good(raw & address_mask)
    } else {
        ZFastPath::Bad(raw)
    }
}

/// Pure color test against explicit masks, **good-mask form**. No memory
/// access, no side effects.
///
/// Kept for callers that hold a good mask and not a bad one, and used by this
/// module's tests. Prefer [`classify_bad_masked`] on any hot path — see its
/// docs for what the extra null test in this form costs and what it once got
/// wrong.
///
/// Null ([`Z_NULL`], the all-zero word) is classified `Good`: a null reference
/// has nothing to mark and nowhere to be relocated to. The test is on the
/// **whole word**, not on the address payload, because a colored word for heap
/// offset 0 has a zero payload and is not null.
#[inline(always)]
pub fn classify_masked(raw: u64, good_mask: u64, address_mask: u64) -> ZFastPath {
    // Good-color test first: it is the overwhelmingly common case, so it is
    // the one the branch predictor should be trained on.
    if raw & good_mask != 0 || raw == Z_NULL {
        ZFastPath::Good(raw & address_mask)
    } else {
        ZFastPath::Bad(raw)
    }
}

/// [`classify_masked`] with the default [`Z_OFFSET_MASK`].
#[inline(always)]
pub fn classify(raw: u64, good_mask: u64) -> ZFastPath {
    classify_masked(raw, good_mask, Z_OFFSET_MASK)
}

/// [`classify_bad_masked`] with the default [`Z_OFFSET_MASK`].
#[inline(always)]
pub fn classify_bad(raw: u64, bad_mask: u64) -> ZFastPath {
    classify_bad_masked(raw, bad_mask, Z_OFFSET_MASK)
}

/// The load-barrier fast path: load the slot, test the color, return the
/// address.
///
/// This is the code that runs on **every reference load in the program**, so
/// it is `#[inline(always)]` and consists of a load, an `AND`, a compare and a
/// not-taken branch. Nothing else may be added here — no counter increment
/// (see [`ZBarrierStats`]'s note on why `fast_path_hits` is gated), no
/// `tracing` call, no function call of any kind.
///
/// ## Why the load is `Relaxed`
///
/// The barrier reads one 64-bit word and, on a hit, hands its address payload
/// straight back. It establishes no happens-before edge of its own, and it
/// does not need to:
///
/// * **Java-level ordering** on the referenced object's *contents* is the
///   storing side's job. A correctly-synchronised Java program publishes an
///   object through a `volatile` store, a `final` field, or a lock release;
///   an incorrectly-synchronised one is racy by the JMM's own rules and the
///   barrier owes it nothing. [`ZBarrierKind::LoadVolatile`] upgrades this
///   load to `Acquire` precisely for the case where the *program* asked for
///   the edge.
///
/// * **GC-level ordering** — seeing the relocated copy's bytes rather than a
///   half-copied one — is carried by a different chain: the relocating thread
///   copies the object and then publishes its forwarding entry with a release
///   store; [`ZBarrierContext::forward`] reads it with acquire; and the heal
///   CAS in [`load_barrier_slow`] republishes with `AcqRel`. A relaxed load
///   that observes an already-healed value therefore observes it through that
///   release/acquire chain, and the subsequent dereference is
///   address-dependent on it. (This is the same argument HotSpot's ZGC makes
///   for its plain-`mov` fast path.)
///
/// Anything stronger here would put a fence on the hottest instruction
/// sequence in the VM to buy a guarantee no caller uses.
#[inline(always)]
pub fn load_barrier_fast(slot: &AtomicU64, good_mask: u64) -> ZFastPath {
    classify_masked(slot.load(Ordering::Relaxed), good_mask, Z_OFFSET_MASK)
}

/// [`load_barrier_fast`] for a heap whose address payload is not
/// [`Z_OFFSET_MASK`].
#[inline(always)]
pub fn load_barrier_fast_masked(slot: &AtomicU64, good_mask: u64, address_mask: u64) -> ZFastPath {
    classify_masked(slot.load(Ordering::Relaxed), good_mask, address_mask)
}

/// The load-barrier fast path in **bad-mask form** — what [`z_barrier`] calls.
///
/// Same load, same `#[inline(always)]` budget, one fewer branch: see
/// [`classify_bad_masked`] for why the bad mask is the right comparand.
#[inline(always)]
pub fn load_barrier_fast_bad(slot: &AtomicU64, bad_mask: u64, address_mask: u64) -> ZFastPath {
    classify_bad_masked(slot.load(Ordering::Relaxed), bad_mask, address_mask)
}

/// Acquire-ordered fast path, for [`ZBarrierKind::LoadVolatile`].
///
/// `Acquire` here is not the barrier's own requirement — it is the Java
/// program's. A `volatile` reference read must see everything the publishing
/// thread wrote before its `volatile` store; if the barrier downgraded that
/// load to `Relaxed` it would silently delete the edge the program asked for.
#[inline(always)]
pub fn load_barrier_fast_acquire(slot: &AtomicU64, good_mask: u64, address_mask: u64) -> ZFastPath {
    classify_masked(slot.load(Ordering::Acquire), good_mask, address_mask)
}

/// Acquire-ordered fast path in bad-mask form, for
/// [`ZBarrierKind::LoadVolatile`]. See [`load_barrier_fast_acquire`] for why
/// the ordering is `Acquire` and [`classify_bad_masked`] for why the mask is
/// the bad one.
#[inline(always)]
pub fn load_barrier_fast_bad_acquire(
    slot: &AtomicU64,
    bad_mask: u64,
    address_mask: u64,
) -> ZFastPath {
    classify_bad_masked(slot.load(Ordering::Acquire), bad_mask, address_mask)
}

// ---------------------------------------------------------------------------
// Address-domain check
// ---------------------------------------------------------------------------

/// Is `value` a bare heap offset under `address_mask` — no color bits, no tag,
/// and no heap base?
///
/// # What this is for (added 2026-08-07 with the address-domain decision)
///
/// This module's addresses are **heap offsets**, not machine pointers (module
/// header, "Address domain"). The one way that decision can be violated at
/// runtime is a [`ZBarrierContext::forward`] implementation that returns a
/// process address — which is the natural mistake, because `page.rs` deals in
/// process addresses and a `ZgcRealHeap` implementor holds both.
///
/// The violation is **machine-checkable**, which is why this module needs a
/// predicate where `forwarding.rs` needed a newtype: an offset satisfies this by
/// construction (it came out of `raw & address_mask`), while a Linux heap
/// reservation sits near `0x7f…` — a ~47-bit number, five bits (a factor of 32)
/// past the 42-bit offset field. So the confusion cannot hide — provided
/// something looks.
///
/// [`load_barrier_slow`] is what looks, and it looks in release builds too: the
/// stray bits would otherwise be OR'd into the healed word and land in its
/// metadata field, silently changing the color of a live reference. On Windows,
/// where the reservation is placed low, that same mistake produces a *plausible*
/// word and no symptom at all — which is precisely the asymmetry that makes a
/// debug-only check insufficient here.
#[inline]
pub const fn is_bare_offset(value: u64, address_mask: u64) -> bool {
    value & !address_mask == 0
}

// ---------------------------------------------------------------------------
// Slow path
// ---------------------------------------------------------------------------

/// The load-barrier slow path: fix the value up and **heal the slot**.
///
/// `observed` must be the exact raw word the fast path loaded from `slot`; it
/// is the CAS comparand, and passing anything else makes the heal a
/// guaranteed loss.
///
/// Returns the correct, color-stripped address **regardless of whether the
/// heal succeeded**.
///
/// # Steps
///
/// 1. **Follow relocation.** If relocation is in progress, ask
///    [`ZBarrierContext::forward`] where the object is now. Not relocating ⇒
///    the address is its own destination and the table is not consulted at all.
///
///    Two answers are *failures* and neither returns: `None` ("this reference
///    must not be used"), and a value outside `address_mask` (a machine pointer
///    or a still-coloured word). Both bump
///    [`ZBarrierStats::forward_failures`] and then call
///    [`ZBarrierContext::on_forward_failure`], whose default panics. Nothing is
///    marked and nothing is healed on those paths — healing would stamp a good
///    colour onto a reference the collector just said is unusable, which would
///    make every future load of that slot take the *fast* path straight to it.
///    See finding N4 on `forward` for why there is no in-band answer.
///
/// 2. **Mark.** Unless this is a [`ZBarrierKind::Weak`] load, and only while
///    marking is running, hand the *destination* address (not the stale one)
///    to [`ZBarrierContext::mark_live`].
///
/// 3. **Heal.** CAS `observed -> destination | heal_color` into `slot`.
///
/// # Exit table — every path out of this function accounts for itself
///
/// *Written 2026-08-07 after the cross-module suite found two entrants with no
/// recorded CAS outcome. The bug turned out to be in the assertion, not here
/// (it compared against a **thread count**, and two threads had hit the fast
/// path on the already-healed slot) — but auditing this table is what showed
/// that, and it also showed that three of the four exits genuinely were
/// unaccounted.*
///
/// | # | exit | condition | CAS? | buckets bumped |
/// |---|---|---|---|---|
/// | 1 | `return 0` | `observed == Z_NULL` | no | `null_slow_paths` + **`heal_skipped`** |
/// | 2 | diverges | `forward` ⇒ `None` | no | `forward_failures` + **`heal_skipped`** |
/// | 3 | diverges | `forward` ⇒ non-`is_bare_offset` | no | `forward_failures` + **`heal_skipped`** |
/// | 5 | `return destination` | [`ZBarrierKind::Weak`] **and** `is_marking()` | no | `weak_slow_paths` + **`heal_skipped`** |
/// | 4 | `return destination` | everything else | **yes, exactly one** | `heal_cas_wins` **xor** `heal_cas_losses` |
///
/// Every exit is preceded by exactly one `slow_path_entries` bump at the top of
/// the function, so
/// `heal_cas_wins + heal_cas_losses + heal_skipped == slow_path_entries`
/// holds for a quiesced barrier. That identity is stated on [`ZBarrierStats`]
/// and pinned by `every_slow_path_exit_lands_in_exactly_one_heal_bucket`.
/// **A new early return added below must extend this table and pick a bucket.**
///
/// Exit 5 is numbered out of order on purpose: it is the newest (2026-09-20) and
/// exit 4 is deliberately kept last in the table as it is last in the function —
/// "everything else" has to stay the final row or the table stops describing a
/// decision procedure. It is the one exit that was added *because* the table
/// existed: auditing the rows is what showed that a weak load's heal stamps the
/// mark colour into a slot the barrier just decided not to mark.
///
/// Exits 2 and 3 bump their counters *before* diverging, so the identity is
/// readable from a crash dump. Two paths deliberately do **not** exit here:
/// the `forwarded == 0 && stale != 0` warning (a suspicious answer is not a
/// refusal — it keeps the stale offset and falls through to the CAS) and
/// `kind.marks() == true` with `is_marking() == false` (that skips the *mark*,
/// never the heal — outside a mark cycle the good colour is not a mark colour,
/// so healing to it cannot hide anything from a marker that is not running).
///
/// # The CAS, in detail
///
/// ```text
///   slot.compare_exchange(observed, healed, AcqRel, Acquire)
/// ```
///
/// * **`compare_exchange`, not `compare_exchange_weak`.** The weak form may
///   fail spuriously, and because we deliberately do not retry, a spurious
///   failure would leave the slot permanently unhealed — every future load of
///   it would re-enter the slow path. That is the one thing this barrier
///   exists to avoid.
///
/// * **Success `AcqRel`.** The release half publishes the healed value: a
///   thread that later loads this slot with a relaxed load and dereferences
///   the result must see the relocated copy's bytes, which were written before
///   the forwarding entry we read acquire-ordered in step 1. The acquire half
///   orders our own subsequent reads of the object against whatever the
///   previous writer of this slot did.
///
/// * **Failure `Acquire`.** We discard the competing value, so `Relaxed`
///   would do; `Acquire` costs nothing measurable on a cold path and means
///   that if a future change *does* start consuming the competing value it
///   cannot be silently unordered.
///
/// * **Exactly one attempt. Never a loop.** A lost CAS means the slot changed
///   under us, and there are only two ways that happens:
///
///   1. Another barrier healed the same value. The slot already holds what we
///      would have written; retrying is pure waste.
///   2. A **mutator stored a different reference**. Retrying here would
///      overwrite the mutator's store with our stale value — a lost update
///      that resurrects a dead reference and later presents as a
///      use-after-free far from this code. This is not a performance
///      argument; a retry loop here is a correctness bug.
///
///   Either way we return `destination`, because the barrier's contract is
///   "the correct address *for the value I loaded*". What the slot holds
///   afterwards belongs to whoever won.
///
/// `#[inline(never)]` keeps this out of the caller's instruction stream so the
/// fast path stays a handful of instructions.
#[inline(never)]
pub fn load_barrier_slow<C: ZBarrierContext + ?Sized>(
    slot: &AtomicU64,
    observed: u64,
    ctx: &C,
    kind: ZBarrierKind,
) -> u64 {
    let stats = ctx.stats();
    stats.slow_path_entries.fetch_add(1, Ordering::Relaxed);

    let address_mask = ctx.address_mask();
    // Null is the ALL-ZERO WORD, not merely a zero address payload. Corrected
    // 2026-08-07 when `zgc::vaddr` landed: a well-formed colored word for an
    // object at heap offset 0 has a zero payload but carries `Z_COLORED_TAG`
    // and a color bit, so the old `observed & address_mask == 0` test would
    // have short-circuited a live object to null here.
    if observed == Z_NULL {
        // Defensive: the fast path classifies null as good, so reaching here
        // means a direct call with a null observation. Nothing to do, and
        // nothing to heal (there is no "correct color" for null).
        //
        // `heal_skipped` as well as `null_slow_paths`: this is an entrant that
        // will attempt no CAS, and the accounting identity on `ZBarrierStats`
        // requires every entrant to land in exactly one of wins/losses/skipped.
        stats.null_slow_paths.fetch_add(1, Ordering::Relaxed);
        stats.heal_skipped.fetch_add(1, Ordering::Relaxed);
        return 0;
    }
    let stale = observed & address_mask;

    // -- 1. Follow relocation ------------------------------------------------
    let mut destination = stale;
    if ctx.is_relocating() {
        // `None` is the error channel added 2026-08-07 (finding N4). There is
        // no value the barrier may return here — the stale offset is the
        // from-space reference `ZRelocate::forward_offset` explicitly forbids
        // falling back to, and null is an NPE in unrelated Java code — so this
        // does not return. The counters are bumped FIRST so they survive the
        // diverging hook and are readable from a crash dump.
        let forwarded = match ctx.forward(stale) {
            Some(f) => f,
            None => {
                stats.forward_failures.fetch_add(1, Ordering::Relaxed);
                stats.heal_skipped.fetch_add(1, Ordering::Relaxed);
                tracing::error!(
                    target: "zgc::barrier",
                    stale_offset = stale,
                    "ZBarrierContext::forward could not resolve a live reference during \
                     relocation. Not healing, not marking, and not returning a value: \
                     see ZBarrierContext::on_forward_failure"
                );
                ctx.on_forward_failure(stale);
            }
        };
        // The address-domain check, moved inside the relocating arm and
        // promoted from "log and carry on" to a `forward` failure (2026-08-07,
        // finding N3). It is only reachable from here: when relocation is off,
        // `destination` is `observed & address_mask` and is a bare offset by
        // construction.
        //
        // Deliberately NOT a bare `debug_assert`: on Linux a `forward` that
        // answered with a machine pointer puts `0x7f…` here, whose stray bits
        // would be OR'd into the healed word and land in its metadata field —
        // silently recolouring a live reference — and would then be handed to
        // `mark_live`, where an offset-domain API silently marks nothing. On
        // Windows the reservation is placed low, the same mistake produces a
        // plausible word, and nothing at all happens. A check that only runs in
        // debug builds would therefore be a check that only runs where the bug
        // is invisible.
        //
        // It previously logged this exact error and then healed anyway, which
        // made the message ("healing will corrupt the metadata field") a
        // prediction rather than a description.
        if !is_bare_offset(forwarded, address_mask) {
            let stray_bits = forwarded & !address_mask;
            stats.forward_failures.fetch_add(1, Ordering::Relaxed);
            stats.heal_skipped.fetch_add(1, Ordering::Relaxed);
            tracing::error!(
                target: "zgc::barrier",
                destination = forwarded,
                address_mask = address_mask,
                stray_bits = stray_bits,
                "ZBarrierContext::forward returned a value outside the barrier's \
                 address domain. This module traffics in bare heap OFFSETS (see the \
                 module header): the value carries bits above address_mask, which \
                 means it is a machine pointer, or still coloured. Healing would \
                 corrupt the metadata field of the slot, and marking it would be \
                 discarded as a wild pointer"
            );
            debug_assert!(
                is_bare_offset(forwarded, address_mask),
                "ZBarrierContext::forward must return a bare heap offset — no \
                 colour bits, no tag, no heap base"
            );
            ctx.on_forward_failure(stale);
        }
        if forwarded == 0 && stale != 0 {
            // A forwarding table that maps a live, non-null address to null is
            // broken. Propagating the null would turn a GC bug into a
            // NullPointerException in unrelated Java code; keeping the stale
            // address at least keeps the failure local and greppable.
            //
            // `&& stale != 0` because under `vaddr` the payload is an OFFSET
            // and offset 0 is a real heap location, whose identity forwarding
            // legitimately returns 0.
            tracing::warn!(
                target: "zgc::barrier",
                stale_address = stale,
                "forwarding table returned null for a live address; keeping the \
                 stale address (this is a forwarding-table defect, not a \
                 mutator error)"
            );
        } else if forwarded != stale {
            stats.forwards_followed.fetch_add(1, Ordering::Relaxed);
            destination = forwarded;
        }
    }
    debug_assert!(
        is_bare_offset(destination, address_mask),
        "destination left the relocation step outside the address domain; the \
         check inside the `is_relocating` arm should have diverged"
    );

    // -- 2. Mark -------------------------------------------------------------
    //
    // The DESTINATION is marked, never the stale address: after relocation the
    // stale copy is about to be reclaimed, and marking it would either be a
    // no-op or (worse) keep a from-space page alive.
    //
    // `destination` is a bare heap OFFSET, and `zgc::mark` wants a machine
    // address: an implementation of `mark_live` that routes to a `ZMarkHandle`
    // must use `mark_live_offset`, not `mark_live`. See that method's docs —
    // the wrong one is silently discarded as a wild pointer (finding N3).
    //
    // Read once and reused by the heal gate below: a `Weak` load needs the same
    // answer, and asking twice would let the phase change between the two
    // questions this function has to answer consistently.
    let marking = ctx.is_marking();
    if kind.marks() {
        if marking {
            ctx.mark_live(destination);
            stats.marks_enqueued.fetch_add(1, Ordering::Relaxed);
        }
    } else {
        stats.weak_slow_paths.fetch_add(1, Ordering::Relaxed);
    }

    // -- 3. Heal -------------------------------------------------------------
    //
    // EXIT 5: A WEAK LOAD MUST NOT HEAL WHILE MARKING IS RUNNING.
    //
    // *Found and fixed 2026-09-20. The enum's own docs called the divergence
    // from OpenJDK here "a performance and self-heal-churn difference, not a
    // correctness one". That was wrong, and this is the counterexample.*
    //
    // `heal_color()` is `Z_COLORED_TAG | good_mask()`, and during marking the
    // good mask IS the cycle's mark colour. So a weak load — which by
    // construction does **not** call `mark_live` — would stamp the mark colour
    // into the slot. Every later load of that slot, including an ordinary
    // strong `getfield`, then passes `classify_bad_masked` on the FAST path and
    // never reaches `mark_live` either. The referent is reachable, was read by
    // the application, and is unmarked: the mark set closes without it and the
    // sweep frees it. A use-after-free, produced by a barrier that was correct
    // at every individual step.
    //
    // OpenJDK does not have this problem because its weak barrier is a
    // different *mask* as well as different marking: `weak_bad` admits
    // `Remapped`, so `weak_load_barrier_on_oop_slow_path` returns a
    // remapped-coloured address, which is still BAD for a strong load during
    // marking.
    //
    // THE TEST BELOW IS NOW THAT PROPERTY, NOT THE VARIANT (2026-09-21).
    //
    // Until this round the gate was literally `!kind.marks() && marking` — a
    // special case naming the symptom. What actually makes a weak heal unsafe is
    // narrower and checkable: *the word this barrier is about to write would be
    // GOOD for a strong load*. Expressed against the masks the context already
    // publishes, that is
    //
    //     weak_healed & ctx.bad_mask() == 0
    //
    // and it is exactly the condition under which a later `getfield` of the same
    // slot fast-paths past `mark_live`. Testing the property rather than the
    // variant is what makes `ZBarrierContext::weak_heal_color` meaningful: a
    // heap whose encoding CAN produce a good-for-weak / bad-for-strong colour
    // (`vaddr`'s `Remapped` under `ZGoodMask::weak_bad`) overrides it, the test
    // stops firing on its own, and weak loads amortise again — with no edit
    // here, and with no risk that the edit forgets why the gate was there.
    //
    // For every context that does NOT override it the expression is identically
    // zero — `heal_color()` is `Z_COLORED_TAG | good_mask()`, `bad_mask()` is
    // `good_mask() ^ Z_METADATA_MASK`, the two share no bit, and `destination`
    // is a bare offset below the metadata field — so this is byte-for-byte the
    // 2026-09-20 behaviour and the tests that pin it still pass.
    //
    // The conservative answer when it does fire is to heal nothing: the slot
    // keeps its bad colour, and the next load of it — weak or strong — takes the
    // slow path and does the right thing. The cost is that weak loads do not
    // amortise DURING MARKING, which is the phase in which they are rarest
    // (reference processing runs at the mark-end safepoint). Outside marking a
    // weak load heals exactly as before, which is where healing actually pays:
    // during relocation it is what stops every load re-walking the forwarding
    // table.
    //
    // Counted in `heal_skipped`, so the accounting identity on `ZBarrierStats`
    // still balances and so the frequency is observable rather than inferred.
    let heal_color = if kind.marks() {
        ctx.heal_color()
    } else {
        ctx.weak_heal_color()
    };
    // NAMES BOTH METHODS. Until 2026-09-21 only `heal_color` could reach this
    // assertion, so the message named it alone; the selection above means a
    // `weak_heal_color` override now trips exactly the same check, and a
    // failure that points the reader at a method they did not write is a
    // failure that gets attributed to the wrong file.
    debug_assert_eq!(
        heal_color & address_mask,
        0,
        "ZBarrierContext::{} must not overlap address_mask, or healing \
         corrupts the address",
        if kind.marks() {
            "heal_color"
        } else {
            "weak_heal_color"
        }
    );
    let healed = destination | heal_color;

    // `&&` short-circuits, so `bad_mask()` is read only on a weak slow path
    // taken while marking — never on the strong path this branch sits in front
    // of.
    if !kind.marks() && marking && healed & ctx.bad_mask() == 0 {
        stats.heal_skipped.fetch_add(1, Ordering::Relaxed);
        tracing::trace!(
            target: "zgc::barrier",
            observed = observed,
            destination = destination,
            healed = healed,
            "weak load during concurrent mark: NOT healing, because the word this \
             barrier would write is GOOD under the strong bad mask — the next \
             strong load of this slot would take the fast path past the \
             mark_live that this weak load deliberately did not perform"
        );
        return destination;
    }

    match slot.compare_exchange(observed, healed, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => {
            stats.heal_cas_wins.fetch_add(1, Ordering::Relaxed);
        }
        Err(current) => {
            // Lost the race. See the "exactly one attempt" note above: we
            // accept the winner's value and do NOT retry.
            stats.heal_cas_losses.fetch_add(1, Ordering::Relaxed);
            tracing::trace!(
                target: "zgc::barrier",
                observed = observed,
                current = current,
                healed = healed,
                "self-heal CAS lost; accepting the winner's value"
            );
        }
    }

    destination
}

// ---------------------------------------------------------------------------
// The compiled barrier's contract (zt t21/zb4, stage 2e groundwork)
// ---------------------------------------------------------------------------
//
// What the JIT emits for a reference load once the good color can change, as
// executable Rust: the inline fast path ([`inline_barrier_fast`]) and the
// helper its slow branch calls ([`inline_barrier_slow`]). Nothing in the VM
// calls either yet; stage 2d's inline decode (`jit/src/x64/licm.rs`,
// `zgc_colored_slot_decode_bytes`) is the pinned-color special case of
// `inline_barrier_fast` with a bad mask of `0`, and the unit tests below pin
// that, and drive the slow path through a whole color rotation.
//
// WHY THE SIGN TEST COMES FIRST. A slot of this VM may hold a PLAIN machine
// pointer (legacy writers, plain-store mode, an address the arena cannot
// encode), which the colored bits of OpenJDK's encoding would misread: a
// pointer's bits 42..=45 are address bits, so `word & bad_mask` would send it
// to the slow path or, worse, `classify_bad_masked` would call it good and
// truncate it (that function's doc, "Where they disagree"). Bit 63 separates
// the two for one `TEST`/`JNS`, so plain words never meet the color test.

/// Outcome of [`inline_barrier_fast`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZInlineFast {
    /// The load is finished: a machine address, or `0` for null.
    Done(u64),
    /// A bad-colored word: the raw word, which [`inline_barrier_slow`] needs
    /// verbatim as its CAS comparand.
    Slow(u64),
}

/// The compiled load barrier's fast path for one loaded slot word:
///
/// ```text
///   TEST RAX, RAX ; JNS .done                 ; plain word or null: as is
///   MOV  R11, &bad_cell ; TEST RAX, [R11] ; JNZ .slow   ; bad color
///   MOV  R11, Z_OFFSET_MASK ; AND RAX, R11
///   MOV  R11, base ; ADD RAX, R11              ; base + offset
/// .done:
/// ```
///
/// `bad_mask` is the published bad mask (`vaddr::ZCompiledBadMask`); `0`
/// gives stage 2d's pinned decode. `base` is the arena base the colored
/// words' offsets are measured from. A colored word for arena offset 0 is
/// `base`, never null: null is the all-zero word, which takes the first
/// branch.
#[inline(always)]
pub const fn inline_barrier_fast(word: u64, bad_mask: u64, base: u64) -> ZInlineFast {
    if word & Z_COLORED_TAG == 0 {
        return ZInlineFast::Done(word);
    }
    if word & bad_mask != 0 {
        return ZInlineFast::Slow(word);
    }
    ZInlineFast::Done(base.wrapping_add(word & Z_OFFSET_MASK))
}

/// The helper the compiled slow branch calls (`jit_load_barrier_slow(ctx,
/// slot, word)` in the t21/zb4 design): [`load_barrier_slow`] on the
/// observed word -- forward if relocating, mark if marking and `kind` marks,
/// self-heal the slot with ONE compare-and-swap to the good color -- and the
/// machine address of the answer.
///
/// `observed` must be the exact word the fast path loaded. An untagged word
/// (never sent here by [`inline_barrier_fast`]) is answered as the plain
/// pointer it is, without touching the slot or the stats, so a caller that
/// reaches here on a stale decision cannot recolor a plain slot.
///
/// The answer for a colored word is `base + offset` even when the offset is
/// `0`: a tagged word is never null, so there is no ambiguity to resolve.
#[inline(never)]
pub fn inline_barrier_slow<C: ZBarrierContext + ?Sized>(
    slot: &AtomicU64,
    observed: u64,
    ctx: &C,
    kind: ZBarrierKind,
    base: u64,
) -> u64 {
    if observed & Z_COLORED_TAG == 0 {
        return observed;
    }
    base.wrapping_add(load_barrier_slow(slot, observed, ctx, kind))
}

// ---------------------------------------------------------------------------
// Composed entry points
// ---------------------------------------------------------------------------

/// The phase masks for one [`ZBarrierKind`], read **once** and carried across a
/// batch of loads.
///
/// *Added 2026-09-21, closing
/// `docs/feature-designs/zgc-architecture.md`.*
///
/// # What this buys
///
/// The module header says the fast path must be "a load, an `AND`, and a
/// not-taken branch — no call, no lock, no atomic RMW".
/// [`load_barrier_fast_bad`] is exactly that. [`z_barrier`] was not: it read
/// `bad_mask()` (whose provided body calls `good_mask()`), then
/// `address_mask()`, then `stats()` — three to four trait calls to re-derive
/// values that are **constant for the whole phase**, because they are flipped
/// only at a safepoint. Through a monomorphised `C` the optimiser folds all of
/// them away; through `&dyn ZBarrierContext` — a wiring the trait's own docs
/// call realistic and which `barrier_works_through_a_trait_object` pins — none
/// of them inline, so a thousand-slot array copy made three thousand indirect
/// calls to learn the same two constants a thousand times.
///
/// A `ZBarrierPhase` is the hoist. Build one, then run
/// [`z_barrier_phased`] per slot: the per-slot cost through `&dyn` becomes one
/// relaxed load, one `AND`, one not-taken branch and one predictable
/// `record_fast_hit` gate — which is the budget, modulo the honest residue
/// listed on [`z_barrier_phased`].
///
/// # Why it is per-KIND rather than carrying every mask
///
/// The alternative was a phase holding `bad_mask`, `weak_bad_mask` and
/// `address_mask` together, with the selection made per slot. That would make
/// the one-shot [`z_barrier`] *more* expensive than the code it replaced — it
/// would read a weak mask that a `Load` never uses — and a batch is never mixed
/// in practice: a root scan is all [`ZBarrierKind::Root`], an array copy is all
/// [`ZBarrierKind::Load`], reference processing is all [`ZBarrierKind::Weak`].
/// Binding the kind in makes the mask selection a construction-time question
/// and the per-slot path free of it.
///
/// # Validity
///
/// Valid for exactly as long as no safepoint can occur, because a safepoint is
/// where `good_mask` flips. That is precisely the region a barriered loop runs
/// in, and it is why a root scan — which *is* a safepoint — can hoist across
/// its whole slot array. Holding one across a safepoint would test this phase's
/// slots against the previous phase's colour, so do not store one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZBarrierPhase {
    /// The mask the fast path tests, already selected for [`Self::kind`] —
    /// [`ZBarrierContext::weak_bad_mask`] for [`ZBarrierKind::Weak`] and
    /// [`ZBarrierContext::bad_mask`] for everything else.
    pub bad_mask: u64,
    /// [`ZBarrierContext::address_mask`].
    pub address_mask: u64,
    /// The kind this phase selected its mask for. Carried so the slow path is
    /// handed the same kind the fast path classified against; a caller that
    /// passed a different one would be testing one barrier's mask and running
    /// another's slow path.
    pub kind: ZBarrierKind,
}

impl ZBarrierPhase {
    /// Read the masks for `kind` out of `ctx`. **One** call per mask, and the
    /// only place the weak/strong selection is made.
    #[inline(always)]
    pub fn of<C: ZBarrierContext + ?Sized>(ctx: &C, kind: ZBarrierKind) -> Self {
        let bad_mask = if matches!(kind, ZBarrierKind::Weak) {
            ctx.weak_bad_mask()
        } else {
            ctx.bad_mask()
        };
        ZBarrierPhase {
            bad_mask,
            address_mask: ctx.address_mask(),
            kind,
        }
    }
}

/// The fast path against a pre-read [`ZBarrierPhase`]: a load, an `AND`, a
/// not-taken branch.
///
/// `stats` is passed in rather than fetched, so a batch reads
/// [`ZBarrierContext::stats`] once as well. `ctx` is still taken because the
/// **slow** path needs it, and the slow path is `#[inline(never)]`.
///
/// # What still stands between this and a real ZGC load barrier
///
/// Stated plainly, because the module header sets a budget this cannot yet
/// meet and a silent shortfall is worse than a named one:
///
/// 1. **Nothing arms it.** `ZgcRealHeap::good_mask()` returns
///    [`Z_REMAPPED`] unconditionally, so every word in every slot classifies
///    `Good` and [`load_barrier_slow`] is unreachable in a real run. Until
///    `proposal-a-load-barrier-and-colored-pointers.md` stage 3 lands, every
///    number in this section is an argument, not a measurement.
/// 2. **It is still a Rust call, not an emitted instruction sequence.** A real
///    ZGC barrier is emitted by the JIT with the mask as an *immediate*, and
///    the branch target is a stub outside the loop body. Here the mask is a
///    register loaded from a `ZBarrierPhase` on the stack, and
///    `#[inline(always)]` is a request the compiler may decline at a `&dyn`
///    call boundary. Hoisting is the most the type system can do; the rest
///    needs a code generator.
/// 3. **`record_fast_hit` is one more load on the hit path.** Its gate is a
///    relaxed load of a never-written `AtomicBool` plus a not-taken branch —
///    deliberately a runtime flag rather than a `cfg`
///    ([`ZBarrierStats`], "Why `fast_path_hits` is off by default"), and that
///    reasoning is unaffected by this change. It is nevertheless a second load
///    the ideal sequence does not have. A JIT-emitted barrier would resolve it
///    at emit time.
/// 4. **The slow path takes `&C`, so a heap that reaches it through `&dyn`
///    pays indirect calls there.** That is the right place for them — the slow
///    path already does a CAS — but it means "no call" holds for the *fast*
///    path only, which is what the header actually claims.
#[inline(always)]
pub fn z_barrier_phased<C: ZBarrierContext + ?Sized>(
    slot: &AtomicU64,
    phase: ZBarrierPhase,
    stats: &ZBarrierStats,
    ctx: &C,
) -> u64 {
    // Bad-mask form: null passes with no extra branch, and an object at heap
    // offset 0 is not mistaken for null. See `classify_bad_masked`.
    let fast = if phase.kind.is_volatile() {
        load_barrier_fast_bad_acquire(slot, phase.bad_mask, phase.address_mask)
    } else {
        load_barrier_fast_bad(slot, phase.bad_mask, phase.address_mask)
    };
    match fast {
        ZFastPath::Good(addr) => {
            stats.record_fast_hit();
            addr
        }
        ZFastPath::Bad(raw) => load_barrier_slow(slot, raw, ctx, phase.kind),
    }
}

/// Apply `kind`'s load barrier to a **run** of slots, reading the phase masks
/// and the stats handle once for the whole run.
///
/// This is the form that pays: array copy, root scan, an interpreter loop over
/// a reference array, anything a JIT would emit with the mask in a register.
/// Semantically identical to a loop over [`z_barrier`].
///
/// Results are discarded, because every caller of the batched form is healing
/// slots in place rather than consuming addresses. A caller that needs the
/// addresses runs [`z_barrier_phased`] over its own loop with one
/// [`ZBarrierPhase::of`] in front of it.
#[inline]
pub fn z_barrier_slice<C: ZBarrierContext + ?Sized>(
    slots: &[AtomicU64],
    ctx: &C,
    kind: ZBarrierKind,
) {
    let phase = ZBarrierPhase::of(ctx, kind);
    let stats = ctx.stats();
    for slot in slots {
        let _ = z_barrier_phased(slot, phase, stats, ctx);
    }
}

/// Apply `kind`'s load barrier to `slot` and return the correct address.
///
/// This is the single implementation every named variant below delegates to;
/// the named wrappers exist so call sites read as intent rather than as an
/// enum argument, and so the wrong variant is a grep away from being spotted.
///
/// The one-shot form: it reads the phase masks itself, so it costs the same
/// three-to-four trait calls it always did. Where a caller has more than one
/// slot, [`z_barrier_slice`] or an explicit [`ZBarrierPhase::of`] plus
/// [`z_barrier_phased`] amortises them to once.
#[inline(always)]
pub fn z_barrier<C: ZBarrierContext + ?Sized>(
    slot: &AtomicU64,
    ctx: &C,
    kind: ZBarrierKind,
) -> u64 {
    z_barrier_phased(slot, ZBarrierPhase::of(ctx, kind), ctx.stats(), ctx)
}

/// Ordinary reference read — `getfield`, `aaload`, `getstatic`.
/// Keeps the referent alive during concurrent mark.
#[inline(always)]
pub fn z_load<C: ZBarrierContext + ?Sized>(slot: &AtomicU64, ctx: &C) -> u64 {
    z_barrier(slot, ctx, ZBarrierKind::Load)
}

/// Volatile / acquire reference read. Identical semantics to [`z_load`] plus
/// the acquire edge the Java program asked for.
#[inline(always)]
pub fn z_load_volatile<C: ZBarrierContext + ?Sized>(slot: &AtomicU64, ctx: &C) -> u64 {
    z_barrier(slot, ctx, ZBarrierKind::LoadVolatile)
}

/// Weak read of a `java.lang.ref` referent that must **not** be resurrected.
///
/// Use this ONLY where the caller is deciding the referent's fate (reference
/// processing). `Reference.get()` from Java must use [`z_keep_alive`] — see
/// [`ZBarrierKind`] for what each mistake costs.
#[inline(always)]
pub fn z_weak_load<C: ZBarrierContext + ?Sized>(slot: &AtomicU64, ctx: &C) -> u64 {
    z_barrier(slot, ctx, ZBarrierKind::Weak)
}

/// Resurrecting read of a weak referent — the barrier behind `Reference.get()`.
#[inline(always)]
pub fn z_keep_alive<C: ZBarrierContext + ?Sized>(slot: &AtomicU64, ctx: &C) -> u64 {
    z_barrier(slot, ctx, ZBarrierKind::KeepAlive)
}

/// Root-scan barrier for a stack slot / JNI handle / static field.
#[inline(always)]
pub fn z_on_root<C: ZBarrierContext + ?Sized>(slot: &AtomicU64, ctx: &C) -> u64 {
    z_barrier(slot, ctx, ZBarrierKind::Root)
}

/// Apply [`z_on_root`] to a contiguous run of root slots.
///
/// Convenience for the root scanner; semantically identical to a loop over
/// [`z_on_root`], but it **actually hoists the mask reads** out of it.
///
/// # This comment used to be a lie (corrected 2026-09-20)
///
/// It said "hoists the mask reads out of it" above a body that called
/// `z_on_root` per slot — and [`z_barrier`] reads `bad_mask()` and
/// `address_mask()` on every call, with the default `bad_mask()` calling
/// `good_mask()` in turn. So a thousand-slot root array made three thousand
/// trait calls purely to re-derive two constants that cannot change during a
/// root scan (the phase masks are flipped at a safepoint, and a root scan *is*
/// inside one). Through `&dyn ZBarrierContext` none of them inline.
///
/// The masks are now read once. Nothing else changes: each slot still gets the
/// identical fast-path test and, on a bad colour, the identical
/// [`load_barrier_slow`].
///
/// # It is now the hoist, rather than a copy of it (2026-09-21)
///
/// The body above was an open-coded loop that happened to hoist. It is now
/// [`z_barrier_slice`], which is the same hoist named as a general operation —
/// so an array copy or an interpreter loop gets it without re-deriving the
/// argument, and there is one place where the hoist can be got wrong.
pub fn z_on_roots<C: ZBarrierContext + ?Sized>(slots: &[AtomicU64], ctx: &C) {
    z_barrier_slice(slots, ctx, ZBarrierKind::Root);
}

/// View a raw heap slot as an [`AtomicU64`] so the barrier can operate on it.
///
/// The barrier's entire design rests on the slot being CASable, and the VM
/// side holds heap slots as raw pointers. This is the bridge.
///
/// # Safety
///
/// * `slot` must be non-null, valid for reads and writes for `'a`, and aligned
///   to 8 bytes (`AtomicU64`'s alignment, which on x86-64 and aarch64 is what
///   makes the CAS single-copy-atomic in the first place).
/// * For the duration of `'a` the pointee must be accessed **only** through
///   atomic operations. A concurrent non-atomic read or write of the same word
///   is a data race, and no amount of care inside the barrier can fix that
///   from this side.
#[inline(always)]
pub unsafe fn slot_as_atomic<'a>(slot: *mut u64) -> &'a AtomicU64 {
    debug_assert!(!slot.is_null(), "null slot pointer");
    debug_assert_eq!(slot as usize % 8, 0, "misaligned slot pointer");
    unsafe { AtomicU64::from_ptr(slot) }
}

// ---------------------------------------------------------------------------
// Per-thread mark buffers
// ---------------------------------------------------------------------------
//
// Modelled on `cratonvm_gc_core::satb`. Three properties of that module were each a
// fixed correctness bug there, and are reproduced here rather than
// rediscovered:
//
//   1. **Queue-id scoping.** Entries are tagged with the target queue's id, so
//      with more than one live queue in the process (parallel unit tests, any
//      future multi-heap embedding) one queue's drain cannot steal another's
//      buffered entries.
//   2. **A registry of live buffers.** The fast path only spills a thread's
//      bucket when it fills, so at the end of a mark cycle every thread holds
//      a partially-full bucket the collector cannot otherwise see. The
//      registry lets a safepoint drain them all.
//   3. **Orphan parking.** A thread that exits with a non-empty bucket must
//      not lose it. The entries describe *heap* reachability, not thread
//      state, and losing one hides a live object from the mark closure.
//
// What is deliberately NOT reproduced: SATB's tri-state activation gate. The
// ZGC load barrier's gate is the color mask (see the module header), so there
// is no check-then-log window to close.

/// Entries a thread buffers locally before spilling into a [`ZMarkQueue`].
///
/// 256 matches [`cratonvm_gc_core::satb`]'s figure: large enough that the shared-lock
/// acquisition is amortised to once per 256 slow paths, small enough that a
/// thread parked mid-cycle is not sitting on a meaningful fraction of the
/// mark set.
pub const Z_MARK_BUFFER_CAPACITY: usize = 256;

/// Number of independent [`ZMarkQueue`] shards. Power of two so the thread →
/// shard mapping is a mask.
const Z_MARK_SHARDS: usize = 16;

/// Monotonic source of process-unique [`ZMarkQueue`] ids. Starts at 1 so 0 is
/// a never-assigned sentinel.
static NEXT_Z_MARK_QUEUE_ID: AtomicU64 = AtomicU64::new(1);

/// Registry of every live per-thread mark-buffer partition set.
///
/// `Weak` rather than `Arc`: a thread that has exited drops the only strong
/// handle (its thread-local), so its slot stops upgrading and is pruned in
/// place — no dangling pointer, no leak.
static Z_MARK_BUFFER_REGISTRY: Mutex<Vec<Weak<Mutex<ZMarkPartitions>>>> = Mutex::new(Vec::new());

/// Buffers of exited threads that still hold undrained entries. Strong `Arc`s:
/// the owning thread is gone, so these are the only handles keeping the
/// entries alive until a collector folds them into their queue.
static Z_ORPHANED_MARK_BUFFERS: Mutex<Vec<Arc<Mutex<ZMarkPartitions>>>> = Mutex::new(Vec::new());

/// Per-thread mark storage: one bucket per distinct [`ZMarkQueue`] this thread
/// has logged against.
///
/// `buckets` holds a single-digit number of entries (exactly one in a
/// production VM), so a linear id scan beats any map.
#[derive(Default)]
struct ZMarkPartitions {
    buckets: Vec<(u64, Vec<u64>)>,
}

impl ZMarkPartitions {
    /// Append `addr` to `queue_id`'s bucket, creating it on first use. When
    /// the bucket reaches [`Z_MARK_BUFFER_CAPACITY`] it is removed and
    /// returned so the caller can spill it into the owning queue.
    fn log(&mut self, queue_id: u64, addr: u64) -> Option<Vec<u64>> {
        for i in 0..self.buckets.len() {
            if self.buckets[i].0 == queue_id {
                self.buckets[i].1.push(addr);
                if self.buckets[i].1.len() >= Z_MARK_BUFFER_CAPACITY {
                    return Some(self.buckets.swap_remove(i).1);
                }
                return None;
            }
        }
        let mut fresh = Vec::with_capacity(Z_MARK_BUFFER_CAPACITY);
        fresh.push(addr);
        self.buckets.push((queue_id, fresh));
        None
    }

    /// Take all buffered entries for `queue_id`, leaving other queues' buckets
    /// untouched.
    fn take(&mut self, queue_id: u64) -> Vec<u64> {
        for i in 0..self.buckets.len() {
            if self.buckets[i].0 == queue_id {
                return self.buckets.swap_remove(i).1;
            }
        }
        Vec::new()
    }
}

/// RAII wrapper stored in TLS so thread exit can decide the buffer's fate: an
/// empty buffer just dies, a non-empty one is parked in
/// [`Z_ORPHANED_MARK_BUFFERS`] so its entries survive to the next drain.
struct ZMarkBufferGuard {
    buffer: Arc<Mutex<ZMarkPartitions>>,
}

impl Drop for ZMarkBufferGuard {
    fn drop(&mut self) {
        if !self.buffer.lock().buckets.is_empty() {
            Z_ORPHANED_MARK_BUFFERS
                .lock()
                .push(Arc::clone(&self.buffer));
        }
    }
}

thread_local! {
    /// This thread's mark-buffer partition set.
    ///
    /// Behind `Arc<Mutex<_>>` rather than `RefCell` because the collector must
    /// be able to reach it through [`Z_MARK_BUFFER_REGISTRY`], and a `RefCell`
    /// is `!Sync`. On the common path this lock is uncontended — it is taken
    /// only by its owning thread, except at a safepoint when no mutator is
    /// running.
    static Z_MARK_BUFFER: ZMarkBufferGuard = {
        let buf = Arc::new(Mutex::new(ZMarkPartitions::default()));
        register_thread_mark_buffer(&buf);
        ZMarkBufferGuard { buffer: buf }
    };
}

/// Register a freshly-created per-thread buffer, opportunistically pruning
/// slots whose owning thread has since exited.
///
/// Pruning here (amortised against the one registration per thread) bounds the
/// registry under a churn of short-lived threads even if no collection runs.
fn register_thread_mark_buffer(buf: &Arc<Mutex<ZMarkPartitions>>) {
    let mut reg = Z_MARK_BUFFER_REGISTRY.lock();
    reg.retain(|w| w.strong_count() > 0);
    reg.push(Arc::downgrade(buf));
}

/// Sharded queue of addresses discovered by the load barrier, drained by the
/// concurrent marker.
///
/// One `Mutex<Vec<u64>>` per shard, picked by thread id, so independent
/// mutators spilling their buffers do not serialise on a single lock. The
/// marker is the single consumer, so cross-shard ordering is meaningless and
/// a drain simply concatenates.
pub struct ZMarkQueue {
    /// Process-unique identity (see the queue-id scoping note above).
    id: u64,
    shards: [Mutex<Vec<u64>>; Z_MARK_SHARDS],
}

impl ZMarkQueue {
    /// Create an empty queue with a fresh process-unique id.
    pub fn new() -> Self {
        let shards: [Mutex<Vec<u64>>; Z_MARK_SHARDS] =
            std::array::from_fn(|_| Mutex::new(Vec::new()));
        Self {
            // Relaxed: we need uniqueness, not ordering against other memory.
            id: NEXT_Z_MARK_QUEUE_ID.fetch_add(1, Ordering::Relaxed),
            shards,
        }
    }

    /// This queue's process-unique id.
    #[inline]
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Spill a batch of addresses into this queue.
    pub fn push_batch(&self, entries: Vec<u64>) {
        if entries.is_empty() {
            return;
        }
        let s = shard_for_current_thread();
        self.shards[s].lock().extend(entries);
    }

    /// Push a single address directly, bypassing the per-thread buffer.
    ///
    /// For callers that are already off the hot path (the TLS-teardown
    /// fallback in [`z_mark_thread_local_log`], collector-internal pushes).
    pub fn push(&self, addr: u64) {
        if addr == 0 {
            return;
        }
        let s = shard_for_current_thread();
        self.shards[s].lock().push(addr);
    }

    /// Drain every shard for the marker to process.
    ///
    /// # One copy, not two (2026-09-20)
    ///
    /// This used to `mem::take` each shard into a `[Vec<u64>; 16]` staging
    /// array, sum the lengths, allocate an exactly-sized result and copy
    /// everything **again** into it. So every drained address was memcpy'd
    /// twice, and sixteen heap allocations were made and immediately thrown
    /// away, to save one reallocation of the output vector.
    ///
    /// `append` moves a shard's contents straight into the result while holding
    /// that shard's lock — which is a memcpy, not a wait for anything, and no
    /// two shard locks are ever held at once. The result grows amortised.
    pub fn drain(&self) -> Vec<u64> {
        let mut out: Vec<u64> = Vec::new();
        for shard in self.shards.iter() {
            let mut guard = shard.lock();
            if guard.is_empty() {
                continue;
            }
            out.append(&mut *guard);
        }
        out
    }

    /// Number of entries currently queued. Diagnostics only — takes every
    /// shard lock briefly.
    pub fn len(&self) -> usize {
        self.shards.iter().map(|s| s.lock().len()).sum()
    }

    /// Whether the queue holds no entries.
    pub fn is_empty(&self) -> bool {
        self.shards.iter().all(|s| s.lock().is_empty())
    }
}

impl Default for ZMarkQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for ZMarkQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZMarkQueue")
            .field("id", &self.id)
            .field("queued", &self.len())
            .finish()
    }
}

/// Map the calling thread onto one of [`Z_MARK_SHARDS`] shards.
///
/// `ThreadId`'s inner value is not public on stable, but its `Hash` impl is
/// stable across calls within a thread, which is the only property needed.
///
/// # Computed once per thread (2026-09-20)
///
/// It used to run on every call, and every call meant
/// `std::thread::current()` — which clones an `Arc<Thread>`, i.e. a shared
/// atomic RMW — followed by a full SipHash-1-3 of the id. That is 20-odd
/// nanoseconds of work to re-derive a value that **cannot change for the
/// lifetime of the thread**, on the buffer-spill path of the load barrier.
///
/// The memo lives in a `Cell<usize>` thread-local with `usize::MAX` as the
/// "not computed yet" sentinel: no `Drop`, so it cannot be destroyed before the
/// buffer that uses it, and `try_with` failing during TLS teardown falls back to
/// computing the value the long way rather than to a wrong shard. (A wrong shard
/// would not be unsound — `drain` empties every shard — but a panic during
/// teardown would be.)
#[inline]
fn shard_for_current_thread() -> usize {
    thread_local! {
        static SHARD: std::cell::Cell<usize> = const { std::cell::Cell::new(usize::MAX) };
    }
    match SHARD.try_with(|cached| {
        let memo = cached.get();
        if memo != usize::MAX {
            return memo;
        }
        let fresh = compute_shard_for_current_thread();
        cached.set(fresh);
        fresh
    }) {
        Ok(shard) => shard,
        // TLS teardown. Correct but slow, which is the right trade for a path
        // that runs once per dying thread.
        Err(_) => compute_shard_for_current_thread(),
    }
}

/// The uncached derivation behind [`shard_for_current_thread`].
fn compute_shard_for_current_thread() -> usize {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    std::thread::current().id().hash(&mut h);
    (h.finish() as usize) & (Z_MARK_SHARDS - 1)
}

/// Buffer `addr` in the calling thread's bucket for `queue`, spilling into the
/// queue when the bucket fills.
///
/// This is what a [`ZBarrierContext::mark_live`] implementation should call.
/// On the common path it takes only this thread's own (uncontended) lock —
/// amortised one shared-lock acquisition per [`Z_MARK_BUFFER_CAPACITY`] slow
/// paths.
///
/// If the thread-local has already been destroyed (this thread is inside TLS
/// teardown), the address is pushed straight to the queue instead. Dropping it
/// would hide a live object from the mark closure; a direct push is slower but
/// correct, and TLS teardown is not a hot path.
#[inline]
pub fn z_mark_thread_local_log(queue: &ZMarkQueue, addr: u64) {
    if addr == 0 {
        return;
    }
    let queue_id = queue.id();
    match Z_MARK_BUFFER.try_with(|g| g.buffer.lock().log(queue_id, addr)) {
        Ok(Some(entries)) => queue.push_batch(entries),
        Ok(None) => {}
        Err(_) => queue.push(addr),
    }
}

/// Drain the calling thread's bucket for `queue` into that queue.
///
/// Called by a mutator at safepoint entry. Buckets this thread holds for other
/// queues are left untouched.
pub fn flush_thread_mark_buffer(queue: &ZMarkQueue) {
    let queue_id = queue.id();
    let entries = match Z_MARK_BUFFER.try_with(|g| g.buffer.lock().take(queue_id)) {
        Ok(entries) => entries,
        Err(_) => Vec::new(),
    };
    if !entries.is_empty() {
        queue.push_batch(entries);
    }
}

/// Drain **every** registered thread's bucket for `queue` into that queue, and
/// reap the buffers of threads that have exited.
///
/// # Correctness — call this at a safepoint
///
/// At a safepoint no mutator is inside the barrier, so no thread can re-fill a
/// bucket after it is drained. Outside a safepoint this flushes a racy
/// snapshot and does NOT establish "every discovered address is now in the
/// queue" — a running mutator can log into a bucket already visited. The
/// per-buffer `Mutex` is still taken (the buffer is `Arc`-shared, so the type
/// demands it, and it cheaply defends against a buggy caller), but the
/// *completeness* guarantee comes from the safepoint, not from the lock.
///
/// Lock ordering is one-directional throughout: registry → buffer, and never
/// across a queue shard lock (entries are collected first, pushed after every
/// other lock is released).
pub fn flush_all_thread_mark_buffers(queue: &ZMarkQueue) {
    let queue_id = queue.id();

    let live: Vec<Arc<Mutex<ZMarkPartitions>>> = {
        let mut reg = Z_MARK_BUFFER_REGISTRY.lock();
        let mut live = Vec::with_capacity(reg.len());
        reg.retain(|w| match w.upgrade() {
            Some(arc) => {
                live.push(arc);
                true
            }
            None => false,
        });
        live
    };

    for buf in live {
        let entries = buf.lock().take(queue_id);
        if !entries.is_empty() {
            queue.push_batch(entries);
        }
    }

    // Exited threads' parked buffers. Each is reaped once nothing is left in
    // ANY of its buckets — a dead thread can never log again, so an emptied
    // orphan stays empty; a bucket belonging to a DIFFERENT queue is left for
    // that queue's own drain.
    let mut orphan_entries: Vec<Vec<u64>> = Vec::new();
    {
        let mut orphans = Z_ORPHANED_MARK_BUFFERS.lock();
        orphans.retain(|buf| {
            let mut b = buf.lock();
            let entries = b.take(queue_id);
            if !entries.is_empty() {
                orphan_entries.push(entries);
            }
            !b.buckets.is_empty()
        });
    }
    for entries in orphan_entries {
        queue.push_batch(entries);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
