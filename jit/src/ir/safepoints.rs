#[allow(unused_imports)]
use super::*;

impl UseLists {
    /// Empty, tracking enabled, nothing derived yet.
    pub fn new() -> Self {
        UseLists {
            users: Vec::new(),
            sp_users: Vec::new(),
            tracking: true,
            valid: false,
            epoch: 0,
            owner: 0,
            sp_len: 0,
            dangling: false,
            examined: 0,
            rebuilds: 0,
        }
    }

    /// Record that the lists now describe the graph, as of this thread and
    /// this edge epoch.
    #[inline]
    pub(in crate::ir) fn stamp(&mut self) {
        self.epoch = edge_epoch();
        self.owner = thread_token();
    }

    /// True when nothing has written an edge outside the mutators since the
    /// last [`UseLists::stamp`], on this thread.
    #[inline]
    pub(in crate::ir) fn stamped(&self) -> bool {
        self.owner == thread_token() && self.epoch == edge_epoch()
    }

    /// Drop the derived state (keeps the counters and the master switch).
    pub(in crate::ir) fn discard(&mut self) {
        self.users = Vec::new();
        self.sp_users = Vec::new();
        self.valid = false;
        self.dangling = false;
    }

    /// Record that input slot of `user` names `def`.
    #[inline]
    pub(in crate::ir) fn record_input(&mut self, def: NodeId, user: NodeId, num_nodes: usize) {
        if def == NO_NODE {
            return;
        }
        if (def as usize) < num_nodes {
            self.users[def as usize].push_untracked(user);
        } else {
            self.dangling = true;
        }
    }

    /// Record that a snapshot slot names `def`.
    #[inline]
    pub(in crate::ir) fn record_slot(
        &mut self,
        def: NodeId,
        slot: SafepointSlot,
        num_nodes: usize,
    ) {
        if def == NO_NODE {
            return;
        }
        if (def as usize) < num_nodes {
            self.sp_users[def as usize].push(slot);
        } else {
            self.dangling = true;
        }
    }
}

impl Default for UseLists {
    fn default() -> Self {
        UseLists::new()
    }
}

impl fmt::Debug for UseLists {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UseLists")
            .field("tracking", &self.tracking)
            .field("valid", &self.valid)
            .field("nodes", &self.users.len())
            .finish()
    }
}

// ── Safepoint snapshot list ──────────────────────────────────────────

/// [`Graph::safepoints`]: the snapshot list, behind a guard that keeps the
/// def-use lists honest (round 11 wave 18, lane irback — step 3 of
/// `r11-irback-use-list-invariant-enforced-by-convention-only`).
///
/// A snapshot slot is a node reference exactly like an input edge, and
/// the `sp_users` half of [`UseLists`] records them so [`Graph::replace_all_uses`] can walk
/// them. Until this type the field was a bare `pub Vec`, so code could
/// INSTALL a node id into a slot (`graph.safepoints[i].locals[k] = v`) without
/// `sp_users` hearing of it; a later tracked `replace_all_uses` then missed that
/// slot and the deopt frame kept naming a node DCE was free to kill. Every
/// production installer went through [`Graph::set_safepoint_slot`] by
/// convention only.
///
/// The guard is the one [`Inputs`] already uses: reading derefs to the `Vec`
/// for free, and EVERY mutable path out of this type (`DerefMut`, so `push`,
/// `iter_mut`, `get_mut`, `retain`, `clear` and `x[i].locals[k] = v`, plus
/// `for sp in &mut graph.safepoints`) bumps [`EDGE_EPOCH`], which demotes the
/// next tracked mutation to the full scan that re-derives the lists. An untracked
/// write therefore costs one rebuild and can no longer cost a missed slot.
///
/// The tracked mutators in this file (`push_safepoint`, `set_safepoint_slot`,
/// `replace_all_uses`, `retain_safepoints`, `clear_safepoint_slots_where`,
/// `or_safepoint_monitor_elided_mask`) write `.0` directly: they keep the
/// lists current themselves, and bumping would throw away the state they are
/// about to re-stamp. `.0` is private to this module on purpose.
#[derive(Clone, Default)]
pub struct Safepoints(pub(in crate::ir) Vec<SafepointSnapshot>);

impl Deref for Safepoints {
    type Target = Vec<SafepointSnapshot>;
    #[inline]
    fn deref(&self) -> &Vec<SafepointSnapshot> {
        &self.0
    }
}

impl DerefMut for Safepoints {
    #[inline]
    fn deref_mut(&mut self) -> &mut Vec<SafepointSnapshot> {
        bump_edge_epoch();
        &mut self.0
    }
}

impl<'a> IntoIterator for &'a Safepoints {
    type Item = &'a SafepointSnapshot;
    type IntoIter = std::slice::Iter<'a, SafepointSnapshot>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl<'a> IntoIterator for &'a mut Safepoints {
    type Item = &'a mut SafepointSnapshot;
    type IntoIter = std::slice::IterMut<'a, SafepointSnapshot>;
    fn into_iter(self) -> Self::IntoIter {
        bump_edge_epoch();
        self.0.iter_mut()
    }
}

impl fmt::Debug for Safepoints {
    /// Prints as the `Vec` it replaced, so a `{:?}` of a graph's snapshots
    /// reads the same as before the wrapper.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}

// ── Graph ────────────────────────────────────────────────────────────

/// TLGET21-3: one trap-form `ThreadLocal.get` / `set` expansion
/// ([`Graph::tl_chains`]). Every node the expansion added has an id in
/// `first..end`; it reads `recv` and starts at control `entry_ctrl` and memory
/// `entry_mem`, and the hit (the only way through it: every miss is a `Guard`
/// at `pc`) leaves at control `hit_ctrl` with, for a `get`, `value`, the
/// `Entry.value` read, which is also the memory state after it; for a `set`,
/// `store`, the `e.value = v` store, the last node of the range.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TlChainRecord {
    pub recv: NodeId,
    pub entry_ctrl: NodeId,
    pub entry_mem: NodeId,
    pub hit_ctrl: NodeId,
    /// `get`: the value read; `set`: [`NO_NODE`].
    pub value: NodeId,
    /// `set`: the store, which stays in the loop when the lookup moves;
    /// `get`: [`NO_NODE`].
    pub store: NodeId,
    pub first: NodeId,
    pub end: NodeId,
    /// The invoke's pc (combined, inside a splice).
    pub pc: usize,
}

