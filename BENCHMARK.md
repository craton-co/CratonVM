# CratonVM Benchmarks

Methodology, full notes, and provenance for the numbers in
[README.md](README.md). The headline rule for every table we publish:
**checksums must match HotSpot on every run** — a fast wrong answer is a
bug, not a result.

## CPU benchmarks

### Harness

> **Note (2026-07-25): the harness and perf gate were missing from `dev` and
> have been restored.** Both were committed on
> `origin/arch/integration-20260723` — `bench/CratonBench.java` in `5f9bc7bcb`,
> the gate in `ece3729b1` with its re-anchor in `42019012b` — but that branch was
> never merged, so for a period this file documented tooling that no reader could
> actually run. The three commits have now been cherry-picked onto
> `arch/tiers-1-3-20260725`, so the commands below work again.
>
> Worth knowing for next time: `git log --all -- bench/CratonBench.java` finds
> the file even when `git log -- bench/CratonBench.java` on your branch does not.
> "Not in the tree" is not the same as "never existed" — check `--all` and
> `git branch -a --contains <sha>` before concluding anything was lost.

All seven CPU rows come from one unified harness,
[`bench/CratonBench.java`](bench/CratonBench.java): arithmetic, fib, sieve,
matrix, hashmap, stringregex, and bintrees, each runnable in-process or as an
isolated phase (`CratonBench <phase>`). Each phase prints its time **and its
checksum**.

**Also in `bench/`**, the per-kernel harnesses, each likewise printing time
**and checksum** — useful when you want one row without the others, and what to
fall back on if the unified harness goes missing again:

| File | Row it covers | Checksum |
|------|---------------|----------|
| `BinTreesClassic.java` | Binary Trees (takes depth as argv, e.g. `18`) | `68332206` at d=18 |
| `HashMapOnly.java` | HashMap put/get | `15499991500000` at n=1,000,000 |
| `StringRegexOnly.java` | String/Regex | (printed) |
| `QuickBenchLong2.java` | arithmetic / fib / sieve / matrix kernels | (printed) |
| `BinT.java`, `IntrinsicBench.java`, `HmSemanticsProbe.java` | targeted probes | (printed) |

Whichever harness you use, compare **before/after on one host with one binary
pair**. That is the methodologically sound comparison regardless of harness, and
on this shared box it is often the only trustworthy one.

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

### Current table (all seven rows re-measured 2026-08-04)

`dev` @ `12b8cbdea`, Azure EPYC bench host, JDK 25.0.3 both sides, `-Xmx8g`
both sides, one phase per fresh process pinned to cpu 13, arms **alternated
with the order flipped on alternate pairs**, 9 pairs per phase, no sample
discarded, checksum verified against HotSpot on every single run (zero
mismatches across all 126 runs). 1-minute load average 2.3–4.1 throughout —
above the gate's own 2.0 ceiling, so the ratios are the durable content and
the absolutes are this host on this day.

| Benchmark                         | JDK 25 C2 | CratonVM  | Ratio     | CV (CratonVM) | was (2026-07) |
|-----------------------------------|-----------|-----------|-----------|---------------|---------------|
| Arithmetic (2B ops)               | 1,860 ms  | 3,641 ms  | 1.96x     | 0.4% | 2.44x |
| Fibonacci(44)                     | 1,493 ms  | 8,612 ms  | 5.77x     | 1.8% | 2.79x |
| Sieve (100K × 20,000)             | 2,412 ms  | 15,680 ms | 6.50x     | 0.4% | 2.28x |
| Matrix 1280×1280                  | 2,124 ms  | 2,111 ms  | **0.99x** | 0.3% | 2.93x |
| HashMap (10M put/get, isolated)   | 995 ms    | 2,060 ms  | 2.07x     | 0.4% | **1.75x** |
| String/Regex (100K, isolated)     | 51 ms     | 285 ms    | 5.59x     | 1.1% | **7.7x** |
| Binary Trees (depth 18, isolated) | 177 ms    | 1,690 ms  | 9.55x     | 7.6% | 8.34x |

