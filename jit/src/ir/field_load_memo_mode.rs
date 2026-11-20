#[allow(unused_imports)]
use super::*;

// ── Round 15 wave 1 (lane chain): G14-1, field-load forwarding ─────────
//
// `this.index.put(k, v); this.index.get(k)` reads `this.index` twice. The
// builder advances the memory token on every read, and the spliced `put`'s
// stores and merges sit between the two, so neither GVN nor the redundant-load
// pass (one block, or only pure reads between) can merge them: the second
// site's receiver is a new node, and every fact about the first (CH5-5's trap
// fact, W3-1's exact proof) is lost. The memo forwards the second read to the
// first when nothing between them can have written the cell.
//
// # Soundness
//
// * **Dominance.** An entry is recorded and consulted only in the compiling
//   method's own code (splice depth 0), and ALL entries are cleared at every
//   join of that code and wherever control is dead (a jump, a range switch,
//   a pruned arm). Between a record and a hit the walk therefore saw one
//   straight line of depth-0 code, through which every path to the hit runs,
//   with whole spliced bodies between (a spliced body is entered only through
//   its `invoke`, and every predecessor of each of its merges is inside it,
//   so its internal control flow, loops included, is walked completely before
//   the caller resumes).
// * **No write between.** Every node the graph gained since the entry was
//   recorded is classified by [`Graph::memory_effect`], path-insensitively
//   (a store on a cold arm of a spliced body counts). Anything ordered
//   (monitor enter/exit, a volatile access: the JMM forbids reusing a value
//   read before an acquire) or opaque (a call, a class-initialising helper,
//   a killed node) clears everything; a field store drops every entry
//   [`Graph::may_alias`] cannot separate from it, except that a store of a
//   different `MemKind` never overwrites the cell (one object, one index is
//   one field, which has one type). A memory φ writes nothing of its own.
// * **The value.** The second read would have seen what the first saw
//   unless something wrote between; forwarding is the execution in which no
//   other thread's racing write (plain field, no happens-before edge) became
//   visible between them, which the JMM allows. The base is the same NODE, so
//   the same object; if the first read had thrown, the second is unreachable.
// * Volatile reads are neither recorded nor forwarded.
//
// # Round 16 (lane fieldguard, CHN15-5 (a)): store-to-load forwarding
//
// `a++; b += a; if ((a & 7) == 0) ...` (`R16SyncSyncM$C.plainStep`) re-reads
// `this.a` twice after writing it, and every such read is a whole guarded
// inline field sequence in the optimizing tier (layout-epoch guard, per-object
// compact test, the load, a home write). With
// `CRATONVM_JIT_IR_FIELD_STORE_FORWARD=1` a plain depth-0 PRIMITIVE `putfield`
// records `(base, field) -> the stored value` (the value already narrowed to
// the field's type, i.e. exactly what a read of the cell returns: JVMS §6.5
// `putfield`, `IrBuilder::narrow_to_field_type`), and a later `getfield` of the
// same `(base, field)` with no possible write between is answered with it.
// The argument is the read memo's, unchanged: same dominance (record and
// consult at depth 0 only, cleared at every join and dead point), same "no
// write between" scan (the entry's cell is classified through the STORE's
// `writes` class, the same `AliasClass::Field` a read of the cell has as its
// `reads`), and the same JMM point (the read may see the program-order-last
// write of its own thread). The store dereferenced the same base node, so a
// null base threw there and the read is unreachable. Not for reference
// fields: a reference store is a barrier helper call in this tier and gains
// nothing from it, and keeping the forwarded set primitive keeps every
// collector argument out of it. Volatile stores are neither recorded nor
// forwarded.

/// How the G14-1 memo runs in this build.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::ir) enum FieldLoadMemoMode {
    /// Neither the switch nor the census is on: the memo does nothing.
    Off,
    /// `CRATONVM_DBG_IR_COMPILES` / `CRATONVM_DBG_JITC` without the switch:
    /// count what would be forwarded, change nothing.
    Census,
    /// `CRATONVM_JIT_IR_FIELD_LOAD_FORWARD=1`: forward.
    Forward,
}

impl FieldLoadMemoMode {
    /// Read once per builder (per compile).
    pub(in crate::ir) fn from_flags() -> Self {
        if ir_field_load_forward_enabled() {
            FieldLoadMemoMode::Forward
        } else if ir_bail_reporting()
            || ir_field_store_forward_enabled()
            || ir_field_final_forward_enabled()
        {
            // Round 16 (lane fieldguard): store forwarding needs the memo to
            // run; its READ entries then stay census-only. nio21 wave 2 (lane
            // L3): so does final-field forwarding, which answers only reads at
            // a `final` pc out of a census-mode memo.
            FieldLoadMemoMode::Census
        } else {
            FieldLoadMemoMode::Off
        }
    }
}

/// The G14-1 census, per build.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::ir) struct FieldLoadMemoCensus {
    /// Plain depth-0 `getfield`s recorded.
    pub(in crate::ir) recorded: usize,
    /// `getfield`s whose `(base, field)` a live entry answered.
    pub(in crate::ir) hits: usize,
    /// ... of which a spliced body was built between the two reads.
    pub(in crate::ir) hits_across_splice: usize,
    /// ... of which were forwarded (0 in census mode).
    pub(in crate::ir) forwarded: usize,
    /// Joins / dead control that cleared a non-empty memo.
    pub(in crate::ir) cleared_at_join: usize,
    /// Calls, class-initialising helpers, killed nodes, OSR memory.
    pub(in crate::ir) cleared_by_opaque: usize,
    /// Monitors and volatile accesses.
    pub(in crate::ir) cleared_by_ordering: usize,
    /// Entries dropped by a field store that may write their cell.
    pub(in crate::ir) dropped_by_store: usize,
    /// Round 15 wave 3 (lane chain2, CHN15-2): store/entry pairs the
    /// index-only model cannot separate but whose declaring classes differ:
    /// kept under `CRATONVM_JIT_IR_FIELD_IDENTITY_ALIAS`, counted in
    /// `dropped_by_store` without it.
    pub(in crate::ir) separated_by_field_identity: usize,
    /// Round 16 (lane fieldguard, CHN15-5 (a)): plain depth-0 primitive
    /// `putfield`s recorded as the cell's value.
    pub(in crate::ir) store_recorded: usize,
    /// `getfield`s a store-recorded entry answered.
    pub(in crate::ir) store_hits: usize,
    /// ... of which were forwarded (0 without the switch).
    pub(in crate::ir) store_forwarded: usize,
    /// nio21 wave 2 (lane L3): read-entry `hits` at a `final` pc.
    pub(in crate::ir) final_hits: usize,
    /// ... of which were forwarded under
    /// `CRATONVM_JIT_IR_FIELD_FINAL_FORWARD` (also counted in `forwarded`).
    pub(in crate::ir) final_forwarded: usize,
}

/// `CRATONVM_JIT_IR_FIELD_STORE_FORWARD` (round 16, lane fieldguard,
/// CHN15-5 (a)) -- **default ON since round 16** (measured as a bundle with
/// `CRATONVM_JIT_IR_STORE_NULL_CHECK_CSE` and `CRATONVM_JIT_IR_THIS_NONNULL`:
/// SyncM p 14→12 ns, alloc kernel -13 %); `0`/`false`/`off`/`no` turns it
/// off. On, it answers a plain depth-0 `getfield`
/// with the value an earlier plain primitive `putfield` of the same
/// `(base, field)` stored, when nothing between can have written the cell.
/// Independent of `CRATONVM_JIT_IR_FIELD_LOAD_FORWARD` (it runs the memo in
/// census mode when that switch is off). Read once per build; no static.
pub(in crate::ir) fn ir_field_store_forward_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_FIELD_LOAD_FORWARD` (round 15 wave 1, lane chain, G14-1) —
/// **default OFF**; `1` forwards a plain depth-0 `getfield` to an earlier read
/// of the same `(base, field)`. Read once per build.
pub(in crate::ir) fn ir_field_load_forward_enabled() -> bool {
    false
}

// ── Final fields (nio21 wave 2, lane L3) ───────────────────────────────────
//
// `CRATONVM_JIT_IR_FIELD_FINAL_FORWARD` lets the G14-1 memo answer a
// `getfield` whose resolved field is `ACC_FINAL` with the earlier read of the
// same `(base, field)`, with the general read switch off.
//
// This is REDUNDANT-LOAD elimination inside one compiled body, not constant
// folding. HotSpot's trusted-final rules (static finals; instance finals only
// of records, hidden classes, `java.lang.invoke` and the other
// `TrustFinalNonStaticFields` holders) govern treating a final's VALUE as a
// compile-time constant that outlives the read -- the JIT bakes it into code
// that runs long after reflection or `Unsafe` may have rewritten the cell.
// Nothing here does that: the forwarded value is an SSA node read at run time
// earlier in the same activation, and the entry dies at the same points every
// memo entry dies at:
//
// * every join and loop header (so never across a safepoint poll, which this
//   tier places only on back edges, and never from one iteration to the next);
// * every node whose memory effect writes `Any` -- every call (reflection's
//   `Field.set`, `Unsafe.put*` and `VarHandle` sets that stay calls are all
//   calls), every class-initialising helper, OSR memory. An allocation
//   (`MemEffect::allocation`) writes nothing and keeps the entry: it is a
//   collection point, and a collection moves objects but writes no field's
//   value; a forwarded REFERENCE is an ordinary live SSA value the safepoint
//   map publishes and the collector relocates, as it would be had the
//   program kept it in a local;
// * every ordered access (monitors, volatile reads and writes);
// * every store whose alias class may name the cell: a same-body `putfield`
//   of the field (a constructor's, or a pre-Java-9 class's), and nothing
//   else -- the only folded raw accesses (`ScopedGet`/`ScopedPut`,
//   `AliasClass::Raw`) address a primitive array payload or off-heap memory,
//   never an instance field.
//
// Between two such points the thread performs no write that may reach the
// cell, so the second read returning the first read's value is a legal JMM
// execution (JLS 17.4: the two reads are not ordered by happens-before with
// any other thread's racing write, and coherence only forbids the second read
// seeing an OLDER value than the first). That argument holds for every plain
// field; HotSpot's C2 eliminates such loads for non-final fields too. The
// `final` bit narrows the default to fields that, outside a constructor and
// deliberate reflection, are never written at all, so a hole in the alias
// model would need a write nobody makes to show. A racing improperly
// published object whose final is read as its default twice is JLS 17.5's
// explicit allowance. A deopt between the reads re-executes in the
// interpreter, which reads the cell again.

/// `CRATONVM_JIT_IR_FIELD_FINAL_FORWARD` (nio21 wave 2, lane L3) -- **default
/// ON**; `0`/`false`/`off`/`no` turns it off. On, a plain `getfield` of a
/// `final` field ([`IrBuilder::add_final_field_pcs`]) is forwarded to an earlier
/// read of the same `(base, field)` the memo still holds. Inert until a compile
/// door supplies the final bit (`CompileRequest::cp_field_final_resolver`,
/// `InlineSite::final_field_pcs`). Read once per builder; no static.
pub fn ir_field_final_forward_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_SCOPED_BYTE_ARRAY_BASE` (nio21 wave 7, lane L4, W5L4-3 of
/// `r20-heapbb-twelve-x-residual-is-guards-not-calls-CLOSED-20261012.md`) -- **default
/// ON**; `0` screens every scoped heap base as before. On, the planner rebases
/// `InlineSite::byte_array_field_pcs` ([`IrInlineTables::byte_array_field_pcs`])
/// and the scoped fold's heap arm emits no primitive-array screen and no
/// element-size read for a base in [`Graph::byte_array_nodes`]. Read per
/// question; no static.
pub fn ir_scoped_byte_array_base_enabled() -> bool {
    true
}

/// The G14-1 memo's map (`IrBuilder::field_load_memo`): `(base, field index)`
/// to `(value, splices at record, recording node, splice depth)`.
pub(in crate::ir) type FieldMemoMap = HashMap<(NodeId, usize), (NodeId, usize, NodeId, usize)>;



/// `CRATONVM_JIT_IR_FIELD_MEMO_PRECISE_JOINS` (JIT round 23 wave 2e, lane L2,
/// W4L1-2 of `jit-r20-nio-proposals-w4-L1.md`) -- **default ON**; `0`
/// restores the clear at every join. On, a forward join of the compiling
/// method's own code keeps the G14-1 memo entries every incoming edge carries
/// unchanged (`IrBuilder::field_load_memo_join`). Read once per builder.
pub(in crate::ir) fn ir_field_memo_precise_joins_enabled() -> bool {
    true
}

/// The entries `fall_through` (when control reaches the join live) and every
/// snapshot carry with the same value and recording node: the memo a precise
/// forward join keeps (`IrBuilder::field_load_memo_join`). Empty when there
/// is no edge at all.
pub(in crate::ir) fn field_memo_intersection(
    fall_through: Option<FieldMemoMap>,
    snapshots: Vec<FieldMemoMap>,
) -> FieldMemoMap {
    let mut edges = fall_through.into_iter().chain(snapshots);
    let Some(mut kept) = edges.next() else {
        return HashMap::new();
    };
    for edge in edges {
        kept.retain(|key, entry| {
            edge.get(key).is_some_and(|other| (other.0, other.2) == (entry.0, entry.2))
        });
    }
    kept
}



/// `CRATONVM_JIT_IR_FIELD_IDENTITY_ALIAS` (round 15 wave 3, lane chain2,
/// CHN15-2) -- **default OFF**; `1` lets the alias model separate two field
/// accesses whose resolved fields have different DECLARING classes
/// ([`Graph::may_alias`], [`Graph::field_identity_alias`]). Inert until the
/// compile doors hand the builder its rows
/// ([`IrBuilder::set_field_decl_classes`]). Read once per build, when the rows
/// are set; no static.
pub(in crate::ir) fn ir_field_identity_alias_enabled() -> bool {
    false
}

impl IrBuilder {
    /// The value an earlier read of `base.#field_index` left, when the memo
    /// may forward it (see the section comment), else `None`. Counts the hit
    /// in census mode and still answers `None` there.
    pub(in crate::ir) fn field_load_memo_forward(
        &mut self,
        base: NodeId,
        field_index: usize,
        ty: IrType,
        is_volatile: bool,
        is_final: bool,
    ) -> Option<NodeId> {
        if self.field_load_memo_mode == FieldLoadMemoMode::Off
            || is_volatile
            || !self.memo_depth_admitted()
        {
            return None;
        }
        self.field_load_memo_scan();
        let &(prev, splices_at, cell, _) = self.field_load_memo.get(&(base, field_index))?;
        // Round 16 (lane fieldguard, CHN15-5 (a)): an entry a `putfield`
        // recorded. Its value is the narrowed stored value, which must have
        // the type this read would produce; the store must still be a plain
        // primitive store (nothing rewrites it before lowering, but say so).
        if cell != prev {
            let plain_prim_store = self.graph.nodes.get(cell as usize).is_some_and(|s| {
                matches!(s.op, Op::Store(k) if k != MemKind::Ref) && !s.volatile_access
            });
            let same_type = self
                .graph
                .nodes
                .get(prev as usize)
                .is_some_and(|v| v.ty == ty && ty != IrType::Ref);
            if !plain_prim_store || !same_type {
                return None;
            }
            self.field_load_memo_census.store_hits += 1;
            if !self.field_store_forward_on {
                return None;
            }
            self.field_load_memo_census.store_forwarded += 1;
            return Some(prev);
        }
        let same_read = self
            .graph
            .nodes
            .get(prev as usize)
            .is_some_and(|n| matches!(n.op, Op::Load(_)) && n.ty == ty && !n.volatile_access);
        if !same_read {
            return None;
        }
        self.field_load_memo_census.hits += 1;
        if self.splices_done > splices_at {
            self.field_load_memo_census.hits_across_splice += 1;
        }
        // nio21 wave 2 (lane L3): a read of a `final` field forwards under
        // its own switch, whatever the general read switch says. The rules
        // above are unchanged -- same entry, same clears, same type check --
        // so this widens only WHICH reads the memo may answer, never when an
        // entry survives. See the "Final fields" section comment.
        let final_forward = is_final && self.field_final_forward_on;
        if is_final {
            self.field_load_memo_census.final_hits += 1;
        }
        if self.field_load_memo_mode != FieldLoadMemoMode::Forward && !final_forward {
            return None;
        }
        self.field_load_memo_census.forwarded += 1;
        if final_forward {
            self.field_load_memo_census.final_forwarded += 1;
        }
        Some(prev)
    }

    /// Record `load` as the current value of `base.#field_index`.
    pub(in crate::ir) fn field_load_memo_record(
        &mut self,
        base: NodeId,
        field_index: usize,
        load: NodeId,
        is_volatile: bool,
    ) {
        if self.field_load_memo_mode == FieldLoadMemoMode::Off
            || is_volatile
            || !self.memo_depth_admitted()
        {
            return;
        }
        // Classify everything up to and including `load` first, so the new
        // entry is judged only by nodes created after it.
        self.field_load_memo_scan();
        let depth = self.splice.len();
        self.field_load_memo
            .insert((base, field_index), (load, self.splices_done, load, depth));
        self.field_load_memo_census.recorded += 1;
    }

    /// Round nio20 wave 4 (lane L1, W3L4-2): may the memo record or answer at
    /// the walk's current splice depth? Always in the compiling method's own
    /// code; inside a spliced body only with [`Self::field_memo_in_splices`].
    pub(in crate::ir) fn memo_depth_admitted(&self) -> bool {
        self.splice.is_empty() || self.field_memo_in_splices
    }

    /// Round nio20 wave 4 (lane L1, W3L4-2): a `getfield` inside a spliced
    /// body was answered by the memo instead of building an `Op::Load`. The
    /// replay bookkeeping the load would have done (`note_splice_local_load`,
    /// `note_splice_field_read_alias`) still records that the body READ
    /// `base`'s field -- the part a constructor replay rule depends on -- and
    /// skips only the locality claim for a node that was never built (no
    /// locality is the conservative answer). A no-op outside a splice.
    pub(in crate::ir) fn note_splice_forwarded_read(&mut self, base: NodeId) {
        if self.splice.is_empty() {
            return;
        }
        if self.splice_ctor_receivers.contains(&base) {
            self.splice_ctor_receiver_read.insert(base);
        }
        self.note_splice_field_read_alias(base);
    }

    /// Round nio20 wave 4 (lane L1, W3L4-2): a join or a dead point of a
    /// SPLICED body (depth `d = self.splice.len() > 0`), with the memo running
    /// inside splices.
    ///
    /// # Why entries recorded at a smaller depth survive a body's forward join
    ///
    /// The depth-0 memo already relies on it: a spliced body is entered only
    /// through its `invoke`, every predecessor of each of its merges is inside
    /// it (the pre-scan registers a body's merges from its own decode), and
    /// the walk visits the whole body before the caller resumes. So an entry
    /// recorded at depth `< d` -- before the `invoke` that opened this body,
    /// on the straight line the memo's own clears leave it on -- dominates
    /// every merge of the body. Entries recorded AT depth `d` or deeper may be
    /// on one arm only, and go.
    ///
    /// # Why a loop header clears everything
    ///
    /// The scan is a forward walk over nodes as they are built: at a loop
    /// header the body's stores are not built yet, so a read in the loop
    /// answered from an entry recorded before it would take the pre-loop value
    /// on EVERY iteration, including those after the loop body stored a new
    /// one. The depth-0 rule clears at every merge, loop headers included, and
    /// this one keeps that for headers at any depth.
    ///
    /// A dead point (`goto`, a multi-return body's `return` edge) needs no
    /// rule of its own beyond the depth clear: the walk resumes at a merge.
    /// The multi-return join itself is [`Self::finish_multi_return_splice`]'s.
    pub(in crate::ir) fn field_load_memo_clear_at_splice_join(&mut self, loop_header: bool) {
        if loop_header {
            self.field_load_memo_clear_at_join();
        } else {
            self.field_load_memo_clear_from_depth(self.splice.len());
        }
    }

    /// Drop every memo entry recorded at splice depth `depth` or deeper.
    pub(in crate::ir) fn field_load_memo_clear_from_depth(&mut self, depth: usize) {
        self.frozen_scope_clear_from(depth);
        let before = self.field_load_memo.len();
        self.field_load_memo
            .retain(|_, &mut (_, _, _, d)| d < depth);
        if self.field_load_memo.len() != before {
            self.field_load_memo_census.cleared_at_join += 1;
        }
    }

    /// Round 16 (lane fieldguard, CHN15-5 (a)): record that the plain
    /// `putfield` `store` left `value` (already narrowed) in
    /// `base.#field_index`. Primitive kinds only; volatile stores are not
    /// recorded, and spliced ones only under [`Self::field_memo_in_splices`]
    /// (round nio20 wave 4; see the section comment). Recorded whenever the
    /// memo runs, so the census counts the hits with the switch off.
    pub(in crate::ir) fn field_load_memo_record_store(
        &mut self,
        base: NodeId,
        field_index: usize,
        store: NodeId,
        value: NodeId,
        kind: MemKind,
        is_volatile: bool,
    ) {
        if self.field_load_memo_mode == FieldLoadMemoMode::Off
            || is_volatile
            || kind == MemKind::Ref
            || !self.memo_depth_admitted()
            || base == NO_NODE
            || value == NO_NODE
        {
            return;
        }
        // Classify everything up to and including `store` first: the store
        // drops every entry it may overwrite (the old one for this cell
        // included) before its own is inserted.
        self.field_load_memo_scan();
        let depth = self.splice.len();
        self.field_load_memo.insert(
            (base, field_index),
            (value, self.splices_done, store, depth),
        );
        self.field_load_memo_census.store_recorded += 1;
    }

    /// JIT round 23 wave 2e, lane L2 (W4L1-2): the memo at pc `pc` of the
    /// compiling method's own code, a join or a dead point.
    ///
    /// At a FORWARD join whose every forward predecessor edge was registered
    /// by [`Self::add_merge_predecessor`] (its memo snapshot taken then), the
    /// memo keeps exactly the entries that every edge -- each snapshot, and the
    /// fall-through state when control reaches `pc` live -- carries
    /// UNCHANGED (same value, same recording node). Sound by the memo's
    /// invariant, "every live entry's value dominates the walk point and
    /// nothing on the path since its record wrote the cell": each edge's entry
    /// satisfies it on that edge, the entry is one SSA node on all of them,
    /// so it dominates the join and no path into the join wrote the cell. A
    /// loop header, a dead point that is no merge, a block walk, an edge some
    /// other route registered (the counts disagree) or the switch off clears,
    /// as before ([`Self::field_load_memo_clear_at_join`]).
    pub(in crate::ir) fn field_load_memo_join(&mut self, pc: usize) {
        let snapshots = self.field_memo_join_snapshots.remove(&pc);
        let forward_join = self.field_memo_precise_joins
            && !self.block_walk
            && !self.loop_headers.contains(&pc)
            && !self.loop_phis.contains_key(&pc);
        let registered = self.merges.get(&pc).map(|m| m.ctrl_inputs.len());
        // JIT round 23 wave 3b (the w2 test failure): the verifier lists every
        // conditional branch's FALL-THROUGH successor as a merge target
        // (`VerifiedCode::merge_targets`), so the instruction right after an
        // `ifeq` is a "join" whose only edge is the fall-through itself (no
        // edge registered yet, no snapshot). Nothing joins there: the memo is
        // the fall-through's, kept as is. Clearing it there undid every
        // precise join after it.
        if forward_join
            && registered == Some(0)
            && self.ctrl_opt().is_some()
            && snapshots.as_ref().is_none_or(|s| s.is_empty())
        {
            return;
        }
        // `CRATONVM_FROZEN_PAGES`: a real join or a dead point.
        self.frozen_scope_clear_from(0);
        let Some(snapshots) =
            snapshots.filter(|s| forward_join && !s.is_empty() && registered == Some(s.len()))
        else {
            self.field_load_memo_clear_at_join();
            return;
        };
        let live = self.ctrl_opt().is_some();
        if live {
            self.field_load_memo_scan();
        }
        let before = self.field_load_memo.len();
        let fall_through = live.then(|| std::mem::take(&mut self.field_load_memo));
        let kept = field_memo_intersection(fall_through, snapshots);
        if kept.len() < before {
            self.field_load_memo_census.cleared_at_join += 1;
        }
        self.field_load_memo = kept;
    }

    /// A join or dead control of the compiling method's own code.
    pub(in crate::ir) fn field_load_memo_clear_at_join(&mut self) {
        self.frozen_scope_clear_from(0);
        if !self.field_load_memo.is_empty() {
            self.field_load_memo.clear();
            self.field_load_memo_census.cleared_at_join += 1;
        }
    }

    /// Classify every node the graph gained since the last scan and drop the
    /// entries it may have overwritten. See the section comment.
    pub(in crate::ir) fn field_load_memo_scan(&mut self) {
        let end = self.graph.nodes.len();
        let from = std::mem::replace(&mut self.field_load_memo_scan_from, end);
        if self.field_load_memo.is_empty() {
            return;
        }
        for id in from..end {
            let Some(node) = self.graph.nodes.get(id) else {
                break;
            };
            // A φ performs no access of its own (a memory φ joins token
            // chains whose writers are scanned themselves), and a `Return`
            // ends every path it is on.
            if matches!(node.op, Op::Phi | Op::Return) {
                continue;
            }
            let store_kind = match &node.op {
                Op::Store(k) => Some(*k),
                _ => None,
            };
            let Ok(nid) = NodeId::try_from(id) else {
                self.field_load_memo.clear();
                return;
            };
            // JIT round 23 wave 2e (W4L1-1): a guarded diamond's miss call,
            // applied at its join (`finish_multi_return_splice`).
            if self.field_memo_deferred_calls.contains(&nid) {
                continue;
            }
            let eff = self.graph.memory_effect(nid);
            if eff.order != MemOrder::Plain {
                self.field_load_memo.clear();
                self.field_load_memo_census.cleared_by_ordering += 1;
                return;
            }
            if matches!(eff.writes, AliasClass::Any) {
                self.field_load_memo.clear();
                self.field_load_memo_census.cleared_by_opaque += 1;
                return;
            }
            if eff.writes.is_none() {
                continue;
            }
            let graph = &self.graph;
            let before = self.field_load_memo.len();
            // Round 15 wave 3 (lane chain2, CHN15-2): pairs only the field
            // identity separates, whether or not the switch lets it.
            let mut by_identity = 0usize;
            // The entry's CELL is its memory node's address: a read's `reads`
            // class, or (round 16, lane fieldguard, a store-recorded entry)
            // the recording store's `writes` class -- the same
            // `AliasClass::Field` either way.
            self.field_load_memo.retain(|_, &mut (_, _, load, _)| {
                let (load_kind, entry_is_store) =
                    match graph.nodes.get(load as usize).map(|n| &n.op) {
                        Some(Op::Load(k)) => (Some(*k), false),
                        Some(Op::Store(k)) => (Some(*k), true),
                        _ => (None, false),
                    };
                if let (Some(s), Some(l)) = (store_kind, load_kind) {
                    if s != l {
                        return true;
                    }
                }
                let cell_eff = graph.memory_effect(load);
                let reads = if entry_is_store {
                    cell_eff.writes
                } else {
                    cell_eff.reads
                };
                if graph.field_decls_differ(nid, load)
                    && graph.may_alias_by_index(eff.writes, reads)
                {
                    by_identity += 1;
                }
                !graph.may_alias(eff.writes, reads)
            });
            self.field_load_memo_census.dropped_by_store += before - self.field_load_memo.len();
            self.field_load_memo_census.separated_by_field_identity += by_identity;
            if self.field_load_memo.is_empty() {
                return;
            }
        }
    }
}

// ── The static-load memo (round 16 wave 14, lane osrir, MON3-2) ──────
//
// `Op::LoadStatic` is `MemAccess::Opaque` (its helper route runs `<clinit>`),
// so every `getstatic` was a new node, a new memory token and a clear of the
// G14-1 field memo. `R16MonitorCrowd.noSync` (`B.acc += B.counter;
// B.counter++`) read `B` three times per iteration, so each `getfield` of `B`
// had its own base node (its own null check) and none could be forwarded.
//
// This memo answers a plain (non-volatile) `getstatic` of the compiling
// method's own code with the node an earlier read of the same `(class,
// field)` built, when nothing between them can have written that static. It
// is the field memo's shape (same scope, same scan), with the classification
// a static needs:
//
// * **What writes a static.** Statics live in a Rust-side table, never in a
//   heap object, so a plain `putfield` / array store cannot reach one. A
//   `putstatic` writes its own cell, and only that cell when its class is
//   initialised (the helper's `<clinit>` guard returns at once); otherwise it
//   may run `<clinit>`, which may write any static. Anything else that can run
//   Java -- a call, an allocation (`new` initialises its class), an `ldc`, a
//   type check, a monitor, a throw, an unbox -- is the top of the lattice
//   (`memory_shape_of` says `Opaque`), and clears the memo.
// * **Class initialisation.** A `getstatic` of a class that may not be
//   initialised may run `<clinit>`, so it clears the memo too, unless the
//   compile's resolver reported the class initialised
//   ([`IrBuilder::set_static_initialized_classes`]) or a live entry already
//   read a static of that class on this path (JVMS 5.5: the first read ran or
//   joined its initialisation; a second read never runs `<clinit>`). The
//   forwarded read itself needs no such test: the earlier read ran first on
//   every path to it.
// * **Ordering.** A volatile read or write, or any monitor, is a JMM
//   synchronisation action; a later read may not take a value from before it.
//   Volatile statics are neither recorded nor forwarded, and any node that
//   orders memory clears the memo. A `Guard` (a deopt, no store) and a
//   safepoint poll are not synchronisation actions.
// * **Scope.** Cleared at every join and dead control of the method's own
//   code, exactly where the field memo is, so the earlier read dominates the
//   later one. Loop headers are joins: this is CSE inside one iteration, not
//   hoisting across iterations (that is LICM's, and `LoadStatic` is still a
//   barrier to it; see `jit-r16-osrir-proposals-RETIRED-20261006.md`).
// * **Redefinition.** A redefinition keeps static values and withdraws the
//   body; nothing here bakes an address.
//
// Default ON since wave 14 (`R16MonitorCrowd` long 18 -> 12, mixed 30 -> 27,
// ring 305 -> 281 ns/op, nothing slower; `=0` turns it off). With
// `CRATONVM_DBG_JITC` / `CRATONVM_DBG_IR_COMPILES` and the switch off the memo
// runs in census mode: it counts what it would forward and changes nothing.
//
// # JIT round 21 (lane staticfwd, OI-4): store-to-load forwarding
//
// A `putstatic` used to only DROP its cell (`StaticMemoEffect::DropCell`), so
// `S = S + 1; return S;` re-read the static it had just written. With
// `CRATONVM_JIT_IR_STATIC_STORE_FORWARD` (default ON) a plain depth-0
// PRIMITIVE `putstatic` records `(class, field) -> the stored value` (already
// narrowed to the field's type by `narrow_to_field_type`, i.e. exactly what a
// read of the cell returns, JVMS 6.5 `putstatic`), and a later plain
// `getstatic` of the same static with no possible write between is answered
// with it. The read memo's argument holds unchanged:
//
// * **No write between.** The entry lives in the same map, so the same scan
//   drops it: another `putstatic` of the cell drops it, and every call,
//   allocation, monitor, volatile access, class-initialising `getstatic` /
//   `putstatic` or other opaque node clears the memo. A safepoint poll and a
//   `Guard` do not (neither writes a static or synchronises).
// * **Class initialisation.** The store's own `<clinit>` (if its class was not
//   known initialised) ran BEFORE the write, and the store node clears the
//   memo when classified, so the entry is inserted after that clear. After a
//   `putstatic` completes, its class is initialised or being initialised by
//   this thread, so a later `getstatic` of it runs no `<clinit>` (JVMS 5.5):
//   the same rule that lets a live read entry vouch for its class.
// * **JMM.** A read may see the program-order-last write of its own thread
//   when no synchronisation action lies between (JLS 17.4.5); every volatile
//   access and monitor clears the memo, so a value another thread published
//   behind a volatile handshake is re-read. Volatile statics are never
//   recorded.
// * **Scope.** Depth 0, cleared at every join and dead control; inside a
//   splice only under `CRATONVM_JIT_IR_STATIC_MEMO_IN_SPLICES` (t1 wave 7
//   lane NBC, STATICFWD21-2, opt-in), at the body's depth, so the body's joins
//   drop its entries.
// * **Primitive only** by default, as the instance-field store forwarding: a
//   reference `putstatic` keeps the collector out of the forwarded set.
//   STATICFWD21-3 (`CRATONVM_JIT_IR_STATIC_STORE_FORWARD_REF=1` on top, t1
//   wave 7 lane NBC, opt-in) forwards references too: the stored reference is
//   an SSA value already live across every safepoint between the store and
//   the read, so the collector updates it like any other SSA reference (the
//   frame maps name it); the store's own write barrier is unchanged, and the
//   read's load barrier is not needed for a value that never left a register
//   / frame slot.

/// `CRATONVM_JIT_IR_STATIC_LOAD_FORWARD` (round 16 wave 14, lane osrir,
/// MON3-2) -- default ON (`=0` off); forwards a plain `getstatic` to an earlier
/// read of the same static (see the section comment). Read once per build; no
/// static.
pub(crate) fn ir_static_load_forward_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_STATIC_STORE_FORWARD` (JIT round 21, lane staticfwd,
/// OI-4) -- default ON (`=0` off); answers a plain `getstatic` with the value
/// an earlier plain primitive `putstatic` of the same static stored, when
/// nothing between can have written the cell (see the section comment).
/// Independent of `CRATONVM_JIT_IR_STATIC_LOAD_FORWARD` (it runs the memo in
/// census mode when that switch is off). Read per use; no static.
pub(crate) fn ir_static_store_forward_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_STATIC_STORE_FORWARD_REF` (t1 wave 7 lane NBC,
/// STATICFWD21-3) -- **default OFF** (opt-in `=1`); with
/// [`ir_static_store_forward_enabled`] on, a reference (`L` / `[`) static's
/// stored value forwards too (lazy-holder `INSTANCE = x; return INSTANCE;`).
/// Inert without the primitive switch. Read per use; no static.
pub(crate) fn ir_static_store_forward_ref_enabled() -> bool {
    ir_static_store_forward_enabled()
        && false
}

/// `CRATONVM_JIT_IR_STATIC_MEMO_IN_SPLICES` (t1 wave 7 lane NBC,
/// STATICFWD21-2) -- **default OFF** (opt-in `=1`): the static memo's READ
/// entries are recorded and answered inside spliced bodies too (a spliced
/// getter re-reading a static the caller or an earlier splice read), scoped
/// by splice depth exactly as `CRATONVM_JIT_IR_FIELD_MEMO_IN_SPLICES` scopes
/// the field memo: a join of a body drops the entries that body (or a deeper
/// one) recorded, a loop header at any depth clears everything, and a
/// multi-return body's join drops its depth. With
/// `CRATONVM_JIT_IR_STATIC_STORE_FORWARD` on too, a spliced `putstatic`
/// records its value at its body's depth (t1 wave 8 lane STATICFWD; a
/// `static synchronized` body's own-class write, or an FG16-3 spliced
/// `putstatic`). Read once per build; no static.
pub(crate) fn ir_static_memo_in_splices_enabled() -> bool {
    false
}

/// How the MON3-2 memo runs in this build.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::ir) enum StaticLoadMemoMode {
    /// Neither the switch nor the census is on: the memo does nothing.
    Off,
    /// `CRATONVM_DBG_IR_COMPILES` / `CRATONVM_DBG_JITC` without the switch:
    /// count what would be forwarded, change nothing.
    Census,
    /// `CRATONVM_JIT_IR_STATIC_LOAD_FORWARD=1`: forward.
    Forward,
}

impl StaticLoadMemoMode {
    /// Read once per builder (per compile).
    pub(in crate::ir) fn from_flags() -> Self {
        if ir_static_load_forward_enabled() {
            StaticLoadMemoMode::Forward
        } else if ir_bail_reporting() || ir_static_store_forward_enabled() {
            // JIT round 21 (lane staticfwd, OI-4): store forwarding needs the
            // memo to run; its READ entries then stay census-only.
            StaticLoadMemoMode::Census
        } else {
            StaticLoadMemoMode::Off
        }
    }
}

/// The MON3-2 census, per build.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::ir) struct StaticLoadMemoCensus {
    /// Plain depth-0 `getstatic`s recorded.
    pub(in crate::ir) recorded: usize,
    /// `getstatic`s a live entry answered.
    pub(in crate::ir) hits: usize,
    /// ... of which were forwarded (0 in census mode).
    pub(in crate::ir) forwarded: usize,
    /// Joins / dead control that cleared a non-empty memo.
    pub(in crate::ir) cleared_at_join: usize,
    /// Nodes that may write a static or order memory, which cleared it.
    pub(in crate::ir) cleared_by_effect: usize,
    /// Entries dropped by a `putstatic` of the same field of an initialised
    /// class.
    pub(in crate::ir) dropped_by_store: usize,
    /// t1 wave 7 (lane NBC, OI-4): plain `putstatic`s recorded as the cell's
    /// value ...
    pub(in crate::ir) store_recorded: usize,
    /// ... of which stored a reference (STATICFWD21-3).
    pub(in crate::ir) store_recorded_ref: usize,
    /// `getstatic`s a store-recorded entry answered.
    pub(in crate::ir) store_hits: usize,
    /// ... of which were forwarded.
    pub(in crate::ir) store_forwarded: usize,
    /// STATICFWD21-2: entries (read or, since t1 wave 8, store) forwarded
    /// inside a spliced body.
    pub(in crate::ir) splice_forwarded: usize,
}

