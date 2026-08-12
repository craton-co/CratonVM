# The young walk treats a run of EMPTY objects as corruption at other sites

**Status:** PARTLY RESOLVED (2026-08-12). Of the seven sibling sites this page
was filed for, the two that a census showed actually firing are fixed; **the
other five were measured at exactly zero on the repro** and are left alone
deliberately. One residual remains: the parallel sweep is accepted on ~74% of
attempts, not all of them.

## Background

An empty object — `ClassId(0)`, `kind = Object` (tag 0), `num_slots = 0`,
identity hash not yet minted — is HEADER_SIZE all-zero bytes, and
`gen_object_total_size` sizes it at exactly HEADER_SIZE. It has been an ordinary
allocation ever since HEADER_SIZE shrank 24 → 16 made the mark word the second
header word and the JIT's `new Object()` fast path left it zero. A **dead** one
is unmarked, so a walk's live set cannot vouch for it.

Eight `zero_run_end` callers in `gc/src/gen_heap.rs` shared the rule "an
all-zero run of at least HEADER_SIZE at a walk-grid offset is evidence the grid
broke". For the shape above that rule is a false positive. The sequential young
sweep's copy of it was the
`young-sweep-empty-object-run-unwind-20260812-FIXED` hang; this page was filed
for the other seven, on the reasoning that a shared predicate is likely wrong
the same way everywhere.

## What the census found

Per-site counters on `org.hibernate.reactive.BatchingConnectionTest` under
`-XX:+UseGenerationalGC`, cumulative over the first four young cycles:

| site | anomaly hits | verdict |
|---|---|---|
| `sweep_chunk` (parallel prefix walker) | 5 — and `par_attempts=5 par_fails=5` | **fired every time; fixed** |
| `clear_all_mark_bits_in_arena` | 2 498 | **fired constantly; fixed** |
| `sweep_young_non_moving`, selective-promotion evacuation pre-pass | **0** | never fires here |
| `sweep_young_non_moving`, second pre-pass walk | **0** | never fires here |
| `mark_young_to_old_refs` | **0** | never fires here |
| `fixup_young_old_refs` | **0** | never fires here |
| `walk_young_objects` | **0** | never fires here |

The first filing of this page asserted that the evacuation pre-pass was
suppressing selective promotion. **That was wrong** — it is a plausible reading
of identical code that the measurement does not support. The five zero rows are
latent, not active: the shape is still mis-read there, and another workload
could reach them, but changing code that never executes on the only repro
available buys an unmeasurable diff and costs real risk.

## What was fixed

`zero_run_is_empty_object_run` (already unit-tested, seven tests,
mutation-checked) applied at the two live sites, plus the `vouched_live` escape
each of them was also missing:

* **`sweep_chunk`** — bailed the whole parallel attempt on the first benign run,
  and `parallel_sweep_walk` discards EVERY chunk on one `None`, so the entire
  arena was re-swept sequentially. The run is also clamped to the chunk's upper
  bound so the chain can still land exactly on the next anchor.
* **`clear_all_mark_bits_in_arena`** — re-anchored at the next free block and
  **left the mark bits in the skipped stretch set**. The next non-moving sweep
  treats a set `GC_FLAG_MARKED` as live regardless of reachability (the bug-C5
  note at the call site), so this false positive fed itself. It now takes the
  live set from its single caller, which had it in hand all along.

Skipping cannot drop a mark: a header carrying `GC_FLAG_MARKED` is not
all-zero, and the predicate refuses any run with a marked base inside it.

## Measurement

`PAR_SWEEP_ATTEMPTS` / `PAR_SWEEP_ACCEPTS` have existed since H2-CID0, and their
doc comment already said "a large abort count means chunks routinely disagree
with the grid" — but **nothing had ever read them**. They now print from
`print_gc_summary` under `--verbose:gc` / `CRATONVM_GC_STATS`:

```
[GC] young_sweep: par_attempts=3 par_accepts=3 zero_spans=0 zero_empty_runs=1528
     phantom_extents=0 live_in_dead=0 walk_overshoot=0 anchor_not_a_base=0
```

ABBA-interleaved, 8 runs per arm, same class, same host:

| | before | after |
|---|---|---|
| parallel sweep accepted | **0 of 26 attempts** | **17 of 23** |
| `phantom_extents` / `live_in_dead` | 0 / 0 | 0 / 0 |
| wall clock (median of 8) | 10.94 s | 10.83 s |

The wall-clock columns are **indistinguishable** — this restores a code path
that was 100% dead on every JIT-warm workload, it does not make this class
faster in a way this host can resolve. Both corruption guards stayed silent with
the parallel path live, which is the result that mattered.

## Residual — the last 26% of attempts

Six of 23 parallel attempts still abort. `zero_spans` (the genuinely-long
unlisted zero runs, which correctly still bail) is non-zero on exactly the runs
with the lowest accept ratio, so that is the first thing to check; `sweep_chunk`
has five other bail reasons that were never separated by counter. Worth doing
only with a per-reason census, since one `None` still discards every chunk —
the all-or-nothing design is what makes a rare bail expensive.

Not covered by a unit test: the two call sites are integration-level, and the
evidence is the `par_accepts` counter above. The predicate they call is unit
tested; the chunk-bound clamp is not.
