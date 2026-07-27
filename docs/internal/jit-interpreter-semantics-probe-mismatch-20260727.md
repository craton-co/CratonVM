# JIT/interpreter semantics probe mismatch — resolved 2026-07-27

## Reported failure

Separate runs of `ArchitectureInterpreterSemantics20260727` appeared to produce
different checksums under default JIT and `--nojit`. The values also changed
between later executions.

`NativeRootRegistryGc` separately lost `TreeSet` contents with the JIT enabled,
which initially seemed to corroborate a compiler problem.

## Root causes

The checksum differential was invalid. The probe placed an `Add` instance in
an `Object[]` and mixed `ref.hashCode()` into its result, but `Add` inherited
identity-based `Object.hashCode()`. Separate VM processes are allowed to assign
different identity hashes, so JIT, interpreter, and HotSpot outputs were not
comparable.

`NativeRootRegistryGc` was an independent collector phase-ordering bug. It
reproduced with the JIT compilation threshold above every method invocation,
so no compiled code had run. Its fix is documented in
`selective-promotion-external-root-phase-order-fixed-20260727.md`.

## Resolution

`Add.hashCode()` is now deterministic. The equivalence script runs four paths
from the same source:

- verified interpreter (`--nojit`);
- forced decoded interpreter fallback (`--nojit --noverify`);
- default tiered JIT; and
- HotSpot from the supplied Java home.

All four produce:

```text
14088725157972731584
```

The script fails if any pair differs. No JIT lowering defect remained after the
probe was corrected.
