# Proposal: the JIT refill gate should probe what `refill_tlab` can serve, not the full request

> **STATUS (2026-09-28, gc defects round, proposal triage d8/y): KEEP (rank 53
> of 54).** Not built as a default
> (`alloc_policy_defaults::TLAB_GATE_BUMP_FLOOR = false`). Meaningful only
> with the share sizer on, whose flip is deferred to a round that measures G1
> and ZGC; make it an arm of that flip. **Gate:** `GenR5W3ConcUnloadProbe`
> with `CRATONVM_TLAB_SHARE_SIZER=1` +/- `CRATONVM_TLAB_GATE_BUMP_FLOOR=1`:
> `[GC] gen-alloc:` old-direct counts lower, correctness lines unchanged (d7
> s10_share_unload baseline: `control-ok=true` 10/10). **Size:** XS.

*Filed 2026-09-27 by gen round 5, wave 6, lane `sizer10`.*

## Direction

`tlab_alloc_shaped_inner`'s gate for the JIT helpers
(`refill_needs_young_room`) asks `young_bump_headroom(requested)`. With the
share sizer, `requested` is 1 MiB from the first young pause on. So the
last 1 MiB of every young from-space, and every free-list stretch below
the fragmentation floor, is served object by object:
- `try_alloc_object_full` / `try_alloc_array` at the arena cursor;
- then, when young is full, `try_alloc_object_old` / `try_alloc_array_humongous`,
  which tenure directly;
- all while the thread keeps its exhausted buffer live.

`refill_tlab` would have served any tail down to `frag_tlab_floor()`.
`CRATONVM_TLAB_GATE_BUMP_FLOOR` (gen r4w3/alloc3, opt-in) already makes the
gate probe `max(floor, total_size)`. Make that the default whenever the share
sizer is on, so that bigger buffers do not widen the per-object band. That
band is suspect for mechanism (A) of the share-sizer page, and it costs
throughput and premature tenuring regardless.

## Measure

- `GenR5W3ConcUnloadProbe`, with `CRATONVM_TLAB_SHARE_SIZER=1` and `+/-`
  `CRATONVM_TLAB_GATE_BUMP_FLOOR=1`: pass rate, and `[GC] gen-alloc:`
  old-direct counts.
- Protocol A of `docs/internal/reviews/gengc-round5-w2-alloc6-20260926.md`.

## Risk

The gate and the server must agree on what is satisfiable (the
`tlab-remnant-wedge` rule). The floor arm already keeps them in agreement.
