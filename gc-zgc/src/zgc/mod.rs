// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! ZGC — the Z-Garbage-Collector-shaped backend, and what it actually is.
//!
//! # What runs
//!
//! [`ZgcRealHeap`] is the collector. It backs every allocation with real
//! memory ([`cratonvm_gc_core::arena::Arena`] over a reserved, granule-committed region --
//! see [`cratonvm_gc_core::reservation`]), writes real [`ObjectHeader`]s, and implements
//! [`cratonvm_gc_core::collector::GarbageCollector`] with the same bounds-checked
//! field/array semantics as [`crate::gen_heap::GenerationalHeap`] and
//! [`crate::g1::G1Collector`].
//!
//! Its cycle is a **stop-the-world mark-sweep with an optional sliding
//! compaction**:
//!
//! 1. **Mark.** Clear the mark bits -- one linear pass over a side bitmap, not
//!    a walk of every header ([`ZgcRealHeap::mark_bits`]) -- then trace the
//!    strong closure from the roots. Concurrently when
//!    `CRATONVM_ZGC_CONC_START` asks for it, otherwise inside the pause.
//! 2. **Sweep.** Walk the object-start registry; unmarked objects are zeroed
//!    and returned to the arena free list. Shardable
//!    ([`ZgcRealHeap::sweep_parallel`]), serial by default.
//! 3. **Relocate**, when the cycle's coverage proof or its page pins allow it:
//!    slide the low region down, pack the large-object region up, rewrite every
//!    reference slot, and leave a forwarding word at every address the slide
//!    emptied and did not refill.
//!
//! # What it is NOT, stated plainly
//!
//! Not OpenJDK ZGC. There are no colored pointers in a reference slot on this
//! backend: `feature-designs/zgc-reference-slot-representation.md` opens with
//! the measured position -- *"Reference slots are plain pointers; nothing in
//! the heap stores a colored word"* -- and the code agrees, because
//! [`ZgcRealHeap::set_barrier_color`] has no non-test caller but the opt-in
//! plain-store arming (`CRATONVM_ZGC_LOAD_BARRIER`, zt t18/zb1), which routes
//! reads through the barrier while every store stays a plain pointer. By
//! default there is therefore no load barrier on the mutator read path, and
//! relocation is stop-the-world rather than concurrent.
//!
//! A metadata-only SIMULATION of ZGC's model (`ColoredPointer`,
//! `LoadBarrier`, `ZPage`, `ZgcHeap`, `ZgcCollector`, `GenerationalZgc`) used
//! to live in this file as scaffolding for that future work. It was deleted on
//! 2026-09-02: every concept in it now has an honest implementation in the
//! submodules below, and 1,250 lines of a model nothing referenced is a cost
//! rather than a plan. See the note where it stood.
//!
//! # The submodules
//!
//! Each was written to be adoptable independently and declares its own
//! decoupling traits, which is why several are used only in part. **Recount
//! before quoting an adoption figure** -- an earlier version of this header
//! carried a hand-maintained census that was quoted by `gc/Cargo.toml` and by a
//! known-issues page, and was wrong within days of being written. The count is
//! now produced by `scripts/zgc-submodule-adoption.sh` and asserted by
//! `zgc_submodule_adoption_matches_the_recorded_census`, so it cannot go stale
//! silently.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, AtomicUsize, Ordering};

use parking_lot::Mutex;
use rustc_hash::{FxHashMap, FxHashSet};

use cratonvm_gc_core::arena::Arena;
use cratonvm_gc_core::collector::{GarbageCollector, MonitorCleanup, StopTheWorldToken};
use cratonvm_gc_core::gc::{GcResult, GcStats};
use cratonvm_gc_core::heap::{
    array_data_size, read_prim_element, write_prim_element, ArrayElementType, ObjectHeader,
    ObjectKind, ARRAY_DATA_OFFSET, GC_FLAG_MARKED, HEADER_SIZE, SLOT_SIZE,
};
use cratonvm_gc_core::reference::{ReferenceProcessingResult, ReferenceProcessor, ReferenceType};
use cratonvm_types::MARK_WORD_OFFSET;
use cratonvm_types::{ClassId, ObjectRef, Value};

