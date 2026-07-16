# Spring Boot suite — known issues

Found triaging `FAIL`s from the first full Spring Boot 4.1.0-SNAPSHOT test
suite run against CratonVM (1975 classes; see
[[project_spring_boot_suite_runner_20260711]] and
`apps/spring-boot-suite-runner/RESULTS-20260711.md`). 508 classes FAILed;
five clusters were characterized here, together accounting for ~146 of them
directly (many more indirectly, via the "Unstarted application context"/
"Failed to parse configuration class" wrapper noise these root causes
produce). Most have since been retired (fixed or found not to reproduce on
current dev, see table). The rest is an uncharacterized long tail of
smaller/individual differences not yet clustered.

| Doc | Classes | Severity | Status |
|---|---:|---|---|
| `OnClassCondition.addAll` NPE-cast-to-`String[]` | 75 (348 occurrences) | CRITICAL | **FIXED/RETIRED 2026-07-13** — moved to [`../../internal/springboot/onclasscondition-npe-cast-string-array-cluster-FIXED.md`](../../internal/springboot/onclasscondition-npe-cast-string-array-cluster-FIXED.md); `@ConditionalOnClass`'s unresolvable-`Class`-element handling now defers to a `TypeNotPresentException` sentinel matching HotSpot, instead of a bare `null`. Verified against all 75/75 originally-affected classes |
| `DisposableBeanAdapter` "Invalid destruction signature" | 34 | HIGH | **RESOLVED 2026-07-12** — moved to [`../../internal/fixed-suite-bugs/spring-disposablebeanadapter-invalid-destruction-signature-cluster-RESOLVED.md`](../../internal/fixed-suite-bugs/spring-disposablebeanadapter-invalid-destruction-signature-cluster-RESOLVED.md); a direct probe against current dev found the destroy-method reflection path works fine, closing the cluster (no distinct residual identified) |
| `sun.misc.Unsafe$MemoryAccessOption` NPE | 26 | HIGH | **FIXED/RETIRED 2026-07-12** — moved to [`../../internal/fixed-suite-bugs/spring-boot-unsafe-memoryaccessoption-npe-FIXED.md`](../../internal/fixed-suite-bugs/spring-boot-unsafe-memoryaccessoption-npe-FIXED.md); same bug independently found+fixed via a concurrent Keycloak investigation, canonical doc is `testsuite-model-unsafe-putorderedlong-memoryaccessoption-npe-FIXED.md` in the same directory |
| `HttpClient.Builder` dead registration | 13 | HIGH | **RESOLVED 2026-07-12** — moved to [`../../internal/fixed-suite-bugs/springboot-httpclient-builder-dead-registration-abstractmethoderror-FIXED.md`](../../internal/fixed-suite-bugs/springboot-httpclient-builder-dead-registration-abstractmethoderror-FIXED.md); the active real-JDK registrar now covers every Java 17 fluent builder method |
| `zip-filedatablock-bulk-bytebuffer-put-aioobe.md` | 9 (whole `spring-boot-loader` module) | HIGH | **FIXED 2026-07-12** — moved to [`../../internal/springboot/zip-filedatablock-bulk-bytebuffer-put-aioobe-FIXED.md`](../../internal/springboot/zip-filedatablock-bulk-bytebuffer-put-aioobe-FIXED.md) |
| `flyway-cglib-heap-corruption-sigsegv-crash.md` | 1 (`FlywayAutoConfigurationTests`) | HIGH (SIGSEGV) | **FIXED 2026-07-12** — moved to [`../../internal/flyway-cglib-heap-corruption-sigsegv-crash-FIXED.md`](../../internal/flyway-cglib-heap-corruption-sigsegv-crash-FIXED.md); `read_string` misidentified a `String[]` array as `java/lang/String` by class ID alone |
| `TestCompiler` annotation/platform listing cluster | 7 (`spring-boot-configuration-processor`) | MEDIUM | **FIXED/RETIRED 2026-07-13** — moved to [`../../internal/springboot/testcompiler-annotation-classes-not-found-cluster-FIXED.md`](../../internal/springboot/testcompiler-annotation-classes-not-found-cluster-FIXED.md); full JRT package listing and generated in-memory `resource:` URL handling fixed, 94/94 tests pass |
| `BraveAutoConfigurationTests` summary-printing `ClassCastException` | 1 | LOW | **FIXED/RETIRED 2026-07-15** — moved to [`../../internal/springboot/brave-baggagefields-classcast-summary-printing-FIXED.md`](../../internal/springboot/brave-baggagefields-classcast-summary-printing-FIXED.md); fixed the early real-`Collections` initialization path and the receiver-polymorphic `Object.equals` dispatch residual. Verified 26/26 with JIT both off and on |
| `JdbcSessionAutoConfigurationTests` HSQLDB `RangeGroupEmpty` loop | 1 | MEDIUM | **FIXED 2026-07-15** — moved to [`../../internal/springboot/jdbcsession-hsqldb-rangegroupempty-hang-FIXED.md`](../../internal/springboot/jdbcsession-hsqldb-rangegroupempty-hang-FIXED.md); `java.lang.reflect.Array.set` now validates primitive-array values before unboxing, so HSQLDB's non-wrapper `RangeGroupEmpty` never reaches a speculative field-0 read |
| `CharBuffer.order()` missing native + IDN `<clinit>` poisoning | 6 FAIL + 1 fatal CRASH | HIGH | **FIXED 2026-07-14** — moved to [`../../internal/springboot/charbuffer-order-missing-native-idn-clinit-cluster-FIXED.md`](../../internal/springboot/charbuffer-order-missing-native-idn-clinit-cluster-FIXED.md); registered `order()` on `CharBuffer` and its 4 `ByteBufferAsCharBuffer{B,L,RB,RL}` views. Verified via standalone repro — the `AbstractMethodError` is gone. **Residual found while verifying**: a deeper, previously-masked bug now surfaces — see [`charbuffer-getarray-scopedmemoryaccess-copymemory-aioobe.md`](charbuffer-getarray-scopedmemoryaccess-copymemory-aioobe.md) below; `java.net.IDN` still does not work end-to-end |
| `CharBuffer.getArray` `ScopedMemoryAccess.copyMemory` AIOOBE | blocks all 7 classes above | HIGH | **FIXED 2026-07-15** — moved to [`../../internal/springboot/charbuffer-getarray-scopedmemoryaccess-copymemory-aioobe-FIXED.md`](../../internal/springboot/charbuffer-getarray-scopedmemoryaccess-copymemory-aioobe-FIXED.md); `s2_bb_as_char_buffer` never seeded the view's `Buffer.address` (`= 16`), so the real bulk-get fast path decoded an offset below the array base. `java.net.IDN` now works end-to-end (== HotSpot), unblocking the whole Netty/RSocket reactive cluster this poisoned |
| `LoggerContext.loggerContextListenerList` null cluster | 20+ | HIGH | **FIXED/RETIRED 2026-07-15** — moved to [`../../internal/springboot/logback-loggercontext-listenerlist-final-field-corruption-FIXED.md`](../../internal/springboot/logback-loggercontext-listenerlist-final-field-corruption-FIXED.md); the SLF4J bridge returned a real-classed but unconstructed `LoggerContext` because it overrode `<init>` as a no-op. The bridge now invokes the real constructor; `BannerTests` passes 6/6 under both `--nojit` and JIT |
| `Map.Entry::getKey`/`getValue` lambda-dispatch precedence | 5 | HIGH | **FIXED 2026-07-14** — moved to [`../../internal/springboot/structured-logging-map-entry-getkey-lambda-dispatch-precedence-FIXED.md`](../../internal/springboot/structured-logging-map-entry-getkey-lambda-dispatch-precedence-FIXED.md); the receiver-specific "C25 rescue" native lookup (`vm/src/vm/vm_exec.rs`) no longer skips itself when a less-specific interface-level native already matched. Verified via standalone repro (correctly returns `"spring"` instead of the whole entry) and a regression check of the `Enumeration$Impl.hasMoreElements()` case this rescue was originally written for (still passes) |
| `*ApplicationContextRunnerTests` lazy CGLIB class-not-found process crash | 3 (fatal CRASH) | HIGH | **FIXED 2026-07-15** — moved to [`../../internal/springboot/applicationcontextrunnertests-lazy-cglib-classnotfound-crash-FIXED.md`](../../internal/springboot/applicationcontextrunnertests-lazy-cglib-classnotfound-crash-FIXED.md); `Class.forName` now initializes the class resolved through the requested loader by its `ClassId`, preserving visibility of newly defined `$$SpringCGLIB$$` proxies. The registration bridge also propagates duplicate-definition errors. All three original runner classes pass in JIT and `--nojit` |
| [`jit-dispatch-depth-guard-shallow-stackoverflow-cluster.md`](jit-dispatch-depth-guard-shallow-stackoverflow-cluster.md) | 3 | MEDIUM | OPEN — found 2026-07-14. All 3 `StackOverflowError`s have anomalously shallow (3-11 frame), non-repeating traces on trivial, non-recursive Spring APIs — not a credible genuine-recursion shape. Traced to `JIT_DISPATCH_DEPTH`'s guard (`vm/src/jit/helpers.rs`) never recording frame metadata, so any `StackOverflowError` raised through it is structurally incapable of showing its real cause; a separate, unused `CallStack` module with correct frame tracking exists but is dead code |
| [`comparable-classcast-lambda-proxy-unknown-class.md`](comparable-classcast-lambda-proxy-unknown-class.md) | 2 | MEDIUM | OPEN — found 2026-07-14. `Collections.sort`/`Arrays.sort`'s `Comparable` check (`native-collections`) only consults `class_manager`, which never registers lambda-proxy classes — so a lambda implementing a `Comparable`-extending functional interface is falsely rejected and reported as class `<unknown>`. Same root-cause family as an already-fixed sibling bug in `Class.getGenericInterfaces()` |
| [`collectionbindertests-classcast-testdescriptor-crash.md`](collectionbindertests-classcast-testdescriptor-crash.md) | 1 (fatal CRASH) | CRITICAL | OPEN — found 2026-07-14. `CollectionBinderTests` crashes the whole process with `ClassCastException: String cannot be cast to TestDescriptor` entirely inside JUnit Platform's own internal engine-failure-reporting path (0 tests ever ran) — a "wrong-type-from-native-collection" shape in the same family as the already-fixed `OnClassCondition` corruption, but a distinct, unconfirmed occurrence; leading suspect is the new `HASHMAP_NATIVE_DISPATCH_CACHE` fast path from the same-day `77f8b37e5` |
| `JsonValueWriterTests` cyclic collection nesting guard | 1 (fatal CRASH) | CRITICAL | **FIXED 2026-07-15** — moved to [`../../internal/springboot/jsonvaluewritertests-nesting-depth-guard-stack-overflow-FIXED.md`](../../internal/springboot/jsonvaluewritertests-nesting-depth-guard-stack-overflow-FIXED.md); native `Map.forEach` now snapshots cyclic map entries without hashing them, and `Iterable` method-reference lambdas use their real iterator implementation |
| `HttpClientSecure` null-provider crash | 2 (fatal CRASH) | HIGH | **FIXED 2026-07-16** — moved to [`../../internal/fixed-suite-bugs/reactor-nettyhttpclient-httpclientsecure-null-provider-crash-FIXED.md`](../../internal/fixed-suite-bugs/reactor-nettyhttpclient-httpclientsecure-null-provider-crash-FIXED.md); native client SSL session support, JKS key recovery, TLS alert delivery, real Tomcat lifecycle, and MethodHandle primitive-return handling now cover the former crash and residuals.
| [`method-getexceptiontypes-null-jdk-dynamic-proxy-synthesized-method.md`](method-getexceptiontypes-null-jdk-dynamic-proxy-synthesized-method.md) | 3 (4 test methods) | MEDIUM | OPEN — found 2026-07-16 (`rerun-20260716`, shard3). `Method.getExceptionTypes()` returns `null` (JDK spec: never null) for the synthetic `Method` object `vm/src/vm/vm_exec.rs::proxy_invoke_handler` builds to pass into `InvocationHandler.invoke()` for JDK dynamic proxies — it never sets the `exceptionTypes` field, unlike the sibling `create_method_object` path which already fixed this exact gap. Hits every Spring Data repository call (`JdkDynamicAopProxy`) via `PersistenceExceptionTranslationInterceptor.invoke` → `ReflectionUtils.declaresException` |
| `TestEngine.getId()` young-GC forwarding-walk truncation | 2 (fatal CRASH) | CRITICAL | **FIXED 2026-07-16** — moved to [`../../internal/springboot/testengine-getid-abstractmethoderror-young-gc-forwarding-gap-FIXED.md`](../../internal/springboot/testengine-getid-abstractmethoderror-young-gc-forwarding-gap-FIXED.md). Found and root-caused same-day as a new young-GC pre-forwarding walk (`gc/src/gen_heap.rs`) that only special-cased the `GAP_FILLER_CLASS_ID` sentinel, unlike the sibling `exact_cursor` walk which also consults the free-list via `skip_free_blocks` — dropping live young-gen objects from the forwarding set on unrecognized free/TLAB-remnant ranges. `fix/wildfly-cce0079-close-20260716` (adds the missing `skip_free_blocks` call) merged into `dev` the same day; verified fixed via rebuild + rerun (fatal crash gone) |
| `BasicErrorControllerIntegrationTests` stale-pointer crash | 1 (fatal CRASH) | CRITICAL | **FIXED 2026-07-16** — moved to [`../../internal/springboot/basiccontroller-stale-pointer-invokevirtual-aqs-conditionnode-crash-FIXED.md`](../../internal/springboot/basiccontroller-stale-pointer-invokevirtual-aqs-conditionnode-crash-FIXED.md). Independent confirmation of the same young-GC forwarding-walk truncation bug as the `TestEngine.getId()` entry above. Verified fixed via rebuild + rerun after `fix/wildfly-cce0079-close-20260716` merged into `dev` (fatal `ClassCastException`/CRASH gone) |
| [`wrong-receiver-virtual-dispatch-corruption-cluster.md`](wrong-receiver-virtual-dispatch-corruption-cluster.md) | 9 FAIL + 1 fatal CRASH | CRITICAL (Case 1) / HIGH unconfirmed (Case 2) | OPEN — found 2026-07-16. Case 1 (`String.setOption`, 9 classes, root-caused): `javax/net/ssl/SSLSocketFactory.createSocket()` (`native-builtins/src/tls.rs`) still hands out a bare 2-field synthetic `Socket` under `CRATONVM_REAL_NET_SOCKETS=1`; real `Socket.getImpl()` bytecode then reads garbage off the undersized object and dispatches onto a leftover `String` — a gap in a previously-fixed sibling bug (`javax/net/SocketFactory`, commit `bd03eb243`) that never covered the SSL variant. Case 2 (`File.get()`, 1 fatal CRASH) has the same symptom shape but is **not confirmed** to share Case 1's mechanism — filed for tracking, root cause still open |
| [`tomcatservletwebserverfactory-cross-module-classnotfound-crash.md`](tomcatservletwebserverfactory-cross-module-classnotfound-crash.md) | 10 (fatal CRASH) | HIGH | OPEN — found 2026-07-16. A native shim (`native-builtins/src/net_phase_e.rs`, `ServletWebServerApplicationContext.getWebServerFactory()`) unconditionally allocates a hardcoded `TomcatServletWebServerFactory` regardless of servlet backend — added to route around a real Tomcat bean-registration bug, but fires identically for Jetty-only/generic-web-server modules where that class genuinely doesn't exist on the classpath (confirmed via real Gradle classpath dumps, not a suite-runner gap). The resulting class-not-found escapes as an uncaught internal error and aborts the process instead of throwing a catchable `NoClassDefFoundError` |

