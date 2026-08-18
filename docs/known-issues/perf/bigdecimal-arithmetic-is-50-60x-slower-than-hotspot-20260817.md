# `BigDecimal` arithmetic is slower than HotSpot — three per-call taxes removed 2026-08-18 (31x -> 21x on the benchmark), residual is general native->heap cost

**Status: OPEN (reduced). Profiled 2026-08-18 on `dev` `64c02b7ac`, which
REFUTED the original per-call-overhead hypothesis in the form it was written.
The three contained items that profile named are fixed
(`perf/bigdecimal-native-overhead-20260818`, measured below): the benchmark
moved 31x -> 21x against HotSpot, and the witness class
`LegendreHighPrecisionTest` went from 110-120 s to 86-105 s — from clearly over
the 90 s per-class suite budget to ON it. It scored PASS in the 310-class sweep
and in 4 of 7 timed runs, and still exceeded 90 s in the other 3 under heavier
host load, so call this borderline rather than closed. The remaining ~32x is not
bignum-specific and is tracked on the two pages under "Related".**

Found triaging the Apache Commons Math test suite
(the suite run recorded in retired/commons-math-suite-run-RETIRED-20260818.md): `LegendreHighPrecisionTest` (2 JUnit
methods, computing 60-digit-precision Gauss-Legendre quadrature rules via
`java.math.BigDecimal` Newton-Raphson root-finding) never finishes — still
making genuine forward progress after 90s+, a legitimate bounded ~119-frame
recursion, not a deadlock. **HotSpot runs the identical class in 3.75s.**

## The gap, re-measured on current dev

`probes/BigDecimalBench.java` — the repro is **a file now**. The previous
revision described it inline and called it "trivial to recreate", which is how a
benchmark stops being comparable: the next person retypes it slightly
differently. It also gates on a checksum *before* timing, so a build that is
fast because it is wrong fails instead of posting a good number.

| host | HotSpot 25 | CratonVM | ratio |
|---|---:|---:|---:|
| Windows, 32 core | 212-215 ms | 13,537-19,192 ms | **63-89x** |
| Azure Linux, 8 core | 1,002 ms | ~12,400 ms (50k x4) | **12x** |

Both VMs print the identical checksum, so **CratonVM is correct here, only
slow**. The Linux ratio is smaller because that host's HotSpot is ~5x slower
than the Windows one while CratonVM is about the same on both — worth knowing
before quoting any single number as "the" ratio.

## What the profile actually says

`perf record -F 999 -g` on the isolated benchmark, Azure Linux. This is the step
the previous revision asked for and could not run; the Windows dev box has no
`perf`.

**The profile is flat. The largest single symbol is 5.06%.** There is no
12-15µs-per-call tax sitting in one place, and so there is no single fix.
Grouped:

| group | share | biggest members |
|---|---:|---|
| name / metadata resolution | **~15%** | `resolve_field_index` 5.06, `__memcmp_evex_movbe` 3.31, class-by-name hash search 2.74, `resolve_field_descriptor_byte_cached` 1.67, `get_loaded_class_id` 1.53 |
| heap address validation + allocation | **~15%** | `is_object_address` 4.50, `ZObjectStarts::contains` 3.81, `alloc_raw_tlab` 3.19, `_mi_page_malloc_zero` 1.92, `load_and_forward` 1.07 |
| native dispatch plumbing | ~6% | `try_jit_site_cached_native_dispatch` 1.94, `safe_native_call_impl` 1.88, argument forwarding 1.96 |
| **the bignum arithmetic itself** | **~2%** | `BigInt::to_decimal` 1.29, `bi_read_int` 0.93 |

**Roughly 2% of the time is arithmetic.** The rest is VM plumbing, spread across
three subsystems with no member above ~5%.

## What was refuted, and what survives

The previous revision reasoned that ~1.1M native calls x a 12-15µs fixed
per-call tax would account for the whole runtime, that this "matches the general
pattern of several *already fixed* issues on this exact native-dispatch path",
and that the fix was therefore "very likely in the same family". It flagged
itself **not confirmed**, which was the right call — it does not survive:

