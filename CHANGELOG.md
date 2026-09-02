# Changelog

All notable changes to CratonVM will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/),
and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

### 2026-09-02 The generational young collector's copy phase can run in parallel

`GenerationalHeap`'s moving (Cheney) young cycle copied its survivors on one
thread, and the reason was structural rather than incidental:
`forward_object` takes `&mut Arena` for to-space and `&mut OldGen` for
promotions, so the borrow checker enforced a single copier. The mark closure
and the sweep's span zeroing had already gone parallel, which left `cheney_drain`
as the one serial phase of a moving pause — and on a survivor-heavy cycle it
*is* the pause.

New `gc/src/gen_evac.rs` supplies the pieces that let N workers copy at once:

* **Copy-then-CAS forwarding.** A worker copies speculatively, then claims the
  object with a tagged compare-exchange on the source mark word. The loser
  abandons its copy and adopts the winner's address, so every reference
  converges. This is the protocol `g1::SharedEvac::evacuate` already uses,
  carried over with the lesson its two `DEFECT-2` sites paid for: a CAS loser,
  and an already-forwarded fast-path hit, must still RECORD `old -> new`, or a
  root naming `old` is never remapped and dangles once from-space is reset.
* **Per-worker to-space buffers**, carved out of the arena's un-bumped tail by
  one atomic `fetch_add` (`Arena::parallel_evacuation_region` /
  `commit_parallel_evacuation`). A retired buffer's tail is stamped with the
  TLAB retire path's existing `TLAB_FILLER` / `GAP_FILLER` sentinels, so the
  arena stays walkable object-by-object when the next cycle reads it as
  from-space. Objects at or above an eighth of a buffer bypass it entirely so
  one large object cannot displace a buffer's worth of small ones; what bounds
  the wasted tail is `ParEvac::plan`'s spendable allowance (see below).
* **Per-worker output shards** for the forwarding map, the deferred dirty
  cards and the copy tally, merged by the driver after the completion barrier.
  The copy tally's thread-local carried a comment asserting the copy phase was
  single-threaded; it now says which half of that is still true and the driver
  folds each shard in with `copy_tally_merge`.

Five things were wrong on the first cut and are worth recording, because every
one of them was invisible to the correctness tests — the object graph came out
right each time. The last two were invisible to the unit tests ENTIRELY and
took an end-to-end run to surface:

* **The helpers did nothing.** Measured on a 6144-node DAG, 96 layers deep,
  eight workers: the driver copied all 6144 and the helpers copied zero. The
  drain only published work once a local stack passed 2048 entries, and a
  transitive closure over a graph like that keeps a frontier of about `width`.
  The load-bearing rule is now the other one — publish half the local stack the
  moment any worker is idle, off a relaxed `idle_hint` load — plus a fair
  `len / threads` acquire share instead of letting the first waking worker
  swallow a seed set smaller than the chunk.
* **The threads were spawned per pause.** With the sharing rule fixed the
  helpers still scanned nothing across twelve consecutive collections: the
  driver drained the whole closure in about a millisecond while seven fresh OS
  threads were still on their way to their first lock. Switching to the
  persistent `evac_pool` — which exists for exactly this, and whose module note
  makes the same argument for G1 — took helper participation from 0 to ~85% of
  destinations scanned, and the test that measures it from 3–5 s of retries to
  0.07 s on the first attempt.
* **The buffers were a constant.** Eight workers each holding a fixed 64 KiB
  buffer retired **327 KiB of filler for 393 KiB of survivors**. Sizing the
  buffer against the live set (`from_used / (workers * 4)`, clamped to
  4 KiB…64 KiB) brought that to tens of KiB with no loss of participation.
* **The reservation was worst-cased, and that made the feature unreachable.**
  Only a real workload could show it. `bench/BinT.java` at depth 18,
  `-Xmx512m`, first moving cycle: `to_headroom=134217728` against
  `from_used=134096736` — a young GC triggers with from-space **99.91% full**,
  so the Cheney invariant's `to_headroom >= from_used` left 120,992 bytes and
  nothing more. The budget asked for `from_used/7` (19 MB) of worst-case
  abandoned buffer tails on top, so it declined — and would have declined on
  every cycle of every real workload, with all twenty unit tests green because
  each sizes its to-space generously. The waste is now BUDGETED instead:
  the slack that exists is split half to in-flight buffers and half to a
  spendable `waste_allowance`, and once that is gone `plab_alloc` serves
  objects from exact per-object spans rather than abandoning another tail.
  Region consumption is then provably `<= from_used + allowance +
  workers * plab`, i.e. exactly the headroom. `plan_accepts_the_measured_shape_of_a_real_young_collection`
  freezes those two numbers so the regression cannot come back.
* **And the fix for that was still not enough**, which only a second real run
  showed: with buffers sized from the slack, eight workers need
  `8 * 4 KiB` of it, and the slack at the trigger point lands either side of
  that from one collection to the next — so the copy phase engaged on roughly
  half of bt18's cycles and fell back to serial on the rest, invisibly.
  Buffers are an OPTIMISATION, not a precondition: with `plab_bytes == 0`
  every object takes its own exact span off the shared cursor, consuming
  exactly the survivors, which `to_headroom >= from_used` already guarantees.
  The cycle now runs bufferless rather than serial when the slack is thin, and
  `declined_for_slack` is reserved for the one case the collection's own
  backstop should have caught first. Measured after: `cycles=1`,
  `declined_for_slack=0` on five consecutive bt18 runs, against roughly one in
  two before.

`PAR_EVAC_HELPER_SCANS` exists so the first of those is a number rather than a
wall-clock mystery next time: "it engaged" and "it spread the work" are
different claims, and the first held while the second did not.

The three seed phases (precise roots, overlay-held edges, dirty-card
old→young slots) are now `seed_roots` / `seed_overlay_roots` /
`seed_dirty_card_roots`, called by BOTH evacuators. Seeding is where the two
could have drifted invisibly — a card slot the parallel path forgot surfaces
as a live object reclaimed, a week later, on the other collector.

`CRATONVM_GC_PAR_EVAC=0` forces the serial evacuator. It is default-on because
it cannot engage on its own: the cycle must be a moving one and the existing
`CRATONVM_GC_PAR_THREADS` policy must already want two or more workers.
`gen_evac::par_evac_census()` reports
`(cycles, cas_losses, declined_for_slack, filler_bytes, helper_scans)`, so
"it never engaged" and "it engaged and did nothing" are distinguishable.

Covered by eight end-to-end young-GC tests (each asserting on a PER-THREAD
cycle counter that the parallel path really ran — the process-global one is
bumped by every other test in the binary) and nine unit tests of `evacuate`'s
refusal and convergence arms, the buffer sizing, and the reservation. The CAS-loser arm needed
a `cfg(test)` seam: on the first cut, measured across the whole young-GC suite,
a 500-parent fan-in produced **zero** CAS losses — every second reader took the
already-forwarded fast path — so a test relying on a real race would have been
asserting nothing. (With the drain sharing work properly the arm now also fires
naturally, a few times per cycle on the wide DAG; the seam stays because that
is a schedule, not a guarantee.)

End-to-end on `bench/BinT.java` depth 18 (`--XX:UseGc Generational -Xmx512m
CRATONVM_MOVING_YOUNG=1 CRATONVM_GC_PAR_THREADS=8`), five consecutive runs: the
copy phase engages every time (`cycles=1 declined_for_slack=0`), helpers scan
~1.3M destinations, and the program's checksum is identical to the
`CRATONVM_GC_PAR_EVAC=0` run. The census is printed by `--verbose:gc` as
`[GC] par_evac:`, unconditionally, including the all-zero line — which is how
both of the reservation defects above were found.