/// The IR graph — a flat arena of nodes with an entry and exit.
pub struct Graph {
    /// All nodes, indexed by `NodeId`.
    pub nodes: Vec<Node>,
    /// The `Start` node.
    pub entry: NodeId,
    /// The `Return` node (may be a Merge if multiple returns).
    pub exit: NodeId,
    /// Deopt safepoint snapshots, recorded by the builder at bytecode
    /// boundaries and resolved into `DeoptimizationPoint`s by the lowerer.
    /// Empty when the builder did not record any (e.g. hand-built graphs).
    ///
    /// Reads deref to the `Vec`; a write through this field bumps the edge
    /// epoch (see [`Safepoints`]). Install a node reference with
    /// [`Graph::set_safepoint_slot`] / [`Graph::push_safepoint`] to keep the
    /// def-use lists current instead of paying a rebuild.
    pub safepoints: Safepoints,
    /// Incremental def-use edges. A hand-built graph can leave this
    /// `UseLists::default()`: the lists are derived on first demand.
    pub uses: UseLists,
    /// Which `Op::Param` holds the receiver, when this graph is an INSTANCE
    /// method's. `None` means "static, or nobody said" — never "no receiver",
    /// because a hand-built graph leaves it `None` and must not thereby claim
    /// a fact about parameter 0.
    ///
    /// # Why the graph carries this and not the lowerer
    ///
    /// It is a property of the method, and the lowerer already holds `&Graph`.
    /// The alternative was a fifteenth positional argument to `Lowerer::new`,
    /// threaded through `lower` and `lower_inner`, to carry one bit that the
    /// graph builder already had in hand.
    ///
    /// What it licenses is one fact, stated once: `this` is non-null. The JVM
    /// enters an instance method only through a call site that has already
    /// null-checked its receiver, `<init>` included — its receiver is
    /// uninitialized but never null.
    pub receiver_param: Option<u16>,
    /// VM class IDs for `(java/lang/Integer, java/lang/Long)`, if resolved.
    pub box_class_ids: (Option<u32>, Option<u32>),
    /// JVMTI `Exception` / `ExceptionCatch` union flag byte addresses, when
    /// exception events may be armed while this body runs.
    pub exception_event_guard: Option<[usize; 2]>,
    /// Bytecode pcs at which the builder planted an unconditional IR site
    /// trap (`IrBuilder::plant_uncommon_trap`): the whole translation of the
    /// bytecode there is `Op::Guard { bci: pc }` on the constant `0`, so every
    /// compiled arrival at that pc traps (round 12 wave 3, lane replay).
    ///
    /// Read by `ir_lower`'s `Op::Guard` to stamp
    /// `ResumeSemantics::first_arrival` on exactly those guards. Keyed by pc
    /// and not by node, so an optimizer copy of the guard (unrolling, peeling)
    /// keeps the fact, and a guard that merely FOLDED to `0` at the same pc in
    /// a hand-built or rebuilt graph never gains it. Empty (the conservative
    /// answer) for every graph the builder did not produce.
    pub site_trap_pcs: Vec<usize>,
    /// Arm-entry traps hoisted to branch guards (`hoist_arm_entry_traps`). Their
    /// deopt frame belongs only to the failing path; a passing arrival must never
    /// force an exit into them under `CRATONVM_DEOPT_EAGER_GUARDS`.
    pub hoisted_arm_trap_pcs: Vec<usize>,
    /// JIT round 19 wave 3, lane eager3 (EAGER2-4): the cause of every site
    /// trap ([`IrBuilder::plant_uncommon_trap`]) and class-init guard
    /// ([`IrBuilder::plant_class_init_guard`]) the builder planted, by the
    /// pc of its `Op::Guard`. Read by `ir_lower`'s `Op::Guard` arm under
    /// `CRATONVM_JIT_TRAP_CAUSE_ON_POINT`, which records the cause beside the
    /// point it boxes (`CompiledMethod::trap_cause_points`), so the VM's deopt
    /// service names the cause instead of inferring it from the bci. Only pcs
    /// of the compiling method's own code (a trap inside a splice refuses the
    /// graph). Empty for every graph the builder did not produce.
    pub trap_causes: Vec<(usize, TrapCause)>,
    /// This graph emits precise exits for spliced calls to unloaded owners.
    /// OSR entries must import arbitrary interpreter state after such an exit.
    pub unloaded_static_call_traps: bool,
    /// Round nio20 wave 2, lane L3 (proposal L1-2 of
    /// `jit-r20-nio-proposals-L1.md`): the COMBINED pcs of the precise splice
    /// exits for a class that was not loaded at compile time -- the
    /// unloaded-static-call exit and the unloaded-typecheck exit
    /// ([`IrBuilder::plant_unloaded_typecheck_exit`]). `ir_lower`'s
    /// `Op::Guard` stamps `DeoptReason::ClassLoading` on a guard listed here,
    /// which `DeoptHistory::recommend_action` maps to
    /// `RecompileAndReinterpret` on the FIRST trap: the interpreter has loaded
    /// the class by the time the recompile runs, so the arm then splices
    /// instead of trapping per call until the count policy escalates. Keyed by
    /// pc like [`Self::site_trap_pcs`] (an optimizer copy keeps the fact).
    /// Kill switch `CRATONVM_JIT_IR_CLASS_LOADING_SPLICE_EXITS=0`.
    pub class_loading_trap_pcs: Vec<usize>,
    /// JIT r20 wave 11, lane pProf (W17L4-5): the pcs of the never-taken
    /// prune guards ([`IrBuilder::prune_never_taken_branch`]). `ir_lower`'s
    /// `Op::Guard` stamps `DeoptReason::UnstableIf` on a guard listed here
    /// (outside a splice, where the prune never runs), which recompiles on
    /// the first trap and is withdrawn at its bci after one charge. Keyed by
    /// pc like [`Self::class_loading_trap_pcs`] (an unrolled copy keeps the
    /// fact). Empty under `CRATONVM_JIT_PROF_UNSTABLE_IF_REASON=0`.
    pub unstable_if_trap_pcs: Vec<usize>,
    /// Round 13 wave 3 (lane framestate): the compiling method's
    /// `"class/Name.method:descriptor"` key, set by `lib.rs`'s IR inline
    /// planner when it splices anything. The OUTERMOST scope of an inlined
    /// deopt chain must name the method the artifact belongs to (the VM's
    /// `materialise_inlined_chain` checks it), and a chain is the one frame
    /// state whose outer key the install-time stamp
    /// (`CompiledMethod::stamp_deopt_method_key`, top-level keys only) cannot
    /// reach. Empty (the default) makes `ir_lower` build no chain at all.
    pub scope_method_key: String,
    /// JIT round 23 L8 close: `Class.method(desc)` of the compile, for
    /// `CRATONVM_DBG_JITC` lines only (background compiles interleave theirs,
    /// so an unnamed line cannot be told apart). Set by the IR door when the
    /// debug flag is on; empty otherwise. Nothing reads it for codegen.
    pub dbg_label: String,
    /// Interpreter round i5 wave 102, lane L2: the compiling method's
    /// `implicit_null::method_stamp_token`, packed beside every throw-site
    /// stamp the lowering emits (`CRATONVM_JIT_THROW_STAMP_OWNER`). `0` (the
    /// default, and every graph a test builds) packs nothing.
    pub throw_stamp_token: u32,
    /// Round 13 wave 3 (lane framestate, W5-3 stage 3): the builder admitted
    /// something into a spliced body ONLY because a trap there resumes through
    /// a caller chain ([`ir_splice_chain_fences_enabled`]): a trapping
    /// instruction after a committed store or call, a store into a
    /// caller-visible array, a guard in an impure body. The flat frame at the
    /// outermost `invoke` would then re-run what the body committed, so every
    /// guard in a spliced body MUST publish its chain: `ir_lower` refuses the
    /// compile when one cannot, escape analysis keeps every object a chain
    /// snapshot names, and LICM hoists no spliced trapping node. `false`
    /// everywhere else, which leaves the chain an optional refinement.
    pub splice_chains_required: bool,
    /// JIT round 23 wave 2, lane L2 (INLINE22-4 stage 2): the combined-buffer
    /// `base` of every OPAQUE lambda SAM forwarder the builder opened
    /// (`SpliceFrame::opaque`): no chain is recorded anywhere under one, so a
    /// guard in its subtree resumes at the outermost `invoke` even under
    /// [`Self::splice_chains_required`]
    /// ([`IrInlineFrameSites::pc_resumes_flat_under_forwarder`]).
    pub opaque_forwarder_bases: Vec<usize>,
    /// JIT round 23 wave 3b: the builder's G14-1 field-memo census at the end
    /// of the build, `(recorded, hits, forwarded, cleared at a join, cleared
    /// by an opaque node)`, so a test can print it. Diagnostic only.
    pub field_memo_census: (usize, usize, usize, usize, usize),
    /// The graph is built for an OSR request (`CompileRequest::osr_requested_bci`).
    /// Escape analysis keeps a loop-carried object allocated in such a body
    /// when `CRATONVM_JIT_OSR_GUARD_EXIT_VIRTUALS=0`: the OSR door resumes a
    /// guard exit in place, and that transfer then re-allocates no
    /// scalar-replaced object (`osr_exit::guard_exit_fact`).
    pub osr_compile: bool,
    pub(crate) vthread_boundaries: Option<Vec<u32>>,
    /// JIT round 19 wave 12 (lane hoist12): `ir_optimize::optimize_with` sets
    /// this to its context's `osr_route` (the optimizing OSR route's compile,
    /// entered at a loop header past every pre-header) for the passes deep
    /// inside it that have no context: the direct-static LICM arm under
    /// `CRATONVM_JIT_IR_LICM_DIRECT_STATIC_OSR_ONLY` (HOIST11-3) and the loop
    /// static-read hoist under `CRATONVM_JIT_IR_LOOP_STATIC_HOIST_OSR_ROUTE`
    /// (HOIST11-4). `false` for every graph `optimize_with` never ran on.
    pub optimize_osr_route: bool,
    /// `CRATONVM_JIT_IR_LATE_LICM`: set by `ir_optimize::optimize_with` around its
    /// late LICM round only, which lets the read hoist step over pre-header
    /// guards that resume at the loop's own header (the loop-entry predicates).
    pub late_licm: bool,
    /// JIT round 23 wave 3b (lane L1, L1W3-1; `CRATONVM_JIT_IR_LOOP_CARRIED_REGS`):
    /// the production pipeline (`lib.rs`, just before scheduling) asks the
    /// scheduler and the lowerer to keep a loop's carried integer values in
    /// registers across the back edge: the scheduler sinks a back-edge source
    /// to the latch at equal loop depth (integers only), and the lowerer gives
    /// it its phi's register (`coalesce_back_edge_phi_sources`). An explicit
    /// value of either underlying switch (`CRATONVM_JIT_IR_SINK_EQUAL_DEPTH`,
    /// `CRATONVM_JIT_IR_PHI_COALESCE_BACK_EDGE`) wins over this. `false` for
    /// every graph a test builds, so fixtures keep the historical schedule.
    pub loop_carried_regs: bool,
    /// JIT r25 wave 10 (W10-2b, `CRATONVM_JIT_IR_CONTEXT_ABI_OF_BASELINE`,
    /// default ON since W13-2, `=0` kills): the lowering reserves the VM-context hidden argument even when
    /// the optimized graph no longer needs it, because the method's single-pass
    /// body takes one (its bytecode scan's `needs_heap`). Then the two bodies
    /// share an ABI and a C2 supersede can forward the baked callers of the C1
    /// body (`not_entrant::supersede_forward_admissible`). `false` for every
    /// graph a test builds.
    pub context_abi_floor: bool,
    /// JIT round 23 wave 3d (lane L1, proxy proposal 4e;
    /// `CRATONVM_JIT_IR_DIRECT_INSTANCE_SELF_CALL`): `(IR row, VM row)` for each
    /// receiver-carrying direct self-call. The IR row is the call's kind-4
    /// `JitInvokeInfo` (what `Op::Call` bakes and every kind-4 reader in the
    /// lowerer asks); the VM row is its kind-1 twin, the one the callee-deopt
    /// service is handed, because the VM knows kind 4 only as the receiver-less
    /// static self-call. A receiver-carrying kind-4 call with no row here is
    /// refused by the lowerer. Empty for every graph a test builds.
    pub self_call_service_infos: Vec<(usize, usize)>,
    /// Round 15 wave 3 (lane chain2, CHN15-2): the DECLARING class id of the
    /// resolved field a builder `getfield` / `putfield` node (`Op::Load` /
    /// `Op::Store`) accesses, keyed by that node, filled from the rows
    /// [`IrBuilder::set_field_decl_classes`] was handed. A node with no row
    /// (a pass's clone, an intrinsic's load, a hand-built graph) has no
    /// identity and keeps the index-only alias rule.
    pub field_decl_class: HashMap<NodeId, u32>,
    /// Round 15 wave 3 (lane chain2, CHN15-2): may [`Graph::memory_effect`]
    /// put [`Self::field_decl_class`] into a field's [`AliasClass::Field`]
    /// (`CRATONVM_JIT_IR_FIELD_IDENTITY_ALIAS`, read once per build)? `false`
    /// answers exactly as before; the table is still kept for the census.
    pub field_identity_alias: bool,
    /// Round nio20 wave 4 (lane L1, W3L4-1): may [`Graph::memory_effect`]
    /// classify a `ScopedMemoryAccess` fold node as [`AliasClass::Raw`]
    /// ([`scoped_raw_effect`]) instead of `Opaque`? Set by [`IrBuilder::new`]
    /// from `CRATONVM_JIT_IR_SCOPED_RAW_ALIAS` (default ON); `false` for every
    /// hand-built graph ([`Graph::empty`]), which keeps the old answer.
    pub scoped_raw_alias: bool,
    /// Round nio20 wave 6 (lane L1): the `(class_id, field_index)` statics
    /// whose plain `getstatic` this compile lowers as the DIRECT read
    /// (`ir_lower::Lowerer::emit_inline_getstatic`: no call, no `<clinit>`,
    /// no frame), fed by `lib.rs` through [`IrBuilder::add_plain_static_reads`]
    /// under `CRATONVM_JIT_IR_PLAIN_STATIC_READ` (default ON). For such a
    /// non-volatile `Op::LoadStatic`, [`Graph::memory_effect`] answers "reads
    /// [`AliasClass::Static`], writes nothing, plain, no safepoint" instead of
    /// the op-level `Opaque` ([`plain_static_read_effect`]). Empty for every
    /// graph nobody feeds, which keeps the old answer.
    pub plain_static_reads: HashSet<(u32, u32)>,
    /// JIT round 20 wave 12 (lane qLoop, W4L3-6): the class ids of the `new`
    /// sites this compile resolved with NO finalizer (`JitNewSite::Resolved`'s
    /// `has_finalizer == false`), fed by `lib.rs` through
    /// [`IrBuilder::note_finalizer_free_new_classes`]. Only the escape-analysis
    /// bridge reads it: a `reachabilityFence` of an `Op::New` of such a class
    /// is a non-escaping use (`escape_analysis::Op::KeepAlive`). Empty (the
    /// default) keeps every fence an escaping call.
    pub finalizer_free_new_classes: HashSet<u32>,
    /// JIT round 20 wave 12 (lane hFin): the class ids of the `new` sites this
    /// compile resolved WITH `has_finalizer` (`JitNewSite::Resolved` /
    /// `JitUninitNewGuardSite`), fed by `lib.rs` through
    /// [`IrBuilder::note_finalizable_new_classes`]. The VM's answer is `true`
    /// for a class that overrides `finalize()`, for every `java.lang.ref`
    /// reference class (`jit_new_site_flags`: discovery is registered at the
    /// allocation), and conservatively for a class not initialised yet. An
    /// `Op::New` of such a class must stay a real allocation: the
    /// escape-analysis bridge maps it to an escaping `EaOp::Call`
    /// (`ea_ir_bridge::escape_analysis_from_ir`), as HotSpot's C2 marks an
    /// allocation of a finalizable or `Reference` class `GlobalEscape`. A
    /// spliced constructor does not ask `is_elidable_construction`, so this is
    /// the screen that covers it. Empty (the default) changes nothing.
    pub finalizable_new_classes: HashSet<u32>,
    /// nio21 wave 7 (lane L4, W5L4-3): nodes whose value is a `byte[]` or
    /// null -- the loads (and memo-forwarded values) of a field the VM proved
    /// `byte[]`-typed (`IrBuilder::add_byte_array_field_pcs`, today
    /// `ByteBuffer.hb`). `ir_lower`'s scoped fold skips the primitive-array
    /// screen and the element-size read for such a base
    /// (`CRATONVM_JIT_IR_SCOPED_BYTE_ARRAY_BASE`); its null test and bounds
    /// compares stay. Node ids are never renumbered, so the set stays true of
    /// its nodes through every pass; a node a pass replaces is simply not in
    /// it. Empty for every graph nobody feeds, which claims nothing.
    pub byte_array_nodes: HashSet<NodeId>,
    /// JIT r23 wave 8 (lane W8-L4, `CRATONVM_JIT_IR_FFM_FIELD_HOIST`): the
    /// guarded segment-field reads (`Op::Unbox` `FieldLong` / `FieldBool` /
    /// `FieldRef`) the FFM element inline expansion emitted
    /// (`IrBuilder::try_emit_ffm_element_inline`). Each reads a FINAL field of
    /// a JDK `jdk.internal.foreign` segment class (`length`, `readOnly`,
    /// `scope`, `offset`, `base`, `min`), which is what lets
    /// `ir_optimize::ffm_field_hoist` read it once per loop entry. Empty for
    /// every graph without the expansion; a replaced node simply leaves it.
    pub ffm_final_field_reads: HashSet<NodeId>,
    /// Regular Java `final` field `Op::Load` nodes (`getfield` of a `final`
    /// field or `String` `coder`/`value` intrinsic), hoisted in read-only loops
    /// by `w7_trap_free_preheader`. Kept separate from `ffm_final_field_reads`.
    pub final_java_field_reads: HashSet<NodeId>,
    /// JIT r23 wave 8 (lane W8-L4, `CRATONVM_JIT_IR_FFM_HEAP_ACCESS_PROVEN`):
    /// the FFM element inline expansion's scoped accesses over a HEAP segment
    /// (`Op::Unbox` `ScopedGet` / `ScopedPut`). For each, the expansion's own
    /// nodes prove what `ir_lower`'s scoped heap arm would otherwise test per
    /// access:
    ///
    /// * the receiver (`sma`) is non-null: the segment-field reads on it, which
    ///   null-test it and deopt, dominate the access (in its block, or in the
    ///   loop's pre-header after `ffm_field_hoist`);
    /// * the base is a non-null one-dimensional primitive array, and
    ///   `[addr - 16, addr - 16 + w)` lies in its payload: the reads guard the
    ///   receiver's EXACT class `HeapMemorySegmentImpl$OfX`, which the JDK
    ///   builds only over a non-null array (`SegmentFactories.fromArray`,
    ///   `ofBuffer` by the base's runtime type; `dup` keeps it), with
    ///   `offset >= Unsafe.arrayBaseOffset (16)` and `offset - 16 + length <=`
    ///   the payload's byte size (checked at construction and by every slice);
    ///   `addr = offset + off` behind the guards `0 <= off <= length - w`.
    ///
    /// Node ids are never renumbered; a copy a pass makes is simply not in it
    /// (and keeps every test). Empty for every graph without the expansion.
    pub ffm_proven_heap_accesses: HashSet<NodeId>,
    /// JIT round 24 B1 (`CRATONVM_JIT_IR_VECTOR_SCOPED_STORE`): for each
    /// scoped access the FFM element expansion built, the segment's length
    /// field as `(class_id, FieldLong(slot))` -- the read its bounds guards
    /// compare against. Kept as the field, not the read's node: the
    /// segment-field hoist replaces that read with a pre-header copy, and a
    /// final field read anywhere on the same receiver answers the same value.
    /// The vector pre-loop (`ir_vector_loop`) bounds a store loop with it.
    pub ffm_scoped_length_fields: HashMap<NodeId, (u32, UnboxOp)>,
    /// The class diamonds emitted for two planned segment shapes.
    pub ffm_bimorphic_tests: HashSet<NodeId>,
    /// A versioned field read may move to its own header's entry edge.
    pub ffm_versioned_field_headers: HashMap<NodeId, NodeId>,
    /// JIT round 24 B2 (`CRATONVM_JIT_IR_FFM_ARENA_CACHE`): the native scoped
    /// accesses whose LAST [`SCOPED_ARENA_CACHE_INPUTS`] inputs are `[entry,
    /// key, base, delta]` -- the loop's [`UnboxOp::ArenaCacheEntry`], `min &
    /// !(2^30 - 1)`, its [`UnboxOp::ArenaCacheBase`] and `addr - min`.
    /// `ir_lower`'s scoped arm then tries `[base + delta]` behind one key
    /// compare first and keeps its full path for a miss. Built only by
    /// `ir_optimize::ffm_arena_cache`.
    pub ffm_arena_cached: HashSet<NodeId>,
    /// JIT r23 wave 9 (lane L4, `CRATONVM_JIT_IR_FFM_HEAP_SESSION_HOIST`): the
    /// `scope` reads (`Op::Unbox` `FieldRef`) the FFM element expansion emitted
    /// for a HEAP segment class. `ir_optimize::ffm_field_hoist` carries the
    /// mark to the pre-header read it plants. Empty without the expansion.
    pub ffm_heap_scope_reads: HashSet<NodeId>,
    /// JIT r23 wave 9 (lane L4): the class id of
    /// `jdk.internal.foreign.GlobalSession$HeapSession` (the scope of
    /// `MemorySegment.ofArray`), as the VM resolves it for this compile
    /// (`ir_lower::scoped_heap_session_class_id`); `0` = unknown, nothing
    /// planted. Set by `IrBuilder::set_ffm_heap_session_class`.
    pub ffm_heap_session_class: u32,
    /// JIT r23 wave 9 (lane L4): pre-header `scope` reads whose class a
    /// dominating loop-entry guard proved to be `ffm_heap_session_class` --
    /// a session that never closes (`checkValidStateRaw` never fails), so
    /// `ir_lower`'s scoped access whose session input is one of these skips
    /// its session check entirely. Planted only by
    /// `ir_optimize::ffm_field_hoist`, which places the guard in the same
    /// pre-header after the read and before every use (all of them in the
    /// loop it entered).
    pub ffm_never_closing_scopes: HashSet<NodeId>,
    /// JIT r23 wave 9 (lane L4): the loop-entry guards that prove a
    /// [`Self::ffm_never_closing_scopes`] read; each deopts to its loop's
    /// entry (`ffm_field_hoist::resumes_at_its_loop_entry`).
    pub ffm_session_guards: HashSet<NodeId>,
    /// JIT round 23 close (`CRATONVM_JIT_IR_FFM_SESSION_PRECHECK`): the
    /// `scope` reads the FFM element expansion emitted for a NATIVE segment
    /// class. `ir_optimize::ffm_field_hoist` carries the mark to the
    /// pre-header read it plants. Empty without the expansion.
    pub ffm_native_scope_reads: HashSet<NodeId>,
    /// JIT round 23 close: the loop-entry [`UnboxOp::SessionPrecheck`] nodes
    /// `ir_optimize::ffm_field_hoist` planted. None of them can trap, so a
    /// pre-header holding one is not a trapping pre-header
    /// (`ffm_field_hoist::resumes_at_its_loop_entry`).
    pub ffm_session_prechecks: HashSet<NodeId>,
    /// Round 14 wave 2 (lane syncsplice, SR-1): the monitor window of every
    /// synchronized splice the builder emitted, which no frame state names.
    /// `ir_lower` refuses the compile unless nothing that can trap is
    /// scheduled inside one (see [`SyncSpliceWindow`]). Empty for every graph
    /// without one.
    pub sync_splice_windows: Vec<SyncSpliceWindow>,
    /// j27 wave 4 (lane MONROWS, SY16-3): the `MonitorEnter` / `MonitorExit`
    /// of every caller-held synchronized direct CALL the builder emitted
    /// (`enter, Call, exit` at one invoke pc; `Graph::note_sync_call_pair`).
    /// `ir_optimize`'s lock coarsening merges such a half only with another
    /// such half, never with a bytecode block's, which `ir_lower`'s
    /// `sync_direct_call_monitor` could not pair and refused the compile for.
    /// Empty for every graph without one.
    pub sync_call_monitor_ops: Vec<NodeId>,
    /// j27 wave 4 (lane MONROWS, MON17W2-2; under the opt-in
    /// `CRATONVM_JIT_IR_LOOP_LOCK_COARSEN_DOMINATED_FIELD_BASE`): `(base,
    /// proof)` -- a coarsened field monitor's copy-0 read `proof` of
    /// `base.lock`, which cannot complete with `base` null. `ir_lower` seeds
    /// `base` non-null in every block `proof`'s block strictly dominates
    /// (`ir_optimize::monitor_rows::dominated_monitor_bases`). Empty for every
    /// graph without one.
    pub monitor_base_proofs: Vec<(NodeId, NodeId)>,
    /// JIT round 20 (lane strings, page
    /// `r21-charat-trap-reads-a-length-predicate-class-guard`): the header bcis
    /// of the loops `ir_optimize::class_predicate` planted a loop-entry class
    /// guard for, AFTER `charat_trap::class_predicate_withheld`'s frame and
    /// de-spec screen admitted the loop. The `charAt` trap's licence
    /// (`charat_trap::seq_charat_traps_on_miss`) is a `Guard{bci: header}
    /// (ExactClassIs(..))` on the entry control of a loop whose header is in
    /// this set, so a pass that plants the same guard SHAPE without the screen
    /// (`plant_length_predicates`' invariant-guard move) cannot license a trap.
    /// Only ever grows during one compile (a bci, not a node id, so a pass
    /// that replaces the guard node keeps the answer the field forward relied
    /// on). Empty for every graph `class_predicate` planted nothing in.
    pub seq_charat_trap_headers: HashSet<usize>,
    /// JIT round 25 wave 7 (lane W7-D, W6C-P4;
    /// `CRATONVM_JIT_IR_SEQ_CHARAT_PREDICATE_PROFILE`): receiver counts by
    /// call pc, read by `ir_optimize`'s `seq_charat_predicate` to plant its
    /// loop-entry `String` guard only behind a monomorphic `String` profile.
    /// A spliced site's row is its CALLEE's own counts re-keyed at the combined
    /// pc ([`IrInlineTables::spliced_receiver_seeds`], merged by
    /// [`IrBuilder::apply_inline_tables`]); the compiling method's own sites
    /// are filled from its profile before `optimize`. Data only: nothing else
    /// reads it, so an empty map changes nothing.
    pub seq_charat_receiver_rows: HashMap<usize, crate::profile::ReceiverCounts>,
    /// Round 14 wave 5 (lane sync5, SS8-3): which `Op::Param` holds the
    /// object whose monitor EVERY execution of this graph runs with, held by
    /// someone other than the graph -- set by `lib.rs` only for an
    /// `ACC_SYNCHRONIZED` instance method's own compile (parameter 0), whose
    /// optimizing body is entered only by a door that holds the receiver's
    /// monitor (`CompiledMethod::requires_wrapped_entry`) or by an OSR
    /// transfer from an interpreter frame that holds it. A parameter INDEX,
    /// like [`Self::receiver_param`]: the node is whichever `Op::Param`
    /// carries it, so a later store to local 0 cannot move the fact. The
    /// graph itself can never release that hold (`monitorexit` must match the
    /// innermost BYTECODE monitor or the build is refused). Licenses
    /// `Thread.holdsLock(this)` -> `1` ([`IrBuilder::try_fold_holds_lock`])
    /// and deleting a synchronized splice's recursive pair on it
    /// (`ir_optimize::elide_region_nested_sync_windows`). `None` (every
    /// hand-built graph, and `CRATONVM_JIT_IR_SYNC_METHOD_MONITOR_FACTS=0`)
    /// claims nothing.
    ///
    /// Round 15 wave 1 (lane sync, SS8-2): a SELF-LOCKING body
    /// ([`Self::method_self_lock`]) takes that hold itself, on the entry edge
    /// ahead of every bytecode, and releases it only as it leaves, so the
    /// facts this licenses hold at every node they are asked of there too.
    pub method_monitor_param: Option<u16>,
    /// Round 14 wave 7 (lane sync7, proposal RV6-1): the `static
    /// synchronized` twin of [`Self::method_monitor_param`] -- the class id
    /// whose `Class` mirror EVERY execution of this graph runs holding, set by
    /// `lib.rs` only for an `ACC_SYNCHRONIZED | ACC_STATIC` method's own compile
    /// (its declaring class, JVMS 2.11.10: the door's wrapped entry, a
    /// caller-held CALL, or the interpreter frame an OSR transfer leaves all
    /// hold it). Licenses deleting a `static synchronized` splice window whose
    /// `SyncSpliceWindow::static_class_id` is this class
    /// (`ir_optimize::elide_region_nested_sync_windows`) and, with the
    /// builder's proven constant-pool entries
    /// ([`IrBuilder::set_method_monitor_class_cps`]), folding
    /// `Thread.holdsLock(C.class)`. `None` (every instance or unsynchronized
    /// method, every hand-built graph, and
    /// `CRATONVM_JIT_IR_SYNC_STATIC_METHOD_MONITOR_FACTS=0`) claims nothing.
    pub method_monitor_class: Option<u32>,
    /// JIT round 25 item 2 (`CRATONVM_JIT_IR_SHADOW_LIVE_RECEIVER`): whether
    /// the compiled method is `ACC_SYNCHRONIZED` -- every publication of such
    /// a body stamps `CompiledMethod::requires_wrapped_entry` (or self-locks),
    /// so `Some(false)` also says "no wrapped entry". Set by `lib.rs` from the
    /// method's own access flags, independent of every monitor-fact switch.
    /// `None` (every hand-built graph) is "unknown", which admits nothing.
    pub method_synchronized: Option<bool>,
    /// JIT round 20 (lane qMisc, correctness;
    /// `jr20-getclass-name-only-predicate-misses-a-private-getclass-FIXED-20261012.md`):
    /// the `JitInvokeInfo` addresses of this method's own `getClass()`-named
    /// call sites that do NOT resolve to `Object.getClass` (a class's own
    /// private or static `getClass()`; `lib.rs` `object_get_class_site_resolved`).
    /// `ir_lower`'s `getClass()` prefix refuses them. A spliced body never
    /// carries one as a virtual row (the VM's splice resolver refuses such a
    /// body, `jit_bridge` `getclass-site-not-object-getclass`).
    pub getclass_prefix_refused_infos: HashSet<usize>,
    /// Interpreter round i1 wave 50, lane L1: `(info_ptr, cell)` of every
    /// `Op::Call` the builder made for a call site of THIS method (never of a
    /// spliced body) that the compile door found unresolved
    /// (`crate::JitFirstCallCell`). `ir_lower`'s `Op::Call` emits the cell's
    /// one-shot first-execution guard in front of the call's whole lowering.
    /// Keyed by the call's info, so an optimizer copy of the call guards the
    /// same cell. Empty for every graph without one.
    pub first_call_cells: Vec<(usize, usize)>,
    /// `(bytecode pc, op, cell)` of every `Op::ScalarIntrinsic` built for such
    /// a site ([`scalar_intrinsic_first_call_enabled`], the r21 vector
    /// residual round): `ir_lower` emits the same one-shot guard in front of
    /// the arithmetic, which has no call to ride on. Keyed by pc and op, so an
    /// optimizer copy of the node (an unrolled body) guards the same cell.
    pub scalar_first_call_cells: Vec<(usize, ScalarOp, usize)>,
    /// Native thin-call context overrides keyed by owned invoke info. A copied
    /// Call retains its original judged descriptor. The artifact owns every
    /// VarHandleJudgedSite whose tagged address is stored here.
    pub native_call_contexts: HashMap<usize, usize>,
    /// Round 15 wave 1 (lane sync, SS8-2): the monitor ops of a SELF-LOCKING
    /// optimizing body ([`MethodSelfLock`]); `None` for every other graph,
    /// whose synchronized method monitor (if any) is the door's.
    pub method_self_lock: Option<MethodSelfLock>,
    /// Round 16 wave 16 (lane monitor4, MON3-1): the `(class_id,
    /// field_index)` statics whose `getstatic` this compile lowers as the
    /// direct read (`DirectHelperTable::resolve_static_base` accepted the
    /// field: its class is initialised, so the read runs no `<clinit>` and
    /// makes no call). Fed by `lib.rs` only while
    /// `CRATONVM_JIT_IR_LOOP_LOCK_COARSEN_BLOCKS` (or `CRATONVM_DBG_JITC`) is
    /// on, and read only by
    /// `ir_optimize`'s loop block coarsening, whose monitor read is repeated
    /// inside the coarsened hold. Empty for every other graph, which claims
    /// nothing.
    pub direct_static_fields: Vec<(u32, u32)>,
    /// JIT round 19 wave 2 (lane irops2, IROPS19-1): may
    /// [`Graph::memory_effect`] classify a DIRECT static access
    /// ([`Graph::direct_static_access`]) as a plain read / write of its
    /// [`AliasClass::Static`] cell instead of `MemEffect::OPAQUE`?
    /// `CRATONVM_JIT_IR_STATIC_ALIAS_CLASS` (default on), set by `lib.rs` together
    /// with [`Self::direct_static_fields`]. `false` answers exactly as before.
    pub static_alias_class: bool,
    /// JIT round 19 wave 4 (lane irops3, IROPS2-5): may
    /// [`Graph::memory_effect`] also classify a non-volatile REFERENCE
    /// `putstatic` of a field in [`Self::direct_static_fields`] (an initialised
    /// class) as a write of its own [`AliasClass::Static`] cell plus a
    /// safepoint ([`Graph::direct_static_ref_store`])? Meaningful only with
    /// [`Self::static_alias_class`]; `lib.rs` sets both once per compile
    /// (`CRATONVM_JIT_IR_STATIC_REF_STORE_EFFECT`, opt-in). `false` answers
    /// exactly as before.
    pub static_ref_store_effect: bool,
    /// JIT round 19 wave 2 (lane irops2, IROPS19-7): the direct static reads
    /// `ir_optimize`'s direct-static LICM hoist moved to a loop pre-header
    /// (`CRATONVM_JIT_IR_LICM_DIRECT_STATIC`). Nothing between that pre-header
    /// and any point inside the loop writes the read's cell (the hoist's
    /// precondition), so an OSR entry inside the loop may re-read the cell
    /// instead of refusing the entry (`ir_lower::osr_remat_compile`, under
    /// `CRATONVM_JIT_IR_OSR_REMAT_STATIC_READ`). Empty for every other graph.
    pub licm_hoisted_static_reads: Vec<NodeId>,
    /// Round 17 wave 2 (lane monitor2, MON3-3 / MON4-3 / MON5-4): the
    /// `(class_id, field_index)` statics the compile's feed reports as
    /// `static final` reference fields that may be trusted as constants: the
    /// declaring class fully initialised at compile time, the field not one
    /// `System.setIn/setOut/setErr` rewrites (`java/lang/System` is never
    /// listed), and only the reference itself trusted (never the elements of
    /// an array it holds). Read only by `ir_optimize`'s loop block coarsening,
    /// under `CRATONVM_JIT_IR_LOOP_LOCK_TRUST_STATIC_FINAL`, and only for a
    /// field that is also in [`Self::direct_static_fields`]. Empty for every
    /// graph whose producer does not feed it, which trusts nothing.
    pub trusted_final_static_fields: Vec<(u32, u32)>,
    /// JIT round 20 wave 11 (lane pIR; L3W6-2 / W6L4-5): `((class_id,
    /// field_index), length)` of each trusted `static final` array field this
    /// graph reads with a non-volatile [`Op::LoadStatic`], as the VM filed it
    /// when the declaring class finished initialising
    /// (`CompileRequest::static_final_array_length_resolver`). Read only by
    /// `ir_optimize::fold_trusted_static_array_lengths`, under
    /// `CRATONVM_JIT_IR_STATIC_FINAL_ARRAY_LENGTH`. Empty for every graph
    /// whose producer does not feed it, which folds nothing.
    pub trusted_static_array_lengths: Vec<((u32, u32), i32)>,
    /// Metadata recorded for `Op::IndyRootRef { slot_addr }` nodes (non-null,
    /// exact class id, array length, originating static field).
    pub const_ref_meta: HashMap<usize, ConstRefMeta>,
    /// `CRATONVM_FROZEN_PAGES`: the folds this build wanted and could not
    /// make (`IrBuilder::fold_frozen_field`), for the recompile the VM asks
    /// once they are available.
    pub frozen_misses: Vec<crate::frozen_facts::FrozenMiss>,
    /// `CRATONVM_FROZEN_PAGES`: synthetic CHA class dependencies
    /// (`cratonvm/cha_field/<owner>`) recorded in `CompiledMethod::inlined_methods`.
    pub frozen_cha_deps: Vec<String>,
    /// Per-compile CHA leaf class IDs (non-final classes with zero loaded
    /// subclasses at compile time, guarded by `cratonvm/cha_leaf/` deps).
    pub cha_leaf_classes: rustc_hash::FxHashSet<u32>,
    /// Count of singleton receiver-identity guards planted in this graph.
    pub frozen_identity_guards: u64,
    /// Exact classes whose frozen fields/elements folded or were identity-guarded
    /// during this build (`FrozenFactsState::record_compile`).
    pub frozen_folded_classes: Vec<u32>,
    /// Exact receiver classes whose identity-guard folding failed or was
    /// rejected during this build (`FrozenFactsState::record_compile`).
    pub frozen_failed_classes: Vec<u32>,
    /// JIT round 17 wave 3 (lane mh4; MHCONST17-2): the `invokevirtual
    /// MethodHandle.invokeExact` / `invoke` pcs the builder built as a call
    /// of a constant handle's member (`IrBuilder::mh_constant_opcode`), the
    /// handle dropped from the stack with the call's arguments. The
    /// instruction's snapshot still holds the handle below the row's
    /// arguments, so `ir_lower`'s post-call exit, which pops the row's count,
    /// refuses each of them. Empty for every graph without such a site.
    pub mh_constant_call_pcs: Vec<usize>,
    /// j27 wave 2 (lane GCMAP; `j27-mhfilter-pre-call-frame-bci-is-the-getstatic-20261028.md`):
    /// `(row pc, reported bci)` of each `Op::Call` whose frame must report a
    /// bci other than the pc its by-pc rows are keyed by -- a constant filter
    /// species' PRE call, built at the site's `getstatic` pc, reports the
    /// `invokeExact`'s bci as HotSpot does. Read through
    /// [`Graph::reported_call_bci`]. Empty for every graph without such a site.
    pub call_reported_bcis: Vec<(usize, usize)>,
    /// JIT round 21 wave 17 (lane records, IN16-6): the `invokedynamic` pcs
    /// [`IrBuilder::try_indy_record_fold`] built as a field-wise fold. Each
    /// carries one `Objects.hashCode` / `Objects.equals` call per reference
    /// component, none of which is the instruction's result, so `ir_lower`'s
    /// post-call exit (which resumes after the instruction with the call's
    /// value pushed) refuses every call at them. Empty for every graph
    /// without such a site.
    pub record_fold_call_pcs: Vec<usize>,
    /// JIT round 21 wave 20 (lane sbfuse, STRINTR21-4): the concat bridge
    /// sites of the `Op::IndyBridge` nodes [`IrBuilder::sb_chain_fuse_step`]
    /// built for a fused `StringBuilder` chain. `ir_lower` lowers each inline
    /// whatever `CRATONVM_JIT_IR_CONCAT_INLINE` says (the fusion's switch,
    /// `CRATONVM_JIT_IR_SB_CHAIN_FUSE`, decided it). Empty for every graph
    /// without such a chain.
    pub sb_chain_fused_sites: Vec<usize>,
    /// JIT round 21 wave 15 (lane lambda3, LAMBDACAP21-2): `(combined SAM
    /// pc, combined invokedynamic pc, capture count)` of each SAM call inside
    /// a spliced body that [`IrBuilder::lambda_impl_opcode`] built as its
    /// impl's splice. Read only by the door's census line under
    /// `CRATONVM_DBG_JITC` (whether the proxy's allocation survived escape
    /// analysis: LAMBDA321-1). Empty for every other graph.
    pub lambda_impl_splice_rows: Vec<(usize, usize, usize)>,
    /// Interpreter round i2 wave 54, lane L2 (stage 2 of
    /// `i39-L3-proposal-post-call-exits-inside-a-splice`): the method keys of
    /// the spliced bodies whose bytecode the artifact keeps
    /// (`CompiledMethod::splice_scope_sources`), set by `lib.rs`'s IR inline
    /// planner only for a method that is not `ACC_SYNCHRONIZED`. A
    /// METHOD-ENTRY compile puts a post-call exit after a call inside a
    /// splice only when every spliced scope of that call's chain is named
    /// here (`ir_lower::Lowerer::post_call_chain_exit_state`): its stash sinks
    /// rebuild a scope whose class a redefinition replaced from that kept
    /// bytecode, and a sink that refused after the call committed would
    /// re-run the method from entry. Empty (the default, every hand-built
    /// graph) admits no method-entry chain site.
    pub entry_chain_source_keys: Vec<String>,
    /// nio21 wave 3, lane L2: one row per call site the builder gave
    /// call-site exception edges ([`Op::CallThrew`]`{ site, .. }` indexes
    /// this). Empty unless `CRATONVM_JIT_IR_CALL_SITE_EXCEPTION_EDGES` is on
    /// and `lib.rs` handed the builder the catch-type names.
    pub call_throw_sites: Vec<IrCallThrowSite>,
    /// nio21 wave 4, lane L2: the `try` ranges (combined-buffer pcs) of the
    /// spliced callees whose handlers the call-site edges cover
    /// ([`ir_splice_callee_handlers_enabled`]). Every exit a node stamped
    /// inside one may take must be an ARMED call-site edge or a guard; the
    /// lowering refuses the compile (and names the splice for a rebuild
    /// without it) otherwise.
    pub splice_handler_covered: Vec<(usize, usize)>,
    /// j27 wave 7 (lane T1L1, `CRATONVM_JIT_IR_SPLICE_HANDLER_LOWER_PROVEN`):
    /// the field/array accesses and allocations inside [`Self::splice_handler_covered`]
    /// the builder proved deliver nothing a covering handler catches
    /// (`IrBuilder::note_splice_handler_proven_exits`); the lowering admits
    /// exactly these ([`splice_handler_exit_proven`]). Empty unless the switch
    /// is on.
    pub splice_handler_proven_exits: Vec<NodeId>,
    pub splice_handler_proven_pcs: Vec<usize>,
    /// JIT round 19 wave 3 (lane splice3, SPLICE2-2): the `(spliced callee
    /// key, callee bci, reason)` guards the de-spec registry names as
    /// expected throws ([`splice_expected_throw_rows`]), set by `lib.rs`'s
    /// planner only under [`ir_splice_expected_throw_enabled`]. `ir_lower`
    /// marks a CHAIN guard whose innermost scope matches a row
    /// (`ResumeSemantics::expected_throw`). Empty (the default, every
    /// hand-built graph) marks nothing.
    pub splice_expected_throw: Vec<(String, u32, crate::deopt::DeoptReason)>,
    /// JIT round 19 (lane splice19, CH16-4): how many chain snapshots the
    /// builder recorded inside spliced bodies, and how many it skipped under
    /// `CRATONVM_JIT_IR_CHAIN_SNAPSHOTS_LAZY` at an instruction no guard can
    /// be lowered at (`IrBuilder::splice_pc_may_read_chain`). Census only:
    /// read by the `CRATONVM_DBG_IR_CHAIN_SNAPSHOTS` line in `lib.rs`.
    pub chain_snapshot_census: ChainSnapshotCensus,
    /// JIT round 19 wave 2 (lane osr2, FG16-5 / IROPS19-6,
    /// `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE`): the bytecode pcs of the loop
    /// headers whose field cells `ir_optimize`'s loop field promotion keeps in
    /// registers, with the stores sunk to the loop exit. Memory is stale inside
    /// such a loop, so no frame may leave it for the interpreter: the pass
    /// admits only loops with no deopt point, and the lowerer refuses the
    /// back-edge polls' mode exits at these headers
    /// (`Lowerer::back_edge_mode_exit_state`, shape `promoted-field-cells`).
    /// Keyed by pc, not node, so a copy of the header keeps the fact. Empty
    /// (the default) changes nothing.
    pub promoted_cell_header_bcis: Vec<usize>,
    /// JIT round 19 wave 4 (lane osr4, OSR2-1,
    /// `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_POLL_WRITEBACK`): `(header region,
    /// exit write-back store)` for every cell the promotion sank, so the
    /// lowerer can give a promoted header's back-edge poll a mode exit that
    /// first writes each cell's current value back
    /// (`Lowerer::promoted_cell_poll_writebacks`). Keyed by the header NODE: a
    /// copy of the loop made after the promotion is not in it, and its poll
    /// keeps the refusal. Empty (the default) changes nothing.
    pub promoted_cell_writebacks: Vec<(NodeId, NodeId)>,
    /// JIT round 19 proposal wave 5, lane concat5 (STRGAP4-1,
    /// `CRATONVM_JIT_IR_CONCAT_INLINE`): THIS compile's `java/lang/String`
    /// layout, copied by [`IrBuilder::set_string_layout`] from the one
    /// `lib.rs` resolves per compile (per VM, never a process latch). Read
    /// only by `ir_lower`'s `Op::IndyBridge` arm, whose inline concat
    /// allocates a `String`. On the graph rather than a sixteenth argument of
    /// the lowering doors for the reason [`Graph::receiver_param`] gives.
    /// `None` (every hand-built graph) keeps every bridge call as it was.
    pub string_layout: Option<crate::StringFieldLayout>,
    /// JIT round 19 wave 5 (lane osr5, OSR2-3,
    /// `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_PENDING_DEOPT`): `(exit write-back
    /// store, in-loop store)` for every in-loop store of a promoted cell that
    /// the promotion KEPT in the graph because its loop holds a deopt point.
    /// Such a store is SILENT: the lowerer emits nothing for it
    /// (`Lowerer::promoted_pending`), it stays only so the schedule keeps its
    /// position relative to the loop's guards and its value stays live; every
    /// deopt pad inside the loop writes each cell's current value back first
    /// (`emit_cell_writeback`). Empty (the default) changes nothing.
    pub promoted_sunk_stores: Vec<(NodeId, NodeId)>,
    /// JIT round 19 wave 6 (lane osr6, OSR5-2,
    /// `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_PREDICT`): this compile is the IR
    /// door's one retry after a lowering refused a promoted loop's deopt
    /// write-back (`promoted field cells: ...`), so the promotion admits no
    /// loop holding a deopt point (wave 2's rule) whatever
    /// `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_PENDING_DEOPT` says. `false` (the
    /// default) changes nothing.
    pub promote_pending_withheld: bool,
    /// JIT round 19 wave 8 (lane osr8, OSR7-2,
    /// `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_JOIN_WALK`): the value phis the
    /// promotion built at a promoted loop's body joins by walking every body
    /// memory phi, whether or not the exit's value reads them. Dead-node
    /// elimination keeps them (`eliminate_dead_nodes` roots them), because
    /// the lowering's pending plan reads a join's value phi at a deopt point
    /// between that join and the cell's next store
    /// (`ir_lower::pending_join_phi`). Empty (the default) changes nothing.
    pub promoted_join_phis: Vec<NodeId>,
    /// JIT round 21 wave 1 (lane loops, LOOPS21-2;
    /// `CRATONVM_JIT_IR_PREDICATE_OWN_SPECULATION`, opt-in): each loop-entry
    /// predicate `Op::Guard` a planter put down, with the planter's kind
    /// ([`crate::deopt::PredicateKind`] as `u8`). The lowering gives such a
    /// guard's deopt point the planter's own speculation id
    /// ([`crate::deopt::predicate_speculation_id`]) instead of the shared
    /// `(bci, UncommonTrap)` one, so a trapping predicate withdraws only
    /// itself. Empty (the default, and always with the switch off) changes
    /// nothing.
    pub predicate_guard_kinds: Vec<(NodeId, u8)>,
    /// JIT round 23 wave 4 (lane L1; `CRATONVM_JIT_IR_NULL_BASE_LOAD_PREDICATE_CHAIN`):
    /// each loop-entry predicate `Op::Guard` whose frame is an inlined CHAIN
    /// (a loop spliced from a callee), with the planter's kind
    /// ([`crate::deopt::PredicateKind`] as `u8`). Filled whatever
    /// `CRATONVM_JIT_IR_PREDICATE_OWN_SPECULATION` says: the lowering gives
    /// such a chain point [`crate::deopt::chain_predicate_speculation_id`] of
    /// its innermost scope, which is how the VM's chain sinks tell a failing
    /// predicate from the splice's other guards and record it at the call
    /// site without withdrawing the whole splice. Empty changes nothing.
    pub chain_predicate_guards: Vec<(NodeId, u8)>,
    /// JIT round 23 wave 8 (lane W8-L7, TYPECHECK21-1 part C;
    /// `CRATONVM_JIT_IR_CHECKCAST_VALUE_FORWARD`): the `Op::CheckCast` nodes
    /// whose every VALUE use (frame slots included) `ir_optimize::
    /// forward_checkcast_values` rewired to the cast's operand. Such a cast
    /// stays (a DCE root, on its memory chain, still throwing
    /// `ClassCastException` through its helper) but no reader takes its result,
    /// so the lowering's `CheckCast` arm stores no result on its inline hit
    /// path. Empty (the default) changes nothing.
    pub checkcast_value_forwarded: Vec<NodeId>,
    /// Round rx27 (lane compile, `CRATONVM_JIT_IR_CHECKCAST_PROXY_WAYS`):
    /// `(name_ptr, cell)` per `Op::CheckCast` target of this body, the cell
    /// being the target's lambda-proxy ways (a `TypecheckSeenCell` the
    /// artifact owns, shared with that target's `instanceof`s). Keyed by the
    /// interned target name, which every copy of a cast keeps. Empty (the
    /// default) changes nothing.
    pub checkcast_proxy_cells: Vec<(usize, usize)>,
    /// JIT r23 w2c close (`CRATONVM_JIT_IR_UNROLL_AFTER_PREDICATES`): the loop
    /// headers a partial unroll already transformed this compile -- a main/post
    /// unroll's main header and its post loop (the original header), a
    /// per-copy-test unroll's header. The second unroll round, after the
    /// loop-entry predicates, skips them: offered again, a kept loop would get
    /// copies of its copies. Empty changes nothing.
    pub partially_unrolled_headers: Vec<NodeId>,
    /// JIT r27 LOOP27-2 (`ir_optimize::long_induction_narrow`): for the
    /// `i2l(header phi)` a narrowed `long` induction hands its frames, the
    /// `long` constant that is its value at loop entry. `loop_entry_values`
    /// reads it (it cannot add a node), and DCE keeps the constants alive.
    pub wide_entry: rustc_hash::FxHashMap<NodeId, NodeId>,
    /// j27 wave 2 (lane STRIPMINE, `CRATONVM_JIT_IR_STRIP_MINE`, default on): the
    /// INNER headers of the strip-mined loops (`ir_optimize::strip_mine`). A
    /// back edge into one of them emits no safepoint poll; its outer loop
    /// polls once per strip of at most `CRATONVM_JIT_IR_STRIP_MINE_ITER` inner
    /// trips (`ir_optimize::latch_enters_only_strip_inner`). Empty changes
    /// nothing.
    pub strip_mined_inner_headers: Vec<NodeId>,
    /// j27 wave 4 (lane STRIPOSR, `CRATONVM_JIT_IR_STRIP_MINE_ARRAYS`, opt-in):
    /// the `ArrayLoad` / `ArrayLength` nodes of strip-mined inner loops whose
    /// null (and bounds) checks `ir_check_elim::analyze` proved on the graph
    /// BEFORE the strip mine hid the main test behind the strip limit. Every
    /// run of such a node in the strip-mined graph is a run of it in that
    /// graph, with the same values, so the proof carries over; `analyze` marks
    /// them elided. Empty changes nothing.
    pub strip_mine_proven_accesses: Vec<NodeId>,
    /// JIT round 19 wave 5 (lane handler5, SPLICE4-1): the exception-table row
    /// each [`Op::CatchEntryFlag`] arm enters, indexed by the flag's `site`.
    /// Empty unless `CRATONVM_JIT_IR_COMPILED_HANDLER_ENTRY` armed the build
    /// (`IrBuilder::set_catch_arm_table`).
    pub catch_arm_sites: Vec<CatchArmSite>,
    /// nio21 wave 18 (lane L3): bytecode pcs at which an optimizer pass placed
    /// a field `Op::Load` that is NOT the field instruction of that pc -- the
    /// loop field forward's pre-header reads (the header's bci) and its
    /// reloads after a call (`ir_optimize::loop_field_forward`, the call's
    /// successor bci, where the frame the reload may deopt to lives). The
    /// lowering's per-site compact rows are keyed by `(pc, is_reference)` and
    /// carry no field index, so a row at such a pc may describe ANOTHER field
    /// (`getObj().f` puts a `getfield` right after an `invoke`); every field
    /// access at a listed pc takes the field-index-addressed arms instead
    /// (`Lowerer::emit_inline_compact_getfield`). Empty (the default) changes
    /// nothing.
    pub synthetic_field_access_pcs: HashSet<usize>,
    /// JIT r20 wave 18 (lane xLib, THROW20-4; `CRATONVM_JIT_IR_REFERENT_SPLICE`
    /// in the VM): bytecode pcs of spliced `Reference.referent` reads
    /// ([`IrInlineTables::referent_load_pcs`]). Every field load at a listed pc
    /// takes the checked `jit_getfield` helper, never an inline arm: the
    /// helper is the one compiled load that logs a referent through the SATB
    /// keep-alive HotSpot's `Reference.get` intrinsic performs (SBR4 satb).
    /// Empty (the default) changes nothing.
    pub referent_load_pcs: HashSet<usize>,
    /// JIT round 25 (item 4, `CRATONVM_JIT_IR_PROVEN_RECEIVER_FIELDS`): the
    /// field-access bytecode pcs at which this VM's de-spec registry withdrew
    /// the deopting compactness guard (`(pc, speculation_id(pc,
    /// SpeculationFailed))` -- a legacy receiver kept arriving there), filled
    /// by `ir_optimize::optimize_with` from the registry. `None` -- no
    /// registry in scope, or the switch off -- plants no such guard at all,
    /// because nothing could ever withdraw one (`Lowerer::proven_receiver_mode`).
    pub proven_compact_withdrawn_pcs: Option<HashSet<usize>>,
    /// nio21 wave 20 (lane L3): per-NODE compact rows `(absolute compact
    /// displacement, is_reference, type tag)` for loads placed at a
    /// [`Self::synthetic_field_access_pcs`] pc, whose per-pc rows the lowering
    /// may not take. Only a pass that knows the exact field fills one (the
    /// `ThreadLocal.get` expansion, from the VM's layout); the lowering
    /// believes a row only for the node it names. Empty changes nothing.
    pub field_rows: HashMap<NodeId, (u32, bool, u8)>,
    /// t1 wave 10 (lane REFS3, `CRATONVM_JIT_IR_REFERS_TO_NO_KEEPALIVE`,
    /// default ON): COMPARE-ONLY `Reference.referent` loads -- the `refersTo`
    /// expansion's read (`IrBuilder::try_reference_refers_to_intrinsic`) and
    /// the `ThreadLocal` expansion's `refersTo(this)` step. HotSpot loads
    /// `refersTo0`'s referent `AS_NO_KEEPALIVE`, so the lowering marks each
    /// one's checked `jit_getfield` call `GETFIELD_NO_KEEPALIVE` and the
    /// helper skips its slot-0 SATB keep-alive; `Reference.get`'s reads
    /// ([`Self::referent_load_pcs`]) are never listed. Empty changes nothing.
    pub no_keepalive_referent_loads: HashSet<NodeId>,
    /// JIT round 23 wave 9 (lane W9-L2, CF-W9-3): volatile-marked stores that
    /// are RELEASE (or opaque) stores -- a constant-`VarHandle` `setRelease` /
    /// `setOpaque` built as its field store
    /// (`IrBuilder::try_vh_const_fresh_field_access`). Every pass orders them
    /// as volatile accesses; the lowering owes them no StoreLoad fence (x86
    /// TSO makes the plain `MOV` a release store). Empty changes nothing.
    pub release_stores: HashSet<NodeId>,
    /// `CRATONVM_JIT_IR_STATIC_FINAL_EXACT_CLASS`: per-NODE, the `getfield`
    /// pc whose compact row a load placed at a
    /// [`Self::synthetic_field_access_pcs`] pc may take -- the read of the
    /// SAME field (declaring class and index) it answers for
    /// (`loop_field_forward`'s pre-header and reload reads). The lowering
    /// believes it only for the node it names, as [`Self::field_rows`].
    pub field_row_pcs: HashMap<NodeId, usize>,
    /// `CRATONVM_JIT_IR_STATIC_FINAL_EXACT_CLASS`: the `(class, static index)`
    /// of every trusted `static final` reference the VM filed with a non-null
    /// value (`IrBuilder::set_static_final_exact_classes`). An
    /// `Op::LoadStatic` of one is never null: its class is initialized and
    /// nothing writes the field again.
    pub static_final_non_null: HashSet<(u32, u32)>,

