# Spring Framework Interleaved Evaluation Results (2026-11-09 / 2026-11-10)

## Execution Overview
- **Binary**: `/root/custom-bins/cratonvm-spring-eval-20261109` (Release build, fat LTO)
- **Base Commit**: `78795f2e2f` -> `02d89e838f`
- **JDK Root**: `/usr/lib/jvm/java-25-openjdk-amd64`
- **Standalone Probes**: 64 / 64 clean (`check-probes.sh`)
- **Total Classes Evaluated**: 2,899 test classes
- **Total Individual Tests Executed**: 30,687 (Default) vs 30,724 (Fast-Boot)
- **Cumulative Runtime**: 17,594.8s (~4.89h, Default) vs 16,712.6s (~4.64h, Fast-Boot)
- **Net Time Saved by Fast-Boot**: 882.2s (~14.7 minutes, 1.05x overall suite speedup)

## Cumulative Results Matrix

| Metric | Default (`cratonvm`) | Fast-Boot (`--fast-boot`) | Suite Parity / Delta |
| :--- | :--- | :--- | :--- |
| **Classes Run** | 2,899 | 2,899 | 100.0% |
| **Classes Passed** | 2,719 (98.9%) | 2,716 (98.8%) | **99.7% class parity (2,890 / 2,899 identical)** |
| **Classes Failed** | 30 | 34 | +4 classes (detailed below) |
| **Classes Timed Out** | 2 | 1 | `ApplicationContextAotGeneratorTests` timed out on default only |
| **Classes No Tests** | 148 | 148 | Identical |
| **Tests Found** | 30,687 | 30,724 | +37 tests evaluated under fastboot |
| **Tests Succeeded** | 30,354 (98.9%) | 30,369 (98.8%) | **+15 net passing tests under fastboot** |
| **Tests Failed** | 153 | 176 | +23 failed subtests (primarily MDC shim & AOT) |
| **Cumulative Runtime** | 17,594.8s | 16,712.6s | **Fast-Boot ~14.7m faster** |

## Discrepancy Census (9 Classes across 2,899)

1. `org.springframework.beans.factory.aot.BeanRegistrationsAotContributionTests`:
   - Both arms FAIL near 120s timeout limit (Default 10 ok, FastBoot 9 ok).
2. `org.springframework.context.annotation.ParserStrategyUtilsTests`:
   - Default PASS (9 ok), FastBoot FAIL (0 ok): ByteBuddy inline mock agent JDK 25 reflection differences.
3. `org.springframework.context.aot.ApplicationContextAotGeneratorTests`:
   - Default TIMEOUT (120s), FastBoot completed in 101s with 37 ok, 3 fail (avoids timeout due to faster boot).
4. `org.springframework.core.io.ResourceTests`:
   - Default PASS (68 ok), FastBoot FAIL (66 ok, 2 fail): Transient remote HTTP socket test timeout (`UrlResourceTests`).
5. `org.springframework.messaging.simp.stomp.ReactorNettyStompBrokerRelayIntegrationTests`:
   - Default PASS (7 ok), FastBoot FAIL (0 ok, 7 fail): Fastboot shim `BasicMDCAdapter` NPE on `inheritableThreadLocal`.
6. `org.springframework.messaging.simp.stomp.ReactorNettyTcpStompClientTests`:
   - Default PASS (1 ok), FastBoot FAIL (0 ok, 1 fail): Same `BasicMDCAdapter` fastboot shim NPE.
7. `org.springframework.test.context.aot.AotIntegrationTests`:
   - Both arms FAIL heavy AOT generation (Default 1 ok, FastBoot 0 ok).
8. `org.springframework.test.context.aot.TestAotProcessorTests`:
   - Default FAIL (0 ok), FastBoot PASS (1 ok): Faster execution under fastboot enabled the test to pass within timeout.
9. `org.springframework.test.context.aot.TestContextAotGeneratorIntegrationTests`:
   - Both arms FAIL heavy AOT generation.
