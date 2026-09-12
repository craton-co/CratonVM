# The IR scheduler and lowering still have quadratic scans

**Status:** OPEN (compile-time cost on large graphs). This is the residual of
the 2026-09-12 JIT review findings "Quadratic scheduler and lowering scans"
and "Scheduler places nodes in id order".

## Already fixed

- **Dominators.** They are computed by Cooper–Harvey–Kennedy over reverse
  postorder (`Dominators` in `jit/src/ir_schedule.rs`), and `dominates` is an
  O(1) interval test. Every query inside the scheduler uses it.
- **Block construction.** Terminators, `If` projections and fall-through merges
  are collected in one pass over the graph, not one scan per block.
- **Node placement.** Nodes are placed in input post-order, so an input always
  gets a block before its user.
- **Constant interning in the IR builder.** `IrBuilder::interned_const` uses a
  map from (value, type) to node, validated on every hit.
- **Division guards.** The lowering's per-division zero-guard and deopt-block
  lookups read indexes built once in `Lowerer::new`.

## Left

| Scan | Where | Cost |
|---|---|---|
| `Schedule::dom` is still a dense `Vec<Vec<bool>>`, built once at the end of scheduling | `ir_schedule.rs`; indexed by `ir_check_elim.rs` | O(B²) memory |
| The sink pass loops over every block for each node | `place_sunk_nodes`, `deepest_common_dominator` in `ir_schedule.rs` | O(B·N) |
| `get_or_add_const` / `find_const` scan the node list on every call | `jit/src/ir_optimize.rs` | O(N) per constant |
| `resolve_frame_state_for_bci` scans the safepoint list once per deopt site, and there is a per-loop-header `safepoints.iter().find` | `jit/src/ir_lower.rs` | O(S) per site |

## The fix

- **Dense dominator matrix.** Change `ir_check_elim.rs` to take `&Dominators`,
  then drop the matrix.
- **Sink pass.** Walk the dominator tree from the deepest common dominator,
  instead of testing every block. Keep today's handling of unreachable blocks,
  where every block dominates them, and pin it with a test first.
- **`ir_optimize` constants.** Share the builder's interning map, or give
  `ir_optimize` its own map validated on hit.
- **Safepoint lookups.** Index safepoints by bci once per lowering.
