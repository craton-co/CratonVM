pub(crate) use super::loop_retire::post_box_fold_cleanup;
#[allow(unused_imports)]
use super::*;

// ── Public API ───────────────────────────────────────────────────────

/// Backstop on [`optimize`]'s `{licm, unroll, cleanup, licm}` round.
///
/// Not the mechanism — the early break on "no progress" is, and it is a real
/// convergence test now that [`run_licm_pass`] reports honestly. Four rather
/// than two because the honest break makes an unused round free, and two was
/// only ever enough for the one interleaving the pass-order conflict names
/// (`licm; unroll; licm`); a nest whose inner loop hoists something the outer
/// one can then hoist again wants one more.
pub(in crate::ir_optimize) const MAX_LOOP_ROUNDS: usize = 4;

/// What [`optimize_with`] may know about the compile beyond the graph itself.
///
/// Round 11 wave 7 (`r11w6-mid-ir-tier-guards-never-read-the-despec-registry`):
/// a pass that plants a guard from a STRUCTURAL fact, rather than from the
/// profile, must ask this VM's per-bci de-spec registry before planting it, or
/// a guard that fails on every call is re-planted on every recompile until the
/// method is given up. `plant_range_check_predicates` is that pass.
pub struct OptimizeContext<'a> {
    /// The compiling VM's per-bci de-spec registry. `None` (no VM in scope: the
    /// unit tests and [`optimize`]) plants no structural guard at all, because
    /// nothing could ever withdraw one.
    pub despec: Option<&'a crate::deopt::DespecRegistry>,
    /// This method's key in `despec`, `"<class>.<method>:<descriptor>"`.
    pub method_key: &'a str,
    /// Round 16 (lane matrix): this is the optimizing OSR route's compile
    /// (`CompileRequest::osr_optimizing_route`). Its body is entered at a loop
    /// HEADER, past that loop's pre-header, so a value hoisted into a
    /// pre-header is live across the entry with no local naming it, and
    /// `ir_lower::emit_osr_entry_stubs` refuses the entry. The array-load hoist
    /// ([`hoist_invariant_array_loads`]) therefore stays off here.
    pub osr_route: bool,
}

/// Run all optimization passes on the graph, with no de-spec registry: every
/// pass that the registry gates stays off. See [`optimize_with`].
pub fn optimize(graph: &mut Graph) {
    optimize_with(
        graph,
        OptimizeContext {
            despec: None,
            method_key: "",
            osr_route: false,
        },
    );
}

/// Run all optimization passes on the graph. The production entry
/// (`lib.rs`'s `ir_tier`), which hands in this VM's de-spec registry.
pub fn optimize_with(graph: &mut Graph, ctx: OptimizeContext<'_>) {
    // The unroller's unreachable-frames net is per-compile state on a
    // thread-local. `ir_lower` takes (reads and clears) it, but a pipeline that
    // bails between `optimize` and lowering never does, and the next compile on
    // this thread would inherit a stale `true`. Clear it at the only entry.
    UNROLL_USED_UNREACHABLE_FRAMES.with(|c| c.set(false));
    // The same, for the pure-body unroll's stranded frames (nio20 wave 5, L3).
    UNROLL_STRANDED_BCIS.with(|c| *c.borrow_mut() = StrandedFrames::new());
    // W19L3-3's census of the forward's writer frame copies, per compile.
    loop_field_forward::reset_frame_copy_census();
    // Read once per compile; cleared again at the end so no pass run outside
    // `optimize` inherits it. See `snapshot_liveness_enabled`.
    let liveness = snapshot_liveness_enabled();
    SNAPSHOT_LIVENESS_ON.with(|c| c.set(liveness));
    // The splice resume map of the build that produced this graph (round 9
    // wave 3), so snapshot liveness can attribute a node of a spliced body to
    // the snapshot its deopt consults. Only needed, and only read, under
    // liveness; cleared with it.
    SPLICE_RESUME.with(|c| {
        *c.borrow_mut() = if liveness {
            crate::ir::splice_resume_map_this_build()
        } else {
            None
        }
    });
    // JIT round 19 wave 12 (lane hoist12): the route, for the passes below
    // that have no context (HOIST11-3 / HOIST11-4).
    graph.optimize_osr_route = ctx.osr_route;
    // JIT round 20 wave 11 (lane pIR; L3W6-2 / W6L4-5): `TABLE.length` of a
    // trusted `static final` array is a constant before any pass reads it, so
    // the constant fold, the divisor strength reduction (`x % TABLE.length`)
    // and the loop passes all see the number.
    fold_trusted_static_array_lengths(graph);
    // JIT round 21 wave 2 (lane alloc, opt-in): and `new T[n].length` with a
    // constant `n`, so escape analysis can drop an array only its length reads.
    fold_fresh_array_lengths(graph);
    // JIT r27 LOOP27-2 (`CRATONVM_JIT_IR_NARROW_LONG_INDUCTION`, default ON): a
    // counted `long` induction as an `int` one, before any loop rule reads it.
    if !ctx.osr_route && long_induction_narrow::narrow_long_induction_enabled() {
        long_induction_narrow::narrow_long_inductions(graph);
    }
    // nio21 wave 5 (lane L5): `CRATONVM_JIT_IR_MASK_PREDICATE_EARLY` (default
    // ON since nio21 wave 7, for the unroll candidates only) plants the mask
    // predicate BEFORE the loop rounds, so the unroller sees the pre-header
    // guard that `ir_lower::node_cannot_deopt`'s masked-load clause reads. Not
    // on the OSR route, for the late plant's reason.
    let early_mask = if !ctx.osr_route
        && mask_predicate::mask_predicate_enabled()
        && mask_predicate::early_mask_predicate_enabled()
    {
        ctx.despec.map(|d| (d, ctx.method_key))
    } else {
        None
    };
    // TLGET21-3: a trap-form `ThreadLocal.get` chain out of its loop BEFORE
    // the loop rounds, so they (LICM, the unroller's peel and copies) see a
    // loop that reads one invariant value, and the chain is moved while its
    // record still names the builder's nodes. The late call below takes what
    // only LICM made invariant.
    if let Some(despec) = ctx.despec {
        if tl_chain_predicate::tl_loop_predicate_enabled()
            && tl_chain_predicate::plant_tl_chain_predicates(
                graph,
                despec,
                ctx.method_key,
                ctx.osr_route,
            ) > 0
        {
            crate::ir_evidence::note(crate::ir_evidence::Transform::Licm);
        }
    }
    optimize_passes(
        graph,
        ctx.osr_route,
        early_mask,
        ctx.despec.map(|d| (d, ctx.method_key)),
    );
    // JIT r23 w2c close (`CRATONVM_JIT_IR_CAST_AFTER_INSTANCEOF`): a cast on
    // the hit edge of an `instanceof` of the same target is no check at all.
    if cast_after_instanceof_enabled() && drop_instanceof_dominated_casts(graph) > 0 {
        crate::ir_evidence::note(crate::ir_evidence::Transform::Simplified);
        scalar_cleanup(graph);
    }
    // Round 16 (lane matrix, `CRATONVM_JIT_IR_LICM_ARRAY_LOAD`): an invariant
    // array element read -- the row `c[i]` of `c[i][j]` in a `j` loop -- out of
    // its loop, BEFORE the range predicates, which plant a guard only for an
    // array that is invariant in the loop and so could never see a row.
    if array_load_hoist_admitted(ctx.osr_route)
        && hoist_invariant_array_loads(graph, ctx.despec, ctx.method_key) > 0
        && !ctx.osr_route
    {
        // MX16-2: `j < c[i].length` re-reads the row's length in the header
        // every trip. The row is invariant now, but LICM's `ArrayLength` arm
        // ran before the hoist made it so; one more pass lifts the length
        // (with its own pre-header deopt frame). Not on the OSR route, whose
        // entry stubs re-execute only the hoisted reads and the predicates.
        run_licm_pass(graph);
    }
    // After the loop rounds: LICM has hoisted the invariant loads a predicate
    // reads, and no later LICM round sees the guard (an outer loop's LICM
    // treats a guard in its body as a hard barrier).
    if let Some(despec) = ctx.despec {
        // TLGET21-3 (`CRATONVM_JIT_IR_THREAD_LOCAL_LOOP_PREDICATE`): a
        // trap-form `ThreadLocal.get` chain out of a loop that writes nothing,
        // first, so the planters below (and the late unroll) see a body that
        // reads its value as a loop-invariant one. Its guards are header
        // predicates, which none of them counts as a pre-header trap.
        if tl_chain_predicate::tl_loop_predicate_enabled() {
            let n = tl_chain_predicate::plant_tl_chain_predicates(
                graph,
                despec,
                ctx.method_key,
                ctx.osr_route,
            );
            if n > 0 {
                crate::ir_evidence::note(crate::ir_evidence::Transform::Licm);
            }
        }
        if !osr_route_withholds_range_predicates(ctx.osr_route) {
            let before = graph.nodes.len();
            plant_range_check_predicates(graph, despec, ctx.method_key);
            tag_predicate_guards(graph, before, crate::deopt::PredicateKind::Range);
        }
        // JIT round 19 wave 5 (lane osr5, IROPS3-1,
        // `CRATONVM_JIT_IR_BCE_CLAMP_PREDICATE`, opt-in): `a.length > 0` at
        // the loop entry of a clamped `a[...]`, in the range predicates' shape.
        if crate::ir_check_elim::clamp_predicate_enabled()
            && !osr_route_withholds_range_predicates(ctx.osr_route)
        {
            let before = graph.nodes.len();
            plant_clamp_length_predicates(graph, despec, ctx.method_key);
            tag_predicate_guards(graph, before, crate::deopt::PredicateKind::ClampLength);
        }
        // Round 17 (CG16P-2): an invariant array's length read past the
        // header, from a pre-header that deopts to the loop entry. Not in the
        // OSR route, whose door admits only exits it can resume exactly.
        if !ctx.osr_route {
            let before = graph.nodes.len();
            plant_length_predicates(graph, despec, ctx.method_key);
            tag_predicate_guards(graph, before, crate::deopt::PredicateKind::Length);
        }
        // JIT round 23 wave 8 (lane W8-L7,
        // `CRATONVM_JIT_IR_AFFINE_GUARD_PREDICATE`, default ON after the
        // n23-w8h A/B): a body
        // `Guard(c*k + d op X)` of a counted loop as one pre-header guard at
        // the range's extreme (`affine_guard_predicate`) -- the segment
        // accessor's two bounds guards. After the length predicate, which
        // moves the invariant ones. Not on the OSR route.
        if !ctx.osr_route && affine_guard_predicate::affine_guard_predicate_enabled() {
            let before = graph.nodes.len();
            let n = affine_guard_predicate::plant_affine_guard_predicates(
                graph,
                despec,
                ctx.method_key,
            );
            tag_predicate_guards(graph, before, crate::deopt::PredicateKind::Range);
            if n > 0 {
                crate::ir_evidence::note(crate::ir_evidence::Transform::Licm);
                // JIT round 23 wave 8 (lane W8-L7): the retired body guards'
                // compares (and the address arithmetic only they read) are
                // dead now; without this they stayed in the loop as
                // `cmp; setcc; movzx; store` (the n23-w8g loss). Dead-node
                // elimination only, and only when the planter replaced
                // something: no other graph changes.
                eliminate_dead_nodes(graph);
            }
        }
        // JIT round 23 wave 2d (lane L1, TCL23-1;
        // `CRATONVM_JIT_IR_NULL_BASE_LOAD_PREDICATE`, default ON): a field read
        // of an invariant, possibly-null base out of its loop, behind a
        // pre-header null guard that deopts to the loop entry. On the OSR
        // route or an OSR-door graph only under
        // `CRATONVM_JIT_IR_NULL_BASE_LOAD_PREDICATE_OSR` (JIT round 23 wave 3c),
        // and there only for a loop every entry reaches through its pre-header
        // (see `plant_null_base_load_predicates`).
        // JIT round 23 wave 5 (lane L1, TCL23-3;
        // `CRATONVM_JIT_IR_CHECKCAST_PREDICATE`, default ON): an invariant
        // operand's in-body `checkcast` leaves the loop behind a pre-header
        // exact-class guard, so the null-base predicate below can hoist the
        // field read through it. Under the same OSR rule.
        // JIT r23 w2c close: the first node either planter below adds, so the
        // late unroll round can find the loops whose pre-headers they used.
        let predicates_from = graph.nodes.len();
        let mut predicated_reads = 0usize;
        if checkcast_predicate_enabled()
            && ((!ctx.osr_route && !graph.osr_compile) || null_base_load_predicate_osr_enabled())
        {
            let before = graph.nodes.len();
            let n = plant_checkcast_predicates(graph, despec, ctx.method_key);
            predicated_reads += n;
            tag_predicate_guards(graph, before, crate::deopt::PredicateKind::CheckCast);
            if n > 0 {
                crate::ir_evidence::note(crate::ir_evidence::Transform::Licm);
            }
        }
        if null_base_load_predicate_enabled()
            && ((!ctx.osr_route && !graph.osr_compile) || null_base_load_predicate_osr_enabled())
        {
            let before = graph.nodes.len();
            let n = plant_null_base_load_predicates(graph, despec, ctx.method_key);
            predicated_reads += n;
            tag_predicate_guards(graph, before, crate::deopt::PredicateKind::NullBase);
            if n > 0 {
                crate::ir_evidence::note(crate::ir_evidence::Transform::Licm);
            }
            // JIT round 23 wave 6 (lane L1,
            // `CRATONVM_JIT_IR_LICM_ARRAY_LOAD_AFTER_PREDICATES`): the
            // array-load hoist ran before the predicates, so `h.tab[5]` saw an
            // in-body `h.tab` and stayed in the loop. A field read the
            // null-base predicate just moved to the pre-header makes such an
            // element read invariant: one more hoist round takes it out.
            if n > 0
                && array_load_hoist_admitted(ctx.osr_route)
                && licm_array_load_after_predicates_enabled()
            {
                let m = hoist_invariant_array_loads(graph, ctx.despec, ctx.method_key);
                if m > 0 {
                    crate::ir_evidence::note(crate::ir_evidence::Transform::Licm);
                }
            }
        }
        // JIT r23 w2c close: the LATE unroll round. The loop rounds (and so
        // the unroller) ran while the loops still held the reads the two
        // planters above take to the pre-header:
        // * `CRATONVM_JIT_IR_UNROLL_AFTER_PREDICATES`: the loops they emptied
        //   are offered once more (a pure body is main/post's first case);
        // * `CRATONVM_JIT_IR_UNROLL_MAIN_POST_DEOPT`: every loop is, with the
        //   deopt-capable main/post form (per-copy frames) admitted here only
        //   -- earlier it would have copied reads a predicate then hoists.
        // Loops a partial unroll already took are skipped either way.
        if unroll_enabled() {
            let late_deopt = unroll_main_post_deopt_enabled();
            let headers = if late_deopt {
                None
            } else if predicated_reads > 0 && unroll_after_predicates_enabled() {
                Some(loops_predicated_since(graph, predicates_from))
            } else {
                Some(FxHashSet::default())
            };
            if headers.as_ref().is_none_or(|h| !h.is_empty()) {
                // The predicates' retired reads, cleaned up only when there
                // are some: a compile they did not touch keeps its graph.
                if predicated_reads > 0 {
                    scalar_cleanup(graph);
                }
                if unroll_wide_pure::unroll_in_yielding_iota(graph, headers.as_ref(), true) {
                    crate::ir_evidence::note(crate::ir_evidence::Transform::Unrolled);
                    scalar_cleanup(graph);
                    // This round runs after `optimize_passes`; reassociate its
                    // newly cloned accumulation trees too.
                    run_linear_reassoc(graph);
                }
            }
        }
        // nio20 wave 6 (lane L4): `a[x & c]` over an invariant array, from a
        // pre-header `c < a.length` that deopts to the loop entry
        // (`mask_predicate`). Not on the OSR route, for the length
        // predicate's reason: its door admits only exits it resumes exactly.
        // nio21 wave 7 (lane L5): always, also after an early plant -- the
        // early plant takes only the loops the unroller can retire
        // (`mask_predicate::early_plant_candidate`), and this one skips an
        // array an early guard already covers.
        // JIT r20 wave 19 (lane yLoopPred, LOOPOPT20-3): on the OSR route too,
        // for the loops an opaque OSR arm enters through their pre-header
        // (`plant_mask_predicates` reads `graph.optimize_osr_route` and skips
        // every other loop there).
        if mask_predicate::mask_predicate_enabled()
            && (!ctx.osr_route
                || (osr_route_arm_predicates_enabled()
                    && !osr_route_withholds_range_predicates(ctx.osr_route)))
        {
            let before = graph.nodes.len();
            mask_predicate::plant_mask_predicates(graph, despec, ctx.method_key, false);
            tag_predicate_guards(graph, before, crate::deopt::PredicateKind::Mask);
        }
        // nio21 wave 3 (lane L5): `x <= a.length`, `0 <= init` and `0 <= x`
        // for a monotone non-counted loop index (`monotone_predicate`), read by
        // `ir_check_elim`'s monotone-induction rule. Not on the OSR route, for
        // the length predicate's reason.
        // JIT r23 wave 7b (lane L1, `CRATONVM_JIT_IR_MONOTONE_PREDICATE_OWN_LENGTH`):
        // on the OSR route too, for the loops every entry reaches through
        // their pre-header (the planter's own check), under the arm
        // predicates' OSR admission -- the `0 <= init` guard an OSR body's
        // `for (i ..; i < a.length; i++) a[i]` needs to lose its bounds check.
        if monotone_predicate::monotone_predicate_enabled()
            && (!ctx.osr_route
                || (monotone_predicate::monotone_predicate_own_length_enabled()
                    && osr_route_arm_predicates_enabled()
                    && !osr_route_withholds_range_predicates(ctx.osr_route)))
        {
            let before = graph.nodes.len();
            monotone_predicate::plant_monotone_predicates(
                graph,
                despec,
                ctx.method_key,
                ctx.osr_route,
            );
            tag_predicate_guards(graph, before, crate::deopt::PredicateKind::Monotone);
        }
        // nio21 wave 4 (lane L5): a loop-invariant receiver's in-loop class
        // tests answered once, by a loop-entry guard (`class_predicate`), and
        // the miss arms they guarded (the virtual calls) cut. LAST, after
        // every other pass that reads the loop: its pre-header guard is a
        // trapping node to LICM's and the length predicate's
        // `preheader_may_trap`, and a hoist re-anchored into the pre-header
        // keeps its (lower) node id, so it would run BEFORE the class guard
        // it was relying on. Not on the OSR route or an OSR-door graph (their
        // entries skip the pre-header).
        let mut class_predicated: FxHashSet<NodeId> = FxHashSet::default();
        if !ctx.osr_route && !graph.osr_compile && class_predicate::loop_class_predicate_enabled() {
            let before = graph.nodes.len();
            let out = class_predicate::plant_class_predicates(graph, despec, ctx.method_key);
            tag_predicate_guards(graph, before, crate::deopt::PredicateKind::Class);
            class_predicated.extend(out.headers.iter().copied());
            if out.planted > 0 {
                crate::ir_evidence::note(crate::ir_evidence::Transform::Licm);
                prune_constant_ifs(graph);
                scalar_cleanup(graph);
                // The same field forward an OSR-door graph runs, for the loops
                // just guarded: their entry already refuses an OSR entry stub
                // (the guard at the header bci), so the value φ the forward
                // adds at the header costs no entry the body still had, and
                // the guard, planted in the pre-header before the forward's
                // reads (a lower node id in one block: `ir_schedule` keeps
                // impure nodes in id order), is the class proof those reads
                // need.
                if loop_field_forward::loop_field_forward_enabled()
                    && loop_field_forward::predicated_forward_enabled()
                    && loop_field_forward::forward_loop_fields_with(
                        graph,
                        Some(&out.headers),
                        Some((despec, ctx.method_key)),
                    ) > 0
                {
                    scalar_cleanup(graph);
                }
                // Wave 7b (lane ir-opt-inline): a loop whose only non-linear
                // node was a devirtualized call (`s += f.applyAsInt(i)` with
                // `f` the `i -> i + 1` lambda, its miss arm cut above) is
                // all-linear now, `s += (long) (i + 1)`; retire it to its
                // closed form as the loop rounds' `retire_accumulator_loops`
                // would have had the call not been there (`OpCostRows.lambda`,
                // both modes). Not on the OSR route or an OSR-door graph: the
                // enclosing test already says so.
                if loop_retire::loop_retire_enabled()
                    && loop_retire::retire_accumulator_loops(graph, Some((despec, ctx.method_key)))
                        > 0
                {
                    crate::ir_evidence::note(crate::ir_evidence::Transform::LoopPromoted);
                    eliminate_dead_nodes(graph);
                    scalar_cleanup(graph);
                }
            }
        }
        // JIT round 25 wave 6 (lane W6-C, `CRATONVM_JIT_IR_SEQ_CHARAT_PREDICATE`,
        // opt-in): the loops whose `CharSequence.charAt` receivers are invariant
        // and that the class predicate left unguarded (it plants only for a
        // receiver the body class-tests, and never on an OSR graph) get the
        // loop-entry `String` guard that licenses the trap and the charAt hoist
        // below (`seq_charat_predicate`). After the class predicate, which keeps
        // first claim on a loop; an OSR graph's loop only when every entry
        // passes its pre-header. The same field forward as above follows for the
        // guarded loops (whose calls now write no memory), on a method-entry
        // graph and on the OSR door's own (which already ran it over every
        // loop, before the calls were traps).
        let mut seq_census = seq_charat_predicate::SeqCharAtPredicateCensus::default();
        let mut seq_predicated_osr = false;
        // JIT round 25 wave 7 (lane W7-D): the loops this pass guarded, the
        // only ones W6C-P2 below is offered.
        let mut seq_predicated: FxHashSet<NodeId> = FxHashSet::default();
        if seq_charat_predicate::seq_charat_predicate_enabled() {
            let before = graph.nodes.len();
            let out = seq_charat_predicate::plant_seq_charat_predicates(
                graph,
                despec,
                ctx.method_key,
                ctx.osr_route,
            );
            tag_predicate_guards(graph, before, crate::deopt::PredicateKind::Class);
            seq_census = out.census;
            if !out.headers.is_empty() {
                crate::ir_evidence::note(crate::ir_evidence::Transform::Licm);
                scalar_cleanup(graph);
                seq_predicated_osr = ctx.osr_route || graph.osr_compile;
                if (!ctx.osr_route || graph.osr_compile)
                    && loop_field_forward::loop_field_forward_enabled()
                    && loop_field_forward::predicated_forward_enabled()
                    && loop_field_forward::forward_loop_fields_with(
                        graph,
                        Some(&out.headers),
                        Some((despec, ctx.method_key)),
                    ) > 0
                {
                    scalar_cleanup(graph);
                }
                class_predicated.extend(out.headers.iter().copied());
                seq_predicated.extend(out.headers.iter().copied());
            }
        }
        // JIT round 25 wave 7 (lane W7-D, W6C-P2,
        // `CRATONVM_JIT_IR_SEQ_CHARAT_FIELD_CLASS_PREDICATE`, opt-in): in the
        // loops guarded above, whose `charAt` calls now trap, a guarded
        // splice's receiver read from an invariant object's field
        // (`this.predicate`, `this.next`) gets one loop-entry read and class
        // guard, its in-body class tests fold and their miss-arm calls are cut
        // (`seq_charat_predicate::w7d`, which runs its own prune and cleanup).
        let mut w7d_census = seq_charat_predicate::w7d::W7dCensus::default();
        if seq_charat_predicate::w7d::field_class_enabled() && !seq_predicated.is_empty() {
            // JIT r25 wave 12 (W12-4): and the loops nested in them.
            let offered = r25_w12_4::with_nested(graph, &seq_predicated);
            let n = seq_charat_predicate::w7d::plant_field_class_predicates(
                graph,
                despec,
                ctx.method_key,
                &offered,
                &mut w7d_census,
            );
            if n > 0 {
                crate::ir_evidence::note(crate::ir_evidence::Transform::Licm);
            }
        }
        // JIT rx27 wave 4 (lane scanloop, P-1,
        // `CRATONVM_JIT_IR_SEQ_CHARAT_IDENTITY_TESTS`, opt-in): in the same
        // loops, after P2 (whose writer rule refuses the regex position loop),
        // each guarded-splice class test of a read whose base chain is
        // invariant becomes an identity compare with one loop-entry read of
        // the chain (`seq_charat_predicate::w7d`, the rx27 wave 4 section).
        if seq_charat_predicate::w7d::identity_tests_enabled() && !seq_predicated.is_empty() {
            let n = seq_charat_predicate::w7d::plant_identity_tests(
                graph,
                despec,
                ctx.method_key,
                &seq_predicated,
            );
            if n > 0 {
                crate::ir_evidence::note(crate::ir_evidence::Transform::Licm);
                scalar_cleanup(graph);
            }
        }
        // nio21 wave 7 (lane L5): the same forward for the loops whose
        // trap-form receiver guard `plant_length_predicates` (above) already
        // moved to the pre-header -- the default for `java.nio` buffer sites
        // -- which the class predicate never sees. See
        // `class_predicate::class_guard_hoisted_headers`.
        // JIT round 20 wave 19 (lane yBuf, MEM20-4): with that switch off (its
        // default), the heap-buffer loops among them still are
        // (`CRATONVM_JIT_IR_LOOP_FIELD_FORWARD_HEAP_BUFFER`,
        // `class_predicate::heap_buffer_loops`).
        let guard_hoisted_all = class_predicate::guard_hoisted_forward_enabled();
        if !ctx.osr_route
            && !graph.osr_compile
            && loop_field_forward::loop_field_forward_enabled()
            && loop_field_forward::predicated_forward_enabled()
            && (guard_hoisted_all || class_predicate::heap_buffer_forward_enabled())
        {
            let mut headers =
                class_predicate::class_guard_hoisted_headers(graph, &class_predicated);
            // The r23 L8 heap-buffer close: and the spliced kernels whose
            // receiver the planner proved exact (no class guard to hoist).
            if class_predicate::spliced_exact_forward_enabled() {
                headers.extend(class_predicate::spliced_exact_receiver_loops(
                    graph,
                    &class_predicated,
                ));
            }
            if !guard_hoisted_all {
                headers = class_predicate::heap_buffer_loops(graph, &headers);
            }
            if !headers.is_empty() {
                if false {
                    eprintln!(
                        "[ir-licm] loop field forward: {} loop(s) with a hoisted class guard",
                        headers.len()
                    );
                }
                if loop_field_forward::forward_loop_fields_with(
                    graph,
                    Some(&headers),
                    Some((despec, ctx.method_key)),
                ) > 0
                {
                    crate::ir_evidence::note(crate::ir_evidence::Transform::Licm);
                    scalar_cleanup(graph);
                    // The r23 L8 heap-buffer close
                    // (`CRATONVM_JIT_IR_PREDICATES_AFTER_FIELD_FORWARD`): the
                    // forward made the buffer's fields pre-header values, so
                    // the guards on them (`segment != null`, `k << 2 < limit
                    // - 3`) are invariant or affine only now. The length and
                    // affine predicates ran before it.
                    if affine_guard_predicate::affine_guard_predicate_enabled()
                        && affine_guard_predicate::predicates_after_field_forward_enabled()
                    {
                        let before = graph.nodes.len();
                        let n = affine_guard_predicate::plant_affine_guard_predicates_after_forward(
                            graph,
                            despec,
                            ctx.method_key,
                            &headers,
                        );
                        tag_predicate_guards(graph, before, crate::deopt::PredicateKind::Range);
                        if n > 0 {
                            eliminate_dead_nodes(graph);
                        }
                    }
                }
            }
        }
        // The r23 L8 heap-buffer close
        // (`CRATONVM_JIT_IR_PREDICATES_AFTER_FIELD_FORWARD`): the same pass for
        // the loops whose reads the null-base / checkcast predicates moved to
        // the pre-header. Such a loop leaves the field forward nothing to do
        // (`absGet` compiled alone: `no forwardable field`), so the run above
        // never saw it, and its `limit - 3` bound and the `segment != null`
        // test became invariant only with those reads.
        if predicated_reads > 0
            && affine_guard_predicate::affine_guard_predicate_enabled()
            && affine_guard_predicate::predicates_after_field_forward_enabled()
        {
            let headers = loops_predicated_since(graph, predicates_from);
            if !headers.is_empty() {
                let before = graph.nodes.len();
                let n = affine_guard_predicate::plant_affine_guard_predicates_after_forward(
                    graph,
                    despec,
                    ctx.method_key,
                    &headers,
                );
                tag_predicate_guards(graph, before, crate::deopt::PredicateKind::Range);
                if n > 0 {
                    crate::ir_evidence::note(crate::ir_evidence::Transform::Licm);
                    eliminate_dead_nodes(graph);
                }
            }
        }
        // nio21 wave 21 (lane L3): a trapping `CharSequence.charAt` on a
        // receiver the forward just made invariant (`src.str`): its String
        // class test and `value` / `coder` / length reads at the loop entry,
        // the index check and the element load in the body
        // (`charat_trap::hoist_seq_charat_string_reads`). After the forward
        // (which makes the receiver a pre-header value), before the offset
        // predicate.
        // JIT round 25 wave 6 (lane W6-C): on an OSR graph too once
        // `seq_charat_predicate` planted there -- the only planter of a
        // licensing guard on such a graph, and only for a loop every entry
        // reaches through its pre-header, where the hoist puts its reads.
        if !class_predicated.is_empty()
            && ((!ctx.osr_route && !graph.osr_compile) || seq_predicated_osr)
            && charat_trap::seq_charat_hoist_enabled()
        {
            let hoisted = charat_trap::hoist_seq_charat_string_reads(graph, despec, ctx.method_key);
            seq_census.hoisted = hoisted;
            if hoisted > 0 {
                crate::ir_evidence::note(crate::ir_evidence::Transform::Licm);
                scalar_cleanup(graph);
            }
            // JIT round 25 wave 7 (lane W7-D, W6C-P3,
            // `CRATONVM_JIT_IR_SEQ_CHARAT_ENTRY_BOUND`, opt-in): a hoisted
            // call's per-character `i < count` (and `0 <= i`) guards answered
            // once at the loop entry from the loop's own `i < x` test.
            if hoisted > 0 && seq_charat_predicate::w7d::entry_bound_enabled() {
                // JIT r25 wave 12 (W12-4): and the loops nested in them.
                let offered = r25_w12_4::with_nested(graph, &class_predicated);
                let n = seq_charat_predicate::w7d::plant_entry_bounds(
                    graph,
                    despec,
                    ctx.method_key,
                    &offered,
                    &mut w7d_census,
                );
                if n > 0 {
                    crate::ir_evidence::note(crate::ir_evidence::Transform::Licm);
                    scalar_cleanup(graph);
                }
            }
        }
        // `CRATONVM_JIT_IR_LATE_LICM`: the predicates above folded these loops'
        // class tests and cut their miss-arm calls, after the loop rounds' LICM
        // had already given up on them; one more read hoist takes what is now invariant.
        // It also runs with no predicated loop: the early rounds refuse any loop whose
        // pre-header may trap (`Matcher.reset`'s `groups` fill spliced after `SUBJ[i]`),
        // and only this round's end-of-pre-header copies can hoist past such a node.
        if (!class_predicated.is_empty() || true) && !ctx.osr_route && !graph.osr_compile {
            graph.late_licm = true;
            let moved = licm_outcome(graph).any_work();
            graph.late_licm = false;
            if moved {
                crate::ir_evidence::note(crate::ir_evidence::Transform::Licm);
                scalar_cleanup(graph);
                // A hoisted `matcher.to` is the invariant `x` of `i < x` the
                // charAt entry bound could not find before.
                if seq_charat_predicate::w7d::entry_bound_enabled() {
                    let offered = r25_w12_4::with_nested(graph, &class_predicated);
                    if seq_charat_predicate::w7d::plant_entry_bounds(
                        graph,
                        despec,
                        ctx.method_key,
                        &offered,
                        &mut w7d_census,
                    ) > 0
                    {
                        scalar_cleanup(graph);
                    }
                }
            }
        }
        if hoist_invariant_guards_and_loads_with_despec(
            graph,
            ctx.osr_route,
            Some((despec, ctx.method_key)),
        ) > 0
        {
            crate::ir_evidence::note(crate::ir_evidence::Transform::Licm);
            scalar_cleanup(graph);
        }
        if w7_trap_free_preheader::trap_free_preheader_enabled() {
            for _ in 0..2 {
                let census = w7_trap_free_preheader::plant_trap_free_preheaders(
                    graph,
                    despec,
                    ctx.method_key,
                    ctx.osr_route,
                );
                if !census.changed() {
                    break;
                }
                crate::ir_evidence::note(crate::ir_evidence::Transform::GuardElided);
                prune_constant_ifs(graph);
                eliminate_dead_nodes(graph);
                scalar_cleanup(graph);
            }
        }
        seq_charat_predicate::note_census(&seq_census, ctx.method_key);
        seq_charat_predicate::w7d::note_census(&w7d_census, ctx.method_key);
        // JIT rx27 wave 1 (lane scanloop): one line per `charAt` loop, what
        // its per-character code still pays after the proofs above.
        charat_trap::note_scan_loop_census(graph, ctx.method_key);
        // nio21 wave 6 (lane L5): `a[i + o]` over an invariant array and
        // offset, from pre-header `x <= a.length - o`, `0 <= o`, `0 <= init`
        // guards that deopt to the loop entry (`offset_predicate`), read by
        // `ir_check_elim`'s variable-offset arm. AFTER the class predicate and
        // the field forward, which make `hb`, `offset` and `limit` pre-header
        // values and `position` a header phi. Not on the OSR route or an
        // OSR-door graph (their entries skip the pre-header).
        if !ctx.osr_route && !graph.osr_compile && offset_predicate::offset_predicate_enabled() {
            let before = graph.nodes.len();
            offset_predicate::plant_offset_predicates(graph, despec, ctx.method_key);
            tag_predicate_guards(graph, before, crate::deopt::PredicateKind::Offset);
        }
        // nio21 wave 16 (lane L3): once more, after the class predicate cut
        // the guarded splices' miss-arm calls (each was a stop for the walk).
        if !class_predicated.is_empty() {
            run_join_load_forward(graph, "after the class predicate");
        }
    }
    if !ctx.osr_route && !graph.osr_compile && hoist_pure_loop_class_init_guards(graph) > 0 {
        crate::ir_evidence::note(crate::ir_evidence::Transform::Licm);
        scalar_cleanup(graph);
    }
    let mut typecheck_changed = final_typecheck_fold::final_typecheck_fold(graph);
    if !ctx.osr_route && !graph.osr_compile {
        if let Some(despec) = ctx.despec {
            typecheck_changed += final_typecheck_fold::strengthen_final_checkcast_to_guard(
                graph,
                despec,
                ctx.method_key,
            );
        }
    }
    if typecheck_changed > 0 {
        crate::ir_evidence::note(crate::ir_evidence::Transform::Simplified);
        scalar_cleanup(graph);
    }
    if !ctx.osr_route && !graph.osr_compile {
        if unroll_enabled() && unroll_wide_pure::unroll_in_yielding_iota(graph, None, true) {
            crate::ir_evidence::note(crate::ir_evidence::Transform::Unrolled);
            scalar_cleanup(graph);
            run_linear_reassoc(graph);
        }
        if fold_range_redundant_masks(graph) {
            scalar_cleanup(graph);
        }
        eliminate_dead_nodes(graph);
    }
    // JIT round 23 wave 8 (lane W8-L7, TYPECHECK21-1 part C;
    // `CRATONVM_JIT_IR_CHECKCAST_VALUE_FORWARD`): LAST, after every pass that
    // reads a cast (the checkcast predicate, the class tests): each remaining
    // cast's value uses read its operand; the cast stays as a check.
    if !ctx.osr_route && checkcast_forward::checkcast_value_forward_enabled() {
        checkcast_forward::forward_checkcast_values(graph);
    }
    // Two predicate planters (the class predicate and the `charAt` one) can
    // test one receiver's class at one loop entry; one guard is enough.
    merge_identical_guards(graph);
    // JIT r23 w2c close: an unrolled copy's frame whose node a later pass
    // removed (a loop-entry predicate hoisted the copy's read out of the main
    // loop) describes nothing now; left in place it is a second, unclaimed
    // snapshot at its bci, which `ir_verify::check_frame_states` rejects.
    drop_unnamed_copy_frames(graph);
    // j27 wave 2 (lane STRIPMINE, `CRATONVM_JIT_IR_STRIP_MINE`, opt-in): after
    // every loop pass (none of them meets a nested strip), the pure main
    // loops poll once per strip (`strip_mine`'s module doc).
    strip_mine::run(graph);
    // JIT round 25 (item 4, `CRATONVM_JIT_IR_PROVEN_RECEIVER_FIELDS`): LAST,
    // once every field access has its final pc -- which of them the registry
    // says must not plant the deopting compactness guard again. Left `None`
    // without a registry: then the lowering plants none.
    if let Some(despec) = ctx.despec {
        if crate::ir_lower::ir_proven_receiver_fields_enabled() {
            graph.proven_compact_withdrawn_pcs =
                Some(proven_compact_withdrawn_pcs(graph, despec, ctx.method_key));
        }
    }
    if liveness {
        note_snapshot_liveness_census(graph);
    }
    SNAPSHOT_LIVENESS_ON.with(|c| c.set(false));
    SPLICE_RESUME.with(|c| *c.borrow_mut() = None);
}

