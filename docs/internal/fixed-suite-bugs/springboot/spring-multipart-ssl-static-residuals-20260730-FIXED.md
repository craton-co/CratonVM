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
- Instance-method invocation tier-up was turned off by default here, because
  its pre-decoded virtual promotion could strand an embedded-server request.
  **Superseded 2026-07-30 — the default is ON again.** The stranding requires a
  callee that DECLARES AN EXCEPTION TABLE, and this change gated exactly that
  on three of the four routes able to reach a direct compiled entry
  (`mic_callee_has_exception_table`, `osr_callee_declares_handlers`,
  `try_jit_upgrade_with_gate`) while missing the fourth: the `bg_compile`
  promotion in `execute_invokevirtual_cached`, which is the DEFAULT route.
  With that gap closed at the promotion site, the blanket default-off is no
  longer what holds the hazard shut — and it cost ~8.4x on ordinary
  instance-method bytecode. See
  [tomcat/32](../tomcat/32-doc04-residual-perf-assertions-CLOSED.md).

  **Caveat for whoever revisits this:** the verification table below can no
  longer be reproduced. `module/spring-boot-servlet`'s jars date from 07-11 and
  predate `ErrorPageRegistrarBeanPostProcessor`, so all 12
  `MultipartAutoConfigurationTests` cases now fail with `NoClassDefFoundError`
  — **identically with tier-up on and off**, i.e. it is a stale-classpath
  problem, not a VM one. Rebuild that module before using this class as a gate
  for anything.

## Verification

Complete real-JDK Spring Boot fixture, dedicated release executable:

| Mode | MultipartAutoConfigurationTests | SslConnectorCustomizerTests | StaticResourceJarsTests |
|---|---:|---:|---:|
| JIT enabled | 12 passed | 8 passed | 7 passed, 1 skipped |
| `--nojit` | 12 passed | 8 passed | 7 passed, 1 skipped |

Every started test passed; no class had an abort or container failure.