// ---------------------------------------------------------------------------
// The submodules
// ---------------------------------------------------------------------------
//
// The real, memory-backed machinery, built out under
// `docs/feature-designs/zgc-production-implementation-plan.md`. Deliberately
// decoupled from one another: each depends on a trait it declares itself rather
// than on a sibling's concrete types, so they can land and be reviewed
// independently.
//
// THERE IS NO HAND-MAINTAINED ADOPTION CENSUS HERE ANY MORE, and the reason is
// worth keeping. One used to sit at this spot. It said "NONE of them is wired
// into `ZgcRealHeap` yet" from 2026-08-07 until 2026-08-10, by the end of which
// HALF of them were; `gc/Cargo.toml` quoted it, and
// `docs/feature-designs/zgc-architecture.md`
// quoted that in turn to argue ZGC's suite result might be an artefact of an
// unwired allocator -- exactly the inference a stale count corrupts. It was
// rewritten, and was wrong again within weeks.
//
// A number a human has to update by hand, in a file this size, is a number that
// will be wrong. `scripts/zgc-submodule-adoption.sh` produces it from the
// source, `types/tests/zgc-submodule-adoption.txt` records the answer, and
// `zgc_submodule_adoption_matches_the_recorded_census` fails the build when the
// two disagree -- so a change that adopts (or abandons) a module updates the
// census as part of landing, or does not land.

/// The object-start bitmap: which addresses are allocation bases, and which
/// objects this cycle has marked. Split out of this file on 2026-09-02.
mod starts;

/// zr z8/st: the census half of a start registry with no per-allocation bit
/// inside a live VM-TLAB chunk (`CRATONVM_ZGC_LAZY_TLAB_STARTS`, default off;
/// a diagnostic since zt t12/rt retired its `materialize` arm).
mod starts_lazy;
pub use starts_lazy::ZLazyStartsReport;

/// The sweep: the shard type both drivers fill, and the per-object body they
/// share. Split out of this file on 2026-09-02.
/// The per-object extra-root filter: the four global tables a marked
/// object.s out-edges live in, reduced to two lock-free indexed loads.
/// Added 2026-09-02.
mod mark_roots;

mod sweep;
pub use sweep::{ZSweepCfg, ZSweepShard};
/// zt t21/zc2: the sweep out of the pause (`CRATONVM_ZGC_CONCURRENT_SWEEP`,
/// opt-in): the pause parks the cycle's sweep, the collecting thread runs it
/// after the release, and its after-sweep hook holds every consumer of the
/// sweep's totals.
mod concurrent_sweep;
pub use concurrent_sweep::{ZConcSweepReport, ZSweepRunner};
/// zt t22/zc3: the referent slot keeps its referent through the collection
/// (`CRATONVM_ZGC_CONCURRENT_REFS`, opt-in): the markers hide it and the pause
/// clears it when the referent dies; plus the census of the VM's reference
/// round (`[GC] zgc-vm-refs:`). Stage 1c of the concurrent-collector program.
mod concurrent_refs;
pub use concurrent_refs::{
    ZConcRefsReport, ZGC_REFERENT_GATE_ARMED, ZGC_REFERENT_GATE_DISARMED, ZGC_REFERENT_GATE_NEVER,
};
/// zt t23/zc6: stage 1f -- the census of what a concurrent cycle's closing
/// pause marks against its phase (`[GC] zgc-markend-work:` /
/// `zgc-markend-census:`), and the director tick closing a cycle at its fixed
/// point (`CRATONVM_ZGC_MARK_END_SERVICE`, opt-in); zt t24/zc8: Concurrent
/// Mark Continue, a Pause Mark End attempt that releases the world when mark
/// work is left (`CRATONVM_ZGC_MARK_CONTINUE`, opt-in).
mod mark_end;
pub use mark_end::{
    ZMarkContinueReport, ZMarkEndAttempt, ZMarkEndReport, ZMarkEndTicket,
    ZGC_MARK_CONTINUE_MAX_ATTEMPTS,
};
/// zt t26/zc9: stage 1a -- an every-trigger heap asks for its concurrent cycle
/// at HotSpot's allocation-rate point, before a late clause leaves it no room
/// to finish a mark (`CRATONVM_ZGC_CONCURRENT_RATE_START`, opt-in; census
/// `[GC] zgc-rate-start:`).
mod rate_start;
pub use rate_start::{ZRateStartReport, ZRateStartVerdict, ZGC_RATE_START_SPIKE_TOLERANCE};
/// zt t27/zc10: the referent-skip class filter in front of the skip-set Bloom
/// filter, so a heap with many live `Reference`s does not send every marked
/// object to the skip set's lock (`CRATONVM_ZGC_MARK_SKIP_CLASS_FILTER`,
/// default ON; census `[GC] zgc-skip-class-filter:`).
pub use cratonvm_gc_core::skip_class_filter;
// zt t31/zc11 (program page 1c, step 2a's CPU): the skip set as a member
// bitmap (`CRATONVM_ZGC_MARK_SKIP_BITS`).
mod skip_bits;
// zt t31/zc11 (program page 1c step 2c): the VM reference round after the
// release (`CRATONVM_ZGC_CONCURRENT_REF_ROUND`).
mod concurrent_ref_round;
mod remap_in_mark;
// zt t32 (R5 / R6): concurrent relocation (`CRATONVM_ZGC_CONCURRENT_RELOCATE`).
mod concurrent_relocate;
pub use concurrent_ref_round::{ZRefRoundRefusal, ZRefRoundReport};
pub use concurrent_relocate::{ZConcRelocRefusal, ZConcRelocReport};
pub use skip_class_filter::ZSkipClassFilterReport;
pub use skip_bits::{ZSkipBitsReport, ZSkipSnapshot};
pub use skip_bits::ZSkipBits;
/// zs s2/rf: tests for the per-object sweep's oversize arm and the screen memo.
#[cfg(test)]
mod zs2_rf_tests;

