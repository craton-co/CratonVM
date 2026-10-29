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
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_LOOP_STATIC_HOIST")
}

/// `CRATONVM_JIT_IR_LOOP_STATIC_HOIST_HEAP_STORES` (JIT round 19 wave 11,
/// lane hoist11, HOIST11-1) -- **default OFF** (opt-in): the pass also moves
/// the reads of a loop whose only writes are heap stores (and, with
/// `Graph::static_alias_class`, direct `putstatic`s of other cells). See the
/// module notes. No `static`.
pub(crate) fn loop_static_hoist_heap_stores_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_IR_LOOP_STATIC_HOIST_HEAP_STORES")
}

/// `CRATONVM_JIT_IR_LOOP_STATIC_HOIST_OSR_REMAT` (JIT round 19 wave 11, lane
/// hoist11, HOIST11-2) -- **default OFF** (opt-in): every read the pass moves
/// is recorded in `Graph::licm_hoisted_static_reads` for the OSR entry stub's
/// re-read. See the module notes. No `static`.
pub(crate) fn loop_static_hoist_osr_remat_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_IR_LOOP_STATIC_HOIST_OSR_REMAT")
}

/// `CRATONVM_JIT_IR_LOOP_STATIC_HOIST_OSR_ROUTE` (JIT round 19 wave 12, lane
/// hoist12, HOIST11-4) -- **default OFF** (opt-in): the pass also runs on the
/// optimizing OSR route, moving only re-readable reads (module notes). No
/// `static`.
pub(crate) fn loop_static_hoist_osr_route_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_IR_LOOP_STATIC_HOIST_OSR_ROUTE")
}

/// `CRATONVM_JIT_IR_STATIC_HOIST_SHARED_SCAN` (JIT round 19 wave 12, lane
/// hoist12, HOIST11-5) -- **default OFF** (opt-in): both direct-static hoists
/// scan their loop bodies through [`classify_for_static_hoist`]. No `static`.
pub(crate) fn static_hoist_shared_scan_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_IR_STATIC_HOIST_SHARED_SCAN")
}