    /// JIT round 21 (lane tlget, `CRATONVM_JIT_IR_THREAD_LOCAL_TRUSTED_LOADS`):
    /// the `ThreadLocal.get` / `set` expansion's REFERENCE field loads
    /// (`IrBuilder::try_thread_local_get_intrinsic`). Each base is a `Ref`
    /// node (the current thread, or a value the expansion null- and
    /// exact-class-tested before reading it), so the lowering gives each the
    /// trusted-receiver form `CRATONVM_JIT_IR_TRUSTED_REF_GETFIELD` gives every
    /// reference load: one "read table still live" compare in place of the
    /// alignment test and the six region-bound compares
    /// (`Lowerer::emit_inline_compact_getfield`). Empty changes nothing.
    pub tl_trusted_loads: HashSet<NodeId>,
    /// TLGET21-3 (`CRATONVM_JIT_IR_THREAD_LOCAL_LOOP_PREDICATE`): every
    /// TRAP-form `ThreadLocal.get` expansion this build made
    /// (`IrBuilder::try_thread_local_get_intrinsic`), which
    /// `ir_optimize::tl_chain_predicate` may move whole to a loop's
    /// pre-header. A record names node ids only; the pass re-checks the
    /// shape before it trusts any of them. Empty changes nothing.
    pub tl_chains: Vec<TlChainRecord>,
    /// JIT round 20 wave 12 (lane qLamNew, `CRATONVM_JIT_IR_INDY_CAPTURE_NEW`):
    /// the lambda-proxy class ids an `Op::New` of this graph allocates for a
    /// capturing lambda site ([`IrBuilder::try_indy_capture_new`]). A VM-spun
    /// proxy has no `<clinit>`, no finalizer, no reference kind and no compact
    /// layout, and the builder stores every field right after the allocation,
    /// so the lowering treats such a `new` as initialised with no post-init
    /// work (`ir_lower`'s `Op::New` arm). Empty (the default) changes nothing.
    pub lambda_proxy_alloc_classes: HashSet<u32>,
}

/// JIT round 19 wave 5 (lane handler5, SPLICE4-1): one compiled handler entry
/// the builder hung off a call ([`Op::CatchEntryFlag`]). The lowerer turns it
/// into a [`crate::JitLocalHandlerSite`] with this ONE candidate: the first
/// row of the method's exception table (in table order) that covers
/// `call_bci`. A throwable that row does not catch makes the lookup answer
/// "propagate", and the exit the call always took routes it through the whole
/// table in the interpreter, so a later row is never entered out of order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatchArmSite {
    /// The `invoke` pc the arm hangs off (caller code, never a spliced pc).
    pub call_bci: usize,
    /// The row's `handler_pc`.
    pub handler_pc: usize,
    /// The row's catch type, `""` for a catch-all (`catch_type == 0`), as
    /// [`crate::intern_catch_type_name`] interned it.
    pub catch_name: &'static str,
    /// The compiling method's declaring class, which resolves `catch_name`.
    pub declaring_class_id: u32,
    /// The candidates the call's ONE local-handler site asks, in table order:
    /// `(catch_name, handler_pc)` per covering row of the call's armed prefix.
    /// `[(catch_name, handler_pc)]` (this arm's row alone) unless
    /// `CRATONVM_JIT_IR_CATCH_ARM_LADDER` (JIT round 19 wave 6, lane handler6,
    /// HANDLER19-3) armed more rows; then every arm hung off the call carries
    /// the same list, and each row's `handler_pc` names the arm it enters.
    pub candidates: Vec<(&'static str, usize)>,
}

/// `CRATONVM_JIT_IR_CATCH_ARM_LADDER` -- JIT round 19 wave 6 (lane handler6,
/// HANDLER19-3). Opt-in, default OFF; inert without
/// `CRATONVM_JIT_IR_COMPILED_HANDLER_ENTRY`. A call gets one compiled handler
/// arm per distinct handler of the longest table-order prefix of its covering
/// exception-table rows whose handlers all qualify (not only the first row),
/// and a call inside an opened handler region is planned like one of the
/// method's own (a `catch` block's call covered by the `finally` row or an
/// outer `catch`). The probe's lookup asks the whole prefix, so the first
/// matching row in table order still wins (JVMS 2.10); a throwable no prefix
/// row catches keeps the exit the call always took.
pub fn ir_catch_arm_ladder_enabled() -> bool {
    false
}

/// `CRATONVM_JIT_IR_CATCH_ARM_SHADOW_EXITS` -- JIT round 19 wave 7 (lane
/// handler7, HANDLER6-3 of `jit-r19-handler6-proposals-RETIRED-20261009.md`). default on, default
/// OFF; inert without `CRATONVM_JIT_IR_COMPILED_HANDLER_ENTRY`. An armed
/// call's exceptional exit that leaves BEFORE its site's shadow reload (an
/// inline-cache `.tail` exit of a virtual / interface call with live
/// references across it) is routed through the compiled handler probe too;
/// the probe's hit edge reloads the publication (copies the possibly moved
/// references back into their homes and retracts the shadow `top`) before it
/// enters the arm, and its miss edge keeps the exit it always took. Off, such
/// an exit keeps the wave-5 route and counts `ir-wire-pending-shadow`.
pub fn ir_catch_arm_shadow_exits_enabled() -> bool {
    true
}

/// JIT round 19 wave 5 (lane handler5, SPLICE4-1): one exception-table row as
/// the builder plans compiled handler entries from it
/// (`IrBuilder::set_catch_arm_table`): `[start_pc, end_pc)`, `handler_pc`, the
/// interned catch type (`""` = catch-all).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatchArmRow {
    pub start_pc: usize,
    pub end_pc: usize,
    pub handler_pc: usize,
    pub catch_name: &'static str,
}

/// `CRATONVM_JIT_IR_COMPILED_HANDLER_ENTRY` -- JIT round 19 wave 5 (lane
/// handler5, SPLICE4-1). Opt-in, default OFF. A plain call of an optimizing
/// body whose `invoke` the method's own first covering exception-table row
/// protects gets a compiled handler entry: the call's exceptional exit asks
/// the local-handler lookup and, when that row catches the pending throwable,
/// continues in the body's compiled copy of the handler instead of leaving the
/// activation for the interpreter. Off, handler bodies are not in the graph
/// (the STUB-S8 skip) and every throw leaves the activation, as before.
pub fn ir_compiled_handler_entry_enabled() -> bool {
    false
}

/// JIT round 19 (lane splice19, CH16-4): the builder's half of the chain
/// snapshot census ([`Graph::chain_snapshot_census`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ChainSnapshotCensus {
    /// Chain snapshots pushed at the top of a spliced instruction (not the
    /// post-call ones).
    pub recorded: usize,
    /// Spliced instructions the lazy switch recorded no snapshot for.
    pub skipped: usize,
    /// Slots (locals + stack + monitors) the recorded ones hold.
    pub recorded_words: usize,
}

impl Graph {
    /// JIT round 19 wave 3, lane eager3 (EAGER2-4): record `cause` for the
    /// trap or class-init guard planted at `pc` ([`Graph::trap_causes`]). The
    /// first cause recorded for a pc wins: one bytecode is one site.
    pub fn note_trap_cause(&mut self, pc: usize, cause: TrapCause) {
        if !self.trap_causes.iter().any(|&(p, _)| p == pc) {
            self.trap_causes.push((pc, cause));
        }
    }

    /// The cause recorded for the trap or guard planted at `pc`, if any.
    pub fn trap_cause_at(&self, pc: usize) -> Option<TrapCause> {
        self.trap_causes
            .iter()
            .find(|&&(p, _)| p == pc)
            .map(|&(_, c)| c)
    }

