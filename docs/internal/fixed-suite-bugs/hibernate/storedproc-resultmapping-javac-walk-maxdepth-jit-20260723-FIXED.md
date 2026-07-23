# Hibernate stored-procedure/result-mapping real-javac archive walk and JIT residual - FIXED

**Closed:** 2026-07-23

## Affected tests

- `org.hibernate.orm.test.sql.storedproc.StoredProcedureTest`
- `org.hibernate.orm.test.sql.storedproc.ResultMappingTest`

Both tests ask H2 to compile `CREATE ALIAS ... AS $$ ... $$` Java bodies with
the real in-process JDK compiler.

## Root causes and fixes

`Files.walkFileTree(Path, Set, int, FileVisitor)` ignored its `maxDepth` in
the native implementation. Javac uses `maxDepth=1` for non-recursive
`ArchiveContainer.list` package queries, but CratonVM recursively walked every
descendant of every queried package. The archive index now caches immediate
children and the walker propagates the requested depth, so javac performs the
bounded listing it requested. The exact directory-only archive index visitor
also uses the cached tree without file callbacks.

After the throughput fix exposed normal JIT tiering, the real javac method
`com.sun.tools.javac.code.Symbol$ClassSymbol.complete()` reproducibly
underflowed the operand stack at an `invokespecial` while compiling an H2
alias. A narrow JIT admission quarantine keeps only that compiler-internal
method interpreted; a skip-list regression test protects the policy.

## Validation

Using a fresh release CratonVM binary and the real JDK 25.0.3:

- `StoredProcedureTest`: 4/4 in `--nojit` (32.3 s); 4/4 JIT with the exact
  admission guard (35.7 s).
- `ResultMappingTest`: 4/4 in `--nojit` (32.6 s).

The final acceptance reruns both classes in JIT and `--nojit` with the
permanent guard enabled.
