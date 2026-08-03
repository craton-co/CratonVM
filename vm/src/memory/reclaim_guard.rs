// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The flag-free "was this receiver RECLAIMED?" verdict, in one place.
//!
//! H2-CID0 (2026-08-01) added [`GenerationalHeap::reclaimed_hole_at`] because
//! a receiver that reads back as `java.lang.Object` is ambiguous on its face:
//! `java.lang.Object` is `ClassId(0)`, and so is the all-zero header the
//! collector leaves over a span it reclaimed, and so is an ordinary
//! `new Object()` whose identity hash has not been minted. Free-list
//! membership is not ambiguous — a live object is never inside a free block,
//! never past the allocation frontier, and never in the inactive semispace —
//! so the heap can answer outright, with no debug flag set in advance.
//!
//! That verdict was wired into the two `checkcast` failure paths and nowhere
//! else. **A dispatch miss has exactly the same face and was not covered**:
//!
//! ```text
//! NoSuchMethodError method="java/lang/Object.hasNext()Z"
//!      caller="org/h2/test/db/TestMultiThread.testConcurrentUpdate()V @pc=252"
//! ```
//!
//! is `jobs.iterator()`'s `Iterator` reading back as `ClassId(0)` with
//! `num_fields=0` — the same defect reaching the VM through
//! `invokeinterface` instead of through a cast, and the same four candidate
//! explanations with nothing to separate them. `CRATONVM_DBG_CCE_BT` prints a
//! rich dump there, but only if it was set BEFORE the run, which is never true
//! of the run that actually reproduces. See
//! `docs/known-issues/h2/bug-h2-blocked-frame-classid0-dispatch-miss.md`.
//!
//! Both `checkcast` reporters had already drifted from each other (the
//! interpreter's consults the young-sweep ring, the JIT's does not), which is
//! why this is a function and not a third copy. `jit::helpers`' copy and this
//! module's new `invoke`-dispatch caller both route through
//! [`report_reclaimed_receiver`]; `runtime::interpreter`'s `checkcast` copy is
//! the remaining un-migrated one and carries a pointer here.

use crate::vm::SharedVm;

/// Rate limit shared by every verdict this module prints. A reclaimed receiver
/// cascades — one freed block is read by many call sites — and the first
/// handful carry all the information.
const MAX_REPORTS: u64 = 8;

fn class_name_of(shared: &SharedVm, cid: u32) -> String {
    shared
        .classes
        .class_manager
        .try_read()
        .and_then(|cm| {
            cm.get_class(cratonvm_types::ClassId::new(cid))
                .map(|c| c.name.to_string())
        })
        .unwrap_or_else(|| format!("class_id={cid}"))
}