This replaces the 2026-07-18 table, whose rows were taken across four separate
sessions on a host that has since been re-provisioned, and three of which this
document already flagged as unverified. Every row above comes from **one**
interleaved series, so the rows are comparable to each other for the first
time.

**Sieve is a live regression that landed 2026-08-03 and is not the steady
state.** The phase measured **2,350 ms** — faster than HotSpot — on the
immediately preceding build. One method is the whole difference:
`CratonBench.sieve([ZI)I` is admitted to the optimizing (C2/IR) pipeline on
both builds, but only the newer one *produces a body* for it; before, it fell
through to the single-pass backend. The IR body is **6.4x slower than the C1
body it replaced**:

| build | sieve, 5 interleaved pairs | C2 reach for the phase |
|---|---|---|
| before (`cov-02` base) | 2,462 / 2,487 / 2,324 / 2,461 / 2,474 ms | 2 requests, 1 admitted, **0 bodies** |
| after (`cov-02` arrays) | 15,949 / 15,805 / 15,662 / 15,922 / 15,823 ms | 2 requests, 1 admitted, **1 body** |

Checksums are identical on both, so this is throughput and not correctness. It
is the case `docs/known-issues/c2/ir-coverage-survey-20260803.md` warns about
in as many words — *"it does not say lowering these opcodes makes anything
faster"* — arriving on the day the arms landed. Read the per-phase reach in any
gate run's `manifest.tsv` (`ir_reach_<phase>`) before attributing a
CratonBench delta to the optimizing tier; that record is what made this a
one-step diagnosis.

**Fibonacci is NOT a `cov-*` regression.** Interleaved against the same
pre-`cov-02` control it measured 8,393–8,572 ms against the merged tree's
8,402–8,533 ms — identical. Its distance from the July 4,790 ms figure predates
all of this work and is unattributed.

Row notes (historical, from the table this replaced):

- **RETRACTED 2026-07-30: the HashMap regression this table used to record
  (`22,077 ms`, `21.2x`, "CONFIRMED and bounded to `a36b9d121..e57f0bc7d`")
  does not reproduce, and the bisect it recommended has nothing in it.**
  Measured before any change, `dev` @ `9ac1feffe`, isolated fresh processes,
  `-Xmx8g`, alternating arms pinned to cpu 15 with `mpstat -P ALL` confirming
  that core was 90-100% the measuring process:

  | n | HotSpot | CratonVM `dev` | ratio | what this table said |
  |---|---:|---:|---:|---|
  | 1,000,000 | 66 ms | 427 ms | 6.5x | 3,084 ms / 42.7x |
  | 10,000,000 | 997 ms | 3,523 ms | 3.53x | 22,077 ms / 21.2x |

  427 ms at n=1M is within noise of the **434 ms** this same note records for
  the *healthy* `a36b9d121` (07-18) binary, so there is no 7.1x delta left to
  bisect for. **Why the 07-25 readings were ~5x too slow is not established.**
  The obvious candidate — the cpu-13 collision warned about further down this
  file — was tested directly (same binary, same phase, alternating cpu 13 and
  cpu 15) and showed **no difference**: 4131/4004 on cpu 13 against 4063/4042
  on cpu 15. Treat any 07-25-era absolute in this document as unverified until
  re-measured. Full detail:
  [`hashmap-half-gap-20260730.md`](docs/internal/performance/hashmap-half-gap-20260730.md).

  String/Regex's 07-25 row is left as recorded — it has not been re-measured
  and it was the *smaller* of the two claims (3.57x → 7.7x), but it came out of
  the same session, so it deserves the same scepticism.
