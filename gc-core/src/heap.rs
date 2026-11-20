// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Heap — semi-space copying GC object and array storage.
//!
//! # Read this first: the MODULE is production-critical, the [`Heap`] STRUCT is not
//!
//! Recorded 2026-09-20 (generational-GC round 2, lane `plumbing2`) because the
//! question "is `heap.rs` still live code?" keeps being re-asked and the answer
//! differs for the module and for the type. Conflating them is how a `rg
//! heap.rs` sweep gets it wrong in both directions.
//!
//! * **The module is load-bearing and is not going anywhere.** It owns the
//!   object model every backend depends on: [`ObjectHeader`], [`ObjectKind`],
//!   [`ArrayElementType`], [`HEADER_SIZE`], [`SLOT_SIZE`], [`ARRAY_DATA_OFFSET`],
//!   [`GC_FLAG_MARKED`], [`GC_FLAG_COMPACT`], the descriptor coercion rules
//!   ([`coerce_field_value_by_descriptor`], [`field_coercion_loss_report`]) and
//!   several corruption probes that `VmHeap::print_gc_summary` renders.
//!   `gen_heap.rs`, `g1.rs`, `zgc/*.rs`, `old_gen.rs`, `region.rs`, `tlab.rs`,
//!   `concurrent_mark.rs`, `gen_evac.rs` and `vm_heap.rs` all import from it.
//! * **[`Heap`] the struct is VESTIGIAL as a collector.** There is no
//!   `GcBackend` variant for it and no `VmHeap` arm, so no `-XX:+Use…` selector
//!   can reach it and no production allocation path runs through it. Its only
//!   constructions in the workspace are test fixtures (`gc/src/gc.rs`,
//!   `vm/src/memory/gc.rs`) and the two doctest examples on
//!   [`crate::collector::GarbageCollector`]. It survives as a small, complete,
//!   *correct* reference implementation of that trait.
//!
//! It is deliberately **not deleted**: `Heap` is re-exported from `lib.rs` and
//! this is a library crate with embedders (`cratonvm-embed`, `libcratonvm`), so
//! removing the type is a breaking change even though nothing in-tree selects
//! it. The retirement path, and why option (2) — moving it behind `cfg(test)` —
//! is the one that actually retires the recurring grep, is in
//! `docs/feature-designs/default-moving-young-gen.md`.
//! Do not cite `Heap::collect_garbage` or `Heap::enter_gpu_critical` as
//! production APIs; `docs/gc-tuning.md` did until 2026-09-20.
//!
//! Objects are laid out as contiguous blocks:
//! ```text
//! [ObjectHeader (HEADER_SIZE bytes)] [field0] [field1] ... [fieldN]
//! ```
//!
//! [`HEADER_SIZE`] is **16** today — the authority is
//! `cratonvm_types::HEADER_SIZE` (`types/src/heap_types.rs`), never a literal
//! written here. It is used symbolically throughout this file on purpose: the
//! header has already shrunk twice, 32 -> 24 (2026-08-06) and 24 -> 16
//! (completed 2026-08-07), and until 2026-08-07 this very paragraph still read
//! "32 today, a shrink to 24 is mapped out in `header-shrink.md`"
//! — i.e. it was itself an instance of the staleness that document's §6.9
//! catalogues, wrong about both the current value and the pending one. Anything
//! below that needs the number must read the constant.
//!
//! Field cell width depends on the field's type:
//!
//! * **primitive** fields occupy `SLOT_SIZE` (16 bytes) — the full tagged
//!   [`cratonvm_types::Value`] enum (4-byte discriminant, then payload);
//! * **reference** fields occupy `REF_FIELD_SIZE` (8 bytes) — a bare pointer,
//!   `0` meaning null — under the compact reference-field layout, which is the
//!   default (`CRATONVM_COMPACT_REF_FIELDS=0` opts back out to a 16-byte tagged
//!   cell). Such objects are flagged [`GC_FLAG_COMPACT`] and the collector
//!   scans them via the per-class oop-map ([`CompactLayout::ref_offsets`])
//!   instead of tag-scanning uniform cells — see [`compact_oop_scan`].
//!
//! Arrays use **compact, natural element widths**, not `SLOT_SIZE` — 1 byte for
//! `boolean`/`byte`, 2 for `char`/`short`, 4 for `int`/`float`, 8 for
//! `long`/`double`, and `REF_ELEMENT_SIZE` (8) for reference elements. See
//! [`element_byte_size`] and [`array_data_size`], which are the authority:
//! ```text
//! [ObjectHeader (HEADER_SIZE bytes)] [elem0] [elem1] ... [elemN]   // element_byte_size(elem_type) each,
//!                                                                  // data area rounded up to 8 bytes
//! ```
//!
//! The heap uses two arenas (from-space and to-space) for a semi-space
//! copying garbage collector. Allocation is linear in from-space. During
//! collection, live objects are copied to to-space, then spaces are swapped.

use std::sync::atomic::{AtomicI32, AtomicU64, AtomicUsize, Ordering};

use parking_lot::Mutex;

use crate::arena::Arena;
use crate::numa;
use cratonvm_types::{ClassId, ObjectRef, Value};

// Re-export heap types from the shared types crate.
use crate::gc_flags;
use cratonvm_types::narrow_oop::{read_ref_slot, ref_element_size, write_ref_slot};
pub use cratonvm_types::{
    array_data_size, array_data_size_checked, array_element_type_from_tag, element_byte_size,
    object_kind_from_tag, ArrayElementType, ObjectHeader, ObjectKind, ARRAY_DATA_OFFSET,
    ARRAY_LENGTH_OFFSET, AUTOBOX_CLASS_ID, GC_FLAG_COMPACT, GC_FLAG_HEADER, GC_FLAG_MARKED,
    GC_FLAG_OLD_GEN, HEADER_SIZE, REF_ELEMENT_SIZE, REF_FIELD_SIZE, SLOT_SIZE,
};
use cratonvm_types::{class_layout_for_fields, is_compact_object, CompactLayout};
use std::sync::Arc;

/// H2-CID0 (2026-08-05) — compact objects whose class oop map the layout
/// registry could not produce, so the GC fell back to scanning their packed
/// body as legacy 16-byte `Value` cells and saw none of their reference slots.
///
/// A marking FAIL-OPEN: non-zero means some object's outgoing edges were
/// invisible to the collector, which is licence to reclaim its referents while
/// it is live. Expected to be ZERO. Reported by `VmHeap::print_gc_summary`.
pub static COMPACT_OOP_MAP_MISSING: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

// ---------------------------------------------------------------------------
// Invariant: a published TLAB skip span holds NO ROOT
// ---------------------------------------------------------------------------

/// Roots found pointing INTO a published TLAB skip span, across every backend.
///
/// A skip span is by contract the still-RESERVED, **un-allocated** tail
/// `[cursor, end)` of some thread's TLAB, so any non-zero reading is a stale
/// span hiding a live object. See [`skip_spans_hold_no_root`].
pub static SKIP_SPAN_ROOT_VIOLATIONS: AtomicU64 = AtomicU64::new(0);

/// Roots EXACTLY at a published skip span's START, which
/// [`skip_spans_hold_no_root`] counts here and does not report.
///
/// gen r5w1/young5 (2026-09-26). The span's start is its TLAB's allocation
/// cursor: one past the end of the last allocated object, not an object. A
/// peer stopped by the take-over is caught in compiled code, whose inline TLAB
/// bump (`emit_inline_tlab_new`) leaves the NEW cursor in a register after the
/// commit and holds the object address (== the published cursor) in another
/// between its header stores and the commit. Either word equals the span start
/// exactly, and the unallocated zero bytes there read as an empty object, so
/// the conservative root set carries it. Measured by the orchestrator on the
/// default path (`GenR4W6AllocRateProbe`, `GenR4W4JitWarmDivertProbe`, ...:
/// `root == span start`, checksums correct). Neither case hides a live object
/// from the collection: in the first there is no object, in the second the
/// object is not allocated until the peer resumes and commits, and its body is
/// all zero until then. A real stale span (published by an earlier cycle, its
/// owner then allocating into it) shows up as roots INSIDE the span, which are
/// still reported.
pub static SKIP_SPAN_CURSOR_ROOTS: AtomicU64 = AtomicU64::new(0);

/// Roots strictly INSIDE a published skip span whose memory PROVES it
/// unallocated, which [`skip_spans_hold_no_root`] counts here and does not
/// report (gcd d10/t, 2026-09-28).
///
/// The take-over probe on Generational and G1 answers any aligned word inside
/// a live arena with the word itself (`is_heap_addr`: "alignment + containment,
/// no header read"), and its companion rule then roots the word below every
/// such echo. A frozen peer's register file and stack band hold plenty of
/// words inside its OWN reserved tail that name no object: the new cursor its
/// inline bump computed but has not committed yet (`LEA RAX, [R11 + size]`
/// before `MOV [R10 + cursor], RAX`), the TLAB `end` a Rust refill left in a
/// caller-saved register or a dead spill slot (its companion is `end - 8`), and
/// stale words naming cells a non-moving sweep freed and the refill then
/// re-carved into this TLAB. None of them is an object: the owner commits its
/// cursor only after writing the whole header (the single linearization point
/// of `emit_inline_tlab_new` / `emit_inline_tlab_newarray` /
/// `emit_inline_tlab_new_ir_impl`), every refill backend hands out ZEROED
/// memory (the JIT's default zero elision is correct only because of that), and
/// the span is read from the owner's live `Tlab` in the same pause that
/// consumes it, with the owner stopped. So an unallocated tail reads all-zero
/// past at most ONE header at its start (the object a peer frozen between its
/// header stores and its commit is building, whose body is still zero), and a
/// span that reads so holds no object a root could name. The d7 reports were
/// exactly these shapes (`root = end - 8` twice, `root = start + 0x50` once).
pub static SKIP_SPAN_UNALLOCATED_ROOTS: AtomicU64 = AtomicU64::new(0);

/// Bounded report counter for [`SKIP_SPAN_ROOT_VIOLATIONS`].
static SKIP_SPAN_ROOT_REPORTS: AtomicU64 = AtomicU64::new(0);

/// Span-list length at or below which [`skip_spans_hold_no_root`] keeps the
/// nested loop: for a handful of spans a sort and a per-root binary search cost
/// more than the linear compares they replace.
const SKIP_SPAN_LINEAR_MAX: usize = 8;

/// A ROOT pointing into a published TLAB skip span **disproves the span**.
///
/// A skip span is the still-reserved, un-allocated tail of a thread's TLAB.
/// The non-moving sweeps' linear walks resync past it so an un-retired tail
/// cannot desync them (BUG-03), and `mark_young`'s anchor oracle builds its
/// `verified_spans` from the same list -- so an address inside one is answered
/// "free/gap space, not an object" and a root pointing there is dropped
/// without marking. That is sound exactly while the span really is
/// unallocated, and catastrophic the moment it is not: the object is neither
/// scanned nor swept, so it survives while everything it references is freed
/// underneath it.
///
/// `roots` is the exact set the mark phase is about to be given, so the
/// question is decidable at the top of a collection, cheaply (the skip list
/// has at most one entry per live thread), and unconditionally -- which is the
/// point. The defect this exists for hid behind an `AbstractMethodError` in
/// netty's `ObjectCleanerTest` and took a full session to reach, because
/// nothing in either sweep said the span was wrong: it was published by an
/// earlier collection, its owner then bump-allocated into it, and a later
/// collection declined to overwrite it (see the unconditional publish in
/// `vm::runtime::interpreter::gc_and_alloc`, which is the repair).
///
/// **Reports, never repairs.** Dropping the span here would put the walk back
/// on bytes that may genuinely be an un-retired tail -- the desync skipping
/// was introduced to prevent -- and this check cannot tell a stale span from a
/// live one, only that THIS one is stale. The publish side establishes the
/// invariant; this is where it is checked.
///
/// ONE function for both backends deliberately. The first cut lived in
/// `gen_heap.rs` alone, which is why the G1 arm of the original A/B reported
/// `guard_fired=0/8` on runs that were failing 8/8 -- a check present in one
/// sweep and absent from the other reads as "the collector is fine here", and
/// that is the reading a second copy would eventually drift into.
///
/// `spans` are ABSOLUTE `[start, end)` address pairs; a caller holding offsets
/// (the generational sweep does) converts before calling. Passing OFFSETS
/// here does not fail loudly -- it simply never matches, and a guard that
/// cannot match reports a clean zero forever, which is worse than no guard.
/// That is the whole reason the conversion is the caller's job and this
/// signature takes one representation only.
///
/// Returns the number of roots found inside a span, so the invariant can be
/// tested without reading a process-global counter. Callers ignore it; the
/// reporting is the point at the call sites.
///
/// A root EXACTLY at a span's start is not a violation (gen r5w1/young5): the
/// start is the TLAB cursor, which compiled code's inline bump leaves in a
/// register; see [`SKIP_SPAN_CURSOR_ROOTS`], where it is counted instead.
///
/// A root strictly INSIDE a span is a violation only when the span's memory
/// does not read unallocated (gcd d10/t): see [`SKIP_SPAN_UNALLOCATED_ROOTS`]
/// for the shapes that were reported on healthy runs and the proof that they
/// name no object, and [`judge_skip_span_memory`] for the test. A genuinely
/// stale span -- its owner allocated into it after it was published -- holds
/// the owner's headers and field values past its first header, and is still
/// reported.
///
/// Up to [`SKIP_SPAN_LINEAR_MAX`] spans are checked with the plain nested loop;
/// past that, with a sorted copy (see the PERF note in the body).
pub fn skip_spans_hold_no_root(
    spans: &[(usize, usize)],
    roots: &[ObjectRef],
    backend: &'static str,
) -> usize {
    // SAFETY: every published span is the reserved tail of a live thread's
    // TLAB (`ThreadRegistry::collect_reserved_tlab_tails` -> `Tlab::reserved_tail`),
    // read in the pause that consumes it with its owner stopped: committed,
    // zeroed-at-refill arena memory the owner is about to allocate into, so
    // reading it cannot fault. `judge_skip_span_memory` is reached only for a
    // span that contains a root, never for an empty or inverted one.
    skip_spans_hold_no_root_judged(spans, roots, backend, &|start, end| unsafe {
        judge_skip_span_memory(start, end)
    })
}

/// What [`judge_skip_span_memory`] read in a published skip span.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkipSpanMemory {
    /// Zero past at most one object header at the span's start: nothing in
    /// the span is an allocated object.
    Unallocated,
    /// A word no unallocated tail can hold, `offset` bytes into the span: a
    /// non-zero word past the first header, or a non-header first word.
    Holds { offset: usize },
}

/// Read a published skip span `[start, end)` and say whether it is still the
/// UNALLOCATED tail its publication claims (gcd d10/t).
///
/// An unallocated TLAB tail is zero (every refill backend zeroes; the JIT's
/// default zero elision depends on it). The one legitimate non-zero content is
/// the header of the object a peer frozen between its header stores and its
/// cursor commit is building AT the span start (the published cursor): its
/// class word and mark word, and for a legacy instance or an array the second
/// header word (`num_slots` / length). Its body is still zero. So the span is
/// unallocated iff the word at `start` is header-shaped (`GC_FLAG_HEADER` in
/// its mark word) and every word past that header -- 8 bytes for a compact
/// instance, [`HEADER_SIZE`] otherwise -- is zero; or its mark dword is still
/// zero (the header half written: any class dword, at most a 32-bit length /
/// `num_slots` in the second word) and every word past [`HEADER_SIZE`] is zero
/// (t1 wave 12, lane ARGFIX; see the body).
///
/// A stale span (the owner allocated into it after publication) fails this as
/// soon as it holds a second object (its header's mark word is never zero) or
/// any non-null field of the first. The one stale shape it accepts is a
/// single object whose fields are all zero -- which has no referent to lose,
/// so the "everything it references is freed" hazard cannot occur for it.
///
/// Cost: one pass over the span, stopping at the first non-zero word, and only
/// for a span a root falls strictly inside (a TLAB is at most 1 MiB).
///
/// # Safety
///
/// `[start, end)` must be readable memory for the duration of the call.
pub unsafe fn judge_skip_span_memory(start: usize, end: usize) -> SkipSpanMemory {
    if start & 7 != 0 || start >= end || end - start < 8 {
        return SkipSpanMemory::Holds { offset: 0 };
    }
    // SAFETY: the caller's contract; 8-aligned, and at least one word long.
    let first = unsafe { std::ptr::read(start as *const u64) };
    // t1 wave 12, lane ARGFIX: the mark word not written YET. Every inline
    // bump writes it AFTER the class dword (`emit_inline_tlab_new_ir_impl`:
    // class, mark, num_slots, hash; the array bumps: class, LENGTH, mark,
    // hash), so a peer frozen between those stores leaves the class dword at
    // +0 (zero for a primitive array) and, for an array, its 32-bit length at
    // +8 -- with the mark dword still zero. That half-written header is the
    // same in-flight object as a whole one (its body is zero, its cursor not
    // committed), not a stale span; the old rule read it as `Holds { 0 }`
    // (instance) or `Holds { 8 }` (`new long[6]`: class 0, length 6), the
    // `T1W8SatbgateYoungOverwrite` guard report under
    // `CRATONVM_DBG_XT_FORCE_TAKEOVER=1`. The second word is admitted only as
    // a 32-bit length / `num_slots` with a zero hash dword.
    let body_from = if first >> 32 == 0 {
        if end - start >= 16 {
            // SAFETY: the caller's contract; the span holds two words.
            let second = unsafe { std::ptr::read((start + 8) as *const u64) };
            if second >> 32 != 0 {
                return SkipSpanMemory::Holds { offset: 8 };
            }
        }
        HEADER_SIZE
    } else {
        // SAFETY: as above; the mark word is the header word's high half.
        let mark = unsafe {
            std::ptr::read_unaligned((start + cratonvm_types::MARK_WORD_OFFSET) as *const u32)
        };
        let flags = ObjectHeader::gc_flags_of(mark);
        if flags & GC_FLAG_HEADER == 0 {
            return SkipSpanMemory::Holds { offset: 0 };
        }
        if flags & GC_FLAG_COMPACT != 0 {
            cratonvm_types::COMPACT_HEADER_SIZE
        } else {
            HEADER_SIZE
        }
    };
    let mut p = start + body_from;
    while p + 8 <= end {
        // SAFETY: the caller's contract.
        if unsafe { std::ptr::read(p as *const u64) } != 0 {
            return SkipSpanMemory::Holds { offset: p - start };
        }
        p += 8;
    }
    SkipSpanMemory::Unallocated
}

/// [`skip_spans_hold_no_root`] with the span-memory reader as a parameter, so
/// the counting rule is testable over fake addresses nothing may read.
pub fn skip_spans_hold_no_root_judged(
    spans: &[(usize, usize)],
    roots: &[ObjectRef],
    backend: &'static str,
    judge: &dyn Fn(usize, usize) -> SkipSpanMemory,
) -> usize {
    if spans.is_empty() || roots.is_empty() {
        return 0;
    }
    // gen r5w1/young5: a root EXACTLY at a span's start is the TLAB cursor,
    // not an object; see `SKIP_SPAN_CURSOR_ROOTS`. Counted, not reported.
    let mut cursor_words = 0usize;
    // Roots strictly inside a span, as `(root, start, end)`, judged below
    // (gcd d10/t). Empty on a healthy pause with no such word.
    let mut inside: Vec<(usize, usize, usize)> = Vec::new();
    if spans.len() <= SKIP_SPAN_LINEAR_MAX {
        for r in roots {
            let a = r.as_ptr() as usize;
            for &(start, end) in spans {
                if a == start && start < end {
                    cursor_words += 1;
                } else if a > start && a < end {
                    inside.push((a, start, end));
                }
            }
        }
    } else {
        // PERF (gc-common w1-g): O(roots * log spans) instead of
        // O(roots * spans). The list has one span per in-JIT peer stopped by
        // the takeover, and this runs on EVERY collection with the full root
        // set in hand -- 64 spinning compiled threads against a 10^6-root set
        // was 6.4e7 compares inside the pause. Sorting a private copy by start
        // and carrying a running maximum of the ends keeps the count EXACT
        // even if the published list were ever unsorted or overlapping (the
        // standing invariant says it is neither, but this guard exists
        // precisely for the day an invariant is wrong): for a root `a`, every
        // span that contains it has `start <= a`, and walking those backwards
        // can stop as soon as no earlier span reaches past `a`.
        let mut sorted: Vec<(usize, usize)> = spans.to_vec();
        sorted.sort_unstable();
        let mut reach = Vec::with_capacity(sorted.len());
        let mut max_end = 0usize;
        for &(_, end) in &sorted {
            max_end = max_end.max(end);
            reach.push(max_end);
        }
        for r in roots {
            let a = r.as_ptr() as usize;
            let mut j = sorted.partition_point(|&(start, _)| start <= a);
            while j > 0 && reach[j - 1] > a {
                j -= 1;
                let (start, end) = sorted[j];
                if a < end {
                    if a == start {
                        cursor_words += 1;
                    } else {
                        inside.push((a, start, end));
                    }
                }
            }
        }
    }
    if cursor_words != 0 {
        SKIP_SPAN_CURSOR_ROOTS.fetch_add(cursor_words as u64, Ordering::Relaxed);
    }
    if inside.is_empty() {
        return 0;
    }
    // gcd d10/t: judge each span a root fell inside ONCE, by its memory. A
    // span that reads unallocated names no object (see
    // `SKIP_SPAN_UNALLOCATED_ROOTS`); its roots are counted, not reported.
    let mut verdicts: std::collections::HashMap<(usize, usize), SkipSpanMemory> =
        std::collections::HashMap::new();
    let mut violations = 0usize;
    let mut unallocated = 0usize;
    let mut first: Option<(usize, usize, usize, usize)> = None;
    for &(a, start, end) in &inside {
        let verdict = *verdicts
            .entry((start, end))
            .or_insert_with(|| judge(start, end));
        match verdict {
            SkipSpanMemory::Unallocated => unallocated += 1,
            SkipSpanMemory::Holds { offset } => {
                violations += 1;
                if first.is_none() {
                    first = Some((a, start, end - start, offset));
                }
            }
        }
    }
    if unallocated != 0 {
        SKIP_SPAN_UNALLOCATED_ROOTS.fetch_add(unallocated as u64, Ordering::Relaxed);
    }
    if violations == 0 {
        return 0;
    }
    SKIP_SPAN_ROOT_VIOLATIONS.fetch_add(violations as u64, Ordering::Relaxed);
    let n = SKIP_SPAN_ROOT_REPORTS.fetch_add(1, Ordering::Relaxed);
    if n < 8 {
        let (root, span, sz, offset) = first.unwrap_or((0, 0, 0, 0));
        tracing::error!(
            target: "cratonvm::gc::guard",
            backend,
            roots_in_spans = violations,
            root = format!("{root:#x}"),
            span = format!("{span:#x}+{sz:#x}"),
            nonzero_at = format!("+{offset:#x}"),
            spans = spans.len(),
            "a ROOT points into a published TLAB skip span, which is supposed to be the \
             un-allocated tail of a reserved TLAB, and the span's memory disagrees: it holds \
             a non-zero word past its first header (`nonzero_at`). The span is therefore \
             stale: its owner allocated into it after it was published. Every walk in this \
             collection skips it and the mark oracle calls it gap space, so the object the \
             root names is neither marked nor swept while everything it references is freed.",
        );
    }
    violations
}

/// gci i27/gs (P1/ga, `gcd-d10t-proposal-takeover-probe-skips-reserved-tlab-tails-20260928`):
/// `CRATONVM_XT_TAKEOVER_DROP_TAIL_WORDS=1` (default on). Read once per
/// take-over pause by the VM (`stw_take_over_and_wait`).
pub fn takeover_drop_tail_words_enabled() -> bool {
    true
}

/// gci i27/gs (P1/ga, d10t): drop every take-over root that lies STRICTLY
/// inside a published reserved-TLAB span whose memory reads unallocated
/// (`judge_skip_span_memory` -> `SkipSpanMemory::Unallocated`). Such a
/// word names no object -- the uncommitted cursor of an inline bump, a TLAB
/// `end` left in a register, a stale word naming a re-carved cell -- so as a
/// root it only costs: the Generational anchor oracle drops it as gap space,
/// G1 treats it as a non-object root (its region is pinned by the span
/// itself, `jit_pinned_region_set_screened`), ZGC resolves it to nothing or
/// to the object below the span. A root EXACTLY at a span start is kept (the
/// in-flight object a peer frozen before its commit is building), and so is
/// every root of a span that holds anything (`Holds`: the guard
/// `skip_spans_hold_no_root` must still see it). Each span is judged once, and
/// only when a root falls inside it. Returns the roots dropped.
///
/// # Safety
///
/// Every span must be readable for the call: the reserved tails the VM
/// collected in THIS pause with every owner stopped (the contract of
/// `skip_spans_hold_no_root`).
pub unsafe fn drop_roots_inside_unallocated_spans(
    spans: &[(usize, usize)],
    roots: &mut Vec<ObjectRef>,
) -> usize {
    drop_roots_inside_unallocated_spans_judged(spans, roots, &|start, end| unsafe {
        judge_skip_span_memory(start, end)
    })
}

