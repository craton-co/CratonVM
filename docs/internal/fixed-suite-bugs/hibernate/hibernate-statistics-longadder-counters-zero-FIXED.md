# Hibernate statistics counters read back zero — FIXED

| | |
|---|---|
| **Status** | FIXED on 2026-07-12 |
| **Discovered** | 2026-07-11 Hibernate 4548-class suite audit |
| **Area** | `java.util.concurrent.atomic.Striped64`, `LongAdder`, `DoubleAdder`; Hibernate `StatisticsImplementor` |
| **Original scope** | 94 Hibernate classes grouped by positive expected counter values reading exactly `0` |

## Symptom

Hibernate successfully performed persistence, cache, query, natural-id, flush,
and statement work, but the corresponding statistics counters remained at
their initial value. The focused reproducer was
`org.hibernate.orm.test.ops.SimpleOpsTest`, which failed with
`java.lang.AssertionError: unexpected insert count` on CratonVM and passed on
HotSpot.

## Root cause

The registered `LongAdder` natives treated instance field slot 0 as the
counter's `base`. With the real JDK 25 layout, `LongAdder` inherits these
fields from `Striped64`:

1. `cells`
2. `base`
3. `cellsBusy`

Writing a `Value::Long` to slot 0 therefore targeted the reference-valued
`cells` field. Descriptor-aware storage coerced the value away, while the CAS
path still reported success. `LongAdder.increment()` and `add()` returned as
if the update had happened, and `sum()` read the same wrong slot back as zero.

`DoubleAdder` had the mirrored problem. Its inherited `base` is a `long`
containing raw double bits, but the native used slot 0 and treated it as a
direct `Value::Double` field.

## Fix

All LongAdder and DoubleAdder native base accesses now resolve
`Striped64.base` through the loaded class hierarchy. Slot 0 remains only as a
fallback for the synthetic one-field objects used by stub-JDK mode and native
unit tests. DoubleAdder now stores and atomically updates the raw IEEE-754 bits
in the inherited long field, matching the JDK representation.

The current `dev` tip also contained a pre-existing missing closing brace in
`../../../../vm/src/jit/helpers.rs`; that one-line prerequisite repair was required before
the baseline VM could build.

## Validation

All builds and probes ran on the Azure Linux host under `/data/data`. The
original focused validation used `cratonvm-hibstats-fixed2-20260712-001`.
The first complete closure sweep rebuilt `dev` at `a6807594` as the unique
release binary `cratonvm-hibstats-complete-currentdev-20260712-002`
(SHA-256 `cefa2d5fad8841a1886710d83c9e3c8bdcfa02a009e0b7188fb951ea59c5c64f`).
After `origin/dev` advanced, the closure branch was rebased onto `40c7e370`
and rebuilt as `cratonvm-hibstats-complete-mergeddev-20260712-003`
(SHA-256 `ded0269c8a7287461f9fcf53dc5bb7a09ff976aee0ccf2ecc3b0973010201c65`).
After another runtime update, the branch was rebased onto `5806fde9` and
rebuilt as `cratonvm-hibstats-complete-finaldev-20260712-004`
(SHA-256 `c43e175d5458e7bdf02da19add4805cdf3b73d0142983c087c06a9c7477171f9`).

| Probe/test | Before | After |
|---|---:|---:|
| LongAdder `increment(); add(9); sum()` | `0` | `10` |
| DoubleAdder `add(1.5); add(2.5); sum()` | `0.0` | `4.0` |
| `ops.SimpleOpsTest` | FAIL: unexpected insert count | PASS (1/1) |
| `stats.StatsTest` | counter-family member | PASS (1/1) |
| `stat.internal.ConcurrentQueryStatisticsTest` | counter-family member | PASS (1/1) |
| `jpa.ops.PersistTest` | counter-family member | PASS (5/5) |

Focused Rust regression:

```text
cargo test --release -p cratonvm-native-builtins striped64_base_slot_tests --lib
2 passed; 0 failed
```

## Complete closure sweep

The current-`dev` binary ran each class in a fresh VM process with a hard
per-process timeout. The inventory included all 94 classes from the original
cluster plus 21 statistics-adjacent candidates from the assertion-failure
longtail, for 115 unique classes total. The entire inventory was repeated after
each rebase onto newer `origin/dev` runtime changes; all three sweeps produced
the same totals.

```text
classes: 115 PASS, 0 FAIL, 0 TIMEOUT, 0 load errors
tests:   418 found, 414 started, 414 passed, 0 failed, 0 aborted, 4 skipped
```

All 94 original members and all 21 adjacent candidates were discovered. The
four skips were test-declared conditions in otherwise successful classes, not
runner or VM failures. The three residuals from the focused validation also
closed on this current-head sweep:

| Former residual | Current result |
|---|---:|
| `cache.CacheRegionStatisticsTest` | PASS (1/1) |
| `querycache.QueryCacheTest` | PASS (8/8) |
| `mapping.naturalid.NaturalIdTest` | PASS (6/6) |

The standalone real-JDK probe reported `LongAdder` values `0 -> 1 -> 10`, and
the inherited-slot Rust regression remained green (2/2). There is no remaining
statistics-counter residual in this cluster or in its adjacent longtail set.
