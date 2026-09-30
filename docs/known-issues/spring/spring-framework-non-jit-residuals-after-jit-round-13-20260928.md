# Spring Framework: failures that remain after JIT round 13, all reproducing under `--nojit`

Status: OPEN
Area: interpreter / runtime / class library (not the JIT): networking (`RestTemplate`, `RestClient`, SSE, reactive connectors), `java.beans` introspection, annotation-driven bean post-processing, `ServiceLoader`, serialization, JMX proxies
Severity: MEDIUM (test-suite failures in Spring Framework modules; no crash)
Found by: JIT round 13 orchestrator, 2026-09-28. Re-run of the Spring census that
`docs/internal/fixed-bugs/spring-framework-and-boot-jit-failures-and-crashes-FIXED-20260928.md` recorded.

## What was run

Linux host, round-13 wave-6 build (`cratonvm-jitr13-w6`, release, no LTO, default `--jdk-only`),
JDK 25.0.4 class library, one JUnit launcher run per class via `/data/jitr13-spring/one.sh`.
Each class ran twice, once with the JIT at default and once with `--nojit`, with the same argfile.
Every class below gives the **same** found / succeeded / failed counts in both arms. So it is not a
JIT defect, and the JIT pages no longer carry it.

| class | module | found | failed (JIT / `--nojit`) | note |
|---|---|---:|---|---|
| `RestTemplateIntegrationTests` | spring-web | 125 | 15 / 15 | times out (900 s) after the failures |
| `RestClientIntegrationTests` | spring-web | 230 | 6 / 5 | times out; the one extra JIT failure did not repeat as a pattern |
| `SseIntegrationTests` | spring-webflux | 48 | 12 / 12 | times out |
| `ClientHttpConnectorTests` | spring-web | 49 | 4 / 4 | times out (400 s) |
| `ZeroCopyIntegrationTests` | spring-webflux | 4 | 4 / 4 | |
| `BeanUtilsTests` | spring-beans | 101 | 1 / 1 | |
| `PropertyDescriptorUtilsPropertyResolutionTests` | spring-beans | 44 | 41 / 41 | `--nojit` takes 137 s against 49 s |
| `InitDestroyAnnotationBeanPostProcessorTests` | spring-beans | 8 | 1 / 1 | |
| `QualifierAnnotationAutowireCandidateResolverTests` | spring-beans | 7 | 7 / 7 | |
| `ServiceLoaderTests` | spring-beans | 3 | 1 / 1 | |
| `MultiValueMapTests` | spring-core | 56 | 1 / 1 | |
| `SerializationUtilsTests` | spring-core | 9 | 1 / 1 | |
| `ScheduledAnnotationBeanPostProcessorTests` | spring-context | 45 | 18 / 18 | |
| `MBeanClientInterceptorTests` | spring-context | 14 | 12 / 12 | |

The earlier census (see the retired page's Part 3) ran the networking classes under `--compatible`,
where they passed in 7 to 27 s. That suggests the strict-mode network and reflection paths
(`--jdk-only`) as the first suspect for the first four rows.

## How to pick one up

1. Run the class under `--nojit` with `--stack-dump-on-timeout 0` and read the first failing test's
   cause chain in the JUnit report (the census recorded counts only, not causes).
2. Compare it against `--compatible --nojit`. If `--compatible` passes, the defect is a strict-mode
   (`--jdk-only`) gap: fix it through `resolve_dispatch` / `retired_shadow.rs`, never with an
   allow-list (AGENTS.md).
3. Put each root cause on its own page and link it from here. Retire this page when the table is empty.

Drivers: `/data/jitr13-spring/one.sh <tag> <module> <class> <label> [vm args]`. The class list is
`/data/jitr13-spring/part3.tsv`, and the outputs are under `/data/jitr13-spring/one/`.