/// JIT round 25 (item 4): the field-access pcs of `graph` at which `despec`
/// records the deopting compactness guard's speculation as failed --
/// `(pc, speculation_id(pc, SpeculationFailed))`, the id
/// `Lowerer::emit_compact_receiver_trap` stamps through `emit_deopt_unless`
/// and the VM charges after `PER_BCI_DESPEC_LIMIT` traps. The lowering keeps
/// the full compact test / legacy arm at every listed pc.
///
/// Item 4 residual 1 (spliced withdrawal): a pc inside a spliced body with a
/// chain snapshot there publishes the callee's chain, whose trap the VM
/// charges at the OUTERMOST call site under
/// [`crate::deopt::chain_compact_speculation_id`] of the innermost scope
/// (`deopt_resume::chain_compact_charge_id`); such a pc is listed when that
/// id is withdrawn there ([`crate::deopt::chain_compact_withdrawn`]), so one
/// spliced site keeps its full sequence and the splice stays.
pub(in crate::ir_optimize) fn proven_compact_withdrawn_pcs(
    graph: &Graph,
    despec: &crate::deopt::DespecRegistry,
    method_key: &str,
) -> std::collections::HashSet<usize> {
    let mut out = std::collections::HashSet::new();
    for n in &graph.nodes {
        if !matches!(n.op, Op::Load(_) | Op::Store(_)) {
            continue;
        }
        let Some(pc) = n.bytecode_pc else {
            continue;
        };
        let Ok(bci) = u32::try_from(pc) else {
            continue;
        };
        if out.contains(&pc) {
            continue;
        }
        if spliced_compact_withdrawn(graph, despec, method_key, pc) {
            out.insert(pc);
            continue;
        }
        let id = crate::deopt::speculation_id(bci, crate::deopt::DeoptReason::SpeculationFailed);
        let null_id = crate::deopt::speculation_id(bci, crate::deopt::DeoptReason::NullCheck);
        if despec.contains_speculation(method_key, bci, id)
            || despec.contains_speculation(method_key, bci, null_id)
            || despec.implicit_null_trapped(method_key, bci)
        {
            out.insert(pc);
        }
    }
    if !out.is_empty() && crate::deopt::proven_compact_one_trap_enabled() {
        withdraw_every_site_on_a_withdrawn_base(graph, &mut out);
    }
    out
}

/// r23 L8 heap-buffer close (`CRATONVM_JIT_PROVEN_COMPACT_ONE_TRAP`): extend
/// the withdrawn pcs to every field access on the same receiver node as a
/// withdrawn one. Compactness is a property of the OBJECT, fixed at
/// allocation, so a legacy receiver that failed one site's guard fails the
/// guard every other site on it would plant next; withdrawing only the failed
/// pc moved the guard to the next access each recompile
/// (`HeapByteBuffer.getInt()`: `session()`'s `segment`, then `hb`, then
/// `bigEndian`), and the traps added up to `MakeNotCompilable`.
pub(in crate::ir_optimize) fn withdraw_every_site_on_a_withdrawn_base(
    graph: &Graph,
    out: &mut std::collections::HashSet<usize>,
) {
    let field_site = |n: &Node| matches!(n.op, Op::Load(_) | Op::Store(_));
    let bases: FxHashSet<NodeId> = graph
        .nodes
        .iter()
        .filter(|n| field_site(n) && n.bytecode_pc.is_some_and(|pc| out.contains(&pc)))
        .filter_map(|n| n.input_opt(2))
        .collect();
    for n in &graph.nodes {
        if let (true, Some(pc), Some(base)) = (field_site(n), n.bytecode_pc, n.input_opt(2)) {
            if bases.contains(&base) {
                out.insert(pc);
            }
        }
    }
}

/// Item 4 residual 1: does any chain snapshot at the spliced raw pc `pc`
/// name a compactness guard the registry withdrew at its call site? Every
/// chain snapshot at one raw pc names the same scopes (the pc is one
/// instruction of one spliced copy); any of them withdrawing is enough.
pub(in crate::ir_optimize) fn spliced_compact_withdrawn(
    graph: &Graph,
    despec: &crate::deopt::DespecRegistry,
    method_key: &str,
    pc: usize,
) -> bool {
    graph.safepoints.iter().any(|sp| {
        if sp.bci != pc {
            return false;
        }
        let Some(chain) = sp.splice_scope.as_deref() else {
            return false;
        };
        match (chain.scopes.first(), chain.scopes.last()) {
            (Some(inner), Some(outer)) if chain.scopes.len() >= 2 => {
                crate::deopt::chain_compact_withdrawn(
                    despec,
                    method_key,
                    outer.bci,
                    &inner.method_key,
                    inner.bci,
                )
            }
            _ => false,
        }
    })
}

/// nio21 wave 16 (lane L3): are the loop passes' per-loop verdicts reported?
/// `CRATONVM_DBG_LICM` (the passes' own token), `ir-bailout` or
/// `ir-compiles` (the tokens a host trace of a compile already carries), so a
/// trace names why a loop kept its per-iteration reads.
pub(crate) fn loop_pass_report_enabled() -> bool {
    false
}

/// JIT round 21 wave 1 (lane loops, LOOPS21-2): record every `Op::Guard` a
/// loop-entry planter added (node ids from `before` on; `Graph::add` appends)
/// as planter `kind`'s, so the lowering gives it the planter's own
/// speculation id (`crate::deopt::predicate_speculation_id`). Nothing with
/// `CRATONVM_JIT_IR_PREDICATE_OWN_SPECULATION` off, which keeps
/// `Graph::predicate_guard_kinds` empty and every point's id as before.
pub(in crate::ir_optimize) fn tag_predicate_guards(
    graph: &mut Graph,
    before: usize,
    kind: crate::deopt::PredicateKind,
) {
    if before >= graph.nodes.len() || !crate::deopt::predicate_own_speculation_enabled() {
        return;
    }
    for i in before..graph.nodes.len() {
        if matches!(graph.nodes[i].op, Op::Guard { .. }) {
            // Cast: arena index to node id.
            graph.predicate_guard_kinds.push((i as NodeId, kind as u8));
        }
    }
}

/// JIT round 25 wave 2 (lane branch-fold): the folds of [`bool_branch`],
/// each round followed by the constant-`If` prune (an implied `If` or a
/// boolean compare folded to a constant leaves one) and the scalar cleanup,
/// until a round rewrites nothing. A no-op with every switch off.
pub(in crate::ir_optimize) fn run_bool_branch_folds(
    graph: &mut Graph,
) -> bool_branch::BoolBranchCensus {
    let mut census = bool_branch::BoolBranchCensus::default();
    if !bool_branch::any_enabled() {
        return census;
    }
    for _ in 0..4 {
        let round = bool_branch::fold_round(graph);
        census.add(&round);
        if round.rewrites() == 0 {
            break;
        }
        census.arms += prune_constant_ifs(graph);
        crate::ir_evidence::note(crate::ir_evidence::Transform::Simplified);
        scalar_cleanup(graph);
    }
    census
}

/// RB25-2 / RB25-3 (`CRATONVM_JIT_IR_SPLIT_IF_PARTIAL_PHI`): the split-if
/// to a fixpoint (a split exposes the next nested join), at most three
/// rounds, each followed by the cleanup and the constant-`If` prune. The
/// caller runs the boolean folds after it (whose masks and compares expose
/// more) and then this once more. A no-op with the switch off. Returns how
/// many `If`s were split.
pub(in crate::ir_optimize) fn run_partial_split_rounds(graph: &mut Graph) -> usize {
    if !split_if::split_if_partial_phi_enabled() {
        return 0;
    }
    let mut total = 0usize;
    for _ in 0..3 {
        let split = split_if::split_const_phi_ifs(graph);
        if loop_pass_report_enabled() && split.candidates > 0 {
            eprintln!(
                "[ir] split-if (partial round): {} candidate(s), {} split ({} with If copies, {} through a value phi), {} refused for a reachable frame in {}",
                split.candidates,
                split.split,
                split.partial,
                split.through_value_phis,
                split.refused_frames,
                graph.scope_method_key
            );
            if !split.shape_refusals.is_empty() {
                eprintln!(
                    "[ir] split-if shape refusals by check line: {:?}",
                    split.shape_refusals
                );
            }
        }
        if split.split == 0 {
            break;
        }
        total += split.split;
        crate::ir_evidence::note(crate::ir_evidence::Transform::Simplified);
        scalar_cleanup(graph);
        if prune_constant_ifs(graph) > 0 {
            scalar_cleanup(graph);
        }
    }
    total
}

/// The `CRATONVM_DBG_JITC` line of the branch folds, one per body: what the
/// [`bool_branch`] folds and the split-if extension
/// (`CRATONVM_JIT_IR_SPLIT_IF_MEMORY_PHI`) did. Printed whenever one of their
/// switches is on, zeros included, so a zero is a measurement.
pub(in crate::ir_optimize) fn note_branch_fold_census(
    census: &bool_branch::BoolBranchCensus,
    split_through_memory_phis: usize,
    split_partial: usize,
    method_key: &str,
) {
    if !crate::x64::dbg_jitc_enabled()
        || !(bool_branch::any_enabled()
            || split_if::split_if_memory_phi_enabled()
            || split_if::split_if_partial_phi_enabled())
    {
        return;
    }
    eprintln!(
        "[cratonvm-jitc] ir-branch-fold {}: branches folded={} arms removed={} phis simplified={} \
         compares simplified={} diamonds collapsed={} split-if through memory phis={} \
         split-if partial={} masks={}",
        if method_key.is_empty() {
            "?"
        } else {
            method_key
        },
        census.branches,
        census.arms,
        census.phis,
        census.compares,
        census.diamonds,
        split_through_memory_phis,
        split_partial,
        census.masks
    );
}

/// nio21 wave 16 (lane L3): [`join_load_forward::forward_join_loads`] under
/// its switch, with the cleanup and a census line.
pub(in crate::ir_optimize) fn run_linear_reassoc(graph: &mut Graph) {
    if !linear_reassoc::linear_reassoc_enabled() {
        return;
    }
    let census = linear_reassoc::reassociate_linear_trees(graph);
    if census.trees == 0 {
        return;
    }
    crate::ir_evidence::note(crate::ir_evidence::Transform::Simplified);
    scalar_cleanup(graph);
    if loop_pass_report_enabled() {
        eprintln!(
            "[ir] linear reassoc: {} tree(s), {} node(s) -> {}",
            census.trees, census.removed, census.added
        );
    }
}

pub(in crate::ir_optimize) fn run_join_load_forward(graph: &mut Graph, when: &str) {
    if !join_load_forward::join_load_forward_enabled() {
        return;
    }
    let c = join_load_forward::forward_join_loads(graph);
    if c.removed == 0 {
        return;
    }
    crate::ir_evidence::note(crate::ir_evidence::Transform::Simplified);
    scalar_cleanup(graph);
    if loop_pass_report_enabled() {
        eprintln!(
            "[ir] join load forward ({when}): {} read(s) answered, {} from a store, {} value phi(s)",
            c.removed, c.from_stores, c.phis
        );
    }
}