* **Native dispatch is ~6%, not the bulk.** A fix in the family of the cited
  dispatch-overhead bugs has a ceiling of a few percent here.
* **There is no per-call tax on natives generally.** Controls, measured on both
  VMs with no lambda in the timed loop:

  | | HotSpot | CratonVM | ratio |
  |---|---:|---:|---:|
  | `Math.abs` | 20.5 ns | 26.6 ns | **1.3x** |
  | `String.length` | 37.1 ns | 48.3 ns | **1.3x** |
  | `BigDecimal.add` | 44.5 ns | 4843 ns | 109x |
  | `BigInteger.add` | 23.9 ns | 1489 ns | 62x |

  Non-bignum natives are within 1.3x. Whatever is expensive is specific to the
  bignum surface, not to crossing into native code.

What survives is the coarse claim that this is overhead and not complexity: at
~200 bits both VMs use schoolbook arithmetic, and the arithmetic is 2% of the
profile. The page was right that the cost is not algorithmic. It was wrong about
where the overhead lives, and wrong that it is one thing.

## Two hypotheses tested and killed along the way

Recorded so nobody re-runs them.

1. **"The `MathContext` overloads have no natives, so they run JDK bytecode."**
   The premise is true — `math_bignum.rs` has **zero** registrations taking a
   `MathContext`, so `add(BigDecimal, MathContext)` and friends do run the JDK's
   own bytecode. It is still not the explanation: the MC overload costs ~2x the
   plain one on *both* VMs (`add`: 19.3→37.4 ns HotSpot, 1927→3991 ns CratonVM),
   so the CratonVM/HotSpot ratio is ~100x either way. The plain, fully-native
   `add(BigDecimal)` is already 100x slower. Registering MC overloads would not
   address this.
2. **A first per-op breakdown that put a `Runnable` in every timed loop.** It
   showed ~1.1µs/op *controls* and looked like a universal per-call tax. That
   was the harness: one SAM dispatch per iteration, on a tree with an open
   `lambda-sam-dispatch-bypasses-the-cached-invoke-path` issue. Rewritten with
   plain monomorphic loops the controls drop to 1.3x. **A microbenchmark that
   dials through a lambda is measuring the lambda.**

## What was fixed, 2026-08-18, and what it bought

Items 1, 2 and 4 of the profile list this page used to carry, implemented on
`perf/bigdecimal-native-overhead-20260818`. Measured on Azure Linux (8 core),
two release binaries from the same `dev` base, arms INTERLEAVED, six rounds:

| | dev base | fixed | HotSpot 25 | change |
|---|---:|---:|---:|---:|
| `BigDecimalBench` 50k (median of 6) | 1,955 ms | **1,299 ms** | 63 ms | **-33%**, 31x -> 21x |
| `LegendreHighPrecisionTest`, suite conditions (`--Xmx 1g`), 6 interleaved rounds | 110-120 s | **86-105 s** | 2.7 s | **-24%; HANG -> borderline PASS** at the 90 s budget |
| `BigDecimal.signum()` per call | 1,232 ns | **140 ns** | 4 ns | -89% |
| `BigInteger.signum()` per call | 288 ns | **101 ns** | 2 ns | -65% |
| `BigDecimal.scale()` per call | 379 ns | **98 ns** | 2 ns | -74% |
| `BigInteger.multiply` per call | 1,438 ns | **1,005 ns** | 47 ns | -30% |

Three changes, all inside `native-builtins/src/math_bignum.rs`:

1. **`mag:[I` is read and written in one copy.** Every read boundary walked the
   array with `get_array_element` per word — a trait-object dispatch, a `Value`
   box, and a full ZGC `audit_access_receiver` address validation EACH. The bulk
   primitives it needed already existed (`read_int_array_into` /
   `write_int_array_from`, added for the BouncyCastle digest kernels for exactly
   this reason): one bounds check, one `copy_nonoverlapping`, one validation
   instead of `len`. The per-element loop stays as the fallback for the cases the
   memcpy declines (wrong array kind; G1 humongous `int[]`, which has no flat
   `array_data_ptr`).
