# Spring Boot Multipart, SSL connector, and static-resource jar residuals

**Status: FIXED — 2026-07-30**

## Repairs

- Windows extended TCP keepalive now treats JDK-owned opaque socket descriptors
  as an advisory no-op instead of routing them through the UDP table and
  throwing `TCP_KEEPIDLE: bad fd for udp`.
- `JarURLConnection.getJarFile()` caches the connection-owned `JarFile`, so a
  non-cached connection exposes the same closed instance on later calls.
  `JarFile.getComment()` now rejects access after that instance is closed.
- SSL context supported-parameter reporting preserves an explicitly configured
  TLSv1.1 connector policy through Tomcat's JSSE validation path.

## Verification

Complete real-JDK Spring Boot fixture, dedicated release executable:

| Mode | MultipartAutoConfigurationTests | SslConnectorCustomizerTests | StaticResourceJarsTests |
|---|---:|---:|---:|
| JIT enabled | 12 passed | 8 passed | 7 passed, 1 skipped |
| `--nojit` | 12 passed | 8 passed | 7 passed, 1 skipped |

Every started test passed; no class had an abort or container failure.