/// [`drop_roots_inside_unallocated_spans`] with the span reader as a
/// parameter (testable over fake addresses).
pub fn drop_roots_inside_unallocated_spans_judged(
    spans: &[(usize, usize)],
    roots: &mut Vec<ObjectRef>,
    judge: &dyn Fn(usize, usize) -> SkipSpanMemory,
) -> usize {
    if spans.is_empty() || roots.is_empty() {
        return 0;
    }
    // O(roots * log spans + verdicts) instead of two linear `find`s per root:
    // the same sorted-starts + prefix-max-end search as
    // `skip_spans_hold_no_root_judged`, walking EVERY span that strictly
    // contains a root (a root is dropped only when each of them reads
    // `Unallocated`; the old `find` looked at the first one only). Verdicts are
    // cached per span, so each span is judged at most once.
    let mut sorted: Vec<(usize, usize)> = spans.to_vec();
    sorted.sort_unstable();
    let mut reach = Vec::with_capacity(sorted.len());
    let mut max_end = 0usize;
    for &(_, end) in &sorted {
        max_end = max_end.max(end);
        reach.push(max_end);
    }
    let mut verdicts: std::collections::HashMap<(usize, usize), bool> =
        std::collections::HashMap::new();
    let before = roots.len();
    roots.retain(|r| {
        let a = r.as_ptr() as usize;
        let mut j = sorted.partition_point(|&(start, _)| start <= a);
        let mut inside_any = false;
        while j > 0 && reach[j - 1] > a {
            j -= 1;
            let (start, end) = sorted[j];
            if a > start && a < end {
                inside_any = true;
                let unallocated = *verdicts
                    .entry((start, end))
                    .or_insert_with(|| matches!(judge(start, end), SkipSpanMemory::Unallocated));
                if !unallocated {
                    return true;
                }
            }
        }
        // Not inside any span: keep. Inside only unallocated spans: drop.
        !inside_any
    });
    before - roots.len()
}

/// For a compact object (one allocated under the compact reference-field
/// layout, [`GC_FLAG_COMPACT`]), return its class oop-map together with the
/// object's own allocated body size in bytes. The GC scans/remaps **only** the
/// reference slots listed in [`CompactLayout::ref_offsets`], each an 8-byte
/// pointer at `HEADER_SIZE + offset`.
///
/// Returns `None` for a legacy object — the caller must fall back to
/// tag-scanning its uniform 16-byte cells.
///
/// The `body_size` is the body of the layout VERSION registered for this
/// object's own `(class_id, num_slots)`, which may be smaller than the class's
/// current `body_size` if the (synthetic-stub) class grew after this object was
/// allocated. Callers iterate `ref_offsets` in ascending order and stop at the
/// first offset that would read past `body_size`, so a grown class never makes
/// the GC read out of bounds.
///
/// # No production caller (gc-common w3-e, 2026-09-23)
///
/// Every scan site now uses the borrowing [`with_compact_oop_scan`] (or its
/// scalar form [`with_compact_oop_scan_ids`]), which serves the same answer
/// without an `Arc` clone/drop per scanned object. This owned-handle form is
/// kept only as the reference the equivalence test
/// (`with_compact_oop_scan_agrees_with_compact_oop_scan`) compares against;
/// new code should not call it.
#[inline]
pub fn compact_oop_scan(header: &ObjectHeader) -> Option<(Arc<CompactLayout>, usize)> {
    if !is_compact_object(header) {
        return None;
    }
    let cid = header.class_id.as_u32();
    // Per-thread single-entry cache. The GC scans long runs of same-class
    // objects (e.g. a tree of one node type), so this avoids re-taking the
    // registry `RwLock` on every scanned object — a hit is just a generation
    // load + an `Arc` refcount bump. Validated against `layout_generation()`
    // so a redefine (which bumps the generation) cannot serve a stale layout.
    thread_local! {
        static OOP_CACHE: std::cell::RefCell<Option<(u32, u32, u64, Arc<CompactLayout>)>> =
            const { std::cell::RefCell::new(None) };
    }
    let gen = cratonvm_types::layout_generation();
    let layout = OOP_CACHE.with(|c| {
        {
            let cache = c.borrow();
            if let Some((cached_cid, cached_fields, cached_gen, arc)) = &*cache {
                if *cached_cid == cid && *cached_fields == header.num_slots() && *cached_gen == gen
                {
                    return Some(arc.clone());
                }
            }
        }
        let arc = class_layout_for_fields(cid, header.num_slots())?;
        *c.borrow_mut() = Some((cid, header.num_slots(), gen, arc.clone()));
        Some(arc)
    });
    let Some(layout) = layout else {
        // See `COMPACT_OOP_MAP_MISSING`. Returning `None` here is not a
        // "this is a legacy object" answer — the header already said it is
        // not — it is the GC agreeing to scan a packed body as `Value` cells
        // and miss every reference in it.
        note_compact_oop_map_missing(cid, header.num_slots(), gen);
        return None;
    };
    let total_size = layout.total_size as usize;
    Some((layout, total_size))
}

/// The `COMPACT_OOP_MAP_MISSING` record, shared by [`compact_oop_scan`] and
/// [`with_compact_oop_scan`] so the two entry points cannot disagree about
/// what a missing oop map costs or how loudly it is reported.
#[cold]
#[inline(never)]
fn note_compact_oop_map_missing(cid: u32, num_slots: u32, gen: u64) {
    let n = COMPACT_OOP_MAP_MISSING.fetch_add(1, Ordering::Relaxed);
    if n < 8 {
        tracing::error!(
            target: "cratonvm::gc::guard",
            class_id = cid,
            num_slots,
            layout_generation = gen,
            "object carries GC_FLAG_COMPACT but the layout registry has no oop map for \
             (class_id, num_slots) — the GC is about to scan its packed body as legacy \
             16-byte `Value` cells and will miss every reference slot it holds.",
        );
    }
}

/// Borrowing twin of `compact_oop_scan`: run `f` against the compact
/// object's oop map and its body size, IN PLACE, and return `f`'s result; or
/// `None` for a legacy object (and for a compact object whose map is missing,
/// counted into [`COMPACT_OOP_MAP_MISSING`] exactly as `compact_oop_scan`
/// counts it).
///
/// # PERF (gc-common w1-g): no refcount traffic per scanned object
///
/// `compact_oop_scan` returns an `Arc<CompactLayout>`, so every scanned object
/// pays an `Arc` clone out of its cache and a drop in the caller: two atomic
/// read-modify-writes on the layout's refcount. Under the PARALLEL evacuators
/// (Generational `CRATONVM_GC_PAR_EVAC`, G1's worker pool) every worker scanning
/// a run of same-class objects -- the common shape, a tree of one node type --
/// increments and decrements the SAME refcount, so that cache line ping-pongs
/// between every GC worker for the whole copy phase. That is a scaling cost,
/// not just a constant one: it grows with the worker count.
///
/// This serves the same registry answer through
/// [`cratonvm_types::with_class_layout`], which lends the cached layout by
/// reference (see that function for its re-entrancy contract: `f` may itself
/// call back into the layout cache, as a copying scan does for the referent's
/// class). The answers are identical: both entry points resolve
/// `(class_id, num_slots)` against the same registry and generation.
///
/// gc-common w3-e (2026-09-23): every in-crate scan site has migrated
/// (`handoff-g-compact-oop-scan-borrow`, now under `applied/`): the
/// generational young scan, copy phase and dirty-card walk, the concurrent
/// old-gen marker, and the old-gen reference walk (the last through
/// [`with_compact_oop_scan_ids`]).
///
/// # `f` runs while the thread's layout cache is borrowed
///
/// A layout lookup made from inside `f` (a copying scan resolving the
/// REFERENT's class) still hits; a nested MISS cannot install and costs one
/// uncached registry probe. See [`cratonvm_types::with_class_layout`].
#[inline]
pub fn with_compact_oop_scan<R>(
    header: &ObjectHeader,
    f: impl FnOnce(&CompactLayout, usize) -> R,
) -> Option<R> {
    if !is_compact_object(header) {
        return None;
    }
    with_compact_oop_scan_ids(header.class_id.as_u32(), header.num_slots(), f)
}

/// [`with_compact_oop_scan`] for a caller that has already copied the two
/// header scalars out and must NOT hold a `&ObjectHeader` while `f` runs.
///
/// The caller is responsible for the `is_compact_object` test (this form has
/// no header to test); it answers `None` only for a missing oop map, which it
/// counts into [`COMPACT_OOP_MAP_MISSING`] exactly as the header form does.
///
/// # Why a scalar form exists (gc-common w3-e)
///
/// `OldGen::for_each_old_gen_ref` calls back into code that may WRITE the
/// referent's header, and for a self-referential object the referent is the
/// object being walked. A `&ObjectHeader` passed as a function argument is live
/// for the whole call, so handing one to [`with_compact_oop_scan`] and then
/// running `f` inside it would alias that write. The walk therefore snapshots
/// `(class_id, num_slots)` up front and looks the layout up by value.
#[inline]
pub fn with_compact_oop_scan_ids<R>(
    class_id: u32,
    num_slots: u32,
    f: impl FnOnce(&CompactLayout, usize) -> R,
) -> Option<R> {
    let out = cratonvm_types::with_class_layout(class_id, num_slots, |layout| {
        f(layout, layout.total_size as usize)
    });
    if out.is_none() {
        note_compact_oop_map_missing(class_id, num_slots, cratonvm_types::layout_generation());
    }
    out
}

// ---------------------------------------------------------------------------
// Heap — semi-space copying GC heap
// ---------------------------------------------------------------------------

/// Default semi-space size: 32 MB per semi-space (64 MB total).
const DEFAULT_SEMI_SPACE_SIZE: usize = 32 * 1024 * 1024;

/// Maximum array length — prevents excessive allocation from malformed bytecode.
/// Matches HotSpot's practical limit.
const MAX_ARRAY_LENGTH: usize = i32::MAX as usize; // 2^31 - 1

/// The array-store error channel — see [`cratonvm_types::oob_index_code`] and
/// [`cratonvm_types::ARRAY_STORE_OUT_OF_MEMORY`]. Re-exported here because
/// every backend in this crate reaches for them through `crate::heap::`.
pub use cratonvm_types::{oob_index_code, ARRAY_STORE_OUT_OF_MEMORY};

/// GC threshold: trigger collection when from-space usage exceeds 75% capacity.
const GC_THRESHOLD_PERCENT: usize = 75;

/// Semi-space heap for Java objects and arrays.
///
/// Uses two arenas (from-space and to-space) for a copying garbage collector.
/// Allocation happens linearly in from-space. During GC, live objects are
/// copied to to-space, then the spaces are swapped.
pub struct Heap {
    /// Compact-layout domain of the VM that owns this heap.
    ///
    /// `class_id` is a per-`ClassStore` index, so it does not identify a class
    /// in the process-global layout registry. Allocating against another
    /// domain's entry gives the object a foreign shape and corrupts every field
    /// access it will ever see. Defaults to the FIRST domain, so a heap nobody
    /// tells behaves exactly as it did before domains existed — and a heap told
    /// the wrong value merely loses compact layouts (tagged slots carry their
    /// own type), it does not corrupt.
    layout_domain: std::sync::atomic::AtomicU32,

    from_space: Mutex<Arena>,
    to_space: Mutex<Arena>,
    /// `(base, len)` of the two semi-space buffers, captured at construction.
    /// The buffers never move and `swap_spaces` exchanges whole arenas, so the
    /// union of the two ranges is invariant: membership needs no arena lock.
    space_ranges: [(usize, usize); 2],
    next_hash_code: AtomicI32,
    gc_threshold: usize,

    // ---- NUMA hint (stub: see TODO below) ---------------------------------
    //
    // Snapshot of the host NUMA topology's node count, captured at
    // `Heap::with_capacity`. Stored so [`Heap::preferred_numa_node`] can
    // bound `numa::current_thread_node()` against it without re-reading the
    // `OnceLock` on every alloc.
    //
    // The current allocator still funnels every request through the single
    // `from_space` semi-space — this field is a "design intent" marker, not
    // an actual per-node partition.
    //
    // TODO(NUMA, multi-arena): replace `from_space`/`to_space` with
    //   `arenas_from: Vec<Mutex<Arena>>` and `arenas_to: Vec<Mutex<Arena>>`,
    //   each indexed by NUMA node. The fast-path alloc would then dispatch
    //   to `arenas_from[numa::current_thread_node()]`, falling back to
    //   round-robin on local OOM. The GC's `collect_garbage` would walk
    //   every (from, to) pair and swap them in lockstep. This is deferred
    //   because the existing GC entry points (`crate::gc::collect`,
    //   `crate::gc::collect_with_finalizers`) take a single `&mut Arena`
    //   pair, so a full multi-arena rewrite touches the collector crate
    //   too — out of scope for this single-file change. See `gc/src/numa.rs`
    //   for `NumaTopology` / `current_thread_node`.
    num_numa_nodes: usize,
    /// Last observed NUMA node hint. Updated on the alloc fast-path purely
    /// for observability / debugging; allocation itself still hits the
    /// single shared `from_space`.
    numa_node_hint: AtomicUsize,

    // ---- GPU-offload coordination (Part F) --------------------------------
    //
    // These fields exist ONLY when the `gpu-offload` Cargo feature is on. They
    // hold the live-token counter and the pinned-ObjectRef set described in
    // [crate::safepoint]. With the feature off the struct has the same shape
    // as before the GPU work.
    /// Number of [`crate::safepoint::SafepointToken`]s currently alive on
    /// this heap. While non-zero, `collect_garbage` and
    /// `collect_garbage_with_finalizers` spin-yield instead of running a
    /// collection. Tokens increment on construction and decrement on
    /// drop; the counter is the sole gate.
    #[cfg(feature = "gpu-offload")]
    pub gpu_critical_count: std::sync::atomic::AtomicU32,

    /// Object refs currently being marshalled to or from the GPU. These
    /// are walked as additional GC roots so that even between kernel
    /// launches (when no token is alive) the underlying JVM arrays stay
    /// reachable for any pending re-use.
    ///
    /// Uses `parking_lot::Mutex` to match every other mutex in this struct.
    /// `HashSet` so a pin/unpin pair is idempotent and order-independent.
    /// Test-only counter for tests that observe "did the GC actually skip
    /// because of the token?" without instrumenting the call path.
    #[cfg(feature = "gpu-offload")]
    pub gpu_pinned_refs: parking_lot::Mutex<std::collections::HashSet<ObjectRef>>,

    /// Number of times `collect_garbage` (or its finalizer variant) was
    /// asked to run but found `gpu_critical_count != 0` and bailed out.
    /// Exposed via [`Heap::gpu_blocked_gc_count`] so tests can assert
    /// "the GC saw the token and stepped aside" without timing tricks.
    #[cfg(feature = "gpu-offload")]
    pub gpu_blocked_gc_count: std::sync::atomic::AtomicU64,
}

// SAFETY: `Heap` contains only `Mutex<Arena>` (which is Send+Sync), an
// `AtomicI32` (Send+Sync), and a `usize` (Send+Sync). The raw pointers
// live *inside* the `Arena` behind the `Mutex`, so they are never directly
// exposed as struct fields. All allocation goes through `self.from_space.lock()`,
// which serializes mutations. After allocation, `ObjectRef` handles are plain
// `*mut u8` pointers into arena-owned memory — reads and writes to disjoint
// objects are safe from different threads because:
//   1. Object fields are at fixed offsets from the allocation base.
//   2. The GC stops the world before relocating objects.
//   3. Volatile field access takes the per-slot stripe lock
//      (`collector::volatile_stripe_lock`) and brackets the access with SeqCst
//      fences, so a 16-byte `Value` is seen fully old or fully new.
//
// HIGH-soundness audit: invariant (2) is now backed at the *type* level by
// [`crate::collector::StopTheWorldToken`]: every mutating-by-`&self` entry
// point on this struct (`collect_garbage`, `collect_garbage_with_finalizers`)
// requires `&StopTheWorldToken` as its first argument, so cross-thread
// callers cannot invoke a moving collection without first parking every
// other mutator. The blanket `unsafe impl` is retained because the heap's
// internal raw `*mut u8` arena pointers are still `!Send` / `!Sync` on
// their own — the impl asserts that the combination of (token-gated
// mutation) + (Mutex-serialised allocation) + (SeqCst volatile fences)
// makes shared `&Heap` usage sound across threads.
unsafe impl Send for Heap {}
unsafe impl Sync for Heap {}

impl Heap {
    /// Bind this heap to its VM's compact-layout domain. Call once, as early as
    /// possible in VM construction: allocations made before it land on tagged
    /// slots, which is safe but larger.
    pub fn set_layout_domain(&self, domain: u32) {
        self.layout_domain
            .store(domain, std::sync::atomic::Ordering::Release);
    }

    /// This heap's compact-layout domain. See [`Self::set_layout_domain`].
    pub fn layout_domain(&self) -> u32 {
        self.layout_domain
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Create a new heap with default capacity (`DEFAULT_SEMI_SPACE_SIZE`,
    /// 32 MiB, per semi-space).
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_SEMI_SPACE_SIZE * 2)
    }