/// `CRATONVM_JIT_IR_DIRECT_STATIC_PREHEADER_NO_TRAP` (JIT round 19 wave 12,
/// lane hoist12, HOIST12-1) -- **default ON** since the wave 12 integration (`=0`
/// kill; read-only 348-376 -> 295-305 ns in 7/7 runs): a direct static read
/// in a loop pre-header (this pass's, or the LICM arm's) is exempt from the
/// pre-header trap test (`ir_optimize::preheader_may_trap`), so it no longer
/// refuses the later LICM arms there. No `static`.
pub(crate) fn direct_static_preheader_no_trap_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on(
        "CRATONVM_JIT_IR_DIRECT_STATIC_PREHEADER_NO_TRAP",
    )
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
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_STATIC_HOIST_PAST_SCOPED_RAW")
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
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_STATIC_FINAL_HOIST")
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
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_STATIC_FINAL_HOIST_ACROSS_GC")
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
/// `docs/internal/jit/jr21-loopstatic-static-read-loop-cost-FIXED-20261029.md`.
/// No `static`.
pub(crate) fn loop_static_hoist_after_promote_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on(
        "CRATONVM_JIT_IR_LOOP_STATIC_HOIST_AFTER_PROMOTE",
    )
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
    let dbg = cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_LICM");
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
    let dbg = cratonvm_types::flags::runtime_flag_on("CRATONVM_DBG_LICM");
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
            if hoist_one(graph, read, entry_pred, entry_mem, &body) {
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
                ..
            } => Some((class_id, field_index)),
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
    // The memory the read is re-anchored on: the loop's entry state, or its
    // own token when that is already defined outside the loop.
    let new_mem = if !body.contains(&own_mem) {
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
mod tests {
    use super::*;
    use crate::ir::IrType;

    const SWITCH: &str = "CRATONVM_JIT_IR_LOOP_STATIC_HOIST";

    /// `for (k = 0; k < 16; k++) h += TABLE[k & 7].length` -- a header
    /// `Merge(entry, back)` with an int induction φ, a memory φ threaded through
    /// the `getstatic`, and an `ArrayLoad` of the static's array.
    /// Returns `(graph, load_static, mem_phi, array_load, entry_ctrl, start_mem)`.
    fn build(direct: bool, extra_write: bool) -> (Graph, NodeId, NodeId, NodeId, NodeId, NodeId) {
        build_with(
            direct,
            if extra_write {
                Write::StaticOther
            } else {
                Write::None
            },
        )
    }

    /// The write (if any) the loop body makes after the element read.
    #[derive(Clone, Copy, PartialEq, Debug)]
    enum Write {
        None,
        /// `putstatic` of `(9, 1)`, a cell the loop does not read.
        StaticOther,
        /// `putstatic` of `(9, 0)`, the cell the loop reads.
        StaticSame,
        /// An instance field store `p0.f = k`.
        Heap,
    }

    fn build_with(direct: bool, write: Write) -> (Graph, NodeId, NodeId, NodeId, NodeId, NodeId) {
        let mut g = Graph::empty(0);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let mem0 = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let entry = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let header = g.add(Op::Merge, IrType::Control, vec![entry], Some(2));
        let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
        let one = g.add(Op::Const(1), IrType::Int, vec![], None);
        let sixteen = g.add(Op::Const(16), IrType::Int, vec![], None);
        let seven = g.add(Op::Const(7), IrType::Int, vec![], None);
        let k = g.add(Op::Phi, IrType::Int, vec![header, zero], None);
        let mphi = g.add(Op::Phi, IrType::Memory, vec![header, mem0], None);
        let cmp = g.add(
            Op::Cmp(crate::ir::CmpOp::Lt),
            IrType::Int,
            vec![k, sixteen],
            None,
        );
        let iff = g.add(Op::If, IrType::Control, vec![header, cmp], Some(4));
        let stay = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
        let exit = g.add(Op::Proj(1), IrType::Control, vec![iff], None);
        let ls = g.add(
            Op::LoadStatic {
                class_id: 9,
                field_index: 0,
                type_tag: b'[',
                is_volatile: false,
            },
            IrType::Ref,
            vec![stay, mphi],
            Some(7),
        );
        let idx = g.add(Op::And, IrType::Int, vec![k, seven], None);
        let al = g.add(
            Op::ArrayLoad(crate::ir::MemKind::Ref),
            IrType::Ref,
            vec![stay, ls, ls, idx],
            Some(12),
        );
        let mut back_mem = ls;
        match write {
            Write::None => {}
            Write::StaticOther | Write::StaticSame => {
                back_mem = g.add(
                    Op::StoreStatic {
                        class_id: 9,
                        field_index: if write == Write::StaticSame { 0 } else { 1 },
                        type_tag: b'I',
                        is_volatile: false,
                    },
                    IrType::Memory,
                    vec![stay, ls, zero],
                    Some(13),
                );
            }
            Write::Heap => {
                let p0 = g.add(Op::Param(0), IrType::Ref, vec![], None);
                back_mem = g.add(
                    Op::Store(crate::ir::MemKind::Int),
                    IrType::Memory,
                    vec![stay, ls, p0, k],
                    Some(13),
                );
            }
        }
        let _ka = g.add(Op::KeepAlive, IrType::Void, vec![stay, al], Some(14));
        let next = g.add(Op::Add, IrType::Int, vec![k, one], None);
        g.nodes[header as usize].inputs.push(stay);
        g.nodes[k as usize].inputs.push(next);
        g.nodes[mphi as usize].inputs.push(back_mem);
        let ret = g.add(Op::Return, IrType::Void, vec![exit], None);
        g.exit = ret;
        if direct {
            g.direct_static_fields = vec![(9, 0), (9, 1)];
        }
        (g, ls, mphi, al, entry, mem0)
    }

    fn run(g: &mut Graph) -> usize {
        cratonvm_types::flags::with_thread_overrides(&[(SWITCH, Some("1"))], || {
            hoist_loop_static_reads(g)
        })
    }

    const HEAP_STORES: &str = "CRATONVM_JIT_IR_LOOP_STATIC_HOIST_HEAP_STORES";
    const OSR_REMAT: &str = "CRATONVM_JIT_IR_LOOP_STATIC_HOIST_OSR_REMAT";

    fn run_with(g: &mut Graph, extra: &[(&str, Option<&str>)]) -> usize {
        let mut all = vec![(SWITCH, Some("1"))];
        all.extend_from_slice(extra);
        cratonvm_types::flags::with_thread_overrides(&all, || hoist_loop_static_reads(g))
    }

    #[test]
    fn a_direct_static_read_in_a_write_free_loop_moves_to_the_pre_header() {
        if !crate::x64::inline_getstatic_enabled() {
            return;
        }
        let (mut g, ls, mphi, al, entry, mem0) = build(true, false);
        assert_eq!(run(&mut g), 1);
        assert_eq!(
            g.nodes[ls as usize].inputs[0], entry,
            "control is the pre-header"
        );
        assert_eq!(
            g.nodes[ls as usize].inputs[1], mem0,
            "memory is the loop-entry state"
        );
        // The element read keeps the array as its BASE, and its memory token
        // is the loop's memory φ (the read's own incoming token).
        assert_eq!(g.nodes[al as usize].inputs[2], ls);
        assert_eq!(g.nodes[al as usize].inputs[1], mphi);
        // The back edge of the memory φ closes over the read.
        assert_eq!(g.nodes[mphi as usize].inputs[2], mphi);
        // A second run finds it already there.
        assert_eq!(run(&mut g), 0);
    }

    #[test]
    fn a_read_the_compile_does_not_read_directly_stays() {
        let (mut g, ls, _, _, entry, _) = build(false, false);
        assert_eq!(run(&mut g), 0);
        assert_ne!(g.nodes[ls as usize].inputs[0], entry);
    }

    #[test]
    fn a_loop_that_writes_a_static_keeps_its_read() {
        let (mut g, ls, _, _, entry, _) = build(true, true);
        assert_eq!(run(&mut g), 0);
        assert_ne!(g.nodes[ls as usize].inputs[0], entry);
    }

    const FINAL_HOIST: &str = "CRATONVM_JIT_IR_STATIC_FINAL_HOIST";

    /// JIT round 23 close (`CRATONVM_JIT_IR_STATIC_FINAL_HOIST`): a loop that
    /// writes another static still moves its read of a TRUSTED final out;
    /// untrusted, or with the switch off, it stays.
    #[test]
    fn r23_close_a_trusted_final_read_leaves_a_writing_loop() {
        if !crate::x64::inline_getstatic_enabled() {
            return;
        }
        let (mut g, ls, mphi, _, entry, mem0) = build(true, true);
        g.trusted_final_static_fields = vec![(9, 0)];
        assert_eq!(run_with(&mut g, &[(FINAL_HOIST, None)]), 1, "default ON");
        assert_eq!(g.nodes[ls as usize].inputs[0], entry, "control is the pre-header");
        assert_eq!(g.nodes[ls as usize].inputs[1], mem0, "memory is the loop-entry state");
        // The putstatic took the read's incoming token: the memory phi.
        let store = g.nodes[mphi as usize].inputs[2];
        assert!(matches!(g.nodes[store as usize].op, Op::StoreStatic { .. }));
        assert_eq!(g.nodes[store as usize].inputs[1], mphi);

        let (mut g, ls, _, _, entry, _) = build(true, true);
        g.trusted_final_static_fields = vec![(9, 0)];
        assert_eq!(run_with(&mut g, &[(FINAL_HOIST, Some("0"))]), 0, "=0 kill");
        assert_ne!(g.nodes[ls as usize].inputs[0], entry);

        let (mut g, ls, _, _, entry, _) = build(true, true);
        g.trusted_final_static_fields = vec![(9, 1)];
        assert_eq!(run_with(&mut g, &[(FINAL_HOIST, None)]), 0, "another field is trusted");
        assert_ne!(g.nodes[ls as usize].inputs[0], entry);

        // Trusted but not read directly (class not initialised): stays.
        let (mut g, ls, _, _, entry, _) = build(false, true);
        g.trusted_final_static_fields = vec![(9, 0)];
        assert_eq!(run_with(&mut g, &[(FINAL_HOIST, None)]), 0);
        assert_ne!(g.nodes[ls as usize].inputs[0], entry);
    }

    /// HOIST11-1: an instance store keeps the read in the loop by default, and
    /// lets it move under the opt-in switch (statics are not heap storage).
    #[test]
    fn a_heap_store_keeps_the_read_unless_hoist11_admits_it() {
        if !crate::x64::inline_getstatic_enabled() {
            return;
        }
        let (mut g, ls, _, _, entry, _) = build_with(true, Write::Heap);
        assert_eq!(
            run(&mut g),
            0,
            "default: a loop that writes memory is left alone"
        );
        assert_ne!(g.nodes[ls as usize].inputs[0], entry);
        let (mut g, ls, mphi, _, entry, mem0) = build_with(true, Write::Heap);
        assert_eq!(run_with(&mut g, &[(HEAP_STORES, Some("1"))]), 1);
        assert_eq!(g.nodes[ls as usize].inputs[0], entry);
        assert_eq!(g.nodes[ls as usize].inputs[1], mem0);
        // The store took the read's incoming token: the memory φ.
        let store = g.nodes[mphi as usize].inputs[2];
        assert!(matches!(g.nodes[store as usize].op, Op::Store(_)));
        assert_eq!(g.nodes[store as usize].inputs[1], mphi);
        // Not recorded for the OSR stub without HOIST11-2.
        assert!(g.licm_hoisted_static_reads.is_empty());
    }

    /// HOIST11-1 with `Graph::static_alias_class`: a direct `putstatic` of
    /// another cell lets the read move; one of the read's own cell does not;
    /// without the alias class a `putstatic` still refuses the loop.
    #[test]
    fn a_direct_putstatic_refuses_only_its_own_cell_under_hoist11() {
        if !crate::x64::inline_getstatic_enabled() || !crate::ir_lower::inline_putstatic_enabled() {
            return;
        }
        let on = [(HEAP_STORES, Some("1"))];
        let (mut g, ls, _, _, entry, _) = build_with(true, Write::StaticOther);
        assert_eq!(
            run_with(&mut g, &on),
            0,
            "no alias class: any putstatic refuses"
        );
        assert_ne!(g.nodes[ls as usize].inputs[0], entry);
        let (mut g, ls, _, _, entry, _) = build_with(true, Write::StaticOther);
        g.static_alias_class = true;
        assert_eq!(run_with(&mut g, &on), 1);
        assert_eq!(g.nodes[ls as usize].inputs[0], entry);
        let (mut g, ls, _, _, entry, _) = build_with(true, Write::StaticSame);
        g.static_alias_class = true;
        assert_eq!(
            run_with(&mut g, &on),
            0,
            "the loop writes the cell it reads"
        );
        assert_ne!(g.nodes[ls as usize].inputs[0], entry);
        // And the switch off keeps the old answer with the alias class on.
        let (mut g, ls, _, _, entry, _) = build_with(true, Write::StaticOther);
        g.static_alias_class = true;
        assert_eq!(run(&mut g), 0);
        assert_ne!(g.nodes[ls as usize].inputs[0], entry);
    }

    /// HOIST11-2: a moved read is recorded for the OSR entry stub's re-read.
    #[test]
    fn hoist11_osr_remat_records_the_moved_read() {
        if !crate::x64::inline_getstatic_enabled() {
            return;
        }
        let (mut g, ls, _, _, _, _) = build(true, false);
        assert_eq!(run(&mut g), 1);
        assert!(g.licm_hoisted_static_reads.is_empty(), "off by default");
        let (mut g, ls2, _, _, _, _) = build(true, false);
        assert_eq!(ls, ls2);
        assert_eq!(run_with(&mut g, &[(OSR_REMAT, Some("1"))]), 1);
        assert_eq!(g.licm_hoisted_static_reads, vec![ls]);
        // A second run moves nothing and records nothing twice.
        assert_eq!(run_with(&mut g, &[(OSR_REMAT, Some("1"))]), 0);
        assert_eq!(g.licm_hoisted_static_reads, vec![ls]);
    }

    /// HOIST11-4: on the OSR route only a read the entry stub can re-read
    /// moves, and it is recorded for that re-read. The fixture's read is a
    /// reference (`[`), so it needs the wide re-read too.
    #[test]
    fn hoist12_on_the_osr_route_only_a_rereadable_read_moves() {
        if !crate::x64::inline_getstatic_enabled() {
            return;
        }
        let none: [(&str, Option<&str>); 2] = [
            ("CRATONVM_JIT_IR_OSR_REMAT_STATIC_READ", None),
            ("CRATONVM_JIT_IR_OSR_REMAT_STATIC_READ_WIDE", None),
        ];
        let narrow: [(&str, Option<&str>); 2] = [
            ("CRATONVM_JIT_IR_OSR_REMAT_STATIC_READ", Some("1")),
            ("CRATONVM_JIT_IR_OSR_REMAT_STATIC_READ_WIDE", None),
        ];
        let wide: [(&str, Option<&str>); 2] = [
            ("CRATONVM_JIT_IR_OSR_REMAT_STATIC_READ", Some("1")),
            ("CRATONVM_JIT_IR_OSR_REMAT_STATIC_READ_WIDE", Some("1")),
        ];
        for (arm, moves) in [(&none, false), (&narrow, false), (&wide, true)] {
            let (mut g, ls, _, _, entry, _) = build(true, false);
            g.optimize_osr_route = true;
            let n = run_with(&mut g, arm);
            assert_eq!(n == 1, moves, "arm {arm:?}");
            assert_eq!(g.nodes[ls as usize].inputs[0] == entry, moves);
            assert_eq!(g.licm_hoisted_static_reads == vec![ls], moves);
        }
        // Off the route the type does not matter (method entry, no stub need).
        let (mut g, _, _, _, _, _) = build(true, false);
        assert_eq!(run_with(&mut g, &none), 1);
    }

    /// t1 (nbc, HOIST11-4 budget): the stub's re-read budget is per COMPILE,
    /// not per call. `optimize_passes` calls the hoist twice on the OSR route
    /// (the early run and the after-promote re-run); reads an earlier call
    /// already recorded count, so the second call cannot hand the entry stub
    /// more than `OSR_ROUTE_MAX_MOVED` re-reads in all.
    #[test]
    fn t1_nbc_a_second_osr_route_call_shares_the_spent_reread_budget() {
        if !crate::x64::inline_getstatic_enabled() {
            return;
        }
        let wide: [(&str, Option<&str>); 2] = [
            ("CRATONVM_JIT_IR_OSR_REMAT_STATIC_READ", Some("1")),
            ("CRATONVM_JIT_IR_OSR_REMAT_STATIC_READ_WIDE", Some("1")),
        ];
        let (mut g, ls, _, _, entry, _) = build(true, false);
        g.optimize_osr_route = true;
        let spent: Vec<NodeId> = (0..OSR_ROUTE_MAX_MOVED)
            .map(|k| 9_000 + k as NodeId)
            .collect();
        g.licm_hoisted_static_reads = spent.clone();
        assert_eq!(run_with(&mut g, &wide), 0, "the budget is spent");
        assert_ne!(g.nodes[ls as usize].inputs[0], entry);
        assert_eq!(g.licm_hoisted_static_reads, spent);
        // One slot left: the read moves.
        let (mut g, ls, _, _, entry, _) = build(true, false);
        g.optimize_osr_route = true;
        g.licm_hoisted_static_reads = spent[..OSR_ROUTE_MAX_MOVED - 1].to_vec();
        assert_eq!(run_with(&mut g, &wide), 1);
        assert_eq!(g.nodes[ls as usize].inputs[0], entry);
    }

    /// JIT r20 wave 19 (lane yLoopPred, LOOPOPT20-3): put the opaque OSR merge
    /// `IrBuilder::build_opaque_osr_merge` builds in front of the fixture's
    /// header (`If(OsrEntryFlag)` on the entry, its arm and the entry side
    /// joined by a `Merge` that becomes the header's forward input). Returns
    /// that merge.
    fn put_an_opaque_arm_in_front(g: &mut Graph, mphi: NodeId, entry: NodeId) -> NodeId {
        let header = g.nodes[mphi as usize].inputs[0];
        let flag = g.add(
            Op::OsrEntryFlag { header_bci: 2 },
            IrType::Int,
            vec![entry],
            None,
        );
        let iff = g.add(Op::If, IrType::Control, vec![entry, flag], None);
        let from_entry = g.add(Op::Proj(1), IrType::Control, vec![iff], None);
        let arm = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
        let pre = g.add(Op::Merge, IrType::Control, vec![from_entry, arm], Some(2));
        let slot = g.nodes[header as usize]
            .inputs
            .iter()
            .position(|&p| p == entry)
            .expect("the header's forward input");
        g.set_input(header, slot, pre);
        pre
    }

    /// LOOPOPT20-3: on the OSR route, a loop an opaque arm enters through its
    /// pre-header moves its read whatever the stub could re-read (the arm
    /// runs the pre-header); the kill switch, and a loop with no arm, keep the
    /// HOIST11-4 rule (nothing moves without the wide re-read here).
    #[test]
    fn yloop_an_arm_entered_loop_on_the_osr_route_moves_its_read() {
        if !crate::x64::inline_getstatic_enabled() {
            return;
        }
        let arm_key = "CRATONVM_JIT_IR_OSR_ROUTE_ARM_PREDICATES";
        let no_wide: [(&str, Option<&str>); 2] = [
            ("CRATONVM_JIT_IR_OSR_REMAT_STATIC_READ_WIDE", None),
            (arm_key, None),
        ];
        let (mut g, ls, mphi, _, entry, _) = build(true, false);
        let pre = put_an_opaque_arm_in_front(&mut g, mphi, entry);
        assert!(super::super::entry_is_opaque_osr_arm(&g, pre));
        assert!(!super::super::entry_is_opaque_osr_arm(&g, entry));
        g.optimize_osr_route = true;
        assert_eq!(run_with(&mut g, &no_wide), 1);
        assert_eq!(
            g.nodes[ls as usize].inputs[0], pre,
            "the read runs in the arm's merge"
        );
        // The kill switch: the old rule, no wide re-read, nothing moves.
        let (mut g, ls, mphi, _, entry, _) = build(true, false);
        let pre = put_an_opaque_arm_in_front(&mut g, mphi, entry);
        g.optimize_osr_route = true;
        let killed = [
            ("CRATONVM_JIT_IR_OSR_REMAT_STATIC_READ_WIDE", None),
            (arm_key, Some("0")),
        ];
        assert_eq!(run_with(&mut g, &killed), 0);
        assert_ne!(g.nodes[ls as usize].inputs[0], pre);
        // No arm in front: the HOIST11-4 rule, and nothing at all when the
        // pipeline asks for the arm-entered loops only.
        let (mut g, ls, _, _, entry, _) = build(true, false);
        g.optimize_osr_route = true;
        assert_eq!(run_with(&mut g, &no_wide), 0);
        assert_ne!(g.nodes[ls as usize].inputs[0], entry);
        let wide = [
            ("CRATONVM_JIT_IR_OSR_REMAT_STATIC_READ", Some("1")),
            ("CRATONVM_JIT_IR_OSR_REMAT_STATIC_READ_WIDE", Some("1")),
            (SWITCH, Some("1")),
        ];
        let (mut g, ls, _, _, entry, _) = build(true, false);
        g.optimize_osr_route = true;
        let n = cratonvm_types::flags::with_thread_overrides(&wide, || {
            hoist_loop_static_reads_in(&mut g, false)
        });
        assert_eq!(n, 0, "HOIST11-4 off: a loop with no arm moves nothing");
        assert_ne!(g.nodes[ls as usize].inputs[0], entry);
    }

    /// HOIST11-5: the shared classifier gives this pass the answers its own
    /// scan gives, on every fixture and switch combination.
    #[test]
    fn hoist12_shared_scan_matches_this_pass() {
        if !crate::x64::inline_getstatic_enabled() {
            return;
        }
        let shared = "CRATONVM_JIT_IR_STATIC_HOIST_SHARED_SCAN";
        for write in [
            Write::None,
            Write::StaticOther,
            Write::StaticSame,
            Write::Heap,
        ] {
            for heap in [None, Some("1")] {
                for alias in [false, true] {
                    let outcome = |scan: Option<&str>| {
                        let (mut g, ls, _, _, _, _) = build_with(true, write);
                        g.static_alias_class = alias;
                        let n = run_with(&mut g, &[(HEAP_STORES, heap), (shared, scan)]);
                        (n, g.nodes[ls as usize].inputs.clone())
                    };
                    assert_eq!(
                        outcome(None),
                        outcome(Some("1")),
                        "write {write:?} heap {heap:?} alias {alias}"
                    );
                }
            }
        }
    }

    /// HOIST11-5: in the LICM arm's mode the classifier refuses exactly the
    /// nodes `blocks_direct_static_hoist` refuses (no per-cell), and in
    /// per-cell mode names the cell a direct `putstatic` writes.
    #[test]
    fn hoist12_shared_scan_matches_the_licm_arm() {
        if !crate::x64::inline_getstatic_enabled() {
            return;
        }
        for write in [
            Write::None,
            Write::StaticOther,
            Write::StaticSame,
            Write::Heap,
        ] {
            let (g, _, _, _, _, _) = build_with(true, write);
            let plain = StaticHoistScanMode {
                heap_stores: true,
                per_cell: false,
                osr_memory: true,
            };
            for (id, node) in g.nodes.iter().enumerate() {
                let id = id as NodeId; // Cast: node index -> NodeId
                let old = super::super::blocks_direct_static_hoist(&g, id);
                let new = classify_for_static_hoist(&g, node, plain) == StaticHoistNode::Refuse;
                assert_eq!(old, new, "node {id} {:?}", node.op);
            }
        }
        if !crate::ir_lower::inline_putstatic_enabled() {
            return;
        }
        let (mut g, _, _, _, _, _) = build_with(true, Write::StaticOther);
        g.static_alias_class = true;
        let per_cell = StaticHoistScanMode {
            heap_stores: true,
            per_cell: true,
            osr_memory: true,
        };
        let cells: Vec<_> = g
            .nodes
            .iter()
            .filter_map(|n| match classify_for_static_hoist(&g, n, per_cell) {
                StaticHoistNode::CellWrite(c) => Some(c),
                _ => None,
            })
            .collect();
        assert_eq!(
            cells,
            vec![crate::ir::AliasClass::Static {
                class_id: 9,
                field: 1
            }]
        );
    }

    #[test]
    fn a_volatile_static_read_stays() {
        let (mut g, ls, _, _, entry, _) = build(true, false);
        if let Op::LoadStatic { is_volatile, .. } = &mut g.nodes[ls as usize].op {
            *is_volatile = true;
        }
        assert_eq!(run(&mut g), 0);
        assert_ne!(g.nodes[ls as usize].inputs[0], entry);
    }

    /// HOIST13-2 (JIT round 19 wave 14): the store-sinking census counts a
    /// direct `putstatic` of a cell nothing else in the loop touches as
    /// sinkable, and one of the cell the loop reads as blocked by that access.
    #[test]
    fn hoist13_2_the_census_counts_a_putstatic_nothing_else_touches() {
        if !crate::x64::inline_getstatic_enabled() || !crate::ir_lower::inline_putstatic_enabled() {
            return;
        }
        let census = |write: Write| {
            let (g, ..) = build_with(true, write);
            let (region, entry_pred, back) = loop_headers(&g)[0].clone();
            let users = build_users(&g);
            let body = loop_body_with(&g, &users, region, entry_pred, &back);
            store_sink_census(&g, region, &body)
        };
        let other = census(Write::StaticOther);
        assert_eq!(other.candidates.len(), 1);
        assert!(other.candidates[0].is_static);
        assert_eq!(other.sinkable(), 1, "nothing else touches (9, 1)");
        assert!(!other.deopt_or_poll);
        let same = census(Write::StaticSame);
        assert_eq!(same.candidates.len(), 1);
        assert!(same.candidates[0].other_access, "the loop reads (9, 0)");
        assert_eq!(same.sinkable(), 0);
        assert_eq!(
            same.promotable(),
            1,
            "only its own cell's read: an accumulator"
        );
        assert!(census(Write::None).candidates.is_empty());
    }

    /// r23 wave 3, lane L8 (`CRATONVM_JIT_IR_STATIC_HOIST_PAST_SCOPED_RAW`):
    /// a scoped raw read is `Free` for the direct-static hoist under the
    /// switch (it touches raw memory only), and `Refuse` (an opaque op) with
    /// `=0`, as before.
    #[test]
    fn r23_a_scoped_raw_access_does_not_pin_static_reads() {
        let mut g = Graph::empty(8);
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        g.entry = start;
        let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let sma = g.add(Op::Param(0), IrType::Ref, vec![start], None);
        let session = g.add(Op::Param(1), IrType::Ref, vec![start], None);
        let base = g.add(Op::Param(2), IrType::Ref, vec![start], None);
        let off = g.add(Op::Param(3), IrType::Long, vec![start], None);
        let get = g.add(
            Op::Unbox {
                op: crate::ir::UnboxOp::ScopedGet {
                    width: 4,
                    signed: true,
                    ordered: false,
                },
                class_id: 0,
            },
            IrType::Int,
            vec![ctrl, mem, sma, session, base, off],
            Some(3),
        );
        let mode = StaticHoistScanMode {
            heap_stores: false,
            per_cell: false,
            osr_memory: false,
        };
        let key = concat!("CRATON", "VM_JIT_IR_STATIC_HOIST_PAST_SCOPED_RAW");
        let node = g.nodes[get as usize].clone();
        let on = cratonvm_types::flags::with_thread_overrides(&[(key, None)], || {
            classify_for_static_hoist(&g, &node, mode)
        });
        assert_eq!(
            on,
            StaticHoistNode::Free,
            "default: raw memory is not a statics block"
        );
        let off_arm = cratonvm_types::flags::with_thread_overrides(&[(key, Some("0"))], || {
            classify_for_static_hoist(&g, &node, mode)
        });
        assert_eq!(off_arm, StaticHoistNode::Refuse, "=0: the opaque rule");
    }

    // ── JIT round 21, lane loopstatic: the run after the loop promotions ──

    const AFTER_PROMOTE: &str = "CRATONVM_JIT_IR_LOOP_STATIC_HOIST_AFTER_PROMOTE";

    fn run_promoted(g: &mut Graph) -> usize {
        cratonvm_types::flags::with_thread_overrides(&[(SWITCH, Some("1"))], || {
            hoist_promoted_loop_static_reads_in(g, true)
        })
    }

    #[test]
    fn jr21_loopstatic_only_a_promoted_header_is_offered() {
        if !crate::x64::inline_getstatic_enabled() {
            return;
        }
        // The header `Merge` carries bci 2.
        let (mut g, ls, _, _, entry, mem0) = build_with(true, Write::None);
        assert_eq!(
            run_promoted(&mut g),
            0,
            "no promotion recorded: nothing moves"
        );
        assert_ne!(g.nodes[ls as usize].inputs[0], entry);
        g.promoted_cell_header_bcis.push(5);
        assert_eq!(run_promoted(&mut g), 0, "another header's bci");
        assert_ne!(g.nodes[ls as usize].inputs[0], entry);
        g.promoted_cell_header_bcis.push(2);
        assert_eq!(run_promoted(&mut g), 1);
        assert_eq!(g.nodes[ls as usize].inputs[0], entry);
        assert_eq!(g.nodes[ls as usize].inputs[1], mem0);
    }

    #[test]
    fn jr21_loopstatic_a_promoted_loop_that_still_writes_keeps_its_read() {
        // A promotion with deopt pads keeps its cells' stores (silent): the
        // loop is not write-free, and the rule of the early run holds.
        let (mut g, ls, _, _, entry, _) = build_with(true, Write::Heap);
        g.promoted_cell_header_bcis.push(2);
        assert_eq!(run_promoted(&mut g), 0);
        assert_ne!(g.nodes[ls as usize].inputs[0], entry);
    }

    #[test]
    fn jr21_loopstatic_the_after_promote_switch_reads_its_kill_word() {
        let off =
            cratonvm_types::flags::with_thread_overrides(&[(AFTER_PROMOTE, Some("0"))], || {
                loop_static_hoist_after_promote_enabled()
            });
        assert!(!off);
        let on =
            cratonvm_types::flags::with_thread_overrides(&[(AFTER_PROMOTE, Some("1"))], || {
                loop_static_hoist_after_promote_enabled()
            });
        assert!(on);
    }

    // ── j27 wave 6, lane T1LOOPSTATIC: the LOOPSTATIC21-1 census ──

    fn body_of(g: &Graph) -> FxHashSet<NodeId> {
        let (region, entry_pred, back) = loop_headers(g)[0].clone();
        let users = build_users(g);
        loop_body_with(g, &users, region, entry_pred, &back)
    }

    #[test]
    fn j27_t1loopstatic_the_census_names_the_putstatic_that_keeps_the_read() {
        if !crate::x64::inline_getstatic_enabled() {
            return;
        }
        let (g, ..) = build_with(true, Write::StaticSame);
        let body = body_of(&g);
        let store = body
            .iter()
            .copied()
            .find(|&id| matches!(g.nodes[id as usize].op, Op::StoreStatic { .. }))
            .expect("the loop's putstatic");
        assert_eq!(direct_static_reads_in(&g, &body), 1);
        assert!(write_free_static_reads(&g, &body, false).is_none());
        assert_eq!(static_hoist_refuser(&g, &body, false), Some(store));
    }

    #[test]
    fn j27_t1loopstatic_the_census_is_silent_for_a_loop_the_pass_takes() {
        if !crate::x64::inline_getstatic_enabled() {
            return;
        }
        let (g, ls, ..) = build_with(true, Write::None);
        let body = body_of(&g);
        assert_eq!(static_hoist_refuser(&g, &body, false), None);
        assert_eq!(write_free_static_reads(&g, &body, false), Some(vec![ls]));
        // HOIST11-1 admits a heap store: no refuser either.
        let (g, ..) = build_with(true, Write::Heap);
        let body = body_of(&g);
        assert_eq!(static_hoist_refuser(&g, &body, true), None);
    }

    #[test]
    fn j27_t1loopstatic_a_helper_route_read_is_its_own_refuser_and_not_counted() {
        let (g, ls, ..) = build_with(false, Write::None);
        let body = body_of(&g);
        assert_eq!(direct_static_reads_in(&g, &body), 0, "not a direct read");
        assert_eq!(static_hoist_refuser(&g, &body, false), Some(ls));
    }
}
