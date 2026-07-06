# Known issues — index & bug map

This folder collects CratonVM-only defects found while running upstream Java
suites. The docs had grown to describe the **same underlying bug from several
angles**; this index is the consolidated map. Read it first.

## 2026-07-05 Hibernate pruned residuals

- [Hibernate JpaLargeBlobTest Object.read() dispatch](hib-jpalargeblobtest-object-read-nosuchmethod.md) - patched locally in the JIT virtual/interface MIC helper: `ClassId(0)` non-Object receivers now fall back to the CP owner and cannot publish MIC/PIC entries under the zero/empty sentinel. Local Windows Hibernate probe no longer reproduces `java/lang/Object.read()I`, but the class remains open on a later no-JIT-independent `GenHeap::set_array_element` object-vs-array assertion in H2 `IOUtils.readFully`.

## 2026-07-04 test.context.* cluster (bean/groovy/junit/junit4/testng/web, branch fix/test-context-cluster)

- [Constructor-parameter-annotation offset fix + 3 residuals](test-context-constructor-param-annotation-offset.md) — core fix: `Constructor.getParameterAnnotations()`'s native override didn't account for a synthetic leading parameter (non-static inner-class constructors' implicit outer-instance arg), causing an AIOOBE that crashed 13 of 25 CV-unique classes across the six packages (7 directly + cascading ABEND in 6 more sharing a batch). 3 residuals open: Groovy TestContext script loading (isPresent stub blocks it; removing the stub exposes a separate ANTLR/jarjar class-layout bug — not fixed), `InheritableThreadLocal` not propagated through `Thread(ThreadGroup, Runnable, String[, long])` constructors (breaks `Executors`-backed pools generally, not just these tests), and one JUnit5-parallel-execution TIMEOUT not yet triaged.


## 2026-07-04 http.server bug cluster (branch fix/http-server-cluster)

- [http.server cluster: fixes + residuals](http-server-cluster-residuals.md) - core fix: `Collections.emptyListIterator()` mis-stamped as `EmptyIterator` crashed Jetty's `ContextHandler.notifyExitScope` on the main thread (fatal, not per-test) — fixed 8 of 9 ABEND classes. Plus `newSetFromMap(LinkedCaseInsensitiveMap)` losing case-insensitivity (+ a `SetFromMap.size()` follow-on regression), `java.net.URI` malformed-`%`-escape validation + multi-arg query quoting, and `URLDecoder`/`URLEncoder` ignoring non-UTF-8 charsets. 2 residuals open: `ServerHttpsRequestIntegrationTests` self-signed-cert `CertificateEncodingException`, `ZeroCopyIntegrationTests` Jetty-Core zero-copy write-0-bytes.

## 2026-07-04 Keycloak full-suite sweep (branch test/keycloak-fullsuite-20260704)

Ran the 1124 Keycloak JUnit classes not covered by the prior 238-class baseline
(39 compiled modules, 1362 concrete classes total) - see the harness fix in
`apps/keycloak-suite-runner/run-keycloak-suite.ps1` (JUnit Platform
launcher/engines + `junit:junit` were missing from every module's classpath;
KcRunner always drives tests through the JUnit Platform Launcher regardless of
whether the module declares JUnit5). Result: 28 PASS, 910 FAIL, 71 CRASH, 115
EMPTY, 0 HANG, wall time 2469s (41 min) at parallel=2. All 981 FAIL+CRASH rows
were exhaustively bucketed by exact terminal-error signature (not sampled); see
`keycloak-07-04/` for every distinct remaining finding.

Fixed from this sweep:
- [crypto/fips1402 CryptoProvider ServiceLoader bootstrap](../internal/fixed-suite-bugs/keycloak-crypto-fips1402-cryptoprovider-serviceloader.md) - `ServiceLoader` now sees the FIPS `CryptoProvider`; representative classes no longer fail at `CryptoInitRule.before` with `containersFailed=1`. Residual: the module can still fail later after bootstrap (`CryptoIntegration.getProvider(): init first`).
- [SmallRyeConfig.getConfigMapping(Class) 1-arg bare-interface AbstractMethodError](../internal/fixed-suite-bugs/smallrye-getconfigmapping-1arg-bare-interface-abstractmethoderror.md).
- [SmallRye Config missing Charset/MemorySize converters](../internal/fixed-suite-bugs/keycloak-smallrye-config-charset-memorysize-converters.md) - `LoggingSetupRecorder.handleFailedStart()` now builds its transient logging config with discovered Quarkus converters. The local 2026-07-04 deep dive also showed this fix exposes a later test-framework/Maven-artifact resolution gap rather than unlocking all `tests/base` classes outright. Residual: [test-framework deployRequestedInstances resolution failure](keycloak-07-04/keycloak-testframework-deploy-requested-instances-resolution.md).
- [quarkus/runtime CompactValue NaN-box collision SIGSEGV](../internal/fixed-suite-bugs/keycloak-quarkus-compactvalue-nanbox-sigsegv.md) - current `dev` no longer reproduces `rc=139`. The post-crash PicocliTest timeout residual is also fixed in [PicocliTest post-CompactValue-fix hang](../internal/fixed-suite-bugs/quarkus-runtime-picocli-post-compactvalue-hang.md).
- [System Rules getenv() field 'm' reflection mismatch](../internal/fixed-suite-bugs/keycloak-system-rules-getenv-field-m-reflection.md) - `System.getenv()` now exposes an OpenJDK-shaped unmodifiable map wrapper whose private `m` field points at the backing map.
- [KcAdmV2HelpTest --help text env-var mentions](../internal/keycloak-07-04/kcadmv2-helptext-env-var-mentions.md) - synthetic `BreakIterator.getLineInstance()` now returns Java UTF-16 text offsets after complete whitespace/hyphen runs, so Picocli no longer hard-wraps `KC_CLI_*` env-var names.
- [IgnoredArtifactsTest.multipleDatasources datasource properties missing](../internal/fixed-suite-bugs/quarkus-runtime-ignoredartifacts-multipledatasources-boolean.md) - module-scoped runner working directories now match Keycloak's relative `src/test/resources` setup, and Windows `File.toPath().toUri()` no longer emits `%5C` backslash URIs.
- [LoggingConfigurationTest getPropertyNames stale log-category key](../internal/fixed-suite-bugs/quarkus-runtime-logging-getpropertynames-garbage-key.md) - `System.setProperties(Properties)` now replaces the VM-wide properties state, so stale fixture keys do not leak into SmallRye property-name iteration.
- [LoggingConfigurationTest wildcard DEBUG level resolves null](../internal/fixed-suite-bugs/quarkus-runtime-logging-wildcard-debug-level-null.md) - fixed by the same System-properties replacement semantics as the stale log-category key.
- [TelemetryConfigurationTest telemetry-service-name wrong value](../internal/fixed-suite-bugs/quarkus-runtime-telemetry-service-name-wrong-value.md) - fixed by System-properties replacement/reset semantics.

Open findings from this sweep, in `keycloak-07-04/`, roughly by priority:
- [test-framework deployRequestedInstances resolution failure](keycloak-07-04/keycloak-testframework-deploy-requested-instances-resolution.md) - surfaced after the converter fix.

Already-tracked, not re-documented: the 37 `testsuite/model` CRASHes are the
existing [Infinispan GlobalConfigurationBuilder.isClustered() NoSuchMethodError](keycloak-model-infinispan-globalconfiguration-isclustered-nosuchmethod.md).
Not CratonVM bugs: 543 FAILs (`testsuite/integration-arquillian/tests/base`
+ `tests/other/sssd`, exhaustively confirmed - 543/544 exact match, the 544th
is the System Rules finding above) are "Not found frontend container:
auth-server-undertow" - an Arquillian environment/container-provisioning gap
in this harness, not a VM defect (would fail identically on real HotSpot run
the same way). 25 additional CRASHes (`scim/core`, `ssf/*`,
`test-framework/*`, `tests/webauthn`, 2x`tests/clustering`) were a harness
classpath gap (missing `junit:junit`, fixed alongside the JUnit Platform
launcher fix above), not a VM bug.

## 2026-07-03 http.client bug cluster (branch fix/http-client-cluster-azure)

- [http.client cluster: redefine-dispatch fix + JDK 21 gaps](http-client-cluster-redefine-dispatch-and-jdk21-gaps.md) - core fix: two of three "native shadow" dispatch paths never checked whether an ANCESTOR class (not just the receiver) was JVMTI-redefined, so Mockito-mocked concrete classes (e.g. `HttpURLConnection`) silently bypassed their own advice. Plus several JDK 21 real-mode native gaps (`JavaLangAccess.defineClass`/`getConstantPool`/`start`, `StackWalker.callStackWalk` overload). 4 residuals documented (Linux-only NIO gaps, a 4-class hang, one order-dependent Mockito state leak, pre-existing `SimpleClientHttpRequestFactoryTests` gaps).

## 2026-07-04 jmx cluster (branch fix/jmx-rmi-cluster)

- FIXED: `sun/nio/ch/FileDispatcherImpl.init0()V` was registered under the
  wrong native name (`"init"` instead of the real JDK 25 `init0`), so any
  bytecode path touching `FileDispatcherImpl` (e.g.
  `ManagementFactory.getPlatformMBeanServer()` on Linux) hit
  `UnsatisfiedLinkError`, aborting `MBeanClientInterceptorTests`,
  `RemoteMBeanClientInterceptorTests`, `JmxUtilsTests`,
  `MBeanServerFactoryBeanTests`. Also landed in this cluster:
  `jdk/internal/platform/CgroupMetrics.isUseContainerSupport()Z` (previously
  unregistered, also on the `getPlatformMBeanServer()` boot path), and a
  `JMXConnectorFactory.newJMXConnector` improvement that delegates to real
  classpath-declared `JMXConnectorProvider`s (via `ServiceLoader`) before
  falling back to the "not implemented" IOException — covers `jmxmp`
  end-to-end with real provider bytecode instead of a canned error.
- FIXED (branch fix/jmx-platform-mxbean-registration): platform MXBeans
  (Memory, Threading, etc.) were never actually registered onto the real
  MBeanServer returned by `ManagementFactory.getPlatformMBeanServer()` —
  the real-bytecode registration loop aborted partway through on the two
  native gaps above. With those fixed, registration completes, but a
  second gap surfaced: synthetic `com/sun/jmx/mbeanserver/MXBeanMapping`
  instances never had `toOpenValue`/`fromOpenValue` implemented, so any
  real attribute value needing OpenType conversion (e.g.
  `MemoryMXBean.getHeapMemoryUsage()`) hit `AbstractMethodError`. Fixed as
  an identity passthrough (correct here since our mappings only ever
  round-trip within the same in-process MBeanServer call). Also fixed
  `MemoryUsage.max` for the heap pool to report the real `-Xmx` instead of
  the `-1` unavailable sentinel.
- OPEN residual: [getThreadInfo(long) operation-signature mismatch](spring-jmx-getthreadinfo-operation-signature-mismatch.md)
  — 2 of the original 4 test methods still fail: `mxBeanOperationAccess()`
  on a JMX operation-signature-matching gap for overloaded native methods,
  unrelated to the registration/marshalling fixes above. jmx.* suite is now
  319/321 passing (up from the original registration failure blocking all
  platform MXBean access).

## Bug-document lifecycle

Every unresolved bug document belongs under `docs/known-issues`. Once the bug
is fixed, resolved, or refuted, move the write-up out of this folder and archive
it under `docs/internal`.

## 2026-07-04 OSR default flip / archived residuals

- `CRATONVM_JIT_OSR` now defaults on in `vm/src/runtime/env_cache.rs`; set
  `CRATONVM_JIT_OSR=0` for the old behavior during diagnosis. The historical
  OSR blocker note moved to
  [`docs/internal/fixed-suite-bugs/jit-osr-backedge-value-corruption-cluster.md`](../internal/fixed-suite-bugs/jit-osr-backedge-value-corruption-cluster.md).
- The G1 parallel-evac forwarding/root-remap note moved to
  [`docs/internal/fixed-suite-bugs/g1-parallel-evac-persistent-forwarding-root-remap.md`](../internal/fixed-suite-bugs/g1-parallel-evac-persistent-forwarding-root-remap.md)
  because its own current evidence says both bugs are fixed and soak-verified.
- The XT takeover activation corruption note moved to
  [`docs/internal/fixed-suite-bugs/xt-takeover-activation-young-corruption.md`](../internal/fixed-suite-bugs/xt-takeover-activation-young-corruption.md).
  Its remaining 1/18 DoHead crash face is not an XT activation residual and stays
  tracked by
  [`dohead-jit-heap-corruption-register-invisibility.md`](dohead-jit-heap-corruption-register-invisibility.md).

## How many distinct bugs are here?

After consolidation (full re-count 2026-06-18, kafka-bug-B/C status reconciled 2026-07-01),
the ~30 docs map to **one root-cause family + ~16 distinct standalone bugs**, of which
**10 are already FIXED on `dev`** (the prior 9 plus the Lucene104 provider
initialization gap). Headline:

**~8 distinct OPEN defects + 1 latent** (was ~9 — the Lucene104 provider
initialization gap was fixed 2026-07-02), grouped as:

1. **Family A — GC root coverage under JIT** (one root cause, several manifestations). Open members:
   **A4** (Fork6 FJP multi-thread, gated) — the last open member. **A1/A2/A3/A5 are FIXED** — **A2**
   (`ReflRepro`) was re-diagnosed and fixed 2026-06-23 (`6e3ddb05`): it was **never** a
   register/native-return missed root, but a GC-side non-moving-sweep free-list double-serve
   (overlapping free blocks not coalesced → `Arena::alloc` served the same region twice); A5 was
   root-caused to the **compiled entry-point
   `main`'s JIT frame being unregistered** (invoked via `Vm::invoke` without a `JitEntryGuard`, so
   the moving young collector relocated its roots); fixed by detecting an unregistered JIT frame on
   the native stack → non-moving sweep + full-stack mark (dev `77c98761`; writeup moved to
   [`docs/internal/app-jvm-bugs/gc-stress-bintrees-main-args-unregistered-jit-frame-FIXED.md`](../internal/app-jvm-bugs/gc-stress-bintrees-main-args-unregistered-jit-frame-FIXED.md)).
   `spring-bug-10` is a Family-A manifestation seen from
   the Spring suite (same root cause, different entry point); `springsuite-bug-04` was the same race
   but **no longer reproduces** (doc removed). The suite-scale field evidence in
   `jit-junit-discovery-reflection-corruption.md` is the same race.
   **Parked-thread deposit member FIXED 2026-07-02**: the blocked-path
   `deposit_root_snapshot` served a stale `JIT_SCAN_CACHE` snapshot (missing
   `invalidate_scan_cache_for_gc()`), so a JIT'd `AQS.await` frame's freshly
   inline-allocated `ConditionNode` was never pinned and selective promotion
   moved it under the parked thread — the 100%-reproducible
   `ConcurrencyThrottleInterceptorTests` TIMEOUT wedge. Writeup:
   [`docs/internal/fixed-suite-bugs/throttle-park-deposit-stale-jit-scan-cache.md`](../internal/fixed-suite-bugs/throttle-park-deposit-stale-jit-scan-cache.md).