impl IrBuilder {
    /// The node an earlier plain read of `class_id`'s static `field_index`
    /// built, when the memo may forward it (see the section comment), else
    /// `None`. Counts the hit in census mode and still answers `None` there.
    pub(in crate::ir) fn static_load_memo_forward(
        &mut self,
        class_id: u32,
        field_index: u32,
        ty: IrType,
        is_volatile: bool,
    ) -> Option<NodeId> {
        if self.static_load_memo_mode == StaticLoadMemoMode::Off
            || is_volatile
            || !self.static_memo_depth_admitted()
        {
            return None;
        }
        self.static_load_memo_scan();
        let &(prev, stored, _) = self.static_load_memo.get(&(class_id, field_index))?;
        let same_read = self.graph.nodes.get(prev as usize).is_some_and(|n| {
            n.ty == ty
                && (stored
                    || matches!(
                        n.op,
                        Op::LoadStatic {
                            class_id: c,
                            field_index: f,
                            is_volatile: false,
                            ..
                        } if c == class_id && f == field_index
                    ))
        });
        if !same_read {
            return None;
        }
        // t1 wave 7 (lane NBC, OI-4): the value an earlier plain `putstatic`
        // of this cell stored. Recorded only with its own switch on
        // (`static_load_memo_record_store`), so it forwards in either mode.
        if stored {
            self.static_load_memo_census.store_hits += 1;
            if !ir_static_store_forward_enabled() {
                return None;
            }
            self.static_load_memo_census.store_forwarded += 1;
            if !self.splice.is_empty() {
                self.static_load_memo_census.splice_forwarded += 1;
            }
            return Some(prev);
        }
        self.static_load_memo_census.hits += 1;
        if self.static_load_memo_mode != StaticLoadMemoMode::Forward {
            return None;
        }
        self.static_load_memo_census.forwarded += 1;
        if !self.splice.is_empty() {
            self.static_load_memo_census.splice_forwarded += 1;
        }
        Some(prev)
    }

    /// t1 wave 7 (lane NBC, STATICFWD21-2): may the static memo record and
    /// answer at the current depth? Always in the compiling method's own
    /// code; inside a spliced body only with [`Self::static_memo_in_splices`].
    pub(in crate::ir) fn static_memo_depth_admitted(&self) -> bool {
        self.splice.is_empty()
    }

    /// t1 wave 7 (lane NBC, STATICFWD21-2): drop every static memo entry
    /// recorded at splice depth `depth` or deeper. The field memo's rule
    /// ([`Self::field_load_memo_clear_at_splice_join`] documents why entries
    /// of a smaller depth dominate every merge of a deeper body).
    pub(in crate::ir) fn static_load_memo_clear_from_depth(&mut self, depth: usize) {
        let before = self.static_load_memo.len();
        self.static_load_memo.retain(|_, &mut (_, _, d)| d < depth);
        if self.static_load_memo.len() != before {
            self.static_load_memo_census.cleared_at_join += 1;
        }
    }

    /// t1 wave 7 (lane NBC, STATICFWD21-2): a join or dead point INSIDE a
    /// spliced body, with the static memo running there: a loop header
    /// clears everything (the loop's writes are not built yet), any other
    /// join the entries this depth or deeper recorded.
    pub(in crate::ir) fn static_load_memo_clear_at_splice_join(&mut self, loop_header: bool) {
        if loop_header {
            self.static_load_memo_clear_at_join();
        } else {
            self.static_load_memo_clear_from_depth(self.splice.len());
        }
    }

    /// Record `load` as the current value of `class_id`'s static
    /// `field_index`.
    pub(in crate::ir) fn static_load_memo_record(
        &mut self,
        class_id: u32,
        field_index: u32,
        load: NodeId,
        is_volatile: bool,
    ) {
        if self.static_load_memo_mode == StaticLoadMemoMode::Off
            || is_volatile
            || !self.static_memo_depth_admitted()
        {
            return;
        }
        // Classify everything up to and including `load` first, so the new
        // entry is judged only by nodes created after it.
        self.static_load_memo_scan();
        let depth = self.splice.len();
        self.static_load_memo.insert((class_id, field_index), (load, false, depth));
        self.static_load_memo_census.recorded += 1;
    }

    /// t1 wave 7 (lane NBC; JIT round 21 lane staticfwd OI-4): the plain
    /// `putstatic` of `class_id`'s static `field_index` that the graph just
    /// gained left `value` (already narrowed to the field's type) in the
    /// cell. Primitive fields only unless
    /// [`ir_static_store_forward_ref_enabled`] (STATICFWD21-3); never
    /// volatile; inside a splice only with [`Self::static_memo_in_splices`]
    /// (t1 wave 8 lane STATICFWD, STATICFWD21-2: a spliced `putstatic` is a
    /// `static synchronized` body's own-class write, whose window cannot trap,
    /// or an FG16-3 write of an initialised class; the entry carries the
    /// body's depth, so the body's joins drop it, and a window's
    /// `MonitorExit` clears the memo at the close).
    /// See the section comment.
    pub(in crate::ir) fn static_load_memo_record_store(
        &mut self,
        class_id: u32,
        field_index: u32,
        type_tag: u8,
        value: NodeId,
        is_volatile: bool,
    ) {
        let is_ref = matches!(type_tag, b'L' | b'[');
        if self.static_load_memo_mode == StaticLoadMemoMode::Off
            || is_volatile
            || !self.static_memo_depth_admitted()
            || value == NO_NODE
            || !ir_static_store_forward_enabled()
            || (is_ref && !ir_static_store_forward_ref_enabled())
        {
            return;
        }
        // Classify everything up to and including the store first: it drops
        // the cell's old entry (or, for a class not known initialised, clears
        // the memo: its `<clinit>` ran BEFORE the write) before the stored
        // value goes in.
        self.static_load_memo_scan();
        let depth = self.splice.len();
        self.static_load_memo.insert((class_id, field_index), (value, true, depth));
        self.static_load_memo_census.store_recorded += 1;
        if is_ref {
            self.static_load_memo_census.store_recorded_ref += 1;
        }
    }

    /// A join or dead control of the compiling method's own code.
    pub(in crate::ir) fn static_load_memo_clear_at_join(&mut self) {
        if !self.static_load_memo.is_empty() {
            self.static_load_memo.clear();
            self.static_load_memo_census.cleared_at_join += 1;
        }
    }

    /// Classify every node the graph gained since the last scan and drop the
    /// entries it may have overwritten. See the section comment.
    pub(in crate::ir) fn static_load_memo_scan(&mut self) {
        let end = self.graph.nodes.len();
        let from = std::mem::replace(&mut self.static_load_memo_scan_from, end);
        if self.static_load_memo.is_empty() {
            return;
        }
        for id in from..end {
            let Some(node) = self.graph.nodes.get(id) else {
                break;
            };
            let memo = &self.static_load_memo;
            let known = &self.static_initialized_classes;
            let effect = static_load_memo_effect(node, |c| {
                known.contains(&c) || memo.keys().any(|&(k, _)| k == c)
            });
            match effect {
                StaticMemoEffect::Keep => {}
                StaticMemoEffect::DropCell(key) => {
                    if self.static_load_memo.remove(&key).is_some() {
                        self.static_load_memo_census.dropped_by_store += 1;
                    }
                }
                StaticMemoEffect::Clear => {
                    // A trusted non-null `static final` (initialised class, written
                    // by nobody after `<clinit>`) is no JMM-ordered read: keep it, so
                    // `synchronized (L) { synchronized (L) {..} }` names one lock node.
                    let trusted = &self.graph.static_final_non_null;
                    self.static_load_memo
                        .retain(|k, &mut (_, stored, _)| !stored && trusted.contains(k));
                    self.static_load_memo_census.cleared_by_effect += 1;
                }
            }
            if self.static_load_memo.is_empty() {
                return;
            }
        }
    }
}

/// What one node does to the MON3-2 memo.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::ir) enum StaticMemoEffect {
    /// It cannot write a static and orders nothing.
    Keep,
    /// It writes exactly this `(class, field)` static (a plain `putstatic` of
    /// an initialised class).
    DropCell((u32, u32)),
    /// It may write any static, or it is a synchronisation action.
    Clear,
}

/// [`IrBuilder::static_load_memo_scan`]'s classification of `node`;
/// `initialized(class)` says a `getstatic` / `putstatic` of `class` runs no
/// `<clinit>`. Pure, so the rules are unit-tested one by one.
pub(in crate::ir) fn static_load_memo_effect(node: &Node, initialized: impl Fn(u32) -> bool) -> StaticMemoEffect {
    match &node.op {
        // A φ performs no access of its own (a memory φ joins chains whose
        // writers are scanned themselves); a `Return` ends every path it is on.
        Op::Phi | Op::Return => StaticMemoEffect::Keep,
        // Heap cells, never a static's; a volatile one is an acquire/release.
        // STRINTR21-5: the String char read is the two `ArrayLoad`s it replaced.
        Op::Load(_)
        | Op::ArrayLoad(_)
        | Op::ArrayLength
        | Op::StrCharDecode
        | Op::Store(_)
        | Op::ArrayStore(_) => {
            if node.volatile_access {
                StaticMemoEffect::Clear
            } else {
                StaticMemoEffect::Keep
            }
        }
        Op::LoadStatic {
            class_id,
            is_volatile,
            ..
        } => {
            if !*is_volatile && initialized(*class_id) {
                StaticMemoEffect::Keep
            } else {
                StaticMemoEffect::Clear
            }
        }
        Op::StoreStatic {
            class_id,
            field_index,
            is_volatile,
            ..
        } => {
            if !*is_volatile && initialized(*class_id) {
                StaticMemoEffect::DropCell((*class_id, *field_index))
            } else {
                StaticMemoEffect::Clear
            }
        }
        // Everything else by the exhaustive coarse table: what touches no
        // memory keeps the memo; calls, allocations, class-loading helpers,
        // monitors, throws, `Dead`, the OSR arm's memory -- the top of the
        // lattice -- clear it. Every `Reads` / `Writes` op is listed above, so
        // a future one lands in the conservative arm.
        op => match memory_shape_of(op) {
            MemoryShape::None => StaticMemoEffect::Keep,
            MemoryShape::Reads | MemoryShape::Writes | MemoryShape::Opaque => {
                StaticMemoEffect::Clear
            }
        },
    }
}

/// t1 wave 7 (lane NBC): the static memo's pieces beyond OI-4 (whose own
/// tests are `jr21_staticfwd_static_store_forward_tests`): STATICFWD21-3
/// (reference statics, `CRATONVM_JIT_IR_STATIC_STORE_FORWARD_REF`) and
/// STATICFWD21-2 (`CRATONVM_JIT_IR_STATIC_MEMO_IN_SPLICES`), plus the
/// class-initialising read that clears a stored entry.










// ── Helpers ──────────────────────────────────────────────────────────

/// The inclusive key bounds of an [`Op::Switch`] for this decoded switch
/// payload, or `None` when the payload must keep the `Op::Cmp`/`Op::If` chain.
///
/// `cases` is `bytecode_analysis::SwitchTable::cases` — `(match value, absolute
/// target)` in TABLE order, which for a `tableswitch` is one entry per slot in
/// `low ..= high` and for a `lookupswitch` is the listed pairs.
///
/// Three refusals, each of them a correctness requirement rather than a
/// heuristic:
///
/// * **Empty.** A `tableswitch` with `high < low` or a `lookupswitch` with
///   `npairs == 0` is legal bytecode that means "always take the default".
///   It has no key range at all, and the chain handles it by falling straight
///   through to the default edge.
/// * **Not strictly ascending.** JVMS §6.5 *requires* a `lookupswitch`'s pairs
///   to be sorted and distinct, and the verifier is entitled to assume it — but
///   nothing on this path checks it, and a duplicate key would want two
///   projections for one slot of the dense form (`ir_verify::check_projections`
///   rejects the second, and before that the later one would silently win).
///   An unsorted or duplicated payload therefore keeps the chain, which
///   compares against every listed key in table order and so preserves the
///   JVMS "first match wins" reading whatever the payload contains.
/// * **Too sparse, or too wide.** [`switch_is_representable`] — see there.
///
/// # The bounds are the LIVE bounds
///
/// `low`/`high` come from the first and last entry whose target is not the
/// default, not from the first and last entry. A `tableswitch` pads its payload
/// with the default offset — `switch (x) { case 3: case 4: }` over a method
/// whose other branches made javac pick `low = 0` lists slots 0, 1 and 2 as
/// default — and a range that included those slots would spend projections on
/// keys the default edge already covers. Trimming them costs nothing: a key
/// outside `[low, high]` takes the default edge, which is exactly where those
/// slots were going.
///
/// The ascending check comes FIRST, because the bounds are read off the ends of
/// the live entries and only a sorted list has its extremes there.
pub(in crate::ir) fn dense_switch_bounds(cases: &[(i32, usize)], default_target: usize) -> Option<(i32, i32)> {
    if !cases.windows(2).all(|w| w[0].0 < w[1].0) {
        return None;
    }
    let mut low: Option<i32> = None;
    let mut high = 0i32;
    let mut live_count = 0usize;
    for &(key, target) in cases {
        if target == default_target {
            continue;
        }
        if low.is_none() {
            low = Some(key);
        }
        high = key;
        live_count += 1;
    }
    // Every listed key pointed at the default: there is nothing for a switch
    // node to dispatch on, and the chain's trailing jump to the default is
    // already the whole instruction.
    let low = low?;
    if !switch_is_representable(low, high, live_count) {
        return None;
    }
    Some((low, high))
}

/// A sparse switch needs at least this many LIVE keys before the builder
/// replaces its compare chain with a search tree. Below it the chain is at
/// most this many compares and no deeper than the tree would be anyway.
pub(in crate::ir) const SWITCH_SEARCH_TREE_MIN_CASES: usize =
    super::sparse_switch::SPARSE_SWITCH_MIN_CASES;

/// A search-tree leaf covers at most this many live keys with an equality
/// chain; a larger range is split on its middle key.
pub(in crate::ir) const SWITCH_SEARCH_TREE_LEAF_CASES: usize = 3;

/// The live `(key, target)` pairs of a switch the dense form refused, when the
/// builder should dispatch on them through a balanced search tree, or `None`
/// to keep the historical equality chain (round 12 wave 2, lane irbuild).
///
/// # Why
///
/// [`dense_switch_bounds`] refuses a switch whose keys are spread wider than
/// [`SWITCH_MAX_WASTE_FACTOR`] slots per live key, and the chain it falls back
/// to costs one compare per LISTED key at run time. That is the shape of every
/// `String` switch (a `lookupswitch` over `hashCode()` values, which are
/// scattered over the whole `int` range) and of any enum-like dispatch on
/// sparse constants: 32 cases meant up to 32 sequential compares where
/// HotSpot's lookupswitch lowering does a binary search of 5.
///
/// # When it is sound
///
/// Only for a STRICTLY ascending payload. JVMS §6.5 requires that of a
/// `lookupswitch` and a `tableswitch` is ascending by construction, but nothing
/// on this path verifies it; an unsorted or duplicated payload keeps the chain,
/// whose table-order compares give "first match wins" whatever the payload
/// holds. With distinct keys, dropping an entry whose target IS the default is
/// exact: that key reaches the default through the tree's fall-through.
///
/// `CRATONVM_JIT_IR_SWITCH_SEARCH_TREE=0` keeps the chain for every switch.
pub(in crate::ir) fn switch_search_tree_cases(
    cases: &[(i32, usize)],
    default_target: usize,
) -> Option<Vec<(i32, usize)>> {
    super::sparse_switch::sparse_switch_cases(cases, default_target)
}

/// The set of bytecode pcs reachable from method entry following only
/// **normal** control flow — branches, switches and fall-through, never an
/// exception edge.
///
/// Everything outside this set is code the compiled body can never execute:
/// in practice the bodies of `catch` / `finally` handlers, which are entered
/// only by the interpreter after the JIT frame has already returned its
/// `i64::MIN` sentinel. [`IrBuilder::build`] skips those pcs entirely rather
/// than abstract-interpreting them with stale state (see the STUB-S8 comment
/// there for what that used to produce).
///
/// Edges come from [`cratonvm_reader::VerifiedCode::successors`], the same
/// canonical CFG contract the verifier and `merge_targets` are derived from,
/// so this cannot disagree with the merge bookkeeping about where an
/// instruction ends or which targets it has.
pub(in crate::ir) fn normally_reachable_pcs(
    verified: &cratonvm_reader::VerifiedCode,
    code_len: usize,
) -> HashSet<usize> {
    let mut reachable = HashSet::with_capacity(verified.instructions().len());
    if code_len == 0 {
        return reachable;
    }
    let mut work = vec![0usize];
    reachable.insert(0usize);
    while let Some(pc) = work.pop() {
        for succ in verified.successors(pc) {
            if succ < code_len && reachable.insert(succ) {
                work.push(succ);
            }
        }
    }
    reachable
}

/// nio21 wave 17 (lane L4): [`normally_reachable_pcs`] with the `(from, to)`
/// edges in `removed` deleted (the taken edges of pruned never-taken
/// branches; see `IrBuilder::never_taken_pc_is_dead`).
pub(in crate::ir) fn reachable_pcs_without_edges(
    verified: &cratonvm_reader::VerifiedCode,
    code_len: usize,
    removed: &[(usize, usize)],
) -> HashSet<usize> {
    let mut reachable = HashSet::with_capacity(verified.instructions().len());
    if code_len == 0 {
        return reachable;
    }
    let mut work = vec![0usize];
    reachable.insert(0usize);
    while let Some(pc) = work.pop() {
        for succ in verified.successors(pc) {
            if removed.contains(&(pc, succ)) {
                continue;
            }
            if succ < code_len && reachable.insert(succ) {
                work.push(succ);
            }
        }
    }
    reachable
}

/// JIT round 19 wave 5 (lane handler5, SPLICE4-1): an admitted handler's
/// region ([`catch_handler_region`]) and whether an arm opened it.
#[derive(Clone, Debug, Default)]
pub(in crate::ir) struct CatchHandlerRegion {
    /// Every pc of the region, sorted: the handler pc and what normal flow
    /// reaches from it before rejoining normally reachable code.
    pub(in crate::ir) pcs: Vec<usize>,
    /// The region's own merge targets other than the handler pc (whose merge
    /// the first arm creates).
    pub(in crate::ir) merges: Vec<usize>,
    /// Has an arm entered it (so its pcs are walked and its merges exist)?
    pub(in crate::ir) opened: bool,
}

/// SPLICE4-1: an upper bound on a compiled handler region's instructions. A
/// handler that size is no longer the cheap `catch` this entry is for, and
/// the walk's budget is the caller's code length.
pub(in crate::ir) const CATCH_REGION_MAX_PCS: usize = 512;

/// SPLICE4-1: the region of the handler at `handler`: every pc normal flow
/// reaches from it that `reachable` (normal flow from entry) does not hold.
/// `None` -- no compiled entry for this handler -- when the region holds a
/// loop header or a `jsr`/`ret`, is larger than [`CATCH_REGION_MAX_PCS`],
/// runs off the method, or rejoins normally reachable code anywhere but at a
/// pc the builder already has a merge for (`merges`): the walk enters such a
/// pc with the region's state as one more predecessor, which only a merge
/// records.
#[allow(clippy::too_many_arguments)]
pub(in crate::ir) fn catch_handler_region(
    code: &[u8],
    code_len: usize,
    verified: &cratonvm_reader::VerifiedCode,
    reachable: &HashSet<usize>,
    merge_targets: &HashSet<usize>,
    headers: &HashSet<usize>,
    merges: &HashMap<usize, MergeState>,
    handler: usize,
) -> Option<CatchHandlerRegion> {
    let mut pcs: Vec<usize> = Vec::new();
    let mut seen: HashSet<usize> = HashSet::new();
    let mut work = vec![handler];
    seen.insert(handler);
    while let Some(p) = work.pop() {
        if headers.contains(&p) || verified.instruction_at(p).is_none() {
            return None;
        }
        let op = *code.get(p)?;
        if crate::bytecode_analysis::is_subroutine_op(op)
            || crate::bytecode_analysis::is_wide_ret(code, p)
        {
            return None;
        }
        pcs.push(p);
        if pcs.len() > CATCH_REGION_MAX_PCS {
            return None;
        }
        for s in verified.successors(p) {
            if s >= code_len {
                return None;
            }
            if reachable.contains(&s) {
                if !merges.contains_key(&s) {
                    return None;
                }
                continue;
            }
            if seen.insert(s) {
                work.push(s);
            }
        }
    }
    pcs.sort_unstable();
    let region_merges = pcs
        .iter()
        .copied()
        .filter(|&p| p != handler && merge_targets.contains(&p))
        .collect();
    Some(CatchHandlerRegion {
        pcs,
        merges: region_merges,
        opened: false,
    })
}

