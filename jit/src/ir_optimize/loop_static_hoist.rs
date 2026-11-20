// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! nio20 wave 6 (lane L4): read a plain `getstatic` of an initialised class
//! once, in the loop's pre-header, when nothing in the loop can write memory
//! (`CRATONVM_JIT_IR_LOOP_STATIC_HOIST`).
//!
//! # Why
//!
//! `Op::LoadStatic` is `MemAccess::Opaque` (its helper route may run
//! `<clinit>`) and advances the memory token, so a loop that reads a static
//! every iteration -- `for (k..) o = TABLE[(i + k) & 7]` over a `static final`
//! table, the `R17NioRows fence` / `Nio20FenceRow own` shape -- is a loop with
//! a memory φ threaded through an opaque node: LICM sees a hard barrier and a
//! write, and hoists nothing, and the unroller sees a side effect. The read
//! itself is two dependent loads per iteration (the statics block, then the
//! cell), and the array it yields is then null- and bounds-checked per
//! iteration because it is a new value each time.
//!
//! # What
//!
//! For a loop whose body writes no memory at all -- no store, call,
//! allocation, monitor, volatile access, `putstatic`, or any other node whose
//! memory shape may write -- every NON-volatile `Op::LoadStatic` of the body
//! whose `(class, field)` the compile reads DIRECTLY ([`super::static_read_is_direct`]:
//! the class was initialised when the compile resolved it, so the lowering
//! emits two plain loads and never the `<clinit>` helper) is moved to the
//! pre-header: its control becomes the loop-entry predecessor, its memory the
//! loop-entry memory, and every node that took it as a memory TOKEN takes its
//! own incoming token instead (the node read memory; it never wrote any, so
//! the chain closes over it).
//!
//! # Why it is sound
//!
//! * **Value.** Statics live in a VM-side table no heap store can reach, and
//!   the loop contains no `putstatic` and no call that could run one, so on
//!   this thread the cell holds the same value on every iteration.
//! * **Other threads (JMM).** The loop contains no synchronisation action (no
//!   volatile access, monitor, or call), so no write by another thread is
//!   ordered before any of the loop's reads; reading the value once is a legal
//!   execution of every iteration's read (JLS 17.4.5), the same argument
//!   [`super::loop_field_forward`] makes for instance fields and the one
//!   HotSpot's C2 makes when it hoists a plain static load out of a call-free
//!   loop. A safepoint poll is not a synchronisation action.
//! * **Traps and class initialisation.** A direct read cannot trap and runs
//!   no `<clinit>`: the class is already initialised, which is the only case
//!   the lowering takes the direct route for (it asks the same
//!   `resolve_static_base` the compile's feed asked). So running it once more
//!   (a loop that runs zero times) or earlier is unobservable.
//! * **Frames.** A frame state naming the read still names a node that holds
//!   the same value; no deopt point moves (the read has none).
//!
//! # Where it is not run
//!
//! On the optimizing OSR route's compile (`OptimizeContext::osr_route`): that
//! body is entered at a loop header, past the pre-header, so a hoisted value
//! live into the requested header has no local to seed it and the entry would
//! be refused -- the same reason the array-load hoist stays off there.
//! A method-entry compile pays the same refusal only for the OSR entry stub
//! at that header (an LICM-hoisted `Op::Load` costs exactly that too).
//!
//! # Wave 11 (JIT round 19, lane hoist11): the strict complement of the LICM arm
//!
//! `CRATONVM_JIT_IR_LICM_DIRECT_STATIC` (IRLOWER17-1, opt-in) hoists the same
//! reads from inside LICM. This pass runs first and moves every read it
//! admits out of the body, so with both on the LICM arm only ever sees what
//! this one left (no double hoist: a moved read's control is the pre-header,
//! outside every body that pre-header precedes). What the LICM arm still
//! covers that this pass did not, and the two opt-in switches that close it
//! here, at this earlier placement (before the unroller and the range
//! predicates):
//!
//! * **Heap stores in the loop** (`CRATONVM_JIT_IR_LOOP_STATIC_HOIST_HEAP_STORES`,
//!   HOIST11-1): a non-volatile `Op::Store` / `Op::ArrayStore` writes instance
//!   or element storage, never a class's statics block (the premise
//!   `Graph::may_alias` records for `AliasClass::Static`), and is not a
//!   synchronisation action, so it cannot change the read's value or order
//!   it. With `Graph::static_alias_class` on, a DIRECT `putstatic`
//!   (`Graph::direct_static_access` / `direct_static_ref_store`: an
//!   initialised class, no `<clinit>`, exactly one cell) refuses only the
//!   reads of its own cell, as in the LICM arm. `dst[i] = SCALE * src[i]`
//!   is the shape.
//! * **OSR entry rematerialisation** (`CRATONVM_JIT_IR_LOOP_STATIC_HOIST_OSR_REMAT`,
//!   HOIST11-2): a moved read is recorded in `Graph::licm_hoisted_static_reads`,
//!   so a method-entry compile's OSR entry stub inside the loop may re-read
//!   the cell (`ir_lower::osr_remat_compile`, under
//!   `CRATONVM_JIT_IR_OSR_REMAT_STATIC_READ`) instead of refusing the entry.
//!   The precondition that list states -- nothing between the pre-header and
//!   any point inside the loop writes the cell -- is this pass's own. Without
//!   it the default-ON pass pre-empted the LICM arm and with it the
//!   remat the LICM arm records.
//!
//! The OSR route stays the LICM arm's alone (see above).
//!
//! # Wave 12 (lane hoist12)
//!
//! * **HOIST11-4, the OSR route** (`CRATONVM_JIT_IR_LOOP_STATIC_HOIST_OSR_ROUTE`,
//!   opt-in): the pass also runs on the optimizing OSR route's compile, but
//!   moves only a read the OSR entry stub can re-read
//!   ([`osr_stub_can_reread`]: `int`-category / `long` under
//!   `CRATONVM_JIT_IR_OSR_REMAT_STATIC_READ`, `float` / `double` / reference
//!   also under `..._WIDE`, with `CRATONVM_JIT_IR_OSR_REMAT_INVARIANTS` on),
//!   at most [`OSR_ROUTE_MAX_MOVED`] of them (the stub re-reads at most
//!   eight unnamed values, shared with every other invariant), and records
//!   every one it moves for that re-read. A read the stub could not re-read
//!   stays in its loop, so the entry is never refused on its account.
//! * **HOIST11-5, one scan** (`CRATONVM_JIT_IR_STATIC_HOIST_SHARED_SCAN`,
//!   opt-in): this pass and the LICM arm (`ir_optimize::hoist_direct_static_reads`)
//!   classify each body node through [`classify_for_static_hoist`], each in
//!   its own [`StaticHoistScanMode`], instead of two hand-kept lists. The
//!   classifier answers what each old list answered in its mode
//!   (`hoist12_shared_scan_*` tests), except that a node carrying
//!   `volatile_access` refuses in both (the LICM list asked only
//!   `is_volatile_access`, i.e. `Load` / `Store`; a volatile static read was
//!   already refused there by its own op field).