/// The arena-backed thread-local allocation buffer -- not `zgc::tlab`, which
/// buffers out of the page allocator. Split out of this file on 2026-09-02.
mod arena_tlab;
pub use arena_tlab::ZArenaTlabRetireSummary;
pub use arena_tlab::{
    zgc_raw_zero_outside_lock_enabled, zgc_tlab_enabled_by_default, zgc_tlab_footprint,
    ZArenaTlabRegistry,
};

/// The VM thread's own `Tlab` on this backend: the `Zgc` arm of
/// `VmHeap::refill_tlab`, per-object registration, tail reclamation, and the
/// skip-region contract with the compactor. Added 2026-09-02.
mod vm_tlab;
pub use vm_tlab::zgc_vm_tlab_enabled_by_default;

/// jr21 gc: the worker that pre-faults the committed bytes just ahead of the
/// low bump cursor, so a fresh heap's first allocating row does not pay a
/// demand-zero fault per page. Added 2026-10-27.
mod prefault;

/// Per-heap state that used to be process statics (zr z3/lt), and the
/// classification of the ones that stay.
mod heap_state;
// `cratonvm_gc_core::tlab::Tlab::new`'s question: is `[lo, hi)` this thread's valid
// owned VM-TLAB chunk, and under which epoch? (round 9 wave 8, `zgc8`).
pub use vm_tlab::owned_vm_tlab_chunk_epoch;

pub use starts::{
    zgc_mark_bits_enabled, zgc_start_bits_enabled_by_default, ZObjectStartBits, ZObjectStarts,
    ZObjectStartsSnapshot,
};

/// Real colored-pointer encoding and virtual address space.
pub mod vaddr;

/// Real memory-backed ZPages (small/medium/large tiers) and the O(1) page table.
pub mod page;

/// Real atomic, self-healing colored-pointer load barrier.
pub mod barrier;

/// Lock-free per-page forwarding table and relocation-set selection.
pub mod forwarding;

/// zt t22/zc4: concurrent-relocation groundwork over the arena (selection from
/// mark-end liveness, per-region forwarding with in-flight accounting, the
/// cycle's lock-free region map). zt t23/zc5: its callers are R1's census
/// (`CRATONVM_DBG_ZGC_RELOC_SELECT_CENSUS`) and R2's stop-the-world arm
/// (`CRATONVM_ZGC_REGION_RELOCATE`), both off by default.
pub mod region_forwarding;

/// ZGC cycle phase/pause instrumentation and allocation-stall accounting.
pub mod metrics;

/// Generational remembered set (old -> young) and the store barrier.
pub mod remembered;

/// Concurrent marking engine: striped work-stealing stacks, the termination
/// handshake, and the mutator ingress the load barrier publishes into.
pub mod mark;

// zt t6/ob4: the opt-in Windows cycle clock behind `mark::thread_cpu_nanos`.
mod thread_clock;