/// The body of [`optimize`], split out so the per-compile thread-local state
/// above is set and cleared around it on every path.
pub(in crate::ir_optimize) fn optimize_passes(
    graph: &mut Graph,
    osr_route: bool,
    early_mask: Option<(&crate::deopt::DespecRegistry, &str)>,
    // JIT round 20 wave 3 (lane loopopt): the field forward's de-spec
    // question (`loop_field_forward::forward_loop_fields_with`).
    despec: Option<(&crate::deopt::DespecRegistry, &str)>,
) {
    // Node count before any pass, so the fixpoint loop below can report whether
    // it actually removed anything. Diagnostic only -- see
    // `ir_evidence::Transform::Simplified`.
    let nodes_before = graph.live_count();
    // Affine strength-reduction (reassociation). Collapses an unrolled affine
    // recurrence such as `x = x*c1 + c2` (×N) into a single `k*root + c` — the
    // optimization C2 performs via Mul/Add reassociation, which dominated the
    // CratonVM-vs-HotSpot CPU gap on compute kernels. Gated default-OFF behind
    // `CRATONVM_JIT_REASSOC` while it soaks (the IR path it feeds is also
    // gate-relaxed for branchy integer loops in `lib.rs`).
    // Round 17 (`r16-codegen-loop-and-straight-line-anatomy-FIXED-20261005.md`):
    // a collapsed chain is real work the single-pass body has no equivalent
    // for, but it ADDS nodes (each rewritten value becomes `k*root + c`), so
    // the node-count test below never saw it and the acceptance gate threw
    // the body away ("evidence: none"). Count the rewrites instead.
    if reassoc_enabled() && reassociate_affine(graph) > 0 {
        crate::ir_evidence::note(crate::ir_evidence::Transform::Simplified);
    }
    // Round 17: `x * (2^n - 1) + k` as `(x << n) - (x - k)` -- two cycles of
    // latency where the multiply-add takes four. See `strength_reduce_mul`.
    if mul_strength_reduce_enabled() && strength_reduce_mul(graph) > 0 {
        crate::ir_evidence::note(crate::ir_evidence::Transform::Simplified);
    }
    // Round 14 wave 6 (lane sync6, S5-2): `Thread.holdsLock(this)` calls the
    // builder kept in a synchronized method (a loop φ it could not trust
    // mid-walk) become `1` on the finished graph, before the cleanup, which
    // then folds the branches they fed.
    if crate::ir::ir_holds_lock_method_fold_enabled() {
        fold_method_monitor_holds_lock(graph);
    }
    if final_typecheck_fold::final_typecheck_fold(graph) > 0 {
        crate::ir_evidence::note(crate::ir_evidence::Transform::Simplified);
    }
    // The scalar fixed point: fold / simplify / GVN / eliminate, to
    // convergence. See [`scalar_cleanup`] for the convergence rule and what it
    // used to miss.
    scalar_cleanup(graph);
    // Round 12 wave 3 (W2-4): compares the range lattice decides, once per
    // compile, on the cleaned graph; the cleanup then folds what they unlock.
    if fold_range_decided_compares(graph) | fold_range_redundant_masks(graph) {
        scalar_cleanup(graph);
    }
    // Round 12 wave 6 (lane iropt5): cut the dead arm of every `If` whose
    // condition is now a constant, then let the cleanup reclaim what it fed
    // (and normalise the dead arm's snapshot slots). Before the loop rounds,
    // so LICM and the unroller never see the arm. The r23 L8 close: an arm
    // that opens with an always-failing guard is made such an arm first
    // (`hoist_arm_entry_traps`).
    let hoisted = hoist_arm_entry_traps(graph);
    if hoisted > 0 {
        crate::ir_evidence::note(crate::ir_evidence::Transform::Simplified);
    }
    if prune_constant_ifs(graph) > 0 || hoisted > 0 {
        scalar_cleanup(graph);
    }
    if !osr_route && !graph.osr_compile && hoist_pure_loop_class_init_guards(graph) > 0 {
        scalar_cleanup(graph);
    }
    // JIT round 20 wave 18 (lane xSplitIf, LOOPOPT20-1,
    // `CRATONVM_JIT_IR_SPLIT_IF_CONST_PHI`): a branch on a phi of constants
    // (a spliced boolean getter's `iconst_1` / `iconst_0` join) routes each
    // predecessor of the join to the successor its constant selects, so the
    // loop passes below see the callee's own test as the loop's exit test.
    // Before the join load forward, which then serves the body's re-reads
    // from the header block through branch projections.
    let split_through_memory_phis = {
        let split = split_if::split_const_phi_ifs(graph);
        if loop_pass_report_enabled() && split.candidates > 0 {
            eprintln!(
                "[ir] split-if on a constant phi: {} candidate(s), {} split ({} through a memory phi or a non-empty arm, {} with If copies, {} through a value phi), {} refused for a reachable frame in {}",
                split.candidates, split.split, split.through_memory_phis, split.partial, split.through_value_phis, split.refused_frames, graph.scope_method_key
            );
            if !split.shape_refusals.is_empty() {
                eprintln!(
                    "[ir] split-if shape refusals by check line: {:?}",
                    split.shape_refusals
                );
            }
        }
        if split.split > 0 {
            crate::ir_evidence::note(crate::ir_evidence::Transform::Simplified);
            scalar_cleanup(graph);
        }
        split.through_memory_phis
    };
    // JIT round 25 wave 2 (lane branch-fold, opt-in): the boolean φs, boolean
    // compares, empty diamonds and branch-implied `If`s (`bool_branch`).
    // After the split-if, which keeps first claim on a constant φ under a
    // loop test (it routes the edges; this pass would only rename the φ),
    // and before the loop rounds, so they see one compare-and-branch.
    // JIT round 26: the split-if keeps first claim through every round it can
    // take (a nested spliced boolean's inner join is a candidate only once the
    // outer one is split); the boolean-φ fold would otherwise turn the inner
    // `phi(1, 0)` into a materialised compare the split could have threaded.
    let mut split_partial = run_partial_split_rounds(graph);
    let mut branch_census = run_bool_branch_folds(graph);
    let more = run_partial_split_rounds(graph);
    if more > 0 {
        split_partial += more;
        let round = run_bool_branch_folds(graph);
        branch_census.add(&round);
    }
    // nio21 wave 16 (lane L3): field reads answered through forward merges
    // (`join_load_forward`), before the loop rounds so LICM and the field
    // forward see one read per cell and iteration.
    run_join_load_forward(graph, "early");
    // Version the FFM diamonds before the field hoist and loop rounds consume
    // the two now-monomorphic loops.
    if !osr_route && !graph.osr_compile && class_unswitch::unswitch_bimorphic_class_loops(graph) > 0
    {
        crate::ir_evidence::note(crate::ir_evidence::Transform::Licm);
    }
    // JIT round 23 wave 8 (lane W8-L4, `CRATONVM_JIT_IR_FFM_FIELD_HOIST`): the
    // FFM element expansion's segment-field reads (final fields of an
    // invariant segment) to the loop's pre-header, deopting to the loop entry
    // (`ffm_field_hoist`). Before the static hoist, which then sees a loop
    // without those opaque reads and moves the layout `getstatic` out too, and
    // before the loop rounds. Not on the OSR route or an OSR-door graph (their
    // entries skip the pre-header).
    if let Some((despec, method_key)) = despec {
        if !osr_route
            && !graph.osr_compile
            && ffm_field_hoist::ffm_field_hoist_enabled()
            && ffm_field_hoist::hoist_ffm_field_reads(graph, despec, method_key) > 0
        {
            crate::ir_evidence::note(crate::ir_evidence::Transform::Licm);
            scalar_cleanup(graph);
        }
    }
    // nio20 wave 6 (lane L4): a direct `getstatic` out of a loop that writes
    // no memory (`loop_static_hoist`), before the loop rounds, so LICM and the
    // unroller see a loop with no opaque node and no memory φ threaded
    // through it, and the range predicates see an invariant array. Not on the
    // OSR route (see that module).
    // JIT round 19 wave 12 (lane hoist12, HOIST11-4): on the OSR route only
    // under `CRATONVM_JIT_IR_LOOP_STATIC_HOIST_OSR_ROUTE`, which moves only
    // the reads the OSR entry stub can re-read.
    // JIT r20 wave 19 (lane yLoopPred, LOOPOPT20-3): or under
    // `CRATONVM_JIT_IR_OSR_ROUTE_ARM_PREDICATES`, which moves every direct read
    // of a loop an opaque OSR arm enters through its pre-header (the arm runs
    // the moved read) and nothing else unless HOIST11-4 is on too.
    if (!osr_route
        || loop_static_hoist::loop_static_hoist_osr_route_enabled()
        || osr_route_arm_predicates_enabled())
        && loop_static_hoist::loop_static_hoist_enabled()
        && loop_static_hoist::hoist_loop_static_reads_in(
            graph,
            !osr_route || loop_static_hoist::loop_static_hoist_osr_route_enabled(),
        ) > 0
    {
        crate::ir_evidence::note(crate::ir_evidence::Transform::Licm);
        scalar_cleanup(graph);
    }
    // JIT round 17 wave 2 (lane irfeed, MON3R17-1;
    // `r17-acceptance-gate-simplified-is-a-net-node-count-CLOSED-20261006.md`): did the
    // passes that only REMOVE work shrink the graph? Measured here, before the
    // loop rounds, because every pass from here on may grow it (the partial
    // unroller's copies, the window and block coarsenings' copies) and the
    // cleanup after a growing pass reclaims what that pass itself retired, so
    // the end-of-pipeline net count below cannot tell a fold from a copy.
    // Noted at the end; evidence only under
    // `CRATONVM_JIT_IR_EVIDENCE_SIMPLIFIED_BEFORE_GROWTH`.
    let simplified_before_growth = graph.live_count() < nodes_before;
    // nio21 wave 5 (lane L5): the early mask predicate (see `optimize_with`),
    // after the measurement above (it adds nodes) and before the loop rounds.
    if let Some((despec, method_key)) = early_mask {
        // JIT r20 wave 19 (lane yLoopPred, LOOPOPT20-6,
        // `CRATONVM_JIT_IR_UNROLL_NAMED_FRAME_PCS`): the single-input `Merge`
        // wrappers the builder puts around branch projections first -- the
        // normalisation LICM and the unroller run before they look at a loop.
        // The early plant's candidate test asks `analyze_counted_loop`, which
        // reads the back edge as the exit test's own projection; with the
        // wrapper still in place every javac loop answered "not a
        // constant-trip counted loop" and no loop was ever an early candidate
        // (the fence row's 0 `early (unroll candidate)` lines on m7 and w3f).
        if unroll_named_frame_pcs_enabled() {
            collapse_trivial_merges(graph);
        }
        let before = graph.nodes.len();
        let early_planted = mask_predicate::plant_mask_predicates(graph, despec, method_key, true);
        tag_predicate_guards(graph, before, crate::deopt::PredicateKind::Mask);
        if early_planted > 0 {
            crate::ir_evidence::note(crate::ir_evidence::Transform::Licm);
        }
    }
    // Full unrolling of small constant-trip counted loops. Default-**ON**;
    // `CRATONVM_JIT_UNROLL=0` turns it off (`unroll_enabled` is
    // `map_or(true, ..)`). This comment said "Default-OFF ... while it soaks"
    // until 2026-08-04; it had outlived the soak. It matters more than a stale
    // comment usually does, because the parity audit in
    // `x64/single_pass_only.rs` decides whether the optimizing tier may take a
    // method by asking which transforms it has, and this one is the reason the
    // single-pass native unroller is NOT on that veto list. Runs after
    // the fixed-point cleanup so init/stride/bound are already folded to
    // constants; re-runs the cleanup so the unrolled straight-line code folds
    // (the concrete induction values collapse the per-iteration computation)
    // and the retired loop nodes are reclaimed.
    //
    // ── Pass ORDER: unroll, then LICM — unless the order is flipped ────
    //
    // The unroller refuses a loop whose body holds an invariant `Op::Load`
    // still anchored to the header or the back edge (`escapes_or_pinned`): the
    // load is not in the clone set, so cloning the body would leave it naming a
    // control node the transform killed. LICM is the pass that re-anchors
    // exactly those loads to the pre-header — and it runs *after* the
    // unroller, so the unroller never sees the re-anchored form.
    //
    // That ordering is why `probes/FieldLoop.java`'s `sum` — `a += this.fx`,
    // the one loop the tiering inversion is actually measured on — refuses.
    // `this.fx` is loop-invariant, so its load is pinned to the header and the
    // partial unroller never reaches the shape whose per-iteration budget
    // (`docs/feature-designs/jit-ir-optimizations.md`
    // §4) prices unrolling at three of the six instructions separating the
    // tiers.
    //
    // `CRATONVM_JIT_IR_LICM_BEFORE_UNROLL=1` runs LICM first instead. Default
    // OFF: both passes are default-ON, so flipping their order also changes
    // which loops the *full* unroller takes, and that is a default-config
    // behaviour change that has to earn its place on evidence.
    //
    // ── The OUTER round, which is what actually settles the order ──────
    //
    // The paragraph above describes a conflict — LICM unpins what the unroller
    // needs unpinned, and runs after it — whose remedy shipped as an env var
    // nothing sets. An env var picks one of two orders; the conflict wants
    // both, because each pass exposes work to the other. `licm; unroll; licm`
    // is the shape that resolves it, and running the whole block twice is the
    // cheapest way to get it in EITHER flag setting.
    //
    // This used to be pinned at two rounds and the comment here said why: a
    // budget rather than a fixpoint, because `licm()` reported "changed" for a
    // compact-form load every time it saw one (there is no control edge to
    // move, so it could not tell a first visit from a second) and a loop driven
    // on that answer would not terminate.
    //
    // `run_licm_pass` reports honestly now — see [`LicmOutcome`] — so the break
    // below is a real convergence test: a round in which no edge was rewritten
    // and no node was killed cannot expose work to the next one. The count is
    // therefore a BACKSTOP and not the mechanism. It is kept, and kept small,
    // because "this cannot loop" should not rest on the monotonicity argument
    // alone: every transform in the block is monotone today (a load re-anchored
    // to the pre-header is skipped on the next visit; the unroller is latched),
    // and a future one that is not would spin a bound-free loop rather than
    // costing three wasted rounds.
    //
    // THE UNROLLER RUNS AT MOST ONCE across the rounds, which is the one place
    // "just run the block again" is not safe. A *partial* unroll keeps the loop
    // and emits `factor` copies of its body; offering that loop to the same
    // pass a second time would emit `factor` copies of the copies. A full
    // unroll retires the loop and could not repeat, but the two share one entry
    // point, so the latch is on the entry point.
    // JIT round 22 wave 1 (lane loops, LOOPS22-1,
    // `CRATONVM_JIT_IR_LOOP_RETIRE_ACCUMULATORS`, opt-in): a loop that is
    // ALREADY all-linear (a local accumulator, an empty loop) is retired before
    // the loop rounds, whose main/post unroller would otherwise copy its body
    // into a shape this pass no longer recognises. The promoted cells' loops
    // are taken after the promotions below.
    if !osr_route && !graph.osr_compile && loop_retire::loop_retire_enabled() {
        let retired = loop_retire::retire_accumulator_loops(graph, despec);
        if retired > 0 {
            crate::ir_evidence::note(crate::ir_evidence::Transform::LoopPromoted);
            scalar_cleanup(graph);
        }
    }
    let licm_first = licm_before_unroll_enabled();
    let mut may_unroll = unroll_enabled();
    // TYPECHECK21-6: did a loop promotion below keep cells in registers?
    let mut promoted_cells = false;
    for _ in 0..MAX_LOOP_ROUNDS {
        let before = graph.live_count();
        let mut progressed = false;
        if licm_first {
            progressed |= run_licm_pass(graph);
        }
        if may_unroll && unroll_wide_pure::unroll_in_yielding_iota(graph, None, false) {
            may_unroll = false;
            progressed = true;
            crate::ir_evidence::note(crate::ir_evidence::Transform::Unrolled);
            // Same convergence rule as the first cleanup above, and it matters
            // more here: unrolling replaces an induction phi with concrete
            // constants, so the post-unroll round is the one most likely to be
            // fold-only.
            scalar_cleanup(graph);
        }
        if !licm_first {
            progressed |= run_licm_pass(graph);
        }
        if !progressed && graph.live_count() == before {
            break;
        }
    }
    // JIT round 24 item C (`CRATONVM_JIT_IR_UNROLL_CFG_BODY`, default ON since j27): a
    // constant-trip loop whose body branches, which the full unroller above
    // cannot take, is cloned per trip and its copies' branches fold on their
    // constant induction values. After the loop rounds, so LICM has already
    // taken what is invariant out of the body.
    if may_unroll_cfg_body() && cfg_unroll::unroll_cfg_loops(graph) > 0 {
        crate::ir_evidence::note(crate::ir_evidence::Transform::Unrolled);
        prune_constant_ifs(graph);
        scalar_cleanup(graph);
    }
    // OSR round (2026-10-06): the fields of a loop-invariant object that no
    // other write in the loop can reach are read once, in the pre-header, and
    // forwarded through the loop (`loop_field_forward`). After the loop rounds,
    // which settle what is invariant; before escape analysis, which handles
    // the objects this compile allocates and so is left every such base.
    //
    // OSR-door compiles only (`Graph::osr_compile`): every entry of such a
    // body arrives through an opaque arm, whose merge is the pre-header the
    // reads sit in. An ordinary entry stub jumps to the header itself, past
    // them, and would be withheld for the value φ it cannot seed
    // (`Lowerer::emit_osr_entry_stubs`) -- a cost to a method-entry body
    // whose loops escape analysis mostly handles already.
    if graph.osr_compile
        && loop_field_forward::loop_field_forward_enabled()
        && loop_field_forward::forward_loop_fields_with(graph, None, despec) > 0
    {
        scalar_cleanup(graph);
        // JIT round 19 wave 2 (lane osr2, FG16-5 / IROPS19-6,
        // `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE`, opt-in): of the forwarded
        // cells, the `int`/`long` ones of a loop that cannot transfer to the
        // interpreter lose their in-loop stores to one conditional write-back
        // at the exit (`loop_field_forward::promote_loop_cells`). Store
        // sinking is loop-invariant code motion, so it is noted as `Licm`
        // evidence for the acceptance gate.
        if loop_field_forward::promote_enabled()
            && loop_field_forward::promote_loop_cells(graph) > 0
        {
            crate::ir_evidence::note(crate::ir_evidence::Transform::LoopPromoted);
            scalar_cleanup(graph);
            promoted_cells = true;
        }
    }
    // JIT r22 nio hand-ins (W19L3-2,
    // `CRATONVM_JIT_IR_LOOP_FIELD_FORWARD_METHOD_ENTRY`, opt-in): the same
    // forward over every loop of a method-entry body, at the cost of the
    // entry stub of each header it gives a value φ.
    if !graph.osr_compile
        && !osr_route
        && loop_field_forward::method_entry_forward_enabled()
        && loop_field_forward::loop_field_forward_enabled()
        && loop_field_forward::forward_loop_fields_with(graph, None, despec) > 0
    {
        crate::ir_evidence::note(crate::ir_evidence::Transform::Licm);
        scalar_cleanup(graph);
    }
    // JIT round 21 wave 1 (lane loops, HOIST13-2,
    // `CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_METHOD_ENTRY`, opt-in): the same
    // forward + cell promotion in a METHOD-ENTRY body, for the loops whose
    // store census shows a field accumulator of an invariant base
    // (`this.sum += v`). Before the static promotion, as on the OSR door (a
    // header the field promotion took is left alone by the static one).
    // JIT r21 wave 1 (`CRATONVM_JIT_IR_LOOP_PROMOTE_SPLICED`, opt-in): the
    // same run for the loops of SPLICED callees in this body, the copy a
    // small hot kernel actually runs as (its base is the splice's
    // null-checked receiver, `loop_field_forward::base_null_checked_before`).
    if !graph.osr_compile
        && !osr_route
        && (loop_field_forward::method_entry_promote_enabled()
            || loop_field_forward::promote_spliced_enabled())
        && loop_field_forward::loop_field_forward_enabled()
        && loop_field_forward::promote_enabled()
    {
        let (forwarded, promoted) =
            loop_field_forward::promote_method_entry_accumulators(graph, despec);
        if promoted > 0 {
            crate::ir_evidence::note(crate::ir_evidence::Transform::LoopPromoted);
            // j27 wave 1 (lane LOOPCG): these headers join
            // `promoted_cell_header_bcis` like the OSR-door promotion's, so
            // the after-promote static hoist and unroll below are offered
            // them too (each keeps its own refusals).
            promoted_cells = true;
        }
        if forwarded > 0 || promoted > 0 {
            scalar_cleanup(graph);
        }
    }
    // JIT round 20 wave 11 (lane pLoop, OSR4-2 / IROPS2-6,
    // `CRATONVM_JIT_IR_LOOP_STATIC_PROMOTE`, default ON): the statics twin of
    // the field promotion -- a direct primitive static a call-free,
    // deopt-free loop stores (`COUNTER += x`) is read once in the pre-header,
    // carried in a register and written back at the exit
    // (`loop_field_forward::promote_loop_statics`). Same OSR-door placement;
    // it needs no field forward first (a static cell has no base to prove).
    // JIT round 20 wave 17 (lane pAtom2,
    // `CRATONVM_JIT_IR_LOOP_STATIC_PROMOTE_METHOD_ENTRY`, default ON): in a
    // method-entry body too (`static_promote_method_entry_enabled`'s doc says
    // why each entry stays exact).
    if (graph.osr_compile || loop_field_forward::static_promote_method_entry_enabled())
        && loop_field_forward::promote_enabled()
        && loop_field_forward::static_promote_enabled()
        && loop_field_forward::promote_loop_statics(graph) > 0
    {
        crate::ir_evidence::note(crate::ir_evidence::Transform::LoopPromoted);
        scalar_cleanup(graph);
        promoted_cells = true;
    }
    // JIT round 20 wave 17 (lane pAtom2, `CRATONVM_JIT_IR_LOOP_FRESH_FIELD_PROMOTE`,
    // default ON): the `int` / `long` fields of a fresh object of this compile
    // that nothing can publish (the accumulator `Acc a = new Acc(); for (..)
    // a.v += ..; return a;`), kept in registers through a call-free,
    // deopt-free loop and written back at its exit
    // (`loop_field_forward::promote_fresh_loop_fields`). Method-entry and
    // OSR-door bodies alike; before escape analysis, which keeps such an
    // object (it escapes after the loop) and sees only the fewer accesses.
    if loop_field_forward::promote_enabled()
        && loop_field_forward::fresh_promote_enabled()
        && loop_field_forward::promote_fresh_loop_fields(graph) > 0
    {
        crate::ir_evidence::note(crate::ir_evidence::Transform::LoopPromoted);
        scalar_cleanup(graph);
        promoted_cells = true;
    }
    // JIT round 21 (lane loopstatic, `CRATONVM_JIT_IR_LOOP_STATIC_HOIST_AFTER_PROMOTE`,
    // default ON): the early static-read hoist above saw each promoted loop
    // with its cells' stores and left its direct `getstatic`s inside; the
    // promotion (no deopt pad) leaves a loop that writes nothing, so they move
    // now -- before the unroller below, to which an in-loop `Op::LoadStatic`
    // is a side effect. Same gating as the early run.
    if promoted_cells
        && loop_static_hoist::loop_static_hoist_after_promote_enabled()
        && (!osr_route
            || loop_static_hoist::loop_static_hoist_osr_route_enabled()
            || osr_route_arm_predicates_enabled())
        && loop_static_hoist::loop_static_hoist_enabled()
        && loop_static_hoist::hoist_promoted_loop_static_reads_in(
            graph,
            !osr_route || loop_static_hoist::loop_static_hoist_osr_route_enabled(),
        ) > 0
    {
        crate::ir_evidence::note(crate::ir_evidence::Transform::Licm);
        scalar_cleanup(graph);
    }
    run_linear_reassoc(graph);
    // JIT round 21 wave 1 (lane loops, LOOPS21-1,
    // `CRATONVM_JIT_IR_LOOP_ARRAY_ELEMENT_PROMOTE`, opt-in): `a[k] += x` with
    // `a` and `k` invariant, the element carried in a register behind
    // pre-header index guards and written back at the exit
    // (`loop_field_forward::promote_loop_array_elements`). LAST of the cell
    // promotions: it leaves a header another one took alone, and the poll pad
    // has no record of an element (an exit there is refused).
    if !osr_route
        && loop_field_forward::promote_enabled()
        && loop_field_forward::array_element_promote_enabled()
        && loop_field_forward::promote_loop_array_elements(graph, despec) > 0
    {
        crate::ir_evidence::note(crate::ir_evidence::Transform::LoopPromoted);
        scalar_cleanup(graph);
    }
    // JIT round 22 wave 1 (lane loops, LOOPS22-1,
    // `CRATONVM_JIT_IR_LOOP_RETIRE_ACCUMULATORS`, opt-in): after the cell
    // promotions, `for (i = 0; i < n; i++) COUNTER += k` is a loop of two
    // register adds; it is replaced by its exit values (`i = max(0, n)`,
    // `COUNTER0 + trip * k`) and deleted, as C2 does
    // (`loop_retire::retire_accumulator_loops`). Not on an OSR route or an
    // OSR-door compile: an ordinary OSR entry stub needs the header it deletes.
    if !osr_route && !graph.osr_compile && loop_retire::loop_retire_enabled() {
        let retired = loop_retire::retire_accumulator_loops(graph, despec);
        if retired > 0 {
            crate::ir_evidence::note(crate::ir_evidence::Transform::LoopPromoted);
            scalar_cleanup(graph);
        }
    }
    // JIT round 21 (lane loopopt, TYPECHECK21-6,
    // `CRATONVM_JIT_IR_UNROLL_PROMOTED_LOOPS`): the promotions above run after
    // the loop rounds, so the unroller only ever saw a promoted loop WITH its
    // cells' loads and stores (a side effect it refuses); what the promotion
    // leaves is a register-only loop. Run AFTER `retire_accumulator_loops` so
    // constant-step linear accumulator loops (e.g. `static-counter-long`) still
    // retire to closed form instead of being split into main/post loops, while
    // non-retirable promoted accumulators (e.g. `p.x += i`) get wide pure
    // unrolling, `run_linear_reassoc`, and strip-mining.
    // j27 wave 6 (lane T1LOOPSTATIC, LOOPSTATIC21-3): under
    // `CRATONVM_DBG_UNROLL`, whether this unroll is offered the graph.
    if promoted_cells && promoted_unroll_census::enabled() {
        promoted_unroll_census::note(
            graph,
            unroll_enabled(),
            may_unroll,
            unroll_promoted_loops_enabled(),
        );
    }
    if promoted_cells
        && may_unroll
        && unroll_promoted_loops_enabled()
        && unroll_wide_pure::unroll_in_yielding_iota(graph, None, false)
    {
        crate::ir_evidence::note(crate::ir_evidence::Transform::Unrolled);
        scalar_cleanup(graph);
        run_linear_reassoc(graph);
    }
    // Lock coarsening (round 11 wave 9). After the loop rounds, because LICM's
    // `collapse_trivial_merges` is what puts `synchronized (o) {..}
    // synchronized (o) {..}` on one control node: javac separates the two
    // regions with a `goto` over the first one's handler, and the builder
    // gives its target a single-input merge. A merge removes a release/acquire
    // pair, which can expose a load in the second region to CSE against one in
    // the first, so the clean-up re-runs when one landed.
    //
    // Nested-lock elision (round 11 wave 14) runs first: it only deletes the
    // inner regions of an object, so it hands coarsening a graph whose
    // remaining monitor ops are all outermost, and the caller-held
    // synchronized CALL pairs it never touches are still whole for the
    // coarsening's instance-call chains. Its clean-up is the same one.
    //
    // MON3R17-1 (lane irfeed): the three pair passes only delete (each kills
    // its enter/exit pair on the spot); the loop coarsening after them grows
    // the graph. Their shrink is measured between the two, for the same
    // reason as `simplified_before_growth`.
    let after_loop_rounds = graph.live_count();
    let nested = elide_nested_monitors(graph) > 0;
    // Round 14 wave 4 (lane sync4, SY3-2): a synchronized splice's pair inside
    // a region on the same object, which the pass above skips by node.
    let windows = elide_region_nested_sync_windows(graph) > 0;
    let coarsened = coarsen_adjacent_monitors(graph) > 0;
    let pairs_shrank = graph.live_count() < after_loop_rounds;
    // Round 16 wave 12 (lane syncco, BN2-3 / MON16-8): a counted loop whose
    // body is one synchronized splice's window takes that window once per
    // `LOOP_LOCK_COARSEN_FACTOR` iterations. After the pair passes above, so
    // the window it copies is the one they settled on.
    let looped = coarsen_loop_sync_windows(graph) > 0;
    if nested || windows || coarsened || looped {
        scalar_cleanup(graph);
    }
    // A branch whose condition only became a constant during the loop rounds
    // (a spliced `PlatformDependent.hasUnsafe()` folded once its static is
    // forwarded) keeps both arms through the early prune above; lowering then
    // folds the branch and lays the dead arm out inline in the loop, where its
    // values still take part in the linear scan and evict the loop's own.
    // `CRATONVM_JIT_IR_CONST_IF_PRUNE_LATE=0` turns this second prune off.
    if prune_constant_ifs(graph) > 0 {
        scalar_cleanup(graph);
    }
    // JIT round 25 wave 2 (lane branch-fold): once more, for the booleans and
    // branches the loop rounds and the pair passes exposed.
    split_partial += run_partial_split_rounds(graph);
    let late = run_bool_branch_folds(graph);
    branch_census.add(&late);
    let more = run_partial_split_rounds(graph);
    if more > 0 {
        split_partial += more;
        let round = run_bool_branch_folds(graph);
        branch_census.add(&round);
    }
    note_branch_fold_census(
        &branch_census,
        split_through_memory_phis,
        split_partial,
        despec.map_or("", |(_, key)| key),
    );
    if graph.live_count() < nodes_before {
        crate::ir_evidence::note(crate::ir_evidence::Transform::Simplified);
    }
    // MON3R17-1: the shrink measured before the growing passes, recorded
    // apart from the bit above (always; it changes a verdict only under
    // `CRATONVM_JIT_IR_EVIDENCE_SIMPLIFIED_BEFORE_GROWTH`, see
    // `ir_evidence::is_worth_publishing`).
    if simplified_before_growth || pairs_shrank {
        crate::ir_evidence::note_simplified_before_growth();
    }
    // JIT round 21, lane tcfuse (TCPERF21-3,
    // `CRATONVM_JIT_TYPECHECK_SHARED_CLASS_WORD`): last, on the cleaned graph
    // (two checks of one value name one operand by now, and an unrolled
    // copy's checks share their copy's word), and after the evidence counts:
    // the words it adds must not read as growth to the acceptance gate.
    typecheck_class_word::share_typecheck_class_words(graph);
}