    /// A graph with no nodes, no snapshots, no exit yet (`exit == NO_NODE`),
    /// derive-on-demand use lists and no receiver claim — room reserved for
    /// `node_capacity` nodes.
    ///
    /// Round 11 wave 6, the `Graph` half of step 1 in
    /// `r11-irback-use-list-invariant-enforced-by-convention-only`: a site that
    /// builds its graph through this instead of a struct literal survives a
    /// change to the type of [`Graph::safepoints`]. A hand-built fixture that
    /// converts should note `exit`: literals in-tree spell it `0`, and this
    /// answers `NO_NODE` until the fixture assigns its terminator.
    pub fn empty(node_capacity: usize) -> Self {
        Graph {
            nodes: Vec::with_capacity(node_capacity),
            entry: 0,
            exit: NO_NODE,
            safepoints: Safepoints::default(),
            uses: UseLists::new(),
            receiver_param: None,
            box_class_ids: (None, None),
            exception_event_guard: None,
            site_trap_pcs: Vec::new(),
            hoisted_arm_trap_pcs: Vec::new(),
            trap_causes: Vec::new(),
            unloaded_static_call_traps: false,
            class_loading_trap_pcs: Vec::new(),
            unstable_if_trap_pcs: Vec::new(),
            scope_method_key: String::new(),
            dbg_label: String::new(),
            throw_stamp_token: 0,
            splice_chains_required: false,
            opaque_forwarder_bases: Vec::new(),
            field_memo_census: (0, 0, 0, 0, 0),
            osr_compile: false,
            vthread_boundaries: None,
            optimize_osr_route: false,
            late_licm: false,
            loop_carried_regs: false,
            context_abi_floor: false,
            self_call_service_infos: Vec::new(),
            field_decl_class: HashMap::new(),
            field_identity_alias: false,
            scoped_raw_alias: false,
            plain_static_reads: HashSet::new(),
            finalizer_free_new_classes: HashSet::new(),
            finalizable_new_classes: HashSet::new(),
            byte_array_nodes: HashSet::new(),
            ffm_final_field_reads: HashSet::new(),
            final_java_field_reads: HashSet::new(),
            ffm_proven_heap_accesses: HashSet::new(),
            ffm_scoped_length_fields: HashMap::new(),
            ffm_bimorphic_tests: HashSet::new(),
            ffm_versioned_field_headers: HashMap::new(),
            ffm_arena_cached: HashSet::new(),
            ffm_heap_scope_reads: HashSet::new(),
            ffm_heap_session_class: 0,
            ffm_native_scope_reads: HashSet::new(),
            ffm_session_prechecks: HashSet::new(),
            ffm_never_closing_scopes: HashSet::new(),
            ffm_session_guards: HashSet::new(),
            sync_splice_windows: Vec::new(),
            sync_call_monitor_ops: Vec::new(),
            monitor_base_proofs: Vec::new(),
            seq_charat_trap_headers: HashSet::new(),
            seq_charat_receiver_rows: HashMap::new(),
            method_monitor_param: None,
            method_monitor_class: None,
            method_synchronized: None,
            getclass_prefix_refused_infos: HashSet::new(),
            first_call_cells: Vec::new(),
            scalar_first_call_cells: Vec::new(),
            native_call_contexts: HashMap::new(),
            method_self_lock: None,
            direct_static_fields: Vec::new(),
            static_alias_class: false,
            static_ref_store_effect: false,
            licm_hoisted_static_reads: Vec::new(),
            trusted_final_static_fields: Vec::new(),
            trusted_static_array_lengths: Vec::new(),
            const_ref_meta: HashMap::new(),
            frozen_misses: Vec::new(),
            frozen_cha_deps: Vec::new(),
            cha_leaf_classes: rustc_hash::FxHashSet::default(),
            frozen_identity_guards: 0,
            frozen_folded_classes: Vec::new(),
            frozen_failed_classes: Vec::new(),
            mh_constant_call_pcs: Vec::new(),
            call_reported_bcis: Vec::new(),
            record_fold_call_pcs: Vec::new(),
            sb_chain_fused_sites: Vec::new(),
            lambda_impl_splice_rows: Vec::new(),
            entry_chain_source_keys: Vec::new(),
            call_throw_sites: Vec::new(),
            splice_handler_covered: Vec::new(),
            splice_handler_proven_exits: Vec::new(),
            splice_handler_proven_pcs: Vec::new(),
            splice_expected_throw: Vec::new(),
            chain_snapshot_census: ChainSnapshotCensus::default(),
            promoted_cell_header_bcis: Vec::new(),
            promoted_cell_writebacks: Vec::new(),
            string_layout: None,
            promoted_sunk_stores: Vec::new(),
            promote_pending_withheld: false,
            promoted_join_phis: Vec::new(),
            predicate_guard_kinds: Vec::new(),
            chain_predicate_guards: Vec::new(),
            checkcast_value_forwarded: Vec::new(),
            checkcast_proxy_cells: Vec::new(),
            partially_unrolled_headers: Vec::new(),
            wide_entry: rustc_hash::FxHashMap::default(),
            strip_mined_inner_headers: Vec::new(),
            strip_mine_proven_accesses: Vec::new(),
            catch_arm_sites: Vec::new(),
            synthetic_field_access_pcs: HashSet::new(),
            referent_load_pcs: HashSet::new(),
            proven_compact_withdrawn_pcs: None,
            field_rows: HashMap::new(),
            no_keepalive_referent_loads: HashSet::new(),
            release_stores: HashSet::new(),
            field_row_pcs: HashMap::new(),
            static_final_non_null: HashSet::new(),

            tl_trusted_loads: HashSet::new(),
            tl_chains: Vec::new(),
            lambda_proxy_alloc_classes: HashSet::new(),
        }
    }

    /// Round 14 wave 5 (lane sync5, SS8-3): the `Op::Param` node
    /// [`Self::method_monitor_param`] names, if the graph claims one and has
    /// it. First match by index; the builder makes one `Param` per index.
    pub(crate) fn method_monitor_node(&self) -> Option<NodeId> {
        let idx = self.method_monitor_param?;
        self.nodes
            .iter()
            .position(|n| matches!(n.op, Op::Param(p) if p == idx))
            .map(|i| i as NodeId)
    }

    /// Allocate a new node, returning its `NodeId`.
    pub fn add(&mut self, op: Op, ty: IrType, inputs: Vec<NodeId>, pc: Option<usize>) -> NodeId {
        self.add_inputs(op, ty, Inputs::from(inputs), pc)
    }

    /// [`Graph::add`] taking an already-compact edge list.
    ///
    /// The `Vec` form stays the primary entry point because every existing
    /// call site spells its edges `vec![…]`; this one exists for code that
    /// already holds an [`Inputs`] (a cloned edge list, a `collect()`).
    pub fn add_inputs(&mut self, op: Op, ty: IrType, inputs: Inputs, pc: Option<usize>) -> NodeId {
        // t1 wave 2 (lane Y): an `int` constant is stored SIGN-EXTENDED, an
        // IR invariant from here on (`ir_verify::check_int_const_width`). It
        // used to be per reader: `Const(0xFFFF_FFFF)` typed `Int` is `-1` to a
        // 32-bit compare, but a reader of the raw `i64` saw 4294967295 --
        // `ir_check_elim::non_negative_by` then took `v >= -1` as proof of
        // `v >= 0` (t1 jitbce,
        // `docs/feature-designs/jit-ir-optimizations.md`).
        // The machine code is unchanged: every `int` consumer reads the low
        // 32 bits, and an `int` home's high half is don't-care.
        let op = match op {
            // Cast: an `int` constant is its low 32 bits.
            Op::Const(c) if ty == IrType::Int => Op::Const(i64::from(c as i32)),
            op => op,
        };
        let id = self.nodes.len() as NodeId;
        // Ids are dense from 0, so this can only fire if a graph ever reaches
        // `u32::MAX` nodes — at which point a real id would be indistinguishable
        // from the `NO_NODE` placeholder and every "is this slot defined?" test
        // in the pipeline would invert. Pin the invariant rather than assume it.
        debug_assert!(
            id != NO_NODE,
            "node id collided with the NO_NODE sentinel ({} nodes)",
            self.nodes.len()
        );
        let current = self.use_lists_current();
        self.nodes.push(Node {
            op,
            ty,
            inputs,
            bytecode_pc: pc,
            // A new node belongs to no COPY until a producer says so: the
            // by-bci scan is the whole story for everything the builder makes.
            frame_snapshot: None,
            // Plain until the builder's `getfield` / `putfield` arm says
            // otherwise (`set_volatile_access`).
            volatile_access: false,
        });
        if current {
            if self.uses.dangling {
                // Some edge already names an id past the old end of the arena,
                // and `id` may be that id. Re-derive rather than guess.
                self.uses.valid = false;
            } else {
                self.uses.users.push(Inputs::new());
                self.uses.sp_users.push(Vec::new());
                let num_nodes = self.nodes.len();
                let (nodes, uses) = (&self.nodes, &mut self.uses);
                for &inp in nodes[id as usize].inputs.as_slice() {
                    uses.record_input(inp, id, num_nodes);
                }
                self.uses.stamp();
            }
        }
        id
    }

    /// Mark `id` as an access of a VOLATILE field ([`Node::volatile_access`]).
    ///
    /// Only an `Op::Load` / `Op::Store` can carry the mark; any other node (or
    /// an out-of-range id) is left untouched, so a caller cannot make a node
    /// "volatile" that no pass would ask about.
    pub fn set_volatile_access(&mut self, id: NodeId) {
        if let Some(n) = self.nodes.get_mut(id as usize) {
            if matches!(n.op, Op::Load(_) | Op::Store(_)) {
                n.volatile_access = true;
            }
        }
    }

    /// Replace all uses of `old` with `new_id` in the entire graph.
    ///
    /// Also rewrites any `old` references held by safepoint snapshots so a
    /// deopt frame state survives optimization rewrites (GVN, const-fold,
    /// materialization) intact — a safepoint slot pointing at `old` must
    /// follow the value to `new_id`, exactly like a real input edge.
    ///
    /// With current [`UseLists`] this visits only the recorded users of `old`
    /// (and only the snapshot slots that name it); otherwise it falls back to
    /// the historical full scan, which re-derives the lists in the same pass so
    /// the fallback is paid once rather than per call. The resulting graph is
    /// the same either way: both paths rewrite exactly the slots whose current
    /// value is `old`, and nodes are neither created, removed nor reordered.
    pub fn replace_all_uses(&mut self, old: NodeId, new_id: NodeId) {
        // Self-replacement is a graph no-op, but it is NOT a use-list no-op on
        // the tracked path: rewriting a slot to `old` leaves it matching `old`,
        // so every visit of a multi-edge user counts the same hits again and
        // re-pushes them. A user with k edges then accrues k² entries. Nothing
        // in the graph changes, so return before either path runs.
        if old == new_id {
            return;
        }
        // `old == NO_NODE` (or an out-of-range id) has no use list to walk: the
        // scan would rewrite every *undefined* slot, which is a different
        // operation. Preserve it exactly by scanning.
        if self.use_lists_current() && old != NO_NODE && (old as usize) < self.nodes.len() {
            self.replace_all_uses_tracked(old, new_id);
        } else {
            self.replace_all_uses_scanning(old, new_id);
        }
    }

    /// Incremental rewrite: walk the recorded users of `old`.
    pub(in crate::ir) fn replace_all_uses_tracked(&mut self, old: NodeId, new_id: NodeId) {
        let num_nodes = self.nodes.len();
        let new_is_node = new_id != NO_NODE && (new_id as usize) < num_nodes;
        let new_is_dangling = new_id != NO_NODE && !new_is_node;
        let mut examined = 0u64;

        // Take the list: after the rewrite nothing names `old` any more, so the
        // entries either move to `new_id` or disappear.
        let candidates = std::mem::take(&mut self.uses.users[old as usize]);
        for &user in candidates.as_slice() {
            if (user as usize) >= num_nodes {
                continue;
            }
            let hits = {
                let slots = self.nodes[user as usize].inputs.slots_mut();
                examined += slots.len() as u64;
                let mut hits = 0usize;
                for slot in slots.iter_mut() {
                    if *slot == old {
                        *slot = new_id;
                        hits += 1;
                    }
                }
                hits
            };
            // A user with two edges to `old` appears twice in the list; the
            // second visit finds nothing left to rewrite, which is why the
            // move-over is driven by `hits` and not by the entry count.
            if hits > 0 && new_is_node {
                for _ in 0..hits {
                    self.uses.users[new_id as usize].push_untracked(user);
                }
            }
        }
        if new_is_dangling {
            self.uses.dangling = true;
        }

        // Snapshot slots. The list is a superset (see `UseLists`), so each slot
        // is re-checked before it is rewritten.
        let sp_candidates = std::mem::take(&mut self.uses.sp_users[old as usize]);
        for r in sp_candidates {
            // `.0`: the tracked path, which must not bump the epoch it stamps.
            let sp = match self.safepoints.0.get_mut(r.snapshot as usize) {
                Some(sp) => sp,
                None => continue,
            };
            let cell = match r.kind {
                SafepointSlotKind::Local => sp.locals.get_mut(r.slot as usize),
                SafepointSlotKind::Stack => sp.stack.get_mut(r.slot as usize),
                SafepointSlotKind::Monitor => sp.monitors.get_mut(r.slot as usize),
                SafepointSlotKind::SbVirtual => sp.sb_virtual_slot_mut(r.slot as usize),
            };
            match cell {
                Some(v) if *v == old => {
                    *v = new_id;
                    if new_is_node {
                        self.uses.sp_users[new_id as usize].push(r);
                    }
                }
                _ => {}
            }
        }

        self.uses.examined += examined;
        // Nothing here bumped the epoch (`slots_mut` is the untracked path), but
        // re-read it rather than assume: an unrelated graph may have moved it.
        self.uses.stamp();
    }

    /// The historical full scan, plus (when tracking is on) a rebuild of the
    /// use lists in the same pass — the scan already touches every edge, so
    /// deriving the lists from it costs only the pushes.
    pub(in crate::ir) fn replace_all_uses_scanning(&mut self, old: NodeId, new_id: NodeId) {
        let num_nodes = self.nodes.len();
        let track = self.uses.tracking;
        let mut users: Vec<Inputs> = if track {
            vec![Inputs::new(); num_nodes]
        } else {
            Vec::new()
        };
        let mut sp_users: Vec<Vec<SafepointSlot>> = if track {
            vec![Vec::new(); num_nodes]
        } else {
            Vec::new()
        };
        let mut dangling = false;
        let mut examined = 0u64;

        for (uid, node) in self.nodes.iter_mut().enumerate() {
            let slots = node.inputs.slots_mut();
            examined += slots.len() as u64;
            for inp in slots.iter_mut() {
                if *inp == old {
                    *inp = new_id;
                }
                if track {
                    let def = *inp;
                    if def == NO_NODE {
                        continue;
                    }
                    if (def as usize) < num_nodes {
                        users[def as usize].push_untracked(uid as NodeId);
                    } else {
                        dangling = true;
                    }
                }
            }
        }
        // `.0`: this scan re-derives the lists and stamps them below.
        for (si, sp) in self.safepoints.0.iter_mut().enumerate() {
            let locals = sp.locals.len();
            for (k, v) in sp.locals.iter_mut().chain(sp.stack.iter_mut()).enumerate() {
                if *v == old {
                    *v = new_id;
                }
                if track {
                    let def = *v;
                    if def == NO_NODE {
                        continue;
                    }
                    let slot = if k < locals {
                        SafepointSlot {
                            snapshot: si as u32,
                            kind: SafepointSlotKind::Local,
                            slot: k as u32,
                        }
                    } else {
                        SafepointSlot {
                            snapshot: si as u32,
                            kind: SafepointSlotKind::Stack,
                            slot: (k - locals) as u32,
                        }
                    };
                    if (def as usize) < num_nodes {
                        sp_users[def as usize].push(slot);
                    } else {
                        dangling = true;
                    }
                }
            }
            for (k, v) in sp.monitors.iter_mut().enumerate() {
                if *v == old {
                    *v = new_id;
                }
                if track {
                    let def = *v;
                    if def == NO_NODE {
                        continue;
                    }
                    let slot = SafepointSlot {
                        snapshot: si as u32,
                        kind: SafepointSlotKind::Monitor,
                        slot: k as u32,
                    };
                    if (def as usize) < num_nodes {
                        sp_users[def as usize].push(slot);
                    } else {
                        dangling = true;
                    }
                }
            }
            let mut sb_slot = 0u32;
            for b in &mut sp.sb_virtual_builders {
                for p in &mut b.parts {
                    if let SbVirtualPart::Value { val, .. } = p {
                        if *val == old {
                            *val = new_id;
                        }
                        if track {
                            let def = *val;
                            if def != NO_NODE {
                                let slot = SafepointSlot {
                                    snapshot: si as u32,
                                    kind: SafepointSlotKind::SbVirtual,
                                    slot: sb_slot,
                                };
                                if (def as usize) < num_nodes {
                                    sp_users[def as usize].push(slot);
                                } else {
                                    dangling = true;
                                }
                            }
                        }
                        sb_slot += 1;
                    }
                }
            }
        }

        self.uses.examined += examined;
        if track {
            self.uses.users = users;
            self.uses.sp_users = sp_users;
            self.uses.dangling = dangling;
            self.uses.sp_len = self.safepoints.len();
            self.uses.valid = true;
            self.uses.rebuilds += 1;
            self.uses.stamp();
        }
    }

    /// Mark a node as dead (clears inputs and op).
    pub fn kill(&mut self, id: NodeId) {
        let current = self.use_lists_current();
        let num_nodes = self.nodes.len();
        let removed = {
            // Index (rather than `get_mut`) so an out-of-range id still panics,
            // as it always has.
            let n = &mut self.nodes[id as usize];
            n.op = Op::Dead;
            let removed = if current {
                let edges = n.inputs.clone();
                n.inputs.clear_untracked();
                Some(edges)
            } else {
                n.inputs.clear();
                None
            };
            n.ty = IrType::Void;
            removed
        };
        if let Some(edges) = removed {
            for &inp in edges.as_slice() {
                if inp == NO_NODE || (inp as usize) >= num_nodes {
                    continue;
                }
                self.uses.users[inp as usize].swap_remove_first_untracked(id);
            }
            self.uses.stamp();
        }
    }

    /// Number of live (non-dead) nodes.
    pub fn live_count(&self) -> usize {
        self.nodes.iter().filter(|n| n.op != Op::Dead).count()
    }

    // ── Checked id accessors ─────────────────────────────────────────
    //
    // The sentinel-free replacements for `id != NO_NODE && (id as usize) <
    // nodes.len()`, which is spelled out (or half spelled out) at dozens of
    // sites across the pipeline. See the [`NO_NODE`] doc.

    /// True when `id` names a real node in this graph — i.e. it is neither the
    /// [`NO_NODE`] placeholder nor out of range.
    ///
    /// Note this says nothing about liveness: a killed node (`Op::Dead`) still
    /// has a valid id.
    #[inline]
    pub fn is_valid_id(&self, id: NodeId) -> bool {
        id != NO_NODE && (id as usize) < self.nodes.len()
    }

    /// Checked node lookup: `None` for [`NO_NODE`] or an out-of-range id.
    #[inline]
    pub fn node_opt(&self, id: NodeId) -> Option<&Node> {
        if id == NO_NODE {
            return None;
        }
        self.nodes.get(id as usize)
    }

    /// Checked mutable node lookup — [`Graph::node_opt`]'s `&mut` twin.
    #[inline]
    pub fn node_opt_mut(&mut self, id: NodeId) -> Option<&mut Node> {
        if id == NO_NODE {
            return None;
        }
        self.nodes.get_mut(id as usize)
    }

    /// The value type of `id`, or `None` if `id` does not name a node.
    #[inline]
    pub fn type_of(&self, id: NodeId) -> Option<IrType> {
        self.node_opt(id).map(|n| n.ty)
    }

    /// The data type of a `Op::Phi` with these `inputs`, or a description of
    /// why one could not be proven.
    ///
    /// `inputs` is the φ's full input list — `[merge_or_region, val_0, val_1,
    /// …]` — so `inputs[0]` (the control edge) is skipped and the remaining
    /// value edges are folded with [`join_data_type`].
    ///
    /// Semantics:
    /// - a value input that is [`NO_NODE`], out of range, or typed
    ///   [`IrType::Void`] contributes nothing and is skipped. This is *normal*,
    ///   not an error: `activate_loop_header` creates a loop-carried φ before
    ///   the back-edge value exists, and a local can be uninitialised on one
    ///   entry edge. `IrBuilder::retype_phi` re-runs the join once the
    ///   back-edge input has been appended;
    /// - `Err` when two inputs have no join (a category conflict — an untypeable
    ///   merge), or when *no* input carries a usable type at all;
    /// - `Ok` otherwise, with the joined type.
    ///
    /// The error is a `String` rather than a bail: the caller decides whether an
    /// untypeable φ is fatal. The builder's infallible wrapper
    /// (`IrBuilder::phi_data_type`) reports it and falls back to
    /// [`PHI_TYPE_FALLBACK`].
    pub fn phi_data_type_checked(&self, inputs: &[NodeId]) -> Result<IrType, String> {
        let mut joined: Option<IrType> = None;
        let mut unknown = 0usize;
        for (k, &n) in inputs.iter().enumerate().skip(1) {
            let ty = match self.node_opt(n) {
                Some(node) if node.op != Op::Dead && node.ty != IrType::Void => node.ty,
                // Placeholder, out of range, REMOVED, or a void-producing node:
                // no information, and not (on its own) a conflict.
                //
                // `op != Op::Dead` is redundant through `Graph::kill`, which
                // also rewrites the type to `Void` — and it is NOT redundant in
                // general: `escape_analysis::apply_*` writes `op = Op::Dead`
                // straight into `graph.nodes` and leaves the type alone, so a
                // retired `Op::New` is still `IrType::Ref`. The two functions
                // that are documented as folding this same lattice the same way
                // (`phi_inputs_conflict` here, `ir_verify::phi_join` there)
                // both carry the term; this one did not, so the builder and the
                // verifier could compute two different joins for one φ.
                _ => {
                    unknown += 1;
                    continue;
                }
            };
            joined = Some(match joined {
                None => ty,
                Some(prev) => join_data_type(prev, ty).ok_or_else(|| {
                    format!(
                        "phi value input #{k} (node {n}) is {ty:?}, which does not \
                         join with the {prev:?} of the preceding input(s)"
                    )
                })?,
            });
        }
        joined.ok_or_else(|| {
            format!(
                "phi has no typed value input ({} value edge(s), {unknown} \
                 undefined/void)",
                inputs.len().saturating_sub(1)
            )
        })
    }

    /// Build a use-count vector: `uses[id]` = number of nodes that reference `id` as an input.
    ///
    /// Reads the maintained [`UseLists`] when they are current, and otherwise
    /// falls back to the historical scan. Both produce the same vector: an
    /// entry per referencing input *slot*, snapshot slots excluded, edges to
    /// [`NO_NODE`] or to an id past the end of the arena ignored.
    pub fn use_counts(&self) -> Vec<u32> {
        if self.use_lists_current() {
            return self.uses.users.iter().map(|u| u.len() as u32).collect();
        }
        let mut counts = vec![0u32; self.nodes.len()];
        for node in &self.nodes {
            for &inp in &node.inputs {
                if (inp as usize) < counts.len() {
                    counts[inp as usize] += 1;
                }
            }
        }
        counts
    }

    // ── Incremental def-use edges ────────────────────────────────────

    /// True when the maintained [`UseLists`] describe this graph and may be
    /// consulted.
    ///
    /// Cheap enough to call on every mutation: a flag, a thread-local read and
    /// two length comparisons. `sp_len` catches snapshots pushed through the
    /// public `safepoints` field; the epoch catches edges written through the
    /// public `Node::inputs` field.
    #[inline]
    pub fn use_lists_valid(&self) -> bool {
        self.use_lists_current()
    }

    #[inline]
    pub(in crate::ir) fn use_lists_current(&self) -> bool {
        self.uses.tracking
            && self.uses.valid
            && self.uses.stamped()
            && self.uses.users.len() == self.nodes.len()
            && self.uses.sp_len == self.safepoints.len()
    }

    /// Turn def-use maintenance on or off. On by default.
    ///
    /// Turning it off drops the derived state and sends every mutation down the
    /// historical full-scan path — the escape hatch if a caller ever needs the
    /// pre-incremental behaviour bit for bit.
    pub fn set_use_tracking(&mut self, on: bool) {
        self.uses.tracking = on;
        if !on {
            self.uses.discard();
        }
    }

    /// Derive the def-use edges from a full scan of the graph.
    ///
    /// `O(nodes + edges + snapshot slots)`. Called automatically whenever a
    /// mutation finds the lists stale; public so a pass that is about to do
    /// many rewrites can pay for it up front. An explicit rebuild also turns
    /// tracking back on if [`Graph::set_use_tracking`] had turned it off.
    pub fn rebuild_use_lists(&mut self) {
        let num_nodes = self.nodes.len();
        self.uses.tracking = true;
        self.uses.users = vec![Inputs::new(); num_nodes];
        self.uses.sp_users = vec![Vec::new(); num_nodes];
        self.uses.dangling = false;
        {
            let (nodes, uses) = (&self.nodes, &mut self.uses);
            for (uid, node) in nodes.iter().enumerate() {
                for &inp in node.inputs.as_slice() {
                    uses.record_input(inp, uid as NodeId, num_nodes);
                }
            }
        }
        {
            let (safepoints, uses) = (&self.safepoints, &mut self.uses);
            for (si, sp) in safepoints.iter().enumerate() {
                for (k, &v) in sp.locals.iter().enumerate() {
                    uses.record_slot(
                        v,
                        SafepointSlot {
                            snapshot: si as u32,
                            kind: SafepointSlotKind::Local,
                            slot: k as u32,
                        },
                        num_nodes,
                    );
                }
                for (k, &v) in sp.stack.iter().enumerate() {
                    uses.record_slot(
                        v,
                        SafepointSlot {
                            snapshot: si as u32,
                            kind: SafepointSlotKind::Stack,
                            slot: k as u32,
                        },
                        num_nodes,
                    );
                }
                for (k, &v) in sp.monitors.iter().enumerate() {
                    uses.record_slot(
                        v,
                        SafepointSlot {
                            snapshot: si as u32,
                            kind: SafepointSlotKind::Monitor,
                            slot: k as u32,
                        },
                        num_nodes,
                    );
                }
                for (k, v) in sp.sb_virtual_nodes().enumerate() {
                    uses.record_slot(
                        v,
                        SafepointSlot {
                            snapshot: si as u32,
                            kind: SafepointSlotKind::SbVirtual,
                            slot: k as u32,
                        },
                        num_nodes,
                    );
                }
            }
        }
        self.uses.sp_len = self.safepoints.len();
        self.uses.valid = true;
        self.uses.rebuilds += 1;
        self.uses.stamp();
    }

