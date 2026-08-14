# `ucl_scan_bench` — `URLClassLoader` classpath-rescan microbenchmark

Isolates the cost of repeated `Class.forName` lookups through a single
`URLClassLoader` over a large classpath — the `ClassUtils.isPresent()` /
`@ConditionalOnClass` shape Spring Boot drives hundreds of times per
`ApplicationContext` refresh.

This is the check that opened and closed the `data-redis` HANG cluster
(`data-redis-urlclassloader-uncached-classpath-hang-FIXED.md` in
`fixed-suite-bugs/springboot/`). It matters because it separates classloading
cost from the general interpreter gap: the test classes it was derived from
are 10-24x slower than HotSpot for unrelated reasons, so they cannot tell you
whether *this* path regressed.

## Run

```bash
javac -d apps/ucl_scan_bench apps/ucl_scan_bench/UclScanBench.java
# <classpath-file> is any module's generated apps/spring-boot/<module>/build/cratonvm-test-cp.txt
java    -cp apps/ucl_scan_bench UclScanBench <classpath-file> 100
cratonvm --cp apps/ucl_scan_bench UclScanBench <classpath-file> 100
```

Prints `BENCH_TOTAL_MS=<n>`.

## Reference numbers

122-entry `spring-boot-data-redis` test classpath, 100 iterations
(= 400 hits + 400 misses), Windows, JDK 25:

| | CratonVM | HotSpot |
|---|---:|---:|
| 2026-07-23, before the caching fix | ~12000ms | 126ms |
| 2026-07-23, after | 176ms | 126ms |
| 2026-08-13 | 101ms | 87ms |

A CratonVM number in the seconds means the per-call classpath rescan is back:
check `cached_class_path_for_paths` in `native-builtins/src/classloader.rs`
and its two callers (`ucl_try_define_local_class`, `loader_local_resource_urls`).