    /// Create a new heap with the given total capacity.
    /// Each semi-space gets half the capacity.
    pub fn with_capacity(total_bytes: usize) -> Self {
        let half = total_bytes.max(1024) / 2;
        let threshold = half * GC_THRESHOLD_PERCENT / 100;
        // Snapshot the host NUMA topology once. On single-node hosts
        // (the common case for non-Linux platforms) `num_nodes == 1`, so
        // the alloc fast-path collapses to its original single-arena
        // behaviour — no regression. See the field-level TODO on `Heap`
        // for the planned multi-arena partitioning.
        let topo = numa::global_topology();
        let num_numa_nodes = topo.num_nodes.max(1);
        let from_arena = Arena::new(half);
        let to_arena = Arena::new(half);
        let space_ranges = [
            (from_arena.base_ptr() as usize, from_arena.capacity()),
            (to_arena.base_ptr() as usize, to_arena.capacity()),
        ];
        Self {
            layout_domain: std::sync::atomic::AtomicU32::new(cratonvm_types::FIRST_LAYOUT_DOMAIN),
            from_space: Mutex::new(from_arena),
            to_space: Mutex::new(to_arena),
            space_ranges,
            next_hash_code: AtomicI32::new(1),
            gc_threshold: threshold,

            num_numa_nodes,
            numa_node_hint: AtomicUsize::new(0),

            #[cfg(feature = "gpu-offload")]
            gpu_critical_count: std::sync::atomic::AtomicU32::new(0),
            #[cfg(feature = "gpu-offload")]
            gpu_pinned_refs: parking_lot::Mutex::new(std::collections::HashSet::new()),
            #[cfg(feature = "gpu-offload")]
            gpu_blocked_gc_count: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Return the NUMA node this thread should *prefer* for allocation,
    /// clamped to the topology size captured at heap construction.
    ///
    /// On single-node hosts this is always `0`. On multi-node hosts the
    /// value is derived from [`numa::current_thread_node()`].
    ///
    /// Currently informational: the alloc fast-path records this on every
    /// call (so observers can confirm per-thread node affinity) but the
    /// underlying arena is still shared. See the field-level TODO on
    /// [`Heap`] for the full multi-arena plan.
    #[inline]
    pub fn preferred_numa_node(&self) -> usize {
        if self.num_numa_nodes <= 1 {
            return 0;
        }
        let raw = numa::NumaTopology::current_thread_node();
        if raw < self.num_numa_nodes {
            raw
        } else {
            // current_thread_node returned a node beyond what we
            // snapshotted (topology changed mid-flight, or platform
            // returned a stale CPU id). Fold back to node 0 rather
            // than indexing OOB.
            0
        }
    }

    // LANE W6-A — `pub fn last_numa_node_hint()` was deleted here.
    //
    // Zero callers anywhere in the workspace, including tests: its own doc
    // said "mainly useful for tests / observability" and neither had ever
    // used it. It is a cache of the LAST value `refresh_numa_hint` happened to
    // observe, not a count of anything, so there is no question about a run it
    // answers — `preferred_numa_node()` is the live value and is read. The
    // field `numa_node_hint` stays: it is the alloc fast path's cache and has
    // a purpose independent of being reported.

    /// Number of NUMA nodes the heap is aware of (>= 1).
    pub fn num_numa_nodes(&self) -> usize {
        self.num_numa_nodes
    }

    /// Allocation fast-path hook: refresh the cached NUMA hint from the
    /// current OS thread. Cheap on single-node hosts (early return).
    ///
    /// TODO(NUMA, multi-arena): once `from_space` is replaced with a
    /// per-node `Vec<Mutex<Arena>>`, this hook should return the chosen
    /// arena index and the caller should lock that arena instead of the
    /// shared `from_space`. Fallback policy on local OOM: round-robin
    /// over the remaining nodes before giving up (so the heap behaves
    /// like a unified pool only when every node is exhausted).
    #[inline]
    fn refresh_numa_hint(&self) -> usize {
        if self.num_numa_nodes <= 1 {
            // Single-node host: avoid the syscall / sysfs read entirely.
            return 0;
        }
        let node = self.preferred_numa_node();
        // Relaxed: this is purely observational; allocations don't depend
        // on its value yet, and racing writes between threads are fine.
        self.numa_node_hint.store(node, Ordering::Relaxed);
        node
    }

    /// Allocate a new Java object with `num_fields` field slots, all zeroed.
    ///
    /// # Panics
    /// Panics if `num_fields * SLOT_SIZE` overflows.
    pub fn alloc_object(&self, class_id: ClassId, num_fields: usize) -> ObjectRef {
        let compact_total = cratonvm_types::compact_object_total_size(
            self.layout_domain(),
            class_id.as_u32(),
            num_fields,
        );
        let total_size = compact_total.unwrap_or_else(|| {
            num_fields
                .checked_mul(SLOT_SIZE)
                .and_then(|b| HEADER_SIZE.checked_add(b))
                .expect("object total size overflow")
        });
        let ptr = self.alloc_zeroed(total_size);

        let mut header = ObjectHeader::new(
            class_id,
            ObjectKind::Object,
            ArrayElementType::Reference, // hash installed lazily in the mark word on first request
            0,
            u32::try_from(num_fields).expect("field count exceeds u32::MAX"),
        );
        if let Some(total) = compact_total {
            header.set_compact_shape(num_fields as u32, total);
        }

        // SAFETY: `ptr` was just returned by `alloc_zeroed`, which guarantees it
        // is valid, non-null, properly aligned (8-byte), and has at least
        // `total_size` bytes available. Writing the header is within bounds
        // (HEADER_SIZE <= total_size). `ObjectRef::from_raw` wraps the raw
        // pointer in a typed handle; the pointed-to memory remains valid for
        // the lifetime of the current semi-space (until the next GC swap).
        unsafe {
            std::ptr::write(ptr as *mut ObjectHeader, header);
            ObjectRef::from_raw(ptr)
        }
    }

    /// Allocate a new Java object with `num_fields` field slots and
    /// initialize each slot to the JVM-spec-mandated default value based on
    /// its descriptor byte.
    ///
    /// `descriptor_bytes[i]` must be the first byte of the JVM field
    /// descriptor for field index `i` (e.g. `b'I'` for int, `b'J'` for long,
    /// `b'L'`/`b'['` for reference types). Fields not covered by
    /// `descriptor_bytes` (when `descriptor_bytes.len() < num_fields`) are
    /// initialized to an explicit `Value::Object(None)` — the correct
    /// default for reference slots.
    ///
    /// This fixes a subtle correctness bug. `read_slot` does a raw
    /// `ptr::read::<Value>`; after `Value::Object` became
    /// `Option<ObjectRef>` with a `NonNull` niche, the all-zero bit pattern
    /// left by `alloc_zeroed` decodes as `Value::Int(0)` (the
    /// discriminant-0 variant of [`Value`]), NOT `Value::Object(None)`.
    /// So neither a primitive nor a reference default can be obtained "for
    /// free" from zeroed memory any more — both must be written explicitly.
    /// For primitive fields the JVM spec §2.3 mandates a typed zero —
    /// `Value::Int(0)` for I/S/B/C/Z, `Value::Long(0)` for J,
    /// `Value::Float(0.0)` for F, `Value::Double(0.0)` for D. For reference
    /// fields the default is `null` (`Value::Object(None)`). Without this
    /// explicit default-init an unwritten reference field reads as
    /// `Int(0)`, violating the documented slot contract, and an unwritten
    /// `int` field would (under the OLD zeroed==Object(None) decode) read as
    /// `Object(None)`, breaking `Unsafe.compareAndSetInt` comparisons
    /// against `Int(0)` (e.g. `ConcurrentHashMap.initTable`) and livelocking
    /// the caller in a CAS retry loop.
    ///
    /// Unknown/malformed descriptor bytes initialize to the reference
    /// default (`Object(None)`), matching the legacy intent so this cannot
    /// regress non-primitive paths.
    ///
    /// # Panics
    /// Panics if `num_fields * SLOT_SIZE` overflows.
    pub fn alloc_object_with_descriptors(
        &self,
        class_id: ClassId,
        num_fields: usize,
        descriptor_bytes: &[u8],
    ) -> ObjectRef {
        let obj = self.alloc_object(class_id, num_fields);
        // gc-common w4-e: `alloc_object` plans the COMPACT body whenever the
        // class has a registered layout, and a compact body is NOT `num_fields`
        // 16-byte cells -- it is the packed per-class layout, usually far
        // smaller. The loop below strides `HEADER_SIZE + i * SLOT_SIZE`, so on
        // a compact object it wrote tagged cells past the end of the
        // allocation (into the next object in the semi-space). It is also
        // unnecessary there: a zeroed compact field IS its JVM default,
        // because `read_compact_field` decodes by the layout's storage kind
        // (typed zero for a primitive, raw 0 = null for a reference). The
        // collectors' twins (`ZgcRealHeap::alloc_object_with_descriptors`) go
        // through the layout-aware `set_field` for the same reason.
        if cratonvm_types::is_compact_object(self.get_header(obj)) {
            return obj;
        }
        // Initialize EVERY slot with an explicitly-tagged default `Value`.
        //
        // R-niche fix: `read_slot` does a raw `ptr::read::<Value>`, and after
        // `Value::Object` became `Option<ObjectRef>` with a `NonNull` niche,
        // the all-zero bit pattern that `alloc_zeroed` leaves now decodes as
        // `Value::Int(0)` (the discriminant-0 variant), NOT `Value::Object(None)`.
        // A zeroed slot and a slot written with `Value::Int(0)` are therefore
        // bit-indistinguishable, so the decode rule cannot be recovered in
        // `read_slot` alone. We instead make the reference/uninitialized default
        // EXPLICIT here: every primitive slot gets its spec-mandated typed zero,
        // and every reference-typed (`L`/`[`), unknown-descriptor, or
        // descriptor-uncovered slot gets an explicit `Value::Object(None)`
        // (discriminant 4) so a later `get_field` reads back the documented
        // `null` rather than a bogus `Int(0)`.
        for i in 0..num_fields {
            let default = descriptor_bytes
                .get(i)
                .and_then(|&b| default_value_for_descriptor(b))
                .unwrap_or(Value::Object(None));
            // SAFETY: `i < num_fields == header.num_slots()`, so `slot_ptr`
            // lands within the freshly-allocated object's field region.
            unsafe {
                let ptr = slot_ptr(obj, i);
                write_slot(ptr, default);
            }
        }
        obj
    }

    /// Like `alloc_object`, but returns `None` instead of panicking on overflow.
    pub fn alloc_object_checked(&self, class_id: ClassId, num_fields: usize) -> Option<ObjectRef> {
        let compact_total = cratonvm_types::compact_object_total_size(
            self.layout_domain(),
            class_id.as_u32(),
            num_fields,
        );
        let total_size = match compact_total {
            Some(total) => total,
            None => HEADER_SIZE.checked_add(num_fields.checked_mul(SLOT_SIZE)?)?,
        };
        // See `alloc_zeroed` for the NUMA hint rationale.
        let _node = self.refresh_numa_hint();
        let mut from = self.from_space.lock();
        let ptr = from.alloc(total_size, 8)?;
        // SAFETY: same as `alloc_object` — ptr is valid, aligned, with sufficient capacity.
        unsafe {
            std::ptr::write_bytes(ptr, 0, total_size);
            let mut header = ObjectHeader::new(
                class_id,
                ObjectKind::Object,
                ArrayElementType::Reference, // hash installed lazily in the mark word on first request
                0,
                u32::try_from(num_fields).ok()?,
            );
            if let Some(total) = compact_total {
                header.set_compact_shape(num_fields as u32, total);
            }
            std::ptr::write(ptr as *mut ObjectHeader, header);
            Some(ObjectRef::from_raw(ptr))
        }
    }

    /// Allocate a new Java array with the given element type and length, all zeroed.
    ///
    /// Arrays use compact element sizes: 1 byte for boolean/byte, 2 for char/short,
    /// 4 for int/float, 8 for long/double/reference.
    pub fn alloc_array(
        &self,
        class_id: ClassId,
        element_type: ArrayElementType,
        length: usize,
    ) -> ObjectRef {
        assert!(
            length <= MAX_ARRAY_LENGTH,
            "array length {} exceeds maximum {}",
            length,
            MAX_ARRAY_LENGTH
        );
        let data_size =
            array_data_size(length, element_type).expect("array data size overflow in alloc_array");
        // M6 (round-12 gc): make the `+ HEADER_SIZE` add checked too, matching
        // `alloc_array_checked` (which uses `HEADER_SIZE.checked_add(data_size)?`).
        let total_size = HEADER_SIZE
            .checked_add(data_size)
            .expect("array total size overflow in alloc_array");
        let ptr = self.alloc_zeroed(total_size);

        let header = ObjectHeader::new(
            class_id,
            ObjectKind::Array,
            element_type, // hash installed lazily in the mark word on first request
            u32::try_from(length).expect("array length exceeds u32::MAX"),
            u32::try_from(length).expect("array length exceeds u32::MAX"),
        );

        // SAFETY: `ptr` was returned by `alloc_zeroed` — valid, non-null, 8-byte
        // aligned, and has at least `total_size` bytes. The header write is in
        // bounds. The data area (elements) is already zero-initialized by
        // `alloc_zeroed`, which is the correct default for all primitive types
        // (0) and reference types (null pointer = 0).
        unsafe {
            std::ptr::write(ptr as *mut ObjectHeader, header);
            ObjectRef::from_raw(ptr)
        }
    }

    /// Like `alloc_array`, but returns `None` instead of panicking on overflow
    /// or out-of-memory.
    pub fn alloc_array_checked(
        &self,
        class_id: ClassId,
        element_type: ArrayElementType,
        length: usize,
    ) -> Option<ObjectRef> {
        if length > MAX_ARRAY_LENGTH {
            return None;
        }
        let data_size = array_data_size_checked(length, element_type)?;
        let total_size = ARRAY_DATA_OFFSET.checked_add(data_size)?;
        // See `alloc_zeroed` for the NUMA hint rationale.
        let _node = self.refresh_numa_hint();
        let mut from = self.from_space.lock();
        let ptr = from.alloc(total_size, 8)?;
        // SAFETY: same as `alloc_array` — ptr is valid, aligned, with sufficient capacity.
        unsafe {
            std::ptr::write_bytes(ptr, 0, total_size);
            let header = ObjectHeader::new(
                class_id,
                ObjectKind::Array,
                element_type, // hash installed lazily in the mark word on first request
                u32::try_from(length).ok()?,
                u32::try_from(length).ok()?,
            );
            std::ptr::write(ptr as *mut ObjectHeader, header);
            Some(ObjectRef::from_raw(ptr))
        }
    }

    // ----- Header access ---------------------------------------------------

    /// Read the object header from a heap reference.
    ///
    /// # Safety
    /// `obj_ref` must point to a valid heap allocation.
    pub fn get_header(&self, obj_ref: ObjectRef) -> &ObjectHeader {
        // SAFETY: `obj_ref` points to a live heap allocation whose first
        // HEADER_SIZE bytes are a valid `ObjectHeader` written during allocation.
        // The reference is valid for the lifetime of the current semi-space.
        unsafe { &*(obj_ref.as_ptr() as *const ObjectHeader) }
    }

    /// Get the class id of a heap object.
    pub fn class_id_of(&self, obj_ref: ObjectRef) -> ClassId {
        self.get_header(obj_ref).class_id
    }

    /// Get the kind (Object or Array) of a heap allocation.
    pub fn kind_of(&self, obj_ref: ObjectRef) -> ObjectKind {
        self.get_header(obj_ref).kind()
    }

    /// Get the element type of an array object.
    pub fn element_type_of(&self, obj_ref: ObjectRef) -> ArrayElementType {
        self.get_header(obj_ref).element_type()
    }

    /// Get the identity hash code of a heap object.
    pub fn identity_hash_code(&self, obj_ref: ObjectRef) -> i32 {
        let header = self.get_header(obj_ref);
        match header.identity_hash(|| match self.next_hash() {
            0 => i32::MAX,
            h => h,
        }) {
            Ok(hash) => hash,
            Err(()) => {
                // A locked compact instance: its hash lives in its Monitor.
                // Never mint here -- see `collector::displaced_identity_hash`.
                crate::collector::displaced_identity_hash(obj_ref.as_ptr() as usize)
            }
        }
    }

    // ----- Field access (for Objects) --------------------------------------

    /// Get the value of a field at the given index.
    ///
    /// # Panics
    /// Panics if `index >= num_slots`.
    pub fn get_field(&self, obj_ref: ObjectRef, index: usize) -> Value {
        assert!(
            index < self.get_header(obj_ref).num_slots() as usize,
            "field index {} out of bounds (num_slots={})",
            index,
            self.get_header(obj_ref).num_slots()
        );
        // An ARRAY receiver is never a plain-object field access — and the
        // assertion above does not catch it, because an array mirrors its
        // LENGTH into `num_slots`. See `refuse_array_receiver_field_access`.
        {
            let header = self.get_header(obj_ref);
            if header.kind() == ObjectKind::Array {
                // SAFETY: `obj_ref` is a live allocation base and `header` is
                // its header, read one line above.
                unsafe {
                    refuse_array_receiver_field_access(
                        obj_ref.as_ptr() as usize,
                        header,
                        index,
                        "heap::get_field",
                    )
                };
                return Value::Object(None);
            }
        }
        if let Some((disp, storage)) =
            cratonvm_types::compact_object_field_disp(self.get_header(obj_ref), index)
        {
            let ptr = unsafe { obj_ref.as_ptr().add(disp) };
            let v = unsafe {
                cratonvm_types::read_compact_field(
                    ptr,
                    storage,
                    std::sync::atomic::Ordering::Relaxed,
                )
            };
            if !storage.is_reference() {
                return v;
            }
            // Un-box the wrapper `set_field` installs for a non-reference value
            // stored into a declared-REFERENCE slot — the field half of what
            // `get_array_element_unboxing` already does for elements. This
            // accessor used to hand such a value to `write_compact_field`,
            // whose `FieldStorageKind::Reference` arm maps every non-`Object`
            // value to raw 0, so the write was silently dropped to null
            // (W7-84-primitive-in-reference-store.md).
            //
            // gc-common w3-e: screened per SLOT (`unbox_reference_field`), so
            // the boot-time class-mirror overlays no longer open the screen
            // for every other field -- `common-g-autobox-latch-always-armed`.
            let owner = self.get_header(obj_ref).class_id;
            return crate::autobox::unbox_reference_field(
                v,
                owner,
                index,
                |r| {
                    if !self.is_valid_heap_object(r) {
                        return None;
                    }
                    // SAFETY: `is_valid_heap_object` confirmed `r` points to an
                    // 8-byte-aligned address inside one of this heap's arenas,
                    // so reading its `ObjectHeader` is valid memory.
                    Some(unsafe { (*(r.as_ptr() as *const ObjectHeader)).class_id })
                },
                |r| self.get_field(r, 0),
            );
        }
        // HIB-DCAST-LATEPHASE.1 (mutator side), the fourth accessor family.
        // `compact_object_field_storage` answers `None` for TWO reasons and
        // only the first licenses the fall-through below: (1) "this is a
        // legacy object" — its contract, and the uniform 16-byte `Value` cell
        // is the right read; (2) "this IS a compact object (`GC_FLAG_COMPACT`,
        // set at allocation by `alloc_object`, which sized the body with
        // `compact_object_body_size`) whose `(class_id, num_slots)` no longer
        // resolves to a registered layout" — a redefinition that changed the
        // field count, or a foreign `layout_domain`.
        //
        // In case (2) `num_slots()` is the FIELD COUNT, not a count of 16-byte
        // cells, so the `index < num_slots` assert above does NOT bound
        // `HEADER_SIZE + index * SLOT_SIZE` and an entirely in-range index
        // still reads past the allocation.
        //
        // `g1::get_field`, `gen_heap::get_field` and `ZgcRealHeap::get_field`
        // all carry this guard; this accessor was the one that did not. Degrade
        // exactly as they do (benign null, loud `cratonvm::gc::guard` record,
        // no panic — a racing redefinition must not abort the JVM).
        if cratonvm_types::is_compact_object(self.get_header(obj_ref)) {
            tracing::warn!(
                target: "cratonvm::gc::guard",
                obj = ?obj_ref.as_ptr(),
                index,
                class_id = ?self.get_header(obj_ref).class_id,
                "heap::get_field: compact receiver has no registered layout for \
                 its (class_id, field_count) — returning null rather than \
                 striding its packed compact body as legacy 16-byte cells \
                 (HIB-DCAST-LATEPHASE.1)",
            );
            return Value::Object(None);
        }
        // SAFETY: `obj_ref` is a live heap object. The assert above
        // confirms `index < num_slots`, and the guard above confirms the
        // object is NOT compact, so its body really is `num_slots` uniform
        // 16-byte cells. `slot_ptr` computes
        // `obj_ref + HEADER_SIZE + index * SLOT_SIZE`, which is within the
        // allocated block. `read_slot` reads a `Value` from that pointer.
        unsafe {
            let ptr = slot_ptr(obj_ref, index);
            read_slot(ptr)
        }
    }

    /// Set the value of a field at the given index.
    ///
    /// # Panics
    /// Panics if `index >= num_slots`.
    pub fn set_field(&self, obj_ref: ObjectRef, index: usize, value: Value) {
        assert!(
            index < self.get_header(obj_ref).num_slots() as usize,
            "field index {} out of bounds (num_slots={})",
            index,
            self.get_header(obj_ref).num_slots()
        );
        // Symmetric with `get_field`: a 16-byte cell stamped over packed
        // element data corrupts the elements that share those bytes, and past
        // the first few indices lands outside the allocation altogether.
        {
            let header = self.get_header(obj_ref);
            if header.kind() == ObjectKind::Array {
                // SAFETY: `obj_ref` is a live allocation base and `header` is
                // its header, read one line above.
                unsafe {
                    refuse_array_receiver_field_access(
                        obj_ref.as_ptr() as usize,
                        header,
                        index,
                        "heap::set_field",
                    )
                };
                return;
            }
        }
        if let Some((disp, storage)) =
            cratonvm_types::compact_object_field_disp(self.get_header(obj_ref), index)
        {
            // A non-reference value into a declared-REFERENCE slot: box it,
            // rather than let `write_compact_field`'s `Reference` arm map it to
            // raw 0 and drop the write to null. This is the field half of what
            // `set_array_element` below has always done for elements
            // (W7-84-primitive-in-reference-store.md).
            let value = if storage.is_reference() {
                let class_id = self.get_header(obj_ref).class_id;
                crate::autobox::box_for_reference_slot(value, class_id, index, |v| {
                    let wrapper = self.alloc_object(AUTOBOX_CLASS_ID, 1);
                    self.set_field(wrapper, 0, v);
                    wrapper
                })
            } else {
                value
            };
            // (This heap's `alloc_object` never collects or moves anything, so
            // `obj_ref` and `disp` stay valid across the boxing closure.)
            let ptr = unsafe { obj_ref.as_ptr().add(disp) };
            unsafe {
                cratonvm_types::write_compact_field(
                    ptr,
                    storage,
                    value,
                    std::sync::atomic::Ordering::Relaxed,
                )
            };
            return;
        }
        // HIB-DCAST-LATEPHASE.1, write half — see the long note on the matching
        // guard in `get_field`. This half is the more damaging of the two: the
        // legacy stride does not merely read past a compact-sized body, it
        // *writes* a 16-byte `Value` cell over whatever follows the object.
        if cratonvm_types::is_compact_object(self.get_header(obj_ref)) {
            tracing::warn!(
                target: "cratonvm::gc::guard",
                obj = ?obj_ref.as_ptr(),
                index,
                class_id = ?self.get_header(obj_ref).class_id,
                value = ?value,
                "heap::set_field: compact receiver has no registered layout for \
                 its (class_id, field_count) — dropping the write rather than \
                 striding its packed compact body as legacy 16-byte cells \
                 (HIB-DCAST-LATEPHASE.1)",
            );
            return;
        }
        // SAFETY: same invariant as `get_field` — index is within bounds, the
        // object is not compact, and the slot pointer is within the allocated
        // object block.
        unsafe {
            let ptr = slot_ptr(obj_ref, index);
            write_slot(ptr, value);
        }
    }

    // ----- Volatile field access (for ACC_VOLATILE fields) -----------------

    /// Get the value of a volatile field at the given index.
    ///
    /// JLS §17.7 requires volatile long/double (and any volatile-declared
    /// field) reads to be atomic. The on-heap `Value` slot is 16 bytes,
    /// wider than any stable Rust atomic on x86-64, so SeqCst fences alone
    /// give the JMM ordering edge but not 16-byte slot atomicity — a
    /// concurrent writer mid-store would expose a torn (tag, payload)
    /// pair to this read.
    ///
    /// We acquire the per-slot stripe lock from
    /// [`crate::collector::volatile_stripe_lock`] so paired
    /// `set_field_volatile` writers serialize against this read; the
    /// 16-byte `Value` therefore appears either fully old or fully new.
    pub fn get_field_volatile(&self, obj_ref: ObjectRef, index: usize) -> Value {
        let _guard = crate::collector::volatile_stripe_lock(obj_ref, index);
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
        let val = self.get_field(obj_ref, index);
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
        val
    }

    /// Set the value of a volatile field at the given index.
    ///
    /// Acquires the per-slot stripe lock so concurrent volatile readers
    /// observe a fully-old or fully-new 16-byte `Value`. SeqCst fences
    /// provide the JMM happens-before edge. See [`Self::get_field_volatile`]
    /// for the full rationale.
    pub fn set_field_volatile(&self, obj_ref: ObjectRef, index: usize, value: Value) {
        let _guard = crate::collector::volatile_stripe_lock(obj_ref, index);
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
        self.set_field(obj_ref, index, value);
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
    }

    // ----- T10.9.E descriptor-aware field access --------------------------

    /// Get the value of a field at the given index, **normalized to the
    /// declared field type** from its descriptor byte.
    ///
    /// This is the T10.9.E defense-in-depth layer: even if an upstream
    /// putfield wrote a drifted `Value` variant (e.g. a `Double` whose
    /// bit pattern happens to match a long, or an `Object(None)` in a
    /// long slot that slipped past the `alloc_object_with_descriptors`
    /// zero-init), reading through this API returns a `Value` whose
    /// variant matches the class-file descriptor for the field.
    ///
    /// Descriptor byte semantics (matches CompactValue::decode_by_descriptor):
    /// - `b'J'` → `Value::Long` (bit reinterpret if needed)
    /// - `b'D'` → `Value::Double`
    /// - `b'F'` → `Value::Float`
    /// - `b'I' | b'B' | b'C' | b'S' | b'Z'` → `Value::Int`
    /// - `b'L' | b'['` → `Value::Object(Some/None)` (unchanged from raw read)
    /// - other → raw `get_field` return (legacy behavior)
    ///
    /// Out-of-bounds behavior matches `get_field` (panics on bad index).
    pub fn get_field_as(&self, obj_ref: ObjectRef, index: usize, desc_byte: u8) -> Value {
        let raw = self.get_field(obj_ref, index);
        coerce_field_value_for_slot(
            raw,
            desc_byte,
            FieldCoercionSite::read(Some(self.class_id_of(obj_ref)), index),
        )
    }

    /// Volatile variant of [`get_field_as`].
    pub fn get_field_volatile_as(&self, obj_ref: ObjectRef, index: usize, desc_byte: u8) -> Value {
        let raw = self.get_field_volatile(obj_ref, index);
        coerce_field_value_for_slot(
            raw,
            desc_byte,
            FieldCoercionSite::read(Some(self.class_id_of(obj_ref)), index),
        )
    }

    /// Set a field value, coercing to the declared type from the descriptor.
    ///
    /// Symmetric to [`get_field_as`] — if a `Value::Double` lands where the
    /// class file declares `J`, it is reinterpreted as `Value::Long` before
    /// the slot write so downstream readers always see a consistent tag.
    ///
    /// G30: the coercion is the provenance-carrying
    /// [`coerce_field_value_for_slot`], so a value-destroying store made
    /// through THIS heap names its class and slot in the warning. The three
    /// collectors on the live `VmHeap` dispatch path do the same (their
    /// trait defaults pass a `FieldCoercionSite` since G45; G30 NOMINATION 1
    /// is taken).
    pub fn set_field_as(&self, obj_ref: ObjectRef, index: usize, value: Value, desc_byte: u8) {
        let coerced = coerce_field_value_for_slot(
            value,
            desc_byte,
            FieldCoercionSite::store(Some(self.class_id_of(obj_ref)), index),
        );
        self.set_field(obj_ref, index, coerced);
    }

    /// Volatile variant of [`set_field_as`].
    pub fn set_field_volatile_as(
        &self,
        obj_ref: ObjectRef,
        index: usize,
        value: Value,
        desc_byte: u8,
    ) {
        let coerced = coerce_field_value_for_slot(
            value,
            desc_byte,
            FieldCoercionSite::store(Some(self.class_id_of(obj_ref)), index),
        );
        self.set_field_volatile(obj_ref, index, coerced);
    }

    // ----- Array access ----------------------------------------------------

    /// Get the length of an array.
    ///
    /// # Panics
    /// Panics if the object is not an array.
    pub fn array_length(&self, obj_ref: ObjectRef) -> usize {
        let header = self.get_header(obj_ref);
        assert_eq!(header.kind(), ObjectKind::Array, "not an array");
        header.array_length() as usize
    }

    /// Get an array element at the given index.
    ///
    /// Returns `Err` with the index if out of bounds.
    pub fn get_array_element(&self, obj_ref: ObjectRef, index: usize) -> Result<Value, i32> {
        let header = self.get_header(obj_ref);
        assert_eq!(header.kind(), ObjectKind::Array, "not an array");
        if index >= header.array_length() as usize {
            return Err(crate::heap::oob_index_code(index));
        }
        // SAFETY: bounds check passed above (`index < array_length`).
        // `obj_ref + HEADER_SIZE` is the start of the data area.
        // `read_prim_element` reads `element_byte_size(et)` bytes at
        // `base + index * element_byte_size(et)`, which is within the
        // allocated data area of size `array_data_size(length, et)`.
        unsafe {
            let base = obj_ref.as_ptr().add(ARRAY_DATA_OFFSET);
            Ok(read_prim_element(base, index, header.element_type()))
        }
    }

    /// Like `get_array_element`, but auto-unboxes values stored by native
    /// collections. When a non-Object value (Int, Long, etc.) was auto-boxed
    /// into a wrapper object during `set_array_element`, this method
    /// transparently returns the original primitive value.
    ///
    /// Use this from NativeContext (for collection code). The interpreter's
    /// bytecode aaload should use plain `get_array_element` — Java-level
    /// Object[] arrays never contain auto-boxed wrappers.
    pub fn get_array_element_unboxing(
        &self,
        obj_ref: ObjectRef,
        index: usize,
    ) -> Result<Value, i32> {
        let header = self.get_header(obj_ref);
        assert_eq!(header.kind(), ObjectKind::Array, "not an array");
        if index >= header.array_length() as usize {
            return Err(crate::heap::oob_index_code(index));
        }
        // SAFETY: bounds check passed above. Same invariant as `get_array_element`.
        let value = unsafe {
            let base = obj_ref.as_ptr().add(ARRAY_DATA_OFFSET);
            read_prim_element(base, index, header.element_type())
        };
        if header.element_type() == ArrayElementType::Reference {
            if let Value::Object(Some(obj)) = value {
                // The stored word is treated as an `ObjectRef`, but a stale or
                // garbage non-zero element could point anywhere. Validate it
                // against this heap's semi-space arenas before dereferencing
                // it as an `ObjectHeader`); otherwise a wild read can crash or
                // mis-classify garbage. If the pointer does not look like a
                // live heap object, skip the unboxing and return the value.
                if self.is_valid_heap_object(obj) {
                    // SAFETY: `is_valid_heap_object` confirmed `obj` points to
                    // an 8-byte-aligned address inside one of this heap's
                    // arenas, so reading its `ObjectHeader` is valid memory.
                    let obj_header = unsafe { &*(obj.as_ptr() as *const ObjectHeader) };
                    if obj_header.class_id == AUTOBOX_CLASS_ID {
                        return Ok(self.get_field(obj, 0));
                    }
                }
            }
        }
        Ok(value)
    }

    /// Conservative validity check for a heap object pointer.
    ///
    /// Returns `true` only if `obj` is 8-byte aligned and falls within one of
    /// this heap's semi-space arenas (`from_space` or `to_space`). Mirrors the
    /// `GenerationalHeap::is_object_address` check used by `gen_heap.rs`.
    ///
    /// This is a structural guard: it lets callers reject a stale or garbage
    /// stored pointer before dereferencing it as an `ObjectHeader`.
    ///
    /// LOCK-FREE: membership is tested against the `(base, len)` pair of each
    /// semi-space captured at construction (`Heap::space_ranges`). The buffers
    /// never move and `swap_spaces` exchanges whole arenas, so the union of the
    /// two ranges never changes and no arena mutex is needed. (An earlier
    /// version used `try_lock` and answered `false` on any contention, which
    /// made a concurrent allocation turn a valid boxed reference into the
    /// `AUTOBOX_CLASS_ID` wrapper: a nondeterministic wrong value.)
    fn is_valid_heap_object(&self, obj: ObjectRef) -> bool {
        let addr = obj.as_ptr() as usize;
        // Object headers are always 8-byte aligned; a real object pointer
        // never has its low 3 bits set.
        if addr == 0 || addr & 0x7 != 0 {
            return false;
        }
        self.space_ranges
            .iter()
            .any(|&(base, len)| addr >= base && addr - base < len)
    }

    /// Fallible twin of [`set_field`](Self::set_field) for a store that may BOX a
    /// primitive into a compact reference slot: `set_field`'s boxing closure
    /// uses the aborting `alloc_object`, where `set_array_element` reports
    /// [`ARRAY_STORE_OUT_OF_MEMORY`]. Returns `true` when the store happened
    /// exactly as `set_field` would have made it, `false` when a wrapper was
    /// needed and the heap could not hold it (the slot then holds null). Same
    /// shape as `VmHeap::try_set_field`.
    pub fn try_set_field(&self, obj_ref: ObjectRef, index: usize, value: Value) -> bool {
        if !crate::autobox::needs_reference_box(value) {
            self.set_field(obj_ref, index, value);
            return true;
        }
        // SAFETY: `self` outlives `scope`: both live in this frame, and the
        // scope is dropped before this function returns.
        let scope = unsafe {
            crate::autobox::arm_fallible_wrapper(
                Self::try_alloc_wrapper_erased,
                (self as *const Self).cast::<()>(),
            )
        };
        self.set_field(obj_ref, index, value);
        let stored = !scope.declined();
        drop(scope);
        stored
    }

    /// The [`crate::autobox::TryAllocWrapper`] of [`Self::try_set_field`].
    fn try_alloc_wrapper_erased(heap: *const (), value: Value) -> Option<ObjectRef> {
        // SAFETY: armed only by `try_set_field` with `self`, which outlives the
        // scope this runs inside.
        let heap = unsafe { &*heap.cast::<Heap>() };
        let wrapper = heap.try_alloc_object(AUTOBOX_CLASS_ID, 1)?;
        heap.set_field(wrapper, 0, value);
        Some(wrapper)
    }

    /// Set an array element at the given index.
    ///
    /// Returns `Err` with the index if out of bounds.
    pub fn set_array_element(
        &self,
        obj_ref: ObjectRef,
        index: usize,
        value: Value,
    ) -> Result<(), i32> {
        let header = self.get_header(obj_ref);
        assert_eq!(header.kind(), ObjectKind::Array, "not an array");
        if index >= header.array_length() as usize {
            return Err(crate::heap::oob_index_code(index));
        }
        // SAFETY: bounds check passed above. Same invariant as `get_array_element`.
        // For reference arrays, non-Object values are auto-boxed into wrapper
        // objects allocated on this heap before being stored.
        unsafe {
            let base = obj_ref.as_ptr().add(ARRAY_DATA_OFFSET);
            // Compact ref arrays only store 8-byte pointers. If a non-Object
            // value is written (e.g. Value::Int from a native collection),
            // auto-box it into a 1-field wrapper object.
            if header.element_type() == ArrayElementType::Reference {
                match value {
                    Value::Object(_) => {
                        write_prim_element(base, index, header.element_type(), value);
                    }
                    _ => {
                        // Fallible: `alloc_object` aborts the process when the
                        // heap is full, and a heap-full auto-box is an
                        // `OutOfMemoryError` (`ARRAY_STORE_OUT_OF_MEMORY`).
                        let Some(wrapper) = self.try_alloc_object(AUTOBOX_CLASS_ID, 1) else {
                            return Err(ARRAY_STORE_OUT_OF_MEMORY);
                        };
                        self.set_field(wrapper, 0, value);
                        // Arm the process-wide wrapper latch — see the matching
                        // note in `GenerationalHeap::set_array_element` and
                        // `crate::autobox`.
                        crate::autobox::note_wrapper_created();
                        write_prim_element(
                            base,
                            index,
                            header.element_type(),
                            Value::Object(Some(wrapper)),
                        );
                    }
                }
            } else {
                write_prim_element(base, index, header.element_type(), value);
            }
        }
        Ok(())
    }

    // ----- Write barrier (no-op for non-generational heap) --------------------

    /// Write barrier — called after every reference store into a heap object.
    ///
    /// For the simple semi-space heap this is a no-op. The generational heap
    /// uses this to mark card table entries dirty.
    #[inline]
    pub fn write_barrier(&self, _obj: ObjectRef, _stored_value: Value) {
        // No-op for non-generational heap.
    }

    // ----- GC-related queries -----------------------------------------------

    /// Returns true when from-space usage exceeds the GC threshold.
    pub fn needs_gc(&self) -> bool {
        self.from_space.lock().used() >= self.gc_threshold
    }

    /// Lock and return mutable references to both semi-spaces.
    /// Used by the GC collector.
    pub fn lock_spaces(
        &self,
    ) -> (
        parking_lot::MutexGuard<'_, Arena>,
        parking_lot::MutexGuard<'_, Arena>,
    ) {
        let from = self.from_space.lock();
        let to = self.to_space.lock();
        (from, to)
    }

    /// Swap from-space and to-space after GC collection.
    pub fn swap_spaces(&self) {
        let mut from = self.from_space.lock();
        let mut to = self.to_space.lock();
        std::mem::swap(&mut *from, &mut *to);
    }

    /// Run a full garbage collection cycle.
    ///
    /// 1. Copies all live objects (reachable from roots) to to-space
    /// 2. Remaps monitor table keys
    /// 3. Swaps from-space and to-space
    ///
    /// After this call, `roots` contains updated ObjectRefs pointing to the
    /// new object locations, and the returned `GcResult` contains the pointer
    /// mapping for updating external references.
    ///
    /// The `_stw` parameter is type-level proof that the caller is in a
    /// stop-the-world phase — see [`crate::collector::StopTheWorldToken`].
    pub fn collect_garbage(
        &self,
        _stw: &crate::collector::StopTheWorldToken,
        roots: &mut [ObjectRef],
        monitors: &dyn crate::collector::MonitorCleanup,
    ) -> crate::gc::GcResult {
        // Part F: take the counter for the whole collection. The wait and the
        // claim are one `compare_exchange(0, COLLECTING)`, so a token taken
        // after the count read zero backs off in `SafepointToken::new` instead
        // of overlapping the collection. Gated to keep the default feature
        // build byte-identical to before.
        #[cfg(feature = "gpu-offload")]
        let _collecting = self.begin_gpu_collecting();

        let mut from = self.from_space.lock();
        let mut to = self.to_space.lock();

        // (JNI critical sections no longer pin: the JNI layer hands native code
        // a detached copy of the array body, so there is no JNI keep-alive set
        // to splice here. The process-global `pinned` module table that used to
        // be spliced in was deleted in gc-common w4-b.)

        // Part F: walk GPU pinned refs as additional roots. We splice them onto
        // a combined buffer, run the collector, then copy the updated
        // ObjectRefs back into both the caller's slice and the pinned-refs
        // set so the next pin/unpin sees post-GC addresses. The pin-set lock is
        // held from the snapshot to the rewrite, so a concurrent `pin_ref` /
        // `unpin_ref` cannot be lost by the rewrite (lock order: arenas, then
        // the pin set; `pin_ref` / `unpin_ref` take only the pin set).
        #[cfg(feature = "gpu-offload")]
        let result = {
            let mut pin_guard = self.gpu_pinned_refs.lock();
            let pinned_snapshot: Vec<ObjectRef> = pin_guard.iter().copied().collect();

            if pinned_snapshot.is_empty() {
                crate::gc::collect(&mut from, &mut to, roots)
            } else {
                let caller_len = roots.len();
                let gpu_len = pinned_snapshot.len();
                let mut combined: Vec<ObjectRef> = Vec::with_capacity(caller_len + gpu_len);
                combined.extend_from_slice(roots);
                combined.extend_from_slice(&pinned_snapshot);

                let result = crate::gc::collect(&mut from, &mut to, &mut combined);

                // Copy the (possibly-updated) caller roots back.
                roots.copy_from_slice(&combined[..caller_len]);

                // Rewrite the GPU pinned-refs set with their new addresses.
                let new_pinned = &combined[caller_len..caller_len + gpu_len];
                pin_guard.clear();
                for r in new_pinned {
                    pin_guard.insert(*r);
                }

                result
            }
        };

        #[cfg(not(feature = "gpu-offload"))]
        let result = crate::gc::collect(&mut from, &mut to, roots);

        // Remap monitor table keys using the pointer mapping
        monitors.remap_after_gc(&result.pointer_map);

        // Swap spaces: to-space (now containing live data) becomes from-space
        std::mem::swap(&mut *from, &mut *to);

        result
    }

    /// Like [`collect_garbage`] but keeps dead finalizable objects alive so
    /// their `finalize()` method can be invoked.  Returns the GC result and
    /// the *new* (post-GC) addresses of dead finalizable objects.
    ///
    /// The `_stw` parameter is type-level proof that the caller is in a
    /// stop-the-world phase — see [`crate::collector::StopTheWorldToken`].
    pub fn collect_garbage_with_finalizers(
        &self,
        _stw: &crate::collector::StopTheWorldToken,
        roots: &mut [ObjectRef],
        finalizer_addrs: &[usize],
        monitors: &dyn crate::collector::MonitorCleanup,
    ) -> (crate::gc::GcResult, Vec<usize>) {
        // Part F: see `collect_garbage` for the rationale on both gated blocks.
        #[cfg(feature = "gpu-offload")]
        let _collecting = self.begin_gpu_collecting();

        let mut from = self.from_space.lock();
        let mut to = self.to_space.lock();

        #[cfg(feature = "gpu-offload")]
        let (result, dead_finalizers) = {
            let mut pin_guard = self.gpu_pinned_refs.lock();
            let pinned_snapshot: Vec<ObjectRef> = pin_guard.iter().copied().collect();

            if pinned_snapshot.is_empty() {
                crate::gc::collect_with_finalizers(&mut from, &mut to, roots, finalizer_addrs)
            } else {
                let caller_len = roots.len();
                let gpu_len = pinned_snapshot.len();
                let mut combined: Vec<ObjectRef> = Vec::with_capacity(caller_len + gpu_len);
                combined.extend_from_slice(roots);
                combined.extend_from_slice(&pinned_snapshot);

                let (result, dead_finalizers) = crate::gc::collect_with_finalizers(
                    &mut from,
                    &mut to,
                    &mut combined,
                    finalizer_addrs,
                );

                roots.copy_from_slice(&combined[..caller_len]);

                let new_pinned = &combined[caller_len..caller_len + gpu_len];
                pin_guard.clear();
                for r in new_pinned {
                    pin_guard.insert(*r);
                }

                (result, dead_finalizers)
            }
        };

        #[cfg(not(feature = "gpu-offload"))]
        let (result, dead_finalizers) =
            crate::gc::collect_with_finalizers(&mut from, &mut to, roots, finalizer_addrs);

        monitors.remap_after_gc(&result.pointer_map);
        std::mem::swap(&mut *from, &mut *to);

        (result, dead_finalizers)
    }

    // ----- Internal --------------------------------------------------------

    /// Allocate `size` bytes, 8-byte aligned, zero-initialized.
    /// Locks the from-space mutex for the duration of the allocation.
    ///
    /// # Panics
    /// Panics on OOM as a last resort. In normal operation the interpreter
    /// calls [`try_alloc_zeroed`](Self::try_alloc_zeroed) first, triggers GC
    /// on failure, and only falls back to this method when the object *must*
    /// be allocated (e.g. internal bookkeeping). If this panic fires it means
    /// the heap is genuinely exhausted after GC has already been attempted.
    fn alloc_zeroed(&self, size: usize) -> *mut u8 {
        // NUMA hint refresh — stub for the planned per-node arena
        // dispatch. On single-node hosts this is a no-op; on multi-node
        // hosts it records the calling thread's preferred node so
        // observers/tests can confirm dispatch intent. See the
        // `TODO(NUMA, multi-arena)` on `Heap` for the full plan.
        let _node = self.refresh_numa_hint();
        let mut from = self.from_space.lock();
        let ptr = from.alloc(size, 8).unwrap_or_else(|| {
            // Use process::abort() instead of panic!() to avoid unwinding
            // through unsafe code. In production, GenerationalHeap's try_alloc
            // path handles OOM gracefully; this code path is only reached by
            // the legacy Heap (used in tests).
            eprintln!(
                "heap out of memory: tried to allocate {} bytes, from-space has {}/{} used",
                size,
                from.used(),
                from.capacity(),
            );
            std::process::abort();
        });
        // SAFETY: `ptr` was returned by `Arena::alloc` and points to `size` bytes
        // of uninitialized (or stale) memory within the arena. Writing zeros is
        // safe because the pointer and size are guaranteed valid by the arena.
        unsafe {
            std::ptr::write_bytes(ptr, 0, size);
        }
        ptr
    }

    /// Try to allocate `size` bytes, returning `None` if from-space is full.
    fn try_alloc_zeroed(&self, size: usize) -> Option<*mut u8> {
        // See `alloc_zeroed` for the NUMA hint rationale.
        let _node = self.refresh_numa_hint();
        let mut from = self.from_space.lock();
        let ptr = from.alloc(size, 8)?;
        // SAFETY: `ptr` was returned by `Arena::alloc` and points to `size` bytes
        // of uninitialized memory within the arena. Writing zeros is safe because
        // the pointer and size are guaranteed valid by the arena.
        unsafe {
            std::ptr::write_bytes(ptr, 0, size);
        }
        Some(ptr)
    }

    /// Try to allocate a new Java object, returning `None` if the heap is full.
    /// Callers should trigger GC and retry, or throw `OutOfMemoryError`.
    pub fn try_alloc_object(&self, class_id: ClassId, num_fields: usize) -> Option<ObjectRef> {
        // gc-common w4-e: plan the compact body exactly as `alloc_object` and
        // `alloc_object_checked` do. This entry was legacy-only, so one class
        // had two shapes on this heap depending on which allocator served it --
        // the semispace twin of `common-w2d-g1-zgc-fallible-object-path-never-plans-compact`.
        let compact_total = cratonvm_types::compact_object_total_size(
            self.layout_domain(),
            class_id.as_u32(),
            num_fields,
        );
        let total_size = match compact_total {
            Some(total) => total,
            None => HEADER_SIZE.checked_add(num_fields.checked_mul(SLOT_SIZE)?)?,
        };
        let ptr = self.try_alloc_zeroed(total_size)?;

        let mut header = ObjectHeader::new(
            class_id,
            ObjectKind::Object,
            ArrayElementType::Reference, // hash installed lazily in the mark word on first request
            0,
            u32::try_from(num_fields).ok()?,
        );
        if let Some(total) = compact_total {
            header.set_compact_shape(num_fields as u32, total);
        }

        // SAFETY: `ptr` was just returned by `try_alloc_zeroed`, which guarantees
        // it is valid, non-null, properly aligned (8-byte), and has at least
        // `total_size` bytes available.
        unsafe {
            std::ptr::write(ptr as *mut ObjectHeader, header);
            Some(ObjectRef::from_raw(ptr))
        }
    }

    /// Try to allocate a new Java array, returning `None` if the heap is full.
    /// Callers should trigger GC and retry, or throw `OutOfMemoryError`.
    pub fn try_alloc_array(
        &self,
        class_id: ClassId,
        element_type: ArrayElementType,
        length: usize,
    ) -> Option<ObjectRef> {
        if length > MAX_ARRAY_LENGTH {
            return None;
        }
        let data_size = array_data_size_checked(length, element_type)?;
        let total_size = ARRAY_DATA_OFFSET.checked_add(data_size)?;
        let ptr = self.try_alloc_zeroed(total_size)?;

        let header = ObjectHeader::new(
            class_id,
            ObjectKind::Array,
            element_type, // hash installed lazily in the mark word on first request
            u32::try_from(length).ok()?,
            u32::try_from(length).ok()?,
        );

        // SAFETY: `ptr` was returned by `try_alloc_zeroed` — valid, non-null,
        // 8-byte aligned, and has at least `total_size` bytes.
        unsafe {
            std::ptr::write(ptr as *mut ObjectHeader, header);
            Some(ObjectRef::from_raw(ptr))
        }
    }

    /// Generate the next identity hash code.
    fn next_hash(&self) -> i32 {
        self.next_hash_code.fetch_add(1, Ordering::Relaxed)
    }

    /// Total bytes currently allocated in from-space.
    pub fn allocated_bytes(&self) -> usize {
        self.from_space.lock().used()
    }

    /// The capacity of each semi-space.
    pub fn semi_space_capacity(&self) -> usize {
        self.from_space.lock().capacity()
    }

    // ----- GPU-offload coordination (Part F) ---------------------------------

    /// Enter a "GPU critical section": acquire a
    /// [`SafepointToken`](crate::safepoint::SafepointToken). While at least
    /// one token is alive on this heap, calls to [`Heap::collect_garbage`]
    /// and [`Heap::collect_garbage_with_finalizers`] spin-yield instead of
    /// running a GC cycle. Drop the token to release the GC.
    ///
    /// Available only with the `gpu-offload` Cargo feature. See
    /// [`crate::safepoint`] for the design rationale.
    #[cfg(feature = "gpu-offload")]
    pub fn enter_gpu_critical(&self) -> crate::safepoint::SafepointToken<'_> {
        crate::safepoint::SafepointToken::new(&self.gpu_critical_count)
    }

    /// Number of [`SafepointToken`](crate::safepoint::SafepointToken)s
    /// currently alive on this heap. Useful for tests.
    #[cfg(feature = "gpu-offload")]
    pub fn gpu_critical_count(&self) -> u32 {
        self.gpu_critical_count
            .load(std::sync::atomic::Ordering::Acquire)
            & !crate::safepoint::GPU_COLLECTING
    }

    /// Pin `obj` so the GC walks it as an additional root for as long as
    /// the pin is in effect. Idempotent.
    ///
    /// Callers must balance every `pin_ref` with an `unpin_ref`. The
    /// canonical pattern is "pin around marshalling, unpin after the
    /// kernel result is read back into the heap".
    #[cfg(feature = "gpu-offload")]
    pub fn pin_ref(&self, obj: ObjectRef) {
        self.gpu_pinned_refs.lock().insert(obj);
    }

    /// Forget a previously pinned `obj`. No-op if the ref was not pinned.
    #[cfg(feature = "gpu-offload")]
    pub fn unpin_ref(&self, obj: ObjectRef) {
        self.gpu_pinned_refs.lock().remove(&obj);
    }

    /// Snapshot the current pinned-ref set. Mainly for tests; the GC path
    /// reads through the mutex directly.
    #[cfg(feature = "gpu-offload")]
    pub fn gpu_pinned_refs_snapshot(&self) -> Vec<ObjectRef> {
        self.gpu_pinned_refs.lock().iter().copied().collect()
    }

    /// Number of times the GC saw a non-zero `gpu_critical_count` and
    /// stepped aside. Exposed for tests.
    #[cfg(feature = "gpu-offload")]
    pub fn gpu_blocked_gc_count(&self) -> u64 {
        self.gpu_blocked_gc_count
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Block until no GPU critical section is in flight, and claim the counter
    /// for the whole collection.
    ///
    /// **This is a yield-spin, and it is no longer "the crate-standard
    /// pattern".** Until 2026-09-20 this comment cited
    /// `reference.rs::remove_timeout` as the pattern being followed;
    /// `remove_timeout` has since been rewritten to `std::thread::sleep` on a
    /// 1 ms tick (`gc/src/reference.rs:216-235`, "long enough that a 60 s wait
    /// costs ~60_000 wakeups rather than a core"), so the citation named a
    /// function that does the opposite of what the sentence claimed. The loop
    /// still calls `std::thread::yield_now()`, i.e. it occupies a core
    /// for as long as the critical section lasts — up to
    /// [`crate::safepoint::GPU_CRITICAL_DEADLINE_SECS`] and beyond, since the
    /// deadline only triggers a warning. Whether it should adopt the sleep tick
    /// is a real question; what is fixed here is only the false claim that it
    /// already had.
    ///
    /// The wait and the claim are ONE `compare_exchange(0, COLLECTING)`
    /// ([`crate::safepoint::GPU_COLLECTING`]), so a token taken after the count
    /// read zero cannot overlap the collection: `SafepointToken::new` backs off
    /// until the returned guard drops. After
    /// [`crate::safepoint::GPU_CRITICAL_DEADLINE_SECS`] seconds we log a
    /// single `tracing::warn!` and keep waiting — we NEVER force a
    /// collection while a token is alive.
    ///
    /// Each call also increments [`Heap::gpu_blocked_gc_count`] iff a token
    /// was alive on entry, so callers can attribute blocked cycles in tests.
    #[cfg(feature = "gpu-offload")]
    fn begin_gpu_collecting(&self) -> crate::safepoint::CollectingGuard<'_> {
        use std::sync::atomic::Ordering;

        crate::safepoint::CollectingGuard::acquire(&self.gpu_critical_count, || {
            // One "GC asked, GPU said no" event per call (not per yield loop),
            // so the test-visible counter matches attempted collections.
            self.gpu_blocked_gc_count.fetch_add(1, Ordering::AcqRel);
        })
    }
}

impl Default for Heap {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Heap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let from = self.from_space.lock();
        let to = self.to_space.lock();
        f.debug_struct("Heap")
            .field("from_space_used", &from.used())
            .field("from_space_capacity", &from.capacity())
            .field("to_space_used", &to.used())
            .field("to_space_capacity", &to.capacity())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Slot read/write helpers
// ---------------------------------------------------------------------------

/// Map a JVM field descriptor's first byte to its default [`Value`] per the
/// JVM spec §2.3 (primitive default values).
///
/// | Descriptor | Java type           | Default `Value` variant |
/// |------------|---------------------|-------------------------|
/// | `B`        | byte                | `Int(0)` (sign-extended) |
/// | `C`        | char                | `Int(0)` (zero-extended) |
/// | `I`        | int                 | `Int(0)`                |
/// | `S`        | short               | `Int(0)` (sign-extended) |
/// | `Z`        | boolean             | `Int(0)` (0 = false)    |
/// | `J`        | long                | `Long(0)`               |
/// | `F`        | float               | `Float(0.0)`            |
/// | `D`        | double              | `Double(0.0)`           |
/// | `L…;` / `[`| reference / array   | `None` → keep zero bits |
/// | *other*    | malformed           | `None` → keep zero bits |
///
/// For reference-typed fields the caller should rely on the zeroed memory
/// (which decodes as `Value::Object(None)`) — returning `None` here signals
/// "no override needed". This also fail-opens for unknown descriptor bytes,
/// preserving the pre-fix behavior so malformed class metadata cannot
/// regress non-primitive paths.
#[inline]
pub fn default_value_for_descriptor(desc_byte: u8) -> Option<Value> {
    match desc_byte {
        // Integer-family primitives all live in Value::Int per JVM spec.
        b'I' | b'B' | b'C' | b'S' | b'Z' => Some(Value::Int(0)),
        b'J' => Some(Value::Long(0)),
        b'F' => Some(Value::Float(0.0)),
        b'D' => Some(Value::Double(0.0)),
        // References (L...;) and arrays ([) default to null, which matches
        // the zero-bits decoding of Value::Object(None).
        _ => None,
    }
}

// ----- G30: the descriptor-coercion loss instrument -----------------------
//
// Why this exists, in one paragraph, because the next reader will otherwise
// re-derive it from a symptom the way three records already had to.
//
// `coerce_field_value_by_descriptor` has two jobs that look alike and are not.
// Most of its arms NORMALISE: a `Double` bit pattern that was meant as a long
// is reinterpreted, an `Int` is widened to a `Long`, and no information is
// lost. Three arms DESTROY: a primitive handed to an `L`/`[` slot becomes
// `null`, a `null` handed to a primitive slot becomes the typed zero, and an
// object POINTER handed to a primitive slot becomes its own address as a
// number. Until 2026-08-17 all three were silent in release, and the only
// detector that named the species — `overlay_check_access` in
// `vm/src/vm/vm_exec.rs` — is gated behind `CRATONVM_DBG_OVERLAY` AND
// requires the class to carry a fabricated shadow layout, so it cannot see a
// class this VM never modelled.
//
// This is NOT the `cratonvm::gc::guard` W7-84 warning and must not be counted
// with it. MEASURED 2026-08-17, `RJdkHello` `--real-jdk`: all 12 W7-84
// warnings in a run are `class_id=ClassId(12) index=0`, one class and one
// slot, the VM's own class-mirror populator over
// `java.lang.Class.cachedConstructor` (`vm/src/vm/vm_object.rs`). That path is
// `autobox::box_for_reference_slot` reached from the DESCRIPTOR-LESS
// `set_field`; it BOXES. This path is descriptor-driven; it NULLS. Different
// function, different answer, disjoint populations.
//
// The instrument deliberately does not change any answer. See
// `docs/feature-designs/zgc-reference-slot-representation.md`.

/// Which kind of value-destroying coercion fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldCoercionLoss {
    /// A primitive was handed to a slot the class declares `L`/`[`, and was
    /// replaced by `null`. The G25/G30 headline species.
    PrimitiveIntoReference,
    /// A primitive was handed to a slot the class declares `L`/`[` and was
    /// **stored as-is** (the `Float` / `ReturnAddress` fall-through). Reported
    /// separately because the value survives — the reader of that slot gets a
    /// primitive where a reference is declared, rather than a null.
    PrimitiveIntoReferenceUncoerced,
    /// `Value::Object(None)` was handed to a primitive slot and became the
    /// typed JVMS zero — i.e. `null` silently means `0` / `false`.
    NullIntoPrimitive,
    /// A live object POINTER was handed to a primitive slot and was published
    /// as its own address. The worst of the three: the answer is not merely
    /// wrong, it is non-deterministic and leaks a heap address.
    PointerIntoPrimitive,
}

impl FieldCoercionLoss {
    /// Number of variants; the row count of the counter matrix.
    pub const COUNT: usize = 4;

