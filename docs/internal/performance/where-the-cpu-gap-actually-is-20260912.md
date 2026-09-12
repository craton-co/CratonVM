# Where the CPU gap actually is, measured before touching C2 again

**2026-09-12.** Taken while landing
[`c2-schedule-late-at-equal-depth-20260912.md`](c2-schedule-late-at-equal-depth-20260912.md),
because that lane produced a −29% spill count and 1.000x, and "the optimizing
tier is too slow" deserved a denominator before another C2 lane was sized from a
page rather than from a number.

Windows dev box with unrelated load, Temurin 25.0.3 both sides, `-Xmx8g`,
one process per arm, checksums identical on every row. Read the ratios.

## 1. `CratonBench`, default configuration

| phase | HotSpot | CratonVM | ratio |
|---|---:|---:|---:|
| arithmetic | 3,055 ms | 7,049 ms | 2.3x |
| fib (44) | 3,281 ms | 12,028 ms | 3.7x |
| sieve | 6,437 ms | 6,974 ms | 1.1x |
| **matrix** | 3,910 ms | **3,324 ms** | **0.85x — CratonVM wins** |
| hashmap (10M) | 658 ms | 13,174 ms | **20x** |
| stringregex | 30 ms | 232 ms | 7.7x |
| **bintrees (d=18)** | 549 ms | 19,133 ms | **35x** |

## 2. The two worst rows are the collector — and that is a KNOWN, DELIBERATE default, not a lane

The same binary, the same classes, the same checksum, one flag apart:

| bintrees (d=18) | time | vs HotSpot |
|---|---:|---:|
| HotSpot | 481 ms | 1.0x |
| CratonVM `-XX:+UseG1GC` | **2,127 ms** | **4.4x** |
| CratonVM `-XX:+UseGenerationalGC` | 2,357 ms | 4.9x |
| CratonVM **default** (`ZgcRealHeap`) | **15,086 ms** | **31x** |

**Seven times, on a default.** `hashmap` moves the same way (12.8 s default,
9.7 s G1, 9.0 s generational).

This is not a surprise once stated, and `docs/GC.md` states it: the default
backend is the ZGC *compatibility* implementation, and a default run of it is
"a non-moving whole-heap STW mark-sweep" over one arena. `bintrees` allocates a
very large volume of short-lived nodes, which is precisely the shape a
whole-heap non-generational sweep serves worst and a generational or regional
collector serves best.

**Nothing is changed here, and — corrected the same day — nothing should be.**

The first draft of this section filed the default as the largest available win
and put it at the top of §4's ordering. That was wrong twice over, and both
corrections come from the project's own history rather than from a new
measurement:

* **The published numbers were always taken under G1.** `README.md`'s and
  `BENCHMARK.md`'s `bintrees` row is the 2,127 ms arm, not the 15,086 ms one.
  There was never a 31x figure on record for anyone to be surprised by; the
  surprise was this page measuring a default that the benchmark methodology
  does not use and then reporting the difference as news.
* **The default is chosen for real applications, where G1 is slower and less
  accurate.** A synthetic allocation kernel is exactly the workload that
  flatters a regional evacuating collector, and it is exactly the workload the
  default is *not* tuned for. Ranking a default by `bintrees` optimises the
  benchmark against the applications.

So the durable content of this section is one sentence, and it is a warning
rather than a lane: **`bintrees` and `hashmap` are collector-dominated, so a
default-configuration run of either says nothing about the JIT.** Quote them
under `-XX:+UseG1GC`, as the published table already does, or do not quote them
when the subject is compilation. §4's ordering is renumbered accordingly — the
collector is not on it.

## 3. And the optimizing tier is 4.5% of the framework-shaped workload

`CratonBenchC2` is the workload built *because* `CratonBench` barely reaches the
optimizing tier (MEAS-02: seven compile requests, two bodies).

| | total |
|---|---:|
| HotSpot | 83 ms |
| CratonVM, default (C2 superseding on) | 3,510 ms |
| CratonVM, `CRATONVM_C2_SUPERSEDE=0` | 3,675 ms |

**42x against HotSpot, and the optimizing tier is worth 4.5% of it.** The census
for that run, under `CRATONVM_DBG=jitc`:

```text
[c2-supersede] compiles: lowered=13 (8 ms) fell_through_to_single_pass=1 (1 ms)
[c2-supersede] acceptance: accepted=15 refused_no_evidence=10 refused_by_policy=0
[c2-supersede] refusals by optimizer activity: simplified=0 inert=10
[c2-supersede] ir alloc sites: inline_tlab_bump=0 stub_only=7
```

Three things to read off it, each of which redirects a lane:

* **Compile time is not the problem.** Thirteen bodies in **8 ms**. Any framing
  of "C2 is slow" that means the compiler rather than its output is answered
  here, and it is answered in the negative.
* **C2's output is frequently the same size as C1's, and sometimes bigger** —
  `c1=2566 c2=2566` on one handler, `c1=2004 c2=2443` on another, `c1=7718
  c2=8813` on `runBatch`. Ten of twenty-five candidates are refused as `inert`:
  the optimizer changed nothing, so there was nothing to accept.
* **Every allocation in this tier is `stub_only`** — a CALL where the
  single-pass backend emits an inline TLAB bump. `c2_alloc_upgrade_enabled()` is
  opt-in for that reason and `ir_inline_tlab_enabled` has a defect
  (`RJitMapTierDiff`, 4 runs in 10) keeping the pair shut.

So the honest statement of the tier's position is: **it takes few methods, it
frequently improves none of them, and where it does the win is a few percent.**
That is a coverage-and-optimizer-strength problem, not a codegen-quality one,
and it is a different lane from every `c2-*` page filed in the last week — all
of which optimise the bodies it already produces.

## 4. What this says about ordering

Sized by what the numbers above support, not by what is interesting:

**The collector is not on this list** — see §2 for why it was struck from it.

1. **`fib` at 3.7x** (§1) — recursive invocation. The largest gap on any row
   that is pure compilation: no allocation, no collections, no collector
   involvement, one arithmetic expression and two calls. Whatever it is, it is
   the JIT's.
2. **Optimizer strength / coverage in C2** (§3): ten of twenty-five candidates
   refused `inert`. Until the optimizer changes something on a typical method,
   improving the code it emits for the few it does change is bounded by 4.5%.
3. **The allocation gate** (§3) — unblocking `ir_inline_tlab_enabled`'s defect
   is the prerequisite for C2 taking any method containing a `new`, which is
   most of them.
4. Everything the recent `c2-*` pages are about — per-iteration instruction
   budget, spills, unrolling, phi copies. Real, correct, and worth low single
   digits each on the bodies this tier already emits.

`matrix` at **0.85x** is worth keeping in view while reading all of that: the
tier is not incapable, and on the one row that is pure compute with no
allocation and no collections it beats HotSpot.

## 5. Reproducing

```bash
cargo build --release -p cratonvm-cli
java -Xmx8g -cp bench-classes CratonBench
./target/release/cratonvm -Xmx8g -cp bench-classes CratonBench
./target/release/cratonvm -Xmx8g -XX:+UseG1GC -cp bench-classes CratonBench bintrees

CRATONVM_DBG=jitc ./target/release/cratonvm -Xmx4g -cp bench-classes CratonBenchC2 \
  2>&1 | grep -v osr-refuse | grep c2-supersede
```