    /// Make the def-use edges current, deriving them if they are not.
    pub fn ensure_use_lists(&mut self) {
        if self.uses.tracking && !self.use_lists_current() {
            self.rebuild_use_lists();
        }
    }

    /// Number of input slots naming `id`, in O(1) when the lists are current.
    ///
    /// The single-node form of [`Graph::use_counts`]; same definition, same
    /// exclusions (snapshot slots do not count).
    pub fn use_count(&self, id: NodeId) -> u32 {
        if self.use_lists_current() {
            return self
                .uses
                .users
                .get(id as usize)
                .map_or(0, |u| u.len() as u32);
        }
        if id == NO_NODE {
            return 0;
        }
        let mut n = 0u32;
        for node in &self.nodes {
            for &inp in &node.inputs {
                if inp == id {
                    n += 1;
                }
            }
        }
        n
    }

    /// The distinct nodes that reference `id` as an input, in no particular
    /// order.
    pub fn users_of(&self, id: NodeId) -> Vec<NodeId> {
        let mut out: Vec<NodeId> = Vec::new();
        if self.use_lists_current() {
            if let Some(list) = self.uses.users.get(id as usize) {
                for &u in list.as_slice() {
                    if !out.contains(&u) {
                        out.push(u);
                    }
                }
            }
            return out;
        }
        if id == NO_NODE {
            return out;
        }
        for (uid, node) in self.nodes.iter().enumerate() {
            if node.inputs.as_slice().contains(&id) {
                out.push(uid as NodeId);
            }
        }
        out
    }

    /// The snapshot slots that reference `id`.
    pub fn safepoint_users_of(&self, id: NodeId) -> Vec<SafepointSlot> {
        let mut out: Vec<SafepointSlot> = Vec::new();
        if id == NO_NODE {
            return out;
        }
        if self.use_lists_current() {
            if let Some(list) = self.uses.sp_users.get(id as usize) {
                // Superset: re-check each slot's current value.
                for &r in list {
                    let live =
                        self.safepoints
                            .get(r.snapshot as usize)
                            .and_then(|sp| match r.kind {
                                SafepointSlotKind::Local => sp.locals.get(r.slot as usize).copied(),
                                SafepointSlotKind::Stack => sp.stack.get(r.slot as usize).copied(),
                                SafepointSlotKind::Monitor => {
                                    sp.monitors.get(r.slot as usize).copied()
                                }
                                SafepointSlotKind::SbVirtual => sp.sb_virtual_slot(r.slot as usize),
                            });
                    if live == Some(id) && !out.contains(&r) {
                        out.push(r);
                    }
                }
            }
            return out;
        }
        for (si, sp) in self.safepoints.iter().enumerate() {
            for (k, &v) in sp.locals.iter().enumerate() {
                if v == id {
                    out.push(SafepointSlot {
                        snapshot: si as u32,
                        kind: SafepointSlotKind::Local,
                        slot: k as u32,
                    });
                }
            }
            for (k, &v) in sp.stack.iter().enumerate() {
                if v == id {
                    out.push(SafepointSlot {
                        snapshot: si as u32,
                        kind: SafepointSlotKind::Stack,
                        slot: k as u32,
                    });
                }
            }
            for (k, &v) in sp.monitors.iter().enumerate() {
                if v == id {
                    out.push(SafepointSlot {
                        snapshot: si as u32,
                        kind: SafepointSlotKind::Monitor,
                        slot: k as u32,
                    });
                }
            }
        }
        out
    }

    // ── Tracked edge mutation ────────────────────────────────────────
    //
    // The write half of the def-use invariant. Every one of these keeps the
    // lists current; the equivalent direct write through `Node::inputs` or
    // `Graph::safepoints` is still legal but costs a rebuild.

    /// Overwrite input slot `idx` of `node`. `false` when the node or the slot
    /// does not exist.
    pub fn set_input(&mut self, node: NodeId, idx: usize, new: NodeId) -> bool {
        if !self.is_valid_id(node) || idx >= self.nodes[node as usize].inputs.len() {
            return false;
        }
        let current = self.use_lists_current();
        let num_nodes = self.nodes.len();
        let old = {
            let slots = self.nodes[node as usize].inputs.slots_mut();
            let old = slots[idx];
            slots[idx] = new;
            old
        };
        if current {
            if old != NO_NODE && (old as usize) < num_nodes {
                self.uses.users[old as usize].swap_remove_first_untracked(node);
            }
            self.uses.record_input(new, node, num_nodes);
            self.uses.stamp();
        } else {
            bump_edge_epoch();
        }
        true
    }

    /// Append an input edge to `node`. `false` when the node does not exist.
    pub fn push_input(&mut self, node: NodeId, new: NodeId) -> bool {
        if !self.is_valid_id(node) {
            return false;
        }
        let current = self.use_lists_current();
        let num_nodes = self.nodes.len();
        self.nodes[node as usize].inputs.push_untracked(new);
        if current {
            self.uses.record_input(new, node, num_nodes);
            self.uses.stamp();
        } else {
            bump_edge_epoch();
        }
        true
    }

    /// Replace the whole edge list of `node`. `false` when it does not exist.
    pub fn set_inputs(&mut self, node: NodeId, inputs: impl Into<Inputs>) -> bool {
        if !self.is_valid_id(node) {
            return false;
        }
        let current = self.use_lists_current();
        let num_nodes = self.nodes.len();
        let new_inputs = inputs.into();
        let old = std::mem::replace(&mut self.nodes[node as usize].inputs, new_inputs);
        if current {
            for &inp in old.as_slice() {
                if inp != NO_NODE && (inp as usize) < num_nodes {
                    self.uses.users[inp as usize].swap_remove_first_untracked(node);
                }
            }
            let (nodes, uses) = (&self.nodes, &mut self.uses);
            for &inp in nodes[node as usize].inputs.as_slice() {
                uses.record_input(inp, node, num_nodes);
            }
            self.uses.stamp();
        } else {
            bump_edge_epoch();
        }
        true
    }

    /// Drop every input edge of `node` (without marking it dead). `false` when
    /// it does not exist.
    pub fn clear_inputs(&mut self, node: NodeId) -> bool {
        self.set_inputs(node, Inputs::new())
    }

    /// Append a safepoint snapshot, recording the node references it carries.
    pub fn push_safepoint(&mut self, sp: SafepointSnapshot) {
        let current = self.use_lists_current();
        let si = self.safepoints.len() as u32;
        // `.0`: recorded below (or left for the next rebuild when the lists
        // are not current, which `sp_len` already says).
        self.safepoints.0.push(sp);
        if current {
            let num_nodes = self.nodes.len();
            let (safepoints, uses) = (&self.safepoints, &mut self.uses);
            let snap = &safepoints[si as usize];
            for (k, &v) in snap.locals.iter().enumerate() {
                uses.record_slot(
                    v,
                    SafepointSlot {
                        snapshot: si,
                        kind: SafepointSlotKind::Local,
                        slot: k as u32,
                    },
                    num_nodes,
                );
            }
            for (k, &v) in snap.stack.iter().enumerate() {
                uses.record_slot(
                    v,
                    SafepointSlot {
                        snapshot: si,
                        kind: SafepointSlotKind::Stack,
                        slot: k as u32,
                    },
                    num_nodes,
                );
            }
            for (k, &v) in snap.monitors.iter().enumerate() {
                uses.record_slot(
                    v,
                    SafepointSlot {
                        snapshot: si,
                        kind: SafepointSlotKind::Monitor,
                        slot: k as u32,
                    },
                    num_nodes,
                );
            }
            for (k, v) in snap.sb_virtual_nodes().enumerate() {
                uses.record_slot(
                    v,
                    SafepointSlot {
                        snapshot: si,
                        kind: SafepointSlotKind::SbVirtual,
                        slot: k as u32,
                    },
                    num_nodes,
                );
            }
            self.uses.sp_len = self.safepoints.len();
            self.uses.stamp();
        }
    }

    /// Drop the snapshots whose `keep` entry is `false`, preserving the order of
    /// the rest. Returns how many were dropped; `0` (and no change) when `keep`
    /// is not one entry per snapshot.
    ///
    /// Every [`Node::frame_snapshot`] is renumbered to its snapshot's new index,
    /// and cleared when that snapshot was dropped — a caller that drops a
    /// snapshot a live node claims has decided nothing reads it. The def-use
    /// edges are rebuilt, since every slot position after the first dropped
    /// snapshot moved.
    ///
    /// Must run before anything records snapshot indices elsewhere; nothing in
    /// the pipeline does before lowering. (`InlineScopeTable` was the one thing
    /// that would have, and it was retired in round 10 wave 9 — see the note
    /// above `MAX_INLINE_SCOPE_DEPTH`.)
    pub fn retain_safepoints(&mut self, keep: &[bool]) -> usize {
        if keep.len() != self.safepoints.len() || keep.iter().all(|&k| k) {
            return 0;
        }
        let mut remap: Vec<Option<u32>> = Vec::with_capacity(keep.len());
        let mut next = 0u32;
        for &k in keep {
            if k {
                remap.push(Some(next));
                next += 1;
            } else {
                remap.push(None);
            }
        }
        let before = self.safepoints.len();
        // `.0`: the lists are rebuilt below whenever tracking is on.
        let old = std::mem::take(&mut self.safepoints.0);
        self.safepoints.0 = old
            .into_iter()
            .zip(keep.iter().copied())
            .filter_map(|(sp, k)| k.then_some(sp))
            .collect();
        for node in self.nodes.iter_mut() {
            if let Some(si) = node.frame_snapshot {
                node.frame_snapshot = remap.get(si as usize).copied().flatten();
            }
        }
        if self.uses.tracking {
            self.rebuild_use_lists();
        }
        before - self.safepoints.len()
    }

    /// Bind `id` to the snapshot at `graph.safepoints[snapshot]` as the frame
    /// describing ITS program point, overriding `ir_lower`'s by-bci scan.
    ///
    /// `false` when `id` is not a node of this graph, or when `snapshot` names
    /// no entry in [`Self::safepoints`] — both are producer bugs, and both are
    /// refused here rather than installed and discovered at a deopt.
    ///
    /// # Fail closed on a bci disagreement
    ///
    /// The snapshot's `bci` must equal the node's `bytecode_pc`. A node whose
    /// frame resumes at a different bytecode index than the node itself sits at
    /// is not a copy identity — it is a mis-substitution, and the interpreter
    /// would resume in the wrong instruction with a frame that looks plausible.
    /// Refused. A node carrying no `bytecode_pc` has no program point to agree
    /// with and is refused too.
    ///
    /// See [`Node::frame_snapshot`] for what this is for.
    pub fn set_node_frame_snapshot(&mut self, id: NodeId, snapshot: u32) -> bool {
        let Some(sp_bci) = self.safepoints.get(snapshot as usize).map(|s| s.bci) else {
            return false;
        };
        let Some(node) = self.nodes.get_mut(id as usize) else {
            return false;
        };
        if node.bytecode_pc != Some(sp_bci) {
            // A node of a spliced callee body (a relocated pc) deopts at its
            // outermost invoke (`ir_lower::resume_bci`), so a frame at THAT bci
            // describes its program point too. Round 11 wave 5
            // (`ir_optimize::install_preheader_deopt_frame`).
            let resumes_there = node.bytecode_pc.is_some_and(|pc| {
                splice_resume_map_this_build().and_then(|m| m.resume_bci(pc)) == Some(sp_bci)
            });
            if !resumes_there {
                return false;
            }
        }
        node.frame_snapshot = Some(snapshot);
        true
    }

    /// Overwrite one snapshot slot. `false` when the snapshot or slot does not
    /// exist.
    ///
    /// Use this rather than `graph.safepoints[i].locals[k] = v` whenever the
    /// new value is a real node id: a direct write that *installs* a reference
    /// is the one edit the def-use edges cannot notice.
    pub fn set_safepoint_slot(
        &mut self,
        snapshot: usize,
        kind: SafepointSlotKind,
        slot: usize,
        new: NodeId,
    ) -> bool {
        let current = self.use_lists_current();
        let num_nodes = self.nodes.len();
        // `.0`: recorded below when the lists are current; otherwise the
        // epoch is bumped below, as `set_input` does.
        let sp = match self.safepoints.0.get_mut(snapshot) {
            Some(sp) => sp,
            None => return false,
        };
        let cell = match kind {
            SafepointSlotKind::Local => sp.locals.get_mut(slot),
            SafepointSlotKind::Stack => sp.stack.get_mut(slot),
            SafepointSlotKind::Monitor => sp.monitors.get_mut(slot),
            SafepointSlotKind::SbVirtual => sp.sb_virtual_slot_mut(slot),
        };
        let cell = match cell {
            Some(c) => c,
            None => return false,
        };
        *cell = new;
        if current {
            self.uses.record_slot(
                new,
                SafepointSlot {
                    snapshot: snapshot as u32,
                    kind,
                    slot: slot as u32,
                },
                num_nodes,
            );
            self.uses.stamp();
        } else {
            // Not current: the lists will be re-derived before anyone trusts
            // them. Bump anyway, as `set_input` does, so no later state that
            // happens to look current again (a length restored by a direct
            // write) can vouch for lists this install never reached.
            bump_edge_epoch();
        }
        // The stale entry left on the previous value is harmless: every rewrite
        // re-checks the slot before touching it (see `UseLists`).
        true
    }

    /// Clear (set to `NO_NODE`) every snapshot slot `clear` selects, without
    /// demoting the def-use lists. Returns how many slots were cleared.
    ///
    /// `clear(slot, bci, value)` is asked about every DEFINED slot (`value !=
    /// NO_NODE`) of every snapshot, locals, stack and monitors alike; `bci` is
    /// the snapshot's. Clearing only ever REMOVES a reference, which the lists
    /// tolerate as a stale superset entry (see [`UseLists`]), so unlike a write
    /// through [`Graph::safepoints`] this does not bump the edge epoch and the
    /// next tracked mutation stays incremental. The dead-slot normalisation in
    /// `ir_optimize::eliminate_dead_nodes` and the liveness prune in
    /// `ea_ir_bridge::ir_prune_dead_snapshot_locals_scoped` are its callers (round 11
    /// wave 18: before [`Safepoints`] they wrote through the public field).
    pub fn clear_safepoint_slots_where(
        &mut self,
        mut clear: impl FnMut(SafepointSlot, usize, NodeId) -> bool,
    ) -> usize {
        let mut cleared = 0usize;
        for (si, sp) in self.safepoints.0.iter_mut().enumerate() {
            let bci = sp.bci;
            let halves: [(SafepointSlotKind, &mut Vec<NodeId>); 3] = [
                (SafepointSlotKind::Local, &mut sp.locals),
                (SafepointSlotKind::Stack, &mut sp.stack),
                (SafepointSlotKind::Monitor, &mut sp.monitors),
            ];
            for (kind, values) in halves {
                for (k, v) in values.iter_mut().enumerate() {
                    if *v == NO_NODE {
                        continue;
                    }
                    let slot = SafepointSlot {
                        snapshot: si as u32,
                        kind,
                        slot: k as u32,
                    };
                    if clear(slot, bci, *v) {
                        *v = NO_NODE;
                        cleared += 1;
                    }
                }
            }
            let mut sb_slot = 0u32;
            for b in &mut sp.sb_virtual_builders {
                for p in &mut b.parts {
                    if let SbVirtualPart::Value { val, .. } = p {
                        if *val != NO_NODE {
                            let slot = SafepointSlot {
                                snapshot: si as u32,
                                kind: SafepointSlotKind::SbVirtual,
                                slot: sb_slot,
                            };
                            if clear(slot, bci, *val) {
                                *val = NO_NODE;
                                cleared += 1;
                            }
                        }
                        sb_slot += 1;
                    }
                }
            }
        }
        cleared
    }

    /// OR `bits` into snapshot `snapshot`'s
    /// [`SafepointSnapshot::monitor_elided_mask`]. `false` when there is no
    /// such snapshot.
    ///
    /// The mask is positional metadata, not a node reference, so this needs no
    /// def-use bookkeeping and does not bump the edge epoch (a write through
    /// [`Graph::safepoints`] would). `ir_optimize::elide_nested_monitors` is the
    /// caller.
    pub fn or_safepoint_monitor_elided_mask(&mut self, snapshot: usize, bits: u64) -> bool {
        match self.safepoints.0.get_mut(snapshot) {
            Some(sp) => {
                sp.monitor_elided_mask |= bits;
                true
            }
            None => false,
        }
    }

    // ── Verification / instrumentation ───────────────────────────────

    /// Re-derive the def-use edges by a full scan and compare them with the
    /// maintained ones.
    ///
    /// `Ok(())` when they agree — or when the lists are not current, in which
    /// case they make no claim and the next mutation re-derives them anyway.
    /// Cheap to call from a test or from `ir_verify`; it allocates two
    /// scratch vectors and touches every edge once.
    ///
    /// Checked:
    /// - the derived vectors are sized to the arena;
    /// - `users[i]` is exactly the multiset of input slots naming `i`
    ///   (so `users[i].len()` is the `use_counts` entry for `i`);
    /// - `sp_users[i]` *contains* every snapshot slot naming `i`. Extra entries
    ///   are tolerated by design — see [`UseLists`] — because a slot cleared
    ///   by [`Graph::clear_safepoint_slots_where`] does not notify the list,
    ///   and a stale entry only ever costs a re-check.
    pub fn verify_use_lists(&self) -> Result<(), String> {
        if !self.use_lists_current() {
            return Ok(());
        }
        let num_nodes = self.nodes.len();
        if self.uses.users.len() != num_nodes || self.uses.sp_users.len() != num_nodes {
            return Err(format!(
                "use lists are sized {} / {} for a {}-node graph",
                self.uses.users.len(),
                self.uses.sp_users.len(),
                num_nodes
            ));
        }

        let mut user_state = vec![0u32; num_nodes];
        let mut out_of_order: rustc_hash::FxHashMap<NodeId, Vec<NodeId>> =
            rustc_hash::FxHashMap::default();
        for (uid, node) in self.nodes.iter().enumerate() {
            let uid = uid as NodeId;
            for &inp in node.inputs.as_slice() {
                if inp != NO_NODE && (inp as usize) < num_nodes {
                    let idx = inp as usize;
                    let st = user_state[idx];
                    if st != u32::MAX {
                        let list = self.uses.users[idx].as_slice();
                        if list.get(st as usize) == Some(&uid) {
                            user_state[idx] = st + 1;
                            continue;
                        }
                        let mut v = Vec::with_capacity(list.len().max(st as usize + 1));
                        v.extend_from_slice(&list[..st as usize]);
                        v.push(uid);
                        out_of_order.insert(inp, v);
                        user_state[idx] = u32::MAX;
                    } else if let Some(v) = out_of_order.get_mut(&inp) {
                        v.push(uid);
                    }
                }
            }
        }
        for (id, &st) in user_state.iter().enumerate() {
            let got_slice = self.uses.users[id].as_slice();
            if st != u32::MAX {
                if st as usize != got_slice.len() {
                    let mut got: Vec<NodeId> = got_slice.to_vec();
                    let mut expect: Vec<NodeId> = got_slice[..st as usize].to_vec();
                    got.sort_unstable();
                    expect.sort_unstable();
                    return Err(format!(
                        "use list for node {id} is {got:?}, the graph says {expect:?}"
                    ));
                }
            } else if let Some(mut expect) = out_of_order.remove(&(id as NodeId)) {
                if got_slice != expect.as_slice() {
                    let mut got: Vec<NodeId> = got_slice.to_vec();
                    got.sort_unstable();
                    expect.sort_unstable();
                    if got != expect {
                        return Err(format!(
                            "use list for node {id} is {got:?}, the graph says {expect:?}"
                        ));
                    }
                }
            }
        }

        let mut sp_cursor = vec![0usize; num_nodes];
        for (si, sp) in self.safepoints.iter().enumerate() {
            let halves = [
                (SafepointSlotKind::Local, &sp.locals),
                (SafepointSlotKind::Stack, &sp.stack),
                (SafepointSlotKind::Monitor, &sp.monitors),
            ];
            for (kind, values) in halves {
                for (k, &v) in values.iter().enumerate() {
                    if v == NO_NODE || (v as usize) >= num_nodes {
                        continue;
                    }
                    let want = SafepointSlot {
                        snapshot: si as u32,
                        kind,
                        slot: k as u32,
                    };
                    let list = self.uses.sp_users[v as usize].as_slice();
                    let cur = &mut sp_cursor[v as usize];
                    let found = if let Some(pos) = list[*cur..].iter().position(|x| *x == want) {
                        *cur += pos + 1;
                        true
                    } else {
                        list[..*cur].contains(&want)
                    };
                    if !found {
                        return Err(format!(
                            "safepoint slot {want:?} names node {v}, but is missing \
                             from its use list"
                        ));
                    }
                }
            }
            for (k, v) in sp.sb_virtual_nodes().enumerate() {
                if v == NO_NODE || (v as usize) >= num_nodes {
                    continue;
                }
                let want = SafepointSlot {
                    snapshot: si as u32,
                    kind: SafepointSlotKind::SbVirtual,
                    slot: k as u32,
                };
                let list = self.uses.sp_users[v as usize].as_slice();
                let cur = &mut sp_cursor[v as usize];
                let found = if let Some(pos) = list[*cur..].iter().position(|x| *x == want) {
                    *cur += pos + 1;
                    true
                } else {
                    list[..*cur].contains(&want)
                };
                if !found {
                    return Err(format!(
                        "safepoint slot {want:?} names node {v}, but is missing from its use list"
                    ));
                }
            }
        }
        Ok(())
    }

    /// Input slots examined by [`Graph::replace_all_uses`] since the counter
    /// was last reset. The measurement hook — see the def-use tests.
    pub fn use_list_slots_examined(&self) -> u64 {
        self.uses.examined
    }

    /// Full rebuilds of the def-use edges since the counter was last reset.
    pub fn use_list_rebuilds(&self) -> u64 {
        self.uses.rebuilds
    }

    /// Zero the instrumentation counters.
    pub fn reset_use_list_stats(&mut self) {
        self.uses.examined = 0;
        self.uses.rebuilds = 0;
    }
}

// ── Memory-effect model ──────────────────────────────────────────────
//
// The IR's only *encoded* ordering relation between two memory operations is
// the memory-token chain: every memory-effecting node takes the previous
// writer as an input edge and hands itself on as the token for the next one.
// That chain is a total order. It is sound — the scheduler's topological sort
// cannot reorder across an input edge — and it is far stronger than the Java
// Memory Model requires: it serialises a load of `a.x` behind a store to
// `b.y`, two reads behind each other, and an `ArrayLength` behind everything.
//
// Every pass that wanted to beat that order had to re-derive, privately, what
// the chain means. `ir_optimize` grew `RefPointsTo` / `resolve_ref_points_to`
// / `loop_store_clobber` for LICM and a second, differently-shaped
// `StoreLoc` / `is_memory_barrier` pair for dead-store elimination; `lib.rs`
// grew a third copy of the token predicate for the escape-analysis bridge;
// `ir_verify` a fourth for its ordering lane. Four private notions of "may
// these two touch the same memory", none of them able to answer the question
// an optimizer actually asks.
//
// This section is that answer, in one place:
//
//   * [`AliasClass`] — *where* a node touches memory: a named field cell, an
//     array element, an array's length word, a static field, an object's
//     monitor, nothing, or anything.
//   * [`MemOrder`] — *how strongly* it is ordered: the JMM acquire/release
//     lattice, so a monitor-enter and a volatile read pin later accesses
//     below them and a monitor-exit and a volatile write pin earlier
//     accesses above them.
//   * [`MemEffect`] — the pair of alias classes a node reads and writes, its
//     [`MemOrder`], and whether it is a safepoint / an allocation.
//   * [`Graph::may_alias`], [`Graph::may_reorder`],
//     [`Graph::may_reorder_effects`] — the query API. The answer is a
//     [`Reorder`], which carries the *reason*: a [`ReorderProof`] naming the
//     fact that licenses the move, or a [`ReorderBlock`] naming the fact that
//     forbids it. A pass records the proof rather than re-deriving it.
//
// **The token chain becomes a consumer of this model, not a rival truth.**
// [`memory_token_slot`] and [`is_memory_token_slot`] — the canonical
// spellings of the predicate that exists in three private copies today — are
// *derived* from the same [`Op::memory_shape`] table the effect
// classification reads, so the two can no longer drift apart. The chain stays
// as the graph's conservative default ordering; [`Graph::may_reorder`] is how
// a pass proves it may deviate from it.
//
// ## What this model deliberately does NOT answer
//
// `may_reorder` is a **memory-side** question. Two other orderings constrain
// node placement and are unaffected by any answer here:
//
//   * **Control dependence.** A node's `ctrl` input pins it to a block. The
//     query refuses outright ([`ReorderBlock::Control`]) when either node is
//     a control node, and it never licenses moving a node across its own
//     control edge.
//   * **Implicit exception order.** `Op::Load` / `Op::Store` /
//     `Op::ArrayLoad` / `Op::ArrayStore` fault on a null base or an
//     out-of-range index and deopt. Whether `a.f = 1; b.g = 2` may be swapped
//     when `b` is null is a question about the *exception*, not about the
//     memory: the two stores provably touch disjoint cells, and it is the
//     control/exception edge that keeps them in order. That is why those four
//     ops are **not** flagged [`MemEffect::safepoint`] — flagging them would
//     make the model answer "no" to every question and it would be answering
//     the wrong one. A scheduler must combine this answer with control
//     dependence; it must not use it alone.
//
// The query is also **pairwise**: `may_reorder(a, b)` says nothing about a
// third node between them. Moving `b` above `a` past an intervening `c`
// requires the query to hold for `(c, b)` too.

