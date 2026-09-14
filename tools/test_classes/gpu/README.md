# test_classes/gpu — GPU-offload Java fixtures

Every fixture under this directory is a **real Java source file**
compiled by `javac`. Nothing here is allowed to be a hand-rolled byte
array. The plan for GPU offload (see
`C:\Users\Admin\.claude\plans\https-nvlabs-github-io-cuda-oxide-examin-declarative-metcalfe.md`)
makes this a hard rule, restated in each Part:

> No synthesised bytecode arrays, no fabricated `Class` structs. Real
> `.class` files are the unit of test.

If you need a new shape of method to test, add a Java source here and
re-run `cargo build` — the workspace `build.rs` chain picks the new
file up automatically.

## Build wiring

The compilation happens in `jit-cuda/build.rs`. It invokes `javac` if
present on the PATH and emits `.class` files next to the sources. The
build prints a `cargo:warning=...` if `javac` is missing — the
analyzer tests then skip with a clear `panic!` when they can't find
the fixture, so missing infrastructure surfaces loudly.

## Fixture catalogue

| File                      | What it exercises                                         |
| ------------------------- | --------------------------------------------------------- |
| `EligibleVectorAdd.java`  | Element-wise `int[]` + `int[]` → `int[]` (the hello world) |
| `EligibleSaxpy.java`      | `float` scalar × `float[]` + `float[]` → `float[]`        |
| `EligibleDotProduct.java` | Reduces two `int[]` to a `long` (scalar return)           |
| `RejectAllocation.java`   | `new int[]` mid-method → analyzer must reject             |
| `RejectInvoke.java`       | Helper static call → analyzer must reject                 |
| `RejectSynchronized.java` | `synchronized` static method → analyzer must reject       |
| `RejectRefArray.java`     | `Integer[]` parameter → analyzer must reject              |
| `GcStress.java`           | Used by Part F's GC-during-offload stress test            |
| `BoundsTrip.java`         | Used by Part E's deopt path test                          |
| `Benchmark.java`          | Part J's end-to-end CPU-vs-GPU comparison driver          |