2. **Standalone B** — JUnit `@Timeout` interceptor double-`proceed()` (open).
3. **Standalone C** — deep JIT→JIT recursion native-stack overflow (latent; stack-bang containment landed; resumable fault recovery still open).
4. ~~**bug-06 F5**~~ — reflection native returns null vs a `Class`/`Method`: **✅ CLOSED 2026-07-02,
   failcause extinct** — 0 instances in the clean 2026-06-30/07-01 full re-runs and in a fresh
   196-class nojit+jit sweep on dev `ffb247e5`; it was a cross-family cascade whose sources
   (fam1/3/4, `toArray` recursion, bug-04 GC, bug-05 generics) are all fixed (doc moved to
   [`docs/internal/fixed-suite-bugs/bug06-fam5-reflection-getdeclaredmethod-null.md`](../internal/fixed-suite-bugs/bug06-fam5-reflection-getdeclaredmethod-null.md)).
5. **bug-06 F6 / `spring-bug-06` / `spring-bug-01`** — annotation **synthesis** value-mismatches.
   (The `MergedAnnotations` **hang** + the `AnnotationUtilsTests` "~2 GB OOM" in this cluster were the
   `toArray` self-recursion and are **FIXED** 2026-06-20, commit `8795b88d`; `MergedAnnotationsTests`
   now runs 174/178, `AnnotationUtilsTests` 72/72. Only the synthesis value-mismatches remain open.)
6. **`spring-bug-08`** — serializable JDK-proxy round-trip (open).
7. **`spring-bug-11` residual** — Groovy hang at `BEGIN` (the SIGSEGV half is FIXED via bug-12; open).
8. ~~**`kafka-bug-B`**~~ — Mockito `mockStatic` + `mock`/`mockConstruction` dispatch:
   **FIXED on `dev`**. The stale JIT call-site residual was fixed by redef-time compiled
   dispatch quiescing; archived at
   [`docs/internal/fixed-suite-bugs/kafka-bug-B-mockstatic-capturing-lambda-jit.md`](../internal/fixed-suite-bugs/kafka-bug-B-mockstatic-capturing-lambda-jit.md).
9. ~~**`kafka-bug-C`**~~ — `WeakHashMap.values().stream()` infinite hang: **FIXED on `dev`** (`1cd0ab26`; doc removed).
10. **Hibernate JTA** (Narayana) — ✅ **RESOLVED 2026-06-20** (docs → [`docs/internal/`](../internal/)).
    L0 fixed on dev; **L1 was never broken (refuted)**; **L2 = an `accept()` deadlock** (global `s2_registry`
    lock held across blocking `accept()`) + `getLocalPort()==0`, both fixed on branch `fix/hib-jta-xa-loopback`
    (`e0426050`). A full Hibernate 8.1 + Narayana 7.3.4 + H2 begin/persist/commit/truncate cycle now passes in
    default (synthetic-socket) mode.
11. **Hibernate JAXB/ByteBuddy bootstrap slow** — ✅ **RESOLVED / does-not-reproduce 2026-06-20**
    (doc → [`docs/internal/`](../internal/)). Class-load storm fixed on dev; ByteBuddy `MethodGraph`
    JoinedSubclass bootstrap completes ~16s `--nojit`.
