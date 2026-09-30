# Proposal: print the allocation and heap policy actually in force, once, on a `[GC] policy:` line

> **STATUS (2026-09-28, gc defects round, proposal triage d8/y): KEEP (rank 19
> of 54).** Not built (no `[GC] policy:`). Its example line is stale:
> `tlab_share_sizer` is OFF on every backend
> (`alloc_policy_defaults::TLAB_SHARE_SIZER = { generational: false, other:
> false }`); since this round `CRATONVM_GC_OLD_HUMONGOUS_TOP`,
> `CRATONVM_GC_FULL_GC_TRUE_ROOTS`,
> `CRATONVM_GC_MOVING_MAJOR_JIT_GUARD` and `CRATONVM_GC_FUTILE_YOUNG_BACKOFF`
> are Generational defaults (and `CRATONVM_GC_OVERHEAD_PROGRESS` since
> 2026-09-24) that the line should name. Every flip decision of this round was argued from command lines, which
> is the case for it. **Gate:** the page's three runs and a unit test that the
> renderer names every constant in `alloc_policy_defaults`. **Size:** S.

*Filed 2026-09-26 by gen round 5, wave 4, lane `defaults8`
(`docs/internal/reviews/gengc-round5-w4-defaults8-20260926.md`). A direction,
not a finding.*

- **Status:** OPEN (proposal, S).
- **Owner:** the observability lane (`VmHeap::print_gc_summary` in
  `gc/src/vm_heap.rs`, `gc/src/gc_metrics.rs`), with the switch owners
  supplying the values.

## Why

Every A/B in the generational rounds decides a default by comparing arms that
differ only in the environment. The lesson this file keeps recording (see
`types/src/flags.rs`, `parse::non_empty_non_zero`, and the retracted
2026-09-20 G1 measurements) is that "a measurement should read back what the
switch actually did, not what the command line asked for".

Since gen r5w4 the answer depends on the collector as well as the
environment. `CRATONVM_TLAB_SHARE_SIZER` and `CRATONVM_SOFTREF_HOTSPOT_LRU` are
ON by default on the Generational heap and OFF on G1 and ZGC.
`CRATONVM_GEN_TLAB_TAIL_SINK`, `CRATONVM_GEN_ZERO_ONCE`,
`CRATONVM_GEN_HUMONGOUS_ZERO_UNLOCKED`, `CRATONVM_GEN_XMS_USABLE_FIRST` and
`CRATONVM_GEN_CONC_ALLOC_FAIL_DOOR` are ON with nothing set. The rest of the
allocation arms are OFF. Only one of them, the share sizer, reads its
effective value back, as `sizer_on=` on `[GC] tlab-sizer:`. For the others an
experimenter must know the default table by heart. A run whose log does not
say which policies ran cannot be compared with another run's log after the
fact.

## Proposal

One line at the start of the `--verbose:gc` shutdown summary, rendered from
the flag snapshot and the heap, never from process state:

```
[GC] policy: backend=generational tlab_share_sizer=on(default) softref_hotspot_lru=on(default) gen_tlab_tail_sink=on(default) gen_xms_usable_first=on(default) gen_zero_once=on(default) tlab_size_retired=off(default) tlab_waste_shrink=off(default) tlab_filler_skip_zero=off(default) tlab_gate_bump_floor=off(default) gen_humongous_ref_zero_unlocked=off(default)
```

- `(default)` / `(set)` says whether the value came from the environment.
  For the two per-backend switches that is exactly `GcFlags::*`'s
  `Option<bool>`: `None` means the default was used.
- Build the renderer as a pure function in `types/src/flags.rs`
  (`GcFlags::policy_line(backend_name, generational) -> String`) next to the
  constants. A new arm added to `alloc_policy_defaults` then fails a unit
  test until it appears on the line. The test iterates the same table as
  `every_allocation_policy_arm_parses_against_its_one_default`.
- `VmHeap::print_gc_summary` prints it with `self.is_generational()`. The
  soft-ref value should come from the VM's `ReferenceProcessor` (its latched
  `hotspot_soft_lru`, which needs a getter), not be recomputed, so the line
  cannot disagree with what ran.

## How to verify

Three runs of any probe, with no environment set:

- `-XX:+UseGenerationalGC --verbose:gc` must print `tlab_share_sizer=on(default)`
  and `softref_hotspot_lru=on(default)`.
- `-XX:+UseG1GC --verbose:gc` must print both as `off(default)`.
- `CRATONVM_TLAB_SHARE_SIZER=0 -XX:+UseGenerationalGC --verbose:gc` must print
  `tlab_share_sizer=off(set)`.

Unit test: the renderer names every constant in `alloc_policy_defaults`.
