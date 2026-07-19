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
