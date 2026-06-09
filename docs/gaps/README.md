# CratonVM open gaps — index

Collected from cross-VM comparison run and app test suites, 2026-06-09.  
Each file is a self-contained bug report with reproduction steps and a fix direction.

---

## Gap files

| File | Severity | Affects | Status |
|---|---|---|---|
| [gap-jit-dispatch-exception-wrapping.md](gap-jit-dispatch-exception-wrapping.md) | **Critical** | Any JIT-compiled method that throws — wraps Java exceptions as `InternalError` | Open |
| [gap-nio-basicfileattributes-isdirectory.md](gap-nio-basicfileattributes-isdirectory.md) | High | `Files.walkFileTree` / JUnit Platform classpath scanner / any NIO file walk | **Fixed** (BFA natives promoted to real-JDK mode) |
| [gap-bc-math-ec-crypto-regression-timeout.md](gap-bc-math-ec-crypto-regression-timeout.md) | High | BC `math-ec` (F2m EC), BC `crypto-regression` (AES/RSA slow) | Open (pqc-crypto fixed) |
| [gap-anonymous-object-getinputstream.md](gap-anonymous-object-getinputstream.md) | Medium | Anonymous class synthetic proxies missing `getInputStream()` | Open (non-fatal) |

---

## App-specific bug files

| App | File | Open bugs |
|---|---|---|
| WildFly health tests | [apps/wildfly/CRATONVM_BUGS.md](../../apps/wildfly/CRATONVM_BUGS.md) | testSchema (Premature EOF), testSubsystem (ISE no-message), stack-trace loss (meta-blocker) |
| Elasticsearch 8.15.5 | [apps/elasticsearch-8.15.5/CRATONVM_BUGS.md](../../apps/elasticsearch-8.15.5/CRATONVM_BUGS.md) | Log4j2 NPE on array reflection param, XContent ServiceLoader gap |
| Commons Math 4 | [apps/_test-suites/commons-math/CRATONVM_BUGS.md](../../apps/_test-suites/commons-math/CRATONVM_BUGS.md) | JIT dispatch InternalError, BasicFileAttributes NIO gap |
| Bouncy Castle | [apps/_test-suites/bc-java/CLAUDE.md](../../apps/_test-suites/bc-java/CLAUDE.md) | math-ec TIMEOUT, crypto-regression TIMEOUT (see gap-bc-math-ec) |

---

## 2026-06-09 app test run — full summary

Run by `test-infra/run-all-apps-suites.sh` + `test-infra/run-comparison-full.sh`.

| Suite | CratonVM | HotSpot | Notes |
|---|---|---|---|
| h2-testall-fast | PASS | PASS | |
| wildfly-health | FAIL | SKIP | testSchema (Premature EOF) + testSubsystem (ISE) |
| elasticsearch -V | FAIL rc=70 | SKIP | Log4j2 NPE + XContent ServiceLoader |
| gpu-bench-cpu | PASS | PASS | |
| gpu-offload-probe | PASS | PASS | |
| bc-asn1-regression | PASS | PASS | 64.7s CratonVM vs 1.4s HS |
| bc-crypto-prng | PASS | PASS | 36.5s CratonVM vs 0.6s HS |
| bc-math-ec | TIMEOUT | PASS | F2m JIT ban |
| bc-math-raw | PASS | PASS | |
| bc-math | PASS | PASS | |
| bc-crypto-regression | TIMEOUT | PASS | AES/RSA slow |
| bc-pqc-crypto | PASS rc=1 | PASS | All Okay lines, rc=1 from cleanup |
| bc-util-encoders | PASS | PASS | (rc=127 in comparison harness was orphan-process fluke) |
| commons-math (JUnitProbe) | FAIL | SKIP | JIT dispatch InternalError |
| commons-math (ConsoleLauncher) | FAIL | FAIL (classpath) | BasicFileAttributes gap (CratonVM) / missing jar (HS harness) |
| dacapo-avrora | PASS | N/S | DaCapo validation broken vs JDK25 on all VMs |

---

## Fix priority order

1. **JIT dispatch exception wrapping** (`gap-jit-dispatch-exception-wrapping.md`) — general correctness regression, affects any JIT-compiled method
2. **`BasicFileAttributes.isDirectory()`** (`gap-nio-basicfileattributes-isdirectory.md`) — blocks file tree walking, test discovery, classpath scanning
3. **WildFly testSubsystem ISE** — fix stack-trace capture (Bug 9) first, then diagnose the ISE root cause
4. **WildFly testSchema Premature EOF** — JAXP XML validation gap
5. **Elasticsearch Log4j2 array reflection NPE** — reflection argument marshaling for array-typed parameters
6. **Elasticsearch XContent ServiceLoader** — custom classloader ServiceLoader gap
7. **BC math-ec / crypto-regression timeout** — JIT-ban `Interleave.expand64To128` + AES native fast-path
