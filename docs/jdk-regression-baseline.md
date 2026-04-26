# JDK Regression Baseline

**Updated**: 2026-04-16 (Session 61, T4.2–T4.6 corpus expansion)

This document records the committed pass floors for the JCK-style curated
TCK corpus in `vm/tests/jck_conformance.rs`. CI fails if any category
drops below its floor. Raise a floor only after a deliberate improvement
has landed AND the new number is reproducible on a clean build.

The harness lives at [`vm/tests/jck_conformance.rs`](../vm/tests/jck_conformance.rs).
It enumerates a hand-maintained corpus of `public static int testXxx()` methods
(each returning `1` for pass, `0` for fail) and tallies the result per category.
A second `#[test]` (`jck_regression_gate`) enforces the baseline floors below —
CI fails if any category's pass count drops below its floor.

## Corpus Size

**421 tests** across 19 categories (expanded from 95 tests / 11 categories in
Session 53).

## Baseline (Session 61, 2026-04-16)

| Category | Floor | Total | Rate | Notes |
|---|---|---|---|---|
| ClassFile | 4 | 5 | 80% | JVMS Ch.4 — magic, version, CP, fields, methods |
| Concurrent | 8 | 19 | 42% | AtomicInteger/Long basic CAS operations |
| Http | 0 | 10 | 0% | java.net.http API surface not yet wired |
| Instructions | 9 | 13 | 69% | JVMS Ch.6 — most opcodes pass |
| Io | 20 | 37 | 54% | BAOS, BAIS, File I/O, StringWriter, BufferedOutput |
| Jdbc | 0 | 11 | 0% | JDBC wired but not through curated TCK path |
| Lang | 34 | 109 | 31% | Core types, wrappers, Math, System, Runtime |
| Loading | 4 | 5 | 80% | Class loading, static init, inheritance |
| Management | 0 | 9 | 0% | MXBeans not yet wired in curated TCK path |
| Math | 3 | 15 | 20% | BigInteger basic add/subtract/multiply |
| Net | 0 | 10 | 0% | URL/URI constructors not through TCK path |
| Nio | 11 | 25 | 44% | ByteBuffer core operations |
| Reflect | 5 | 21 | 24% | Class metadata basics (forName, getName, etc.) |
| Regex | 0 | 11 | 0% | Pattern/Matcher not through TCK path |
| Security | 0 | 19 | 0% | Crypto APIs not through TCK path |
| Sql | 8 | 12 | 67% | java.sql.Types constants, SQLException |
| Text | 0 | 16 | 0% | DecimalFormat/MessageFormat not wired |
| Time | 0 | 31 | 0% | java.time API not wired |
| Util | 3 | 43 | 7% | Basic collections (needs synthetic-jdk for full) |
| **TOTAL** | **109** | **421** | **26%** | |

Legend:

- **Pass**: invocation returned `Ok(Some(Int(1)))`.
- **Fail**: invocation returned a non-1 int (test logic rejected).
- **Error**: invocation returned `Err(...)` — usually a `NoSuchMethodError`,
  `LinkageError`, or `NoClassDefFoundError` from the VM, indicating a real
  implementation gap rather than an incorrect test.

## Categories added in Session 61

The T4.2–T4.6 expansion added 8 new categories and 20 new Java test classes:

| Category | Test class(es) | T4 item |
|---|---|---|
| Concurrent | TckAtomic, TckLocks | T4.2.6, T4.2.7 |
| Regex | TckRegex | T4.2.8 |
| Time | TckInstant, TckZonedDateTime, TckLocalDate | T4.2.13–T4.2.15 |
| Text | TckDecimalFormat, TckMessageFormat | T4.2.16–T4.2.17 |
| Math | TckBigMath | T4.2.20 |
| Http | TckHttpClient | T4.3 |
| Sql | TckSql | T4.4 |
| Management | TckManagement | T4.6 |

Existing categories expanded: Lang (+TckStringBuilder, TckThread, TckStackTrace),
Util (+TckCollections), Io (+TckReader, TckPrintStream), Nio (+TckFileChannel,
TckFiles), Security (+9 methods), Jdbc (+11 existing methods now referenced),
Reflect (+16 existing methods now referenced).

## Regression floors

The `BASELINE_FLOORS` constant in `vm/tests/jck_conformance.rs` mirrors the
Floor column above. The `jck_regression_gate` test fails if any category's pass
count drops below its floor. Raising a floor requires:

1. Landing a change that makes the new pass count reproducible on a clean build.
2. Updating both `BASELINE_FLOORS` **and** this document in the same commit.
3. Running `cargo test -p rustjvm-vm --test jck_conformance` locally to
   confirm the gate still passes.

Floors may be lowered only to reflect an intentional test removal — never to
paper over a real regression.

## How to run

```bash
# Default features (honest baseline — no synthetic JDK stubs):
cargo test -p rustjvm-vm --test jck_conformance -- --nocapture

# With synthetic JDK (enables ~5200 native stubs, may trigger JIT bugs):
cargo test -p rustjvm-vm --test jck_conformance --features synthetic-jdk -- --nocapture
```

## History

| Date | Corpus | Passing | Rate | Session |
|---|---|---|---|---|
| 2026-04-15 | 95 | 32 | 34% | S53 (initial NEW-16 corpus) |
| 2026-04-16 | 421 | 109 | 26% | S61 (T4.2–T4.6 expansion) |
| 2026-04-16 | 421 | 109 | 26% | S62 (T4 DELIVERED — all 80 items) |
| 2026-04-16 | 421 | 109 | 26% | S63 (T7 DELIVERED — 145 AWT natives, 17/17 tests) |
| 2026-04-16 | 421 | 109 | 26% | S64 (T8 DELIVERED — 607+ natives, 135+29 dep tests) |
| 2026-04-16 | 421 | 109 | 26% | S65 (T11 DELIVERED — safety hardening, 329/330 SAFETY, 991/993 casts, lock ordering) |
