# CratonVM Roadmap

CratonVM is an experimental Java Virtual Machine in Rust. This roadmap lists
the next milestones in rough priority order. Granular, session-level work
items are tracked internally and are subject to change.

## Status

CratonVM currently runs a wide subset of Java SE 8-25 bytecode on a custom
x86-64 JIT. It is research-grade software; see [SECURITY.md](SECURITY.md) for
production caveats.

## Near-term (next minor release)

- Tighter HotSpot C2 performance parity, with blocking regression budgets for
  HashMap, Binary Trees, and Regex before optimizing broader benchmark totals.
- GC throughput improvements on allocation-heavy workloads (Binary Trees).
  On the generational backend, moving-young collection is requested by default
  (`CRATONVM_NO_MOVING_YOUNG` opts out), so its remaining optimizations — the
  `pointer_map` `FxHashMap`, the disabled self-call spill elision, and the
  unpriced `jit_frame_record` helper — are still live work. The *default*
  collector is ZGC (below), whose throughput work is tracked separately.
- Repeatable framework-throughput qualification for Spring Boot, Quarkus, and
  Micronaut, following [`docs/framework-throughput.md`](docs/framework-throughput.md).
- Complete `java.util.concurrent` parity (ForkJoin, ReentrantReadWriteLock,
  Phaser).
- Bytecode verifier completeness for pre-Java-7 class files.
- AArch64 JIT backend feature parity with x86-64.

The near-term and medium-term lists below are not stretch goals. Pre-Java-7
verifier coverage, `java.util.concurrent` parity, AArch64 parity, real-JDK boot
from a stock `java.base`, JNI function-table and lifecycle coverage, JFR event
coverage, and JEP 261 module semantics are the unresolved holes that currently
define what CratonVM can be used for. Read them as the platform's ceiling, not
as a wish list.

## Medium-term

- Real-JDK boot path stability: load `java.base` JMOD as primary stdlib. The
  instrument for measuring how far that has actually got is the JDK-only mode
  section below.
- JNI: full function-table coverage and OnLoad/OnUnload protocol.
- JCK compliance run on Java SE 25 (see [docs/legal.md](docs/legal.md)).
- Concurrent garbage collector: G1 maturity, and production low-latency ZGC.
  ZGC has been the **default** collector since 2026-08-10 (`default = ["zgc"]`
  in `gc/Cargo.toml`, `GcAlgorithm::Zgc` in `vm/src/config.rs`) and has since
  grown colored pointers, a load barrier, concurrent marking, compaction and an
  opt-in generational mode. What remains open is production hardening rather
  than existence, and the JIT-side load barrier is the centre of it. It is now
  **plumbed but not armed**: `gc/src/vm_heap.rs::load_ref_slot_barriered` is the
  backend-dispatching seam, the reference-slot accessors and ZGC's compaction
  rewrite are relaxed atomics, the JIT's compact-field read routes through the
  seam, and the inline `aastore` arm consults the armed gate. None of that has
  executed: `vm/src/vm/vm_init.rs` pins `RELOCATION_REQUESTED = false` and
  `set_barrier_color` has no non-test caller, so the barrier is unarmed for the
  life of every shipping process and the work is **staged, not measured**. The
  measurement that would settle it is a run with the barrier armed showing
  `ref_load_census::BARRIERED_LOADS` non-zero and `UNBARRIERED_LOADS` zero; it
  cannot be taken until `jit_aaload` can reach a heap handle (an ABI change),
  the `ObjectRef`-holding static and legacy slots become atomic, and arming
  happens at a safepoint. Design:
  [docs/feature-designs/zgc-jit-load-barrier.md](docs/feature-designs/zgc-jit-load-barrier.md);
  the plan for the rest is
  [docs/feature-designs/zgc-production-implementation-plan.md](docs/feature-designs/zgc-production-implementation-plan.md).
- JFR event coverage matching OpenJDK 25.

## Longer-term

- AWT / Swing headful support (current builds are headless-only).
- Module system: full JEP 261 resolution semantics.
- GPU offload — see the dedicated section below.
- Project Panama foreign linker maturity.

## JDK-only mode (`--jdk-only`)

`--jdk-only` is a runtime policy declaring that **real JDK class bytes are
authoritative**: no non-array class is fabricated without real bytes, no
synthetic-stub native is registered or invoked, and concrete bytecode beats a
registered native unless that native is a reviewed intrinsic. It is orthogonal
to `--real-jdk` / `--synthetic-jdk`, which choose which class library boots.
Normative contract:
[`docs/feature-designs/jdk-only-mode.md`](docs/feature-designs/jdk-only-mode.md).

It exists to answer a question the rest of this roadmap cannot: *how much of a
program's execution is real class-library behaviour, and how much is CratonVM
standing in for it?* Everything above about real-JDK boot, `java.util.concurrent`
parity, JNI, and module semantics is measured by this mode's censuses.

### Staged rollout

Four stages. **Stage 1 is where the feature is today** — the stage names and
the gates for stages 2-4 are a proposal recorded here so that "stage 1 of 4",
which several documents already cite, resolves to something specific.

