# Default JIT diverges from the interpreter semantics probe

Status: open
Found: 2026-07-27 while verifying interpreter convergence
Observed on both pre-fix r2 and fixed r3 release binaries

## Evidence

`ArchitectureInterpreterSemantics20260727` with 20,000 iterations produces:

```text
default JIT: 16805363457397911308
--nojit:      3471186786924291744
```

The mismatch is unchanged between r2 and r3, so it predates and is independent
of the package-routing removal.

Additional deterministic coverage from the moving-root remediation:
`NativeRootRegistryGc` succeeds for all 240 rounds with `--nojit`, including
TreeSet cardinality checks, but the default JIT path reports
`size mismatch at round 29: list=30, linked=30, tree=30, set=29`. This gives the
JIT slice a smaller collection operation to minimize in addition to the
checksum probe.

## Required fix

Bisect the kernel by opcode/operation, identify the compiled method and
backend, add a minimal JIT-vs-interpreter regression, and correct the lowering
or admission gate. Move this document under `docs/internal` only after the
default-JIT, `--nojit`, and HotSpot results agree.
