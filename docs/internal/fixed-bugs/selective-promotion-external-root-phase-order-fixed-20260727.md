# Selective-promotion external-root phase ordering — fixed 2026-07-27

## Failure

`NativeRootRegistryGc` lost every element in a live `TreeSet` after the eighth
explicit full GC when the JIT was enabled, even with the compilation threshold
raised high enough that zero methods compiled. `--nojit` and
`CRATONVM_NO_SELECTIVE_PROMOTE=1` both masked the failure.

The `TreeSet` side-table retained its size, but its promoted backing
`Object[]` had been reclaimed and zeroed.

## Root cause

The generational non-moving young collector performs selective promotion before
an explicit `System.gc()` request triggers an in-place old-space sweep in the
same `collect_garbage` call.

Collection overlays are registered external-root providers. Their backing-array
references and reverse owner index were remapped only by the VM after the whole
collector call returned. When selective promotion moved a backing array from
young to old, the immediately following old marker queried a provider that
still returned the forwarded young source address. It therefore failed to seed
the new old-space copy and reclaimed that copy in the same cycle.

This was a phase-ordering defect, not a missing provider callback, missing write
barrier, or JIT code-generation error.

## Fix

`GenerationalHeap::run_non_moving_young_cycle` now publishes the selective
promotion pointer map to every registered external-root provider immediately
after the young sweep and before deciding whether to run the same-cycle old
sweep.

The normal VM post-GC remap remains in place. Provider remapping is idempotent,
while the later VM pass still owns all root sources outside the collector
provider registry.

## Regression coverage

`vm/tests/resources/cratonvm/NativeRootRegistryGc.java` now rejects a duplicate
`TreeSet.add` at the first point of corruption rather than allowing the set's
side-table size to conceal lost backing contents.

Qualification runs cover:

- JIT enabled with the threshold set above the probe's invocation counts;
- default JIT settings;
- `--nojit`;
- repeated young plus explicit full collections; and
- collection-overlay scan, relocation, owner lookup, and pruning harnesses.
