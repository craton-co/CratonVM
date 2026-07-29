# `OriginTrackedYamlLoaderTests.canLoadFilesBiggerThan3Mb`: tiny-array OOM after non-moving young sweep — FIXED

**Status: FIXED 2026-07-29.** Retired from `docs/known-issues/springboot/`.

## Symptom

`core/spring-boot`'s
`org.springframework.boot.env.OriginTrackedYamlLoaderTests` crashed in real
SnakeYAML parsing with:

```
java/lang/OutOfMemoryError: Java heap space (alloc_array length 1026)
```

The failing allocation was tiny and occurred in
`Arrays.copyOfRange()` / `StreamReader.update()` while parsing the test's
3MB-plus YAML input. The HotSpot control passed, so this was not a fixture
size or runner-classpath problem.

## Root cause and repair

Under a live JIT frame whose roots cannot be safely rewritten, young GC
correctly falls back to a non-moving mark/sweep. That can leave young space
fragmented even when the VM has ample old-generation headroom. Interpreter
array allocation (`gc_alloc_array`) retried only the young-only
`try_alloc_array` API, then converted that fragmentation into OOM. The
corresponding object path and the JIT array helper already use
`try_alloc_array_full`, which first tries young and then spills safely to the
non-moving old generation.

`vm/src/runtime/interpreter.rs` now uses `try_alloc_array_full` for each
interpreter array attempt. The repair preserves young-first allocation,
uses existing old-generation pressure accounting, and only reports OOM once
both generations cannot satisfy the request. The JIT direct-call master gate
also now covers the hashed megamorphic virtual-dispatch stub, so its opt-out
does not leave a raw compiled-callee call reachable.

## Validation

All runs used the complete Spring Boot fixture at
`C:\craton\CratonVM-spring-boot-residual-20260728\apps\spring-boot`; the
user-supplied checkout could not configure because
`build-plugin/spring-boot-antlib` is absent. The suite runner and class list
came from the supplied `apps/spring-boot-suite-runner` checkout. Each result
is for the full class and reports `tests=13 failed=0 aborted=0 skipped=0
containersFailed=0`.

| VM / mode | Result | Wall time |
| --- | --- | --- |
| HotSpot JDK 25 | PASS, 13/13 | 2.083s |
| CratonVM JIT | PASS, 13/13 | 422.340s |
| CratonVM `--nojit` | PASS, 13/13 | 373.866s |
| CratonVM JIT repeat | PASS, 13/13 | 266.349s |

The final CratonVM executable was
`cratonvm-origintrackedyaml-final-019faf1d.exe`
(SHA-256 `CBA782AD98C9F220C0A6634EFFE927AC14CD0C026ECBB68F40CE852BE075CA20`).