## 2026-07-16 rerun of non-passed classes (post-dev-sync, 764 classes, 8 shards)

Merged `origin/dev` into `feat/spring-boot-crashfail-20260714` (which had just
been pushed as `dev` itself, then moved twice more the same session — typical
for this shared host) and reran every class that was not `PASS` in the
`crashfail-20260714` run (517 non-PASS + 247 classes from shard6, which never
produced any results at all last time — the whole shard died silently),
764 classes total, 8 fresh shards against the newly-merged+rebuilt binary.
The merge itself brought in a huge amount of concurrent work — nearly all of
this round's own residual docs from `crashfail-20260714` (the `CharBuffer`
`getArray` AIOOBE, the Logback field corruption, the `@Lazy`/CGLIB crash, the
lambda/Comparable gap, the JIT-dispatch-depth cluster, the Reactor Netty NPE)
had independently been fixed by concurrent sessions the same day — see the
`FIXED`/moved-to-`internal` rows above.

Triaged the new CRASH/FAIL set from this rerun and filed 5 more docs (rows
above, dated 2026-07-16). Two were the **same newly-discovered, critical GC
regression** (`testengine-getid...`/`basiccontroller-stale-pointer...`) — a
brand-new young-GC pre-forwarding walk added the same day silently dropping
live objects from the forwarding set under certain free-list/TLAB-remnant
shapes. The fix (`fix/wildfly-cce0079-close-20260716`) merged into `dev`
while this investigation was still in progress; both docs were verified
FIXED and moved to `internal/` the same session (rebuild + rerun confirmed
the fatal crashes are gone). One (`wrong-receiver-virtual-dispatch...`)
root-causes a gap in a previously-fixed sibling bug (SSL socket variant never
covered). One (`tomcatservletwebserverfactory...`) is a hardcoded-Tomcat-shim
bug affecting any non-Tomcat servlet module. One
(`method-getexceptiontypes-null...`) is a narrow reflection-contract
violation for JDK dynamic-proxy `Method` objects.

