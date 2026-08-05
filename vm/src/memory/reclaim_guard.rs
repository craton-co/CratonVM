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
//! `docs/known-issues/h2/bug-h2-classid0-stale-address-family.md`.
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
    let mut holder = String::from("<not found in frames>");
    'outer: for (fi, fr) in thread.frames.iter().enumerate() {
        let mask = fr.live_locals_mask_here();
        for li in 0..fr.locals_len() {
            if let Value::Object(Some(o)) = fr.get_local(li as u16) {
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
            if let Value::Object(Some(o)) = fr.stack.peek_at(si) {
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
