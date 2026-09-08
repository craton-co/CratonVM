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
//! `bug-h2-classid0-stale-address-family-FIXED.md`.
//!
//! Both `checkcast` reporters had already drifted from each other (the
//! interpreter's consults the young-sweep ring, the JIT's does not), which is
//! why this is a function and not a third copy. `jit::helpers`' copy and this
//! module's new `invoke`-dispatch caller both route through
//! [`report_reclaimed_receiver`]; `runtime::interpreter`'s `checkcast` copy is
//! the remaining un-migrated one and carries a pointer here.

use crate::threading::JvmThread;
use crate::vm::SharedVm;
use cratonvm_types::{ObjectKind, Value};

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
    report_reclaimed_receiver_inner(shared, addr, site, target, actual_cid, false)
}

/// The same verdict, with the free-list question asked REGARDLESS of what the
/// header's class id says.
///
/// The `actual_cid == 0` gate on [`report_reclaimed_receiver`] exists because
/// its callers include terminals that fire in bulk on a HEALTHY run, and the
/// free-list question takes both heap locks. That gate is exactly wrong for a
/// terminal that is *never* legitimate: a `clone()` dispatch that lands in
/// `java.lang.Thread.clone` or `java.lang.Enum.clone` — bodies that consist of
/// nothing but `throw new CloneNotSupportedException()` — is not something any
/// program asked for, so it should pay for the full verdict on its first
/// occurrence.
///
/// It also happens to be the case the gate cannot see. A prematurely freed
/// block reads as `java.lang.Object` only until the allocator hands it out
/// again; afterwards the stale reference sees a *valid* header of an unrelated
/// class, `actual_cid != 0`, and the un-forced call skips both the free-list
/// location and the `live_holders_of` scan — i.e. it goes quiet precisely on
/// the RE-SERVED face, which is the one that reaches the reader as
/// `Thread.clone` rather than as `ClassId(0)`.
pub(crate) fn report_reclaimed_receiver_forced(
    shared: &SharedVm,
    addr: usize,
    site: &'static str,
    target: &str,
    actual_cid: u32,
) -> bool {
    report_reclaimed_receiver_inner(shared, addr, site, target, actual_cid, true)
}

