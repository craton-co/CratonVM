# CratonVM open gaps — index

Collected from cross-VM comparison run and app test suites, 2026-06-09.  
Each file is a self-contained bug report with reproduction steps and a fix direction.

---

**→ See [ALL_OPEN_FAILURES.md](ALL_OPEN_FAILURES.md) for a consolidated prioritized list of all open JVM failures.**

## Gap files

| File | Severity | Affects | Status |
|---|---|---|---|
| [gap-jit-dispatch-exception-wrapping.md](gap-jit-dispatch-exception-wrapping.md) | **Critical** | Any JIT-compiled method that throws — wraps Java exceptions as `InternalError` | Open |
| [gap-nio-basicfileattributes-isdirectory.md](gap-nio-basicfileattributes-isdirectory.md) | High | `Files.walkFileTree` / JUnit Platform classpath scanner / any NIO file walk | **Fixed** (BFA natives promoted to real-JDK mode) |
| [gap-bc-math-ec-crypto-regression-timeout.md](gap-bc-math-ec-crypto-regression-timeout.md) | High | BC `math-ec` TIMEOUT (F2m JIT ban); `crypto-regression` **RESOLVED** (342s, PASS) | math-ec Open |
| [gap-anonymous-object-getinputstream.md](gap-anonymous-object-getinputstream.md) | Medium | Synthetic `Process` had no reachable natives (`getInputStream`/`waitFor`/`isAlive`/...) — root cause was the ClassId(0) alloc fallback, not anonymous-class proxies | **Fixed** (branch `fix/anonymous-object-getinputstream`) |
| [gap-gpu-compute-large-n-crash.md](gap-gpu-compute-large-n-crash.md) | Medium | GPU offload crashes at N=2²⁸ (GpuCompute.heavy) | **Fixed** (dev `85c80dfd`; 1090ms GPU ✅ vs TornadoVM 370ms; GpuProbe CPU still fails N=2²⁸) |
| [gap-jit-ternary-in-loop-increment.md](gap-jit-ternary-in-loop-increment.md) | Medium | Any code using `i += i == 0 ? a : b` (ternary in for-loop step) | **Fixed** (const peepholes no longer fuse across merge points) |
| [gap-jit-canonicalize-operand-clobber.md](gap-jit-canonicalize-operand-clobber.md) | High | Conditional branches with a register-allocated local below frame-resident operands (methods with >7 live int locals); `swap` of two frame slots was a no-op | **Fixed** (alias-safe parallel-move canonicalization, canonicalize-before-pop, swap repair) |
| [gap-bintrees18-gc-throughput.md](gap-bintrees18-gc-throughput.md) | — | bintrees18 throughput deficit — bt18 now `68332206` at ~10.9s (dev `85c80dfd`); remaining ~22× is expected interpreter overhead | **Resolved** |
| [gap-junit5-test-discovery.md](gap-junit5-test-discovery.md) | High | JUnit5 test discovery blocked by 3 bugs: `FileSystemProvider.newFileSystem` no-Code, NIO `Path.exists` wrong on Windows abs paths, NPE in `MethodSelector` reflection | **Fixed** (branch `fix/junit5-test-discovery`; not yet merged) |
| [gap-annotation-retention-policy.md](gap-annotation-retention-policy.md) | Medium | `@Retention(CLASS)` annotations visible at runtime — `RuntimeInvisibleAnnotations` not filtered; `isAnnotationPresent`/`getAnnotation` return wrong results | **Fixed** (2026-06-11; reflection registers only `RuntimeVisibleAnnotations`) |
| [gap-jit-dispatch-exception-wrapping.md](gap-jit-dispatch-exception-wrapping.md) | Critical | JIT-compiled callee exceptions wrapped as `InternalError` instead of propagating | **Fixed** (`handle_jit_dispatch_error` maps Runtime/Linkage/ClassNotFound to real throwables) |
| [gap-phaser-real-bytecode-state.md](gap-phaser-real-bytecode-state.md) | Medium | Real `Phaser` bytecode misexecuted (`state` reads 0, `root` reads null → NPE in `reconcileState`) — a *partial* synthetic native shadow (native-collections) overwrote the real 5-field layout | **Fixed** (2026-06-11; gate `register_phaser_natives` behind `synthetic-jdk`) |

---

## App-specific bug files

