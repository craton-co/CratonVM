    use super::*;
    use crate::ir::IrBuilder;
    use crate::ir_optimize;
    use crate::ir_schedule;
    /// The class id every unbox-accessor test object carries, and the constant
    /// the emitted guard compares against. Any value but `0` will do; `0` is
    /// what the recognizer and the layout resolvers treat as "unresolved".
    #[cfg(test)]
    const UNBOX_TEST_CLASS_ID: u32 = 0x00C0_FFEE;

    /// Build `f(recv[, delta]) = <op>(recv[, delta])` as a graph and lower it.
    ///
    /// Direct graph construction rather than bytecode, for the reason
    /// `compile_scalar_intrinsic` gives: reaching `Op::Unbox` from bytecode
    /// needs the invoke planner, a constant pool AND a resolved receiver class
    /// id, and that machinery is tested where it lives. This is a test of the
    /// emitted instructions.
    ///
    /// `unbox_offsets` derives the two offsets from the class id, so the test
    /// object below must be laid out to match whatever it answers -- which is
    /// why the caller reads them back rather than choosing them.
    #[cfg(test)]
    fn compile_unbox(op: crate::ir::UnboxOp) -> Option<(CompiledMethod, i32, i32)> {
        let (compact_off, legacy_off) = crate::ir::unbox_offsets(op, UNBOX_TEST_CLASS_ID)?;
        let n = 1 + op.arity();
        let mut graph = Graph::empty(0);
        let start = graph.add(Op::Start, IrType::Control, vec![], None);
        graph.entry = start;
        let ctrl = graph.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem = graph.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let recv = graph.add(Op::Param(0), IrType::Ref, vec![start], None);
        let mut inputs = vec![ctrl, mem, recv];
        if op.arity() == 1 {
            // The delta is a `long` for the one family that takes one.
            inputs.push(graph.add(Op::Param(1), IrType::Long, vec![start], None));
        }
        let v = graph.add(
            Op::Unbox {
                op,
                class_id: UNBOX_TEST_CLASS_ID,
            },
            op.result_type(),
            inputs,
            Some(0),
        );
        graph.exit = graph.add(Op::Return, IrType::Void, vec![ctrl, v], Some(1));
        let schedule = ir_schedule::schedule(&graph);
        let cm = lower(&graph, &schedule, n, n, &no_helpers()).expect("the accessor body lowers");
        Some((cm, compact_off, legacy_off))
    }

    /// A 64-byte buffer shaped like an object header, with the class id at
    /// offset 0 and the COMPACT flag set or clear as asked.
    #[cfg(test)]
    fn unbox_test_object(compact: bool) -> Box<[u64; 8]> {
        let mut obj = Box::new([0u64; 8]);
        // SAFETY: `obj` is 64 bytes and 8-byte aligned; the class id is a `u32`
        // at offset 0 and the flags byte is inside the first word, so both
        // writes land well inside it.
        unsafe {
            let p = obj.as_mut_ptr() as *mut u8; // Cast: array base -> byte cursor
            std::ptr::write_unaligned(p as *mut u32, UNBOX_TEST_CLASS_ID); // Cast: header field
            *p.add(cratonvm_types::GC_FLAGS_BYTE_OFFSET) = if compact {
                cratonvm_types::GC_FLAG_COMPACT
            } else {
                0
            };
        }
        obj
    }

    // ── JIT r23 wave 4 stage 1 (lane L8): `UnboxOp::FieldBool` / `FieldRef` ──
    //
    // The two reads the FFM element inline expansion needs, behind
    // `CRATONVM_JIT_IR_FFM_ELEMENT_INLINE` (opt-in, no consumer yet). Executed
    // against a forged receiver of a class whose compact layout this module
    // registers (slot 0 a reference at body offset 8, slot 1 a boolean at 16),
    // in both object layouts, with a fake `JIT_READ_BOUNDS` table as the G1
    // barrier tests use.

    /// The forged class of the FFM element read tests. Registered once
    /// ([`r23w4_ffm_read_layout`]); never re-registered, so it moves no epoch.
    #[cfg(test)]
    const R23W4_FFM_READ_CLASS_ID: u32 = 900_083;

    /// Register the forged class's compact layout once per process.
    #[cfg(test)]
    fn r23w4_ffm_read_layout() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            use cratonvm_types::FieldStorageKind as K;
            cratonvm_types::register_class_layout(
                cratonvm_types::FIRST_LAYOUT_DOMAIN,
                R23W4_FFM_READ_CLASS_ID,
                std::sync::Arc::new(cratonvm_types::CompactLayout {
                    field_disps: vec![8, 16],
                    is_ref: vec![true, false],
                    field_kinds: vec![K::Reference, K::Boolean],
                    ref_disps: vec![8],
                    total_size: 24,
                }),
            );
        });
    }


    /// JIT r23 wave 8 (lane W8-L4, `CRATONVM_JIT_IR_FFM_HEAP_ACCESS_PROVEN`):
    /// lower `f(sma, base, off) = ScopedGet(sma, null session, base, off)`,
    /// with the access filed in `Graph::ffm_proven_heap_accesses` or not.
    #[cfg(test)]
    fn r23w8_compile_scoped_heap_get(proven: bool) -> Option<CompiledMethod> {
        let mut graph = Graph::empty(0);
        let start = graph.add(Op::Start, IrType::Control, vec![], None);
        graph.entry = start;
        let ctrl = graph.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem = graph.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let sma = graph.add(Op::Param(0), IrType::Ref, vec![start], None);
        let base = graph.add(Op::Param(1), IrType::Ref, vec![start], None);
        let off = graph.add(Op::Param(2), IrType::Long, vec![start], None);
        let null = graph.add(Op::Const(0), IrType::Ref, vec![], None);
        let le = graph.add(Op::Const(0), IrType::Int, vec![], None);
        let get = graph.add(
            Op::Unbox {
                op: crate::ir::UnboxOp::ScopedGet {
                    width: 4,
                    signed: true,
                    ordered: true,
                },
                class_id: 0,
            },
            IrType::Int,
            vec![ctrl, mem, sma, null, base, off, le],
            Some(0),
        );
        graph.exit = graph.add(Op::Return, IrType::Void, vec![ctrl, get], Some(1));
        graph.safepoints.push(crate::ir::SafepointSnapshot::new(
            0,
            vec![sma, base, off],
            Vec::new(),
            Vec::new(),
        ));
        if proven {
            graph.ffm_proven_heap_accesses.insert(get);
        }
        let schedule = ir_schedule::schedule(&graph);
        lower(&graph, &schedule, 3, 3, &no_helpers())
    }

    /// JIT r23 wave 9 (lane L4, `CRATONVM_JIT_IR_FFM_HEAP_SESSION_HOIST`): a
    /// proven heap access over a session `Param(3)`, the session filed in
    /// `Graph::ffm_never_closing_scopes` or not.
    #[cfg(test)]
    fn r23w9_compile_scoped_get_with_session(never_closing: bool) -> Option<CompiledMethod> {
        let mut graph = Graph::empty(0);
        let start = graph.add(Op::Start, IrType::Control, vec![], None);
        graph.entry = start;
        let ctrl = graph.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem = graph.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let sma = graph.add(Op::Param(0), IrType::Ref, vec![start], None);
        let base = graph.add(Op::Param(1), IrType::Ref, vec![start], None);
        let off = graph.add(Op::Param(2), IrType::Long, vec![start], None);
        let session = graph.add(Op::Param(3), IrType::Ref, vec![start], None);
        let le = graph.add(Op::Const(0), IrType::Int, vec![], None);
        let get = graph.add(
            Op::Unbox {
                op: crate::ir::UnboxOp::ScopedGet {
                    width: 4,
                    signed: true,
                    ordered: true,
                },
                class_id: 0,
            },
            IrType::Int,
            vec![ctrl, mem, sma, session, base, off, le],
            Some(0),
        );
        graph.exit = graph.add(Op::Return, IrType::Void, vec![ctrl, get], Some(1));
        graph.safepoints.push(crate::ir::SafepointSnapshot::new(
            0,
            vec![sma, base, off, session],
            Vec::new(),
            Vec::new(),
        ));
        graph.ffm_proven_heap_accesses.insert(get);
        if never_closing {
            graph.ffm_never_closing_scopes.insert(session);
        }
        let schedule = ir_schedule::schedule(&graph);
        lower(&graph, &schedule, 4, 4, &no_helpers())
    }

    /// JIT round 24 B2 (`CRATONVM_JIT_IR_FFM_ARENA_CACHE`): lower a native
    /// `ScopedGet(sma, session, null, min + delta)` with or without the
    /// loop-entry arena cache inputs (`Graph::ffm_arena_cached`).
    #[cfg(test)]
    fn r24_b2_compile_native_get(cached: bool) -> Option<CompiledMethod> {
        use crate::ir::UnboxOp as U;
        let mut graph = Graph::empty(0);
        let start = graph.add(Op::Start, IrType::Control, vec![], None);
        graph.entry = start;
        let ctrl = graph.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem = graph.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let sma = graph.add(Op::Param(0), IrType::Ref, vec![start], None);
        let min = graph.add(Op::Param(1), IrType::Long, vec![start], None);
        let delta = graph.add(Op::Param(2), IrType::Long, vec![start], None);
        let session = graph.add(Op::Param(3), IrType::Ref, vec![start], None);
        let null = graph.add(Op::Const(0), IrType::Ref, vec![], None);
        let le = graph.add(Op::Const(0), IrType::Int, vec![], None);
        let addr = graph.add(Op::Add, IrType::Long, vec![min, delta], Some(0));
        let mut inputs = vec![ctrl, mem, sma, session, null, addr, le];
        if cached {
            let span = graph.add(Op::Const(256), IrType::Long, vec![], None);
            let km = graph.add(Op::Const(!((1i64 << 30) - 1)), IrType::Long, vec![], None);
            let key = graph.add(Op::And, IrType::Long, vec![min, km], Some(0));
            let entry = graph.add(
                Op::Unbox {
                    op: U::ArenaCacheEntry,
                    class_id: 0,
                },
                IrType::Long,
                vec![ctrl, mem, min, span, session],
                Some(0),
            );
            let base = graph.add(
                Op::Unbox {
                    op: U::ArenaCacheBase,
                    class_id: 0,
                },
                IrType::Long,
                vec![ctrl, mem, entry, min],
                Some(0),
            );
            inputs.extend([entry, key, base, delta]);
        }
        let get = graph.add(
            Op::Unbox {
                op: U::ScopedGet {
                    width: 4,
                    signed: true,
                    ordered: true,
                },
                class_id: 0,
            },
            IrType::Int,
            inputs,
            Some(0),
        );
        if cached {
            graph.ffm_arena_cached.insert(get);
        }
        graph.exit = graph.add(Op::Return, IrType::Void, vec![ctrl, get], Some(1));
        graph.safepoints.push(crate::ir::SafepointSnapshot::new(
            0,
            vec![sma, min, delta, session],
            Vec::new(),
            Vec::new(),
        ));
        let schedule = ir_schedule::schedule(&graph);
        lower(&graph, &schedule, 4, 4, &no_helpers())
    }



    /// A 64-byte receiver of the forged class, COMPACT flag as asked.
    #[cfg(test)]
    fn r23w4_ffm_read_object(compact: bool) -> Box<[u64; 8]> {
        let mut obj = Box::new([0u64; 8]);
        // SAFETY: as in `unbox_test_object`: both writes land in the first word.
        unsafe {
            let p = obj.as_mut_ptr() as *mut u8; // Cast: array base -> byte cursor
            std::ptr::write_unaligned(p as *mut u32, R23W4_FFM_READ_CLASS_ID); // Cast: header field
            *p.add(cratonvm_types::GC_FLAGS_BYTE_OFFSET) = if compact {
                cratonvm_types::GC_FLAG_COMPACT
            } else {
                0
            };
        }
        obj
    }


    // ── Increment 2: the machine list emits ───────────────────────────
    //
    // The oracle is byte equality on a corpus: compile a method both ways and
    // compare the emitted bytes. Every `PATTERNS` row carries that property per
    // row — it names the hand-written emitter it reproduces — and these are the
    // same check at method scale.

    /// `int f(int a, int b) { return (a + b) * a; }` — `iload_0; iload_1;
    /// iadd; iload_0; imul; ireturn`, and the length is **6**, the whole
    /// method. (The neighbouring shadow tests pass 4, which truncates after the
    /// `iload_0` and leaves the multiply out of the graph entirely; that costs
    /// them nothing because they only count, but it would make every assertion
    /// below about `Rule::AluReg` vacuous.)
    #[cfg(test)]
    const MIR_ALU_CODE: [u8; 8] = [0x1a, 0x1b, 0x60, 0x1a, 0x68, 0xac, 0, 0];

    /// [`MIR_ALU_CODE`] as a graph and a schedule, for the tests that work on
    /// the machine list directly instead of on the emitted bytes.
    #[cfg(test)]
    fn mir_alu_graph() -> (Graph, Schedule) {
        let builder = IrBuilder::new(2, 2);
        let graph = builder.build(&MIR_ALU_CODE, 6).expect("build");
        let schedule = ir_schedule::schedule(&graph);
        (graph, schedule)
    }


    // ── The callee-saved XMM save area ───────────────────────────────
    //
    // `IR_LOWER_SAVED_XMMS` is the prerequisite the level-2 lane was blocked
    // on, and its failure mode is the quietest one in this file: a caller's
    // XMM6 comes back holding a callee's `double`, on Windows only, with no
    // diagnostic anywhere. Nothing downstream checks it — the GC skips the
    // band by design, the deopt verifier skips it, and the value is a `double`
    // so it never looks like a bad pointer. So it is checked here, on the
    // emitted bytes, at every exit.

    /// Build a real `Lowerer` over a trivial graph, with a residency plan that
    /// names `reg`.
    ///
    /// The plan is synthesised rather than allocated, because what is under
    /// test is the *prologue's* reaction to a plan, not the allocator: a test
    /// that had to find a graph with five simultaneously live doubles would
    /// stop testing the save area the moment the allocator's heuristics moved.
    #[cfg(test)]
    pub(super) fn lowerer_with_resident_xmm(buf_cap: usize, reg: Option<u8>) -> Lowerer<'static> {
        // Leaked so the returned `Lowerer<'static>` can borrow them; a handful
        // of small allocations per test, in a process that is about to exit.
        let (graph, schedule) = add_one_graph();
        let graph: &'static Graph = Box::leak(Box::new(graph));
        let schedule: &'static Schedule = Box::leak(Box::new(schedule));
        let plan: &'static SlotPlan = Box::leak(Box::new(plan_slots(graph, schedule, None)));
        let mut lowerer = lowerer_over(graph, schedule, plan, buf_cap);
        if let Some(reg) = reg {
            lowerer.set_residency(RegResidency {
                reg_of: vec![Some(reg)],
                // This helper drives the XMM half; the GP file is exercised by
                // its own tests.
                gp_reg_of: Vec::new(),
                promoted: 1,
                demoted: 0,
                peak_live: 1,
                // No split value in this fixture, so the emission never calls
                // `ls_apply_transitions` and residency stays whole-range.
                splits: LsSplitPlan::default(),
                // No phi in this fixture's graph, so every field is zero and
                // the accounting identity holds trivially.
                phi_census: PhiCensus::default(),
                poll_fp_reloads: HashMap::new(),
                caller_saved_republish: Vec::new(),
            });
        }
        lowerer
    }

    /// A `Lowerer` over any graph, with no residency installed.
    ///
    /// The half of `lowerer_with_resident_xmm` that is about construction
    /// rather than about a plan, factored out because the split-residency tests
    /// need their own fixture graph — a value that is live across a call — and
    /// a second copy of these fifteen arguments is a second thing to update
    /// when `Lowerer::new` grows one.
    #[cfg(test)]
    pub(super) fn lowerer_over(
        graph: &'static Graph,
        schedule: &'static Schedule,
        plan: &'static SlotPlan,
        buf_cap: usize,
    ) -> Lowerer<'static> {
        let empty: &'static HashMap<usize, bool> = Box::leak(Box::new(HashMap::new()));
        let no_direct: &'static HashMap<usize, (usize, bool)> = Box::leak(Box::new(HashMap::new()));
        let no_ic: &'static HashMap<usize, (usize, usize)> = Box::leak(Box::new(HashMap::new()));
        let no_compact: HashMap<(usize, bool), (u32, bool, u8)> = HashMap::new();
        let no_frame_sites: &'static crate::ir::IrInlineFrameSites =
            Box::leak(Box::new(crate::ir::IrInlineFrameSites::default()));
        let helpers: &'static JitRuntimeHelpers = Box::leak(Box::new(no_helpers()));
        let buf = ExecutableBuffer::new(buf_cap).expect("executable buffer");
        Lowerer::new(
            graph,
            schedule,
            buf,
            0,
            2,
            plan,
            helpers,
            empty,
            &[],
            None,
            no_direct,
            no_ic,
            &no_compact,
            no_frame_sites,
        )
    }


    /// `MOVUPS [rbp - disp], xmm` and its load counterpart, for `reg < 8`.
    ///
    /// **Both** displacement forms, because `emit_rbp_modrm_disp` picks the
    /// smallest legal one: `mod=10` (`0x85 | reg<<3`, disp32) for a deep frame
    /// and `mod=01` (`0x45 | reg<<3`, disp8) for a shallow one. A needle
    /// spelling only one of them silently counts zero on the other, which reads
    /// as "the restore is missing" — the exact failure this test exists to
    /// report, arriving for the wrong reason.
    #[cfg(test)]
    fn movups_frame_needles(reg: u8, store: bool) -> [[u8; 3]; 2] {
        let op = if store { 0x11 } else { 0x10 };
        [
            [0x0F, op, 0x85 | ((reg & 7) << 3)],
            [0x0F, op, 0x45 | ((reg & 7) << 3)],
        ]
    }

    /// Occurrences of a frame `MOVUPS` for `reg` in `code`, either
    /// displacement form. See [`movups_frame_needles`].
    #[cfg(test)]
    fn count_movups_frame(code: &[u8], reg: u8, store: bool) -> usize {
        movups_frame_needles(reg, store)
            .iter()
            .map(|n| count_seq(code, n))
            .sum()
    }

    // ── R14: every XMM encoder reaches XMM8 ──────────────────────────

    /// The bytes one emitter produces for one XMM number, in isolation.
    #[cfg(test)]
    fn xmm_encoding(reg: u8, emit: &dyn Fn(&mut Lowerer<'static>, u8)) -> Vec<u8> {
        let mut lo = lowerer_with_resident_xmm(4096, None);
        let start = lo.buf.pos();
        emit(&mut lo, reg);
        lo.buf.as_slice()[start..].to_vec()
    }

    /// Build a lowerer whose GP residency file holds exactly `reg`.
    ///
    /// `lowerer_with_resident_xmm` covers the other bank; this one is what
    /// makes `saved_gpr_regs` non-empty (it filters on `gp_reg_of`) and what
    /// gives `set_residency` something to compute `deopt_nameable` from.
    #[cfg(test)]
    pub(super) fn lowerer_with_resident_gpr(buf_cap: usize, reg: u8) -> Lowerer<'static> {
        let mut lo = lowerer_with_resident_xmm(buf_cap, None);
        let n = lo.graph.nodes.len().max(1);
        let mut gp_reg_of = vec![None; n];
        gp_reg_of[0] = Some(reg);
        lo.set_residency(RegResidency {
            reg_of: vec![None; n],
            gp_reg_of,
            promoted: 1,
            demoted: 0,
            peak_live: 0,
            splits: LsSplitPlan::default(),
            phi_census: PhiCensus::default(),
            poll_fp_reloads: HashMap::new(),
            caller_saved_republish: Vec::new(),
        });
        lo
    }

    // ── R4: split residency (`CRATONVM_JIT_IR_LS_SPLITS`) ────────────
    //
    // What these hold up, in one sentence each: a value the allocator split
    // keeps a register on BOTH sides of the thing that split it; a reference it
    // reloads is still dropped wherever a collector could have run; several
    // moves at one position are a parallel copy and are ordered like one; and
    // everything this file cannot prove falls back to no register rather than
    // to a guess.
    //
    // The allocations below are HAND-BUILT rather than produced by
    // `allocate_linear_scan`, deliberately. What is under test is this file's
    // consumption of a split, and a test that had to coax the scan's heuristics
    // into splitting one particular value would stop testing the consumer the
    // moment those heuristics moved -- which is how the `split_or_spilled=5`
    // miscount recorded in `plan_register_residency` came to be believed.

    // The GP linear-scan file, by encoding number. `ir_lower` names RAX, RCX,
    // RDX, R10 and R11 at module scope and leaves the allocatable file to
    // `IR_LOWER_LS_GPRS`, which is a slice and not named registers -- so the
    // three these tests drive are spelled here once.
    #[cfg(test)]
    const RBX: u8 = 3;
    #[cfg(test)]
    const R12: u8 = 12;
    #[cfg(test)]
    const R13: u8 = 13;

    /// `int f(int a) { int s = a + 1; return s + g(s); }` -- one block, and `s`
    /// is live ACROSS the call because it is both an argument to it and an
    /// operand of the add that consumes its result.
    ///
    /// Returns the two ids the tests name: the value that gets split, and the
    /// call that splits it.
    #[cfg(test)]
    fn value_live_across_a_call_graph() -> (Graph, Schedule, NodeId, NodeId) {
        let mut graph = Graph::empty(0);
        let start = graph.add(Op::Start, IrType::Control, vec![], None);
        graph.entry = start;
        let ctrl = graph.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem = graph.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let a = graph.add(Op::Param(0), IrType::Int, vec![start], None);
        let one = graph.add(Op::Const(1), IrType::Int, vec![], None);
        let sum = graph.add(Op::Add, IrType::Int, vec![a, one], None);
        // `[ctrl, mem, args…]`, so `sum` is the one Java argument -- which is
        // also what forces the schedule to place `sum` BEFORE the call rather
        // than leaving the order to the scheduler's tie-breaks.
        let call = graph.add(
            Op::Call { info_ptr: 0 },
            IrType::Int,
            vec![ctrl, mem, sum],
            None,
        );
        let total = graph.add(Op::Add, IrType::Int, vec![sum, call], None);
        graph.exit = graph.add(Op::Return, IrType::Void, vec![ctrl, total], None);
        let schedule = ir_schedule::schedule(&graph);
        (graph, schedule, sum, call)
    }

    /// The same shape with a REFERENCE in the middle: `a` is an argument to one
    /// call and still read by a second, so it is live across a point where a
    /// collector can run and a copy of it in a register is a copy of something
    /// evacuation moves.
    #[cfg(test)]
    fn reference_live_across_a_call_graph() -> (Graph, Schedule, NodeId, NodeId) {
        let mut graph = Graph::empty(0);
        let start = graph.add(Op::Start, IrType::Control, vec![], None);
        graph.entry = start;
        let ctrl = graph.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem = graph.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let a = graph.add(Op::Param(0), IrType::Ref, vec![start], None);
        let first = graph.add(
            Op::Call { info_ptr: 0 },
            IrType::Int,
            vec![ctrl, mem, a],
            None,
        );
        // A call node IS the new memory token, so the second call chains off
        // the first -- and reads `a` again, which is what keeps `a` live across
        // the first one.
        let second = graph.add(
            Op::Call { info_ptr: 0 },
            IrType::Int,
            vec![ctrl, first, a],
            None,
        );
        graph.exit = graph.add(Op::Return, IrType::Void, vec![ctrl, second], None);
        let schedule = ir_schedule::schedule(&graph);
        (graph, schedule, a, first)
    }

    /// `value` in `reg` up to the position before `cut`, in its home word AT
    /// `cut`, and back in `reg` from `cut + 1` -- the timeline a call produces
    /// for a value in a caller-saved register, and the one the scan's own
    /// splitting step writes.
    #[cfg(test)]
    fn split_allocation(
        n: usize,
        value: NodeId,
        range: crate::regalloc::PosRange,
        cut: usize,
        reg: crate::regalloc::PhysReg,
    ) -> crate::regalloc::Allocation {
        use crate::regalloc::{Allocation, PosRange, Segment, SpillEvent, SpillKind};
        let mut alloc = Allocation {
            segments: vec![Vec::new(); n],
            stack_slot: vec![None; n],
            ..Allocation::default()
        };
        alloc.segments[value as usize] = vec![
            Segment {
                range: PosRange {
                    lo: range.lo,
                    hi: cut - 1,
                },
                reg: Some(reg),
            },
            Segment {
                range: PosRange { lo: cut, hi: cut },
                reg: None,
            },
            Segment {
                range: PosRange {
                    lo: cut + 1,
                    hi: range.hi,
                },
                reg: Some(reg),
            },
        ];
        alloc.events = vec![
            SpillEvent {
                node: value,
                pos: cut - 1,
                kind: SpillKind::Store,
                from: Some(reg),
                to: None,
            },
            SpillEvent {
                node: value,
                pos: cut + 1,
                kind: SpillKind::Load,
                from: None,
                to: Some(reg),
            },
        ];
        alloc.spills = 1;
        alloc.reloads = 1;
        alloc.splits = 1;
        alloc.promoted = 1;
        alloc
    }

    /// Install `transitions` for `value` on a lowerer over `graph`, exactly as
    /// `plan_register_residency` would, and give the value its home word and
    /// its definition-site publish.
    #[cfg(test)]
    fn lowerer_with_split(
        graph: &'static Graph,
        schedule: &'static Schedule,
        plan: &'static SlotPlan,
        live: &crate::regalloc::LiveModel,
        value: NodeId,
        first: Option<u8>,
        transitions: Vec<(usize, LsTransition)>,
    ) -> Lowerer<'static> {
        let n = graph.nodes.len();
        let at =
            ls_sequence_split_plan(&[(value as usize, true, transitions)], live.total_positions)
                .expect("the fixture's transitions sit at distinct positions");
        let mut managed = vec![false; n];
        managed[value as usize] = true;
        let mut gp_reg_of = vec![None; n];
        gp_reg_of[value as usize] = first;
        let mut lo = lowerer_over(graph, schedule, plan, 4096);
        lo.set_residency(RegResidency {
            reg_of: vec![None; n],
            gp_reg_of,
            promoted: 1,
            demoted: 0,
            peak_live: live.peak_live,
            splits: LsSplitPlan {
                block_start: live.span.iter().map(|&(start, _)| start).collect(),
                managed,
                values: 1,
                transitions: at.iter().map(|v| v.len()).sum(),
                at,
                edges: Vec::new(),
            },
            phi_census: PhiCensus::default(),
            poll_fp_reloads: HashMap::new(),
            caller_saved_republish: Vec::new(),
        });
        lo.alloc_slot_checked(value)
            .expect("the split value has a planned home word");
        // The definition's own write-through publish, which is what `first`
        // describes and what every transition after it builds on.
        lo.mark_gp_reg_live(value);
        lo
    }




mod compile_via_ir_no_opt;
#[allow(unused_imports)]
pub use compile_via_ir_no_opt::*;
#[allow(unused_imports)]
use compile_via_ir_no_opt::*;
mod virtual_call_with_ic;
#[allow(unused_imports)]
pub use virtual_call_with_ic::*;
#[allow(unused_imports)]
use virtual_call_with_ic::*;
mod an_osr_entry_publishes_the_kind_of;
#[allow(unused_imports)]
pub use an_osr_entry_publishes_the_kind_of::*;
#[allow(unused_imports)]
use an_osr_entry_publishes_the_kind_of::*;
mod gcg_g3v_an_ir_osr_entry_stub_saves;
#[allow(unused_imports)]
pub use gcg_g3v_an_ir_osr_entry_stub_saves::*;
#[allow(unused_imports)]
use gcg_g3v_an_ir_osr_entry_stub_saves::*;
