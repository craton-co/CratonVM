# spring-boot-zipkin real-socket hang cluster (FIXED)

**Fixed 2026-07-18.**

## What actually happened

The original pair of silent hangs was stale runner evidence, not a shared
socket-retry loop. A fresh watchdog capture showed recursive JUnit
`ModifiedClassPathExtension` execution in the old binary; current `dev`
already contained the classpath repair, and the Testcontainers class passed
under both execution modes.

The remaining `ZipkinHttpClientSenderTests` residual exposed two request-body
fidelity bugs on the real-JDK HTTP path:

1. RE5 `HttpRequest.BodyPublishers.ofByteArray` converted binary data through
   a lossy UTF-8 `String`, turning non-UTF-8 gzip bytes into `EF BF BD`.
   The publisher now retains the original `byte[]`.
2. The real `java.util.zip.Deflater` native route used a standards-compliant
   but byte-different compression backend. Spring Boot compares the exact
   HotSpot gzip stream. Both that route and the GZIPOutputStream bridge now
   use bundled stock zlib for deterministic HotSpot-compatible DEFLATE bytes.

The gzip bridge's buffer-growth path was also rooted across moving-GC
allocations and Java output callbacks, preventing stale object references
from corrupting an accumulated payload.

The Hazelcast and Reactor Netty entries mentioned by the original hypothesis
remain separate tracked issues: this root cause is confined to Zipkin's
HttpClient request-body and compression paths, not socket connection retry.

## Validation

Dedicated release executable:
`artifacts/cratonvm-zipkin-realsocket-closure-20260718-019f768f.exe`

| Mode | `ZipkinHttpClientSenderTests` | `ZipkinContainerConnectionDetailsFactoryWithoutActuatorTests` |
|---|---:|---:|
| JIT | PASS (7/7, 26.7s) | PASS (1/1, 7.5s) |
| `--nojit` | PASS (7/7, 26.3s) | PASS (1/1, 6.9s) |

Runner result roots:

- `apps/spring-boot-suite-runner/.suite-zipkin-realsocket-20260718-019f768f/results/zipkin-final2-jit-019f768f/zipkin-final2-jit`
- `apps/spring-boot-suite-runner/.suite-zipkin-realsocket-20260718-019f768f/results/zipkin-final2-nojit-019f768f/zipkin-final2-nojit`

## Update 2026-07-23 — `ZipkinHttpClientSenderTests` regressed to 6/7 (`sendShouldCompressData` fails again)

`craton-rerun-20260723` (`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260723/shard7/logs/module_spring-boot-zipkin.org.springframework.boot.zipkin.autoconfigure.ZipkinHttpClientSenderTests.out.log`)
shows `sendShouldCompressData()` failing again with
`org.assertj.core.error.AssertJMultipleFailuresError` — but the harness's
`TestExecutionSummary.printFailuresTo` captured **no message body and no
stack trace** past the 3-line call chain down to
`ZipkinHttpClientSenderTests.java:168` (`assertThat(request).satisfies(assertions)`,
line 148's lambda checking method/`Content-Type`/`Content-Encoding`/gzip body
bytes). Which of the 4 `requestAssertions` sub-checks actually failed is
**not determinable from this log alone** — not re-run to get a fuller capture
(out of this triage pass's scope, log-reading only). Given this doc's fix was
specifically about byte-exact DEFLATE/gzip output (`Deflater`/`GZIPOutputStream`
routed through bundled stock zlib to match HotSpot's exact compressed bytes),
the gzip-body assertion (`request.getBody().readByteArray()).isEqualTo(compressed)`)
is the most likely candidate to have regressed, but this is **not confirmed**
— could equally be one of the three simpler header/method assertions. Other
6/7 tests in the class still pass. Flagged as a real residual needing a fresh
run with fuller failure capture (e.g. `-Dassertj.printAssertionsMessages` or
just re-running just this method in isolation) before it can be root-caused
further.

## Update 2026-07-28 — still failing, identical un-detailed shape; `zlib-rs` is still enabled so this is not simply the compression-backend feature flag reverting

`craton-rerun-20260728` (`apps/spring-boot-suite-runner/.suite/results/craton-rerun-20260728/shard2/logs/module_spring-boot-zipkin.org.springframework.boot.zipkin.autoconfigure.ZipkinHttpClientSenderTests.out.log`)
shows `sendShouldCompressData()` failing again, still with a bare
`AssertJMultipleFailuresError` and no captured sub-assertion detail (same
gap the 2026-07-23 note above already flagged: "no message body and no
stack trace past the 3-line call chain"), 6/7 other tests in the class pass.

Checked one thing the 2026-07-23 note didn't: `native-builtins/Cargo.toml`
(current worktree) still declares
`flate2 = { version = "1", features = ["zlib-rs"] }` — the exact
byte-compatible-with-real-zlib backend this doc's own fix relies on, with
its explanatory comment still present and still citing this test
("Spring Boot's Zipkin sender verifies the exact bytes produced by
HotSpot"). This rules out "the `zlib-rs` Cargo feature got reverted/dropped"
as the regression cause — whatever's failing now is either a different
byte-level mismatch than the original DEFLATE-encoding gap this doc fixed
(e.g. the gzip header's OS byte, mtime field, or `FLG` byte, none of which
`zlib-rs` governs), or one of the three simpler header/method assertions the
2026-07-23 note already flagged as equally possible. Not re-investigated
further this session (log-analysis/triage only, no build or test execution
performed) — still needs the same fuller-capture rerun the 2026-07-23 note
called for.
