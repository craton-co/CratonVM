# CratonVM Benchmarks

Methodology, full notes, and provenance for the numbers in
[README.md](README.md). The headline rule for every table we publish:
**checksums must match HotSpot on every run** — a fast wrong answer is a
bug, not a result.

## CPU benchmarks

### Harness

> **⚠ The harness this section described is not in the repository.**
> Until 2026-07-25 this section stated that all seven CPU rows come from one
> unified harness, `bench/CratonBench.java`, runnable per-phase as
> `CratonBench <phase>`. **That file does not exist** — it is not tracked by
> git and is not present untracked on either the Windows checkout or the Azure
> Linux build host. Likewise the "mandatory" perf gate described below,
> `regression-suite/perf/run-cratonbench-gate.sh`, does not exist:
> `regression-suite/` has no `perf/` directory at all.
>
> So the table below **cannot currently be reproduced as documented**, and the
> gate it claims to be guarded by cannot currently be run. Either the harness
> and gate were never committed, or they were lost; the git history does not
> show a deletion. Treat the numbers as historical measurements whose exact
> harness is unavailable, not as something you can re-derive today.

**What actually exists in `bench/`** and can be run right now — these are the
per-kernel harnesses, each printing its own time **and checksum**:

| File | Row it covers | Checksum |
|------|---------------|----------|
| `BinTreesClassic.java` | Binary Trees (takes depth as argv, e.g. `18`) | `68332206` at d=18 |
| `HashMapOnly.java` | HashMap put/get | `15499991500000` at n=1,000,000 |
| `StringRegexOnly.java` | String/Regex | (printed) |
| `QuickBenchLong2.java` | arithmetic / fib / sieve / matrix kernels | (printed) |
| `BinT.java`, `IntrinsicBench.java`, `HmSemanticsProbe.java` | targeted probes | (printed) |

Rebuilding a unified `CratonBench` harness plus the gate script is tracked
work. Until then, use the per-kernel files above and compare **before/after on
one host with one binary pair**, which is the methodologically sound comparison
regardless of harness.

### Methodology

- Both VMs run the **same flags** (notably `-Xmx8g` for Binary Trees).
- Fresh process per measurement, alternating HotSpot / CratonVM runs.
- Pinned to one logical CPU with `taskset` on the benchmark host
  (Azure Linux, EPYC 9V45, SMT).
- Medians over 5–7 runs; **no samples discarded**.
- Reference JDK: Temurin JDK 25.0.3 (C2).
- Measurements are taken in quiet windows (1-minute load average below 2);
  the shared host's load otherwise inflates both columns — and CratonVM's
  memory-heavy rows more — so ratios, not absolute times, are the durable
  content across host re-provisionings.

### Current table (measured 2026-07-18, Binary Trees re-validated 2026-07-24)

| Benchmark                         | JDK 25 C2 | CratonVM  | Ratio |
|-----------------------------------|-----------|-----------|-------|
| Arithmetic (2B ops)               | 2,006 ms  | 4,895 ms  | 2.44x |
| Fibonacci(44)                     | 1,719 ms  | 4,790 ms  | 2.79x |
| Sieve (100K × 20,000)             | 2,851 ms  | 6,508 ms  | 2.28x |
| Matrix 1280×1280                  | 2,349 ms  | 6,875 ms  | 2.93x |
| HashMap (10M put/get, isolated)   | 1,471 ms  | 5,488 ms  | 3.73x |
| String/Regex (100K, isolated)     | 54 ms     | 193 ms    | 3.57x |
| Binary Trees (depth 18, isolated) | 176 ms    | 1,468 ms  | 8.34x |

Row notes:

- **Fibonacci** is recursion-bound; the recursive self-call already
  compiles to a guarded direct call, and the remaining gap is register
  allocation and recursion inlining, which the current backends do not do.
- **Binary Trees** was measured at `-Xmx8g` as seven alternating
  fresh-process pairs; all fourteen checksums were `68332206`. The full
  optimization story is
  [docs/internal/performance/binarytrees-half-gap-20260718.md](docs/internal/performance/binarytrees-half-gap-20260718.md).
  A July 2026 dev regression that temporarily quadrupled this row was
  root-caused (two independent causes) and fixed — see
  [docs/internal/performance/bt18-inline-tlab-regression-20260724.md](docs/internal/performance/bt18-inline-tlab-regression-20260724.md).
  The perf gate's anchored baseline is 1,550 ms, reflecting a deliberate
  ~2% correctness hardening (explicit header initialization in the inline
  allocator) accepted after that fix.

### The performance gate — DOES NOT CURRENTLY EXIST

This section previously described a mandatory gate,
`regression-suite/perf/run-cratonbench-gate.sh`, that ran every phase isolated
(pinned, `-Xmx8g`, median of 5), verified the exact checksum on every run,
enforced a 5% budget over per-host anchored baselines, and refused to measure on
a loaded host.

**That script is not in the repository.** `regression-suite/` contains
`README.md`, `run.sh`, `src/` and `build/` — there is no `perf/` directory, and
no file matching `*cratonbench*` or a perf-gate name is tracked anywhere in the
tree. It is also not present untracked on the Windows checkout or the Azure
build host. So there is at present **no automated perf regression gate**, and
the "anchored baseline" numbers quoted in the row notes above cannot be checked
by any committed tooling.

The described design is a good one and worth rebuilding. Until it exists, the
honest process for a perf-affecting change is:

1. Build the pre-change commit and the post-change commit as **two
   uniquely-named binaries on the same host**.
