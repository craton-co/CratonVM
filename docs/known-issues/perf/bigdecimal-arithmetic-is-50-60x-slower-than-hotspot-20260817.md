# `BigDecimal` arithmetic is 50-64x slower than HotSpot — profiled 2026-08-18: the arithmetic is ~2% of the profile, and there is no single hotspot

**Status: OPEN, perf. Profiled 2026-08-18 on `dev` `64c02b7ac`; the original
per-call-overhead hypothesis is REFUTED in the form it was written. No fix
attempted — but the plan this page used to prescribe is now known to be the
wrong one, and its replacement is measured rather than reasoned from shape.**

Found triaging the Apache Commons Math test suite
(`apps/commons-math/RESULTS-20260817.md`): `LegendreHighPrecisionTest` (2 JUnit
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

## Where to actually look, in profile order

Nothing below is attempted. Each carries the ceiling it can buy, so nobody
spends a week on a 5% item expecting 60x.

1. **`resolve_field_index` (5.06%, plus much of the 3.31% `memcmp`).**
   `bd_layout` resolves `intVal` / `scale` / `precision` / `intCompact` **by name
   on every call**, and `resolve_field_index_in_hierarchy_desc` is a linear
   string-compare scan over every non-static field, walking the superclass chain.
   `native_bd_add` pays that for both operands and the result, plus `bi_layout`
   for each `BigInteger`. Memoizing the layout is contained and is the single
   biggest coherent item — **ceiling ~8%, i.e. 1.09x, not 60x.** Note AGENTS.md
   forbids process globals for per-VM state, so the cache must hang off the VM
   rather than a `static`.
2. **Heap address validation (~8%: `is_object_address` + `ZObjectStarts::contains`).**
   Every `get_array_element` / `set_field` from a native re-validates the
   address, and the bignum natives walk `mag:[I` element by element, so this
   scales with digit count.
3. **Allocation (~7%).** Each operation allocates a `BigDecimal`, a `BigInteger`
   and an `int[]`.
4. **`BigInt::to_decimal` at 1.29%** — a decimal *rendering* on a path that
   should be pure limbs. Small, but it is exactly the kind of decimal round trip
   the limb rewrite retired elsewhere, so it may be a loose end rather than a
   cost.

The honest summary for planning: **no single change here returns the 12-64x.**
Three subsystems each cost several times what the arithmetic does, and closing
the gap means making native→heap interaction cheap in general, not patching
`math_bignum.rs`.

## Reproduction

```bash
javac -d <dir> probes/BigDecimalBench.java
java -cp <dir> BigDecimalBench 200000                       # HotSpot
<cratonvm> --java-home <jdk-25> --Xmx 1g -cp <dir> BigDecimalBench 200000
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

* `apps/commons-math/RESULTS-20260817.md` — the suite run this was found from.
* [`bobyqa-numeric-kernel-is-80x-slower-than-hotspot-20260817.md`](bobyqa-numeric-kernel-is-80x-slower-than-hotspot-20260817.md)
  — the other CratonVM-only "hang" found in the same run. It was first filed as
  an OSR refusal; that gate was real and is now fixed, and the wall time did not
  move. It is the SAME root cause as this page — compiled-code throughput, not
  an admission gap — that happens to produce the same symptom (a test that never
  finishes).
* [`lambda-sam-dispatch-bypasses-the-cached-invoke-path-20260817.md`](lambda-sam-dispatch-bypasses-the-cached-invoke-path-20260817.md)
  — why the first per-op breakdown here measured its own harness.