12. **Hibernate deserialized-`SessionFactory`-null** (`SessionFactoryRegistry` reconnect; open).
13. **ES-HANG-02 — ✅ RESOLVED 2026-06-20** (`fix/es-restclient-gc-safety`; docs moved to
    [`docs/internal/elasticsearch-suite/`](../internal/elasticsearch-suite/)). The RestClient
    embedded-HTTP-server hang AND both residuals are fixed: residual 1 (real non-blocking connect) +
    residual 2 — which was **NOT throughput** (the prior handoff's theory) but **three GC-correctness bugs**:
    a safepoint-resume monitor/native-root remap gap (`IllegalMonitorStateException`), an unrooted synthetic
    `com.sun.net.httpserver` handler ref (`NoSuchMethodError java/lang/Object.handle` storm), and
    per-request native read-alloc-use staleness. Both ES RestClient suites are green at `-Xmx1g` with
    `CRATONVM_ROOTSNAP_CACHE=0` (single-host 22/22, multi-host 4/4) — see the two new GC defects #15/#16.
    Former siblings also resolved: **ES-HANG-01** (`1cd0ab26`), **ES-FAIL-03**, **ES-FAIL-04**.
14. **Spring-suite sweep (2026-06-19 → re-verified 2026-06-20).** Status after running the suite
    through the JUnit-Platform `KRun` harness on a fresh dev build (`cratonvm-spring0620`, off
    `697134f8`):
    - ✅ **toArray-recursion FIXED** (commit `8795b88d`, → dev) — `ReferencePipeline.toArray(IntFunction)`
      was shadowed by a native that re-entered no-arg `toArray()` → `StackOverflowError`. This was the
      open **`MergedAnnotations` hang** AND **bug-06 fam6 "~2 GB OOM"**, and it blocked the *entire*
      JUnit launcher (no test class could run). Now: `MergedAnnotationsTests` 174/178,
      `AnnotationUtilsTests` 72/72, `AnnotatedElementUtilsTests` 82/82. Doc:
      [`docs/internal/fixed-suite-bugs/springsuite-0620-toarray-referencepipeline-recursion.md`](../internal/fixed-suite-bugs/springsuite-0620-toarray-referencepipeline-recursion.md).
    - ✅ **Unsafe off-heap DirectBuffer (bug-A + bug-A2) FULLY RESOLVED** — `PooledDataBufferTests`
      **10/10**, `LeakAwareDataBufferFactoryTests` 2/2. The bug-A2 Netty `refCnt` AIOOBE no longer
      reproduces. Archived → [`docs/internal/fixed-suite-bugs/springsuite-0619-unsafe-offheap-directbuffer.md`](../internal/fixed-suite-bugs/springsuite-0619-unsafe-offheap-directbuffer.md).
    - ✅ **getBeanClassName bean-filter (bug-B) FIXED** — primary filter fix holds, and **bug-B2
      (CGLIB method-injection) is fully implemented 2026-06-21** (commit `ffa71253`): the instantiate
      shim synthesises a concrete subclass overriding each abstract `<lookup-method>`/`@Lookup` method
      via `bf.getBean(name|args)` (NullBean→null), `bf.getBeanProvider(ResolvableType.forMethodReturnType(m)).getObject()`
      for generic by-type, with overload-aware + child-precedence override matching.
      **`LookupMethodTests` 0/7 → 7/7** (JIT and `--nojit`), **`LookupAnnotationTests` 0/10 → 10/10**.
      (The "flaky JIT `invokeVoid`" turned out to be a one-byte emitter typo — `IFEQ` vs `IFNULL` —
      not a VM defect.) [bug-B / bug-B2 doc](../internal/spring/springsuite-0619-getbeanclassname-bean-filter.md) — ✅ **FIXED on dev** (`6d596473`; moved to docs/internal).
    - Still untriaged from the sweep: `ReactiveAdapterRegistry$MutinyRegistrar` NCDFE, XML
      "Unexpected failure during bean definition parsing", "Unnamed bean definition", spring-jdbc
      mass-TIMEOUT, scheduler `StringIndexOutOfBounds`, and `DataBufferUtilsTests` TIMEOUT
      (heavy-reactive). (The prior `springsuite-0619-open-candidates.md` link was already dangling.)
15. **[GC: moving-collector lost-tag missed root](gc-moving-interpreter-lost-tag-missed-root.md)** — 🔴 **OPEN**
    (benign in practice). Under `-Xmx1g` GC pressure the **moving** young collector zeroes a live object
    whose only reference is a frame slot tagged non-`Object` at the marking snapshot ("all-zero header" /
    `Stale pointer … StringBuilder.flush`). Localized **deterministically** to `RandomizedRunner.invoke
    local[3]` by a new gated `CRATONVM_GC_VERIFY_STALE` per-parked-thread verifier. Same *class* as the
    Family-A "lost-tag interpreter local" (A4) but on the `--nojit` moving path.
16. **[GC: rs_cache-presence reactor-shutdown timing race](gc-rscache-reactor-shutdown-timing-race.md)** —
    🔴 **OPEN** (workaround validated). The ES RestClient reactor-worker `ThreadLeakError` at
    `restClient.close()`: GC-frequency-driven and `rs_cache`-PRESENCE-triggered (a latent
    GC-STW-vs-reactor-shutdown race exposed by snapshot timing), **NOT** a socket/OP_WRITE bug and **NOT** an
    rs_cache correctness bug. Reliably avoided by `CRATONVM_ROOTSNAP_CACHE=0` (suite-level — do NOT flip the
    global default). Supersedes the former `reactor-worker-thread-leak-at-shutdown.md` (removed — see git history).
17. **[CompletableFuture untimed `get()` never wakes on cross-thread completion](../internal/app-jvm-bugs/gc-gen-promotion-completablefuture-completion-loss.md)** —
    ✅ **FIXED on dev** (moved to `docs/internal/app-jvm-bugs/`). The original gen-GC "lost young `Signaller`"
    theory was **refuted** (the hang is deterministic + GC-independent); the real cause was the synthetic
    `CompletableFuture.complete` native never running `postComplete()`. Fixed in the native — no GC change.
18. **[GC: live young `Thread` mirror in a blocked thread's frame reclaimed (Tomcat real-net/real-AQS HARD CRASHES)](gc-blocked-thread-frame-stale-thread-mirror.md)** —
    🟠 **OPEN on dev**, but a **defensive crash-mitigation is MERGED** (`6a04b0e3` + `e06ed934`). Six Tomcat
    encoding/tribes classes SIGSEGV/panic (`compact_value.rs:502`) under `CRATONVM_REAL_NET_SOCKETS` +
    `CRATONVM_REAL_AQS`: a live young `java.lang.Thread` mirror held in a **blocked** thread's frame local
    (object-tagged, so NOT the lost-tag item 15) is reclaimed because it is missing from that thread's
    deposited `root_snapshot`; the freed slot is reused as byte-buffer data and decoded as an object pointer.
    Related to the now-fixed `currentThread()` mirror construction corruption archived at
    [`../internal/repros/gc-concurrent-spawn-reclamation/`](../internal/repros/gc-concurrent-spawn-reclamation/).
    Current `dev` has that re-read / pin fix and the stale-mirror recovery table, but this Tomcat item remains
    open for the blocked-frame snapshot gap and register-resident remainder. The mitigation (`plausible_heap_pointer`
    gate at every ref-decode + JIT receiver-deref boundary) degrades a stale ref to a Java NPE — all 6 are now
    **crash-free jit+nojit** but still fail/time out (the residual reclamation itself is unfixed).
19. **[Hibernate `type.temporal.*` — moving GC strands lambda refs in native stream/collection intrinsics](hib-temporal-gc-lambda-native-stale-local.md)** —
    🟠 **OPEN** (native callback pinning hardened; full Hibernate rerun still pending). The 5 `org.hibernate.orm.test.type.temporal.*` classes
    abort rc=1 / SIGSEGV with `linkage error: no such method java/lang/Object.<sam>` — **not** a java.time
    binding bug. Same native-stale-Rust-local family as the StackWalker corruption
    ([hibernate-bytearraymapping-stackwalk-gc-corruption.md](../internal/fixed-suite-bugs/hibernate-bytearraymapping-stackwalk-gc-corruption.md)):
    `Stream.forEach`/`sorted`, `Spliterator.tryAdvance`/`forEachRemaining`, `ArrayList.forEach` hold the lambda
    + materialized elements in Rust locals across `invoke_virtual`; the scheduled-task pump had the same issue
    when firing multiple accrued `Runnable.run()` callbacks. Current focused fix = `pin_native_root` /
    `read_native_pin` per callback/native (NOT force-non-moving — that hits the HIB-CV-33 precise-root gap).
    This remains open until the Hibernate runner fixture is available and the temporal class loop is proven
    crash-free at the default heap.

FIXED bugs whose standalone docs were **removed** from this folder (resolved; full writeups in
`git` history or [`docs/internal/fixed-suite-bugs/`](../internal/fixed-suite-bugs/)): A1 (reflection
mirror-array pinning), A3 (register-invisibility — precise maps default-on), the Hibernate JAXB
class-load rescan storm (HIB-DEV-03), the JSON-function `al_state` SIGSEGV, the reversed stack-trace
order, the `ReferencePipeline.toArray(IntFunction)` recursion, and the off-heap DirectBuffer
(bug-A/A2). The springrepos hang is mostly fixed (`dev` passes the test; only the latent
deep-recursion item remains — see below).

### Consolidations applied (2026-06-18)
- The two Hibernate-JTA docs (`hibernate-jta-narayana-…` + `hibernate-jta-txcontrol-getinetaddress-per-class-report`)
  described the **same** Narayana cluster → merged into `hibernate-jta-narayana-xa-completion-and-socket-loopback.md`;
  the per-class file is now a redirect stub.
- (2026-06-17) The two Fork6 precise-maps files were already merged into `fork6-fjp-multithread-jit-root-reclamation.md`.

The former **JIT regalloc callee-saved-register clobber** umbrella family is
resolved on x64 dev (2026-07-04) by making callee-saved GPR local homes opt-in only;
the archived write-up is
[`docs/internal/jit-regalloc-callee-saved-clobber-family.md`](../internal/jit-regalloc-callee-saved-clobber-family.md).
Do not conflate that historical register-*clobber* family with Family A below,
which is about root-*scanning* completeness.

---

## Family A — GC root coverage under JIT  *(one root cause, four manifestations)*

**Root cause (shared):** when a JIT frame is active, the young collection is the
**non-moving sweep with selective-promotion evacuation** (`gc_quiescence`; the
moving Cheney collector only runs with no JIT frame). That sweep is only correct
if the **root set is complete**. CratonVM's JIT-frame root scan is *conservative*
(it reads stack memory only) and has historically had gaps; a live young object
whose only reference sits in a gap is not marked (and not added to the
selective-promotion pin set) → it is swept-zeroed or evacuated-and-zeroed → its
stale reference later reads an all-zero / garbage header → `inconsistent header`,
`Stale pointer … all-zero header`, CCE, NPE, or SIGSEGV. `--nojit` always passes
(interpreter frames are precisely scanned and the moving collector remaps every
root); `-Xmx8g` passes (no young GC).

> **Current-dev refresh (re-verified 2026-06-29, fresh build off dev HEAD
> `9928052c`; precise-jit-maps-default Steps 1–8).** **A3 is CLOSED** by precise
> JIT oop maps (default-on) — validated green across the GC-root repro+bench lane,
> OSR frames, and the BouncyCastle app suites
> (`test-infra/regression-pool/gc-root-lane.sh` + `gc-root-apps-lane.sh`).
> **A2 is now FIXED** (`6e3ddb05`, 2026-06-23): `ReflRepro 8000 @ GC_STRESS=65536`
> → `ok=8000 bad=0 rc=0` (and `20000 @ GC_STRESS=524288 --Xmx 256m` → `bad=0`),
> previously `rc=139`. It was **never** a register/native-return missed root — it
> was a GC-side non-moving-sweep free-list double-serve (overlapping free blocks
> not coalesced), fixed in the sweep coalescer; precise maps are orthogonal.
> **A4 is OPEN:** non-stress gated `CRATONVM_REAL_FORKJOINPOOL=1`
> `Fork6`/`Fork6Hard` remains ALL-OK on current dev (re-verified 2026-07-01).
> **Correction (2026-07-03): the aggressive `GC_STRESS=262144`/`65536`
> failures are NOT A4** (or any JIT-root coverage gap) — root-caused as three
> live-object-freeing races in the *concurrent old-gen* mark/sweep, unrelated
> to JIT/register roots (reproduces under `--nojit` with zero live JIT frames
> at every STW). Those three defects are **FIXED on dev** (`57f545be`;
> [gcstress-concurrent-oldgen-races-FIXED.md](../internal/gcstress-concurrent-oldgen-races-FIXED.md)).
> A *different*, still-unexplained residual corruption survives that fix on
> the same aggressive lane — tracked separately at
> [gcstress-residual-corruption-faces.md](gcstress-residual-corruption-faces.md);
> face 2 there (JIT lost-tag int-in-ref-slot) may or may not be the same
> mechanism as A4's register-only residual — unconfirmed. Separately, the
> cross-thread STW JIT takeover (BUG-03) had a **gate-polarity bug making it
> silently inert in every default-env run** — fixed 2026-07-02, see the "Fix
> 2026-07-02" section of the A4 doc. A4's own register-only residual is
> unaffected by any of this and remains **OPEN**, gated on the deferred
> precise-JIT-stack-maps project. The *family is not yet formally retired*
> (A4 + container-app CI remain); **nothing was removed** — all repros, GC
> guards, `CRATONVM_DBG_*` knobs, and the shadow stack are retained as
> experimental/debug tools.
>
> See `docs/feature-designs/precise-jit-maps-default.md` "Step 8 — GC-root family
> retirement status".

The eventual correct fix for the whole family is **precise JIT stack roots**
(know exactly which registers/slots hold oops at each safepoint), tracked under
`project_precise_jit_stack_maps`. The `CRATONVM_SHADOW_STACK` mechanism is the
current (incomplete/buggy) implementation of that.

| # | Manifestation | Repro | Status | Doc |
|---|---|---|---|---|
| **A1** | Reflection mirror-array builders held an `ObjectRef` array in a Rust local across allocating calls (`Field[]`/`Method[]`/annotation arrays) | `wildfly-suite/repro/MinRepro` | ✅ **FIXED on dev** (`pin_native_root` sweep) | _(doc removed; resolved)_ |
| **A2** | **`implausible object size` young-sweep-walker crash** (reflection/String-array allocation churn) — a *distinct* bug, NOT the register root: a non-moving-sweep free-list double-serve (overlapping free blocks not coalesced) | [`../internal/repros/A2-reflrepro/`](../internal/repros/A2-reflrepro/) | ✅ **FIXED on dev** (`6e3ddb05`, 2026-06-23; coalesce overlapping free blocks) — `ReflRepro 8000 @ GC_STRESS=65536` → `ok=8000 bad=0` (re-verified 2026-06-29); precise maps orthogonal; moved to docs/internal | [reflrepro-register-resident-jit-root-handoff.md](../internal/app-jvm-bugs/reflrepro-register-resident-jit-root-handoff.md) |
| **A3** | **Register-invisibility** — a live oop sits only in a CPU register at a young-GC safepoint, invisible to the stack-only scan (single thread) | `apps/spring-boot/buildSrc/runner/MinRegexProbe` | ✅ **FIXED on dev** (`32649b56`, precise maps default-on) | _(doc removed; resolved)_ |
| **A4** | Multi-thread: live `ForkJoinTask`s reclaimed under **FJP worker threads** + a **lost-tag** interpreter local (register-only residual) | `scratch/xworker/Fork6` / `docs/known-issues/repros/A4-fork6/Fork6Hard.java` (needs `CRATONVM_REAL_FORKJOINPOOL=1`) | 🟡 **OPEN** — non-stress `Fork6`/`Fork6Hard` is ALL-OK on current dev (re-verified 2026-07-01). The `GC_STRESS` lane failures previously attributed to A4 are **re-scoped as a separate bug** (2026-07-03, see [gcstress-residual-corruption-faces.md](gcstress-residual-corruption-faces.md)) — three concurrent-old-gen races were FIXED, a different residual remains. A4's own register-only gap is unaffected, still gated on precise-JIT-stack-maps. | [fork6-fjp-multithread-jit-root-reclamation.md](fork6-fjp-multithread-jit-root-reclamation.md) |
| **A4-gcstress** | Concurrent old-gen mark/sweep: SATB never wired (no production caller), young→old roots never traced, failed remark STW fell through to the sweep — three JIT-**unrelated** live-object-freeing races surfaced by the aggressive `GC_STRESS` lane | `docs/known-issues/repros/A4-fork6/Fork6Hard.java` + `CRATONVM_DBG_GC_STRESS=65536` | ✅ **FIXED on dev** (`57f545be`) — 42/42 focused `cratonvm-gc` unit tests; controls (`Fork6`, `Fork6Hard 256 40`, bt16) green. Residual (different signatures) tracked separately | [../internal/gcstress-concurrent-oldgen-races-FIXED.md](../internal/gcstress-concurrent-oldgen-races-FIXED.md) (fixed) / [gcstress-residual-corruption-faces.md](gcstress-residual-corruption-faces.md) (residual, open) |
| **A5** | **Object-binarytrees moving-GC corruption** — the compiled entry-point `main`'s JIT frame is invisible to `gc_quiescence` (invoked via `Vm::invoke` without a `JitEntryGuard`), so the **moving** young collector relocates its roots and can't rewrite the raw stack slots → stale all-zero receiver. (The earlier "register-only stale root in `bottomUpTree`" framing was wrong — `bottomUpTree` isn't even compiled at the crash.) | [`../internal/repros/gc-stress-bintrees-main-args/`](../internal/repros/gc-stress-bintrees-main-args/) (`VAAload`) | ✅ **FIXED** (dev `77c98761`) — detect an unregistered JIT frame on the native stack → non-moving sweep + full-stack mark. Repro archived under `docs/internal`. Residual: Windows-only (portable stack-bound is a follow-up) | [docs/internal/.../gc-stress-bintrees-main-args-unregistered-jit-frame-FIXED.md](../internal/app-jvm-bugs/gc-stress-bintrees-main-args-unregistered-jit-frame-FIXED.md) |

> ## ✅ FIX (2026-06-17, dev `32649b56`): **precise JIT oop maps, default-on** — closes the register-invisibility root-scan gap (A3)
> `CRATONVM_PRECISE_JIT_MAPS` is now **default-on** (opt out: `CRATONVM_NO_PRECISE_JIT_MAPS`).
> It makes the non-moving sweep's root scan **precise across every active JIT frame**
> (RBP-chain `frame_record` + per-safepoint oop maps + a conservative per-frame
> fallback), so a live oop in a callee-saved register of a **caller** frame — the
> register-invisible root the conservative deepest-band-only scan missed — is found.
> This fixes the **register-invisibility class** (A3 and the kafka bug-21/22 /
> tomcat-style register-invisibility reclaims). **Verified:** A3 (`MinRegexProbe`)
> green at `GC_STRESS` 524288 **and** 4 MB; `bintrees16/18` == golden
> `14985902`/`68332206` (no under-count); `matrix`/`sieve`/`fib` correct; opt-out
> reverts to the broken path.
>
> **Scope correction (verified 2026-06-17): A2 and A4 are NOT closed by this** — they
> have *separate* bugs. **[SUPERSEDED 2026-06-23: A2 is now FIXED — `6e3ddb05`, a
> free-list double-serve in the sweep coalescer, not a register root; `ReflRepro`
> `bad=0`. See the top-of-section refresh + the A2 table row. A4 remains open.]**
> **A2** (`ReflRepro`) *(then)* still crashed with `implausible object
> size` / `inconsistent header` on the young-sweep WALK (a core array/String
> allocation↔sweep bug, distinct from the register root — precise maps fix root
> *scanning*, not the sweep walker; it actually surfaced *more* of A2's corruption by
> retaining more — a symptom-timing effect, since cured). **A4** (`Fork6`, under the experimental `CRATONVM_REAL_FORKJOINPOOL`
> gate) now fails with a real-FJP `ForkJoinPool` CAS conflict on *both* precise-on and
> -off, masking the original reclaim — so unverified, not regressed.
>
> **Perf:** the per-invocation `frame_record` CALL was made ~40% cheaper by caching
> the top-frame RBP in a thread-local (`82cf85e9`): **fib44 2.5× → 1.68×**; alloc/
> compute/array are neutral or slightly faster. Residual = the CALL itself (follow-up:
> inline the RBP store in codegen; the NOP-skip lever is unsafe — it anchors the frame
> walk; details in SB-CRASH-04 #5).
>
> **Regression sweep (2026-06-17, default-on vs `CRATONVM_NO_PRECISE_JIT_MAPS`):**
> **zero correctness regressions** — `bintrees10/12/14/16/18`, `matrix600/800`,
> `sieve250k`, `fib44` all checksum-identical; 10 standalone app probes
> (`AR`/`Antora`/`CHMEq`/`CollCopy`/`DOMWalk`/`Builtins`/`CPUtil`/`Asm`/`ArrInst`/`AnonM`)
> byte-identical output to legacy. The full 50+ app gauntlet remains the CI bar.
> Predecessor: the `SHADOW_STACK` reload SIGSEGV fix (`19fd6707`).

The history below predates the fix.

> **Correction (2026-06-17):** A2 was briefly believed fixed via a "JIT-scan-cache
> is unsound → default-OFF" change (`8dfd5c2b` / `bug-06b`). That was a **GC-timing
> mask, not a fix**, and was **reverted** (`b41c0484`): on current `dev` ReflRepro
> crashes identically cache-on and cache-off. `bug-06b-jit-scan-cache-unsound.md` is
> superseded by the reflrepro handoff (the genuine `collection_count` cache key it
> added was kept).

> **Verified on current `dev` (binary built 2026-06-17):** for the A3 repro
> (`MinRegexProbe code 20000`, `CRATONVM_DBG_GC_STRESS=524288`), the *only*
> correct config is `--nojit`. Every conservative trick
> (`CRATONVM_NO_JIT_SCAN_CACHE`, `CRATONVM_JIT_SAFEPOINT_REG_SPILL`,
> `CRATONVM_DBG_FULLSTACK_SCAN`, and combinations) still crashes — confirming the
> root is genuinely register-resident and unreachable by any stack scan. Every
> `CRATONVM_SHADOW_STACK` variant is also currently broken (movable → SIGSEGV;
> `+pin` → hang; `+noreload` → hang). See SB-CRASH-04 for the live signatures.
>
> **Note the asymmetry:** `CRATONVM_SHADOW_STACK` *fixes* A2 (`ReflRepro` — no
> crash) but *crashes* A3 (`MinRegexProbe`). The difference is a distinct
> **shadow-reload codegen bug** that only the A3 path exercises: the post-call
> reload writes the integer `1` into the callee-saved register holding `this`
> (`r13`/`r15`), which a later `getfield` then dereferences. So "complete the
> shadow stack" (the A2 handoff's recommended fix) must *also* fix this reload bug
> before it can be the universal fix. Pinned in SB-CRASH-04 to the reload at
> `Matcher.reset` pc=110 / `Pattern.append` pc=29.

---

## Standalone bugs

| # | Bug | Status | Doc |
|---|---|---|---|
| **C** | Deep JIT->JIT recursion overruns the **native** stack (ANTLR `closure()`); same-method recursive edges and compile-time-discovered mutual direct-call cycles now route through guarded dispatch except static tail self-jumps, the ANTLR cold-path validation lift keeps the known-bad `PredictionContext` cluster interpreted, and x64 prologues now stack-bang page-by-page with one-page headroom. The full overflow path still needs a resumable fault-recovery handler / catchable `StackOverflowError` routing from the fault context. | **LATENT / PARTIALLY CONTAINED** (not a current blocker; 2026-07-01 SpringRepos validation is green, recursive-edge + compile-cycle routing, native-shadow gate narrowing, guarded ANTLR validation lift, and x64 stack-bang containment landed) | [jit-deep-recursion-fault-recovery.md](jit-deep-recursion-fault-recovery.md) |
| **MT-STW** | `Thread.join` monitor-ownership **desync under concurrent GC** (JIT-off): `monitor_wait`/terminate-tail used a raw `ObjectRef` captured before blocking, then `arrive_and_wait` let the in-flight STW relocate+zero it → `ensure_inflated` on the stale address synthesised a fresh `owner=None` monitor → IMSE in the javac synchronized-exit loop → joiner livelock → STW wedge. ~1–3% on `scratch_churn/Churn.java`; the tail after the four barrier+expansion fixes (`68c6993e`, 0%→~97%). | ✅ **FIXED** (`74b195b4`) — remap the receiver through `arrive_and_wait`'s returned pointer map; Churn 0 hangs / ~480 runs | [../internal/mt-stw-join-monitor-desync.md](../internal/mt-stw-join-monitor-desync.md) |

> Bug **B** (JUnit `@Timeout` "interceptor invoked twice") is **FIXED** and archived — it was
> never threading/`MethodHandle`: a synthetic natural-order compare raised `NoSuchMethodError`
> instead of `ClassCastException` for a non-`Comparable` element, escaping Spring's `catch (CCE)`
> and tripping JUnit's chain detector. See [`docs/internal/fixed-suite-bugs/spring-bug-04-junit-timeout-interceptor-double-proceed.md`](../internal/fixed-suite-bugs/spring-bug-04-junit-timeout-interceptor-double-proceed.md).

## Standalone — bug-06 assertion-mismatch family (Spring suite reflection/annotation tail)

The bug-06 census (`spring-suite/crash-reports-2026-06-16/bug-06-assertion-mismatch-families.md`)
clustered ~529 genuine assertion mismatches into 6 families. Families 1–5 are closed
(field-updaters `fe52db3a`; `HttpClient.executor`; synthetic-`Object` superclass `40b6d94a`;
`findLoadedClass` no-load `4b923e86`; F5 extinct 2026-07-02, all on `dev`). The one open family is
annotation-synthesis **native-return-value** correctness, not GC/JIT:

| # | Bug | Status | Doc |
|---|---|---|---|
| **F5** | Reflection native returns `null` where HotSpot returns a `Class`/`Method` (`getDeclaredMethod on null` ×28). Common paths **verified clean** (`Refl5` == HotSpot); the ×28 aggregate was a cross-family cascade and is **extinct** — 0 instances in the clean 2026-06-30/07-01 full re-runs and a fresh 196-class nojit+jit sweep on dev `ffb247e5`. **Do not** touch `synthetic_class_mirror` slot 0 (refuted hypothesis). | ✅ **CLOSED 2026-07-02** — failcause extinct, nothing to attribute | [bug06-fam5-reflection-getdeclaredmethod-null.md](../internal/fixed-suite-bugs/bug06-fam5-reflection-getdeclaredmethod-null.md) (moved) |
| **F6** | Spring annotation **synthesis** (`@AliasFor`/`MergedAnnotation`/`MirrorSets`) value mismatches (the `AnnotationUtilsTests` "~2 GB OOM" half was the `toArray` self-recursion, now **FIXED**). | 🔴 **OPEN** — `[[spring-bug-01]]` umbrella | _(doc removed; tracked on branch `fix/bug06-fam6-repeatable-merge`)_ |

## Consolidated suite bug docs (open & distinct — copied in 2026-06-18)

Genuine open VM defects pulled in from the suite-specific trackers
(`spring-suite/crash-reports-2026-06-16/`, `spring-suite/bugs/`,
`docs/kafka-suite-0617/`) so this folder is the single map. **These are copies** —
the originals remain in their suite folders (which keep their own numbering). Only
open, distinct bugs were copied; FIXED docs (bug-03, crash-01/02/03,
spring-bug-02/03/04/05/09/12, kafka bug-A — see `docs/internal/fixed-suite-bugs/`)
and already-consolidated ones (fam5/6) were left in place.

| Bug | Category | Status | Doc |
|---|---|---|---|
| String constant corrupted → `Object` under load | VM-CORRECTNESS / GC | ✅ **RESOLVED / NOT REPRODUCED 2026-06-20** — a 45-class single-JVM spring-core batch on dev ran clean (0 `status=java.lang.Object`, all OK, rc=0); Family-A precise-maps default-on + `toArray` recursion fixed. Doc removed (writeup in git history). | _(removed)_ |
| `MergedAnnotations` hang | VM-HANG | ✅ **FIXED 2026-06-20** (`8795b88d`) — was the `ReferencePipeline.toArray(IntFunction)` self-recursion; `MergedAnnotationsTests` now 174/178 (residual 4 = synthesis mismatch, cf. bug-06 F6) | [toArray-recursion fix](../internal/fixed-suite-bugs/springsuite-0620-toarray-referencepipeline-recursion.md) |
| Serializable proxy round-trip | VM-CORRECTNESS (proxy + serialization) | ✅ **RESOLVED on `dev`** — the standalone serialize→deserialize JDK-proxy repro round-trips correctly; `SerializableTypeWrapperTests` generic-type-render residual tracked on branch `fix/generic-array-type-tostring`. Doc removed. | _(removed)_ |
| JUnit-platform execution `LoadError` | VM-CORRECTNESS / dispatch | 🔴 **OPEN** — JUnit platform internals; Family-A GC-root race (NOT related to the now-fixed bug-04, which was a non-`Comparable` compare exception-type bug, not a GC race) | [spring-bug-10-junit-platform-execution-loaderr.md](../internal/spring-bug-10-junit-platform-execution-loaderr.md) |
| Groovy / scheduler crashes (rc=139) | VM-CRASH | 🟡 **PARTIAL** — Groovy SIGSEGV fixed via the bug-12 HashMap-layout fix; residual = a separate Groovy **hang at BEGIN** (inventory, needs per-cluster trace) | [spring-bug-11-groovy-and-scheduler-crashes.md](../internal/spring-bug-11-groovy-and-scheduler-crashes.md) |
| Mockito `mockStatic` + mock dispatch | VM-CORRECTNESS (Mockito dispatch) | ✅ **FIXED on `dev`** — the dispatch/shadowing half landed earlier, and the stale JIT call-site residual was fixed by redefine-time compiled dispatch quiescing. | [fixed residual](../internal/fixed-suite-bugs/kafka-bug-B-mockstatic-capturing-lambda-jit.md) |
| `WeakHashMap` stream infinite hang | VM-HANG → JIT codegen | ✅ **FIXED on `dev`** (`1cd0ab26`, JIT ban; verified; doc removed). The broader callee-saved GPR local-home family is now retired by the default-off fix and archived in the [JIT regalloc family doc](../internal/jit-regalloc-callee-saved-clobber-family.md). | _(removed)_ |

> `spring-bug-10` is a **Family A** (GC-root-coverage-under-JIT) manifestation seen from the
> Spring suite — same root cause as A1–A4 above, a different entry point. Fixing precise JIT
> stack roots should clear it; tracked there. (`springsuite-bug-04` was the same race but no
> longer reproduces — doc removed.)

## The springrepos handoff (mostly fixed)

[`springrepos-extension-hang-jit-throughput-and-deep-recursion.md`](../internal/springrepos-extension-hang-jit-throughput-and-deep-recursion.md)
is the archived multi-defect handoff for `SpringRepositoriesExtensionTests`. The hang,
parse-NPE (#1), generics (#2), and the indy `MethodHandle.type()` layers
(3/3b/3c/3d) are all **fixed**, and **layer 3e is now ✅ FIXED on dev** (`7335f918`)
— the test is **11/11 FULL GREEN**. The 3e writeup (the Groovy indy call on a
**Mockito mock**, `this.repositories.maven { … }`) moved to
[`docs/internal/spring/spring-boot-groovy-indy-mockito-mock-dispatch.md`](../internal/spring/spring-boot-groovy-indy-mockito-mock-dispatch.md).
The 3c/3d fix writeup is in
[`docs/internal/spring-boot-groovy-indy-runtime-argcount-3c-FIXED.md`](../internal/spring-boot-groovy-indy-runtime-argcount-3c-FIXED.md).
The 2026-07-01 retry passed 11/11 with
`CRATONVM_JIT_ALLOW_PACKAGES=groovyjarjarantlr4/`, so this handoff is no longer
an open known-issue doc. Also still relevant: **defect #2 (= family A3 above)**
and **bug C** (the cold-path deep-recursion/fault-recovery residual), now tracked
in [`jit-deep-recursion-fault-recovery.md`](jit-deep-recursion-fault-recovery.md).
The Hibernate HQL census-H4 timeout
(`function.json.JsonArrayUnnestTest`) is the same cold ANTLR prediction
throughput bug and is now consolidated under bug C.

## Resolved standalone bugs (full writeups in `docs/internal/`)

These were open here and are now **fixed / do-not-reproduce**; the detailed writeups live under
[`docs/internal/`](../internal/):

- **Hibernate JAXB class-load storm** (HIB-DEV-03) — ✅ FIXED (`fix/hib-dev-03-jaxb-classload`): the
  synthetic-stub "upgrade" re-ran a full O(num_jars) classpath scan on every map-node allocation; memoizing
  the known-absent result dropped a 20k-node put loop 20 120 ms → 132 ms.
- **Hibernate JTA cluster** (Narayana XA + socket loopback) — ✅ RESOLVED. L0 `getInetAddress` NPE fixed on
  dev (`ada6cebf`); L1 XA-completion was never broken (refuted); L2 was a process-wide `accept()` deadlock +
  `getLocalPort()==0`, fixed on `fix/hib-jta-xa-loopback`. → [`docs/internal/hibernate-jta-narayana-xa-completion-and-socket-loopback.md`](../internal/hibernate-jta-narayana-xa-completion-and-socket-loopback.md).
- **Hibernate JAXB/ByteBuddy bootstrap slow** — ✅ RESOLVED / no-repro (`1db07c35`/`25c42e13`). → [`docs/internal/hibernate-jaxb-classloading-bytebuddy-bootstrap-slow.md`](../internal/hibernate-jaxb-classloading-bytebuddy-bootstrap-slow.md).
- **JUnit 5 `@ExtendWith` meta-annotation `ParameterResolver`** — ⚠️ MISDIAGNOSED / does-not-reproduce;
  annotation discovery is byte-identical to HotSpot and `JtaCustomAfterCompletionTest` passes 5/5. → [`docs/internal/junit5-extendwith-meta-annotation-parameterresolver.md`](../internal/junit5-extendwith-meta-annotation-parameterresolver.md).

## Standalone — HQL parser rejects chained additive/duration/concat operators

✅ **FIXED and MERGED to `dev`** (`3864097b`, `fix/hql-chained-operator-parse`,
commit `4e2a4493`); fully re-verified 2026-07-04 (all 3 affected classes now
100% pass, no regressions). Moved to
[`docs/internal/hql-antlr-chained-operator-syntax-error-FIXED.md`](../internal/hql-antlr-chained-operator-syntax-error-FIXED.md).
`a + b + c`
(or `a || b || c`, chained date/duration arithmetic, etc.) failed to parse —
CratonVM-only, second occurrence of the same operator class rejected with
ANTLR `SyntaxException: no viable alternative`. **NOT the same bug as
`jit-deep-recursion-fault-recovery.md` (Bug C)** — reproduced identically
with `--nojit` (Bug C is JIT-only) and with a trivial standalone ANTLR4
grammar unrelated to Hibernate/Groovy. Root-caused to a one-line defect in
CratonVM's native Rust reimplementation of `ParserATNSimulator`'s closure
algorithm (passed `inContext = !full_ctx` instead of `depth == 0`), a
generic defect in revisiting the same parser decision
twice within one parse; the specific defective method is not yet identified.

## Standalone — Hibernate deserialized SessionFactory is null

[docs/internal/hibernate-deserialization-sessionfactory-reconnect-null.md](../internal/hibernate-deserialization-sessionfactory-reconnect-null.md)
— full-suite census (dev, 2026-06-17). 5 serialization round-trip tests NPE
(`getMappingMetamodel`/`getClassLoaderService` on null) because a deserialized
`EntityManager`/`SessionFactory` doesn't reconnect to the live factory. Generic
`readObject`/`readResolve` work on CV (verified); the gap is Hibernate's
`SessionFactoryRegistry.findSessionFactory(uuid,name)` returning null after deser. 🔴 open.

## Hibernate full-suite census (dev 2026-06-17) — additional docs

Per-run bug reports from the (gitignored) `apps/hibernate-orm/cratonvm-bug-reports/dev-run-20260617/`.
(The JSON-function `al_state` SIGSEGV and the reversed stack-trace order were **FIXED** and their docs
removed; the JTA `getInetAddress` per-class report was consolidated into the resolved JTA doc in
[`docs/internal/`](../internal/).)
- [hibernate-hang-clusters-summary.md](../internal/hibernate-hang-clusters-summary.md) — census overview (now in `docs/internal/`); **H1/H2/H3 resolved 2026-06-20**, **H4 root-caused** (its own open doc below).
- HQL/ANTLR parser census H4 (`function.json.JsonArrayUnnestTest`) is not a
  separate open issue file anymore. It is consolidated into
  [jit-deep-recursion-fault-recovery.md](jit-deep-recursion-fault-recovery.md)
  as a second reproducer for the same cold ANTLR prediction
  throughput / JIT backend-coverage cluster.

Also fixed on dev this run (no standalone doc — see commit): `Locale.toLanguageTag()` dropped all subtags for real Locales (`13e8c761`).

- **Hibernate wrong-result assertion failures** — cluster of CV-only wrong-result assertion FAILs (UniqueConstraintBatching 1-vs-0, DetachedBag true-vs-false, EntityGraphBatchSize, immutable+converter deser, …); each likely a separate root cause. 🔴 open (handoff; standalone doc not preserved).

## Hibernate suite residuals (2026-06-24)

Triaged while fixing the collection-delegation native stack overflow (`7b224d8a`:
`try_delegate_real_collection` self-recursion → `EXCEPTION_STACK_OVERFLOW` building a
`SessionFactory`). Once that crash was fixed, three pre-existing CratonVM-only
residuals surfaced (all fail identically at baseline `b0aab8f9`, so none is from the
regression):

- [hib-proxyclassreuse-loader-blind-class-resolution.md](hib-proxyclassreuse-loader-blind-class-resolution.md) —
  🔴 **OPEN.** `ProxyClassReuseTest.testNoReuse`: `CONSTANT_Class` resolution is loader-blind
  (a class constant inside custom-loader bytecode resolves through the flat global/app store, not
  the holder's defining loader), so an isolated loader's `MyEntity` collapses to the app namespace
  and its ByteBuddy proxy collides. loadClass-override isolation itself works; this is deeper
  (core class-store change, broad blast radius). Min repro `.scratch-hhsf/IsoProbe3.java`. Same
  family as **SBR-14** / `SC-custom-classloader`.
- [hib-bytecode-enhancement-loader-faithful-linking.md](hib-bytecode-enhancement-loader-faithful-linking.md) —
  🔴 **REOPENED 2026-07-04** (moved back from `docs/internal`, where it was mis-archived as
  "FIXED / ARCHIVED"). Builds on the proxyclassreuse fix above with superclass/interface linking,
  `invokespecial` owner dispatch, and SessionFactory-build loader-identity fixes — all confirmed
  genuinely landed on `dev` by a fresh source audit. But `enhancement.lazy.*`/`mapping.lazytoone.*`
  remain the single largest open Hibernate-enhancement gap (a fresh 18-class sample: 2/18 PASS,
  14/18 FAIL, 1/18 HANG), not the "separate residual" the archived doc implied.
- [hib-sortnatural-persistentsortedset-cascade-drop.md](../internal/hibernate-bugs/hib-sortnatural-persistentsortedset-cascade-drop.md) —
  ✅ **FIXED on dev** (`9ac7ef1d`; moved to docs/internal). `SortNaturalTest` (`sorted.set`/`sorted.map`): a cascaded `@OneToMany SortedSet`
  drops an element on **persist** (only 1 of 2 rows inserted; `size()` 2→1). Core `TreeSet` verified
  correct on every access path; the loss is in the Hibernate `PersistentSortedSet` cascade
  interaction. Part of the HIB-CV-35 cvonly long-tail.
- [hib-nodepth-shrinkwrap-par-archive-url.md](../internal/hibernate-bugs/hib-nodepth-shrinkwrap-par-archive-url.md) —
  ✅ **FIXED on dev** (`9258f821`; moved to docs/internal). `NoDepthTests` JPA variants: ShrinkWrap in-memory `.par` archive +
  `ShrinkWrapClassLoader` need a custom `URLStreamHandler` ("Could not create URL for archive");
  the 2 non-JPA variants pass.

## Test-suite repair findings (2026-06-21)

While repairing the in-repo test suites (most failures were stale tests / missing
fixtures / a wrong feature set — all fixed on `dev`), three defects were left open
because each needs a risky core change or a large quality pass. One of the three (the
JIT divide-by-zero re-run) has since been fixed; the other two remain open:

- ✅ **JIT `idiv`/`irem` divide-by-zero re-runs the whole method** (side effects double-execute):
  FIXED on `dev` (direct-throw of `ArithmeticException`, verified vs HotSpot + bt18 soak). The
  historical record moved to [../internal/nested-try-catch-jit-divzero-rerun.md](../internal/nested-try-catch-jit-divzero-rerun.md);
  regression coverage is `test_jit_*_zero_no_double_side_effect` in `vm/tests/exception_tests.rs`.
- **Brooks read-barrier vs CompactHeader forwarding** (`load_and_forward` reads the legacy
  forwarding slot while the test installs the compact one):
  [tier1-brooks-compactheader-forwarding.md](tier1-brooks-compactheader-forwarding.md). Needs a
  maintainer call on which forwarding format the live collector uses before any change.
- **T11 safety-annotation coverage below thresholds**:
  [t11-safety-annotation-coverage.md](t11-safety-annotation-coverage.md). Documentation-only but
  large (~264 cast annotations in interpreter.rs); must be authored accurately, not marker-spammed.

## Keycloak suite classpath (2026-07-02)

- ✅ **Mixed JUnit 5.10.3/6.0.3 runtime on `kc-universal-cp.txt`** — FIXED (local
  classpath file normalized to a single JUnit 6.0.3 stack). Caused 338 `CRASH` rows
  (`NamespaceAwareStore.computeIfAbsent` `NoSuchMethodError`) across `tests/base`
  and `tests/clustering`. Historical record moved to
  [../internal/keycloak-junit-namespaceawarestore-classpath-crashes.md](../internal/keycloak-junit-namespaceawarestore-classpath-crashes.md).
- ✅ **`Assert.assertNotNull` linkage crash (37 `CRASH` rows, `testsuite/model`)** —
  FIXED (`smallrye-common-constraint-2.16.0.jar` was entirely absent from
  `kc-universal-cp.txt`; added). Also added: a `CRATONVM_TRACE_UNIMPLEMENTED`-gated
  diagnostic that names the missing class whenever CratonVM's classloader falls
  back to an empty synthetic stub for an unresolvable `org/jboss/`, `org/wildfly/`,
  `io/quarkus/`, `io/smallrye/`, … class, plus a hint on the terminal
  `NoSuchMethodError` warning when the target class is such a stub — so this class
  of masked-classpath-gap bug self-diagnoses next time instead of needing a
  multi-hour investigation. Historical record moved to
  [../internal/keycloak-smallrye-assertnotnull-linkage-crashes.md](../internal/keycloak-smallrye-assertnotnull-linkage-crashes.md).
- ✅ **`SmallRyeConfigBuilder.addDefaultSources` linkage crash (3 `CRASH` rows,
  `tests/db` + `tests/clustering`)** — FIXED (`smallrye-config`/
  `smallrye-config-common`/`smallrye-config-core` 3.16.0 and, one layer down,
  `microprofile-config-api-3.1.jar` were entirely absent from
  `kc-universal-cp.txt`; both added). Historical record moved to
  [../internal/keycloak-smallrye-configbuilder-defaultsources-linkage-crashes.md](../internal/keycloak-smallrye-configbuilder-defaultsources-linkage-crashes.md).
- ✅ **`org.keycloak.testframework.config.Config` Quarkus classpath gap** — FIXED
  (2026-07-06). `quarkus-core` and four more layers behind it (the full
  `smallrye-common-*` family, `org.ow2.asm:asm`, `jboss-logmanager`,
  `quarkus-bootstrap-runner`) were entirely absent from `kc-universal-cp.txt`;
  all added. `AccountConsoleDisabledTest` now runs past `Config.initConfig()`
  and Quarkus logging bootstrap into real JUnit 5 test execution. Also added
  `apps/keycloak-suite-runner/generate-kc-universal-cp.ps1` (a dry-run-by-default
  helper that pulls a named module's already-resolved `cratonvm-full-cp.txt`
  jars into `kc-universal-cp.txt`, per this doc's "no in-repo generator"
  ask) — see it for why a blind full-repo union isn't used by default.
  Historical record moved to
  [../internal/fixed-suite-bugs/keycloak-testframework-quarkus-config-classpath-gap.md](../internal/fixed-suite-bugs/keycloak-testframework-quarkus-config-classpath-gap.md).
- [keycloak-testframework-enterprisedb-supplier-noclassdef.md](keycloak-testframework-enterprisedb-supplier-noclassdef.md) —
  🔴 open. Residual uncovered by the fix above: `Registry`'s extension-supplier
  discovery now runs (it couldn't before) and immediately fails with
  `NoClassDefFoundError: org/keycloak/testframework/database/EnterpriseDbDatabaseSupplier`
  — puzzling because the class/jar/classpath-dir all genuinely exist; possibly
  the same misleading-diagnostic pattern as the Infinispan `isClustered()` bug
  below, or a missing Testcontainers jar. Not yet root-caused.

## Keycloak post-PreviewFeatures rerun (2026-07-03)

After `jdk/internal/misc/PreviewFeatures.isPreviewEnabled()Z` was fixed, the
1044-class Azure non-passed rerun no longer contains the original
PreviewFeatures native crash. The remaining non-passed rows are tracked here:

- [keycloak-arquillian-system1-defineclass-nosuchmethod.md](keycloak-arquillian-system1-defineclass-nosuchmethod.md) -
  621 `CRASH` rows in `testsuite/integration-arquillian/tests/base`, missing
  `java/lang/System$1.defineClass(...ProtectionDomain;String;)Class`.
- [keycloak-quarkus-cmimpl-no-class-def.md](keycloak-quarkus-cmimpl-no-class-def.md) -
  283 `CRASH` rows on generated Quarkus/SmallRye `$$CMImpl` config mapping
  implementation classes (`LogBuildTimeConfig$$CMImpl` and `TestConfig$$CMImpl`).
- ~~keycloak-junit-stringutils-anonymousobject-anymatch.md~~ - FIXED
  2026-07-04: 64 `FAIL` rows from `StringUtils.containsWhitespace` calling
  missing `cratonvm/synthetic/AnonymousObject$1.anyMatch(IntPredicate)Z`. Root
  cause was `String.chars()`/`codePoints()` (`native_string_chars`) allocating
  its IntStream via a raw `ClassId::new(0)`, which the VM's undersized-object
  guard silently substituted with a generic `AnonymousObject$1` placeholder
  instead of the real `IntStream` interface stamp — broke every IntStream op
  on `chars()`, not just `anyMatch`. Moved to
  `docs/internal/fixed-suite-bugs/`.
- [keycloak-model-infinispan-globalconfiguration-isclustered-nosuchmethod.md](keycloak-model-infinispan-globalconfiguration-isclustered-nosuchmethod.md) -
  still reproduces for the `testsuite/model` module: 37 `CRASH` rows plus one
  abstract/no-test `EMPTY` row.
- [keycloak-sssd-system1-findbootstrapclassornull-nosuchmethod.md](keycloak-sssd-system1-findbootstrapclassornull-nosuchmethod.md) -
  2 `FAIL` rows in the SSSD module, missing
  `java/lang/System$1.findBootstrapClassOrNull(String)Class`.

## Consolidation log

- **2026-06-17:** Merged `precise-jit-stack-maps-multithread-fjp-worker-testcase.md`
  (handoff/testcase) and `precise-jit-stack-maps-fork6-findings.md` (findings)
  into a single [fork6-fjp-multithread-jit-root-reclamation.md](fork6-fjp-multithread-jit-root-reclamation.md)
  — they described the *same* Fork6 bug (A4). Added this index framing the
  A1–A4 family + standalone B/C, and recorded the current-`dev` A3 verification.

## Spring Framework full-suite run (2026-06-21) — archived

A full `spring-core` run (binary built from dev) surfaced a cluster of CratonVM
divergences. The historical suite index has moved to
[docs/internal/spring/spring-core-suite-2026-06-21.md](../internal/spring/spring-core-suite-2026-06-21.md).
Most contained fixes from that sweep have landed on `dev`, including the stream
close-handler OOB, CHM null-key parity, LinkedHashMap null-replace behavior,
generic-bounds reification, keySet containment, StAX cursor natives, lambda-SAM
dispatch, JSpecify nullness reflection, and synthetic `Collector` SAM registration.

Residual handoffs from the sweep are retained in focused internal Spring notes:

- [SC-env-classreading.md](../internal/spring/SC-env-classreading.md) — `Object.equals` override precedence and custom-CL resource-stream follow-up.
- [SC-resource-io-family.md](../internal/spring/SC-resource-io-family.md) — remaining Resource/IO handoffs; several FileNotFoundExceptions are harness-CWD artifacts.
- [SC-stax-xml-family.md](../internal/spring/SC-stax-xml-family.md) — namespace SAX-event-sequence mismatch after the fixed cursor-native/prefix items.
- [SC-task-retry-util-misc.md](../internal/spring/SC-task-retry-util-misc.md) — Throwable deser, retry timing, ByteBuddy ClassInjector, and AQS throttle handoffs.
- [SC-misc-core-spring.md](../internal/spring/SC-misc-core-spring.md) — SortedProperties OutputStream store handoff; CHM null-key methods are fixed on dev.
- [SC-hangs-mergedannotations-charsequence.md](../internal/spring/SC-hangs-mergedannotations-charsequence.md) — two hangs: `MergedAnnotations.stream().toArray()` re-entry and Reactor `StepVerifier` producer scheduling.

Cross-cutting: ByteBuddy `ClassInjector$UsingReflection` failure breaks AssertJ
`assertSoftly` + Mockito; JUnit "TimeoutExtension multiple times" masks
underlying VM errors.

## Open bug docs relocated from `docs/internal/` (2026-06-22)

Still-**OPEN** bug docs are consolidated here so every unfixed bug lives under
`docs/known-issues/`. (A2/A4 are the `reflrepro-…` / `fork6-…` docs above;
FIXED/resolved bugs stay in `docs/internal/` — they are moved out only when fixed.)

**app-jvm-bugs/**
- [bug-bc-crypto-prng-abnormal-exit-127.md](app-jvm-bugs/bug-bc-crypto-prng-abnormal-exit-127.md)
- [bug-commons-math-full-reactor.md](app-jvm-bugs/bug-commons-math-full-reactor.md)
- [bug-commons-math-junit-probe-jit-execute.md](app-jvm-bugs/bug-commons-math-junit-probe-jit-execute.md)
- [bug-elasticsearch-log4j2-serviceloader.md](app-jvm-bugs/bug-elasticsearch-log4j2-serviceloader.md)
- [bug-gpu-build-native-builtins-crash.md](app-jvm-bugs/bug-gpu-build-native-builtins-crash.md)
- [bug-hibernate-duplicate-persistence-unit-scan.md](app-jvm-bugs/bug-hibernate-duplicate-persistence-unit-scan.md)
- [bug-hibernate-jpa-persistence-xml-properties.md](app-jvm-bugs/bug-hibernate-jpa-persistence-xml-properties.md)
- [bug-hibernate-log-format-placeholder.md](app-jvm-bugs/bug-hibernate-log-format-placeholder.md)
- [bug-wildfly-jaxp-premature-end-of-file.md](app-jvm-bugs/bug-wildfly-jaxp-premature-end-of-file.md)
- [bug-wildfly-msc-service-start-callback.md](app-jvm-bugs/bug-wildfly-msc-service-start-callback.md)
- [bug-wildfly-throwable-stack-trace-capture.md](app-jvm-bugs/bug-wildfly-throwable-stack-trace-capture.md)

**gaps/**
- [gap-bc-math-ec-crypto-regression-timeout.md](gaps/gap-bc-math-ec-crypto-regression-timeout.md)
- [gap-jit-fastmath-transform-miscompile.md](gaps/gap-jit-fastmath-transform-miscompile.md)

**h2-suite-bugs/**
- [bug-h2-charset-cp500-unsupported.md](h2-suite-bugs/bug-h2-charset-cp500-unsupported.md)
- [bug-h2-inprocess-javac-resource-bundle.md](h2-suite-bugs/bug-h2-inprocess-javac-resource-bundle.md)
- [bug-h2-mvstore-insert-loop-perf-hang.md](h2-suite-bugs/bug-h2-mvstore-insert-loop-perf-hang.md)
- [bug-h2-netutils-missing-pbe-algparams.md](h2-suite-bugs/bug-h2-netutils-missing-pbe-algparams.md)
- [bug-h2-timezone-dst-offset.md](h2-suite-bugs/bug-h2-timezone-dst-offset.md)

**keycloak-crash-reports/**
- [06-keypair-verifier-decode.md](keycloak-crash-reports/06-keypair-verifier-decode.md)
- [08-stripsecrets-json-comparison.md](keycloak-crash-reports/08-stripsecrets-json-comparison.md)
- [09-streamsutil-onclose-propagation.md](keycloak-crash-reports/09-streamsutil-onclose-propagation.md)
- [10-jwksutils-one-failure.md](keycloak-crash-reports/10-jwksutils-one-failure.md)

**tomcat-suite-bugs/**
- [04-embedded-server-throughput-wall-OPEN.md](tomcat-suite-bugs/04-embedded-server-throughput-wall-OPEN.md)
- [05-suite-rerun-fail-triage.md](tomcat-suite-bugs/05-suite-rerun-fail-triage.md)

**wildfly-suite-bugs/**
- [bug-06b-jit-scan-cache-unsound.md](wildfly-suite-bugs/bug-06b-jit-scan-cache-unsound.md)

## Spring Boot runner-probe sweep (2026-06-22)

A 103-probe sweep of `apps/spring-boot/buildSrc/runner/` (each a standalone
`main()`) under a fresh dev build (`cvsbfull.exe`, worktree `CratonVM-sbfull`) vs
HotSpot jdk-25 found **14 CratonVM-only bugs** (0 crash, 11 hang, 19 real DIFF).
Index + per-bug reports: [spring-boot-probe-sweep/INDEX.md](../internal/spring-boot-probe-sweep/INDEX.md).

**3 FIXED + merged to `dev`** (writeups in [`docs/internal/`](../internal/)):
- **SBR-03** (`a245002c`) strict array `instanceof` ([writeup](../internal/spring-boot-probe-sweep/SBR-03-array-interface-instanceof.md)) — `Object[] instanceof I[]` → `false`.
- **SBR-06** (`bce39db7`) Constructor `Parameter.getParameterizedType()` generics
  ([writeup](../internal/spring-boot-probe-sweep/SBR-06-field-getgenerictype-raw.md)) — populate the Constructor mirror's `signature` field.
- **SBR-02** (`0d7dfc28`, merge `01375f90`) regex `replaceAll` / literal `replace` throughput wall
  ([writeup](../internal/spring-boot-probe-sweep/SBR-02-string-regex-throughput.md)) — flip `CRATONVM_NATIVE_STRING_REGEX`
  default-ON; `String.{replaceAll,replaceFirst,matches,replace(CharSequence,…)}` route to fast
  Rust-regex natives. Was the `PluginXmlParserTests` hang.

**Open** (root-caused; none a safe one-liner — see each report):
- **SBR-01** Groovy `parseClass` hang ×9 — see [SBR-01-groovy-parseclass-hang.md](../internal/spring-boot-probe-sweep/SBR-01-groovy-parseclass-hang.md); the overlapping buildSrc coldpath suite index is archived at [spring-boot-buildsrc-coldpath-hangs-2026-06-22.md](../internal/spring-boot-probe-sweep/spring-boot-buildsrc-coldpath-hangs-2026-06-22.md). **handoff**
- **SBR-14** custom `URLClassLoader(parent=null)` bypassed → `AppClassLoader` (classloader isolation). **handoff**
- **SBR-04** annotation `getClass()`/`toString`; **SBR-05** `getDeclaredMethods` order (won't-fix, spec-unspecified);
  **SBR-07** `getSimpleName` (real defect = `getDeclaringClass0`/InnerClasses for Kotlin classes);
  **SBR-08/09/10/11/13** object-identity cluster (CV synthesizes JDK objects as abstract/base-typed —
  jar conn, NIO FS, IntStream, MethodHandle, ProtectionDomain); **SBR-12** `cratonvm.internal.UnmodifiableList`
  name leak (needs real `ImmutableCollections` or a guarded alias).

