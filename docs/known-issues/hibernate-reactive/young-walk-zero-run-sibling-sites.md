# The young walk treats a run of EMPTY objects as corruption at seven more sites

**Status:** OPEN (filed 2026-08-12). Throughput and over-retention, **not** a
hang and not a correctness defect — every one of these paths errs towards
retaining, never towards freeing something live.

## Background

An empty object — `ClassId(0)`, `kind = Object` (tag 0), `num_slots = 0`,
identity hash not yet minted — is HEADER_SIZE all-zero bytes, and
`gen_object_total_size` sizes it at exactly HEADER_SIZE. It has been an
ordinary, common allocation ever since HEADER_SIZE shrank 24 → 16 made the mark
word the second header word and the JIT's `new Object()` fast path left it zero.
A **dead** one is unmarked, so a walk's live set cannot vouch for it.

Eight `zero_run_end` callers in `gc/src/gen_heap.rs` share the rule "an all-zero
run of at least HEADER_SIZE at a walk-grid offset is evidence the grid broke".
For the shape above that rule is a false positive, and on a JIT-warm workload it
fires constantly: measured at **147 times per young cycle** on
`org.hibernate.reactive.BatchingConnectionTest` under `-XX:+UseGenerationalGC`,
every span exactly 16 bytes.

One of the eight is now fixed — the sequential young sweep walk in
`sweep_young_non_moving`; see the retired
`young-sweep-empty-object-run-unwind-20260812-FIXED` write-up. The predicate it
introduced, `zero_run_is_empty_object_run`, is the reusable half of that fix.
(The pre-existing `vouched_live` escape at the same site is not a substitute: it
only recognises a MARKED object at the run start, and a dead empty object is by
definition unmarked.)

## The sites still using the old rule

Named by enclosing function, since line numbers move:

| function | what the false positive costs |
|---|---|
| `sweep_chunk` | returns `None` on the first benign run, which **aborts the parallel young-sweep prefix entirely**. `par_prefix_end=0` on every cycle measured on the repro above — the whole-arena sweep runs sequentially. |
| `clear_all_mark_bits_in_arena` | re-anchors at the next free block, leaving **stale mark bits** in the skipped stretch. The next non-moving sweep treats a set mark bit as live regardless of reachability (the bug-C5 note in that function), so this is a self-feeding over-retention. |
| `sweep_young_non_moving`, selective-promotion evacuation pre-pass | unwinds the evacuation candidates since the last anchor and resyncs to the next free block, so **selective promotion is suppressed**. Under a permanent non-moving sweep, promotion is the young generation's only exit for live data. |
| `sweep_young_non_moving`, the second pre-pass walk | same `anomaly = true` shape. |
| `mark_young_to_old_refs` | walk stops / resyncs early. |
| `fixup_young_old_refs` | walk stops / resyncs early. |
| `walk_young_objects` | diagnostic/heap-info walk under-reports. |

## Why they were left alone

The fix that landed was scoped to the one site that was measured to cause a
hang, and each of the remaining sites needs its own measurement: `sweep_chunk`
changes the parallel/sequential split, the promotion pre-pass changes what
leaves young, and `clear_all_mark_bits_in_arena` changes what the *next* cycle
believes is live. Sharing a predicate is a reason to expect them to be wrong the
same way, not evidence that changing them is safe.

## How to work it

`zero_run_is_empty_object_run(base, cursor, run_end, used, side_sorted)` already
exists and is unit-tested (seven tests in `gen_heap::tests`, mutation-checked).
Each site needs the live set it should pass as `side_sorted` — `sweep_chunk` and
`clear_all_mark_bits_in_arena` do not have one to hand, which is the real work.

Repro and instrument, on Azure host `azureuser@20.80.105.49`:

```bash
cd /data/cratonvm/apps/hibernate-reactive-suite-runner
export DOCKER_HOST=unix:///var/run/docker.sock TESTCONTAINERS_RYUK_DISABLED=true
<cratonvm> --java-home /data/toolchain/jdk-25 --Xmx 1500m -XX:+UseGenerationalGC \
    @common.args -Dcraton.batch=1 CratonRunner \
    org.hibernate.reactive.BatchingConnectionTest
```

`CRATONVM_DBG_YOUNG_TRIGGER=1` shows the occupancy regime,
`CRATONVM_DBG_SWEEP_CENSUS=1` shows what each cycle actually reclaimed (expensive
— it brute-scans for holders), and a temporary per-cycle `eprintln!` beside the
`tracing::debug!("non-moving young sweep: …")` at the end of the sweep is what
produced the numbers above (`tracing` is compiled at `max_level_info`, so
`RUST_LOG=…=debug` cannot reach that line).