## 2026-07-14 crashfail run (post-dev-sync full rerun)

Reran the full suite from scratch after all 7 previously-filed clusters above
were fixed by concurrent sessions (worktree `CratonVM-spring-boot-crashfail-20260714`,
branch `feat/spring-boot-crashfail-20260714`, off `dev@1021533f9`, following a
full worktree loss in a disk-space-recovery sweep — see
[[feedback_shared_host_disk_cleanup]]). 8 shards, ~1975 classes; while the run
was still in progress (~1500/1975 classes recorded, 3/8 shards fully done: 8
CRASH, 271+ FAIL, 112 HANG, 34 EMPTY, 1077 PASS so far), triaged the CRASH set
and the clearest FAIL signature clusters and dispatched 8 parallel
investigation agents (one per cluster) — 9 new docs filed above (rows dated
2026-07-14). Two are process-fatal crashes traced to genuine, precisely
root-caused CratonVM bugs with concrete fix directions (`CharBuffer.order()`
missing native, `Map.Entry::getKey` lambda-dispatch precedence, the
`@Lazy`/CGLIB classloader gap); the rest are OPEN with strong hypotheses but
not yet confirmed by live bisection (logback field corruption, the two other
crashes, the JIT-dispatch-depth shallow-trace cluster, the lambda-Comparable
gap, the Reactor Netty NPE). None of the 9 overlap with the 7 already-retired
clusters from the 07-11/07-13 rounds. The 79-FAIL `AssertionError`/37-FAIL
`AssertionFailedError` generic buckets were not clustered in this pass (too
coarse to attribute to a single signature without deeper per-class digging) —
remain an uncharacterized long tail.