/// Generational young/old split (JEP 439) over the real page allocator.
pub mod generation;

/// Object relocation: the copy/CAS-publish protocol, the from-space page
/// lifetime state machine, and the stop-the-world compaction path.
pub mod relocate;

/// Thread-local allocation buffers for the ZGC page allocator.
pub mod tlab;

/// HotSpot-ZGC-shaped collection rules (allocation rate, proactive, high
/// usage), evaluated as a byte threshold `needs_gc` compares against
/// (zr z1/tr; `CRATONVM_ZGC_RATE_TRIGGER`, default off).
pub mod director;

/// zt t7/tg: the `Timer` rule (`-XX:ZCollectionInterval`) and the director
/// tick that serves it on an idle heap.
pub mod timer;

/// Reference-slot census: measures the compact/legacy/array/static split that
/// decides how much of the load barrier's slot-resolution work is critical path.
pub mod census;

/// The seam layer between the submodules above. These modules were authored
/// independently and each declares its own decoupling traits, so the glue that
/// joins them lives here rather than in any one of them. An adapter in this
/// module is a *symptom*: the long-term fix for several of them is for the two
/// modules to agree on a shared type.
pub mod adapters;

/// zr z4/jf: the facts one collection leaves for the VM's JFR writers.
/// Private (not in the submodule census's `pub mod` block); the types the VM
/// reads are re-exported below.
mod jfr_facts;
pub use jfr_facts::{hotspot_stall_page, ZJfrCycle, ZJfrRelocationGroup, ZJfrRelocationSet};

/// t1 wave 6 lane Y4 (zt3gn S2.4 slice 1, `CRATONVM_ZGC_GEN_YOUNG_EVACUATE`,
/// opt-in): the nursery-scoped young slide's scope and counters. Private, not
/// in the submodule census's module list.
mod young_evacuate;
#[cfg(test)]
mod zt20x16_load_barrier_single_heap_tests;
// ---------------------------------------------------------------------------
// Thread-local allocation buffers for `ZgcRealHeap`
// ---------------------------------------------------------------------------
//
// This section adopts `gc::zgc::tlab`'s contract onto `ZgcRealHeap`. Read the
// header on [`ZArenaTlabRegistry`] first: it states which parts of that module
// are reused verbatim, which one part could not be, and why.

use std::sync::Arc;

use cratonvm_gc_core::tlab::Tlab;
use crate::zgc::tlab::{current_thread_key, ZTlabConfig, ZTlabHeapHooks, ZTlabStats};
use crate::zgc::vaddr::ZColor;