    #[inline]
    fn index(self) -> usize {
        match self {
            FieldCoercionLoss::PrimitiveIntoReference => 0,
            FieldCoercionLoss::PrimitiveIntoReferenceUncoerced => 1,
            FieldCoercionLoss::NullIntoPrimitive => 2,
            FieldCoercionLoss::PointerIntoPrimitive => 3,
        }
    }

    /// Stable, greppable name for logs and dumps.
    pub fn name(self) -> &'static str {
        match self {
            FieldCoercionLoss::PrimitiveIntoReference => "primitive-into-reference",
            FieldCoercionLoss::PrimitiveIntoReferenceUncoerced => {
                "primitive-into-reference-uncoerced"
            }
            FieldCoercionLoss::NullIntoPrimitive => "null-into-primitive",
            FieldCoercionLoss::PointerIntoPrimitive => "pointer-into-primitive",
        }
    }
}

/// Which side of the field access the coercion ran on.
///
/// A READ that coerces is usually benign — the slot was never
/// descriptor-initialised and the read is *repairing* it (MEASURED: 336 of
/// the 352 coercions in one `RJdkNet` run are reads of
/// `java.lang.ref.ReferenceQueue.head`, which correctly answer `null`). A
/// STORE that coerces is the defect. Reporting them in one bucket is how the
/// signal gets lost, so they are counted separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldAccessKind {
    Read,
    Store,
    /// The caller did not say: only the descriptor-only entry point
    /// ([`coerce_field_value_by_descriptor`]) lands here. Every heap on the live
    /// `VmHeap` dispatch path passes a real [`FieldCoercionSite`] since G45;
    /// see [`coerce_field_value_for_slot`].
    Unattributed,
}