/// The offset operand of a memory access — the second half of an
/// [`AliasClass`]'s address, after the base reference.
///
/// Two accesses to the same base are disjoint exactly when their offsets are
/// two *different compile-time constants*. Every other combination is
/// "possibly the same cell": two runtime values may be equal, and a runtime
/// value may equal any constant. Note that [`AccessOffset::Dynamic`] naming
/// the *same* node twice is a must-alias, not a disjointness — the same node
/// computes the same value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AccessOffset {
    /// A compile-time constant: a field index (`Op::Load` / `Op::Store`) or a
    /// constant-folded array index.
    Const(i64),
    /// A runtime value produced by this node.
    Dynamic(NodeId),
    /// The layout carries no offset operand at all — the compact
    /// `[base, value]` `Store` and `[base]` `Load` forms the EA bridge and the
    /// hand-built test graphs use. Treated as "the object's storage", so it is
    /// never disjoint from another access to the same base.
    Absent,
}

impl AccessOffset {
    /// True when these two offsets provably name different cells.
    ///
    /// The whole precision of the model lives in this one line, so it is
    /// stated positively: disjointness must be *proved*, and only a pair of
    /// unequal constants proves it.
    pub fn provably_distinct(self, other: AccessOffset) -> bool {
        match (self, other) {
            (AccessOffset::Const(a), AccessOffset::Const(b)) => a != b,
            _ => false,
        }
    }
}

/// Where in memory a node reads or writes.
///
/// The classes are *storage kinds*, and different kinds never overlap: an
/// instance field cell is not an array element, an array's length word is not
/// a field cell, a class's static storage is not any object's instance
/// storage, and an object's monitor is not any of them. Those cross-kind
/// disjointness claims are the model's premises, and each is recorded at its
/// arm in [`Graph::may_alias`] so a future op that violates one is caught at
/// the place that would silently start returning the wrong answer.
///
/// [`AliasClass::Any`] is the top of the lattice ("could be anywhere") and
/// [`AliasClass::None`] the bottom ("touches no memory"). An unreadable node
/// layout always degrades to `Any`, never to `None`: the failure direction has
/// to be the one that refuses optimizations, not the one that licenses them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AliasClass {
    /// No memory at all. The bottom of the lattice — aliases nothing, not even
    /// itself.
    None,
    /// The instance field cell `base.#offset`. `Op::Load` / `Op::Store`.
    Field {
        /// The object reference.
        base: NodeId,
        /// The field index (constant in the builder-emitted form).
        offset: AccessOffset,
        /// Round 15 wave 3 (lane chain2, CHN15-2): the field's DECLARING
        /// class id, when the resolver named it
        /// ([`Graph::field_decl_class`]). Only [`Graph::memory_effect`] fills
        /// it (and only under [`Graph::field_identity_alias`]);
        /// [`effect_of_node`] alone always answers `None`. See the premise in
        /// [`Graph::may_alias`].
        decl: Option<u32>,
    },
    /// The array element `array[index]`. `Op::ArrayLoad` / `Op::ArrayStore`.
    ArrayElem {
        /// The array reference.
        array: NodeId,
        /// The element index — usually [`AccessOffset::Dynamic`].
        index: AccessOffset,
        /// The element **type**, read off the accessing op itself
        /// (`Op::ArrayLoad` / `Op::ArrayStore` each carry their [`MemKind`])
        /// by [`access_location`]. A layout that does not yield one degrades
        /// the whole class to [`AliasClass::Any`].
        ///
        /// It lives in the class rather than alongside it because a consumer
        /// that carries its own copy is trusting its *producer* for a fact the
        /// memory model already knows. The deleted `x64::simd_analysis`
        /// `vector_gate` refused a vector access to [`MemKind::Ref`] elements, because a
        /// vector store of oops bypasses the GC write barrier; while that
        /// refusal read a caller-supplied `MemKind`, the barrier elision was
        /// only as sound as whoever filled the field in. Reading it here makes
        /// the refusal unforgeable.
        ///
        /// It also buys precision: a Java array's component type is fixed at
        /// allocation, so two accesses at *different* element kinds cannot be
        /// the same object — see [`Graph::may_alias`].
        elem: MemKind,
    },
    /// An array's length word. `Op::ArrayLength`.
    ///
    /// Its own class because array length is **immutable**: nothing in the JVM
    /// writes it after the allocation that produced it, so no store — not even
    /// an opaque call — can clobber it. That is what lets the model prove an
    /// `ArrayLength` commutes with a store, which the token chain (where it
    /// sits between two writers) cannot.
    ArrayLength {
        /// The array reference.
        array: NodeId,
    },
    /// A class's static field cell.
    ///
    /// Keyed by `(class_id, field)` rather than by a base node, because static
    /// storage has no reference operand to resolve — which makes static
    /// accesses the one class the model can disambiguate *exactly*. Produced
    /// only by [`Graph::memory_effect`] for a DIRECT static access
    /// ([`Graph::direct_static_access`], JIT round 19 wave 2, IROPS19-1,
    /// under `CRATONVM_JIT_IR_STATIC_ALIAS_CLASS`); every other `getstatic` /
    /// `putstatic` is a helper route that can run `<clinit>` and stays
    /// `MemAccess::Opaque`.
    Static {
        /// Declaring class.
        class_id: u32,
        /// Field index within the class's static storage.
        field: u32,
    },
    /// An object's monitor (lock word). [`Op::MonitorEnter`] /
    /// [`Op::MonitorExit`].
    ///
    /// Aliasing is only half the story for a monitor: its *ordering* comes
    /// from [`MemOrder`], not from this class. Two monitor operations on
    /// provably distinct objects do not alias, and they still do not commute
    /// with the accesses between them, because monitor-enter is an acquire and
    /// monitor-exit a release.
    Monitor {
        /// The locked object.
        obj: NodeId,
    },
    /// RAW memory: the payload of SOME primitive array, or off-heap memory
    /// at an untagged real address. Round nio20 wave 4 (lane L1, W3L4-1 of
    /// `jit-r20-nio-proposals-w3-L4.md`): the class of the `ScopedMemoryAccess`
    /// fold ([`UnboxOp::ScopedGet`] / [`UnboxOp::ScopedPut`]), and only that,
    /// only under [`Graph::scoped_raw_alias`] ([`Graph::memory_effect`] /
    /// [`scoped_raw_effect`]).
    ///
    /// No base and no offset: a raw access addresses `base + offset` with a
    /// computed `offset`, so the model cannot say WHICH primitive array or
    /// which element. What it can say is what the lowering
    /// (`ir_lower::Lowerer::lower_scoped_raw`) admits: either a
    /// one-dimensional PRIMITIVE array whose payload holds the whole access
    /// (anything else deopts before the access), or `base == null` with an
    /// address that is not an `Unsafe` arena handle, or (nio21 wave 1 L3) a
    /// live arena block resolved through the JIT-readable table -- a Rust-side
    /// `Vec<u8>`, never the Java heap. So it aliases itself,
    /// every non-reference [`AliasClass::ArrayElem`], and [`AliasClass::Any`]
    /// -- and NOTHING else: no instance field (a field is never inside a
    /// primitive array's payload, and off-heap memory is not the Java heap),
    /// no array length (immutable, and outside the payload), no reference
    /// element (a reference array deopts), no static (statics live in a
    /// Rust-side table), no monitor (the lock word is header, not payload).
    /// This is the premise HotSpot's C2 makes for its raw memory slice; an
    /// FFM segment built over an arbitrary address the program forged is the
    /// one way to break it, and C2 does not defend against that either.
    Raw,
    /// Anywhere. The top of the lattice: aliases everything except
    /// [`AliasClass::None`].
    Any,
}

impl AliasClass {
    /// True when this names no memory at all.
    pub fn is_none(self) -> bool {
        matches!(self, AliasClass::None)
    }

    /// True when this names memory (anything but [`AliasClass::None`]).
    pub fn is_some(self) -> bool {
        !self.is_none()
    }

    /// The base reference operand, when the class has one.
    ///
    /// [`AliasClass::Static`] has no base by construction, `None` / `Any`
    /// are not addressed by a reference, and [`AliasClass::Raw`] names no ONE
    /// object (its base operand may be null, and its address is computed).
    pub fn base(self) -> Option<NodeId> {
        match self {
            AliasClass::Field { base, .. } => Some(base),
            AliasClass::ArrayElem { array, .. } => Some(array),
            AliasClass::ArrayLength { array } => Some(array),
            AliasClass::Monitor { obj } => Some(obj),
            AliasClass::None | AliasClass::Static { .. } | AliasClass::Raw | AliasClass::Any => {
                Option::None
            }
        }
    }
}

/// The Java Memory Model ordering strength of a node.
///
/// The lattice is the two independent fence halves:
///
/// ```text
///            SeqCst          (both — a volatile write)
///           /      \
///      Acquire    Release    (monitor-enter / volatile read,
///           \      /          monitor-exit / volatile write)
///            Plain           (an ordinary load or store)
/// ```
///
/// Read the two halves as motion bans, which is what the query needs:
///
/// * **Acquire** on the *earlier* node — nothing after it may move above it.
/// * **Release** on the *later* node — nothing before it may move below it.
///
/// That asymmetry is the JMM's "roach motel": accesses may move *into* a
/// synchronized region from either end, never *out* of it. It is what makes
/// `monitorenter … monitorexit` bound the operations between them without
/// pinning the operations outside them.
///
/// A volatile **read** is `Acquire`. A volatile **write** is
/// [`MemOrder::SeqCst`] rather than a bare `Release`: the JMM requires
/// sequential consistency across volatile accesses, which HotSpot implements
/// with a `StoreLoad` fence after the write, so it is a barrier in both
/// directions. Being stronger than a bare release here is the conservative
/// direction and it is what the acceptance test pins.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MemOrder {
    /// No ordering of its own — the ordinary case.
    Plain,
    /// Acquire: no later access may move above this node.
    Acquire,
    /// Release: no earlier access may move below this node.
    Release,
    /// A full fence: both halves.
    SeqCst,
}

impl MemOrder {
    /// True when nothing after this node may move above it.
    pub fn has_acquire(self) -> bool {
        matches!(self, MemOrder::Acquire | MemOrder::SeqCst)
    }

    /// True when nothing before this node may move below it.
    pub fn has_release(self) -> bool {
        matches!(self, MemOrder::Release | MemOrder::SeqCst)
    }

    /// True for a full (bidirectional) fence.
    pub fn is_fence(self) -> bool {
        matches!(self, MemOrder::SeqCst)
    }

    /// True when this imposes no ordering of its own.
    pub fn is_plain(self) -> bool {
        matches!(self, MemOrder::Plain)
    }

    /// Least upper bound: the strength of a node that does both.
    pub fn join(self, other: MemOrder) -> MemOrder {
        match (
            self.has_acquire() || other.has_acquire(),
            self.has_release() || other.has_release(),
        ) {
            (true, true) => MemOrder::SeqCst,
            (true, false) => MemOrder::Acquire,
            (false, true) => MemOrder::Release,
            (false, false) => MemOrder::Plain,
        }
    }
}

/// The complete memory effect of one node: what it reads, what it writes, how
/// strongly it is ordered, and whether it is a safepoint or an allocation.
///
/// Reads and writes are kept as two separate [`AliasClass`]es rather than one
/// class plus a read/write flag, because the interesting nodes do both to
/// *different* places — and because a read-read pair commutes regardless of
/// aliasing, which a merged representation cannot express.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MemEffect {
    /// Locations this node may read.
    pub reads: AliasClass,
    /// Locations this node may write.
    pub writes: AliasClass,
    /// JMM ordering strength — see [`MemOrder`].
    pub order: MemOrder,
    /// This node is a safepoint: control may leave for the runtime here and
    /// an interpreter frame may be rebuilt from it (`Op::Call`, `Op::New`,
    /// `Op::NewArray`, `Op::Guard`).
    ///
    /// The constraint that follows is specifically about **writes**: every
    /// store the method has performed must have happened before the frame is
    /// rebuilt, and no store it has not yet performed may have. Loads may
    /// cross a safepoint freely — a load has no visible effect, and re-running
    /// one after a deopt reproduces it. Two safepoints may cross each other
    /// unless one of them allocates.
    ///
    /// See the section header for why the implicit null/bounds faults of
    /// `Op::Load` and friends are *not* modelled here.
    pub safepoint: bool,
    /// This node may allocate (`Op::New`, `Op::NewArray`, and any call, whose
    /// callee may). Two allocating nodes do not commute: which one runs first
    /// is observable through `OutOfMemoryError` and through the addresses and
    /// identity hashes the collector hands out.
    pub allocates: bool,
}

impl MemEffect {
    /// The effect of a node that touches no memory and imposes no ordering.
    pub const NONE: MemEffect = MemEffect {
        reads: AliasClass::None,
        writes: AliasClass::None,
        order: MemOrder::Plain,
        safepoint: false,
        allocates: false,
    };

    /// Reads and writes anything, is a safepoint, and may allocate — the
    /// effect of a call whose callee is unknown, and the fallback for any node
    /// whose layout could not be read. The top of the effect lattice.
    pub const OPAQUE: MemEffect = MemEffect {
        reads: AliasClass::Any,
        writes: AliasClass::Any,
        order: MemOrder::Plain,
        safepoint: true,
        allocates: true,
    };

    /// A plain read of one location.
    pub fn read(class: AliasClass) -> MemEffect {
        MemEffect {
            reads: class,
            ..MemEffect::NONE
        }
    }

    /// A plain write of one location.
    pub fn write(class: AliasClass) -> MemEffect {
        MemEffect {
            writes: class,
            ..MemEffect::NONE
        }
    }

    /// A fresh allocation: writes only storage nothing else can yet name, but
    /// is a safepoint and orders against other allocations.
    pub fn allocation() -> MemEffect {
        MemEffect {
            safepoint: true,
            allocates: true,
            ..MemEffect::NONE
        }
    }

    /// `monitorenter` on `obj` — an **acquire**.
    ///
    /// [`Op::MonitorEnter`] is the producing op, and [`effect_of_node`]
    /// reaches this through [`MemEffect::monitor_enter_of`] after reading the
    /// locked reference out of the node's `[ctrl, mem, obj]` edge list. This
    /// entry point stays because a caller holding a reference and no node —
    /// a test, or a pass reasoning about an effect it has not built yet — is
    /// the case the ordering discipline was first stated for.
    ///
    /// Both halves of the effect are deliberate. It **reads and writes** the
    /// monitor, so two enters of the same object never commute. It is a
    /// **safepoint**, because the runtime may block here and an interpreter
    /// frame may be rebuilt from it — which is also why a coarsened lock
    /// region moves deopt points (`docs/jit/lock-elimination.md` §3).
    pub fn monitor_enter(obj: NodeId) -> MemEffect {
        MemEffect::monitor_enter_of(AliasClass::Monitor { obj })
    }

    /// `monitorexit` on `obj` — a **release**. See [`MemEffect::monitor_enter`].
    pub fn monitor_exit(obj: NodeId) -> MemEffect {
        MemEffect::monitor_exit_of(AliasClass::Monitor { obj })
    }

    /// [`MemEffect::monitor_enter`] over an already-built class.
    ///
    /// [`effect_of_node`] resolves the locked reference out of the edge list
    /// and hands the class straight over, so an unreadable layout arrives here
    /// as [`AliasClass::Any`] — the refusing direction — instead of being
    /// silently rebuilt around a wrong node id.
    pub fn monitor_enter_of(class: AliasClass) -> MemEffect {
        MemEffect {
            reads: class,
            writes: class,
            order: MemOrder::Acquire,
            safepoint: true,
            allocates: false,
        }
    }

    /// [`MemEffect::monitor_exit`] over an already-built class. See
    /// [`MemEffect::monitor_enter_of`].
    pub fn monitor_exit_of(class: AliasClass) -> MemEffect {
        MemEffect {
            reads: class,
            writes: class,
            order: MemOrder::Release,
            safepoint: true,
            allocates: false,
        }
    }

    /// A volatile read of `class` — an **acquire**. Produced by
    /// [`effect_of_node`] for an `Op::Load` marked [`Node::volatile_access`]
    /// (round 9 wave 6); still callable directly for a caller holding an
    /// effect no node produces.
    pub fn volatile_read(class: AliasClass) -> MemEffect {
        MemEffect {
            reads: class,
            writes: AliasClass::None,
            order: MemOrder::Acquire,
            safepoint: false,
            allocates: false,
        }
    }

    /// A volatile write of `class` — a **full fence**, so a barrier in both
    /// directions. See [`MemOrder`] for why this is `SeqCst` and not a bare
    /// release.
    pub fn volatile_write(class: AliasClass) -> MemEffect {
        MemEffect {
            reads: AliasClass::None,
            writes: class,
            order: MemOrder::SeqCst,
            safepoint: false,
            allocates: false,
        }
    }

    /// True when this node reads or writes memory.
    pub fn touches_memory(self) -> bool {
        self.reads.is_some() || self.writes.is_some()
    }

    /// True when this node writes memory.
    pub fn is_write(self) -> bool {
        self.writes.is_some()
    }

    /// True when this node constrains motion in *no* way — no memory, no
    /// fence, no safepoint, no allocation. Such a node commutes with anything.
    pub fn is_inert(self) -> bool {
        !self.touches_memory() && self.order.is_plain() && !self.safepoint && !self.allocates
    }
}

/// What kind of memory access an op performs — the `access` half of
/// [`OpMemoryShape`], and the discriminator [`access_location`] keys off when
/// it reads the base/offset operands out of a node's edge list.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MemAccess {
    /// Reads one instance field cell.
    FieldRead,
    /// Writes one instance field cell.
    FieldWrite,
    /// Reads one array element.
    ArrayRead,
    /// Writes one array element.
    ArrayWrite,
    /// Reads an array's (immutable) length word.
    LengthRead,
    /// Allocates fresh storage.
    Allocate,
    /// Enters an object's monitor — an acquire, and a safepoint.
    MonitorEnter,
    /// Exits an object's monitor — a release, and a safepoint.
    MonitorExit,
    /// Reads and writes anything.
    Opaque,
    /// JIT r21 wave 2 (lane typecheck, TYPECHECK21-1 part A): reads only the
    /// IMMUTABLE header words of its operand (class id, kind byte) -- an
    /// `Op::InstanceOf` with `header_only`. No store can change what it
    /// reads ([`AliasClass::None`]), it writes nothing, and it is NOT an
    /// `ArrayLength`: no `LengthRead` consumer (the length forwarding of a
    /// fresh `NewArray`) may see it. Its slow path calls `jit_instanceof`,
    /// which may collect, so its effect is still a safepoint
    /// ([`effect_of_node`]); the register allocator keeps calling it a call
    /// and a safepoint (`regalloc::ir_op_is_call` / `ir_op_is_safepoint`).
    HeaderRead,
}

/// Static description of a memory-effecting op: where its incoming memory
/// token sits, what distinguishes the documented full layout from the compact
/// ones, and what it does to memory.
///
/// **This is the single table.** [`memory_token_slot`] reads it to answer
/// "which slot is the token", and [`effect_of_node`] reads it to answer "what
/// does this node touch". Before this existed the two questions had separate
/// tables in separate files (three copies of the first, four private notions
/// of the second), which is how they drifted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct OpMemoryShape {
    /// Input slot carrying the incoming memory token in the documented
    /// `[ctrl, mem, …]` form. Always 1 today; named rather than assumed.
    pub token_slot: usize,
    /// Minimum input count of the documented full form.
    ///
    /// The arity guard is what distinguishes that form from the compact
    /// hand-built / EA-bridge layouts `ir_optimize::store_operands` and
    /// `ir_optimize::load_base` also accept — a compact `Store` is
    /// `[base, value]`, whose slot 1 is a *value*, not a token. Below this
    /// arity the node has no memory token at all.
    pub min_full_arity: usize,
    /// What the op does to memory.
    pub access: MemAccess,
}

impl Op {
    /// The memory shape of this op, or `None` when it consumes no memory token
    /// in the documented `[ctrl, mem, …]` layout.
    ///
    /// `None` here is **not** "touches no memory": a memory-typed `Op::Phi`
    /// consumes a token from every predecessor rather than at one fixed slot,
    /// and `Op::Return` ends the method. Both are classified directly by
    /// [`effect_of_node`] / [`is_memory_token_slot`]. This table answers the
    /// narrower question "does this op have a single token slot, and where".
    ///
    /// This is the ONE arity table. `ir_optimize::memory_token_slot` /
    /// `is_memory_token_slot`, `ea_ir_bridge::ea_memory_token_slot` /
    /// `ea_is_memory_token_slot` and `ir_verify::is_memory_token_input` all
    /// delegate to [`memory_token_slot`] / [`is_memory_token_slot`], which read
    /// it; none keeps a copy to reconcile.
    pub fn memory_shape(&self) -> Option<OpMemoryShape> {
        let (min_full_arity, access) = match self {
            Op::Load(_) => (3, MemAccess::FieldRead), // [ctrl, mem, base]
            Op::Store(_) => (4, MemAccess::FieldWrite), // [ctrl, mem, base, value]
            Op::ArrayLoad(_) => (4, MemAccess::ArrayRead), // [ctrl, mem, array, index]
            Op::ArrayStore(_) => (5, MemAccess::ArrayWrite), // [ctrl, mem, array, index, value]
            Op::ByteArrayCopy => (7, MemAccess::ArrayWrite), // [ctrl, mem, src, src_pos, dst, dst_pos, len]
            Op::ArrayLength => (3, MemAccess::LengthRead),   // [ctrl, mem, array_ref]
            Op::New { .. } => (2, MemAccess::Allocate),      // [ctrl, mem]
            Op::NewArray { .. } => (3, MemAccess::Allocate), // [ctrl, mem, length]
            Op::Call { .. } => (2, MemAccess::Opaque),       // [ctrl, mem, args…]
            // cov-01. Each is a call to a runtime helper that can intern, load
            // a class, or run `<clinit>` — i.e. arbitrary Java. `Opaque` is the
            // top of the effect lattice (reads Any, writes Any, safepoint,
            // allocates), which is the same classification `Op::Call` gets and
            // the only defensible one for a node whose callee is a class
            // initialiser.
            Op::ConstString { .. } | Op::ConstClass { .. } => (2, MemAccess::Opaque), // [ctrl, mem]
            Op::LoadStatic { .. } => (2, MemAccess::Opaque),                          // [ctrl, mem]
            // `putstatic`: the same helper family, which can run `<clinit>`.
            // `Opaque` also covers a volatile write's full fence, and keeps
            // `access_location` from reading the VALUE in slot 2 as a base.
            Op::StoreStatic { .. } => (3, MemAccess::Opaque), // [ctrl, mem, value]
            // cov-05: same Opaque classification as the three above, for the
            // same reason — the helper it calls can allocate a Class mirror.
            // JIT r21 wave 2 (lane typecheck, TYPECHECK21-1 part A): a
            // header-only test reads the object's class id and kind byte,
            // which never change for its life; see `MemAccess::HeaderRead`.
            Op::InstanceOf {
                header_only: true, ..
            } => (3, MemAccess::HeaderRead), // [ctrl, mem, obj]
            Op::InstanceOf { .. } => (3, MemAccess::Opaque), // [ctrl, mem, obj]
            // cov-05: same reasoning, plus the helper can stash a pending
            // ClassCastException and return the deopt/exception sentinel.
            Op::CheckCast { .. } => (3, MemAccess::Opaque), // [ctrl, mem, obj]
            // cov-07: same Opaque classification — `jit_throw_exception` can
            // allocate an NPE (a null athrow), touches thread-local pending-
            // exception state, and is a safepoint.
            Op::Throw => (3, MemAccess::Opaque), // [ctrl, mem, exc]
            Op::LambdaIntToDouble => (4, MemAccess::Opaque), // [ctrl, mem, lambda, index]
            // Round 16 (lane indy): a bridged `invokedynamic` runs a bootstrap
            // or an operand `toString()` -- arbitrary Java, like `Op::Call`.
            Op::IndyBridge { .. } => (2, MemAccess::Opaque), // [ctrl, mem, args…]
            // Round 16 (lane matrix): `multianewarray` resolves its per-level
            // array classes, which can run a user class loader, and allocates.
            // `[ctrl, mem, dims…]`.
            Op::MultiNewArray { .. } => (2, MemAccess::Opaque),
            // nio21 w15 L3: a deferred `new` resolves its class (a user class
            // loader, `<clinit>`) and allocates. `[ctrl, mem]`.
            Op::NewCp { .. } => (2, MemAccess::Opaque),
            Op::MonitorEnter => (3, MemAccess::MonitorEnter), // [ctrl, mem, obj]
            // The guarded slot-0 accessors. `Opaque` -- the top of the lattice --
            // rather than the `FieldRead`/`FieldWrite` pair the shape suggests:
            // `access_location` reads a `(base, offset)` out of input slots 2
            // and 3, and this node's slot 3 is a DELTA, not an offset (the
            // offsets it uses are resolved from `class_id` at lowering and are
            // carried by no edge at all). Three of the six are a `LOCK XADD` on
            // a VOLATILE field, whose ordering is a full fence anyway, which is
            // what `Opaque` already means here.
            //
            // Added 2026-09-08 with the `Atomic*` families. Before them the op
            // had no entry at all, which was survivable for a pure load and is
            // not for a read-modify-write.
            Op::Unbox { .. } => (3, MemAccess::Opaque), // [ctrl, mem, receiver]
            Op::MonitorExit => (3, MemAccess::MonitorExit), // [ctrl, mem, obj]
            // JIT round 21 (lane charat, STRINTR21-5): an element read of the
            // `byte[]` `value`, but at a CHAR index, so `access_location` gives
            // it an `Absent` index: a byte-array read never matched against a
            // byte access's cell. `[ctrl, mem, value, index, coder]`.
            Op::StrCharDecode => (5, MemAccess::ArrayRead),
            _ => return None,
        };
        Some(OpMemoryShape {
            token_slot: 1,
            min_full_arity,
            access,
        })
    }
}