use super::{
    build_users, is_memory_token_slot, loop_body_with, loop_entry_memory, loop_headers,
    static_read_is_direct, FxHashSet,
};
use crate::ir::{memory_shape_of, Graph, NodeId, Op, NO_NODE};

/// `CRATONVM_JIT_IR_LOOP_STATIC_HOIST` (nio20 wave 6, lane L4) -- **default
/// ON**, `=0` is the kill switch. Read without a `static`
/// (`process_global_statics_ratchet`); asked once per optimize, and by
/// `lib.rs` when it decides whether to feed `Graph::direct_static_fields`.
pub(crate) fn loop_static_hoist_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_LOOP_STATIC_HOIST_HEAP_STORES` (JIT round 19 wave 11,
/// lane hoist11, HOIST11-1) -- **default OFF** (opt-in): the pass also moves
/// the reads of a loop whose only writes are heap stores (and, with
/// `Graph::static_alias_class`, direct `putstatic`s of other cells). See the
/// module notes. No `static`.
pub(crate) fn loop_static_hoist_heap_stores_enabled() -> bool {
    false}

/// `CRATONVM_JIT_IR_LOOP_STATIC_HOIST_OSR_REMAT` (JIT round 19 wave 11, lane
/// hoist11, HOIST11-2) -- **default on** (default on): every read the pass moves
/// is recorded in `Graph::licm_hoisted_static_reads` for the OSR entry stub's
/// re-read. See the module notes. No `static`.
pub(crate) fn loop_static_hoist_osr_remat_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_LOOP_STATIC_HOIST_OSR_ROUTE` (JIT round 19 wave 12, lane
/// hoist12, HOIST11-4) -- **default OFF** (opt-in): the pass also runs on the
/// optimizing OSR route, moving only re-readable reads (module notes). No
/// `static`.
pub(crate) fn loop_static_hoist_osr_route_enabled() -> bool {
    false}

/// `CRATONVM_JIT_IR_STATIC_HOIST_SHARED_SCAN` (JIT round 19 wave 12, lane
/// hoist12, HOIST11-5) -- **default on** (default on): both direct-static hoists
/// scan their loop bodies through [`classify_for_static_hoist`]. No `static`.
pub(crate) fn static_hoist_shared_scan_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_DIRECT_STATIC_PREHEADER_NO_TRAP` (JIT round 19 wave 12,
/// lane hoist12, HOIST12-1) -- **default ON** since the wave 12 integration (`=0`
/// kill; read-only 348-376 -> 295-305 ns in 7/7 runs): a direct static read
/// in a loop pre-header (this pass's, or the LICM arm's) is exempt from the
/// pre-header trap test (`ir_optimize::preheader_may_trap`), so it no longer
/// refuses the later LICM arms there. No `static`.
pub(crate) fn direct_static_preheader_no_trap_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_STATIC_HOIST_PAST_SCOPED_RAW` (JIT round 23 wave 3, lane
/// L8; perf, **default ON**, `=0` kills): a scoped raw access
/// (`Op::Unbox(ScopedGet / ScopedPut)`, [`crate::ir::scoped_raw_effect`] with
/// plain order) does not keep a loop's direct static reads in it. It reads or
/// writes RAW memory only -- a primitive array element or off-heap bytes,
/// never a class's statics block (`AliasClass::Raw` is disjoint from
/// `AliasClass::Static`) -- runs no `<clinit>` and orders nothing; its
/// failure is a deopt, and the hoisted read cannot trap. The heap-buffer
/// loops (`R23W2L8HeapBB absget`: `ByteBuffer`'s statics re-read twice per
/// `getInt`) were refused only for it. No `static`.
pub(crate) fn static_hoist_past_scoped_raw_enabled() -> bool {
    true
}

/// Does `node` pass [`static_hoist_past_scoped_raw_enabled`]'s rule?
fn scoped_raw_static_free(node: &crate::ir::Node) -> bool {
    static_hoist_past_scoped_raw_enabled()
        && !node.volatile_access
        && crate::ir::scoped_raw_effect(node)
            .is_some_and(|e| matches!(e.order, crate::ir::MemOrder::Plain))
}

/// `CRATONVM_JIT_IR_STATIC_FINAL_HOIST` (JIT round 23 close, the zki
/// `objarr3` / `volatile-store` rows of `r21-lean-allocation-rows-*`) --
/// perf, **default ON**, `=0` kills. A loop that WRITES memory (a
/// `putstatic`, a heap store, a volatile access) but has no allocation and
/// no call, which
/// [`write_free_static_reads`] leaves alone, still has its direct reads of a
/// trusted `static final` reference field moved to its pre-header
/// ([`trusted_final_static_reads`]). Also read by `lib.rs` to ask the VM's
/// trusted-final answer (`Graph::trusted_final_static_fields`). No `static`.
pub(crate) fn static_final_hoist_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_STATIC_FINAL_HOIST_ACROSS_GC` (JIT round 24 item A4;
/// perf, **default ON since j27 2026-10-28**, `=0` kills): [`trusted_final_static_reads`] hoists out
/// of a loop that allocates or calls too, the arm the r23 close measured as a
/// loss (`objarr3` 12.5 ns/op kept, 14-21 moved) and the round-24 book's
/// rematerialisation lever would at best bring back to the kept arm: kept,
/// the read costs its two loads per iteration; moved, the value is live over
/// the allocation's slow-path call, whose map and reload a rematerialised
/// value would trade for the same two loads after that call. The switch
/// re-measures the claim on the current tree. No `static`.
pub(crate) fn static_final_hoist_across_gc_enabled() -> bool {
    // WARNING j27-user-flip-20261028 (default ON; =0 restores the old default)
    true
}

/// The direct, non-volatile, full-form reads in `body` of a field the
/// compile's feed calls a trusted `static final` reference
/// (`Graph::trusted_final_static_fields`: the VM's rule -- an application or
/// user-loader class, class file 53+ -- and an initialised class, which the
/// direct read already requires), or `None` when there is none or the switch
/// is off.
///
/// Why such a read may leave a loop whatever else the loop does: its cell
/// was written by the class's `<clinit>`, which completed before any direct
/// read was admitted, and nothing writes it again -- `putstatic` of a final
/// field outside `<clinit>` is an `IllegalAccessError` from class file 53 on
/// (the trust rule's floor), reflection refuses a `static final` field, and
/// JLS 17.5.3 lets a compiler ignore an `Unsafe` write, as HotSpot's C2
/// does when it folds the same field to a constant. So the read answers the
/// same value at the loop entry as on every iteration, across allocations,
/// calls and synchronisation actions alike; it cannot trap and runs no
/// `<clinit>` (the direct route), so running it once more, or earlier, is
/// unobservable. `java/lang/System`'s rewritable finals never reach the list
/// (`resolve_static_base` declines that class).
fn trusted_final_static_reads(graph: &Graph, body: &FxHashSet<NodeId>) -> Option<Vec<NodeId>> {
    if graph.trusted_final_static_fields.is_empty() || !static_final_hoist_enabled() {
        return None;
    }
    // Not across an allocation or a call: a hoisted reference is then live
    // over a GC point in every iteration, and its frame publication and
    // dead-home bookkeeping cost more than the two loads it saves (measured
    // on the zki `objarr3` kernel: 12.5 ns/op kept in the loop, 14-21 moved).
    let gc_point = body.iter().any(|&id| {
        graph.node_opt(id).is_some_and(|n| {
            matches!(
                n.op,
                Op::New { .. }
                    | Op::NewArray { .. }
                    | Op::MultiNewArray { .. }
                    | Op::NewCp { .. }
                    | Op::Call { .. }
            )
        })
    });
    if gc_point && !static_final_hoist_across_gc_enabled() {
        return None;
    }
    let mut reads: Vec<NodeId> = body
        .iter()
        .copied()
        .filter(|&id| {
            graph.node_opt(id).is_some_and(|node| {
                let Op::LoadStatic {
                    class_id,
                    field_index,
                    is_volatile: false,
                    ..
                } = node.op
                else {
                    return false;
                };
                !node.volatile_access
                    && static_read_is_direct(graph, class_id, field_index)
                    && crate::ir::memory_token_slot(node).is_some()
                    && graph
                        .trusted_final_static_fields
                        .contains(&(class_id, field_index))
            })
        })
        .collect();
    if reads.is_empty() {
        return None;
    }
    reads.sort_unstable();
    Some(reads)
}

/// `CRATONVM_JIT_IR_LOOP_STATIC_HOIST_AFTER_PROMOTE` (JIT round 21, lane
/// loopstatic; perf, **default ON**, `=0` kill): run this pass once more,
/// after the loop promotions (`loop_field_forward::promote_loop_cells` /
/// `promote_loop_statics` / `promote_fresh_loop_fields`), over the loops a
/// promotion took (`Graph::promoted_cell_header_bcis`). The early run sees
/// such a loop WITH its cells' loads and stores and so leaves its direct
/// static reads inside (`R20PLoopStaticBarrier.run`'s `SCALE`); a promotion
/// with no deopt pad leaves a loop that writes no memory at all, which is
/// exactly this pass's premise. A read left in the loop costs two dependent
/// loads per trip and, as an `Op::LoadStatic`, is a side effect to the
/// unroller (`ir_optimize::unroll`), which then refuses the promoted loop
/// `CRATONVM_JIT_IR_UNROLL_PROMOTED_LOOPS` offers it. Page:
/// `docs/feature-designs/jit-ir-optimizations.md`.
/// No `static`.
pub(crate) fn loop_static_hoist_after_promote_enabled() -> bool {
    true
}

/// HOIST11-4: at most this many reads moved per OSR-route compile. The entry
/// stub re-reads at most eight unnamed values (`ir_lower`'s
/// `OSR_REMAT_MAX_ROOTS`), shared with every other invariant the schedule
/// places in front of the loop; half of that is left to them.
pub(super) const OSR_ROUTE_MAX_MOVED: usize = 4;

/// HOIST11-4: can an OSR entry stub re-read `read` (a direct static read),
/// under the switches `ir_lower::osr_remat_compile` and
/// `osr_remat_wide_static_insn` ask? Their own type rules: `int`-category and
/// `long` with `CRATONVM_JIT_IR_OSR_REMAT_STATIC_READ`; `float`, `double` and
/// references with `..._WIDE` too; all of it only with
/// `CRATONVM_JIT_IR_OSR_REMAT_INVARIANTS` (default on).
pub(super) fn osr_stub_can_reread(graph: &Graph, read: NodeId) -> bool {
    use crate::ir::IrType;
    let Some(node) = graph.node_opt(read) else {
        return false;
    };
    let Op::LoadStatic {
        type_tag,
        is_volatile: false,
        ..
    } = node.op
    else {
        return false;
    };
    if !crate::ir_lower::ir_osr_remat_invariants_enabled()
        || !crate::ir_lower::ir_osr_remat_static_read_enabled()
    {
        return false;
    }
    match (type_tag, node.ty) {
        (b'J', IrType::Long) | (b'I' | b'Z' | b'B' | b'C' | b'S', IrType::Int) => true,
        (b'F', IrType::Float) | (b'D', IrType::Double) | (b'L' | b'[', IrType::Ref) => {
            crate::ir_lower::ir_osr_remat_static_read_wide_enabled()
        }
        _ => false,
    }
}

/// HOIST11-5: which body nodes a direct-static hoist admits, per caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StaticHoistScanMode {
    /// Non-volatile `Op::Store` / `Op::ArrayStore` are admitted (the LICM
    /// arm always; this pass under HOIST11-1).
    pub(crate) heap_stores: bool,
    /// A direct `putstatic` (`Graph::direct_static_access` /
    /// `direct_static_ref_store`) writes only its own cell
    /// (`Graph::static_alias_class`; this pass also needs `heap_stores`).
    pub(crate) per_cell: bool,
    /// `Op::OsrMemory` is admitted (the LICM arm, whose method-entry reads the
    /// OSR stub re-reads or whose entry is refused); this pass refuses it.
    pub(crate) osr_memory: bool,
}

/// HOIST11-5: the shared verdict on one loop-body node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StaticHoistNode {
    /// Cannot change or order a static cell.
    Free,
    /// A direct, non-volatile, full-form `getstatic`: a candidate.
    Read,
    /// A direct `putstatic` of exactly this cell (per-cell mode).
    CellWrite(crate::ir::AliasClass),
    /// May change a static, run `<clinit>` or order memory: the loop is left
    /// alone.
    Refuse,
}

/// HOIST11-5: classify `node` for a direct-static hoist in `mode`. See the
/// module notes; the rules are the union of the two hand-kept lists it
/// replaces under its switch.
pub(crate) fn classify_for_static_hoist(
    graph: &Graph,
    node: &crate::ir::Node,
    mode: StaticHoistScanMode,
) -> StaticHoistNode {
    use StaticHoistNode::{CellWrite, Free, Read, Refuse};
    // `Op::Throw` (an `Opaque` helper) is outside every loop body since
    // cov-07; refused rather than reasoned about if one ever appears.
    if node.op == Op::Throw {
        return Refuse;
    }
    if matches!(node.op, Op::Dead | Op::Phi) || node.op.is_control() {
        return Free;
    }
    // An acquire or a release: nothing moves across it.
    if node.volatile_access {
        return Refuse;
    }
    match node.op {
        Op::LoadStatic {
            class_id,
            field_index,
            is_volatile,
            ..
        } => {
            if !is_volatile
                && static_read_is_direct(graph, class_id, field_index)
                && crate::ir::memory_token_slot(node).is_some()
            {
                Read
            } else {
                // A helper-route read may run `<clinit>`; a compact read has
                // no `[ctrl, mem]` to rewrite.
                Refuse
            }
        }
        Op::Store(_) | Op::ArrayStore(_) => {
            if mode.heap_stores {
                Free
            } else {
                Refuse
            }
        }
        Op::StoreStatic { .. } => {
            if !mode.per_cell {
                return Refuse;
            }
            if let Some((cell, true)) = graph.direct_static_access(node) {
                CellWrite(cell)
            } else if let Some(cell) = graph.direct_static_ref_store(node) {
                CellWrite(cell)
            } else {
                Refuse
            }
        }
        Op::OsrMemory => {
            if mode.osr_memory {
                Free
            } else {
                Refuse
            }
        }
        Op::Guard { .. } | Op::Load(_) | Op::ArrayLoad(_) | Op::ArrayLength => Free,
        Op::Unbox { .. } if scoped_raw_static_free(node) => Free,
        ref op => {
            // Every remaining op that reads memory reads it through the
            // three arms above (`MemoryShape::Reads` is exactly those), so
            // "may write" and "touches memory" agree here.
            if memory_shape_of(op).may_write() {
                Refuse
            } else {
                Free
            }
        }
    }
}

/// Hoist the direct static reads of every write-free loop of `graph` (see the
/// module notes). Returns how many reads moved. (The tests' spelling; the
/// pipeline calls [`hoist_loop_static_reads_in`].)
#[cfg(test)]
pub(super) fn hoist_loop_static_reads(graph: &mut Graph) -> usize {
    hoist_loop_static_reads_in(graph, true)
}

/// The pass (`hoist_loop_static_reads` in the tests), with `rereadable_elsewhere` saying whether a
/// loop of the optimizing OSR route that no opaque arm enters moves its
/// re-readable reads (HOIST11-4's rule) or nothing. JIT r20 wave 19 (lane
/// yLoopPred, LOOPOPT20-3): `ir_optimize::optimize_passes` passes `false`
/// when it runs this pass on the OSR route only for the arm-entered loops
/// (`CRATONVM_JIT_IR_OSR_ROUTE_ARM_PREDICATES` on, HOIST11-4 off).
pub(super) fn hoist_loop_static_reads_in(graph: &mut Graph, rereadable_elsewhere: bool) -> usize {
    let dbg = false;
    // HOIST13-2 (JIT round 19 wave 14): the store-sinking census, before
    // anything moves; one line per loop with a candidate.
    if dbg {
        for (region, entry_pred, back_ctrls) in loop_headers(graph) {
            if entry_pred == NO_NODE {
                continue;
            }
            let users = build_users(graph);
            let body = loop_body_with(graph, &users, region, entry_pred, &back_ctrls);
            let c = store_sink_census(graph, region, &body);
            if c.candidates.is_empty() {
                continue;
            }
            let statics = c.candidates.iter().filter(|x| x.is_static).count();
            eprintln!(
                "[DBG_LICM] store-sink census header {region}: candidates={} (static={statics} \
                 field={}) sinkable={} promotable={} other-access={} opaque={} deopt_or_poll={}",
                c.candidates.len(),
                c.candidates.len() - statics,
                c.sinkable(),
                c.promotable(),
                c.candidates.iter().filter(|x| x.other_access).count(),
                c.candidates.iter().any(|x| x.opaque),
                c.deopt_or_poll,
            );
        }
    }
    hoist_reads_of_loops(graph, rereadable_elsewhere, false)
}

/// JIT round 21 (lane loopstatic, `CRATONVM_JIT_IR_LOOP_STATIC_HOIST_AFTER_PROMOTE`):
/// [`hoist_loop_static_reads_in`] over the loops a promotion took only
/// (`Graph::promoted_cell_header_bcis`), run after the promotions. Every rule
/// of the pass applies unchanged: a loop still holding any write (a deopt pad
/// promotion's silent stores, a call, a `putstatic`) is left alone, and on
/// the OSR route `rereadable_elsewhere` means what it means there. Returns how
/// many reads moved.
pub(super) fn hoist_promoted_loop_static_reads_in(
    graph: &mut Graph,
    rereadable_elsewhere: bool,
) -> usize {
    if graph.promoted_cell_header_bcis.is_empty() {
        return 0;
    }
    hoist_reads_of_loops(graph, rereadable_elsewhere, true)
}

/// The pass proper; `promoted_only` restricts it to the loops whose header
/// bci a loop promotion recorded.
fn hoist_reads_of_loops(
    graph: &mut Graph,
    rereadable_elsewhere: bool,
    promoted_only: bool,
) -> usize {
    let dbg = false;
    if graph.direct_static_fields.is_empty() {
        return 0;
    }
    let heap_stores = loop_static_hoist_heap_stores_enabled();
    // HOIST11-4: on the OSR route every moved read is recorded (the entry
    // depends on the re-read), and only re-readable reads move.
    let osr_route = graph.optimize_osr_route;
    let osr_remat = osr_route || loop_static_hoist_osr_remat_enabled();
    // JIT r20 wave 19 (lane yLoopPred, LOOPOPT20-3): on the OSR route, a loop
    // an opaque OSR arm enters through its pre-header moves every read (the
    // arm falls into the pre-header, so it runs them;
    // `super::osr_route_arm_predicates_enabled`); any other loop there moves
    // only what HOIST11-4 admits, and nothing with HOIST11-4 off.
    let arm_route = osr_route && super::osr_route_arm_predicates_enabled();
    let mut moved = 0usize;
    // The HOIST11-4 moves alone, which share the stub's re-read budget. The
    // budget is per compile: `optimize_passes` calls this twice on the OSR
    // route (the early run, then `hoist_promoted_loop_static_reads_in`), so
    // the reads an earlier call recorded are already spent. Arm-entered reads
    // are recorded too; counting them is conservative (t1 nbc, HOIST11-4).
    let mut moved_reread = if osr_route {
        graph.licm_hoisted_static_reads.len()
    } else {
        0
    };
    for (region, entry_pred, back_ctrls) in loop_headers(graph) {
        if !matches!(
            graph.node_opt(region).map(|n| &n.op),
            Some(Op::Region | Op::Merge)
        ) || entry_pred == NO_NODE
        {
            continue;
        }
        if promoted_only
            && !graph
                .node_opt(region)
                .and_then(|n| n.bytecode_pc)
                .is_some_and(|bci| graph.promoted_cell_header_bcis.contains(&bci))
        {
            continue;
        }
        // Rebuilt per loop: the previous loop's rewiring changed use edges.
        let users = build_users(graph);
        let body = loop_body_with(graph, &users, region, entry_pred, &back_ctrls);
        let Some(mut reads) = write_free_static_reads(graph, &body, heap_stores)
            .or_else(|| trusted_final_static_reads(graph, &body))
        else {
            if dbg {
                note_kept_static_reads(graph, &body, heap_stores, region);
            }
            continue;
        };
        let arm_entered = arm_route && super::entry_is_opaque_osr_arm(graph, entry_pred);
        if osr_route && !arm_entered {
            if !rereadable_elsewhere {
                continue;
            }
            reads.retain(|&r| osr_stub_can_reread(graph, r));
            reads.truncate(OSR_ROUTE_MAX_MOVED.saturating_sub(moved_reread));
        }
        if reads.is_empty() {
            continue;
        }
        let entry_mem = loop_entry_memory(graph, region, entry_pred);
        for read in reads {
            if hoist_one(graph, read, region, entry_pred, entry_mem, &body) {
                moved += 1;
                if osr_route && !arm_entered {
                    moved_reread += 1;
                }
                if osr_remat && !graph.licm_hoisted_static_reads.contains(&read) {
                    graph.licm_hoisted_static_reads.push(read);
                }
                if dbg {
                    eprintln!(
                        "[DBG_LICM] static read {read}: HOIST to preheader {entry_pred} \
                         (header {region})"
                    );
                }
            }
        }
    }
    moved
}

/// HOIST13-2 (JIT round 19 wave 14): one loop store a sinking pass could be
/// asked about, and what it found in the loop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct StoreSinkCandidate {
    /// The store node.
    pub(super) store: NodeId,
    /// `true` for a direct `putstatic`, `false` for a field of an invariant
    /// base.
    pub(super) is_static: bool,
    /// Another access in the loop reads or writes a cell the store may write
    /// (the same static; a field load / store at the same offset, any base).
    pub(super) other_access: bool,
    /// Every other access is a LOAD of exactly this cell (the same static, or
    /// the same base node and offset): an accumulator (`box.sum += v`). Such a
    /// store sinks only together with forwarding its reads (scalar promotion).
    pub(super) same_cell_reads_only: bool,
    /// The loop holds a node that may read or write any memory (a call, an
    /// allocation, a monitor, a non-direct static access).
    pub(super) opaque: bool,
}

/// HOIST13-2: what [`store_sink_census`] counted for one loop.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct StoreSinkCensus {
    pub(super) candidates: Vec<StoreSinkCandidate>,
    /// The loop has a deopt point (a `Guard`, or a node carrying a frame
    /// snapshot): a sunk store would have to be re-materialised there. The
    /// back-edge poll is the lowering's, on every loop, so it is not counted.
    pub(super) deopt_or_poll: bool,
}

impl StoreSinkCensus {
    /// Candidates nothing else in the loop touches and no opaque node can
    /// observe: the ones a sinking pass could take.
    pub(super) fn sinkable(&self) -> usize {
        self.candidates
            .iter()
            .filter(|c| !c.other_access && !c.opaque)
            .count()
    }

    /// Candidates a promotion (forward the cell's reads, sink the store) could
    /// take: blocked only by loads of their own cell, and no opaque node.
    pub(super) fn promotable(&self) -> usize {
        self.candidates
            .iter()
            .filter(|c| c.other_access && c.same_cell_reads_only && !c.opaque)
            .count()
    }
}

/// HOIST13-2 (JIT round 19 wave 14): the census of loop stores a store
/// sinking pass could take -- a non-volatile store to a loop-invariant cell
/// (a direct `putstatic`, or a constant-offset field of an invariant base)
/// that no other access in the loop reads or writes. Diagnostic only: it
/// moves nothing, and runs only under `CRATONVM_DBG_LICM`
/// ([`hoist_loop_static_reads`] prints one line per loop with a candidate).
pub(super) fn store_sink_census(
    graph: &Graph,
    region: NodeId,
    body: &FxHashSet<NodeId>,
) -> StoreSinkCensus {
    let mut out = StoreSinkCensus::default();
    let field_offset = |n: &crate::ir::Node| -> Option<i64> {
        let off = graph.node_opt(n.input_opt(3)?)?;
        match off.op {
            Op::Const(v) => Some(v),
            _ => None,
        }
    };
    let static_cell = |op: &Op| -> Option<(u32, u32)> {
        match *op {
            Op::LoadStatic {
                class_id,
                field_index,
                ..
            }
            | Op::StoreStatic {
                class_id,
                field_index,
                ..            } => Some((class_id, field_index)),
            _ => None,
        }
    };
    let mut ids: Vec<NodeId> = body.iter().copied().collect();
    ids.sort_unstable();
    let mut stores: Vec<(NodeId, bool)> = Vec::new();
    for &id in &ids {
        let Some(n) = graph.node_opt(id) else {
            continue;
        };
        if n.frame_snapshot.is_some() || matches!(n.op, Op::Guard { .. }) {
            out.deopt_or_poll = true;
        }
        if n.volatile_access {
            continue;
        }
        match n.op {
            Op::StoreStatic {
                is_volatile: false, ..
            } if graph.direct_static_access(n).is_some() => stores.push((id, true)),
            Op::Store(_)
                if field_offset(n).is_some()
                    && n.input_opt(2)
                        .is_some_and(|b| super::is_loop_invariant(graph, b, region, body)) =>
            {
                stores.push((id, false))
            }
            _ => {}
        }
    }
    let candidate_ids: FxHashSet<NodeId> = stores.iter().map(|&(s, _)| s).collect();
    let opaque = ids.iter().any(|&id| {
        graph.node_opt(id).is_some_and(|n| {
            !candidate_ids.contains(&id)
                && n.op != Op::Dead
                && n.op != Op::Phi
                && !n.op.is_control()
                && !(static_cell(&n.op).is_some() && graph.direct_static_access(n).is_some())
                && matches!(memory_shape_of(&n.op), crate::ir::MemoryShape::Opaque)
        })
    });
    for (store, is_static) in stores {
        let Some(s) = graph.node_opt(store) else {
            continue;
        };
        let mut other_access = false;
        let mut same_cell_reads_only = true;
        for &id in &ids {
            if id == store {
                continue;
            }
            let Some(n) = graph.node_opt(id) else {
                continue;
            };
            let touches = if is_static {
                static_cell(&n.op).is_some() && static_cell(&n.op) == static_cell(&s.op)
            } else {
                matches!(n.op, Op::Load(_) | Op::Store(_)) && field_offset(n) == field_offset(s)
            };
            if !touches {
                continue;
            }
            other_access = true;
            let same_cell_read = match n.op {
                Op::LoadStatic { .. } => is_static,
                Op::Load(_) => !is_static && n.input_opt(2) == s.input_opt(2),
                _ => false,
            };
            same_cell_reads_only &= same_cell_read;
        }
        out.candidates.push(StoreSinkCandidate {
            store,
            is_static,
            other_access,
            same_cell_reads_only: other_access && same_cell_reads_only,
            opaque,
        });
    }
    out
}

/// JIT round 21 wave 1 (lane loops, HOIST13-2): the headers of the loops
/// whose [`store_sink_census`] holds a promotable FIELD accumulator -- a
/// constant-offset store through a loop-invariant base, every other access of
/// that offset a load through the same base, no opaque node in the loop.
/// The candidate list the method-entry field promotion offers to the forward
/// (`loop_field_forward::promote_method_entry_accumulators`); a screen only,
/// the forward and the promotion re-check everything they rely on.
pub(super) fn accumulator_loop_headers(graph: &Graph) -> FxHashSet<NodeId> {
    let mut out: FxHashSet<NodeId> = FxHashSet::default();
    let users = build_users(graph);
    for (region, entry_pred, back_ctrls) in loop_headers(graph) {
        if entry_pred == NO_NODE
            || !matches!(
                graph.node_opt(region).map(|n| &n.op),
                Some(Op::Region | Op::Merge)
            )
        {
            continue;
        }
        let body = loop_body_with(graph, &users, region, entry_pred, &back_ctrls);
        let census = store_sink_census(graph, region, &body);
        if census
            .candidates
            .iter()
            .any(|c| !c.is_static && !c.opaque && c.other_access && c.same_cell_reads_only)
        {
            out.insert(region);
        }
    }
    out
}

/// The direct, plain static reads of `body`, ascending, or `None` when the
/// body may write memory or order it (the loop is then left alone).
///
/// `heap_stores` (HOIST11-1) admits non-volatile instance and element stores,
/// and -- with `Graph::static_alias_class` -- direct `putstatic`s, which then
/// drop only the reads of the cells they write.
fn write_free_static_reads(
    graph: &Graph,
    body: &FxHashSet<NodeId>,
    heap_stores: bool,
) -> Option<Vec<NodeId>> {
    scan_static_reads(graph, body.iter().copied(), heap_stores).ok()
}

/// j27 wave 6 (lane T1LOOPSTATIC, LOOPSTATIC21-1 census): the node of `body`
/// (lowest id first, so the answer is stable) that makes
/// [`write_free_static_reads`] leave the loop alone, or `None` when it would
/// not. Diagnostic only (`CRATONVM_DBG_LICM`): the same scan, so it can never
/// name a node the pass admits.
fn static_hoist_refuser(
    graph: &Graph,
    body: &FxHashSet<NodeId>,
    heap_stores: bool,
) -> Option<NodeId> {
    let mut ids: Vec<NodeId> = body.iter().copied().collect();
    ids.sort_unstable();
    scan_static_reads(graph, ids, heap_stores).err()
}

/// The direct, non-volatile `getstatic`s of `body`: the reads this pass would
/// move out of a write-free loop.
fn direct_static_reads_in(graph: &Graph, body: &FxHashSet<NodeId>) -> usize {
    body.iter()
        .filter(|&&id| {
            graph.node_opt(id).is_some_and(|n| match n.op {
                Op::LoadStatic {
                    class_id,
                    field_index,
                    is_volatile: false,
                    ..
                } => static_read_is_direct(graph, class_id, field_index),
                _ => false,
            })
        })
        .count()
}

/// `CRATONVM_DBG_LICM=1` (j27 wave 6, lane T1LOOPSTATIC, LOOPSTATIC21-1's
/// first step): a loop that keeps its direct static reads names the node that
/// kept them. Those reads stay `Op::LoadStatic`s threading the loop's memory
/// token -- a side effect to the unroller (`loop side effect LoadStatic`) --
/// so this line counts the loops a token-free read would change. Silent for a
/// loop with no direct read.
fn note_kept_static_reads(
    graph: &Graph,
    body: &FxHashSet<NodeId>,
    heap_stores: bool,
    region: NodeId,
) {
    let kept = direct_static_reads_in(graph, body);
    if kept == 0 {
        return;
    }
    let why = static_hoist_refuser(graph, body, heap_stores)
        .and_then(|id| {
            graph
                .node_opt(id)
                .map(|n| format!("n{id} {:?} at pc {:?}", n.op, n.bytecode_pc))
        })
        .unwrap_or_else(|| String::from("?"));
    let bci = graph.node_opt(region).and_then(|n| n.bytecode_pc);
    eprintln!(
        "[DBG_LICM] static read census {} header {region} (bci {bci:?}): {kept} direct \
         read(s) kept -- {why}",
        graph.scope_method_key
    );
}

/// [`write_free_static_reads`]'s scan over `ids`: `Err` names the first node
/// that refuses the loop.
fn scan_static_reads(
    graph: &Graph,
    ids: impl IntoIterator<Item = NodeId>,
    heap_stores: bool,
) -> Result<Vec<NodeId>, NodeId> {
    let mut reads: Vec<NodeId> = Vec::new();
    let mut written: Vec<crate::ir::AliasClass> = Vec::new();
    // HOIST11-5 (wave 12, opt-in): the shared classifier in this pass's mode.
    let shared = static_hoist_shared_scan_enabled();
    let mode = StaticHoistScanMode {
        heap_stores,
        per_cell: heap_stores && graph.static_alias_class,
        osr_memory: false,
    };
    for id in ids {
        let Some(node) = graph.node_opt(id) else {
            return Err(id);
        };
        if shared {
            match classify_for_static_hoist(graph, node, mode) {
                StaticHoistNode::Free => {}
                StaticHoistNode::Read => reads.push(id),
                StaticHoistNode::CellWrite(cell) => written.push(cell),
                StaticHoistNode::Refuse => return Err(id),
            }
            continue;
        }
        // `Op::Throw` is a control node with a helper call (`Opaque`): a loop
        // that throws is left alone rather than reasoned about.
        if node.op == Op::Throw {
            return Err(id);
        }
        if node.op == Op::Dead || node.op == Op::Phi || node.op.is_control() {
            continue;
        }
        // A volatile access is an acquire or a release: no read may move
        // across one (and a volatile read is one this pass never moves).
        if node.volatile_access {
            return Err(id);
        }
        match node.op {
            Op::LoadStatic {
                class_id,
                field_index,
                is_volatile,
                ..
            } => {
                if is_volatile || !static_read_is_direct(graph, class_id, field_index) {
                    // A helper-route read may run `<clinit>`: a write.
                    return Err(id);
                }
                // The full `[ctrl, mem]` form only: the move rewrites both.
                if crate::ir::memory_token_slot(node).is_none() {
                    return Err(id);
                }
                reads.push(id);
            }
            // HOIST11-1: instance / element storage is not a statics block,
            // and a plain store orders nothing (volatile refused above).
            Op::Store(_) | Op::ArrayStore(_) if heap_stores => {}
            // HOIST11-1, under `Graph::static_alias_class`: a direct
            // `putstatic` writes exactly its own cell and runs no `<clinit>`.
            Op::StoreStatic { .. } if heap_stores && graph.static_alias_class => {
                if let Some((cell, true)) = graph.direct_static_access(node) {
                    written.push(cell);
                } else if let Some(cell) = graph.direct_static_ref_store(node) {
                    written.push(cell);
                } else {
                    return Err(id);
                }
            }
            // r23 wave 3, lane L8: a scoped raw access writes no statics block.
            Op::Unbox { .. } if scoped_raw_static_free(node) => {}
            ref op => {
                if memory_shape_of(op).may_write() {
                    return Err(id);
                }
            }
        }
    }
    if !written.is_empty() {
        // Fail closed: a read whose cell cannot be named stays in the loop.
        reads.retain(|&read| {
            graph
                .node_opt(read)
                .and_then(|n| graph.direct_static_access(n))
                .is_some_and(|(cell, _)| !written.contains(&cell))
        });
    }
    reads.sort_unstable();
    Ok(reads)
}

/// Move one read to the pre-header. `false` (and nothing changed) when its
/// memory cannot be re-anchored or a user names it in a way the move cannot
/// account for.
fn hoist_one(
    graph: &mut Graph,
    read: NodeId,
    region: NodeId,
    entry_pred: NodeId,
    entry_mem: Option<NodeId>,
    body: &FxHashSet<NodeId>,
) -> bool {
    let Some(node) = graph.node_opt(read) else {
        return false;
    };
    let (Some(ctrl), Some(own_mem)) = (node.input_opt(0), node.input_opt(1)) else {
        return false;
    };
    if ctrl == entry_pred {
        return false; // already there
    }
    // Membership alone does not prove invariance: loop_body deliberately
    // omits phis at inner joins, including a guarded splice's memory phi.
    // Keeping that token while moving control above the loop places an input
    // below its user and loses the optimized compile. Ask the invariance
    // analysis, which checks a phi's anchor, before retaining the token.
    let new_mem = if super::is_loop_invariant(graph, own_mem, region, body) {
        own_mem
    } else {
        match entry_mem {
            Some(m) => m,
            None => return false,
        }
    };
    // Every user that takes the read as a memory TOKEN takes the read's own
    // incoming token instead. Any other use is a use of its value, which the
    // move keeps.
    let mut token_uses: Vec<(NodeId, usize)> = Vec::new();
    for u in graph.users_of(read) {
        let Some(un) = graph.node_opt(u) else {
            continue;
        };
        if un.op == Op::Dead {
            continue;
        }
        for (slot, &inp) in un.inputs.iter().enumerate() {
            if inp == read && is_memory_token_slot(un, slot) {
                token_uses.push((u, slot));
            }
        }
    }
    for (u, slot) in token_uses {
        graph.set_input(u, slot, own_mem);
    }
    graph.set_input(read, 0, entry_pred);
    graph.set_input(read, 1, new_mem);
    true
}




#[cfg(test)]
mod same_target_join_tests;