2. Run the relevant per-kernel harness from the table above against both,
   alternating, several runs each.
3. **Compare checksums first.** A changed checksum means the change is wrong;
   a faster wrong answer is a bug, not a result.
4. Report absolute times for both binaries and the host's load average, since
   this host is shared and contended — ratios between your two binaries are
   meaningful, absolute numbers across hosts and dates are not.
5. `perf record -g --call-graph=dwarf -F 199` on both, and report the symbol
   deltas, not just wall clock.

### Optimization history

The journey from the earliest 20–100x gaps to the current table is
documented round by round:

- [docs/JIT_OPTIMIZATION.md](docs/JIT_OPTIMIZATION.md) — the full 26-round JIT journey
- [docs/internal/performance/halfgap-residuals-20260718.md](docs/internal/performance/halfgap-residuals-20260718.md)
- [docs/internal/performance/halfgap-20260717.md](docs/internal/performance/halfgap-20260717.md)
- [docs/internal/performance/hashmap-sieve-half-gap-20260714.md](docs/internal/performance/hashmap-sieve-half-gap-20260714.md)
- [docs/internal/performance/quickbench-half-gap-3rows-20260713.md](docs/internal/performance/quickbench-half-gap-3rows-20260713.md)
- [docs/internal/performance/string-regex-overallocated-groups-fastpath-20260714.md](docs/internal/performance/string-regex-overallocated-groups-fastpath-20260714.md)

## GPU benchmarks

### Setup

Measured 2026-07-11 on a GeForce RTX 2060 (sm_75) against HotSpot JDK 25
(C2) and TornadoVM 4.0.1 (PTX backend, `@Parallel`/`@Reduce` + TaskGraph
API). CratonVM offload is **transparent**: plain static methods over
primitive arrays, no annotations, no API
(`cargo build --features gpu-driver`, run with `--gpu`). All timings are
warm and include the full per-call H2D + kernel + D2H round-trip.

| Kernel (N = 2²⁴)                         | HotSpot C2 | TornadoVM GPU | CratonVM GPU | vs HotSpot | vs TornadoVM |
|------------------------------------------|------------|---------------|--------------|------------|--------------|
| Integer div-chain (48 divs/elem)         | 1,910 ms   | 28 ms         | **9 ms**     | **212x**   | **3.1x**     |
| Double div-chain (64 divs/elem)          | 1,508 ms   | 129 ms        | **91 ms**    | **16.6x**  | **1.4x**     |
| 96 multiply-adds/elem (AVX2 on CPU)      | 8 ms       | 17 ms         | 11 ms        | 0.7x       | 1.5x         |
| Dot-product reduction (int·int → long)   | 7 ms       | unimplemented | 18 ms        | 0.4x       | n/a          |

Notes:

- The div-chain rows are the "GPU wins big" cases: division has no
  competitive CPU-vectorized form, so raw parallelism wins at every size
  tested (2²⁰–2²⁶; the ratios hold steady across sizes).
- CratonVM's double-division checksum is **bit-exact** with HotSpot at
  every size (`div.rn.f64` is IEEE-754 round-to-nearest, same as x86
  `vdivpd`); TornadoVM's diverges slightly — its PTX backend doesn't
  guarantee bit-exact division.
- TornadoVM 4.0.1 throws `TornadoInternalError: unimplemented` on the
  equivalent `@Reduce`-over-`LongArray` kernel; CratonVM's transparent
  reduction handles it (slowly — a proper tree/shared-memory reduction is
  an open item).
- The multiply-add and dot-product rows are kept as honest counter-cases:
  CPU AVX2 stays competitive on MAD-dominated kernels at every size, and a
  single atomic accumulator doesn't get relatively cheaper with more
  elements.

Full GPU results — more input sizes, `ldc`-constant kernels, cold-start
numbers up to N = 2²⁸, kernel sources, and eligibility rules — are in
[docs/gpu/README.md](docs/gpu/README.md).

## Reproducing

The commands previously listed here referenced `bench/CratonBench.java` and
`regression-suite/perf/run-cratonbench-gate.sh`, neither of which exists (see
the Harness and performance-gate sections above). What follows works today.

```bash
# Build. On a memory-constrained or contended host, disable fat LTO: the
# release profile's lto="fat" + codegen-units=1 can get the final cratonvm-cli
# rustc OOM-SIGKILLed (shows up as "signal: 9, SIGKILL", NOT a compile error).
CARGO_PROFILE_RELEASE_LTO=off cargo build --release -p cratonvm-cli -j6

# Compile the per-kernel harnesses that actually exist.
javac -d bench-classes bench/BinTreesClassic.java bench/HashMapOnly.java

# Binary Trees (depth as argv). Expected checksum at d=18: 68332206.
./target/release/cratonvm -Xmx8g -cp bench-classes BinTreesClassic 18

# HashMap put/get. Expected checksum at n=1,000,000: 15499991500000.
./target/release/cratonvm -Xmx8g -cp bench-classes HashMapOnly

# HotSpot reference on the same host, same flags:
$JAVA_HOME/bin/java -Xmx8g -cp bench-classes BinTreesClassic 18

# Profile rather than guess. This is how the layout-registry lookup was found
# to be ~40% of the Binary Trees run (37.8% in class_layout_for_fields plus
# 4.8% in the SipHash it used for a (u32,u32) key).
perf record -g --call-graph=dwarf -F 199 -o bt.perf -- \
  ./target/release/cratonvm -Xmx8g -cp bench-classes BinTreesClassic 18
perf report -i bt.perf --no-children --percent-limit 1 --stdio
```