// ── Lock coarsening: `monitorexit(o); monitorenter(o)` back to back ──────
//
// Round 11 wave 9 (irlower proposal W7-4). `synchronized (o) { a } synchronized
// (o) { b }` builds `enter(o) a exit(o) enter(o) b exit(o)`; the inner pair is
// two CASes (or two helper calls) and two lock-stack writes that buy nothing.
// This deletes it, for ANY object — escaping or not — when nothing can run
// between the two, which is the case HotSpot coarsens too.
//
// Reachability, stated plainly: as of wave 9 no javac `synchronized` BLOCK
// reaches the IR tier at all. Its catch-any handler reads the monitor temp, so
// `lib.rs` requires precise exceptional frames and IR admission refuses the
// method (`r11w9-lower-ir-tier-never-compiles-a-javac-synchronized-block`).
// Until the IR tier gets reason-9 rethrow pads, this pass (and escape
// analysis's lock elision) fires only on handler-free monitor bytecode, and is
// pinned by unit tests on builder-shaped graphs.
//
// # Why it is sound under the JMM
//
// The deleted pair is an unlock of `o` immediately followed by a lock of `o`
// by the same thread. Deleting it can only REMOVE executions: every execution
// of the merged program is an execution of the original in which no other
// thread acquired `o` in the (empty) window between the two, which the JMM
// always permits and never requires (JLS 17.4.4 gives no fairness). No access
// moves out of a critical section; the window holds no access at all. And the
// merged region is bounded: both ops sit on one control node, so no back edge
// (and no loop) lies between them — the lock is never held across a loop
// iteration it was released in before.
//
// # What "nothing between" means, and why it is checked the way it is
//
// A monitor is a deopt/exception point, and the interpreter frame a deopt
// rebuilds names the monitors held at its bci. A deopt landing in the window
// would rebuild a frame WITHOUT `o` while the merged compiled frame holds it —
// a lock leaked for the life of the thread. So the window must contain no node
// that can deopt, raise, call, touch memory or be a safepoint:
//
// * `exit` has exactly one user, `enter`, which takes it as its memory token:
//   no load, store, call or guard is chained between them;
// * both sit on the SAME control node, so they are in one block and no join,
//   branch or back-edge poll separates them;
// * every live node whose id lies strictly between them is inert
//   ([`lock_gap_node_is_inert`]). `ir_schedule` places a block's nodes in id
//   order (a node's unplaced inputs first) and keeps impure nodes in that
//   relative order, which is also what makes a guard's position meaningful —
//   so an impure node outside the id window cannot land between the two
//   unless an inert window node pulls it in as an input. That is refused too
//   (a window node reading an id above `enter`).
//
// Frames inside either region are unchanged: they name `o` held, the merged
// compiled frame holds it, and a surviving live monitor op on `o` keeps
// `ir_lower`'s `relock` false for it. The monitor stack after the pair is the
// stack before it (the builder only admits a `monitorexit` of the innermost
// held reference), so nesting depth is unchanged everywhere outside the window.
//
// Not coarsened: one half of a synchronized splice's window with a monitor op
// that is not in a window (round 14 wave 3, SS-4; see `coarsenable_monitor_pair`),
// different reference NODES (even if they are one object at run
// time), a gap with any access (the EA-side `CRATONVM_JIT_LOCK_COARSEN` handles
// confined objects with confined accesses in the gap), a loop's exit/enter pair
// across the back edge, and nested (re-entrant) regions — eliding an inner
// `enter(o)` inside an outer one needs a deopt frame that says "held twice, one
// to re-take", which is [`elide_nested_monitors`]' job (below), not this one's.

/// May `node` sit in the window between a coarsened `monitorexit` and the
/// `monitorenter` after it? Only if it can neither transfer control (deopt,
/// raise, trap) nor touch memory. A φ is read on its merge's incoming edges,
/// not at its position, and a dead node emits nothing.
pub(in crate::ir_optimize) fn lock_gap_node_is_inert(node: &Node) -> bool {
    let op = &node.op;
    if matches!(op, Op::Dead | Op::Phi) {
        return true;
    }
    !memory_shape_of(op).touches_memory() && !may_raise(op)
}

/// The `monitorenter` that `exit` may be merged with, or `None`. See the
/// section comment for each condition.
pub(in crate::ir_optimize) fn coarsenable_monitor_pair(
    graph: &Graph,
    exit: NodeId,
) -> Option<NodeId> {
    let x = graph.node_opt(exit)?;
    if x.op != Op::MonitorExit || x.inputs.len() != 3 {
        return None;
    }
    let ctrl = x.input_opt(0)?;
    let token = x.input_opt(1)?;
    let obj = x.input_opt(2)?;
    if !graph.node_opt(ctrl).is_some_and(|n| n.op.is_control()) {
        return None;
    }
    // The pair is spliced onto `exit`'s own incoming token: it must be live.
    if !graph.node_opt(token).is_some_and(|n| n.op != Op::Dead) {
        return None;
    }
    let users = graph.users_of(exit);
    let [enter] = users.as_slice() else {
        return None;
    };
    let enter = *enter;
    if enter <= exit {
        return None;
    }
    let e = graph.node_opt(enter)?;
    if e.op != Op::MonitorEnter
        || e.inputs.len() != 3
        || e.input_opt(0) != Some(ctrl)
        || e.input_opt(1) != Some(exit)
        || e.input_opt(2) != Some(obj)
    {
        return None;
    }
    if !graph.safepoint_users_of(exit).is_empty() || !graph.safepoint_users_of(enter).is_empty() {
        return None;
    }
    // Round 14 wave 3 (lane sync, proposal SS-4): both halves of a synchronized
    // splice's window (`Graph::sync_splice_windows`) or neither. `ir_lower`
    // re-pairs a window whose exit was merged with the NEXT window's enter
    // (`sync_splice_window_refusal`), but a window op merged with a bytecode
    // monitor op or a caller-held CALL's pair leaves an enter or exit that no
    // check can pair, and the whole compile was refused for it.
    let in_window = |n: NodeId| {
        graph
            .sync_splice_windows
            .iter()
            .any(|w| w.enter == n || w.exit == n)
    };
    if in_window(exit) != in_window(enter) {
        return None;
    }
    // j27 wave 4 (lane MONROWS, SY16-3): likewise both halves of caller-held
    // synchronized CALL pairs or neither (`Graph::sync_call_monitor_ops`).
    if !monitor_rows::sync_call_halves_agree(graph, exit, enter) {
        return None;
    }
    // Round 15 wave 1 (lane sync, SS8-2): never a self-locking body's method
    // monitor op (`Graph::method_self_lock`), which `ir_lower` requires whole.
    if graph
        .method_self_lock
        .as_ref()
        .is_some_and(|lock| lock.names_op(exit) || lock.names_op(enter))
    {
        return None;
    }
    // Everything downstream names `enter` as a memory token and nothing else,
    // or the splice below would turn an ordering edge into a value.
    for u in graph.users_of(enter) {
        let n = graph.node_opt(u)?;
        if n.op == Op::Dead {
            continue;
        }
        for (i, &inp) in n.inputs.iter().enumerate() {
            if inp == enter && !is_memory_token_slot(n, i) {
                return None;
            }
        }
    }
    for id in (exit + 1)..enter {
        let n = graph.node_opt(id)?;
        if !lock_gap_node_is_inert(n) {
            return None;
        }
        if n.op == Op::Dead || matches!(n.op, Op::Phi) || n.op.is_control() {
            continue;
        }
        // An input above `enter` is unplaced when the scheduler reaches this
        // node, so it would be placed here, inside the window.
        if n.inputs.iter().any(|&i| i != NO_NODE && i > enter) {
            return None;
        }
    }
    Some(enter)
}

/// Delete every `monitorexit(o); monitorenter(o)` pair with nothing between
/// the two, splicing the memory chain over the hole. Returns the number of
/// pairs merged. See the section comment above for the soundness argument.
pub(in crate::ir_optimize) fn coarsen_adjacent_monitors(graph: &mut Graph) -> usize {
    let exits: Vec<NodeId> = graph
        .nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| n.op == Op::MonitorExit)
        .map(|(id, _)| id as NodeId)
        .collect();
    let mut merged = 0usize;
    for exit in exits {
        let Some(enter) = coarsenable_monitor_pair(graph, exit) else {
            continue;
        };
        let Some(token) = graph.node_opt(exit).and_then(|n| n.input_opt(1)) else {
            continue;
        };
        graph.replace_all_uses(enter, token);
        graph.kill(enter);
        graph.kill(exit);
        merged += 1;
    }
    if merged > 0 && false {
        eprintln!("[cratonvm-jitc] ir lock coarsening: merged {merged} exit/enter pair(s)");
    }
    merged
}

// ── Loop lock coarsening: one window per `K` iterations ──────────────────
//
// Round 16 wave 12 (lane syncco; proposals BN2-3 of `jit-r16-bignum2-
// proposals-RETIRED-20261006.md` and MON16-8 of `jit-r16-monitor-proposals-RETIRED-20261006.md`; page
// `r12w8-orch-synchronized-instance-calls-and-bigdecimal-are-slow-CLOSED-20261006.md`
// section 1). `for (i = 0; i < n; i++) s += c.step();` with `step` a
// synchronized leaf spliced into the loop builds one synchronized splice
// window per iteration: an inline thin lock and unlock, two `LOCK CMPXCHG`s
// on the mark word, ~9 ns a call on the Linux host. HotSpot measures 5 ns
// (`ns`) and 2 ns (`s`) on `R16SyncSyncM`, and 9-10 ns with
// `-XX:-EliminateLocks` or `-XX:LoopUnrollLimit=0`: C2 unrolls the loop and
// merges the adjacent `unlock(c); lock(c)` of consecutive iterations. The
// adjacent-pair pass above cannot see that pair (it straddles the back edge)
// and the partial unroller clones no store or monitor, so this pass does the
// two steps as one transform, for exactly one shape.
//
// # The shape
//
// A counted loop tested at its header (`i < n` or `i <= n`, `i` an `int`
// counting φ with a positive constant stride, `n` loop-invariant) whose body
// is, in control order: pure arithmetic, at most a null guard on the monitor
// and the monitor's class constant (a static window), then ONE synchronized
// splice window (`Graph::sync_splice_windows`) opening on the body's first
// control node and closing on its back edge, then pure arithmetic. Inside the
// window every node is one `ir::sync_splice_window_node_admitted` admits --
// the window's own contract: nothing there can call, trap or safepoint -- and
// the body has no other memory effect. Branches inside the window are fine
// (a forward diamond); a call, an exception edge, an inner loop, a second
// monitor, a volatile access or a value leaving the body refuses the loop.
//
// # The transform
//
// ```text
// header: phis; if (i < n) {                  header: phis; if (i < n) {
//   guard(c != null)                             if (n - i > (K-1)*s) {      // K left
//   enter(c) W(i) exit(c)              ==>         guard(c != null); enter(c)
//   s' = s + r; i' = i + s                         W(i) W(i+s) .. W(i+(K-1)s)  // K copies
//   goto header                                    exit(c); goto header      // one pair
// }                                              } else { (the original body) goto header }
//                                             }
// ```
//
// The fast path is a straight run of `K` copies of the body (their internal
// diamonds included) under one enter and one exit, entered only when the
// header's own test would pass for all `K` of them; it rejoins the header as
// a third predecessor with each φ's value after the `K`-th copy. The original
// body is untouched and runs the remaining `< K` iterations.
//
// Why `n - i > (K-1)*s` decides "K left" without overflow: the header has
// just proved `i < n` (resp. `<=`), so the true difference lies in
// `(0, 2^32)` (resp. `[0, 2^32)`). The `int` subtraction either equals it or,
// above `Integer.MAX_VALUE`, wraps NEGATIVE, so the signed compare can only
// answer "not enough room" falsely, never "enough" -- the slow path then runs
// those iterations one at a time, exactly as before. When it answers yes,
// `i + j*s` for `j < K` stays below `n <= Integer.MAX_VALUE`, so every copy
// sees exactly the counter value (and every carried φ exactly the value) its
// iteration had. Nothing is computed that the original did not compute.
//
// # Why it is sound under the JMM (JLS 17.4)
//
// The deleted `K - 1` pairs are each an unlock of `c` followed, with nothing
// but register arithmetic between, by a lock of `c` by the same thread -- the
// adjacent-pair argument above: every execution of the coarsened loop is an
// execution of the original in which no other thread took `c` in those empty
// gaps, which the JMM always permits (no fairness, JLS 17.4.4). The skipped
// loop tests are pure; the skipped guards test an invariant that copy 0's
// guard already passed; the skipped class-constant reads return the same
// mirror. No access moves out of a critical section; accesses of later
// copies run under a lock taken earlier (roach motel, always legal). The pass
// still refuses a volatile access in the body: legal or not, it is not the
// shape this was built for, and it keeps the claim to "plain accesses only".
//
// # Hold time, polls, deopt states, exceptions
//
// * The merged window holds the lock for `K` (4) iterations of a body that
//   has no call, no loop and no back edge -- bounded by construction. The
//   back-edge safepoint poll stays on the fast path's back edge, AFTER the
//   exit, so a poll never runs with the coarsened lock held, and a coarsened
//   window is never live across a safepoint.
// * Reentrancy: one enter and one exit per fast iteration group, on the same
//   object, both outside every other monitor op of the body (there is none).
// * Deopt: the fast path's only nodes that can leave it are copy 0's guard,
//   the class constant and the enter -- all at copy 0's own program point,
//   where every carried φ still holds its header value, so the frame the
//   builder recorded for them (the invoke's, at the header's values) is
//   exact. The admitted window nodes keep the window's own contract: their
//   null-receiver exits cannot fire (the receiver was just locked; a static
//   window's class is initialised), and they publish the flat frame
//   (`ir_lower::sync_window_site_publishes_no_chain`). A frame consulted at
//   any of those bcis that names a value of the body (and so would describe
//   the wrong copy) refuses the loop.
// * Exceptions: nothing between the coarsened enter and exit can raise; the
//   enter raises before it holds anything (as copy 0's did); the exit cannot
//   fail on a monitor this thread holds. `ir_lower::sync_splice_window_refusal`
//   checks the fast window on the final schedule like every other window (a
//   refusal rebuilds without the splice: fail closed).
//
// `CRATONVM_JIT_IR_LOOP_LOCK_COARSEN` arms it: default ON since wave 12
// (`R16SyncSyncM` ns 13 -> 5, s 12 -> 4 ns/op; 40/40 loaded runs), `=0` off. `CRATONVM_DBG_JITC=1` names every
// coarsened loop and every refusal of a loop that holds a window.

/// Iterations one coarsened window covers. HotSpot's unrolled `R16SyncSyncM`
/// loops pay ~5 ns against ~9 ns uncoarsened; four iterations per pair puts
/// the pair at a quarter of its cost, and the hold stays a few dozen
/// instructions.
pub(in crate::ir_optimize) const LOOP_LOCK_COARSEN_FACTOR: usize = 4;

/// Budget on the nodes the fast path adds (all copies together).
pub(in crate::ir_optimize) const LOOP_LOCK_COARSEN_MAX_CLONED: usize = 256;

/// Budget on the body's control nodes (a body is a few diamonds at most).
pub(in crate::ir_optimize) const LOOP_LOCK_COARSEN_MAX_CONTROL: usize = 64;

/// One loop [`coarsen_loop_sync_windows`] rewrites, as
/// [`plan_loop_window_coarsening`] found it.
#[derive(Debug)]
pub(in crate::ir_optimize) struct LoopWindowPlan {
    /// The loop header (`Merge`/`Region`, two inputs: entry and back edge).
    pub(in crate::ir_optimize) header: NodeId,
    /// The header test's projection that enters the body.
    pub(in crate::ir_optimize) body_entry: NodeId,
    /// The body's last control node: the header's back-edge input.
    pub(in crate::ir_optimize) back_ctrl: NodeId,
    /// Every φ anchored at the header, ascending.
    pub(in crate::ir_optimize) carried: Vec<NodeId>,
    /// The φ input slot (1 or 2) of the back edge.
    pub(in crate::ir_optimize) back_slot: usize,
    /// The one memory φ of the header.
    pub(in crate::ir_optimize) mem_phi: NodeId,
    /// The counting φ the header test reads.
    pub(in crate::ir_optimize) iv_phi: NodeId,
    /// The loop-invariant bound it is compared with.
    pub(in crate::ir_optimize) bound: NodeId,
    /// The counter's positive constant stride.
    pub(in crate::ir_optimize) stride: i64,
    /// `i < n` (true) or `i <= n` (false).
    pub(in crate::ir_optimize) strict: bool,
    /// The window's enter, at the top of the body.
    pub(in crate::ir_optimize) enter: NodeId,
    /// The window's exit, on the back edge.
    pub(in crate::ir_optimize) exit: NodeId,
    /// The locked object.
    pub(in crate::ir_optimize) monitor: NodeId,
    /// The window's `static_class_id` (0 for an instance window).
    pub(in crate::ir_optimize) static_class_id: u32,
    /// Nodes copy 0 runs once for every copy, ascending: the monitor's null
    /// guard and its class constant.
    pub(in crate::ir_optimize) prefix: Vec<NodeId>,
    /// The nodes each copy clones, ascending: the body's control nodes but
    /// its entry, its pinned nodes but the window pair and the prefix, and
    /// the floating values they and the back edge read.
    pub(in crate::ir_optimize) clone_order: Vec<NodeId>,
}

/// The control node `n` is pinned to: its control input 0, if it has one
/// (a φ's merge, a memory op's or a guard's control). `None` for a floating
/// value and for a control node itself.
pub(in crate::ir_optimize) fn loop_window_anchor(graph: &Graph, n: &Node) -> Option<NodeId> {
    if n.op.is_control() {
        return None;
    }
    let c = n.input_opt(0)?;
    graph
        .node_opt(c)
        .filter(|cn| cn.ty == IrType::Control && cn.op.is_control())
        .map(|_| c)
}

/// The control region test projection `entry` opens, if it is a loop body:
/// every control node forward of `entry` up to (not through) `header`, all of
/// them `If`/`Proj`/`Merge`/`Region`, entered only through `entry` (every
/// merge's inputs inside), acyclic, and leaving only through ONE edge into
/// the header, from a node that is one of `header_inputs`. Returns the region
/// in discovery order (`entry` first) and that back-edge node.
pub(in crate::ir_optimize) fn loop_window_body_region(
    graph: &Graph,
    header: NodeId,
    header_inputs: [NodeId; 2],
    entry: NodeId,
) -> Option<(Vec<NodeId>, NodeId)> {
    let mut seen: FxHashSet<NodeId> = FxHashSet::default();
    seen.insert(entry);
    let mut order = vec![entry];
    let mut work = vec![entry];
    let mut back: Option<NodeId> = None;
    while let Some(c) = work.pop() {
        for u in graph.users_of(c) {
            let Some(un) = graph.node_opt(u) else {
                continue;
            };
            if un.op == Op::Dead || !un.op.is_control() {
                continue;
            }
            if u == header {
                if back.is_some_and(|b| b != c) {
                    return None;
                }
                back = Some(c);
                continue;
            }
            if !matches!(un.op, Op::If | Op::Proj(_) | Op::Merge | Op::Region) {
                return None;
            }
            if seen.insert(u) {
                if order.len() >= LOOP_LOCK_COARSEN_MAX_CONTROL {
                    return None;
                }
                order.push(u);
                work.push(u);
            }
        }
    }
    let back = back?;
    let inside: Vec<NodeId> = header_inputs
        .iter()
        .copied()
        .filter(|i| seen.contains(i))
        .collect();
    if inside != [back] {
        return None;
    }
    // Single entry, and the in-region predecessors of each node for the
    // cycle check.
    let mut preds: FxHashMap<NodeId, Vec<NodeId>> = FxHashMap::default();
    for &c in &order {
        let n = graph.node_opt(c)?;
        let ins: Vec<NodeId> = match n.op {
            Op::Merge | Op::Region => n.inputs.iter().copied().collect(),
            Op::If => vec![n.input_opt(0)?],
            Op::Proj(w) => {
                let src = n.input_opt(0)?;
                let src_is_if = graph.node_opt(src).is_some_and(|s| s.op == Op::If);
                if !src_is_if || w > 1 {
                    return None;
                }
                if c == entry {
                    Vec::new()
                } else {
                    vec![src]
                }
            }
            _ => return None,
        };
        if ins.iter().any(|i| !seen.contains(i)) {
            return None;
        }
        preds.insert(c, ins);
    }
    // Acyclic (Kahn): a back edge inside would be an inner loop.
    let mut done: FxHashSet<NodeId> = FxHashSet::default();
    let mut progressed = true;
    while progressed && done.len() < order.len() {
        progressed = false;
        for &c in &order {
            if done.contains(&c) {
                continue;
            }
            if preds
                .get(&c)
                .is_some_and(|ps| ps.iter().all(|p| done.contains(p)))
            {
                done.insert(c);
                progressed = true;
            }
        }
    }
    if done.len() != order.len() {
        return None;
    }
    Some((order, back))
}

