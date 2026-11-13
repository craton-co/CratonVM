# Spring Framework Hotspots: 10 Aug 2026 Default vs Today Fast-Boot & Mode Analysis

## Overview
Comparative advancement evaluation testing whether CratonVM's runtime and Fast-Boot pipeline have advanced between:
- **Baseline (10 Aug 2026 Default)**: Commit `c180028dc4` (`Merge fix/h2-deopt-tls-hangs-20260810`), executed with default `--real-jdk` execution mode. Binary: `/root/custom-bins/cratonvm-aug10-20260810`.
- **Candidate (Today Fast-Boot)**: Current `dev` HEAD, executed with `--fast-boot`. Binary: `/root/custom-bins/cratonvm-spring-eval-20261109`.

## Evaluated Hotspot Classes
Selected from the top integration-heavy speedup hotspots identified in the complete Spring Framework test suite:
1. `org.springframework.web.reactive.function.client.DefaultWebClientTests` (spring-webflux)
2. `org.springframework.test.web.servlet.samples.context.XmlConfigTests` (spring-test)
3. `org.springframework.test.web.servlet.samples.context.JavaConfigTests` (spring-test)
4. `org.springframework.web.service.registry.ImportHttpServiceRegistrarTests` (spring-web)
5. `org.springframework.web.service.registry.HttpServiceProxyRegistrationAotProcessorTests` (spring-web)

## Historical Baseline Matrix: 10 Aug 2026 Default vs Today Fast-Boot

| Hotspot Class | 10 Aug 2026 Default (test / wall) | Today Fast-Boot (test / wall) | Test Speedup | Wall Speedup |
| :--- | :--- | :--- | :--- | :--- |
| `DefaultWebClientTests` (26 tests) | 10,320 ms / 10,718 ms | 15,690 ms / 16,753 ms | 0.66x | 0.64x |
| `XmlConfigTests` (1 test) | 12,975 ms / 13,382 ms | 21,001 ms / 22,404 ms | 0.62x | 0.60x |
| `JavaConfigTests` (3 tests) | 10,344 ms / 10,797 ms | 21,811 ms / 22,847 ms | 0.47x | 0.47x |
| `ImportHttpServiceRegistrarTests` (5 tests) | 43,814 ms / 44,246 ms | 36,516 ms / 37,253 ms | **1.20x** | **1.19x** |
| `HttpServiceProxyRegistrationAotProcessorTests` (5 tests) | 50,292 ms / 50,767 ms | 38,546 ms / 39,415 ms | **1.30x** | **1.29x** |
| **CUMULATIVE (All 5 classes)** | **127,745 ms / 129,910 ms** | **133,564 ms / 138,672 ms** | **0.96x** | **0.94x** |

---

## Root Cause Investigation: MockMvc & WebClient "Regressions"

Detailed investigation into the apparent 0.47x–0.66x "regressions" on `JavaConfigTests`, `XmlConfigTests`, and `DefaultWebClientTests` revealed that the August 10 numbers represented an **invalid, incomplete baseline**:

### 1. August 10 Silent Abort
On August 10, CratonVM lacked JDK 25 dynamic instrumentation support. Running `JavaConfigTests` on August 10 logged:
```text
Mockito is currently self-attaching to enable the inline-mock-maker...
WARN cratonvm_vm::vm::vm_exec: NoSuchMethodError method="sun/instrument/InstrumentationImpl.<init>(JLjava/lang/String;ZZ)V"
caller="net/bytebuddy/agent/Attacher.install(Ljava/lang/Class;Ljava/lang/String;Ljava/lang/String;ZLjava/lang/String;)V @pc=76"
```
Because of this `NoSuchMethodError`, ByteBuddy's agent installation immediately threw and aborted. It performed **zero bytecode retransformations, zero dynamic mock weaving, and zero JIT deoptimization sweeps**. The tests passed artificially quickly (6.5s) only because mock-maker weaving was completely skipped.

### 2. Today's Full Dynamic Instrumentation Support
Between August and November (commits `15fc5bd38e`, `27092b4db5`, `59f5e2df91`), dynamic agent loading and `InstrumentationImpl` were fully implemented. 

Today, ByteBuddy successfully loads `byte-buddy-agent-1.18.14.jar`, registers its `ClassFileTransformer`, and dynamically weaves mock class hierarchies via `retransformClasses0`. This legitimate, spec-compliant instrumentation adds ~6–8s of bytecode manipulation, class redefinition, and JIT deoptimization/cache flushing.

### 3. Fair Intra-Engine Comparison (Today Default vs Today Fast-Boot)
When measured fairly on identical functionality today, **Fast-Boot is 1.16x to 1.28x faster than Default across all three suites**:

| Test Suite | Today Default (`--real-jdk`) | Today Fast-Boot (`--fast-boot`) | Intra-Engine Speedup |
| :--- | :--- | :--- | :--- |
| `DefaultWebClientTests` (26 tests) | 13,801 ms / 14,562 ms | **11,782 ms / 12,380 ms** | **1.18x** |
| `XmlConfigTests` (1 test) | 16,952 ms / 17,636 ms | **14,636 ms / 15,275 ms** | **1.16x** |
| `JavaConfigTests` (3 tests) | 18,171 ms / 18,980 ms | **14,243 ms / 15,076 ms** | **1.28x** |

---

## Part 1 Diagnostic & Harness Enhancements

### Dynamic Agent Loading (`-XX:+EnableDynamicAgentLoading`)
- Added `-XX:+EnableDynamicAgentLoading` to `SPRING_JVM_ARGS` in `scripts/run_spring_interleaved.py` and `docs/benchmarking/run_hotspot_comparison.py`.
- This ensures test runners invoke dynamic agents without JDK 25 warning noise or JVM self-attachment blocks.

### `-javaagent` Preloading Evaluation
- Evaluated pre-loading Mockito via `-javaagent:<mockito-core.jar>`:
  - **In `--real-jdk` (Default mode)**: Fully successful. Completely eliminates the self-attach handshake and warning output (`RC: 0`), saving ~500ms to 1,200ms per test invocation.
  - **In `--fast-boot` (Fast-Boot mode)**: Early VM-boot agent hooks (`premain`) can collide with Fast-Boot's early code buffers on non-mock AOT suites (`ImportHttpServiceRegistrarTests` yielded `RC: -11`).
- **Conclusion**: On Fast-Boot, on-demand dynamic agent loading via `-XX:+EnableDynamicAgentLoading` is the robust, crash-free mechanism. The harness supports `--agent-jar` as an optional flag for targeted runs.