/// Does the caller's code have a loop header that the pc-order walk in
/// [`IrBuilder::build`] cannot build?
///
/// That is a reachable header other than pc 0 with no reachable predecessor at
/// a LOWER pc. At pc 0 the method entry itself is the forward entry. The pc
/// walk activates a header when it arrives, and needs an entry edge recorded
/// by then. A rotated loop's body, `goto test; body: …; test: if<cond> body`,
/// has no such edge. Its only predecessors are the back edge from the test and,
/// through the test, the `goto`, and both sit at higher pcs or target the test
/// instead.
///
/// `loop_headers` is the caller's own set: the verifier's "target of a
/// backward branch" headers, filtered to reachable pcs.
pub(in crate::ir) fn has_rotated_loop_header(
    verified: &cratonvm_reader::VerifiedCode,
    reachable: &HashSet<usize>,
    loop_headers: &HashSet<usize>,
    code_len: usize,
) -> bool {
    // A method with no loop, which is most methods, pays nothing more.
    if !loop_headers.iter().any(|&h| h != 0 && h < code_len) {
        return false;
    }
    let mut entered_forward: HashSet<usize> = HashSet::new();
    for insn in verified.instructions() {
        let pc = insn.pc as usize;
        if pc >= code_len || !reachable.contains(&pc) {
            continue;
        }
        for succ in verified.successors(pc) {
            if succ > pc && loop_headers.contains(&succ) {
                entered_forward.insert(succ);
            }
        }
    }
    loop_headers
        .iter()
        .any(|&h| h != 0 && h < code_len && !entered_forward.contains(&h))
}

/// The order in which [`IrBuilder::build`] walks a method that has a rotated
/// loop (see [`has_rotated_loop_header`]): the reachable caller bytecode cut
/// into basic blocks, in reverse post-order of the block CFG.
///
/// # Why reverse post-order
///
/// In RPO every edge goes forward except the retreating ones, and in a
/// reducible CFG those are exactly the back edges, whose targets dominate their
/// sources. So when the walk reaches a block, every forward predecessor has
/// already recorded its snapshot, which is the invariant `activate_merge` and
/// `activate_loop_header` both rest on. Every later predecessor is a back edge,
/// which `add_merge_predecessor` back-patches into the header's phis. The pc
/// walk has the same invariant for javac's top-tested loops only because pc
/// order happens to be such an order for them.
///
/// The loop headers are therefore the targets of the edges that are retreating
/// in THIS order, not the verifier's backward-branch targets. In
/// `goto test; body: …; test: if<cond> body` the header is the test, which
/// dominates the body. The body becomes an ordinary one-predecessor merge.
///
/// # What the walk relies on, and checks
///
/// * **A block reached only by fall-through is placed right after the block
///   that falls into it.** Such a block is not a merge target, so only the
///   live walk state carries its entry. The DFS visits a block's fall-through
///   successor LAST (`VerifiedCode::successors` lists it last), and that puts
///   the successor immediately after its only predecessor in reverse
///   post-order.
/// * **Every loop header is a merge target**, so there is an `Op::Merge` to
///   hang its phis on.
///
/// A method that breaks either is refused (`None`), never walked.
pub(in crate::ir) struct BlockWalk {
    /// `[start, end)` bytecode ranges in walk order. Blocks adjacent both in
    /// the order and in the bytecode are coalesced into one range, so a range
    /// ends exactly where the order leaves the bytecode's own sequence. The
    /// first range starts at pc 0.
    pub(in crate::ir) ranges: Vec<(usize, usize)>,
    /// Start pcs of the blocks targeted by an edge that is retreating in this
    /// order.
    pub(in crate::ir) headers: HashSet<usize>,
}

impl BlockWalk {
    /// Cut the reachable instructions below `code_len` into blocks and order
    /// them. `None` for a CFG whose shape breaks one of the checks on
    /// [`BlockWalk`].
    pub(in crate::ir) fn reverse_postorder(
        verified: &cratonvm_reader::VerifiedCode,
        reachable: &HashSet<usize>,
        code_len: usize,
    ) -> Option<BlockWalk> {
        // Reachable instructions in pc order, as `(pc, next_pc)`.
        let insns: Vec<(usize, usize)> = verified
            .instructions()
            .iter()
            .map(|insn| (insn.pc as usize, insn.next_pc as usize))
            .filter(|&(pc, _)| pc < code_len && reachable.contains(&pc))
            .collect();
        if insns.first().map(|&(pc, _)| pc) != Some(0) {
            return None;
        }
        let merge_targets: HashSet<usize> = verified
            .merge_targets()
            .iter()
            .map(|&t| t as usize)
            .collect();

        // Block leaders are: the entry; every merge target; every successor of
        // an instruction that does not simply fall through, and the
        // instruction after it; and any instruction whose textual predecessor
        // is unreachable.
        let mut leaders: HashSet<usize> = HashSet::new();
        leaders.insert(0);
        for (k, &(pc, next)) in insns.iter().enumerate() {
            if merge_targets.contains(&pc) || (k > 0 && insns[k - 1].1 != pc) {
                leaders.insert(pc);
            }
            let succs = verified.successors(pc);
            if !(succs.len() == 1 && succs[0] == next) {
                leaders.extend(succs.iter().copied());
                leaders.insert(next);
            }
        }

        // Blocks, numbered in pc order: start, end (the `next_pc` of the last
        // instruction), and the last instruction's pc. Block 0 starts at pc 0.
        let mut starts: Vec<usize> = Vec::new();
        let mut ends: Vec<usize> = Vec::new();
        let mut lasts: Vec<usize> = Vec::new();
        for &(pc, next) in &insns {
            if leaders.contains(&pc) || starts.is_empty() {
                starts.push(pc);
                ends.push(next);
                lasts.push(pc);
            } else {
                let b = ends.len() - 1;
                ends[b] = next;
                lasts[b] = pc;
            }
        }
        let nb = starts.len();
        let block_at: HashMap<usize, usize> =
            starts.iter().enumerate().map(|(b, &s)| (s, b)).collect();

        // Successor blocks, branch targets first and the fall-through last.
        let mut succ: Vec<Vec<usize>> = Vec::with_capacity(nb);
        for &last in &lasts {
            let mut out = Vec::new();
            for s in verified.successors(last) {
                if s >= code_len {
                    continue;
                }
                // Every successor of a block's last instruction is a leader
                // by construction; a miss means the cut above is wrong.
                out.push(*block_at.get(&s)?);
            }
            succ.push(out);
        }

        // Iterative DFS from the entry: a long chain of blocks must not recurse.
        let mut seen = vec![false; nb];
        let mut postorder: Vec<usize> = Vec::with_capacity(nb);
        let mut dfs: Vec<(usize, usize)> = vec![(0, 0)];
        seen[0] = true;
        while let Some(&(b, i)) = dfs.last() {
            if i < succ[b].len() {
                let top = dfs.len() - 1;
                dfs[top].1 = i + 1;
                let s = succ[b][i];
                if !seen[s] {
                    seen[s] = true;
                    dfs.push((s, 0));
                }
            } else {
                dfs.pop();
                postorder.push(b);
            }
        }
        // Every block holds reachable code, so the DFS must reach all of them.
        if postorder.len() != nb {
            return None;
        }
        let order: Vec<usize> = postorder.into_iter().rev().collect();
        let mut pos = vec![0usize; nb];
        for (k, &b) in order.iter().enumerate() {
            pos[b] = k;
        }

        let mut headers: HashSet<usize> = HashSet::new();
        for b in 0..nb {
            for &s in &succ[b] {
                let target = starts[s];
                if pos[s] <= pos[b] {
                    if !merge_targets.contains(&target) {
                        return None;
                    }
                    headers.insert(target);
                }
                if target == ends[b] && !merge_targets.contains(&target) && pos[s] != pos[b] + 1 {
                    return None;
                }
            }
        }

        let mut ranges: Vec<(usize, usize)> = Vec::new();
        for &b in &order {
            match ranges.last_mut() {
                Some(last) if last.1 == starts[b] => last.1 = ends[b],
                _ => ranges.push((starts[b], ends[b])),
            }
        }
        Some(BlockWalk { ranges, headers })
    }
}

/// A call-site intrinsic family the optimizing tier lowers as arithmetic.
///
/// The first families (min/max/abs/compare) are branchless baseline x86-64
/// (`CMP`, `CMOVcc`, `SETcc`, `SAR`, `XOR`, `SUB`). Later ones are not all so:
/// the FP min/max take local branches, and the rounding, `fma` and `bitCount`
/// rows need SSE4.1 / FMA3 / POPCNT and are recognised only where the host has
/// them (see each group's comment and [`try_ir_scalar_intrinsic`]). Families
/// that are plain arithmetic over existing nodes are not members at all: they
/// are expanded by `ir::pure_expand` (j27 wave 2, lane INTRINS). Coverage
/// against HotSpot 25: `docs/feature-designs/jit-ir-optimizations.md`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ScalarOp {
    /// `Math.min(int,int)` / `StrictMath.min`.
    MinI,
    /// `Math.max(int,int)`.
    MaxI,
    /// `Math.min(long,long)`.
    MinL,
    /// `Math.max(long,long)`.
    MaxL,
    /// `Math.abs(int)`. NOTE `Math.abs(Integer.MIN_VALUE) == Integer.MIN_VALUE`
    /// by JLS 15.15.4, which the branchless sequence reproduces exactly (the
    /// two's-complement negation of the minimum is itself); a `CMOV` form
    /// written from the obvious `x < 0 ? -x : x` would too, but only because
    /// the negation wraps -- worth knowing before anyone "simplifies" it.
    AbsI,
    /// `Math.abs(long)`, same edge at `Long.MIN_VALUE`.
    AbsL,
    /// `Integer.compare(int,int)` -> `-1`/`0`/`1`, SIGNED.
    CompareI,
    /// `Long.compare(long,long)` -> `-1`/`0`/`1`, SIGNED.
    CompareL,

    // -- Bit-scan families, 2026-09-06 -------------------------------
    //
    // `refused_method=59` on H2 was the work list these come off. Every one
    // uses BSR / BSF / BSWAP / ROL / ROR, which are BASELINE x86-64 -- no
    // POPCNT, LZCNT or TZCNT, and so no CPU feature probe and no fallback.
    // `Integer.bitCount` is deliberately still absent for exactly that reason:
    // it wants POPCNT or a twelve-instruction SWAR, and a family that is
    // *sometimes* an intrinsic makes its own A/B a measurement of the host.
    //
    // The ZERO input is the edge every one of these has, and each answers it
    // BRANCHLESSLY. BSR and BSF leave the destination UNDEFINED on a zero
    // source and set ZF -- so the sequences read ZF with CMOVZ or SETZ before
    // anything clobbers the flags, which is why the MOV sitting between the
    // scan and the conditional is load-bearing rather than sloppy.
    /// `Integer.numberOfLeadingZeros(int)` -> `int`; 32 for zero.
    NlzI,
    /// `Long.numberOfLeadingZeros(long)` -> `int`; 64 for zero.
    NlzL,
    /// `Integer.numberOfTrailingZeros(int)` -> `int`; 32 for zero.
    NtzI,
    /// `Long.numberOfTrailingZeros(long)` -> `int`; 64 for zero.
    NtzL,
    /// `Integer.reverseBytes(int)`.
    ReverseBytesI,
    /// `Long.reverseBytes(long)`.
    ReverseBytesL,
    /// `Integer.lowestOneBit(int)`, the `x AND -x` identity; 0 for zero, and
    /// the only member of this group with no undefined-register edge at all.
    LowestOneBitI,
    /// `Long.lowestOneBit(long)`.
    LowestOneBitL,
    /// `Integer.highestOneBit(int)`; 0 for zero.
    HighestOneBitI,
    /// `Long.highestOneBit(long)`; 0 for zero.
    HighestOneBitL,
    /// `Integer.rotateLeft(int, int)`. x86 masks the count to 5 bits, which is
    /// exactly the `distance AND 31` the JLS specifies -- including for a
    /// NEGATIVE distance, where `rotateLeft(i, -1) == rotateRight(i, 1)`.
    RotateLeftI,
    /// `Long.rotateLeft(long, int)`; masked to 6 bits.
    RotateLeftL,
    /// `Integer.rotateRight(int, int)`.
    RotateRightI,
    /// `Long.rotateRight(long, int)`.
    RotateRightL,

    // -- Floating-point sign mask, 2026-09-07 ------------------------
    //
    // `Math.abs(float)` / `Math.abs(double)` clear the sign bit, which is one
    // AND against a mask -- no branch, no memory, and no NaN special case:
    // clearing the sign of a NaN yields a NaN, which is what the JLS requires.
    // It also gets `abs(-0.0) == +0.0` right for free, where a comparison-based
    // sequence (`x < 0 ? -x : x`) returns -0.0 and is WRONG.
    //
    // These are the first FP members of this enum. They do NOT go through
    // `gp_load_value`/`store_rax` like every variant above; see
    // `ScalarOp::is_fp` and the guard at the top of the lowering arm.
    /// `Math.abs(float)`.
    AbsF,
    /// `Math.abs(double)`.
    AbsD,

    // -- Raw FP bit reinterpretations, round 15 wave 6 (lane irosr2, IRO-1) --
    //
    // One `MOVQ`/`MOVD` between the two register files each, and bitwise: a
    // NaN keeps its payload (a signalling one stays signalling), -0.0 keeps
    // its sign. That is exactly the RAW contract and the single-pass FP_BITS
    // intrinsics' semantics (`jit/src/lib.rs`). The CANONICALISING siblings
    // `Double.doubleToLongBits` / `Float.floatToIntBits` collapse every NaN to
    // one pattern and are deliberately NOT members: no single move implements
    // them. These are the only CROSS-BANK families -- see
    // [`ScalarOp::raw_bits_move`] and the lowering arm's guard.
    // `CRATONVM_JIT_IR_FP_RAW_BITS=0` withdraws all four
    // ([`ir_fp_raw_bits_enabled`]).
    /// `Double.doubleToRawLongBits(double)` -> `long`.
    DoubleToRawLongBits,
    /// `Double.longBitsToDouble(long)` -> `double`.
    LongBitsToDouble,
    /// `Float.floatToRawIntBits(float)` -> `int`.
    FloatToRawIntBits,
    /// `Float.intBitsToFloat(int)` -> `float`.
    IntBitsToFloat,

    // -- FP math and the 128-bit product, JIT round 19 (lane irops19, R15B-31) --
    //
    // Opt-in: `CRATONVM_JIT_IR_FP_MATH_OPS` ([`ir_fp_math_ops_enabled`]) is
    // the only door, read through [`try_ir_scalar_intrinsic`] -- except the
    // four FP min/max rows, also opened by the default-ON
    // `CRATONVM_JIT_IR_FP_MINMAX` (r23 w2 L8, [`ir_fp_minmax_rows_enabled`]). Off, every one
    // of these is a single-pass `JitIntrinsic` site and so refuses its whole
    // METHOD from this tier ("loop-shaped or memory call-site intrinsic").
    //
    // The four FP min/max members are NOT `MINSD`/`MAXSD`: those return the
    // SECOND operand when either is NaN or both are zero, while Java returns
    // the NaN argument itself and orders -0.0 below +0.0. See the lowering
    // arm, which reproduces the JDK body's answers bit for bit (the NaN
    // ARGUMENT, payload included -- the single-pass intrinsic's contract).
    // Each takes one or two local branches inside its own sequence; none
    // has a CFG edge, a trap or a memory effect.
    /// `Math.sqrt(double)` / `StrictMath.sqrt` -- `SQRTSD`, which IEEE 754
    /// specifies correctly rounded, so both classes agree exactly.
    SqrtD,
    /// `Math.min(float,float)`.
    MinF,
    /// `Math.max(float,float)`.
    MaxF,
    /// `Math.min(double,double)`.
    MinD,
    /// `Math.max(double,double)`.
    MaxD,
    /// `Math.multiplyHigh(long,long)` -- the high half of the SIGNED 128-bit
    /// product, one-operand `IMUL r64`.
    MultiplyHighL,
    /// `Math.unsignedMultiplyHigh(long,long)` -- one-operand `MUL r64`.
    UnsignedMultiplyHighL,

    // -- Rounding, fused multiply-add, population count: JIT round 19 wave 2
    //    (lane irops2, IROPS19-2) --
    //
    // Opt-in: `CRATONVM_JIT_IR_ROUND_FMA_BITCOUNT_OPS`
    // ([`ir_round_fma_bitcount_ops_enabled`]), read only through
    // [`try_ir_scalar_intrinsic`]. Each row ALSO carries the CPU-feature gate
    // the single-pass resolver (`try_resolve_intrinsic`) applies to the same
    // site, so a host without the feature keeps refusing exactly as before
    // (the site is then either a plain call -- no single-pass intrinsic -- or
    // a single-pass intrinsic site the planner refuses, never an instruction
    // the host lacks). None of them branches, traps or touches memory.
    /// `Math.floor(double)` / `StrictMath.floor` -- SSE4.1 `ROUNDSD` toward
    /// -inf. IEEE 754 `roundToIntegralTowardNegative`: -0.0, NaN and the
    /// infinities come back unchanged, exactly the JDK's specification.
    FloorD,
    /// `Math.ceil(double)` -- `ROUNDSD` toward +inf (`ceil(-0.5)` is -0.0,
    /// as the JDK specifies).
    CeilD,
    /// `Math.rint(double)` -- `ROUNDSD` to nearest, ties to even (the JDK's
    /// definition of `rint`).
    RintD,
    /// `Math.fma(double,double,double)` -- FMA3 `VFMADD213SD`: `a*b + c`
    /// rounded ONCE, the JDK's specification. Three operands.
    FmaD,
    /// `Math.fma(float,float,float)` -- `VFMADD213SS`.
    FmaF,
    /// `Integer.bitCount(int)` -- `POPCNT r32`.
    BitCountI,
    /// `Long.bitCount(long)` -> `int` -- `POPCNT r64`.
    BitCountL,

    // -- UTF-16 surrogate tests, nio20 wave 6 (lane L3) ---------------
    //
    // `Character.isHighSurrogate(char)` / `isLowSurrogate(char)` are
    // registered `Intrinsic`-kind natives in both modes (the kind map's
    // `java/lang/Character ... intrinsic` rows), so a compiled site was a
    // native CALL per test -- `Surrogate.Parser.parse` makes two per
    // surrogate pair of every UTF-8 encode, and a spliced body that keeps such
    // a call is refused or priced as a blind dispatch. The JDK body is one
    // range test (`ch >= MIN && ch < MAX + 1`), which this lowers as
    // `SUB EAX, MIN ; CMP EAX, 0x400 ; SETB` -- exact for ANY 32-bit operand,
    // as the bytecode's two signed compares are.
    // `CRATONVM_JIT_IR_CHAR_SURROGATE_SCALAR=0` withdraws both
    // ([`ir_char_surrogate_scalar_enabled`]).
    /// `Character.isHighSurrogate(char)` -> `boolean` (`0xD800..=0xDBFF`).
    IsHighSurrogate,
    /// `Character.isLowSurrogate(char)` -> `boolean` (`0xDC00..=0xDFFF`).
    IsLowSurrogate,

    // -- Code-point tests, JIT round 20 wave 11 (lane pIR, L5W1-2) -----
    //
    // `Character.charCount(int)` / `isBmpCodePoint(int)` /
    // `isValidCodePoint(int)` are registered `Intrinsic`-kind natives in both
    // modes, like the surrogate tests above, and are one compare each in the
    // JDK body. `CRATONVM_JIT_IR_CODE_POINT_SCALAR=0` withdraws all three
    // ([`ir_code_point_scalar_enabled`]).
    /// `Character.charCount(int)` -> `int`: `cp >= 0x10000 ? 2 : 1` (SIGNED
    /// compare, as the JDK's; a negative operand answers 1).
    CharCount,
    /// `Character.isBmpCodePoint(int)` -> `boolean`: `cp >>> 16 == 0`.
    IsBmpCodePoint,
    /// `Character.isValidCodePoint(int)` -> `boolean`: `cp >>> 16 < 0x11`.
    IsValidCodePoint,
    /// `Character.isISOControl(int|char)` -> `boolean`: `0 <= cp <= 0x1F || 0x7F <= cp <= 0x9F`.
    IsIsoControl,

    // -- `Math.round`, JIT round 22 wave 1, lane intrinsics -------------
    //
    // INTRINSICS21-7 (`CRATONVM_JIT_IR_MATH_ROUND_SCALAR`, opt-in). Both are
    // registered `Intrinsic` natives, so a compiled site was a native leaf
    // call per rounding. CROSS-BANK like the raw bit moves, the other way
    // round from `LongBitsToDouble`: an FP operand ([`Self::round_fp_input`]
    // loads it into XMM0 and its raw bits into RAX) and a general-purpose
    // result. The lowering is the JDK 25 body's bit arithmetic (exponent,
    // shift, `((r >> shift) + 1) >> 1`) with `(long) a` / `(int) a` -- the
    // saturating truncation and its NaN -> 0 fix-up -- outside the shift
    // window, so every JDK-6430675 corner is the body's answer.
    /// `Math.round(double)` -> `long`.
    RoundD,
    /// `Math.round(float)` -> `int`.
    RoundF,

    // -- Bit compress / expand, j27 wave 6 (lane T1BCE) -----------------
    //
    // BMI2 `PEXT` / `PDEP`, recognised only on a BMI2 host and under the
    // `CRATONVM_JIT_IR_BIT_COMPRESS_EXPAND` (default ON since 2026-10-28, j27
    // WARNING flip; =0 restores) (`ir::bit_compress`, which
    // owns the rows, the encodings and their tests). Operand 0 is the value,
    // operand 1 the mask, as in the JDK signatures.
    /// `Integer.compress(int, int)` -- `PEXT r32`.
    CompressI,
    /// `Long.compress(long, long)` -- `PEXT r64`.
    CompressL,
    /// `Integer.expand(int, int)` -- `PDEP r32`.
    ExpandI,
    /// `Long.expand(long, long)` -- `PDEP r64`.
    ExpandL,
    /// Direct ABI-call math transcendental.
    MathCall(MathCallOp),
}

/// Direct ABI-call math transcendentals (unary/binary `f64` -> `f64`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MathCallOp {
    MathSin, MathCos, MathTan, MathExp, MathLog, MathLog10, MathCbrt, MathPow,
    StrictSin, StrictCos, StrictTan, StrictExp, StrictLog, StrictLog10, StrictCbrt, StrictPow,
    Asin, Acos, Atan, Atan2, Sinh, Cosh, Tanh, Expm1, Log1p, Hypot, IeeeRemainder,
}

macro_rules! def_math_c_unary {
    ($($name:ident => $expr:expr),* $(,)?) => {
        $(extern "C" fn $name(a: f64) -> f64 { ($expr)(a) })*
    };
}
macro_rules! def_math_c_binary {
    ($($name:ident => $expr:expr),* $(,)?) => {
        $(extern "C" fn $name(a: f64, b: f64) -> f64 { ($expr)(a, b) })*
    };
}

def_math_c_unary! {
    jit_c_math_sin => f64::sin,
    jit_c_math_cos => f64::cos,
    jit_c_math_tan => f64::tan,
    jit_c_math_exp => f64::exp,
    jit_c_math_log => f64::ln,
    jit_c_math_log10 => |v: f64| if v < 0.0 { f64::from_bits(0xFFF8_0000_0000_0000) } else { v.log10() },
    jit_c_math_cbrt => f64::cbrt,
    jit_c_strict_sin => cratonvm_types::fdlibm::sin,
    jit_c_strict_cos => cratonvm_types::fdlibm::cos,
    jit_c_strict_tan => cratonvm_types::fdlibm::tan,
    jit_c_strict_exp => cratonvm_types::fdlibm::exp,
    jit_c_strict_log => cratonvm_types::fdlibm::log,
    jit_c_strict_log10 => cratonvm_types::fdlibm::log10,
    jit_c_strict_cbrt => cratonvm_types::fdlibm::cbrt,
    jit_c_fdlibm_asin => cratonvm_types::fdlibm::asin,
    jit_c_fdlibm_acos => cratonvm_types::fdlibm::acos,
    jit_c_fdlibm_atan => cratonvm_types::fdlibm::atan,
    jit_c_fdlibm_sinh => cratonvm_types::fdlibm::sinh,
    jit_c_fdlibm_cosh => cratonvm_types::fdlibm::cosh,
    jit_c_fdlibm_tanh => cratonvm_types::fdlibm::tanh,
    jit_c_fdlibm_expm1 => cratonvm_types::fdlibm::expm1,
    jit_c_fdlibm_log1p => cratonvm_types::fdlibm::log1p,
}

def_math_c_binary! {
    jit_c_math_pow => cratonvm_types::intel_libm::java_math_pow,
    jit_c_strict_pow => cratonvm_types::fdlibm::pow,
    jit_c_fdlibm_atan2 => cratonvm_types::fdlibm::atan2,
    jit_c_fdlibm_hypot => cratonvm_types::fdlibm::hypot,
    jit_c_fdlibm_ieee_remainder => cratonvm_types::fdlibm::ieee_remainder,
}

impl MathCallOp {
    pub fn arity(self) -> usize {
        match self {
            Self::MathPow | Self::StrictPow | Self::Atan2 | Self::Hypot | Self::IeeeRemainder => 2,
            _ => 1,
        }
    }

    pub fn fn_ptr(self) -> usize {
        match self {
            Self::MathSin => jit_c_math_sin as *const () as usize,
            Self::MathCos => jit_c_math_cos as *const () as usize,
            Self::MathTan => jit_c_math_tan as *const () as usize,
            Self::MathExp => jit_c_math_exp as *const () as usize,
            Self::MathLog => jit_c_math_log as *const () as usize,
            Self::MathLog10 => jit_c_math_log10 as *const () as usize,
            Self::MathCbrt => jit_c_math_cbrt as *const () as usize,
            Self::MathPow => jit_c_math_pow as *const () as usize,
            Self::StrictSin => jit_c_strict_sin as *const () as usize,
            Self::StrictCos => jit_c_strict_cos as *const () as usize,
            Self::StrictTan => jit_c_strict_tan as *const () as usize,
            Self::StrictExp => jit_c_strict_exp as *const () as usize,
            Self::StrictLog => jit_c_strict_log as *const () as usize,
            Self::StrictLog10 => jit_c_strict_log10 as *const () as usize,
            Self::StrictCbrt => jit_c_strict_cbrt as *const () as usize,
            Self::StrictPow => jit_c_strict_pow as *const () as usize,
            Self::Asin => jit_c_fdlibm_asin as *const () as usize,
            Self::Acos => jit_c_fdlibm_acos as *const () as usize,
            Self::Atan => jit_c_fdlibm_atan as *const () as usize,
            Self::Atan2 => jit_c_fdlibm_atan2 as *const () as usize,
            Self::Sinh => jit_c_fdlibm_sinh as *const () as usize,
            Self::Cosh => jit_c_fdlibm_cosh as *const () as usize,
            Self::Tanh => jit_c_fdlibm_tanh as *const () as usize,
            Self::Expm1 => jit_c_fdlibm_expm1 as *const () as usize,
            Self::Log1p => jit_c_fdlibm_log1p as *const () as usize,
            Self::Hypot => jit_c_fdlibm_hypot as *const () as usize,
            Self::IeeeRemainder => jit_c_fdlibm_ieee_remainder as *const () as usize,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::MathSin => "Math.sin(D)",
            Self::MathCos => "Math.cos(D)",
            Self::MathTan => "Math.tan(D)",
            Self::MathExp => "Math.exp(D)",
            Self::MathLog => "Math.log(D)",
            Self::MathLog10 => "Math.log10(D)",
            Self::MathCbrt => "Math.cbrt(D)",
            Self::MathPow => "Math.pow(DD)",
            Self::StrictSin => "StrictMath.sin(D)",
            Self::StrictCos => "StrictMath.cos(D)",
            Self::StrictTan => "StrictMath.tan(D)",
            Self::StrictExp => "StrictMath.exp(D)",
            Self::StrictLog => "StrictMath.log(D)",
            Self::StrictLog10 => "StrictMath.log10(D)",
            Self::StrictCbrt => "StrictMath.cbrt(D)",
            Self::StrictPow => "StrictMath.pow(DD)",
            Self::Asin => "Math.asin(D)",
            Self::Acos => "Math.acos(D)",
            Self::Atan => "Math.atan(D)",
            Self::Atan2 => "Math.atan2(DD)",
            Self::Sinh => "Math.sinh(D)",
            Self::Cosh => "Math.cosh(D)",
            Self::Tanh => "Math.tanh(D)",
            Self::Expm1 => "Math.expm1(D)",
            Self::Log1p => "Math.log1p(D)",
            Self::Hypot => "Math.hypot(DD)",
            Self::IeeeRemainder => "Math.IEEEremainder(DD)",
        }
    }

