# Curated JDK regression baseline

This is the committed contract for `vm/tests/jck_conformance.rs`.
The corpus is an in-tree engineering suite, not Oracle's JCK and not evidence
of Java certification. Floors are minimum passing counts, not coverage claims.
Update this table and `BASELINE_FLOORS` together when intentionally changing a
floor or the corpus population. Runtime corpus tests require `synthetic-jdk`
and compiled fixtures; table-consistency checks require neither.

| Category | Floor | Total |
|---|---|---|
| ClassFile | 4 | 5 |
| Concurrent | 8 | 19 |
| Http | 0 | 10 |
| Instructions | 9 | 13 |
| Io | 18 | 37 |
| Jdbc | 0 | 11 |
| Lang | 34 | 110 |
| Loading | 4 | 5 |
| Management | 0 | 9 |
| Math | 3 | 15 |
| Net | 0 | 10 |
| Nio | 11 | 25 |
| Reflect | 5 | 21 |
| Regex | 0 | 11 |
| Security | 0 | 19 |
| Sql | 8 | 12 |
| Text | 0 | 16 |
| Time | 0 | 31 |
| Util | 3 | 45 |
| **TOTAL** | **107** | **424** |