## HANG-rerun follow-up (150 classes @ 1500s timeout)

Reran the original 150 HANG classes at 5x the timeout (see
`apps/spring-boot-suite-runner/RESULTS-20260711.md` "HANG rerun" section):
90 still hung, 51 turned FAIL, 7 PASS, 2 CRASH. Of the 51 FAILs: 7 are the
the now-retired `TestCompiler` cluster above; ~16 more
overlap with the (now fixed) `OnClassCondition` cluster and
the two retired clusters (destroy-method resolution, `MemoryAccessOption`)
above — those classes just needed more wall time to *reach* the
already-known failure instead of timing out first. The remaining ~28 are a
scattered long tail (mostly one-off `AssertionError`s, a couple of
`GroovyRuntimeException`s from the Thymeleaf layout-dialect integration, two
`ParameterResolutionException`s for `WebTestClient` autowiring) — not yet
clustered; no single dominant pattern found. Of the 2 CRASHes: one
(`FlywayAutoConfigurationTests`) is the now-fixed heap-corruption doc above; the
other (`OriginTrackedYamlLoaderTests`, `NoClassDefFoundError:
org/junit/platform/commons/util/ExceptionUtils`) is a **runner classpath
artifact** (likely pathing-jar manifest truncation for a very long
classpath), not a CratonVM bug — not filed.

## Methodology note (runner fix, not a bug)

The first run (`full-20260711b`) did **not** set `CRATONVM_REAL_NET_SOCKETS`,
so any test doing a real socket bind (e.g. `HazelcastAutoConfigurationClientTests`)
hit the already-known, already-fixed-behind-a-flag
`java.net.ServerSocket.socketLock` null bug (see `reference_server_socket_gap`
in memory) rather than a new issue. `run-spring-boot-suite.ps1` now sets
`CRATONVM_REAL_NET_SOCKETS`/`CRATONVM_REAL_AQS`/
`CRATONVM_DISABLE_DEFAULT_WATCHDOG`/`CRATONVM_ROOTSNAP_CACHE` for craton runs,
matching `tomcat-suite-runner`'s convention — a rerun with the fixed runner
would likely shift some FAIL/HANG counts.