/// The input slot carrying `node`'s incoming memory token, or `None` when this
/// op does not consume one.
///
/// **The canonical spelling.** `ir_optimize::memory_token_slot`,
/// `ea_ir_bridge::ea_memory_token_slot` and `ir_verify::is_memory_token_input`
/// used to be three hand-reconciled copies of this predicate; each now
/// delegates here (the verifier and the φ-aware forms through
/// [`is_memory_token_slot`]), so there is no second table to keep in step.
///
/// Derived from [`Op::memory_shape`], so the token convention and the effect
/// classification cannot disagree about which ops are memory operations.
pub fn memory_token_slot(node: &Node) -> Option<usize> {
    let shape = node.op.memory_shape()?;
    if node.inputs.len() >= shape.min_full_arity {
        Some(shape.token_slot)
    } else {
        None
    }
}

/// True when input `idx` of `node` is a memory *token* — an ordering edge
/// naming the previous writer — rather than a value the node reads.
///
/// A memory-typed φ is the one op whose token edges are not a single slot:
/// *every* value input (slots 1.., past the control anchor) is a token from
/// one predecessor.
pub fn is_memory_token_slot(node: &Node, idx: usize) -> bool {
    if matches!(node.op, Op::Phi) {
        return node.ty == IrType::Memory && idx >= 1;
    }
    memory_token_slot(node) == Some(idx)
}

// ── The guard token edge ─────────────────────────────────────────────
//
// `Op::Guard` carries `[ctrl, cond]` and shares its control input with the
// nodes it licenses, so "the guard ran before this load" has always been a
// fact about the ORDER INSIDE ONE BLOCK that no edge expressed. Every
// predicate in `ir_optimize` that reasons about a guard —
// `control_token_has_guard`, `licm_hoist_barrier`'s guard arm, `LoopGuards`'
// cone attribution — exists because of that gap.
//
// The token edge closes it: a node the guard licenses names the guard as a
// trailing input, exactly the way a memory operation names its previous
// writer. `Op::Guard` is `IrType::Void` and defines no value, so the slot is
// an ORDERING edge and nothing else — `regalloc::build_live_model` skips it
// (the guard is absent from `ir_op_defines_value`, so `wants_loc` is false)
// and `ir_lower::verify_data_locations` skips it (`is_value_ty(Void)` is
// false). Neither needs a guard-shaped special case; both were checked
// against the code rather than assumed.
//
// **One table, like `Op::memory_shape`.** The slot is identified by ARITY,
// which is what lets the three consumers that must know about it —
// `expected_input_type`, `ir_verify::expected_arity` and
// `ir_verify::data_input_indices` — read this instead of holding three
// opinions. The third is the one a hand-written version of this change
// forgets: `data_input_indices` classifies slots `2..` of a `Load` and ALL
// slots of a `Div` as data, and `check_types` reports a `Void` node in a data
// slot in every build profile, so an unfiltered guard slot would make
// `ir_verify_reject` true for every method containing a guard.

/// Static description of an op that may carry a trailing `Op::Guard` token.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct OpGuardShape {
    /// Input count of the documented form WITHOUT the guard token. The token,
    /// when present, sits at exactly this index and is the last input.
    pub unguarded_arity: usize,
}

impl Op {
    /// The guard-token shape of this op, or `None` when nothing ever licenses
    /// one of these with a guard.
    ///
    /// The members are the guarded families the builder actually emits:
    ///
    /// * `Op::Load` / `Op::ArrayLoad` / `Op::ArrayLength` — the IR `String`
    ///   expansion's receiver-null, `value`-null and bounds guards;
    /// * `Op::Div` / `Op::Rem` — `IrBuilder::add_div_zero_guard`.
    ///
    /// `Op::Store` / `Op::ArrayStore` are deliberately absent: nothing licenses
    /// a store with a guard today, and giving them a shape would put an
    /// optional slot immediately after the one `access_location` and
    /// `ir_optimize::store_operands` read the stored VALUE from — the one
    /// place where an extra trailing edge really would be ambiguous.
    ///
    /// # Why the arities below cannot collide with an existing form
    ///
    /// Each is one past the widest arity the op already has: `Op::Load`'s
    /// documented form is `[ctrl, mem, base, offset]` (4) and its compact
    /// forms are 1..=3; `Op::ArrayLoad`'s is `[ctrl, mem, array, index]` (4);
    /// `Op::ArrayLength`'s is `[ctrl, mem, array]` (3), with the hand-built
    /// `[array]` at 1; `Op::Div`/`Op::Rem` are `[a, b]` (2) and have no other
    /// form at all. So "arity == unguarded_arity + 1" names the guarded shape
    /// and nothing else.
    pub fn guard_shape(&self) -> Option<OpGuardShape> {
        let unguarded_arity = match self {
            // [ctrl, mem, base, offset]
            Op::Load(_) => 4,
            // [ctrl, mem, array, index]
            Op::ArrayLoad(_) => 4,
            // [ctrl, mem, array]
            Op::ArrayLength => 3,
            // [ctrl, mem, value, index, coder] (STRINTR21-5): the bounds guard.
            Op::StrCharDecode => 5,
            // [a, b] — a floating data node with no control edge of its own.
            Op::Div | Op::Rem => 2,
            _ => return None,
        };
        Some(OpGuardShape { unguarded_arity })
    }
}

/// The input slot carrying `op`'s trailing `Op::Guard` token at this `arity`,
/// or `None` when this shape carries none.
///
/// The arity-keyed core of [`guard_token_slot`], split out because
/// [`expected_input_type`] is handed `(op, idx, arity)` and never sees a
/// [`Node`].
pub fn guard_token_slot_for(op: &Op, arity: usize) -> Option<usize> {
    let shape = op.guard_shape()?;
    (arity == shape.unguarded_arity + 1).then_some(shape.unguarded_arity)
}

/// The input slot carrying `node`'s trailing `Op::Guard` token, or `None`.
///
/// The companion of [`memory_token_slot`], and read the same way: it answers
/// *where the token is*, not *whether the edge is well-formed*.
/// `ir_verify::check_guard_tokens` is what checks the slot really names a live
/// `Op::Guard`, so a producer that puts a value there is a reported violation
/// rather than a silently-ignored operand.
pub fn guard_token_slot(node: &Node) -> Option<usize> {
    guard_token_slot_for(&node.op, node.inputs.len())
}

/// The `Op::Guard` `node` names as its licensing guard, or [`NO_NODE`].
pub fn guard_token_of(node: &Node) -> NodeId {
    match guard_token_slot(node) {
        Some(slot) => node.inputs.get(slot).copied().unwrap_or(NO_NODE),
        None => NO_NODE,
    }
}

/// `CRATONVM_JIT_IR_GUARD_TOKEN=1` — emit the guard token edge from the
/// builder.
///
/// **Default OFF.** The consumer side (the table above, the verifier's three
/// lanes, `ir_optimize::LoopGuards::permits_hoist`'s use of the token) is
/// complete and correct at either setting; this gate decides only whether
/// [`IrBuilder`] actually appends the edge. It is off because the failure mode
/// of a modelling gap here is neither a crash nor a failing test: a lane that
/// rejects the new shape makes `ir_verify_reject` answer `true`, the method
/// falls back to the single-pass backend, and the optimizing tier goes quiet
/// on every method containing a guard. Turn it on, soak, then flip the
/// default — see `NOTES-opts8.md` §1.
///
/// Memoised per override scope: the `String` expansion and every `idiv` consult it.
pub fn guard_token_edges_enabled() -> bool {
    static ENABLED: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    crate::memo_flag::memo_bool(&ENABLED, || false)
}

/// Append `guard` to a node's edge list when the token edge is enabled.
///
/// The one spelling of the producer side: a site that forgets the gate is a
/// site that does not compile.
pub fn with_guard_token(mut inputs: Vec<NodeId>, guard: NodeId) -> Vec<NodeId> {
    if guard != NO_NODE && guard_token_edges_enabled() {
        inputs.push(guard);
    }
    inputs
}

/// Append `guard` to a node's edge list ALWAYS (when it is a guard): the
/// producer for an access an optimizer pass places in a block beside the guard
/// that licenses it, where the scheduler would otherwise be free to float the
/// access above the guard.
///
/// JIT r21/r22 LOOPS21-4 / LOOPS22-5 close: the loop field forward's
/// pre-header read of a possibly-null base sat beside its
/// `Guard(base != null)` ordered only positionally, and was scheduled first:
/// `R20ReviewForwardNullBase` threw the NPE its loop never raises
/// (`drive:22`) under `CRATONVM_JIT_IR_LOOP_FIELD_FORWARD_NULL_GUARD=1`, and
/// printed HotSpot's facts once `CRATONVM_JIT_IR_GUARD_TOKEN=1` made the order
/// an edge. [`with_guard_token`]'s gate decides only whether the BUILDER's own
/// guarded families carry the edge; every consumer reads it at either setting
/// (the verifier's lanes, the scheduler, `ir_optimize::LoopGuards`, the
/// lowering's guarded arities), as LOOPOPT20-2's tokened masked-load copies
/// have relied on since JIT round 20. The node must be a guard-shaped op
/// ([`Op::guard_shape`]) at its unguarded arity.
pub fn with_licensing_guard(mut inputs: Vec<NodeId>, guard: NodeId) -> Vec<NodeId> {
    if guard != NO_NODE {
        inputs.push(guard);
    }
    inputs
}

/// Read the `(base, offset)` a memory access addresses, out of whichever edge
/// layout the node is in.
///
/// Mirrors `ir_optimize::store_operands` / `ir_optimize::load_base` exactly,
/// including their tolerance for the compact hand-built and EA-bridge forms.
/// An unreadable layout returns [`AliasClass::Any`] — the direction that
/// refuses optimizations.
///
/// Offsets come back as [`AccessOffset::Dynamic`]; [`Graph::memory_effect`]
/// constant-folds them, which is where the model gets the precision that lets
/// two field stores commute.
pub(in crate::ir) fn access_location(node: &Node, access: MemAccess) -> AliasClass {
    let inputs = node.inputs.as_slice();
    let dyn_at = |i: usize| match inputs.get(i).copied().and_then(node_id_opt) {
        Some(id) => AccessOffset::Dynamic(id),
        None => AccessOffset::Absent,
    };
    match access {
        // `load_base`: compact `[base]` / `[base, index]`, full
        // `[ctrl, mem, base, index?]`.
        MemAccess::FieldRead => match inputs.len() {
            1 | 2 => AliasClass::Field {
                base: inputs[0],
                offset: dyn_at(1),
                decl: None,
            },
            n if n >= 3 => AliasClass::Field {
                base: inputs[2],
                offset: dyn_at(3),
                decl: None,
            },
            _ => AliasClass::Any,
        },
        // `store_operands`: compact `[base, value]`, full-without-offset
        // `[ctrl, mem, base, value]`, full `[ctrl, mem, base, offset, value]`.
        // Note the arity-4 form's slot 3 is the *value*, so the offset is
        // Absent there — reading it as an offset would invent a disjointness.
        MemAccess::FieldWrite => match inputs.len() {
            2 => AliasClass::Field {
                base: inputs[0],
                offset: AccessOffset::Absent,
                decl: None,
            },
            4 => AliasClass::Field {
                base: inputs[2],
                offset: AccessOffset::Absent,
                decl: None,
            },
            n if n >= 5 => AliasClass::Field {
                base: inputs[2],
                offset: dyn_at(3),
                decl: None,
            },
            _ => AliasClass::Any,
        },
        // STRINTR21-5: the String char read `[ctrl, mem, value, index,
        // coder]` reads one or two bytes of the `byte[]` `value` at a CHAR
        // index, so its index names no byte cell: `Absent` (never disjoint
        // from, and never the same cell as, another access of `value`), and
        // it aliases nothing but a byte array's elements.
        MemAccess::ArrayRead if node.op == Op::StrCharDecode => {
            if inputs.len() >= 5 {
                AliasClass::ArrayElem {
                    array: inputs[2],
                    index: AccessOffset::Absent,
                    elem: MemKind::Byte,
                }
            } else {
                AliasClass::Any
            }
        }
        // `[ctrl, mem, array, index]` — the only documented form. The element
        // kind comes from the op, never from the layout: `array_elem_kind` is
        // what makes the class unforgeable by a consumer (see
        // `AliasClass::ArrayElem::elem`). An op that reached here without one
        // is a shape-table entry nobody taught the kind reader about, and it
        // degrades to `Any` rather than guessing an element type.
        MemAccess::ArrayRead | MemAccess::ArrayWrite => {
            match (inputs.len() >= 4, array_elem_kind(&node.op)) {
                (true, Some(elem)) => AliasClass::ArrayElem {
                    array: inputs[2],
                    index: dyn_at(3),
                    elem,
                },
                _ => AliasClass::Any,
            }
        }
        // `[ctrl, mem, array_ref]`, or the hand-built `[array]`. The arity
        // range is deliberately 1..=3 (see `ir_verify`'s arity lane).
        MemAccess::LengthRead => match inputs.len() {
            1 | 2 => AliasClass::ArrayLength { array: inputs[0] },
            n if n >= 3 => AliasClass::ArrayLength { array: inputs[2] },
            _ => AliasClass::Any,
        },
        MemAccess::Allocate => AliasClass::None,
        // `[ctrl, mem, obj]` — the only form. A monitor is addressed by the
        // locked reference and by nothing else: there is no offset operand, so
        // two monitors on the same object are always a must-alias.
        MemAccess::MonitorEnter | MemAccess::MonitorExit => {
            if inputs.len() >= 3 {
                AliasClass::Monitor { obj: inputs[2] }
            } else {
                AliasClass::Any
            }
        }
        MemAccess::Opaque => AliasClass::Any,
        // JIT r21 wave 2 (lane typecheck): an immutable header word is no
        // location any write names.
        MemAccess::HeaderRead => AliasClass::None,
    }
}

/// `class` with the declaring class `decl` recorded, when it is a field class
/// (round 15 wave 3, lane chain2, CHN15-2; [`Graph::memory_effect`]).
pub(in crate::ir) fn with_field_decl(class: AliasClass, decl: u32) -> AliasClass {
    match class {
        AliasClass::Field { base, offset, .. } => AliasClass::Field {
            base,
            offset,
            decl: Some(decl),
        },
        other => other,
    }
}

/// `class` with no declaring class ([`Graph::may_alias_by_index`]).
pub(in crate::ir) fn without_field_decl(class: AliasClass) -> AliasClass {
    match class {
        AliasClass::Field { base, offset, .. } => AliasClass::Field {
            base,
            offset,
            decl: None,
        },
        other => other,
    }
}

/// The element type an array-element access reads or writes, taken from the op
/// itself.
///
/// The one authority for `AliasClass::ArrayElem::elem`. `Op::ArrayLoad` and
/// `Op::ArrayStore` each carry their [`MemKind`] as a type tag, so the element
/// type of an access is a property of the *node*, not of whoever is asking —
/// which is the whole point of moving it into the alias class.
pub(in crate::ir) fn array_elem_kind(op: &Op) -> Option<MemKind> {
    match op {
        Op::ArrayLoad(kind) | Op::ArrayStore(kind) => Some(*kind),
        _ => None,
    }
}

/// The memory effect of a node, read from its op and edge layout alone.
///
/// Offsets stay [`AccessOffset::Dynamic`] because folding one needs the graph;
/// use [`Graph::memory_effect`] to get the folded form. Everything else — the
/// alias classes, the ordering, the safepoint and allocation flags — is final
/// here.
///
/// Ops with no memory shape are classified explicitly:
///
/// * a **memory-typed φ** is [`MemEffect::OPAQUE`]. It performs no access of
///   its own, but it is the merge of two token chains, and letting an access
///   float across it is a control-flow question this model does not answer.
/// * a **non-memory φ** is inert: it computes a value, it reads no memory.
/// * `Op::Return` is [`MemEffect::OPAQUE`] — the method's writes must all have
///   happened.
/// * every other **control** op is inert. `Op::Start`'s `Proj(1)` is the
///   initial memory token, but producing a token is not an access; a control
///   node's placement is governed by control edges, and
///   [`Graph::may_reorder`] refuses control nodes outright.
/// * `Op::Dead` is [`MemEffect::OPAQUE`]. A killed node should never be
///   queried; if one is, it must not answer "commutes with everything".
/// * everything else (arithmetic, comparisons, conversions, constants,
///   parameters, `Op::Phi` on data) is inert. `Op::Div` / `Op::Rem` throw on a
///   zero divisor, which is an exception-ordering question, not a memory one —
///   see the section header.
///
/// `Op::MonitorEnter` / `Op::MonitorExit` do have a shape, so they come out of
/// the table like any other memory op — but they are the one family whose
/// effect is neither a read nor a write alone: each reads *and* writes
/// [`AliasClass::Monitor`], is a safepoint, and carries half a JMM fence.
pub fn effect_of_node(node: &Node) -> MemEffect {
    match &node.op {
        Op::Phi => {
            if node.ty == IrType::Memory {
                MemEffect::OPAQUE
            } else {
                MemEffect::NONE
            }
        }
        Op::Return | Op::Dead => MemEffect::OPAQUE,
        // The interpreter's memory on OSR entry (round 11 wave 14): anything
        // may have been written. It has no `memory_shape` (no incoming token),
        // so it is classified here, the way a memory φ is.
        Op::OsrMemory => MemEffect::OPAQUE,
        Op::Guard { .. } => MemEffect {
            safepoint: true,
            ..MemEffect::NONE
        },
        op => match op.memory_shape() {
            None => MemEffect::NONE,
            Some(shape) => {
                let class = access_location(node, shape.access);
                match shape.access {
                    // A VOLATILE field access (round 9 wave 6): the same cell,
                    // with JMM ordering — an acquire for a read, a full fence
                    // for a write. See `Node::volatile_access`.
                    MemAccess::FieldRead if node.volatile_access => MemEffect::volatile_read(class),
                    MemAccess::FieldWrite if node.volatile_access => {
                        MemEffect::volatile_write(class)
                    }
                    MemAccess::FieldRead | MemAccess::ArrayRead | MemAccess::LengthRead => {
                        MemEffect::read(class)
                    }
                    MemAccess::FieldWrite | MemAccess::ArrayWrite => MemEffect::write(class),
                    MemAccess::Allocate => MemEffect::allocation(),
                    MemAccess::MonitorEnter => MemEffect::monitor_enter_of(class),
                    MemAccess::MonitorExit => MemEffect::monitor_exit_of(class),
                    MemAccess::Opaque => MemEffect::OPAQUE,
                    // JIT r21 wave 2 (lane typecheck, TYPECHECK21-1 part A):
                    // reads and writes nothing mutable, plain, allocates
                    // nothing Java-visible -- but the slow path is a helper
                    // call that may collect, so it stays a safepoint (load
                    // CSE, `join_load_forward` and the scheduler refuse to
                    // cross it exactly as they refuse a `Guard`'s).
                    MemAccess::HeaderRead => MemEffect {
                        reads: AliasClass::None,
                        writes: AliasClass::None,
                        order: MemOrder::Plain,
                        safepoint: true,
                        allocates: false,
                    },
                }
            }
        },
    }
}

/// `CRATONVM_JIT_IR_SCOPED_RAW_ALIAS` (round nio20 wave 4, lane L1, W3L4-1) --
/// **default ON**; `0`/`false`/`off`/`no` restores the `Opaque` answer for the
/// `ScopedMemoryAccess` fold. Read once per builder into
/// [`Graph::scoped_raw_alias`]; no static.
pub fn ir_scoped_raw_alias_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_PLAIN_STATIC_READ` (round nio20 wave 6, lane L1) --
/// **default ON**; `0`/`false`/`off`/`no` leaves [`Graph::plain_static_reads`]
/// empty, which restores the `Opaque` answer for every `getstatic`. Read per
/// compile by `lib.rs`'s feed; no static.
pub fn ir_plain_static_read_enabled() -> bool {
    true
}

/// The memory effect of a plain `getstatic` the lowering serves as the direct
/// read, or `None` for anything else. Round nio20 wave 6 (lane L1;
/// `r20-buffer-loop-barriers-left-after-the-raw-class-CLOSED-20261012.md` item 2,
/// W4L1-3 of `jit-r20-nio-proposals-w4-L1.md`).
///
/// [`Op::memory_shape`] calls every `Op::LoadStatic` `Opaque` because its
/// HELPER route (`jit_getstatic`) may run `<clinit>`, i.e. arbitrary Java.
/// `plain` is [`Graph::plain_static_reads`]' answer for this node's
/// `(class_id, field_index)`: the lowering takes the direct route
/// (`ir_lower::Lowerer::emit_inline_getstatic`: `MOV RAX, &base_cell; MOV RAX,
/// [RAX]; MOV RAX, [RAX + cell]`), which `lib.rs` admits only where
/// `DirectHelperTable::resolve_static_base` answers -- the declaring class
/// INITIALISED at compile time (monotone: a class never becomes uninitialised),
/// never `java/lang/System` -- and the inline route is on. Such a read:
///
/// * **reads** exactly the static cell ([`AliasClass::Static`]; statics live
///   in a Rust-side table, so no field, array or raw access aliases it, and a
///   `putstatic` stays `Opaque`, which aliases everything);
/// * **writes nothing**, runs no Java and calls nothing, so it is **no
///   safepoint** and **allocates nothing**;
/// * is **plain**: a VOLATILE static is never answered here (an acquire), so
///   the node keeps the op-level answer.
///
/// HotSpot's C2 gives the same read its own alias slice for the same reason.
/// What this buys is that a `getstatic` stops being the top of the lattice for
/// the alias-precise consumers ([`Graph::memory_effect`]'s readers: the
/// builder's field memo, load CSE, `loop_field_forward`, the array-load hoist,
/// the schedule check) -- in a `ByteBuffer` access, `getstatic
/// SCOPED_MEMORY_ACCESS` cleared the memo and refused the loop once per access.
/// The op-level consumers (LICM's hard-barrier test) are unchanged.
pub fn plain_static_read_effect(node: &Node, plain: bool) -> Option<MemEffect> {
    let Op::LoadStatic {
        class_id,
        field_index,
        is_volatile: false,
        ..
    } = node.op
    else {
        return None;
    };
    if !plain || node.volatile_access {
        return None;
    }
    Some(MemEffect {
        reads: AliasClass::Static {
            class_id,
            field: field_index,
        },
        writes: AliasClass::None,
        order: MemOrder::Plain,
        safepoint: false,
        allocates: false,
    })
}