| App | File | Open bugs |
|---|---|---|
| WildFly health tests | [apps/wildfly/CRATONVM_BUGS.md](../../apps/wildfly/CRATONVM_BUGS.md) | ~~testSchema~~ FIXED; ~~testSubsystem 300s TIMEOUT~~ **FIXED 2026-06-11** — partial synthetic `EnhancedQueueExecutor` shadow (build() skipped real `<init>` → pool size 0 → no workers → jboss-msc dead) gated behind `CRATONVM_SYNTHETIC_EQE`; `tests=2 failures=0` in 12.5s |
| Elasticsearch 8.15.5 | [apps/elasticsearch-8.15.5/CRATONVM_BUGS.md](../../apps/elasticsearch-8.15.5/CRATONVM_BUGS.md) | ~~Log4j2 NPE + XContent ServiceLoader~~ **BOTH FIXED** (dev `aac45ade`) — elasticsearch-version PASS ✅ |
| Commons Math 4 | [apps/_test-suites/commons-math/CRATONVM_BUGS.md](../../apps/_test-suites/commons-math/CRATONVM_BUGS.md) | JIT dispatch InternalError, BasicFileAttributes NIO gap |
| Bouncy Castle | [apps/_test-suites/bc-java/CLAUDE.md](../../apps/_test-suites/bc-java/CLAUDE.md) | math-ec TIMEOUT (only remaining); crypto-regression PASS (342s) |

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
| bc-crypto-regression | PASS (342s) | PASS | Was TIMEOUT; now completes within 360s limit |
| bc-pqc-crypto | PASS rc=1 | PASS | All Okay lines, rc=1 from cleanup; 41.5s (was 123s) |
| bc-util-encoders | PASS | PASS | rc=127 in harness = orphaned-process fluke (harness issue) |
| commons-math (JUnitProbe) | **PASS** 4/4 | SKIP (not needed) | Both Bugs 1+2 FIXED on dev; harness false-FAIL fixed (FAILED=0 grep) |
| commons-math (ConsoleLauncher) | PASS 4/4 | PASS | BasicFileAttributes gap — **FIXED** (commit 2a7b5121) |
| dacapo-avrora | PASS | N/S | DaCapo validation broken vs JDK25 on all VMs |

---

## 2026-06-09 rerun (CratonVM-run worktree, CPU + GPU mode, dev `f98fbca8`)

Run by `apps/probe/test-infra/run-vm-comparison.sh` with `ROOT=C:/craton/CratonVM-run`.  
4 variants: `cratonvm-cpu`, `cratonvm-gpu`, `hotspot`, `tornadovm`.

### Micro-benchmarks (all OK, all checksums match across 4 variants)

| benchmark | cratonvm-cpu | cratonvm-gpu | hotspot | tornadovm |
|---|---|---|---|---|
| arith1500M | 4,928ms | 4,735ms | 2,553ms | 2,389ms |
| fib44 | 4,547ms | 4,517ms | 3,534ms | 3,369ms |
| sieve250k | 1,639ms | 1,504ms | 968ms | 1,029ms |
| matrix600 | 564ms | 529ms | 273ms | 280ms |
| bintrees18 | 28,020ms ✓ | 28,458ms ✓ | 480ms | 517ms |
| vadd2_28 | **1,609ms** | 1,616ms | 1,293ms | 1,302ms |

> vadd2_28 improved from ~102s (previous session) to 1.6s — likely `analyze_escapes` return-barrier fix.  
> bintrees18 checksum `68332206` = HotSpot ✓; 58× GC-bound slowdown (known).

### GPU direct comparison — GpuCompute.heavy kernel

| N | CratonVM CPU | CratonVM GPU | Speedup | TornadoVM GPU vadd |
|---|---|---|---|---|
| 2²⁰ | 148ms | 10ms | **14.8×** ✓ | 5ms |
| 2²⁴ | — | 78ms | — | 27ms |
| 2²⁸ | ~88s | CRASH | — | 461ms |

### BC suites (2026-06-09 rerun)

| suite | cratonvm | hotspot | tornadovm |
|---|---|---|---|
| asn1-regression | OK 26s | OK 0.9s | OK 1.2s |
| **math-ec** | **TIMEOUT** | OK 30s | OK 29s |
| math-raw | OK 1.5s | OK 0.3s | OK 0.3s |
| math | OK 5.2s | OK 0.6s | OK 0.6s |
| crypto-regression | **OK 342s** ← was TIMEOUT | OK 127s | OK 120s |
| crypto-prng | OK 19.7s | OK 0.5s | OK 0.5s |
| pqc-crypto | OK 41.5s | OK 1.2s | OK 1.2s |
| util-encoders | FAIL rc=127 (harness) | OK | OK |

### Commons Math full reactor

| VM | tests | fail | wall | result |
|---|---|---|---|---|
| hotspot | 3,204 | 0 | 88s | PASS |
| tornadovm | 3,204 | 0 | 128s | PASS |
| cratonvm | — | — | 61s | CRASH |

---

---

## 2026-06-10 app suite run — full summary

Run by `test-infra/run-all-apps-suites.sh` with dev `f98fbca8`.  
Logs: `test-infra/suite-results/apps-all-20260609-222654/`