2. **The layout and the `ClassId` are memoized per VM.** `bi_layout` /
   `bd_layout` resolved every field index BY NAME on every call, and every result
   object re-resolved `"java/math/BigInteger"` through `ensure_class_initialized`
   before allocating. Both are fixed for the life of a VM once the class loads.
   The memo is a thread-local scoped by `NativeContext::vm_identity()` — the
   discriminator that API documents for exactly this ("native side caches must
   scope entries to this value; Rust tests can create multiple independent `Vm`
   instances in one process") — and only a SUCCESSFUL resolve is stored, because
   the pre-load `None` is legitimate and caching it would pin every later call to
   the synthetic-stub fallback. The `ClassId` memo additionally arms only once
   BOTH real-JDK layouts are visible, so synthetic-JDK mode — where the
   allocation funnel may FABRICATE a class whose identity is not stable — keeps
   going through the funnel unchanged.
3. **`signum` / `negate` / `abs` stopped rendering the magnitude to a decimal
   string.** This page had `BigInt::to_decimal` at 1.29% and called it "a loose
   end rather than a cost". It was a cost, concentrated in one method:
   `BigDecimal.signum()` read the whole magnitude and formatted it to a `String`
   to look at the first byte, and it runs 8x per benchmark iteration because it
   sits inside the real `compareTo`/`doRound` bytecode — 1,232 ns/call against
   ~150 ns for the accessors beside it. `negate` was worse in kind: render, edit
   the leading '-', re-parse, i.e. two O(digits^2) conversions to flip one bit.
   The sign is already stored in `intCompact` and in the backing `BigInteger`'s
   `signum:I`, so `signum` now touches `mag[]` not at all.

**The ceiling this page predicted for item 1 was "~8%, i.e. 1.09x".** The three
together are 1.5x. Two reasons the per-symbol shares understated it, both worth
carrying forward: a name-keyed resolve costs more than its own samples (it takes
the class-manager `RwLock`, and its `memcmp` lands in libc), and a 1.29% symbol
can be a 30x outlier concentrated in ONE caller rather than a thin tax spread
over many. `--dump-native-registry`'s invocation census plus a per-call probe
(`probes/BignumNativeCostProbe.java`) separate those two; a flat profile alone
cannot.

### A fourth item, found by re-profiling AFTER the three above (2026-08-18)

The three fixes above changed the shape of the profile, and a second
`perf record` on `c8c7545af` surfaced something the first one had buried:
**~4.3% of the run was hashing flag names with SipHash.**

`cratonvm_types::flags::runtime_var_os` consults `declared_flag_names()` on
**every** call, and with the default `RandomState` that is a SipHash of the key
plus a `memcmp` — to look up compile-time string constants in a set that never
changes after startup.

| symbol | before | after |
|---|---:|---:|
| `hash_one::<&str>` (SipHash) | 1.70% | 0.24% (and now a `TypeId` caller, not this path) |
| `sip::Hasher::write` | 1.41% | 0.19% |
| `runtime_var_os::<&str>` | 1.21% | absent |
| `FxHasher::hash_one` | — | 0.05% |

The fix is the trade this crate had already made once: `types/Cargo.toml`
records `StringPool` moving off SipHash because "FxHash is ~3-5x faster than
SipHash for the short ASCII strings", and the same reasoning applies verbatim to
`declared_flag_names()` and `MapSource`. Neither takes untrusted input — one is
built from a compile-time inventory, the other from the process environment — so
SipHash's HashDoS resistance is not load-bearing.

**Wall clock: 7,993 -> 7,752 ms median over 9 interleaved rounds (+3.0%), which
matches the profile share removed but is INSIDE this benchmark's noise** (the
same binary ranges 7,064-21,012 ms across those rounds). Stated as "consistent
with, not demonstrated by" the wall clock on purpose. One Azure run read
1,311 ms against an earlier 2,233 ms — 1.7x — and that is **noise, not the
effect**: the patched binary alone ranges 1,311-2,154 ms over five runs.
Recorded because a 1.7x that cannot be true is exactly the figure somebody
quotes later.