/// Ceiling on a TLAB chunk carved from the arena.
///
/// Two forces pull on this number in opposite directions.
///
/// DOWN: every byte sitting in a live chunk but not yet handed to an object is
/// a byte `ZgcRealHeap::allocated` does not know about (see
/// [`ZgcRealHeap::alloc_tlab`] on accounting). That blind spot is bounded by
/// `live_threads * chunk`, so the chunk is the knob that bounds it. This was
/// 64 KiB for that reason alone — deliberately below
/// `cratonvm_gc_core::tlab::default_tlab_size()` (256 KiB), which is sized for the
/// generational young arena and its disjoint accounting.
///
/// UP, and this is the force that was missing: **on a non-compacting heap the
/// chunk size sets the GRANULARITY OF FREE-LIST HOLES.** A chunk is one
/// `arena.alloc`; the objects inside it are freed individually by the sweep and
/// merged back by the coalescer, but a single survivor anywhere in a chunk
/// walls it off from its neighbours. The steady state is therefore a free list
/// whose largest block is about one chunk — and an allocation LARGER than a
/// chunk can then never be served again, no matter how much of the heap is
/// free.
///
/// 64 KiB was the worst possible value for that, because a 64 KiB hole is
/// exactly too small for one of the most common large Java allocations there
/// is: `new T[8192]` is `8192 * 8 + 16 = 65552` bytes. Measured on the
/// 2026-08-11 Azure Linux Hibernate run, `sql.exec.SmokeTests` died with
/// `OutOfMemoryError` on precisely that array — ANTLR's
/// `ParserATNSimulator.computeTargetState` allocating `DFAState[8192]` — with
/// the guard line reading `free_list_bytes=1211993376 largest_free_block=65528`:
/// 1.13 GiB free, short by 24 bytes. The same class passes on the same heap
/// with `CRATONVM_ZGC_TLAB=0` (no chunks, no walls) and on G1 (which
/// evacuates).
///
/// 512 KiB keeps the hole granularity an order of magnitude above the array
/// shapes real workloads repeat, and raises `max_tlab_alloc` (`chunk / 8`) from
/// 8 KiB to 64 KiB so mid-sized arrays stop needing an arena hole at all.
///
/// **Do not reach for this knob again.** Raising it was the answer to the
/// 64 KiB failure and it did not survive the next one: on 2026-08-13 a 2 MB
/// `char[]` hit the same wall at 512 KiB. Two changes have since removed the
/// coupling this constant was being used to paper over — allocations no TLAB
/// can serve are placed at the arena's other end (`ZGC_LARGE_OBJECT_MIN`,
/// `Arena::high_cursor`), and a refill now accepts a shorter RECYCLED chunk so
/// retired chunks stop being one survivor short of reusable
/// (`ZgcRealHeap::tlab_refill`). What is left for this number to decide is the
/// accounting blind spot named above, and that is what it should be derived
/// from. The
/// accounting blind spot it costs is `live_threads * 512 KiB` — a few MiB on
/// the thread counts this VM runs, against a heap sized in gigabytes — and the
/// unused tail is returned to the free list at every retire
/// ([`ZgcRealHeap::tlab_retire_locked`]), so it is a reservation, not waste.
///
/// Small heaps are unaffected: [`ZArenaTlabRegistry::chunk_bytes_for_capacity`]
/// takes `capacity / 1024` first, so this ceiling only binds above ~512 MiB,
/// which is exactly where the fragmentation it exists to prevent appears.
/// Why a ZGC collection declined to run its low-end slide.
///
/// The gate in [`ZgcRealHeap::relocate_stw`] is a five-term conjunction, and
/// until 2026-08-30 its only trace was one scalar. Each term has a different
/// repair — a kill switch that is off wants turning on, an incomplete coverage
/// proof wants a stronger JIT contract, a conservatively-scanned thread wants
/// nothing at all — so the number that matters is which term said no.
pub mod relocation_skip_reason {
    /// `CRATONVM_ZGC_RELOCATE_UNDER_PROVEN_JIT=0` — the kill switch.
    pub const SWITCH_OFF: usize = 0;
    /// Moving-young is off process-wide, so no rewritable-root contract exists
    /// to lean on.
    ///
    /// # `CRATONVM_NO_MOVING_YOUNG` DOES bind ZGC, and that is deliberate
    ///
    /// The token reads as the **generational** collector's policy — this term
    /// is `gc_quiescence::moving_young_enabled()`, which is what
    /// `CRATONVM_NO_MOVING_YOUNG=1` clears — and for a long time nothing said
    /// which collectors it reached. It reaches this one, on purpose.
    ///
    /// It is the process-wide *"do not move objects out from under compiled
    /// frames"* switch, and the entire rewritable-root contract the refusal
    /// chain around this constant leans on is maintained only while it is on:
    /// `moving_young_coverage_incomplete`, the `MOVABLE_BOUNDS` publication and
    /// the conservative-scan suppression all stop being computed when it is
    /// off. A ZGC that kept sliding with that contract off would be relocating
    /// on a proof nothing is producing.
    ///
    /// The consequence an operator should know, because it is surprising: with
    /// `CRATONVM_NO_MOVING_YOUNG=1`, ZGC still compacts on a cycle with **no**
    /// compiled frame live (this term is one of the four the
    /// `compiled_frames_live` gate discards) and refuses on every cycle with
    /// one. `[GC] zgc-relocation-refusal-term:` counts the verdicts,
    /// `[GC] zgc-relocation-skip-reason:` counts the ones that stopped a slide,
    /// and the gap between them is exactly that population. See
    /// `docs/flag-tokens.md`'s collector-reach rows.
    pub const MOVING_YOUNG_DISABLED: usize = 1;
    /// The per-cycle coverage proof reported at least one unproven obligation.
    /// `relocation_coverage_reason:` names which.
    pub const COVERAGE_INCOMPLETE: usize = 2;
    /// This thread's root scan asked for non-moving JIT roots.
    pub const FORCED_NON_MOVING_ROOTS: usize = 3;
    /// A JIT frame was on this thread's stack without a `JitEntryGuard`.
    pub const UNREGISTERED_JIT_FRAME: usize = 4;
    /// This collection's safepoint TLAB retire could not lock a cell, so a
    /// thread's chunk is still reserved and invisible to the collector. See
    /// `ZArenaTlabRegistry`-side `retire_all_tlabs_at_safepoint`.
    pub const TLAB_RETIRE_INCOMPLETE: usize = 5;
    // 6 was `JIT_ACTIVE_BLANKET` (`CRATONVM_ZGC_JIT_BLANKET_REFUSAL=1`, a
    // priced trade: ~9700 fragmentation OOMs, no run completing). Retired in
    // zt t12/rt; the number stays unassigned so the codes below keep theirs.
    // 7 was `UNREWRITABLE_PEER_STATE` (`CRATONVM_ZGC_UNREWRITABLE_PEER_REFUSES=1`:
    // a frozen peer's `unrewritable_peer_state()` as a refusal term of its own,
    // inert without `CRATONVM_XT_KEEP_UNREWRITABLE_ON_DISCHARGE`, never
    // measured). Retired in zt t12/rt, unassigned likewise.
    /// zt t1/rl (`zgc-zr1rl-raw-jni-locals-on-a-native-peer`): a thread of this
    /// VM is inside a JNI native (or attached) with RAW local references --
    /// C copies of addresses the slide cannot rewrite. An unconditional
    /// refusal like [`TLAB_RETIRE_INCOMPLETE`], independent of compiled
    /// frames. Only under `CRATONVM_ZGC_RELOCATE_REFUSES_RAW_JNI_LOCALS=1`
    /// (opt-in); locals are indirect handles by default
    /// (`CRATONVM_JNI_INDIRECT_LOCALS`), so the default arm has no such holder
    /// outside a raw escape.
    pub const RAW_JNI_LOCALS: usize = 8;
    /// One past the highest code; sizes the counter array.
    pub const COUNT: usize = 9;

