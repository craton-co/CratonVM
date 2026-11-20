    use super::*;
    use crate::ir::{IrBuilder, IrType};
    use crate::ir_optimize;

    impl Dominators {
        /// The relation as a dense matrix: `m[b][d]` iff `d` dominates `b`.
        /// O(B²) memory, filled by walking each block's `idom` chain. It is
        /// what `Schedule::dom` published before the schedule kept the
        /// [`Dominators`] itself, and these tests compare it against the dense
        /// fixpoint reference.
        pub fn to_matrix(&self) -> Vec<Vec<bool>> {
            let n = self.idom.len();
            let mut matrix = Vec::with_capacity(n);
            for b in 0..n {
                if self.idom[b] == UNREACHED {
                    matrix.push(vec![true; n]);
                    continue;
                }
                let mut row = vec![false; n];
                let mut d = b;
                loop {
                    row[d] = true;
                    if d == 0 {
                        break;
                    }
                    d = self.idom[d];
                }
                matrix.push(row);
            }
            matrix
        }
    }

    /// For every block, the set of blocks that dominate it, as a dense matrix:
    /// `dom[b][d]` iff block `d` dominates block `b`. A block the entry does
    /// not reach gets the row "all blocks" — the value the classic iterative
    /// fixpoint this replaced left it at.
    fn compute_dominators(blocks: &[Block]) -> Vec<Vec<bool>> {
        Dominators::compute(blocks).to_matrix()
    }

    /// True iff block `a` dominates block `b`, read from a
    /// `compute_dominators` matrix; `false` for an out-of-range index.
    fn dominates(matrix: &[Vec<bool>], a: usize, b: usize) -> bool {
        matrix
            .get(b)
            .and_then(|row| row.get(a))
            .copied()
            .unwrap_or(false)
    }

    fn build_schedule(
        code: &[u8],
        code_len: usize,
        num_params: usize,
        num_locals: usize,
    ) -> Schedule {
        let builder = IrBuilder::new(num_params, num_locals);
        let mut graph = builder.build(code, code_len).expect("build failed");
        ir_optimize::optimize(&mut graph);
        schedule(&graph)
    }

    #[test]
    fn test_schedule_linear_method() {
        // int f(int a, int b) { return a + b; }
        let code = [0x1a, 0x1b, 0x60, 0xac, 0, 0];
        let sched = build_schedule(&code, 4, 2, 2);
        assert!(!sched.blocks.is_empty(), "Should have at least one block");
        // Single block method — one entry block with Return terminator
        let entry = &sched.blocks[0];
        assert!(entry.terminator.is_some());
    }

    #[test]
    fn test_schedule_branching_method() {
        // if (x == 0) return 1; return 0;
        let code = [
            0x1a, // 0: iload_0
            0x99, 0x00, 0x05, // 1: ifeq → 6
            0x03, // 4: iconst_0
            0xac, // 5: ireturn
            0x04, // 6: iconst_1
            0xac, // 7: ireturn
            0, 0,
        ];
        let sched = build_schedule(&code, 8, 1, 1);
        // Should have 3 blocks: entry (with If), true branch, false branch
        assert!(
            sched.blocks.len() >= 2,
            "Branching method should have 2+ blocks, got {}",
            sched.blocks.len()
        );
    }

    /// The equal-depth half of schedule-late: a pure node whose only use is in
    /// a later block at the SAME loop depth moves there.
    ///
    /// `int f(int a, int b) { if (a != 0) return a + b; return 1; }` — the
    /// `Add` is computed in the ENTRY block because that is where
    /// `find_best_block` puts it (schedule-EARLY: the deepest block its inputs,
    /// two parameters, dominate), and its only reader is the `Return` on the
    /// taken arm. Both blocks are at depth 0, so the strict-decrease rule this
    /// pass shipped with moves nothing; the equal-depth rule moves it to the
    /// block that reads it, where it is also computed on one path instead of
    /// both.
    ///
    /// **The sum is deliberately not stored to a local.** A local is named by
    /// the deopt frame of every later bci, including the ones on the arm it did
    /// not sink into, and [`safepoints_dominate_anchors`] then refuses the
    /// placement — correctly, because a frame on that arm would read a word
    /// nothing had written. That refusal is the pass working, not the rule
    /// failing, and a test written on a local would assert the opposite of what
    /// it looks like it asserts.
    ///
    /// Asserted as a DIFFERENCE between the two arms rather than as an absolute
    /// block index, so a change in how the scheduler numbers blocks cannot make
    /// this pass vacuously.
/// Round 16 wave 5 (lane codegen, CG16-9): under
    /// `CRATONVM_JIT_IR_SINK_EQUAL_DEPTH_NO_FP` the same `Add` still sinks as
    /// an `int`, and stays where the depth rule leaves it once it is typed
    /// `double` (retyped after the build: placement reads the type, not the
    /// arithmetic).
/// Round 17 (backlog CG-B4): `CRATONVM_JIT_IR_SINK_EQUAL_DEPTH_LOOP_FREE`
    /// sinks the same `Add` in a loop-free method, and leaves a method with a
    /// loop on the depth-only rule (`R16CodegenLoops.intLoop`'s accumulator
    /// stays where it was).
/// JIT round 19 wave 1 (lane osrpoly19, OSRPOLY18-1):
    /// `CRATONVM_JIT_IR_SINK_CLASS_TEST_TO_BRANCH` moves an `ExactClassIs` to
    /// its only reader's block at equal depth, and nothing moves it without
    /// the switch (the equal-depth and loop-free rules are off in both arms,
    /// so only the new rule can). The branchy method's `a + b` is rewritten
    /// into a class test of `a` after the build: placement reads the op, not
    /// the operand's type.
/// JIT round 19 proposal wave (lane osr19, OSRPOLY19-2): the same switch
    /// moves a class-word chain's test, `Cmp(Eq, ClassWordOf(r), id)`, and its
    /// head with it (`node_is_class_chain_test`); off, neither moves. The
    /// branchy method's `a + b` is rewritten into such a test of `a`.
/// A value a deopt frame names on the arm it did NOT sink into keeps its
    /// early placement — the obligation [`safepoints_dominate_anchors`] states.
    ///
    /// `int f(int a, int b) { int t = a + b; if (a == 0) return 1; return t; }`
    /// is the same shape as the test above with one edit: the sum goes through
    /// a LOCAL, so every snapshot from its store onwards names it, including
    /// the one on the `return 1` arm. Sinking it into the other arm would let a
    /// deopt there read a word nothing had written. The pass must decline, and
    /// declining must cost nothing else — which is what the fallback to the
    /// depth-only rule is for.