    pub fn match_triple(class: &str, method: &str, descriptor: &str) -> Option<Self> {
        match (class, method, descriptor) {
            ("java/lang/Math", "sin", "(D)D") => Some(Self::MathSin),
            ("java/lang/Math", "cos", "(D)D") => Some(Self::MathCos),
            ("java/lang/Math", "tan", "(D)D") => Some(Self::MathTan),
            ("java/lang/Math", "exp", "(D)D") => Some(Self::MathExp),
            ("java/lang/Math", "log", "(D)D") => Some(Self::MathLog),
            ("java/lang/Math", "log10", "(D)D") => Some(Self::MathLog10),
            ("java/lang/Math", "cbrt", "(D)D") => Some(Self::MathCbrt),
            ("java/lang/Math", "pow", "(DD)D") => Some(Self::MathPow),
            ("java/lang/StrictMath", "sin", "(D)D") => Some(Self::StrictSin),
            ("java/lang/StrictMath", "cos", "(D)D") => Some(Self::StrictCos),
            ("java/lang/StrictMath", "tan", "(D)D") => Some(Self::StrictTan),
            ("java/lang/StrictMath", "exp", "(D)D") => Some(Self::StrictExp),
            ("java/lang/StrictMath", "log", "(D)D") => Some(Self::StrictLog),
            ("java/lang/StrictMath", "log10", "(D)D") => Some(Self::StrictLog10),
            ("java/lang/StrictMath", "cbrt", "(D)D") => Some(Self::StrictCbrt),
            ("java/lang/StrictMath", "pow", "(DD)D") => Some(Self::StrictPow),
            ("java/lang/Math" | "java/lang/StrictMath", "asin", "(D)D") => Some(Self::Asin),
            ("java/lang/Math" | "java/lang/StrictMath", "acos", "(D)D") => Some(Self::Acos),
            ("java/lang/Math" | "java/lang/StrictMath", "atan", "(D)D") => Some(Self::Atan),
            ("java/lang/Math" | "java/lang/StrictMath", "atan2", "(DD)D") => Some(Self::Atan2),
            ("java/lang/Math" | "java/lang/StrictMath", "sinh", "(D)D") => Some(Self::Sinh),
            ("java/lang/Math" | "java/lang/StrictMath", "cosh", "(D)D") => Some(Self::Cosh),
            ("java/lang/Math" | "java/lang/StrictMath", "tanh", "(D)D") => Some(Self::Tanh),
            ("java/lang/Math" | "java/lang/StrictMath", "expm1", "(D)D") => Some(Self::Expm1),
            ("java/lang/Math" | "java/lang/StrictMath", "log1p", "(D)D") => Some(Self::Log1p),
            ("java/lang/Math" | "java/lang/StrictMath", "hypot", "(DD)D") => Some(Self::Hypot),
            ("java/lang/Math" | "java/lang/StrictMath", "IEEEremainder", "(DD)D") => Some(Self::IeeeRemainder),
            _ => None,
        }
    }
}


impl ScalarOp {
    /// How many data inputs the node takes. `ir_verify` reads this rather than
    /// carrying a second copy of the table.
    pub fn arity(self) -> usize {
        match self {
            ScalarOp::AbsI
            | ScalarOp::AbsL
            | ScalarOp::NlzI
            | ScalarOp::NlzL
            | ScalarOp::NtzI
            | ScalarOp::NtzL
            | ScalarOp::ReverseBytesI
            | ScalarOp::ReverseBytesL
            | ScalarOp::LowestOneBitI
            | ScalarOp::LowestOneBitL
            | ScalarOp::HighestOneBitI
            | ScalarOp::HighestOneBitL
            | ScalarOp::AbsF
            | ScalarOp::AbsD
            | ScalarOp::DoubleToRawLongBits
            | ScalarOp::LongBitsToDouble
            | ScalarOp::FloatToRawIntBits
            | ScalarOp::IntBitsToFloat
            | ScalarOp::IsHighSurrogate
            | ScalarOp::IsLowSurrogate
            | ScalarOp::CharCount
            | ScalarOp::IsBmpCodePoint
            | ScalarOp::IsValidCodePoint
            | ScalarOp::IsIsoControl
            | ScalarOp::RoundD
            | ScalarOp::RoundF
            | ScalarOp::SqrtD
            | ScalarOp::FloorD
            | ScalarOp::CeilD
            | ScalarOp::RintD
            | ScalarOp::BitCountI
            | ScalarOp::BitCountL => 1,
            // JIT round 19 wave 2 (lane irops2): the only three-operand family.
            ScalarOp::FmaD | ScalarOp::FmaF => 3,
            ScalarOp::MathCall(m) => m.arity(),
            _ => 2,
        }
    }

    /// For a raw bit-reinterpretation family, `Some((to_fp, double))`: the
    /// direction of its one move (`to_fp` = general-purpose operand, XMM
    /// result) and its width (`double` = 64 bits). `None` for every other
    /// family, which keeps operand and result in ONE register file.
    ///
    /// Not folded into [`Self::is_fp`]: that predicate names the XMM-in,
    /// XMM-out families, and the lowering arm reads it to decide where
    /// `inputs[0]` is loaded from. A cross-bank family answering it would
    /// load a `long` operand through the FP path, or an FP operand into RAX.
    pub fn raw_bits_move(self) -> Option<(bool, bool)> {
        match self {
            ScalarOp::DoubleToRawLongBits => Some((false, true)),
            ScalarOp::LongBitsToDouble => Some((true, true)),
            ScalarOp::FloatToRawIntBits => Some((false, false)),
            ScalarOp::IntBitsToFloat => Some((true, false)),
            _ => None,
        }
    }

    /// For `Math.round` ([`Self::RoundD`] / [`Self::RoundF`]), `Some(double)`:
    /// the FP operand's width. The lowering loads it into XMM0 (for the
    /// saturating fallback) AND its raw bits into RAX (for the body's bit
    /// arithmetic); the result is general-purpose. `None` for every other
    /// family. Not a [`Self::raw_bits_move`] answer: that one promises a
    /// single bitwise move, which the constant folder and the identity
    /// rewrites rely on.
    pub fn round_fp_input(self) -> Option<bool> {
        match self {
            ScalarOp::RoundD => Some(true),
            ScalarOp::RoundF => Some(false),
            _ => None,
        }
    }

    /// Does this family operate in XMM registers rather than general-purpose
    /// ones?
    ///
    /// The lowering arm loads `inputs[0]` into RAX before it dispatches, which
    /// is wrong for an FP family; this is the predicate that guards it. Kept as
    /// its own question rather than derived from `result_type`, because
    /// `Long.numberOfLeadingZeros` already proves operand width and result
    /// width are independent here.
    pub fn is_fp(self) -> bool {
        matches!(
            self,
            ScalarOp::AbsF
                | ScalarOp::AbsD
                | ScalarOp::SqrtD
                | ScalarOp::MinF
                | ScalarOp::MaxF
                | ScalarOp::MinD
                | ScalarOp::MaxD
                | ScalarOp::FloorD
                | ScalarOp::CeilD
                | ScalarOp::RintD
                | ScalarOp::FmaD
                | ScalarOp::FmaF
                | ScalarOp::MathCall(_)
        )
    }

    /// The type of data input `idx`.
    ///
    /// A per-input answer rather than a per-op one, because the ROTATES are
    /// mixed: `Long.rotateLeft` takes a `long` VALUE and an `int` DISTANCE.
    /// Reading [`Self::operands_are_long`] for both would type the distance as
    /// a `long`, and a mistyped stack entry is joined against a real one at the
    /// next merge.
    pub fn input_ty(self, idx: usize) -> IrType {
        match self {
            ScalarOp::RotateLeftI
            | ScalarOp::RotateLeftL
            | ScalarOp::RotateRightI
            | ScalarOp::RotateRightL
                if idx == 1 =>
            {
                IrType::Int
            }
            ScalarOp::AbsF | ScalarOp::MinF | ScalarOp::MaxF | ScalarOp::FmaF => IrType::Float,
            ScalarOp::AbsD
            | ScalarOp::SqrtD
            | ScalarOp::MinD
            | ScalarOp::MaxD
            | ScalarOp::FloorD
            | ScalarOp::CeilD
            | ScalarOp::RintD
            | ScalarOp::FmaD
            | ScalarOp::MathCall(_) => IrType::Double,
            // The cross-bank families: the operand is the OTHER file's type.
            ScalarOp::DoubleToRawLongBits => IrType::Double,
            ScalarOp::LongBitsToDouble => IrType::Long,
            ScalarOp::FloatToRawIntBits => IrType::Float,
            ScalarOp::IntBitsToFloat => IrType::Int,
            ScalarOp::RoundD => IrType::Double,
            ScalarOp::RoundF => IrType::Float,
            _ if self.operands_are_long() => IrType::Long,
            _ => IrType::Int,
        }
    }

    /// The node's result type.
    pub fn result_type(self) -> IrType {
        match self {
            ScalarOp::MinL
            | ScalarOp::MaxL
            | ScalarOp::AbsL
            | ScalarOp::ReverseBytesL
            | ScalarOp::LowestOneBitL
            | ScalarOp::HighestOneBitL
            | ScalarOp::RotateLeftL
            | ScalarOp::RotateRightL
            | ScalarOp::DoubleToRawLongBits
            | ScalarOp::MultiplyHighL
            | ScalarOp::UnsignedMultiplyHighL
            | ScalarOp::RoundD
            | ScalarOp::CompressL
            | ScalarOp::ExpandL => IrType::Long,
            ScalarOp::AbsF
            | ScalarOp::IntBitsToFloat
            | ScalarOp::MinF
            | ScalarOp::MaxF
            | ScalarOp::FmaF => IrType::Float,
            ScalarOp::AbsD
            | ScalarOp::LongBitsToDouble
            | ScalarOp::SqrtD
            | ScalarOp::MinD
            | ScalarOp::MaxD
            | ScalarOp::FloorD
            | ScalarOp::CeilD
            | ScalarOp::RintD
            | ScalarOp::FmaD
            | ScalarOp::MathCall(_) => IrType::Double,
            // `Long.bitCount` takes a `long` and answers an `int`, like the
            // bit scans below.
            // `Float.floatToRawIntBits` falls to the `int` default below.
            // `Integer.compare`, `Long.compare` and BOTH widths of
            // `numberOfLeading/TrailingZeros` return `int` --
            // `Long.numberOfLeadingZeros` takes a `long` and answers an `int`,
            // which is why this table and `operands_are_long` are two tables
            // and not one.
            _ => IrType::Int,
        }
    }

    /// True when the operands are 64-bit. Distinct from [`Self::result_type`]
    /// precisely because `Long.compare` takes `long`s and returns an `int` --
    /// reading the result type to size the `CMP` is the bug this pair exists to
    /// make impossible.
    pub fn operands_are_long(self) -> bool {
        matches!(
            self,
            ScalarOp::MinL
                | ScalarOp::MaxL
                | ScalarOp::AbsL
                | ScalarOp::CompareL
                | ScalarOp::NlzL
                | ScalarOp::NtzL
                | ScalarOp::ReverseBytesL
                | ScalarOp::LowestOneBitL
                | ScalarOp::HighestOneBitL
                | ScalarOp::RotateLeftL
                | ScalarOp::RotateRightL
                | ScalarOp::LongBitsToDouble
                | ScalarOp::MultiplyHighL
                | ScalarOp::UnsignedMultiplyHighL
                | ScalarOp::BitCountL
                | ScalarOp::CompressL
                | ScalarOp::ExpandL
        )
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ScalarOp::MinI => "Math.min(II)",
            ScalarOp::MaxI => "Math.max(II)",
            ScalarOp::MinL => "Math.min(JJ)",
            ScalarOp::MaxL => "Math.max(JJ)",
            ScalarOp::AbsI => "Math.abs(I)",
            ScalarOp::AbsL => "Math.abs(J)",
            ScalarOp::CompareI => "Integer.compare(II)",
            ScalarOp::CompareL => "Long.compare(JJ)",
            ScalarOp::NlzI => "Integer.numberOfLeadingZeros(I)",
            ScalarOp::NlzL => "Long.numberOfLeadingZeros(J)",
            ScalarOp::NtzI => "Integer.numberOfTrailingZeros(I)",
            ScalarOp::NtzL => "Long.numberOfTrailingZeros(J)",
            ScalarOp::ReverseBytesI => "Integer.reverseBytes(I)",
            ScalarOp::ReverseBytesL => "Long.reverseBytes(J)",
            ScalarOp::LowestOneBitI => "Integer.lowestOneBit(I)",
            ScalarOp::LowestOneBitL => "Long.lowestOneBit(J)",
            ScalarOp::HighestOneBitI => "Integer.highestOneBit(I)",
            ScalarOp::HighestOneBitL => "Long.highestOneBit(J)",
            ScalarOp::RotateLeftI => "Integer.rotateLeft(II)",
            ScalarOp::RotateLeftL => "Long.rotateLeft(JI)",
            ScalarOp::RotateRightI => "Integer.rotateRight(II)",
            ScalarOp::RotateRightL => "Long.rotateRight(JI)",
            ScalarOp::AbsF => "Math.abs(F)",
            ScalarOp::AbsD => "Math.abs(D)",
            ScalarOp::DoubleToRawLongBits => "Double.doubleToRawLongBits(D)",
            ScalarOp::LongBitsToDouble => "Double.longBitsToDouble(J)",
            ScalarOp::FloatToRawIntBits => "Float.floatToRawIntBits(F)",
            ScalarOp::IntBitsToFloat => "Float.intBitsToFloat(I)",
            ScalarOp::SqrtD => "Math.sqrt(D)",
            ScalarOp::MinF => "Math.min(FF)",
            ScalarOp::MaxF => "Math.max(FF)",
            ScalarOp::MinD => "Math.min(DD)",
            ScalarOp::MaxD => "Math.max(DD)",
            ScalarOp::MultiplyHighL => "Math.multiplyHigh(JJ)",
            ScalarOp::UnsignedMultiplyHighL => "Math.unsignedMultiplyHigh(JJ)",
            ScalarOp::FloorD => "Math.floor(D)",
            ScalarOp::CeilD => "Math.ceil(D)",
            ScalarOp::RintD => "Math.rint(D)",
            ScalarOp::FmaD => "Math.fma(DDD)",
            ScalarOp::FmaF => "Math.fma(FFF)",
            ScalarOp::BitCountI => "Integer.bitCount(I)",
            ScalarOp::BitCountL => "Long.bitCount(J)",
            ScalarOp::IsHighSurrogate => "Character.isHighSurrogate(C)",
            ScalarOp::IsLowSurrogate => "Character.isLowSurrogate(C)",
            ScalarOp::CharCount => "Character.charCount(I)",
            ScalarOp::IsBmpCodePoint => "Character.isBmpCodePoint(I)",
            ScalarOp::IsValidCodePoint => "Character.isValidCodePoint(I)",
            ScalarOp::IsIsoControl => "Character.isISOControl(I)",
            ScalarOp::RoundD => "Math.round(D)",
            ScalarOp::RoundF => "Math.round(F)",
            ScalarOp::CompressI => "Integer.compress(II)",
            ScalarOp::CompressL => "Long.compress(JJ)",
            ScalarOp::ExpandI => "Integer.expand(II)",
            ScalarOp::ExpandL => "Long.expand(JJ)",
            ScalarOp::MathCall(m) => m.as_str(),
        }
    }
}

/// Does this call site name a family the optimizing tier lowers as arithmetic?
///
/// The single source of truth, consulted from BOTH sides: `try_compile_inner`
/// asks it to decide whether a `JitIntrinsic` site still refuses the method,
/// and `IrBuilder` asks it again to decide what to emit. Two enumerations of
/// one question is the failure this file has recorded more than once, so there
/// is one.
///
/// `StrictMath` is accepted alongside `Math` for `min`/`max`/`abs` only: those
/// three are specified identically in both classes (JLS 15.20, `StrictMath`'s
/// own javadoc delegates), unlike the transcendentals, which are not.
/// The unboxing accessors, as a GUARDED FIELD LOAD rather than a refusal.
///
/// `ScalarOp` covers call-site intrinsics that are pure register arithmetic.
/// These are not: they read a field, and the byte offset of that field depends
/// on how the INSTANCE was allocated -- a compact instance stores the payload
/// at a registered body offset, a legacy one inside its 16-byte `Value` cell,
/// and both shapes exist at once because different allocators build different
/// cells. That per-object branch is why this family sat on the refusal list
/// while the arithmetic ones were lowered.
///
/// The node carries only the guard class id. The offsets are re-derived at
/// LOWERING time from that id, through the very same `AtomicLongFieldLayout` /
/// `AtomicIntFieldLayout` the single-pass emitter uses, so the two backends
/// cannot drift into disagreeing about where the field is -- which, for a raw
/// load, is the difference between a value and a wild read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum UnboxOp {
    /// `java/lang/Long.longValue()J` -- 8-byte payload.
    LongValue,
    /// `java/lang/Integer.intValue()I` -- 4-byte payload.
    IntValue,

    // -- The `Atomic*` accessors, 2026-09-08 ----------------------------
    //
    // Same node, same three guards, same per-object layout branch: these are
    // the SAME shape as the two above -- a guarded field access at slot 0 of a
    // receiver whose class id the planner resolved -- differing only in the
    // instruction at the bottom and, for one of them, an argument.
    //
    // They joined `UnboxOp` rather than arriving as a second op with a second
    // recognizer and a second planner clause, which is what a first cut of this
    // work did (`claude/unboxing-atomic-ir-nodes-d7aff4`). Two recognizers both
    // claiming `Long.longValue` is one recognizer plus dead code that still
    // compiles and still has passing tests -- the failure this file has a
    // standing rule about.
    //
    // The name is now a little narrow for its contents. It is kept because
    // every reference to it is, and because "the guarded slot-0 accessor
    // families" has no shorter spelling.
    /// `AtomicLong.get()J` -- a VOLATILE 8-byte load. An aligned 8-byte `MOV`
    /// is atomic on x86-64 and loads are not reordered with older loads under
    /// TSO, so the acquire is owed nothing: the same plain `MOV`
    /// [`Self::LongValue`] emits is the correct encoding.
    AtomicLongGet,
    /// `AtomicInteger.get()I` -- a volatile aligned 32-bit load.
    AtomicIntGet,
    /// `AtomicInteger.getAndIncrement()I` -- LOCK XADD +1, returning the old value.
    AtomicIntGetAndIncrement,
    /// `AtomicInteger.incrementAndGet()I` -- `LOCK XADD` of `+1`, returning the
    /// POST-add value.
    AtomicIntIncrementAndGet,
    /// `AtomicInteger.decrementAndGet()I` -- `LOCK XADD` of `-1`, POST-add.
    AtomicIntDecrementAndGet,
    /// `AtomicLong.getAndAdd(J)J` -- `LOCK XADD` of a runtime delta, returning
    /// the PRE-add value, which is what `XADD` leaves in its source register
    /// with no fixup at all.
    AtomicLongGetAndAdd,

    // -- Constant-folded `VarHandle` instance-field reads ---------------
    //
    // A `static final VarHandle` over one INSTANCE field of one class is a
    // compile-time constant: the handle's coordinate class, field slot and
    // value kind never change. `VarHandle.get/getVolatile/getAcquire/getOpaque`
    // on it is then exactly one aligned load of that slot from a receiver of
    // that class -- on x86-64 TSO a plain `MOV` is a correct volatile/acquire
    // load -- so it lowers through the SAME guarded shape as the families
    // above (null, exact receiver class, layout epoch, compact/legacy branch),
    // with the slot carried in the variant instead of being slot 0.
    //
    // Why this exists: every `ByteBuf` operation reads its reference count
    // through `RefCnt$VarHandleRefCnt`'s `VarHandle.get`, and the generic route
    // for that read (a thin native helper that re-validates the handle, the
    // receiver and the heap on every call) cost 80-150 ns against HotSpot's
    // ~1 ns. See `docs/feature-designs/jit-ir-optimizations.md`.
    //
    // The `u16` is the field's slot index in the receiver's field array (the
    // same index `AtomicIntFieldLayout::new` takes), and the node's `class_id`
    // is the handle's coordinate class: the planner only folds a handle whose
    // coordinate class is `final`, so the exact-class guard cannot spuriously
    // fail on a subclass.
    /// `VarHandle.get*` over an `int` instance field at the given slot.
    FieldInt(u16),
    /// `VarHandle.get*` over a `long` instance field at the given slot.
    FieldLong(u16),

    // -- JIT r23 wave 4 stage 1 (lane L8): the two reads the FFM element
    // inline expansion needs (`CRATONVM_JIT_IR_FFM_ELEMENT_INLINE`, opt-in) --
    //
    // A `MemorySegment.get(layout, offset)` expansion reads the segment's
    // `readOnly` flag (a `boolean`) and its heap base (`Object`). They are the
    // same guarded shape as the field reads above -- null, exact class, layout
    // epoch, per-object compact/legacy branch -- with a different access at the
    // bottom. No planner produces them yet; [`unbox_offsets`] refuses both
    // while the switch is off, so neither the planner nor the lowering can
    // reach them by accident.
    /// A `boolean` instance field at the given slot: a 1-byte compact storage
    /// read zero-extended (`MOVZX`), or the legacy cell's 32-bit payload. An
    /// `Int` result (0 or 1).
    FieldBool(u16),
    /// A reference instance field at the given slot: an 8-byte compact slot
    /// (a narrow oop is refused), or the legacy cell's pointer word once its
    /// tag says `Object` (any other tag deopts). A `Ref` result.
    ///
    /// # The collector's read barrier
    ///
    /// Reference slots hold plain pointers under every collector this VM runs
    /// (`x64::licm::zgc_codegen_honours_read_barrier`), so G1 and Gen owe a
    /// plain `MOV`. ZGC's barrier is honoured the way every inline reference
    /// load honours it: emission is refused while it is armed
    /// ([`crate::x64::zgc_read_barrier_blocks_inline_fields`]), and the
    /// emitted read first checks that `JIT_READ_BOUNDS` is live (arming clears
    /// it), deopting otherwise -- so code compiled before an arming stops
    /// reading raw slots the moment it happens.
    FieldRef(u16),

    // -- `ScopedMemoryAccess` raw reads and writes ----------------------
    //
    // Every `ByteBuffer.get*/put*` of a multi-byte value, heap or direct, ends
    // in `ScopedMemoryAccess.{get,put}{Short,Char,Int,Long}Unaligned` (and the
    // byte accessors in `{get,put}Byte`), whose body is a session liveness
    // check around `Unsafe.get/put*Unaligned(base, offset[, value[, bigEndian]])`.
    // For the shapes a `ByteBuffer` produces -- no memory session, and either a
    // primitive array at an offset inside its payload or an untagged real
    // address with `base == null` -- that is one plain memory access, plus a
    // byte swap for a big-endian request. The generic route for it is a thin
    // native helper (65-150 ns); HotSpot's is one `mov`.
    //
    // Same node, same deopt contract as the families above: every shape the
    // inline access does not cover (a non-null session, a reference array, an
    // offset outside the payload, a tagged `Unsafe` arena handle) deopts, and
    // the interpreter re-executes the call through the real method. Inputs are
    // `[ctrl, mem, sma, session, base, offset, (value,) (bigEndian)]`: the
    // receiver slot holds the `ScopedMemoryAccess` instance (null-checked), and
    // the arity is [`UnboxOp::arity`].
    /// A raw read of `width` bytes (1, 2, 4 or 8). `signed` selects
    /// sign-extension of a 1- or 2-byte read (`short`/`byte`) over zero-extension
    /// (`char`); `ordered` says the call carries a trailing `bigEndian` operand.
    ScopedGet {
        width: u8,
        signed: bool,
        ordered: bool,
    },
    /// A raw write of the low `width` bytes of `value`; no result.
    ScopedPut { width: u8, ordered: bool },
    /// JIT round 23 close (`CRATONVM_JIT_IR_FFM_SESSION_PRECHECK`): the
    /// scoped access's session check, run ONCE in a loop's pre-header on the
    /// hoisted `scope` read (the receiver), with no deopt: the result is
    /// `null` when the session passes for the whole loop -- a never-closing
    /// class, or a confined session this thread owns and that reads open --
    /// and the session itself otherwise. The loop's scoped accesses take it as
    /// their session, so a proven one pays `TEST; JZ` and anything else the
    /// full check. Built only by `ir_optimize::ffm_field_hoist` for a loop
    /// that runs no Java (`Graph::ffm_session_prechecks`); `class_id` is 0.
    SessionPrecheck,
    /// JIT round 24 B2 (`CRATONVM_JIT_IR_FFM_ARENA_CACHE`): a native
    /// segment loop's arena slot, resolved ONCE in its pre-header. Inputs
    /// `[ctrl, mem, min, span, session]`: the address of the JIT-readable
    /// arena table's slot for `min`'s block when `session` (the loop's
    /// [`UnboxOp::SessionPrecheck`]) reads null, `min` is a live slotted
    /// handle and `[min, min + span)` lies inside its block -- else 0. Never
    /// deopts. Built only by `ir_optimize::ffm_arena_cache`; `class_id` is 0.
    ArenaCacheEntry,
    /// JIT round 24 B2: the real address of `min` through an
    /// [`UnboxOp::ArenaCacheEntry`] -- `slot.ptr + (min & (2^30 - 1))`, 0 for a
    /// 0 entry. Inputs `[ctrl, mem, entry, min]`; never deopts.
    ArenaCacheBase,
}

/// JIT round 24 B2: how many inputs `ir_optimize::ffm_arena_cache` appends to
/// a cached scoped access (`Graph::ffm_arena_cached`).
pub const SCOPED_ARENA_CACHE_INPUTS: usize = 4;

impl UnboxOp {
    pub fn result_type(self) -> IrType {
        if matches!(self, UnboxOp::FieldRef(_) | UnboxOp::SessionPrecheck) {
            IrType::Ref
        } else if self.is_wide() {
            IrType::Long
        } else {
            IrType::Int
        }
    }

    /// True when the field access is 64-bit (`REX.W`) rather than 32-bit.
    pub fn is_wide(self) -> bool {
        matches!(
            self,
            UnboxOp::LongValue
                | UnboxOp::AtomicLongGet
                | UnboxOp::AtomicLongGetAndAdd
                | UnboxOp::FieldLong(_)
                | UnboxOp::FieldRef(_)
                | UnboxOp::ScopedGet { width: 8, .. }
                | UnboxOp::ArenaCacheEntry
                | UnboxOp::ArenaCacheBase
        )
    }

    /// `true` for the `ScopedMemoryAccess` raw read/write families, which share
    /// the node but not the lowering: they address a primitive array or a real
    /// address, not an instance field.
    pub fn is_scoped(self) -> bool {
        matches!(self, UnboxOp::ScopedGet { .. } | UnboxOp::ScopedPut { .. })
    }

    /// True for the families that only READ the field -- a `MOV`, with no
    /// `LOCK` prefix and no delta register.
    pub fn is_load(self) -> bool {
        matches!(
            self,
            UnboxOp::LongValue
                | UnboxOp::IntValue
                | UnboxOp::AtomicLongGet
                | UnboxOp::AtomicIntGet
                | UnboxOp::FieldInt(_)
                | UnboxOp::FieldLong(_)
                | UnboxOp::FieldBool(_)
                | UnboxOp::FieldRef(_)
        )
    }

    /// `true` for the two r23 wave 4 reads ([`Self::FieldBool`],
    /// [`Self::FieldRef`]), whose offsets come from their own resolvers
    /// ([`unbox_offsets`]) and whose access is not a plain 4/8-byte `MOV`.
    pub fn is_ffm_element_read(self) -> bool {
        matches!(self, UnboxOp::FieldBool(_) | UnboxOp::FieldRef(_))
    }

    /// `true` for the constant-folded `VarHandle` field reads
    /// ([`Self::FieldInt`], [`Self::FieldLong`]): the families whose receiver
    /// is the node's guard class by construction and whose slot is carried in
    /// the variant.
    pub fn is_varhandle_field_read(self) -> bool {
        matches!(self, UnboxOp::FieldInt(_) | UnboxOp::FieldLong(_))
    }

    /// The field slot a [`Self::is_varhandle_field_read`] family reads, or
    /// slot 0 for every other family (which all read `value`/slot 0).
    pub fn field_slot(self) -> usize {
        match self {
            UnboxOp::FieldInt(slot)
            | UnboxOp::FieldLong(slot)
            | UnboxOp::FieldBool(slot)
            | UnboxOp::FieldRef(slot) => usize::from(slot),
            _ => 0,
        }
    }

    /// How many DATA inputs the node takes past `[ctrl, mem, receiver]`.
    /// `ir_verify` reads this rather than carrying a second copy of the table.
    pub fn arity(self) -> usize {
        match self {
            UnboxOp::AtomicLongGetAndAdd => 1,
            // session, base, offset, (bigEndian)
            UnboxOp::ScopedGet { ordered, .. } => 3 + usize::from(ordered),
            // session, base, offset, value, (bigEndian)
            UnboxOp::ScopedPut { ordered, .. } => 4 + usize::from(ordered),
            // span, session (JIT round 24 B2)
            UnboxOp::ArenaCacheEntry => 2,
            // min
            UnboxOp::ArenaCacheBase => 1,
            _ => 0,
        }
    }

    /// The `LOCK XADD` delta when it is a compile-time immediate, or `None`
    /// when the family has no delta (the loads) or takes it as an argument.
    ///
    /// `+1`/`-1` are the only immediates here.
    pub fn delta_imm(self) -> Option<i32> {
        match self {
            UnboxOp::AtomicIntIncrementAndGet | UnboxOp::AtomicIntGetAndIncrement => Some(1),
            UnboxOp::AtomicIntDecrementAndGet => Some(-1),
            _ => None,
        }
    }

    /// True when the result is the POST-add value, so the delta must be added
    /// back after the `XADD` (which leaves the PRE-add value in its source).
    pub fn returns_post_add(self) -> bool {
        matches!(
            self,
            UnboxOp::AtomicIntIncrementAndGet | UnboxOp::AtomicIntDecrementAndGet
        )
    }