/// For a loop test `a op b` naming the counter on the left (`iv_left`) and
/// entering the body on its true edge (`enter_on_true`): `Some(true)` when
/// the body runs exactly while `i < n`, `Some(false)` while `i <= n`, `None`
/// for every other test (`!=`, a down count, ...).
pub(in crate::ir_optimize) fn loop_window_continue_strict(
    op: CmpOp,
    iv_left: bool,
    enter_on_true: bool,
) -> Option<bool> {
    let op = if enter_on_true { op } else { op.negate() };
    let op = if iv_left { op } else { op.swapped() };
    match op {
        CmpOp::Lt => Some(true),
        CmpOp::Le => Some(false),
        _ => None,
    }
}

/// The plan for the loop at `header`, `Err(None)` when it is not a loop this
/// pass looks at (not a loop, or no monitor op in its body), and
/// `Err(Some(why))` when it holds a window and is refused. See the section
/// comment for every rule.
pub(in crate::ir_optimize) fn plan_loop_window_coarsening(
    graph: &Graph,
    header: NodeId,
) -> Result<LoopWindowPlan, Option<&'static str>> {
    let h = graph.node_opt(header).ok_or(None)?;
    if !matches!(h.op, Op::Merge | Op::Region) || h.inputs.len() != 2 {
        return Err(None);
    }
    let header_inputs = [h.inputs[0], h.inputs[1]];
    if header_inputs.contains(&NO_NODE) || header_inputs[0] == header_inputs[1] {
        return Err(None);
    }
    let node = |id: NodeId| graph.node_opt(id).filter(|n| n.op != Op::Dead);
    // The header's one control user is the loop test.
    let ctrl_users: Vec<NodeId> = graph
        .users_of(header)
        .into_iter()
        .filter(|&u| node(u).is_some_and(|n| n.op.is_control()))
        .collect();
    let [test] = ctrl_users.as_slice() else {
        return Err(None);
    };
    let test = *test;
    let t = node(test).ok_or(None)?;
    if t.op != Op::If || t.input_opt(0) != Some(header) {
        return Err(None);
    }
    let mut projs = [NO_NODE; 2];
    for u in graph.users_of(test) {
        let Some(un) = node(u) else {
            continue;
        };
        match un.op {
            Op::Proj(w) if w <= 1 && projs[w as usize] == NO_NODE => projs[w as usize] = u,
            _ => return Err(None),
        }
    }
    if projs.contains(&NO_NODE) {
        return Err(None);
    }
    let mut found: Option<(NodeId, Vec<NodeId>, NodeId)> = None;
    for &p in &projs {
        if let Some((region, back)) = loop_window_body_region(graph, header, header_inputs, p) {
            if found.is_some() {
                return Err(None);
            }
            found = Some((p, region, back));
        }
    }
    let (body_entry, body_ctrl, back_ctrl) = found.ok_or(None)?;
    let body_set: FxHashSet<NodeId> = body_ctrl.iter().copied().collect();

    // The body's pinned nodes, and anything pinned to the header itself.
    let mut anchored_body: Vec<NodeId> = Vec::new();
    let mut pinned_to_header = false;
    for (id, n) in graph.nodes.iter().enumerate() {
        if n.op == Op::Dead {
            continue;
        }
        let Some(a) = loop_window_anchor(graph, n) else {
            continue;
        };
        if a == header && n.op != Op::Phi {
            pinned_to_header = true;
        }
        if body_set.contains(&a) {
            anchored_body.push(id as NodeId);
        }
    }
    let enters: Vec<NodeId> = anchored_body
        .iter()
        .copied()
        .filter(|&id| node(id).is_some_and(|n| n.op == Op::MonitorEnter))
        .collect();
    let exits: Vec<NodeId> = anchored_body
        .iter()
        .copied()
        .filter(|&id| node(id).is_some_and(|n| n.op == Op::MonitorExit))
        .collect();
    if enters.is_empty() && exits.is_empty() {
        return Err(None);
    }
    // From here the loop holds a monitor op: every refusal is named.
    if !true {
        return Err(Some(
            "CRATONVM_JIT_IR_SYNC_WINDOW_NO_CHAIN_POINTS=0: a window site may publish a chain",
        ));
    }
    if pinned_to_header {
        return Err(Some("a node is pinned to the loop header"));
    }
    let ([enter], [exit]) = (enters.as_slice(), exits.as_slice()) else {
        return Err(Some("the body holds more than one monitor pair"));
    };
    let (enter, exit) = (*enter, *exit);
    let windows = &graph.sync_splice_windows;
    let Some(rec_enter) = windows.iter().find(|w| w.enter == enter) else {
        return Err(Some(
            "the body's monitor enter opens no synchronized splice window",
        ));
    };
    let Some(rec_exit) = windows.iter().find(|w| w.exit == exit) else {
        return Err(Some(
            "the body's monitor exit closes no synchronized splice window",
        ));
    };
    if rec_enter.static_class_id != rec_exit.static_class_id {
        return Err(Some("the enter and the exit belong to different windows"));
    }
    let static_class_id = rec_enter.static_class_id;
    if graph
        .method_self_lock
        .as_ref()
        .is_some_and(|lock| lock.names_op(enter) || lock.names_op(exit))
    {
        return Err(Some("the pair is the method's own monitor"));
    }
    let (e, x) = (node(enter).ok_or(None)?, node(exit).ok_or(None)?);
    if e.inputs.len() != 3 || x.inputs.len() != 3 {
        return Err(Some("a monitor op of an unexpected arity"));
    }
    if e.input_opt(0) != Some(body_entry) {
        return Err(Some("the window does not open at the top of the body"));
    }
    if x.input_opt(0) != Some(back_ctrl) {
        return Err(Some("the window does not close on the back edge"));
    }
    let monitor = e.input_opt(2).ok_or(None)?;
    if x.input_opt(2) != Some(monitor) {
        return Err(Some("the enter and the exit name two objects"));
    }
    let x_tok = x.input_opt(1).ok_or(Some("the exit has no memory input"))?;

    // The header's φs, the memory one, and the back-edge slot.
    let back_slot = 1 + header_inputs
        .iter()
        .position(|&i| i == back_ctrl)
        .ok_or(None)?;
    let mut carried: Vec<NodeId> = graph
        .users_of(header)
        .into_iter()
        .filter(|&u| node(u).is_some_and(|n| n.op == Op::Phi && n.input_opt(0) == Some(header)))
        .collect();
    carried.sort_unstable();
    if carried
        .iter()
        .any(|&p| node(p).is_none_or(|n| n.inputs.len() != 3))
    {
        return Err(Some("a header φ of an unexpected arity"));
    }
    let carried_set: FxHashSet<NodeId> = carried.iter().copied().collect();
    let mems: Vec<NodeId> = carried
        .iter()
        .copied()
        .filter(|&p| node(p).is_some_and(|n| n.ty == IrType::Memory))
        .collect();
    let [mem_phi] = mems.as_slice() else {
        return Err(Some("the header has no single memory φ"));
    };
    let mem_phi = *mem_phi;
    if node(mem_phi).and_then(|n| n.input_opt(back_slot)) != Some(exit) {
        return Err(Some(
            "the window's exit is not the body's last memory effect",
        ));
    }
    if graph
        .users_of(exit)
        .into_iter()
        .any(|u| u != mem_phi && node(u).is_some())
    {
        return Err(Some("something after the window reads the exit's memory"));
    }

    // Between the loop's memory and the enter: only the monitor's class
    // constant (a static window), pinned to the body's entry.
    let mut prefix: Vec<NodeId> = Vec::new();
    let mut tok = e
        .input_opt(1)
        .ok_or(Some("the enter has no memory input"))?;
    while tok != mem_phi {
        let tn = node(tok).ok_or(Some("the enter's memory chain is broken"))?;
        if !matches!(tn.op, Op::ConstClass { .. })
            || tn.input_opt(0) != Some(body_entry)
            || tn.inputs.len() != 2
            || prefix.len() >= 2
        {
            return Err(Some(
                "something other than the monitor's class constant runs before the enter",
            ));
        }
        prefix.push(tok);
        tok = tn
            .input_opt(1)
            .ok_or(Some("the enter's memory chain is broken"))?;
    }
    let chain_head = prefix.last().copied().unwrap_or(enter);
    for u in graph.users_of(mem_phi) {
        let Some(un) = node(u) else {
            continue;
        };
        if u == chain_head || u == mem_phi {
            continue;
        }
        let outside =
            loop_window_anchor(graph, un).is_some_and(|a| a != header && !body_set.contains(&a));
        if !outside {
            return Err(Some("the loop's memory is read elsewhere in the body"));
        }
    }

    // What varies per iteration: the header φs, everything pinned in the
    // body, and every value computed from them (not through another merge's
    // φ, which holds one value for the loop's whole run).
    let mut variant: FxHashSet<NodeId> = carried_set.clone();
    variant.extend(anchored_body.iter().copied());
    let mut vw: Vec<NodeId> = variant.iter().copied().collect();
    while let Some(c) = vw.pop() {
        for u in graph.users_of(c) {
            let Some(un) = node(u) else {
                continue;
            };
            if un.op.is_control() {
                continue;
            }
            if un.op == Op::Phi
                && !carried_set.contains(&u)
                && !un.input_opt(0).is_some_and(|a| body_set.contains(&a))
            {
                continue;
            }
            if variant.insert(u) {
                vw.push(u);
            }
        }
    }
    let invariant = |id: NodeId| !variant.contains(&id);
    if !invariant(monitor) && !prefix.contains(&monitor) {
        return Err(Some("the locked object varies in the loop"));
    }
    // The monitor's null guard: pinned at the body's entry, testing only
    // invariant values, so copy 0's answer is every copy's.
    for &id in &anchored_body {
        let Some(n) = node(id) else {
            continue;
        };
        if matches!(n.op, Op::Guard { .. })
            && n.input_opt(0) == Some(body_entry)
            && n.inputs
                .iter()
                .skip(1)
                .all(|&i| i != NO_NODE && invariant(i))
        {
            prefix.push(id);
        }
    }
    prefix.sort_unstable();

    // The loop test: `i < n` / `i <= n` on an `int` counter.
    let cond = t.input_opt(1).ok_or(None)?;
    let cn = node(cond).ok_or(None)?;
    let Op::Cmp(op) = cn.op else {
        return Err(Some("the loop test is not an int compare"));
    };
    if cn.inputs.len() != 2 {
        return Err(Some("the loop test is not an int compare"));
    }
    let counting = |id: NodeId| -> Option<i64> {
        if !carried_set.contains(&id) {
            return None;
        }
        let phi = node(id)?;
        if phi.ty != IrType::Int {
            return None;
        }
        let next = node(phi.input_opt(back_slot)?)?;
        if next.op != Op::Add || next.ty != IrType::Int || next.inputs.len() != 2 {
            return None;
        }
        let s = if next.inputs[0] == id {
            const_i64(graph, next.inputs[1])?
        } else if next.inputs[1] == id {
            const_i64(graph, next.inputs[0])?
        } else {
            return None;
        };
        (s > 0).then_some(s)
    };
    let (a, b) = (cn.inputs[0], cn.inputs[1]);
    let (iv_phi, bound, stride, iv_left) = match (counting(a), counting(b)) {
        (Some(s), None) => (a, b, s, true),
        (None, Some(s)) => (b, a, s, false),
        _ => return Err(Some("the loop test names no single counting φ")),
    };
    if bound == NO_NODE || !invariant(bound) || node(bound).is_none_or(|n| n.ty != IrType::Int) {
        return Err(Some("the loop bound is not an invariant int"));
    }
    let strict = loop_window_continue_strict(op, iv_left, body_entry == projs[0])
        .ok_or(Some("the loop test is not `i < n` or `i <= n`"))?;
    let span_max = i64::from(i32::MAX) / (LOOP_LOCK_COARSEN_FACTOR as i64 - 1);
    if stride > span_max {
        return Err(Some("the stride is too large to test K iterations ahead"));
    }

    // What each copy clones.
    let skip: FxHashSet<NodeId> = prefix.iter().copied().chain([enter, exit]).collect();
    let mut cloned: FxHashSet<NodeId> = FxHashSet::default();
    let mut pending: Vec<NodeId> = anchored_body
        .iter()
        .copied()
        .chain(body_ctrl.iter().copied())
        .collect();
    for &p in &carried {
        if p != mem_phi {
            pending.extend(node(p).and_then(|n| n.input_opt(back_slot)));
        }
    }
    while let Some(v) = pending.pop() {
        if v == NO_NODE
            || v == body_entry
            || cloned.contains(&v)
            || skip.contains(&v)
            || carried_set.contains(&v)
        {
            continue;
        }
        let Some(n) = node(v) else {
            return Err(Some("a body edge names a dead node"));
        };
        if n.op.is_control() {
            if !body_set.contains(&v) {
                continue;
            }
        } else if invariant(v) {
            continue;
        }
        cloned.insert(v);
        pending.extend(n.inputs.iter().copied());
    }
    let mut clone_order: Vec<NodeId> = cloned.iter().copied().collect();
    clone_order.sort_unstable();
    if clone_order.len() * LOOP_LOCK_COARSEN_FACTOR > LOOP_LOCK_COARSEN_MAX_CLONED {
        return Err(Some("the body is too large to copy"));
    }
    let window = crate::ir::SyncSpliceWindow {
        enter,
        exit,
        monitor,
        static_class_id,
    };
    for &v in &clone_order {
        let n = node(v).ok_or(None)?;
        if n.frame_snapshot.is_some() {
            return Err(Some("a body node names a frame of its own"));
        }
        if n.volatile_access
            || matches!(
                n.op,
                Op::LoadStatic {
                    is_volatile: true,
                    ..
                } | Op::StoreStatic {
                    is_volatile: true,
                    ..
                }
            )
        {
            return Err(Some("the body has a volatile access"));
        }
        if n.op.is_control() || n.op == Op::Phi {
            continue;
        }
        if !crate::ir::sync_splice_window_node_admitted(graph, n, &window) {
            // MON12-2 (JIT round 19 wave 14): name the op, for the census of
            // what the plan would have to admit next.
            note_lock_coarsen_refusing_op("loop", v, &n.op);
            return Err(Some("a body node could call or trap with the monitor held"));
        }
    }
    // Nothing of the body is read outside it but by the header φs' back
    // edge; the enter feeds only the window, the prefix only the pair and
    // the window.
    for &v in &clone_order {
        for u in graph.users_of(v) {
            let Some(un) = node(u) else {
                continue;
            };
            // The back-edge node's one outside user is the header itself.
            if cloned.contains(&u) || u == exit || (u == header && v == back_ctrl) {
                continue;
            }
            let back_edge_only = carried_set.contains(&u)
                && un
                    .inputs
                    .iter()
                    .enumerate()
                    .all(|(slot, &i)| i != v || slot == back_slot);
            if !back_edge_only {
                return Err(Some("a body value is read outside the body"));
            }
        }
    }
    for &v in prefix.iter().chain([enter].iter()) {
        if graph
            .users_of(v)
            .into_iter()
            .any(|u| node(u).is_some() && !cloned.contains(&u) && !skip.contains(&u))
        {
            return Err(Some("the enter or its prefix is read outside the window"));
        }
    }
    if x_tok != enter && !cloned.contains(&x_tok) {
        return Err(Some("the exit's memory does not come from the window"));
    }
    // The frames the fast path can consult must not name a value of the body:
    // copies 1..K would read copy 0's (or the slow path's) node there. Copy
    // 0's guard, class constant, enter and exit may consult any snapshot at
    // their pc or its resume bci; a window site only the flat frame at its
    // resume bci (`ir_lower::sync_window_site_publishes_no_chain`, required
    // on above) -- the sites the window contract says cannot fire.
    let resume = crate::ir::splice_resume_map_this_build();
    let mut any_bcis: FxHashSet<usize> = FxHashSet::default();
    let mut flat_bcis: FxHashSet<usize> = FxHashSet::default();
    for &v in clone_order
        .iter()
        .chain(prefix.iter())
        .chain([enter, exit].iter())
    {
        let n = node(v).ok_or(None)?;
        if n.op.is_control() || n.op == Op::Phi || crate::ir_lower::node_cannot_deopt(graph, n) {
            continue;
        }
        let outside_window = !cloned.contains(&v);
        let Some(pc) = n.bytecode_pc else {
            return Err(Some("a body node that can leave has no bytecode pc"));
        };
        let resumed = resume.as_ref().and_then(|m| m.resume_bci(pc));
        if outside_window {
            if let Op::Guard { bci } = n.op {
                any_bcis.insert(bci);
            }
            any_bcis.insert(pc);
            any_bcis.extend(resumed);
        } else {
            flat_bcis.insert(resumed.unwrap_or(pc));
        }
    }
    let names_body = graph.safepoints.iter().any(|sp| {
        let consulted =
            any_bcis.contains(&sp.bci) || (flat_bcis.contains(&sp.bci) && !sp.is_splice_state());
        consulted
            && !sp.post_call_only
            && sp
                .locals
                .iter()
                .chain(sp.stack.iter())
                .chain(sp.monitors.iter())
                .any(|s| cloned.contains(s))
    });
    if names_body {
        return Err(Some(
            "a frame the fast path can consult names a value of the body",
        ));
    }
    Ok(LoopWindowPlan {
        header,
        body_entry,
        back_ctrl,
        carried,
        back_slot,
        mem_phi,
        iv_phi,
        bound,
        stride,
        strict,
        enter,
        exit,
        monitor,
        static_class_id,
        prefix,
        clone_order,
    })
}

/// A copy of node `n` whose inputs are renamed through `subst`, added at the
/// end of the arena (its volatility kept).
pub(in crate::ir_optimize) fn loop_window_clone(
    graph: &mut Graph,
    n: NodeId,
    subst: &FxHashMap<NodeId, NodeId>,
) -> NodeId {
    let (op, ty, pc, volatile, inputs) = {
        let nd = &graph.nodes[n as usize];
        let inputs: Vec<NodeId> = nd
            .inputs
            .iter()
            .map(|i| subst.get(i).copied().unwrap_or(*i))
            .collect();
        (
            nd.op.clone(),
            nd.ty,
            nd.bytecode_pc,
            nd.volatile_access,
            inputs,
        )
    };
    let id = graph.add(op, ty, inputs, pc);
    if volatile {
        graph.set_volatile_access(id);
    }
    id
}

/// Rewrite the loop `plan` describes (see the section comment). Every check
/// ran in [`plan_loop_window_coarsening`]; nothing here refuses. Returns the
/// fast path's `(enter, exit)`.
pub(in crate::ir_optimize) fn apply_loop_window_coarsening(
    graph: &mut Graph,
    plan: &LoopWindowPlan,
) -> (NodeId, NodeId) {
    let k = LOOP_LOCK_COARSEN_FACTOR;
    let body_users = graph.users_of(plan.body_entry);
    let (e_pc, e_tok) = {
        let e = &graph.nodes[plan.enter as usize];
        (e.bytecode_pc, e.inputs[1])
    };
    let (x_pc, x_tok) = {
        let x = &graph.nodes[plan.exit as usize];
        (x.bytecode_pc, x.inputs[1])
    };

    // The fork: `K` iterations left, or the original body.
    let span = graph.add(
        Op::Const((k as i64 - 1) * plan.stride),
        IrType::Int,
        vec![],
        None,
    );
    let left = graph.add(Op::Sub, IrType::Int, vec![plan.bound, plan.iv_phi], None);
    let cc = if plan.strict { CmpOp::Gt } else { CmpOp::Ge };
    let room = graph.add(Op::Cmp(cc), IrType::Int, vec![left, span], None);
    // No bytecode pc: the header test's profile does not describe this branch,
    // and no OSR entry may land on either arm.
    let fork = graph.add(Op::If, IrType::Control, vec![plan.body_entry, room], None);
    let fast = graph.add(Op::Proj(0), IrType::Control, vec![fork], None);
    let slow = graph.add(Op::Proj(1), IrType::Control, vec![fork], None);

    // Copy 0's prefix, shared by every copy; then the one enter.
    let mut shared: FxHashMap<NodeId, NodeId> = FxHashMap::default();
    shared.insert(plan.body_entry, fast);
    for &n in &plan.prefix {
        let id = loop_window_clone(graph, n, &shared);
        shared.insert(n, id);
    }
    let monitor = shared.get(&plan.monitor).copied().unwrap_or(plan.monitor);
    let tok0 = shared.get(&e_tok).copied().unwrap_or(e_tok);
    let enter = graph.add(
        Op::MonitorEnter,
        IrType::Memory,
        vec![fast, tok0, monitor],
        e_pc,
    );

    // `K` copies of the body, each entered with the previous one's values.
    let mut cur: FxHashMap<NodeId, NodeId> = plan
        .carried
        .iter()
        .copied()
        .filter(|&p| p != plan.mem_phi)
        .map(|p| (p, p))
        .collect();
    let mut start = fast;
    let mut window_tok = enter;
    for _ in 0..k {
        let mut subst = shared.clone();
        subst.insert(plan.body_entry, start);
        subst.insert(plan.enter, window_tok);
        for (&p, &v) in &cur {
            subst.insert(p, v);
        }
        // Two phases: a merge's inputs (and a φ's) may name nodes of this
        // copy that come later in id order.
        let mut made: Vec<(NodeId, NodeId)> = Vec::with_capacity(plan.clone_order.len());
        for &n in &plan.clone_order {
            let (op, ty, pc, volatile, inputs) = {
                let nd = &graph.nodes[n as usize];
                (
                    nd.op.clone(),
                    nd.ty,
                    nd.bytecode_pc,
                    nd.volatile_access,
                    nd.inputs.clone(),
                )
            };
            let id = graph.add_inputs(op, ty, inputs, pc);
            if volatile {
                graph.set_volatile_access(id);
            }
            subst.insert(n, id);
            made.push((n, id));
        }
        for &(n, id) in &made {
            let arity = graph.nodes[n as usize].inputs.len();
            for slot in 0..arity {
                let orig = graph.nodes[n as usize].inputs[slot];
                if let Some(&renamed) = subst.get(&orig) {
                    if renamed != orig {
                        graph.set_input(id, slot, renamed);
                    }
                }
            }
        }
        window_tok = subst.get(&x_tok).copied().unwrap_or(x_tok);
        start = subst.get(&plan.back_ctrl).copied().unwrap_or(start);
        let next: Vec<(NodeId, NodeId)> = cur
            .keys()
            .map(|&p| {
                let back = graph.nodes[p as usize].inputs[plan.back_slot];
                (p, subst.get(&back).copied().unwrap_or(back))
            })
            .collect();
        cur.extend(next);
    }
    let exit = graph.add(
        Op::MonitorExit,
        IrType::Memory,
        vec![start, window_tok, monitor],
        x_pc,
    );
    graph.sync_splice_windows.push(crate::ir::SyncSpliceWindow {
        enter,
        exit,
        monitor,
        static_class_id: plan.static_class_id,
    });

    // The fast path's back edge: a third header predecessor, each φ taking
    // its value after the `K`-th copy (the memory φ the fast exit).
    graph.push_input(plan.header, start);
    for &p in &plan.carried {
        let v = if p == plan.mem_phi {
            exit
        } else {
            cur.get(&p).copied().unwrap_or(p)
        };
        graph.push_input(p, v);
    }

    // The original body hangs off the fork's other arm.
    for u in body_users {
        if graph.node_opt(u).is_none_or(|n| n.op == Op::Dead) {
            continue;
        }
        let arity = graph.nodes[u as usize].inputs.len();
        for slot in 0..arity {
            if graph.nodes[u as usize].inputs[slot] == plan.body_entry {
                graph.set_input(u, slot, slow);
            }
        }
    }
    (enter, exit)
}