Order-safety was the one real risk of swapping a hasher and was checked:
`declared_flag_names()` is iterated in exactly one place, which inserts distinct
names into a map (order cannot change the result), and `from_process_env`
iterates `env::vars_os()` — the source, not the map — so its documented
first-wins duplicate semantics are untouched.

**Found (2026-08-18):** the uncached caller was
`cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_COMPACT_INLINE")` inside
**`jit_getfield`** (`vm/src/jit/helpers.rs`), read on every JIT getfield helper
call purely to decide whether to print a debug dump. Now `OnceLock`-cached,
which is the idiom the sibling gate ten lines above it
(`getfield_receiver_census_enabled`) already used.

How it was found matters more than the fix. `perf` could not name it: the flat
profile showed `runtime_var_os` at 1.21% with no caller, and a `dwarf` capture
attributed it to `alloc_raw_tlab` / `is_object_address` / `get_field` — the
*callee* side of the JIT->heap boundary, which sent the first search into
`zgc.rs` where every call site is properly cached. A **per-name census** of flag
reads (`CRATONVM_DBG_FLAGREADS=1`, kept in `flags.rs`) named it in one run:

```
total=4,600,000
   4,560,891  CRATONVM_DBG_COMPACT_INLINE     <- 99.1%, ~91 per iteration
      23,086  CRATONVM_DBG_SHADOW
       6,734  CRATONVM_DBG_EXCFRAME
```

After the fix the same run never reaches the first 200k report. **A flag read is
supposed to be rare — every gate is expected to cache its answer — so a name in
the millions IS the bug**, which is why counting by key beat counting by stack.

Worth recording where it sat: the comment on `GETFIELD_HELPER_CALLS` directly
above the call site argues carefully that one relaxed atomic increment is too
cheap to appear in the measured 8.2 ns `receiverFieldTax`. That is correct. The
uncached environment lookup on the very next line was the expensive one, and it
had been reasoned right past.

**Wall clock: not resolved, and not claimed.** Nine interleaved rounds put the
median at 6,836 -> 6,421 ms (+6.1%) but the mean at 6,792 -> 6,831 (~0%) and the
min at 6,155 -> 6,135 (~0%), with the fixed arm ahead in 6 of 9 paired rounds.
Median and mean disagreeing means the distribution moved, not the centre. The
arithmetic agrees it should be small: after the FxHash change above, each read is
~2 Fx hashes, so 4.56M x ~25 ns is ~1-2% — inside this benchmark's noise. What is
demonstrated is the removal of 4.6M redundant reads per 50k iterations; the
timing benefit is not, and these two fixes overlap (this one removes the calls,
the FxHash one made whatever remains cheap).

## What is left, and why it is not on this page

The post-fix profile of the witness class is flat and no longer bignum-shaped:
~25% GC and allocation (`alloc_raw_tlab`, `is_object_address`,
`ZObjectStarts::contains`, `MonitorTable::prune_dead`), ~14% JIT call and
allocation helpers, ~5% class/method metadata lookup, no member above 6.2%.
Every remaining bignum native costs ~1 us against HotSpot's ~50 ns, of which
~100 ns is the native funnel itself — measured directly, since `BigDecimal.scale()`
now does nothing but read one field and still costs 98 ns against a 23 ns
one-line Java getter on the same VM.

That last row is a range, not a number, and the range straddles the budget. The
host is shared — other sessions build and run VMs on it — so a class that costs
~90 s is scored PASS or HANG by load as much as by the binary. The controlled
statement is the INTERLEAVED comparison: across six rounds the fixed binary beat
the `dev`-base control in every round, by 20-27%, and the control never once came
in under 90 s. Do not quote "it passes now" without that qualifier.