    /// Human-readable label, for the summary line.
    pub fn label(code: usize) -> &'static str {
        match code {
            SWITCH_OFF => "relocate-under-proven-jit-switch-off",
            MOVING_YOUNG_DISABLED => "moving-young-disabled",
            COVERAGE_INCOMPLETE => "coverage-proof-incomplete",
            FORCED_NON_MOVING_ROOTS => "forced-non-moving-jit-roots",
            UNREGISTERED_JIT_FRAME => "unregistered-jit-frame-on-stack",
            TLAB_RETIRE_INCOMPLETE => "tlab-retire-incomplete-at-safepoint",
            RAW_JNI_LOCALS => "raw-jni-locals-open",
            _ => "unknown",
        }
    }
}


/// Fewest survivors a compaction worker is given. A header read is a few
/// nanoseconds, so a chunk this size is tens of microseconds of work, which is
/// the order of one thread spawn and join. Small in tests so a fixture of a
/// few thousand objects exercises the parallel arm.
#[cfg(not(test))]
const ZGC_PAR_RELOCATE_MIN_PER_WORKER: usize = 32 * 1024;
#[cfg(test)]
const ZGC_PAR_RELOCATE_MIN_PER_WORKER: usize = 64;
#[cfg(test)]
mod zki_explicit_commit_tests;
#[cfg(test)]
thread_local! {
    /// Per-test override of `CRATONVM_ZGC_FUSED_SLIDE_SCREEN`, so one test
    /// process can drive both arms over the same fixture.
    static FUSED_SLIDE_SCREEN_OVERRIDE: std::cell::Cell<Option<bool>> =
        const { std::cell::Cell::new(None) };
}

#[cfg(test)]
thread_local! {
    /// Per-test override of `CRATONVM_ZGC_RELOCATE_SIZE_TABLE`.
    static RELOCATE_SIZE_TABLE_OVERRIDE: std::cell::Cell<Option<bool>> =
        const { std::cell::Cell::new(None) };
}

#[cfg(test)]
thread_local! {
    /// Per-test override of `CRATONVM_ZGC_SWEEP_SIZE_HINTS`.
    static SWEEP_SIZE_HINTS_OVERRIDE: std::cell::Cell<Option<bool>> =
        const { std::cell::Cell::new(None) };
}