/// Coarsen every loop whose body is one synchronized splice window into one
/// window per [`LOOP_LOCK_COARSEN_FACTOR`] iterations. Returns the number of
/// loops rewritten. See the section comment above.
///
/// Round 16 wave 16 (lane monitor4, MON3-1): with
/// `CRATONVM_JIT_IR_LOOP_LOCK_COARSEN_BLOCKS=1` a loop whose body is one
/// bytecode `synchronized (STATIC) { .. }` block is coarsened too
/// ([`plan_loop_block_coarsening`]). Under `CRATONVM_DBG_JITC` such a loop is
/// planned with that switch off as well and named -- coarsenable, or refused
/// and why -- without being rewritten.
pub(in crate::ir_optimize) fn coarsen_loop_sync_windows(graph: &mut Graph) -> usize {
    let dbg = false;
    let blocks = ir_loop_lock_coarsen_blocks_enabled();
    let instance = ir_loop_lock_coarsen_instance_blocks_enabled();
    let windows = !graph.sync_splice_windows.is_empty();
    if !windows && !blocks && !instance && !dbg {
        return 0;
    }
    let headers: Vec<NodeId> = graph
        .nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| matches!(n.op, Op::Merge | Op::Region) && n.inputs.len() == 2)
        .map(|(id, _)| id as NodeId)
        .collect();
    let mut rewritten = 0usize;
    for header in headers {
        if windows {
            match plan_loop_window_coarsening(graph, header) {
                Ok(plan) => {
                    let bci = graph.nodes[plan.enter as usize].bytecode_pc;
                    let per_copy = plan.clone_order.len();
                    let (enter, exit) = apply_loop_window_coarsening(graph, &plan);
                    rewritten += 1;
                    // JIT round 17 wave 2 (lane monitor3): the acceptance
                    // gate's fact; changes no verdict unless
                    // `CRATONVM_JIT_IR_LOOP_LOCK_COARSEN_WINDOW_EVIDENCE` is on.
                    crate::ir_evidence::note_loop_window_coarsened();
                    if dbg {
                        eprintln!(
                            "[cratonvm-jitc] ir loop lock coarsening: window at bci {bci:?} \
                             coarsened x{LOOP_LOCK_COARSEN_FACTOR} (header n{header}, \
                             {per_copy} node(s) a copy, fast pair n{enter}/n{exit})"
                        );
                    }
                    continue;
                }
                Err(Some(why)) => {
                    if dbg {
                        eprintln!(
                            "[cratonvm-jitc] ir loop lock coarsening REFUSED header n{header}: {why}"
                        );
                    }
                }
                Err(None) => {}
            }
        }
        if blocks || instance || dbg {
            rewritten += coarsen_loop_block(graph, header, blocks, instance, dbg);
        }
    }
    rewritten
}

// ── Loop lock coarsening of a bytecode `synchronized` block ─────────────
//
// Round 16 wave 16 (lane monitor4; MON3-1 of `jit-r16-monitor3-proposals-RETIRED-20261006.md`,
// SC-3 of `jit-r16-syncco-proposals-RETIRED-20261006.md`). The window pass above serves a
// synchronized SPLICE, whose pair no frame describes. `R16MonitorCrowd`'s
// `shortCs` is the other shape, a javac block in the compiling method:
//
// ```text
// for (int i = 0; i < n; i++) { synchronized (LOCK) { B.acc += B.counter; B.counter++; } }
// header:  i < n ?
// body:    o = getstatic LOCK ; [guard o != null] ; enter(o)
//          ... loads / stores, each of which can leave with o held ...
//          exit(o) ; i++ ; -> header
// ```
//
// The rewrite forks like the window pass (`n - i > (K-1)*s`, else the
// original body) and gives the fast arm ONE enter and ONE exit around K copies
// of the block:
//
// ```text
// fast:  o0 = getstatic LOCK ; [guard o0] ; enter(o0) ; copy 0
//        o1 = getstatic LOCK ; o1 == o0 ? copy 1 : exit(o0), -> header
//        ...                         (one such test per copy 1..K-1)
//        exit(o0) ; -> header
// ```
//
// Why each piece is sound:
//
// * JMM. Copy j's read of `LOCK` moved from after iteration j-1's release to
//   before it -- into the critical section, the direction lock coarsening may
//   always move an access -- and on the path that continues it read the object
//   copy 0 locked, so the elided `exit(o0); enter(o_j)` is an adjacent pair on
//   one object: removing it removes executions, never adds one (the window
//   pass's argument). A different object leaves through the identity exit:
//   `exit(o0)` with iteration j not yet started, so the header re-runs
//   iteration j from its own values and re-reads `LOCK` (the discarded read is
//   a plain read nothing observed). The hold is bounded: K iterations, no
//   back edge, no safepoint poll inside it.
// * Frames. A node of the block that can leave (a field access's null check, a
//   helper's pending exception, a guard) resumes the interpreter at its own
//   bci, and the snapshot the builder recorded there describes the ORIGINAL
//   iteration. Each copy gets its own copy of every such snapshot, renamed
//   through that copy's substitution (body node -> its clone, header phi ->
//   the value entering the copy, `o` -> `o0`), stamped on the copy's nodes
//   with `Node::frame_snapshot` -- the per-copy frames the partial unroller
//   uses. Every such frame names `o` held exactly once, innermost, and the
//   coarsened code holds `o0` exactly once at every point of every copy, so a
//   deopt or a rethrow pad hands the interpreter (and javac's catch-all
//   handler) exactly the one hold it must release. `ir_lower` publishes the
//   hold `relock == false` (a live monitor op names `o0`). The fast enter and
//   the guards before it take copy 0's frame at their bci (no monitor held);
//   copy j's identity exit and the fast exit take copy j-1's / copy K-1's
//   frame at the block's `monitorexit` (the hold they release).
// * The identity re-read cannot leave: it is required to be the direct read of
//   an initialised class (`Graph::direct_static_fields`, the lowering's own
//   `resolve_static_base` answer), which makes no call and needs no frame.
// * GC: `o0` is an ordinary reference value live across the copies; every
//   call inside them publishes it in its map like any other.
// * Inflation under contention: the hold is the thin or inflated lock the
//   ordinary enter took; a contender inflating it meanwhile makes the fast
//   exit take the inflated path, as any long critical section does.
//
// Refused, each with a reason under `CRATONVM_DBG_JITC`: anything but a
// non-volatile reference `getstatic` as the monitor; a read that may call; a
// node before the enter other than that read and guards on it; a body node
// that calls, allocates or resumes at a bci other than its own; a frame that
// does not hold the monitor exactly once (or holds it before the enter); a
// frame naming a value no copy can rename; a spliced callee's node; and every
// loop-skeleton rule of the window pass.
//
// Round 17 (lane monitor5, MON4-1): two more monitor kinds, under the sibling
// switch `CRATONVM_JIT_IR_LOOP_LOCK_COARSEN_INSTANCE_BLOCKS` ([`BlockMonitor`]):
//
// * `synchronized (this.lock)` -- a plain reference `getfield` of a
//   loop-invariant base at a constant offset. Treated as the static read is:
//   copy 0 reads it (with copy 0's pre-enter frame: its null check is the
//   base's), copies 1..K re-read it and leave through an identity exit when
//   it changed. The re-read cannot leave: its base is the invariant copy 0
//   already dereferenced, and the offset is a constant, so neither the null
//   check nor the unconditional-deopt arm of `Op::Load` can fire; it needs no
//   frame of its own.
// * `synchronized (this)` / `synchronized (local)` -- a loop-invariant value
//   and nothing read between the loop's memory and the enter. No re-read and
//   no identity exit: every copy locks the same object, the window pass's
//   adjacent-pair argument exactly.
//
// The JMM, frame, GC and inflation arguments above are unchanged.

/// What a coarsened block locks, which decides what copies 1..K do before
/// their body ([`plan_loop_block_coarsening`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::ir_optimize) enum BlockMonitor {
    /// A direct `getstatic` (round 16 wave 16): re-read per copy, identity
    /// exit. Under `CRATONVM_JIT_IR_LOOP_LOCK_COARSEN_BLOCKS`.
    Static,
    /// A plain reference `getfield` of a loop-invariant base at a constant
    /// offset: re-read per copy, identity exit. Under
    /// `CRATONVM_JIT_IR_LOOP_LOCK_COARSEN_INSTANCE_BLOCKS`.
    Field,
    /// A loop-invariant value: no re-read, no identity exit. Under
    /// `CRATONVM_JIT_IR_LOOP_LOCK_COARSEN_INSTANCE_BLOCKS`.
    Invariant,
}

impl BlockMonitor {
    /// The census word.
    pub(in crate::ir_optimize) fn name(self) -> &'static str {
        match self {
            BlockMonitor::Static => "static",
            BlockMonitor::Field => "field",
            BlockMonitor::Invariant => "invariant",
        }
    }

    /// Does each later copy re-read the monitor and test its identity?
    pub(in crate::ir_optimize) fn rereads(self) -> bool {
        self != BlockMonitor::Invariant
    }
}

