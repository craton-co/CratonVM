# `module/spring-boot-data-redis` HANG cluster — uncached `URLClassLoader` classpath rescan — FIXED 2026-07-23

## Symptom

Four `module/spring-boot-data-redis` classes HANG (300s suite timeout, zero
tests completed) on every rerun since at least 2026-07-17:

- `DataRedisAutoConfigurationTests`
- `DataRedisAutoConfigurationJedisTests`
- `DataRedisAutoConfigurationLettuceWithoutCommonsPool2Tests`
- `DataRedisHealthContributorAutoConfigurationTests`

Confirmed via `docs`/`apps/spring-boot-suite-runner/RESULTS-20260723.md`'s
2026-07-23 rerun and the earlier 2026-07-17 round — same 4 classes both
times, always `HANG`, never `FAIL`.

## Root cause (two compounding bugs, both in `native-builtins/src/`)

`module/spring-boot-data-redis`'s test classpath has ~121 jars (Lettuce +
Jedis + Netty + commons-pool2 + Spring SSL infra stack up unusually large
compared to most modules). Every one of these 4 classes' test methods
creates a fresh `ApplicationContext` via `ApplicationContextRunner`, which
triggers hundreds of `ClassUtils.isPresent()`/`ClassLoader.loadClass()`
calls per test (checking optional dependencies, evaluating
`@ConditionalOnClass`, etc.) — completely ordinary Spring Boot
auto-configuration bootstrap, not Redis-specific.

**Bug 1 (dominant, ~99% of the cost) — `classloader.rs`:**
`ucl_try_define_local_class` (backs `URLClassLoader.findClass`) and
`loader_local_resource_urls` (backs `findResource`/`getResources`) both
called `cratonvm_classloading::ClassPath::new(&paths)` **fresh on every
single invocation**, with no caching at all. `ClassPath::new` opens and
parses every classpath entry eagerly. On a 121-jar classpath this meant
every `isPresent()`/`loadClass()` call — not just the first — repeated the
full classpath scan. Measured: 100 mixed hit/miss `Class.forName` calls on
one `URLClassLoader` took ~10-12s on CratonVM vs ~126ms on HotSpot (a
~90x-ish gap, and growing with classpath size and call count — genuinely
unbounded for a class with dozens of test methods).

**Bug 2 (secondary, `phases_late.rs`):** `jar_contents_cached` (backing the
synthetic `JarFile.getEntry`/`getJarEntry`/`entries`/`stream`/`getManifest`
natives) eagerly `read_to_end`'d (fully DEFLATE-decompressed) **every
entry** in a jar on the first touch, even when the caller only wanted a
metadata/existence answer. A single `new JarFile(...)` on `testcontainers`
`.jar` (2.0.5, 12566 entries) cost ~600-750ms just from decompressing
thousands of unrelated class files nobody asked for; measured ~60-70us/entry,
scaling linearly with jar size. Also, `p98_read_jar_manifest` (called from
every `JarFile`/`ZipFile` `<init>`) did its own independent, uncached
`zip::ZipArchive::new` + manifest read, redundant with the metadata cache.

Neither bug is Redis-specific — they're general classloading/jar
infrastructure — but `spring-boot-data-redis`'s unusually large test
classpath was what tipped these 4 specific classes over the 300s suite
timeout while smaller-classpath modules mostly stayed under it (some
residuals in other modules, e.g. `DataJpaRepositoriesAutoConfigurationTests`,
`WebSocketMessagingAutoConfigurationTests`, were hitting the exact same
bugs — see "Residuals unmasked" below).

## Fix

- `native-builtins/src/classloader.rs`: added `cached_class_path_for_paths`,
  a `Mutex<HashMap<Vec<String>, Arc<ClassPath>>>` cache keyed by the exact
  `paths` vector (so `URLClassLoader.addURL` naturally gets its own fresh,
  correct entry — no explicit invalidation needed). Both
  `ucl_try_define_local_class` and `loader_local_resource_urls` now go
  through it instead of calling `ClassPath::new` directly.
- `native-builtins/src/phases_late.rs`: split `jar_contents_cached`'s
  per-entry record (`JarEntryRec`) into metadata-only (no `bytes` field);
  added a new `jar_entry_bytes_cached`, a per-(path, mtime, entry name)
  cache that decompresses lazily, only when `getInputStream` is actually
  called for that entry. `p98_read_jar_manifest` now reads the manifest via
  `jar_entry_bytes_cached` instead of its own independent
  `ZipArchive::new`+`read_to_end`.

## Verification

- Microbenchmark (100 mixed `Class.forName` calls over the data-redis test
  classpath): **~12000ms → 176ms** (HotSpot baseline: 126ms — now
  comparable).
- All 4 originally-HANG classes: `DataRedisAutoConfigurationLettuceWithoutCommonsPool2Tests`
  and `DataRedisHealthContributorAutoConfigurationTests` now `PASS` cleanly
  (~18-24s). `DataRedisAutoConfigurationTests` (56 tests) now completes and
  **passes 56/56** — just takes ~349s, 49s over the 300s suite-runner
  default (see "Residual" below; this is a genuine perf gap unrelated to
  the caching bug, not a hang). `DataRedisAutoConfigurationJedisTests` now
  completes in ~215s, 22/23 tests pass — see
  `data-redis-jedis-sslbundle-withpackageresources-classloader-leak.md` for
  the one remaining failure (pre-existing, unrelated, previously masked by
  the class-level hang).
- Regression sweep: 82 classes (all `module/spring-boot-data-redis` +
  71 classes across the codebase using `@ClassPathExclusions`/
  `@ClassPathOverrides`, chosen because they most directly exercise the
  changed caching code). 41 PASS, 41 FAIL/HANG — **every single FAIL/HANG
  cross-checked against the pre-fix 2026-07-23 baseline
  (`apps/spring-boot-suite-runner/RESULTS-20260723.md`) and found already
  FAIL or HANG there** — zero new regressions.

### Residuals unmasked (not new — previously hidden behind the 300s HANG on these specific classes)

- `DataJpaRepositoriesAutoConfigurationTests`: HANG(300s) → FAIL(96s).
- `WebSocketMessagingAutoConfigurationTests`: HANG(300s) → FAIL(184s).

Both were already broken before this fix (confirmed via the 2026-07-23
baseline); the classloading fix just lets them run to completion fast
enough to show their real, different, unrelated failures instead of timing
out. Not triaged further here — flagged for whoever picks up those classes
next.

### Residual: `DataRedisAutoConfigurationTests` still ~349s (> 300s default)

Fully correct (56/56 pass) but the raw CratonVM interpreter cost of ~50+
individual `ApplicationContext` refreshes (each doing real bean
instantiation, condition evaluation, etc.) adds up to more real time than
HotSpot needs. This is the same general "interpreted bytecode dispatch
overhead" class of gap documented elsewhere (see
`[[reference_junit5_execution_machinery_dispatch_overhead]]`,
`[[reference_hashmap_native_call_dispatch_overhead_20260711]]` etc.) —
not something specific to Redis or to this fix, and out of scope for a
targeted follow-up here. Worth rerunning with a longer per-class timeout
in future suite passes rather than chasing further speedups.

## Worktree / branch

`C:\craton\CratonVM-data-redis-fix-20260723`, branch
`fix/spring-boot-data-redis-hangs-20260723`, binary
`cratonvm-data-redis-fix-20260723.exe`.