**Soak.** Driven by `CRATONVM_DBG_GC_STRESS` so the copy phase runs hundreds of
times per process rather than once, and checked against closed-form oracles
(each benchmark's own documented checksum, never a second run of this VM):

| dimension | coverage |
|---|---|
| object shapes | `BinT` two-reference nodes; `HashMapOnly` boxed Integers over a resizing REFERENCE ARRAY plus compact-layout nodes; `StringRegexOnly` primitive arrays, Strings and a stateful `Matcher` |
| heap sizes | 128m / 256m / 512m |
| GC stress | 250 KB and 1–2 MB per forced collection |
| worker counts | 2, 3, 4, 8, 16 |

**45 runs, 45 correct checksums, ~12,950 parallel copy cycles**, and
`helper_scans` rises monotonically with the worker count (29,855 at 2 workers
to 49,855 at 16) — so the extra workers really do take work rather than merely
existing. Two runs reported `cycles=0`; the census says so rather than letting
a vacuous run read as a pass.

**Measured on the merged tree**, `BinT` depth 18 at `-Xmx512m` — one large
cycle copying ~1.5M objects out of a 128 MB from-space. ABBA-interleaved
(P S S P), first pair discarded as cold, `objects_copied` checked equal within
every pair, and runs that took the NON-moving sweep retried rather than counted
(only about half of them take the moving path).

| | arm | n | min | median | max |
|---|---|---|---|---|---|
| `cheney_drain` | parallel, 8 workers | 8 | 2,304 ms | **3,008 ms** | 3,601 ms |
| `cheney_drain` | serial | 8 | 8,693 ms | **11,560 ms** | 14,098 ms |
| whole pause | parallel, 8 workers | 6 | 2,035 ms | **2,664 ms** | 2,901 ms |
| whole pause | serial | 5 | 6,316 ms | **9,760 ms** | 12,412 ms |

Both pairs of ranges are DISJOINT — the parallel arm's worst sample beats the
serial arm's best — which is what makes this a result on a shared host rather
than a ratio between two noisy medians. Copy phase: median 3.84x, pessimal
pairing 2.41x. Whole pause: median 3.66x, pessimal 2.18x. Taken with the host
at 100% CPU throughout, and the parallel arm's spread was TIGHTER than in an
earlier quiet-host run (1.6x against 2.4x), so contention is not what produced
the separation.

The pause tracks the phase now because `pre_evacuate` — the from-space
object-start walk, 3,174 ms and the largest phase when this change was first
measured on its own branch — is **0 ms** on the merged tree: another lane
parallelised it. An earlier draft of this entry named it as the next place to
work; that was true of the branch and is not true of `dev`.

Two caveats the numbers do not state. This is a **debug build**: the
per-object copy is unoptimised on both arms, and release is likely to narrow
the ratio, since optimisation makes the copy cheaper while the coordination
stays. And it is **one workload**, chosen because it is survivor-heavy and
therefore the best case for parallel copying.

An earlier wall-clock A/B under `CRATONVM_DBG_GC_STRESS` was discarded rather
than reported: it showed a 6x spread WITHIN one arm against an 11% median gap.
GC stress is the right instrument for a soak and the wrong one for a throughput
measurement — it maximises the number of cycles while minimising the work in
each, which is exactly where a parallel copy has least to offer.

### 2026-09-02 The loop-invariant `arraylength` is hoisted, and it was worth 5.4x

`for (int i = 0; i < a.length; i++)` re-evaluates `a.length` at the top of
every iteration, because that is what javac emits and neither x86-64 backend
moved it. Both do now — `ArrayLenHoist` in the single-pass backend
(`jit/src/x64/licm.rs`), `Op::ArrayLength` support in the optimizing tier's
LICM (`jit/src/ir_optimize.rs`) — and one bounds-checked `char[]` element read
in a compiled counted loop goes from **3.46 to 0.64 ns/char** — onto the
hand-hoisted control, and from 24.7x HotSpot to 4.6x on the same host and run.

The size is the finding. `array-element-load-baseline-codegen-20260901` sized
this at ~5 instructions of 21 by reading the emitter, concluded "44x to about
14x", and never ran the one-method control — the same loop with `a.length` in a
local — that prices it. It was the whole gap. Two things the reading could not
see:

* the traced emitter is not the one that ran. The optimizing tier is *better*
  than the single-pass backend on the local-bound loop and **3x worse** on the
  `arraylength`-bound one, and the routing sends the second shape to it;
* in that tier, `Op::ArrayLength` is impure, so `loop_has_hard_barrier` counted
  it and a javac counted loop **disqualified its own LICM by the very node it
  needed hoisted** — `CRATONVM_DBG_LICM=1` reported `hard_barrier=true,
  0 load(s)` on every candidate header.

That pass also never saw an inner loop at all: it classified a header's inputs
by forward reachability, which for a nested inner header wraps round the outer
back edge and makes every input look like a back edge. Dominance answers it —
one reachability walk with the header deleted — applied only where the old test
found no pre-header, so every loop it already handled keeps its answer.

Neither hoist needs a deopt: both take only sites that run unconditionally on
the first pass through the header, so a null receiver throws the NPE the body
would have thrown, at the same instant and through the same stub.

Also on this path: the array bounds check's length load moved into its cold
stub (`CMP ECX,[RAX+len] ; JAE` — one instruction and four bytes fewer on every
emitted bounds check), the safepoint poll became a single RIP-relative `TEST`
where the flag is in reach, and `ARRAY_LENGTH_OFFSET`'s comment — which said
"8, not 12" above a value of 4 — is now a const assert against
`offset_of!(ObjectHeader, shape)`.

Default-on with a switch each: `CRATONVM_DISABLE_ARRAYLEN_LICM`,
`CRATONVM_JIT_LICM=0`, `CRATONVM_JIT_RIP_SAFEPOINT_POLL=0`,
`CRATONVM_JIT_FUSED_BOUNDS_LOAD=0`. Probe:
`probes/ArrayElemLoadCost.java`. See
`array-element-load-baseline-codegen-FIXED-20260902.md`.

### 2026-08-06 The `ThreadPoolExecutor.execute` receiver-shape special case is gone

Nine dispatch sites across four files decided whether to run
`ThreadPoolExecutor.execute`'s real bytecode by reading the receiver's
`workers` field — eight receiver-shape probes plus the one receiver-blind
`force_native_over_real_jdk_bytecode` arm they existed to override. All nine
and the probe helper are deleted.

What replaced them is class-scoped and lives at registration:
`native_es_execute` is tagged `NativeKind::SyntheticStub` and
`java/util/concurrent/ThreadPoolExecutor` joins the real-protected-stub
allow-list, so the one centralised arbitration yields it to the real
`execute()` body for every receiver, on both the warm and the cold dispatch
path. That became correct once the entry above removed CratonVM's ability to
mint a fabricated executor at all. **The native is not deleted** — strict mode
declines to admit it, and the `--features synthetic-jdk` build still runs it,
which is the only build where the real `execute()` bytecode is absent.

No behaviour change on a real JDK image: `probes/L10ThreadPoolInitProbe`,
`JdkOnlyCensusLoadProbe` and the three-arm strict-corpus gate are unchanged in
both modes, and the registry census moves by exactly the two retagged
registrations (`bridge` 10,434 → 10,432, `synthetic-stub` 755 → 757, total
unchanged). Stub ratchet re-frozen 553 → 555 — no new fake; two registrations
that were mis-tagged `Bridge` are now counted where they belonged.

See `jdk-only-wave2-threadpoolexecutor-execute-receiver-shape-RETIRED-20260806.md`.

### 2026-08-06 `Class::is_synthetic_stub` is deleted; `ClassOrigin` is the only answer

The bool answered two different questions — *is this a compatibility
substitution?* (the census and `--jdk-only` policy question) and *does this
class have no class file, so dispatch must look for a native under its own
exact name?* Splitting them is what let `java/lang/reflect/Proxy$Instance` be
reclassified honestly: it is `ClassOrigin::VmInternal`, a generation artefact,
not a stand-in for bytes that were never found — while keeping the three
dispatch sites that genuinely need it, which now ask
`Class::dispatch_lacks_class_file`.

A fabricated `$$Lambda` / `$ProxyN` / `Generated*Accessor*` is likewise
reported as what generated it, the same answer the define-from-bytes path
already gave those names.

`--dump-class-origins` on a dynamic-proxy workload: 420 rows before and after,
`compatibility-stub` 14 → 13, `vm-internal` 1 → 2 — exactly one class moved,
and under `--jdk-only` that probe now fabricates none at all.

See `jdk-only-wave2-vm-internal-classes-mislabelled-RETIRED-20260806.md`.

### 2026-08-06 `Executors.new*` returns real JDK executors in real-JDK mode

`java.util.concurrent.Executors`' pool factories are no longer intercepted when
CratonVM runs against a real JDK image: the real `Executors` bytecode constructs
every executor, so a factory-made pool is built by the genuine
`ThreadPoolExecutor.<init>` rather than by a native that allocated the object and
then tried to reproduce the constructor.

**User-visible fix.** `Executors.newSingleThreadExecutor()` returned a bare
`ThreadPoolExecutor` where the JDK returns
`Executors$AutoShutdownDelegatedExecutorService` wrapping one. Every
`instanceof ThreadPoolExecutor` on the result flipped, and the pool the JDK
guarantees is unconfigurable accepted `setCorePoolSize`. It now matches HotSpot.

Also removed: two fallbacks in the old construction path that wrote a
two-slot placeholder shape onto a real-layout object and returned it as if
construction had succeeded. Nothing observed them firing, but while they existed
an executor could be half-built, which is the receiver shape nine dispatch sites
in the interpreter exist to detect.

`probes/L10ThreadPoolInitProbe` (new) is byte-identical to HotSpot 25 under both
`--real-jdk` and `--jdk-only`. A diagnostic added with it, `CRATONVM_DBG_TPE_SHAPE=1`, reported every
`ThreadPoolExecutor.execute` receiver-shape decision; it was removed the same
day together with the predicate, when L11 item 7 deleted all nine dispatch
sites (see below). The
`--features synthetic-jdk` build is unaffected — it has no real `Executors`
bytecode to fall back to and keeps its own factories.

See `L10-blocker-threadpool-init-DONE-20260806.md`.

### 2026-08-05 CPU benchmark table re-measured in a quiet window; Sieve at parity

All seven CratonBench rows re-taken in one interleaved series on `dev`
@ `ded183df8` against JDK 25.0.3, in a window opened only once the load fell
below 2.5 **and** nothing else was pinned to the measuring core. CratonVM's
run-to-run spread is under 1% on five of the seven rows.

| | ratio | was 2026-07 |
|---|---|---|
| Arithmetic | 1.95x | 2.44x |
| Fibonacci(44) | 5.89x | 2.79x |
| **Sieve** | **0.99x** | 2.28x |
| **Matrix** | **0.99x** | 2.93x |
| HashMap | 2.07x | 1.75x |
| String/Regex | 5.37x | 7.7x |
| Binary Trees | 9.46x | 8.34x |

Two rows are now at parity with HotSpot C2. Sieve's 6.50x of 2026-08-04 was a
live regression and is fixed; see the entry below.

**Sieve's HotSpot arm is bimodal** — ~2,369 ms or ~2,739 ms with nothing
between, so a 9-sample median reports whichever mode won, and two consecutive
series on an unchanged binary read 2,386 ms and 2,734 ms. That row's figure is
the median of 18 pooled samples; the cleanest single series would have claimed
0.87x, i.e. CratonVM 14% faster than HotSpot, which the data does not support.
CratonVM's own samples on that phase are unimodal.

### 2026-08-04 The optimizing tier stops taking methods the single-pass backend does better

`cov-02` taught `IrBuilder::build` to lower `bastore`. The side effect was that
`CratonBench.sieve([ZI)I` stopped falling through to the single-pass backend —
which *vectorises* its `boolean[]` loops — and started getting a scalar IR
body. **2,462 ms became 15,823 ms**, on a phase where CratonVM had been faster
than HotSpot C2, with an unchanged checksum and no failing test.

The general problem: the optimizing tier installs its body whenever it *can*,
and nothing checks that the body is faster than the one the single-pass backend
would have installed.

#### Added
- `jit/src/x64/single_pass_only.rs` — the enumeration of what the single-pass
  backend can do that the optimizing tier cannot: seven classes, each consumed
  by a single-pass emitter at a loop header, each without a counterpart in
  `ir_optimize`/`ir_lower` (three bulk byte-array lowerings, four vectorising
  ones). The admission chain consults it and its verdict names which lowering
  it protected. **The IR tier has no vectoriser at all**, so `cov-02` hitting
  one of these was not bad luck — four more of the same shape were waiting.
- `CRATONVM_JIT='-c1-vector-veto'` — hand those methods back to the IR tier.
  Default on; the switch exists so the veto is bisectable and so its blast
  radius can be measured on one binary rather than argued across two.

#### Changed
- `x64/driver.rs`'s three inlined bulk-byte detector loops are now one call to
  `escape_analysis::detect_bulk_byte_loops`, shared with the veto, so the
  emission path and the admission chain cannot disagree about what the backend
  would emit.
- Corrected two stale comments in `ir_optimize.rs`: `unroll` and `licm` are
  default-**ON**, not "Default-OFF while it soaks". They are why the
  single-pass unroller and hoists are *not* on the veto list, so the stale
  claim was load-bearing in the wrong direction.

#### Notes
- Blast radius, measured with the off-switch on one binary across all ten
  benchmark phases: **exactly one** IR body, the one that was 6.4x slower.
- Loop unswitching was in the first draft of the list and is not in it: its
  emitter's own contract says the sequence is additive and "removing the
  emission yields identical final state". Vetoing on it would have cost IR
  bodies for every loop with an invariant branch to protect nothing.
- Still open: the enumeration catches an advantage somebody wrote down, not one
  nobody did. Closing that needs a backend-parity harness that compiles a
  corpus both ways and compares emitted bytes — see
  `perf-01-sieve-ir-body-slower-than-c1-FIXED-20260804.md`.

### 2026-08-03 Perf gate: it records its own C2 reach, and it can compile its benchmark again

Two changes to `regression-suite/perf/`, from `docs/known-issues/c2/`'s MEAS-02.

**The gate could not compile `bench/CratonBench.java`.** Its own
`export LC_ALL=C` — correct, for the awk distribution arithmetic — makes a
`javac` that derives its source encoding from the platform charset (17 on the
bench host) default to US-ASCII, and the benchmark's header comment has
em-dashes. Every run died at setup with 30 `unmappable character` errors before
measuring anything. The bench host's ambient locale is `C.UTF-8`, so the same
command run by hand succeeded and the failure appeared only inside the gate.
Pinned with `-encoding UTF-8`.

**Every run now records the optimizing tier's per-phase reach.** Across all
seven CratonBench phases the C2/IR tier is asked 8 times, admits 3 and produces
**3** bodies — so the gate measures the single-pass backend, and a CratonBench
delta is not evidence about C2 in either direction. That fact now travels with
the numbers instead of having to be rediscovered.

#### Added
- `ir_requests` / `ir_admitted` / `ir_bodies` in `samples.tsv` and
  `summary.tsv`; one `ir_reach_<phase>` line per phase in `manifest.tsv`, plus
  `ir_reach_total`, `ir_reach_recorded` and `ir_reach_scrape_broken`; a reach
  summary on the console at the end of every run and of every `--calibrate`.
- `regression-suite/perf/c2-reach.sh` — any workload's C2 reach in one run,
  with two consistency checks that refuse rather than report a zero when the
  scrape is reading a log that no longer says what it expects.
- `bench/CratonBenchC2.java` — a candidate workload with a framework-shaped
  node mix, reaching the tier 36/17/11 across three phases. Deliberately **not**
  a gate phase and with no baseline; see
  `meas-02-bench-suite-c2-reach-RETIRED-20260803.md`.

#### Changed
- Results schema **1 → 2**; the default results directory is now
  `regression-suite/perf/results/v2/`. Every existing column kept its name and
  every consumer resolves columns by name, so a v1 reader reads a v2 directory
  correctly.
- `compare.py` reports `C2-tier compiles`, `C2 admitted` and `C2 bodies`
  separately, and names the phases whose delta is not evidence about the
  optimizing tier. `compiles_c2` alone never was that number: it counts
  compiles whose requested *tier* was C2, including every one the optimizing
  pipeline declined and handed back to the single-pass backend, and including
  OSR compiles.
- The gate asks for its VM summaries with the grouped `CRATONVM_DBG=` spelling,
  so a run no longer opens stderr with a legacy-variable deprecation line.

### 2026-07-31 JDK-only mode (`--jdk-only`) — provenance instrumentation, wave 1

A new **runtime** compatibility policy: `--jdk-only` declares that real JDK class
bytes are authoritative, so no non-array class is fabricated without real bytes
and no `NativeKind::SyntheticStub` native is registered or invoked. It is
orthogonal to `--real-jdk` / `--synthetic-jdk`, which select *which class
library* boots; this selects *which substitutions are permitted*. One binary
runs both policies, so a failure can be A/B'd in the same shell.

**This is an internal diagnostic, not a supported runtime mode.** Wave 1 is
instrumentation and measurement: only class fabrication and synthetic-native
*registration* actually enforce, while the remaining dispatch paths are counted
rather than blocked. A program that runs fine under `--real-jdk` may fail under
`--jdk-only` — that is the signal the mode exists to produce. The default
(`compatible`) behaviour is unchanged, on both the launcher and embedded entry
points, and is reached by doing nothing. Normative contract:
`docs/feature-designs/jdk-only-mode.md`; operator guide:
`docs/jdk-only-migration.md`.

#### Added
- `--jdk-only` launcher flag. Implies `JdkMode::Real` and requires a real JDK
  runtime image — there is no silent fallback, and the failure names the flag,
  the searched paths and the accepted JDK layout. Conflicts with
  `--synthetic-jdk` (that library *is* the set of substitutions the flag
  forbids), and the conflict is diagnosed as a policy error rather than a
  library error.
- Four diagnostic flags, all usable in either mode — under the default
  `compatible` mode they census what strict mode *would* reject:
  `--jdk-only-report <FILE>` (JSON violations plus class-origin and
  per-`NativeKind` invocation counters, `schema_version` 1),
  `--dump-class-origins <FILE>` (see below), `--trace-jdk-only` (log each
  recorded violation to stderr), and `--explain-jdk-only` (long-form
  operator-facing explanation per violation, and leaves absolute paths
  unredacted in every report file; they are redacted by default).
- `--dump-class-origins <FILE>` — a new class-origin census, one row per class
  the class manager holds: `{name, origin, reason, requested_by,
  real_bytes_found, loader_id}`, sorted by `(name, loader_id, origin)` for
  byte-stable output, with a `counts` block keyed by origin tag.
- A `ClassOrigin` provenance model on `Class` (`classloading/src/class_origin.rs`),
  replacing "is this a stub, yes or no?" with where the bytes actually came
  from: `BootImage`, `ApplicationClassPath`, `UserDefined`, `VmArray`,
  `HiddenClass`, `GeneratedLambda`, `GeneratedProxy`, `ReflectionAccessor`,
  `VmInternal`, `CompatibilityStub`. Only `CompatibilityStub` is rejected under
  the strict policy; arrays, hidden classes, lambdas, proxies and reflection
  accessors are products of a conforming JVM and are allowed, with their own
  distinct origins. The pre-existing `Class::is_synthetic_stub` bool is
  retained as a **derived mirror** of `origin.is_compatibility_stub()` (~160
  read sites across 17 files depend on it); both are written together through
  `Class::set_origin`.
- Shared policy token `cratonvm_types::compat` (`CompatibilityMode`,
  `ExecutionPolicy`) with `NativeKind::allowed_in` and `ClassOrigin::allowed_in`
  as predicates next to their own types, a structured `JdkOnlyViolation` error
  family in `types/src/error.rs`, and a single policy-aware
  `resolve_dispatch` / `DispatchDecision` native-vs-bytecode decision point in
  `vm/src/vm/vm_exec.rs` that the main interpreter path now routes through.
- Per-VM policy state: `VmConfig::compatibility_mode` (plus `is_jdk_only`,
  `execution_policy`, `validate_compatibility`), propagated into the native
  registry and the `ClassManager` at VM init. No process globals were added for
  this feature.
- C ABI (`libcratonvm`): `cratonvm_create_with_compatibility(args, mode)`,
  `cratonvm_compatibility_mode(vm)` (read back what the live VM actually got),
  and `cratonvm_compatibility_mode_supported(mode)` (a capability probe that
  needs no VM, so a host can avoid a failed create). The mode constants are
  `CRATONVM_COMPATIBILITY_COMPATIBLE = 0` and
  `CRATONVM_COMPATIBILITY_JDK_ONLY = 1`. **These numeric values are a published,
  append-only part of the ABI** — a value may be added, never renumbered — and
  they are `cratonvm_jint` rather than a boolean so a third posture can be added
  later without breaking a compiled host. An unrecognised value is *rejected*
  (`NULL` + `cratonvm_last_error()`), never clamped to `COMPATIBLE`;
  `cratonvm_compatibility_mode` returns `-1`, never a mode value, on a bad
  handle. The option string `"--jdk-only"` is the second route to strict mode
  and the only one available to `JNI_CreateJavaVM`; passing
  `CRATONVM_COMPATIBILITY_COMPATIBLE` alongside `--jdk-only` is a contradiction
  error, not a precedence rule.
- A 21-vector strict regression corpus (`regression-suite`, `SUITE=jdk-only`)
  indexed against the blocker rows in `docs/jdk-only-runtime-services.md`, and
  an advisory `jdk-only` CI job that runs the censuses. Some strict-mode vectors
  are **expected to fail** while fabrication enforcement is incomplete: that is
  the enforcement test working, not a regression.

#### Changed
- **`--dump-native-registry` output format changed (consumer-visible).** The
  native census now emits `"schema_version": 2`; the previous output carried no
  `schema_version` key at all, so any consumer that parsed the old shape needs
  updating. Each `natives[]` entry gains `registered_by` (the registration site,
  captured via `#[track_caller]`), `overwrote` (the `NativeKind` of the entry
  this registration replaced, if any — registration is last-write-wins), and
  `invocations` (times the slot was dispatched this run). A `real_declaring_method`
  field is present and is `null` on every row today; filling it in needs a
  *non-initiating* probe of the runtime image, because resolving it at shutdown
  through ordinary class loading would load classes the run never touched and
  change the very census the file reports. A top-level `"invocations"` block
  gives the per-`NativeKind` dispatch totals. Rows are sorted by
  `(class, name, descriptor, registered_by)` — `registered_by` is part of the
  key because a superseded row and the row that overwrote it share the triple.
  Absolute paths in `registered_by` are redacted unless `--explain-jdk-only` is
  passed.
- `NativeMethodRegistry` gained VM-scoped policy (`set_compatibility_mode`,
  `compatibility_mode`), a refusal log (`refused_registrations`), a
  `schema_version` 2 census (`census`), and hot-path-safe invocation counting
  (`record_invocation`, `invocations_of_kind`) that does not require `&mut self`.
  Under `JdkOnly`, `register()` refuses to insert a `SyntheticStub` and records
  a `SyntheticNativeRegistered` violation instead.

#### Deprecated
- **`CRATONVM_REAL=-stubs` in favour of `--jdk-only`.** The env token keeps
  working unchanged as a native-registry filter, but it now prints a one-time
  note recommending `--jdk-only`: the token can only drop stub *registrations*,
  and cannot express the class-loading or dispatch half of the policy. Strict
  mode is deliberately never inferred from `CRATONVM_REAL` / `CRATONVM_NO_STUBS`,
  from a Cargo feature, or from what the host machine has installed — a run must
  not end up enforcing rules nobody asked for.

#### Known follow-ups
- The residual synthetic-stub set is unchanged: `native-builtins/tests/stub_ratchet.rs`
  still freezes `BASELINE_SYNTHETIC_STUBS = 157` exactly with `SLACK = 0`, and
  the end-state `strict_mode_refuses_nothing` test is deliberately `#[ignore]`d
  until that baseline reaches zero. Wave 1 refuses those registrations under
  `--jdk-only`; it does not retire them. The path is reclassification first,
  deletion second — a previous global drop was reverted the same day it landed.
- The wave-2 backlog is ranked by danger in `docs/known-issues/jdk-only/README.md`;
  its first tier causes **silent wrong behaviour** rather than clean failure.
  Summarised with staging gates in [ROADMAP.md](ROADMAP.md#jdk-only-mode---jdk-only).

---

### 2026-07-11 GPU offload — first real-hardware validation and feature completion

First systematic validation of the GPU offload stack on real hardware (RTX
2060, sm_75, CUDA driver 591.86, `--features gpu-driver`). Two passes the same
day: a morning validation run that found and fixed two dispatch-correctness
bugs, and an evening feature wave that closed most of the follow-ups the
morning pass turned up. See
`docs/known-issues/gpu-offload-followups-20260711.md` for full detail and
remaining open items.

#### Fixed (morning validation pass)
- Offload-eligible `invokestatic` call sites were being promoted into the interpreter's invoke cache after their first dispatch (or first per-call `--gpu-min-work` rejection), permanently bypassing the GPU offload hook on every later call at that site — a cached target dispatches straight to the CPU body and never re-enters `try_dispatch`. Fixed by never promoting a `Handled`/`HandledWithValue`/`FallThroughKeepHooked` site into the invoke cache (`vm/src/runtime/offload.rs`, `DispatchOutcome`).
- A failed kernel's bounds-check `failure_flag` was drained *after* the kernel's array writebacks, so a bounds-check failure let partially-corrupted device state copy into the Java heap before the failure was observed — violating the documented "the interpreter observes no partial GPU state on kernel failure" guarantee. Fixed by draining `FailureFlag` writebacks first regardless of push order (`vm/src/runtime/offload.rs::finalize_submission`).
- Benchmarked the fixed dispatch path against HotSpot JDK 25 (C2) and TornadoVM 4.0.1 (PTX backend) on an RTX 2060, checksums matching HotSpot bit-for-bit at every size: div-chain kernel (48 data-dependent integer divisions/element, unvectorizable on x86) 204–235× over the best CPU; 96-multiply-add kernel (a shape HotSpot C2 *can* auto-vectorize) ties or beats vectorized HotSpot C2 and outruns TornadoVM ~2× on the same kernel. See `bench-gpu/results/` and the README "GPU offload benchmarks" section.

#### Added (evening feature wave)
- Transparent offload for integer/long reduction kernels (`)I`/`)J`-returning methods, e.g. `sum += a[i]*b[i]`) — the interpreter's void-return-only dispatch gate is lifted for proven reductions, with the scalar result pushed onto the operand stack. `)F`/`)D` reductions stay CPU-only by design (GPU float atomic-add is not bit-identical to Java's sequential fp accumulation). Found and fixed in the process: the reduction PTX epilogue emitted the 2-operand `atom.global.add` form, which `ptxas` rejects with "Arguments mismatch" — every reduction kernel had been silently failing module load and falling back to CPU since the epilogue was written; fixed to the 1-operand `red.global.add` accumulate form, with new `ptxas` round-trip tests added for all six lowering shapes (`jit-cuda/src/lowering.rs`). Measured (RTX 2060, N = 2²⁴, `bench-gpu/GpuDotBench.java`, checksum bit-exact): CratonVM-GPU 18 ms vs CratonVM-CPU 76 ms vs HotSpot C2 7 ms — the GPU beats CratonVM's own CPU 4.2× but not vectorized HotSpot C2 at this size (the kernel is PCIe-bound plus single-cell atomic contention); the value is completing the transparent-offload surface for a reduction shape TornadoVM 4.0.1's own PTX backend currently throws `TornadoInternalError: unimplemented` on (`bench-tornado/TornadoDotBench.java`).
- Offload eligibility and lowering for `ldc`/`ldc_w`/`ldc2_w` constant-pool loads (int constants outside `sipush` range, and any float/double/long literal) — previously any such constant killed eligibility for the whole method. Measured: `bench-gpu/GpuLdcBench.java` (96-step multiply-add chain, N = 2²⁴) warm 8 ms on GPU vs ~2,000 ms CPU-bound before, sample bit-exact vs HotSpot.
- A curated `Math`/`StrictMath` GPU-intrinsics table under the existing `ALLOW_INTRINSIC_CALLS` admission hint — `sqrt`(double), `abs`/`min`/`max`(int/long/float/double, NaN- and signed-zero-correct per Java's contract), `fma`(float/double) — replacing the previous analyzer hole where any `invokestatic` was admitted but the emitter had no lowering for any of them, so every such method silently blacklisted itself to the CPU. `sin`/`cos`/`exp`/`log`/`pow` are deliberately excluded: PTX only offers `.approx` transcendentals, which would silently violate Java's `Math`/`StrictMath` precision contract. See `docs/gpu/annotations.md`.
- `frem`/`drem` (IEEE remainder) lowering, gated behind the existing `ALLOW_DIV_BY_ZERO` admission hint (reused rather than adding a new hint for one opcode pair); exact only for bounded quotients.
- `lcmp`/`fcmpl`/`fcmpg`/`dcmpl`/`dcmpg` value-form lowering (bit-exact `setp`/`selp` sequences, correct NaN-result asymmetry between the `l`/`g` variants). A compare that feeds a branch still rejects at the branch opcode, so no new false eligibility was introduced.
- Non-zero-start counted loops (`i = K; i < bound; i++` with `K >= 0`, `K` sourced from an `ldc`).
- A JIT-caller admission gate (`vm/src/runtime/offload_jit_gate.rs`) that denies JIT/OSR compilation of any caller method containing an offload-eligible call site while `--gpu` is active, wired into all 5 JIT/OSR admission checks in the interpreter — closes the "a hot caller's OSR silently degrades offload back to CPU" structural gap. Hardware-validated: 100 hot repetitions of a caller loop at N = 2²² hold steady at 2 ms warm per call.
- `dispatch_async` launch-configuration fixes: the thread-count floor no longer clamps every launch to a minimum of 2²⁰ threads (the real per-call array length wins when known), and block size now comes from `cuOccupancyMaxPotentialBlockSize` instead of a fixed 256-thread block.
- Async API surface: a non-blocking `poll_submission_status` now backs `Native.futureIsDone`/`futureStatus` (a real device probe via a best-effort `cuLaunchHostFunc` host callback, falling back to non-blocking `Event::query`/`cuEventQuery`), finalizing a submission inline the moment the device reports done — `isDone()` returning `true` now means the submission is really finalized, not just "probably." `Native.futureGetResult` now surfaces real scalar reduction results (boxed as `Integer`/`Long`/`Float`/`Double`) from the real submission registry instead of only the pre-Phase-6 synthetic stub map. `GpuExecutor`'s default CUDA stream is now real and shared across an executor's submissions instead of a fresh private stream per dispatch (`resolve_or_create_default_stream`); `newStream()` also now mints a genuine CUDA stream, though nothing yet routes a dispatch onto it explicitly. `GpuArray.allocate`'s Rust-side native shims (`arrayAllocateInt/Long/Float/Double`) landed; the `craton-gpu-java` jar binding is still pending. `i8`/`i16` bulk array marshalling. `--print-gpu-decisions` is now self-sufficient — it no longer requires a separate `RUST_LOG=info` to see any output.
- `.github/workflows/gpu-selfhosted.yml` + `bench-gpu/ci-gate.sh` — weekly self-hosted-GPU-runner CI scaffolding for the `bench-gpu/` benchmark suite (checksum-verified); runner enrollment against the workflow's runner label is still pending.

#### Known follow-ups
- `GpuFuture` completion is now poll-driven but still not push-driven: `isDone()`/`getNow()` do a real non-blocking device check and finalize inline, but nothing drives that check without an application thread calling it — no background thread or driver callback completes a future on its own yet.
- 2-D/nested loops and general (non-loop-guard) branches are still rejected by the analyzer; `)F`/`)D` reductions remain CPU-only by design.
- Full open-items list in `docs/known-issues/gpu-offload-followups-20260711.md`.

---

### 2026-06 multi-agent review remediation

A second, larger review-driven remediation pass (one Opus agent per finding, merged in
severity order with a build gate) closed the full critical/high/medium tier plus perf and
features. Highlights:

#### Security
- `SecureRandom` now draws from the OS CSPRNG (`BCryptGenRandom`/`getrandom`) instead of an invertible splitmix64 DRBG (`native-builtins/src/crypto_impl.rs`); RSA private-key ops gained base blinding.
- SSRF: the always-on cloud-metadata/link-local block now unwraps IPv4-mapped/compatible IPv6 (`::ffff:169.254.169.254`) (`native-io/src/outbound_policy.rs`); optional outbound-hostname DNS resolution closes the alias/rebind bypass.
- Built-in HTTP server honors `Transfer-Encoding: chunked` (request-smuggling/body-desync fix); HTTP client strips `Authorization`/`Cookie` on cross-host redirects (`native-builtins/src/{net_phase_e,http_client}.rs`).
- `X509Certificate.verify` fails closed; `Class.forName` rejects control-byte/separator/`..` injection; AOT cache integrity moved to SHA-256.
- New sandbox/egress knobs documented in `docs/SECURITY_HARDENING.md`.

#### Soundness (GC / JIT / memory safety)
- Closed the JIT/GC "register-resident root" use-after-free family: a uniform native-root registry (`vm/src/memory/native_roots.rs`) + per-subsystem scan/remap for native collections overlays, NIO selector keys, ScheduledThreadPoolExecutor runnables, XNIO IoFutures, the ClassFileTransformer chain, the `ObjectStreamClass` cache, and value-stack smuggled jobjects; JIT x64 now spills callee-saved operand-stack oops at safepoints.
- JNI: implicit local-reference frame around native calls + a refcounted GC pin set for `GetPrimitiveArrayCritical`/`Get*ArrayElements` (`gc/src/pinned.rs`).
- CompactHeader forwarding pointers no longer truncate above 4 GB; per-thread SATB buffers are drained at remark; concurrent-mark 16-byte slot reads are stripe-locked; ZGC backend runs reference processing.
- `vm-exec` JNI TLS cleanup is RAII (panic-safe); `<clinit>` failure no longer leaks the init claim; libcratonvm hands out validated opaque handles instead of raw heap pointers.

#### Correctness
- Bytecode verifier rejects unverified `jsr`/`ret` by default; `ldc`/`invokespecial` verifier-model fixes.
- `BigInteger.modPow`/`modInverse` honor signs and throw on non-invertible input; `AtomicXFieldUpdater` RMW ops no longer lose updates; `AbstractStringBuilder.getChars` bounds-checks; interpreter runs `finally`/catch-all on JIT-unknown-PC unwind.
- JNI `DefineClass` defines from the supplied buffer; `Call*MethodV`/`Call*MethodA` implemented.

#### Performance
- Thread-local scratch buffer for socket read/write (no per-syscall `Vec`); O(1) maps for JNI global refs, unified-logging handles, and the regex cache; bounded JIT code-cache + deopt history; metaspace bump fast-path.

#### Features
- Advisory `cargo-llvm-cov` coverage workflow (`.github/workflows/coverage.yml`, `docs/COVERAGE.md`).
- Container/cgroup-aware default heap sizing (`vm/src/runtime/container.rs`, `docs/CONTAINER.md`).
- `README.md` for `libcratonvm` and `cratonvm-embed` (crates.io pages); embedding guide (`docs/EMBEDDING.md`).
- Five L/XL design docs under `docs/feature-designs/` (precise-JIT-maps-default, deopt/OSR, concurrent-GC maturation, foreign-thread attach, differential fuzzer).

#### Build / OSS
- MSRV raised `1.77` → `1.80` (`Cargo.toml`, `clippy.toml`) to match the std APIs the code already uses; `gc`/`reader`/`craton-gpu` clippy cleaned.
- Untracked the gitignored `bench/` build artifacts and stray `dd1.out` (kept on disk); test-fixture `.class` files retained.
- Crate-count references corrected to **20** workspace members (`libcratonvm`, `cratonvm-embed`, and `cratonvm-difftest` present; `fuzz/` remains standalone); `docs/CRYPTO_STATUS.md` reclassified PBKDF2/ML-KEM/DESede as implemented.

---

### Earlier review round

A cross-crate review-driven fix orchestrator landed 50+ commits across security, soundness, correctness, and OSS-distribution hygiene. Highlights:

#### Security
- JEP-290 `ObjectInputFilter` honored with `maxdepth`/`maxrefs`/`maxbytes`/`maxarray` caps (`native-builtins/src/object_input_filter.rs`).
- JAR signer chain verified against the JCE/JDK trust store before classes load (`classloading/src/jar_signer.rs`).
- Panama / FFI host calls gated behind `--enable-native-access`; unauthorized callers throw `IllegalCallerException` (`native-builtins/src/panama_*.rs`).
- `ProcessBuilder.start` and Panama host calls now consult `SecurityManager.checkExec` (`native-builtins/src/process.rs`).
- Test-only TLS certs and keys moved behind `cfg(test)` so they cannot ship in release artifacts (`native-builtins/src/tls_test_certs.rs`).
- Outbound network calls (HTTP/Socket/URL) run through an SSRF policy hook with a per-connect timeout (`native-io/src/net.rs`).
- `RandomAccessFile`, `WatchService`, and `ProcessBuilder` now route paths through `validate_path` before opening (`native-io/src/*`, `native-builtins/src/process.rs`).
- New libfuzzer targets cover classfile reader, JImage parser, PKCS#12 keystore, and JAR signer (`fuzz/fuzz_targets/`).
- `vm` identity-validates resolution-cache keys so a forged class identity cannot poison lookups (`vm/src/runtime/resolution_cache.rs`).
- `vm` verifier-skip path is now gated on the bootstrap classloader identity, not just the loader pointer (`vm/src/runtime/verifier_gate.rs`).

#### Soundness
- SATB pre-barrier wired at remaining `aastore`/`putfield` sites plus a real stop-the-world for `newarray` (`vm/src/runtime/interpreter.rs`, `jit/src/runtime_helpers.rs`).
- `gc` mutating heap entry points now require a `StopTheWorldToken` witness (`gc/src/lib.rs`).
- Async-signal-safe SIGSEGV handler installed on Unix (no allocations, no locks) (`vm/src/runtime/signals.rs`).
- AArch64 icache flush on Linux and FreeBSD after JIT code emission (`jit/src/aarch64.rs`).
- `vm` hot locks reordered through `OrderedMutex` matching `docs/lock-order.md` (`vm/src/lock_order.rs`).
- JIT switch-target offsets are now overflow-checked; `try_patch` replaces panicking `patch_i32`/`patch_byte` (`jit/src/buffer.rs`).
- `reader::ByteView::try_new` returns `Result` on overflow / misalignment instead of UB (`reader/src/byte_view.rs`).
- `gc` bitmap clears use `AcqRel` ordering; the `SATB` write barrier is now part of the trait surface (`gc/src/g1.rs`).
- `jfr::SpscEventRing::Drop` performs a bounded shutdown and releases pending payloads (`jfr/src/ring.rs`).

#### Correctness
- JFR field emit validates variant against declared type per event (`jfr/src/event.rs`).
- Native collections rekey GC overlays on `identity_hash_code` so post-GC pointer remap keeps maps consistent (`native-collections/src/*`).
- Blocking queue park / notify discipline cleaned up with read-locks instead of unsynchronized shared state (`native-collections/src/blocking_queue.rs`).
- CUDA H2D → kernel → D2H now sequenced on the same stream; previous code raced (`cuda-bridge/src/stream.rs`).
- `jit-api` exposes a `validate()` loop, `repr(C)` golden offsets, and a fixed `NUM_FIELDS` constant for ABI lock-in (`jit-api/src/lib.rs`).
- `types::CompactValue::update_object_ptr` returns `Result`, and `as_long_unchecked` documents its lazy-decode invariant (`types/src/compact_value.rs`).
- `native-builtins` `--enable-native-access` audit; Panama host calls check the caller module against the allow-list.
- `reader` attribute shape validation propagates the signature depth-guard "sticky" flag (`reader/src/attribute.rs`).
- `native-builtins` JCA crypto routes AES / AES-GCM through `aes` / `aes-gcm` RustCrypto (constant-time).
- `vm-cli` rebuilt for HotSpot `-Xmx` / `-XX` parsing, `--nojit`, `String[] args` (`vm-cli/src/main.rs`).
- `native-api::allocate` no longer leaks on the error path; `init_level` is monotonic; `tcp_available` no longer clobbers state (`native-api/src/lib.rs`).

#### OSS / Distribution
- `vm-cli` produces the `cratonvm` binary by default; the `java[.exe]` alias is opt-in via `--features java-bin-alias` so `cargo install` does not shadow a real JDK (`vm-cli/Cargo.toml`).
- Added `SUPPORT.md`, `GOVERNANCE.md`, `MAINTAINERS.md`, `THIRD-PARTY-NOTICES.md`, and a GitHub issue-template config (top-level + `.github/`).
- SPDX `Apache-2.0` headers on every Rust source file across the workspace.
- MSRV bumped to 1.77 and synchronized across `README.md`, `BUILD_GUIDE.md`, `CONTRIBUTING.md`, and `docs/INSTALL.md`.
- Workspace version raised to `0.3.0`; every inter-crate `path = "../<crate>"` declaration now carries `version = "0.3.0"` so `cargo publish --dry-run` accepts the manifest.
- Per-crate `README.md` added for crates.io rendering across the then-current publishable crates and tooling crates.
- `fuzz/` has its own standalone nightly-only workspace and remains `publish = false`.
- Workspace crate-count references were aligned in `README.md`, `ARCHITECTURE.md`, and `BUILD_GUIDE.md`; later workspace additions bring the current count to 20.
- CI parked workflows reactivated with `clippy -D warnings` as a hard gate (`.github/workflows/ci.yml`).

#### Known follow-ups
- Re-enable JIT loop unrolling — previous byte-copy unrolling produced corrupt native code and was disabled (`jit/src/x64/unroll.rs`).
- Real-JDK boot via `java.base` JMOD remains opt-in; synthetic stubs cover the default path.
- Concurrent GC marking is still serialized under STW; G1 / ZGC remain experimental.

## [0.3.0] - 2026-05-24

### Added
- Real cryptographic signature verification in `x509_manager::validate_chain` for RSA-SHA256 (PKCS#1 v1.5) and ECDSA-with-SHA256 over P-256, replacing the previous structural-only "signature present" check. DSA-with-SHA1, RSA-PSS, and Ed25519 now report `TrustError::NotImplemented { oid }` so callers can choose to delegate to JCE.
- JIT XMM register allocation for float/double locals (callee-saved XMM8-XMM15 on Windows x64), eliminating frame spills for FP-heavy methods.
- JIT `Math.sqrt` intrinsic inlined as `SQRTSD` instead of going through interpreter dispatch.
- JIT `dup2` opcode support, enabling compound array assignments like `a[i] += x`.
- JIT `ldc2_w` opcode support for loading long/double constants from the constant pool.
- JIT OSR trampoline now transfers float/double locals into their assigned XMM registers.
- JIT `getstatic` caching: unique static field values are loaded once in the method prologue and cached in frame slots.
- JIT `StackSlot::Xmm` operand-stack variant so consecutive double operations chain in XMM registers without memory traffic.
- Extracted 10 crates from the monolithic vm: classloading, gc, jit, jit-api, types, native-api, native-builtins, native-collections, native-io, jfr.
- G1 and ZGC garbage collectors.
- AArch64 JIT backend (partial; 45% of x86-64 opcode coverage).
- Java Flight Recorder support.
- JVMTI event framework.
- Security hardening: checked arithmetic throughout GC and JIT.

### Changed
- MSRV bumped to 1.77 (was 1.75).
- Updated benchmark numbers against JDK 25.0.1 C2: QuickBench 1.50x, Fannkuch 1.57x, N-Body 20x (down from 464x interpreter-only).
- Added Binary Trees (CLBG) benchmark, exposing a GC allocation bottleneck (23.3x ratio).
- N-Body and Fannkuch-Redux benchmarks now run to completion with correct results.
- Rewrote roadmap with an honest production-readiness evaluation distinguishing real working features from Rust-side stubs.
- New tiered priority matrix (Tier 0 basic correctness through Tier 3 production grade) with measurable success metrics verified against real Java code.

### Fixed
- VM-generated exceptions (NPE, AIOOBE, ArithmeticException, ClassCastException, etc.) are now catchable by Java `try/catch` instead of being Rust-side errors that bypassed exception handling.
- `HashMap.entrySet()` iteration: synthetic inner-class types like `HashMap$Entry` now satisfy `checkcast`/`instanceof` against `Map.Entry`, `Iterator`, `Iterable`, `Collection`, and `Comparable`.
- `Thread(Runnable)` and `Thread(String)` constructors are now registered; `thread.start()` works as an alias for `start0()`.
- `Class.getName()` and `Class.getSimpleName()` are now registered.
- `java.io.FileWriter` constructors and write methods are registered, including append mode and `File`-path overloads.
- JIT-compiled methods returning `boolean`/`byte`/`char`/`short`/`float`/`double` now return the correct value instead of being treated as `void`.
- JIT call dispatch now preserves `float` and `double` argument bit patterns (previously collapsed to 0).
- JIT invoke dispatch now installs the thread context before executing compiled code, fixing `invokevirtual`/`invokeinterface` returning 0.
- `Stream.filter(...).count()` and `stream().filter(...).collect(...)` now return correct results (previously returned 0 or stack-overflowed).
- JIT register allocator rewritten to use instruction-level liveness, fixing Fannkuch miscompilations where two locals shared a register.
- JIT operand-stack canonicalization at forward-branch targets and dead-to-live transitions, fixing miscompilation on complex control flow.
- JIT `ifeq..ifle` now uses `TEST` instead of `CMP reg,reg`, correctly setting flags.
- JIT `if_icmpXX` codegen optimized to use direct register comparison.
- N-Body segfault root-caused to loop unrolling producing corrupt native code; N-Body now runs cleanly with unrolling disabled.
- JIT loop unrolling re-enabled behind a byte-copy-safety predicate. The byte-copy unroller is only correct when every opcode in the body is position-independent (or one of the rel32 patch flavours the duplicator now handles, namely `forward_patches`, `bounds_check_stubs`, and `null_check_store_stubs`). Bodies containing field/static accesses, invokes, allocations, throws, instanceof/checkcast, monitor ops, switches, or any other helper-call opcode are skipped. Set `CRATONVM_UNROLL_UNSAFE_BODIES=1` to re-enter the legacy unguarded path for bisection.
- Integer truncation in array allocation (security).
- Unchecked branch offsets in JIT (security).
- Path traversal in resource loading (security).
- StringBuilder `insert()` O(n^2) performance regression.
- Bytecode verifier now accepts `InterfaceMethodref` for `invokestatic`/`invokespecial` (Java 8+ static interface methods).
- `SSLEngine` handshake state machine: `wrap`/`unwrap`/`beginHandshake` transitions.
- Crypto `deriveKey`/`deriveData` now call the HKDF implementation instead of returning empty output.
- File descriptor leak in `fd_table`: rollback on overflow, `close()` returns `Result`.
- Serialization write methods now throw `UnsupportedOperationException` instead of silently succeeding.
- JIT negative cache: failed compilations are no longer re-attempted on every invocation.
- `vm-cli` args-array error handling uses `map_err` instead of `with_context` on non-`Error` types.

### Performance
- GC `alloc_array` no longer double-zeroes the data region; the redundant memset after young-gen allocation is removed.
- GC young-gen mutex is released before the zero-init memset, so large-allocation latency no longer holds the global allocation lock.
- N-Body FP arithmetic improved from 464x to 20x vs JDK 25 C2 via XMM stack slots, `Math.sqrt` intrinsic, and OSR XMM transfer.

### Known Issues
- JIT loop unrolling is now gated on a byte-copy safety predicate (above); pure-arithmetic and array-index-store kernels are unrolled, but loops with field accesses or invokes still execute unrolled-by-1 until the duplicator learns to clone deopt/exception/MIC/PIC stubs.
- BigDecimal/BigInteger arithmetic on post-clinit-populated statics returns 0 (`BigDecimal.ONE.add(BigDecimal.TEN)` yields 0). Boot paths that only reference these values work; numeric workloads (JDBC numeric, Jackson numeric) do not.
- `ForkJoinPool.invoke(RecursiveTask)` at recursion depth >= 10 returns 0 due to a JIT register clobber in deeply-recursive boxed-`Long` arithmetic. Workaround: disable the JIT for affected workloads.
- GC throughput is roughly 23x slower than JDK on allocation-heavy workloads (Binary Trees).

## [0.2.0] - 2025-06-01

### Added
- x86-64 JIT compiler with 26 optimization rounds (~140 bytecodes compiled)
  - AVX2 SIMD vectorization for integer reduction loops
  - On-Stack Replacement (OSR) at hot loop back-edges
  - Loop-Invariant Code Motion (LICM)
  - Array Bounds Check Elimination (BCE)
  - Magic number division (no IDIV)
  - SSE float/double arithmetic pipeline
  - SoA (Structure-of-Arrays) value layout for 44% memory reduction
- Generational garbage collector with write barriers and card table
- Multi-threading with monitors, ReentrantLock, CountDownLatch, Semaphore, CyclicBarrier
- Virtual threads (simplified carrier-based scheduler)
- Java 11 support: nest-based access control (JEP 181)
- Java 17 support: records (JEP 395), sealed classes (JEP 409)
- Java 21 support: pattern matching for switch, sequenced collections
- Java 25 support: stream gatherers, scoped values, structured concurrency
- Panama FFI: MemorySegment, Arena, ValueLayout, SymbolLookup, Linker (downcall/upcall)
- 3,100+ native method registrations across java.lang, java.util, java.io, java.time, java.nio
- Full reflection: Class.forName, Method.invoke, Field.get/set, Constructor.newInstance
- Lambda/invokedynamic via LambdaMetafactory and StringConcatFactory
- CONSTANT_Dynamic (condy) support
- Enhanced NPE messages (JEP 358)
- Hidden classes (JEP 371)
- Partial JNI function table (229 slots, 13 implemented)
- Module system basics: Module, ModuleDescriptor, ModuleLayer
- Class file versions 45-69 (Java 1.1 through Java 25)
- Dependabot for automated dependency updates
- CODEOWNERS for review routing
- GitHub Security Advisories for private vulnerability reporting
- ARCHITECTURE.md for contributor onboarding
- Release workflow for automated binary builds

### Changed
- Improved SAFETY documentation on unsafe blocks in heap allocator
- Added checked allocation methods (`alloc_object_checked`, `alloc_array_checked`)
- Replaced test `panic!()` calls with proper `assert!` macros in GC and JIT tests
- Updated test documentation references across the public docs.

### Performance
- Within 1.41x of JDK 25 C2 on QuickBench overall
- Fibonacci(42): 1.07x — within 7% of C2

## [0.1.0] - 2025-01-15

### Added
- Bytecode interpreter with 200+ JVM instructions
- `.class` file parser supporting all standard attributes
- Command-line launcher with classpath and heap size configuration
- CI pipeline with cross-platform testing (coverage and Miri jobs scaffolded but planned, not yet enabled)

[Unreleased]: https://github.com/craton-co/cratonvm/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/craton-co/cratonvm/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/craton-co/cratonvm/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/craton-co/cratonvm/releases/tag/v0.1.0