    pub fn as_str(self) -> &'static str {
        match self {
            UnboxOp::LongValue => "java/lang/Long.longValue",
            UnboxOp::IntValue => "java/lang/Integer.intValue",
            UnboxOp::AtomicLongGet => "java/util/concurrent/atomic/AtomicLong.get",
            UnboxOp::AtomicIntGet => "java/util/concurrent/atomic/AtomicInteger.get",
            UnboxOp::AtomicIntGetAndIncrement => {
                "java/util/concurrent/atomic/AtomicInteger.getAndIncrement"
            }
            UnboxOp::AtomicIntIncrementAndGet => {
                "java/util/concurrent/atomic/AtomicInteger.incrementAndGet"
            }
            UnboxOp::AtomicIntDecrementAndGet => {
                "java/util/concurrent/atomic/AtomicInteger.decrementAndGet"
            }
            UnboxOp::AtomicLongGetAndAdd => "java/util/concurrent/atomic/AtomicLong.getAndAdd",
            UnboxOp::FieldInt(_) => "java/lang/invoke/VarHandle.get(int field)",
            UnboxOp::FieldLong(_) => "java/lang/invoke/VarHandle.get(long field)",
            UnboxOp::FieldBool(_) => "field read (boolean)",
            UnboxOp::FieldRef(_) => "field read (reference)",
            UnboxOp::ScopedGet { .. } => "jdk/internal/misc/ScopedMemoryAccess.get*",
            UnboxOp::ScopedPut { .. } => "jdk/internal/misc/ScopedMemoryAccess.put*",
            UnboxOp::SessionPrecheck => "MemorySessionImpl.checkValidStateRaw (loop entry)",
            UnboxOp::ArenaCacheEntry => "Unsafe arena slot (loop entry)",
            UnboxOp::ArenaCacheBase => "Unsafe arena address (loop entry)",
        }
    }
}

