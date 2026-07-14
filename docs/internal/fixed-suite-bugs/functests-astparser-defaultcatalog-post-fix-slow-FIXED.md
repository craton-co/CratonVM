# `FunctionTests` / `ASTParserLoadingTest` / `DefaultCatalogAndSchemaTest` - fixed

Status: FIXED - 2026-07-13

The original host-load-limited report was retired after complete isolated
remote-host validation with the final CratonVM binary built in `/data`.

## Root causes fixed

- Bound `Class` method references bypassed the VM's Class-mirror overrides.
  Hibernate Models therefore saw an empty annotation set for mapped classes.
  Lambda cache dispatch now returns Class overrides to the normal dispatcher.
- Synthetic `BufferedInputStream` overrides were incorrectly forced over the
  real JDK implementation, truncating Jandex class-resource parsing.
- Resource-backed stream helpers now pin moving-GC references across allocation.
- JIT root registration remaps the innermost compiled boundary frame from its
  precise RBP, and nested compiled calls register their live frame.
- Inline TLAB object allocation is now opt-in while its moving-GC contract is
  incomplete; the default JIT allocation path uses the canonical helper
  initializer.

## Validation

Remote host `20.83.144.174`, unique binary
`cvfunctests-astparser-defaultcatalog-complete-20260713-003`, `--nojit`,
`--Xmx 512m`:

- `ASTParserLoadingTest`: `found=106 started=106 ok=106 failed=0`
- `FunctionTests`: `found=123 started=117 ok=117 failed=0 skipped=6`
- `DefaultCatalogAndSchemaTest`: `found=33 started=33 ok=33 failed=0`

The DefaultCatalog run completed in 1,172,203 ms with no OOM or GC-header
corruption diagnostics.