/// `CRATONVM_JIT_IR_LOOP_LOCK_COARSEN_INSTANCE_BLOCKS` (default ON since the
/// round-17 close, `=0` off; A/B in jit-optin-switch-triage-r17-CLOSED-20261006.md;
/// round 17, lane monitor5, MON4-1): [`coarsen_loop_sync_windows`] also coarsens a loop
/// whose body is one bytecode block on a field monitor (`synchronized
/// (this.lock)`) or a loop-invariant one (`synchronized (this)`)
/// ([`BlockMonitor::Field`], [`BlockMonitor::Invariant`]). Independent of
/// `CRATONVM_JIT_IR_LOOP_LOCK_COARSEN_BLOCKS`, which keeps the static kind.
/// Read live, once per compile (no `static`).
pub(in crate::ir_optimize) fn ir_loop_lock_coarsen_instance_blocks_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_LOOP_LOCK_COARSEN_BLOCKS` (default ON since the round-17
/// close, `=0` off; A/B in jit-optin-switch-triage-r17-CLOSED-20261006.md):
/// [`coarsen_loop_sync_windows`] also coarsens a loop whose body is one
/// bytecode `synchronized (STATIC)` block ([`plan_loop_block_coarsening`]). Needs
/// `CRATONVM_JIT_IR_LOOP_LOCK_COARSEN` (default on). Read live, once per
/// compile (no `static`); `lib.rs` feeds `Graph::direct_static_fields` only
/// while it, `CRATONVM_JIT_IR_LOOP_STATIC_HOIST` (nio20 wave 6) or
/// `CRATONVM_DBG_JITC` is on.
pub(crate) fn ir_loop_lock_coarsen_blocks_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_LOOP_LOCK_COARSEN_IN_PLACE=1` (opt-in; JIT round 17 wave
/// 1, lane monitor): a block either of the two switches above coarsens takes
/// the IN-PLACE shape ([`apply_loop_block_coarsening_in_place`]) instead of
/// the forked one ([`apply_loop_block_coarsening`]): no `n - i > 3*s` fork
/// and no second copy of the block. Inert unless a block switch is on. Read
/// live, once per coarsened loop (no `static`).
pub(in crate::ir_optimize) fn ir_loop_lock_coarsen_in_place_enabled() -> bool {
    false
}

/// Is a `getstatic` of `(class_id, field_index)` the direct read -- no call,
/// no `<clinit>`, no frame -- in this compile? The lowering's own answer
/// (`ir_lower::emit_inline_getstatic`): the inline route on, and the compile's
/// static-base resolver accepting the field.
pub(in crate::ir_optimize) fn static_read_is_direct(
    graph: &Graph,
    class_id: u32,
    field_index: u32,
) -> bool {
    crate::x64::inline_getstatic_enabled()
        && graph
            .direct_static_fields
            .contains(&(class_id, field_index))
}

/// `CRATONVM_JIT_IR_LOOP_LOCK_COARSEN_STATIC_STORES` (JIT round 19 wave 12,
/// lane mon12, MON11-1 of `docs/feature-designs/jit-ir-optimizations.md`;
/// default ON since the wave 12 integration, `=0` kill; method-sync-loop
/// 10.7 -> 3 ns, contended-sync-loop 61-110 -> 21-28 ns in 7/7 runs): the block
/// coarsening also admits a body `putstatic` the
/// lowering stores directly ([`static_store_is_direct`]). Without it javac's
/// `for (..) { synchronized (LOCK) { count++; } }` -- `R19Mon10Rows`
/// `method-sync-loop`, 12.5 ns against HotSpot's 2.7 -- is refused ("a body
/// node could call, ...") and pays two CAS per iteration on the optimizing
/// tier too. Read live, once per planned loop (no `static`).
pub(in crate::ir_optimize) fn ir_loop_lock_coarsen_static_stores_enabled() -> bool {
    true
}

/// Is a body `putstatic` of `(class_id, field_index)` with `type_tag` the
/// direct store -- no call, no `<clinit>`, no frame -- in this compile, under
/// [`ir_loop_lock_coarsen_static_stores_enabled`]? The lowering's own rule
/// (`ir_lower::Lowerer::emit_inline_putstatic`): `CRATONVM_JIT_INLINE_PUTSTATIC`
/// on, a primitive type, and the compile's static-base resolver accepting the
/// field (`Graph::direct_static_fields`, fed from that resolver for every
/// static site). A reference store (barriers) stays refused. The node is still
/// a `leaving` one, so each copy stamps its own frame on it: should the
/// lowering fall back to the helper after all (a field index past the cell
/// table), the call resumes at the copy's own frame, which holds the monitor
/// exactly once.
pub(in crate::ir_optimize) fn static_store_is_direct(
    graph: &Graph,
    class_id: u32,
    field_index: u32,
    type_tag: u8,
) -> bool {
    ir_loop_lock_coarsen_static_stores_enabled()
        && crate::ir_lower::inline_putstatic_enabled()
        && matches!(
            type_tag,
            b'I' | b'Z' | b'B' | b'C' | b'S' | b'F' | b'J' | b'D'
        )
        && graph
            .direct_static_fields
            .contains(&(class_id, field_index))
}

/// One loop [`apply_loop_block_coarsening`] rewrites, as
/// [`plan_loop_block_coarsening`] found it.
#[derive(Debug)]
pub(in crate::ir_optimize) struct LoopBlockPlan {
    /// The loop header (`Merge`/`Region`, two inputs: entry and back edge).
    pub(in crate::ir_optimize) header: NodeId,
    /// The header test's projection that enters the body.
    pub(in crate::ir_optimize) body_entry: NodeId,
    /// The body's last control node: the header's back-edge input.
    pub(in crate::ir_optimize) back_ctrl: NodeId,
    /// Every φ anchored at the header, ascending.
    pub(in crate::ir_optimize) carried: Vec<NodeId>,
    /// The φ input slot (1 or 2) of the back edge.
    pub(in crate::ir_optimize) back_slot: usize,
    /// The one memory φ of the header.
    pub(in crate::ir_optimize) mem_phi: NodeId,
    /// The counting φ the header test reads.
    pub(in crate::ir_optimize) iv_phi: NodeId,
    /// The loop-invariant bound it is compared with.
    pub(in crate::ir_optimize) bound: NodeId,
    /// The counter's positive constant stride.
    pub(in crate::ir_optimize) stride: i64,
    /// `i < n` (true) or `i <= n` (false).
    pub(in crate::ir_optimize) strict: bool,
    /// The header test's condition (`Cmp` of `iv_phi` and `bound`), which the
    /// in-place shape re-asks per later copy
    /// ([`apply_loop_block_coarsening_in_place`]).
    pub(in crate::ir_optimize) cond: NodeId,
    /// Which projection of the header test enters the body (`body_entry` is
    /// `Proj(body_proj)`).
    pub(in crate::ir_optimize) body_proj: u8,
    /// The block's monitor. [`BlockMonitor::Static`] / [`BlockMonitor::Field`]:
    /// the read at the top of the body, which is both the locked object and
    /// the enter's memory input. [`BlockMonitor::Invariant`]: the locked
    /// value itself, defined outside the loop.
    pub(in crate::ir_optimize) read: NodeId,
    /// What `read` is.
    pub(in crate::ir_optimize) monitor: BlockMonitor,
    /// Round 17 wave 2 (lane monitor2, MON3-3): a [`BlockMonitor::Static`]
    /// read of a trusted `static final` field
    /// ([`static_final_monitor_is_trusted`]): copies 1..K neither re-read it
    /// nor test its identity ([`LoopBlockPlan::rereads_per_copy`]).
    pub(in crate::ir_optimize) trusted_final: bool,
    /// The block's `monitorenter`, right after the read.
    pub(in crate::ir_optimize) enter: NodeId,
    /// The block's `monitorexit`, on the back edge.
    pub(in crate::ir_optimize) exit: NodeId,
    /// Guards copy 0 runs between the read and the enter, testing only the
    /// read and invariants, ascending.
    pub(in crate::ir_optimize) pre: Vec<NodeId>,
    /// The floating values those guards test (computed from the read alone).
    pub(in crate::ir_optimize) pre_float: Vec<NodeId>,
    /// The nodes each copy clones, ascending (the window pass's rule, with
    /// the read, the pre-enter nodes and the pair skipped).
    pub(in crate::ir_optimize) clone_order: Vec<NodeId>,
    /// The cloned nodes that can leave the compiled code, ascending: each copy
    /// stamps its clone of them with a per-copy frame.
    pub(in crate::ir_optimize) leaving: Vec<NodeId>,
    /// Round 17 wave 2 (lane monitor2): the other cloned nodes with a pc,
    /// ascending. Each copy stamps its clone of one with the copy's frame when
    /// `frames` has one for its pc, so no node of a copy is found by bci.
    pub(in crate::ir_optimize) quiet: Vec<NodeId>,
    /// bci -> the first non-chain snapshot there, for every bci a per-copy
    /// frame is made at (the leaving nodes', the pre-enter guards', the
    /// enter's and the exit's; since round 17 wave 2 also every other cloned
    /// pc inside the block that has one).
    pub(in crate::ir_optimize) frames: FxHashMap<usize, usize>,
    /// Coarsening factor (`4`, or up to `16` for tiny single-monitor bodies).
    pub(in crate::ir_optimize) factor: usize,
}

impl LoopBlockPlan {
    /// Does each later copy re-read the monitor and test its identity? The
    /// monitor kind's answer ([`BlockMonitor::rereads`]), except for a trusted
    /// `static final` one, which is the same object in every copy.
    pub(in crate::ir_optimize) fn rereads_per_copy(&self) -> bool {
        self.monitor.rereads() && !self.trusted_final
    }
}

/// What [`apply_loop_block_coarsening`] built, for the census line.
#[derive(Debug, Clone, Copy)]
pub(in crate::ir_optimize) struct LoopBlockApplied {
    pub(in crate::ir_optimize) enter: NodeId,
    pub(in crate::ir_optimize) exit: NodeId,
    /// Per-copy snapshots appended.
    pub(in crate::ir_optimize) frames: usize,
    /// Identity exits (copies 1..K).
    pub(in crate::ir_optimize) bails: usize,
}

/// Plan, and with `apply` rewrite, the bytecode-block loop at `header`;
/// `CRATONVM_DBG_JITC` (`dbg`) names the outcome. Returns 1 when rewritten.
pub(in crate::ir_optimize) fn coarsen_loop_block(
    graph: &mut Graph,
    header: NodeId,
    blocks: bool,
    instance: bool,
    dbg: bool,
) -> usize {
    match plan_loop_block_coarsening(graph, header) {
        Ok(plan) => {
            let bci = graph.nodes[plan.enter as usize].bytecode_pc;
            let per_copy = plan.clone_order.len();
            // Round 17 wave 2 (lane irfeed): name a trusted `static final`
            // monitor, so the feed's engagement shows on the census line.
            let kind = if plan.trusted_final {
                "trusted static final"
            } else {
                plan.monitor.name()
            };
            let (apply, switch) = match plan.monitor {
                BlockMonitor::Static => (blocks, "CRATONVM_JIT_IR_LOOP_LOCK_COARSEN_BLOCKS"),
                BlockMonitor::Field | BlockMonitor::Invariant => (
                    instance,
                    "CRATONVM_JIT_IR_LOOP_LOCK_COARSEN_INSTANCE_BLOCKS",
                ),
            };
            if !apply {
                if dbg {
                    eprintln!(
                        "[cratonvm-jitc] ir loop lock coarsening: synchronized block ({kind} \
                         monitor) at bci {bci:?} coarsenable (header n{header}, {per_copy} \
                         node(s) a copy); {switch} is off"
                    );
                }
                return 0;
            }
            // JIT round 17 wave 2 (lane monitor3): the acceptance gate's fact
            // (`ir_evidence::note_loop_block_coarsened`). Both shapes below
            // always rewrite once planned; reached only with a block switch on.
            crate::ir_evidence::note_loop_block_coarsened();
            if ir_loop_lock_coarsen_in_place_enabled() && plan.factor == LOOP_LOCK_COARSEN_FACTOR {
                let done = apply_loop_block_coarsening_in_place(graph, &plan);
                if dbg {
                    eprintln!(
                        "[cratonvm-jitc] ir loop lock coarsening: synchronized block ({kind} \
                         monitor) at bci {bci:?} coarsened x{} in place \
                         (header n{header}, {per_copy} node(s) a copy, {} per-copy frame(s), {} \
                         early exit(s), pair n{}/n{})",
                        plan.factor, done.frames, done.bails, done.enter, done.exit
                    );
                }
                return 1;
            }
            let done = apply_loop_block_coarsening(graph, &plan);
            if dbg {
                eprintln!(
                    "[cratonvm-jitc] ir loop lock coarsening: synchronized block ({kind} monitor) \
                     at bci {bci:?} coarsened x{} (header n{header}, \
                     {per_copy} node(s) a copy, {} per-copy frame(s), {} identity exit(s), fast \
                     pair n{}/n{})",
                    plan.factor, done.frames, done.bails, done.enter, done.exit
                );
            }
            1
        }
        Err(Some(why)) => {
            if dbg {
                eprintln!(
                    "[cratonvm-jitc] ir loop lock coarsening REFUSED block header n{header}: {why}"
                );
            }
            0
        }
        Err(None) => 0,
    }
}

/// The plan for the bytecode-block loop at `header`: `Err(None)` when it is
/// not a loop this pass looks at (not a loop, no monitor op, or a synchronized
/// splice's window, which the window pass owns), `Err(Some(why))` when it
/// holds a block and is refused. See the section comment for every rule.
pub(in crate::ir_optimize) fn plan_loop_block_coarsening(
    graph: &Graph,
    header: NodeId,
) -> Result<LoopBlockPlan, Option<&'static str>> {
    let h = graph.node_opt(header).ok_or(None)?;
    if !matches!(h.op, Op::Merge | Op::Region) || h.inputs.len() != 2 {
        return Err(None);
    }
    let header_inputs = [h.inputs[0], h.inputs[1]];
    if header_inputs.contains(&NO_NODE) || header_inputs[0] == header_inputs[1] {
        return Err(None);
    }
    let node = |id: NodeId| graph.node_opt(id).filter(|n| n.op != Op::Dead);
    // The loop skeleton: the window pass's rules.
    let ctrl_users: Vec<NodeId> = graph
        .users_of(header)
        .into_iter()
        .filter(|&u| node(u).is_some_and(|n| n.op.is_control()))
        .collect();
    let [test] = ctrl_users.as_slice() else {
        return Err(None);
    };
    let test = *test;
    let t = node(test).ok_or(None)?;
    if t.op != Op::If || t.input_opt(0) != Some(header) {
        return Err(None);
    }
    let mut projs = [NO_NODE; 2];
    for u in graph.users_of(test) {
        let Some(un) = node(u) else {
            continue;
        };
        match un.op {
            Op::Proj(w) if w <= 1 && projs[w as usize] == NO_NODE => projs[w as usize] = u,
            _ => return Err(None),
        }
    }
    if projs.contains(&NO_NODE) {
        return Err(None);
    }
    let mut found: Option<(NodeId, Vec<NodeId>, NodeId)> = None;
    for &p in &projs {
        if let Some((region, back)) = loop_window_body_region(graph, header, header_inputs, p) {
            if found.is_some() {
                return Err(None);
            }
            found = Some((p, region, back));
        }
    }
    let (body_entry, body_ctrl, back_ctrl) = found.ok_or(None)?;
    let body_set: FxHashSet<NodeId> = body_ctrl.iter().copied().collect();
    let mut anchored_body: Vec<NodeId> = Vec::new();
    let mut pinned_to_header = false;
    for (id, n) in graph.nodes.iter().enumerate() {
        if n.op == Op::Dead {
            continue;
        }
        let Some(a) = loop_window_anchor(graph, n) else {
            continue;
        };
        if a == header && n.op != Op::Phi {
            pinned_to_header = true;
        }
        if body_set.contains(&a) {
            anchored_body.push(id as NodeId);
        }
    }
    let enters: Vec<NodeId> = anchored_body
        .iter()
        .copied()
        .filter(|&id| node(id).is_some_and(|n| n.op == Op::MonitorEnter))
        .collect();
    let exits: Vec<NodeId> = anchored_body
        .iter()
        .copied()
        .filter(|&id| node(id).is_some_and(|n| n.op == Op::MonitorExit))
        .collect();
    if enters.is_empty() && exits.is_empty() {
        return Err(None);
    }
    let ([enter], [exit]) = (enters.as_slice(), exits.as_slice()) else {
        return Err(Some("the body holds more than one monitor pair"));
    };
    let (enter, exit) = (*enter, *exit);
    // A synchronized splice's window is the window pass's.
    if graph
        .sync_splice_windows
        .iter()
        .any(|w| w.enter == enter || w.exit == exit)
    {
        return Err(None);
    }
    if pinned_to_header {
        return Err(Some("a node is pinned to the loop header"));
    }
    if graph
        .method_self_lock
        .as_ref()
        .is_some_and(|lock| lock.names_op(enter) || lock.names_op(exit))
    {
        return Err(Some("the pair is the method's own monitor"));
    }
    let (e, x) = (node(enter).ok_or(None)?, node(exit).ok_or(None)?);
    if e.inputs.len() != 3 || x.inputs.len() != 3 {
        return Err(Some("a monitor op of an unexpected arity"));
    }
    if e.input_opt(0) != Some(body_entry) {
        return Err(Some("the block does not open at the top of the body"));
    }
    if x.input_opt(0) != Some(back_ctrl) {
        return Err(Some("the block does not close on the back edge"));
    }
    let read = e.input_opt(2).ok_or(None)?;
    if x.input_opt(2) != Some(read) {
        return Err(Some("the enter and the exit name two objects"));
    }
    let e_tok = e
        .input_opt(1)
        .ok_or(Some("the enter has no memory input"))?;
    let x_tok = x.input_opt(1).ok_or(Some("the exit has no memory input"))?;
    let x_pc = x.bytecode_pc.ok_or(Some("the exit has no bytecode pc"))?;

    // The header's φs, the memory one, and the back-edge slot.
    let back_slot = 1 + header_inputs
        .iter()
        .position(|&i| i == back_ctrl)
        .ok_or(None)?;
    let mut carried: Vec<NodeId> = graph
        .users_of(header)
        .into_iter()
        .filter(|&u| node(u).is_some_and(|n| n.op == Op::Phi && n.input_opt(0) == Some(header)))
        .collect();
    carried.sort_unstable();
    if carried
        .iter()
        .any(|&p| node(p).is_none_or(|n| n.inputs.len() != 3))
    {
        return Err(Some("a header φ of an unexpected arity"));
    }
    let carried_set: FxHashSet<NodeId> = carried.iter().copied().collect();
    let mems: Vec<NodeId> = carried
        .iter()
        .copied()
        .filter(|&p| node(p).is_some_and(|n| n.ty == IrType::Memory))
        .collect();
    let [mem_phi] = mems.as_slice() else {
        return Err(Some("the header has no single memory φ"));
    };
    let mem_phi = *mem_phi;
    // What the block locks (round 17, MON4-1): nothing read between the
    // loop's memory and the enter (an invariant value, checked below), or one
    // read the enter follows.
    let monitor = if e_tok == mem_phi {
        match node(read).map(|r| (&r.op, r)) {
            Some((
                Op::LoadStatic {
                    class_id,
                    field_index,
                    type_tag,
                    is_volatile,
                },
                r,
            )) if !*is_volatile
                && !r.volatile_access
                && r.ty == IrType::Ref
                && matches!(*type_tag, b'L' | b'[')
                && r.inputs.len() == 2
                && r.input_opt(0) == Some(body_entry)
                && r.input_opt(1) == Some(mem_phi)
                && static_read_is_direct(graph, *class_id, *field_index)
                && static_final_monitor_is_trusted(graph, *class_id, *field_index) =>
            {
                BlockMonitor::Static
            }
            _ => BlockMonitor::Invariant,
        }
    } else if e_tok == read {
        let r = node(read).ok_or(None)?;
        let kind = match r.op {
            Op::LoadStatic {
                class_id,
                field_index,
                type_tag,
                is_volatile,
            } => {
                if is_volatile || r.volatile_access {
                    return Err(Some("the monitor's static is volatile"));
                }
                if r.ty != IrType::Ref || !matches!(type_tag, b'L' | b'[') {
                    return Err(Some("the monitor's static is not a reference"));
                }
                if r.inputs.len() != 2 || r.input_opt(0) != Some(body_entry) {
                    return Err(Some("the monitor's read is not at the top of the body"));
                }
                if !static_read_is_direct(graph, class_id, field_index) {
                    return Err(Some(
                        "the monitor's getstatic may call (no direct read of an initialised class)",
                    ));
                }
                BlockMonitor::Static
            }
            Op::Load(MemKind::Ref) => {
                if r.volatile_access {
                    return Err(Some("the monitor's field is volatile"));
                }
                if r.ty != IrType::Ref || r.inputs.len() != 4 {
                    return Err(Some("the monitor's getfield has an unexpected shape"));
                }
                if r.input_opt(0) != Some(body_entry) {
                    return Err(Some("the monitor's read is not at the top of the body"));
                }
                if r.input_opt(3)
                    .and_then(node)
                    .is_none_or(|o| !matches!(o.op, Op::Const(_)))
                {
                    return Err(Some("the monitor's getfield offset is not a constant"));
                }
                BlockMonitor::Field
            }
            _ => return Err(Some(
                "the locked object is neither a getstatic, a getfield nor a loop-invariant value",
            )),
        };
        if r.input_opt(1) != Some(mem_phi) {
            return Err(Some(
                "the monitor's read does not start the iteration's memory",
            ));
        }
        kind
    } else {
        return Err(Some(
            "the enter's memory input is not the read of its monitor",
        ));
    };
    let chain_head = if monitor.rereads() { read } else { enter };
    if node(mem_phi).and_then(|n| n.input_opt(back_slot)) != Some(exit) {
        return Err(Some(
            "the block's exit is not the body's last memory effect",
        ));
    }
    if graph
        .users_of(exit)
        .into_iter()
        .any(|u| u != mem_phi && node(u).is_some())
    {
        return Err(Some("something after the block reads the exit's memory"));
    }
    for u in graph.users_of(mem_phi) {
        let Some(un) = node(u) else {
            continue;
        };
        if u == chain_head || u == enter || u == mem_phi {
            continue;
        }
        let outside =
            loop_window_anchor(graph, un).is_some_and(|a| a != header && !body_set.contains(&a));
        if !outside {
            return Err(Some("the loop's memory is read elsewhere in the body"));
        }
    }

    // What varies per iteration (the window pass's closure).
    let mut variant: FxHashSet<NodeId> = carried_set.clone();
    variant.extend(anchored_body.iter().copied());
    let mut vw: Vec<NodeId> = variant.iter().copied().collect();
    while let Some(c) = vw.pop() {
        for u in graph.users_of(c) {
            let Some(un) = node(u) else {
                continue;
            };
            if un.op.is_control() {
                continue;
            }
            if un.op == Op::Phi
                && !carried_set.contains(&u)
                && !un.input_opt(0).is_some_and(|a| body_set.contains(&a))
            {
                continue;
            }
            if variant.insert(u) {
                vw.push(u);
            }
        }
    }
    let invariant = |id: NodeId| !variant.contains(&id);
    match monitor {
        BlockMonitor::Invariant if !invariant(read) => {
            return Err(Some("the locked object varies in the loop"));
        }
        BlockMonitor::Field
            if node(read)
                .and_then(|r| r.input_opt(2))
                .is_none_or(|base| base == NO_NODE || !invariant(base)) =>
        {
            return Err(Some("the monitor's getfield base varies in the loop"));
        }
        // Round 17 wave 2 (lane monitor2;
        // `r17-monitor-field-reread-would-deopt-with-the-pre-enter-frame`): the
        // re-read of copies 1..K has no honest frame (the code holds copy 0's
        // object, every frame at the read's bci holds nothing), so it must not
        // be able to leave at all. Its null check is the only exit it could
        // have, and the lowering drops it in every block for the receiver
        // parameter alone (`ir_lower`'s per-block `this` seed). Any other base
        // would get a null check whose deopt resumes before the enter with the
        // hold recorded nowhere.
        // j27 wave 4 (lane MONROWS, MON17W2-2): or, under the opt-in switch,
        // any invariant base: the rewrite records copy 0's read as its proof
        // and the lowering seeds it below that read (`monitor_rows`).
        BlockMonitor::Field
            if !node(read)
                .and_then(|r| r.input_opt(2))
                .is_some_and(|base| field_monitor_base_is_seeded_receiver(graph, base))
                && !monitor_rows::ir_loop_lock_coarsen_dominated_field_base_enabled() =>
        {
            return Err(Some(
                "the monitor's getfield base is not the receiver (its re-read would need a null check)",
            ));
        }
        _ => {}
    }
    // Round 17 wave 2 (lane monitor2, MON3-3 / MON4-3 / MON5-4): a `static
    // final` monitor of an initialised class is one object for the whole loop,
    // so copies 1..K neither re-read it nor test its identity.
    let trusted_final = match node(read).map(|r| &r.op) {
        Some(Op::LoadStatic {
            class_id,
            field_index,
            ..
        }) if monitor == BlockMonitor::Static => {
            static_final_monitor_is_trusted(graph, *class_id, *field_index)
        }
        _ => false,
    };
    // A value of the read and invariants alone (a null compare): the same in
    // every copy whose re-read found copy 0's object.
    let of_read_alone = |v: NodeId| -> bool {
        let mut work = vec![v];
        let mut seen: FxHashSet<NodeId> = FxHashSet::default();
        while let Some(c) = work.pop() {
            if c == NO_NODE {
                return false;
            }
            if c == read || invariant(c) || !seen.insert(c) {
                continue;
            }
            if seen.len() > 16 {
                return false;
            }
            let Some(cn) = node(c) else {
                return false;
            };
            if cn.op.is_control()
                || cn.op == Op::Phi
                || cn.ty == IrType::Memory
                || loop_window_anchor(graph, cn).is_some()
                || !crate::ir_lower::op_cannot_deopt(&cn.op)
            {
                return false;
            }
            work.extend(cn.inputs.iter().copied());
        }
        true
    };
    // Between the read and the enter: guards on the read, plus any one-shot
    // pre-enter virtual-thread pinning guard (`VthreadBoundaryTest` + `Guard`
    // anchored at `body_entry` before `enter`). Copy 0 runs these once before
    // `enter`; copies 1..K stay on the same carrier thread throughout the window.
    let is_vthread_test = |tid: NodeId| -> bool {
        tid < enter
            && node(tid).is_some_and(|tn| {
                matches!(tn.op, Op::VthreadBoundaryTest { .. })
                    && tn.inputs.as_slice() == [body_entry]
                    && graph.users_of(tid).into_iter().all(|u| {
                        u < enter
                            && node(u).is_some_and(|un| {
                                matches!(un.op, Op::Guard { .. })
                                    && un.inputs.as_slice() == [body_entry, tid]
                            })
                    })
            })
    };
    let mut pre: Vec<NodeId> = Vec::new();
    for &id in &anchored_body {
        if id == read || id == enter || id == exit {
            continue;
        }
        let Some(n) = node(id) else {
            continue;
        };
        if loop_window_anchor(graph, n) != Some(body_entry) {
            continue;
        }
        if is_vthread_test(id) {
            continue;
        }
        let guard_on_vthread = id < enter
            && matches!(n.op, Op::Guard { .. })
            && n.inputs.len() == 2
            && n.inputs[0] == body_entry
            && is_vthread_test(n.inputs[1]);
        let guard_on_read = matches!(n.op, Op::Guard { .. })
            && n.inputs.len() >= 2
            && n.inputs.iter().skip(1).all(|&i| of_read_alone(i));
        if guard_on_read || guard_on_vthread {
            pre.push(id);
        } else if id < enter {
            return Err(Some(
                "something other than the monitor's read and guards on it runs before the enter",
            ));
        }
    }
    pre.sort_unstable();
    let mut pre_float: Vec<NodeId> = Vec::new();
    let mut pw: Vec<NodeId> = pre
        .iter()
        .filter_map(|&g| node(g))
        .flat_map(|n| n.inputs.iter().skip(1).copied().collect::<Vec<NodeId>>())
        .collect();
    while let Some(c) = pw.pop() {
        if c == NO_NODE || c == read || c == body_entry || invariant(c) || pre_float.contains(&c) {
            continue;
        }
        pre_float.push(c);
        if let Some(cn) = node(c) {
            if matches!(cn.op, Op::VthreadBoundaryTest { .. }) {
                continue;
            }
            pw.extend(cn.inputs.iter().copied());
        }
    }
    pre_float.sort_unstable();

    // The loop test: `i < n` / `i <= n` on an `int` counter.
    let cond = t.input_opt(1).ok_or(None)?;
    let cn = node(cond).ok_or(None)?;
    let Op::Cmp(op) = cn.op else {
        return Err(Some("the loop test is not an int compare"));
    };
    if cn.inputs.len() != 2 {
        return Err(Some("the loop test is not an int compare"));
    }
    let counting = |id: NodeId| -> Option<i64> {
        if !carried_set.contains(&id) {
            return None;
        }
        let phi = node(id)?;
        if phi.ty != IrType::Int {
            return None;
        }
        let next = node(phi.input_opt(back_slot)?)?;
        if next.op != Op::Add || next.ty != IrType::Int || next.inputs.len() != 2 {
            return None;
        }
        let s = if next.inputs[0] == id {
            const_i64(graph, next.inputs[1])?
        } else if next.inputs[1] == id {
            const_i64(graph, next.inputs[0])?
        } else {
            return None;
        };
        (s > 0).then_some(s)
    };
    let (a, b) = (cn.inputs[0], cn.inputs[1]);
    let (iv_phi, bound, stride, iv_left) = match (counting(a), counting(b)) {
        (Some(s), None) => (a, b, s, true),
        (None, Some(s)) => (b, a, s, false),
        _ => return Err(Some("the loop test names no single counting φ")),
    };
    if bound == NO_NODE || !invariant(bound) || node(bound).is_none_or(|n| n.ty != IrType::Int) {
        return Err(Some("the loop bound is not an invariant int"));
    }
    let strict = loop_window_continue_strict(op, iv_left, body_entry == projs[0])
        .ok_or(Some("the loop test is not `i < n` or `i <= n`"))?;
    let span_max = i64::from(i32::MAX) / (LOOP_LOCK_COARSEN_FACTOR as i64 - 1);
    if stride > span_max {
        return Err(Some("the stride is too large to test K iterations ahead"));
    }

    // What each copy clones.
    let mut skip: FxHashSet<NodeId> = pre.iter().chain(pre_float.iter()).copied().collect();
    skip.extend([read, enter, exit]);
    let mut cloned: FxHashSet<NodeId> = FxHashSet::default();
    let mut pending: Vec<NodeId> = anchored_body
        .iter()
        .copied()
        .chain(body_ctrl.iter().copied())
        .collect();
    for &p in &carried {
        if p != mem_phi {
            pending.extend(node(p).and_then(|n| n.input_opt(back_slot)));
        }
    }
    while let Some(v) = pending.pop() {
        if v == NO_NODE
            || v == body_entry
            || cloned.contains(&v)
            || skip.contains(&v)
            || carried_set.contains(&v)
        {
            continue;
        }
        let Some(n) = node(v) else {
            return Err(Some("a body edge names a dead node"));
        };
        if n.op.is_control() {
            if !body_set.contains(&v) {
                continue;
            }
        } else if invariant(v) {
            continue;
        }
        cloned.insert(v);
        pending.extend(n.inputs.iter().copied());
    }
    let mut clone_order: Vec<NodeId> = cloned.iter().copied().collect();
    clone_order.sort_unstable();
    if clone_order.len() * LOOP_LOCK_COARSEN_FACTOR > LOOP_LOCK_COARSEN_MAX_CLONED {
        return Err(Some("the body is too large to copy"));
    }
    let mut leaving: Vec<NodeId> = Vec::new();
    // Round 17 wave 2 (lane monitor2): the clones that cannot leave but carry
    // a pc. Each copy names its own frame on them too when one describes their
    // pc ([`LoopBlockPlan::quiet`]).
    let mut quiet: Vec<NodeId> = Vec::new();
    for &v in &clone_order {
        let n = node(v).ok_or(None)?;
        if n.frame_snapshot.is_some() {
            return Err(Some("a body node names a frame of its own"));
        }
        if n.volatile_access
            || matches!(
                n.op,
                Op::LoadStatic {
                    is_volatile: true,
                    ..
                } | Op::StoreStatic {
                    is_volatile: true,
                    ..
                }
            )
        {
            return Err(Some("the body has a volatile access"));
        }
        if n.op.is_control() || n.op == Op::Phi || crate::ir_lower::node_cannot_deopt(graph, n) {
            // A data node only: it is what anchors a bci in the lowering.
            if !n.op.is_control() && n.op != Op::Phi && n.bytecode_pc.is_some() {
                quiet.push(v);
            }
            continue;
        }
        let admitted = match n.op {
            Op::Load(_) | Op::Store(_) => true,
            Op::LoadStatic {
                class_id,
                field_index,
                ..
            } => static_read_is_direct(graph, class_id, field_index),
            // MON11-1 (JIT round 19 wave 12, lane mon12): a direct primitive
            // `putstatic` (`methodCount++` under `synchronized (LOCK)`) under
            // `CRATONVM_JIT_IR_LOOP_LOCK_COARSEN_STATIC_STORES`.
            Op::StoreStatic {
                class_id,
                field_index,
                type_tag,
                is_volatile: false,
            } => static_store_is_direct(graph, class_id, field_index, type_tag),
            Op::Guard { bci } => n.bytecode_pc == Some(bci),
            // j27 wave 4 (lane MONROWS, MON17-5): a primitive element access
            // or a length, under the opt-in switch.
            _ => monitor_rows::block_admits_array_op(&n.op),
        };
        if !admitted {
            note_lock_coarsen_refusing_op("block", v, &n.op);
            return Err(Some(
                "a body node could call, allocate or resume elsewhere with the monitor held",
            ));
        }
        leaving.push(v);
    }
    // Nothing of the body is read outside it but by the header φs' back edge.
    for &v in &clone_order {
        for u in graph.users_of(v) {
            let Some(un) = node(u) else {
                continue;
            };
            if cloned.contains(&u) || u == exit || (u == header && v == back_ctrl) {
                continue;
            }
            let back_edge_only = carried_set.contains(&u)
                && un
                    .inputs
                    .iter()
                    .enumerate()
                    .all(|(slot, &i)| i != v || slot == back_slot);
            if !back_edge_only {
                return Err(Some("a body value is read outside the body"));
            }
        }
    }
    // The read may also feed a header φ's back edge (the monitor temp's local
    // when a frame at the header names it): every copy read copy 0's object.
    let back_edge_of = |u: NodeId, v: NodeId| {
        carried_set.contains(&u)
            && node(u).is_some_and(|un| {
                un.inputs
                    .iter()
                    .enumerate()
                    .all(|(slot, &i)| i != v || slot == back_slot)
            })
    };
    let read_in_body = monitor.rereads();
    for &v in [read, enter]
        .iter()
        .filter(|&&v| v != read || read_in_body)
        .chain(pre.iter())
        .chain(pre_float.iter())
    {
        if graph.users_of(v).into_iter().any(|u| {
            node(u).is_some()
                && !cloned.contains(&u)
                && !skip.contains(&u)
                && !(v == read && back_edge_of(u, v))
        }) {
            return Err(Some(
                "the monitor's read, a guard on it or the enter is read outside the block",
            ));
        }
    }
    if x_tok != enter && !cloned.contains(&x_tok) {
        return Err(Some("the exit's memory does not come from the block"));
    }

    // The frames each copy needs, and whether the original describes them.
    let mut wanted: Vec<(usize, bool)> = Vec::new();
    for &v in &leaving {
        let pc = node(v)
            .and_then(|n| n.bytecode_pc)
            .ok_or(Some("a body node that can leave has no bytecode pc"))?;
        wanted.push((pc, true));
    }
    let field_read = (monitor == BlockMonitor::Field).then_some(read);
    for &v in pre.iter().chain([enter].iter()).chain(field_read.iter()) {
        let pc = node(v)
            .and_then(|n| n.bytecode_pc)
            .ok_or(Some("the enter or a guard before it has no bytecode pc"))?;
        wanted.push((pc, false));
    }
    wanted.push((x_pc, true));
    let splices = crate::ir::splice_resume_map_this_build();
    let renamable = |s: NodeId| {
        s == NO_NODE
            || invariant(s)
            || cloned.contains(&s)
            || carried_set.contains(&s)
            || skip.contains(&s)
    };
    let mut frames: FxHashMap<usize, usize> = FxHashMap::default();
    for (pc, holds) in wanted {
        if splices
            .as_ref()
            .and_then(|m| m.resume_bci(pc))
            .is_some_and(|resumed| resumed != pc)
        {
            return Err(Some("a block node belongs to a spliced callee"));
        }
        let si = graph
            .safepoints
            .iter()
            .position(|sp| sp.bci == pc && !sp.is_splice_state())
            .ok_or(Some("a block node that can leave has no frame at its bci"))?;
        let sp = &graph.safepoints[si];
        let held = sp.monitors.iter().filter(|&&m| m == read).count();
        if holds {
            let innermost = sp.monitors.len().saturating_sub(1);
            if held != 1 || sp.monitors.last() != Some(&read) || sp.monitor_elided(innermost) {
                return Err(Some(
                    "a frame inside the block does not hold its monitor exactly once",
                ));
            }
        } else if held != 0 {
            return Err(Some("a frame before the enter already holds the monitor"));
        }
        if !sp
            .locals
            .iter()
            .chain(sp.stack.iter())
            .chain(sp.monitors.iter())
            .all(|&s| renamable(s))
        {
            return Err(Some("a frame names a value the copies cannot rename"));
        }
        frames.insert(pc, si);
    }
    // Round 17 wave 2 (lane monitor2): a frame for every other clone's pc
    // where an honest one exists -- inside the block (the monitor held exactly
    // once, innermost), renamable, not a spliced callee's. The partial
    // unroller's discipline (`install_copy_frames` stamps every clone): a
    // copy's node that names its own frame never anchors its bci
    // (`ir_lower::note_bci_anchor`), so the builder's snapshot there is
    // anchored only at the original's nodes and no by-bci reader of the
    // lowering finds it inside a copy. A pc with no such frame (after the
    // block's `monitorexit`, say) is left as before: its clones cannot leave.
    for &v in &quiet {
        let Some(pc) = node(v).and_then(|n| n.bytecode_pc) else {
            continue;
        };
        if frames.contains_key(&pc)
            || splices
                .as_ref()
                .and_then(|m| m.resume_bci(pc))
                .is_some_and(|resumed| resumed != pc)
        {
            continue;
        }
        let Some(si) = graph
            .safepoints
            .iter()
            .position(|sp| sp.bci == pc && !sp.is_splice_state())
        else {
            continue;
        };
        let sp = &graph.safepoints[si];
        let innermost = sp.monitors.len().saturating_sub(1);
        let holds_once = sp.monitors.iter().filter(|&&m| m == read).count() == 1
            && sp.monitors.last() == Some(&read)
            && !sp.monitor_elided(innermost);
        let renames = sp
            .locals
            .iter()
            .chain(sp.stack.iter())
            .chain(sp.monitors.iter())
            .all(|&s| renamable(s));
        if holds_once && renames {
            frames.insert(pc, si);
        }
    }
    let factor = if (!monitor.rereads() || trusted_final)
        && clone_order.len() <= 16
        && clone_order.len() * 16 <= LOOP_LOCK_COARSEN_MAX_CLONED
        && stride <= i64::from(i32::MAX) / 15
    {
        16
    } else if (!monitor.rereads() || trusted_final)
        && clone_order.len() <= 24
        && clone_order.len() * 8 <= LOOP_LOCK_COARSEN_MAX_CLONED
        && stride <= i64::from(i32::MAX) / 7
    {
        8
    } else {
        LOOP_LOCK_COARSEN_FACTOR
    };
    Ok(LoopBlockPlan {
        header,
        body_entry,
        back_ctrl,
        carried,
        back_slot,
        mem_phi,
        iv_phi,
        bound,
        stride,
        strict,
        cond,
        body_proj: if body_entry == projs[0] { 0 } else { 1 },
        read,
        monitor,
        trusted_final,
        enter,
        exit,
        pre,
        pre_float,
        clone_order,
        leaving,
        quiet,
        frames,
        factor,
    })
}

/// Round 17 wave 2 (lane monitor2): is `base` the node the lowering seeds
/// non-null in every block -- the FIRST `Op::Param` of the receiver slot
/// (`ir_lower`'s `seed_block_null_proofs`), with that seed switched on
/// (`CRATONVM_JIT_IR_THIS_NONNULL`, default on)? Then an `Op::Load` of it emits
/// no null check anywhere, which is what lets a coarsened `Field` monitor's
/// re-read run while copy 0's object is held.
pub(in crate::ir_optimize) fn field_monitor_base_is_seeded_receiver(
    graph: &Graph,
    base: NodeId,
) -> bool {
    let Some(recv) = graph.receiver_param else {
        return false;
    };
    crate::ir_lower::ir_this_nonnull_enabled()
        && graph
            .nodes
            .iter()
            .position(|n| n.op == Op::Param(recv))
            .is_some_and(|first| first == base as usize)
}