impl FieldAccessKind {
    /// Number of variants; the column count of the counter matrix.
    pub const COUNT: usize = 3;

    #[inline]
    fn index(self) -> usize {
        match self {
            FieldAccessKind::Read => 0,
            FieldAccessKind::Store => 1,
            FieldAccessKind::Unattributed => 2,
        }
    }

    /// Stable, greppable name for logs and dumps.
    pub fn name(self) -> &'static str {
        match self {
            FieldAccessKind::Read => "read",
            FieldAccessKind::Store => "store",
            FieldAccessKind::Unattributed => "unattributed",
        }
    }
}

/// Everything the instrument knows about where a coercion came from.
///
/// `Copy` and three words wide so passing it costs nothing on the arms that
/// never look at it.
#[derive(Debug, Clone, Copy)]
pub struct FieldCoercionSite {
    pub kind: FieldAccessKind,
    pub class_id: Option<ClassId>,
    pub index: Option<usize>,
}

impl FieldCoercionSite {
    /// A caller that carries no provenance at all.
    pub const UNATTRIBUTED: Self = Self {
        kind: FieldAccessKind::Unattributed,
        class_id: None,
        index: None,
    };

    /// A descriptor-aware READ of `index` on an object of `class_id`.
    #[inline]
    pub fn read(class_id: Option<ClassId>, index: usize) -> Self {
        Self {
            kind: FieldAccessKind::Read,
            class_id,
            index: Some(index),
        }
    }

    /// A descriptor-aware STORE to `index` on an object of `class_id`.
    #[inline]
    pub fn store(class_id: Option<ClassId>, index: usize) -> Self {
        Self {
            kind: FieldAccessKind::Store,
            class_id,
            index: Some(index),
        }
    }
}

// The (species, access kind) counter matrix used to be a `static` here. It
// now lives in `cratonvm_types::compact_value::coercion_census`, and the
// accessors below read it from there.
//
// It moved for ONE reason, and not for tidiness: the count has to be printed
// from `native-builtins`' `System.exit` shutdown trailer, and
// `cratonvm-native-builtins` does not depend on `cratonvm-gc`. A summary the
// shutdown trailer cannot call is a summary a JUnit runner never prints --
// which is the failure `cell_census` was moved into `cratonvm-types` to fix,
// after a 1975-class sweep produced its line in zero of 1975 logs and read as
// a clean run. Keeping a second copy of the counters here and mirroring into
// the census was the alternative, and was rejected: any skew between the two
// would be indistinguishable from a real miscount, which is the argument
// `DEGRADATION_COUNTS` already makes for defining its total as a sum rather
// than keeping a fourth atomic.
//
// The two tables must stay the same width. A mismatch would silently drop a
// species (the census ignores an out-of-range index rather than panicking on
// a path that has already lost a value), so make it a compile error here,
// where the enums are.
const _: () = assert!(
    FieldCoercionLoss::COUNT == cratonvm_types::compact_value::coercion_census::SPECIES,
    "FieldCoercionLoss and coercion_census::SPECIES must be widened together"
);
const _: () = assert!(
    FieldAccessKind::COUNT == cratonvm_types::compact_value::coercion_census::KINDS,
    "FieldAccessKind and coercion_census::KINDS must be widened together"
);

/// How many times each (species, access kind) pair has fired in this process.
///
/// Rows are indexed by [`FieldCoercionLoss`] in declaration order, columns by
/// [`FieldAccessKind`]. Intended for a shutdown summary or the native-registry
/// dump; see [`field_coercion_loss_report`] for a rendered form.
pub fn field_coercion_loss_counts() -> [[u64; FieldAccessKind::COUNT]; FieldCoercionLoss::COUNT] {
    cratonvm_types::compact_value::coercion_census::counts()
}

/// Total number of value-destroying descriptor coercions in this process.
pub fn field_coercion_loss_total() -> u64 {
    cratonvm_types::compact_value::coercion_census::total()
}

/// A one-line-per-species rendering of [`field_coercion_loss_counts`], or
/// `None` when nothing fired.
///
/// `None` rather than an empty string so a caller can print a section header
/// only when there is something under it.
pub fn field_coercion_loss_report() -> Option<String> {
    let counts = field_coercion_loss_counts();
    if counts.iter().flatten().all(|&n| n == 0) {
        return None;
    }
    let species = [
        FieldCoercionLoss::PrimitiveIntoReference,
        FieldCoercionLoss::PrimitiveIntoReferenceUncoerced,
        FieldCoercionLoss::NullIntoPrimitive,
        FieldCoercionLoss::PointerIntoPrimitive,
    ];
    let kinds = [
        FieldAccessKind::Read,
        FieldAccessKind::Store,
        FieldAccessKind::Unattributed,
    ];
    let mut s = String::new();
    for sp in species {
        let row = counts[sp.index()];
        if row.iter().all(|&n| n == 0) {
            continue;
        }
        s.push_str(sp.name());
        for k in kinds {
            s.push_str(&format!(" {}={}", k.name(), row[k.index()]));
        }
        s.push('\n');
    }
    Some(s)
}

/// Memoise a boolean flag in a [`cratonvm_types::flags::MemoSlot`].
///
/// A plain `OnceLock` latched the first value read in the process and so
/// defeated the per-thread / per-process test overrides the callers' comments
/// promise (`flags::with_thread_overrides`). A `MemoSlot` is reset when an
/// override is installed or dropped and declines to publish while one is live,
/// so an override is honoured and never latches.
fn memoised_flag(slot: &'static cratonvm_types::flags::MemoSlot, read: impl FnOnce() -> bool) -> bool {
    match slot.load() {
        1 => false,
        2 => true,
        _ => {
            let v = read();
            slot.publish(if v { 2 } else { 1 });
            v
        }
    }
}

/// `CRATONVM_DBG_COERCION=1` — log EVERY loss with a backtrace.
///
/// Read once (re-read after a flag override). This is the flag a lane repairing individual sites wants. Since
/// 2026-09-01 it is also the only way to get a per-occurrence line at all; see
/// [`note_field_coercion_loss`] and [`coercion_loss_sampled`].
fn coercion_loss_verbose() -> bool {
    static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    memoised_flag(&ON, || {
        // `flags::runtime_var`, not `std::env::var`. A DECLARED name read
        // raw is served by a live `getenv` instead of the latched snapshot,
        // so `CRATONVM_DBG=coercion` would silently do nothing here and a
        // test could not arrange it through `with_thread_overrides` — which
        // matters for a diagnostic the guard's own WARN text tells operators
        // to set. Check 4 of `tools/flag-census/check-surface.sh` names this
        // call site as a core-crate bypass.
        false    })}

/// `CRATONVM_DBG_OVERLAY=1` — keep the escalating per-occurrence SAMPLE
/// (occurrence 0, 1, 2, 4, 8, … per species) without the backtrace.
///
/// The escalating dedup is a good design and is kept, just not on by default:
/// it bounds a boot that coerces thousands of times to about a dozen lines,
/// which is the right budget for a reader who has ASKED to watch slot-level
/// damage. `CRATONVM_DBG_OVERLAY` rather than a flag of its own because that
/// is the lane that already reads these rows — the overlay hunter in
/// `vm/src/vm/vm_exec.rs` reports a native writing a value whose tag
/// contradicts the model, which is the same event this guard sees one layer
/// down, and having to arm two flags to see two halves of one story is how
/// half of it gets missed.
fn coercion_loss_sampled() -> bool {
    static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    memoised_flag(&ON, || {
        // `flags::runtime_var_os`, not `std::env::var_os`, for the reason
        // spelled out in `coercion_loss_verbose`: a DECLARED name read raw is
        // served by a live `getenv` instead of the latched snapshot, so
        // `CRATONVM_DBG=overlay` would silently do nothing here.
        false    })}

/// The instrument. Counts always; on a DEFAULT run says nothing at all.
///
/// # 2026-09-01: this became a counter, and the reason is a measurement
///
/// A stock `cratonvm Hello` — a one-line hello-world — printed 28 lines of
/// internal diagnostics to stderr where HotSpot prints zero, and 24 of the 28
/// came from this one function. The `primitive-into-reference` read species
/// reaches occurrence 2048 during boot (an ordinary `HashSet.add` reading
/// `java.util.HashMap.table`, which the real JDK layout declares
/// `[Ljava/util/HashMap$Node;` and CratonVM's synthetic init writes
/// `Int(capacity)` into), and even the escalating sample turns that into a
/// dozen WARN lines with the full paragraph attached.
///
/// That is no longer an instrument. It is the background against which a real
/// warning has to be noticed, and any tool reading this VM's stderr — CI, a
/// build script, a subprocess consumer, a test harness — sees it. So: the
/// counter is unchanged and always runs, the per-occurrence rows moved behind
/// [`coercion_loss_verbose`] / [`coercion_loss_sampled`], and the numbers are
/// printed once by
/// `cratonvm_types::compact_value::coercion_census::exit_summary`.
///
/// **Nothing about the coercion itself changed.** Every arm still returns the
/// byte-identical `Value` it returned before; see the `b'L' | b'['` arm of
/// [`coerce_field_value_for_slot`] for why that rule is load-bearing.
///
/// Rate limit — when a flag has asked for the rows at all — is
/// `n < 4 || n.is_power_of_two()`, PER SPECIES: the same shape as
/// `autobox::observe_primitive_into_reference_field` and the sibling
/// `cratonvm::gc::guard` records, chosen so a boot that coerces thousands of
/// times costs ~a dozen lines rather than minutes of stderr. Per species
/// rather than globally so a high-frequency benign read population (see
/// [`FieldAccessKind`]) cannot bury a rare store.
///
/// Target `cratonvm::gc::guard`, deliberately: that target already emits on
/// this VM's default stderr configuration, so anyone who has ever looked at a
/// CratonVM boot log has the filter for it, and a new target would have been
/// one more thing to know about.
///
/// **No `debug_assert!` and no refusal.** `autobox.rs`'s module note settles
/// why, and the reasoning transfers verbatim: the population reaching this is
/// live on shipped paths (400 measured sites), so a hard error converts a
/// wrong answer into a crash, and an assert reds the synthetic-JDK tests where
/// a fabricated class's slot genuinely IS the primitive it is handed. Take the
/// diagnostic half without the behaviour half.
#[cold]
fn note_field_coercion_loss(
    loss: FieldCoercionLoss,
    site: FieldCoercionSite,
    value: Value,
    desc_byte: u8,
) {
    use cratonvm_types::compact_value::coercion_census;

    // The census carries the locator as well as the count, because a count
    // with no locator is not actionable: "2048 coercions" and "2048
    // coercions, all of them class 64 slot 2 at descriptor `[`" are different
    // findings and only the second one can be repaired. `NO_CLASS`/`NO_INDEX`
    // are the sentinels for a caller that has no provenance (the descriptor-only
    // `coerce_field_value_by_descriptor` entry point; the collectors on the live
    // `VmHeap` dispatch path pass a site since G45); they render as the same `-1` the WARN below prints, so
    // old and new logs grep alike. A slot index is truncated to 16 bits in
    // the census key, which is lossless for every real field table.
    let class_key = site
        .class_id
        .map(|c| c.as_u32())
        .unwrap_or(coercion_census::NO_CLASS);
    let index_key = site
        .index
        .map(|i| u32::try_from(i).unwrap_or(coercion_census::NO_INDEX))
        .unwrap_or(coercion_census::NO_INDEX);
    let n = coercion_census::note(
        loss.index(),
        site.kind.index(),
        desc_byte,
        class_key,
        index_key,
    );

    // DEFAULT RUN RETURNS HERE — silent, counted, reported once at exit.
    let verbose = coercion_loss_verbose();
    if !verbose && !(coercion_loss_sampled() && (n < 4 || n.is_power_of_two())) {
        return;
    }
    let descriptor = desc_byte as char;
    let class = site.class_id.map(|c| c.as_u32() as i64).unwrap_or(-1);
    let index = site.index.map(|i| i as i64).unwrap_or(-1);
    // G45-1 NOMINATION 3: the id was already resolvable (`CRATONVM_DBG_LAYOUT=1`),
    // just not resolved in this line, so a reader had to cross-reference two
    // separate runs. `crate::gc::resolve_class_info` is the same hook
    // `gen_heap.rs`'s FIELD-WATCH path already calls on this same cold
    // diagnostic tier — reachable from here too, `gc` was never actually
    // unable to ask, only this call site had not asked yet.
    let class_name = site
        .class_id
        .and_then(|c| crate::gc::resolve_class_info(c.as_u32()))
        .map(|(name, _)| name)
        .unwrap_or_else(|| "<unresolved>".to_string());
    tracing::warn!(
        target: "cratonvm::gc::guard",
        species = loss.name(),
        access = site.kind.name(),
        descriptor = %descriptor,
        value = ?value,
        class_id = class,
        class_name = %class_name,
        index,
        occurrence = n,
        "a descriptor-aware field access DESTROYED the value it was handed \
         (G30-1-the-silent-reference-slot-coercion-20260817.md). This is NOT \
         the W7-84 autobox guard: that one boxes and fires only for the class \
         mirror; this one nulls (or zeroes) and is the shape behind the null \
         `java.net.ServerSocket.impl`. class_id=-1/index=-1 means the caller \
         came through the descriptor-only entry point, which carries no \
         provenance. class_name=\"<unresolved>\" means the id has no \
         registered class (a raw test-only id, or the resolver hook was never \
         installed) — CRATONVM_DBG_COERCION=1 still gives every occurrence \
         with a backtrace. THIS GUARD SEES DESCRIPTOR MISMATCHES ONLY: a \
         wrong slot whose value happens to fit the field's own descriptor is \
         invisible here, so a quiet log is not a clean one. G59-1 measured \
         both halves of one defect at once -- two writes warned, and two \
         more from the same line landed silently on an int and set a 1ms \
         connect timeout.",
    );
    if verbose {
        tracing::warn!(
            target: "cratonvm::gc::guard",
            "  coercion-loss backtrace:\n{}",
            std::backtrace::Backtrace::force_capture(),
        );
    }
}

/// T10.9.E — Coerce a loaded-from-slot `Value` to match its declared field
/// type.
///
/// This is the runtime complement of [`default_value_for_descriptor`]: that
/// helper sets the **initial** slot contents so zero-init has the correct
/// tag; this helper **normalises** any drifted Value on read/write so
/// downstream code always sees the declared variant.
///
/// Behaviour is a superset of `CompactValue::decode_by_descriptor`:
/// - `Value::Object(None)` on a primitive descriptor → the typed JVMS zero.
/// - `Value::Long(x)` on a J descriptor → unchanged.
/// - `Value::Double(d)` on a J descriptor → `Value::Long(d.to_bits() as i64)`.
///   This is the Session 93 fix: a long bit pattern that happened to be
///   written as Double (via CompactValue::long + round-trip through
///   `to_value`) is reinterpreted as the intended long.
/// - symmetric for D/F/I families.
/// - references (L/[): unchanged from raw read.
/// - unknown descriptor: unchanged (`_ => value`).
///
/// Defensive: never panics; always returns a `Value`.
///
/// # G30: the lossy arms are now instrumented, and NOTHING ELSE CHANGED
///
/// Three of the arms below do not normalise a value, they DESTROY it, and
/// until 2026-08-17 every one of them was silent in a release build
/// (`G30-1-the-silent-reference-slot-coercion-20260817.md`). They are now
/// routed through [`note_field_coercion_loss`], which counts and (rate-limited)
/// warns. **The returned `Value` is byte-identical to what this function
/// returned before**, deliberately: 400 measured sites in this tree rely on
/// the current answers, `java.util.HashMap.table` load-bearingly so (see the
/// `b'L'` arm), and a VM that started refusing them all at once would fail
/// catastrophically and prove nothing. Visibility first; the per-site repairs
/// are nominated individually.
///
/// See [`coerce_field_value_for_slot`] for the variant that carries the
/// class and slot into the report.
#[inline]
pub fn coerce_field_value_by_descriptor(value: Value, desc_byte: u8) -> Value {
    coerce_field_value_for_slot(value, desc_byte, FieldCoercionSite::UNATTRIBUTED)
}

/// [`coerce_field_value_by_descriptor`] with provenance for the instrument.
///
/// The coercion itself is identical; `site` only decides how good the warning
/// is. `Heap`'s own four descriptor-aware accessors pass a real site, and so
/// do the three collectors on the live `VmHeap` dispatch path (`gen_heap.rs`,
/// `g1.rs`, `zgc.rs` via the `collector.rs` trait defaults, G45); only a
/// caller of the descriptor-only entry point above reports `UNATTRIBUTED`.
#[inline]
pub fn coerce_field_value_for_slot(value: Value, desc_byte: u8, site: FieldCoercionSite) -> Value {
    if matches!(
        (desc_byte, value),
        (b'I' | b'Z' | b'B' | b'C' | b'S', Value::Int(_))
            | (b'J', Value::Long(_))
            | (b'L' | b'[', Value::Object(_))
            | (b'D', Value::Double(_))
            | (b'F', Value::Float(_))
    ) {
        return value;
    }
    coerce_field_value_for_slot_slow(value, desc_byte, site)
}

