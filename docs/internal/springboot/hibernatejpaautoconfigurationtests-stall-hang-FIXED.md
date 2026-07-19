# `HibernateJpaAutoConfigurationTests` stall/hang — FIXED

**Status: FIXED — 2026-07-18**

## Resolution

The original 300-second result was a false timeout classification. The real
Spring Boot Hibernate/H2 class is a long-running, CPU-active workload, not a
deadlock, and now receives a narrowly scoped 1,200-second runner allowance.
The suite-wide default remains 300 seconds.

Two VM defects were found while following the related failures to completion:

- `URL.openConnection()` returned a synthetic HTTP connection for `file:` URLs.
  `URLClassLoader.getResourceAsStream()` consequently observed EOF for valid
  `@WithResource` SQL files. `file:` URLs now construct the real JDK
  `sun.net.www.protocol.file.FileURLConnection`.
- `ModuleLayer.modules()` created a real `HashSet` but did not retain it while
  subsequently re-entering Java to populate the service catalog. A moving GC
  could leave a stale reference that surfaced as `Object cannot be cast to
  Collection` in Spring's module-path resource scanner. The collection is now
  rooted and refreshed before it is stored or returned.

`UrlClassLoaderResourceDelegation` is a checked-in real-JDK regression that
verifies parent-first resource lookup and stream contents with JIT on and off.

## Validation

- `cargo test -p cratonvm-vm --test url_classloader_resource_delegation -- --nocapture`
  passed (both JIT modes).
- Spring Boot suite runner, exact fixture class and release binary:
  - JIT: 70 tests, 0 failures, 3 skipped, 851.965 seconds.
  - `--nojit`: 70 tests, 0 failures, 3 skipped, 768.733 seconds.

The results were produced by the isolated worktree runner under
`apps/spring-boot-suite-runner`; only this class receives the 1,200-second
override.