/// `CRATONVM_JIT_IR_LOOP_LOCK_TRUST_STATIC_FINAL=1` (opt-in; JIT round 17
/// wave 2, lane monitor2; MON3-3 = MON4-3 = MON5-4): a block coarsened on a
/// `static final` reference monitor of an initialised class
/// ([`static_final_monitor_is_trusted`]) takes it as one object for the whole
/// loop: copies 1..K make no re-read and no identity exit (the
/// [`BlockMonitor::Invariant`] shape, with the read still made once per group
/// by copy 0). Inert unless a block switch coarsens the loop. Read live, once
/// per planned loop (no `static`). `lib.rs` (`ir_tier_attempt`) feeds
/// `Graph::trusted_final_static_fields` only while it is on (round 17 wave 2,
/// lane irfeed).
pub(crate) fn ir_loop_lock_trust_static_final_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_STATIC_FINAL_ARRAY_LENGTH`, **default ON** (`=0` is the
/// kill switch) -- JIT round 20 wave 11, lane pIR (L3W6-2 of
/// `docs/feature-designs/jit-ir-optimizations.md`, W6L4-5 of
/// `-w6-L4.md`): `arraylength` of a trusted `static final` array of an
/// initialised class folds to the length the VM filed at the end of the
/// class's `<clinit>` ([`fold_trusted_static_array_lengths`]). Read by both
/// halves (the VM's filing hook and this compiler's feed and fold), never
/// cached (no `static`).
pub fn ir_static_final_array_length_enabled() -> bool {
    true
}

/// Fold `ArrayLength(LoadStatic f)` to `Const(len)` for every trusted
/// `static final` array field `f` in [`Graph::trusted_static_array_lengths`].
/// Returns how many nodes it folded.
///
/// # Why it is exact
///
/// The field is `static final`, its class is initialised (so `<clinit>`, the
/// only code that may write it in a class file of version 53+, has finished),
/// and the VM filed `len` from the very array the field held then: the field
/// holds that array forever, and an array's length never changes. HotSpot's C2
/// folds the same thing (a `static final` of an initialised class is a
/// constant; JLS 17.5.3). The array is non-null (a null field is never filed),
/// so dropping the `ArrayLength`'s null check is exact too. Only the LENGTH is
/// trusted; the reference stays a load and the elements stay memory.
///
/// # Refusals
///
/// * a volatile read (a final is never volatile; belt and braces);
/// * an `ArrayLength` that anchors an exception edge (`Op::CallThrew`): its
///   flag reads the node's own check, which a constant no longer has;
/// * the switch off, or an empty table: nothing changes.
pub(in crate::ir_optimize) fn fold_trusted_static_array_lengths(graph: &mut Graph) -> usize {
    if (graph.trusted_static_array_lengths.is_empty() && graph.const_ref_meta.is_empty())
        || !ir_static_final_array_length_enabled()
    {
        return 0;
    }
    let anchors = crate::ir::exception_edge_anchors(graph);
    let mut folds: Vec<(NodeId, i32)> = Vec::new();
    for (idx, n) in graph.nodes.iter().enumerate() {
        if n.op != Op::ArrayLength {
            continue;
        }
        let array = match n.inputs.len() {
            1 => n.inputs[0],
            len if len >= 3 => n.inputs[2],
            _ => continue,
        };
        let Some(src) = graph.node_opt(array) else {
            continue;
        };
        let length = match src.op {
            Op::LoadStatic {
                class_id,
                field_index,
                is_volatile: false,
                ..
            } => {
                let Some(&(_, len)) = graph
                    .trusted_static_array_lengths
                    .iter()
                    .find(|&&(key, _)| key == (class_id, field_index))
                else {
                    continue;
                };
                len
            }
            Op::IndyRootRef { slot_addr } => {
                let Some(meta) = graph.const_ref_meta.get(&slot_addr) else {
                    continue;
                };
                let Some(len) = meta.array_length.or_else(|| {
                    let key = meta.static_field?;
                    graph
                        .trusted_static_array_lengths
                        .iter()
                        .find(|&&(k, _)| k == key)
                        .map(|&(_, len)| len)
                }) else {
                    continue;
                };
                len
            }
            _ => continue,
        };
        if length < 0 {
            continue;
        }
        let Ok(id) = NodeId::try_from(idx) else {
            continue;
        };
        if anchors.contains(&id) {
            continue;
        }
        folds.push((id, length));
    }
    for &(id, length) in &folds {
        // Rewritten in place, as `fold_constants` rewrites: same id, type
        // (`Int`) and bytecode pc; the edges go through `clear_inputs`.
        graph.nodes[id as usize].op = Op::Const(i64::from(length));
        graph.clear_inputs(id);
    }
    if !folds.is_empty() && false {
        eprintln!(
            "[ir] static final array length: {} arraylength(s) folded to a constant ({:?})",
            folds.len(),
            folds
                .iter()
                .map(|&(_, length)| length)
                .collect::<Vec<i32>>(),
        );
    }
    folds.len()
}

/// `CRATONVM_JIT_IR_FRESH_ARRAY_LENGTH_FOLD` (JIT round 21 wave 2, lane alloc;
/// opt-in, default OFF): fold `ArrayLength(NewArray(.., Const(n)))` to
/// `Const(n)` ([`fold_fresh_array_lengths`]). Read once per compile.
pub fn ir_fresh_array_length_fold_enabled() -> bool {
    false
}

/// Fold `ArrayLength(a)` to `Const(n)` where `a` is an `Op::NewArray` of this
/// graph whose length operand is `Const(n)`, `0 <= n <= i32::MAX`. Returns how
/// many nodes it folded.
///
/// # Why it is exact
///
/// An array's length never changes, and `a` was created with length `n`. The
/// `arraylength` cannot throw: `a` is a fresh allocation, non-null by
/// construction. A negative constant is left alone (its `newarray` throws,
/// and nothing after it runs); so is an `ArrayLength` anchoring an exception
/// edge, as [`fold_trusted_static_array_lengths`] refuses it.
///
/// # What it is for
///
/// Escape analysis refuses to scalar-replace an array whose length is read
/// (`ArrayRefusal::LengthReadNotFolded`: no applier arm folds the read). With
/// the read folded first, `byte[] b = new byte[16]; s += b.length;` leaves the
/// array with no use at all (`R21AllocLean local-byte16`; HotSpot 0 ns).
pub(in crate::ir_optimize) fn fold_fresh_array_lengths(graph: &mut Graph) -> usize {
    if !ir_fresh_array_length_fold_enabled() {
        return 0;
    }
    fold_fresh_array_lengths_ungated(graph)
}

/// JIT round 23 wave 2b, lane L1 (churn page J3; `CRATONVM_JIT_EA_DEAD_ARRAY_ELIM`,
/// perf, default ON, `=0` kill): [`fold_fresh_array_lengths`] asked at the
/// top of every escape-analysis round, without the r21 switch. The pre-pass
/// fold above sees `dead.payload.length` as a field LOAD of the object
/// `new Node(.., new byte[n], ..)` stores the array into; only the round that
/// scalar-replaces the `Node` forwards that load to the `Op::NewArray`, so
/// the read was never folded and the array kept a live use
/// (`ArrayRefusal::LengthTooLarge` / `LengthReadNotFolded` on the churn loop).
/// Folded here, the next EA question sees an array nothing uses, which
/// `escape_analysis::dead_array_elim_enabled` deletes. Returns how many reads
/// were folded (counted by the caller as the round's applied work).
pub(crate) fn fold_fresh_array_lengths_in_ea_round(graph: &mut Graph) -> usize {
    if !crate::escape_analysis::dead_array_elim_enabled() {
        return 0;
    }
    fold_fresh_array_lengths_ungated(graph)
}

/// The fold of [`fold_fresh_array_lengths`], whoever gates it.
pub(in crate::ir_optimize) fn fold_fresh_array_lengths_ungated(graph: &mut Graph) -> usize {
    let anchors = crate::ir::exception_edge_anchors(graph);
    let mut folds: Vec<(NodeId, i64)> = Vec::new();
    for (idx, n) in graph.nodes.iter().enumerate() {
        if n.op != Op::ArrayLength {
            continue;
        }
        let array = match n.inputs.len() {
            1 => n.inputs[0],
            len if len >= 3 => n.inputs[2],
            _ => continue,
        };
        let Some(src) = graph.node_opt(array) else {
            continue;
        };
        if !matches!(src.op, Op::NewArray { .. }) {
            continue;
        }
        let Some(&len_node) = src.inputs.get(2) else {
            continue;
        };
        let length = match graph.node_opt(len_node).map(|l| &l.op) {
            Some(Op::Const(v)) => *v,
            _ => continue,
        };
        if !(0..=i64::from(i32::MAX)).contains(&length) {
            continue;
        }
        let Ok(id) = NodeId::try_from(idx) else {
            continue;
        };
        if anchors.contains(&id) {
            continue;
        }
        folds.push((id, length));
    }
    for &(id, length) in &folds {
        // In place, as `fold_trusted_static_array_lengths` rewrites.
        graph.nodes[id as usize].op = Op::Const(length);
        graph.clear_inputs(id);
    }
    if !folds.is_empty() && false {
        eprintln!(
            "[ir] fresh array length: {} arraylength(s) of a constant-length newarray folded",
            folds.len()
        );
    }
    folds.len()
}

/// May the coarsening treat the `getstatic` of `(class_id, field_index)` as
/// a constant for the length of the loop? Only under
/// [`ir_loop_lock_trust_static_final_enabled`], and only when this compile
/// both reads it directly (its class is initialised: no `<clinit>` can still
/// write it; the resolver also declines `java/lang/System`, whose `in` / `out`
/// / `err` are `static final` fields `System.setOut` rewrites) and was told by
/// the compile's feed that the field is `static final`, of reference type, and
/// trustworthy (`Graph::trusted_final_static_fields`). HotSpot's rule: a
/// `static final` field of an initialised class is a constant (JLS 17.5.3
/// lets a compiler ignore reflective and `Unsafe` writes to it); its VALUE is
/// trusted, never the elements of an array it holds.
pub(in crate::ir_optimize) fn static_final_monitor_is_trusted(
    graph: &Graph,
    class_id: u32,
    field_index: u32,
) -> bool {
    ir_loop_lock_trust_static_final_enabled()
        && static_read_is_direct(graph, class_id, field_index)
        && graph
            .trusted_final_static_fields
            .contains(&(class_id, field_index))
}

/// Clone `order` (ascending) at the end of the arena, renaming every input
/// through `subst`, which gains each clone. Two phases: a merge's or a φ's
/// input may name a node of `order` that comes later. Returns `(original,
/// clone)` pairs.
pub(in crate::ir_optimize) fn loop_block_clone_all(
    graph: &mut Graph,
    order: &[NodeId],
    subst: &mut FxHashMap<NodeId, NodeId>,
) -> Vec<(NodeId, NodeId)> {
    let mut made: Vec<(NodeId, NodeId)> = Vec::with_capacity(order.len());
    for &n in order {
        let (op, ty, pc, volatile, inputs) = {
            let nd = &graph.nodes[n as usize];
            (
                nd.op.clone(),
                nd.ty,
                nd.bytecode_pc,
                nd.volatile_access,
                nd.inputs.clone(),
            )
        };
        let id = graph.add_inputs(op, ty, inputs, pc);
        if volatile {
            graph.set_volatile_access(id);
        }
        subst.insert(n, id);
        made.push((n, id));
    }
    for &(n, id) in &made {
        let arity = graph.nodes[n as usize].inputs.len();
        for slot in 0..arity {
            let orig = graph.nodes[n as usize].inputs[slot];
            if let Some(&renamed) = subst.get(&orig) {
                if renamed != orig {
                    graph.set_input(id, slot, renamed);
                }
            }
        }
    }
    made
}

/// Append a copy of snapshot `si` with every slot renamed through `subst` (its
/// per-entry nested-lock marks kept); returns the copy's index.
pub(in crate::ir_optimize) fn loop_block_copy_frame(
    graph: &mut Graph,
    si: usize,
    subst: &FxHashMap<NodeId, NodeId>,
) -> u32 {
    let rename = |n: &NodeId| subst.get(n).copied().unwrap_or(*n);
    let (bci, locals, stack, monitors, mask) = {
        let sp = &graph.safepoints[si];
        let locals: Vec<NodeId> = sp.locals.iter().map(rename).collect();
        let stack: Vec<NodeId> = sp.stack.iter().map(rename).collect();
        let monitors: Vec<NodeId> = sp.monitors.iter().map(rename).collect();
        (sp.bci, locals, stack, monitors, sp.monitor_elided_mask)
    };
    let mut copy = SafepointSnapshot::new(bci, locals, stack, monitors);
    copy.monitor_elided_mask = mask;
    // Cast: one snapshot per bci and copy, far below `u32::MAX`.
    let idx = graph.safepoints.len() as u32;
    graph.push_safepoint(copy);
    idx
}

/// Stamp `node` with snapshot `snap`. The plan checked that `snap` is a copy
/// of the snapshot at the node's own bci, so the bind cannot be refused.
pub(in crate::ir_optimize) fn loop_block_stamp(graph: &mut Graph, node: NodeId, snap: u32) {
    let stamped = graph.set_node_frame_snapshot(node, snap);
    debug_assert!(stamped, "a per-copy block frame was refused at its own bci");
}

/// Rewrite the loop `plan` describes (see the section comment). Every check
/// ran in [`plan_loop_block_coarsening`]; nothing here refuses.
pub(in crate::ir_optimize) fn apply_loop_block_coarsening(
    graph: &mut Graph,
    plan: &LoopBlockPlan,
) -> LoopBlockApplied {
    let k = plan.factor;
    let body_users = graph.users_of(plan.body_entry);
    let (read_op, read_pc, read_inputs) = {
        let r = &graph.nodes[plan.read as usize];
        let inputs: Vec<NodeId> = r.inputs.iter().copied().collect();
        (r.op.clone(), r.bytecode_pc, inputs)
    };
    let rereads = plan.monitor.rereads();
    // Round 17 wave 2 (lane monitor2): a trusted `static final` monitor is
    // still read once (copy 0's), but not per copy.
    let per_copy = plan.rereads_per_copy();
    let e_pc = graph.nodes[plan.enter as usize].bytecode_pc;
    let (x_pc, x_tok) = {
        let x = &graph.nodes[plan.exit as usize];
        (x.bytecode_pc, x.inputs[1])
    };
    let mut frames = 0usize;

    // The fork: `K` iterations left, or the original body.
    let span = graph.add(
        Op::Const((k as i64 - 1) * plan.stride),
        IrType::Int,
        vec![],
        None,
    );
    let left = graph.add(Op::Sub, IrType::Int, vec![plan.bound, plan.iv_phi], None);
    let cc = if plan.strict { CmpOp::Gt } else { CmpOp::Ge };
    let room = graph.add(Op::Cmp(cc), IrType::Int, vec![left, span], None);
    // No bytecode pc: no profile describes this branch, and no OSR entry may
    // land on either arm.
    let fork = graph.add(Op::If, IrType::Control, vec![plan.body_entry, room], None);
    let fast = graph.add(Op::Proj(0), IrType::Control, vec![fork], None);
    let slow = graph.add(Op::Proj(1), IrType::Control, vec![fork], None);

    // Copy 0's prologue: the read, its guards, the one enter -- at the
    // header's values, before anything is held.
    let mut shared: FxHashMap<NodeId, NodeId> = FxHashMap::default();
    shared.insert(plan.body_entry, fast);
    // An invariant monitor is locked as it is; a read is made once for copy 0.
    let l0 = if rereads {
        loop_window_clone(graph, plan.read, &shared)
    } else {
        plan.read
    };
    shared.insert(plan.read, l0);
    let mut prologue: Vec<NodeId> = plan
        .pre
        .iter()
        .chain(plan.pre_float.iter())
        .copied()
        .collect();
    prologue.sort_unstable();
    let pre_made = loop_block_clone_all(graph, &prologue, &mut shared);
    let tok0 = if rereads && graph.nodes[plan.enter as usize].input_opt(1) == Some(plan.read) {
        l0
    } else {
        plan.mem_phi
    };
    let enter = graph.add(Op::MonitorEnter, IrType::Memory, vec![fast, tok0, l0], e_pc);
    let field_read = (plan.monitor == BlockMonitor::Field).then_some((plan.read, l0));
    // j27 wave 4 (lane MONROWS, MON17W2-2): copy 0's read proves its base for
    // the fast copies it dominates.
    monitor_rows::note_field_monitor_base_proof(graph, field_read.is_some(), l0);
    let mut before: FxHashMap<usize, u32> = FxHashMap::default();
    for (orig, id) in pre_made
        .into_iter()
        .chain([(plan.enter, enter)])
        .chain(field_read)
    {
        if orig != plan.enter && orig != plan.read && !plan.pre.contains(&orig) {
            continue;
        }
        let Some(pc) = graph.nodes[orig as usize].bytecode_pc else {
            continue;
        };
        let Some(&si) = plan.frames.get(&pc) else {
            continue;
        };
        let snap = match before.get(&pc) {
            Some(&s) => s,
            None => {
                let s = loop_block_copy_frame(graph, si, &shared);
                before.insert(pc, s);
                frames += 1;
                s
            }
        };
        loop_block_stamp(graph, id, snap);
    }

    // `K` copies of the block, each entered with the previous one's values;
    // copies 1..K first re-read the monitor and leave unless it is copy 0's.
    let mut cur: FxHashMap<NodeId, NodeId> = plan
        .carried
        .iter()
        .copied()
        .filter(|&p| p != plan.mem_phi)
        .map(|p| (p, p))
        .collect();
    let mut start = fast;
    let mut window_tok = enter;
    let mut last_exit_frame: Option<u32> = None;
    let mut bails = 0usize;
    for j in 0..k {
        let (entry, entry_tok) = if j == 0 {
            (fast, enter)
        } else if !per_copy {
            // The same object every copy: the copies simply follow.
            (start, window_tok)
        } else {
            // The read's own inputs past control and memory (a getfield's
            // base and offset) are invariant.
            let mut ins = read_inputs.clone();
            ins[0] = start;
            ins[1] = window_tok;
            let lj = graph.add(read_op.clone(), IrType::Ref, ins, read_pc);
            let same = graph.add(Op::Cmp(CmpOp::Eq), IrType::Int, vec![lj, l0], None);
            let test = graph.add(Op::If, IrType::Control, vec![start, same], None);
            let ok = graph.add(Op::Proj(0), IrType::Control, vec![test], None);
            let bail = graph.add(Op::Proj(1), IrType::Control, vec![test], None);
            // Iteration j has not started: release copy 0's hold (the frame
            // is iteration j-1's at the block's `monitorexit`) and let the
            // header run iteration j from the values entering copy j.
            let bx = graph.add(Op::MonitorExit, IrType::Memory, vec![bail, lj, l0], x_pc);
            if let Some(snap) = last_exit_frame {
                loop_block_stamp(graph, bx, snap);
            }
            graph.push_input(plan.header, bail);
            for &p in &plan.carried {
                let v = if p == plan.mem_phi {
                    bx
                } else {
                    cur.get(&p).copied().unwrap_or(p)
                };
                graph.push_input(p, v);
            }
            bails += 1;
            (ok, lj)
        };
        let mut subst = shared.clone();
        subst.insert(plan.body_entry, entry);
        subst.insert(plan.enter, entry_tok);
        for (&p, &v) in &cur {
            subst.insert(p, v);
        }
        loop_block_clone_all(graph, &plan.clone_order, &mut subst);
        // This copy's frames: every leaving clone's, every other clone's
        // whose pc has one (round 17 wave 2), and the exit's.
        let mut at: FxHashMap<usize, u32> = FxHashMap::default();
        for &v in plan.leaving.iter().chain(plan.quiet.iter()) {
            let Some(pc) = graph.nodes[v as usize].bytecode_pc else {
                continue;
            };
            let (Some(&si), Some(&clone)) = (plan.frames.get(&pc), subst.get(&v)) else {
                continue;
            };
            let snap = match at.get(&pc) {
                Some(&s) => s,
                None => {
                    let s = loop_block_copy_frame(graph, si, &subst);
                    at.insert(pc, s);
                    frames += 1;
                    s
                }
            };
            loop_block_stamp(graph, clone, snap);
        }
        // Only when something will name it (round 17 wave 2, lane monitor2):
        // the next copy's identity exit, or the fast exit after the last copy.
        // A copy that is followed without a re-read used to append a frame no
        // node claimed -- a duplicate at the exit's bci that `ir_verify`
        // reports and the by-bci readers would resolve by position.
        let exit_frame_named = per_copy || j + 1 == k;
        if let (Some(pc), true) = (x_pc, exit_frame_named) {
            if let Some(&si) = plan.frames.get(&pc) {
                let snap = match at.get(&pc) {
                    Some(&s) => s,
                    None => {
                        frames += 1;
                        loop_block_copy_frame(graph, si, &subst)
                    }
                };
                last_exit_frame = Some(snap);
            }
        }
        window_tok = subst.get(&x_tok).copied().unwrap_or(x_tok);
        start = subst.get(&plan.back_ctrl).copied().unwrap_or(start);
        let next: Vec<(NodeId, NodeId)> = cur
            .keys()
            .map(|&p| {
                let back = graph.nodes[p as usize].inputs[plan.back_slot];
                (p, subst.get(&back).copied().unwrap_or(back))
            })
            .collect();
        cur.extend(next);
    }
    let exit = graph.add(
        Op::MonitorExit,
        IrType::Memory,
        vec![start, window_tok, l0],
        x_pc,
    );
    if let Some(snap) = last_exit_frame {
        loop_block_stamp(graph, exit, snap);
    }

    // The fast path's back edge, each φ taking its value after the K-th copy.
    graph.push_input(plan.header, start);
    for &p in &plan.carried {
        let v = if p == plan.mem_phi {
            exit
        } else {
            cur.get(&p).copied().unwrap_or(p)
        };
        graph.push_input(p, v);
    }

    // The original body hangs off the fork's other arm.
    for u in body_users {
        if graph.node_opt(u).is_none_or(|n| n.op == Op::Dead) {
            continue;
        }
        let arity = graph.nodes[u as usize].inputs.len();
        for slot in 0..arity {
            if graph.nodes[u as usize].inputs[slot] == plan.body_entry {
                graph.set_input(u, slot, slow);
            }
        }
    }
    LoopBlockApplied {
        enter,
        exit,
        frames,
        bails,
    }
}

// ── The in-place shape (round 17 wave 1, lane monitor) ───────────────────
//
// Under `CRATONVM_JIT_IR_LOOP_LOCK_COARSEN_IN_PLACE`. The forked shape above
// keeps the ORIGINAL block on the slow arm and builds the fast arm out of
// clones, so every bci of the block exists twice, on two disjoint paths. The
// builder's snapshot at such a bci -- the first one there, which every by-bci
// reader of the lowering and of the VM's resume finds, and which the
// lowering's slot liveness merges into every site at that bci -- then names
// the SLOW arm's nodes while the fast arm's sites run without them. The
// partial unroller, whose per-copy frames this pass borrowed, never builds
// that: its copies follow each other on one path, so the first snapshot at a
// bci names copy 0's values, which dominate every later copy. Round 16 wave 16
// and round 17 (monitor5) measured the forked shape coarsening (`coarsened
// x4`) without one inline enter fewer at run time (1,999,000 in both arms of
// `R16MonitorCrowd uncontended-1t`, 23.93M vs 23.92M on
// `R16Monitor5InstanceBlock`) and slower timings, which reads as the
// coarsened compile not being the body that ran.
//
// This shape is the unroller's: the original body IS copy 0, and copies
// 1..K-1 follow it on the same path, each behind
//
// * the loop test, re-asked for the iteration it is about to run (`i_j < n`
//   on the counter entering it, the header's own `Cmp` with the counter
//   renamed). When it fails: `exit(o)`, then the header, which asks the same
//   question about the same values, gets the same answer and leaves the loop;
// * for a re-read monitor, the identity test of the forked shape.
//
// ```text
// header: i < n ?
// body:   o = getstatic S ; [guard o] ; enter(o) ; copy 0 (the original nodes)
//         i1 < n ?                       else exit(o) -> header
//         o1 = getstatic S ; o1 == o ?   else exit(o) -> header
//         copy 1 ; ... ; copy K-1 ; exit(o) -> header
// ```
//
// No fork: the last `n % K` iterations are a group that leaves early, so they
// are coarsened too, and the block exists K times rather than K + 1. Every
// early exit releases the one hold before the header, stamped with the frame
// of the iteration it ends (copy j-1's frame at the block's `monitorexit`;
// copy 0's is the builder's own), and the group's last exit takes copy K-1's.
// The JMM, frame, GC and inflation arguments of the forked shape hold as they
// stand: each copy's leaving node resumes into its own iteration holding `o`
// once (javac's catch-all releases exactly that hold on a throw); the elided
// `exit(o); enter(o)` between two copies has only the pure loop test and the
// plain re-read between them; the hold is bounded by K iterations with no
// call and no poll inside (the back-edge polls are on the edges INTO the
// header, after every exit).

/// Release copy 0's hold on control edge `ctrl` (memory `tok`) and rejoin the
/// header with `cur`, the values entering the iteration that did not start.
/// `frame`: the frame of the iteration the exit ends.
pub(in crate::ir_optimize) fn loop_block_exit_to_header(
    graph: &mut Graph,
    plan: &LoopBlockPlan,
    ctrl: NodeId,
    tok: NodeId,
    x_pc: Option<usize>,
    frame: Option<u32>,
    cur: &FxHashMap<NodeId, NodeId>,
) {
    let bx = graph.add(
        Op::MonitorExit,
        IrType::Memory,
        vec![ctrl, tok, plan.read],
        x_pc,
    );
    if let Some(snap) = frame {
        loop_block_stamp(graph, bx, snap);
    }
    graph.push_input(plan.header, ctrl);
    for &p in &plan.carried {
        let v = if p == plan.mem_phi {
            bx
        } else {
            cur.get(&p).copied().unwrap_or(p)
        };
        graph.push_input(p, v);
    }
}