/// `CRATONVM_JIT_IR_SCOPED_FOLD`, default ON: lower the `ScopedMemoryAccess`
/// raw read/write accessors in the optimizing tier ([`UnboxOp::ScopedGet`] /
/// [`UnboxOp::ScopedPut`]) instead of calling the thin native helper. `=0`
/// restores the helper route. It only selects between two correct lowerings.
pub fn ir_scoped_fold_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_SCOPED_FOLD_ALIGNED` (zt t31, 2026-10-24, default ON, `=0`
/// kills): the scoped fold also takes the ALIGNED native-order
/// `short`/`char`/`int`/`long` accessors (`getChar(session, base, offset)`,
/// `putChar(.., value)`, ..), which carry no `bigEndian` flag. A native-order
/// direct `CharBuffer` (`DirectCharBufferU.get(I)C`) and its `short`/`int`/
/// `long` twins call exactly these; without the row the public accessor is
/// refused as a splice (its `ScopedAccessError` handler) and every element
/// was a real call (page `zt31-direct-charbuffer-element-access-is-700x-hotspot`).
/// The access is the unaligned row's with no swap: [`AliasClass::Raw`] and
/// `lower_scoped_raw` do not depend on the width or the order, and x86-64
/// loads and stores any width at any address. Read per site; no `static`.
pub fn ir_scoped_fold_aligned_enabled() -> bool {
    true
}

/// Recognise a `ScopedMemoryAccess` accessor call the optimizing tier lowers as
/// a raw memory access (see [`UnboxOp::ScopedGet`]): the byte accessors, the
/// `short`/`char`/`int`/`long` unaligned ones and (zt t31,
/// [`ir_scoped_fold_aligned_enabled`]) their aligned native-order twins, public
/// wrappers and `*Internal` twins alike.
pub fn try_ir_scoped_unbox(class: &str, method: &str, descriptor: &str) -> Option<UnboxOp> {
    if class != "jdk/internal/misc/ScopedMemoryAccess" || !ir_scoped_fold_enabled() {
        return None;
    }
    const HEAD: &str = "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;J";
    let tail = descriptor.strip_prefix(HEAD)?;
    let (name, is_put) = match method.strip_prefix("get") {
        Some(rest) => (rest, false),
        None => (method.strip_prefix("put")?, true),
    };
    let name = name.strip_suffix("Internal").unwrap_or(name);
    // (accessor, width, signed, tail of the descriptor)
    let (width, signed, ordered) = match (name, is_put, tail) {
        ("Byte", false, ")B") => (1, true, false),
        ("Byte", true, "B)V") => (1, true, false),
        ("ShortUnaligned", false, "Z)S") => (2, true, true),
        ("CharUnaligned", false, "Z)C") => (2, false, true),
        ("IntUnaligned", false, "Z)I") => (4, true, true),
        ("LongUnaligned", false, "Z)J") => (8, true, true),
        ("ShortUnaligned", true, "SZ)V") => (2, true, true),
        ("CharUnaligned", true, "CZ)V") => (2, false, true),
        ("IntUnaligned", true, "IZ)V") => (4, true, true),
        ("LongUnaligned", true, "JZ)V") => (8, true, true),
        // zt t31: the aligned native-order accessors (no `bigEndian`).
        ("Short", false, ")S") | ("Short", true, "S)V") | ("Char", true, "C)V")
            if ir_scoped_fold_aligned_enabled() =>
        {
            (2, name == "Short", false)
        }
        ("Char", false, ")C") if ir_scoped_fold_aligned_enabled() => (2, false, false),
        ("Int", false, ")I") | ("Int", true, "I)V") if ir_scoped_fold_aligned_enabled() => {
            (4, true, false)
        }
        ("Long", false, ")J") | ("Long", true, "J)V") if ir_scoped_fold_aligned_enabled() => {
            (8, true, false)
        }
        _ => return None,
    };
    Some(if is_put {
        UnboxOp::ScopedPut { width, ordered }
    } else {
        UnboxOp::ScopedGet {
            width,
            signed,
            ordered,
        }
    })
}


/// Recognise an unboxing call site the optimizing tier can lower.
///
/// `class_id` is the receiver guard, resolved from the constant pool by the
/// caller. Id 0 means "unresolved", and the layout resolvers refuse it -- a
/// site with no class id must decline, because the emitter has nothing to
/// derive an offset from.
pub fn try_ir_unbox_intrinsic(
    class: &str,
    method: &str,
    descriptor: &str,
    class_id: u32,
) -> Option<UnboxOp> {
    if !ir_scalar_intrinsics_enabled() || class_id == 0 {
        return None;
    }
    let op = match (class, method, descriptor) {
        ("java/lang/Long", "longValue", "()J") => UnboxOp::LongValue,
        ("java/lang/Integer", "intValue", "()I") => UnboxOp::IntValue,
        ("java/util/concurrent/atomic/AtomicLong", "get", "()J") => UnboxOp::AtomicLongGet,
        ("java/util/concurrent/atomic/AtomicInteger", "get", "()I") => UnboxOp::AtomicIntGet,
        ("java/util/concurrent/atomic/AtomicInteger", "getAndIncrement", "()I") => {
            UnboxOp::AtomicIntGetAndIncrement
        }
        ("java/util/concurrent/atomic/AtomicInteger", "incrementAndGet", "()I") => {
            UnboxOp::AtomicIntIncrementAndGet
        }
        ("java/util/concurrent/atomic/AtomicInteger", "decrementAndGet", "()I") => {
            UnboxOp::AtomicIntDecrementAndGet
        }
        ("java/util/concurrent/atomic/AtomicLong", "getAndAdd", "(J)J") => {
            UnboxOp::AtomicLongGetAndAdd
        }
        _ => return None,
    };
    // Ask the layout NOW as well as at lowering: a class whose `value` field is
    // not an 8/4-byte scalar at a resolvable offset must be refused at the
    // planner, not discovered by the emitter after the method was admitted.
    unbox_offsets(op, class_id)?;
    Some(op)
}

/// `(compact, legacy)` byte offsets of the payload, from the SAME resolver the
/// single-pass backend uses. The single source of truth for both backends.
pub fn unbox_offsets(op: UnboxOp, class_id: u32) -> Option<(i32, i32)> {
    // The WIDTH decides which resolver applies, and it is read off the family
    // rather than off the class name: `AtomicLong` and `AtomicInteger` differ by
    // exactly this, and pointing the 64-bit form at a 4-byte payload is a wrong
    // VALUE that no test of the 32-bit form would catch. Both constructors
    // carry the width check too -- they refuse unless slot 0's compact storage
    // is exactly 8 / 4 bytes -- so a layout the access could not address never
    // reaches codegen.
    // Every family but the constant-folded `VarHandle` field reads addresses
    // slot 0 (`Long.value`, `Integer.value`, `Atomic*.value`); those carry
    // their own slot. The layout resolvers are slot-generic and refuse any
    // slot whose compact storage is not exactly 4 / 8 bytes wide.
    let slot = op.field_slot();
    if op.is_ffm_element_read() {
        if !ir_ffm_element_inline_enabled() {
            return None;
        }
        return ffm_element_read_offsets(op, slot, class_id);
    }
    if op.is_wide() {
        crate::AtomicLongFieldLayout::new(slot, class_id)
            .map(|l| (l.value_compact_offset, l.value_legacy_offset))
    } else {
        crate::AtomicIntFieldLayout::new(slot, class_id)
            .map(|l| (l.value_compact_offset, l.value_legacy_offset))
    }
}

/// `CRATONVM_JIT_IR_FFM_ELEMENT_INLINE` (JIT r23 wave 4 stage 1, lane L8;
/// perf, default ON since r23 wave 7 -- R23-W7-FLIP, `=0` kills): admit
/// [`UnboxOp::FieldBool`] / [`UnboxOp::FieldRef`]. Off,
/// [`unbox_offsets`] refuses both, so a planner asking first declines and a
/// node built anyway bails the compile at lowering. Read per call (thread
/// overrides apply).
pub fn ir_ffm_element_inline_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_UNBOX_GUARD_CSE` (JIT r23 wave 7, lane L8, applied by L1;
/// perf, default ON, kill `0`): a later `Op::Unbox` on a receiver node and
/// class whose null test and exact-class guard already passed in the same
/// block emits neither again (`ir_lower::Lowerer::unbox_class_proven`). Read
/// per lowered `Op::Unbox`, never on a hot path; no `static`.
pub fn ir_unbox_guard_cse_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_FFM_HEAP_ACCESS_PROVEN` (JIT r23 wave 8, lane W8-L4; perf,
/// default ON, kill `0`): the FFM element inline expansion files its HEAP scoped
/// access in [`Graph::ffm_proven_heap_accesses`], and `ir_lower`'s scoped arm
/// then emits neither the receiver null test, the base null test (and its
/// off-heap arm), the primitive-array screen nor the array bounds compares for
/// it: the expansion's own guards prove all four (see the field). Read per
/// expansion at build time; no static.
pub fn ir_ffm_heap_access_proven_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_FFM_INDEX_ALIGN_INVARIANT` (JIT r23 wave 8, lane W8-L4;
/// perf, default ON, kill `0`): in the index form (`getAtIndex` /
/// `setAtIndex`) the expansion's alignment guard tests the segment's start
/// address alone, `(start & (align - 1)) == 0`, instead of `start + idx *
/// width`. Equal for every index: `align` divides `width` (both powers of
/// two, `align <= width`, `ffm_inline_access_plan`), so `idx * width` (even
/// wrapped mod 2^64) adds no bit under the mask. The test is then
/// loop-invariant once the segment-field hoist moved `start` out, and the
/// length predicate moves the guard to the loop's pre-header. No static.
pub fn ir_ffm_index_align_invariant_enabled() -> bool {
    true
}

/// The `(compact, legacy)` addresses of [`UnboxOp::FieldBool`] /
/// [`UnboxOp::FieldRef`] at field `slot` of `class_id` -- the 1-byte and the
/// reference twins of `AtomicIntFieldLayout::new`:
///
/// * legacy: the 16-byte `Value` cell at `HEADER_SIZE + slot * SLOT_SIZE`,
///   payload at `FIELD_CELL_PAYLOAD32_OFFSET` (a `boolean` is an `Int` cell)
///   or `FIELD_CELL_PAYLOAD64_OFFSET` (the object pointer; its tag word is at
///   `legacy - (PAYLOAD64 - TAG)`, see [`ffm_element_ref_legacy_tag_offset`]);
/// * compact: the registered body offset, REFUSED unless the storage is
///   exactly the read's kind and width -- `Boolean` at 1 byte, `Reference` at
///   8 (a narrow oop would need decoding). No registered layout keeps the
///   legacy address for both arms (no instance can then carry
///   `GC_FLAG_COMPACT`), as the int resolver does.
pub fn ffm_element_read_offsets(op: UnboxOp, slot: usize, class_id: u32) -> Option<(i32, i32)> {
    use cratonvm_types::FieldStorageKind;
    if class_id == 0 {
        return None;
    }
    let (want, width, payload) = match op {
        UnboxOp::FieldBool(_) => (
            FieldStorageKind::Boolean,
            1,
            cratonvm_types::FIELD_CELL_PAYLOAD32_OFFSET,
        ),
        UnboxOp::FieldRef(_) => (
            FieldStorageKind::Reference,
            8,
            cratonvm_types::FIELD_CELL_PAYLOAD64_OFFSET,
        ),
        _ => return None,
    };
    let cell =
        cratonvm_types::HEADER_SIZE.checked_add(slot.checked_mul(cratonvm_types::SLOT_SIZE)?)?;
    let legacy = i32::try_from(cell.checked_add(payload)?).ok()?;
    let mut compact = legacy;
    if cratonvm_types::compact_ref_fields_enabled() {
        if let Some((body_off, storage)) = cratonvm_types::compact_field_disp_storage(class_id, slot)
        {
            if storage != want || storage.size_runtime() != width {
                return None;
            }
            compact = i32::try_from(body_off).ok().filter(|&d| d > 0)?;
        }
    }
    Some((compact, legacy))
}

/// The legacy cell's tag-word address for a [`UnboxOp::FieldRef`] whose
/// legacy payload address is `legacy` (from [`ffm_element_read_offsets`]).
pub fn ffm_element_ref_legacy_tag_offset(legacy: i32) -> i32 {
    // Cast: two small `usize` layout constants (8 and 0).
    legacy
        - (cratonvm_types::FIELD_CELL_PAYLOAD64_OFFSET as i32
            - cratonvm_types::FIELD_CELL_TAG_OFFSET as i32)
}

/// Unbox sites the optimizing tier LOWERED, for the engagement census.
pub(in crate::ir) static UNBOX_LOWERED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub(crate) fn note_unbox_lowered() {
    UNBOX_LOWERED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

pub fn unbox_lowered() -> u64 {
    UNBOX_LOWERED.load(std::sync::atomic::Ordering::Relaxed)
}

/// `CRATONVM_JIT_IR_SCALAR_INTRINSIC_FIRST_CALL` (r21 vector residual round) --
/// a call site the compile door found unresolved (`crate::JitFirstCallCell`)
/// may still lower as a scalar intrinsic, with the cell's one-shot
/// first-execution guard emitted in front of the arithmetic instead of a
/// call. Before it, such a site refused its whole METHOD from the optimizing
/// tier ("loop-shaped or memory call-site intrinsic"): `Math.max` in a loop
/// of an application class whose `java/lang/Math` entry nothing recorded --
/// every one, since the interpreter serves `Math.max` without resolving the
/// entry -- never reached the IR tier, so VEC21-5's min/max reduction never
/// ran there. Default ON; `=0` restores the refusal.
pub fn scalar_intrinsic_first_call_enabled() -> bool {
    true
}

pub fn try_ir_scalar_intrinsic(class: &str, method: &str, descriptor: &str) -> Option<ScalarOp> {
    if !ir_scalar_intrinsics_enabled() {
        return None;
    }
    match (class, method, descriptor) {
        ("java/lang/Math" | "java/lang/StrictMath", "min", "(II)I") => Some(ScalarOp::MinI),
        ("java/lang/Math" | "java/lang/StrictMath", "max", "(II)I") => Some(ScalarOp::MaxI),
        ("java/lang/Math" | "java/lang/StrictMath", "min", "(JJ)J") => Some(ScalarOp::MinL),
        ("java/lang/Math" | "java/lang/StrictMath", "max", "(JJ)J") => Some(ScalarOp::MaxL),
        ("java/lang/Math" | "java/lang/StrictMath", "abs", "(I)I") => Some(ScalarOp::AbsI),
        ("java/lang/Math" | "java/lang/StrictMath", "abs", "(J)J") => Some(ScalarOp::AbsL),
        ("java/lang/Integer", "compare", "(II)I") => Some(ScalarOp::CompareI),
        ("java/lang/Long", "compare", "(JJ)I") => Some(ScalarOp::CompareL),
        // `Integer.min`/`max` and `Long.min`/`max` are one-line delegations to
        // the `Math` methods directly above — `Integer.min(a, b)` IS
        // `Math.min(a, b)` in the JDK source — so they lower to the identical
        // sequence and need no new `ScalarOp`.
        //
        // They are here because they are separately native-shadowed in this VM,
        // and a shadowed leaf is an inlining barrier as well as an
        // un-compilable method: the caller cannot splice it (there is no
        // bytecode to splice) and, without a row here, cannot lower it as
        // arithmetic either, so a two-instruction operation costs a call
        // through Rust. `Math.min` was already covered; these three-character
        // spellings of it were not, and `Integer.max(a, b)` is not a rare way
        // to write it.
        ("java/lang/Integer", "min", "(II)I") => Some(ScalarOp::MinI),
        ("java/lang/Integer", "max", "(II)I") => Some(ScalarOp::MaxI),
        ("java/lang/Long", "min", "(JJ)J") => Some(ScalarOp::MinL),
        ("java/lang/Long", "max", "(JJ)J") => Some(ScalarOp::MaxL),
        ("java/lang/Integer", "numberOfLeadingZeros", "(I)I") => Some(ScalarOp::NlzI),
        ("java/lang/Long", "numberOfLeadingZeros", "(J)I") => Some(ScalarOp::NlzL),
        ("java/lang/Integer", "numberOfTrailingZeros", "(I)I") => Some(ScalarOp::NtzI),
        ("java/lang/Long", "numberOfTrailingZeros", "(J)I") => Some(ScalarOp::NtzL),
        ("java/lang/Integer", "reverseBytes", "(I)I") => Some(ScalarOp::ReverseBytesI),
        ("java/lang/Long", "reverseBytes", "(J)J") => Some(ScalarOp::ReverseBytesL),
        ("java/lang/Integer", "lowestOneBit", "(I)I") => Some(ScalarOp::LowestOneBitI),
        ("java/lang/Long", "lowestOneBit", "(J)J") => Some(ScalarOp::LowestOneBitL),
        ("java/lang/Integer", "highestOneBit", "(I)I") => Some(ScalarOp::HighestOneBitI),
        ("java/lang/Long", "highestOneBit", "(J)J") => Some(ScalarOp::HighestOneBitL),
        ("java/lang/Integer", "rotateLeft", "(II)I") => Some(ScalarOp::RotateLeftI),
        ("java/lang/Long", "rotateLeft", "(JI)J") => Some(ScalarOp::RotateLeftL),
        ("java/lang/Integer", "rotateRight", "(II)I") => Some(ScalarOp::RotateRightI),
        ("java/lang/Long", "rotateRight", "(JI)J") => Some(ScalarOp::RotateRightL),
        // JDK 25 `StrictMath.abs(float)` / `abs(double)` are `return
        // Math.abs(a);`: the same sign-bit clear, NaN payload kept (j27 wave 4,
        // lane SMALLFIX).
        ("java/lang/Math" | "java/lang/StrictMath", "abs", "(F)F") => Some(ScalarOp::AbsF),
        ("java/lang/Math" | "java/lang/StrictMath", "abs", "(D)D") => Some(ScalarOp::AbsD),
        // RAW only (round 15 wave 6, lane irosr2): `doubleToLongBits` and
        // `floatToIntBits` canonicalise NaN and must stay unmatched.
        ("java/lang/Double", "doubleToRawLongBits", "(D)J") if ir_fp_raw_bits_enabled() => {
            Some(ScalarOp::DoubleToRawLongBits)
        }
        ("java/lang/Double", "longBitsToDouble", "(J)D") if ir_fp_raw_bits_enabled() => {
            Some(ScalarOp::LongBitsToDouble)
        }
        ("java/lang/Float", "floatToRawIntBits", "(F)I") if ir_fp_raw_bits_enabled() => {
            Some(ScalarOp::FloatToRawIntBits)
        }
        ("java/lang/Float", "intBitsToFloat", "(I)F") if ir_fp_raw_bits_enabled() => {
            Some(ScalarOp::IntBitsToFloat)
        }
        // JIT round 19 (lane irops19, R15B-31), opt-in. `StrictMath` is
        // accepted for exactly the rows its javadoc specifies identically:
        // `sqrt` is IEEE 754 correctly rounded in both, and `StrictMath.min`/
        // `max`/`multiplyHigh`/`unsignedMultiplyHigh` delegate to `Math`.
        // `Math.min(int,int)`'s rows above already match; these are the FP
        // and 128-bit-product spellings the single-pass tier lowers as
        // `JitIntrinsic::MathSqrt` / `MathMin*` / `MathMultiplyHigh`.
        ("java/lang/Math" | "java/lang/StrictMath", "sqrt", "(D)D") if ir_fp_math_ops_enabled() => {
            Some(ScalarOp::SqrtD)
        }
        ("java/lang/Math" | "java/lang/StrictMath", "min", "(FF)F")
            if ir_fp_minmax_rows_enabled() =>
        {
            Some(ScalarOp::MinF)
        }
        ("java/lang/Math" | "java/lang/StrictMath", "max", "(FF)F")
            if ir_fp_minmax_rows_enabled() =>
        {
            Some(ScalarOp::MaxF)
        }
        ("java/lang/Math" | "java/lang/StrictMath", "min", "(DD)D")
            if ir_fp_minmax_rows_enabled() =>
        {
            Some(ScalarOp::MinD)
        }
        ("java/lang/Math" | "java/lang/StrictMath", "max", "(DD)D")
            if ir_fp_minmax_rows_enabled() =>
        {
            Some(ScalarOp::MaxD)
        }
        ("java/lang/Math" | "java/lang/StrictMath", "multiplyHigh", "(JJ)J")
            if ir_fp_math_ops_enabled() =>
        {
            Some(ScalarOp::MultiplyHighL)
        }
        ("java/lang/Math" | "java/lang/StrictMath", "unsignedMultiplyHigh", "(JJ)J")
            if ir_fp_math_ops_enabled() =>
        {
            Some(ScalarOp::UnsignedMultiplyHighL)
        }
        // JIT round 19 wave 2 (lane irops2, IROPS19-2), opt-in. Each gate is
        // the single-pass resolver's own (`try_resolve_intrinsic`: SSE4.1 for
        // the roundings, POPCNT for `bitCount`), and `fma` additionally needs
        // the INLINE FMA3 form (`inline_fma_enabled`: FMA3 + AVX present and
        // `CRATONVM_JIT_FMA_INLINE` not off) -- the single-pass tier calls a
        // helper otherwise, which this tier does not model; without it the
        // site stays a single-pass intrinsic and the planner refuses the
        // method as before. `StrictMath`'s floor/ceil/rint are exactly
        // specified and its `fma` delegates to `Math.fma`.
        //
        // JIT round 21 wave 3, lane intrinsics: exactly specified INCLUDING
        // the NaN bits, which `ROUNDSD` does not reproduce (it quiets a
        // signalling NaN, and keeps the sign `rint`'s body clears), and
        // HotSpot never intrinsifies the `StrictMath` three. Unmatched under
        // `crate::strictmath_rounding_exact_enabled` (default ON); the
        // single-pass resolver refuses the same sites, so they are plain calls.
        ("java/lang/StrictMath", "floor" | "ceil" | "rint", "(D)D")
            if crate::strictmath_rounding_exact_enabled() =>
        {
            None
        }
        ("java/lang/Math" | "java/lang/StrictMath", "floor", "(D)D")
            if ir_round_fma_bitcount_ops_enabled() && crate::x64::has_sse41() =>
        {
            Some(ScalarOp::FloorD)
        }
        ("java/lang/Math" | "java/lang/StrictMath", "ceil", "(D)D")
            if ir_round_fma_bitcount_ops_enabled() && crate::x64::has_sse41() =>
        {
            Some(ScalarOp::CeilD)
        }
        ("java/lang/Math" | "java/lang/StrictMath", "rint", "(D)D")
            if ir_round_fma_bitcount_ops_enabled() && crate::x64::has_sse41() =>
        {
            Some(ScalarOp::RintD)
        }
        ("java/lang/Math" | "java/lang/StrictMath", "fma", "(DDD)D")
            if ir_round_fma_bitcount_ops_enabled() && crate::x64::inline_fma_enabled() =>
        {
            Some(ScalarOp::FmaD)
        }
        ("java/lang/Math" | "java/lang/StrictMath", "fma", "(FFF)F")
            if ir_round_fma_bitcount_ops_enabled() && crate::x64::inline_fma_enabled() =>
        {
            Some(ScalarOp::FmaF)
        }
        ("java/lang/Integer", "bitCount", "(I)I")
            if ir_round_fma_bitcount_ops_enabled() && crate::x64::has_popcnt() =>
        {
            Some(ScalarOp::BitCountI)
        }
        ("java/lang/Long", "bitCount", "(J)I")
            if ir_round_fma_bitcount_ops_enabled() && crate::x64::has_popcnt() =>
        {
            Some(ScalarOp::BitCountL)
        }
        // nio20 wave 6, lane L3: the two surrogate range tests.
        ("java/lang/Character", "isHighSurrogate", "(C)Z")
            if ir_char_surrogate_scalar_enabled() =>
        {
            Some(ScalarOp::IsHighSurrogate)
        }
        ("java/lang/Character", "isLowSurrogate", "(C)Z") if ir_char_surrogate_scalar_enabled() => {
            Some(ScalarOp::IsLowSurrogate)
        }
        // JIT round 20 wave 11, lane pIR (L5W1-2): the three code-point tests.
        ("java/lang/Character", "charCount", "(I)I") if ir_code_point_scalar_enabled() => {
            Some(ScalarOp::CharCount)
        }
        ("java/lang/Character", "isBmpCodePoint", "(I)Z") if ir_code_point_scalar_enabled() => {
            Some(ScalarOp::IsBmpCodePoint)
        }
        ("java/lang/Character", "isValidCodePoint", "(I)Z") if ir_code_point_scalar_enabled() => {
            Some(ScalarOp::IsValidCodePoint)
        }
        ("java/lang/Character", "isISOControl", "(I)Z" | "(C)Z")
            if ir_code_point_scalar_enabled() =>
        {
            Some(ScalarOp::IsIsoControl)
        }
        // JIT round 22 wave 1, lane intrinsics (INTRINSICS21-7), opt-in. Wave 2
        // (INTRINSICS22-4): `StrictMath.round` too -- JDK 25's body is
        // `return Math.round(a);` for both widths, the same specification.
        ("java/lang/Math" | "java/lang/StrictMath", "round", "(D)J")
            if ir_math_round_scalar_enabled() =>
        {
            Some(ScalarOp::RoundD)
        }
        ("java/lang/Math" | "java/lang/StrictMath", "round", "(F)I")
            if ir_math_round_scalar_enabled() =>
        {
            Some(ScalarOp::RoundF)
        }
        // j27 wave 6 (lane T1BCE): BMI2 `PEXT` / `PDEP`, opt-in, BMI2 hosts
        // only (`ir::bit_compress` decides).
        (_, "compress" | "expand", _) => bit_compress::bit_compress_row(class, method, descriptor),
        _ => MathCallOp::match_triple(class, method, descriptor).map(ScalarOp::MathCall),
    }
}

/// `CRATONVM_JIT_IR_MATH_ROUND_SCALAR`, **default ON (flipped; `=0` kills)** -- JIT round
/// 22 wave 1, lane intrinsics, INTRINSICS21-7 of
/// `docs/feature-designs/jit-ir-optimizations.md`: the
/// optimizing tier lowers `Math.round(D)J` / `Math.round(F)I` as
/// [`ScalarOp::RoundD`] / [`ScalarOp::RoundF`] (the JDK body's bit arithmetic
/// inline) instead of a call into their registered `Intrinsic` natives.
/// Compile time only, not cached (no `static`).
pub fn ir_math_round_scalar_enabled() -> bool {
    // R22-AB-FLIP: default ON since the JIT r22 opt-in A/B (docs/feature-designs/jit-ir-optimizations.md); `=0` is the kill switch.
    true
}

/// `CRATONVM_JIT_IR_SCALAR_RANGE_FOLD`, **default on** (default on) -- JIT round
/// 19 wave 2, lane irops2, IROPS19-3 of `docs/feature-designs/jit-ir-optimizations.md`:
/// `range_analysis` gives the integer [`ScalarOp`] families exact interval
/// transfers (`min`/`max`/`abs`/`compare`, the bit counts and scans, ...)
/// instead of top, and `ir_optimize::fold_constants` folds a scalar intrinsic
/// whose operands are all constants (integer families, and the exactly
/// specified FP ones: `abs`, `sqrt`, `floor`, `ceil`, `rint`). Read per node
/// that IS a scalar intrinsic only (no `static`).
pub(crate) fn ir_scalar_range_fold_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_ROUND_FMA_BITCOUNT_OPS`, **default OFF** (opt-in) -- JIT
/// round 19 wave 2, lane irops2, IROPS19-2 of `docs/feature-designs/jit-ir-optimizations.md`:
/// the optimizing tier lowers `Math.floor`/`ceil`/`rint(D)` (SSE4.1
/// `ROUNDSD`), `Math.fma` (FMA3, both widths) and `Integer`/`Long.bitCount`
/// (`POPCNT`) as [`ScalarOp`]s instead of refusing the calling METHOD, each
/// only where the host has the instruction (see the rows in
/// [`try_ir_scalar_intrinsic`]). No `static`: `process_global_statics_ratchet`.
pub(in crate::ir) fn ir_round_fma_bitcount_ops_enabled() -> bool {
    false
}

/// `CRATONVM_JIT_IR_STATIC_ALIAS_CLASS`, **default OFF** (opt-in) -- JIT
/// round 19 wave 2, lane irops2, IROPS19-1 of
/// `docs/feature-designs/jit-ir-optimizations.md`: [`Graph::memory_effect`] classifies a
/// direct, non-volatile `getstatic` (and a direct primitive `putstatic`) as a
/// plain read (write) of its [`AliasClass::Static`] cell rather than
/// `MemEffect::OPAQUE`; `ir_optimize`'s LICM then lets such accesses stay in
/// a loop whose field / array loads it hoists, and refuses a direct-static
/// hoist per CELL rather than per loop. `lib.rs` sets
/// [`Graph::static_alias_class`] from it once per compile. No `static`:
/// `process_global_statics_ratchet`.
pub fn ir_static_alias_class_enabled() -> bool {
    false
}

/// `CRATONVM_JIT_IR_STATIC_REF_STORE_EFFECT`, **default on** (default on) -- JIT
/// round 19 wave 4, lane irops3, IROPS2-5 of `docs/feature-designs/jit-ir-optimizations.md`:
/// with [`ir_static_alias_class_enabled`] also on, a non-volatile reference
/// `putstatic` of an initialised class is `read + write(Static)` plus a
/// safepoint in [`Graph::memory_effect`] ([`Graph::direct_static_ref_store`])
/// instead of `MemEffect::OPAQUE`, and `ir_optimize`'s LICM predicates treat
/// it as writing only that cell. Inert alone. `lib.rs` sets
/// [`Graph::static_ref_store_effect`] from it once per compile. No `static`:
/// `process_global_statics_ratchet`.
pub fn ir_static_ref_store_effect_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_FP_MATH_OPS`, **default OFF** (opt-in) -- JIT round 19,
/// lane irops19, R15B-31 of the r18 triage work list: the optimizing tier
/// lowers `Math.sqrt(D)`, FP `Math.min`/`max` (float and double) and
/// `Math.multiplyHigh` / `unsignedMultiplyHigh` as [`ScalarOp`]s
/// (`SqrtD`, `MinF`..`MaxD`, `MultiplyHighL`, `UnsignedMultiplyHighL`)
/// instead of refusing the calling METHOD. Read through
/// [`try_ir_scalar_intrinsic`] only, so the planner and the builder cannot
/// disagree within one compile. No `static`: `process_global_statics_ratchet`.
pub(in crate::ir) fn ir_fp_math_ops_enabled() -> bool {
    false
}

/// `CRATONVM_JIT_IR_FP_MINMAX`, **default ON** (`=0` kills) -- JIT round 23
/// wave 2, lane L8: the four FP `Math.min`/`max` rows (`MinF`..`MaxD`) are
/// recognised without `CRATONVM_JIT_IR_FP_MATH_OPS`, so a method calling them
/// reaches this tier (and VEC21-5's FP reduction can engage) instead of being
/// refused whole. Their lowering is the JDK body's answer bit for bit (NaN
/// ARGUMENT with its payload, `-0.0` below `+0.0`); `sqrt` and the 128-bit
/// products stay under the older opt-in.
pub(in crate::ir) fn ir_fp_minmax_rows_enabled() -> bool {
    ir_fp_math_ops_enabled()
        || true
}

/// `CRATONVM_JIT_IR_CHAR_SURROGATE_SCALAR`, **default ON** (`=0` is the kill
/// switch) -- nio20 wave 6, lane L3: the optimizing tier lowers
/// `Character.isHighSurrogate(C)Z` / `isLowSurrogate(C)Z` as one range test
/// ([`ScalarOp::IsHighSurrogate`] / [`ScalarOp::IsLowSurrogate`]) instead of a
/// call into their registered native. Off: both sites are an `Op::Call` again.
/// Read through [`try_ir_scalar_intrinsic`] only (planner and builder ask the
/// same function), compile time only, not cached.
pub fn ir_char_surrogate_scalar_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_CODE_POINT_SCALAR`, **default ON** (`=0` is the kill
/// switch) -- JIT round 20 wave 11, lane pIR (L5W1-2 of
/// `docs/feature-designs/jit-ir-optimizations.md`): the
/// optimizing tier lowers `Character.charCount(I)I`, `isBmpCodePoint(I)Z` and
/// `isValidCodePoint(I)Z` as one compare each ([`ScalarOp::CharCount`],
/// [`ScalarOp::IsBmpCodePoint`], [`ScalarOp::IsValidCodePoint`]) instead of a
/// call into their registered `Intrinsic`-kind native, and (through
/// [`try_ir_scalar_intrinsic`]) admits them as spliced scalar rows. Off: all
/// three are an `Op::Call` again. Compile time only, not cached.
pub fn ir_code_point_scalar_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_FP_RAW_BITS`, **default ON** (`=0` is the kill switch) --
/// round 15 wave 6, lane irosr2 (IRO-1 of `jit-r15w6-irosr-proposals-RETIRED-20261003.md`): the
/// optimizing tier lowers `Double.doubleToRawLongBits` / `longBitsToDouble`
/// and `Float.floatToRawIntBits` / `intBitsToFloat` as one register move each
/// ([`ScalarOp::raw_bits_move`]). Off: the two `Double` sites refuse the whole
/// METHOD again (they are single-pass `JitIntrinsic` sites, so the planner's
/// "loop-shaped or memory call-site intrinsic" refusal takes them), and the
/// two `Float` sites go back to an `Op::Call`. Read through
/// [`try_ir_scalar_intrinsic`] only, which both the planner and the builder
/// ask, so the two halves cannot disagree within one compile (a declared flag
/// reads the VM's latched snapshot). No `static`:
/// `process_global_statics_ratchet`. j27 SPLICEFFM2: `pub(crate)` for the
/// spliced element admission (`ffm_element_inline::spliced_ffm_element_admission`),
/// which refuses an FP carrier the builder below would decline.
pub(crate) fn ir_fp_raw_bits_enabled() -> bool {
    true
}

/// May a spliced callee body contain its own branches? **Default ON**;
/// `CRATONVM_JIT_IR_SPLICE_BRANCH=0` restores the straight-line-only shape.
///
/// Read by BOTH halves of the feature — the splice scanner in `jit_bridge`,
/// which decides whether to admit such a callee, and [`IrBuilder::build`],
/// which pre-scans the admitted body's control flow. They must agree: a body
/// admitted without its pre-scan is exactly the orphan-node failure STUB-S8
/// was, so the scanner asks the VM-side mirror of this and the builder asks
/// this, and neither may be flipped alone.
///
/// Read per call (round 15 wave 5, lane chain6; CH4W4-7 of
/// `jit-r15w4-chain4-proposals-RETIRED-20261003.md`), no process `OnceLock`: it gates
/// [`ir_splice_multi_return_enabled`] and so the chain arm's `multi-return`
/// stage, and a latched answer ignored a test's thread override after the
/// first read in the process. The key is a declared flag, so both readers see
/// the same latched `VmFlags` snapshot in production (the agreement above
/// holds exactly as before); its readers are once per planned site or build,
/// never a hot path. Off-words are now the shared default-on rule
/// (`runtime_flag_default_on`: also trimmed and case-insensitive).
pub fn ir_splice_branch_enabled() -> bool {
    true
}

/// Round nio21 wave 10 (lane L2): may a division by a non-zero CONSTANT go
/// unguarded, and so be spliced? **Default ON**;
/// `CRATONVM_JIT_IR_SPLICE_CONST_DIVISION=0` restores both halves at once:
/// `IrBuilder::add_div_zero_guard` plants its guard for every divisor again,
/// and the VM's inline resolver refuses every spliced division
/// (`ir-splice-division`). One switch for both, because a resolver that
/// admitted a body the builder then guarded would only cost a refused build.
pub fn ir_splice_const_division_enabled() -> bool {
    true
}

/// Round nio21 wave 10 (lane L2, proposal W9L2-3): may a division whose
/// divisor is NOT a known constant keep its zero-divisor guard inside a
/// spliced body (`IrBuilder::splice_div_guard_resumes`)? **Default ON**;
/// `CRATONVM_JIT_IR_SPLICE_DIV_GUARD=0` restores the fence (such a guard
/// refuses the build) and makes the VM's inline resolver refuse every
/// variable-divisor body again. The resolver admits one only for a reviewed
/// row (`crate::splice_division_by_variable_admitted`: the `Math.floorDiv` /
/// `floorMod` bodies).
pub fn ir_splice_div_guard_enabled() -> bool {
    true
}

/// Round nio21 wave 12 (lane L4, W10L2-3): may the byte screens that used to
/// refuse every integer division admit one by a non-zero constant? The four
/// are `lib.rs`'s `bytecode_body_cannot_throw`, `ir_self_copy_trap_free_opcode`
/// (both self-copy admissions), `ir_sync_splice_body_scan`, and this file's
/// [`splice_callee_handler_plan`]. **Default ON**;
/// `CRATONVM_JIT_IR_SPLICE_SCREENS_CONST_DIVISION=0` restores the blanket
/// refusal in all four (the handler plan's variable-divisor admission, which
/// rides [`ir_splice_div_guard_enabled`], included).
pub fn ir_splice_screens_const_division_enabled() -> bool {
    true
}

/// The non-zero constant divisor of the integer division (`idiv` / `ldiv` /
/// `irem` / `lrem`) at `div_pc` of `code[..code_len]`, judged from the bytes
/// the way the VM's inline resolver judges a spliced one (round nio21 wave
/// 12, lane L4): the instruction immediately before it on the linear walk is
/// a constant push -- `iconst_m1..5`, `lconst_0/1`, `bipush`, `sipush`, or an
/// `ldc` / `ldc_w` / `ldc2_w` whose integral value `ldc_value(pc)` supplies
/// (`None`: unresolved, floating, or not offered) -- and no branch lands on
/// the division (a join there would make the divisor a phi). Non-zero at the
/// division's width; -1 and `MIN` are answered, both cannot trap (the
/// lowering's `NEG` / 0 and a plain `IDIV`; see `IrBuilder::add_div_zero_guard`).
pub fn splice_division_const_divisor(
    code: &[u8],
    code_len: usize,
    div_pc: usize,
    ldc_value: impl Fn(usize) -> Option<i64>,
) -> Option<i64> {
    let len = code_len.min(code.len());
    let op = *code.get(div_pc).filter(|_| div_pc < len)?;
    if !matches!(op, 0x6c | 0x6d | 0x70 | 0x71) {
        return None;
    }
    if crate::bytecode_analysis::branch_target_map(code, len)
        .get(div_pc)
        .copied()
        .unwrap_or(true)
    {
        return None;
    }
    let mut pc = 0usize;
    let mut prev = None;
    while pc < div_pc {
        prev = Some(pc);
        pc += crate::bytecode_analysis::step(code, pc).max(1);
    }
    if pc != div_pc {
        return None;
    }
    let p = prev?;
    let v = match *code.get(p)? {
        push @ 0x02..=0x08 => i64::from(push) - 3,
        push @ (0x09 | 0x0a) => i64::from(push) - 9,
        // Cast: `bipush`'s operand is a signed byte.
        0x10 => i64::from(*code.get(p + 1)? as i8),
        0x11 => i64::from(i16::from_be_bytes([*code.get(p + 1)?, *code.get(p + 2)?])),
        0x12..=0x14 => ldc_value(p)?,
        _ => return None,
    };
    // Cast: an `int` division reads the low 32 bits.
    let nonzero = if matches!(op, 0x6d | 0x71) {
        v != 0
    } else {
        (v as i32) != 0
    };
    nonzero.then_some(v)
}

/// [`splice_division_const_divisor`] under the screens' switch
/// ([`ir_splice_screens_const_division_enabled`]) and the builder's own
/// ([`ir_splice_const_division_enabled`], without which the builder guards
/// even a constant divisor): can the division at `div_pc` not trap?
pub fn splice_division_cannot_trap(
    code: &[u8],
    code_len: usize,
    div_pc: usize,
    ldc_value: impl Fn(usize) -> Option<i64>,
) -> bool {
    ir_splice_screens_const_division_enabled()
        && ir_splice_const_division_enabled()
        && splice_division_const_divisor(code, code_len, div_pc, ldc_value).is_some()
}

/// Is `divisor` a constant other than 0 at the division's width `ty`
/// (`int` reads the low 32 bits, as `ir_lower::const_int_divisor` does)?
/// -1 and `MIN` count: neither can trap in the lowering
/// (see `IrBuilder::add_div_zero_guard`).
pub(in crate::ir) fn const_divisor_is_nonzero(graph: &Graph, ty: IrType, divisor: NodeId) -> bool {
    match graph.node_opt(divisor).map(|n| &n.op) {
        // Cast: an `int` division reads the low 32 bits.
        Some(Op::Const(c)) => match ty {
            IrType::Int => (*c as i32) != 0,
            IrType::Long => *c != 0,
            _ => false,
        },
        _ => false,
    }
}

/// May a spliced callee body have more than one `return`? Its own default is
/// OFF, but unset it follows the chain arm, which is ON by default since
/// round 16 ([`CHAIN_ARM_DEFAULT_STAGE`]);
/// `CRATONVM_JIT_IR_SPLICE_MULTI_RETURN=1` lifts the
/// `ir-splice-not-single-trailing-return` refusal, `=0` keeps it.
///
/// # Why it is off, given that it works
///
/// It works, and it is soak-clean: 39 deterministic workloads under three
/// collectors with 0 divergence, checksum parity against Temurin JDK 25 on 14
/// workloads, and a probe built for it (`MultiRet`) where the census reads
/// `bodies=3 return_edges=8` on and `bodies=0` off.
///
/// What it does not do is go faster. Interleaved, order-flipped, 14 rounds on
/// that probe: **+0.0 % median, faster in 5 of 14 paired rounds.** Splicing
/// removes a call and adds a merge and a phi, and on this shape those cancel.
///
/// It used to cost something on the other side as well: a merge built inside a
/// spliced body set `SPLICED_MERGE_SEEN`, which withheld the artifact's
/// `ir_osr_entries` wholesale. That containment came off on 2026-09-09 — the
/// wrong answer was `emit_osr_entry_stubs` jumping past the block that writes a
/// constant arm's home word, not the splice — so the OSR door is no longer
/// forfeited by turning this on.
///
/// What is left is the measurement, and the measurement is neutral. It stays off
/// until a shape is found where splicing a multi-return body pays; the argument
/// that would have flipped it (coverage against nothing) is gone with the
/// containment, because there is no longer anything to trade coverage against.
///
/// Implies [`ir_splice_branch_enabled`] in practice — a second `return` is
/// only reachable through a branch. This is the GENERAL admission switch: the
/// splice scanner in `jit_bridge` and `lib.rs`'s `append_ir_inline_site` admit
/// a multi-return body for any callee only under it.
///
/// # The builder no longer reads it (round 11 wave 11, lane `calls`)
///
/// It used to be read by both halves — the scanner, and
/// [`IrBuilder::splice_return`] / [`IrBuilder::finish_multi_return_splice`],
/// which build the continuation — so that neither could be flipped alone:
/// admitting a body the builder walks straight through is the orphan-node
/// failure STUB-S8 was. The builder now recognises a multi-return body from
/// its own verified decode whatever this says (see `IrBuilder::build`'s
/// splice pre-scan), so the hazard cannot arise from a switch at all, and the
/// one narrower admission that does not go through this switch — a bounded
/// self-recursive copy, [`ir_recursive_inline_enabled`] — needs no second
/// reader to agree with.
///
/// # No longer implied by `CRATONVM_JIT_IR_RECURSIVE_INLINE` (round 11 wave 11)
///
/// Self-recursion is the shape where a multi-return splice pays (every
/// recursive method has a base case, so almost every one has two `return`s,
/// and a recursive splice removes a whole frame per copy). Round 9 made the
/// recursive-inline flag turn THIS switch on, which admitted multi-return
/// bodies for every callee of every method. The recursive admission is now
/// its own, narrower rule — a body that calls itself, spliced only as a copy
/// of the method being compiled — so this switch keeps its measured-neutral
/// default for everything else.
///
/// # Read per call, not cached (round 14 wave 1, lane chain; proposal CH5-4)
///
/// It was a process `OnceLock`, unlike the two chain switches it is flipped
/// with (`ir_splice_frame_states_enabled`, `ir_splice_chain_fences_enabled`):
/// the first reader in the process fixed it for every VM, and a test pinning it
/// with `with_thread_overrides` read whatever an earlier test latched. Now the
/// same per-read rule as those two (`runtime_flag_on`, which also accepts any
/// on-word the flag table accepts). Its readers are the planner and the splice
/// resolver, once per planned site, never a hot path.
///
/// # Part of the chain arm's umbrella (round 15 wave 4, lane chain4; CHN15-7)
///
/// Unset, it follows [`ir_chain_arm_stage`] (`CRATONVM_JIT_IR_CHAIN_ARM`,
/// stage 1 and up); set, its own value wins either way
/// (`chain_arm_switch_on`). Flipping the chain arm is
/// [`CHAIN_ARM_DEFAULT_STAGE`], not a word here.
pub fn ir_splice_multi_return_enabled() -> bool {
    ir_splice_branch_enabled()
        && chain_arm_switch_on(
            "CRATONVM_JIT_IR_SPLICE_MULTI_RETURN",
            ChainArmStage::MultiReturn,
        )
}

/// The chain arm's stages, cumulative (round 15 wave 4, lane chain4; proposal
/// CHN15-7 of `jit-r15-chain-proposals-RETIRED-20261003.md`), as the umbrella
/// `CRATONVM_JIT_IR_CHAIN_ARM` names them. Each stage adds one switch to the
/// one before, in the order the proposal flips them -- the switch that adds
/// no deopt state first, the switch that relaxes replay fences last -- so a
/// regression found at a stage is localised to the switch that stage added.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ChainArmStage {
    /// `0` / `off`: none of the three (their own values still apply). Unset
    /// meant this until round 16 flipped [`CHAIN_ARM_DEFAULT_STAGE`] to `Full`.
    Off,
    /// `multi-return`: [`ir_splice_multi_return_enabled`] alone. Adds no deopt
    /// state (a multi-return splice's caller scopes are one `SpliceFrame`
    /// state on every return path).
    MultiReturn,
    /// `frame-states`: + [`ir_splice_frame_states_enabled`] (W5-3 stages 1-2:
    /// chain snapshots, a guard's chain preferred and the flat frame its
    /// fall-back; no fence relaxed).
    FrameStates,
    /// `1` / `on` / `all` / any other on-word: + [`ir_splice_chain_fences_enabled`]
    /// (W5-3 stage 3: fences relaxed on the strength of the chains) -- the
    /// whole chain arm the round-13 wave-11 verdict judged sound.
    Full,
}

/// The umbrella's value when `CRATONVM_JIT_IR_CHAIN_ARM` is unset. **FULL**
/// since round 16.
///
/// This constant IS the chain arm's flip (CHN15-7). Round 16 flipped it to
/// [`ChainArmStage::Full`] (measured: `CratonBench hashmap` 645→564 ms,
/// `strings` 3448→2632 ms; the round-16 lane-chain soak read `refused=0` with
/// every forced exit resumed, see
/// `docs/feature-designs/profile-guided-inlining.md`),
/// registered the token's `off_word` (`0`), and re-pinned the tests that
/// assumed the three switches default off. `CRATONVM_JIT_IR_CHAIN_ARM=0` is
/// the kill switch: nothing then reads differently from before the umbrella
/// existed.
pub const CHAIN_ARM_DEFAULT_STAGE: ChainArmStage = ChainArmStage::Full;

/// `CRATONVM_JIT_IR_CHAIN_ARM` -- the chain arm's staging umbrella (round 15
/// wave 4, lane chain4; CHN15-7). One token turns on the three chain switches
/// ([`ir_splice_multi_return_enabled`], [`ir_splice_frame_states_enabled`],
/// [`ir_splice_chain_fences_enabled`]) together, or the first one or two of
/// them ([`ChainArmStage`]): `multi-return`, `frame-states`, and any other
/// on-word for all three; an off-word (`0`, `false`, `off`, `no`, empty) for
/// none. Unset: [`CHAIN_ARM_DEFAULT_STAGE`]. A switch set explicitly keeps its
/// own value whatever the umbrella says (`chain_arm_switch_on`), so
/// `CRATONVM_JIT_IR_CHAIN_ARM=1 CRATONVM_JIT_IR_SPLICE_CHAIN_FENCES=0` is the
/// stage-2 arm, and after the flip `CRATONVM_JIT_IR_SPLICE_FRAME_STATES=0`
/// still turns chains off (and with them the fences, which require them).
/// Read per call like the three (never a hot path); no static.
pub fn ir_chain_arm_stage() -> ChainArmStage {
    match None::<std::ffi::OsString> {
        None => CHAIN_ARM_DEFAULT_STAGE,
        Some(value) => {
            let value = value.to_string_lossy().trim().to_ascii_lowercase();
            if value.is_empty() || cratonvm_types::flags::value_is_off(&value) {
                ChainArmStage::Off
            } else if value == "multi-return" {
                ChainArmStage::MultiReturn
            } else if value == "frame-states" {
                ChainArmStage::FrameStates
            } else {
                ChainArmStage::Full
            }
        }
    }
}

/// One of the chain arm's three switches: `key`'s own value when it is set
/// (the `runtime_flag_on` rule: on unless empty or an off-word), otherwise
/// whether the umbrella's stage reaches `from` ([`ir_chain_arm_stage`]).
pub(in crate::ir) fn chain_arm_switch_on(key: &str, from: ChainArmStage) -> bool {
    match cratonvm_types::flags::runtime_var_os(key) {
        Some(value) => {
            let value = value.to_string_lossy();
            !value.trim().is_empty() && !cratonvm_types::flags::value_is_off(&value)
        }
        None => ir_chain_arm_stage() >= from,
    }
}

/// `CRATONVM_JIT_IR_SPLICE_FRAME_STATES=1` -- round 13 wave 3 (lane
/// framestate), W5-3 stages 1 and 2
/// (`docs/feature-designs/jit-ir-optimizations.md`).
/// **Default ON since round 16** through the chain arm
/// ([`CHAIN_ARM_DEFAULT_STAGE`]); `=0` turns it off.
///
/// On: the builder records a chain snapshot ([`SpliceScopeChain`]) at every
/// instruction of a spliced body, and `ir_lower` gives a guard inside the body
/// a deopt frame that is the CALLEE's own frame at its own bci, with every
/// caller parked mid-`invoke` as `FrameState::caller` -- the JVMS chain
/// HotSpot publishes -- instead of the caller's frame at the outermost
/// `invoke` with re-execute semantics. The VM's chain materialiser
/// (`deopt_resume::materialise_inlined_chain`) resumes it.
///
/// No splice fence is relaxed by it: every body it applies to is one the
/// replay fences already proved re-executable, so the flat re-execution the
/// chain replaces would have computed the same thing. That is what makes the
/// fall-back safe -- a chain the lowerer cannot make resumable by the VM (a
/// scalar-replaced value, a lock to re-take, a slot with no location) leaves
/// that guard on the flat frame, exactly as with the switch off. What changes
/// observably: an implicit exception raised by such a deopt is thrown from
/// the callee's frame (the stack trace gains it). Since round 13 wave 8 (lane
/// chain3) a chain guard admits the optimizing OSR door's planless guard exit
/// when no scope holds a monitor or a scalar-replaced object
/// (`osr_exit::chain_resumable_in_place`); before, any chain box refused the
/// whole body's guard exits. Default off
/// until a soak reads it clean. Read once per build; not cached in a static
/// (`process_global_statics_ratchet`). Round 15 wave 4 (lane chain4, CHN15-7):
/// unset, it follows the umbrella `CRATONVM_JIT_IR_CHAIN_ARM` from
/// [`ChainArmStage::FrameStates`] up ([`ir_chain_arm_stage`]).
pub fn ir_splice_frame_states_enabled() -> bool {
    chain_arm_switch_on(
        "CRATONVM_JIT_IR_SPLICE_FRAME_STATES",
        ChainArmStage::FrameStates,
    )
}

/// `CRATONVM_JIT_IR_SPLICE_ANEWARRAY` -- JIT round 25 wave 10 (W10-6),
/// default ON since j27 2026-10-28, `=0` kills: the optimizing tier splices a callee body that allocates a
/// REFERENCE array (`anewarray`, 0xbd). The VM's resolver resolved every such
/// site's component class (the caller's own `anewarray` rows come from the
/// same resolver) and refuses the body otherwise; `lib.rs` rebases the rows
/// (`InlineSite::ir_anewarray_info`) into [`IrInlineTables::anewarray_info`],
/// and the builder's `0xbd` arm, which has handled a spliced `anewarray` since
/// round 11, builds the `Op::NewArray` exactly as for the caller's own. Off,
/// every such body is refused (`new/anewarray/multianewarray`), which kept
/// `Matcher.<init>` (`new IntHashSet[...]`) a call out of `Pattern.matcher`,
/// and `ArrayList.grow` out of `add`. A primitive `newarray` was never
/// refused. Read per resolution and per rebase; no static.
pub fn ir_splice_anewarray_enabled() -> bool {
    // WARNING j27-user-flip-20261028 (default ON; =0 restores the old default)
    true
}

/// `CRATONVM_JIT_IR_SPLICE_ANEWARRAY_LOOP_CALLEES=1` -- JIT round 25 wave 13
/// (W13-6), **opt-in** and inert without [`ir_splice_anewarray_enabled`]: also
/// splice an `anewarray`-bearing callee that has a back edge, which is wave
/// 10's rule. Off (the default), such a callee stays a call
/// ([`splice_anewarray_refuses_loop_callee`]). Kept so the host can A/B the
/// two rules on one binary. Read per resolution; no static.
///
/// Round rx27 wave 1 (lane matcher): back to opt-in. The j27 mass flip
/// (`WARNING j27-user-flip-20261028`) turned this switch ON, which is the
/// measured LOSS arm of W13-6 (`cached` +12.6 % instructions per op: the
/// `Pattern.split(CharSequence,int,boolean)` loop root moved into a caller
/// whose budget it had spent), and left the W13-6 fix inert. Its ON state is
/// not a feature to flip: it exists to reproduce the loss.
pub fn ir_splice_anewarray_loop_callees_enabled() -> bool {
    false
}

/// JIT round 25 wave 13 (W13-6, under `CRATONVM_JIT_IR_SPLICE_ANEWARRAY`):
/// does the splice resolver keep an `anewarray`-bearing callee body as a CALL
/// because it is a loop root? `Some(back_edges)` when it does: the body
/// (`code[..code_len]`, the bytes the resolver scanned) carries an `anewarray`
/// (`has_anewarray`) and at least one back edge, and
/// [`ir_splice_anewarray_loop_callees_enabled`] is off.
///
/// Why. Before wave 10 every such body was refused for its `anewarray`, so a
/// loop-rooted callee (`Pattern.split(CharSequence, int, boolean)`, whose loop
/// runs `find`, `subSequence` and `ArrayList.add` per match) compiled as its
/// own optimizing-tier root with the whole splice budget for its in-loop
/// sites (21 sites, 1 181 bytes on the host). Admitting it moved the loop up
/// into the caller (`Pattern.split(CharSequence)` -> the row's lambda ->
/// `spin`), which had spent most of its budget on the body itself, so the
/// loop's own sites (`Matcher.find`, `String.subSequence`, ...) were left as
/// calls: `cached` +12.6 % instructions per op (orchestrator, c26/c31). A
/// constructor or an accessor that sizes an array (`Matcher.<init>`,
/// `ArrayList.grow`) has no back edge and is still admitted.
///
/// Only a refusal: with `has_anewarray` false, or the opt-in switch on, the
/// answer is `None` and the resolver is exactly wave 10's. Pure apart from the
/// switch read.
pub fn splice_anewarray_refuses_loop_callee(
    code: &[u8],
    code_len: usize,
    has_anewarray: bool,
) -> Option<usize> {
    if !has_anewarray || ir_splice_anewarray_loop_callees_enabled() {
        return None;
    }
    let edges = crate::bytecode_analysis::back_edges(code, code_len).len();
    (edges > 0).then_some(edges)
}

/// JIT round 19 (lane splice19, CH16-4): an opcode that, built inside a
/// spliced body, makes no node a guard or deopt can be lowered at: the
/// constants (not `ldc`), local loads and stores, `pop`/`dup`/`swap` and
/// friends, and the arithmetic, shift, logic, `iinc`, conversion and compare
/// families save division and remainder. See
/// `IrBuilder::splice_pc_may_read_chain`.
pub(in crate::ir) fn splice_opcode_cannot_trap(op: u8) -> bool {
    matches!(
        op,
        // nop, aconst_null, iconst/lconst/fconst/dconst, bipush, sipush
        0x00..=0x11
            // iload .. aload_3
            | 0x15..=0x2d
            // istore .. astore_3
            | 0x36..=0x4e
            // pop, dup -- exactly the stack ops `admits_after_committed_splice_store`
            // admits, so a raised fence never asks `relax_fence_by_chain` at a
            // pc this rule skipped (`dup_x1`, `dup2`, `swap` keep their chain)
            | 0x57
            | 0x59
            // add/sub/mul; 0x6c..=0x73 (div/rem) are the gap
            | 0x60..=0x6b
            // neg, shifts, and/or/xor, iinc, conversions, lcmp/fcmp/dcmp
            | 0x74..=0x98
    )
}

/// `CRATONVM_JIT_IR_CHAIN_SNAPSHOTS_LAZY` -- JIT round 19 (lane splice19,
/// proposal CH16-4 of `jit-r16-chain-proposals`). Opt-in, default OFF.
///
/// With the chain snapshots on (`CRATONVM_JIT_IR_SPLICE_FRAME_STATES`, the
/// chain arm), the builder records one at EVERY spliced instruction; on: only
/// at an instruction a guard can be lowered at
/// (`IrBuilder::splice_pc_may_read_chain`). The skipped ones are never read
/// (the post-escape-analysis prune drops them before lowering), but until then
/// they cost build and optimizer time and keep every value they name alive
/// through GVN/DCE/LICM. A/B: `CRATONVM_DBG_IR_CHAIN_SNAPSHOTS=1` prints the
/// per-compile census (`recorded=`, `skipped=`, `kept=`, `published=`,
/// `code=`, `compile_us=`). Read once per splicing build; not cached in a
/// static (`process_global_statics_ratchet`).
pub fn ir_chain_snapshots_lazy_enabled() -> bool {
    false
}

/// `CRATONVM_JIT_IR_POST_CALL_SPLICE_EXITS` -- interpreter round i1 wave 47,
/// lane L3: the first stage of
/// `docs/feature-designs/deopt-osr.md`.
///
/// On: the builder records, at the successor of every real call it builds
/// INSIDE a spliced body, a chain snapshot of the frames that call returns
/// into ([`SafepointSnapshot::post_call_of`], [`SafepointSnapshot::post_call_only`]:
/// no guard, fence or by-pc reader sees it), and an optimizing OSR-door
/// compile puts a post-call exit after that call whose frame state is the
/// chain (`ir_lower::Lowerer::post_call_chain_exit_state`). A redefinition
/// that withdraws the body forces the site; the frame then leaves through
/// the VM's planless chain transfer
/// (`deopt_resume::transfer_osr_guard_exit_chain_into_live_frame`): the
/// looping method parked in its `invoke` of the spliced callee, the callee
/// pushed above it at the call's successor, as HotSpot's deopt of the
/// compiled frame unpacks it. Off (`=0`): nothing is recorded, and a call
/// inside a splice gets no site, as before wave 47. **Default ON** since the
/// wave-47 lane's last commit (revert that commit to turn it off by default).
///
/// Read once per build, and by the planner for the chain's identities
/// (`Graph::scope_method_key`, the spliced scopes' class ids and sources);
/// not cached in a static (`process_global_statics_ratchet`).
pub fn ir_post_call_splice_exits_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_ENTRY_POST_CALL_SPLICE_EXITS` -- interpreter round i2 wave
/// 54, lane L2: the second stage of
/// `docs/feature-designs/deopt-osr.md`.
///
/// On: an optimizing METHOD-ENTRY compile puts a post-call exit after a real
/// call made inside a spliced body, as the OSR-door compiles have since wave
/// 47 ([`ir_post_call_splice_exits_enabled`], which must be on too: it records
/// the chain snapshot). Its point is the same caller chain; it leaves through
/// the method-entry stash sinks (the interpreter doors and the compiled
/// callers' call-site service), which rebuild the chain and, for a spliced
/// scope whose class a redefinition replaced, that scope from the bytecode
/// the compile spliced. Admitted only when every spliced scope of the chain
/// has its bytecode kept on the artifact ([`Graph::entry_chain_source_keys`];
/// never for an `ACC_SYNCHRONIZED` method), and the VM's verdict refuses the
/// exit while a kept body would not translate onto its class's current pool
/// (`vm::jit::helpers::post_call_chain_exits_resumable`), so the sink never
/// meets a chain it must refuse after the call committed. Off (`=0`): such a
/// call gets no site in a method-entry compile, as before wave 54. **Default
/// ON** since the wave-54 lane's last commit (revert that commit to turn it
/// off by default).
///
/// Read once per compile that has a chain snapshot (the lowerer's
/// `post_call_chain_exit_state`); not cached in a static
/// (`process_global_statics_ratchet`).
pub fn ir_entry_post_call_splice_exits_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_SPLICED_HEADER_POLL_EXITS` -- interpreter round i2 wave
/// 57, lane L2: stage 1 of
/// `docs/feature-designs/deopt-osr.md`.
///
/// On: the unconditional back-edge poll of a loop INSIDE a spliced callee of
/// an optimizing OSR-door compile gets a mode exit whose frame is a caller
/// chain -- the callee at its own loop header with this edge's φ inputs,
/// every caller parked in its `invoke` -- built from the chain snapshot the
/// builder records at the header ([`ir_splice_frame_states_enabled`]), and
/// leaves through the VM's planless chain transfer
/// (`deopt_resume::transfer_osr_guard_exit_chain_into_live_frame`), as a
/// post-call chain exit does (`ir_lower::Lowerer::spliced_back_edge_mode_exit_state`).
/// Off (`=0`): such a poll stays verdict-blind (`shape:spliced-header`), as
/// before wave 57. **Default ON** since the wave-57 lane's last commit
/// (revert that commit to turn it off by default).
///
/// Read once per spliced back-edge poll at compile time; not cached in a
/// static (`process_global_statics_ratchet`).
pub fn ir_spliced_header_poll_exits_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_ENTRY_SPLICED_HEADER_POLL_EXITS` -- interpreter round i2
/// wave 57, lane L2: stage 2 of the proposal of
/// [`ir_spliced_header_poll_exits_enabled`], for METHOD-ENTRY compiles. The
/// same chain exit, admitted only when the artifact keeps the bytecode of
/// every spliced scope it names ([`Graph::entry_chain_source_keys`]), the
/// condition of [`ir_entry_post_call_splice_exits_enabled`]: the method-entry
/// stash sinks rebuild the chain and, for a spliced scope a redefinition
/// replaced, that scope from the kept bytecode; the VM's poll verdict stays
/// while a kept body would not translate
/// (`vm::jit::helpers::chain_poll_exits_resumable_for`). Off (`=0`): such a
/// poll stays verdict-blind in a method-entry compile, as before wave 57.
/// **Default ON** since the wave-57 lane's last commit (revert that commit
/// to turn it off by default).
pub fn ir_entry_spliced_header_poll_exits_enabled() -> bool {
    true
}

/// `CRATONVM_DEOPT_EAGER_CHAINS=1` -- round 13 wave 11 (lane chain5), proposal
/// CH-1 of `jit-r13-chain2-proposals-RETIRED-20260929.md`: forced chain deopts, a differential
/// soak switch. **Default OFF; a debug switch, never a performance one.**
///
/// Every guard `ir_lower` publishes with a caller chain (a guard inside a
/// spliced body under [`ir_splice_frame_states_enabled`]) also takes its deopt
/// pad on every 64th arrival at which it PASSED. The interpreter then resumes
/// the chain at a state where the guarded bytecode does not throw, so the
/// rebuilt frames keep running -- every local, stack slot and re-allocated
/// object of every scope is read -- instead of ending in the exception a real
/// null / bounds trap raises at once. Sound by construction: every chain guard
/// fires BEFORE the bytecode it protects (`REEXECUTE` innermost, `RESUME`
/// after the `invoke` for every caller), so a resume at a passing guard
/// computes what the compiled code would have. A probe whose answer changes
/// under this switch has found a chain-resume defect. The forced trips are
/// ordinary traps to the VM (de-speculation and trap counts see them), so a
/// hot method is recompiled, and eventually left to the interpreter, after a
/// bounded number of them: the switch soaks resume correctness, not speed.
pub fn ir_forced_chain_deopts_enabled() -> bool {
    false
}

/// `CRATONVM_JIT_IR_SPLICE_CHAIN_FENCES=1` -- round 13 wave 3 (lane
/// framestate), W5-3 stage 3. **Default ON since round 16** through the
/// chain arm ([`CHAIN_ARM_DEFAULT_STAGE`]; `=0` turns it off), and inert
/// unless [`ir_splice_frame_states_enabled`] is on too.
///
/// The splice replay fences exist because every trap inside a splice used to
/// resume at the OUTERMOST `invoke` with re-execute semantics, which runs the
/// callee again from its first byte: nothing observable may have happened
/// before a trap. With a chain snapshot at the trapping pc the trap resumes the
/// CALLEE at its own bci instead (the JVMS chain), so what the body already
/// committed stays committed exactly once. Under this switch the builder
/// therefore admits, at a pc whose chain snapshot it recorded:
///
/// * any instruction after a committed store or a surviving call (the
///   walk-top fence, [`IrBuilder::admits_after_committed_splice_store`]);
/// * an array store into an array the splice did not allocate (committed like
///   a `putfield` is);
/// * a receiver null guard at a nested `invoke` in an impure body;
///
/// and marks the graph [`Graph::splice_chains_required`], which turns every
/// guard in a spliced body from "chain preferred, flat frame as fall-back" into
/// "chain or refuse the compile" (`ir_lower`), keeps escape analysis from
/// scalar-replacing an object a chain names (a chain the VM could not
/// materialise), and stops LICM from hoisting a spliced trapping node onto a
/// flat frame. `lib.rs`'s planner stops pre-refusing sites for the two fences.
/// Multi-return bodies (`putVal`, `getNode`) still need
/// `CRATONVM_JIT_IR_SPLICE_MULTI_RETURN=1`. Round 15 wave 4 (lane chain4,
/// CHN15-7): unset, it follows the umbrella `CRATONVM_JIT_IR_CHAIN_ARM` at
/// [`ChainArmStage::Full`] ([`ir_chain_arm_stage`]).
pub fn ir_splice_chain_fences_enabled() -> bool {
    ir_splice_frame_states_enabled()
        && chain_arm_switch_on("CRATONVM_JIT_IR_SPLICE_CHAIN_FENCES", ChainArmStage::Full)
}

/// `CRATONVM_JIT_IR_LAMBDA_SAM_SPLICE` (JIT round 23 wave 1, lane L2;
/// INLINE22-4 = RUNTIME22-3; perf, **default ON**, `=0` the kill switch): the
/// optimizing tier splices a profiled lambda proxy receiver's SAM call as the
/// proxy's forwarding body -- the capture reads, the argument loads and casts,
/// the implementation's call -- behind `ExactClassIs(proxy id)`, with the
/// implementation spliced in turn, as C2 inlines the spun class's method. The
/// VM resolver builds the body (`jit_bridge::resolve_ir_lambda_sam_site`);
/// with the switch off every such site keeps the `receiver-is-lambda-proxy`
/// refusal and its interface call. Read per resolution.
pub fn ir_lambda_sam_splice_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_FORWARDER_FLAT_UNBOX` (JIT r23 wave 5, lane L2; perf,
/// **default ON**, `=0` the kill switch): see
/// [`IrBuilder::unbox_resumes_flat_in_forwarder`]. Read per site.
pub fn ir_forwarder_flat_unbox_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_LDC_CONDY_SLOT` (JIT r23 wave 4b, lane L2; L2W4B-1; perf,
/// **default OFF**, opt-in): the optimizing builder builds an `ldc` of a
/// resolved reference `CONSTANT_Dynamic` (`JitLdcConstant::DynamicSlot`) as an
/// [`Op::IndyRootRef`] of its GC-maintained slot, so a method holding one (the
/// JDK-spun lambda forwarder that loads its implementation handle as class
/// data) reaches the optimizing tier instead of staying single-pass. Off, the
/// site has no IR row and the builder bails the method, as before. Read per
/// compile.
pub fn ir_ldc_condy_slot_enabled() -> bool {
    false
}

/// `CRATONVM_JIT_IR_LAMBDA_SAM_SPLICE_CONVERSIONS` (JIT round 23 wave 2, lane
/// L2; INLINE22-4 stage 2, L2W1-1; perf, **default ON**, `=0` the kill
/// switch): a lambda SAM forwarder with LambdaMetafactory's boxing / unboxing
/// calls around the implementation is spliced too, as an OPAQUE forwarder (no
/// deopt chain under it: `SpliceFrame::opaque`); a conversion call that does
/// not splice stays a call. Read per resolution.
pub fn ir_lambda_sam_splice_conversions_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_LAMBDA_SAM_SPLICE_ARRAY_CASTS` (t1 wave 11, lane JLAM;
/// LAMBDA221-2's remainder; perf, **default ON since j27 2026-10-28**, `=0`
/// kills): the lambda SAM splice's forwarder screen admits an ARRAY `checkcast`
/// (`Function<String[], R>`) as a typecheck row, as the general splice's
/// typecheck rows already do, instead of refusing the forwarder ("an array
/// cast"). Read per resolution.
pub fn ir_lambda_sam_splice_array_casts_enabled() -> bool {
    // WARNING j27-user-flip-20261028 (default ON; =0 restores the old default)
    true
}

/// `CRATONVM_JIT_IR_LAMBDA_SAM_SPLICE_NESTED` (JIT round 23 wave 2d, lane
/// L2; L2W1-3; perf, **default ON**, `=0` the kill switch): a lambda SAM
/// forwarder is spliced inside another spliced body too, where the builder
/// proves its receiver the proxy (an exact-receiver row, a nested guarded or
/// trap-guarded row); a trap in it resumes the enclosing body at the
/// replaced `invoke` (`IrBuilder::forwarder_reexecute_chain`). Transparent
/// forwarders only. Read per resolution.
pub fn ir_lambda_sam_splice_nested_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_LAMBDA_SAM_SPLICE_CTOR_REFS` (JIT round 23 wave 3, lane
/// L2; perf, **default ON**, `=0` the kill switch): a constructor reference's
/// forwarder (`new C; dup; ..; invokespecial C.<init>; areturn`) is spliced as
/// an OPAQUE forwarder at depth 0 (`SpliceFrame::opaque`: its result is the
/// allocation), the `new` an ordinary spliced allocation of an initialised,
/// non-finalizable class and the `<init>` spliced in turn or kept as its
/// direct call. Read per resolution.
pub fn ir_lambda_sam_splice_ctor_refs_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_LAMBDA_SAM_SPLICE_HANDLE_FORWARDERS` (JIT round 23 wave
/// 4, lane L2; perf, **default ON**, `=0` the kill switch): a forwarder that
/// calls the implementation through its direct `MethodHandle`
/// (`InnerClassLambdaMetafactory.useImplMethodHandle`: a protected
/// implementation in another package, a `super::` reference) is spliced as the
/// direct call that handle is (`invokespecial` / `invokevirtual` /
/// `invokestatic` of the implementation), its class-data `ldc` dropped. Read
/// per resolution.
pub fn ir_lambda_sam_splice_handle_forwarders_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_LAMBDA_SAM_SPLICE_IMPL_CALL` (JIT round 23 wave 3, lane
/// L2; perf, **default ON**, `=0` the kill switch): a depth-0 forwarder whose
/// implementation does not splice -- an overridable virtual / interface
/// method reference (`Object::toString`, `List::size`), a body over the cap --
/// is spliced with the implementation kept as its call (an inline cache, or a
/// direct entry), instead of refused. Read per resolution.
pub fn ir_lambda_sam_splice_impl_call_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_LAMBDA_SAM_SPLICE_VIRTUAL_REFS` (JIT round 23 wave 2, lane
/// L2; INLINE22-4 stage 2; perf, **default ON**, `=0` the kill switch): a
/// `REF_invokeVirtual` (or class `REF_invokeInterface`) method reference's
/// forwarder is spliced when the nested resolution proves its target
/// monomorphic (a final class or method: `String::length`). Read per
/// resolution.
pub fn ir_lambda_sam_splice_virtual_refs_enabled() -> bool {
    true
}

/// JIT round 23 wave 1, lane L2 (INLINE22-4): is a spliced body whose
/// `class_id` this is a lambda proxy's SAM forwarder? The VM mints proxy class
/// ids from [`crate::ir_lower::GETCLASS_WORDS_LAMBDA_BASE_ID`] upward, a range
/// disjoint from every defined class (`class_realm::LAMBDA_PROXY_ID_BASE`,
/// asserted equal), and the only resolver that answers a body for a proxy is
/// the forwarder's ([`ir_lambda_sam_splice_enabled`]). Such a body has no
/// interpreter frame: it names no level of a stack trace
/// ([`IrInlineFrameSites::chain_at`]) and no scope of a deopt chain
/// (`IrBuilder::splice_scope_chain`). `u32::MAX` is never one (a sentinel).
pub fn ir_splice_body_is_lambda_forwarder(class_id: u32) -> bool {
    class_id >= crate::ir_lower::GETCLASS_WORDS_LAMBDA_BASE_ID && class_id != u32::MAX
}

/// Splice an `invokevirtual` whose receiver is proven to be EXACTLY the class
/// the body was resolved for (round 12 wave 4, lane iropt3, proposal W3-1).
/// **Default ON**; `CRATONVM_JIT_IR_EXACT_RECEIVER_SPLICE=0` is the kill
/// switch, and with it every exact-receiver site stays the call it was.
///
/// The resolver refuses an IR splice of a virtual target that the language
/// does not make monomorphic (`ir-splice-target-not-provably-monomorphic`),
/// because the IR has no receiver guard to fall back on. `map.put(k, v)` on a
/// `HashMap map = new HashMap<>()` is monomorphic all the same: the receiver's
/// class is exactly `HashMap`, and that fact is monotone. The planner asks the
/// resolver for such a site under that contract and hands the builder the
/// class ([`IrBuilder::add_exact_receiver_site`]); the builder splices only
/// when [`IrBuilder::exact_receiver_proof`] shows the receiver is that class,
/// and builds the ordinary call otherwise.
///
/// Read per compile (and per exact site), never on a hot path, like
/// [`ir_recursive_inline_enabled`].
pub fn ir_exact_receiver_splice_enabled() -> bool {
    true
}



/// `CRATONVM_JIT_IR_NESTED_GUARDED_SPLICE` -- JIT round 19 (lane splice19,
/// CH6W5-1 of `jit-r15w5-chain6-proposals`). Opt-in, default OFF.
///
/// On: a nested exact-receiver site (an `invokevirtual` inside a spliced body
/// that the resolver resolved for its constant-pool class, HS-2) whose
/// receiver the builder cannot prove exact splices behind an `ExactClassIs`
/// test on that class, with its call on the miss edge -- the W6-1 shape one
/// level down -- instead of staying the call. Needs the chain arm's fences
/// (the planner keeps nested exact rows only under them,
/// `ir_splice_nested_exact_admitted`). The class is the constant-pool one
/// (HS-2's exact row) or, where the VM's nested resolver finds no such row
/// (`Object.equals` / `hashCode` on a map key, an abstract declaration), the
/// class the CALLEE's own receiver profile or compiled inline cache says
/// dominates that pc (`jit_bridge` `nest-profiled`, same switch). A/B:
/// `CRATONVM_DBG_JITC=1` prints
/// `ir-nested-guarded-splice` per opened splice; `R16ChainGetNode`,
/// `R16ChainSoak`, `R16Bench hashmap`.
///
/// JIT round 19 proposal round (2026-10-08): default ON, kill word `0`
/// (R19Splice19NestedGuard field-exact 4 -> 2 ns, nothing worse).
pub fn ir_nested_guarded_splice_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_NESTED_BIMORPHIC_SPLICE` -- JIT round 19 wave 2 (lane
/// splice2, SPLICE19-4 of `jit-r19-splice19-proposals`). Opt-in, default OFF,
/// and inert without [`ir_nested_guarded_splice_enabled`] (and the bimorphic
/// splice's own switches).
///
/// On: a nested virtual site the VM's nested resolver finds no dominant class
/// for (`jit_bridge` `nest-profiled`) but whose CALLEE receiver profile names
/// a bimorphic pair ([`nested_bimorphic_receiver_pair`], the top-level GS-1
/// shares) is handed to the planner with both bodies; the builder tests the
/// second class on the first test's miss edge and keeps the call on the last
/// miss edge -- GS-1's shape one level down
/// (`IrBuilder::begin_bimorphic_receiver_splice`, at the depth the site is
/// at). A/B: `CRATONVM_DBG_JITC=1` prints `ir-nested-bimorphic-splice` per
/// opened site and `nest-profiled ... guard on classes A+B`;
/// `R19Splice19NestedGuard` `map-mixed`.
pub fn ir_nested_bimorphic_splice_enabled() -> bool {
    ir_nested_guarded_splice_enabled()
        && ir_bimorphic_guarded_splice_enabled()
        && false
}

/// `CRATONVM_JIT_IR_NESTED_GUARDED_INTERFACE` -- JIT round 19 proposal wave 5
/// (lane mega5, MEGAOSR4-1 of `docs/feature-designs/deopt-osr.md`). Opt-in,
/// default OFF, and inert without [`ir_nested_guarded_splice_enabled`].
///
/// On: the VM's nested resolver (`jit_bridge` `nest-profiled`) also plans an
/// `invokeinterface` inside a spliced body from the CALLEE's own receiver
/// profile (or compiled inline cache), exactly as it plans an
/// `invokevirtual`: a dominant class becomes a guarded nested row, a
/// bimorphic pair (under [`ir_nested_bimorphic_splice_enabled`]) a two-class
/// row. The planner and builder already treat both opcodes alike: the
/// builder's `0xb9` arm asks the same exact-receiver proof and the same
/// [`IrBuilder::add_nested_guarded_receiver_site`] plan the `0xb6` arm does
/// (W7-1's guarded interface splice, one level down), with the kept
/// interface call (its own row and inline cache) on the miss edge. Before
/// this, a spliced body's interface site could only stay a call (seeded at
/// best, MEGAOSR19-2). A/B: `CRATONVM_DBG_JITC=1` prints
/// `nest-profiled-iface ... -> guard on class N` per planned row and
/// `ir-nested-guarded-splice` per opened splice; `R19Mega5Nested` rows
/// `iface-nested-mono-call` / `iface-nested-pair-call`.
///
/// Read per nested interface site at resolution time, never on a hot path.
pub fn ir_nested_guarded_interface_enabled() -> bool {
    ir_nested_guarded_splice_enabled()
}

/// `CRATONVM_JIT_IR_NESTED_BIMORPHIC_EXACT_ROW` -- JIT round 19 wave 3 (lane
/// splice3, SPLICE2-4 of `jit-r19-splice2-proposals`). Opt-in, default OFF,
/// inert without [`ir_nested_bimorphic_splice_enabled`]. The VM's HS-2 arm
/// (`jit_bridge` `nest-exact`, the constant-pool class) also resolves the
/// other class of a bimorphic callee profile that contains the constant-pool
/// class, as the row's second body; planner and builder treat it as the
/// `nest-profiled` pair (SPLICE19-4).
pub fn ir_nested_bimorphic_exact_row_enabled() -> bool {
    ir_nested_bimorphic_splice_enabled()
}

/// `CRATONVM_JIT_IR_NESTED_MISS_POST_CALL_EXIT` -- JIT round 19 wave 3 (lane
/// splice3, SPLICE2-5 of `jit-r19-splice2-proposals`). Opt-in, default OFF,
/// and inert unless the builder records post-call chain snapshots
/// (`CRATONVM_JIT_IR_POST_CALL_SPLICE_EXITS`). The nested guarded splice's
/// miss call (`SpliceFrame::miss_call`) gets the post-call chain snapshot
/// an ordinary spliced call gets, at the join's successor, with the call's
/// own result in place of the join's value phi (on the miss edge the phi IS
/// the call's result; nothing else at the successor differs between the
/// edges, since the join restores the saved caller frame). See
/// `IrBuilder::push_post_call_splice_state`.
pub fn ir_nested_miss_post_call_exit_enabled() -> bool {
    false
}

/// `CRATONVM_JIT_IR_SPLICE_EXPECTED_THROW` -- JIT round 19 wave 3 (lane
/// splice3, SPLICE2-2 = SPLICE19-3 = TF15-1). Opt-in, default OFF; both
/// halves read it:
///
/// * VM (`deopt_resume`): the per-bci escalation of a CHAIN trap whose
///   reason is an implicit exception ([`crate::deopt::is_expected_throw_reason`])
///   records the named innermost entry `(callee key, callee bci,
///   expected_throw_speculation_id)` instead of the call-site wildcard, once;
///   a later escalation at the same site (the recompile did not mark the
///   guard) records the wildcard as before. A trip of a point marked
///   [`crate::deopt::ResumeSemantics::expected_throw`] is resumed (the
///   interpreter throws) and neither charged nor de-spec'd.
/// * JIT: the planner keeps the splice (no wildcard), lists the named rows
///   of every spliced body ([`splice_expected_throw_rows`] into
///   [`Graph::splice_expected_throw`]), and `ir_lower` marks a chain guard of
///   that `(innermost key, innermost bci, reason)`.
pub fn ir_splice_expected_throw_enabled() -> bool {
    false
}

/// `CRATONVM_JIT_IR_NESTED_GUARD_CLASS_EVIDENCE` -- JIT round 19 wave 4 (lane
/// splice4, SPLICE3-1 of `docs/feature-designs/deopt-osr.md`). Default ON since the round-19 proposal round (`=0` turns it off),
/// inert without [`ir_nested_guarded_splice_enabled`]. The VM's
/// `nest-profiled` resolver, finding neither a callee receiver profile nor an
/// inline-cache answer at the nested pc, falls back to the class the callee's
/// OWN optimizing artifact tested there
/// (`CompiledMethod::guarded_splice_class_at_bci`): since SPLICE2-3 that
/// artifact's miss-edge inline cache is (rightly) no evidence, but its guard
/// class is the class it chose for the hit edge. The nested guard keeps the
/// splice sound whatever the class; a wrong one costs the miss edge.
pub fn ir_nested_guard_class_evidence_enabled() -> bool {
    ir_nested_guarded_splice_enabled()
}

/// `CRATONVM_JIT_IR_SPLICE_EXPECTED_THROW_STASH_SINKS` -- JIT round 19 wave 4
/// (lane splice4, SPLICE3-2). Opt-in, default OFF, inert without
/// [`ir_splice_expected_throw_enabled`]. The stash sinks outside the door sink
/// (`deopt_resume`) -- the trapped-callee helper sink (a compiled caller's
/// callee trapped) and the first-call tier-up sink -- apply SPLICE2-2's rule
/// too: a marked expected-throw trip is resumed without a charge, and a
/// chain's implicit-exception escalation names the innermost guard first
/// instead of the call-site wildcard. Off, they charge and wildcard every
/// chain trap, marked or not (wave 3's shape).
pub fn ir_splice_expected_throw_stash_sinks_enabled() -> bool {
    ir_splice_expected_throw_enabled()
        && false
}

/// `CRATONVM_JIT_IR_SPLICE_EXPECTED_THROW_RATE_LIMIT` -- JIT round 19 wave 4
/// (lane splice4, SPLICE3-3). Opt-in, default OFF, inert without
/// [`ir_splice_expected_throw_enabled`]. A marked expected-throw trip is never
/// charged, so a spliced guard that fails on (almost) EVERY call keeps paying a
/// chain deopt plus an interpreted throw per call where the unmarked rule
/// would have withdrawn the splice and left a compiled call. On, each body
/// counts its marked trips (`CompiledMethod::splice_expected_throw_trips`) and
/// a trip is charged as without the mark once
/// [`expected_throw_trip_rate_exceeded`] says so; the ordinary escalation then
/// records the call-site wildcard on its next limit (the named entry is
/// already present), and the recompile drops the splice.
pub fn ir_splice_expected_throw_rate_limit_enabled() -> bool {
    ir_splice_expected_throw_enabled()
        && false
}

/// SPLICE3-3: marked trips one body takes before its rate is judged at all
/// (a short run of throws on a cold body is what the mark exists to keep).
pub const EXPECTED_THROW_TRIP_FLOOR: u32 = 64;

/// SPLICE3-3: has a body that took `trips` marked expected-throw trips over
/// `entries` method entries (`CompiledMethod::entry_count`) thrown too often to
/// keep the mark? Asked only for a body whose entry counter is armed (JIT
/// round 20, lane throw: a body without one has no denominator and keeps the
/// mark, `deopt_resume::expected_throw_trip_uncharged`). Past
/// [`EXPECTED_THROW_TRIP_FLOOR`], when the trips exceed half the entries.
/// An OSR or loop-resident body enters rarely and trips per iteration, so it
/// crosses the bar as soon as the floor is passed: such a site is the
/// pathological loop the rule exists for. Pure.
pub fn expected_throw_trip_rate_exceeded(trips: u32, entries: u64) -> bool {
    trips > EXPECTED_THROW_TRIP_FLOOR && u64::from(trips).saturating_mul(2) > entries
}

/// SPLICE2-2: the `(callee method key, callee bci, reason)` rows of the
/// spliced bodies in `sites` that the de-spec registry names as expected
/// throws ([`crate::deopt::expected_throw_speculation_id`], exact entries
/// only: a wildcard is not one), kept only where the body's opcode at that
/// bci raises the reason's exception when the guard fails
/// ([`crate::deopt::opcode_throws_for_reason`]), so a hoisted or speculative
/// guard that names the reason without an exception is never marked. Sorted,
/// deduplicated. `combined` is the planner's buffer (each body at its
/// `base`); `original_bodies` its unmasked copies
/// ([`IrInlineTables::original_body_codes`]), read first.
pub fn splice_expected_throw_rows(
    registry: &crate::deopt::DespecRegistry,
    sites: &HashMap<usize, IrInlineSite>,
    combined: &[u8],
    original_bodies: &HashMap<usize, Vec<u8>>,
) -> Vec<(String, u32, crate::deopt::DeoptReason)> {
    let mut out: Vec<(String, u32, crate::deopt::DeoptReason)> = Vec::new();
    for site in sites.values() {
        if site.method_key.is_empty() {
            continue;
        }
        let body = match original_bodies.get(&site.base) {
            Some(original) => original.get(..site.code_len),
            None => combined.get(site.base..site.base.saturating_add(site.code_len)),
        };
        let Some(body) = body else {
            continue;
        };
        for (bci, id) in registry.entries_for(&site.method_key) {
            for reason in crate::deopt::EXPECTED_THROW_REASONS {
                if id == crate::deopt::expected_throw_speculation_id(bci, reason)
                    && body
                        .get(bci as usize)
                        .is_some_and(|&op| crate::deopt::opcode_throws_for_reason(op, reason))
                {
                    out.push((site.method_key.clone(), bci, reason));
                }
            }
        }
    }
    out.sort_unstable_by(|a, b| (&a.0, a.1, a.2 as u32).cmp(&(&b.0, b.1, b.2 as u32)));
    out.dedup();
    out
}

/// JIT round 19 wave 2 (lane splice2, SPLICE19-4): the bimorphic pair a
/// nested site's CALLEE receiver profile names, under exactly the shares the
/// top-level GS-1 planner asks of the compiling method's own profile
/// ([`profile_bimorphic_receiver_classes`]). `None` for anything else.
pub fn nested_bimorphic_receiver_pair(
    counts: &crate::profile::ReceiverCounts,
) -> Option<(u32, u32)> {
    crate::profile::ReceiverEvidence::of(counts)
        .bimorphic_pair(
            crate::MIN_GUARDED_RECEIVER_EVIDENCE,
            PROFILE_GUARDED_MIN_RECEIVER_PCT,
            GUARDED_SPLICE_MIN_RECEIVER_PCT,
            BIMORPHIC_MIN_PAIR_RECEIVER_PCT,
        )
        .filter(|&(a, b)| a != 0 && b != 0 && a != b)
}

/// Round 12 wave 7 (lane iropt6, proposal W6-1): the guarded splice of an
/// exact-receiver site whose receiver is not proven exact
/// ([`IrBuilder::add_guarded_receiver_site`]).
///
/// **Default ON**; `CRATONVM_JIT_IR_GUARDED_RECEIVER_SPLICE=0` is the kill
/// switch, and with it every such site is the plain call it was in wave 6.
/// Read per compile and per planned site, never on a hot path.
pub(crate) fn ir_guarded_receiver_splice_enabled() -> bool {
    true
}



/// The share (percent) of the recorded receivers at a site below which a
/// guarded splice ([`IrBuilder::add_guarded_receiver_site`]) is not planned.
///
/// Low on purpose: the miss edge is the ordinary call, not a deopt, so a miss
/// costs one class-id compare. What a site the profile says is dominated by
/// other classes loses is that compare and the code size of a body that never
/// runs, which is what this bar refuses.
pub(in crate::ir) const GUARDED_SPLICE_MIN_RECEIVER_PCT: u32 = 10;

/// The largest body (in bytecode bytes, the callee's own) a guarded splice
/// takes: HotSpot's `MaxInlineSize` for a site it has no frequency for.
///
/// Before round 12 wave 7 every such body was appended by the planner and then
/// declined by the builder unwalked, so a guarded splice walks bodies no build
/// walked before; a refusal inside one costs a W5-2 rebuild, and the rebuilds
/// are capped. Keeping to small bodies keeps that exposure (and the code the
/// miss edge makes dead weight) small. `HashMap.newNode` is 13 bytes, the
/// `afterNode*` hooks 1. Round 13 wave 12 (lane guardsplice, W7-1): also the
/// planner's cap for a profile-selected site (`lib.rs`), which takes no body
/// the guarded plan would then decline unwalked.
pub(crate) const GUARDED_SPLICE_MAX_BODY_BYTES: usize = 35;

/// nio21 wave 6, lane L4
/// (`r21-profile-guarded-splice-caps-hot-sites-at-the-cold-size-FIXED-20261012.md`):
/// the largest body a PROFILE-HOT one-class guarded splice takes -- HotSpot's
/// `FreqInlineSize`, the cap it applies to a call site its profile says is
/// frequent. [`GUARDED_SPLICE_MAX_BODY_BYTES`] is `MaxInlineSize`, HotSpot's cap
/// for a site with NO frequency; a profile-guarded site always has one (that is
/// how it got its class), so holding every such body to 35 bytes treated hot
/// sites as cold. The planner decides which sites are hot and how much of the
/// compile's byte budget one may take (`lib.rs` `ir_guarded_splice_site_cap`),
/// and registers them with [`IrBuilder::add_guarded_hot_cap_site`]; every
/// other guarded body (the bimorphic and polymorphic ones included) keeps the
/// cold cap.
pub(crate) const GUARDED_SPLICE_HOT_MAX_BODY_BYTES: usize = crate::MAX_INLINE_BYTECODE_SIZE;

/// `CRATONVM_JIT_IR_GUARDED_SPLICE_HOT_CAP` (nio21 wave 6, lane L4) --
/// **default ON**, `=0` restores [`GUARDED_SPLICE_MAX_BODY_BYTES`] for every
/// guarded body, in the planner and the builder alike. Read per compile and
/// per planned site, never on a hot path.
pub fn ir_guarded_splice_hot_cap_enabled() -> bool {
    true
}

/// Round 13 wave 12 (lane guardsplice, proposal W7-1 of the round-12 iropt
/// book): the IR planner may resolve an `invokevirtual` / `invokeinterface`
/// site for the receiver class its profile says dominates it
/// ([`profile_guarded_receiver_class`]), and hand the builder that class as a
/// guarded site ([`IrBuilder::add_guarded_receiver_site`]): the class test is
/// exact, the miss edge is the site's own call, as for the constant-pool class
/// of round 12 wave 7.
///
/// **Default ON**; `CRATONVM_JIT_IR_PROFILE_GUARDED_SPLICE=0` is the kill
/// switch, and the guarded splice's own switch
/// (`CRATONVM_JIT_IR_GUARDED_RECEIVER_SPLICE=0`) turns this off with it. Read
/// per compile and per planned site (the planner, and the VM's resolver gate),
/// never on a hot path.
pub fn ir_profile_guarded_splice_enabled() -> bool {
    ir_guarded_receiver_splice_enabled()
        && true
}

/// The share (percent) of a site's recorded receivers its top class must hold
/// before the planner resolves a profile-selected guarded splice for it
/// ([`profile_guarded_receiver_class`]). The same bar the inline cache at that
/// site is seeded with (`dominant_receiver(.., 80)` in `lib.rs`'s invoke plan),
/// so the splice and the cache agree on which class the site is "about".
/// Below HotSpot's `TypeProfileMajorReceiverPercent` (90) because the miss
/// edge here is the call, never a trap.
pub(crate) const PROFILE_GUARDED_MIN_RECEIVER_PCT: u32 = 80;

/// Round 13 wave 12 (lane guardsplice, W7-1): the profile-read half of a
/// profile-selected guarded splice. The class id the receiver profile of the
/// caller's `pc` says dominates it, or `None`:
///
/// * no profile, no row for `pc`, or fewer than
///   [`crate::MIN_GUARDED_RECEIVER_EVIDENCE`] samples (a guess costs a body in
///   the code and a compare per call, so a thin profile plans nothing);
/// * a top class under [`PROFILE_GUARDED_MIN_RECEIVER_PCT`] percent (a
///   bimorphic or megamorphic site; see the lane's proposal book for the
///   two-guard form);
/// * the "unknown" class id 0.
///
/// Read-only over [`crate::profile::MethodProfile::receivers`], the
/// receiver-type profile the interpreter and the compiled virtual door record
/// per bci -- the same map the site's inline cache is seeded from. A HINT, not
/// a proof: the builder's class test is exact and its miss edge is the
/// ordinary call, so a stale or wrong answer costs a compare and never a
/// wrong target.
pub fn profile_guarded_receiver_class(
    profile: Option<&crate::profile::MethodProfile>,
    pc: usize,
) -> Option<u32> {
    let counts = profile.and_then(|p| p.receivers.get(&pc))?;
    let evidence = crate::profile::ReceiverEvidence::of(counts);
    if evidence.summary().observations < crate::MIN_GUARDED_RECEIVER_EVIDENCE {
        return None;
    }
    evidence
        .seed_a_guarded_cache(PROFILE_GUARDED_MIN_RECEIVER_PCT)
        .filter(|&class_id| class_id != 0)
}

/// Round 14 wave 2 (lane guard, GS-1 of `jit-r13-guardsplice-proposals-RETIRED-20260929.md`):
/// the bimorphic guarded splice ([`IrBuilder::add_bimorphic_receiver_site`]):
/// a profile-selected site whose receivers are split between two classes
/// tests both, each with its own body, and keeps the call for the rest.
///
/// **Default ON**; `CRATONVM_JIT_IR_BIMORPHIC_GUARDED_SPLICE=0` is the kill
/// switch (every such site is the plain call it was in round 13), and both
/// switches of [`ir_profile_guarded_splice_enabled`] turn it off with them.
/// Read per compile and per planned site, never on a hot path.
pub(crate) fn ir_bimorphic_guarded_splice_enabled() -> bool {
    ir_profile_guarded_splice_enabled()
        && true
}

/// The share (percent) of a site's recorded receivers its top TWO classes
/// must hold together before the planner resolves a bimorphic guarded splice
/// for it ([`profile_bimorphic_receiver_classes`]). HotSpot's bimorphic
/// inlining asks for exactly two types because its miss edge is a trap; here
/// the miss edge is the call, so a small remainder is admitted.
pub(crate) const BIMORPHIC_MIN_PAIR_RECEIVER_PCT: u32 = 90;

/// Round 14 wave 2 (lane guard, GS-1): the two classes the receiver profile
/// of the caller's `pc` splits between, top first, or `None`
/// ([`crate::profile::ReceiverEvidence::bimorphic_pair`]):
///
/// * no profile, no row, or fewer than [`crate::MIN_GUARDED_RECEIVER_EVIDENCE`]
///   samples;
/// * a top class at [`PROFILE_GUARDED_MIN_RECEIVER_PCT`] or more -- the
///   one-class profile-guarded splice owns that site
///   ([`profile_guarded_receiver_class`]), unchanged;
/// * a pair under [`BIMORPHIC_MIN_PAIR_RECEIVER_PCT`], a second class under
///   [`GUARDED_SPLICE_MIN_RECEIVER_PCT`], or the "unknown" class id 0.
///
/// A HINT, like the one-class reader: both tests are exact and the miss edge
/// is the ordinary call.
pub(crate) fn profile_bimorphic_receiver_classes(
    profile: Option<&crate::profile::MethodProfile>,
    pc: usize,
) -> Option<(u32, u32)> {
    let counts = profile.and_then(|p| p.receivers.get(&pc))?;
    crate::profile::ReceiverEvidence::of(counts).bimorphic_pair(
        crate::MIN_GUARDED_RECEIVER_EVIDENCE,
        PROFILE_GUARDED_MIN_RECEIVER_PCT,
        GUARDED_SPLICE_MIN_RECEIVER_PCT,
        BIMORPHIC_MIN_PAIR_RECEIVER_PCT,
    )
}

/// Round 16 (lane mega): the POLYMORPHIC guarded splice
/// ([`IrBuilder::add_polymorphic_receiver_sites`]): a profile-selected site
/// whose receivers are split over three or four classes (none holding the
/// one-class share, no two holding the pair share) tests each in turn, each
/// with its own spliced body, and keeps the call for the rest -- the
/// bimorphic splice with more tests. HotSpot's C2 would make a virtual call
/// here; R16Bench `poly` (an `area()` over three final classes) ran 2.9x
/// HotSpot through the inline cache and a real call per iteration.
///
/// **Default OFF** (`CRATONVM_JIT_IR_POLYMORPHIC_GUARDED_SPLICE=1` opts in):
/// unmeasured. Needs the bimorphic splice's own switches. Read per compile
/// and per planned site, never on a hot path.
pub fn ir_polymorphic_guarded_splice_enabled() -> bool {
    ir_bimorphic_guarded_splice_enabled()
}

/// JIT round 19 wave 1 (lane osrpoly19, OSRFOLLOW18-3): the POLYMORPHIC
/// guarded splice of [`ir_polymorphic_guarded_splice_enabled`] for OPTIMIZING
/// OSR-DOOR compiles only (`CompileRequest::osr_mode_exit_polls`), so a flip
/// for polymorphic loops leaves every method-entry compile on its measured
/// default. The planner (`try_compile_inner`) registers polymorphic sites
/// only for a door compile under this switch, and the builder's own check
/// (`IrBuilder::polymorphic_extra_classes`) accepts either switch: a builder
/// without registered sites tests no further class whatever it says.
/// While it is on, `CRATONVM_JIT_OSR_PREFER_SPLICED_SINGLE_PASS` stands aside
/// (`osr_door_lost_polymorphic_splice_pc`), as it does for the process-wide
/// switch: the door's optimizing body splices the site and competes on
/// evidence. **Default OFF** (`=1` opts in). Needs the bimorphic splice's own
/// switches. Read per compile and per planned site.
pub(crate) fn osr_door_ir_polymorphic_splice_enabled() -> bool {
    ir_bimorphic_guarded_splice_enabled()
        && true
}

/// JIT round 19 proposal wave (lane osr19, OSRPOLY19-2 / OSRFOLLOW18-2 of
/// `docs/feature-designs/deopt-osr.md`): `CRATONVM_JIT_IR_CLASS_WORD_CHAIN=1`.
/// A bimorphic or polymorphic guarded splice's test chain
/// ([`IrBuilder::begin_bimorphic_receiver_splice`]) loads the receiver's class
/// word ONCE at its head (`k = Op::ClassWordOf(r)`, `0` for a null or an
/// array) and tests `If(Cmp(Eq, k, Const(id)))` per class, instead of one
/// `ExactClassIs(r, id)` (null test, kind byte, class word) per class. Each
/// compare fuses into its branch by the ordinary compare fusion
/// (`CMP r32/[home], imm32; JE`), so this needs neither
/// `CRATONVM_JIT_IR_FUSED_CLASS_TEST_BRANCH` nor the sink switch to fuse;
/// `CRATONVM_JIT_IR_SINK_CLASS_TEST_TO_BRANCH` sinks these compares too.
///
/// Sound edge by edge: every tested class id is non-zero
/// (`polymorphic_extra_classes` and the planner skip `0`), so
/// `ClassWordOf(r) == id` is `1` exactly when `ExactClassIs(r, id)` is, null
/// and arrays included, and the hit / miss edges carry the same facts.
/// **Default OFF** (an opt-in perf lever for the OSRPOLY19-1 arms). Read once
/// per guarded chain the builder plants, never on a hot path. No `static`.
pub fn ir_class_word_chain_enabled() -> bool {
    true
}

/// JIT round 19 proposal wave 2 (lane osr2, OSR19-2 of
/// `jit-r19-osr19-proposals.md`): `CRATONVM_JIT_IR_CLASS_WORD_SINGLE_TEST=1`.
/// The ONE-class guarded splice's test (call-miss and trap-miss forms) as a
/// class-word compare, `Cmp(Eq, ClassWordOf(r), Const(id))`, instead of
/// `ExactClassIs(r, id)` ([`IrBuilder::one_class_receiver_test`]), so every
/// test of one receiver shares one header read after GVN.
///
/// Its own switch, not folded under `CRATONVM_JIT_IR_CLASS_WORD_CHAIN`, though
/// the semantics are identical (each compare answers what the `ExactClassIs`
/// it replaces answered): the populations differ by an order of magnitude
/// (every monomorphic guarded site against the bi/polymorphic ones), and the
/// chain's A/B (`R19OsrpolyShapes`) must stay readable without the
/// monomorphic sites moving under it. **Default OFF**. Read once per site
/// the builder plants, never on a hot path. No `static`.
pub(crate) fn ir_class_word_single_test_enabled() -> bool {
    false
}

/// Most classes one polymorphic guarded site tests (round 16, lane mega).
/// Every test costs a compare per call that reaches it, and every body code.
pub(crate) const POLYMORPHIC_MAX_CLASSES: usize = 4;

/// Round 16 (lane mega): the three or four classes the receiver profile of
/// the caller's `pc` splits between, best first, or `None`
/// ([`crate::profile::ReceiverEvidence::polymorphic_set`]): the bimorphic
/// reader's bars (evidence, the one-class share, the per-class share), with
/// the pair share asked of the whole set.
pub(crate) fn profile_polymorphic_receiver_classes(
    profile: Option<&crate::profile::MethodProfile>,
    pc: usize,
) -> Option<Vec<u32>> {
    let counts = profile.and_then(|p| p.receivers.get(&pc))?;
    crate::profile::ReceiverEvidence::of(counts).polymorphic_set(
        crate::MIN_GUARDED_RECEIVER_EVIDENCE,
        POLYMORPHIC_MAX_CLASSES,
        PROFILE_GUARDED_MIN_RECEIVER_PCT,
        GUARDED_SPLICE_MIN_RECEIVER_PCT,
        BIMORPHIC_MIN_PAIR_RECEIVER_PCT,
    )
}

/// The share (percent) a site's top receiver class must hold for the MAJOR
/// receiver guarded splice ([`profile_major_receiver_class`]).
pub(crate) const MAJOR_RECEIVER_MIN_PCT: u32 = 50;

/// JIT round 23 wave 7 (lane L1): `CRATONVM_JIT_IR_MAJOR_RECEIVER_GUARDED_SPLICE`
/// (perf, **default ON since j27 2026-10-28**, `=0` kills; was opt-in after the
/// neutral n23-w7 A/B, RxLoop/regex rows within noise). A site whose profile falls between every
/// multi-class bar still gets its top class spliced behind one exact test,
/// with the call as the miss edge. The case is a top class under the
/// one-class share ([`PROFILE_GUARDED_MIN_RECEIVER_PCT`]), a runner-up under
/// [`GUARDED_SPLICE_MIN_RECEIVER_PCT`] or a pair under
/// [`BIMORPHIC_MIN_PAIR_RECEIVER_PCT`], and no polymorphic set.
///
/// The site that measured it is `Pattern$BmpCharPropertyGreedy.match`'s
/// `CharPredicate.is` (pc 26, `R23ArraysEqRegex regex-groups`): `obs=2221
/// types=5 top=76% second=9%`. Every reader declined it, so each character paid
/// a full compiled call into a six-instruction lambda.
///
/// Sound for the reason the one-class guarded splice is: the test is exact and
/// the miss edge is the site's unchanged call. The policy only changes how
/// often the body runs, never which body a class gets. Needs
/// [`ir_profile_guarded_splice_enabled`]. Read per planned site, no `static`.
pub(crate) fn ir_major_receiver_guarded_splice_enabled() -> bool {
    ir_profile_guarded_splice_enabled()
        // WARNING j27-user-flip-20261028 (default ON; =0 restores the old default)
}

/// The top receiver class of the caller's `pc` for a major-receiver guarded
/// splice ([`ir_major_receiver_guarded_splice_enabled`]), or `None`. It is
/// `None` when the switch is off, and when the one-class reader already
/// answers (it owns that site). It is also `None` when the bimorphic pair
/// answers, or the polymorphic set does while `polymorphic_on`, because those
/// splice more classes and are preferred. Otherwise it needs the evidence
/// floor, a top class at [`MAJOR_RECEIVER_MIN_PCT`] or more, and a non-zero
/// class id.
pub(crate) fn profile_major_receiver_class(
    profile: Option<&crate::profile::MethodProfile>,
    pc: usize,
    polymorphic_on: bool,
) -> Option<u32> {
    if !ir_major_receiver_guarded_splice_enabled() {
        return None;
    }
    profile_major_receiver_class_policy(profile, pc, polymorphic_on)
}

/// [`profile_major_receiver_class`] without its switch (the pure policy).
pub(crate) fn profile_major_receiver_class_policy(
    profile: Option<&crate::profile::MethodProfile>,
    pc: usize,
    polymorphic_on: bool,
) -> Option<u32> {
    let counts = profile.and_then(|p| p.receivers.get(&pc))?;
    let evidence = crate::profile::ReceiverEvidence::of(counts);
    if evidence.summary().observations < crate::MIN_GUARDED_RECEIVER_EVIDENCE {
        return None;
    }
    if profile_guarded_receiver_class(profile, pc).is_some()
        || profile_bimorphic_receiver_classes(profile, pc).is_some()
        || (polymorphic_on && profile_polymorphic_receiver_classes(profile, pc).is_some())
    {
        return None;
    }
    evidence
        .seed_a_guarded_cache(MAJOR_RECEIVER_MIN_PCT)
        .filter(|&class_id| class_id != 0)
}

/// Round 14 wave 2 (lane guard, CH5-5 of `jit-r13-chain5-proposals-RETIRED-20260929.md`): a
/// guarded splice whose receiver profile names ONE class may take a deopt as
/// its miss edge ([`IrBuilder::add_guarded_trap_site`]), which makes the
/// receiver exact past the test.
///
/// **Default OFF** (`CRATONVM_JIT_IR_GUARDED_SPLICE_TRAP_MISS=1` opts in): a
/// new speculation whose deopt / de-spec round trip has not been measured.
/// Needs the guarded splice's own switch. Read per compile and per planned
/// site, never on a hot path.
pub(crate) fn ir_guarded_splice_trap_miss_enabled() -> bool {
    ir_guarded_receiver_splice_enabled()
}

/// Round 14 wave 2 (lane guard, CH5-5): does the receiver profile of the
/// caller's `pc` name `class_id` and nothing else, over at least
/// [`crate::MIN_GUARDED_RECEIVER_EVIDENCE`] exact (unsaturated) samples?
/// HotSpot's evidence rule for a trap-on-miss receiver guard
/// ([`crate::profile::ReceiverEvidence::only_class`]). `false` without a
/// profile: a guess here costs a deopt, not a compare.
pub(crate) fn profile_admits_trap_miss(
    profile: Option<&crate::profile::MethodProfile>,
    pc: usize,
    class_id: u32,
) -> bool {
    class_id != 0
        && profile
            .and_then(|p| p.receivers.get(&pc))
            .and_then(|counts| {
                crate::profile::ReceiverEvidence::of(counts)
                    .only_class(crate::MIN_GUARDED_RECEIVER_EVIDENCE)
            })
            == Some(class_id)
}

/// Does `profile` say a guarded splice on `class_id` at the caller's `pc` would
/// almost never hit? Fail-open like [`crate::receiver_profile_rejects_guard`]
/// (no profile, no entry, or fewer than
/// [`crate::MIN_GUARDED_RECEIVER_EVIDENCE`] samples answers `false`), with a
/// share bar suited to a call miss edge rather than a deopt.
pub(crate) fn guarded_receiver_profile_rejects(
    profile: Option<&crate::profile::MethodProfile>,
    pc: usize,
    class_id: u32,
) -> bool {
    let Some(counts) = profile.and_then(|p| p.receivers.get(&pc)) else {
        return false;
    };
    crate::profile::ReceiverEvidence::of(counts).guard_evidence(
        class_id,
        crate::MIN_GUARDED_RECEIVER_EVIDENCE,
        GUARDED_SPLICE_MIN_RECEIVER_PCT,
    ) == crate::profile::GuardEvidence::Rejects
}

/// "No splice to blame" in a [`IrBuilder::set_splice_refusal_sink`] sink
/// (round 12 wave 6, lane iropt5).
pub const SPLICE_REFUSAL_NONE: usize = usize::MAX;

/// The three staged relaxations of the splice replay fence (round 12 wave 5,
/// lane iropt4; page
/// `r12w4-iropt3-hashmap-put-get-blocked-by-the-splice-replay-fence-FIXED-20260926.md`).
/// Each has its own kill switch and each is **default ON**:
///
/// * `forward_branch` -- `CRATONVM_JIT_IR_SPLICE_FENCE_FORWARD_BRANCH=0`;
/// * `null_facts` -- `CRATONVM_JIT_IR_SPLICE_FENCE_NULL_FACTS=0`;
/// * `guarded_call` -- `CRATONVM_JIT_IR_SPLICE_FENCE_GUARDED_CALL=0`.
///
/// All three off is the round-11 fence exactly, in the builder
/// ([`IrBuilder::admits_after_committed_splice_store`]) and in the planner
/// (`lib.rs`, `ir_splice_fence_refuses_site`), which read this one value.
///
/// # The fence, and what each relaxation adds to it
///
/// A trap inside a splice deopts to the OUTERMOST `invoke` with REEXECUTE.
/// Once the outermost splice has made a call the resolver did not splice
/// (`splice_call_committed`), the call has run, and a trap after it would run
/// it again. So from the call to the end of the outermost splice the walk
/// builds only what cannot trap. The fence is positional: it looks at what the
/// walk visits after the call, not at what executes after it. All three
/// relaxations apply only to a fence a CALL raised. A fence a committed STORE
/// raised (`splice_committed_bases` non-empty) keeps the round-11 rule, which
/// `jit/tests/r11_hashmap_splice_trailing_store.rs` pins; the argument below is
/// the same for it (proposal W5-4 in `docs/feature-designs/jit-ir-optimizations.md`).
///
/// **1. Forward branches.** An `if*`, `ifnull`/`ifnonnull`, `goto` or
/// `goto_w` whose target lies AFTER it, while every open splice is walked in
/// pc order (no rotated-loop block walk, [`SpliceFrame::ranges`] empty).
/// Proof: let π be walk order. Inside a pc-ordered body a fall-through and a
/// forward branch increase π; entering a nested splice, returning from one, and
/// joining a multi-return body's exits increase π too, because a nested body is
/// walked right after the `invoke` that enters it and before the instruction
/// it returns to. Only a backward branch decreases π. Take any execution path
/// from the call `c` to an instruction `x` with `π(x) < π(c)`: its first
/// π-decreasing step is a backward branch `b` with `π(b) > π(c)`, walked after
/// the commit and refused. So every instruction that can execute after the
/// call is walked after it and is fenced. The branch itself cannot trap: it is
/// an `Op::If` on an `Op::Cmp` (or nothing, for a `goto`); profile pruning,
/// the only thing that turns a branch into a guard, is off inside a splice
/// ([`IrBuilder::prune_always_taken_branch`]); and a safepoint poll sits on a
/// back edge, never on a forward one.
///
/// **2. Null facts.** A `getfield`/`putfield` base (and, with 3, a call
/// receiver) is also proven non-null when it is
/// * the receiver of an open splice: [`IrBuilder::begin_splice`] either proved
///   it (`definitely_non_null`) or built a null guard at the `invoke` that
///   dominates the whole body (inside an impure splice that guard refuses the
///   build through `splice_guard_seen`, so no graph relies on it); or
/// * `v` on the taken edge of `ifnonnull v` or the fall-through of `ifnull v`.
///   The fact lives only in the region that edge dominates: from the
///   fall-through, or from the target's merge when that edge is the merge's
///   ONLY input, up to the next merge activation, splice entry or splice exit,
///   all of which drop every fact. Inside a splice every other way into a pc is
///   a merge: branch targets are merges, and there are no exception edges (the
///   resolver refuses a callee with an exception table). A value is an SSA
///   node, so every use of `v` in that region sees the tested value.
///
/// A non-null base's null check cannot fire, so such an access is exactly as
/// trap-free as one through a committed base.
///
/// **3. A call after the call.** An `invokevirtual`/`invokespecial` that the
/// builder lowers as a plain `Op::Call` (no splice, no scalar or unbox
/// intrinsic, no caller-held monitor pair, no exact-receiver site), with a
/// receiver proven non-null by the rules above. Its lowering deopts nowhere
/// but in `ir_lower::emit_direct_cross_call`'s receiver `NullCheck`, dead for a
/// non-null receiver: an inline cache's null edge is the slow helper's thrown
/// NPE, and every prefix (`ArrayList`, box hash/equals, `valueOf`) declines to
/// the call, never to a deopt. What remains is the call's own exit: an
/// exception is thrown at the outer `invoke`'s bci and routed through the
/// caller's table (the first call's effects stand once, as in the
/// interpreter), and a callee deopt is serviced at the call site
/// (`jit_service_callee_deopt`). That "a returning or throwing `Op::Call`
/// re-executes nothing" is the premise the round-11 admission of ONE surviving
/// call with a trap-free tail already rests on; its known exceptions re-run the
/// whole method from a sink and are lane replay's
/// (`r12w4-replay2-replay-sinks-residual-internalerror-and-reruns`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SpliceFenceRelax {
    pub(crate) forward_branch: bool,
    pub(crate) null_facts: bool,
    pub(crate) guarded_call: bool,
}

/// Default-on IR expansion for `java/util/ArrayList` / `java/util/List`
/// `get(int)` and `size()` across all collectors and both execution modes.
pub fn ir_arraylist_ir_intrinsics_enabled() -> bool {
    true
}

/// Default-on inline fast-path prefix for `java/util/HashMap.get(Object)` and
/// `java/util/Map.get(Object)` across all collectors and both execution modes.
pub fn ir_hashmap_get_inline_enabled() -> bool {
    true
}