fn report_reclaimed_receiver_inner(
    shared: &SharedVm,
    addr: usize,
    site: &'static str,
    target: &str,
    actual_cid: u32,
    force: bool,
) -> bool {
    let zero_header = actual_cid == 0 || force;
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
            // H2-CID0 (2026-08-05): and WHO still holds it. The verdict above
            // says the address is reclaimed; this says which live object and
            // slot still names it, which is the one fact the "survived
            // un-rewritten" face has never had. Decoded through each object's
            // own slot enumerator — a raw word scan of this heap reports ~1 M
            // stale `Value`-cell padding words per compaction.
            let holders = shared.mem.heap.live_holders_of(addr, 8);
            if holders.is_empty() {
                tracing::error!(
                    target: "cratonvm::gc::guard",
                    obj = format!("{addr:#x}"),
                    site = site,
                    "…and NO live heap object holds this address in a decoded reference \
                     slot. The holder is therefore a frame local, a register, or a native \
                     side table — not a heap field.",
                );
            } else {
                for (holder, cid, slot) in holders {
                    tracing::error!(
                        target: "cratonvm::gc::guard",
                        obj = format!("{addr:#x}"),
                        site = site,
                        holder = format!("{holder:#x}"),
                        holder_class = %class_name_of(shared, cid),
                        holder_slot = slot,
                        "…and this LIVE object still holds the stale address in a reference \
                         slot — the reference was not rewritten when the referent moved.",
                    );
                }
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
    if let Some((base, size, cycle, seq, xt)) = cratonvm_gc::gen_heap::young_freed_lookup(addr) {
        static Y: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        if Y.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < MAX_REPORTS {
            // H2-CID0 (2026-08-05): the coverage of the sweep that freed THIS
            // span, captured while that sweep ran. `xt_unclassified > 0` means
            // it marked from a root set that provably omitted a still-RUNNING
            // peer's JIT-frame oops, and the non-moving sweep then freed on
            // `GC_FLAG_MARKED` alone — which is this defect, stated as a
            // measurement rather than a hypothesis.
            //
            // `xt_passes = 0` is a THIRD reading, not a quiet version of zero
            // unclassified: the take-over is gated on an `any_thread_in_jit()`
            // hint, so zero passes means the scan never looked at all.
            let (verdict, passes, taken, unclassified) = match xt {
                Some((0, _, _)) => ("NEVER-LOOKED", 0, 0, 0),
                Some((p, t, 0)) => ("complete", p, t, 0),
                Some((p, t, u)) => ("INCOMPLETE", p, t, u),
                None => ("not-captured", 0, 0, 0),
            };
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
                root_coverage = verdict,
                xt_passes = passes,
                xt_taken_over = taken,
                xt_unclassified = unclassified,
                "receiver is inside a YOUNG span the non-moving sweep zeroed and returned to \
                 the free list. The span is coalesced, so `freed_span` bounds the victim \
                 rather than naming it. `root_coverage` is that sweep's cross-thread \
                 coverage: INCOMPLETE means it freed on `GC_FLAG_MARKED` while a running \
                 peer's JIT frames were in no root set.",
            );
        }
    }
    // Was this address dropped from a root snapshot by the per-bci local
    // liveness filter? That filter's whole contract is that the slot can never
    // be read again — so a hit here names the method and slot where the
    // analysis is wrong, which is the only thing that turns "disable the
    // filter and the corruption stops" into a fix.
    if let Some(where_) = cratonvm_gc::gc_quiescence::liveness_filtered_at(addr) {
        static L: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        if L.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < MAX_REPORTS {
            tracing::error!(
                target: "cratonvm::gc::guard",
                obj = format!("{addr:#x}"),
                site = site,
                filtered_at = %where_,
                "…and the per-bci local-liveness filter DROPPED this address from a root                  snapshot at the frame named here. The filter guarantees such a slot is never                  read again; it was.",
            );
        }
    }
    // ZGC's relocation ledger, when `CRATONVM_DBG_ZGC_CORPSE` armed the run.
    // Unlike everything above it names the address's PREDECESSOR rather than
    // its span: which object the slide moved away from here, where that object
    // went, and whether it is still alive there. A live target means the holder
    // of this stale address was simply never rewritten when its referent moved;
    // a dead one means the object died afterwards and the defect is a lifetime
    // bug instead. `None` unless the flag was set, which is the one thing this
    // module's header complains about -- but the surrounding verdicts are
    // flag-free, so an armed re-run now adds identity to a report that already
    // says "reclaimed" on its own.
    if let Some((from, to, cid, size, still_live)) = shared.mem.heap.zgc_corpse_lookup(addr) {
        static Z: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        if Z.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < MAX_REPORTS {
            tracing::error!(
                target: "cratonvm::gc::guard",
                obj = format!("{addr:#x}"),
                site = site,
                vacated_from = format!("{from:#x}"),
                interior_off = addr - from,
                moved_to = format!("{to:#x}"),
                original_class = %class_name_of(shared, cid),
                original_size = size,
                target_still_live = still_live,
                "receiver names an address the ZGC slide VACATED. `original_class` is what \
                 lived here; `moved_to` is where it went. `target_still_live=true` means the \
                 object is alive at its new address and this holder was never rewritten.",
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

/// The full verdict for a dispatch that PROVES its receiver is not the object
/// the call site is holding.
///
/// Three sites qualify, and none of them is reachable from well-formed code:
///
/// * `java.lang.Thread.clone` and `java.lang.Enum.clone` — bodies that are
///   `throw new CloneNotSupportedException()` and nothing else. javac will not
///   compile a call to either (`Enum.clone` is `protected final`; nothing
///   clones a Thread), so an arrival means virtual dispatch was driven by a
///   receiver whose class is not the one the call site named;
/// * an **array-typed call site with a non-array receiver** — the verifier
///   guarantees the operand of `invokevirtual "[J".clone()` is a `[J`, so a
///   receiver whose header says otherwise is a broken heap, full stop. This one
///   fires one step EARLIER than the two above (before the target is chosen at
///   all), which is what makes it independent of which wrong body the corrupt
///   header happened to select.
///
/// Two defects produce these and this is what tells them apart:
///
/// * `receiver_kind=Array` — an array dispatched through its COMPONENT class
///   id, the defect fixed 2026-07-31 (arrays carry the component class id in
///   the header, so `someArray.m()` must be routed through `java/lang/Object`);
/// * `receiver_kind=Object` with a concrete `receiver_class` — the receiver's
///   block was reclaimed while still referenced and then RE-SERVED, so the
///   header now describes whatever occupies it. This is the
///   `ClassId(0)`-family's re-served face, and it is invisible to anything
///   gated on a zero header.
///
/// Three questions get asked, and the second and third are what the earlier
/// call-site-local version did not ask:
///
/// 1. the reclamation verdict, FORCED — see
///    [`report_reclaimed_receiver_forced`] for why the `actual_cid == 0` gate
///    is exactly wrong here;
/// 2. the OWNING thread's root bookkeeping, unconditionally rather than only
///    when the free-list verdict fired. Gating on that return value loses the
///    re-served face, which is the one this terminal reports;
/// 3. the interpreter frames, so the reader learns *which* call site handed
///    over the bad receiver (`java/util/Arrays.copyOf` at the `original.clone()`
///    bytecode, in the H2 witness) rather than only that one did.
///
/// Rate-limited as a whole: one impossible dispatch cascades into many once the
/// caller retries, and the first few carry everything.
pub(crate) fn report_impossible_dispatch_terminal(
    shared: &SharedVm,
    thread: &JvmThread,
    recv: cratonvm_types::ObjectRef,
    site: &'static str,
    target: &str,
    pre_refresh: Option<cratonvm_types::ObjectRef>,
) {
    static R: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    if R.fetch_add(1, std::sync::atomic::Ordering::Relaxed) >= MAX_REPORTS {
        return;
    }
    let addr = recv.as_ptr() as usize;
    let cid = shared.mem.heap.class_id_of(recv).as_u32();
    // The raw header, because every competing explanation makes a different
    // prediction about it: all-zero is the collector's own wipe, a plausible
    // class id with `kind=Array` is the component-class-dispatch defect, and a
    // plausible class id with `kind=Object` is a re-served block.
    // SAFETY: `recv` is a live `ObjectRef` popped as this invoke's receiver;
    // the interpreter has already read its class id and kind through the same
    // header, one line above.
    //
    // The WHOLE header, sized off `HEADER_SIZE` rather than a literal: the
    // mark word is where `is_forwarded`, the identity hash and the
    // kind/element-type/age/flags quartet all live after the 2026-08-06 and
    // 2026-08-07 shrinks, so a dump that stops before it stops short of every
    // bit worth reading — and a literal that outlives the next shrink would
    // read past the object.
    let header: [u8; cratonvm_types::HEADER_SIZE] =
        unsafe { std::ptr::read(recv.as_ptr() as *const [u8; cratonvm_types::HEADER_SIZE]) };
    tracing::error!(
        target: "cratonvm::gc::guard",
        obj = format!("{addr:#x}"),
        site = site,
        target_method = %target,
        receiver_kind = ?shared.mem.heap.kind_of(recv),
        receiver_class_id = cid,
        receiver_class = %class_name_of(shared, cid),
        in_heap = shared.mem.heap.is_heap_addr(addr).is_some(),
        // Which generation, asked unconditionally. `reclaimed_hole_at` answers
        // only while the block is still FREE, so on the re-served face — the one
        // this terminal exists for — it says nothing at all, and the log would
        // otherwise not even record which half of the heap to look in.
        in_young = shared.mem.heap.is_in_young_addr(addr),
        header = format!("{header:02x?}"),
        // The `ClassId(0)` family's own page closes with "if a ClassId(0)
        // receiver reappears, reopen this page and check
        // `object_degradation_count()` first" — the counter its 2026-08-06 root
        // cause (a `SUB_OBJECT` slot the encoder minted and the decoder refused)
        // increments. Nothing in the VM read it, so that instruction was not
        // actionable on a real run; a relaxed load on an already-failed path is.
        // Non-zero means live references degraded to `Value::Long` in this
        // process and `scan_local_objects` skipped them.
        object_degradations = cratonvm_types::compact_value::object_degradation_count(),
        "clone() dispatched to a body that only ever throws \
         CloneNotSupportedException. The receiver's header does not describe \
         what the caller is holding — either an array dispatched through its \
         COMPONENT class id, or a block that was reclaimed while still \
         referenced and then re-served.",
    );
    // Did the invoke's own forwarding read barrier produce this receiver?
    //
    // `execute_invoke_kind` runs `load_and_forward` over every reference it
    // pops, to repair a from-space address a moving collection left in a frame
    // slot. That barrier trusts one bit of the source object's header
    // (`is_forwarded`) and then a pointer word next to it. If a LIVE object's
    // header carries a stale forwarded bit — never cleared when its memory was
    // re-served — the barrier redirects a perfectly good reference to whatever
    // that word names, and the target validation only checks that the
    // destination *is* some live object, which a re-served block is.
    //
    // The two explanations make opposite predictions here and nothing else in
    // this report separates them:
    //
    // * `pre == post` — the operand stack already held the wrong address, so
    //   the loss is upstream: a root the collector did not remap (or freed).
    // * `pre != post` — the stack held one address and the barrier handed back
    //   another. Then the frames are innocent, which is exactly what
    //   `holder=<not found in frames>` looks like from the outside, and the
    //   forwarding word is the thing to distrust.
    if let Some(pre) = pre_refresh {
        let pre_addr = pre.as_ptr() as usize;
        // SAFETY: same contract as the receiver header read above — `pre` was
        // popped as this invoke's receiver and `load_and_forward` already
        // dereferenced its header.
        let pre_header: [u8; cratonvm_types::HEADER_SIZE] =
            unsafe { std::ptr::read(pre.as_ptr() as *const [u8; cratonvm_types::HEADER_SIZE]) };
        let pre_cid = shared.mem.heap.class_id_of(pre).as_u32();
        // The mark word AS THE BARRIER READ IT, versus what it says now. The
        // barrier's whole decision is `mark & 0b11 == MARK_FORWARDED`, and the
        // first witness had a source whose word read `MARK_NEUTRAL` by the time
        // the report ran — so only the recorded value can say whether the
        // barrier saw a genuine forwarding marker (and was handed a wrong
        // target) or read a word that was never a forwarding marker at all.
        let (b_src, b_mark, b_dst) = last_barrier_rewrite().unwrap_or((0, 0, 0));
        // SAFETY: `pre` is a live heap object (`load_and_forward` validated the
        // address); `MARK_WORD_OFFSET` is inside its header by construction.
        let mark_now: u64 = unsafe {
            std::ptr::read(pre.as_ptr().add(cratonvm_types::MARK_WORD_OFFSET) as *const u64)
        };
        tracing::error!(
            target: "cratonvm::gc::guard",
            obj = format!("{addr:#x}"),
            site = site,
            pre_refresh_obj = format!("{pre_addr:#x}"),
            barrier_rewrote = pre_addr != addr,
            barrier_src = format!("{b_src:#x}"),
            barrier_mark_at_read = format!("{b_mark:#x}"),
            barrier_mark_state = b_mark & cratonvm_types::MARK_STATE_MASK,
            barrier_dst = format!("{b_dst:#x}"),
            pre_mark_now = format!("{mark_now:#x}"),
            pre_refresh_kind = ?shared.mem.heap.kind_of(pre),
            pre_refresh_class = %class_name_of(shared, pre_cid),
            pre_refresh_header = format!("{pre_header:02x?}"),
            "…and this is the receiver as the OPERAND STACK handed it over, \
             before `load_and_forward`. `barrier_rewrote=true` means the frame \
             was holding a different address and the invoke's forwarding read \
             barrier replaced it — the stale forwarding word is then the \
             suspect, not the root scan.",
        );
    }
    report_reclaimed_receiver_forced(shared, addr, site, target, cid);
    report_root_slice_provenance(shared, thread, addr, site);
    if let Some(pre) = pre_refresh {
        let pre_addr = pre.as_ptr() as usize;
        if pre_addr != addr {
            // The pre-barrier address is the one the frames should still hold,
            // so run the same provenance question against it too.
            report_root_slice_provenance(shared, thread, pre_addr, "pre-refresh receiver");
        }
    }
    for (i, f) in thread.frames.iter().enumerate().rev().take(12) {
        tracing::error!(
            target: "cratonvm::gc::guard",
            obj = format!("{addr:#x}"),
            site = site,
            frame = i,
            at = format!("{}.{}{} @pc={}", f.class_name(), f.method_name(), f.method_descriptor(), f.pc),
            "…frame",
        );
    }
}

/// Walk a thread's interpreter frames and report any object slot that now
/// points into memory the collector has RECLAIMED.
///
/// This is the earliest point at which the `ClassId(0)` family is observable:
/// the object is already gone, but nothing has read through the dangling slot
/// yet, so the frame, method, pc and slot index that OWN the reference are
/// still available. Every reader-side reporter — the `checkcast` guards, the
/// `invoke` dispatch terminal — sees only an address and a failed operation.
///
/// Extracted from `NativeContextImpl::audit_frames_for_reclaimed_slots` so the
/// SAFEPOINT publish can run it as well. The blocked-region deposit/wake pair
/// only ever covers a PARKED thread; a running one holds frame slots too, and
/// the safepoint call bounds the loss window to "since the previous
/// safepoint", which no reader-side reporter can do — by the time a
/// `checkcast` or an `invoke` trips over the address, any number of
/// collections have passed.
///
/// Two properties keep it affordable enough to run with no flag set:
///
/// * a slot is examined at all only if its header reads `ClassId(0)` AND
///   `kind == Object`. A live object almost never does — the exception is a
///   genuine `new Object()`, and a PRIMITIVE ARRAY (whose header carries the
///   COMPONENT class id, and `long[]`/`int[]` have none) is excluded by the
///   kind test, which is what makes this cheap on real workloads;
/// * only then is the free-list question asked, and that is the one with no
///   false positives: a live object is never inside a free block, never past
///   the allocation frontier, and never in the inactive semispace.
///
/// Locals are filtered by the frame's own per-bci liveness mask, because a
/// DEAD local pointing into a reclaimed span is the collector working as
/// designed.
pub(crate) fn audit_thread_frames(shared: &SharedVm, thread: &JvmThread, site: &'static str) {
    // Process-wide probe budget. The free-list verdict takes the young and old
    // heap locks, so a workload that genuinely parks with `new Object()` locals
    // in frame slots must not pay for it forever.
    static PROBES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    const PROBE_BUDGET: u64 = 200_000;
    // THE COMPILED HALF. Everything below walks `thread.frames`, which holds
    // only INTERPRETER frames -- a JIT frame's oops live in the machine stack
    // band and in register images, so on a workload whose stale holder is
    // compiled this function was silent by construction and the first symptom
    // was a SIGSEGV at a JIT pc. Same ledger, same verdict, other storage.
    // No-op unless `CRATONVM_DBG_VACATED_FRAMES` is armed.
    crate::jit::conservative_roots::audit_jit_frames_for_vacated(
        Some(shared),
        thread.thread_id.0,
        site,
    );
    let heap = &shared.mem.heap;
    for (fi, fr) in thread.frames.iter().enumerate() {
        let live_mask = fr.live_locals_mask_here();
        let mut check = |o: cratonvm_types::ObjectRef, what: &str, idx: usize| {
            let a = o.as_ptr() as usize;
            // Region membership first: a lost-tag slot can hold a non-address,
            // and the header read below is a raw dereference.
            if heap.is_heap_addr(a).is_none() {
                return;
            }
            // `CRATONVM_DBG_VACATED_FRAMES` — the RE-OCCUPIED face, which the
            // `ClassId(0)` test below cannot see. A compacting collector slides
            // a survivor onto the address it vacated, so a slot left naming the
            // old address reads back a perfectly valid object of an unrelated
            // class, and every test in this function stays silent. Asked FIRST,
            // and only when armed.
            if let Some(moved_to) = cratonvm_gc::gc_quiescence::was_vacated(a) {
                static V: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                if V.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < MAX_REPORTS {
                    tracing::error!(
                        target: "cratonvm::gc::guard",
                        obj = format!("{a:#x}"),
                        site = site,
                        tid = thread.thread_id.0,
                        frame = fi,
                        class = %fr.class_name(),
                        method = %fr.method_name(),
                        pc = fr.pc,
                        slot = format!("{what}[{idx}]"),
                        slot_class = %class_name_of(shared, heap.class_id_of(o).as_u32()),
                        moved_to = format!("{moved_to:#x}"),
                        heap_collection = heap.collection_count(),
                        thread_last_heal = thread.last_heal_collection,
                        class_at_target = %class_name_of(
                            shared,
                            // SAFETY: `moved_to` is a post-move object base the
                            // collector just wrote; its header is mapped.
                            heap.class_id_of(unsafe {
                                cratonvm_types::ObjectRef::from_raw(moved_to as *mut u8)
                            })
                            .as_u32(),
                        ),
                        "a LIVE frame slot still names an address the LAST collection moved an \
                         object away from — the frame remap did not reach this slot. \
                         `slot_class` is whatever the slide has since put at that address.",
                    );
                }
            }
            if heap.class_id_of(o).as_u32() != 0 || heap.kind_of(o) != ObjectKind::Object {
                return;
            }
            if PROBES.fetch_add(1, std::sync::atomic::Ordering::Relaxed) >= PROBE_BUDGET {
                return;
            }
            if heap.reclaimed_hole_at(a).is_none() {
                return;
            }
            let ctx = format!(
                "tid={} frame#{fi} {}.{} pc={} {what}[{idx}]",
                thread.thread_id.0,
                fr.class_name(),
                fr.method_name(),
                fr.pc,
            );
            report_reclaimed_receiver(shared, a, site, &ctx, 0);
        };
        for li in 0..fr.locals_len() {
            if li < 64 && live_mask & (1u64 << li) == 0 {
                continue;
            }
            if let Value::Object(Some(o)) = fr.get_local(li as u16) {
                check(o, "local", li);
            }
        }
        for si in 0..fr.stack.len() {
            if let Value::Object(Some(o)) = fr.stack.peek_at(si) {
                check(o, "stack", si);
            }
        }
    }
}

/// Report where a reclaimed receiver stood in THIS thread's own GC bookkeeping
/// at the moment it was used.
///
/// The collector cannot read a peer's frames — it only ever sees the snapshot
/// that peer published — so "was the root slice complete?" is not a question
/// the sweep can answer about anyone but itself. It IS answerable here, on the
/// thread that owns the frame, at the moment the stale address surfaces.
///
/// The 2026-08-05 `DriverManager.getConnection` witness is exactly this shape:
/// a brand-new `java.util.Properties` in LOCAL 3 of the running frame, zeroed
/// by young sweep cycle 0 while the thread sat at a safepoint, and
/// `ROOT_IN_DEAD_SPANS` — which compares the doomed spans against the root
/// slice the mark phase was handed — stayed silent. That narrows it to root
/// COLLECTION, and this line says which half:
///
/// * `in_published_snapshot=false` — the snapshot this thread published is
///   missing a slot its own frames hold, i.e. the publish is stale or filtered;
/// * `in_published_snapshot=true` — the snapshot had it and the collector did
///   not use it (a delivery problem, not a publishing one).
///
/// Reached only from terminal error paths, so it costs nothing until something
/// has already gone wrong; it takes the snapshot mutex, which is why it is a
/// separate call rather than folded into `report_reclaimed_receiver` (that one
/// is reached in bulk on healthy runs).
/// The PRODUCER side of a corrupt `Value` cell.
///
/// `heap::read_value_cell_checked` reports the cell and stops there - it runs in
/// the collector crate and cannot see a Java frame. This runs on the VM side of
/// the same read, where the receiver and the owning thread's frames are both in
/// hand, and it asks the three questions the cell record cannot:
///
/// 1. WHAT is the receiver now - class, kind, generation, field count. Two
///    heap-pointer-shaped raw words in a slot the reader expected to hold a
///    `(tag, payload)` pair is the signature of a swept-then-re-served block,
///    and the receiver's current class names what re-served it.
/// 2. Was the address reclaimed - [`report_reclaimed_receiver_forced`], the same
///    verdict the failed-cast terminal asks for, `_forced` because the re-served
///    face carries a perfectly valid class id and the gated entry point is
///    silent on exactly that face.
/// 3. WHO is holding it - [`report_root_slice_provenance`] plus the frame walk,
///    which is what turns "something handed a stale reference to a native" into
///    a named bytecode.
///
/// Rate-limited as a whole. Armed by `CRATONVM_DBG_CORRUPT_CELL`; the caller
/// pays one relaxed load per field read while armed and nothing otherwise.
/// How many corrupt cells this process decoded, and how many a door or the
/// backstop actually named — printed once at exit while the flag is armed.
///
/// This is the line whose absence cost a day. The instrument reported nothing
/// through a run that had tripped the collector's guard, and "nothing" was read
/// as "no producer" when it meant "no door of mine saw it". A count that says
/// `decoded=1 reported=0` cannot be misread that way.
pub fn corrupt_cell_exit_summary() {
    cratonvm_types::cell_census::exit_summary();
}

/// Fabricate ONE corrupt-cell hit per named site, once per process.
///
/// See `heap::corrupt_cell_inject_for_selftest`. `"getfield"` lands inside a
/// watch, so the DOOR reporter must claim it; `"set_field"` lands where there
/// is no watch, so only the safepoint BACKSTOP can. A run with
/// `CRATONVM_DBG_CORRUPT_CELL_SELFTEST` set that prints fewer than both has a
/// broken instrument, and that is the whole point of having it.
#[inline]
pub(crate) fn corrupt_cell_selftest_inject(site: &'static str) {
    if !crate::runtime::env_cache::corrupt_cell_selftest() {
        return;
    }
    corrupt_cell_selftest_inject_cold(site);
}

#[cold]
#[inline(never)]
fn corrupt_cell_selftest_inject_cold(site: &'static str) {
    static GETFIELD: std::sync::Once = std::sync::Once::new();
    static PUTFIELD: std::sync::Once = std::sync::Once::new();
    let once = if site == "getfield" {
        &GETFIELD
    } else {
        &PUTFIELD
    };
    once.call_once(|| {
        // `raw0` spells "SELFTEST" so the record cannot be mistaken for a real
        // cell by anyone reading a log later.
        cratonvm_gc::heap::corrupt_cell_inject_for_selftest(
            0xdead_0000_0000_0000,
            u64::from_le_bytes(*b"SELFTEST"),
            0,
        );
    });
}

/// Arm the corrupt-cell watch around ONE read.
///
/// `None` when `CRATONVM_DBG_CORRUPT_CELL` is off, which is the whole cost on
/// the default path: a cached bool and a branch. See
/// [`corrupt_cell_watch_close`].
#[inline]
pub(crate) fn corrupt_cell_watch() -> Option<u64> {
    if crate::runtime::env_cache::corrupt_cell_dbg() {
        Some(cratonvm_gc::heap::corrupt_cell_hits())
    } else {
        None
    }
}

/// How many corrupt cells this thread has ACCOUNTED FOR — a running count, not
/// a high-water mark, and the difference is the whole correctness of the
/// backstop.
///
/// MEASURED while building this: with a high-water mark, a door that reports
/// its own cell also marks every cell outstanding BEFORE its watch opened as
/// seen, and the backstop then stays silent about them. The self-test caught it
/// as `decoded=2 reported=1` — one cell injected at a door, one at a site with
/// none, and only the door's was named. Counting claims instead of stamping a
/// position is what makes "nobody claimed this one" a question the backstop can
/// still answer.
fn corrupt_cell_seen() -> &'static std::thread::LocalKey<std::cell::Cell<u64>> {
    thread_local! {
        static SEEN: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    }
    &SEEN
}

/// Close a watch opened by [`corrupt_cell_watch`]: if the collector decoded a
/// corrupt `Value` cell during the read, name the DOOR it came through, the
/// receiver when the door has one, and the Java frames.
///
/// # Why there are several doors
///
/// The first version of this instrument watched exactly one:
/// `NativeContext::get_field`. That was enough to name the `String[]` producer
/// (`corrupt-value-cell-producer-was-a-string-array-FIXED-20260822`) and NOT
/// enough for the next one: a Spring Boot sweep tripped the guard once in
/// `KafkaMetricsAutoConfigurationTests` and the instrument stayed silent,
/// which says only that the read came through some OTHER door. A one-door
/// instrument cannot tell "no defect" from "not my door", and this one was
/// read as the former for a day.
pub(crate) fn corrupt_cell_watch_close(
    shared: &SharedVm,
    thread: &JvmThread,
    before: Option<u64>,
    door: &'static str,
    recv: Option<cratonvm_types::ObjectRef>,
    index: Option<usize>,
) {
    let Some(before) = before else { return };
    let now = cratonvm_gc::heap::corrupt_cell_hits();
    if now == before {
        return;
    }
    // Claim only what happened inside THIS read. Cells that were already
    // outstanding when the watch opened stay unaccounted, so the backstop can
    // still speak for them.
    corrupt_cell_seen().with(|c| c.set(c.get() + (now - before)));
    report_corrupt_cell_producer(shared, thread, door, recv, index);
}

/// The backstop: a corrupt cell decoded by THIS thread that no instrumented
/// read door claimed.
///
/// Called from the root-snapshot publish, which every running thread reaches
/// regularly, so a producer in a VM-internal reader — reflection, `Unsafe`, a
/// class-mirror populator, a serialization walk — still surfaces with a Java
/// stack instead of surfacing as silence. The stack is the one at the NEXT
/// safepoint rather than at the read, and the report says so.
///
/// Thread-scoped on purpose. The hit counter is process-GLOBAL, so without the
/// thread id recorded at the hit this would let one thread report a cell
/// another decoded — and a Spring Boot test class runs several threads, which
/// is exactly where that would mislead.
pub(crate) fn corrupt_cell_backstop(shared: &SharedVm, thread: &JvmThread) {
    if !crate::runtime::env_cache::corrupt_cell_dbg() {
        return;
    }
    let now = cratonvm_gc::heap::corrupt_cell_hits();
    if now == 0 || corrupt_cell_seen().with(|c| c.get()) >= now {
        return;
    }
    if cratonvm_gc::heap::corrupt_cell_last_thread() != cratonvm_gc::heap::probe_thread_id() {
        return;
    }
    corrupt_cell_seen().with(|c| c.set(now));
    report_corrupt_cell_producer(
        shared,
        thread,
        "backstop: no instrumented read door claimed this cell — the frames are \
         the ones at the NEXT safepoint, not at the read",
        None,
        None,
    );
}

pub(crate) fn report_corrupt_cell_producer(
    shared: &SharedVm,
    thread: &JvmThread,
    door: &'static str,
    recv: Option<cratonvm_types::ObjectRef>,
    index: Option<usize>,
) {
    cratonvm_types::cell_census::note_reported();
    static R: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    if R.fetch_add(1, std::sync::atomic::Ordering::Relaxed) >= MAX_REPORTS {
        return;
    }
    // The cell's own coordinates come from the collector rather than from the
    // caller: every door would otherwise have to carry three words it does not
    // use, and the backstop has no read to carry them from.
    let (slot, raw0, raw1) = cratonvm_gc::heap::corrupt_cell_last();
    // The raw words as BYTES as well as hex. The `String[]` producer's `raw0`
    // was a heap pointer; the `KafkaMetricsAutoConfigurationTests` one is the
    // ASCII text `"t/Proxy\0"`, and nothing in the record made that visible --
    // it took decoding the hex by hand to notice. A reader should not have to.
    let ascii = |w: u64| -> String {
        w.to_le_bytes()
            .iter()
            .map(|&b| {
                if (0x20..0x7f).contains(&b) {
                    b as char
                } else {
                    '.'
                }
            })
            .collect()
    };
    match recv {
        Some(recv) => {
            let addr = recv.as_ptr() as usize;
            let cid = shared.mem.heap.class_id_of(recv).as_u32();
            let name = class_name_of(shared, cid);
            tracing::error!(
                target: "cratonvm::gc::guard",
                door = door,
                obj = format!("{addr:#x}"),
                slot = format!("{slot:#x}"),
                slot_index = index,
                raw0 = format!("{raw0:#018x}"),
                raw1 = format!("{raw1:#018x}"),
                raw0_ascii = %ascii(raw0),
                raw1_ascii = %ascii(raw1),
                receiver_class = %name,
                receiver_kind = ?shared.mem.heap.kind_of(recv),
                receiver_fields = shared.mem.heap.num_fields(recv),
                in_heap = shared.mem.heap.is_heap_addr(addr).is_some(),
                in_young = shared.mem.heap.is_in_young_addr(addr),
                collections_now = shared.mem.heap.collection_count(),
                "the RECEIVER of the read that decoded a corrupt Value cell. Its \
                 class is what re-served the block, not what the holder thinks \
                 it is holding.",
            );
            report_reclaimed_receiver_forced(shared, addr, "corrupt-cell", &name, cid);
            report_root_slice_provenance(shared, thread, addr, "corrupt-cell");
        }
        None => {
            tracing::error!(
                target: "cratonvm::gc::guard",
                door = door,
                slot = format!("{slot:#x}"),
                raw0 = format!("{raw0:#018x}"),
                raw1 = format!("{raw1:#018x}"),
                raw0_ascii = %ascii(raw0),
                raw1_ascii = %ascii(raw1),
                in_heap = shared.mem.heap.is_heap_addr(slot).is_some(),
                in_young = shared.mem.heap.is_in_young_addr(slot),
                collections_now = shared.mem.heap.collection_count(),
                "a corrupt Value cell with NO receiver to name -- the read did \
                 not come through an instrumented door. The slot address and \
                 the frames below are the whole of what is known.",
            );
        }
    }
    let frames: Vec<String> = thread
        .frames
        .iter()
        .rev()
        .take(24)
        .map(|f| {
            format!(
                "{}.{}{} pc={}",
                f.class_name(),
                f.method_name(),
                f.method_descriptor(),
                f.pc
            )
        })
        .collect();
    tracing::error!(
        target: "cratonvm::gc::guard",
        site = "corrupt-cell",
        door = door,
        "...and the Java stack that reached it, top first:\n  {}",
        frames.join("\n  "),
    );
}

pub(crate) fn report_root_slice_provenance(
    shared: &SharedVm,
    thread: &JvmThread,
    addr: usize,
    site: &'static str,
) {
    let collections_now = shared.mem.heap.collection_count();
    static R: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    if R.fetch_add(1, std::sync::atomic::Ordering::Relaxed) >= MAX_REPORTS {
        return;
    }
    let snap = thread.root_snapshot.lock();
    let in_snapshot = snap.iter().any(|r| r.as_ptr() as usize == addr);
    let snap_len = snap.len();
    drop(snap);
    let blocked = thread
        .gc_block_state
        .in_blocked_region
        .load(std::sync::atomic::Ordering::Acquire);
    let top = thread
        .frames
        .last()
        .map(|f| format!("{}.{} pc={}", f.class_name(), f.method_name(), f.pc))
        .unwrap_or_else(|| "<no frame>".to_string());
    // Where the address sits in the frames, and why a root scan might have
    // skipped it. `kind` is the `local_kinds` mark (LONG/DOUBLE => skipped
    // outright); `live` is the per-bci liveness bit the same scan filters on.
    // H2-CID0 (2026-08-06): a slot whose payload IS the victim but which no
    // longer decodes as a reference is the degradation signature — see
    // `CompactValue`'s encoder/decoder provenance asymmetry. `to_value` refuses
    // a `SUB_OBJECT` tag whose payload never crossed a recorded
    // reference-construction boundary and hands back `Value::Long` with the same
    // bits; the `Value`-typed store then marks `local_kinds[i] = LKIND_LONG`,
    // and `Frame::scan_local_objects` skips LONG/DOUBLE slots BY DESIGN, so the
    // root never reaches the published snapshot.
    //
    // Matching only `Value::Object` printed `<not found in frames>` for exactly
    // this case — a verdict that says "look outside the frames" while the
    // address sits in a local. Report the degraded variant instead.
    fn payload_of(v: &Value) -> Option<(usize, &'static str)> {
        match v {
            Value::Object(Some(o)) => Some((o.as_ptr() as usize, "Object")),
            Value::Long(l) => Some((*l as u64 as usize, "Long")),
            Value::Double(d) => Some((d.to_bits() as usize, "Double")),
            Value::Int(i) => Some((*i as u32 as usize, "Int")),
            _ => None,
        }
    }
    let mut holder = String::from("<not found in frames>");
    'outer: for (fi, fr) in thread.frames.iter().enumerate() {
        let mask = fr.live_locals_mask_here();
        for li in 0..fr.locals_len() {
            let slot = fr.get_local(li as u16);
            if let Some((payload, variant)) = payload_of(&slot) {
                if payload == addr && variant != "Object" {
                    holder = format!(
                        "frame#{fi} {}.{} pc={} local[{li}] DEGRADED decoded_as={variant} \
                         kind={} live={} — the slot holds the victim's bits but is not a \
                         reference, so `scan_local_objects` skipped it and the root was \
                         never published (CompactValue provenance degradation)",
                        fr.class_name(),
                        fr.method_name(),
                        fr.pc,
                        fr.local_kind_at(li),
                        li >= 64 || mask & (1u64 << li) != 0,
                    );
                    break 'outer;
                }
            }
            if let Value::Object(Some(o)) = slot {
                if o.as_ptr() as usize == addr {
                    holder = format!(
                        "frame#{fi} {}.{} pc={} local[{li}] kind={} live={}",
                        fr.class_name(),
                        fr.method_name(),
                        fr.pc,
                        fr.local_kind_at(li),
                        li >= 64 || mask & (1u64 << li) != 0,
                    );
                    break 'outer;
                }
            }
        }
        for si in 0..fr.stack.len() {
            let sv = fr.stack.peek_at(si);
            if let Some((payload, variant)) = payload_of(&sv) {
                if payload == addr && variant != "Object" {
                    holder = format!(
                        "frame#{fi} {}.{} pc={} stack[{si}] DEGRADED decoded_as={variant} \
                         — operand-stack twin of the local case above",
                        fr.class_name(),
                        fr.method_name(),
                        fr.pc,
                    );
                    break 'outer;
                }
            }
            if let Value::Object(Some(o)) = sv {
                if o.as_ptr() as usize == addr {
                    holder = format!(
                        "frame#{fi} {}.{} pc={} stack[{si}]",
                        fr.class_name(),
                        fr.method_name(),
                        fr.pc,
                    );
                    break 'outer;
                }
            }
        }
    }
    let (publish_cc, publish_pc) = last_root_publish();
    tracing::error!(
        target: "cratonvm::gc::guard",
        obj = format!("{addr:#x}"),
        site = site,
        in_published_snapshot = in_snapshot,
        published_roots = snap_len,
        // How old the snapshot the collector marked this thread from was.
        // `collections_since_publish > 0` means at least one collection
        // completed after this thread last published — so it was marked from a
        // snapshot that could not contain anything allocated since.
        last_publish_at_collection = publish_cc,
        collections_now = collections_now,
        last_publish_pc = publish_pc,
        holder = %holder,
        in_blocked_region = blocked,
        frames = thread.frames.len(),
        top_frame = %top,
        "…and this is where that address stood in the OWNING thread's own GC \
         bookkeeping. `in_published_snapshot=false` means the snapshot the \
         collector marks this thread from did not contain a slot the thread's \
         frames hold — a root COLLECTION gap, not a mark or sweep one.",
    );
}

thread_local! {
    /// The last time this thread's invoke forwarding read barrier actually
    /// CHANGED a reference: `(source, mark word the barrier read, destination)`.
    ///
    /// The barrier's decision is a single relaxed load of the source object's
    /// mark word, and by the time any reader-side reporter looks at that object
    /// the word can already say something else. The 2026-08-07 `TestTempTables`
    /// witness is exactly that shape: the barrier redirected a live `long[1]` to
    /// an old-gen `java.lang.Thread`, and a millisecond later the same source
    /// header read `MARK_NEUTRAL` — so "was the source really forwarded?" is not
    /// answerable after the fact and has to be recorded AT the decision.
    static LAST_BARRIER_REWRITE: std::cell::Cell<Option<(usize, u64, usize)>> =
        const { std::cell::Cell::new(None) };

    /// Collection count at this thread's last root-snapshot publish, and the
    /// top frame's pc at that moment.
    ///
    /// The collector marks a thread it did not stop from that thread's LAST
    /// PUBLISHED snapshot, so "how old is the snapshot the collector used"
    /// is the difference between this and the collection count now. A thread
    /// that allocated an object and then had a collection complete without
    /// publishing again is marked from a snapshot that predates the object —
    /// the shape the 2026-08-05 `DriverManager.getConnection` witness has, where
    /// a brand-new `Properties` in LOCAL 3 was reclaimed while the thread sat
    /// out the pause. `Properties.<init>()V` returns void, and the publish
    /// hook fires on object-RETURNING native calls, so nothing between
    /// `new` and the failing `put` necessarily republishes.
    static LAST_PUBLISH: std::cell::Cell<(u64, u32)> = const { std::cell::Cell::new((u64::MAX, 0)) };
}

/// Record that the invoke forwarding read barrier rewrote a reference, with the
/// mark word it based that on. One `Cell` store, on a path that already loaded
/// the word — see [`LAST_BARRIER_REWRITE`].
pub(crate) fn note_barrier_rewrite(src: usize, mark_at_read: u64, dst: usize) {
    LAST_BARRIER_REWRITE.with(|c| c.set(Some((src, mark_at_read, dst))));
}

/// The last barrier rewrite this thread performed, if any.
pub(crate) fn last_barrier_rewrite() -> Option<(usize, u64, usize)> {
    LAST_BARRIER_REWRITE.with(std::cell::Cell::get)
}

/// Stamp the current collection count and top-frame pc as this thread's last
/// root publish. One `Cell` store; called from every publish site.
pub(crate) fn note_root_publish(shared: &SharedVm, thread: &JvmThread) {
    let pc = thread.frames.last().map_or(0, |f| f.pc as u32);
    let cc = shared.mem.heap.collection_count();
    LAST_PUBLISH.with(|c| c.set((cc, pc)));
}

/// `(collection_count, pc)` of this thread's last root publish.
pub(crate) fn last_root_publish() -> (u64, u32) {
    LAST_PUBLISH.with(std::cell::Cell::get)
}
