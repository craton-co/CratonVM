    use super::*;
    use crate::ir::IrBuilder;
    use crate::ir_schedule;
    use std::collections::HashSet;

    // ── Fixtures ─────────────────────────────────────────────────────

    /// A hand-written single-block liveness model.
    ///
    /// [`build_live_model`] is exercised end to end at the bottom of this
    /// module, on real scheduled graphs. These fixtures exist because the
    /// allocator's interesting behaviour — who wins an eviction, where a split
    /// lands, which register a clobber pushes a value out of — is a function of
    /// *intervals*, and steering a scheduler into an exact pair of positions is
    /// neither reliable nor the thing under test.
    struct Fixture {
        graph: Graph,
        live: LiveModel,
    }

    impl Fixture {
        fn new(total_positions: usize) -> Fixture {
            Fixture {
                graph: Graph::empty(0),
                live: LiveModel {
                    pos_of: Vec::new(),
                    span: vec![(0, total_positions.saturating_sub(1))],
                    total_positions,
                    block_of_pos: vec![0; total_positions],
                    wants_loc: Vec::new(),
                    range: Vec::new(),
                    class: Vec::new(),
                    is_ref: Vec::new(),
                    pinned: Vec::new(),
                    uses: Vec::new(),
                    weight: Vec::new(),
                    loop_depth: vec![0],
                    peak_live: 0,
                    carried: Vec::new(),
                    carried_and_read: Vec::new(),
                    converged: true,
                    block_live_in: Vec::new(),
                },
            }
        }

        fn value(&mut self, op: Op, ty: IrType, lo: usize, hi: usize, uses: &[usize]) -> NodeId {
            self.push(op, ty, lo, hi, uses, false)
        }

        /// A value `ir_lower` owns the home of — a phi, or anything a deopt
        /// frame names. Never promoted.
        fn home_bound(&mut self, ty: IrType, lo: usize, hi: usize, uses: &[usize]) -> NodeId {
            self.push(Op::Phi, ty, lo, hi, uses, true)
        }

        fn push(
            &mut self,
            op: Op,
            ty: IrType,
            lo: usize,
            hi: usize,
            uses: &[usize],
            pin: bool,
        ) -> NodeId {
            let id = self.graph.add(op, ty, vec![], None);
            assert_eq!(
                id as usize,
                self.live.wants_loc.len(),
                "fixture node ids must stay dense"
            );
            assert!(
                lo <= hi && hi < self.live.total_positions,
                "interval [{lo}, {hi}] does not fit the fixture"
            );
            self.live.pos_of.push(Some(lo));
            self.live.wants_loc.push(true);
            self.live.range.push(Some(PosRange { lo, hi }));
            self.live.class.push(RegClass::of(ty));
            self.live.is_ref.push(ty == IrType::Ref);
            self.live.pinned.push(pin);
            let mut u: Vec<usize> = uses.to_vec();
            u.sort_unstable();
            u.dedup();
            self.live.weight.push(u.len().max(1) as u64);
            self.live.uses.push(u);
            id
        }

        fn finish(mut self) -> (Graph, LiveModel) {
            let mut delta = vec![0i64; self.live.total_positions + 2];
            for r in self.live.range.iter().flatten() {
                delta[r.lo] += 1;
                delta[r.hi + 1] -= 1;
            }
            let (mut running, mut peak) = (0i64, 0i64);
            for d in &delta {
                running += d;
                peak = peak.max(running);
            }
            self.live.peak_live = peak.max(0) as usize;
            (self.graph, self.live)
        }
    }

    /// `n` callee-saved GP registers, handed out r0 first.
    fn gp(n: u8) -> RegFile {
        RegFile::from_specs((0..n).map(|num| RegSpec {
            reg: PhysReg::gp(num),
            caller_saved: false,
        }))
    }

    /// `volatile` caller-saved GP registers **first**, then `saved`
    /// callee-saved ones. The preference order is what makes "a value that
    /// crosses a call has to skip the volatile registers" observable: with the
    /// callee-saved ones first every value would trivially survive.
    fn gp_volatile_first(volatile: u8, saved: u8) -> RegFile {
        RegFile::from_specs(
            (0..volatile)
                .map(|num| RegSpec {
                    reg: PhysReg::gp(num),
                    caller_saved: true,
                })
                .chain((volatile..volatile + saved).map(|num| RegSpec {
                    reg: PhysReg::gp(num),
                    caller_saved: false,
                })),
        )
    }

    fn bare_model(regs: RegFile) -> MachineModel {
        MachineModel {
            regs,
            clobbers: Vec::new(),
            fixed: Vec::new(),
            safepoints: Vec::new(),
            refs_may_cross_safepoints: false,
            reads_precede_clobber: Vec::new(),
            read_before_write: Vec::new(),
        }
    }

    fn internal_message(err: &Bailout) -> String {
        match &err.reason {
            BailoutReason::Internal(msg) => (*msg).to_string(),
            other => panic!("expected an Internal bailout, got {other:?}"),
        }
    }

    // ── The allocator core ───────────────────────────────────────────

    #[test]
    fn read_write_handoffs_keep_a_one_register_chain_resident() {
        let mut f = Fixture::new(12);
        let a = f.value(Op::Add, IrType::Int, 0, 3, &[3]);
        let b = f.value(Op::Sub, IrType::Int, 3, 6, &[6]);
        let c = f.value(Op::Mul, IrType::Int, 6, 9, &[9]);
        let (graph, live) = f.finish();
        let mut model = bare_model(gp(1));
        model.read_before_write = vec![
            ReadBeforeWrite { result: b, inputs: vec![a], pos: 3 },
            ReadBeforeWrite { result: c, inputs: vec![b], pos: 6 },
        ];
        let alloc = allocate_linear_scan(&graph, &live, &model).unwrap();
        for (id, at) in [(a, 3), (b, 3), (b, 6), (c, 6)] {
            assert_eq!(alloc.location_at(id, at), Some(ValueLoc::Reg(PhysReg::gp(0))));
        }
        assert_eq!(alloc.stack_slots, 0);
        assert!(alloc.events.is_empty(), "no reload or copy between tied instructions");
        verify_allocation(&graph, &live, &model, &alloc).unwrap();
        // The same allocation is illegal without the emitter's ordering proof.
        model.read_before_write.clear();
        assert!(verify_allocation(&graph, &live, &model, &alloc).is_err());
    }

    #[test]
    fn read_write_handoffs_prefer_the_tied_left_operand() {
        let mut f = Fixture::new(12);
        let rhs = f.value(Op::Add, IrType::Long, 0, 3, &[3]);
        let lhs = f.value(Op::Add, IrType::Long, 1, 3, &[3]);
        let dst = f.value(Op::Sub, IrType::Long, 3, 9, &[9]);
        let (graph, live) = f.finish();
        let mut model = bare_model(gp(2));
        model.read_before_write.push(ReadBeforeWrite { result: dst, inputs: vec![lhs, rhs], pos: 3 });
        let alloc = allocate_linear_scan(&graph, &live, &model).unwrap();
        assert_ne!(alloc.first_reg(lhs), alloc.first_reg(rhs));
        assert_eq!(alloc.first_reg(dst), alloc.first_reg(lhs), "reuse lhs before register preference order");
        verify_allocation(&graph, &live, &model, &alloc).unwrap();
    }

    #[test]
    fn read_write_handoffs_keep_live_after_inputs_separate() {
        let mut f = Fixture::new(12);
        let input = f.value(Op::Add, IrType::Int, 0, 9, &[3, 9]);
        let dst = f.value(Op::Sub, IrType::Int, 3, 6, &[6]);
        let (graph, live) = f.finish();
        let mut model = bare_model(gp(2));
        model.read_before_write.push(ReadBeforeWrite { result: dst, inputs: vec![input], pos: 3 });
        let alloc = allocate_linear_scan(&graph, &live, &model).unwrap();
        assert_ne!(alloc.location_at(input, 3), alloc.location_at(dst, 3));
        assert!(matches!(alloc.location_at(input, 3), Some(ValueLoc::Reg(_))));
        assert!(matches!(alloc.location_at(dst, 3), Some(ValueLoc::Reg(_))));
        let mut corrupt = alloc.clone();
        corrupt.segments[dst as usize][0].reg = alloc.first_reg(input);
        assert!(verify_allocation(&graph, &live, &model, &corrupt).is_err());
    }

    #[test]
    fn read_write_handoffs_do_not_share_frame_words() {
        let mut f = Fixture::new(12);
        let input = f.value(Op::Add, IrType::Int, 0, 3, &[3]);
        let dst = f.value(Op::Sub, IrType::Int, 3, 6, &[6]);
        let (graph, live) = f.finish();
        let mut model = bare_model(gp(0));
        model.read_before_write.push(ReadBeforeWrite { result: dst, inputs: vec![input], pos: 3 });
        let alloc = allocate_linear_scan(&graph, &live, &model).unwrap();
        assert_ne!(alloc.stack_slot[input as usize], alloc.stack_slot[dst as usize]);
        verify_allocation(&graph, &live, &model, &alloc).unwrap();
    }

    #[test]
    fn read_write_handoffs_do_not_bypass_clobbers_or_fixed_operands() {
        let mut f = Fixture::new(12);
        let input = f.value(Op::Add, IrType::Int, 0, 3, &[3]);
        let dst = f.value(Op::Sub, IrType::Int, 3, 6, &[6]);
        let (graph, live) = f.finish();
        for fixed in [false, true] {
            let mut model = bare_model(gp(1));
            model.read_before_write.push(ReadBeforeWrite { result: dst, inputs: vec![input], pos: 3 });
            if fixed {
                model.fixed.push(FixedConstraint { node: input, pos: 3, reg: PhysReg::gp(0) });
            } else {
                model.add_clobbers(3, &[PhysReg::gp(0)]);
            }
            let alloc = allocate_linear_scan(&graph, &live, &model).unwrap();
            assert!(!(alloc.location_at(input, 3) == Some(ValueLoc::Reg(PhysReg::gp(0)))
                && alloc.location_at(dst, 3) == Some(ValueLoc::Reg(PhysReg::gp(0)))));
            verify_allocation(&graph, &live, &model, &alloc).unwrap();
        }
    }

    #[test]
    fn read_write_handoffs_reject_invalid_boundaries() {
        let mut f = Fixture::new(12);
        let input = f.value(Op::Add, IrType::Int, 0, 3, &[3]);
        let dst = f.value(Op::Sub, IrType::Int, 3, 6, &[6]);
        let (graph, live) = f.finish();
        let valid = ReadBeforeWrite { result: dst, inputs: vec![input], pos: 3 };
        for writes in [
            vec![ReadBeforeWrite { pos: 2, ..valid.clone() }],
            vec![ReadBeforeWrite { inputs: vec![dst], ..valid.clone() }],
            vec![ReadBeforeWrite { inputs: vec![NO_NODE], ..valid.clone() }],
            vec![ReadBeforeWrite { inputs: vec![], ..valid.clone() }],
            vec![valid.clone(), valid.clone()],
        ] {
            let mut model = bare_model(gp(1));
            model.read_before_write = writes;
            assert!(allocate_linear_scan(&graph, &live, &model).is_err());
        }
        let mut ref_graph = graph;
        ref_graph.nodes[input as usize].ty = IrType::Ref;
        let mut model = bare_model(gp(1));
        model.read_before_write = vec![valid];
        assert!(allocate_linear_scan(&ref_graph, &live, &model).is_err(), "register-only reference handoffs are not declared by the integer emitter");
    }

    #[test]
    fn non_overlapping_values_share_one_register() {
        let mut f = Fixture::new(12);
        let a = f.value(Op::Add, IrType::Int, 0, 3, &[3]);
        let b = f.value(Op::Add, IrType::Int, 5, 8, &[8]);
        let (graph, live) = f.finish();
        let model = bare_model(gp(2));

        let alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        assert_eq!(alloc.first_reg(a), Some(PhysReg::gp(0)));
        assert_eq!(
            alloc.first_reg(b),
            Some(PhysReg::gp(0)),
            "a dies at 3, so b may reuse r0 — that reuse is the whole point"
        );
        assert_eq!(alloc.promoted, 2);
        assert_eq!((alloc.spills, alloc.reloads), (0, 0));
        assert_eq!(alloc.stack_slots, 0, "neither value ever touches memory");
        verify_allocation(&graph, &live, &model, &alloc).expect("verifies");
    }

    #[test]
    fn overlapping_values_never_share_a_register() {
        let mut f = Fixture::new(12);
        let a = f.value(Op::Add, IrType::Int, 0, 8, &[8]);
        let b = f.value(Op::Add, IrType::Int, 2, 6, &[6]);
        // `c` starts exactly where `a` ends. Overlap is endpoint-inclusive —
        // a node's lowering may write its result before reading its operands —
        // so this must NOT reuse a's register either.
        let c = f.value(Op::Add, IrType::Int, 8, 10, &[10]);
        let (graph, live) = f.finish();
        let model = bare_model(gp(2));

        let alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        assert_eq!(alloc.first_reg(a), Some(PhysReg::gp(0)));
        assert_ne!(alloc.first_reg(b), alloc.first_reg(a));
        assert_ne!(
            alloc.first_reg(c),
            alloc.first_reg(a),
            "[0, 8] and [8, 10] touch at 8, which counts as overlapping"
        );
        assert_eq!(live.peak_live, 2);
        verify_allocation(&graph, &live, &model, &alloc).expect("verifies");
    }

    /// The default: a reference live across a safepoint gets no register,
    /// because the oop map names frame slots and a collector could neither see
    /// nor relocate one held in a register.
    #[test]
    fn a_reference_live_across_a_safepoint_gets_no_register_by_default() {
        let mut f = Fixture::new(12);
        let across = f.value(
            Op::Load(crate::ir::MemKind::Ref),
            IrType::Ref,
            0,
            8,
            &[3, 8],
        );
        let prim = f.value(Op::Add, IrType::Int, 0, 8, &[3, 8]);
        let (graph, live) = f.finish();
        let mut model = bare_model(gp(4));
        model.safepoints = vec![5];

        let alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        assert_eq!(
            alloc.first_reg(across),
            None,
            "a reference crossing a safepoint took a register the oop map cannot name"
        );
        assert!(
            alloc.first_reg(prim).is_some(),
            "the primitive must be promoted, or the assertion above is vacuous"
        );
        verify_allocation(&graph, &live, &model, &alloc).expect("verifies");
    }

    /// `refs_may_cross_safepoints` lifts exactly that refusal and nothing else.
    ///
    /// The caller that sets it — `ir_lower`, whose file is a write-through read
    /// cache over a slot the map DOES name — discharges the obligation by
    /// invalidating the cached copy at every point a collector could have run.
    /// See `ir_lower::invalidate_ref_residency`. This test's job is only to
    /// pin that the knob is what decides it, so that turning it on cannot
    /// become a silent property of some other change.
    #[test]
    fn refs_may_cross_safepoints_lifts_that_refusal_and_only_that_one() {
        let mut f = Fixture::new(12);
        let across = f.value(
            Op::Load(crate::ir::MemKind::Ref),
            IrType::Ref,
            0,
            8,
            &[3, 8],
        );
        let pinned = f.home_bound(IrType::Ref, 0, 8, &[3, 8]);
        let (graph, live) = f.finish();
        let mut model = bare_model(gp(4));
        model.safepoints = vec![5];
        model.refs_may_cross_safepoints = true;

        let alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        assert!(
            alloc.first_reg(across).is_some(),
            "the knob is on, so the reference must be allocatable"
        );
        assert_eq!(
            alloc.first_reg(pinned),
            None,
            "the knob lifts the SAFEPOINT refusal only — a home-bound value is \
             still refused for its own reason"
        );
        verify_allocation(&graph, &live, &model, &alloc).expect("verifies");
    }

    #[test]
    fn a_call_clobbers_the_caller_saved_registers() {
        let mut f = Fixture::new(12);
        let across = f.value(Op::Add, IrType::Int, 0, 8, &[3, 8]);
        let short = f.value(Op::Add, IrType::Int, 1, 3, &[3]);
        let (graph, live) = f.finish();
        let mut model = bare_model(gp_volatile_first(1, 1));
        model.clobbers = vec![(5, vec![PhysReg::gp(0)])];
        model.safepoints = vec![5];

        let alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        assert_eq!(
            alloc.first_reg(across),
            Some(PhysReg::gp(1)),
            "a value live across the call must land in the callee-saved register"
        );
        assert_eq!(
            alloc.first_reg(short),
            Some(PhysReg::gp(0)),
            "a value that dies before the call may still use the volatile one"
        );
        for seg in &alloc.segments[across as usize] {
            assert!(
                !(seg.reg == Some(PhysReg::gp(0)) && seg.range.contains(5)),
                "nothing may hold r0 at the call"
            );
        }
        assert_eq!(
            (alloc.spills, alloc.reloads),
            (0, 0),
            "the callee-saved register made the call cost nothing"
        );
        verify_allocation(&graph, &live, &model, &alloc).expect("verifies");
    }

    // ── The clobber-list invariant (R2/R3) ───────────────────────────

    #[test]
    fn add_clobbers_merges_into_a_position_that_is_already_recorded() {
        // The failing shape: `for_graph` records the caller-saved set at a call
        // position, then a backend adds the registers ITS OWN emission destroys
        // at the same position — which is what `for_graph`'s doc comment tells
        // backends to do. Pushing produces two entries at one position and
        // `clobbered_at`'s binary search then resolves to an arbitrary one.
        let mut model = bare_model(gp_volatile_first(2, 0));
        model.add_clobbers(5, &[PhysReg::gp(0)]);
        model.add_clobbers(5, &[PhysReg::gp(1)]);

        assert_eq!(model.clobbers.len(), 1, "one entry per position");
        assert_eq!(
            model.clobbered_at(5),
            &[PhysReg::gp(0), PhysReg::gp(1)][..],
            "BOTH producers' registers must be visible to the binary search"
        );
    }

    #[test]
    fn add_clobbers_keeps_the_list_ordered_and_drops_registers_outside_the_file() {
        let mut model = bare_model(gp_volatile_first(2, 0));
        model.add_clobbers(9, &[PhysReg::gp(1)]);
        model.add_clobbers(3, &[PhysReg::gp(0)]);
        // Not in the file: the allocator cannot hand r7 out, so a clobber of it
        // says nothing. `for_graph` drops its own out-of-file fixed-operand
        // rules the same way.
        model.add_clobbers(6, &[PhysReg::gp(7)]);

        let positions: Vec<usize> = model.clobbers.iter().map(|(p, _)| *p).collect();
        assert_eq!(positions, vec![3, 9]);
        assert!(model.clobbered_at(6).is_empty());
    }

    #[test]
    fn canonicalize_clobbers_merges_a_list_that_was_built_by_pushing() {
        let mut model = bare_model(gp_volatile_first(2, 0));
        model.clobbers = vec![
            (9, vec![PhysReg::gp(1)]),
            (5, vec![PhysReg::gp(1), PhysReg::gp(0)]),
            (5, vec![PhysReg::gp(1)]),
            (5, vec![PhysReg::gp(0)]),
        ];
        model.canonicalize_clobbers();

        assert_eq!(
            model.clobbers,
            vec![
                (5, vec![PhysReg::gp(0), PhysReg::gp(1)]),
                (9, vec![PhysReg::gp(1)]),
            ]
        );
        // Idempotent: running it on an already-canonical list changes nothing.
        let once = model.clobbers.clone();
        model.canonicalize_clobbers();
        assert_eq!(model.clobbers, once);
    }

    #[test]
    fn verify_allocation_rejects_a_duplicate_clobber_position() {
        // Sorted-but-not-unique used to pass: the verifier checked `w[0].0 >
        // w[1].0`, which a duplicate satisfies. That left the ONE guard on the
        // invariant blind to the ONE way it actually gets broken.
        let mut f = Fixture::new(12);
        let _across = f.value(Op::Add, IrType::Int, 0, 8, &[3, 8]);
        let (graph, live) = f.finish();
        let mut model = bare_model(gp_volatile_first(1, 1));
        model.add_clobbers(5, &[PhysReg::gp(0)]);
        model.safepoints = vec![5];

        let alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        verify_allocation(&graph, &live, &model, &alloc).expect("verifies as built");

        // Plant the duplicate a naive second producer would have created, in
        // sorted order so only the uniqueness half of the check can catch it.
        model.clobbers.push((5, vec![PhysReg::gp(1)]));
        model.clobbers.sort_by_key(|(pos, _)| *pos);
        let err = verify_allocation(&graph, &live, &model, &alloc)
            .expect_err("a duplicate position must be refused");
        assert_eq!(
            internal_message(&err),
            "regalloc: the clobber list is not strictly increasing by position"
        );
    }

    #[test]
    fn a_value_crossing_a_call_with_no_callee_saved_register_is_split_and_reloaded() {
        let mut f = Fixture::new(12);
        let v = f.value(Op::Add, IrType::Int, 0, 8, &[3, 8]);
        let (graph, live) = f.finish();
        // One register, and a call destroys it.
        let mut model = bare_model(gp_volatile_first(1, 0));
        model.clobbers = vec![(5, vec![PhysReg::gp(0)])];
        model.safepoints = vec![5];

        let alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        let segs = &alloc.segments[v as usize];
        assert_eq!(
            segs.len(),
            3,
            "prefix in r0, memory over the call, then back"
        );
        assert_eq!(
            segs[0],
            Segment {
                range: PosRange { lo: 0, hi: 4 },
                reg: Some(PhysReg::gp(0)),
            },
            "the register segment stops one short of the clobber, not on it"
        );
        assert_eq!(segs[1].reg, None);
        assert_eq!(
            segs[2],
            Segment {
                range: PosRange { lo: 8, hi: 8 },
                reg: Some(PhysReg::gp(0)),
            }
        );
        assert_eq!((alloc.spills, alloc.reloads, alloc.splits), (1, 1, 1));

        let store = alloc
            .events
            .iter()
            .find(|e| e.kind == SpillKind::Store)
            .expect("one store");
        assert_eq!(store.pos, 4, "the store may sink to the last live position");
        let load = alloc
            .events
            .iter()
            .find(|e| e.kind == SpillKind::Load)
            .expect("one reload");
        assert_eq!(
            load.pos, 8,
            "the reload lands on the next use, not on the split point"
        );
        assert_eq!(
            alloc.stack_slot[v as usize],
            Some(0),
            "a value that touches memory needs a home word"
        );
        verify_allocation(&graph, &live, &model, &alloc).expect("verifies");
    }

    #[test]
    fn a_rematerializable_constant_is_evicted_but_never_stored() {
        let mut f = Fixture::new(12);
        // The constant is long-lived and cheap to recompute; the arithmetic
        // value that wants the same single register is not.
        let k = f.value(Op::Const(7), IrType::Int, 0, 10, &[2, 10]);
        let x = f.value(Op::Add, IrType::Int, 1, 9, &[9]);
        let (graph, live) = f.finish();
        let model = bare_model(gp(1));

        let alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        assert_eq!(
            alloc.first_reg(x),
            Some(PhysReg::gp(0)),
            "the constant loses the register to the value that cannot be rebuilt"
        );
        assert_eq!(alloc.spills, 0, "a constant is never stored to memory");
        assert_eq!(alloc.reloads, 0, "and never loaded back from it");
        assert_eq!(alloc.remats, 1, "it is recomputed at its next use instead");
        assert!(
            !alloc.events.iter().any(|e| e.kind == SpillKind::Store),
            "no store may be emitted for a rematerializable value"
        );
        let remat = alloc
            .events
            .iter()
            .find(|e| e.kind == SpillKind::Remat)
            .expect("one remat");
        assert_eq!((remat.node, remat.pos), (k, 10));
        verify_allocation(&graph, &live, &model, &alloc).expect("verifies");
    }

    #[test]
    fn an_entry_parameter_keeps_its_abi_register() {
        let mut f = Fixture::new(10);
        let p = f.value(Op::Param(0), IrType::Int, 0, 8, &[8]);
        let q = f.value(Op::Add, IrType::Int, 1, 7, &[7]);
        let (graph, live) = f.finish();
        let mut model = bare_model(gp(3));
        model.fixed = vec![FixedConstraint {
            node: p,
            pos: 0,
            reg: PhysReg::gp(2),
        }];

        let alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        assert_eq!(
            alloc.first_reg(p),
            Some(PhysReg::gp(2)),
            "the pinned register wins over the preference order"
        );
        assert_ne!(alloc.first_reg(q), Some(PhysReg::gp(2)));
        verify_allocation(&graph, &live, &model, &alloc).expect("verifies");
    }

    // ── R6: the fixed / pre-coloured register path ───────────────────

    #[test]
    fn the_x86_64_file_recovers_the_five_fixed_operand_registers() {
        let full = RegFile::x86_64();
        let narrow = RegFile::x86_64_scratch_reserved();

        for (num, name) in [
            (0u8, "RAX"),
            (1, "RCX"),
            (2, "RDX"),
            (10, "R10"),
            (11, "R11"),
        ] {
            assert!(
                full.contains(PhysReg::gp(num)),
                "{name} is allocatable again now that the fixed-operand rules are \
                 modelled rather than dodged"
            );
            assert!(
                !narrow.contains(PhysReg::gp(num)),
                "{name} must stay out of the file a backend using it as private \
                 scratch asks for"
            );
        }
        // RSP and RBP are not registers this allocator may ever hand out, and
        // nothing about R6 changes that.
        assert!(!full.contains(PhysReg::gp(4)), "RSP");
        assert!(!full.contains(PhysReg::gp(5)), "RBP");
        assert_eq!(
            full.class_size(RegClass::Gp),
            narrow.class_size(RegClass::Gp) + 5,
            "exactly five registers came back"
        );
        assert_eq!(
            full.class_size(RegClass::Xmm),
            narrow.class_size(RegClass::Xmm),
            "R6 is a GP change; the XMM half is untouched"
        );
        for num in [0u8, 1, 2, 10, 11] {
            assert_eq!(
                full.is_caller_saved(PhysReg::gp(num)),
                Some(true),
                "every one of the five is volatile on both x86-64 ABIs"
            );
        }
    }

    /// `iload_0; iload_1; idiv` — the rule that has been unreachable since the
    /// constraint machinery was written.
    pub(crate) fn div_and_shift_graph() -> (Graph, ir_schedule::Schedule) {
        let code = [
            0x1a, 0x1b, 0x6c, 0x3d, // 0: iload_0; iload_1; idiv; istore_2
            0x1c, 0x1b, 0x78, 0x3e, // 4: iload_2; iload_1; ishl; istore_3
            0x1d, 0xac, // 8: iload_3; ireturn
            0, 0,
        ];
        let graph = IrBuilder::new(2, 4)
            .build(&code, 10)
            .expect("the divide-and-shift method builds");
        let schedule = ir_schedule::schedule(&graph);
        (graph, schedule)
    }

    /// The position of the first node matching `pred`.
    fn pos_of_op(
        graph: &Graph,
        live: &LiveModel,
        pred: impl Fn(&Op, IrType) -> bool,
    ) -> Option<(NodeId, usize)> {
        graph.nodes.iter().enumerate().find_map(|(id, node)| {
            if !pred(&node.op, node.ty) {
                return None;
            }
            live.pos_of
                .get(id)
                .copied()
                .flatten()
                .map(|pos| (id as NodeId, pos))
        })
    }

    #[test]
    fn an_integer_division_pins_its_dividend_to_rax_and_clobbers_rax_and_rdx() {
        let (graph, schedule) = div_and_shift_graph();
        let live = build_live_model(&graph, &schedule);
        let model = MachineModel::for_graph(&graph, &schedule, &live, RegFile::x86_64());

        let (div, pos) = pos_of_op(&graph, &live, |op, ty| {
            matches!(op, Op::Div) && matches!(ty, IrType::Int)
        })
        .expect("the method divides");
        let dividend = graph.nodes[div as usize].inputs.as_slice()[0];

        assert!(
            model.fixed.contains(&FixedConstraint {
                node: dividend,
                pos,
                reg: X86_RAX,
            }),
            "`idiv` reads its dividend in RDX:RAX, and only a FixedConstraint can \
             say so — a clobber cannot. fixed = {:?}",
            model.fixed
        );
        let destroyed = model.clobbered_at(pos);
        assert!(
            destroyed.contains(&X86_RAX) && destroyed.contains(&X86_RDX),
            "the quotient lands in RAX and the remainder in RDX, and the `cqo` \
             that feeds the divide destroys RDX before it runs: {destroyed:?}"
        );
        assert!(
            !model.fixed.iter().any(|f| f.node == div),
            "the RESULT is deliberately unpinned: it would want RAX at the same \
             position the dividend does, which is the unsatisfiable model the \
             allocator bails on"
        );

        let alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        verify_allocation(&graph, &live, &model, &alloc).expect("verifies");
    }

    #[test]
    fn a_variable_shift_pins_its_count_to_rcx_and_destroys_nothing() {
        let (graph, schedule) = div_and_shift_graph();
        let live = build_live_model(&graph, &schedule);
        let model = MachineModel::for_graph(&graph, &schedule, &live, RegFile::x86_64());

        let (shift, pos) =
            pos_of_op(&graph, &live, |op, _| matches!(op, Op::Shl)).expect("the method shifts");
        let count = graph.nodes[shift as usize].inputs.as_slice()[1];

        assert!(
            model.fixed.contains(&FixedConstraint {
                node: count,
                pos,
                reg: X86_RCX,
            }),
            "input 1 is the shift count and CL is the only place it can be read \
             from. fixed = {:?}",
            model.fixed
        );
        assert!(
            !model.clobbered_at(pos).contains(&X86_RCX),
            "`shl r/m, cl` READS CL and writes only r/m. Clobbering RCX here was \
             an over-approximation that emptied the register for no machine reason"
        );

        let alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        verify_allocation(&graph, &live, &model, &alloc).expect("verifies");
    }

    #[test]
    fn a_fixed_operand_rule_says_nothing_when_its_register_is_not_in_the_file() {
        let (graph, schedule) = div_and_shift_graph();
        let live = build_live_model(&graph, &schedule);
        // The file `ir_lower` allocates over: RAX/RCX/RDX are its own per-opcode
        // scratch and are not handed out, so the model it gets must be exactly
        // the one it got before R6.
        let model =
            MachineModel::for_graph(&graph, &schedule, &live, RegFile::x86_64_scratch_reserved());

        assert!(
            model.fixed.is_empty(),
            "a constraint naming a register the allocator cannot hand out is not \
             a constraint: {:?}",
            model.fixed
        );
        for (_, regs) in model.clobbers.iter() {
            for reg in regs {
                assert!(
                    *reg != X86_RAX && *reg != X86_RCX && *reg != X86_RDX,
                    "{reg} is outside this file and must not appear as a clobber"
                );
            }
        }
    }

    #[test]
    fn a_floating_point_division_carries_no_general_purpose_constraint() {
        // `dload_0; dload_2; ddiv; dreturn` — `divsd`, which reads and writes
        // one XMM and touches no GPR at all. The old clobber rule fired on
        // `Op::Div` regardless of type; that was invisible only because the
        // registers it named were filtered out on the next line.
        let code = [0x26, 0x28, 0x6f, 0xaf, 0, 0];
        // A builder refusal is not what is under test here: the assertion is
        // about `for_graph`'s type gate, and it is vacuous rather than wrong if
        // this shape never reaches it.
        // `build` answers `Option`, not `Result` -- a refusal carries no
        // reason out of the builder, it simply declines the method.
        // Two `double` parameters, typed: untyped, `dload` feeds an Int into
        // `ddiv`, which the operand type lane rejects.
        let mut builder = IrBuilder::new(2, 4);
        builder.set_param_types(&[IrType::Double, IrType::Double]);
        let Some(graph) = builder.build(&code, 4) else {
            return;
        };
        let schedule = ir_schedule::schedule(&graph);
        let live = build_live_model(&graph, &schedule);
        let model = MachineModel::for_graph(&graph, &schedule, &live, RegFile::x86_64());

        let Some((_, pos)) = pos_of_op(&graph, &live, |op, ty| {
            matches!(op, Op::Div) && matches!(ty, IrType::Double)
        }) else {
            // The builder declined this shape; there is nothing to assert and
            // nothing to be wrong about.
            return;
        };
        assert!(
            !model.fixed.iter().any(|f| f.pos == pos),
            "`divsd` has no fixed GP operand"
        );
        let destroyed = model.clobbered_at(pos);
        assert!(
            !destroyed.contains(&X86_RAX) && !destroyed.contains(&X86_RDX),
            "an FP divide destroys no GPR: {destroyed:?}"
        );
    }

    /// JIT round 19 wave 4 (lane irops3, IROPS2-3): over the WIDE file, every
    /// GP register the scalar-intrinsic arm writes is destroyed at the node's
    /// position -- `multiplyHigh`'s RCX (its `b`) included, which the wave-1
    /// clobber left out -- and the narrow file `ir_lower` allocates over still
    /// carries none of them.
    #[test]
    fn every_scalar_intrinsic_clobbers_the_gp_registers_its_arm_writes() {
        use crate::ir::ScalarOp as S;
        const ALL: &[S] = &[
            S::MinI,
            S::MaxI,
            S::MinL,
            S::MaxL,
            S::AbsI,
            S::AbsL,
            S::CompareI,
            S::CompareL,
            S::NlzI,
            S::NlzL,
            S::NtzI,
            S::NtzL,
            S::ReverseBytesI,
            S::ReverseBytesL,
            S::LowestOneBitI,
            S::LowestOneBitL,
            S::HighestOneBitI,
            S::HighestOneBitL,
            S::RotateLeftI,
            S::RotateLeftL,
            S::RotateRightI,
            S::RotateRightL,
            S::AbsF,
            S::AbsD,
            S::DoubleToRawLongBits,
            S::LongBitsToDouble,
            S::FloatToRawIntBits,
            S::IntBitsToFloat,
            S::SqrtD,
            S::MinF,
            S::MaxF,
            S::MinD,
            S::MaxD,
            S::MultiplyHighL,
            S::UnsignedMultiplyHighL,
            S::FloorD,
            S::CeilD,
            S::RintD,
            S::FmaD,
            S::FmaF,
            S::BitCountI,
            S::BitCountL,
            S::RoundD,
            S::RoundF,
        ];
        assert!(scalar_op_gp_writes(S::MultiplyHighL).contains(&X86_RCX));
        for &sop in ALL {
            let mut g = Graph::empty(0);
            let start = g.add(Op::Start, IrType::Control, vec![], None);
            g.entry = start;
            let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
            let inputs: Vec<NodeId> = (0..sop.arity())
                .map(|k| g.add(Op::Param(k as u16), sop.input_ty(k), vec![start], None))
                .collect();
            let v = g.add(Op::ScalarIntrinsic(sop), sop.result_type(), inputs, Some(0));
            g.exit = g.add(Op::Return, IrType::Void, vec![ctrl, v], Some(1));
            let schedule = ir_schedule::schedule(&g);
            let live = build_live_model(&g, &schedule);
            let Some((_, pos)) = pos_of_op(&g, &live, |op, _| *op == Op::ScalarIntrinsic(sop))
            else {
                panic!("{sop:?} has no position");
            };
            let wide = MachineModel::for_graph(&g, &schedule, &live, RegFile::x86_64());
            for reg in scalar_op_gp_writes(sop) {
                assert!(
                    wide.clobbered_at(pos).contains(reg),
                    "{sop:?} writes {reg} but the wide model keeps it live across"
                );
            }
            let narrow =
                MachineModel::for_graph(&g, &schedule, &live, RegFile::x86_64_scratch_reserved());
            assert!(
                narrow.clobbered_at(pos).is_empty(),
                "{sop:?}: the narrow file holds no register the arm writes"
            );
        }
    }

    #[test]
    fn a_dividend_keeps_its_pinned_register_through_the_clobber_that_ends_it() {
        // The early-clobber shape, in isolation: the value is REQUIRED in r0 at
        // position 4 and r0 is DESTROYED at position 4, because `idiv` reads RAX
        // and then writes it. It is also read again at 9, so the allocator has
        // to make a decision rather than getting the right answer by the value
        // being dead.
        let mut f = Fixture::new(12);
        let dividend = f.value(Op::Add, IrType::Int, 0, 9, &[4, 9]);
        let _other = f.value(Op::Add, IrType::Int, 1, 8, &[8]);
        let (graph, live) = f.finish();

        let mut model = bare_model(gp(2));
        model.fixed = vec![FixedConstraint {
            node: dividend,
            pos: 4,
            reg: PhysReg::gp(0),
        }];
        model.add_clobbers(4, &[PhysReg::gp(0), PhysReg::gp(1)]);

        let alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        assert_eq!(
            alloc.location_at(dividend, 4),
            Some(ValueLoc::Reg(PhysReg::gp(0))),
            "the dividend must reach the divide IN r0; a clobber at the same \
             position is the write half of the same instruction, not a conflict"
        );
        let holding = alloc.segments[dividend as usize]
            .iter()
            .find(|s| s.range.contains(4))
            .expect("a segment covers the constrained position");
        assert_eq!(
            (holding.range.lo, holding.range.hi),
            (0, 4),
            "and it must give the register up AT the clobber, not after it"
        );
        verify_allocation(&graph, &live, &model, &alloc)
            .expect("the verifier exempts exactly this segment");

        // The control. With no constraint the clobber is an ordinary one and
        // the value is in memory at 4 — which is what makes the assertion above
        // a statement about the constraint rather than about the fixture.
        let mut unpinned = bare_model(gp(2));
        unpinned.add_clobbers(4, &[PhysReg::gp(0), PhysReg::gp(1)]);
        let plain = allocate_linear_scan(&graph, &live, &unpinned).expect("allocates");
        assert!(
            !matches!(plain.location_at(dividend, 4), Some(ValueLoc::Reg(_))),
            "without the constraint nothing may hold a clobbered register at the \
             clobbering position"
        );
    }

    #[test]
    fn a_value_with_two_fixed_roles_is_split_rather_than_left_in_the_wrong_register() {
        // A value used as `idiv`'s dividend at 4 and as a shift's count at 8
        // requires two different registers inside one live range. Before R6 the
        // second constraint was invisible to `ls_select_register`, the value
        // kept r0 across position 8, and `verify_allocation`'s proof 4b rejected
        // the result as an INTERNAL error — costing the method its optimized
        // body over a shape that just needs a split.
        let mut f = Fixture::new(14);
        let v = f.value(Op::Add, IrType::Int, 0, 12, &[4, 8, 12]);
        let (graph, live) = f.finish();

        let mut model = bare_model(gp(3));
        model.fixed = vec![
            FixedConstraint {
                node: v,
                pos: 4,
                reg: PhysReg::gp(0),
            },
            FixedConstraint {
                node: v,
                pos: 8,
                reg: PhysReg::gp(1),
            },
        ];

        let alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        assert_eq!(
            alloc.location_at(v, 4),
            Some(ValueLoc::Reg(PhysReg::gp(0))),
            "the earliest constraint in the interval is the one the assignment honours"
        );
        assert!(
            !matches!(
                alloc.location_at(v, 8),
                Some(ValueLoc::Reg(PhysReg { num: 0, .. }))
            ),
            "and the value must be out of r0 by the position that wants r1"
        );
        verify_allocation(&graph, &live, &model, &alloc).expect("verifies");
    }

    #[test]
    fn a_clobber_between_definition_and_constraint_is_covered_by_a_split() {
        // r0 is destroyed at 2 and required at 6, so no single assignment covers
        // both. This used to be a hard `RegisterPressure` bailout on the reading
        // that "the value must be in that register and it is not" — which would
        // now refuse the optimized body of every pressured method containing a
        // division. The constraint's real promise is narrower (see
        // `FixedConstraint`): nobody ELSE is in r0 at 6, and this value is not
        // in some other register there. A home word satisfies both.
        let mut f = Fixture::new(10);
        let a = f.value(Op::Add, IrType::Int, 0, 8, &[6, 8]);
        let (graph, live) = f.finish();

        let mut model = bare_model(gp(1));
        model.fixed = vec![FixedConstraint {
            node: a,
            pos: 6,
            reg: PhysReg::gp(0),
        }];
        model.add_clobbers(2, &[PhysReg::gp(0)]);

        let alloc = allocate_linear_scan(&graph, &live, &model)
            .expect("a clobber before the constraint is not a failed compile");
        // The constraint IS honoured, and this assertion was inverted before.
        //
        // The comment above reasons that "no single assignment covers both" —
        // true, and it is the wrong unit. A linear-scan interval is not one
        // assignment: the value takes r0 from its definition, gives it up one
        // short of the clobber at 2, sits in its home word across it, and
        // takes r0 again at its next use — which is 6, the constrained
        // position. Splitting is exactly what makes a clobber between a
        // definition and a constraint survivable, so demanding a demotion here
        // asked the allocator for the worse of two correct answers.
        //
        // The demotion path is still there and still right for a constraint
        // that genuinely cannot be met. What it must never do is leave the
        // value in a DIFFERENT register across the constrained position: an
        // emitter told "this value is in r1" cannot know to load the fixed
        // operand from the home word instead. That is enforced where the
        // segment is cut, not here.
        assert_eq!(
            alloc.location_at(a, 6),
            Some(ValueLoc::Reg(PhysReg::gp(0))),
            "the split gives r0 back at the constrained position"
        );
        assert!(
            !matches!(alloc.location_at(a, 2), Some(ValueLoc::Reg(_))),
            "and the value is out of r0 across the clobber that destroys it"
        );
        verify_allocation(&graph, &live, &model, &alloc).expect("verifies");
    }

    // ── R12: what actually bounds the eviction path ──────────────────

    /// The guard in [`ls_pick_victim`] that makes the eviction path's re-queue a
    /// strict advance, pinned as behaviour.
    ///
    /// `far` is defined at 4 and not read again until 14; `near` is defined at
    /// the same position 4 and read constantly. By the eviction score alone
    /// `near` — high frequency weight, so a LOW score — should keep the register
    /// and `far` should lose it. It is the other way round here only because
    /// `near` is already active and starts at exactly the position `far` starts
    /// at, so `ls_pick_victim` refuses to nominate it: an interval cut at its own
    /// start has no prefix to keep and would be re-queued at its own unchanged
    /// start, which is the shape that would let this loop run for ever.
    #[test]
    fn ls_pick_victim_never_nominates_an_active_that_starts_here() {
        let mut f = Fixture::new(18);
        // Pops first among equal starts: the heap orders by `(lo, hi, node)`.
        let near = f.value(Op::Add, IrType::Int, 4, 6, &[6]);
        let far = f.value(
            Op::Add,
            IrType::Int,
            4,
            15,
            &[5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
        );
        let (graph, live) = f.finish();
        let model = bare_model(gp(1));

        // The score `far` would have won with: distance 1 over weight 11 is a
        // far worse victim score than `near`'s distance 2 over weight 1.
        assert!(live.weight[far as usize] > live.weight[near as usize]);

        let alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        assert_eq!(
            alloc.location_at(near, 4),
            Some(ValueLoc::Reg(PhysReg::gp(0))),
            "the active that starts here keeps its register however attractive a \
             victim it looks"
        );
        assert!(
            !matches!(alloc.location_at(far, 4), Some(ValueLoc::Reg(_))),
            "and the arriving interval is the one that goes to memory"
        );
        verify_allocation(&graph, &live, &model, &alloc).expect("verifies");
    }

    // ── A10 / R13: the AArch64 file ──────────────────────────────────

    #[test]
    fn the_aarch64_file_excludes_every_register_the_emitter_owns() {
        let regs = RegFile::aarch64(&[]);

        for (num, why) in [
            (0u8, "X0 is an argument register"),
            (7, "X7 is an argument register"),
            (8, "X8 is the indirect result location register"),
            (16, "X16 is IP0, and this backend's scratch four times over"),
            (17, "X17 is IP1"),
            (18, "X18 is the platform register"),
            (29, "X29 is FP"),
            (30, "X30 is LR"),
            (31, "encoding 31 is SP, which is also XZR"),
        ] {
            assert!(!regs.contains(PhysReg::gp(num)), "{why}");
        }
        for num in 19..=28u8 {
            assert!(regs.contains(PhysReg::gp(num)), "X{num} is callee-saved");
        }
        for num in 9..=15u8 {
            assert!(regs.contains(PhysReg::gp(num)), "X{num} is a temporary");
        }
        assert_eq!(regs.class_size(RegClass::Gp), 17);
        assert_eq!(regs.class_size(RegClass::Xmm), 8);
    }

    #[test]
    fn the_aarch64_fp_half_is_volatile_until_a_frame_saves_it() {
        // AAPCS64 makes D8–D15 callee-saved, but `aarch64_backend`'s prologue
        // saves only GPRs — so with no save area a value left in D8 across a
        // call is destroyed and the caller's copy is gone. That is the bug that
        // made this backend stop homing float locals in FP registers at all, and
        // the file must not re-introduce it by quoting the ABI at the emitter.
        let no_save_area = RegFile::aarch64(&[]);
        for num in ARM64_FP_LINEAR_SCAN {
            assert_eq!(
                no_save_area.is_caller_saved(PhysReg::xmm(num)),
                Some(true),
                "with no save area D{num} must split every interval that spans a call"
            );
        }

        let with_save_area = RegFile::aarch64(&ARM64_FP_LINEAR_SCAN);
        for num in ARM64_FP_LINEAR_SCAN {
            assert_eq!(
                with_save_area.is_caller_saved(PhysReg::xmm(num)),
                Some(false),
                "and once the frame gives D{num} back, a value may live across a call in it"
            );
        }

        // The GP half does not depend on the save area: a callee-saved GPR is
        // given back by the CALLEE.
        for (file, label) in [(&no_save_area, "no save area"), (&with_save_area, "saved")] {
            assert_eq!(
                file.is_caller_saved(PhysReg::gp(19)),
                Some(false),
                "X19 under {label}"
            );
            assert_eq!(
                file.is_caller_saved(PhysReg::gp(9)),
                Some(true),
                "X9 under {label}"
            );
        }
    }

    /// The AArch64 file must run the same allocator the x86-64 one does — that
    /// is what "the pass is not x86-specific" has to mean to be worth saying.
    #[test]
    fn a_scheduled_loop_allocates_over_the_aarch64_file_too() {
        let (graph, schedule) = swap_loop();
        let live = build_live_model(&graph, &schedule);
        let model = MachineModel::for_graph(&graph, &schedule, &live, RegFile::aarch64(&[]));

        assert!(
            model.fixed.is_empty(),
            "the fixed-operand rules are gated on their registers being in the \
             file, and an AArch64 file has none of them: {:?}",
            model.fixed
        );

        let alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        verify_allocation(&graph, &live, &model, &alloc).expect("verifies");
        for seg in alloc.segments.iter().flatten() {
            if let Some(reg) = seg.reg {
                assert!(model.regs.contains(reg), "{reg} is not in the AArch64 file");
            }
        }
    }

    // ── R7: the two frame colourings ─────────────────────────────────

    /// The finding that makes `NOTES-regalloc.md` §4's "assert they agree"
    /// option wrong rather than merely unimplemented.
    #[test]
    fn the_allocators_home_colouring_is_not_the_emitters_numbering() {
        let mut f = Fixture::new(14);
        // `resident` never touches memory: two registers, and it is the only
        // long-lived value, so it holds one for its whole range.
        let resident = f.value(Op::Add, IrType::Int, 0, 12, &[12]);
        // `spilled` is forced out by a clobber in the middle of its range.
        let spilled = f.value(Op::Add, IrType::Int, 1, 11, &[4, 11]);
        let (graph, live) = f.finish();
        let mut model = bare_model(gp(2));
        // Only r0. r1 survives the clobber, so `resident` — allocated first,
        // and the only value live at position 0 — takes it and never needs a
        // home word at all.
        model.add_clobbers(6, &[PhysReg::gp(0)]);

        let alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        assert!(
            !alloc.needs_home(resident),
            "a value that lives its whole life in a register needs no home word \
             — that frame-size win is the reason the two colourings differ"
        );
        assert!(alloc.needs_home(spilled));
        assert_eq!(
            alloc.stack_slot[resident as usize], None,
            "and so it consumes no COLOUR either, which shifts every colour after \
             it relative to a colouring that gives every value a word"
        );
        assert_eq!(
            alloc.stack_slot[spilled as usize],
            Some(0),
            "the spilled value takes word 0, not the word a plan_slots-style \
             colouring — which would have spent word 0 on `resident` — would give it"
        );
    }

    #[test]
    fn adopting_the_emitters_frame_makes_every_slot_name_a_word_it_can_address() {
        let mut f = Fixture::new(14);
        let resident = f.value(Op::Add, IrType::Int, 0, 12, &[12]);
        let spilled = f.value(Op::Add, IrType::Int, 1, 11, &[4, 11]);
        let (graph, live) = f.finish();
        let mut model = bare_model(gp(2));
        // Only r0. r1 survives the clobber, so `resident` — allocated first,
        // and the only value live at position 0 — takes it and never needs a
        // home word at all.
        model.add_clobbers(6, &[PhysReg::gp(0)]);
        let mut alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");

        // What a write-through consumer's own colourer produces: a word for
        // EVERY value, in its own numbering.
        let emitter = vec![Some(0u32), Some(1u32)];
        alloc
            .adopt_home_colouring(&emitter, 2)
            .expect("the emitter's frame has a word for everything that needs one");

        assert_eq!(alloc.stack_slot, emitter);
        assert_eq!(alloc.stack_slots, 2);
        assert_eq!(
            alloc.location_at(spilled, 6),
            Some(ValueLoc::Slot(1)),
            "a Slot payload now names the emitter's word, not the allocator's"
        );
        assert!(matches!(
            alloc.location_at(resident, 6),
            Some(ValueLoc::Reg(_))
        ));
        verify_allocation(&graph, &live, &model, &alloc)
            .expect("and proof 6 is now a statement about the frame that is emitted");
    }

    #[test]
    fn adopting_a_frame_with_no_word_for_a_spilled_value_is_refused() {
        let mut f = Fixture::new(14);
        let _resident = f.value(Op::Add, IrType::Int, 0, 12, &[12]);
        let _spilled = f.value(Op::Add, IrType::Int, 1, 11, &[4, 11]);
        let (graph, live) = f.finish();
        let mut model = bare_model(gp(2));
        // Only r0. r1 survives the clobber, so `resident` — allocated first,
        // and the only value live at position 0 — takes it and never needs a
        // home word at all.
        model.add_clobbers(6, &[PhysReg::gp(0)]);
        let mut alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");

        // No word for the value that spends positions 6..=10 in memory. Silently
        // accepting this would make `location_at` answer `None` at a position
        // inside that value's own live range.
        let err = alloc
            .adopt_home_colouring(&[Some(0), None], 1)
            .expect_err("a value with nowhere to spill to must be refused");
        assert!(
            internal_message(&err).contains("no home word"),
            "wrong diagnosis: {err:?}"
        );

        let err = alloc
            .adopt_home_colouring(&[Some(0)], 1)
            .expect_err("a colouring that is not sized for the graph must be refused");
        assert!(
            internal_message(&err).contains("sized for the graph"),
            "wrong diagnosis: {err:?}"
        );
    }

    // ── R10: loop depth from the dominators ──────────────────────────

    /// A CFG whose block *order* is not a reverse postorder, which is the state
    /// `ir_schedule::layout_blocks` leaves every method in.
    ///
    /// ```text
    ///   0 ──▶ 1 ──▶ 2 ──▶ 4          1 is the loop header
    ///         │╰────▶ 3 ─╯(back)     3 is the only body block
    ///         ╰───────────┘          2 is a loop EXIT, laid out inside the
    ///                                index window [1, 3]
    /// ```
    ///
    /// The RPO of this graph is `0, 1, 3, 2, 4`; the layout here is `0..=4`.
    /// Block 2 therefore sits between the header and the back-edge source
    /// without being in the loop.
    fn non_rpo_loop_schedule() -> ir_schedule::Schedule {
        let edges: [(Vec<usize>, Vec<usize>); 5] = [
            (vec![1], vec![]),        // 0: entry
            (vec![2, 3], vec![0, 3]), // 1: loop header
            (vec![4], vec![1]),       // 2: loop EXIT, not in the loop
            (vec![1], vec![1]),       // 3: loop body, back edge to 1
            (vec![], vec![2]),        // 4: exit
        ];
        let blocks: Vec<ir_schedule::Block> = edges
            .into_iter()
            .enumerate()
            .map(|(id, (successors, predecessors))| ir_schedule::Block {
                id,
                ctrl: NO_NODE,
                nodes: Vec::new(),
                terminator: None,
                successors,
                predecessors,
            })
            .collect();
        let dom = ir_schedule::Dominators::compute(&blocks);
        ir_schedule::Schedule {
            blocks,
            node_to_block: Vec::new(),
            dom,
            freq: Default::default(),
            layout: Default::default(),
            pairing: Default::default(),
        }
    }

    #[test]
    fn loop_depth_comes_from_the_dominators_and_not_from_block_order() {
        let schedule = non_rpo_loop_schedule();
        assert!(
            schedule.dom.dominates(1, 3),
            "3 -> 1 is a back edge because its target dominates its source"
        );

        let depths = ls_loop_depths(&schedule);
        assert_eq!(
            depths,
            vec![0, 1, 0, 1, 0],
            "the natural loop of 3 -> 1 is {{1, 3}}. The old model declared every \
             block in the INDEX WINDOW [1, 3] one level deeper, which charges \
             block 2 — a loop exit — ten times the frequency it earns, and with \
             it ten times the spill cost"
        );
    }

    /// **One loop-depth model (round 11 wave 3).** `ls_loop_depths` used to be
    /// a second copy of `ir_schedule::loop_depths`, and a doc-comment scan was
    /// the only test tying the two together. It now delegates, so the pin is
    /// behavioural: on a schedule whose dominators match its blocks the two
    /// answer identically, and on one whose dominators do not, this side
    /// answers all zeros rather than asking the scheduler's copy to index a
    /// relation sized for another CFG.
    #[test]
    fn the_allocator_reads_the_schedulers_loop_depths() {
        for schedule in [non_rpo_loop_schedule(), swap_loop().1] {
            assert_eq!(
                ls_loop_depths(&schedule),
                ir_schedule::loop_depths(&schedule.blocks, &schedule.dom)
            );
        }
        let mut mis_sized = non_rpo_loop_schedule();
        mis_sized.dom = ir_schedule::Dominators::compute(&[]);
        assert_eq!(
            ls_loop_depths(&mis_sized),
            vec![0u32; mis_sized.blocks.len()]
        );
    }

    #[test]
    fn a_real_loop_still_reads_as_one_level_deep() {
        // The dominator model must not have made every depth zero: the whole
        // point of `weight` is that a value used inside a loop outranks one used
        // outside it.
        let (graph, schedule) = swap_loop();
        let live = build_live_model(&graph, &schedule);
        assert!(
            live.loop_depth.iter().any(|&d| d >= 1),
            "a method with a back edge must have a block inside a loop: {:?}",
            live.loop_depth
        );
        assert!(
            live.loop_depth.iter().all(|&d| d <= 1),
            "and a singly-nested loop must not read as doubly nested: {:?}",
            live.loop_depth
        );
    }

    // ── The reference-at-a-safepoint decision ────────────────────────

    #[test]
    fn a_reference_live_across_a_safepoint_stays_in_memory() {
        let mut f = Fixture::new(12);
        let across = f.value(Op::Load(crate::ir::MemKind::Ref), IrType::Ref, 0, 8, &[8]);
        let between = f.value(Op::Load(crate::ir::MemKind::Ref), IrType::Ref, 5, 7, &[7]);
        let prim = f.value(Op::Add, IrType::Int, 0, 8, &[8]);
        let (graph, live) = f.finish();
        let mut model = bare_model(gp(4));
        model.safepoints = vec![4];

        let alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        assert!(
            !alloc.is_promoted(across),
            "the oop map describes frame words, so a reference that is live at a \
             safepoint must be in one"
        );
        assert!(
            alloc.stack_slot[across as usize].is_some(),
            "and it must therefore have a home word"
        );
        assert!(
            alloc.is_promoted(between),
            "a reference that lives and dies strictly after the safepoint may take \
             a register"
        );
        assert!(
            alloc.is_promoted(prim),
            "the rule is about references, not about crossing a safepoint"
        );
        verify_allocation(&graph, &live, &model, &alloc).expect("verifies");
    }

    // ── The verifier ─────────────────────────────────────────────────

    #[test]
    fn verify_allocation_rejects_a_planted_register_alias() {
        let mut f = Fixture::new(12);
        let a = f.value(Op::Add, IrType::Int, 0, 8, &[8]);
        let b = f.value(Op::Add, IrType::Int, 2, 6, &[6]);
        let (graph, live) = f.finish();
        let model = bare_model(gp(2));
        let mut alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        assert_ne!(alloc.first_reg(a), alloc.first_reg(b));

        // Two values live at the same position, one register.
        let a_reg = alloc.first_reg(a);
        alloc.segments[b as usize][0].reg = a_reg;
        let err = verify_allocation(&graph, &live, &model, &alloc)
            .expect_err("an alias must be rejected");
        assert!(
            internal_message(&err).contains("one register"),
            "wrong diagnosis: {err:?}"
        );
    }

    #[test]
    fn verify_allocation_rejects_a_register_class_violation() {
        let mut f = Fixture::new(12);
        let a = f.value(Op::Add, IrType::Int, 0, 8, &[8]);
        let (graph, live) = f.finish();
        let model = bare_model(RegFile::from_specs([
            RegSpec {
                reg: PhysReg::gp(0),
                caller_saved: false,
            },
            RegSpec {
                reg: PhysReg::xmm(0),
                caller_saved: false,
            },
        ]));
        let mut alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        assert_eq!(alloc.first_reg(a), Some(PhysReg::gp(0)));

        // An integer parked in an SSE register: allocatable, but the wrong bank.
        alloc.segments[a as usize][0].reg = Some(PhysReg::xmm(0));
        let err = verify_allocation(&graph, &live, &model, &alloc)
            .expect_err("a class violation must be rejected");
        assert!(
            internal_message(&err).contains("wrong register class"),
            "wrong diagnosis: {err:?}"
        );
    }

    #[test]
    fn verify_allocation_rejects_an_abi_violation() {
        let mut f = Fixture::new(12);
        let a = f.value(Op::Param(0), IrType::Int, 0, 8, &[8]);
        let b = f.value(Op::Add, IrType::Int, 2, 6, &[6]);
        // Home-bound: `ir_lower` owns this one's location, so it is never
        // promoted and the pin below can only be about who else is in the way.
        let homed = f.home_bound(IrType::Int, 0, 8, &[8]);
        let (graph, live) = f.finish();
        let model = bare_model(gp(2));
        let alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        assert_eq!(alloc.first_reg(a), Some(PhysReg::gp(0)));
        assert_eq!(alloc.first_reg(b), Some(PhysReg::gp(1)));
        assert!(!alloc.is_promoted(homed));

        // (a) the pinned value itself is in the wrong register.
        let mut wrong_reg = bare_model(gp(2));
        wrong_reg.fixed = vec![FixedConstraint {
            node: a,
            pos: 0,
            reg: PhysReg::gp(1),
        }];
        let err = verify_allocation(&graph, &live, &wrong_reg, &alloc)
            .expect_err("a misplaced ABI value must be rejected");
        assert!(
            internal_message(&err).contains("ABI-pinned value"),
            "wrong diagnosis: {err:?}"
        );

        // (b) somebody else is occupying the register the ABI reserved.
        let mut occupied = bare_model(gp(2));
        occupied.fixed = vec![FixedConstraint {
            node: homed,
            pos: 4,
            reg: PhysReg::gp(1),
        }];
        let err = verify_allocation(&graph, &live, &occupied, &alloc)
            .expect_err("an occupied ABI register must be rejected");
        assert!(
            internal_message(&err).contains("ABI-pinned register"),
            "wrong diagnosis: {err:?}"
        );
    }

    #[test]
    fn verify_allocation_rejects_a_reference_in_a_register_at_a_safepoint() {
        let mut f = Fixture::new(12);
        let r = f.value(Op::Load(crate::ir::MemKind::Ref), IrType::Ref, 0, 8, &[8]);
        let (graph, live) = f.finish();
        let mut model = bare_model(gp(2));
        model.safepoints = vec![4];
        let mut alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        assert!(!alloc.is_promoted(r));

        // The exact shape that produced a moving-GC use-after-free before: a
        // live oop in a location the collector's frame walker cannot see.
        alloc.segments[r as usize] = vec![Segment {
            range: PosRange { lo: 0, hi: 8 },
            reg: Some(PhysReg::gp(1)),
        }];
        let err = verify_allocation(&graph, &live, &model, &alloc)
            .expect_err("an invisible oop must be rejected");
        assert!(
            internal_message(&err).contains("across a safepoint"),
            "wrong diagnosis: {err:?}"
        );
    }

    #[test]
    fn verify_allocation_rejects_a_timeline_gap() {
        let mut f = Fixture::new(12);
        let a = f.value(Op::Add, IrType::Int, 0, 8, &[8]);
        let (graph, live) = f.finish();
        let model = bare_model(gp(2));
        let mut alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");

        alloc.segments[a as usize][0].range.hi = 4;
        let err = verify_allocation(&graph, &live, &model, &alloc)
            .expect_err("an uncovered position must be rejected");
        assert!(
            internal_message(&err).contains("live range"),
            "wrong diagnosis: {err:?}"
        );
    }

    // ── Bailing rather than guessing ─────────────────────────────────

    #[test]
    fn an_unsatisfiable_fixed_constraint_bails_with_register_pressure() {
        let mut f = Fixture::new(8);
        let a = f.value(Op::Param(0), IrType::Int, 0, 6, &[6]);
        let b = f.value(Op::Param(1), IrType::Int, 0, 6, &[6]);
        let (graph, live) = f.finish();
        let mut model = bare_model(gp(2));
        model.fixed = vec![
            FixedConstraint {
                node: a,
                pos: 0,
                reg: PhysReg::gp(0),
            },
            FixedConstraint {
                node: b,
                pos: 0,
                reg: PhysReg::gp(0),
            },
        ];

        let err = allocate_linear_scan(&graph, &live, &model)
            .expect_err("two values cannot share one register at one position");
        assert!(
            matches!(err.reason, BailoutReason::RegisterPressure),
            "{err:?}"
        );
    }

    #[test]
    fn an_over_pressure_graph_bails_with_register_pressure() {
        // Four long-lived values, each used every fourth position, over a single
        // register: every promotion immediately costs a reload, which is the
        // case linear scan must decline rather than churn through.
        const LAST: usize = 120;
        let mut f = Fixture::new(LAST + 1);
        for i in 0..4usize {
            let uses: Vec<usize> = ((i + 4)..=LAST).step_by(4).collect();
            f.value(Op::Add, IrType::Int, i, LAST, &uses);
        }
        let (graph, live) = f.finish();
        let model = bare_model(gp(1));

        let err = allocate_linear_scan(&graph, &live, &model)
            .expect_err("one register cannot serve four live values");
        assert!(
            matches!(err.reason, BailoutReason::RegisterPressure),
            "{err:?}"
        );
        assert_eq!(live.peak_live, 4, "the report must carry the pressure");
    }

    // ── Parallel copy ────────────────────────────────────────────────

    fn slot(n: u32) -> ValueLoc {
        ValueLoc::Slot(n)
    }

    /// Apply a *sequentialised* copy and return the resulting location→value
    /// map, so a test can compare it against the parallel semantics.
    fn run_copies(start: &[(ValueLoc, u32)], ops: &[CopyOp]) -> HashMap<ValueLoc, u32> {
        let mut state: HashMap<ValueLoc, u32> = start.iter().copied().collect();
        let mut scratch: Option<u32> = None;
        for op in ops {
            match *op {
                CopyOp::Move { from, to } => {
                    let v = match state.get(&from) {
                        Some(&v) => v,
                        None => panic!("read of an uninitialised location {from:?}"),
                    };
                    state.insert(to, v);
                }
                CopyOp::Save { from } => {
                    scratch = state.get(&from).copied();
                }
                CopyOp::Restore { to } => {
                    let v = match scratch {
                        Some(v) => v,
                        None => panic!("restore with nothing saved"),
                    };
                    state.insert(to, v);
                }
            }
        }
        state
    }

    /// The semantics a phi web has: every destination receives its source's
    /// value as it was *before* any copy ran.
    fn parallel_result(
        start: &[(ValueLoc, u32)],
        copies: &[(ValueLoc, ValueLoc)],
    ) -> HashMap<ValueLoc, u32> {
        let before: HashMap<ValueLoc, u32> = start.iter().copied().collect();
        let mut after = before.clone();
        for &(dst, src) in copies {
            match before.get(&src) {
                Some(&v) => {
                    after.insert(dst, v);
                }
                None => panic!("the fixture does not initialise {src:?}"),
            }
        }
        after
    }

    /// Give every location the copy mentions a distinct starting value.
    fn seed(copies: &[(ValueLoc, ValueLoc)]) -> Vec<(ValueLoc, u32)> {
        let mut start: Vec<(ValueLoc, u32)> = Vec::new();
        for &(dst, src) in copies {
            for loc in [dst, src] {
                if !start.iter().any(|(l, _)| *l == loc) {
                    let next = 1000 + start.len() as u32;
                    start.push((loc, next));
                }
            }
        }
        start
    }

    fn check_parallel(copies: &[(ValueLoc, ValueLoc)]) -> Vec<CopyOp> {
        let ops = resolve_parallel_copy(copies).expect("sequentialises");
        let start = seed(copies);
        assert_eq!(
            run_copies(&start, &ops),
            parallel_result(&start, copies),
            "sequentialised copy disagrees with the parallel one: {ops:?}"
        );
        ops
    }

    #[test]
    fn a_copy_chain_writes_the_readers_first() {
        // Naive order would emit `s1 ← s0` first and then read the overwritten
        // s1 into s2.
        let copies = [(slot(1), slot(0)), (slot(2), slot(1))];
        let ops = check_parallel(&copies);
        assert_eq!(
            ops,
            vec![
                CopyOp::Move {
                    from: slot(1),
                    to: slot(2),
                },
                CopyOp::Move {
                    from: slot(0),
                    to: slot(1),
                },
            ],
            "no scratch is needed for an acyclic web"
        );
    }

    #[test]
    fn a_two_element_phi_cycle_uses_the_scratch() {
        // The swap loop's back edge: each phi's incoming value is the other phi.
        let copies = [(slot(0), slot(1)), (slot(1), slot(0))];
        let ops = check_parallel(&copies);
        assert_eq!(
            ops.iter()
                .filter(|o| matches!(o, CopyOp::Save { .. }))
                .count(),
            1,
            "exactly one save breaks a single cycle: {ops:?}"
        );
        assert_eq!(
            ops.iter()
                .filter(|o| matches!(o, CopyOp::Restore { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn a_three_element_phi_cycle_resolves() {
        let copies = [(slot(0), slot(1)), (slot(1), slot(2)), (slot(2), slot(0))];
        let ops = check_parallel(&copies);
        assert_eq!(
            ops.len(),
            4,
            "one save, two moves, one restore — the third move IS the restore: {ops:?}"
        );
    }

    #[test]
    fn a_cycle_with_a_fan_out_resolves() {
        // Two phis swap, a third reads one of them, and a register copy hangs
        // off the other — a shape a real loop header produces.
        let copies = [
            (slot(0), slot(1)),
            (slot(1), slot(0)),
            (slot(2), slot(0)),
            (ValueLoc::Reg(PhysReg::gp(3)), slot(1)),
        ];
        check_parallel(&copies);
    }

    #[test]
    fn two_disjoint_cycles_resolve() {
        let copies = [
            (slot(0), slot(1)),
            (slot(1), slot(0)),
            (slot(2), slot(3)),
            (slot(3), slot(2)),
        ];
        check_parallel(&copies);
    }

    #[test]
    fn a_self_copy_is_dropped() {
        let ops = resolve_parallel_copy(&[(slot(0), slot(0))]).expect("resolves");
        assert!(ops.is_empty(), "copying a location to itself emits nothing");
    }

    #[test]
    fn a_parallel_copy_with_a_duplicate_destination_is_rejected() {
        let err = resolve_parallel_copy(&[(slot(0), slot(1)), (slot(0), slot(2))])
            .expect_err("one destination, two sources is not a copy");
        assert!(
            internal_message(&err).contains("destination twice"),
            "wrong diagnosis: {err:?}"
        );
    }

    /// Round 11 wave 8: `resolve_parallel_copy` keeps reader counts instead of
    /// rescanning `pending`. The emitted sequence must be the one the
    /// rescanning version chose, op for op, so `ir_lower`'s φ-edge code is
    /// byte-identical; this is that version, run against pseudo-random webs
    /// (chains, fan-out, several cycles, self-copies, repeated pairs).
    #[test]
    fn r11w8_parallel_copy_matches_the_rescanning_reference() {
        fn reference(copies: &[(ValueLoc, ValueLoc)]) -> Vec<CopyOp> {
            let mut pending: Vec<(ValueLoc, Option<ValueLoc>)> = Vec::new();
            for &(dst, src) in copies {
                if dst == src {
                    continue;
                }
                match pending.iter().find(|(d, _)| *d == dst).map(|(_, s)| *s) {
                    Some(_) => continue,
                    None => pending.push((dst, Some(src))),
                }
            }
            let mut out = Vec::new();
            while !pending.is_empty() {
                let ready = pending
                    .iter()
                    .position(|(d, _)| !pending.iter().any(|(_, s)| *s == Some(*d)));
                match ready {
                    Some(i) => {
                        let (dst, src) = pending.remove(i);
                        out.push(match src {
                            Some(from) => CopyOp::Move { from, to: dst },
                            None => CopyOp::Restore { to: dst },
                        });
                    }
                    None => {
                        let (cycle, _) = pending[0];
                        out.push(CopyOp::Save { from: cycle });
                        for (_, src) in pending.iter_mut() {
                            if *src == Some(cycle) {
                                *src = None;
                            }
                        }
                    }
                }
            }
            out
        }

        let loc = |k: u64| -> ValueLoc {
            if k % 5 == 4 {
                ValueLoc::Reg(PhysReg::gp((k % 7) as u8))
            } else {
                slot((k % 9) as u32)
            }
        };
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = || {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            state >> 33
        };
        let mut cyclic = 0usize;
        for _ in 0..500 {
            let len = (next() % 12) as usize;
            let mut copies: Vec<(ValueLoc, ValueLoc)> = Vec::new();
            for _ in 0..len {
                let dst = loc(next());
                let src = loc(next());
                // Keep the web well formed: one source per destination, but
                // let an identical pair repeat.
                match copies.iter().find(|(d, _)| *d == dst) {
                    Some(&(_, s)) if s != src => continue,
                    _ => copies.push((dst, src)),
                }
            }
            let ops = check_parallel(&copies);
            if ops.iter().any(|o| matches!(o, CopyOp::Save { .. })) {
                cyclic += 1;
            }
            assert_eq!(ops, reference(&copies), "copies: {copies:?}");
        }
        assert!(cyclic > 0, "the generator must produce some cycles");

        // Round 11 wave 9: the ready set is now a min-heap. Wide webs (a
        // switch join's φ count) with many interleaved cycles, chains that
        // feed cycles and fan-out, still op-for-op the reference.
        let mut wide_cyclic = 0usize;
        // (The reference is cubic in the width, so the widths stay modest.)
        for round in 0..40u32 {
            let width = 40 + round * 2;
            let mut copies: Vec<(ValueLoc, ValueLoc)> = Vec::new();
            for d in 0..width {
                let src = match next() % 4 {
                    // a rotation of the whole web: long cycles
                    0 => slot((d + 1) % width),
                    // pairwise swaps
                    1 => slot(d ^ 1),
                    // a chain into a location outside the web
                    2 => slot(width + (next() % 8) as u32),
                    // fan-out from a random member
                    _ => slot((next() % u64::from(width)) as u32),
                };
                copies.push((slot(d), src));
            }
            let ops = check_parallel(&copies);
            if ops.iter().any(|o| matches!(o, CopyOp::Save { .. })) {
                wide_cyclic += 1;
            }
            assert_eq!(ops, reference(&copies), "width {width}");
        }
        assert!(wide_cyclic > 0, "the wide webs must contain cycles");
    }

    /// Round 11 wave 9: `ClobberIndex::first_in` answers exactly what the walk
    /// it replaced in `ls_select_register` and proof 4 answered — the first
    /// clobber of `reg` in `[lo, hi]`, `hi` excluded when exempt — on every
    /// canonical clobber list.
    #[test]
    fn r11w9_clobber_index_matches_the_clobber_walk() {
        fn walk(
            clobbers: &[(usize, Vec<PhysReg>)],
            reg: PhysReg,
            lo: usize,
            hi: usize,
            exempt_hi: bool,
        ) -> Option<usize> {
            let start = clobbers.partition_point(|(p, _)| *p < lo);
            for (pos, regs) in &clobbers[start..] {
                if *pos > hi {
                    break;
                }
                if exempt_hi && *pos == hi {
                    continue;
                }
                if regs.contains(&reg) {
                    return Some(*pos);
                }
            }
            None
        }
        let regs = [
            PhysReg::gp(0),
            PhysReg::gp(1),
            PhysReg::gp(2),
            PhysReg::xmm(0),
            PhysReg::xmm(1),
        ];
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..200 {
            let mut model = MachineModel::default();
            let mut pos = 0usize;
            for _ in 0..(next() % 20) {
                pos += 1 + (next() % 4) as usize;
                let mut at: Vec<PhysReg> =
                    regs.iter().copied().filter(|_| next() % 3 == 0).collect();
                if at.is_empty() {
                    continue;
                }
                at.sort_unstable();
                model.clobbers.push((pos, at));
            }
            let index = ClobberIndex::of(&model);
            for _ in 0..40 {
                let lo = (next() % 90) as usize;
                let hi = lo + (next() % 30) as usize;
                let reg = regs[(next() % regs.len() as u64) as usize];
                for exempt in [false, true] {
                    assert_eq!(
                        index.first_in(reg, lo, hi, exempt),
                        walk(&model.clobbers, reg, lo, hi, exempt),
                        "{reg} over [{lo}, {hi}] exempt={exempt}: {:?}",
                        model.clobbers
                    );
                }
            }
        }
    }

    /// Round 11 wave 9: `width` disjoint swaps. Every one is a cycle, so the
    /// breaker runs `width` times; each break must touch only its own
    /// cycle's reader (the `reads` index), and every save is restored before
    /// the next save reuses the scratch.
    #[test]
    fn r11w9_many_disjoint_swaps_pair_each_save_with_its_restore() {
        let width = 400u32;
        let mut copies: Vec<(ValueLoc, ValueLoc)> = Vec::new();
        for k in 0..width / 2 {
            copies.push((slot(2 * k), slot(2 * k + 1)));
            copies.push((slot(2 * k + 1), slot(2 * k)));
        }
        let ops = check_parallel(&copies);
        let mut open = false;
        for op in &ops {
            match op {
                CopyOp::Save { .. } => {
                    assert!(!open, "a save before the previous restore: {ops:?}");
                    open = true;
                }
                CopyOp::Restore { .. } => {
                    assert!(open, "a restore with no save: {ops:?}");
                    open = false;
                }
                CopyOp::Move { .. } => {}
            }
        }
        assert!(!open);
        assert_eq!(
            ops.len(),
            (width / 2 * 3) as usize,
            "save + move + restore per swap"
        );
    }

    // ── End to end, on a real scheduled graph ────────────────────────

    /// `int f(int n) { int a=1, b=2; for (int i=0; i<n; i++) swap(a, b); return a; }`
    ///
    /// The swap is done on the operand stack (`iload_1; iload_2; istore_1;
    /// istore_2`), so it needs no temporary local — which makes the loop
    /// header's two phis each other's back-edge value, i.e. a copy cycle.
    pub(crate) fn swap_loop() -> (Graph, ir_schedule::Schedule) {
        let code = [
            0x04, 0x3c, // 0: iconst_1; istore_1        a = 1
            0x05, 0x3d, // 2: iconst_2; istore_2        b = 2
            0x03, 0x3e, // 4: iconst_0; istore_3        i = 0
            0x1d, 0x1a, // 6: iload_3; iload_0          loop header
            0xa2, 0x00, 0x0d, // 8: if_icmpge 21
            0x1b, 0x1c, // 11: iload_1; iload_2         push a, then b
            0x3c, 0x3d, // 13: istore_1; istore_2       a = b; b = old a
            0x84, 0x03, 0x01, // 15: iinc 3, 1
            0xa7, 0xff, 0xf4, // 18: goto 6
            0x1b, 0xac, // 21: iload_1; ireturn
            0, 0,
        ];
        let graph = IrBuilder::new(1, 4)
            .build(&code, 23)
            .expect("the swap loop builds");
        let schedule = ir_schedule::schedule(&graph);
        (graph, schedule)
    }



    /// `static int f(int n){ int s=0; for(int i=0;i<n;i++) for(int j=0;j<n;j++) s+=j; return s; }`
    /// with the i58-L2 proposal's shape added by hand: a volatile
    /// `getstatic stop` (`Int`) in the outer body, and a `getstatic burnt` in
    /// the inner header that names it ONLY as its memory token. Answers the
    /// graph, its schedule, the inner header block and `stop`.
    pub(crate) fn nested_loop_with_a_token_read() -> (Graph, ir_schedule::Schedule, usize, NodeId) {
        let code: [u8; 36] = [
            0x03, 0x3c, 0x03, 0x3d, 0x1c, 0x1a, 0xa2, 0x00, 0x1a, 0x03, 0x3e, 0x1d, 0x1a, 0xa2,
            0x00, 0x0d, 0x1b, 0x1d, 0x60, 0x3c, 0x84, 0x03, 0x01, 0xa7, 0xff, 0xf4, 0x84, 0x02,
            0x01, 0xa7, 0xff, 0xe7, 0x1b, 0xac, 0, 0,
        ];
        let mut graph = IrBuilder::new(1, 4)
            .build(&code, 34)
            .expect("the nested loop builds");
        let mut schedule = ir_schedule::schedule(&graph);
        let inner = schedule
            .blocks
            .iter()
            .position(|b| {
                let ctrl = &graph.nodes[b.ctrl as usize];
                ctrl.bytecode_pc == Some(11) && matches!(ctrl.op, Op::Merge | Op::Region)
            })
            .expect("the inner header is a block");
        let outer_body = schedule
            .dom
            .idom(inner)
            .expect("the inner header has an idom");
        let place = |schedule: &mut ir_schedule::Schedule, id: NodeId, b: usize| {
            let i = id as usize;
            if schedule.node_to_block.len() <= i {
                schedule.node_to_block.resize(i + 1, usize::MAX);
            }
            schedule.node_to_block[i] = b;
            schedule.blocks[b].nodes.push(id);
        };
        let outer_ctrl = schedule.blocks[outer_body].ctrl;
        let stop = graph.add(
            Op::LoadStatic {
                class_id: 1,
                field_index: 0,
                type_tag: b'Z',
                is_volatile: true,
            },
            IrType::Int,
            vec![outer_ctrl, outer_ctrl],
            None,
        );
        place(&mut schedule, stop, outer_body);
        let inner_ctrl = schedule.blocks[inner].ctrl;
        let burnt = graph.add(
            Op::LoadStatic {
                class_id: 1,
                field_index: 1,
                type_tag: b'J',
                is_volatile: false,
            },
            IrType::Long,
            vec![inner_ctrl, stop],
            None,
        );
        place(&mut schedule, burnt, inner);
        assert!(
            crate::ir::is_memory_token_slot(&graph.nodes[burnt as usize], 1),
            "`burnt` names `stop` in its token slot"
        );
        (graph, schedule, inner, stop)
    }

    /// Interpreter round i2 wave 63, lane L2 (i58-L2 proposal, first
    /// measurement): the census sees the proposal's shape. Off, `stop` is live
    /// into the inner header and across its back edge only because `burnt`
    /// names it as a token; on, its interval is its definition alone.
    #[test]
    fn i63_l2_token_trim_census_sees_a_value_held_across_a_loop_by_a_token_read() {
        let (graph, schedule, inner, stop) = nested_loop_with_a_token_read();
        let off = build_live_model_with_token_edges(&graph, &schedule, false);
        let on = build_live_model_with_token_edges(&graph, &schedule, true);
        assert!(
            off.converged && on.converged,
            "a small nested loop converges"
        );
        assert_eq!(
            off.live_in_at(inner, stop),
            Some(true),
            "off: live into the inner header"
        );
        assert_eq!(
            on.live_in_at(inner, stop),
            Some(false),
            "on: not live into it"
        );
        // `carried` and the census's header set use the block-INDEX test
        // (a successor at or before its source); a frequency layout may
        // place the inner latch first, so those two are asked only when the
        // index test sees the inner header as one.
        let inner_is_indexed_header = schedule
            .blocks
            .iter()
            .enumerate()
            .any(|(p, b)| p >= inner && b.successors.contains(&inner));
        assert!(!on.carried[stop as usize], "on: spans no back edge");
        let r_on = on.range[stop as usize].expect("`stop` wants a location");
        assert_eq!(
            r_on.lo, r_on.hi,
            "on: no read, so the interval is the definition"
        );
        assert!(on.uses[stop as usize].is_empty(), "on: no use recorded");

        let census = token_trim_census_of(&graph, &schedule, &off, &on).expect("both converge");
        assert_eq!(
            census,
            token_trim_census(&graph, &schedule).expect("both converge")
        );
        assert!(census.trimmed >= 1);
        assert!(census.trimmed_value_typed >= 1, "{}", census.render());
        if off.carried[stop as usize] {
            assert!(census.carried_dropped >= 1, "{}", census.render());
        }
        if inner_is_indexed_header {
            assert!(census.header_live_in_dropped >= 1, "{}", census.render());
        }
        assert!(census.peak_live_on <= census.peak_live_off);
        assert!(
            census
                .examples
                .iter()
                .any(|&(id, ty, _, _)| id == stop && ty == IrType::Int),
            "{}",
            census.render()
        );
        assert!(census.render().starts_with("[ir-ls] token-trim census: "));
    }



    /// The soundness half of the proposal's risk: with the token reads gone
    /// the allocator still produces an allocation `verify_allocation`
    /// accepts, on the shape the switch shortens.
    #[test]
    fn i63_l2_a_trimmed_model_allocates_and_verifies() {
        let (graph, schedule, _, _) = nested_loop_with_a_token_read();
        let live = build_live_model_with_token_edges(&graph, &schedule, true);
        let model = MachineModel::for_graph(&graph, &schedule, &live, RegFile::x86_64());
        let alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        verify_allocation(&graph, &live, &model, &alloc).expect("verifies");
    }

    /// Interpreter round i2 wave 64 (lane L2): the slot-plan column renders
    /// LAST, after the wave-63 fields, and only when `ir_lower` filled it.
    #[test]
    fn i64_l2_the_census_line_carries_the_slot_plan_column_last() {
        let (graph, schedule, _, _) = nested_loop_with_a_token_read();
        let mut census = token_trim_census(&graph, &schedule).expect("both converge");
        let wave63 = census.render();
        assert!(!wave63.contains("slot-words="), "{wave63}");
        census.slot_plan = Some(TokenTrimSlotColumn {
            words_off: 9,
            words_on: 7,
            ref_words_off: 3,
            ref_words_on: 2,
            homes_trimmed: 1,
        });
        let wave64 = census.render();
        assert!(wave64.starts_with(&wave63), "{wave64}");
        assert!(
            wave64.ends_with(" slot-words=9->7 ref-words=3->2 homes-trimmed=1"),
            "{wave64}"
        );
    }



    // ── Interpreter round i2 wave 65, lane L2: one liveness core ─────────

    /// The test graphs the wave-65 core is compared on: a φ loop, a
    /// straight-line graph, and a nested loop with a value-typed memory token
    /// (memory φs, a token-only read).
    pub(crate) fn i65_l2_graphs() -> Vec<(Graph, ir_schedule::Schedule)> {
        let (graph, schedule, _, _) = nested_loop_with_a_token_read();
        vec![swap_loop(), div_and_shift_graph(), (graph, schedule)]
    }

    /// Every combination of the two rules, reads collected.
    pub(crate) fn i65_l2_rule_grid() -> Vec<LivenessRules> {
        let mut out = Vec::new();
        for token_edges_not_uses in [false, true] {
            for phi_edge_interfere in [false, true] {
                out.push(LivenessRules {
                    token_edges_not_uses,
                    phi_edge_interfere,
                    collect_reads: true,
                });
            }
        }
        out
    }

    /// Field-for-field equality of two models (`LiveModel` has no
    /// `PartialEq`).
    pub(crate) fn i65_l2_assert_live_models_equal(a: &LiveModel, b: &LiveModel, what: &str) {
        assert_eq!(a.pos_of, b.pos_of, "{what}: pos_of");
        assert_eq!(a.span, b.span, "{what}: span");
        assert_eq!(
            a.total_positions, b.total_positions,
            "{what}: total_positions"
        );
        assert_eq!(a.block_of_pos, b.block_of_pos, "{what}: block_of_pos");
        assert_eq!(a.wants_loc, b.wants_loc, "{what}: wants_loc");
        assert_eq!(a.range, b.range, "{what}: range");
        assert_eq!(a.class, b.class, "{what}: class");
        assert_eq!(a.is_ref, b.is_ref, "{what}: is_ref");
        assert_eq!(a.pinned, b.pinned, "{what}: pinned");
        assert_eq!(a.uses, b.uses, "{what}: uses");
        assert_eq!(a.weight, b.weight, "{what}: weight");
        assert_eq!(a.loop_depth, b.loop_depth, "{what}: loop_depth");
        assert_eq!(a.carried, b.carried, "{what}: carried");
        assert_eq!(
            a.carried_and_read, b.carried_and_read,
            "{what}: carried_and_read"
        );
        assert_eq!(a.peak_live, b.peak_live, "{what}: peak_live");
        assert_eq!(a.converged, b.converged, "{what}: converged");
        assert_eq!(a.block_live_in, b.block_live_in, "{what}: block_live_in");
    }

    /// The refactor is behaviour-identical: for every rule combination, the
    /// model built over `liveness_core` is the legacy model, field for field.
    #[test]
    fn i65_l2_the_core_live_model_is_the_legacy_live_model() {
        for (i, (graph, schedule)) in i65_l2_graphs().iter().enumerate() {
            for rules in i65_l2_rule_grid() {
                let legacy = build_live_model_legacy(graph, schedule, rules);
                let core = build_live_model_from_core(graph, schedule, rules);
                assert!(legacy.converged, "graph {i}: a small graph converges");
                i65_l2_assert_live_models_equal(&legacy, &core, &format!("graph {i} {rules:?}"));
            }
        }
    }



    /// The core's read list is the legacy model's use list (as a set per
    /// value), and turning `collect_reads` off changes nothing but the list.
    #[test]
    fn i65_l2_the_core_reads_are_the_legacy_uses() {
        for (i, (graph, schedule)) in i65_l2_graphs().iter().enumerate() {
            let wants = live_model_wants_loc(graph, schedule);
            for rules in i65_l2_rule_grid() {
                let core = liveness_core(
                    graph,
                    schedule,
                    &wants,
                    rules,
                    LIVE_MODEL_WORK_BUDGET,
                    LIVE_MODEL_MAX_ITERATIONS,
                );
                let legacy = build_live_model_legacy(graph, schedule, rules);
                let mut uses: Vec<Vec<usize>> = vec![Vec::new(); graph.nodes.len()];
                for &(v, at) in &core.reads {
                    uses[v as usize].push(at);
                }
                for list in &mut uses {
                    list.sort_unstable();
                    list.dedup();
                }
                assert_eq!(uses, legacy.uses, "graph {i} {rules:?}");

                let quiet = liveness_core(
                    graph,
                    schedule,
                    &wants,
                    LivenessRules {
                        collect_reads: false,
                        ..rules
                    },
                    LIVE_MODEL_WORK_BUDGET,
                    LIVE_MODEL_MAX_ITERATIONS,
                );
                assert!(quiet.reads.is_empty());
                assert_eq!(
                    LivenessCore {
                        reads: Vec::new(),
                        ..core
                    },
                    quiet,
                    "graph {i} {rules:?}: collect_reads moves nothing else"
                );
            }
        }
    }

    /// The φ edge rule moves φ intervals only, only outward, and never the
    /// live-in/live-out sets; on the swap loop it moves at least one.
    #[test]
    fn i65_l2_the_phi_edge_rule_moves_only_phi_intervals() {
        let (graph, schedule) = swap_loop();
        let wants = live_model_wants_loc(&graph, &schedule);
        let base = LivenessRules::default();
        let off = liveness_core(
            &graph,
            &schedule,
            &wants,
            base,
            LIVE_MODEL_WORK_BUDGET,
            LIVE_MODEL_MAX_ITERATIONS,
        );
        let on = liveness_core(
            &graph,
            &schedule,
            &wants,
            LivenessRules {
                phi_edge_interfere: true,
                ..base
            },
            LIVE_MODEL_WORK_BUDGET,
            LIVE_MODEL_MAX_ITERATIONS,
        );
        assert!(off.converged && on.converged);
        assert_eq!(off.live_in, on.live_in);
        assert_eq!(off.live_out, on.live_out);
        assert_eq!(off.pos_of, on.pos_of);
        assert!(
            off.set_only_by_phi_rule.iter().all(|&b| !b),
            "no rule, no flag"
        );
        let mut moved = 0usize;
        for id in 0..graph.nodes.len() {
            if !matches!(graph.nodes[id].op, Op::Phi) {
                assert_eq!((off.lo[id], off.hi[id]), (on.lo[id], on.hi[id]), "n{id}");
                continue;
            }
            assert!(on.lo[id] <= off.lo[id] && on.hi[id] >= off.hi[id], "n{id}");
            if (on.lo[id], on.hi[id]) != (off.lo[id], off.hi[id]) {
                moved += 1;
            }
            if off.lo[id] != usize::MAX {
                assert!(
                    !on.set_only_by_phi_rule[id],
                    "n{id} was placed before the rule"
                );
            }
        }
        assert!(moved > 0, "a loop φ's interval reaches its entry edge");
    }

    /// Past the work budget nothing is analysed; out of iterations the fixed
    /// point is reported unconverged. Both are the callers' whole-method case.
    #[test]
    fn i65_l2_the_core_fallbacks() {
        let (graph, schedule) = swap_loop();
        let wants = live_model_wants_loc(&graph, &schedule);
        let rules = LivenessRules {
            token_edges_not_uses: false,
            phi_edge_interfere: true,
            collect_reads: true,
        };
        let skipped = liveness_core(
            &graph,
            &schedule,
            &wants,
            rules,
            0,
            LIVE_MODEL_MAX_ITERATIONS,
        );
        assert!(!skipped.converged);
        assert!(skipped.live_in.is_empty() && skipped.live_out.is_empty());
        assert!(skipped.reads.is_empty());
        assert!(skipped.lo.iter().all(|&l| l == usize::MAX));
        let full = build_live_model_legacy(&graph, &schedule, rules);
        assert_eq!(skipped.total_positions, full.total_positions);
        assert_eq!(skipped.span, full.span);

        let stuck = liveness_core(&graph, &schedule, &wants, rules, LIVE_MODEL_WORK_BUDGET, 0);
        assert!(!stuck.converged);
        assert_eq!(stuck.live_in.len(), stuck.words * schedule.blocks.len());
    }

    /// Compile-time bench probe (interpreter round i2 wave 65, lane L2): the
    /// legacy model vs the model over the shared core, min ns per build over
    /// 9 reps, to stderr. Not a pass/fail test.
    ///
    /// `cargo test -p cratonvm-jit --release --lib i65_l2_bench -- --ignored --nocapture`
    #[test]
    #[ignore = "bench probe; run with --ignored --nocapture"]
    fn i65_l2_bench_live_model_legacy_vs_core() {
        const REPS: usize = 9;
        const ITERS: u32 = 2000;
        let rules = LivenessRules {
            token_edges_not_uses: false,
            phi_edge_interfere: true,
            collect_reads: true,
        };
        let names = ["swap_loop", "div_and_shift", "nested_token"];
        for (name, (graph, schedule)) in names.iter().zip(i65_l2_graphs()) {
            let (mut best_legacy, mut best_core) = (f64::MAX, f64::MAX);
            // Interleaved, so a noisy stretch of the host hits both columns.
            for _ in 0..REPS {
                let t0 = std::time::Instant::now();
                let mut sink = 0usize;
                for _ in 0..ITERS {
                    sink = sink.wrapping_add(
                        build_live_model_legacy(&graph, &schedule, rules).total_positions,
                    );
                }
                std::hint::black_box(sink);
                best_legacy = best_legacy.min(t0.elapsed().as_nanos() as f64 / f64::from(ITERS));

                let t0 = std::time::Instant::now();
                let mut sink = 0usize;
                for _ in 0..ITERS {
                    sink = sink.wrapping_add(
                        build_live_model_from_core(&graph, &schedule, rules).total_positions,
                    );
                }
                std::hint::black_box(sink);
                best_core = best_core.min(t0.elapsed().as_nanos() as f64 / f64::from(ITERS));
            }
            eprintln!(
                "[i65-l2 bench] {name:<14} live-model legacy {best_legacy:>9.0} ns/op  core {best_core:>9.0} ns/op"
            );
        }
    }

    // ── Interpreter round i2 wave 66, lane L2: one liveness pass ─────────

    /// `slot_view` keeps every field the slot plan reads and drops the bit
    /// sets and the read list; `live_model_from_core` over a held core is
    /// `build_live_model_from_core`.
    #[test]
    fn i66_l2_a_held_core_serves_both_consumers() {
        for (i, (graph, schedule)) in i65_l2_graphs().iter().enumerate() {
            let wants = live_model_wants_loc(graph, schedule);
            for rules in i65_l2_rule_grid() {
                let core = liveness_core(
                    graph,
                    schedule,
                    &wants,
                    rules,
                    LIVE_MODEL_WORK_BUDGET,
                    LIVE_MODEL_MAX_ITERATIONS,
                );
                let view = core.slot_view();
                assert_eq!(view.pos_of, core.pos_of, "graph {i}");
                assert_eq!(view.span, core.span, "graph {i}");
                assert_eq!(view.total_positions, core.total_positions, "graph {i}");
                assert_eq!(view.lo, core.lo, "graph {i}");
                assert_eq!(view.hi, core.hi, "graph {i}");
                assert_eq!(
                    view.set_only_by_phi_rule, core.set_only_by_phi_rule,
                    "graph {i}"
                );
                assert_eq!(view.converged, core.converged, "graph {i}");
                assert_eq!(view.words, core.words, "graph {i}");
                assert!(
                    view.live_in.is_empty() && view.live_out.is_empty() && view.reads.is_empty()
                );

                let held = live_model_from_core(graph, schedule, wants.clone(), core);
                let fresh = build_live_model_from_core(graph, schedule, rules);
                i65_l2_assert_live_models_equal(&held, &fresh, &format!("graph {i} {rules:?}"));
            }
        }
    }



    // ── Interpreter round i2 wave 67, lane L2: dense value numbering ─────

    /// `static int f(int n){ int s=0; for(i..n) s+=i; for(i..n) s+=i; ... return s; }`
    /// with `loops` sequential counted loops: a handful of blocks and a few
    /// dozen arena nodes (control, frame states, φs, values) per loop. The
    /// large synthetic graph of the wave-67 tests and bench.
    pub(crate) fn i67_l2_sequential_loops(loops: usize) -> (Graph, ir_schedule::Schedule) {
        let mut code: Vec<u8> = vec![0x03, 0x3c]; // iconst_0; istore_1   s = 0
        for _ in 0..loops {
            code.extend_from_slice(&[
                0x03, 0x3d, // +0: iconst_0; istore_2          i = 0
                0x1c, 0x1a, // +2: iload_2; iload_0            header
                0xa2, 0x00, 0x0d, // +4: if_icmpge +13 (to +17)
                0x1b, 0x1c, 0x60, 0x3c, // +7: iload_1; iload_2; iadd; istore_1
                0x84, 0x02, 0x01, // +11: iinc 2, 1
                0xa7, 0xff, 0xf4, // +14: goto -12 (to +2)
            ]);
        }
        code.extend_from_slice(&[0x1b, 0xac]); // iload_1; ireturn
        let len = code.len();
        code.extend_from_slice(&[0, 0]);
        let graph = IrBuilder::new(1, 4)
            .build(&code, len)
            .expect("the sequential loops build");
        let schedule = ir_schedule::schedule(&graph);
        (graph, schedule)
    }

    /// `graph` with `pad` extra unscheduled constants appended: arena nodes
    /// no block places and no value set wants (dead nodes the scheduler left
    /// behind), i.e. the bits the dense numbering exists to drop.
    pub(crate) fn i67_l2_padded(
        (mut graph, schedule): (Graph, ir_schedule::Schedule),
        pad: usize,
    ) -> (Graph, ir_schedule::Schedule) {
        for i in 0..pad {
            let _ = graph.add(Op::Const(1_000_000 + i as i64), IrType::Int, vec![], None);
        }
        (graph, schedule)
    }

    /// The wave-65 test graphs plus the wave-67 synthetic ones: small and
    /// medium sequential loops, and two graphs mostly made of unwanted nodes.
    pub(crate) fn i67_l2_graphs() -> Vec<(&'static str, Graph, ir_schedule::Schedule)> {
        let mut out: Vec<(&'static str, Graph, ir_schedule::Schedule)> = Vec::new();
        let names = ["swap_loop", "div_and_shift", "nested_token"];
        for (name, (graph, schedule)) in names.iter().zip(i65_l2_graphs()) {
            out.push((*name, graph, schedule));
        }
        let (g, s) = i67_l2_sequential_loops(3);
        out.push(("loops3", g, s));
        let (g, s) = i67_l2_sequential_loops(20);
        out.push(("loops20", g, s));
        let (g, s) = i67_l2_padded(swap_loop(), 1000);
        out.push(("swap+1000dead", g, s));
        let (g, s) = i67_l2_padded(i67_l2_sequential_loops(5), 3000);
        out.push(("loops5+3000dead", g, s));
        out
    }

    /// Value sets to run the cores over: the live model's, a sparser subset
    /// of it, every node (with a `wants` longer than the arena), and none.
    fn i67_l2_value_sets(graph: &Graph, schedule: &ir_schedule::Schedule) -> Vec<Vec<bool>> {
        let loc = live_model_wants_loc(graph, schedule);
        let two_of_three: Vec<bool> = loc
            .iter()
            .enumerate()
            .map(|(i, &w)| w && i % 3 != 1)
            .collect();
        vec![
            loc,
            two_of_three,
            vec![true; graph.nodes.len() + 5],
            Vec::new(),
        ]
    }

    /// The default live-model rules, without reading any switch.
    fn i67_l2_model_rules() -> LivenessRules {
        LivenessRules {
            token_edges_not_uses: false,
            phi_edge_interfere: true,
            collect_reads: true,
        }
    }

    /// The refactor is byte-identical: the dense core's answer is the sparse
    /// core's, field for field (the bit sets mapped back to node ids), on
    /// every test graph, every rule combination with and without the read
    /// list, every value set, and the budget and iteration fallbacks,
    /// including fixed points cut off after one and two passes.
    #[test]
    fn i67_l2_the_dense_core_is_the_sparse_core() {
        let limits = [
            (LIVE_MODEL_WORK_BUDGET, LIVE_MODEL_MAX_ITERATIONS),
            (0, LIVE_MODEL_MAX_ITERATIONS),
            (LIVE_MODEL_WORK_BUDGET, 0),
            (LIVE_MODEL_WORK_BUDGET, 1),
            (LIVE_MODEL_WORK_BUDGET, 2),
        ];
        let mut compared = 0usize;
        for (name, graph, schedule) in i67_l2_graphs() {
            for wants in i67_l2_value_sets(&graph, &schedule) {
                for base in i65_l2_rule_grid() {
                    for collect_reads in [true, false] {
                        let rules = LivenessRules {
                            collect_reads,
                            ..base
                        };
                        for (budget, iters) in limits {
                            let sparse = liveness_core_sparse(
                                &graph, &schedule, &wants, rules, budget, iters,
                            );
                            let dense = liveness_core_dense(
                                &graph, &schedule, &wants, rules, budget, iters, false,
                            );
                            assert_eq!(
                                sparse, dense,
                                "{name} {rules:?} budget {budget} iterations {iters}"
                            );
                            compared += 1;
                        }
                    }
                }
            }
        }
        assert!(compared > 0);
    }

    /// The full-budget runs above are real answers, not two empty ones: on
    /// every test graph the live model's value set converges, the published
    /// layout is node-indexed, and (outside the straight-line graph) some
    /// value is live into some block.
    #[test]
    fn i67_l2_the_compared_answers_are_converged_and_nonempty() {
        for (name, graph, schedule) in i67_l2_graphs() {
            let wants = live_model_wants_loc(&graph, &schedule);
            let dense = liveness_core_dense(
                &graph,
                &schedule,
                &wants,
                i67_l2_model_rules(),
                LIVE_MODEL_WORK_BUDGET,
                LIVE_MODEL_MAX_ITERATIONS,
                false,
            );
            assert!(dense.converged, "{name}");
            assert_eq!(dense.words, graph.nodes.len().div_ceil(64).max(1), "{name}");
            assert_eq!(
                dense.live_in.len(),
                dense.words * schedule.blocks.len(),
                "{name}"
            );
            assert_eq!(dense.live_out.len(), dense.live_in.len(), "{name}");
            if name != "div_and_shift" {
                assert!(
                    dense.live_in.iter().any(|&w| w != 0),
                    "{name}: something is live-in"
                );
            }
        }
    }



    /// `CRATONVM_JIT_IR_LIVENESS_BUDGET_BY_VALUES` is the one behaviour
    /// change: a graph whose arena is over the budget but whose value set is
    /// not gets the answer the sparse core gives it under an unlimited budget,
    /// where without the switch it gets the skipped (whole-method) answer.
    #[test]
    fn i67_l2_the_budget_by_values_admits_a_graph_over_the_node_budget() {
        let (graph, schedule) = i67_l2_padded(i67_l2_sequential_loops(5), 3000);
        let wants = live_model_wants_loc(&graph, &schedule);
        let rules = i67_l2_model_rules();
        let nb = schedule.blocks.len();
        let values = wants.iter().filter(|&&w| w).count();
        let n = graph.nodes.len();
        assert!(values * 10 < n, "the padded graph is mostly unwanted nodes");
        let budget = values * nb;
        assert!(budget > 0 && n * nb > budget);

        let skipped = liveness_core_sparse(&graph, &schedule, &wants, rules, budget, 256);
        assert!(
            !skipped.converged && skipped.live_in.is_empty(),
            "over the node budget"
        );
        let dense_by_nodes =
            liveness_core_dense(&graph, &schedule, &wants, rules, budget, 256, false);
        assert_eq!(skipped, dense_by_nodes, "without the switch, the same skip");

        let admitted = liveness_core_dense(&graph, &schedule, &wants, rules, budget, 256, true);
        let unlimited = liveness_core_sparse(&graph, &schedule, &wants, rules, usize::MAX, 256);
        assert!(admitted.converged);
        assert_eq!(admitted, unlimited, "the analysed answer is the sparse one");

        let one_short =
            liveness_core_dense(&graph, &schedule, &wants, rules, budget - 1, 256, true);
        assert!(
            !one_short.converged && one_short.live_in.is_empty(),
            "the budget still bites"
        );
    }

    /// Compile-time bench probe (interpreter round i2 wave 67, lane L2): the
    /// sparse core vs the dense core, ns per `liveness_core` run (live-model
    /// rules, read list kept, the live model's value set), min over 15
    /// interleaved reps, with `values / nodes` and the block count, to stderr.
    /// Not a pass/fail test beyond the two answers being equal.
    ///
    /// `cargo test -p cratonvm-jit --release --lib i67_l2_bench -- --ignored --nocapture`
    #[test]
    #[ignore = "bench probe; run with --ignored --nocapture"]
    fn i67_l2_bench_sparse_vs_dense_liveness_core() {
        const REPS: usize = 15;
        let rules = i67_l2_model_rules();
        let mut graphs = i67_l2_graphs();
        for (name, loops) in [("loops50", 50usize), ("loops150", 150), ("loops250", 250)] {
            let (g, s) = i67_l2_sequential_loops(loops);
            graphs.push((name, g, s));
        }
        let (g, s) = i67_l2_padded(i67_l2_sequential_loops(150), 20_000);
        graphs.push(("loops150+20000dead", g, s));
        for (name, graph, schedule) in &graphs {
            let wants = live_model_wants_loc(graph, schedule);
            let n = graph.nodes.len();
            let nb = schedule.blocks.len();
            let values = wants.iter().filter(|&&w| w).count();
            let sparse_run = || {
                liveness_core_sparse(
                    graph,
                    schedule,
                    &wants,
                    rules,
                    LIVE_MODEL_WORK_BUDGET,
                    LIVE_MODEL_MAX_ITERATIONS,
                )
            };
            let dense_run = || {
                liveness_core_dense(
                    graph,
                    schedule,
                    &wants,
                    rules,
                    LIVE_MODEL_WORK_BUDGET,
                    LIVE_MODEL_MAX_ITERATIONS,
                    false,
                )
            };
            let once = sparse_run();
            assert_eq!(once, dense_run(), "{name}");
            // Roughly the same amount of work per column per rep on every
            // graph, at least 3 runs.
            let iters = (20_000_000 / (n * nb / 64 + n + 1)).clamp(3, 20_000);
            let (mut best_sparse, mut best_dense) = (f64::MAX, f64::MAX);
            for _ in 0..REPS {
                let t0 = std::time::Instant::now();
                let mut sink = 0usize;
                for _ in 0..iters {
                    sink = sink.wrapping_add(sparse_run().live_in.len());
                }
                std::hint::black_box(sink);
                best_sparse = best_sparse.min(t0.elapsed().as_nanos() as f64 / iters as f64);

                let t0 = std::time::Instant::now();
                let mut sink = 0usize;
                for _ in 0..iters {
                    sink = sink.wrapping_add(dense_run().live_in.len());
                }
                std::hint::black_box(sink);
                best_dense = best_dense.min(t0.elapsed().as_nanos() as f64 / iters as f64);
            }
            eprintln!(
                "[i67-l2 bench] {name:<20} nodes {n:>6} values {values:>6} ({:>5.3}) blocks {nb:>5} \
                 converged {:<5} sparse {best_sparse:>12.0} ns/run dense {best_dense:>12.0} ns/run \
                 dense/sparse {:>5.3}",
                values as f64 / n.max(1) as f64,
                once.converged,
                best_dense / best_sparse,
            );
        }
    }

    // ── Interpreter round i2 wave 69, lane L2: the worklist fixed point ──

    /// A small deterministic generator (xorshift64*), so the random CFGs and
    /// sets are the same on every host.
    struct I69Rng(u64);

    impl I69Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }

        fn below(&mut self, n: usize) -> usize {
            if n == 0 {
                0
            } else {
                (self.next() % n as u64) as usize
            }
        }
    }

    /// A synthetic CFG: block `b`'s successor list, blocks in layout order.
    type I69Cfg = Vec<Vec<usize>>;

    /// A typical method body: `segments` straight-line diamonds (if/else
    /// joins), every third segment a single-block-body loop instead.
    fn i69_l2_typical(segments: usize) -> I69Cfg {
        let mut succ: I69Cfg = vec![vec![1]];
        for k in 0..segments {
            let b = succ.len();
            if k % 3 == 2 {
                // header -> body, exit (the next segment); body -> header
                succ.push(vec![b + 1, b + 2]);
                succ.push(vec![b]);
            } else {
                // b -> b+1, b+2; both -> b+3 (the next segment)
                succ.push(vec![b + 1, b + 2]);
                succ.push(vec![b + 3]);
                succ.push(vec![b + 3]);
            }
        }
        succ.push(Vec::new()); // the return block
        succ
    }

    /// `depth` perfectly nested loops: headers `1..=depth`, the innermost
    /// body, then each loop's exit (innermost first), which branches back to
    /// the enclosing header, and the return block.
    fn i69_l2_loop_nest(depth: usize) -> I69Cfg {
        let d = depth.max(1);
        let header = |i: usize| 1 + i;
        let body = 1 + d;
        let exit = |i: usize| 2 + d + (d - 1 - i);
        let ret = 2 + 2 * d;
        let mut succ: I69Cfg = vec![Vec::new(); ret + 1];
        succ[0] = vec![header(0)];
        for i in 0..d {
            let inner = if i + 1 < d { header(i + 1) } else { body };
            succ[header(i)] = vec![inner, exit(i)];
            succ[exit(i)] = vec![if i == 0 { ret } else { header(i - 1) }];
        }
        succ[body] = vec![header(d - 1)];
        succ
    }

    /// A loop whose header is an `arms`-way switch: every arm joins, and the
    /// join branches back to the header.
    fn i69_l2_wide_switch(arms: usize) -> I69Cfg {
        let join = 2 + arms;
        let exit = join + 1;
        let mut succ: I69Cfg = vec![Vec::new(); exit + 1];
        succ[0] = vec![1];
        succ[1] = (2..2 + arms).chain([exit]).collect();
        for arm in succ.iter_mut().take(2 + arms).skip(2) {
            *arm = vec![join];
        }
        succ[join] = vec![1];
        succ
    }

    /// `regions` two-entry (irreducible) loops in sequence; with `handlers`,
    /// every block of the middle third also has an exception-style edge to
    /// one handler block laid out after the return block, which rejoins the
    /// return block (a retreating edge in layout order).
    fn i69_l2_irreducible(regions: usize, handlers: bool) -> I69Cfg {
        let mut succ: I69Cfg = vec![vec![1]];
        for _ in 0..regions {
            let c = succ.len();
            succ.push(vec![c + 1, c + 2]); // enter at a or at b
            succ.push(vec![c + 2, c + 3]); // a -> b, out
            succ.push(vec![c + 1, c + 3]); // b -> a, out
        }
        let ret = succ.len();
        succ.push(Vec::new());
        if handlers {
            let handler = succ.len();
            succ.push(vec![ret]);
            for b in succ.iter_mut().take(2 * ret / 3).skip(ret / 3) {
                b.push(handler);
            }
        }
        succ
    }

    /// A random CFG: 0 to 3 successors per block (sometimes 9, a switch),
    /// duplicate edges, self loops and out-of-range indices (which both fixed
    /// points ignore) included.
    fn i69_l2_random_cfg(rng: &mut I69Rng, nb: usize) -> I69Cfg {
        (0..nb)
            .map(|_| {
                let k = if rng.below(10) == 0 { 9 } else { rng.below(4) };
                (0..k).map(|_| rng.below(nb + 2)).collect::<Vec<usize>>()
            })
            .collect()
    }

    /// Random `(phi_out, uses, defs)` sets, `words` per block: sparse reads,
    /// denser definitions.
    fn i69_l2_random_sets(
        rng: &mut I69Rng,
        nb: usize,
        words: usize,
    ) -> (Vec<u64>, Vec<u64>, Vec<u64>) {
        let len = nb * words;
        let phi_out: Vec<u64> = (0..len)
            .map(|_| rng.next() & rng.next() & rng.next() & rng.next())
            .collect();
        let uses: Vec<u64> = (0..len)
            .map(|_| rng.next() & rng.next() & rng.next())
            .collect();
        let defs: Vec<u64> = (0..len).map(|_| rng.next() & rng.next()).collect();
        (phi_out, uses, defs)
    }

    /// Run one fixed point from all-zero sets.
    fn i69_l2_run(
        succ: &I69Cfg,
        sets: LsFixedPointSets<'_>,
        max_iterations: usize,
        fixed_point: LivenessFixedPoint,
    ) -> (bool, Vec<u64>, Vec<u64>, usize) {
        let mut li = vec![0u64; sets.nb * sets.words];
        let mut lout = vec![0u64; sets.nb * sets.words];
        let mut visits = 0usize;
        let converged = match fixed_point {
            LivenessFixedPoint::RoundRobin | LivenessFixedPoint::RoundRobinDriver => {
                ls_fixed_point_round_robin(
                    succ.as_slice(),
                    sets,
                    &mut li,
                    &mut lout,
                    max_iterations,
                    &mut visits,
                )
            }
            LivenessFixedPoint::Worklist => ls_fixed_point_worklist(
                succ.as_slice(),
                sets,
                &mut li,
                &mut lout,
                max_iterations,
                &mut visits,
            ),
            // Wave 71: a bare list has no predecessor lists of its own; the
            // caller-built index stands in for the schedule's.
            LivenessFixedPoint::WorklistSchedulePreds => ls_fixed_point_worklist_preds(
                succ.as_slice(),
                &LsPredIndex::build(succ.as_slice(), sets.nb),
                sets,
                &mut li,
                &mut lout,
                max_iterations,
                &mut visits,
            ),
            LivenessFixedPoint::WorklistPostorder { .. } => {
                let (order, rank) = ls_postorder(succ.as_slice(), sets.nb);
                ls_fixed_point_worklist_ordered(
                    succ.as_slice(),
                    &LsPredIndex::build(succ.as_slice(), sets.nb),
                    &order,
                    &rank,
                    sets,
                    &mut li,
                    &mut lout,
                    max_iterations,
                    &mut visits,
                )
            }
        };
        (converged, li, lout, visits)
    }

    /// The two fixed points agree pass for pass on `succ`: for every cap from
    /// 0 to one past the round-robin pass count, the same verdict and the
    /// same `li`/`lout` (cut-off runs included), and the worklist never visits
    /// more blocks. Returns the round-robin pass count.
    fn i69_l2_assert_same_fixed_point(
        name: &str,
        succ: &I69Cfg,
        sets: LsFixedPointSets<'_>,
    ) -> usize {
        let nb = sets.nb;
        let (converged, _, _, visits) =
            i69_l2_run(succ, sets, 100_000, LivenessFixedPoint::RoundRobin);
        assert!(converged, "{name}: the round-robin fixed point settles");
        let passes = if nb == 0 { 1 } else { visits / nb };
        for cap in 0..=passes + 1 {
            let rr = i69_l2_run(succ, sets, cap, LivenessFixedPoint::RoundRobin);
            let wl = i69_l2_run(succ, sets, cap, LivenessFixedPoint::Worklist);
            assert_eq!(rr.0, wl.0, "{name} cap {cap}: converged");
            assert_eq!(rr.0, cap >= passes, "{name} cap {cap}: passes {passes}");
            assert_eq!(rr.1, wl.1, "{name} cap {cap}: live-in");
            assert_eq!(rr.2, wl.2, "{name} cap {cap}: live-out");
            assert!(wl.3 <= rr.3, "{name} cap {cap}: visits {} > {}", wl.3, rr.3);
            // Wave 71: the driver over a caller-built index (and the stack
            // dirty set below 65 blocks) is the per-call one, visits included.
            let wp = i69_l2_run(succ, sets, cap, LivenessFixedPoint::WorklistSchedulePreds);
            assert_eq!(wl, wp, "{name} cap {cap}: prebuilt predecessor index");
        }
        passes
    }

    #[test]
    fn i69_l2_highest_set_below() {
        let mut bits = vec![0u64; 3];
        assert_eq!(ls_highest_set_below(&bits, 192), None);
        for i in [0usize, 5, 63, 64, 127, 130] {
            ls_bit_set(&mut bits, i);
        }
        assert_eq!(ls_highest_set_below(&bits, 0), None);
        assert_eq!(ls_highest_set_below(&bits, 1), Some(0));
        assert_eq!(ls_highest_set_below(&bits, 5), Some(0));
        assert_eq!(ls_highest_set_below(&bits, 6), Some(5));
        assert_eq!(ls_highest_set_below(&bits, 64), Some(63));
        assert_eq!(ls_highest_set_below(&bits, 65), Some(64));
        assert_eq!(ls_highest_set_below(&bits, 127), Some(64));
        assert_eq!(ls_highest_set_below(&bits, 128), Some(127));
        assert_eq!(ls_highest_set_below(&bits, 131), Some(130));
        assert_eq!(ls_highest_set_below(&bits, 192), Some(130));
        // A limit past the words reads nothing out of range.
        assert_eq!(ls_highest_set_below(&bits, 1000), Some(130));
    }

    /// The shaped CFGs (typical, deep nest, wide switch, irreducible with and
    /// without exception-style edges) and 300 random ones, one- and
    /// multi-word sets: the worklist is the round-robin fixed point, pass for
    /// pass.
    #[test]
    fn i69_l2_the_worklist_is_the_round_robin_fixed_point_on_every_cfg_shape() {
        let mut rng = I69Rng(0x9E37_79B9_7F4A_7C15);
        let mut shaped: Vec<(String, I69Cfg)> = vec![
            ("empty".into(), Vec::new()),
            ("one block".into(), vec![Vec::new()]),
            ("self loop".into(), vec![vec![0, 1], vec![]]),
            ("typical".into(), i69_l2_typical(12)),
            ("nest8".into(), i69_l2_loop_nest(8)),
            ("switch40".into(), i69_l2_wide_switch(40)),
            ("irreducible".into(), i69_l2_irreducible(6, false)),
            ("irreducible+handlers".into(), i69_l2_irreducible(6, true)),
        ];
        for i in 0..300 {
            let nb = 1 + rng.below(70);
            shaped.push((format!("random{i}"), i69_l2_random_cfg(&mut rng, nb)));
        }
        let mut multi_pass = 0usize;
        for (name, succ) in &shaped {
            let nb = succ.len();
            for words in [1usize, 3] {
                let (phi_out, uses, defs) = i69_l2_random_sets(&mut rng, nb, words);
                let sets = LsFixedPointSets {
                    nb,
                    words,
                    phi_out: &phi_out,
                    uses: &uses,
                    defs: &defs,
                };
                if i69_l2_assert_same_fixed_point(name, succ, sets) > 2 {
                    multi_pass += 1;
                }
            }
        }
        assert!(
            multi_pass > 10,
            "the grid exercises graphs that need several passes"
        );
    }

    /// On every scheduled test graph, value set, rule combination, budget
    /// and iteration cap (0 to 3 passes and the real cap), sparse and dense:
    /// the core with the worklist is the round-robin core, field for field,
    /// and the round-robin core is the pre-wave-69 one.
    #[test]
    fn i69_l2_the_worklist_core_is_the_round_robin_core() {
        let limits = [
            (LIVE_MODEL_WORK_BUDGET, LIVE_MODEL_MAX_ITERATIONS),
            (0, LIVE_MODEL_MAX_ITERATIONS),
            (LIVE_MODEL_WORK_BUDGET, 0),
            (LIVE_MODEL_WORK_BUDGET, 1),
            (LIVE_MODEL_WORK_BUDGET, 2),
            (LIVE_MODEL_WORK_BUDGET, 3),
        ];
        let mut compared = 0usize;
        for (name, graph, schedule) in i67_l2_graphs() {
            for wants in i67_l2_value_sets(&graph, &schedule) {
                for base in i65_l2_rule_grid() {
                    for collect_reads in [true, false] {
                        let rules = LivenessRules {
                            collect_reads,
                            ..base
                        };
                        for (budget, iters) in limits {
                            for dense in [false, true] {
                                let what = format!(
                                    "{name} {rules:?} budget {budget} iterations {iters} dense {dense}"
                                );
                                let (rr, rr_visits) = liveness_core_with(
                                    &graph,
                                    &schedule,
                                    &wants,
                                    rules,
                                    budget,
                                    iters,
                                    dense,
                                    false,
                                    LivenessFixedPoint::RoundRobin,
                                );
                                let (wl, wl_visits) = liveness_core_with(
                                    &graph,
                                    &schedule,
                                    &wants,
                                    rules,
                                    budget,
                                    iters,
                                    dense,
                                    false,
                                    LivenessFixedPoint::Worklist,
                                );
                                assert_eq!(rr, wl, "{what}");
                                assert!(wl_visits <= rr_visits, "{what}");
                                let before = if dense {
                                    liveness_core_dense(
                                        &graph, &schedule, &wants, rules, budget, iters, false,
                                    )
                                } else {
                                    liveness_core_sparse(
                                        &graph, &schedule, &wants, rules, budget, iters,
                                    )
                                };
                                assert_eq!(rr, before, "{what}: the refactored round robin");
                                compared += 1;
                            }
                        }
                    }
                }
            }
        }
        assert!(compared > 0);
    }

    /// The worklist does save visits where the proposal said it would: on
    /// sequential loops it visits fewer blocks than whole passes do.
    #[test]
    fn i69_l2_the_worklist_skips_settled_blocks_on_sequential_loops() {
        let (graph, schedule) = i67_l2_sequential_loops(20);
        let wants = live_model_wants_loc(&graph, &schedule);
        let run = |fixed_point| {
            liveness_core_with(
                &graph,
                &schedule,
                &wants,
                i67_l2_model_rules(),
                LIVE_MODEL_WORK_BUDGET,
                LIVE_MODEL_MAX_ITERATIONS,
                false,
                false,
                fixed_point,
            )
        };
        let (rr, rr_visits) = run(LivenessFixedPoint::RoundRobin);
        let (wl, wl_visits) = run(LivenessFixedPoint::Worklist);
        assert!(rr.converged);
        assert_eq!(rr, wl);
        let nb = schedule.blocks.len();
        assert!(
            rr_visits >= 2 * nb,
            "at least the settling pass and the check pass"
        );
        assert!(wl_visits < rr_visits, "{wl_visits} < {rr_visits}");
    }



    /// The iteration caps the `i70_l2_*` tests run every legacy copy under:
    /// none, one to three passes (cut-off runs on every looping graph), and
    /// the real cap.
    pub(crate) const I70_L2_CAPS: [usize; 5] = [0, 1, 2, 3, LIVE_MODEL_MAX_ITERATIONS];

    /// The φ-free transfer (the chain copy's, `ir_lower::chain_read_colour_ranges`):
    /// with an all-zero φ-out set the worklist is still the round-robin fixed
    /// point, pass for pass, on every shaped CFG and 100 random ones.
    #[test]
    fn i70_l2_the_worklist_is_the_round_robin_fixed_point_without_phi_out() {
        let mut rng = I69Rng(0x2545_F491_4F6C_DD1D);
        let mut shaped: Vec<(String, I69Cfg)> = vec![
            ("empty".into(), Vec::new()),
            ("self loop".into(), vec![vec![0, 1], vec![]]),
            ("typical".into(), i69_l2_typical(12)),
            ("nest8".into(), i69_l2_loop_nest(8)),
            ("switch40".into(), i69_l2_wide_switch(40)),
            ("irreducible+handlers".into(), i69_l2_irreducible(6, true)),
        ];
        for i in 0..100 {
            let nb = 1 + rng.below(70);
            shaped.push((format!("random{i}"), i69_l2_random_cfg(&mut rng, nb)));
        }
        let mut multi_pass = 0usize;
        for (name, succ) in &shaped {
            let nb = succ.len();
            for words in [1usize, 2] {
                let (_, uses, defs) = i69_l2_random_sets(&mut rng, nb, words);
                let zeros = vec![0u64; nb * words];
                let sets = LsFixedPointSets {
                    nb,
                    words,
                    phi_out: &zeros,
                    uses: &uses,
                    defs: &defs,
                };
                if i69_l2_assert_same_fixed_point(name, succ, sets) > 2 {
                    multi_pass += 1;
                }
            }
        }
        assert!(
            multi_pass > 5,
            "the grid exercises graphs that need several passes"
        );
    }





    // ── Interpreter round i3 wave 71, lane L2: the block-count gate and the
    //    schedule's own predecessor lists ────────────────────────────────


/// The scheduled graphs the wave-71 tests run over: the wave-67 test
    /// graphs, sequential loops of several lengths, and the same loops
    /// scheduled with the hot-path layout (which remaps both edge lists).
    pub(crate) fn i71_l2_scheduled_graphs() -> Vec<(String, Graph, ir_schedule::Schedule)> {
        let mut out: Vec<(String, Graph, ir_schedule::Schedule)> = i67_l2_graphs()
            .into_iter()
            .map(|(name, g, s)| (name.to_string(), g, s))
            .collect();
        for loops in [1usize, 2, 8, 30] {
            let (g, s) = i67_l2_sequential_loops(loops);
            out.push((format!("loops{loops}"), g, s));
            let (g, _) = i67_l2_sequential_loops(loops);
            let opts = ir_schedule::ScheduleOptions {
                layout_hot_paths: true,
                ..Default::default()
            };
            let s = ir_schedule::schedule_with_options(&g, &opts);
            out.push((format!("loops{loops} hot layout"), g, s));
        }
        out
    }



    /// The invariant `CRATONVM_JIT_IR_LIVENESS_SCHEDULE_PREDS` rests on: on
    /// every schedule `ir_schedule` builds (hot-path layout included), each
    /// block's own predecessor set is the inverted in-range successor lists.
    #[test]
    fn i71_l2_schedule_predecessors_are_the_inverted_successors() {
        let mut edges = 0usize;
        for (name, _graph, schedule) in i71_l2_scheduled_graphs() {
            let blocks = schedule.blocks.as_slice();
            let nb = blocks.len();
            let index = LsPredIndex::build(blocks, nb);
            let own = LsSchedulePreds(blocks);
            for b in 0..nb {
                let mut inverted: Vec<usize> = index.ls_predecessors(b).to_vec();
                inverted.sort_unstable();
                inverted.dedup();
                let mut listed: Vec<usize> = own
                    .ls_predecessors(b)
                    .iter()
                    .copied()
                    .filter(|&p| p < nb)
                    .collect();
                listed.sort_unstable();
                listed.dedup();
                assert_eq!(inverted, listed, "{name}: block {b}");
                edges += inverted.len();
            }
        }
        assert!(edges > 0);
    }

    /// The worklist over the schedule's own predecessor lists is the
    /// per-call-index worklist, visit for visit, and so the round robin's
    /// answer: in the shared core (every value set, rule combination and cap),
    /// in the live model's legacy copy and in `ir_lower`'s three legacy copies.
    #[test]
    fn i71_l2_the_schedule_preds_worklist_is_the_per_call_worklist() {
        let mut compared = 0usize;
        for (name, graph, schedule) in i71_l2_scheduled_graphs() {
            for wants in i67_l2_value_sets(&graph, &schedule) {
                for rules in i65_l2_rule_grid() {
                    for cap in I70_L2_CAPS {
                        let run = |fixed_point| {
                            liveness_core_with(
                                &graph,
                                &schedule,
                                &wants,
                                rules,
                                LIVE_MODEL_WORK_BUDGET,
                                cap,
                                false,
                                false,
                                fixed_point,
                            )
                        };
                        let what = format!("{name} {rules:?} cap {cap}");
                        let rr = run(LivenessFixedPoint::RoundRobin);
                        let wl = run(LivenessFixedPoint::Worklist);
                        let wp = run(LivenessFixedPoint::WorklistSchedulePreds);
                        assert_eq!(rr.0, wp.0, "{what}: core");
                        assert_eq!(wl, wp, "{what}: core visits");
                        compared += 1;
                    }
                }
            }
            for rules in i65_l2_rule_grid() {
                for cap in I70_L2_CAPS {
                    let what = format!("{name} {rules:?} cap {cap}");
                    let run = |fixed_point| {
                        let mut trace = LsLegacyRun::default();
                        let model = build_live_model_legacy_with(
                            &graph,
                            &schedule,
                            rules,
                            cap,
                            fixed_point,
                            Some(&mut trace),
                        );
                        (model, trace)
                    };
                    let (rr, rr_trace) = run(LivenessFixedPoint::RoundRobin);
                    let (_, wl_trace) = run(LivenessFixedPoint::Worklist);
                    let (wp, wp_trace) = run(LivenessFixedPoint::WorklistSchedulePreds);
                    assert_eq!(rr_trace.answer(), wp_trace.answer(), "{what}: live model");
                    assert_eq!(wl_trace, wp_trace, "{what}: live model visits");
                    i65_l2_assert_live_models_equal(&rr, &wp, &what);
                    compared += 1;
                }
            }
            for (copy, run) in crate::ir_lower::i70_l2_legacy_liveness_tests::i70_l2_ir_lower_copies(
                &graph, &schedule,
            ) {
                for cap in I70_L2_CAPS {
                    let rr = run(LivenessFixedPoint::RoundRobin, cap);
                    let wl = run(LivenessFixedPoint::Worklist, cap);
                    let wp = run(LivenessFixedPoint::WorklistSchedulePreds, cap);
                    assert_eq!(rr.answer(), wp.answer(), "{name} {copy} cap {cap}");
                    assert_eq!(wl, wp, "{name} {copy} cap {cap}: visits");
                    compared += 1;
                }
            }
        }
        assert!(compared > 0);
    }



    /// The shaped CFGs and 100 random ones the postorder tests run over.
    fn i71_l2_cfgs(rng: &mut I69Rng) -> Vec<(String, I69Cfg)> {
        let mut out: Vec<(String, I69Cfg)> = vec![
            ("empty".into(), Vec::new()),
            ("one".into(), vec![Vec::new()]),
            ("self loop".into(), vec![vec![0, 1], vec![]]),
            ("typical".into(), i69_l2_typical(12)),
            ("nest8".into(), i69_l2_loop_nest(8)),
            ("switch40".into(), i69_l2_wide_switch(40)),
            ("irreducible".into(), i69_l2_irreducible(6, false)),
            ("irreducible+handlers".into(), i69_l2_irreducible(6, true)),
            ("reversed chain".into(), i71_l2_reversed_chain(30)),
        ];
        for i in 0..100 {
            let nb = 1 + rng.below(90);
            out.push((format!("random{i}"), i69_l2_random_cfg(rng, nb)));
        }
        out
    }

    /// A chain laid out backwards: the entry (block 0) jumps to the last
    /// block, each block `b >= 2` falls to `b - 1`, block 1 returns. Every
    /// edge but the entry's retreats in index order, so the index-order
    /// passes move a live value one block per pass; in postorder it is one
    /// pass.
    fn i71_l2_reversed_chain(nb: usize) -> I69Cfg {
        let nb = nb.max(3);
        let mut succ: I69Cfg = vec![Vec::new(); nb];
        succ[0] = vec![nb - 1];
        for (b, s) in succ.iter_mut().enumerate().skip(2) {
            *s = vec![b - 1];
        }
        succ
    }

    /// `ls_postorder` is a permutation with `rank` its inverse, and on a DAG
    /// every edge goes from a lower rank to a higher one (the driver visits
    /// the higher rank first: successors before predecessors).
    #[test]
    fn i71_l2_postorder_is_a_permutation_and_orders_dag_edges() {
        let mut rng = I69Rng(0x0123_4567_89AB_CDEF);
        for (name, succ) in i71_l2_cfgs(&mut rng) {
            let nb = succ.len();
            let (order, rank) = ls_postorder(succ.as_slice(), nb);
            let mut seen = vec![false; nb];
            for k in 0..nb {
                assert!(!seen[order[k]], "{name}: block {} twice", order[k]);
                seen[order[k]] = true;
                assert_eq!(rank[order[k]], k, "{name}: rank is the inverse");
            }
        }
        for nb in [1usize, 2, 5, 17, 64, 65, 130] {
            let dag: I69Cfg = (0..nb)
                .map(|b| {
                    let picks = rng.below(4);
                    (0..picks)
                        .filter_map(|_| {
                            let room = nb - b - 1;
                            (room > 0).then(|| b + 1 + rng.below(room))
                        })
                        .collect::<Vec<usize>>()
                })
                .collect();
            let (_, rank) = ls_postorder(dag.as_slice(), nb);
            for (b, ss) in dag.iter().enumerate() {
                for &s in ss {
                    assert!(rank[s] > rank[b], "dag {nb}: edge {b} -> {s}");
                }
            }
        }
    }

    /// The ordered driver with the identity order is the index-order worklist
    /// at every cap, visits included; with the postorder it converges to the
    /// round robin's sets (the least fixed point) on every CFG, with and
    /// without a φ-out term.
    #[test]
    fn i71_l2_the_postorder_worklist_converges_to_the_round_robin() {
        let mut rng = I69Rng(0x5DEE_CE66_D1CE_4E5B);
        let mut fewer = 0usize;
        for (name, succ) in i71_l2_cfgs(&mut rng) {
            let nb = succ.len();
            for words in [1usize, 2] {
                let (phi_out, uses, defs) = i69_l2_random_sets(&mut rng, nb, words);
                let zeros = vec![0u64; nb * words];
                for phi in [&phi_out, &zeros] {
                    let sets = LsFixedPointSets {
                        nb,
                        words,
                        phi_out: phi,
                        uses: &uses,
                        defs: &defs,
                    };
                    let rr = i69_l2_run(&succ, sets, 100_000, LivenessFixedPoint::RoundRobin);
                    assert!(rr.0, "{name}: the round robin settles");
                    let passes = if nb == 0 { 1 } else { rr.3 / nb };
                    let identity: Vec<usize> = (0..nb).collect();
                    let index = LsPredIndex::build(succ.as_slice(), nb);
                    for cap in 0..=passes + 1 {
                        let wl = i69_l2_run(&succ, sets, cap, LivenessFixedPoint::Worklist);
                        let mut li = vec![0u64; nb * words];
                        let mut lout = vec![0u64; nb * words];
                        let mut visits = 0usize;
                        let converged = ls_fixed_point_worklist_ordered(
                            succ.as_slice(),
                            &index,
                            &identity,
                            &identity,
                            sets,
                            &mut li,
                            &mut lout,
                            cap,
                            &mut visits,
                        );
                        assert_eq!(
                            wl,
                            (converged, li, lout, visits),
                            "{name} cap {cap}: identity"
                        );
                    }
                    let po = i69_l2_run(
                        &succ,
                        sets,
                        100_000,
                        LivenessFixedPoint::WorklistPostorder {
                            schedule_preds: false,
                        },
                    );
                    assert!(po.0, "{name}: the postorder settles");
                    assert_eq!(
                        (&rr.1, &rr.2),
                        (&po.1, &po.2),
                        "{name}: the least fixed point"
                    );
                    if po.3 < rr.3 {
                        fewer += 1;
                    }
                }
            }
        }
        assert!(fewer > 0);

        // The reversed chain: one value read in the return block, defined
        // nowhere. Index order: about a pass per block; postorder: two.
        let nb = 40;
        let succ = i71_l2_reversed_chain(nb);
        let mut uses = vec![0u64; nb];
        uses[1] = 1;
        let zeros = vec![0u64; nb];
        let sets = LsFixedPointSets {
            nb,
            words: 1,
            phi_out: &zeros,
            uses: &uses,
            defs: &zeros,
        };
        let rr = i69_l2_run(&succ, sets, 100_000, LivenessFixedPoint::RoundRobin);
        let po = i69_l2_run(
            &succ,
            sets,
            100_000,
            LivenessFixedPoint::WorklistPostorder {
                schedule_preds: false,
            },
        );
        assert_eq!((&rr.1, &rr.2), (&po.1, &po.2));
        assert!(rr.3 >= (nb - 2) * nb, "index order: {} visits", rr.3);
        assert!(po.3 <= 2 * nb, "postorder: {} visits", po.3);
        // Cut off after one pass, the postorder has already settled.
        let po1 = i69_l2_run(
            &succ,
            sets,
            2,
            LivenessFixedPoint::WorklistPostorder {
                schedule_preds: false,
            },
        );
        assert!(po1.0);
        let rr1 = i69_l2_run(&succ, sets, 2, LivenessFixedPoint::RoundRobin);
        assert!(!rr1.0);
    }

    /// On the scheduled graphs the postorder worklist (either predecessor
    /// source) gives the round robin's converged answer in the shared core
    /// and in all four legacy copies, at the real caps.
    #[test]
    fn i71_l2_the_postorder_worklist_answers_like_the_round_robin_on_schedules() {
        let mut compared = 0usize;
        for (name, graph, schedule) in i71_l2_scheduled_graphs() {
            for wants in i67_l2_value_sets(&graph, &schedule) {
                for rules in i65_l2_rule_grid() {
                    let run = |fixed_point| {
                        liveness_core_with(
                            &graph,
                            &schedule,
                            &wants,
                            rules,
                            LIVE_MODEL_WORK_BUDGET,
                            LIVE_MODEL_MAX_ITERATIONS,
                            false,
                            false,
                            fixed_point,
                        )
                        .0
                    };
                    let rr = run(LivenessFixedPoint::RoundRobin);
                    for schedule_preds in [false, true] {
                        let po = run(LivenessFixedPoint::WorklistPostorder { schedule_preds });
                        assert_eq!(rr, po, "{name} {rules:?} {schedule_preds}");
                        compared += 1;
                    }
                }
            }
            for rules in i65_l2_rule_grid() {
                let run = |fixed_point| {
                    let mut trace = LsLegacyRun::default();
                    let model = build_live_model_legacy_with(
                        &graph,
                        &schedule,
                        rules,
                        LIVE_MODEL_MAX_ITERATIONS,
                        fixed_point,
                        Some(&mut trace),
                    );
                    (model, trace)
                };
                let (rr, rr_trace) = run(LivenessFixedPoint::RoundRobin);
                for schedule_preds in [false, true] {
                    let (po, po_trace) =
                        run(LivenessFixedPoint::WorklistPostorder { schedule_preds });
                    let what = format!("{name} {rules:?} {schedule_preds}: live model");
                    assert_eq!(rr_trace.answer(), po_trace.answer(), "{what}");
                    i65_l2_assert_live_models_equal(&rr, &po, &what);
                    compared += 1;
                }
            }
            for (copy, run) in crate::ir_lower::i70_l2_legacy_liveness_tests::i70_l2_ir_lower_copies(
                &graph, &schedule,
            ) {
                let rr = run(LivenessFixedPoint::RoundRobin, LIVE_MODEL_MAX_ITERATIONS);
                for schedule_preds in [false, true] {
                    let po = run(
                        LivenessFixedPoint::WorklistPostorder { schedule_preds },
                        LIVE_MODEL_MAX_ITERATIONS,
                    );
                    assert_eq!(rr.answer(), po.answer(), "{name} {copy} {schedule_preds}");
                    compared += 1;
                }
            }
        }
        assert!(compared > 0);
    }

    /// Compile-time bench probe (interpreter round i2 wave 69, lane L2): the
    /// round-robin fixed point vs the worklist, ns per graph (min over 15
    /// interleaved reps) and block visits, to stderr. Two tables: the fixed
    /// point alone on synthetic CFGs ((a) typical method bodies, (b) deep
    /// loop nests, (c) wide switches, (d) irreducible graphs with and without
    /// exception-style edges; random sets of 4 values per block), and the
    /// whole `liveness_core` (live-model rules and value set, sparse) on the
    /// scheduled test graphs and the long sequential-loop graphs. Not a
    /// pass/fail test beyond the answers being equal.
    ///
    /// `cargo test -p cratonvm-jit --release --lib i69_l2_bench -- --ignored --nocapture`
    #[test]
    #[ignore = "bench probe; run with --ignored --nocapture"]
    fn i69_l2_bench_round_robin_vs_worklist_liveness() {
        const REPS: usize = 15;
        // Min ns per call of `f` over REPS reps of `iters` calls each, for
        // two closures interleaved.
        fn best_pair(
            iters: usize,
            mut a: impl FnMut() -> usize,
            mut b: impl FnMut() -> usize,
        ) -> (f64, f64) {
            let (mut best_a, mut best_b) = (f64::MAX, f64::MAX);
            for _ in 0..REPS {
                let t0 = std::time::Instant::now();
                let mut sink = 0usize;
                for _ in 0..iters {
                    sink = sink.wrapping_add(a());
                }
                std::hint::black_box(sink);
                best_a = best_a.min(t0.elapsed().as_nanos() as f64 / iters as f64);
                let t0 = std::time::Instant::now();
                let mut sink = 0usize;
                for _ in 0..iters {
                    sink = sink.wrapping_add(b());
                }
                std::hint::black_box(sink);
                best_b = best_b.min(t0.elapsed().as_nanos() as f64 / iters as f64);
            }
            (best_a, best_b)
        }

        let mut rng = I69Rng(0xD1B5_4A32_D192_ED03);
        let cfgs: Vec<(&str, I69Cfg)> = vec![
            ("(a) typical 10", i69_l2_typical(10)),
            ("(a) typical 40", i69_l2_typical(40)),
            ("(a) typical 200", i69_l2_typical(200)),
            ("(b) nest 4", i69_l2_loop_nest(4)),
            ("(b) nest 16", i69_l2_loop_nest(16)),
            ("(b) nest 64", i69_l2_loop_nest(64)),
            ("(c) switch 64", i69_l2_wide_switch(64)),
            ("(c) switch 1000", i69_l2_wide_switch(1000)),
            ("(d) irreducible 10", i69_l2_irreducible(10, false)),
            ("(d) irreducible 100", i69_l2_irreducible(100, false)),
            ("(d) irred+exc 100", i69_l2_irreducible(100, true)),
        ];
        for (name, succ) in &cfgs {
            let nb = succ.len();
            let words = (4 * nb).div_ceil(64).max(1);
            let (phi_out, uses, defs) = i69_l2_random_sets(&mut rng, nb, words);
            let sets = LsFixedPointSets {
                nb,
                words,
                phi_out: &phi_out,
                uses: &uses,
                defs: &defs,
            };
            let max = LIVE_MODEL_MAX_ITERATIONS;
            let rr = i69_l2_run(succ, sets, max, LivenessFixedPoint::RoundRobin);
            let wl = i69_l2_run(succ, sets, max, LivenessFixedPoint::Worklist);
            assert_eq!((rr.0, &rr.1, &rr.2), (wl.0, &wl.1, &wl.2), "{name}");
            let iters = (20_000_000 / (rr.3 * words + 1)).clamp(3, 100_000);
            let (ns_rr, ns_wl) = best_pair(
                iters,
                || i69_l2_run(succ, sets, max, LivenessFixedPoint::RoundRobin).3,
                || i69_l2_run(succ, sets, max, LivenessFixedPoint::Worklist).3,
            );
            eprintln!(
                "[i69-l2 bench] fixed point {name:<20} blocks {nb:>5} words {words:>3} converged {:<5} \
                 visits rr {:>7} wl {:>7} round-robin {ns_rr:>11.0} ns/graph worklist {ns_wl:>11.0} \
                 ns/graph wl/rr {:>5.3}",
                rr.0,
                rr.3,
                wl.3,
                ns_wl / ns_rr,
            );
        }

        let rules = i67_l2_model_rules();
        let mut graphs = i67_l2_graphs();
        for (name, loops) in [("loops50", 50usize), ("loops150", 150), ("loops250", 250)] {
            let (g, s) = i67_l2_sequential_loops(loops);
            graphs.push((name, g, s));
        }
        for (name, graph, schedule) in &graphs {
            let wants = live_model_wants_loc(graph, schedule);
            let n = graph.nodes.len();
            let nb = schedule.blocks.len();
            let run = |fixed_point| {
                liveness_core_with(
                    graph,
                    schedule,
                    &wants,
                    rules,
                    LIVE_MODEL_WORK_BUDGET,
                    LIVE_MODEL_MAX_ITERATIONS,
                    false,
                    false,
                    fixed_point,
                )
            };
            let (rr, rr_visits) = run(LivenessFixedPoint::RoundRobin);
            let (wl, wl_visits) = run(LivenessFixedPoint::Worklist);
            assert_eq!(rr, wl, "{name}");
            let iters = (20_000_000 / (n * nb / 64 + n + 1)).clamp(3, 20_000);
            let (ns_rr, ns_wl) = best_pair(
                iters,
                || run(LivenessFixedPoint::RoundRobin).0.live_in.len(),
                || run(LivenessFixedPoint::Worklist).0.live_in.len(),
            );
            eprintln!(
                "[i69-l2 bench] core {name:<20} nodes {n:>6} blocks {nb:>5} converged {:<5} \
                 visits rr {rr_visits:>7} wl {wl_visits:>7} round-robin {ns_rr:>11.0} ns/run \
                 worklist {ns_wl:>11.0} ns/run wl/rr {:>5.3}",
                rr.converged,
                ns_wl / ns_rr,
            );
        }

        // Interpreter round i2 wave 70, lane L2: the legacy copies the
        // default JIT runs (`CRATONVM_JIT_IR_LEGACY_LIVENESS_WORKLIST`), each
        // timed whole (its set-up included, which the switch does not touch):
        // the live model's (`build_live_model_legacy`) and `ir_lower`'s slot
        // plan, value-read ranges and chain-read ranges.
        for (name, graph, schedule) in &graphs {
            let n = graph.nodes.len();
            let nb = schedule.blocks.len();
            let live_model: crate::ir_lower::i70_l2_legacy_liveness_tests::I70Copy<'_> =
                Box::new(move |fixed_point: LivenessFixedPoint, cap: usize| {
                    let mut trace = LsLegacyRun::default();
                    let _ = build_live_model_legacy_with(
                        graph,
                        schedule,
                        rules,
                        cap,
                        fixed_point,
                        Some(&mut trace),
                    );
                    trace
                });
            let mut copies = vec![("live model", live_model)];
            copies.extend(
                crate::ir_lower::i70_l2_legacy_liveness_tests::i70_l2_ir_lower_copies(
                    graph, schedule,
                ),
            );
            for (copy, run) in &copies {
                let max = LIVE_MODEL_MAX_ITERATIONS;
                let rr = run(LivenessFixedPoint::RoundRobin, max);
                let wl = run(LivenessFixedPoint::Worklist, max);
                assert_eq!(rr.answer(), wl.answer(), "{name} {copy}");
                let iters = (20_000_000 / (n * nb / 64 + n + 1)).clamp(3, 20_000);
                let (ns_rr, ns_wl) = best_pair(
                    iters,
                    || run(LivenessFixedPoint::RoundRobin, max).visits,
                    || run(LivenessFixedPoint::Worklist, max).visits,
                );
                eprintln!(
                    "[i70-l2 bench] legacy {copy:<12} {name:<20} nodes {n:>6} blocks {nb:>5} \
                     ran {:<5} converged {:<5} visits rr {:>7} wl {:>7} round-robin {ns_rr:>11.0} \
                     ns/run worklist {ns_wl:>11.0} ns/run wl/rr {:>5.3}",
                    rr.ran,
                    rr.converged,
                    rr.visits,
                    wl.visits,
                    ns_wl / ns_rr,
                );
            }
        }
    }

    /// Min ns per call of each of three closures, interleaved, over 11 reps;
    /// the batch size is calibrated on the first closure to about 2 ms
    /// (interpreter round i3 wave 71, lane L2).
    fn i71_l2_best3(
        a: &mut dyn FnMut() -> usize,
        b: &mut dyn FnMut() -> usize,
        c: &mut dyn FnMut() -> usize,
    ) -> [f64; 3] {
        let mut fs: [&mut dyn FnMut() -> usize; 3] = [a, b, c];
        let mut sink = 0usize;
        for _ in 0..4 {
            sink = sink.wrapping_add((fs[0])());
        }
        let t0 = std::time::Instant::now();
        for _ in 0..16 {
            sink = sink.wrapping_add((fs[0])());
        }
        let per_call = (t0.elapsed().as_nanos() as f64 / 16.0).max(1.0);
        std::hint::black_box(sink);
        let iters = ((2_000_000.0 / per_call) as usize).clamp(3, 200_000);
        let mut best = [f64::MAX; 3];
        for _ in 0..11 {
            for (k, f) in fs.iter_mut().enumerate() {
                let t0 = std::time::Instant::now();
                let mut sink = 0usize;
                for _ in 0..iters {
                    sink = sink.wrapping_add((*f)());
                }
                std::hint::black_box(sink);
                best[k] = best[k].min(t0.elapsed().as_nanos() as f64 / iters as f64);
            }
        }
        best
    }

    /// The smallest block count from which every row of a family (sorted by
    /// block count) has a ratio below 1, if any: where a block-count gate
    /// would go for that family.
    fn i71_l2_crossover(rows: &[(usize, f64)]) -> Option<usize> {
        let mut from = None;
        for &(nb, ratio) in rows.iter().rev() {
            if ratio < 1.0 {
                from = Some(nb);
            } else {
                break;
            }
        }
        from
    }

    /// Compile-time bench probe (interpreter round i3 wave 71, lane L2;
    /// `i70-L2-proposal-one-predecessor-index-per-schedule`): where the
    /// worklist starts to beat the round robin, by block count, for the
    /// `CRATONVM_JIT_IR_LIVENESS_WORKLIST_MIN_BLOCKS` gate, and what reading
    /// the schedule's own predecessor lists
    /// (`CRATONVM_JIT_IR_LIVENESS_SCHEDULE_PREDS`) saves. Three columns per
    /// row, ns per run (min over 11 interleaved reps): round robin (`rr`),
    /// worklist with its per-call predecessor index (`wl`), worklist over a
    /// predecessor index built once (`wp`; on a schedule, its own lists).
    /// Tables, to stderr:
    ///   * `fixed point`: the driver alone on synthetic CFG families of 2 to
    ///     ~130 blocks (typical method bodies, loop nests; random sets of 4
    ///     values per block), the index for `wp` built outside the timing;
    ///   * `core`: the whole `liveness_core` (live-model rules and values,
    ///     sparse) on scheduled sequential-loop graphs of 1 to 24 loops and
    ///     the three small test graphs;
    ///   * `postorder` / `core postorder`: the postorder pass order
    ///     (`CRATONVM_JIT_IR_LIVENESS_POSTORDER`) against the round robin and
    ///     the index-order worklist, on the families plus a chain laid out
    ///     backwards, and in the core on the scheduled graphs;
    ///   * `legacy`: the four legacy copies the default JIT runs, on the same
    ///     graphs, timed whole;
    ///   * `crossover`: per family, the smallest block count from which `wl`
    ///     (and `wp`) beats `rr` on every larger row.
    /// Not a pass/fail test beyond the answers being equal.
    ///
    /// `cargo test -p cratonvm-jit --release --lib i71_l2_bench -- --ignored --nocapture`
    #[test]
    #[ignore = "bench probe; run with --ignored --nocapture"]
    fn i71_l2_bench_worklist_crossover_by_block_count() {
        let max = LIVE_MODEL_MAX_ITERATIONS;
        let mut rng = I69Rng(0x9E37_79B9_7F4A_7C15);
        let families: Vec<(&str, Vec<I69Cfg>)> = vec![
            (
                "typical",
                [0usize, 1, 2, 3, 5, 6, 9, 12, 15, 18, 24, 36, 48]
                    .iter()
                    .map(|&s| i69_l2_typical(s))
                    .collect(),
            ),
            (
                "nest",
                [1usize, 2, 4, 6, 8, 12, 16, 24, 32, 64]
                    .iter()
                    .map(|&d| i69_l2_loop_nest(d))
                    .collect(),
            ),
            (
                "irred+exc",
                [1usize, 2, 4, 8, 12, 16, 24, 40]
                    .iter()
                    .map(|&r| i69_l2_irreducible(r, true))
                    .collect(),
            ),
        ];
        let mut crossovers: Vec<(String, Option<usize>, Option<usize>)> = Vec::new();
        for (family, cfgs) in &families {
            let mut wl_rows: Vec<(usize, f64)> = Vec::new();
            let mut wp_rows: Vec<(usize, f64)> = Vec::new();
            for succ in cfgs.iter() {
                let nb = succ.len();
                let words = (4 * nb).div_ceil(64).max(1);
                let (phi_out, uses, defs) = i69_l2_random_sets(&mut rng, nb, words);
                let sets = LsFixedPointSets {
                    nb,
                    words,
                    phi_out: &phi_out,
                    uses: &uses,
                    defs: &defs,
                };
                let rr = i69_l2_run(succ, sets, max, LivenessFixedPoint::RoundRobin);
                let wl = i69_l2_run(succ, sets, max, LivenessFixedPoint::Worklist);
                let wp = i69_l2_run(succ, sets, max, LivenessFixedPoint::WorklistSchedulePreds);
                assert_eq!((rr.0, &rr.1, &rr.2), (wl.0, &wl.1, &wl.2), "{family} {nb}");
                assert_eq!(wl, wp, "{family} {nb}");
                let index = LsPredIndex::build(succ.as_slice(), nb);
                let mut run_rr = || i69_l2_run(succ, sets, max, LivenessFixedPoint::RoundRobin).3;
                let mut run_wl = || i69_l2_run(succ, sets, max, LivenessFixedPoint::Worklist).3;
                let mut run_wp = || {
                    let mut li = vec![0u64; nb * words];
                    let mut lout = vec![0u64; nb * words];
                    let mut visits = 0usize;
                    let _ = ls_fixed_point_worklist_preds(
                        succ.as_slice(),
                        &index,
                        sets,
                        &mut li,
                        &mut lout,
                        max,
                        &mut visits,
                    );
                    visits
                };
                let [ns_rr, ns_wl, ns_wp] = i71_l2_best3(&mut run_rr, &mut run_wl, &mut run_wp);
                eprintln!(
                    "[i71-l2 bench] fixed point {family:<10} blocks {nb:>4} words {words:>2} \
                     visits rr {:>6} wl {:>6} rr {ns_rr:>9.0} ns/run wl {ns_wl:>9.0} ns/run \
                     wp {ns_wp:>9.0} ns/run wl/rr {:>5.3} wp/rr {:>5.3}",
                    rr.3,
                    wl.3,
                    ns_wl / ns_rr,
                    ns_wp / ns_rr,
                );
                wl_rows.push((nb, ns_wl / ns_rr));
                wp_rows.push((nb, ns_wp / ns_rr));
            }
            wl_rows.sort_by_key(|r| r.0);
            wp_rows.sort_by_key(|r| r.0);
            crossovers.push((
                format!("fixed point {family}"),
                i71_l2_crossover(&wl_rows),
                i71_l2_crossover(&wp_rows),
            ));
        }

        // The postorder pass order (`CRATONVM_JIT_IR_LIVENESS_POSTORDER`):
        // round robin vs the index-order worklist vs the postorder worklist
        // (its order and index built per run, as on a schedule), on the same
        // families plus a chain laid out backwards (every edge retreats).
        let mut po_families = families.clone();
        po_families.push((
            "reversed",
            [4usize, 8, 16, 32, 64, 128]
                .iter()
                .map(|&n| i71_l2_reversed_chain(n))
                .collect(),
        ));
        for (family, cfgs) in &po_families {
            for succ in cfgs.iter() {
                let nb = succ.len();
                let words = (4 * nb).div_ceil(64).max(1);
                let (phi_out, uses, defs) = i69_l2_random_sets(&mut rng, nb, words);
                let sets = LsFixedPointSets {
                    nb,
                    words,
                    phi_out: &phi_out,
                    uses: &uses,
                    defs: &defs,
                };
                let po_fp = LivenessFixedPoint::WorklistPostorder {
                    schedule_preds: false,
                };
                let rr = i69_l2_run(succ, sets, max, LivenessFixedPoint::RoundRobin);
                let wl = i69_l2_run(succ, sets, max, LivenessFixedPoint::Worklist);
                let po = i69_l2_run(succ, sets, max, po_fp);
                assert_eq!((rr.0, &rr.1, &rr.2), (po.0, &po.1, &po.2), "{family} {nb}");
                let mut run_rr = || i69_l2_run(succ, sets, max, LivenessFixedPoint::RoundRobin).3;
                let mut run_wl = || i69_l2_run(succ, sets, max, LivenessFixedPoint::Worklist).3;
                let mut run_po = || i69_l2_run(succ, sets, max, po_fp).3;
                let [ns_rr, ns_wl, ns_po] = i71_l2_best3(&mut run_rr, &mut run_wl, &mut run_po);
                eprintln!(
                    "[i71-l2 bench] postorder {family:<10} blocks {nb:>4} visits rr {:>6} wl {:>6} \
                     po {:>6} rr {ns_rr:>9.0} ns/run wl {ns_wl:>9.0} ns/run po {ns_po:>9.0} \
                     ns/run po/rr {:>5.3} po/wl {:>5.3}",
                    rr.3,
                    wl.3,
                    po.3,
                    ns_po / ns_rr,
                    ns_po / ns_wl,
                );
            }
        }

        // Scheduled graphs: the three small test graphs, then sequential
        // loops of growing length.
        let rules = i67_l2_model_rules();
        let mut graphs: Vec<(String, Graph, ir_schedule::Schedule)> = Vec::new();
        for (name, (g, s)) in ["swap_loop", "div_and_shift", "nested_token"]
            .iter()
            .zip(i65_l2_graphs())
        {
            graphs.push((name.to_string(), g, s));
        }
        for loops in [1usize, 2, 3, 4, 6, 8, 12, 16, 24] {
            let (g, s) = i67_l2_sequential_loops(loops);
            graphs.push((format!("loops{loops}"), g, s));
        }
        let mut core_wl: Vec<(usize, f64)> = Vec::new();
        let mut core_wp: Vec<(usize, f64)> = Vec::new();
        let mut legacy_wl: Vec<(usize, f64)> = Vec::new();
        let mut legacy_wp: Vec<(usize, f64)> = Vec::new();
        for (name, graph, schedule) in &graphs {
            let wants = live_model_wants_loc(graph, schedule);
            let n = graph.nodes.len();
            let nb = schedule.blocks.len();
            let core = |fixed_point| {
                liveness_core_with(
                    graph,
                    schedule,
                    &wants,
                    rules,
                    LIVE_MODEL_WORK_BUDGET,
                    max,
                    false,
                    false,
                    fixed_point,
                )
            };
            let (rr, rr_visits) = core(LivenessFixedPoint::RoundRobin);
            let wl = core(LivenessFixedPoint::Worklist);
            let wp = core(LivenessFixedPoint::WorklistSchedulePreds);
            assert_eq!(rr, wl.0, "{name}");
            assert_eq!(wl, wp, "{name}");
            let [ns_rr, ns_wl, ns_wp] = i71_l2_best3(
                &mut || core(LivenessFixedPoint::RoundRobin).1,
                &mut || core(LivenessFixedPoint::Worklist).1,
                &mut || core(LivenessFixedPoint::WorklistSchedulePreds).1,
            );
            eprintln!(
                "[i71-l2 bench] core {name:<14} nodes {n:>5} blocks {nb:>4} visits rr {rr_visits:>6} \
                 wl {:>6} rr {ns_rr:>9.0} ns/run wl {ns_wl:>9.0} ns/run wp {ns_wp:>9.0} ns/run \
                 wl/rr {:>5.3} wp/rr {:>5.3}",
                wl.1,
                ns_wl / ns_rr,
                ns_wp / ns_rr,
            );
            if name.starts_with("loops") {
                core_wl.push((nb, ns_wl / ns_rr));
                core_wp.push((nb, ns_wp / ns_rr));
            }
            let po_fp = LivenessFixedPoint::WorklistPostorder {
                schedule_preds: true,
            };
            let po = core(po_fp);
            assert_eq!(rr, po.0, "{name}: postorder");
            let [ns_rr2, ns_wp2, ns_po] = i71_l2_best3(
                &mut || core(LivenessFixedPoint::RoundRobin).1,
                &mut || core(LivenessFixedPoint::WorklistSchedulePreds).1,
                &mut || core(po_fp).1,
            );
            eprintln!(
                "[i71-l2 bench] core postorder {name:<14} blocks {nb:>4} visits wp {:>6} po {:>6} \
                 rr {ns_rr2:>9.0} ns/run wp {ns_wp2:>9.0} ns/run po {ns_po:>9.0} ns/run \
                 po/rr {:>5.3} po/wp {:>5.3}",
                wp.1,
                po.1,
                ns_po / ns_rr2,
                ns_po / ns_wp2,
            );

            let live_model: crate::ir_lower::i70_l2_legacy_liveness_tests::I70Copy<'_> =
                Box::new(move |fixed_point: LivenessFixedPoint, cap: usize| {
                    let mut trace = LsLegacyRun::default();
                    let _ = build_live_model_legacy_with(
                        graph,
                        schedule,
                        rules,
                        cap,
                        fixed_point,
                        Some(&mut trace),
                    );
                    trace
                });
            let mut copies = vec![("live model", live_model)];
            copies.extend(
                crate::ir_lower::i70_l2_legacy_liveness_tests::i70_l2_ir_lower_copies(
                    graph, schedule,
                ),
            );
            let (mut sum_rr, mut sum_wl, mut sum_wp) = (0.0f64, 0.0f64, 0.0f64);
            for (copy, run) in &copies {
                let rr = run(LivenessFixedPoint::RoundRobin, max);
                let wl = run(LivenessFixedPoint::Worklist, max);
                let wp = run(LivenessFixedPoint::WorklistSchedulePreds, max);
                assert_eq!(rr.answer(), wl.answer(), "{name} {copy}");
                assert_eq!(wl, wp, "{name} {copy}");
                let [ns_rr, ns_wl, ns_wp] = i71_l2_best3(
                    &mut || run(LivenessFixedPoint::RoundRobin, max).visits,
                    &mut || run(LivenessFixedPoint::Worklist, max).visits,
                    &mut || run(LivenessFixedPoint::WorklistSchedulePreds, max).visits,
                );
                sum_rr += ns_rr;
                sum_wl += ns_wl;
                sum_wp += ns_wp;
                eprintln!(
                    "[i71-l2 bench] legacy {copy:<12} {name:<14} nodes {n:>5} blocks {nb:>4} \
                     ran {:<5} visits rr {:>6} wl {:>6} rr {ns_rr:>9.0} ns/run wl {ns_wl:>9.0} \
                     ns/run wp {ns_wp:>9.0} ns/run wl/rr {:>5.3} wp/rr {:>5.3}",
                    rr.ran,
                    rr.visits,
                    wl.visits,
                    ns_wl / ns_rr,
                    ns_wp / ns_rr,
                );
            }
            eprintln!(
                "[i71-l2 bench] legacy all four   {name:<14} nodes {n:>5} blocks {nb:>4} \
                 rr {sum_rr:>9.0} ns/compile wl {sum_wl:>9.0} ns/compile wp {sum_wp:>9.0} \
                 ns/compile wl/rr {:>5.3} wp/rr {:>5.3}",
                sum_wl / sum_rr,
                sum_wp / sum_rr,
            );
            if name.starts_with("loops") {
                legacy_wl.push((nb, sum_wl / sum_rr));
                legacy_wp.push((nb, sum_wp / sum_rr));
            }
        }
        for (what, mut wl_rows, mut wp_rows) in [
            ("core loops", core_wl, core_wp),
            ("legacy loops", legacy_wl, legacy_wp),
        ] {
            wl_rows.sort_by_key(|r| r.0);
            wp_rows.sort_by_key(|r| r.0);
            crossovers.push((
                what.to_string(),
                i71_l2_crossover(&wl_rows),
                i71_l2_crossover(&wp_rows),
            ));
        }
        for (what, wl_from, wp_from) in &crossovers {
            eprintln!(
                "[i71-l2 bench] crossover {what:<22} wl beats rr from {} blocks, wp from {} blocks",
                wl_from.map_or("never".to_string(), |b| b.to_string()),
                wp_from.map_or("never".to_string(), |b| b.to_string()),
            );
        }
    }

    #[test]
    fn phi_arguments_are_consumed_at_the_predecessor_edge() {
        let (graph, schedule) = swap_loop();
        // The edge-use rule of `plan_slots` (a memory φ's incoming edge is a use); the token
        // rule that drops it (`CRATONVM_JIT_IR_LS_TOKEN_EDGES_NOT_USES`, default ON) is tested apart.
        let live = build_live_model_with_token_edges(&graph, &schedule, false);
        assert!(live.converged, "a small loop must reach the fixed point");

        let edges: HashSet<usize> = live.span.iter().map(|&(_, e)| e).collect();
        let mut checked = 0usize;
        for (pid, phi) in graph.nodes.iter().enumerate() {
            if !matches!(phi.op, Op::Phi) || !live.wants_loc[pid] {
                continue;
            }
            for v in phi.phi_value_inputs().flatten() {
                if !live.wants_loc[v as usize] {
                    continue;
                }
                assert!(
                    live.uses[v as usize].iter().any(|u| edges.contains(u)),
                    "n{v} feeds phi n{pid} but records no use at any outgoing edge — \
                     the position model disagrees with ir_lower::plan_slots"
                );
                checked += 1;
            }
        }
        assert!(
            checked > 0,
            "the swap loop must have phi arguments to check"
        );
    }

    #[test]
    fn a_scheduled_loop_allocates_and_verifies() {
        let (graph, schedule) = swap_loop();
        let live = build_live_model(&graph, &schedule);
        let model = MachineModel::for_graph(&graph, &schedule, &live, RegFile::x86_64());
        assert!(
            !model.safepoints.is_empty(),
            "a back edge polls, which publishes the oop map"
        );

        let alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        verify_allocation(&graph, &live, &model, &alloc).expect("verifies");
        assert_eq!(alloc.peak_live, live.peak_live);
        // Every phi keeps the home word `ir_lower::emit_phi_copies` writes.
        for (id, node) in graph.nodes.iter().enumerate() {
            if matches!(node.op, Op::Phi) && live.wants_loc[id] {
                assert!(
                    !alloc.is_promoted(id as NodeId),
                    "phi n{id} must stay home-bound"
                );
            }
        }
    }

    #[test]
    fn an_unconverged_model_promotes_nothing() {
        let (graph, schedule) = swap_loop();
        let mut live = build_live_model(&graph, &schedule);
        // Simulate the skipped / non-converging fixed point: every value gets
        // the whole method and is pinned, exactly as `build_live_model` leaves
        // it when it cannot analyse a graph.
        live.converged = false;
        let model = MachineModel::for_graph(&graph, &schedule, &live, RegFile::x86_64());
        let alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        assert_eq!(
            alloc.promoted, 0,
            "an unanalysed graph keeps the frame layout ir_lower already had"
        );
        verify_allocation(&graph, &live, &model, &alloc).expect("verifies");
    }

    /// The clobber model must count every op `ir_lower` lowers to a `CALL`
    /// that RETURNS into the body, not only the ops whose name says "call".
    ///
    /// Found while giving this allocator its first production call site: with
    /// a caller-saved register file, an FP value held across an `Op::Load`
    /// (which calls `jit_getfield`) or an `Op::Rem` (which calls `jit_drem`)
    /// would have been destroyed by the helper and read back as garbage. The
    /// old predicate named only `Call`/`New`/`NewArray`/`LambdaIntToDouble`.
    #[test]
    fn every_op_ir_lower_lowers_to_a_returning_call_is_a_clobber() {
        for op in [
            Op::Call { info_ptr: 0 },
            Op::New {
                class_id: 0,
                num_fields: 0,
            },
            Op::NewArray {
                element_type: 0,
                component_class_id: 0,
            },
            Op::LambdaIntToDouble,
            // Round 16 (lane indy): `jit_indy_bridge`.
            Op::IndyBridge { site: 0 },
            // Round 16 (lane matrix): `jit_multianewarray_n`.
            Op::MultiNewArray { site: 0 },
            // nio21 w15 L3: `jit_new_object_cp`.
            Op::NewCp {
                holder_class_id: 0,
                cp_idx: 0,
            },
            // `jit_putstatic_*`.
            Op::StoreStatic {
                class_id: 0,
                field_index: 0,
                type_tag: b'D',
                is_volatile: false,
            },
            // `jit_frem` / `jit_drem`.
            Op::Rem,
            // `jit_getfield` / `jit_putfield_int`.
            Op::Load(crate::ir::MemKind::Double),
            Op::Store(crate::ir::MemKind::Int),
            // `jit_monitor_enter` / `jit_monitor_exit` / `jit_aastore`.
            Op::MonitorEnter,
            Op::MonitorExit,
            Op::ArrayStore(crate::ir::MemKind::Ref),
        ] {
            assert!(
                ir_op_is_call(&op),
                "{op:?} is lowered to a CALL that returns into the body, so it \
                 destroys the caller-saved registers"
            );
        }
        // A guard's failure edge never returns into the body: the deopt stub
        // runs the epilogue. Counting it would cost a register in every
        // bounds-checked loop for no soundness gain.
        // `jit_putstatic_*` can also run `<clinit>` (arbitrary Java, a GC):
        // a safepoint as well as a clobber.
        assert!(ir_op_is_safepoint(&Op::StoreStatic {
            class_id: 0,
            field_index: 0,
            type_tag: b'L',
            is_volatile: true,
        }));
        assert!(!ir_op_is_call(&Op::Guard { bci: 0 }));
        assert!(!ir_op_is_call(&Op::Add));
        assert!(!ir_op_is_call(&Op::ArrayLoad(crate::ir::MemKind::Double)));
    }

    /// `Op::Phi` DEFINES a value, and this enumeration has to say so.
    ///
    /// Not a restatement of the match arm. `ir_op_defines_value` and
    /// `ir_lower::op_defines_result_slot` are two enumerations of one question,
    /// and `ir_lower`'s `the_two_value_defining_enumerations_agree` compares
    /// the two SETS — so it fails the day one list drops `Op::Phi` and the
    /// other keeps it, but it passes the day BOTH drop it, and a phi that
    /// neither enumeration calls a value is a phi with no home and no register.
    /// That is the state the phi-residency work exists to leave behind, so it
    /// gets an absolute assertion rather than a relative one.
    ///
    /// The relative check is what caught the real bug (`Op::ArrayLength` and
    /// `Op::NewArray` in one list and not the other, silently declining
    /// residency for every method containing an `arraylength`); this is the
    /// floor beneath it.
    #[test]
    fn a_phi_defines_a_value_in_this_enumeration() {
        assert!(
            ir_op_defines_value(&Op::Phi),
            "a phi with no location has no home word for `emit_phi_copies` to \
             write and no interval for the scan to colour"
        );
    }

    /// `release_phi_pins` releases the phis and NOTHING else.
    ///
    /// The pin it drops is structural — "no definition arm ever writes a phi,
    /// so its home word must always hold the value" — and it is legal to drop
    /// exactly for a consumer that supplies that write itself at every incoming
    /// edge. `ir_lower::emit_phi_copies` does. What must not happen is this
    /// call also releasing a DEOPT pin, whose premise ("the home word must hold
    /// the value at any recorded bci") no edge copy discharges: that is
    /// `release_deopt_pins`' separate decision, made by a caller with its own
    /// argument for it.
    ///
    /// Asserted as a difference between two models rather than as an absolute,
    /// so a fixture whose deopt pins happen to be empty cannot make it vacuous.
    #[test]
    fn release_phi_pins_releases_the_phis_and_leaves_every_other_pin_alone() {
        let (graph, schedule) = swap_loop();
        let mut live = build_live_model(&graph, &schedule);
        assert!(live.converged, "a small loop must reach the fixed point");

        let before = live.pinned.clone();
        let pinned_phis: Vec<usize> = (0..graph.nodes.len())
            .filter(|&id| matches!(graph.nodes[id].op, Op::Phi) && before[id])
            .collect();
        assert!(
            !pinned_phis.is_empty(),
            "the swap loop's header phis must be pinned, or this test proves \
             nothing"
        );
        let pinned_others: Vec<usize> = (0..graph.nodes.len())
            .filter(|&id| !matches!(graph.nodes[id].op, Op::Phi) && before[id])
            .collect();
        assert!(
            !pinned_others.is_empty(),
            "the builder records a frame state per bci, so some non-phi value \
             must be deopt-pinned, or the second half of this test is vacuous"
        );

        let released = live.release_phi_pins(&graph);
        assert_eq!(
            released,
            pinned_phis.len(),
            "the count must be the phis it actually unpinned"
        );
        for id in pinned_phis {
            assert!(!live.pinned[id], "phi n{id} kept its structural pin");
        }
        for id in pinned_others {
            assert!(
                live.pinned[id],
                "n{id} is not a phi and lost its pin; `release_phi_pins` has no \
                 argument for releasing a deopt pin"
            );
        }
    }

    // ── r9 ───────────────────────────────────────────────────────────

    /// An integral `Op::Rem` is `CDQ/CQO; IDIV` inline and calls nothing, so
    /// it must not destroy the caller-saved half of the file. The FP remainder
    /// still does (`jit_frem` / `jit_drem`).
    #[test]
    fn an_integer_remainder_is_not_a_call() {
        let mut f = Fixture::new(1);
        let int_rem = f.graph.add(Op::Rem, IrType::Int, vec![], None);
        let long_rem = f.graph.add(Op::Rem, IrType::Long, vec![], None);
        let fp_rem = f.graph.add(Op::Rem, IrType::Double, vec![], None);
        let float_rem = f.graph.add(Op::Rem, IrType::Float, vec![], None);
        assert!(is_inline_integer_rem(&f.graph.nodes[int_rem as usize]));
        assert!(is_inline_integer_rem(&f.graph.nodes[long_rem as usize]));
        assert!(!is_inline_integer_rem(&f.graph.nodes[fp_rem as usize]));
        assert!(!is_inline_integer_rem(&f.graph.nodes[float_rem as usize]));

        // End to end through `for_graph`: `iload_0; iload_1; irem; istore_2;
        // iload_2; ireturn` over a file with one volatile XMM register.
        let code = [0x1a, 0x1b, 0x70, 0x3d, 0x1c, 0xac, 0, 0];
        let graph = IrBuilder::new(2, 4)
            .build(&code, 6)
            .expect("the remainder method builds");
        let schedule = ir_schedule::schedule(&graph);
        let live = build_live_model(&graph, &schedule);
        let xmm2 = PhysReg::xmm(2);
        let file = RegFile::from_specs([
            RegSpec {
                reg: PhysReg::gp(3),
                caller_saved: false,
            },
            RegSpec {
                reg: xmm2,
                caller_saved: true,
            },
        ]);
        let model = MachineModel::for_graph(&graph, &schedule, &live, file);
        let (_, pos) = pos_of_op(&graph, &live, |op, ty| {
            matches!(op, Op::Rem) && matches!(ty, IrType::Int)
        })
        .expect("the method takes a remainder");
        assert!(
            !model.clobbered_at(pos).contains(&xmm2),
            "an integer remainder calls no helper, so a double in a volatile XMM \
             register survives it: {:?}",
            model.clobbered_at(pos)
        );
    }

    /// With a REQUIRED register busy, only that register's holder may be
    /// nominated: evicting any other active frees a register the retry will
    /// not take, and costs a split and a spill for nothing.
    #[test]
    fn a_required_register_is_made_room_for_by_its_holder_only() {
        let mut f = Fixture::new(12);
        let every: Vec<usize> = (1..=10).collect();
        let a = f.value(Op::Add, IrType::Int, 0, 10, &every);
        let b = f.value(Op::Add, IrType::Int, 1, 10, &[10]);
        let c = f.value(Op::Add, IrType::Int, 5, 9, &[6, 9]);
        let (graph, live) = f.finish();
        let active = vec![
            Active {
                reg: PhysReg::gp(0),
                lo: 0,
                hi: 10,
                node: a,
                seg: 0,
            },
            Active {
                reg: PhysReg::gp(1),
                lo: 1,
                hi: 10,
                node: b,
                seg: 0,
            },
        ];
        let current = Pending {
            lo: 5,
            hi: 9,
            node: c,
        };

        // Unrestricted, `b` — one far use — is the obvious victim.
        assert!(
            matches!(
                ls_pick_victim(&graph, &live, &active, RegClass::Gp, current, None),
                Victim::Active(1)
            ),
            "precondition: by score alone `b` is the victim"
        );
        // But `c` must take r0, which `a` holds. Cutting `b` short frees r1,
        // which `c` cannot use; `a` is used on every position and is a worse
        // victim than `c` itself, so `c` is the one that waits.
        assert!(
            matches!(
                ls_pick_victim(
                    &graph,
                    &live,
                    &active,
                    RegClass::Gp,
                    current,
                    Some(PhysReg::gp(0))
                ),
                Victim::Current
            ),
            "only the holder of the required register may be nominated"
        );
        // Nobody holds the required register (it is blocked, not busy): no
        // eviction can help.
        assert!(matches!(
            ls_pick_victim(
                &graph,
                &live,
                &active,
                RegClass::Gp,
                current,
                Some(PhysReg::gp(2))
            ),
            Victim::Current
        ));
    }

    /// A re-queued suffix whose `hi` stops short of an own fixed constraint
    /// must not have its segment stretched to that constraint.
    ///
    /// `v` takes r0 for its constraint at 4 and is cut at 9 by r0's clobber at
    /// 10; `w` then evicts it at 6, and its piece `[7, 9]` is re-queued with no
    /// constraint inside it. It gets r0 again — and the own constraint at 20
    /// (on r1) used to set the segment's end to 19, through the clobber at 10,
    /// which `ls_finish`/`verify_allocation` then refused as an internal error.
    #[test]
    fn an_own_constraint_beyond_a_suffix_does_not_stretch_its_segment() {
        let mut f = Fixture::new(24);
        let v = f.value(Op::Add, IrType::Int, 0, 22, &[7, 22]);
        let dense: Vec<usize> = (2..=21).collect();
        let _x = f.value(Op::Add, IrType::Int, 1, 21, &dense);
        let _w = f.value(Op::Add, IrType::Int, 6, 6, &[6]);
        let (graph, live) = f.finish();

        let mut model = bare_model(gp(2));
        model.fixed = vec![
            FixedConstraint {
                node: v,
                pos: 4,
                reg: PhysReg::gp(0),
            },
            FixedConstraint {
                node: v,
                pos: 20,
                reg: PhysReg::gp(1),
            },
        ];
        model.add_clobbers(10, &[PhysReg::gp(0)]);

        let alloc = allocate_linear_scan(&graph, &live, &model)
            .expect("a suffix is allocated within its own bounds");
        for seg in &alloc.segments[v as usize] {
            if seg.reg == Some(PhysReg::gp(0)) {
                assert!(
                    !seg.range.contains(10),
                    "n{v} holds r0 across the clobber at 10: {:?}",
                    alloc.segments[v as usize]
                );
            }
        }
        verify_allocation(&graph, &live, &model, &alloc).expect("verifies");
    }

    /// A helper-produced reference (`ldc` String here) may donate a home word
    /// but never receive a recycled one — the rule `ir_lower::plan_slots`
    /// applies to all four such ops and this colouring applied to `Op::Call`
    /// alone.
    #[test]
    fn a_helper_produced_reference_never_recycles_a_home_word() {
        let mut f = Fixture::new(12);
        let early = f.value(Op::Load(crate::ir::MemKind::Ref), IrType::Ref, 0, 2, &[2]);
        let string = f.value(
            Op::ConstString {
                holder_class_id: 0,
                cp_idx: 1,
                slot_addr: 0,
            },
            IrType::Ref,
            4,
            8,
            &[8],
        );
        let (graph, live) = f.finish();
        // No register at all, so both values live in their home words and the
        // colouring is the only thing under test.
        let model = bare_model(RegFile::default());

        let alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        assert_eq!(alloc.stack_slot[early as usize], Some(0));
        assert_eq!(
            alloc.stack_slot[string as usize],
            Some(1),
            "`early`'s word is free by position 4, and a `ConstString` result \
             must still not be handed it"
        );
        assert_eq!(alloc.stack_slots, 2);
        verify_allocation(&graph, &live, &model, &alloc).expect("verifies");
    }

    /// An interval that crosses no call takes a free caller-saved register in
    /// preference to a callee-saved one listed before it, leaving the
    /// callee-saved one for an interval that does cross a call.
    #[test]
    fn an_interval_that_crosses_no_call_prefers_a_volatile_register() {
        let mut f = Fixture::new(12);
        let short = f.value(Op::Add, IrType::Int, 0, 3, &[3]);
        let across = f.value(Op::Add, IrType::Int, 1, 8, &[2, 8]);
        let (graph, live) = f.finish();
        // Callee-saved FIRST, as `RegFile::x86_64` and `RegFile::aarch64` list
        // them.
        let file = RegFile::from_specs([
            RegSpec {
                reg: PhysReg::gp(0),
                caller_saved: false,
            },
            RegSpec {
                reg: PhysReg::gp(1),
                caller_saved: true,
            },
        ]);
        let mut model = bare_model(file);
        model.add_clobbers(5, &[PhysReg::gp(1)]);

        let alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        assert_eq!(
            alloc.first_reg(short),
            Some(PhysReg::gp(1)),
            "`short` dies before the call, so the volatile register serves it"
        );
        assert_eq!(
            alloc.first_reg(across),
            Some(PhysReg::gp(0)),
            "which leaves the callee-saved register for the value that crosses \
             the call"
        );
        assert_eq!(alloc.splits, 0, "and nothing has to be split");
        verify_allocation(&graph, &live, &model, &alloc).expect("verifies");
    }

    /// Three blocks, `0 -> 1`, `1 -> {1, 2}`: block 1 is a self loop.
    fn self_loop_schedule() -> ir_schedule::Schedule {
        let edges: [(Vec<usize>, Vec<usize>); 3] = [
            (vec![1], vec![]),        // 0: preheader
            (vec![1, 2], vec![0, 1]), // 1: loop, back edge to itself
            (vec![], vec![1]),        // 2: exit
        ];
        let blocks: Vec<ir_schedule::Block> = edges
            .into_iter()
            .enumerate()
            .map(|(id, (successors, predecessors))| ir_schedule::Block {
                id,
                ctrl: NO_NODE,
                nodes: Vec::new(),
                terminator: None,
                successors,
                predecessors,
            })
            .collect();
        let dom = ir_schedule::Dominators::compute(&blocks);
        ir_schedule::Schedule {
            blocks,
            node_to_block: Vec::new(),
            dom,
            freq: Default::default(),
            layout: Default::default(),
            pairing: Default::default(),
        }
    }

    #[test]
    fn a_split_that_leaves_a_register_on_the_back_edge_is_an_edge_conflict() {
        let mut f = Fixture::new(13);
        // Evicted inside the loop and never reloaded before the back edge: the
        // loop's first positions read r0, which from the second iteration on
        // holds whatever took it at 6.
        let evicted = f.value(Op::Add, IrType::Int, 1, 9, &[4, 9]);
        // Reloaded into the SAME register before the back edge: consistent.
        let reloaded = f.value(Op::Add, IrType::Int, 1, 9, &[4, 8, 9]);
        // One register for its whole range: nothing to disagree about.
        let whole = f.value(Op::Add, IrType::Int, 1, 9, &[9]);
        let (_graph, mut live) = f.finish();
        live.span = vec![(0, 2), (3, 9), (10, 12)];
        let schedule = self_loop_schedule();

        let seg = |lo: usize, hi: usize, reg: Option<u8>| Segment {
            range: PosRange { lo, hi },
            reg: reg.map(PhysReg::gp),
        };
        let alloc = Allocation {
            segments: vec![
                vec![seg(1, 5, Some(0)), seg(6, 9, None)],
                vec![seg(1, 5, Some(1)), seg(6, 7, None), seg(8, 9, Some(1))],
                vec![seg(1, 9, Some(2))],
            ],
            stack_slot: vec![Some(0), Some(1), None],
            stack_slots: 2,
            ..Default::default()
        };

        let conflicts = split_edge_conflicts(&schedule, &live, &alloc);
        assert_eq!(conflicts.len(), 3);
        assert!(
            conflicts[evicted as usize],
            "the back edge arrives with n{evicted} in memory, and block 1's code \
             reads it from r0"
        );
        assert!(
            !conflicts[reloaded as usize],
            "n{reloaded} is back in r1 by the back edge, which is where the loop \
             reads it"
        );
        assert!(
            !conflicts[whole as usize],
            "a single segment cannot disagree"
        );
    }

    /// A caller-supplied pin (a scalar-replacement field value, which
    /// `build_live_model` cannot see) keeps the value out of registers and in a
    /// home word of its own, and is released with the deopt pins.
    #[test]
    fn a_caller_supplied_pin_keeps_a_value_home_bound_until_released() {
        let mut f = Fixture::new(12);
        let field = f.value(Op::Add, IrType::Int, 0, 8, &[8]);
        let other = f.value(Op::Add, IrType::Int, 9, 10, &[10]);
        let (graph, mut live) = f.finish();

        assert_eq!(live.pin_values([field, NO_NODE, 9999]), 1);
        assert_eq!(
            live.pin_values([field]),
            0,
            "already pinned: not counted twice"
        );
        let model = bare_model(gp(2));
        let alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        assert!(
            !alloc.is_promoted(field),
            "a pinned value is never promoted"
        );
        assert!(alloc.is_promoted(other));
        verify_allocation(&graph, &live, &model, &alloc).expect("verifies");

        // The deopt-pin release covers it, on the same write-through argument.
        assert_eq!(live.release_deopt_pins(&graph), 1);
        let alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        assert!(alloc.is_promoted(field));
    }

    /// On a real loop the helper runs and never flags a value that holds one
    /// register (or none) for its whole range.
    #[test]
    fn edge_conflicts_never_flag_an_unsplit_value() {
        let (graph, schedule) = swap_loop();
        let live = build_live_model(&graph, &schedule);
        let model = MachineModel::for_graph(&graph, &schedule, &live, RegFile::x86_64());
        let alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        let conflicts = split_edge_conflicts(&schedule, &live, &alloc);
        assert_eq!(conflicts.len(), graph.nodes.len());
        for (id, segs) in alloc.segments.iter().enumerate() {
            if segs.len() <= 1 {
                assert!(!conflicts[id], "n{id} has one segment and was flagged");
            }
        }
    }

    // ── Round 9, wave 2 (lane regalloc2) ─────────────────────────────

    /// A schedule with `n` empty blocks and exactly `edges` — the
    /// `ir_schedule` tests' `cfg` fixture, rebuilt here because that one is
    /// private to its module.
    fn schedule_of(n: usize, edges: &[(usize, usize)]) -> ir_schedule::Schedule {
        let mut blocks: Vec<ir_schedule::Block> = (0..n)
            .map(|id| ir_schedule::Block {
                id,
                ctrl: NO_NODE,
                nodes: Vec::new(),
                terminator: None,
                successors: Vec::new(),
                predecessors: Vec::new(),
            })
            .collect();
        for &(from, to) in edges {
            blocks[from].successors.push(to);
            blocks[to].predecessors.push(from);
        }
        let dom = ir_schedule::Dominators::compute(&blocks);
        ir_schedule::Schedule {
            blocks,
            node_to_block: Vec::new(),
            dom,
            freq: Default::default(),
            layout: Default::default(),
            pairing: Default::default(),
        }
    }

    /// A `while` whose `continue` jumps straight back to the header has two
    /// latches and is ONE loop. The per-edge model counted the body both
    /// latches reach two levels deep. Port of `ir_schedule`'s
    /// `a_loop_with_two_latches_is_one_level_deep` (irsched request 1).
    #[test]
    fn a_loop_with_two_latches_is_one_level_deep() {
        let schedule = schedule_of(5, &[(0, 1), (1, 2), (2, 3), (3, 1), (2, 1), (1, 4)]);
        assert_eq!(ls_loop_depths(&schedule), vec![0, 1, 1, 1, 0]);
    }

    /// An edge out of a block the entry never reaches is not a back edge,
    /// although `dominates` answers `true` for every dominator of dead code.
    /// Port of `ir_schedule`'s `an_edge_from_dead_code_does_not_make_a_loop`.
    #[test]
    fn an_edge_from_dead_code_does_not_make_a_loop() {
        // 0 -> 1 -> 2 is live; 3 is unreachable and jumps into 1.
        let schedule = schedule_of(4, &[(0, 1), (1, 2), (3, 1)]);
        assert!(!schedule.dom.is_reachable(3));
        assert_eq!(ls_loop_depths(&schedule), vec![0, 0, 0, 0]);
    }

    /// A value whose LAST USE is at a clobber the consumer declared
    /// read-before-clobber keeps its caller-saved register through the call;
    /// one that is still live after the call is split exactly as before; and
    /// with the declaration absent (the default) nothing changes.
    #[test]
    fn a_last_use_at_a_declared_call_keeps_its_register() {
        let build = || {
            let mut f = Fixture::new(8);
            let dies = f.value(Op::Add, IrType::Int, 0, 5, &[3, 5]);
            let (graph, live) = f.finish();
            (graph, live, dies)
        };
        let clobbered = |declare: bool| {
            let mut model = bare_model(gp_volatile_first(1, 0));
            model.add_clobbers(5, &[PhysReg::gp(0)]);
            if declare {
                model.reads_precede_clobber = vec![5];
            }
            model
        };

        // Declared: one segment, through the call.
        let (graph, live, dies) = build();
        let model = clobbered(true);
        let alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        assert_eq!(
            alloc.segments[dies as usize],
            vec![Segment {
                range: PosRange { lo: 0, hi: 5 },
                reg: Some(PhysReg::gp(0)),
            }],
            "the operand is read before the call destroys r0, and nothing reads \
             it again"
        );
        verify_allocation(&graph, &live, &model, &alloc).expect("proof 4 exempts it");

        // The same allocation against the default model is the old violation.
        let err = verify_allocation(&graph, &live, &clobbered(false), &alloc)
            .expect_err("without the declaration a register live at a clobber is refused");
        assert_eq!(
            internal_message(&err),
            "regalloc: a value is live in a clobbered register"
        );

        // Default: the pre-change split.
        let model = clobbered(false);
        let alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        assert!(
            alloc.segments[dies as usize].len() > 1,
            "no declaration, no exemption: {:?}",
            alloc.segments[dies as usize]
        );

        // Declared, but the value is read AGAIN after the call: still split.
        let mut f = Fixture::new(8);
        let survives = f.value(Op::Add, IrType::Int, 0, 6, &[3, 6]);
        let (graph, live) = f.finish();
        let model = clobbered(true);
        let alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        assert!(
            alloc.segments[survives as usize]
                .iter()
                .all(|s| !(s.reg == Some(PhysReg::gp(0)) && s.range.contains(5))),
            "a value live past the call must not keep a register the call \
             destroys: {:?}",
            alloc.segments[survives as usize]
        );
        verify_allocation(&graph, &live, &model, &alloc).expect("verifies");
    }

    /// The admit-everything form (`|_| true`) lists exactly the call positions that
    /// carry a clobber: an FP remainder (a helper call) is listed, an integer
    /// remainder (inline `idiv`) is not, and a call position the model records
    /// no clobber at is not.
    #[test]
    fn marking_call_operand_reads_lists_only_clobbered_call_positions() {
        let mut f = Fixture::new(12);
        let fp_rem = f.value(Op::Rem, IrType::Double, 4, 6, &[6]);
        let _int_rem = f.value(Op::Rem, IrType::Int, 7, 8, &[8]);
        let _unclobbered = f.value(Op::Rem, IrType::Double, 9, 10, &[10]);
        let (graph, live) = f.finish();
        let mut model = bare_model(gp_volatile_first(1, 0));
        model.add_clobbers(4, &[PhysReg::gp(0)]);
        model.add_clobbers(7, &[PhysReg::gp(0)]);
        assert!(model.reads_precede_clobber.is_empty(), "empty by default");
        assert_eq!(
            model.mark_operand_reads_first_where(&graph, &live, |_| true),
            1
        );
        assert_eq!(model.reads_precede_clobber, vec![4]);
        assert_eq!(live.pos_of[fp_rem as usize], Some(4));
        assert_eq!(
            model.mark_operand_reads_first_where(&graph, &live, |_| true),
            0,
            "idempotent"
        );
    }

    /// Round 9 wave 4 (irlower4): the filtered form lists only the call
    /// positions the backend admitted -- here the store, not the FP remainder
    /// -- and keeps the unfiltered form's filters (a non-call and an
    /// unclobbered position are never listed).
    #[test]
    fn marking_admitted_call_operand_reads_lists_only_admitted_ops() {
        let mut f = Fixture::new(12);
        let _fp_rem = f.value(Op::Rem, IrType::Double, 4, 6, &[6]);
        let store = f.value(
            Op::Store(crate::ir::MemKind::Int),
            IrType::Double,
            7,
            8,
            &[8],
        );
        let _add = f.value(Op::Add, IrType::Double, 9, 10, &[10]);
        let (graph, live) = f.finish();
        let mut model = bare_model(gp_volatile_first(1, 0));
        model.add_clobbers(4, &[PhysReg::gp(0)]);
        model.add_clobbers(7, &[PhysReg::gp(0)]);
        model.add_clobbers(9, &[PhysReg::gp(0)]);
        let added =
            model.mark_operand_reads_first_where(&graph, &live, |n| matches!(n.op, Op::Store(_)));
        assert_eq!(added, 1);
        assert_eq!(model.reads_precede_clobber, vec![7]);
        assert_eq!(live.pos_of[store as usize], Some(7));
        // ...and the unfiltered form still adds the remainder on top.
        assert_eq!(
            model.mark_operand_reads_first_where(&graph, &live, |_| true),
            1
        );
        assert_eq!(model.reads_precede_clobber, vec![4, 7]);
    }

    /// Four blocks laid out `latch, entry, header, exit`: the latch (block 0)
    /// comes BEFORE the definitions in block 1, and its back edge `0 -> 2` is
    /// not a fall-through. Spans `0:(0,2) 1:(3,5) 2:(6,9) 3:(10,12)`.
    fn latch_first_fixture() -> (Fixture, ir_schedule::Schedule) {
        let schedule = schedule_of(4, &[(0, 2), (1, 2), (2, 0), (2, 3)]);
        let mut f = Fixture::new(13);
        f.live.span = vec![(0, 2), (3, 5), (6, 9), (10, 12)];
        f.live.block_of_pos = vec![0, 0, 0, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3];
        (f, schedule)
    }

    /// The wave-1 rule compared raw timelines, so a predecessor laid out
    /// before a value's definition "delivered" the register the timeline names
    /// there — a register the layout-order emitter never filled on that path.
    /// It now delivers memory, and the conflict is flagged; and a target laid
    /// out before the definition claims nothing, so it is never flagged.
    #[test]
    fn a_predecessor_laid_out_before_the_definition_delivers_memory() {
        let (mut f, schedule) = latch_first_fixture();
        let v = f.value(Op::Add, IrType::Int, 0, 9, &[4, 7, 9]);
        let w = f.value(Op::Add, IrType::Int, 0, 9, &[8, 9]);
        let (_graph, mut live) = f.finish();
        // `v` is defined at 4 (block 1), `w` at 7 (block 2); both ranges were
        // widened back to 0 by the loop.
        live.pos_of[v as usize] = Some(4);
        live.pos_of[w as usize] = Some(7);
        let seg = |lo: usize, hi: usize, reg: Option<u8>| Segment {
            range: PosRange { lo, hi },
            reg: reg.map(PhysReg::gp),
        };
        let alloc = Allocation {
            segments: vec![
                vec![seg(0, 7, Some(0)), seg(8, 9, None)],
                vec![seg(0, 5, Some(1)), seg(6, 9, Some(2))],
            ],
            stack_slot: vec![Some(0), Some(1)],
            stack_slots: 2,
            ..Default::default()
        };

        let conflicts = split_edge_conflicts(&schedule, &live, &alloc);
        assert!(
            conflicts[v as usize],
            "block 2 was emitted with n{v} in r0 (carried from its definition in \
             block 1); the latch, laid out before that definition, never put it \
             there"
        );
        assert!(
            !conflicts[w as usize],
            "n{w} is defined inside block 2, so nothing carried into block 2 \
             claims a register for it"
        );

        let res = split_edge_resolution(&schedule, &live, &alloc);
        assert_eq!(res.unresolvable, vec![false, false]);
        assert_eq!(
            res.edges,
            vec![EdgeResolution {
                pred: 0,
                succ: 2,
                placement: EdgeCopyPlacement::PredEnd,
                copies: vec![(ValueLoc::Reg(PhysReg::gp(0)), ValueLoc::Slot(0))],
                nodes: vec![v],
            }],
            "the latch has one successor, so the reload goes at its end"
        );
    }

    /// Resolution on the wave-1 back-edge fixture, plus a register exchange
    /// across the back edge (a copy CYCLE, which only a parallel sequencing
    /// gets right) and a value whose target register already belongs to
    /// another value at the top of the loop (unresolvable, copies withdrawn).
    #[test]
    fn edge_resolution_repairs_exactly_the_flagged_values() {
        let mut f = Fixture::new(13);
        let evicted = f.value(Op::Add, IrType::Int, 1, 9, &[4, 9]);
        let a = f.value(Op::Add, IrType::Int, 1, 9, &[4, 9]);
        let b = f.value(Op::Add, IrType::Int, 1, 9, &[4, 9]);
        let blocked = f.value(Op::Add, IrType::Int, 1, 9, &[9]);
        let squatter = f.value(Op::Add, IrType::Int, 3, 9, &[9]);
        let (_graph, mut live) = f.finish();
        live.span = vec![(0, 2), (3, 9), (10, 12)];
        let schedule = self_loop_schedule();

        let seg = |lo: usize, hi: usize, reg: Option<u8>| Segment {
            range: PosRange { lo, hi },
            reg: reg.map(PhysReg::gp),
        };
        let alloc = Allocation {
            segments: vec![
                // evicted: r0 into the loop, memory at the back edge.
                vec![seg(1, 5, Some(0)), seg(6, 9, None)],
                // a and b trade r1 and r2 inside the loop.
                vec![seg(1, 5, Some(1)), seg(6, 9, Some(2))],
                vec![seg(1, 5, Some(2)), seg(6, 9, Some(1))],
                // blocked: r3 up to the loop top, memory after...
                vec![seg(1, 2, Some(3)), seg(3, 9, None)],
                // ...because the squatter holds r3 from the loop top on.
                vec![seg(3, 9, Some(3))],
            ],
            stack_slot: vec![Some(0), Some(1), Some(2), Some(3), None],
            stack_slots: 4,
            ..Default::default()
        };

        let conflicts = split_edge_conflicts(&schedule, &live, &alloc);
        assert_eq!(conflicts, vec![true, true, true, true, false]);

        let res = split_edge_resolution(&schedule, &live, &alloc);
        assert_eq!(
            res.unresolvable,
            vec![false, false, false, true, false],
            "n{blocked}'s copy would overwrite n{squatter}, which the loop top \
             reads from r3"
        );
        assert_eq!(res.edges.len(), 1);
        let edge = &res.edges[0];
        assert_eq!((edge.pred, edge.succ), (1, 1));
        assert_eq!(
            edge.placement,
            EdgeCopyPlacement::Critical,
            "block 1 has two successors and two predecessors"
        );
        let r = |n: u8| ValueLoc::Reg(PhysReg::gp(n));
        assert_eq!(edge.nodes, vec![evicted, a, b]);
        assert_eq!(
            edge.copies,
            vec![(r(0), ValueLoc::Slot(0)), (r(1), r(2)), (r(2), r(1))]
        );

        // Every flagged value is either repaired or refused — the gate and the
        // resolver read one enumeration.
        for (id, &flagged) in conflicts.iter().enumerate() {
            let repaired = res.edges.iter().any(|e| e.nodes.contains(&(id as NodeId)));
            assert_eq!(
                flagged,
                repaired || res.unresolvable[id],
                "n{id}: flagged={flagged} repaired={repaired}"
            );
        }

        // And the copy is a real parallel copy: after sequencing, each register
        // holds what the loop top expects. Tokens: 100+id for a value's home
        // word, and the back-edge register contents as the timeline has them.
        let start = vec![
            (ValueLoc::Slot(0), 100 + evicted),
            (r(2), 100 + a),
            (r(1), 100 + b),
        ];
        let ops = check_parallel(&edge.copies);
        let after = run_copies(&start, &ops);
        assert_eq!(after.get(&r(0)), Some(&(100 + evicted)));
        assert_eq!(after.get(&r(1)), Some(&(100 + a)));
        assert_eq!(after.get(&r(2)), Some(&(100 + b)));
    }

    /// A value the model KNOWS is not live-in at the loop top is not held
    /// against the back edge: nothing past the edge reads it.
    #[test]
    fn a_value_not_live_into_the_target_is_not_an_edge_conflict() {
        let mut f = Fixture::new(13);
        let evicted = f.value(Op::Add, IrType::Int, 1, 9, &[4, 9]);
        let (_graph, mut live) = f.finish();
        live.span = vec![(0, 2), (3, 9), (10, 12)];
        let schedule = self_loop_schedule();
        let alloc = Allocation {
            segments: vec![vec![
                Segment {
                    range: PosRange { lo: 1, hi: 5 },
                    reg: Some(PhysReg::gp(0)),
                },
                Segment {
                    range: PosRange { lo: 6, hi: 9 },
                    reg: None,
                },
            ]],
            stack_slot: vec![Some(0)],
            stack_slots: 1,
            ..Default::default()
        };
        assert!(split_edge_conflicts(&schedule, &live, &alloc)[evicted as usize]);

        // Block 1's live-in set, as a converged model would record it, without
        // `evicted` in it.
        live.block_live_in = vec![vec![0], vec![0], vec![0]];
        assert_eq!(live.live_in_at(1, evicted), Some(false));
        assert!(!split_edge_conflicts(&schedule, &live, &alloc)[evicted as usize]);
        assert_eq!(
            split_edge_resolution(&schedule, &live, &alloc),
            SplitEdgeResolution {
                edges: Vec::new(),
                unresolvable: vec![false],
            }
        );

        // And with it in: flagged again.
        live.block_live_in[1][0] |= 1u64 << evicted;
        assert!(split_edge_conflicts(&schedule, &live, &alloc)[evicted as usize]);
    }

    /// On a real scheduled loop the model keeps per-block live-in sets, and a
    /// loop-carried value is live into the header.
    #[test]
    fn a_converged_model_keeps_per_block_live_in_sets() {
        let (graph, schedule) = swap_loop();
        let live = build_live_model(&graph, &schedule);
        assert!(live.converged);
        assert_eq!(live.block_live_in.len(), schedule.blocks.len());
        let mut phis = 0usize;
        for (id, node) in graph.nodes.iter().enumerate() {
            if !matches!(node.op, Op::Phi) || !live.wants_loc[id] {
                continue;
            }
            let Some(pos) = live.pos_of[id] else { continue };
            let b = live.block_of_pos[pos];
            phis += 1;
            assert_eq!(
                live.live_in_at(b, id as NodeId),
                Some(false),
                "a phi is defined in its own block, not live into it"
            );
        }
        assert!(phis > 0, "the swap loop has loop-header phis");
        // Resolution runs on a real allocation and every copy it proposes is
        // a well-formed parallel copy.
        let model = MachineModel::for_graph(&graph, &schedule, &live, RegFile::x86_64());
        let alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        let res = split_edge_resolution(&schedule, &live, &alloc);
        for edge in &res.edges {
            resolve_parallel_copy(&edge.copies).expect("sequentialises");
        }
    }

    /// Round 11 wave 5: a value live THROUGH a loop it is never read in is
    /// `carried` (its range covers the back edge) but not `carried_and_read`,
    /// so the spill heuristic no longer shields it like a per-iteration value;
    /// the loop's own phis are both.
    #[test]
    fn a_value_live_through_a_loop_it_never_reads_is_not_carried_and_read() {
        // static int f(int a, int n) { int s = a, i = 0;
        //     do { s += i; i++; } while (i < n); return s + a; }
        let code = [
            0x1a, 0x3d, // 0: iload_0; istore_2          s = a
            0x03, 0x3e, // 2: iconst_0; istore_3         i = 0
            0x1c, 0x1d, 0x60, 0x3d, // 4: s += i           loop header
            0x84, 0x03, 0x01, // 8: iinc 3, 1
            0x1d, 0x1b, // 11: iload_3; iload_1
            0xa1, 0xff, 0xf7, // 13: if_icmplt 4
            0x1c, 0x1a, 0x60, 0xac, // 16: return s + a
            0, 0,
        ];
        let mut graph = IrBuilder::new(2, 4)
            .build(&code, 20)
            .expect("the do-while builds");
        // The builder gives every local a header φ; the optimizer drops the
        // trivial one for `a`, so `a` is the parameter itself, as in production.
        crate::ir_optimize::optimize(&mut graph);
        let schedule = ir_schedule::schedule(&graph);
        let live = build_live_model(&graph, &schedule);
        assert!(live.converged);
        let a = graph
            .nodes
            .iter()
            .position(|n| n.op == Op::Param(0))
            .expect("parameter a");
        assert!(
            live.carried[a],
            "fixture: `a` is defined before the loop and read after it, so its \
             range covers the back edge (range {:?})",
            live.range[a]
        );
        assert!(
            !live.carried_and_read[a],
            "`a` is never read inside the loop; evicting it costs one reload, not \
             one per iteration"
        );
        // The loop's own φs (`s`, `i`) are read every iteration. Only "some φ"
        // is asserted: the optimizer may unroll and add exit φs of its own.
        assert!(
            graph
                .nodes
                .iter()
                .enumerate()
                .any(|(id, node)| node.op == Op::Phi && live.carried_and_read[id]),
            "a loop-header φ is carried and read"
        );
        // `carried_and_read` never names a value `carried` does not.
        for id in 0..graph.nodes.len() {
            assert!(!live.carried_and_read[id] || live.carried[id], "n{id}");
        }
    }

    /// `ir_lower::scalar_replacement_field_values` fed to `pin_values` pins exactly `plan_slots`' third
    /// population: every field value, skipping `None` and `NO_NODE`.
    #[test]
    fn scalar_replacement_field_values_are_pinned_from_the_map() {
        let mut f = Fixture::new(12);
        let field = f.value(Op::Add, IrType::Int, 0, 8, &[8]);
        let other = f.value(Op::Add, IrType::Int, 9, 10, &[10]);
        let (graph, mut live) = f.finish();
        let mut sr = crate::ir_lower::ScalarReplacementMap::default();
        sr.objects.insert(
            1000,
            crate::ir_lower::VirtualObjectInfo {
                loop_phis: None,
                class_id: 1,
                array_element_type: None,
                num_fields: 3,
                field_values: vec![Some(field), None, Some(NO_NODE)],
                new_ctrl: NO_NODE,
                store_ctrls: Vec::new(),
                store_nodes: Vec::new(),
                store_fields: Vec::new(),
                non_null_cond: None,
            },
        );
        assert_eq!(
            live.pin_values(crate::ir_lower::scalar_replacement_field_values(&sr)),
            1
        );
        assert!(live.pinned[field as usize]);
        assert!(!live.pinned[other as usize]);
        let model = bare_model(gp(2));
        let alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        assert!(
            !alloc.is_promoted(field),
            "a field value keeps its home word"
        );
        assert!(alloc.is_promoted(other));
    }

    // ── Round 11 wave 2 (lane ircore) ────────────────────────────────────

    /// An early-clobber segment ends AT the `idiv` that destroys its register;
    /// the value's one store must be planned BEFORE that instruction, not at
    /// it (where the register already holds the quotient).
    #[test]
    fn an_early_clobber_segment_is_stored_before_the_clobbering_instruction() {
        let mut f = Fixture::new(12);
        let dividend = f.value(Op::Add, IrType::Int, 0, 9, &[4, 9]);
        let (graph, live) = f.finish();
        let mut model = bare_model(gp(2));
        model.fixed = vec![FixedConstraint {
            node: dividend,
            pos: 4,
            reg: PhysReg::gp(0),
        }];
        model.add_clobbers(4, &[PhysReg::gp(0), PhysReg::gp(1)]);
        let alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        assert_eq!(
            alloc.location_at(dividend, 4),
            Some(ValueLoc::Reg(PhysReg::gp(0))),
            "the fixture must take the early-clobber path"
        );
        let stores: Vec<&SpillEvent> = alloc
            .events
            .iter()
            .filter(|e| e.node == dividend && e.kind == SpillKind::Store)
            .collect();
        assert_eq!(stores.len(), 1, "{:?}", alloc.events);
        assert_eq!(
            (stores[0].pos, stores[0].from),
            (3, Some(PhysReg::gp(0))),
            "the store must run before the idiv at 4 overwrites r0"
        );
        verify_allocation(&graph, &live, &model, &alloc).expect("verifies");
    }

    /// A value DEFINED into its home (it lost the register contest at its
    /// definition) needs no store when a later register segment ends: the
    /// home already holds it. Only its reloads are planned.
    #[test]
    fn a_value_defined_in_memory_is_never_stored() {
        let mut f = Fixture::new(12);
        let w = f.value(Op::Add, IrType::Int, 0, 4, &[2, 4]);
        let v = f.value(Op::Add, IrType::Int, 1, 9, &[6, 9]);
        let (graph, live) = f.finish();
        let mut model = bare_model(gp(1));
        model.add_clobbers(8, &[PhysReg::gp(0)]);
        let alloc = allocate_linear_scan(&graph, &live, &model).expect("allocates");
        let segs = &alloc.segments[v as usize];
        assert_eq!(segs.first().map(|s| s.reg), Some(None), "{segs:?}");
        assert!(
            segs.iter().any(|s| s.reg.is_some()),
            "the value must visit a register for the case to mean anything: {segs:?}"
        );
        assert!(
            segs.windows(2)
                .any(|p| p[0].reg.is_some() && p[1].reg.is_none()),
            "…and leave it again: {segs:?}"
        );
        assert!(
            !alloc
                .events
                .iter()
                .any(|e| e.node == v && e.kind == SpillKind::Store),
            "{:?}",
            alloc.events
        );
        assert_eq!(alloc.first_reg(w), Some(PhysReg::gp(0)));
        verify_allocation(&graph, &live, &model, &alloc).expect("verifies");
    }