- **HashMap's 1.75x is that corrected baseline plus a real 72.3% gap
  reduction.** From 3,776 ms to 1,780 ms against HotSpot's 1,017 ms, medians of
  five interleaved fresh-process runs at load 3.4-3.8, every checksum
  `1549999915000000`; the repository's own gate independently reports `PASS
  median 1767ms`. A second series at load 10-12 reads 4,065 / 1,870 / 1,062,
  i.e. 73.1% — the reduction is stable across load levels. Two
  causes: the Integer-keyed dense overlay was maintaining a second full
  `FxHashMap` purely to remember insertion order it could derive from the key
  (19.7% of the phase in `note_fresh_insert` alone), and a set of per-object
  fixed costs — `get_header`, the `ObjectRef` provenance bitmap, the
  single-OS-thread tripwire, `a2dbg::record`, and re-deriving a monomorphic
  `checkcast`'s answer — that were each an out-of-line call around a no-op or
  an idempotent update. The host was at load 10-12, so both columns are
  inflated; the ratio is the durable content.
- **Fibonacci** is recursion-bound and its recursive self-call compiles to a
  guarded direct call. The 2026-07-30 closeout removed the Linux
  `jit_frame_record` helper from the prologue and both post-recursive-call
  restoration sites, replacing each with one sentinel-probed `fs:` TLS store.
  It also permits a metadata-only empty root map when moving-young analysis
  proves the recursive caller has no live oops. The merged-binary alternating
  acceptance reduced the HotSpot gap by **71.90%**. Full measurements and
  generated-code evidence are in
  [`fibonacci-half-gap-20260730.md`](docs/internal/performance/fibonacci-half-gap-20260730.md).
- **Binary Trees** was measured at `-Xmx8g` as seven alternating
  fresh-process pairs; all fourteen checksums were `68332206`.
  A July 2026 dev regression that temporarily quadrupled this row was
  root-caused (two independent causes) and fixed.
  The perf gate's anchored baseline is 1,550 ms, reflecting a deliberate
  ~2% correctness hardening (explicit header initialization in the inline
  allocator) accepted after that fix.
  A 2026-07-30 follow-up (TLAB zero-elision for the fields that don't need
  it, plus a GC-inert proof for `itemCheck`'s allocation-free self-
  recursion) cut a further **14.5%** off the wall-time in a clean 10-round
  interleaved Azure measurement (1,529.5 ms candidate vs 1,789.5 ms
  `origin/dev`, 177 ms HotSpot — 16.1% HotSpot-gap reduction). An
  accompanying attempt to also cap the initial young semispace at 512 MiB
  measured as a **12-13% regression** instead (it multiplied a pre-existing
  "young GC always falls back to non-moving sweep for this workload" defect
  by forcing ~6x more young collections) and was reverted. Full measurement
  history, isolation methodology, and the root-cause writeup are in
  [`binarytrees-bt18-half-gap-20260730.md`](docs/internal/performance/binarytrees-bt18-half-gap-20260730.md).
- **Sieve** is three counted `boolean[]` loops, and single-pass BCE refuses
  inclusive (`<=`) loops and non-`arr.length` bounds, so every element kept a
  null and bounds check. A 2026-07-30 change added three fall-through-only
  guarded preheaders — a block clear, a strided store, and the whole sieve
  nest, the last of which scans eight bytes at a time for the next unmarked
  index. Two independent 9-round interleaved same-binary A/B measurements
  (`CRATONVM_JIT_BULK_BYTE_LOOPS=0` as the dev-equivalent control, all 54
  runs checksum `9592`) cut the HotSpot gap by **94.35%** and **95.30%**,
  moving the ratio from 1.85x to **1.05x**. Full measurements, the guard
  contract, and the differential probe are in
  [`cratonbench-sieve-half-gap-20260730.md`](docs/internal/performance/cratonbench-sieve-half-gap-20260730.md).

### The performance gate

Perf regressions are guarded by
[`regression-suite/perf/run-cratonbench-gate.sh`](regression-suite/perf/run-cratonbench-gate.sh),
which runs every phase isolated (pinned, `-Xmx8g`, median of 5), verifies the
exact checksum on every run, enforces a 5% budget over the per-host anchored
baselines in
[`cratonbench-baseline-azure-epyc.tsv`](regression-suite/perf/cratonbench-baseline-azure-epyc.tsv),
and **refuses to measure on a loaded host** rather than produce noisy verdicts.
Anchored baselines may only be re-anchored with a linked evidence document.

This gate was absent from `dev` until 2026-07-25 (see the Harness note above) —
if you are working on a commit that predates the restore, it will not be there.

Even with the gate available, use the process below for a perf-affecting change
whenever you cannot get a quiet host — the gate deliberately refuses to measure
under load, and the shared Azure box frequently runs at load 5–8 with several
sessions building:

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

## GPU benchmarks

### Setup

Measured 2026-07-11 on a GeForce RTX 2060 (sm_75) against HotSpot JDK 25
(C2) and TornadoVM 4.0.1 (PTX backend, `@Parallel`/`@Reduce` + TaskGraph
API). CratonVM offload is **opt-in and automatic within a narrow envelope**:
the kernels below are plain static methods over primitive arrays with no
annotations and no API at the call site, but they require a GPU build and an
explicit flag (`cargo build --features gpu-driver`, run with `--gpu`), and
anything the analyzer doesn't accept stays on the CPU. All timings are warm
and include the full per-call H2D + kernel + D2H round-trip. There is no
self-hosted GPU hardware CI, so these are point-in-time measurements rather
than a continuously enforced budget.

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
  equivalent `@Reduce`-over-`LongArray` kernel; CratonVM's automatic
  `--gpu` path handles it (slowly — a proper tree/shared-memory reduction is
  an open item).
- The multiply-add and dot-product rows are kept as honest counter-cases:
  CPU AVX2 stays competitive on MAD-dominated kernels at every size, and a
  single atomic accumulator doesn't get relatively cheaper with more
  elements.

Full GPU results — more input sizes, `ldc`-constant kernels, cold-start
numbers up to N = 2²⁸, kernel sources, and eligibility rules — are in
[docs/gpu/README.md](docs/gpu/README.md).

## Reproducing

> **⚠ Build with fat LTO for any number you intend to compare against the
> baselines.** `[profile.release]` is `lto = "fat"` + `codegen-units = 1`, and the
> anchored baselines in `cratonbench-baseline-azure-epyc.tsv` were measured that
> way. Building with `CARGO_PROFILE_RELEASE_LTO=off` — the usual workaround for
> the OOM SIGKILL below — produces a **materially slower** binary whose absolute
> times are not comparable to those baselines. Measured 2026-07-25, same commit,
> two binaries differing only in LTO, interleaved, 3 pairs per phase (host load
> ~6-7, so absolutes are inflated and the spread is wide — hashmap ranged
> 23461-31120 *within one arm*):
>
> | phase | LTO=off median | fat-LTO median | fat-LTO effect |
> |---|---|---|---|
> | bintrees | 3019 ms | 2262 ms | **−25%** |
> | hashmap | 25484 ms | 26196 ms | none |
> | sieve | 10262 ms | 9552 ms | ~−7% |
>
> Fat LTO is worth a real amount on bintrees and close to nothing on hashmap and
> sieve. Use it for anything baseline-comparable — but note what it does NOT
> explain, below.
>
> LTO=off is fine for **relative** A/B between two binaries built the same way —
> that is how the layout-registry win was measured — but say so when reporting,
> and never quote an LTO=off absolute against a baseline.

> **⚠ THE 2026-07-25 MEASUREMENTS BELOW ARE UNRELIABLE — READ THIS FIRST
> (added 2026-07-30).** The hashmap arm of this investigation was re-run from
> scratch on 2026-07-30 and **nothing in it reproduces**. Where this block
> reports 22.3 s "under *every* methodology tried" and "nothing reproduces 4237
> ms", the same phase on `dev` @ `9ac1feffe` measures **4,065 ms** (median of 5,
> cpu 15, `-Xmx8g`, interleaved against HotSpot at 1,062 ms), and n=1M measures
> 427 ms against the 3,084 ms recorded here. The 4,300 ms baseline this block
> calls unreproducible is reproduced within 6%.
>
> Since the hashmap arm is wrong by ~5x, **the reasoning that generalised from
> it to the other three rows does not stand either.** Sieve, stringregex and
> bintrees were not re-measured as part of that work and are simply unknown;
> a fresh 5-rep interleaved run of all three on 2026-07-30 put bintrees at
> 1,635 ms (against the 2,023 ms "quiet median" below and its own 1,550 ms
> anchored baseline) and stringregex at 229 ms (against 457 ms below), which is
> consistent with the whole series being inflated rather than with four
> independent row-specific defects.
>
> The cause was NOT identified. The candidate explanation — that the gate's
> default cpu 13 pin collides with other sessions' benchmarks (see the
> methodology warning at the end of this block) — was tested directly on
> 2026-07-30 with the same binary alternating cpu 13 and cpu 15, and showed no
> difference (4131/4004 against 4063/4042). **Do not build on any absolute
> number in this block without re-measuring it.** See
> [`hashmap-half-gap-20260730.md`](docs/internal/performance/hashmap-half-gap-20260730.md).
>
> The original 2026-07-25 text is kept below, unedited, because it documents
> what was believed and how it was argued.
>
> ---
>
> **⚠ OPEN: 4 of the 7 baselines are not reproducible from the commit they were
> anchored at. It is NOT host load and NOT a regression.** Resolved 2026-07-25 on
> the Azure EPYC host; both earlier candidate explanations are refuted by
> measurement.
>
> **Not host load.** The gate was run at its documented defaults (cpu 13, `-Xmx8g`,
> median of 5) on a genuinely quiet host — 1-min load 1.10, every core idle in
> `mpstat`, zero competing benchmarks. The same 4 phases still fail, and the
> run-to-run spread collapses from the 33% seen under load to **under 1%**
> (hashmap: 22289/22312/22240/22317/22439 ms), which is itself the proof the host
> was quiet:
>
> | phase | quiet median | budget | verdict |
> |---|---|---|---|
> | arithmetic | 4067 ms | 4935 | PASS |
> | fib | 4442 ms | 4672 | PASS |
> | matrix | 2859 ms | 5985 | PASS |
> | sieve | 11655 ms | 6090 | **FAIL 1.9x** |
> | hashmap | 22312 ms | 4515 | **FAIL 4.9x** |
> | stringregex | 457 ms | 173 | **FAIL 2.6x** |
> | bintrees | 2023 ms | 1627 | **FAIL 1.24x** |
>
> That arithmetic/fib/matrix *pass* — matrix by 2x — on the very same runs proves
> the core is delivering full throughput. Uniform CPU starvation cannot produce a
> pass/fail split that is stable across load levels.
>
> **Not a regression.** `e57f0bc7d` (the commit the baselines were anchored at)
> was built with fat LTO and run interleaved against `58c9b643c` on an idle core.
> It fails the *same* 4 phases with statistically identical numbers — anchor
> hashmap 26350 vs dev 26075, anchor stringregex 463 vs dev 468. Where the two
> differ, **dev is faster**: bintrees 2101 vs 2895 (−27%), sieve 9029 vs 11597
> (−22%), i.e. the layout-registry/inline-TLAB work is measurably paying off.
> There is no regression to bisect.
>
> **Therefore the baselines themselves are wrong for these 4 rows.** They are
> marked `provisional` and were recorded as *"pair-1 best"* — a best-of, not a
> median — and evidently came from a different measurement series than the gate
> performs. hashmap in particular is ~22.3 s under *every* methodology tried
> (isolated cold, all-phase warm in-process at 23.5 s, `-Xmx8g` and `-Xmx2g`), at
> *both* commits. Nothing reproduces 4237 ms.
>
> **Do not re-anchor these baselines just to make the gate go green** — but note
> the reason has changed: the open question is no longer "is dev slow?" (it is
> not) but "where did 4237/5800/165/1550 come from, and on what?". Re-anchoring
> requires answering that first, under the README's evidence-doc policy. The
> `anchored` bintrees row is the one with a real evidence doc
> (`bt18-inline-tlab-regression-20260724.md`, 1527–1533 ms); at 2023 ms quiet it
> is 1.24x off its own doc and is the most tractable thread to pull.
>
> *(End of the 2026-07-25 text. The hashmap row was re-anchored to 1,800 ms on
> 2026-07-30 with the evidence doc the policy above asks for — see the banner at
> the top of this block for why the "nothing reproduces 4237 ms" premise no
> longer holds. The other three rows are untouched.)*
>
> **Where hashmap's 22 s actually goes** (perf, `-F 199`, quiet core): it is not
> the layout registry that `994a543bf` fixed for bintrees — that symbol does not
> appear. The profile is *entirely interpreter dispatch into the synthetic native
> collections*: `NativeMethodRegistry::find` 8.3%, the synthetic HashMap engine
> (`DenseIntEntries::note_fresh_insert` + `try_hm_int_fast_put`) 9.7%,
> `execute_invokestatic`/`execute_invoke_kind`/`resolve_method_metadata` ~11%, the
> per-call native-vs-bytecode policy checks
> (`synthetic_stub_should_yield_to_real_bytecode` +
> `should_force_registered_native_over_bytecode`) 5.2%, `OrderedPlRwLock::read`
> 2.6%. `hashMapPutGet` is entered *once* with two 10M-iteration loops, so
> invocation-count tier-up can never fire on it.
>
> **~11% of hashmap CPU was pure waste in `getenv` — now FIXED** (−13.1% on
> hashmap, −15.4% on stringregex; see the note below this one).
>
> **Methodology warning for whoever picks this up:** the gate's default pin is
> **cpu 13**, and concurrent sessions on this shared host run their own
> CratonBench pinned to the same cpu 13. Two benchmarks then timeshare one core
> while `mpstat` shows 14 other cores idle — contention far worse than the 1-min
> load average suggests, and `--max-load` does not catch it. Check
> `taskset -cp <pid>` on any competing `CratonBench` and use `--cpu N` on a
> verified-idle core, or wait for `pgrep -f CratonBench` to come back empty.

> **⚠ The gate CANNOT see the parallel young GC — it measures it at its worst
> configuration.** `regression-suite/perf/run-cratonbench-gate.sh` (line 89) pins with `taskset -c $CPU`, and
> the Rust `available_parallelism()` call honours the affinity mask, so
> `young_gc_threads` resolves to **1** on every gated run. Both the parallel
> mark drain and the parallel sweep are therefore disabled for the numbers the
> gate reports. A change that only helps multi-core GC will read as flat, or
> slightly negative from its added bookkeeping, on the gate — and a GC
> regression that only bites multi-core will not be caught at all.
>
> Before concluding a GC change did nothing, re-measure unpinned (or pinned to
> a CPU *set*, e.g. `taskset -c 8-15`) with `CRATONVM_DBG_GCPHASE=1` for the
> phase table. Worked example: the allocator-anchor change below measures
> −13.8% wall on `taskset -c 8-15`, and its headline 241 → 0 ms phase win is
> invisible to a single-CPU gate run.

> **✅ FIXED 2026-07-25: the young-GC mark-oracle walk (241 ms/collection) is
> gone**, replaced by allocator-recorded object-grid anchors (`gc/src/arena.rs`,
> `gc/src/gen_heap.rs`; merged `26e866b82`).
>
> The sweep parallel split points used to be a by-product of a full-arena
> exact-base walk. That walk chased 2 147 483 592 bytes of headers per
> collection. Anchors are now recorded by the allocator as objects are handed
> out — one verified start per 4 KiB bucket — so the grid is known without
> rediscovering it, and the remaining conservative-candidate oracle visits only
> intervals that contain a candidate: **4 718 536 bytes walked, a 455× drop**.
>
> Interleaved A/B, 12 pairs, `taskset -c 8-15`, quiet host: bintrees-18 median
> **1777 → 1531 ms (−13.8%)**, non-overlapping ranges, B won 12/12. Phase table
> (`CRATONVM_DBG_GCPHASE=1`): `mark-oracle-walk` **241 → 0 ms**, young GC total
> **419 → 170 ms**. Single-CPU (parallel sweep disabled): 2033 → 1800 ms wall,
> GC 916 → 393 ms.
>
> **The obvious alternative was measured, not assumed.** Dropping the
> `cand_idx` early-exit so the walk covers the whole arena and the sweep
> parallelises unconditionally is a **no-op on this workload**: the bintrees-18
> 126 candidates already spanned the full 2 GiB, the walk already reached
> `used`, and the sequential sweep tail was already 0 ms. It would have bought
> nothing while leaving the 241 ms in place. Allocator-sourced anchors make the
> grid independent of the candidate set entirely, which subsumes it.
>
> Correctness: 336 checksum-verified runs, 0 mismatches (7 multi-collection
> configs × 4 `CRATONVM_GC_SWEEP_ANCHOR_STRIDE` values × 4 worker counts × 3
> reps, each compared against the baseline binary own answer), plus a new
> 8-thread mixed-size-class stress matching HotSpot 12/12, and all 7 CratonBench
> phase checksums exact.
>
> Known residual: with full-arena anchors, a single mid-arena grid anomaly now
> aborts the *whole* parallel sweep rather than truncating the prefix. Still
> correct (it falls back to the sequential walk) and never observed across 336+
> runs, but it is a sharper failure edge than before.

> **✅ FIXED 2026-07-25: 130 million `getenv` calls per hashmap run.**
> −13.1% on hashmap, −15.4% on stringregex, neutral elsewhere.
>
> **How it was found — and how the first attempt got it wrong.** A sampled
> `perf` profile showed ~11% of the hashmap phase in the `getenv` family. A
> `--call-graph=dwarf` profile attributed it to `invoke_on_class_shared_inner`,
> which pointed at an uncached `CRATONVM_INVOKE_VIRTUAL_ENTRY_TRACE` probe on
> the `invokevirtual` path. **That attribution was wrong** — the dwarf stacks
> only resolved ~15% of samples, and caching that flag produced *no measurable
> change* (hashmap 25444 → 25536 ms, i.e. nothing). Do not trust partial dwarf
> unwinds on this binary; it is built `debug = "line-tables-only"` with no frame
> pointers.
>
> **What actually worked** was an `LD_PRELOAD` shim that intercepts `getenv` and
> tallies calls *by variable name* — no unwinding, no sampling, exact counts.
> Over one hashmap run (10M put/get) it recorded **~130,000,000 calls, ≈13 per
> benchmark iteration**:
>
> | calls | variable |
> |---:|---|
> | 60,008,062 | `CRATONVM_DBG_LOADER_TRACE` |
> | 20,001,011 | `CRATONVM_DBG_MH_STACK` |
> | 20,001,011 | `CRATONVM_DBG_MH_ADAPTER` |
> | 20,001,005 | `CRATONVM_DBG_STACKLESS` |
> | 10,002,013 | `CRATONVM_DBG_H2TRACE` |
>
> All were uncached `std::env::var`/`var_os` probes on the `new` opcode and
> `try_stackless_invoke` paths (~40 call sites, `CRATONVM_DBG_LOADER_TRACE`
> alone had 33). `getenv` takes the process environ lock and linearly scans
> environ, so each one is far from free. Most had the env probe as the **left**
> operand of an `&&` whose right operand is a cheap string compare, so the
> `getenv` ran unconditionally and the cheap test could never short-circuit it.
>
> Fix: cached predicates in `vm/src/runtime/env_cache.rs` (`cached_is_set!` /
> `cached_is_ok!`, the idiom already used for ~80 other flags) plus local
> `OnceLock` helpers in `native-builtins`, which cannot reach the vm crate.
> Total `getenv` calls per hashmap run: **130,000,000 → ~8,000**.
>
> Measured interleaved on a quiet host, same base commit, both arms fat-LTO,
> with `arithmetic` as an unaffected control:
>
> | phase | before | after | delta |
> |---|---|---|---|
> | hashmap | 21961 ms | 19078 ms | **−13.1%** |
> | stringregex | 462 ms | 391 ms | **−15.4%** |
> | bintrees | 1988 ms | 2000 ms | ~0 |
> | arithmetic (control) | 4098 ms | 4078 ms | ~0 |
>
> This does **not** close any gate phase — hashmap is still ~4.2x over a budget
> that nothing reproduces. It is an independent, real win on the interpreter's
> native-invoke path.
>
> **Generalisable lesson:** when a profile says "time is in `getenv`" (or any
> libc leaf), an `LD_PRELOAD` counting shim identifies the culprit by *name* in
> one run and cannot be fooled by missing unwind info. Reach for it before
> trusting a call-graph attribution.

> **bintrees 1.24x: investigated, NOT the bt18 regression.** The `anchored`
> 1550 row derives from `binarytrees-half-gap-20260718.md`'s 1468 ms, and
> `bt18-inline-tlab-regression-20260724.md` verified the fix at 1527–1533 ms.
> Quiet-host measurement is ~1990–2200 ms. All of the bt18 doc's own acceptance
> criteria still hold on current dev, so the regression it describes has **not**
> recurred:
> - **single** young GC cycle under `CRATONVM_DBG_GCPHASE=1` (the doc's
>   "method of record"; the regressed state showed two), checksum `68332206`;
> - `CRATONVM_NO_JIT_INLINE_TLAB_NEW=1` → 5884 ms vs 1988 ms default (**2.96x**),
>   so the inline TLAB `new` fast path is active and carrying its weight;
> - `CRATONVM_NO_JIT_INLINE_PUTFIELD=1` → 4973 ms vs 1988 ms (**2.5x**), so the
>   inline constructor stores are being emitted — this is exactly the "regains a
>   measurable delta" check the doc asks for.
>
> Nor is it a harness-transfer artifact: the CratonBench `bintrees` phase and
> the standalone `bench/BinTreesClassic.java` the 1468 ms number came from are
> **byte-identical kernels**, and measured head-to-head on the same binary they
> agree — 2204 ms vs 2152 ms. The original harness no longer reproduces its own
> recorded number either.
>
> Also refuted: memory fragmentation / transparent huge pages. Despite the host
> showing 96% compaction failure after 3 days uptime, the running VM's heap is
> `AnonHugePages: 1912832 kB` of `Anonymous: 1914180 kB` — **99.93% huge-page
> backed** — so TLB pressure is not the mechanism.
>
> bintrees therefore joins the other three rows: in the documented fixed state,
> with the optimisations verifiably active, measuring ~1.3x its recorded number
> for reasons not yet explained by code, load, harness, heap size, or paging.

```bash
# Build. Fat LTO — required for baseline-comparable numbers.
cargo build --release -p cratonvm-cli

# If the final cratonvm-cli rustc dies with "signal: 9, SIGKILL" that is the OOM
# killer during fat-LTO codegen, NOT a compile error. Lower parallelism first:
cargo build --release -p cratonvm-cli -j2
# Only as a last resort, and only for relative A/B (see the warning above):
CARGO_PROFILE_RELEASE_LTO=off cargo build --release -p cratonvm-cli -j6

# CPU, all seven phases in-process:
javac -d bench-classes bench/CratonBench.java
./target/release/cratonvm -Xmx8g -cp bench-classes CratonBench

# One isolated phase (the per-row methodology):
./target/release/cratonvm -Xmx8g -cp bench-classes CratonBench bintrees

# The gated regression check (Linux bench host, refuses to run under load):
bash regression-suite/perf/run-cratonbench-gate.sh -Exe /abs/path/to/cratonvm

# Per-kernel harnesses, for one row without the others.
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