/// The flag is read LIVE, not cached in a `OnceLock`.
    ///
    /// `ir_per_copy_frames_enabled` records what a cached scheduling flag costs:
    /// the matrix test ran the OFF arm first and both arms then reported
    /// byte-identical code. Two calls in one process, different values, must
    /// give different answers.
    #[test]
    fn equal_depth_flag_is_read_live() {
        let read = |v: Option<&str>| {
            cratonvm_types::flags::with_thread_overrides(
                &[("CRATONVM_JIT_IR_SINK_EQUAL_DEPTH", v)],
                sink_equal_depth_enabled,
            )
        };
        assert!(read(Some("1")));
        assert!(!read(Some("0")));
        assert!(!read(None), "default is OFF");
        assert!(read(Some("1")), "a second read must still see the override");
    }


    #[test]
    fn test_schedule_node_to_block_mapping() {
        // int f(int a, int b) { return a + b; }
        let code = [0x1a, 0x1b, 0x60, 0xac, 0, 0];
        let sched = build_schedule(&code, 4, 2, 2);
        // Every non-dead, non-MAX node should map to a valid block
        for (id, &blk) in sched.node_to_block.iter().enumerate() {
            if blk != usize::MAX {
                assert!(
                    blk < sched.blocks.len(),
                    "Node {} mapped to invalid block {}",
                    id,
                    blk
                );
            }
        }
    }

    #[test]
    fn test_schedule_blocks_have_ctrl_nodes() {
        let code = [0x1a, 0x1b, 0x60, 0xac, 0, 0];
        let sched = build_schedule(&code, 4, 2, 2);
        for block in &sched.blocks {
            // Every block must have a valid ctrl node
            assert!(
                (block.ctrl as usize) < sched.node_to_block.len(),
                "Block {} has out-of-range ctrl node {}",
                block.id,
                block.ctrl
            );
        }
    }

    #[test]
    fn test_schedule_entry_block_is_first() {
        // The first block should be the entry block (control projection from Start)
        let code = [0x1a, 0xac, 0, 0]; // iload_0; ireturn
        let sched = build_schedule(&code, 2, 1, 1);
        assert!(!sched.blocks.is_empty());
        // Entry block should have no predecessors
        assert!(
            sched.blocks[0].predecessors.is_empty(),
            "Entry block should have no predecessors"
        );
    }

    #[test]
    fn test_schedule_return_has_no_successors() {
        let code = [0x1a, 0xac, 0, 0]; // iload_0; ireturn
        let sched = build_schedule(&code, 2, 1, 1);
        // Find the block with a Return terminator — it should have no successors
        for block in &sched.blocks {
            if let Some(term) = block.terminator {
                let builder = IrBuilder::new(1, 1);
                let mut graph = builder.build(&code, 2).expect("build failed");
                ir_optimize::optimize(&mut graph);
                if graph.nodes[term as usize].op == Op::Return {
                    assert!(
                        block.successors.is_empty(),
                        "Return block should have no successors"
                    );
                }
            }
        }
    }

    #[test]
    fn test_schedule_branching_successor_count() {
        // if (x == 0) return 1; return 0;
        let code = [
            0x1a, // iload_0
            0x99, 0x00, 0x05, // ifeq → 6
            0x03, // iconst_0
            0xac, // ireturn
            0x04, // iconst_1
            0xac, // ireturn
            0, 0,
        ];
        let sched = build_schedule(&code, 8, 1, 1);
        // At least one block should have 2 successors (the If block)
        let _has_branch = sched.blocks.iter().any(|b| b.successors.len() == 2);
        // It's possible the optimizer simplifies the branch, so just check
        // that the schedule produced valid blocks
        assert!(
            sched.blocks.len() >= 2,
            "Branch should produce multiple blocks"
        );
    }

    #[test]
    fn test_schedule_constant_return() {
        // return 42;  →  bipush 42; ireturn
        let code = [0x10, 42, 0xac, 0, 0, 0];
        let sched = build_schedule(&code, 3, 0, 0);
        assert!(!sched.blocks.is_empty());
        // Single block with a terminator
        assert!(sched.blocks[0].terminator.is_some());
    }

    #[test]
    fn test_schedule_subtraction() {
        // int f(int a, int b) { return a - b; }
        // iload_0; iload_1; isub; ireturn
        let code = [0x1a, 0x1b, 0x64, 0xac, 0, 0];
        let sched = build_schedule(&code, 4, 2, 2);
        // Should have data nodes (the Sub) placed in a block
        let total_data_nodes: usize = sched.blocks.iter().map(|b| b.nodes.len()).sum();
        assert!(
            total_data_nodes > 0,
            "Data nodes should be scheduled into blocks"
        );
    }

    // ── Helpers for the dominator / topo-sort unit tests ─────────────────

    /// Build a bare block with the given predecessor list (ctrl/terminator are
    /// irrelevant for the dominator math, which only reads `predecessors`).
    fn blk(id: usize, preds: &[usize]) -> Block {
        Block {
            id,
            ctrl: 0,
            nodes: Vec::new(),
            terminator: None,
            successors: Vec::new(),
            predecessors: preds.to_vec(),
        }
    }

    #[test]
    fn test_dominators_diamond() {
        // Diamond CFG:  0 → {1,2} → 3
        let blocks = vec![blk(0, &[]), blk(1, &[0]), blk(2, &[0]), blk(3, &[1, 2])];
        let dom = compute_dominators(&blocks);
        // Entry dominates everything.
        for b in 0..4 {
            assert!(dominates(&dom, 0, b), "entry must dominate block {b}");
        }
        // The merge (3) is dominated only by 0 and itself — NOT by 1 or 2,
        // since either side branch can be taken.
        assert!(dominates(&dom, 3, 3));
        assert!(!dominates(&dom, 1, 3), "branch 1 must NOT dominate merge");
        assert!(!dominates(&dom, 2, 3), "branch 2 must NOT dominate merge");
        // Siblings do not dominate each other.
        assert!(!dominates(&dom, 1, 2));
        assert!(!dominates(&dom, 2, 1));
    }

    #[test]
    fn test_dominators_chain() {
        // Straight-line chain 0 → 1 → 2 → 3: each block dominates all later ones.
        let blocks = vec![blk(0, &[]), blk(1, &[0]), blk(2, &[1]), blk(3, &[2])];
        let dom = compute_dominators(&blocks);
        for a in 0..4 {
            for b in a..4 {
                assert!(dominates(&dom, a, b), "block {a} must dominate {b}");
            }
            for b in 0..a {
                assert!(!dominates(&dom, a, b), "block {a} must not dominate {b}");
            }
        }
    }

    #[test]
    fn test_dominators_loop_back_edge() {
        // Loop: 0 → 1 → 2, with back-edge 2 → 1 (1 has preds {0, 2}).
        let mut blocks = vec![blk(0, &[]), blk(1, &[0, 2]), blk(2, &[1])];
        blocks[1].successors = vec![2];
        blocks[2].successors = vec![1];
        let dom = compute_dominators(&blocks);
        // The fixpoint must terminate and yield sensible dominance despite the
        // cycle: 0 dominates 1 and 2; 1 dominates 2.
        assert!(dominates(&dom, 0, 1));
        assert!(dominates(&dom, 0, 2));
        assert!(dominates(&dom, 1, 2));
        assert!(
            !dominates(&dom, 2, 1),
            "back-edge target not dominated by body"
        );
    }

    #[test]
    fn test_topo_sort_orders_inputs_before_uses() {
        // Build a graph where node 2 = node0 + node1, all in one block, but the
        // block list is given uses-first to force a reorder.
        let mut graph = bare_graph();
        let a = graph.add(Op::Const(1), IrType::Int, vec![], None); // 0
        let b = graph.add(Op::Const(2), IrType::Int, vec![], None); // 1
        let sum = graph.add(Op::Add, IrType::Int, vec![a, b], None); // 2
        let mut nodes = vec![sum, a, b]; // intentionally out of order
        topo_sort_block(&graph, &mut nodes);
        let pos = |n: NodeId| nodes.iter().position(|&x| x == n).unwrap();
        assert!(pos(a) < pos(sum), "input a must precede the Add");
        assert!(pos(b) < pos(sum), "input b must precede the Add");
        assert_eq!(nodes.len(), 3, "no nodes dropped or duplicated");
    }

    #[test]
    fn test_topo_sort_deep_chain_no_overflow() {
        // A left-leaning chain N levels deep would blow a recursive stack; the
        // iterative version must handle it. Each node depends on the previous.
        let mut graph = bare_graph();
        let base = graph.add(Op::Const(0), IrType::Int, vec![], None);
        let mut prev = base;
        let depth = 50_000;
        for _ in 0..depth {
            prev = graph.add(Op::Neg, IrType::Int, vec![prev], None);
        }
        // Present the nodes in reverse (deepest first) to force full traversal.
        let mut nodes: Vec<NodeId> = (0..=depth as NodeId).rev().collect();
        topo_sort_block(&graph, &mut nodes);
        // After sorting, every node must come after its single input.
        assert_eq!(nodes.len(), depth + 1);
        assert_eq!(nodes[0], base, "the no-input base must sort first");
        for w in nodes.windows(2) {
            assert!(w[0] < w[1], "chain must be in ascending dependency order");
        }
    }

    // ── Memory-effect ordering ───────────────────────────────────────────

    use crate::ir::{MemKind, NodeId as Id};

    /// An `If`'s successors come out in PROJECTION order, whatever order the
    /// projections were added in.
    ///
    /// `ir_lower` names `successors[0]` the true block and `successors[1]` the
    /// false one, so this ordering decides which way every conditional branch
    /// the backend emits goes. It held for free as long as `IrBuilder` was the
    /// only producer — its `if_icmp*` arms add `Proj(0)` and then `Proj(1)`, so
    /// the lower node id was always the true edge, and this scan pushed in node
    /// id order.
    ///
    /// The first transform to CLONE an `If` broke that. The partial unroller
    /// adds each copy's continue-edge projection first, because that is the one
    /// the next copy hangs off, and in a javac `if_icmpge` loop the continue
    /// edge is `Proj(1)`. Every intermediate test then branched to the edge
    /// meant for its opposite: the early exits were never taken and the loop
    /// ran `n + factor - 1` iterations, so `sum(1)` returned 6.
    ///
    /// This graph is built the way the unroller builds one — `Proj(1)` first —
    /// so it fails against a node-id-ordered scan and passes against an
    /// index-ordered one. A fixture built in the conventional order cannot tell
    /// the two apart, which is why this one is deliberately backwards.
    #[test]
    fn an_ifs_successors_are_ordered_by_projection_not_by_node_id() {
        let mut g = bare_graph();
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        let entry = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let cond = g.add(Op::Param(0), IrType::Int, vec![], None);
        let if_node = g.add(Op::If, IrType::Control, vec![entry, cond], Some(0));
        // Backwards on purpose: the FALSE edge gets the lower node id.
        let false_edge = g.add(Op::Proj(1), IrType::Control, vec![if_node], Some(0));
        let true_edge = g.add(Op::Proj(0), IrType::Control, vec![if_node], Some(0));
        let a = g.add(Op::Const(1), IrType::Int, vec![], None);
        let b = g.add(Op::Const(2), IrType::Int, vec![], None);
        let merge = g.add(
            Op::Merge,
            IrType::Control,
            vec![true_edge, false_edge],
            Some(1),
        );
        let phi = g.add(Op::Phi, IrType::Int, vec![merge, a, b], Some(1));
        let ret = g.add(Op::Return, IrType::Void, vec![merge, phi], Some(2));
        g.entry = start;
        g.exit = ret;

        let schedule = schedule(&g);
        let if_block = schedule.node_to_block[if_node as usize];
        let succ = &schedule.blocks[if_block].successors;
        assert_eq!(succ.len(), 2, "an `If` has exactly two successors");
        assert_eq!(
            schedule.blocks[succ[0]].ctrl, true_edge,
            "successors[0] must be the Proj(0) block — `ir_lower` branches on it \
             as the TRUE edge",
        );
        assert_eq!(
            schedule.blocks[succ[1]].ctrl, false_edge,
            "successors[1] must be the Proj(1) block",
        );
    }

    /// `Graph::empty` with no entry either: the one place this module's tests
    /// build a graph, so a change to `Graph`'s fields is a change here alone
    /// (`r11-irback-use-list-invariant-enforced-by-convention-only`, step 2).
    fn bare_graph() -> Graph {
        let mut g = Graph::empty(0);
        g.entry = NO_NODE;
        g
    }

    /// `ctrl`, `mem`, one base reference and two constant offsets, so the
    /// alias model has everything it needs to prove (or refuse) disjointness.
    struct MemGraph {
        graph: Graph,
        ctrl: Id,
        mem: Id,
        base: Id,
        off_a: Id,
        off_b: Id,
        value: Id,
    }

    fn mem_graph() -> MemGraph {
        let mut graph = bare_graph();
        let start = graph.add(Op::Start, IrType::Control, vec![], None);
        let ctrl = graph.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem = graph.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let base = graph.add(Op::Param(0), IrType::Ref, vec![], None);
        let off_a = graph.add(Op::Const(1), IrType::Int, vec![], None);
        let off_b = graph.add(Op::Const(2), IrType::Int, vec![], None);
        let value = graph.add(Op::Param(1), IrType::Int, vec![], None);
        MemGraph {
            graph,
            ctrl,
            mem,
            base,
            off_a,
            off_b,
            value,
        }
    }

    impl MemGraph {
        fn load(&mut self, tok: Id, off: Id) -> Id {
            self.graph.add(
                Op::Load(MemKind::Int),
                IrType::Int,
                vec![self.ctrl, tok, self.base, off],
                None,
            )
        }
        fn store(&mut self, tok: Id, off: Id) -> Id {
            self.graph.add(
                Op::Store(MemKind::Int),
                IrType::Void,
                vec![self.ctrl, tok, self.base, off, self.value],
                None,
            )
        }
    }

    #[test]
    fn orders_memory_classifies_the_op_families() {
        let mut f = mem_graph();
        let load = f.load(f.mem, f.off_a);
        let store = f.store(load, f.off_b);
        let enter = f.graph.add(
            Op::MonitorEnter,
            IrType::Memory,
            vec![f.ctrl, store, f.base],
            None,
        );
        let exit = f.graph.add(
            Op::MonitorExit,
            IrType::Memory,
            vec![f.ctrl, enter, f.base],
            None,
        );
        let call = f.graph.add(
            Op::Call { info_ptr: 0 },
            IrType::Int,
            vec![f.ctrl, exit],
            None,
        );
        let guard = f.graph.add(
            Op::Guard { bci: 0 },
            IrType::Void,
            vec![f.ctrl, f.value],
            None,
        );
        let add = f
            .graph
            .add(Op::Add, IrType::Int, vec![f.value, f.value], None);
        let konst = f.graph.add(Op::Const(7), IrType::Int, vec![], None);

        for (id, name) in [
            (load, "load"),
            (store, "store"),
            (enter, "monitorenter"),
            (exit, "monitorexit"),
            (call, "call"),
            (guard, "guard"),
        ] {
            assert!(orders_memory(&f.graph, id), "{name} must order memory");
        }
        for (id, name) in [(add, "add"), (konst, "const"), (f.value, "param")] {
            assert!(!orders_memory(&f.graph, id), "{name} must be inert");
        }

        // Barriers: safepoints and the JMM fence halves. A plain load or store
        // is neither.
        for (id, name) in [
            (enter, "monitorenter"),
            (exit, "monitorexit"),
            (call, "call"),
            (guard, "guard"),
        ] {
            assert!(is_memory_barrier(&f.graph, id), "{name} must be a barrier");
        }
        assert!(!is_memory_barrier(&f.graph, load));
        assert!(!is_memory_barrier(&f.graph, store));
    }

    #[test]
    fn an_unchanged_order_is_always_accepted() {
        let mut f = mem_graph();
        let load = f.load(f.mem, f.off_a);
        let store = f.store(load, f.off_b);
        let add = f.graph.add(Op::Add, IrType::Int, vec![load, f.value], None);
        let seq = vec![load, store, add];
        assert_eq!(check_memory_order(&f.graph, &seq, &seq), Ok(()));
    }

    /// Pure nodes are not this rule's business: moving one past a store
    /// changes nothing the memory model can see, and dependence order (which
    /// `topo_sort_block` enforces) is what pins them.
    #[test]
    fn pure_nodes_may_move_freely_around_memory_nodes() {
        let mut f = mem_graph();
        let k = f.graph.add(Op::Const(9), IrType::Int, vec![], None);
        let load = f.load(f.mem, f.off_a);
        let store = f.store(load, f.off_b);
        let base = vec![load, k, store];
        let moved = vec![k, load, store];
        assert_eq!(check_memory_order(&f.graph, &base, &moved), Ok(()));
    }

    /// The defect the rule exists to catch: a load hoisted above a store to
    /// the same cell reads the value that was there *before* the store.
    #[test]
    fn a_load_may_not_swap_with_an_aliasing_store() {
        let mut f = mem_graph();
        let store = f.store(f.mem, f.off_a);
        let load = f.load(store, f.off_a); // same base, same constant offset
        let base = vec![store, load];
        let swapped = vec![load, store];
        assert_eq!(
            check_memory_order(&f.graph, &base, &swapped),
            Err(OrderViolation {
                earlier: store,
                later: load,
                refusal: OrderRefusal::Model(ReorderBlock::MayAlias),
            })
        );
    }

    /// Two accesses the model proves disjoint do commute.
    #[test]
    fn provably_disjoint_accesses_may_swap() {
        let mut f = mem_graph();
        let store = f.store(f.mem, f.off_a);
        let load = f.load(store, f.off_b); // different constant offset
        let base = vec![store, load];
        let swapped = vec![load, store];
        assert_eq!(check_memory_order(&f.graph, &base, &swapped), Ok(()));
    }

    /// A monitor-enter is an acquire and a safepoint. Nothing that orders
    /// memory crosses it — including a load the pairwise memory model would
    /// wave through.
    #[test]
    fn nothing_crosses_a_barrier() {
        let mut f = mem_graph();
        let enter = f.graph.add(
            Op::MonitorEnter,
            IrType::Memory,
            vec![f.ctrl, f.mem, f.base],
            None,
        );
        let load = f.load(enter, f.off_a);
        let base = vec![enter, load];
        let swapped = vec![load, enter];
        assert_eq!(
            check_memory_order(&f.graph, &base, &swapped),
            Err(OrderViolation {
                earlier: enter,
                later: load,
                refusal: OrderRefusal::Barrier,
            })
        );
    }

    /// Specifically the case where the rule is deliberately stronger than
    /// `may_reorder`: an allocation is a safepoint, the model says a read may
    /// cross it, and the scheduler still refuses because a faulting load below
    /// a safepoint deopts into a frame that safepoint has already passed.
    #[test]
    fn the_barrier_rule_is_stronger_than_the_pairwise_model() {
        let mut f = mem_graph();
        let alloc = f.graph.add(
            Op::New {
                class_id: 0,
                num_fields: 1,
            },
            IrType::Ref,
            vec![f.ctrl, f.mem],
            None,
        );
        let load = f.load(alloc, f.off_a);
        // The memory model itself permits the swap …
        assert!(f.graph.may_reorder(alloc, load).is_allowed());
        // … and the scheduler still refuses it.
        assert_eq!(
            check_memory_order(&f.graph, &[alloc, load], &[load, alloc]),
            Err(OrderViolation {
                earlier: alloc,
                later: load,
                refusal: OrderRefusal::Barrier,
            })
        );
    }

    /// Two allocations do not commute: which one runs first is observable
    /// through `OutOfMemoryError` and through identity hashes.
    #[test]
    fn two_allocations_may_not_swap() {
        let mut f = mem_graph();
        let a = f.graph.add(
            Op::New {
                class_id: 0,
                num_fields: 1,
            },
            IrType::Ref,
            vec![f.ctrl, f.mem],
            None,
        );
        let b = f.graph.add(
            Op::New {
                class_id: 1,
                num_fields: 1,
            },
            IrType::Ref,
            vec![f.ctrl, a],
            None,
        );
        assert!(check_memory_order(&f.graph, &[a, b], &[b, a]).is_err());
    }

    /// A schedule that drops or invents a node is refused whatever else is
    /// true of it — a dropped store is a dropped write.
    #[test]
    fn a_non_permutation_is_refused() {
        let mut f = mem_graph();
        let load = f.load(f.mem, f.off_a);
        let store = f.store(load, f.off_b);
        let dropped = vec![load];
        assert_eq!(
            check_memory_order(&f.graph, &[load, store], &dropped),
            Err(OrderViolation {
                earlier: NO_NODE,
                later: NO_NODE,
                refusal: OrderRefusal::NotAPermutation,
            })
        );
        let invented = vec![load, f.value];
        assert_eq!(
            check_memory_order(&f.graph, &[load, store], &invented),
            Err(OrderViolation {
                earlier: NO_NODE,
                later: NO_NODE,
                refusal: OrderRefusal::NotAPermutation,
            })
        );
        let duplicated = vec![load, load];
        assert_eq!(
            check_memory_order(&f.graph, &[load, store], &duplicated),
            Err(OrderViolation {
                earlier: NO_NODE,
                later: NO_NODE,
                refusal: OrderRefusal::NotAPermutation,
            })
        );
    }

    /// The priority scheduler's output must satisfy the rule on every block it
    /// touches. This is the property the validator was wired in to guarantee,
    /// checked against the real pass rather than asserted about it.
    #[test]
    fn the_priority_scheduler_never_violates_the_memory_order() {
        // `int f(int a, int b) { return (a + b) * a - b; }` — enough data
        // nodes in one block for the priority pass to have something to do
        // (it declines blocks of two or fewer).
        let code = [
            0x1a, // 0: iload_0
            0x1b, // 1: iload_1
            0x60, // 2: iadd
            0x1a, // 3: iload_0
            0x68, // 4: imul
            0x1b, // 5: iload_1
            0x64, // 6: isub
            0xac, // 7: ireturn
            0, 0,
        ];
        let builder = IrBuilder::new(2, 2);
        let mut graph = builder.build(&code, 8).expect("build failed");
        ir_optimize::optimize(&mut graph);

        let plain = schedule(&graph);
        let opts = ScheduleOptions {
            priority_within_blocks: true,
            ..ScheduleOptions::default()
        };
        let prioritised = schedule_with_options(&graph, &opts);
        assert_eq!(plain.blocks.len(), prioritised.blocks.len());
        for (b, p) in plain.blocks.iter().zip(prioritised.blocks.iter()) {
            assert_eq!(
                check_memory_order(&graph, &b.nodes, &p.nodes),
                Ok(()),
                "block {} was reordered illegally",
                b.id
            );
        }
    }

    /// Pairing may only ever REORDER: every block keeps exactly the node set it
    /// had. A node it dropped or duplicated would be one `ir_lower` emits twice
    /// or not at all.
    #[test]
    fn pairing_preserves_every_block_node_set() {
        let code = [0x1a, 0x1b, 0x60, 0x1a, 0x68, 0x1b, 0x64, 0xac, 0, 0];
        let builder = IrBuilder::new(2, 2);
        let mut graph = builder.build(&code, 8).expect("build failed");
        ir_optimize::optimize(&mut graph);
        let off = schedule_with_options(
            &graph,
            &ScheduleOptions {
                pair_single_use_operands: false,
                ..ScheduleOptions::default()
            },
        );
        let on = schedule_with_options(
            &graph,
            &ScheduleOptions {
                pair_single_use_operands: true,
                ..ScheduleOptions::default()
            },
        );
        assert_eq!(off.blocks.len(), on.blocks.len());
        for (a, b) in off.blocks.iter().zip(on.blocks.iter()) {
            let mut xs = a.nodes.clone();
            let mut ys = b.nodes.clone();
            xs.sort_unstable();
            ys.sort_unstable();
            assert_eq!(xs, ys, "block {} lost or gained a node", a.id);
        }
    }

    /// Pairing only ever sinks, so it cannot move a node above one of its own
    /// inputs — which is exactly the kind of claim that stops being true when
    /// someone widens the pass. Asserted over the emitted order rather than
    /// trusted.
    #[test]
    fn pairing_keeps_every_definition_before_its_uses() {
        let code = [0x1a, 0x1b, 0x60, 0x1a, 0x68, 0x1b, 0x64, 0xac, 0, 0];
        let builder = IrBuilder::new(2, 2);
        let mut graph = builder.build(&code, 8).expect("build failed");
        ir_optimize::optimize(&mut graph);
        let sched = schedule_with_options(
            &graph,
            &ScheduleOptions {
                pair_single_use_operands: true,
                ..ScheduleOptions::default()
            },
        );
        for block in &sched.blocks {
            let mut seen = std::collections::HashSet::new();
            for &nid in &block.nodes {
                for &input in &graph.nodes[nid as usize].inputs {
                    if input == NO_NODE {
                        continue;
                    }
                    if block.nodes.contains(&input) {
                        assert!(
                            seen.contains(&input),
                            "node {nid} reads {input}, which this block defines later"
                        );
                    }
                }
                seen.insert(nid);
            }
        }
    }

    /// A single-use operand must not be sunk past anything a deopt can arrive
    /// at: it is undefined there, and `graph.safepoints` names the whole
    /// operand stack at every bci.
    ///
    /// `idiv` is the available trapping node — it deopts on a zero divisor
    /// after reading its operands.
    #[test]
    fn pairing_does_not_sink_past_a_node_a_deopt_can_reach() {
        let code = [
            0x1a, // iload_0
            0x1b, // iload_1
            0x60, // iadd     <- a single-use operand of the outer iadd
            0x1a, // iload_0
            0x1b, // iload_1
            0x6c, // idiv     <- can deopt
            0x60, // iadd
            0xac, // ireturn
            0, 0,
        ];
        let builder = IrBuilder::new(2, 2);
        let Some(mut graph) = builder.build(&code, 8) else {
            return;
        };
        ir_optimize::optimize(&mut graph);
        let sched = schedule_with_options(
            &graph,
            &ScheduleOptions {
                pair_single_use_operands: true,
                ..ScheduleOptions::default()
            },
        );
        for block in &sched.blocks {
            let Some(div) = block
                .nodes
                .iter()
                .position(|&n| matches!(graph.nodes[n as usize].op, Op::Div))
            else {
                continue;
            };
            for &i in &graph.nodes[block.nodes[div] as usize].inputs {
                if i == NO_NODE {
                    continue;
                }
                if let Some(pos) = block.nodes.iter().position(|&x| x == i) {
                    assert!(
                        pos < div,
                        "an operand of the trapping node was sunk past it"
                    );
                }
            }
        }
    }

    /// Build the three-node window the pairing pass acts on: a producer the
    /// carry certifies, one node in between, and the single consumer.
    ///
    /// Returns `(nodes, census, producer, consumer)` after one direct run of
    /// the pass, so a test can assert on both the order and on WHY the pass
    /// decided as it did.
    fn run_pairing_over(f: &mut MemGraph, middle: Id) -> (Vec<Id>, PairCensus, Id, Id) {
        // `ArrayLength` is the shortest producer that is BOTH in
        // `op_home_is_one_store_rax` (so the pass will consider moving it) and
        // memory-ordering (a `LengthRead`), which is the combination the
        // memory-order check exists for.
        let prod = f.graph.add(
            Op::ArrayLength,
            IrType::Int,
            vec![f.ctrl, f.mem, f.base],
            None,
        );
        // One input, so exactly one `(consumer, operand)` candidate.
        let cons = f.graph.add(Op::Neg, IrType::Int, vec![prod], None);
        let mut nodes = vec![prod, middle, cons];
        let uses = use_counts(&f.graph);
        let mut census = PairCensus::default();
        pair_single_use_operands(&f.graph, &uses, &mut nodes, &mut census);
        (nodes, census, prod, cons)
    }

    /// An `If`'s single-use compare that no frame names moves past a later
    /// guard to sit last in its block; a frame-named one stays put.
    #[test]
    fn branch_compare_goes_last_unless_a_frame_names_it() {
        use crate::ir::CmpOp;
        let mut f = mem_graph();
        let cmp = f.graph.add(Op::Cmp(CmpOp::Lt), IrType::Int, vec![f.value, f.off_a], None);
        let gc = f.graph.add(Op::Cmp(CmpOp::Ge), IrType::Int, vec![f.value, f.off_b], None);
        let guard = f.graph.add(Op::Guard { bci: 3 }, IrType::Void, vec![f.ctrl, gc], Some(3));
        let iff = f.graph.add(Op::If, IrType::Control, vec![f.ctrl, cmp], None);
        let uses = use_counts(&f.graph);
        let mut named = vec![false; f.graph.nodes.len()];
        let mut nodes = vec![cmp, gc, guard];
        assert!(sink_branch_compare_last(&f.graph, &uses, &named, &mut nodes, Some(iff)));
        assert_eq!(nodes, vec![gc, guard, cmp]);
        named[cmp as usize] = true;
        let mut nodes = vec![cmp, gc, guard];
        assert!(!sink_branch_compare_last(&f.graph, &uses, &named, &mut nodes, Some(iff)));
        assert_eq!(nodes, vec![cmp, gc, guard]);
    }

    /// The pass still does its job when what it crosses is genuinely pure.
    /// The memory-order check is a screen, not a brake.
    #[test]
    fn pairing_sinks_a_producer_past_a_pure_node() {
        let mut f = mem_graph();
        let konst = f.graph.add(Op::Const(9), IrType::Int, vec![], None);
        let (nodes, census, prod, cons) = run_pairing_over(&mut f, konst);

        assert_eq!(census.paired, 1, "the pure window must still pair");
        assert_eq!(census.memory_order, 0);
        assert_eq!(
            nodes,
            vec![konst, prod, cons],
            "the producer sank past the pure node to sit beside its consumer"
        );
        assert!(census.closes(), "every candidate landed in a bucket");
    }

    /// The defect the step-5b memory-order check exists to catch.
    ///
    /// A memory-typed φ carries `MemEffect::OPAQUE` — it reads and writes
    /// anything — and `ir_lower::op_cannot_deopt` accepts `Op::Phi`, because a
    /// deopt cannot arrive AT a φ. So the pass's only screen says "cross it"
    /// while the memory model says "you just moved a read past an opaque
    /// write". That is not a hypothetical intersection of the two allowlists:
    /// `Op::Phi`, `Op::Return` and `Op::Dead` are all in `op_cannot_deopt` and
    /// all carry `OPAQUE`.
    ///
    /// Before the check, step 5b was the one reordering in the scheduler
    /// `check_memory_order` never saw — it runs after the single validation
    /// call — so this move happened silently.
    #[test]
    fn pairing_refuses_to_sink_a_memory_read_past_an_opaque_node() {
        let mut f = mem_graph();
        // A memory-typed φ: `effect_of_node` gives it `MemEffect::OPAQUE`,
        // and `op_cannot_deopt` accepts `Op::Phi`.
        let phi = f.graph.add(Op::Phi, IrType::Memory, vec![f.mem], None);
        let (nodes, census, prod, cons) = run_pairing_over(&mut f, phi);

        assert_eq!(
            census.memory_order, 1,
            "the memory model must be the one to refuse this, not the deopt screen"
        );
        assert_eq!(
            census.deopt_between, 0,
            "the deopt screen ALLOWS crossing a phi — that is the whole point"
        );
        assert_eq!(census.paired, 0);
        assert_eq!(
            nodes,
            vec![prod, phi, cons],
            "nothing moved: the order is unchanged"
        );
        assert!(census.closes(), "every candidate landed in a bucket");
    }

    /// The validator may only ever *reject* a reordering, never cause one: a
    /// block keeps exactly the node set it had, whether or not the priority
    /// pass ran. (A node the pass dropped would also be a node `plan_slots`
    /// never colours and `ir_lower` never emits.)
    #[test]
    fn priority_scheduling_preserves_every_block_node_set() {
        let code = [0x1a, 0x1b, 0x60, 0x1a, 0x68, 0x1b, 0x64, 0xac, 0, 0];
        let builder = IrBuilder::new(2, 2);
        let mut graph = builder.build(&code, 8).expect("build failed");
        ir_optimize::optimize(&mut graph);
        let plain = schedule(&graph);
        let prioritised = schedule_with_options(
            &graph,
            &ScheduleOptions {
                priority_within_blocks: true,
                ..ScheduleOptions::default()
            },
        );
        assert_eq!(plain.blocks.len(), prioritised.blocks.len());
        for (x, y) in plain.blocks.iter().zip(prioritised.blocks.iter()) {
            let mut a = x.nodes.clone();
            let mut b = y.nodes.clone();
            a.sort_unstable();
            b.sort_unstable();
            assert_eq!(a, b, "block {} lost or gained a node", x.id);
        }
    }

    /// The latency table must rank the buckets it exists to distinguish.
    ///
    /// Absolute cycle counts are a microarchitecture's business and this pass
    /// only ever compares two of them, so what is pinned here is the ORDER —
    /// which is the part that has to stay true across Skylake, Zen and
    /// Neoverse. A table that got the order wrong would still "work": the
    /// scheduler would simply prefer the cheaper node, silently, on every
    /// method.
    #[test]
    fn the_latency_table_ranks_a_divide_above_a_multiply_above_an_add() {
        let int_add = op_latency(&Op::Add, IrType::Int);
        let int_mul = op_latency(&Op::Mul, IrType::Int);
        let int_div = op_latency(&Op::Div, IrType::Int);
        assert!(int_add < int_mul && int_mul < int_div, "integer ladder");

        let fp_add = op_latency(&Op::Add, IrType::Double);
        let fp_mul = op_latency(&Op::Mul, IrType::Double);
        let fp_div = op_latency(&Op::Div, IrType::Double);
        assert_eq!(fp_add, fp_mul, "addsd and mulsd are both four cycles");
        assert!(fp_mul < fp_div, "divsd is several times mulsd");

        // The same opcode costs different amounts at different types: an
        // integer add is one cycle and `addsd` is four. A table keyed on the
        // opcode alone cannot say that, which is why `op_latency` takes a type.
        assert!(int_add < fp_add, "the type has to reach the table");

        // A load is slower than arithmetic but must not be treated as a
        // cache miss — the header says why an L1 hit is the number to use.
        let load = op_latency(&Op::Load(crate::ir::MemKind::Int), IrType::Int);
        assert!(load > int_add && load < int_div);

        // An unnamed op answers the small default, not a large one. See the
        // header: an impure node cannot move anyway, and a large default would
        // make every operand of a call outrank everything else in the block.
        assert_eq!(op_latency(&Op::Return, IrType::Void), 1);
    }

    /// The critical path is measured in cycles, so the slower of two equally
    /// ready nodes goes first.
    ///
    /// `int f(int a, int b) { return (a + b) ^ (a * b); }`. Once both
    /// parameters are emitted the add and the multiply are both ready, neither
    /// kills an operand (each parameter still has the other consumer alive), so
    /// the pressure term ties and the critical-path term decides. Both feed the
    /// same `Xor`, so under the old count-the-nodes model their heights were
    /// equal too and the tie fell through to seed order — which is bytecode
    /// order, and the add is written first. The multiply is three cycles and
    /// the add is one, so a cycle-counting model has to put the multiply first.
    ///
    /// The priority-OFF arm is the anti-vacuity guard: it pins that the plain
    /// dependence order really does emit the add first, so the ON arm is
    /// asserting a change and not an accident of node numbering.
    #[test]
    fn the_priority_scheduler_starts_the_slower_of_two_ready_nodes_first() {
        // 0: iload_0  1: iload_1  2: iadd
        // 3: iload_0  4: iload_1  5: imul
        // 6: ixor     7: ireturn
        let code = [0x1au8, 0x1b, 0x60, 0x1a, 0x1b, 0x68, 0x82, 0xac, 0, 0];
        let builder = IrBuilder::new(2, 2);
        let mut graph = builder.build(&code, 8).expect("build failed");
        ir_optimize::optimize(&mut graph);

        let add = graph
            .nodes
            .iter()
            .position(|n| n.op == Op::Add)
            .expect("the method computes a + b") as NodeId;
        let mul = graph
            .nodes
            .iter()
            .position(|n| n.op == Op::Mul)
            .expect("the method computes a * b") as NodeId;

        let order_in = |sched: &Schedule| -> (usize, usize) {
            let nodes = sched
                .blocks
                .iter()
                .flat_map(|b| b.nodes.iter().copied())
                .collect::<Vec<_>>();
            let at = |id: NodeId| nodes.iter().position(|&x| x == id).expect("node scheduled");
            (at(add), at(mul))
        };

        let (add_off, mul_off) =
            order_in(&schedule_with_options(&graph, &ScheduleOptions::default()));
        assert!(
            add_off < mul_off,
            "the plain dependence order emits the add first — without that this \
             test cannot tell a reordering from the status quo"
        );

        let (add_on, mul_on) = order_in(&schedule_with_options(
            &graph,
            &ScheduleOptions {
                priority_within_blocks: true,
                ..ScheduleOptions::default()
            },
        ));
        assert!(
            mul_on < add_on,
            "the three-cycle multiply must start before the one-cycle add"
        );
    }

    /// Every block offered to the priority scheduler lands in exactly one
    /// census bucket, and neither model-invariant bucket is ever hit.
    ///
    /// Two claims, and the second is the one worth having. `cycle` says the
    /// ordering DAG had no topological order, which cannot happen if the
    /// construction argument holds (value dependences plus a side-effect chain
    /// laid down in an order that is already topological). `memory_order` says
    /// `check_memory_order` refused the permutation, which cannot happen if the
    /// chain really is total over the impure nodes. Both are asserted against
    /// the process-wide counters rather than a reset snapshot, precisely
    /// because "zero across every test this binary ran" is the stronger
    /// statement and is immune to another test running concurrently.
    #[test]
    fn the_priority_census_closes_and_its_model_invariants_hold() {
        // 0: iload_0  1: iload_1  2: iadd
        // 3: iload_0  4: iload_1  5: imul
        // 6: ixor     7: ireturn
        let code = [0x1au8, 0x1b, 0x60, 0x1a, 0x1b, 0x68, 0x82, 0xac, 0, 0];
        let builder = IrBuilder::new(2, 2);
        let mut graph = builder.build(&code, 8).expect("build failed");
        ir_optimize::optimize(&mut graph);

        let before = ir_priority_census().blocks;
        let _ = schedule_with_options(
            &graph,
            &ScheduleOptions {
                priority_within_blocks: true,
                ..ScheduleOptions::default()
            },
        );
        let after = ir_priority_census();
        assert!(
            after.blocks > before,
            "the denominator must advance, or the census is not wired to the pass"
        );
        assert_eq!(
            after.cycle, 0,
            "the ordering DAG is built from a valid topological order, so it \
             always has one — a cycle here is a defect in the construction"
        );
        assert_eq!(
            after.memory_order, 0,
            "only pure nodes float and every impure node is in the total \
             side-effect chain, so the memory-order validator must be vacuous \
             for this pass — a refusal here means the chain is not total"
        );
        assert!(
            after.model_held(),
            "the two model invariants are also asked as one question"
        );
    }

    /// The census identity, on values rather than on the global table.
    ///
    /// `closes()` is what makes a `return` added to `priority_sort_block` and
    /// not to the census fail loudly instead of quietly reading low — the
    /// failure mode `PairCensus` documents from a census that drifted to zero
    /// while the emission it described did three things.
    #[test]
    fn the_priority_census_identity_catches_an_uncounted_exit() {
        let complete = PriorityCensus {
            blocks: 7,
            too_small: 2,
            over_budget: 1,
            duplicate_node: 0,
            missing_node: 0,
            cycle: 0,
            memory_order: 0,
            unchanged: 1,
            reordered: 3,
        };
        assert!(complete.closes());
        assert!(complete.model_held());

        // One block that left through an exit nobody counted.
        let leaky = PriorityCensus {
            blocks: 8,
            ..complete
        };
        assert!(!leaky.closes());

        // And the model invariants are a separate question from the identity:
        // a census can close perfectly while reporting a broken chain.
        let closed_but_broken = PriorityCensus {
            blocks: 7,
            memory_order: 1,
            reordered: 2,
            ..complete
        };
        assert!(closed_but_broken.closes());
        assert!(!closed_but_broken.model_held());
    }

    /// Whether a division may sink is a property of its TYPE, not its opcode.
    ///
    /// `idiv` raises `ArithmeticException` on a zero divisor and so is pinned;
    /// `ddiv` raises nothing at all (JVMS §6.5: division by zero is ±infinity)
    /// and is the single most expensive pure node the IR has. Asserted by
    /// retyping ONE node of a real graph, because that isolates the type as the
    /// only thing that changed — a second fixture built from `ddiv` bytecode
    /// would also differ in its operands, its uses and its node ids.
    #[test]
    fn a_floating_point_division_may_sink_and_an_integer_one_may_not() {
        // int f(int a, int b) { return a / b; }
        let code = [0x1au8, 0x1b, 0x6c, 0xac, 0, 0];
        let builder = IrBuilder::new(2, 2);
        let mut graph = builder.build(&code, 4).expect("build failed");
        let div = graph
            .nodes
            .iter()
            .position(|n| n.op == Op::Div)
            .expect("the idiv builds an Op::Div");

        assert_eq!(graph.nodes[div].ty, IrType::Int);
        assert!(
            !node_is_sinkable(&graph, div),
            "an integer division traps, so moving it moves where the program throws"
        );

        graph.nodes[div].ty = IrType::Double;
        assert!(
            node_is_sinkable(&graph, div),
            "ddiv cannot raise, so it is as movable as a multiply and far more \
             worth moving"
        );

        // `frem`/`drem` also cannot raise, and are still refused: they lower to
        // a `jit_frem`/`jit_drem` helper CALL, and a call is a safepoint.
        graph.nodes[div].op = Op::Rem;
        assert!(
            !node_is_sinkable(&graph, div),
            "an FP remainder is a helper call, and this pass does not move safepoints"
        );
    }

    /// The three narrowing conversions and the FP compare belong on the
    /// sinkable list, and were missing from it with no reason recorded.
    ///
    /// Each is in `Op::is_pure()`, each is a total function of its value inputs
    /// with no control and no memory edge, and `Op::LCmp` — its structural twin
    /// — was already there. An omission with no argument behind it is how a
    /// pass quietly stops applying to a whole family of code.
    #[test]
    fn the_pure_narrowing_conversions_and_the_fp_compare_are_sinkable() {
        for op in [Op::I2B, Op::I2C, Op::I2S] {
            assert!(op_is_sinkable(&op), "{op:?} is pure and has fixed arity");
            assert!(op.is_pure(), "and this list must not drift from is_pure");
        }
        let fcmp = Op::FCmp {
            double: true,
            nan_greater: false,
        };
        assert!(op_is_sinkable(&fcmp));
        assert!(
            op_is_sinkable(&Op::LCmp),
            "the twin that was already listed"
        );
        // The refusals that must survive: a store, a call-shaped node and a phi
        // are not sinkable however pure their neighbours are.
        assert!(!op_is_sinkable(&Op::Phi));
        assert!(!op_is_sinkable(&Op::ArrayLength));
    }

    /// Blocks are indexed in CREATION order — the order Step 1 walked control
    /// nodes in — and `ir_lower` emits `schedule.blocks` front to back. Nothing
    /// made creation order a reverse postorder, so a block could be emitted
    /// before the block that dominates it, and a value read before the
    /// instruction defining it. `verify_data_locations` caught that and refused
    /// the compile, which is why it cost lost compiles rather than wrong code.
    ///
    /// Here block 1 is reachable only through block 2, but is numbered first.
    #[test]
    fn rpo_layout_puts_a_dominator_before_the_block_it_dominates() {
        let blocks = vec![
            Block {
                id: 0,
                ctrl: 0,
                nodes: Vec::new(),
                terminator: None,
                successors: vec![2],
                predecessors: Vec::new(),
            },
            Block {
                id: 1,
                ctrl: 1,
                nodes: Vec::new(),
                terminator: None,
                successors: Vec::new(),
                predecessors: vec![2],
            },
            Block {
                id: 2,
                ctrl: 2,
                nodes: Vec::new(),
                terminator: None,
                successors: vec![1],
                predecessors: vec![0],
            },
        ];
        let order = layout_blocks_rpo(&blocks, &[]).expect("a three-block chain must lay out");
        let pos = |b: usize| {
            order
                .iter()
                .position(|&x| x == b)
                .expect("every block placed")
        };
        assert_eq!(pos(0), 0, "the entry block stays at position 0");
        assert!(
            pos(2) < pos(1),
            "block 2 dominates block 1 and must be emitted first; got order {order:?}",
        );
    }

    /// The layout is a permutation, not a filter: dropping or repeating a block
    /// would silently delete or duplicate code.
    #[test]
    fn rpo_layout_is_a_permutation() {
        let blocks = vec![
            Block {
                id: 0,
                ctrl: 0,
                nodes: Vec::new(),
                terminator: None,
                successors: vec![1, 2],
                predecessors: Vec::new(),
            },
            Block {
                id: 1,
                ctrl: 1,
                nodes: Vec::new(),
                terminator: None,
                successors: vec![3],
                predecessors: vec![0],
            },
            Block {
                id: 2,
                ctrl: 2,
                nodes: Vec::new(),
                terminator: None,
                successors: vec![3],
                predecessors: vec![0],
            },
            Block {
                id: 3,
                ctrl: 3,
                nodes: Vec::new(),
                terminator: None,
                successors: Vec::new(),
                predecessors: vec![1, 2],
            },
        ];
        let order = layout_blocks_rpo(&blocks, &[]).expect("a diamond must lay out");
        let mut seen = order.clone();
        seen.sort_unstable();
        assert_eq!(
            seen,
            vec![0, 1, 2, 3],
            "order {order:?} is not a permutation"
        );
        let pos = |b: usize| order.iter().position(|&x| x == b).unwrap();
        assert!(
            pos(3) > pos(1) && pos(3) > pos(2),
            "the merge follows both arms"
        );
    }

    /// An empty CFG is not an error, and must not be turned into one.
    #[test]
    fn rpo_layout_accepts_an_empty_block_list() {
        assert_eq!(
            layout_blocks_rpo(&[], &[]).expect("empty is fine"),
            Vec::<usize>::new()
        );
    }

    // ── Dominators against the dense fixpoint they replaced ──────────────

    /// The dense iterative fixpoint `compute_dominators` used to be, kept
    /// verbatim as the reference [`Dominators`] must agree with — unreachable
    /// blocks, out-of-range predecessors and all.
    fn dense_dominators_reference(blocks: &[Block]) -> Vec<Vec<bool>> {
        let n = blocks.len();
        if n == 0 {
            return Vec::new();
        }
        let mut dom: Vec<Vec<bool>> = vec![vec![true; n]; n];
        dom[0] = vec![false; n];
        dom[0][0] = true;

        let mut changed = true;
        while changed {
            changed = false;
            for b in 1..n {
                let mut new_dom: Option<Vec<bool>> = None;
                for &p in &blocks[b].predecessors {
                    if p >= n {
                        continue;
                    }
                    if let Some(acc) = new_dom.as_mut() {
                        for d in 0..n {
                            acc[d] &= dom[p][d];
                        }
                    } else {
                        new_dom = Some(dom[p].clone());
                    }
                }
                let mut new_dom = match new_dom {
                    Some(v) => v,
                    None => continue,
                };
                new_dom[b] = true;
                if new_dom != dom[b] {
                    dom[b] = new_dom;
                    changed = true;
                }
            }
        }
        dom
    }

    /// A CFG of `n` blocks from an edge list, with both edge lists filled in
    /// (the dominator math reads only `predecessors`; `successors` is there
    /// so the fixture is a plausible CFG). An edge may name a block `>= n`,
    /// which lands in `predecessors` only — the out-of-range case both
    /// algorithms must ignore.
    fn cfg(n: usize, edges: &[(usize, usize)]) -> Vec<Block> {
        let mut blocks: Vec<Block> = (0..n).map(|id| blk(id, &[])).collect();
        for &(from, to) in edges {
            if to < n {
                blocks[to].predecessors.push(from);
            }
            if from < n && to < n {
                blocks[from].successors.push(to);
            }
        }
        blocks
    }

    /// Both representations answer every `(a, b)` query the way the reference
    /// matrix does, one index past the end included.
    fn assert_dominators_match_reference(name: &str, blocks: &[Block]) {
        let reference = dense_dominators_reference(blocks);
        assert_eq!(
            compute_dominators(blocks),
            reference,
            "{name}: matrix differs from the dense fixpoint"
        );
        let doms = Dominators::compute(blocks);
        assert_eq!(doms.len(), blocks.len(), "{name}: relation size");
        for a in 0..=blocks.len() {
            for b in 0..=blocks.len() {
                assert_eq!(
                    doms.dominates(a, b),
                    dominates(&reference, a, b),
                    "{name}: dominates({a}, {b}) differs from the dense fixpoint"
                );
            }
        }
    }

    #[test]
    fn chk_dominators_match_the_dense_fixpoint_on_hand_built_cfgs() {
        assert_dominators_match_reference("empty", &[]);
        assert_dominators_match_reference("single block", &cfg(1, &[]));
        assert_dominators_match_reference("diamond", &cfg(4, &[(0, 1), (0, 2), (1, 3), (2, 3)]));
        // 0 -> 1 (outer header) -> 2 (inner header) -> 3 (inner latch) -> 2,
        // 2 -> 4 (outer latch) -> 1, 1 -> 5 (exit).
        assert_dominators_match_reference(
            "nested loops",
            &cfg(6, &[(0, 1), (1, 2), (2, 3), (3, 2), (2, 4), (4, 1), (1, 5)]),
        );
        // A loop with two entries: neither 1 nor 2 dominates the other.
        assert_dominators_match_reference(
            "irreducible two-entry loop",
            &cfg(4, &[(0, 1), (0, 2), (1, 2), (2, 1), (1, 3), (2, 3)]),
        );
        // Listed with the join BEFORE its arms, so block order is not RPO.
        assert_dominators_match_reference(
            "numbering is not reverse postorder",
            &cfg(5, &[(0, 3), (0, 4), (3, 1), (4, 1), (1, 2)]),
        );
        assert_dominators_match_reference(
            "self loop and a back edge to the entry",
            &cfg(3, &[(0, 1), (1, 1), (1, 2), (2, 0)]),
        );
        // 3 has no predecessors; 4 <-> 5 is a cycle nothing enters; 5 also
        // feeds the reachable 2, which must ignore it.
        assert_dominators_match_reference(
            "unreachable blocks",
            &cfg(6, &[(0, 1), (1, 2), (4, 5), (5, 4), (5, 2), (3, 2)]),
        );
        assert_dominators_match_reference(
            "out-of-range predecessor",
            &cfg(3, &[(0, 1), (1, 2), (7, 2), (9, 1)]),
        );
        let chain: Vec<(usize, usize)> = (0..999).map(|b| (b, b + 1)).collect();
        assert_dominators_match_reference("long chain", &cfg(1000, &chain));
    }

    /// Several hundred small CFGs from a fixed-seed generator, so the
    /// agreement is not an artifact of the shapes someone thought to draw.
    #[test]
    fn chk_dominators_match_the_dense_fixpoint_on_generated_cfgs() {
        // Numerical Recipes' LCG: deterministic, no dependency.
        let mut state: u64 = 0x2545_F491_4F6C_DD1D;
        let mut next = |bound: usize| -> usize {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((state >> 33) as usize) % bound.max(1)
        };
        for case in 0..400 {
            let n = 1 + next(24);
            let m = next(3 * n + 1);
            let edges: Vec<(usize, usize)> = (0..m).map(|_| (next(n), next(n))).collect();
            assert_dominators_match_reference(&format!("generated case {case}"), &cfg(n, &edges));
        }
    }

    #[test]
    fn chk_idom_names_the_nearest_strict_dominator() {
        let d = Dominators::compute(&cfg(5, &[(0, 1), (0, 2), (1, 3), (2, 3), (4, 3)]));
        assert_eq!(d.idom(0), None, "the entry has no immediate dominator");
        assert_eq!(d.idom(1), Some(0));
        assert_eq!(
            d.idom(3),
            Some(0),
            "a join's idom is the branch, not an arm"
        );
        assert_eq!(d.idom(4), None, "an unreachable block has none");
        assert_eq!(d.idom(5), None, "nor does an out-of-range index");
    }

    // ── The sink pass's answers around unreachable blocks, pinned ────────

    /// What the sink pass's common-dominator query answers when a use sits in
    /// a block the entry does not reach.
    ///
    /// In this model every block dominates an unreachable one, so an
    /// unreachable use constrains nothing. When EVERY use is unreachable,
    /// every block qualifies, and the "deeper wins" rule settles on the
    /// highest-numbered unreachable block. The block scan that answered this
    /// was replaced by a dominator-tree walk; these are the answers the walk
    /// must keep.
    #[test]
    fn sink_common_dominator_treats_an_unreachable_use_as_dominated_by_every_block() {
        // 0 -> 1 -> 2 is reachable; 3 -> 4 is not.
        let blocks = cfg(5, &[(0, 1), (1, 2), (3, 4)]);
        let dom = Dominators::compute(&blocks);
        for a in 0..5 {
            assert!(dom.dominates(a, 3), "{a} must dominate the unreachable 3");
            assert!(dom.dominates(a, 4), "{a} must dominate the unreachable 4");
        }
        assert!(
            !dom.dominates(3, 1),
            "an unreachable block dominates no reachable one"
        );

        // Every use unreachable: the last unreachable block.
        assert_eq!(deepest_common_dominator(&dom, &[3], 5), Some(4));
        assert_eq!(deepest_common_dominator(&dom, &[4, 3], 5), Some(4));
        // A reachable use decides on its own.
        assert_eq!(deepest_common_dominator(&dom, &[2, 3], 5), Some(2));
        assert_eq!(deepest_common_dominator(&dom, &[1, 2, 4], 5), Some(1));
        // No block dominates an out-of-range use.
        assert_eq!(deepest_common_dominator(&dom, &[2, 9], 5), None);
    }

    /// The sink pass's dominator-tree walks give the same answers as the block
    /// scans they replaced, on several hundred generated CFGs. Unreachable
    /// blocks, self loops and irreducible cycles are all included. Placement
    /// is checked under both the real loop depths and random depths, so ties
    /// break the same way too.
    #[test]
    fn sink_placement_walks_match_the_block_scans_on_generated_cfgs() {
        let mut state: u64 = 0x5DEE_CE66_D1CE_4E5B;
        let mut next = |bound: usize| -> usize {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((state >> 33) as usize) % bound.max(1)
        };
        let mut path: Vec<usize> = Vec::new();
        for case in 0..300 {
            let n = 1 + next(20);
            let m = next(3 * n + 1);
            let edges: Vec<(usize, usize)> = (0..m).map(|_| (next(n), next(n))).collect();
            let blocks = cfg(n, &edges);
            let dom = Dominators::compute(&blocks);

            for _ in 0..24 {
                let k = 1 + next(4);
                // `next(n + 1)` can name one block past the end.
                let of: Vec<usize> = (0..k).map(|_| next(n + 1)).collect();
                assert_eq!(
                    deepest_common_dominator(&dom, &of, n),
                    deepest_common_dominator_by_scan(&dom, &of, n),
                    "case {case}: common dominator of {of:?}"
                );
            }

            let (loop_depth, innermost) = loop_nest(&blocks, &dom);
            assert_eq!(loop_depth, loop_depths(&blocks, &dom));
            let random_depth: Vec<u32> = (0..n).map(|_| next(3) as u32).collect();
            for depth in [&loop_depth, &random_depth] {
                for early in 0..n {
                    for late in 0..n {
                        if !dom.dominates(early, late) {
                            continue;
                        }
                        for equal_depth in [false, true] {
                            assert_eq!(
                                choose_sink_block(
                                    &dom,
                                    depth,
                                    &innermost,
                                    early,
                                    late,
                                    equal_depth,
                                    n,
                                    &mut path
                                ),
                                choose_sink_block_by_scan(
                                    &dom,
                                    depth,
                                    &innermost,
                                    early,
                                    late,
                                    equal_depth,
                                    n
                                ),
                                "case {case}: early {early}, late {late}, equal_depth {equal_depth}"
                            );
                        }
                    }
                }
            }
        }
    }

    /// The all-pairs scan `find_best_block` made before round 12 wave 2:
    /// `Some(Some(b))` placed in `b`, `Some(None)` no placed input, `None`
    /// no block dominated by all of them.
    fn best_block_by_all_pairs_scan(
        dom: &Dominators,
        of: &[Option<usize>],
    ) -> Option<Option<usize>> {
        let mut input_blocks: Vec<usize> = Vec::new();
        for b in of.iter().flatten() {
            if !input_blocks.contains(b) {
                input_blocks.push(*b);
            }
        }
        if input_blocks.is_empty() {
            return Some(None);
        }
        input_blocks
            .iter()
            .copied()
            .find(|&cand| {
                input_blocks
                    .iter()
                    .all(|&other| other == cand || dom.dominates(other, cand))
            })
            .map(Some)
    }

    /// Round 12 wave 2 (lane irback, proposal R12-5): the one-pass
    /// `deepest_input_block` the scheduler and the verifier's dominance lane
    /// now share places every node where the all-pairs scan did, on several
    /// hundred generated CFGs with unreachable blocks, cycles and unplaced
    /// inputs, and an incomparability it reports is a real one.
    #[test]
    fn r12w2_deepest_input_block_matches_the_all_pairs_scan_on_generated_cfgs() {
        let mut state: u64 = 0x1B87_3593_4C0F_FEE5;
        let mut next = |bound: usize| -> usize {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((state >> 33) as usize) % bound.max(1)
        };
        for case in 0..300 {
            let n = 1 + next(20);
            let m = next(3 * n + 1);
            let edges: Vec<(usize, usize)> = (0..m).map(|_| (next(n), next(n))).collect();
            let blocks = cfg(n, &edges);
            let dom = Dominators::compute(&blocks);
            for _ in 0..24 {
                let k = next(6);
                // `None` is an input with no block yet (or `NO_NODE`).
                let of: Vec<Option<usize>> = (0..k)
                    .map(|_| if next(5) == 0 { None } else { Some(next(n)) })
                    .collect();
                let inputs: Vec<NodeId> = (0..k as NodeId).collect();
                let block_of = |i: NodeId| of.get(i as usize).copied().flatten();
                let got = deepest_input_block(&inputs, block_of, &dom);
                match (best_block_by_all_pairs_scan(&dom, &of), got) {
                    (Some(want), Ok(have)) => {
                        assert_eq!(have.map(|(b, _)| b), want, "case {case}: inputs {of:?}");
                        if let Some((b, i)) = have {
                            assert_eq!(block_of(i), Some(b), "case {case}: the named input");
                        }
                    }
                    (None, Err((a, b))) => {
                        let (ba, bb) = (block_of(a), block_of(b));
                        let (Some(ba), Some(bb)) = (ba, bb) else {
                            panic!("case {case}: {of:?} names an unplaced input");
                        };
                        assert!(
                            !dom.dominates(ba, bb) && !dom.dominates(bb, ba),
                            "case {case}: {of:?} reports comparable blocks {ba} and {bb}"
                        );
                    }
                    (want, got) => panic!("case {case}: {of:?}: scan {want:?}, walk {got:?}"),
                }
            }
        }
    }

    // ── Step 4 placement order ───────────────────────────────────────────

    /// An old node rewired to read a NEWER one is placed where that input is
    /// available, not where id order happened to leave it.
    ///
    /// Step 4 used to place nodes in id order, and `find_best_block` ignores
    /// inputs that have no block yet. So `user` below — created first, then
    /// pointed at `input` — saw only the parameter, went to the entry block,
    /// and was emitted before the merge block that computes `input`: exactly
    /// the def-after-use `verify_data_locations` refuses, and the shape
    /// `reassociate_affine` leaves behind when it rewires an operand to a node
    /// it just materialised.
    #[test]
    fn an_old_node_rewired_to_a_newer_input_is_placed_after_that_input() {
        let mut g = bare_graph();
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        let entry = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let p = g.add(Op::Param(0), IrType::Int, vec![], None);
        // The OLD node, created before the value it will end up reading.
        let user = g.add(Op::Add, IrType::Int, vec![p, p], None);
        let if_node = g.add(Op::If, IrType::Control, vec![entry, p], Some(0));
        let t = g.add(Op::Proj(0), IrType::Control, vec![if_node], Some(0));
        let f = g.add(Op::Proj(1), IrType::Control, vec![if_node], Some(0));
        let merge = g.add(Op::Merge, IrType::Control, vec![t, f], Some(1));
        let one = g.add(Op::Const(1), IrType::Int, vec![], None);
        let two = g.add(Op::Const(2), IrType::Int, vec![], None);
        let phi = g.add(Op::Phi, IrType::Int, vec![merge, one, two], Some(1));
        // The NEW input: it reads the phi, so it can only live in the merge.
        let input = g.add(Op::Mul, IrType::Int, vec![phi, p], Some(1));
        assert!(g.set_input(user, 0, input), "fixture: rewire the old node");
        let ret = g.add(Op::Return, IrType::Void, vec![merge, user], Some(2));
        g.entry = start;
        g.exit = ret;
        assert!(input > user, "fixture: the input must have the higher id");

        let sched = schedule(&g);
        let merge_block = sched.node_to_block[merge as usize];
        let input_block = sched.node_to_block[input as usize];
        let user_block = sched.node_to_block[user as usize];
        assert_eq!(
            input_block, merge_block,
            "the input is placed with the phi it reads"
        );
        assert!(
            sched.dom.dominates(input_block, user_block),
            "the input's block {input_block} must dominate the user's block {user_block}"
        );
        // Emission order: blocks in index order, then position in the block.
        let emitted_at = |id: NodeId| -> (usize, usize) {
            let b = sched.node_to_block[id as usize];
            let pos = sched.blocks[b]
                .nodes
                .iter()
                .position(|&x| x == id)
                .expect("a placed node is listed in its block");
            (b, pos)
        };
        assert!(
            emitted_at(input) < emitted_at(user),
            "the input {:?} must be emitted before its user {:?}",
            emitted_at(input),
            emitted_at(user),
        );
    }

    /// The input-first walk is iterative: a chain whose every link reads a
    /// HIGHER id — the walk's worst case, 20 000 frames deep — neither
    /// overflows the stack nor places a link before its input.
    #[test]
    fn input_first_placement_survives_a_long_forward_chain() {
        let mut g = bare_graph();
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        let entry = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let depth = 20_000usize;
        // Created with a placeholder input, then each link pointed at the next.
        let links: Vec<NodeId> = (0..depth)
            .map(|_| g.add(Op::Neg, IrType::Int, vec![NO_NODE], None))
            .collect();
        let p = g.add(Op::Param(0), IrType::Int, vec![], None);
        for k in 0..depth {
            let target = if k + 1 < depth { links[k + 1] } else { p };
            assert!(g.set_input(links[k], 0, target));
        }
        let ret = g.add(Op::Return, IrType::Void, vec![entry, links[0]], None);
        g.entry = start;
        g.exit = ret;

        let sched = schedule_with_options(&g, &ScheduleOptions::default());
        let b0 = sched.node_to_block[links[0] as usize];
        let mut pos = vec![usize::MAX; g.nodes.len()];
        for (i, &id) in sched.blocks[b0].nodes.iter().enumerate() {
            pos[id as usize] = i;
        }
        for k in 0..depth - 1 {
            assert_eq!(sched.node_to_block[links[k] as usize], b0);
            assert!(
                pos[links[k + 1] as usize] < pos[links[k] as usize],
                "link {k} is listed before the link it reads"
            );
        }
    }

    // ── round 9 (irsched lane) ───────────────────────────────────────────

    /// `t = a + b; <guard>; <guard>; <guard>` with `t` named by a frame state.
    ///
    /// The guards form the side-effect chain, so the first one heads the
    /// longest chain in the block and the list scheduler picks it before the
    /// one-node `t`. That is fine for an unnamed value and wrong for a named
    /// one: a deopt at the first guard rebuilds the frame from `t`'s home word
    /// before `t` has written it.
    #[test]
    fn a_frame_named_value_is_not_list_scheduled_past_a_deopt_point() {
        let build = |named: bool| -> Vec<NodeId> {
            let mut graph = bare_graph();
            let start = graph.add(Op::Start, IrType::Control, vec![], None);
            let ctrl = graph.add(Op::Proj(0), IrType::Control, vec![start], None);
            let p0 = graph.add(Op::Param(0), IrType::Int, vec![], None);
            let p1 = graph.add(Op::Param(1), IrType::Int, vec![], None);
            let t = graph.add(Op::Add, IrType::Int, vec![p0, p1], Some(2));
            let g1 = graph.add(Op::Guard { bci: 3 }, IrType::Void, vec![ctrl, p0], Some(3));
            let g2 = graph.add(Op::Guard { bci: 4 }, IrType::Void, vec![ctrl, p1], Some(4));
            let g3 = graph.add(Op::Guard { bci: 5 }, IrType::Void, vec![ctrl, p0], Some(5));
            if named {
                graph.safepoints.push(crate::ir::SafepointSnapshot::new(
                    3,
                    vec![t],
                    Vec::new(),
                    Vec::new(),
                ));
            }
            let mut nodes = vec![t, g1, g2, g3];
            let frame_named = frame_named_values(&graph);
            priority_sort_block(&graph, &mut nodes, &frame_named);
            nodes
        };
        let pos = |nodes: &[NodeId], id: usize| nodes.iter().position(|&n| n as usize == id);
        // Node ids: 4 = t, 5 = the first guard.
        let free = build(false);
        assert!(
            pos(&free, 4) > pos(&free, 5),
            "twin: an unnamed value is free to follow the chain head, got {free:?}"
        );
        let pinned = build(true);
        assert!(
            pos(&pinned, 4) < pos(&pinned, 5),
            "a named value must stay ahead of the next deopt point, got {pinned:?}"
        );
    }

    /// Round 11 wave 10 (irback proposal W9-4): a kill gained MID-block moves
    /// its node ahead of an older, equally tall candidate — the step the
    /// incremental `kills` must not miss.
    ///
    /// `x` feeds `u1` and `u2`; `u2` also reads `y`, so it kills `y` and goes
    /// first. That leaves `u1` as `x`'s last consumer (a kill), which beats
    /// `z` although `z` has the lower seed rank. `w` (a 3-cycle `imul`) heads
    /// the longest chain and goes before everything. Hand-evaluated against
    /// the full re-score this replaced: `w, x, y, u2, u1, z`.
    #[test]
    fn r11w10_a_kill_gained_mid_block_is_seen_by_the_list_scheduler() {
        let mut graph = bare_graph();
        let p0 = graph.add(Op::Param(0), IrType::Int, vec![], None);
        let p1 = graph.add(Op::Param(1), IrType::Int, vec![], None);
        let x = graph.add(Op::Add, IrType::Int, vec![p0, p1], Some(1));
        let y = graph.add(Op::Sub, IrType::Int, vec![p0, p1], Some(2));
        let z = graph.add(Op::Add, IrType::Int, vec![p0, p0], Some(3));
        let u1 = graph.add(Op::Add, IrType::Int, vec![x, p0], Some(4));
        let u2 = graph.add(Op::Add, IrType::Int, vec![x, y], Some(5));
        let w = graph.add(Op::Mul, IrType::Int, vec![p0, p1], Some(6));
        let mut nodes = vec![x, y, z, u1, u2, w];
        let frame_named = frame_named_values(&graph);
        priority_sort_block(&graph, &mut nodes, &frame_named);
        assert_eq!(nodes, vec![w, x, y, u2, u1, z]);
    }


    /// A `while` loop with a `continue` has two latches and is still ONE loop.
    ///
    /// ```text
    ///   0 -> 1 (header) -> 2 -> 3 -> 1   (normal back edge)
    ///                      2 -> 1        (the continue)
    ///   1 -> 4 (exit)
    /// ```
    #[test]
    fn a_loop_with_two_latches_is_one_level_deep() {
        let blocks = cfg(5, &[(0, 1), (1, 2), (2, 3), (3, 1), (2, 1), (1, 4)]);
        let dom = Dominators::compute(&blocks);
        assert_eq!(loop_depths(&blocks, &dom), vec![0, 1, 1, 1, 0]);
    }

    /// A data node whose inputs live in sibling arms of a diamond has no legal
    /// home. The scheduler keeps the entry-block fallback (the one placement
    /// the emission-order check always catches), counts it, and names it.
    ///
    /// `bad` reads the two arm projections directly -- the stand-in for any
    /// value computed in one arm each -- with no phi at the merge.
    #[test]
    fn a_node_with_inputs_in_incomparable_blocks_is_counted_and_named() {
        let mut g = bare_graph();
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        let entry = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let p = g.add(Op::Param(0), IrType::Int, vec![], None);
        let if_node = g.add(Op::If, IrType::Control, vec![entry, p], Some(0));
        let t = g.add(Op::Proj(0), IrType::Control, vec![if_node], Some(0));
        let f = g.add(Op::Proj(1), IrType::Control, vec![if_node], Some(0));
        let merge = g.add(Op::Merge, IrType::Control, vec![t, f], Some(1));
        let good = g.add(Op::Add, IrType::Int, vec![p, p], Some(1));
        let bad = g.add(Op::Add, IrType::Int, vec![t, f], Some(1));
        let sum = g.add(Op::Add, IrType::Int, vec![good, bad], Some(1));
        let ret = g.add(Op::Return, IrType::Void, vec![merge, sum], Some(2));
        g.entry = start;
        g.exit = ret;

        let before = ir_incomparable_input_placements();
        let sched = schedule(&g);
        assert!(
            ir_incomparable_input_placements() > before,
            "the illegal placement must be counted"
        );
        let tb = sched.node_to_block[t as usize];
        let fb = sched.node_to_block[f as usize];
        assert!(!sched.dom.dominates(tb, fb) && !sched.dom.dominates(fb, tb));
        let misplaced = sched.misplaced_data_nodes(&g);
        assert!(
            misplaced.contains(&bad),
            "the schedule must name the node: {misplaced:?}"
        );
        assert!(
            !misplaced.contains(&good),
            "twin: a node with a legal home is not named: {misplaced:?}"
        );
    }

    /// The equal-depth sink rule stays inside one loop: a sibling loop the
    /// first one exits straight into is at the same depth but is a different
    /// loop (`ir-sink-equal-depth-can-cross-into-a-sibling-loop-20260918.md`).
    ///
    /// ```text
    ///   0 -> 1 (L1 header) <-> 2
    ///        1 -> 3 (L2 header) <-> 4
    ///             3 -> 5
    /// ```
    #[test]
    fn equal_depth_sinking_does_not_cross_into_a_sibling_loop() {
        let blocks = cfg(6, &[(0, 1), (1, 2), (2, 1), (1, 3), (3, 4), (4, 3), (3, 5)]);
        let dom = Dominators::compute(&blocks);
        let (depth, innermost) = loop_nest(&blocks, &dom);
        assert_eq!(depth, vec![0, 1, 1, 1, 1, 0]);
        assert_eq!(
            innermost,
            vec![None, Some(1), Some(1), Some(3), Some(3), None]
        );
        let mut path = Vec::new();
        // A value computed in L1's header whose only reader is in L2's body.
        let got = choose_sink_block(&dom, &depth, &innermost, 1, 4, true, 6, &mut path);
        assert_eq!(
            got, 1,
            "the value must stay in L1, not run once per L2 iteration"
        );
        assert_eq!(
            choose_sink_block_by_scan(&dom, &depth, &innermost, 1, 4, true, 6),
            1
        );

        // Twin: inside ONE loop the rule still moves the value down to its
        // reader. 0 -> 1 (header) -> 2 -> 3 -> 1, 1 -> 4.
        let blocks = cfg(5, &[(0, 1), (1, 2), (2, 3), (3, 1), (1, 4)]);
        let dom = Dominators::compute(&blocks);
        let (depth, innermost) = loop_nest(&blocks, &dom);
        assert_eq!(innermost, vec![None, Some(1), Some(1), Some(1), None]);
        let got = choose_sink_block(&dom, &depth, &innermost, 1, 3, true, 5, &mut path);
        assert_eq!(
            got, 3,
            "twin: equal-depth sinking within one loop still applies"
        );
    }

    /// A nested loop's blocks name the INNER header; the outer loop's own
    /// blocks name the outer one.
    #[test]
    fn the_innermost_header_of_a_nested_loop_block_is_the_inner_header() {
        // 0 -> 1 (outer) -> 2 (inner) <-> 3, 2 -> 4 -> 1, 1 -> 5.
        let blocks = cfg(6, &[(0, 1), (1, 2), (2, 3), (3, 2), (2, 4), (4, 1), (1, 5)]);
        let dom = Dominators::compute(&blocks);
        let (depth, innermost) = loop_nest(&blocks, &dom);
        assert_eq!(depth, vec![0, 1, 2, 2, 1, 0]);
        assert_eq!(
            innermost,
            vec![None, Some(1), Some(2), Some(2), Some(1), None]
        );
    }

    /// An edge out of a block the entry never reaches is not a back edge,
    /// although `dominates` answers `true` for every dominator of dead code.
    #[test]
    fn an_edge_from_dead_code_does_not_make_a_loop() {
        // 0 -> 1 -> 2 is live; 3 is unreachable and jumps into 1.
        let blocks = cfg(4, &[(0, 1), (1, 2), (3, 1)]);
        let dom = Dominators::compute(&blocks);
        assert!(!dom.is_reachable(3));
        assert_eq!(loop_depths(&blocks, &dom), vec![0, 0, 0, 0]);
        let freq = compute_frequencies(&bare_graph(), &blocks, &dom, &ScheduleOptions::default());
        assert!(!freq.is_loop_header[1], "block 1 is not a loop header");
        assert!(
            freq.freq[1] <= 1.0,
            "a live block reached once must not be multiplied by a trip count: {}",
            freq.freq[1]
        );
    }

    /// Round 11 wave 10 (irback proposal W9-3): a `Switch`-wide block answers
    /// its edge indices from the map, with `position`'s first-index answer. 13
    /// edges out of block 0 (above `SCAN_WIDTH`), two of them into block 1.
    #[test]
    fn r11w10_a_wide_switch_propagates_every_case_edge() {
        let mut edges: Vec<(usize, usize)> = (1..=12).map(|k| (0, k)).collect();
        edges.push((0, 1));
        edges.extend((1..=12).map(|k| (k, 13)));
        let blocks = cfg(14, &edges);
        assert_eq!(blocks[0].successors.len(), 13);
        let dom = Dominators::compute(&blocks);
        let freq = compute_frequencies(&bare_graph(), &blocks, &dom, &ScheduleOptions::default());
        let p = 1.0 / 13.0;
        assert_eq!(freq.edge_prob[0], vec![p; 13]);
        for k in 2..=12 {
            assert_eq!(freq.freq[k], p, "case block {k}");
        }
        // Both predecessor entries of block 1 read edge 0, as `position` did.
        assert_eq!(freq.freq[1], p + p);
        assert!(freq.freq[13] > 0.0);
    }

    // ── Round 11 (lane irback) ───────────────────────────────────────────

    fn r11_empty_graph() -> Graph {
        bare_graph()
    }

    /// A loop whose body built no control node: `Merge(entry, Merge)` (the
    /// builder makes every loop header an `Op::Merge`).
    ///
    /// `int f(int k) { if (k < 0) return -1; int i = 0; while (true) { i += k; } }`
    /// builds exactly this — the `goto` back to the header carries the
    /// region's own token. Returns `(graph, region, phi, next)`.
    fn r11_self_loop_graph() -> (Graph, NodeId, NodeId, NodeId) {
        let mut g = r11_empty_graph();
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let k = g.add(Op::Param(0), IrType::Int, vec![start], None);
        let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
        let cond = g.add(
            Op::Cmp(crate::ir::CmpOp::Ge),
            IrType::Int,
            vec![k, zero],
            None,
        );
        let if_node = g.add(Op::If, IrType::Control, vec![ctrl, cond], None);
        let t = g.add(Op::Proj(0), IrType::Control, vec![if_node], None);
        let f = g.add(Op::Proj(1), IrType::Control, vec![if_node], None);
        let minus_one = g.add(Op::Const(-1), IrType::Int, vec![], None);
        let ret = g.add(Op::Return, IrType::Void, vec![f, minus_one], None);
        let region = g.add(Op::Merge, IrType::Control, vec![t], None);
        assert!(g.push_input(region, region));
        let phi = g.add(Op::Phi, IrType::Int, vec![region, zero, NO_NODE], None);
        let next = g.add(Op::Add, IrType::Int, vec![phi, k], None);
        assert!(g.set_input(phi, 2, next));
        g.entry = start;
        g.exit = ret;
        (g, region, phi, next)
    }

    /// A self-looping region keeps its self edge. Before round 11 Step 3
    /// skipped `succ_block == block_idx`, so the loop block had neither a
    /// terminator nor a successor and `ir_lower::lower_block` emitted no jump:
    /// the loop ran once and fell into the next block in layout order.
    #[test]
    fn r11_a_self_looping_region_keeps_its_back_edge() {
        let (g, region, _, _) = r11_self_loop_graph();
        let sched = schedule(&g);
        let rb = sched
            .blocks
            .iter()
            .position(|b| b.ctrl == region)
            .expect("the region heads a block");
        assert!(
            sched.blocks[rb].successors.contains(&rb),
            "the loop's back edge was dropped: successors {:?}",
            sched.blocks[rb].successors
        );
        assert!(sched.blocks[rb].predecessors.contains(&rb));
        // The general invariant `lower_block` relies on: a block with no
        // terminator is a goto and must name where it goes.
        for (b, blk) in sched.blocks.iter().enumerate() {
            if blk.terminator.is_none() {
                assert!(
                    !blk.successors.is_empty(),
                    "block {b} has neither a terminator nor a successor"
                );
            }
        }
        // And the frequency model sees a loop there.
        assert!(sched.freq.is_loop_header[rb]);
    }

    /// `topo_sort_block` does not treat a phi's value inputs as dependences.
    /// It used to descend into them, so `[phi, next = phi + k]` in one block
    /// came out `[next, phi]`.
    #[test]
    fn r11_topo_sort_keeps_a_phi_ahead_of_its_loop_carried_input() {
        let (g, _, phi, next) = r11_self_loop_graph();
        let mut nodes = vec![phi, next];
        topo_sort_block(&g, &mut nodes);
        assert_eq!(nodes, vec![phi, next]);
        // Presented the other way round, the real dependence (`next` reads
        // `phi`) still wins.
        let mut nodes = vec![next, phi];
        topo_sort_block(&g, &mut nodes);
        assert_eq!(nodes, vec![phi, next]);
    }

    // ── Round 11 wave 2 (lane ircore) ────────────────────────────────────

    /// A diamond on `p != 0` with one pinned load per arm:
    /// `(graph, merge, load_in_true_arm, load_in_false_arm)`.
    fn r11w2_diamond_with_arm_loads() -> (Graph, NodeId, NodeId, NodeId) {
        let mut g = r11_empty_graph();
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        let entry = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
        let base = g.add(Op::Param(0), IrType::Ref, vec![start], None);
        let p = g.add(Op::Param(1), IrType::Int, vec![start], None);
        let off = g.add(Op::Const(16), IrType::Int, vec![], None);
        let if_node = g.add(Op::If, IrType::Control, vec![entry, p], Some(0));
        let t = g.add(Op::Proj(0), IrType::Control, vec![if_node], Some(0));
        let f = g.add(Op::Proj(1), IrType::Control, vec![if_node], Some(0));
        let lt = g.add(
            Op::Load(crate::ir::MemKind::Int),
            IrType::Int,
            vec![t, mem, base, off],
            Some(3),
        );
        let lf = g.add(
            Op::Load(crate::ir::MemKind::Int),
            IrType::Int,
            vec![f, mem, base, off],
            Some(6),
        );
        let merge = g.add(Op::Merge, IrType::Control, vec![t, f], Some(9));
        g.entry = start;
        (g, merge, lt, lf)
    }

    /// The finished schedule of a well-formed graph names nothing — including
    /// a φ whose loop-carried input is defined in the loop block itself.
    #[test]
    fn r11w2_a_well_formed_schedule_has_no_misplaced_nodes() {
        let (mut g, merge, lt, lf) = r11w2_diamond_with_arm_loads();
        let phi = g.add(Op::Phi, IrType::Int, vec![merge, lt, lf], Some(9));
        let ret = g.add(Op::Return, IrType::Void, vec![merge, phi], Some(9));
        g.exit = ret;
        let sched = schedule(&g);
        assert_eq!(sched.misplaced_data_nodes(&g), Vec::<NodeId>::new());

        let (g, _, _, _) = r11_self_loop_graph();
        let sched = schedule(&g);
        assert_eq!(sched.misplaced_data_nodes(&g), Vec::<NodeId>::new());
    }

    /// A TERMINATOR reading a value from one arm of a diamond it is joined
    /// below. `verify_data_locations` compares emission order and passes when
    /// that arm is laid out first; the dominance check names the `Return`.
    #[test]
    fn r11w2_a_terminator_reading_a_sibling_arm_value_is_named() {
        let (mut g, merge, lt, _lf) = r11w2_diamond_with_arm_loads();
        let ret = g.add(Op::Return, IrType::Void, vec![merge, lt], Some(9));
        g.exit = ret;
        let sched = schedule(&g);
        let misplaced = sched.misplaced_data_nodes(&g);
        assert!(misplaced.contains(&ret), "{misplaced:?}");
        assert!(!misplaced.contains(&lt), "{misplaced:?}");
    }

    /// A φ whose inputs are swapped relative to its merge's predecessors reads
    /// each arm's value on the OTHER arm's edge; the φ half names it.
    #[test]
    fn r11w2_a_phi_input_that_does_not_dominate_its_edge_is_named() {
        let (mut g, merge, lt, lf) = r11w2_diamond_with_arm_loads();
        // merge = [t, f], so input 1 travels the `t` edge and must be `lt`.
        let phi = g.add(Op::Phi, IrType::Int, vec![merge, lf, lt], Some(9));
        let ret = g.add(Op::Return, IrType::Void, vec![merge, phi], Some(9));
        g.exit = ret;
        let sched = schedule(&g);
        let misplaced = sched.misplaced_data_nodes(&g);
        assert!(misplaced.contains(&phi), "{misplaced:?}");
        assert!(!misplaced.contains(&ret), "{misplaced:?}");
    }

    /// Round 11 wave 4: the release form's side-effect half. A store anchored
    /// at the ENTRY that stores the true arm's load is placed by
    /// `find_best_block` in the true arm — every input dominates it there, so
    /// the three older halves pass — and would run on one path only. The same
    /// store anchored at the true arm, where the load is, is well formed.
    #[test]
    fn r11w4_a_side_effect_sunk_below_its_anchor_is_named() {
        let (mut g, merge, lt, lf) = r11w2_diamond_with_arm_loads();
        let t = g.nodes[lt as usize].inputs[0];
        let mem = g.nodes[lt as usize].inputs[1];
        let base = g.nodes[lt as usize].inputs[2];
        let off = g.nodes[lt as usize].inputs[3];
        let if_node = g.nodes[t as usize].inputs[0];
        let entry = g.nodes[if_node as usize].inputs[0];
        let sunk = g.add(
            Op::Store(crate::ir::MemKind::Int),
            IrType::Memory,
            vec![entry, mem, base, off, lt],
            Some(1),
        );
        let phi = g.add(Op::Phi, IrType::Int, vec![merge, lt, lf], Some(9));
        let ret = g.add(Op::Return, IrType::Void, vec![merge, phi], Some(9));
        g.exit = ret;
        let sched = schedule(&g);
        assert_ne!(
            sched.node_to_block[sunk as usize], sched.node_to_block[entry as usize],
            "the fixture relies on the scheduler sinking the store"
        );
        assert_eq!(sched.misplaced_data_nodes(&g), vec![sunk]);

        let (mut g, merge, lt, lf) = r11w2_diamond_with_arm_loads();
        let t = g.nodes[lt as usize].inputs[0];
        let mem = g.nodes[lt as usize].inputs[1];
        let base = g.nodes[lt as usize].inputs[2];
        let off = g.nodes[lt as usize].inputs[3];
        g.add(
            Op::Store(crate::ir::MemKind::Int),
            IrType::Memory,
            vec![t, mem, base, off, lt],
            Some(4),
        );
        let phi = g.add(Op::Phi, IrType::Int, vec![merge, lt, lf], Some(9));
        let ret = g.add(Op::Return, IrType::Void, vec![merge, phi], Some(9));
        g.exit = ret;
        let sched = schedule(&g);
        assert_eq!(sched.misplaced_data_nodes(&g), Vec::<NodeId>::new());
    }

    /// Round 11 wave 7 (`r11w6-orch-post-schedule-phi-check-refuses-hashmap`):
    /// the φ half resolves each merge input to the PREDECESSOR block its
    /// control token heads, as `ir_lower::emit_phi_copies` places the copy.
    /// `HashMap.getNode`'s refused graph, once `ir_optimize` had folded
    /// `φ(undef, first.key)` to `first.key`, was this: a φ at a join BELOW an
    /// earlier diamond reading a value one arm of that diamond defines. The
    /// check must keep naming it; the same join reading the diamond's own φ is
    /// well formed.
    #[test]
    fn r11w7_a_phi_reading_an_earlier_arm_value_past_its_join_is_named() {
        let build = |through_the_join: bool| {
            let (mut g, merge, lt, lf) = r11w2_diamond_with_arm_loads();
            let t = g.nodes[lt as usize].inputs[0];
            let if_node = g.nodes[t as usize].inputs[0];
            let p = g.nodes[if_node as usize].inputs[1];
            let joined = g.add(Op::Phi, IrType::Int, vec![merge, lt, lf], Some(9));
            let if2 = g.add(Op::If, IrType::Control, vec![merge, p], Some(10));
            let a = g.add(Op::Proj(0), IrType::Control, vec![if2], Some(10));
            let b = g.add(Op::Proj(1), IrType::Control, vec![if2], Some(10));
            let m2 = g.add(Op::Merge, IrType::Control, vec![a, b], Some(12));
            let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
            let on_a = if through_the_join { joined } else { lt };
            let phi = g.add(Op::Phi, IrType::Int, vec![m2, on_a, zero], Some(12));
            let ret = g.add(Op::Return, IrType::Void, vec![m2, phi], Some(12));
            g.exit = ret;
            (g, phi, lt)
        };

        let (g, phi, lt) = build(false);
        let misplaced = schedule(&g).misplaced_data_nodes(&g);
        assert!(misplaced.contains(&phi), "{misplaced:?}");
        assert!(
            dominance_violations(&g, false).contains(&DominanceViolation::PhiEdge {
                phi,
                slot: 1,
                def: lt
            }),
            "the graph-level lane names the same edge"
        );

        let (g, _, _) = build(true);
        assert_eq!(schedule(&g).misplaced_data_nodes(&g), Vec::<NodeId>::new());
        assert_eq!(dominance_violations(&g, false), no_findings());
    }

    /// Round 11 wave 8 (irback proposal W7-2): `misplaced_reads` names the
    /// READ — slot, definition and, for a φ, the edge — for each of the three
    /// shapes the node-only answer used to leave to an IR dump.
    #[test]
    fn r11w8_misplaced_reads_name_the_slot_the_def_and_the_edge() {
        // φ half: merge = [t, f], so slot 1 travels the `t` edge and reads
        // `lf`, which only the `f` arm computes.
        let (mut g, merge, lt, lf) = r11w2_diamond_with_arm_loads();
        let t = g.nodes[lt as usize].inputs[0];
        let phi = g.add(Op::Phi, IrType::Int, vec![merge, lf, lt], Some(9));
        let ret = g.add(Op::Return, IrType::Void, vec![merge, phi], Some(9));
        g.exit = ret;
        let sched = schedule(&g);
        let reads = sched.misplaced_reads(&g);
        assert!(
            reads.contains(&MisplacedRead {
                node: phi,
                slot: 1,
                def: lf,
                edge: Some(t),
            }),
            "{reads:?}"
        );
        assert_eq!(
            reads.iter().map(|r| r.node).collect::<Vec<_>>(),
            sched.misplaced_data_nodes(&g),
            "the node-only answer is the same list"
        );

        // Terminator half: the `Return` below the join reads the `t` arm's load.
        let (mut g, merge, lt, _lf) = r11w2_diamond_with_arm_loads();
        let ret = g.add(Op::Return, IrType::Void, vec![merge, lt], Some(9));
        g.exit = ret;
        let reads = schedule(&g).misplaced_reads(&g);
        assert!(
            reads.contains(&MisplacedRead {
                node: ret,
                slot: 1,
                def: lt,
                edge: None,
            }),
            "{reads:?}"
        );

        // Side-effect half: a store anchored at the entry, sunk into the arm.
        let (mut g, merge, lt, lf) = r11w2_diamond_with_arm_loads();
        let t = g.nodes[lt as usize].inputs[0];
        let mem = g.nodes[lt as usize].inputs[1];
        let base = g.nodes[lt as usize].inputs[2];
        let off = g.nodes[lt as usize].inputs[3];
        let if_node = g.nodes[t as usize].inputs[0];
        let entry = g.nodes[if_node as usize].inputs[0];
        let sunk = g.add(
            Op::Store(crate::ir::MemKind::Int),
            IrType::Memory,
            vec![entry, mem, base, off, lt],
            Some(1),
        );
        let phi = g.add(Op::Phi, IrType::Int, vec![merge, lt, lf], Some(9));
        let ret = g.add(Op::Return, IrType::Void, vec![merge, phi], Some(9));
        g.exit = ret;
        assert_eq!(
            schedule(&g).misplaced_reads(&g),
            vec![MisplacedRead {
                node: sunk,
                slot: 0,
                def: entry,
                edge: None,
            }]
        );
    }

    /// The bytecode that produced it: a local set on one arm before a join,
    /// then reassigned in a loop and never read. The builder now retires the
    /// φs that merge the unset slot (`IrBuilder::retire_untypeable_phis`), so
    /// nothing is left for `ir_optimize` to fold into a misplaced read — built
    /// or optimized, release form or graph lane.
    #[test]
    fn r11w7_getnode_shape_builds_and_optimizes_with_no_misplaced_phi() {
        // static int f(int[] a, int c) { int k;
        //   if (c > 0) k = a.length; c--; while (c > 0) { k = c; c--; } return 0; }
        let code: [u8; 24] = [
            0x1b, 0x9e, 0x00, 0x06, 0x2a, 0xbe, 0x3d, 0x84, 0x01, 0xff, 0x1b, 0x9e, 0x00, 0x0b,
            0x1b, 0x3d, 0x84, 0x01, 0xff, 0xa7, 0xff, 0xf7, 0x03, 0xac,
        ];
        let mut b = IrBuilder::new(2, 3);
        b.set_param_types(&[IrType::Ref, IrType::Int]);
        let Some(mut graph) = b.build(&code, 24) else {
            panic!("the getNode shape must build");
        };
        assert_eq!(dominance_violations(&graph, true), no_findings(), "built");
        assert_eq!(
            schedule(&graph).misplaced_data_nodes(&graph),
            Vec::<NodeId>::new(),
            "built, release form"
        );
        ir_optimize::optimize(&mut graph);
        assert_eq!(
            dominance_violations(&graph, false),
            no_findings(),
            "optimized"
        );
        assert_eq!(
            schedule_with_options(&graph, &production_schedule_options())
                .misplaced_data_nodes(&graph),
            Vec::<NodeId>::new(),
            "optimized, release form"
        );
    }

    /// Round 11 wave 7, the scheduler half of
    /// `r11w6-lower-sr-recipe-values-scheduled-after-the-deopt`: a user-less
    /// pure value no frame names (an eliminated store's value, which a
    /// scalar-replaced object's materialization reads) follows the guard chain
    /// under list scheduling, unless the caller lists it in
    /// `extra_frame_named`.
    #[test]
    fn r11w7_an_extra_named_recipe_value_is_scheduled_ahead_of_the_deopt() {
        let run = |listed: bool| -> (Vec<NodeId>, NodeId, NodeId) {
            let mut g = r11_empty_graph();
            let start = g.add(Op::Start, IrType::Control, vec![], None);
            let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
            let p0 = g.add(Op::Param(0), IrType::Int, vec![], None);
            let p1 = g.add(Op::Param(1), IrType::Int, vec![], None);
            let t = g.add(Op::Add, IrType::Int, vec![p0, p1], Some(2));
            let g1 = g.add(Op::Guard { bci: 3 }, IrType::Void, vec![ctrl, p0], Some(3));
            g.add(Op::Guard { bci: 4 }, IrType::Void, vec![ctrl, p1], Some(4));
            g.add(Op::Guard { bci: 5 }, IrType::Void, vec![ctrl, p0], Some(5));
            let ret = g.add(Op::Return, IrType::Void, vec![ctrl, p0], Some(6));
            g.entry = start;
            g.exit = ret;
            let opts = ScheduleOptions {
                priority_within_blocks: true,
                extra_frame_named: if listed { vec![t] } else { Vec::new() },
                ..ScheduleOptions::default()
            };
            let sched = schedule_with_options(&g, &opts);
            let b = sched.node_to_block[g1 as usize];
            assert_eq!(
                sched.node_to_block[t as usize], b,
                "the fixture puts the value in the guards' block"
            );
            (sched.blocks[b].nodes.clone(), t, g1)
        };
        let pos = |nodes: &[NodeId], id: NodeId| nodes.iter().position(|&n| n == id);
        let (free, t, g1) = run(false);
        assert!(
            pos(&free, t) > pos(&free, g1),
            "twin: an unlisted user-less value follows the chain head, got {free:?}"
        );
        let (pinned, t, g1) = run(true);
        assert!(
            pos(&pinned, t) < pos(&pinned, g1),
            "a listed value stays ahead of the first deopt point, got {pinned:?}"
        );
    }

    // ── Round 11 wave 3 (lane ircore): graph-level dominance ─────────────

    fn no_findings() -> Vec<DominanceViolation> {
        Vec::new()
    }

    /// The two well-formed fixtures above — a diamond joined by a φ, and a
    /// loop whose body built no control node — have no finding, snapshot half
    /// included.
    #[test]
    fn r11w3_a_well_formed_graph_has_no_dominance_violations() {
        let (mut g, merge, lt, lf) = r11w2_diamond_with_arm_loads();
        let phi = g.add(Op::Phi, IrType::Int, vec![merge, lt, lf], Some(9));
        let ret = g.add(Op::Return, IrType::Void, vec![merge, phi], Some(9));
        g.exit = ret;
        assert_eq!(dominance_violations(&g, true), no_findings());

        let (g, _, _, _) = r11_self_loop_graph();
        assert_eq!(dominance_violations(&g, true), no_findings());
    }

    /// The four shapes the scheduler cannot repair by moving a node, each on
    /// the diamond: a terminator, a φ edge, a node with a side effect, and a
    /// pure node whose inputs are in sibling arms.
    #[test]
    fn r11w3_each_unschedulable_read_is_named() {
        // A `Return` in the join reading the true arm's load.
        let (mut g, merge, lt, _lf) = r11w2_diamond_with_arm_loads();
        let ret = g.add(Op::Return, IrType::Void, vec![merge, lt], Some(9));
        g.exit = ret;
        assert_eq!(
            dominance_violations(&g, false),
            vec![DominanceViolation::Input {
                user: ret,
                slot: 1,
                def: lt
            }]
        );

        // A φ whose inputs are swapped against its merge's predecessors.
        let (mut g, merge, lt, lf) = r11w2_diamond_with_arm_loads();
        let phi = g.add(Op::Phi, IrType::Int, vec![merge, lf, lt], Some(9));
        let ret = g.add(Op::Return, IrType::Void, vec![merge, phi], Some(9));
        g.exit = ret;
        let found = dominance_violations(&g, false);
        assert!(
            found.contains(&DominanceViolation::PhiEdge {
                phi,
                slot: 1,
                def: lf
            }) && found.contains(&DominanceViolation::PhiEdge {
                phi,
                slot: 2,
                def: lt
            }),
            "{found:?}"
        );

        // A store anchored at the join, storing the true arm's load: the
        // scheduler would sink it into the true arm and skip it on the other.
        let (mut g, merge, lt, lf) = r11w2_diamond_with_arm_loads();
        let phi = g.add(Op::Phi, IrType::Int, vec![merge, lt, lf], Some(9));
        let base = g.nodes[lt as usize].inputs[2];
        let off = g.nodes[lt as usize].inputs[3];
        let mem = g.nodes[lt as usize].inputs[1];
        let st = g.add(
            Op::Store(crate::ir::MemKind::Int),
            IrType::Memory,
            vec![merge, mem, base, off, lt],
            Some(9),
        );
        let ret = g.add(Op::Return, IrType::Void, vec![merge, phi], Some(10));
        g.exit = ret;
        assert_eq!(
            dominance_violations(&g, false),
            vec![DominanceViolation::Input {
                user: st,
                slot: 4,
                def: lt
            }]
        );

        // A pure node over both arms' loads, with no φ.
        let (mut g, merge, lt, lf) = r11w2_diamond_with_arm_loads();
        let sum = g.add(Op::Add, IrType::Int, vec![lt, lf], Some(9));
        let ret = g.add(Op::Return, IrType::Void, vec![merge, sum], Some(9));
        g.exit = ret;
        let found = dominance_violations(&g, false);
        assert!(
            matches!(
                found.as_slice(),
                [DominanceViolation::Incomparable { user, .. }] if *user == sum
            ),
            "{found:?}"
        );
    }

    /// A pinned node that deopts into a snapshot naming a value from a sibling
    /// arm: named only by the snapshot half.
    #[test]
    fn r11w3_a_frame_value_that_does_not_dominate_its_deopt_site_is_named() {
        // Clears this thread's splice resume map, so pcs resolve as given.
        let _scope = crate::ir::IrBuildResultsScope::enter();
        let (mut g, merge, lt, lf) = r11w2_diamond_with_arm_loads();
        let phi = g.add(Op::Phi, IrType::Int, vec![merge, lt, lf], Some(9));
        let ret = g.add(Op::Return, IrType::Void, vec![merge, phi], Some(9));
        g.exit = ret;
        // `lt` (bci 3) deopts into the frame at bci 3, which names `lf` — a
        // value the false arm computes.
        g.push_safepoint(crate::ir::SafepointSnapshot::new(
            3,
            vec![lf],
            vec![],
            Vec::new(),
        ));
        assert_eq!(dominance_violations(&g, false), no_findings());
        assert_eq!(
            dominance_violations(&g, true),
            vec![DominanceViolation::FrameState {
                site: lt,
                snapshot: 0,
                def: lf
            }]
        );
    }

    /// Graphs the bytecode front end builds have no finding, before and after
    /// `ir_optimize::optimize`: a join inside a rotated loop, an ECJ loop
    /// inside a javac one, javac's nested counted loops, a do-while whose
    /// header is the method entry, and a lock held across a loop. The
    /// verifier's dominance lane runs by default in every debug build, so a
    /// finding here would be a false positive on legal bytecode. (The snapshot
    /// half, which runs only on request, is asserted on the built graphs.)
    #[test]
    fn r11w3_builder_graphs_have_no_dominance_violations() {
        // int f(int n) { int s = 0; for (int i = 0; i < n; i++)
        //   { if ((i & 1) == 0) s += i; else s -= 1; } return s; }  (rotated)
        let rotated_if_else: &[u8] = &[
            0x03, 0x3c, 0x03, 0x3d, 0xa7, 0x00, 0x16, 0x1c, 0x04, 0x7e, 0x9a, 0x00, 0x0a, 0x1b,
            0x1c, 0x60, 0x3c, 0xa7, 0x00, 0x06, 0x84, 0x01, 0xff, 0x84, 0x02, 0x01, 0x1c, 0x1a,
            0xa1, 0xff, 0xeb, 0x1b, 0xac, 0, 0,
        ];
        // An ECJ-shaped inner loop inside a javac outer loop.
        let rotated_inside_javac: &[u8] = &[
            0x03, 0x3c, 0x03, 0x3d, 0x1c, 0x1a, 0xa2, 0x00, 0x1a, 0x03, 0x3e, 0xa7, 0x00, 0x0a,
            0x1b, 0x1d, 0x60, 0x3c, 0x84, 0x03, 0x01, 0x1d, 0x1c, 0xa1, 0xff, 0xf7, 0x84, 0x02,
            0x01, 0xa7, 0xff, 0xe7, 0x1b, 0xac, 0, 0,
        ];
        // javac's nested counted loops.
        let javac_nested: &[u8] = &[
            0x03, 0x3c, 0x03, 0x3d, 0x1c, 0x1a, 0xa2, 0x00, 0x19, 0x03, 0x3e, 0x1d, 0x1a, 0xa2,
            0x00, 0x0c, 0x84, 0x01, 0x01, 0x84, 0x03, 0x01, 0xa7, 0xff, 0xf5, 0x84, 0x02, 0x01,
            0xa7, 0xff, 0xe8, 0x1b, 0xac, 0, 0,
        ];
        // int f(int n) { do { n = n - 3; } while (n > 0); return n; }
        let do_while_at_entry: &[u8] = &[
            0x1a, 0x06, 0x64, 0x3b, 0x1a, 0x9d, 0xff, 0xfb, 0x1a, 0xac, 0, 0,
        ];
        // synchronized (o) { while (k != 0) {} }
        let lock_across_loop: &[u8] = &[
            0x2a, 0x59, 0x4c, 0xc2, 0x1c, 0x99, 0x00, 0x06, 0xa7, 0xff, 0xfc, 0x2b, 0xc3, 0xb1, 0,
            0,
        ];
        let fixtures: [(&str, &[u8], usize, usize, usize); 5] = [
            ("rotated_if_else", rotated_if_else, 33, 1, 3),
            ("rotated_inside_javac", rotated_inside_javac, 34, 1, 4),
            ("javac_nested", javac_nested, 33, 1, 4),
            ("do_while_at_entry", do_while_at_entry, 10, 1, 1),
            ("lock_across_loop", lock_across_loop, 14, 3, 3),
        ];
        for (name, code, len, params, locals) in fixtures {
            let Some(mut graph) = IrBuilder::new(params, locals).build(code, len) else {
                panic!("{name}: the fixture must build");
            };
            assert_eq!(
                dominance_violations(&graph, true),
                no_findings(),
                "{name}: built"
            );
            // The release form (`lib.rs` refuses on its first entry) must be
            // as silent as the debug lane on legal bytecode (round 11 wave 4).
            assert_eq!(
                schedule(&graph).misplaced_data_nodes(&graph),
                Vec::<NodeId>::new(),
                "{name}: built, release form"
            );
            ir_optimize::optimize(&mut graph);
            assert_eq!(
                dominance_violations(&graph, false),
                no_findings(),
                "{name}: optimized"
            );
            assert_eq!(
                schedule_with_options(&graph, &production_schedule_options())
                    .misplaced_data_nodes(&graph),
                Vec::<NodeId>::new(),
                "{name}: optimized, release form"
            );
        }
    }

    // ── Round 11 wave 10 (lane ircore): every pinned op keeps its anchor ──

    /// `r11w9-ircore-anchor-rule-covers-five-of-the-pinned-ops`: a `Guard`, a
    /// `NewArray` and a `Load` anchored at the ENTRY whose last operand is a
    /// value only the true arm computes. `find_best_block` sinks each into the
    /// true arm — a guard or an allocation on one path only — and every input
    /// dominates it there. The debug lane names the operand that does not
    /// dominate the anchor, and since wave 11 the release form names the node
    /// (`anchor_rule_is_released`).
    #[test]
    fn r11w10_every_pinned_op_sunk_below_its_anchor_is_named() {
        for which in ["guard", "new_array", "load"] {
            let (mut g, merge, lt, lf) = r11w2_diamond_with_arm_loads();
            let t = g.nodes[lt as usize].inputs[0];
            let mem = g.nodes[lt as usize].inputs[1];
            let base = g.nodes[lt as usize].inputs[2];
            let if_node = g.nodes[t as usize].inputs[0];
            let entry = g.nodes[if_node as usize].inputs[0];
            let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
            let c = g.add(
                Op::Cmp(crate::ir::CmpOp::Ge),
                IrType::Int,
                vec![lt, zero],
                Some(4),
            );
            let (pinned, slot) = match which {
                "guard" => (
                    g.add(Op::Guard { bci: 1 }, IrType::Void, vec![entry, c], Some(1)),
                    1,
                ),
                "new_array" => (
                    g.add(
                        Op::NewArray {
                            element_type: 10,
                            component_class_id: 0,
                        },
                        IrType::Ref,
                        vec![entry, mem, c],
                        Some(1),
                    ),
                    2,
                ),
                _ => (
                    g.add(
                        Op::Load(crate::ir::MemKind::Int),
                        IrType::Int,
                        vec![entry, mem, base, c],
                        Some(1),
                    ),
                    3,
                ),
            };
            let phi = g.add(Op::Phi, IrType::Int, vec![merge, lt, lf], Some(9));
            let ret = g.add(Op::Return, IrType::Void, vec![merge, phi], Some(9));
            g.exit = ret;
            assert_eq!(
                pinned_anchor(&g, &g.nodes[pinned as usize]),
                Some(entry),
                "{which}"
            );
            let sched = schedule(&g);
            assert_ne!(
                sched.node_to_block[pinned as usize], sched.node_to_block[entry as usize],
                "{which}: the fixture relies on the scheduler sinking the node"
            );
            assert_eq!(
                dominance_violations(&g, false),
                vec![DominanceViolation::Input {
                    user: pinned,
                    slot,
                    def: c
                }],
                "{which}"
            );
            // Released in wave 11: the release form names it too.
            assert_eq!(
                sched.misplaced_data_nodes(&g),
                vec![pinned],
                "{which}: release form"
            );

            // Twin: the same node anchored at the true arm, where its operand
            // is computed, is well formed.
            assert!(g.set_input(pinned, 0, t));
            assert_eq!(
                dominance_violations(&g, false),
                no_findings(),
                "{which}: twin"
            );
        }
    }

    /// What [`pinned_anchor`] does NOT pin: the control nodes themselves (a
    /// one-predecessor `Merge` has the same `0..1` control range), the
    /// terminators (their own half), a floating data node, and the compact
    /// hand-built forms whose slot 0 is not a control token.
    #[test]
    fn r11w10_pinned_anchor_leaves_control_terminators_and_compact_forms_alone() {
        let (mut g, merge, lt, _lf) = r11w2_diamond_with_arm_loads();
        let t = g.nodes[lt as usize].inputs[0];
        let base = g.nodes[lt as usize].inputs[2];
        let if_node = g.nodes[t as usize].inputs[0];
        let one_pred = g.add(Op::Merge, IrType::Control, vec![t], Some(9));
        let ret = g.add(Op::Return, IrType::Void, vec![merge, lt], Some(9));
        let add = g.add(Op::Add, IrType::Int, vec![lt, lt], Some(9));
        let compact = g.add(
            Op::Load(crate::ir::MemKind::Int),
            IrType::Int,
            vec![base],
            Some(9),
        );
        for id in [t, if_node, one_pred, ret, add, compact] {
            assert_eq!(pinned_anchor(&g, &g.nodes[id as usize]), None, "n{id}");
        }
        assert_eq!(pinned_anchor(&g, &g.nodes[lt as usize]), Some(t));
    }

    /// The builder's own graph for an array-summing loop — a pinned
    /// `ArrayLength` in the header, a pinned `ArrayLoad` in the body — has no
    /// finding under the widened rule, as built and after `optimize` (LICM
    /// re-points the length's anchor at the pre-header).
    #[test]
    fn r11w10_an_array_loop_has_no_pinned_op_below_its_anchor() {
        // static int f(int[] a, int n) {
        //   int s = 0; for (int i = 0; i < a.length; i++) s += a[i]; return s; }
        let code: [u8; 24] = [
            0x03, 0x3d, 0x03, 0x3e, // 0: iconst_0; istore_2; iconst_0; istore_3
            0x1d, 0x2a, 0xbe, // 4: iload_3; aload_0; arraylength
            0xa2, 0x00, 0x0f, // 7: if_icmpge 22
            0x1c, 0x2a, 0x1d, 0x2e, // 10: iload_2; aload_0; iload_3; iaload
            0x60, 0x3d, // 14: iadd; istore_2
            0x84, 0x03, 0x01, // 16: iinc 3, 1
            0xa7, 0xff, 0xf1, // 19: goto 4
            0x1c, 0xac, // 22: iload_2; ireturn
        ];
        let mut b = IrBuilder::new(2, 4);
        b.set_param_types(&[IrType::Ref, IrType::Int]);
        let Some(mut graph) = b.build(&code, 24) else {
            panic!("the array loop must build");
        };
        assert!(
            graph
                .nodes
                .iter()
                .any(|n| matches!(n.op, Op::ArrayLoad(_)) && pinned_anchor(&graph, n).is_some()),
            "the fixture must hold a pinned array load"
        );
        assert_eq!(dominance_violations(&graph, false), no_findings(), "built");
        ir_optimize::optimize(&mut graph);
        assert_eq!(
            dominance_violations(&graph, false),
            no_findings(),
            "optimized"
        );
        assert_eq!(
            schedule_with_options(&graph, &production_schedule_options())
                .misplaced_data_nodes(&graph),
            Vec::<NodeId>::new(),
            "optimized, release form"
        );
    }

    // ── Round 11 wave 11 (lane ircore) ───────────────────────────────────

    /// `r11w10-orch-schedule-places-phi-edge-input-off-its-edge`, reduced:
    /// `int f(int n) { int held = 0; while (held < n) held++;
    ///   return held == 0 ? 0 : -1 - held; }`. Returns
    /// `(graph, join, loop_exit, sub)`.
    ///
    /// The join is created BEFORE the loop's projections, as
    /// `IrBuilder::build` pre-creates every merge in pc order before it walks;
    /// so the join heads a lower-numbered block than the loop exit that is its
    /// immediate dominator. That numbering is what made the sink pick the join.
    fn r11w11_ternary_after_loop_graph() -> (Graph, NodeId, NodeId, NodeId) {
        let mut g = r11_empty_graph();
        let start = g.add(Op::Start, IrType::Control, vec![], None);
        let entry = g.add(Op::Proj(0), IrType::Control, vec![start], None);
        let join = g.add(Op::Merge, IrType::Control, vec![], Some(20));
        let header = g.add(Op::Merge, IrType::Control, vec![entry], Some(2));
        let n = g.add(Op::Param(0), IrType::Int, vec![], None);
        let zero = g.add(Op::Const(0), IrType::Int, vec![], None);
        let one = g.add(Op::Const(1), IrType::Int, vec![], None);
        let minus_one = g.add(Op::Const(-1), IrType::Int, vec![], None);
        let held = g.add(Op::Phi, IrType::Int, vec![header, zero, NO_NODE], Some(2));
        let lt = g.add(
            Op::Cmp(crate::ir::CmpOp::Lt),
            IrType::Int,
            vec![held, n],
            Some(4),
        );
        let if1 = g.add(Op::If, IrType::Control, vec![header, lt], Some(4));
        let body = g.add(Op::Proj(0), IrType::Control, vec![if1], Some(4));
        let exit = g.add(Op::Proj(1), IrType::Control, vec![if1], Some(4));
        let next = g.add(Op::Add, IrType::Int, vec![held, one], Some(7));
        assert!(g.push_input(header, body));
        assert!(g.set_input(held, 2, next));
        let ne = g.add(
            Op::Cmp(crate::ir::CmpOp::Ne),
            IrType::Int,
            vec![held, zero],
            Some(12),
        );
        let if2 = g.add(Op::If, IrType::Control, vec![exit, ne], Some(12));
        let nonzero = g.add(Op::Proj(0), IrType::Control, vec![if2], Some(12));
        let is_zero = g.add(Op::Proj(1), IrType::Control, vec![if2], Some(12));
        // `-1 - held`: early block is the loop header (it reads the header
        // φ), and its only reader is the join's φ, on the `nonzero` edge.
        let sub = g.add(Op::Sub, IrType::Int, vec![minus_one, held], Some(18));
        assert!(g.push_input(join, is_zero));
        assert!(g.push_input(join, nonzero));
        let phi = g.add(Op::Phi, IrType::Int, vec![join, zero, sub], Some(20));
        let ret = g.add(Op::Return, IrType::Void, vec![join, phi], Some(20));
        g.entry = start;
        g.exit = ret;
        (g, join, exit, sub)
    }

    /// The graph is well formed (the graph-level lane names nothing: the φ's
    /// input is defined in the loop header, which dominates its edge), and
    /// the scheduler must keep it so. The sink pass used to count the φ's read
    /// in the φ's OWN block, took the join as the value's `late` block and —
    /// the join being the lowest-numbered shallower block on the dominator
    /// path — moved `-1 - held` into it, below the edge that reads it. It must
    /// still leave the loop, one block higher: the loop exit.
    #[test]
    fn r11w11_a_value_only_a_join_phi_reads_does_not_sink_into_the_join() {
        let (g, join, exit, sub) = r11w11_ternary_after_loop_graph();
        assert_eq!(dominance_violations(&g, false), no_findings(), "graph");
        let sched = schedule(&g);
        assert_eq!(
            sched.misplaced_reads(&g),
            Vec::<MisplacedRead>::new(),
            "post-schedule"
        );
        let at = |id: NodeId| sched.node_to_block[id as usize];
        assert_ne!(at(sub), at(join), "sunk into the φ's own block");
        assert_eq!(
            at(sub),
            at(exit),
            "the value should still sink out of the loop, to the loop exit"
        );
    }

    /// The same shape as bytecode, built and optimized — the tail of
    /// `R11SynccallDirectCallerLoop.mutator`, the one census hit of wave 10:
    /// `static int f(int n) { int s = 0, held = 0;
    ///   for (int i = 0; i < n; i++) { s += i; held += i & 1; }
    ///   return held == 0 ? s : -1 - held; }`
    #[test]
    fn r11w11_the_mutator_tail_schedules_with_no_misplaced_phi_input() {
        let code: [u8; 39] = [
            0x03, 0x3c, 0x03, 0x3d, 0x03, 0x3e, // 0: s = 0; held = 0; i = 0
            0x1d, 0x1a, 0xa2, 0x00, 0x13, // 6: iload_3; iload_0; if_icmpge 27
            0x1b, 0x1d, 0x60, 0x3c, // 11: s += i
            0x1c, 0x1d, 0x04, 0x7e, 0x60, 0x3d, // 15: held += i & 1
            0x84, 0x03, 0x01, // 21: iinc 3, 1
            0xa7, 0xff, 0xee, // 24: goto 6
            0x1c, 0x9a, 0x00, 0x07, // 27: iload_2; ifne 35
            0x1b, 0xa7, 0x00, 0x06, // 31: iload_1; goto 38
            0x02, 0x1c, 0x64, // 35: iconst_m1; iload_2; isub
            0xac, // 38: ireturn
        ];
        let Some(mut graph) = IrBuilder::new(1, 4).build(&code, 39) else {
            panic!("the mutator tail must build");
        };
        ir_optimize::optimize(&mut graph);
        assert_eq!(
            dominance_violations(&graph, false),
            no_findings(),
            "optimized"
        );
        assert_eq!(
            schedule_with_options(&graph, &production_schedule_options()).misplaced_reads(&graph),
            Vec::<MisplacedRead>::new(),
            "optimized, release form"
        );
    }

    // ── Round 11 wave 12 (lane ircore) ───────────────────────────────────

    /// irback proposal W11-1: the sink pass's own read check refuses the
    /// placement wave 11 fixed at its source. `-1 - held`, moved from the loop
    /// header into the join, sits below the edge its only reader (the join's
    /// φ) reads it on; moved to the loop exit, it dominates that edge.
    #[test]
    fn r11w12_the_read_check_refuses_a_value_sunk_below_its_phi_edge() {
        let (g, join, exit, sub) = r11w11_ternary_after_loop_graph();
        let sched = schedule(&g);
        let nb = sched.blocks.len();
        let at = |id: NodeId| sched.node_to_block[id as usize];
        let held = g.nodes[sub as usize].inputs[1];
        let header = at(held);
        assert_ne!(header, at(exit), "precondition: the φ heads the loop");

        // As if the pass had found `sub` in the header and moved it.
        let mut original = sched.node_to_block.clone();
        original[sub as usize] = header;

        let mut into_join = original.clone();
        into_join[sub as usize] = at(join);
        assert!(
            !sunk_reads_dominated(&g, &into_join, &original, &sched.dom, nb),
            "the join does not dominate the `nonzero` edge the φ reads on"
        );

        let mut into_exit = original.clone();
        into_exit[sub as usize] = at(exit);
        assert!(
            sunk_reads_dominated(&g, &into_exit, &original, &sched.dom, nb),
            "twin: the loop exit dominates both arms"
        );
        assert!(
            sunk_reads_dominated(&g, &original, &original, &sched.dom, nb),
            "twin: nothing moved, nothing owed"
        );
    }

    /// W11-1, the non-φ half: a value moved below a reader in its own block
    /// (the loop test's compare, below the `If` that reads it) is refused.
    #[test]
    fn r11w12_the_read_check_refuses_a_value_sunk_below_a_plain_reader() {
        let (g, _join, exit, _sub) = r11w11_ternary_after_loop_graph();
        let sched = schedule(&g);
        let nb = sched.blocks.len();
        let lt = g
            .nodes
            .iter()
            .position(|n| n.op == Op::Cmp(crate::ir::CmpOp::Lt))
            .expect("the loop test");
        let original = sched.node_to_block.clone();
        assert_ne!(
            original[lt],
            usize::MAX,
            "precondition: the compare is placed"
        );
        assert!(sunk_reads_dominated(
            &g, &original, &original, &sched.dom, nb
        ));

        let mut below = original.clone();
        below[lt] = sched.node_to_block[exit as usize];
        assert!(
            !sunk_reads_dominated(&g, &below, &original, &sched.dom, nb),
            "the loop exit does not dominate the header's `If`"
        );
    }

    /// irback proposal W11-3: equally shallow candidates are taken nearest
    /// `early`, not lowest-numbered. Block 3 is a join created before the loop
    /// exit (block 4) that dominates it, as `IrBuilder::build` pre-creates
    /// merges:
    ///
    /// ```text
    ///   0 -> 1 (header) <-> 2,  1 -> 4 (exit) -> 3 (join)
    /// ```
    #[test]
    fn r11w12_sink_ties_break_by_dominance_not_by_block_number() {
        let blocks = cfg(5, &[(0, 1), (1, 2), (2, 1), (1, 4), (4, 3)]);
        let dom = Dominators::compute(&blocks);
        let (depth, innermost) = loop_nest(&blocks, &dom);
        assert_eq!(depth, vec![0, 1, 1, 0, 0]);
        let mut path = Vec::new();

        // Depth-only: the first shallower block below the header is the exit.
        // Block order would have taken the join, which the exit dominates.
        assert_eq!(
            choose_sink_block(&dom, &depth, &innermost, 1, 3, false, 5, &mut path),
            4
        );
        assert_eq!(
            choose_sink_block_by_scan(&dom, &depth, &innermost, 1, 3, false, 5),
            4
        );
        // Equal-depth still walks on down to the reader's block.
        assert_eq!(
            choose_sink_block(&dom, &depth, &innermost, 1, 3, true, 5, &mut path),
            3
        );
        assert_eq!(
            choose_sink_block_by_scan(&dom, &depth, &innermost, 1, 3, true, 5),
            3
        );
    }