#[cfg(test)]
mod zt21ca7_pressure_select_reader_tests;
/// `a` and `b` as ONE ascending sequence: a linear merge when each is already
/// ascending, else concatenate and `sort_unstable` (the fallback is the exact
/// code this replaced). Either way the output is `a ++ b` sorted, so a caller
/// cannot tell which ran.
///
/// Test-only since round 9 wave 6 (`zgc6`): `relocate_stw` no longer
/// materialises its post-slide extents (see [`block_overlapping_spans`]), and
/// this is kept as the REFERENCE the streaming screen is checked against in
/// `vacated_screen_tests`.
#[cfg(test)]
fn merge_ascending_spans(a: Vec<(usize, usize)>, b: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    let ascending = |v: &[(usize, usize)]| v.windows(2).all(|w| w[0] <= w[1]);
    if b.is_empty() && ascending(&a) {
        return a;
    }
    if !(ascending(&a) && ascending(&b)) {
        let mut all = a;
        all.extend(b);
        all.sort_unstable();
        return all;
    }
    let mut out: Vec<(usize, usize)> = Vec::with_capacity(a.len() + b.len());
    let (mut i, mut j) = (0usize, 0usize);
    while i < a.len() && j < b.len() {
        if a[i] <= b[j] {
            out.push(a[i]);
            i += 1;
        } else {
            out.push(b[j]);
            j += 1;
        }
    }
    out.extend_from_slice(&a[i..]);
    out.extend_from_slice(&b[j..]);
    out
}

#[cfg(test)]
mod merge_spans_tests;

/// Round 9 wave 6 (`zgc6`): the streaming vacated-span screen and the cost
/// gate's decision.
#[cfg(test)]
mod vacated_screen_tests;
/// zt t1/mk test seam (`ZgcCounters::test_driver_safepoint_panic`): the
/// stop-the-world mark driver panics when it opens its first mark-end
/// safepoint, i.e. with the pool's workers armed and inside the cycle -- the
/// state the driver-panic arm of `ZgcRealHeap::mark_with_controller_stw` must
/// clean up.
#[cfg(test)]
struct ZtPanickingDriverSafepoint;

#[cfg(test)]
impl crate::zgc_concurrent::ZgcMarkSafepoint for ZtPanickingDriverSafepoint {
    fn begin_mark_end_safepoint(&self) {
        panic!("zt1mk test: injected mark-driver panic");
    }

    fn flush_mutator_buffers(&self, _handle: &mark::ZMarkHandle) -> usize {
        0
    }

    fn end_mark_end_safepoint(&self) {}
}

