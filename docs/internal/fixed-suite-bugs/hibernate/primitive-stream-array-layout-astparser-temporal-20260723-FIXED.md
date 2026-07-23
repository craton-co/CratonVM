# Hibernate primitive-stream array layout — fixed (2026-07-23)

## Root cause

Synthetic `IntStream`, `LongStream`, and `DoubleStream` objects stored primitive
`Value`s in a reference array.  The array write converted primitive values to
`null`, so interpreter-mode primitive callbacks received zero.  Woodstox uses
`IntStream.rangeClosed(...).forEach(...)` to populate its XML-name lookup table;
the empty table made `hql.ASTParserLoadingTest` reject `hibernate-mapping` in
`Animal.hbm.xml` before any tests started.

Primitive iterators shared the same incorrect reference-array storage.

## Fix

`native-collections/src/lib.rs` now allocates primitive backing arrays matching
the stream/iterator element kind (`int`, `long`, or `double`).

## Validation

Fresh 2 GiB JVM invocations using the Hibernate suite runner and the fixed
binary passed all requested classes in both modes:

| Class | JIT | `--nojit` |
| --- | --- | --- |
| `OracleInlineMutationStrategyIdTest` | 6/6 | 6/6 |
| `ASTParserLoadingTest` | 106/106 | 106/106 |
| `OffsetDateTimeTest` | 324 passed, 164 expected aborts | 324 passed, 164 expected aborts |
| `ZonedDateTimeTest` | 404 passed, 204 expected aborts | 404 passed, 204 expected aborts |

The four classes cannot be treated as one 2 GiB shared-process gate: after the
first three finish, the runner retains enough Hibernate state for Zoned test
discovery to OOM in both modes.  Each class passes in its normal fresh process.