| Stage | Meaning | Gate to leave it |
|---|---|---|
| **1. Internal diagnostic** *(current)* | Instrumentation and measurement. Only class fabrication and synthetic-native **registration** enforce; the remaining dispatch paths are counted, not blocked. A program that runs fine under `--real-jdk` may fail here — that is the intended signal. Not a compatibility guarantee, and not a supported runtime mode. | Every tier-1 item below closed; every native-vs-bytecode dispatch path (interpreter warm and cold, JIT, reflection, JNI, method handles) routed through `resolve_dispatch` and counted; the census's `real_declaring_method` probe filled in so classification stops being guesswork. |
| **2. Experimental** | `--jdk-only` refuses on every dispatch path, not just registration and fabrication. Failures are structured and actionable. | The 21-vector strict corpus (`SUITE=jdk-only`) green on both OS legs; the advisory `jdk-only` CI job promoted by deleting its `continue-on-error`, which its own comment ties to `difftest/seeds-jdk-only` being deterministic across both legs. |
| **3. Preview** | The compatibility surface is gone rather than merely refused: zero `SyntheticStub` entries in the final registry, zero `CompatibilityStub` non-array classes. | `BASELINE_SYNTHETIC_STUBS` driven from 157 to 0 in `native-builtins/tests/stub_ratchet.rs`, with `strict_mode_refuses_nothing` un-ignored in the same change; `Class::is_synthetic_stub` deleted once its readers move to `ClassOrigin`. |
| **4. Stable** | A supported runtime mode with a compatibility claim behind it. | The full acceptance criteria in contract §11, including no new HotSpot divergence on the strict differential corpus, and a **blocking** zero-stub census gate rather than an advisory one. |

Two notes on the gates, so they are not read as more than they are:

- **The blocking/advisory split is already real and should not be confused.**
  The synthetic-stub ratchet step in `build-and-test` is **blocking** today — it
  runs in a job that provisions a JDK and carries no `continue-on-error`. The
  new `jdk-only` job is advisory on purpose, because at stage 1 a strict run is
  *expected* to fail on real workloads.
- **The JDK matrix is a proposal, not existing coverage.** Every other CI job
  pins JDK 25, so JDK 21 is currently tested by nothing at all. "Real class
  bytes are authoritative" is a claim about a *specific* runtime image, and a
  boot-image or module-set difference between feature versions shows up as a
  class-origin census diff and nowhere else — which is why the advisory job
  proposes a second leg. Widening JDK coverage repo-wide is a separate decision.

### Work required before the mode is complete

Ranked by **danger, not effort**, and tracked in
[`docs/known-issues/jdk-only/README.md`](docs/known-issues/jdk-only/README.md),
which carries the evidence for each. Nothing in either tier is closed.

**Tier 1 — causes silent wrong behaviour.** No exception, no log line, no
failing test. These are the reason the mode cannot advance past stage 1, and
they are dangerous in the *default* `compatible` mode too, not only under
`--jdk-only`:

1. **`NativeKind` is ambient and defaults to `SyntheticStub`.** `register()`
   takes no kind; it is inherited from a mutable registry field. A genuine
   bridge registered outside a `with_category` scope is silently classified a
   stub — and then dropped under `CRATONVM_NO_STUBS` / `--jdk-only`. A stub
   created by omission has no syntactic marker, so a grep-based census
   undercounts by construction. This already caused one boot regression.
   Everything else in this tier ends with "let `resolve_dispatch`
   decide from the kind", so this must land first.
2. **Fabricated object layouts leak into native code.** Index-based field access
   against assumed synthetic layouts still resolves against real bytes, pointing
   at a *different* field. Two confirmed sites break under strict mode; three
   crates were never swept.
3. **The forced-native `String` policy exists in three disagreeing copies** — a
   positive list on one path, an exclusion list on another, and thin direct-call
   ladders baked into JIT-emitted code. The disagreement has already made a
   landed, measured performance fix statically unreachable.
4. **`ensure_synthetic_class` can record a violation but cannot enforce one.**
   It returns a bare `ClassId`, so under `--jdk-only` it records and fabricates
   anyway, across 64 call sites in 28 files.
5. **VM-internal classes are mislabelled `CompatibilityStub`** to avoid flipping
   the derived `is_synthetic_stub` bool, which makes the stage-3 zero-stub
   criterion unachievable by construction.
6. **Cached invoke targets drop the `NativeKind`,** so a cache *hit* cannot
   re-apply the policy and is uncounted. The JIT's inline-cache slots have the
   same hole; fixing either alone buys nothing.
7. **The real-protected-stub allow-lists diverge** (11 classes vs 10, with a
   documented heap-corruption reason behind the difference). They must be
   reconciled; both naive merge directions reintroduce a known defect.
8. **The `ThreadPoolExecutor.execute` receiver-shape special case is copied
   eight times.** A mechanical "delete every marked site" sweep leaves half the
   duplication enforcing a policy the other half no longer applies.