fn coerce_field_value_for_slot_slow(value: Value, desc_byte: u8, site: FieldCoercionSite) -> Value {
    match desc_byte {
        b'J' => match value {
            Value::Long(_) => value,
            Value::Double(d) => Value::Long(d.to_bits() as i64),
            Value::Float(f) => Value::Long(f.to_bits() as i64),
            Value::Int(i) => Value::Long(i as i64),
            // `Uninitialized` is the allocator's "no value yet" tag, not a
            // claim by any writer about what the slot holds, so it is the one
            // arm here that is NOT a loss and is left silent. Splitting it out
            // of the shared `Object(None) | Uninitialized` pattern is the only
            // structural change in this function; both still yield `Long(0)`.
            Value::Uninitialized => Value::Long(0),
            Value::Object(None) => {
                note_field_coercion_loss(FieldCoercionLoss::NullIntoPrimitive, site, value, b'J');
                Value::Long(0)
            }
            // An object pointer landing in a long slot is upstream drift;
            // surface the raw pointer as a long so downstream unsafe ops
            // can decode it (matches HotSpot behavior of treating the slot
            // as raw bits).
            Value::Object(Some(o)) => {
                note_field_coercion_loss(
                    FieldCoercionLoss::PointerIntoPrimitive,
                    site,
                    value,
                    b'J',
                );
                Value::Long(o.as_ptr() as usize as i64)
            }
            Value::ReturnAddress(pc) => Value::Long(pc as i64),
        },
        b'D' => match value {
            Value::Double(_) => value,
            Value::Long(l) => Value::Double(f64::from_bits(l as u64)),
            Value::Float(f) => Value::Double(f as f64),
            Value::Int(i) => Value::Double(i as f64),
            Value::Uninitialized => Value::Double(0.0),
            Value::Object(None) => {
                note_field_coercion_loss(FieldCoercionLoss::NullIntoPrimitive, site, value, b'D');
                Value::Double(0.0)
            }
            Value::Object(Some(o)) => {
                note_field_coercion_loss(
                    FieldCoercionLoss::PointerIntoPrimitive,
                    site,
                    value,
                    b'D',
                );
                Value::Double(f64::from_bits(o.as_ptr() as usize as u64))
            }
            Value::ReturnAddress(pc) => Value::Double(pc as f64),
        },
        b'F' => match value {
            Value::Float(_) => value,
            Value::Int(i) => Value::Float(f32::from_bits(i as u32)),
            Value::Long(l) => Value::Float(f32::from_bits(l as u32)),
            // G52, THE ONE CELL THAT DISAGREES WITH ITSELF — reported, not
            // moved. The `Long` arm one line up reads the slot as a BIT
            // pattern; this arm reads it as a NUMBER. They are the same
            // untagged compact slot, so they contradict each other:
            // `Long(5)` at `F` is `Float(7e-45)` and
            // `Double(f64::from_bits(5))` at `F` is `Float(0.0)`. Every other
            // cross-variant pair in this function agrees (see the `b'I'`
            // `Double` arm for the argument and the invariant it is pinned
            // by); this is the only pair that does not.
            //
            // NOT changed here, deliberately. Either answer is defensible in
            // isolation — `d as f32` is the correct JVMS `d2f` for a genuine
            // double, `f32::from_bits(d.to_bits() as u32)` is the correct
            // decode for an untagged slot — and MEASURED 2026-08-17, zero
            // `Value::Double` reached ANY `b'F'` slot in a 26-vector-run
            // sweep, so there is no live population to decide it against and
            // no run that could falsify a change. Whoever gets a
            // non-zero count here first should decide it; until then the
            // disagreement is pinned by
            // `the_double_and_long_arms_disagree_only_at_a_float_slot` so it
            // cannot drift in silence.
            Value::Double(d) => Value::Float(d as f32),
            Value::Uninitialized => Value::Float(0.0),
            Value::Object(None) => {
                note_field_coercion_loss(FieldCoercionLoss::NullIntoPrimitive, site, value, b'F');
                Value::Float(0.0)
            }
            Value::Object(Some(o)) => {
                note_field_coercion_loss(
                    FieldCoercionLoss::PointerIntoPrimitive,
                    site,
                    value,
                    b'F',
                );
                Value::Float(f32::from_bits(o.as_ptr() as usize as u32))
            }
            Value::ReturnAddress(pc) => Value::Float(f32::from_bits(pc)),
        },
        b'I' | b'B' | b'C' | b'S' | b'Z' => match value {
            Value::Int(_) => value,
            Value::Long(l) => Value::Int(l as i32),
            Value::Float(f) => Value::Int(f.to_bits() as i32),
            // G52, DELIBERATE — this is a BIT projection and not `d as i32`,
            // and the reason is the same one the `b'J'` arm above states.
            //
            // `G43-1` NOMINATION 5 asked for this to become the numeric
            // narrowing `d as i32`, on the ground that a bit-cast makes the
            // blast radius of a mis-slotted `Double` depend on its VALUE. The
            // complaint is right; the proposed repair is wrong, three times
            // over, and this comment exists so nobody has to re-derive that.
            //
            // 1. IT IS THE UNTAGGED-COMPACT-SLOT DECODE, NOT A DOUBLE
            //    CONVERSION. This function declares itself a superset of
            //    `CompactValue::decode_by_descriptor`, and this is the arm
            //    that makes the claim true: `types/src/compact_value.rs:1675`
            //    answers an integral descriptor on an UNTAGGED slot with
            //    `Value::Int(self.0 as u32 as i32)` — the low 32 raw bits.
            //    An untagged slot decoded through `to_value()` surfaces as
            //    `Value::Double(f64::from_bits(raw))`, so a `Value::Double`
            //    arriving here is, on the shipped compact-layout path,
            //    OVERWHELMINGLY A LONG/INT BIT PATTERN rather than a number.
            //    Taking its low half is the correct `l2i`.
            //
            // 2. IT IS FORCED BY THE `b'J'` ARM. `Value::Long(l)` and
            //    `Value::Double(f64::from_bits(l as u64))` are the same
            //    untagged slot read two ways; they MUST agree about the
            //    slot's low half, or a long read at `I` and the same long
            //    read at `J` contradict each other. `d.to_bits() as i32`
            //    makes them agree for every `l`; `d as i32` makes them
            //    disagree for every `l` outside the subnormal window —
            //    `CompactValue::long(5)` would read back as `0`, which is
            //    Session 93 resurrected at 32 bits. Pinned by
            //    `a_double_at_an_integral_slot_decodes_like_the_untagged_long_it_usually_is`.
            //
            // 3. ON `G43-1`'s OWN CASE THE PROPOSED REPAIR IS STRICTLY WORSE.
            //    `G43-1` §5.2 shows a synthetic `Provider` version landing on
            //    the real `java.util.Hashtable.count`, surviving only because
            //    `25.0f64.to_bits() as i32 == 0` and `getEnumeration`
            //    early-returns on `count == 0`. Under `d as i32` that same
            //    write yields `count == 25`, the early return does NOT fire,
            //    and `keys()` walks the `String` sitting in `table` as an
            //    `Entry[]` — i.e. the numeric rule converts that record's
            //    LATENT corruption into a LIVE one. (The producer is gated
            //    off at HEAD by `provider_has_named_layout`,
            //    `native-builtins/src/jca/provider_chain.rs:285`.)
            //
            // What IS wrong is that the ambiguity is unresolvable here: a
            // genuine `double` mis-slotted into an `int` field and an
            // untagged long that round-tripped through `to_value()` arrive as
            // the same `Value::Double`, and this arm resolves it in favour of
            // the one that happens on a shipped path. That is a property of
            // the boxed-`Value` representation, not of this line, and it
            // cannot be fixed by choosing the other answer.
            Value::Double(d) => Value::Int(d.to_bits() as i32),
            Value::Uninitialized => Value::Int(0),
            // MEASURED, G25-1 §1 consequence 1: this arm is how
            // `create_ssl_server_socket`'s fourth write — an explicit
            // `Value::Object(None)` aimed at what its model called slot 3 —
            // became `java.net.ServerSocket.closed = false`. It is not a
            // "dropped" store, it is a store of the typed zero.
            Value::Object(None) => {
                note_field_coercion_loss(
                    FieldCoercionLoss::NullIntoPrimitive,
                    site,
                    value,
                    desc_byte,
                );
                Value::Int(0)
            }
            Value::Object(Some(o)) => {
                note_field_coercion_loss(
                    FieldCoercionLoss::PointerIntoPrimitive,
                    site,
                    value,
                    desc_byte,
                );
                Value::Int(o.as_ptr() as usize as i32)
            }
            Value::ReturnAddress(pc) => Value::Int(pc as i32),
        },
        b'L' | b'[' => match value {
            Value::Object(_) => value,
            // A primitive landing where a reference is declared is a verifier
            // violation; degrade to null rather than surface a bogus Value
            // variant to native code. S111r29: extend this to ALL Int/Long
            // values (not just zero) — synthetic init paths historically wrote
            // `Int(capacity)` to slots that the real-JDK class layout declares
            // as references (e.g. `HashMap.table: [Ljava/util/HashMap$Node;`),
            // which then aborted JDK bytecode `arraylength` with
            //   `expected object reference, got int(N)`. Coercing the bogus
            // primitive to null lets `HashMap.resize()`'s
            //   `(oldTab == null) ? 0 : oldTab.length` branch handle the
            // never-initialized case correctly.
            //
            // G30: this rule is DELIBERATE AND LOAD-BEARING and must not be
            // deleted — `the_hashmap_table_degrade_to_null_is_pinned` fails if
            // it is. What was wrong was never the rule; it was that the rule
            // fired in silence, so the 374 callers writing an `Int` at a
            // reference slot could not tell they were writing null. The
            // instrument below is the whole of the change.
            //
            // 2026-09-01, WHERE THE REAL FIX IS. The instrument is now a
            // counter: it says nothing per occurrence and contributes one
            // line at exit (`coercion_census::exit_summary`). That is a
            // change to the REPORT, not to the defect, and it deliberately
            // makes the defect smaller on screen while leaving it exactly as
            // large in the program. The defect is the writer population —
            // ~374 callers, `native-collections`' `HashMap`/`HashSet` model
            // among them, that store an `Int` into a slot the real JDK layout
            // declares as a reference. The number this census prints IS the
            // size of that population's traffic, and the population is what
            // should shrink; a repair that made the number smaller by any
            // other route (quieting the guard further, narrowing what counts,
            // or "fixing" this arm) would be measuring its own reporting.
            // Repairs are nominated per writer, not here.
            Value::Int(_) | Value::Long(_) => {
                note_field_coercion_loss(
                    FieldCoercionLoss::PrimitiveIntoReference,
                    site,
                    value,
                    desc_byte,
                );
                Value::Object(None)
            }
            // A `Value::Double` landing in a reference-typed (`L`/`[`) field is
            // a genuine type error — the bytecode wrote a primitive where the
            // class layout declares a reference. Degrade it to null, exactly
            // like the `Int`/`Long` case above.
            //
            // SAFETY/UAF NOTE: a previous revision reinterpreted an 8-aligned,
            // sub-2^48 double bit-pattern as a live `ObjectRef` via
            // `ObjectRef::from_raw`. That fabricates a wild heap pointer out of
            // arbitrary numeric data: any double that happens to bit-match an
            // aligned address would be handed to the GC and field accessors as
            // a real object, causing a use-after-free or worse. Numeric data
            // is NOT a pointer — never manufacture one. Coerce to null.
            Value::Double(_) => {
                note_field_coercion_loss(
                    FieldCoercionLoss::PrimitiveIntoReference,
                    site,
                    value,
                    desc_byte,
                );
                Value::Object(None)
            }
            // G30, SOURCE-VERIFIED and left alone on purpose: `Float` and
            // `ReturnAddress` fall through the old `_ => value` arm, so a
            // `Value::Float` written at a reference slot is neither refused
            // nor nulled — it is STORED, and a later reader sees a `Float`
            // where the class declares an object. That asymmetry with the
            // `Double` arm two lines up is almost certainly an oversight, but
            // closing it is a behaviour change at an unknown number of sites,
            // so it is reported and NOT repaired here (G30 NOMINATION 6).
            // The value is passed through byte-identically to before.
            //
            // G52 — "an unknown number of sites" is no longer unknown, and
            // the answer is ZERO. This species now has a denominator from
            // three independent directions:
            //
            // * RUNTIME, `primitive-into-reference-uncoerced`: 0 events in
            //   1,339 (G45-1 §3, 19 vectors) and 0 in a further 638 (G52-1,
            //   10 vectors). 0 / 1,977.
            // * RUNTIME, the other instrument: 0 `Float`-at-a-reference-slot
            //   rows in a 4-vector `CRATONVM_DBG_OVERLAY=1` sweep, whose
            //   211 `[cross-type]` rows are ALL `Int` at `L`/`[`.
            // * STATIC: every `Value::Float` reaching a `set_field`-family
            //   call in the native crates (25 sites) targets a slot whose
            //   REAL JDK-25 descriptor is `F` — `HashMap`/`Hashtable`
            //   `loadFactor`, `java.lang.Float.value`, `CharsetEncoder`
            //   slots 1/2 (`averageBytesPerChar`/`maxBytesPerChar`), and
            //   `Float` wrapper boxes. Resolved against `javap -p` on
            //   HotSpot 25.0.3+9; see the G52-1 record for the table.
            //
            // `ReturnAddress` is stronger than empty, it is UNREACHABLE: the
            // only producer in the tree is `jsr`/`jsr_w`
            // (`vm/src/runtime/interpreter/opcodes.rs:4009`/`:4016`), those
            // opcodes are illegal in class files of version >= 51, and even
            // a hypothetical one could only reach a slot through the
            // interpreter's `putfield`, which does not coerce.
            //
            // So closing the hole (nulling `Float` like `Double`) is now a
            // safe one-line change — and also an UNVERIFIABLE one, because
            // an empty population means no run can tell the two versions
            // apart. It is still not taken here for that reason. What WOULD
            // justify taking it: a non-zero
            // `primitive-into-reference-uncoerced` count from any vector.
            Value::Float(_) | Value::ReturnAddress(_) => {
                note_field_coercion_loss(
                    FieldCoercionLoss::PrimitiveIntoReferenceUncoerced,
                    site,
                    value,
                    desc_byte,
                );
                value
            }
            Value::Uninitialized => value,
        },
        _ => value,
    }
}

/// Get a pointer to the slot at `index` (field or array element).
#[inline]
unsafe fn slot_ptr(obj_ref: ObjectRef, index: usize) -> *mut u8 {
    obj_ref.as_ptr().add(HEADER_SIZE + index * SLOT_SIZE)
}

/// Read a `Value` from a slot.
///
/// We store the raw 16-byte `Value` enum directly in the slot. The first
/// byte is the variant discriminant (`Int=0`, `Long=1`, `Float=2`,
/// `Double=3`, `Object=4`, `ReturnAddress=5`, `Uninitialized=6`); the niche
/// `Object(None)` is discriminant 4 with an all-zero pointer payload.
///
/// DECODE RULE (R-niche): there is no "zero bytes decode to null" shortcut.
/// Since `Value::Object` gained a `NonNull` niche, the all-zero bit pattern
/// has discriminant byte 0 and decodes as `Value::Int(0)` — bit-identical to
/// a slot that was explicitly written `Value::Int(0)`. A read therefore
/// returns exactly the variant whose discriminant was last *written* into the
/// slot. Reference/uninitialized slots are NOT left zeroed: callers that need
/// the `null` default (e.g. `alloc_object_with_descriptors`) write an explicit
/// `Value::Object(None)` so the discriminant byte is 4 and this read yields
/// `null`, not `Int(0)`.
///
/// PLAIN-SLOT TEARING FIX (2026-07-06): this used to be a bare
/// `std::ptr::read::<Value>(ptr)` — a non-atomic 16-byte copy that can race
/// a concurrent plain `set_field` from another mutator thread. Commit
/// `4e6b560f` ("atomic-per-word object-slot access for concurrent marking")
/// already closed this gap for the GC-marker-vs-JIT-store race using
/// `cratonvm_types::read_value_atomic`/`write_value_atomic`, but left the
/// interpreter's own plain `get_field`/`set_field` (this function) using the
/// raw, non-atomic path — so two ordinary mutator threads doing plain
/// `getfield`/`putfield` on the SAME field slot could still tear each
/// other's writes. Real JDK library code legally relies on this being
/// tear-free (e.g. `ReentrantReadWriteLock$Sync`'s plain `firstReader`/
/// `firstReaderHoldCount`, published via a nearby `volatile`/CAS write to
/// `state` — see
/// elasticsearch-lucene-binary-docvalues-range-hangs.md
/// #3). Delegates to the same already-proven `read_value_atomic` helper.
///
/// # Safety
/// The pointer must be valid and 8-byte aligned.
unsafe fn read_slot(ptr: *mut u8) -> Value {
    read_value_cell_checked(ptr as *const Value, "heap::read_slot")
}