| Suite | CratonVM | HotSpot | Status |
|---|---|---|---|
| h2-testall-fast | PASS (1.2s) | PASS | ✓ |
| wildfly-health | FAIL (4.4s) | SKIP | Bug 7 + Bug 8/9 |
| elasticsearch-version | FAIL (3.6s) | SKIP | Bug 1 + Bug 2 |
| gpu-bench-cpu | PASS (1.2s) | PASS | ✓ |
| gpu-offload-probe | PASS (1.5s) | PASS | ✓ |
| bc-asn1-regression | PASS (13.6s) | PASS | ✓ |
| bc-crypto-prng | PASS (45.4s) | PASS | ✓ |
| commons-math-junit-probe | PASS (5.1s) | SKIP | ✓ (harness false-FAIL fixed) |
| dacapo-avrora | PASS (6.9s) | N/S | DaCapo/JDK25 compat artifact |
| pool:kafka-codec | PASS | — | ✓ |
| pool:spring-boot-run | PASS | — | ✓ |
| pool:tomcat-server-info | PASS | — | ✓ |

**No new JVM bugs** found vs 2026-06-09 baseline. All failures are pre-existing open bugs.

---

---

## 2026-06-10 app suite run — full summary (after WildFly MSC merge, dev `85f6799b`)

CPU binary rebuilt Jun 10 00:36, GPU binary rebuilt Jun 10 00:49.  
Logs: `test-infra/suite-results/apps-all-20260610-005325/`

| Suite | CratonVM | HotSpot | Wall time (CV/HS) | Notes |
|---|---|---|---|---|
| h2-testall-fast | PASS | PASS | 1.3s / 0.4s | 3.3× |
| wildfly-health | **PARTIAL** | SKIP | 4.3s | testSchema ✅ **FIXED** — testSubsystem still fails |
| elasticsearch-version | FAIL | SKIP | 3.5s | 2 pre-existing bugs unchanged |
| gpu-bench-cpu | PASS | PASS | 1.3s / 0.2s | 6.5× |
| gpu-offload-probe | PASS | PASS | 1.6s / 0.2s | 8× |
| bc-asn1-regression | PASS | PASS | 25.5s / 0.8s | 31.9× |
| bc-crypto-prng | PASS | PASS | 32.3s / 0.5s | 64.6× |
| commons-math-junit-probe | PASS | PASS† | 4.9s / 0.5s | 9.8× |
| dacapo-avrora | PASS | N/S | 5.5s / — | DaCapo/JDK25 compat |
| pool:kafka-codec/spring-boot-run/tomcat | PASS | REGRESS* | | ✓ |

† HotSpot JUnitProbe discovers 0 tests (classpath scan difference); CratonVM 4/4 is correct.  
\* pool:* REGRESS = HotSpot baselines not set up; expected.

**New vs 2026-06-09:** wildfly testSchema FIXED. No new regressions.

---

## 2026-06-10 GPU comparison — CratonVM vs TornadoVM (RTX 2060, GpuCompute.heavy 96×multiply-add)

Full results: `test-infra/suite-results/gpu-comparison-20260610-005542.md`  
Fresh GPU binary rebuilt Jun 10 00:49.

| N | CratonVM CPU | CratonVM GPU | HotSpot CPU | TornadoVM GPU |
|---|---|---|---|---|
| 2²⁰ (1M)  | 149ms | 10ms **(14.9× CV-CPU)** | 21ms | 2ms (10.5× HS) |
| 2²² (4M)  | ~618ms | 24ms **(25.8× CV-CPU)** | 21ms | 5ms (4.2× HS) |
| 2²⁴ (16M) | 2369ms | 78ms **(30.4× CV-CPU)** | 28ms | 24ms (1.2× HS) |
| 2²⁶ (64M) | 9884ms | 330ms **(29.9× CV-CPU)** | 58ms | 87ms (0.7× HS) |

CratonVM GPU still crashes for GpuProbe (3-kernel) at N=2^26; GpuCompute.heavy stable to 2^26; always crashes at 2^28.  
All checksums match HotSpot ✓. TornadoVM GPU advantage over HotSpot vanishes above 2^24 (memory-bandwidth wall).

---

## Fix priority order

1. **JIT dispatch exception wrapping** (`gap-jit-dispatch-exception-wrapping.md`) — general correctness regression, affects any JIT-compiled method
2. **`BasicFileAttributes.isDirectory()`** (`gap-nio-basicfileattributes-isdirectory.md`) — blocks file tree walking, test discovery, classpath scanning
3. **WildFly testSubsystem ISE** — fix stack-trace capture (Bug 9) first, then diagnose the ISE root cause
4. **WildFly testSchema Premature EOF** — JAXP XML validation gap
5. **Elasticsearch Log4j2 array reflection NPE** — reflection argument marshaling for array-typed parameters
6. **Elasticsearch XContent ServiceLoader** — custom classloader ServiceLoader gap
7. **BC math-ec / crypto-regression timeout** — JIT-ban `Interleave.expand64To128` + AES native fast-path
