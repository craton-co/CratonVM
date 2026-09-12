# FIXED: the IR scheduler, optimizer and lowering still had quadratic scans

**Status: FIXED 2026-09-12.** This was the residual of the 2026-09-12 JIT review
finding #90, "Quadratic scheduler and lowering scans". Dominator computation,
block construction, node placement, the builder's constant interning and the
division-guard indexes had already been fixed under that finding. The four
scans below were left. The ledger is `jit-review-20260912-findings-FIXED.md`.

## The defect

| Scan | Where | Cost |
|---|---|---|
| `Schedule::dom` was a dense `Vec<Vec<bool>>`, built at the end of every schedule only because `ir_check_elim` indexed it | `jit/src/ir_schedule.rs`, `jit/src/ir_check_elim.rs` | O(B²) memory per method |
| The sink pass tested every block for every sinkable node, twice: once for the deepest common dominator of the uses, once for the placement between early and late | `place_sunk_nodes`, `deepest_common_dominator` in `ir_schedule.rs` | O(B·N) per round, up to 8 rounds |
| `get_or_add_const` scanned the node arena on every call | `jit/src/ir_optimize.rs` | O(N) per constant |
| `resolve_frame_state_for_bci` scanned `graph.safepoints` once per deopt site, and the per-block OSR entry planner scanned it once per block | `jit/src/ir_lower.rs` | O(S) per site or block |

None of them changed an answer. Each was compile time on a large graph.

## The fix

### `Schedule::dom` is the `Dominators`

`Schedule::dom` is now a `Dominators`. It is recomputed after a layout
permutation, as the matrix was. Every reader asks `Dominators::dominates`,
which gives the matrix's answers, out-of-range and unreachable blocks included:

- `ir_check_elim.rs`: `dominates`, `dominates_reflexive`
- `ir_schedule.rs`: `Schedule::node_strictly_dominates_block`, and the placement
  test `an_old_node_rewired_to_a_newer_input_is_placed_after_that_input`

`compute_dominators`, the matrix `dominates` helper and `Dominators::to_matrix`
are now `#[cfg(test)]`. The dense-fixpoint comparison tests still read the
matrix form.

### The sink pass walks the dominator tree

The unreachable-block answers were pinned first, in a separate commit, by
`sink_common_dominator_treats_an_unreachable_use_as_dominated_by_every_block`.
In this model every block dominates an unreachable block.

- **`deepest_common_dominator`** climbs `idom` chains over the reachable uses.
  An unreachable use constrains nothing. When every use is unreachable, the
  answer is the scan's: the highest-numbered unreachable block, now
  `Dominators::last_unreached`. A use past the end still gives `None`.
- **`choose_sink_block`**, split out of `place_sunk_nodes`, visits only the
  `idom` path from `late` up to `early`. It sorts that path by block number, so
  the scan's tie-breaking order is kept. When `late` is unreachable, every block
  dominates it and the candidates are not a path, so that case goes to the scan.

Both scans are kept verbatim as `deepest_common_dominator_by_scan` and
`choose_sink_block_by_scan`. They answer the cases the walk hands back and are
the references the tests hold the walk to.

A walk is O(dominator-tree depth) per node. On a deep, chain-shaped CFG that is
still proportional to the block count.

### `ir_optimize` interns constants

`ConstIntern` maps `(value, type)` to the lowest-id `Op::Const`, mirroring
`IrBuilder::interned_const`:

1. New nodes are indexed incrementally.
2. Every hit is re-checked.
3. A hit whose node was killed or rewritten falls back to `const_node_by_scan`,
   which repairs the entry.

`reassociate_affine`, through `build_affine`, and `algebraic_simplify` each make
one per invocation. A map is never shared across passes, because constant
folding rewrites ops in place between them, and the map cannot see an older node
turned into a constant. The unused `find_const` is removed.

### `ir_lower` indexes safepoints by bci

`Lowerer::safepoint_first_by_bci` is built once in `Lowerer::new` by
`Lowerer::first_safepoint_index_by_bci`. It keeps the FIRST snapshot index at
each bci, which is what `position` and `find` returned. The lowerer holds the
graph by shared reference, so the index cannot go stale; the division-guard
indexes rest on the same argument. Two readers use it:

- `resolve_frame_state_for_bci`
- the per-block OSR entry planner

## Regression coverage

- `ir_schedule.rs`:
  - `sink_common_dominator_treats_an_unreachable_use_as_dominated_by_every_block`
    pins the unreachable-block answers.
  - `sink_placement_walks_match_the_block_scans_on_generated_cfgs` covers 300
    generated CFGs with unreachable blocks, self loops and irreducible cycles.
    Both walks must equal their scans, placement under both real loop depths and
    random depths, with both settings of the equal-depth rule.
  - The existing `chk_dominators_match_the_dense_fixpoint_*` tests still compare
    `Dominators` with the dense fixpoint.
- `ir_optimize.rs`: `const_intern_returns_the_node_the_arena_scan_returns` runs
  2000 random steps of adds behind the map's back, kills and lookups. Each lookup
  must equal the arena scan.
- `ir_lower.rs`: `the_safepoint_index_names_the_first_snapshot_at_each_bci`
  covers duplicate bcis and bcis with no snapshot.

No flag was added. No `static` was added.