/// The memory effect of a `ScopedMemoryAccess` fold node ([`UnboxOp::ScopedGet`]
/// / [`UnboxOp::ScopedPut`] in the documented full form `[ctrl, mem, sma,
/// session, base, offset, (value,) (bigEndian)]`), or `None` for any other
/// node (or a short edge list, which keeps the `Opaque` answer).
///
/// Round nio20 wave 4 (lane L1, W3L4-1). The op-level table
/// ([`Op::memory_shape`], [`effect_of_node`], [`memory_shape_of`]) still calls
/// every `Op::Unbox` `Opaque`: it has no graph, and so no
/// [`Graph::scoped_raw_alias`] to ask. [`Graph::memory_effect`] answers with
/// this instead when the switch is on, which is what every alias-precise
/// consumer reads (the builder's field memo, load CSE, `loop_field_forward`,
/// the array-load hoist, `isel`'s load fold, the schedule check).
///
/// * **reads / writes [`AliasClass::Raw`]** -- see that variant for what the
///   lowering admits and why nothing else can be the target.
/// * **a safepoint**: every inline check deopts (REEXECUTE at the `invoke`),
///   so no store may cross it. But it is a DEOPT, never a call or a poll:
///   [`Graph::safepoint_is_deopt_only`] says so for the passes that care.
/// * **no allocation, plain order**: the accessors folded are the plain
///   `get*`/`put*` ones (`try_ir_scoped_unbox`); nothing volatile, nothing
///   that runs Java or the collector on the path that continues.
///
/// The session (`MemorySessionImpl`) is only null-tested, the
/// `ScopedMemoryAccess` receiver only null-tested, and the base array's header
/// and length (both immutable) only read, so none of them is a heap READ any
/// store could change.
pub fn scoped_raw_effect(node: &Node) -> Option<MemEffect> {
    let Op::Unbox { op, .. } = &node.op else {
        return None;
    };
    let op = *op;
    if !op.is_scoped() || node.inputs.len() < 3 + op.arity() {
        return None;
    }
    let is_put = matches!(op, UnboxOp::ScopedPut { .. });
    Some(MemEffect {
        reads: if is_put {
            AliasClass::None
        } else {
            AliasClass::Raw
        },
        writes: if is_put {
            AliasClass::Raw
        } else {
            AliasClass::None
        },
        order: MemOrder::Plain,
        safepoint: true,
        allocates: false,
    })
}

/// What an op may do to memory — **independent of whether it is GVN-able**.
///
/// # Why this exists
///
/// Four predicates in `ir_optimize` — `preheader_may_trap`,
/// `loop_has_hard_barrier`, `loop_writes_memory` and `is_memory_barrier` —
/// spelled "can this node touch memory?" as
///
/// ```text
/// !op.is_pure() && !op.is_control() && !matches!(op, Load|Store|Phi|Dead)
/// ```
///
/// [`Op::is_pure`] is a *GVN/DCE* predicate — "may I fold, hash and delete
/// this?" — and its allow-list is maintained for that question. Any
/// pure-but-unlisted op was therefore silently reclassified as "may read or
/// write arbitrary memory". That is not a hypothetical: `Op::LCmp` and
/// `Op::FCmp` were missing from it, and the consequence was that **every loop
/// containing an `lcmp`, `fcmpl`, `fcmpg`, `dcmpl` or `dcmpg` was declared to
/// contain a hard memory barrier and lost all of its LICM** — `while (i < n)`
/// over `long` counters, and every FP comparison in a kernel loop. Adding the
/// two ops to `is_pure` fixed that instance and left the mechanism in place
/// for the next op somebody adds to the IR, silently, with no failing test.
///
/// # Why the match below has no `_` arm
///
/// That is the actual fix. A new `Op` variant now fails to compile until
/// someone states what it does to memory, which is the one question this
/// table exists to answer and the one nobody remembered to ask. Deriving the
/// answer for the memory-token ops from [`Op::memory_shape`] rather than
/// re-listing them keeps a single table: this function is the coarse
/// projection of that one, not a second opinion about it.
///
/// # Trapping is a SEPARATE question
///
/// `Op::Div` and `Op::Rem` answer [`MemoryShape::None`] here and that is
/// correct — neither touches memory — but neither is freely movable, because
/// a zero divisor raises `ArithmeticException`. [`may_raise`] is that
/// question, and the call sites ask whichever one they mean. Nothing here
/// makes a trapping node hoistable; all it does is stop one from CLOBBERING
/// an unrelated load somebody else wants hoisted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MemoryShape {
    /// Touches no memory at all: arithmetic, constants, comparisons, control,
    /// and a `Guard` (which transfers control to a deopt and writes nothing).
    None,
    /// May read a named location; never writes one.
    Reads,
    /// Writes a named location (and so also constrains reads of it).
    Writes,
    /// May read and write arbitrary memory — a call, an allocation, a class
    /// initialiser, a monitor. The top of the lattice.
    Opaque,
}

impl MemoryShape {
    /// True when this shape touches memory in any way at all.
    pub fn touches_memory(self) -> bool {
        !matches!(self, MemoryShape::None)
    }

    /// True when this shape may WRITE memory — the narrower question
    /// `ir_optimize::loop_writes_memory` asks, where a read cannot clobber the
    /// location a hoisted load reads and so cannot make a hoist unsafe.
    pub fn may_write(self) -> bool {
        matches!(self, MemoryShape::Writes | MemoryShape::Opaque)
    }
}

/// The memory shape of `op`. See [`MemoryShape`] for why this is not
/// [`Op::is_pure`], and for why there is no `_` arm.
pub fn memory_shape_of(op: &Op) -> MemoryShape {
    match op {
        // ── The ops with a `[ctrl, mem, …]` token: ONE table answers ───────
        //
        // `Op::memory_shape` is the table `memory_token_slot` and
        // `effect_of_node` already read; this arm is its coarse projection.
        Op::Load(_)
        | Op::Store(_)
        | Op::ArrayLoad(_)
        | Op::ArrayStore(_)
        | Op::ByteArrayCopy
        | Op::ArrayLength
        | Op::StrCharDecode
        | Op::New { .. }
        | Op::NewArray { .. }
        | Op::Call { .. }
        | Op::ConstString { .. }
        | Op::ConstClass { .. }
        | Op::LoadStatic { .. }
        | Op::StoreStatic { .. }
        | Op::InstanceOf { .. }
        | Op::CheckCast { .. }
        | Op::Throw
        | Op::LambdaIntToDouble
        | Op::IndyBridge { .. }
        | Op::MultiNewArray { .. }
        | Op::NewCp { .. }
        | Op::MonitorEnter
        | Op::MonitorExit
        | Op::Unbox { .. } => match op.memory_shape() {
            Some(shape) => match shape.access {
                MemAccess::FieldRead | MemAccess::ArrayRead | MemAccess::LengthRead => {
                    MemoryShape::Reads
                }
                // JIT r21 wave 2 (lane typecheck): a read that writes
                // nothing, so `loop_writes_memory`, the static hoist and the
                // array-load hoist no longer count it a writer. It still
                // touches memory, so LICM's hard-barrier test and the dead
                // store pass treat it as before.
                MemAccess::HeaderRead => MemoryShape::Reads,
                MemAccess::FieldWrite | MemAccess::ArrayWrite => MemoryShape::Writes,
                // An allocation runs a GC-visible store into fresh storage and
                // is a safepoint; a monitor is a full fence on
                // `AliasClass::Monitor`. Both are the top of the lattice for
                // the same reason `effect_of_node` gives them one.
                MemAccess::Allocate
                | MemAccess::MonitorEnter
                | MemAccess::MonitorExit
                | MemAccess::Opaque => MemoryShape::Opaque,
            },
            // Unreachable: every op listed above has a table entry, and the
            // anti-drift test in this file asserts it. `Opaque` rather than a
            // panic, because the safe answer to "I do not know" is the top of
            // the lattice and a compiler does not get to abort a compile over
            // a table it can fall back from.
            None => MemoryShape::Opaque,
        },

        // ── No op-level answer: the conservative one ───────────────────────
        //
        // * A **φ** has no single shape. A `Memory`-typed one carries a token
        //   and `effect_of_node` gives it `OPAQUE`; a value φ touches nothing.
        //   An `&Op` cannot tell them apart, so the op-level answer is the top
        //   of the lattice, and every call site that means the value case
        //   allows `Op::Phi` explicitly (all four of them already do).
        // * **`Op::Dead`** is a node whose inputs have been cleared. Its
        //   effects are not something to reason about — `effect_of_node` says
        //   `OPAQUE` for the same reason — and the call sites that know a dead
        //   node emits nothing exempt it by name.
        // * **`Op::Return`** ends the method; everything written before it is
        //   observable after it. `effect_of_node` says `OPAQUE`.
        Op::Phi | Op::Dead | Op::Return => MemoryShape::Opaque,
        // The interpreter's memory on OSR entry (round 11 wave 14): the top of
        // the lattice, like the memory φ it feeds. Conservative for an
        // ENCLOSING loop too (its LICM treats the arm as a barrier), which
        // costs only under `CRATONVM_JIT_OSR_OPAQUE_ENTRY`.
        Op::OsrMemory => MemoryShape::Opaque,

        // ── Touches no memory ─────────────────────────────────────────────
        //
        // `Op::Guard` belongs here on the merits: it carries `[ctrl, cond]`,
        // no memory edge at all, and transfers control to a deopt. It is
        // still a barrier to *motion* — see `ir_optimize::licm_hoist_barrier`
        // and `control_token_has_guard` — because a node hoisted above it runs
        // on an input the guard was going to reject. That is a control fact,
        // not a memory one, and conflating the two is what this enum is for.
        //
        // `Op::Div` / `Op::Rem` likewise: no memory, but [`may_raise`].
        Op::Start
        | Op::If
        // A multi-way branch reads its key out of a register and writes
        // nothing. The jump TABLE `ir_lower` may emit for it is read-only
        // constant data in the code buffer, not a location any Java program
        // can name, so it is outside this lattice entirely.
        | Op::Switch { .. }
        | Op::Merge
        | Op::Region
        | Op::Proj(_)
        | Op::Const(_)
        | Op::ConstF(_)
        | Op::Param(_)
        | Op::Add
        | Op::Sub
        | Op::Mul
        | Op::Div
        | Op::Rem
        | Op::Neg
        | Op::And
        | Op::Or
        | Op::Xor
        | Op::Shl
        | Op::Shr
        | Op::UShr
        | Op::Cmp(_)
        | Op::LCmp
        | Op::FCmp { .. }
        | Op::I2L
        | Op::L2I
        | Op::I2F
        | Op::I2D
        | Op::L2F
        | Op::L2D
        | Op::F2I
        | Op::F2L
        | Op::F2D
        | Op::D2I
        | Op::D2L
        | Op::D2F
        | Op::I2B
        | Op::I2C
        | Op::I2S
        | Op::Guard { .. }
        | Op::ScalarIntrinsic(_)
        // Reads the object header, which no Java write can reach: the class
        // id and the kind byte are fixed at allocation. No alias class
        // describes them, and nothing a store, call or monitor does changes
        // the answer, so there is no ordering to preserve.
        | Op::ExactClassIs { .. }
        // JIT round 19 (lane osr19): the same header word, the same answer.
        | Op::ClassWordOf
        // Round 11 wave 14: a constant `0` with a control pin, and a read of
        // the INTERPRETER's locals array, which no Java store can reach. Both
        // sit in the OSR arm, which no path through the compiled body runs.
        | Op::OsrEntryFlag { .. }
        | Op::OsrLocal(_)
        // JIT round 19 wave 3 (lane eager3): the class's init word, which no
        // Java store reaches (the VM's initialization writes it). Pinned to
        // its control input, so no ordering beyond the pin is needed. As a
        // frozen page's generation byte a store CAN move it; see the op's
        // doc for what orders it then.
        | Op::ClassInitTest { .. }
        | Op::CurrentThreadMirror { .. }
        | Op::VthreadBoundaryTest { .. }
        // JIT round 20 wave 11 (lane pIndy): a root word read, like the mirror.
        | Op::IndyRootRef { .. }
        // JIT round 19 wave 5 (lane handler5, SPLICE4-1): a constant `0` with
        // a control pin, and a read of a frame word no Java store reaches,
        // pinned to the arm no compiled path runs into.
        | Op::CatchEntryFlag { .. }
        | Op::CaughtException
        // nio20 L2: a keep-alive only extends its operand's live range.
        | Op::KeepAlive
        // nio21 wave 3 L2: a constant `0` pinned after its call, and a read
        // of a frame word no Java store can reach. The call before them is
        // the barrier; they order nothing themselves. (`CaughtException` is
        // listed with `CatchEntryFlag` above.)
        | Op::CallThrew { .. } => MemoryShape::None,
    }
}

/// May executing this op raise, deopt, or fault — i.e. transfer control
/// somewhere other than its own successor?
///
/// The companion question to [`memory_shape_of`], and the reason that one can
/// answer [`MemoryShape::None`] for `Op::Div` without making a division
/// hoistable. Separating them is the whole point: the memory question decides
/// whether a node can CLOBBER what somebody else wants to move, and this one
/// decides whether the node ITSELF can move.
///
/// Also with no `_` arm, and for the same reason.
///
/// Note what this does *not* say: a caller that exempts some trapping op
/// because of a property of its own analysis — `ir_optimize::preheader_may_trap`
/// exempts `Load`/`Store`, because the nodes it is clearing a path for are
/// themselves guarded loads at the same control token — states that exemption
/// at the call site, where the justification lives. It is not encoded here,
/// because here it would be false.
pub fn may_raise(op: &Op) -> bool {
    match op {
        // Arithmetic that traps on a zero divisor. The only pure-looking
        // members of this list, and the reason it exists.
        Op::Div | Op::Rem => true,
        // A null base faults; an out-of-range index raises; a store into an
        // array of references can raise `ArrayStoreException`.
        Op::Load(_) | Op::Store(_) | Op::ArrayLoad(_) | Op::ArrayStore(_) | Op::ArrayLength => true,
        // JIT round 24 B1: the loop-entry session pre-check never leaves the
        // body -- every failed test answers "keep the session" (the session
        // itself), a null session included (`ir_lower`
        // `lower_session_precheck`). Counted as raising, it made the native
        // segment loop's pre-header "trap" and refused its bounds predicate,
        // leaving both guards in the body.
        Op::Unbox {
            op: UnboxOp::SessionPrecheck | UnboxOp::ArenaCacheEntry | UnboxOp::ArenaCacheBase,
            ..
        } => false,
        // Allocation raises `OutOfMemoryError` / `NegativeArraySizeException`;
        // anything that may run `<clinit>`, a callee, or a runtime helper can
        // raise whatever that code raises.
        Op::New { .. }
        | Op::NewArray { .. }
        | Op::Call { .. }
        | Op::ConstString { .. }
        | Op::ConstClass { .. }
        | Op::LoadStatic { .. }
        | Op::StoreStatic { .. }
        | Op::InstanceOf { .. }
        | Op::CheckCast { .. }
        | Op::Throw
        | Op::LambdaIntToDouble
        | Op::IndyBridge { .. }
        | Op::MultiNewArray { .. }
        | Op::NewCp { .. }
        | Op::MonitorEnter
        | Op::MonitorExit
        | Op::Unbox { .. } => true,
        // A guard's whole purpose is to leave the compiled body.
        Op::Guard { .. } => true,
        // A removed node: conservative, like its memory shape.
        Op::Dead => true,
        // Everything else computes a value or joins control and cannot fail.
        // `Op::ScalarIntrinsic` is on this side deliberately: every member of
        // `ScalarOp` is pure arithmetic with a defined answer for every
        // input, zero included (see the bit-scan note on that enum). The
        // round-19 FP min/max members branch locally inside their own
        // sequence (NaN / signed-zero edges) but still cannot fail.
        Op::Start
        | Op::Return
        | Op::If
        // Every key has an edge: the ones in `[low, high]` by projection and
        // the rest by the default edge, which JVMS makes mandatory. There is
        // no "nothing matched" outcome for a switch to raise.
        | Op::Switch { .. }
        | Op::Merge
        | Op::Region
        | Op::Proj(_)
        | Op::Const(_)
        | Op::ConstF(_)
        | Op::Param(_)
        | Op::Phi
        | Op::Add
        | Op::Sub
        | Op::Mul
        | Op::Neg
        | Op::And
        | Op::Or
        | Op::Xor
        | Op::Shl
        | Op::Shr
        | Op::UShr
        | Op::Cmp(_)
        | Op::LCmp
        | Op::FCmp { .. }
        | Op::I2L
        | Op::L2I
        | Op::I2F
        | Op::I2D
        | Op::L2F
        | Op::L2D
        | Op::F2I
        | Op::F2L
        | Op::F2D
        | Op::D2I
        | Op::D2L
        | Op::D2F
        | Op::I2B
        | Op::I2C
        | Op::I2S
        | Op::ScalarIntrinsic(_)
        // Total: a null or an array answers `0`; the guard that consumes it is
        // the node that leaves.
        | Op::ExactClassIs { .. }
        // JIT round 19 (lane osr19): total in the same way.
        | Op::ClassWordOf
        // Round 11 wave 14: a zero, a read of a locals array the door sized
        // (`ir_osr_enter` refuses a short one), and a token. None can fail.
        | Op::OsrEntryFlag { .. }
        | Op::OsrLocal(_)
        | Op::OsrMemory
        // JIT round 19 wave 3 (lane eager3): one byte load from a live
        // `Class::init_state`; the guard consuming it is the node that leaves.
        | Op::ClassInitTest { .. }
        | Op::CurrentThreadMirror { .. }
        | Op::VthreadBoundaryTest { .. }
        // JIT round 20 wave 11 (lane pIndy): a root word read, like the mirror.
        | Op::IndyRootRef { .. }
        // JIT round 19 wave 5 (lane handler5, SPLICE4-1): a zero and a frame
        // word read. Neither can fail.
        | Op::CatchEntryFlag { .. }
        | Op::CaughtException
        // nio20 L2: emits no code.
        | Op::KeepAlive
        // STRINTR21-5: one unchecked load behind the guards that prove it in
        // bounds; the guards are the nodes that leave. Its control pin keeps
        // it below them.
        | Op::StrCharDecode
        | Op::ByteArrayCopy
        // nio21 wave 3 L2: a zero and a frame-word read. The CALL they follow
        // is what raises; these only name which edge its exception took.
        // (`CaughtException` is listed with `CatchEntryFlag` above.)
        | Op::CallThrew { .. } => false,
    }
}

/// Largest allocation set [`Graph::ref_origin`] will carry before giving up
/// and reporting unknown provenance.
///
/// A reference that may be one of nine different allocations is not a
/// reference any pass profits from disambiguating, and the bound keeps the
/// join over a deep φ web linear.
pub const MAX_ALIAS_ALLOC_SET: usize = 8;

/// Recursion bound for [`Graph::ref_origin`]. A loop-carried φ cycle resolves
/// to unknown rather than hanging.
pub(in crate::ir) const REF_ORIGIN_MAX_DEPTH: u32 = 32;

/// What a reference node may point to — the provenance half of
/// [`Graph::may_alias`].
///
/// Same shape as `ir_optimize`'s private `RefPointsTo`, which it is meant to
/// replace: `allocs` is the set of in-method allocations the reference may
/// name (`None` = unknown provenance, may be anything), and `pre_existing`
/// records whether it may be an object that existed before the method ran.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefOrigin {
    /// The fresh in-method allocations this reference may name, or `None` when
    /// its provenance is unknown (a loaded reference, a call result, an
    /// over-wide φ join).
    pub allocs: Option<Vec<NodeId>>,
    /// True when it may be a *pre-existing* reference — a parameter, `this`.
    /// Never an in-method allocation.
    pub pre_existing: bool,
}

impl RefOrigin {
    /// Unknown provenance: may be any object at all.
    pub fn unknown() -> RefOrigin {
        RefOrigin {
            allocs: None,
            pre_existing: false,
        }
    }

    /// True when nothing is known about this reference.
    pub fn is_unknown(&self) -> bool {
        self.allocs.is_none()
    }

    /// Exactly one fresh allocation.
    pub(in crate::ir) fn alloc(id: NodeId) -> RefOrigin {
        RefOrigin {
            allocs: Some(vec![id]),
            pre_existing: false,
        }
    }

    /// A pre-existing reference and nothing else.
    pub(in crate::ir) fn pre_existing_only() -> RefOrigin {
        RefOrigin {
            allocs: Some(Vec::new()),
            pre_existing: true,
        }
    }

    /// Join another origin into this one (the φ merge). Any unknown input
    /// poisons the result, and overflowing [`MAX_ALIAS_ALLOC_SET`] degrades it
    /// to unknown.
    pub(in crate::ir) fn absorb(&mut self, other: RefOrigin) {
        self.pre_existing |= other.pre_existing;
        match (self.allocs.as_mut(), other.allocs) {
            (Some(mine), Some(theirs)) => {
                for id in theirs {
                    if !mine.contains(&id) {
                        mine.push(id);
                    }
                }
                if mine.len() > MAX_ALIAS_ALLOC_SET {
                    self.allocs = None;
                }
            }
            _ => self.allocs = None,
        }
    }

    /// True when these two references provably name **different** objects.
    ///
    /// Requires both sides to have known provenance, their allocation sets to
    /// be disjoint, and at most one of them to be possibly-pre-existing —
    /// because two parameters can be the same object (`foo(x, x)`), while a
    /// fresh allocation can never be a value that existed before the method
    /// ran.
    pub fn provably_distinct(&self, other: &RefOrigin) -> bool {
        let (mine, theirs) = match (&self.allocs, &other.allocs) {
            (Some(a), Some(b)) => (a, b),
            _ => return false,
        };
        if self.pre_existing && other.pre_existing {
            return false;
        }
        !mine.iter().any(|id| theirs.contains(id))
    }
}

/// A `may_reorder` answer, with the fact that justifies it.
///
/// The point of returning a reason rather than a `bool` is the acceptance
/// criterion: *"optimizations cannot reorder through side effects without a
/// proof encoded in the graph."* A pass that moves a node records the
/// [`ReorderProof`] it moved on; a pass that declines records the
/// [`ReorderBlock`] it hit. Both are checkable after the fact, and a `Blocked`
/// reason is the diagnostic a missed optimization report needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Reorder {
    /// The two nodes may be swapped, on the strength of this fact.
    Allowed(ReorderProof),
    /// They may not, because of this fact.
    Blocked(ReorderBlock),
}

impl Reorder {
    /// True when the swap is licensed.
    pub fn is_allowed(self) -> bool {
        matches!(self, Reorder::Allowed(_))
    }

    /// True when the swap is refused.
    pub fn is_blocked(self) -> bool {
        matches!(self, Reorder::Blocked(_))
    }
}

/// Why a reorder is licensed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ReorderProof {
    /// At least one of the two constrains motion in no way at all — no memory,
    /// no fence, no safepoint, no allocation.
    EffectFree,
    /// Neither node writes memory, so no read can observe a difference.
    ReadOnly,
    /// Both touch memory, but the locations are provably disjoint.
    DisjointLocations,
}

/// Why a reorder is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ReorderBlock {
    /// One of the ids does not name a node.
    UnknownNode,
    /// One of them is a control node. Control placement is governed by control
    /// edges, which this model does not reason about.
    Control,
    /// The later node consumes the earlier one as a **value** (or control)
    /// input. Note this deliberately does *not* fire for a memory-token edge:
    /// breaking an over-strict token edge is exactly what the model is for.
    DataDependence,
    /// One of them is a full fence — a volatile access.
    Fence,
    /// The earlier node is an acquire (monitor-enter, volatile read), so
    /// nothing after it may move above it.
    Acquire,
    /// The later node is a release (monitor-exit, volatile write), so nothing
    /// before it may move below it.
    Release,
    /// Both allocate, and allocation order is observable.
    Allocation,
    /// One is a safepoint and the other writes memory.
    Safepoint,
    /// Their locations may be the same, and at least one of them writes.
    MayAlias,
}