/// Say whether `addr` is memory the collector has already reclaimed, and what
/// the freeing collector recorded about the block.
///
/// `site` names the VM operation that tripped over it ("checkcast",
/// "JIT checkcast", "invoke dispatch"); `target` is the class or method it was
/// trying to reach; `actual_cid` is the class the receiver's header resolved
/// to. Returns `true` when the free-list verdict fired.
///
/// The two questions are asked separately, because they go stale at different
/// times:
///
/// * **the free-list location** answers only while the block is still free, and
///   it is the one that needs the heap locks — so it is asked only for
///   `actual_cid == 0`, the all-zero-header face. Callers on paths that fire in
///   bulk on a HEALTHY run (a `NoSuchMethodError` against a synthetic classpath
///   stub logs on every call) therefore pay nothing;
/// * **the old-gen reclamation ring** is asked ALWAYS. A block freed while
///   still referenced only reads as `java.lang.Object` until the allocator
///   reuses it; afterwards the same stale reference sees a *valid* object of an
///   unrelated class (`java.util.BitSet cannot be cast to
///   org.h2.mvstore.Chunk`) and the `ClassId(0)` gate would miss it entirely.
///   The ring is lock-free and bounded.
///
/// Costs nothing until something has already gone wrong: every caller is on a
/// terminal error path.
pub(crate) fn report_reclaimed_receiver(
    shared: &SharedVm,
    addr: usize,
    site: &'static str,
    target: &str,
    actual_cid: u32,
) -> bool {
    let zero_header = actual_cid == 0;
    let mut verdict = false;
    if zero_header {
        if let Some((what, span, size)) = shared.mem.heap.reclaimed_hole_at(addr) {
            verdict = true;
            static R: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            if R.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < MAX_REPORTS {
                tracing::error!(
                    target: "cratonvm::gc::guard",
                    obj = format!("{addr:#x}"),
                    site = site,
                    location = %what,
                    span = format!("{span:#x}+{size:#x}"),
                    target_class = %target,
                    "receiver points into RECLAIMED memory — a still-referenced object \
                     was collected. `java.lang.Object` here is the all-zero header the \
                     collector left behind, not a real Object.",
                );
            }
        }
    }
    // What the block held, and which reclamation freed it. Unconditional, so
    // unlike the young sweep ring this answers on the FIRST occurrence rather
    // than only on a re-run with the right flag pre-set — and it still answers
    // once the block has been handed out again under a concrete class.
    if let Some((cid, kind, freed_site, seq, base, size, flags)) =
        cratonvm_gc::gen_heap::old_freed_lookup_covering(addr)
    {
        static F: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        if F.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < MAX_REPORTS {
            tracing::error!(
                target: "cratonvm::gc::guard",
                obj = format!("{addr:#x}"),
                site = site,
                actual_class_id = actual_cid,
                target_class = %target,
                original_class = %class_name_of(shared, cid),
                original_kind = kind,
                freed_block = format!("{base:#x}+{size:#x}"),
                interior_off = addr - base,
                was_weak_referent =
                    flags & cratonvm_gc::gen_heap::OLD_FREED_FLAG_WATCHED != 0,
                interior_root_pointed_in =
                    flags & cratonvm_gc::gen_heap::OLD_FREED_FLAG_INTERIOR_ROOT != 0,
                freed_by = if freed_site == 1 {
                    "in-place old-gen sweep"
                } else {
                    "old-gen mark-compact"
                },
                free_seq = seq,
                "receiver is an OLD-GEN block this process RECLAIMED while it was still \
                 referenced. `original_class` is what the block held when it was freed; \
                 `freed_by` names the mark phase with the gap. A non-zero `interior_off` \
                 means the reference was into the block, not at its base.",
            );
        }
    }
    // H2-CID0 (2026-08-02): the young-generation twin, and unlike
    // `sweep_zero_lookup` below it is unconditional — so it answers on the
    // first occurrence instead of only on a re-run that was armed in advance
    // (and armed with a flag that changes which young collector runs).
    if let Some((base, size, cycle, seq)) = cratonvm_gc::gen_heap::young_freed_lookup(addr) {
        static Y: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        if Y.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < MAX_REPORTS {
            tracing::error!(
                target: "cratonvm::gc::guard",
                obj = format!("{addr:#x}"),
                site = site,
                actual_class_id = actual_cid,
                target_class = %target,
                freed_span = format!("{base:#x}+{size:#x}"),
                interior_off = addr - base,
                sweep_cycle = cycle,
                free_seq = seq,
                "receiver is inside a YOUNG span the non-moving sweep zeroed and returned to \
                 the free list. The span is coalesced, so `freed_span` bounds the victim \
                 rather than naming it.",
            );
        }
    }
    // The per-object young-sweep ring only records under
    // `CRATONVM_DBG_SWEEP_ZERO`, which also switches the young collector to the
    // sequential walk — so a hit here means the run was instrumented, and a
    // miss says nothing either way. It is still worth asking, because it names
    // the victim's original CLASS, which the span ring above cannot.
    if zero_header {
        if let Some((cid, kind, cycle, reason, initiator, blocked)) =
            cratonvm_gc::gen_heap::sweep_zero_lookup(addr)
        {
            static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            if N.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < MAX_REPORTS {
                tracing::error!(
                    target: "cratonvm::gc::guard",
                    obj = format!("{addr:#x}"),
                    site = site,
                    original_class = %class_name_of(shared, cid),
                    original_kind = kind,
                    sweep_cycle = cycle,
                    gc_reason = reason,
                    gc_initiator = initiator,
                    threads_blocked = blocked,
                    "…and it was RECLAIMED BY THE YOUNG SWEEP while still reachable. The \
                     original class names the root-coverage gap.",
                );
            }
        }
    }
    verdict
}