**Tier 2 — the instruments tier 1 must be measured with.** The census's
`real_declaring_method` is `null` on every row, `ClassOriginEntry::requested_by`
is `null`, and `--trace-jdk-only` is a poll rather than a live trace. None
causes wrong behaviour; all three are why the tier-1 items say "needs runtime
evidence from the census".

Beyond those, the per-service-area blockers that stop broad real-class execution
— `String` dispatch, thread/executor semantics, ForkJoin worker execution,
`ProcessHandle`, reflection accessors, JNI binding, JPMS, NIO — are inventoried
with their current evidence in
[`docs/known-issues/jdk-only/runtime-services-blocker-inventory.md`](docs/known-issues/jdk-only/runtime-services-blocker-inventory.md).

Two standing constraints on all of it: the default `compatible` mode must remain
**byte-for-byte unchanged** (most of the dangerous mistakes catalogued so far
were `compatible`-mode behaviour changes made while intending to fix strict
mode), and no process-global state may be added for this feature — two existing
globals are already logged as violations to remove.

## GPU offload

**Validated on real hardware** (RTX 2060, `--features gpu-driver`):
automatic offload for the documented eligible kernel subset matches HotSpot
checksums bit-for-bit and reaches ~210x over HotSpot C2 and
~3x over TornadoVM's PTX backend on a 48-division-per-element div-chain kernel at n = 2^24
(see [`bench-gpu/results/divchain-comparison-20260711.md`](bench-gpu/results/divchain-comparison-20260711.md)
and [docs/gpu/COMPARISON.md](docs/gpu/COMPARISON.md)). It remains opt-in
(`--features gpu-offload`/`gpu-driver`, `--gpu` at runtime) and CUDA/NVIDIA-only; see
[docs/gpu/README.md](docs/gpu/README.md).

Current product limits:

- Array-returning kernels and device-resident array chaining are not yet
  implemented; scalar reductions and caller-supplied output arrays are.
- Explicit non-default stream selection is only partially wired through the
  Java API.
- Analyzer/lowering coverage remains intentionally narrower than Java:
  non-canonical and nested loop shapes are rejected rather than silently
  offloaded.
- There is no self-hosted hardware CI: the `bench-gpu/` suite is not run on real
  CUDA hardware on every change, so the numbers above are point-in-time
  measurements rather than a continuously enforced budget.

Asynchronous completion and the JIT-caller admission gate are complete.

Longer-horizon:

- 2D / nested counted loops (the analyzer currently accepts only a single canonical
  `for (i = 0; i < bound; i++)` loop per method).
- Multi-GPU selection beyond a single `--gpu-device` ordinal.
- Float/double reductions, pending a deterministic-summation strategy (naive atomic float
  add is non-associative and would diverge from HotSpot's sequential result).
- OpenCL/SPIR-V/multi-vendor backends are a non-goal: CratonVM's offload path is deliberately
  CUDA-only (see the positioning discussion in [docs/gpu/COMPARISON.md](docs/gpu/COMPARISON.md));
  multi-backend support is TornadoVM's niche, not this project's.

## Success criteria (aspirational targets)

These are directional goals to gauge progress, not guarantees or claims of
current state. CratonVM remains research-grade software; numbers are targets we
are aiming at, and may shift as priorities change.

- **Performance vs HotSpot C2** - bring every row of
  [BENCHMARK.md](BENCHMARK.md)'s interleaved series to <=1.2x of JDK 25 C2, with
  no single kernel above 1.5x. Two rows (Matrix, Sieve) are already at parity;
  the worst is Binary Trees at depth 18, measured **9.66x** in that series,
  which GC throughput work has to bring under 5x. There is no aggregate "TOTAL"
  ratio to quote — BENCHMARK.md deliberately publishes seven per-kernel rows
  from one window rather than one number, and the `3.7x` TOTAL and `23.7x`
  Binary Trees figures this bullet carried until 2026-09-01 were not
  reproducible from any table in the tree.
- **Bytecode verifier** - reach 100% of the structural/type checks needed to
  verify the pre-Java-7 split-verifier class-file corpus without `--noverify`.
- **JCK / compliance** - target >=90% pass rate on a single chosen JCK area,
  such as `lang` or `vm`, on Java SE 25 as a first compliance milestone
  (subject to the licensing notes in [docs/legal.md](docs/legal.md)).
- **AArch64 JIT** - reach feature parity with the x86-64 backend, validated by
  an identical JIT test suite pass rate across both backends.
- **`java.util.concurrent`** - full parity for ForkJoin, ReentrantReadWriteLock,
  and Phaser, with the corresponding JDK conformance tests passing.
- **Real-JDK boot** - make the `java.base` JMOD load path the default stdlib
  source, booting a stock `java.base` without fallback.
- **JDK-only mode** - reach stage 4 (stable) as defined above: a zero-stub,
  zero-fabricated-class strict run held by a blocking CI gate. The measurable
  intermediate is the frozen synthetic-stub baseline, currently 157, and the
  count of dispatch paths not yet routed through `resolve_dispatch`.

## How to influence the roadmap

File an issue describing your use case, or open a pull request implementing a
milestone. See [CONTRIBUTING.md](CONTRIBUTING.md).