/// Corrupt-`Value`-cell telemetry, published for the VM.
///
/// The guard below lives in the collector crate, so it can name the CELL - the
/// address and the two words it holds - and nothing else. The question the
/// record it belongs to actually asks is the other half: which reference points
/// at a block that was swept and re-served, and who is holding it. Only the VM
/// can answer that, because only the VM has the receiver and the owning thread's
/// frames. Publishing the hit count lets a VM-side field read notice "that read
/// tripped the guard" and report the producer at the moment it happens; the
/// three coordinates let it report the same cell the guard did.
///
/// Relaxed throughout: this is a diagnostic on an already-failed path, and the
/// only ordering that matters - the counter moving before the reader re-reads it
/// - comes from the read being sequenced between the two loads on one thread.
///
/// The four coordinates are written together under [`CORRUPT_CELL_SEQ`] (a
/// seqlock: odd while a writer is mid-update; a writer that finds another one
/// mid-update drops its record, since only the last hit is kept anyway) and
/// read as one snapshot by [`corrupt_cell_last_with_thread`], so two hits at
/// once can no longer leave one cell's slot beside another's raw words and tid.
static CORRUPT_CELL_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static CORRUPT_CELL_SLOT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static CORRUPT_CELL_RAW0: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static CORRUPT_CELL_RAW1: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static CORRUPT_CELL_TID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Publish one corrupt-cell record: slot, both raw words and the calling
/// thread's [`probe_thread_id`], as one seqlock-protected group.
fn corrupt_cell_record(slot: usize, raw0: u64, raw1: u64) {
    let seq = CORRUPT_CELL_SEQ.load(Ordering::Relaxed);
    if seq & 1 != 0
        || CORRUPT_CELL_SEQ
            .compare_exchange(seq, seq + 1, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
    {
        // Another thread is mid-record; keep its record rather than mix ours in.
        return;
    }
    CORRUPT_CELL_SLOT.store(slot, Ordering::Relaxed);
    CORRUPT_CELL_RAW0.store(raw0, Ordering::Relaxed);
    CORRUPT_CELL_RAW1.store(raw1, Ordering::Relaxed);
    CORRUPT_CELL_TID.store(probe_thread_id(), Ordering::Relaxed);
    CORRUPT_CELL_SEQ.store(seq + 2, Ordering::Release);
}

/// How many corrupt `Value` cells this process has decoded (the
/// `cratonvm_types::cell_census` decoded count).
pub fn corrupt_cell_hits() -> u64 {
    cratonvm_types::cell_census::decoded()
}

/// One consistent `(slot address, raw word 0, raw word 1, thread id)` snapshot
/// of the last corrupt cell recorded. Meaningful only when
/// [`corrupt_cell_hits`] is non-zero.
pub fn corrupt_cell_last_with_thread() -> (usize, u64, u64, u64) {
    loop {
        let s0 = CORRUPT_CELL_SEQ.load(Ordering::Acquire);
        if s0 & 1 != 0 {
            std::hint::spin_loop();
            continue;
        }
        let out = (
            CORRUPT_CELL_SLOT.load(Ordering::Relaxed),
            CORRUPT_CELL_RAW0.load(Ordering::Relaxed),
            CORRUPT_CELL_RAW1.load(Ordering::Relaxed),
            CORRUPT_CELL_TID.load(Ordering::Relaxed),
        );
        std::sync::atomic::fence(Ordering::Acquire);
        if CORRUPT_CELL_SEQ.load(Ordering::Relaxed) == s0 {
            return out;
        }
    }
}

/// `(slot address, raw word 0, raw word 1)` of the last corrupt cell decoded.
/// Meaningful only when [`corrupt_cell_hits`] is non-zero.
pub fn corrupt_cell_last() -> (usize, u64, u64) {
    let (slot, raw0, raw1, _) = corrupt_cell_last_with_thread();
    (slot, raw0, raw1)
}

/// A process-local id for the calling thread, cheap and stable.
///
/// `std::thread::ThreadId` has no stable numeric form, and the corrupt-cell
/// counter is process-GLOBAL: without this, a backstop on thread B would report
/// a cell thread A decoded, and on a Spring Boot test class -- which runs
/// several threads -- that is the difference between naming a producer and
/// naming a bystander. The reporter compares this against the id recorded AT
/// the hit and stays silent when they differ.
pub fn probe_thread_id() -> u64 {
    thread_local! {
        static TID: u64 = {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
            NEXT.fetch_add(1, Ordering::Relaxed)
        };
    }
    TID.with(|t| *t)
}

/// The [`probe_thread_id`] of the thread that decoded the last corrupt cell.
pub fn corrupt_cell_last_thread() -> u64 {
    corrupt_cell_last_with_thread().3
}

/// Fabricate one corrupt-cell hit so the VM can prove its reporting chain works.
///
/// A diagnostic that only ever produces silence is indistinguishable from a
/// broken one, and that is not hypothetical here: the first version of this
/// instrument watched a single door, stayed quiet through a guard hit it could
/// not see, and the quiet was read as "nothing to see". The VM arms this from
/// `CRATONVM_DBG_CORRUPT_CELL_SELFTEST` and checks that both the door reporter
/// and the safepoint backstop actually speak.
///
/// Diagnostic-only: nothing calls it unless that flag is set, and it touches
/// only this module's own telemetry statics — never the heap.
pub fn corrupt_cell_inject_for_selftest(slot: usize, raw0: u64, raw1: u64) {
    corrupt_cell_record(slot, raw0, raw1);
    cratonvm_types::cell_census::note_decoded();
}

/// How many plain-object field accesses this process has refused because the
/// RECEIVER was an array. See [`refuse_array_receiver_field_access`].
static ARRAY_RECEIVER_FIELD_ACCESSES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Count of array-receiver field accesses refused this process.
pub fn array_receiver_field_accesses() -> u64 {
    ARRAY_RECEIVER_FIELD_ACCESSES.load(Ordering::Relaxed)
}

/// Refuse a plain-object field access whose RECEIVER is an array, and report it
/// through the corrupt-`Value`-cell chain so the VM names the door.
///
/// # Why this is always a defect, and why the index check does not catch it
///
/// `get_field`/`set_field` address slot `i` at `HEADER_SIZE + i * SLOT_SIZE` —
/// a 16-byte tagged `Value` cell. An array's body holds neither: primitive
/// elements are PACKED at their element width, and reference elements are bare
/// `REF_ELEMENT_SIZE` (8-byte) pointers. So the read decodes two unrelated
/// elements (or two halves of one packed run) as a `(tag, payload)` pair, and
/// the write stamps a 16-byte cell over whatever elements share those bytes.
///
/// The bounds check does not stop it because an array MIRRORS ITS LENGTH into
/// `num_slots` — `alloc_array` writes `ObjectHeader::new(class_id, Array,
/// element_type, len, len)` on every backend. `index < num_slots` therefore
/// admits every index a real element has, while the byte offset it computes
/// runs 2x (reference) to 16x (byte) past the element it names, straight out of
/// the allocation for anything but the first few indices.
///
/// # How a receiver gets here
///
/// Two known species, both "a class-id test with no kind check":
///
///   * a REFERENCE array carries its COMPONENT's class id (a reference array
///     has no class of its own in this VM's class store), so
///     `class_id_of_object(new String[2])` answers `java/lang/String` and a
///     `class_name == "java/lang/String"` fast path matches. That is
///     `corrupt-value-cell-producer-was-a-string-array-FIXED-20260822`, fixed
///     at four named doors with `is_plain_string`.
///   * a SHAPE probe that screens on `num_fields` rather than on kind — the
///     `num_fields(obj) < 2` guard in `vm_object::java_string_value_and_coder`
///     was one, and a `byte[]` of length >= 2 walked straight through it.
///
/// Screening at the accessor is what makes the rule one rule instead of one
/// rule per door: the doors keep their fast-path checks, and a door that
/// forgets one gets a named refusal rather than a silent stride.
///
/// # Why it reports through the corrupt-cell chain
///
/// Because it is the same defect, seen one step earlier. The
/// `cratonvm::gc::guard` corrupt-cell report only fires when the striden bytes
/// happen to form an out-of-range discriminant; bytes that happen to form a
/// valid tag are invisible to it, so its count is a FLOOR and not a census.
/// Routing this through `cratonvm_types::cell_census::note_decoded` and the
/// `CORRUPT_CELL_*` coordinates means the VM-side producer reporter
/// (`CRATONVM_DBG_CORRUPT_CELL` — doors plus the safepoint backstop) names the
/// receiver, the Java frames and the door for every occurrence, not for the
/// lucky ones. See
/// `corrupt-value-cell-array-receiver-species-CLOSED-20260823`.
///
/// # Safety
/// `obj_ptr` must be a live allocation base whose header is `header`. The two
/// raw words are read ONLY when the cell they name lies inside the array's own
/// body; past that they are reported as zero rather than read, because the
/// striding access this refuses is frequently out of the allocation entirely —
/// which is the other half of what it is fixing.
pub unsafe fn refuse_array_receiver_field_access(
    obj_ptr: usize,
    header: &ObjectHeader,
    index: usize,
    site: &'static str,
) {
    // BOTH counters. `note_decoded` publishes the hit to the VM-side producer
    // reporter (`CRATONVM_DBG_CORRUPT_CELL` compares the count across a field
    // read to decide "that read tripped the guard"), so this refusal gets a
    // door, a receiver and a Java stack for free. `note_array_receiver` keeps
    // it a distinguishable measurement: `decoded` counts corrupt cells from
    // every route, and only this one is a count rather than a floor.
    let n = cratonvm_types::cell_census::note_decoded();
    cratonvm_types::cell_census::note_array_receiver();
    ARRAY_RECEIVER_FIELD_ACCESSES.fetch_add(1, Ordering::Relaxed);
    let length = header.array_length() as usize;
    let element_type = header.element_type();
    let cell_off = HEADER_SIZE + index.saturating_mul(SLOT_SIZE);
    let body_end = cratonvm_types::array_data_size_checked(length, element_type)
        .and_then(|d| ARRAY_DATA_OFFSET.checked_add(d));
    let in_body = matches!(body_end, Some(end) if cell_off.saturating_add(16) <= end);
    let (raw0, raw1) = if in_body {
        // SAFETY: the whole 16-byte cell lies inside this array's own body,
        // which the caller's live allocation covers. Per-word atomic so the
        // diagnostic cannot tear against a concurrent element store.
        unsafe {
            let p = obj_ptr as *const u8;
            (
                (*(p.add(cell_off) as *const std::sync::atomic::AtomicU64)).load(Ordering::Relaxed),
                (*(p.add(cell_off + 8) as *const std::sync::atomic::AtomicU64))
                    .load(Ordering::Relaxed),
            )
        }
    } else {
        (0, 0)
    };
    corrupt_cell_record(obj_ptr.wrapping_add(cell_off), raw0, raw1);
    if n < 32 || gc_flags().diag_hib32 {
        tracing::error!(
            target: "cratonvm::gc::guard",
            obj = ?(obj_ptr as *const u8),
            index,
            length,
            element_type = ?element_type,
            class_id = ?header.class_id,
            raw0 = format!("{raw0:#018x}"),
            raw1 = format!("{raw1:#018x}"),
            in_body,
            "{site}: plain-object field access on an ARRAY receiver — refusing \
             rather than striding packed element data as 16-byte Value cells. \
             An array mirrors its LENGTH into num_slots, so the index check \
             admits this; the byte offset it computes does not name the element \
             it claims to and frequently leaves the allocation. The caller \
             identified this receiver by class id without a kind check — see \
             `is_plain_string` for the rule.",
        );
    }
}

/// Read a legacy 16-byte `Value` cell, screening the discriminant first.
///
/// # why this is not `read_value_atomic`
///
/// `gen_heap::read_slot` has validated the discriminant since `HIB-CV-32`; the
/// other three legacy-cell readers did not — `heap::read_slot` and
/// `g1::get_field` used the unchecked `read_value_atomic`, and `zgc::get_field`
/// used a bare non-atomic `std::ptr::read`. That asymmetry is not a style
/// difference, it decides whether a heap reference-integrity defect is
/// *reported* or *fatal*, and it is why one defect presented as two unrelated
/// outcomes:
///
/// `JsonMarshallerTests` reads a `String.value` field through a stale receiver
/// whose memory has been swept and handed to something else, so the cell holds
/// two heap pointers (`raw0=0x…3c50a5d8 raw1=0x…3c7218e0`) rather than a
/// `(tag, payload)` pair. Generational screened it, returned null, logged, and
/// the class passed 17/17. ZGC and G1 transmuted it and handed the result to a
/// `match`, whose jump-table load is `[table + disc*4]` with no bounds check
/// because Rust guarantees an in-range discriminant — so the low word of a heap
/// pointer became the index. The faulting address is exactly that arithmetic:
/// `r10=0x00007FF7EEFFB914` (table) `+ rax=0x440A7D38` (the bogus discriminant)
/// `* 4 = 0x00007FF8FF29ADF4`, the address in the SIGSEGV report.
///
/// Screening here does NOT fix the producer — the stale receiver is a separate,
/// still-open defect that is present under Generational too, where this guard is
/// the only reason its green looks clean. What it fixes is that the same corrupt
/// cell must not be a localizable diagnostic on one collector and an
/// unrecoverable crash on the others.
///
/// # Safety
/// The pointer must be valid, readable for 16 bytes, and 8-byte aligned.
pub unsafe fn read_value_cell_checked(ptr: *const Value, site: &'static str) -> Value {
    match cratonvm_types::read_value_checked_atomic(ptr) {
        Some(v) => v,
        None => {
            let n = cratonvm_types::cell_census::note_decoded();
            // SAFETY: caller contract - 16 readable, 8-byte-aligned bytes.
            corrupt_cell_record(
                ptr as usize,
                (*(ptr as *const std::sync::atomic::AtomicU64)).load(Ordering::Relaxed),
                (*((ptr as *const u8).add(8) as *const std::sync::atomic::AtomicU64))
                    .load(Ordering::Relaxed),
            );
            if n < 32 || gc_flags().diag_hib32 {
                // SAFETY: caller contract — 16 readable, 8-byte-aligned bytes.
                // Read atomically per word so the diagnostic itself cannot tear
                // against a concurrent plain writer (PLAIN-SLOT TEARING FIX).
                let raw0 = (*(ptr as *const std::sync::atomic::AtomicU64)).load(Ordering::Relaxed);
                let raw1 = (*((ptr as *const u8).add(8) as *const std::sync::atomic::AtomicU64))
                    .load(Ordering::Relaxed);
                tracing::error!(
                    target: "cratonvm::gc::guard",
                    slot = ?ptr,
                    raw0 = format!("{raw0:#018x}"),
                    raw1 = format!("{raw1:#018x}"),
                    "{site}: corrupt Value cell (out-of-range discriminant) — \
                     returning null instead of a UB-on-match Value. Heap \
                     reference-integrity defect (see HIB-CV-32).",
                );
            }
            Value::Object(None)
        }
    }
}

/// Write a `Value` into a slot.
///
/// See [`read_slot`]'s PLAIN-SLOT TEARING FIX note — delegates to the same
/// already-proven `write_value_atomic` helper so a concurrent plain reader
/// on another mutator thread can never observe a torn (tag, payload) pair.
///
/// # Safety
/// The pointer must be valid and 8-byte aligned.
unsafe fn write_slot(ptr: *mut u8, value: Value) {
    cratonvm_types::write_value_atomic(ptr as *mut Value, value);
}

// ---------------------------------------------------------------------------
// Compact primitive array element access
// ---------------------------------------------------------------------------

// --- Reference-element decode chokepoint for `read_prim_element` ------------
//
// WHY THIS EXISTS. `read_prim_element`'s `Reference` arm ended commit
// `6a04b0e3c1` (2026-06-29, "degrade stale references to null at every decode
// boundary") applying `cratonvm_types::plausible_heap_pointer` to the loaded
// word and, on failure, returning `Value::Object(None)` — handing Java a `null`
// where the slot held bits. That commit's own message records what the filter
// is: defense-in-depth for an *unfixed* GC defect (live blocked-thread frame
// objects swept by the non-moving young sweep; observed as 8000+ all-zero-header
// stale `Thread` receivers, then `0x77..` / `": contex"` buffer bytes read back
// through this arm), not an integrity guard on trusted data. Two failure modes
// hid behind the bare `else`:
//
// (1) THE DEGRADE WAS SILENT — and its interpreter twin is not. The same commit
//     put the same filter on `cratonvm_types`' own decode paths (`decode_value`
//     `VTAG_OBJECT`, `CompactValue::to_value` SUB_OBJECT), where it feeds a
//     process-wide counter and a one-shot stderr line (`note_object_degradation`,
//     types/src/compact_value.rs:330, read back via the `pub`
//     `cratonvm_types::compact_value::object_degradation_count`). THIS arm fed
//     nothing at all. `read_prim_element` is the array-element read for EVERY
//     collector — `Heap::get_array_element` / `get_array_element_unboxing` above,
//     `GenerationalHeap`'s twins (gc/src/gen_heap.rs:4072, :4092, :3518) and
//     `zgc.rs:2601` all funnel through it — so on the DEFAULT (Generational)
//     collector a live `Object[]` element could be nulled with no trace
//     anywhere: no counter, no log, no assert, and `object_degradation_count()`
//     reading a reassuring 0. That is an observability bug today, independent of
//     ZGC, and [`ref_element_degradation_count`] closes it.
//
//     NULL IS NOT A DEGRADATION. `plausible_heap_pointer(0)` is `false`, so an
//     ordinary null element — by far the common case for `Object[]` — reaches
//     the cold arm too. Counting it would put the counter in the millions on a
//     clean run and make "non-zero means a live object was nulled" false on
//     first use. That test is load-bearing, not a micro-optimisation, and it
//     now lives in the shared sink
//     (`cratonvm_types::compact_value::note_ref_word_degradation`) rather than
//     in this file — see [`ref_element_degradation_count`].
//
// (2) A ZGC COLORED WORD IS NOT CORRUPTION, and must never take the degrade.
//     `gc/src/zgc/vaddr.rs` sets bit 63 (`Z_COLORED_TAG`) on every non-null
//     colored word *precisely so* it fails `plausible_heap_pointer`'s 47-bit
//     test and is caught loudly rather than dereferenced as a wild pointer. The
//     bare `else` did the exact opposite of loud: it converted "the load barrier
//     has not run on this word" into a null handed to Java, i.e. an NPE or a
//     silently dropped store at an arbitrary point far from the cause. Under a
//     relocating collector that is silent heap corruption, which
//     `docs/feature-designs/zgc-jit-load-barrier.md` (risk J1) rates worse than
//     a clean SIGSEGV; `docs/feature-designs/zgc-reference-slot-representation.md`
//     names this arm as the most dangerous unmigrated read in the tree, and
//     `gc/src/zgc/census.rs` reads raw words rather than call it for exactly
//     this reason.
//
//     The fix is NOT to weaken `plausible_heap_pointer` — both studies say so
//     explicitly — it is to make the caller barrier the word first. Until that
//     lands, a *structurally well-formed* colored word is an invariant violation
//     and fails loudly instead of fabricating a null. Genuine garbage keeps the
//     old degrade: `0x8D8D8D8D8D8D8D8D` has bit 63 set but fails
//     `vaddr::is_well_formed` (which additionally demands bits 62-46 clear and
//     exactly one metadata bit), so a `--features zgc` build running
//     Generational or G1 behaves as before.

/// Read the process-wide count of **non-null** array reference elements
/// [`read_prim_element`] degraded to `null` because they failed
/// [`cratonvm_types::plausible_heap_pointer`].
///
/// The read-side companion to [`COMPACT_OOP_MAP_MISSING`]: that one is a
/// marking FAIL-OPEN, this one is a read FAIL-SILENT. Non-zero means Java was
/// handed `null` for a slot that held bits — i.e. the GC root-coverage gap
/// recorded in commit `6a04b0e3c1` is live in this run. Expected to be ZERO;
/// zero is the only good value.
///
/// # This is one slot of the shared counter, not a private one
///
/// It used to be a `pub static REF_ELEMENT_DEGRADATIONS: AtomicU64` in this
/// file, because the sink it belonged in
/// (`cratonvm_types::compact_value::note_object_degradation`) was `pub`
/// and this crate could not reach it. That produced the exact failure the
/// counter exists to prevent: a triager reading
/// `cratonvm_types::compact_value::object_degradation_count()` saw a reassuring
/// `0` while this path was nulling live elements, because the total did not
/// include them. The sink is now `pub` and source-tagged, so this reads
/// [`DegradationSource::ArrayElement`]'s slot of the one process-wide table and
/// the total genuinely totals. `vm::jit::helpers::jit_ref_degradation_count`
/// (the `Jit` slot) was merged the same way.
///
/// Advisory and `Relaxed`: it carries no happens-before relationship with the
/// slot it counts.
#[inline]
pub fn ref_element_degradation_count() -> u64 {
    cratonvm_types::compact_value::object_degradation_count_from(
        cratonvm_types::compact_value::DegradationSource::ArrayElement,
    )
}

/// Decode a raw reference word read out of an array element slot into the
/// `Value` the interpreter expects, degrading a provably-impossible pointer to
/// `Value::Object(None)`.
///
/// The fast path is byte-for-byte the predicate the `Reference` arm used before
/// this chokepoint existed: `plausible_heap_pointer(raw)` and nothing else.
/// Everything new lives in the `#[cold]`, `#[inline(never)]` callee, which is
/// only reached once that predicate has *already* failed — so this costs
/// nothing per element read on any build or any collector. See the block
/// comment above for why the callee exists.
///
/// # Safety
/// Nothing beyond `ObjectRef::from_raw`'s contract, and `raw` has passed
/// [`cratonvm_types::plausible_heap_pointer`] before it is wrapped.
#[inline(always)]
unsafe fn decode_ref_element_word(raw: u64) -> Value {
    if cratonvm_types::plausible_heap_pointer(raw) {
        Value::Object(Some(ObjectRef::from_raw(raw as usize as *mut u8)))
    } else {
        ref_element_word_implausible(raw)
    }
}

/// Cold arm of [`decode_ref_element_word`]: the word cannot be a live heap
/// pointer. Separates the three reasons a word lands here, which the previous
/// bare `else { Value::Object(None) }` conflated into one silent answer:
///
/// * **`raw == 0`** — an ordinary null element. Not a degradation, not counted;
///   the slot said null and the caller gets null. See the block comment above:
///   counting this would destroy the counter's meaning on the first clean run.
///   The test is no longer written here: it lives inside
///   [`cratonvm_types::compact_value::note_ref_word_degradation`], which exists
///   because this arm and the JIT's twin each had to discover the rule
///   separately. Stated once, where the next raw-word path cannot miss it.
/// * **A structurally well-formed ZGC colored word** — legitimate data that has
///   simply not been through the load barrier. Nulling it is the silent-null
///   corruption of `zgc-jit-load-barrier.md` J1; returning the colored word is a
///   wild-pointer deref. Neither is acceptable, so this is a hard failure that
///   names the missing barrier.
/// * **Stale/garbage bits** (the `0x8D8D..` class from commit `6a04b0e3c1`) —
///   genuinely not a pointer. Keeps the existing degrade-to-null contract,
///   now counted.
///
/// The ZGC arm is `#[cfg(feature = "zgc")]`, so a default build does not merely
/// behave identically — the branch is not compiled. `crate::zgc` is itself
/// `#[cfg(feature = "zgc")]` in `gc/src/lib.rs`, so the path is only nameable
/// under that cfg.
#[cold]
#[inline(never)]
pub fn ref_element_word_implausible(raw: u64) -> Value {
        {
        // TODO(zgc): once the load barrier runs AHEAD of this decode (see the
        // TODO in `read_prim_element`'s `Reference` arm) this branch becomes
        // unreachable, because the word arriving here will already be a plain
        // address. The barrier entry point is
        // `crate::zgc::barrier::z_load(slot: &std::sync::atomic::AtomicU64,
        // ctx: &C) -> u64` where `C: crate::zgc::barrier::ZBarrierContext +
        // ?Sized` (gc/src/zgc/barrier.rs, `pub fn z_load`, line 1233 as of
        // 2026-08-07). It must be applied to the element SLOT before any
        // plausibility test, and the test must then ask about the barrier's
        // UNMASKED address, never about the colored word. Do not weaken
        // `plausible_heap_pointer` to admit colored words. Keep this panic as
        // the tripwire for a caller that was missed.
        if z_is_colored_word_well_formed(raw) {
            panic!(
                "ZGC colored word {raw:#018x} reached `read_prim_element`'s \
                 Reference arm with no load barrier: bit 63 (Z_COLORED_TAG) is \
                 set by gc/src/zgc/vaddr.rs, so this word is a legitimate \
                 reference that has not been unmasked, not corruption. Barrier \
                 it via crate::zgc::barrier::z_load and plausibility-check the \
                 UNMASKED address; do not degrade it to null (that is the silent \
                 heap corruption of zgc-jit-load-barrier.md J1) and do not weaken \
                 plausible_heap_pointer to admit it."
            );
        }
    }
    // Counts the event and emits the one-shot "array-element" diagnostic, or
    // returns `false` and does neither for `raw == 0`. See
    // `ref_element_degradation_count` for why this is the shared table's
    // `ArrayElement` slot rather than a static in this file.
    cratonvm_types::compact_value::note_ref_word_degradation(
        raw,
        cratonvm_types::compact_value::DegradationSource::ArrayElement,
        "gc::heap::read_prim_element",
    );
    Value::Object(None)
}

// ---------------------------------------------------------------------------
// Primitive element cells: relaxed atomics, not plain accesses
// ---------------------------------------------------------------------------
//
// gc-common w3-e (2026-09-23), `common-g-prim-array-element-plain-accesses`.
// Two Java threads racing on `a[i]` of a shared `int[]` is legal Java (a data
// race with DEFINED outcomes: no out-of-thin-air values, no tearing of `int`,
// and no tearing of `long`/`double` on every JVM that matters). Served with
// `ptr::read_unaligned` / `ptr::write_unaligned` it was two unsynchronised
// non-atomic Rust accesses to one location -- undefined behaviour in the Rust
// memory model whatever the hardware does, and an optimiser licence to split
// or re-load the access. The reference arm already went through
// `read_ref_slot` / `write_ref_slot` (relaxed atomics) and the instance-field
// cells through `read_value_checked_atomic`; these helpers give the primitive
// arms the same treatment.
//
// A relaxed load/store of the element's own width compiles to the same single
// `mov` / `ldr` / `str` on x86-64 and AArch64, so the hot path is unchanged.
// Every production `base` is `obj + ARRAY_DATA_OFFSET` (16) of an 8-aligned
// object, so each element is naturally aligned; the alignment test is a
// well-predicted branch kept so that a future caller with an odd `base` gets
// the old plain access instead of a misaligned atomic (which would be UB of a
// worse kind).

#[inline(always)]
unsafe fn load_u8_relaxed(p: *const u8) -> u8 {
    (*(p as *const std::sync::atomic::AtomicU8)).load(Ordering::Relaxed)
}

#[inline(always)]
unsafe fn store_u8_relaxed(p: *mut u8, v: u8) {
    (*(p as *const std::sync::atomic::AtomicU8)).store(v, Ordering::Relaxed)
}

#[inline(always)]
unsafe fn load_u16_relaxed(p: *const u8) -> u16 {
    if (p as usize) & 1 == 0 {
        (*(p as *const std::sync::atomic::AtomicU16)).load(Ordering::Relaxed)
    } else {
        std::ptr::read_unaligned(p as *const u16)
    }
}

#[inline(always)]
unsafe fn store_u16_relaxed(p: *mut u8, v: u16) {
    if (p as usize) & 1 == 0 {
        (*(p as *const std::sync::atomic::AtomicU16)).store(v, Ordering::Relaxed)
    } else {
        std::ptr::write_unaligned(p as *mut u16, v)
    }
}

#[inline(always)]
unsafe fn load_u32_relaxed(p: *const u8) -> u32 {
    if (p as usize) & 3 == 0 {
        (*(p as *const std::sync::atomic::AtomicU32)).load(Ordering::Relaxed)
    } else {
        std::ptr::read_unaligned(p as *const u32)
    }
}

#[inline(always)]
unsafe fn store_u32_relaxed(p: *mut u8, v: u32) {
    if (p as usize) & 3 == 0 {
        (*(p as *const std::sync::atomic::AtomicU32)).store(v, Ordering::Relaxed)
    } else {
        std::ptr::write_unaligned(p as *mut u32, v)
    }
}

#[inline(always)]
unsafe fn load_u64_relaxed(p: *const u8) -> u64 {
    if (p as usize) & 7 == 0 {
        (*(p as *const AtomicU64)).load(Ordering::Relaxed)
    } else {
        std::ptr::read_unaligned(p as *const u64)
    }
}

#[inline(always)]
unsafe fn store_u64_relaxed(p: *mut u8, v: u64) {
    if (p as usize) & 7 == 0 {
        (*(p as *const AtomicU64)).store(v, Ordering::Relaxed)
    } else {
        std::ptr::write_unaligned(p as *mut u64, v)
    }
}

/// Read an array element from compact storage.
///
/// Primitive elements are read with a relaxed atomic load of their own width
/// (see the block comment above `load_u8_relaxed`); references with
/// `read_ref_slot`.
///
/// # Safety
/// `base` must point to the start of the array data area (header + HEADER_SIZE).
/// `index` must have been validated against the array length by the caller.
#[inline]
pub unsafe fn read_prim_element(base: *mut u8, index: usize, et: ArrayElementType) -> Value {
    // Use a helper to compute byte offset with overflow checking.
    // `checked_mul(..).expect(..)` panics on overflow in every build -- the
    // caller MUST bounds-check `index` against the array length before
    // calling this function.
    macro_rules! elem_ptr {
        ($base:expr, $index:expr, $stride:expr) => {{
            let offset = $index
                .checked_mul($stride)
                .expect("array element offset overflow");
            $base.add(offset) as *const u8
        }};
    }

    match et {
        ArrayElementType::Int => Value::Int(load_u32_relaxed(elem_ptr!(base, index, 4)) as i32),
        ArrayElementType::Long => Value::Long(load_u64_relaxed(elem_ptr!(base, index, 8)) as i64),
        ArrayElementType::Float => {
            Value::Float(f32::from_bits(load_u32_relaxed(elem_ptr!(base, index, 4))))
        }
        ArrayElementType::Double => {
            Value::Double(f64::from_bits(load_u64_relaxed(elem_ptr!(base, index, 8))))
        }
        ArrayElementType::Byte => {
            let v = load_u8_relaxed(base.add(index));
            Value::Int(v as i8 as i32) // sign-extend
        }
        ArrayElementType::Boolean => {
            let v = load_u8_relaxed(base.add(index));
            Value::Int(v as i32) // zero-extend (0 or 1)
        }
        ArrayElementType::Char => Value::Int(load_u16_relaxed(elem_ptr!(base, index, 2)) as i32),
        ArrayElementType::Short => {
            Value::Int(load_u16_relaxed(elem_ptr!(base, index, 2)) as i16 as i32)
        }
        ArrayElementType::Reference => {
            let offset = index
                .checked_mul(ref_element_size())
                .expect("array ref element offset overflow");
            // TODO(zgc): this is a raw reference-array element load — Category A
            // in `docs/feature-designs/zgc-jit-load-barrier.md` §2.3, and entry
            // 13 of `zgc-reference-slot-representation.md`'s migration table.
            // Under `VmHeap::Zgc` the word must go through
            // `crate::zgc::barrier::z_load(slot: &std::sync::atomic::AtomicU64,
            // ctx: &C)` (gc/src/zgc/barrier.rs, `pub fn z_load`, line 1233 as of
            // 2026-08-07) HERE — between `read_ref_slot` and the decode below —
            // and the plausibility test inside `decode_ref_element_word` must
            // then be applied to the barrier's unmasked address rather than to
            // the colored word. Until that lands,
            // `ref_element_word_implausible` fails loudly on a well-formed
            // colored word instead of nulling it.
            let raw: u64 = read_ref_slot(base.add(offset));
            // Defense-in-depth reference-slot decode. Mirrors the VTAG_OBJECT
            // degrade in `cratonvm_types::decode_value` (operand/local SoA path)
            // and `CompactValue::to_value`'s SUB_OBJECT plausibility gate, which
            // this hot heap-read path previously bypassed by wrapping ANY
            // non-zero bits verbatim. A non-null reference slot whose bits are
            // unaligned, inside the null-guard page, or outside the 47-bit
            // user-address range is provably NOT a live object pointer.
            // Fabricating an `ObjectRef` from such bits SIGSEGVs on its next
            // deref, or panics in `CompactValue::object` for >47-bit bits.
            // Degrade to null instead — the field/array read callers already
            // normalise `Object(None)`, matching HotSpot's "you get a null, not
            // a VM crash". Pure bit ops (no heap probe), so it is safe on this
            // hot path and never rejects a valid pointer (every real object is
            // 8-aligned, above the guard page, <=47-bit). The degrade is now
            // COUNTED rather than silent — see the chokepoint block comment
            // above `ref_element_degradation_count` for the failure that closes.
            decode_ref_element_word(raw)
        }
    }
}

/// gcstress residual face-1 hunt (`CRATONVM_DBG_WATCH_CELL=<hex addr>`) — a
/// software write-watch over the heap write primitives. Every instrumented
/// writer calls [`cell_watch_check`] with its destination range; a range that
/// covers the watched address prints the site, the range, and a backtrace.
/// Unlike a DR hardware watchpoint this covers BULK copies (range overlap)
/// and needs no per-thread arming. `0` = disabled (single cached load + cmp
/// on the hot paths).
#[inline]
pub fn cell_watch_addr() -> usize {
    crate::gc_flags().dbg_watch_cell
}

/// ES-FAIL-FAMILY-20260710 hunt: a RUNTIME-settable companion to
/// [`cell_watch_addr`] (which only reads a fixed address from the
/// `CRATONVM_DBG_WATCH_CELL` env var at process start). Some hunts don't know
/// the address to watch until the program has already run for a while (e.g.
/// "watch the `cause` slot of the next `BufferUnderflowException` this
/// constructs" — the address is only known once that object is allocated).
/// `set_dynamic_watch` lets native code (via a `NativeContext` hook) arm the
/// watch mid-run; [`cell_watch_check`] checks both addresses. `0` = disabled.
static DYNAMIC_WATCH: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// See [`DYNAMIC_WATCH`].
#[inline]
pub fn set_dynamic_watch(addr: usize) {
    DYNAMIC_WATCH.store(addr, std::sync::atomic::Ordering::SeqCst);
}

/// `CRATONVM_DBG_MARK_WHY_CLASS=<internal/class/Name>` — the address whose
/// young-mark REASON should be reported. Armed at runtime.
///
/// "Why is this object still alive after a collection that should have
/// reclaimed it" is not answerable from outside the marker. Every root source
/// can be eliminated one at a time — the `TestDefaultInstanceManager` chain has
/// now done that four times — and still leave the question open, because the
/// retaining edge may be a SIDE TABLE (`loader_pin`, `mirror_pin`,
/// `metadata_pin`, an overlay owner edge). Those are invisible to a referrer
/// walk, absent from the root vector, and followed only inside the marker.
///
/// The old-gen BFS already labels each such edge (`mark_and_push_old_gen`'s
/// `reason`). The young precise marker did not — so on the `System.gc()` path,
/// which is exactly the non-moving young sweep, nothing recorded WHICH edge did
/// the marking. That asymmetry is why the question kept being answered by
/// elimination instead of by evidence.
///
/// Armed from `vm_object`'s `add_mirror_pin` hook: the interesting address (a
/// JSP `ClassLoader`) is not known until its class is defined. Snapshotted into
/// `YoungMarkCtx` once per collection, so the per-edge cost is a compare
/// against a struct field, not an atomic load. `0` = disabled.
static YOUNG_MARK_WATCH: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// See [`YOUNG_MARK_WATCH`].
#[inline]
pub fn set_young_mark_watch(addr: usize) {
    YOUNG_MARK_WATCH.store(addr, std::sync::atomic::Ordering::SeqCst);
}

/// See [`YOUNG_MARK_WATCH`].
///
/// `CRATONVM_DBG_YOUNG_MARK_WATCH=1` arms it at address 1 instead, which is
/// below every from-space base and so satisfies the `w != 0 && w >= from_base`
/// gate on the MARKWHY *census* blocks without naming a victim. Those blocks
/// print per-cycle totals — the disposition tally and the `sweep done` line —
/// and answer the one question a watch address is not needed for: when
/// `dead_regions` comes out EMPTY, was everything classified live, or were
/// spans collected and then unwound? Arming previously required the
/// `add_mirror_pin` hook to have seen an interesting `ClassLoader`, so on a
/// workload with no such object the census could not be reached at all.
///
/// The per-object watch paths keep their own `w == addr` compares, so an
/// address of 1 never matches one and this cannot forge a per-object report.
#[inline]
pub fn young_mark_watch() -> usize {
    let w = YOUNG_MARK_WATCH.load(std::sync::atomic::Ordering::Relaxed);
    if w != 0 {
        return w;
    }
    static ENV: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *ENV.get_or_init(|| {
        match None::<std::ffi::OsString> {
            Some(raw) => {
                let s = raw.to_string_lossy().trim().to_ascii_lowercase();
                if s.is_empty() || s == "0" {
                    0
                } else if let Some(hex) = s.strip_prefix("0x") {
                    usize::from_str_radix(hex, 16).unwrap_or(1)
                } else {
                    s.parse::<usize>().unwrap_or(1)
                }
            }
            None => 0,
        }
    })
}

/// See [`DYNAMIC_WATCH`].
#[inline]
pub fn dynamic_watch_addr() -> usize {
    DYNAMIC_WATCH.load(std::sync::atomic::Ordering::SeqCst)
}

/// See [`cell_watch_addr`]. `extra` carries the value/context being written.
#[inline]
pub fn cell_watch_check(dst: usize, len: usize, site: &str, extra: &dyn std::fmt::Debug) {
    let env_w = cell_watch_addr();
    let dyn_w = dynamic_watch_addr();
    for w in [env_w, dyn_w] {
        if w != 0 && dst <= w && w.wrapping_sub(dst) < len {
            eprintln!(
                "[CELLWATCH] {site}: write [{dst:#x} +{len}) covers watch {w:#x} value={extra:?}\n{}",
                std::backtrace::Backtrace::force_capture(),
            );
        }
    }
}

/// Write an array element to compact storage.
///
/// Primitive elements are written with a relaxed atomic store of their own
/// width (see the block comment above `load_u8_relaxed`); references with
/// `write_ref_slot`.
///
/// # Safety
/// `base` must point to the start of the array data area (header + HEADER_SIZE).
#[inline]
pub unsafe fn write_prim_element(base: *mut u8, index: usize, et: ArrayElementType, value: Value) {
    // gcstress face-1 hunt (no-op unless CRATONVM_DBG_WATCH_CELL is set).
    if cell_watch_addr() != 0 {
        let width: usize = match et {
            ArrayElementType::Byte | ArrayElementType::Boolean => 1,
            ArrayElementType::Char | ArrayElementType::Short => 2,
            ArrayElementType::Int | ArrayElementType::Float => 4,
            // gcj j1/cm: the store below strides `ref_element_size()` (4
            // under compressed oops), so the watch must too.
            ArrayElementType::Reference => ref_element_size(),
            _ => 8,
        };
        cell_watch_check(
            base as usize + index * width,
            width,
            "write_prim_element",
            &value,
        );
    }
    match et {
        ArrayElementType::Int => {
            let v = match value {
                Value::Int(i) => i,
                _ => 0,
            };
            store_u32_relaxed(base.add(index * 4), v as u32);
        }
        ArrayElementType::Long => {
            let v = match value {
                Value::Long(l) => l,
                _ => 0,
            };
            store_u64_relaxed(base.add(index * 8), v as u64);
        }
        ArrayElementType::Float => {
            let v = match value {
                Value::Float(f) => f,
                _ => 0.0,
            };
            store_u32_relaxed(base.add(index * 4), v.to_bits());
        }
        ArrayElementType::Double => {
            let v = match value {
                Value::Double(d) => d,
                _ => 0.0,
            };
            store_u64_relaxed(base.add(index * 8), v.to_bits());
        }
        ArrayElementType::Byte => {
            let v = match value {
                Value::Int(i) => i as u8,
                _ => 0,
            };
            store_u8_relaxed(base.add(index), v);
        }
        // JVMS `bastore`: a store into a *boolean* array narrows to one bit
        // (`value & 1`), not to a byte — the opcode serves `byte[]` and
        // `boolean[]` both, and the verifier permits either. `read_prim_element`
        // zero-extends whatever is here, so a truncated 2 reads back as 2 and
        // tests `true`, where HotSpot stores 0 and tests `false`. Reachable via
        // hand-written `bastore` on a `boolean[]` and via
        // `Unsafe.putByte`/`putBoolean`; not via javac output.
        ArrayElementType::Boolean => {
            let v = match value {
                Value::Int(i) => (i & 1) as u8,
                _ => 0,
            };
            store_u8_relaxed(base.add(index), v);
        }
        ArrayElementType::Char => {
            let v = match value {
                Value::Int(i) => i as u16,
                _ => 0,
            };
            store_u16_relaxed(base.add(index * 2), v);
        }
        ArrayElementType::Short => {
            let v = match value {
                Value::Int(i) => i as i16,
                _ => 0,
            };
            store_u16_relaxed(base.add(index * 2), v as u16);
        }
        ArrayElementType::Reference => {
            let raw: u64 = match value {
                Value::Object(Some(r)) => r.as_ptr() as u64,
                Value::Object(None) => 0,
                _ => 0,
            };
            write_ref_slot(base.add(index * ref_element_size()), raw);
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;

/// [`install_gap_filler`] without the [`PAR_EVAC_FILLER_BYTES`] tally.
///
/// gengc-round1 2026-09-20. Split out for the one other caller that needs a
/// walkable dead-object stamp but is NOT a to-space buffer tail: the
/// forwarding-CAS loser abandoning a copy it had already promoted (see
/// [`PAR_EVAC_ABANDONED_OLD_BYTES`]). Folding those bytes into `filler_bytes`
/// would make "bytes lost to PLAB tail fillers" mean two different things, and
/// the whole point of that counter is that it answers one question.
///
/// gen r4w5/pinned5 (2026-09-24): `pub` for the third such caller,
/// the pinned in-place young copy, which stamps the sub-`HEADER_SIZE` dead
/// slivers it cannot free-list (`GenerationalHeap::finish_in_place_young_cycle`)
/// and must not fold them into this module's PLAB filler census either.
///
/// # Safety
/// The span must be 8-aligned, a non-zero multiple of 8, writable, dead, and
/// owned by the caller.
pub unsafe fn write_filler(addr: usize, size: usize) {
    debug_assert!(
        addr % 8 == 0 && size % 8 == 0 && size > 0,
        "filler span must be an 8-aligned, non-zero multiple of 8 (addr={addr:#x} size={size})",
    );
    if size == 0 || size % 8 != 0 {
        return;
    }
    if size < HEADER_SIZE {
        // Too small for a header. The 8-byte sentinel: class id at +0, exact
        // gap length at +4. Zeroing instead would be byte-identical to a live
        // `new Object()` and desync every linear walk (Bug-D).
        // SAFETY: the span is at least 8 bytes and 8-aligned.
        unsafe {
            std::ptr::write(addr as *mut u32, crate::tlab::GAP_FILLER_CLASS_ID.as_u32());
            std::ptr::write((addr + 4) as *mut u32, size as u32);
        }
        return;
    }
    // gci i41/ev: a span too large for ONE `int[]` (its length is a `u32`
    // header field, and a walker refuses a length past `i32::MAX` as a
    // corrupt header) is tiled with several, back to back. Only spans above
    // ~8 GiB take the loop; every filler this collector stamps today is far
    // smaller, so the common path is the single `int[]` below unchanged.
    let (mut addr, mut size) = (addr, size);
    loop {
        let chunk = filler_chunk_len(size);
        // SAFETY: `[addr, addr + chunk)` is a prefix of the caller's span,
        // 8-aligned, a multiple of 8 and at least `HEADER_SIZE` bytes
        // (`filler_chunk_len`'s contract).
        unsafe { write_int_filler(addr, chunk) };
        if chunk == size {
            return;
        }
        addr += chunk;
        size -= chunk;
    }
}


#[inline]
fn z_is_colored_word_well_formed(word: u64) -> bool {
    // ZGC colored pointer check: bit 63 (`Z_COLORED_TAG`) set, metadata bits
    // 42..=45 (`Z_METADATA_MASK`) with exactly one bit set, and reserved bits
    // 46..=62 (`Z_RESERVED_MBZ_MASK`) clear. Matches `gc_zgc::zgc::vaddr::is_well_formed`.
    const Z_COLORED_TAG: u64 = 1u64 << 63;
    const Z_METADATA_BITS: u64 = 0xF << 42;
    const Z_RESERVED_MBZ_MASK: u64 = 0x7FFF_C000_0000_0000;
    let meta = (word & Z_METADATA_BITS) >> 42;
    (word & Z_COLORED_TAG) != 0 && (word & Z_RESERVED_MBZ_MASK) == 0 && meta.count_ones() == 1
}

/// gci i41/ev — how much of a `size`-byte span (a multiple of 8, at least
/// `HEADER_SIZE`) the FIRST filler of [`write_filler`]'s tiling covers: all
/// of it when one `int[]` can, else one maximal `int[]`, shortened by one
/// header when the rest would be too short to hold a header of its own. The
/// result is a multiple of 8, at least `HEADER_SIZE`, and leaves either
/// nothing or at least `HEADER_SIZE` bytes. Pure.
///
/// Before i41 the length was `(data_bytes / 4) as u32`, which wrapped for a
/// span of 16 GiB and named an `int[]` longer than `i32::MAX` from 8 GiB: a
/// walker then sized the filler wrongly (or refused it as corrupt) and
/// desynced on everything behind it.
pub fn filler_chunk_len(size: usize) -> usize {
    let data = size.saturating_sub(HEADER_SIZE) as u64;
    if data <= MAX_FILLER_DATA_BYTES {
        return size;
    }
    let full = HEADER_SIZE + MAX_FILLER_DATA_BYTES as usize;
    if size - full < HEADER_SIZE {
        full - HEADER_SIZE
    } else {
        full
    }
}

/// One reference-free `int[]` of exactly `size` bytes at `addr`.
///
/// # Safety
/// `size` is a multiple of 8, at least `HEADER_SIZE`, with a payload of at
/// most [`MAX_FILLER_DATA_BYTES`]; the span is 8-aligned, writable, dead and
/// owned by the caller.
pub unsafe fn write_int_filler(addr: usize, size: usize) {
    let data_bytes = size - HEADER_SIZE;
    debug_assert_eq!(data_bytes % 8, 0, "filler payload must stay 8-aligned");
    debug_assert!(data_bytes as u64 <= MAX_FILLER_DATA_BYTES);
    let header = ObjectHeader::new(
        crate::tlab::TLAB_FILLER_CLASS_ID,
        ObjectKind::Array,
        ArrayElementType::Int,
        (data_bytes / 4) as u32,
        0,
    );
    // SAFETY: the span is at least `HEADER_SIZE` bytes and 8-aligned.
    unsafe {
        std::ptr::write(addr as *mut ObjectHeader, header);
        if data_bytes > 0 {
            std::ptr::write_bytes((addr + HEADER_SIZE) as *mut u8, 0, data_bytes);
        }
    }
}

/// Compute total size of a heap object (for GC cursor advancement). Objects
/// use SLOT_SIZE per field. Arrays use compact element sizes.
///
/// A malformed `array_length` makes `array_data_size` overflow/fail. Rather
/// than silently returning a 0-byte payload (`HEADER_SIZE`, which advances a
/// scan cursor by the wrong amount and mis-parses subsequent memory), this
/// returns `0` — a value below `HEADER_SIZE`. Every caller already either
/// breaks the walk on `total_size < HEADER_SIZE` or rejects it with a
/// corrupt-header size check (`forward_object_impl`'s `total_size <
/// HEADER_SIZE || !fits_in_arena`, `gc.rs`'s `MAX_SANE_OBJECT_SIZE`), so a bad
/// array length is surfaced as corruption instead of corrupting the cursor.
///
/// (This doc was stranded above `gcw_enabled` until gen r4w2, 2026-09-23. It
/// used to cite a `MAX_SANE_OBJECT_SIZE` guard in `forward_object`, which has
/// been the `fits_in_arena` test for a long time; `gc.rs` still says so.)
///
/// PERF: called once per object by every linear collector walk, and at 41 call
/// sites in this module. A depth-18 `BinTreesClassic` profile put this at 22.1%
/// of samples — not because the arithmetic is expensive (a few loads, a branch
/// and an add) but because the `tracing::warn!` expansions inline into it made
/// the function large enough that LLVM declined to inline it, so every object in
/// the walk paid a real call. The diagnostics now live in `#[cold]` helpers and
/// this is `#[inline]`.
#[inline]
pub fn gen_object_total_size(header: &ObjectHeader) -> usize {
    // GCAUD-3 (2026-08-01) — the "not a real object" backstop, hoisted.
    //
    // The `kind as u8 != Object` screen in the legacy arm below is documented
    // as this function's backstop for a `HumongousFiller` sentinel or an
    // out-of-range kind byte reaching a walk that failed to screen it. It was
    // not one: it sat in the THIRD arm of the dispatch, so any such header
    // that also carried `GC_FLAG_COMPACT` took the `is_compact_object` arm
    // instead — a pure `gc_flags` bit test that never looks at `kind` — and
    // came back as `HEADER_SIZE + object_body_size(header)`. For an
    // unregistered class id `object_body_size` is `0`, so the sentinel sized
    // as exactly `HEADER_SIZE`: large enough to pass every caller's
    // `total_size < HEADER_SIZE` corruption test, and therefore a stride that
    // walks the sentinel as a real 40-byte object instead of re-syncing.
    //
    // Screening both non-`Object`/`Array` kinds here makes the backstop total
    // and leaves the two live arms byte-identical. Compare the RAW byte, not
    // the enum: the optimiser is entitled to assume `kind` is a valid `0..=2`
    // discriminant, which is exactly what would let it fold away the
    // out-of-range case this screen exists to catch.
    let kind_byte = ObjectHeader::kind_tag(header.mark_word.load(Ordering::Relaxed));
    if kind_byte != ObjectKind::Object as u8 && kind_byte != ObjectKind::Array as u8 {
        warn_non_object_kind_in_object_arm(header);
        return 0;
    }
    let raw_size = if header.kind() == ObjectKind::Array {
        match array_data_size(header.array_length() as usize, header.element_type()) {
            Ok(data) => ARRAY_DATA_OFFSET + data,
            Err(_) => {
                warn_corrupt_array_header(header);
                0
            }
        }
    } else if is_compact_object(header) {
        // Compact object: `shape` holds the logical field count (as for any
        // object — see the note in the legacy arm below); the body size comes
        // from the class's registered layout via `object_body_size`. The sum is
        // bounded by the u32 body size + HEADER_SIZE, no overflow.
        cratonvm_types::object_instance_size(header)
    } else {
        // Header-coherence sanity checks. Anything reaching here must be a
        // coherent `kind = Object` header; otherwise return 0. The non-moving
        // sweep walker interprets `total_size < HEADER_SIZE` as corruption and
        // falls through to its re-sync path, which finds the next plausible
        // header and resumes walking — losing only the skipped region
        // (recovered by the next major-GC compaction) instead of aborting the
        // whole arena sweep.
        //
        // ---------------------------------------------------------------
        // Why there is no `array_length != 0` check here any more
        // ---------------------------------------------------------------
        // Until `d46e70521` ("gc: compact object headers and field storage")
        // this arm opened with a live `if header.array_length != 0 { … return
        // 0 }`. Back then `array_length: u32` and `num_slots: u32` were two
        // *separate* header words, so "an Object whose array-length word is
        // non-zero" was a decidable statement about a field that a correct
        // `kind = Object` allocation always left at 0.
        //
        // `d46e70521` merged both words into the single `shape: u32` at
        // `ARRAY_LENGTH_OFFSET == NUM_SLOTS_OFFSET == 12`. `array_length()`
        // now returns `shape` only when `kind == Array` and 0 otherwise, and
        // `num_slots()` returns `shape` unconditionally — so for a
        // `kind = Object` header the array length *is* the field count, the
        // same four bytes. The merge silently rewrote the condition to
        // `if header.kind == ObjectKind::Array`, dead by construction inside
        // the `else` of that very test, and rewrote its `warn!` argument to
        // `header.shape >> 16` — the high half of the field count, not an
        // array length; `shape` was never a packed pair. `821bb2cb1` removed
        // the corpse.
        //
        // The check is therefore **not reinstatable**: post-merge no
        // header-local predicate can distinguish "shape holds an array length"
        // from "shape holds a field count", and any `array_length() != 0` test
        // in this arm is dead for the same reason the old one was. Nor can the
        // layout registry adjudicate it — `plan_object_alloc` routes an object
        // to *this* legacy arm precisely when no registered layout matches its
        // field count, so a registry miss is the defined normal here, not
        // evidence of corruption. See `gen_object_total_size_undecidability`.
        //
        // The defect the old check was written for is fixed at the producer.
        // Its comment blamed "a JIT inline-allocation path that writes
        // array_length but leaves kind at its TLAB-zeroed default" — that was a
        // misdiagnosis. Per the fix's own notes in `jit/src/x64.rs`
        // (`emit_inline_tlab_new`), the observed "kind=Object but
        // array_length=1 (num_slots=384, class_id=4)" reports came from the
        // walker stepping HEADER_SIZE into a *committed-but-unheadered* object
        // and decoding its first `Value` field cell as a header — a walker
        // desync, not a mis-allocated object. That path now writes the whole
        // header (kind and shape included) *before* the cursor-commit store
        // that publishes the object, and it allocates objects only: every array
        // allocation goes through the `newarray` / `anewarray_object` /
        // `multianewarray_2d` helpers, which set `kind` and `shape` together
        // via `ObjectHeader::new`.
        //
        // What *is* still decidable is the kind itself. `ObjectKind` has a
        // third variant: `HumongousFiller`, the sentinel the regional collector
        // installs at the start of every humongous continuation region. It is
        // explicitly "not a real object, no oops to scan" (see `ObjectKind`),
        // yet only 2 of this function's 26 call sites screen it out before
        // calling; at the other 24 it lands here and gets sized as
        // `HEADER_SIZE + shape * SLOT_SIZE` — a stride computed from a word
        // that means nothing for a sentinel. Return 0 so those callers re-sync
        // instead. The generational heap never allocates humongous, so this is
        // defence-in-depth rather than a live fix.
        //
        // Testing `!= Object` rather than `== HumongousFiller` also covers a
        // header whose kind byte is not a valid `ObjectKind` discriminant at
        // all — notably a `GAP_FILLER_CLASS_ID` sentinel, whose 8-byte span
        // puts the low byte of its length field (8/16/24/32) where `kind`
        // lives and has no valid `shape` word at offset 12. Every call site is
        // already required to screen gap fillers out first (see the "must
        // precede `gen_object_total_size`" notes); this is the backstop if one
        // does not. Note the optimiser is entitled to assume `kind` is in
        // 0..=2, so treat that second case as belt-and-braces, not a guarantee.
        // Compare the RAW byte rather than the enum: the paragraph above notes
        // the optimiser may assume `kind` is a valid 0..=2 discriminant, which
        // is precisely what would let it fold this screen away for the
        // out-of-range case the screen exists to catch.
        //
        // GCAUD-3: this check is now REDUNDANT — the same screen runs at the
        // top of the function, where it also covers the `is_compact_object`
        // arm that used to bypass it. Retained because it is the one the
        // paragraphs above are written against, and because a future edit that
        // reorders the dispatch should not silently lose it twice.
        if ObjectHeader::kind_tag(header.mark_word.load(Ordering::Relaxed))
            != ObjectKind::Object as u8
        {
            warn_non_object_kind_in_object_arm(header);
            return 0;
        }

        // Defensive cap on num_slots: no real class has 1<<24 fields, and a
        // value above this is almost certainly garbage from an uninitialised
        // region. Walker re-syncs.
        if header.num_slots() > (1 << 24) {
            warn_implausible_num_slots(header);
            return 0;
        }
        HEADER_SIZE + header.num_slots() as usize * SLOT_SIZE
    };

    // Arena allocations reserve an 8-byte-aligned footprint. Compact object
    // bodies need not be naturally aligned, so their trailing padding belongs
    // to the object for every linear collector walk.
    raw_size.checked_add(7).map(|size| size & !7).unwrap_or(0)
}


pub const MAX_FILLER_DATA_BYTES: u64 = ((i32::MAX as u64) - 1) * 4;
/// Cold diagnostic for a corrupt array header. Split out of
/// [`gen_object_total_size`] so the `tracing::warn!` expansion (argument
/// formatting, a `&dyn Value` table) does not sit inline in a function called
/// once per object on every linear heap walk — that bulk is what stopped LLVM
/// inlining the size computation at its 41 call sites.
#[cold]
#[inline(never)]
pub fn warn_corrupt_array_header(header: &ObjectHeader) {
    // Raw byte, not `Debug` — see `warn_non_object_kind_in_object_arm`. This
    // header has already been judged corrupt, and `ArrayElementType`'s valid
    // discriminants are 4..=11, so an out-of-range byte here is *more* likely
    // than for `ObjectKind`, not less.
    //
    // gen r4/mark (2026-09-23): the RAW tag, via `element_type_tag`. This
    // printed `header.element_type() as u8`, which is the DECODED value —
    // `ObjectHeader::element_type` is total (`unwrap_or(Reference)`), so every
    // out-of-range tag, i.e. exactly the case the comment above is about, was
    // reported as Reference's discriminant and the corrupt byte was never shown.
    tracing::warn!(
        "GC: implausible array_length {} (element_type=0x{:02x}) in heap object \
         header — treating as corrupt; caller will stop/skip the walk",
        header.array_length(),
        ObjectHeader::element_type_tag(header.mark_word.load(Ordering::Relaxed)),
    );
}

/// Cold diagnostic for a non-`Object` kind reaching the legacy-object sizing
/// arm. See [`warn_corrupt_array_header`] for why this is out-of-line.
///
/// Formats the RAW kind byte, never `header.kind` through `Debug`. This
/// function is called *because* the kind is not `Object`, and one of the two
/// documented reasons for that (see the call site in
/// [`gen_object_total_size`]) is a byte that is not a valid `ObjectKind`
/// discriminant at all — a `GAP_FILLER_CLASS_ID` sentinel puts the low byte of
/// its length field (8/16/24/32) exactly where `kind` lives. The derived
/// `Debug` for a `#[repr(u8)]` enum indexes a static variant-name table by
/// discriminant, so `{:?}` on 8/16/24/32 reads a `&str` from past the end of
/// that table and the formatter then walks a wild pointer — turning a
/// recoverable corrupt-header detection into a hard `SIGSEGV` inside
/// `core::fmt`. The sweep's own corrupt-header path already learned this (see
/// the "format the RAW kind byte" note in `sweep_young_non_moving`); this is
/// the same hazard at the sibling site, and it is the one that killed H2's
/// `TestOutOfMemory` under sustained heap pressure.
#[cold]
#[inline(never)]
pub fn warn_non_object_kind_in_object_arm(header: &ObjectHeader) {
    tracing::warn!(
        "GC: header kind=0x{:02x} reached the legacy-object sizing arm (shape={}, \
         class_id={}); a region sentinel is not a sizable object — treating as \
         corrupt so the walker can re-sync.",
        ObjectHeader::kind_tag(header.mark_word.load(Ordering::Relaxed)),
        header.num_slots(),
        header.class_id.as_u32(),
    );
}

/// Cold diagnostic for an implausible `num_slots` on a legacy object header.
/// See [`warn_corrupt_array_header`] for why this is out-of-line.
#[cold]
#[inline(never)]
pub fn warn_implausible_num_slots(header: &ObjectHeader) {
    if crate::a2dbg::enabled() {
        tracing::warn!(
            "GC: implausible num_slots {} on kind=Object header (class_id={}); \
             treating as corrupt so the walker can re-sync.",
            header.num_slots(),
            header.class_id.as_u32(),
        );
    }
}