Controls that rule the collector out as the differentiator, one command each:
`--Xmx 8g` and `--XX:UseGc G1` both reproduce the witness class's wall time
within noise (154 s / 158 s / 142 s). The cost is allocation and access RATE,
not collector choice or heap size.

So the residual is the general "make native->heap interaction cheap" problem —
the same verdict `bobyqa-numeric-kernel-is-80x-slower-than-hotspot` reaches from
a workload with no bignum in it at all. Further work belongs there, not here.

The suite consequence, measured on all 310 `commons-math-legacy` test classes
(one class per process, 90 s cap, real-JDK backend, JIT on, default GC):
**303 PASS, 2 HANG, 5 FAIL**, with every one of the 5 FAILs reproduced on
HotSpot in the same session or proven to be an unseeded-RNG flake that flips on
both VMs. The 2 HANGs are `BOBYQAOptimizerTest` and `PSquarePercentileTest`,
both already filed as throughput, both of them optimizer/statistic inner loops
rather than bignum. `CMAESOptimizerTest` is the same shape a third time — it
passes, at 68-118 s against HotSpot's 2.5 s.

## Reproduction

```bash
javac -d <dir> probes/BigDecimalBench.java probes/BignumNativeCostProbe.java
java -cp <dir> BigDecimalBench 200000                       # HotSpot
<cratonvm> --java-home <jdk-25> --Xmx 1g -cp <dir> BigDecimalBench 200000

# Per-call cost of the cheapest methods on the surface, against a one-line Java
# getter as the control. This is what separates "the method is slow" from "the
# call is slow", and it is how BigDecimal.signum() was caught.
<cratonvm> --java-home <jdk-25> --Xmx 1g -cp <dir> BignumNativeCostProbe 2000000

# Which natives a workload actually reaches, and how often.
<cratonvm> --java-home <jdk-25> --dump-native-registry /tmp/census.json \
    -cp <dir> BigDecimalBench 20000
```

The witness class itself, against HotSpot in the same shell (needs the
commons-math test classpath — /data/cm-legacy-classpath.txt on the Azure Linux box):

```bash
<cratonvm> --java-home <jdk-25> --Xmx 1g -c "<runner>:$CP" CratonRunner \
  org.apache.commons.math4.legacy.analysis.integration.gauss.LegendreHighPrecisionTest
```

Profiling needs a Linux host (`perf` is absent on the Windows dev box):

```bash
perf record -F 999 -g --call-graph=fp -o bd.perf.data -- \
  <cratonvm> --java-home <jdk-25> --Xmx 1g -cp <dir> BigDecimalBench 50000
perf report -i bd.perf.data --stdio --no-children --percent-limit 0.5
```

Call-graph note: `--call-graph=fp` yields shallow stacks on this release build
(`--children` attributes 18% to `osr_trampoline` and stops being informative),
so the flat profile above is the usable view. Capture with `dwarf` if caller
breakdown is needed.

## Related

* retired/commons-math-suite-run-RETIRED-20260818.md — the suite run this was found from, now closed.
* [`bobyqa-numeric-kernel-is-80x-slower-than-hotspot-20260817.md`](bobyqa-numeric-kernel-is-80x-slower-than-hotspot-20260817.md)
  — the other CratonVM-only "hang" found in the same run. It was first filed as
  an OSR refusal; that gate was real and is now fixed, and the wall time did not
  move. It is the SAME root cause as this page — compiled-code throughput, not
  an admission gap — that happens to produce the same symptom (a test that never
  finishes).
* [`lambda-sam-dispatch-bypasses-the-cached-invoke-path-20260817.md`](lambda-sam-dispatch-bypasses-the-cached-invoke-path-20260817.md)
  — why the first per-op breakdown here measured its own harness.
