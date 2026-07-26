# Moving-young binaryTrees OSR coverage guard

Status: **CLOSED**. The moving-young OSR under-count is fixed on current `dev` by the
moving-young incomplete-JIT-coverage fallback:

- `../../../vm/src/memory/roots.rs` detects `CRATONVM_MOVING_YOUNG` plus an active OSR artifact whose
  rewritable shadow coverage is not proven.
- `../../../vm/src/jit/conservative_roots.rs` checks the OSR artifact's shadow layout, debug shadow-disable
  gates, precise-map coverage, and exact-RBP availability.
- When coverage is incomplete, root gathering re-enables the conservative JIT-frame scan and sets
  `gc_quiescence::force_non_moving_jit_roots()`.
- `../../../gc/src/gen_heap.rs` then treats moving young as unavailable for that cycle and runs the
  non-moving sweep, so conservatively discovered OSR frame words are not relocated behind raw JIT
  slots.

## Original Symptom

With both moving young and OSR enabled, `MovingYoungBtOsr 16` could print the wrong checksum:

```text
14854832
```

Expected:

```text
14985902
```

The missed object was the `longLived` tree in local slot 6 of an OSR-entered `binaryTrees(I)J`
frame. A moving young GC relocated the tree, but the OSR frame did not publish a rewritable shadow
home for that local. The final `check(longLived)` then saw the stale from-space copy as a leaf,
producing an exact `131070` under-count.

## Why the Fix Is General

The guard is not keyed to binaryTrees or to this repro. It applies to any active OSR-compiled method
under `CRATONVM_MOVING_YOUNG` where the runtime cannot prove the frame is safely rewritable for a
moving collection. Complete, proven frames can still use moving young; incomplete OSR frames force
only that GC cycle back to the existing non-moving conservative fallback.

## Repro

```powershell
javac -d scratch\moving-young-bt docs\internal\repros\moving-young-bt-osr\MovingYoungBtOsr.java
$env:CRATONVM_MOVING_YOUNG='1'
$env:CRATONVM_JIT_OSR='1'
target\release\cratonvm_moving_young_osr_guard_20260701_7.exe --stack-dump-on-timeout=0 -Xmx4g -cp scratch\moving-young-bt MovingYoungBtOsr 16
```

Expected output:

```text
14985902
```

## Regression Coverage

`../../../vm/src/jit/conservative_roots.rs` now has focused unit coverage for the OSR fallback predicate:

- non-OSR methods never trigger it;
- OSR methods without a shadow layout trigger it;
- OSR methods with a shadow layout and no precise maps are accepted;
- OSR methods with precise maps require `fully_oop_covered` plus an exact RBP;
- debug shadow-disable gates force fallback.

## Related

- `../../feature-designs/default-moving-young-gen.md`
- `../../feature-designs/precise-jit-maps-default.md`
- `docs/internal/app-jvm-bugs/jit-osr-main-corruptor-investigation.md`