#[cfg(test)]
thread_local! {
    /// zr z4/rv4: test fault -- `mark_with_controller_stw` claims the roots
    /// and fails, the state a panicking or uncertified driver leaves.
    static ZGC_TEST_PARMARK_FAIL_AFTER_ROOTS: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------





#[cfg(test)]
mod zr1_tr_trigger_tests;

/// zr z1/al (2026-10-12): allocation and sweep -- the raw-path zeroing outside
/// the arena lock, the large-object sweep's oversize arrays, the snapshot's
/// range screen, and the engagement counters of the nio21 registry switches.
#[cfg(test)]
mod zr1_al_tests;

#[cfg(test)]
mod zr2rv_tests;

/// zr z4/ab: the huge-extent table residuals of
/// `docs/feature-designs/zgc-architecture.md`
/// (sticky overflow mode, no length screen). The headers are forged on small
/// arrays: the table reads only the header, and no collection runs while a
/// header is forged (every test restores it before the heap drops).
#[cfg(test)]
mod zr4ab_huge_extent_tests;

#[cfg(test)]
mod zr2_r2_tests;



/// zr z2/cp: `CRATONVM_ZGC_RELOCATE_PRESSURE_SELECT` -- the widened relocation
/// selector after a hard allocation failure.
#[cfg(test)]
mod zr2_cp_tests;

/// zr z2/t2 (2026-10-12): the allocation-failure door's cause and the JMX
/// young / old split.
#[cfg(test)]
mod zr2_t2_tests;

/// zr z3/fp: footprint -- the table/scratch trims, the sparse bitmap clear,
/// the end-of-sweep uncommit clock and the `zgc-footprint` figures.
#[cfg(test)]
mod zr3_fp_tests;

/// zr z4/un: the uncommitter (give-back without a collection), the reset
/// accounting behind `Runtime.totalMemory()`, and the slide's bookkeeping
/// buffer lifetimes.
#[cfg(test)]
mod zr4_un_tests;



#[cfg(test)]
mod zr6_tp_pacing_tests;

#[cfg(test)]
mod zs6_no_exhausted_tests;



#[cfg(test)]
mod zr7_rs_footprint_tests;



/// zr z11/rv9: the residual-page fixes that have no better home.
#[cfg(test)]
mod zr11rv9_tests;

#[cfg(test)]
mod zr10_mt_side_table_tests;

/// zs s1/dg: `CRATONVM_ZGC_REWRITE_OVERSIZE_ARRAYS` -- a LIVE array past
/// `MAX_PLAUSIBLE_ARRAY_LEN` is walked by the slide's rewrite pass instead of
/// being skipped as "not walkable" (the `NativeGrowthReclaimProbe` guard
/// lines: `kind=Array class_id=0 array_length=273366446 sizable=false`, a real
/// `byte[]` past the element cap, not a torn header). Page:
/// `docs/feature-designs/zgc-architecture.md`.
#[cfg(test)]
mod zs1_dg_oversize_rewrite_tests;

#[cfg(test)]
mod zs2_mc2_mark_tests;

#[cfg(test)]
mod zs3_ac_cpu_tests;

/// zs s3/cr: what one whole-live-set pass pays for its scoped threads.
#[cfg(test)]
mod zs3_cr_bench;

#[cfg(test)]
mod zs5_mc3_mark_tests;



/// zt t6/rv3 (`zgc-residuals-handed-over-by-the-gci-round-20261014` item 2):
/// the ZGC-owned debug / opt-in switches that parsed PRESENCE read the value.
/// Before the fix `CRATONVM_DBG_ZGC_CORPSE=0` armed the corpse ledger and
/// `CRATONVM_ZGC_NO_JIT_READ_BOUNDS=0` withdrew the JIT read bounds.
#[cfg(test)]
mod zt6_rv3_flag_value_tests;

#[cfg(test)]
mod zt35zrr_tests;



#[cfg(test)]
pub mod tests;

mod hotspot_cause_for_mask;
#[allow(unused_imports)]
pub use hotspot_cause_for_mask::*;
#[allow(unused_imports)]
use hotspot_cause_for_mask::*;
mod compact_batch_reads_enabled;
#[allow(unused_imports)]
pub use compact_batch_reads_enabled::*;
#[allow(unused_imports)]
use compact_batch_reads_enabled::*;
mod trigger_tallies;
#[allow(unused_imports)]
pub use trigger_tallies::*;
#[allow(unused_imports)]
use trigger_tallies::*;
mod zgc_page_pinned_relocate;
#[allow(unused_imports)]
pub use zgc_page_pinned_relocate::*;
#[allow(unused_imports)]
use zgc_page_pinned_relocate::*;
mod relocate_stw_for_test;
#[allow(unused_imports)]
pub use relocate_stw_for_test::*;
#[allow(unused_imports)]
use relocate_stw_for_test::*;
mod verify_no_dangling_slots_after_slide;
#[allow(unused_imports)]
pub use verify_no_dangling_slots_after_slide::*;
#[allow(unused_imports)]
use verify_no_dangling_slots_after_slide::*;
mod zgc_relocate_budget_bytes;
#[allow(unused_imports)]
pub use zgc_relocate_budget_bytes::*;
#[allow(unused_imports)]
use zgc_relocate_budget_bytes::*;
mod zgc_flag_default_on;
#[allow(unused_imports)]
pub use zgc_flag_default_on::*;
#[allow(unused_imports)]
use zgc_flag_default_on::*;
mod z_phase_clock;
#[allow(unused_imports)]
pub use z_phase_clock::*;
#[allow(unused_imports)]
use z_phase_clock::*;
mod try_alloc_object;
#[allow(unused_imports)]
pub use try_alloc_object::*;
#[allow(unused_imports)]
use try_alloc_object::*;

static COLORED_ARMING_TEST_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());
pub fn colored_arming_test_lock() -> parking_lot::MutexGuard<'static, ()> {
    COLORED_ARMING_TEST_LOCK.lock()
}

#[cfg(test)]
mod skip_class_filter_tests;

impl ZgcRealHeap {
    /// The `[GC] zgc-skip-class-filter:` figures (zt t27/zc10).
    pub fn skip_class_filter_report(&self) -> skip_class_filter::ZSkipClassFilterReport {
        self.counters.skipcls.report()
    }
}
