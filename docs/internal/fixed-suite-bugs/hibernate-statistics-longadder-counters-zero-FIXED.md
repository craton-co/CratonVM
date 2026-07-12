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
`vm/src/jit/helpers.rs`; that one-line prerequisite repair was required before
the baseline VM could build.

## Validation

All builds and probes ran on the Azure Linux host under `/data/data` with the
task-specific release binary
`cratonvm-hibstats-fixed2-20260712-001`.

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

## Residual boundary

This archive closes the shared zero-counter defect, not every unrelated failure
in every originally grouped class. On the fixed binary:

- `cache.CacheRegionStatisticsTest` fails before statistics assertions because
  `Dog` is not registered in its persistence unit.
- `querycache.QueryCacheTest` and `mapping.naturalid.NaturalIdTest` reach their
  per-class timeout without reproducing the zero-counter assertion.

Those outcomes must remain separate from this resolved LongAdder family.
